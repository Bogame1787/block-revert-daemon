// golden image plus an in-memory overlay, reverted by dropping the map. no
// syscall and no disk touched, but each block is its own allocation so the
// revert still costs one free per dirty block.

// the overlay is volatile and bounded by RAM, so a guard refuses writes past a
// configured ceiling rather than letting a runaway guest take the host down.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::blockmap::{BlockMap, block_span};
use super::{BlockStore, MarkReport, ResetReport, StoreInfo, pread_exact, pwrite_all};

#[derive(Debug)]
pub struct CowStore {
    golden: File,
    golden_path: PathBuf,
    // block index to its diverged contents. the bitmap mirrors the key set so
    // the read path can ask "is this block mine" without taking the lock.
    overlay: RwLock<HashMap<usize, Box<[u8]>>>,
    dirty: BlockMap,
    size: AtomicU64,
    block: usize,
    max_bytes: u64,
    held_bytes: AtomicU64,
}

impl CowStore {
    pub fn open(golden: &Path, block: usize, max_bytes: u64) -> io::Result<Self> {
        let golden_f = OpenOptions::new().read(true).write(true).open(golden)?;
        let size = golden_f.metadata()?.len();
        Ok(CowStore {
            golden: golden_f,
            golden_path: golden.to_path_buf(),
            overlay: RwLock::new(HashMap::new()),
            dirty: BlockMap::new((size as usize).div_ceil(block)),
            size: AtomicU64::new(size),
            block,
            max_bytes,
            held_bytes: AtomicU64::new(0),
        })
    }

    fn span_of(&self, b: usize) -> usize {
        let off = (b * self.block) as u64;
        let size = self.size.load(Ordering::Acquire);
        (self.block as u64).min(size.saturating_sub(off)) as usize
    }

    fn write_guard(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<usize, Box<[u8]>>> {
        match self.overlay.write() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        }
    }

    fn read_guard(&self) -> std::sync::RwLockReadGuard<'_, HashMap<usize, Box<[u8]>>> {
        match self.overlay.read() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        }
    }
}

impl BlockStore for CowStore {
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

        // the common case by far is that nothing in range has diverged, and it
        // costs one bitmap scan plus one pread
        if (first..=last).all(|b| !self.dirty.test(b)) {
            return pread_exact(&self.golden, off, &mut buf[..want]);
        }

        let map = self.read_guard();
        let mut done = 0usize;
        let mut b = first;
        while b <= last && done < want {
            let b_off = (b * self.block) as u64;
            let in_block = (off + done as u64 - b_off) as usize;
            let take = (self.block - in_block).min(want - done);

            match map.get(&b) {
                Some(blk) => {
                    let end = (in_block + take).min(blk.len());
                    if end > in_block {
                        buf[done..done + (end - in_block)].copy_from_slice(&blk[in_block..end]);
                    }
                    // a short block at the tail of the image leaves the rest as
                    // whatever the caller gave us, which is correct because the
                    // image genuinely ends there
                    done += take;
                }
                None => {
                    let n = pread_exact(&self.golden, off + done as u64, &mut buf[done..done + take])?;
                    done += n;
                    if n < take {
                        break;
                    }
                }
            }
            b += 1;
        }
        Ok(done)
    }

    fn write_at(&self, off: u64, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let (first, last) = block_span(off, buf.len(), self.block);
        let mut map = self.write_guard();

        // admission control before anything is mutated, so a refused write
        // leaves no half-applied state
        let new_blocks = (first..=last).filter(|b| !map.contains_key(b)).count() as u64;
        if self.max_bytes > 0 {
            let projected =
                self.held_bytes.load(Ordering::Acquire) + new_blocks * self.block as u64;
            if projected > self.max_bytes {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
        }

        let mut done = 0usize;
        for b in first..=last {
            let span = self.span_of(b);
            if span == 0 {
                break;
            }
            let b_off = (b * self.block) as u64;
            let in_block = (off + done as u64 - b_off) as usize;
            let take = (span - in_block).min(buf.len() - done);

            if !map.contains_key(&b) {
                // read-modify-write on first touch. a write covering the whole
                // block skips the read, so aligned guest io is cheaper.
                let mut blk = vec![0u8; span];
                if !(in_block == 0 && take == span) {
                    pread_exact(&self.golden, b_off, &mut blk)?;
                }
                map.insert(b, blk.into_boxed_slice());
                self.held_bytes.fetch_add(span as u64, Ordering::AcqRel);
            }

            if let Some(blk) = map.get_mut(&b) {
                blk[in_block..in_block + take].copy_from_slice(&buf[done..done + take]);
            }
            self.dirty.set(b);
            done += take;
            if done >= buf.len() {
                break;
            }
        }
        self.size.fetch_max(off + done as u64, Ordering::AcqRel);
        Ok(done)
    }

    fn flush(&self, _datasync: bool) -> io::Result<()> {
        // the overlay is memory, so there is nothing to make durable. a crash
        // loses the diverged blocks, which is intended for a disposable vm.
        Ok(())
    }

    fn truncate(&self, size: u64) -> io::Result<()> {
        let mut map = self.write_guard();
        let cutoff = (size as usize).div_ceil(self.block);
        map.retain(|b, _| *b < cutoff);
        self.size.store(size, Ordering::Release);
        Ok(())
    }

    fn reset(&self) -> io::Result<ResetReport> {
        let t = Instant::now();
        let runs = self.dirty.runs();
        let mut map = self.write_guard();
        let blocks = map.len() as u64;
        let bytes = self.held_bytes.swap(0, Ordering::AcqRel);
        map.clear();
        drop(map);
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
        let mut map = self.write_guard();
        let mut blocks = 0u64;
        let mut bytes = 0u64;
        // fold in block order so the writes to golden are sequential
        let mut keys: Vec<usize> = map.keys().copied().collect();
        keys.sort_unstable();
        for b in keys {
            if let Some(blk) = map.get(&b) {
                pwrite_all(&self.golden, (b * self.block) as u64, blk)?;
                blocks += 1;
                bytes += blk.len() as u64;
            }
        }
        self.golden.sync_data()?;
        map.clear();
        drop(map);
        self.held_bytes.store(0, Ordering::Release);
        self.dirty.clear_all();
        Ok(MarkReport {
            blocks_folded: blocks,
            bytes_folded: bytes,
            micros: t.elapsed().as_micros() as u64,
        })
    }

    fn backing_fd(&self) -> Option<BorrowedFd<'_>> {
        // golden plus memory, so no single descriptor represents it
        None
    }

    fn info(&self) -> StoreInfo {
        let blocks = self.dirty.count();
        StoreInfo {
            kind: "cow",
            path: self.golden_path.clone(),
            golden: Some(self.golden_path.clone()),
            size: self.size(),
            block_size: self.block as u32,
            read_only: false,
            resettable: true,
            private_bytes: self.held_bytes.load(Ordering::Acquire),
            dirty_blocks: blocks,
            dirty_runs: self.dirty.runs(),
        }
    }
}
