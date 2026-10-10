// the dirty bitmap shared by every resettable store. one bit per block, packed
// into atomic words so the write path can mark a block without a lock.

use std::sync::atomic::{AtomicU64, Ordering};

const BITS: usize = 64;

#[derive(Debug)]
pub struct BlockMap {
    words: Vec<AtomicU64>,
    blocks: usize,
}

impl BlockMap {
    pub fn new(blocks: usize) -> Self {
        BlockMap {
            words: (0..blocks.div_ceil(BITS)).map(|_| AtomicU64::new(0)).collect(),
            blocks,
        }
    }

    pub fn blocks(&self) -> usize {
        self.blocks
    }

    // true if this call was the one that set the bit, so callers can tell a
    // first touch from a rewrite
    pub fn set(&self, i: usize) -> bool {
        if i >= self.blocks {
            return false;
        }
        let prev = self.words[i / BITS].fetch_or(1u64 << (i % BITS), Ordering::AcqRel);
        prev & (1u64 << (i % BITS)) == 0
    }

    pub fn test(&self, i: usize) -> bool {
        if i >= self.blocks {
            return false;
        }
        self.words[i / BITS].load(Ordering::Acquire) & (1u64 << (i % BITS)) != 0
    }

    pub fn clear_all(&self) {
        for w in &self.words {
            w.store(0, Ordering::Release);
        }
    }

    pub fn count(&self) -> u64 {
        self.words
            .iter()
            .map(|w| u64::from(w.load(Ordering::Acquire).count_ones()))
            .sum()
    }

    // visit each maximal run of consecutive dirty blocks as (first, len). the
    // stores that copy or punch per extent care about runs, not block counts.
    pub fn for_each_run<F: FnMut(usize, usize)>(&self, mut f: F) {
        let mut run_start: Option<usize> = None;
        for i in 0..self.blocks {
            if self.test(i) {
                run_start.get_or_insert(i);
            } else if let Some(s) = run_start.take() {
                f(s, i - s);
            }
        }
        if let Some(s) = run_start {
            f(s, self.blocks - s);
        }
    }

    pub fn runs(&self) -> u64 {
        let mut n = 0u64;
        self.for_each_run(|_, _| n += 1);
        n
    }
}

// inclusive first and last block covering [off, off + len)
pub fn block_span(off: u64, len: usize, block: usize) -> (usize, usize) {
    let first = (off / block as u64) as usize;
    let last = ((off + len.max(1) as u64 - 1) / block as u64) as usize;
    (first, last)
}
