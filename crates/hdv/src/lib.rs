//! Safe RAII over `hdv-sys`. Everything that touches an HDV handle goes through
//! here so the rest of the stack never holds a raw pointer: handles are owned by
//! `Drop` types, apertures unmap themselves, doorbells unregister themselves.
//!
//! This layer is **device-agnostic** — it knows nothing about virtio or virtio-fs.
//! Any HDV device (a future block device, a console, etc.) can sit on top of it.
//! The virtio specifics live one crate up, in `virtio-hdv`.
//!
//! Status: SKELETON — the types below fix the ownership model; method bodies are
//! stubbed until `hdv-sys` links the real API (`link-hdv`).

/// Errors from the HDV layer. Carries the raw HRESULT so callers can log it.
#[derive(Debug)]
pub enum Error {
    /// The HDV API returned a failing HRESULT.
    Hdv(i32),
    /// The feature `link-hdv` is off, so no real call was made.
    NotLinked,
}

pub type Result<T> = core::result::Result<T, Error>;

/// Owns an HDV device host bound to one externally-owned compute system.
/// Tearing this down detaches every device created from it.
pub struct DeviceHost {
    // raw: hdv_sys::HDV_HOST,  // populated once link-hdv is wired
    _priv: (),
}

impl DeviceHost {
    /// Initialize a device host against a compute system the caller owns,
    /// addressed by HCS system id. Maps to `HdvInitializeDeviceHost`.
    ///
    /// TODO(spike-1): the linchpin. Prove this succeeds against an HCS system we
    /// created (by id or inherited handle) — design §7 unknown #1.
    pub fn open(_hcs_system_id: &str) -> Result<Self> {
        Err(Error::NotLinked)
    }
}

impl Drop for DeviceHost {
    fn drop(&mut self) {
        // TODO: HdvTeardownDeviceHost(self.raw)
    }
}

// TODO: DeviceInstance (HdvCreateDeviceInstance), GuestMemoryAperture
// (HdvCreateGuestMemoryAperture/Destroy), Doorbell (HdvRegister/Unregister) —
// each a Drop-owning RAII type. These are the primitives `virtio-hdv` composes
// into a working transport.
