//! The activation of a clone, which a restore of a template's snapshot started, and which the
//! worker repaired, on x86_64, whose time ABI a clone needs: the worker takes the clone's own
//! configuration from the activation record of its restore packet, through the clone's
//! `nvx::guest::clone::Activation`, answers the host's `Await` with `Ready` once the clone is
//! ready, and then serves the host's order as after a cold boot, as the
//! [worker's documentation](super#clones) describes.
//!
//! The activation's failures are final, and end the worker, and with it the VM, with the status of
//! `nvx::protocol::status` that [`failure_status`] selects by the failure's class.
//!
//! The worker of a clone also answers the [`ACTIVATION_PROBE`] once the clone is ready, at any
//! time, executions running or not, with when the events of the clone's activation and order
//! happened, in microseconds after the repair's acknowledgement of the restore, before which the
//! host's input must not reach the clone:
//!
//! - `acknowledged`: `yes` once the repair acknowledged the restore, and `none` for a restore
//!   whose host did not gate its input until the acknowledgement, whose repair acknowledged
//!   nothing;
//! - `repair-delay`: how long, in microseconds, the worker waited before the repair, as the
//!   template's `template::REPAIR_DELAY_FILE` asked, for tests, and `none` without the file;
//! - `reset`: when the clone took the reset that the restore queued, at which it started to apply
//!   the record of its restore packet;
//! - `message`: when the host's first message came, which must be its `Await`;
//! - `applied`: when the application of the record completed, which may be before the host's
//!   first message;
//! - `ready`: when the clone was ready: once the record applied and the host asked for its
//!   Ready;
//! - `started`: when the clone's sandbox started;
//! - `execution`: when the start of the host's first execution came.
//!
//! Each event is `none` until it happened, `before` if it happened before the acknowledgement, and
//! `unknown` without one.

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use nvx::activation::{self, ActivationRecord, ReadyActivation};
use nvx::guest::clone::{
    Activation, ActivationAction, ActivationCause, ActivationFailure, ActivationInput,
    ActivationState, ApplyError, RootTemplate,
};
use nvx::guest::entry::Publication;
use nvx::protocol::status;

use super::capture::Admission;
use super::connection::Connection;
use super::executions::{Executions, Sandbox};
use super::order::Order;
use super::{RootOperations, Worked, serve_root};
use crate::GUEST_BUILD;
use crate::frame::Message;
use crate::guest::{ServeError, Step};
use crate::probe::{Facts, Probe};
use crate::root::Ready;

/// How long a clone's activation may take, from the repair's acknowledgement of the restore until
/// the clone is ready: long enough for the reset that the restore queued to come, and for the
/// application of the record to wait for each block device as long as the template's default
/// configuration allows. The application's waits end by then at the latest, and a clone that is
/// not ready by then ends.
pub const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(120);

/// The probe that the worker of a clone answers with when the events of the clone's activation and
/// order happened, as the [module documentation](self) describes, and which the worker of a VM
/// that boots cold does not run.
pub const ACTIVATION_PROBE: &str = "activation";

/// Returns the status with which the worker of a clone, and with it the VM, ends once the clone's
/// activation failed with `failure`: the status of `nvx::protocol::status` of the failure's
/// class, by its cause:
///
/// - [`ACTIVATION_DEADLINE`](status::ACTIVATION_DEADLINE): the deadline passed before the clone
///   was ready;
/// - [`ACTIVATION_PROTOCOL`](status::ACTIVATION_PROTOCOL): the host sent a request other than its
///   `Await` before the clone was ready;
/// - [`ACTIVATION_RECORD`](status::ACTIVATION_RECORD): the restore packet carried no activation
///   record, or its record does not decode, breaks a limit, or is a cold boot's;
/// - [`ACTIVATION_RESOURCES`](status::ACTIVATION_RESOURCES): the record expects other resources
///   than the repaired clone has;
/// - [`ACTIVATION_ROOT`](status::ACTIVATION_ROOT): the root's assembly failed, the scratch's upper
///   is not fresh among its causes, or the kernel did not reread the partition table of the
///   record's GPT disk before it, as [`ApplyError::Assembly`] and [`ApplyError::Gpt`] tell, or
///   the application failed otherwise, as this build of the adapter does not know;
/// - [`ACTIVATION_WORKLOAD`](status::ACTIVATION_WORKLOAD): the machine ID that the record asks the
///   guest to generate cannot be written, as [`ApplyError::MachineId`] tells, or the record's
///   hostname or that machine ID cannot be set for every execution, as [`ApplyError::View`]
///   tells;
/// - [`ACTIVATION_EXPORT`](status::ACTIVATION_EXPORT): the host export of the record cannot be
///   mounted, as [`ApplyError::Export`] tells;
/// - [`ACTIVATION_NETWORK`](status::ACTIVATION_NETWORK): the resolver file that names the DNS
///   server of the record's network cannot be written or set for every execution, or the
///   announcer of the workload's listeners, which the network publishes, cannot start, as
///   [`ApplyError::Network`] tells. The worker also ends a clone whose announcer ended with it,
///   whether the clone was ready or not, outside the activation;
/// - [`ACTIVATION_ORDER`](status::ACTIVATION_ORDER): the worker's own order accepted the record
///   again, completed an activation that did not run, or started the sandbox before the clone was
///   ready, or again.
///
/// A cause that this build of the adapter does not know ends it with `ACTIVATION_PROTOCOL`.
pub fn failure_status(failure: &ActivationFailure) -> u8 {
    match failure.cause() {
        ActivationCause::Expired => status::ACTIVATION_DEADLINE,
        ActivationCause::Request => status::ACTIVATION_PROTOCOL,
        ActivationCause::Missing | ActivationCause::Record(_) | ActivationCause::Cold => {
            status::ACTIVATION_RECORD
        }
        ActivationCause::Resources { .. } => status::ACTIVATION_RESOURCES,
        ActivationCause::Failed(error) => match error.downcast_ref::<ApplyError>() {
            Some(ApplyError::Gpt(_) | ApplyError::Assembly(_)) => status::ACTIVATION_ROOT,
            Some(ApplyError::MachineId { .. } | ApplyError::View { .. }) => {
                status::ACTIVATION_WORKLOAD
            }
            Some(ApplyError::Export { .. }) => status::ACTIVATION_EXPORT,
            Some(ApplyError::Network { .. }) => status::ACTIVATION_NETWORK,
            // A failure of the application that this build does not know.
            _ => status::ACTIVATION_ROOT,
        },
        ActivationCause::Completion | ActivationCause::Start => status::ACTIVATION_ORDER,
        _ => status::ACTIVATION_PROTOCOL,
    }
}

/// A clone's activation, as the worker's session drives it, with the inputs of the root's
/// assembly that the clone's template captured, the application that awaits the activation's
/// completion, if any, the host's `Await`s that joined it, and the worker's order once the record
/// applied.
pub(super) struct CloneState {
    /// The clone's activation, behind the worker's fence of workload admission.
    activation: Activation<Admission>,
    /// The inputs of the root's assembly that the clone's template captured.
    template: RootTemplate,
    /// The application of the accepted record, once it ran, until the worker
    /// [settles](Self::settle) the activation with its result.
    applied: Option<Applied>,
    /// The number of the host's `Await`s of the current session that joined the activation, which
    /// its completion answers.
    joined: usize,
    /// The worker's order, once the record applied, which serves the host once the clone is ready.
    order: Option<Order>,
    /// When the events of the clone's activation and order happened.
    timeline: Timeline,
}

/// The application of the record that a clone's activation accepted, which ran, and which awaits
/// the activation's completion.
struct Applied {
    /// Whether the application succeeded, or why it failed.
    result: Result<(), ApplyError>,
    /// The number of mappings that the record announces.
    mappings: u32,
    /// The hostname of every execution: the record's.
    hostname: Vec<u8>,
    /// The publication of the listeners of every execution: that of the record's network, and
    /// unpublished without one.
    publication: Publication,
}

impl CloneState {
    /// Returns the state of the clone whose activation is `activation`, pending, whose template
    /// captured `template`, whose worker waited `repair_delay` before the repair, if the template
    /// set a delay, and whose repair acknowledged the restore at `acknowledged`, if it did.
    pub(super) fn new(
        activation: Activation<Admission>,
        template: RootTemplate,
        acknowledged: Option<Instant>,
        repair_delay: Option<Duration>,
    ) -> Self {
        Self {
            activation,
            template,
            applied: None,
            joined: 0,
            order: None,
            timeline: Timeline {
                acknowledged,
                repair_delay,
                ..Timeline::default()
            },
        }
    }

    /// Takes the session's reset, which `connection` acknowledged already: the one that the
    /// restore queued, or a later one, as when the host attaches again, which abandons the `Await`s
    /// of the earlier session. At the first, the worker activates the clone: it writes what the
    /// stream takes of the acknowledgement first, since OpenVMM's broker attaches the host only
    /// once the guest acknowledged the reset, so that the host's attachment and the application of
    /// the record overlap, then accepts the record of the clone's restore packet, and applies it
    /// through `operations`, as [`apply`](Self::apply) describes. Returns how the worker ends once
    /// the activation failed: a record that the activation refuses, or a deadline that passed.
    pub(super) fn reset<S: Read + Write + AsFd>(
        &mut self,
        operations: &mut dyn RootOperations,
        connection: &mut Connection<S>,
    ) -> Option<Worked> {
        let now = Instant::now();
        self.timeline.reset.get_or_insert(now);
        if let Err(failure) = self.activation.input(now, ActivationInput::Reset) {
            return Some(unactivated(&failure));
        }
        self.joined = 0;
        if self.activation.state() != ActivationState::Pending {
            return None;
        }
        // A failure of the stream fails the worker's next flush, once the session took what it
        // received, so that a clone that ends before it is ready still ends with its own status.
        let _ = connection.flush();
        let record = match self.activation.accept(Instant::now()) {
            Ok(record) => record,
            Err(failure) => return Some(unactivated(&failure)),
        };
        self.apply(operations, &record);
        None
    }

    /// Returns when the activation fails unless the clone is ready, if it is not.
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.activation.deadline()
    }

    /// Returns whether the clone is ready: whether the record applied, which gave the worker its
    /// order, and the host asked for Ready.
    pub(super) fn is_ready(&self) -> bool {
        self.activation.established().is_some()
    }

    /// Returns how the worker ends once the activation's deadline passed before the clone was
    /// ready, as it may whether the host sent anything or not.
    pub(super) fn tick(&mut self) -> Option<Worked> {
        let ticked = self.activation.tick(Instant::now());
        ticked.err().map(|failure| unactivated(&failure))
    }

    /// Handles `message` at `now` through the activation, and, once the clone is ready, in the
    /// worker's order, through `operations`, with `executions`, which queue what they answer on
    /// `connection`, as [`serve_root`] does once the root of a VM that boots cold is ready.
    ///
    /// While the record applies, the activation takes the host's `Await`, which joins it, and
    /// which its completion answers with `Ready`, once the worker [settles](Self::settle) it. Once
    /// the record applied, the worker answers an `Await` with `Ready` at once, the first of which
    /// makes the clone ready, and it answers the [`ACTIVATION_PROBE`] itself once the clone is
    /// ready, and serves every other message in its order. Before the clone is ready, any other
    /// message fails the activation. Once the activation failed, the worker drops its executions,
    /// and ends.
    pub(super) fn handle<S: Read + Write + AsFd>(
        &mut self,
        operations: &mut dyn RootOperations,
        executions: &mut Executions,
        connection: &mut Connection<S>,
        now: Instant,
        message: Message,
    ) -> Result<Step<Worked>, ServeError> {
        self.timeline.message.get_or_insert(now);
        let input = match &message {
            Message::Await => ActivationInput::Await,
            _ => ActivationInput::Request,
        };
        let action = match self.activation.input(now, input) {
            Ok(action) => action,
            Err(failure) => return Ok(self.end(executions, &failure)),
        };
        Ok(match action {
            ActivationAction::Join => {
                self.joined += 1;
                Step::Continue(Vec::new())
            }
            ActivationAction::Ready(activation) => {
                self.timeline.ready.get_or_insert(now);
                Step::Continue(vec![ready(activation)])
            }
            ActivationAction::Serve => {
                if let Some(answer) = self.timeline.answer(&message) {
                    return Ok(Step::Continue(vec![answer]));
                }
                if matches!(message, Message::ExecStart(_)) {
                    self.timeline.execution.get_or_insert(now);
                }
                let order = self
                    .order
                    .as_mut()
                    .expect("a ready activation has the worker's order");
                let started = order.is_started();
                let step = serve_root(operations, order, executions, connection, now, message)?;
                if !started && order.is_started() {
                    self.timeline.started = Some(now);
                    if let Err(failure) = self.activation.start() {
                        return Ok(self.end(executions, &failure));
                    }
                }
                step
            }
            ActivationAction::Continue | ActivationAction::Acknowledge => {
                Step::Continue(Vec::new())
            }
        })
    }

    /// Applies `record`, the record of the restore packet that the activation accepted, through
    /// `operations`, with the template's inputs, by the activation's deadline, and keeps the
    /// application's result, with the record's hostname, its publication of listeners, and the
    /// number of its mappings, until the worker [settles](Self::settle) the activation with it.
    fn apply(&mut self, operations: &mut dyn RootOperations, record: &ActivationRecord) {
        // The application's waits for the devices end by the activation's deadline, past which
        // the completion fails the activation, whatever the application's result.
        let deadline = self.activation.deadline();
        let result = operations.apply(record, &self.template, deadline);
        // The application completed: before the worker takes what the session received while it
        // ran, such as the host's `Await`, which therefore comes after it.
        self.timeline.applied = Some(Instant::now());
        let configuration = record.configuration();
        // A record announces at most `nvx::activation::MAX_MAPPINGS` mappings, which fit a `u32`.
        let mappings = configuration.mappings as u32;
        // The executions enter the root with the record's publication of listeners, as a launch's
        // executions enter it with its root configuration's: published, which refuses
        // `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW`, under a network that publishes them, and
        // unpublished otherwise, a clone without a network included.
        let publication = match configuration.network.as_ref() {
            Some(network) if network.publication == activation::Publication::Published => {
                Publication::Published
            }
            _ => Publication::Unpublished,
        };
        self.applied = Some(Applied {
            result,
            mappings,
            hostname: configuration.workload.hostname.clone(),
            publication,
        });
    }

    /// Completes the activation with the result of the application that ran, if any, once the
    /// worker took what the session received while it ran, which comes before `Ready`, and
    /// returns the worker's step: `Ready`, which reports the activation, for each `Await` of the
    /// current session that joined it, which made the clone ready, and nothing without one, until
    /// the host's next `Await`, once the record applied, after which `executions` run in the
    /// cgroups that the template's bootstrap prepared, each as the identity that it names, with
    /// the record's hostname and publication of listeners, and the worker's order awaits the
    /// record's mappings; or the end, once the activation failed.
    pub(super) fn settle(
        &mut self,
        operations: &mut dyn RootOperations,
        executions: &mut Executions,
    ) -> Option<Step<Worked>> {
        let applied = self.applied.take()?;
        let result = applied.result.map_err(Into::into);
        Some(match self.activation.complete(Instant::now(), result) {
            Ok(activation) => {
                if self.activation.established().is_some() {
                    self.timeline.ready = Some(Instant::now());
                }
                // A clone has no workload identity of the whole sandbox: each execution names
                // its own.
                let sandbox =
                    Sandbox::new(None, applied.publication).with_hostname(applied.hostname);
                executions.run_in(operations.cgroups(), sandbox);
                self.order = Some(Order::new(applied.mappings));
                Step::Continue(vec![ready(activation); std::mem::take(&mut self.joined)])
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

/// When the events of a clone's activation and order happened, which the [`ACTIVATION_PROBE`]
/// reports, as the [module documentation](self) describes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Timeline {
    /// When the repair acknowledged the restore, if it did.
    acknowledged: Option<Instant>,
    /// How long the worker waited before the repair, if the template set a delay.
    repair_delay: Option<Duration>,
    /// When the clone took the reset that the restore queued.
    reset: Option<Instant>,
    /// When the host's first message came.
    message: Option<Instant>,
    /// When the application of the record completed.
    applied: Option<Instant>,
    /// When the clone was ready.
    ready: Option<Instant>,
    /// When the clone's sandbox started.
    started: Option<Instant>,
    /// When the start of the host's first execution came.
    execution: Option<Instant>,
}

impl Timeline {
    /// Returns the worker's answer to `message` if it is the [`ACTIVATION_PROBE`]: the timeline's
    /// [facts](Self::facts), or the problem of a probe with arguments.
    fn answer(&self, message: &Message) -> Option<Message> {
        let Message::Probe(body) = message else {
            return None;
        };
        let probe = Probe::decode(body).ok()?;
        if probe.name() != ACTIVATION_PROBE {
            return None;
        }
        Some(if probe.arguments().is_empty() {
            Message::Report(self.facts().encode())
        } else {
            let problem = format!("the {ACTIVATION_PROBE} probe takes no arguments");
            Message::ProbeFailed(problem.into_bytes())
        })
    }

    /// Returns the timeline's facts, as the [module documentation](self) describes them.
    fn facts(&self) -> Facts {
        let mut facts = Facts::new();
        let acknowledged = if self.acknowledged.is_some() {
            "yes"
        } else {
            "none"
        };
        facts.push("acknowledged", acknowledged);
        let repair_delay = self
            .repair_delay
            .map_or("none".to_owned(), |delay| delay.as_micros().to_string());
        facts.push("repair-delay", repair_delay);
        let events = [
            ("reset", self.reset),
            ("message", self.message),
            ("applied", self.applied),
            ("ready", self.ready),
            ("started", self.started),
            ("execution", self.execution),
        ];
        for (name, at) in events {
            let value = match (self.acknowledged, at) {
                (_, None) => "none".to_owned(),
                (None, Some(_)) => "unknown".to_owned(),
                (Some(acknowledged), Some(at)) => at
                    .checked_duration_since(acknowledged)
                    .map_or("before".to_owned(), |after| after.as_micros().to_string()),
            };
            facts.push(name, value);
        }
        facts
    }
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
mod tests;
