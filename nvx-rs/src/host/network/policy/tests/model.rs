//! A reference model of egress policies, which the properties of the compiler compare it with.
//!
//! The model is written from the policy format that `doc/run.md` documents, and from what the
//! documentation of [`policy`](super::super) adds about CIDRs, and it shares no code with the
//! compiler: it reads JSON with `serde_json` and addresses with the standard library.
//!
//! - [`read_file`] and [`read_policy`] read a policy file as the format describes it, and refuse
//!   what the format refuses.
//! - [`Rule::matches`] decides whether a rule matches a [`Packet`]: an address family, a
//!   destination address, a protocol, and for TCP and UDP a destination port.
//! - [`Native::parse`] reads a native rule, `CIDR[:PROTOCOL[:PORT[-LAST]]]`, which
//!   [`Native::matches`] matches as `doc/run.md` describes OpenVMM's rules.
//! - [`difference`] compares the rules of an action with the native rules that it lowers to,
//!   exactly; [`redundant`] finds a native rule that the others cover; and [`not_canonical`]
//!   checks how native rules are written and sorted.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

use serde::de::{DeserializeSeed, Deserializer, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

/// Largest policy file, in bytes: 1 MiB.
pub(super) const MAX_FILE_SIZE: usize = 1024 * 1024;

/// Most arrays and objects that a policy file nests, the root included.
const MAX_DEPTH: usize = 32;

/// Most digits of a prefix length, leading zeros included, which Python's `int` reads by default.
const MAX_PREFIX_DIGITS: usize = 4300;

/// Address family of a network or a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Family {
    /// IPv4.
    V4,
    /// IPv6.
    V6,
}

impl Family {
    /// Both families.
    pub(super) const ALL: [Self; 2] = [Self::V4, Self::V6];

    /// Returns the number of bits of an address.
    pub(super) const fn bits(self) -> u8 {
        match self {
            Self::V4 => 32,
            Self::V6 => 128,
        }
    }

    /// Returns the last address, whose bits are all set.
    pub(super) const fn last(self) -> u128 {
        match self {
            Self::V4 => u32::MAX as u128,
            Self::V6 => u128::MAX,
        }
    }
}

/// A network: the addresses of one family that share their first `prefix` bits.
///
/// Networks order by family, address, and prefix length, as compiled rules sort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct Net {
    /// Address family.
    pub(super) family: Family,
    /// First address, without host bits.
    pub(super) address: u128,
    /// Prefix length, at most the family's number of bits.
    pub(super) prefix: u8,
}

/// Why [`Net::parse`] refuses a CIDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CidrError {
    /// The text is not an IPv4 or IPv6 network.
    Invalid,
    /// The text is an IPv6 network that names a scope, such as `fe80::1%eth0/64`.
    Scoped,
}

impl Net {
    /// Returns the network of `family` with `prefix` that holds `address`, without host bits.
    pub(super) fn new(family: Family, address: u128, prefix: u8) -> Self {
        assert!(
            prefix <= family.bits(),
            "/{prefix} is too long for {family:?}"
        );
        let mut network = Self {
            family,
            address,
            prefix,
        };
        network.address &= !network.host_mask();
        network
    }

    /// Returns the host bits of the network's addresses.
    fn host_mask(self) -> u128 {
        if self.prefix == self.family.bits() {
            0
        } else {
            self.family.last() >> self.prefix
        }
    }

    /// Returns the first address.
    pub(super) fn first(self) -> u128 {
        self.address
    }

    /// Returns the last address.
    pub(super) fn last(self) -> u128 {
        self.address | self.host_mask()
    }

    /// Returns whether the network holds `address` of `family`.
    pub(super) fn holds(self, family: Family, address: u128) -> bool {
        self.family == family && self.first() <= address && address <= self.last()
    }

    /// Returns whether every address of `other` lies within the network.
    pub(super) fn contains(self, other: Self) -> bool {
        self.family == other.family && self.first() <= other.first() && other.last() <= self.last()
    }

    /// Returns whether the network and `other` share an address.
    pub(super) fn overlaps(self, other: Self) -> bool {
        self.family == other.family && self.first() <= other.last() && other.first() <= self.last()
    }

    /// Returns the two halves of the network, unless it holds one address.
    pub(super) fn halves(self) -> Option<[Self; 2]> {
        let prefix = self.prefix + 1;
        (prefix <= self.family.bits()).then(|| {
            let upper = 1u128 << (self.family.bits() - prefix);
            [
                Self::new(self.family, self.address, prefix),
                Self::new(self.family, self.address | upper, prefix),
            ]
        })
    }

    /// Parses a CIDR as the format reads it: an IPv4 or IPv6 address, whose host bits are
    /// cleared, and an optional prefix length of decimal digits, which may have leading zeros, or
    /// for IPv4 a netmask, such as `255.255.0.0`, or a hostmask, such as `0.0.255.255`. An IPv6
    /// address may not name a scope.
    pub(super) fn parse(text: &str) -> Result<Self, CidrError> {
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text, None),
        };
        if let Ok(address) = address.parse::<Ipv4Addr>() {
            let prefix = match prefix {
                None => 32,
                Some(prefix) => prefix_length(prefix, 32)
                    .or_else(|| ipv4_mask(prefix))
                    .ok_or(CidrError::Invalid)?,
            };
            return Ok(Self::new(Family::V4, u32::from(address).into(), prefix));
        }
        // A scope follows the first `%`, and is neither empty nor holds another `%`.
        let (address, scoped) = match address.split_once('%') {
            None => (address, false),
            Some((_, scope)) if scope.is_empty() || scope.contains('%') => {
                return Err(CidrError::Invalid);
            }
            Some((address, _)) => (address, true),
        };
        let address: Ipv6Addr = address.parse().map_err(|_| CidrError::Invalid)?;
        let prefix = match prefix {
            None => 128,
            Some(prefix) => prefix_length(prefix, 128).ok_or(CidrError::Invalid)?,
        };
        if scoped {
            return Err(CidrError::Scoped);
        }
        Ok(Self::new(Family::V6, address.into(), prefix))
    }
}

/// Returns the prefix length that `text` writes in decimal digits, leading zeros included, if it
/// is at most `maximum`.
fn prefix_length(text: &str, maximum: u8) -> Option<u8> {
    let digits = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !digits || text.len() > MAX_PREFIX_DIGITS {
        return None;
    }
    let significant = text.trim_start_matches('0');
    if significant.len() > 3 {
        return None;
    }
    let prefix: u16 = significant.parse().unwrap_or(0);
    u8::try_from(prefix)
        .ok()
        .filter(|&prefix| prefix <= maximum)
}

/// Returns the prefix length of an IPv4 netmask or, failing that, of an IPv4 hostmask.
fn ipv4_mask(text: &str) -> Option<u8> {
    let mask = u32::from(text.parse::<Ipv4Addr>().ok()?);
    let netmask = |mask: u32| (mask.leading_ones() + mask.trailing_zeros() >= 32).then_some(mask);
    let prefix = netmask(mask).or_else(|| netmask(!mask))?.leading_ones();
    u8::try_from(prefix).ok()
}

impl fmt::Display for Net {
    /// Writes the network as compiled rules write it: the address as the standard library writes
    /// it, which agrees with Python 3.13 and later, and the prefix length.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.family {
            Family::V4 => write!(formatter, "{}", Ipv4Addr::from(self.address as u32))?,
            Family::V6 => write!(formatter, "{}", Ipv6Addr::from(self.address))?,
        }
        write!(formatter, "/{}", self.prefix)
    }
}

/// The protocol of a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Protocol {
    /// TCP.
    Tcp,
    /// UDP.
    Udp,
    /// ICMP, or ICMPv6 for IPv6 packets.
    Icmp,
    /// Any other protocol.
    Other,
}

impl Protocol {
    /// Every protocol.
    const ALL: [Self; 4] = [Self::Tcp, Self::Udp, Self::Icmp, Self::Other];

    /// Returns whether the protocol has ports.
    fn has_ports(self) -> bool {
        matches!(self, Self::Tcp | Self::Udp)
    }
}

/// A packet that the guest sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Packet {
    /// Address family.
    pub(super) family: Family,
    /// Destination address.
    pub(super) address: u128,
    /// Protocol.
    pub(super) protocol: Protocol,
    /// Destination port, for TCP and UDP; 0 otherwise.
    pub(super) port: u16,
}

impl fmt::Display for Packet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let address = Net::new(self.family, self.address, self.family.bits());
        write!(formatter, "{:?} to {address}", self.protocol)?;
        if self.protocol.has_ports() {
            write!(formatter, " port {}", self.port)?;
        }
        Ok(())
    }
}

/// The protocol that a port selector or a flat rule names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SelectorProtocol {
    /// `tcp`.
    Tcp,
    /// `udp`.
    Udp,
    /// `icmp`.
    Icmp,
    /// `any`: every protocol, or with ports TCP and UDP.
    Any,
}

/// Traffic that a port selector selects at a rule's destinations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Selector {
    /// The protocol.
    pub(super) protocol: SelectorProtocol,
    /// The first and last ports, or `None` for every port. Only TCP and UDP have ports.
    pub(super) ports: Option<(u16, u16)>,
}

impl Selector {
    /// Returns whether the selector selects traffic of `protocol` to `port`.
    fn selects(self, protocol: Protocol, port: u16) -> bool {
        let in_range = self
            .ports
            .is_none_or(|(first, last)| (first..=last).contains(&port));
        match (self.protocol, protocol) {
            (SelectorProtocol::Any, _) if self.ports.is_none() => true,
            (SelectorProtocol::Any, Protocol::Tcp | Protocol::Udp)
            | (SelectorProtocol::Tcp, Protocol::Tcp)
            | (SelectorProtocol::Udp, Protocol::Udp) => in_range,
            (SelectorProtocol::Icmp, Protocol::Icmp) => true,
            _ => false,
        }
    }
}

/// A destination of a rule: a network without the networks of its exceptions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Peer {
    /// The network.
    pub(super) network: Net,
    /// Networks within `network` that the destination leaves out.
    pub(super) except: Vec<Net>,
}

impl Peer {
    /// Returns whether the destination holds `address` of `family`.
    fn holds(&self, family: Family, address: u128) -> bool {
        self.network.holds(family, address)
            && !self
                .except
                .iter()
                .any(|exception| exception.holds(family, address))
    }
}

/// A rule of an action, in either form: a flat rule is the MXC rule with its one destination and,
/// if it names a protocol, its one port selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Rule {
    /// Destinations, or `None` for every address of both families.
    pub(super) to: Option<Vec<Peer>>,
    /// Port selectors, or `None` for every protocol and port.
    pub(super) ports: Option<Vec<Selector>>,
}

impl Rule {
    /// Returns whether the rule's destinations hold `address` of `family`.
    fn holds(&self, family: Family, address: u128) -> bool {
        self.to
            .as_ref()
            .is_none_or(|peers| peers.iter().any(|peer| peer.holds(family, address)))
    }

    /// Returns whether the rule's selectors select traffic of `protocol` to `port`.
    fn selects(&self, protocol: Protocol, port: u16) -> bool {
        self.ports.as_ref().is_none_or(|selectors| {
            selectors
                .iter()
                .any(|selector| selector.selects(protocol, port))
        })
    }

    /// Returns whether the rule matches `packet`.
    pub(super) fn matches(&self, packet: Packet) -> bool {
        self.holds(packet.family, packet.address) && self.selects(packet.protocol, packet.port)
    }
}

/// A policy: the rules of each action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Policy {
    /// Rules of traffic to permit.
    pub(super) allow: Vec<Rule>,
    /// Rules of traffic to block.
    pub(super) deny: Vec<Rule>,
}

impl Policy {
    /// Returns the rules of the action `category`, `allow` or `deny`.
    pub(super) fn action(&self, category: &str) -> &[Rule] {
        match category {
            "allow" => &self.allow,
            "deny" => &self.deny,
            _ => panic!("unknown action {category}"),
        }
    }
}

/// Reads `data` as the JSON of a policy file: at most [`MAX_FILE_SIZE`] bytes of UTF-8 JSON, as
/// RFC 8259 defines it and `serde_json` reads it, without a byte-order mark, a repeated field in
/// any object, or arrays and objects nested more than [`MAX_DEPTH`] deep. Returns `None` for
/// anything else.
pub(super) fn read_file(data: &[u8]) -> Option<Value> {
    if data.len() > MAX_FILE_SIZE {
        return None;
    }
    let text = std::str::from_utf8(data).ok()?;
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = UniqueFields.deserialize(&mut deserializer).ok()?;
    deserializer.end().ok()?;
    (depth(&value) <= MAX_DEPTH).then_some(value)
}

/// Returns how deep arrays and objects nest in `value`, itself included.
fn depth(value: &Value) -> usize {
    match value {
        Value::Array(elements) => 1 + elements.iter().map(depth).max().unwrap_or(0),
        Value::Object(fields) => 1 + fields.values().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// Reads a JSON value as `serde_json` reads a [`Value`], but fails at an object that repeats a
/// field.
struct UniqueFields;

impl<'de> DeserializeSeed<'de> for UniqueFields {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for UniqueFields {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_f64<E>(self, value: f64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut elements: A) -> Result<Value, A::Error> {
        let mut array = Vec::new();
        while let Some(element) = elements.next_element_seed(UniqueFields)? {
            array.push(element);
        }
        Ok(Value::Array(array))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut members: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(name) = members.next_key::<String>()? {
            if object.contains_key(&name) {
                return Err(A::Error::custom(format!("repeated field {name:?}")));
            }
            let value = members.next_value_seed(UniqueFields)?;
            object.insert(name, value);
        }
        Ok(Value::Object(object))
    }
}

/// Fields of a flat rule.
pub(super) const FLAT_FIELDS: [&str; 5] = ["cidr", "except", "protocol", "port", "endPort"];
/// Fields of an MXC rule.
const MXC_FIELDS: [&str; 2] = ["to", "ports"];
/// Fields of a destination of an MXC rule.
const PEER_FIELDS: [&str; 2] = ["cidr", "except"];
/// Fields of a port selector of an MXC rule.
const SELECTOR_FIELDS: [&str; 3] = ["protocol", "port", "endPort"];

/// Reads a policy as the format describes it, or returns `None` for a policy that it refuses:
///
/// - the root is an object whose only fields are `allow` and `deny`, each an array of rules;
/// - a rule with a field of the flat form, `cidr`, `except`, `protocol`, `port`, or `endPort`, is
///   a flat rule, which has no other field, needs `cidr`, and needs `protocol` for `port`;
/// - any other rule is an MXC rule, whose only fields are `to`, a nonempty array of destinations
///   whose only fields are `cidr`, which they need, and `except`, and `ports`, a nonempty array of
///   port selectors whose only fields are `protocol`, `port`, and `endPort`;
/// - a CIDR is a string that [`Net::parse`] reads, and each exception lies within its network;
/// - a protocol is `tcp`, `udp`, `icmp`, or `any`, the default of a port selector;
/// - a port is an integer from 1 to 65535, and only `tcp`, `udp`, and `any` take one; `endPort`
///   needs `port` and is not below it.
pub(super) fn read_policy(value: &Value) -> Option<Policy> {
    let root = only(value, &["allow", "deny"])?;
    let action = |category: &str| -> Option<Vec<Rule>> {
        match root.get(category) {
            None => Some(Vec::new()),
            Some(rules) => rules.as_array()?.iter().map(read_rule).collect(),
        }
    };
    Some(Policy {
        allow: action("allow")?,
        deny: action("deny")?,
    })
}

/// Returns the object `value` if its fields are among `fields`.
fn only<'a>(value: &'a Value, fields: &[&str]) -> Option<&'a Map<String, Value>> {
    let object = value.as_object()?;
    object
        .keys()
        .all(|name| fields.contains(&name.as_str()))
        .then_some(object)
}

/// Returns the elements of `value` if it is a nonempty array.
fn nonempty(value: &Value) -> Option<&[Value]> {
    value
        .as_array()
        .map(Vec::as_slice)
        .filter(|elements| !elements.is_empty())
}

/// Returns whether `rule`, an object, is a flat rule: whether it has a field of the flat form.
pub(super) fn is_flat(rule: &Map<String, Value>) -> bool {
    FLAT_FIELDS.iter().any(|field| rule.contains_key(*field))
}

/// Reads a rule of either form.
fn read_rule(value: &Value) -> Option<Rule> {
    if is_flat(value.as_object()?) {
        let rule = only(value, &FLAT_FIELDS)?;
        let peer = read_peer(rule)?;
        let selector = match rule.get("protocol") {
            None if rule.contains_key("port") || rule.contains_key("endPort") => return None,
            None => None,
            Some(protocol) => Some(read_selector(read_protocol(protocol)?, rule)?),
        };
        return Some(Rule {
            to: Some(vec![peer]),
            ports: selector.map(|selector| vec![selector]),
        });
    }
    let rule = only(value, &MXC_FIELDS)?;
    let to = match rule.get("to") {
        None => None,
        Some(peers) => Some(
            nonempty(peers)?
                .iter()
                .map(|peer| read_peer(only(peer, &PEER_FIELDS)?))
                .collect::<Option<_>>()?,
        ),
    };
    let ports = match rule.get("ports") {
        None => None,
        Some(selectors) => Some(
            nonempty(selectors)?
                .iter()
                .map(|selector| {
                    let selector = only(selector, &SELECTOR_FIELDS)?;
                    let protocol = match selector.get("protocol") {
                        None => SelectorProtocol::Any,
                        Some(protocol) => read_protocol(protocol)?,
                    };
                    read_selector(protocol, selector)
                })
                .collect::<Option<_>>()?,
        ),
    };
    Some(Rule { to, ports })
}

/// Reads the `cidr` and `except` fields of a destination or a flat rule.
fn read_peer(object: &Map<String, Value>) -> Option<Peer> {
    let network = read_cidr(object.get("cidr")?)?;
    let except: Vec<Net> = match object.get("except") {
        None => Vec::new(),
        Some(except) => except
            .as_array()?
            .iter()
            .map(read_cidr)
            .collect::<Option<_>>()?,
    };
    except
        .iter()
        .all(|exception| network.contains(*exception))
        .then_some(Peer { network, except })
}

/// Reads a CIDR string.
pub(super) fn read_cidr(value: &Value) -> Option<Net> {
    Net::parse(value.as_str()?).ok()
}

/// Reads a protocol: `tcp`, `udp`, `icmp`, or `any`.
fn read_protocol(value: &Value) -> Option<SelectorProtocol> {
    match value.as_str()? {
        "tcp" => Some(SelectorProtocol::Tcp),
        "udp" => Some(SelectorProtocol::Udp),
        "icmp" => Some(SelectorProtocol::Icmp),
        "any" => Some(SelectorProtocol::Any),
        _ => None,
    }
}

/// Reads the `port` and `endPort` fields of a selector of `protocol`.
fn read_selector(protocol: SelectorProtocol, object: &Map<String, Value>) -> Option<Selector> {
    let port = |name: &str| -> Option<Option<u16>> {
        match object.get(name) {
            None => Some(None),
            Some(value) => {
                let port = u16::try_from(value.as_u64()?).ok()?;
                (port != 0).then_some(Some(port))
            }
        }
    };
    let ports = match (port("port")?, port("endPort")?) {
        (None, None) => None,
        (None, Some(_)) => return None,
        (Some(_), _) if protocol == SelectorProtocol::Icmp => return None,
        (Some(first), last) => {
            let last = last.unwrap_or(first);
            if last < first {
                return None;
            }
            Some((first, last))
        }
    };
    Some(Selector { protocol, ports })
}

/// A protocol that a native rule names.
///
/// Protocols order by name, as compiled rules sort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum NativeProtocol {
    /// `icmp`.
    Icmp,
    /// `tcp`.
    Tcp,
    /// `udp`.
    Udp,
}

impl NativeProtocol {
    /// Returns the protocol of the packets that the protocol matches.
    fn protocol(self) -> Protocol {
        match self {
            Self::Icmp => Protocol::Icmp,
            Self::Tcp => Protocol::Tcp,
            Self::Udp => Protocol::Udp,
        }
    }

    /// Returns the name of the protocol in a native rule.
    fn name(self) -> &'static str {
        match self {
            Self::Icmp => "icmp",
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// A native rule, which OpenVMM matches as `doc/run.md` describes: every packet of its network's
/// family to its network, or only the packets of its protocol and, for TCP and UDP, of its ports.
///
/// Native rules order by family, address, prefix length, protocol, and ports, as compiled rules
/// sort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Native {
    /// The network, without host bits.
    pub(super) network: Net,
    /// The protocol, or `None` for every protocol and port.
    pub(super) protocol: Option<NativeProtocol>,
    /// The first and last ports, or `None` for every port. Only TCP and UDP rules have ports.
    pub(super) ports: Option<(u16, u16)>,
}

impl Native {
    /// Parses a native rule as compiled rules write it: `CIDR`, `CIDR:PROTOCOL`,
    /// `CIDR:PROTOCOL:PORT`, or `CIDR:PROTOCOL:FIRST-LAST`, where `CIDR` is an address and a
    /// prefix length, `PROTOCOL` is `tcp`, `udp`, or `icmp`, only `tcp` and `udp` take ports,
    /// and ports run from 1 to 65535. Host bits are cleared, as OpenVMM clears them.
    pub(super) fn parse(text: &str) -> Option<Self> {
        // The selector starts at the first `:` after the prefix length.
        let slash = text.find('/')?;
        let (cidr, selector) = match text[slash..].find(':') {
            Some(colon) => (&text[..slash + colon], Some(&text[slash + colon + 1..])),
            None => (text, None),
        };
        let (address, prefix) = (&cidr[..slash], &cidr[slash + 1..]);
        let network = if let Ok(address) = address.parse::<Ipv4Addr>() {
            Net::new(
                Family::V4,
                u32::from(address).into(),
                prefix_length(prefix, 32)?,
            )
        } else {
            let address: Ipv6Addr = address.parse().ok()?;
            Net::new(Family::V6, address.into(), prefix_length(prefix, 128)?)
        };
        let Some(selector) = selector else {
            return Some(Self {
                network,
                protocol: None,
                ports: None,
            });
        };
        let (protocol, ports) = match selector.split_once(':') {
            Some((protocol, ports)) => (protocol, Some(ports)),
            None => (selector, None),
        };
        let protocol = match protocol {
            "icmp" => NativeProtocol::Icmp,
            "tcp" => NativeProtocol::Tcp,
            "udp" => NativeProtocol::Udp,
            _ => return None,
        };
        let ports = match ports {
            None => None,
            Some(_) if protocol == NativeProtocol::Icmp => return None,
            Some(ports) => {
                let port = |text: &str| text.parse::<u16>().ok().filter(|&port| port != 0);
                let (first, last) = match ports.split_once('-') {
                    Some((first, last)) => (port(first)?, port(last)?),
                    None => (port(ports)?, port(ports)?),
                };
                if last < first {
                    return None;
                }
                Some((first, last))
            }
        };
        Some(Self {
            network,
            protocol: Some(protocol),
            ports,
        })
    }

    /// Returns whether the rule selects traffic of `protocol` to `port` at its network.
    fn selects(self, protocol: Protocol, port: u16) -> bool {
        match self.protocol {
            None => true,
            Some(native) => {
                native.protocol() == protocol
                    && self
                        .ports
                        .is_none_or(|(first, last)| (first..=last).contains(&port))
            }
        }
    }

    /// Returns whether the rule matches `packet`.
    pub(super) fn matches(self, packet: Packet) -> bool {
        self.network.holds(packet.family, packet.address)
            && self.selects(packet.protocol, packet.port)
    }
}

impl fmt::Display for Native {
    /// Writes the rule canonically: its network as [`Net`] writes it, and a range of one port as
    /// that port.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.network)?;
        if let Some(protocol) = self.protocol {
            write!(formatter, ":{}", protocol.name())?;
        }
        match self.ports {
            None => Ok(()),
            Some((first, last)) if first == last => write!(formatter, ":{first}"),
            Some((first, last)) => write!(formatter, ":{first}-{last}"),
        }
    }
}

/// Returns the first address of each cell of `family` that the boundaries of `networks`
/// delimit: within a cell, each network holds every address or none.
fn address_cells(family: Family, networks: impl IntoIterator<Item = Net>) -> Vec<u128> {
    let mut starts = vec![0];
    for network in networks {
        if network.family == family {
            starts.push(network.first());
            if network.last() < family.last() {
                starts.push(network.last() + 1);
            }
        }
    }
    starts.sort_unstable();
    starts.dedup();
    starts
}

/// Returns the first port of each cell that the boundaries of `ranges` delimit, from port 0:
/// within a cell, each range holds every port or none.
fn port_cells(ranges: impl IntoIterator<Item = (u16, u16)>) -> Vec<u16> {
    let mut starts = vec![0, 1];
    for (first, last) in ranges {
        starts.push(first);
        if let Some(next) = last.checked_add(1) {
            starts.push(next);
        }
    }
    starts.sort_unstable();
    starts.dedup();
    starts
}

/// Returns the ports at which to evaluate packets of `protocol`: one of each cell of `cells` for
/// TCP and UDP, and port 0 for the protocols without ports.
fn ports_of(protocol: Protocol, cells: &[u16]) -> &[u16] {
    if protocol.has_ports() { cells } else { &[0] }
}

/// Compares `rules`, the rules of an action, with `natives`, the native rules that it lowers to,
/// for every packet, and returns a packet that they disagree on, if any, with whether `rules`
/// match it.
///
/// For each family and protocol, the boundaries of the networks and port ranges of both sides
/// partition the addresses and ports into cells, within which each network, exception, and range
/// holds every packet or none, so one point of each cell decides the cell.
pub(super) fn difference(rules: &[Rule], natives: &[Native]) -> Option<(Packet, bool)> {
    let networks = || {
        rules
            .iter()
            .flat_map(|rule| rule.to.iter().flatten())
            .flat_map(|peer| std::iter::once(peer.network).chain(peer.except.iter().copied()))
            .chain(natives.iter().map(|native| native.network))
    };
    let ranges = rules
        .iter()
        .flat_map(|rule| rule.ports.iter().flatten())
        .filter_map(|selector| selector.ports)
        .chain(natives.iter().filter_map(|native| native.ports));
    let ports = port_cells(ranges);
    for family in Family::ALL {
        for address in address_cells(family, networks()) {
            // Only the rules that hold the cell's addresses can match its packets.
            let rules: Vec<&Rule> = rules
                .iter()
                .filter(|rule| rule.holds(family, address))
                .collect();
            let natives: Vec<&Native> = natives
                .iter()
                .filter(|native| native.network.holds(family, address))
                .collect();
            if rules.is_empty() && natives.is_empty() {
                continue;
            }
            for protocol in Protocol::ALL {
                for &port in ports_of(protocol, &ports) {
                    let packet = Packet {
                        family,
                        address,
                        protocol,
                        port,
                    };
                    let expected = rules.iter().any(|rule| rule.matches(packet));
                    let actual = natives.iter().any(|native| native.matches(packet));
                    if expected != actual {
                        return Some((packet, expected));
                    }
                }
            }
        }
    }
    None
}

/// Returns the index of a rule of `natives` that the others cover, if any: removing it would not
/// change what they match.
pub(super) fn redundant(natives: &[Native]) -> Option<usize> {
    (0..natives.len()).find(|&index| {
        let rule = natives[index];
        let others: Vec<Native> = natives
            .iter()
            .enumerate()
            .filter(|&(other, native)| other != index && native.network.overlaps(rule.network))
            .map(|(_, native)| *native)
            .collect();
        let family = rule.network.family;
        let addresses: Vec<u128> = address_cells(family, others.iter().map(|other| other.network))
            .into_iter()
            .filter(|&address| rule.network.holds(family, address))
            .chain([rule.network.first()])
            .collect();
        let (first, last) = rule.ports.unwrap_or((0, u16::MAX));
        let ports: Vec<u16> = port_cells(others.iter().filter_map(|other| other.ports))
            .into_iter()
            .filter(|port| (first..=last).contains(port))
            .chain([first])
            .collect();
        let protocols = match rule.protocol {
            None => Protocol::ALL.to_vec(),
            Some(protocol) => vec![protocol.protocol()],
        };
        // The rule is redundant if the others match a point of every cell of its own.
        addresses.iter().all(|&address| {
            protocols.iter().all(|&protocol| {
                ports_of(protocol, &ports).iter().all(|&port| {
                    let packet = Packet {
                        family,
                        address,
                        protocol,
                        port,
                    };
                    others.iter().any(|other| other.matches(packet))
                })
            })
        })
    })
}

/// Returns why `rules`, the native rules of an action, are not canonical, if they are not.
///
/// Canonical rules parse as [`Native`]s and are written as [`Native`] writes them, so without
/// host bits; they are sorted, without duplicates; no network holds or merges with another
/// network of the same protocol and ports; and no two ranges of one network and protocol overlap
/// or meet.
pub(super) fn not_canonical(rules: &[String]) -> Option<String> {
    let mut natives = Vec::with_capacity(rules.len());
    for rule in rules {
        let Some(native) = Native::parse(rule) else {
            return Some(format!("{rule:?} is not a native rule"));
        };
        if native.to_string() != *rule {
            return Some(format!("{rule:?} is not written as {native}"));
        }
        natives.push(native);
    }
    if let Some(pair) = natives.windows(2).find(|pair| pair[0] >= pair[1]) {
        return Some(format!("{} does not sort before {}", pair[0], pair[1]));
    }
    let parent = |network: Net| Net::new(network.family, network.address, network.prefix - 1);
    for (index, &first) in natives.iter().enumerate() {
        for &second in &natives[index + 1..] {
            let (a, b) = (first.network, second.network);
            if (first.protocol, first.ports) == (second.protocol, second.ports) {
                if a.contains(b) || b.contains(a) {
                    return Some(format!("{first} and {second}: one network holds the other"));
                }
                if a.family == b.family
                    && a.prefix == b.prefix
                    && a.prefix > 0
                    && parent(a) == parent(b)
                {
                    return Some(format!("{first} and {second}: the networks merge"));
                }
            }
            if a == b
                && first.protocol == second.protocol
                && let (Some(x), Some(y)) = (first.ports, second.ports)
                && u32::from(x.0) <= u32::from(y.1) + 1
                && u32::from(y.0) <= u32::from(x.1) + 1
            {
                return Some(format!(
                    "{first} and {second}: the port ranges overlap or meet"
                ));
            }
        }
    }
    None
}

// Checks of the model itself.

/// Returns the network that the model reads `text` as, written as compiled rules write it.
fn net(text: &str) -> Result<String, CidrError> {
    Net::parse(text).map(|network| network.to_string())
}

#[test]
fn the_model_reads_cidrs_as_the_format_documents() {
    for (text, expected) in [
        ("10.0.0.5/24", "10.0.0.0/24"),
        ("10.0.0.5/255.255.255.0", "10.0.0.0/24"),
        ("10.0.0.5/0.0.0.255", "10.0.0.0/24"),
        ("10.0.0.5/0.0.0.0", "0.0.0.0/0"),
        ("10.0.0.5/255.255.255.255", "10.0.0.5/32"),
        ("10.0.0.5/0008", "10.0.0.0/8"),
        ("192.0.2.7", "192.0.2.7/32"),
        ("2001:DB8::7/64", "2001:db8::/64"),
        ("::ffff:192.0.2.1/120", "::ffff:192.0.2.0/120"),
        ("1:2:3:4:5:6:1.2.3.4", "1:2:3:4:5:6:102:304/128"),
        ("::", "::/128"),
    ] {
        assert_eq!(net(text).as_deref(), Ok(expected), "{text}");
    }
    let longest = format!("10.0.0.5/{}8", "0".repeat(MAX_PREFIX_DIGITS - 1));
    assert_eq!(net(&longest).as_deref(), Ok("10.0.0.0/8"));
    let too_long = format!("10.0.0.5/{}8", "0".repeat(MAX_PREFIX_DIGITS));
    for text in [
        "",
        "10.0.0.5/",
        "10.0.0.5/33",
        "10.0.0.5/+8",
        "10.0.0.5/8/8",
        "10.0.0.5/255.0.255.0",
        "010.0.0.5",
        "::/129",
        "fe80::1%",
        "fe80::1%a%b",
        "fe80::1%eth0/129",
        "[::1]",
        &too_long,
    ] {
        assert_eq!(net(text), Err(CidrError::Invalid), "{text:?}");
    }
    for text in ["fe80::1%eth0", "fe80::1%eth0/64", "::ffff:192.0.2.1%1/120"] {
        assert_eq!(net(text), Err(CidrError::Scoped), "{text:?}");
    }
}

#[test]
fn the_model_reads_native_rules_back_as_written() {
    for rule in [
        "0.0.0.0/0",
        "192.0.2.0/24:icmp",
        "192.0.2.0/24:tcp",
        "192.0.2.7/32:udp:53",
        "2001:db8::/32:tcp:8000-8010",
        "::ffff:192.0.2.0/120:udp:1-65535",
    ] {
        assert_eq!(Native::parse(rule).unwrap().to_string(), rule);
    }
    assert_eq!(
        Native::parse("192.0.2.7/24:tcp:80-80").unwrap().to_string(),
        "192.0.2.0/24:tcp:80"
    );
    for rule in [
        "192.0.2.7",
        "192.0.2.0/24:sctp",
        "192.0.2.0/24:icmp:8",
        "192.0.2.0/24:tcp:0",
        "192.0.2.0/24:tcp:10-9",
        "192.0.2.0/24:tcp:1-65536",
        "192.0.2.0/24:tcp:1:2",
    ] {
        assert_eq!(Native::parse(rule), None, "{rule}");
    }
}

#[test]
fn the_model_finds_differences_redundancy_and_uncanonical_rules() {
    let natives = |rules: &[&str]| -> Vec<Native> {
        rules
            .iter()
            .map(|rule| Native::parse(rule).unwrap())
            .collect()
    };
    let rule = |cidr: &str, ports: Option<Vec<Selector>>| Rule {
        to: Some(vec![Peer {
            network: Net::parse(cidr).unwrap(),
            except: Vec::new(),
        }]),
        ports,
    };
    let tcp_80 = Selector {
        protocol: SelectorProtocol::Tcp,
        ports: Some((80, 80)),
    };
    let rules = [rule("192.0.2.0/24", Some(vec![tcp_80]))];
    assert_eq!(difference(&rules, &natives(&["192.0.2.0/24:tcp:80"])), None);
    let (packet, expected) = difference(&rules, &natives(&["192.0.2.0/25:tcp:80"])).unwrap();
    assert_eq!(packet.to_string(), "Tcp to 192.0.2.128/32 port 80");
    assert!(expected);
    let (packet, expected) = difference(&rules, &natives(&["192.0.2.0/24:tcp"])).unwrap();
    assert_eq!(packet.to_string(), "Tcp to 192.0.2.0/32 port 0");
    assert!(!expected);
    assert!(difference(&[rule("::/0", None)], &natives(&["::/0:tcp"])).is_some());
    assert!(difference(&[rule("::/0", None)], &natives(&["0.0.0.0/0"])).is_some());

    assert_eq!(
        redundant(&natives(&["10.0.0.0/8", "10.0.0.0/24:tcp:80"])),
        Some(1)
    );
    assert_eq!(
        redundant(&natives(&["10.0.0.0/8:udp", "10.0.0.0/24:tcp:80"])),
        None
    );
    assert_eq!(
        redundant(&natives(&[
            "10.0.0.0/24:tcp:1-10",
            "10.0.0.0/25:tcp:5",
            "10.0.0.128/25:tcp",
        ])),
        Some(1)
    );

    let rules =
        |rules: &[&str]| -> Vec<String> { rules.iter().map(|rule| (*rule).to_owned()).collect() };
    assert_eq!(
        not_canonical(&rules(&["10.0.0.0/8", "10.0.0.0/8:tcp"])),
        None
    );
    for invalid in [
        &["10.0.0.1/8"][..],
        &["10.0.0.0/08"],
        &["10.0.0.0/8:tcp:1-1"],
        &["::FFFF:10.0.0.0/104"],
        &["10.0.0.0/8:tcp", "10.0.0.0/8"],
        &["10.0.0.0/8", "10.0.0.0/8"],
        &["10.0.0.0/8", "10.0.0.0/9"],
        &["10.0.0.0/9", "10.128.0.0/9"],
        &["10.0.0.0/8:tcp:1-5", "10.0.0.0/8:tcp:6-7"],
    ] {
        assert!(not_canonical(&rules(invalid)).is_some(), "{invalid:?}");
    }
}
