use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::io::{AsHandle, AsRawHandle};
use std::process::{Child, Command, ExitStatus, Stdio};

use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};

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
    use windows_sys::Win32::System::Pipes::DisconnectNamedPipe;

    // SAFETY: the owned synchronous pipe finished its previous client.
    assert_ne!(
        unsafe { DisconnectNamedPipe(server.as_handle().as_raw_handle()) },
        0
    );
}

struct StaleServer {
    child: Option<Child>,
    job: Option<OwnedHandle>,
    directory: tempfile::TempDir,
    endpoint: String,
}

impl StaleServer {
    fn new() -> io::Result<Self> {
        let directory = tempfile::tempdir()?;
        let endpoint = format!(
            r"\\.\pipe\nvx-stale-probe-{}-{}",
            std::process::id(),
            crate::host::store::LaunchId::generate().unwrap().as_str()
        );
        // SAFETY: this creates an unnamed, non-inheritable job for this fixture alone.
        let job = owned(unsafe { CreateJobObjectW(ptr::null(), ptr::null()) })
            .ok_or_else(io::Error::last_os_error)?;
        // SAFETY: this Win32 structure is valid with every field zeroed.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the job and the initialized input buffer remain valid for the call.
        if unsafe {
            SetInformationJobObject(
                raw(&job),
                JobObjectExtendedLimitInformation,
                ptr::from_ref(&limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let output = File::create(directory.path().join("helper.log"))?;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args([
                "--exact",
                "host::platform::windows::endpoint::tests::stale_probe_accept_helper",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("NVX_TEST_STALE_CONTROL_ENDPOINT", &endpoint)
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone()?))
            .stderr(Stdio::from(output));
        let child = crate::host::tests::spawn_locked(&mut command)?;
        let server = Self {
            child: Some(child),
            job: Some(job),
            directory,
            endpoint,
        };
        // SAFETY: both owned handles identify this fixture's job and its exact child.
        if unsafe {
            AssignProcessToJobObject(
                raw(server.job.as_ref().unwrap()),
                server.child.as_ref().unwrap().as_raw_handle(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        server.ready("ready")?;
        drop(connect_endpoint(
            &server.endpoint,
            server.child.as_ref().unwrap().id(),
            Duration::from_secs(2),
        )?);
        Ok(server)
    }

    fn ready(&self, name: &str) -> io::Result<()> {
        if crate::host::tests::released(|| self.directory.path().join(name).exists()) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the pipe helper was not ready",
            ))
        }
    }

    fn accept(&self) -> io::Result<()> {
        std::fs::write(self.directory.path().join("accept"), b"")?;
        self.ready("accepting")
    }

    fn wait(&mut self, deadline: Instant) -> io::Result<Option<ExitStatus>> {
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait()? {
                return Ok(Some(status));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            std::thread::sleep(left.min(Duration::from_millis(25)));
        }
    }

    fn stop(&mut self) -> io::Result<()> {
        drop(self.job.take());
        if self.child.is_none() {
            return Ok(());
        }
        // Also owns cleanup if admission failed before the child entered the job.
        let termination = self.child.as_mut().unwrap().kill();
        if self
            .wait(Instant::now() + Duration::from_secs(2))?
            .is_some()
        {
            drop(self.child.take());
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("cannot confirm the pipe helper exited; termination: {termination:?}"),
        ))
    }

    fn finish(mut self, client: io::Result<()>) -> io::Result<()> {
        if client.is_err() {
            if let Err(error) = self.stop() {
                eprintln!("pipe-helper cleanup also failed: {error}");
            }
            return client;
        }
        let status = self
            .wait(Instant::now() + Duration::from_secs(2))?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "the pipe helper did not exit")
            })?;
        drop(self.child.take());
        if !status.success() {
            return Err(io::Error::other(format!(
                "the pipe helper exited with {status}: {}",
                std::fs::read_to_string(self.directory.path().join("helper.log"))?
            )));
        }
        Ok(())
    }
}

impl Drop for StaleServer {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("cannot clean up the owned pipe helper: {error}");
        }
    }
}

#[test]
fn a_stale_control_probe_is_disconnected_before_the_actual_client_is_accepted() {
    let server = StaleServer::new().unwrap();
    server.accept().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let result = (|| {
        let mut client = loop {
            let left = time_left(Some(deadline))?.unwrap();
            match connect_endpoint(
                &server.endpoint,
                server.child.as_ref().unwrap().id(),
                left.min(Duration::from_millis(25)),
            ) {
                Ok(client) => break client,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        };
        client.write_all(&[7], Some(Duration::from_secs(2)))?;
        let mut response = [0];
        assert_eq!(client.read(&mut response, Some(Duration::from_secs(2)))?, 1);
        assert_eq!(response, [9]);
        Ok(())
    })();
    server.finish(result).unwrap();
}

#[test]
fn client_failure_timeout_and_panic_reap_the_stale_accept_helper_and_preserve_the_cause() {
    for (accepting, failure) in [
        (false, "connect"),
        (true, "connect"),
        (false, "panic"),
        (true, "panic"),
        (false, "timeout"),
    ] {
        let server = StaleServer::new().unwrap();
        let process = server
            .child
            .as_ref()
            .unwrap()
            .as_handle()
            .try_clone_to_owned()
            .unwrap();
        if accepting {
            server.accept().unwrap();
        }
        let client_error = if failure == "timeout" {
            let deadline = Instant::now() + Duration::from_millis(50);
            loop {
                let left = match time_left(Some(deadline)) {
                    Ok(left) => left.unwrap(),
                    Err(error) => break error,
                };
                match connect_endpoint(
                    &server.endpoint,
                    server.child.as_ref().unwrap().id(),
                    left.min(Duration::from_millis(25)),
                ) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => {
                        panic!("unexpected failure waiting for the busy fixture: {error}")
                    }
                    Ok(_) => panic!("the unreleased stale fixture must stay busy"),
                }
            }
        } else {
            connect_endpoint(
                &format!("{}-missing", server.endpoint),
                server.child.as_ref().unwrap().id(),
                Duration::from_secs(2),
            )
            .err()
            .expect("the deliberately missing client endpoint must fail")
        };
        let original = (
            client_error.kind(),
            client_error.raw_os_error(),
            client_error.to_string(),
        );
        let started = Instant::now();
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            if failure == "panic" {
                let _owned = server;
                std::panic::panic_any("forced stale-client panic");
            }
            server.finish(Err(client_error))
        }));
        if failure == "panic" {
            assert_eq!(
                failed.unwrap_err().downcast_ref::<&str>(),
                Some(&"forced stale-client panic")
            );
        } else {
            let error = failed.unwrap().unwrap_err();
            assert_eq!(error.kind(), original.0);
            assert_eq!(error.raw_os_error(), original.1);
            assert_eq!(error.to_string(), original.2);
        }
        // SAFETY: this held process handle still names the exact owned helper, not a reused PID.
        assert_eq!(
            unsafe { WaitForSingleObject(raw(&process), 0) },
            WAIT_OBJECT_0
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

#[test]
#[ignore = "invoked as an owned subprocess by the stale-pipe tests"]
fn stale_probe_accept_helper() {
    let endpoint = std::env::var("NVX_TEST_STALE_CONTROL_ENDPOINT").unwrap();
    let server = control_server(&endpoint);
    std::fs::write("ready", b"").unwrap();
    assert!(crate::host::tests::released(|| std::path::Path::new(
        "accept"
    )
    .exists()));
    std::fs::write("accepting", b"").unwrap();
    accept_control(&server);
    let mut request = [0];
    (&server).read_exact(&mut request).unwrap();
    assert_eq!(request, [7]);
    (&server).write_all(&[9]).unwrap();
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

#[test]
fn a_wait_that_times_out_early_waits_again_until_its_deadline() {
    // `WaitForSingleObject` can time out before as long as it was given has passed by `Instant`;
    // this one always times out at once.
    let deadline = Instant::now() + Duration::from_micros(20_500);
    assert!(!wait_until(Some(deadline), |_| WAIT_TIMEOUT));
    assert!(Instant::now() >= deadline);
    // A deadline that passed still checks the event, once, without waiting.
    let mut waits = Vec::new();
    assert!(wait_until(Some(Instant::now()), |milliseconds| {
        waits.push(milliseconds);
        WAIT_OBJECT_0
    }));
    assert_eq!(waits, [0]);
}
