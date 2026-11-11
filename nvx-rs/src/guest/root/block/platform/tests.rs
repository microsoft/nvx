use super::*;
use std::os::unix::fs::symlink;

#[test]
fn of_reg_uses_the_declared_big_endian_cells_and_exact_checked_bounds() {
    let wide = reg(0xd0003000, 0x1000);
    assert_eq!(resource(&wide, 2, 2).unwrap(), (0xd0003000, 0xd0004000));
    let narrow: Vec<_> = [0xd0003000_u32, 0x1000]
        .into_iter()
        .flat_map(u32::to_be_bytes)
        .collect();
    assert_eq!(resource(&narrow, 1, 1).unwrap(), (0xd0003000, 0xd0004000));
    for (bytes, address, size) in [
        (vec![], 2, 2),
        (wide[..15].to_vec(), 2, 2),
        (wide.repeat(2), 2, 2),
        (wide.clone(), 0, 2),
        (wide, 3, 1),
        (reg(0xd0003000, 0), 2, 2),
        (reg(u64::MAX, 1), 2, 2),
    ] {
        assert!(resource(&bytes, address, size).is_err());
    }
}

fn reg(start: u64, size: u64) -> Vec<u8> {
    [start, size]
        .into_iter()
        .flat_map(u64::to_be_bytes)
        .collect()
}

#[test]
fn reserved_and_unknown_slots_fail_before_sysfs_access() {
    let paths = Paths::default();
    for address in [0xd0004000, 0xd0005000, 0xd0008000, 0xd0003001, u64::MAX] {
        assert_eq!(
            window_directories(&paths, address).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

#[test]
fn multiple_virtio_or_block_children_are_not_selected_by_order() {
    let root = tempfile::tempdir().unwrap();
    let window = root.path();
    assert!(block_device(window).unwrap().is_none());
    bind_transport(window);
    fs::create_dir_all(window.join("virtio9/block/vdz")).unwrap();
    assert_eq!(block_device(window).unwrap().as_deref(), Some("vdz"));
    fs::create_dir_all(window.join("virtio9/block/vda")).unwrap();
    assert!(block_device(window).is_err());
    fs::remove_dir(window.join("virtio9/block/vda")).unwrap();
    fs::create_dir_all(window.join("virtio1")).unwrap();
    assert!(block_device(window).is_err());
}

fn paths(root: &Path) -> Paths {
    Paths {
        proc: root.join("proc"),
        sys: root.join("sys"),
        dev: root.join("dev"),
    }
}

fn bind_transport(device: &Path) {
    symlink(
        "/sys/bus/platform/drivers/virtio-mmio",
        device.join("driver"),
    )
    .unwrap();
}

fn transport(paths: &Paths, name: &str, start: u64, size: u64) -> PathBuf {
    let root = paths.sys.join("firmware/devicetree/base");
    let node = root.join(format!("unrelated-node-name-{name}"));
    fs::create_dir_all(&node).unwrap();
    fs::write(root.join("#address-cells"), 2_u32.to_be_bytes()).unwrap();
    fs::write(root.join("#size-cells"), 2_u32.to_be_bytes()).unwrap();
    fs::write(node.join("compatible"), b"virtio,mmio\0").unwrap();
    fs::write(node.join("reg"), reg(start, size)).unwrap();
    let directory = paths.sys.join("bus/platform/devices").join(name);
    fs::create_dir_all(&directory).unwrap();
    symlink(node, directory.join("of_node")).unwrap();
    directory
}

#[test]
fn actual_of_links_find_roles_without_a_platform_resource_file_or_node_name() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let directory = transport(&paths, "arbitrary-name", 0xd0003000, 0x1000);
    assert!(!directory.join("resource").exists());
    bind_transport(&directory);
    fs::create_dir_all(directory.join("virtio9/block/vdz")).unwrap();
    assert_eq!(
        window_directories(&paths, 0xd0003000).unwrap(),
        vec![directory.clone()]
    );
    assert_eq!(block_device(&directory).unwrap().as_deref(), Some("vdz"));
}

#[test]
fn pure_role_lookup_distinguishes_absent_unbound_pending_and_ready_without_mutation() {
    use crate::guest::root::{BlockRoleState, block::lookup};
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let distro = transport(&paths, "shuffled", 0xd0003000, 0x1000);
    assert!(matches!(
        lookup(&paths, 0xd0006000).unwrap(),
        BlockRoleState::Absent
    ));
    let scratch = transport(&paths, "not-vdb", 0xd0006000, 0x1000);
    assert!(matches!(
        lookup(&paths, 0xd0006000).unwrap(),
        BlockRoleState::Probing
    ));
    assert!(!scratch.join("driver").exists());
    bind_transport(&distro);
    bind_transport(&scratch);
    assert!(matches!(
        lookup(&paths, 0xd0006000).unwrap(),
        BlockRoleState::Probing
    ));
    fs::create_dir_all(distro.join("virtio9/block/vdz")).unwrap();
    fs::create_dir_all(&paths.dev).unwrap();
    fs::write(paths.dev.join("vdz"), b"fixture devtmpfs node").unwrap();
    assert!(
        matches!(lookup(&paths, 0xd0003000).unwrap(), BlockRoleState::Ready(path) if path == paths.dev.join("vdz"))
    );
    fs::create_dir_all(scratch.join("virtio2/block/vdc")).unwrap();
    assert!(matches!(
        lookup(&paths, 0xd0006000).unwrap(),
        BlockRoleState::Probing
    ));
    fs::write(paths.dev.join("vdc"), b"fixture devtmpfs node").unwrap();
    assert!(
        matches!(lookup(&paths, 0xd0006000).unwrap(), BlockRoleState::Ready(path) if path == paths.dev.join("vdc"))
    );
    fs::write(scratch.join("of_node/compatible"), b"not,a-virtio-device\0").unwrap();
    assert!(lookup(&paths, 0xd0006000).is_err());
    fs::write(scratch.join("of_node/compatible"), b"virtio,mmio\0").unwrap();
    fs::remove_file(scratch.join("driver")).unwrap();
    symlink("/sys/bus/platform/drivers/wrong", scratch.join("driver")).unwrap();
    assert!(lookup(&paths, 0xd0006000).is_err());
}

#[test]
fn repeated_or_overlapping_resources_and_wrong_driver_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let first = transport(&paths, "first", 0xd0003000, 0x1000);
    let second = transport(&paths, "second", 0xd0003000, 0x1000);
    assert!(window_directories(&paths, 0xd0003000).is_err());
    fs::remove_dir_all(second).unwrap();
    for bad in [
        reg(0xd0003000, 0xfff),
        reg(0xd0003000, 0x1001),
        reg(0xd0002fff, 2),
    ] {
        fs::write(first.join("of_node/reg"), bad).unwrap();
        assert!(window_directories(&paths, 0xd0003000).is_err());
    }
    fs::write(first.join("of_node/reg"), reg(0xd0003000, 0x1000).repeat(2)).unwrap();
    assert!(window_directories(&paths, 0xd0003000).is_err());
    fs::write(first.join("of_node/reg"), reg(0xd0003000, 0x1000)).unwrap();
    symlink("/sys/bus/platform/drivers/not-virtio", first.join("driver")).unwrap();
    assert!(window_directories(&paths, 0xd0003000).is_err());
    fs::remove_file(first.join("driver")).unwrap();
    symlink(
        "/sys/bus/platform/drivers/virtio-mmio",
        first.join("driver"),
    )
    .unwrap();
    assert_eq!(window_directories(&paths, 0xd0003000).unwrap(), vec![first]);
}

#[test]
fn malformed_cells_compatible_and_translated_bus_nodes_are_explicit_errors() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let device = transport(&paths, "transport", 0xd0003000, 0x1000);
    let base = paths.sys.join("firmware/devicetree/base");
    for cells in [
        vec![],
        vec![2],
        0_u32.to_be_bytes().to_vec(),
        3_u32.to_be_bytes().to_vec(),
    ] {
        fs::write(base.join("#address-cells"), cells).unwrap();
        assert!(window_directories(&paths, 0xd0003000).is_err());
    }

    fs::write(base.join("#address-cells"), 2_u32.to_be_bytes()).unwrap();
    fs::write(device.join("of_node/compatible"), b"virtio,mmio").unwrap();
    assert!(window_directories(&paths, 0xd0003000).is_err());
    fs::write(device.join("of_node/compatible"), b"virtio,mmio\0").unwrap();
    let nested = base.join("translated-bus/node");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("compatible"), b"virtio,mmio\0").unwrap();
    fs::write(nested.join("reg"), reg(0xd0003000, 0x1000)).unwrap();
    fs::remove_file(device.join("of_node")).unwrap();
    symlink(nested, device.join("of_node")).unwrap();
    assert!(window_directories(&paths, 0xd0003000).is_err());
}

#[test]
fn a_non_of_virtio_transport_is_not_reported_as_an_absent_role() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    transport(&paths, "distro", 0xd0003000, 0x1000);
    let cmdline = paths.sys.join("bus/platform/devices/command-line-device");
    fs::create_dir_all(&cmdline).unwrap();
    fs::write(cmdline.join("modalias"), b"platform:virtio-mmio\n").unwrap();
    assert!(window_directories(&paths, 0xd0006000).is_err());
}
