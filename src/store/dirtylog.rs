// private copy plus a dirty bitmap, reverted by copying back only what moved.
// reads go straight to the image with no indirection and no second page cache,
// so the whole cost lands on reset and scales with what the guest dirtied.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::blockmap::{BlockMap, block_span};
use super::{BlockStore, MarkReport, ResetReport, StoreInfo, pread_exact, pwrite_all};

#[derive(Debug)]
pub struct DirtyLogStore {
    golden: File,
    image: File,
    golden_path: PathBuf,
    image_path: PathBuf,
    size: AtomicU64,
    block: usize,
    dirty: BlockMap,
}

impl DirtyLogStore {
    pub fn open(golden: &Path, image: &Path, block: usize, reseed: bool) -> io::Result<Self> {
        let golden_f = OpenOptions::new().read(true).write(true).open(golden)?;
        let size = golden_f.metadata()?.len();
        let image_f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(image)?;
        image_f.set_len(size)?;

        let s = DirtyLogStore {
            golden: golden_f,
            image: image_f,
            golden_path: golden.to_path_buf(),
            image_path: image.to_path_buf(),
            size: AtomicU64::new(size),
            block,
            dirty: BlockMap::new((size as usize).div_ceil(block)),
        };
        if reseed {
            // a fresh run starts from the golden, and nothing is dirty yet
            s.copy_range(&s.golden, &s.image, 0, size)?;
            s.dirty.clear_all();
        }
        Ok(s)
    }

    fn copy_range(&self, from: &File, to: &File, off: u64, len: u64) -> io::Result<u64> {
        let mut buf = vec![0u8; len.min(4 << 20) as usize];
        let mut done = 0u64;
        while done < len {
            let want = buf.len().min((len - done) as usize);
            let n = pread_exact(from, off + done, &mut buf[..want])?;
            if n == 0 {
                break;
            }
            pwrite_all(to, off + done, &buf[..n])?;
            done += n as u64;
        }
        Ok(done)
    }

    // walk the dirty runs moving each between the two files. reset goes golden
    // to image and mark goes the other way, but the traversal is the same.
    fn move_dirty(&self, reverse: bool) -> io::Result<(u64, u64, u64)> {
        let mut blocks = 0u64;
        let mut bytes = 0u64;
        let mut runs = 0u64;
        let mut err: Option<io::Error> = None;
        let size = self.size.load(Ordering::Acquire);

        self.dirty.for_each_run(|first, len| {
            if err.is_some() {
                return;
            }
            let off = (first * self.block) as u64;
            let span = ((len * self.block) as u64).min(size.saturating_sub(off));
            if span == 0 {
                return;
            }
            let r = if reverse {
                self.copy_range(&self.image, &self.golden, off, span)
            } else {
                self.copy_range(&self.golden, &self.image, off, span)
            };
            match r {
                Ok(n) => {
                    blocks += len as u64;
                    bytes += n;
                    runs += 1;
                }
                Err(e) => err = Some(e),
            }
        });

        match err {
            Some(e) => Err(e),
            None => Ok((blocks, bytes, runs)),
        }
    }
}

impl BlockStore for DirtyLogStore {
    fn size(&self) -> u64 {
        self.size.load(Ordering::Acquire)
    }

    fn read_only(&self) -> bool {
        false
    }

    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        // no indirection at all: this is why the read path costs nothing
        pread_exact(&self.image, off, buf)
    }

    fn write_at(&self, off: u64, buf: &[u8]) -> io::Result<usize> {
        let n = pwrite_all(&self.image, off, buf)?;
        let (first, last) = block_span(off, n, self.block);
        for b in first..=last {
            self.dirty.set(b);
        }
        self.size.fetch_max(off + n as u64, Ordering::AcqRel);
        Ok(n)
    }

    fn flush(&self, datasync: bool) -> io::Result<()> {
        if datasync { self.image.sync_data() } else { self.image.sync_all() }
    }

    fn truncate(&self, size: u64) -> io::Result<()> {
        self.image.set_len(size)?;
        self.size.store(size, Ordering::Release);
        Ok(())
    }

    fn reset(&self) -> io::Result<ResetReport> {
        let t = Instant::now();
        let (blocks, bytes, runs) = self.move_dirty(false)?;
        self.image.sync_data()?;
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
        let (blocks, bytes, _) = self.move_dirty(true)?;
        self.golden.sync_data()?;
        self.dirty.clear_all();
        Ok(MarkReport {
            blocks_folded: blocks,
            bytes_folded: bytes,
            micros: t.elapsed().as_micros() as u64,
        })
    }

    fn backing_fd(&self) -> Option<BorrowedFd<'_>> {
        // expressible as one file, but handing it to the kernel would bypass
        // write_at and lose the bitmap, so the revert would silently miss
        // everything the guest touched.
        None
    }

    fn info(&self) -> StoreInfo {
        let blocks = self.dirty.count();
        StoreInfo {
            kind: "dirtylog",
            path: self.image_path.clone(),
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
