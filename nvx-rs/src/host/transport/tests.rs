#![cfg(any(target_os = "linux", windows))]

use std::error::Error as _;
#[cfg(feature = "cli")]
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};

use super::*;
use crate::host::process::BootId;

mod connection;

#[cfg(target_os = "linux")]
type Server = std::os::unix::net::UnixListener;
#[cfg(windows)]
type Server = std::fs::File;

fn reference(pid: u32) -> ProcessRef {
    ProcessRef {
        pid,
        start_time: platform::process_start_time(pid).unwrap().unwrap(),
        boot: BootId::current().unwrap(),
    }
}

fn endpoint(directory: &tempfile::TempDir) -> String {
    #[cfg(target_os = "linux")]
    {
        directory
            .path()
            .join("control.sock")
            .to_str()
            .unwrap()
            .to_owned()
    }
    #[cfg(windows)]
    {
        let _ = directory;
        format!(
            r"\\.\pipe\nvx-verified-{}-{}",
            std::process::id(),
            crate::host::tests::unique_suffix()
        )
    }
}

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

#[cfg(feature = "cli")]
fn exchange(server: Server) {
    #[cfg(target_os = "linux")]
    let mut peer = server.accept().unwrap().0;
    #[cfg(windows)]
    let mut peer = {
        platform::tests::accept_control(&server);
        server
    };
    let mut request = [0; 7];
    peer.read_exact(&mut request).unwrap();
    assert_eq!(&request, b"request");
    peer.write_all(b"response").unwrap();
}

struct Sleeper(Child);

impl Sleeper {
    fn new() -> Self {
        let mut command = if cfg!(windows) {
            let mut command = Command::new("ping");
            command.args(["-n", "60", "127.0.0.1"]);
            command
        } else {
            let mut command = Command::new("sleep");
            command.arg("60");
            command
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Self(crate::host::tests::spawn_locked(&mut command).unwrap())
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
            self.0.wait().unwrap();
        }
    }
}

#[cfg(feature = "cli")]
#[test]
fn verified_and_legacy_connections_wait_for_the_owned_endpoint_and_keep_native_io() {
    let process = reference(std::process::id());
    for legacy in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = endpoint(&directory);
        let server_endpoint = endpoint.clone();
        let server = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            exchange(bind(&server_endpoint));
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut transport = if legacy {
            crate::host::session::connect(&endpoint, process.pid, process.start_time, deadline)
                .unwrap()
        } else {
            connect_verified(&endpoint, &process, deadline).unwrap()
        };
        assert!(transport.try_clone_handle().unwrap().is_some());
        #[cfg(target_os = "linux")]
        assert_eq!(transport.unread().unwrap(), Some(0));
        #[cfg(windows)]
        assert_eq!(transport.unread().unwrap(), None);
        transport
            .write_all(b"request", Some(Duration::from_secs(10)))
            .unwrap();
        let mut response = [0; 8];
        let mut read = 0;
        while read < response.len() {
            let count = transport
                .read(&mut response[read..], Some(Duration::from_secs(10)))
                .unwrap();
            assert_ne!(count, 0);
            read += count;
        }
        assert_eq!(&response, b"response");
        server.join().unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(transport.unread().unwrap(), Some(0));
    }
}

#[cfg(feature = "cli")]
#[test]
fn another_process_serving_the_endpoint_is_refused() {
    let vm = Sleeper::new();
    let process = reference(vm.0.id());
    for legacy in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = endpoint(&directory);
        let _server = bind(&endpoint);
        let deadline = Instant::now() + Duration::from_secs(10);
        if legacy {
            let error =
                crate::host::session::connect(&endpoint, process.pid, process.start_time, deadline)
                    .err()
                    .unwrap();
            assert!(matches!(
                error,
                crate::host::session::SessionError::Refused(_)
            ));
        } else {
            let error = connect_verified(&endpoint, &process, deadline)
                .err()
                .unwrap();
            assert_eq!(error.kind(), ErrorKind::Protocol);
            let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
            assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
            assert!(source.to_string().contains(&std::process::id().to_string()));
            assert!(source.to_string().contains(&process.pid.to_string()));
        }
        let Opened::Running(held) = Process::open(&process).unwrap() else {
            panic!("the unrelated VM process still runs");
        };
        assert!(!held.has_exited().unwrap());
    }
}

#[test]
fn endpoint_timeout_keeps_the_actual_unavailable_endpoint_error() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = endpoint(&directory);
    let process = reference(std::process::id());
    let error = connect_verified(
        &endpoint,
        &process,
        Instant::now() + Duration::from_millis(80),
    )
    .err()
    .unwrap();
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
    assert_eq!(source.kind(), io::ErrorKind::NotFound);
    assert!(source.raw_os_error().is_some());
}

#[test]
fn the_recorded_vm_is_checked_even_when_its_pid_serves_the_endpoint() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = endpoint(&directory);
    let _server = bind(&endpoint);
    let mut process = reference(std::process::id());
    process.start_time += 1;
    let error = connect_verified(
        &endpoint,
        &process,
        Instant::now() + Duration::from_secs(10),
    )
    .err()
    .unwrap();
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
fn an_owned_vm_that_exited_is_reported_before_connecting() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = endpoint(&directory);
    let mut vm = Sleeper::new();
    let process = reference(vm.0.id());
    vm.0.kill().unwrap();
    vm.0.wait().unwrap();
    let error = connect_verified(
        &endpoint,
        &process,
        Instant::now() + Duration::from_secs(10),
    )
    .err()
    .unwrap();
    assert_eq!(error.kind(), ErrorKind::Exited);
    assert!(error.source().is_some());
}

#[test]
fn an_owned_vm_exit_interrupts_waiting_for_its_endpoint() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = endpoint(&directory);
    let mut vm = Sleeper::new();
    let process = reference(vm.0.id());
    let ending = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        vm.0.kill().unwrap();
        vm.0.wait().unwrap();
    });
    let error = connect_verified(
        &endpoint,
        &process,
        Instant::now() + Duration::from_secs(10),
    )
    .err()
    .unwrap();
    ending.join().unwrap();
    assert_eq!(error.kind(), ErrorKind::Exited);
    assert!(error.source().is_some());
}

#[test]
fn an_indeterminate_process_reference_retains_its_io_failure() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = endpoint(&directory);
    let process = ProcessRef {
        pid: 0,
        start_time: 0,
        boot: BootId::current().unwrap(),
    };
    let error = connect_verified(
        &endpoint,
        &process,
        Instant::now() + Duration::from_secs(10),
    )
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
        io::ErrorKind::InvalidInput,
    );
}
