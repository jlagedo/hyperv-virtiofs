//! Hand-rolled blocking worker pool for FUSE request dispatch (strategy A).
//!
//! N OS threads drain an `Arc<Mutex<VecDeque<Job>> + Condvar>`; the offload
//! device's dispatcher submits one job per popped descriptor chain and the job
//! runs the blocking `fuse::Session::dispatch` (host filesystem I/O) off the
//! async task. No external crate — the pool is ~100 lines and the only
//! synchronization is the queue lock (CLAUDE.md: hand-rolled over new deps).
//!
//! Sizing comes from `VIRTIO_HDV_WORKERS` (see [`configured_workers`]); `0`
//! workers means [`WorkerPool::submit`] runs the job inline on the calling
//! thread — the knob that also serves as the A/B switch back to serial
//! behaviour. Drop semantics: signal shutdown, let workers **drain the queue**,
//! then join — a submitted job is never lost, which matters because in A2 a
//! dropped job is a descriptor that never completes (a guest request that hangs
//! forever).

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// A unit of blocking work. `FnOnce` because a job carries its descriptor work
/// item through by value (pop → dispatch → completion channel).
pub(crate) type Job = Box<dyn FnOnce() + Send + 'static>;

/// Queue state under the pool's one lock.
#[derive(Default)]
struct PoolState {
    jobs: VecDeque<Job>,
    shutdown: bool,
}

/// What workers sleep on.
#[derive(Default)]
struct Shared {
    state: Mutex<PoolState>,
    cond: Condvar,
}

/// Fixed-size blocking worker pool. See the module docs for semantics.
pub(crate) struct WorkerPool {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    /// Spawn `workers` OS threads. `0` is valid and means "inline": `submit`
    /// runs jobs on the calling thread (low queue depth can win this way, and
    /// it keeps one code path for the A/B comparison).
    pub fn new(workers: usize) -> Self {
        let shared = Arc::new(Shared::default());
        let workers = (0..workers)
            .map(|i| {
                let shared = shared.clone();
                std::thread::Builder::new()
                    .name(format!("virtio-hdv-fuse-{i}"))
                    .spawn(move || worker_loop(&shared))
                    .expect("spawn pool worker")
            })
            .collect();
        Self { shared, workers }
    }

    /// Queue `job` (or run it inline when the pool has no threads).
    pub fn submit(&self, job: Job) {
        if self.workers.is_empty() {
            run_job(job);
            return;
        }
        {
            // Poison-tolerant: pool threads are un-guarded; a panic caught by
            // `run_job` can't poison (it never holds the lock), but stay
            // consistent with the rest of the crate's lock discipline.
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.jobs.push_back(job);
        }
        self.shared.cond.notify_one();
    }
}

impl Drop for WorkerPool {
    /// Shut down: workers finish everything already queued, then exit; join so
    /// no job is ever abandoned mid-dispatch.
    fn drop(&mut self) {
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.shutdown = true;
        }
        self.shared.cond.notify_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

/// Pop-run loop: drain jobs, then exit on shutdown (drain *before* the shutdown
/// check, so a queued job survives a racing drop).
fn worker_loop(shared: &Shared) {
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(job) = state.jobs.pop_front() {
            drop(state);
            run_job(job);
            state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        } else if state.shutdown {
            return;
        } else {
            state = shared.cond.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Run one job, containing any panic so the worker thread survives. A panic
/// here is a host-side bug (the FUSE/host-FS path), but killing the worker
/// would silently shrink the pool; in A2 the job's completion send happens in
/// a scope that outlives the dispatch call, so containment keeps the device
/// alive. Never aborts — we're a DLL in someone else's process.
fn run_job(job: Job) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
        crate::ratelimit::warn_ratelimited!("panic in pool worker job (contained)");
    }
}

/// Worker count from `VIRTIO_HDV_WORKERS`:
/// - unset or `0` → `0` (the caller keeps the serial/inline path — the gate),
/// - a positive number → exactly that many threads,
/// - anything else (e.g. `auto`) → `clamp(cpus, 4..=16)`.
pub(crate) fn configured_workers() -> usize {
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    parse_workers(std::env::var("VIRTIO_HDV_WORKERS").ok().as_deref(), cpus)
}

/// Pure part of [`configured_workers`], split out for unit tests.
fn parse_workers(val: Option<&str>, cpus: usize) -> usize {
    match val {
        None => 0,
        Some(s) => match s.trim().parse::<usize>() {
            Ok(n) => n,
            Err(_) => cpus.clamp(4, 16),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    /// Every submitted job runs, and drop drains the queue before returning.
    #[test]
    fn runs_all_jobs_and_drains_on_drop() {
        let ran = Arc::new(AtomicUsize::new(0));
        let pool = WorkerPool::new(4);
        for _ in 0..100 {
            let ran = ran.clone();
            pool.submit(Box::new(move || {
                ran.fetch_add(1, Relaxed);
            }));
        }
        drop(pool);
        assert_eq!(ran.load(Relaxed), 100);
    }

    /// Zero workers = inline execution on the submitting thread.
    #[test]
    fn zero_workers_runs_inline() {
        let pool = WorkerPool::new(0);
        let caller = std::thread::current().id();
        let ran_on = Arc::new(Mutex::new(None));
        let slot = ran_on.clone();
        pool.submit(Box::new(move || {
            *slot.lock().unwrap() = Some(std::thread::current().id());
        }));
        // Inline: already ran, on this thread.
        assert_eq!(*ran_on.lock().unwrap(), Some(caller));
    }

    /// A panicking job is contained; the worker survives and runs later jobs.
    #[test]
    fn panicking_job_does_not_kill_worker() {
        let pool = WorkerPool::new(1);
        pool.submit(Box::new(|| panic!("boom")));
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        pool.submit(Box::new(move || {
            r.fetch_add(1, Relaxed);
        }));
        drop(pool);
        assert_eq!(ran.load(Relaxed), 1);
    }

    /// The `VIRTIO_HDV_WORKERS` parse rules: unset/0 gate off, N is exact,
    /// junk falls back to clamp(cpus, 4..=16).
    #[test]
    fn parse_workers_rules() {
        assert_eq!(parse_workers(None, 8), 0);
        assert_eq!(parse_workers(Some("0"), 8), 0);
        assert_eq!(parse_workers(Some("6"), 8), 6);
        assert_eq!(parse_workers(Some("32"), 8), 32);
        assert_eq!(parse_workers(Some("auto"), 8), 8);
        assert_eq!(parse_workers(Some("auto"), 2), 4);
        assert_eq!(parse_workers(Some("auto"), 64), 16);
        assert_eq!(parse_workers(Some(""), 8), 8);
    }
}
