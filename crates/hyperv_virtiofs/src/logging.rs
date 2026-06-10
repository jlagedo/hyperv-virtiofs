//! Process-global logging spine for the DLL.
//!
//! Follows OpenVMM's convention (see `CLAUDE.md`, "Logging & diagnostics"): code
//! emits structured [`tracing`] events and never decides *where* they go — routing
//! is set up once here by composing subscriber layers. We install (best-effort) a
//! process-global subscriber that fans every event out to:
//!
//! 1. the caller-installed [`hvfs_set_logger`](crate::hvfs_set_logger) C callback
//!    ([`CallbackLayer`]), and
//! 2. **stderr**, but only when a dev env var asks for it (`VIRTIO_HDV_TRACE`,
//!    `VIRTIO_HDV_APERTURE_STATS`, or `RUST_LOG`) — so an embedding consumer stays
//!    quiet by default while the data-path firehose still works for developers.
//!
//! Because the reused OpenVMM crates (`virtio`, `virtiofs`, `guestmem`, …) also
//! emit via `tracing`, installing this subscriber is what lets a logger consumer
//! finally see the device-host stream. We install with `try_init().ok()`: in a
//! C/Go consumer (no Rust subscriber) we win; if the host process already owns a
//! global subscriber we respect it (and simply don't capture OpenVMM's stream),
//! exactly as OpenVMM's own `debug_output_tracing` does.

use crate::hvfs_log_fn;
use std::ffi::{c_void, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Once, RwLock};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// The caller-installed logger callback plus its opaque context pointer.
struct LoggerSink {
    cb: hvfs_log_fn,
    ctx: *mut c_void,
}

// SAFETY: the ABI contract (`hvfs_set_logger`) requires `cb` to be callable from
// any thread and `ctx` to remain valid for the process lifetime. We only ever read
// the pair and hand `ctx` back to `cb` verbatim, so sharing it across threads is
// sound; the wrapper exists solely to make the global `Send + Sync`.
unsafe impl Send for LoggerSink {}
unsafe impl Sync for LoggerSink {}

/// The installed sink, read per-event so a logger set before *or* after
/// [`init`] both take effect. `None` until `hvfs_set_logger` is called.
static SINK: RwLock<Option<LoggerSink>> = RwLock::new(None);

/// Store (or replace) the process-global logger sink. A `None` callback disables
/// delivery without uninstalling the subscriber.
pub(crate) fn set_sink(cb: hvfs_log_fn, ctx: *mut c_void) {
    if let Ok(mut g) = SINK.write() {
        *g = Some(LoggerSink { cb, ctx });
    }
}

/// Map a `tracing` level to a syslog severity (what the C callback's `level`
/// carries). syslog has no level finer than DEBUG, so TRACE collapses onto it.
fn syslog_level(level: &Level) -> i32 {
    match *level {
        Level::ERROR => 3, // LOG_ERR
        Level::WARN => 4,  // LOG_WARNING
        Level::INFO => 6,  // LOG_INFO
        Level::DEBUG => 7, // LOG_DEBUG
        Level::TRACE => 7, // LOG_DEBUG (no finer syslog severity)
    }
}

/// Renders an event into `"<target>: <message>[ k=v …]"`. The format-args message
/// arrives as the special `message` field; everything else is appended as `k=v`.
#[derive(Default)]
struct LineVisitor {
    message: String,
    fields: String,
}

impl tracing::field::Visit for LineVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        match field.name() {
            "message" => {
                let _ = write!(self.message, "{value:?}");
            }
            name => {
                let _ = write!(self.fields, " {name}={value:?}");
            }
        }
    }
}

/// A subscriber layer that forwards each event to the installed C callback.
struct CallbackLayer;

impl<S: Subscriber> Layer<S> for CallbackLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // Fast path: nothing to do if no callback is installed.
        let guard = match SINK.read() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(sink) = guard.as_ref() else {
            return;
        };
        let Some(cb) = sink.cb else {
            return;
        };

        let meta = event.metadata();
        let level = syslog_level(meta.level());
        let mut v = LineVisitor::default();
        event.record(&mut v);
        let line = format!("{}: {}{}", meta.target(), v.message, v.fields);
        // Skip silently if the line has an interior NUL (can't cross as a C string).
        let Ok(c) = CString::new(line) else {
            return;
        };
        let ctx = sink.ctx;
        // Guard against a panicking callback so we never unwind into `tracing`
        // internals. `cb` is a plain `extern "C" fn` (safe to call); `c` outlives it.
        let _ = catch_unwind(AssertUnwindSafe(|| cb(level, c.as_ptr(), ctx)));
    }
}

/// Whether any developer diagnostic env var is set — gates the stderr layer.
fn dev_env_set() -> bool {
    [
        "VIRTIO_HDV_TRACE",
        "VIRTIO_HDV_APERTURE_STATS",
        "VIRTIO_HDV_REQ_STATS",
        "RUST_LOG",
    ]
    .iter()
    .any(|k| std::env::var_os(k).is_some())
}

/// Build the global level filter: `RUST_LOG` if set, else INFO. `VIRTIO_HDV_TRACE`
/// raises our transport crates to TRACE (the per-access data-path firehose);
/// `VIRTIO_HDV_APERTURE_STATS` or `VIRTIO_HDV_REQ_STATS` alone raises `virtio_hdv`
/// to DEBUG (the aperture-cache / request-path stats events).
fn build_filter() -> EnvFilter {
    let mut filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let extra: &[&str] = if std::env::var_os("VIRTIO_HDV_TRACE").is_some() {
        &["virtio_hdv=trace", "hyperv_virtiofs=trace"]
    } else if std::env::var_os("VIRTIO_HDV_APERTURE_STATS").is_some()
        || std::env::var_os("VIRTIO_HDV_REQ_STATS").is_some()
    {
        &["virtio_hdv=debug"]
    } else {
        &[]
    };
    for d in extra {
        if let Ok(dir) = d.parse() {
            filter = filter.add_directive(dir);
        }
    }
    filter
}

/// Install the process-global subscriber exactly once. Idempotent and cheap to
/// call at the top of every entry point. Best-effort: if a global subscriber is
/// already set (the host owns one), we leave it alone.
pub(crate) fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let stderr_layer = dev_env_set().then(|| {
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(std::io::stderr)
        });
        let _ = tracing_subscriber::registry()
            .with(build_filter())
            .with(CallbackLayer)
            .with(stderr_layer)
            .try_init();
    });
}
