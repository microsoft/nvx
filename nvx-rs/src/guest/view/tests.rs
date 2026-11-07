use super::*;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::unix::fs::{PermissionsExt, symlink};

#[test]
fn machine_ids_are_version_4_uuids_in_lowercase_hexadecimal() {
    assert_eq!(
        &machine_id_text([0; 16]),
        b"00000000000040008000000000000000\n"
    );
    assert_eq!(
        &machine_id_text([0xff; 16]),
        b"ffffffffffff4fffbfffffffffffffff\n"
    );
    assert_eq!(
        &machine_id_text([
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef
        ]),
        b"0123456789ab4def8123456789abcdef\n"
    );
}

#[test]
fn a_machine_id_is_generated_once_per_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("machine-id");
    let mode = MachineId::generate(&path).unwrap();
    assert_eq!(mode, MachineId::Generated(path.clone()));
    let text = std::fs::read(&path).unwrap();
    assert_eq!(text.len(), 33);
    assert_eq!(text[32], b'\n');
    assert!(
        text[..32]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    );
    assert_eq!(text[12], b'4');
    assert!(matches!(text[16], b'8' | b'9' | b'a' | b'b'));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o444
    );
    let error = MachineId::generate(&path).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&path).unwrap(), text);
    let target = directory.path().join("target");
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert_eq!(
        MachineId::generate(&link).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert!(!target.exists());
    let other = directory.path().join("other");
    MachineId::generate(&other).unwrap();
    assert_ne!(std::fs::read(&other).unwrap(), text);
}

#[test]
fn a_generated_machine_id_has_mode_0444_whatever_the_umask() {
    if crate::guest::managed_agent::tests::isolate_process_test(
        "NVX_ENTRY_UMASK_TEST_CHILD",
        "guest::view::tests::a_generated_machine_id_has_mode_0444_whatever_the_umask",
    ) {
        return;
    }
    nix::sys::stat::umask(sys::Mode::from_bits_truncate(0o077));
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("machine-id");
    MachineId::generate(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o444
    );
}

#[test]
fn a_resolver_file_names_its_dns_server_once_and_every_execution_may_read_it() {
    // The file names the server in its canonical text, as NVX's init writes the initramfs's,
    // with mode 0444, whatever the umask.
    let directory = tempfile::tempdir().unwrap();
    let dns = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
    let ipv6 = IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0xa00, 1));
    let cases = [
        ("v4", dns, &b"nameserver 10.0.0.1\n"[..]),
        ("v6", ipv6, &b"nameserver fd00::a00:1\n"[..]),
    ];
    for (name, server, text) in cases {
        let path = directory.path().join(name);
        sys::tests::with_thread_umask(0o077, || write_resolver_file(&path, server)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), text);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o444, "{mode:o}");
    }

    // A file there already fails the write, and is left alone, and so is the file that a
    // symbolic link there names, or the path where a dangling one leads.
    let written = directory.path().join("v4");
    let error = write_resolver_file(&written, ipv6).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    let link = directory.path().join("link");
    symlink(&written, &link).unwrap();
    let error = write_resolver_file(&link, ipv6).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&written).unwrap(), b"nameserver 10.0.0.1\n");
    let absent = directory.path().join("absent");
    let dangling = directory.path().join("dangling");
    symlink(&absent, &dangling).unwrap();
    let error = write_resolver_file(&dangling, dns).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert!(!absent.exists());
}
