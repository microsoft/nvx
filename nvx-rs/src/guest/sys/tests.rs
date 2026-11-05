//! Test streams and focused checks shared by the guest tools.

use std::io::{self, Write};

use super::*;

pub(crate) use super::entry::tests::Simulation;
pub(crate) use super::entry::tests::seed_inheritable_and_ambient;
pub(crate) use super::fd::tests::{create_fifo, set_descriptor_limit};
#[cfg(target_arch = "x86_64")]
pub(crate) use super::kernel::tests::nice_of;
#[cfg(target_arch = "x86_64")]
pub(crate) use super::port::tests::PortBackend;
pub(crate) use super::process::tests::{
    BEFORE_LINUX_6_16, continue_traced, deny_system_call, fork_isolated_test, has_exited,
    isolate_process_test, spawn_child_without_exit_signal, spawn_traced_child,
};
#[cfg(target_arch = "x86_64")]
pub(crate) use super::process::tests::{deny_group_kill, exit_main_thread, forbid_system_calls};
pub(crate) use super::rooted::tests::{
    exchange_paths, restrict_new_entries, thread_umask, unshare_standard_descriptors,
    with_thread_umask,
};
pub(crate) use super::signal::tests::{default_sigpipe, has_child, reap_children_automatically};
pub(crate) use super::sock_diag::tests::{encode_inet_diag_done, encode_inet_diag_socket};
pub(crate) use super::spawn::tests::{
    CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH, CAP_SETGID, CAP_SETUID, CAP_SYS_ADMIN, holds_capability,
};
#[cfg(target_arch = "x86_64")]
pub(crate) use super::spawn::tests::{CAP_SYS_NICE, CAP_SYS_RESOURCE};

/// A stream whose writes fail with a chosen `errno`.
pub(crate) struct Failing(pub(crate) Errno);

impl Write for Failing {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::from(self.0))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn errno_of_takes_the_os_error_or_eio() {
    let error = io::Error::from_raw_os_error(Errno::ENOSPC as i32);
    assert_eq!(errno_of(&error), Errno::ENOSPC);
    assert_eq!(errno_of(&io::ErrorKind::WriteZero.into()), Errno::EIO);
}

#[test]
fn errors_are_described_by_their_errno_or_their_own_text() {
    let error = io::Error::from_raw_os_error(Errno::ENOSPC as i32);
    assert_eq!(describe(&error), Errno::ENOSPC.desc());
    assert_eq!(
        describe(&io::ErrorKind::UnexpectedEof.into()),
        "unexpected end of file"
    );
}

#[test]
fn report_concatenates_and_ignores_failures() {
    let mut stream = Vec::new();
    report(&mut stream, &[b"a: ", b"b", b"\n"]);
    assert_eq!(stream, b"a: b\n");
    report(&mut Failing(Errno::EIO), &[b"lost\n"]);
}

#[test]
fn print_returns_the_errno_of_a_failed_write() {
    let mut stream = Vec::new();
    assert_eq!(print(&mut stream, b"7\n"), Ok(()));
    assert_eq!(stream, b"7\n");
    assert_eq!(print(&mut Failing(Errno::EPIPE), b"7\n"), Err(Errno::EPIPE));
}

#[test]
fn explicit_close_reports_errno() {
    let descriptor = open(c"/dev/null", OpenFlags::O_RDONLY, Mode::empty()).unwrap();
    assert_eq!(descriptor.close(), Ok(()));
}

#[test]
fn descriptors_read_through_the_standard_io_traits() {
    use std::io::{BufRead, BufReader, Read};

    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), b"ab\ncd").unwrap();
    let path = path_c_string(file.path()).unwrap();
    let flags = OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC;
    let mut reader = BufReader::new(open(&path, flags, Mode::empty()).unwrap());
    let mut line = Vec::new();
    assert_eq!(reader.read_until(b'\n', &mut line).unwrap(), 3);
    assert_eq!(line, b"ab\n");
    let mut rest = Vec::new();
    assert_eq!(reader.read_to_end(&mut rest).unwrap(), 2);
    assert_eq!(rest, b"cd");

    let mut directory = open(c"/", flags, Mode::empty()).unwrap();
    let error = directory.read_to_end(&mut Vec::new()).unwrap_err();
    assert_eq!(errno_of(&error), Errno::EISDIR);
}

#[test]
fn truncate_and_sync_resize_a_file() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let path = std::ffi::CString::new(file.path().to_str().unwrap()).unwrap();
    let descriptor = open(&path, OpenFlags::O_RDWR, Mode::empty()).unwrap();
    assert_eq!(descriptor.truncate(8192), Ok(()));
    assert_eq!(descriptor.sync(), Ok(()));
    assert_eq!(std::fs::metadata(file.path()).unwrap().len(), 8192);
    let read_only = open(&path, OpenFlags::O_RDONLY, Mode::empty()).unwrap();
    assert!(read_only.truncate(0).is_err());
}

#[test]
fn mount_ids_tell_the_mounts_apart() {
    let directory = tempfile::tempdir().unwrap();
    let child = directory.path().join("child");
    std::fs::create_dir(&child).unwrap();
    let id = |path: &std::path::Path| mount_id(&path_c_string(path).unwrap());
    assert_eq!(id(&child).unwrap(), id(directory.path()).unwrap());
    let proc = id(std::path::Path::new("/proc")).unwrap();
    assert_ne!(proc, id(std::path::Path::new("/")).unwrap());
    assert_eq!(id(&child.join("missing")), Err(Errno::ENOENT));
}

#[test]
fn mount_ids_follow_the_calling_threads_descriptor_table() {
    let root = mount_id(c"/").unwrap();
    let proc = mount_id(c"/proc").unwrap();
    assert_ne!(root, proc);
    std::thread::spawn(move || {
        unshare_standard_descriptors().unwrap();
        assert_eq!(mount_id(c"/"), Ok(root));
        assert_eq!(mount_id(c"/proc"), Ok(proc));
    })
    .join()
    .unwrap();
}

#[test]
fn connected_datagrams_wait_for_and_receive_echoes() {
    let server = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let std::net::SocketAddr::V4(peer) = server.local_addr().unwrap() else {
        panic!("an IPv4 server");
    };
    let client = connect_datagram(peer).unwrap();
    assert_eq!(client.wait_readable(1), Ok(false));
    assert_eq!(client.write(b"echo"), Ok(4));
    let mut buffer = [0; 8];
    let (length, source) = server.recv_from(&mut buffer).unwrap();
    assert_eq!(&buffer[..length], b"echo");
    server.send_to(b"back", source).unwrap();
    assert_eq!(client.wait_readable(5_000), Ok(true));
    assert_eq!(client.read(&mut buffer), Ok(4));
    assert_eq!(&buffer[..4], b"back");
}
