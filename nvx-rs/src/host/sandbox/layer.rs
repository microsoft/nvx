//! The read-only EROFS layers of a sandbox.

use std::fmt;
use std::path::Path;

use super::refused;
use crate::error::Result;
use crate::host::python::builtins::repr;
use crate::host::python::path::{Flavor, PurePath};
use crate::host::python::uuid::parse_uuid;

/// The role of a read-only layer, which fixes its place in the guest's stack of layers and the
/// MMIO base of its block device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LayerRole {
    /// The distribution: the lowest layer.
    Distro,
    /// A language runtime over the distribution.
    Runtime,
    /// The workload's own files, on top.
    Custom,
}

/// The layer roles in the order in which the guest stacks them, bottom first: `LAYER_ROLES`.
pub const LAYER_ROLES: [LayerRole; 3] = [LayerRole::Distro, LayerRole::Runtime, LayerRole::Custom];

impl LayerRole {
    /// Returns the role's name: `distro`, `runtime`, or `custom`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Distro => "distro",
            Self::Runtime => "runtime",
            Self::Custom => "custom",
        }
    }

    /// Returns the role that `name` names, if any.
    pub fn from_name(name: &str) -> Option<Self> {
        LAYER_ROLES.into_iter().find(|role| role.as_str() == name)
    }

    /// Returns the MMIO base of the role's sandbox block device: `BLOCK_MMIO_BASES`.
    pub const fn mmio_base(self) -> u64 {
        match self {
            Self::Distro => 0xD000_3000,
            Self::Runtime => 0xD000_4000,
            Self::Custom => 0xD000_5000,
        }
    }
}

impl fmt::Display for LayerRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A read-only EROFS layer: its role, its host image, and the UUID of its filesystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxLayer {
    /// The layer's role.
    role: LayerRole,
    /// The image, as `pathlib` normalizes it.
    path: PurePath,
    /// The filesystem UUID, which the guest checks before it mounts the layer.
    uuid: String,
}

impl SandboxLayer {
    /// Returns the layer of `role` at `path` with filesystem `uuid`, which are used as given,
    /// except that `path` is normalized as `pathlib.Path` normalizes it.
    pub fn new(role: LayerRole, path: &str, uuid: impl Into<String>) -> Self {
        Self::new_in(Flavor::NATIVE, role, path, uuid)
    }

    /// Implements [`new`](Self::new) for paths of `flavor`.
    pub(crate) fn new_in(
        flavor: Flavor,
        role: LayerRole,
        path: &str,
        uuid: impl Into<String>,
    ) -> Self {
        Self {
            role,
            path: PurePath::new(flavor, path),
            uuid: uuid.into(),
        }
    }

    /// Parses a `--layer` value, `ROLE,PATH,EROFS_UUID`.
    ///
    /// The UUID may take any form that Python's `uuid.UUID` accepts, such as braces, a
    /// `urn:uuid:` prefix, no hyphens, or uppercase digits, and is kept in its lowercase
    /// hyphenated form. Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
    pub fn parse(value: &str) -> Result<Self> {
        Self::parse_in(Flavor::NATIVE, value)
    }

    /// Implements [`parse`](Self::parse) for paths of `flavor`.
    pub(crate) fn parse_in(flavor: Flavor, value: &str) -> Result<Self> {
        let fields: Vec<&str> = value.split(',').collect();
        let [role, path, uuid] = fields[..] else {
            return Err(refused("--layer must be ROLE,PATH,EROFS_UUID"));
        };
        let Some(role) = LayerRole::from_name(role) else {
            return Err(refused(format!(
                "unsupported layer role {}; choose distro, runtime, custom",
                repr(role)
            )));
        };
        if path.is_empty() {
            return Err(refused(format!("{role} layer path is empty")));
        }
        let Some(uuid) = parse_uuid(uuid) else {
            return Err(refused(format!(
                "{role} layer UUID is invalid: {}",
                repr(uuid)
            )));
        };
        Ok(Self::new_in(flavor, role, path, uuid))
    }

    /// Returns the layer's role.
    pub fn role(&self) -> LayerRole {
        self.role
    }

    /// Returns the layer's image.
    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    /// Returns the layer's image as a pure path.
    pub(crate) fn pure_path(&self) -> &PurePath {
        &self.path
    }

    /// Returns the UUID of the layer's filesystem.
    pub fn uuid(&self) -> &str {
        &self.uuid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(value: &str) -> String {
        SandboxLayer::parse(value).unwrap_err().to_string()
    }

    #[test]
    fn layers_parse_with_python_uuids() {
        let layer = SandboxLayer::parse_in(
            Flavor::Posix,
            "custom,layers//custom.erofs,{22222222-2222-2222-2222-22222222222A}",
        )
        .unwrap();
        assert_eq!(layer.role(), LayerRole::Custom);
        assert_eq!(layer.pure_path().as_str(), "layers/custom.erofs");
        assert_eq!(layer.uuid(), "22222222-2222-2222-2222-22222222222a");
        let layer = SandboxLayer::parse_in(
            Flavor::Windows,
            "distro,C:/l/d.erofs,urn:uuid:11111111111111111111111111111111",
        )
        .unwrap();
        assert_eq!(layer.pure_path().as_str(), "C:\\l\\d.erofs");
        assert_eq!(layer.uuid(), "11111111-1111-1111-1111-111111111111");
    }

    #[test]
    fn layer_parser_rejects_invalid_role_and_uuid() {
        assert_eq!(
            message("unknown,layer.erofs,11111111-1111-1111-1111-111111111111"),
            "unsupported layer role 'unknown'; choose distro, runtime, custom"
        );
        assert_eq!(
            message("distro,layer.erofs,not-a-uuid"),
            "distro layer UUID is invalid: 'not-a-uuid'"
        );
        assert_eq!(message("distro,,1"), "distro layer path is empty");
        for value in ["distro", "distro,a", "distro,a,b,c"] {
            assert_eq!(message(value), "--layer must be ROLE,PATH,EROFS_UUID");
        }
    }
}
