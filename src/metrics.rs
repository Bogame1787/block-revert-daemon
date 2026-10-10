// lock free counters for the request path. relaxed atomics and a histogram
// bucketed by leading zero count, so recording a sample costs a few
// instructions and no allocation. the ops being measured are microseconds
// long, so a mutex here would be a real share of them.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::Serialize;

// bucket i holds samples in [2^(i-1), 2^i) nanoseconds, so 40 of them covers
// one nanosecond through about nine minutes
const BUCKETS: usize = 40;

#[derive(Debug)]
pub struct OpCounter {
    calls: AtomicU64,
    errors: AtomicU64,
    bytes: AtomicU64,
    nanos: AtomicU64,
    buckets: [AtomicU64; BUCKETS],
}

impl Default for OpCounter {
    fn default() -> Self {
        OpCounter {
            calls: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl OpCounter {
    pub fn record(&self, started: Instant, bytes: u64, ok: bool) {
        let nanos = started.elapsed().as_nanos() as u64;
        self.calls.fetch_add(1, Ordering::Relaxed);
        if !ok {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        self.nanos.fetch_add(nanos, Ordering::Relaxed);
        let bucket = if nanos == 0 {
            0
        } else {
            (64 - nanos.leading_zeros()) as usize
        };
        self.buckets[bucket.min(BUCKETS - 1)].fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> OpSnapshot {
        let calls = self.calls.load(Ordering::Relaxed);
        let nanos = self.nanos.load(Ordering::Relaxed);
        let counts: Vec<u64> = self
            .buckets
            .iter()
            .map(|b| b.load(Ordering::Relaxed))
            .collect();
        OpSnapshot {
            calls,
            errors: self.errors.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            mean_ns: nanos.checked_div(calls).unwrap_or(0),
            p50_ns: percentile(&counts, calls, 50),
            p90_ns: percentile(&counts, calls, 90),
            p99_ns: percentile(&counts, calls, 99),
        }
    }
}

// upper edge of the bucket the percentile falls in, so accurate to a factor of
// two. enough to tell a page cache hit from a round trip.
fn percentile(counts: &[u64], total: u64, pct: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    let target = total * pct / 100;
    let mut seen = 0u64;
    for (i, c) in counts.iter().enumerate() {
        seen += c;
        if seen >= target.max(1) {
            return if i == 0 { 0 } else { 1u64 << (i - 1) };
        }
    }
    0
}

#[derive(Debug, Default, Serialize)]
pub struct OpSnapshot {
    pub calls: u64,
    pub errors: u64,
    pub bytes: u64,
    pub mean_ns: u64,
    pub p50_ns: u64,
    pub p90_ns: u64,
    pub p99_ns: u64,
}

#[derive(Debug, Default)]
pub struct Metrics {
    pub read: OpCounter,
    pub write: OpCounter,
    pub flush: OpCounter,
    pub fsync: OpCounter,
    pub getattr: OpCounter,
    pub lookup: OpCounter,
    pub open: OpCounter,
    pub setattr: OpCounter,
    pub reset: OpCounter,
    pub invalidate: OpCounter,
}

#[derive(Debug, Default, Serialize)]
pub struct MetricsSnapshot {
    pub read: OpSnapshot,
    pub write: OpSnapshot,
    pub flush: OpSnapshot,
    pub fsync: OpSnapshot,
    pub getattr: OpSnapshot,
    pub lookup: OpSnapshot,
    pub open: OpSnapshot,
    pub setattr: OpSnapshot,
    pub reset: OpSnapshot,
    pub invalidate: OpSnapshot,
}

impl Metrics {
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            read: self.read.snapshot(),
            write: self.write.snapshot(),
            flush: self.flush.snapshot(),
            fsync: self.fsync.snapshot(),
            getattr: self.getattr.snapshot(),
            lookup: self.lookup.snapshot(),
            open: self.open.snapshot(),
            setattr: self.setattr.snapshot(),
            reset: self.reset.snapshot(),
            invalidate: self.invalidate.snapshot(),
        }
    }

    // the things worth watching while a guest is running
    pub fn summary_line(&self) -> String {
        let s = self.snapshot();
        format!(
            "read {} ops {} MiB p50 {}us p99 {}us | write {} ops {} MiB p50 {}us p99 {}us | \
             fsync {} | resets {}",
            s.read.calls,
            s.read.bytes / (1 << 20),
            s.read.p50_ns / 1000,
            s.read.p99_ns / 1000,
            s.write.calls,
            s.write.bytes / (1 << 20),
            s.write.p50_ns / 1000,
            s.write.p99_ns / 1000,
            s.fsync.calls,
            s.reset.calls,
        )
    }
}
