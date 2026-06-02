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

### Coherent guest memory (aperture eviction)
The proof masks HDV aperture-cache staleness with a persistent mapping + interrupt re-arm + a
4-attempt boot retry. For a fully coherent, zero-copy mapping, participate in HDV's aperture
**eviction protocol** (à la WSL's `HdvGuestMemoryEvictionWorker`), replacing the mitigation.

- Refs: `virtio-hdv` guest-memory aperture path; spike's aperture-staleness notes.

---

## shipped

The verified record of what works. Each item is proven by tests and/or the design docs.

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
