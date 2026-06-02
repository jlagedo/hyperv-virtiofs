//! The interesting crate: OpenVMM's virtio transport, carried over HDV instead of
//! over OpenVMM's own PCI/VPCI stack. This is the open counterpart to WSL's
//! closed `wsldevicehost.dll` — the piece that lets *any* OpenVMM virtio device
//! run as a host-side HDV device against a stock HCS guest.
//!
//! Responsibilities:
//!   - present a virtio PCI device's config space to the guest (via the `hdv`
//!     device instance),
//!   - back the device's `GuestMemory` with `hdv` guest-memory apertures,
//!   - turn `hdv` doorbells into virtqueue kicks.
//!
//! It is **device-neutral**: virtio-fs, virtio-blk, virtio-console all ride it.
//! `hyperv_virtiofs` is the first consumer.
//!
//! Status: SKELETON. The adapter shape depends on spike-2 (does OpenVMM's `virtio`
//! transport seam accept an external memory + notify source, or does it assume its
//! own PCI front end?). Design §7 unknown #2.

use hdv::DeviceHost;

/// A virtio device presented to the guest over HDV. Generic over the OpenVMM
/// device implementation it fronts (virtio-fs, etc.) once the seam is wired.
pub struct VirtioHdvDevice {
    _host: DeviceHost,
}

impl VirtioHdvDevice {
    /// Bind a virtio device of the given `device_id` onto an HDV device host.
    /// `device_id` is the virtio PCI device id (0x1a == virtio-fs).
    pub fn new(host: DeviceHost, _device_id: u16) -> Self {
        // TODO(spike-2): create the hdv DeviceInstance with virtio-fs PCI ids,
        // wire config space, apertures, and doorbells to OpenVMM's transport.
        Self { _host: host }
    }
}
