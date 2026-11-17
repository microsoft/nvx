//! The fork/exec boundary of nvx-init's plain-process executions.
//!
//! [`PreparedChild::new`] checks a [`ChildSpec`] and allocates or opens everything that the child
//! uses after `fork`, and [`PreparedChild::spawn`] forks it. The child branch
//! performs no allocation, locking, formatting, or Rust cleanup: it makes direct system calls
//! over that storage, in the order of [`LaunchStage`], and ends in `execve`, `execv`, or `_exit`.
//! It makes them through `nix` where its wrappers neither allocate nor consume an owner, and
//! otherwise through libc: its `syscall` fills the preallocated `PWD` entry, because its `getcwd`
//! may walk the file system, closes the descriptors that the child does not use with
//! `close_range`, resets the signals with `rt_sigaction` and `rt_sigprocmask`, since libc's
//! `sigaction` refuses the signals that libc reserves for itself and musl's takes a lock for
//! `SIGABRT`, and makes the workload identity's calls that the [entry](super::entry) module
//! lists; and its `execve`, `execv`, and `_exit` end the child. A stage that fails writes its
//! number and `errno` to the launch report, a close-on-exec pipe, with `SIGPIPE` blocked, and
//! ends the child with status 125, so that the report's reader reaches end of file without a
//! report once the program runs, and a report that no reader takes still ends the child with
//! that status.
//!
//! Every descriptor that the child uses after it places its standard streams has a number above
//! theirs, so that placing them replaces none of it: [`PreparedChild::new`] moves a stream source
//! or the report's writer that has a lower number. It also makes them close-on-exec, so that the
//! program receives them only as its standard streams. A workload's child first closes every
//! other descriptor, so that it holds no copy of another child's, and closes, before it enters
//! its working directory, every descriptor but its standard streams and its launch report's.

use std::ffi::{CStr, CString};
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;

use nix::fcntl::{FcntlArg, FdFlag};
use nix::libc::{c_char, c_int, c_uint, c_ulong};
use nix::unistd::{ForkResult, Pid};

use super::{Errno, FileDescriptor, PATH_MAX, ProcessId};

/// Prefix of the mutable physical-working-directory environment entry.
const PWD_PREFIX: &[u8] = b"PWD=";
/// Capacity of the physical working directory in its entry, with its terminating NUL: Linux's
/// `PATH_MAX`.
const PHYSICAL_PWD_CAPACITY: usize = 4096;
/// Linux's `NAME_MAX`: the longest name of a file, in bytes, without its terminating NUL, and the
/// longest name of a program that a search looks for, as `execvp`'s does.
const NAME_MAX: usize = nix::libc::NAME_MAX as usize;
/// The lowest descriptor number above the standard streams'.
const FIRST_NONSTANDARD: RawFd = 3;
/// Exit status of a child whose launch stage failed.
const LAUNCH_FAILURE_STATUS: i32 = 125;
/// Length of a launch report: the stage's number and the `errno`, as native-endian 32-bit
/// integers.
pub(crate) const REPORT_LENGTH: usize = 8;
/// The number of Linux's signals, numbered from 1: its `_NSIG` on x86-64 and AArch64, whose
/// signal sets `rt_sigaction` and `rt_sigprocmask` take as one 64-bit word.
const SIGNALS: c_int = 64;

/// A step that a child takes after it starts and before its program runs.
///
/// The variants are in the order of the steps, and a child skips the steps that its
/// [`ChildSpec`] does not ask for. A child whose step fails reports it and exits with status 125
/// without running its program.
///
/// Each stage has a number, its [`code`](Self::code), with which the child's launch report names
/// the stage that failed, and which the report's readers decode: a stage keeps its number, and a
/// new stage takes a number of its own, wherever it comes in the order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaunchStage {
    /// The child makes itself the leader of a new process group.
    ProcessGroup,
    /// The child places its standard streams. A workload's child first closes every descriptor
    /// that it does not use.
    Descriptors,
    /// The child drops every capability but its own from its bounding set.
    Bounding,
    /// The child takes its supplementary groups, group ID, and user ID.
    Identity,
    /// The child applies its capabilities, and empties its inheritable and ambient sets.
    Capabilities,
    /// The child sets `no_new_privs`.
    NoNewPrivileges,
    /// The child enters its working directory. A workload's child first closes every descriptor
    /// but its standard streams and its launch report's.
    WorkingDirectory,
    /// The child records the physical path of its working directory in `PWD`.
    PhysicalWorkingDirectory,
    /// The child gives every signal its default disposition, then unblocks every signal.
    Signals,
    /// The child replaces itself with its program: the first of those that its search finds that
    /// it can run, if its spec has a search path.
    Exec,
}

impl LaunchStage {
    /// Returns the stage's number: from 1, for [`ProcessGroup`](Self::ProcessGroup), to 21, for
    /// [`Exec`](Self::Exec), in the order of the stages, with gaps where stages left. A stage
    /// that NVX adds takes 22, then 23, and so on, wherever it comes in the order, and no stage
    /// is numbered 0.
    pub(crate) const fn code(self) -> u32 {
        match self {
            Self::ProcessGroup => 1,
            Self::Descriptors => 4,
            Self::Bounding => 13,
            Self::Identity => 14,
            Self::Capabilities => 15,
            Self::NoNewPrivileges => 16,
            Self::WorkingDirectory => 18,
            Self::PhysicalWorkingDirectory => 19,
            Self::Signals => 20,
            Self::Exec => 21,
        }
    }
}

/// Everything that a caller asks of one child.
#[derive(Debug)]
pub(crate) struct ChildSpec {
    /// Path of the program.
    program: CString,
    /// Arguments, argument zero first.
    arguments: Vec<CString>,
    /// The `PATH` value in whose directories the child looks for a program that it names without
    /// a `/`, if any.
    search_path: Option<CString>,
    /// Exact environment, or `None` to inherit the caller's.
    environment: Option<Vec<CString>>,
    /// Whether `PWD` receives the physical working directory.
    physical_pwd: bool,
    /// Absolute working directory, resolved in the root, or `None` to keep the current one.
    directory: Option<CString>,
    /// Sources of standard input, output, and error; `None` keeps the caller's.
    streams: [Option<FileDescriptor>; 3],
    /// Whether the child leads a new process group.
    process_group: bool,
    /// Whether the child gives every signal its default disposition, and blocks none, before its
    /// program runs.
    reset_signals: bool,
    /// The identity of an init workload, without an entry or per-execution containment.
    workload: Option<(u32, u32)>,
    /// Test-only outcome of the credential system calls, which the child then skips:
    /// `Some(None)` succeeds, and `Some(Some(error))` fails the first one with `error`.
    #[cfg(test)]
    simulated_credentials: Option<Option<Errno>>,
}

impl ChildSpec {
    /// Describes a child that runs `program` with `arguments` and keeps everything else of its
    /// parent's.
    pub(crate) fn new(program: CString, arguments: Vec<CString>) -> Self {
        Self {
            program,
            arguments,
            search_path: None,
            environment: None,
            physical_pwd: false,
            directory: None,
            streams: [None, None, None],
            process_group: false,
            reset_signals: false,
            workload: None,
            #[cfg(test)]
            simulated_credentials: None,
        }
    }

    /// Makes the child look for a program that it names without a `/` in the directories of
    /// `path`, a `PATH` value.
    pub(crate) fn set_search_path(&mut self, path: CString) {
        self.search_path = Some(path);
    }

    /// Gives the child exactly `environment` instead of the caller's.
    pub(crate) fn set_environment(&mut self, environment: Vec<CString>) {
        self.environment = Some(environment);
    }

    /// Makes the child record its physical working directory in `PWD`.
    pub(crate) fn export_physical_pwd(&mut self) {
        self.physical_pwd = true;
    }

    /// Makes the absolute path `directory`, resolved in the root, the child's working directory.
    pub(crate) fn set_directory(&mut self, directory: CString) {
        self.directory = Some(directory);
    }

    /// Makes `descriptor` the child's standard stream numbered `stream`: 0, 1, or 2.
    pub(crate) fn set_stream(&mut self, stream: usize, descriptor: FileDescriptor) {
        self.streams[stream] = Some(descriptor);
    }

    /// Gives an init workload its identity, its own process group, and reset signals.
    pub(crate) fn set_workload(&mut self, uid: u32, gid: u32) {
        self.workload = Some((uid, gid));
        self.process_group = true;
        self.reset_signals = true;
    }
}

/// Mutable physical-working-directory entry prepared before `fork`.
struct PhysicalPwd {
    /// Writable `PWD=` entry, with capacity for the path and its trailing NUL.
    entry: Box<[u8]>,
    /// Position of the entry in the child's environment.
    index: usize,
}

impl PhysicalPwd {
    /// Allocates the entry and removes the first `PWD` entry of `environment`, whose position it
    /// takes, or appends it.
    fn new(environment: &mut Vec<CString>) -> Self {
        let index = environment
            .iter()
            .position(|entry| entry.as_bytes().starts_with(PWD_PREFIX))
            .map_or(environment.len(), |index| {
                environment.remove(index);
                index
            });
        let mut entry = vec![0u8; PWD_PREFIX.len() + PHYSICAL_PWD_CAPACITY].into_boxed_slice();
        entry[..PWD_PREFIX.len()].copy_from_slice(PWD_PREFIX);
        Self { entry, index }
    }
}

/// A `PATH` search for a child's program, prepared before `fork`: the directories in which the
/// child looks for it, and the one buffer in which the child builds each path in turn, so that the
/// search takes the same memory however long the program's name and the `PATH` are.
struct Search {
    /// The `PATH` value in whose directories the child looks, in their order.
    path: CString,
    /// The buffer in which the child builds each path, of [`PATH_MAX`] bytes: it holds every path
    /// that the kernel takes, with its terminating NUL, so the kernel refuses every other with
    /// `ENAMETOOLONG`.
    buffer: Box<[u8]>,
}

impl Search {
    /// Prepares the search for `program` in the directories of `path`, a `PATH` value, or returns
    /// `None` for a program whose name is empty or holds a `/`, which the child runs as it is.
    fn new(program: &CStr, path: CString) -> Option<Self> {
        let name = program.to_bytes();
        if name.is_empty() || name.contains(&b'/') {
            return None;
        }
        Some(Self {
            path,
            buffer: vec![0; PATH_MAX].into_boxed_slice(),
        })
    }

    /// Runs the program `name` from the first directory in which the child can run it, as
    /// [`exec`] does, and returns the `errno` of the search's failure, as [`search`] tells.
    ///
    /// # Safety
    ///
    /// As for [`exec`].
    unsafe fn run(
        &mut self,
        name: &CStr,
        arguments: &[*const c_char],
        environment: Option<&[*const c_char]>,
    ) -> Errno {
        search(name, &self.path, &mut self.buffer, |path| {
            // SAFETY: the caller establishes what `exec` requires.
            unsafe { exec(path, arguments, environment) }
        })
    }
}

/// A checked child whose every input is allocated or opened, ready to fork.
pub(crate) struct PreparedChild {
    /// Path of the program.
    program: CString,
    /// Arguments, argument zero first.
    arguments: Vec<CString>,
    /// The search in which the child looks for its program, or `None` to run `program` as it is.
    search: Option<Search>,
    /// Exact environment, without the physical `PWD` entry, or `None` to inherit the caller's.
    environment: Option<Vec<CString>>,
    /// Physical `PWD` entry that the child fills.
    physical_pwd: Option<PhysicalPwd>,
    /// Absolute working directory, resolved in the root.
    directory: Option<CString>,
    /// Sources of standard input, output, and error.
    streams: [Option<FileDescriptor>; 3],
    /// Whether the child leads a new process group.
    process_group: bool,
    /// Whether the child resets its signals before its program runs.
    reset_signals: bool,
    /// The identity of an init workload, whose priority is reset before it takes it.
    workload: Option<(u32, u32)>,
    /// Reader of the launch report, which the caller receives.
    report_reader: FileDescriptor,
    /// Writer of the launch report, which only the child keeps.
    report_writer: FileDescriptor,
    /// The numbers of every descriptor that the child uses before its program runs, in ascending
    /// order: the stream sources and the report's writer. A workload's child closes every other.
    used: Box<[RawFd]>,
    /// The number of the report's writer: all that a workload's child keeps, but its standard
    /// streams, once its stages have run.
    kept: Box<[RawFd]>,
    /// Test-only outcome of the credential system calls.
    #[cfg(test)]
    simulated_credentials: Option<Option<Errno>>,
}

/// A child that [`PreparedChild::spawn`] started.
pub(crate) struct Started {
    /// The child's process ID.
    pub(crate) process: ProcessId,
    /// The reader of the child's launch report.
    pub(crate) report: FileDescriptor,
}

impl PreparedChild {
    /// Checks `spec` and prepares every input of its child.
    ///
    /// The working directory must be absolute, and the physical `PWD` needs an exact
    /// environment; a command with no argument zero is invalid too. Each fails with `EINVAL`. A
    /// program that the spec names without a `/`, in a spec with a search path, gets here the one
    /// buffer in which its child builds, in turn, each path at which it looks for it.
    pub(crate) fn new(spec: ChildSpec) -> Result<Self, Errno> {
        if spec.arguments.is_empty()
            || spec
                .directory
                .as_ref()
                .is_some_and(|directory| directory.as_bytes().first() != Some(&b'/'))
            || (spec.physical_pwd && spec.environment.is_none())
        {
            return Err(Errno::EINVAL);
        }
        let [stdin, stdout, stderr] = spec.streams;
        let streams = [
            stdin.map(above_standard_streams).transpose()?,
            stdout.map(above_standard_streams).transpose()?,
            stderr.map(above_standard_streams).transpose()?,
        ];
        let (report_reader, report_writer) = super::create_pipe()?;
        let report_writer = above_standard_streams(report_writer)?;
        let mut environment = spec.environment;
        let physical_pwd = match (&mut environment, spec.physical_pwd) {
            (Some(environment), true) => Some(PhysicalPwd::new(environment)),
            _ => None,
        };
        let used = ascending_numbers(streams.iter().flatten().chain([&report_writer]));
        let kept = ascending_numbers([&report_writer]);
        let search = spec
            .search_path
            .and_then(|path| Search::new(&spec.program, path));
        Ok(Self {
            program: spec.program,
            arguments: spec.arguments,
            search,
            environment,
            physical_pwd,
            directory: spec.directory,
            streams,
            process_group: spec.process_group,
            reset_signals: spec.reset_signals,
            workload: spec.workload,
            report_reader,
            report_writer,
            used,
            kept,
            #[cfg(test)]
            simulated_credentials: spec.simulated_credentials,
        })
    }

    /// Forks the child, which runs its launch stages and then its program, and returns its
    /// process ID and the reader of its launch report.
    ///
    /// On x86-64, it fails with `EPERM`, before it forks, while the calling thread may still
    /// hold the permission for OpenVMM's I/O ports that a time call could not revoke, which the
    /// child would inherit.
    ///
    /// The parent closes its copies of the child's descriptors once the child exists. This
    /// function is safe because every pointer and descriptor that the child uses belongs to this
    /// value or to this frame, which the child's copied stack keeps, and the child path never
    /// returns into Rust. `fork` may occur in a multithreaded process: the child makes only
    /// direct async-signal-safe system calls over storage allocated before the fork.
    pub(crate) fn spawn(mut self) -> Result<Started, Errno> {
        #[cfg(target_arch = "x86_64")]
        if super::check_released().is_err() {
            return Err(Errno::EPERM);
        }
        let mut argument_pointers: Vec<*const c_char> = self
            .arguments
            .iter()
            .map(|argument| argument.as_ptr())
            .collect();
        argument_pointers.push(std::ptr::null());
        // The child writes the physical `PWD` entry through one raw pointer, which also serves
        // the environment. The entry's owner leaves `self`, which the child borrows, and lives
        // in this frame, so no reference to the entry exists while the child fills it.
        let mut physical_pwd = self.physical_pwd.take();
        let physical = physical_pwd
            .as_mut()
            .map(|physical_pwd| (physical_pwd.entry.as_mut_ptr(), physical_pwd.index));
        let environment_pointers = self.environment.as_ref().map(|environment| {
            let mut pointers = Vec::with_capacity(environment.len() + 2);
            for index in 0..=environment.len() {
                if let Some((entry, position)) = physical
                    && position == index
                {
                    pointers.push(entry.cast_const().cast::<c_char>());
                }
                if let Some(entry) = environment.get(index) {
                    pointers.push(entry.as_ptr());
                }
            }
            pointers.push(std::ptr::null());
            pointers
        });
        // The child builds the paths of its search in the search's buffer, whose owner leaves
        // `self` too, so that the child may write to it while it borrows `self`.
        let mut search = self.search.take();

        // SAFETY: every child input and pointer vector was allocated above, and the child runs
        // only the non-returning routine below.
        let child = match unsafe { nix::unistd::fork() }? {
            ForkResult::Child => None,
            ForkResult::Parent { child } => Some(child),
        };
        match child {
            None => {
                // SAFETY: this is the just-started child, and the pointers name storage that
                // `self`, the physical `PWD` entry's owner, and the vectors above own.
                unsafe {
                    self.run_child(
                        &argument_pointers,
                        environment_pointers.as_deref(),
                        physical.map(|(entry, _)| entry),
                        search.as_mut(),
                    )
                }
            }
            Some(child) => {
                if self.process_group {
                    // The child makes the same call; whichever runs first creates the group, so
                    // that the caller may signal it at once. The child's `exec` or exit makes a
                    // failure here harmless.
                    let _ = nix::unistd::setpgid(child, child);
                }
                Ok(Started {
                    process: ProcessId::from_pid(child),
                    report: self.report_reader,
                })
            }
        }
    }

    /// Runs the child's launch stages, then its program.
    ///
    /// # Safety
    ///
    /// This must run only in the just-forked child. `arguments` and `environment` must be
    /// NUL-terminated vectors of pointers to C strings that outlive the call, `physical_pwd`, if
    /// any, the physical `PWD` entry of this child, and `search`, if any, the search for its
    /// program.
    unsafe fn run_child(
        &self,
        arguments: &[*const c_char],
        environment: Option<&[*const c_char]>,
        physical_pwd: Option<*mut u8>,
        search: Option<&mut Search>,
    ) -> ! {
        let report = &self.report_writer;
        if self.process_group
            && let Err(error) = nix::unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0))
        {
            // SAFETY: the caller establishes that this is the post-fork child.
            unsafe { fail(report, LaunchStage::ProcessGroup, error) }
        }
        if let Err(error) = self.place_descriptors() {
            // SAFETY: as above.
            unsafe { fail(report, LaunchStage::Descriptors, error) }
        }
        if let Some((uid, gid)) = self.workload
            && let Err((stage, error)) = self.take_workload(uid, gid)
        {
            // SAFETY: as above.
            unsafe { fail(report, stage, error) }
        }
        if self.workload.is_some() {
            // Close the stages' descriptors before resolving the working directory, with chdir's
            // resolution under the workload's final credentials.
            let directory = self.directory.as_deref().unwrap_or(c"/");
            if let Err(error) =
                close_other_descriptors(&self.kept).and_then(|()| nix::unistd::chdir(directory))
            {
                // SAFETY: as above.
                unsafe { fail(report, LaunchStage::WorkingDirectory, error) }
            }
        } else if let Some(directory) = &self.directory
            && let Err(error) = nix::unistd::chdir(directory.as_c_str())
        {
            // SAFETY: as above.
            unsafe { fail(report, LaunchStage::WorkingDirectory, error) }
        }
        if let Some(entry) = physical_pwd {
            // `nix`'s `getcwd` allocates its result, and glibc's walks the file system, which
            // allocates, when the kernel's path is too long or outside the root. So the system
            // call fills the entry in place, and its failures are musl's: the kernel's `errno`,
            // or `ENOENT` for a directory outside the root, which the kernel marks
            // `(unreachable)`.
            // SAFETY: the entry holds the prefix and `PHYSICAL_PWD_CAPACITY` initialized bytes
            // after it, which the system call writes at most.
            let (length, first) = unsafe {
                let path = entry.add(PWD_PREFIX.len());
                let length = nix::libc::syscall(nix::libc::SYS_getcwd, path, PHYSICAL_PWD_CAPACITY);
                (length, path.read())
            };
            if length < 0 {
                // SAFETY: the caller establishes that this is the post-fork child.
                unsafe { fail(report, LaunchStage::PhysicalWorkingDirectory, Errno::last()) }
            }
            if first != b'/' {
                // SAFETY: as above.
                unsafe { fail(report, LaunchStage::PhysicalWorkingDirectory, Errno::ENOENT) }
            }
        }
        if self.reset_signals
            && let Err(error) = reset_signals()
        {
            // SAFETY: as above.
            unsafe { fail(report, LaunchStage::Signals, error) }
        }
        // SAFETY: the caller establishes that `arguments` and `environment` are NUL-terminated
        // vectors of pointers to C strings that outlive the call.
        let error = unsafe {
            match search {
                Some(search) => search.run(&self.program, arguments, environment),
                None => exec(&self.program, arguments, environment),
            }
        };
        // SAFETY: as above.
        unsafe { fail(report, LaunchStage::Exec, error) }
    }

    /// Resets priority before taking a plain workload's identity, without allocating.
    fn take_workload(&self, uid: u32, gid: u32) -> Result<(), (LaunchStage, Errno)> {
        #[cfg(test)]
        if let Some(simulated) = self.simulated_credentials {
            return simulated.map_or(Ok(()), |error| Err((LaunchStage::Identity, error)));
        }
        reset_workload_priority().map_err(|error| (LaunchStage::Identity, error))?;
        super::entry::take_workload_identity(uid, gid)
    }

    /// Places the standard streams.
    ///
    /// Every source has a number above the standard streams', so `dup2` replaces each stream
    /// without closing a source, and clears close-on-exec on its copy. A workload's child first
    /// closes every descriptor that it does not use, those that its caller opened without
    /// close-on-exec too, so that its program holds none of the caller's.
    fn place_descriptors(&self) -> Result<(), Errno> {
        if self.workload.is_some() {
            close_other_descriptors(&self.used)?;
        }
        let [stdin, stdout, stderr] = &self.streams;
        if let Some(source) = stdin {
            retry(|| nix::unistd::dup2_stdin(source.borrowed()))?;
        }
        if let Some(source) = stdout {
            retry(|| nix::unistd::dup2_stdout(source.borrowed()))?;
        }
        if let Some(source) = stderr {
            retry(|| nix::unistd::dup2_stderr(source.borrowed()))?;
        }
        Ok(())
    }
}

/// Resets an init workload's out-of-memory adjustment and nice value before it loses privilege.
///
/// The child opens its own procfs entry: a descriptor opened before `fork` would still name
/// the parent's adjustment. These operations allocate nothing after `fork`.
fn reset_workload_priority() -> Result<(), Errno> {
    let adjustment = super::open(
        super::kernel::OOM_SCORE_ADJUSTMENT,
        super::OpenFlags::O_WRONLY | super::OpenFlags::O_CLOEXEC,
        super::Mode::empty(),
    )?;
    let written = write_decimal(&adjustment, 0);
    let closed = adjustment.close();
    written.and(closed)?;
    let process = nix::unistd::getpid().as_raw().unsigned_abs();
    // SAFETY: `setpriority`, which `nix` does not wrap, reads only these integers.
    Errno::result(unsafe { nix::libc::setpriority(nix::libc::PRIO_PROCESS, process, 0) }).map(drop)
}

/// Returns `descriptor` with close-on-exec set and a number above the standard streams',
/// duplicating it if its number is lower.
fn above_standard_streams(descriptor: FileDescriptor) -> Result<FileDescriptor, Errno> {
    if descriptor.raw() >= FIRST_NONSTANDARD {
        nix::fcntl::fcntl(descriptor.borrowed(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
        return Ok(descriptor);
    }
    let raised = nix::fcntl::fcntl(
        descriptor.borrowed(),
        FcntlArg::F_DUPFD_CLOEXEC(FIRST_NONSTANDARD),
    )?;
    // SAFETY: the kernel has just created `raised`, so it is open and nothing else owns it.
    Ok(unsafe { FileDescriptor::from_raw(raised) })
}

/// Returns the numbers of `descriptors` in ascending order, each once.
fn ascending_numbers<'a>(
    descriptors: impl IntoIterator<Item = &'a FileDescriptor>,
) -> Box<[RawFd]> {
    let mut numbers: Vec<RawFd> = descriptors.into_iter().map(FileDescriptor::raw).collect();
    numbers.sort_unstable();
    numbers.dedup();
    numbers.into_boxed_slice()
}

/// Closes, in a child, every descriptor from 3 up but those numbered in `kept`, which ascend
/// from 3 and hold each number once, through `close_range` over the gaps between them: those
/// that the caller opened without close-on-exec too, and the child's copies of other children's
/// descriptors, which would otherwise stay open until the child runs its program.
fn close_other_descriptors(kept: &[RawFd]) -> Result<(), Errno> {
    let mut first = FIRST_NONSTANDARD as c_uint;
    for &number in kept {
        // A kept descriptor's number is at least 3, so it converts.
        let number = number as c_uint;
        if number > first {
            close_range(first, number - 1)?;
        }
        first = number + 1;
    }
    close_range(first, c_uint::MAX)
}

/// Closes the descriptors numbered from `first` to `last`, with libc's `syscall`, which neither
/// allocates nor relies on the state that libc keeps for a thread.
fn close_range(first: c_uint, last: c_uint) -> Result<(), Errno> {
    // SAFETY: `close_range` takes three integers and dereferences nothing.
    let result =
        unsafe { nix::libc::syscall(nix::libc::SYS_close_range, first, last, 0 as c_uint) };
    if result < 0 {
        Err(Errno::last())
    } else {
        Ok(())
    }
}

/// Makes `root` the calling process's root directory and working directory.
pub(crate) fn enter_root(root: &FileDescriptor) -> Result<(), Errno> {
    nix::unistd::fchdir(root.borrowed())?;
    nix::unistd::chroot(c".")
}

/// Repeats `call` while it is interrupted.
fn retry(mut call: impl FnMut() -> Result<(), Errno>) -> Result<(), Errno> {
    loop {
        match call() {
            Err(Errno::EINTR) => {}
            result => return result,
        }
    }
}

/// Replaces the child with the program at `program`, with `arguments` and `environment`, or the
/// caller's environment without one, and returns the `errno` of the failure. `nix`'s `exec`
/// wrappers allocate their pointer vectors, so libc takes the ones that the parent prepared.
///
/// # Safety
///
/// This must run only in a post-fork child. `arguments` and `environment` must be
/// NUL-terminated vectors of pointers to C strings that outlive the call.
unsafe fn exec(
    program: &CStr,
    arguments: &[*const c_char],
    environment: Option<&[*const c_char]>,
) -> Errno {
    // SAFETY: the caller establishes that the vectors are NUL-terminated, and that every pointer
    // names a C string that outlives the call.
    unsafe {
        match environment {
            Some(environment) => {
                nix::libc::execve(program.as_ptr(), arguments.as_ptr(), environment.as_ptr())
            }
            None => nix::libc::execv(program.as_ptr(), arguments.as_ptr()),
        };
    }
    Errno::last()
}

/// Looks for the program `name` in the directories of `path`, a `PATH` value, as `execvp` does,
/// and returns the `errno` of the search's failure: it builds in `buffer`, in turn, the path of
/// `name` in each directory, in their order, and passes it to `run`, which runs the program there
/// or returns why not. An empty directory, such as the one between two adjacent colons, stands for
/// the working directory, so its path is the name alone.
///
/// A name longer than [`NAME_MAX`] fails with `ENAMETOOLONG` before any path, as `execvp`'s
/// does. A path that does not fit in `buffer` with its terminating NUL is skipped as one that
/// `run` refused with `ENAMETOOLONG`, which the kernel does for every path that a search's buffer
/// of [`PATH_MAX`] bytes cannot hold.
///
/// A path that does not exist, `ENOENT`, `ENOTDIR`, or `ENAMETOOLONG`, is skipped, as is one that
/// the child may not run, `EACCES`, which the search remembers. Any other failure, such as
/// `ENOEXEC` for a file that the kernel cannot run, ends the search with its `errno`. A search
/// that runs no path fails with `EACCES` if it skipped one that the child may not run, and
/// otherwise with the last path's `errno`.
fn search(
    name: &CStr,
    path: &CStr,
    buffer: &mut [u8],
    mut run: impl FnMut(&CStr) -> Errno,
) -> Errno {
    let name = name.to_bytes();
    if name.len() > NAME_MAX {
        return Errno::ENAMETOOLONG;
    }
    let mut refused = false;
    let mut last = Errno::ENOENT;
    for directory in path.to_bytes().split(|&byte| byte == b':') {
        let error = match place(buffer, directory, name) {
            // SAFETY: the path ends with the NUL that `place` wrote after parts of `name` and
            // `path`, which hold no other.
            Some(candidate) => run(unsafe { CStr::from_bytes_with_nul_unchecked(candidate) }),
            None => Errno::ENAMETOOLONG,
        };
        match error {
            Errno::EACCES => refused = true,
            error @ (Errno::ENOENT | Errno::ENOTDIR | Errno::ENAMETOOLONG) => last = error,
            error => return error,
        }
    }
    if refused { Errno::EACCES } else { last }
}

/// Writes in `buffer` the path of the program `name` in `directory`, `DIRECTORY/NAME`, or `NAME`
/// alone for an empty directory, which stands for the working directory, and its terminating NUL,
/// and returns them, or `None` if they do not fit.
fn place<'a>(buffer: &'a mut [u8], directory: &[u8], name: &[u8]) -> Option<&'a [u8]> {
    let start = if directory.is_empty() {
        0
    } else {
        directory.len() + 1
    };
    let end = start + name.len();
    let path = buffer.get_mut(..=end)?;
    if start > 0 {
        path[..directory.len()].copy_from_slice(directory);
        path[directory.len()] = b'/';
    }
    path[start..end].copy_from_slice(name);
    path[end] = 0;
    Some(path)
}

/// The kernel's `struct sigaction`, which `rt_sigaction` reads, unlike libc's, as x86-64 lays it
/// out: the handler, the flags, the restorer, and the mask of [`SIGNALS`] signals. Its default
/// value, all zeros, is the default disposition, without flags or a mask, and so it is to an
/// architecture whose shorter structure has no restorer.
#[repr(C)]
#[derive(Default)]
struct KernelSigaction {
    /// The handler: 0 is the default disposition.
    handler: usize,
    /// The `SA_` flags.
    flags: c_ulong,
    /// The restorer, which only a handler uses.
    restorer: usize,
    /// The signals that the handler blocks.
    mask: u64,
}

/// Gives every signal that a process may change its default disposition, then unblocks every
/// signal, so that the child's program ignores and blocks no signal because its caller does, as
/// `exec` would keep both. `SIGKILL` and `SIGSTOP` keep the dispositions that no process may
/// change.
///
/// The dispositions come first, so that a signal that arrived while blocked takes its default
/// action once unblocked, and never runs a handler of the caller's.
pub(crate) fn reset_signals() -> Result<(), Errno> {
    let default = KernelSigaction::default();
    for signal in 1..=SIGNALS {
        if signal == nix::libc::SIGKILL || signal == nix::libc::SIGSTOP {
            continue;
        }
        // SAFETY: `default` is a kernel `struct sigaction` that outlives the call, which returns
        // no former action.
        let result = unsafe {
            nix::libc::syscall(
                nix::libc::SYS_rt_sigaction,
                signal,
                &default,
                std::ptr::null_mut::<KernelSigaction>(),
                std::mem::size_of::<u64>(),
            )
        };
        if result < 0 {
            return Err(Errno::last());
        }
    }
    signal_mask(nix::libc::SIG_SETMASK, 0)
}

/// Makes `command`, the announcer's that `nvx-ingress start` runs, start its program in a session
/// of its own, without a controlling terminal, with every signal's default disposition, and none
/// blocked, as [`reset_signals`] leaves them, whatever the caller ignores or blocks.
pub(crate) fn configure_announcer(command: &mut std::process::Command) {
    // SAFETY: the hook runs in the child between fork and exec, where it captures nothing, and
    // makes only system calls, which neither allocate nor lock.
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid()?;
            reset_signals()?;
            Ok(())
        });
    }
}

/// Changes the calling thread's signal mask with `set`, a set of [`SIGNALS`] signals in which
/// bit `n - 1` stands for signal `n`, as `how` says.
fn signal_mask(how: c_int, set: u64) -> Result<(), Errno> {
    // SAFETY: `set` outlives the call, which returns no former mask.
    let result = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_rt_sigprocmask,
            how,
            &set,
            std::ptr::null_mut::<u64>(),
            std::mem::size_of::<u64>(),
        )
    };
    if result < 0 {
        Err(Errno::last())
    } else {
        Ok(())
    }
}

/// Reports a failed launch stage and ends the child at once.
///
/// The child blocks `SIGPIPE` first: a report that no reader takes would raise it, and its
/// default disposition, which a child that reset its signals has, would end the child before it
/// exits with its status.
///
/// # Safety
///
/// This must run only in a post-fork child, with `report` its launch report's writer.
unsafe fn fail(report: &FileDescriptor, stage: LaunchStage, error: Errno) -> ! {
    let mut bytes = [0u8; REPORT_LENGTH];
    bytes[..4].copy_from_slice(&stage.code().to_ne_bytes());
    bytes[4..].copy_from_slice(&(error as i32).to_ne_bytes());
    let _ = signal_mask(nix::libc::SIG_BLOCK, 1 << (nix::libc::SIGPIPE - 1));
    let _ = write_all(report, &bytes);
    // `nix` has no `_exit`, and `std::process::exit` runs cleanup.
    // SAFETY: the caller guarantees that this is the post-fork child.
    unsafe { nix::libc::_exit(LAUNCH_FAILURE_STATUS) }
}

/// Writes a decimal process ID and newline without allocation.
fn write_decimal(descriptor: &FileDescriptor, value: nix::libc::pid_t) -> Result<(), Errno> {
    let mut bytes = [0u8; 32];
    let mut position = bytes.len() - 1;
    bytes[position] = b'\n';
    let mut value = value as u32;
    loop {
        position -= 1;
        bytes[position] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    write_all(descriptor, &bytes[position..])
}

/// Writes all bytes with direct async-signal-safe calls, retrying interruptions. A write that
/// makes no progress is `EIO`.
fn write_all(descriptor: &FileDescriptor, mut bytes: &[u8]) -> Result<(), Errno> {
    while !bytes.is_empty() {
        match nix::unistd::write(descriptor.borrowed(), bytes) {
            Ok(0) => return Err(Errno::EIO),
            Ok(count) => bytes = &bytes[count..],
            Err(Errno::EINTR) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests;
