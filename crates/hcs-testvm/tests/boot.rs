//! Boots Rocky Linux under HCS and confirms the guest reaches userspace, proving
//! the validation rig works. `#[ignore]` because it needs a Hyper-V-capable host
//! and the Rocky artifacts — not available on CI runners.
//!
//! Run it explicitly:
//!   $env:HVFS_KERNEL="E:\dev\spike\out\vmlinuz"
//!   $env:HVFS_INITRD="E:\dev\spike\out\initramfs.cpio.gz"
//!   cargo test -p hcs-testvm --test boot -- --ignored --nocapture
#![cfg(windows)]

use hcs_testvm::{RockyConfig, RockyVm};
use std::time::Duration;

#[test]
#[ignore = "requires Hyper-V + Rocky artifacts; run with --ignored"]
fn boots_rocky_under_hcs() {
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

    let cfg = RockyConfig::new(kernel, initrd);
    let vm = RockyVm::boot(&cfg).expect("boot Rocky under HCS");
    eprintln!("compute system id: {}", vm.id());

    // The guest's self-test init prints this banner once userspace runs. (It then
    // tries to mount virtio-fs, which fails until the virtio-hdv device exists —
    // that PASS is the next milestone; here we only prove the guest boots.)
    let booted = vm.wait_for_console("OPENVMM-VIRTIOFS-SPIKE", Duration::from_secs(90));
    eprintln!(
        "===== guest console =====\n{}\n========================",
        vm.console()
    );
    assert!(booted, "guest did not reach userspace within timeout");
}
