use super::*;

/// Returns the line that the kernel writes for a mount of `filesystem` from `source` at
/// `mount_point`, on the device `device`, with the optional fields `optional`.
fn line(device: &str, mount_point: &str, optional: &str, filesystem: &str, source: &str) -> String {
    format!("36 25 {device} / {mount_point} rw,relatime{optional} - {filesystem} {source} rw")
}

#[test]
fn a_line_tells_the_device_mount_point_type_and_source_of_its_mount() {
    let text = line(
        "254:2",
        "/run/nvx/layers/p2",
        " shared:7 master:1",
        "ext4",
        "/dev/vda2",
    );
    assert_eq!(
        Line::parse(text.as_bytes()),
        Some(Line {
            device: DeviceNumber {
                major: 254,
                minor: 2
            },
            mount_point: PathBuf::from("/run/nvx/layers/p2"),
            filesystem: b"ext4".to_vec(),
            source: b"/dev/vda2".to_vec(),
        })
    );
    // Without optional fields or the file system's options.
    let parsed = Line::parse(b"1 1 0:2 / / rw - rootfs rootfs").unwrap();
    assert_eq!(parsed.device, DeviceNumber { major: 0, minor: 2 });
    assert_eq!(parsed.filesystem, b"rootfs");
    assert_eq!(parsed.source, b"rootfs");
    // A source that the mounter left empty, and file system options of any form.
    let parsed = Line::parse(b"36 25 0:50 / /mnt rw - tmpfs  rw  x").unwrap();
    assert_eq!(parsed.filesystem, b"tmpfs");
    assert_eq!(parsed.source, b"");
    assert_eq!(
        DeviceNumber {
            major: 259,
            minor: 0
        }
        .to_string(),
        "259:0"
    );
}

#[test]
fn escaped_fields_are_unescaped() {
    let text = line(
        "0:31",
        "/mnt/a\\040b\\011c\\012d\\134e",
        "",
        "virt\\134iofs",
        "tag\\040x",
    );
    let parsed = Line::parse(text.as_bytes()).unwrap();
    assert_eq!(parsed.mount_point, Path::new("/mnt/a b\tc\nd\\e"));
    assert_eq!(parsed.filesystem, b"virt\\iofs");
    assert_eq!(parsed.source, b"tag x");
    // A backslash that starts no escape stays, and so does one before a byte above 0o377.
    assert_eq!(unescape(b"a\\b\\04\\400\\377"), b"a\\b\\04\\400\xff");
}

#[test]
fn lines_of_another_form_are_refused() {
    for text in [
        "",
        "36 25 254:2 / /mnt rw - ext4",
        "36 25 254:2 / /mnt rw shared:1 ext4 /dev/vda rw",
        "36 25 254:2 / /mnt rw",
        "x 25 254:2 / /mnt rw - ext4 /dev/vda rw",
        "36 -1 254:2 / /mnt rw - ext4 /dev/vda rw",
        "36 25 254 / /mnt rw - ext4 /dev/vda rw",
        "36 25 254:x / /mnt rw - ext4 /dev/vda rw",
        "36 25 4294967296:0 / /mnt rw - ext4 /dev/vda rw",
        "36 25 +254:0 / /mnt rw - ext4 /dev/vda rw",
        "36 25  254:0 / /mnt rw - ext4 /dev/vda rw",
        "36 25 254:0 /  /mnt rw - ext4 /dev/vda rw",
        "36 25 254:0 / /mnt rw  shared:1 - ext4 /dev/vda rw",
        "36 25 254:0 / /mnt rw -  ext4 /dev/vda rw",
    ] {
        assert_eq!(Line::parse(text.as_bytes()), None, "{text:?}");
    }
}
