use std::process::Command;

use super::super::seam::tests::TestSystem;
use super::*;

#[test]
fn task_count_requires_a_canonical_decimal() {
    let _process = super::super::tests::PROCESS_TEST_LOCK.lock().unwrap();
    let (_temporary, system) = TestSystem::with_cgroup();
    let tasks = system.paths.cgroup_root.join("nvx-exec/pids.current");
    for (contents, expected) in [
        (b"0\n".as_slice(), false),
        (b"1\n", true),
        (b"10\n", true),
        (b"4194304\n", true),
    ] {
        std::fs::write(&tasks, contents).unwrap();
        assert_eq!(has_tasks(&system).unwrap(), expected, "{contents:?}");
    }
    for contents in [
        b"".as_slice(),
        b"\n",
        b"0",
        b"00\n",
        b"01\n",
        b"-1\n",
        b"+1\n",
        b" 1\n",
        b"1 \n",
        b"0\n0\n",
        b"1\0\n",
        b"max\n",
    ] {
        std::fs::write(&tasks, contents).unwrap();
        assert_eq!(
            has_tasks(&system).unwrap_err(),
            Errno::EPROTO,
            "{contents:?}"
        );
    }
    std::fs::write(&tasks, [b'1'; 40]).unwrap();
    assert_eq!(has_tasks(&system).unwrap_err(), Errno::EOVERFLOW);
}

#[test]
fn missing_and_unreadable_task_counts_are_errors() {
    let _process = super::super::tests::PROCESS_TEST_LOCK.lock().unwrap();
    let (_temporary, system) = TestSystem::with_cgroup();
    let tasks = system.paths.cgroup_root.join("nvx-exec/pids.current");
    std::fs::remove_file(&tasks).unwrap();
    assert_eq!(has_tasks(&system).unwrap_err(), Errno::ENOENT);
    std::fs::create_dir(&tasks).unwrap();
    assert!(has_tasks(&system).is_err());
}

#[test]
fn preparation_enables_the_pids_controller_once() {
    let _process = super::super::tests::PROCESS_TEST_LOCK.lock().unwrap();
    let (_temporary, mut system) = TestSystem::with_cgroup();
    let root = system.paths.cgroup_root.clone();
    prepare(&mut system).unwrap();
    assert_eq!(
        std::fs::read(root.join("cgroup.subtree_control")).unwrap(),
        b""
    );
    std::fs::remove_file(root.join("nvx-exec/pids.current")).unwrap();
    prepare(&mut system).unwrap();
    assert_eq!(
        std::fs::read(root.join("cgroup.subtree_control")).unwrap(),
        b"+pids"
    );
    std::fs::remove_file(root.join("cgroup.subtree_control")).unwrap();
    assert_eq!(prepare(&mut system).unwrap_err(), Errno::ENOENT);
}

#[test]
fn settlement_verifies_emptiness_and_propagates_failures() {
    if super::super::tests::isolate_process_test(
        "NVX_CGROUP_SETTLEMENT_TEST_CHILD",
        "guest::managed_agent::cgroup::tests::settlement_verifies_emptiness_and_propagates_failures",
    ) {
        return;
    }
    let _process = super::super::tests::PROCESS_TEST_LOCK.lock().unwrap();
    let (_temporary, mut system) = TestSystem::with_cgroup();
    let execution = system.paths.cgroup_root.join("nvx-exec");
    settle(&mut system).unwrap();
    assert_eq!(std::fs::read(execution.join("cgroup.kill")).unwrap(), b"1");

    std::fs::remove_file(execution.join("cgroup.kill")).unwrap();
    assert_eq!(settle(&mut system).unwrap_err(), Errno::ENOENT);
    std::fs::write(execution.join("cgroup.kill"), b"").unwrap();
    std::fs::write(execution.join("pids.current"), b"bad\n").unwrap();
    assert_eq!(settle(&mut system).unwrap_err(), Errno::EPROTO);
}

#[test]
fn verification_reaps_orphaned_children() {
    if super::super::tests::isolate_process_test(
        "NVX_CGROUP_REAP_TEST_CHILD",
        "guest::managed_agent::cgroup::tests::verification_reaps_orphaned_children",
    ) {
        return;
    }
    let _process = super::super::tests::PROCESS_TEST_LOCK.lock().unwrap();
    let (_temporary, system) = TestSystem::with_cgroup();
    let mut child = Command::new("/bin/true").spawn().unwrap();
    let process = ProcessId::from_child(&child);
    while !sys::tests::has_exited(process).unwrap() {
        std::thread::yield_now();
    }
    assert!(!verify_empty(&system).unwrap());
    assert_eq!(sys::wait(process), None);
    let _ = child.wait();
}

#[test]
fn unreaped_tasks_at_deadline_are_not_settled() {
    if super::super::tests::isolate_process_test(
        "NVX_CGROUP_DEADLINE_TEST_CHILD",
        "guest::managed_agent::cgroup::tests::unreaped_tasks_at_deadline_are_not_settled",
    ) {
        return;
    }
    let _process = super::super::tests::PROCESS_TEST_LOCK.lock().unwrap();
    let (_temporary, mut system) = TestSystem::with_cgroup();
    std::fs::write(
        system.paths.cgroup_root.join("nvx-exec/pids.current"),
        b"1\n",
    )
    .unwrap();
    assert_eq!(settle(&mut system).unwrap_err(), Errno::ETIMEDOUT);
}
