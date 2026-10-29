//! The kernel command line as Linux and the guest's init read it: the parameters into which the
//! kernel splits it, the virtio-mmio devices that its `virtio_mmio.device` parameters describe,
//! and the tokens into which the guest's init splits it; and the guest's DNS server and the IPv4
//! gateway of its NIC, which OpenVMM names in it.
//!
//! A reader that splits a command line by rules of its own may find parameters that the kernel
//! never sees, or miss ones that it applies: the kernel splits at more bytes than ASCII
//! whitespace, honors double quotes, and stops at `--`. This module reads a command line by the
//! rules of Linux 6.18, the kernel of NVX's guests, for the readers that need the kernel's view of
//! it:
//!
//! - [`parameters`] splits a command line as the kernel's `parse_args` and `next_arg` split it,
//!   at the bytes that [`is_space`] accepts, and [`Parameter::is_named`] compares a parameter's
//!   name as `parameq` does.
//! - [`VirtioMmioDevice::parse`] reads the value of a `virtio_mmio.device` parameter as the
//!   virtio-mmio driver's `vm_cmdline_set` reads it, for a device of any type, and
//!   [`virtio_mmio_devices`] reads those of every such parameter of a command line.
//!
//! NVX's guest init reads its own boot tokens, such as those of a layered root and of the host
//! export, by other rules: its shell splits the command line at spaces, tabs, and newlines alone,
//! without quoting and past a `--`, and matches a token by its name, up to its first `=`, byte for
//! byte. This module reads a command line by those rules too, for the readers of those tokens,
//! which must read them as the init does:
//!
//! - [`init_tokens`] splits a command line into [`Token`]s as the init's shell splits it.
//!   [`HostExport::parse`](super::export::HostExport::parse),
//!   [`RootLayout::parse`](super::root::RootLayout::parse), and
//!   [`parse_host_mapping_count`](super::parse_host_mapping_count) read their tokens with it, and
//!   so do the time ABI's check of report-only mode, in `nvx::guest::time`, and the sandbox plans'
//!   check of the user's kernel parameters, in `nvx::host::sandbox`, which splits them both as the
//!   kernel and as the init does, and compares their names as the kernel does.
//!
//! The kernel leaves OpenVMM's `virtnet_dns=` and `virtnet_gw=` tokens to the guest, which reads
//! them from the whole command line, as OpenVMM checks them:
//!
//! - [`dns_server`] reads the DNS server that the `virtnet_dns=` token names, strictly, as OpenVMM
//!   renders and reads it: at most one token, with one IPv4 or IPv6 address.
//! - [`write_resolver`] writes the resolver file that names that server, as NVX's init writes the
//!   initramfs's `/etc/resolv.conf` from the token.
//! - [`gateway`] reads the IPv4 gateway that the `virtnet_gw=` token names, as strictly: at most
//!   one token, with one IPv4 address.
//!
//! Like the rest of [`protocol`](super), it needs nothing outside the standard library, so the
//! host, the guest, and test doubles share it.

use std::fmt::Display;
use std::io;
use std::iter::FusedIterator;
use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;

use super::{ProtocolError, violation};

/// The name of the kernel parameter that adds a virtio-mmio device.
pub const VIRTIO_MMIO_DEVICE: &str = "virtio_mmio.device";

/// Prefix of the token through which OpenVMM names the guest's DNS server, `virtnet_dns=`: one of
/// the gateway's addresses, IPv4 or IPv6, which it adds to a microVM's command line when the
/// network policy lets the guest reach the gateway's DNS service.
pub const DNS_TOKEN: &str = "virtnet_dns=";

/// Prefix of the token through which OpenVMM names the IPv4 gateway of the guest's NIC,
/// `virtnet_gw=`, which it adds to a microVM's command line with the NIC's other tokens: the
/// gateway's address, to which the guest's announcer of its listeners sends its announcements.
pub const GATEWAY_TOKEN: &str = "virtnet_gw=";

/// Returns whether the kernel takes `byte` for whitespace, as its `isspace` does: the space, tab,
/// newline, vertical tab, form feed, carriage return, and Latin-1's no-break space, 0xa0.
pub const fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | 0xa0)
}

/// A parameter of a kernel command line, as the kernel splits the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Parameter<'a> {
    /// The name: the parameter's text up to its first `=` after its first byte, or all of it.
    pub name: &'a [u8],
    /// The value: the text after that `=`, or `None` if the parameter has none.
    pub value: Option<&'a [u8]>,
}

impl Parameter<'_> {
    /// Returns whether the parameter is named `name`, as the kernel's `parameq` compares names:
    /// byte for byte, except that a dash and an underscore match each other.
    pub fn is_named(&self, name: &str) -> bool {
        let fold = |byte: u8| if byte == b'-' { b'_' } else { byte };
        self.name.len() == name.len()
            && self
                .name
                .iter()
                .zip(name.bytes())
                .all(|(&byte, expected)| fold(byte) == fold(expected))
    }
}

/// Returns the parameters of `command_line`, in order, as the kernel's `parse_args` and
/// `next_arg` split it.
///
/// A parameter ends at whitespace, which [`is_space`] tells, outside double quotes, which it may
/// open anywhere to hold whitespace. The kernel drops a quote that starts the parameter or its
/// value, and then a quote that ends the parameter. The name ends at the first `=` after the
/// parameter's first byte, and a parameter without one has no value. The kernel stops at a NUL,
/// and at a `--` without a value, after which it leaves the rest of the command line to init.
pub fn parameters(command_line: &[u8]) -> Parameters<'_> {
    let end = command_line
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(command_line.len());
    Parameters {
        rest: &command_line[..end],
    }
}

/// The parameters of a kernel command line, in order, which [`parameters`] returns.
#[derive(Clone, Debug)]
pub struct Parameters<'a> {
    /// The text that the kernel has yet to split, which is empty once it stopped.
    rest: &'a [u8],
}

impl<'a> Iterator for Parameters<'a> {
    type Item = Parameter<'a>;

    fn next(&mut self) -> Option<Parameter<'a>> {
        let start = self
            .rest
            .iter()
            .position(|&byte| !is_space(byte))
            .unwrap_or(self.rest.len());
        self.rest = &self.rest[start..];
        let quoted = *self.rest.first()? == b'"';
        let text = if quoted { &self.rest[1..] } else { self.rest };
        let mut in_quote = quoted;
        let mut equals = None;
        let mut length = 0;
        while let Some(&byte) = text.get(length) {
            if is_space(byte) && !in_quote {
                break;
            }
            if equals.is_none() && byte == b'=' && length > 0 {
                equals = Some(length);
            }
            if byte == b'"' {
                in_quote = !in_quote;
            }
            length += 1;
        }
        let parameter = &text[..length];
        self.rest = &text[length..];
        let closing = parameter.last() == Some(&b'"');
        let Some(equals) = equals else {
            let name = if quoted && closing {
                &parameter[..length - 1]
            } else {
                parameter
            };
            if name == b"--" {
                self.rest = &[];
                return None;
            }
            return Some(Parameter { name, value: None });
        };
        let value = &parameter[equals + 1..];
        let value = match value.strip_prefix(b"\"") {
            Some(value) if closing => &value[..value.len().saturating_sub(1)],
            Some(value) => value,
            // The parameter's closing quote ends its value.
            None if quoted && closing => &value[..value.len() - 1],
            None => value,
        };
        Some(Parameter {
            name: &parameter[..equals],
            value: Some(value),
        })
    }
}

impl FusedIterator for Parameters<'_> {}

/// A virtio-mmio device that a `virtio_mmio.device` parameter describes, `SIZE@BASE:IRQ[:ID]`,
/// whatever the type of the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VirtioMmioDevice {
    /// The size of the device's MMIO region, in bytes.
    pub size: u64,
    /// The base address of the device's MMIO region.
    pub base: u64,
    /// The device's interrupt, which is never 0.
    pub irq: u32,
    /// The number of the device's platform device, `virtio-mmio.ID`, if the value gives one; the
    /// kernel numbers a device without one after the device before it.
    pub id: Option<i32>,
}

impl VirtioMmioDevice {
    /// Reads the device that the value of a `virtio_mmio.device` parameter describes, as the
    /// virtio-mmio driver's `vm_cmdline_set` reads it, or returns `None` if the driver refuses the
    /// value.
    ///
    /// The driver reads SIZE as `memparse` does, an integer with an optional suffix, `K`, `M`,
    /// `G`, `T`, `P`, or `E` in either case, which it takes even without digits. The rest of the
    /// value must then be what `sscanf` reads with `@%lli:%u%n:%d%n`, to its end: `@`; BASE, after
    /// any whitespace, with an optional minus sign that negates it at 64 bits; `:`; IRQ, a decimal
    /// integer after any whitespace, which keeps its low 32 bits and must not be 0; and, if more
    /// follows, `:` and ID, a decimal integer after any whitespace, with an optional minus sign,
    /// which keeps its low 32 bits. SIZE and BASE take C's prefixes, hexadecimal after `0x` or `0X`
    /// and a hexadecimal digit and octal after any other leading `0`, and every integer wraps at
    /// 64 bits.
    ///
    /// The kernel may still fail to add a device whose value the driver reads, if it cannot claim
    /// the device's region: one that is empty, one past the physical address space, or one that
    /// conflicts with another resource.
    pub fn parse(value: &[u8]) -> Option<Self> {
        let (size, length) = integer(value, 0);
        let rest = &value[length..];
        let (shift, rest) = match rest.first() {
            Some(b'K' | b'k') => (10, &rest[1..]),
            Some(b'M' | b'm') => (20, &rest[1..]),
            Some(b'G' | b'g') => (30, &rest[1..]),
            Some(b'T' | b't') => (40, &rest[1..]),
            Some(b'P' | b'p') => (50, &rest[1..]),
            Some(b'E' | b'e') => (60, &rest[1..]),
            _ => (0, rest),
        };
        let (base, rest) = scan_integer(rest.strip_prefix(b"@")?, 0, true)?;
        let (irq, rest) = scan_integer(rest.strip_prefix(b":")?, 10, false)?;
        let id = match rest {
            [] => None,
            [b':', id @ ..] => match scan_integer(id, 10, true)? {
                (id, []) => Some(id),
                _ => return None,
            },
            _ => return None,
        };
        // The driver stores IRQ in an `unsigned int` and ID in an `int`, which keep the low 32
        // bits of the integers that `sscanf` reads.
        let irq = irq as u32;
        (irq != 0).then_some(Self {
            size: size << shift,
            base,
            irq,
            id: id.map(|id| id as i32),
        })
    }
}

/// Returns the virtio-mmio devices that the `virtio_mmio.device` parameters of `command_line`
/// describe, in order, as [`parameters`] finds the parameters and [`VirtioMmioDevice::parse`]
/// reads their values. A parameter without a value, or with one that the driver refuses,
/// describes no device.
pub fn virtio_mmio_devices(command_line: &[u8]) -> impl Iterator<Item = VirtioMmioDevice> {
    parameters(command_line)
        .filter(|parameter| parameter.is_named(VIRTIO_MMIO_DEVICE))
        .filter_map(|parameter| parameter.value.and_then(VirtioMmioDevice::parse))
}

/// A token of a kernel command line, as the guest's init splits the command line, which
/// [`init_tokens`] returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Token<'a> {
    /// The name: the token's text up to its first `=`, even if the token starts with it, or all
    /// of it.
    pub name: &'a [u8],
    /// The value: the text after that `=`, or `None` if the token has none.
    pub value: Option<&'a [u8]>,
}

/// Returns the tokens of `command_line`, in order, as the guest's init splits it: as its shell
/// splits the command line into fields, with the default `IFS` and without pathname expansion.
///
/// A token ends at a space, a tab, or a newline, the only bytes at which the command line splits,
/// and no token is empty. Unlike [`parameters`], the split takes no other byte for whitespace,
/// such as a carriage return or 0xa0, honors no quotes, and reads past a NUL and a `--`. A token's
/// name ends at its first `=`, and a token without one has no value.
pub fn init_tokens(command_line: &[u8]) -> impl Iterator<Item = Token<'_>> {
    command_line
        .split(|byte| matches!(byte, b' ' | b'\t' | b'\n'))
        .filter(|token| !token.is_empty())
        .map(|token| match token.iter().position(|&byte| byte == b'=') {
            Some(equals) => Token {
                name: &token[..equals],
                value: Some(&token[equals + 1..]),
            },
            None => Token {
                name: token,
                value: None,
            },
        })
}

/// Returns the DNS server that the `virtnet_dns=` token of `command_line`, such as the guest's
/// `/proc/cmdline`, names, or `None` if no token names one.
///
/// The reader takes the command line's tokens as OpenVMM takes them when it checks the token:
/// split at ASCII whitespace, spaces, tabs, newlines, form feeds, and carriage returns, without
/// quoting, and up to the end of the command line, past a `--` too. It reads each token that
/// starts with `virtnet_dns=`, and accepts it only as OpenVMM renders it: at most one token, whose
/// value is one IPv4 or IPv6 address in its canonical text, as Rust's [`IpAddr`] writes it, such
/// as `10.0.0.1` or `fd00::a00:1`. Like OpenVMM's own reader, it refuses with a [`ProtocolError`]
/// a repeated token, an empty value, a list of addresses, and another spelling of an address,
/// such as `010.0.0.1`, `fd00:0::a00:1`, or `FD00::A00:1`. OpenVMM also checks, before the guest
/// boots, that the address is one of the gateway's, which the guest leaves to it.
pub fn dns_server(command_line: &[u8]) -> Result<Option<IpAddr>, ProtocolError> {
    address_token(command_line, DNS_TOKEN, "IPv4 or IPv6")
}

/// Returns the IPv4 gateway that the `virtnet_gw=` token of `command_line`, such as the guest's
/// `/proc/cmdline`, names, or `None` if no token names one, as for a guest without a NIC.
///
/// The reader takes the command line's tokens as [`dns_server`] takes them, and accepts the token
/// only as OpenVMM renders it: at most one token, whose value is one IPv4 address in its canonical
/// text, as Rust's [`Ipv4Addr`] writes it, such as `10.0.0.1`. It refuses with a
/// [`ProtocolError`] a repeated token, an empty value, an IPv6 address, which OpenVMM names with
/// `virtnet_gw6=` instead, and another spelling of an address, such as `010.0.0.1`.
pub fn gateway(command_line: &[u8]) -> Result<Option<Ipv4Addr>, ProtocolError> {
    address_token(command_line, GATEWAY_TOKEN, "IPv4")
}

/// Returns the address of the `family` that the token of `command_line` that starts with
/// `prefix`, such as `virtnet_dns=`, names, or `None` if no token names one, as [`dns_server`]
/// reads its token: at most one, whose value is one address in its canonical text.
fn address_token<A: FromStr + Display>(
    command_line: &[u8],
    prefix: &str,
    family: &str,
) -> Result<Option<A>, ProtocolError> {
    let name = prefix.strip_suffix('=').unwrap_or(prefix);
    let mut found = None;
    for token in command_line.split(u8::is_ascii_whitespace) {
        let Some(value) = token.strip_prefix(prefix.as_bytes()) else {
            continue;
        };
        if found.is_some() {
            return Err(violation(format!(
                "the {name} kernel parameter is repeated"
            )));
        }
        let address = std::str::from_utf8(value).ok().and_then(|text| {
            text.parse::<A>()
                .ok()
                .filter(|address| address.to_string() == text)
        });
        let Some(address) = address else {
            return Err(violation(format!(
                "the {name} kernel parameter must name one {family} address as OpenVMM renders \
                 it, not '{}'",
                value.escape_ascii()
            )));
        };
        found = Some(address);
    }
    Ok(found)
}

/// Writes to `writer` a resolver file, in the format of `/etc/resolv.conf`, that names `server` as
/// the guest's one DNS server: `nameserver ADDRESS` and a newline, with the address in its
/// canonical text, which [`dns_server`] reads, as NVX's init writes the initramfs's
/// `/etc/resolv.conf` from the `virtnet_dns=` token.
pub fn write_resolver(mut writer: impl io::Write, server: IpAddr) -> io::Result<()> {
    writer.write_all(format!("nameserver {server}\n").as_bytes())
}

/// Reads an integer conversion of the kernel's `sscanf` at the start of `text`, `%i` with `base`
/// 0, or `%u` or `%d` with `base` 10: after any whitespace and, if `signed`, an optional minus
/// sign, the integer that [`integer`] reads, which must start with a decimal digit and which the
/// sign negates at 64 bits. Returns the integer and the rest of `text`, or `None` if no integer
/// starts it.
fn scan_integer(text: &[u8], base: u32, signed: bool) -> Option<(u64, &[u8])> {
    let start = text
        .iter()
        .position(|&byte| !is_space(byte))
        .unwrap_or(text.len());
    let text = &text[start..];
    let (negative, digits) = match text {
        [b'-', digits @ ..] if signed => (true, digits),
        _ => (false, text),
    };
    if !digits.first().is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let (integer, length) = integer(digits, base);
    let integer = if negative {
        integer.wrapping_neg()
    } else {
        integer
    };
    Some((integer, &digits[length..]))
}

/// Reads the integer that starts `text` as the kernel's `simple_strtoull` reads it in `base`, 10,
/// or 0, which picks the base from C's prefixes: hexadecimal after `0x` or `0X` and a hexadecimal
/// digit, octal after any other leading `0`, and decimal otherwise. The integer wraps at 64 bits.
/// Returns the integer and the length of its text, which is 0 if `text` starts with no digit.
fn integer(text: &[u8], base: u32) -> (u64, usize) {
    let (radix, start) = if base != 0 {
        (base, 0)
    } else {
        match text {
            [b'0', b'x' | b'X', digit, ..] if digit.is_ascii_hexdigit() => (16, 2),
            [b'0', ..] => (8, 0),
            _ => (10, 0),
        }
    };
    let mut integer = 0_u64;
    let mut end = start;
    while let Some(digit) = text
        .get(end)
        .and_then(|&byte| char::from(byte).to_digit(radix))
    {
        integer = integer
            .wrapping_mul(u64::from(radix))
            .wrapping_add(u64::from(digit));
        end += 1;
    }
    (integer, end)
}

#[cfg(test)]
pub(crate) mod model;
#[cfg(test)]
mod tests;
