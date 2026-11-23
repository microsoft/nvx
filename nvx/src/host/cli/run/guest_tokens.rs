//! The blockless guest settings owned by the CLI, beside the library's root and view tokens.

use crate::host::filesystem::ExportPlan;
use crate::host::sandbox::{
    has_kernel_separator, parse_named_workload_identity, parse_workload_id,
};
use crate::{Error, Result};

/// Refusal of a workload identity that the caller's tokens name more than once.
const REPEATED_IDENTITY: &str = "a workload identity may be specified only once";

/// Renders both blockless modes and the planned boot binds, keeping other caller tokens.
///
/// The caller's tokens give each of the CLI's settings at most once: the blockless mode, with
/// `nvx_mode=` or `nvx_lifecycle=`, and the workload identity, with `nvx_identity=` or both of
/// `nvx_workload_uid=` and `nvx_workload_gid=`. A repeated one is refused, never resolved.
pub(super) fn render(command_line: &str, export: Option<&ExportPlan>) -> Result<String> {
    let (mut mode, mut uid, mut gid, mut identity) = (None, None, None, None);
    let mut caller = String::new();
    let mut tokens = Vec::new();
    for part in command_line.split_inclusive(|ch: char| ch.is_ascii_whitespace()) {
        let token = part.trim_ascii_end();
        let (name, value) = token.split_once('=').unwrap_or((token, ""));
        let (setting, repeated) = match name {
            "nvx_mode" | "nvx_lifecycle" => {
                (&mut mode, "the blockless mode may be specified only once")
            }
            "nvx_workload_uid" => (&mut uid, REPEATED_IDENTITY),
            "nvx_workload_gid" => (&mut gid, REPEATED_IDENTITY),
            "nvx_identity" => (&mut identity, REPEATED_IDENTITY),
            _ => {
                caller.push_str(part);
                continue;
            }
        };
        if setting.replace(value).is_some() {
            return Err(Error::unsupported(repeated));
        }
        caller.push_str(&part[token.len()..]);
    }
    let mode = mode.unwrap_or("one-shot");
    if !matches!(mode, "one-shot" | "managed") {
        return Err(Error::unsupported(
            "blockless mode must be one-shot or managed",
        ));
    }
    let identity = match (uid, gid, identity) {
        (None, None, None) => None,
        (None, None, Some(identity)) => {
            Some(parse_named_workload_identity("nvx_identity", identity)?)
        }
        (Some(uid), Some(gid), None) => Some((
            parse_workload_id("nvx_workload_uid", uid)?,
            parse_workload_id("nvx_workload_gid", gid)?,
        )),
        (Some(_), Some(_), Some(_)) => return Err(Error::unsupported(REPEATED_IDENTITY)),
        _ => return Err(Error::unsupported("a workload needs both its UID and GID")),
    };
    tokens.push(format!("nvx_mode={mode}"));
    if let Some((uid, gid)) = identity {
        tokens.push(format!("nvx_identity={uid}:{gid}"));
    }
    if let Some(export) = export {
        for (row, bind) in export.guest_table().into_iter().zip(&export.binds) {
            if [&row.source, &row.target]
                .into_iter()
                .any(|text| has_kernel_separator(text) || text.contains([',', '\\', '=']))
            {
                return Err(Error::unsupported(
                    "a planned boot share's source and target must each fit one kernel token",
                ));
            }
            let child = &export.children[bind.child];
            let file = if bind.source.is_empty() {
                child.file
            } else {
                let path = child.root.join(&bind.source);
                std::fs::metadata(&path)
                    .map_err(|error| {
                        Error::io(format!("cannot inspect boot share {}", path.display()))
                            .with_source(error)
                    })?
                    .is_file()
            };
            let access = if row.read_only { "ro" } else { "rw" };
            let kind = if file { ",file" } else { "" };
            tokens.push(format!(
                "nvx_share={},{},{access}{kind}",
                row.source, row.target
            ));
        }
    }
    if !caller.is_empty() {
        tokens.insert(0, caller);
    }
    Ok(tokens.join(" "))
}

#[cfg(test)]
mod tests;
