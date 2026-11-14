use std::error::Error as _;

use super::*;
use crate::protocol::nvxs::{ErrorCode, Identity};

#[test]
fn connection_failures_keep_their_description_and_cause_and_tell_a_lost_session() {
    use ConnectionErrorKind::*;

    let io = |kind| HostConnectionError::Io(io::Error::new(kind, "the transport failed"));
    let identity = Identity {
        instance_id: [7; 16],
        epoch: 2,
    };
    let cases = [
        (HostConnectionError::TimedOut, TimedOut, false),
        (HostConnectionError::Closed, Closed, false),
        (
            HostConnectionError::Rejected(ErrorCode::Authentication),
            Rejected,
            false,
        ),
        (HostConnectionError::Reset(identity), Reset, false),
        (
            HostConnectionError::Protocol(SessionError::Truncated { buffered: 3 }),
            Closed,
            true,
        ),
        (
            HostConnectionError::Protocol(SessionError::InvalidState),
            Protocol,
            true,
        ),
        (io(io::ErrorKind::TimedOut), TimedOut, true),
        (io(io::ErrorKind::BrokenPipe), Closed, true),
        (io(io::ErrorKind::ConnectionAborted), Closed, true),
        (io(io::ErrorKind::ConnectionReset), Closed, true),
        (io(io::ErrorKind::NotConnected), Closed, true),
        (io(io::ErrorKind::UnexpectedEof), Closed, true),
        (io(io::ErrorKind::WriteZero), Closed, true),
        (io(io::ErrorKind::PermissionDenied), Io, true),
        (io(io::ErrorKind::Other), Io, true),
        (HostConnectionError::Ended, Ended, false),
    ];
    for (failure, kind, caused) in cases {
        let message = failure.to_string();
        let error = ConnectionError::from(failure);
        assert_eq!(error.kind(), kind, "{message}");
        assert_eq!(error.to_string(), message);
        assert_eq!(error.is_lost(), matches!(kind, Closed | Reset), "{message}");
        match error.source() {
            Some(source) => {
                assert!(caused, "{message}");
                assert!(source.is::<SessionError>() || source.is::<io::Error>());
                assert!(message.ends_with(&source.to_string()), "{message}");
            }
            None => assert!(!caused, "{message}"),
        }
    }
}

#[test]
fn connecting_tells_a_peer_mismatch_and_keeps_the_cause() {
    let cause = |kind| io::Error::new(kind, "the endpoint failed");
    let cases = [
        (
            Error::timed_out("the control connection timed out")
                .with_source(cause(io::ErrorKind::NotFound)),
            ConnectionErrorKind::TimedOut,
        ),
        (
            Error::exited("the recorded VM process exited")
                .with_source(cause(io::ErrorKind::NotFound)),
            ConnectionErrorKind::Exited,
        ),
        (
            Error::new(
                ErrorKind::Protocol,
                "cannot connect to the control endpoint",
            )
            .with_source(cause(io::ErrorKind::PermissionDenied)),
            ConnectionErrorKind::PeerMismatch,
        ),
        (
            Error::new(
                ErrorKind::Protocol,
                "cannot connect to the control endpoint",
            )
            .with_source(cause(io::ErrorKind::ConnectionReset)),
            ConnectionErrorKind::Closed,
        ),
        (
            Error::io("cannot verify the endpoint's VM process")
                .with_source(cause(io::ErrorKind::Other)),
            ConnectionErrorKind::Io,
        ),
    ];
    for (failure, kind) in cases {
        let message = failure.to_string();
        let error = ConnectionError::from_connect(failure);
        assert_eq!(error.kind(), kind, "{message}");
        assert_eq!(error.to_string(), message);
        assert_eq!(
            error.source().unwrap().to_string(),
            "the endpoint failed",
            "{message}"
        );
    }
}

#[test]
fn a_connection_failure_becomes_the_crates_error_whose_source_it_is() {
    let cases = [
        (HostConnectionError::TimedOut, ErrorKind::TimedOut),
        (HostConnectionError::Closed, ErrorKind::Protocol),
        (
            HostConnectionError::Rejected(ErrorCode::Authentication),
            ErrorKind::Protocol,
        ),
        (
            HostConnectionError::Io(io::Error::other("the transport failed")),
            ErrorKind::Io,
        ),
        (HostConnectionError::Ended, ErrorKind::Protocol),
    ];
    for (failure, kind) in cases {
        let failure = ConnectionError::from(failure);
        let message = failure.to_string();
        let failure_kind = failure.kind();
        let error = Error::from(failure);
        assert_eq!(error.kind(), kind, "{message}");
        assert_eq!(error.message(), message);
        let source = error
            .source()
            .unwrap()
            .downcast_ref::<ConnectionError>()
            .unwrap();
        assert_eq!(source.kind(), failure_kind);
    }
    let exited = ConnectionError::from_connect(Error::exited("the recorded VM process exited"));
    assert_eq!(Error::from(exited).kind(), ErrorKind::Exited);
}

#[test]
fn a_connection_and_a_console_move_to_another_thread() {
    fn movable<T: Send>() {}
    movable::<Connection>();
    movable::<Console>();
}

#[cfg(any(target_os = "linux", windows))]
mod console {
    use std::io::{Read, Write};
    use std::sync::mpsc;
    use std::thread;

    use super::*;
    use crate::host::transport::tests::{accept, bind, endpoint, reference};

    fn outputs(console: Option<String>) -> VmOutputs {
        VmOutputs {
            log: "openvmm.log".into(),
            outcome: "outcome.json".into(),
            endpoint: "control".to_owned(),
            console,
        }
    }

    #[test]
    fn a_vm_without_a_console_listener_has_no_console() {
        let process = reference(std::process::id());
        let error = Console::connect(&outputs(None), &process, Instant::now()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn a_console_reads_what_arrived_without_waiting_once_its_deadline_passed() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = endpoint(&directory);
        let server = bind(&endpoint);
        let (written, wait) = mpsc::channel();
        let guest = thread::spawn(move || {
            let mut peer = accept(server);
            peer.write_all(b"boot").unwrap();
            written.send(()).unwrap();
            let mut input = [0; 5];
            peer.read_exact(&mut input).unwrap();
            assert_eq!(&input, b"input");
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut console = Console::connect(
            &outputs(Some(endpoint)),
            &reference(std::process::id()),
            deadline,
        )
        .unwrap();
        wait.recv_timeout(Duration::from_secs(10)).unwrap();
        let mut received = Vec::new();
        let mut buffer = [0; 16];
        while received.len() < 4 {
            let count = console.read(&mut buffer, Some(deadline)).unwrap();
            assert_ne!(count, 0);
            received.extend_from_slice(&buffer[..count]);
        }
        assert_eq!(received, b"boot");
        let started = Instant::now();
        let error = console.read(&mut buffer, Some(started)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
        console.write_all(b"input", Some(deadline)).unwrap();
        guest.join().unwrap();
        assert_eq!(console.read(&mut buffer, Some(deadline)).unwrap(), 0);
    }
}
