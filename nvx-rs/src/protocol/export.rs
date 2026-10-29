//! The boot tokens of the host export: the kernel command-line tokens through which OpenVMM tells
//! the guest of the aggregate virtio-fs export from which the guest maps host paths, and the
//! guest's strict parser of them.
//!
//! A host that maps host paths into the guest through a mapping table, as NVX's managed lifecycle
//! does, exports them through OpenVMM's one virtio-fs device as an aggregate: a read-only root that
//! lists one child per exported host directory or file, which the guest mounts out of its
//! workloads' reach, and from which it maps each entry of the table at its target. The host's plan
//! exports the aggregate at [`EXPORT_DIRECTORY`], and OpenVMM appends four tokens for it to the
//! kernel command line:
//!
//! - `virtfs_dir=/run/nvx/hostfs/root`, the guest directory of the export, [`EXPORT_DIRECTORY`];
//! - `virtfs_tag=TAG`, the virtio-fs tag of the export's device, a nonempty name of letters,
//!   digits, `_`, and `-`;
//! - `virtfs_mode=ro`, or `virtfs_mode=rw` when a child is read-write, the [`ExportMode`] of the
//!   guest's mount;
//! - and `virtfs_aggregate=1`, which tells the aggregate's root, which lists the children, from a
//!   single shared directory.
//!
//! Guests read them with [`HostExport::parse`], which accepts exactly these forms, all four tokens
//! or none, each once. It applies the checks with which NVX's shell init mounts the export of a
//! managed sandbox, and refuses `nvx_share=` tokens as that init does: NVX's one-shot lifecycle
//! binds the children of an aggregate as live shares with them, while a guest that mounts the
//! export maps host paths from its mapping table alone.
//!
//! Like the rest of [`protocol`](super), it needs nothing outside the standard library, so the
//! host, the guest, and test doubles share it.

use std::fmt;

use super::cmdline::{self, Token};
use super::{ProtocolError, violation};

/// The guest directory of the host export, the value of its `virtfs_dir=` token: the directory
/// where the host's plan exports the aggregate, and where NVX's guests mount it.
pub const EXPORT_DIRECTORY: &str = "/run/nvx/hostfs/root";

/// The name of the kernel parameter that names the export's guest directory.
const DIRECTORY: &str = "virtfs_dir";
/// The name of the kernel parameter that names the virtio-fs tag of the export's device.
const TAG: &str = "virtfs_tag";
/// The name of the kernel parameter that names the mode of the guest's mount of the export.
const MODE: &str = "virtfs_mode";
/// The name of the kernel parameter that marks the export as an aggregate.
const AGGREGATE: &str = "virtfs_aggregate";
/// The value of the kernel parameter that marks the export as an aggregate.
const AGGREGATE_VALUE: &str = "1";
/// The name of the kernel parameter with which NVX's one-shot lifecycle binds a child of an
/// aggregate as a live share.
const SHARE: &str = "nvx_share";
/// The names of the export's kernel parameters, in the order in which OpenVMM appends them.
const PARAMETERS: [&str; 4] = [DIRECTORY, TAG, MODE, AGGREGATE];

/// The mode in which the guest mounts the host export, which its `virtfs_mode=` token names.
///
/// OpenVMM limits each child's writes to that child's writable parts, so the mode only lets the
/// guest's mount carry the read-write children: it is never the boundary of a child's access.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExportMode {
    /// `ro`: every child of the export is read-only.
    ReadOnly,
    /// `rw`: a child of the export is read-write.
    ReadWrite,
}

impl ExportMode {
    /// Every mode.
    const ALL: [Self; 2] = [Self::ReadOnly, Self::ReadWrite];

    /// Returns the mode's value in the token: `ro` or `rw`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "ro",
            Self::ReadWrite => "rw",
        }
    }

    /// Returns the mode that `value` names in the token, if any.
    fn from_value(value: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.as_str().as_bytes() == value)
    }
}

impl fmt::Display for ExportMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The host export that the boot tokens of a kernel command line describe: the device that the
/// guest mounts, and how. The export's guest directory is always [`EXPORT_DIRECTORY`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostExport {
    /// The virtio-fs tag of the export's device: a nonempty name of letters, digits, `_`, and `-`.
    pub tag: String,
    /// The mode in which the guest mounts the export.
    pub mode: ExportMode,
}

impl HostExport {
    /// Reads the host export from the boot tokens of a kernel command line, such as the guest's
    /// `/proc/cmdline`, or returns `None` if the command line has none of them, as that of a guest
    /// that the host maps no paths into has none.
    ///
    /// The command line splits at spaces, tabs, and newlines, without quoting, as the guest's init
    /// splits it, with [`cmdline::init_tokens`], and its tokens may come in any order. Each of the
    /// export's tokens must take its form of the [module](self), with a value and at most once, and
    /// the command line must hold all four or none: `virtfs_dir=` [`EXPORT_DIRECTORY`], which a
    /// single shared directory or the aggregate of NVX's one-shot live shares names another way,
    /// `virtfs_tag=` a nonempty name of letters, digits, `_`, and `-`, `virtfs_mode=` `ro` or `rw`,
    /// and `virtfs_aggregate=1`. An `nvx_share=` token is refused too, whether or not the export's
    /// tokens are there: it binds a child of an aggregate as a live share, which the guest of an
    /// export never does. Other tokens are ignored.
    ///
    /// Fails with a [`ProtocolError`] that names the first token that breaks these rules, or the
    /// tokens that an incomplete export lacks.
    pub fn parse(command_line: &[u8]) -> Result<Option<Self>, ProtocolError> {
        let mut directory = false;
        let mut tag = None;
        let mut mode = None;
        let mut aggregate = false;
        for Token { name, value } in cmdline::init_tokens(command_line) {
            if name == SHARE.as_bytes() && value.is_some() {
                return Err(violation(format!(
                    "the {SHARE} kernel parameter binds a live share, but a guest that mounts the \
                     host export maps host paths from its mapping table alone"
                )));
            }
            let Some(parameter) = PARAMETERS
                .into_iter()
                .find(|parameter| parameter.as_bytes() == name)
            else {
                continue;
            };
            let Some(value) = value else {
                return Err(violation(format!(
                    "the {parameter} kernel parameter has no value"
                )));
            };
            let seen = match parameter {
                DIRECTORY => directory,
                TAG => tag.is_some(),
                MODE => mode.is_some(),
                _ => aggregate,
            };
            if seen {
                return Err(violation(format!(
                    "the {parameter} kernel parameter is repeated"
                )));
            }
            match parameter {
                DIRECTORY => {
                    if value != EXPORT_DIRECTORY.as_bytes() {
                        return Err(violation(format!(
                            "the {DIRECTORY} kernel parameter must be {EXPORT_DIRECTORY}, not '{}'",
                            value.escape_ascii()
                        )));
                    }
                    directory = true;
                }
                TAG => tag = Some(parse_tag(value)?),
                MODE => {
                    mode = Some(ExportMode::from_value(value).ok_or_else(|| {
                        violation(format!(
                            "the {MODE} kernel parameter must be ro or rw, not '{}'",
                            value.escape_ascii()
                        ))
                    })?);
                }
                _ => {
                    if value != AGGREGATE_VALUE.as_bytes() {
                        return Err(violation(format!(
                            "the {AGGREGATE} kernel parameter must be {AGGREGATE_VALUE}, not '{}'",
                            value.escape_ascii()
                        )));
                    }
                    aggregate = true;
                }
            }
        }
        match (directory, tag, mode, aggregate) {
            (true, Some(tag), Some(mode), true) => Ok(Some(Self { tag, mode })),
            (false, None, None, false) => Ok(None),
            (directory, tag, mode, aggregate) => {
                let present = [directory, tag.is_some(), mode.is_some(), aggregate];
                let missing: Vec<&str> = PARAMETERS
                    .into_iter()
                    .zip(present)
                    .filter_map(|(parameter, present)| (!present).then_some(parameter))
                    .collect();
                Err(violation(format!(
                    "the host export is incomplete: the command line has no {} kernel parameter",
                    alternatives(&missing)
                )))
            }
        }
    }
}

/// Parses the value of the tag's token, a nonempty name of letters, digits, `_`, and `-`.
fn parse_tag(value: &[u8]) -> Result<String, ProtocolError> {
    if value.is_empty()
        || !value
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(violation(format!(
            "the {TAG} kernel parameter must be a tag of letters, digits, _, and -, not '{}'",
            value.escape_ascii()
        )));
    }
    // The bytes are ASCII.
    Ok(value.iter().copied().map(char::from).collect())
}

/// Returns `names` as alternatives in a sentence: `a`, `a or b`, or `a, b, or c`.
fn alternatives(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [name] => (*name).to_owned(),
        [first, second] => format!("{first} or {second}"),
        [rest @ .., last] => format!("{}, or {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests;
