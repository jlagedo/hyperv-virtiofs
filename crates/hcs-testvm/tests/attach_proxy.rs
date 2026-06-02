//! **HDV-proxy attach spike** — the real `ExternalRestricted` FlexibleIov path
//! (task #16), reproducing WSL's host-side protocol in a single process.
//!
//! Unlike `attach.rs` / `attach_oop.rs` (which declared a slot but never
//! registered a device host, so the VID saw `'Unknown'`), this drives the full
//! handshake reverse-engineered in `docs/hdv-proxy-abi.md`:
//!   1. create + start the VM with **no** FlexibleIov slot;
//!   2. `HdvInitializeDeviceHostForProxy(ctx, ourIVmDeviceHostSupport, &host)` —
//!      HDV calls back our `RegisterDeviceHost`, which runs `HdvProxyDeviceHost`,
//!      binding the device host to the partition;
//!   3. `HdvCreateDeviceInstance` (the same `hdv::pci` device, classId == slot
//!      EmulatorId);
//!   4. `HcsModifyComputeSystem` **Add** the FlexibleIov slot
//!      (instanceId == DeviceInstanceId);
//!   5. assert the guest enumerates `1af4:1100`.
//!
//! PASS → the proxy path works in-process; the linchpin is finally retired.
//! FAIL → the logged `register_hr` + console + HCS error pinpoint which step.
//!
//! RESULT (2026-06-02): **PASS.** `register_hr=S_OK`; the FlexibleIov VID drives our
//! device `Initialize → GetDetails → Start → ReadConfigSpace`; `HcsStartComputeSystem`
//! succeeds (no more `0x8000FFFF`); and the guest enumerates it over VMBus VPCI:
//! `hv_pci a7e11e40-…-002: PCI host bridge to bus 0001:00` /
//! `pci 0001:00:00.0: [1af4:1100] type 00 class 0xff0000`. The guest needs the
//! Hyper-V VPCI stack loaded (`hv_vmbus` + `pci-hyperv`); the spike initramfs init
//! `modprobe`s them. The §7 #1 linchpin is retired.
//!
//! `#[ignore]` — needs Hyper-V + Rocky artifacts. Run it:
//!   $env:HVFS_KERNEL="E:\dev\spike\out\vmlinuz"
//!   $env:HVFS_INITRD="E:\dev\spike\out\initramfs.cpio.gz"
//!   cargo test -p hcs-testvm --test attach_proxy -- --ignored --nocapture
#![cfg(windows)]

use hcs_testvm::{FlexibleIovSlot, RockyConfig, RockyVm, SpikeDevice};
use hdv::pci::{
    guid_to_string, PciDevice, HVFS_DEVICE_CLASS_ID, HVFS_DEVICE_HOST_ID, HVFS_DEVICE_INSTANCE_ID,
};
use hdv::proxy::DeviceHostSupport;
use hdv::DeviceHost;
use std::io::Write;
use std::time::Duration;

/// Flushed progress marker — so the last line before a hard AV is on screen.
fn mark(msg: &str) {
    eprintln!("--- {msg}");
    let _ = std::io::stderr().flush();
}

#[test]
#[ignore = "requires Hyper-V + Rocky artifacts; run with --ignored"]
fn attaches_via_hdv_proxy() {
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

    // Cold path: declare the FlexibleIov slot in the create document so the guest
    // has a VPCI bus at its *first* enumeration (the hot path raced the fast-exiting
    // initramfs init), and proxy-register the device host BEFORE start so the start
    // reservation resolves the slot to our device.
    let cfg = RockyConfig::new(kernel, initrd).with_flexible_iov(FlexibleIovSlot::new(
        guid_to_string(&HVFS_DEVICE_INSTANCE_ID),
        guid_to_string(&HVFS_DEVICE_CLASS_ID),
    ));
    let vm = RockyVm::create(&cfg).expect("create Rocky compute system");
    eprintln!("compute system id: {}", vm.id());

    // Build our IVmDeviceHostSupport and create the proxied device host (registers
    // it with the partition via HdvProxyDeviceHost) — before start.
    // SAFETY: the VM (hence its system handle) outlives `support` and `device`.
    let support = unsafe { DeviceHostSupport::new(vm.system_handle()) };
    mark("calling HdvInitializeDeviceHostForProxy (pre-start)");
    let host = unsafe { DeviceHost::from_proxy(&HVFS_DEVICE_HOST_ID, support.as_iunknown()) }
        .unwrap_or_else(|e| panic!("HdvInitializeDeviceHostForProxy failed: {e}"));
    eprintln!(
        "proxy host created; registered={} register_hr={:#010x} pid={} ipc={:#x}",
        support.was_registered(),
        support.register_hr() as u32,
        support.device_host_pid(),
        support.ipc_section(),
    );

    // Create the device on the proxied host (classId == slot EmulatorId).
    let device = PciDevice::create(host, Box::new(SpikeDevice))
        .expect("HdvCreateDeviceInstance on proxy host");
    eprintln!("HDV device created (1af4:1100); starting guest…");

    mark("starting VM");
    vm.start().expect("start Rocky compute system");
    mark("VM started");

    // The guest should enumerate the device at boot.
    let needle = "1af4:1100";
    let seen = vm.wait_for_console(needle, Duration::from_secs(90));
    eprintln!(
        "===== guest console =====\n{}\n=========================",
        vm.console()
    );

    // Keep both alive until enumeration is checked, then tear down device → support.
    let _ = &device;
    let _ = &support;

    assert!(
        seen,
        "guest never enumerated the HDV PCI device [{needle}] — register_hr={:#010x}",
        support.register_hr() as u32
    );
}
