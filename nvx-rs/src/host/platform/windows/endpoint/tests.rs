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

    // SAFETY: the synchronous server and its handle remain valid while it accepts.
    let connected =
        unsafe { ConnectNamedPipe(server.as_handle().as_raw_handle(), ptr::null_mut()) };
    if connected == 0 {
        assert_eq!(last_error(), ERROR_PIPE_CONNECTED);
    }
    connected
}

#[test]
fn a_duplicated_transport_keeps_the_verified_connection_without_reconnecting() {
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
    assert_eq!(accept_control(&server), 0);
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
