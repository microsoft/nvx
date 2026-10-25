//! The worker of a template's VM, which captures the VM before it assembles any root, through
//! `nvx::guest::clone`, on x86_64, whose time ABI a capture needs. The
//! [`template`](crate::template) module tells such a VM by its kernel command line, and holds the
//! statuses with which its worker ends.
//!
//! [`capture_on_terminal`] opens the VM's control tty, in raw mode, as the worker of a VM that
//! boots cold does, and then, in order:
//!
//! 1. attaches the control session with `nvx::guest::nvxs::GuestConnection::attach`, and waits
//!    until the tty has sent the attachment, so that the snapshot holds the guest attached. It
//!    reads nothing from the session before the capture, and serves no application session: the
//!    host opens none with a template's VM;
//! 2. prepares the guest for the capture with `nvx::guest::clone::CapturePreparation`, behind the
//!    worker's [`Admission`], its fence of workload admission, and from whose [`devices`] nothing
//!    may be mounted: the preparation closes the fence, checks the mount table, waits for the time
//!    ABI's boot checks, runs its capture checks, and suppresses the stall detectors;
//! 3. requests the capture with `CaptureBarrier::capture`, with the platform tier's request,
//!    which keeps no scratch.
//!
//! A committed capture ends the template's VM. The worker returns only once the capture ended
//! without a snapshot, or failed, or in a clone, which a restore of the snapshot started, as a
//! [`Templated`] tells, and with which the worker ends. In a clone, the worker repairs the restore
//! next, with `nvx::guest::clone::repair`, behind its fence, which stays closed: the repair
//! activates the restore's memory target, reseeds the kernel's random number generator, and
//! finishes the restore, with the acknowledgement that the host awaits, which hands the deferred
//! time work to the time daemon. NVX's `/init` started the daemon in the template, with the time
//! ABI's boot step, before the worker, so the snapshot holds it, and PID 1 collects its status. The
//! worker does not activate a clone yet: a clone that it repaired ends at once, with
//! [`RESTORED_STATUS`] and its fence still closed, which may cut the deferred time work short. A
//! repair that failed ends the clone with the [status](nvx::guest::clone::RepairError::status) of
//! its failure, which `nvx::protocol::status` documents, and a capture request that failed in a
//! clone, since the time ABI could not repair its restore, with the status of the time ABI's
//! violation, as [`capture_status`] tells.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;

use nvx::guest::clone::{
    self, AdmissionFence, CaptureBarrier, CaptureCause, CaptureError, CapturePreparation,
    CaptureStage, Captured, Devices, RepairError, Repaired,
};
use nvx::guest::nvxs::GuestConnection;
use nvx::guest::root::Device;
use nvx::guest::terminal::Terminal;
use nvx::guest::time::TimeError;
use nvx::protocol::root::{DISTRO_MMIO_BASE, SCRATCH_MMIO_BASE};

use crate::guest::{self, ServeError};
use crate::template::{CAPTURE_STATUS_BASE, CONTINUED_STATUS, RESTORED_STATUS};

/// The virtio-fs tag of the VM's host filesystem device, OpenVMM's one filesystem slot: dormant in
/// a template's VM, and the device of the host export that the worker of a VM that boots cold
/// mounts.
pub const FILESYSTEM_TAG: &str = "microvm";

/// The worker's fence of workload admission in a template's VM, which it hands the capture's
/// preparation.
///
/// The worker of a template's VM admits no execution before the capture, nor in a clone, which it
/// repairs behind the fence, and whose activation would open it, which the worker does not run
/// yet. Closing the fence acquires it, as `nvx::guest::clone` requires:
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

/// How the capture of a template's VM ended, once the worker attached its session.
#[derive(Debug)]
pub enum Templated {
    /// The capture ended without a snapshot, as without a destination or once OpenVMM rejected
    /// it, and the template continued: the barrier opened the fence, and restored the stall
    /// detectors' settings.
    Continued,
    /// A restore of the template's snapshot started this VM, a clone, which the worker repaired,
    /// and does not activate yet.
    Repaired {
        /// The repaired clone, with its resources.
        repaired: Repaired,
        /// The worker's fence of workload admission, still closed: opening it is the activation's.
        fence: Admission,
    },
    /// A restore of the template's snapshot started this VM, a clone, whose repair failed. The
    /// fence stays closed.
    Unrepaired(RepairError),
    /// A stage of the capture failed. The preparation released what it took, and a failed
    /// request left the fence closed.
    Failed(CaptureError),
}

impl Templated {
    /// Returns the status with which the worker, and with it the VM, ends: [`CONTINUED_STATUS`],
    /// [`RESTORED_STATUS`] once it repaired a clone, the
    /// [status](nvx::guest::clone::RepairError::status) of a repair that failed, or the
    /// [status](capture_status) of a capture that failed.
    pub fn status(&self) -> u8 {
        match self {
            Self::Continued => CONTINUED_STATUS,
            Self::Repaired { .. } => RESTORED_STATUS,
            Self::Unrepaired(error) => error.status(),
            Self::Failed(error) => capture_status(error),
        }
    }
}

impl fmt::Display for Templated {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Continued => formatter.write_str(
                "the template's capture ended without a snapshot, and the template continued",
            ),
            Self::Repaired { repaired, .. } => write!(
                formatter,
                "a restore of the template's snapshot started this clone, which the worker \
                 repaired, with {} online CPUs and {} bytes of memory, and does not activate",
                repaired.cpus(),
                repaired.memory()
            ),
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

/// Captures the template's VM on its control tty at `tty`, as the [module documentation](self)
/// describes, and returns how the capture ended, once the worker repaired a clone that a restore
/// of the snapshot started.
///
/// It opens the tty through [`guest::on_terminal`], as the worker of a VM that boots cold does,
/// and fails as that does if the tty cannot be opened or put in raw mode. It fails with
/// [`ServeError::Session`] if the attachment cannot be written, and with [`ServeError::Terminal`]
/// if the tty does not send it, before the preparation.
pub fn capture_on_terminal(tty: &Path) -> Result<Templated, ServeError> {
    guest::on_terminal(tty, |terminal| {
        let send = |terminal: &&mut Terminal| {
            terminal.drain().map_err(|error| {
                let problem = format!("cannot send the attachment on {}: {error}", tty.display());
                io::Error::new(error.kind(), problem)
            })
        };
        attached(terminal, send, || {
            let preparation = CapturePreparation::new(Admission::default(), devices());
            match preparation.prepare().and_then(CaptureBarrier::capture) {
                Ok(Captured::Continued) => Templated::Continued,
                Ok(Captured::Restored(restored)) => {
                    // The repair runs behind the fence, which stays closed.
                    let (restore, fence) = restored.into_parts();
                    match clone::repair(restore) {
                        Ok(repaired) => Templated::Repaired { repaired, fence },
                        Err(error) => Templated::Unrepaired(error),
                    }
                }
                Err(error) => Templated::Failed(error),
            }
        })
    })
}

/// Attaches the guest's end of the control session on `stream`, waits until `send` has sent the
/// attachment, then returns what `capture` returns, which runs without reading the stream.
///
/// Fails with [`ServeError::Session`] if the attachment cannot be written, and with
/// [`ServeError::Terminal`] if `send` fails, before `capture` runs.
fn attached<S: Read + Write, T>(
    stream: S,
    send: impl FnOnce(&S) -> io::Result<()>,
    capture: impl FnOnce() -> T,
) -> Result<T, ServeError> {
    let mut connection = GuestConnection::new(stream);
    connection.attach()?;
    send(connection.get_ref()).map_err(ServeError::Terminal)?;
    Ok(capture())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::os::unix::net::UnixStream;

    use nvx::guest::nvxs::GuestConnectionError;
    use nvx::protocol::nvxs::GuestSession;
    use nvx::protocol::status;

    use super::*;
    use crate::root::RootStage;
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
        statuses.extend([
            COMMAND_LINE_STATUS,
            CONTINUED_STATUS,
            RESTORED_STATUS,
            SESSION_STATUS,
            0,
        ]);
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
    fn the_worker_attaches_its_session_before_the_capture_and_reads_nothing() {
        let (guest, broker) = UnixStream::pair().unwrap();
        // The broker's record waits on the stream, as a reset would once a host attached.
        (&broker).write_all(b"waiting").unwrap();
        let mut attachment = Vec::new();
        GuestSession::new().attach(&mut attachment).unwrap();
        let sent = Cell::new(false);
        let captured = attached(
            &guest,
            |_| {
                sent.set(true);
                Ok(())
            },
            || {
                // The capture runs once the attachment was sent, which the broker holds.
                assert!(sent.get());
                let mut received = vec![0; attachment.len()];
                (&broker).read_exact(&mut received).unwrap();
                assert_eq!(received, attachment);
                "captured"
            },
        )
        .unwrap();
        assert_eq!(captured, "captured");
        // The worker read nothing of the session.
        let mut waiting = [0; 7];
        (&guest).read_exact(&mut waiting).unwrap();
        assert_eq!(&waiting, b"waiting");
    }

    #[test]
    fn no_capture_runs_without_the_attachment() {
        let (guest, _broker) = UnixStream::pair().unwrap();
        let unsent = io::Error::new(io::ErrorKind::TimedOut, "the tty sent nothing");
        let error = attached::<_, ()>(&guest, |_| Err(unsent), || panic!("captured")).unwrap_err();
        assert!(
            matches!(&error, ServeError::Terminal(error) if error.kind() == io::ErrorKind::TimedOut),
            "{error}"
        );

        let (guest, broker) = UnixStream::pair().unwrap();
        drop(broker);
        let error =
            attached::<_, ()>(&guest, |_| panic!("sent"), || panic!("captured")).unwrap_err();
        assert!(
            matches!(error, ServeError::Session(GuestConnectionError::Io(_))),
            "{error}"
        );
    }
}
