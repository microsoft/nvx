//! IP networks in CIDR notation, as used by egress rules.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr};

/// An IPv4 or IPv6 network whose host bits are zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cidr {
    address: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// Parses `ADDRESS` or `ADDRESS/PREFIX`. Host bits must be zero.
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
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
            Some(prefix) => prefix
                .parse::<u8>()
                .ok()
                .filter(|prefix| *prefix <= width && !prefix_has_leading_zero(value))
                .ok_or_else(|| format!("{value:?} has an invalid prefix length"))?,
        };
        let cidr = Self { address, prefix };
        if cidr.network() != address {
            return Err(format!("{value:?} has host bits set"));
        }
        Ok(cidr)
    }

    /// Returns the IPv4 network as an address and prefix length.
    pub(crate) fn ipv4(self) -> Option<Ipv4Cidr> {
        match self.address {
            IpAddr::V4(address) => Some(Ipv4Cidr {
                address: u32::from(address),
                prefix: self.prefix,
            }),
            IpAddr::V6(_) => None,
        }
    }

    /// Returns whether `other` lies within this network.
    pub(crate) fn contains(self, other: Self) -> bool {
        match (self.address, other.address) {
            (IpAddr::V4(_), IpAddr::V4(_)) => {
                let (outer, inner) = (self.ipv4().unwrap(), other.ipv4().unwrap());
                outer.contains(inner)
            }
            (IpAddr::V6(outer), IpAddr::V6(inner)) => {
                self.prefix <= other.prefix
                    && mask128(u128::from(inner), self.prefix) == u128::from(outer)
            }
            _ => false,
        }
    }

    fn network(self) -> IpAddr {
        match self.address {
            IpAddr::V4(address) => {
                IpAddr::V4(Ipv4Addr::from(mask32(u32::from(address), self.prefix)))
            }
            IpAddr::V6(address) => IpAddr::V6(mask128(u128::from(address), self.prefix).into()),
        }
    }
}

fn prefix_has_leading_zero(value: &str) -> bool {
    value
        .split_once('/')
        .is_some_and(|(_, prefix)| prefix.len() > 1 && prefix.starts_with('0'))
}

fn mask32(address: u32, prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        address & (u32::MAX << (32 - u32::from(prefix)))
    }
}

fn mask128(address: u128, prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        address & (u128::MAX << (128 - u32::from(prefix)))
    }
}

/// An IPv4 network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Ipv4Cidr {
    address: u32,
    prefix: u8,
}

// Only the OpenVMM backend expands rules into networks.
#[cfg_attr(not(feature = "openvmm"), allow(dead_code))]
impl Ipv4Cidr {
    fn contains(self, other: Self) -> bool {
        self.prefix <= other.prefix && mask32(other.address, self.prefix) == self.address
    }

    fn overlaps(self, other: Self) -> bool {
        self.contains(other) || other.contains(self)
    }

    fn halves(self) -> [Self; 2] {
        let prefix = self.prefix + 1;
        [
            Self {
                address: self.address,
                prefix,
            },
            Self {
                address: self.address | (1 << (32 - u32::from(prefix))),
                prefix,
            },
        ]
    }

    /// Returns the smallest set of networks that cover this network except `excluded`, or `None`
    /// if that set has more than `limit` networks.
    ///
    /// Each exclusion is followed only into the half of a network that contains it, and expansion
    /// stops as soon as the result exceeds `limit`, so the work grows linearly with the number of
    /// exclusions rather than with that number times the size of the result.
    pub(crate) fn subtract(self, excluded: &[Self], limit: usize) -> Option<Vec<Self>> {
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
}

impl fmt::Display for Ipv4Cidr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}/{}",
            Ipv4Addr::from(self.address),
            self.prefix
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(value: &str) -> Ipv4Cidr {
        Cidr::parse(value).unwrap().ipv4().unwrap()
    }

    #[test]
    fn parsing_requires_canonical_networks() {
        assert_eq!(v4("192.0.2.1").to_string(), "192.0.2.1/32");
        assert_eq!(v4("10.0.0.0/8").to_string(), "10.0.0.0/8");
        assert_eq!(v4("0.0.0.0/0").to_string(), "0.0.0.0/0");
        assert!(Cidr::parse("2001:db8::/32").unwrap().ipv4().is_none());
        for invalid in [
            "10.0.0.1/8",
            "10.0.0.0/33",
            "10.0.0.0/08",
            "example.com",
            "10.0.0.0/",
            "2001:db8::1/32",
        ] {
            assert!(Cidr::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn containment_is_family_aware() {
        let outer = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(outer.contains(Cidr::parse("10.1.0.0/16").unwrap()));
        assert!(outer.contains(outer));
        assert!(!outer.contains(Cidr::parse("11.0.0.0/16").unwrap()));
        assert!(!outer.contains(Cidr::parse("::/0").unwrap()));
        let v6 = Cidr::parse("2001:db8::/32").unwrap();
        assert!(v6.contains(Cidr::parse("2001:db8:1::/48").unwrap()));
    }

    #[test]
    fn subtraction_covers_exactly_the_remainder() {
        let subtract = |network: &str, excluded: &[Ipv4Cidr]| {
            v4(network).subtract(excluded, usize::MAX).unwrap()
        };
        let remainder: Vec<String> = subtract("10.0.0.0/8", &[v4("10.0.0.0/9")])
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(remainder, ["10.128.0.0/9"]);

        let remainder = subtract("10.0.0.0/30", &[v4("10.0.0.1")]);
        let rendered: Vec<String> = remainder.iter().map(ToString::to_string).collect();
        assert_eq!(rendered, ["10.0.0.0/32", "10.0.0.2/31"]);

        assert!(subtract("10.1.0.0/16", &[v4("10.0.0.0/8")]).is_empty());
        assert_eq!(
            subtract("10.0.0.0/8", &[v4("11.0.0.0/8")]),
            [v4("10.0.0.0/8")]
        );
        // Every remaining address is outside the exclusions, and the sizes add up.
        let excluded = [v4("10.0.0.0/24"), v4("10.0.7.0/24")];
        let remainder = subtract("10.0.0.0/16", &excluded);
        let covered: u64 = remainder
            .iter()
            .map(|cidr| 1u64 << (32 - u32::from(cidr.prefix)))
            .sum();
        assert_eq!(covered, (1 << 16) - 2 * 256);
        assert!(
            remainder
                .iter()
                .all(|cidr| !excluded.iter().any(|exclusion| exclusion.overlaps(*cidr)))
        );
    }

    /// The direct recursion that rescans every exclusion at each split, as a reference.
    fn reference_subtract(network: Ipv4Cidr, excluded: &[Ipv4Cidr]) -> Vec<Ipv4Cidr> {
        if excluded.iter().any(|exclusion| exclusion.contains(network)) {
            return Vec::new();
        }
        if !excluded.iter().any(|exclusion| exclusion.overlaps(network)) {
            return vec![network];
        }
        network
            .halves()
            .into_iter()
            .flat_map(|half| reference_subtract(half, excluded))
            .collect()
    }

    #[test]
    fn partitioned_subtraction_matches_the_direct_recursion() {
        // A linear congruential generator keeps the cases deterministic.
        let mut state = 0x2545_f491_u32;
        let mut next = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state
        };
        let network = v4("10.0.0.0/16");
        for _ in 0..500 {
            let count = next() % 32;
            let excluded: Vec<Ipv4Cidr> = (0..count)
                .map(|_| {
                    let prefix = 12 + (next() % 21) as u8;
                    // Most exclusions fall inside the network; some cover it or lie elsewhere.
                    let address = if next() % 4 == 0 {
                        next()
                    } else {
                        network.address | (next() & 0xffff)
                    };
                    Ipv4Cidr {
                        address: mask32(address, prefix),
                        prefix,
                    }
                })
                .collect();
            assert_eq!(
                network.subtract(&excluded, usize::MAX).unwrap(),
                reference_subtract(network, &excluded),
                "{excluded:?}"
            );
        }
    }

    #[test]
    fn subtraction_stops_once_the_remainder_exceeds_its_limit() {
        // 512 evenly spaced addresses split the address space into 512 * 23 networks.
        let excluded: Vec<Ipv4Cidr> = (0..512u32)
            .map(|index| Ipv4Cidr {
                address: index << 23,
                prefix: 32,
            })
            .collect();
        let everything = v4("0.0.0.0/0");
        let size = |limit| {
            everything
                .subtract(&excluded, limit)
                .map(|remainder| remainder.len())
        };
        assert_eq!(size(usize::MAX), Some(512 * 23));
        assert_eq!(size(512 * 23), Some(512 * 23));
        assert_eq!(size(512 * 23 - 1), None);
        assert_eq!(size(256), None);
    }
}
