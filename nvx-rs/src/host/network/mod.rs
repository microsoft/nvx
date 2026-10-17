//! Translation of a network posture into OpenVMM options, and the egress policy files of
//! `nvx.py`.
//!
//! OpenVMM's portable profile enforces an egress default plus IPv4 and IPv6 allow and deny rules
//! that match a network of one family and, optionally, one protocol (TCP, UDP, or ICMP) and, for
//! TCP or UDP, one port or an inclusive range of ports; deny rules take precedence. [`EgressRule`]s
//! are expanded into that form exactly: exceptions are subtracted from their networks, a rule
//! without destinations matches `0.0.0.0/0` and `::/0`, and `any` protocol with a port or a port
//! range becomes a TCP and a UDP rule. Rules that cannot be expressed exactly are rejected.
//! [`policy`] compiles the egress policy files of `nvx.py` into OpenVMM's rules too.
//!
//! Host-loopback access is bidirectional: with it allowed, the guest reaches host loopback
//! services through its gateway, and OpenVMM publishes the guest's listeners on host loopback.
//! [`LoopbackForward`]s publish further host loopback ports at guest ports of their choice.

mod address;
mod lowering;
pub mod policy;

use std::collections::HashSet;
use std::net::Ipv4Addr;

use crate::error::{Error, Result};
use crate::net::cidr::{self, Cidr};

/// Largest number of allow rules, and of deny rules, that OpenVMM accepts.
pub const MAX_RULES: usize = 256;

/// Largest number of [`LoopbackForward`]s that OpenVMM accepts.
pub const MAX_LOOPBACK_FORWARDS: usize = 64;

/// Longest guest network prefix that host-loopback access accepts. OpenVMM presents a host peer
/// to the guest at an address of the guest network, and a /30 network holds only the gateway and
/// the guest.
pub const MAX_HOST_LOOPBACK_PREFIX: u8 = 29;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// Destination address or CIDR, such as `"10.0.0.0/8"` or `"192.0.2.1"`.
    pub cidr: String,
    /// Sub-networks of `cidr` that the rule does not match.
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
}

/// The egress rules of one action, and the name by which messages refer to them, such as
/// `egress.allow`. Messages name a rule by its index, as in `egress.allow[0]`.
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

/// Transport protocol of a [`LoopbackForward`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForwardProtocol {
    /// TCP connections.
    Tcp,
    /// UDP datagrams.
    Udp,
}

impl ForwardProtocol {
    /// Returns OpenVMM's spelling of this protocol.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// A port on host IPv4 loopback that OpenVMM forwards to a guest port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LoopbackForward {
    /// Transport protocol.
    pub protocol: ForwardProtocol,
    /// Port on `127.0.0.1` where OpenVMM listens; nonzero.
    pub host_port: u16,
    /// Guest port that receives the traffic; nonzero.
    pub guest_port: u16,
}

/// The network posture of a VM.
///
/// A connection matching a `deny` rule is blocked. Otherwise a connection matching an `allow`
/// rule is permitted, and any other connection follows the `egress` default. Access between the
/// guest and host loopback follows `host_loopback` in both directions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPolicy {
    /// Default policy for guest-initiated connections.
    pub egress: Access,
    /// Destinations permitted despite a `deny` default.
    pub allow: EgressRules,
    /// Destinations blocked despite an `allow` default or an `allow` rule.
    pub deny: EgressRules,
    /// Default policy for connections that clients initiate to the guest. With [`Access::Allow`],
    /// OpenVMM listens on every host address at the TCP and UDP ports where the guest listens,
    /// and admits clients on the host's private networks, whose addresses are not globally
    /// reachable, but not clients inside the guest network, IPv6 link-local clients, or the host
    /// itself unless `host_loopback` allows it; replies to their connections pass whatever the
    /// egress policy.
    pub ingress: Access,
    /// Access between the guest and host loopback, in both directions. With [`Access::Allow`],
    /// the guest reaches host loopback services through its gateway, subject to the egress
    /// policy, and the host reaches the guest's TCP and UDP listeners on non-loopback guest
    /// addresses at the same ports of host IPv4 and IPv6 loopback, where OpenVMM listens without
    /// displacing a host listener and admits only the host; replies to the host pass whatever
    /// the egress policy. With `ingress` allowed too, the listeners on every host address admit
    /// the host instead. With [`Access::Deny`], neither direction is open. [`Access::Allow`]
    /// needs a guest network prefix of at most [`MAX_HOST_LOOPBACK_PREFIX`].
    pub host_loopback: Access,
    /// Ports on host IPv4 loopback forwarded to guest ports, at most [`MAX_LOOPBACK_FORWARDS`],
    /// each protocol and host port at most once, and each protocol and guest port at most once.
    /// Forwards need `host_loopback` allowed, and because the egress policy filters the guest's
    /// replies, which go to addresses that OpenVMM assigns inside the guest network, also an
    /// `egress` default of [`Access::Allow`] and no `deny` rule that covers the guest network.
    pub host_loopback_forwards: Vec<LoopbackForward>,
}

impl NetworkPolicy {
    /// Applies `egress` as the default egress policy, with no rules, and denies ingress and
    /// host-loopback access. Messages call the rules `egress.allow` and `egress.deny`.
    pub fn new(egress: Access) -> Self {
        Self {
            egress,
            allow: EgressRules::new("egress.allow"),
            deny: EgressRules::new("egress.deny"),
            ingress: Access::Deny,
            host_loopback: Access::Deny,
            host_loopback_forwards: Vec::new(),
        }
    }

    /// Adds a rule that permits matching destinations.
    #[must_use]
    pub fn with_allow(mut self, rule: EgressRule) -> Self {
        self.allow.rules.push(rule);
        self
    }

    /// Adds a rule that blocks matching destinations.
    #[must_use]
    pub fn with_deny(mut self, rule: EgressRule) -> Self {
        self.deny.rules.push(rule);
        self
    }
}

/// Returns the OpenVMM options that enforce `policy` for a guest at `guest_network`.
///
/// Without a policy, or when egress is denied without allow rules and ingress and host-loopback
/// access are denied, no network device is attached. Otherwise the portable profile applies the
/// egress posture, the ingress default, the host-loopback access, and the host-loopback forwards.
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) when a rule cannot be
/// expressed exactly, when the rules of one action expand to more than [`MAX_RULES`] OpenVMM
/// rules, when host-loopback access is allowed for a guest network whose prefix is longer than
/// [`MAX_HOST_LOOPBACK_PREFIX`], or when the host-loopback forwards break the rules of
/// [`NetworkPolicy::host_loopback_forwards`].
pub fn openvmm_arguments(
    policy: Option<&NetworkPolicy>,
    guest_network: &str,
) -> Result<Vec<String>> {
    let Some(policy) = policy else {
        return Ok(Vec::new());
    };
    if !policy.host_loopback_forwards.is_empty() && policy.host_loopback != Access::Allow {
        return Err(Error::unsupported(
            "host-loopback forwards need host-loopback access allowed",
        ));
    }
    // OpenVMM presents a host peer to the guest at an address of the guest network, which a /30
    // network, holding only the gateway and the guest, cannot spare.
    if policy.host_loopback == Access::Allow
        && guest_network
            .split_once('/')
            .and_then(|(_, prefix)| cidr::parse_prefix_length(prefix))
            .is_some_and(|prefix| prefix > MAX_HOST_LOOPBACK_PREFIX)
    {
        return Err(Error::unsupported(format!(
            "host-loopback access needs a guest network prefix of /{MAX_HOST_LOOPBACK_PREFIX} or \
             shorter, which leaves an address at which OpenVMM presents host peers to the guest"
        )));
    }
    if policy.egress == Access::Deny
        && policy.allow.rules.is_empty()
        && policy.ingress == Access::Deny
        && policy.host_loopback == Access::Deny
    {
        return Ok(Vec::new());
    }
    let mut arguments: Vec<String> = [
        "--net",
        guest_network,
        "--network-profile",
        "portable",
        "--network-egress",
        policy.egress.as_str(),
        "--network-ingress",
        policy.ingress.as_str(),
        "--host-loopback",
        policy.host_loopback.as_str(),
    ]
    .map(str::to_owned)
    .to_vec();
    for (rules, option) in [
        (&policy.allow, "--network-egress-allow"),
        (&policy.deny, "--network-egress-deny"),
    ] {
        let field = &rules.name;
        let mut expanded = Vec::new();
        for (index, rule) in rules.rules.iter().enumerate() {
            expand_rule(rule, &format!("{field}[{index}]"), &mut expanded)?;
        }
        expanded.sort_unstable();
        expanded.dedup();
        if expanded.len() > MAX_RULES {
            return Err(Error::unsupported(format!(
                "{field} expands to {} OpenVMM rules; the openvmm backend accepts at most \
                 {MAX_RULES}",
                expanded.len()
            )));
        }
        for rule in expanded {
            arguments.push(option.to_owned());
            arguments.push(rule);
        }
    }
    check_loopback_forwards(policy, guest_network)?;
    for forward in &policy.host_loopback_forwards {
        arguments.push("--host-loopback-forward".to_owned());
        arguments.push(format!(
            "{}:{}:{}",
            forward.protocol.as_str(),
            forward.host_port,
            forward.guest_port
        ));
    }
    Ok(arguments)
}

/// Checks the host-loopback forwards of a policy whose rules expand, and whose host-loopback
/// access is allowed when it has forwards, against the rules of
/// [`NetworkPolicy::host_loopback_forwards`].
fn check_loopback_forwards(policy: &NetworkPolicy, guest_network: &str) -> Result<()> {
    let forwards = &policy.host_loopback_forwards;
    if forwards.is_empty() {
        return Ok(());
    }
    let unsupported = |message: String| Err(Error::unsupported(message));
    if forwards.len() > MAX_LOOPBACK_FORWARDS {
        return unsupported(format!(
            "the openvmm backend accepts at most {MAX_LOOPBACK_FORWARDS} host-loopback forwards"
        ));
    }
    let mut bindings = HashSet::new();
    let mut targets = HashSet::new();
    for forward in forwards {
        if forward.host_port == 0 || forward.guest_port == 0 {
            return unsupported(
                "host-loopback forward ports must be between 1 and 65535".to_owned(),
            );
        }
        if !bindings.insert((forward.protocol, forward.host_port)) {
            return unsupported(format!(
                "host-loopback forwards bind {} host port {} more than once",
                forward.protocol.as_str(),
                forward.host_port
            ));
        }
        // OpenVMM keys a forward's listener by its guest port, so a second forward to one guest
        // port would fail to bind when the VM starts.
        if !targets.insert((forward.protocol, forward.guest_port)) {
            return unsupported(format!(
                "host-loopback forwards reach {} guest port {} more than once",
                forward.protocol.as_str(),
                forward.guest_port
            ));
        }
    }
    if policy.egress != Access::Allow {
        return unsupported(
            "host-loopback forwards need egress allowed by default for their replies".to_owned(),
        );
    }
    let Some(network) = guest_subnet(guest_network) else {
        return unsupported(format!("guest network {guest_network:?} is not valid"));
    };
    for (index, rule) in policy.deny.rules.iter().enumerate() {
        if rule_covers(rule, network) {
            return unsupported(format!(
                "{}[{index}] cannot cover the guest network, which carries host-loopback forward \
                 replies",
                policy.deny.name
            ));
        }
    }
    Ok(())
}

/// Returns the IPv4 network of `guest_network`, a static guest address in `ADDRESS/PREFIX` form.
fn guest_subnet(guest_network: &str) -> Option<Cidr> {
    let (address, prefix) = guest_network.split_once('/')?;
    let address: Ipv4Addr = address.parse().ok()?;
    let prefix = cidr::parse_prefix_length(prefix).filter(|prefix| (1..=32).contains(prefix))?;
    let mask = u32::MAX << (32 - u32::from(prefix));
    Cidr::parse(&format!(
        "{}/{prefix}",
        Ipv4Addr::from(u32::from(address) & mask)
    ))
    .ok()
}

/// Returns whether `rule`, whose destinations parse, matches an address of `network`.
fn rule_covers(rule: &EgressRule, network: Cidr) -> bool {
    rule.to.is_empty()
        || rule.to.iter().any(|peer| {
            let Ok(destination) = Cidr::parse(&peer.cidr) else {
                return false;
            };
            let excluded = peer
                .except
                .iter()
                .filter_map(|value| Cidr::parse(value).ok())
                .collect::<Vec<_>>();
            destination
                .subtract(&excluded, MAX_RULES)
                .is_none_or(|remainder| remainder.iter().any(|part| part.overlaps(network)))
        })
}

/// Appends the OpenVMM rules equivalent to `rule`.
fn expand_rule(rule: &EgressRule, field: &str, output: &mut Vec<String>) -> Result<()> {
    let unsupported = |message: String| Err(Error::unsupported(format!("{field}{message}")));
    let mut networks = Vec::new();
    if rule.to.is_empty() {
        networks.extend([Cidr::everything(false), Cidr::everything(true)]);
    }
    for (index, peer) in rule.to.iter().enumerate() {
        let Ok(network) = Cidr::parse(&peer.cidr) else {
            return unsupported(format!(".to[{index}]: the destination is not a network"));
        };
        let mut excluded = Vec::new();
        for value in &peer.except {
            match Cidr::parse(value) {
                Ok(exclusion) if network.contains(exclusion) => excluded.push(exclusion),
                _ => {
                    return unsupported(format!(
                        ".to[{index}].except: an exception is not a network within the destination"
                    ));
                }
            }
        }
        let Some(remainder) = network.subtract(&excluded, MAX_RULES - networks.len()) else {
            return unsupported(format!(
                " covers more than {MAX_RULES} networks after removing its exceptions"
            ));
        };
        networks.extend(remainder);
    }

    let mut selectors = Vec::new();
    if rule.ports.is_empty() {
        selectors.push(Selector::Everything);
    }
    for (index, port) in rule.ports.iter().enumerate() {
        let protocols: &[&'static str] = match port.protocol {
            Protocol::Tcp => &["tcp"],
            Protocol::Udp => &["udp"],
            Protocol::Icmp => &["icmp"],
            Protocol::Any => &["tcp", "udp"],
        };
        let Some(start) = port.port else {
            if port.end_port.is_some() {
                return unsupported(format!(".ports[{index}].endPort requires port"));
            }
            if port.protocol == Protocol::Any {
                selectors.push(Selector::Everything);
            } else {
                selectors.extend(protocols.iter().copied().map(Selector::Protocol));
            }
            continue;
        };
        if port.protocol == Protocol::Icmp {
            return unsupported(format!(".ports[{index}]: ICMP has no ports"));
        }
        let end = port.end_port.unwrap_or(start);
        if end < start {
            return unsupported(format!(".ports[{index}].endPort must not be below port"));
        }
        selectors.extend(
            protocols
                .iter()
                .map(|&protocol| Selector::Ports(protocol, start, end)),
        );
    }

    for network in &networks {
        for selector in &selectors {
            output.push(match *selector {
                Selector::Everything => network.to_string(),
                Selector::Protocol(protocol) => format!("{network}:{protocol}"),
                Selector::Ports(protocol, start, end) if start == end => {
                    format!("{network}:{protocol}:{start}")
                }
                Selector::Ports(protocol, start, end) => {
                    format!("{network}:{protocol}:{start}-{end}")
                }
            });
            if output.len() > MAX_RULES * 4 {
                return unsupported(format!(" expands to more than {MAX_RULES} OpenVMM rules"));
            }
        }
    }
    Ok(())
}

/// The traffic that one OpenVMM rule selects at its destination network.
#[derive(Clone, Copy)]
enum Selector {
    /// Every protocol and port.
    Everything,
    /// Every port of TCP or UDP, or every ICMP message.
    Protocol(&'static str),
    /// The TCP or UDP ports from the first through the last, both included.
    Ports(&'static str, u16, u16),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    fn rules(arguments: &[String], option: &str) -> Vec<String> {
        arguments
            .windows(2)
            .filter(|pair| pair[0] == option)
            .map(|pair| pair[1].clone())
            .collect()
    }

    #[test]
    fn default_postures_map_to_portable_profile_options() {
        assert!(openvmm_arguments(None, "10.0.0.2/24").unwrap().is_empty());
        assert!(
            openvmm_arguments(Some(&NetworkPolicy::new(Access::Deny)), "10.0.0.2/24")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            openvmm_arguments(Some(&NetworkPolicy::new(Access::Allow)), "10.0.0.2/24").unwrap(),
            [
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--network-egress",
                "allow",
                "--network-ingress",
                "deny",
                "--host-loopback",
                "deny",
            ]
        );
        // Deny rules under a deny default change nothing, so no device is attached.
        let denied = NetworkPolicy::new(Access::Deny).with_deny(EgressRule::to("1.2.3.4"));
        assert!(
            openvmm_arguments(Some(&denied), "10.0.0.2/24")
                .unwrap()
                .is_empty()
        );
        // Ingress needs the device even when egress is denied, and host loopback stays denied.
        for egress in [Access::Deny, Access::Allow] {
            let mut ingress = NetworkPolicy::new(egress);
            ingress.ingress = Access::Allow;
            assert_eq!(
                openvmm_arguments(Some(&ingress), "10.0.0.2/24").unwrap(),
                [
                    "--net",
                    "10.0.0.2/24",
                    "--network-profile",
                    "portable",
                    "--network-egress",
                    egress.as_str(),
                    "--network-ingress",
                    "allow",
                    "--host-loopback",
                    "deny",
                ]
            );
        }
        // Host-loopback access needs the device even when egress and ingress are denied.
        let mut host_loopback = NetworkPolicy::new(Access::Deny);
        host_loopback.host_loopback = Access::Allow;
        assert_eq!(
            openvmm_arguments(Some(&host_loopback), "10.0.0.2/24").unwrap(),
            [
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-ingress",
                "deny",
                "--host-loopback",
                "allow",
            ]
        );
    }

    #[test]
    fn host_loopback_access_needs_room_for_host_peers() {
        let mut policy = NetworkPolicy::new(Access::Deny);
        policy.host_loopback = Access::Allow;
        // A /30 network holds only the gateway and the guest.
        let error = openvmm_arguments(Some(&policy), "10.0.0.2/30").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(error.message().contains("/29 or shorter"), "{error}");
        for guest_network in ["10.0.0.2/29", "10.0.0.2/24"] {
            let arguments = openvmm_arguments(Some(&policy), guest_network).unwrap();
            assert!(
                has_pair(&arguments, "--host-loopback", "allow"),
                "{guest_network}"
            );
        }
        // Host-loopback access that is denied needs no host peers.
        let mut denied = NetworkPolicy::new(Access::Allow);
        denied.host_loopback = Access::Deny;
        let arguments = openvmm_arguments(Some(&denied), "10.0.0.2/30").unwrap();
        assert!(has_pair(&arguments, "--host-loopback", "deny"));
    }

    #[test]
    fn host_loopback_forwards_follow_openvmm_rules() {
        let forward = |protocol, host_port, guest_port| LoopbackForward {
            protocol,
            host_port,
            guest_port,
        };
        let forwarded = |forwards: Vec<LoopbackForward>| {
            let mut policy = NetworkPolicy::new(Access::Allow);
            policy.host_loopback = Access::Allow;
            policy.host_loopback_forwards = forwards;
            policy
        };
        let rejected = |policy: &NetworkPolicy, expected: &str| {
            let error = openvmm_arguments(Some(policy), "10.0.0.2/24").unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported);
            assert!(error.message().contains(expected), "{error}");
        };

        let valid = forwarded(vec![
            forward(ForwardProtocol::Tcp, 18080, 8080),
            forward(ForwardProtocol::Udp, 18080, 8081),
            // One guest port may receive a forward of each protocol.
            forward(ForwardProtocol::Udp, 18081, 8080),
        ]);
        let arguments = openvmm_arguments(Some(&valid), "10.0.0.2/24").unwrap();
        assert_eq!(
            rules(&arguments, "--host-loopback-forward"),
            ["tcp:18080:8080", "udp:18080:8081", "udp:18081:8080"]
        );
        assert!(has_pair(&arguments, "--host-loopback", "allow"));

        let mut denied = valid.clone();
        denied.host_loopback = Access::Deny;
        rejected(&denied, "need host-loopback access allowed");
        let mut closed = valid.clone();
        closed.egress = Access::Deny;
        rejected(&closed, "egress allowed by default");
        rejected(
            &forwarded(vec![
                forward(ForwardProtocol::Tcp, 18080, 8080),
                forward(ForwardProtocol::Tcp, 18080, 8081),
            ]),
            "bind tcp host port 18080 more than once",
        );
        // OpenVMM could not bind a second forward to one guest port.
        rejected(
            &forwarded(vec![
                forward(ForwardProtocol::Udp, 18080, 8080),
                forward(ForwardProtocol::Udp, 18081, 8080),
            ]),
            "reach udp guest port 8080 more than once",
        );
        rejected(
            &forwarded(vec![forward(ForwardProtocol::Udp, 0, 8080)]),
            "between 1 and 65535",
        );
        rejected(
            &forwarded(
                (1..=65)
                    .map(|port| forward(ForwardProtocol::Tcp, port, port))
                    .collect(),
            ),
            "at most 64",
        );

        // The guest's replies to forwarded traffic leave toward the guest network.
        for rule in [
            EgressRule::default(),
            EgressRule::to("10.0.0.0/8"),
            EgressRule::to("10.0.0.0/24"),
            EgressRule::to("10.0.0.77").on_port(Protocol::Tcp, 443),
        ] {
            rejected(&valid.clone().with_deny(rule), "forward replies");
        }
        for rule in [
            EgressRule::to("10.0.1.0/24"),
            EgressRule::to("fd00::/8"),
            EgressRule {
                to: vec![Destination {
                    cidr: "0.0.0.0/0".to_owned(),
                    except: vec!["10.0.0.0/24".to_owned()],
                }],
                ports: Vec::new(),
            },
        ] {
            openvmm_arguments(Some(&valid.clone().with_deny(rule)), "10.0.0.2/24").unwrap();
        }
    }

    #[test]
    fn rules_expand_exactly() {
        let egress = NetworkPolicy::new(Access::Deny)
            .with_allow(EgressRule {
                to: vec![Destination {
                    cidr: "10.0.0.0/30".to_owned(),
                    except: vec!["10.0.0.1".to_owned()],
                }],
                ports: vec![PortSelector {
                    protocol: Protocol::Tcp,
                    port: Some(80),
                    end_port: Some(81),
                }],
            })
            .with_allow(EgressRule::to("192.0.2.7").on_port(Protocol::Any, 53))
            .with_allow(EgressRule::to("198.51.100.0/24"));
        let arguments = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap();
        assert_eq!(
            rules(&arguments, "--network-egress-allow"),
            [
                "10.0.0.0/32:tcp:80-81",
                "10.0.0.2/31:tcp:80-81",
                "192.0.2.7/32:tcp:53",
                "192.0.2.7/32:udp:53",
                "198.51.100.0/24",
            ]
        );
        assert!(has_pair(&arguments, "--network-egress", "deny"));

        let egress = NetworkPolicy::new(Access::Allow).with_deny(EgressRule {
            to: Vec::new(),
            ports: vec![PortSelector {
                protocol: Protocol::Udp,
                port: Some(53),
                end_port: None,
            }],
        });
        let arguments = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap();
        // A rule without destinations matches both address families.
        assert_eq!(
            rules(&arguments, "--network-egress-deny"),
            ["0.0.0.0/0:udp:53", "::/0:udp:53"]
        );
    }

    #[test]
    fn ipv6_rules_expand_exactly() {
        let egress = NetworkPolicy::new(Access::Deny)
            .with_allow(EgressRule::to("2001:db8:1::/64").on_port(Protocol::Tcp, 443))
            .with_allow(EgressRule {
                to: vec![Destination {
                    cidr: "2001:db8:2::/126".to_owned(),
                    except: vec!["2001:db8:2::1".to_owned()],
                }],
                ports: vec![PortSelector {
                    protocol: Protocol::Any,
                    port: Some(53),
                    end_port: None,
                }],
            })
            .with_allow(EgressRule::to("192.0.2.0/24").on_protocol(Protocol::Icmp))
            .with_allow(EgressRule::to("::/0").on_protocol(Protocol::Icmp))
            .with_deny(EgressRule::to("2001:db8:1::123"));
        let arguments = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap();
        assert_eq!(
            rules(&arguments, "--network-egress-allow"),
            [
                "192.0.2.0/24:icmp",
                "2001:db8:1::/64:tcp:443",
                "2001:db8:2::/128:tcp:53",
                "2001:db8:2::/128:udp:53",
                "2001:db8:2::2/127:tcp:53",
                "2001:db8:2::2/127:udp:53",
                "::/0:icmp",
            ]
        );
        assert_eq!(
            rules(&arguments, "--network-egress-deny"),
            ["2001:db8:1::123/128"]
        );
    }

    #[test]
    fn exclusions_remain_local_to_their_rules() {
        let excluding = |cidr: &str, excluded: &str| EgressRule {
            to: vec![Destination {
                cidr: cidr.to_owned(),
                except: vec![excluded.to_owned()],
            }],
            ports: Vec::new(),
        };
        let egress = NetworkPolicy::new(Access::Deny)
            .with_allow(excluding("192.0.2.0/24", "192.0.2.128/25"))
            .with_allow(EgressRule::to("192.0.2.200"))
            .with_deny(excluding("198.51.100.0/24", "198.51.100.128/25"));
        let arguments = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap();

        assert_eq!(
            rules(&arguments, "--network-egress-allow"),
            ["192.0.2.0/25", "192.0.2.200/32"]
        );
        assert_eq!(
            rules(&arguments, "--network-egress-deny"),
            ["198.51.100.0/25"]
        );
    }

    fn has_pair(arguments: &[String], name: &str, value: &str) -> bool {
        arguments
            .windows(2)
            .any(|pair| pair[0] == name && pair[1] == value)
    }

    #[test]
    fn protocols_without_ports_expand_to_protocol_rules() {
        let egress = NetworkPolicy::new(Access::Deny)
            .with_allow(EgressRule::to("192.0.2.7").on_protocol(Protocol::Tcp))
            .with_allow(EgressRule::to("192.0.2.7").on_protocol(Protocol::Udp))
            .with_allow(EgressRule::to("192.0.2.7").on_protocol(Protocol::Icmp))
            .with_allow(EgressRule::to("198.51.100.0/24").on_protocol(Protocol::Any))
            .with_allow(EgressRule::to("203.0.113.9").on_port(Protocol::Any, 443))
            .with_deny(
                EgressRule::to("192.0.2.7")
                    .on_protocol(Protocol::Icmp)
                    .on_port(Protocol::Tcp, 22),
            );
        let arguments = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap();
        assert_eq!(
            rules(&arguments, "--network-egress-allow"),
            [
                "192.0.2.7/32:icmp",
                "192.0.2.7/32:tcp",
                "192.0.2.7/32:udp",
                "198.51.100.0/24",
                "203.0.113.9/32:tcp:443",
                "203.0.113.9/32:udp:443",
            ]
        );
        assert_eq!(
            rules(&arguments, "--network-egress-deny"),
            ["192.0.2.7/32:icmp", "192.0.2.7/32:tcp:22"]
        );
    }

    #[test]
    fn port_ranges_expand_to_one_rule_per_network_and_protocol() {
        let egress = NetworkPolicy::new(Access::Deny)
            .with_allow(EgressRule::to("192.0.2.7").on_port_range(Protocol::Tcp, 8000, 8010))
            .with_allow(EgressRule::to("192.0.2.7").on_port_range(Protocol::Udp, 5000, 5010))
            .with_allow(EgressRule::to("198.51.100.0/24").on_port_range(Protocol::Any, 1, 65535))
            // A range of one port is that port's rule.
            .with_allow(EgressRule::to("203.0.113.9").on_port_range(Protocol::Tcp, 443, 443))
            .with_deny(EgressRule::to("192.0.2.7").on_port(Protocol::Tcp, 8005));
        let arguments = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap();
        assert_eq!(
            rules(&arguments, "--network-egress-allow"),
            [
                "192.0.2.7/32:tcp:8000-8010",
                "192.0.2.7/32:udp:5000-5010",
                "198.51.100.0/24:tcp:1-65535",
                "198.51.100.0/24:udp:1-65535",
                "203.0.113.9/32:tcp:443",
            ]
        );
        assert_eq!(
            rules(&arguments, "--network-egress-deny"),
            ["192.0.2.7/32:tcp:8005"]
        );
    }

    #[test]
    fn inexpressible_rules_are_rejected() {
        let port = |protocol, port, end_port| EgressRule {
            to: vec![Destination::new("10.0.0.0/8")],
            ports: vec![PortSelector {
                protocol,
                port,
                end_port,
            }],
        };
        for rule in [
            port(Protocol::Icmp, Some(8), None),
            // Structural validation rejects these first, unless the backend is called directly.
            port(Protocol::Tcp, Some(90), Some(80)),
            port(Protocol::Udp, None, Some(80)),
            EgressRule {
                to: (0..300)
                    .map(|index| Destination::new(format!("10.0.{}.0/24", index % 256)))
                    .collect(),
                ports: vec![PortSelector {
                    protocol: Protocol::Tcp,
                    port: Some(1),
                    end_port: Some(2),
                }],
            },
            // 129 separate ranges of both TCP and UDP need 258 OpenVMM rules.
            EgressRule {
                to: vec![Destination::new("10.0.0.0/8")],
                ports: (0..129)
                    .map(|index| PortSelector {
                        protocol: Protocol::Any,
                        port: Some(3 * index + 1),
                        end_port: Some(3 * index + 2),
                    })
                    .collect(),
            },
        ] {
            let egress = NetworkPolicy::new(Access::Deny).with_allow(rule.clone());
            assert_eq!(
                openvmm_arguments(Some(&egress), "10.0.0.2/24")
                    .unwrap_err()
                    .kind(),
                ErrorKind::Unsupported,
                "{rule:?}"
            );
        }
    }

    #[test]
    fn exceptions_that_split_a_network_too_finely_are_rejected() {
        // Without a limit, these exceptions would split the address space into 512 * 23 networks.
        let peer = Destination {
            cidr: "0.0.0.0/0".to_owned(),
            except: (0..512u32)
                .map(|index| std::net::Ipv4Addr::from(index << 23).to_string())
                .collect(),
        };
        let rule = EgressRule {
            to: vec![peer],
            ports: Vec::new(),
        };
        let egress = NetworkPolicy::new(Access::Deny).with_allow(rule);
        let error = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(
            error.message().contains("covers more than 256 networks"),
            "{error}"
        );
    }
}
