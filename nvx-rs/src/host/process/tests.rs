use serde_json::json;

use super::*;

#[cfg(target_os = "linux")]
mod linux;

/// The boot of the references in these tests.
const BOOT: BootId = BootId::from_bytes([0x5a; 16]);

/// Returns a reference to process `pid` started at `start_time` during [`BOOT`].
fn reference(pid: u32, start_time: u64) -> ProcessRef {
    ProcessRef {
        pid,
        start_time,
        boot: BOOT,
    }
}

/// Fails the test if a process is opened.
fn never_opened(_: u32, _: u64) -> io::Result<Option<()>> {
    panic!("a reference that cannot name a running process is never opened")
}

#[test]
fn boot_identities_have_one_text_form() {
    let text = "00112233445566778899aabbccddeeff";
    let boot: BootId = text.parse().unwrap();
    assert_eq!(
        boot.as_bytes(),
        &[
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff
        ]
    );
    assert_eq!(boot.to_string(), text);
    assert_eq!(format!("{boot:?}"), format!("BootId({text})"));
    assert_eq!(text.to_uppercase().parse::<BootId>().unwrap(), boot);
    for malformed in [
        "",
        "0011",
        &text[1..],
        &format!("{text}0"),
        "00112233-4455-6677-8899-aabbccddeeff",
        "0g112233445566778899aabbccddeeff",
        "+0112233445566778899aabbccddeeff",
        " 0112233445566778899aabbccddeeff",
        "0011223344556677\u{e9}8899aabbccddee",
    ] {
        let error = malformed.parse::<BootId>().unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::Io, "{malformed:?}");
    }

    let encoded = serde_json::to_string(&boot).unwrap();
    assert_eq!(encoded, format!("\"{text}\""));
    assert_eq!(serde_json::from_str::<BootId>(&encoded).unwrap(), boot);
    assert!(serde_json::from_str::<BootId>("\"0011\"").is_err());
    assert!(serde_json::from_str::<BootId>("[0, 17]").is_err());
}

#[test]
fn references_record_their_three_parts() {
    let reference = reference(7, 42);
    let encoded = serde_json::to_value(&reference).unwrap();
    assert_eq!(
        encoded,
        json!({"pid": 7, "startTime": 42, "boot": "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a"})
    );
    assert_eq!(
        serde_json::from_value::<ProcessRef>(encoded).unwrap(),
        reference
    );
    // A reference without its boot, or with a field that this crate does not know, is not one.
    for malformed in [
        json!({"pid": 7, "startTime": 42}),
        json!({"pid": 7, "startTime": 42, "boot": BOOT, "endpoint": "x"}),
        json!({"pid": -7, "startTime": 42, "boot": BOOT}),
    ] {
        assert!(serde_json::from_value::<ProcessRef>(malformed).is_err());
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn the_current_boot_has_one_identity() {
    let boot = BootId::current().unwrap();
    assert_eq!(BootId::current().unwrap(), boot);
    assert_eq!(BootId::from_bytes(platform::boot_id().unwrap()), boot);
}

#[test]
fn references_from_another_boot_name_exited_processes() {
    let other = BootId::from_bytes([0xa5; 16]);
    let opened = open_reference(&reference(7, 42), || Ok(other), never_opened);
    assert!(opened.unwrap().is_none());
}

#[test]
fn undetermined_identities_are_errors_and_never_opened() {
    // The current boot is unknown.
    let unknown = open_reference(
        &reference(7, 42),
        || Err(io::Error::other("no boot identity")),
        never_opened,
    );
    assert_eq!(unknown.unwrap_err().to_string(), "no boot identity");
    // The state of the process cannot be read.
    let undetermined = open_reference(
        &reference(7, 42),
        || Ok(BOOT),
        |_, _| Err::<Option<()>, _>(io::Error::from(io::ErrorKind::PermissionDenied)),
    );
    assert_eq!(
        undetermined.unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    // No process has the ID 0.
    let zero = open_reference(
        &reference(0, 42),
        || panic!("an invalid reference is not checked against the boot"),
        never_opened,
    );
    assert_eq!(zero.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    // During the same boot, the process is opened with its recorded identity.
    let opened = open_reference(
        &reference(7, 42),
        || Ok(BOOT),
        |pid, start_time| Ok(Some((pid, start_time))),
    );
    assert_eq!(opened.unwrap(), Some((7, 42)));
}

#[test]
fn polling_checks_the_identity_until_the_deadline() {
    let later = Instant::now() + Duration::from_secs(60);
    // The process exits after two checks.
    let mut checks = [Ok(true), Ok(true), Ok(false)].into_iter();
    assert!(poll_exit(later, || checks.next().unwrap()).unwrap());
    // The process runs past the deadline, which bounds the wait.
    let started = Instant::now();
    let deadline = started + Duration::from_millis(80);
    assert!(!poll_exit(deadline, || Ok(true)).unwrap());
    assert!(Instant::now() >= deadline);
    assert!(started.elapsed() < Duration::from_secs(10));
    // A check that cannot determine the state ends the wait with its error, not an exit.
    let error = poll_exit(later, || Err(io::Error::other("unreadable"))).unwrap_err();
    assert_eq!(error.to_string(), "unreadable");
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn failures_to_end_a_process_that_exited_are_not_failures() {
    let refused = || Err(io::Error::from(io::ErrorKind::PermissionDenied));
    platform::unless_exited(Ok(()), || panic!("a success asks nothing")).unwrap();
    platform::unless_exited(refused(), || Ok(true)).unwrap();
    let running = platform::unless_exited(refused(), || Ok(false));
    assert_eq!(running.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    let unknown = platform::unless_exited(refused(), || Err(io::Error::other("unknown")));
    assert_eq!(unknown.unwrap_err().to_string(), "unknown");
}

/// Returns a command that runs for a minute unless it is killed. The tests spawn it under a
/// `SpawnLock`, so that on Windows it cannot hold the log of a VMM that another test spawns
/// meanwhile.
#[cfg(any(target_os = "linux", windows))]
fn sleeper() -> std::process::Command {
    let mut command = if cfg!(windows) {
        let mut command = std::process::Command::new("ping");
        command.args(["-n", "60", "127.0.0.1"]);
        command
    } else {
        let mut command = std::process::Command::new("sleep");
        command.arg("60");
        command
    };
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    command
}

#[cfg(any(target_os = "linux", windows))]
struct ExitChild(std::process::Child);

#[cfg(any(target_os = "linux", windows))]
impl ExitChild {
    fn spawn() -> Self {
        Self(crate::host::tests::spawn_locked(&mut sleeper()).unwrap())
    }

    fn reference(&self) -> ProcessRef {
        ProcessRef {
            pid: self.0.id(),
            start_time: platform::process_start_time(self.0.id()).unwrap().unwrap(),
            boot: BootId::current().unwrap(),
        }
    }
}

#[cfg(any(target_os = "linux", windows))]
impl Drop for ExitChild {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
            self.0.wait().unwrap();
        }
    }
}

#[cfg(target_os = "linux")]
fn wait_exit_handle(handle: ExitHandle, timeout: Duration) -> io::Result<bool> {
    use std::os::fd::AsFd;

    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

    let mut descriptors = [PollFd::new(handle.as_fd(), PollFlags::POLLIN)];
    if poll(&mut descriptors, PollTimeout::try_from(timeout).unwrap())? == 0 {
        return Ok(false);
    }
    let events = descriptors[0].revents().unwrap();
    assert!(
        events.intersects(PollFlags::POLLIN | PollFlags::POLLHUP),
        "duplicated pidfd reported {events:?}",
    );
    Ok(true)
}

#[cfg(windows)]
fn wait_exit_handle(handle: ExitHandle, timeout: Duration) -> io::Result<bool> {
    platform::tests::wait_exit_handle(handle, Instant::now() + timeout)
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn exit_handles_are_independently_owned_and_exist_only_for_exit_events() {
    let mut child = ExitChild(crate::host::tests::spawn_locked(&mut sleeper()).unwrap());
    let reference = ProcessRef {
        pid: child.0.id(),
        start_time: platform::process_start_time(child.0.id()).unwrap().unwrap(),
        boot: BootId::current().unwrap(),
    };
    let Opened::Running(process) = Process::open(&reference).unwrap() else {
        panic!("the child runs");
    };
    let handle = process.exit_handle().unwrap();
    assert_eq!(handle.is_some(), process.wait_method() == WaitMethod::Event);
    if handle.is_some() {
        let probe = process.exit_handle().unwrap().unwrap();
        assert!(!wait_exit_handle(probe, Duration::ZERO).unwrap());
    }
    drop(process);
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    if let Some(handle) = handle {
        assert!(wait_exit_handle(handle, Duration::from_secs(10)).unwrap());
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn opening_a_recorded_child_returns_running_only_for_its_identity() {
    let mut child = ExitChild::spawn();
    let reference = child.reference();
    let Opened::Running(process) = Process::open(&reference).unwrap() else {
        panic!("the recorded child runs");
    };
    assert_eq!(process.reference(), &reference);
    assert!(!process.has_exited().unwrap());
    let mut other_boot = *reference.boot.as_bytes();
    other_boot[0] ^= 1;
    for exited in [
        ProcessRef {
            boot: BootId::from_bytes(other_boot),
            ..reference.clone()
        },
        ProcessRef {
            start_time: reference.start_time + 1,
            ..reference.clone()
        },
    ] {
        assert!(matches!(Process::open(&exited).unwrap(), Opened::Exited(_)));
    }
    assert!(child.0.try_wait().unwrap().is_none());
    assert!(!process.has_exited().unwrap());
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn opening_a_reaped_recorded_child_proves_exit() {
    let mut child = ExitChild::spawn();
    let reference = child.reference();
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(matches!(
        Process::open(&reference).unwrap(),
        Opened::Exited(_)
    ));
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn polling_follows_a_real_process_until_it_exits() {
    let mut child = crate::host::tests::spawn_locked(&mut sleeper()).unwrap();
    let pid = child.id();
    let start_time = platform::process_start_time(pid).unwrap().unwrap();
    let runs = || -> io::Result<bool> { Ok(platform::open_process(pid, start_time)?.is_some()) };
    let running = poll_exit(Instant::now() + Duration::from_millis(80), runs);
    child.kill().unwrap();
    let exited = poll_exit(Instant::now() + Duration::from_secs(10), runs);
    child.wait().unwrap();
    assert!(!running.unwrap());
    assert!(exited.unwrap());
}

#[cfg(target_os = "linux")]
#[test]
fn hosts_without_pidfds_poll_and_never_signal() {
    let mut child = crate::host::tests::spawn_locked(&mut sleeper()).unwrap();
    let pid = child.id();
    let process = Process {
        reference: ProcessRef {
            pid,
            start_time: platform::process_start_time(pid).unwrap().unwrap(),
            boot: BootId::current().unwrap(),
        },
        handle: platform::ProcessHandle::without_pidfd(),
    };
    assert_eq!(process.wait_method(), WaitMethod::Polling);
    assert!(process.exit_handle().unwrap().is_none());
    let running = process.wait(Instant::now() + Duration::from_millis(80));
    // Nothing holds the process, so it is not signaled.
    let refused = process.kill();
    let still_running = process.has_exited();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(running.unwrap().is_none());
    assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::Unsupported);
    assert!(!still_running.unwrap());
    // Once the child is reaped, its ID names no process, or another one.
    assert!(
        process
            .wait(Instant::now() + Duration::from_secs(10))
            .unwrap()
            .is_some()
    );
}

/// Returns a command that reads the first line of the file at `path`, prints `read`, then
/// waits for a line of its standard input, which reaches its end once the test closes it, and
/// exits. The shell reads the line itself, through read calls of its own.
#[cfg(windows)]
fn reader(path: &std::path::Path) -> std::process::Command {
    use std::os::windows::process::CommandExt;

    let mut command = std::process::Command::new("cmd");
    // `cmd` takes the rest of its command line as it is, the quotes around the path included.
    command.args(["/d", "/q", "/c"]).raw_arg(format!(
        "set /p line=<\"{}\" & echo read & set /p wait=",
        path.display()
    ));
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    command
}

/// Returns a command that reads the first line of the file at `path`, prints `read`, then
/// waits for a line of its standard input, which reaches its end once the test closes it, and
/// exits. The shell reads the line itself, through read calls of its own.
#[cfg(target_os = "linux")]
fn reader(path: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new("sh");
    command
        .args(["-c", "read -r line < \"$0\"; echo read; read -r wait"])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    command
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn processes_count_the_bytes_that_their_read_calls_read() {
    use std::io::{BufRead, BufReader, Write};

    // A line short enough for `set /p`, which reads at most 1023 characters of one.
    const LINE: usize = 512;
    let path = std::env::temp_dir().join(format!("nvx-bytes-read-{}", std::process::id()));
    let mut contents = vec![b'x'; LINE];
    contents.push(b'\n');
    std::fs::write(&path, &contents).unwrap();

    // This process counts what it reads, and what other tests' threads read meanwhile.
    let before = own_bytes_read().unwrap();
    let read = std::fs::read(&path).unwrap();
    assert!(own_bytes_read().unwrap() >= before + read.len() as u64);

    // Another process counts what it read once it says so, while it runs.
    let mut child = crate::host::tests::spawn_locked(&mut reader(&path)).unwrap();
    let mut said = String::new();
    let stdout = child.stdout.take().unwrap();
    BufReader::new(stdout).read_line(&mut said).unwrap();
    let reference = ProcessRef {
        pid: child.id(),
        start_time: platform::process_start_time(child.id()).unwrap().unwrap(),
        boot: BootId::current().unwrap(),
    };
    let Opened::Running(process) = Process::open(&reference).unwrap() else {
        panic!("the reader runs");
    };
    let counted = process.bytes_read();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"x\n").unwrap();
    drop(stdin);
    child.wait().unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(said.trim_end(), "read");
    assert!(counted.unwrap() >= LINE as u64);
    // Once the process exited, Linux keeps no count of it, and Windows keeps its count in the
    // handle.
    let exited = process.bytes_read();
    if cfg!(windows) {
        assert!(exited.unwrap() >= LINE as u64);
    } else {
        assert_eq!(exited.unwrap_err().kind(), io::ErrorKind::NotFound);
    }
}
