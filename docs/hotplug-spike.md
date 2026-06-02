# Device-hotplug spike — live virtio-fs shares by hot-plugging a device per share

> **Status: spike concluded (2026-06-02).** Bottom line: **device-per-share, N
> concurrent shares hot-added on a running VM — WORKS.** The blocker was using a
> *custom* device class GUID; switching to WSL's well-known `VIRTIO_FS_DEVICE_ID` as
> the `EmulatorId` makes any number of virtio-fs devices coexist (verified: two mount
> concurrently). The one remaining limitation is **hot-remove**: on this Windows build
> `HcsModifyComputeSystem` Remove returns `ERROR_NOT_SUPPORTED` — *exactly* the case
> WSL's own code marks "best effort since not all versions of Windows support it" — so
> a device lingers until VM shutdown. **Strategy B is viable for adding concurrent
> shares; removal is the open question.** See [Net go/no-go (revised)](#net-gono-go-revised).

## Why this spike exists

Atelier runs **one** long-lived utility VM and maps a **new host directory per work
session, live**, while the VM keeps running (the macOS/VZ backend does this today via
`VZMultipleDirectoryShare` + `fsdev.SetShare()` on the running device). To match that
on Windows we need *live, mutable* virtio-fs shares. There are two ways to get there:

- **Strategy A — composite multi-root FUSE wrapper.** One device, one tag; implement
  `fuse::Fuse` as a mutable router over N host dirs presented as `/sessions/<tag>`
  subdirs. Faithful to VZ (guest does nothing on attach/detach), but we own a lot of
  FUSE code — the hard part is **inode-namespace translation** across sub-volumes.
- **Strategy B — hot-plug a virtio-fs *device* per share.** Each session = its own
  `VirtioFsDevice` (single dir, **OpenVMM unchanged**) hot-added/removed over VPCI on
  the running VM. This is the **OpenVMM-endorsed** answer:
  [microsoft/openvmm#861](https://github.com/microsoft/openvmm/issues/861) —
  *"virtiofs uses a device per tag … to add/remove a share after the VM has started you
  will need to dynamically add/remove a device. This is fairly straightforward … if
  you're using the VPCI transport (supported only with Windows hosts)."* VPCI + Windows
  is exactly our transport.

B reuses `VirtioFsDevice` as-is (no FUSE work) and **merges the caller-supplied-GUIDs
roadmap item** (each device needs its own GUID triple). Its risk is *runtime* device
add/remove through our HDV proxy, which we have not yet exercised. This spike settles
that risk.

## What the research already established

Host-side primitives **all exist** (bound, some unused) — see the source survey:

| Primitive | State | Where |
|---|---|---|
| `HcsModifyComputeSystem` (add/remove a `FlexibleIov` slot live) | bound | `hcs-sys/src/lib.rs:75` |
| `RockyVm::modify()` wrapper | bound, **never called** | `hcs-testvm/src/lib.rs:300` |
| post-start `DeviceHost::from_proxy` + `PciDevice::create` | works (proxy lives on IPC, not the system handle) | `hdv/src/lib.rs:94`, `hdv/src/pci.rs:336` |
| guest stays alive after boot | yes (`sleep 600`) | `E:\dev\spike\init:54` |
| per-device destroy (`HdvDestroyDeviceInstance`) | **missing** — teardown is host-level only (`HdvTeardownDeviceHost` via `DeviceHost::Drop`) | `hdv-sys`, `hdv/src/lib.rs:117` |

The one *recorded* reason runtime hot-add was abandoned in `attach_proxy.rs` is a
**timing race** — "the hot path raced the fast-exiting initramfs init" — not an HCS
failure. `docs/hdv-proxy-abi.md:94` states hot-add and cold-declare **both** succeed
once the device host is proxy-registered. The current spike guest now **stays alive**,
so that race no longer applies.

**Net:** the *add* half is very likely cheap. The *remove* half (no per-device destroy
API; surprise VPCI eject in the guest) is the genuine unknown and the reason Stage 3 is
the pivot.

## Design levers

- **One device host per device.** `PciDevice::create` consumes a `DeviceHost` by value,
  and teardown today is host-level. So giving each device its *own* device host makes
  per-device teardown fall out *for free* (drop that one `PciDevice` → `HdvTeardownDeviceHost`
  for only that host). This turns the "no per-device destroy" gap from a blocker into a
  non-issue — *if* tearing down one host doesn't disturb the others (Stage 3 tests this).
- **Slot strategy.** Prefer runtime `HcsModifyComputeSystem` **Add** (`ResourcePath
  VirtualMachine/Devices/FlexibleIov/<DeviceInstanceId>`, Settings `{EmulatorId,
  HostingModel: ExternalRestricted}`). Fallback if runtime-Add proves flaky: declare a
  **cold pool** of N empty slots at boot and hot-create devices into pooled slots.
- **Distinct GUIDs per device.** Two concurrent devices need two distinct
  `DeviceInstanceId`s (the slot map-key) and two device-host ids; the `EmulatorId`
  (class) can stay the virtio-fs constant. Stage 2 uses two hardcoded GUID triples;
  full caller-GUID plumbing is deferred.
- **Guest mount layout.** Each device's tag mounts at `/sessions/<tag>` (the atelier
  contract). Under B the guest reacts to hotplug per device, unlike VZ's single-mount
  multi-root — but the `/sessions/<tag>` paths the runner expects are preserved.

## Stages (each a go/no-go gate)

### Stage 1 — hot-add one device after start *(crux of feasibility)*
- **1a:** slot declared cold; device host registered + device created **after**
  `vm.start()`. Isolates "create device post-start".
- **1b:** no cold slot; after the guest signals ready, `vm.modify()` **Add** the slot,
  then `from_proxy` + attach. Tests runtime slot-Add with a long-lived guest.
- **Gate:** guest prints `HOTPLUG_MOUNT_PASS tag=<tag>`. If 1a passes and 1b doesn't →
  adopt the cold-pool fallback.

### Stage 2 — two concurrent devices *(multi-share core)*
- Hot-add a second device (distinct tag + `DeviceInstanceId` + host id). Guest mounts
  both `/sessions/<tag1>` and `/sessions/<tag2>` at once.
- **Gate:** both mounts live simultaneously. Confirms distinct-GUID coexistence (the
  caller-GUIDs merge).

### Stage 3 — hot-remove one device *(A-vs-B decision pivot)*
- Guest unmounts `/sessions/<tag1>`; host drops device #1's `PciDevice`/host and
  `HcsModifyComputeSystem` **Remove**s its slot, while device #2 stays mounted.
- **Gate:** tag1's mount vanishes, tag2 still reads/writes, no guest crash on eject.
- **Outcome:** clean → commit to **B** (and fold in caller-GUIDs). Intractable → fall
  back to **A** (multi-root FUSE wrapper).

## Test surface

- Host: `crates/hcs-testvm/tests/hotplug.rs` (`#[ignore]`, Hyper-V + Rocky artifacts;
  same env vars + 4-attempt aperture-staleness retry as `attach_virtiofs.rs`).
- Guest: `E:\dev\spike\init` extended to print `GUEST_READY`, then poll
  `/sys/bus/virtio/devices/*/tag` for hot-added virtio-fs devices, mount each at
  `/sessions/<tag>`, and print `HOTPLUG_MOUNT_PASS` / `HOTPLUG_REMOVE_OK`. Backward
  compatible: the legacy boot-time `ws` mount + `PROOF_COMPLETE_PASS` is preserved so
  `attach_virtiofs`/`attach_abi`/`attach_proxy` stay green. Rebuild via
  `E:\dev\spike\build-rocky-initramfs.sh` (Docker/WSL).

## Results & findings

**Stage 1 — hot-add one device: PASS (attempt 1).**
`crates/hcs-testvm/tests/hotplug.rs::guest_hot_mounts_added_device`. With the device
host proxy-registered pre-start, then **after** `GUEST_READY`: `VirtioHdvDevice::attach`
on the running partition + `HcsModifyComputeSystem` **Add** of the `FlexibleIov` slot
→ the guest hot-detected the VPCI device and printed `HOTPLUG_MOUNT_PASS tag=hp1`
(`register_hr=0x0`, mount within seconds). **Runtime hot-add works** — the
`attach_proxy.rs` "race" really was only the fast-exiting initramfs, now gone. This is
the core of Strategy B, and it is feasible.

**Stage 2 — two device hosts: REJECTED, design reshaped.**
Registering a **second** device host on the same VM fails: the second
`DeviceHost::from_proxy` returns **HRESULT `0xC0370030`** (facility `0x37` =
FACILITY_USERMODE_VIRTUALIZATION / the VID; cf. `0xC0370029 =
ERROR_VID_SAVED_STATE_INCOMPATIBLE`). So the **one-host-per-device** lever is invalid.
This agrees with WSL's real architecture: a *single* `wsldevicehost` carries multiple
FlexibleIov emulators (`virtiofs`/`virtio_net`/`virtio_pmem`, `docs/hdv-proxy-abi.md`).

**Corrected model: one device host per VM, hosting N devices.** Each share is still
its own `VirtioFsDevice`/instance id/slot — but all are created on **one** shared
`DeviceHost`. Two consequences:
1. **Sharing the host.** `PciDevice`/`VirtioHdvDevice` currently *own* the host
   (`PciDevice { host: DeviceHost, … }`) and consume it per `create`. Multi-device
   needs the host shared (an `Arc<DeviceHost>` or a borrow-create that returns just the
   per-device handle), without changing the shipped single-device `attach` path
   (`attach_virtiofs`/`attach_abi`/`hvfs_attach` must stay green).
2. **Per-device removal needs `HdvDestroyDeviceInstance`.** With a shared host, Stage
   3 can no longer "drop the host to remove one device" — that tears down *all* of
   them. `hdv-sys` does **not** bind a per-device destroy yet; it must be added (it is
   in `vmdevicehost.dll`; teardown today is host-level only).

What already landed for this (kept — it's the per-device GUID plumbing B needs):
`PciDevice::create_with_instance` and `VirtioHdvDevice::attach_with_instance` thread a
caller-chosen `DeviceInstanceId` (class + host id still fixed — the tracked
caller-GUIDs follow-up), and `hdv` now re-exports `GUID`.

**Shared-host refactor — DONE and works.** `PciDevice` now holds an `Arc<DeviceHost>`;
`PciDevice::create_shared` / `VirtioHdvDevice::attach_shared` create a device on a
shared host (class + instance id parameterised). A single device created on the shared
`Arc<DeviceHost>` mounts fine (Stage 1 reconfirmed through the shared path). The shipped
single-device `attach`/`attach_abi`/`hvfs_attach` path is untouched and still compiles.

**Stage 2 — N concurrent FlexibleIov devices: SOLVED.** First attempts failed —
adding a second device returned `0xC0350005 = ERROR_HV_INVALID_PARAMETER` at device
power-on (runtime) / start (cold), even with a distinct custom class id. The HCS
`FlexibleIoDevice` schema (microsoft/hcsshim `schema2/flexible_io_device.go`) has only
`{EmulatorId, HostingModel, Configuration[]}` — no placement field — and our apertures
are lazy (ruled out). The actual fix came from **WSL's source** (`E:\dev\WSL`,
`GuestDeviceManager.h` + `DeviceHostProxy::AddNewDevice`): WSL uses the **well-known
`VIRTIO_FS_DEVICE_ID = 872270E1-A899-4AF6-B454-7193634435AD`** as the `EmulatorId`
(== HDV `DeviceClassId`) for **every** virtio-fs device, distinguished only by a unique
per-device instance GUID. A *custom* class GUID works for one device but the VID won't
power on a second; the **recognized** device-type id is what lets N coexist.

With that change, **two virtio-fs devices mount concurrently** on one running VM
(`guest_hot_mounts_two_devices_concurrently`, PASS): each enumerates its own VMBus VPCI
bus → `1af4:105a` → `virtiofs virtio0`/`virtio1` → `/sessions/hp1` + `/sessions/hp2`.
This is exactly WSL's model: **one device host, N devices, all `EmulatorId =
VIRTIO_FS_DEVICE_ID`**, hot-added at runtime via `HcsModifyComputeSystem` Add.

**Stage 3 — hot-remove: best-effort, UNSUPPORTED (confirmed across the support matrix).**
`HcsModifyComputeSystem` **Remove** of a FlexibleIov slot returns `0x80070032 =
ERROR_NOT_SUPPORTED` — matching WSL's request shape exactly (`Settings` included). This
is **not our bug**: WSL's own `DeviceHostProxy::RemoveDevice` wraps the Remove in
`try/CATCH_LOG` — *"Removing the FlexIov device is best effort since not all versions of
Windows support it."*

**Windows support-matrix check (the obvious lever was the schema version):** WSL selects
`SchemaVersion {2,7}` on Windows 11 vs `{2,3}` on Windows 10
(`HcsVirtualMachine.cpp` `IsWindows11OrAbove`); we were sending `{2,1}`. Bumping our doc
to **`{2,7}` was tested on this Win11 build (26200) — create/boot/two-device-mount all
still work, but Remove still returns `ERROR_NOT_SUPPORTED`.** So the schema version is
**not** the removal gate. We **keep `{2,7}`** anyway (it's WSL's Win11 value and the
correct, future-proof schema for a Win11 host). WSL's `WslCoreVm.cpp` even carries *"if hot
remove of pmem devices is ever added, this logic will need to be updated"* — i.e.
hot-remove of these FlexibleIov-class devices is **not a current platform capability**,
not merely a version flag we're missing. There is also no per-device HDV destroy export
(`HdvDestroyDeviceInstance` does not exist; the bound lifecycle is create +
host-teardown).

**How WSL actually copes (the real teardown model):** on Remove failure it just *forgets*
the device — `m_devices.erase` + releasing its COM references — and the device lingers in
the VM until shutdown, when a kill-on-close **job object** terminates the device-host
process. So even WSL does **not** truly hot-remove on these builds; it releases host-side
references and reclaims everything at VM teardown. That is the pattern atelier should copy
(option 1 below).

## Net go/no-go (revised)

- **Adding concurrent shares (device-per-share): PROVEN.** N virtio-fs devices hot-add
  and coexist on one running VM, using `VIRTIO_FS_DEVICE_ID` + per-device instance ids,
  on one shared device host. This is the make-or-break for B, and it works — it's the
  exact mechanism WSL ships.
- **Removing a share live: blocked on this Windows build** (`ERROR_NOT_SUPPORTED`, a
  platform limitation WSL also hits). A device lingers until the VM is recycled.

So **Strategy B is viable for the add path atelier needs**, with one caveat to design
around: **no reliable live removal**. What shipped copes by reclaim-at-recycle (the DLL frees
host-side resources; the guest device lingers until the VM is torn down). The open follow-ups this
leaves — confirming removal across the Windows support matrix, and the recycle policy — are tracked
in [`roadmap.md`](roadmap.md), not here; this doc is the spike record.

This is materially better than the earlier "multi-device blocked" reading: concurrency —
the thing atelier fundamentally needs — **works**. Only the teardown is constrained, and
the same constraint exists in WSL.

## Honest risk ledger

- **Hot-remove** is the real unknown (Stage 3). Mitigated by one-host-per-device +
  guest-unmounts-first.
- **Runtime slot-Add (1b)** could still fail for a non-timing reason → cold-pool
  fallback.
- **Per-device device-host overhead** — fine at atelier's scale (`ATELIER_MAX_ACTIVE`
  default 3).
- **Tag discovery** — `/sys/bus/virtio/devices/*/tag` may lag driver bind; the poll
  loop retries, with a known-tag fallback if the node never appears.
- Windows/VPCI only (our target). Aperture-cache staleness retry still applies per
  device.
