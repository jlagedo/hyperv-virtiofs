# Roadmap

**The single source of truth for status** — what's already **shipped** (bottom) and
everything left to spike, confirm, or develop. If a capability is unfinished, blocked, or
unverified, it is tracked here — not scattered across the README, the spike notes, or code
TODOs. Those carry at most a one-line pointer back to this file. Design and record detail for
shipped work lives in the design docs ([`share-abi.md`](share-abi.md),
[`hotplug-spike.md`](hotplug-spike.md), [`hdv-proxy-abi.md`](hdv-proxy-abi.md)).

Each open item is tagged by the kind of work remaining:
- **develop** — the path is understood; it needs building.
- **confirm** — built or believed, but unverified on a wider matrix; needs a measurement.
- **blocked** — the platform refuses it today; tracked so we revisit when that changes.

---

## blocked

### Live share removal (`FlexibleIov` hot-remove)
`hvfs_remove_share` issues an `HcsModifyComputeSystem` **Remove**, but on Win11 26200 the platform
returns `ERROR_NOT_SUPPORTED` (`0x80070032`), independent of `SchemaVersion` ({2,7} tested). There
is no `HdvDestroyDeviceInstance` export. **WSL hits the same wall** — its `DeviceHostProxy::RemoveDevice`
wraps Remove in `CATCH_LOG` ("best effort since not all versions of Windows support it") and reclaims
devices at VM shutdown via a kill-on-close job object.

- **Behaviour today (honest):** the DLL returns `HVFS_ERR_UNSUPPORTED`, releases host-side resources,
  and de-registers the share; the **guest device lingers until the compute system is torn down**
  (reclaim-at-recycle). The caller (e.g. atelier's `atelierd`) owns the recycle policy.
- **Revisit when:** the platform adds hot-remove for FlexibleIov-class devices.
- Refs: `hvfs_remove_share` (`crates/hyperv_virtiofs/src/lib.rs`), the spike's Stage 3 +
  support-matrix analysis ([`hotplug-spike.md`](hotplug-spike.md)).

---

## confirm

### Re-test live removal across the Windows support matrix
WSL's "not all versions of Windows support it" implies **some** builds do allow FlexibleIov Remove.
Before designing around the blocker permanently, measure it: run the Remove path on other Windows
host builds (and Win10 `{2,3}` vs Win11 `{2,7}`) and record which, if any, return `HVFS_OK`. If a
supported build exists, "Live share removal" above moves from **blocked** to **develop**.

- Verification: `hvfs_remove_share` returning `HVFS_OK` (not `HVFS_ERR_UNSUPPORTED`) on some build,
  with the guest device actually disappearing.

---

## develop

### `ro` enforcement (read-only shares)
`share_json`'s `ro: true` is **honestly refused** today — `hvfs_add_share` returns
`HVFS_ERR_NOT_IMPLEMENTED` rather than silently mounting read-write (the ABI won't claim a guarantee
it can't keep). To deliver it, honor `ro` in the FUSE backend (`virtiofs`/`lx` ops) behind a
Windows reparse/junction-safe directory jail. **No ABI change** when it lands — `ro: false`/omitted
already works; enabling `ro: true` just stops returning the not-implemented code.

- Refs: the `ro` branch in `hvfs_add_share` (`crates/hyperv_virtiofs/src/lib.rs`), `ShareConfig.ro`.

### End-to-end CI on a self-hosted Hyper-V runner
The e2e ladder ([`testing.md`](testing.md)) is reproducible locally but doesn't run on hosted CI —
GitHub `windows-latest` has no nested virtualization, so HCS/HDV can't create a VM. To gate merges on
it, stand up a **self-hosted runner on a Hyper-V-capable Windows host**, stage (or build) the guest
artifacts in a pre-step, and drive the same `test/run-e2e.ps1`. Until then the e2e tier is a manual,
documented, on-demand check; only the build/lint/unit gates are automated.

- Refs: [`testing.md`](testing.md), `.github/workflows/ci.yml`, `test/run-e2e.ps1`.

---

## shipped

The verified record of what works. Each item is proven by tests and/or the design docs.

- [x] **Logging / diagnostics via `tracing` + `hvfs_set_logger` wired.** Internal diagnostics
  are structured `tracing` events (the `virtio-hdv` transport firehose under `VIRTIO_HDV_TRACE`,
  the aperture-cache stats under `VIRTIO_HDV_APERTURE_STATS`, and the ABI lifecycle/error events).
  `hvfs_set_logger` (previously a no-op stub) now installs a best-effort process-global subscriber
  that fans both our events *and* the reused OpenVMM crates' events out to the caller's C callback
  (syslog-level mapped) and, when a dev env var is set, to stderr — routing decided once, emitters
  destination-agnostic (the OpenVMM convention; see `CLAUDE.md` "Logging & diagnostics"). Proven by
  the offline delivery unit test (`set_logger_delivers_events_and_none_is_safe`) and verified live:
  `attach_abi` with `VIRTIO_HDV_TRACE=1` routes the data-path firehose through `tracing` to stderr.
  Refs: `crates/hyperv_virtiofs/src/logging.rs`, `crates/virtio-hdv/src/{mem.rs,ratelimit.rs}`.

- [x] **Guest memory: the "aperture-coherence" flakiness was a `max_address` ceiling bug (fixed).**
  The long-blamed `file_selftest` flakiness ("aperture snapshot staleness on sustained I/O") was a
  misdiagnosis. Byte-level instrumentation traced it to a FUSE `-EIO` (guest: `ls: Invalid argument`)
  from a descriptor buffer at GPA ≈4.04 GiB rejected **before** the aperture path ran. Root cause:
  Hyper-V remaps RAM above the 4 GiB MMIO hole, but `max_address` was set to the flat RAM size, so
  high-RAM DMA buffers exceeded the ceiling and `guestmem` rejected them. Fixed by deriving
  `max_address = 4 GiB + ram_size` (`crates/virtio-hdv/src/mem.rs::ram_size_to_max_gpa`).
  `file_selftest` now passes reliably incl. 64 MiB transfers (~99.9 % aperture-cache hit rate, zero
  failures). RE of both `vmdevicehost.dll` and `wsldevicehost.dll` confirmed there is **no closed
  coherence-notification protocol** to mirror — `HdvGuestMemoryEvictionWorker` is quota management;
  an aperture is a direct `VidMapMemoryBlockPageRangeEx` of *backed* pages and is coherent. `RearmNet`
  (5 ms MSI re-arm) + boot-retry remain as belt-and-suspenders for a rarer boot-stall/EVENT_IDX
  window (removal is a measured follow-up, not blind). Full writeup: [`testing.md`](testing.md),
  [`hdv-aperture-internals.md`](hdv-aperture-internals.md).

- [x] **GUID assignment is settled (not caller-tunable, by design).** The device **instance** id is
  caller-supplied over the C ABI (`hvfs_add_share`'s `instance_id`); the device **class** id is the
  platform-mandated `VIRTIO_FS_DEVICE_CLASS_ID` (a *custom* class is refused for a 2nd virtio-fs
  device — proven in the hotplug spike, `ERROR_HV_INVALID_PARAMETER`); the device-**host** id is a
  per-process constant (`HVFS_DEVICE_HOST_ID`) and needs no caller override under Model A (separate
  processes, below). So the earlier "caller-supplied class/host GUIDs" idea is retired — only the
  instance id varies, which is exactly what lets N shares coexist on one host.

- [x] **Concurrency / deployment model: one device host per process (Model A).** The DLL
  registers a device host in the calling process; the supported deployment runs each VM's
  device host in its own process (as WSL runs one `wsldevicehost` surrogate per device).
  Driving *multiple* device hosts in *one* process is **out of scope** — the in-process
  proxy path races in the closed platform (`from_proxy` → `E_ACCESSDENIED 0x80070005`, plus
  an unverified teardown/IPC/aperture surface) on a configuration WSL never ships, so we
  don't claim it. Proven by `hcs-testvm/tests/concurrent_processes.rs` (two hosts, two
  processes, concurrent); rationale in [`share-abi.md`](share-abi.md#deployment-model--one-device-host-per-process-model-a).
  This retires the earlier "caller-supplied host GUID for in-process coexistence" idea —
  separate processes need no such thing.

- [x] **Reuse OpenVMM `virtio` + `virtiofs`** — wired as pinned git deps and compiling on
  Windows (the whole tree: `mesh`, `chipset_device`, `pci_core`, `lx`/`lxutil` FUSE backend).
  The foundational feasibility question is answered.
- [x] **HDV FFI + RAII** — real `vmdevicehost.dll` bindings (`HdvInitializeDeviceHost`,
  `HdvCreateDeviceInstance`, guest-memory apertures, doorbells, the proxy ABI) and safe
  wrappers.
- [x] **HDV attach handshake** *(the linchpin)* — the `ExternalRestricted` FlexibleIov proxy
  path works in-process: `HdvInitializeDeviceHostForProxy` →
  `IVmDeviceHostSupport::RegisterDeviceHost` → `HdvProxyDeviceHost`, then the guest enumerates
  the device over VMBus VPCI ([`hdv-proxy-abi.md`](hdv-proxy-abi.md)).
- [x] **virtio-pci-over-HDV transport** — an *adapter*, not a rewrite: implements
  `hdv::pci::PciOps` over OpenVMM's public `VirtioPciDevice`, backing its seams with HDV —
  `GuestMemory` ← apertures (`HdvCreateGuestMemoryAperture`), `PciInterruptModel::Msix` ←
  `HdvDeliverGuestInterrupt`, PCI config + BAR MMIO ← HDV's device-vtable callbacks (routed
  `(bar, offset)` → `find_bar` via internal BAR bases, since the VMBus VID owns guest-facing
  BAR placement). Drives the reused `VirtioFsDevice`; the guest mounts and does file I/O.
  (`shmem_size = 0` → no DAX BAR yet.)
- [x] **Wire the C ABI (host/share, v2)** — the cdylib opens the compute system
  (`HcsOpenComputeSystem`), proxy-registers one HDV device host (`hvfs_host_open`), and
  **hot-adds a virtio-fs device per share at runtime** (`hvfs_add_share` →
  `HcsModifyComputeSystem` Add); a guest hot-mounts each share through the shipped ABI
  (`hcs-testvm/tests/attach_abi.rs`). Multiple shares coexist on one VM via the well-known
  virtio-fs class id + a caller-supplied unique instance id. Full design:
  [`share-abi.md`](share-abi.md).
