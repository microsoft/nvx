//! The lowering of egress rules into OpenVMM's native rules, which the egress policy compiler of
//! [`policy`](super::policy) and [`openvmm_arguments`](super::openvmm_arguments) share.
//!
//! It ports the former `scripts/nvx_tools/egress_policy.py` exactly. Rules of one action, allow
//! or deny, become [`Rule`]s: one address family, the address intervals that the rule's
//! destinations select once their exceptions are subtracted, and a protocol and port range.
//! [`lower`] then emits the native rules of each family, within one budget of
//! [`MAX_RULES_PER_ACTION`]:
//!
//! 1. the union of the rules for every protocol, as the fewest networks that cover it;
//! 2. for ICMP, TCP, and UDP, the union of the rules for every port of the protocol, as the
//!    networks that cover it together with the addresses of step 1, except those that step 1
//!    covers entirely;
//! 3. for TCP and UDP, a sweep over the ports at which rules start and end: each segment of
//!    ports with the same active rules gets the networks of their union, without the addresses
//!    that steps 1 and 2 cover, and a network whose addresses stay active extends its range.
//!
//! The native rules are then sorted by family, address, prefix length, protocol, and ports, and
//! written as `CIDR`, `CIDR:PROTOCOL`, `CIDR:PROTOCOL:PORT`, or `CIDR:PROTOCOL:FIRST-LAST`.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use super::address::{self, Family, InterfaceError, Interval, Network};
use super::policy::MAX_RULES_PER_ACTION;
use crate::error::{Error, Result};

/// A protocol that a native rule can select.
///
/// Protocols order by name, as the compiler sorts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum NativeProtocol {
    /// ICMP, or ICMPv6 for IPv6 networks.
    Icmp,
    /// TCP.
    Tcp,
    /// UDP.
    Udp,
}

impl NativeProtocol {
    /// Returns OpenVMM's name of the protocol.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Icmp => "icmp",
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// The protocol of a rule as written: a native protocol, or `any`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuleProtocol {
    /// TCP.
    Tcp,
    /// UDP.
    Udp,
    /// ICMP.
    Icmp,
    /// Every protocol, or TCP and UDP with ports.
    Any,
}

impl RuleProtocol {
    /// Parses one of `tcp`, `udp`, `icmp`, and `any`, which must be lowercase.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "tcp" => Some(Self::Tcp),
            "udp" => Some(Self::Udp),
            "icmp" => Some(Self::Icmp),
            "any" => Some(Self::Any),
            _ => None,
        }
    }

    /// Returns the native protocol, or `None` for `any`.
    pub(crate) fn native(self) -> Option<NativeProtocol> {
        match self {
            Self::Tcp => Some(NativeProtocol::Tcp),
            Self::Udp => Some(NativeProtocol::Udp),
            Self::Icmp => Some(NativeProtocol::Icmp),
            Self::Any => None,
        }
    }

    /// Returns the native protocols that a port selector of this protocol selects: TCP and UDP
    /// for `any`.
    fn port_protocols(self) -> &'static [NativeProtocol] {
        match self {
            Self::Tcp => &[NativeProtocol::Tcp],
            Self::Udp => &[NativeProtocol::Udp],
            Self::Icmp => &[NativeProtocol::Icmp],
            Self::Any => &[NativeProtocol::Tcp, NativeProtocol::Udp],
        }
    }
}

/// An inclusive range of ports.
pub(crate) type PortRange = (u16, u16);

/// Traffic that a rule selects at its destinations: a protocol, or every protocol, and a range of
/// its ports, or every port.
pub(crate) type Selector = (Option<NativeProtocol>, Option<PortRange>);

/// A value that should be a port number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PortValue {
    /// An integer, saturated to the range of `i128`.
    Integer(i128),
    /// Anything else, including a Boolean, which Python's `_port` refuses although `bool` is a
    /// subclass of `int`.
    Other,
}

/// A rule of one action, as the compiler lowers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rule {
    /// Address family of the rule's addresses.
    pub(crate) family: Family,
    /// Addresses that the rule selects: sorted, disjoint intervals of `family`, possibly none.
    ///
    /// The rules of one rule's selectors share them, as Python's share one tuple, so that many
    /// destinations times many selectors cost no more memory than they take to write.
    pub(crate) addresses: Rc<[Interval]>,
    /// Protocol that the rule selects, or `None` for every protocol.
    pub(crate) protocol: Option<NativeProtocol>,
    /// Ports that the rule selects, or `None` for every port. Only TCP and UDP rules have ports.
    pub(crate) ports: Option<PortRange>,
}

/// Returns an [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) error with `message`.
pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::unsupported(message)
}

/// Parses the CIDR `value` of the field `description`, as `_network` does: the network of an
/// IPv4 or IPv6 interface without an IPv6 scope.
pub(crate) fn network(value: &str, description: &str) -> Result<Network> {
    address::parse_interface(value).map_err(|error| {
        invalid(match error {
            InterfaceError::Invalid => {
                format!("{description} is not a valid IPv4 or IPv6 CIDR: {value:?}")
            }
            InterfaceError::Scoped => {
                format!("{description} must not name an IPv6 scope: {value:?}")
            }
        })
    })
}

/// Returns the error for a CIDR field whose value is not a string.
pub(crate) fn not_a_cidr_string(description: &str) -> Error {
    invalid(format!("{description} must be an IPv4 or IPv6 CIDR string"))
}

/// Returns the family of the destination `description`, whose network is `parent`, and the
/// addresses that it holds outside `exceptions`, as `_subtract_exclusions` does. Each exception,
/// which is parsed when it is read, must lie within `parent`.
pub(crate) fn destination(
    parent: Network,
    exceptions: impl IntoIterator<Item = Result<Network>>,
    description: &str,
) -> Result<(Family, Vec<Interval>)> {
    let mut excluded = Vec::new();
    for (index, exception) in exceptions.into_iter().enumerate() {
        let exception = exception?;
        if !parent.contains(exception) {
            return Err(invalid(format!(
                "{description}.except[{index}] must be contained in {parent}"
            )));
        }
        excluded.push(exception.interval());
    }
    Ok((
        parent.family(),
        address::subtract(&[parent.interval()], &address::merge(excluded)),
    ))
}

/// Returns the addresses of each family that `destinations`, each a family and its addresses,
/// select, for the families that have destinations, as an MXC rule's `to` selects them.
pub(crate) fn group_destinations(
    destinations: &[(Family, Vec<Interval>)],
) -> Vec<(Family, Vec<Interval>)> {
    Family::ALL
        .into_iter()
        .filter(|&family| destinations.iter().any(|(each, _)| *each == family))
        .map(|family| {
            let addresses = address::merge(
                destinations
                    .iter()
                    .filter(|(each, _)| *each == family)
                    .flat_map(|(_, addresses)| addresses.iter().copied()),
            );
            (family, addresses)
        })
        .collect()
}

/// Checks the port `value` of the field `description`, as `_port` does.
fn port(value: PortValue, description: &str) -> Result<u16> {
    match value {
        PortValue::Other => Err(invalid(format!("{description} must be an integer"))),
        PortValue::Integer(port) => u16::try_from(port)
            .ok()
            .filter(|&port| port != 0)
            .ok_or_else(|| invalid(format!("{description} must be between 1 and 65535"))),
    }
}

/// Returns the range of the `port` and optional `endPort` fields of the selector
/// `description`, which ends at `port` without `endPort`.
pub(crate) fn port_range(
    start: PortValue,
    end: Option<PortValue>,
    description: &str,
) -> Result<PortRange> {
    let start = port(start, &format!("{description}.port"))?;
    let end = match end {
        Some(end) => port(end, &format!("{description}.endPort"))?,
        None => start,
    };
    if end < start {
        return Err(invalid(format!(
            "{description}.endPort {end} cannot be below port {start}"
        )));
    }
    Ok((start, end))
}

/// Returns the selectors of the port fields, `port` and the optional `endPort`, of
/// `description`, an MXC port selector or a legacy flat rule whose protocol is valid, as the rest
/// of `_parse_mxc_port` does. ICMP takes no port.
pub(crate) fn port_selectors(
    protocol: RuleProtocol,
    port: Option<PortValue>,
    end_port: Option<PortValue>,
    description: &str,
) -> Result<Vec<Selector>> {
    let Some(port) = port else {
        if end_port.is_some() {
            return Err(invalid(format!("{description}.endPort requires port")));
        }
        return Ok(vec![(protocol.native(), None)]);
    };
    if protocol == RuleProtocol::Icmp {
        return Err(invalid(format!(
            "{description}.port is not supported with icmp"
        )));
    }
    let range = port_range(port, end_port, description)?;
    Ok(protocol
        .port_protocols()
        .iter()
        .map(|&protocol| (Some(protocol), Some(range)))
        .collect())
}

/// Returns the rules that select each of `selectors` at the addresses of each family of
/// `destinations`, which the rules share, as `_parse_mxc_rule` and `_parse_legacy_rule` build
/// them. Without destinations, the rules select every address of both families, and without
/// selectors every protocol and port.
pub(crate) fn combine(
    destinations: Option<Vec<(Family, Vec<Interval>)>>,
    selectors: Option<Vec<Selector>>,
) -> Vec<Rule> {
    let destinations: Vec<(Family, Rc<[Interval]>)> = match destinations {
        Some(destinations) => destinations
            .into_iter()
            .map(|(family, addresses)| (family, Rc::from(addresses)))
            .collect(),
        None => Family::ALL
            .into_iter()
            .map(|family| (family, Rc::from([family.everything()])))
            .collect(),
    };
    let selectors = selectors.unwrap_or_else(|| vec![(None, None)]);
    destinations
        .iter()
        .flat_map(|(family, addresses)| {
            selectors.iter().map(|&(protocol, ports)| Rule {
                family: *family,
                addresses: Rc::clone(addresses),
                protocol,
                ports,
            })
        })
        .collect()
}

/// A native rule: a network, and optionally a protocol and a range of its ports. Native rules
/// order as the compiler sorts them.
type NativeRule = (Network, Option<NativeProtocol>, Option<PortRange>);

/// Returns the error of an action `category` whose rules need more native rules than OpenVMM
/// accepts.
fn too_many_rules(category: &str) -> Error {
    invalid(format!(
        "{category} emits at most {MAX_RULES_PER_ACTION} native rules"
    ))
}

/// Lowers the rules of the action `category` into native rules, in the order in which the
/// compiler sorts them, as `_compile_category` does once it has parsed them.
pub(crate) fn lower(rules: &[Rule], category: &str) -> Result<Vec<String>> {
    lower_within(rules, category, MAX_RULES_PER_ACTION)
}

/// Lowers the rules of the action `category` as [`lower`] does, but fails once they need more
/// than `limit` native rules rather than [`MAX_RULES_PER_ACTION`]: tests lift the limit to learn
/// how many native rules an action needs.
pub(crate) fn lower_within(rules: &[Rule], category: &str, limit: usize) -> Result<Vec<String>> {
    // A native rule matches only its own family, so each family is lowered on its own, within
    // the action's shared limit.
    let mut lowered: Vec<NativeRule> = Vec::new();
    for family in Family::ALL {
        let family_rules: Vec<&Rule> = rules.iter().filter(|rule| rule.family == family).collect();
        if !family_rules.is_empty() {
            let budget = limit - lowered.len();
            let family_lowered = lower_family(&family_rules, family, category, budget)?;
            lowered.extend(family_lowered);
        }
    }
    lowered.sort_unstable();
    Ok(lowered.into_iter().map(render).collect())
}

/// Writes a native rule in OpenVMM's form.
fn render((network, protocol, ports): NativeRule) -> String {
    match (protocol, ports) {
        (None, _) => network.to_string(),
        (Some(protocol), None) => format!("{network}:{}", protocol.as_str()),
        (Some(protocol), Some((start, end))) if start == end => {
            format!("{network}:{}:{start}", protocol.as_str())
        }
        (Some(protocol), Some((start, end))) => {
            format!("{network}:{}:{start}-{end}", protocol.as_str())
        }
    }
}

/// Lowers the rules of one family, emitting at most `budget` native rules, as `_compile_family`
/// does.
fn lower_family(
    rules: &[&Rule],
    family: Family,
    category: &str,
    budget: usize,
) -> Result<Vec<NativeRule>> {
    let address_only = union(rules.iter().copied().filter(|rule| rule.protocol.is_none()));
    let mut lowered: Vec<NativeRule> =
        intervals_to_networks(&address_only, family, budget, category)?
            .into_iter()
            .map(|network| (network, None, None))
            .collect();
    // Addresses that already match every port of each protocol.
    let mut covered: BTreeMap<NativeProtocol, Vec<Interval>> = BTreeMap::new();
    for protocol in [
        NativeProtocol::Icmp,
        NativeProtocol::Tcp,
        NativeProtocol::Udp,
    ] {
        let protocol_wide = union(
            rules
                .iter()
                .copied()
                .filter(|rule| rule.protocol == Some(protocol) && rule.ports.is_none()),
        );
        if protocol_wide.is_empty() {
            continue;
        }
        let networks = protocol_intervals_to_networks(
            &protocol_wide,
            &address_only,
            family,
            budget - lowered.len(),
            category,
        )?;
        lowered.extend(
            networks
                .into_iter()
                .map(|network| (network, Some(protocol), None)),
        );
        covered.insert(
            protocol,
            address::merge(address_only.iter().chain(&protocol_wide).copied()),
        );
    }
    for protocol in [NativeProtocol::Tcp, NativeProtocol::Udp] {
        let covered = covered.get(&protocol).unwrap_or(&address_only);
        let port_rules = lower_port_rules(
            rules,
            protocol,
            family,
            category,
            covered,
            budget - lowered.len(),
        )?;
        lowered.extend(port_rules);
    }
    Ok(lowered)
}

/// Returns the union of the addresses of `rules`, as `_compile_family` merges them, reading the
/// addresses that rules share once: the rules of one rule's selectors would otherwise repeat
/// them once for each selector.
fn union<'a>(rules: impl IntoIterator<Item = &'a Rule>) -> Vec<Interval> {
    let sets: BTreeSet<AddressSet<'a>> = rules
        .into_iter()
        .map(|rule| AddressSet(&rule.addresses))
        .collect();
    address::merge(sets.into_iter().flat_map(|set| set.0.iter().copied()))
}

/// Returns the networks that cover `intervals`, failing once they exceed `maximum`, as
/// `_intervals_to_networks` does.
fn intervals_to_networks(
    intervals: &[Interval],
    family: Family,
    maximum: usize,
    category: &str,
) -> Result<Vec<Network>> {
    let mut networks = Vec::new();
    for &interval in intervals {
        for network in address::summarize(family, interval) {
            if networks.len() >= maximum {
                return Err(too_many_rules(category));
            }
            networks.push(network);
        }
    }
    Ok(networks)
}

/// Returns the networks that cover `protocol_intervals` without those that `covered` holds
/// entirely, failing once they exceed `maximum`, as `_protocol_intervals_to_networks` does.
///
/// Rules over `covered` already match the traffic, so widening networks into it keeps the result
/// compact without changing what matches.
fn protocol_intervals_to_networks(
    protocol_intervals: &[Interval],
    covered: &[Interval],
    family: Family,
    maximum: usize,
    category: &str,
) -> Result<Vec<Network>> {
    let combined = address::merge(protocol_intervals.iter().chain(covered).copied());
    let mut networks = Vec::new();
    for interval in combined {
        for network in address::summarize(family, interval) {
            if address::subtract(&[network.interval()], covered).is_empty() {
                continue;
            }
            if networks.len() >= maximum {
                return Err(too_many_rules(category));
            }
            networks.push(network);
        }
    }
    Ok(networks)
}

/// Lowers the rules of one family for the ports of `protocol`, TCP or UDP, emitting at most
/// `budget` native rules, as `_lower_protocol_rules` does.
///
/// Rules start and end at port boundaries. Between two consecutive boundaries, the active rules
/// select the union of their addresses; the networks of the previous segment whose addresses stay
/// selected extend their ranges, and new networks cover the rest of the union, without the
/// addresses that `covered` already matches on every port.
fn lower_port_rules(
    rules: &[&Rule],
    protocol: NativeProtocol,
    family: Family,
    category: &str,
    covered: &[Interval],
    budget: usize,
) -> Result<Vec<NativeRule>> {
    // Each boundary port, and the address sets that start (+1) or end (-1) there. Ports run to
    // 65536, where rules that end at port 65535 end.
    let mut events: BTreeMap<u32, Vec<(i64, AddressSet<'_>)>> = BTreeMap::new();
    // Whether each address set holds addresses that `covered` does not, which rules that share
    // their addresses ask once.
    let mut uncovered: BTreeMap<AddressSet<'_>, bool> = BTreeMap::new();
    for rule in rules {
        let Some((start, end)) = rule.ports else {
            continue;
        };
        if rule.protocol != Some(protocol) || rule.addresses.is_empty() {
            continue;
        }
        let addresses = AddressSet(&rule.addresses);
        if !*uncovered
            .entry(addresses)
            .or_insert_with(|| !address::subtract(addresses.0, covered).is_empty())
        {
            continue;
        }
        events
            .entry(u32::from(start))
            .or_default()
            .push((1, addresses));
        events
            .entry(u32::from(end) + 1)
            .or_default()
            .push((-1, addresses));
    }

    // The number of active rules with each address set.
    let mut active: BTreeMap<AddressSet<'_>, i64> = BTreeMap::new();
    let mut port_ranges: BTreeMap<Network, Vec<(u32, u32)>> = BTreeMap::new();
    // Networks whose last range ends just before the current segment.
    let mut open_networks: Vec<Network> = Vec::new();
    let mut rule_count = 0;
    let mut previous_port: Option<u32> = None;
    for (&port, port_events) in &events {
        if previous_port.is_some() && active.is_empty() {
            open_networks.clear();
        }
        if let Some(previous) = previous_port
            && previous < port
            && !active.is_empty()
        {
            let addresses = address::merge(
                active
                    .keys()
                    .flat_map(|addresses| addresses.0.iter().copied()),
            );
            let available = address::merge(addresses.iter().chain(covered).copied());
            let mut kept: Vec<Network> = Vec::new();
            for &network in &open_networks {
                if address::subtract(&[network.interval()], &available).is_empty()
                    && let Some(range) = port_ranges
                        .get_mut(&network)
                        .and_then(|ranges| ranges.last_mut())
                {
                    range.1 = port - 1;
                    kept.push(network);
                }
            }
            let kept_cover = address::merge(
                covered
                    .iter()
                    .copied()
                    .chain(kept.iter().map(|network| network.interval())),
            );
            let networks = protocol_intervals_to_networks(
                &addresses,
                &kept_cover,
                family,
                budget - rule_count,
                category,
            )?;
            for network in networks {
                if rule_count >= budget {
                    return Err(too_many_rules(category));
                }
                port_ranges
                    .entry(network)
                    .or_default()
                    .push((previous, port - 1));
                rule_count += 1;
                kept.push(network);
            }
            open_networks = kept;
        }
        for &(direction, addresses) in port_events {
            let count = active.entry(addresses).or_insert(0);
            *count += direction;
            if *count == 0 {
                active.remove(&addresses);
            }
        }
        previous_port = Some(port);
    }
    Ok(port_ranges
        .into_iter()
        .flat_map(|(network, ranges)| {
            ranges.into_iter().map(move |(start, end)| {
                // Segments end before a boundary of at most 65536, so their ports fit.
                let range = (start as u16, end as u16);
                (network, Some(protocol), Some(range))
            })
        })
        .collect())
}

/// The addresses of a rule, which compare by value, as Python's tuples do: the port sweep counts
/// the active rules of each address set, as `_lower_protocol_rules` counts them in a `Counter`.
/// Like CPython's dictionaries, it compares identical addresses without reading them, so that the
/// rules of one rule's selectors, which share their addresses, compare at once.
#[derive(Debug, Clone, Copy)]
struct AddressSet<'a>(&'a [Interval]);

impl PartialEq for AddressSet<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for AddressSet<'_> {}

impl PartialOrd for AddressSet<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AddressSet<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        if std::ptr::eq(self.0, other.0) {
            Ordering::Equal
        } else {
            self.0.cmp(other.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rules_of_one_rule_share_its_addresses() {
        let ipv4: Vec<Interval> = vec![(1, 1), (3, 3)];
        let ipv6: Vec<Interval> = vec![(5, 6)];
        let selectors: Vec<Selector> = vec![
            (Some(NativeProtocol::Tcp), Some((80, 80))),
            (Some(NativeProtocol::Udp), Some((53, 60))),
            (None, None),
        ];
        let rules = combine(
            Some(vec![(Family::V4, ipv4.clone()), (Family::V6, ipv6.clone())]),
            Some(selectors.clone()),
        );
        assert_eq!(rules.len(), 6);
        for (family_rules, (family, addresses)) in rules
            .chunks(selectors.len())
            .zip([(Family::V4, &ipv4), (Family::V6, &ipv6)])
        {
            for (rule, &(protocol, ports)) in family_rules.iter().zip(&selectors) {
                assert_eq!(rule.family, family);
                assert_eq!(*rule.addresses, **addresses);
                assert_eq!((rule.protocol, rule.ports), (protocol, ports));
                assert!(Rc::ptr_eq(&rule.addresses, &family_rules[0].addresses));
            }
            assert_eq!(
                Rc::strong_count(&family_rules[0].addresses),
                selectors.len()
            );
        }

        let rules = combine(None, Some(selectors.clone()));
        assert_eq!(rules.len(), 6);
        for (family_rules, family) in rules.chunks(selectors.len()).zip(Family::ALL) {
            for rule in family_rules {
                assert_eq!(rule.family, family);
                assert_eq!(*rule.addresses, [family.everything()]);
                assert!(Rc::ptr_eq(&rule.addresses, &family_rules[0].addresses));
            }
        }

        let rules = combine(Some(vec![(Family::V6, ipv6.clone())]), None);
        assert_eq!(
            rules,
            [Rule {
                family: Family::V6,
                addresses: Rc::from(ipv6),
                protocol: None,
                ports: None,
            }]
        );
    }

    #[test]
    fn address_sets_compare_by_value() {
        let addresses: Rc<[Interval]> = Rc::from([(1, 2), (4, 4)]);
        let copy: Rc<[Interval]> = Rc::from(&*addresses);
        let longer: Rc<[Interval]> = Rc::from([(1, 2), (4, 4), (6, 6)]);
        let later: Rc<[Interval]> = Rc::from([(1, 3)]);
        let empty: Rc<[Interval]> = Rc::from([]);
        assert_eq!(AddressSet(&addresses), AddressSet(&addresses));
        assert_eq!(AddressSet(&addresses), AddressSet(&copy));
        assert!(AddressSet(&empty) < AddressSet(&addresses));
        assert!(AddressSet(&addresses) < AddressSet(&longer));
        assert!(AddressSet(&longer) < AddressSet(&later));
        let sets: BTreeSet<AddressSet<'_>> = [&later, &addresses, &copy, &empty, &longer]
            .into_iter()
            .map(|addresses| AddressSet(addresses))
            .collect();
        let sorted: Vec<&[Interval]> = sets.iter().map(|set| set.0).collect();
        assert_eq!(sorted, [&*empty, &*addresses, &*longer, &*later]);
    }

    #[test]
    fn shared_addresses_are_merged_once() {
        let addresses: Vec<Interval> = (0..64).map(|index| (2 * index, 2 * index)).collect();
        // The union of the 64 rules for every protocol reads their addresses once; for ICMP, so
        // do the cover of its networks and the addresses that it covers. Python merges the
        // addresses of each rule into the union: 64 times as many.
        for (protocol, merged) in [(None, 64), (Some(NativeProtocol::Icmp), 3 * 64)] {
            let rules = combine(
                Some(vec![(Family::V4, addresses.clone())]),
                Some(vec![(protocol, None); 64]),
            );
            assert_eq!(rules.len(), 64);
            let (lowered, _, merged_intervals) = address::probe::count(|| lower(&rules, "allow"));
            assert_eq!(lowered.unwrap().len(), 64, "{protocol:?}");
            assert_eq!(merged_intervals, merged, "{protocol:?}");
        }
    }
}
