// private copy of a golden image, reverted by copying the whole thing back.
// reset costs the image size however little the guest touched, so this is the
// worst case the other stores are read against.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::{BlockStore, MarkReport, ResetReport, StoreInfo, pread_exact, pwrite_all};

// how much the whole-image copy moves per syscall
const CHUNK: usize = 4 << 20;

#[derive(Debug)]
pub struct FullCopyStore {
    golden: File,
    image: File,
    golden_path: PathBuf,
    image_path: PathBuf,
    size: AtomicU64,
}

impl FullCopyStore {
    // reseed copies the golden over the image at startup, so a run begins from
    // a known state rather than whatever the last one left
    pub fn open(golden: &Path, image: &Path, reseed: bool) -> io::Result<Self> {
        let golden_f = OpenOptions::new().read(true).write(true).open(golden)?;
        let size = golden_f.metadata()?.len();
        let image_f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(image)?;
        image_f.set_len(size)?;

        let s = FullCopyStore {
            golden: golden_f,
            image: image_f,
            golden_path: golden.to_path_buf(),
            image_path: image.to_path_buf(),
            size: AtomicU64::new(size),
        };
        if reseed {
            s.copy(&s.golden, &s.image)?;
        }
        Ok(s)
    }

    fn copy(&self, from: &File, to: &File) -> io::Result<u64> {
        let size = self.size.load(Ordering::Acquire);
        let mut buf = vec![0u8; CHUNK];
        let mut off = 0u64;
        while off < size {
            let want = CHUNK.min((size - off) as usize);
            let n = pread_exact(from, off, &mut buf[..want])?;
            if n == 0 {
                break;
            }
            pwrite_all(to, off, &buf[..n])?;
            off += n as u64;
        }
        Ok(off)
    }
}

impl BlockStore for FullCopyStore {
    fn size(&self) -> u64 {
        self.size.load(Ordering::Acquire)
    }

    fn read_only(&self) -> bool {
        false
    }

    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        pread_exact(&self.image, off, buf)
    }

    fn write_at(&self, off: u64, buf: &[u8]) -> io::Result<usize> {
        let n = pwrite_all(&self.image, off, buf)?;
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
        let bytes = self.copy(&self.golden, &self.image)?;
        self.image.sync_data()?;
        Ok(ResetReport {
            blocks_reverted: 0,
            bytes_reverted: bytes,
            runs: 1,
            micros: t.elapsed().as_micros() as u64,
        })
    }

    fn mark(&self) -> io::Result<MarkReport> {
        let t = Instant::now();
        let bytes = self.copy(&self.image, &self.golden)?;
        self.golden.sync_data()?;
        Ok(MarkReport {
            blocks_folded: 0,
            bytes_folded: bytes,
            micros: t.elapsed().as_micros() as u64,
        })
    }

    fn backing_fd(&self) -> Option<BorrowedFd<'_>> {
        // the served bytes are exactly this one file, so passthrough works
        // here even though a revert is not free
        Some(self.image.as_fd())
    }

    fn info(&self) -> StoreInfo {
        StoreInfo {
            kind: "fullcopy",
            path: self.image_path.clone(),
            golden: Some(self.golden_path.clone()),
            size: self.size(),
            block_size: 0,
            read_only: false,
            resettable: true,
            // private by construction, this store never shares
            private_bytes: self.size(),
            dirty_blocks: 0,
            dirty_runs: 0,
        }
    }
}
