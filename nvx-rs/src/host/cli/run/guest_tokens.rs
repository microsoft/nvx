//! The blockless guest settings owned by the CLI, beside the library's root and view tokens.

use crate::host::filesystem::ExportPlan;
use crate::host::sandbox::{has_kernel_separator, parse_workload_identity};
use crate::{Error, Result};

/// Renders both blockless modes and the planned boot binds, keeping other caller tokens.
pub(super) fn render(command_line: &str, export: Option<&ExportPlan>) -> Result<String> {
    let mut mode = "one-shot";
    let (mut uid, mut gid, mut identity) = (None, None, None);
    let mut caller = String::new();
    let mut tokens = Vec::new();
    for part in command_line.split_inclusive(|ch: char| ch.is_ascii_whitespace()) {
        let token = part.trim_ascii_end();
        let (name, value) = token.split_once('=').unwrap_or((token, ""));
        match name {
            "nvx_mode" | "nvx_lifecycle" => mode = value,
            "nvx_workload_uid" => uid = Some(value),
            "nvx_workload_gid" => gid = Some(value),
            "nvx_identity" => identity = Some(value.to_owned()),
            _ => {
                caller.push_str(part);
                continue;
            }
        }
        caller.push_str(&part[token.len()..]);
    }
    if !matches!(mode, "one-shot" | "managed") {
        return Err(Error::unsupported(
            "blockless mode must be one-shot or managed",
        ));
    }
    if uid.is_some() || gid.is_some() {
        let (Some(uid), Some(gid)) = (uid, gid) else {
            return Err(Error::unsupported("a workload needs both its UID and GID"));
        };
        if identity.is_some() {
            return Err(Error::unsupported(
                "a workload identity may be specified only once",
            ));
        }
        identity = Some(format!("{uid}:{gid}"));
    }
    tokens.push(format!("nvx_mode={mode}"));
    if let Some(identity) = identity {
        let (uid, gid) = parse_workload_identity(&identity)?;
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
