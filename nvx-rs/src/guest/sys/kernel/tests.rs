//! Checks of the kernel queries and calls on the build host.

use super::*;

/// Returns the nice value of the process `process`, the 19th field of its procfs `stat` file.
#[cfg(target_arch = "x86_64")]
pub(crate) fn nice_of(process: crate::guest::sys::ProcessId) -> i32 {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", process.raw())).unwrap();
    // The command name, the second field, is in parentheses, and may hold spaces.
    let (_, fields) = stat.rsplit_once(") ").unwrap();
    fields.split(' ').nth(16).unwrap().parse().unwrap()
}

#[test]
fn the_process_has_a_vdso() {
    assert!(has_vdso());
}

#[test]
#[cfg(target_arch = "x86_64")]
fn a_nice_value_is_set_for_the_named_process() {
    use crate::guest::sys::ProcessId;
    use crate::guest::sys::tests::{CAP_SYS_NICE, holds_capability};

    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let process = ProcessId::from_child(&child);
    // Any process may raise the nice value of its user's processes, up to 19.
    assert_eq!(set_nice(Some(process), 19), Ok(()));
    assert_eq!(nice_of(process), 19);
    // Lowering it needs `CAP_SYS_NICE`, or here an `RLIMIT_NICE` of at least 30.
    match set_nice(Some(process), -10) {
        Ok(()) => assert_eq!(nice_of(process), -10),
        Err(error) => {
            assert_eq!(error, Errno::EACCES);
            assert!(!holds_capability(CAP_SYS_NICE).unwrap());
            assert_eq!(nice_of(process), 19);
        }
    }
    child.kill().unwrap();
    // Another test's collector of every child's status may collect this one's first.
    let _ = child.wait();
    assert_eq!(set_nice(Some(process), 19), Err(Errno::ESRCH));
}

#[test]
#[cfg(target_arch = "x86_64")]
fn a_nice_value_needs_a_named_process() {
    // A `set_nice` that read `None` as the calling thread would change the test thread's nice
    // value, so the test runs in a process of its own.
    if crate::guest::managed_agent::tests::isolate_process_test(
        "NVX_SET_NICE_TEST_CHILD",
        "guest::sys::kernel::tests::a_nice_value_needs_a_named_process",
    ) {
        return;
    }
    let thread = crate::guest::sys::current_thread();
    let nice = nice_of(thread);
    assert_eq!(set_nice(None, 19), Err(Errno::EINVAL));
    assert_eq!(nice_of(thread), nice);
}

#[test]
fn the_kernel_log_reads_into_any_buffer() {
    // Containers and unprivileged users may not read the log.
    if let Ok(size) = kernel_log_size() {
        assert!(size > 0);
        let mut buffer = vec![0; size];
        if let Ok(count) = read_kernel_log(&mut buffer) {
            assert!(count <= size);
        }
        assert!(matches!(read_kernel_log(&mut []), Ok(0) | Err(_)));
    }
}
