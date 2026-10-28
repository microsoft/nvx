//! The lifecycle transitions that hold an instance's lock.

use std::time::{Duration, Instant};

use super::{
    Found, Instance, Mismatched, Running, Started, Stopped, control_endpoint, generate_capability,
    readiness_failure, session_code,
};
use crate::error::{Error, ErrorCode, Result};
use crate::host::launch::{LaunchSpec, timeout_problem};
use crate::host::process::KILL_TIMEOUT;
use crate::host::report;
use crate::host::session::{self, ControlSession, SessionError};
use crate::host::store::{LaunchId, LockedStore, RemovalError, RuntimeIdentity};
use crate::host::supervisor::{self, Reattached, StopOutcome, VmmCommand};
use crate::protocol::GuestFeatures;

/// An instance whose lock this holds, through which its lifecycle transitions change its
/// records. Dropping it releases the lock.
#[derive(Debug)]
pub struct LockedInstance {
    /// The instance.
    pub(super) instance: Instance,
    /// Its locked store.
    pub(super) locked: LockedStore,
}

impl LockedInstance {
    /// Returns the instance.
    pub fn instance(&self) -> &Instance {
        &self.instance
    }

    /// Finds the instance's VM with [`supervisor::reattach`], under the runtime identity that
    /// `runtime` returns: the one with which the caller would launch it now.
    ///
    /// `runtime` is asked only when the store records a VM or a start, so that an instance that
    /// never started, or that stopped, needs no runtime identity. Reattach ends a start that was
    /// interrupted, signaling only an OpenVMM process that it verified, and forgets a VM that
    /// exited, as one that a record of an earlier host boot names has. Fails as `runtime` fails,
    /// for records that the store does not read, such as those of an earlier format, whose
    /// message names it, and when whether the VM runs cannot be determined, because an
    /// interrupted start cannot be ended yet or the recorded process cannot be examined: the
    /// records then remain for a later attempt.
    ///
    /// A VM whose capability cannot be read, as when its file is missing or damaged, still runs,
    /// and is found [`Found::Running`]: [`Instance::exec`] then fails as reading it failed, and
    /// [`stop`](Self::stop) ends the VM through its verified process identity.
    pub fn find(&mut self, runtime: impl FnOnce() -> Result<RuntimeIdentity>) -> Result<Found> {
        if self.locked.journal()?.is_none() && self.locked.runtime()?.is_none() {
            return Ok(Found::Stopped);
        }
        let runtime = runtime()?;
        Ok(match supervisor::reattach(&mut self.locked, &runtime)? {
            Reattached::Live(vmm) => Found::Running(Running {
                vmm,
                capability: self.locked.read_capability(),
            }),
            Reattached::Mismatched { vmm, differences } => Found::Mismatched(Mismatched {
                name: self.instance.name.clone(),
                vmm,
                differences,
            }),
            Reattached::Exited { .. } => Found::Stopped,
            Reattached::Indeterminate(reason) => {
                return Err(Error::new(
                    ErrorCode::VmmIndeterminate,
                    format!(
                        "cannot determine whether the OpenVMM process of sandbox {} runs: \
                         {reason}",
                        self.instance.name
                    ),
                ));
            }
        })
    }

    /// Launches the VM that `spec` describes under the runtime identity `runtime`, and waits up
    /// to `timeout` until its guest is ready.
    ///
    /// The caller has found the instance stopped with [`find`](Self::find). Start first checks the
    /// export plan with [`ExportPlan::verify`](crate::host::filesystem::ExportPlan::verify): that the
    /// host keeps its read-only paths read-only, and that the mapped host paths still name the
    /// objects they named when they were planned. It launches OpenVMM with
    /// [`supervisor::launch`], whose journal records the launch before OpenVMM is spawned and its
    /// process identity right after, so that [`find`](Self::find) ends a start whose caller died.
    /// The capability travels through OpenVMM's standard-input pipe, which is closed before
    /// OpenVMM starts, and never appears in arguments, the environment, or the log. Once the guest
    /// is ready, has the required features, and has mounted the mapped host paths, start commits
    /// the launch: the runtime record names the VM from then on.
    ///
    /// Fails with [`crate::ErrorKind::Unavailable`], and the code [`ErrorCode::GuestFeaturesMissing`],
    /// when the guest image lacks a feature in [`GuestFeatures::REQUIRED`], and with
    /// [`crate::ErrorKind::Unsupported`] when `timeout` is zero or
    /// longer than [`MAX_TIMEOUT`](crate::host::launch::MAX_TIMEOUT), when the export plan maps a
    /// read-only path that the host cannot keep read-only, when the specification fails
    /// [`LaunchSpec::check`] or exceeds a limit, or when OpenVMM's command line would not fit
    /// [`PROCESS_COMMAND_LINE_LIMIT`](crate::host::launch::PROCESS_COMMAND_LINE_LIMIT), which holds on
    /// every host; such a launch records nothing. Unlike [`crate::host::instance::check_launch`], start keeps no reserve.
    /// A launch that does not become ready is terminated through its verified process identity;
    /// one that cannot be ended keeps its journal, and the next transition ends it.
    pub fn start(
        &mut self,
        spec: &LaunchSpec,
        runtime: &RuntimeIdentity,
        timeout: Duration,
    ) -> Result<Started> {
        check_timeout("start timeout", timeout, true)?;
        if let Some(plan) = &spec.filesystem {
            plan.verify()?;
        }
        let instance = &self.instance;
        let report = instance.report_path();
        // Every control endpoint of the instance has the same length, so a launch that would not
        // fit is refused before the store records it.
        let endpoint = control_endpoint(&instance.store.socket_path())?;
        spec.fit_process_command_line(&spec.openvmm_arguments(&endpoint, &report)?, 0)?;
        let capability = generate_capability()?;
        let launch = LaunchId::generate()?;
        let started = Instant::now();
        let starting = supervisor::launch(
            &mut self.locked,
            &launch,
            runtime,
            &capability,
            |endpoint| {
                let arguments = spec.openvmm_arguments(endpoint, &report)?;
                spec.fit_process_command_line(&arguments, 0)?;
                let mut command = VmmCommand::new(&spec.openvmm, arguments, instance.dir());
                command.breakaway_from_job = spec.breakaway_from_job;
                Ok(command)
            },
        )?;
        let (pid, start_time) = {
            let reference = starting.process().reference();
            (reference.pid, reference.start_time)
        };
        let deadline = started + timeout;
        let ready = session::connect(starting.endpoint(), pid, start_time, deadline)
            .and_then(|transport| ControlSession::attach(transport, &capability, deadline))
            .and_then(|mut session| {
                session.ping(deadline)?;
                let features = session.features(deadline)?;
                Ok((session, features))
            });
        let (mut session, features) = match ready {
            Ok(ready) => ready,
            Err(error) => {
                let reason = readiness_failure(&error, spec.identity.create_account);
                return Err(instance.abort_start(starting, session_code(&error), &reason));
            }
        };
        let missing = features.missing(GuestFeatures::REQUIRED);
        if !missing.is_empty() {
            drop(session);
            return Err(instance.reject_guest(starting, &missing));
        }
        // Workloads run only once the guest has mounted every mapped host path.
        if let Some(plan) = &spec.filesystem
            && let Err(error) = session.map_host_paths(&plan.guest_table(), deadline)
        {
            drop(session);
            let reason = format!("the host paths were not mapped: {error}");
            return Err(instance.abort_start(starting, session_code(&error), &reason));
        }
        drop(session);
        let vmm = starting.commit()?;
        Ok(Started {
            runtime: vmm.record().clone(),
            boot: started.elapsed(),
        })
    }

    /// Asks the guest of the running VM `running` to shut down, and terminates OpenVMM through
    /// its verified process identity if it has not exited within `timeout`, as [`crate::host::supervisor::Vmm::stop`]
    /// does with a forced step, or at once when the guest cannot be asked, as without the VM's
    /// capability.
    ///
    /// A running execution keeps the control session until it ends, so a stop that cannot reach
    /// the guest in time terminates the VM, and the execution then fails. Once OpenVMM has
    /// exited, the stop clears the runtime record and reads OpenVMM's outcome report. Fails with
    /// [`crate::ErrorKind::Unsupported`] when `timeout` is zero or longer than
    /// [`MAX_TIMEOUT`](crate::host::launch::MAX_TIMEOUT), and when OpenVMM cannot be terminated, or does
    /// not exit after it was, which keeps its runtime record.
    pub fn stop(&mut self, running: Running, timeout: Duration) -> Result<Stopped> {
        check_timeout("stop timeout", timeout, true)?;
        let deadline = Instant::now() + timeout;
        let Running { vmm, capability } = running;
        let stopped = vmm.stop(
            &mut self.locked,
            deadline,
            Some(KILL_TIMEOUT),
            |vmm, deadline| {
                // Without its capability, no session can reach the guest, and the forced step
                // ends the VM.
                let capability =
                    capability.map_err(|error| SessionError::Protocol(error.to_string()))?;
                let process = vmm.record().process();
                let transport =
                    session::connect(vmm.endpoint(), process.pid, process.start_time, deadline)?;
                ControlSession::attach(transport, &capability, deadline)?.stop(deadline)
            },
        )?;
        let forced = match stopped.outcome {
            StopOutcome::Graceful => false,
            StopOutcome::Forced => true,
            StopOutcome::TimedOut => {
                return Err(Error::other(format!(
                    "OpenVMM process {} of sandbox {} did not exit after it was terminated",
                    vmm.record().process().pid,
                    self.instance.name
                )));
            }
        };
        Ok(Stopped {
            forced,
            report: report::read(&self.instance.report_path()),
        })
    }

    /// Stops the VM `mismatched`, which runs under another runtime identity than the caller's, by
    /// identity-verified forced termination: terminates OpenVMM through its verified process
    /// identity, as [`crate::host::supervisor::Vmm::terminate`] does, and waits up to `timeout` for it to exit.
    ///
    /// The VM's guest might not speak this build's control protocol, so the stop opens no control
    /// session to it, and does not ask it to shut down: only the OpenVMM process that
    /// [`find`](Self::find) verified is signaled. Once OpenVMM has exited, the stop clears the
    /// runtime record. It reads no outcome report, so [`Stopped::report`] is `None`: a terminated
    /// OpenVMM writes none, and one that exited before it was terminated wrote it under the other
    /// runtime's contracts. [`Stopped::forced`] is false only for an OpenVMM that exited before it
    /// was terminated.
    ///
    /// Fails with [`crate::ErrorKind::Unsupported`] when `timeout` is zero or longer than
    /// [`MAX_TIMEOUT`](crate::host::launch::MAX_TIMEOUT), and when OpenVMM's state cannot be
    /// determined, OpenVMM cannot be terminated, or it does not exit within `timeout`: its runtime
    /// record then remains, and a later stop retries.
    pub fn terminate(&mut self, mismatched: Mismatched, timeout: Duration) -> Result<Stopped> {
        check_timeout("stop timeout", timeout, true)?;
        let Mismatched { vmm, .. } = mismatched;
        let forced = match vmm.terminate(&mut self.locked, timeout)? {
            StopOutcome::Graceful => false,
            StopOutcome::Forced => true,
            StopOutcome::TimedOut => {
                return Err(Error::other(format!(
                    "OpenVMM process {} of sandbox {} did not exit after it was terminated",
                    vmm.record().process().pid,
                    self.instance.name
                )));
            }
        };
        Ok(Stopped {
            forced,
            report: None,
        })
    }

    /// Removes the stopped instance: its store's records and files, the caller's files that it
    /// declared, the configuration record last, and its directory, through the store's
    /// [`LockedStore::remove`], and releases the lock. Open files, such as the lock files of other
    /// transitions, that keep the emptied directory are then waited for until `deadline`, or for
    /// as long as it takes without one, as [`Lingering::wait`](crate::host::store::Lingering::wait)
    /// does.
    ///
    /// The caller has found the instance stopped with [`find`](Self::find): the removal refuses,
    /// and removes nothing, while the store records a VM or a start, and for an instance whose
    /// caller declared no configuration record. Entries in the directory that are neither the
    /// store's nor the caller's are refused, and kept with the directory, once the rest is gone,
    /// among them the temporary files that interrupted writes of the caller's files left, which
    /// the caller removes first with [`crate::host::instance::remove_interrupted_writes`].
    pub fn remove(self, deadline: Option<Instant>) -> std::result::Result<(), RemovalError> {
        match self.locked.remove()? {
            None => Ok(()),
            Some(mut lingering) => lingering.wait(deadline),
        }
    }
}

/// Fails with [`crate::ErrorKind::Unsupported`] when the duration that `name` describes is longer than
/// [`MAX_TIMEOUT`](crate::host::launch::MAX_TIMEOUT), or zero while it must be `positive`, so that no
/// deadline derived from it overflows.
pub(super) fn check_timeout(name: &str, value: Duration, positive: bool) -> Result<()> {
    match timeout_problem(value, positive) {
        Some(problem) => Err(Error::unsupported(format!("the {name} {problem}"))),
        None => Ok(()),
    }
}
