#![cfg(any(target_os = "linux", windows))]
#![cfg_attr(windows, allow(unsafe_code))]

// It speaks version 1 of NVXC, which only the guest agents and the fake OpenVMM build.
#[cfg(all(
    feature = "cli",
    any(
        feature = "testing",
        all(feature = "guest", target_os = "linux", target_arch = "x86_64")
    )
))]
mod live_init;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use super::*;
use crate::ErrorKind;
use crate::activation::{
    ActivationRecord, Configuration, ExpectedResources, Export, ExportKind, Image, MachineIdMode,
    OverlayMode, ReadyActivation, RestoreSection, TransactionId, Upper, View,
};
use crate::host::process::{BootId, Opened};
use crate::host::tests::spawn_locked;
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
    let Opened::Running(process) = Process::open(&ProcessRef {
        pid,
        start_time: platform::process_start_time(pid).unwrap().unwrap(),
        boot: BootId::current().unwrap(),
    })
    .unwrap() else {
        panic!("the fixture process runs");
    };
    process
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
        let child = spawn_locked(&mut command).unwrap();
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
    use std::os::unix::fs::MetadataExt;
    use std::thread;

    let namespace = fs::read_link("/proc/self/ns/pid").unwrap();
    let mount = fs::read_link("/proc/self/ns/mnt").unwrap();
    let outer = "NVX_TEST_READY_OUTER_PID_NAMESPACE";
    let outer_mount = "NVX_TEST_READY_OUTER_MOUNT_NAMESPACE";
    let verified = "NVX-READY-PID-REUSE-VERIFIED";
    if std::env::var_os(outer).is_none() {
        let mut diagnostics = String::new();
        for privileged in [false, true] {
            let route = if privileged { "sudo" } else { "userns" };
            let mut command = Command::new(if privileged { "sudo" } else { "unshare" });
            if privileged {
                command.args([
                    "-n",
                    "--preserve-env=NVX_TEST_READY_OUTER_PID_NAMESPACE,NVX_TEST_READY_OUTER_MOUNT_NAMESPACE,TMPDIR,TEMP,TMP",
                    "/usr/bin/unshare",
                ]);
            } else {
                command.args(["--user", "--map-root-user"]);
            }
            command
                .args([
                    "--mount",
                    "--propagation",
                    "private",
                    "--pid",
                    "--fork",
                    "--mount-proc",
                    "/bin/bash",
                    "-c",
                    "\"$@\"; result=$?; exit \"$result\"",
                    "--",
                ])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "host::restore::tests::a_queued_peer_cannot_be_the_vm_that_reuses_its_pid",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(outer, &namespace)
                .env(outer_mount, &mount)
                .env("LC_ALL", "C")
                .env_remove("BASH_ENV")
                .env_remove("ENV")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let output = spawn_locked(&mut command)
                .and_then(Child::wait_with_output)
                .unwrap_or_else(|error| panic!("{diagnostics}\n{route} helper: {error}"));
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            diagnostics.push_str(&format!("{route}: {}\n{stdout}\n{stderr}\n", output.status));
            if output.status.success() {
                assert!(stdout.contains(verified), "{diagnostics}");
                println!("{route}: {stdout}");
                return;
            }
            assert!(
                !privileged
                    && output.status.code() == Some(1)
                    && stderr.contains("unshare:")
                    && (stderr.contains("Operation not permitted")
                        || stderr.contains("Permission denied")),
                "{diagnostics}"
            );
        }
        unreachable!("both namespace setup failures must fail the test");
    }
    // Both namespaces must be private before any PID allocation, reaping, or signal in the test.
    assert_ne!(namespace.as_os_str(), std::env::var_os(outer).unwrap());
    assert_ne!(mount.as_os_str(), std::env::var_os(outer_mount).unwrap());
    assert_eq!(nix::unistd::getppid().as_raw(), 1);
    assert_eq!(fs::read_to_string("/proc/1/comm").unwrap().trim(), "bash");
    let (_temporary, directory) = vm_directory();
    let mut listener = ReadyListener::bind(&directory).unwrap();
    let mut peer = Client::start(listener.endpoint(), b"OPENVMM_RESTORE_READY_V1\n", true);
    let recycled = peer.child.id();
    let uid = fs::metadata(format!("/proc/{recycled}")).unwrap().uid();
    assert_eq!(uid, nix::unistd::geteuid().as_raw());
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
    let child = spawn_locked(&mut command).unwrap();
    assert_eq!(child.id(), recycled);
    let process = held_process(child.id());
    let mut vm = Client { child, process };
    assert_eq!(
        fs::metadata(format!("/proc/{recycled}")).unwrap().uid(),
        uid
    );
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
    println!(
        "{verified} pid={recycled} uid={uid} reaper=bash pidns={} mountns={}",
        namespace.display(),
        mount.display()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn the_receiver_fails_closed_without_original_peer_pidfds() {
    let child = "NVX_TEST_READY_PIDFD_DENIAL";
    if std::env::var_os(child).is_none() {
        for errno in [nix::libc::ENOPROTOOPT, nix::libc::EPERM, nix::libc::EINVAL] {
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
            let output = spawn_locked(&mut command)
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
    let expected = if std::env::var(child).unwrap() == nix::libc::EINVAL.to_string() {
        ErrorKind::Protocol
    } else {
        ErrorKind::Unavailable
    };
    assert_eq!(error.kind(), expected);
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

fn restore_record() -> ActivationRecord {
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

fn restore_machine_at(phase: RestorePhase) -> RestoreMachine {
    let mut machine = RestoreMachine::new(&restore_record());
    machine.input(RestoreInput::Spawned(ProcessRef {
        pid: 7,
        start_time: 11,
        boot: BootId::from_bytes([3; 16]),
    }));
    assert!(matches!(
        machine.next_action(),
        Some(RestoreAction::AwaitReadyEvent)
    ));
    if phase == RestorePhase::ReadyEvent {
        return machine;
    }
    machine.input(RestoreInput::ReadyEvent(
        b"OPENVMM_RESTORE_READY_V1\n".to_vec(),
    ));
    assert!(matches!(
        machine.next_action(),
        Some(RestoreAction::Connect)
    ));
    if phase == RestorePhase::Connect {
        return machine;
    }
    machine.input(RestoreInput::Connected);
    assert!(matches!(
        machine.next_action(),
        Some(RestoreAction::SendActivate(_))
    ));
    if phase == RestorePhase::Ready {
        machine.input(RestoreInput::ActivateSent);
    }
    machine
}

fn restore_failure(machine: &mut RestoreMachine) -> RestoreError {
    let Some(RestoreAction::Fail(error)) = machine.next_action() else {
        panic!("the restore did not fail");
    };
    assert!(machine.next_action().is_none());
    error
}

#[test]
fn restore_machine_resends_canonical_bytes_and_stops_at_ready() {
    let record = restore_record();
    let mut machine = restore_machine_at(RestorePhase::Connect);
    for _ in 0..5 {
        machine.input(RestoreInput::Connected);
        let Some(RestoreAction::SendActivate(bytes)) = machine.next_action() else {
            panic!("the session received no activation");
        };
        assert_eq!(bytes, record.encode());
        assert!(machine.next_action().is_none());
        machine.input(RestoreInput::ActivateSent);
        machine.input(RestoreInput::SessionLost);
        assert!(matches!(
            machine.next_action(),
            Some(RestoreAction::Connect)
        ));
    }
    machine.input(RestoreInput::Connected);
    assert!(matches!(
        machine.next_action(),
        Some(RestoreAction::SendActivate(_))
    ));
    machine.input(RestoreInput::ActivateSent);
    machine.input(RestoreInput::GuestReady(ReadyActivation::of(&record)));
    assert!(matches!(machine.next_action(), Some(RestoreAction::Done)));
    machine.input(RestoreInput::SessionLost);
    machine.input(RestoreInput::Cancelled);
    assert!(machine.next_action().is_none());
}

#[test]
fn restore_machine_checks_the_transaction_and_refuses_a_cold_record() {
    for transaction in [None, Some(TransactionId::from_bytes([2; 16]))] {
        let mut machine = restore_machine_at(RestorePhase::Ready);
        machine.input(RestoreInput::GuestReady(ReadyActivation { transaction }));
        let error = restore_failure(&mut machine);
        assert_eq!(error.phase, RestorePhase::Ready);
        assert_eq!(error.evidence, RestoreEvidence::Malformed);
        assert_eq!(error.attribution, Attribution::Unknown);
        assert!(error.guest_may_have_run);
        assert!(error.source.is_some());
    }
    let cold = ActivationRecord::new(None, restore_record().configuration().clone()).unwrap();
    let error = restore_failure(&mut RestoreMachine::new(&cold));
    assert_eq!(error.phase, RestorePhase::ReadyEvent);
    assert_eq!(error.evidence, RestoreEvidence::InvalidActivate);
    assert_eq!(error.attribution, Attribution::Request);
    assert!(!error.guest_may_have_run);
    assert!(
        error
            .message
            .contains("activation record with a transaction")
    );
}

#[test]
fn restore_machine_session_loss_invalidates_each_session_phase() {
    for phase in [
        RestorePhase::Connect,
        RestorePhase::Activate,
        RestorePhase::Ready,
    ] {
        let mut machine = restore_machine_at(phase);
        machine.input(RestoreInput::SessionLost);
        assert!(matches!(
            machine.next_action(),
            Some(RestoreAction::Connect)
        ));
        assert!(machine.next_action().is_none());
        machine.input(RestoreInput::Connected);
        let Some(RestoreAction::SendActivate(bytes)) = machine.next_action() else {
            panic!("the replacement session received no activation");
        };
        assert_eq!(bytes, restore_record().encode());
        machine.input(RestoreInput::GuestReady(ReadyActivation::of(
            &restore_record(),
        )));
        assert!(matches!(machine.next_action(), Some(RestoreAction::Done)));
    }

    let mut machine = restore_machine_at(RestorePhase::Connect);
    machine.input(RestoreInput::Connected);
    machine.input(RestoreInput::SessionLost);
    assert!(matches!(
        machine.next_action(),
        Some(RestoreAction::Connect)
    ));
    assert!(machine.next_action().is_none());
}

#[test]
fn restore_machine_keeps_operation_phase_evidence_and_original_source() {
    for phase in [
        RestorePhase::ReadyEvent,
        RestorePhase::Connect,
        RestorePhase::Activate,
        RestorePhase::Ready,
    ] {
        let mut machine = restore_machine_at(phase);
        let source = Box::new(io::Error::other("original operation failure"));
        let original: *const io::Error = &*source;
        machine.input(RestoreInput::OperationFailed {
            phase: RestorePhase::Activate,
            evidence: RestoreEvidence::PeerMismatch,
            source: Some(source),
        });
        let error = restore_failure(&mut machine);
        assert_eq!(error.phase, RestorePhase::Activate);
        assert_eq!(error.evidence, RestoreEvidence::PeerMismatch);
        assert_eq!(error.attribution, Attribution::Unknown);
        assert!(error.guest_may_have_run);
        assert_eq!(
            error.message,
            "restore failed during Activate: PeerMismatch: original operation failure"
        );
        let returned = StdError::source(&error)
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        assert!(std::ptr::eq(original, returned));
        assert_eq!(
            error.to_string(),
            "restore failed during Activate: PeerMismatch"
        );

        let mut machine = restore_machine_at(phase);
        machine.input(RestoreInput::OperationFailed {
            phase,
            evidence: RestoreEvidence::Io,
            source: None,
        });
        let error = restore_failure(&mut machine);
        assert_eq!(error.phase, phase);
        assert_eq!(error.evidence, RestoreEvidence::Io);
        assert_eq!(error.attribution, Attribution::Host);
        assert!(error.guest_may_have_run);
        assert!(error.source.is_none());
    }
}

#[test]
fn restore_machine_expiry_cancellation_and_exit_fail_every_pending_phase() {
    for phase in [
        RestorePhase::ReadyEvent,
        RestorePhase::Connect,
        RestorePhase::Activate,
        RestorePhase::Ready,
    ] {
        for (input, evidence) in [
            (RestoreInput::Expired, RestoreEvidence::Expired),
            (RestoreInput::Cancelled, RestoreEvidence::Cancelled),
            (RestoreInput::Exited(None), RestoreEvidence::Exited),
            (
                RestoreInput::Exited(Some(Report {
                    status_code: Some(0),
                    ..Report::default()
                })),
                RestoreEvidence::Exited,
            ),
        ] {
            let mut machine = restore_machine_at(phase);
            machine.input(input);
            machine.input(RestoreInput::Connected);
            machine.input(RestoreInput::GuestReady(ReadyActivation::of(
                &restore_record(),
            )));
            let error = restore_failure(&mut machine);
            assert_eq!(error.phase, phase);
            assert_eq!(error.evidence, evidence);
            assert_eq!(error.attribution, Attribution::Unknown);
            assert!(error.guest_may_have_run);
            assert!(error.message.contains(&format!("{phase:?}")));
            assert!(error.source.is_none());
        }
    }
}

#[test]
fn restore_failures_attribute_the_existing_policy_free_evidence_classes() {
    let classes: [(&[RestoreEvidence], Attribution); 5] = [
        (
            &[
                RestoreEvidence::InvalidTargets,
                RestoreEvidence::InvalidSettings,
                RestoreEvidence::InvalidActivate,
                RestoreEvidence::InvalidMappings,
                RestoreEvidence::UnservableImage,
                RestoreEvidence::UnusableScratch,
                RestoreEvidence::UnusableRoot,
                RestoreEvidence::SuppliedIdentity,
                RestoreEvidence::SuppliedLength,
                RestoreEvidence::SuppliedGeometry,
            ],
            Attribution::Request,
        ),
        (
            &[
                RestoreEvidence::Io,
                RestoreEvidence::ReportUnreadable,
                RestoreEvidence::EndpointBind,
                RestoreEvidence::ForwardBind,
                RestoreEvidence::ResourceExhaustion,
                RestoreEvidence::IncompatibleHost,
                RestoreEvidence::RuntimeMismatch,
                RestoreEvidence::HostFailure,
            ],
            Attribution::Host,
        ),
        (
            &[
                RestoreEvidence::PeerMismatch,
                RestoreEvidence::Malformed,
                RestoreEvidence::Expired,
                RestoreEvidence::Cancelled,
                RestoreEvidence::Exited,
                RestoreEvidence::LostStatus,
                RestoreEvidence::Reset,
                RestoreEvidence::ForcedKill,
            ],
            Attribution::Unknown,
        ),
        (
            &[
                RestoreEvidence::MalformedSnapshot,
                RestoreEvidence::SnapshotDefect,
            ],
            Attribution::Snapshot,
        ),
        (&[RestoreEvidence::RepairStatus], Attribution::Repair),
    ];
    for phase in [
        RestorePhase::ReadyEvent,
        RestorePhase::Connect,
        RestorePhase::Activate,
        RestorePhase::Ready,
    ] {
        for (evidence, attribution) in classes {
            for &evidence in evidence {
                let mut machine = restore_machine_at(phase);
                machine.input(RestoreInput::OperationFailed {
                    phase,
                    evidence,
                    source: None,
                });
                let error = restore_failure(&mut machine);
                assert_eq!(error.phase, phase);
                assert_eq!(error.evidence, evidence);
                assert_eq!(error.attribution, attribution);
                assert!(error.guest_may_have_run);
                assert!(!error.message.is_empty());
                assert!(error.source.is_none());
            }
        }
    }
}

#[test]
fn restore_exit_reports_keep_guest_status_classification_and_actual_details() {
    let classes = [
        (
            status::ACTIVATION_DEADLINE,
            RestoreEvidence::Expired,
            Attribution::Unknown,
        ),
        (
            status::ACTIVATION_PROTOCOL,
            RestoreEvidence::Malformed,
            Attribution::Unknown,
        ),
        (
            status::ACTIVATION_ORDER,
            RestoreEvidence::Malformed,
            Attribution::Unknown,
        ),
        (
            status::ACTIVATION_RECORD,
            RestoreEvidence::InvalidActivate,
            Attribution::Request,
        ),
        (
            status::ACTIVATION_RESOURCES,
            RestoreEvidence::InvalidTargets,
            Attribution::Request,
        ),
        (
            status::ACTIVATION_ROOT,
            RestoreEvidence::UnusableRoot,
            Attribution::Request,
        ),
        (
            status::ACTIVATION_WORKLOAD,
            RestoreEvidence::InvalidSettings,
            Attribution::Request,
        ),
        (
            status::ACTIVATION_NETWORK,
            RestoreEvidence::InvalidSettings,
            Attribution::Request,
        ),
        (
            status::ACTIVATION_EXPORT,
            RestoreEvidence::InvalidMappings,
            Attribution::Request,
        ),
    ];
    for phase in [
        RestorePhase::ReadyEvent,
        RestorePhase::Connect,
        RestorePhase::Activate,
        RestorePhase::Ready,
    ] {
        for category in ["guest-exit", "success"] {
            let repair = status::TIME_STATUSES
                .chain(status::REPAIR_STATUSES)
                .map(|code| (code, RestoreEvidence::RepairStatus, Attribution::Repair));
            for (code, evidence, attribution) in classes.into_iter().chain(repair) {
                let mut machine = restore_machine_at(phase);
                machine.input(RestoreInput::Exited(Some(Report {
                    category: Some(category.to_owned()),
                    status_code: Some(i64::from(code)),
                    teardown_complete: Some(true),
                })));
                let error = restore_failure(&mut machine);
                assert_eq!(error.phase, phase);
                assert_eq!(error.evidence, evidence);
                assert_eq!(error.attribution, attribution);
                assert!(error.guest_may_have_run);
                assert!(error.message.contains(category));
                assert!(error.message.contains(&format!("status Some({code})")));
                assert!(error.source.is_none());
            }
        }
    }
    for (category, code) in [
        (None, Some(195)),
        (Some("vmm-failure"), Some(195)),
        (Some("teardown-failure"), Some(195)),
        (Some("guest-exit"), None),
        (Some("guest-exit"), Some(-1)),
        (Some("guest-exit"), Some(256)),
        (Some("guest-exit"), Some(0)),
        (Some("guest-exit"), Some(137)),
    ] {
        let mut machine = restore_machine_at(RestorePhase::Ready);
        machine.input(RestoreInput::Exited(Some(Report {
            category: category.map(str::to_owned),
            status_code: code,
            teardown_complete: None,
        })));
        let error = restore_failure(&mut machine);
        assert_eq!(error.evidence, RestoreEvidence::Exited);
        assert_eq!(error.attribution, Attribution::Unknown);
        assert!(error.message.contains(&format!("status {code:?}")));
    }
}

#[test]
fn restore_guest_execution_possibility_is_false_before_spawn_and_sticky_after_it() {
    let mut machine = RestoreMachine::new(&restore_record());
    machine.input(RestoreInput::Cancelled);
    machine.input(RestoreInput::Spawned(
        held_process(std::process::id()).reference().clone(),
    ));
    assert!(!restore_failure(&mut machine).guest_may_have_run);

    for phase in [
        RestorePhase::Connect,
        RestorePhase::Activate,
        RestorePhase::Ready,
    ] {
        let mut machine = restore_machine_at(phase);
        machine.input(RestoreInput::SessionLost);
        assert!(matches!(
            machine.next_action(),
            Some(RestoreAction::Connect)
        ));
        machine.input(RestoreInput::OperationFailed {
            phase: RestorePhase::ReadyEvent,
            evidence: RestoreEvidence::Io,
            source: None,
        });
        let error = restore_failure(&mut machine);
        assert_eq!(error.phase, RestorePhase::ReadyEvent);
        assert!(error.guest_may_have_run);
    }
}

#[test]
fn restore_failure_messages_keep_the_original_cause_chain_without_replacing_it() {
    let source = Box::new(
        Error::io("activation write failed")
            .with_source(Error::io("transport failed").with_source(io::Error::other("root cause"))),
    );
    let original: *const Error = &*source;
    let mut machine = restore_machine_at(RestorePhase::Ready);
    machine.input(RestoreInput::OperationFailed {
        phase: RestorePhase::Activate,
        evidence: RestoreEvidence::Io,
        source: Some(source),
    });
    let error = restore_failure(&mut machine);
    assert_eq!(error.attribution, Attribution::Host);
    assert!(error.message.contains("activation write failed"));
    assert!(error.message.contains("transport failed"));
    assert!(error.message.ends_with("root cause"));
    assert_eq!(error.to_string(), "restore failed during Activate: Io");
    let returned = StdError::source(&error)
        .unwrap()
        .downcast_ref::<Error>()
        .unwrap();
    assert!(std::ptr::eq(original, returned));
}

#[test]
fn restore_failure_and_drop_leave_real_running_or_exited_processes_and_caller_files_unchanged() {
    use std::collections::BTreeSet;
    use std::fs;

    for exited in [false, true] {
        let (_temporary, directory) = vm_directory();
        let state = directory.path().join("caller-state");
        fs::write(&state, b"caller-owned").unwrap();
        let listener = ReadyListener::bind(&directory).unwrap();
        let mut client = Client::start(listener.endpoint(), b"OPENVMM_RESTORE_READY_V1\n", false);
        if exited {
            client.stop();
        }
        let entries = || {
            fs::read_dir(directory.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<BTreeSet<_>>()
        };
        let before = entries();
        let mut machine = RestoreMachine::new(&restore_record());
        machine.input(RestoreInput::Spawned(client.process.reference().clone()));
        assert!(matches!(
            machine.next_action(),
            Some(RestoreAction::AwaitReadyEvent)
        ));
        machine.input(if exited {
            RestoreInput::Exited(None)
        } else {
            RestoreInput::Cancelled
        });
        let error = restore_failure(&mut machine);
        assert!(error.guest_may_have_run);
        drop(machine);
        drop(error);
        assert_eq!(client.process.has_exited().unwrap(), exited);
        assert_eq!(client.child.try_wait().unwrap().is_some(), exited);
        assert_eq!(entries(), before);
        assert_eq!(fs::read(&state).unwrap(), b"caller-owned");
        client.stop();
        assert!(client.process.has_exited().unwrap());
    }
}

#[test]
fn restore_machine_refuses_malformed_events_and_out_of_order_progress() {
    for event in [
        b"OPENVMM_RESTORE_READY_V1".to_vec(),
        b"OPENVMM_RESTORE_READY_V1\nx".to_vec(),
    ] {
        let mut machine = restore_machine_at(RestorePhase::ReadyEvent);
        machine.input(RestoreInput::ReadyEvent(event));
        let error = restore_failure(&mut machine);
        assert_eq!(error.phase, RestorePhase::ReadyEvent);
        assert_eq!(error.evidence, RestoreEvidence::Malformed);
        assert!(error.source.is_some());
    }
    let mut machine = RestoreMachine::new(&restore_record());
    machine.input(RestoreInput::Connected);
    assert_eq!(
        restore_failure(&mut machine).evidence,
        RestoreEvidence::Malformed
    );
    let mut machine = restore_machine_at(RestorePhase::Connect);
    machine.input(RestoreInput::Connected);
    machine.input(RestoreInput::GuestReady(ReadyActivation::of(
        &restore_record(),
    )));
    assert_eq!(
        restore_failure(&mut machine).evidence,
        RestoreEvidence::Malformed
    );
}
