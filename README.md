# hyperv-virtiofs

**virtiofsd for Hyper-V.** A standalone C-ABI DLL that attaches an
[OpenVMM](https://github.com/microsoft/openvmm) **virtio-fs** device to *any*
HCS / Hyper-V guest via the Windows **Host Device Virtualization (HDV)** API — so a
host process can share a host directory into a VM over virtio-fs, with no 9p and no
in-box device support.

It is the open counterpart to the device-host glue that ships **closed** inside
WSL's `wsldevicehost.dll`.

> **Status: work in progress.** The public C ABI is stable, and the OpenVMM
> `virtio` + `virtiofs` crates are **wired in and compiling** on Windows (the
> reuse that avoids reimplementing a FUSE server — verified, not assumed). The
> remaining work is the HDV transport bridge itself; until it lands, `hvfs_attach`
> returns `HVFS_ERR_NOT_IMPLEMENTED`. See [Roadmap](#roadmap).

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
1. **HDV FFI + RAII** — real `vmdevicehost.dll` bindings (`HdvInitializeDeviceHost`,
   `HdvCreateDeviceInstance`, guest-memory apertures, doorbells) and safe wrappers.
2. **virtio-pci-over-HDV transport** — reimplement the virtio-pci modern
   config-space machine (OpenVMM's `VirtioPciDevice` is too chipset-coupled to
   reuse) driven by HDV callbacks; source `QueueResources` (memory ← apertures,
   kick ← doorbells, IRQ ← HDV interrupt) and drive the reused `VirtioFsDevice`.
3. **HDV attach handshake** *(the linchpin)* — prove `HdvInitializeDeviceHost`
   against an HCS compute system owned by the caller, surfacing a virtio-fs PCI
   device the guest enumerates. Until this lands, `hvfs_attach` is
   `HVFS_ERR_NOT_IMPLEMENTED`.
4. **`set_shares`** — live directory-map updates with a Windows reparse/junction-
   safe directory jail.

## License

[MIT](LICENSE). Reuses MIT-licensed OpenVMM virtio crates — see [NOTICE](NOTICE).
