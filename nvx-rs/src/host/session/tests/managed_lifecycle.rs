//! The managed lifecycle of a guest whose init hands the boot to the managed agent, on a real
//! hypervisor: authenticated sessions and reconnects, request-scoped working directories and
//! environments, the workload identity, timeouts, descendants that outlive their workload,
//! refused working directories, and stop, driven through the host's control session.
//!
//! `scripts/nvx.py test-microvm --scenario managed-lifecycle` runs it for the selected guest and
//! backend, then checks the outcome report that OpenVMM wrote and the guest console. It sets:
//!
//! - `NVX_MANAGED_LIFECYCLE_COMMAND`: OpenVMM's command line, as a JSON array of strings, with the
//!   managed lifecycle, the control console, `--microvm-control-auth-stdin`, and the report.
//! - `NVX_MANAGED_LIFECYCLE_ENDPOINT`: the control endpoint that the command listens on.
//! - `NVX_MANAGED_LIFECYCLE_CONSOLE`: the endpoint at which the command's guest console listens:
//!   a Unix socket on Linux, and a named pipe on Windows.
//! - `NVX_MANAGED_LIFECYCLE_TIMEOUT`: the longest wait for each operation, in seconds.
//! - `NVX_MANAGED_LIFECYCLE_OUTPUT_DIR`: receives OpenVMM's output, `managed-lifecycle.log`, and
//!   the guest console, `managed-lifecycle-guest.log`, which the scenario then scans.
//! - `NVX_LAYERED_INIT_MAPS`, only for the dedicated real layered-init fixture: `0` for a live
//!   share without an export, or `2` for the retained host export and an ordered mapping table.
//!   That fixture boots the actual `nvx-init` as PID 1 with CLI identity `1000:1001`.
//!
//! The test also records OpenVMM's process ID in `NVX_ADVERSARIAL_OPENVMM_PID_JOURNAL`, if set,
//! as the microVM scenarios do for the adversarial campaigns.
//!
//! These checks were `run_managed_lifecycle` of `scripts/nvx_tools/microvm_tests.py`, which drove
//! the control session from Python until microsoft/nvx#359 left the protocol to nvx-rs alone.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::host::platform::{Transport, process_start_time};
use crate::host::session::{self, ControlSession, ExecEvent};
use crate::protocol::{
    self, APP_ERROR, APP_MAPS, APP_READY, CAPABILITY_LEN, ExitCategory, HostMap, Workload,
    WorkloadEnvironment,
};

/// The categories with which the guest refuses a working directory that the workload cannot
/// enter, a malformed request, and a workload before it applied every host mapping: NVXC's wire
/// values, which the guest's own constants hold, but which the `cli` feature alone, with which
/// this test runs, does not build.
const CWD_FAILED: &str = "cwd-failed";
/// See [`CWD_FAILED`].
const INVALID_REQUEST: &str = "invalid-request";
/// See [`CWD_FAILED`].
const MAPPINGS_INCOMPLETE: &str = "mappings-incomplete";

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

/// Copies a guest console into `log` until its end, which a reset, as OpenVMM's exit may cause,
/// also marks. Any other error, such as a log on a full disk, fails the copy, since the scenario
/// would then scan an incomplete log.
fn copy_console(console: &mut impl io::Read, log: &mut impl Write) -> io::Result<()> {
    match io::copy(console, log) {
        Err(error)
            if !matches!(
                error.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
            ) =>
        {
            Err(error)
        }
        _ => Ok(()),
    }
}

#[test]
fn a_console_copy_ends_at_a_reset_and_fails_on_other_errors() {
    /// A console that prints `output`, then fails with `end`.
    struct Console {
        output: &'static [u8],
        end: io::ErrorKind,
    }

    impl io::Read for Console {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.output.is_empty() {
                return Err(self.end.into());
            }
            let length = buffer.len().min(self.output.len());
            buffer[..length].copy_from_slice(&self.output[..length]);
            self.output = &self.output[length..];
            Ok(length)
        }
    }

    /// A log on a full disk.
    struct Full;

    impl Write for Full {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::StorageFull.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    for end in [
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::ConnectionAborted,
    ] {
        let mut log = Vec::new();
        let mut console = Console {
            output: b"NVX-GUEST-BOOT-OK\n",
            end,
        };
        copy_console(&mut console, &mut log).expect("a reset ends the console");
        assert_eq!(log, b"NVX-GUEST-BOOT-OK\n");
    }
    let mut console = Console {
        output: b"",
        end: io::ErrorKind::TimedOut,
    };
    let error = copy_console(&mut console, &mut Vec::new()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    let mut console = Console {
        output: b"NVX-GUEST-BOOT-OK\n",
        end: io::ErrorKind::ConnectionReset,
    };
    let error = copy_console(&mut console, &mut Full).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::StorageFull);
}

/// The guest console as a reader: each read waits for what the guest prints, and reads nothing
/// once OpenVMM closes the console.
struct Console(Box<dyn Transport>);

impl io::Read for Console {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer, None)
    }
}

/// Copies the guest console that OpenVMM process `pid`, which started at `start_time`, serves at
/// `endpoint` into `path` until OpenVMM closes it, and returns the copy and a receiver that it
/// signals once it has connected.
///
/// The console of a microVM listens on a Unix socket or a named pipe, never on TCP, which guests
/// could reach through the host-loopback mapping of their gateways (microsoft/nvx#551). The copy
/// connects as soon as OpenVMM listens, as the scenario's console monitor did when it drove the
/// session from Python, and only to OpenVMM itself.
fn capture_console(
    endpoint: String,
    (pid, start_time): (u32, u64),
    path: PathBuf,
    deadline: Instant,
) -> (thread::JoinHandle<()>, mpsc::Receiver<()>) {
    let (connected, connection) = mpsc::sync_channel(1);
    let copy = thread::spawn(move || {
        let mut console = Console(
            session::connect(&endpoint, pid, start_time, deadline).unwrap_or_else(|error| {
                panic!("the guest console at {endpoint} did not accept a connection: {error}")
            }),
        );
        // The test stops waiting for the connection only when it fails.
        let _ = connected.send(());
        let mut log = File::create(&path).expect("the guest console log opens");
        copy_console(&mut console, &mut log).unwrap_or_else(|error| {
            panic!(
                "the guest console was not copied to {}: {error}",
                path.display()
            )
        });
    });
    (copy, connection)
}

/// Waits until the console copy signals on `connection` that it has connected, while OpenVMM
/// runs. The guest runs until the test stops it, so the copy cannot then miss a lifecycle that
/// ends before it connects.
fn await_console(connection: &mpsc::Receiver<()>, openvmm: &mut OpenVmm, deadline: Instant) {
    loop {
        match connection.recv_timeout(Duration::from_millis(25)) {
            Ok(()) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("the guest console copy ended before it connected")
            }
        }
        if let Some(status) = openvmm.0.try_wait().unwrap() {
            panic!("OpenVMM exited with {status} before its guest console accepted a connection");
        }
        assert!(
            Instant::now() < deadline,
            "the guest console did not accept a connection in time"
        );
    }
}

/// Records OpenVMM's process ID for an adversarial campaign in the journal that
/// `NVX_ADVERSARIAL_OPENVMM_PID_JOURNAL` names, if it is set, as `record_adversarial_openvmm_pid`
/// of the microVM tests does.
fn record_pid(pid: u32) {
    if let Some(journal) = env::var_os("NVX_ADVERSARIAL_OPENVMM_PID_JOURNAL") {
        append_pid(Path::new(&journal), pid);
    }
}

/// Appends an entry for `pid` to a process ID journal and syncs it. Each entry is a line,
/// `{"pid":PID,"recorded_at_ns":NS}`, which `_read_openvmm_pids` of
/// `scripts/nvx_tools/adversarial_executor.py` reads to check that OpenVMM exited.
fn append_pid(journal: &Path, pid: u32) {
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
fn the_process_id_journal_has_the_entries_that_the_adversarial_executor_reads() {
    let directory = tempfile::tempdir().unwrap();
    let journal = directory.path().join("openvmm-pids.jsonl");
    append_pid(&journal, 4321);
    append_pid(&journal, u32::MAX);
    let text = fs::read_to_string(&journal).unwrap();
    assert!(
        text.ends_with('\n') && !text.contains('\r'),
        "each entry ends with a line feed: {text:?}"
    );
    let pids: Vec<u64> = text
        .lines()
        .map(|line| {
            let entry: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(line).expect("an entry is a JSON object");
            assert_eq!(
                entry.keys().collect::<Vec<_>>(),
                ["pid", "recorded_at_ns"],
                "an entry has exactly these properties: {line}"
            );
            let recorded_at_ns = entry["recorded_at_ns"]
                .as_u64()
                .expect("the time is an integer");
            assert!(
                (1..=i64::MAX.unsigned_abs()).contains(&recorded_at_ns),
                "the time fits the executor's range: {line}"
            );
            entry["pid"].as_u64().expect("the process ID is an integer")
        })
        .collect();
    assert_eq!(pids, [4321, u64::from(u32::MAX)]);
}

fn layered_mapping_session(
    guest: &Guest,
    mut session: ControlSession,
    count: u32,
) -> ControlSession {
    assert!(
        matches!(count, 0 | 2),
        "the layered fixture has zero or two mappings"
    );
    let maps = ["/workspace", "/run/task-state"].map(|target| HostMap {
        source: "0".to_owned(),
        target: target.to_owned(),
        read_only: true,
    });
    let parts = protocol::encode_maps_payloads(&maps, 40).unwrap();
    assert_eq!(parts.len(), 2);
    let send = |session: &mut ControlSession, payload: &[u8], accepted| {
        let deadline = Instant::now() + guest.timeout;
        session
            .send_request(APP_MAPS, 900, payload, deadline)
            .unwrap();
        let expected = if accepted {
            (APP_READY, 900, 0, Vec::new())
        } else {
            (APP_ERROR, 900, 22, INVALID_REQUEST.as_bytes().to_vec())
        };
        assert_eq!(session.read_response(Some(deadline)).unwrap(), expected);
    };
    if count == 0 {
        send(&mut session, &parts[0], false);
    } else {
        let incomplete = |session: &mut ControlSession| {
            let outcome = guest.exec(
                session,
                &["/bin/true"],
                5_000,
                None,
                WorkloadEnvironment::Default,
            );
            assert_eq!(outcome.refusal, Some((11, MAPPINGS_INCOMPLETE.to_owned())));
            assert!(outcome.exit.is_none());
        };
        incomplete(&mut session);
        send(&mut session, &parts[1], false);
        send(&mut session, &parts[0], true);
        drop(session);
        session = guest.connect();
        incomplete(&mut session);
        send(&mut session, &parts[1], true);
    }
    drop(session);
    session = guest.connect();
    session.ping(Instant::now() + guest.timeout).unwrap();
    send(&mut session, &parts[0], false);
    let defaults = guest.exec(
        &mut session,
        &[
            "/bin/sh",
            "-c",
            "test \"$(cat /proc/1/comm)\" = nvx-init && \
             test \"$(cat /proc/sys/kernel/hostname)\" = sl76b-real-root && \
             test -s /run/nvx/time/state && test \"$(wc -c </etc/machine-id)\" -eq 33 && \
             test ! -e /run/nvx/hostfs/root && \
             printf '%s:%s|%s|%s|%s' \"$(id -u)\" \"$(id -g)\" \"$USER\" \"$HOME\" \"$PWD\"",
        ],
        5_000,
        None,
        WorkloadEnvironment::Default,
    );
    assert!(defaults.printed("1000:1001|nvx|/tmp|/"), "{defaults:?}");
    let targets: &[&str] = if count == 0 {
        &["/run/app/data"]
    } else {
        &["/workspace/data", "/run/task-state/data"]
    };
    for path in targets {
        let mapped = guest.exec(
            &mut session,
            &["/bin/cat", path],
            5_000,
            None,
            WorkloadEnvironment::Default,
        );
        assert!(mapped.printed("sl76b-real-export\n"), "{path}: {mapped:?}");
    }
    let request = session
        .start_exec(
            &Workload {
                argv: vec!["/bin/sleep".to_owned(), "30".to_owned()],
                timeout_ms: 0,
                cwd: None,
                environment: WorkloadEnvironment::Default,
            },
            Instant::now() + guest.timeout,
        )
        .unwrap();
    session
        .cancel_exec(request, Instant::now() + guest.timeout)
        .unwrap();
    assert!(matches!(
        session
            .next_exec_event(request, Some(Instant::now() + guest.timeout))
            .unwrap(),
        ExecEvent::Exit {
            category: ExitCategory::Cancelled,
            status: 137
        }
    ));
    session
}

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-microvm \
            --scenario managed-lifecycle"]
fn a_managed_guest_runs_request_scoped_workloads_until_it_stops() {
    let layered_maps = env::var_os("NVX_LAYERED_INIT_MAPS").map(|value| {
        value
            .into_string()
            .expect("NVX_LAYERED_INIT_MAPS is ASCII")
            .parse::<u32>()
            .expect("NVX_LAYERED_INIT_MAPS is 0 or 2")
    });
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
    let mut capability = [0; CAPABILITY_LEN];
    getrandom::fill(&mut capability).unwrap();
    // OpenVMM reads its capability as soon as it starts, so its input already holds the
    // capability and its end, as `nvx::host::supervisor::spawn` prepares it.
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
    let start_time = process_start_time(pid)
        .unwrap()
        .expect("OpenVMM runs after it started");
    let (console, console_connection) = capture_console(
        variable("NVX_MANAGED_LIFECYCLE_CONSOLE"),
        (pid, start_time),
        output_dir.join("managed-lifecycle-guest.log"),
        Instant::now() + timeout,
    );
    await_console(&console_connection, &mut openvmm, Instant::now() + timeout);
    let guest = Guest {
        endpoint: variable("NVX_MANAGED_LIFECYCLE_ENDPOINT"),
        pid,
        start_time,
        capability,
        timeout,
    };

    // The first session finds the guest agent ready, and a workload runs in its own working
    // directory with exactly the environment that it asks for, an empty value included.
    let mut session = guest.connect();
    session
        .ping(Instant::now() + timeout)
        .expect("the guest agent is ready");
    if let Some(count) = layered_maps {
        session = layered_mapping_session(&guest, session, count);
    }
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
        identity.printed(if layered_maps.is_some() {
            "1000:1001"
        } else {
            WORKLOAD_IDENTITY
        }),
        "the workload identity is {identity:?}"
    );
    // State persists across requests, while working directories and environments do not, and a
    // direct-mode guest has no workload machine ID: the marker prints only if every check passes.
    let second = guest.exec(
        &mut session,
        &[
            "/bin/sh",
            "-c",
            "pwd && test -f /tmp/nvx-managed-state && test \"${COMPLEX-unset}\" = unset \
             && test ! -e /run/nvx/workload-machine-id && printf second-exec",
        ],
        5_000,
        None,
        WorkloadEnvironment::Default,
    );
    assert!(
        second.printed("/\nsecond-exec"),
        "a second request lost the workload state, kept the first one's environment, or found a \
         workload machine ID: {second:?}"
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
    // The core ends descendants in the original process group when its leader exits, so their
    // output closes and the next request can run without waiting for the descendant's sleep.
    let started = Instant::now();
    let detached = guest.exec(
        &mut session,
        &["/bin/sh", "-c", "sleep 30 & printf started"],
        5_000,
        None,
        WorkloadEnvironment::Default,
    );
    let elapsed = started.elapsed();
    assert!(
        detached.printed("started") && elapsed < Duration::from_secs(15),
        "a workload whose descendant held its output ended with {detached:?} after {elapsed:?}"
    );
    let after_detached = guest.exec(
        &mut session,
        &["/bin/sh", "-c", "printf still-usable"],
        5_000,
        None,
        WorkloadEnvironment::Default,
    );
    assert!(
        after_detached.printed("still-usable"),
        "the guest did not end a workload's process group: {after_detached:?}"
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
    if let Some(count) = layered_maps {
        let output = [
            fs::read(output_dir.join("managed-lifecycle-guest.log")).unwrap(),
            fs::read(output_dir.join("managed-lifecycle.log")).unwrap(),
        ]
        .concat();
        let marker = format!("NVX-INIT-ROOT: switched mappings={count}");
        assert!(
            output
                .windows(marker.len())
                .any(|bytes| bytes == marker.as_bytes())
        );
        assert!(
            !output
                .windows(b"NVX-INIT-ERROR:".len())
                .any(|bytes| bytes == b"NVX-INIT-ERROR:")
        );
    }
}
