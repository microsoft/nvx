//! The worker of a template's VM, which captures the VM before it assembles any root, through
//! `nvx::guest::clone`, on x86_64, whose time ABI a capture needs, and then serves each clone that
//! a restore of the snapshot starts. The [`template`](crate::template) module tells such a VM by
//! its kernel command line, and holds the statuses with which its worker ends.
//!
//! [`capture_on_terminal`] opens the VM's control tty, in raw mode, as the worker of a VM that
//! boots cold does, and then, in order:
//!
//! 1. bootstraps the guest for the root of every clone, with the template's own root
//!    configuration, [`template_config`], since the host sends a template's VM none: its cgroups
//!    and their resource values, through [`RootOperations::bootstrap`], as the worker of a VM that
//!    boots cold does before it assembles its root. The snapshot holds them, so that a clone
//!    assembles its root without them;
//! 2. attaches the control session with `nvx::guest::nvxs::GuestConnection::attach`, takes the
//!    reset with which the broker answers the attachment at once, acknowledges it with the guest
//!    agent's receive window, and waits until the tty has sent the acknowledgement, so that the
//!    snapshot holds the guest's session active, in the broker's first epoch. A clone then reads
//!    no reset but the one that the restore queues, of another instance: the restore's broker
//!    would finish sending a reset that the template's broker began to send, and the clone would
//!    read one that the template did not. The worker reads nothing else from the session before
//!    the capture, and serves no application session: the host opens none with a template's VM;
//! 3. prepares the guest for the capture with `nvx::guest::clone::CapturePreparation`, behind the
//!    worker's [`Admission`], its fence of workload admission, and from whose [`devices`] nothing
//!    may be mounted: the preparation closes the fence, checks the mount table, waits for the time
//!    ABI's boot checks, runs its capture checks, and suppresses the stall detectors;
//! 4. requests the capture with `CaptureBarrier::capture`, with the platform tier's request,
//!    which keeps no scratch.
//!
//! A committed capture ends the template's VM. The worker returns only once the bootstrap failed,
//! or the capture ended without a snapshot, or failed, or in a clone, which a restore of the
//! snapshot started, as a [`Templated`] tells, and with which the worker ends. In a clone, the
//! worker repairs the restore next, with `nvx::guest::clone::repair`, behind its fence, which
//! stays closed: the repair activates the restore's memory target, reseeds the kernel's random
//! number generator, and finishes the restore, with the acknowledgement that the host awaits,
//! which hands the deferred time work to the time daemon. NVX's `/init` started the daemon in the
//! template, with the time ABI's boot step, before the worker, so the snapshot holds it, and PID 1
//! collects its status. A repair that failed ends the clone with the
//! [status](nvx::guest::clone::RepairError::status) of its failure, which `nvx::protocol::status`
//! documents, and a capture request that failed in a clone, since the time ABI could not repair
//! its restore, with the status of the time ABI's violation, as [`capture_status`] tells.
//!
//! Once the worker repaired a clone, it serves the clone's session on the same tty, with
//! [`work_clone`]: through the clone's activation, `nvx::guest::clone::Activation` of the repaired
//! clone and of the fence, whose deadline ends [`ACTIVATION_TIMEOUT`] after the repair's
//! acknowledgement, it answers the reset that the restore queued for the session that the snapshot
//! holds, takes the host's `Activate`, and applies its record with [`root_template`] of the
//! template's configuration and its NIC's gateway, then serves the host's order as after a cold
//! boot, as the
//! [worker's documentation](super#clones) describes. The clone ends as that session does. Once its
//! activation failed, or the broker closed the stream before the clone was ready, the worker waits
//! for the tty to send what it wrote for at most [`UNACTIVATED_DRAIN`], so that a tty that sends
//! nothing does not keep the clone from ending.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Instant;

use nvx::guest::clone::{
    self, Activation, AdmissionFence, CaptureBarrier, CaptureCause, CaptureError,
    CapturePreparation, CaptureStage, Captured, Devices, RepairError, Repaired, RootTemplate,
};
use nvx::guest::nvxs::GuestConnection;
use nvx::guest::root::Device;
use nvx::guest::terminal::Terminal;
use nvx::guest::time::TimeError;
use nvx::protocol::nvxs::GuestSession;
use nvx::protocol::root::{DISTRO_MMIO_BASE, SCRATCH_MMIO_BASE};

use super::activation::ACTIVATION_TIMEOUT;
use super::{RootOperations, UNACTIVATED_DRAIN, Worked, open_tty, work_clone};
use crate::guest::{self, RECEIVE_WINDOW, ServeError};
use crate::root::{RootConfig, RootFailure, RootStage};
use crate::template::{CAPTURE_STATUS_BASE, CONTINUED_STATUS};

/// The virtio-fs tag of the VM's host filesystem device, OpenVMM's one filesystem slot: dormant in
/// a template's VM, and the device of the host export that the worker of a VM that boots cold
/// mounts, and that the application of a clone's record mounts if the record has an export.
pub const FILESYSTEM_TAG: &str = "microvm";

/// Returns the root configuration that a template's VM holds as its own, since the host sends it
/// none: [`RootConfig::default`], the configuration that the host program sends a VM that boots
/// cold unless its caller names another. Its cgroups and their resource values, with which the
/// template bootstraps the guest before its capture, and the inputs of the root's assembly that
/// [`root_template`] takes from it, are the template's, which every clone of it shares.
pub fn template_config() -> RootConfig {
    RootConfig::default()
}

/// Returns the inputs of a clone's activation that its template captured, from `config`, the
/// template's own root configuration, and `gateway`, the IPv4 gateway of the template's NIC, which
/// its kernel command line names, if it has one: its runtime directory; the distro and the scratch
/// block devices, at the MMIO windows from which the worker of a VM that boots cold assembles its
/// root, with the sizes that the configuration names, if any; how long the assembly waits for
/// each; the host filesystem device, by [`FILESYSTEM_TAG`], through which the host attaches the
/// export of a clone's record; and the gateway, to which the announcer of the listeners of a
/// clone whose record publishes them announces them. The record of the clone's `Activate` holds
/// the rest.
pub fn root_template(config: &RootConfig, gateway: Option<Ipv4Addr>) -> RootTemplate {
    RootTemplate {
        runtime: PathBuf::from(&config.runtime),
        distro: Device {
            mmio: DISTRO_MMIO_BASE,
            sectors: config.distro_sectors,
        },
        scratch: Device {
            mmio: SCRATCH_MMIO_BASE,
            sectors: config.scratch_sectors,
        },
        timeout: config.device_timeout,
        filesystem_tag: FILESYSTEM_TAG.to_owned(),
        gateway,
    }
}

/// The worker's fence of workload admission in a template's VM, which it hands the capture's
/// preparation.
///
/// The worker of a template's VM admits no execution before the capture, nor in a clone, which it
/// repairs behind the fence, before the clone's activation opens it, once the clone is ready.
/// Closing the fence acquires it, as `nvx::guest::clone` requires:
/// [`close`](AdmissionFence::close) fails while the fence is closed, whoever closed it, and leaves
/// it closed.
#[derive(Debug, Default)]
pub struct Admission {
    /// Whether the fence is closed, so that no execution starts.
    closed: bool,
}

impl Admission {
    /// Returns whether the fence is closed, so that no execution starts.
    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

impl AdmissionFence for Admission {
    fn close(&mut self) -> io::Result<()> {
        if self.closed {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "the worker's admission of executions is fenced already",
            ));
        }
        self.closed = true;
        Ok(())
    }

    fn open(&mut self) {
        self.closed = false;
    }
}

/// Returns the devices of a template's VM from which its capture must find nothing mounted: the
/// distro and the scratch block devices, at the MMIO windows from which the worker of a VM that
/// boots cold assembles its root, and the host filesystem device, by [`FILESYSTEM_TAG`].
pub fn devices() -> Devices {
    Devices {
        distro: Device {
            mmio: DISTRO_MMIO_BASE,
            sectors: None,
        },
        scratch: Device {
            mmio: SCRATCH_MMIO_BASE,
            sectors: None,
        },
        filesystem_tag: FILESYSTEM_TAG.to_owned(),
    }
}

/// Returns the status with which the worker of a template's VM, and with it the VM, ends once its
/// capture failed at `stage`: [`CAPTURE_STATUS_BASE`] plus the stage's number, in the order in
/// which `CaptureStage` declares the stages, from 21 for the fence to 28 for the release, or 29 for
/// a stage that the worker does not know.
pub fn stage_status(stage: CaptureStage) -> u8 {
    CAPTURE_STATUS_BASE
        + match stage {
            CaptureStage::Fence => 1,
            CaptureStage::Mounts => 2,
            CaptureStage::BootChecks => 3,
            CaptureStage::CaptureChecks => 4,
            CaptureStage::Suppression => 5,
            CaptureStage::GenerationId => 6,
            CaptureStage::Capture => 7,
            CaptureStage::Release => 8,
            _ => 9,
        }
}

/// Returns the status with which the worker of a template's VM ends once its capture failed with
/// `error`: the [status](nvx::guest::time::Violation::status) of the time ABI's violation, 195 or
/// 193, when the request failed with one, as in a clone whose restore the time ABI could not
/// repair, and the [status](stage_status) of the stage that failed otherwise.
pub fn capture_status(error: &CaptureError) -> u8 {
    let violation = match error.cause() {
        CaptureCause::Time(TimeError::Violation(violation)) => Some(violation.status()),
        _ => None,
    };
    failure_status(error.stage(), violation)
}

/// Returns the status of a capture that failed at `stage`, with a violation of the time ABI of
/// the status `violation`, if any, as [`capture_status`] tells.
fn failure_status(stage: CaptureStage, violation: Option<u8>) -> u8 {
    match (stage, violation) {
        (CaptureStage::Capture, Some(status)) => status,
        (stage, _) => stage_status(stage),
    }
}

/// How the worker of a template's VM ended, once it opened its tty.
#[derive(Debug)]
pub enum Templated {
    /// The bootstrap of the guest for the template's root, with the template's own root
    /// configuration, failed, before the capture.
    Unbootstrapped(RootFailure),
    /// The capture ended without a snapshot, as without a destination or once OpenVMM rejected
    /// it, and the template continued: the barrier opened the fence, and restored the stall
    /// detectors' settings.
    Continued,
    /// A restore of the template's snapshot started this VM, a clone, which the worker repaired,
    /// then served, through its activation, and once the clone was ready, in its order, until the
    /// session ended as this tells: once the host stopped the worker, a teardown failed, the
    /// broker closed the stream, before the clone was ready too, or the activation failed.
    Cloned(Worked),
    /// A restore of the template's snapshot started this VM, a clone, whose repair failed. The
    /// fence stays closed.
    Unrepaired(RepairError),
    /// A stage of the capture failed. The preparation released what it took, and a failed
    /// request left the fence closed.
    Failed(CaptureError),
}

impl Templated {
    /// Returns the status with which the worker, and with it the VM, ends: the
    /// [status](RootStage::exit_status) of the bootstrap's stage once it failed,
    /// [`CONTINUED_STATUS`], the [status](Worked::status) with which a clone's session ended, the
    /// [status](nvx::guest::clone::RepairError::status) of a repair that failed, or the
    /// [status](capture_status) of a capture that failed.
    pub fn status(&self) -> u8 {
        match self {
            Self::Unbootstrapped(failure) => failure.stage().exit_status(),
            Self::Continued => CONTINUED_STATUS,
            Self::Cloned(worked) => worked.status(),
            Self::Unrepaired(error) => error.status(),
            Self::Failed(error) => capture_status(error),
        }
    }
}

impl fmt::Display for Templated {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let clone = "a restore of the template's snapshot started this clone, which the worker \
                     repaired";
        match self {
            Self::Unbootstrapped(failure) => {
                write!(formatter, "the template could not capture: {failure}")
            }
            Self::Continued => formatter.write_str(
                "the template's capture ended without a snapshot, and the template continued",
            ),
            Self::Cloned(Worked::Stopped) => {
                write!(
                    formatter,
                    "{clone}, activated, and served until the host stopped it"
                )
            }
            Self::Cloned(Worked::Failed(failure)) => write!(
                formatter,
                "{clone}, activated, and served until the host stopped it: {failure}"
            ),
            Self::Cloned(Worked::Closed) => {
                write!(
                    formatter,
                    "{clone}, activated, and whose session the broker closed"
                )
            }
            Self::Cloned(Worked::Abandoned) => write!(
                formatter,
                "{clone}, and whose session the broker closed before the clone was activated"
            ),
            Self::Cloned(Worked::Unactivated { problem, .. }) => {
                write!(formatter, "{clone}, but not activated: {problem}")
            }
            Self::Unrepaired(error) => write!(
                formatter,
                "the repair of this clone, which a restore of the template's snapshot started, \
                 {error} ({})",
                error.progress()
            ),
            Self::Failed(error) => error.fmt(formatter),
        }
    }
}

/// How a template's capture ended, once the worker repaired a clone that a restore of the snapshot
/// started.
enum Capture {
    /// The capture ended as this tells, without a clone to serve.
    Ended(Templated),
    /// A restore of the template's snapshot started this VM, a clone, which the worker repaired.
    Repaired {
        /// The repaired clone, with its resources.
        repaired: Repaired,
        /// The worker's fence of workload admission, still closed: opening it is the activation's.
        fence: Admission,
    },
}

/// Captures the template's VM on its control tty at `tty`, with the root operations `operations`,
/// whose NIC's IPv4 gateway is `gateway`, if it has a NIC, as its kernel command line names it,
/// as the [module documentation](self) describes, and returns how the worker ended: once the
/// bootstrap failed, or the capture ended without a snapshot, or failed, or in a clone, which a
/// restore of the snapshot started, once its repair failed, or the session that the worker served
/// once it repaired the clone ended.
///
/// It opens the tty through [`guest::on_terminal_within`], as the worker of a VM that boots cold
/// does, and fails as that does if the tty cannot be opened or put in raw mode. It fails with
/// [`ServeError::Session`] if the attachment or the acknowledgement of the broker's reset cannot
/// be written, or if the broker closes the stream or sends anything but a reset first, and with
/// [`ServeError::Terminal`] if the tty does not send the acknowledgement, before the preparation.
/// A clone's session fails as [`work_clone`] does, on the tty, which the worker opens again for
/// itself, as [`work_on_terminal`](super::work_on_terminal) does.
pub fn capture_on_terminal(
    tty: &Path,
    operations: &mut dyn RootOperations,
    gateway: Option<Ipv4Addr>,
) -> Result<Templated, ServeError> {
    guest::on_terminal_within(tty, |terminal| {
        let config = template_config();
        if let Err(problem) = operations.bootstrap(&config) {
            let failure = RootFailure::new(RootStage::Bootstrap, problem);
            return Ok((Templated::Unbootstrapped(failure), None));
        }
        let send = |terminal: &&mut Terminal| {
            terminal.drain().map_err(|error| {
                let problem = format!(
                    "cannot send the acknowledgement of the broker's reset on {}: {error}",
                    tty.display()
                );
                io::Error::new(error.kind(), problem)
            })
        };
        let (captured, session) = attached(terminal, send, || {
            let preparation = CapturePreparation::new(Admission::default(), devices());
            match preparation.prepare().and_then(CaptureBarrier::capture) {
                Ok(Captured::Continued) => Capture::Ended(Templated::Continued),
                Ok(Captured::Restored(restored)) => {
                    // The repair runs behind the fence, which stays closed.
                    let (restore, fence) = restored.into_parts();
                    match clone::repair(restore) {
                        Ok(repaired) => Capture::Repaired { repaired, fence },
                        Err(error) => Capture::Ended(Templated::Unrepaired(error)),
                    }
                }
                Err(error) => Capture::Ended(Templated::Failed(error)),
            }
        })?;
        let (repaired, fence) = match captured {
            Capture::Ended(templated) => return Ok((templated, None)),
            Capture::Repaired { repaired, fence } => (repaired, fence),
        };
        // The activation's deadline starts at the repair's acknowledgement of the restore.
        let acknowledged = repaired.acknowledged_at();
        let activation = Activation::new(&repaired, fence, ACTIVATION_TIMEOUT, Instant::now());
        let template = root_template(&config, gateway);
        let worked = work_clone(
            open_tty(tty)?,
            session,
            operations,
            activation,
            template,
            acknowledged,
        )?;
        // A clone that ends before it is ready, or whose activation failed, ends even if the tty
        // sends nothing.
        let unready = matches!(worked, Worked::Abandoned | Worked::Unactivated { .. });
        Ok((
            Templated::Cloned(worked),
            unready.then_some(UNACTIVATED_DRAIN),
        ))
    })
}

/// Attaches the guest's end of the control session on `stream`, takes the reset with which the
/// broker answers the attachment, acknowledges it with the guest agent's [`RECEIVE_WINDOW`],
/// waits until `send` has sent the acknowledgement, then returns what `capture` returns, which
/// runs without reading the stream any further, with the guest's role of the session, which the
/// acknowledgement activated in the broker's first epoch.
///
/// Fails with [`ServeError::Session`] if the attachment or the acknowledgement cannot be written,
/// or if the broker closes the stream or sends anything but a reset first, and with
/// [`ServeError::Terminal`] if `send` fails, before `capture` runs.
fn attached<S: Read + Write, T>(
    stream: S,
    send: impl FnOnce(&S) -> io::Result<()>,
    capture: impl FnOnce() -> T,
) -> Result<(T, GuestSession), ServeError> {
    let mut connection = GuestConnection::new(stream);
    connection.attach()?;
    // A session takes nothing but a reset before its first epoch, so the event is the broker's
    // reset, which the acknowledgement requires.
    connection.next_event()?;
    connection.acknowledge(RECEIVE_WINDOW)?;
    send(connection.get_ref()).map_err(ServeError::Terminal)?;
    let captured = capture();
    Ok((captured, connection.session().clone()))
}

#[cfg(test)]
mod tests {
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
        let error =
            attached::<_, ()>(&guest, |_| panic!("sent"), || panic!("captured")).unwrap_err();
        assert!(
            matches!(error, ServeError::Session(GuestConnectionError::Io(_))),
            "{error}"
        );

        // The broker closes the stream, or sends anything but a reset first.
        let (guest, broker) = UnixStream::pair().unwrap();
        broker.shutdown(Shutdown::Write).unwrap();
        let error =
            attached::<_, ()>(&guest, |_| panic!("sent"), || panic!("captured")).unwrap_err();
        assert!(
            matches!(error, ServeError::Session(GuestConnectionError::Closed)),
            "{error}"
        );
        let (guest, broker) = UnixStream::pair().unwrap();
        (&broker)
            .write_all(&first(RecordType::Data, b"early"))
            .unwrap();
        let error =
            attached::<_, ()>(&guest, |_| panic!("sent"), || panic!("captured")).unwrap_err();
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
}
