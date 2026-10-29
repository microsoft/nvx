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
//! the child checks its working directory. NVX's managed agent is one caller.
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
/// the stage that failed, and which a [`Binding`](super::exec::binding::Binding) reports to its
/// runner as the stage of a [`LaunchFailure`](crate::exec::LaunchFailure).
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

    /// Returns the stage that `code` numbers, or `None` if no stage has that number, such as 0,
    /// which a [`Binding`](super::exec::binding::Binding) reports for a launch whose report it
    /// could not read.
    ///
    /// ```
    /// use nvx::exec::{LaunchFailure, Outcome};
    /// use nvx::guest::spawn::LaunchStage;
    ///
    /// /// Returns whether the program of `outcome` did not start because its working directory
    /// /// was missing or out of its reach.
    /// fn lacks_working_directory(outcome: &Outcome) -> bool {
    ///     outcome.launch_failure().is_some_and(|failure| {
    ///         LaunchStage::from_code(failure.stage()) == Some(LaunchStage::WorkingDirectory)
    ///     })
    /// }
    ///
    /// let failure = LaunchFailure::new(LaunchStage::WorkingDirectory.code(), 2);
    /// assert!(lacks_working_directory(
    ///     &Outcome::new(None).with_launch_failure(failure)
    /// ));
    /// assert_eq!(LaunchStage::from_code(0), None);
    /// ```
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

    /// Prepares the child without starting it, for focused launch tests.
    #[cfg(test)]
    pub(crate) fn prepare(self) -> PreparedChild {
        PreparedChild::new(self.spec).expect("the command prepares")
    }

    /// Starts the child as [`spawn`](Self::spawn) does, and returns with it a copy of its launch
    /// report's writer: the report arrives only once the copy closes too, as it does when a
    /// process that another thread forked while the child was spawning holds the writer, for
    /// focused tests.
    #[cfg(test)]
    pub(crate) fn spawn_holding_report(self) -> io::Result<(Spawned, OwnedFd)> {
        let child = PreparedChild::new(self.spec)?;
        let writer = child.report_writer_copy()?;
        let started = child.spawn()?;
        let spawned = Spawned {
            process: started.process,
            report: LaunchReport {
                reader: started.report,
            },
            barrier: started.barrier.map(|barrier| Barrier { writer: barrier }),
        };
        Ok((spawned, writer.into()))
    }

    /// Replaces the system calls of the child's credential stage, which then succeed, or fail
    /// with `failure`, without privileges.
    #[cfg(test)]
    pub(crate) fn simulate_credentials(mut self, failure: Option<Errno>) -> Self {
        self.spec.simulate_credentials(failure);
        self
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

/// Returns a command that runs `/bin/sh -c script`, with `$0` set to `sh`, for the tests of the
/// libraries that start programs.
#[cfg(test)]
pub(crate) fn shell(script: &str) -> Command {
    Command::new(
        c"/bin/sh".to_owned(),
        vec![
            c"sh".to_owned(),
            c"-c".to_owned(),
            CString::new(script).unwrap(),
        ],
    )
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;
    use std::io::{Read, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::guest::sys::ChildStatus;

    /// Returns `path` as a C string.
    fn c_path(path: &Path) -> CString {
        CString::new(path.as_os_str().as_bytes()).unwrap()
    }

    /// Returns the bytes that `reader` yields until end of file.
    fn read_all(mut reader: std::io::PipeReader) -> Vec<u8> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        bytes
    }

    /// Spawns `command` with piped standard output and error, and returns its launch failure,
    /// status, output, and error output.
    fn run(command: Command) -> (Option<LaunchFailure>, Option<ChildStatus>, Vec<u8>, Vec<u8>) {
        let (stdout, stdout_writer) = std::io::pipe().unwrap();
        let (stderr, stderr_writer) = std::io::pipe().unwrap();
        let spawned = command
            .stdout(stdout_writer)
            .stderr(stderr_writer)
            .spawn()
            .unwrap();
        let process = spawned.process();
        let failure = spawned.into_report().wait().unwrap();
        // The command took the writers, so the readers end when the child and its descendants
        // close theirs.
        let output = read_all(stdout);
        let error = read_all(stderr);
        (failure, sys::wait(process), output, error)
    }

    #[test]
    fn a_program_runs_with_its_arguments_environment_and_streams() {
        let (stdin, mut stdin_writer) = std::io::pipe().unwrap();
        stdin_writer.write_all(b"input\n").unwrap();
        drop(stdin_writer);
        let command = Command::new(
            c"/bin/sh".to_owned(),
            vec![
                c"zero".to_owned(),
                c"-c".to_owned(),
                c"read -r line; printf '%s|%s|%s|%s' \"$0\" \"$1\" \"$VALUE\" \"$line\"; printf err >&2"
                    .to_owned(),
                c"argument zero".to_owned(),
                c"one".to_owned(),
            ],
        )
        .environment(vec![c"VALUE=exact".to_owned()])
        .stdin(stdin);
        let (failure, status, output, error) = run(command);
        assert_eq!(failure, None);
        assert_eq!(status, Some(ChildStatus::Exited(0)));
        assert_eq!(output, b"argument zero|one|exact|input");
        assert_eq!(error, b"err");
    }

    #[test]
    fn a_program_without_an_environment_inherits_its_parent_s() {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let (failure, status, output, _) = run(shell("printf %s \"$PATH\""));
        assert_eq!((failure, status), (None, Some(ChildStatus::Exited(0))));
        assert_eq!(output, path.as_bytes());
    }

    #[test]
    fn inherited_descriptors_stay_open_at_their_numbers_and_others_close() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"handed over").unwrap();
        std::io::Seek::rewind(&mut file).unwrap();
        let inherited = std::os::fd::AsRawFd::as_raw_fd(&file);
        let unrelated = tempfile::tempfile().unwrap();
        let other = std::os::fd::AsRawFd::as_raw_fd(&unrelated);
        // `cat` reads the inherited descriptor through its own descriptor table, because `sh`
        // accepts only single-digit redirections.
        let (failure, status, output, _) = run(shell(&format!(
            "cat /proc/self/fd/{inherited}; test ! -e /proc/self/fd/{other} && printf ' closed'"
        ))
        .inherit(file));
        assert_eq!((failure, status), (None, Some(ChildStatus::Exited(0))));
        assert_eq!(output, b"handed over closed");
        drop(unrelated);
    }

    #[test]
    fn the_working_directory_and_physical_pwd_follow_the_command() {
        let directory = tempfile::tempdir().unwrap();
        let physical = directory.path().join("physical");
        std::fs::create_dir(&physical).unwrap();
        std::os::unix::fs::symlink(&physical, directory.path().join("link")).unwrap();
        let requested = directory.path().join("link/.");
        let (failure, status, output, _) = run(shell("printf '%s|' \"$PWD\"; pwd -P")
            .environment(vec![c"PWD=/stale".to_owned(), c"KEPT=1".to_owned()])
            .working_directory(c_path(&requested))
            .export_physical_working_directory());
        assert_eq!((failure, status), (None, Some(ChildStatus::Exited(0))));
        let physical = std::fs::canonicalize(&physical).unwrap();
        let expected = [
            physical.as_os_str().as_bytes(),
            b"|",
            physical.as_os_str().as_bytes(),
            b"\n",
        ]
        .concat();
        assert_eq!(output, expected);
    }

    #[test]
    fn a_program_named_without_a_slash_is_searched_for_in_the_command_s_path() {
        // No program here was ever open for writing, which a child that another thread forks
        // meanwhile could still hold, and fail the `exec` with `ETXTBSY`: the one that runs is a
        // link to the shell, and the empty one is made by `mknod`.
        let directory = tempfile::tempdir().unwrap();
        let path = |name: &str| directory.path().join(name);
        for name in ["found", "denied", "empty", "other"] {
            std::fs::create_dir(path(name)).unwrap();
        }
        // `found/prog` runs; no one may run `denied/prog`; the kernel cannot run the empty
        // `empty/prog`; and `file` is no directory.
        std::os::unix::fs::symlink("/bin/sh", path("found/prog")).unwrap();
        std::fs::write(path("denied/prog"), b"").unwrap();
        nix::sys::stat::mknod(
            &path("empty/prog"),
            nix::sys::stat::SFlag::S_IFREG,
            Mode::from_bits_truncate(0o755),
            0,
        )
        .unwrap();
        std::fs::set_permissions(path("empty/prog"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::write(path("file"), b"").unwrap();
        let named = |program: &CStr| {
            Command::new(
                program.to_owned(),
                vec![
                    c"prog".to_owned(),
                    c"-c".to_owned(),
                    c"printf ran".to_owned(),
                ],
            )
        };
        // An empty name stands for the empty directory, the working directory.
        let searched = |directories: &[&str]| {
            let directories: Vec<String> = directories
                .iter()
                .map(|&name| match name {
                    "" => String::new(),
                    name => path(name).display().to_string(),
                })
                .collect();
            named(c"prog").search_path(CString::new(directories.join(":")).unwrap())
        };
        for command in [
            searched(&["missing", "file", "denied", "found", "empty"]),
            searched(&["missing", ""]).working_directory(c_path(&path("found"))),
            // A name with a slash is the program's path, from the working directory.
            named(c"found/prog")
                .search_path(c_path(&path("other")))
                .working_directory(c_path(directory.path())),
        ] {
            let (failure, status, output, _) = run(command);
            assert_eq!((failure, status), (None, Some(ChildStatus::Exited(0))));
            assert_eq!(output, b"ran");
        }
        // A search that runs nothing fails with `EACCES` once it has met a program that it may
        // not run, and otherwise with its last path's error. A program that the kernel cannot run
        // ends it.
        for (command, error) in [
            (searched(&["missing", "denied", "file"]), Errno::EACCES),
            (searched(&["missing", "file"]), Errno::ENOTDIR),
            (searched(&["file", "missing"]), Errno::ENOENT),
            (searched(&["empty", "found"]), Errno::ENOEXEC),
            (
                named(c"found/prog")
                    .search_path(c_path(directory.path()))
                    .working_directory(c_path(&path("other"))),
                Errno::ENOENT,
            ),
        ] {
            let (failure, status, output, _) = run(command);
            let failure = failure.expect("the search fails");
            assert_eq!(
                (failure.stage(), failure.raw_os_error()),
                (LaunchStage::Exec, error as i32)
            );
            assert_eq!(status, Some(ChildStatus::Exited(125)));
            assert!(output.is_empty());
        }
    }

    /// Returns a command that looks for the program `name` in `directories`, and runs it as
    /// `sh -c 'printf ran'`.
    fn searched_for(name: &str, directories: &[&str]) -> Command {
        Command::new(
            CString::new(name).unwrap(),
            vec![c"sh".to_owned(), c"-c".to_owned(), c"printf ran".to_owned()],
        )
        .search_path(CString::new(directories.join(":")).unwrap())
    }

    /// Runs `command`, whose search fails, and returns the search's error once the child has
    /// exited with the launch failure status without output.
    fn search_error(command: Command) -> i32 {
        let (failure, status, output, _) = run(command);
        assert_eq!(status, Some(ChildStatus::Exited(125)));
        assert!(output.is_empty());
        let failure = failure.expect("the search fails");
        assert_eq!(failure.stage(), LaunchStage::Exec);
        failure.raw_os_error()
    }

    #[test]
    fn a_search_skips_a_path_longer_than_path_max() {
        let directory = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/bin/sh", directory.path().join("prog")).unwrap();
        let found = directory.path().display().to_string();
        let missing = directory.path().join("missing").display().to_string();
        // The path from this directory to any name is longer than `PATH_MAX`.
        let long = format!("/{}", "d/".repeat(2048));
        let (failure, status, output, _) = run(searched_for("prog", &[&long, &found]));
        assert_eq!((failure, status), (None, Some(ChildStatus::Exited(0))));
        assert_eq!(output, b"ran");
        // The search skips that directory as if the kernel had refused its path with
        // `ENAMETOOLONG`, which a later path's error replaces.
        assert_eq!(
            search_error(searched_for("prog", &[&missing, &long])),
            Errno::ENAMETOOLONG as i32
        );
        assert_eq!(
            search_error(searched_for("prog", &[&long, &missing])),
            Errno::ENOENT as i32
        );
    }

    #[test]
    fn a_search_refuses_a_name_longer_than_name_max() {
        let directory = tempfile::tempdir().unwrap();
        let found = directory.path().display().to_string();
        let missing = directory.path().join("missing").display().to_string();
        // The longest name that a search looks for, 255 bytes, is found.
        let longest = "n".repeat(255);
        std::os::unix::fs::symlink("/bin/sh", directory.path().join(&longest)).unwrap();
        let (failure, status, output, _) = run(searched_for(&longest, &[&missing, &found]));
        assert_eq!((failure, status), (None, Some(ChildStatus::Exited(0))));
        assert_eq!(output, b"ran");
        // A longer name fails before any path, even where the last path's error would be
        // `ENOENT`.
        assert_eq!(
            search_error(searched_for(&"n".repeat(256), &[&found, &missing])),
            Errno::ENAMETOOLONG as i32
        );
    }

    /// Returns the peak resident memory of the calling process, in KiB: its `VmHWM`.
    fn peak_resident_kib() -> u64 {
        std::fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("VmHWM:"))
            .and_then(|value| value.trim().strip_suffix("kB"))
            .and_then(|value| value.trim().parse().ok())
            .expect("the status holds the peak resident memory")
    }

    #[test]
    fn a_search_s_memory_is_bounded_whatever_its_name_and_path() {
        if crate::guest::managed_agent::isolate_process_test(
            "NVX_SPAWN_BOUNDED_SEARCH_TEST_CHILD",
            "guest::spawn::tests::a_search_s_memory_is_bounded_whatever_its_name_and_path",
        ) {
            return;
        }
        // The peak resident memory of the isolated process tells how much a launch prepared,
        // once a first launch has brought in the code that every launch runs, which it counts
        // too.
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing").display().to_string();
        assert_eq!(
            search_error(searched_for("prog", &[&missing])),
            Errno::ENOENT as i32
        );
        // A 4,096-byte name in each of the 32,764 empty directories of 32,763 colons would make
        // 32,764 paths of 4,097 bytes: 128 MiB.
        let name = CString::new("n".repeat(4096)).unwrap();
        let command = Command::new(name.clone(), vec![name])
            .search_path(CString::new(":".repeat(32_763)).unwrap());
        let before = peak_resident_kib();
        let error = search_error(command);
        let grown = peak_resident_kib() - before;
        assert_eq!(error, Errno::ENAMETOOLONG as i32);
        assert!(
            grown < 16 * 1024,
            "the launch grew the peak resident memory by {grown} KiB"
        );
    }

    #[test]
    fn launch_stage_failures_are_reported_and_run_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("file");
        std::fs::write(&file, b"").unwrap();
        let marker = directory.path().join("ran");
        let touch = |command: Command| run(command).0;
        let script = format!("touch '{}'", marker.display());
        for (path, stage, error) in [
            (
                directory.path().join("missing"),
                LaunchStage::WorkingDirectory,
                Errno::ENOENT,
            ),
            (file.clone(), LaunchStage::WorkingDirectory, Errno::ENOTDIR),
        ] {
            let failure = touch(shell(&script).working_directory(c_path(&path))).unwrap();
            assert_eq!(
                (failure.stage(), failure.raw_os_error()),
                (stage, error as i32),
                "{path:?}"
            );
        }
        let failure = touch(
            shell(&script)
                .working_directory(c_path(directory.path()))
                .check_working_directory_as(1000, 1000)
                .simulate_credentials(Some(Errno::EPERM)),
        )
        .unwrap();
        assert_eq!(
            (failure.stage(), failure.raw_os_error()),
            (LaunchStage::Credentials, Errno::EPERM as i32)
        );
        let failure = touch(Command::new(
            c_path(&directory.path().join("missing")),
            vec![c"missing".to_owned()],
        ))
        .unwrap();
        assert_eq!(
            (failure.stage(), failure.raw_os_error()),
            (LaunchStage::Exec, Errno::ENOENT as i32)
        );
        // A cgroup whose membership file is full refuses the child's process ID.
        let cgroup = directory.path().join("cgroup");
        std::fs::create_dir(&cgroup).unwrap();
        std::os::unix::fs::symlink("/dev/full", cgroup.join("cgroup.procs")).unwrap();
        let failure = touch(shell(&script).cgroup(Cgroup::open(&cgroup).unwrap())).unwrap();
        assert_eq!(
            (failure.stage(), failure.raw_os_error()),
            (LaunchStage::Cgroup, Errno::ENOSPC as i32)
        );
        assert!(!marker.exists());
        assert_eq!(
            failure.to_string(),
            format!(
                "the cgroup launch stage failed: {}",
                io::Error::from_raw_os_error(Errno::ENOSPC as i32)
            )
        );
    }

    #[test]
    fn a_child_joins_its_cgroup_and_enters_its_checked_directory() {
        let directory = tempfile::tempdir().unwrap();
        let cgroup = directory.path().join("cgroup");
        std::fs::create_dir(&cgroup).unwrap();
        std::fs::write(cgroup.join("cgroup.procs"), b"").unwrap();
        let (stdout, stdout_writer) = std::io::pipe().unwrap();
        let spawned = shell("pwd -P")
            .working_directory(c_path(directory.path()))
            .check_working_directory_as(1000, 1000)
            .simulate_credentials(None)
            .cgroup(Cgroup::open(&cgroup).unwrap())
            .stdout(stdout_writer)
            .spawn()
            .unwrap();
        let (id, process) = (spawned.id(), spawned.process());
        let failure = spawned.into_report().wait().unwrap();
        let output = read_all(stdout);
        assert_eq!(sys::wait(process), Some(ChildStatus::Exited(0)));
        assert_eq!(failure, None);
        let physical = std::fs::canonicalize(directory.path()).unwrap();
        assert_eq!(output, [physical.as_os_str().as_bytes(), b"\n"].concat());
        assert_eq!(
            std::fs::read(cgroup.join("cgroup.procs")).unwrap(),
            format!("{id}\n").into_bytes()
        );
        assert!(Cgroup::open(&directory.path().join("missing")).is_err());
    }

    #[test]
    fn invalid_commands_fail_to_spawn() {
        for command in [
            Command::new(c"/bin/true".to_owned(), Vec::new()),
            shell("exit 0").working_directory(c"relative".to_owned()),
            shell("exit 0").check_working_directory_as(1000, 1000),
            shell("exit 0").export_physical_working_directory(),
        ] {
            let error = command.spawn().unwrap_err();
            assert_eq!(error.raw_os_error(), Some(Errno::EINVAL as i32));
        }
    }

    #[test]
    fn short_and_unknown_reports_are_protocol_errors() {
        for (bytes, expected) in [
            (Vec::new(), Ok(None)),
            (
                [18u32.to_ne_bytes(), 2i32.to_ne_bytes()].concat(),
                Ok(Some(LaunchFailure {
                    stage: LaunchStage::WorkingDirectory,
                    error: 2,
                })),
            ),
            (vec![1, 0, 0], Err(Errno::EPROTO as i32)),
            (
                [22u32.to_ne_bytes(), 2i32.to_ne_bytes()].concat(),
                Err(Errno::EPROTO as i32),
            ),
            (
                [0u32.to_ne_bytes(), 2i32.to_ne_bytes()].concat(),
                Err(Errno::EPROTO as i32),
            ),
        ] {
            let (reader, writer) = sys::create_pipe().unwrap();
            writer.write_all(&bytes).unwrap();
            drop(writer);
            let report = LaunchReport { reader }.wait();
            assert_eq!(
                report.map_err(|error| error.raw_os_error().unwrap()),
                expected,
                "{bytes:?}"
            );
        }
        for stage in LaunchStage::ALL {
            assert_eq!(LaunchStage::from_code(stage.code()), Some(stage));
        }
    }

    #[test]
    fn launch_stages_keep_their_numbers() {
        // Callers tell a failed stage by its number, so no stage changes its number: a new stage
        // takes 22, then 23, and so on, wherever it comes in the order.
        let numbers = [
            (LaunchStage::ProcessGroup, 1),
            (LaunchStage::Session, 2),
            (LaunchStage::Barrier, 3),
            (LaunchStage::Descriptors, 4),
            (LaunchStage::Cgroup, 5),
            (LaunchStage::Propagation, 6),
            (LaunchStage::Hostname, 7),
            (LaunchStage::MoveRoot, 8),
            (LaunchStage::Proc, 9),
            (LaunchStage::Sys, 10),
            (LaunchStage::Devices, 11),
            (LaunchStage::Root, 12),
            (LaunchStage::Bounding, 13),
            (LaunchStage::Identity, 14),
            (LaunchStage::Capabilities, 15),
            (LaunchStage::NoNewPrivileges, 16),
            (LaunchStage::Credentials, 17),
            (LaunchStage::WorkingDirectory, 18),
            (LaunchStage::PhysicalWorkingDirectory, 19),
            (LaunchStage::Signals, 20),
            (LaunchStage::Exec, 21),
        ];
        assert_eq!(numbers.map(|(stage, _)| stage), LaunchStage::ALL);
        for (stage, number) in numbers {
            assert_eq!(stage.code(), number, "{stage:?}");
            assert_eq!(LaunchStage::from_code(number), Some(stage), "{number}");
        }
        for number in [0, 22, u32::MAX] {
            assert_eq!(LaunchStage::from_code(number), None, "{number}");
        }
    }

    #[test]
    fn launch_stages_are_in_their_order() {
        // The entry's view comes after the cgroup, which a child joins before it drops its
        // privileges, and its identity before the working directory, which it enters as that
        // identity. The signals take their defaults last, just before the program.
        let position = |stage: LaunchStage| {
            LaunchStage::ALL
                .iter()
                .position(|other| *other == stage)
                .unwrap()
        };
        assert!(position(LaunchStage::Cgroup) < position(LaunchStage::Identity));
        assert!(position(LaunchStage::MoveRoot) < position(LaunchStage::Proc));
        assert!(position(LaunchStage::NoNewPrivileges) < position(LaunchStage::WorkingDirectory));
        assert!(position(LaunchStage::Barrier) < position(LaunchStage::Descriptors));
        assert!(position(LaunchStage::PhysicalWorkingDirectory) < position(LaunchStage::Signals));
        assert!(position(LaunchStage::Signals) < position(LaunchStage::Exec));
    }

    /// Returns whether the launch report of `spawned` has arrived, without waiting.
    fn report_arrived(spawned: &Spawned) -> bool {
        sys::readable_or_hung_up(&[spawned.report.descriptor()], 0).unwrap()[0]
    }

    #[test]
    fn a_child_waits_at_its_barrier_until_released() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("ran");
        let (stdout, stdout_writer) = std::io::pipe().unwrap();
        let mut spawned = shell(&format!("touch '{}'; printf released", marker.display()))
            .barrier()
            .stdout(stdout_writer)
            .spawn()
            .unwrap();
        let process = spawned.process();
        let barrier = spawned.take_barrier().unwrap();
        assert!(spawned.take_barrier().is_none());
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!report_arrived(&spawned));
        assert!(!marker.exists());
        barrier.release().unwrap();
        assert_eq!(spawned.into_report().wait().unwrap(), None);
        assert_eq!(read_all(stdout), b"released");
        assert_eq!(sys::wait(process), Some(ChildStatus::Exited(0)));
        assert!(marker.exists());
    }

    #[test]
    fn a_dropped_barrier_cancels_the_launch() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("ran");
        let script = format!("touch '{}'", marker.display());
        // Dropping the barrier, or the spawned child that still holds it, cancels the launch.
        let mut spawned = shell(&script).barrier().spawn().unwrap();
        drop(spawned.take_barrier());
        let cancelled = spawned.process();
        let first = spawned.into_report().wait().unwrap();
        let spawned = shell(&script).barrier().spawn().unwrap();
        let held = spawned.process();
        let second = spawned.into_report().wait().unwrap();
        for (failure, process) in [(first, cancelled), (second, held)] {
            let failure = failure.unwrap();
            assert_eq!(
                (failure.stage(), failure.raw_os_error()),
                (LaunchStage::Barrier, Errno::ECANCELED as i32)
            );
            assert_eq!(sys::wait(process), Some(ChildStatus::Exited(125)));
        }
        assert!(!marker.exists());
    }

    #[test]
    fn a_dropped_barrier_cancels_its_launch_while_another_child_waits_at_its_own() {
        let directory = tempfile::tempdir().unwrap();
        let marker = |name: &str| directory.path().join(name);
        let command = |name: &str| shell(&format!("touch '{}'", marker(name).display())).barrier();
        let mut first = command("first").spawn().unwrap();
        let first_barrier = first.take_barrier().unwrap();
        // The second child starts while the first waits, with a copy of the first's barrier,
        // which it closes before it waits at its own; the drop cancels the first's launch
        // whichever copies remain.
        let mut second = command("second").spawn().unwrap();
        let second_barrier = second.take_barrier().unwrap();
        let (first_process, second_process) = (first.process(), second.process());
        drop(first_barrier);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !report_arrived(&first) {
            assert!(
                std::time::Instant::now() < deadline,
                "the first child still waits at its dropped barrier"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let failure = first.into_report().wait().unwrap().unwrap();
        assert_eq!(
            (failure.stage(), failure.raw_os_error()),
            (LaunchStage::Barrier, Errno::ECANCELED as i32)
        );
        assert_eq!(sys::wait(first_process), Some(ChildStatus::Exited(125)));
        assert!(!report_arrived(&second));
        second_barrier.release().unwrap();
        assert_eq!(second.into_report().wait().unwrap(), None);
        assert_eq!(sys::wait(second_process), Some(ChildStatus::Exited(0)));
        assert!(!marker("first").exists());
        assert!(marker("second").exists());
    }

    #[test]
    fn another_childs_pipes_hang_up_while_a_child_waits_at_its_barrier() {
        // The first child's launch report and output pipe exist before the waiting child
        // starts, which therefore starts with copies of their writers, and closes them before it
        // waits: the first child's pipes hang up once it ends, while the other still waits.
        let (stdout, stdout_writer) = std::io::pipe().unwrap();
        let first = shell("printf done").stdout(stdout_writer).prepare();
        let mut waiting = shell("exit 0").barrier().spawn().unwrap();
        let barrier = waiting.take_barrier().unwrap();
        let first = first.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !(sys::hung_up(first.report.borrowed()).unwrap()
            && sys::hung_up(std::os::fd::AsFd::as_fd(&stdout)).unwrap())
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the waiting child holds the first child's pipes"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!report_arrived(&waiting));
        let report = LaunchReport {
            reader: first.report,
        };
        assert_eq!(report.wait().unwrap(), None);
        assert_eq!(read_all(stdout), b"done");
        assert_eq!(sys::wait(first.process), Some(ChildStatus::Exited(0)));
        let process = waiting.process();
        barrier.release().unwrap();
        assert_eq!(waiting.into_report().wait().unwrap(), None);
        assert_eq!(sys::wait(process), Some(ChildStatus::Exited(0)));
    }

    #[test]
    fn releasing_the_barrier_of_an_ended_child_fails() {
        // Another test's child holds a copy of every descriptor of this process from its fork to
        // its `exec`, which would read the release, so the test runs in a process of its own.
        if crate::guest::managed_agent::isolate_process_test(
            "NVX_SPAWN_ENDED_BARRIER_TEST_CHILD",
            "guest::spawn::tests::releasing_the_barrier_of_an_ended_child_fails",
        ) {
            return;
        }
        let mut spawned = shell("exit 0").barrier().spawn().unwrap();
        let process = spawned.process();
        let barrier = spawned.take_barrier().unwrap();
        sys::kill_process(process).unwrap();
        assert_eq!(
            sys::wait(process),
            Some(ChildStatus::Signaled(sys::Signal::SIGKILL as i32))
        );
        let error = barrier.release().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn a_new_session_has_no_controlling_terminal() {
        for command in [
            shell(STAT_SCRIPT).new_session(),
            // A session replaces a new process group, which the parent then leaves alone.
            shell(STAT_SCRIPT).new_process_group().new_session(),
        ] {
            let (failure, status, output, _) = run(command);
            assert_eq!((failure, status), (None, Some(ChildStatus::Exited(0))));
            let output = String::from_utf8(output).unwrap();
            let ids: Vec<&str> = output.split(' ').collect();
            assert_eq!(ids[1], ids[0], "{output}");
            assert_eq!(ids[2], ids[0], "{output}");
            assert_eq!(ids[3], "0", "{output}");
        }
    }

    /// Returns a command that prints its program's status file.
    fn print_status() -> Command {
        Command::new(
            c"/bin/cat".to_owned(),
            vec![c"cat".to_owned(), c"/proc/self/status".to_owned()],
        )
    }

    /// Returns the signals that the process whose status file is `status` ignores, and those
    /// that it blocks, from its `SigIgn` and `SigBlk` lines, in which bit `n - 1` stands for
    /// signal `n`.
    fn ignored_and_blocked(status: &[u8]) -> (u64, u64) {
        let status = std::str::from_utf8(status).unwrap();
        let set = |name: &str| {
            let digits = status
                .lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
                .unwrap_or_else(|| panic!("{name} is missing from {status}"));
            u64::from_str_radix(digits.trim(), 16).unwrap()
        };
        (set("SigIgn"), set("SigBlk"))
    }

    #[test]
    fn a_program_keeps_its_caller_s_ignored_and_blocked_signals_unless_its_command_resets_them() {
        // Ignoring a signal changes the whole process, so the test runs in a process of its own.
        if crate::guest::managed_agent::isolate_process_test(
            "NVX_SPAWN_SIGNALS_TEST_CHILD",
            "guest::spawn::tests::a_program_keeps_its_caller_s_ignored_and_blocked_signals_unless_its_command_resets_them",
        ) {
            return;
        }
        // The caller ignores `SIGHUP` and `SIGPIPE`, and its thread blocks `SIGCHLD`, as one
        // that collects its children through a signal descriptor does.
        let bit = |signal: sys::Signal| 1u64 << (signal as i32 - 1);
        sys::ignore_signals(&[sys::Signal::SIGHUP, sys::Signal::SIGPIPE]);
        nix::sys::signal::SigSet::from(sys::Signal::SIGCHLD)
            .thread_block()
            .unwrap();
        let caller = ignored_and_blocked(&std::fs::read("/proc/thread-self/status").unwrap());
        let ignored = bit(sys::Signal::SIGHUP) | bit(sys::Signal::SIGPIPE);
        assert_eq!(caller.0 & ignored, ignored);
        assert_ne!(caller.1 & bit(sys::Signal::SIGCHLD), 0);

        let (failure, status, output, _) = run(print_status());
        assert_eq!((failure, status), (None, Some(ChildStatus::Exited(0))));
        assert_eq!(ignored_and_blocked(&output), caller);
        let (failure, status, output, _) = run(print_status().reset_signals());
        assert_eq!((failure, status), (None, Some(ChildStatus::Exited(0))));
        assert_eq!(ignored_and_blocked(&output), (0, 0));
    }

    #[test]
    fn a_child_whose_report_has_no_reader_still_exits_with_the_launch_failure_status() {
        // Another test's child holds a copy of every descriptor of this process from its fork to
        // its `exec`, the report's reader too, so the test runs in a process of its own.
        if crate::guest::managed_agent::isolate_process_test(
            "NVX_SPAWN_UNREAD_REPORT_TEST_CHILD",
            "guest::spawn::tests::a_child_whose_report_has_no_reader_still_exits_with_the_launch_failure_status",
        ) {
            return;
        }
        // The child writes its report with `SIGPIPE` blocked, since the default disposition that
        // the reset gives it would end the child at a write that no reader takes.
        let mut spawned = Command::new(c"/nonexistent".to_owned(), vec![c"nonexistent".to_owned()])
            .reset_signals()
            .barrier()
            .spawn()
            .unwrap();
        let process = spawned.process();
        let barrier = spawned.take_barrier().unwrap();
        drop(spawned.into_report());
        barrier.release().unwrap();
        assert_eq!(sys::wait(process), Some(ChildStatus::Exited(125)));
    }

    /// A script that prints its process ID, process group ID, session ID, and controlling
    /// terminal from its status file.
    const STAT_SCRIPT: &str = "read -r pid comm state ppid pgrp session tty rest </proc/self/stat; \
         printf '%s %s %s %s' \"$pid\" \"$pgrp\" \"$session\" \"$tty\"";
}
