//! The boot consoles of the host program's clones: what a clone's guest writes to its kernel
//! console once restored, such as a panic, an oops, or an out-of-memory kill, which the host
//! program reads, so that a clone that fails, before or after it is ready, leaves a record of why.
//!
//! A template captures a boot console that discards its output, so that its capture waits for no
//! reader. NVX's clone of it, whose plan asks for one with `ClonePlan::with_boot_console`, as
//! `runtime-adapter-host start --boot-console listen` asks, attaches a listener to that console at
//! the endpoint that the clone's lease fixes, `nvx::host::template::Store::clone_console_endpoint`:
//! a Unix socket beside the template's snapshot on Linux, and a named pipe on Windows. OpenVMM
//! discards what the guest writes there while no reader is connected, so the clone never waits for
//! a reader: a reader gets what the guest writes once it connected, and the clone runs on once it
//! disconnected, as once its reader crashed.
//!
//! `runtime-adapter-host console` [`connect`]s to the console through the identity of the clone's
//! VMM, as to its control endpoint, with `nvx::host::session::connect`, which refuses an endpoint
//! that another process serves, and reads it from then on, with [`read_marker`], while it
//! reattaches to the clone, which ends a clone whose guest does not answer, and asks the guest to
//! write a marker to its kernel's log, until what it read holds the marker, whatever the reattach
//! and the guest did. Under the sandbox's lock, it appends what it read to the sandbox's console
//! log, `console.log`, the file that `nvx::host::store::SandboxStore::console_path` names, which it
//! opens with [`open_log`], and which each launch in the sandbox starts anew. It appends what it
//! read even when the reattach ended the clone, or failed, or the guest did not answer, or the
//! console ended, or its deadline passed, before the marker, so that the log keeps what a clone
//! that failed wrote last.

#[cfg(test)]
mod tests;

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use std::{panic, thread};

use nvx::host::platform::Transport;
use nvx::host::process::{Process, ProcessRef};
use nvx::host::session::{self, SessionError};

/// The bytes that a read of the console takes at most.
const CHUNK: usize = 4096;

/// The mode of a console log, private to the current user, as the files of the sandbox's store
/// are.
#[cfg(unix)]
const LOG_MODE: u32 = 0o600;

/// How long a reader that reads while the guest is asked for its marker waits for the console at a
/// time before it checks whether the guest answered.
const POLL: Duration = Duration::from_millis(100);

/// Connects to the boot console of a clone at `endpoint`, which its VMM, the process `vmm`,
/// serves, retrying until `deadline` while the endpoint is not yet there and the VMM runs, as
/// `nvx::host::session::connect` does.
///
/// It first checks that `vmm` names a process that runs, with `Process::open`, which takes a
/// reference from another boot for one that exited: `session::connect` checks the ID and the start
/// time of the process that serves the endpoint, which on Linux counts from the boot, so that a
/// process of this boot may share both with a VMM of an earlier one.
///
/// Fails with [`io::ErrorKind::NotConnected`] if the VMM has exited, or exits before the console
/// serves a reader, with [`io::ErrorKind::PermissionDenied`] if another process serves the
/// endpoint, with [`io::ErrorKind::TimedOut`] if nothing serves it by `deadline`, as for a clone
/// whose launch attached no boot console that its caller reads, and as the check of the VMM or
/// the connection fails otherwise.
pub fn connect(
    endpoint: &str,
    vmm: &ProcessRef,
    deadline: Instant,
) -> io::Result<Box<dyn Transport>> {
    let exited = || {
        io::Error::new(
            io::ErrorKind::NotConnected,
            "the clone's VMM exited before its boot console served a reader",
        )
    };
    if Process::open(vmm)?.is_none() {
        return Err(exited());
    }
    session::connect(endpoint, vmm.pid, vmm.start_time, deadline).map_err(|error| match error {
        SessionError::Refused(error) | SessionError::Io(error) => error,
        SessionError::TimedOut => io::Error::new(
            io::ErrorKind::TimedOut,
            format!("nothing serves the boot console {endpoint}"),
        ),
        SessionError::ProcessExited => exited(),
        error => io::Error::other(error.to_string()),
    })
}

/// Opens the console log at `path` for appending, creating it, private to the current user, if
/// it does not exist, as the files of the sandbox's store are. On Unix, the umask may clear bits
/// of the mode that the log is created with, even the current user's permission to write it, so
/// the mode is set again through the new log.
///
/// It refuses a log that is not a regular file of its own, as the store refuses such files: a
/// link, which the open does not follow, nor on Windows any other reparse point, or a file that a
/// link replaced once it was opened. On Linux, the open does not wait, as it would for a FIFO
/// without a reader: a log that is not a regular file is refused before anything is written to
/// it.
pub fn open_log(path: &Path) -> io::Result<File> {
    // A new log is created where its path is, never through a link, which `create_new` refuses.
    let created = append_options().create_new(true).open(path);
    let file = match created {
        Ok(file) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                file.set_permissions(std::fs::Permissions::from_mode(LOG_MODE))?;
            }
            file
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let existing = append_options().open(path);
            existing.map_err(|error| refused(path, error))?
        }
        Err(error) => return Err(error),
    };
    check_log(path, &file)?;
    Ok(file)
}

/// Returns the options of the opens of a console log: for appending, private to the current
/// user on Unix, of the file itself, rather than of a link's target, without waiting for a FIFO's
/// reader, on Linux, and of the reparse point itself, rather than of its target, on Windows.
fn append_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, LOG_MODE);
    // Appends to a regular file do not depend on `O_NONBLOCK`.
    #[cfg(target_os = "linux")]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        (nix::fcntl::OFlag::O_NOFOLLOW | nix::fcntl::OFlag::O_NONBLOCK).bits(),
    );
    #[cfg(windows)]
    {
        /// Opens a reparse point itself.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        std::os::windows::fs::OpenOptionsExt::custom_flags(
            &mut options,
            FILE_FLAG_OPEN_REPARSE_POINT,
        );
    }
    options
}

/// Returns `error`, with which the open of the existing console log at `path` failed, as the
/// refusal of a log that is not a regular file if the open refused one: on Linux, a link, which
/// `O_NOFOLLOW` refuses, or a FIFO without a reader, or a socket, which an open that does not
/// wait refuses.
fn refused(path: &Path, error: io::Error) -> io::Error {
    #[cfg(target_os = "linux")]
    if [nix::errno::Errno::ELOOP, nix::errno::Errno::ENXIO]
        .into_iter()
        .any(|errno| error.raw_os_error() == Some(errno as i32))
    {
        return not_regular(path);
    }
    let _ = path;
    error
}

/// Returns the error of a console log at `path` that is not a regular file of the sandbox's
/// store.
fn not_regular(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "the console log {} is not a regular file of the sandbox's store",
            path.display()
        ),
    )
}

/// Refuses the console log `file`, which the open of `path` returned, unless it is a regular file
/// that no link names: on Unix, the open followed a link, so the file must be the one that `path`
/// names itself; on Windows, the open opened a reparse point itself, which the log must not be.
fn check_log(path: &Path, file: &File) -> io::Result<()> {
    let opened = file.metadata()?;
    #[cfg(unix)]
    let own = {
        use std::os::unix::fs::MetadataExt;

        let named = std::fs::symlink_metadata(path)?;
        !named.file_type().is_symlink()
            && (named.dev(), named.ino()) == (opened.dev(), opened.ino())
    };
    #[cfg(windows)]
    let own = {
        use std::os::windows::fs::MetadataExt;

        /// The attribute of a reparse point.
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        let _ = path;
        opened.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
    };
    #[cfg(not(any(unix, windows)))]
    let own = {
        let _ = path;
        true
    };
    if opened.is_file() && own {
        Ok(())
    } else {
        Err(not_regular(path))
    }
}

/// Runs `ask`, which asks the guest to write `marker` to its kernel's log, among what else it
/// does, such as a reattach to the clone, while it reads the console `console`, until `ask`
/// returned and the console delivered the marker, or for at most `timeout` once `ask` returned,
/// whatever `ask` did: a guest whose kernel panicked, or killed its agent, once the reader
/// connected answers nothing, but its console holds why. The reader reads for as long as `ask`
/// runs, since a console whose output its reader leaves unread holds the guest's writes to it
/// back, which could keep the guest from answering. The marker is text that the guest writes only
/// when asked, such as text unique to the read: the first that the console delivers is the one
/// that `ask` asked for. Returns what `ask` returned, all that the reader read, and how the read
/// ended. A panic of `ask` ends the read at once, and then goes on.
pub fn read_marker<T>(
    console: &mut dyn Transport,
    ask: impl FnOnce() -> T,
    marker: &[u8],
    timeout: Duration,
) -> (T, Vec<u8>, io::Result<()>) {
    // Once `ask` returned, by when the console must have delivered the marker.
    let answered: Mutex<Option<Instant>> = Mutex::new(None);
    let limit = || *answered.lock().unwrap_or_else(PoisonError::into_inner);
    thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut read = Vec::new();
            let mut from = 0;
            let mut found = false;
            let delivered = loop {
                let limit = limit();
                if found && limit.is_some() {
                    break Ok(());
                }
                let deadline = limit.unwrap_or_else(|| Instant::now() + POLL);
                if found {
                    // The console delivered the marker, whatever ends it then.
                    if drain(console, deadline, &mut read).is_err() {
                        break Ok(());
                    }
                    continue;
                }
                match search(console, marker, deadline, &mut read, &mut from) {
                    Ok(()) => found = true,
                    Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                        if limit.is_some() {
                            break Err(error);
                        }
                    }
                    Err(error) => break Err(error),
                }
            };
            (read, delivered)
        });
        // The reader, which reads until the console ends otherwise, stops at once when `ask`
        // panicked, so that the panic goes on.
        let asked = panic::catch_unwind(panic::AssertUnwindSafe(ask));
        let left = if asked.is_ok() {
            timeout
        } else {
            Duration::ZERO
        };
        *answered.lock().unwrap_or_else(PoisonError::into_inner) = Some(Instant::now() + left);
        let (read, delivered) = reader.join().unwrap_or_else(|_| {
            (
                Vec::new(),
                Err(io::Error::other("the reader of the console panicked")),
            )
        });
        let asked = asked.unwrap_or_else(|panic| panic::resume_unwind(panic));
        (asked, read, delivered)
    })
}

/// Reads the console `console` until what it read holds `marker`, appending what it reads to
/// `read`, which may already hold what an earlier read returned.
///
/// Fails with [`io::ErrorKind::TimedOut`] if `deadline` passes first, with
/// [`io::ErrorKind::UnexpectedEof`] if the console ends first, as when the clone's VMM exited,
/// and as a read fails otherwise; `read` then holds what the console delivered before.
pub fn read_until(
    console: &mut dyn Transport,
    marker: &[u8],
    deadline: Instant,
    read: &mut Vec<u8>,
) -> io::Result<()> {
    search(console, marker, deadline, read, &mut 0)
}

/// Reads the console `console` until what it read holds `marker`, as [`read_until`] does, where
/// no marker starts before `from` in what `read` holds, which it moves on as it searches. It
/// searches each read's bytes once, with the end of the bytes before them that a marker spanning
/// reads starts in.
fn search(
    console: &mut dyn Transport,
    marker: &[u8],
    deadline: Instant,
    read: &mut Vec<u8>,
    from: &mut usize,
) -> io::Result<()> {
    let mut buffer = [0; CHUNK];
    loop {
        if holds(&read[*from..], marker) {
            return Ok(());
        }
        *from = read.len().saturating_sub(marker.len().saturating_sub(1));
        let left = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "the boot console did not deliver {} by its deadline, after {} bytes",
                        marker.escape_ascii(),
                        read.len()
                    ),
                )
            })?;
        match console.read(&mut buffer, Some(left)) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "the boot console ended before it delivered {}, after {} bytes",
                        marker.escape_ascii(),
                        read.len()
                    ),
                ));
            }
            Ok(count) => read.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
            Err(error) => return Err(error),
        }
    }
}

/// Reads the console `console` until `deadline`, appending what it reads to `read`.
///
/// Fails with [`io::ErrorKind::UnexpectedEof`] if the console ends first, and as a read fails
/// otherwise.
fn drain(console: &mut dyn Transport, deadline: Instant, read: &mut Vec<u8>) -> io::Result<()> {
    let mut buffer = [0; CHUNK];
    while let Some(left) = deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
    {
        match console.read(&mut buffer, Some(left)) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => read.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Returns whether `read` holds `marker`.
fn holds(read: &[u8], marker: &[u8]) -> bool {
    marker.is_empty() || read.windows(marker.len()).any(|window| window == marker)
}
