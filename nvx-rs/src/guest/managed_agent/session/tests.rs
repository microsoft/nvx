use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::*;
use crate::guest::snapshot::{CaptureCause, CaptureFailure, CaptureStage};
use crate::protocol::{
    APP_CAPTURE, APP_READY, OUTER_ACK, OUTER_CREDIT, OUTER_DATA, OUTER_GUEST_ATTACH,
    OUTER_HEADER_LEN, OUTER_RESET, OuterHeader, decode_app, decode_outer_header, encode_outer,
};

const INSTANCE: [u8; 16] = [7; 16];

impl Session<'_> {
    pub(crate) fn activation_state_for_test(&self) -> Option<ActivationState> {
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

    fn app(&mut self, kind: u8, id: u64, status: i32, payload: &[u8]) {
        let (header, bytes) = self.read();
        assert_eq!(header.record_type, OUTER_DATA);
        assert_eq!(header.epoch, self.epoch);
        let frame = decode_app(&bytes).unwrap();
        assert_eq!(
            (frame.kind, frame.request_id, frame.status, frame.payload),
            (kind, id, status, payload)
        );
    }

    fn send(&mut self, kind: u8, id: u64, payload: &[u8]) {
        let bytes = protocol::encode_app(kind, id, 0, payload);
        self.stream
            .write_all(
                &encode_outer(
                    OUTER_DATA,
                    &INSTANCE,
                    self.epoch,
                    self.host_sequence,
                    &bytes,
                )
                .unwrap(),
            )
            .unwrap();
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

fn activate_bytes(session: &mut Session<'_>, peer: &mut Peer, id: u64, payload: &[u8]) -> Vec<u8> {
    peer.send(protocol::APP_ACTIVATE, id, payload);
    let Event::Data(bytes) = session
        .next_event()
        .unwrap_or_else(|_| panic!("activate DATA"))
    else {
        panic!("the activate is DATA");
    };
    bytes
}

#[test]
fn a_replacement_session_joins_one_activation_and_caches_ready_without_reapplying() {
    let (mut session, mut peer, payload) = restored();
    let deadline = session.connection.get_ref().deadline.get();
    let (record, _) = protocol::decode_activate_payload(&payload).unwrap();
    let ready = crate::activation::ReadyActivation::of(&record).encode();
    let bytes = activate_bytes(&mut session, &mut peer, 61, &payload);
    let request = session
        .accept_request(&bytes)
        .unwrap_or_else(|_| panic!("valid activate"));
    peer.credit();
    let retained = session
        .activate_with(request, |restored, _, accepted, workload| {
            assert_eq!(restored.activation.state(), ActivationState::Activating);
            assert_eq!(restored.activation.deadline(), deadline);
            assert_eq!(accepted, &record);
            assert_eq!(
                (workload.uid, workload.gid, workload.create_account),
                (1000, 1001, true)
            );
            peer.reset(2, 4);
            peer.send(protocol::APP_ACTIVATE, 62, &payload);
            Ok("one applied space")
        })
        .unwrap();
    assert_eq!(retained, Some("one applied space"));
    assert_eq!(peer.read().0.record_type, OUTER_ACK);
    peer.credit();
    peer.app(APP_READY, 62, 0, &ready);
    assert!(!session.rootless());
    assert_eq!(session.connection.get_ref().deadline.get(), None);
    let bytes = activate_bytes(&mut session, &mut peer, 63, &payload);
    let request = session
        .accept_request(&bytes)
        .unwrap_or_else(|_| panic!("valid retry"));
    peer.credit();
    assert!(
        session
            .activate_with::<()>(request, |_, _, _, _| panic!(
                "must not apply or switch twice"
            ))
            .unwrap()
            .is_none()
    );
    peer.app(APP_READY, 63, 0, &ready);
    session.start_activation().unwrap();
    session.start_activation().unwrap();
    assert_eq!(
        session.activation_state_for_test(),
        Some(ActivationState::Started)
    );
}

#[test]
fn postactivation_capture_stays_refused_without_pending_cleanup() {
    let (mut session, mut peer, payload) = restored();
    let state = session.capture.as_mut().unwrap().restored.as_mut().unwrap();
    assert!(matches!(
        state.accept(&payload, Instant::now()).unwrap().0,
        ActivationAction::Apply(_)
    ));
    state.activation.complete(Instant::now(), Ok(())).unwrap();
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
fn changed_record_or_workload_bytes_during_application_fail_without_ready() {
    for changed_identity in [false, true] {
        let (mut session, mut peer, payload) = restored();
        let mut changed = payload.clone();
        if changed_identity {
            changed[0] ^= 1;
        } else {
            let (record, _) = protocol::decode_activate_payload(&payload).unwrap();
            let mut configuration = record.configuration().clone();
            configuration.view.hostname = b"changed".to_vec();
            let record =
                crate::activation::ActivationRecord::new(record.restore().copied(), configuration)
                    .unwrap();
            changed.truncate(9);
            changed.extend(record.encode());
        }
        let bytes = activate_bytes(&mut session, &mut peer, 64, &payload);
        let request = session
            .accept_request(&bytes)
            .unwrap_or_else(|_| panic!("valid activate"));
        peer.credit();
        let result = session.activate_with(request, |_, _, _, _| {
            peer.send(protocol::APP_ACTIVATE, 65, &changed);
            Ok(())
        });
        assert_eq!(result.err(), Some(Errno::EPROTO));
        peer.credit();
        peer.stream.set_nonblocking(true).unwrap();
        assert_eq!(
            peer.stream.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(session.rootless());
    }
}

#[test]
fn failed_application_never_reports_ready_or_admits_root_dependent_work() {
    let (mut session, mut peer, payload) = restored();
    let bytes = activate_bytes(&mut session, &mut peer, 66, &payload);
    let request = session
        .accept_request(&bytes)
        .unwrap_or_else(|_| panic!("valid activate"));
    peer.credit();
    assert_eq!(
        session
            .activate_with::<()>(request, |_, _, _, _| Err(crate::Error::io(
                "runtime mounts failed"
            )))
            .err(),
        Some(Errno::EPROTO)
    );
    assert!(session.rootless());
    assert_eq!(
        session.activation_state_for_test(),
        Some(ActivationState::Failed)
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
    let (mut session, mut peer, payload) = restored();
    assert_eq!(session.activation_status(), None);
    let bytes = activate_bytes(&mut session, &mut peer, 68, &payload);
    let request = session
        .accept_request(&bytes)
        .unwrap_or_else(|_| panic!("valid activate"));
    peer.credit();
    let failure = crate::Error::io("the network settings cannot be applied").with_source(
        crate::guest::activation::ApplyError::Network {
            error: crate::guest::activation::NetworkError::Gateway,
            teardown: None,
        },
    );
    assert_eq!(
        session
            .activate_with::<()>(request, |_, _, _, _| Err(failure))
            .err(),
        Some(Errno::EPROTO)
    );
    assert_eq!(
        session.activation_status(),
        Some(protocol::status::ACTIVATION_NETWORK)
    );
    // A request while the record applies fails the activation as the protocol's failure.
    let (mut session, mut peer, payload) = restored();
    let bytes = activate_bytes(&mut session, &mut peer, 69, &payload);
    let request = session
        .accept_request(&bytes)
        .unwrap_or_else(|_| panic!("valid activate"));
    peer.credit();
    let result = session.activate_with(request, |_, _, _, _| {
        peer.send(protocol::APP_PING, 70, &[]);
        Ok(())
    });
    assert_eq!(result.err(), Some(Errno::EPROTO));
    assert_eq!(
        session.activation_state_for_test(),
        Some(ActivationState::Failed)
    );
    assert_eq!(
        session.activation_status(),
        Some(protocol::status::ACTIVATION_PROTOCOL)
    );
}

fn short_deadline(session: &mut Session<'_>) -> Instant {
    let now = Instant::now();
    let repaired = crate::guest::snapshot::repair::tests::repaired_fixture();
    let state = session.capture.as_mut().unwrap().restored.as_mut().unwrap();
    state.activation = crate::guest::activation::Activation::with_resources(
        crate::activation::ExpectedResources {
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
    let payload = vec![0; OUTER_MAX_PAYLOAD - protocol::APP_HEADER_LEN];
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
    peer.send(APP_CAPTURE, id, payload);
    let Event::Data(bytes) = session
        .next_event()
        .unwrap_or_else(|_| panic!("valid request DATA"))
    else {
        panic!("the capture request is DATA");
    };
    let request = session
        .accept_request(&bytes)
        .unwrap_or_else(|_| panic!("valid frame"));
    let accepted = session.queue_capture(request, active).unwrap();
    peer.credit();
    accepted
}

#[test]
fn capture_refuses_active_execution_without_kill_and_validates_the_existing_flag() {
    for (payload, status, category) in [
        (b"\0".as_slice(), Errno::EBUSY, protocol::BUSY),
        (b"\x02".as_slice(), Errno::EINVAL, protocol::INVALID_REQUEST),
        (b"".as_slice(), Errno::EINVAL, protocol::INVALID_REQUEST),
    ] {
        let (mut session, mut peer) = connected(true);
        assert!(!queue(&mut session, &mut peer, 31, payload, true));
        assert!(!session.capture_pending());
        peer.app(APP_ERROR, 31, status as i32, category.as_bytes());
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
                        crate::guest::snapshot::repair::tests::repaired_fixture(),
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
    assert_eq!(
        protocol::decode_ack_payload(&payload).unwrap(),
        OUTER_RECEIVE_WINDOW
    );
    let (mut session, result) = worker.join().unwrap();
    assert!(result.unwrap().is_none());
    assert!(session.rootless());
    assert!(session.capture_status().is_none());
    assert_eq!(session.connection.session().identity().unwrap().epoch, 2);
    let bytes = protocol::encode_app(APP_CAPTURE, 38, 0, &[0]);
    assert!(
        !session
            .queue_capture(decode_app(&bytes).unwrap(), false)
            .unwrap()
    );
    assert!(!session.capture_pending());
    peer.app(
        APP_ERROR,
        38,
        Errno::EBUSY as i32,
        protocol::BUSY.as_bytes(),
    );
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
