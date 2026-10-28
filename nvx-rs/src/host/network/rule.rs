//! The allow and deny decision, the egress rules of a network posture and their native form, and
//! the parsing of the rules of an action into the rules that the lowering takes.

use super::lowering::{self, PortValue, Rule, RuleProtocol};
#[cfg(feature = "cli")]
use crate::error::Error;
use crate::error::Result;
#[cfg(feature = "cli")]
use crate::net::cidr;

/// Allow or deny decision of a network posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Access {
    /// Permit the traffic.
    Allow,
    /// Block the traffic.
    Deny,
}

impl Access {
    /// Returns OpenVMM's spelling of this decision.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// Transport protocol of a [`PortSelector`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// TCP.
    Tcp,
    /// UDP.
    Udp,
    /// ICMP, or ICMPv6 for IPv6 networks.
    Icmp,
    /// Any protocol, or TCP and UDP with a port.
    #[default]
    Any,
}

/// A destination network of an [`EgressRule`], without its exceptions.
///
/// Networks are read as egress policy files read their CIDRs ([`policy`]): host bits are cleared,
/// IPv4 netmasks such as `/255.0.0.0` are accepted, and IPv6 scopes are refused.
///
/// [`policy`]: crate::host::network::policy
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// Destination address or CIDR, such as `"10.0.0.0/8"` or `"192.0.2.1"`.
    pub cidr: String,
    /// Sub-networks of `cidr` that the rule does not match, each within `cidr`.
    pub except: Vec<String>,
}

impl Destination {
    /// Creates a destination for `cidr` without exceptions.
    pub fn new(cidr: impl Into<String>) -> Self {
        Self {
            cidr: cidr.into(),
            except: Vec::new(),
        }
    }
}

/// A destination protocol and port range of an [`EgressRule`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSelector {
    /// Transport protocol.
    pub protocol: Protocol,
    /// First destination port. `None` matches every port.
    pub port: Option<u16>,
    /// Last destination port of a range that starts at `port`.
    pub end_port: Option<u16>,
}

/// Traffic to some destinations and ports.
///
/// An empty `to` matches every destination of both families, and an empty `ports` matches every
/// protocol and port.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EgressRule {
    /// Destination networks.
    pub to: Vec<Destination>,
    /// Destination protocols and ports.
    pub ports: Vec<PortSelector>,
}

impl EgressRule {
    /// Creates a rule that matches every protocol and port of `cidr`, such as `"10.0.0.0/8"` or
    /// `"192.0.2.1"`.
    pub fn to(cidr: impl Into<String>) -> Self {
        Self {
            to: vec![Destination::new(cidr)],
            ports: Vec::new(),
        }
    }

    /// Restricts the rule to one port of `protocol`.
    #[must_use]
    pub fn on_port(mut self, protocol: Protocol, port: u16) -> Self {
        self.ports.push(PortSelector {
            protocol,
            port: Some(port),
            end_port: None,
        });
        self
    }

    /// Restricts the rule to the ports of `protocol` from `port` through `end_port`, both
    /// included.
    #[must_use]
    pub fn on_port_range(mut self, protocol: Protocol, port: u16, end_port: u16) -> Self {
        self.ports.push(PortSelector {
            protocol,
            port: Some(port),
            end_port: Some(end_port),
        });
        self
    }

    /// Restricts the rule to `protocol` on every port.
    #[must_use]
    pub fn on_protocol(mut self, protocol: Protocol) -> Self {
        self.ports.push(PortSelector {
            protocol,
            port: None,
            end_port: None,
        });
        self
    }

    /// Parses one of OpenVMM's native rules, the value of an `--network-egress-allow` or
    /// `--network-egress-deny` option, as OpenVMM parses it: `CIDR`, `CIDR:PROTOCOL`,
    /// `CIDR:PROTOCOL:PORT`, or `CIDR:PROTOCOL:FIRST-LAST`, where `CIDR` is an IPv4 or IPv6
    /// address with an optional prefix length, whose host bits are cleared, `PROTOCOL` is `tcp`,
    /// `udp`, or `icmp`, and only `tcp` and `udp` take ports. The rule's destination is the
    /// canonical network.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) and a message that
    /// starts with `value`.
    #[cfg(feature = "cli")]
    pub(crate) fn parse_native(value: &str) -> Result<Self> {
        let refused = |reason: &str| Err(Error::unsupported(format!("{value}: {reason}")));
        let (destination, selector) = split_native_rule(value);
        let (protocol, ports) =
            match selector.map(|selector| selector.split(':').collect::<Vec<_>>()) {
                None => (None, None),
                Some(fields) => match fields[..] {
                    [protocol] => (Some(protocol), None),
                    [protocol, ports] => (Some(protocol), Some(ports)),
                    _ => return refused("expected CIDR[:PROTOCOL[:PORT[-PORT]]]"),
                },
            };
        let protocol = match protocol {
            None => None,
            Some("tcp") => Some(Protocol::Tcp),
            Some("udp") => Some(Protocol::Udp),
            Some("icmp") => Some(Protocol::Icmp),
            Some(_) => return refused("the protocol must be tcp, udp, or icmp"),
        };
        let ports = match ports {
            None => None,
            Some(_) if protocol == Some(Protocol::Icmp) => return refused("icmp takes no port"),
            Some(ports) => match ports.split_once('-') {
                None => match parse_port(ports) {
                    Some(port) => Some((port, port)),
                    None => return refused("ports must be between 1 and 65535"),
                },
                Some((first, last))
                    if first.is_empty() || last.is_empty() || last.contains('-') =>
                {
                    return refused("a port range must be FIRST-LAST");
                }
                Some((first, last)) => match (parse_port(first), parse_port(last)) {
                    (Some(first), Some(last)) if first <= last => Some((first, last)),
                    (Some(_), Some(_)) => {
                        return refused("the port range ends below its first port");
                    }
                    _ => return refused("ports must be between 1 and 65535"),
                },
            },
        };
        let Ok(destination) = cidr::Cidr::parse_masked(destination) else {
            return refused(
                "the destination must be an IPv4 or IPv6 address with an optional prefix",
            );
        };
        let rule = Self::to(destination.to_string());
        // A range of one port is that port.
        Ok(match (protocol, ports) {
            (None, _) => rule,
            (Some(protocol), None) => rule.on_protocol(protocol),
            (Some(protocol), Some((first, last))) if first == last => rule.on_port(protocol, first),
            (Some(protocol), Some((first, last))) => rule.on_port_range(protocol, first, last),
        })
    }
}

/// Splits a native rule into its destination and its optional protocol and ports, where OpenVMM
/// splits it: an IPv4 address holds no `:`, so its first `:` starts the selector; a network with a
/// prefix ends at the first `:` after its `/`; and an IPv6 address without a prefix ends at the
/// last `:` that leaves an IPv6 address before it, since protocol names are not hexadecimal.
#[cfg(feature = "cli")]
fn split_native_rule(value: &str) -> (&str, Option<&str>) {
    let split_at = |index: usize| (&value[..index], Some(&value[index + 1..]));
    if let Some(slash) = value.find('/') {
        return match value[slash..].find(':') {
            Some(colon) => split_at(slash + colon),
            None => (value, None),
        };
    }
    match value.find(':') {
        None => (value, None),
        Some(colon) if value[..colon].contains('.') => split_at(colon),
        Some(_) if value.parse::<std::net::Ipv6Addr>().is_ok() => (value, None),
        Some(_) => value
            .rmatch_indices(':')
            .map(|(index, _)| index)
            .find(|&index| value[..index].parse::<std::net::Ipv6Addr>().is_ok())
            .map_or((value, None), split_at),
    }
}

/// Returns the port that `text` names, read as OpenVMM reads its ports, if it is nonzero.
pub(super) fn parse_port(text: &str) -> Option<u16> {
    text.parse().ok().filter(|&port| port != 0)
}

/// The egress rules of one action, and the name by which messages refer to them, such as
/// `egress.allow`. Messages name a rule by its index and its fields by their MXC names, as in
/// `egress.allow[0].to[1].cidr`, and the action by the name, as in `egress.allow emits at most 256
/// native rules`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressRules {
    /// The name of the rules in messages.
    pub name: String,
    /// The rules.
    pub rules: Vec<EgressRule>,
}

impl EgressRules {
    /// Creates an empty list of rules that messages call `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            rules: Vec::new(),
        }
    }
}

/// Parses the rules of one action, each into the rules that the lowering takes, as an egress
/// policy file's MXC rules are parsed: an empty `to` or `ports` is a rule without that field.
pub(super) fn parse_rules(rules: &EgressRules) -> Result<Vec<Vec<Rule>>> {
    rules
        .rules
        .iter()
        .enumerate()
        .map(|(index, rule)| parse_rule(rule, &format!("{}[{index}]", rules.name)))
        .collect()
}

/// Parses one rule, which messages call `description`.
fn parse_rule(rule: &EgressRule, description: &str) -> Result<Vec<Rule>> {
    let mut destinations = None;
    if !rule.to.is_empty() {
        let mut parsed = Vec::with_capacity(rule.to.len());
        for (index, peer) in rule.to.iter().enumerate() {
            let description = format!("{description}.to[{index}]");
            let network = |value: &str, field: &str| lowering::network(value, field);
            let parent = network(&peer.cidr, &format!("{description}.cidr"))?;
            parsed.push(lowering::destination(
                parent,
                peer.except.iter().enumerate().map(|(exception, value)| {
                    network(value, &format!("{description}.except[{exception}]"))
                }),
                &description,
            )?);
        }
        destinations = Some(lowering::group_destinations(&parsed));
    }
    let mut selectors = None;
    if !rule.ports.is_empty() {
        let mut parsed = Vec::new();
        for (index, port) in rule.ports.iter().enumerate() {
            let protocol = match port.protocol {
                Protocol::Tcp => RuleProtocol::Tcp,
                Protocol::Udp => RuleProtocol::Udp,
                Protocol::Icmp => RuleProtocol::Icmp,
                Protocol::Any => RuleProtocol::Any,
            };
            let value = |port: u16| PortValue::Integer(i128::from(port));
            parsed.extend(lowering::port_selectors(
                protocol,
                port.port.map(value),
                port.end_port.map(value),
                &format!("{description}.ports[{index}]"),
            )?);
        }
        selectors = Some(parsed);
    }
    Ok(lowering::combine(destinations, selectors))
}
