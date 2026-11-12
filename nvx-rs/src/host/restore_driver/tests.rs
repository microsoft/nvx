#![cfg(any(target_os = "linux", windows))]

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

use super::*;
use crate::activation::{
    Configuration, ExpectedResources, Export, ExportKind, Image, MachineIdMode, OverlayMode,
    ReadyActivation, RestoreSection, TransactionId, Upper, View,
};
use crate::host::process::Opened;
use crate::host::session::ExecEvent;
use crate::protocol::WorkloadEnvironment;
use crate::protocol::v2::Outcome;

/// A stand-in for OpenVMM and its guest, which serves the control endpoint, sends the
/// restore-ready event, attaches the host with the capability of its standard input, and answers
/// NVXC requests of version 2 as `case` asks, with `activation` in its Ready. It records each
/// request in `trace`, and the step at which it stalls in `stage`.
const PEER: &str = r#"
import json, os, socket, struct, sys, time
case, endpoint, ready, trace, stage, activation = sys.argv[1:]
capability = sys.stdin.buffer.read()
def stall(value):
    with open(stage, 'w', encoding='ascii') as out: out.write(value)
    time.sleep(3600)
    sys.exit(0)
if os.name == 'nt':
    import ctypes, msvcrt
    from ctypes import wintypes
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.CreateNamedPipeW.argtypes = [wintypes.LPCWSTR] + [wintypes.DWORD] * 6 + [ctypes.c_void_p]
    kernel.CreateNamedPipeW.restype = wintypes.HANDLE
    kernel.ConnectNamedPipe.argtypes = [wintypes.HANDLE, ctypes.c_void_p]
    kernel.ConnectNamedPipe.restype = wintypes.BOOL
    kernel.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                                  ctypes.c_void_p, wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
    kernel.CreateFileW.restype = wintypes.HANDLE
    def owned(handle):
        if handle == ctypes.c_void_p(-1).value: raise ctypes.WinError(ctypes.get_last_error())
        return os.fdopen(msvcrt.open_osfhandle(handle, os.O_BINARY), 'r+b', buffering=0)
    def bind():
        # The first instance of a duplex pipe for local clients, with small buffers.
        handle = kernel.CreateNamedPipeW(endpoint.replace('/', '\\'), 0x80003, 8, 1, 4096, 4096,
                                         0, None)
        return handle, owned(handle)
    def accept(server):
        handle, stream = server
        if not kernel.ConnectNamedPipe(handle, None) and ctypes.get_last_error() != 535:
            raise ctypes.WinError(ctypes.get_last_error())
        return stream
    def send_ready(data):
        stream = owned(kernel.CreateFileW(ready.replace('/', '\\'), 0x40000000, 0, None, 3, 0,
                                          None))
        stream.write(data)
        return stream
else:
    def bind():
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(endpoint)
        listener.listen()
        return listener
    def accept(server):
        return server.accept()[0].makefile('rwb', buffering=0)
    def send_ready(data):
        peer = socket.socket(socket.AF_UNIX)
        peer.connect(ready)
        peer.sendall(data)
        return peer
def exact(stream, count):
    data = b''
    while len(data) < count:
        part = stream.read(count - len(data))
        if not part: raise EOFError()
        data += part
    return data
def record(stream, kind, epoch, sequence, payload=b''):
    stream.write(struct.pack('<4sHBB16sQQI', b'NVXS', 1, kind, 0, bytes([7]) * 16, epoch,
                             sequence, len(payload)) + payload)
def reply(stream, kind, rid, status, payload, epoch, sequence):
    frame = struct.pack('<4sBBHQiI', b'NVXC', 2, kind, 0, rid, status, len(payload)) + payload
    record(stream, 5, epoch, sequence, frame)
server = None if case == 'connect' else bind()
if case == 'stall':
    held = accept(server)
    stall(case)
if case == 'ready-accept': stall(case)
if case == 'ready-read':
    held = send_ready(b'O')
    stall(case)
if case != 'control-only': send_ready(b'OPENVMM_RESTORE_READY_V1\n').close()
if case == 'connect': stall(case)
for session in range(1, 4):
    if os.name == 'nt' and session > 1: server = bind()
    stream = accept(server)
    try:
        header = exact(stream, 44)
        assert header[:4] == b'NVXS' and header[6] == 2
        assert exact(stream, struct.unpack('<I', header[40:])[0]) == capability
        if case == 'attach-read': stall(case)
        record(stream, 7, session, 0)
        sequence = 1
        while True:
            header = exact(stream, 44)
            frame = exact(stream, struct.unpack('<I', header[40:])[0])
            assert header[:4] == b'NVXS' and header[6] == 5 and frame[:5] == b'NVXC\x02'
            kind, rid, payload = frame[5], struct.unpack('<Q', frame[8:16])[0], frame[24:]
            with open(trace, 'a', encoding='ascii') as out:
                out.write(json.dumps({'session': session, 'kind': kind, 'id': rid,
                                      'payload': payload.hex()}) + '\n')
            if kind == 8:
                if case == 'activate-read': stall(case)
                if case == 'session-limit' or (case == 'reconnect' and session < 3):
                    record(stream, 3, session + 100, sequence)
                    stream.close()
                    break
                if case == 'malformed':
                    reply(stream, 0x81, rid, 0, b'\x02\x00', session, sequence)
                    stall(case)
                reply(stream, 0x81, rid, 0, b'\x02' + bytes.fromhex(activation), session,
                      sequence)
            elif kind == 6:
                if case == 'maps-read': stall(case)
                if case == 'maps-error':
                    reply(stream, 0xff, rid, 5, b'mapping-failed', session, sequence)
                    stall(case)
                reply(stream, 0x81, rid, 0, b'\x02', session, sequence)
            elif kind == 2:
                reply(stream, 0x82, rid, 0, b'restored', session, sequence)
                reply(stream, 0x84, rid, 0, b'exit', session, sequence + 1)
                stall('started')
            else:
                raise AssertionError('unexpected request')
            sequence += 1
    except EOFError:
        stall('closed')
stall('limit')
"#;

/// Returns the Python interpreter on `PATH`: `python3` on Linux, and on Windows `python.exe`
/// outside `WindowsApps`, whose aliases start the interpreter as another process.
fn python() -> PathBuf {
    let name = if cfg!(windows) {
        "python.exe"
    } else {
        "python3"
    };
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join(name))
        .find(|path| path.is_file() && !path.to_string_lossy().contains("WindowsApps"))
        .expect("Python is on PATH")
}

fn record() -> ActivationRecord {
    ActivationRecord::new(
        Some(RestoreSection {
            transaction: TransactionId::from_bytes([1; 16]),
        }),
        Configuration {
            image: Image::Erofs { uuid: None },
            upper: Upper::Tmpfs {
                max_memory_percent: 50,
            },
            overlay: OverlayMode::TrustedLowers,
            export: Some(Export {
                kind: ExportKind::Aggregate,
            }),
            mappings: 1,
            network: None,
            view: View {
                hostname: b"restore-test".to_vec(),
                machine_id: MachineIdMode::Generated,
            },
            resources: ExpectedResources {
                cpus: 2,
                memory: 1 << 30,
            },
        },
    )
    .unwrap()
}

fn workload() -> Workload<'static> {
    Workload {
        identity: WorkloadIdentity {
            uid: 65534,
            gid: 65534,
            create_account: false,
        },
        exec: protocol::Workload {
            argv: vec!["/bin/true".to_owned()],
            timeout_ms: 0,
            cwd: None,
            environment: WorkloadEnvironment::Default,
        },
        request_id: 19,
    }
}

fn maps() -> Vec<HostMap> {
    vec![HostMap {
        source: "entry".to_owned(),
        target: "/mapped".to_owned(),
        read_only: true,
    }]
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Holds `process` again, independently of its launch.
fn reopen(process: &Process) -> Process {
    let Opened::Running(process) = Process::open(process.reference()).unwrap() else {
        panic!("the process runs");
    };
    process
}

/// Waits until a [`PEER`] whose stage file is `file` stalls at `stage`.
fn wait_for(file: &Path, stage: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while fs::read_to_string(file).ok().as_deref() != Some(stage) {
        assert!(Instant::now() < deadline, "the peer did not reach {stage}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// A launch whose VM is a stand-in that the test ends when it drops the launch.
struct Vm {
    launched: Launched,
    directory: tempfile::TempDir,
}

impl Vm {
    /// Spawns `command`, or the stand-in VMM without one, as a VM launched in a directory of its
    /// own, and binds its restore-ready listener first when `restore` asks for one.
    fn spawn(
        restore: bool,
        command: impl FnOnce(&Path, &VmOutputs, &str) -> Vec<OsString>,
    ) -> Self {
        let (directory, vm) = crate::host::vm_directory::tests::vm_dir();
        let outputs = vm.outputs(false).unwrap();
        let ready = restore.then(|| ReadyListener::bind(&vm).unwrap());
        let endpoint = ready.as_ref().map_or("", |ready| ready.endpoint());
        let arguments = command(directory.path(), &outputs, endpoint);
        let command = (!arguments.is_empty()).then(|| (python(), arguments));
        Self {
            launched: Launched::stand_in(&vm, outputs, ready, command),
            directory,
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.launched.process.kill();
        let _ = self
            .launched
            .process
            .wait(Instant::now() + Duration::from_secs(10));
    }
}

/// A [`PEER`], launched as a restored VM.
struct Peer {
    vm: Vm,
    stage: PathBuf,
    trace: PathBuf,
}

impl Peer {
    fn start(case: &str) -> Self {
        let vm = Vm::spawn(true, |directory, outputs, ready| {
            let activation = hex(&ReadyActivation::of(&record()).encode());
            ["-c", PEER, case, &outputs.endpoint, ready]
                .map(OsString::from)
                .into_iter()
                .chain(["trace", "stage"].map(|name| directory.join(name).into_os_string()))
                .chain([OsString::from(activation)])
                .collect()
        });
        Self {
            stage: vm.directory.path().join("stage"),
            trace: vm.directory.path().join("trace"),
            vm,
        }
    }

    /// Returns each NVXC request that the peer received, in order.
    fn requests(&self) -> Vec<serde_json::Value> {
        fs::read_to_string(&self.trace)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Returns the kinds of [`requests`](Self::requests).
    fn kinds(&self) -> Vec<u64> {
        self.requests()
            .iter()
            .map(|request| request["kind"].as_u64().unwrap())
            .collect()
    }
}

fn machine(error: ControlRestoreError) -> RestoreError {
    match error {
        ControlRestoreError::Machine(error) => error,
        error => panic!("not a failure of the machine: {error:?}"),
    }
}

#[test]
fn a_lost_session_is_replaced_with_the_same_activate_message_and_the_live_session_returns() {
    let mut peer = Peer::start("reconnect");
    let cancel = Cancel::new(false);
    let workload = workload();
    let mut session = restore(
        &mut peer.vm.launched,
        &record(),
        &workload,
        &maps(),
        &cancel,
    )
    .unwrap();
    // The returned session no longer observes the restore's cancellation.
    cancel.store(true, Ordering::Release);
    let deadline = Some(Instant::now() + Duration::from_secs(5));
    assert_eq!(
        session
            .next_exec_event(workload.request_id, deadline)
            .unwrap(),
        ExecEvent::Stdout(b"restored".to_vec())
    );
    assert_eq!(
        session
            .next_exec_event(workload.request_id, deadline)
            .unwrap(),
        ExecEvent::Exit {
            category: Outcome::Exit,
            status: 0
        }
    );
    let requests = peer.requests();
    let activate = hex(&protocol::encode_activate_payload(&record(), &workload.identity).unwrap());
    let sessions: Vec<_> = requests
        .iter()
        .filter(|request| request["kind"] == 8)
        .map(|request| {
            assert_eq!(request["payload"], activate.as_str());
            request["session"].as_u64().unwrap()
        })
        .collect();
    assert_eq!(sessions, [1, 2, 3]);
    assert_eq!(peer.kinds(), [8, 8, 8, 6, 2]);
    assert_eq!(requests[4]["id"], workload.request_id);
    assert!(!peer.vm.launched.process.has_exited().unwrap());
    assert!(peer.vm.launched.ready.is_some());
}

#[test]
fn a_third_lost_session_and_a_malformed_ready_are_final() {
    for (case, phase, evidence, requests) in [
        (
            "session-limit",
            RestorePhase::Activate,
            RestoreEvidence::Reset,
            3,
        ),
        (
            "malformed",
            RestorePhase::Ready,
            RestoreEvidence::Malformed,
            1,
        ),
    ] {
        let mut peer = Peer::start(case);
        let error = machine(
            restore(
                &mut peer.vm.launched,
                &record(),
                &workload(),
                &maps(),
                &Cancel::new(false),
            )
            .err()
            .unwrap(),
        );
        assert_eq!(error.phase, phase, "{case}");
        assert_eq!(error.evidence, evidence, "{case}");
        assert!(error.guest_may_have_run, "{case}");
        assert_eq!(peer.requests().len(), requests, "{case}");
        assert!(!peer.vm.launched.process.has_exited().unwrap(), "{case}");
    }
}

#[test]
fn neither_an_unverified_restore_ready_client_nor_control_server_gets_the_activation() {
    for ready_mismatch in [true, false] {
        let mut peer = Peer::start(if ready_mismatch {
            "reconnect"
        } else {
            "connect"
        });
        let mut other = Peer::start("control-only");
        if ready_mismatch {
            std::mem::swap(
                &mut peer.vm.launched.process,
                &mut other.vm.launched.process,
            );
        } else {
            peer.vm.launched.outputs.endpoint = other.vm.launched.outputs.endpoint.clone();
        }
        let error = machine(
            restore(
                &mut peer.vm.launched,
                &record(),
                &workload(),
                &maps(),
                &Cancel::new(false),
            )
            .err()
            .unwrap(),
        );
        assert_eq!(error.evidence, RestoreEvidence::PeerMismatch);
        assert_eq!(
            error.phase,
            if ready_mismatch {
                RestorePhase::ReadyEvent
            } else {
                RestorePhase::Connect
            }
        );
        assert!(peer.requests().is_empty() && other.requests().is_empty());
    }
}

/// How a test ends a restore's pending operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Deadline,
    Cancel,
    Exit,
}

/// The cases of [`PEER`] that stall a restore's pending accept or read, with the phase that
/// fails, or none for the mapping table.
const STALLS: [(&str, Option<RestorePhase>); 6] = [
    ("ready-accept", Some(RestorePhase::ReadyEvent)),
    ("ready-read", Some(RestorePhase::ReadyEvent)),
    ("connect", Some(RestorePhase::Connect)),
    ("attach-read", Some(RestorePhase::Connect)),
    ("activate-read", Some(RestorePhase::Activate)),
    ("maps-read", None),
];

/// Restores against a peer that stalls at `case`, ends the restore as `end` says once the peer
/// stalls, and checks how and when the restore fails.
fn check_ended(case: &str, phase: Option<RestorePhase>, end: End) {
    let mut peer = Peer::start(case);
    let cancel = Cancel::new(false);
    let stage = peer.stage.clone();
    let vm = reopen(&peer.vm.launched.process);
    let began = Instant::now();
    let error = thread::scope(|scope| {
        if end != End::Deadline {
            let cancel = &cancel;
            scope.spawn(move || {
                wait_for(&stage, case);
                thread::sleep(Duration::from_millis(50));
                if end == End::Cancel {
                    cancel.store(true, Ordering::Release);
                } else {
                    vm.kill().unwrap();
                }
            });
        }
        restore(
            &mut peer.vm.launched,
            &record(),
            &workload(),
            &maps(),
            &cancel,
        )
        .err()
        .unwrap()
    });
    let elapsed = began.elapsed();
    if end == End::Deadline {
        // Ready has 60 seconds from the call, and the mapping table and Start 30 from Ready.
        let budget = Duration::from_secs(if phase.is_some() { 60 } else { 30 });
        assert!(
            budget <= elapsed && elapsed < budget + Duration::from_secs(30),
            "{case}: {elapsed:?}, {error:?}"
        );
    } else {
        assert!(
            elapsed < Duration::from_secs(10),
            "{case}/{end:?}: {elapsed:?}, {error:?}"
        );
    }
    match (phase, error) {
        (Some(phase), ControlRestoreError::Machine(error)) => {
            assert_eq!(error.phase, phase, "{case}/{end:?}");
            let evidence = match end {
                End::Deadline => RestoreEvidence::Expired,
                End::Cancel => RestoreEvidence::Cancelled,
                End::Exit => RestoreEvidence::Exited,
            };
            assert_eq!(error.evidence, evidence, "{case}/{end:?}");
        }
        (None, ControlRestoreError::Maps(error)) => match (end, error) {
            (End::Deadline, SessionError::TimedOut) | (End::Exit, SessionError::ProcessExited) => {}
            (End::Cancel, SessionError::Io(error))
                if error.kind() == io::ErrorKind::Interrupted => {}
            (end, error) => panic!("{case}/{end:?}: {error:?}"),
        },
        (_, error) => panic!("{case}/{end:?}: {error:?}"),
    }
    if end != End::Exit {
        assert!(!peer.vm.launched.process.has_exited().unwrap(), "{case}");
    }
}

#[test]
fn ready_has_sixty_seconds_and_start_thirty_from_ready_for_each_pending_accept_and_read() {
    // The cases wait out their budgets together.
    thread::scope(|scope| {
        for (case, phase) in STALLS {
            scope.spawn(move || check_ended(case, phase, End::Deadline));
        }
    });
}

#[test]
fn cancellation_ends_each_pending_accept_and_read() {
    for (case, phase) in STALLS {
        check_ended(case, phase, End::Cancel);
    }
}

#[test]
fn the_verified_exit_of_the_vm_ends_each_pending_accept_and_read() {
    for (case, phase) in STALLS {
        check_ended(case, phase, End::Exit);
    }
}

#[test]
fn the_deadline_cancellation_and_the_verified_exit_end_a_pending_write() {
    for end in [End::Deadline, End::Cancel, End::Exit] {
        // The peer never reads what the restore writes, so a large write stays pending.
        let peer = Peer::start("stall");
        let vm = Vm::spawn(false, |_, _, _| Vec::new());
        let process = &vm.launched.process;
        let cancel = Cancel::new(false);
        let watch = Watch {
            process,
            cancel: &cancel,
            interrupt: OnceLock::new(),
            connection: Mutex::new(None),
        };
        let server = peer.vm.launched.process.reference();
        let mut transport = session::connect(
            &peer.vm.launched.outputs.endpoint,
            server.pid,
            server.start_time,
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        watch.register(&*transport).unwrap();
        let timeout = Duration::from_millis(if end == End::Deadline { 500 } else { 20_000 });
        let began = Instant::now();
        let error = thread::scope(|scope| {
            let (_running, stopped) = mpsc::channel::<()>();
            scope.spawn(|| watch.run(stopped));
            if end != End::Deadline {
                let (stage, cancel) = (&peer.stage, &cancel);
                scope.spawn(move || {
                    wait_for(stage, "stall");
                    thread::sleep(Duration::from_millis(200));
                    if end == End::Cancel {
                        cancel.store(true, Ordering::Release);
                    } else {
                        process.kill().unwrap();
                    }
                });
            }
            transport
                .write_all(&vec![7; 16 << 20], Some(timeout))
                .unwrap_err()
        });
        let elapsed = began.elapsed();
        let interrupt = match end {
            End::Deadline => {
                assert_eq!(error.kind(), io::ErrorKind::TimedOut);
                assert!(elapsed >= timeout, "{elapsed:?}");
                None
            }
            End::Cancel => Some(Interrupt::Cancelled),
            End::Exit => Some(Interrupt::Exited),
        };
        assert!(elapsed < Duration::from_secs(10), "{end:?}: {elapsed:?}");
        assert_eq!(watch.interrupted(), interrupt);
    }
}

#[test]
fn mapping_and_start_failures_are_told_apart_from_the_machines() {
    for case in ["maps-error", "start-error", "maps-count"] {
        let mut peer = Peer::start(case);
        let mut workload = workload();
        if case == "start-error" {
            // EXEC refuses a workload without a program.
            workload.exec.argv.clear();
        }
        let maps = if case == "maps-count" {
            Vec::new()
        } else {
            maps()
        };
        let error = restore(
            &mut peer.vm.launched,
            &record(),
            &workload,
            &maps,
            &Cancel::new(false),
        )
        .err()
        .unwrap();
        match (case, &error) {
            ("maps-error" | "maps-count", ControlRestoreError::Maps(SessionError::Protocol(_)))
            | ("start-error", ControlRestoreError::Start(SessionError::Protocol(_))) => {}
            (case, error) => panic!("{case}: {error:?}"),
        }
        assert!(!peer.kinds().contains(&2), "{case}");
        assert!(!peer.vm.launched.process.has_exited().unwrap(), "{case}");
    }
    // A restore without an export sends no mapping table, and starts its workload at once.
    let mut configuration = record().configuration().clone();
    configuration.export = None;
    configuration.mappings = 0;
    let record = ActivationRecord::new(record().restore().cloned(), configuration).unwrap();
    let mut peer = Peer::start("reconnect");
    // The session stays open until the peer answered its Start.
    let _session = restore(
        &mut peer.vm.launched,
        &record,
        &workload(),
        &[],
        &Cancel::new(false),
    )
    .unwrap();
    wait_for(&peer.stage, "started");
    assert_eq!(peer.kinds(), [8, 8, 8, 2]);
}

#[test]
fn a_cold_record_and_a_root_workload_fail_before_the_vm_is_contacted() {
    let cold = ActivationRecord::new(None, record().configuration().clone()).unwrap();
    let mut root = workload();
    root.identity = WorkloadIdentity {
        uid: 0,
        gid: 0,
        create_account: false,
    };
    for (record, workload, phase, evidence) in [
        (
            cold,
            workload(),
            RestorePhase::ReadyEvent,
            RestoreEvidence::InvalidActivate,
        ),
        (
            record(),
            root,
            RestorePhase::Activate,
            RestoreEvidence::InvalidSettings,
        ),
    ] {
        let mut peer = Peer::start("connect");
        let error = machine(
            restore(
                &mut peer.vm.launched,
                &record,
                &workload,
                &maps(),
                &Cancel::new(false),
            )
            .err()
            .unwrap(),
        );
        assert_eq!((error.phase, error.evidence), (phase, evidence));
        assert!(error.guest_may_have_run);
        assert!(peer.requests().is_empty());
        assert!(!peer.vm.launched.process.has_exited().unwrap());
    }
}
