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

### Caller-supplied class / host GUIDs
The device **instance** id is already caller-chosen (`hvfs_add_share`'s `instance_id`). Still fixed:
- the device **class** id — pinned to the well-known `VIRTIO_FS_DEVICE_CLASS_ID` because the VID
  refuses a second virtio-fs device under any *custom* class (a platform constraint, not a choice;
  see the spike). Unlikely to ever be caller-chosen for virtio-fs.
- the device-**host** id — a built-in constant (`HVFS_DEVICE_HOST_ID`). Let the caller override it
  so the host can coexist with another device host where the platform allows.

- Refs: `// TODO(caller-guids)` in `crates/hdv/src/pci.rs` (the constants + `create_shared`).

### Wire `hvfs_set_logger`
`hvfs_set_logger` is currently a **no-op stub** — it accepts `(cb, ctx)` and ignores them
(`crates/hyperv_virtiofs/src/lib.rs`, `// TODO: store cb/ctx…`). The header advertises it as
"install a process-global logger", so until it is wired this is an unfulfilled promise. Store the
callback in a global and route the device-host log stream to it. (Consider disclosing "not yet
wired" in the header doc comment + regenerating until it lands, to stay honest like `ro`.)

### End-to-end CI on a self-hosted Hyper-V runner
The e2e ladder ([`testing.md`](testing.md)) is reproducible locally but doesn't run on hosted CI —
GitHub `windows-latest` has no nested virtualization, so HCS/HDV can't create a VM. To gate merges on
it, stand up a **self-hosted runner on a Hyper-V-capable Windows host**, stage (or build) the guest
artifacts in a pre-step, and drive the same `test/run-e2e.ps1`. Until then the e2e tier is a manual,
documented, on-demand check; only the build/lint/unit gates are automated.

- Refs: [`testing.md`](testing.md), `.github/workflows/ci.yml`, `test/run-e2e.ps1`.

### Guest memory: the "aperture staleness" was a `max_address` ceiling bug (RESOLVED)
For most of this project the `file_selftest` flakiness — guest mounts fine, then stalls/errors
mid-I/O, worse on sustained transfers — was attributed to **HDV aperture *staleness*** ("snapshot
semantics"). That diagnosis was **wrong**. Byte-level data-path instrumentation (June 2026,
`VIRTIO_HDV_TRACE` + `VIRTIO_HDV_APERTURE_STATS`) traced the actual failure to a FUSE reply of
**`-EIO`** that the guest surfaces as `ls: Invalid argument`, and the EIO to a descriptor buffer at
GPA **`0x1_0269_1680`** (≈4.04 GiB) being **rejected before it ever reached the aperture path**
(`bad_range=0`, zero aperture create failures across ~254 k ops).

**Root cause.** Hyper-V splits guest RAM around the 32-bit MMIO hole: low RAM below ~3.75 GiB, the
remainder **remapped to start at 4 GiB**. A 4 GiB guest therefore references DMA buffers at GPAs
*above* 4 GiB. We set OpenVMM's `max_address` to the flat RAM size (`memory_mb · 1 MiB =
0x1_0000_0000`), so every buffer landing in high RAM exceeded the ceiling and `guestmem` rejected it
**upstream of our fallback** → the FUSE server returned `-EIO`. Flaky because only some allocations
land high, and **sustained I/O is the most exposed** — which is exactly why it masqueraded as
"staleness on large transfers."

**Fix** (`crates/virtio-hdv/src/mem.rs`, `ram_size_to_max_gpa`): derive `max_address` from the RAM
size as `4 GiB + ram_size`, a provably-safe upper bound that admits all real RAM for any low/high
split (the high region is `ram − low < ram`, so its top is `< 4 GiB + ram`). Result: `file_selftest`
passes reliably, **including 64 MiB transfers + 500 files** (sha256 write-through verified; ~750–970
MB/s read, ~160–190 MB/s write), with a healthy on-demand aperture cache (~99.9 % hit rate, quota
evict-and-retry exercised, zero failures).

**What this retires.** The `PinBackingPages` (rejected `0xC037002E`) and `AllowOvercommit: false`
(no help) experiments were chasing a backing-coherence problem that **did not exist**: those A/B
failures were this ceiling bug, not snapshot semantics. The closed
`HdvGuestMemoryEvictionWorker` is confirmed (by RE of both `vmdevicehost.dll` and
`wsldevicehost.dll`, see `hdv-aperture-internals.md`) to be **quota/resource management, not a
coherence-notification protocol** — there is no closed eviction notification to reverse-engineer.
An HDV aperture is a direct `VidMapMemoryBlockPageRangeEx` of *backed* pages and is coherent; our
on-demand per-range cache maps only backed pages and reuses them.

**Residual / belt-and-suspenders.** `RearmNet` (5 ms MSI re-arm, `virtio-hdv/src/lib.rs`) and the
boot-retry remain. They cover a *separate*, much rarer symptom (a fraction of boots stall on
early-boot guest-memory apertures, and EVENT_IDX notification suppression); they are not proven
necessary now and removing them is a follow-up spike, to be measured against `file_selftest`, not
removed blind.

- Refs: `crates/virtio-hdv/src/mem.rs` (`ram_size_to_max_gpa`, on-demand cache, instrumentation),
  `virtio-hdv/src/lib.rs` (`RearmNet`), `docs/hdv-aperture-internals.md`; public HDV API at
  learn.microsoft.com/virtualization/api/hcs/reference/hdv.

---

## shipped

The verified record of what works. Each item is proven by tests and/or the design docs.

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
