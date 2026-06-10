//! Raw FFI to the Hyper-V **VM worker process** device channel: the
//! `GetVmWorkerProcess` export of `vmwpctrl.dll` plus the FlexibleIov
//! guest-notification COM interfaces it hands out.
//!
//! This is the owner-side route to a real (kernel-signaled) doorbell for an
//! `ExternalRestricted` device host, whose own `HdvRegisterDoorbell` is denied
//! by design (observed: `E_ACCESSDENIED`). A process that **owns** the compute
//! system asks the VM worker for the device object and registers the doorbell
//! there:
//!
//! ```text
//! GetVmWorkerProcess(runtimeId, IID_IVmVirtualDeviceAccess, …)      vmwpctrl.dll
//!   → IVmVirtualDeviceAccess::GetDevice(FLEXIO_DEVICE_ID, instanceId, …)
//!     → QueryInterface(IVmFiovGuestMemoryFastNotification)
//!       → RegisterDoorbell(bar, offset, trigger, flags, event)
//! ```
//!
//! Shapes are transcribed from Microsoft's open-source WSL (MIT) —
//! `src/windows/service/inc/windowsdefs.idl` (interfaces, IIDs, enums),
//! `src/windows/inc/wdk.h` (`GetVmWorkerProcess`, `FLEXIO_DEVICE_ID`),
//! `src/windows/common/DeviceHostProxy.cpp` (call flow) — with
//! `IVmFiovGuestMemoryFastNotification` / `IVmVirtualDeviceAccess::GetDevice`
//! also documented on Microsoft Learn (`/windows/win32/devnotes/`).
//!
//! **Vtable layout trap** (from the IDL): `IVmVirtualDeviceAccess` has exactly
//! **one** vtable slot (`_Reserved`) between IUnknown and `GetDevice` — the
//! original declaration's other preceding method is `[call_as]` and emits no
//! vtable entry. `GetDevice` is slot **4** (0-based, counting IUnknown's 3).
//!
//! `vmwpctrl.dll` ships no import library, so [`get_vm_worker_process`] binds
//! the export dynamically (as WSL does). This crate is the unsafe
//! transcription only — the safe wrapper lives in `hdv::vmworker`.

#![allow(non_camel_case_types, non_snake_case)]

use core::ffi::c_void;

pub use hdv_sys::{GUID, HANDLE, HRESULT};

/// `IID_IUnknown` `{00000000-0000-0000-C000-000000000046}`.
pub const IID_IUNKNOWN: GUID = GUID {
    Data1: 0,
    Data2: 0,
    Data3: 0,
    Data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46],
};

/// `IID_IVmVirtualDeviceAccess` `{3e57bd3c-5a5d-4bdc-a0a6-5b4193d4b719}`.
pub const IID_IVM_VIRTUAL_DEVICE_ACCESS: GUID = GUID {
    Data1: 0x3e57_bd3c,
    Data2: 0x5a5d,
    Data3: 0x4bdc,
    Data4: [0xa0, 0xa6, 0x5b, 0x41, 0x93, 0xd4, 0xb7, 0x19],
};

/// `IID_IVmFiovGuestMemoryFastNotification` `{f5dfbec1-b9f3-4b26-bf6f-c251448bcf7a}`.
pub const IID_IVM_FIOV_GUEST_MEMORY_FAST_NOTIFICATION: GUID = GUID {
    Data1: 0xf5df_bec1,
    Data2: 0xb9f3,
    Data3: 0x4b26,
    Data4: [0xbf, 0x6f, 0xc2, 0x51, 0x44, 0x8b, 0xcf, 0x7a],
};

/// `IID_IVmFiovGuestMmioMappings` `{9d416457-abbc-46cf-8b93-901c68bec627}` —
/// the section-backed MMIO (DAX window) sibling on the same device object.
pub const IID_IVM_FIOV_GUEST_MMIO_MAPPINGS: GUID = GUID {
    Data1: 0x9d41_6457,
    Data2: 0xabbc,
    Data3: 0x46cf,
    Data4: [0x8b, 0x93, 0x90, 0x1c, 0x68, 0xbe, 0xc6, 0x27],
};

/// `FLEXIO_DEVICE_ID` `{a8679153-843f-467f-ad7e-f429328f7568}` — the
/// `GetDevice` *category* id for FlexibleIov devices.
pub const FLEXIO_DEVICE_ID: GUID = GUID {
    Data1: 0xa867_9153,
    Data2: 0x843f,
    Data3: 0x467f,
    Data4: [0xad, 0x7e, 0xf4, 0x29, 0x32, 0x8f, 0x75, 0x68],
};

/// `FIOV_BAR_SELECTOR` (`v1_enum` → 32-bit).
pub type FIOV_BAR_SELECTOR = i32;
pub const FIOV_BAR0: FIOV_BAR_SELECTOR = 0;
pub const FIOV_BAR1: FIOV_BAR_SELECTOR = 1;
pub const FIOV_BAR2: FIOV_BAR_SELECTOR = 2;
pub const FIOV_BAR3: FIOV_BAR_SELECTOR = 3;
pub const FIOV_BAR4: FIOV_BAR_SELECTOR = 4;
pub const FIOV_BAR5: FIOV_BAR_SELECTOR = 5;
pub const FIOV_ROMBAR: FIOV_BAR_SELECTOR = 6;

/// `FiovMmioMappingFlags` (`v1_enum` → 32-bit).
pub type FiovMmioMappingFlags = i32;
pub const FIOV_MMIO_MAPPING_FLAG_NONE: FiovMmioMappingFlags = 0;
pub const FIOV_MMIO_MAPPING_FLAG_WRITEABLE: FiovMmioMappingFlags = 0x1;
pub const FIOV_MMIO_MAPPING_FLAG_EXECUTABLE: FiovMmioMappingFlags = 0x2;

// --- COM vtables (hand-declared; `extern "system"` is the COM calling
// convention on both x86 and x64). Each interface struct is just the vtable
// pointer, so a `*mut IVm…` is a valid COM interface pointer. ---

/// `IUnknown`'s three slots, the prefix of every vtable below.
#[repr(C)]
pub struct IUnknownVtbl {
    pub QueryInterface: unsafe extern "system" fn(
        this: *mut c_void,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> HRESULT,
    pub AddRef: unsafe extern "system" fn(this: *mut c_void) -> u32,
    pub Release: unsafe extern "system" fn(this: *mut c_void) -> u32,
}

/// `IVmVirtualDeviceAccess` — `GetDevice` at slot 4 (see the module doc's
/// layout-trap note).
#[repr(C)]
pub struct IVmVirtualDeviceAccessVtbl {
    pub base: IUnknownVtbl,
    /// `[local] HRESULT _Reserved()` — never call.
    pub _Reserved: unsafe extern "system" fn(this: *mut c_void) -> HRESULT,
    /// `HRESULT GetDevice(REFGUID CategoryID, REFGUID DeviceID, IUnknown** Device)`.
    pub GetDevice: unsafe extern "system" fn(
        this: *mut c_void,
        categoryId: *const GUID,
        deviceId: *const GUID,
        device: *mut *mut c_void,
    ) -> HRESULT,
}

#[repr(C)]
pub struct IVmVirtualDeviceAccess {
    pub lpVtbl: *const IVmVirtualDeviceAccessVtbl,
}

/// `IVmFiovGuestMemoryFastNotification` — kernel-consumed doorbells on a
/// FlexibleIov device's BARs.
#[repr(C)]
pub struct IVmFiovGuestMemoryFastNotificationVtbl {
    pub base: IUnknownVtbl,
    /// `HRESULT RegisterDoorbell(FIOV_BAR_SELECTOR, UINT64 BarOffset, UINT64
    /// TriggerValue, UINT64 Flags, [system_handle(sh_event)] HANDLE)` — the
    /// proxy duplicates the event handle into the worker process.
    pub RegisterDoorbell: unsafe extern "system" fn(
        this: *mut c_void,
        barIndex: FIOV_BAR_SELECTOR,
        barOffset: u64,
        triggerValue: u64,
        flags: u64,
        notificationEvent: HANDLE,
    ) -> HRESULT,
    /// `HRESULT UnregisterDoorbell(FIOV_BAR_SELECTOR, UINT64, UINT64, UINT64)`.
    pub UnregisterDoorbell: unsafe extern "system" fn(
        this: *mut c_void,
        barIndex: FIOV_BAR_SELECTOR,
        barOffset: u64,
        triggerValue: u64,
        flags: u64,
    ) -> HRESULT,
}

#[repr(C)]
pub struct IVmFiovGuestMemoryFastNotification {
    pub lpVtbl: *const IVmFiovGuestMemoryFastNotificationVtbl,
}

/// `IVmFiovGuestMmioMappings` — section-backed BAR ranges (the DAX-window
/// mechanism). Declared now because it rides the same `GetDevice` channel;
/// unused until the DAX work.
#[repr(C)]
pub struct IVmFiovGuestMmioMappingsVtbl {
    pub base: IUnknownVtbl,
    /// `HRESULT CreateSectionBackedMmioRange(FIOV_BAR_SELECTOR, ULONG64
    /// BarOffsetInPages, UINT64 PageCount, FiovMmioMappingFlags,
    /// [system_handle(sh_section)] HANDLE, UINT64 SectionOffsetInPages)`.
    pub CreateSectionBackedMmioRange: unsafe extern "system" fn(
        this: *mut c_void,
        barIndex: FIOV_BAR_SELECTOR,
        barOffsetInPages: u64,
        pageCount: u64,
        mappingFlags: FiovMmioMappingFlags,
        sectionHandle: HANDLE,
        sectionOffsetInPages: u64,
    ) -> HRESULT,
    /// `HRESULT DestroySectionBackedMmioRange(FIOV_BAR_SELECTOR, ULONG64)`.
    pub DestroySectionBackedMmioRange: unsafe extern "system" fn(
        this: *mut c_void,
        barIndex: FIOV_BAR_SELECTOR,
        barOffsetInPages: u64,
    ) -> HRESULT,
}

#[repr(C)]
pub struct IVmFiovGuestMmioMappings {
    pub lpVtbl: *const IVmFiovGuestMmioMappingsVtbl,
}

/// `STDAPI GetVmWorkerProcess(REFGUID VirtualMachineId, REFIID ObjectIid,
/// IUnknown** Object)` — `VirtualMachineId` is the HCS **runtime** id
/// (`Properties.RuntimeId`), *not* the configuration GUID.
pub type GetVmWorkerProcessFn = unsafe extern "system" fn(
    vmRuntimeId: *const GUID,
    objectIid: *const GUID,
    object: *mut *mut c_void,
) -> HRESULT;

/// Opaque cookie from [`CoIncrementMTAUsage`].
pub type CO_MTA_USAGE_COOKIE = *mut c_void;

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::sync::OnceLock;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(lpLibFileName: *const u16) -> *mut c_void;
        fn GetProcAddress(hModule: *mut c_void, lpProcName: *const u8) -> *mut c_void;
    }

    #[link(name = "ole32")]
    unsafe extern "system" {
        /// Keeps the process's multi-threaded apartment alive so COM calls work
        /// from threads that never called `CoInitializeEx` (we are a DLL in a
        /// host process whose threading we don't control).
        pub fn CoIncrementMTAUsage(pCookie: *mut CO_MTA_USAGE_COOKIE) -> HRESULT;
    }

    /// Resolve `vmwpctrl.dll!GetVmWorkerProcess`, once. `None` if the DLL or
    /// export is absent (no Hyper-V, or a future Windows dropping the
    /// undocumented export — callers must degrade gracefully).
    pub fn get_vm_worker_process() -> Option<GetVmWorkerProcessFn> {
        static FN: OnceLock<Option<GetVmWorkerProcessFn>> = OnceLock::new();
        *FN.get_or_init(|| {
            let name: Vec<u16> = "vmwpctrl.dll\0".encode_utf16().collect();
            // SAFETY: `name` is NUL-terminated UTF-16; LoadLibraryW tolerates
            // a missing DLL by returning null.
            let module = unsafe { LoadLibraryW(name.as_ptr()) };
            if module.is_null() {
                return None;
            }
            // SAFETY: `module` is a live HMODULE (never freed — resolved once
            // per process); the export name is NUL-terminated ASCII.
            let addr = unsafe { GetProcAddress(module, c"GetVmWorkerProcess".as_ptr().cast()) };
            if addr.is_null() {
                return None;
            }
            // SAFETY: the export's signature is `GetVmWorkerProcessFn` (WSL
            // `wdk.h`); transmuting the non-null proc address to it is the
            // standard GetProcAddress contract.
            Some(unsafe { std::mem::transmute::<*mut c_void, GetVmWorkerProcessFn>(addr) })
        })
    }
}
#[cfg(windows)]
pub use windows_impl::{get_vm_worker_process, CoIncrementMTAUsage};

// Off-Windows stubs: keep the crate graph checkable on non-Windows dev hosts.
#[cfg(not(windows))]
mod not_windows {
    use super::*;
    pub fn get_vm_worker_process() -> Option<GetVmWorkerProcessFn> {
        None
    }
    /// # Safety
    /// Stub; never dereferences `pCookie`.
    pub unsafe fn CoIncrementMTAUsage(_pCookie: *mut CO_MTA_USAGE_COOKIE) -> HRESULT {
        -0x7fff_bfff // E_NOTIMPL
    }
}
#[cfg(not(windows))]
pub use not_windows::{get_vm_worker_process, CoIncrementMTAUsage};

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    /// The hand-declared vtables must be exactly their slot count in pointers —
    /// any accidental field reorder/addition would shift COM slots.
    #[test]
    fn vtable_layouts() {
        let p = size_of::<usize>();
        assert_eq!(size_of::<IUnknownVtbl>(), 3 * p);
        assert_eq!(size_of::<IVmVirtualDeviceAccessVtbl>(), 5 * p); // GetDevice = slot 4
        assert_eq!(size_of::<IVmFiovGuestMemoryFastNotificationVtbl>(), 5 * p);
        assert_eq!(size_of::<IVmFiovGuestMmioMappingsVtbl>(), 5 * p);
    }
}
