//! A VM's [`View`], its hostname and its [`MachineIdMode`], which cold boots and restores share,
//! and a cold boot's tokens of it: `nvx_hostname=HEX`, the hostname's bytes as lowercase
//! hexadecimal, two digits per byte, and `nvx_machine_id=image` or `nvx_machine_id=generated`.
//!
//! The hostname holds 1 to [`MAX_HOSTNAME_BYTES`] bytes without a NUL, as [`View`] specifies,
//! without a UTF-8 or DNS restriction. It is always encoded, even for an ASCII hostname:
//! `box` is `nvx_hostname=626f78`, not the legacy sandbox's raw `nvx_hostname=box`.
//! Consumers of this codec use the decoded bytes, not the legacy raw-hostname reader.
//! Restores receive their view through a guest-restore record instead.

use super::cmdline;
use super::{ProtocolError, violation};
use crate::hex::hex;

/// The longest hostname, in bytes: Linux's `HOST_NAME_MAX`, which the guest's entry applies.
pub const MAX_HOSTNAME_BYTES: usize = 64;

/// The machine ID that the workload sees at `/etc/machine-id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MachineIdMode {
    /// The image's own machine ID, which the guest leaves untouched.
    Image,
    /// A machine ID that the guest generates when it boots or restores.
    Generated,
}

/// The view that a VM's workload has of it: its hostname and its machine ID, without an identity
/// of the executions, which each execution names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct View {
    /// The hostname: 1 to [`MAX_HOSTNAME_BYTES`] bytes, without a NUL.
    pub hostname: Vec<u8>,
    /// The machine ID that the workload sees.
    pub machine_id: MachineIdMode,
}

/// Returns whether `hostname` is a hostname that a VM's [`View`] holds: 1 to
/// [`MAX_HOSTNAME_BYTES`] bytes without a NUL, since the guest's entry sets any other.
pub(crate) fn is_hostname(hostname: &[u8]) -> bool {
    !hostname.is_empty() && hostname.len() <= MAX_HOSTNAME_BYTES && !hostname.contains(&0)
}

/// Prefix of the token that encodes the hostname's bytes as lowercase hexadecimal.
pub const HOSTNAME_TOKEN: &str = "nvx_hostname=";
/// Prefix of the token that names the machine-ID mode, `image` or `generated`.
pub const MACHINE_ID_TOKEN: &str = "nvx_machine_id=";

/// The name of the kernel parameter of [`HOSTNAME_TOKEN`].
const HOSTNAME: &str = "nvx_hostname";
/// The name of the kernel parameter of [`MACHINE_ID_TOKEN`].
const MACHINE_ID: &str = "nvx_machine_id";
/// The names of the view's kernel parameters.
const PARAMETERS: [&str; 2] = [HOSTNAME, MACHINE_ID];

/// Renders the hostname token, then the machine-ID token.
///
/// Fails with a [`ProtocolError`] if the hostname is not 1 to [`MAX_HOSTNAME_BYTES`] non-NUL
/// bytes. Its hexadecimal value needs no quoting or escaping on the kernel command line.
pub fn tokens(view: &View) -> Result<[String; 2], ProtocolError> {
    if !is_hostname(&view.hostname) {
        return Err(violation(format!(
            "the view's hostname must be 1 to {MAX_HOSTNAME_BYTES} bytes without a NUL"
        )));
    }
    let hostname = hex(&view.hostname);
    let machine_id = match view.machine_id {
        MachineIdMode::Image => "image",
        MachineIdMode::Generated => "generated",
    };
    Ok([
        format!("{HOSTNAME_TOKEN}{hostname}"),
        format!("{MACHINE_ID_TOKEN}{machine_id}"),
    ])
}

/// Reads a complete view, or `None` when neither token is present.
///
/// Reads exact token names with [`cmdline::init_tokens`]: spaces, tabs and newlines separate
/// tokens, quotes are literal, and `--` does not stop the reader. Both tokens must occur
/// exactly once, in either order. Missing values, partial pairs, duplicates and values outside
/// the forms of the [module](self) fail with a [`ProtocolError`]; unrelated tokens are ignored.
pub fn parse(command_line: &[u8]) -> Result<Option<View>, ProtocolError> {
    let mut hostname = None;
    let mut machine_id = None;
    for token in cmdline::init_tokens(command_line) {
        let Some((parameter, value)) = cmdline::boot_parameter(&PARAMETERS, token)? else {
            continue;
        };
        let seen = match parameter {
            HOSTNAME => hostname.is_some(),
            _ => machine_id.is_some(),
        };
        if seen {
            return Err(cmdline::repeated_parameter(parameter));
        }
        if parameter == HOSTNAME {
            hostname = Some(parse_hostname(value)?);
        } else {
            machine_id = Some(match value {
                b"image" => MachineIdMode::Image,
                b"generated" => MachineIdMode::Generated,
                _ => {
                    return Err(violation(
                        "the nvx_machine_id kernel parameter must be image or generated",
                    ));
                }
            });
        }
    }
    match (hostname, machine_id) {
        (Some(hostname), Some(machine_id)) => Ok(Some(View {
            hostname,
            machine_id,
        })),
        (None, None) => Ok(None),
        _ => Err(violation(
            "a view needs both nvx_hostname and nvx_machine_id kernel parameters",
        )),
    }
}

/// Decodes a bounded, canonical hostname value, refusing a decoded NUL.
fn parse_hostname(value: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    if value.is_empty()
        || value.len() > 2 * MAX_HOSTNAME_BYTES
        || !value.len().is_multiple_of(2)
        || !value
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(violation(format!(
            "the nvx_hostname kernel parameter must encode 1 to {MAX_HOSTNAME_BYTES} bytes \
             as lowercase hexadecimal"
        )));
    }
    let digit = |byte: u8| {
        if byte.is_ascii_digit() {
            byte - b'0'
        } else {
            byte - b'a' + 10
        }
    };
    let hostname: Vec<u8> = value
        .chunks_exact(2)
        .map(|pair| digit(pair[0]) * 16 + digit(pair[1]))
        .collect();
    if !is_hostname(&hostname) {
        return Err(violation("the nvx_hostname kernel parameter encodes a NUL"));
    }
    Ok(hostname)
}

#[cfg(test)]
mod tests;
