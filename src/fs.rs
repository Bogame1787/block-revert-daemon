// the fuse filesystem itself. the namespace is one root directory holding one
// regular file, the disk image. firecracker opens that file and issues
// positional reads and writes, so lookup, getattr, open, read, write, fsync and
// statfs are the whole hot surface. readdir only exists so the mount can be
// inspected by hand.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::{
    AccessFlags, BackingId, BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem,
    FopenFlags, Generation, INodeNo, InitFlags, KernelConfig, LockOwner, OpenAccMode, OpenFlags,
    ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use log::{debug, error, info, warn};

use crate::config::{CacheMode, Config};
use crate::metrics::Metrics;
use crate::store::BlockStore;

pub const ROOT_INO: INodeNo = INodeNo(1);
pub const IMAGE_INO: INodeNo = INodeNo(2);

// most the kernel may ask for in one read. it cannot exceed this given our
// max_write and the kernel's max_pages, but bound it anyway so a malformed
// request cannot make us allocate without limit.
const MAX_IO: usize = 8 << 20;

thread_local! {
    // each event loop thread handles one request at a time, so a per-thread
    // scratch buffer saves an allocation and a memset on every read. it grows
    // to the largest request that thread has seen and then settles.
    static IO_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn from_nanos(n: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(n)
}

pub struct ImageFs {
    cfg: Arc<Config>,
    store: Arc<dyn BlockStore>,
    metrics: Arc<Metrics>,
    uid: u32,
    gid: u32,
    // clock fields reported through getattr. kept here rather than restatted
    // from the backing file, which for a cow store is not what the guest sees.
    born_nanos: u64,
    mtime_nanos: AtomicU64,
    // descriptor handed to the kernel in passthrough mode. it must outlive
    // every file handle that referenced it, so it is dropped with the
    // filesystem rather than at release time.
    backing: Mutex<Option<Arc<BackingId>>>,
    open_handles: AtomicU64,
    next_fh: AtomicU64,
}

impl std::fmt::Debug for ImageFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageFs")
            .field("name", &self.cfg.name)
            .field("cache", &self.cfg.cache.as_str())
            .field("store", &self.store.info().kind)
            .finish()
    }
}

impl ImageFs {
    pub fn new(cfg: Arc<Config>, store: Arc<dyn BlockStore>, metrics: Arc<Metrics>) -> Self {
        let now = now_nanos();
        ImageFs {
            cfg,
            store,
            metrics,
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            born_nanos: now,
            mtime_nanos: AtomicU64::new(now),
            backing: Mutex::new(None),
            open_handles: AtomicU64::new(0),
            next_fh: AtomicU64::new(1),
        }
    }

    fn touch(&self) {
        self.mtime_nanos.store(now_nanos(), Ordering::Relaxed);
    }

    fn image_attr(&self) -> FileAttr {
        let size = self.store.size();
        let mtime = from_nanos(self.mtime_nanos.load(Ordering::Relaxed));
        FileAttr {
            ino: IMAGE_INO,
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: from_nanos(self.born_nanos),
            kind: FileType::RegularFile,
            perm: if self.store.read_only() { 0o400 } else { 0o600 },
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn root_attr(&self) -> FileAttr {
        let born = from_nanos(self.born_nanos);
        FileAttr {
            ino: ROOT_INO,
            size: 0,
            blocks: 0,
            atime: born,
            mtime: born,
            ctime: born,
            crtime: born,
            kind: FileType::Directory,
            perm: 0o700,
            nlink: 2,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}

impl Filesystem for ImageFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        let have = config.capabilities();

        // large requests mean fewer round trips per megabyte moved, which is
        // the cheapest win available to a fuse daemon.
        match config.set_max_write(self.cfg.max_write) {
            Ok(v) => info!("max_write {} KiB", v / 1024),
            Err(cap) => {
                warn!(
                    "max_write {} KiB refused, kernel allows {} KiB",
                    self.cfg.max_write / 1024,
                    cap / 1024
                );
                let _ = config.set_max_write(cap);
            }
        }
        match config.set_max_readahead(self.cfg.max_readahead) {
            Ok(v) => info!("max_readahead {} KiB", v / 1024),
            Err(cap) => {
                warn!("max_readahead capped to {} KiB", cap / 1024);
                let _ = config.set_max_readahead(cap);
            }
        }
        if let Err(cap) = config.set_max_background(self.cfg.max_background) {
            warn!("max_background capped to {cap}");
            let _ = config.set_max_background(cap);
        }

        // asked for unconditionally: these only ever reduce the number of times
        // the kernel has to talk to us.
        let mut want = InitFlags::FUSE_ASYNC_READ
            | InitFlags::FUSE_BIG_WRITES
            | InitFlags::FUSE_MAX_PAGES
            | InitFlags::FUSE_PARALLEL_DIROPS
            | InitFlags::FUSE_ASYNC_DIO;

        if self.cfg.writeback {
            want |= InitFlags::FUSE_WRITEBACK_CACHE;
        }
        if self.cfg.cache == CacheMode::Passthrough {
            if !have.contains(InitFlags::FUSE_PASSTHROUGH) {
                error!("kernel does not offer FUSE_PASSTHROUGH, cannot serve in passthrough mode");
                return Err(std::io::Error::from_raw_os_error(libc::ENOTSUP));
            }
            want |= InitFlags::FUSE_PASSTHROUGH;
            // the kernel needs to be told how deep it may chain a passthrough
            // onto another stacked filesystem.
            if let Err(cap) = config.set_max_stack_depth(1) {
                warn!("max_stack_depth capped to {cap}");
            }
        }

        let missing = want.difference(have);
        if !missing.is_empty() {
            warn!("kernel does not offer {missing:?}, continuing without");
        }
        if let Err(rejected) = config.add_capabilities(want.intersection(have)) {
            warn!("kernel rejected capabilities {rejected:?}");
        }

        if self.cfg.writeback && !have.contains(InitFlags::FUSE_WRITEBACK_CACHE) {
            warn!("writeback requested but unavailable, writes stay write-through");
        }

        info!(
            "serving {} as {} in {} mode, {} threads, {} bytes",
            self.cfg.served_path().display(),
            self.cfg.name,
            self.cfg.cache.as_str(),
            self.cfg.threads,
            self.store.size(),
        );
        Ok(())
    }

    fn destroy(&mut self) {
        // drop the passthrough registration before the channel goes away,
        // otherwise the close ioctl has nothing to talk to.
        if let Ok(mut g) = self.backing.lock() {
            g.take();
        }
        info!("session destroyed; {}", self.metrics.summary_line());
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let started = Instant::now();
        if parent != ROOT_INO {
            self.metrics.lookup.record(started, 0, false);
            reply.error(Errno::ENOENT);
            return;
        }
        if name == OsStr::new(self.cfg.name.as_str()) {
            self.metrics.lookup.record(started, 0, true);
            reply.entry(&self.cfg.entry_ttl, &self.image_attr(), Generation(0));
        } else {
            self.metrics.lookup.record(started, 0, false);
            reply.error(Errno::ENOENT);
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let started = Instant::now();
        match ino {
            ROOT_INO => {
                self.metrics.getattr.record(started, 0, true);
                reply.attr(&self.cfg.attr_ttl, &self.root_attr());
            }
            IMAGE_INO => {
                self.metrics.getattr.record(started, 0, true);
                reply.attr(&self.cfg.attr_ttl, &self.image_attr());
            }
            _ => {
                self.metrics.getattr.record(started, 0, false);
                reply.error(Errno::ENOENT);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let started = Instant::now();
        if ino != IMAGE_INO {
            self.metrics.setattr.record(started, 0, false);
            reply.error(Errno::EPERM);
            return;
        }
        // ownership, mode and timestamps are fixed by the daemon; accepting the
        // request and reporting the real attributes back is friendlier than
        // failing a chmod that changes nothing. resizing is the one thing that
        // genuinely has to reach the store.
        if let Some(new_size) = size {
            if let Err(e) = self.store.truncate(new_size) {
                self.metrics.setattr.record(started, 0, false);
                reply.error(Errno::from(e));
                return;
            }
            self.touch();
        }
        self.metrics.setattr.record(started, 0, true);
        reply.attr(&self.cfg.attr_ttl, &self.image_attr());
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let started = Instant::now();
        if ino != IMAGE_INO {
            self.metrics.open.record(started, 0, false);
            reply.error(Errno::ENOENT);
            return;
        }
        if self.store.read_only() && flags.acc_mode() != OpenAccMode::O_RDONLY {
            self.metrics.open.record(started, 0, false);
            reply.error(Errno::EROFS);
            return;
        }

        let fh = FileHandle(self.next_fh.fetch_add(1, Ordering::Relaxed));

        match self.cfg.cache {
            CacheMode::Passthrough => {
                // one backing registration is shared by every handle, which the
                // kernel requires: reusing the id is what lets it map several
                // open files onto the same backing inode.
                let existing = {
                    let guard = match self.backing.lock() {
                        Ok(g) => g,
                        Err(e) => e.into_inner(),
                    };
                    guard.clone()
                };
                let id = match existing {
                    Some(id) => id,
                    None => {
                        let Some(fd) = self.store.backing_fd() else {
                            self.metrics.open.record(started, 0, false);
                            reply.error(Errno::ENOTSUP);
                            return;
                        };
                        match reply.open_backing(fd) {
                            Ok(new_id) => {
                                let new_id = Arc::new(new_id);
                                let mut guard = match self.backing.lock() {
                                    Ok(g) => g,
                                    Err(e) => e.into_inner(),
                                };
                                guard.get_or_insert(new_id).clone()
                            }
                            Err(e) => {
                                error!("failed to register backing descriptor: {e}");
                                self.metrics.open.record(started, 0, false);
                                reply.error(Errno::from(e));
                                return;
                            }
                        }
                    }
                };
                self.open_handles.fetch_add(1, Ordering::Relaxed);
                self.metrics.open.record(started, 0, true);
                reply.opened_passthrough(fh, FopenFlags::FOPEN_KEEP_CACHE, &id);
            }
            CacheMode::Direct => {
                self.open_handles.fetch_add(1, Ordering::Relaxed);
                self.metrics.open.record(started, 0, true);
                reply.opened(fh, FopenFlags::FOPEN_DIRECT_IO);
            }
            CacheMode::Cached => {
                // keep the cache across opens. nothing changes the image behind
                // the kernel's back except a reset, and a reset invalidates
                // explicitly.
                self.open_handles.fetch_add(1, Ordering::Relaxed);
                self.metrics.open.record(started, 0, true);
                reply.opened(fh, FopenFlags::FOPEN_KEEP_CACHE);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let started = Instant::now();
        if ino != IMAGE_INO {
            self.metrics.read.record(started, 0, false);
            reply.error(Errno::ENOENT);
            return;
        }
        let want = size as usize;
        if want > MAX_IO {
            warn!("read of {want} bytes exceeds the {MAX_IO} byte ceiling");
            self.metrics.read.record(started, 0, false);
            reply.error(Errno::EINVAL);
            return;
        }

        IO_BUF.with(|cell| {
            let mut buf = cell.borrow_mut();
            if buf.len() < want {
                buf.resize(want, 0);
            }
            match self.store.read_at(offset, &mut buf[..want]) {
                Ok(n) => {
                    self.metrics.read.record(started, n as u64, true);
                    reply.data(&buf[..n]);
                }
                Err(e) => {
                    debug!("read {want}@{offset} failed: {e}");
                    self.metrics.read.record(started, 0, false);
                    reply.error(Errno::from(e));
                }
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let started = Instant::now();
        if ino != IMAGE_INO {
            self.metrics.write.record(started, 0, false);
            reply.error(Errno::ENOENT);
            return;
        }
        match self.store.write_at(offset, data) {
            Ok(n) => {
                self.touch();
                self.metrics.write.record(started, n as u64, true);
                reply.written(n as u32);
            }
            Err(e) => {
                debug!("write {}@{offset} failed: {e}", data.len());
                self.metrics.write.record(started, 0, false);
                reply.error(Errno::from(e));
            }
        }
    }

    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        // flush fires on every close of a descriptor, not on fsync. it is not a
        // durability point, so it deliberately does no io. forcing a sync here
        // would make every close as expensive as a barrier.
        let started = Instant::now();
        let ok = ino == IMAGE_INO;
        self.metrics.flush.record(started, 0, ok);
        if ok {
            reply.ok()
        } else {
            reply.error(Errno::ENOENT)
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let started = Instant::now();
        if ino != IMAGE_INO {
            self.metrics.fsync.record(started, 0, false);
            reply.error(Errno::ENOENT);
            return;
        }
        match self.store.flush(datasync) {
            Ok(()) => {
                self.metrics.fsync.record(started, 0, true);
                reply.ok();
            }
            Err(e) => {
                self.metrics.fsync.record(started, 0, false);
                reply.error(Errno::from(e));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // the passthrough registration is intentionally not dropped here. the
        // kernel may still hold references to it, and re-registering per open
        // would defeat the point of sharing one backing id.
        self.open_handles.fetch_sub(1, Ordering::Relaxed);
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if ino == ROOT_INO {
            reply.opened(FileHandle(0), FopenFlags::empty());
        } else {
            reply.error(Errno::ENOTDIR);
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        if ino != ROOT_INO {
            reply.error(Errno::ENOTDIR);
            return;
        }
        let entries: [(INodeNo, FileType, &str); 3] = [
            (ROOT_INO, FileType::Directory, "."),
            (ROOT_INO, FileType::Directory, ".."),
            (IMAGE_INO, FileType::RegularFile, self.cfg.name.as_str()),
        ];
        for (i, (e_ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            // the offset handed back is where to resume, so it is one past the
            // entry just added.
            if reply.add(*e_ino, i as u64 + 1, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn access(&self, _req: &Request, ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        if ino == ROOT_INO || ino == IMAGE_INO {
            reply.ok();
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        const BSIZE: u32 = 4096;
        let blocks = self.store.size().div_ceil(u64::from(BSIZE));
        // the image is fixed size, so there is no free space to report. saying
        // zero free is honest and stops anything trying to grow into it.
        reply.statfs(blocks, 0, 0, 2, 0, BSIZE, 255, BSIZE);
    }
}
