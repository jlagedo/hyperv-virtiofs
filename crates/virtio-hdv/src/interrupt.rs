//! MSI delivery: route OpenVMM's `SignalMsi` to `HdvDeliverGuestInterrupt`.
//!
//! When the guest's virtio driver gets a used-ring completion, `VirtioPciDevice`
//! signals its MSI-X target with the `(address, data)` pair the guest programmed
//! into the MSI-X table. We forward that straight to HDV, which injects the
//! interrupt into the partition.
//!
//! We also record every distinct `(address, data)` we deliver into a shared set,
//! which the [`crate`]'s interrupt re-arm safety net periodically re-delivers —
//! see the note in `lib.rs` (it covers a missed-interrupt window caused by the
//! copy/snapshot semantics of HDV guest-memory apertures).

use crate::handle::DeviceHandle;
use pci_core::msi::SignalMsi;
use std::sync::{Arc, Mutex};

/// The set of MSI `(address, data)` pairs seen so far, shared with the re-arm net.
pub type SeenInterrupts = Arc<Mutex<Vec<(u64, u32)>>>;

/// A `SignalMsi` target that injects via the late-bound HDV device.
pub struct HdvSignalMsi {
    handle: DeviceHandle,
    seen: SeenInterrupts,
}

impl HdvSignalMsi {
    pub fn new(handle: DeviceHandle, seen: SeenInterrupts) -> Self {
        Self { handle, seen }
    }
}

impl SignalMsi for HdvSignalMsi {
    fn signal_msi(&self, _devid: Option<u32>, address: u64, data: u32) {
        // `devid` (requester id) is HDV's concern, not ours — it owns the BDF.
        // Drop the interrupt if the device handle isn't bound yet (can't happen
        // once the guest is running, since binding precedes Start).
        if let Some(device) = self.handle.get() {
            {
                let mut seen = self.seen.lock().unwrap();
                if !seen.contains(&(address, data)) {
                    seen.push((address, data));
                }
            }
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
