//! Raw FFI to the Windows **Host Device Virtualization (HDV)** API exported by
//! `vmdevicehost.dll`. This is the unsafe, 1:1 transcription layer — no RAII, no
//! safety. The safe wrapper lives in the `hdv` crate.
//!
//! HDV is the documented escape hatch for attaching a *custom* virtual device to
//! an HCS/Hyper-V guest from a host-side user-mode process: the host registers a
//! device host against a compute system, maps guest memory apertures, and arms
//! doorbells (the guest's kick path). On top of it we run an OpenVMM virtio
//! transport (`virtio-hdv`) and a virtio-fs device (`hyperv_virtiofs`).
//!
//! Reference call graph (design doc §5/§7 in the Atelier repo):
//!   HdvInitializeDeviceHost(compute_system) -> device host
//!   HdvCreateDeviceInstance(host, ...)       -> PCI device the guest enumerates
//!   HdvCreateGuestMemoryAperture(...)        -> map guest RAM for virtqueue access
//!   HdvRegisterDoorbell(...)                 -> guest kick notification
//!
//! Status: SKELETON. The signatures below describe the intended surface; the real
//! `extern "system"` link block is gated behind the `link-hdv` feature so the
//! crate compiles without the Windows SDK import library present.

#![allow(non_camel_case_types)]

use core::ffi::c_void;

/// HDV status codes are `HRESULT` (S_OK == 0).
pub type HRESULT = i32;
pub const S_OK: HRESULT = 0;

/// Opaque HDV handles. The real API hands back pointer-sized handles; we model
/// them as distinct newtypes so the safe layer can't transpose them.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct HDV_HOST(pub *mut c_void);

#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct HDV_DEVICE(pub *mut c_void);

// SAFETY note for the future link block: these are `extern "system"` (stdcall on
// x86, the platform default elsewhere). vmdevicehost.dll is the host-side HDV
// surface; the import library ships with the Windows SDK / HCS headers.
//
// #[cfg(all(windows, feature = "link-hdv"))]
// #[link(name = "vmdevicehost")]
// unsafe extern "system" {
//     pub fn HdvInitializeDeviceHost(compute_system: *mut c_void, host: *mut HDV_HOST) -> HRESULT;
//     pub fn HdvTeardownDeviceHost(host: HDV_HOST) -> HRESULT;
//     pub fn HdvCreateDeviceInstance(/* host, type, ids, callbacks, ctx */) -> HRESULT;
//     pub fn HdvCreateGuestMemoryAperture(/* device, gpa, len, write, out */) -> HRESULT;
//     pub fn HdvDestroyGuestMemoryAperture(/* device, mapping */) -> HRESULT;
//     pub fn HdvRegisterDoorbell(/* device, bar, offset, value, len, event */) -> HRESULT;
//     pub fn HdvUnregisterDoorbell(/* device, bar, offset, value, len */) -> HRESULT;
// }

// TODO(spike-1): resolve the exact signatures + struct layouts (HDV_PCI_DEVICE_*,
// callback table) against the SDK headers, then enable the block above behind
// `link-hdv` and delete this note. See design §7 unknown #1 (the attach linchpin).
