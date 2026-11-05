//! Network primitives that do not depend on the host: IP networks in CIDR notation and their
//! subtraction, which egress rules use, the forms of a VM's hostname and guest network address,
//! a checked guest address, and the gateway of a guest network.

use std::fmt;
use std::net::Ipv4Addr;

pub mod cidr;

pub use cidr::Cidr;

/// Returns whether `hostname` is a lowercase RFC 1123 label of up to 63 characters.
pub fn is_valid_hostname(hostname: &str) -> bool {
    let bytes = hostname.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        && bytes.first() != Some(&b'-')
        && bytes.last() != Some(&b'-')
}

/// Returns whether `value` is a static guest address that OpenVMM accepts: an IPv4
/// `ADDRESS/PREFIX` with a /1 to /30 prefix and an address other than the network's own, its
/// broadcast address, and the gateway, which is its first address.
pub fn is_valid_guest_network(value: &str) -> bool {
    Ipv4Cidr::parse(value).is_ok()
}

/// Returns the gateway of a static guest address that [`is_valid_guest_network`] accepts: the
/// first address of its network, at which OpenVMM presents the guest's gateway. Returns `None`
/// for any other value.
pub fn guest_gateway(value: &str) -> Option<Ipv4Addr> {
    let guest = Ipv4Cidr::parse(value).ok()?;
    Some(Ipv4Addr::from(
        (u32::from(guest.address) & guest.mask()) + 1,
    ))
}

/// A static guest address that OpenVMM accepts, checked when it is built: an IPv4 address with a
/// /1 to /30 prefix, which is not its network's own address, its broadcast address, or the
/// gateway, its network's first address.
///
/// It displays and parses as `ADDRESS/PREFIX`, such as `10.0.0.2/24`, the form that
/// [`is_valid_guest_network`] accepts, and with the `serde` feature it serializes as that text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ipv4Cidr {
    /// The guest's address.
    address: Ipv4Addr,
    /// The length of the network's prefix: 1 to 30.
    prefix_len: u8,
}

impl Ipv4Cidr {
    /// Returns the guest address `address/prefix_len`.
    ///
    /// Fails with a message when the prefix is not /1 to /30, or when the address is its network's
    /// own address, its broadcast address, or its gateway.
    pub fn new(address: Ipv4Addr, prefix_len: u8) -> Result<Self, String> {
        let guest = Self {
            address,
            prefix_len,
        };
        let host = u32::from(address);
        let valid = (1..=30).contains(&prefix_len) && {
            let network = host & guest.mask();
            ![network, network | !guest.mask(), network + 1].contains(&host)
        };
        if valid {
            Ok(guest)
        } else {
            Err(invalid_guest_address(&guest.to_string()))
        }
    }

    /// Parses a guest address in the form `ADDRESS/PREFIX`, whose prefix is decimal digits without
    /// a sign or leading zeros, as [`Ipv4Cidr::new`] checks it.
    ///
    /// Fails with a message that names `value` when it is not such an address.
    pub fn parse(value: &str) -> Result<Self, String> {
        let invalid = || invalid_guest_address(value);
        let (address, prefix_len) = value.split_once('/').ok_or_else(invalid)?;
        let address = address.parse().map_err(|_| invalid())?;
        let prefix_len = cidr::parse_prefix_length(prefix_len).ok_or_else(invalid)?;
        Self::new(address, prefix_len).map_err(|_| invalid())
    }

    /// Returns the address.
    pub fn address(self) -> Ipv4Addr {
        self.address
    }

    /// Returns the prefix length: 1 to 30.
    pub fn prefix_len(self) -> u8 {
        self.prefix_len
    }

    /// Returns the network mask of the prefix.
    fn mask(self) -> u32 {
        u32::MAX << (32 - u32::from(self.prefix_len))
    }
}

impl fmt::Display for Ipv4Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix_len)
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Ipv4Cidr {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Ipv4Cidr {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// Returns the message of a value that is not a guest address that OpenVMM accepts.
fn invalid_guest_address(value: &str) -> String {
    format!(
        "guest network {value:?} must be an IPv4 address with a /1 to /30 prefix that is not its \
         network's network, broadcast, or gateway (first) address"
    )
}

#[cfg(test)]
mod tests;
