//! The activation of a clone, which a restore of a template's snapshot started, and which the
//! worker repaired, on x86_64, whose time ABI a clone needs: the worker takes the clone's own
//! configuration from the host in one `Activate`, through the clone's
//! `nvx::guest::clone::Activation`, and then serves the host's order as after a cold boot, as the
//! [worker's documentation](super#clones) describes.
//!
//! The activation's failures are final, and end the worker, and with it the VM, with the status of
//! `nvx::protocol::status` that [`failure_status`] selects by the failure's class.

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use nvx::activation::{ActivationRecord, ReadyActivation};
use nvx::guest::clone::{
    Activation, ActivationAction, ActivationCause, ActivationFailure, ActivationInput, ApplyError,
    RootTemplate,
};
use nvx::guest::entry::{Identity, Publication};
use nvx::protocol::status;

use super::capture::Admission;
use super::connection::Connection;
use super::executions::{Executions, Sandbox};
use super::order::Order;
use super::{RootOperations, Worked, serve_root};
use crate::GUEST_BUILD;
use crate::frame::{FrameError, Message};
use crate::guest::{ServeError, Step};
use crate::root::Ready;

/// How long a clone's activation may take, from the repair's acknowledgement of the restore until
/// the clone is ready: long enough for the host to connect once the restore is ready, and for the
/// application of the record to wait for each block device as long as the template's default
/// configuration allows. The application's waits end by then at the latest, and a clone that the
/// host did not activate by then ends.
pub const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(120);

/// Returns the status with which the worker of a clone, and with it the VM, ends once the clone's
/// activation failed with `failure`: the status of `nvx::protocol::status` of the failure's
/// class, by its cause:
///
/// - [`ACTIVATION_DEADLINE`](status::ACTIVATION_DEADLINE): the deadline passed before the clone
///   was ready;
/// - [`ACTIVATION_PROTOCOL`](status::ACTIVATION_PROTOCOL): the host broke the restricted session:
///   it reset the session again, or sent a request before the clone was ready, or an Activate of
///   another transaction, or one that changes the accepted record;
/// - [`ACTIVATION_RECORD`](status::ACTIVATION_RECORD): the Activate's record does not decode,
///   breaks a limit, or is a cold boot's;
/// - [`ACTIVATION_RESOURCES`](status::ACTIVATION_RESOURCES): the record expects other resources
///   than the repaired clone has;
/// - [`ACTIVATION_ROOT`](status::ACTIVATION_ROOT): the root's assembly failed, or the record's
///   image is a GPT disk, which a clone does not assemble yet, as
///   [`ApplyError::Assembly`] and [`ApplyError::Gpt`] tell, or the application failed otherwise;
/// - [`ACTIVATION_WORKLOAD`](status::ACTIVATION_WORKLOAD): the image refuses the record's
///   workload identity, as [`ApplyError::Account`] tells;
/// - [`ACTIVATION_ORDER`](status::ACTIVATION_ORDER): the worker's own order completed an
///   activation that did not run, or started the sandbox before the clone was ready, or again.
///
/// A cause that this build of the adapter does not know ends it with `ACTIVATION_PROTOCOL`.
pub fn failure_status(failure: &ActivationFailure) -> u8 {
    match failure.cause() {
        ActivationCause::Expired => status::ACTIVATION_DEADLINE,
        ActivationCause::Reset
        | ActivationCause::Request
        | ActivationCause::Transaction { .. }
        | ActivationCause::Changed => status::ACTIVATION_PROTOCOL,
        ActivationCause::Record(_) | ActivationCause::Cold => status::ACTIVATION_RECORD,
        ActivationCause::Resources { .. } => status::ACTIVATION_RESOURCES,
        ActivationCause::Failed(error) => match error.downcast_ref::<ApplyError>() {
            Some(ApplyError::Account { .. }) => status::ACTIVATION_WORKLOAD,
            // The root's assembly, a GPT image, and any other failure of the application.
            _ => status::ACTIVATION_ROOT,
        },
        ActivationCause::Completion | ActivationCause::Start => status::ACTIVATION_ORDER,
        _ => status::ACTIVATION_PROTOCOL,
    }
}

/// A clone's activation, as the worker's session drives it, with the inputs of the root's
/// assembly that the clone's template captured, the application that awaits the activation's
/// completion, if any, and the worker's order once the clone is ready.
pub(super) struct CloneState {
    /// The clone's activation, behind the worker's fence of workload admission.
    activation: Activation<Admission>,
    /// The inputs of the root's assembly that the clone's template captured.
    template: RootTemplate,
    /// The application of the accepted record, once it ran, until the worker
    /// [settles](Self::settle) the activation with its result.
    applied: Option<Applied>,
    /// The worker's order, once the clone is ready.
    order: Option<Order>,
}

/// The application of the record of the Activate that a clone's activation accepted, which ran,
/// and which awaits the activation's completion.
struct Applied {
    /// The identity in which every execution runs, if the record has a workload identity, or why
    /// the application failed.
    result: Result<Option<Identity>, ApplyError>,
    /// The number of mappings that the record announces.
    mappings: u32,
    /// The number of retries of the Activate that joined the application, which its completion
    /// answers too.
    joined: usize,
}

impl CloneState {
    /// Returns the state of the clone whose activation is `activation`, in its transport
    /// bootstrap, and whose template captured `template`.
    pub(super) fn new(activation: Activation<Admission>, template: RootTemplate) -> Self {
        Self {
            activation,
            template,
            applied: None,
            order: None,
        }
    }

    /// Takes the session's reset, which the connection acknowledged already: the one that the
    /// restore queued, which ends the activation's transport bootstrap, or a later one once the
    /// clone is ready, as when the host attaches again. Returns how the worker ends once the reset
    /// failed the activation: another reset before the clone is ready.
    pub(super) fn reset(&mut self) -> Option<Worked> {
        let action = self
            .activation
            .input(Instant::now(), ActivationInput::Reset);
        action.err().map(|failure| unactivated(&failure))
    }

    /// Returns when the activation fails unless the clone is ready, if it is not.
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.activation.deadline()
    }

    /// Returns whether the clone is ready: whether the activation completed, which gave the worker
    /// its order.
    pub(super) fn is_ready(&self) -> bool {
        self.order.is_some()
    }

    /// Returns how the worker ends once the activation's deadline passed before the clone was
    /// ready, as it may whether the host sent anything or not.
    pub(super) fn tick(&mut self) -> Option<Worked> {
        let ticked = self.activation.tick(Instant::now());
        ticked.err().map(|failure| unactivated(&failure))
    }

    /// Returns how the worker ends once the session's receipt failed with `error`, if the
    /// activation takes the failure: an `Activate` whose header announced more than
    /// [`MAX_ACTIVATE_BODY`](crate::frame::MAX_ACTIVATE_BODY) bytes, the most that a record has,
    /// which the framing refused before it took any byte of its body. The activation takes it as
    /// an `Activate` whose record has that many bytes, which fails it, with
    /// [`ACTIVATION_RECORD`](status::ACTIVATION_RECORD) once the session is restricted, whether
    /// the clone is ready or not. Any other failure ends the session as it ends that of a VM that
    /// boots cold.
    pub(super) fn refused(&mut self, error: &ServeError) -> Option<Worked> {
        let ServeError::Frame(FrameError::Length { kind, length }) = *error else {
            return None;
        };
        if kind != Message::Activate(Vec::new()).kind() {
            return None;
        }
        let input = ActivationInput::TooLong(length);
        let failure = self.activation.input(Instant::now(), input).err()?;
        Some(unactivated(&failure))
    }

    /// Handles `message` at `now` through the activation, and, once the clone is ready, in the
    /// worker's order, through `operations`, with `executions`, which queue what they answer on
    /// `connection`, as [`serve_root`] does once the root of a VM that boots cold is ready.
    ///
    /// Before the clone is ready, the activation takes an `Activate`, whose record it hands back
    /// once it accepted it, and which the worker applies then, through `operations`, and keeps the
    /// result of until it [settles](Self::settle) the activation. Until then, the activation takes
    /// what the session received meanwhile, as while it activates: a retry of the same
    /// `Activate`, byte for byte, joins the application, and any other message fails the
    /// activation. The worker answers an `Activate` that the activation completed, and each retry
    /// that joined its application, with `Ready`, which reports the activation, as it answers a
    /// retry once the clone is ready. Once the activation failed, the worker drops its executions,
    /// and ends.
    pub(super) fn handle<S: Read + Write + AsFd>(
        &mut self,
        operations: &mut dyn RootOperations,
        executions: &mut Executions,
        connection: &mut Connection<S>,
        now: Instant,
        message: Message,
    ) -> Result<Step<Worked>, ServeError> {
        let input = match &message {
            Message::Activate(record) => ActivationInput::Activate(record),
            _ => ActivationInput::Request,
        };
        let action = match self.activation.input(now, input) {
            Ok(action) => action,
            Err(failure) => return Ok(self.end(executions, &failure)),
        };
        Ok(match action {
            ActivationAction::Apply(record) => {
                self.apply(operations, &record);
                Step::Continue(Vec::new())
            }
            ActivationAction::Join => {
                if let Some(applied) = &mut self.applied {
                    applied.joined += 1;
                }
                Step::Continue(Vec::new())
            }
            ActivationAction::Ready(activation) => Step::Continue(vec![ready(activation)]),
            ActivationAction::Serve => {
                let order = self
                    .order
                    .as_mut()
                    .expect("a ready activation has the worker's order");
                let started = order.is_started();
                let step = serve_root(operations, order, executions, connection, now, message)?;
                if !started
                    && order.is_started()
                    && let Err(failure) = self.activation.start()
                {
                    return Ok(self.end(executions, &failure));
                }
                step
            }
            ActivationAction::Continue | ActivationAction::Acknowledge => {
                Step::Continue(Vec::new())
            }
        })
    }

    /// Applies `record`, the record of the Activate that the activation accepted, through
    /// `operations`, with the template's inputs, by the activation's deadline, and keeps the
    /// application's result until the worker [settles](Self::settle) the activation with it.
    fn apply(&mut self, operations: &mut dyn RootOperations, record: &ActivationRecord) {
        // The application's waits for the devices end by the activation's deadline, past which
        // the completion fails the activation, whatever the application's result.
        let deadline = self.activation.deadline();
        let result = operations.apply(record, &self.template, deadline);
        // A record announces at most `nvx::activation::MAX_MAPPINGS` mappings, which fit a `u32`.
        let mappings = record.configuration().mappings as u32;
        self.applied = Some(Applied {
            result,
            mappings,
            joined: 0,
        });
    }

    /// Completes the activation with the result of the application that ran, if any, once the
    /// worker took what the session received while it ran, which comes before `Ready`, and
    /// returns the worker's step: `Ready`, which reports the activation, for the `Activate` and
    /// for each retry that joined its application, once the clone is ready, after which
    /// `executions` run in the cgroups that the template's bootstrap prepared, as the record's
    /// workload identity, if it has one, and the worker's order awaits the record's mappings; or
    /// the end, once the activation failed.
    pub(super) fn settle(
        &mut self,
        operations: &mut dyn RootOperations,
        executions: &mut Executions,
    ) -> Option<Step<Worked>> {
        let applied = self.applied.take()?;
        let (result, identity) = match applied.result {
            Ok(identity) => (Ok(()), identity),
            Err(error) => (Err(error.into()), None),
        };
        Some(match self.activation.complete(Instant::now(), result) {
            Ok(activation) => {
                let identity = identity.map(|identity| (identity.uid(), identity.gid()));
                // The executions' listeners are published, the entry's default, which refuses
                // the most, as a root configuration that names no publication has them.
                let sandbox = Sandbox::new(identity, Publication::Published);
                executions.run_in(operations.cgroups(), sandbox);
                self.order = Some(Order::new(applied.mappings));
                Step::Continue(vec![ready(activation); 1 + applied.joined])
            }
            Err(failure) => self.end(executions, &failure),
        })
    }

    /// Drops the worker's executions, which ends their programs, once the activation failed with
    /// `failure`, and returns the end of the worker.
    fn end(&mut self, executions: &mut Executions, failure: &ActivationFailure) -> Step<Worked> {
        executions.clear();
        Step::End(Vec::new(), unactivated(failure))
    }
}

/// Returns the worker's `Ready` once the activation completed, which reports `activation` after
/// the guest agent's build.
fn ready(activation: ReadyActivation) -> Message {
    let ready = Ready::new(GUEST_BUILD).expect("this crate's version is a build");
    Message::Ready(ready.with_activation(activation).encode())
}

/// Returns how the worker ends once the activation failed with `failure`: with its
/// [status](failure_status), and its description.
fn unactivated(failure: &ActivationFailure) -> Worked {
    Worked::Unactivated {
        status: failure_status(failure),
        problem: failure.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::num::NonZeroU64;

    use nvx::activation::{
        Configuration, ExpectedResources, Identity, Image, MachineIdMode, OverlayMode,
        RestoreSection, StoreGeneration, TemplateReference, TransactionId, Upper, WorkloadIdentity,
        WorkloadView,
    };
    use nvx::guest::clone::AdmissionFence;
    use nvx::guest::entry;

    use super::*;

    /// The resources of the clones of the tests.
    const RESOURCES: ExpectedResources = ExpectedResources {
        cpus: 2,
        memory: 1 << 30,
    };

    /// Returns a clone's record of the transaction `transaction`, with the workload identity of
    /// `uid`.
    fn record(transaction: u8, uid: u32) -> ActivationRecord {
        let configuration = Configuration {
            image: Image::Erofs {
                binding: Identity::from_bytes([0x11; 32]),
                uuid: None,
            },
            upper: Upper::Tmpfs,
            overlay: OverlayMode::UntrustedLowers,
            export: None,
            mappings: 0,
            network: None,
            workload: WorkloadView {
                hostname: b"clone".to_vec(),
                machine_id: MachineIdMode::Image,
                identity: Some(WorkloadIdentity { uid, gid: uid }),
            },
            resources: RESOURCES,
        };
        let restore = RestoreSection {
            transaction: TransactionId::from_bytes([transaction; 16]),
            template: TemplateReference {
                generation: StoreGeneration::new(NonZeroU64::MIN),
                key: Identity::from_bytes([0x44; 32]),
            },
        };
        ActivationRecord::new(Some(restore), configuration).unwrap()
    }

    /// Returns a clone's activation, in its transport bootstrap, behind a closed fence, with
    /// `resources`, which the deadline ends `timeout` after now.
    fn activation(resources: ExpectedResources, timeout: Duration) -> Activation<Admission> {
        let mut fence = Admission::default();
        fence.close().unwrap();
        let now = Instant::now();
        Activation::with_resources(resources, Some(now), fence, timeout, now)
    }

    /// Returns an activation that took the session's reset, and is restricted.
    fn restricted() -> Activation<Admission> {
        let mut activation = activation(RESOURCES, ACTIVATION_TIMEOUT);
        let action = activation.input(Instant::now(), ActivationInput::Reset);
        assert_eq!(action.unwrap(), ActivationAction::Acknowledge);
        activation
    }

    /// Returns an activation that accepted the record of the transaction 1.
    fn activating() -> Activation<Admission> {
        let mut activation = restricted();
        let bytes = record(1, 1000).encode();
        let action = activation.input(Instant::now(), ActivationInput::Activate(&bytes));
        assert!(
            matches!(action, Ok(ActivationAction::Apply(_))),
            "{action:?}"
        );
        activation
    }

    /// Returns the failure of the activation with which `result` fails.
    fn failed<T: std::fmt::Debug>(result: Result<T, ActivationFailure>) -> ActivationFailure {
        result.unwrap_err()
    }

    #[test]
    fn each_class_of_failure_ends_the_clone_with_its_own_status() {
        let now = Instant::now;
        let bytes = record(1, 1000).encode();
        let mut cases = Vec::new();

        // The deadline.
        let mut expired = activation(RESOURCES, Duration::ZERO);
        cases.push((failed(expired.tick(now())), status::ACTIVATION_DEADLINE));
        // The host's session: another reset, a request, another transaction, a changed record.
        let mut reset = restricted();
        let failure = failed(reset.input(now(), ActivationInput::Reset));
        cases.push((failure, status::ACTIVATION_PROTOCOL));
        let mut request = restricted();
        let failure = failed(request.input(now(), ActivationInput::Request));
        cases.push((failure, status::ACTIVATION_PROTOCOL));
        let early =
            activation(RESOURCES, ACTIVATION_TIMEOUT).input(now(), ActivationInput::Request);
        cases.push((failed(early), status::ACTIVATION_PROTOCOL));
        let other = record(2, 1000).encode();
        let failure = failed(activating().input(now(), ActivationInput::Activate(&other)));
        assert!(matches!(
            failure.cause(),
            ActivationCause::Transaction { .. }
        ));
        cases.push((failure, status::ACTIVATION_PROTOCOL));
        let changed = record(1, 1001).encode();
        let failure = failed(activating().input(now(), ActivationInput::Activate(&changed)));
        assert!(matches!(failure.cause(), ActivationCause::Changed));
        cases.push((failure, status::ACTIVATION_PROTOCOL));
        // The record: one that does not decode, and a cold boot's.
        let failure = failed(restricted().input(now(), ActivationInput::Activate(b"\x01")));
        cases.push((failure, status::ACTIVATION_RECORD));
        let cold = ActivationRecord::new(None, record(1, 1000).configuration().clone()).unwrap();
        let cold = cold.encode();
        let failure = failed(restricted().input(now(), ActivationInput::Activate(&cold)));
        assert!(matches!(failure.cause(), ActivationCause::Cold));
        cases.push((failure, status::ACTIVATION_RECORD));
        // The resources.
        let mut more = activation(
            ExpectedResources {
                cpus: 4,
                ..RESOURCES
            },
            ACTIVATION_TIMEOUT,
        );
        more.input(now(), ActivationInput::Reset).unwrap();
        let failure = failed(more.input(now(), ActivationInput::Activate(&bytes)));
        cases.push((failure, status::ACTIVATION_RESOURCES));
        // The application: of the root, of the workload, and any other.
        let root = activating().complete(now(), Err(ApplyError::Gpt.into()));
        cases.push((failed(root), status::ACTIVATION_ROOT));
        let refused = ApplyError::Account {
            error: entry::Error::Root(io::Error::other("no account")),
            teardown: None,
        };
        let workload = activating().complete(now(), Err(refused.into()));
        cases.push((failed(workload), status::ACTIVATION_WORKLOAD));
        let other = activating().complete(now(), Err("another failure of the application".into()));
        cases.push((failed(other), status::ACTIVATION_ROOT));
        // The worker's own order.
        let failure = failed(restricted().complete(now(), Ok(())));
        assert!(matches!(failure.cause(), ActivationCause::Completion));
        cases.push((failure, status::ACTIVATION_ORDER));
        let failure = failed(restricted().start());
        assert!(matches!(failure.cause(), ActivationCause::Start));
        cases.push((failure, status::ACTIVATION_ORDER));

        for (failure, expected) in &cases {
            assert_eq!(failure_status(failure), *expected, "{failure}");
            let ended = unactivated(failure);
            assert_eq!(ended.status(), *expected, "{failure}");
            assert_eq!(
                ended,
                Worked::Unactivated {
                    status: *expected,
                    problem: failure.to_string(),
                }
            );
        }
        // Every class that the adapter's clones reach has its status; the export's and the
        // network's are left to the activation of those settings.
        let mut statuses = cases
            .iter()
            .map(|(failure, _)| failure_status(failure))
            .collect::<Vec<_>>();
        statuses.sort_unstable();
        statuses.dedup();
        assert_eq!(statuses, [50, 51, 52, 53, 54, 55, 58]);
        assert!(
            statuses
                .iter()
                .all(|status| status::ACTIVATION_STATUSES.contains(status))
        );
    }

    #[test]
    fn the_activation_s_timeout_covers_the_wait_for_both_devices() {
        use super::super::capture::{root_template, template_config};

        let template = template_config();
        assert!(ACTIVATION_TIMEOUT >= 2 * template.device_timeout);
        assert_eq!(root_template(&template).timeout, template.device_timeout);
    }
}
