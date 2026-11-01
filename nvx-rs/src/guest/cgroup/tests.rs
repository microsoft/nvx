use std::cell::Cell;
use std::ffi::CString;
use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::process::ExitStatusExt;

use nix::poll::{PollFd, PollFlags, PollTimeout};

use super::*;
use crate::guest::process::Child;
use crate::guest::spawn::Command;

thread_local! {
    /// What the next turn on this thread runs once it found its cgroup, before it acts on the
    /// cgroup's path: the pause in which a test races it.
    static PAUSE: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::Cell::new(None) };
}

/// Runs the pause that a test set for this thread's next turn, if any.
pub(super) fn pause() {
    if let Some(pause) = PAUSE.take() {
        pause();
    }
}

/// Returns a command that runs `/bin/sh -c script`.
fn shell(script: &str) -> Command {
    Command::new(
        c"/bin/sh".to_owned(),
        vec![
            c"sh".to_owned(),
            c"-c".to_owned(),
            CString::new(script).unwrap(),
        ],
    )
}

/// Returns the error number of `result`'s error.
fn error<T: std::fmt::Debug>(result: io::Result<T>) -> Option<i32> {
    result.unwrap_err().raw_os_error()
}

/// Returns a fake hierarchy of regular files in a new temporary directory, which the caller
/// keeps: the root, which holds the agent, with an empty `cgroup.subtree_control`, and the
/// workload cgroup `workload`, without processes, as the layout of an agent that runs in the
/// root.
fn hierarchy() -> (tempfile::TempDir, Layout) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("cgroup");
    let workload = root.join("workload");
    fs::create_dir_all(&workload).unwrap();
    fs::write(root.join(SUBTREE_CONTROL), b"").unwrap();
    for (file, contents) in [
        (PIDS_CURRENT, b"0\n".as_slice()),
        (KILL, b""),
        (PROCS, b""),
        (SUBTREE_CONTROL, b""),
    ] {
        fs::write(workload.join(file), contents).unwrap();
    }
    let layout = Layout::new(&root, &workload).unwrap();
    (temporary, layout)
}

/// Creates the fake cgroup `name` of an execution below `layout`'s workload cgroup, with
/// the processes that `pids.current` counts, and returns its handle.
fn fake_execution(layout: &Layout, name: &str, pids: &[u8]) -> ExecutionCgroup {
    let path = layout.workload().join(name);
    fs::create_dir(&path).unwrap();
    for (file, contents) in [(PIDS_CURRENT, pids), (KILL, b""), (PROCS, b"")] {
        fs::write(path.join(file), contents).unwrap();
    }
    ExecutionCgroup::open(layout, Some(name)).unwrap()
}

#[test]
fn layouts_keep_the_agent_out_of_the_workload_subtree() {
    for (agent, workload) in [
        ("/sys/fs/cgroup", "/sys/fs/cgroup/nvx-exec"),
        ("/sys/fs/cgroup/agent", "/sys/fs/cgroup/container"),
        ("/sys/fs/cgroup/agent", "/sys/fs/cgroup/agent-workloads"),
        (
            "/sys/fs/cgroup/workloads/agent",
            "/sys/fs/cgroup/workloads/executions",
        ),
    ] {
        let layout = Layout::new(agent, workload).unwrap();
        assert_eq!(
            (layout.agent(), layout.workload()),
            (Path::new(agent), Path::new(workload))
        );
    }
    for (agent, workload) in [
        ("sys/fs/cgroup", "/sys/fs/cgroup/nvx-exec"),
        ("/sys/fs/cgroup", "sys/fs/cgroup/nvx-exec"),
        ("/sys/fs/cgroup/agent", "/"),
        ("/sys/fs/cgroup/workload", "/sys/fs/cgroup/workload"),
        ("/sys/fs/cgroup/workload/", "/sys/fs/cgroup/workload"),
        ("/sys/fs/cgroup/workload/agent", "/sys/fs/cgroup/workload"),
        ("/sys/fs/cgroup", "/sys/fs/cgroup/nvx-exec/.."),
        (
            "/sys/fs/cgroup/workload/../agent",
            "/sys/fs/cgroup/workload",
        ),
    ] {
        assert_eq!(
            error(Layout::new(agent, workload)),
            Some(Errno::EINVAL as i32),
            "{agent} {workload}"
        );
    }
}

#[test]
fn executions_run_in_the_workload_cgroup_or_in_one_child_of_it() {
    let (_temporary, layout) = hierarchy();
    let workload = ExecutionCgroup::open(&layout, None).unwrap();
    assert_eq!(workload.path(), layout.workload());
    for name in ["exec-1", "exec.1", "..."] {
        fs::create_dir(layout.workload().join(name)).unwrap();
        let execution = ExecutionCgroup::open(&layout, Some(name)).unwrap();
        assert_eq!(execution.path(), layout.workload().join(name));
    }
    for name in ["", ".", "..", "a/b", "/a", "a/", "./a", "a\0b"] {
        assert_eq!(
            error(ExecutionCgroup::open(&layout, Some(name))),
            Some(Errno::EINVAL as i32),
            "{name:?}"
        );
        assert_eq!(
            error(ExecutionCgroup::prepare(&layout, Some(name), &[])),
            Some(Errno::EINVAL as i32),
            "{name:?}"
        );
    }
    // A handle holds its cgroups' directories, which must exist.
    assert_eq!(
        error(ExecutionCgroup::open(&layout, Some("missing"))),
        Some(Errno::ENOENT as i32)
    );
    fs::write(layout.workload().join("file"), b"").unwrap();
    assert_eq!(
        error(ExecutionCgroup::open(&layout, Some("file"))),
        Some(Errno::ENOTDIR as i32)
    );
    let missing = Layout::new(layout.agent(), layout.agent().join("missing")).unwrap();
    assert_eq!(
        error(ExecutionCgroup::open(&missing, None)),
        Some(Errno::ENOENT as i32)
    );
}

#[test]
fn preparation_creates_the_cgroup_and_enables_its_controllers_together() {
    let (temporary, _) = hierarchy();
    let root = temporary.path().join("cgroup");
    let layout = Layout::new(&root, root.join("fresh")).unwrap();
    let control = root.join(SUBTREE_CONTROL);
    // A new cgroup lacks the files of both controllers, which one write enables.
    let failure = ExecutionCgroup::prepare(&layout, None, &[("memory.max", "max")]).unwrap_err();
    assert!(layout.workload().is_dir());
    assert_eq!(fs::read(&control).unwrap(), b"+pids +memory");
    // A fake hierarchy gains no file when a controller is enabled.
    assert_eq!(failure.raw_os_error(), Some(Errno::ENOENT as i32));

    // With every file in place, nothing is enabled, and the values are written in order.
    fs::write(&control, b"").unwrap();
    for file in [PIDS_CURRENT, "memory.max", "memory.high"] {
        fs::write(layout.workload().join(file), b"").unwrap();
    }
    let execution = ExecutionCgroup::prepare(
        &layout,
        None,
        &[("memory.max", "1024"), ("memory.high", "512")],
    )
    .unwrap();
    assert_eq!(execution.path(), layout.workload());
    assert_eq!(fs::read(&control).unwrap(), b"");
    assert_eq!(
        fs::read(layout.workload().join("memory.max")).unwrap(),
        b"1024"
    );
    assert_eq!(
        fs::read(layout.workload().join("memory.high")).unwrap(),
        b"512"
    );

    // An existing workload cgroup is accepted, a file in its place is not.
    fs::remove_dir_all(layout.workload()).unwrap();
    fs::write(layout.workload(), b"").unwrap();
    assert_eq!(
        error(ExecutionCgroup::prepare(&layout, None, &[])),
        Some(Errno::ENOTDIR as i32)
    );
}

#[test]
fn invalid_resources_change_nothing() {
    let (temporary, _) = hierarchy();
    let root = temporary.path().join("cgroup");
    let layout = Layout::new(&root, root.join("fresh")).unwrap();
    for file in [
        "cgroup.procs",
        "cgroup.subtree_control",
        "pids",
        ".max",
        "pids.",
        "pids..max",
        "Pids.max",
        "pids/max",
        "../pids.max",
        "pids.max/x",
        "pids.max\n",
    ] {
        assert_eq!(
            error(ExecutionCgroup::prepare(&layout, None, &[(file, "1")])),
            Some(Errno::EINVAL as i32),
            "{file:?}"
        );
    }
    assert!(!layout.workload().exists());
    assert_eq!(fs::read(root.join(SUBTREE_CONTROL)).unwrap(), b"");
    for file in [
        "hugetlb.2MB.max",
        "memory.swap.max",
        "cpuset.cpus.partition",
        "io.max",
    ] {
        assert_eq!(controller(file).unwrap(), file.split('.').next().unwrap());
    }
}

#[test]
fn an_execution_s_own_cgroup_is_new_and_takes_its_controllers_from_the_workload() {
    let (_temporary, layout) = hierarchy();
    let root = layout.agent().to_owned();
    let execution = ExecutionCgroup::prepare(&layout, Some("exec-1"), &[]).unwrap();
    assert!(execution.path().is_dir());
    // The workload cgroup counts its processes already, so only it enables the controller.
    assert_eq!(fs::read(root.join(SUBTREE_CONTROL)).unwrap(), b"");
    assert_eq!(
        fs::read(layout.workload().join(SUBTREE_CONTROL)).unwrap(),
        b"+pids"
    );
    // A second execution of the same name would share the first one's cgroup.
    let duplicate = ExecutionCgroup::prepare(&layout, Some("exec-1"), &[]).unwrap_err();
    assert_eq!(duplicate.kind(), io::ErrorKind::AlreadyExists);
    // A workload cgroup that lacks the controller gets it from its parent first.
    fs::remove_file(layout.workload().join(PIDS_CURRENT)).unwrap();
    ExecutionCgroup::prepare(&layout, Some("exec-2"), &[]).unwrap();
    assert_eq!(fs::read(root.join(SUBTREE_CONTROL)).unwrap(), b"+pids");
}

#[test]
fn a_failed_preparation_leaves_no_cgroup_of_the_execution_s_own() {
    let (_temporary, layout) = hierarchy();
    // No controller creates `pids.max` in a fake cgroup, so its write fails once the
    // execution's cgroup exists.
    let failed = ExecutionCgroup::prepare(&layout, Some("exec-1"), &[("pids.max", "16")]);
    assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::NotFound);
    assert!(!layout.workload().join("exec-1").exists());
    // The name can be prepared anew.
    let execution = ExecutionCgroup::prepare(&layout, Some("exec-1"), &[]).unwrap();
    assert!(execution.path().is_dir());
    // An execution in the workload cgroup itself leaves the shared cgroup in place.
    let failed = ExecutionCgroup::prepare(&layout, None, &[("pids.max", "16")]);
    assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::NotFound);
    assert!(layout.workload().join(PIDS_CURRENT).is_file());
}

#[test]
fn an_empty_resource_value_reaches_its_file_as_a_newline() {
    let (_temporary, layout) = hierarchy();
    let limit = layout.workload().join("pids.max");
    fs::write(&limit, b"").unwrap();
    ExecutionCgroup::prepare(&layout, None, &[("pids.max", "")]).unwrap();
    // The kernel gets a value to apply or refuse, as from `echo`, not a write of nothing.
    assert_eq!(fs::read(&limit).unwrap(), b"\n");
}

#[test]
fn occupancy_is_the_scope_s_canonical_process_count() {
    let (_temporary, layout) = hierarchy();
    let execution = fake_execution(&layout, "exec-1", b"1\n");
    assert!(execution.is_occupied(Scope::Execution).unwrap());
    assert!(!execution.is_occupied(Scope::Workload).unwrap());
    let count = layout.workload().join(PIDS_CURRENT);
    for (contents, expected) in [
        (b"0\n".as_slice(), Ok(false)),
        (b"10\n", Ok(true)),
        (b"4194304\n", Ok(true)),
        (b"", Err(Errno::EPROTO)),
        (b"0", Err(Errno::EPROTO)),
        (b"00\n", Err(Errno::EPROTO)),
        (b"-1\n", Err(Errno::EPROTO)),
        (b" 1\n", Err(Errno::EPROTO)),
        (b"max\n", Err(Errno::EPROTO)),
        (&[b'1'; 40], Err(Errno::EOVERFLOW)),
    ] {
        fs::write(&count, contents).unwrap();
        assert_eq!(
            execution
                .is_occupied(Scope::Workload)
                .map_err(|error| Errno::from_raw(error.raw_os_error().unwrap())),
            expected,
            "{contents:?}"
        );
    }
    fs::remove_file(&count).unwrap();
    assert_eq!(
        error(execution.is_occupied(Scope::Workload)),
        Some(Errno::ENOENT as i32)
    );
}

#[test]
fn a_kill_reaches_the_scope_s_cgroup_alone() {
    let (_temporary, layout) = hierarchy();
    let execution = fake_execution(&layout, "exec-1", b"0\n");
    execution.kill(Scope::Execution).unwrap();
    assert_eq!(fs::read(execution.path().join(KILL)).unwrap(), b"1");
    assert_eq!(fs::read(layout.workload().join(KILL)).unwrap(), b"");
    execution.kill(Scope::Workload).unwrap();
    assert_eq!(fs::read(layout.workload().join(KILL)).unwrap(), b"1");
    fs::remove_file(execution.path().join(KILL)).unwrap();
    assert_eq!(
        error(execution.kill(Scope::Execution)),
        Some(Errno::ENOENT as i32)
    );
}

#[test]
fn settlement_collects_statuses_between_its_checks_until_the_deadline() {
    let (_temporary, layout) = hierarchy();
    let execution = fake_execution(&layout, "exec-1", b"2\n");
    let count = execution.path().join(PIDS_CURRENT);
    // Each collection lets one process go, as collecting a zombie does.
    let collections = Cell::new(0);
    execution
        .settle(Scope::Execution, Duration::from_secs(10), || {
            collections.set(collections.get() + 1);
            let left = 3 - collections.get().min(3);
            fs::write(&count, format!("{left}\n"))
        })
        .unwrap();
    assert_eq!(collections.get(), 3);

    fs::write(&count, b"1\n").unwrap();
    let failed = execution.settle(Scope::Execution, Duration::from_secs(10), || {
        Err(Errno::ECHILD.into())
    });
    assert_eq!(error(failed), Some(Errno::ECHILD as i32));
    collections.set(0);
    let timed_out = execution.settle(Scope::Execution, Duration::from_millis(50), || {
        collections.set(collections.get() + 1);
        Ok(())
    });
    assert_eq!(error(timed_out), Some(Errno::ETIMEDOUT as i32));
    assert!(collections.get() > 1);
    fs::write(&count, b"bad\n").unwrap();
    assert_eq!(
        error(execution.settle(Scope::Execution, Duration::ZERO, || Ok(()))),
        Some(Errno::EPROTO as i32)
    );
    // The workload cgroup is empty, whatever its execution's count.
    execution
        .settle(Scope::Workload, Duration::ZERO, || Ok(()))
        .unwrap();
}

#[test]
fn processes_are_placed_in_the_execution_s_cgroup() {
    let (_temporary, layout) = hierarchy();
    let execution = fake_execution(&layout, "exec-1", b"0\n");
    let procs = execution.path().join(PROCS);
    let spawned = shell("exit 0")
        .cgroup(execution.placement().unwrap())
        .spawn()
        .unwrap();
    let mut child = Child::new(spawned.id()).unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(
        fs::read(&procs).unwrap(),
        format!("{}\n", child.id()).into_bytes()
    );
    execution.place(4_194_304).unwrap();
    assert_eq!(fs::read(&procs).unwrap(), b"4194304\n");
    for id in [0, u32::MAX] {
        assert_eq!(error(execution.place(id)), Some(Errno::EINVAL as i32));
    }
}

#[test]
fn a_handle_reaches_its_own_cgroups_alone_once_another_takes_a_path() {
    let (_temporary, layout) = hierarchy();
    let old = fake_execution(&layout, "exec-1", b"0\n");
    let clone = old.clone();
    // The caller removes the cgroup, as `remove` does in a real hierarchy, and prepares
    // another of the same name.
    fs::remove_dir_all(old.path()).unwrap();
    let new = fake_execution(&layout, "exec-1", b"1\n");
    for handle in [&old, &clone] {
        assert_eq!(
            error(handle.kill(Scope::Execution)),
            Some(Errno::ENOENT as i32)
        );
        assert_eq!(
            error(handle.is_occupied(Scope::Execution)),
            Some(Errno::ENOENT as i32)
        );
        assert_eq!(error(handle.place(1)), Some(Errno::ENOENT as i32));
        assert_eq!(error(handle.placement()), Some(Errno::ENOENT as i32));
        handle.remove().unwrap();
    }
    assert!(new.path().is_dir());
    assert_eq!(fs::read(new.path().join(KILL)).unwrap(), b"");
    assert_eq!(fs::read(new.path().join(PROCS)).unwrap(), b"");
    assert!(new.is_occupied(Scope::Execution).unwrap());
    // The workload cgroup is still the one that the old handle opened.
    old.kill(Scope::Workload).unwrap();
    assert_eq!(fs::read(layout.workload().join(KILL)).unwrap(), b"1");
    new.kill(Scope::Execution).unwrap();
    assert_eq!(fs::read(new.path().join(KILL)).unwrap(), b"1");
}

#[test]
fn only_an_empty_cgroup_of_the_execution_s_own_is_removed() {
    let (_temporary, layout) = hierarchy();
    ExecutionCgroup::open(&layout, None)
        .unwrap()
        .remove()
        .unwrap();
    assert!(layout.workload().is_dir());
    let execution = fake_execution(&layout, "exec-1", b"1\n");
    assert_eq!(error(execution.remove()), Some(Errno::EBUSY as i32));
    fs::remove_file(execution.path().join(PIDS_CURRENT)).unwrap();
    assert_eq!(error(execution.remove()), Some(Errno::ENOENT as i32));
    fs::remove_dir_all(execution.path()).unwrap();
    execution.remove().unwrap();
}

#[test]
fn cgroup_operations_leave_an_unrelated_child_s_status() {
    let mut unrelated = std::process::Command::new("/bin/true").spawn().unwrap();
    let process = ProcessId::from_child(&unrelated);
    while !sys::tests::has_exited(process).unwrap() {
        std::thread::yield_now();
    }
    let (_temporary, layout) = hierarchy();
    let workload = ExecutionCgroup::prepare(&layout, None, &[]).unwrap();
    let execution = fake_execution(&layout, "exec-1", b"0\n");
    for cgroup in [&workload, &execution] {
        let spawned = shell("exit 0")
            .cgroup(cgroup.placement().unwrap())
            .spawn()
            .unwrap();
        let mut child = Child::new(spawned.id()).unwrap();
        for scope in [Scope::Execution, Scope::Workload] {
            assert!(!cgroup.is_occupied(scope).unwrap());
            cgroup.kill(scope).unwrap();
            cgroup
                .settle(scope, Duration::from_secs(10), || {
                    child.try_wait().map(drop)
                })
                .unwrap();
        }
        assert!(child.wait().unwrap().success());
    }
    // Nothing collected a status that it did not name.
    assert!(unrelated.try_wait().unwrap().unwrap().success());
}

/// A workload cgroup in the real hierarchy, which the test removes, with everything in it,
/// when it ends.
struct RealWorkload(Layout);

impl RealWorkload {
    /// Creates a new workload cgroup below the root of the hierarchy at `/sys/fs/cgroup`,
    /// with the test's own cgroup as the agent's. It returns `None` where the test cannot
    /// change the hierarchy: unless the test runs as root in a cgroup v2 hierarchy with the
    /// `pids` controller, in which it can create a cgroup, as a read-only mount, or one
    /// that a user namespace's root does not own, does not let it.
    fn new(name: &str) -> Option<Self> {
        let root = Path::new("/sys/fs/cgroup");
        let controllers = fs::read_to_string(root.join("cgroup.controllers")).ok()?;
        if !nix::unistd::geteuid().is_root()
            || !controllers
                .split_whitespace()
                .any(|controller| controller == "pids")
        {
            return None;
        }
        let own = fs::read_to_string("/proc/self/cgroup").ok()?;
        let own = own.lines().find_map(|line| line.strip_prefix("0::"))?;
        let agent = root.join(own.trim_start_matches('/'));
        let workload = root.join(format!("nvx-test-{}-{name}", std::process::id()));
        // The test's cgroup lies outside the mount's view in another cgroup namespace,
        // where it reads as a path with `..` components, which no layout accepts.
        let layout = Layout::new(agent, workload).ok()?;
        match fs::create_dir(layout.workload()) {
            Ok(()) => Some(Self(layout)),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ReadOnlyFilesystem | io::ErrorKind::PermissionDenied
                ) =>
            {
                None
            }
            Err(error) => panic!(
                "cannot create the test's cgroup {}: {error}",
                layout.workload().display()
            ),
        }
    }
}

impl Drop for RealWorkload {
    /// Kills whatever is left in the workload cgroup, then removes its cgroups.
    fn drop(&mut self) {
        let workload = self.0.workload();
        let _ = write(&workload.join(KILL), b"1");
        for _ in 0..1000 {
            if fs::read(workload.join(PIDS_CURRENT)).is_ok_and(|count| count == b"0\n") {
                break;
            }
            std::thread::sleep(SETTLE_INTERVAL);
        }
        if let Ok(entries) = fs::read_dir(workload) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    let _ = fs::remove_dir(entry.path());
                }
            }
        }
        let _ = fs::remove_dir(workload);
    }
}

/// Returns whether `reader` polls readable, with data or end of file, within `timeout`.
fn readable(reader: &std::io::PipeReader, timeout: Duration) -> bool {
    let mut descriptors = [PollFd::new(reader.as_fd(), PollFlags::POLLIN)];
    nix::poll::poll(&mut descriptors, PollTimeout::try_from(timeout).unwrap()).unwrap() == 1
}

#[test]
fn descendants_that_hold_pipes_are_found_and_settled() {
    // The test changes the real hierarchy, which needs root; elsewhere it does nothing.
    let Some(workload) = RealWorkload::new("settle") else {
        return;
    };
    let layout = &workload.0;
    let first = ExecutionCgroup::prepare(layout, Some("first"), &[("pids.max", "16")]).unwrap();
    let second = ExecutionCgroup::prepare(layout, Some("second"), &[]).unwrap();
    assert_eq!(
        fs::read_to_string(first.path().join("pids.max")).unwrap(),
        "16\n"
    );
    assert_eq!(
        ExecutionCgroup::prepare(layout, Some("first"), &[])
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    // A value that the kernel refuses, an empty one included, leaves no cgroup behind, so the
    // name can be prepared anew.
    for value in ["-1", ""] {
        assert_eq!(
            error(ExecutionCgroup::prepare(
                layout,
                Some("third"),
                &[("pids.max", value)]
            )),
            Some(Errno::EINVAL as i32),
            "{value:?}"
        );
        assert!(!layout.workload().join("third").exists());
    }
    ExecutionCgroup::prepare(layout, Some("third"), &[("pids.max", "8")])
        .unwrap()
        .remove()
        .unwrap();

    // The first execution's program ends at once, but leaves a descendant in a session of its
    // own, outside the program's process group, which holds the program's output open.
    let (mut output, writer) = std::io::pipe().unwrap();
    let spawned = shell("setsid sleep 60 & echo started")
        .stdout(writer)
        .cgroup(first.placement().unwrap())
        .spawn()
        .unwrap();
    let mut child = Child::with_pidfd(spawned.id()).unwrap();
    // A child joins its cgroup before its program runs, which its launch report waits for.
    assert!(spawned.into_report().wait().unwrap().is_none());
    assert!(child.wait().unwrap().success());
    let mut started = [0u8; 8];
    output.read_exact(&mut started).unwrap();
    assert_eq!(&started, b"started\n");
    assert!(!readable(&output, Duration::from_millis(200)));
    assert!(first.is_occupied(Scope::Execution).unwrap());

    // The second execution keeps running.
    let spawned = shell("exec sleep 60")
        .cgroup(second.placement().unwrap())
        .spawn()
        .unwrap();
    let mut sibling = Child::new(spawned.id()).unwrap();
    assert!(spawned.into_report().wait().unwrap().is_none());

    // The first execution's cgroup finds the descendant: once it is settled, the output ends.
    first.kill(Scope::Execution).unwrap();
    first
        .settle(Scope::Execution, Duration::from_secs(10), || Ok(()))
        .unwrap();
    assert!(readable(&output, Duration::ZERO));
    let mut rest = Vec::new();
    output.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty());
    first.remove().unwrap();
    assert!(!first.path().exists());
    // A cgroup prepared at the removed one's path is out of the old handle's reach.
    let again = ExecutionCgroup::prepare(layout, Some("first"), &[]).unwrap();
    assert_eq!(
        error(first.kill(Scope::Execution)),
        Some(Errno::ENOENT as i32)
    );
    assert_eq!(
        error(first.is_occupied(Scope::Execution)),
        Some(Errno::ENOENT as i32)
    );
    first.remove().unwrap();
    assert!(again.path().is_dir());
    again.remove().unwrap();
    assert_eq!(sibling.try_wait().unwrap(), None);
    assert!(second.is_occupied(Scope::Execution).unwrap());

    // Settling the whole workload subtree ends the sibling too.
    second.kill(Scope::Workload).unwrap();
    second
        .settle(Scope::Workload, Duration::from_secs(10), || {
            sibling.try_wait().map(drop)
        })
        .unwrap();
    assert_eq!(sibling.status().unwrap().signal(), Some(nix::libc::SIGKILL));
    second.remove().unwrap();
}

/// Runs a racer on another thread once this thread's next turn pauses, and lets the turn go
/// on once the racer has returned, or half a second after its setup, as it does when the turn
/// holds the racer off. The racer runs `setup` first, which the turn waits for, so that it
/// takes place within the pause however late the thread starts, then `racer`, with what
/// `setup` returned. Joining the thread returns what `racer` returned, or `None` if no turn
/// paused.
fn race<S, T: Send + 'static>(
    setup: impl FnOnce() -> S + Send + 'static,
    racer: impl FnOnce(S) -> T + Send + 'static,
) -> std::thread::JoinHandle<Option<T>> {
    let (go, start) = std::sync::mpsc::channel();
    let (ready, set_up) = std::sync::mpsc::channel();
    let (finished, done) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        start.recv().ok()?;
        let state = setup();
        let _ = ready.send(());
        let result = racer(state);
        let _ = finished.send(());
        Some(result)
    });
    PAUSE.set(Some(Box::new(move || {
        go.send(()).unwrap();
        // A setup that fails drops its sender, which ends the wait at once.
        let _ = set_up.recv_timeout(Duration::from_secs(10));
        let _ = done.recv_timeout(Duration::from_millis(500));
    })));
    thread
}

/// Returns what the racer of `thread` returned, once no turn of this thread is left to pause.
fn finish<T>(thread: std::thread::JoinHandle<Option<T>>) -> T {
    drop(PAUSE.take());
    thread.join().unwrap().expect("the turn paused")
}

#[test]
fn a_removal_leaves_a_cgroup_prepared_at_its_path_by_a_concurrent_one() {
    // The test changes the real hierarchy, which needs root; elsewhere it does nothing.
    let Some(workload) = RealWorkload::new("removals") else {
        return;
    };
    let layout = workload.0.clone();
    let first = ExecutionCgroup::prepare(&layout, Some("exec"), &[]).unwrap();
    let delayed = first.clone();
    // Once the delayed clone found the cgroup empty, the racer removes it through the other
    // clone and prepares another of the same name, unless the delayed removal holds it off.
    let racer = race(
        move || {
            // The delayed removal pauses between its check and its removal.
            assert!(!first.is_occupied(Scope::Execution).unwrap());
            first
        },
        {
            let layout = layout.clone();
            move |first: ExecutionCgroup| {
                first.remove().unwrap();
                ExecutionCgroup::prepare(&layout, Some("exec"), &[]).unwrap()
            }
        },
    );
    delayed.remove().unwrap();
    let replacement = finish(racer);
    assert!(replacement.path().is_dir());
    assert!(!replacement.is_occupied(Scope::Execution).unwrap());
    replacement.remove().unwrap();
    assert!(!replacement.path().exists());
}

#[test]
fn a_failed_preparation_removes_its_own_cgroup_alone() {
    // The test changes the real hierarchy, which needs root; elsewhere it does nothing.
    let Some(workload) = RealWorkload::new("preparations") else {
        return;
    };
    let layout = workload.0.clone();
    // A first preparation gives the workload cgroup's children the `pids` controller.
    ExecutionCgroup::prepare(&layout, Some("first"), &[])
        .unwrap()
        .remove()
        .unwrap();
    // Once the preparation created its cgroup, the racer opens that cgroup, within the pause,
    // removes it, and prepares another of the same name, unless the preparation holds it off.
    // The preparation then fails, as the kernel refuses its value, and removes its cgroup
    // again.
    let racer = race(
        {
            let layout = layout.clone();
            move || ExecutionCgroup::open(&layout, Some("exec")).unwrap()
        },
        {
            let layout = layout.clone();
            move |opened: ExecutionCgroup| {
                opened.remove().unwrap();
                ExecutionCgroup::prepare(&layout, Some("exec"), &[]).unwrap()
            }
        },
    );
    assert_eq!(
        error(ExecutionCgroup::prepare(
            &layout,
            Some("exec"),
            &[("pids.max", "-1")]
        )),
        Some(Errno::EINVAL as i32)
    );
    let replacement = finish(racer);
    assert!(replacement.path().is_dir());
    assert!(!replacement.is_occupied(Scope::Execution).unwrap());
    replacement.remove().unwrap();
}
