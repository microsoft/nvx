//! Safe process, signal, inherited-descriptor, and standard-stream operations.

use std::ffi::{CStr, CString};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};

use nix::sys::signal::{SigHandler, Signal, kill};
use nix::unistd::Pid;

use super::{Errno, FileDescriptor, Port};
use crate::guest::parse::parse_decimal;

/// Whether this process inherited an ignored SIGPIPE across `exec`.
static SIGPIPE_IGNORED: AtomicBool = AtomicBool::new(false);

/// Runs before Rust's standard runtime changes SIGPIPE to `SIG_IGN`.
#[used]
#[unsafe(link_section = ".init_array")]
static CAPTURE_SIGPIPE: extern "C" fn() = capture_sigpipe;

/// Records the inherited SIGPIPE disposition before Rust startup, and leaves it in place.
extern "C" fn capture_sigpipe() {
    // SAFETY: `SIG_IGN` installs no handler. Setting it returns the previous disposition, which
    // the second call restores before another constructor runs.
    let Ok(inherited) = (unsafe { nix::sys::signal::signal(Signal::SIGPIPE, SigHandler::SigIgn) })
    else {
        return;
    };
    // SAFETY: `inherited` is the disposition that the preceding call returned.
    let _ = unsafe { nix::sys::signal::signal(Signal::SIGPIPE, inherited) };
    SIGPIPE_IGNORED.store(matches!(inherited, SigHandler::SigIgn), Ordering::Relaxed);
}

/// Restores the SIGPIPE disposition inherited across `exec`.
///
/// POSIX preserves `SIG_IGN` across `exec` and resets caught handlers to `SIG_DFL`. Rust's
/// standard runtime changes either state to `SIG_IGN` before `main`, so the constructor above
/// records the original first. The default disposition is restored unless the constructor saw an
/// ignored one, which preserves the historical behavior if a platform fails to run it.
pub(crate) fn restore_sigpipe() {
    let handler = if SIGPIPE_IGNORED.load(Ordering::Relaxed) {
        SigHandler::SigIgn
    } else {
        SigHandler::SigDfl
    };
    // SAFETY: neither `SIG_DFL` nor `SIG_IGN` installs a Rust callback.
    let _ = unsafe { nix::sys::signal::signal(Signal::SIGPIPE, handler) };
}

/// Ignores SIGINT and SIGHUP for the rest of the process, as `nvx-ingress`'s C program did before
/// it started announcing.
///
/// Like the C program, it ignores failures, which `signal` reports only for an invalid signal.
pub(crate) fn ignore_interrupt_and_hangup() {
    super::ignore_signals(&[Signal::SIGINT, Signal::SIGHUP]);
}

/// Returns an owned, close-on-exec duplicate of the descriptor whose decimal number `text`
/// names, which this process inherited across `exec`, and makes that descriptor close-on-exec
/// too, so that the next `exec` closes it, as the C helper closed it before its own.
///
/// Duplicating the inherited descriptor, rather than adopting it, needs no proof that nothing
/// else in this process owns it: the duplicate is new, and only its flags change on the
/// original. A malformed number is `EINVAL`, and a closed descriptor `EBADF`.
pub(crate) fn take_inherited_descriptor(text: &CStr) -> Result<FileDescriptor, Errno> {
    let inherited = parse_decimal(text.to_bytes())
        .and_then(|value| RawFd::try_from(value).ok())
        .ok_or(Errno::EINVAL)?;
    // `nix`'s `fcntl` takes a borrowed descriptor, which must stay open while it is borrowed, so
    // these calls on an untrusted number go through libc.
    // SAFETY: `F_DUPFD_CLOEXEC` reads only the descriptor number, and fails with `EBADF` if it is
    // closed.
    let duplicate =
        Errno::result(unsafe { nix::libc::fcntl(inherited, nix::libc::F_DUPFD_CLOEXEC, 0) })?;
    // SAFETY: the kernel has just created `duplicate`, so it is open and nothing else owns it.
    let descriptor = unsafe { FileDescriptor::from_raw(duplicate) };
    // SAFETY: `F_SETFD` changes only the flags of the descriptor number.
    Errno::result(unsafe {
        nix::libc::fcntl(inherited, nix::libc::F_SETFD, nix::libc::FD_CLOEXEC)
    })?;
    Ok(descriptor)
}

/// One child process identifier returned by the safe fork/exec boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcessId(Pid);

impl ProcessId {
    /// Wraps a positive process identifier returned by `fork` or `waitpid`.
    pub(super) fn from_pid(process: Pid) -> Self {
        Self(process)
    }

    /// Wraps a process or thread identifier read from a file, or returns `None` for a value
    /// that no process can have.
    pub(crate) fn from_positive(process: i64) -> Option<Self> {
        i32::try_from(process)
            .ok()
            .filter(|&process| process > 0)
            .map(|process| Self(Pid::from_raw(process)))
    }

    /// Returns the decimal process identifier followed by a newline.
    pub(crate) fn line(self) -> Vec<u8> {
        format!("{}\n", self.0.as_raw()).into_bytes()
    }

    /// Returns the identifier as the kernel's integer.
    pub(crate) fn raw(self) -> i32 {
        self.0.as_raw()
    }
}

/// The side of a fork on which the calling process continues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ForkResult {
    /// The calling process is the new child.
    Child,
    /// The calling process is the parent of the child that this identifies.
    Parent(ProcessId),
}

/// Forks the calling process, which must have only the calling thread, or fails with `EBUSY`.
///
/// A child of a multithreaded process could inherit a lock, such as the allocator's, that
/// another thread held, and may call only async-signal-safe functions. A child of a
/// single-threaded process inherits no such lock, and may run any code, as the time tools'
/// children do. Only the calling thread could start another thread between the check and the
/// fork, so the check holds at the fork.
pub(crate) fn fork() -> Result<ForkResult, Errno> {
    if super::thread_count()? != 1 {
        return Err(Errno::EBUSY);
    }
    // SAFETY: the process is single-threaded, so the child may run any code.
    match unsafe { nix::unistd::fork() }? {
        nix::unistd::ForkResult::Child => Ok(ForkResult::Child),
        nix::unistd::ForkResult::Parent { child } => {
            Ok(ForkResult::Parent(ProcessId::from_pid(child)))
        }
    }
}

/// Ends the process with `status` at once, without the cleanup that `std::process::exit` runs,
/// as a forked child ends.
///
/// `nix` does not wrap `_exit`, so this calls it through libc.
pub(crate) fn exit_immediately(status: u8) -> ! {
    // SAFETY: `_exit` accepts any status and does not return.
    unsafe { nix::libc::_exit(i32::from(status)) }
}

/// Returns the calling process's identifier.
pub(crate) fn current_process() -> ProcessId {
    ProcessId::from_pid(nix::unistd::getpid())
}

/// Calls `getpid` with the system call itself and returns the identifier.
///
/// The latency probe measures this system call, which libc's `getpid` may answer without one,
/// so this makes it through libc's `syscall`.
#[cfg(target_arch = "x86_64")]
pub(crate) fn syscall_process_id() -> u64 {
    // SAFETY: `getpid` has no arguments.
    unsafe { nix::libc::syscall(nix::libc::SYS_getpid) as u64 }
}

/// Returns whether `process` exists, which includes a zombie, by sending it no signal.
pub(crate) fn process_exists(process: ProcessId) -> bool {
    kill(process.0, None).is_ok()
}

/// Returns the command name of `process`, which its procfs `comm` file holds with a newline.
pub(crate) fn process_name(process: ProcessId) -> Result<Vec<u8>, Errno> {
    let path = super::c_string(format!("/proc/{}/comm", process.0.as_raw()).as_bytes())?;
    let mut descriptor = super::open(
        &path,
        super::OpenFlags::O_RDONLY | super::OpenFlags::O_CLOEXEC,
        super::Mode::empty(),
    )?;
    let mut name = Vec::new();
    descriptor
        .read_to_end(&mut name)
        .map_err(|error| super::errno_of(&error))?;
    Ok(name)
}

/// Sets the command name of the calling thread, of which the kernel keeps the first 15 bytes.
/// The process's procfs `comm` file holds the name of its first thread, whose ID is the process
/// ID.
pub(crate) fn set_command_name(name: &CStr) -> Result<(), Errno> {
    nix::sys::prctl::set_name(name)
}

/// Starts a new session, makes `null` the standard input, output, and error, and changes to
/// the root directory, as the C programs' daemons did when they detached. Without `null`, the
/// standard streams stay.
pub(crate) fn detach(null: Option<&FileDescriptor>) -> Result<(), Errno> {
    nix::unistd::setsid()?;
    if let Some(null) = null {
        nix::unistd::dup2_stdin(null.borrowed())?;
        nix::unistd::dup2_stdout(null.borrowed())?;
        nix::unistd::dup2_stderr(null.borrowed())?;
    }
    nix::unistd::chdir(c"/")
}

/// A terminal state observed while reaping a child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChildStatus {
    /// The child called `_exit` or returned from its program.
    Exited(i32),
    /// A signal terminated the child.
    Signaled(i32),
    /// A wait state that is neither a normal exit nor signal termination.
    Other,
}

/// A child collected by a nonblocking reap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReapedChild {
    /// Identifier of the collected child.
    pub(crate) process: ProcessId,
    /// Terminal state reported by the kernel.
    pub(crate) status: ChildStatus,
    /// The raw wait status that `waitpid` reported, from which `status` is decoded.
    pub(crate) wait_status: i32,
}

/// Decodes Linux's raw wait status without narrowing real-time signal numbers.
fn child_status(process: nix::libc::pid_t, status: nix::libc::c_int) -> ReapedChild {
    let decoded = if nix::libc::WIFEXITED(status) {
        ChildStatus::Exited(nix::libc::WEXITSTATUS(status))
    } else if nix::libc::WIFSIGNALED(status) {
        ChildStatus::Signaled(nix::libc::WTERMSIG(status))
    } else {
        ChildStatus::Other
    };
    ReapedChild {
        process: ProcessId::from_pid(Pid::from_raw(process)),
        status: decoded,
        wait_status: status,
    }
}

/// Reaps one available child without blocking.
///
/// Only a program that is its process's one status collector calls this: the managed agent, which
/// as the direct agent is PID 1 and also collects its workloads' orphaned descendants, and the
/// time daemon. The guest libraries wait only for the child that the caller names, with
/// [`wait_named`].
///
/// libc is used instead of `nix` because `nix` converts a terminating signal to its finite
/// `Signal` enum and rejects Linux real-time signal numbers, after the child is reaped.
/// Interruptions are retried. `ECHILD`, a running child, and other errors all mean that no
/// reapable child is available, matching the C agent's best-effort orphan-reaping loop.
pub(crate) fn reap_any_nonblocking() -> Option<ReapedChild> {
    loop {
        let mut status = 0;
        // SAFETY: `status` is writable, `WNOHANG` never blocks, and a positive result names a
        // child that this call atomically reaped.
        let process = unsafe { nix::libc::waitpid(-1, &mut status, nix::libc::WNOHANG) };
        if process > 0 {
            return Some(child_status(process, status));
        }
        if process == 0 {
            return None;
        }
        if Errno::last() != Errno::EINTR {
            return None;
        }
    }
}

/// Waits for `process`, retrying interruptions.
///
/// libc is used for the same real-time signals as [`reap_any_nonblocking`].
pub(crate) fn wait(process: ProcessId) -> Option<ChildStatus> {
    loop {
        let mut status = 0;
        // SAFETY: `status` is writable and `process` came from this module's successful fork
        // boundary or a test-owned `Child`.
        let observed = unsafe { nix::libc::waitpid(process.0.as_raw(), &mut status, 0) };
        if observed == process.0.as_raw() {
            return Some(child_status(observed, status).status);
        }
        if observed < 0 && Errno::last() == Errno::EINTR {
            continue;
        }
        return None;
    }
}

/// Reaps `process` if it has ended, without blocking, and returns its state, or `None` while
/// it runs.
///
/// libc is used for the same real-time signals as [`reap_any_nonblocking`].
pub(crate) fn try_wait(process: ProcessId) -> Result<Option<ChildStatus>, Errno> {
    let mut status = 0;
    // SAFETY: `status` is writable, and `WNOHANG` never blocks.
    let observed = Errno::result(unsafe {
        nix::libc::waitpid(process.0.as_raw(), &mut status, nix::libc::WNOHANG)
    })?;
    Ok((observed == process.0.as_raw()).then(|| child_status(observed, status).status))
}

/// Sends `SIGKILL` to the process group led by `process`.
pub(crate) fn kill_process_group(process: ProcessId) -> Result<(), Errno> {
    signal_process_group(process, nix::libc::SIGKILL)
}

/// Sends the signal numbered `signal` to the process group led by `process`, by the group's ID.
///
/// libc's `kill` is used because `nix`'s `Signal` cannot represent a real-time signal.
pub(crate) fn signal_process_group(
    process: ProcessId,
    signal: nix::libc::c_int,
) -> Result<(), Errno> {
    // SAFETY: `kill` reads only its integer arguments.
    Errno::result(unsafe { nix::libc::kill(-process.0.as_raw(), signal) }).map(drop)
}

/// Sends `SIGKILL` to `process` alone.
pub(crate) fn kill_process(process: ProcessId) -> Result<(), Errno> {
    kill(process.0, Signal::SIGKILL)
}

/// Returns the ID of the process group to which `process` belongs. The lookup goes by
/// `process`'s ID, which names `process` only while its status stays uncollected.
pub(crate) fn process_group_of(process: ProcessId) -> Result<ProcessId, Errno> {
    nix::unistd::getpgid(Some(process.0)).map(ProcessId)
}

/// One child that a wait names, which is all that it may collect.
#[derive(Clone, Copy, Debug)]
pub(crate) enum WaitTarget<'a> {
    /// The child with this process ID.
    Process(ProcessId),
    /// The child to which this pidfd refers.
    Pidfd(BorrowedFd<'a>),
}

/// Collects the status of the child that `target` names once it has ended, and returns its raw
/// wait status, in the form that `waitpid` reports. Without `block`, it returns `None` while the
/// child runs.
///
/// Only an exit or a terminating signal ends the child. `waitid` also reports each stop of a
/// child that this process traces, whatever its options, which leaves the child running: a
/// blocking wait goes on past such a stop, and one without `block` returns `None`.
///
/// Only the named child's status is collected, so every other child's stays for its collector.
/// The wait names the child whatever signal it reports its end with, as `__WALL` makes it, so a
/// child that `clone` started with an exit signal other than `SIGCHLD`, or none, and that ends
/// before it runs a program, which would reset that signal, is collected too. Interruptions are
/// retried. A target that is not a child of this process, or whose status was collected already,
/// is `ECHILD`, and a blocking wait through a pidfd that its opener made nonblocking, while the
/// child runs, is `EAGAIN`.
///
/// libc is used because `nix`'s `waitid` decodes the state into a `WaitStatus`, which fails for a
/// real-time signal after the child is reaped. The raw status is rebuilt from the child event's
/// code and status, which keeps the signal number and the core dump.
pub(crate) fn wait_named(target: WaitTarget<'_>, block: bool) -> Result<Option<i32>, Errno> {
    let (kind, id) = match target {
        WaitTarget::Process(process) => (nix::libc::P_PID, process.0.as_raw() as nix::libc::id_t),
        WaitTarget::Pidfd(descriptor) => (
            nix::libc::P_PIDFD,
            descriptor.as_raw_fd() as nix::libc::id_t,
        ),
    };
    let options = if block {
        nix::libc::WEXITED | nix::libc::__WALL
    } else {
        nix::libc::WEXITED | nix::libc::WNOHANG | nix::libc::__WALL
    };
    loop {
        // SAFETY: zero is a valid initial representation for `siginfo_t`, and leaves `si_pid` zero
        // unless a child event fills it.
        let mut information = unsafe { std::mem::zeroed::<nix::libc::siginfo_t>() };
        // SAFETY: `information` is writable, and the call collects at most the named child.
        let result = unsafe { nix::libc::waitid(kind, id, &mut information, options) };
        if result < 0 {
            match Errno::last() {
                Errno::EINTR => continue,
                error => return Err(error),
            }
        }
        // SAFETY: a successful `waitid` either initialized the Linux child-event fields, or, for a
        // child that still runs, left the zeroed `si_pid`.
        let (process, status) = unsafe { (information.si_pid(), information.si_status()) };
        if process == 0 {
            return Ok(None);
        }
        return Ok(Some(match information.si_code {
            nix::libc::CLD_EXITED => (status & 0xff) << 8,
            nix::libc::CLD_KILLED => status & 0x7f,
            nix::libc::CLD_DUMPED => (status & 0x7f) | 0x80,
            // A traced child's stop, which does not end it.
            _ if block => continue,
            _ => return Ok(None),
        }));
    }
}

/// Returns whether the child to which `pidfd` refers is unreaped: whether it runs, or has ended
/// with a status that no collector has taken yet, which the check leaves in place, as `WNOWAIT`
/// makes it. Unlike a signal 0, the check needs no permission to signal the child. A child whose
/// status a collector took, as the wait's `ECHILD` tells, is not.
///
/// libc is used for the same reason as in [`wait_named`], whose options the check shares.
pub(crate) fn is_unreaped(pidfd: BorrowedFd<'_>) -> Result<bool, Errno> {
    let options = nix::libc::WEXITED | nix::libc::WNOHANG | nix::libc::WNOWAIT | nix::libc::__WALL;
    loop {
        // SAFETY: zero is a valid initial representation for `siginfo_t`.
        let mut information = unsafe { std::mem::zeroed::<nix::libc::siginfo_t>() };
        // SAFETY: `information` is writable, and `WNOWAIT` leaves the child's status in place.
        let result = unsafe {
            nix::libc::waitid(
                nix::libc::P_PIDFD,
                pidfd.as_raw_fd() as nix::libc::id_t,
                &mut information,
                options,
            )
        };
        if result == 0 {
            return Ok(true);
        }
        match Errno::last() {
            Errno::EINTR => {}
            Errno::ECHILD => return Ok(false),
            error => return Err(error),
        }
    }
}

/// Opens a close-on-exec pidfd that refers to `process`, which polls readable once the process
/// ends, and keeps naming it after it is reaped.
///
/// An ID that no process has fails with `ESRCH` on every kernel: once a collector has taken the
/// status of the process that had it, the ID may name another process, which the caller must not
/// take for it. `pidfd_open` answers `ESRCH` for an ID that nothing holds any more, but an ID
/// outlives its process while a process group or a session that the process led keeps it, and a
/// thread that leads no process has an ID too. For those, a kernel before Linux 6.16 answers
/// `EINVAL`, and a later one `ESRCH`, or `ENOENT` for a thread's ID, so `EINVAL` and `ENOENT` fail
/// with `ESRCH` too: for a positive ID and valid flags, they mean nothing else. A test may have
/// `pidfd_open` answer as a kernel before Linux 6.16 does.
///
/// libc's `syscall` is used because neither `nix` nor musl wraps `pidfd_open`.
pub(crate) fn open_pidfd(process: ProcessId) -> Result<FileDescriptor, Errno> {
    // SAFETY: `pidfd_open` reads only its integer arguments, and returns a new descriptor or -1.
    let opened = Errno::result(unsafe {
        nix::libc::syscall(nix::libc::SYS_pidfd_open, process.0.as_raw(), 0)
    });
    #[cfg(test)]
    let opened = tests::as_before_linux_6_16(process, opened);
    let descriptor = opened.map_err(|error| match error {
        Errno::EINVAL | Errno::ENOENT => Errno::ESRCH,
        error => error,
    })?;
    // A descriptor number fits a `RawFd`.
    let descriptor = descriptor as RawFd;
    // SAFETY: the kernel has just created `descriptor`, so it is open and nothing else owns it.
    Ok(unsafe { FileDescriptor::from_raw(descriptor) })
}

/// Sends `SIGKILL` through `pidfd`, as [`signal_through_pidfd`] sends any signal.
pub(crate) fn kill_through_pidfd(pidfd: BorrowedFd<'_>, process_group: bool) -> Result<(), Errno> {
    signal_through_pidfd(pidfd, nix::libc::SIGKILL, process_group)
}

/// Sends the signal numbered `signal` through `pidfd` to the process to which it refers or, with
/// `process_group`, to every process of the process group that the process leads, since Linux
/// 6.9. A real-time signal, which `nix`'s `Signal` cannot represent, goes as any other.
///
/// A pidfd names its process, and the group that the process leads, whatever later takes their
/// IDs: the signal to the process fails with `ESRCH` once a collector has taken the process's
/// status, and the signal to the group fails with `ESRCH` once no process of the group is left,
/// even after a collector took the status of the process that led it. A kernel that cannot
/// signal a group through a pidfd, before Linux 6.9, fails with `EINVAL`, as it does for a number
/// that names no signal.
///
/// libc's `syscall` is used because neither `nix` nor musl wraps `pidfd_send_signal`.
pub(crate) fn signal_through_pidfd(
    pidfd: BorrowedFd<'_>,
    signal: nix::libc::c_int,
    process_group: bool,
) -> Result<(), Errno> {
    let flags = if process_group {
        nix::libc::PIDFD_SIGNAL_PROCESS_GROUP
    } else {
        0
    };
    // SAFETY: `pidfd_send_signal` reads its integer arguments, and a null `siginfo_t` pointer
    // asks the kernel for the details of a signal that `kill` sends.
    Errno::result(unsafe {
        nix::libc::syscall(
            nix::libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<nix::libc::siginfo_t>(),
            flags,
        )
    })
    .map(drop)
}

/// Replaces the process image with `path` and `arguments`, and with `environment`, or without
/// one, the current environment. It returns only the error of a failed `exec`.
pub(crate) fn exec(path: &CStr, arguments: &[CString], environment: Option<&[CString]>) -> Errno {
    let result = match environment {
        Some(environment) => nix::unistd::execve(path, arguments, environment),
        None => nix::unistd::execv(path, arguments),
    };
    match result {
        Ok(never) => match never {},
        Err(error) => error,
    }
}

/// Writes all bytes to standard error and ignores failures.
pub(crate) fn write_standard_error(bytes: &[u8]) {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match nix::unistd::write(io::stderr(), remaining) {
            Ok(0) => return,
            Ok(count) => remaining = &remaining[count..],
            Err(Errno::EINTR) => {}
            Err(_) => return,
        }
    }
}

/// Writes `message` to a diagnostic I/O port through a descriptor.
///
/// A write that stores no byte is `EIO`, the status that the agent reports for it.
pub(crate) fn write_port_diagnostic(
    descriptor: &FileDescriptor,
    message: &[u8],
) -> Result<(), Errno> {
    for byte in message {
        if descriptor.pwrite(std::slice::from_ref(byte), Port::PortbData.offset())? != 1 {
            return Err(Errno::EIO);
        }
    }
    Ok(())
}

/// Standard output with the error behavior of C stdio.
///
/// C line-buffers a terminal, so a failed terminal write fails `printf`. It fully buffers other
/// destinations until `exit`, whose flush failure these tools ignored. This adapter performs the
/// write immediately but suppresses the same nonterminal errors; interrupted writes remain
/// visible so `Write::write_all` retries them.
pub(crate) struct Stdout {
    /// Whether C stdio would line-buffer standard output.
    terminal: bool,
}

impl Stdout {
    /// Returns an adapter for the process's standard output.
    pub(crate) fn new() -> Self {
        let stdout = io::stdout();
        let terminal = nix::unistd::isatty(&stdout).unwrap_or(false);
        Self { terminal }
    }
}

impl Write for Stdout {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let stdout = io::stdout();
        match nix::unistd::write(&stdout, bytes) {
            Ok(count) => Ok(count),
            Err(error) if self.terminal || error == nix::errno::Errno::EINTR => {
                Err(io::Error::from(error))
            }
            Err(_) => Ok(bytes.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Standard output written with direct `write` calls, whose every failure is reported.
///
/// `nvx-device-io`'s C program wrote its result with its own `write` loop rather than stdio. The
/// loop retried only `EINTR`, and failed on any other error or on a write that wrote nothing.
/// [`Write::write_all`] over this adapter does the same.
pub(crate) struct DirectStdout;

impl Write for DirectStdout {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        nix::unistd::write(io::stdout(), bytes).map_err(io::Error::from)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Reads standard input until its end and discards what it reads, retrying interruptions.
///
/// It reads standard input with direct `read` calls, past the standard library's buffer, which
/// therefore stays empty.
#[cfg(target_arch = "x86_64")]
pub(crate) fn discard_standard_input() -> Result<(), Errno> {
    let mut buffer = [0_u8; 512];
    loop {
        match nix::unistd::read(io::stdin(), &mut buffer) {
            Ok(0) => return Ok(()),
            Ok(_) | Err(Errno::EINTR) => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
pub(super) mod tests;
