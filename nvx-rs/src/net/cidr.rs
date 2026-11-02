//! IP networks in CIDR notation, as used by egress rules.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// An IPv4 or IPv6 network whose host bits are zero.
///
/// Networks order by address, IPv4 before IPv6, and then by prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cidr {
    /// First address of the network, with every host bit clear.
    address: IpAddr,
    /// Number of leading network bits, from zero through the address-family width.
    prefix: u8,
}

impl Cidr {
    /// Parses `ADDRESS` or `ADDRESS/PREFIX`. Host bits must be zero.
    pub fn parse(value: &str) -> Result<Self, String> {
        let cidr = Self::parse_with(value, parse_prefix_length)?;
        if cidr.network() != cidr.address {
            return Err(format!("{value:?} has host bits set"));
        }
        Ok(cidr)
    }

    /// Parses `ADDRESS` or `ADDRESS/PREFIX` as OpenVMM reads the destination of an egress rule,
    /// and clears the host bits. The prefix length is read as `u8::from_str` reads it.
    pub fn parse_masked(value: &str) -> Result<Self, String> {
        let cidr = Self::parse_with(value, |prefix| prefix.parse().ok())?;
        Ok(Self {
            address: cidr.network(),
            prefix: cidr.prefix,
        })
    }

    /// Parses `ADDRESS` or `ADDRESS/PREFIX`, reading the prefix length with `prefix_length`,
    /// without checking the host bits.
    fn parse_with(value: &str, prefix_length: fn(&str) -> Option<u8>) -> Result<Self, String> {
        let (address, prefix) = match value.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (value, None),
        };
        let address: IpAddr = address
            .parse()
            .map_err(|_| format!("{value:?} is not an IP address or CIDR"))?;
        let width = match address {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        let prefix = match prefix {
            None => width,
            Some(prefix) => prefix_length(prefix)
                .filter(|prefix| *prefix <= width)
                .ok_or_else(|| format!("{value:?} has an invalid prefix length"))?,
        };
        Ok(Self { address, prefix })
    }

    /// Returns the network of every address of one family: `::/0` with `ipv6`, and `0.0.0.0/0`
    /// without.
    pub fn everything(ipv6: bool) -> Self {
        let address = if ipv6 {
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        } else {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        };
        Self { address, prefix: 0 }
    }

    /// Returns whether this is an IPv6 network.
    pub fn is_ipv6(self) -> bool {
        self.address.is_ipv6()
    }

    /// Returns whether `other` lies within this network. Networks of different families never
    /// contain each other.
    pub fn contains(self, other: Self) -> bool {
        self.is_ipv6() == other.is_ipv6()
            && self.prefix <= other.prefix
            && mask(other.bits(), self.prefix, self.width()) == self.bits()
    }

    /// Returns whether this network and `other` share an address.
    pub(crate) fn overlaps(self, other: Self) -> bool {
        self.contains(other) || other.contains(self)
    }

    /// Returns the bit width of this network's address family.
    fn width(self) -> u8 {
        if self.is_ipv6() { 128 } else { 32 }
    }

    /// Returns the network address as an integer, with IPv4 in the low 32 bits.
    fn bits(self) -> u128 {
        match self.address {
            IpAddr::V4(address) => u128::from(u32::from(address)),
            IpAddr::V6(address) => u128::from(address),
        }
    }

    /// Returns the network of this one's family at `bits` with `prefix`.
    fn with_bits(self, bits: u128, prefix: u8) -> Self {
        let address = match self.address {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::from(bits as u32)),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::from(bits)),
        };
        Self { address, prefix }
    }

    /// Splits this network into its equally sized low and high child networks.
    fn halves(self) -> [Self; 2] {
        let prefix = self.prefix + 1;
        [
            self.with_bits(self.bits(), prefix),
            self.with_bits(self.bits() | (1 << (self.width() - prefix)), prefix),
        ]
    }

    /// Returns the smallest set of networks that cover this network except `excluded`, or `None`
    /// if that set has more than `limit` networks. Exclusions of the other family exclude nothing.
    ///
    /// Each exclusion is followed only into the half of a network that contains it, and expansion
    /// stops as soon as the result exceeds `limit`, so the work grows linearly with the number of
    /// exclusions rather than with that number times the size of the result.
    pub fn subtract(self, excluded: &[Self], limit: usize) -> Option<Vec<Self>> {
        let mut overlapping: Vec<Self> = excluded
            .iter()
            .copied()
            .filter(|exclusion| exclusion.overlaps(self))
            .collect();
        overlapping.sort_unstable();
        let mut remainder = Vec::new();
        self.subtract_sorted(&overlapping, limit, &mut remainder)
            .then_some(remainder)
    }

    /// Appends the networks that cover this network except `excluded` to `remainder`, and returns
    /// whether `remainder` still holds at most `limit` networks. `excluded` holds, in ascending
    /// order, only exclusions that overlap this network.
    fn subtract_sorted(self, excluded: &[Self], limit: usize, remainder: &mut Vec<Self>) -> bool {
        if excluded.is_empty() {
            remainder.push(self);
            return remainder.len() <= limit;
        }
        if excluded.iter().any(|exclusion| exclusion.contains(self)) {
            return true;
        }
        // Every exclusion lies strictly inside this network, so it has at least one more bit and
        // lies inside exactly one half; sorted by address, the low half's exclusions come first.
        let [low, high] = self.halves();
        let split = excluded.partition_point(|exclusion| exclusion.address < high.address);
        low.subtract_sorted(&excluded[..split], limit, remainder)
            && high.subtract_sorted(&excluded[split..], limit, remainder)
    }

    /// Clears the host bits and returns the resulting network address.
    fn network(self) -> IpAddr {
        self.with_bits(mask(self.bits(), self.prefix, self.width()), self.prefix)
            .address
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.address, self.prefix)
    }
}

/// Parses a prefix length in canonical form: decimal digits without a sign or leading zeros.
pub fn parse_prefix_length(text: &str) -> Option<u8> {
    let digits = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    // `u8::from_str` alone would also accept a leading `+`.
    if digits && (text.len() == 1 || !text.starts_with('0')) {
        text.parse().ok()
    } else {
        None
    }
}

/// Clears the host bits of an address of `width` bits.
fn mask(address: u128, prefix: u8, width: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        address & !((1u128 << (width - prefix)) - 1)
    }
}

#[cfg(test)]
mod tests;
