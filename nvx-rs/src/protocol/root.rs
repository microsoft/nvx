//! The boot tokens of a layered root: the kernel command-line tokens through which the host tells
//! the guest which image its layered root assembles, and where the root keeps its upper.
//!
//! A layered root overlays one read-only image, on the `distro` block device, with a writable
//! upper. Two tokens describe it, which [`RootLayout::tokens`] renders:
//!
//! - `nvx_layer=distro,0xd0003000,FORMAT[:IDENTITY]` names the image's format, `erofs` for an
//!   EROFS filesystem or `gpt` for a GPT disk, and the identity that the guest expects of the
//!   image, if any: an EROFS filesystem's UUID, or a GPT disk's GUID. No token names a partition's
//!   GUID.
//! - `nvx_scratch=0xd0006000,ext4` keeps the upper on the `scratch` block device, a whole-device
//!   ext4 filesystem, and `nvx_overlay_upper=ramfs` keeps it in RAM, with no scratch block device.
//!
//! The addresses are the MMIO bases of the two block devices, [`DISTRO_MMIO_BASE`] and
//! [`SCRATCH_MMIO_BASE`], which the microVM fixes for their roles, and identities take the
//! canonical text of a [`Uuid`].
//!
//! Guests read the tokens with [`RootLayout::parse`], which accepts exactly these forms and
//! refuses every other, with no fallback that probes the image or the devices. NVX's shell init
//! reads them with the same rules, and refuses the GPT image, which it cannot assemble. It finds a
//! scratch block device beside a RAM upper among the devices that the kernel added, rather than
//! on the command line.
//!
//! The tokens belong to the boot contract that
//! [`BOOT_CONTRACT_VERSION`](crate::contract::BOOT_CONTRACT_VERSION) versions, so a change to
//! them changes that version too.

use std::fmt;

use super::cmdline::{self, Token};
use super::{ProtocolError, violation};
use crate::uuid::Uuid;

/// The MMIO base of the `distro` block device, which holds a layered root's image.
pub const DISTRO_MMIO_BASE: u64 = 0xd000_3000;

/// The MMIO base of the `scratch` block device, which holds the ext4 upper of a layered root.
pub const SCRATCH_MMIO_BASE: u64 = 0xd000_6000;

/// Prefix of the token that names a layered root's image, `nvx_layer=`.
pub const LAYER_TOKEN: &str = "nvx_layer=";

/// Prefix of the token that keeps a layered root's upper on its scratch block device,
/// `nvx_scratch=`.
pub const SCRATCH_TOKEN: &str = "nvx_scratch=";

/// The token that keeps a layered root's upper in RAM.
pub const RAM_UPPER_TOKEN: &str = "nvx_overlay_upper=ramfs";

/// The name of the kernel parameter that names the image.
const LAYER: &str = "nvx_layer";
/// The name of the kernel parameter that keeps the upper on the scratch block device.
const SCRATCH: &str = "nvx_scratch";
/// The name of the kernel parameter that keeps the upper in RAM.
const OVERLAY_UPPER: &str = "nvx_overlay_upper";
/// The value of the kernel parameter that keeps the upper in RAM.
const RAM_UPPER: &str = "ramfs";
/// The role of the image's block device.
const DISTRO_ROLE: &str = "distro";
/// The filesystem of the scratch block device.
const SCRATCH_FILESYSTEM: &str = "ext4";

/// The format of a layered root's image, which its token names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageFormat {
    /// An EROFS filesystem, whose identity is its filesystem UUID.
    Erofs,
    /// A GPT disk, whose identity is its disk GUID.
    Gpt,
}

impl ImageFormat {
    /// Every format.
    const ALL: [Self; 2] = [Self::Erofs, Self::Gpt];

    /// Returns the format's name in the token: `erofs` or `gpt`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Erofs => "erofs",
            Self::Gpt => "gpt",
        }
    }

    /// Returns the format that `name` names in the token, if any.
    pub fn from_name(name: impl AsRef<[u8]>) -> Option<Self> {
        let name = name.as_ref();
        Self::ALL
            .into_iter()
            .find(|format| format.as_str().as_bytes() == name)
    }

    /// Returns what the format calls the identity of an image.
    const fn identity_name(self) -> &'static str {
        match self {
            Self::Erofs => "EROFS filesystem UUID",
            Self::Gpt => "GPT disk GUID",
        }
    }
}

impl fmt::Display for ImageFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The image of a layered root: its format, and the identity that the guest expects of it, which
/// the guest checks before it uses the image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RootImage {
    /// The image's format.
    pub format: ImageFormat,
    /// The identity that the guest expects of the image, if any: the UUID of an EROFS
    /// filesystem, or the disk GUID of a GPT disk.
    pub identity: Option<Uuid>,
}

impl RootImage {
    /// Returns an EROFS image, with the filesystem UUID that the guest expects, if any.
    pub const fn erofs(identity: Option<Uuid>) -> Self {
        Self {
            format: ImageFormat::Erofs,
            identity,
        }
    }

    /// Returns a GPT image, with the disk GUID that the guest expects, if any.
    pub const fn gpt(identity: Option<Uuid>) -> Self {
        Self {
            format: ImageFormat::Gpt,
            identity,
        }
    }
}

/// Where a layered root keeps the upper of its overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RootUpper {
    /// A whole-device ext4 filesystem on the `scratch` block device.
    Scratch,
    /// RAM, with no `scratch` block device.
    Ram,
}

impl RootUpper {
    /// Returns the upper's name: `scratch` or `ram`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scratch => "scratch",
            Self::Ram => "ram",
        }
    }
}

impl fmt::Display for RootUpper {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The layout of a layered root: its image, on the `distro` block device, and its upper.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RootLayout {
    /// The read-only image.
    pub image: RootImage,
    /// The upper.
    pub upper: RootUpper,
}

impl RootLayout {
    /// Returns the layout's boot tokens: the image's, `nvx_layer=distro,0xd0003000,FORMAT`, with
    /// `:IDENTITY` when the image has an identity, then the upper's, `nvx_scratch=0xd0006000,ext4`
    /// or `nvx_overlay_upper=ramfs`.
    pub fn tokens(&self) -> [String; 2] {
        let RootImage { format, identity } = self.image;
        let layer = format!("{LAYER_TOKEN}{DISTRO_ROLE},{DISTRO_MMIO_BASE:#x},{format}");
        let layer = match identity {
            Some(identity) => format!("{layer}:{identity}"),
            None => layer,
        };
        let upper = match self.upper {
            RootUpper::Scratch => {
                format!("{SCRATCH_TOKEN}{SCRATCH_MMIO_BASE:#x},{SCRATCH_FILESYSTEM}")
            }
            RootUpper::Ram => RAM_UPPER_TOKEN.to_owned(),
        };
        [layer, upper]
    }

    /// Reads the layout of a layered root from the boot tokens of a kernel command line, such as
    /// the guest's `/proc/cmdline`, or returns `None` if the command line has none of them, as
    /// that of a blockless root has none.
    ///
    /// The command line splits at spaces, tabs, and newlines, without quoting, as the guest's init
    /// splits it, with [`cmdline::init_tokens`], and its tokens may come in any order. Each boot
    /// token must take one of the forms of the [module](self), with a value and at most once. The
    /// image needs one upper, and each upper the image. A RAM upper also refuses a scratch block
    /// device: the kernel adds a microVM's virtio-mmio devices from the
    /// `virtio_mmio.device=SIZE@BASE:IRQ` parameters that the VMM appends, and none may sit at
    /// [`SCRATCH_MMIO_BASE`]. The parser reads those parameters as the kernel does, with
    /// [`cmdline::virtio_mmio_devices`], since the kernel splits its command line at more
    /// whitespace than init, honors double quotes, and stops at `--`. Other tokens are ignored, as
    /// are the devices that the layout uses, which the guest resolves itself.
    ///
    /// Fails with a [`ProtocolError`] that names the first token that breaks these rules, or the
    /// tokens that conflict.
    pub fn parse(command_line: &[u8]) -> Result<Option<Self>, ProtocolError> {
        let mut image = None;
        let mut scratch = false;
        let mut ram = false;
        for Token { name, value } in cmdline::init_tokens(command_line) {
            let Some(parameter) = [LAYER, SCRATCH, OVERLAY_UPPER]
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
                LAYER => image.is_some(),
                SCRATCH => scratch,
                _ => ram,
            };
            if seen {
                return Err(violation(format!(
                    "the {parameter} kernel parameter is repeated"
                )));
            }
            match parameter {
                LAYER => image = Some(parse_image(value)?),
                SCRATCH => {
                    let expected = format!("{SCRATCH_MMIO_BASE:#x},{SCRATCH_FILESYSTEM}");
                    if value != expected.as_bytes() {
                        return Err(violation(format!(
                            "the {SCRATCH} kernel parameter must be {expected}, not '{}'",
                            value.escape_ascii()
                        )));
                    }
                    scratch = true;
                }
                _ => {
                    if value != RAM_UPPER.as_bytes() {
                        return Err(violation(format!(
                            "the {OVERLAY_UPPER} kernel parameter must be {RAM_UPPER}, not '{}'",
                            value.escape_ascii()
                        )));
                    }
                    ram = true;
                }
            }
        }
        let upper = match (scratch, ram) {
            (true, true) => {
                return Err(violation(format!(
                    "a layered root has one upper, but the command line names both {SCRATCH} and \
                     {OVERLAY_UPPER}"
                )));
            }
            (false, true) if adds_scratch_device(command_line) => {
                return Err(violation(format!(
                    "a RAM upper takes no scratch block device, but the command line adds a \
                     virtio-mmio device at {SCRATCH_MMIO_BASE:#x}"
                )));
            }
            (true, false) => Some(RootUpper::Scratch),
            (false, true) => Some(RootUpper::Ram),
            (false, false) => None,
        };
        match (image, upper) {
            (Some(image), Some(upper)) => Ok(Some(Self { image, upper })),
            (None, None) => Ok(None),
            (Some(_), None) => Err(violation(format!(
                "the {LAYER} kernel parameter needs an upper, {SCRATCH} or {OVERLAY_UPPER}"
            ))),
            (None, Some(upper)) => {
                let parameter = match upper {
                    RootUpper::Scratch => SCRATCH,
                    RootUpper::Ram => OVERLAY_UPPER,
                };
                Err(violation(format!(
                    "the {parameter} kernel parameter needs an image, which {LAYER} names"
                )))
            }
        }
    }
}

/// Parses the value of the image's token, `distro,0xd0003000,FORMAT[:IDENTITY]`.
fn parse_image(value: &[u8]) -> Result<RootImage, ProtocolError> {
    let fields: Vec<&[u8]> = value.split(|&byte| byte == b',').collect();
    let [role, address, image] = fields[..] else {
        return Err(violation(format!(
            "the {LAYER} kernel parameter must be \
             {DISTRO_ROLE},{DISTRO_MMIO_BASE:#x},FORMAT[:IDENTITY], not '{}'",
            value.escape_ascii()
        )));
    };
    if role != DISTRO_ROLE.as_bytes() {
        return Err(violation(format!(
            "the {LAYER} kernel parameter names the block device '{}', but a layered root has only \
             the {DISTRO_ROLE} one",
            role.escape_ascii()
        )));
    }
    if address != format!("{DISTRO_MMIO_BASE:#x}").as_bytes() {
        return Err(violation(format!(
            "the {LAYER} kernel parameter places the {DISTRO_ROLE} block device at '{}', not at \
             {DISTRO_MMIO_BASE:#x}",
            address.escape_ascii()
        )));
    }
    let (format, identity) = match image.iter().position(|&byte| byte == b':') {
        Some(colon) => (&image[..colon], Some(&image[colon + 1..])),
        None => (image, None),
    };
    let Some(format) = ImageFormat::from_name(format) else {
        // The form without a format, `distro,MMIO,UUID`, names an identity in its place.
        if format.is_empty() || Uuid::parse(format).is_some() {
            return Err(violation(format!(
                "the {LAYER} kernel parameter names no image format, erofs or gpt: '{}'",
                value.escape_ascii()
            )));
        }
        return Err(violation(format!(
            "the {LAYER} kernel parameter names the unsupported image format '{}'",
            format.escape_ascii()
        )));
    };
    let identity = match identity {
        None => None,
        Some(text) => Some(Uuid::parse(text).ok_or_else(|| {
            violation(format!(
                "the {LAYER} kernel parameter names an invalid {}: '{}'",
                format.identity_name(),
                text.escape_ascii()
            ))
        })?),
    };
    Ok(RootImage { format, identity })
}

/// Returns whether a `virtio_mmio.device` parameter of `command_line` describes a virtio-mmio
/// device at [`SCRATCH_MMIO_BASE`], as the kernel reads them.
fn adds_scratch_device(command_line: &[u8]) -> bool {
    cmdline::virtio_mmio_devices(command_line).any(|device| device.base == SCRATCH_MMIO_BASE)
}

#[cfg(test)]
mod tests;
