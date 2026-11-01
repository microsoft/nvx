use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::PathBuf;

use nix::pty::{self, PtyMaster};

use super::*;

/// Opens the master end of a pseudo-terminal, closed on `exec` so that no process that
/// another test starts holds it, and returns it with the path of its terminal, which stays
/// locked, and fails to open with `EIO`, until [`pty::unlockpt`] unlocks it.
fn locked_pseudo_terminal() -> (PtyMaster, PathBuf) {
    let flags = OpenFlags::O_RDWR | OpenFlags::O_NOCTTY | OpenFlags::O_CLOEXEC;
    let master = pty::posix_openpt(flags).unwrap();
    pty::grantpt(&master).unwrap();
    let path = PathBuf::from(pty::ptsname_r(&master).unwrap());
    (master, path)
}

#[test]
fn a_terminal_that_the_kernel_is_setting_up_opens_once_it_is_set_up() {
    let (master, path) = locked_pseudo_terminal();
    let opening = thread::spawn(move || Terminal::open(&path));
    thread::sleep(Duration::from_millis(200));
    assert!(!opening.is_finished(), "a locked terminal opened");
    pty::unlockpt(&master).unwrap();
    let terminal = opening.join().unwrap().unwrap();
    terminal.configure_raw().unwrap();
}

#[test]
fn opening_fails_at_once_for_other_errors_and_after_its_attempts() {
    let error = Terminal::open(Path::new("/")).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(Errno::EISDIR as i32), "{error}");
    let error = Terminal::open(Path::new("/dev/\0null")).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(Errno::EINVAL as i32), "{error}");
    let missing = c"/nonexistent/nvx-terminal";
    let error = open_with(missing, 3, Duration::from_millis(1)).unwrap_err();
    assert_eq!(error, Errno::ETIMEDOUT);
    assert_eq!(io::Error::from(error).kind(), io::ErrorKind::TimedOut);
}

#[test]
fn a_file_that_is_no_terminal_cannot_be_put_in_raw_mode() {
    let terminal = Terminal::open(Path::new("/dev/null")).unwrap();
    let error = terminal.configure_raw().unwrap_err();
    assert_eq!(error.raw_os_error(), Some(Errno::ENOTTY as i32), "{error}");
}

#[test]
fn a_terminal_in_raw_mode_passes_every_byte_as_it_is() {
    let (master, path) = locked_pseudo_terminal();
    pty::unlockpt(&master).unwrap();
    let master = FileDescriptor::from_owned(OwnedFd::from(master));
    let mut terminal = Terminal::open(&path).unwrap();
    terminal.configure_raw().unwrap();
    // Every byte value, which a terminal that was not in raw mode would translate, echo, or
    // act on: carriage returns and newlines, and the characters of signals, flow control,
    // line editing, and the end of a file.
    let forward = (0..=u8::MAX).collect::<Vec<_>>();
    let backward = forward.iter().rev().copied().collect::<Vec<_>>();
    master.write_all(&forward).unwrap();
    let mut received = vec![0; forward.len()];
    terminal.read_exact(&mut received).unwrap();
    assert_eq!(received, forward);
    // An echo of the bytes that the terminal received would arrive first.
    terminal.write_all(&backward).unwrap();
    terminal.drain().unwrap();
    let mut sent = vec![0; backward.len()];
    master.read_exact(&mut sent).unwrap();
    assert_eq!(sent, backward);
}
