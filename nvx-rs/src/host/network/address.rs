//! IP networks as Python's `ipaddress` module parses, orders, and prints them, and the intervals
//! of addresses that egress rules select.
//!
//! The egress policy compiler ports the former `scripts/nvx_tools/egress_policy.py`, which read
//! CIDRs with `ipaddress.ip_interface`, so [`parse_interface`] accepts exactly what it accepted:
//! host bits, which it clears, IPv4 netmasks and hostmasks such as `/255.0.0.0`, prefix lengths
//! with leading zeros, embedded IPv4 in IPv6 addresses, and IPv6 scopes, which callers reject. A
//! [`Network`] prints as `str(network)` does in Python 3.13 and later, which writes an
//! IPv4-mapped IPv6 address such as `::ffff:192.0.2.0` with its IPv4 address in dotted form.

use std::fmt;
use std::net::Ipv4Addr;

/// Address family of a [`Network`].
///
/// IPv4 orders before IPv6, as Python's IP versions 4 and 6 do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Family {
    /// IPv4: 32-bit addresses.
    V4,
    /// IPv6: 128-bit addresses.
    V6,
}

impl Family {
    /// Both families, in the order in which the compiler lowers them.
    pub(crate) const ALL: [Self; 2] = [Self::V4, Self::V6];

    /// Returns the number of bits of an address of this family.
    const fn bits(self) -> u32 {
        match self {
            Self::V4 => 32,
            Self::V6 => 128,
        }
    }

    /// Returns the last address of this family, whose bits are all set.
    pub(crate) const fn last_address(self) -> u128 {
        match self {
            Self::V4 => u32::MAX as u128,
            Self::V6 => u128::MAX,
        }
    }

    /// Returns every address of this family as one interval.
    pub(crate) const fn everything(self) -> Interval {
        (0, self.last_address())
    }
}

/// An inclusive interval of addresses of one family: its first and its last address.
pub(crate) type Interval = (u128, u128);

/// A network of one family, without host bits.
///
/// Networks order as the compiler sorts its output: by family, then by address, then by prefix
/// length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Network {
    /// Address family.
    family: Family,
    /// First address of the network, with every host bit clear.
    address: u128,
    /// Number of leading network bits, at most the family's address width.
    prefix: u8,
}

impl Network {
    /// Returns the network of `family` with `prefix` that holds `address`, clearing its host
    /// bits.
    fn new(family: Family, address: u128, prefix: u32) -> Self {
        let network = Self {
            family,
            address,
            prefix: prefix as u8,
        };
        Self {
            address: address & !network.host_mask(),
            ..network
        }
    }

    /// Returns the address family of the network.
    pub(crate) fn family(self) -> Family {
        self.family
    }

    /// Returns the interval of the network's addresses: its network and broadcast addresses.
    pub(crate) fn interval(self) -> Interval {
        (self.address, self.address | self.host_mask())
    }

    /// Returns the host bits of an address of the network's family.
    fn host_mask(self) -> u128 {
        let prefix = u32::from(self.prefix);
        if prefix >= self.family.bits() {
            0
        } else {
            self.family.last_address() >> prefix
        }
    }

    /// Returns whether `other` lies within this network, as Python's `subnet_of` decides.
    pub(crate) fn contains(self, other: Self) -> bool {
        let (first, last) = self.interval();
        let (other_first, other_last) = other.interval();
        self.family == other.family && first <= other_first && last >= other_last
    }
}

impl fmt::Display for Network {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.family {
            Family::V4 => write!(formatter, "{}", Ipv4Addr::from(self.address as u32))?,
            Family::V6 => write_ipv6(formatter, self.address)?,
        }
        write!(formatter, "/{}", self.prefix)
    }
}

/// Writes an IPv6 address as Python's `str(IPv6Address)` does: an IPv4-mapped address as
/// `::ffff:` and its IPv4 address in dotted form, and any other address as lowercase hexadecimal
/// groups whose first longest run of two or more zero groups becomes `::`.
fn write_ipv6(formatter: &mut fmt::Formatter<'_>, address: u128) -> fmt::Result {
    if address >> 32 == 0xFFFF {
        return write!(formatter, "::ffff:{}", Ipv4Addr::from(address as u32));
    }
    let groups: [u16; 8] = std::array::from_fn(|index| (address >> (112 - 16 * index)) as u16);
    // The first of the longest runs of zero groups, if a run has at least two groups.
    let mut best: Option<(usize, usize)> = None;
    let mut index = 0;
    while index < groups.len() {
        if groups[index] != 0 {
            index += 1;
            continue;
        }
        let start = index;
        while index < groups.len() && groups[index] == 0 {
            index += 1;
        }
        let length = index - start;
        if length > best.map_or(1, |(_, best_length)| best_length) {
            best = Some((start, length));
        }
    }
    let write_groups = |formatter: &mut fmt::Formatter<'_>, groups: &[u16]| {
        for (index, group) in groups.iter().enumerate() {
            if index > 0 {
                formatter.write_str(":")?;
            }
            write!(formatter, "{group:x}")?;
        }
        Ok(())
    };
    match best {
        None => write_groups(formatter, &groups),
        Some((start, length)) => {
            write_groups(formatter, &groups[..start])?;
            formatter.write_str("::")?;
            write_groups(formatter, &groups[start + length..])
        }
    }
}

/// Why [`parse_interface`] refused a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterfaceError {
    /// The string is neither an IPv4 nor an IPv6 interface.
    Invalid,
    /// The string is an IPv6 interface that names a scope, such as `fe80::1%eth0/64`.
    Scoped,
}

/// Parses `text` as Python's `ipaddress.ip_interface` does and returns its network, whose host
/// bits are clear.
pub(crate) fn parse_interface(text: &str) -> Result<Network, InterfaceError> {
    let text = text.as_bytes();
    // `ip_interface` tries IPv4 first, and no string is both.
    if let Some(network) = parse_ipv4_interface(text) {
        return Ok(network);
    }
    match parse_ipv6_interface(text) {
        Some((network, false)) => Ok(network),
        Some((_, true)) => Err(InterfaceError::Scoped),
        None => Err(InterfaceError::Invalid),
    }
}

/// Splits `text` at every `/` as `_split_optional_netmask` does, and returns the address and the
/// netmask, if any, or `None` for more than one `/`.
fn split_netmask(text: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
    let mut parts = text.split(|&byte| byte == b'/');
    let address = parts.next()?;
    let netmask = parts.next();
    parts.next().is_none().then_some((address, netmask))
}

/// Parses an `IPv4Interface`.
fn parse_ipv4_interface(text: &[u8]) -> Option<Network> {
    let (address, netmask) = split_netmask(text)?;
    let address = parse_ipv4(address)?;
    let prefix = match netmask {
        None => 32,
        Some(netmask) => {
            prefix_from_prefix_string(netmask, 32).or_else(|| prefix_from_ipv4_mask(netmask))?
        }
    };
    Some(Network::new(Family::V4, u128::from(address), prefix))
}

/// Parses an `IPv6Interface` and returns its network and whether it names a scope.
fn parse_ipv6_interface(text: &[u8]) -> Option<(Network, bool)> {
    let (address, netmask) = split_netmask(text)?;
    // `_split_scope_id`: a scope follows the first `%`, and is not empty and has no other `%`.
    let (address, scoped) = match address.iter().position(|&byte| byte == b'%') {
        None => (address, false),
        Some(position) => {
            let scope = &address[position + 1..];
            if scope.is_empty() || scope.contains(&b'%') {
                return None;
            }
            (&address[..position], true)
        }
    };
    let address = parse_ipv6(address)?;
    let prefix = match netmask {
        None => 128,
        Some(netmask) => prefix_from_prefix_string(netmask, 128)?,
    };
    Some((Network::new(Family::V6, address, prefix), scoped))
}

/// Parses a prefix length as `_prefix_from_prefix_string` does: ASCII digits, leading zeros
/// included, whose value is at most `maximum`.
fn prefix_from_prefix_string(text: &[u8], maximum: u32) -> Option<u32> {
    // Python's `int` refuses strings of more than 4300 digits, its default
    // `sys.get_int_max_str_digits()`, even when they are mostly leading zeros.
    const MAX_INT_DIGITS: usize = 4300;
    if text.is_empty() || text.len() > MAX_INT_DIGITS || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let significant = &text[text.iter().take_while(|&&byte| byte == b'0').count()..];
    if significant.len() > 3 {
        return None;
    }
    let value = significant
        .iter()
        .fold(0, |value, digit| value * 10 + u32::from(digit - b'0'));
    (value <= maximum).then_some(value)
}

/// Parses an IPv4 netmask, such as `255.255.0.0`, or a hostmask, such as `0.0.255.255`, as
/// `_prefix_from_ip_string` does, and returns its prefix length. The ambiguous all-ones and
/// all-zeros masks are netmasks.
fn prefix_from_ipv4_mask(text: &[u8]) -> Option<u32> {
    let mask = parse_ipv4(text)?;
    prefix_from_ipv4_netmask(mask).or_else(|| prefix_from_ipv4_netmask(!mask))
}

/// Returns the prefix length of an IPv4 netmask: leading ones and trailing zeros.
fn prefix_from_ipv4_netmask(mask: u32) -> Option<u32> {
    let prefix = mask.leading_ones();
    (mask.checked_shl(prefix).unwrap_or(0) == 0).then_some(prefix)
}

/// Parses an IPv4 address as `IPv4Address._ip_int_from_string` does: four decimal octets of at
/// most three ASCII digits each, without leading zeros, and at most 255.
fn parse_ipv4(text: &[u8]) -> Option<u32> {
    let mut octets = text.split(|&byte| byte == b'.');
    let mut address = 0u32;
    for _ in 0..4 {
        let octet = octets.next()?;
        let valid = !octet.is_empty()
            && octet.len() <= 3
            && octet.iter().all(u8::is_ascii_digit)
            && (octet == b"0" || octet[0] != b'0');
        if !valid {
            return None;
        }
        let value = octet
            .iter()
            .fold(0u32, |value, digit| value * 10 + u32::from(digit - b'0'));
        if value > 255 {
            return None;
        }
        address = (address << 8) | value;
    }
    octets.next().is_none().then_some(address)
}

/// One colon-separated part of an IPv6 address: its text, or a group that an embedded IPv4
/// address contributes.
#[derive(Clone, Copy)]
enum Part<'a> {
    /// A hexadecimal group as written, possibly empty.
    Text(&'a [u8]),
    /// A group of an embedded IPv4 address.
    Group(u16),
}

impl Part<'_> {
    /// Returns whether the part is empty, as at a `::`.
    fn is_empty(self) -> bool {
        matches!(self, Self::Text(text) if text.is_empty())
    }

    /// Returns the value of the group, as `_parse_hextet` parses it: one to four hexadecimal
    /// digits.
    fn value(self) -> Option<u16> {
        match self {
            Self::Group(group) => Some(group),
            Self::Text(text) => {
                if text.is_empty() || text.len() > 4 {
                    return None;
                }
                text.iter().try_fold(0u16, |value, &digit| {
                    Some((value << 4) | (char::from(digit).to_digit(16)? as u16))
                })
            }
        }
    }
}

/// Parses an IPv6 address without a scope as `IPv6Address._ip_int_from_string` does.
fn parse_ipv6(text: &[u8]) -> Option<u128> {
    // Python counts characters, not bytes, but no address with a non-ASCII character is valid.
    if text.is_empty() || text.len() > 45 || !text.is_ascii() {
        return None;
    }
    // At most eight groups and one more part for a leading or trailing `::`; `split` keeps the
    // rest of a longer address in a tenth part.
    const MAX_PARTS: usize = 9;
    let mut parts: Vec<Part<'_>> = text
        .splitn(MAX_PARTS + 1, |&byte| byte == b':')
        .map(Part::Text)
        .collect();
    if parts.len() < 3 {
        return None;
    }
    if let Some(&Part::Text(last)) = parts.last()
        && last.contains(&b'.')
    {
        let ipv4 = parse_ipv4(last)?;
        parts.pop();
        parts.push(Part::Group((ipv4 >> 16) as u16));
        parts.push(Part::Group(ipv4 as u16));
    }
    if parts.len() > MAX_PARTS {
        return None;
    }
    let count = parts.len();
    // A `::` is an empty part between the endpoints, and at most one is allowed.
    let mut skip = None;
    for (index, part) in parts.iter().enumerate().take(count - 1).skip(1) {
        if part.is_empty() {
            if skip.is_some() {
                return None;
            }
            skip = Some(index);
        }
    }
    let (high, low, skipped) = match skip {
        Some(skip) => {
            let mut high = skip;
            let mut low = count - skip - 1;
            if parts[0].is_empty() {
                high -= 1;
                if high != 0 {
                    return None;
                }
            }
            if parts[count - 1].is_empty() {
                low -= 1;
                if low != 0 {
                    return None;
                }
            }
            let skipped = 8usize
                .checked_sub(high + low)
                .filter(|&skipped| skipped >= 1)?;
            (high, low, skipped)
        }
        None => {
            if count != 8 || parts[0].is_empty() || parts[count - 1].is_empty() {
                return None;
            }
            (count, 0, 0)
        }
    };
    let mut address = 0u128;
    for part in &parts[..high] {
        address = (address << 16) | u128::from(part.value()?);
    }
    address = address.checked_shl(16 * skipped as u32).unwrap_or(0);
    for part in &parts[count - low..] {
        address = (address << 16) | u128::from(part.value()?);
    }
    Some(address)
}

/// The networks that Python's `ipaddress.summarize_address_range` yields for an interval: the
/// fewest networks that cover it exactly, from its first address.
pub(crate) struct Summary {
    /// Address family of the interval.
    family: Family,
    /// First address that the networks yielded so far do not cover, or `None` once they cover the
    /// interval.
    next: Option<u128>,
    /// Last address of the interval.
    last: u128,
}

/// Returns the networks that cover the addresses of `family` from `first` through `last`, as
/// `ipaddress.summarize_address_range` yields them.
pub(crate) fn summarize(family: Family, (first, last): Interval) -> Summary {
    #[cfg(test)]
    probe::SUMMARIES.with(|count| count.set(count.get() + 1));
    Summary {
        family,
        next: Some(first),
        last,
    }
}

impl Iterator for Summary {
    type Item = Network;

    fn next(&mut self) -> Option<Network> {
        let first = self.next.filter(|&first| first <= self.last)?;
        let bits = self.family.bits();
        let alignment = if first == 0 {
            bits
        } else {
            first.trailing_zeros().min(bits)
        };
        // The largest power of two that fits in the rest of the interval; the whole IPv6 address
        // space holds 2^128 addresses, one more than a `u128` holds.
        let span = self.last - first;
        let fitting = span
            .checked_add(1)
            .map_or(128, |count| 127 - count.leading_zeros());
        let size_bits = alignment.min(fitting);
        let network = Network::new(self.family, first, bits - size_bits);
        // Python stops once the next address passes the family's last address.
        self.next = 1u128
            .checked_shl(size_bits)
            .and_then(|size| first.checked_add(size))
            .filter(|&next| next - 1 != self.family.last_address());
        Some(network)
    }
}

/// Returns the union of `intervals`, sorted, with overlapping and adjacent intervals merged, as
/// `_merge_intervals` does.
pub(crate) fn merge(intervals: impl IntoIterator<Item = Interval>) -> Vec<Interval> {
    let mut sorted: Vec<Interval> = intervals.into_iter().collect();
    #[cfg(test)]
    probe::MERGED_INTERVALS.with(|count| count.set(count.get() + sorted.len()));
    sorted.sort_unstable();
    let mut merged: Vec<Interval> = Vec::with_capacity(sorted.len());
    for (start, end) in sorted {
        match merged.last_mut() {
            Some(previous) if start <= previous.1.saturating_add(1) => {
                previous.1 = previous.1.max(end);
            }
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Returns the addresses of `sources` that `exclusions` do not hold, as `_subtract_intervals`
/// does. Both lists are sorted and their intervals disjoint.
pub(crate) fn subtract(sources: &[Interval], exclusions: &[Interval]) -> Vec<Interval> {
    let mut remaining = Vec::new();
    let mut first_exclusion = 0;
    for &(source_start, source_end) in sources {
        while first_exclusion < exclusions.len() && exclusions[first_exclusion].1 < source_start {
            first_exclusion += 1;
        }
        let mut cursor = source_start;
        // Whether the cursor passed the last address of the family, which a `u128` cannot hold.
        let mut exhausted = false;
        for &(excluded_start, excluded_end) in exclusions[first_exclusion..]
            .iter()
            .take_while(|(excluded_start, _)| *excluded_start <= source_end)
        {
            if cursor < excluded_start {
                remaining.push((cursor, excluded_start - 1));
            }
            match excluded_end.checked_add(1) {
                Some(next) => cursor = cursor.max(next),
                None => {
                    exhausted = true;
                    break;
                }
            }
            if cursor > source_end {
                break;
            }
        }
        if !exhausted && cursor <= source_end {
            remaining.push((cursor, source_end));
        }
    }
    remaining
}

/// Counters that tests read to check how much work the compiler does, as the Python tests check
/// by mocking `ipaddress.summarize_address_range` and `_merge_intervals`.
#[cfg(test)]
pub(crate) mod probe {
    use std::cell::Cell;

    thread_local! {
        /// Number of [`summarize`](super::summarize) calls on this thread.
        pub(crate) static SUMMARIES: Cell<usize> = const { Cell::new(0) };
        /// Number of intervals that [`merge`](super::merge) received on this thread.
        pub(crate) static MERGED_INTERVALS: Cell<usize> = const { Cell::new(0) };
    }

    /// Resets the counters and returns the result of `operation` with the
    /// [`summarize`](super::summarize) calls and merged intervals that it caused.
    pub(crate) fn count<T>(operation: impl FnOnce() -> T) -> (T, usize, usize) {
        SUMMARIES.with(|count| count.set(0));
        MERGED_INTERVALS.with(|count| count.set(0));
        let result = operation();
        (
            result,
            SUMMARIES.with(Cell::get),
            MERGED_INTERVALS.with(Cell::get),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn network(text: &str) -> Result<String, InterfaceError> {
        parse_interface(text).map(|network| network.to_string())
    }

    #[test]
    fn interfaces_parse_as_python_does() {
        for (text, expected) in [
            ("10.0.0.5/24", "10.0.0.0/24"),
            ("10.0.0.0/255.0.0.0", "10.0.0.0/8"),
            ("10.0.0.0/0.255.255.255", "10.0.0.0/8"),
            ("10.0.0.0/255.255.255.255", "10.0.0.0/32"),
            ("10.0.0.0/0.0.0.0", "0.0.0.0/0"),
            ("10.0.0.0/08", "10.0.0.0/8"),
            ("10.0.0.0/0008", "10.0.0.0/8"),
            ("192.0.2.7", "192.0.2.7/32"),
            ("::/0", "::/0"),
            ("::/000128", "::/128"),
            ("::1.2.3.4", "::102:304/128"),
            ("1:2:3:4:5:6:7:8", "1:2:3:4:5:6:7:8/128"),
            ("1:2:3:4:5:6:7::", "1:2:3:4:5:6:7:0/128"),
            ("::2:3:4:5:6:7:8", "0:2:3:4:5:6:7:8/128"),
            ("1:2:3:4:5:6:1.2.3.4", "1:2:3:4:5:6:102:304/128"),
            ("1:2:3:4:5::1.2.3.4", "1:2:3:4:5:0:102:304/128"),
            ("0000::", "::/128"),
            ("::FFFF:1.2.3.4", "::ffff:1.2.3.4/128"),
            ("::ffff:c000:201/120", "::ffff:192.0.2.0/120"),
            ("::c000:200/120", "::c000:200/120"),
            ("::1.2.3.4/96", "::/96"),
            ("2001:DB8::/32", "2001:db8::/32"),
            ("::ffff:0:0/95", "::fffe:0:0/95"),
            ("1:0:0:2:0:0:0:3", "1:0:0:2::3/128"),
            ("0:0:1::", "0:0:1::/128"),
            ("::1:2:3:4:5:6:7", "0:1:2:3:4:5:6:7/128"),
            ("fe80::1/64", "fe80::/64"),
        ] {
            assert_eq!(network(text).as_deref(), Ok(expected), "{text}");
        }
        let long_prefix = format!("10.0.0.0/{}8", "0".repeat(4299));
        assert_eq!(network(&long_prefix).as_deref(), Ok("10.0.0.0/8"));
    }

    #[test]
    fn invalid_and_scoped_interfaces_are_refused() {
        for text in [
            "10.0.0.0/+8",
            "10.0.0.0/-8",
            " 10.0.0.0/8",
            "10.0.0.0/8 ",
            "10.0.0.0/",
            "10.0.0.0//8",
            "10.0.0.0/8/8",
            "010.0.0.0/8",
            "1.2.3",
            "1.2.3.4.5",
            "256.0.0.0",
            "1.2.3.4/33",
            "1.2.3.4/255.0.255.0",
            "1.2.3.4/255.255.255.000",
            "::/129",
            "fe80::1%/64",
            "fe80::1%a%b",
            "fe80::1%%",
            "1::2::3",
            ":1::2",
            "1::2:",
            "1:2:3:4:5:6:7:8:9",
            "1:2:3:4:5:6:7:1.2.3.4",
            "1:2:3:4:5:6::1.2.3.4",
            "00000::",
            "::ffff:1.2.3.04",
            "::1.2.3",
            "1.2.3.4/255.255.255.0x",
            "\u{FF11}.2.3.4",
            "::/12\u{FF18}",
            "0:0:0:0:0:0:0:0:0",
            ":::",
            "1:::2",
            "",
            "/8",
            "1.2.3.4/0x8",
            "1.2.3.4/8.0",
            "::/0x8",
            "1.2.3.4/\u{0668}",
            "fe80::1%eth0/129",
            "::1:2:3:4:5:6:7:8",
            "1:2:3:4:5:6:7:8:",
            ":1:2:3:4:5:6:7",
        ] {
            assert_eq!(network(text), Err(InterfaceError::Invalid), "{text:?}");
        }
        let long_prefix = format!("10.0.0.0/{}8", "0".repeat(4300));
        assert_eq!(network(&long_prefix), Err(InterfaceError::Invalid));
        for text in [
            "fe80::1%eth0/64",
            "fe80::%eth0/64",
            "fe80::1%1",
            "::ffff:192.0.2.1%eth0/120",
            "fe80::1%\u{FF11}",
            "1:2:3:4:5:6:7:8%x/128",
        ] {
            assert_eq!(network(text), Err(InterfaceError::Scoped), "{text:?}");
        }
    }

    #[test]
    fn ipv6_addresses_print_as_python_does() {
        for (address, expected) in [
            (0u128, "::/128"),
            (1, "::1/128"),
            (0xFFFF_0000_0000, "::ffff:0.0.0.0/128"),
            (0xFFFF_C000_0201, "::ffff:192.0.2.1/128"),
            (0xFFFE_C000_0201, "::fffe:c000:201/128"),
            (0x2001_0db8 << 96, "2001:db8::/128"),
            ((0x2001_0db8 << 96) | 1, "2001:db8::1/128"),
            // The longest run of zero groups becomes `::`, the first of two equally long runs,
            // and a single zero group stays.
            ((1 << 112) | (1 << 64) | 1, "1:0:0:1::1/128"),
            ((1 << 112) | (1 << 64) | (1 << 16) | 1, "1::1:0:0:1:1/128"),
            (
                (1 << 112) | (2 << 96) | (3 << 80) | (4 << 64) | (5 << 48) | (6 << 32) | (7 << 16),
                "1:2:3:4:5:6:7:0/128",
            ),
            (u128::MAX, "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/128"),
        ] {
            let network = Network::new(Family::V6, address, 128);
            assert_eq!(network.to_string(), expected, "{address:#x}");
        }
    }

    #[test]
    fn summaries_match_python() {
        let summary = |family, first, last| -> Vec<String> {
            summarize(family, (first, last))
                .map(|network| network.to_string())
                .collect()
        };
        let ipv4 = |text: &str| u128::from(u32::from(text.parse::<Ipv4Addr>().unwrap()));
        assert_eq!(
            summary(Family::V4, ipv4("192.0.2.0"), ipv4("192.0.2.130")),
            ["192.0.2.0/25", "192.0.2.128/31", "192.0.2.130/32"]
        );
        assert_eq!(summary(Family::V4, 0, u128::from(u32::MAX)), ["0.0.0.0/0"]);
        assert_eq!(
            summary(Family::V4, 1, u128::from(u32::MAX)).len(),
            32,
            "every power of two below 2^32"
        );
        assert_eq!(summary(Family::V6, 0, u128::MAX), ["::/0"]);
        assert_eq!(
            summary(Family::V6, u128::MAX, u128::MAX),
            ["ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/128"]
        );
        assert_eq!(summary(Family::V6, 1, u128::MAX).len(), 128);
        assert_eq!(summary(Family::V6, 1 << 127, u128::MAX), ["8000::/1"]);
    }

    #[test]
    fn intervals_merge_and_subtract_at_the_family_bounds() {
        assert_eq!(
            merge([(5, 9), (0, 2), (3, 4), (20, 30), (25, 26)]),
            [(0, 9), (20, 30)]
        );
        assert_eq!(merge([(0, u128::MAX), (7, 9)]), [(0, u128::MAX)]);
        assert_eq!(merge([(10, u128::MAX), (0, 9)]), [(0, u128::MAX)]);
        assert!(merge([]).is_empty());
        assert_eq!(
            subtract(&[(0, 100)], &[(10, 20), (30, 40)]),
            [(0, 9), (21, 29), (41, 100)]
        );
        assert_eq!(subtract(&[(0, u128::MAX)], &[(1, u128::MAX)]), [(0, 0)]);
        assert!(subtract(&[(5, u128::MAX)], &[(0, u128::MAX)]).is_empty());
        assert_eq!(
            subtract(&[(0, 9), (20, 29)], &[(5, 24)]),
            [(0, 4), (25, 29)]
        );
        assert_eq!(subtract(&[(0, 9)], &[]), [(0, 9)]);
    }

    #[test]
    fn containment_is_family_aware() {
        let parse = |text: &str| parse_interface(text).unwrap();
        assert!(parse("10.0.0.0/8").contains(parse("10.1.0.0/16")));
        assert!(!parse("10.0.0.0/8").contains(parse("11.0.0.0/16")));
        assert!(!parse("::/96").contains(parse("0.0.0.0/0")));
        assert!(!parse("0.0.0.0/0").contains(parse("::/96")));
        assert_eq!(parse("10.0.0.0/8").interval(), (0x0A00_0000, 0x0AFF_FFFF));
    }
}
