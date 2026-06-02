# HDV proxy ABI — reverse-engineered (2026-06-02)

The `ExternalRestricted` FlexibleIov path needs three `vmdevicehost.dll` exports that are **not** in
the public SDK header `hypervdevicevirtualization.h`. `HdvProxyDeviceHost`'s signature is in
`microsoft/WSL` `src/windows/inc/wdk.h`; the two `…ForProxy…` ones are not in any header (the device
host that calls them — `wsldevicehost.dll` — is closed). They were recovered by disassembling
`vmdevicehost.dll` (`dumpbin /disasm:nobytes`, image base `0x180000000`) and validated by checking the
method against the *known* `HdvInitializeDeviceHost`/`…Ex`/`HdvProxyDeviceHost` shapes.

## The handshake

Two halves, bridged by a COM callback. WSL splits them across processes (host = `wslservice`, device
host = a `dllhost` COM surrogate); **we can run both in one process** (atelierd) — the contract is the
calls, not the process split.

```
device-host side                          host/broker side (implements IVmDeviceHostSupport)
----------------                          --------------------------------------------------
HdvInitializeDeviceHostForProxy(
    ctx, hostSupport /*IVmDeviceHostSupport*/, &hdvHost)
  │  builds hdvHost, wraps it as IVmDeviceHost,
  │  calls back ──────────────────────────────►  IVmDeviceHostSupport::RegisterDeviceHost(
  │                                                   IVmDeviceHost, pid, &ipcSection)
  │                                                 └─ HdvProxyDeviceHost(
  │                                                        HCS_SYSTEM, IVmDeviceHost-as-IUnknown,
  │                                                        pid, &ipcSection)
HdvCreateDeviceInstance(hdvHost, Pci, DeviceClassId=EmulatorId, DeviceInstanceId, vtable, …)
```

Then the host hot-adds the slot (`HcsModifyComputeSystem`, ResourcePath
`VirtualMachine/Devices/FlexibleIov/<DeviceInstanceId>`, `{EmulatorId, HostingModel:
ExternalRestricted}`). The VID's `FinishReservingResources` resolves the slot to the registered device
host via `IVmDeviceHost::GetDeviceInstance(EmulatorId, DeviceInstanceId)` and the guest enumerates it.

## Signatures (x64)

| Export | RVA | Signature | Source |
|---|---|---|---|
| `HdvProxyDeviceHost` | `0xCBE0` | `(HCS_SYSTEM, IUnknown* /*IVmDeviceHost*/, DWORD pid, UINT64* ipcSection) -> HRESULT` | WSL `wdk.h:409` (confirmed by disasm) |
| `HdvInitializeDeviceHostForProxy` | `0xC960` | `(PVOID ctx, IUnknown* /*IVmDeviceHostSupport*/, HDV_HOST* out) -> HRESULT` | disasm |
| `HdvInitializeDeviceHostForProxyEx` | `0xCAA0` | `(PVOID ctx, IUnknown* /*IVmDeviceHostSupport*/, DWORD flags, HDV_HOST* out) -> HRESULT` | disasm |

How the disasm yields each (incoming x64 args = rcx, rdx, r8, r9):

- **`HdvProxyDeviceHost`** — `rcx`→system; `rdx` is AddRef'd then `QueryInterface`d for IID
  **`78523d62-d919-47ca-9cd7-08139172d685` (`IVmDeviceHost`)**; `r8d` is a DWORD (pid); `r9` is the
  `UINT64*` out (`mov [r9-derived], rax`). Matches `wdk.h` exactly → calibration anchor.
- **`HdvInitializeDeviceHostForProxy`** — `rcx`→`rdi` (ctx); `rdx` AddRef'd + `QueryInterface`d for
  IID **`e31aa49b-0914-465e-b145-1b9ba13efb10` (`IVmDeviceHostSupport`)**; `r8`→`rsi`, and the produced
  handle is stored to `[rsi]` → `r8` is the `HDV_HOST*` out. Internal ctor `F7FC(&out, &support, ctx,
  0)`.
- **`HdvInitializeDeviceHostForProxyEx`** — same, plus `r8d` (DWORD flags) flows into the ctor's 4th
  arg (`F7FC(&out, &support, ctx, flags)`) and `r9` is the `HDV_HOST*` out.

`ctx` (the first arg of both `…ForProxy…`) is a 64-bit value handed to the device-host object's
initializer (`F7FC` → `Init(obj, ctx, flags)`); its exact type is **unverified** (the non-proxy
`HdvInitializeDeviceHost` passes a constant `1` in the analogous slot). Treat as an optional `PVOID`
and pass null for the first spike; revisit if init fails.

## COM interfaces (from WSL `src/windows/service/inc/windowsdefs.idl`)

- `IVmDeviceHost` `{78523d62-d919-47ca-9cd7-08139172d685}` — device-host side. One method past IUnknown:
  `GetDeviceInstance(REFGUID DeviceClassId, REFGUID DeviceInstanceId, IUnknown** DeviceInstance)`.
- `IVmDeviceHostSupport` `{e31aa49b-0914-465e-b145-1b9ba13efb10}` — host side. One method past IUnknown:
  `RegisterDeviceHost(IVmDeviceHost* DeviceHost, DWORD ProcessId, UINT64* IpcSectionHandle)`.

## Device-type GUIDs (the `EmulatorId`, = HDV `DeviceClassId`)

From WSL `src/windows/common/GuestDeviceManager.h` / `wdk.h`:

- `VIRTIO_FS_DEVICE_ID = {872270E1-A899-4AF6-B454-7193634435AD}` — virtio-fs (what we want).
- `FLEXIO_DEVICE_ID   = {a8679153-843f-467f-ad7e-f429328f7568}` — the VID's own category (the
  `"DeviceId"` in our spike's failure JSON), used with `IVmVirtualDeviceAccess::GetDevice`.

The COM activation CLSIDs WSL registers (`WslDeviceHost_VirtioFs {60285AE6-…}` etc., `InProcServer32` →
`wsldevicehost.dll`, AppID `{17696EAC-…}` with an empty `DllSurrogate` → `dllhost.exe`) are **only**
how WSL sandboxes its device host out-of-process; they are *not* part of the HDV/HCS contract. An
in-process device host needs none of it.

## Open items before a working spike

1. Implement two COM objects in Rust: `IVmDeviceHost` (our device host) and `IVmDeviceHostSupport` (our
   `RegisterDeviceHost` → `HdvProxyDeviceHost`). Minimal hand-rolled vtables (`#[repr(C)]` +
   `extern "system"` thunks, like `hdv::pci`).
2. Bind/verify `HcsModifyComputeSystem` use for the hot-add (already bound in `hcs-sys`).
3. Resolve `ctx` (arg1) empirically; resolve what `HDV_HOST` from the proxy path expects for
   `HdvCreateDeviceInstance` (likely identical to the in-process host).
4. `GetDeviceInstance` must return an `IUnknown` the VID accepts — likely the device exposed by
   `HdvCreateDeviceInstance` (or an object `vmdevicehost` produces). Confirm during the spike.
