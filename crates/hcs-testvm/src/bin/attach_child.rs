//! Out-of-process half of the HDV attach spike — the **task #16 A-vs-B experiment**.
//!
//! `tests/attach_oop.rs` creates the Rocky compute system with a `FlexibleIov`
//! slot but does **not** open the HDV device host itself. It spawns THIS binary,
//! passing the compute-system id. We open our *own* handle to that system
//! (`HcsOpenComputeSystem`) and stand up the HDV device host + the minimal
//! `SpikeDevice` here, in a separate process the parent spawns and owns, then
//! print a readiness line and block — holding the device alive until killed.
//!
//! This distinguishes the two horns of the `FlexibleIov` fork. After the parent
//! starts the VM:
//!   * start **succeeds** → Variant A: the emulator may live in any process the
//!     host spawns and owns, so atelierd can run a child helper and keep full
//!     lifecycle control.
//!   * start **still fails** in `FinishReservingResources` (0x8000FFFF) → Variant
//!     B: an `ExternalRestricted` emulator must be the one HCS itself launches;
//!     a process we spawn is not enough. We then adopt the WSL-style registered
//!     out-of-process emulator (revisits plan §6.2).

#[cfg(windows)]
mod imp {
    use hcs_sys as hcs;
    use hcs_testvm::SpikeDevice;
    use hdv::pci::PciDevice;
    use hdv::DeviceHost;
    use std::io::Write;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn main() {
        let id = match std::env::args().nth(1) {
            Some(id) => id,
            None => {
                eprintln!("attach_child: usage: attach_child <compute-system-id>");
                std::process::exit(2);
            }
        };
        eprintln!("attach_child: opening compute system {id}");

        // Our own HCS_SYSTEM handle to the system the parent created. Kept open
        // (as a raw handle) for the whole process lifetime so the device host
        // below outlives nothing it shouldn't; the OS reclaims it on exit.
        let mut system: hcs::HCS_SYSTEM = std::ptr::null_mut();
        // SAFETY: valid wide id; `system` is a valid out ptr.
        let hr =
            unsafe { hcs::HcsOpenComputeSystem(wide(&id).as_ptr(), hcs::GENERIC_ALL, &mut system) };
        if hr < 0 {
            eprintln!(
                "attach_child: HcsOpenComputeSystem failed: HRESULT {:#010x}",
                hr as u32
            );
            std::process::exit(3);
        }

        // Stand up the device host + device in THIS process.
        // SAFETY: `system` is a live HCS_SYSTEM we just opened and keep open.
        let host = match unsafe { DeviceHost::open(system) } {
            Ok(h) => h,
            Err(e) => {
                eprintln!("attach_child: HdvInitializeDeviceHost failed: {e}");
                std::process::exit(4);
            }
        };
        let _device = match PciDevice::create(host, Box::new(SpikeDevice)) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("attach_child: HdvCreateDeviceInstance failed: {e}");
                std::process::exit(5);
            }
        };
        eprintln!("attach_child: HDV device created (1af4:1100); signalling ready");

        // Tell the parent the emulator is registered, so it can start the VM
        // without racing FinishReservingResources.
        println!("HVFS_CHILD_READY");
        let _ = std::io::stdout().flush();

        // Hold `_device` (hence the device host) alive until the parent kills us.
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
}

fn main() {
    #[cfg(windows)]
    imp::main();
    #[cfg(not(windows))]
    {
        eprintln!("attach_child is Windows-only");
        std::process::exit(1);
    }
}
