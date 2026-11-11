use std::cell::Cell;
use std::io::{Seek, Write};
use std::os::fd::{AsFd, AsRawFd};

use nix::fcntl::{FcntlArg, FdFlag, fcntl};

use super::*;

thread_local! {
    /// Whether a test has `pidfd_open` answer this thread as a kernel before Linux 6.16 does.
    pub(crate) static BEFORE_LINUX_6_16: Cell<bool> = const { Cell::new(false) };
}

/// Returns `opened`, this kernel's answer to `pidfd_open` for `process`, as a kernel before Linux
/// 6.16 gives it, if a test has asked for one on this thread.
///
/// For an ID that no thread-group leader has, but that something still holds, such a kernel
/// answers `EINVAL`, where a later one answers `ENOENT` if a thread has the ID, and `ESRCH` if a
/// process group or a session that a reaped process led keeps it. Only a process group is looked
/// for, as the tests leave no other trace of a reaped process. Every other answer, such as
/// `ESRCH` for an ID that nothing holds, is the same before Linux 6.16.
pub(super) fn as_before_linux_6_16(
    process: ProcessId,
    opened: Result<nix::libc::c_long, Errno>,
) -> Result<nix::libc::c_long, Errno> {
    if !BEFORE_LINUX_6_16.get() {
        return opened;
    }
    match opened {
        Err(Errno::ENOENT) => Err(Errno::EINVAL),
        Err(Errno::ESRCH) if process_group_exists(process) => Err(Errno::EINVAL),
        opened => opened,
    }
}

/// Returns whether a process group has the ID of `process`, as a signal 0 to the group tells,
/// even one whose processes the caller may not signal.
fn process_group_exists(process: ProcessId) -> bool {
    signal_process_group(process, 0) != Err(Errno::ESRCH)
}

#[test]
#[cfg(target_arch = "aarch64")]
fn arm_diagnostics_write_console_bytes_without_x86_port_offsets() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let descriptor = FileDescriptor::from_owned(file.reopen().unwrap().into());
    write_port_diagnostic(&descriptor, b"ARM diagnostic\n").unwrap();
    assert_eq!(std::fs::read(file.path()).unwrap(), b"ARM diagnostic\n");
}

/// Forks a test's process, which the harness's threads keep [`fork`] from forking, for a test
/// that the process runs alone, as `isolate_process_test` starts one. The child has only the
/// calling thread, so [`fork`] works in it.
pub(crate) fn fork_isolated_test() -> Result<ForkResult, Errno> {
    // SAFETY: the process runs only the calling test, whose harness thread waits for it, so the
    // child inherits no lock that another thread holds.
    match unsafe { nix::unistd::fork() }? {
        nix::unistd::ForkResult::Child => Ok(ForkResult::Child),
        nix::unistd::ForkResult::Parent { child } => {
            Ok(ForkResult::Parent(ProcessId::from_pid(child)))
        }
    }
}

/// Starts a child that ends at once with `status`, before it could run a program, and that
/// reports its end with no signal, as a child that `clone` starts with no exit signal does, for
/// the tests of the waits that name such a child.
pub(crate) fn spawn_child_without_exit_signal(status: u8) -> Result<ProcessId, Errno> {
    let none: nix::libc::c_long = 0;
    // SAFETY: without `CLONE_VM`, and with no stack of its own, the child runs in a copy of this
    // process, as after `fork`, where it calls only `_exit`, which takes no lock that another of
    // this process's threads could hold. No flag asks for an exit signal.
    let child = unsafe { nix::libc::syscall(nix::libc::SYS_clone, none, none, none, none, none) };
    match child {
        0 => exit_immediately(status),
        child if child < 0 => Err(Errno::last()),
        child => {
            let child = nix::libc::pid_t::try_from(child).map_err(|_| Errno::EOVERFLOW)?;
            Ok(ProcessId::from_pid(Pid::from_raw(child)))
        }
    }
}

/// Forks a child that makes the calling thread its tracer, stops with `SIGSTOP`, and exits with
/// `status` once its tracer continues it, or with 99 if it cannot be traced.
///
/// libc's `ptrace` is used because the crate does not enable `nix`'s.
pub(crate) fn spawn_traced_child(status: u8) -> ProcessId {
    // SAFETY: the child makes only the async-signal-safe `ptrace`, `raise`, and `_exit` calls,
    // as a child of a multithreaded process may.
    match unsafe { nix::unistd::fork() }.expect("fork") {
        nix::unistd::ForkResult::Child => {
            // SAFETY: `PTRACE_TRACEME` reads none of the other arguments.
            let traced = unsafe {
                nix::libc::ptrace(
                    nix::libc::PTRACE_TRACEME,
                    0,
                    std::ptr::null_mut::<nix::libc::c_void>(),
                    std::ptr::null_mut::<nix::libc::c_void>(),
                )
            };
            if traced != 0 {
                exit_immediately(99);
            }
            let _ = nix::sys::signal::raise(Signal::SIGSTOP);
            exit_immediately(status)
        }
        nix::unistd::ForkResult::Parent { child } => ProcessId::from_pid(child),
    }
}

/// Continues `process`, which the calling thread traces and which is stopped, without a signal.
pub(crate) fn continue_traced(process: ProcessId) {
    // SAFETY: `PTRACE_CONT` reads the process ID, and a null signal, which delivers none.
    let result = unsafe {
        nix::libc::ptrace(
            nix::libc::PTRACE_CONT,
            process.0.as_raw(),
            std::ptr::null_mut::<nix::libc::c_void>(),
            std::ptr::null_mut::<nix::libc::c_void>(),
        )
    };
    assert_eq!(result, 0, "PTRACE_CONT: {}", Errno::last());
}

/// Makes the system call `number` fail with `EPERM` in the calling thread, and in every process
/// that it starts from now on, through a seccomp filter that allows every other call, for a test
/// that runs in a process of its own, since no filter can be removed. It first sets the thread's
/// `no_new_privs`, without which a filter needs privileges.
pub(crate) fn deny_system_call(number: nix::libc::c_long) {
    use nix::libc;

    // The filter loads the call's number, the first word of the kernel's `seccomp_data`, and
    // returns `EPERM` for `number`, or allows the call.
    let mut filter = [
        filter_statement(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0, 0, 0),
        // A system call number fits 32 bits.
        filter_statement(
            libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
            0,
            1,
            number as u32,
        ),
        filter_statement(
            libc::BPF_RET | libc::BPF_K,
            0,
            0,
            libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
        ),
        filter_statement(libc::BPF_RET | libc::BPF_K, 0, 0, libc::SECCOMP_RET_ALLOW),
    ];
    install_seccomp_filter(&mut filter);
}

/// Ends the calling process with `SIGSYS` at the first of the system calls `numbers` that the
/// calling thread, or a process that it starts from now on, makes, through a seccomp filter that
/// allows every other call, for a test that runs in a process of its own, since no filter can be
/// removed. It first sets the thread's `no_new_privs`, without which a filter needs privileges.
#[cfg(target_arch = "x86_64")]
pub(crate) fn forbid_system_calls(numbers: &[nix::libc::c_long]) {
    use nix::libc;

    let count = u8::try_from(numbers.len()).expect("a filter's jump spans at most 255 statements");
    // The filter loads the call's number, then compares it with each of `numbers`: a match jumps
    // past the other comparisons and the allowance, to the end of the process.
    let mut filter = vec![filter_statement(
        libc::BPF_LD | libc::BPF_W | libc::BPF_ABS,
        0,
        0,
        0,
    )];
    for (compared, &number) in (0..count).zip(numbers) {
        // A system call number fits 32 bits.
        filter.push(filter_statement(
            libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
            count - compared,
            0,
            number as u32,
        ));
    }
    filter.push(filter_statement(
        libc::BPF_RET | libc::BPF_K,
        0,
        0,
        libc::SECCOMP_RET_ALLOW,
    ));
    filter.push(filter_statement(
        libc::BPF_RET | libc::BPF_K,
        0,
        0,
        libc::SECCOMP_RET_KILL_PROCESS,
    ));
    install_seccomp_filter(&mut filter);
}

/// Returns the classic BPF statement of a seccomp filter with the operation `code`, the jumps
/// `if_equal` and `otherwise` of a comparison, and the operand `k`.
fn filter_statement(code: u32, if_equal: u8, otherwise: u8, k: u32) -> nix::libc::sock_filter {
    nix::libc::sock_filter {
        // Every BPF code fits 16 bits.
        code: code as u16,
        jt: if_equal,
        jf: otherwise,
        k,
    }
}

/// Sets the calling thread's `no_new_privs`, then installs `filter` as a seccomp filter of the
/// thread, which passes to the processes that it starts.
///
/// libc's `prctl` is used because `nix` does not wrap seccomp.
fn install_seccomp_filter(filter: &mut [nix::libc::sock_filter]) {
    use nix::libc::{self, c_ulong};

    let program = libc::sock_fprog {
        len: u16::try_from(filter.len()).expect("a filter has fewer than 65,536 statements"),
        filter: filter.as_mut_ptr(),
    };
    // SAFETY: `PR_SET_NO_NEW_PRIVS` reads only its integer arguments.
    let result = unsafe {
        libc::prctl(
            libc::PR_SET_NO_NEW_PRIVS,
            1 as c_ulong,
            0 as c_ulong,
            0 as c_ulong,
            0 as c_ulong,
        )
    };
    assert_eq!(result, 0, "PR_SET_NO_NEW_PRIVS: {}", Errno::last());
    // SAFETY: the program names its statements, which the kernel copies before it returns.
    let result = unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER as c_ulong,
            &program as *const libc::sock_fprog,
            0 as c_ulong,
            0 as c_ulong,
        )
    };
    assert_eq!(result, 0, "PR_SET_SECCOMP: {}", Errno::last());
}

/// Returns whether `process` has reached a waitable terminal state without reaping it.
///
/// libc is used because `nix`'s `waitid` decodes the state into a `WaitStatus` too, which fails
/// for a real-time signal, whereas only the child that `si_pid` names matters here.
pub(crate) fn has_exited(process: ProcessId) -> Result<bool, Errno> {
    // SAFETY: zero is a valid initial representation for `siginfo_t`.
    let mut information = unsafe { std::mem::zeroed::<nix::libc::siginfo_t>() };
    // SAFETY: `information` is writable; `WNOWAIT` leaves the child available to the owner.
    Errno::result(unsafe {
        nix::libc::waitid(
            nix::libc::P_PID,
            process.0.as_raw() as nix::libc::id_t,
            &mut information,
            nix::libc::WEXITED | nix::libc::WNOHANG | nix::libc::WNOWAIT,
        )
    })?;
    // SAFETY: successful `waitid` initialized the Linux `siginfo_t` child-event union.
    Ok(unsafe { information.si_pid() } == process.0.as_raw())
}

impl ProcessId {
    /// Borrows the identifier of a child created by the standard test harness.
    pub(crate) fn from_child(child: &std::process::Child) -> Self {
        Self(Pid::from_raw(child.id() as i32))
    }
}

/// Returns whether `descriptor` is close-on-exec.
fn closes_on_exec(descriptor: impl AsFd) -> bool {
    let flags = fcntl(descriptor, FcntlArg::F_GETFD).unwrap();
    FdFlag::from_bits_retain(flags).contains(FdFlag::FD_CLOEXEC)
}

#[test]
fn an_inherited_descriptor_is_duplicated_and_closes_on_exec() {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"config").unwrap();
    file.rewind().unwrap();
    fcntl(file.as_fd(), FcntlArg::F_SETFD(FdFlag::empty())).unwrap();
    let argument = CString::new(file.as_raw_fd().to_string()).unwrap();
    let duplicate = take_inherited_descriptor(&argument).unwrap();
    assert_ne!(duplicate.argument(), argument);
    assert!(closes_on_exec(duplicate.borrowed()));
    assert!(closes_on_exec(&file));
    let mut bytes = [0u8; 8];
    assert_eq!(duplicate.read(&mut bytes), Ok(6));
    assert_eq!(&bytes[..6], b"config");
    assert_eq!(duplicate.close(), Ok(()));
}

#[test]
fn an_inherited_descriptor_number_is_decimal_and_open() {
    for text in [c"", c"+7", c" 42", c"-1", c"1x", c"2147483648"] {
        assert_eq!(
            take_inherited_descriptor(text).unwrap_err(),
            Errno::EINVAL,
            "{text:?}"
        );
    }
    assert_eq!(
        take_inherited_descriptor(c"2147483647").unwrap_err(),
        Errno::EBADF
    );
}

#[test]
fn a_signal_through_a_pidfd_fails_once_the_status_is_collected() {
    let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
    let process = ProcessId::from_child(&child);
    let pidfd = open_pidfd(process).unwrap();
    while !has_exited(process).unwrap() {
        std::thread::yield_now();
    }
    // The child has ended, but it is unreaped until its status is collected.
    assert_eq!(kill_through_pidfd(pidfd.borrowed(), false), Ok(()));
    assert!(child.wait().unwrap().success());
    assert_eq!(
        kill_through_pidfd(pidfd.borrowed(), false),
        Err(Errno::ESRCH)
    );
}

#[test]
fn a_pidfd_shows_its_child_unreaped_until_its_status_is_collected() {
    use std::os::unix::process::ExitStatusExt;

    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let process = ProcessId::from_child(&child);
    let pidfd = open_pidfd(process).unwrap();
    assert_eq!(is_unreaped(pidfd.borrowed()), Ok(true));
    kill_process(process).unwrap();
    while !has_exited(process).unwrap() {
        std::thread::yield_now();
    }
    // The check leaves the ended child's status for its collector.
    assert_eq!(is_unreaped(pidfd.borrowed()), Ok(true));
    assert_eq!(child.wait().unwrap().signal(), Some(nix::libc::SIGKILL));
    assert_eq!(is_unreaped(pidfd.borrowed()), Ok(false));
}

#[test]
fn a_real_time_signal_goes_through_a_pidfd_and_to_a_process_group() {
    use std::os::unix::process::{CommandExt, ExitStatusExt};

    // `nix`'s `Signal` cannot represent signal 40, whose default action ends the process.
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pidfd = open_pidfd(ProcessId::from_child(&child)).unwrap();
    // The kernel refuses a number that names no signal, and sends nothing.
    assert_eq!(
        signal_through_pidfd(pidfd.borrowed(), 65, false),
        Err(Errno::EINVAL)
    );
    assert_eq!(signal_through_pidfd(pidfd.borrowed(), 40, false), Ok(()));
    assert_eq!(child.wait().unwrap().signal(), Some(40));
    let mut leader = std::process::Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let process = ProcessId::from_child(&leader);
    assert_eq!(process_group_of(process), Ok(process));
    assert_eq!(signal_process_group(process, 65), Err(Errno::EINVAL));
    assert_eq!(signal_process_group(process, 41), Ok(()));
    assert_eq!(leader.wait().unwrap().signal(), Some(41));
}

#[test]
fn a_group_signal_through_a_pidfd_reaches_the_group_once_its_leader_is_collected() {
    use std::io::BufRead;
    use std::os::unix::process::CommandExt;

    // The leader leaves a member in its process group, and tells its ID.
    let mut leader = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 30 </dev/null >/dev/null 2>&1 & echo $!"])
        .process_group(0)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pidfd = open_pidfd(ProcessId::from_child(&leader)).unwrap();
    let mut line = String::new();
    std::io::BufReader::new(leader.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let member = ProcessId::from_positive(line.trim().parse().unwrap()).unwrap();
    let member_pidfd = open_pidfd(member).unwrap();
    assert!(leader.wait().unwrap().success());
    match kill_through_pidfd(pidfd.borrowed(), true) {
        // A kernel before Linux 6.9 cannot signal a group through a pidfd.
        Err(Errno::EINVAL) => kill_process(member).unwrap(),
        signalled => signalled.unwrap(),
    }
    assert_eq!(
        crate::guest::sys::readable_or_hung_up(&[member_pidfd.borrowed()], 20_000).unwrap(),
        [true]
    );
}

#[test]
fn an_id_that_no_process_has_opens_no_pidfd_on_any_kernel() {
    use std::io::BufRead;
    use std::os::unix::process::CommandExt;

    // The leader leaves a member in its process group, which keeps the leader's ID once its
    // status is collected, and tells its ID.
    let mut leader = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 30 </dev/null >/dev/null 2>&1 & echo $!"])
        .process_group(0)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let reaped = ProcessId::from_child(&leader);
    let mut line = String::new();
    std::io::BufReader::new(leader.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let member = ProcessId::from_positive(line.trim().parse().unwrap()).unwrap();
    assert!(leader.wait().unwrap().success());
    // A thread of this process, which leads no process, has an ID of its own.
    let (sender, receiver) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        sender
            .send(ProcessId::from_pid(nix::unistd::gettid()))
            .unwrap();
        let _ = released.recv();
    });
    let other_thread = receiver.recv().unwrap();
    // Whatever this kernel answers, and as a kernel before Linux 6.16 answers, `EINVAL` for
    // both IDs, where a later one answers `ESRCH` and `ENOENT`.
    let mut answers = Vec::new();
    for before_linux_6_16 in [false, true] {
        BEFORE_LINUX_6_16.set(before_linux_6_16);
        answers.push([open_pidfd(reaped).err(), open_pidfd(other_thread).err()]);
    }
    BEFORE_LINUX_6_16.set(false);
    drop(release);
    thread.join().unwrap();
    kill_process(member).unwrap();
    assert_eq!(answers, [[Some(Errno::ESRCH); 2]; 2]);
}
