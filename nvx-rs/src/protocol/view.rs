//! A cold boot's view: `nvx_hostname=HEX`, the hostname's bytes as lowercase hexadecimal, two
//! digits per byte, and `nvx_machine_id=image` or `nvx_machine_id=generated`.
//!
//! The hostname holds 1 to [`MAX_HOSTNAME_BYTES`] bytes without a NUL, as [`View`] specifies,
//! without a UTF-8 or DNS restriction. It is always encoded, even for an ASCII hostname:
//! `box` is `nvx_hostname=626f78`, not the legacy sandbox's raw `nvx_hostname=box`.
//! Consumers of this codec use the decoded bytes, not the legacy raw-hostname reader.
//! Restores receive their view through an activation record instead.

use super::cmdline::{self, Token};
use super::{ProtocolError, violation};
use crate::activation::{self, MAX_HOSTNAME_BYTES, MachineIdMode, View};

/// Prefix of the token that encodes the hostname's bytes as lowercase hexadecimal.
pub const HOSTNAME_TOKEN: &str = "nvx_hostname=";
/// Prefix of the token that names the machine-ID mode, `image` or `generated`.
pub const MACHINE_ID_TOKEN: &str = "nvx_machine_id=";

/// Renders the hostname token, then the machine-ID token.
///
/// Fails with a [`ProtocolError`] if the hostname is not 1 to [`MAX_HOSTNAME_BYTES`] non-NUL
/// bytes. Its hexadecimal value needs no quoting or escaping on the kernel command line.
pub fn tokens(view: &View) -> Result<[String; 2], ProtocolError> {
    if !activation::is_hostname(&view.hostname) {
        return Err(violation(format!(
            "the view's hostname must be 1 to {MAX_HOSTNAME_BYTES} bytes without a NUL"
        )));
    }
    let hostname: String = view
        .hostname
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
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
    for Token { name, value } in cmdline::init_tokens(command_line) {
        let parameter = match name {
            b"nvx_hostname" => "nvx_hostname",
            b"nvx_machine_id" => "nvx_machine_id",
            _ => continue,
        };
        let value = value
            .ok_or_else(|| violation(format!("the {parameter} kernel parameter has no value")))?;
        let repeated = if parameter == "nvx_hostname" {
            hostname.is_some()
        } else {
            machine_id.is_some()
        };
        if repeated {
            return Err(violation(format!(
                "the {parameter} kernel parameter is repeated"
            )));
        }
        if parameter == "nvx_hostname" {
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
    if !activation::is_hostname(&hostname) {
        return Err(violation("the nvx_hostname kernel parameter encodes a NUL"));
    }
    Ok(hostname)
}

#[cfg(test)]
mod tests;
