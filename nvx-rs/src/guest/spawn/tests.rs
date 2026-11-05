use std::ffi::CStr;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::guest::sys::ChildStatus;

/// Returns a command that runs `/bin/sh -c script`, with `$0` set to `sh`, for the tests of the
/// libraries that start programs.
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

impl Command {
    /// Prepares the child without starting it, for focused launch tests.
    pub(crate) fn prepare(self) -> PreparedChild {
        PreparedChild::new(self.spec).expect("the command prepares")
    }

    /// Starts the child as [`spawn`](Self::spawn) does, and returns with it a copy of its launch
    /// report's writer: the report arrives only once the copy closes too, as it does when a
    /// process that another thread forked while the child was spawning holds the writer, for
    /// focused tests.
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
    pub(crate) fn simulate_credentials(mut self, failure: Option<Errno>) -> Self {
        self.spec.simulate_credentials(failure);
        self
    }
}

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
    std::fs::set_permissions(path("empty/prog"), std::fs::Permissions::from_mode(0o755)).unwrap();
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
    if crate::guest::sys::tests::isolate_process_test(
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
    if crate::guest::sys::tests::isolate_process_test(
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
    if crate::guest::sys::tests::isolate_process_test(
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
    if crate::guest::sys::tests::isolate_process_test(
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
