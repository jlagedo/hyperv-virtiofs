//! **virtio-fs-over-HDV end-to-end spike** (task #8 / milestone 2).
//!
//! Where `attach_proxy.rs` proved the *transport* (a driverless `SpikeDevice`
//! enumerates over the `ExternalRestricted` FlexibleIov proxy path), this proves
//! the *device*: it swaps in `virtio_hdv::VirtioHdvDevice` — OpenVMM's real
//! `VirtioPciDevice` driving a `VirtioFsDevice` over a host directory — and
//! asserts the guest **mounts** the share and reads a file through it.
//!
//! Path (same proxy handshake as `attach_proxy.rs`, see `docs/hdv-proxy-abi.md`):
//!   1. create the VM with the FlexibleIov slot (cold), no start yet;
//!   2. `DeviceHostSupport` + `DeviceHost::from_proxy` → proxy-register the host;
//!   3. `VirtioHdvDevice::attach(host, ws, "ws", mem)` — builds the virtio PCI
//!      device, backs guest-memory/MSI/MMIO with HDV, and creates the instance;
//!   4. start; the guest's init `modprobe virtiofs` + `mount -t virtiofs ws`.
//!
//! PASS → the guest console prints `PROOF_COMPLETE_PASS` (mounted + read the
//! SENTINEL). This retires design §7's milestone 2: a stock HCS/EL-family guest
//! mounts a host folder with **no 9p**, via our open HDV virtio-fs bridge.
//!
//! `#[ignore]` — needs Hyper-V + the Rocky artifacts. Run it:
//!   $env:HVFS_KERNEL="E:\dev\spike\out\vmlinuz"
//!   $env:HVFS_INITRD="E:\dev\spike\out\initramfs.cpio.gz"
//!   cargo test -p hcs-testvm --test attach_virtiofs -- --ignored --nocapture
#![cfg(windows)]

use hcs_testvm::{FlexibleIovSlot, RockyConfig, RockyVm};
use hdv::pci::{guid_to_string, SPIKE_CLASS_ID, SPIKE_HOST_ID, SPIKE_INSTANCE_ID};
use hdv::proxy::DeviceHostSupport;
use hdv::DeviceHost;
use std::time::Duration;
use virtio_hdv::VirtioHdvDevice;

/// Build a throwaway host workspace dir with a sentinel file for the guest to read.
fn make_workspace() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("atelier-virtiofs-spike-ws");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workspace dir");
    std::fs::write(
        dir.join("SENTINEL.txt"),
        b"hello from the host via virtio-fs over HDV\n",
    )
    .expect("write sentinel");
    dir
}

#[test]
#[ignore = "requires Hyper-V + Rocky artifacts; run with --ignored"]
fn guest_mounts_virtiofs_over_hdv() {
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

    let ws = make_workspace();
    eprintln!("host workspace: {}", ws.display());

    // Cold path: declare the FlexibleIov slot in the create document (its
    // EmulatorId == the HDV DeviceClassId our device uses, == SPIKE_CLASS_ID),
    // then proxy-register the device host before start.
    let cfg = RockyConfig::new(&kernel, &initrd).with_flexible_iov(FlexibleIovSlot::new(
        guid_to_string(&SPIKE_INSTANCE_ID),
        guid_to_string(&SPIKE_CLASS_ID),
    ));
    let vm = RockyVm::create(&cfg).expect("create Rocky compute system");
    eprintln!("compute system id: {}", vm.id());

    // Proxy-register the device host (the proven ExternalRestricted path).
    // SAFETY: the VM (hence its system handle) outlives `support` and the device.
    let support = unsafe { DeviceHostSupport::new(vm.system_handle()) };
    let host = unsafe { DeviceHost::from_proxy(&SPIKE_HOST_ID, support.as_iunknown()) }
        .unwrap_or_else(|e| panic!("HdvInitializeDeviceHostForProxy failed: {e}"));
    eprintln!(
        "proxy host created; registered={} register_hr={:#010x}",
        support.was_registered(),
        support.register_hr() as u32,
    );

    // The real device: OpenVMM virtio-fs over HDV, sharing `ws` under tag "ws".
    let guest_mem = cfg.memory_mb as u64 * 1024 * 1024;
    let device = VirtioHdvDevice::attach(host, &ws, "ws", guest_mem)
        .expect("attach virtio-fs over HDV");
    eprintln!("virtio-fs device attached; starting guest…");

    vm.start().expect("start Rocky compute system");

    // The guest init mounts `-t virtiofs ws /mnt/ws` and prints the sentinel.
    let pass = vm.wait_for_console("PROOF_COMPLETE_PASS", Duration::from_secs(120));
    eprintln!(
        "===== guest console =====\n{}\n=========================",
        vm.console()
    );

    // Keep the device alive until the console is checked.
    let _ = &device;
    let _ = &support;

    assert!(
        pass,
        "guest did not mount virtio-fs (no PROOF_COMPLETE_PASS); see console above"
    );
}
