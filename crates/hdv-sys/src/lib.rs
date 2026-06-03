//! Raw FFI to the Windows **Host Device Virtualization (HDV)** API, transcribed
//! 1:1 from the Windows SDK header `hypervdevicevirtualization.h` (ApiSet
//! `ext-ms-win-hyperv-devicevirtualization-l1`, import library `vmdevicehost.lib`).
//!
//! HDV lets a host-side user-mode process attach a *custom* PCI device to an
//! HCS/Hyper-V guest: register a device host against a compute system, expose a
//! PCI device via a callback vtable ([`HDV_PCI_DEVICE_INTERFACE`]), map guest
//! memory ([`HdvCreateGuestMemoryAperture`]), arm doorbells for the guest's kick
//! path ([`HdvRegisterDoorbell`]), and inject MSIs ([`HdvDeliverGuestInterrupt`]).
//!
//! This crate is the unsafe transcription only — no RAII, no safety. The safe
//! wrapper lives in the `hdv` crate; the virtio transport on top lives in
//! `virtio-hdv`.

#![allow(non_camel_case_types, non_snake_case)]

use core::ffi::c_void;

// --- Windows base types (kept local so this crate has no windows-sys feature
// surface; layouts match the Win32 ABI). ---
pub type HRESULT = i32;
pub type BOOL = i32;
pub type HANDLE = *mut c_void;
pub type PVOID = *mut c_void;
pub type LPCWSTR = *const u16;

/// `HCS_SYSTEM` from `ComputeDefs.h` — an opaque handle to a compute system.
pub type HCS_SYSTEM = *mut c_void;
/// Opaque HDV device-host handle (`typedef void* HDV_HOST`).
pub type HDV_HOST = *mut c_void;
/// Opaque HDV device handle (`typedef void* HDV_DEVICE`).
pub type HDV_DEVICE = *mut c_void;

pub const S_OK: HRESULT = 0;
/// Standard COM error codes, returned from callbacks whose Rust body panicked or
/// was handed bad arguments — so a failure crosses the FFI boundary as an HRESULT
/// rather than unwinding into HDV.
pub const E_POINTER: HRESULT = 0x8000_4003u32 as HRESULT;
pub const E_FAIL: HRESULT = 0x8000_4005u32 as HRESULT;
pub const E_NOINTERFACE: HRESULT = 0x8000_4002u32 as HRESULT;

/// Win32 `GUID` (a.k.a. `IID`/`CLSID`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GUID {
    pub Data1: u32,
    pub Data2: u16,
    pub Data3: u16,
    pub Data4: [u8; 8],
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HDV_DEVICE_TYPE {
    Undefined = 0,
    Pci = 1,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HDV_DEVICE_HOST_FLAGS {
    None = 0,
    InitializeComSecurity = 1,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HDV_PCI_BAR_SELECTOR {
    Bar0 = 0,
    Bar1 = 1,
    Bar2 = 2,
    Bar3 = 3,
    Bar4 = 4,
    Bar5 = 5,
}

pub const HDV_PCI_BAR_COUNT: u32 = 6;

// HDV_DOORBELL_FLAGS (passed to HdvRegisterDoorbell as a UINT64 `Flags`).
pub const HDV_DOORBELL_FLAG_TRIGGER_SIZE_ANY: u64 = 0;
pub const HDV_DOORBELL_FLAG_TRIGGER_SIZE_BYTE: u64 = 1;
pub const HDV_DOORBELL_FLAG_TRIGGER_SIZE_WORD: u64 = 2;
pub const HDV_DOORBELL_FLAG_TRIGGER_SIZE_DWORD: u64 = 3;
pub const HDV_DOORBELL_FLAG_TRIGGER_SIZE_QWORD: u64 = 4;
pub const HDV_DOORBELL_FLAG_TRIGGER_ANY_VALUE: u64 = 0x8000_0000;

/// PCI PnP identity returned to HDV from the `GetDetails` callback.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HDV_PCI_PNP_ID {
    pub VendorID: u16,
    pub DeviceID: u16,
    pub RevisionID: u8,
    pub ProgIf: u8,
    pub SubClass: u8,
    pub BaseClass: u8,
    pub SubVendorID: u16,
    pub SubSystemID: u16,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HDV_PCI_INTERFACE_VERSION {
    Invalid = 0,
    Version1 = 1,
}

// --- PCI device callback vtable. HDV invokes these on its own threads. CALLBACK
// is `__stdcall` on x86 and the single x64 convention; `extern "system"` selects
// the right one on each target. ---
pub type HDV_PCI_DEVICE_INITIALIZE = Option<unsafe extern "system" fn(ctx: PVOID) -> HRESULT>;
pub type HDV_PCI_DEVICE_TEARDOWN = Option<unsafe extern "system" fn(ctx: PVOID)>;
pub type HDV_PCI_DEVICE_SET_CONFIGURATION =
    Option<unsafe extern "system" fn(ctx: PVOID, count: u32, values: *const LPCWSTR) -> HRESULT>;
pub type HDV_PCI_DEVICE_GET_DETAILS = Option<
    unsafe extern "system" fn(
        ctx: PVOID,
        pnpId: *mut HDV_PCI_PNP_ID,
        probedBarsCount: u32,
        probedBars: *mut u32,
    ) -> HRESULT,
>;
pub type HDV_PCI_DEVICE_START = Option<unsafe extern "system" fn(ctx: PVOID) -> HRESULT>;
pub type HDV_PCI_DEVICE_STOP = Option<unsafe extern "system" fn(ctx: PVOID)>;
pub type HDV_PCI_READ_CONFIG_SPACE =
    Option<unsafe extern "system" fn(ctx: PVOID, offset: u32, value: *mut u32) -> HRESULT>;
pub type HDV_PCI_WRITE_CONFIG_SPACE =
    Option<unsafe extern "system" fn(ctx: PVOID, offset: u32, value: u32) -> HRESULT>;
pub type HDV_PCI_READ_INTERCEPTED_MEMORY = Option<
    unsafe extern "system" fn(
        ctx: PVOID,
        bar: HDV_PCI_BAR_SELECTOR,
        offset: u64,
        length: u64,
        value: *mut u8,
    ) -> HRESULT,
>;
pub type HDV_PCI_WRITE_INTERCEPTED_MEMORY = Option<
    unsafe extern "system" fn(
        ctx: PVOID,
        bar: HDV_PCI_BAR_SELECTOR,
        offset: u64,
        length: u64,
        value: *const u8,
    ) -> HRESULT,
>;

/// The device interface vtable passed to [`HdvCreateDeviceInstance`].
#[repr(C)]
pub struct HDV_PCI_DEVICE_INTERFACE {
    pub Version: HDV_PCI_INTERFACE_VERSION,
    pub Initialize: HDV_PCI_DEVICE_INITIALIZE,
    pub Teardown: HDV_PCI_DEVICE_TEARDOWN,
    pub SetConfiguration: HDV_PCI_DEVICE_SET_CONFIGURATION,
    pub GetDetails: HDV_PCI_DEVICE_GET_DETAILS,
    pub Start: HDV_PCI_DEVICE_START,
    pub Stop: HDV_PCI_DEVICE_STOP,
    pub ReadConfigSpace: HDV_PCI_READ_CONFIG_SPACE,
    pub WriteConfigSpace: HDV_PCI_WRITE_CONFIG_SPACE,
    pub ReadInterceptedMemory: HDV_PCI_READ_INTERCEPTED_MEMORY,
    pub WriteInterceptedMemory: HDV_PCI_WRITE_INTERCEPTED_MEMORY,
}

// --- Imported functions (vmdevicehost.dll). Windows-only; a non-Windows stub
// below keeps the crate compiling on other hosts for editor/tooling use. ---
#[cfg(windows)]
#[link(name = "vmdevicehost")]
unsafe extern "system" {
    pub fn HdvInitializeDeviceHost(
        computeSystem: HCS_SYSTEM,
        deviceHostHandle: *mut HDV_HOST,
    ) -> HRESULT;

    pub fn HdvInitializeDeviceHostEx(
        computeSystem: HCS_SYSTEM,
        flags: HDV_DEVICE_HOST_FLAGS,
        deviceHostHandle: *mut HDV_HOST,
    ) -> HRESULT;

    /// Register an **out-of-process** device host (a COM `IVmDeviceHost`) with a
    /// compute system the caller owns, so the partition's FlexibleIov VID can
    /// resolve a slot to it. This is the *host/broker-side* half of the
    /// `ExternalRestricted` model (the device-host side calls
    /// `HdvInitializeDeviceHostForProxy`). Signature transcribed from
    /// `microsoft/WSL` `src/windows/inc/wdk.h` (the symbol is **not** in the public
    /// `hypervdevicevirtualization.h`). `DeviceHost_IUnknown` is the device host's
    /// `IVmDeviceHost` queried as `IUnknown*`; `TargetProcessId` is the process that
    /// hosts it (may be the caller's own PID for an in-process device host);
    /// `IpcSectionHandle` receives a shared-memory section handle for the data path.
    pub fn HdvProxyDeviceHost(
        computeSystem: HCS_SYSTEM,
        deviceHostIUnknown: PVOID,
        targetProcessId: u32,
        ipcSectionHandle: *mut u64,
    ) -> HRESULT;

    /// Device-host-side counterpart to [`HdvProxyDeviceHost`]: create an HDV device
    /// host that is **proxied** to a partition through the host's
    /// `IVmDeviceHostSupport` callback — instead of bound to a compute system the
    /// caller owns (the in-process [`HdvInitializeDeviceHost`] path). Internally it
    /// builds the host, wraps it as an `IVmDeviceHost`, and invokes
    /// `IVmDeviceHostSupport::RegisterDeviceHost` (which on the host side calls
    /// [`HdvProxyDeviceHost`]).
    ///
    /// **Not** in the public `hypervdevicevirtualization.h`. Signature
    /// reverse-engineered from `vmdevicehost.dll` (export RVA `0xC960`); see
    /// `docs/hdv-proxy-abi.md`. `deviceHostSupport` is the host's
    /// `IVmDeviceHostSupport` as `IUnknown*` (it is `QueryInterface`d for IID
    /// `e31aa49b-0914-465e-b145-1b9ba13efb10`). `context` is a 64-bit value passed
    /// to the device-host object's initializer — **unverified** type (pass null for
    /// the first spike). `deviceHostHandle` receives the new host.
    pub fn HdvInitializeDeviceHostForProxy(
        context: PVOID,
        deviceHostSupport: PVOID,
        deviceHostHandle: *mut HDV_HOST,
    ) -> HRESULT;

    /// As [`HdvInitializeDeviceHostForProxy`] with an extra `flags` DWORD (export
    /// RVA `0xCAA0`; the flags flow into the same device-host initializer). Same
    /// reverse-engineering caveat.
    pub fn HdvInitializeDeviceHostForProxyEx(
        context: PVOID,
        deviceHostSupport: PVOID,
        flags: u32,
        deviceHostHandle: *mut HDV_HOST,
    ) -> HRESULT;

    pub fn HdvTeardownDeviceHost(deviceHostHandle: HDV_HOST) -> HRESULT;

    pub fn HdvCreateDeviceInstance(
        deviceHostHandle: HDV_HOST,
        deviceType: HDV_DEVICE_TYPE,
        deviceClassId: *const GUID,
        deviceInstanceId: *const GUID,
        deviceInterface: *const c_void,
        deviceContext: PVOID,
        deviceHandle: *mut HDV_DEVICE,
    ) -> HRESULT;

    pub fn HdvReadGuestMemory(
        requestor: HDV_DEVICE,
        guestPhysicalAddress: u64,
        byteCount: u32,
        buffer: *mut u8,
    ) -> HRESULT;

    pub fn HdvWriteGuestMemory(
        requestor: HDV_DEVICE,
        guestPhysicalAddress: u64,
        byteCount: u32,
        buffer: *const u8,
    ) -> HRESULT;

    pub fn HdvCreateGuestMemoryAperture(
        requestor: HDV_DEVICE,
        guestPhysicalAddress: u64,
        byteCount: u32,
        writeProtected: BOOL,
        mappedAddress: *mut PVOID,
    ) -> HRESULT;

    pub fn HdvDestroyGuestMemoryAperture(requestor: HDV_DEVICE, mappedAddress: PVOID) -> HRESULT;

    pub fn HdvDeliverGuestInterrupt(
        requestor: HDV_DEVICE,
        msiAddress: u64,
        msiData: u32,
    ) -> HRESULT;

    pub fn HdvRegisterDoorbell(
        requestor: HDV_DEVICE,
        barIndex: HDV_PCI_BAR_SELECTOR,
        barOffset: u64,
        triggerValue: u64,
        flags: u64,
        doorbellEvent: HANDLE,
    ) -> HRESULT;

    pub fn HdvUnregisterDoorbell(
        requestor: HDV_DEVICE,
        barIndex: HDV_PCI_BAR_SELECTOR,
        barOffset: u64,
        triggerValue: u64,
        flags: u64,
    ) -> HRESULT;
}

// Off-Windows stubs: let the whole crate graph type-check on non-Windows dev
// machines (e.g. rust-analyzer on macOS). Never linked into the shipped DLL,
// which is Windows-only. Every entry point returns E_NOTIMPL.
#[cfg(not(windows))]
mod not_windows {
    #![allow(clippy::missing_safety_doc)]
    use super::*;
    pub const E_NOTIMPL: HRESULT = -0x7fff_bfff; // 0x80004001

    pub unsafe fn HdvInitializeDeviceHost(_: HCS_SYSTEM, _: *mut HDV_HOST) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HdvInitializeDeviceHostEx(
        _: HCS_SYSTEM,
        _: HDV_DEVICE_HOST_FLAGS,
        _: *mut HDV_HOST,
    ) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HdvProxyDeviceHost(_: HCS_SYSTEM, _: PVOID, _: u32, _: *mut u64) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HdvInitializeDeviceHostForProxy(_: PVOID, _: PVOID, _: *mut HDV_HOST) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HdvInitializeDeviceHostForProxyEx(
        _: PVOID,
        _: PVOID,
        _: u32,
        _: *mut HDV_HOST,
    ) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HdvTeardownDeviceHost(_: HDV_HOST) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HdvCreateDeviceInstance(
        _: HDV_HOST,
        _: HDV_DEVICE_TYPE,
        _: *const GUID,
        _: *const GUID,
        _: *const c_void,
        _: PVOID,
        _: *mut HDV_DEVICE,
    ) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HdvReadGuestMemory(_: HDV_DEVICE, _: u64, _: u32, _: *mut u8) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HdvWriteGuestMemory(_: HDV_DEVICE, _: u64, _: u32, _: *const u8) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HdvCreateGuestMemoryAperture(
        _: HDV_DEVICE,
        _: u64,
        _: u32,
        _: BOOL,
        _: *mut PVOID,
    ) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HdvDestroyGuestMemoryAperture(_: HDV_DEVICE, _: PVOID) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HdvDeliverGuestInterrupt(_: HDV_DEVICE, _: u64, _: u32) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HdvRegisterDoorbell(
        _: HDV_DEVICE,
        _: HDV_PCI_BAR_SELECTOR,
        _: u64,
        _: u64,
        _: u64,
        _: HANDLE,
    ) -> HRESULT {
        E_NOTIMPL
    }
    pub unsafe fn HdvUnregisterDoorbell(
        _: HDV_DEVICE,
        _: HDV_PCI_BAR_SELECTOR,
        _: u64,
        _: u64,
        _: u64,
    ) -> HRESULT {
        E_NOTIMPL
    }
}
#[cfg(not(windows))]
pub use not_windows::*;
