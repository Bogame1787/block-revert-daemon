// serves a regular file unchanged. no reference state, so no revert. this is
// the floor the other stores are measured against: whatever it costs is the
// cost of fuse itself.

use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{BlockStore, ResetReport, StoreInfo, pread_exact, pwrite_all};

#[derive(Debug)]
pub struct PassthroughStore {
    file: File,
    path: PathBuf,
    size: AtomicU64,
    read_only: bool,
}

impl PassthroughStore {
    pub fn open(path: &Path, read_only: bool) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        let size = file.metadata()?.len();
        Ok(PassthroughStore {
            file,
            path: path.to_path_buf(),
            size: AtomicU64::new(size),
            read_only,
        })
    }
}

impl BlockStore for PassthroughStore {
    fn size(&self) -> u64 {
        self.size.load(Ordering::Acquire)
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        pread_exact(&self.file, off, buf)
    }

    fn write_at(&self, off: u64, buf: &[u8]) -> io::Result<usize> {
        if self.read_only {
            return Err(io::Error::from_raw_os_error(libc::EROFS));
        }
        let n = pwrite_all(&self.file, off, buf)?;
        // a write past the old end grows the image, and the kernel asks for the
        // new size on the next getattr
        self.size.fetch_max(off + n as u64, Ordering::AcqRel);
        Ok(n)
    }

    fn flush(&self, datasync: bool) -> io::Result<()> {
        if self.read_only {
            return Ok(());
        }
        if datasync { self.file.sync_data() } else { self.file.sync_all() }
    }

    fn truncate(&self, size: u64) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::from_raw_os_error(libc::EROFS));
        }
        self.file.set_len(size)?;
        self.size.store(size, Ordering::Release);
        Ok(())
    }

    fn reset(&self) -> io::Result<ResetReport> {
        Err(io::Error::new(
            ErrorKind::Unsupported,
            "passthrough store has no reference state to revert to; \
             pick --store fullcopy, dirtylog, overlay or cow",
        ))
    }

    // the only store that can hand the kernel a descriptor, since what it
    // serves really is one file. this is what --cache passthrough needs.
    fn backing_fd(&self) -> Option<BorrowedFd<'_>> {
        Some(self.file.as_fd())
    }

    fn info(&self) -> StoreInfo {
        StoreInfo {
            kind: "passthrough",
            path: self.path.clone(),
            golden: None,
            size: self.size(),
            block_size: 0,
            read_only: self.read_only,
            resettable: false,
            private_bytes: 0,
            dirty_blocks: 0,
            dirty_runs: 0,
        }
    }
}
