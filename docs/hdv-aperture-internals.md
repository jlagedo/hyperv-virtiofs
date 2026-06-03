# HDV guest-memory apertures: how they work, and how WSL uses them

Clean-room notes on the HDV guest-memory aperture mechanism and WSL's coherence
strategy, reconstructed by **reverse-engineering the shipping binaries** (Ghidra) to
recover *facts, interfaces, and design* — not to copy code. This file informs
`crates/virtio-hdv/src/mem.rs` and the roadmap item "Coherent guest memory."

Binaries examined (Win11 26100/26200):
- `C:\Windows\System32\vmdevicehost.dll` — the HDV platform API. **Public PDB available**
  (`VmDeviceHost.pdb` on `msdl.microsoft.com`), so functions are named.
- `C:\Program Files\WSL\wsldevicehost.dll` — WSL's closed device-host. **No public PDB**
  (404); it embeds a Rust crate named `hyper_v_hdv` (`hyper_v_hdv/src/api.rs`, visible in
  panic-location metadata) — the closed counterpart of *this* project.

> **Provenance / licensing.** Everything below is the *behaviour and interface contract*
> learned from the decompile — ideas and API shapes, which are not copyrightable
> expression. No decompiled code is transcribed into this repo. Our implementation is
> written independently against the **documented** HDV API.

## An aperture is a VID memory-block page-range map

In `vmdevicehost.dll` (named symbols), the public API is a thin wrapper:

```
HdvCreateGuestMemoryAperture
  -> HDV::ExtensibleDevice::CreateGuestMemoryAperture
       -> VidMapMemoryBlockPageRangeEx(memBlock, ..., gpa >> 12, pageCount, prot)
HdvDestroyGuestMemoryAperture
  -> HDV::ExtensibleDevice::DestroyGuestMemoryAperture
       -> VidUnmapMemoryBlockPageRange(memBlock, mappedVa)
```

So an aperture is a **direct map of a guest physical page range into the device-host
process's address space**, via the Virtual Infrastructure Driver (VID). The mapping is
tracked in an SRW-locked red-black tree keyed by mapped VA. The `writeProtected` flag
selects RW vs RO protection. The platform DLL does **pure map/unmap** — it contains *no*
notification, eviction, or invalidation logic (the only `Vid*` calls present are
`VidMapMemoryBlockPageRangeEx` and `VidUnmapMemoryBlockPageRange`).

Consequence: a page that is **already backed** when mapped is coherent with the guest
thereafter (same physical page). A page that is **not yet backed** at map time does **not**
become coherent when the guest later backs it — the existing mapping is stale. This is the
root of our staleness: mapping a large region *early* captures mostly-unbacked pages.

## WSL's strategy: an on-demand per-range cache + a quota-eviction worker

From `wsldevicehost.dll` (the Rust `hyper_v_hdv` crate; unsymbolised, read by behaviour and
by its named imports of the `Hdv*` functions):

- **Not** one giant persistent mapping, and **not** map/unmap-per-access. Instead a **cache
  of apertures**: a sorted array of entries (binary-searched by GPA base), each entry
  holding a mapping plus an **active/inactive** state and refcounts. Apertures are created
  **on demand** for an accessed range and **reused** on subsequent hits.
- A background thread literally named **`HdvGuestMemoryEvictionWorker`** reclaims
  **inactive** cache entries (debug string: *"Inactive aperture not found in cache"*).
- The acquire path is **quota-driven**: when `HdvCreateGuestMemoryAperture` fails with
  **`0x80070718` (`ERROR_NOT_ENOUGH_QUOTA`)**, it logs, **sleeps ~100 ms**, and **retries**
  — relying on the worker to free aperture quota in the background.

**Key conclusion:** eviction exists for **resource management** (VID page-range maps are a
limited quota), **not** as a coherence/​invalidation protocol. There is **no guest-memory
"changed" notification** to subscribe to. Coherence comes from *when* you map: WSL maps each
range **after the guest has backed it** (on first real access) and keeps it cached;
direct VID maps of backed pages stay coherent.

(For completeness: the broker side in the **open** WSL source —
`src/windows/common/DeviceHostProxy.{h,cpp}` — uses `IVmFiovGuestMemoryFastNotification` /
`IVmFiovGuestMmioMappings` via `vmwpctrl.dll!GetVmWorkerProcess` →
`IVmVirtualDeviceAccess::GetDevice`, but only for **doorbell registration** and
**section-backed MMIO**, not for aperture eviction.)

## What this means for `virtio-hdv`

Our original `mem.rs` mapped one persistent `[0, 3 GiB)` aperture on first access and never
refreshed it — so every page backed *after* that moment read stale. The fix, faithful to
WSL and using only documented APIs, is an **on-demand per-range cached aperture**:

- map the **exact accessed range** (page-aligned base, page-rounded length) on demand, so we
  only ever map **backed** pages;
- **cache** by `(base, len)` and reuse on hits (hot ring/descriptor reads become cache hits
  — far less churn than the old map/unmap-per-access experiment that thrashed at ~40%);
- bound the cache with a simple **LRU** count cap, and on `ERROR_NOT_ENOUGH_QUOTA`
  (`0x80070718`) **evict and retry** (synchronously — we don't need a separate worker
  thread at our scale);
- no `Vid*` calls, no closed protocol, no eviction notification.

A residual window remains only if the partition **remaps an already-backed, still-cached**
page (rare for a running, statically-sized VM); WSL has the same exposure and only refreshes
such a range when it is evicted and re-accessed. The interrupt-suppression net
(`RearmNet` in `lib.rs`) stays as belt-and-suspenders.

## Postscript: the bug this RE was chasing wasn't a coherence bug

This investigation set out to find the closed coherence mechanism behind `file_selftest`'s
flakiness. The RE conclusion above is sound — there **is** no closed coherence-notification
protocol; aperture coherence comes from mapping backed pages on demand. But the actual cause of the
flakiness was found later, by **byte-level data-path instrumentation**, to be unrelated to apertures:

- The device replied **`-EIO`** to FUSE (guest: `ls: Invalid argument`) because a descriptor buffer
  at GPA **≈4.04 GiB** was rejected by OpenVMM's `guestmem` *before* the aperture path ran
  (`bad_range=0`, zero aperture failures across ~254 k ops).
- Hyper-V remaps part of guest RAM **above 4 GiB** (32-bit MMIO hole), but `max_address` was set to
  the flat RAM size (`memory_mb · 1 MiB`), so high-RAM buffers exceeded the ceiling. Fixed in
  `mem.rs::ram_size_to_max_gpa` (`max_address = 4 GiB + ram_size`).

The RE work still pays off: it ruled out a phantom "closed eviction notification" we might otherwise
have spent weeks reverse-engineering, and it validated the on-demand cache design as faithful and
sufficient. Lesson: instrument the *data path* and read the bytes before theorising about the
platform.

Refs: `crates/virtio-hdv/src/mem.rs` (`ram_size_to_max_gpa`, on-demand cache),
`crates/virtio-hdv/src/lib.rs` (`RearmNet`), `docs/roadmap.md` ("Guest memory…"), public HDV API at
`learn.microsoft.com/virtualization/api/hcs/reference/hdv`.
