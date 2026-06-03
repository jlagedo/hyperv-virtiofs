//! **C ABI edge cases against a live device host.**
//!
//! `attach_abi.rs` proves the happy path; this proves the ABI *rejects* bad input
//! correctly, with a real `hvfs_host` (so the validation runs in its true context,
//! not a mock). The negative `hvfs_add_share` paths all return **before** the device
//! is attached or the compute system is modified, so this needs only a *created*
//! (not started) VM — no guest boot, no aperture-staleness retries. Fast and
//! deterministic.
//!
//! Covered:
//!   - `ro: true`            -> `HVFS_ERR_NOT_IMPLEMENTED` (honestly refused, not faked)
//!   - non-GUID instance_id  -> `HVFS_ERR_INVALID_ARG`
//!   - malformed share_json  -> `HVFS_ERR_INVALID_ARG`
//!   - missing required field -> `HVFS_ERR_INVALID_ARG`
//!   - null share_json        -> `HVFS_ERR_INVALID_ARG`
//!
//! Each must also leave `*out` NULL and set a thread-local `hvfs_last_error`.
//!
//!   .\test\build-guest-artifacts.ps1   # one-time: build test\guest\out artifacts
//!   cargo test -p hcs-testvm --test edge_cases -- --ignored --nocapture
//! (Override artifact paths with $env:HVFS_KERNEL / $env:HVFS_INITRD; see docs/testing.md.)
#![cfg(windows)]

use hcs_testvm::{RockyConfig, RockyVm};
use hyperv_virtiofs::{
    hvfs_add_share, hvfs_host, hvfs_host_close, hvfs_host_open, hvfs_last_error, hvfs_share,
    HVFS_ERR_INVALID_ARG, HVFS_ERR_NOT_IMPLEMENTED, HVFS_OK,
};
use std::ffi::{CStr, CString};
use std::ptr;

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

/// Call `hvfs_add_share` with the given JSON and assert it fails with `want`, leaves
/// `*out` NULL, and records a non-empty last_error.
fn expect_add_share_err(host: *mut hvfs_host, json: &str, want: i32, case: &str) {
    let json_c = CString::new(json).expect("json has no interior NUL");
    let mut out: *mut hvfs_share = ptr::null_mut();
    // SAFETY: live host; valid C string; writable out slot.
    let rc = unsafe { hvfs_add_share(host, json_c.as_ptr(), &mut out) };
    assert_eq!(rc, want, "[{case}] rc; last_error={}", last_error());
    assert!(out.is_null(), "[{case}] *out must stay NULL on error");
    assert_ne!(
        last_error(),
        "(none)",
        "[{case}] a failing call must set last_error"
    );
    eprintln!("[{case}] rc={rc} OK; last_error={}", last_error());
}

#[test]
#[ignore = "requires Hyper-V + Rocky artifacts; run with --ignored"]
fn abi_rejects_bad_share_input() {
    let (kernel, initrd) = hcs_testvm::artifact_paths();
    assert!(
        std::path::Path::new(&kernel).exists(),
        "kernel not found: {kernel}"
    );
    assert!(
        std::path::Path::new(&initrd).exists(),
        "initrd not found: {initrd}"
    );

    // A created-but-not-started VM is enough: the device host registers pre-start
    // (the proven proxy path), and every negative add_share returns before the VM
    // would need to be running.
    let cfg = RockyConfig::new(&kernel, &initrd);
    let vm = RockyVm::create(&cfg).expect("create Rocky compute system");
    eprintln!("compute system id: {}", vm.id());

    let id_c = CString::new(vm.id()).expect("system id has no interior NUL");
    let host_json = serde_json::json!({ "memory_mb": cfg.memory_mb }).to_string();
    let host_json_c = CString::new(host_json).expect("host_json has no interior NUL");

    let mut host: *mut hvfs_host = ptr::null_mut();
    // SAFETY: valid NUL-terminated C strings; writable out slot.
    let rc = unsafe { hvfs_host_open(id_c.as_ptr(), host_json_c.as_ptr(), &mut host) };
    assert_eq!(rc, HVFS_OK, "hvfs_host_open failed: {}", last_error());
    assert!(!host.is_null());
    eprintln!("hvfs_host_open OK; exercising negative add_share paths…");

    let good_id = "c1c1c1c1-3333-4333-8333-333333333333";

    // ro:true is honestly refused (the FUSE backend doesn't enforce read-only yet).
    expect_add_share_err(
        host,
        &serde_json::json!({"tag":"ro","path":"C:\\","instance_id":good_id,"ro":true}).to_string(),
        HVFS_ERR_NOT_IMPLEMENTED,
        "ro:true",
    );

    // A non-GUID instance_id is rejected before any device is created.
    expect_add_share_err(
        host,
        &serde_json::json!({"tag":"t","path":"C:\\","instance_id":"not-a-guid"}).to_string(),
        HVFS_ERR_INVALID_ARG,
        "bad-instance-id",
    );

    // Malformed JSON.
    expect_add_share_err(
        host,
        "{ not valid json",
        HVFS_ERR_INVALID_ARG,
        "malformed-json",
    );

    // Missing the required `tag` field.
    expect_add_share_err(
        host,
        &serde_json::json!({"path":"C:\\","instance_id":good_id}).to_string(),
        HVFS_ERR_INVALID_ARG,
        "missing-tag",
    );

    // Null share_json pointer.
    {
        let mut out: *mut hvfs_share = ptr::null_mut();
        // SAFETY: live host; null share_json is the contract under test; writable out.
        let rc = unsafe { hvfs_add_share(host, ptr::null(), &mut out) };
        assert_eq!(rc, HVFS_ERR_INVALID_ARG, "null share_json");
        assert!(out.is_null());
    }

    // The host is still healthy after all the rejections: close cleanly.
    // SAFETY: `host` came from a successful hvfs_host_open and is closed once.
    let crc = unsafe { hvfs_host_close(host) };
    assert_eq!(crc, HVFS_OK, "hvfs_host_close after rejections");
    eprintln!("all negative cases rejected; host closed cleanly — PASS");
}
