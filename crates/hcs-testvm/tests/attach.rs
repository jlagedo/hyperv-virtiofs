//! The **HDV attach spike** (design §7 unknown #1, "the linchpin"): prove that an
//! in-process `HdvInitializeDeviceHost` against a compute system *we own* plus
//! `HdvCreateDeviceInstance` makes the EL10 guest **enumerate a PCI device** — with
//! no virtio and no FUSE. If this passes, the full `virtio-hdv` adapter (milestone
//! 2) is "just engineering".
//!
//! Strategy: declare a `FlexibleIov` slot (so the guest gets a VPCI bus), attach
//! the device *before* starting the VM, and assert our distinctive `1af4:1100` id
//! appears on the console once `pci-hyperv` enumerates it.
//!
//! STATUS (2026-06-02): the in-process HDV attach succeeds and HCS routes the
//! FlexibleIov slot to our emulator by GUID, but `HcsStartComputeSystem` fails in
//! the FlexibleIov VID's `FinishReservingResources` (0x8000FFFF) — evidence that
//! `FlexibleIov` + `HostingModel: ExternalRestricted` wants an *out-of-process*
//! HCS-registered emulator, not our in-process device host. See plan §7 #3 / task
//! #16. This test is the standing gate for resolving that.
//!
//! `#[ignore]` — needs a Hyper-V-capable host + the Rocky artifacts. Run it:
//!   $env:HVFS_KERNEL="E:\dev\spike\out\vmlinuz"
//!   $env:HVFS_INITRD="E:\dev\spike\out\initramfs.cpio.gz"
//!   cargo test -p hcs-testvm --test attach -- --ignored --nocapture
#![cfg(windows)]

use hcs_testvm::{FlexibleIovSlot, RockyConfig, RockyVm, SpikeDevice};
use hdv::pci::{guid_to_string, PciDevice, SPIKE_CLASS_ID, SPIKE_INSTANCE_ID};
use hdv::DeviceHost;
use std::time::Duration;

#[test]
#[ignore = "requires Hyper-V + Rocky artifacts; run with --ignored"]
fn attaches_hdv_pci_device() {
    let kernel =
        std::env::var("HVFS_KERNEL").unwrap_or_else(|_| r"E:\dev\spike\out\vmlinuz".into());
    let initrd = std::env::var("HVFS_INITRD")
        .unwrap_or_else(|_| r"E:\dev\spike\out\initramfs.cpio.gz".into());
    assert!(
        std::path::Path::new(&kernel).exists(),
        "kernel not found: {kernel}"
    );
    assert!(
        std::path::Path::new(&initrd).exists(),
        "initrd not found: {initrd}"
    );

    // Declare a FlexibleIov slot so the guest gets a VPCI bus for the device.
    // The slot's GUIDs must match the HDV device: map-key == DeviceInstanceId,
    // EmulatorId == DeviceClassId (both minted in `hdv::pci`).
    let cfg = RockyConfig::new(kernel, initrd).with_flexible_iov(FlexibleIovSlot::new(
        guid_to_string(&SPIKE_INSTANCE_ID),
        guid_to_string(&SPIKE_CLASS_ID),
    ));

    // Create (but do not start) the VM, then attach the HDV device while the guest
    // is not yet running.
    let vm = RockyVm::create(&cfg).expect("create Rocky compute system");
    eprintln!("compute system id: {}", vm.id());

    // NB: InitializeComSecurity (HdvInitializeDeviceHostEx) was tried and made no
    // difference — the FinishReservingResources failure is not a COM-security issue.
    let host = unsafe { DeviceHost::open(vm.system_handle()) }.unwrap_or_else(|e| {
        panic!(
            "HdvInitializeDeviceHost failed: {e}\n  \
             fallback #1: attach after start() and rely on PCI hotplug;\n  \
             fallback #2: the compute-system doc may need a VPCI/FlexibleIov slot."
        )
    });
    let _device = PciDevice::create(host, Box::new(SpikeDevice))
        .unwrap_or_else(|e| panic!("HdvCreateDeviceInstance failed: {e}"));
    eprintln!("HDV device created (1af4:1100); starting guest…");

    vm.start().expect("start Rocky compute system");

    // The kernel logs every enumerated PCI device at boot, e.g.
    //   pci 0000:00:01.0: [1af4:1100] type 00 class 0xff0000
    let needle = "1af4:1100";
    let seen = vm.wait_for_console(needle, Duration::from_secs(90));
    eprintln!(
        "===== guest console =====\n{}\n=========================",
        vm.console()
    );
    assert!(
        seen,
        "guest kernel never enumerated the HDV PCI device [{needle}] within timeout \
         — see fallbacks in the test header / plan §2d"
    );

    // `_device` stays alive until here, so the device isn't torn down before the
    // guest enumerates it.
}
