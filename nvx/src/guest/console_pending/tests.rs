use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use super::*;
use crate::guest::status::ExitStatus;
use crate::guest::status::tests::command_line;
use crate::guest::sys::tests::Failing;

fn console_pending(arguments: &[&[u8]]) -> (u8, Vec<u8>, Vec<u8>) {
    let args = command_line("nvx-console-pending", arguments);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = ExitStatus::from(run(&args, &mut out, &mut err));
    (status as u8, out, err)
}

fn message(operation: &str, path: &Path, errno: Errno) -> Vec<u8> {
    let mut message = format!("nvx-console-pending: {operation} ").into_bytes();
    message.extend_from_slice(path.as_os_str().as_bytes());
    message.extend_from_slice(b": ");
    message.extend_from_slice(errno.desc().as_bytes());
    message.push(b'\n');
    message
}

#[test]
fn usage_takes_exactly_one_device() {
    let usage = b"usage: nvx-console-pending DEVICE\n".to_vec();
    for arguments in [&[][..], &[&b"a"[..], b"b"]] {
        assert_eq!(console_pending(arguments), (2, Vec::new(), usage.clone()));
    }
}

#[test]
fn prints_the_bytes_waiting_in_a_fifo() {
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("console");
    let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(
        sys::tests::create_fifo(&path, Mode::S_IRUSR | Mode::S_IWUSR),
        Ok(())
    );
    let path = fifo.as_os_str().as_bytes();
    assert_eq!(console_pending(&[path]), (0, b"0\n".to_vec(), Vec::new()));

    // Opening a FIFO's reader without blocking lets the writer open it. The reader stays
    // open until the test ends, so the FIFO keeps the bytes.
    let _reader = fs::File::options()
        .read(true)
        .custom_flags(OpenFlags::O_NONBLOCK.bits())
        .open(&fifo)
        .unwrap();
    fs::write(&fifo, b"twelve bytes").unwrap();
    assert_eq!(console_pending(&[path]), (0, b"12\n".to_vec(), Vec::new()));
}

#[test]
fn prints_the_unread_size_of_a_regular_file() {
    let file = tempfile::NamedTempFile::new().unwrap();
    fs::write(file.path(), b"12345").unwrap();
    let path = file.path().as_os_str().as_bytes();
    assert_eq!(console_pending(&[path]), (0, b"5\n".to_vec(), Vec::new()));
}

#[test]
fn reports_failures_with_the_errno_description() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing");
    assert_eq!(
        console_pending(&[missing.as_os_str().as_bytes()]),
        (1, Vec::new(), message("open", &missing, Errno::ENOENT))
    );
    // A directory opens, but has no FIONREAD.
    assert_eq!(
        console_pending(&[directory.path().as_os_str().as_bytes()]),
        (
            1,
            Vec::new(),
            message("ioctl", directory.path(), Errno::ENOTTY)
        )
    );
}

#[test]
fn reports_a_failed_write() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let args = command_line("nvx-console-pending", &[file.path().as_os_str().as_bytes()]);
    let mut err = Vec::new();
    assert_eq!(
        ExitStatus::from(run(&args, &mut Failing(Errno::EIO), &mut err)),
        ExitStatus::Failure
    );
    let mut expected = b"nvx-console-pending: write: ".to_vec();
    expected.extend_from_slice(Errno::EIO.desc().as_bytes());
    expected.push(b'\n');
    assert_eq!(err, expected);
}
