// the storage seam. everything above this trait is fuse plumbing and knows
// nothing about how the image is held. five implementations:
// | store       | reset       | host space   | overlay lives |
// | passthrough | unsupported | 1x           | -             |
// | fullcopy    | O(size)     | 2x           | the image     |
// | dirtylog    | O(dirty)    | 2x           | the image     |
// | overlay     | O(extents)  | 1x + dirty   | sparse file   |
// | cow         | O(dirty)    | 1x + dirty   | daemon memory |

pub mod blockmap;
mod cow;
mod dirtylog;
mod fullcopy;
mod overlay;
mod passthrough;

use std::io::{self, ErrorKind};
use std::os::fd::BorrowedFd;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;

pub use cow::CowStore;
pub use dirtylog::DirtyLogStore;
pub use fullcopy::FullCopyStore;
pub use overlay::OverlayStore;
pub use passthrough::PassthroughStore;

use crate::config::{Config, StoreKind};

// what a revert did, reported back over the control socket so the caller can
// tell a real revert from a no-op
#[derive(Debug, Default, Clone, Serialize)]
pub struct ResetReport {
    pub blocks_reverted: u64,
    pub bytes_reverted: u64,
    pub runs: u64,
    pub micros: u64,
}

// what establishing a new reference point did
#[derive(Debug, Default, Clone, Serialize)]
pub struct MarkReport {
    pub blocks_folded: u64,
    pub bytes_folded: u64,
    pub micros: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreInfo {
    pub kind: &'static str,
    pub path: PathBuf,
    pub golden: Option<PathBuf>,
    pub size: u64,
    pub block_size: u32,
    pub read_only: bool,
    pub resettable: bool,
    // bytes diverged from the golden image, zero right after a reset
    pub private_bytes: u64,
    pub dirty_blocks: u64,
    pub dirty_runs: u64,
}

// a byte-addressable image. implementations must be safe to call concurrently
// from every event loop thread: the file-backed stores get that from pread and
// pwrite carrying their own offset, the cow store from a lock on its map.
pub trait BlockStore: Send + Sync + std::fmt::Debug {
    fn size(&self) -> u64;
    fn read_only(&self) -> bool;

    // short only at end of image
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<usize>;

    fn write_at(&self, off: u64, buf: &[u8]) -> io::Result<usize>;

    // datasync skips metadata where the backend can
    fn flush(&self, datasync: bool) -> io::Result<()>;

    fn truncate(&self, size: u64) -> io::Result<()>;

    // revert to the reference state. stores without one return Unsupported.
    fn reset(&self) -> io::Result<ResetReport>;

    // make the current contents the new reference point. the caller must have
    // quiesced io first, which in practice means the vm is paused for a
    // snapshot, and this is allowed to be slow since it runs once per snapshot
    // rather than once per reset.
    fn mark(&self) -> io::Result<MarkReport> {
        Err(io::Error::new(
            ErrorKind::Unsupported,
            "this store has no reference point to move",
        ))
    }

    // descriptor the kernel may take over for passthrough mode. a store built
    // from several files has no single fd and returns none.
    fn backing_fd(&self) -> Option<BorrowedFd<'_>>;

    fn info(&self) -> StoreInfo;
}

// loop a positional read until the buffer is full or the file ends
pub(crate) fn pread_exact(
    file: &std::fs::File,
    off: u64,
    buf: &mut [u8],
) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        match file.read_at(&mut buf[done..], off + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

// loop a positional write until everything is out
pub(crate) fn pwrite_all(file: &std::fs::File, off: u64, buf: &[u8]) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        match file.write_at(&buf[done..], off + done as u64) {
            Ok(0) => return Err(io::Error::from(ErrorKind::WriteZero)),
            Ok(n) => done += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

// drop the backing extents for a range, leaving a hole that reads as zeroes
pub(crate) fn punch_hole(file: &std::fs::File, off: u64, len: u64) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: fd is live for the call, offset and length are plain integers
    let rc = unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            off as libc::off_t,
            len as libc::off_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// the only place that knows which implementation is in play. the fuse layer
// and the control plane hold Arc<dyn BlockStore> and never find out.
pub fn open(cfg: &Config) -> io::Result<Arc<dyn BlockStore>> {
    let block = cfg.block_size;
    Ok(match cfg.store {
        StoreKind::Passthrough => {
            Arc::new(PassthroughStore::open(cfg.image_path()?, cfg.read_only)?)
        }
        StoreKind::FullCopy => Arc::new(FullCopyStore::open(
            cfg.golden_path()?,
            cfg.image_path()?,
            cfg.reseed,
        )?),
        StoreKind::DirtyLog => Arc::new(DirtyLogStore::open(
            cfg.golden_path()?,
            cfg.image_path()?,
            block,
            cfg.reseed,
        )?),
        StoreKind::Overlay => Arc::new(OverlayStore::open(
            cfg.golden_path()?,
            cfg.image_path()?,
            block,
        )?),
        StoreKind::Cow => Arc::new(CowStore::open(
            cfg.golden_path()?,
            block,
            cfg.overlay_max_bytes,
        )?),
    })
}
