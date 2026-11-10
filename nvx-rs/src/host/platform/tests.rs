#[cfg(target_os = "linux")]
pub(crate) use super::linux::tests::{
    check_identity, owner_only_problem, start_time_from_stat, store_dir_problem,
};
#[cfg(all(windows, feature = "cli"))]
pub(crate) use super::windows::tests::disconnect_control;
#[cfg(windows)]
pub(crate) use super::windows::tests::set_access;
#[cfg(windows)]
pub(crate) use super::windows::tests::{accept_control, control_server, wait_exit_handle};
#[cfg(windows)]
pub(crate) use super::windows::tests::{
    access, claim_released_within, inherits_file, untrusted_log_job, user_sid, with_spawn_window,
};
use super::*;

#[test]
fn a_bounded_read_refuses_a_regular_file_that_holds_more_than_its_bound() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("record.bin");
    std::fs::write(&path, b"data").unwrap();
    assert_eq!(
        read_at_most(open_regular(&path).unwrap(), 4, "a record").unwrap(),
        b"data"
    );
    let error = read_at_most(open_regular(&path).unwrap(), 3, "a record").unwrap_err();
    assert_eq!(
        error.to_string(),
        "it holds more than the 3 bytes that a record may hold"
    );
    assert!(is_refused(&error), "{error}");
    // A file that is missing is a failure of the host's, not a refusal of what a file is.
    let error = open_regular(&directory.path().join("missing")).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
    assert!(!is_refused(&error), "{error}");
}

#[cfg(unix)]
#[test]
fn an_open_of_a_regular_file_refuses_a_link_and_a_directory() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target.bin");
    std::fs::write(&target, b"data").unwrap();
    std::os::unix::fs::symlink(&target, directory.path().join("link")).unwrap();
    std::fs::create_dir(directory.path().join("directory")).unwrap();
    for name in ["link", "directory"] {
        let error = open_regular(&directory.path().join(name)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "it is a link, or not a regular file",
            "{name}"
        );
        assert!(is_refused(&error), "{name}: {error}");
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn a_transport_without_a_native_handle_needs_no_clone_implementation() {
    struct CustomTransport;

    impl Transport for CustomTransport {
        fn read(&mut self, _: &mut [u8], _: Option<Duration>) -> io::Result<usize> {
            unreachable!("cloning a handle does not read the transport")
        }

        fn write_all(&mut self, _: &[u8], _: Option<Duration>) -> io::Result<()> {
            unreachable!("cloning a handle does not write the transport")
        }
    }

    let transport: Box<dyn Transport> = Box::new(CustomTransport);
    assert!(transport.try_clone_handle().unwrap().is_none());
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn host_identities_are_read_as_openvmm_reads_them() {
    let id = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
        0xef,
    ];
    for text in [
        "0123456789abcdef0123456789ABCDEF\n",
        "01234567-89ab-cdef-0123-456789abcdef",
        "{01234567-89AB-CDEF-0123-456789ABCDEF}",
        " 0123456789abcdef0123456789abcdef ",
    ] {
        assert_eq!(parse_host_id(text), Some(id), "{text:?}");
    }
    for text in [
        "0123",
        "0123456789abcdef0123456789abcdeg",
        "0123456789abcdef0123456789abcdef00",
        "",
    ] {
        assert_eq!(parse_host_id(text), None, "{text:?}");
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn the_hosts_identity_and_monotonic_time_are_read() {
    let first = monotonic_ns().unwrap();
    assert!(monotonic_ns().unwrap() >= first);
    match host_id() {
        Ok(id) => assert_eq!(host_id().unwrap(), id),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            eprintln!("skipped: this host has no machine identity: {error}");
        }
        Err(error) => panic!("{error}"),
    }
}

/// The stores tell files apart by the identities that their seals hold, with the 128-bit file
/// IDs of `FILE_ID_INFO` on Windows, whether they examine a path, an open file, or an entry of
/// an open directory.
#[cfg(any(target_os = "linux", windows))]
#[test]
fn the_stores_tell_files_apart_by_the_identities_that_their_seals_hold() {
    use crate::host::seal::FileSeal;

    let directory = tempfile::tempdir().unwrap();
    let opened = Directory::open(directory.path()).unwrap();
    let mut ids = Vec::new();
    for name in ["first", "second"] {
        let path = directory.path().join(name);
        std::fs::write(&path, b"").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let seal = FileSeal::of(&file).unwrap();
        let id = (seal.device(), seal.file_id());
        assert_eq!(opened_file_id(&file).unwrap(), id, "{name}");
        assert_eq!(file_id(&path).unwrap(), id, "{name}");
        assert_eq!(opened.entry_id(name).unwrap(), Some(id), "{name}");
        ids.push(id);
    }
    assert_ne!(ids[0], ids[1]);
    assert_eq!(opened.entry_id("missing").unwrap(), None);
}
