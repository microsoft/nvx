use std::fs;
use std::os::unix::fs::symlink;

use super::*;

#[test]
fn selected_window_requires_matching_dt_identity_not_a_platform_filename() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths {
        sys: root.path().join("sys"),
        proc: root.path().join("proc"),
        dev: root.path().join("dev"),
    };
    let tree = paths.sys.join("firmware/devicetree/base");
    fs::create_dir_all(&tree).unwrap();
    for name in ["#address-cells", "#size-cells"] {
        fs::write(tree.join(name), 2u32.to_be_bytes()).unwrap();
    }
    let node = tree.join("virtio@d0003000");
    fs::create_dir(&node).unwrap();
    let reg = [0xd0003000u64.to_be_bytes(), 0x200u64.to_be_bytes()].concat();
    fs::write(node.join("reg"), &reg).unwrap();
    fs::write(node.join("compatible"), b"virtio,mmio\0").unwrap();
    let device = paths
        .sys
        .join("bus/platform/devices/different-platform-name");
    fs::create_dir_all(&device).unwrap();
    assert!(windows(&paths, 0xd0003000).unwrap().is_empty());
    symlink(&node, device.join("of_node")).unwrap();
    assert_eq!(
        windows(&paths, 0xd0003000).unwrap().as_slice(),
        std::slice::from_ref(&device)
    );
    assert!(windows(&paths, 0xd0006000).unwrap().is_empty());
    fs::write(node.join("compatible"), b"not-virtio\0").unwrap();
    assert_eq!(
        windows(&paths, 0xd0003000).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    fs::write(node.join("compatible"), b"virtio,mmio\0").unwrap();
    fs::write(node.join("status"), b"disabled\0").unwrap();
    assert!(windows(&paths, 0xd0003000).is_err());
    fs::write(node.join("status"), b"okay\0").unwrap();
    let other = paths.sys.join("bus/platform/devices/duplicate");
    fs::create_dir(&other).unwrap();
    symlink(&node, other.join("of_node")).unwrap();
    assert!(windows(&paths, 0xd0003000).is_err());
    fs::remove_file(other.join("of_node")).unwrap();
    let duplicate = tree.join("duplicate");
    fs::create_dir(&duplicate).unwrap();
    fs::write(duplicate.join("reg"), reg).unwrap();
    fs::write(duplicate.join("compatible"), b"virtio,mmio\0").unwrap();
    assert!(windows(&paths, 0xd0003000).is_err());
    fs::remove_file(duplicate.join("reg")).unwrap();
    fs::write(tree.join("#size-cells"), 1u32.to_be_bytes()).unwrap();
    assert!(windows(&paths, 0xd0003000).is_err());
}
