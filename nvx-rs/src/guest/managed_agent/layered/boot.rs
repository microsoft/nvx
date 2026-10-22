//! The kernel command line of a managed sandbox's layered root: the tokens that the host program
//! writes and that the VMM appends, which the layered root's PID 1 reads before it changes
//! anything, with the checks and messages of NVX's shell init.

use std::ffi::CString;

use crate::guest::parse::parse_decimal;
use crate::guest::root::Resources;
use crate::protocol::ProtocolError;
use crate::protocol::root::{ImageFormat, RAM_UPPER_TOKEN, RootLayout, RootUpper};
use crate::protocol::share::Shares;
use crate::uuid::Uuid;

/// The agent's `memory.low`, in bytes, when the command line names none: 16 MiB.
const AGENT_MEMORY_LOW: u64 = 16 << 20;
/// The sandbox's hostname when the command line names none.
const HOSTNAME: &[u8] = b"nvx-sandbox";
/// The dedicated control terminal, which the managed lifecycle requires.
const CONTROL_TTY: &[u8] = b"hvc2";

/// What the kernel command line asks of the layered root.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Boot<'a> {
    /// The resource values of the agent's and the workload cgroups.
    pub(super) resources: Resources,
    /// The UUID that the EROFS image must have, if the command line names one.
    pub(super) image_uuid: Option<Uuid>,
    /// The size that the image's device must have, in 512-byte sectors, if the command line
    /// names one.
    pub(super) layer_sectors: Option<u64>,
    /// The sandbox's hostname.
    pub(super) hostname: CString,
    /// The workload's user ID, in the decimal text of the command line.
    pub(super) uid: Vec<u8>,
    /// The workload's group ID, in the decimal text of the command line.
    pub(super) gid: Vec<u8>,
    /// The live shares that the VMM exports, which the agent mounts in the root.
    pub(super) shares: Shares<'a>,
}

impl<'a> Boot<'a> {
    /// Reads `command_line`, the guest's `/proc/cmdline`. Its tokens split at spaces, tabs, and
    /// newlines, and the first of a repeated token counts, except the root's boot tokens, which
    /// [`RootLayout::parse`] reads and which may not repeat, and the live shares, which
    /// [`Shares::parse`] reads.
    ///
    /// Returns the message of the first check that fails, in the shell init's order: the
    /// resource values, the root's boot tokens, which must name an EROFS image over the scratch,
    /// the entrypoint, the workload's identity, the control terminal, and the live shares.
    pub(super) fn parse(command_line: &'a [u8]) -> Result<Self, String> {
        let tokens: Vec<&[u8]> = command_line
            .split(|byte| matches!(byte, b' ' | b'\t' | b'\n'))
            .filter(|token| !token.is_empty())
            .collect();
        let value = |name: &str| first_value(&tokens, name);
        let resources = Resources {
            agent_memory_low: Some(
                decimal(
                    value("nvx_agent_memory_low"),
                    "invalid agent memory reservation",
                )?
                .unwrap_or(AGENT_MEMORY_LOW),
            ),
            workload_memory_max: decimal(
                value("nvx_memory_max"),
                "invalid container memory limit",
            )?,
            workload_pids_max: decimal(value("nvx_pids_max"), "invalid container process limit")?,
        };
        let layout = match RootLayout::parse(command_line) {
            Ok(Some(layout)) => layout,
            Ok(None) => {
                return Err("the command line names no distro layer: this init needs \
                            nvx_layer=distro,0xd0003000,erofs[:UUID]"
                    .into());
            }
            Err(error) => return Err(error.into_message()),
        };
        if layout.image.format != ImageFormat::Erofs {
            let [layer, _] = layout.tokens();
            return Err(format!(
                "{layer} names a GPT image, which this init cannot assemble: it assembles only an \
                 EROFS image over a scratch disk"
            ));
        }
        if layout.upper != RootUpper::Scratch {
            return Err(format!(
                "{RAM_UPPER_TOKEN} asks for a RAM upper, which this init cannot assemble: it \
                 assembles only an EROFS image over a scratch disk"
            ));
        }
        let layer_sectors = match value("nvx_layer_capacity_sectors") {
            None | Some(b"") => None,
            Some(text) => Some(parse_decimal(text).ok_or_else(|| {
                format!(
                    "the nvx_layer_capacity_sectors kernel parameter must be a decimal number of \
                     sectors, not '{}'",
                    text.escape_ascii()
                )
            })?),
        };
        // The managed lifecycle runs no entrypoint, but the shell init checked it for both.
        if value("nvx_entrypoint")
            .is_some_and(|entrypoint| !entrypoint.is_empty() && !entrypoint.starts_with(b"/"))
        {
            return Err("entrypoint must be an absolute path".into());
        }
        let hostname = match value("nvx_hostname") {
            None | Some(b"") => HOSTNAME,
            Some(hostname) => hostname,
        };
        let uid = identity(value("nvx_workload_uid"), "workload UID is unavailable")?;
        let gid = identity(value("nvx_workload_gid"), "workload GID is unavailable")?;
        if value("nvx_control_tty") != Some(CONTROL_TTY) {
            return Err("managed lifecycle requires the dedicated control tty".into());
        }
        Ok(Self {
            resources,
            image_uuid: layout.image.identity,
            layer_sectors,
            // The kernel separates the command line's arguments with spaces, so none holds a NUL.
            hostname: CString::new(hostname).map_err(|_| "the hostname holds a NUL")?,
            uid,
            gid,
            shares: Shares::parse(command_line).map_err(ProtocolError::into_message)?,
        })
    }
}

/// Returns the value of the first token `name=VALUE` among `tokens`.
fn first_value<'a>(tokens: &[&'a [u8]], name: &str) -> Option<&'a [u8]> {
    tokens.iter().find_map(|token| {
        token
            .strip_prefix(name.as_bytes())
            .and_then(|rest| rest.strip_prefix(b"="))
    })
}

/// Parses an optional decimal value, which an empty token leaves out, or fails with `message`.
fn decimal(value: Option<&[u8]>, message: &str) -> Result<Option<u64>, String> {
    match value {
        None | Some(b"") => Ok(None),
        Some(text) => parse_decimal(text).map(Some).ok_or_else(|| message.into()),
    }
}

/// Returns the decimal text of a workload's user or group ID, which may be neither empty nor
/// `0`, or fails with `message`.
fn identity(value: Option<&[u8]>, message: &str) -> Result<Vec<u8>, String> {
    match value {
        Some(text) if !text.is_empty() && text != b"0" && text.iter().all(u8::is_ascii_digit) => {
            Ok(text.to_vec())
        }
        _ => Err(message.into()),
    }
}
