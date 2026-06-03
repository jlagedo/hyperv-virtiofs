//! A tiny, dependency-free rate limiter for `tracing` warnings on guest-triggerable
//! paths. Per the CLAUDE.md logging rule, anything a guest can provoke repeatedly
//! (a bad descriptor, an out-of-range DMA) must not be able to flood the log.
//!
//! [`warn_ratelimited!`] emits at most one `tracing::warn!` per [`WINDOW_MS`] *per
//! call site* (each expansion gets its own atomic clock). This is the local stand-in
//! for OpenVMM's `tracelimit` crate, which we deliberately don't pull in.

/// One emission per call site per this window (milliseconds).
const WINDOW_MS: u64 = 5_000;

/// Milliseconds since first use of the rate limiter. A process-wide monotonic
/// clock (an `Instant` can't be `const`-initialised, so it's lazily created).
fn now_ms() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Returns `true` for the one caller that may emit now, advancing `last`. `last`
/// holds the ms timestamp of the previous emission (0 = never). The CAS makes a
/// single winner under concurrency; losers are suppressed.
pub(crate) fn should_emit(last: &std::sync::atomic::AtomicU64) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    let now = now_ms().max(1); // reserve 0 for "never emitted"
    let prev = last.load(Relaxed);
    if prev == 0 || now.saturating_sub(prev) >= WINDOW_MS {
        last.compare_exchange(prev, now, Relaxed, Relaxed).is_ok()
    } else {
        false
    }
}

/// `tracing::warn!`, but at most once per [`WINDOW_MS`] per call site. Takes the
/// same argument forms as `tracing::warn!`.
macro_rules! warn_ratelimited {
    ($($arg:tt)*) => {{
        static LAST_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if $crate::ratelimit::should_emit(&LAST_MS) {
            tracing::warn!($($arg)*);
        }
    }};
}
pub(crate) use warn_ratelimited;
