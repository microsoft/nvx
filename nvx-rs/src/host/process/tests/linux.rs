use std::env;
use std::fs;
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use super::*;
use crate::host::sys;

fn sleeper() -> Command {
    let mut command = Command::new("sleep");
    command.arg("60").stdin(Stdio::null());
    command
}

#[test]
fn zombies_run_while_other_threads_remain() {
    // Field 22 of a stat line, the start time, is 22 here; the command name holds a space
    // and a parenthesis.
    let stat = |state: &str| {
        let fields: Vec<String> = (4..=22).map(|field| field.to_string()).collect();
        format!("7 (open vmm)) {state} {}\n", fields.join(" ")).into_bytes()
    };
    let unasked = || -> io::Result<bool> { panic!("only zombies count their threads") };
    let start_time = |state: &str, remain: io::Result<bool>| {
        platform::tests::start_time_from_stat(7, &stat(state), || remain)
    };
    for state in ["R", "S", "D", "T"] {
        assert_eq!(
            platform::tests::start_time_from_stat(7, &stat(state), unasked).unwrap(),
            Some(22)
        );
    }
    assert_eq!(
        platform::tests::start_time_from_stat(7, &stat("X"), unasked).unwrap(),
        None
    );
    assert_eq!(start_time("Z", Ok(true)).unwrap(), Some(22));
    assert_eq!(start_time("Z", Ok(false)).unwrap(), None);
    assert!(start_time("Z", Err(io::Error::other("unreadable"))).is_err());
    assert!(platform::tests::start_time_from_stat(7, b"7 (vmm) S 1 2", unasked).is_err());
    assert!(platform::tests::start_time_from_stat(7, b"garbage", unasked).is_err());
}

#[test]
fn command_names_need_not_be_utf8() {
    // Field 22 of a stat line, the start time, is 22 here.
    let stat = |name: &[u8]| {
        let fields: Vec<String> = (4..=22).map(|field| field.to_string()).collect();
        let mut stat = b"7 (".to_vec();
        stat.extend_from_slice(name);
        stat.extend_from_slice(format!(") S {}\n", fields.join(" ")).as_bytes());
        stat
    };
    let unasked = || -> io::Result<bool> { panic!("only zombies count their threads") };
    // The kernel cuts this name at 15 bytes, inside the two bytes of its last character, and
    // a process may also name itself with bytes that are never UTF-8, here around a
    // parenthesis and a space.
    let cut = &"vmm-reused-pid\u{e9}".as_bytes()[..15];
    for name in [cut, b"\xff) \xfe"] {
        assert!(std::str::from_utf8(name).is_err());
        assert_eq!(
            platform::tests::start_time_from_stat(7, &stat(name), unasked).unwrap(),
            Some(22)
        );
    }
    // The fields after the command name are text.
    let garbled = b"7 (vmm) S \xff 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22\n";
    assert!(platform::tests::start_time_from_stat(7, garbled, unasked).is_err());
}

/// Kills and reaps a child process when dropped, even when a test fails.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_process_that_proc_hides_never_counts_as_exited() {
    let mut child = Reaped(sleeper().spawn().unwrap());
    let pid = child.0.id();
    let start_time = platform::process_start_time(pid).unwrap().unwrap();
    let pidfd = || Some(sys::PidFd::open(pid).unwrap());
    // `/proc` hides the process, as `hidepid` hides the processes of other users, while its
    // pidfd, or without one, signal 0, shows that a process has its ID.
    for pidfd in [pidfd(), None] {
        let hidden = platform::tests::check_identity(pid, start_time, pidfd, None);
        assert_eq!(
            hidden.err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
    // Shown, it is held.
    let stat = fs::read(format!("/proc/{pid}/stat")).unwrap();
    let shown = platform::tests::check_identity(pid, start_time, pidfd(), Some(stat));
    assert!(shown.unwrap().is_some());
    // Once it has exited, its pidfd says so, and once it is reaped, no process has its ID.
    let held = pidfd();
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let exited = platform::tests::check_identity(pid, start_time, held, None);
    assert!(exited.unwrap().is_none());
    let reaped = platform::tests::check_identity(pid, start_time, None, None);
    assert!(reaped.unwrap().is_none());
}

#[test]
fn a_process_runs_until_its_last_thread_exits() {
    // The main thread leaves through pthread_exit, which turns it into a zombie, while
    // another thread sleeps. Python is the only portable way to arrange that here.
    let spawned = Command::new("python3")
        .args([
            "-c",
            "import ctypes, threading, time\n\
                 threading.Thread(target=time.sleep, args=(60,)).start()\n\
                 ctypes.CDLL(None).pthread_exit(None)\n",
        ])
        .stdin(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(child) => Reaped(child),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            eprintln!("skipping: python3 is not installed");
            return;
        }
        Err(error) => panic!("cannot start python3: {error}"),
    };
    let pid = child.0.id();
    let start_time = platform::process_start_time(pid).unwrap().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        if stat[stat.rfind(')').unwrap() + 1..]
            .trim_start()
            .starts_with('Z')
        {
            break;
        }
        assert!(Instant::now() < deadline, "the main thread did not exit");
        thread::sleep(POLL_INTERVAL);
    }
    assert_eq!(platform::process_start_time(pid).unwrap(), Some(start_time));
    let Opened::Running(process) = Process::open(&ProcessRef {
        pid,
        start_time,
        boot: BootId::current().unwrap(),
    })
    .unwrap() else {
        panic!("a process whose main thread exited still runs");
    };
    assert!(
        process
            .wait(Instant::now() + Duration::from_millis(100))
            .unwrap()
            .is_none()
    );
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(process.wait(Instant::now()).unwrap().is_some());
}

#[test]
fn a_process_whose_command_name_is_not_utf8_can_be_examined() {
    // A process takes as its command name the first 15 bytes of the name of the file that it
    // executes: here a link to `sleep` whose 15th byte is the first of a two-byte character.
    // `argv[0]` still says `sleep`, which multi-call executables such as BusyBox read.
    let path = env::var_os("PATH").unwrap_or_default();
    let sleep = env::split_paths(&path)
        .map(|directory| directory.join("sleep"))
        .find(|candidate| candidate.is_file())
        .expect("sleep is on PATH");
    let directory = tempfile::tempdir().unwrap();
    let link = directory.path().join("vmm-reused-pid\u{e9}");
    // A `PATH` entry may be relative to the current directory, but a link resolves a relative
    // target from its own.
    symlink(std::path::absolute(sleep).unwrap(), &link).unwrap();
    let mut child = Reaped(
        Command::new(&link)
            .arg0("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid = child.0.id();
    // The child may take the name only after `spawn` returns.
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::str::from_utf8(&fs::read(format!("/proc/{pid}/stat")).unwrap()).is_ok() {
        assert!(Instant::now() < deadline, "the child did not take the name");
        thread::sleep(POLL_INTERVAL);
    }
    let reference = ProcessRef {
        pid,
        start_time: platform::process_start_time(pid).unwrap().unwrap(),
        boot: BootId::current().unwrap(),
    };
    assert!(matches!(
        Process::open(&reference).unwrap(),
        Opened::Running(_)
    ));
    // A reference to a process that exited and whose ID this one reused names no process.
    let stale = ProcessRef {
        start_time: reference.start_time + 1,
        ..reference
    };
    assert!(matches!(Process::open(&stale).unwrap(), Opened::Exited(_)));
    child.0.kill().unwrap();
    child.0.wait().unwrap();
}
