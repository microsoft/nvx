use std::ffi::CString;

use crate::guest::init::Runtime;
use crate::guest::mappings::{Mapper, MappingTable, SourceRoot};
use crate::guest::sys::{self, Errno, FileDescriptor};
use crate::protocol::{
    self, APP_ACTIVATE, APP_CANCEL, APP_CAPTURE, APP_EXEC, APP_MAPS, APP_PING, APP_READY, APP_STOP,
    APP_STOPPED, INVALID_REQUEST, MAPPINGS_INCOMPLETE, UNSUPPORTED_OPERATION,
};

use super::diagnostics;
use super::mappings::{HostMappings, MappingError};
use super::seam::{Paths, RealSystem, System};
use super::session::{Event, ReceiveFailure, Request, Session};
use super::{AgentConfig, exec, launch, tty};

impl AgentConfig {
    /// Takes a layered cold boot's CLI identity and its already-resolved account defaults.
    pub(crate) fn layered(
        command_line: &[u8],
        user: CString,
        home: CString,
    ) -> Result<Self, Errno> {
        let mut identities = crate::protocol::cmdline::init_tokens(command_line)
            .filter(|token| token.name == b"nvx_identity")
            .filter_map(|token| token.value);
        let identity = identities.next().ok_or(Errno::EINVAL)?;
        let mut ids = identity.split(|&byte| byte == b':');
        let (Some(uid), Some(gid), None) = (ids.next(), ids.next(), ids.next()) else {
            return Err(Errno::EINVAL);
        };
        if identities.next().is_some() {
            return Err(Errno::EINVAL);
        }
        launch::parse_id(uid)?;
        launch::parse_id(gid)?;
        Ok(Self {
            uid: sys::c_string(uid)?,
            gid: sys::c_string(gid)?,
            user,
            home,
            restored_proxy: None,
        })
    }

    /// Serves an initialized layered root, returning shutdown to its init owner.
    pub(crate) fn serve_layered(
        &self,
        runtime: &mut Runtime,
        control: FileDescriptor,
        (table, mapper, export): (MappingTable, Mapper, Option<SourceRoot>),
    ) -> Result<u8, String> {
        let mappings = HostMappings::layered(table, mapper, export)
            .map_err(|error| format!("invalid layered mappings: {}", error.desc()))?;
        self.serve_init(runtime, control, mappings, false)
    }

    /// The Fresh loop has no root, mapper, account resolution or workload admission.
    pub(crate) fn serve_capture(
        runtime: &mut Runtime,
        control: FileDescriptor,
    ) -> Result<u8, String> {
        let config = Self {
            uid: c"0".to_owned(),
            gid: c"0".to_owned(),
            user: c"".to_owned(),
            home: c"".to_owned(),
            restored_proxy: None,
        };
        config.serve_init(runtime, control, HostMappings::default(), true)
    }

    /// Borrows the one init runtime for either boot entry and returns its actual exit status: a
    /// failed restored activation, which its diagnostics named, ends the VM with the status of its
    /// class.
    fn serve_init(
        &self,
        runtime: &mut Runtime,
        control: FileDescriptor,
        mut mappings: HostMappings,
        fresh: bool,
    ) -> Result<u8, String> {
        let capture = runtime.capture(fresh);
        let mut system = RealSystem::new(Paths::default());
        let mut session = Session::with_runtime(control, runtime)?.with_capture(capture);
        run(&mut system, &mut session, self, &mut mappings)
            .map(|Outcome::Stopped| session.capture_status().unwrap_or_default())
            .or_else(|error| match session.runtime_failure() {
                Some(failure) => Err(failure),
                None => session
                    .activation_status()
                    .ok_or_else(|| format!("control session failed: {}", error.desc())),
            })
    }
}

/// Preserves the existing receive-stage diagnostic for a restored capture's RESET.
pub(crate) fn report_failure_for_capture(failure: ReceiveFailure) -> Errno {
    report_failure(&mut RealSystem::new(Paths::default()), failure)
}

/// Terminal outcome of the long-lived dispatch loop.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The host requested STOP, or init accepted an orderly termination.
    Stopped,
}

/// Serves control requests until STOP or a fatal session error.
///
/// The control descriptor is nonblocking, so that the session's send queue never waits for the
/// console while an execution of NVXC version 2 runs.
pub(crate) fn run(
    system: &mut impl System,
    session: &mut Session,
    config: &AgentConfig,
    mappings: &mut HostMappings,
) -> Result<Outcome, Errno> {
    if session.stopping() {
        return Ok(Outcome::Stopped);
    }
    session.descriptor().set_nonblocking()?;
    if let Err(error) = session.send_attach() {
        return if session.stopping() {
            Ok(Outcome::Stopped)
        } else {
            Err(error)
        };
    }
    let mut restored = None;
    loop {
        let payload = match session.next_event() {
            Ok(Event::Reset) => {
                if let Err(error) = session.acknowledge() {
                    if session.stopping() {
                        return Ok(Outcome::Stopped);
                    }
                    diagnostics::error(system, "reset-ack", error as i32);
                    return Err(error);
                }
                continue;
            }
            Ok(Event::Data(payload)) => payload,
            Err(_) if session.stopping() => return Ok(Outcome::Stopped),
            Err(failure) => return Err(report_failure(system, failure)),
        };
        // As in the C agent, a DATA record that fails also reports the `data` stage, after any
        // more specific stage. Its status is the error that ends the session, which
        // `control-session` reports too: `EPROTO` where C's `errno` was incidental, as after a
        // validation failure.
        match handle_data(system, session, config, &mut restored, mappings, &payload) {
            Ok(Some(outcome)) => return Ok(outcome),
            Ok(None) => {}
            Err(_) if session.stopping() => return Ok(Outcome::Stopped),
            Err(error) => {
                diagnostics::error(system, "data", error as i32);
                return Err(error);
            }
        }
    }
}

/// Reports a record from the broker that ended the session at the C agent's stage, and returns
/// the error that ends the session.
fn report_failure(system: &mut impl System, failure: ReceiveFailure) -> Errno {
    match failure {
        ReceiveFailure::Read(error) => {
            diagnostics::error(system, "outer-read", error as i32);
            error
        }
        ReceiveFailure::Type(code) => {
            diagnostics::error(system, "outer-type", i32::from(code));
            Errno::EPROTO
        }
        ReceiveFailure::Reset => {
            diagnostics::error(system, "reset-ack", Errno::EPROTO as i32);
            Errno::EPROTO
        }
        ReceiveFailure::Data(failure) => {
            diagnostics::error(system, failure.kind.stage(), failure.status);
            let error = failure.into_error();
            diagnostics::error(system, "data", error as i32);
            error
        }
    }
}

/// Decodes the NVXC request of one DATA payload and serves it, returning the outcome that ends
/// the session, if any.
fn handle_data(
    system: &mut impl System,
    session: &mut Session,
    config: &AgentConfig,
    restored: &mut Option<AgentConfig>,
    mappings: &mut HostMappings,
    payload: &[u8],
) -> Result<Option<Outcome>, Errno> {
    let request = match session.accept_request(payload) {
        Ok(Request::App(request)) => request,
        // The input of an execution that ended takes no more bytes.
        Ok(Request::Stdin(..)) => return Ok(None),
        Err(failure) => {
            diagnostics::error(system, failure.kind.stage(), failure.status);
            return Err(failure.into_error());
        }
    };
    if session.rootless() && !matches!(request.kind, APP_ACTIVATE | APP_CAPTURE | APP_STOP) {
        session.send_error(
            request.request_id,
            Errno::ENOTSUP as i32,
            "unsupported-operation",
        )?;
        return Ok(None);
    }
    let config = restored.as_ref().unwrap_or(config);
    match request.kind {
        APP_ACTIVATE if session.has_restored() => {
            if let Some(handoff) = session.activate(request)? {
                *mappings = HostMappings::layered(handoff.table, handoff.mapper, handoff.export)?;
                *restored = Some(AgentConfig {
                    uid: CString::new(handoff.workload.uid.to_string()).expect("decimal UID"),
                    gid: CString::new(handoff.workload.gid.to_string()).expect("decimal GID"),
                    user: handoff.user,
                    home: handoff.home,
                    restored_proxy: Some(handoff.proxy),
                });
            }
        }
        APP_CAPTURE => {
            if session.queue_capture(request, false)? {
                return session.finish_capture();
            }
        }
        APP_PING => {
            if request.payload.is_empty() {
                session
                    .send_app(APP_READY, request.request_id, 0, &[])
                    .inspect_err(|error| {
                        diagnostics::error(system, "ping-ready", *error as i32);
                    })?;
            } else {
                diagnostics::error(system, "ping-payload", request.payload.len() as i32);
                session.send_error(request.request_id, Errno::EINVAL as i32, INVALID_REQUEST)?;
            }
        }
        APP_EXEC => {
            if !mappings.table.is_complete() {
                session.send_error(
                    request.request_id,
                    Errno::EAGAIN as i32,
                    MAPPINGS_INCOMPLETE,
                )?;
                return Ok(None);
            }
            let execution = match protocol::decode_guest_exec_payload(request.payload) {
                Ok(execution) => execution,
                Err(_) => {
                    session.send_error(
                        request.request_id,
                        Errno::EINVAL as i32,
                        INVALID_REQUEST,
                    )?;
                    return Ok(None);
                }
            };
            session.start_activation()?;
            let mut stop = None;
            exec::run(
                system,
                session,
                config,
                request.request_id,
                &execution,
                &mut stop,
            )?;
            if let Some(request_id) = stop {
                return stopped(system, session, request_id);
            }
            if session.capture_pending() {
                return session.finish_capture();
            }
        }
        APP_CANCEL => {
            // The targeted workload has already reported its outcome.
        }
        APP_MAPS => match super::mappings::apply_request(system, mappings, request.payload) {
            Ok(()) => session.send_app(APP_READY, request.request_id, 0, &[])?,
            Err(MappingError::Invalid) => {
                session.send_error(request.request_id, Errno::EINVAL as i32, INVALID_REQUEST)?
            }
            Err(MappingError::Mount { status, entry }) => {
                diagnostics::mapping_error(system, status, entry);
                session.send_error(request.request_id, status, protocol::MAPPING_FAILED)?;
            }
        },
        APP_STOP => {
            if !request.payload.is_empty() {
                return Err(Errno::EPROTO);
            }
            return stopped(system, session, request.request_id);
        }
        _ => session.send_error(
            request.request_id,
            Errno::ENOTSUP as i32,
            std::str::from_utf8(UNSUPPORTED_OPERATION).expect("the category is ASCII"),
        )?,
    }
    Ok(None)
}

/// Acknowledges the `STOP` of `request_id`, and drains the control terminal before the session
/// ends.
fn stopped(
    system: &mut impl System,
    session: &mut Session,
    request_id: u64,
) -> Result<Option<Outcome>, Errno> {
    session.send_app(APP_STOPPED, request_id, 0, &[])?;
    tty::drain(system, session.descriptor());
    Ok(Some(Outcome::Stopped))
}

#[cfg(all(test, feature = "cli"))]
mod tests;
