//! The boot tokens of a layered root: the kernel command-line tokens through which the host tells
//! the guest which image its layered root assembles, where the root keeps its upper, and how the
//! guest assembles them.
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
//! Four more tokens each name a setting of the root, after those two and in this order, when the
//! layout names it. Without one, the guest assembles the root with its own default, as it does for
//! a command line that predates them:
//!
//! - `nvx_overlay_mode=trusted-lowers` or `nvx_overlay_mode=untrusted-lowers` names the overlay's
//!   [`OverlayMode`]; the guest's default trusts the lowers.
//! - `nvx_scratch_journal=allowed` or `nvx_scratch_journal=forbidden` names whether the ext4
//!   filesystem of a scratch upper may have a journal, its [`Journal`] policy; the guest's default
//!   allows one.
//! - `nvx_first_partition_bound=BYTES` bounds the bytes of a GPT disk's first partition.
//! - `nvx_ram_upper_percent=PERCENT` caps a RAM upper at PERCENT percent of the VM's memory, from 1
//!   to 90; the guest's default is the kernel's default size of a tmpfs, half of its memory.
//!
//! A number is decimal, at least 1, without a sign or leading zeros. The journal policy needs a
//! scratch upper, the cap a RAM upper, and the bound a GPT disk.
//!
//! Guests read the tokens with [`RootLayout::parse`], which accepts exactly these forms and
//! refuses every other, with no fallback that probes the image or the devices.
//!
//! The tokens belong to the boot contract that
//! [`BOOT_CONTRACT_VERSION`](crate::contract::BOOT_CONTRACT_VERSION) versions, so a change to
//! them changes that version too.

use std::fmt;
use std::ops::RangeInclusive;

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

/// Prefix of the token that names the overlay's mode, `nvx_overlay_mode=`.
pub const OVERLAY_MODE_TOKEN: &str = "nvx_overlay_mode=";

/// Prefix of the token that names whether a scratch upper may have a journal,
/// `nvx_scratch_journal=`.
pub const SCRATCH_JOURNAL_TOKEN: &str = "nvx_scratch_journal=";

/// Prefix of the token that bounds the bytes of a GPT disk's first partition,
/// `nvx_first_partition_bound=`.
pub const FIRST_PARTITION_BOUND_TOKEN: &str = "nvx_first_partition_bound=";

/// Prefix of the token that caps a RAM upper, in percent of the VM's memory,
/// `nvx_ram_upper_percent=`.
pub const RAM_UPPER_PERCENT_TOKEN: &str = "nvx_ram_upper_percent=";

/// The caps of a RAM upper that the tokens carry, in percent of the VM's memory.
pub(crate) const RAM_UPPER_CAPS: RangeInclusive<u8> = 1..=90;

/// The name of the kernel parameter that names the image.
const LAYER: &str = "nvx_layer";
/// The name of the kernel parameter that keeps the upper on the scratch block device.
const SCRATCH: &str = "nvx_scratch";
/// The name of the kernel parameter that keeps the upper in RAM.
const OVERLAY_UPPER: &str = "nvx_overlay_upper";
/// The name of the kernel parameter that names the overlay's mode.
const OVERLAY_MODE: &str = "nvx_overlay_mode";
/// The name of the kernel parameter that names the scratch's journal policy.
const SCRATCH_JOURNAL: &str = "nvx_scratch_journal";
/// The name of the kernel parameter that bounds a GPT disk's first partition.
const FIRST_PARTITION_BOUND: &str = "nvx_first_partition_bound";
/// The name of the kernel parameter that caps a RAM upper.
const RAM_UPPER_PERCENT: &str = "nvx_ram_upper_percent";
/// The names of the kernel parameters of the boot tokens.
const PARAMETERS: [&str; 7] = [
    LAYER,
    SCRATCH,
    OVERLAY_UPPER,
    OVERLAY_MODE,
    SCRATCH_JOURNAL,
    FIRST_PARTITION_BOUND,
    RAM_UPPER_PERCENT,
];
/// The value of the kernel parameter that keeps the upper in RAM.
const RAM_UPPER: &str = "ramfs";
/// The role of the image's block device.
const DISTRO_ROLE: &str = "distro";
/// The filesystem of the scratch block device.
const SCRATCH_FILESYSTEM: &str = "ext4";
/// Every overlay mode.
const OVERLAY_MODES: [OverlayMode; 2] = [OverlayMode::TrustedLowers, OverlayMode::UntrustedLowers];
/// Every journal policy.
const JOURNALS: [Journal; 2] = [Journal::Allowed, Journal::Forbidden];

/// The overlay's mode, which selects its one option set, as the guest's assembly names it, at a
/// cold boot and at a restore alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OverlayMode {
    /// `metacopy=on,xino=on`, for lowers whose overlay xattrs the guest trusts.
    TrustedLowers,
    /// `metacopy=off,redirect_dir=nofollow`, for lowers that may carry untrusted overlay xattrs.
    UntrustedLowers,
}

/// Whether an ext4 scratch may have a journal, at a cold boot and at a restore alike: the guest's
/// assembly refuses a scratch with one when the policy [forbids](Self::Forbidden) it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Journal {
    /// The scratch may have a journal or none.
    Allowed,
    /// The scratch may not have a journal.
    Forbidden,
}

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

/// The layout of a layered root: its image, on the `distro` block device, its upper, and the
/// settings of its assembly that it names. A setting that it leaves `None` has no token, and the
/// guest assembles the root with its own default for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RootLayout {
    /// The read-only image.
    pub image: RootImage,
    /// The upper.
    pub upper: RootUpper,
    /// The overlay's mode.
    pub overlay: Option<OverlayMode>,
    /// Whether the ext4 filesystem of a [scratch](RootUpper::Scratch) upper may have a journal.
    pub journal: Option<Journal>,
    /// The bound of the bytes of a [GPT](ImageFormat::Gpt) disk's first partition, at least 1.
    pub gpt_first_partition_bound: Option<u64>,
    /// The cap of a [RAM](RootUpper::Ram) upper, in percent of the VM's memory, from 1 to 90.
    pub max_memory_percent: Option<u8>,
}

impl RootLayout {
    /// Returns the layout's boot tokens: the image's, `nvx_layer=distro,0xd0003000,FORMAT`, with
    /// `:IDENTITY` when the image has an identity, then the upper's, `nvx_scratch=0xd0006000,ext4`
    /// or `nvx_overlay_upper=ramfs`, then the token of each setting that the layout names, in the
    /// order of the [module](self). [`parse`](Self::parse) refuses the tokens of a setting that
    /// breaks its rules.
    pub fn tokens(&self) -> Vec<String> {
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
        let settings = [
            self.overlay
                .map(|mode| format!("{OVERLAY_MODE_TOKEN}{}", overlay_name(mode))),
            self.journal
                .map(|journal| format!("{SCRATCH_JOURNAL_TOKEN}{}", journal_name(journal))),
            self.gpt_first_partition_bound
                .map(|bound| format!("{FIRST_PARTITION_BOUND_TOKEN}{bound}")),
            self.max_memory_percent
                .map(|percent| format!("{RAM_UPPER_PERCENT_TOKEN}{percent}")),
        ];
        let mut tokens = vec![layer, upper];
        tokens.extend(settings.into_iter().flatten());
        tokens
    }

    /// Reads the layout of a layered root from the boot tokens of a kernel command line, such as
    /// the guest's `/proc/cmdline`, or returns `None` if the command line has none of them, as
    /// that of a blockless root has none.
    ///
    /// The command line splits at spaces, tabs, and newlines, without quoting, as the guest's init
    /// splits it, with [`cmdline::init_tokens`], and its tokens may come in any order. Each boot
    /// token must take one of the forms of the [module](self), with a value and at most once. The
    /// image needs one upper, each upper and setting the image, and each setting the upper or the
    /// format that it sets. A RAM upper also refuses a scratch block device: the kernel adds a
    /// microVM's virtio-mmio devices from the `virtio_mmio.device=SIZE@BASE:IRQ` parameters that
    /// the VMM appends, and none may sit at [`SCRATCH_MMIO_BASE`]. The parser reads those
    /// parameters as the kernel does, with [`cmdline::virtio_mmio_devices`], since the kernel
    /// splits its command line at more whitespace than init, honors double quotes, and stops at
    /// `--`. Other tokens are ignored, as are the devices that the layout uses, which the guest
    /// resolves itself.
    ///
    /// Fails with a [`ProtocolError`] that names the first token that breaks these rules, or the
    /// tokens that conflict.
    pub fn parse(command_line: &[u8]) -> Result<Option<Self>, ProtocolError> {
        let mut image = None;
        let mut scratch = false;
        let mut ram = false;
        let mut overlay = None;
        let mut journal = None;
        let mut gpt_first_partition_bound = None;
        let mut max_memory_percent = None;
        // The parameters that the command line names, in its order.
        let mut seen = Vec::new();
        for Token { name, value } in cmdline::init_tokens(command_line) {
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
            if seen.contains(&parameter) {
                return Err(violation(format!(
                    "the {parameter} kernel parameter is repeated"
                )));
            }
            seen.push(parameter);
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
                OVERLAY_UPPER => {
                    if value != RAM_UPPER.as_bytes() {
                        return Err(violation(format!(
                            "the {OVERLAY_UPPER} kernel parameter must be {RAM_UPPER}, not '{}'",
                            value.escape_ascii()
                        )));
                    }
                    ram = true;
                }
                OVERLAY_MODE => {
                    overlay = Some(named(parameter, value, OVERLAY_MODES, overlay_name)?)
                }
                SCRATCH_JOURNAL => journal = Some(named(parameter, value, JOURNALS, journal_name)?),
                FIRST_PARTITION_BOUND => {
                    gpt_first_partition_bound = Some(number(parameter, value, 1..=u64::MAX)?);
                }
                _ => max_memory_percent = Some(number(parameter, value, RAM_UPPER_CAPS)?),
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
        // Without an image, the parameters that the command line names are those of an upper or
        // of settings.
        let Some(image) = image else {
            return match seen.first() {
                Some(parameter) => Err(violation(format!(
                    "the {parameter} kernel parameter needs an image, which {LAYER} names"
                ))),
                None => Ok(None),
            };
        };
        let Some(upper) = upper else {
            return Err(violation(format!(
                "the {LAYER} kernel parameter needs an upper, {SCRATCH} or {OVERLAY_UPPER}"
            )));
        };
        let needs = if journal.is_some() && upper != RootUpper::Scratch {
            Some((SCRATCH_JOURNAL, "a scratch upper, nvx_scratch"))
        } else if max_memory_percent.is_some() && upper != RootUpper::Ram {
            Some((RAM_UPPER_PERCENT, "a RAM upper, nvx_overlay_upper"))
        } else if gpt_first_partition_bound.is_some() && image.format != ImageFormat::Gpt {
            Some((FIRST_PARTITION_BOUND, "a GPT image"))
        } else {
            None
        };
        if let Some((parameter, needs)) = needs {
            return Err(violation(format!(
                "the {parameter} kernel parameter needs {needs}"
            )));
        }
        Ok(Some(Self {
            image,
            upper,
            overlay,
            journal,
            gpt_first_partition_bound,
            max_memory_percent,
        }))
    }
}

/// Returns the name of `mode` in the overlay mode's token.
const fn overlay_name(mode: OverlayMode) -> &'static str {
    match mode {
        OverlayMode::TrustedLowers => "trusted-lowers",
        OverlayMode::UntrustedLowers => "untrusted-lowers",
    }
}

/// Returns the name of `journal` in the journal policy's token.
const fn journal_name(journal: Journal) -> &'static str {
    match journal {
        Journal::Allowed => "allowed",
        Journal::Forbidden => "forbidden",
    }
}

/// Parses the value of the token of `parameter`, a setting of which `name` names each of
/// `settings`.
fn named<T: Copy>(
    parameter: &str,
    value: &[u8],
    settings: [T; 2],
    name: fn(T) -> &'static str,
) -> Result<T, ProtocolError> {
    let [first, second] = settings.map(name);
    settings
        .into_iter()
        .find(|&setting| name(setting).as_bytes() == value)
        .ok_or_else(|| {
            violation(format!(
                "the {parameter} kernel parameter must be {first} or {second}, not '{}'",
                value.escape_ascii()
            ))
        })
}

/// Parses the value of the token of `parameter`, a decimal number in `range`, without a sign or
/// leading zeros.
fn number<T>(parameter: &str, value: &[u8], range: RangeInclusive<T>) -> Result<T, ProtocolError>
where
    T: std::str::FromStr + PartialOrd + fmt::Display,
{
    // `str::parse` takes a leading `+` too.
    std::str::from_utf8(value)
        .ok()
        .filter(|text| !text.starts_with(['0', '+']))
        .and_then(|text| text.parse().ok())
        .filter(|number| range.contains(number))
        .ok_or_else(|| {
            violation(format!(
                "the {parameter} kernel parameter must be a decimal number from {} to {}, \
                 without a sign or leading zeros, not '{}'",
                range.start(),
                range.end(),
                value.escape_ascii()
            ))
        })
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
