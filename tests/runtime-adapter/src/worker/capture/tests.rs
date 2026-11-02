use std::cell::Cell;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;

use nvx::guest::nvxs::GuestConnectionError;
use nvx::protocol::nvxs::{GuestState, Identity, RecordType, SessionError, encode_record};
use nvx::protocol::status;

use super::*;
use crate::template::COMMAND_LINE_STATUS;
use crate::worker::SESSION_STATUS;

#[test]
fn closing_the_fence_acquires_it_until_it_opens() {
    let mut admission = Admission::default();
    assert!(!admission.is_closed());
    admission.close().unwrap();
    assert!(admission.is_closed());
    // A second close fails, and leaves the fence closed.
    let error = admission.close().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::ResourceBusy);
    assert_eq!(
        error.to_string(),
        "the worker's admission of executions is fenced already"
    );
    assert!(admission.is_closed());
    admission.open();
    assert!(!admission.is_closed());
    admission.close().unwrap();
    assert!(admission.is_closed());
}

#[test]
fn each_stage_of_a_capture_ends_the_worker_with_a_status_of_its_own() {
    let stages = [
        (CaptureStage::Fence, 21),
        (CaptureStage::Mounts, 22),
        (CaptureStage::BootChecks, 23),
        (CaptureStage::CaptureChecks, 24),
        (CaptureStage::Suppression, 25),
        (CaptureStage::GenerationId, 26),
        (CaptureStage::Capture, 27),
        (CaptureStage::Release, 28),
    ];
    for (stage, status) in stages {
        assert_eq!(stage_status(stage), status, "{stage}");
        assert_eq!(failure_status(stage, None), status, "{stage}");
    }
    assert_eq!(Templated::Continued.status(), CONTINUED_STATUS);
    assert_eq!(
        Templated::Continued.to_string(),
        "the template's capture ended without a snapshot, and the template continued"
    );
    // No two outcomes of the worker share a status, nor one of a clone's repair or
    // activation.
    let mut statuses = stages.map(|(_, status)| status).to_vec();
    statuses.extend([COMMAND_LINE_STATUS, CONTINUED_STATUS, SESSION_STATUS, 0]);
    statuses.extend(RootStage::ALL.map(RootStage::exit_status));
    statuses.extend(status::TIME_STATUSES);
    statuses.extend(status::REPAIR_STATUSES);
    statuses.extend(status::ACTIVATION_STATUSES);
    let count = statuses.len();
    statuses.sort_unstable();
    statuses.dedup();
    assert_eq!(statuses.len(), count);
}

#[test]
fn a_request_that_the_time_abi_could_not_repair_ends_with_the_violation_s_status() {
    // In a clone, whose restore the time ABI's part of the repair refused.
    assert_eq!(
        failure_status(CaptureStage::Capture, Some(status::TIME_REPAIR)),
        195
    );
    assert_eq!(
        failure_status(CaptureStage::Capture, Some(status::TIME_CONFORMANCE)),
        193
    );
    // A violation of the preparation, which only a report-only time ABI returns, keeps the
    // stage's status.
    assert_eq!(
        failure_status(CaptureStage::BootChecks, Some(status::TIME_CONFORMANCE)),
        23
    );
    assert_eq!(
        failure_status(CaptureStage::CaptureChecks, Some(status::TIME_CONFORMANCE)),
        24
    );
}

#[test]
fn a_template_s_devices_are_those_of_a_root_and_the_host_filesystem() {
    let devices = devices();
    assert_eq!(devices.distro.mmio, DISTRO_MMIO_BASE);
    assert_eq!(devices.scratch.mmio, SCRATCH_MMIO_BASE);
    assert_eq!(
        (devices.distro.sectors, devices.scratch.sectors),
        (None, None)
    );
    assert_eq!(devices.filesystem_tag, "microvm");
}

#[test]
fn a_clone_assembles_its_root_with_the_template_s_configuration() {
    // The configuration that the host program sends a VM that boots cold unless its caller
    // names another.
    let config = template_config();
    assert_eq!(config, RootConfig::default());
    let template = root_template(&config, None);
    assert_eq!(template.runtime, Path::new(&config.runtime));
    assert_eq!(template.timeout, config.device_timeout);
    assert_eq!(template.gateway, None);
    // The clone's root comes from the devices whose mounts the template's capture checks, and
    // its export, if its record has one, through the host filesystem device that the capture
    // found unmounted.
    let devices = devices();
    assert_eq!(template.distro, devices.distro);
    assert_eq!(template.scratch, devices.scratch);
    assert_eq!(template.filesystem_tag, devices.filesystem_tag);
    // A configuration that sizes the devices sizes the clone's.
    let sized = RootConfig {
        distro_sectors: Some(8),
        scratch_sectors: Some(16),
        ..config
    };
    // A template with a NIC hands its clones its gateway.
    let gateway = std::net::Ipv4Addr::new(10, 0, 0, 1);
    let template = root_template(&sized, Some(gateway));
    assert_eq!(
        (template.distro.sectors, template.scratch.sectors),
        (Some(8), Some(16))
    );
    assert_eq!(template.gateway, Some(gateway));
    assert_eq!(
        (template.distro.mmio, template.scratch.mmio),
        (DISTRO_MMIO_BASE, SCRATCH_MMIO_BASE)
    );
}

/// Identity of the epoch that the broker of the tests' templates starts, with the reset with
/// which it answers the attachment.
const TEMPLATE: Identity = Identity {
    instance_id: [0x7e; 16],
    epoch: 1,
};

/// Returns the record of `record_type`, with `payload`, that starts or acknowledges the
/// template's epoch, as the first of its side.
fn first(record_type: RecordType, payload: &[u8]) -> Vec<u8> {
    let mut record = Vec::new();
    let (instance, epoch) = (TEMPLATE.instance_id, TEMPLATE.epoch);
    encode_record(record_type, &instance, epoch, 0, payload, &mut record).unwrap();
    record
}

#[test]
fn the_worker_acknowledges_the_broker_s_reset_before_the_capture_and_reads_nothing_more() {
    let (guest, broker) = UnixStream::pair().unwrap();
    // The broker answers the attachment with a reset, and a record waits on the stream behind
    // it, as one of the restore's broker would.
    (&broker).write_all(&first(RecordType::Reset, &[])).unwrap();
    (&broker).write_all(b"waiting").unwrap();
    let mut attachment = Vec::new();
    GuestSession::new().attach(&mut attachment).unwrap();
    let acknowledgement = first(RecordType::Ack, &RECEIVE_WINDOW.to_le_bytes());
    let sent = Cell::new(false);
    let (captured, session) = attached(
        &guest,
        |_| {
            sent.set(true);
            Ok(())
        },
        || {
            // The capture runs once the acknowledgement was sent, after the attachment, which
            // the broker holds.
            assert!(sent.get());
            let mut received = vec![0; attachment.len() + acknowledgement.len()];
            (&broker).read_exact(&mut received).unwrap();
            assert_eq!(received, [attachment, acknowledgement].concat());
            "captured"
        },
    )
    .unwrap();
    assert_eq!(captured, "captured");
    // The session that a clone resumes is active in the broker's first epoch, so that the
    // restore's reset, of another instance, is the only one that the clone reads.
    assert_eq!(session.state(), GuestState::Active);
    assert_eq!(session.identity(), Some(TEMPLATE));
    assert_eq!(session.window(), Some(RECEIVE_WINDOW));
    // The worker read nothing past the reset.
    let mut waiting = [0; 7];
    (&guest).read_exact(&mut waiting).unwrap();
    assert_eq!(&waiting, b"waiting");
}

#[test]
fn no_capture_runs_without_the_session_s_handshake() {
    // The tty does not send the acknowledgement.
    let (guest, broker) = UnixStream::pair().unwrap();
    (&broker).write_all(&first(RecordType::Reset, &[])).unwrap();
    let unsent = io::Error::new(io::ErrorKind::TimedOut, "the tty sent nothing");
    let error = attached::<_, ()>(&guest, |_| Err(unsent), || panic!("captured")).unwrap_err();
    assert!(
        matches!(&error, ServeError::Terminal(error) if error.kind() == io::ErrorKind::TimedOut),
        "{error}"
    );

    // The attachment cannot be written.
    let (guest, broker) = UnixStream::pair().unwrap();
    drop(broker);
    let error = attached::<_, ()>(&guest, |_| panic!("sent"), || panic!("captured")).unwrap_err();
    assert!(
        matches!(error, ServeError::Session(GuestConnectionError::Io(_))),
        "{error}"
    );

    // The broker closes the stream, or sends anything but a reset first.
    let (guest, broker) = UnixStream::pair().unwrap();
    broker.shutdown(Shutdown::Write).unwrap();
    let error = attached::<_, ()>(&guest, |_| panic!("sent"), || panic!("captured")).unwrap_err();
    assert!(
        matches!(error, ServeError::Session(GuestConnectionError::Closed)),
        "{error}"
    );
    let (guest, broker) = UnixStream::pair().unwrap();
    (&broker)
        .write_all(&first(RecordType::Data, b"early"))
        .unwrap();
    let error = attached::<_, ()>(&guest, |_| panic!("sent"), || panic!("captured")).unwrap_err();
    assert!(
        matches!(
            error,
            ServeError::Session(GuestConnectionError::Protocol(SessionError::Unexpected(
                RecordType::Data
            )))
        ),
        "{error}"
    );
}
