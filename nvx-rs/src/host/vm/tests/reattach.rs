use std::error::Error as _;
use std::io::{Read, Write};
use std::process::{Child, ExitStatus};
use std::thread;

use super::*;
use crate::host::process::BootId;
use crate::host::transport::HostConnectionError;
use crate::protocol::OUTER_HEADER_LEN;
use crate::protocol::nvxs::{ErrorCode, HostState, Identity, RecordType, encode_record};

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
    other.control_session += 1;
    saved.process.pid = 0;
    let error = verify(&saved, &other).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::RuntimeMismatch);
    assert!(error.to_string().contains("control session"));
    other.kernel = ArtifactIdentity::new("other-kernel").unwrap();
    let error = verify(&saved, &other).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::RuntimeMismatch);
    assert!(error.to_string().contains("kernel, control session"));
    assert_eq!(
        reattach(
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
fn reattaching_refuses_an_endpoint_served_by_another_owned_process() {
    let (_directory, vm) = vm_dir();
    let launched = RunningVm(spawn(stand_in(&vm)).unwrap());
    let saved = saved(&vm, launched.0.process.reference().clone());
    let _server = bind(&saved.endpoint);
    let error = reattach(
        &saved,
        &cold_runtime(),
        Instant::now() + Duration::from_secs(10),
    )
    .err()
    .unwrap();
    assert_eq!(error.kind(), ErrorKind::Protocol);
    assert_eq!(
        error
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap()
            .kind(),
        io::ErrorKind::PermissionDenied,
    );
    assert!(!launched.0.process.has_exited().unwrap());
}

#[test]
fn reattaching_keeps_the_actual_unavailable_endpoint_error() {
    let (_directory, vm) = vm_dir();
    let launched = RunningVm(spawn(stand_in(&vm)).unwrap());
    let saved = saved(&vm, launched.0.process.reference().clone());
    let error = reattach(
        &saved,
        &cold_runtime(),
        Instant::now() + Duration::from_millis(80),
    )
    .err()
    .unwrap();
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
    assert_eq!(source.kind(), io::ErrorKind::NotFound);
    assert!(source.raw_os_error().is_some());
    assert!(!launched.0.process.has_exited().unwrap());
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
                "host::vm::tests::reattach::owned_endpoint_child",
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
fn reattaching_returns_the_held_owned_child_and_authenticated_connection() {
    let (_directory, vm) = vm_dir();
    let (mut owner, saved) = ServerChild::start(&vm, false);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut attached = reattach(&saved, &cold_runtime(), deadline).unwrap();
    assert_eq!(attached.process.reference(), &saved.process);
    assert!(!attached.process.has_exited().unwrap());
    assert_eq!(attached.connection.identity(), IDENTITY);
    assert_eq!(attached.connection.session().state(), HostState::Attached);
    attached
        .connection
        .send(b"request", Some(deadline))
        .unwrap();
    assert_eq!(
        attached.connection.receive(Some(deadline)).unwrap(),
        b"response"
    );
    owner.wait();
    assert!(attached.process.wait(deadline).unwrap());
}

#[test]
fn reattaching_keeps_the_brokers_authentication_refusal() {
    let (_directory, vm) = vm_dir();
    let (mut owner, mut saved) = ServerChild::start(&vm, false);
    saved.capability[0] ^= 1;
    let error = reattach(
        &saved,
        &cold_runtime(),
        Instant::now() + Duration::from_secs(10),
    )
    .err()
    .unwrap();
    assert_eq!(error.kind(), ErrorKind::Protocol);
    assert!(matches!(
        error
            .source()
            .unwrap()
            .downcast_ref::<HostConnectionError>(),
        Some(HostConnectionError::Rejected(ErrorCode::Authentication)),
    ));
    owner.wait();
}

#[test]
fn reattaching_keeps_the_callers_deadline_while_the_broker_is_silent() {
    let (_directory, vm) = vm_dir();
    let (mut owner, saved) = ServerChild::start(&vm, true);
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let started = Instant::now();
        let result = reattach(
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
    let (result, elapsed) = completed.expect("reattach did not honor the attach deadline");
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
#[ignore = "invoked as an owned endpoint child by the reattach tests"]
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
