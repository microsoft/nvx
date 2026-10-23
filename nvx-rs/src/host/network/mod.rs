//! Translation of a network posture into OpenVMM options, and the egress policy files of
//! `nvx.py`.
//!
//! OpenVMM's portable profile enforces an egress default plus IPv4 and IPv6 allow and deny rules
//! that match a network of one family and, optionally, one protocol (TCP, UDP, or ICMP) and, for
//! TCP or UDP, one port or an inclusive range of ports; deny rules take precedence.
//! [`EgressRule`]s are lowered into that form exactly, by the lowering that also compiles egress
//! policy files ([`policy`]): exceptions are subtracted from their networks, a rule without
//! destinations matches `0.0.0.0/0` and `::/0`, `any` protocol with a port or a port range becomes
//! a TCP and a UDP rule, destinations merge into the fewest networks that cover them, rules for
//! every port of a protocol subsume narrower rules, and overlapping and adjacent port ranges
//! merge. Each action emits at most [`MAX_RULES`] OpenVMM rules, sorted by family, address, prefix
//! length, protocol, and ports.
//!
//! Host-loopback access is bidirectional: with it allowed, the guest reaches host loopback
//! services through its gateway, and OpenVMM publishes the guest's listeners on host loopback.
//! [`LoopbackForward`]s publish further host loopback ports at guest ports of their choice.
//! [`HostLoopback::Outbound`] keeps OpenVMM's own default instead: the guest reaches host loopback
//! services, and the host none of the guest's listeners.
//!
//! A proxy-only network has one way out: a [`NetworkProxy`], an HTTP or HTTPS proxy that listens
//! on a port of host IPv4 loopback, which the guest reaches at the same port of its gateway.
//! OpenVMM relays the guest's TCP connections to that port, and refuses all other traffic, since
//! a policy with a proxy denies egress without rules, ingress, and host-loopback access, and has
//! no forwards. [`NetworkProxy::url`] gives the proxy's URL as the guest sees it; how workloads
//! learn it is the caller's choice.

mod address;
mod lowering;
pub mod policy;

use std::collections::HashSet;
use std::net::Ipv4Addr;

use self::address::{Family, Interval};
use self::lowering::{PortValue, Rule, RuleProtocol};
use crate::error::{Error, Result};
use crate::net::{self, cidr};

/// Largest number of allow rules, and of deny rules, that OpenVMM accepts.
pub const MAX_RULES: usize = 256;

/// Largest number of [`LoopbackForward`]s that OpenVMM accepts.
pub const MAX_LOOPBACK_FORWARDS: usize = 64;

/// Longest guest network prefix that [`HostLoopback::Allow`] accepts. OpenVMM presents a host peer
/// to the guest at an address of the guest network, and a /30 network holds only the gateway and
/// the guest.
pub const MAX_HOST_LOOPBACK_PREFIX: u8 = 29;

/// The port at which OpenVMM answers the guest's DNS queries at its gateway, over TCP as well as
/// UDP, which no [`NetworkProxy`] may take.
const DNS_PORT: u16 = 53;

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

/// Access between the guest and host loopback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostLoopback {
    /// Neither direction: the guest reaches no host loopback service other than a
    /// [`NetworkProxy`], and the host reaches none of the guest's listeners.
    Deny,
    /// From the guest alone: the guest reaches host loopback services through its gateway,
    /// subject to the egress policy, and the host reaches none of the guest's listeners. This is
    /// OpenVMM's own default, which the lowering keeps by passing no `--host-loopback` option.
    Outbound,
    /// Both directions: the guest reaches host loopback services through its gateway, subject to
    /// the egress policy, and the host reaches the guest's listeners; see
    /// [`NetworkPolicy::host_loopback`].
    Allow,
}

impl HostLoopback {
    /// Returns the name of the access: `deny`, `outbound`, or `allow`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Outbound => "outbound",
            Self::Allow => "allow",
        }
    }
}

impl From<Access> for HostLoopback {
    /// Returns the access that allows or denies both directions.
    fn from(access: Access) -> Self {
        match access {
            Access::Allow => Self::Allow,
            Access::Deny => Self::Deny,
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
fn parse_port(text: &str) -> Option<u16> {
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

impl LoopbackForward {
    /// Parses OpenVMM's `--host-loopback-forward` value: `tcp:HOST_PORT:GUEST_PORT` or
    /// `udp:HOST_PORT:GUEST_PORT`, with ports between 1 and 65535.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) and a message that
    /// starts with `value`.
    #[cfg(feature = "cli")]
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let refused = |reason: &str| Err(Error::unsupported(format!("{value}: {reason}")));
        let fields: Vec<&str> = value.split(':').collect();
        let [protocol, host_port, guest_port] = fields[..] else {
            return refused("expected tcp:HOST_PORT:GUEST_PORT or udp:HOST_PORT:GUEST_PORT");
        };
        let protocol = match protocol {
            "tcp" => ForwardProtocol::Tcp,
            "udp" => ForwardProtocol::Udp,
            _ => return refused("the protocol must be tcp or udp"),
        };
        let (Some(host_port), Some(guest_port)) = (parse_port(host_port), parse_port(guest_port))
        else {
            return refused("ports must be between 1 and 65535");
        };
        Ok(Self {
            protocol,
            host_port,
            guest_port,
        })
    }
}

/// The scheme of a [`NetworkProxy`]'s URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProxyScheme {
    /// `http`.
    Http,
    /// `https`.
    Https,
}

impl ProxyScheme {
    /// Returns the scheme's name: `http` or `https`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }

    /// Returns the scheme that `name` names: `http` or `https`, in lowercase.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for any other name.
    pub fn from_name(name: &str) -> Result<Self> {
        match name {
            "http" => Ok(Self::Http),
            "https" => Ok(Self::Https),
            _ => Err(Error::unsupported(format!(
                "network proxy scheme {name:?} must be http or https"
            ))),
        }
    }
}

/// An HTTP or HTTPS proxy that listens on host IPv4 loopback: the one way out of a proxy-only
/// network, which its guest reaches at the same port of its gateway.
///
/// OpenVMM relays the guest's TCP connections to `port` of its gateway to `127.0.0.1:port` on the
/// host, whatever they carry. The scheme only names the URL that [`url`](Self::url) gives: an
/// HTTPS proxy's certificate must therefore be valid for the gateway's address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetworkProxy {
    /// TCP port where the proxy listens on `127.0.0.1`, and where the guest reaches it at its
    /// gateway; nonzero, and not 53, where OpenVMM answers DNS at the gateway, over TCP too,
    /// before it relays a connection to a proxy.
    pub port: u16,
    /// Scheme of the proxy's URL, if any.
    pub scheme: Option<ProxyScheme>,
}

impl NetworkProxy {
    /// Creates a proxy at `port` of host loopback, whose URL has no scheme.
    pub const fn new(port: u16) -> Self {
        Self { port, scheme: None }
    }

    /// Names the scheme of the proxy's URL.
    #[must_use]
    pub const fn with_scheme(mut self, scheme: ProxyScheme) -> Self {
        self.scheme = Some(scheme);
        self
    }

    /// Parses the URL of a proxy on host loopback, as the host names it: `http` or `https`,
    /// `127.0.0.1` or `localhost`, and an explicit port, such as `http://127.0.0.1:3128`, with at
    /// most a `/` after the port.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for another scheme, a
    /// host other than IPv4 loopback, a missing or zero port, or a path, query, or fragment.
    pub fn parse_url(url: &str) -> Result<Self> {
        let refused = |reason: &str| {
            Err(Error::unsupported(format!(
                "network proxy URL {url}: {reason}"
            )))
        };
        let Some((scheme, rest)) = url.split_once("://") else {
            return refused("expected http://HOST:PORT or https://HOST:PORT");
        };
        let scheme = match ProxyScheme::from_name(scheme) {
            Ok(scheme) => scheme,
            Err(_) => return refused("the scheme must be http or https"),
        };
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        if authority.contains(['/', '?', '#']) {
            return refused("nothing but a / may follow the port");
        }
        let Some((host, port)) = authority.rsplit_once(':') else {
            return refused("the port must be explicit");
        };
        if !matches!(host, "127.0.0.1" | "localhost") {
            return refused("the host must be 127.0.0.1 or localhost");
        }
        let port = Some(port)
            .filter(|port| port.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(parse_port);
        let Some(port) = port else {
            return refused("the port must be between 1 and 65535");
        };
        Ok(Self::new(port).with_scheme(scheme))
    }

    /// Parses OpenVMM's `--network-proxy` value, `IPV4:TCP-PORT`, for a guest whose gateway is
    /// `gateway`, which the address must be. The proxy's URL has no scheme.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) and a message that
    /// starts with `endpoint`.
    #[cfg(feature = "cli")]
    pub(crate) fn parse_endpoint(endpoint: &str, gateway: Ipv4Addr) -> Result<Self> {
        let refused = |reason: String| Err(Error::unsupported(format!("{endpoint}: {reason}")));
        let Some((address, port)) = endpoint
            .rsplit_once(':')
            .and_then(|(address, port)| Some((address.parse::<Ipv4Addr>().ok()?, port)))
        else {
            return refused("expected IPV4:TCP-PORT".to_owned());
        };
        let Some(port) = parse_port(port) else {
            return refused("the port must be between 1 and 65535".to_owned());
        };
        if address != gateway {
            return refused(format!(
                "the address must be the guest's gateway, {gateway}"
            ));
        }
        Ok(Self::new(port))
    }

    /// Returns the proxy's URL as the guest sees it, at `gateway`: `SCHEME://GATEWAY:PORT`, or
    /// `GATEWAY:PORT` without a scheme.
    pub fn url(&self, gateway: Ipv4Addr) -> String {
        match self.scheme {
            Some(scheme) => format!("{}://{gateway}:{}", scheme.as_str(), self.port),
            None => format!("{gateway}:{}", self.port),
        }
    }
}

/// The network posture of a VM.
///
/// A connection matching a `deny` rule is blocked. Otherwise a connection matching an `allow`
/// rule is permitted, and any other connection follows the `egress` default. Access between the
/// guest and host loopback follows `host_loopback`. A policy with a `proxy` is proxy-only: the
/// proxy is the guest's one way out.
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
    /// Access between the guest and host loopback. With [`HostLoopback::Allow`], the guest
    /// reaches host loopback services through its gateway, subject to the egress policy, and the
    /// host reaches the guest's TCP and UDP listeners on non-loopback guest addresses at the same
    /// ports of host IPv4 and IPv6 loopback, where OpenVMM listens without displacing a host
    /// listener and admits only the host; replies to the host pass whatever the egress policy.
    /// With `ingress` allowed too, the listeners on every host address admit the host instead.
    /// [`HostLoopback::Outbound`] opens only the guest's direction, and [`HostLoopback::Deny`]
    /// neither. [`HostLoopback::Allow`] needs a guest network prefix of at most
    /// [`MAX_HOST_LOOPBACK_PREFIX`].
    pub host_loopback: HostLoopback,
    /// Ports on host IPv4 loopback forwarded to guest ports, at most [`MAX_LOOPBACK_FORWARDS`],
    /// each protocol and host port at most once, and each protocol and guest port at most once.
    /// Forwards need `host_loopback` [`HostLoopback::Allow`]. OpenVMM lets the guest's replies on
    /// a forwarded TCP connection through whatever the egress policy, but filters its UDP
    /// replies, which go to addresses that OpenVMM assigns inside the guest network, so UDP
    /// forwards also need an `egress` default of [`Access::Allow`] and no `deny` rule that
    /// covers the guest network.
    pub host_loopback_forwards: Vec<LoopbackForward>,
    /// The proxy that is the guest's one way out, if any. A policy with a proxy denies egress by
    /// default without `allow` or `deny` rules, denies ingress and host-loopback access, and has
    /// no host-loopback forwards: direct egress and a proxy are separate connectivity models.
    pub proxy: Option<NetworkProxy>,
}

impl NetworkPolicy {
    /// Applies `egress` as the default egress policy, with no rules, and denies ingress and
    /// host-loopback access, without a proxy. Messages call the rules `egress.allow` and
    /// `egress.deny`.
    pub fn new(egress: Access) -> Self {
        Self {
            egress,
            allow: EgressRules::new("egress.allow"),
            deny: EgressRules::new("egress.deny"),
            ingress: Access::Deny,
            host_loopback: HostLoopback::Deny,
            host_loopback_forwards: Vec::new(),
            proxy: None,
        }
    }

    /// Returns the proxy-only policy whose one way out is `proxy`: egress denied without rules,
    /// ingress and host-loopback access denied, and no forwards. Messages call the rules
    /// `egress.allow` and `egress.deny`.
    pub fn proxy_only(proxy: NetworkProxy) -> Self {
        Self {
            proxy: Some(proxy),
            ..Self::new(Access::Deny)
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
/// Without a policy, or when egress is denied without allow rules, ingress and host-loopback
/// access are denied, and there is no proxy, no network device is attached. A proxy-only policy
/// attaches the portable profile with egress, ingress, and host-loopback access denied, and the
/// proxy at the guest network's gateway (`--network-proxy GATEWAY:PORT`). Otherwise the portable
/// profile applies the egress posture, the ingress default, the host-loopback access, which
/// [`HostLoopback::Outbound`] leaves to OpenVMM's default, and the host-loopback forwards. Each
/// action's rules are lowered as an egress policy file's are ([`policy`]), and fail with its
/// messages, which name the rules after [`EgressRules::name`].
///
/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) when a rule is invalid,
/// when the rules of one action need more than [`MAX_RULES`] OpenVMM rules, when host-loopback
/// access is allowed for a guest network whose prefix is longer than
/// [`MAX_HOST_LOOPBACK_PREFIX`], when the host-loopback forwards break the rules of
/// [`NetworkPolicy::host_loopback_forwards`], or when a policy with a proxy is not proxy-only
/// ([`NetworkPolicy::proxy`]), its proxy's port is one that [`NetworkProxy::port`] excludes, or
/// its guest network has no gateway.
pub fn openvmm_arguments(
    policy: Option<&NetworkPolicy>,
    guest_network: &str,
) -> Result<Vec<String>> {
    let Some(policy) = policy else {
        return Ok(Vec::new());
    };
    if let Some(proxy) = &policy.proxy {
        return proxy_arguments(policy, proxy, guest_network);
    }
    if !policy.host_loopback_forwards.is_empty() && policy.host_loopback != HostLoopback::Allow {
        return Err(Error::unsupported(
            "host-loopback forwards need host-loopback access allowed",
        ));
    }
    // OpenVMM presents a host peer to the guest at an address of the guest network, which a /30
    // network, holding only the gateway and the guest, cannot spare.
    if policy.host_loopback == HostLoopback::Allow
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
        && policy.host_loopback == HostLoopback::Deny
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
    ]
    .map(str::to_owned)
    .to_vec();
    // Without the option, OpenVMM lets the guest alone reach host loopback.
    if policy.host_loopback != HostLoopback::Outbound {
        arguments.push("--host-loopback".to_owned());
        arguments.push(policy.host_loopback.as_str().to_owned());
    }
    // As in an egress policy file, the allow rules are checked and lowered before the deny rules.
    let mut deny = Vec::new();
    for (rules, option) in [
        (&policy.allow, "--network-egress-allow"),
        (&policy.deny, "--network-egress-deny"),
    ] {
        let parsed = parse_rules(rules)?;
        let lowered = lowering::lower(&parsed.concat(), &rules.name)?;
        for rule in lowered {
            arguments.push(option.to_owned());
            arguments.push(rule);
        }
        deny = parsed;
    }
    check_loopback_forwards(policy, &deny, guest_network)?;
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

/// Returns the OpenVMM options of a policy with a proxy, which must be proxy-only, for a guest at
/// `guest_network`: everything denied but the proxy, which the guest reaches at its gateway.
fn proxy_arguments(
    policy: &NetworkPolicy,
    proxy: &NetworkProxy,
    guest_network: &str,
) -> Result<Vec<String>> {
    check_proxy_only(policy, proxy)?;
    let Some(gateway) = net::guest_gateway(guest_network) else {
        return Err(Error::unsupported(format!(
            "guest network {guest_network:?} is not valid"
        )));
    };
    let mut arguments: Vec<String> = [
        "--net",
        guest_network,
        "--network-profile",
        "portable",
        "--network-egress",
        "deny",
        "--network-ingress",
        "deny",
        "--host-loopback",
        "deny",
        "--network-proxy",
    ]
    .map(str::to_owned)
    .to_vec();
    arguments.push(format!("{gateway}:{}", proxy.port));
    Ok(arguments)
}

/// Checks that a policy with `proxy` is proxy-only, as [`NetworkPolicy::proxy`] describes it, and
/// that its port is one that [`NetworkProxy::port`] allows.
fn check_proxy_only(policy: &NetworkPolicy, proxy: &NetworkProxy) -> Result<()> {
    let refused = |message: String| Err(Error::unsupported(message));
    if proxy.port == 0 {
        return refused("a network proxy needs a port between 1 and 65535".to_owned());
    }
    // OpenVMM's DNS service at the gateway takes the guest's TCP connections to this port before
    // the proxy's relay does: the proxy would receive nothing, and the guest would reach the
    // host's resolver instead.
    if proxy.port == DNS_PORT {
        return refused(format!(
            "a network proxy needs a port other than {DNS_PORT}, where OpenVMM answers DNS at the \
             guest's gateway"
        ));
    }
    if policy.egress != Access::Deny {
        return refused(
            "a network proxy needs egress denied by default, since the proxy is the guest's only \
             way out"
                .to_owned(),
        );
    }
    for rules in [&policy.allow, &policy.deny] {
        if !rules.rules.is_empty() {
            return refused(format!(
                "a network proxy takes no {} rules, since the proxy is the guest's only way out",
                rules.name
            ));
        }
    }
    if policy.ingress != Access::Deny {
        return refused("a network proxy needs ingress denied".to_owned());
    }
    if policy.host_loopback != HostLoopback::Deny {
        return refused("a network proxy needs host-loopback access denied".to_owned());
    }
    if !policy.host_loopback_forwards.is_empty() {
        return refused("a network proxy takes no host-loopback forwards".to_owned());
    }
    Ok(())
}

/// Parses the rules of one action, each into the rules that the lowering takes, as an egress
/// policy file's MXC rules are parsed: an empty `to` or `ports` is a rule without that field.
fn parse_rules(rules: &EgressRules) -> Result<Vec<Vec<Rule>>> {
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

/// Checks the host-loopback forwards of a policy whose rules lower, and whose host-loopback
/// access is allowed when it has forwards, against the rules of
/// [`NetworkPolicy::host_loopback_forwards`]. `deny` holds the parsed deny rules.
fn check_loopback_forwards(
    policy: &NetworkPolicy,
    deny: &[Vec<Rule>],
    guest_network: &str,
) -> Result<()> {
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
    if forwards
        .iter()
        .all(|forward| forward.protocol != ForwardProtocol::Udp)
    {
        // OpenVMM lets the guest's replies on a forwarded TCP connection through whatever the
        // egress policy, since it carries that connection.
        return Ok(());
    }
    if policy.egress != Access::Allow {
        return unsupported(
            "host-loopback UDP forwards need egress allowed by default for their replies"
                .to_owned(),
        );
    }
    let Some((first, last)) = guest_subnet(guest_network) else {
        return unsupported(format!("guest network {guest_network:?} is not valid"));
    };
    // A deny rule covers the guest network if it selects one of its addresses, on any protocol.
    for (index, rules) in deny.iter().enumerate() {
        let covers = rules.iter().any(|rule| {
            rule.family == Family::V4
                && rule
                    .addresses
                    .iter()
                    .any(|&(start, end)| start <= last && first <= end)
        });
        if covers {
            return unsupported(format!(
                "{}[{index}] cannot cover the guest network, which carries host-loopback UDP \
                 forward replies",
                policy.deny.name
            ));
        }
    }
    Ok(())
}

/// Returns the addresses of the IPv4 network of `guest_network`, a static guest address in
/// `ADDRESS/PREFIX` form.
fn guest_subnet(guest_network: &str) -> Option<Interval> {
    let (address, prefix) = guest_network.split_once('/')?;
    let address: Ipv4Addr = address.parse().ok()?;
    let prefix = cidr::parse_prefix_length(prefix).filter(|prefix| (1..=32).contains(prefix))?;
    let mask = u32::MAX << (32 - u32::from(prefix));
    let network = u32::from(address) & mask;
    Some((u128::from(network), u128::from(network | !mask)))
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
        host_loopback.host_loopback = HostLoopback::Allow;
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
        // So does outbound-only access, which OpenVMM's default provides.
        host_loopback.host_loopback = HostLoopback::Outbound;
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
            ]
        );
    }

    #[test]
    fn host_loopback_access_needs_room_for_host_peers() {
        let mut policy = NetworkPolicy::new(Access::Deny);
        policy.host_loopback = HostLoopback::Allow;
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
        // Host-loopback access that is denied, or open only to the guest, needs no host peers.
        let mut denied = NetworkPolicy::new(Access::Allow);
        denied.host_loopback = HostLoopback::Deny;
        let arguments = openvmm_arguments(Some(&denied), "10.0.0.2/30").unwrap();
        assert!(has_pair(&arguments, "--host-loopback", "deny"));
        denied.host_loopback = HostLoopback::Outbound;
        let arguments = openvmm_arguments(Some(&denied), "10.0.0.2/30").unwrap();
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "--host-loopback")
        );
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
            policy.host_loopback = HostLoopback::Allow;
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
        denied.host_loopback = HostLoopback::Deny;
        rejected(&denied, "need host-loopback access allowed");
        // Forwards publish host ports to the guest, which outbound-only access does not.
        let mut outbound = valid.clone();
        outbound.host_loopback = HostLoopback::Outbound;
        rejected(&outbound, "need host-loopback access allowed");
        let mut closed = valid.clone();
        closed.egress = Access::Deny;
        rejected(
            &closed,
            "host-loopback UDP forwards need egress allowed by default for their replies",
        );
        // OpenVMM lets the guest's replies on a forwarded TCP connection through whatever the
        // egress policy, so TCP forwards alone need neither egress allowed by default nor a guest
        // network that no deny rule covers.
        let mut tcp = forwarded(vec![forward(ForwardProtocol::Tcp, 18080, 8080)]);
        tcp.egress = Access::Deny;
        let tcp = tcp.with_deny(EgressRule::to("10.0.0.0/24"));
        let arguments = openvmm_arguments(Some(&tcp), "10.0.0.2/24").unwrap();
        assert!(has_pair(&arguments, "--network-egress", "deny"));
        assert_eq!(rules(&arguments, "--network-egress-deny"), ["10.0.0.0/24"]);
        assert_eq!(
            rules(&arguments, "--host-loopback-forward"),
            ["tcp:18080:8080"]
        );
        // A UDP forward beside them needs both.
        let mut udp = tcp.clone();
        udp.host_loopback_forwards
            .push(forward(ForwardProtocol::Udp, 18081, 8081));
        rejected(
            &udp,
            "host-loopback UDP forwards need egress allowed by default",
        );
        udp.egress = Access::Allow;
        rejected(
            &udp,
            "egress.deny[0] cannot cover the guest network, which carries host-loopback UDP \
             forward replies",
        );
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
            // Networks are read as egress policy files read them, host bits and netmasks included.
            EgressRule::to("10.0.0.77/24"),
            EgressRule::to("10.0.0.0/255.255.255.0"),
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
    fn ipv6_rules_lower_exactly() {
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
        // Rules sort by family and then by address, so `::/0` leads the IPv6 rules.
        assert_eq!(
            rules(&arguments, "--network-egress-allow"),
            [
                "192.0.2.0/24:icmp",
                "::/0:icmp",
                "2001:db8:1::/64:tcp:443",
                "2001:db8:2::/128:tcp:53",
                "2001:db8:2::/128:udp:53",
                "2001:db8:2::2/127:tcp:53",
                "2001:db8:2::2/127:udp:53",
            ]
        );
        assert_eq!(
            rules(&arguments, "--network-egress-deny"),
            ["2001:db8:1::123/128"]
        );
    }

    #[test]
    fn rules_lower_like_egress_policy_files() {
        let lowered = |rules: Vec<EgressRule>| {
            let mut egress = NetworkPolicy::new(Access::Deny);
            egress.allow.rules = rules;
            let arguments = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap();
            self::rules(&arguments, "--network-egress-allow")
        };
        let tcp = |cidr: &str, port, end_port| {
            EgressRule::to(cidr).on_port_range(Protocol::Tcp, port, end_port)
        };
        // Destinations merge into the fewest networks, and port ranges into one range.
        assert_eq!(
            lowered(vec![EgressRule {
                to: (0..300)
                    .map(|index| Destination::new(format!("10.0.{}.0/24", index % 256)))
                    .collect(),
                ports: vec![PortSelector {
                    protocol: Protocol::Tcp,
                    port: Some(1),
                    end_port: Some(2),
                }],
            }]),
            ["10.0.0.0/16:tcp:1-2"]
        );
        assert_eq!(
            lowered(vec![
                tcp("192.0.2.7", 8000, 8004),
                tcp("192.0.2.7", 8003, 8010),
                tcp("192.0.2.6", 8000, 8010),
            ]),
            ["192.0.2.6/31:tcp:8000-8010"]
        );
        // Rules for every port of a protocol, or every protocol, subsume narrower ones.
        assert_eq!(
            lowered(vec![
                EgressRule::to("192.0.2.7").on_port(Protocol::Tcp, 443),
                EgressRule::to("192.0.2.0/24").on_protocol(Protocol::Tcp),
                EgressRule::to("198.51.100.0/24").on_port(Protocol::Udp, 53),
                EgressRule::to("198.51.100.0/23"),
            ]),
            ["192.0.2.0/24:tcp", "198.51.100.0/23"]
        );
        // Host bits are cleared, as an egress policy file's are.
        assert_eq!(
            lowered(vec![EgressRule::to("10.0.0.5/255.255.255.0")]),
            ["10.0.0.0/24"]
        );
    }

    #[test]
    fn invalid_rules_are_rejected_with_the_policy_compilers_messages() {
        let port = |protocol, port, end_port| EgressRule {
            to: vec![Destination::new("10.0.0.0/8")],
            ports: vec![PortSelector {
                protocol,
                port,
                end_port,
            }],
        };
        let excluding = |cidr: &str, except: &str| EgressRule {
            to: vec![
                Destination::new("192.0.2.0/24"),
                Destination {
                    cidr: cidr.to_owned(),
                    except: vec![except.to_owned()],
                },
            ],
            ports: Vec::new(),
        };
        for (rule, message) in [
            (
                port(Protocol::Icmp, Some(8), None),
                "egress.allow[0].ports[0].port is not supported with icmp",
            ),
            // Structural validation rejects these first, unless the backend is called directly.
            (
                port(Protocol::Tcp, Some(90), Some(80)),
                "egress.allow[0].ports[0].endPort 80 cannot be below port 90",
            ),
            (
                port(Protocol::Udp, None, Some(80)),
                "egress.allow[0].ports[0].endPort requires port",
            ),
            (
                port(Protocol::Tcp, Some(0), None),
                "egress.allow[0].ports[0].port must be between 1 and 65535",
            ),
            (
                port(Protocol::Any, Some(80), Some(0)),
                "egress.allow[0].ports[0].endPort must be between 1 and 65535",
            ),
            (
                EgressRule::to("example.com"),
                "egress.allow[0].to[0].cidr is not a valid IPv4 or IPv6 CIDR: \"example.com\"",
            ),
            (
                EgressRule::to("fe80::1%eth0/64"),
                "egress.allow[0].to[0].cidr must not name an IPv6 scope: \"fe80::1%eth0/64\"",
            ),
            (
                excluding("10.0.0.0/8", "11.0.0.0/16"),
                "egress.allow[0].to[1].except[0] must be contained in 10.0.0.0/8",
            ),
            (
                excluding("10.0.0.7/8", "10.1.0.0/+16"),
                "egress.allow[0].to[1].except[0] is not a valid IPv4 or IPv6 CIDR: \"10.1.0.0/+16\"",
            ),
            // 129 separate ranges of both TCP and UDP need 258 OpenVMM rules.
            (
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
                "egress.allow emits at most 256 native rules",
            ),
        ] {
            let egress = NetworkPolicy::new(Access::Deny).with_allow(rule.clone());
            let error = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{rule:?}");
            assert_eq!(error.message(), message, "{rule:?}");
        }
        // The allow rules are checked and lowered before the deny rules.
        let egress = NetworkPolicy::new(Access::Allow)
            .with_allow(EgressRule::to("10.0.0.0/33"))
            .with_deny(port(Protocol::Icmp, Some(8), None));
        let error = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap_err();
        assert_eq!(
            error.message(),
            "egress.allow[0].to[0].cidr is not a valid IPv4 or IPv6 CIDR: \"10.0.0.0/33\""
        );
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
        assert_eq!(
            error.message(),
            "egress.allow emits at most 256 native rules"
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

    /// The proxy-only policy of a proxy at port 3128 of host loopback.
    fn proxy_only() -> NetworkPolicy {
        NetworkPolicy::proxy_only(NetworkProxy::new(3128))
    }

    #[test]
    fn proxy_only_policies_reach_the_proxy_at_the_gateway() {
        for (guest_network, endpoint) in [
            ("10.0.0.2/24", "10.0.0.1:3128"),
            ("172.16.40.9/16", "172.16.0.1:3128"),
            // A /30 network holds the gateway and the guest, which is all that a proxy needs.
            ("192.168.7.6/30", "192.168.7.5:3128"),
        ] {
            assert_eq!(
                openvmm_arguments(Some(&proxy_only()), guest_network).unwrap(),
                [
                    "--net",
                    guest_network,
                    "--network-profile",
                    "portable",
                    "--network-egress",
                    "deny",
                    "--network-ingress",
                    "deny",
                    "--host-loopback",
                    "deny",
                    "--network-proxy",
                    endpoint,
                ]
            );
        }
        // The scheme names only the URL.
        let https =
            NetworkPolicy::proxy_only(NetworkProxy::new(3128).with_scheme(ProxyScheme::Https));
        assert_eq!(
            openvmm_arguments(Some(&https), "10.0.0.2/24").unwrap(),
            openvmm_arguments(Some(&proxy_only()), "10.0.0.2/24").unwrap()
        );
        let mut built = NetworkPolicy::new(Access::Deny);
        built.proxy = Some(NetworkProxy::new(3128));
        assert_eq!(built, proxy_only());
    }

    #[test]
    fn proxies_refuse_every_other_way_out() {
        let refused = |edit: &dyn Fn(&mut NetworkPolicy), message: &str| {
            let mut policy = proxy_only();
            edit(&mut policy);
            let error = openvmm_arguments(Some(&policy), "10.0.0.2/24").unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported);
            assert_eq!(error.message(), message);
        };
        let only_way_out = "since the proxy is the guest's only way out";
        refused(
            &|policy| policy.egress = Access::Allow,
            &format!("a network proxy needs egress denied by default, {only_way_out}"),
        );
        refused(
            &|policy| policy.allow.rules.push(EgressRule::to("192.0.2.0/24")),
            &format!("a network proxy takes no egress.allow rules, {only_way_out}"),
        );
        refused(
            &|policy| policy.deny.rules.push(EgressRule::to("192.0.2.0/24")),
            &format!("a network proxy takes no egress.deny rules, {only_way_out}"),
        );
        refused(
            &|policy| policy.ingress = Access::Allow,
            "a network proxy needs ingress denied",
        );
        for host_loopback in [HostLoopback::Allow, HostLoopback::Outbound] {
            refused(
                &|policy| policy.host_loopback = host_loopback,
                "a network proxy needs host-loopback access denied",
            );
        }
        refused(
            &|policy| {
                policy.host_loopback_forwards.push(LoopbackForward {
                    protocol: ForwardProtocol::Tcp,
                    host_port: 18080,
                    guest_port: 8080,
                });
            },
            "a network proxy takes no host-loopback forwards",
        );
        refused(
            &|policy| policy.proxy = Some(NetworkProxy::new(0)),
            "a network proxy needs a port between 1 and 65535",
        );
        // OpenVMM's DNS service at the gateway takes the guest's TCP connections to port 53
        // before the proxy's relay does, whatever the proxy's scheme.
        refused(
            &|policy| policy.proxy = Some(NetworkProxy::new(53).with_scheme(ProxyScheme::Http)),
            "a network proxy needs a port other than 53, where OpenVMM answers DNS at the guest's \
             gateway",
        );
        // The proxy's checks come first: a proxy beside allowed forwards is refused as a proxy.
        refused(
            &|policy| {
                policy.egress = Access::Allow;
                policy.host_loopback = HostLoopback::Allow;
                policy.host_loopback_forwards.push(LoopbackForward {
                    protocol: ForwardProtocol::Tcp,
                    host_port: 18080,
                    guest_port: 8080,
                });
            },
            &format!("a network proxy needs egress denied by default, {only_way_out}"),
        );
        // The guest reaches the proxy at its gateway, which an invalid guest network lacks.
        for guest_network in [
            "10.0.0.1/24",
            "10.0.0.0/24",
            "10.0.0.255/24",
            "10.0.0.2/31",
            "10.0.0.2",
            "fd00::2/64",
        ] {
            let error = openvmm_arguments(Some(&proxy_only()), guest_network).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported);
            assert_eq!(
                error.message(),
                format!("guest network {guest_network:?} is not valid")
            );
        }
    }

    #[test]
    fn proxy_urls_name_the_gateway_as_the_guest_sees_it() {
        let gateway = Ipv4Addr::new(10, 0, 0, 1);
        assert_eq!(NetworkProxy::new(3128).url(gateway), "10.0.0.1:3128");
        assert_eq!(
            NetworkProxy::new(3128)
                .with_scheme(ProxyScheme::Http)
                .url(gateway),
            "http://10.0.0.1:3128"
        );
        assert_eq!(
            NetworkProxy::new(8443)
                .with_scheme(ProxyScheme::Https)
                .url(gateway),
            "https://10.0.0.1:8443"
        );
    }

    #[test]
    fn proxy_urls_name_a_port_of_host_loopback() {
        let http = |port| NetworkProxy::new(port).with_scheme(ProxyScheme::Http);
        let https = |port| NetworkProxy::new(port).with_scheme(ProxyScheme::Https);
        for (url, proxy) in [
            ("http://127.0.0.1:3128", http(3128)),
            ("http://localhost:3128/", http(3128)),
            ("https://127.0.0.1:65535", https(65535)),
            ("https://localhost:1", https(1)),
        ] {
            assert_eq!(NetworkProxy::parse_url(url).unwrap(), proxy, "{url}");
        }
        for (url, reason) in [
            (
                "127.0.0.1:3128",
                "expected http://HOST:PORT or https://HOST:PORT",
            ),
            ("ftp://127.0.0.1:3128", "the scheme must be http or https"),
            ("HTTP://127.0.0.1:3128", "the scheme must be http or https"),
            (
                "http://10.0.0.1:3128",
                "the host must be 127.0.0.1 or localhost",
            ),
            (
                "http://[::1]:3128",
                "the host must be 127.0.0.1 or localhost",
            ),
            (
                "http://user@localhost:3128",
                "the host must be 127.0.0.1 or localhost",
            ),
            ("http://127.0.0.1", "the port must be explicit"),
            ("http://127.0.0.1:", "the port must be between 1 and 65535"),
            ("http://127.0.0.1:0", "the port must be between 1 and 65535"),
            (
                "http://127.0.0.1:65536",
                "the port must be between 1 and 65535",
            ),
            (
                "http://127.0.0.1:+3128",
                "the port must be between 1 and 65535",
            ),
            (
                "http://127.0.0.1:3128/path",
                "nothing but a / may follow the port",
            ),
            (
                "http://127.0.0.1:3128?query",
                "nothing but a / may follow the port",
            ),
            (
                "http://127.0.0.1:3128//",
                "nothing but a / may follow the port",
            ),
        ] {
            let error = NetworkProxy::parse_url(url).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{url}");
            assert_eq!(
                error.message(),
                format!("network proxy URL {url}: {reason}")
            );
        }
        assert_eq!(ProxyScheme::from_name("http").unwrap(), ProxyScheme::Http);
        assert_eq!(ProxyScheme::from_name("https").unwrap(), ProxyScheme::Https);
        let error = ProxyScheme::from_name("ftp").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert_eq!(
            error.message(),
            "network proxy scheme \"ftp\" must be http or https"
        );
    }

    #[test]
    #[cfg(feature = "cli")]
    fn proxy_endpoints_name_the_guest_gateway() {
        let gateway = Ipv4Addr::new(10, 0, 0, 1);
        assert_eq!(
            NetworkProxy::parse_endpoint("10.0.0.1:3128", gateway).unwrap(),
            NetworkProxy::new(3128)
        );
        assert_eq!(
            NetworkProxy::parse_endpoint("192.168.7.5:8080", Ipv4Addr::new(192, 168, 7, 5))
                .unwrap(),
            NetworkProxy::new(8080)
        );
        for (endpoint, reason) in [
            (
                "198.51.100.1:3128",
                "the address must be the guest's gateway, 10.0.0.1",
            ),
            (
                "10.0.0.2:3128",
                "the address must be the guest's gateway, 10.0.0.1",
            ),
            ("10.0.0.1", "expected IPV4:TCP-PORT"),
            ("proxy:3128", "expected IPV4:TCP-PORT"),
            ("[::1]:3128", "expected IPV4:TCP-PORT"),
            ("10.0.0.1:", "the port must be between 1 and 65535"),
            ("10.0.0.1:0", "the port must be between 1 and 65535"),
            ("10.0.0.1:65536", "the port must be between 1 and 65535"),
        ] {
            let error = NetworkProxy::parse_endpoint(endpoint, gateway).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{endpoint}");
            assert_eq!(error.message(), format!("{endpoint}: {reason}"));
        }
    }

    #[test]
    #[cfg(feature = "cli")]
    fn native_rules_parse_as_openvmm_parses_them() {
        let parsed = |value: &str| EgressRule::parse_native(value).unwrap();
        for (value, rule) in [
            ("10.0.0.0/8", EgressRule::to("10.0.0.0/8")),
            // Host bits are cleared, and an address is a network of one address.
            (
                "10.0.0.5/8:tcp",
                EgressRule::to("10.0.0.0/8").on_protocol(Protocol::Tcp),
            ),
            (
                "192.0.2.7:udp:53",
                EgressRule::to("192.0.2.7/32").on_port(Protocol::Udp, 53),
            ),
            (
                "192.0.2.7:tcp:8000-8010",
                EgressRule::to("192.0.2.7/32").on_port_range(Protocol::Tcp, 8000, 8010),
            ),
            // A range of one port is that port.
            (
                "192.0.2.7:tcp:443-443",
                EgressRule::to("192.0.2.7/32").on_port(Protocol::Tcp, 443),
            ),
            (
                "192.0.2.0/24:icmp",
                EgressRule::to("192.0.2.0/24").on_protocol(Protocol::Icmp),
            ),
            (
                "::/0:icmp",
                EgressRule::to("::/0").on_protocol(Protocol::Icmp),
            ),
            (
                "2001:db8::/32:tcp:443",
                EgressRule::to("2001:db8::/32").on_port(Protocol::Tcp, 443),
            ),
            ("2001:db8::1", EgressRule::to("2001:db8::1/128")),
            // Protocol names are not hexadecimal, so they never belong to an IPv6 address.
            (
                "2001:db8::1:tcp:443",
                EgressRule::to("2001:db8::1/128").on_port(Protocol::Tcp, 443),
            ),
        ] {
            assert_eq!(parsed(value), rule, "{value}");
        }
        let destination = "the destination must be an IPv4 or IPv6 address with an optional prefix";
        let ports = "ports must be between 1 and 65535";
        for (value, reason) in [
            (
                "10.0.0.0/8:tcp:80:90",
                "expected CIDR[:PROTOCOL[:PORT[-PORT]]]",
            ),
            ("10.0.0.0/8:sctp", "the protocol must be tcp, udp, or icmp"),
            ("10.0.0.0/8:TCP", "the protocol must be tcp, udp, or icmp"),
            ("10.0.0.0/8:", "the protocol must be tcp, udp, or icmp"),
            ("10.0.0.0/8:icmp:8", "icmp takes no port"),
            ("10.0.0.0/8:tcp:0", ports),
            ("10.0.0.0/8:tcp:65536", ports),
            ("10.0.0.0/8:tcp:http", ports),
            ("10.0.0.0/8:udp:0-80", ports),
            (
                "10.0.0.0/8:tcp:90-80",
                "the port range ends below its first port",
            ),
            ("10.0.0.0/8:tcp:-80", "a port range must be FIRST-LAST"),
            ("10.0.0.0/8:tcp:80-", "a port range must be FIRST-LAST"),
            (
                "10.0.0.0/8:tcp:80-90-100",
                "a port range must be FIRST-LAST",
            ),
            ("10.0.0.0/33", destination),
            ("10.0.0.0/255.0.0.0", destination),
            ("example.com", destination),
            ("example.com:tcp:443", destination),
            ("fe80::1%eth0", destination),
        ] {
            let error = EgressRule::parse_native(value).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{value}");
            assert_eq!(error.message(), format!("{value}: {reason}"));
        }
    }

    #[test]
    #[cfg(feature = "cli")]
    fn lowered_rules_parse_back_to_the_same_rules() {
        let egress = NetworkPolicy::new(Access::Deny)
            .with_allow(EgressRule {
                to: vec![Destination {
                    cidr: "10.0.0.0/30".to_owned(),
                    except: vec!["10.0.0.1".to_owned()],
                }],
                ports: vec![PortSelector {
                    protocol: Protocol::Any,
                    port: Some(80),
                    end_port: Some(81),
                }],
            })
            .with_allow(EgressRule::to("2001:db8::/32").on_protocol(Protocol::Icmp))
            .with_allow(EgressRule::to("198.51.100.0/24"))
            .with_deny(EgressRule::to("2001:db8::123").on_port(Protocol::Udp, 53));
        let arguments = openvmm_arguments(Some(&egress), "10.0.0.2/24").unwrap();
        let mut parsed = NetworkPolicy::new(Access::Deny);
        for (option, rules) in [
            ("--network-egress-allow", &mut parsed.allow),
            ("--network-egress-deny", &mut parsed.deny),
        ] {
            for rule in self::rules(&arguments, option) {
                rules.rules.push(EgressRule::parse_native(&rule).unwrap());
            }
        }
        assert_eq!(
            openvmm_arguments(Some(&parsed), "10.0.0.2/24").unwrap(),
            arguments
        );
    }

    #[test]
    #[cfg(feature = "cli")]
    fn loopback_forwards_parse_as_openvmm_parses_them() {
        for (value, protocol, host_port, guest_port) in [
            ("tcp:18080:8080", ForwardProtocol::Tcp, 18080, 8080),
            ("udp:1:65535", ForwardProtocol::Udp, 1, 65535),
        ] {
            assert_eq!(
                LoopbackForward::parse(value).unwrap(),
                LoopbackForward {
                    protocol,
                    host_port,
                    guest_port
                }
            );
        }
        let format = "expected tcp:HOST_PORT:GUEST_PORT or udp:HOST_PORT:GUEST_PORT";
        let ports = "ports must be between 1 and 65535";
        for (value, reason) in [
            ("8080", format),
            ("tcp:8080", format),
            ("tcp:8080:80:90", format),
            ("sctp:8080:80", "the protocol must be tcp or udp"),
            ("tcp:0:80", ports),
            ("tcp:8080:0", ports),
            ("udp:65536:80", ports),
            ("udp:8080:http", ports),
        ] {
            let error = LoopbackForward::parse(value).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{value}");
            assert_eq!(error.message(), format!("{value}: {reason}"));
        }
    }
}
