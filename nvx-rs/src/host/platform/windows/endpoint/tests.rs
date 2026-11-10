use std::fs::File;
use std::io::{Read, Write};

use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;

use super::*;

pub(crate) fn control_server(endpoint: &str) -> File {
    let name = wide(endpoint);
    // SAFETY: the name is NUL-terminated; this creates a fresh local byte pipe.
    let server = owned(unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            4096,
            4096,
            0,
            ptr::null(),
        )
    })
    .unwrap();
    File::from(server)
}

pub(crate) fn accept_control(server: &File) -> i32 {
    use std::os::windows::io::{AsHandle, AsRawHandle};

    let connect = || {
        // SAFETY: the synchronous server and its handle remain valid while it accepts.
        unsafe { ConnectNamedPipe(server.as_handle().as_raw_handle(), ptr::null_mut()) }
    };
    let mut connected = connect();
    if connected == 0 && last_error() == ERROR_NO_DATA {
        disconnect_control(server);
        connected = connect();
    }
    if connected == 0 {
        assert_eq!(last_error(), ERROR_PIPE_CONNECTED);
    }
    connected
}

pub(crate) fn disconnect_control(server: &File) {
    use std::os::windows::io::{AsHandle, AsRawHandle};

    use windows_sys::Win32::System::Pipes::DisconnectNamedPipe;

    // SAFETY: the owned synchronous pipe finished its previous client.
    assert_ne!(
        unsafe { DisconnectNamedPipe(server.as_handle().as_raw_handle()) },
        0
    );
}

#[test]
fn a_stale_control_probe_is_disconnected_before_the_actual_client_is_accepted() {
    let endpoint = format!(
        r"\\.\pipe\nvx-stale-probe-{}-{}",
        std::process::id(),
        crate::host::store::LaunchId::generate().unwrap().as_str()
    );
    let server = control_server(&endpoint);
    drop(connect_endpoint(&endpoint, std::process::id(), Duration::from_secs(2)).unwrap());
    let (ready, accepting) = std::sync::mpsc::channel();
    let accepted = std::thread::spawn(move || {
        ready.send(()).unwrap();
        accept_control(&server);
        let mut request = [0];
        (&server).read_exact(&mut request).unwrap();
        assert_eq!(request, [7]);
        (&server).write_all(&[9]).unwrap();
    });
    accepting.recv_timeout(Duration::from_secs(2)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut client = loop {
        let left = time_left(Some(deadline)).unwrap().unwrap();
        match connect_endpoint(
            &endpoint,
            std::process::id(),
            left.min(Duration::from_millis(25)),
        ) {
            Ok(client) => break client,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("the actual control client failed: {error}"),
        }
    };
    client
        .write_all(&[7], Some(Duration::from_secs(2)))
        .unwrap();
    let mut response = [0];
    assert_eq!(
        client
            .read(&mut response, Some(Duration::from_secs(2)))
            .unwrap(),
        1
    );
    assert_eq!(response, [9]);
    accepted.join().unwrap();
}

#[test]
fn a_busy_control_pipe_never_replaces_the_callers_probe_deadline() {
    let endpoint = format!(
        r"\\.\pipe\nvx-probe-budget-{}-{}",
        std::process::id(),
        crate::host::store::LaunchId::generate().unwrap().as_str()
    );
    let server = control_server(&endpoint);
    let client = connect_endpoint(&endpoint, std::process::id(), Duration::from_secs(2)).unwrap();
    for (budget, limit) in [
        (Duration::from_millis(50), Duration::from_millis(250)),
        (Duration::from_micros(500), Duration::from_millis(40)),
    ] {
        let started = Instant::now();
        let deadline = started + budget;
        let error = endpoint_server_pid_until(&endpoint, deadline).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < limit, "{:?}", started.elapsed());
        assert_eq!(
            endpoint_server_pid_until(&endpoint, started)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }
    drop(client);
    drop(server);
    assert_eq!(
        endpoint_server_pid_until(&endpoint, Instant::now())
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
    let server = control_server(&endpoint);
    assert_eq!(
        endpoint_server_pid(&endpoint).unwrap(),
        Some(std::process::id())
    );
    drop(server);
}

#[test]
fn a_duplicated_transport_keeps_the_verified_connection_without_reconnecting() {
    use std::os::windows::io::{AsHandle, AsRawHandle};

    let endpoint = format!(
        r"\\.\pipe\nvx-transport-{}-{}",
        std::process::id(),
        crate::host::store::LaunchId::generate().unwrap().as_str()
    );
    let server = control_server(&endpoint);
    let transport =
        connect_endpoint(&endpoint, std::process::id(), Duration::from_secs(2)).unwrap();
    drop(transport.try_clone_handle().unwrap().unwrap());
    let pipe = transport.try_clone_handle().unwrap().unwrap();
    drop(transport);
    // A replacement connection would hide a lost duplicate, so this check stays strict.
    // SAFETY: the owned synchronous server remains valid while it accepts.
    assert_eq!(
        unsafe { ConnectNamedPipe(server.as_handle().as_raw_handle(), ptr::null_mut()) },
        0
    );
    assert_eq!(last_error(), ERROR_PIPE_CONNECTED);
    // SAFETY: this creates a fresh unnamed manual-reset event.
    let event = owned(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) }).unwrap();
    let mut duplicate = PipeTransport { pipe, event };
    let mut server = server;
    duplicate
        .write_all(b"request", Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = [0; 7];
    server.read_exact(&mut request).unwrap();
    assert_eq!(&request, b"request");
    server.write_all(b"response").unwrap();
    let mut response = [0; 8];
    assert_eq!(
        duplicate
            .read(&mut response, Some(Duration::from_secs(2)))
            .unwrap(),
        response.len()
    );
    assert_eq!(&response, b"response");
    drop(duplicate);
    assert_eq!(server.read(&mut response).unwrap(), 0);
}
