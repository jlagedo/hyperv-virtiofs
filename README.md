# hyperv-virtiofs

**virtiofsd for Hyper-V.** A standalone C-ABI DLL that attaches an
[OpenVMM](https://github.com/microsoft/openvmm) **virtio-fs** device to *any*
HCS / Hyper-V guest via the Windows **Host Device Virtualization (HDV)** API — so a
host process can share a host directory into a VM over virtio-fs, with no 9p and no
in-box device support.

It is the open counterpart to the device-host glue that ships **closed** inside
WSL's `wsldevicehost.dll`.

> **Status: it works through the C ABI.** A stock Rocky Linux 10 (EL10) guest under
> Hyper-V/HCS **mounts a host directory over our HDV virtio-fs bridge** — reads a
> host file and writes one back — with no 9p and no kernel changes, driven entirely
> through the exported `hvfs_attach` (`hcs-testvm/tests/attach_abi.rs`,
> `PROOF_COMPLETE_PASS`). The transport (`virtio-hdv`) drives OpenVMM's public
> `VirtioPciDevice`/`VirtioFsDevice` over the `ExternalRestricted` FlexibleIov proxy
> path. Remaining for product use: live `set_shares`, caller-supplied device GUIDs,
> and a fully coherent guest-memory mapping (HDV apertures are an evictable cache;
> the proof uses a persistent mapping + interrupt re-arm + boot retry to mask the
> residual staleness). See [Roadmap](#roadmap).

## Why

Modern enterprise-Linux guests (RHEL/Rocky/Alma EL10) drop the `9p` filesystem but
ship `virtiofs.ko` in-box. On Windows, HCS exposes Plan 9 sharing but **no**
virtio-fs device. HDV is the documented escape hatch: a host-side user-mode process
can register a *custom* virtual device against a compute system it owns. This
project puts a real virtio-fs device on that escape hatch — empirically, a stock
EL10 kernel mounts an OpenVMM virtio-fs share read-write under WHP (proof in the
design notes).

## Design: agnostic by construction

The product's entire contract is the C ABI in
[`include/hyperv_virtiofs.h`](include/hyperv_virtiofs.h). It contains **no**
consumer-specific concepts — its vocabulary is purely *compute systems, tags,
directory maps, read-only*. Any host can drive it.

```c
uint32_t hvfs_abi_version(void);
int32_t  hvfs_attach(const char *hcs_system_id, const char *device_json, hvfs_device **out);
int32_t  hvfs_set_shares(hvfs_device *dev, const char *shares_json);  // {"ws":{"path":..,"ro":..}}
int32_t  hvfs_detach(hvfs_device *dev);
const char *hvfs_last_error(void);
void     hvfs_set_logger(hvfs_log_fn cb, void *ctx);
```

Contract rules: `0` = OK / `< 0` = error (details in the thread-local
`hvfs_last_error`); opaque handles; **every** `const char*` is borrowed (no caller
`free`); and **no Rust panic ever crosses the boundary** — every entry point runs
under `catch_unwind` and returns `HVFS_ERR_PANIC` instead of aborting the host.

`hvfs_attach`'s `device_json` carries the initial share + guest memory:

```json
{ "tag": "ws", "path": "C:\\host\\dir", "ro": false, "memory_mb": 512 }
```

`tag` is the virtio-fs mount tag (`mount -t virtiofs <tag> …`), `path` the host
directory, `ro` is accepted but **not yet enforced**, and `memory_mb` must equal the
compute system's RAM (the GPA ceiling the device's DMA may reference). The caller's
own compute-system document **must pre-declare a `FlexibleIov` slot** whose map-key
GUID is the well-known `HVFS_DEVICE_INSTANCE_ID` and whose `EmulatorId` is
`HVFS_DEVICE_CLASS_ID`, with `HostingModel: "ExternalRestricted"` (both GUIDs live in
`hdv::pci`). Those device GUIDs are fixed product constants today — see the
caller-supplied-GUIDs follow-up in the [Roadmap](#roadmap).

## Crate layering

| Crate | Responsibility |
|---|---|
| `hdv-sys` | Raw FFI to the HDV API (`HdvInitializeDeviceHost`, `HdvCreateDeviceInstance`, guest-memory apertures, doorbells). |
| `hdv` | Safe RAII over `hdv-sys`. Device-agnostic — usable for any HDV device. |
| `virtio-hdv` | OpenVMM virtio transport carried over HDV (guest memory ← apertures, kick ← doorbells, config space). Device-neutral. |
| `hyperv_virtiofs` | The `cdylib`: wires OpenVMM's `virtiofs` onto `virtio-hdv`; exposes the C ABI. |

The lower three crates are a reusable "HDV device toolkit"; virtio-fs is just the
first device on top. They can be split to crates.io later if a second device wants
them.

This layering mirrors WSL's closed `wsldevicehost.dll` (1.6 MB, Rust). Its embedded
source paths split by depot prefix: **`oss\…`** = the public `microsoft/openvmm` tree
(`virtio`, `virtiofs`, `pci_core`, `fuse`, `lxutil`, `guestmem` — exactly what we reuse),
and **`hyper-v\…`** = Microsoft's internal Windows depot (not mirrored): an `hdv` crate
(`api.rs` ≈ our `hdv-sys`+`hdv`; `virtio_hdv.rs` ≈ our `virtio-hdv`; `virtiofs.rs` ≈ the
`cdylib` wiring) plus a `wsldevicehost` COM/DLL shim we don't need. So `virtio-hdv` is the
open re-implementation of one internal file (`virtio_hdv.rs`) over otherwise-public crates.

## Bindings — there are none here, on purpose

This repo ships the **DLL + import lib + the C header**. Language bindings live with
each **consumer**, not here, because the agnostic contract is the C ABI. Go consumers
bind it with `syscall.NewLazyDLL` + `NewProc` — **no cgo** — exactly as Windows hosts
bind `computecore.dll`; the surface is five calls, so a maintained cgo module would
be pure overhead. [`examples/`](examples) holds an **authoritative C** reference and
an **illustrative Go** snippet (not a published module — copy the pattern, don't
import it).

Load by **absolute path**, never a bare name: consumers are typically elevated
services, and a bare name invites DLL-preloading. See
[Go's Windows DLL guidance](https://go.dev/wiki/WindowsDLLs).

## Build

Prerequisites: a Rust 1.95+ toolchain and **`protoc`** (the reused OpenVMM crates
pull `mesh → prost → protobuf`, whose build needs the Protocol Buffers compiler).
Install it from your package manager or the [protobuf releases](https://github.com/protocolbuffers/protobuf/releases),
and ensure it's on `PATH` (or set the `PROTOC` env var to the binary).

```pwsh
cargo build --release          # -> target/release/hyperv_virtiofs.dll (+ .dll.lib, .pdb)
cargo test --workspace
cbindgen --config cbindgen.toml --crate hyperv_virtiofs --output include/hyperv_virtiofs.h
```

The OpenVMM crates are git dependencies pinned to one revision in the workspace
`Cargo.toml`; the first build fetches that tree (large) and compiles it.

CI (`windows-latest`) builds, clippy-gates, runs tests, and **fails if the committed
header drifts** from the Rust source. Tagged `v*` pushes publish a GitHub release
carrying `hyperv_virtiofs.{dll,dll.lib,pdb}` + the header — the bundle a consumer
**pins** (it does not build this from source).

## Roadmap

- [x] **Reuse OpenVMM `virtio` + `virtiofs`** — wired as pinned git deps and
  compiling on Windows (the whole tree: `mesh`, `chipset_device`, `pci_core`,
  `lx`/`lxutil` FUSE backend). The foundational feasibility question is answered.
- [x] **HDV FFI + RAII** — real `vmdevicehost.dll` bindings (`HdvInitializeDeviceHost`,
  `HdvCreateDeviceInstance`, guest-memory apertures, doorbells, the proxy ABI) and
  safe wrappers.
- [x] **HDV attach handshake** *(the linchpin)* — the `ExternalRestricted` FlexibleIov
  proxy path works in-process: `HdvInitializeDeviceHostForProxy` →
  `IVmDeviceHostSupport::RegisterDeviceHost` → `HdvProxyDeviceHost`, then the guest
  enumerates the device over VMBus VPCI (`docs/hdv-proxy-abi.md`).
- [x] **virtio-pci-over-HDV transport** — an *adapter*, not a rewrite: implements
  `hdv::pci::PciOps` over OpenVMM's public `VirtioPciDevice`, backing its seams with
  HDV — `GuestMemory` ← apertures (`HdvCreateGuestMemoryAperture`),
  `PciInterruptModel::Msix` ← `HdvDeliverGuestInterrupt`, PCI config + BAR MMIO ←
  HDV's device-vtable callbacks (routed `(bar, offset)` → `find_bar` via internal
  BAR bases, since the VMBus VID owns guest-facing BAR placement). Drives the reused
  `VirtioFsDevice`; the guest mounts and does file I/O. (`shmem_size = 0` → no DAX
  BAR yet.)
- [x] **Wire `hvfs_attach`** — the cdylib opens the compute system
  (`HcsOpenComputeSystem`), proxy-registers an HDV device host, and calls
  `VirtioHdvDevice::attach`; a guest mounts the share via `device_json` through the
  shipped ABI (`hcs-testvm/tests/attach_abi.rs`, `PROOF_COMPLETE_PASS`). The initial
  share rides in `device_json` (live updates are `set_shares`, below).
- [ ] **Caller-supplied device GUIDs** — today host/class/instance are built-in
  well-known constants (`hdv::pci`) the consumer must mirror in its `FlexibleIov`
  slot, so only one such device can exist per guest. Let the host override them via
  `device_json`, threading the ids through `attach` / `from_proxy` /
  `PciDevice::create`.
- [ ] **Coherent guest memory** — participate in HDV's aperture eviction protocol
  (à la WSL's `HdvGuestMemoryEvictionWorker`) for a fully coherent, zero-copy mapping
  (replacing the persistent-aperture + re-arm + retry mitigation).
- [ ] **`set_shares`** — live directory-map updates with a Windows reparse/junction-
  safe directory jail.

## License

[MIT](LICENSE). Reuses MIT-licensed OpenVMM virtio crates — see [NOTICE](NOTICE).
