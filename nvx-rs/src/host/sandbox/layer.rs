//! The read-only layers of a sandbox: EROFS filesystems and GPT disks.

use std::fmt;
use std::path::Path;

use super::distro::DistroFormat;
use super::refused;
use crate::error::Result;
use crate::host::python::path::{Flavor, PurePath};
use crate::uuid::Uuid;

/// The role of a read-only layer of `nvx.py sandbox`.
///
/// A sandbox takes exactly one layer, the distro layer, which its plan attaches as the `distro`
/// [`BlockRole`](super::BlockRole).
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
    /// The image, as `pathlib` normalizes it.
    path: PurePath,
    /// The image's format.
    format: DistroFormat,
    /// The identity that the guest checks before it uses the image: the UUID of an EROFS
    /// filesystem, or the GUID of a GPT disk.
    uuid: String,
}

impl SandboxLayer {
    /// Returns the EROFS layer of `role` at `path` with filesystem `uuid`, which are used as
    /// given, except that `path` is normalized as `pathlib.Path` normalizes it.
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
            format: DistroFormat::Erofs,
            uuid: uuid.into(),
        }
    }

    /// Returns the GPT disk of `role` at `path` with the disk GUID `guid`, which are used as
    /// given, except that `path` is normalized as `pathlib.Path` normalizes it.
    pub fn gpt(role: LayerRole, path: &str, guid: impl Into<String>) -> Self {
        Self::gpt_in(Flavor::NATIVE, role, path, guid)
    }

    /// Implements [`gpt`](Self::gpt) for paths of `flavor`.
    pub(crate) fn gpt_in(
        flavor: Flavor,
        role: LayerRole,
        path: &str,
        guid: impl Into<String>,
    ) -> Self {
        Self {
            format: DistroFormat::Gpt,
            ..Self::new_in(flavor, role, path, guid)
        }
    }

    /// Parses a `--layer` value, `ROLE,PATH,EROFS_UUID`, whose role is one of the
    /// [`LAYER_ROLES`].
    ///
    /// The UUID must be canonical: lowercase hexadecimal digits in hyphenated groups of
    /// 8, 4, 4, 4, and 12. Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
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
        Ok(Self::new_in(flavor, role, path, uuid.to_string()))
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
mod tests {
    use super::*;

    fn message(value: &str) -> String {
        SandboxLayer::parse(value).unwrap_err().to_string()
    }

    #[test]
    fn layers_require_canonical_uuids() {
        let layer =
            SandboxLayer::parse("distro,layers//distro.erofs,22222222-2222-2222-2222-22222222222a")
                .unwrap();
        assert_eq!(layer.role(), LayerRole::Distro);
        assert_eq!(layer.path(), Path::new("layers").join("distro.erofs"));
        assert_eq!(layer.uuid(), "22222222-2222-2222-2222-22222222222a");
        for uuid in [
            "{22222222-2222-2222-2222-22222222222a}",
            "urn:uuid:22222222-2222-2222-2222-22222222222a",
            "2222222222222222222222222222222a",
            "22222222-2222-2222-2222-22222222222A",
        ] {
            assert!(SandboxLayer::parse(&format!("distro,layer.erofs,{uuid}")).is_err());
        }
    }

    #[test]
    fn layers_keep_the_format_of_their_image() {
        let erofs = SandboxLayer::new(
            LayerRole::Distro,
            "l//d.erofs",
            "22222222-2222-2222-2222-222222222222",
        );
        assert_eq!(erofs.format(), DistroFormat::Erofs);
        let gpt = SandboxLayer::gpt(
            LayerRole::Distro,
            "l//d.img",
            "22222222-2222-2222-2222-222222222222",
        );
        assert_eq!(gpt.format(), DistroFormat::Gpt);
        assert_eq!(gpt.path(), Path::new("l").join("d.img"));
        assert_eq!(gpt.uuid(), erofs.uuid());
        assert_eq!(gpt.role(), LayerRole::Distro);
        // `--layer` names EROFS layers.
        assert_eq!(
            SandboxLayer::parse("distro,l/d.erofs,22222222-2222-2222-2222-222222222222")
                .unwrap()
                .format(),
            DistroFormat::Erofs
        );
    }

    #[test]
    fn layer_parser_rejects_invalid_role_and_uuid() {
        assert_eq!(LayerRole::from_name("distro"), Some(LayerRole::Distro));
        for role in ["unknown", "runtime", "custom", "Distro", ""] {
            assert_eq!(
                message(&format!(
                    "{role},layer.erofs,22222222-2222-2222-2222-222222222222"
                )),
                format!("unsupported layer role {role:?}; choose distro")
            );
            assert_eq!(LayerRole::from_name(role), None, "{role}");
        }
        assert_eq!(
            message("distro,layer.erofs,not-a-uuid"),
            "distro layer UUID is invalid: \"not-a-uuid\""
        );
        assert_eq!(message("distro,,1"), "distro layer path is empty");
        for value in ["distro", "distro,a", "distro,a,b,c"] {
            assert_eq!(message(value), "--layer must be ROLE,PATH,EROFS_UUID");
        }
    }
}
