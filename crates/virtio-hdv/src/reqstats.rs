//! Request-path profiling for the offload device (`VIRTIO_HDV_REQ_STATS`).
//!
//! Decomposes the residual per-op cost of the serial request path — the
//! question the offload work left open (`docs/perf-optimization.md`,
//! *Measured outcome*): of the ~115 µs FUSE round-trip, how much is the FUSE
//! dispatch itself (host filesystem work), how much the used-ring write, how
//! much the `HdvDeliverGuestInterrupt` injection, and how much the
//! dispatcher/pool plumbing around them. Each stage gets a `(sum, count, max)`
//! triple in µs; the emitted means add up against the benchmark's per-op time,
//! and what they *don't* cover is the guest+wakeup share.
//!
//! Stages (all host-side, per request unless noted):
//!
//! - `pop` — one `try_next` drain pass that yielded work (per batch).
//! - `inline` / `pool` — `dispatch_one` on the dispatcher task vs a pool thread.
//! - `pool_wait` — fan-out `submit` → job start (queue + thread hop).
//! - `done_hop` — job end → the dispatcher resuming to complete it.
//! - `complete` — `VirtioQueue::complete` (used-ring write **including** the
//!   MSI delivery when not suppressed).
//! - `deliver` — `HdvDeliverGuestInterrupt` alone (also counts *all* MSI-X
//!   signals, so `delivers / ops` reads as the event-idx suppression ratio).
//!
//! Same conventions as the aperture-cache stats (`mem.rs`): one process-global
//! instance, counters skipped entirely unless the env var is set (a relaxed
//! `fetch_add` on a shared line is real cost under parallel dispatch), emitted
//! as a structured DEBUG event (target `virtio_hdv::reqpath`) by op count or
//! wall clock, whichever comes first.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;
use std::time::Instant;

/// Emit a summary at most every this many completed ops…
const STATS_EVERY: u64 = 4096;
/// …or this often by wall clock, so a stalled run still reports.
const STATS_EVERY_MS: u64 = 500;

/// One timed stage: total µs, sample count, worst single sample.
#[derive(Default)]
struct StageStat {
    sum_us: AtomicU64,
    count: AtomicU64,
    max_us: AtomicU64,
}

impl StageStat {
    fn record_us(&self, us: u64) {
        self.sum_us.fetch_add(us, Relaxed);
        self.count.fetch_add(1, Relaxed);
        self.max_us.fetch_max(us, Relaxed);
    }

    fn count_now(&self) -> u64 {
        self.count.load(Relaxed)
    }

    /// Mean µs (0 when no samples — reads as "stage never ran").
    fn mean_us(&self) -> u64 {
        self.sum_us
            .load(Relaxed)
            .checked_div(self.count.load(Relaxed))
            .unwrap_or(0)
    }

    fn max(&self) -> u64 {
        self.max_us.load(Relaxed)
    }
}

/// Process-global request-path counters. All methods are no-ops (one branch,
/// no shared write) unless `VIRTIO_HDV_REQ_STATS` is set.
pub struct ReqStats {
    enabled: bool,
    pop: StageStat,
    inline: StageStat,
    pool: StageStat,
    pool_wait: StageStat,
    done_hop: StageStat,
    complete: StageStat,
    deliver: StageStat,
    /// Total work items across all counted `pop` batches.
    batch_items: AtomicU64,
    /// Largest single drain batch.
    batch_max: AtomicU64,
    /// Drain passes that yielded exactly one item (the inline-eligible case).
    lone_batches: AtomicU64,
    /// Clock base + last-emit stamp for the wall-clock emit floor.
    start: Instant,
    last_emit_ms: AtomicU64,
}

/// The global instance (the process hosts one VM's devices — Model A — so
/// per-process aggregation is per-VM aggregation).
pub fn global() -> &'static ReqStats {
    static GLOBAL: OnceLock<ReqStats> = OnceLock::new();
    GLOBAL.get_or_init(ReqStats::new)
}

impl ReqStats {
    fn new() -> Self {
        Self {
            enabled: std::env::var_os("VIRTIO_HDV_REQ_STATS").is_some(),
            pop: StageStat::default(),
            inline: StageStat::default(),
            pool: StageStat::default(),
            pool_wait: StageStat::default(),
            done_hop: StageStat::default(),
            complete: StageStat::default(),
            deliver: StageStat::default(),
            batch_items: AtomicU64::new(0),
            batch_max: AtomicU64::new(0),
            lone_batches: AtomicU64::new(0),
            start: Instant::now(),
            last_emit_ms: AtomicU64::new(0),
        }
    }

    /// Start a stage timer — `None` when stats are off, so the disabled path
    /// never reads the clock.
    pub fn now(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    fn rec(&self, stage: &StageStat, t0: Option<Instant>) {
        if let Some(t0) = t0 {
            stage.record_us(t0.elapsed().as_micros() as u64);
        }
    }

    /// One drain pass that popped `items ≥ 1` work items.
    pub fn rec_pop(&self, t0: Option<Instant>, items: usize) {
        if t0.is_some() {
            self.batch_items.fetch_add(items as u64, Relaxed);
            self.batch_max.fetch_max(items as u64, Relaxed);
            if items == 1 {
                self.lone_batches.fetch_add(1, Relaxed);
            }
        }
        self.rec(&self.pop, t0);
    }

    /// `dispatch_one` on the dispatcher task (adaptive-inline path).
    pub fn rec_inline(&self, t0: Option<Instant>) {
        self.rec(&self.inline, t0);
    }

    /// `dispatch_one` on a pool thread.
    pub fn rec_pool(&self, t0: Option<Instant>) {
        self.rec(&self.pool, t0);
    }

    /// Fan-out `submit` → the job actually starting on a pool thread.
    pub fn rec_pool_wait(&self, t0: Option<Instant>) {
        self.rec(&self.pool_wait, t0);
    }

    /// Job finished on the pool → dispatcher resumed to complete it.
    pub fn rec_done_hop(&self, t0: Option<Instant>) {
        self.rec(&self.done_hop, t0);
    }

    /// `VirtioQueue::complete` (used-ring write + MSI when unsuppressed).
    /// The per-op terminal event — also drives the periodic emit.
    pub fn rec_complete(&self, t0: Option<Instant>) {
        self.rec(&self.complete, t0);
        if t0.is_some() {
            self.maybe_emit();
        }
    }

    /// One `HdvDeliverGuestInterrupt` call (every MSI-X signal lands here).
    pub fn rec_deliver(&self, t0: Option<Instant>) {
        self.rec(&self.deliver, t0);
    }

    /// Emit if due by completed-op count or wall clock (same scheme as the
    /// aperture stats; the CAS keeps concurrent completers from double-emitting).
    fn maybe_emit(&self) {
        let n = self.complete.count_now();
        let due_by_ops = n.is_multiple_of(STATS_EVERY);
        let due_by_time = {
            let elapsed = self.start.elapsed().as_millis() as u64;
            let last = self.last_emit_ms.load(Relaxed);
            elapsed.saturating_sub(last) >= STATS_EVERY_MS
                && self
                    .last_emit_ms
                    .compare_exchange(last, elapsed, Relaxed, Relaxed)
                    .is_ok()
        };
        if due_by_ops || due_by_time {
            self.emit("periodic");
        }
    }

    /// Final summary, hooked to device teardown.
    pub fn emit_final(&self) {
        if self.enabled {
            self.emit("final");
        }
    }

    fn emit(&self, phase: &'static str) {
        tracing::debug!(
            target: "virtio_hdv::reqpath",
            phase,
            ops = self.complete.count_now(),
            batches = self.pop.count_now(),
            batch_items = self.batch_items.load(Relaxed),
            batch_max = self.batch_max.load(Relaxed),
            lone_batches = self.lone_batches.load(Relaxed),
            pop_mean_us = self.pop.mean_us(),
            inline_n = self.inline.count_now(),
            inline_mean_us = self.inline.mean_us(),
            inline_max_us = self.inline.max(),
            pool_n = self.pool.count_now(),
            pool_mean_us = self.pool.mean_us(),
            pool_max_us = self.pool.max(),
            pool_wait_mean_us = self.pool_wait.mean_us(),
            pool_wait_max_us = self.pool_wait.max(),
            done_hop_mean_us = self.done_hop.mean_us(),
            done_hop_max_us = self.done_hop.max(),
            complete_mean_us = self.complete.mean_us(),
            complete_max_us = self.complete.max(),
            delivers = self.deliver.count_now(),
            deliver_mean_us = self.deliver.mean_us(),
            deliver_max_us = self.deliver.max(),
            "request path stats"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stage arithmetic: sum/count/max and the zero-sample mean.
    #[test]
    fn stage_stat_mean_and_max() {
        let s = StageStat::default();
        assert_eq!(s.mean_us(), 0);
        s.record_us(100);
        s.record_us(300);
        s.record_us(20);
        assert_eq!(s.count_now(), 3);
        assert_eq!(s.mean_us(), 140);
        assert_eq!(s.max(), 300);
    }

    /// Disabled stats never start a timer and record nothing.
    #[test]
    fn disabled_stats_are_inert() {
        let stats = ReqStats::new_for_test(false);
        assert!(stats.now().is_none());
        stats.rec_pop(None, 5);
        stats.rec_complete(None);
        assert_eq!(stats.complete.count_now(), 0);
        assert_eq!(stats.batch_items.load(Relaxed), 0);
    }

    /// Batch accounting: items accumulate, max and lone-batch counts track.
    #[test]
    fn batch_accounting() {
        let stats = ReqStats::new_for_test(true);
        stats.rec_pop(stats.now(), 1);
        stats.rec_pop(stats.now(), 4);
        stats.rec_pop(stats.now(), 2);
        assert_eq!(stats.pop.count_now(), 3);
        assert_eq!(stats.batch_items.load(Relaxed), 7);
        assert_eq!(stats.batch_max.load(Relaxed), 4);
        assert_eq!(stats.lone_batches.load(Relaxed), 1);
    }

    impl ReqStats {
        /// Test constructor with an explicit enable flag (the real one reads
        /// the env var, which tests must not depend on).
        fn new_for_test(enabled: bool) -> Self {
            Self {
                enabled,
                ..Self::new()
            }
        }
    }
}
