#[cfg(target_os = "linux")]
pub(crate) use super::linux::tests::{
    check_identity, owner_only_problem, start_time_from_stat, store_dir_problem,
};
#[cfg(windows)]
pub(crate) use super::windows::tests::set_access;
use super::*;

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
