use std::fs;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::path::Path;

use nix::fcntl::{FcntlArg, FdFlag, fcntl};

use super::*;
use crate::host::sys;

/// Kills and reaps a child process when dropped, even when a test fails.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Returns the object that the descriptor `descriptor` of process `pid` holds, as the target
/// of its link in `/proc`, or `None` if the process has no such descriptor.
fn object(pid: u32, descriptor: RawFd) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/fd/{descriptor}")).ok()
}

/// Returns whether process `pid` holds a descriptor of `object`, under any number.
fn holds(pid: u32, object: &Path) -> bool {
    fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .any(|entry| fs::read_link(entry.unwrap().path()).is_ok_and(|target| target == object))
}

/// Returns whether the descriptor `descriptor` of this process is close-on-exec.
fn close_on_exec(descriptor: BorrowedFd<'_>) -> bool {
    FdFlag::from_bits_retain(fcntl(descriptor, FcntlArg::F_GETFD).unwrap())
        .contains(FdFlag::FD_CLOEXEC)
}

/// Spawns a long-running child with `spawn` while this process holds an inheritable pipe
/// writer, as a descriptor that the caller's parent or native code left without close-on-exec
/// would be. Returns whether the child holds a copy, as its own descriptor table shows. Unlike
/// the pipe reaching end-of-file, that cannot depend on the children of concurrent tests,
/// which may inherit the writer too.
fn child_inherits_writer(spawn: impl FnOnce() -> Child) -> bool {
    let (pipe, child) = {
        let (_, writer) = io::pipe().unwrap();
        sys::tests::clear_close_on_exec(&writer).unwrap();
        let pipe = object(std::process::id(), writer.as_raw_fd()).unwrap();
        (pipe, Reaped(spawn()))
    };
    // `spawn` returns once the child has executed its program, which closed the child's
    // close-on-exec copies, so a copy that remains was inherited.
    holds(child.0.id(), &pipe)
}

/// Returns a command that runs for a minute unless it is killed.
fn sleeper() -> Command {
    let mut command = Command::new("sleep");
    command.arg("60").stdin(Stdio::null());
    command
}

/// Starts `sleep 60` as a VMM in `directory`, which also inherits `inherit`.
fn spawn_sleeper(directory: &Path, inherit: Vec<Inherited<'_>>) -> io::Result<Launched> {
    let log = File::create(directory.join("vmm.log"))?;
    let mut command = VmmCommand::new("sleep", vec![OsString::from("60")], directory);
    command.inherit = inherit;
    spawn(&command, &[1; CAPABILITY_LEN], log)
}

#[test]
fn vmms_inherit_no_descriptor_beyond_their_standard_streams() {
    // A plain child keeps the writer, which shows that the check detects a leak.
    let plain = || sleeper().spawn().unwrap();
    assert!(child_inherits_writer(plain));

    let directory = tempfile::tempdir().unwrap();
    let vmm = || spawn_sleeper(directory.path(), Vec::new()).unwrap().child;
    assert!(!child_inherits_writer(vmm));

    // Kernels without CLOSE_RANGE_CLOEXEC take the /proc/self/fd fallback.
    let fallback = || {
        let mut command = sleeper();
        sys::tests::mark_listed_descriptors_on_spawn(&mut command);
        command.spawn().unwrap()
    };
    assert!(!child_inherits_writer(fallback));
}

#[test]
fn vmms_inherit_the_allowed_descriptors_under_their_numbers() {
    let directory = tempfile::tempdir().unwrap();
    let (_reader, pipe) = io::pipe().unwrap();
    let file = File::create(directory.path().join("distro")).unwrap();
    // A descriptor that the list names twice is inherited once.
    let inherit = vec![
        Inherited::new(&pipe),
        Inherited::new(&file),
        Inherited::new(&pipe),
    ];
    let vmm = Reaped(spawn_sleeper(directory.path(), inherit).unwrap().child);
    let (pid, this) = (vmm.0.id(), std::process::id());
    for descriptor in [pipe.as_raw_fd(), file.as_raw_fd()] {
        let inherited = object(pid, descriptor);
        assert!(
            inherited.is_some(),
            "descriptor {descriptor} was not inherited"
        );
        assert_eq!(inherited, object(this, descriptor));
    }
    // Only the VMM's process cleared their close-on-exec flags.
    assert!(close_on_exec(pipe.as_fd()));
    assert!(close_on_exec(file.as_fd()));
    assert_eq!(
        Inherited::new(&file).proc_path(),
        PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
    );
}

#[test]
fn standard_streams_cannot_be_inherited() {
    let directory = tempfile::tempdir().unwrap();
    for stream in [
        io::stdin().as_fd(),
        io::stdout().as_fd(),
        io::stderr().as_fd(),
    ] {
        let error = spawn_sleeper(directory.path(), vec![Inherited::from(stream)]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}

#[test]
fn descriptors_that_are_not_close_on_exec_are_refused() {
    let directory = tempfile::tempdir().unwrap();
    // As a descriptor that the caller's parent or native code left without close-on-exec,
    // which any process that the caller spawns would inherit, and not only the VMM.
    let (_reader, writer) = io::pipe().unwrap();
    sys::tests::clear_close_on_exec(&writer).unwrap();
    let error = spawn_sleeper(directory.path(), vec![Inherited::new(&writer)]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("close-on-exec"), "{error}");
    // The refusal, which comes before the spawn, leaves the descriptor as it was.
    assert!(!close_on_exec(writer.as_fd()));
}

/// Returns the umask that the status file of a process or a thread at `status`, in procfs,
/// tells.
fn umask_in(status: &str) -> u32 {
    fs::read_to_string(status)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("Umask:"))
        .and_then(|umask| u32::from_str_radix(umask.trim(), 8).ok())
        .unwrap()
}

/// Asserts that the process that `spawn` starts in a directory of its own, from a thread whose
/// umask no other thread shares, has the thread's umask without the owner's bits, and that
/// the thread keeps its own, for the usual umasks, which keep the owner's bits, and for umasks
/// that clear some of them.
fn assert_spawns_clear_the_owners_bits(spawn: impl Fn(&Path) -> Child + Sync) {
    use nix::sys::stat::{Mode, umask};

    for mask in [
        0o002, 0o022, 0o027, 0o077, 0o100, 0o200, 0o400, 0o477, 0o777,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let (child, caller) = thread::scope(|scope| {
            scope
                .spawn(|| {
                    nix::sched::unshare(nix::sched::CloneFlags::CLONE_FS).unwrap();
                    umask(Mode::from_bits_truncate(mask));
                    let child = Reaped(spawn(directory.path()));
                    (child, umask_in("/proc/thread-self/status"))
                })
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        });
        assert_eq!(
            caller, mask,
            "the caller's umask {mask:03o} became {caller:03o}"
        );
        let pid = child.0.id();
        let inherited = umask_in(&format!("/proc/{pid}/status"));
        assert_eq!(
            inherited,
            mask & 0o077,
            "the caller's umask {mask:03o} gave the process {inherited:03o}"
        );
    }
}

#[test]
fn vmms_start_with_the_callers_umask_without_the_owners_bits() {
    assert_spawns_clear_the_owners_bits(|directory| {
        spawn_sleeper(directory, Vec::new()).unwrap().child
    });
}

/// The one-shot runs of the CLI start OpenVMM in the caller's session, as `inherit_only` makes
/// a command start, with the umask that a detached VMM starts with.
#[cfg(feature = "cli")]
#[test]
fn one_shot_vmms_start_with_the_callers_umask_without_the_owners_bits() {
    assert_spawns_clear_the_owners_bits(|_| {
        let mut command = sleeper();
        platform::inherit_only(&mut command, &[]).unwrap();
        command.spawn().unwrap()
    });
}

#[test]
fn spawns_fail_with_the_error_of_a_refused_umask() {
    // As under a seccomp policy that refuses `umask`: the spawn fails with the call's error,
    // rather than return a process that ended before it could run its program. The filter
    // refuses the call with `EDOM`, which no other step of the spawn returns, so a failure to
    // install the filter cannot pass for the refusal.
    let mut command = sleeper();
    sys::tests::refuse_umask_on_spawn(&mut command, nix::libc::EDOM);
    platform::detach(&mut command, &[]).unwrap();
    let error = command
        .spawn()
        .map(Reaped)
        .err()
        .expect("the spawn succeeded");
    assert_eq!(error.raw_os_error(), Some(nix::libc::EDOM), "{error}");
}
