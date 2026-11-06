use super::*;
use std::os::unix::fs::PermissionsExt;

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
    if crate::guest::sys::tests::isolate_process_test(
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
fn the_resolver_keeps_the_single_server_format_and_refuses_existing_files_and_links() {
    let directory = tempfile::tempdir().unwrap();
    for (name, server, expected) in [
        ("ipv4", "10.0.0.1", "nameserver 10.0.0.1\n"),
        ("ipv6", "fd00::53", "nameserver fd00::53\n"),
    ] {
        let path = directory.path().join(name);
        write_resolver_file(&path, server.parse().unwrap()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), expected.as_bytes());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o444
        );
        let error = write_resolver_file(&path, "10.0.0.2".parse().unwrap()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), expected.as_bytes());
    }
    let link = directory.path().join("link");
    let target = directory.path().join("missing");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let error = write_resolver_file(&link, "10.0.0.1".parse().unwrap()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert!(!target.exists());
}
