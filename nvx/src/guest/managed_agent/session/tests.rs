use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::*;
use crate::guest::snapshot::{CaptureCause, CaptureFailure, CaptureStage};
use crate::protocol::{
    APP_CAPTURE, APP_READY, OUTER_ACK, OUTER_CREDIT, OUTER_DATA, OUTER_GUEST_ATTACH,
    OUTER_HEADER_LEN, OUTER_RESET, OuterHeader, decode_outer_header, encode_outer,
};

const INSTANCE: [u8; 16] = [7; 16];
/// The kind of `BUSY`, with which the agent refuses a request while an execution runs.
const APP_BUSY: u8 = 0x87;

impl Session<'_> {
    pub(crate) fn activation_state_for_test(&self) -> Option<GuestRestoreState> {
        self.capture
            .as_ref()?
            .restored
            .as_ref()
            .map(|state| state.activation.state())
    }
}

struct Peer {
    stream: UnixStream,
    epoch: u64,
    host_sequence: u64,
    guest_sequence: u64,
}

impl Peer {
    fn reset(&mut self, epoch: u64, sequence: u64) {
        self.stream
            .write_all(&encode_outer(OUTER_RESET, &INSTANCE, epoch, sequence, &[]).unwrap())
            .unwrap();
        self.epoch = epoch;
        self.host_sequence = sequence + 1;
        self.guest_sequence = 0;
    }

    fn read(&mut self) -> (OuterHeader, Vec<u8>) {
        let mut bytes = [0; OUTER_HEADER_LEN];
        self.stream.read_exact(&mut bytes).unwrap();
        let header = decode_outer_header(&bytes).unwrap();
        let mut payload = vec![0; header.length];
        self.stream.read_exact(&mut payload).unwrap();
        assert_eq!(header.sequence, self.guest_sequence);
        self.guest_sequence += 1;
        (header, payload)
    }

    /// Reads the agent's next frame, of version 2, and checks its kind, request ID and status, and
    /// its payload, which follows a `READY`'s protocol version.
    fn app(&mut self, kind: u8, id: u64, status: i32, payload: &[u8]) {
        let (header, bytes) = self.read();
        assert_eq!(header.record_type, OUTER_DATA);
        assert_eq!(header.epoch, self.epoch);
        let (request_id, frame) = v2::Frame::decode(&bytes).unwrap();
        let mut carried = &bytes[protocol::APP_HEADER_LEN..];
        if let v2::Frame::Ready {
            version,
            activation,
        } = frame
        {
            assert_eq!(version, v2::VERSION);
            carried = activation;
        }
        let frame_status = i32::from_le_bytes(bytes[16..20].try_into().unwrap());
        assert_eq!(
            (bytes[5], request_id, frame_status, carried),
            (kind, id, status, payload)
        );
    }

    fn send_v2(&mut self, id: u64, frame: v2::Frame<'_>) {
        let bytes = frame.encode(id).unwrap();
        let record = encode_outer(
            OUTER_DATA,
            &INSTANCE,
            self.epoch,
            self.host_sequence,
            &bytes,
        );
        self.stream.write_all(&record.unwrap()).unwrap();
        self.host_sequence += 1;
    }

    fn credit(&mut self) {
        let (header, _) = self.read();
        assert_eq!(header.record_type, OUTER_CREDIT);
        assert_eq!(header.epoch, self.epoch);
    }
}

fn connected(fresh: bool) -> (Session<'static>, Peer) {
    let (host, guest) = UnixStream::pair().unwrap();
    let mut session = Session::new(FileDescriptor::from_owned(guest.into()))
        .with_capture(Capture::fixture(fresh));
    let mut peer = Peer {
        stream: host,
        epoch: 1,
        host_sequence: 8,
        guest_sequence: 0,
    };
    session.send_attach().unwrap();
    let (header, _) = peer.read();
    assert_eq!(header.record_type, OUTER_GUEST_ATTACH);
    peer.stream
        .write_all(&encode_outer(OUTER_RESET, &INSTANCE, 1, 7, &[]).unwrap())
        .unwrap();
    assert!(matches!(
        session
            .next_event()
            .unwrap_or_else(|_| panic!("valid initial RESET")),
        Event::Reset
    ));
    session.acknowledge().unwrap();
    peer.guest_sequence = 0;
    let (header, _) = peer.read();
    assert_eq!(header.record_type, OUTER_ACK);
    (session, peer)
}

fn restored() -> (Session<'static>, Peer, Vec<u8>) {
    let (mut session, peer) = connected(true);
    let (capture, payload) = Capture::activation_fixture();
    let deadline = capture.restored.as_ref().unwrap().activation.deadline();
    session.capture = Some(capture);
    session.descriptor().set_nonblocking().unwrap();
    session.connection.get_ref().deadline.set(deadline);
    (session, peer, payload)
}

/// Sends the host's request for Ready, a `PING` of `id`, and returns the request that the session
/// accepted, once the peer took the credit that it returned.
fn ping<'b>(
    session: &mut Session<'_>,
    peer: &mut Peer,
    id: u64,
    bytes: &'b mut Vec<u8>,
) -> protocol::AppFrame<'b> {
    peer.send_v2(id, v2::Frame::Ping);
    let Event::Data(data) = session.next_event().unwrap_or_else(|_| panic!("ping DATA")) else {
        panic!("the ping is DATA");
    };
    *bytes = data;
    let Ok(Request::App(request)) = session.accept_request(bytes) else {
        panic!("valid ping");
    };
    peer.credit();
    request
}

#[test]
fn a_restored_clone_applies_its_record_once_and_answers_each_session_s_request_for_ready() {
    let (mut session, mut peer, payload) = restored();
    let deadline = session.connection.get_ref().deadline.get();
    let (record, _) = protocol::decode_activation_record(&payload).unwrap();
    let ready = crate::guest_restore::GuestRestoreReady::of(&record).encode();
    // The record of the restore packet applies once, before the host asks for Ready.
    let retained = session
        .activate_with(|restored, _| {
            assert_eq!(restored.activation.state(), GuestRestoreState::Activating);
            assert_eq!(restored.activation.deadline(), deadline);
            Ok(Some("one applied space"))
        })
        .unwrap();
    assert_eq!(retained, Some("one applied space"));
    assert_eq!(
        session.activation_state_for_test(),
        Some(GuestRestoreState::Applied)
    );
    assert!(
        session
            .activate_with::<()>(|_, _| panic!("must not apply or switch twice"))
            .unwrap()
            .is_none()
    );
    // Root-dependent work waits for the host's request for Ready, by the same deadline.
    assert!(session.rootless());
    assert_eq!(session.connection.get_ref().deadline.get(), deadline);
    let mut bytes = Vec::new();
    let request = ping(&mut session, &mut peer, 61, &mut bytes);
    session.ready(request).unwrap();
    peer.app(APP_READY, 61, 0, &ready);
    assert!(!session.rootless());
    assert_eq!(session.connection.get_ref().deadline.get(), None);
    // A later session's request gets the same Ready, without applying or switching again.
    peer.reset(2, 4);
    assert!(matches!(session.next_event(), Ok(Event::Reset)));
    session.acknowledge().unwrap();
    assert_eq!(peer.read().0.record_type, OUTER_ACK);
    let request = ping(&mut session, &mut peer, 62, &mut bytes);
    session.ready(request).unwrap();
    peer.app(APP_READY, 62, 0, &ready);
    session.start_activation().unwrap();
    session.start_activation().unwrap();
    assert_eq!(
        session.activation_state_for_test(),
        Some(GuestRestoreState::Started)
    );
}

#[test]
fn postactivation_capture_stays_refused_without_pending_cleanup() {
    let (mut session, mut peer, _) = restored();
    let state = session.capture.as_mut().unwrap().restored.as_mut().unwrap();
    state.activation.complete(Instant::now(), Ok(())).unwrap();
    state.ready(Instant::now()).unwrap();
    session.start_activation().unwrap();
    for kill in [0, 1] {
        assert!(!queue(&mut session, &mut peer, 71, &[kill], false));
        assert!(!session.capture_pending());
        assert!(session.finish_capture().unwrap().is_none());
        peer.app(
            APP_ERROR,
            71,
            Errno::ENOTSUP as i32,
            b"unsupported-operation",
        );
    }
}

#[test]
fn failed_application_never_reports_ready_or_admits_root_dependent_work() {
    let (mut session, mut peer, _) = restored();
    assert_eq!(
        session
            .activate_with::<()>(|_, _| Err(crate::Error::io("runtime mounts failed")))
            .err(),
        Some(Errno::EPROTO)
    );
    assert!(session.rootless());
    assert_eq!(
        session.activation_state_for_test(),
        Some(GuestRestoreState::Failed)
    );
    peer.stream.set_nonblocking(true).unwrap();
    assert_eq!(
        peer.stream.read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn a_failed_restored_activation_gives_init_the_status_of_its_class() {
    let (mut session, _peer) = connected(true);
    assert_eq!(session.activation_status(), None);
    let (mut session, _peer, _) = restored();
    assert_eq!(session.activation_status(), None);
    let failure = crate::Error::io("the network settings cannot be applied").with_source(
        crate::guest::restore::ApplyError::Network {
            error: crate::guest::restore::state::apply::NetworkError::Gateway,
            teardown: None,
        },
    );
    assert_eq!(
        session.activate_with::<()>(|_, _| Err(failure)).err(),
        Some(Errno::EPROTO)
    );
    assert_eq!(
        session.activation_status(),
        Some(protocol::status::GUEST_RESTORE_NETWORK)
    );
}

#[test]
fn a_restored_capture_whose_packet_carried_no_record_ends_before_its_session() {
    let (mut session, mut peer) = connected(true);
    assert!(queue(&mut session, &mut peer, 72, &[0], false));
    let result = session.finish_capture_with(|capture, session| {
        capture.perform_with(
            |_, _| Ok(()),
            |id| session.send_app(APP_READY, id, 0, &[]),
            |_, _, _| {
                Ok(Captured::Restored(
                    crate::guest::snapshot::repair::tests::repaired_fixture(),
                ))
            },
        )
    });
    assert_eq!(result.err(), Some(Errno::EPROTO));
    peer.app(APP_READY, 72, 0, &[]);
    // The clone took no reset, and acknowledged none: it ends with the record's status.
    assert_eq!(
        session.activation_status(),
        Some(protocol::status::GUEST_RESTORE_RECORD)
    );
    peer.stream.set_nonblocking(true).unwrap();
    assert_eq!(
        peer.stream.read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

fn short_deadline(session: &mut Session<'_>) -> Instant {
    let now = Instant::now();
    let repaired = crate::guest::snapshot::repair::tests::repaired_fixture();
    let state = session.capture.as_mut().unwrap().restored.as_mut().unwrap();
    state.activation = crate::guest::restore::GuestRestore::with_resources(
        crate::guest_restore::ExpectedResources {
            cpus: repaired.cpus(),
            memory: repaired.memory(),
        },
        Some(now - Duration::from_millis(100)),
        Duration::from_millis(300),
        now,
    );
    state.reset(now).unwrap();
    let deadline = state.activation.deadline().unwrap();
    session.connection.get_ref().deadline.set(Some(deadline));
    deadline
}

#[test]
fn reconnect_and_partial_input_wait_only_until_the_original_activation_deadline() {
    let (mut session, mut peer, _) = restored();
    let deadline = short_deadline(&mut session);
    peer.reset(2, 4);
    assert!(matches!(session.next_event(), Ok(Event::Reset)));
    session.acknowledge().unwrap();
    assert_eq!(peer.read().0.record_type, OUTER_ACK);
    assert_eq!(session.connection.get_ref().deadline.get(), Some(deadline));
    let partial = encode_outer(OUTER_DATA, &INSTANCE, 2, 5, &[0; 24]).unwrap();
    peer.stream.write_all(&partial[..11]).unwrap();
    assert!(matches!(
        session.next_event(),
        Err(ReceiveFailure::Read(Errno::ETIMEDOUT))
    ));
    assert!(Instant::now() >= deadline);
}

#[test]
fn an_unread_output_stream_cannot_extend_the_activation_deadline() {
    let (mut session, _peer, _) = restored();
    let deadline = short_deadline(&mut session);
    // The largest payload that a `READY` carries after its protocol version.
    let payload = vec![0; v2::MAX_PAYLOAD - 1];
    let error = loop {
        if let Err(error) = session.send_app(APP_READY, 67, 0, &payload) {
            break error;
        }
    };
    assert_eq!(error, Errno::ETIMEDOUT);
    assert!(Instant::now() >= deadline);
}

fn queue(
    session: &mut Session<'_>,
    peer: &mut Peer,
    id: u64,
    payload: &[u8],
    active: bool,
) -> bool {
    peer.send_v2(id, v2::Frame::Capture(payload));
    let Event::Data(bytes) = session
        .next_event()
        .unwrap_or_else(|_| panic!("valid request DATA"))
    else {
        panic!("the capture request is DATA");
    };
    let Ok(Request::App(request)) = session.accept_request(&bytes) else {
        panic!("valid frame");
    };
    let accepted = session.queue_capture(request, active).unwrap();
    peer.credit();
    accepted
}

#[test]
fn capture_refuses_active_execution_without_kill_and_validates_the_existing_flag() {
    let invalid = protocol::INVALID_REQUEST.as_bytes();
    for (payload, kind, status, category) in [
        (b"\0".as_slice(), APP_BUSY, 0, b"".as_slice()),
        (b"\x02".as_slice(), APP_ERROR, Errno::EINVAL as i32, invalid),
        (b"".as_slice(), APP_ERROR, Errno::EINVAL as i32, invalid),
    ] {
        let (mut session, mut peer) = connected(true);
        assert!(!queue(&mut session, &mut peer, 31, payload, true));
        assert!(!session.capture_pending());
        peer.app(kind, 31, status, category);
    }
    let (mut session, mut peer) = connected(true);
    assert!(queue(&mut session, &mut peer, 32, &[1], true));
    assert!(session.capture_pending());
}

#[test]
fn rooted_capture_refuses_both_kill_flags_without_acknowledgement_or_pending_cleanup() {
    for active in [false, true] {
        for kill in [0, 1] {
            let (mut session, mut peer) = connected(false);
            assert!(!queue(&mut session, &mut peer, 37, &[kill], active));
            assert!(!session.capture_pending());
            assert!(session.finish_capture().unwrap().is_none());
            peer.app(
                APP_ERROR,
                37,
                Errno::ENOTSUP as i32,
                b"unsupported-operation",
            );
            peer.stream.set_nonblocking(true).unwrap();
            assert_eq!(
                peer.stream.read(&mut [0]).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }
    }
}

#[test]
fn timed_out_cleanup_refuses_before_ready_and_keeps_capture_admission_open() {
    let (mut session, mut peer) = connected(true);
    assert!(queue(&mut session, &mut peer, 39, &[1], false));
    assert!(
        session
            .finish_capture_with(|capture, session| {
                capture.perform_with(
                    |_, _| Err(Errno::ETIMEDOUT),
                    |id| session.send_app(APP_READY, id, 0, &[]),
                    |_, _, _| panic!("timed-out cleanup does not capture"),
                )
            })
            .unwrap()
            .is_none()
    );
    peer.app(APP_ERROR, 39, Errno::ETIMEDOUT as i32, b"capture-failed");
    assert!(!session.capture_pending());
    assert_eq!(session.capture_status(), None);
    assert!(queue(&mut session, &mut peer, 40, &[0], false));
}

#[test]
fn capture_acknowledges_then_sends_nothing_until_return_and_reports_on_the_same_session() {
    let (mut session, mut peer) = connected(true);
    assert!(queue(&mut session, &mut peer, 33, &[0], false));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = session.finish_capture_with(|capture, session| {
            capture.perform_with(
                |_, _| Ok(()),
                |id| session.send_app(APP_READY, id, 0, &[]),
                |_, _, _| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(Captured::Continued)
                },
            )
        });
        (session, result)
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    peer.app(APP_READY, 33, 0, &[]);
    peer.stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut byte = [0];
    let error = peer.stream.read(&mut byte).unwrap_err();
    assert!(matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ));
    peer.stream.set_read_timeout(None).unwrap();
    release_tx.send(()).unwrap();
    peer.app(APP_ERROR, 33, Errno::EAGAIN as i32, b"capture-continued");
    let (session, result) = worker.join().unwrap();
    assert!(result.unwrap().is_none());
    assert!(session.capture_status().is_none());
}

#[test]
fn resumed_capture_reports_on_the_same_epoch_without_attaching_again() {
    let (mut session, mut peer) = connected(true);
    assert!(queue(&mut session, &mut peer, 34, &[0], false));
    assert!(
        session
            .finish_capture_with(|capture, session| {
                capture.perform_with(
                    |_, _| Ok(()),
                    |id| session.send_app(APP_READY, id, 0, &[]),
                    |_, _, _| {
                        Err(CaptureFailure {
                            stage: CaptureStage::Mounts,
                            cause: CaptureCause::Io(io::Error::other("capture preparation failed")),
                            disposition: Disposition::Resumed,
                        })
                    },
                )
            })
            .unwrap()
            .is_none()
    );
    peer.app(APP_READY, 34, 0, &[]);
    peer.app(APP_ERROR, 34, Errno::EIO as i32, b"capture-failed");
    assert!(session.capture_status().is_none());
}

#[test]
fn restored_capture_consumes_reset_and_acknowledges_the_existing_connection() {
    let (mut session, mut peer) = connected(true);
    assert!(queue(&mut session, &mut peer, 35, &[0], false));
    let worker = thread::spawn(move || {
        let result = session.finish_capture_with(|capture, session| {
            capture.perform_with(
                |_, _| Ok(()),
                |id| session.send_app(APP_READY, id, 0, &[]),
                |_, _, _| {
                    Ok(Captured::Restored(
                        crate::guest::snapshot::repair::tests::repaired_with_record(
                            &Capture::activation_payload(),
                        ),
                    ))
                },
            )
        });
        (session, result)
    });
    peer.app(APP_READY, 35, 0, &[]);
    peer.stream
        .write_all(&encode_outer(OUTER_RESET, &[8; 16], 2, 4, &[]).unwrap())
        .unwrap();
    peer.epoch = 2;
    peer.guest_sequence = 0;
    let (header, payload) = peer.read();
    assert_eq!((header.record_type, header.epoch), (OUTER_ACK, 2));
    assert_eq!(payload, OUTER_RECEIVE_WINDOW.to_le_bytes());
    let (mut session, result) = worker.join().unwrap();
    assert!(result.unwrap().is_none());
    assert!(session.rootless());
    assert!(session.capture_status().is_none());
    assert_eq!(session.connection.session().identity().unwrap().epoch, 2);
    let request = protocol::AppFrame {
        kind: APP_CAPTURE,
        request_id: 38,
        status: 0,
        payload: &[0],
    };
    assert!(!session.queue_capture(request, false).unwrap());
    assert!(!session.capture_pending());
    peer.app(APP_BUSY, 38, 0, b"");
    peer.stream.set_nonblocking(true).unwrap();
    assert_eq!(
        peer.stream.read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn must_end_returns_the_exact_status_without_a_report_or_more_control_work() {
    for status in [0, 41, 194] {
        let (mut session, mut peer) = connected(true);
        assert!(queue(&mut session, &mut peer, 36, &[0], false));
        let returned = session
            .finish_capture_with(|capture, session| {
                capture.perform_with(
                    |_, _| Ok(()),
                    |id| session.send_app(APP_READY, id, 0, &[]),
                    |_, _, _| {
                        Err(CaptureFailure {
                            stage: CaptureStage::Capture,
                            cause: CaptureCause::Io(io::Error::other("capture must end")),
                            disposition: Disposition::MustEnd { status },
                        })
                    },
                )
            })
            .unwrap();
        assert!(matches!(
            returned,
            Some(super::super::dispatch::Outcome::Stopped)
        ));
        assert_eq!(session.capture_status(), Some(status));
        peer.app(APP_READY, 36, 0, &[]);
        peer.stream.set_nonblocking(true).unwrap();
        assert_eq!(
            peer.stream.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}
