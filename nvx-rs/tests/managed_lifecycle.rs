//! The managed lifecycle of a guest whose init hands the boot to the managed agent, on a real
//! hypervisor: authenticated sessions and reconnects, request-scoped working directories and
//! environments, the workload identity, timeouts, refused working directories, and stop, driven
//! through `nvx::host::session` as `nvx::host::instance` drives it.
//!
//! `scripts/nvx.py test-microvm --scenario managed-lifecycle` runs it for the selected guest and
//! backend, then checks the outcome report that OpenVMM wrote and the guest console. It sets:
//!
//! - `NVX_MANAGED_LIFECYCLE_COMMAND`: OpenVMM's command line, as a JSON array of strings, with the
//!   managed lifecycle, the control console, `--microvm-control-auth-stdin`, and the report.
//! - `NVX_MANAGED_LIFECYCLE_ENDPOINT`: the control endpoint that the command listens on.
//! - `NVX_MANAGED_LIFECYCLE_CONSOLE`: the TCP address at which the command's guest console
//!   listens, `HOST:PORT`.
//! - `NVX_MANAGED_LIFECYCLE_TIMEOUT`: the longest wait for each operation, in seconds.
//! - `NVX_MANAGED_LIFECYCLE_OUTPUT_DIR`: receives OpenVMM's output, `managed-lifecycle.log`, and
//!   the guest console, `managed-lifecycle-guest.log`, which the scenario then scans.
//!
//! The test also records OpenVMM's process ID in `NVX_ADVERSARIAL_OPENVMM_PID_JOURNAL`, if set,
//! as the microVM scenarios do for the adversarial campaigns.
//!
//! These checks were `run_managed_lifecycle` of `scripts/nvx_tools/microvm_tests.py`, which drove
//! the control session from Python until microsoft/nvx#359 left the protocol to nvx-rs alone.
#![cfg(all(feature = "host", any(target_os = "linux", windows)))]

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nvx::host::instance::generate_capability;
use nvx::host::platform::process_start_time;
use nvx::host::session::{self, ControlSession, ExecEvent};
use nvx::protocol::{CAPABILITY_LEN, CWD_FAILED, ExitCategory, Workload, WorkloadEnvironment};

/// An entry whose value has spaces, an equals sign, and a character beyond ASCII.
const COMPLEX: &str = "COMPLEX=space = \u{2603}";
/// The file that a workload creates if it runs despite a refused working directory.
const REFUSED_CWD_MARKER: &str = "/tmp/nvx-refused-cwd-ran";
/// Working directories that the workload cannot enter, each with the Linux error number that the
/// guest refuses it with, which does not depend on the host.
const REFUSED_CWDS: [(&str, &str, i32); 3] = [
    ("missing", "/does-not-exist", 2),
    ("file", "/etc/passwd", 20),
    ("inaccessible", "/root", 13),
];
/// The identity of the workloads, which the scenario's command line selects.
const WORKLOAD_IDENTITY: &str = "65534:65534";

fn variable(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| {
        panic!("{name} must be set; run scripts/nvx.py test-microvm --scenario managed-lifecycle")
    })
}

/// OpenVMM, killed when the test ends before it exits.
struct OpenVmm(Child);

impl Drop for OpenVmm {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// How a workload ended, and its output.
#[derive(Debug, Default)]
struct Outcome {
    /// How it exited, and its status, if it ran.
    exit: Option<(ExitCategory, i32)>,
    /// The guest's error number and category, if the guest refused it.
    refusal: Option<(i32, String)>,
    /// Its standard output.
    stdout: Vec<u8>,
    /// Its standard error, or the guest's diagnostic of a refusal.
    stderr: Vec<u8>,
}

impl Outcome {
    /// Returns whether the workload exited with status 0 and printed exactly `stdout`.
    fn printed(&self, stdout: &str) -> bool {
        self.exit == Some((ExitCategory::Exit, 0))
            && self.stdout == stdout.as_bytes()
            && self.stderr.is_empty()
    }
}

/// The control session of the guest that OpenVMM `pid` runs.
struct Guest {
    /// The control endpoint.
    endpoint: String,
    /// OpenVMM's process ID.
    pid: u32,
    /// OpenVMM's start time.
    start_time: u64,
    /// The capability that OpenVMM received.
    capability: [u8; CAPABILITY_LEN],
    /// The longest wait for each operation.
    timeout: Duration,
}

impl Guest {
    /// Opens and authenticates a session.
    fn connect(&self) -> ControlSession {
        let deadline = Instant::now() + self.timeout;
        let transport = session::connect(&self.endpoint, self.pid, self.start_time, deadline)
            .expect("the control endpoint accepts a session");
        ControlSession::attach(transport, &self.capability, deadline)
            .expect("the control endpoint accepts the capability")
    }

    /// Runs `argv` in `session` and returns how it ended.
    fn exec(
        &self,
        session: &mut ControlSession,
        argv: &[&str],
        timeout_ms: u32,
        cwd: Option<&str>,
        environment: WorkloadEnvironment<'_>,
    ) -> Outcome {
        let workload = Workload {
            argv: argv.iter().map(|&argument| argument.to_owned()).collect(),
            timeout_ms,
            cwd,
            environment,
        };
        // The longest wait bounds the whole exchange, so the workloads must end well before it.
        let deadline = Instant::now() + self.timeout;
        let request = session
            .start_exec(&workload, deadline)
            .expect("the guest accepts the request");
        let mut outcome = Outcome::default();
        loop {
            match session
                .next_exec_event(request, Some(deadline))
                .unwrap_or_else(|error| panic!("{argv:?} did not end: {error}"))
            {
                ExecEvent::Stdout(chunk) => outcome.stdout.extend(chunk),
                ExecEvent::Stderr(chunk) => outcome.stderr.extend(chunk),
                ExecEvent::Exit { category, status } => {
                    outcome.exit = Some((category, status));
                    return outcome;
                }
                ExecEvent::Rejected { status, category } => {
                    outcome.refusal = Some((status, category));
                    return outcome;
                }
            }
        }
    }
}

/// Copies the guest console that listens at `address` into `path` until OpenVMM closes it.
///
/// The console of a microVM listens on TCP, so the copy connects as soon as OpenVMM listens, as
/// the scenario's console monitor did when it drove the session from Python.
fn capture_console(address: String, path: PathBuf, deadline: Instant) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut console = loop {
            match TcpStream::connect(&address) {
                Ok(console) => break console,
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "the guest console at {address} did not accept a connection: {error}"
                    );
                    thread::sleep(Duration::from_millis(25));
                }
            }
        };
        let mut log = File::create(&path).expect("the guest console log opens");
        // A reset, as OpenVMM's exit may cause, ends the console like its end.
        let _ = io::copy(&mut console, &mut log);
    })
}

/// Records OpenVMM's process ID for an adversarial campaign, as `record_adversarial_openvmm_pid`
/// of the microVM tests does.
fn record_pid(pid: u32) {
    let Ok(journal) = env::var("NVX_ADVERSARIAL_OPENVMM_PID_JOURNAL") else {
        return;
    };
    let recorded_at_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after the epoch")
        .as_nanos();
    let mut stream = OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal)
        .expect("the process ID journal opens");
    writeln!(
        stream,
        "{{\"pid\":{pid},\"recorded_at_ns\":{recorded_at_ns}}}"
    )
    .expect("the process ID journal is writable");
    stream.sync_all().expect("the process ID journal syncs");
}

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-microvm \
            --scenario managed-lifecycle"]
fn a_managed_guest_runs_request_scoped_workloads_until_it_stops() {
    let command: Vec<String> = serde_json::from_str(&variable("NVX_MANAGED_LIFECYCLE_COMMAND"))
        .expect("NVX_MANAGED_LIFECYCLE_COMMAND is a JSON array of strings");
    let timeout = Duration::from_secs_f64(
        variable("NVX_MANAGED_LIFECYCLE_TIMEOUT")
            .parse()
            .expect("NVX_MANAGED_LIFECYCLE_TIMEOUT is a number of seconds"),
    );
    let output_dir = PathBuf::from(variable("NVX_MANAGED_LIFECYCLE_OUTPUT_DIR"));
    fs::create_dir_all(&output_dir).unwrap();
    let log = File::create(output_dir.join("managed-lifecycle.log")).unwrap();
    let capability = generate_capability().unwrap();
    // OpenVMM reads its capability as soon as it starts, so its input already holds the
    // capability and its end, as `nvx::host::process::spawn` prepares it.
    let (input, mut writer) = io::pipe().unwrap();
    writer.write_all(&capability).unwrap();
    drop(writer);
    let child = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::from(input))
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap_or_else(|error| panic!("cannot start {}: {error}", command[0]));
    let mut openvmm = OpenVmm(child);
    let pid = openvmm.0.id();
    record_pid(pid);
    let console = capture_console(
        variable("NVX_MANAGED_LIFECYCLE_CONSOLE"),
        output_dir.join("managed-lifecycle-guest.log"),
        Instant::now() + timeout,
    );
    let guest = Guest {
        endpoint: variable("NVX_MANAGED_LIFECYCLE_ENDPOINT"),
        pid,
        start_time: process_start_time(pid)
            .unwrap()
            .expect("OpenVMM runs after it started"),
        capability,
        timeout,
    };

    // The first session finds the guest agent ready, and a workload runs in its own working
    // directory with exactly the environment that it asks for, an empty value included.
    let mut session = guest.connect();
    session
        .ping(Instant::now() + timeout)
        .expect("the guest agent is ready");
    let exact = vec!["EMPTY=".to_owned(), COMPLEX.to_owned()];
    let first = guest.exec(
        &mut session,
        &[
            "/bin/sh",
            "-c",
            "/bin/touch nvx-managed-state; printf '%s|%s|%s' \"$PWD\" \"$EMPTY\" \"$COMPLEX\"",
        ],
        5_000,
        Some("/tmp"),
        WorkloadEnvironment::Replaced(&exact),
    );
    assert!(
        first.printed("/tmp||space = \u{2603}"),
        "the first managed workload returned {first:?}"
    );
    drop(session);

    // A second session reaches the same guest. An explicitly empty environment holds no
    // variable, and an exact one holds exactly its entries.
    let mut session = guest.connect();
    let empty = guest.exec(
        &mut session,
        &["/usr/bin/env"],
        5_000,
        None,
        WorkloadEnvironment::Replaced(&[]),
    );
    assert!(empty.printed(""), "an empty environment printed {empty:?}");
    let replaced = guest.exec(
        &mut session,
        &["/usr/bin/env"],
        5_000,
        None,
        WorkloadEnvironment::Replaced(&exact),
    );
    assert!(
        replaced.printed("EMPTY=\nCOMPLEX=space = \u{2603}\n"),
        "an exact environment printed {replaced:?}"
    );
    // Every timeout that fits the protocol's 32 bits is accepted, and a workload that ends first
    // finishes normally.
    for timeout_ms in [0, 3_600_001, 86_400_000, u32::MAX] {
        let outcome = guest.exec(
            &mut session,
            &["/bin/true"],
            timeout_ms,
            None,
            WorkloadEnvironment::Default,
        );
        assert!(outcome.printed(""), "timeout {timeout_ms} ms: {outcome:?}");
    }
    // Workloads run as the configured identity.
    let identity = guest.exec(
        &mut session,
        &["/bin/sh", "-c", "printf '%s:%s' \"$(id -u)\" \"$(id -g)\""],
        5_000,
        None,
        WorkloadEnvironment::Default,
    );
    assert!(
        identity.printed(WORKLOAD_IDENTITY),
        "the workload identity is {identity:?}"
    );
    // State persists across requests, while working directories and environments do not, and a
    // direct-mode guest has no workload machine ID.
    let second = guest.exec(
        &mut session,
        &[
            "/bin/sh",
            "-c",
            "pwd; test -f /tmp/nvx-managed-state; test \"${COMPLEX-unset}\" = unset; \
             test ! -e /run/nvx/workload-machine-id; printf second-exec",
        ],
        5_000,
        None,
        WorkloadEnvironment::Default,
    );
    assert!(
        second.printed("/\nsecond-exec"),
        "managed workload state did not survive across requests: {second:?}"
    );
    // A workload that outlives its timeout is ended, and the guest stays usable.
    let timed_out = guest.exec(
        &mut session,
        &["/bin/sleep", "5"],
        100,
        None,
        WorkloadEnvironment::Default,
    );
    assert_eq!(
        timed_out.exit,
        Some((ExitCategory::Timeout, 124)),
        "{timed_out:?}"
    );
    let after_timeout = guest.exec(
        &mut session,
        &["/bin/sh", "-c", "printf still-usable"],
        5_000,
        None,
        WorkloadEnvironment::Default,
    );
    assert!(
        after_timeout.printed("still-usable"),
        "the guest was not usable after a timeout: {after_timeout:?}"
    );
    // The guest refuses a working directory that the workload cannot enter, with its error
    // number and a diagnostic that names it, before anything of the workload runs.
    for (description, cwd, error) in REFUSED_CWDS {
        let refused = guest.exec(
            &mut session,
            &["/bin/touch", REFUSED_CWD_MARKER],
            5_000,
            Some(cwd),
            WorkloadEnvironment::Default,
        );
        let diagnostic = String::from_utf8_lossy(&refused.stderr);
        assert!(
            refused.exit.is_none()
                && refused.refusal == Some((error, CWD_FAILED.to_owned()))
                && refused.stdout.is_empty()
                && diagnostic.contains(&format!("cannot enter working directory {cwd}: ")),
            "the {description} working directory {cwd} was not refused clearly: {refused:?} \
             ({diagnostic})"
        );
    }
    let after_refusals = guest.exec(
        &mut session,
        &[
            "/bin/sh",
            "-c",
            &format!("test ! -e {REFUSED_CWD_MARKER} && printf still-usable"),
        ],
        5_000,
        None,
        WorkloadEnvironment::Default,
    );
    assert!(
        after_refusals.printed("still-usable"),
        "a refused workload ran, or the guest was not usable after refusals: {after_refusals:?}"
    );

    // Stop ends the VM, and OpenVMM exits successfully once it has written its report.
    session
        .stop(Instant::now() + timeout)
        .expect("the guest agent acknowledges stop");
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = openvmm.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "OpenVMM did not exit after stop");
        thread::sleep(Duration::from_millis(50));
    };
    console
        .join()
        .expect("the guest console was copied until its end");
    assert!(
        status.success(),
        "the managed OpenVMM process exited with {status}"
    );
}
