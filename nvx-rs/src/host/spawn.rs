//! The detached spawn of a VMM, with an explicit allowlist of the descriptors or handles of the
//! caller that it inherits, and the child that the spawn returns until its caller releases it.

use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
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

/// How [`spawn`] starts a VMM: its executable, arguments, and working directory, whether it
/// leaves the caller's job object on Windows, and the descriptors or handles of the caller that it
/// inherits besides its standard streams.
#[derive(Debug, Clone)]
pub struct VmmCommand<'a> {
    /// The VMM's executable.
    pub program: PathBuf,
    /// The VMM's arguments, after the name of its executable.
    pub arguments: Vec<OsString>,
    /// The VMM's working directory, which [`vmm_working_dir`] chooses for the VMM of a sandbox
    /// store.
    ///
    /// Windows permits at most 258 UTF-16 code units in its absolute path, including any verbatim
    /// prefix and trailing separator, which a store's paths may exceed.
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

/// Returns the working directory in which to start the VMM of the sandbox store `store`: on
/// Windows, the system directory, such as `C:\Windows\System32`, and on other hosts, the store.
///
/// Windows starts no process in a directory whose absolute path exceeds 258 UTF-16 code units,
/// whatever its prefix and whether the host enables long paths, as [`spawn`] refuses one, while a
/// store's paths may be longer. OpenVMM uses its working directory only to resolve the relative
/// paths of its arguments, and the launches of instances, template captures, and clones give it
/// absolute ones, so a VMM on Windows starts in the system directory, which every host has, whose
/// path is short, and which only administrators may change: a relative path that a caller lets
/// through names no file that another user could have placed there, and OpenVMM cannot create a
/// file there. On other hosts, which have no such limit, the VMM starts in its store.
///
/// Fails on Windows as `GetSystemDirectoryW` fails.
pub fn vmm_working_dir(store: &Path) -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        let _ = store;
        platform::system_directory()
    }
    #[cfg(not(windows))]
    Ok(store.to_owned())
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
/// if the VMM has none: see [the store](crate::host::store#the-log-claim).
///
/// Record the VMM's identity right after the spawn, with
/// [`Launch::record_process`](crate::host::store::Launch::record_process), as
/// [`launch`](crate::host::supervisor::launch) does. Fails with [`io::ErrorKind::InvalidInput`] on
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
/// On Windows, give each VMM a log file of its own, created anew, as a store's launch does. The
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
    spawn_inner(command, capability, log, false)
}

/// Starts a library VM only with a trusted log-named job on Windows.
pub(crate) fn spawn_vm(
    command: &VmmCommand<'_>,
    capability: &[u8; CAPABILITY_LEN],
    log: File,
) -> io::Result<Launched> {
    spawn_inner(command, capability, log, true)
}

/// Shares the legacy spawn mechanics without its optional Windows log-job fallback for a VM.
fn spawn_inner(
    command: &VmmCommand<'_>,
    capability: &[u8; CAPABILITY_LEN],
    log: File,
    require_log_job: bool,
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
        platform::spawn_detached(
            &command.program,
            &command.arguments,
            &command.working_dir,
            [stdin.into(), log.into(), stderr.into()],
            &command.inherit,
            command.breakaway_from_job,
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
        platform::detach(&mut child, &command.inherit)?;
        child.spawn().map(|child| Launched {
            child,
            reaped: false,
        })
    }
}

/// A VMM that [`spawn`] started, which the caller has not released yet: on Unix, its child
/// process; on Windows, its process handle. On Unix the child's ID names no other process until
/// [`wait_timeout`](Self::wait_timeout) reaps it; hold it with [`process`](Self::process) before
/// waiting to keep a verified handle after it exits.
///
/// Record it with [`Launch::record_process`](crate::host::store::Launch::record_process), or hold
/// it with [`process`](Self::process). Then release it with [`detach`](Self::detach), or end it
/// with [`kill_and_wait`](Self::kill_and_wait); the [`Process`] stays valid either way.
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
    /// [`detach`](Self::detach) or [`kill_and_wait`](Self::kill_and_wait) releases this
    /// `Launched`. A process that exited already keeps its identity, and the `Process` reports
    /// that it exited.
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
    /// Unlike legacy cleanup's timeout, this never detaches a child after a failed verification.
    pub(crate) fn kill_and_confirm_exit(self) -> io::Result<()> {
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

    /// Asks the process to end, as `Popen.terminate` does: with `SIGTERM` on Unix, and by
    /// terminating it with status 1 on Windows. A process that has exited is left alone.
    #[cfg(feature = "cli")]
    pub(crate) fn terminate(&mut self) -> io::Result<()> {
        #[cfg(windows)]
        return self.process.request_termination();
        #[cfg(not(windows))]
        self.signal(nix::sys::signal::Signal::SIGTERM)
    }

    /// Ends the process, as `Popen.kill` does: with `SIGKILL` on Unix, and by terminating it with
    /// status 1 on Windows. A process that has exited is left alone.
    #[cfg(feature = "cli")]
    pub(crate) fn kill(&mut self) -> io::Result<()> {
        #[cfg(windows)]
        return self.process.request_termination();
        #[cfg(not(windows))]
        self.signal(nix::sys::signal::Signal::SIGKILL)
    }

    /// Sends `signal` to the process unless it has exited. The child is not reaped until it is
    /// waited for, so its process ID cannot name another process meanwhile.
    #[cfg(all(feature = "cli", not(windows)))]
    fn signal(&mut self, signal: nix::sys::signal::Signal) -> io::Result<()> {
        if self.try_reap()?.is_some() {
            return Ok(());
        }
        let pid = i32::try_from(self.child.id()).map_err(io::Error::other)?;
        match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(error) => Err(error.into()),
        }
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
