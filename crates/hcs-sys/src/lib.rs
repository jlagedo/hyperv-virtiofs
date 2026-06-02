//! Raw FFI to the Windows **Host Compute System (HCS)** control API exported by
//! `computecore.dll`, transcribed from the SDK header `computecore.h`.
//!
//! Just enough to drive a compute system's lifecycle: create an async operation,
//! create/open/start/terminate a system, and wait for results. The product uses
//! [`HcsOpenComputeSystem`] to turn an externally-owned system id into the
//! `HCS_SYSTEM` handle that HDV attaches to; the test harness additionally uses
//! create/start/terminate to stand up a throwaway Linux VM.
//!
//! Windows-only (a non-Windows stub keeps the crate graph checkable elsewhere).

#![allow(non_camel_case_types, non_snake_case)]

use core::ffi::c_void;

pub type HRESULT = i32;
pub type PCWSTR = *const u16;
pub type PWSTR = *mut u16;
pub type DWORD = u32;

/// Opaque handle to an async HCS operation (`HCS_OPERATION`).
pub type HCS_OPERATION = *mut c_void;
/// Opaque handle to a compute system (`HCS_SYSTEM`). The same type HDV's
/// `HdvInitializeDeviceHost` consumes.
pub type HCS_SYSTEM = *mut c_void;

/// Operation-completion callback (`HCS_OPERATION_COMPLETION`).
pub type HCS_OPERATION_COMPLETION =
    Option<unsafe extern "system" fn(operation: HCS_OPERATION, context: *mut c_void)>;

/// `GENERIC_ALL` — requested access for [`HcsOpenComputeSystem`].
pub const GENERIC_ALL: DWORD = 0x1000_0000;

#[cfg(windows)]
#[link(name = "computecore")]
unsafe extern "system" {
    pub fn HcsCreateOperation(
        context: *const c_void,
        callback: HCS_OPERATION_COMPLETION,
    ) -> HCS_OPERATION;

    pub fn HcsCloseOperation(operation: HCS_OPERATION);

    pub fn HcsWaitForOperationResult(
        operation: HCS_OPERATION,
        timeoutMs: DWORD,
        resultDocument: *mut PWSTR,
    ) -> HRESULT;

    pub fn HcsCreateComputeSystem(
        id: PCWSTR,
        configuration: PCWSTR,
        operation: HCS_OPERATION,
        securityDescriptor: *const c_void,
        computeSystem: *mut HCS_SYSTEM,
    ) -> HRESULT;

    pub fn HcsOpenComputeSystem(
        id: PCWSTR,
        requestedAccess: DWORD,
        computeSystem: *mut HCS_SYSTEM,
    ) -> HRESULT;

    pub fn HcsStartComputeSystem(
        computeSystem: HCS_SYSTEM,
        operation: HCS_OPERATION,
        options: PCWSTR,
    ) -> HRESULT;

    pub fn HcsTerminateComputeSystem(
        computeSystem: HCS_SYSTEM,
        operation: HCS_OPERATION,
        options: PCWSTR,
    ) -> HRESULT;

    pub fn HcsCloseComputeSystem(computeSystem: HCS_SYSTEM);
}

#[cfg(not(windows))]
mod not_windows {
    #![allow(clippy::missing_safety_doc)]
    use super::*;
    pub const E_NOTIMPL: HRESULT = -0x7fff_bfff;
    pub unsafe fn HcsCreateOperation(
        _: *const c_void,
        _: HCS_OPERATION_COMPLETION,
    ) -> HCS_OPERATION {
        core::ptr::null_mut()
    }
    pub unsafe fn HcsCloseOperation(_: HCS_OPERATION) {}
    pub unsafe fn HcsWaitForOperationResult(_: HCS_OPERATION, _: DWORD, _: *mut PWSTR) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HcsCreateComputeSystem(
        _: PCWSTR,
        _: PCWSTR,
        _: HCS_OPERATION,
        _: *const c_void,
        _: *mut HCS_SYSTEM,
    ) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HcsOpenComputeSystem(_: PCWSTR, _: DWORD, _: *mut HCS_SYSTEM) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HcsStartComputeSystem(_: HCS_SYSTEM, _: HCS_OPERATION, _: PCWSTR) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HcsTerminateComputeSystem(_: HCS_SYSTEM, _: HCS_OPERATION, _: PCWSTR) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HcsCloseComputeSystem(_: HCS_SYSTEM) {}
}
#[cfg(not(windows))]
pub use not_windows::*;
