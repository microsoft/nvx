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
//!
//! A template's capture attaches its NIC under the complete deny-all policy instead, whatever the
//! network's own: egress denied without rules, ingress and host-loopback access denied, without a
//! proxy or forwards. It is the one policy of a snapshot that OpenVMM lets a restore replace, so
//! each restore of the template installs its clone's own policy, and the clone's activation
//! record gives the guest what OpenVMM gives a VM that boots with the policy on its kernel command
//! line: its DNS server, and whether its listeners are published.

mod address;
mod arguments;
mod loopback;
mod lowering;
pub mod policy;
mod proxy;
mod rule;

pub use self::arguments::openvmm_arguments;
pub(crate) use self::arguments::{dns_server, template_arguments};
pub use self::loopback::{ForwardProtocol, HostLoopback, LoopbackForward};
pub use self::proxy::{NetworkProxy, ProxyScheme};
pub use self::rule::{Access, Destination, EgressRule, EgressRules, PortSelector, Protocol};

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

    /// Returns whether OpenVMM publishes the guest's listeners under the policy: to the host's
    /// private networks with ingress allowed, or on host loopback with host-loopback access
    /// allowed both ways. While it does, the guest announces its listeners to the gateway, and
    /// refuses its workloads `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW`, with which they could forge
    /// an announcement.
    pub fn publishes_listeners(&self) -> bool {
        self.ingress == Access::Allow || self.host_loopback == HostLoopback::Allow
    }
}

#[cfg(test)]
mod tests;
