//! MSI delivery: route OpenVMM's `SignalMsi` to `HdvDeliverGuestInterrupt`.
//!
//! When the guest's virtio driver gets a used-ring completion, `VirtioPciDevice`
//! signals its MSI-X target with the `(address, data)` pair the guest programmed
//! into the MSI-X table. We forward that straight to HDV, which injects the
//! interrupt into the partition.

use crate::handle::DeviceHandle;
use pci_core::msi::SignalMsi;

/// A `SignalMsi` target that injects via the late-bound HDV device.
pub struct HdvSignalMsi {
    handle: DeviceHandle,
}

impl HdvSignalMsi {
    pub fn new(handle: DeviceHandle) -> Self {
        Self { handle }
    }
}

impl SignalMsi for HdvSignalMsi {
    fn signal_msi(&self, _devid: Option<u32>, address: u64, data: u32) {
        // `devid` (requester id) is HDV's concern, not ours — it owns the BDF.
        // Drop the interrupt if the device handle isn't bound yet (can't happen
        // once the guest is running, since binding precedes Start).
        if let Some(device) = self.handle.get() {
            // A failed injection is logged-and-dropped: there is no caller to
            // propagate to (this is a fire-and-forget notification path).
            let r = device.deliver_interrupt(address, data);
            crate::trace::trace!("deliver_interrupt addr={address:#x} data={data:#x} -> {r:?}");
        } else {
            crate::trace::trace!(
                "deliver_interrupt addr={address:#x} data={data:#x} DROPPED (no handle)"
            );
        }
    }
}
