use crate::guest::sys::Errno;
use crate::protocol::{
    self, APP_CANCEL, APP_EXEC, APP_FEATURES, APP_MAPS, APP_PING, APP_READY, APP_STOP, APP_STOPPED,
    GuestFeatures, INVALID_REQUEST, MAPPINGS_INCOMPLETE, UNSUPPORTED_OPERATION,
};

use super::diagnostics;
use super::mappings::{HostMappings, MappingError};
use super::seam::System;
use super::session::{Event, ReceiveFailure, Session};
use super::{AgentConfig, exec, tty};

/// Terminal outcome of the long-lived dispatch loop.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The host requested and acknowledged an orderly stop.
    Stopped,
}

/// Serves control requests until STOP or a fatal session error.
pub(crate) fn run(
    system: &mut impl System,
    session: &mut Session,
    config: &AgentConfig,
    mappings: &mut HostMappings,
) -> Result<Outcome, Errno> {
    session.send_attach()?;
    loop {
        let payload = match session.next_event() {
            Ok(Event::Reset) => {
                session.acknowledge().inspect_err(|error| {
                    diagnostics::error(system, "reset-ack", *error as i32);
                })?;
                continue;
            }
            Ok(Event::Data(payload)) => payload,
            Err(failure) => return Err(report_failure(system, failure)),
        };
        // As in the C agent, a DATA record that fails also reports the `data` stage, after any
        // more specific stage. Its status is the error that ends the session, which
        // `control-session` reports too: `EPROTO` where C's `errno` was incidental, as after a
        // validation failure.
        match handle_data(system, session, config, mappings, &payload) {
            Ok(Some(outcome)) => return Ok(outcome),
            Ok(None) => {}
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
    mappings: &mut HostMappings,
    payload: &[u8],
) -> Result<Option<Outcome>, Errno> {
    let request = match session.accept_request(payload) {
        Ok(request) => request,
        Err(failure) => {
            diagnostics::error(system, failure.kind.stage(), failure.status);
            return Err(failure.into_error());
        }
    };
    match request.kind {
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
            exec::run(system, session, config, request.request_id, &execution)?;
        }
        APP_CANCEL => {
            // The targeted workload has already reported its outcome.
        }
        APP_FEATURES => {
            if request.payload.is_empty() {
                let features = if config.direct {
                    GuestFeatures::DIRECT_AGENT
                } else {
                    GuestFeatures::SANDBOX_AGENT
                };
                session.send_app(APP_READY, request.request_id, 0, &features.encode())?;
            } else {
                session.send_error(request.request_id, Errno::EINVAL as i32, INVALID_REQUEST)?;
            }
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
            session.send_app(APP_STOPPED, request.request_id, 0, &[])?;
            tty::drain(system, session.descriptor());
            return Ok(Some(Outcome::Stopped));
        }
        _ => session.send_error(
            request.request_id,
            Errno::ENOTSUP as i32,
            std::str::from_utf8(UNSUPPORTED_OPERATION).expect("the category is ASCII"),
        )?,
    }
    Ok(None)
}

#[cfg(test)]
mod tests;
