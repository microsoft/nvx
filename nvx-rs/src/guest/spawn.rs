//! Starting a program in a new process, for any guest init or agent.
//!
//! A [`Command`] describes a program and how its process starts: its arguments and environment, a
//! `PATH` in which it looks for a program that it names without a `/`, if any, an optional root
//! directory and a working directory in it, the descriptors of its standard streams and of any
//! others that it inherits, the cgroup that it joins, whether it leads a new process group or
//! session, whether it waits at a [`Barrier`] until its caller releases it, whether it resets its
//! signals, and the execution that it [enters](Command::enter), if any. [`Command::spawn`]
//! allocates and opens everything that the child needs before it starts the child, with `fork`,
//! or for an entry with `clone3`; the child then makes only system calls over that storage, in
//! the order of [`LaunchStage`], and replaces itself with the program. A caller may therefore
//! spawn from any thread.
//!
//! A command owns the descriptors that it takes, and hands each one over once: the child places
//! the standard streams and keeps the inherited descriptors open at their numbers, and the
//! caller's process closes all of them as soon as it has started the child, so that the reader
//! of a workload's pipe sees end of file once the program and its descendants close their ends.
//! Before the child starts, a command moves any descriptor that the child uses after it places
//! its standard streams above their numbers and makes it close-on-exec, so the program receives
//! the command's descriptors only as its standard streams and inherited descriptors. Descriptors
//! that the caller opened without close-on-exec stay open in the program, as across any `exec`,
//! except in the program of a child that waits at a [`Barrier`] or enters an execution: such a
//! child closes every descriptor that it does not use before it waits or enters, so that it holds
//! no copy of another child's pipes while it waits, and the program of an entry none of its
//! caller's.
//!
//! The signals that the caller ignores, and its thread's signal mask, also pass to the program,
//! as across any `exec`, unless the command [resets its signals](Command::reset_signals), as an
//! entry's does: a caller that ignores `SIGPIPE`, as Rust programs do, or that blocks `SIGCHLD`
//! to collect its children through a signal descriptor, would otherwise start every program so.
//!
//! No step needs NVX's control protocol, its managed agent and helper, or its workload identity:
//! the caller chooses the program, and the identity, if any, that an entry gives it, or as which
//! the child checks its working directory.
//!
//! The caller owns the child. [`Spawned::id`] names it, and the caller collects its status, for
//! example through a [`process::Child`](super::process::Child): this module never waits for a
//! process. [`LaunchReport::wait`] tells whether the program started, or which stage failed and
//! why.
//!
//! ```no_run
//! use std::fs::File;
//!
//! use nvx::guest::spawn::Command;
//!
//! # fn main() -> std::io::Result<()> {
//! let spawned = Command::new(c"/bin/echo".to_owned(), vec![c"echo".to_owned(), c"hi".to_owned()])
//!     .working_directory(c"/tmp".to_owned())
//!     .stdin(File::open("/dev/null")?)
//!     .new_process_group()
//!     .spawn()?;
//! let process = spawned.id();
//! if let Some(failure) = spawned.into_report().wait()? {
//!     eprintln!("process {process} did not start: {failure}");
//! }
//! // The caller reaps `process`.
//! # Ok(())
//! # }
//! ```

use std::ffi::CString;
use std::fmt;
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::Path;

use super::sys::{
    self, ChildSpec, Errno, FileDescriptor, Mode, OpenFlags, PreparedChild, ProcessId,
    REPORT_LENGTH,
};

/// A step that a child takes after it starts and before its program runs.
///
/// The variants are in the order of the steps, and a child skips the steps that its [`Command`]
/// does not ask for. A child whose step fails reports it and exits with status 125 without
/// running its program. The steps from [`Propagation`](Self::Propagation) to
/// [`Devices`](Self::Devices), and from [`Bounding`](Self::Bounding) to
/// [`NoNewPrivileges`](Self::NoNewPrivileges), are an execution's [entry](super::entry), which
/// [`Command::enter`] asks for; the child of an entry also takes the [`Signals`](Self::Signals)
/// step, which [`Command::reset_signals`] asks for otherwise.
///
/// Each stage has a number, its [`code`](Self::code), with which the child's launch report names
/// the stage that failed.
/// [`from_code`](Self::from_code) turns a number back into its stage. The numbers are part of the
/// API: a stage keeps its number, and a new stage takes a number of its own, wherever it comes in
/// the order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LaunchStage {
    /// The child makes itself the leader of a new process group.
    ProcessGroup,
    /// The child makes itself the leader of a new session, and of its process group, without a
    /// controlling terminal.
    Session,
    /// The child closes every descriptor that it does not use, then waits until its caller
    /// releases its [`Barrier`].
    Barrier,
    /// The child places its standard streams and keeps its inherited descriptors open. The child
    /// of an entry first closes every descriptor that it does not use.
    Descriptors,
    /// The child joins its cgroup.
    Cgroup,
    /// The child makes every mount of its new mount namespace private.
    Propagation,
    /// The child sets the hostname of its new UTS namespace.
    Hostname,
    /// The child moves its root over `/`, and makes it its root and working directory.
    MoveRoot,
    /// The child clears its umask, then mounts its procfs, and restricts it for user ID 0.
    Proc,
    /// The child mounts its read-only sysfs, and restricts it for user ID 0.
    Sys,
    /// The child mounts its `/dev`, with its device nodes, devpts, and shm, then restores its
    /// umask.
    Devices,
    /// The child makes the root directory its root.
    Root,
    /// The child drops every capability but its own from its bounding set.
    Bounding,
    /// The child takes its supplementary groups, group ID, and user ID.
    Identity,
    /// The child applies its capabilities, and empties its inheritable and ambient sets.
    Capabilities,
    /// The child sets `no_new_privs`.
    NoNewPrivileges,
    /// The child takes the identity of its working-directory check, or gives it back after.
    Credentials,
    /// The child enters its working directory. The child of an entry first closes every
    /// descriptor but its program's and its launch report's, and resolves the directory through
    /// no magic link.
    WorkingDirectory,
    /// The child records the physical path of its working directory in `PWD`.
    PhysicalWorkingDirectory,
    /// The child gives every signal its default disposition, then unblocks every signal.
    Signals,
    /// The child replaces itself with its program: the first of those that its
    /// [search](Command::search_path) finds that it can run, if its command searches for it.
    Exec,
}

impl LaunchStage {
    /// Every stage, in order.
    const ALL: [Self; 21] = [
        Self::ProcessGroup,
        Self::Session,
        Self::Barrier,
        Self::Descriptors,
        Self::Cgroup,
        Self::Propagation,
        Self::Hostname,
        Self::MoveRoot,
        Self::Proc,
        Self::Sys,
        Self::Devices,
        Self::Root,
        Self::Bounding,
        Self::Identity,
        Self::Capabilities,
        Self::NoNewPrivileges,
        Self::Credentials,
        Self::WorkingDirectory,
        Self::PhysicalWorkingDirectory,
        Self::Signals,
        Self::Exec,
    ];

    /// Returns the stage's number: from 1, for [`ProcessGroup`](Self::ProcessGroup), to 21, for
    /// [`Exec`](Self::Exec), in the order of the stages. A stage that NVX adds takes 22, then 23,
    /// and so on, wherever it comes in the order, and no stage is numbered 0.
    pub const fn code(self) -> u32 {
        match self {
            Self::ProcessGroup => 1,
            Self::Session => 2,
            Self::Barrier => 3,
            Self::Descriptors => 4,
            Self::Cgroup => 5,
            Self::Propagation => 6,
            Self::Hostname => 7,
            Self::MoveRoot => 8,
            Self::Proc => 9,
            Self::Sys => 10,
            Self::Devices => 11,
            Self::Root => 12,
            Self::Bounding => 13,
            Self::Identity => 14,
            Self::Capabilities => 15,
            Self::NoNewPrivileges => 16,
            Self::Credentials => 17,
            Self::WorkingDirectory => 18,
            Self::PhysicalWorkingDirectory => 19,
            Self::Signals => 20,
            Self::Exec => 21,
        }
    }

    /// Returns the stage that `code` numbers, or `None` if no stage has that number, such as 0.
    pub fn from_code(code: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|stage| stage.code() == code)
    }
}

impl fmt::Display for LaunchStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ProcessGroup => "process group",
            Self::Session => "session",
            Self::Barrier => "barrier",
            Self::Descriptors => "descriptors",
            Self::Cgroup => "cgroup",
            Self::Propagation => "mount propagation",
            Self::Hostname => "hostname",
            Self::MoveRoot => "root move",
            Self::Proc => "procfs",
            Self::Sys => "sysfs",
            Self::Devices => "devices",
            Self::Root => "root directory",
            Self::Bounding => "bounding set",
            Self::Identity => "identity",
            Self::Capabilities => "capabilities",
            Self::NoNewPrivileges => "no new privileges",
            Self::Credentials => "credentials",
            Self::WorkingDirectory => "working directory",
            Self::PhysicalWorkingDirectory => "physical working directory",
            Self::Signals => "signals",
            Self::Exec => "exec",
        })
    }
}

/// The launch stage at which a child stopped, and the `errno` of the failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaunchFailure {
    /// The stage that failed.
    stage: LaunchStage,
    /// The `errno` with which it failed.
    error: i32,
}

impl LaunchFailure {
    /// Returns the stage that failed.
    pub fn stage(&self) -> LaunchStage {
        self.stage
    }

    /// Returns the `errno` with which the stage failed.
    pub fn raw_os_error(&self) -> i32 {
        self.error
    }
}

impl fmt::Display for LaunchFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "the {} launch stage failed: {}",
            self.stage,
            io::Error::from_raw_os_error(self.error)
        )
    }
}

impl std::error::Error for LaunchFailure {}

/// A cgroup v2 that a child joins before its program runs.
#[derive(Debug)]
pub struct Cgroup {
    /// The cgroup's membership file, open for writing.
    procs: FileDescriptor,
}

impl Cgroup {
    /// Opens the membership file, `cgroup.procs`, of the cgroup v2 directory `directory`. The
    /// child writes its process ID to it.
    pub fn open(directory: &Path) -> io::Result<Self> {
        let path = sys::path_c_string(&directory.join("cgroup.procs"))?;
        let procs = sys::open(
            &path,
            OpenFlags::O_WRONLY | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self { procs })
    }

    /// Opens the membership file of the cgroup v2 whose directory `directory` holds open, so
    /// that a child joins that cgroup even once another cgroup takes its path.
    pub(crate) fn open_in(directory: &FileDescriptor) -> io::Result<Self> {
        let procs = sys::open_at(
            directory,
            c"cgroup.procs",
            OpenFlags::O_WRONLY | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self { procs })
    }
}

/// A program, and how a new process starts it.
///
/// By default the child keeps its parent's environment, root and working directories, standard
/// streams, process group, cgroup, ignored signals, and signal mask, inherits no other descriptor
/// of the command's, and runs the program at the path that the command names. Each method
/// changes one of them.
#[derive(Debug)]
pub struct Command {
    /// The child's inputs.
    spec: ChildSpec,
}

impl Command {
    /// Describes a process that runs the executable at `program`, which is not searched for in a
    /// `PATH` unless the command [searches one](Self::search_path), with `arguments`, argument
    /// zero first. A command without arguments fails to spawn.
    pub fn new(program: CString, arguments: Vec<CString>) -> Self {
        Self {
            spec: ChildSpec::new(program, arguments),
        }
    }

    /// Gives the program exactly the `NAME=value` entries of `environment`, instead of the
    /// parent's environment.
    pub fn environment(mut self, environment: Vec<CString>) -> Self {
        self.spec.set_environment(environment);
        self
    }

    /// Looks for a program that the command names without a `/` in the directories of `path`, a
    /// `PATH` value, as `execvp` does, once the child has entered its root and working directory:
    /// the child runs the first `DIRECTORY/NAME`, in the order of the directories, that it can
    /// run, where an empty directory, such as the one between two adjacent colons, stands for the
    /// working directory. A program whose name is empty or holds a `/` runs as the command names
    /// it. The child searches `path`, not the `PATH` of the program's environment: a caller whose
    /// program's own `PATH` should decide passes that one.
    ///
    /// The search takes the same memory however long the name and `path` are, as `execvp`'s does:
    /// one buffer of `PATH_MAX` bytes, 4096, which the command prepares before the child starts,
    /// and in which the child builds each path in turn, so that the child allocates nothing. A
    /// path that does not fit in the buffer with its terminating NUL is one that the kernel
    /// refuses with `ENAMETOOLONG`, and the child skips it as such without trying it. A name
    /// longer than `NAME_MAX`, 255 bytes, fails the [stage](LaunchStage::Exec) with
    /// `ENAMETOOLONG` before the child tries a path, as it does in `execvp`.
    ///
    /// The child skips a path that does not exist, which `ENOENT`, `ENOTDIR`, or `ENAMETOOLONG`
    /// reports, and one that it may not run, `EACCES`, which it remembers. Any other failure,
    /// such as `ENOEXEC` for a file that the kernel cannot run, ends the search: unlike some
    /// `execvp`s, the child runs no shell for such a file. A search that runs nothing fails the
    /// [stage](LaunchStage::Exec) with `EACCES` if the child skipped a path that it may not run,
    /// and otherwise with the last path's error.
    pub fn search_path(mut self, path: CString) -> Self {
        self.spec.set_search_path(path);
        self
    }

    /// Makes the directory that `root` opens the child's root directory, and its working
    /// directory unless [`working_directory`](Self::working_directory) names another one. Changing
    /// the root needs the privilege to `chroot`.
    pub fn root(mut self, root: impl Into<OwnedFd>) -> Self {
        self.spec.set_root(FileDescriptor::from_owned(root.into()));
        self
    }

    /// Makes the absolute path `directory`, which the child resolves in its root, its working
    /// directory. A relative path fails to spawn.
    pub fn working_directory(mut self, directory: CString) -> Self {
        self.spec.set_directory(directory);
        self
    }

    /// Makes the child enter its working directory with the effective user ID `uid`, the
    /// effective group ID `gid`, and no supplementary groups, then take back its own, so that the
    /// directory's permissions apply as they will to a program that runs as `uid` and `gid`.
    ///
    /// The check needs a [working directory](Self::working_directory), without which the command
    /// fails to spawn, and the privilege to change the child's credentials.
    pub fn check_working_directory_as(mut self, uid: u32, gid: u32) -> Self {
        self.spec.set_credentials(uid, gid);
        self
    }

    /// Sets `PWD`, in the place of the environment's first `PWD` entry or after the last entry,
    /// to the physical path of the child's working directory once the child has entered it.
    ///
    /// The path may hold 4095 bytes. The child records it without allocating, in an entry of
    /// the [environment](Self::environment), without which the command fails to spawn. A longer
    /// path fails the [stage](LaunchStage::PhysicalWorkingDirectory) with `ENAMETOOLONG`, and a
    /// working directory outside the child's root, which has no path in it, with `ENOENT`.
    pub fn export_physical_working_directory(mut self) -> Self {
        self.spec.export_physical_pwd();
        self
    }

    /// Makes `descriptor` the program's standard input.
    pub fn stdin(mut self, descriptor: impl Into<OwnedFd>) -> Self {
        self.spec
            .set_stream(0, FileDescriptor::from_owned(descriptor.into()));
        self
    }

    /// Makes `descriptor` the program's standard output.
    pub fn stdout(mut self, descriptor: impl Into<OwnedFd>) -> Self {
        self.spec
            .set_stream(1, FileDescriptor::from_owned(descriptor.into()));
        self
    }

    /// Makes `descriptor` the program's standard error.
    pub fn stderr(mut self, descriptor: impl Into<OwnedFd>) -> Self {
        self.spec
            .set_stream(2, FileDescriptor::from_owned(descriptor.into()));
        self
    }

    /// Keeps `descriptor` open in the program at its current number, which the caller passes to
    /// the program, for example as an argument. A descriptor numbered 0, 1, or 2, which are the
    /// standard streams', fails to spawn.
    pub fn inherit(mut self, descriptor: impl Into<OwnedFd>) -> Self {
        self.spec
            .inherit(FileDescriptor::from_owned(descriptor.into()));
        self
    }

    /// Makes the child join `cgroup` once it has placed its descriptors, before the rest of its
    /// stages and its program, so that the program and its descendants start in it.
    pub fn cgroup(mut self, cgroup: Cgroup) -> Self {
        self.spec.set_cgroup(cgroup.procs);
        self
    }

    /// Makes the child the leader of a new process group, whose ID is its process ID.
    pub fn new_process_group(mut self) -> Self {
        self.spec.new_process_group();
        self
    }

    /// Makes the child the leader of a new session, without a controlling terminal, and of its
    /// process group, whose IDs are its process ID. The session replaces a
    /// [new process group](Self::new_process_group).
    pub fn new_session(mut self) -> Self {
        self.spec.new_session();
        self
    }

    /// Makes the child wait, before it places its descriptors, until its caller releases the
    /// [`Barrier`] that [`Spawned::take_barrier`] returns, for example once it has placed the
    /// child in its cgroup. A child whose barrier is dropped unreleased, with the [`Spawned`] that
    /// holds it too, fails the [stage](LaunchStage::Barrier) with `ECANCELED` and never runs its
    /// program.
    ///
    /// Before it waits, the child closes every descriptor that it does not use, those that the
    /// caller opened without close-on-exec too, so that it holds no copy of the descriptors of a
    /// child that the caller starts meanwhile, which would keep that child's pipes open.
    pub fn barrier(mut self) -> Self {
        self.spec.set_barrier();
        self
    }

    /// Gives every signal its default disposition, and then blocks none, just before the program
    /// runs, so that the program ignores and blocks no signal because its caller does, as `exec`
    /// would keep both: a write to a pipe without a reader then ends the program by `SIGPIPE`,
    /// which Rust programs ignore, and a `SIGCHLD` reaches it once a child of its own ends. The
    /// child of an [entry](Self::enter) always does.
    pub fn reset_signals(mut self) -> Self {
        self.spec.reset_signals();
        self
    }

    /// Makes the child enter the execution that `entry` prepares before its program runs, as the
    /// [entry](super::entry) module describes: its namespaces, view, root, and identity. The
    /// child then leads a [new session](Self::new_session), closes every descriptor but those that
    /// its program receives, and enters its working directory, by default `/`, with that
    /// identity, through no magic link, such as `/proc/self/cwd` or a `/proc/self/fd` entry, which
    /// could name a directory outside the root. Its program starts with every signal's default
    /// disposition and none blocked, as [`reset_signals`](Self::reset_signals) has it.
    ///
    /// The command must also place the child: it needs a [cgroup](Self::cgroup) or a
    /// [barrier](Self::barrier), at which its caller places it. It may name neither a
    /// [root](Self::root) nor a [check](Self::check_working_directory_as), which the entry's root
    /// and identity replace. A command that breaks either rule fails to spawn.
    pub fn enter(mut self, entry: super::entry::Prepared) -> Self {
        self.spec.set_entry(entry.into_child());
        self
    }

    /// Starts the child.
    ///
    /// The call returns once it has started the child, before the child's launch stages finish;
    /// the child's [`LaunchReport`] tells how they end. It fails without a child if the command
    /// is invalid, or if a resource that the child needs cannot be allocated or opened. On
    /// x86-64, it also fails, with `PermissionDenied`, while the calling thread may still hold
    /// the permission for OpenVMM's I/O ports that a time call could not revoke, as
    /// `TimeError::PortRelease` reports, because the child would inherit it.
    pub fn spawn(self) -> io::Result<Spawned> {
        let started = PreparedChild::new(self.spec)?.spawn()?;
        Ok(Spawned {
            process: started.process,
            report: LaunchReport {
                reader: started.report,
            },
            barrier: started.barrier.map(|writer| Barrier { writer }),
        })
    }
}

/// A child that [`Command::spawn`] started.
///
/// Dropping it neither waits for the child nor stops it, but it drops the child's
/// [`Barrier`], if the caller has not taken it, which cancels the child's launch.
#[derive(Debug)]
pub struct Spawned {
    /// The child's process ID.
    process: ProcessId,
    /// The report of the child's launch.
    report: LaunchReport,
    /// The barrier at which the child waits, until the caller takes it.
    barrier: Option<Barrier>,
}

impl Spawned {
    /// Returns the child's process ID, which is also its process group's ID when its command
    /// asked for a [new process group](Command::new_process_group) or a
    /// [new session](Command::new_session).
    pub fn id(&self) -> u32 {
        self.process.raw().unsigned_abs()
    }

    /// Returns the child's process ID for the crate's own callers.
    pub(crate) fn process(&self) -> ProcessId {
        self.process
    }

    /// Takes the [`Barrier`] at which the child waits, if its command asked for
    /// [one](Command::barrier) and it has not been taken.
    pub fn take_barrier(&mut self) -> Option<Barrier> {
        self.barrier.take()
    }

    /// Returns the report of the child's launch.
    pub fn into_report(self) -> LaunchReport {
        self.report
    }
}

/// The barrier at which a child waits before it places its descriptors, until its caller
/// [releases](Self::release) it.
///
/// Dropping it unreleased cancels the child's launch: the child reads end of file, fails the
/// [stage](LaunchStage::Barrier) with `ECANCELED`, and never runs its program. So a caller that
/// cannot place the child, or that cancels it or times out first, never releases it by mistake.
/// The drop cancels the launch at once, even while another child that started meanwhile, and has
/// not run its program yet, holds a copy of the barrier's descriptor, which closing it alone
/// would leave open.
#[derive(Debug)]
pub struct Barrier {
    /// The caller's end of the barrier's connected socket pair, which the drop shuts down.
    writer: FileDescriptor,
}

impl Barrier {
    /// Lets the child continue its launch.
    ///
    /// A child that has ended already fails the call with `BrokenPipe`, without a `SIGPIPE` for
    /// the caller; its launch report tells why it ended.
    pub fn release(self) -> io::Result<()> {
        match sys::write_without_sigpipe(&self.writer, &[1]) {
            Ok(1) => Ok(()),
            Ok(_) => Err(io::ErrorKind::WriteZero.into()),
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for Barrier {
    fn drop(&mut self) {
        // The child of a released barrier reads the release before the end of file. A failure
        // leaves only the close, which still ends the wait once no copy remains.
        let _ = self.writer.shut_down_sending();
    }
}

/// The outcome of a child's launch stages.
#[derive(Debug)]
pub struct LaunchReport {
    /// The read end of the child's report pipe, whose write end only the child holds.
    reader: FileDescriptor,
}

impl LaunchReport {
    /// Lends the report's pipe, which polls readable, or hung up, once the report has arrived:
    /// the child wrote it, or replaced itself with its program or exited without one.
    pub(crate) fn descriptor(&self) -> BorrowedFd<'_> {
        self.reader.borrowed()
    }

    /// Waits until the child's program runs, which returns `None`, or a launch stage fails,
    /// which returns the failure.
    ///
    /// The wait ends when the child replaces itself with its program or exits. A report that is
    /// cut short or names no stage is `EPROTO`.
    pub fn wait(self) -> io::Result<Option<LaunchFailure>> {
        let mut bytes = [0u8; REPORT_LENGTH];
        let mut length = 0;
        while length < bytes.len() {
            match self.reader.read(&mut bytes[length..]) {
                Ok(0) => break,
                Ok(count) => length += count,
                Err(Errno::EINTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if length == 0 {
            return Ok(None);
        }
        let stage = u32::from_ne_bytes(bytes[..4].try_into().expect("four bytes"));
        let error = i32::from_ne_bytes(bytes[4..].try_into().expect("four bytes"));
        match LaunchStage::from_code(stage) {
            Some(stage) if length == REPORT_LENGTH => Ok(Some(LaunchFailure { stage, error })),
            _ => Err(Errno::EPROTO.into()),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
