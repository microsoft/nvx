//! The spawn of a VMM, with an explicit allowlist of the descriptors or handles of the
//! caller that it inherits, and the child that the spawn returns until its caller releases it.

use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
#[cfg(not(windows))]
use std::process::{Child, Command, Stdio};
#[cfg(not(windows))]
use std::thread;
use std::time::Duration;
#[cfg(not(windows))]
use std::time::Instant;

use crate::host::platform;
use crate::host::platform::inherit::Inherited;
#[cfg(not(windows))]
use crate::host::process::POLL_INTERVAL;
use crate::host::process::{BootId, KILL_TIMEOUT, Process, ProcessRef};
use crate::protocol::CAPABILITY_LEN;

/// How [`spawn`] or [`spawn_vm`] starts a VMM: its executable, arguments, and working directory,
/// whether it leaves the caller's job object on Windows, and the descriptors or handles of the
/// caller that it inherits besides its standard streams.
#[derive(Debug, Clone)]
pub struct VmmCommand<'a> {
    /// The VMM's executable.
    pub program: PathBuf,
    /// The VMM's arguments, after the name of its executable.
    pub arguments: Vec<OsString>,
    /// The VMM's working directory.
    ///
    /// Windows permits at most 258 UTF-16 code units in its absolute path, including any verbatim
    /// prefix and trailing separator. It may differ from the directories of the files that the
    /// VMM uses, whose paths can be longer.
    pub working_dir: PathBuf,
    /// Whether the VMM starts outside the caller's job object on Windows, which must permit
    /// breakaway, so that the VMM survives the job. Other hosts ignore it.
    pub breakaway_from_job: bool,
    /// The descriptors or handles of the caller that the VMM inherits under their numbers in the
    /// caller, besides its standard streams. One that the list names twice is inherited once.
    pub inherit: Vec<Inherited<'a>>,
}

impl VmmCommand<'_> {
    /// Returns the command that starts `program` with `arguments` in `working_dir`, inside the
    /// caller's job object, and with nothing of the caller's besides its standard streams.
    pub fn new(
        program: impl Into<PathBuf>,
        arguments: Vec<OsString>,
        working_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            program: program.into(),
            arguments,
            working_dir: working_dir.into(),
            breakaway_from_job: false,
            inherit: Vec::new(),
        }
    }
}

/// Starts the VMM that `command` describes, detached from the caller, with `capability` on its
/// standard input and `log` as its standard output and error.
///
/// OpenVMM reads its capability from standard input as soon as it starts, so the capability is
/// written into a pipe whose write end is closed before the spawn: it never appears in arguments,
/// the environment, or the log. The VMM inherits its standard streams and the descriptors or
/// handles that `command` allows, as [`Inherited`] describes, and nothing else of the caller's,
/// so a caller whose own output is a pipe sees its end when the caller exits rather than when the
/// VM does. On Linux, the VMM starts in a new session, so that it outlives the caller and its
/// terminal, and with the caller's umask without the owner's bits, so that the files that OpenVMM
/// creates for the caller, such as its outcome report, keep the owner's permissions that OpenVMM
/// gives them whatever the caller's umask, outside directories with a default ACL, which Linux
/// applies instead of the umask; on Windows, without a console window, and outside the caller's
/// job object when `command` asks for it, in a job object of its own that is named after `log`,
/// which limits no resource but lets no process that the VMM starts break away from it, unless
/// `log` is not a file of a disk volume, such as `NUL` or a pipe, or what takes that name is not
/// the user's job. A process that the caller spawns while the VMM is created, which can inherit
/// the log as [`Inherited`] describes, never joins the VMM's job, so it holds the log's claim only
/// if the VMM has none.
///
/// Record the VMM's identity right after the spawn, through [`Launched::process`], if the caller
/// recovers its launches. Fails with [`io::ErrorKind::InvalidInput`] on
/// Linux for a descriptor of a standard stream, 0 to 2, or one that is not close-on-exec, and on
/// Windows for a working directory whose absolute path exceeds 258 UTF-16 code units, including
/// any verbatim prefix and trailing separator, or for a batch file, which Windows would run
/// through `cmd.exe` with arguments that it reads as shell syntax, and with
/// [`io::ErrorKind::AlreadyExists`] on Windows
/// if a job of the user takes
/// the name of the job of `log` already, as described below; and with
/// [`io::ErrorKind::Unsupported`] for any descriptor or handle on hosts other than Linux and
/// Windows.
///
/// On Windows, give each VMM a log file of its own, created anew, as
/// [`vm::create`](crate::host::vm::create) does. The
/// job of `log` is named after the file's identity, which the file keeps when it is truncated, as
/// [`File::create`](std::fs::File::create) truncates an existing file, and its name stays taken
/// while another spawn with the file holds the job, or while a process that inherited the job's
/// handle in the window of such a spawn still runs, as one spawned while an earlier VMM of the
/// file started may: a spawn with the file fails until then.
pub fn spawn(
    command: &VmmCommand<'_>,
    capability: &[u8; CAPABILITY_LEN],
    log: File,
) -> io::Result<Launched> {
    spawn_inner(command, capability, log, false, false)
}

/// Starts a VMM with the strict launch contract of [`crate::host::vm::VmmCommand`].
///
/// Uses [`spawn`]'s standard streams, inheritance allowlist, detachment, and process lifetime.
/// Unlike [`spawn`], on Windows this requires a trusted log-named job: a log that is not a file
/// of a disk volume, or an untrusted object or object-manager link under its job name, fails with
/// [`io::ErrorKind::PermissionDenied`] before any child is created. A job of the user already
/// holding that name fails with [`io::ErrorKind::AlreadyExists`], as in [`spawn`].
///
/// On other hosts this has the same behavior and errors as [`spawn`]. The caller retains its
/// owned inherited resources until the spawn returns, and keeps or ends the returned child as
/// [`Launched`] describes.
pub fn spawn_vm(
    command: &VmmCommand<'_>,
    capability: &[u8; CAPABILITY_LEN],
    log: File,
) -> io::Result<Launched> {
    spawn_vm_with_mode(command, capability, log, false)
}

/// Shares the strict VM spawn with the CLI's private foreground mode.
pub(crate) fn spawn_vm_with_mode(
    command: &VmmCommand<'_>,
    capability: &[u8; CAPABILITY_LEN],
    log: File,
    foreground: bool,
) -> io::Result<Launched> {
    spawn_inner(command, capability, log, true, foreground)
}

/// Shares the legacy spawn mechanics without its optional Windows log-job fallback for a VM.
fn spawn_inner(
    command: &VmmCommand<'_>,
    capability: &[u8; CAPABILITY_LEN],
    log: File,
    require_log_job: bool,
    foreground: bool,
) -> io::Result<Launched> {
    #[cfg(not(windows))]
    let _ = require_log_job;
    let (stdin, mut writer) = io::pipe()?;
    writer.write_all(capability)?;
    drop(writer);
    let stderr = log.try_clone()?;
    #[cfg(windows)]
    {
        let job = platform::create_log_job(&log)?;
        if require_log_job && job.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the VM log has no trusted log-named job",
            ));
        }
        let mode = if foreground {
            platform::SpawnMode::Foreground
        } else {
            platform::SpawnMode::Detached {
                breakaway_from_job: command.breakaway_from_job,
            }
        };
        platform::spawn_with_stdio(
            &command.program,
            &command.arguments,
            &command.working_dir,
            [stdin.into(), log.into(), stderr.into()],
            &command.inherit,
            mode,
            job,
        )
        .map(|process| Launched { process })
    }
    #[cfg(not(windows))]
    {
        // Rust opens its own descriptors close-on-exec, and `platform::detach` marks any other
        // descriptor of the caller, except those that `command` allows, so OpenVMM receives only
        // these and its standard streams.
        let mut child = Command::new(&command.program);
        child
            .args(&command.arguments)
            .current_dir(&command.working_dir)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        #[cfg(feature = "cli")]
        if foreground {
            platform::inherit_only(&mut child, &command.inherit)?;
        } else {
            platform::detach(&mut child, &command.inherit)?;
        }
        #[cfg(not(feature = "cli"))]
        {
            let _ = foreground;
            platform::detach(&mut child, &command.inherit)?;
        }
        child.spawn().map(|child| Launched {
            child,
            reaped: false,
        })
    }
}

/// A VMM that [`spawn`] or [`spawn_vm`] started, which the caller has not released yet: on Unix,
/// its child process; on Windows, its process handle. On Unix the child's ID names no other
/// process until [`wait_timeout`](Self::wait_timeout) reaps it; hold it with
/// [`process`](Self::process) before waiting to keep a verified handle after it exits.
///
/// Hold it with [`process`](Self::process). Then release it with [`detach`](Self::detach), or end it
/// with [`kill_and_confirm_exit`](Self::kill_and_confirm_exit); the [`Process`] stays valid
/// either way. [`kill_and_wait`](Self::kill_and_wait) provides legacy, time-bounded cleanup.
pub struct Launched {
    #[cfg(not(windows))]
    /// Child process retained until it is detached or terminated.
    child: Child,
    #[cfg(not(windows))]
    /// Whether the child has been reaped, after which its process ID may name another process.
    reaped: bool,
    #[cfg(windows)]
    /// Owned Windows process handle and identifier.
    process: platform::LaunchedProcess,
}

impl fmt::Debug for Launched {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Launched")
            .field("id", &self.id())
            .finish_non_exhaustive()
    }
}

impl Launched {
    /// Returns the process ID.
    pub fn id(&self) -> u32 {
        #[cfg(not(windows))]
        return self.child.id();
        #[cfg(windows)]
        return self.process.id();
    }

    /// Returns a [`Process`] that holds the launched process through a handle of its own.
    ///
    /// The caller has not reaped the process, so its ID and start time still identify it, and
    /// the reference of the `Process` is the one to record: a successor of the caller can find
    /// the process again with [`Process::open`]. The `Process` stays valid after
    /// [`detach`](Self::detach), [`kill_and_confirm_exit`](Self::kill_and_confirm_exit), or
    /// [`kill_and_wait`](Self::kill_and_wait) releases this `Launched`. A process that exited
    /// already keeps its identity, and the `Process` reports that it exited.
    ///
    /// Fails if the identity of the process or of the current boot cannot be read, and with
    /// [`io::ErrorKind::Unsupported`] on hosts other than Linux and Windows.
    pub fn process(&self) -> io::Result<Process> {
        #[cfg(windows)]
        let (handle, start_time) = self.process.pin()?;
        #[cfg(not(windows))]
        let (handle, start_time) = if self.reaped {
            return Err(io::Error::other(format!(
                "launched process {} was reaped, so its ID may name another process",
                self.id()
            )));
        } else {
            platform::pin_child(self.id())?
        };
        let reference = ProcessRef {
            pid: self.id(),
            start_time,
            boot: BootId::current()?,
        };
        Ok(Process::held(reference, handle))
    }

    /// Releases the process without waiting for it: it keeps running.
    ///
    /// On Unix a background thread reaps the child once it exits so it does not linger as a
    /// zombie while this process lives. If that thread cannot start, the child becomes a zombie
    /// after it exits; liveness checks treat zombies without threads as exited, so only the
    /// process table entry leaks.
    pub fn detach(self) {
        #[cfg(not(windows))]
        {
            let mut child = self.child;
            let _ = thread::Builder::new()
                .name("nvx-openvmm-reaper".to_owned())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
        #[cfg(windows)]
        drop(self);
    }

    /// Ends the process through its handle, which cannot race with the reuse of its ID, and
    /// returns whether it exited within ten seconds. A process that did not is released as
    /// [`detach`](Self::detach) releases it.
    pub fn kill_and_wait(self) -> bool {
        #[cfg(windows)]
        return self.process.terminate(KILL_TIMEOUT);
        #[cfg(not(windows))]
        {
            let mut child = self.child;
            if child.kill().is_ok() {
                let deadline = Instant::now() + KILL_TIMEOUT;
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => return true,
                        Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
                        _ => break,
                    }
                }
            }
            Self {
                child,
                reaped: self.reaped,
            }
            .detach();
            false
        }
    }

    /// Terminates this owned child and waits without a deadline for proof of its exit.
    ///
    /// Unlike [`kill_and_wait`](Self::kill_and_wait), this has no timeout and never hands off a
    /// child to a background reaper. On Unix a successful call reaps the child. Even if
    /// termination fails, it waits for the child's exit; a child that exited already succeeds.
    /// Use this when setup fails after [`spawn_vm`] created a child, including when
    /// [`process`](Self::process) cannot verify its identity.
    ///
    /// Fails if waiting cannot confirm exit, retaining both failures if termination also failed.
    pub fn kill_and_confirm_exit(self) -> io::Result<()> {
        #[cfg(windows)]
        return self.process.terminate_and_wait();
        #[cfg(not(windows))]
        {
            let mut child = self.child;
            let termination = child.kill();
            // A refused signal needs no repair when the wait proves the child already exited.
            child.wait().map(|_| ()).map_err(|wait| match termination {
                Ok(()) => wait,
                Err(termination) => io::Error::other(format!(
                    "cannot terminate the child: {termination}; cannot wait for its exit: {wait}"
                )),
            })
        }
    }

    /// Waits up to `timeout` for the process to exit, and returns its status if it has, as
    /// `Popen.wait` and `Popen.poll` report it: the exit code, or on Unix the negated number of
    /// the signal that ended the process.
    ///
    /// On Unix a successful wait reaps the child; call [`process`](Self::process) before it to
    /// retain a verified process handle. Later waits return the same status.
    pub fn wait_timeout(&mut self, timeout: Duration) -> io::Result<Option<i64>> {
        #[cfg(windows)]
        return Ok(self.process.wait_timeout(timeout)?.map(i64::from));
        #[cfg(not(windows))]
        {
            let deadline = Instant::now().checked_add(timeout);
            loop {
                if let Some(status) = self.try_reap()? {
                    return Ok(Some(status_value(status)));
                }
                let now = Instant::now();
                match deadline {
                    Some(deadline) if now >= deadline => return Ok(None),
                    Some(deadline) => thread::sleep((deadline - now).min(POLL_INTERVAL)),
                    None => thread::sleep(POLL_INTERVAL),
                }
            }
        }
    }

    /// Reaps the child if it has exited, and returns its status if it has.
    #[cfg(not(windows))]
    fn try_reap(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        let status = self.child.try_wait()?;
        self.reaped |= status.is_some();
        Ok(status)
    }
}

/// Returns `Popen.returncode` for `status`: the exit code, or the negated signal number.
#[cfg(not(windows))]
fn status_value(status: std::process::ExitStatus) -> i64 {
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;

    #[cfg(unix)]
    let signal = status.signal();
    #[cfg(not(unix))]
    let signal: Option<i32> = None;

    match (status.code(), signal) {
        (Some(code), _) => i64::from(code),
        (None, Some(signal)) => -i64::from(signal),
        (None, None) => 0,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
