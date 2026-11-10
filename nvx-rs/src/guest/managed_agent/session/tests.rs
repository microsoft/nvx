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

struct Peer {
    stream: UnixStream,
    epoch: u64,
    host_sequence: u64,
    guest_sequence: u64,
}

impl Peer {
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
