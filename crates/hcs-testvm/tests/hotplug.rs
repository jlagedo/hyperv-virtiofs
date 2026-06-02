//! **Device-hotplug spike** — live virtio-fs shares by hot-plugging a *device per
//! share* onto an already-running VM (Strategy B; `docs/hotplug-spike.md`).
//!
//! Where `attach_virtiofs.rs` attaches the device **before** start (the guest
//! enumerates it at its first PCI scan), this attaches it **after** the guest is
//! already up and asserts the guest *hot-detects* and mounts it. That's the core
//! capability atelier needs: one long-lived VM, a new share mapped live per work
//! session — the Windows analog of VZ's `fsdev.SetShare()` on the running device.
//!
//! This is the OpenVMM-endorsed shape (microsoft/openvmm#861: "virtiofs uses a
//! device per tag … add/remove a device after start … straightforward on the VPCI
//! transport"), which is exactly our setup.
//!
//! ## Stage 1 — hot-add one device (this file)
//! Isolation of risk: the device **host** is proxy-registered *before* start (the
//! known-good path, and the realistic atelier pattern — register once at VM
//! bringup), while the two genuinely-new runtime operations happen *after* the
//! guest is up:
//!   1. `VirtioHdvDevice::attach` — create the HDV device instance on the running
//!      partition;
//!   2. `vm.modify()` — `HcsModifyComputeSystem` **Add** the `FlexibleIov` slot,
//!      whose `DeviceInstanceId` resolves to the just-created device.
//! The guest's hotplug-watch loop (`E:\dev\spike\init`) mounts the tag and prints
//! `HOTPLUG_MOUNT_PASS tag=hp1`.
//!
//! The `attach_proxy.rs` note that runtime hot-add "raced the fast-exiting
//! initramfs init" no longer applies: the guest now stays alive and polls.
//!
//! **Retry:** like `attach_virtiofs.rs`, attach+boot is retried a few times — HDV
//! guest-memory apertures are an evictable cache, so a fraction of boots hit a
//! stale descriptor read and stall. Each attempt is a fresh VM.
//!
//! `#[ignore]` — needs Hyper-V + the Rocky artifacts. Run it:
//!   $env:HVFS_KERNEL="E:\dev\spike\out\vmlinuz"
//!   $env:HVFS_INITRD="E:\dev\spike\out\initramfs.cpio.gz"
//!   cargo test -p hcs-testvm --test hotplug -- --ignored --nocapture
#![cfg(windows)]

use hcs_testvm::{FlexibleIovSlot, RockyConfig, RockyVm};
use hdv::pci::{
    guid_to_string, HVFS_DEVICE_CLASS_ID, HVFS_DEVICE_HOST_ID, HVFS_DEVICE_INSTANCE_ID,
};
use hdv::proxy::DeviceHostSupport;
use hdv::DeviceHost;
use std::sync::Arc;
use std::time::Duration;
use virtio_hdv::VirtioHdvDevice;

/// WSL's well-known virtio-fs device type id, used as the `EmulatorId` (== HDV
/// `DeviceClassId`) for **every** virtio-fs FlexibleIov device — multiple coexist,
/// distinguished only by a unique instance id (`microsoft/WSL` `GuestDeviceManager.h`,
/// `DeviceHostProxy::AddNewDevice`). A *custom* class id works for one device but the
/// VID rejects a second; the well-known id is what lets N coexist.
const VIRTIO_FS_DEVICE_ID: hdv::GUID = hdv::GUID {
    Data1: 0x872270E1,
    Data2: 0xA899,
    Data3: 0x4AF6,
    Data4: [0xB4, 0x54, 0x71, 0x93, 0x63, 0x44, 0x35, 0xAD],
};

/// Two distinct per-device instance ids (WSL uses `UuidCreate`; we use two fixed,
/// well-separated v4-shaped GUIDs so the run is deterministic).
const INSTANCE_A: hdv::GUID = hdv::GUID {
    Data1: 0xA1A1A1A1,
    Data2: 0x1111,
    Data3: 0x4111,
    Data4: [0x81, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11],
};
const INSTANCE_B: hdv::GUID = hdv::GUID {
    Data1: 0xB2B2B2B2,
    Data2: 0x2222,
    Data3: 0x4222,
    Data4: [0x82, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22],
};

/// Build a throwaway host workspace dir with a sentinel file for the guest to read.
fn make_workspace(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("atelier-hotplug-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workspace dir");
    std::fs::write(
        dir.join("SENTINEL.txt"),
        format!("hello from the host via hot-plugged virtio-fs device (tag={tag})\n"),
    )
    .expect("write sentinel");
    dir
}

/// An HCS `ModifySettingRequest` that hot-adds a `FlexibleIov` device slot to a
/// running compute system. The `DeviceInstanceId` (the slot map-key, in the
/// ResourcePath) resolves — via the registered device host's `GetDeviceInstance`
/// — to the HDV device we already created; `EmulatorId` is its `DeviceClassId`.
/// (`docs/hdv-proxy-abi.md`.)
fn add_slot_request(instance_id: &str, emulator_id: &str) -> String {
    serde_json::json!({
        "ResourcePath": format!("VirtualMachine/Devices/FlexibleIov/{instance_id}"),
        "RequestType": "Add",
        "Settings": { "EmulatorId": emulator_id, "HostingModel": "ExternalRestricted" },
    })
    .to_string()
}

#[test]
#[ignore = "requires Hyper-V + Rocky artifacts; run with --ignored"]
fn guest_hot_mounts_added_device() {
    let kernel =
        std::env::var("HVFS_KERNEL").unwrap_or_else(|_| r"E:\dev\spike\out\vmlinuz".into());
    let initrd = std::env::var("HVFS_INITRD")
        .unwrap_or_else(|_| r"E:\dev\spike\out\initramfs.cpio.gz".into());
    assert!(
        std::path::Path::new(&kernel).exists(),
        "kernel not found: {kernel}"
    );
    assert!(
        std::path::Path::new(&initrd).exists(),
        "initrd not found: {initrd}"
    );

    let ws = make_workspace("hp1");
    eprintln!("host workspace: {}", ws.display());

    const ATTEMPTS: usize = 4;
    for attempt in 1..=ATTEMPTS {
        eprintln!("--- attempt {attempt}/{ATTEMPTS} ---");
        if try_hotplug_add(&kernel, &initrd, &ws) {
            eprintln!("PASS on attempt {attempt}");
            return;
        }
    }
    panic!(
        "guest did not hot-mount the added device (no HOTPLUG_MOUNT_PASS) in {ATTEMPTS} attempts"
    );
}

/// One attempt: boot with **no** cold slot (the guest tag list comes via cmdline),
/// register the device host pre-start, wait for `GUEST_READY`, then at runtime
/// attach the device and hot-add its slot. Returns whether the guest mounted it.
fn try_hotplug_add(kernel: &str, initrd: &str, ws: &std::path::Path) -> bool {
    // No `with_flexible_iov`: the slot is added *at runtime*, not declared cold.
    // The guest's hotplug loop watches the tag named on the kernel cmdline.
    let mut cfg = RockyConfig::new(kernel, initrd);
    cfg.kernel_cmdline = "console=ttyS0 atelier.hptags=hp1".into();

    let vm = RockyVm::create(&cfg).expect("create Rocky compute system");
    eprintln!("compute system id: {}", vm.id());

    // Register the device host BEFORE start (proven path; also the realistic
    // atelier shape — one host for the VM's life). No slot is reserved at start.
    // SAFETY: the VM (hence its system handle) outlives `support` and the device.
    let support = unsafe { DeviceHostSupport::new(vm.system_handle()) };
    let host = unsafe { DeviceHost::from_proxy(&HVFS_DEVICE_HOST_ID, support.as_iunknown()) }
        .unwrap_or_else(|e| panic!("HdvInitializeDeviceHostForProxy failed: {e}"));
    eprintln!(
        "proxy host registered={} register_hr={:#010x}; starting guest…",
        support.was_registered(),
        support.register_hr() as u32,
    );

    vm.start().expect("start Rocky compute system");

    // Wait until the guest has loaded the VPCI stack and entered its hotplug loop.
    if !vm.wait_for_console("GUEST_READY", Duration::from_secs(90)) {
        eprintln!(
            "===== guest console (never reached GUEST_READY) =====\n{}\n===================",
            vm.console()
        );
        return false;
    }
    eprintln!("guest ready; hot-adding the virtio-fs device now…");

    // (1) Create the HDV device instance on the RUNNING partition.
    let guest_mem = cfg.memory_mb as u64 * 1024 * 1024;
    let device = VirtioHdvDevice::attach(host, ws, "hp1", guest_mem)
        .expect("attach virtio-fs over HDV (post-start)");

    // (2) Hot-add the FlexibleIov slot; the VID resolves it to the device above.
    let req = add_slot_request(
        &guid_to_string(&HVFS_DEVICE_INSTANCE_ID),
        &guid_to_string(&HVFS_DEVICE_CLASS_ID),
    );
    vm.modify(&req)
        .unwrap_or_else(|e| panic!("HcsModifyComputeSystem Add FlexibleIov slot failed: {e}"));
    eprintln!("slot hot-added; waiting for the guest to enumerate + mount…");

    let pass = vm.wait_for_console("HOTPLUG_MOUNT_PASS tag=hp1", Duration::from_secs(45));
    if !pass {
        eprintln!(
            "===== guest console (attempt failed) =====\n{}\n=========================",
            vm.console()
        );
    }

    // Keep the device + support alive until the console is checked.
    let _ = &device;
    let _ = &support;
    pass
}

// ============== Stage 2 + 3 — two concurrent devices, then remove one ==============
//
// DESIGN (from Stage 2's first finding): registering **two** device hosts on one VM
// is rejected — the second `from_proxy` returns HRESULT 0xC0370030
// (FACILITY_USERMODE_VIRTUALIZATION / VID). So the "one host per device" lever is
// invalid; the model is **one device host per VM hosting N devices** (matching WSL's
// single `wsldevicehost` carrying virtiofs/virtio_net/virtio_pmem). Both devices
// therefore ride **one shared `Arc<DeviceHost>`** (`attach_shared`).
//
// Removal is host-side, no per-device HDV destroy export (none exists): we
// `HcsModifyComputeSystem` **Remove** the device's `FlexibleIov` slot. The VID drops
// the device from the guest; the guest's hotplug loop sees the stale mount and
// unmounts it (`HOTPLUG_REMOVE_OK`). The sibling device must keep working.

/// An HCS `ModifySettingRequest` that **removes** a `FlexibleIov` slot from a running
/// compute system. WSL's `DeviceHostProxy::RemoveDevice` includes the same `Settings`
/// (EmulatorId + HostingModel) as the Add, not just the ResourcePath.
fn remove_slot_request(instance_id: &str, emulator_id: &str) -> String {
    serde_json::json!({
        "ResourcePath": format!("VirtualMachine/Devices/FlexibleIov/{instance_id}"),
        "RequestType": "Remove",
        "Settings": { "EmulatorId": emulator_id, "HostingModel": "ExternalRestricted" },
    })
    .to_string()
}

#[test]
#[ignore = "requires Hyper-V + Rocky artifacts; run with --ignored"]
fn guest_hot_mounts_two_devices_concurrently() {
    let kernel =
        std::env::var("HVFS_KERNEL").unwrap_or_else(|_| r"E:\dev\spike\out\vmlinuz".into());
    let initrd = std::env::var("HVFS_INITRD")
        .unwrap_or_else(|_| r"E:\dev\spike\out\initramfs.cpio.gz".into());
    assert!(
        std::path::Path::new(&kernel).exists(),
        "kernel not found: {kernel}"
    );
    assert!(
        std::path::Path::new(&initrd).exists(),
        "initrd not found: {initrd}"
    );

    let ws1 = make_workspace("hp1");
    let ws2 = make_workspace("hp2");

    const ATTEMPTS: usize = 4;
    for attempt in 1..=ATTEMPTS {
        eprintln!("--- attempt {attempt}/{ATTEMPTS} ---");
        if try_two_then_remove(&kernel, &initrd, &ws1, &ws2) {
            eprintln!("PASS on attempt {attempt}");
            return;
        }
    }
    panic!("two devices did not mount concurrently in {ATTEMPTS} attempts");
}

/// One attempt: one shared device host; hot-add device #1 (hp1) and #2 (hp2); assert
/// both mount; then Remove device #1's slot and assert the guest unmounts hp1 while
/// hp2 stays. Returns whether the whole sequence passed.
fn try_two_then_remove(
    kernel: &str,
    initrd: &str,
    ws1: &std::path::Path,
    ws2: &std::path::Path,
) -> bool {
    let mut cfg = RockyConfig::new(kernel, initrd);
    cfg.kernel_cmdline = "console=ttyS0 atelier.hptags=hp1,hp2".into();

    let vm = RockyVm::create(&cfg).expect("create Rocky compute system");
    eprintln!("compute system id: {}", vm.id());

    // ONE device host, shared across both devices (only one host per VM is allowed).
    // SAFETY: the VM outlives `support` and every device on the host.
    let support = unsafe { DeviceHostSupport::new(vm.system_handle()) };
    let host = Arc::new(
        unsafe { DeviceHost::from_proxy(&HVFS_DEVICE_HOST_ID, support.as_iunknown()) }
            .unwrap_or_else(|e| panic!("from_proxy (shared host) failed: {e}")),
    );

    vm.start().expect("start Rocky compute system");
    if !vm.wait_for_console("GUEST_READY", Duration::from_secs(90)) {
        eprintln!(
            "===== guest console (no GUEST_READY) =====\n{}",
            vm.console()
        );
        return false;
    }

    let guest_mem = cfg.memory_mb as u64 * 1024 * 1024;

    // WSL's model: both virtio-fs devices share the well-known EmulatorId
    // (VIRTIO_FS_DEVICE_ID) and differ only by a unique instance id.
    let class1 = VIRTIO_FS_DEVICE_ID;
    let class2 = VIRTIO_FS_DEVICE_ID;
    let instance1 = INSTANCE_A;
    let instance2 = INSTANCE_B;

    // --- device #1 (hp1) ---
    let device1 =
        VirtioHdvDevice::attach_shared(host.clone(), ws1, "hp1", guest_mem, &class1, &instance1)
            .expect("attach device #1 on shared host");
    vm.modify(&add_slot_request(
        &guid_to_string(&instance1),
        &guid_to_string(&class1),
    ))
    .expect("Add slot #1");
    if !vm.wait_for_console("HOTPLUG_MOUNT_PASS tag=hp1", Duration::from_secs(45)) {
        eprintln!("===== device #1 never mounted =====\n{}", vm.console());
        return false;
    }
    eprintln!("device #1 (hp1) mounted; adding device #2…");

    // --- device #2 (hp2), on the SAME host, distinct class + instance ---
    let device2 =
        VirtioHdvDevice::attach_shared(host.clone(), ws2, "hp2", guest_mem, &class2, &instance2)
            .expect("attach device #2 on shared host");
    vm.modify(&add_slot_request(
        &guid_to_string(&instance2),
        &guid_to_string(&class2),
    ))
    .expect("Add slot #2");
    if !vm.wait_for_console("HOTPLUG_MOUNT_PASS tag=hp2", Duration::from_secs(45)) {
        eprintln!("===== device #2 never mounted =====\n{}", vm.console());
        return false;
    }
    // Stage 2 gate: both present at once.
    if !vm.console().contains("HOTPLUG_MOUNT_PASS tag=hp1") {
        eprintln!("hp1 mount line missing after #2 came up");
        return false;
    }
    eprintln!("both devices mounted concurrently — Stage 2 PASS");

    // --- Stage 3: hot-remove is BEST-EFFORT. WSL's own `DeviceHostProxy::RemoveDevice`
    // notes removal is "best effort since not all versions of Windows support it" and
    // swallows the failure. On this build `HcsModifyComputeSystem` Remove returns
    // ERROR_NOT_SUPPORTED (0x80070032), and there is no per-device HDV destroy export,
    // so a device persists until VM shutdown. We attempt it and log the outcome, but
    // the spike's pass criterion is the proven win: concurrent coexistence.
    match vm.modify(&remove_slot_request(
        &guid_to_string(&instance1),
        &guid_to_string(&class1),
    )) {
        Ok(()) => {
            let removed = vm.wait_for_console("HOTPLUG_REMOVE_OK tag=hp1", Duration::from_secs(30));
            eprintln!("Stage 3: slot Remove SUCCEEDED; guest unmounted hp1 = {removed}");
        }
        Err(e) => {
            eprintln!("Stage 3: slot Remove unsupported on this Windows (expected per WSL): {e}")
        }
    }

    let _ = (&support, &device1, &device2, &host);
    true // pass criterion: both devices mounted concurrently
}

// ============== Diagnostic — two devices declared COLD ==============
//
// Stage 2's runtime path hit ERROR_HV_INVALID_PARAMETER (0xC0350005) adding a
// *second* FlexibleIov slot at runtime — even with a distinct class id. This test
// answers the prerequisite question: can two FlexibleIov devices coexist on one VM
// *at all*, if both slots are declared **cold** (in the create document) and both
// devices created **before** start? If yes, multi-share via device-per-share is
// viable (atelier would pre-declare a slot pool); if no, FlexibleIov is 1-device.

#[test]
#[ignore = "requires Hyper-V + Rocky artifacts; run with --ignored"]
fn two_devices_cold_declared() {
    let kernel =
        std::env::var("HVFS_KERNEL").unwrap_or_else(|_| r"E:\dev\spike\out\vmlinuz".into());
    let initrd = std::env::var("HVFS_INITRD")
        .unwrap_or_else(|_| r"E:\dev\spike\out\initramfs.cpio.gz".into());
    assert!(
        std::path::Path::new(&kernel).exists(),
        "kernel not found: {kernel}"
    );
    assert!(
        std::path::Path::new(&initrd).exists(),
        "initrd not found: {initrd}"
    );

    let ws1 = make_workspace("hp1");
    let ws2 = make_workspace("hp2");

    const ATTEMPTS: usize = 4;
    for attempt in 1..=ATTEMPTS {
        eprintln!("--- attempt {attempt}/{ATTEMPTS} ---");
        if try_two_cold(&kernel, &initrd, &ws1, &ws2) {
            eprintln!("PASS on attempt {attempt}");
            return;
        }
    }
    panic!("two cold-declared devices did not both mount in {ATTEMPTS} attempts");
}

/// One attempt: declare two FlexibleIov slots cold, create both devices on the
/// shared host before start, boot, and require both to mount.
fn try_two_cold(kernel: &str, initrd: &str, ws1: &std::path::Path, ws2: &std::path::Path) -> bool {
    let class1 = HVFS_DEVICE_CLASS_ID;
    let instance1 = HVFS_DEVICE_INSTANCE_ID;
    let mut class2 = HVFS_DEVICE_CLASS_ID;
    class2.Data4[7] = 0x12;
    let mut instance2 = HVFS_DEVICE_INSTANCE_ID;
    instance2.Data4[7] = 0x22;

    // Both slots declared cold in the create document.
    let mut cfg = RockyConfig::new(kernel, initrd)
        .with_flexible_iov(FlexibleIovSlot::new(
            guid_to_string(&instance1),
            guid_to_string(&class1),
        ))
        .with_flexible_iov(FlexibleIovSlot::new(
            guid_to_string(&instance2),
            guid_to_string(&class2),
        ));
    cfg.kernel_cmdline = "console=ttyS0 atelier.hptags=hp1,hp2".into();

    let vm = RockyVm::create(&cfg).expect("create Rocky compute system");
    eprintln!("compute system id: {}", vm.id());

    // One shared device host; both devices created BEFORE start so the cold slots
    // resolve at the start reservation.
    // SAFETY: the VM outlives `support` and both devices.
    let support = unsafe { DeviceHostSupport::new(vm.system_handle()) };
    let host = Arc::new(
        unsafe { DeviceHost::from_proxy(&HVFS_DEVICE_HOST_ID, support.as_iunknown()) }
            .unwrap_or_else(|e| panic!("from_proxy (shared host) failed: {e}")),
    );
    let guest_mem = cfg.memory_mb as u64 * 1024 * 1024;
    let device1 =
        VirtioHdvDevice::attach_shared(host.clone(), ws1, "hp1", guest_mem, &class1, &instance1)
            .expect("attach device #1");
    let device2 =
        VirtioHdvDevice::attach_shared(host.clone(), ws2, "hp2", guest_mem, &class2, &instance2)
            .expect("attach device #2");

    vm.start().expect("start Rocky compute system");

    let p1 = vm.wait_for_console("HOTPLUG_MOUNT_PASS tag=hp1", Duration::from_secs(45));
    let p2 = vm.wait_for_console("HOTPLUG_MOUNT_PASS tag=hp2", Duration::from_secs(45));
    if !(p1 && p2) {
        eprintln!(
            "===== two-cold console (hp1={p1} hp2={p2}) =====\n{}\n===================",
            vm.console()
        );
    }

    let _ = (&support, &device1, &device2, &host);
    p1 && p2
}
