use std::error::Error as _;
use std::io::{Read, Write};
use std::process::{Child, ExitStatus};
use std::thread;

use super::*;
use crate::host::process::BootId;
use crate::protocol::OUTER_HEADER_LEN;
use crate::protocol::nvxs::{ErrorCode, Identity, RecordType, encode_record};

const CREDENTIAL: [u8; CAPABILITY_LEN] = [0x5a; CAPABILITY_LEN];
const IDENTITY: Identity = Identity {
    instance_id: [9; 16],
    epoch: 1,
};

#[cfg(target_os = "linux")]
type Server = std::os::unix::net::UnixListener;
#[cfg(windows)]
type Server = File;

fn bind(endpoint: &str) -> Server {
    #[cfg(target_os = "linux")]
    {
        Server::bind(endpoint).unwrap()
    }
    #[cfg(windows)]
    {
        platform::tests::control_server(endpoint)
    }
}

fn saved(vm: &VmDir, process: ProcessRef) -> SavedVm {
    SavedVm {
        vm_dir: vm.clone(),
        endpoint: vm.outputs(false).unwrap().endpoint,
        capability: CREDENTIAL,
        process,
        runtime: cold_runtime().identity,
    }
}

/// Returns the connection failure that `error` keeps as its source.
fn connection_error(error: &Error) -> &ConnectionError {
    error
        .source()
        .unwrap()
        .downcast_ref::<ConnectionError>()
        .unwrap()
}

/// Returns the I/O cause of a connection failure.
fn io_cause(error: &ConnectionError) -> &io::Error {
    error.source().unwrap().downcast_ref::<io::Error>().unwrap()
}

#[test]
fn verifying_saved_values_checks_runtime_and_process_without_opening_an_endpoint() {
    let (_directory, vm) = vm_dir();
    let launched = RunningVm(spawn(stand_in(&vm)).unwrap());
    let mut saved = saved(&vm, launched.0.process.reference().clone());
    saved.endpoint = "\0not-an-endpoint".to_owned();
    let process = verify(&saved, &saved.runtime).unwrap();
    assert_eq!(process.reference(), &saved.process);
    assert!(!process.has_exited().unwrap());
    assert!(!format!("{saved:?}").contains(&format!("{CREDENTIAL:?}")));

    let mut other = saved.runtime.clone();
    other.compatibility = CompatibilityIdentity::new("other-runtime").unwrap();
    saved.process.pid = 0;
    let error = verify(&saved, &other).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::RuntimeMismatch);
    assert!(error.to_string().contains("compatibility"));
    other.kernel = ArtifactIdentity::new("other-kernel").unwrap();
    let error = verify(&saved, &other).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::RuntimeMismatch);
    assert!(error.to_string().contains("kernel, compatibility"));
    assert_eq!(
        attach(
            &saved,
            &Runtime {
                identity: other,
                ..cold_runtime()
            },
            Instant::now()
        )
        .err()
        .unwrap()
        .kind(),
        ErrorKind::RuntimeMismatch,
    );
    let error = verify(&saved, &saved.runtime).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(
        error
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidInput,
    );
    saved.process = launched.0.process.reference().clone();
    saved.process.start_time += 1;
    let error = verify(&saved, &saved.runtime).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Exited);
    assert_eq!(
        error
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap()
            .kind(),
        io::ErrorKind::NotFound,
    );
}

#[test]
fn attaching_refuses_an_endpoint_served_by_another_owned_process() {
    let (_directory, vm) = vm_dir();
    let launched = RunningVm(spawn(stand_in(&vm)).unwrap());
    let saved = saved(&vm, launched.0.process.reference().clone());
    let _server = bind(&saved.endpoint);
    let error = attach(
        &saved,
        &cold_runtime(),
        Instant::now() + Duration::from_secs(10),
    )
    .err()
    .unwrap();
    assert_eq!(error.kind(), ErrorKind::Protocol);
    let connection = connection_error(&error);
    assert_eq!(connection.kind(), ConnectionErrorKind::PeerMismatch);
    assert_eq!(io_cause(connection).kind(), io::ErrorKind::PermissionDenied);
    assert!(!launched.0.process.has_exited().unwrap());
}

#[test]
fn connecting_to_a_launched_vm_refuses_an_endpoint_that_another_process_serves() {
    let (_directory, vm) = vm_dir();
    let launched = RunningVm(spawn(stand_in(&vm)).unwrap());
    let _server = bind(&launched.0.outputs.endpoint);
    let error = launched
        .0
        .connect(Instant::now() + Duration::from_secs(10))
        .unwrap_err();
    assert_eq!(error.kind(), ConnectionErrorKind::PeerMismatch);
    assert_eq!(io_cause(&error).kind(), io::ErrorKind::PermissionDenied);
    assert!(!launched.0.process.has_exited().unwrap());
}

#[test]
fn attaching_keeps_the_actual_unavailable_endpoint_error() {
    let (_directory, vm) = vm_dir();
    let launched = RunningVm(spawn(stand_in(&vm)).unwrap());
    let saved = saved(&vm, launched.0.process.reference().clone());
    let error = attach(
        &saved,
        &cold_runtime(),
        Instant::now() + Duration::from_millis(80),
    )
    .err()
    .unwrap();
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    let connection = connection_error(&error);
    assert_eq!(connection.kind(), ConnectionErrorKind::TimedOut);
    let source = io_cause(connection);
    assert_eq!(source.kind(), io::ErrorKind::NotFound);
    assert!(source.raw_os_error().is_some());
    let error = launched
        .0
        .connect(Instant::now() + Duration::from_millis(80))
        .unwrap_err();
    assert_eq!(error.kind(), ConnectionErrorKind::TimedOut);
    assert!(!launched.0.process.has_exited().unwrap());
}

#[test]
fn connecting_to_a_launched_vm_whose_process_exited_tells_so() {
    let (_directory, vm) = vm_dir();
    let launched = RunningVm(spawn(stand_in(&vm)).unwrap());
    launched.0.process.kill().unwrap();
    assert!(
        launched
            .0
            .process
            .wait(Instant::now() + Duration::from_secs(10))
            .unwrap()
            .is_some()
    );
    let error = launched
        .0
        .connect(Instant::now() + Duration::from_secs(10))
        .unwrap_err();
    assert_eq!(error.kind(), ConnectionErrorKind::Exited);
    assert!(!error.is_lost());
    assert_eq!(
        launched
            .0
            .console(Instant::now() + Duration::from_secs(10))
            .unwrap_err()
            .kind(),
        ErrorKind::Exited
    );
}

struct ServerChild {
    child: Child,
    log: PathBuf,
}

impl ServerChild {
    fn start(vm: &VmDir, silent: bool) -> (Self, SavedVm) {
        let endpoint = vm.outputs(false).unwrap().endpoint;
        let ready = vm.path().join("owned-endpoint-ready");
        let log = vm.path().join("owned-endpoint.log");
        let output = File::create(&log).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "host::vm::tests::attach::owned_endpoint_child",
                "--ignored",
                "--nocapture",
            ])
            .env("NVX_SAVED_VM_ENDPOINT", &endpoint)
            .env("NVX_SAVED_VM_READY", &ready)
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output);
        if silent {
            command.env("NVX_SAVED_VM_SILENT_ATTACH", "1");
        }
        let mut owner = Self {
            child: crate::host::tests::spawn_locked(&mut command).unwrap(),
            log,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(
                owner.child.try_wait().unwrap().is_none() && Instant::now() < deadline,
                "owned endpoint did not become ready: {}",
                fs::read_to_string(&owner.log).unwrap(),
            );
            thread::sleep(Duration::from_millis(10));
        }
        let reference = ProcessRef {
            pid: owner.child.id(),
            start_time: platform::process_start_time(owner.child.id())
                .unwrap()
                .unwrap(),
            boot: BootId::current().unwrap(),
        };
        let mut saved = saved(vm, reference);
        saved.endpoint = endpoint;
        (owner, saved)
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "owned endpoint failed: {}",
                    fs::read_to_string(&self.log).unwrap(),
                );
                return status;
            }
            assert!(Instant::now() < deadline, "owned endpoint did not exit");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ServerChild {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
            self.child.wait().unwrap();
        }
    }
}

#[test]
fn attaching_returns_the_held_owned_child_and_authenticated_connection() {
    let (_directory, vm) = vm_dir();
    let (mut owner, saved) = ServerChild::start(&vm, false);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut attached = attach(&saved, &cold_runtime(), deadline).unwrap();
    assert_eq!(attached.process.reference(), &saved.process);
    assert!(!attached.process.has_exited().unwrap());
    attached
        .connection
        .send(b"request", Some(deadline))
        .unwrap();
    let mut response = Vec::new();
    while response.len() < b"response".len() {
        response.extend(attached.connection.receive(Some(deadline)).unwrap());
    }
    assert_eq!(response, b"response");
    owner.wait();
    let error = attached.connection.receive(Some(deadline)).unwrap_err();
    assert_eq!(error.kind(), ConnectionErrorKind::Closed);
    assert!(error.is_lost());
    assert!(attached.process.wait(deadline).unwrap().is_some());
}

#[test]
fn attaching_keeps_the_brokers_authentication_refusal() {
    let (_directory, vm) = vm_dir();
    let (mut owner, mut saved) = ServerChild::start(&vm, false);
    saved.capability[0] ^= 1;
    let error = attach(
        &saved,
        &cold_runtime(),
        Instant::now() + Duration::from_secs(10),
    )
    .err()
    .unwrap();
    assert_eq!(error.kind(), ErrorKind::Protocol);
    let connection = connection_error(&error);
    assert_eq!(connection.kind(), ConnectionErrorKind::Rejected);
    assert!(
        connection
            .to_string()
            .contains(&ErrorCode::Authentication.to_string())
    );
    owner.wait();
}

#[test]
fn attaching_keeps_the_callers_deadline_while_the_broker_is_silent() {
    let (_directory, vm) = vm_dir();
    let (mut owner, saved) = ServerChild::start(&vm, true);
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let started = Instant::now();
        let result = attach(
            &saved,
            &cold_runtime(),
            started + Duration::from_millis(300),
        )
        .map(|_| ());
        sender.send((result, started.elapsed())).unwrap();
    });
    let completed = receiver.recv_timeout(Duration::from_secs(3));
    if completed.is_err() && owner.child.try_wait().unwrap().is_none() {
        owner.child.kill().unwrap();
        owner.child.wait().unwrap();
    }
    worker.join().unwrap();
    let (result, elapsed) = completed.expect("attach did not honor the attach deadline");
    assert_eq!(result.unwrap_err().kind(), ErrorKind::TimedOut);
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    owner.wait();
}

fn record(record_type: RecordType, sequence: u64, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    encode_record(
        record_type,
        &IDENTITY.instance_id,
        IDENTITY.epoch,
        sequence,
        payload,
        &mut bytes,
    )
    .unwrap();
    bytes
}

#[test]
#[ignore = "invoked as an owned endpoint child by the attach tests"]
fn owned_endpoint_child() {
    let endpoint = std::env::var("NVX_SAVED_VM_ENDPOINT").unwrap();
    let ready = PathBuf::from(std::env::var_os("NVX_SAVED_VM_READY").unwrap());
    let server = bind(&endpoint);
    fs::write(ready, b"ready").unwrap();
    #[cfg(target_os = "linux")]
    let mut peer = {
        let peer = server.accept().unwrap().0;
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        peer
    };
    #[cfg(windows)]
    let mut peer = {
        platform::tests::accept_control(&server);
        server
    };
    let mut expected = Vec::new();
    encode_record(
        RecordType::HostAttach,
        &[0; 16],
        0,
        0,
        &CREDENTIAL,
        &mut expected,
    )
    .unwrap();
    let mut request = vec![0; expected.len()];
    peer.read_exact(&mut request).unwrap();
    assert_eq!(&request[..OUTER_HEADER_LEN], &expected[..OUTER_HEADER_LEN]);
    if request[OUTER_HEADER_LEN..] != CREDENTIAL {
        peer.write_all(&record(
            RecordType::Error,
            0,
            &ErrorCode::Authentication.code().to_le_bytes(),
        ))
        .unwrap();
        return;
    }
    if std::env::var_os("NVX_SAVED_VM_SILENT_ATTACH").is_some() {
        match peer.read(&mut [0]) {
            Ok(0) => {}
            #[cfg(windows)]
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
            result => panic!("silent owned endpoint was not closed: {result:?}"),
        }
        return;
    }
    peer.write_all(&record(RecordType::Ready, 0, &[])).unwrap();
    let expected = record(RecordType::Data, 0, b"request");
    let mut request = vec![0; expected.len()];
    peer.read_exact(&mut request).unwrap();
    assert_eq!(request, expected);
    peer.write_all(&record(RecordType::Data, 1, b"response"))
        .unwrap();
}

/// An idle copy of the test binary, which stands in for a VM's process, and is killed and
/// reaped when dropped.
struct IdleChild(Child);

impl IdleChild {
    fn start() -> Self {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "host::vm::tests::attach::idle_child",
                "--ignored",
                "--test-threads=1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null());
        Self(crate::host::tests::spawn_locked(&mut command).unwrap())
    }

    fn reference(&self) -> ProcessRef {
        ProcessRef {
            pid: self.0.id(),
            start_time: platform::process_start_time(self.0.id()).unwrap().unwrap(),
            boot: BootId::current().unwrap(),
        }
    }
}

impl Drop for IdleChild {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
            self.0.wait().unwrap();
        }
    }
}

#[test]
#[ignore = "the idle stand-in for a VM's process, which only the checks of saved values start"]
fn idle_child() {
    io::stdin().read_to_end(&mut Vec::new()).unwrap();
}

#[test]
fn a_saved_vm_runs_until_its_process_exits_and_another_runtime_is_refused_without_a_signal() {
    let (_directory, vm) = vm_dir();
    let mut child = IdleChild::start();
    let saved = saved(&vm, child.reference());
    let held = verify(&saved, &saved.runtime).unwrap();
    assert!(held.wait(Instant::now()).unwrap().is_none());
    let mut other = saved.runtime.clone();
    other.compatibility = CompatibilityIdentity::new("other-runtime").unwrap();
    assert_eq!(
        verify(&saved, &other).unwrap_err().kind(),
        ErrorKind::RuntimeMismatch
    );
    assert!(!held.has_exited().unwrap());
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(
        held.wait(Instant::now() + Duration::from_secs(10))
            .unwrap()
            .is_some()
    );
    let error = verify(&saved, &saved.runtime).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Exited);
    assert_eq!(
        error
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap()
            .kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn attaching_refuses_an_endpoint_that_another_process_serves_and_keeps_the_vm_and_its_values() {
    let (_directory, vm) = vm_dir();
    let child = IdleChild::start();
    let saved = saved(&vm, child.reference());
    let before = saved.clone();
    let _server = bind(&saved.endpoint);
    let deadline = Instant::now() + Duration::from_secs(10);
    let error = attach(&saved, &cold_runtime(), deadline).err().unwrap();
    assert_eq!(error.kind(), ErrorKind::Protocol);
    let connection = connection_error(&error);
    assert_eq!(connection.kind(), ConnectionErrorKind::PeerMismatch);
    assert_eq!(io_cause(connection).kind(), io::ErrorKind::PermissionDenied);
    assert!(
        !verify(&saved, &saved.runtime)
            .unwrap()
            .has_exited()
            .unwrap()
    );
    assert_eq!(saved.endpoint, before.endpoint);
    assert_eq!(saved.capability, before.capability);
    assert_eq!(saved.process, before.process);
    assert_eq!(saved.runtime, before.runtime);
    assert_eq!(saved.vm_dir.path(), before.vm_dir.path());

    let mut invalid = saved.clone();
    invalid.process.pid = 0;
    let error =
        crate::host::transport::connect_verified(&invalid.endpoint, &invalid.process, deadline)
            .err()
            .unwrap();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(
        error
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert!(
        !verify(&saved, &saved.runtime)
            .unwrap()
            .has_exited()
            .unwrap()
    );
}
