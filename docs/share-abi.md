# Share lifecycle ABI (v2) — device-per-share, honest to HDV/HCS

Status: **design / proposed.** Supersedes the declarative `hvfs_set_shares` sketch (ABI v1),
which assumed a mutable directory map. The hotplug spike (`docs/hotplug-spike.md`) proved that
model impossible: OpenVMM's `VirtioFs` is single-root and immutable, so a live share is a *new
device*, not a map entry. This ABI exposes that reality directly.

## Design stance

This is a **general, reusable** C ABI for attaching virtio-fs shares to an externally-owned
HCS/Hyper-V compute system over the HDV ExternalRestricted proxy path. It is **not** shaped for any
one consumer. It does **not** imitate the macOS/VZ `SetShare` semantics, and it does **not** hide the
platform's quirks behind a tidy abstraction. It names the real objects of the stack — a **device
host** carrying **N virtio-fs devices** — and reports what the platform actually does, including its
refusals. Any declarative "desired set of shares" reconciliation, or policy about when to recycle a
VM to reclaim removed devices, is the **caller's** job (for atelier, that's the `atelierd` Go
adapter), not the DLL's.

## What the stack actually allows (from the spike — load-bearing facts)

1. **One device host per compute system.** A second `HdvInitializeDeviceHostForProxy` on the same VM
   fails with `0xC0370030` (VID). So there is exactly one host; every device rides it (a shared
   `Arc<DeviceHost>`). This mirrors WSL's single `wsldevicehost`.
2. **Every virtio-fs device uses the *well-known* class id** `872270E1-A899-4AF6-B454-7193634435AD`
   (WSL's `VIRTIO_FS_DEVICE_ID`) as its `EmulatorId` / HDV `DeviceClassId`. A *custom* class id works
   for the first device but the VID rejects the second with `ERROR_HV_INVALID_PARAMETER`
   (`0xC0350005`). So the class is **not caller-choosable** — it is virtio-fs's type id by platform
   necessity. Devices differ only by a **unique instance id**.
3. **Add is runtime.** A device is hot-added to a *running* system via `HcsModifyComputeSystem`
   (`RequestType: "Add"`, `ResourcePath: VirtualMachine/Devices/FlexibleIov/<instanceId>`,
   `Settings: { EmulatorId, HostingModel: "ExternalRestricted" }`). The guest hot-detects and mounts
   it. Proven for 1 and 2 concurrent devices.
4. **Remove is best-effort and currently unsupported.** `HcsModifyComputeSystem` `Remove` returns
   `ERROR_NOT_SUPPORTED` (`0x80070032`) on Win11 26200, independent of `SchemaVersion` ({2,7}
   tested). There is **no** `HdvDestroyDeviceInstance` export. WSL hits the same wall and copes by
   *forgetting* the device host-side and reclaiming it when the VM shuts down (kill-on-close job
   object). **Teardown is therefore reclaim-at-recycle**, not live removal.
5. **The device host registers before start.** `from_proxy` is done at VM bringup (as WSL does);
   shares are added after the guest is up.

## Object model

```
hvfs_host    one per compute system  — owns: HCS_SYSTEM handle, IVmDeviceHostSupport,
             (registered pre-start)     Arc<DeviceHost>, the system RAM size, a registry of
                                        its still-open shares.
   │
   ├── hvfs_share   one per share/device — owns: the virtio-fs HDV device (PciDevice), a clone of
   │   (hot-added,    the host's Arc<DeviceHost>, its instance-id GUID string; borrows the host's
   │    post-start)   HCS_SYSTEM handle to issue its own Remove.
   ├── hvfs_share
   └── …
```

A `hvfs_share` **borrows** its `hvfs_host`: the host must outlive every share opened on it.

## Lifecycle / call ordering (the honest contract)

```
1. caller creates the HCS compute system   (with NO FlexibleIov slots — none are pre-declared)
2. hvfs_host_open(system_id, {memory_mb})   → register the device host   [BEFORE start]
3. caller starts the compute system
4. hvfs_add_share(host, {tag,path,…})  × N  → hot-add a device per share [AFTER start]
5. hvfs_remove_share(share)                 → best-effort; may report UNSUPPORTED
6. hvfs_host_close(host)                     → reclaim every still-open share, then the host
```

`hvfs_host_close` reclaims any shares still open; **share handles are invalid after their host is
closed.** Per-device `Remove` not working is *expected* — the real reclamation happens when the
caller tears the compute system down (which closing the host, then stopping/deleting the system,
accomplishes).

## Proposed C ABI (v2 — breaking; `HVFS_ABI_VERSION` 1 → 2)

```c
#define HVFS_ABI_VERSION 2

#define HVFS_OK                  0
#define HVFS_ERR_INVALID_ARG    -1
#define HVFS_ERR_NOT_IMPLEMENTED -2
#define HVFS_ERR_PANIC          -3
#define HVFS_ERR_HDV            -4
#define HVFS_ERR_UNSUPPORTED    -5   /* the platform refused (e.g. live device Remove) — not a bug */

typedef struct hvfs_host  hvfs_host;    /* one device host, registered against a compute system */
typedef struct hvfs_share hvfs_share;   /* one virtio-fs device == one shared directory          */

uint32_t hvfs_abi_version(void);

/* Register an HDV device host against an already-created compute system, by id.
 * MUST be called BEFORE the system is started. host_json: { "memory_mb": <u32> }
 * — the compute system's RAM; the GPA ceiling each device's virtqueues may reference. */
int32_t hvfs_host_open(const char *hcs_system_id, const char *host_json, hvfs_host **out);

/* Hot-add one virtio-fs device (== one share) to the RUNNING compute system.
 * share_json: { "tag": "ws", "path": "C:\\host\\dir", "instance_id": "<guid>", "ro": false }
 * - tag         : virtio-fs mount tag the guest uses (mount -t virtiofs <tag> …)
 * - path        : host directory to share
 * - instance_id : REQUIRED; the device's unique FlexibleIov DeviceInstanceId (GUID string). The
 *                 caller owns uniqueness across the compute system. The device CLASS is NOT
 *                 caller-choosable — it is virtio-fs's well-known type id by platform necessity.
 * - ro          : optional (default false). ro:true currently returns HVFS_ERR_NOT_IMPLEMENTED
 *                 (the FUSE backend does not yet enforce read-only). */
int32_t hvfs_add_share(hvfs_host *host, const char *share_json, hvfs_share **out);

/* The share's on-wire identity: its FlexibleIov DeviceInstanceId (GUID string). Borrowed,
 * valid for the share's lifetime, never freed by the caller. Lets the caller correlate the
 * handle with the guest's PCI device. */
const char *hvfs_share_instance_id(const hvfs_share *share);

/* Best-effort live remove + host-side teardown of one share. Returns HVFS_ERR_UNSUPPORTED when
 * the platform refuses the live Remove (current Windows): the guest-visible device then persists
 * until the compute system is torn down (reclaim-at-recycle), though host-side resources are
 * released now. Frees the share handle on HVFS_OK and on HVFS_ERR_UNSUPPORTED. */
int32_t hvfs_remove_share(hvfs_share *share);

/* Tear down every remaining device, then the device host, then close the system handle.
 * Invalidates all share handles opened on this host. */
int32_t hvfs_host_close(hvfs_host *host);

const char *hvfs_last_error(void);
void        hvfs_set_logger(hvfs_log_fn cb, void *ctx);
```

### What changes vs v1
- **Removed:** `hvfs_attach`, `hvfs_set_shares`, `hvfs_detach` (the device-bundled-in-attach +
  mutable-map model). **Added:** `hvfs_host_open`, `hvfs_add_share`, `hvfs_share_instance_id`,
  `hvfs_remove_share`, `hvfs_host_close`, and `HVFS_ERR_UNSUPPORTED`.
- `HVFS_ERR_UNSUPPORTED` is **distinct** from `HVFS_ERR_NOT_IMPLEMENTED`: the latter means "we
  haven't built it"; the former means "the platform said no." Live Remove is the latter case.
- The old "caller-supplied GUIDs" follow-up is **partly resolved**: the device *class* can't be
  caller-chosen (platform), and the *instance* id is caller-optional here.

## Implementation outline

- **`crates/hdv/src/pci.rs`** — add the well-known class constant
  `VIRTIO_FS_DEVICE_CLASS_ID = 872270E1-…`; make it the default class for `attach_shared`. Keep
  `HVFS_DEVICE_HOST_ID` (our device-host id; separate from the device class). The custom
  `HVFS_DEVICE_CLASS_ID` becomes legacy (only the single-device cold tests use it).
- **`crates/hyperv_virtiofs/src/lib.rs`** — rewrite the surface:
  - `hvfs_host` { `OwnedSystem`, `DeviceHostSupport`, `Arc<DeviceHost>`, `memory_bytes`,
    `Mutex<Vec<*mut hvfs_share>>` registry }. `Send` justified as today.
  - `hvfs_share` { `VirtioHdvDevice`, `Arc<DeviceHost>` (clone), `instance_id: CString`,
    borrowed `HCS_SYSTEM` for its Remove }.
  - `add_share`: parse → instance GUID (caller or generated) → `VirtioHdvDevice::attach_shared`
    (well-known class) → build the Add `ModifySettingRequest` → `HcsModifyComputeSystem` → register
    in the host's slab → `Box::into_raw`.
  - `remove_share`: build the Remove request → `HcsModifyComputeSystem`; map `0x80070032` →
    `HVFS_ERR_UNSUPPORTED` (still free the handle + drop host-side device); de-register from the slab.
  - `host_close`: drain the slab (Box::from_raw + drop each share), then drop host.
  - A small private `modify(system, json) -> HRESULT` helper over `hcs-sys` (the test rig's
    `RockyVm::modify` pattern, but the DLL owns its own).
- **Instance-GUID generation** — `UuidCreate` (rpcrt4) binding, or accept caller-supplied only at
  first. *(decision below.)*
- **Header** — `cbindgen` regen, commit; bump `HVFS_ABI_VERSION` to 2.
- **Test** — replace/extend `attach_abi.rs`: drive the real exports end-to-end through a booted VM
  — `host_open` (pre-start) → start → `add_share` ×2 (assert both mount) → `remove_share`
  (assert `HVFS_OK` *or* `HVFS_ERR_UNSUPPORTED`, both green) → `host_close`. `#[ignore]`, 4-attempt
  aperture retry, mirrors `hotplug.rs`.
- **Docs** — README ABI section + roadmap; `atelier/docs/plans/windows-virtiofs-hdv.md` pointer.

## Resolved decisions
1. **Instance-GUID generation: caller-supplied only.** `instance_id` is **required** in
   `share_json`; the DLL never generates one (no `UuidCreate` dependency). Maximally explicit —
   uniqueness across a compute system's shares is the caller's contract (a collision is rejected by
   the VID). A missing/malformed `instance_id` → `HVFS_ERR_INVALID_ARG`.
2. **`ro` enforcement: honest-refuse.** `ro: true` returns `HVFS_ERR_NOT_IMPLEMENTED` until the FUSE
   backend actually enforces read-only — never claim a guarantee we don't deliver. `ro: false` (or
   omitted) works normally. When the backend honors `ro`, enforce it with no ABI change.
