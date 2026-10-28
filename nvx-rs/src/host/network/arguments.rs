//! The OpenVMM options that enforce a network posture.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::address::{Family, Interval};
use super::loopback::check_loopback_forwards;
use super::lowering::{self, NativeProtocol, Rule};
use super::rule::parse_rules;
use super::{
    Access, DNS_PORT, HostLoopback, MAX_HOST_LOOPBACK_PREFIX, NetworkPolicy, NetworkProxy,
};
use crate::error::{Error, Result};
use crate::net::{self, cidr};

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
///
/// [`policy`]: crate::host::network::policy
/// [`EgressRules::name`]: crate::host::network::EgressRules::name
/// [`MAX_RULES`]: crate::host::network::MAX_RULES
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

/// Returns the OpenVMM options with which a template's capture attaches its NIC to a guest at
/// `guest_network`: the portable profile under the complete deny-all policy, egress denied
/// without rules, ingress and host-loopback access denied, without a proxy or forwards.
///
/// It is the one policy of a snapshot that OpenVMM lets a restore replace, with
/// `--restore-network-policy` and the options of the new policy: under it, the guest learned no
/// DNS server and announces no listeners, and the NIC's endpoint held no flows, listeners, or
/// forwards, so nothing that the snapshot holds depends on the policy beyond the NIC's identity.
pub(crate) fn template_arguments(guest_network: &str) -> Vec<String> {
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
    ]
    .map(str::to_owned)
    .to_vec()
}

/// Returns the DNS server that OpenVMM names to a guest under `policy`, whose NIC's IPv4 gateway
/// is `gateway`, and whose IPv6 gateway is `ipv6_gateway` if its NIC has an IPv6 identity, as
/// OpenVMM chooses it: the IPv4 gateway if the policy lets the guest reach its DNS service, TCP
/// or UDP port 53, and otherwise the IPv6 gateway if it lets the guest reach that one's, and none
/// if it lets the guest reach neither, as a proxy-only policy does. A deny rule that matches the
/// traffic denies it, and otherwise an allow rule that matches it, or the egress default, allows
/// it. Denied host-loopback access leaves the gateway's DNS service to the egress policy.
///
/// OpenVMM names the server on the kernel command line of a VM that boots with the policy, and a
/// clone whose restore installs the policy learns it from its activation record instead.
///
/// Fails as [`openvmm_arguments`] does for rules that it cannot parse.
pub(crate) fn dns_server(
    policy: &NetworkPolicy,
    gateway: Ipv4Addr,
    ipv6_gateway: Option<Ipv6Addr>,
) -> Result<Option<IpAddr>> {
    if policy.proxy.is_some() {
        return Ok(None);
    }
    let allow = parse_rules(&policy.allow)?.concat();
    let deny = parse_rules(&policy.deny)?.concat();
    let reaches = |server: IpAddr| {
        [NativeProtocol::Tcp, NativeProtocol::Udp]
            .into_iter()
            .any(|protocol| {
                let selected = |rule: &Rule| selects(rule, server, protocol, DNS_PORT);
                !deny.iter().any(selected)
                    && (allow.iter().any(selected) || policy.egress == Access::Allow)
            })
    };
    Ok([Some(IpAddr::V4(gateway)), ipv6_gateway.map(IpAddr::V6)]
        .into_iter()
        .flatten()
        .find(|&server| reaches(server)))
}

/// Returns whether `rule` selects traffic of `protocol` to `port` of `address`, as OpenVMM's rule
/// selects it: an address of the rule's family among its addresses, and any traffic for a rule of
/// every protocol, or traffic of its protocol to one of its ports, or to any for a rule without
/// ports.
fn selects(rule: &Rule, address: IpAddr, protocol: NativeProtocol, port: u16) -> bool {
    let (family, address) = match address {
        IpAddr::V4(address) => (Family::V4, u128::from(u32::from(address))),
        IpAddr::V6(address) => (Family::V6, u128::from(address)),
    };
    rule.family == family
        && rule
            .addresses
            .iter()
            .any(|&(first, last)| first <= address && address <= last)
        && rule.protocol.is_none_or(|selected| {
            selected == protocol
                && rule
                    .ports
                    .is_none_or(|(first, last)| (first..=last).contains(&port))
        })
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

/// Returns the addresses of the IPv4 network of `guest_network`, a static guest address in
/// `ADDRESS/PREFIX` form.
pub(super) fn guest_subnet(guest_network: &str) -> Option<Interval> {
    let (address, prefix) = guest_network.split_once('/')?;
    let address: Ipv4Addr = address.parse().ok()?;
    let prefix = cidr::parse_prefix_length(prefix).filter(|prefix| (1..=32).contains(prefix))?;
    let mask = u32::MAX << (32 - u32::from(prefix));
    let network = u32::from(address) & mask;
    Some((u128::from(network), u128::from(network | !mask)))
}
