// golden image plus a sparse overlay file holding only the blocks that moved.
// reset punches holes over the dirty runs, so the cost follows extents and the
// worst case is bounded only by fragmentation.

// the read path is indirected: every read decides per block whether the overlay
// or the golden owns it, which means two possible sources and two page caches.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::blockmap::{BlockMap, block_span};
use super::{
    BlockStore, MarkReport, ResetReport, StoreInfo, pread_exact, punch_hole, pwrite_all,
};

#[derive(Debug)]
pub struct OverlayStore {
    golden: File,
    overlay: File,
    golden_path: PathBuf,
    overlay_path: PathBuf,
    size: AtomicU64,
    block: usize,
    dirty: BlockMap,
}

impl OverlayStore {
    pub fn open(golden: &Path, overlay: &Path, block: usize) -> io::Result<Self> {
        // golden is writable only so mark can fold into it. reads still share
        // one inode, and therefore one page cache, across every daemon.
        let golden_f = OpenOptions::new().read(true).write(true).open(golden)?;
        let size = golden_f.metadata()?.len();

        // a fresh overlay every time. inheriting a previous run's would mean
        // serving one guest's writes to the next.
        let overlay_f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(overlay)?;
        overlay_f.set_len(size)?;

        Ok(OverlayStore {
            golden: golden_f,
            overlay: overlay_f,
            golden_path: golden.to_path_buf(),
            overlay_path: overlay.to_path_buf(),
            size: AtomicU64::new(size),
            block,
            dirty: BlockMap::new((size as usize).div_ceil(block)),
        })
    }

    // bring a block into the overlay so a partial write lands on real contents
    // rather than a hole. only happens on first touch of a block.
    fn materialise(&self, b: usize) -> io::Result<()> {
        if self.dirty.test(b) {
            return Ok(());
        }
        let off = (b * self.block) as u64;
        let size = self.size.load(Ordering::Acquire);
        let span = (self.block as u64).min(size.saturating_sub(off)) as usize;
        if span == 0 {
            return Ok(());
        }
        let mut buf = vec![0u8; span];
        pread_exact(&self.golden, off, &mut buf)?;
        pwrite_all(&self.overlay, off, &buf)?;
        self.dirty.set(b);
        Ok(())
    }
}

impl BlockStore for OverlayStore {
    fn size(&self) -> u64 {
        self.size.load(Ordering::Acquire)
    }

    fn read_only(&self) -> bool {
        false
    }

    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        let size = self.size.load(Ordering::Acquire);
        if off >= size {
            return Ok(0);
        }
        let want = buf.len().min((size - off) as usize);
        if want == 0 {
            return Ok(0);
        }
        let (first, last) = block_span(off, want, self.block);

        // walk runs of blocks with the same owner so a sequential read of
        // untouched image costs one pread, not one per block
        let mut done = 0usize;
        let mut b = first;
        while b <= last && done < want {
            let from_overlay = self.dirty.test(b);
            let mut end = b;
            while end + 1 <= last && self.dirty.test(end + 1) == from_overlay {
                end += 1;
            }

            let run_end_off = ((end + 1) * self.block) as u64;
            let chunk = (run_end_off.min(off + want as u64) - (off + done as u64)) as usize;
            let src = if from_overlay { &self.overlay } else { &self.golden };
            let n = pread_exact(src, off + done as u64, &mut buf[done..done + chunk])?;
            done += n;
            if n < chunk {
                break;
            }
            b = end + 1;
        }
        Ok(done)
    }

    fn write_at(&self, off: u64, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let (first, last) = block_span(off, buf.len(), self.block);

        // the edge blocks may be partially covered, so they have to exist in
        // the overlay first. fully covered interior blocks do not.
        for b in [first, last] {
            let b_off = (b * self.block) as u64;
            let covers_whole = off <= b_off && off + buf.len() as u64 >= b_off + self.block as u64;
            if !covers_whole {
                self.materialise(b)?;
            }
        }

        let n = pwrite_all(&self.overlay, off, buf)?;
        for b in first..=last {
            self.dirty.set(b);
        }
        self.size.fetch_max(off + n as u64, Ordering::AcqRel);
        Ok(n)
    }

    fn flush(&self, datasync: bool) -> io::Result<()> {
        if datasync { self.overlay.sync_data() } else { self.overlay.sync_all() }
    }

    fn truncate(&self, size: u64) -> io::Result<()> {
        self.overlay.set_len(size)?;
        self.size.store(size, Ordering::Release);
        Ok(())
    }

    fn reset(&self) -> io::Result<ResetReport> {
        let t = Instant::now();
        let size = self.size.load(Ordering::Acquire);
        let mut blocks = 0u64;
        let mut bytes = 0u64;
        let mut runs = 0u64;
        let mut err: Option<io::Error> = None;

        self.dirty.for_each_run(|firstb, len| {
            if err.is_some() {
                return;
            }
            let off = (firstb * self.block) as u64;
            let span = ((len * self.block) as u64).min(size.saturating_sub(off));
            if span == 0 {
                return;
            }
            // the hole reads as zero and costs nothing on disk. the next read
            // falls through to golden because the bitmap is about to clear.
            match punch_hole(&self.overlay, off, span) {
                Ok(()) => {
                    blocks += len as u64;
                    bytes += span;
                    runs += 1;
                }
                Err(e) => err = Some(e),
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        self.dirty.clear_all();
        Ok(ResetReport {
            blocks_reverted: blocks,
            bytes_reverted: bytes,
            runs,
            micros: t.elapsed().as_micros() as u64,
        })
    }

    fn mark(&self) -> io::Result<MarkReport> {
        let t = Instant::now();
        let size = self.size.load(Ordering::Acquire);
        let mut blocks = 0u64;
        let mut bytes = 0u64;
        let mut err: Option<io::Error> = None;

        self.dirty.for_each_run(|firstb, len| {
            if err.is_some() {
                return;
            }
            let off = (firstb * self.block) as u64;
            let span = ((len * self.block) as u64).min(size.saturating_sub(off));
            if span == 0 {
                return;
            }
            let mut buf = vec![0u8; span as usize];
            let r = pread_exact(&self.overlay, off, &mut buf)
                .and_then(|_| pwrite_all(&self.golden, off, &buf))
                .and_then(|_| punch_hole(&self.overlay, off, span));
            match r {
                Ok(()) => {
                    blocks += len as u64;
                    bytes += span;
                }
                Err(e) => err = Some(e),
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        self.golden.sync_data()?;
        self.dirty.clear_all();
        Ok(MarkReport {
            blocks_folded: blocks,
            bytes_folded: bytes,
            micros: t.elapsed().as_micros() as u64,
        })
    }

    fn backing_fd(&self) -> Option<BorrowedFd<'_>> {
        // two files compose the served image, so there is no single descriptor
        None
    }

    fn info(&self) -> StoreInfo {
        let blocks = self.dirty.count();
        StoreInfo {
            kind: "overlay",
            path: self.overlay_path.clone(),
            golden: Some(self.golden_path.clone()),
            size: self.size(),
            block_size: self.block as u32,
            read_only: false,
            resettable: true,
            private_bytes: blocks * self.block as u64,
            dirty_blocks: blocks,
            dirty_runs: self.dirty.runs(),
        }
    }
}
