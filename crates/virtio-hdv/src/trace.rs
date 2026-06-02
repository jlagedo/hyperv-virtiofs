//! Spike-only stderr tracing of the HDV data path, gated on `VIRTIO_HDV_TRACE=1`
//! (cached once). For diagnosing the guest-visible MMIO/config/DMA/interrupt
//! path on the rig; compiled out of the hot path by the cached bool check.

/// Whether tracing is enabled (reads the env var once).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VIRTIO_HDV_TRACE").is_some())
}

macro_rules! trace {
    ($($arg:tt)*) => {
        if $crate::trace::on() {
            eprintln!("[virtio-hdv] {}", format!($($arg)*));
        }
    };
}
pub(crate) use trace;
