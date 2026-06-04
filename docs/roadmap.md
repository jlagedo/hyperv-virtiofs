# Roadmap

**The single source of truth for status** — everything left to spike, confirm, or develop.
If a capability is unfinished, blocked, or unverified, it is tracked here — not scattered
across the README, the spike notes, or code TODOs. Those carry at most a one-line pointer
back to this file. Design and record detail for shipped work lives in the design docs
([`share-abi.md`](share-abi.md), [`hotplug-spike.md`](hotplug-spike.md),
[`hdv-proxy-abi.md`](hdv-proxy-abi.md)).

Each open item is tagged by the kind of work remaining:
- **develop** — the path is understood; it needs building.
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
- **Revisit when:** the platform adds hot-remove for FlexibleIov-class devices. WSL's "not all
  versions of Windows support it" implies **some** builds may allow FlexibleIov Remove, so before
  designing around the blocker permanently it's worth measuring: run the Remove path on other host
  builds (Win10 `{2,3}` vs Win11 `{2,7}`) and record which, if any, return `HVFS_OK` with the guest
  device actually disappearing. If a supported build turns up, this item moves from **blocked** to
  **develop**.
- Refs: `hvfs_remove_share` (`crates/hyperv_virtiofs/src/lib.rs`), the spike's Stage 3 +
  support-matrix analysis ([`hotplug-spike.md`](hotplug-spike.md)).

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

### DAX window (`shmem_size > 0`) — *nice-to-have, performance only*
Today `shmem_size = 0`: all I/O is FUSE-over-virtqueue, with guest DMA serviced by the on-demand
aperture cache (`crates/virtio-hdv/src/mem.rs`). DAX instead maps host page-cache pages **directly
into a guest BAR4 window** (virtio-fs shared-memory region, driven by `FUSE_SETUPMAPPING`), so the
guest loads/stores against host memory — zero-copy, coherent `mmap`, no per-I/O VMEXIT, no
double-caching. The HDV plumbing exists and is proven: `HdvCreate/DestroySectionBackedMmioRange`
(device-host exports, used by `wsldevicehost.dll`); wiring is an HDV `MemoryMapper` passed as
`shared_mem_mapper` + `shmem_size > 0`.

**Not** a WSL-parity gap — WSL's file shares are non-DAX too (its section-backed MMIO path serves
only the WSLg shared-memory device). This is going *beyond* WSL, for bulk-data/`mmap` workloads.

Limitations to weigh before investing:
- **Bulk-data only.** ~6–18× on sequential/random data reads and enables `mmap` (0 → fast) **when the
  working set fits the window**; if data exceeds the window, reclaim thrashes and it *regresses*.
- **No metadata help.** DAX accelerates data bytes of open files, not `LOOKUP`/`GETATTR`/`OPEN`
  round-trips — so it does nothing for the metadata-bound pain of dev-tree shares (`git status`,
  builds, `ls -R`). Profile the real workload first; if the bottleneck is FUSE round-trips, DAX is
  the wrong lever.
- **Cost is in the backend, not HDV.** Our directory backend (`lxutil::LxVolume`) has no
  `setup_mapping` — only OpenVMM's `SectionFs` does. Real-file DAX needs net-new work: file→NT-section
  conversion, window sizing/reclaim, Windows writeback/coherence. Guest must mount `-o dax` (long
  experimental in Linux). Per-file DAX exists to skip the window for files <32 KB where it's a net loss.
- Cheap de-risking step: a `SectionFs` round-trip spike (which *does* implement `setup_mapping`) proves
  the HDV path end-to-end with zero backend work, before committing to the directory-backend changes.

- Refs: `shmem_size = 0` scope note (`crates/virtio-hdv/src/lib.rs`); OpenVMM `SectionFs`,
  `MemoryMapper`/`MappedMemoryRegion`. Background:
  [LWN virtiofs DAX](https://lwn.net/Articles/813807/),
  [per-file DAX / window cost](https://lwn.net/Articles/870248/),
  [Red Hat fio numbers](https://www.mail-archive.com/virtio-fs@redhat.com/msg02371.html),
  [cloud-hypervisor #5591](https://github.com/cloud-hypervisor/cloud-hypervisor/issues/5591).
