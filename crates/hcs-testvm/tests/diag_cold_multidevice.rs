//! **Diagnostic (negative): two *custom*-class FlexibleIov devices can't coexist.**
//!
//! The shipped multi-share path gives every virtio-fs device the **well-known**
//! `VIRTIO_FS_DEVICE_CLASS_ID` and a unique instance id — that's what lets N devices
//! coexist (proven by `hotplug::guest_hot_mounts_two_devices_concurrently`). This test
//! pins *why* that's required: declaring two devices with **distinct custom class ids**
//! cold and starting the VM is rejected by the platform at power-on with
//! `ERROR_HV_INVALID_PARAMETER` (`0xC0350005`). It asserts the limitation **holds**, so
//! it passes today and will fail (a useful signal to re-check) if a platform update ever
//! lifts it. Kept out of the green ladder because it's a platform-behavior probe, not a
//! product-path test.
//!
//!   cargo test -p hcs-testvm --test diag_cold_multidevice -- --ignored --nocapture
#![cfg(windows)]

use hcs_testvm::{FlexibleIovSlot, RockyConfig, RockyVm};
use hdv::pci::{
    guid_to_string, HVFS_DEVICE_CLASS_ID, HVFS_DEVICE_HOST_ID, HVFS_DEVICE_INSTANCE_ID,
};
use hdv::proxy::DeviceHostSupport;
use hdv::DeviceHost;
use std::sync::Arc;
use virtio_hdv::VirtioHdvDevice;

fn make_workspace(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("atelier-coldmulti-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workspace");
    std::fs::write(dir.join("SENTINEL.txt"), b"cold multi-device probe\n").unwrap();
    dir
}

#[test]
#[ignore = "diagnostic; requires Hyper-V + Rocky artifacts; run with --ignored"]
fn two_custom_class_cold_devices_are_rejected() {
    let (kernel, initrd) = hcs_testvm::artifact_paths();
    assert!(std::path::Path::new(&kernel).exists(), "kernel: {kernel}");
    assert!(std::path::Path::new(&initrd).exists(), "initrd: {initrd}");

    let ws1 = make_workspace("c1");
    let ws2 = make_workspace("c2");

    // Two DISTINCT custom class ids (the anti-pattern): derived from the product
    // constants by tweaking the last byte.
    let class1 = HVFS_DEVICE_CLASS_ID;
    let instance1 = HVFS_DEVICE_INSTANCE_ID;
    let mut class2 = HVFS_DEVICE_CLASS_ID;
    class2.Data4[7] = 0x12;
    let mut instance2 = HVFS_DEVICE_INSTANCE_ID;
    instance2.Data4[7] = 0x22;

    let mut cfg = RockyConfig::new(&kernel, &initrd)
        .with_flexible_iov(FlexibleIovSlot::new(
            guid_to_string(&instance1),
            guid_to_string(&class1),
        ))
        .with_flexible_iov(FlexibleIovSlot::new(
            guid_to_string(&instance2),
            guid_to_string(&class2),
        ));
    cfg.kernel_cmdline = "console=ttyS0 atelier.hptags=c1,c2".into();

    let vm = RockyVm::create(&cfg).expect("create compute system");
    eprintln!("compute system id: {}", vm.id());

    // SAFETY: the VM outlives `support` and both devices.
    let support = unsafe { DeviceHostSupport::new(vm.system_handle()) };
    let host = Arc::new(
        unsafe { DeviceHost::from_proxy(&HVFS_DEVICE_HOST_ID, support.as_iunknown()) }
            .unwrap_or_else(|e| panic!("from_proxy failed: {e}")),
    );
    let guest_mem = cfg.memory_mb as u64 * 1024 * 1024;
    // Both device *creations* succeed; the platform only rejects the pair at power-on.
    let _d1 =
        VirtioHdvDevice::attach_shared(host.clone(), &ws1, "c1", guest_mem, &class1, &instance1)
            .expect("attach device #1");
    let _d2 =
        VirtioHdvDevice::attach_shared(host.clone(), &ws2, "c2", guest_mem, &class2, &instance2)
            .expect("attach device #2");

    match vm.start() {
        Err(e) if e.contains("0xc0350005") => {
            eprintln!("limitation confirmed — two custom-class devices rejected at power-on:\n{e}");
        }
        Err(e) => panic!("start failed, but not with the expected 0xC0350005:\n{e}"),
        Ok(()) => panic!(
            "two custom-class cold devices unexpectedly started — the platform limitation \
             may be lifted; re-check whether the well-known class id is still required"
        ),
    }

    let _ = (&support, &host);
}
