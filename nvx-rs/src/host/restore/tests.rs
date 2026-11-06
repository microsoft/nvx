#![cfg(any(target_os = "linux", windows))]
#![cfg_attr(windows, allow(unsafe_code))]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use super::*;
use crate::ErrorKind;
use crate::host::platform::inherit::spawn_command;
use crate::host::process::BootId;
use crate::host::vm::create_private_dir;
use crate::protocol::restore_ready::parse_ready_event;

fn vm_directory() -> (tempfile::TempDir, VmDir) {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("vm");
    create_private_dir(&path).unwrap();
    let directory = VmDir::open(&path).unwrap();
    (temporary, directory)
}

fn held_process(pid: u32) -> Process {
    Process::open(&ProcessRef {
        pid,
        start_time: platform::process_start_time(pid).unwrap().unwrap(),
        boot: BootId::current().unwrap(),
    })
    .unwrap()
    .unwrap()
}

/// A separate process, held before it may connect, and alive until the test ends it.
struct Client {
    child: Child,
    process: Process,
}

impl Client {
    fn start(endpoint: &str, data: &[u8], close: bool) -> Self {
        #[cfg(target_os = "linux")]
        let mut command = {
            let mut command = Command::new("python3");
            command.args([
                "-c",
                "import os,socket,sys\n\
                 sys.stdin.buffer.read(1)\n\
                 peer=socket.socket(socket.AF_UNIX)\n\
                 peer.connect(os.environ['NVX_TEST_READY_ENDPOINT'])\n\
                 peer.sendall(os.environ['NVX_TEST_READY_DATA'].encode('ascii'))\n\
                 if os.environ['NVX_TEST_READY_CLOSE']=='1': peer.close()\n\
                 print('sent',flush=True)\n\
                 sys.stdin.buffer.read(1)\n\
                 peer.close()\n",
            ]);
            command
        };
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("powershell");
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$ErrorActionPreference='Stop'; \
                 $inputStream=[Console]::OpenStandardInput(); $null=$inputStream.ReadByte(); \
                 $name=$env:NVX_TEST_READY_ENDPOINT.Substring('//./pipe/'.Length); \
                 $peer=[IO.Pipes.NamedPipeClientStream]::new('.', $name, [IO.Pipes.PipeDirection]::Out); \
                 $peer.Connect(10000); \
                 $data=[Text.Encoding]::ASCII.GetBytes([string]$env:NVX_TEST_READY_DATA); \
                 $peer.Write($data,0,$data.Length); $peer.Flush(); \
                 if ($env:NVX_TEST_READY_CLOSE -eq '1') { $peer.Dispose() }; \
                 [Console]::Out.WriteLine('sent'); [Console]::Out.Flush(); \
                 $null=$inputStream.ReadByte(); $peer.Dispose()",
            ]);
            command
        };
        command
            .env("NVX_TEST_READY_ENDPOINT", endpoint)
            .env("NVX_TEST_READY_DATA", std::str::from_utf8(data).unwrap())
            .env("NVX_TEST_READY_CLOSE", if close { "1" } else { "0" })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let child = spawn_command(&mut command).unwrap();
        let process = held_process(child.id());
        let mut client = Self { child, process };
        client
            .child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"g")
            .unwrap();
        let mut line = String::new();
        BufReader::new(client.child.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "sent");
        client
    }

    fn stop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn the_held_vm_process_is_accepted_even_after_its_client_closed() {
    let (_temporary, directory) = vm_directory();
    for data in [&b""[..], b"OPENVMM_RESTORE_READY_V1\n"] {
        let mut listener = ReadyListener::bind(&directory).unwrap();
        let client = Client::start(listener.endpoint(), data, true);
        let mut received = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while listener
            .receive(
                &client.process,
                &mut received,
                64,
                Duration::from_millis(50),
            )
            .unwrap()
        {
            assert!(Instant::now() < deadline, "the endpoint did not close");
        }
        assert_eq!(received, data);
        assert_eq!(
            listener.into_std().unwrap_err().kind(),
            ErrorKind::Unsupported
        );
    }
}

#[test]
fn a_different_real_process_cannot_supply_the_event() {
    let (_temporary, directory) = vm_directory();
    let mut listener = ReadyListener::bind(&directory).unwrap();
    let expected = held_process(std::process::id());
    let client = Client::start(listener.endpoint(), b"OPENVMM_RESTORE_READY_V1\n", false);
    assert_ne!(client.process.reference().pid, expected.reference().pid);
    let mut received = Vec::new();
    let error = listener
        .receive(&expected, &mut received, 64, Duration::from_secs(2))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Protocol);
    assert!(received.is_empty());
}

#[test]
fn an_exited_held_process_cannot_supply_even_an_already_queued_event() {
    let (_temporary, directory) = vm_directory();
    let mut listener = ReadyListener::bind(&directory).unwrap();
    let mut client = Client::start(listener.endpoint(), b"OPENVMM_RESTORE_READY_V1\n", true);
    client.stop();
    assert!(client.process.has_exited().unwrap());
    let mut received = Vec::new();
    let error = listener
        .receive(&client.process, &mut received, 64, Duration::from_secs(2))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Exited);
    assert!(received.is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn a_queued_peer_cannot_be_the_vm_that_reuses_its_pid() {
    use std::fs;
    use std::thread;

    let namespace = fs::read_link("/proc/self/ns/pid").unwrap();
    let outer = "NVX_TEST_READY_OUTER_PID_NAMESPACE";
    if std::env::var_os(outer).is_none() {
        let mut command = Command::new("unshare");
        command
            .args([
                "--user",
                "--map-root-user",
                "--pid",
                "--fork",
                "--mount-proc",
            ])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host::restore::tests::a_queued_peer_cannot_be_the_vm_that_reuses_its_pid",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(outer, namespace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = spawn_command(&mut command)
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    // Never write the host's PID allocator: this process is its private namespace's init.
    assert_eq!(std::process::id(), 1);
    assert_ne!(namespace.as_os_str(), std::env::var_os(outer).unwrap());
    let (_temporary, directory) = vm_directory();
    let mut listener = ReadyListener::bind(&directory).unwrap();
    let mut peer = Client::start(listener.endpoint(), b"OPENVMM_RESTORE_READY_V1\n", true);
    let recycled = peer.child.id();
    peer.stop();
    assert!(peer.process.has_exited().unwrap());
    thread::sleep(Duration::from_millis(20));
    fs::write("/proc/sys/kernel/ns_last_pid", (recycled - 1).to_string()).unwrap();
    let mut command = Command::new("python3");
    command
        .args([
            "-c",
            "import sys\nprint('running',flush=True)\nsys.stdin.buffer.read(1)\n",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let child = spawn_command(&mut command).unwrap();
    assert_eq!(child.id(), recycled);
    let process = held_process(child.id());
    let mut vm = Client { child, process };
    let mut line = String::new();
    BufReader::new(vm.child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "running");
    assert!(!vm.process.has_exited().unwrap());
    let mut received = Vec::new();
    let error = listener
        .receive(&vm.process, &mut received, 64, Duration::from_secs(2))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Protocol);
    assert!(received.is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn the_receiver_fails_closed_without_original_peer_pidfds() {
    let child = "NVX_TEST_READY_PIDFD_DENIAL";
    if std::env::var_os(child).is_none() {
        for errno in [nix::libc::ENOPROTOOPT, nix::libc::EPERM] {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "host::restore::tests::the_receiver_fails_closed_without_original_peer_pidfds",
                    "--nocapture",
                ])
                .env(child, errno.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            crate::host::sys::tests::refuse_peer_pidfd_on_spawn(&mut command, errno);
            let output = spawn_command(&mut command)
                .unwrap()
                .wait_with_output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    }
    let (_temporary, directory) = vm_directory();
    let mut listener = ReadyListener::bind(&directory).unwrap();
    let client = Client::start(listener.endpoint(), b"OPENVMM_RESTORE_READY_V1\n", true);
    let mut received = Vec::new();
    let error = listener
        .receive(&client.process, &mut received, 64, Duration::from_secs(2))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert!(received.is_empty());
}

#[test]
fn the_expected_process_cannot_change_after_acceptance() {
    let (_temporary, directory) = vm_directory();
    let mut listener = ReadyListener::bind(&directory).unwrap();
    let client = Client::start(listener.endpoint(), b"OPENVMM_RESTORE_READY_V1\n", false);
    let mut received = Vec::new();
    listener
        .receive(&client.process, &mut received, 64, Duration::from_secs(2))
        .unwrap();
    parse_ready_event(&received).unwrap();
    let error = listener
        .receive(
            &held_process(std::process::id()),
            &mut received,
            64,
            Duration::from_secs(2),
        )
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Protocol);
}

#[test]
fn a_native_listener_transfer_keeps_the_endpoint_usable_after_an_empty_receive() {
    let (_temporary, directory) = vm_directory();
    let mut listener = ReadyListener::bind(&directory).unwrap();
    let endpoint = listener.endpoint().to_owned();
    assert!(
        listener
            .receive(
                &held_process(std::process::id()),
                &mut Vec::new(),
                64,
                Duration::from_millis(1),
            )
            .unwrap()
    );
    let native = listener.into_std().unwrap();
    let client = Client::start(&endpoint, b"OPENVMM_RESTORE_READY_V1\n", false);
    #[cfg(target_os = "linux")]
    let data = {
        use std::io::Read;

        assert_eq!(
            native.local_addr().unwrap().as_pathname(),
            Some(Path::new(&endpoint))
        );
        native.set_nonblocking(false).unwrap();
        let (mut peer, _) = native.accept().unwrap();
        assert_eq!(
            crate::host::sys::peer_pid(&peer).unwrap(),
            client.process.reference().pid
        );
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut data = vec![0; b"OPENVMM_RESTORE_READY_V1\n".len()];
        peer.read_exact(&mut data).unwrap();
        data
    };
    #[cfg(windows)]
    let data = read_native_pipe(native, &client.process);
    parse_ready_event(&data).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn the_socket_is_owner_only_even_when_the_umask_clears_owner_write() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::thread;

    let (_temporary, directory) = vm_directory();
    let listener = thread::scope(|scope| {
        scope
            .spawn(|| {
                nix::sched::unshare(nix::sched::CloneFlags::CLONE_FS).unwrap();
                nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o277));
                ReadyListener::bind(&directory).unwrap()
            })
            .join()
            .unwrap()
    });
    let path = directory.path().join("restore-ready.sock");
    assert_eq!(
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    UnixStream::connect(listener.endpoint()).unwrap();
}

#[cfg(windows)]
#[test]
fn the_pipe_is_a_fresh_owner_only_protected_first_instance() {
    use std::os::windows::io::AsRawHandle;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
        SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION};

    let (_temporary, directory) = vm_directory();
    let listener = ReadyListener::bind(&directory).unwrap();
    let endpoint = listener.endpoint().to_owned();
    let second = ReadyListener::bind(&directory).unwrap();
    assert_ne!(endpoint, second.endpoint());
    let native = listener.into_std().unwrap();
    let error = platform::ReadyListener::bind(&endpoint).unwrap_err();
    // The single-instance limit may reject the competing bind before the first-instance flag.
    assert!(matches!(
        error.raw_os_error().map(|code| code as u32),
        Some(ERROR_ACCESS_DENIED | ERROR_PIPE_BUSY)
    ));
    let mut descriptor = ptr::null_mut();
    let information = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    // SAFETY: the live pipe handle and descriptor output pointer are valid.
    let error = unsafe {
        GetSecurityInfo(
            native.as_raw_handle(),
            SE_FILE_OBJECT,
            information,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(error, 0);
    let mut text = ptr::null_mut();
    let mut length = 0;
    // SAFETY: the descriptor belongs to this test, and the conversion's output pointers are valid.
    assert_ne!(
        unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                information,
                &mut text,
                &mut length,
            )
        },
        0
    );
    // SAFETY: the conversion returned `length` UTF-16 units, including its terminating NUL.
    let access =
        String::from_utf16(unsafe { std::slice::from_raw_parts(text, length as usize - 1) })
            .unwrap();
    // SAFETY: each function allocated the corresponding buffer with LocalAlloc.
    unsafe {
        LocalFree(text.cast());
        LocalFree(descriptor);
    }
    let sid = platform::tests::user_sid();
    assert_eq!(access, format!("O:{sid}D:P(A;;FA;;;{sid})"));
}

#[cfg(windows)]
fn read_native_pipe(pipe: std::os::windows::io::OwnedHandle, process: &Process) -> Vec<u8> {
    use std::os::windows::io::AsRawHandle;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, ERROR_PIPE_CONNECTED};
    use windows_sys::Win32::Storage::FileSystem::ReadFile;
    use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, GetNamedPipeClientProcessId};

    let raw = pipe.as_raw_handle();
    let mut operation: OVERLAPPED = unsafe { std::mem::zeroed() };
    let mut transferred = 0;
    // SAFETY: the operation and live pipe outlast the wait below.
    if unsafe { ConnectNamedPipe(raw, &mut operation) } == 0 {
        let error = io::Error::last_os_error().raw_os_error().unwrap() as u32;
        if error != ERROR_PIPE_CONNECTED {
            assert_eq!(error, ERROR_IO_PENDING);
            // SAFETY: waits for the pending operation before its storage is reused.
            assert_ne!(
                unsafe { GetOverlappedResult(raw, &operation, &mut transferred, 1) },
                0
            );
        }
    }
    let mut client = 0;
    // SAFETY: the handle is a connected pipe server and the output pointer is valid.
    assert_ne!(unsafe { GetNamedPipeClientProcessId(raw, &mut client) }, 0);
    assert_eq!(client, process.reference().pid);
    assert!(!process.has_exited().unwrap());
    let mut data = vec![0; b"OPENVMM_RESTORE_READY_V1\n".len()];
    let mut used = 0;
    while used < data.len() {
        operation = unsafe { std::mem::zeroed() };
        // SAFETY: the buffer, operation and pipe remain alive until GetOverlappedResult returns.
        if unsafe {
            ReadFile(
                raw,
                data[used..].as_mut_ptr(),
                (data.len() - used) as u32,
                ptr::null_mut(),
                &mut operation,
            )
        } == 0
        {
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(ERROR_IO_PENDING as i32)
            );
        }
        // SAFETY: completes the read before its operation or buffer is reused.
        assert_ne!(
            unsafe { GetOverlappedResult(raw, &operation, &mut transferred, 1) },
            0
        );
        assert_ne!(transferred, 0);
        used += transferred as usize;
    }
    data
}
