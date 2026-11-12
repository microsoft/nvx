use std::io::Read;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use super::*;
use crate::guest::sys::{self, ChildStatus};

/// Capability to bypass file read, write, and execute permission checks.
#[cfg(feature = "cli")]
pub(crate) const CAP_DAC_OVERRIDE: u32 = 1;

/// Capability to bypass file read permission and directory read and search permission checks.
#[cfg(feature = "cli")]
pub(crate) const CAP_DAC_READ_SEARCH: u32 = 2;

/// Capability to change group IDs and supplementary groups.
#[cfg(feature = "cli")]
pub(crate) const CAP_SETGID: u32 = 6;

/// Capability to change user IDs.
#[cfg(feature = "cli")]
pub(crate) const CAP_SETUID: u32 = 7;

/// Capability to mount, and to enter new namespaces.
pub(crate) const CAP_SYS_ADMIN: u32 = 21;

/// Capability to lower nice values.
#[cfg(target_arch = "x86_64")]
pub(crate) const CAP_SYS_NICE: u32 = 23;

/// Capability to override resource limits, and to lower an out-of-memory score adjustment.
#[cfg(target_arch = "x86_64")]
pub(crate) const CAP_SYS_RESOURCE: u32 = 24;

/// Returns whether the calling thread's effective set holds `capability`, because tests whose
/// outcome depends on a privilege cannot infer it from the user ID: a root in a container may
/// lack some capabilities.
pub(crate) fn holds_capability(capability: u32) -> std::io::Result<bool> {
    let status = std::fs::read_to_string("/proc/thread-self/status")?;
    let effective = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
        .ok_or_else(|| std::io::Error::other("the status has no effective capability set"))?;
    Ok(effective & (1 << capability) != 0)
}

impl ChildSpec {
    /// Replaces the child's credential system calls, which then succeed, or fail with `failure`.
    pub(crate) fn simulate_credentials(&mut self, failure: Option<Errno>) {
        self.simulated_credentials = Some(failure);
    }
}

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
    spec.physical_pwd = true;
    assert_eq!(PreparedChild::new(spec).err(), Some(Errno::EINVAL));
}

#[test]
fn stream_sources_below_the_standard_streams_are_raised_and_kept_from_the_program() {
    if crate::guest::sys::tests::isolate_process_test(
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
    drop(streams);

    assert_eq!(outcome, (None, Some(ChildStatus::Exited(0))));
    assert_eq!(std::fs::read(output.path()).unwrap(), b"placed");
}

#[test]
fn the_report_writer_is_raised_above_the_standard_streams() {
    if crate::guest::sys::tests::isolate_process_test(
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
    if crate::guest::sys::tests::isolate_process_test(
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
    if crate::guest::sys::tests::isolate_process_test(
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
    spec.streams[2] = Some(descriptor());
    let prepared = PreparedChild::new(spec).unwrap();
    for descriptor in [
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
