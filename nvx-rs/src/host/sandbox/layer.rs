//! The read-only layers of a sandbox: EROFS filesystems and GPT disks.

use std::fmt;
use std::path::{Path, PathBuf};

use super::distro::DistroFormat;
use super::refused;
use crate::error::Result;
use crate::host::paths;
use crate::uuid::Uuid;

/// The role of a read-only layer of `nvx-rs sandbox`.
///
/// A sandbox takes exactly one layer, the distro layer, which OpenVMM attaches as the `distro`
/// sandbox block device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LayerRole {
    /// The distribution, the only layer of a sandbox.
    Distro,
}

/// The layer roles that `--layer` accepts and that a launch holds: `LAYER_ROLES`.
pub const LAYER_ROLES: [LayerRole; 1] = [LayerRole::Distro];

impl LayerRole {
    /// Returns the role's name: `distro`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Distro => "distro",
        }
    }

    /// Returns the role that `name` names, if any, one of the [`LAYER_ROLES`].
    pub fn from_name(name: &str) -> Option<Self> {
        LAYER_ROLES.into_iter().find(|role| role.as_str() == name)
    }
}

impl fmt::Display for LayerRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A read-only layer: its role, its host image, the image's format, and the identity that the
/// guest expects of it, the UUID of an EROFS filesystem or the GUID of a GPT disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxLayer {
    /// The layer's role.
    role: LayerRole,
    /// The image, with its components normalized.
    path: PathBuf,
    /// The image's format.
    format: DistroFormat,
    /// The identity that the guest checks before it uses the image: the UUID of an EROFS
    /// filesystem, or the GUID of a GPT disk.
    uuid: String,
}

impl SandboxLayer {
    /// Returns the EROFS layer of `role` at `path` with filesystem `uuid`, which are used as
    /// given, except that `path` is spelled with its components alone, as
    /// [`Path::components`] reads them.
    pub fn new(role: LayerRole, path: &str, uuid: impl Into<String>) -> Self {
        Self {
            role,
            path: paths::normalized(Path::new(path)),
            format: DistroFormat::Erofs,
            uuid: uuid.into(),
        }
    }

    /// Returns the GPT disk of `role` at `path` with the disk GUID `guid`, which are used as
    /// given, except that `path` is spelled with its components alone, as [`new`](Self::new)
    /// spells it.
    pub fn gpt(role: LayerRole, path: &str, guid: impl Into<String>) -> Self {
        Self {
            format: DistroFormat::Gpt,
            ..Self::new(role, path, guid)
        }
    }

    /// Parses a `--layer` value, `ROLE,PATH,EROFS_UUID`, whose role is one of the
    /// [`LAYER_ROLES`].
    ///
    /// The UUID must be canonical: lowercase hexadecimal digits in hyphenated groups of
    /// 8, 4, 4, 4, and 12. Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
    pub fn parse(value: &str) -> Result<Self> {
        let fields: Vec<&str> = value.split(',').collect();
        let [role, path, uuid] = fields[..] else {
            return Err(refused("--layer must be ROLE,PATH,EROFS_UUID"));
        };
        let Some(role) = LayerRole::from_name(role) else {
            let choices: Vec<&str> = LAYER_ROLES.iter().map(|role| role.as_str()).collect();
            return Err(refused(format!(
                "unsupported layer role {role:?}; choose {}",
                choices.join(", ")
            )));
        };
        if path.is_empty() {
            return Err(refused(format!("{role} layer path is empty")));
        }
        let Some(uuid) = Uuid::parse(uuid) else {
            return Err(refused(format!("{role} layer UUID is invalid: {uuid:?}")));
        };
        Ok(Self::new(role, path, uuid.to_string()))
    }

    /// Returns the layer's role.
    pub fn role(&self) -> LayerRole {
        self.role
    }

    /// Returns the layer's image.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the format of the layer's image.
    pub fn format(&self) -> DistroFormat {
        self.format
    }

    /// Returns the identity that the guest expects of the layer's image: the UUID of an EROFS
    /// filesystem, or the GUID of a GPT disk.
    pub fn uuid(&self) -> &str {
        &self.uuid
    }
}

#[cfg(test)]
mod tests;
