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
//! Status: SKELETON — milestone 2. The shape is now decided (design §7 unknown #2
//! is **resolved**, see `windows-virtiofs-hdv.md` §4 "Correction²" + Appendix B):
//! the generic HDV PCI device + callback vtable live in [`hdv::pci`]; this crate
//! implements [`hdv::pci::PciOps`] by driving OpenVMM's **public** `VirtioPciDevice`
//! — backing its `GuestMemory` with `hdv` apertures (via `GuestMemoryAccess`),
//! its `DoorbellRegistration` with `HdvRegisterDoorbell`, its `PciInterruptModel`
//! with `HdvDeliverGuestInterrupt`, and its `RegisterMmioIntercept` with the HDV
//! BAR callbacks. The attach spike (`hcs-testvm`) exercises `hdv::pci` directly
//! with a trivial device first; this crate fills in once that handshake is proven.

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
        // TODO(milestone-2): implement hdv::pci::PciOps over OpenVMM's
        // VirtioPciDevice (GuestMemory ← apertures, doorbells, MSI, BAR intercepts).
        Self { _host: host }
    }
}
