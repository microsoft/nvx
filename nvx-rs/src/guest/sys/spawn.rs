//! The fork/exec boundary of [`crate::guest::spawn`].
//!
//! [`PreparedChild::new`] checks a [`ChildSpec`] and allocates or opens everything that the child
//! uses after `fork`, and [`PreparedChild::spawn`] forks it, or, for an execution's
//! [entry](super::EntryChild), starts it with `clone3` in new namespaces. The child branch
//! performs no allocation, locking, formatting, or Rust cleanup: it makes direct system calls
//! over that storage, in the order of [`LaunchStage`], and ends in `execve`, `execv`, or `_exit`.
//! It makes them through `nix` where its wrappers neither allocate nor consume an owner, and
//! otherwise through libc: its `syscall` fills the preallocated `PWD` entry, because its `getcwd`
//! may walk the file system, closes the descriptors that the child does not use with
//! `close_range`, resets the signals with `rt_sigaction` and `rt_sigprocmask`, since libc's
//! `sigaction` refuses the signals that libc reserves for itself and musl's takes a lock for
//! `SIGABRT`, and makes the entry's calls that the [entry](super::entry) module lists; and its
//! `execve`, `execv`, and `_exit` end the child. A stage that fails writes its number and `errno`
//! to the launch report, a close-on-exec pipe, with `SIGPIPE` blocked, and ends the child with
//! status 125, so that the report's reader reaches end of file without a report once the program
//! runs, and a report that no reader takes still ends the child with that status.
//!
//! Every descriptor that the child uses after it places its standard streams has a number above
//! theirs, so that placing them replaces none of it: [`PreparedChild::new`] moves a stream source,
//! the root, the cgroup's membership file, the barrier's ends, or the report's writer that has a
//! lower number. It also makes them close-on-exec, so that the program receives
//! them only as its standard streams. A child that waits at a barrier, or enters an execution,
//! first closes every other descriptor, so that it holds no copy of another child's while it
//! waits; an entry's child also closes, before it enters its working directory, every descriptor
//! that its stages used or opened but those that its program inherits.

use std::ffi::{CStr, CString};
use std::os::fd::RawFd;

use nix::fcntl::{FcntlArg, FdFlag};
use nix::libc::{c_char, c_int, c_uint, c_ulong};
use nix::unistd::{ForkResult, Gid, Pid, Uid};

use super::{EntryChild, Errno, FileDescriptor, PATH_MAX, ProcessId};
use crate::guest::spawn::LaunchStage;

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

/// Everything that a caller asks of one child, as [`crate::guest::spawn::Command`] collects it.
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
    /// Directory that becomes the child's root.
    root: Option<FileDescriptor>,
    /// Absolute working directory, resolved in the root, or `None` to keep the current one.
    directory: Option<CString>,
    /// Effective user and group IDs with which the child enters `directory`.
    credentials: Option<(u32, u32)>,
    /// Sources of standard input, output, and error; `None` keeps the caller's.
    streams: [Option<FileDescriptor>; 3],
    /// Descriptors that stay open across `exec`, at their numbers.
    inherited: Vec<FileDescriptor>,
    /// Membership file of the cgroup that the child joins.
    cgroup: Option<FileDescriptor>,
    /// Whether the child leads a new process group.
    process_group: bool,
    /// Whether the child leads a new session.
    session: bool,
    /// Whether the child waits at a barrier until its caller releases it.
    barrier: bool,
    /// Whether the child gives every signal its default disposition, and blocks none, before its
    /// program runs.
    reset_signals: bool,
    /// The execution that the child enters, if any.
    entry: Option<EntryChild>,
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
            root: None,
            directory: None,
            credentials: None,
            streams: [None, None, None],
            inherited: Vec::new(),
            cgroup: None,
            process_group: false,
            session: false,
            barrier: false,
            reset_signals: false,
            entry: None,
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

    /// Makes the directory that `root` opens the child's root.
    pub(crate) fn set_root(&mut self, root: FileDescriptor) {
        self.root = Some(root);
    }

    /// Makes the absolute path `directory`, resolved in the root, the child's working directory.
    pub(crate) fn set_directory(&mut self, directory: CString) {
        self.directory = Some(directory);
    }

    /// Makes the child enter its working directory with the effective `uid` and `gid`.
    pub(crate) fn set_credentials(&mut self, uid: u32, gid: u32) {
        self.credentials = Some((uid, gid));
    }

    /// Makes `descriptor` the child's standard stream numbered `stream`: 0, 1, or 2.
    pub(crate) fn set_stream(&mut self, stream: usize, descriptor: FileDescriptor) {
        self.streams[stream] = Some(descriptor);
    }

    /// Keeps `descriptor` open in the child at its number.
    pub(crate) fn inherit(&mut self, descriptor: FileDescriptor) {
        self.inherited.push(descriptor);
    }

    /// Makes the child join the cgroup whose membership file `cgroup` opens.
    pub(crate) fn set_cgroup(&mut self, cgroup: FileDescriptor) {
        self.cgroup = Some(cgroup);
    }

    /// Makes the child the leader of a new process group.
    pub(crate) fn new_process_group(&mut self) {
        self.process_group = true;
    }

    /// Makes the child the leader of a new session, without a controlling terminal.
    pub(crate) fn new_session(&mut self) {
        self.session = true;
    }

    /// Makes the child wait at a barrier until its caller releases it.
    pub(crate) fn set_barrier(&mut self) {
        self.barrier = true;
    }

    /// Makes the child give every signal its default disposition, and block none, before its
    /// program runs.
    pub(crate) fn reset_signals(&mut self) {
        self.reset_signals = true;
    }

    /// Makes the child enter the execution that `entry` prepares.
    pub(crate) fn set_entry(&mut self, entry: EntryChild) {
        self.entry = Some(entry);
    }

    /// Replaces the child's credential system calls, which then succeed, or fail with `failure`.
    #[cfg(test)]
    pub(crate) fn simulate_credentials(&mut self, failure: Option<Errno>) {
        self.simulated_credentials = Some(failure);
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

/// Credentials of the working-directory check, and the supplementary groups that the child gives
/// back after it.
struct Credentials {
    /// Effective user ID of the check.
    uid: Uid,
    /// Effective group ID of the check.
    gid: Gid,
    /// The parent's supplementary groups, which the child shares until the check.
    groups: Vec<Gid>,
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
    /// Directory that becomes the child's root.
    root: Option<FileDescriptor>,
    /// Absolute working directory, resolved in the root.
    directory: Option<CString>,
    /// Credentials of the working-directory check.
    credentials: Option<Credentials>,
    /// Sources of standard input, output, and error.
    streams: [Option<FileDescriptor>; 3],
    /// Descriptors that stay open across `exec`, at their numbers.
    inherited: Vec<FileDescriptor>,
    /// Membership file of the cgroup that the child joins.
    cgroup: Option<FileDescriptor>,
    /// Whether the child leads a new process group.
    process_group: bool,
    /// Whether the child leads a new session, which an entry implies.
    session: bool,
    /// The reader and the writer of the barrier at which the child waits, if any: the ends of a
    /// connected socket pair, of which the child keeps the reader, and its caller the writer,
    /// whose shutdown the child reads as end of file even while other children hold copies of
    /// it.
    barrier: Option<(FileDescriptor, FileDescriptor)>,
    /// Whether the child resets its signals before its program runs, which an entry implies.
    reset_signals: bool,
    /// The execution that the child enters, if any.
    entry: Option<EntryChild>,
    /// Reader of the launch report, which the caller receives.
    report_reader: FileDescriptor,
    /// Writer of the launch report, which only the child keeps.
    report_writer: FileDescriptor,
    /// The numbers of every descriptor that the child uses before its program runs, or that its
    /// program inherits, in ascending order: the stream sources, the root, the cgroup's
    /// membership file, the barrier's reader, the report's writer, and the inherited
    /// descriptors. A child that waits at a barrier, or enters an execution, closes every other.
    used: Box<[RawFd]>,
    /// The numbers of the descriptors that an entry's program inherits, and of the report's
    /// writer, in ascending order: all that its child keeps once its stages have run.
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
    /// The writer of the barrier at which the child waits, if it waits at one.
    pub(crate) barrier: Option<FileDescriptor>,
}

impl PreparedChild {
    /// Checks `spec` and prepares every input of its child.
    ///
    /// The working directory must be absolute, credentials need a working directory, the physical
    /// `PWD` an exact environment, and an inherited descriptor a number above the standard
    /// streams'; a command with no argument zero is invalid too. An entry needs a cgroup or a
    /// barrier, at which its caller places it, and takes neither a root directory nor
    /// credentials, as its own root and identity replace them. Each fails with `EINVAL`. A program
    /// that the spec names without a `/`, in a spec with a search path, gets here the one buffer
    /// in which its child builds, in turn, each path at which it looks for it.
    pub(crate) fn new(spec: ChildSpec) -> Result<Self, Errno> {
        if spec.arguments.is_empty()
            || spec
                .directory
                .as_ref()
                .is_some_and(|directory| directory.as_bytes().first() != Some(&b'/'))
            || (spec.credentials.is_some() && spec.directory.is_none())
            || (spec.physical_pwd && spec.environment.is_none())
            || spec
                .inherited
                .iter()
                .any(|descriptor| descriptor.raw() < FIRST_NONSTANDARD)
            || (spec.entry.is_some()
                && ((spec.cgroup.is_none() && !spec.barrier)
                    || spec.root.is_some()
                    || spec.credentials.is_some()))
        {
            return Err(Errno::EINVAL);
        }
        let [stdin, stdout, stderr] = spec.streams;
        let streams = [
            stdin.map(above_standard_streams).transpose()?,
            stdout.map(above_standard_streams).transpose()?,
            stderr.map(above_standard_streams).transpose()?,
        ];
        let root = spec.root.map(above_standard_streams).transpose()?;
        let cgroup = spec.cgroup.map(above_standard_streams).transpose()?;
        let barrier = if spec.barrier {
            let (reader, writer) = super::create_socket_pair()?;
            Some((
                above_standard_streams(reader)?,
                above_standard_streams(writer)?,
            ))
        } else {
            None
        };
        let entry = spec.entry;
        let (report_reader, report_writer) = super::create_pipe()?;
        let report_writer = above_standard_streams(report_writer)?;
        let credentials = match spec.credentials {
            Some((uid, gid)) => Some(Credentials {
                uid: Uid::from_raw(uid),
                gid: Gid::from_raw(gid),
                groups: nix::unistd::getgroups()?,
            }),
            None => None,
        };
        let mut environment = spec.environment;
        let physical_pwd = match (&mut environment, spec.physical_pwd) {
            (Some(environment), true) => Some(PhysicalPwd::new(environment)),
            _ => None,
        };
        let used = ascending_numbers(
            streams
                .iter()
                .flatten()
                .chain(&root)
                .chain(&cgroup)
                .chain(barrier.as_ref().map(|(reader, _)| reader))
                .chain([&report_writer])
                .chain(&spec.inherited),
        );
        let kept = ascending_numbers(spec.inherited.iter().chain([&report_writer]));
        let search = spec
            .search_path
            .and_then(|path| Search::new(&spec.program, path));
        Ok(Self {
            program: spec.program,
            arguments: spec.arguments,
            search,
            environment,
            physical_pwd,
            root,
            directory: spec.directory,
            credentials,
            streams,
            inherited: spec.inherited,
            cgroup,
            process_group: spec.process_group,
            session: spec.session || entry.is_some(),
            barrier,
            reset_signals: spec.reset_signals || entry.is_some(),
            entry,
            report_reader,
            report_writer,
            used,
            kept,
            #[cfg(test)]
            simulated_credentials: spec.simulated_credentials,
        })
    }

    /// Forks the child, which runs its launch stages and then its program, and returns its
    /// process ID, the reader of its launch report, and the writer of its barrier, if any. The
    /// child of an entry starts with `clone3`, in new mount, PID, and UTS namespaces, of which it
    /// is PID 1.
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
        let child = match self.entry.as_ref().map(EntryChild::clone_flags) {
            // SAFETY: as above; the entry's child makes only the system calls that its module
            // permits.
            Some(flags) => unsafe { super::entry::clone_into_namespaces(flags) }?,
            None => match unsafe { nix::unistd::fork() }? {
                ForkResult::Child => None,
                ForkResult::Parent { child } => Some(child),
            },
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
                if self.process_group && !self.session {
                    // The child makes the same call; whichever runs first creates the group, so
                    // that the caller may signal it at once. The child's `exec` or exit makes a
                    // failure here harmless. A child that starts a session leads the group that
                    // it creates, which the caller's call would forbid.
                    let _ = nix::unistd::setpgid(child, child);
                }
                Ok(Started {
                    process: ProcessId::from_pid(child),
                    report: self.report_reader,
                    barrier: self.barrier.take().map(|(_, writer)| writer),
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
            && !self.session
            && let Err(error) = nix::unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0))
        {
            // SAFETY: the caller establishes that this is the post-fork child.
            unsafe { fail(report, LaunchStage::ProcessGroup, error) }
        }
        if self.session
            && let Err(error) = nix::unistd::setsid()
        {
            // SAFETY: as above.
            unsafe { fail(report, LaunchStage::Session, error) }
        }
        if let Some((reader, _)) = &self.barrier {
            // Before it waits, the child closes every descriptor that it does not use, its copy
            // of the barrier's writer among them. It then holds no copy of another child's pipes
            // or launch report, which would stay open while it waits, and reads end of file once
            // its caller shuts the writer down, or once no other copy of it remains.
            if let Err(error) =
                close_other_descriptors(&self.used).and_then(|()| wait_for_release(reader))
            {
                // SAFETY: as above.
                unsafe { fail(report, LaunchStage::Barrier, error) }
            }
        }
        if let Err(error) = self.place_descriptors() {
            // SAFETY: as above.
            unsafe { fail(report, LaunchStage::Descriptors, error) }
        }
        if let Some(cgroup) = &self.cgroup
            && let Err(error) = write_decimal(cgroup, nix::unistd::getpid().as_raw())
        {
            // SAFETY: as above.
            unsafe { fail(report, LaunchStage::Cgroup, error) }
        }
        if let Some(entry) = &self.entry
            && let Err((stage, error)) = entry.enter_view()
        {
            // SAFETY: as above.
            unsafe { fail(report, stage, error) }
        }
        if let Some(root) = &self.root
            && let Err(error) = enter_root(root)
        {
            // SAFETY: as above.
            unsafe { fail(report, LaunchStage::Root, error) }
        }
        if let Some(entry) = &self.entry
            && let Err((stage, error)) = entry.take_identity()
        {
            // SAFETY: as above.
            unsafe { fail(report, stage, error) }
        }
        if self.entry.is_some() {
            // An entry's child closes every descriptor that its stages used or opened, but its
            // program's, and resolves its working directory, `/` unless the command names
            // another, through no magic link: no `/proc` link to a descriptor, a working
            // directory, or a root then reaches outside the root, or a directory that a mount of
            // the view covers.
            let directory = self.directory.as_deref().unwrap_or(c"/");
            if let Err(error) = close_other_descriptors(&self.kept)
                .and_then(|()| super::entry::enter_directory(directory))
            {
                // SAFETY: as above.
                unsafe { fail(report, LaunchStage::WorkingDirectory, error) }
            }
        } else if let Some(directory) = &self.directory {
            match &self.credentials {
                Some(credentials) => {
                    let saved = (nix::unistd::geteuid(), nix::unistd::getegid());
                    if let Err(error) = self.take_credentials(credentials) {
                        // SAFETY: as above.
                        unsafe { fail(report, LaunchStage::Credentials, error) }
                    }
                    if let Err(error) = nix::unistd::chdir(directory.as_c_str()) {
                        // SAFETY: as above.
                        unsafe { fail(report, LaunchStage::WorkingDirectory, error) }
                    }
                    if let Err(error) = self.restore_credentials(credentials, saved) {
                        // SAFETY: as above.
                        unsafe { fail(report, LaunchStage::Credentials, error) }
                    }
                }
                None => {
                    if let Err(error) = nix::unistd::chdir(directory.as_c_str()) {
                        // SAFETY: as above.
                        unsafe { fail(report, LaunchStage::WorkingDirectory, error) }
                    }
                }
            }
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

    /// Places the standard streams and clears close-on-exec on the inherited descriptors.
    ///
    /// Every source has a number above the standard streams', so `dup2` replaces each stream
    /// without closing a source, and clears close-on-exec on its copy. The child of an entry
    /// first closes every descriptor that it does not use, those that its caller opened without
    /// close-on-exec too, so that its program holds none of the caller's.
    fn place_descriptors(&self) -> Result<(), Errno> {
        if self.entry.is_some() {
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
        for descriptor in &self.inherited {
            nix::fcntl::fcntl(descriptor.borrowed(), FcntlArg::F_SETFD(FdFlag::empty()))?;
        }
        Ok(())
    }

    /// Takes the credentials of the working-directory check: no supplementary groups, then the
    /// effective group, then the effective user.
    fn take_credentials(&self, credentials: &Credentials) -> Result<(), Errno> {
        #[cfg(test)]
        if let Some(simulated) = self.simulated_credentials {
            return simulated.map_or(Ok(()), Err);
        }
        nix::unistd::setgroups(&[])?;
        nix::unistd::setegid(credentials.gid)?;
        nix::unistd::seteuid(credentials.uid)
    }

    /// Gives back the effective user, then the effective group and the supplementary groups,
    /// that the child had before the check.
    fn restore_credentials(
        &self,
        credentials: &Credentials,
        (uid, gid): (Uid, Gid),
    ) -> Result<(), Errno> {
        #[cfg(test)]
        if self.simulated_credentials.is_some() {
            return Ok(());
        }
        nix::unistd::seteuid(uid)?;
        nix::unistd::setegid(gid)?;
        nix::unistd::setgroups(&credentials.groups)
    }

    /// Returns the prepared executable path for focused launch tests.
    #[cfg(test)]
    pub(crate) fn program(&self) -> &CString {
        &self.program
    }

    /// Returns the prepared arguments for focused launch tests.
    #[cfg(test)]
    pub(crate) fn arguments(&self) -> &[CString] {
        &self.arguments
    }

    /// Returns the prepared exact environment, without the physical `PWD` entry, for focused
    /// launch tests.
    #[cfg(test)]
    pub(crate) fn environment(&self) -> &[CString] {
        self.environment.as_deref().unwrap_or_default()
    }

    /// Returns the preallocated physical `PWD` entry for focused launch tests.
    #[cfg(test)]
    pub(crate) fn physical_pwd(&self) -> Option<&[u8]> {
        self.physical_pwd
            .as_ref()
            .map(|physical_pwd| physical_pwd.entry.as_ref())
    }

    /// Returns the environment position of the physical `PWD` entry for focused launch tests.
    #[cfg(test)]
    pub(crate) fn physical_pwd_index(&self) -> Option<usize> {
        self.physical_pwd
            .as_ref()
            .map(|physical_pwd| physical_pwd.index)
    }

    /// Returns whether the child joins a cgroup, for focused launch tests.
    #[cfg(test)]
    pub(crate) fn has_cgroup(&self) -> bool {
        self.cgroup.is_some()
    }

    /// Returns a close-on-exec copy of the launch report's writer, which holds the report back
    /// while it is open, as a process that another thread forks before the child does, for
    /// focused tests.
    #[cfg(test)]
    pub(crate) fn report_writer_copy(&self) -> Result<FileDescriptor, Errno> {
        let copy = nix::fcntl::fcntl(
            self.report_writer.borrowed(),
            FcntlArg::F_DUPFD_CLOEXEC(FIRST_NONSTANDARD),
        )?;
        // SAFETY: `F_DUPFD_CLOEXEC` returned a new descriptor, which nothing else owns.
        Ok(unsafe { FileDescriptor::from_raw(copy) })
    }

    /// Returns whether the child waits at a barrier, enters an execution, and leads a new
    /// session, for focused launch tests.
    #[cfg(test)]
    pub(crate) fn barrier_entry_and_session(&self) -> (bool, bool, bool) {
        (self.barrier.is_some(), self.entry.is_some(), self.session)
    }
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
fn enter_root(root: &FileDescriptor) -> Result<(), Errno> {
    nix::unistd::fchdir(root.borrowed())?;
    nix::unistd::chroot(c".")
}

/// Waits until the caller releases the barrier whose reader is `reader`, which a byte tells.
/// End of file without a release, once the caller has shut the writer down, or once no copy of
/// the writer remains, is `ECANCELED`.
fn wait_for_release(reader: &FileDescriptor) -> Result<(), Errno> {
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => return Err(Errno::ECANCELED),
            Ok(_) => return Ok(()),
            Err(Errno::EINTR) => {}
            Err(error) => return Err(error),
        }
    }
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
fn reset_signals() -> Result<(), Errno> {
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

/// Capability to bypass file read, write, and execute permission checks.
#[cfg(test)]
pub(crate) const CAP_DAC_OVERRIDE: u32 = 1;
/// Capability to bypass file read permission and directory read and search permission checks.
#[cfg(test)]
pub(crate) const CAP_DAC_READ_SEARCH: u32 = 2;
/// Capability to change group IDs and supplementary groups.
#[cfg(test)]
pub(crate) const CAP_SETGID: u32 = 6;
/// Capability to change user IDs.
#[cfg(test)]
pub(crate) const CAP_SETUID: u32 = 7;
/// Capability to change the root directory.
#[cfg(test)]
pub(crate) const CAP_SYS_CHROOT: u32 = 18;
/// Capability to mount, and to enter new namespaces, which an execution's entry needs.
#[cfg(test)]
pub(crate) const CAP_SYS_ADMIN: u32 = 21;

/// Returns whether the calling thread's effective set holds `capability`, because tests whose
/// outcome depends on a privilege cannot infer it from the user ID: a root in a container may
/// lack some capabilities.
#[cfg(test)]
pub(crate) fn holds_capability(capability: u32) -> std::io::Result<bool> {
    let status = std::fs::read_to_string("/proc/thread-self/status")?;
    let effective = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
        .ok_or_else(|| std::io::Error::other("the status has no effective capability set"))?;
    Ok(effective & (1 << capability) != 0)
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

    use super::*;
    use crate::guest::sys::{self, ChildStatus};

    /// Closes standard streams of an isolated test process, and restores them when dropped, so
    /// that the test harness can report the outcome.
    struct ClosedStreams(Vec<(RawFd, OwnedFd)>);

    impl ClosedStreams {
        /// Closes the standard streams `numbers`, keeping a copy of each.
        fn new(numbers: &[RawFd]) -> Self {
            Self(
                numbers
                    .iter()
                    .map(|&number| {
                        // SAFETY: the standard stream is open while this borrow lasts.
                        let stream = unsafe { BorrowedFd::borrow_raw(number) };
                        let copy = stream.try_clone_to_owned().unwrap();
                        // SAFETY: the isolated test process gives up the stream, which nothing
                        // else in it uses until the guard restores it.
                        drop(unsafe { OwnedFd::from_raw_fd(number) });
                        (number, copy)
                    })
                    .collect(),
            )
        }
    }

    impl Drop for ClosedStreams {
        fn drop(&mut self) {
            for (number, copy) in &self.0 {
                // SAFETY: `dup2` replaces whatever the test left at the stream's number.
                unsafe { nix::libc::dup2(copy.as_raw_fd(), *number) };
            }
        }
    }

    /// Returns one ordinary owned descriptor.
    fn descriptor() -> FileDescriptor {
        FileDescriptor::from_owned(tempfile::tempfile().unwrap().into())
    }

    /// Returns a spec that runs `/bin/sh -c script`.
    fn shell(script: &str) -> ChildSpec {
        ChildSpec::new(
            c"/bin/sh".to_owned(),
            vec![
                c"sh".to_owned(),
                c"-c".to_owned(),
                CString::new(script).unwrap(),
            ],
        )
    }

    /// Spawns `spec`, reads its launch report to the end, reaps it, and returns both.
    fn run(spec: ChildSpec) -> (Option<(u32, i32)>, Option<ChildStatus>) {
        let Started {
            process,
            report: mut reader,
            ..
        } = PreparedChild::new(spec).unwrap().spawn().unwrap();
        let mut report = Vec::new();
        reader.read_to_end(&mut report).unwrap();
        let report = (report.len() == REPORT_LENGTH).then(|| {
            (
                u32::from_ne_bytes(report[..4].try_into().unwrap()),
                i32::from_ne_bytes(report[4..].try_into().unwrap()),
            )
        });
        (report, sys::wait(process))
    }

    #[test]
    fn the_physical_pwd_entry_takes_the_first_pwd_entry_s_place() {
        let mut environment = vec![
            c"FIRST=1".to_owned(),
            c"PWD=/old".to_owned(),
            c"PWD=/second".to_owned(),
        ];
        let physical_pwd = PhysicalPwd::new(&mut environment);
        assert_eq!(physical_pwd.index, 1);
        assert_eq!(
            environment,
            [c"FIRST=1".to_owned(), c"PWD=/second".to_owned()]
        );
        assert_eq!(&physical_pwd.entry[..PWD_PREFIX.len()], PWD_PREFIX);
        assert_eq!(
            physical_pwd.entry.len(),
            PWD_PREFIX.len() + PHYSICAL_PWD_CAPACITY
        );
        assert!(
            physical_pwd.entry[PWD_PREFIX.len()..]
                .iter()
                .all(|&byte| byte == 0)
        );

        let mut environment = vec![c"FIRST=1".to_owned()];
        assert_eq!(PhysicalPwd::new(&mut environment).index, 1);
    }

    #[test]
    fn invalid_specs_are_refused_before_forking() {
        let mut spec = shell("exit 0");
        spec.arguments.clear();
        assert_eq!(PreparedChild::new(spec).err(), Some(Errno::EINVAL));

        let mut spec = shell("exit 0");
        spec.directory = Some(c"relative".to_owned());
        assert_eq!(PreparedChild::new(spec).err(), Some(Errno::EINVAL));

        let mut spec = shell("exit 0");
        spec.credentials = Some((1000, 1000));
        assert_eq!(PreparedChild::new(spec).err(), Some(Errno::EINVAL));

        let mut spec = shell("exit 0");
        spec.physical_pwd = true;
        assert_eq!(PreparedChild::new(spec).err(), Some(Errno::EINVAL));
    }

    #[test]
    fn stream_sources_below_the_standard_streams_are_raised_and_kept_from_the_program() {
        if crate::guest::managed_agent::isolate_process_test(
            "NVX_SPAWN_LOW_STREAM_TEST_CHILD",
            "guest::sys::spawn::tests::stream_sources_below_the_standard_streams_are_raised_and_kept_from_the_program",
        ) {
            return;
        }
        let output = tempfile::NamedTempFile::new().unwrap();
        let streams = ClosedStreams::new(&[0]);
        // The file takes number 0, the lowest free one, and becomes the program's standard
        // output; the program has no standard input.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(output.path())
            .unwrap();
        assert_eq!(file.as_raw_fd(), 0);
        let mut spec = shell("printf placed; test ! -e /proc/self/fd/0");
        spec.streams[1] = Some(FileDescriptor::from_owned(file.into()));
        let outcome = run(spec);

        // An inherited descriptor keeps its number, which must not be a standard stream's.
        let file = std::fs::File::open(output.path()).unwrap();
        assert_eq!(file.as_raw_fd(), 0);
        let mut spec = shell("exit 0");
        spec.inherited
            .push(FileDescriptor::from_owned(OwnedFd::from(file)));
        let refused = PreparedChild::new(spec).err();
        drop(streams);

        assert_eq!(outcome, (None, Some(ChildStatus::Exited(0))));
        assert_eq!(std::fs::read(output.path()).unwrap(), b"placed");
        assert_eq!(refused, Some(Errno::EINVAL));
    }

    #[test]
    fn the_report_writer_is_raised_above_the_standard_streams() {
        if crate::guest::managed_agent::isolate_process_test(
            "NVX_SPAWN_LOW_REPORT_TEST_CHILD",
            "guest::sys::spawn::tests::the_report_writer_is_raised_above_the_standard_streams",
        ) {
            return;
        }
        // With standard input and output closed, the report's pipe takes their numbers, and
        // placing the child's standard output would replace a writer that kept number 1.
        let streams = ClosedStreams::new(&[0, 1]);
        let mut spec = ChildSpec::new(c"/nonexistent".to_owned(), vec![c"nonexistent".to_owned()]);
        spec.streams[1] = Some(descriptor());
        let outcome = run(spec);
        drop(streams);
        assert_eq!(
            outcome,
            (
                Some((LaunchStage::Exec.code(), Errno::ENOENT as i32)),
                Some(ChildStatus::Exited(LAUNCH_FAILURE_STATUS))
            )
        );
    }

    #[test]
    fn a_new_process_group_is_led_by_the_child() {
        for process_group in [true, false] {
            let (reader, writer) = sys::create_pipe().unwrap();
            let mut spec = shell("read -r line");
            spec.streams[0] = Some(reader);
            spec.process_group = process_group;
            let Started {
                process,
                report: _report,
                ..
            } = PreparedChild::new(spec).unwrap().spawn().unwrap();
            let child = Pid::from_raw(process.raw());
            let expected = if process_group {
                child
            } else {
                nix::unistd::getpgrp()
            };
            let group = nix::unistd::getpgid(Some(child));
            writer.write_all(b"done\n").unwrap();
            assert_eq!(sys::wait(process), Some(ChildStatus::Exited(0)));
            assert_eq!(group, Ok(expected), "{process_group}");
        }
    }

    #[test]
    fn a_child_enters_its_root_or_reports_why_not() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("inside")).unwrap();
        let mut spec = ChildSpec::new(c"/bin/sh".to_owned(), vec![c"sh".to_owned()]);
        spec.root = Some(
            sys::open(
                &sys::path_c_string(root.path()).unwrap(),
                sys::OpenFlags::O_RDONLY | sys::OpenFlags::O_DIRECTORY | sys::OpenFlags::O_CLOEXEC,
                sys::Mode::empty(),
            )
            .unwrap(),
        );
        spec.directory = Some(c"/inside".to_owned());
        let (report, status) = run(spec);
        assert_eq!(status, Some(ChildStatus::Exited(LAUNCH_FAILURE_STATUS)));
        // Without the privilege to change its root, the child stops there. With it, the child
        // finds `/inside` in the new root, which has no `/bin/sh`.
        let (stage, error) = if holds_capability(CAP_SYS_CHROOT).unwrap() {
            (LaunchStage::Exec, Errno::ENOENT)
        } else {
            (LaunchStage::Root, Errno::EPERM)
        };
        assert_eq!(report, Some((stage.code(), error as i32)));
    }

    #[test]
    fn the_check_s_credentials_keep_the_parent_s_supplementary_groups() {
        let mut spec = shell("exit 0");
        spec.directory = Some(c"/".to_owned());
        spec.credentials = Some((1000, 1001));
        let prepared = PreparedChild::new(spec).unwrap();
        let credentials = prepared.credentials.as_ref().unwrap();
        assert_eq!(credentials.groups, nix::unistd::getgroups().unwrap());
        assert_eq!(
            (credentials.uid, credentials.gid),
            (Uid::from_raw(1000), Gid::from_raw(1001))
        );
    }

    /// Returns a spec that records the physical path of the working directory that the child
    /// inherits.
    fn inherited_physical_pwd() -> ChildSpec {
        let mut spec = shell("exit 0");
        spec.environment = Some(Vec::new());
        spec.physical_pwd = true;
        spec
    }

    #[test]
    fn a_physical_path_longer_than_the_entry_fails_its_stage() {
        if crate::guest::managed_agent::isolate_process_test(
            "NVX_SPAWN_LONG_PHYSICAL_PWD_TEST_CHILD",
            "guest::sys::spawn::tests::a_physical_path_longer_than_the_entry_fails_its_stage",
        ) {
            return;
        }
        // The isolated process enters, one component at a time, a directory whose path is
        // longer than `PATH_MAX`, and the child inherits it. glibc's `getcwd` would walk the file
        // system and fail with `ERANGE`.
        let root = tempfile::tempdir().unwrap();
        let component = "d".repeat(250);
        let mut directory = OwnedFd::from(std::fs::File::open(root.path()).unwrap());
        for _ in 0..20 {
            nix::sys::stat::mkdirat(&directory, component.as_str(), sys::Mode::S_IRWXU).unwrap();
            directory = nix::fcntl::openat(
                &directory,
                component.as_str(),
                sys::OpenFlags::O_RDONLY | sys::OpenFlags::O_DIRECTORY | sys::OpenFlags::O_CLOEXEC,
                sys::Mode::empty(),
            )
            .unwrap();
        }
        nix::unistd::fchdir(&directory).unwrap();
        assert_eq!(
            run(inherited_physical_pwd()),
            (
                Some((
                    LaunchStage::PhysicalWorkingDirectory.code(),
                    Errno::ENAMETOOLONG as i32
                )),
                Some(ChildStatus::Exited(LAUNCH_FAILURE_STATUS))
            )
        );
    }

    #[test]
    fn a_working_directory_outside_the_root_has_no_physical_path() {
        if crate::guest::managed_agent::isolate_process_test(
            "NVX_SPAWN_UNREACHABLE_PWD_TEST_CHILD",
            "guest::sys::spawn::tests::a_working_directory_outside_the_root_has_no_physical_path",
        ) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let real_root = OwnedFd::from(std::fs::File::open("/").unwrap());
        // The isolated process changes its root without entering it, so the working directory
        // that the child inherits lies outside the root, where the kernel names it
        // `(unreachable)`. Without the privilege to change its root, the test stops here.
        match nix::unistd::chroot(root.path()) {
            Ok(()) => {}
            Err(Errno::EPERM) => return,
            Err(error) => panic!("cannot change the root: {error}"),
        }
        let outcome = run(inherited_physical_pwd());
        // The process leaves the root again, so that the temporary directory can be removed.
        nix::unistd::fchdir(&real_root).unwrap();
        nix::unistd::chroot(c".").unwrap();
        assert_eq!(
            outcome,
            (
                Some((
                    LaunchStage::PhysicalWorkingDirectory.code(),
                    Errno::ENOENT as i32
                )),
                Some(ChildStatus::Exited(LAUNCH_FAILURE_STATUS))
            )
        );
    }

    #[test]
    fn the_launch_descriptors_are_close_on_exec() {
        let mut spec = shell("exit 0");
        spec.cgroup = Some(descriptor());
        spec.streams[2] = Some(descriptor());
        let prepared = PreparedChild::new(spec).unwrap();
        for descriptor in [
            prepared.cgroup.as_ref().unwrap(),
            prepared.streams[2].as_ref().unwrap(),
            &prepared.report_reader,
            &prepared.report_writer,
        ] {
            assert!(descriptor.raw() >= FIRST_NONSTANDARD);
            let flags = nix::fcntl::fcntl(descriptor.borrowed(), FcntlArg::F_GETFD).unwrap();
            assert!(FdFlag::from_bits_retain(flags).contains(FdFlag::FD_CLOEXEC));
        }
    }

    /// Searches for `name` in the directories of `path` with a buffer of `size` bytes, where every
    /// path fails with `error`, and returns the paths that the search tried, in order, and its
    /// `errno`.
    fn searched(name: &CStr, path: &CStr, size: usize, error: Errno) -> (Vec<String>, Errno) {
        let mut buffer = vec![0; size];
        let mut tried = Vec::new();
        let outcome = search(name, path, &mut buffer, |candidate| {
            // The kernel reads a path up to its first NUL, which must end it.
            let bytes = candidate.to_bytes_with_nul();
            assert_eq!(
                bytes.iter().position(|&byte| byte == 0),
                Some(bytes.len() - 1)
            );
            tried.push(candidate.to_str().unwrap().to_owned());
            error
        });
        (tried, outcome)
    }

    #[test]
    fn a_search_tries_each_directory_of_its_path_in_order() {
        let tried = |name: &CStr, path: &CStr| searched(name, path, PATH_MAX, Errno::ENOENT).0;
        assert_eq!(
            tried(c"prog", c"/usr/bin:/bin/"),
            ["/usr/bin/prog", "/bin//prog"]
        );
        // An empty directory, at either end or between two colons, or an empty path, is the
        // working directory.
        assert_eq!(
            tried(c"prog", c":/bin::"),
            ["prog", "/bin/prog", "prog", "prog"]
        );
        assert_eq!(tried(c"prog", c""), ["prog"]);
        // A name that is empty or holds a slash is not searched for.
        for program in [c"", c"/bin/sh", c"bin/sh", c"./prog"] {
            assert!(
                Search::new(program, c"/bin".to_owned()).is_none(),
                "{program:?}"
            );
        }
        // Only a spec with a search path prepares a search, whose one buffer holds the longest
        // path that the kernel takes.
        let spec = ChildSpec::new(c"prog".to_owned(), vec![c"prog".to_owned()]);
        assert!(PreparedChild::new(spec).unwrap().search.is_none());
        let mut spec = ChildSpec::new(c"prog".to_owned(), vec![c"prog".to_owned()]);
        spec.set_search_path(c"/bin".to_owned());
        let prepared = PreparedChild::new(spec).unwrap();
        let search = prepared.search.as_ref().unwrap();
        assert_eq!(
            (search.path.as_c_str(), search.buffer.len()),
            (c"/bin", PATH_MAX)
        );
    }

    #[test]
    fn a_search_skips_a_path_that_its_buffer_cannot_hold() {
        // A buffer of `PATH_MAX` bytes holds a path of `PATH_MAX - 1` bytes and its NUL, and no
        // longer one.
        let directory = |length: usize| format!("/{}", "d".repeat(length - 1));
        let fits = directory(PATH_MAX - "/prog".len() - 1);
        let path = CString::new(format!("{}:{fits}", directory(fits.len() + 1))).unwrap();
        let (tried, error) = searched(c"prog", &path, PATH_MAX, Errno::ENOENT);
        assert_eq!(tried, [format!("{fits}/prog")]);
        assert_eq!(error, Errno::ENOENT);
        // A skipped path counts as one that failed with `ENAMETOOLONG`: the last path's error
        // replaces it, and a path that the child may not run takes precedence over it, as they
        // do over the kernel's. `/bin/prog` and its NUL fill 10 bytes, and `/sbin/prog` does not
        // fit.
        for (path, error, outcome) in [
            (c"/sbin:/bin", Errno::ENOENT, Errno::ENOENT),
            (c"/bin:/sbin", Errno::ENOENT, Errno::ENAMETOOLONG),
            (c"/bin:/sbin", Errno::EACCES, Errno::EACCES),
            (c"/sbin:/bin:/usr", Errno::ENOEXEC, Errno::ENOEXEC),
        ] {
            let (tried, failure) = searched(c"prog", path, 10, error);
            assert_eq!(tried, ["/bin/prog"], "{path:?}");
            assert_eq!(failure, outcome, "{path:?}");
        }
    }

    #[test]
    fn a_search_refuses_a_name_longer_than_name_max_before_any_path() {
        let name = |length: usize| CString::new("n".repeat(length)).unwrap();
        let (tried, error) = searched(&name(NAME_MAX + 1), c"/bin", PATH_MAX, Errno::ENOENT);
        assert!(tried.is_empty());
        assert_eq!(error, Errno::ENAMETOOLONG);
        let (tried, error) = searched(&name(NAME_MAX), c"/bin", PATH_MAX, Errno::ENOENT);
        assert_eq!(tried, [format!("/bin/{}", "n".repeat(NAME_MAX))]);
        assert_eq!(error, Errno::ENOENT);
    }

    #[test]
    fn the_kernel_refuses_every_path_that_a_search_s_buffer_cannot_hold() {
        // The kernel takes a path of `PATH_MAX - 1` bytes, which it looks up, and refuses a longer
        // one with `ENAMETOOLONG`, as a search does with every path that does not fit in its
        // buffer of `PATH_MAX` bytes.
        let directory = tempfile::tempdir().unwrap();
        let mut long = sys::path_c_string(&directory.path().join("missing"))
            .unwrap()
            .into_bytes();
        while long.len() < PATH_MAX {
            long.extend_from_slice(b"/d");
        }
        for (length, error) in [
            (PATH_MAX - 1, Errno::ENOENT),
            (PATH_MAX, Errno::ENAMETOOLONG),
        ] {
            let program = CString::new(&long[..length]).unwrap();
            assert_eq!(
                run(ChildSpec::new(program, vec![c"program".to_owned()])),
                (
                    Some((LaunchStage::Exec.code(), error as i32)),
                    Some(ChildStatus::Exited(LAUNCH_FAILURE_STATUS))
                ),
                "{length}"
            );
        }
    }

    #[test]
    fn an_entry_leads_a_session_and_keeps_its_barrier_from_the_program() {
        if crate::guest::managed_agent::isolate_process_test(
            "NVX_SPAWN_ENTRY_BARRIER_TEST_CHILD",
            "guest::sys::spawn::tests::an_entry_leads_a_session_and_keeps_its_barrier_from_the_program",
        ) {
            return;
        }
        // With the standard streams closed, the barrier's ends take their numbers.
        let streams = ClosedStreams::new(&[0, 1, 2]);
        let mut spec = shell("exit 0");
        spec.barrier = true;
        spec.entry = Some(EntryChild::default());
        let prepared = PreparedChild::new(spec);
        drop(streams);
        let prepared = prepared.unwrap();
        assert!(prepared.session && !prepared.process_group);
        assert!(prepared.entry.is_some());
        let (reader, writer) = prepared.barrier.as_ref().unwrap();
        for descriptor in [reader, writer] {
            assert!(descriptor.raw() >= FIRST_NONSTANDARD);
            let flags = nix::fcntl::fcntl(descriptor.borrowed(), FcntlArg::F_GETFD).unwrap();
            assert!(FdFlag::from_bits_retain(flags).contains(FdFlag::FD_CLOEXEC));
        }
    }
}
