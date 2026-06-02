//! **virtio-fs-over-HDV through the public C ABI.**
//!
//! Where `attach_virtiofs.rs` proves the *transport* by calling
//! `VirtioHdvDevice::attach` directly (around the ABI), this proves the **shipped
//! front door**: it drives the exported C functions — `hvfs_attach` with a
//! compute-system id string + a `device_json` blob, then `hvfs_detach` — and
//! asserts the guest mounts the share and reads the sentinel.
//!
//! The cdylib is linked in-process via its `rlib` output, so these are the real
//! ABI bodies (`crates/hyperv_virtiofs/src/lib.rs`), not a re-implementation.
//!
//! Note: `hvfs_attach` re-opens the system **by id** (`HcsOpenComputeSystem`), so
//! it holds a *second* handle to the same system the rig created — expected and
//! fine; HCS allows multiple handles to one compute system.
//!
//! **Retry:** like `attach_virtiofs.rs`, the attach+boot is retried a few times.
//! HDV guest-memory apertures are an evictable cache (see `virtio-hdv::mem`); a
//! fraction of boots hit a stale descriptor read and stall. Each attempt is a
//! fresh ~5 s VM.
//!
//! `#[ignore]` — needs Hyper-V + the Rocky artifacts. Run it:
//!   $env:HVFS_KERNEL="E:\dev\spike\out\vmlinuz"
//!   $env:HVFS_INITRD="E:\dev\spike\out\initramfs.cpio.gz"
//!   cargo test -p hcs-testvm --test attach_abi -- --ignored --nocapture
#![cfg(windows)]

use hcs_testvm::{FlexibleIovSlot, RockyConfig, RockyVm};
use hdv::pci::{guid_to_string, HVFS_DEVICE_CLASS_ID, HVFS_DEVICE_INSTANCE_ID};
use hyperv_virtiofs::{hvfs_attach, hvfs_detach, hvfs_device, hvfs_last_error, HVFS_OK};
use std::ffi::{CStr, CString};
use std::ptr;
use std::time::Duration;

/// Build a throwaway host workspace dir with a sentinel file for the guest to read.
fn make_workspace() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("atelier-virtiofs-abi-ws");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workspace dir");
    std::fs::write(
        dir.join("SENTINEL.txt"),
        b"hello from the host via virtio-fs over HDV (C ABI)\n",
    )
    .expect("write sentinel");
    dir
}

/// The DLL's thread-local last-error message (for assert diagnostics).
fn last_error() -> String {
    let p = hvfs_last_error();
    if p.is_null() {
        return "(none)".into();
    }
    // SAFETY: `p` is a valid NUL-terminated string owned by the DLL, valid until
    // the next ABI call on this thread.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

#[test]
#[ignore = "requires Hyper-V + Rocky artifacts; run with --ignored"]
fn guest_mounts_virtiofs_via_c_abi() {
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

    let ws = make_workspace();
    eprintln!("host workspace: {}", ws.display());

    const ATTEMPTS: usize = 4;
    for attempt in 1..=ATTEMPTS {
        eprintln!("--- attempt {attempt}/{ATTEMPTS} ---");
        if try_attach_via_abi(&kernel, &initrd, &ws) {
            eprintln!("PASS on attempt {attempt}");
            return;
        }
    }
    panic!("guest did not mount virtio-fs via the C ABI (no PROOF_COMPLETE_PASS) in {ATTEMPTS} attempts");
}

/// One attempt: create the VM (cold, with the FlexibleIov slot), drive the C ABI
/// `hvfs_attach`, boot, wait for the guest proof, then `hvfs_detach`. Returns
/// whether the proof sentinel was seen.
fn try_attach_via_abi(kernel: &str, initrd: &str, ws: &std::path::Path) -> bool {
    // The slot's GUIDs are the product's well-known device identity (Decision A):
    // map-key == DeviceInstanceId, EmulatorId == DeviceClassId.
    let cfg = RockyConfig::new(kernel, initrd).with_flexible_iov(FlexibleIovSlot::new(
        guid_to_string(&HVFS_DEVICE_INSTANCE_ID),
        guid_to_string(&HVFS_DEVICE_CLASS_ID),
    ));
    let vm = RockyVm::create(&cfg).expect("create Rocky compute system");
    eprintln!("compute system id: {}", vm.id());

    // device_json: the initial share + this VM's RAM (the GPA ceiling attach needs).
    let ws_path = ws.to_str().expect("utf-8 workspace path");
    let device_json = serde_json::json!({
        "tag": "ws",
        "path": ws_path,
        "ro": false,
        "memory_mb": cfg.memory_mb,
    })
    .to_string();

    let id_c = CString::new(vm.id()).expect("system id has no interior NUL");
    let json_c = CString::new(device_json).expect("device_json has no interior NUL");

    // Drive the real exported C ABI.
    let mut out: *mut hvfs_device = ptr::null_mut();
    // SAFETY: both pointers are valid NUL-terminated C strings for the call; `out`
    // is a valid writable slot.
    let rc = unsafe { hvfs_attach(id_c.as_ptr(), json_c.as_ptr(), &mut out) };
    assert_eq!(rc, HVFS_OK, "hvfs_attach failed: rc={rc}, {}", last_error());
    assert!(!out.is_null(), "hvfs_attach returned OK but a null handle");
    eprintln!("hvfs_attach OK; starting guest…");

    vm.start().expect("start Rocky compute system");

    let pass = vm.wait_for_console("PROOF_COMPLETE_PASS", Duration::from_secs(30));
    if !pass {
        eprintln!(
            "===== guest console (attempt failed) =====\n{}\n=========================",
            vm.console()
        );
    }

    // Detach through the ABI: frees the handle and tears the device down.
    // SAFETY: `out` came from a successful `hvfs_attach` and is detached once.
    let drc = unsafe { hvfs_detach(out) };
    assert_eq!(drc, HVFS_OK, "hvfs_detach failed: rc={drc}");

    pass
}
