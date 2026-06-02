//! **virtio-fs-over-HDV through the public C ABI (v2 host/share surface).**
//!
//! Where `attach_virtiofs.rs` proves the *transport* by calling
//! `VirtioHdvDevice::attach` directly (around the ABI), this proves the **shipped
//! front door**: it drives the exported C functions end-to-end —
//! `hvfs_host_open` → (start) → `hvfs_add_share` → `hvfs_share_instance_id` →
//! `hvfs_remove_share` → `hvfs_host_close` — and asserts the guest *hot-mounts* the
//! share on a running VM (the device-per-share model; `docs/share-abi.md`).
//!
//! The cdylib is linked in-process via its `rlib` output, so these are the real ABI
//! bodies (`crates/hyperv_virtiofs/src/lib.rs`), not a re-implementation.
//!
//! Lifecycle mirrors the stack's real ordering: the device host is registered
//! **before** start (`hvfs_host_open`), the guest comes up, then each share is
//! **hot-added at runtime** (`hvfs_add_share` → `HcsModifyComputeSystem` Add). The
//! guest's hotplug-watch loop (`E:\dev\spike\init`) mounts the tag and prints
//! `HOTPLUG_MOUNT_PASS tag=<tag>`.
//!
//! `hvfs_remove_share` is best-effort: live `FlexibleIov` Remove is unsupported on
//! current Windows, so the test accepts **either** `HVFS_OK` or `HVFS_ERR_UNSUPPORTED`.
//!
//! **Retry:** like `attach_virtiofs.rs`, attach+boot is retried a few times — HDV
//! guest-memory apertures are an evictable cache, so a fraction of boots stall. Each
//! attempt is a fresh VM.
//!
//! `#[ignore]` — needs Hyper-V + the Rocky artifacts. Run it:
//!   $env:HVFS_KERNEL="E:\dev\spike\out\vmlinuz"
//!   $env:HVFS_INITRD="E:\dev\spike\out\initramfs.cpio.gz"
//!   cargo test -p hcs-testvm --test attach_abi -- --ignored --nocapture
#![cfg(windows)]

use hcs_testvm::{RockyConfig, RockyVm};
use hyperv_virtiofs::{
    hvfs_add_share, hvfs_host, hvfs_host_close, hvfs_host_open, hvfs_last_error, hvfs_remove_share,
    hvfs_share, hvfs_share_instance_id, HVFS_ERR_UNSUPPORTED, HVFS_OK,
};
use std::ffi::{CStr, CString};
use std::ptr;
use std::time::Duration;

/// A fixed, caller-supplied instance id (the ABI requires one; the caller owns
/// uniqueness). Canonical lowercase so it round-trips through `hvfs_share_instance_id`.
const INSTANCE_ID: &str = "c1c1c1c1-3333-4333-8333-333333333333";
const TAG: &str = "hp1";

/// Build a throwaway host workspace dir with a sentinel file for the guest to read.
fn make_workspace() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("atelier-virtiofs-abi-ws");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workspace dir");
    std::fs::write(
        dir.join("SENTINEL.txt"),
        b"hello from the host via virtio-fs over HDV (C ABI v2)\n",
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
    // SAFETY: `p` is a valid NUL-terminated string owned by the DLL, valid until the
    // next ABI call on this thread.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

#[test]
#[ignore = "requires Hyper-V + Rocky artifacts; run with --ignored"]
fn guest_hot_mounts_share_via_c_abi() {
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
        if try_add_share_via_abi(&kernel, &initrd, &ws) {
            eprintln!("PASS on attempt {attempt}");
            return;
        }
    }
    panic!("guest did not hot-mount the share via the C ABI in {ATTEMPTS} attempts");
}

/// One attempt: create the VM (no cold FlexibleIov slot), `hvfs_host_open` before
/// start, boot, wait for GUEST_READY, `hvfs_add_share` at runtime, wait for the mount,
/// check the instance-id getter, then `hvfs_remove_share` (best-effort) +
/// `hvfs_host_close`. Returns whether the guest hot-mounted the share.
fn try_add_share_via_abi(kernel: &str, initrd: &str, ws: &std::path::Path) -> bool {
    // No cold slot: the share is hot-added at runtime. The guest watches the tag named
    // on the kernel cmdline.
    let mut cfg = RockyConfig::new(kernel, initrd);
    cfg.kernel_cmdline = format!("console=ttyS0 atelier.hptags={TAG}");

    let vm = RockyVm::create(&cfg).expect("create Rocky compute system");
    eprintln!("compute system id: {}", vm.id());

    let id_c = CString::new(vm.id()).expect("system id has no interior NUL");
    let host_json = serde_json::json!({ "memory_mb": cfg.memory_mb }).to_string();
    let host_json_c = CString::new(host_json).expect("host_json has no interior NUL");

    // (1) Register the device host BEFORE start, through the C ABI.
    let mut host: *mut hvfs_host = ptr::null_mut();
    // SAFETY: valid NUL-terminated C strings; `host` is a writable out slot.
    let rc = unsafe { hvfs_host_open(id_c.as_ptr(), host_json_c.as_ptr(), &mut host) };
    assert_eq!(
        rc,
        HVFS_OK,
        "hvfs_host_open failed: rc={rc}, {}",
        last_error()
    );
    assert!(
        !host.is_null(),
        "hvfs_host_open returned OK but a null handle"
    );
    eprintln!("hvfs_host_open OK; starting guest…");

    vm.start().expect("start Rocky compute system");

    let mounted = if !vm.wait_for_console("GUEST_READY", Duration::from_secs(90)) {
        eprintln!(
            "===== guest console (no GUEST_READY) =====\n{}",
            vm.console()
        );
        false
    } else {
        // (2) Hot-add the share at runtime, through the C ABI.
        let ws_path = ws.to_str().expect("utf-8 workspace path");
        let share_json = serde_json::json!({
            "tag": TAG,
            "path": ws_path,
            "instance_id": INSTANCE_ID,
            "ro": false,
        })
        .to_string();
        let share_json_c = CString::new(share_json).expect("share_json has no interior NUL");

        let mut share: *mut hvfs_share = ptr::null_mut();
        // SAFETY: live host handle; valid C string; writable out slot.
        let arc = unsafe { hvfs_add_share(host, share_json_c.as_ptr(), &mut share) };
        assert_eq!(
            arc,
            HVFS_OK,
            "hvfs_add_share failed: rc={arc}, {}",
            last_error()
        );
        assert!(
            !share.is_null(),
            "hvfs_add_share returned OK but a null handle"
        );

        // The on-wire identity getter must echo the canonical instance id.
        // SAFETY: `share` is a live handle from hvfs_add_share.
        let got = unsafe { CStr::from_ptr(hvfs_share_instance_id(share)) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(got, INSTANCE_ID, "hvfs_share_instance_id mismatch");

        let ok = vm.wait_for_console(
            &format!("HOTPLUG_MOUNT_PASS tag={TAG}"),
            Duration::from_secs(45),
        );
        if !ok {
            eprintln!(
                "===== guest console (share never mounted) =====\n{}",
                vm.console()
            );
        }

        // (3) Best-effort remove: OK or UNSUPPORTED are both acceptable (live Remove
        // is a platform gap). Frees the share handle in both cases.
        // SAFETY: `share` is a live handle whose host is still open.
        let rrc = unsafe { hvfs_remove_share(share) };
        assert!(
            rrc == HVFS_OK || rrc == HVFS_ERR_UNSUPPORTED,
            "hvfs_remove_share unexpected rc={rrc}, {}",
            last_error()
        );
        eprintln!("hvfs_remove_share rc={rrc} (OK or UNSUPPORTED expected)");
        ok
    };

    // (4) Close the host: tears down every remaining device + the host + the system.
    // SAFETY: `host` came from a successful hvfs_host_open and is closed once.
    let crc = unsafe { hvfs_host_close(host) };
    assert_eq!(crc, HVFS_OK, "hvfs_host_close failed: rc={crc}");

    mounted
}
