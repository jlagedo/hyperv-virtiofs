# HDV proxy ABI — reverse-engineered, then confirmed against the public PDB (2026-06-02)

> **Confirmed.** The signatures below were first recovered by disassembling `vmdevicehost.dll`, then
> **validated against Microsoft's published `VmDeviceHost.pdb`** (the public symbol server *does* carry
> it, even though `wsldevicehost.pdb` 404s). The PDB's C++ symbols match our inferences exactly — see
> [§ Ground truth](#ground-truth--public-vmdevicehostpdb) at the bottom. (`wsldevicehost.dll` itself
> stays closed/symbol-less; this is the lower, reusable HDV layer it sits on.)


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
| `HdvInitializeDeviceHostForProxy` | `0xC960` | `(GUID* ctx, IUnknown* /*IVmDeviceHostSupport*/, HDV_HOST* out) -> HRESULT` | disasm + PDB |
| `HdvInitializeDeviceHostForProxyEx` | `0xCAA0` | `(GUID* ctx, IUnknown* /*IVmDeviceHostSupport*/, DWORD flags, HDV_HOST* out) -> HRESULT` | disasm + PDB |

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

`ctx` (the first arg of both `…ForProxy…`) is a **`*const GUID`** — the device-host **identity**.
`Init` (`E808`) reads it with a 16-byte `movups xmm0,[rbp]` and copies it into the device-host record,
so it is **not nullable** (passing null faults). Confirmed by spike: null → `STATUS_ACCESS_VIOLATION`;
a valid `*const GUID` → success. Distinct from the per-device class/instance ids.

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

## Proven end-to-end (2026-06-02, `hcs-testvm/tests/attach_proxy.rs`)

The spike drives the whole path in one process and **passes**:

1. We implement **only** `IVmDeviceHostSupport` (`hdv::proxy::DeviceHostSupport`) — `RegisterDeviceHost`
   → `HdvProxyDeviceHost`. The `IVmDeviceHost` is built by HDV inside `ForProxy` and handed to us as a
   parameter, so we never author it (one COM object, not two).
2. `HcsModifyComputeSystem` is bound (`hcs-sys`) and works for the hot-add; the slot may equivalently
   be declared in the create document (cold) — both reservation paths succeed once the device host is
   proxy-registered.
3. `ctx` resolved: a non-null `*const GUID` (device-host id) — see above.
4. The proxy `HDV_HOST` takes `HdvCreateDeviceInstance` exactly like the in-process host. The VID drives
   the device `Initialize → SetConfiguration → GetDetails → Start → ReadConfigSpace`, and `GetDeviceInstance`
   is handled by HDV's own `IVmDeviceHost` wrapper — we never see it.

Guest side: the device surfaces over **VMBus VPCI**, so the guest needs `hv_vmbus` + `pci-hyperv`
loaded. With them, the guest logs `hv_pci <instanceId>: PCI host bridge to bus 0001:00` and
`pci 0001:00:00.0: [1af4:1100]`. Outcome: the `ExternalRestricted` FlexibleIov path works in-process;
the design §7 #1 linchpin is retired. Remaining for the product: swap the driverless `SpikeDevice` for
the OpenVMM `VirtioFsDevice` (task #8) and ensure the cage guest image loads the VPCI modules.

## Ground truth — public `VmDeviceHost.pdb`

`vmdevicehost.dll` has a **public PDB on the Microsoft symbol server** (`wsldevicehost.pdb` does not):

```
https://msdl.microsoft.com/download/symbols/VmDeviceHost.pdb/123599F7CFF5F4908D6D78280AC706D11/VmDeviceHost.pdb
```

(PDB GUID `123599F7-CFF5-F490-8D6D-78280AC706D1`, age 1 — from the DLL's RSDS debug record; the path key
is `<GUID-no-dashes><age>`.) It has function symbols (names + undecorated C++ signatures) but **no UDT
member layouts** (stripped public PDB). Dumped via a small DIA (`msdia140.dll`) reader. The symbols
confirm every reverse-engineered inference:

| Public PDB symbol (undecorated) | Confirms |
|---|---|
| `HDV::CreateDeviceHostForProxy(IVmDeviceHostSupport const&, _GUID const&, HDV_DEVICE_HOST_FLAGS)` | `ctx` **is** a `_GUID const&` (device-host id); arg2 is the `IVmDeviceHostSupport` callback; the `Ex` dword is `HDV_DEVICE_HOST_FLAGS` |
| `HDV::CreateDeviceHost(HCS_SYSTEM__*, HDV_DEVICE_HOST_FLAGS)` | the in-process path (`HdvInitializeDeviceHost`) — owns a system, no proxy |
| `HDV::DeviceHost::DeviceHost(_GUID const&, HDV::DeviceHostFlags)` vs `(HCS_SYSTEM__*, …)` | two ctors: proxy-by-GUID-id vs owned-system — so the proxy host is identified by the `ctx` GUID |
| `HDV::VirtualMachine::RegisterDeviceHost(IVmDeviceHostSupport const&, IVmDeviceHost*, unsigned long, unsigned __int64*)` | `HdvProxyDeviceHost`'s internal target — `(IVmDeviceHost*, pid, ipcSection*)` |
| `HDV::DeviceHost::GetDeviceInstance(_GUID const&, _GUID const&, IUnknown**)` | HDV serves `IVmDeviceHost::GetDeviceInstance(classId, instanceId)` itself — we never author it |
| `HDV::PciExtensibleDevice::PciExtensibleDevice(_GUID const&, _GUID const&, void const*, void*, bool)` | the `HdvCreateDeviceInstance` device: `(classId, instanceId, vtable, context, +bool)` |

Real namespace map (useful when reading further): the device side is `HDV::DeviceHost` /
`HDV::ExtensibleDevice` / `HDV::PciExtensibleDevice` / `HDV::IExtensibleDevice` / `HDV::IDeviceHost`;
the host bridge is `HDV::VirtualMachine`; the proxy data path is `HDV::IPC::*`
(`DeviceHostCallHandler`, `Details::CallHandlerThread`). Net: our binding
`HdvInitializeDeviceHostForProxy(device_host_id: *const GUID, support: IUnknown*, out: HDV_HOST*)` and
the `hdv::proxy` `IVmDeviceHostSupport` object are correct as shipped.
