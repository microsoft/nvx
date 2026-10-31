use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::mem;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsHandle;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CompareObjectHandles, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_ACCESS_DENIED,
    ERROR_INVALID_PARAMETER, ERROR_SHARING_VIOLATION, GetHandleInformation, HANDLE_FLAG_INHERIT,
    SetHandleInformation,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::JobObjects::CreateJobObjectW;
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, INFINITE, TerminateProcess,
};

use super::identity::status_error;
use super::log_claim::{local_directory, log_job_id, log_job_name, log_job_processes, object_name};
use super::process::{end_process, wait_step};
use super::spawn::command_line;
use super::*;
use crate::host::platform::inherit::{Inherited, lock_spawns, spawn_command};

thread_local! {
    /// What a spawn of this thread runs in its window, once its handles are inheritable and
    /// before it creates its process.
    static IN_WINDOW: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Runs what the test of this thread asked its spawns to run in their window, if anything.
pub(super) fn in_window() {
    IN_WINDOW.with_borrow_mut(|hook| {
        if let Some(hook) = hook {
            hook();
        }
    });
}

fn line(program: &str, arguments: &[&str]) -> String {
    let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
    let mut units = command_line(OsStr::new(program), &arguments).unwrap();
    assert_eq!(units.pop(), Some(0));
    String::from_utf16(&units).unwrap()
}

#[test]
fn a_process_that_is_exiting_counts_as_killed_before_its_exit_shows() {
    let denied = || Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32));
    let invalid = || Err(io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER as i32));
    // Windows refused to terminate a process whose exit began, which the handle does not
    // show yet.
    end_process(true, 7, denied, || Ok(false)).unwrap();
    // Other failures stand until the exit shows.
    let failure = end_process(true, 7, invalid, || Ok(false)).unwrap_err();
    assert_eq!(failure.raw_os_error(), Some(ERROR_INVALID_PARAMETER as i32));
    end_process(true, 7, invalid, || Ok(true)).unwrap();
    // Without terminate access, nothing is terminated, and only a shown exit succeeds.
    let refused = end_process(false, 7, || panic!("terminated"), || Ok(false)).unwrap_err();
    assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
    end_process(false, 7, || panic!("terminated"), || Ok(true)).unwrap();
}

#[test]
fn a_restore_ready_client_that_closed_before_its_connection_was_accepted_is_read_to_its_end() {
    let pid = std::process::id();
    let receive_all = |listener: &mut ReadyListener| {
        let mut received = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while listener
            .receive(pid, &mut received, 64, Duration::from_millis(50))
            .unwrap()
        {
            assert!(Instant::now() < deadline, "the endpoint did not close");
        }
        received
    };
    // A client that sent nothing, as a VMM that fails as it starts, and one that sent its
    // event, each closed before the listener accepted their connection: what they sent
    // remains to read, and then the end of the pipe.
    for sent in [&b""[..], b"the event\n"] {
        let (endpoint, mut listener) = listen_ready(Path::new("unused")).unwrap();
        let mut client = fs::OpenOptions::new().write(true).open(&endpoint).unwrap();
        client.write_all(sent).unwrap();
        drop(client);
        assert_eq!(receive_all(&mut listener), sent);
    }
    // A client that closes once the listener accepted its connection.
    let (endpoint, mut listener) = listen_ready(Path::new("unused")).unwrap();
    let client = fs::OpenOptions::new().write(true).open(&endpoint).unwrap();
    let mut received = Vec::new();
    assert!(
        listener
            .receive(pid, &mut received, 64, Duration::from_millis(50))
            .unwrap()
    );
    drop(client);
    assert_eq!(receive_all(&mut listener), b"");
}

#[test]
fn windows_refuses_to_terminate_a_process_that_exited_with_access_denied() {
    let mut child = spawn_command(&mut sleeper()).unwrap();
    let pid = child.id();
    let start_time = process_start_time(pid).unwrap().unwrap();
    let process = open_process(pid, start_time).unwrap().unwrap();
    assert!(process.terminate);
    child.kill().unwrap();
    child.wait().unwrap();
    // SAFETY: the handle has terminate access.
    let terminated = unsafe { TerminateProcess(raw(&process.process), 1) } != 0;
    let error = io::Error::last_os_error();
    assert!(!terminated);
    assert_eq!(error.raw_os_error(), Some(ERROR_ACCESS_DENIED as i32));
    process.kill(pid).unwrap();
}

#[test]
fn a_wait_sleeps_a_remainder_below_a_millisecond_instead_of_waiting_a_tick() {
    assert_eq!(wait_step(Duration::ZERO), (0, Duration::ZERO));
    for remainder in [Duration::from_micros(300), Duration::from_micros(999)] {
        assert_eq!(wait_step(remainder), (0, remainder));
    }
    assert_eq!(wait_step(Duration::from_micros(1_999)), (1, Duration::ZERO));
    let infinite = Duration::from_millis(u64::from(INFINITE));
    assert_eq!(wait_step(infinite), (INFINITE - 1, Duration::ZERO));
    assert_eq!(wait_step(Duration::MAX), (INFINITE - 1, Duration::ZERO));
}

#[test]
fn a_wait_with_less_than_a_millisecond_left_ends_at_its_deadline() {
    let mut child = spawn_command(&mut sleeper()).unwrap();
    let pid = child.id();
    let start_time = process_start_time(pid).unwrap().unwrap();
    let process = open_process(pid, start_time).unwrap().unwrap();
    let started = Instant::now();
    let mut waits = Vec::new();
    for _ in 0..20 {
        let deadline = Instant::now() + Duration::from_micros(300);
        waits.push((process.wait(deadline), Instant::now() >= deadline));
    }
    let elapsed = started.elapsed();
    child.kill().unwrap();
    child.wait().unwrap();
    for (wait, ended_after_deadline) in waits {
        assert_eq!(wait.unwrap(), Some(false));
        assert!(ended_after_deadline);
    }
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    assert!(process.wait(Instant::now()).unwrap().unwrap());
}

#[test]
fn command_lines_follow_the_c_runtime_rules() {
    assert_eq!(
        line(r"C:\Program Files\openvmm.exe", &["--a", "b c", ""]),
        r#""C:\Program Files\openvmm.exe" --a "b c" """#
    );
    assert_eq!(line("x", &[r#"say "hi""#]), r#""x" "say \"hi\"""#);
    assert_eq!(line("x", &[r#"a"b"#]), r#""x" a\"b"#);
    assert_eq!(line("x", &[r"dir\ with\"]), r#""x" "dir\ with\\""#);
    assert_eq!(line("x", &[r"a\\b"]), r#""x" a\\b"#);
    assert!(command_line(OsStr::new("x"), &[OsString::from("a\0b")]).is_err());
}

#[test]
fn children_inherit_only_their_standard_handles() {
    // An inheritable pipe stands in for a caller's own redirected standard output.
    let (mut leaked_reader, leaked_writer) = io::pipe().unwrap();
    let leaked = OwnedHandle::from(leaked_writer);
    // SAFETY: the handle is open and owned above.
    assert_ne!(
        unsafe { SetHandleInformation(raw(&leaked), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) },
        0
    );
    let (stdin, _stdin_writer) = io::pipe().unwrap();
    let (_stdout_reader, stdout) = io::pipe().unwrap();
    let stderr = stdout.try_clone().unwrap();
    let system = std::env::var_os("SystemRoot").unwrap();
    let cmd = Path::new(&system).join("System32").join("cmd.exe");
    let child = spawn_detached(
        &cmd,
        &[OsString::from("/c"), OsString::from("ping -n 30 127.0.0.1")],
        &std::env::temp_dir(),
        [stdin.into(), stdout.into(), stderr.into()],
        &[],
        false,
        None,
    )
    .unwrap();

    // The running child holds no copy of the leaked pipe, so closing ours ends it.
    drop(leaked);
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut rest = Vec::new();
        let _ = sender.send(leaked_reader.read_to_end(&mut rest).map(|_| rest));
    });
    let rest = receiver.recv_timeout(Duration::from_secs(5));
    assert!(child.terminate(Duration::from_secs(10)));
    assert!(
        rest.expect("the child inherited the caller's pipe")
            .unwrap()
            .is_empty()
    );
}

/// Returns whether the handle `value` of `process` refers to the object that `ours` holds, as
/// a handle that the process inherited from this one does, under the same value.
fn holds(process: &OwnedHandle, value: HANDLE, ours: &impl AsRawHandle) -> bool {
    let mut copy = ptr::null_mut();
    // SAFETY: the process handle has full access, the value is only looked up in that
    // process, and the output pointer is valid.
    let duplicated = unsafe {
        DuplicateHandle(
            raw(process),
            value,
            GetCurrentProcess(),
            &mut copy,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } != 0;
    // A value that names no handle of the process names none of ours.
    let Some(copy) = duplicated.then(|| owned(copy)).flatten() else {
        return false;
    };
    // SAFETY: both handles are open.
    unsafe { CompareObjectHandles(raw(&copy), ours.as_raw_handle() as HANDLE) != 0 }
}

/// Returns whether `handle` is inheritable.
fn inheritable(handle: &impl AsRawHandle) -> bool {
    let mut flags = 0;
    // SAFETY: the handle is open, and the output pointer is valid.
    let read = unsafe { GetHandleInformation(handle.as_raw_handle() as HANDLE, &mut flags) };
    assert_ne!(read, 0, "{}", io::Error::last_os_error());
    flags & HANDLE_FLAG_INHERIT != 0
}

/// Makes `handle` inheritable, as native code or a parent may have left it.
fn make_inheritable(handle: &impl AsRawHandle) {
    // SAFETY: the handle is open.
    let set = unsafe {
        SetHandleInformation(
            handle.as_raw_handle() as HANDLE,
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        )
    };
    assert_ne!(set, 0, "{}", io::Error::last_os_error());
}

#[test]
fn a_child_loses_only_the_inherited_handle_that_is_closed_in_it() {
    let directory = tempfile::tempdir().unwrap();
    let closed = std::fs::File::create(directory.path().join("closed")).unwrap();
    let kept = std::fs::File::create(directory.path().join("kept")).unwrap();
    let (_other_reader, other) = io::pipe().unwrap();
    let (stdin, _stdin_writer) = io::pipe().unwrap();
    let (_stdout_reader, stdout) = io::pipe().unwrap();
    let stderr = stdout.try_clone().unwrap();
    let system = std::env::var_os("SystemRoot").unwrap();
    let ping = Path::new(&system).join("System32").join("PING.EXE");
    let child = spawn_detached(
        &ping,
        &["-n", "30", "127.0.0.1"].map(OsString::from),
        &std::env::temp_dir(),
        [stdin.into(), stdout.into(), stderr.into()],
        &[Inherited::new(&closed), Inherited::new(&kept)],
        false,
        None,
    )
    .unwrap();
    let pid = child.id();
    let (process, start_time) = child.pin().unwrap();
    let value = |handle: &std::fs::File| handle.as_raw_handle() as HANDLE;
    let held = |handle: &std::fs::File| holds(&child.process, value(handle), handle);
    let inherited = (held(&closed), held(&kept));
    // A handle that the child did not inherit is not closed in it, and neither is any of its
    // own.
    let refused = process.close_inherited(pid, other.as_handle());
    // A process found again, through a handle that may not duplicate its handles, keeps them.
    let found = open_process(pid, start_time).unwrap().unwrap();
    let denied = found.close_inherited(pid, closed.as_handle());
    let closing = process.close_inherited(pid, closed.as_handle());
    let after = (held(&closed), held(&kept));
    assert!(child.terminate(Duration::from_secs(10)));
    // Once the child has exited, it holds no handle, so a close succeeds.
    let exited = process.close_inherited(pid, kept.as_handle());
    assert_eq!(inherited, (true, true), "the child did not inherit both");
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(denied.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    closing.unwrap();
    assert_eq!(
        after,
        (false, true),
        "the close did not take the closed handle alone"
    );
    exited.unwrap();
}

#[test]
fn children_inherit_the_allowed_handles_while_they_are_created() {
    let (_allowed_reader, allowed) = io::pipe().unwrap();
    // A handle that was inheritable before the spawn stays so.
    let (_kept_reader, kept) = io::pipe().unwrap();
    make_inheritable(&kept);
    let (_other_reader, other) = io::pipe().unwrap();
    make_inheritable(&other);
    assert!(!inheritable(&allowed));
    let (stdin, _stdin_writer) = io::pipe().unwrap();
    let (_stdout_reader, stdout) = io::pipe().unwrap();
    let stderr = stdout.try_clone().unwrap();
    let system = std::env::var_os("SystemRoot").unwrap();
    let ping = Path::new(&system).join("System32").join("PING.EXE");
    // A handle that the list names twice is inherited once.
    let inherit = [
        Inherited::new(&allowed),
        Inherited::new(&kept),
        Inherited::new(&allowed),
    ];
    let child = spawn_detached(
        &ping,
        &["-n", "30", "127.0.0.1"].map(OsString::from),
        &std::env::temp_dir(),
        [stdin.into(), stdout.into(), stderr.into()],
        &inherit,
        false,
        None,
    )
    .unwrap();
    let value = |handle: &io::PipeWriter| handle.as_raw_handle() as HANDLE;
    let (allowed_held, kept_held, other_held) = (
        holds(&child.process, value(&allowed), &allowed),
        holds(&child.process, value(&kept), &kept),
        holds(&child.process, value(&other), &other),
    );
    let (allowed_inheritable, kept_inheritable) = (inheritable(&allowed), inheritable(&kept));
    assert!(child.terminate(Duration::from_secs(10)));
    assert!(
        allowed_held && kept_held,
        "an allowed handle was not inherited"
    );
    assert!(!other_held, "a handle that was not allowed was inherited");
    assert!(!allowed_inheritable && kept_inheritable);
}

#[cfg(feature = "cli")]
#[test]
fn foreground_children_inherit_only_the_allowed_handles_besides_their_standard_ones() {
    // A handle that this process leaves inheritable, as its parent may have passed it, and
    // that `std::process::Command` would pass on.
    let (_other_reader, other) = io::pipe().unwrap();
    make_inheritable(&other);
    let (_allowed_reader, allowed) = io::pipe().unwrap();
    let system = std::env::var_os("SystemRoot").unwrap();
    let cmd = Path::new(&system).join("System32").join("cmd.exe");
    // The child shares this process's console and standard handles, so it writes nothing.
    let child = spawn_foreground(
        &cmd,
        &["/c", "ping -n 30 127.0.0.1 >NUL"].map(OsString::from),
        &[Inherited::new(&allowed)],
    )
    .unwrap();
    let value = |handle: &io::PipeWriter| handle.as_raw_handle() as HANDLE;
    let (allowed_held, other_held) = (
        holds(&child.process, value(&allowed), &allowed),
        holds(&child.process, value(&other), &other),
    );
    let allowed_inheritable = inheritable(&allowed);
    assert!(child.terminate(Duration::from_secs(10)));
    assert!(allowed_held, "an allowed handle was not inherited");
    assert!(!other_held, "a handle that was not allowed was inherited");
    assert!(
        !allowed_inheritable,
        "the spawn left the allowed handle inheritable"
    );
}

#[test]
fn batch_files_are_refused_before_cmd_could_read_their_arguments() {
    // Batch files that would leave a marker if they ran, with an argument that cmd.exe would
    // read as a second command.
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("ran");
    let injected = directory.path().join("injected");
    let arguments = [OsString::from(format!("x&echo>{}", injected.display()))];
    // A file named only `.cmd` is a batch file too, and Windows drops the trailing dot of a
    // name, so the last path names `trailing.cmd`.
    for (name, program) in [
        ("plain.cmd", "plain.cmd"),
        ("SHOUT.BAT", "SHOUT.BAT"),
        (".cmd", ".cmd"),
        ("trailing.cmd", "trailing.cmd."),
    ] {
        let script = format!("@echo ran>\"{}\"\r\n", marker.display());
        std::fs::write(directory.path().join(name), script).unwrap();
        let program = directory.path().join(program);
        let refused = |result: io::Result<LaunchedProcess>| {
            let error = result.err().expect("a batch file started");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
        };
        #[cfg(feature = "cli")]
        refused(spawn_foreground(&program, &arguments, &[]));
        let (stdin, _stdin_writer) = io::pipe().unwrap();
        let (_stdout_reader, stdout) = io::pipe().unwrap();
        let stderr = stdout.try_clone().unwrap();
        refused(spawn_detached(
            &program,
            &arguments,
            directory.path(),
            [stdin.into(), stdout.into(), stderr.into()],
            &[],
            false,
            None,
        ));
    }
    std::thread::sleep(Duration::from_millis(500));
    assert!(!marker.exists() && !injected.exists());
}

#[test]
fn concurrent_spawns_that_allow_one_handle_all_inherit_it_and_restore_it() {
    /// Threads that spawn at once.
    const THREADS: usize = 4;
    /// Spawns of each thread.
    const SPAWNS: usize = 16;
    let (_shared_reader, shared) = io::pipe().unwrap();
    assert!(!inheritable(&shared));
    let system = std::env::var_os("SystemRoot").unwrap();
    let ping = Path::new(&system).join("System32").join("PING.EXE");
    // Each spawn reads the handle's inheritability while others may have made it inheritable
    // for their own spawns, and must neither take that for the handle's own nor find the
    // handle no longer inheritable when it creates its process.
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| {
                let value = shared.as_raw_handle() as HANDLE;
                for _ in 0..SPAWNS {
                    let (stdin, _stdin_writer) = io::pipe().unwrap();
                    let (_stdout_reader, stdout) = io::pipe().unwrap();
                    let stderr = stdout.try_clone().unwrap();
                    let child = spawn_detached(
                        &ping,
                        &["-n", "30", "127.0.0.1"].map(OsString::from),
                        &std::env::temp_dir(),
                        [stdin.into(), stdout.into(), stderr.into()],
                        &[Inherited::new(&shared)],
                        false,
                        None,
                    )
                    .unwrap();
                    let held = holds(&child.process, value, &shared);
                    assert!(child.terminate(Duration::from_secs(10)));
                    assert!(held, "a spawn did not inherit the allowed handle");
                }
            });
        }
    });
    assert!(
        !inheritable(&shared),
        "the spawns left the handle inheritable"
    );
}

#[test]
fn a_spawn_that_panics_in_its_window_restores_the_handles_and_lets_other_spawns_run() {
    /// Spawns a stand-in VMM, `cmd /c exit 0`, that is allowed the handles `inherit`.
    fn stand_in(inherit: &[Inherited<'_>]) -> io::Result<LaunchedProcess> {
        let system = std::env::var_os("SystemRoot").unwrap();
        let cmd = Path::new(&system).join("System32").join("cmd.exe");
        let (stdin, _stdin_writer) = io::pipe()?;
        let (_stdout_reader, stdout) = io::pipe()?;
        let stderr = stdout.try_clone()?;
        spawn_detached(
            &cmd,
            &["/c", "exit", "0"].map(OsString::from),
            &std::env::temp_dir(),
            [stdin.into(), stdout.into(), stderr.into()],
            inherit,
            false,
            None,
        )
    }

    let (_shared_reader, shared) = io::pipe().unwrap();
    // The panic unwinds out of the window with the handles inheritable and the lock held,
    // which it poisons, and this thread lives on.
    IN_WINDOW.set(Some(Box::new(|| panic!("a panic in the window"))));
    let panicked = std::panic::catch_unwind(|| stand_in(&[Inherited::new(&shared)])).is_err();
    IN_WINDOW.set(None);
    assert!(panicked, "the spawn did not panic in its window");
    assert!(
        !inheritable(&shared),
        "the spawn left the handle inheritable"
    );
    // Another thread's spawn takes the lock all the same.
    let (sender, spawned) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(stand_in(&[]));
    });
    let other = spawned
        .recv_timeout(WINDOW_TIMEOUT)
        .expect("a spawn after the panic did not take the lock")
        .unwrap();
    assert!(other.terminate(WINDOW_TIMEOUT), "the stand-in did not exit");
}

/// The longest wait of the tests of a spawn's window.
const WINDOW_TIMEOUT: Duration = Duration::from_secs(30);

/// A process that a test spawned, which is killed and waited for when it is dropped, also
/// when the test fails.
struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Returns a command that runs for a minute, without standard streams.
fn sleeper() -> std::process::Command {
    let mut command = std::process::Command::new("ping");
    command
        .args(["-n", "60", "127.0.0.1"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}

/// A spawn of a stand-in VMM, with a log as its standard output and error, as
/// `supervisor::spawn` passes the VMM its launch's log, that another thread runs and holds in
/// its window, with the log's handles inheritable, until it is released.
struct HeldSpawn {
    /// The log.
    log: std::path::PathBuf,
    /// The log's identity, as its claim is checked.
    identity: (u64, u64),
    /// Releases the spawn from its window when it sends, or when it is dropped.
    release: std::sync::mpsc::Sender<()>,
    /// The thread of the spawn, which returns the stand-in.
    spawn: thread::JoinHandle<io::Result<LaunchedProcess>>,
}

impl HeldSpawn {
    /// Starts the spawn of `cmd /c exit 0`, outside any job, with its log in `directory`, and
    /// returns once it is in its window.
    fn start(directory: &Path) -> Self {
        let system = std::env::var_os("SystemRoot").unwrap();
        let cmd = Path::new(&system).join("System32").join("cmd.exe");
        Self::start_with(directory, cmd, &["/c", "exit", "0"], false)
    }

    /// Starts the spawn of a stand-in that runs for a minute, `ping`, in the job of its log,
    /// as `supervisor::spawn` starts the VMM, with its log in `directory`, and returns once it
    /// is in its window.
    fn start_in_job(directory: &Path) -> Self {
        let system = std::env::var_os("SystemRoot").unwrap();
        let ping = Path::new(&system).join("System32").join("PING.EXE");
        Self::start_with(directory, ping, &["-n", "60", "127.0.0.1"], true)
    }

    /// Starts the spawn of `program` with `arguments`, in the job of its log if `in_job`, with
    /// its log in `directory`, and returns once it is in its window.
    fn start_with(
        directory: &Path,
        program: std::path::PathBuf,
        arguments: &[&str],
        in_job: bool,
    ) -> Self {
        let log = directory.join("vmm.log");
        let file = fs::File::create(&log).unwrap();
        let identity = file_identity(&log).unwrap();
        let job = in_job.then(|| {
            create_log_job(&file)
                .unwrap()
                .expect("a log on a disk volume has a job")
        });
        let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
        let (entered, in_window) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let spawn = thread::spawn(move || {
            IN_WINDOW.set(Some(Box::new(move || {
                let _ = entered.send(());
                // A test that fails drops its sender, which ends the wait.
                let _ = released.recv_timeout(WINDOW_TIMEOUT);
            })));
            let (stdin, _stdin_writer) = io::pipe()?;
            let stderr = file.try_clone()?;
            spawn_detached(
                &program,
                &arguments,
                &std::env::temp_dir(),
                [stdin.into(), file.into(), stderr.into()],
                &[],
                false,
                job,
            )
        });
        in_window
            .recv_timeout(WINDOW_TIMEOUT)
            .expect("the spawn did not reach its window");
        Self {
            log,
            identity,
            release,
            spawn,
        }
    }

    /// Releases the spawn from its window, and returns the stand-in once it is created, with
    /// the log and its identity.
    fn stand_in(self) -> (LaunchedProcess, std::path::PathBuf, (u64, u64)) {
        let _ = self.release.send(());
        let stand_in = self.spawn.join().unwrap().unwrap();
        (stand_in, self.log, self.identity)
    }

    /// Releases the spawn from its window, and returns once the stand-in has exited, with the
    /// log and its identity.
    fn finish(self) -> (std::path::PathBuf, (u64, u64)) {
        let (stand_in, log, identity) = self.stand_in();
        assert!(
            stand_in.terminate(WINDOW_TIMEOUT),
            "the stand-in did not exit"
        );
        (log, identity)
    }
}

/// Returns whether the claim on the log `log`, the file `identity`, is released within
/// `timeout`.
fn claim_released_within(log: &Path, identity: (u64, u64), timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !log_claim_held(log, identity).unwrap() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn a_process_spawned_under_the_spawn_lock_does_not_inherit_a_vmms_log() {
    let directory = tempfile::tempdir().unwrap();
    let held = HeldSpawn::start(directory.path());
    // The spawn of the other process waits for the window to close.
    let other = thread::spawn(|| spawn_command(&mut sleeper()).map(Running));
    thread::sleep(Duration::from_millis(500));
    let waited = !other.is_finished();
    let (log, identity) = held.finish();
    let mut other = other.join().unwrap().unwrap();
    // The log's claim is released once the stand-in has exited, while the other process
    // runs.
    let released = claim_released_within(&log, identity, WINDOW_TIMEOUT);
    let running = other.0.try_wait().unwrap().is_none();
    drop(other);
    assert!(
        waited,
        "a spawn under the spawn lock did not wait for the window"
    );
    assert!(
        released,
        "a process spawned under the spawn lock held the log"
    );
    assert!(
        running,
        "the process spawned under the spawn lock exited early"
    );
}

#[test]
fn a_process_spawned_without_the_spawn_lock_in_a_window_holds_the_vmms_log() {
    let directory = tempfile::tempdir().unwrap();
    let held = HeldSpawn::start(directory.path());
    // `std::process::Command` passes the process every handle of this one that is
    // inheritable, the log's among them in the window.
    let other = sleeper().spawn().map(Running);
    let (log, identity) = held.finish();
    let mut other = other.unwrap();
    // The stand-in ran outside a job, so the log's handles are its claim, which the process
    // holds after the stand-in has exited, for longer than a recovery checks a held claim
    // again, as long as it runs.
    let kept = !claim_released_within(&log, identity, Duration::from_secs(2));
    let running = other.0.try_wait().unwrap().is_none();
    drop(other);
    let released = claim_released_within(&log, identity, WINDOW_TIMEOUT);
    assert!(running, "the process spawned in the window exited early");
    assert!(kept, "a process spawned in the window did not hold the log");
    assert!(released, "the log stayed held after the process exited");
}

#[test]
fn a_process_spawned_in_the_window_of_a_vmm_in_its_logs_job_holds_the_log_but_not_its_claim() {
    let directory = tempfile::tempdir().unwrap();
    let held = HeldSpawn::start_in_job(directory.path());
    // The process inherits the log in the window, and the job's handle with it, but does not
    // join the job.
    let other = sleeper().spawn().map(Running);
    let (stand_in, log, identity) = held.stand_in();
    let mut other = other.unwrap();
    let held_while_running = log_claim_held(&log, identity).unwrap();
    assert!(
        stand_in.terminate(WINDOW_TIMEOUT),
        "the stand-in did not exit"
    );
    // Once the stand-in has exited, the job holds no process, and the claim is released, while
    // the process still holds the log.
    let released = claim_released_within(&log, identity, WINDOW_TIMEOUT);
    let holds = held_open(&log);
    let running = other.0.try_wait().unwrap().is_none();
    drop(other);
    assert!(running, "the process spawned in the window exited early");
    assert!(
        held_while_running,
        "the claim was released while the stand-in ran in its log's job"
    );
    assert!(
        holds,
        "the process spawned in the window did not hold the log"
    );
    assert!(
        released,
        "a process spawned in the window held the claim of a VMM in its log's job"
    );
}

#[test]
fn a_log_has_one_job_at_a_time() {
    let directory = tempfile::tempdir().unwrap();
    let log = fs::File::create(directory.path().join("vmm.log")).unwrap();
    let job = create_log_job(&log)
        .unwrap()
        .expect("a log on a disk volume has a job");
    let error = create_log_job(&log).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
    // A job that no process holds is gone.
    drop(job);
    assert!(create_log_job(&log).unwrap().is_some());
}

/// Creates a job object named `name`, NUL-terminated, whose security descriptor is `sddl`, in
/// the security descriptor definition language.
fn job_with_descriptor(name: &[u16], sddl: &str) -> OwnedHandle {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };

    let sddl = wide(sddl);
    let mut descriptor = ptr::null_mut();
    // SAFETY: the string is NUL-terminated, and the output pointer is valid.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    };
    assert_ne!(converted, 0, "{}", io::Error::last_os_error());
    let attributes = SECURITY_ATTRIBUTES {
        nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // SAFETY: the name is NUL-terminated, and the attributes hold a valid descriptor.
    let job = owned(unsafe { CreateJobObjectW(&attributes, name.as_ptr()) });
    // SAFETY: the conversion allocated the descriptor, which nothing uses any more.
    unsafe { LocalFree(descriptor) };
    job.unwrap_or_else(|| panic!("cannot create a job: {}", io::Error::last_os_error()))
}

#[test]
fn a_job_that_is_not_the_users_under_the_name_of_a_logs_job_is_not_the_logs() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("vmm.log");
    let log = fs::File::create(&path).unwrap();
    let identity = file_identity(&path).unwrap();
    let id = opened_file_id(&log).unwrap();
    // As jobs of other users look to this one: one that allows it nothing, and one that allows
    // every user, whose processes another user could choose.
    for sddl in ["D:P", "D:P(A;;GA;;;WD)"] {
        let other = job_with_descriptor(&log_job_name(id), sddl);
        // A spawn starts the VMM without a job rather than in that one...
        assert!(create_log_job(&log).unwrap().is_none(), "{sddl}");
        // ...and the claim is the log's handles, which this test holds, whatever that job
        // holds: none of its processes.
        assert_eq!(log_job_processes(id).unwrap(), None, "{sddl}");
        assert!(log_claim_held(&path, identity).unwrap(), "{sddl}");
        drop(other);
    }
    drop(log);
    assert!(!log_claim_held(&path, identity).unwrap());
}

#[test]
fn an_object_of_another_type_under_the_name_of_a_logs_job_is_not_the_logs() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("vmm.log");
    let log = fs::File::create(&path).unwrap();
    let identity = file_identity(&path).unwrap();
    let id = opened_file_id(&log).unwrap();
    let name = log_job_name(id);
    // An event shares the namespace of jobs, so both the creation and the opening of a job
    // under its name fail, with ERROR_INVALID_HANDLE.
    // SAFETY: the name is NUL-terminated.
    let event = owned(unsafe { CreateEventW(ptr::null(), 1, 0, name.as_ptr()) })
        .unwrap_or_else(|| panic!("cannot create an event: {}", io::Error::last_os_error()));
    // A spawn starts the VMM without a job...
    assert!(create_log_job(&log).unwrap().is_none());
    // ...and the claim is the log's handles, which this test holds.
    assert_eq!(log_job_processes(id).unwrap(), None);
    assert!(log_claim_held(&path, identity).unwrap());
    drop(log);
    assert!(!log_claim_held(&path, identity).unwrap());
    drop(event);
}

/// Puts a symbolic link of the object manager under the name `name` of the `Local\`
/// namespace, NUL-terminated, to the object that the object manager names `target`, as
/// another user that may create objects in the namespace could, and returns it: the link
/// lasts until it is closed.
fn local_link(name: &[u16], target: &[u16]) -> OwnedHandle {
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Win32::Foundation::{NTSTATUS, OBJ_CASE_INSENSITIVE, UNICODE_STRING};

    #[link(name = "ntdll", kind = "raw-dylib")]
    unsafe extern "system" {
        fn NtCreateSymbolicLinkObject(
            link: *mut HANDLE,
            access: u32,
            attributes: *const OBJECT_ATTRIBUTES,
            target: *const UNICODE_STRING,
        ) -> NTSTATUS;
    }
    /// Every right to a symbolic link, which `ntifs.h` defines.
    const SYMBOLIC_LINK_ALL_ACCESS: u32 = 0x000f_0001;

    let name = name.strip_suffix(&[0]).unwrap_or(name);
    let separator = name
        .iter()
        .rposition(|&unit| unit == u16::from(b'\\'))
        .unwrap();
    let mut path = local_directory().unwrap();
    path.extend_from_slice(&name[separator..]);
    let string = |units: &[u16]| UNICODE_STRING {
        Length: u16::try_from(mem::size_of_val(units)).unwrap(),
        MaximumLength: u16::try_from(mem::size_of_val(units)).unwrap(),
        Buffer: units.as_ptr().cast_mut(),
    };
    let (path_string, target_string) = (string(&path), string(target));
    let attributes = OBJECT_ATTRIBUTES {
        Length: mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        ObjectName: &path_string,
        Attributes: OBJ_CASE_INSENSITIVE,
        ..OBJECT_ATTRIBUTES::default()
    };
    let mut link = ptr::null_mut();
    // SAFETY: the attributes, the names that they and the target point to, and the output
    // outlive the call.
    let status = unsafe {
        NtCreateSymbolicLinkObject(
            &mut link,
            SYMBOLIC_LINK_ALL_ACCESS,
            &attributes,
            &target_string,
        )
    };
    assert!(
        status >= 0,
        "cannot create a link: {}",
        status_error(status)
    );
    owned(link).unwrap()
}

#[test]
fn what_a_link_under_the_name_of_a_logs_job_leads_to_is_not_the_logs() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("vmm.log");
    let log = fs::File::create(&path).unwrap();
    let identity = file_identity(&path).unwrap();
    let id = opened_file_id(&log).unwrap();
    let name = log_job_name(id);
    // An empty job of the user, as that of another log whose VMM has exited while a process
    // spawned in its window still holds the job.
    let other = fs::File::create(directory.path().join("other.log")).unwrap();
    let other = create_log_job(&other)
        .unwrap()
        .expect("a log on a disk volume has a job");
    // A link under the name of the log's job, which no job holds, to that one.
    let link = local_link(&name, &object_name(raw(&other)).unwrap());
    // A claim check leaves the job that the link leads to aside, and the claim is the log's
    // handles, which this test holds, where that empty job would have released it...
    assert_eq!(log_job_processes(id).unwrap(), None);
    assert!(log_claim_held(&path, identity).unwrap());
    // ...and a spawn starts the VMM without a job, rather than in the other log's.
    assert!(create_log_job(&log).unwrap().is_none());
    drop(link);
    // A link into a directory that does not exist leads to no object at all.
    let directory_name = local_directory().unwrap();
    let missing: Vec<u16> = directory_name
        .iter()
        .copied()
        .chain(format!(r"\nvx-missing-{:032x}\job", id.1).encode_utf16())
        .collect();
    let link = local_link(&name, &missing);
    assert_eq!(log_job_processes(id).unwrap(), None);
    assert!(log_claim_held(&path, identity).unwrap());
    assert!(create_log_job(&log).unwrap().is_none());
    drop(link);
    // A link to a name that no object holds leads the creation of a job there, which a spawn
    // drops, and a claim check, which finds no job at the end of the link, follows the log's
    // handles.
    let elsewhere: Vec<u16> = directory_name
        .iter()
        .copied()
        .chain(format!(r"\nvx-elsewhere-{:032x}", id.1).encode_utf16())
        .collect();
    let link = local_link(&name, &elsewhere);
    assert!(create_log_job(&log).unwrap().is_none());
    assert_eq!(log_job_processes(id).unwrap(), None);
    assert!(log_claim_held(&path, identity).unwrap());
    drop(link);
    // Without the links, the log takes a job of its own.
    drop(other);
    assert!(create_log_job(&log).unwrap().is_some());
    drop(log);
    assert!(!log_claim_held(&path, identity).unwrap());
}

#[test]
fn a_vmm_never_starts_outside_a_job_that_a_claim_check_takes_for_its_logs() {
    let directory = tempfile::tempdir().unwrap();
    let log = fs::File::create(directory.path().join("vmm.log")).unwrap();
    let id = opened_file_id(&log).unwrap();
    // A job of the user that allows the user to query it alone, which a creation of the log's
    // job cannot open, but which a claim check takes for the log's.
    let job = job_with_descriptor(&log_job_name(id), "D:P(A;;0x20004;;;OW)");
    assert_eq!(log_job_processes(id).unwrap(), Some(0));
    let error = create_log_job(&log).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
    drop(job);
    assert!(create_log_job(&log).unwrap().is_some());
}

#[test]
fn a_vmm_whose_log_is_no_file_of_a_disk_volume_starts_without_a_job() {
    use crate::host::supervisor::{self, VmmCommand};
    use crate::protocol::CAPABILITY_LEN;

    let null = OpenOptions::new().write(true).open("NUL").unwrap();
    assert!(create_log_job(&null).unwrap().is_none());
    let (_reader, writer) = io::pipe().unwrap();
    let pipe = fs::File::from(OwnedHandle::from(writer));
    assert!(create_log_job(&pipe).unwrap().is_none());
    // A pipe has an identity that a claim check can match, but no file ID: a claim check, as
    // a spawn does, takes it for a log without a job.
    assert!(opened_file_identity(&pipe).is_ok());
    assert_eq!(log_job_id(&null).unwrap(), None);
    assert_eq!(log_job_id(&pipe).unwrap(), None);
    let system = std::env::var_os("SystemRoot").unwrap();
    let cmd = Path::new(&system).join("System32").join("cmd.exe");
    let command = VmmCommand::new(
        cmd,
        ["/c", "exit", "0"].map(OsString::from).to_vec(),
        std::env::temp_dir(),
    );
    for log in [null, pipe] {
        let launched = supervisor::spawn(&command, &[7; CAPABILITY_LEN], log).unwrap();
        assert!(launched.kill_and_wait(), "the stand-in did not exit");
    }
}

#[test]
fn the_jobs_of_logs_whose_file_ids_share_their_low_64_bits_are_distinct() {
    // ReFS keeps the 128-bit file IDs of its files unique, but not their low 64 bits, which
    // its file indices are, and every store of a session names its jobs in one namespace.
    assert_ne!(log_job_name((7, 1)), log_job_name((7, (1 << 64) | 1)));
    assert_ne!(log_job_name((7, 1)), log_job_name((8, 1)));
}

/// Returns whether any process holds the file at `path` open: an open that shares nothing
/// then fails with a sharing violation.
fn held_open(path: &Path) -> bool {
    match OpenOptions::new().read(true).share_mode(0).open(path) {
        Ok(_) => false,
        Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) => true,
        Err(error) => panic!("cannot open {}: {error}", path.display()),
    }
}

#[test]
fn a_launch_whose_vmm_exited_ends_while_a_process_spawned_in_its_window_holds_its_log() {
    use crate::host::store::{ArtifactIdentity, LaunchId, Recovery, RuntimeIdentity, SandboxStore};
    use crate::host::supervisor::{self, VmmCommand};
    use crate::protocol::CAPABILITY_LEN;

    let directory = tempfile::tempdir().unwrap();
    let store = SandboxStore::create(directory.path().join("sandbox")).unwrap();
    let artifact = |digit: char| ArtifactIdentity::sha256(&digit.to_string().repeat(64)).unwrap();
    let runtime = RuntimeIdentity::new(artifact('a'), artifact('b'), artifact('c'));
    let capability = [7; CAPABILITY_LEN];
    let mut locked = store.lock(None).unwrap();
    let mut launch = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime, &capability)
        .unwrap();
    let system = std::env::var_os("SystemRoot").unwrap();
    let cmd = Path::new(&system).join("System32").join("cmd.exe");
    let command = VmmCommand::new(
        cmd,
        ["/c", "exit", "0"].map(OsString::from).to_vec(),
        store.dir(),
    );
    // While the VMM is created, another part of the caller spawns a process without the
    // spawn lock, as `std::process::Command` spawns one, which inherits the VMM's log.
    let other = std::rc::Rc::new(std::cell::RefCell::new(None));
    let spawned = std::rc::Rc::clone(&other);
    IN_WINDOW.set(Some(Box::new(move || {
        *spawned.borrow_mut() = Some(sleeper().spawn().map(Running));
    })));
    let launched = supervisor::spawn(&command, &capability, launch.take_log().unwrap());
    IN_WINDOW.set(None);
    let launched = launched.unwrap();
    let mut other = other
        .take()
        .expect("no process was spawned in the window")
        .unwrap();
    let vmm = launch.record_process(&launched).unwrap();
    launched.detach();
    assert!(
        vmm.wait(Instant::now() + WINDOW_TIMEOUT).unwrap(),
        "the stand-in did not exit"
    );
    // The VMM has exited, so the launch ends, whatever the other process holds.
    let ended = launch.abandon().unwrap();
    let holds = held_open(&store.log_path());
    let running = other.0.try_wait().unwrap().is_none();
    drop(other);
    assert!(running, "the process spawned in the window exited early");
    assert!(
        holds,
        "the process spawned in the window did not hold the log"
    );
    assert_eq!(ended, Recovery::Exited);
    assert!(store.journal().unwrap().is_none());
}

#[test]
fn a_thread_that_holds_the_spawn_lock_spawns_under_it_until_its_last_lock_is_dropped() {
    let first = lock_spawns();
    let second = lock_spawns();
    // Neither a spawn of a VMM nor a spawn under the lock waits for the thread's own hold.
    let (stdin, _stdin_writer) = io::pipe().unwrap();
    let (_stdout_reader, stdout) = io::pipe().unwrap();
    let stderr = stdout.try_clone().unwrap();
    let system = std::env::var_os("SystemRoot").unwrap();
    let cmd = Path::new(&system).join("System32").join("cmd.exe");
    let vmm = spawn_detached(
        &cmd,
        &["/c", "exit", "0"].map(OsString::from),
        &std::env::temp_dir(),
        [stdin.into(), stdout.into(), stderr.into()],
        &[],
        false,
        None,
    )
    .unwrap();
    let other = spawn_command(&mut sleeper()).map(Running).unwrap();
    // The hold lasts until the thread's last lock is dropped, whichever that is.
    let (taken, acquired) = std::sync::mpsc::channel();
    let waiter = thread::spawn(move || {
        let _lock = lock_spawns();
        let _ = taken.send(());
    });
    drop(first);
    let early = acquired.recv_timeout(Duration::from_millis(200)).is_ok();
    drop(second);
    let late = acquired.recv_timeout(WINDOW_TIMEOUT).is_ok();
    waiter.join().unwrap();
    assert!(vmm.terminate(WINDOW_TIMEOUT), "the stand-in did not exit");
    drop(other);
    assert!(
        !early,
        "another thread took the lock while this one held it"
    );
    assert!(late, "another thread could not take the lock once released");
}
