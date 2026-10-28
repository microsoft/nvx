//! Access between the guest and host loopback, the host loopback ports that OpenVMM forwards to
//! guest ports, and the checks of those forwards.

use std::collections::HashSet;

use super::address::Family;
use super::arguments::guest_subnet;
use super::lowering::Rule;
#[cfg(feature = "cli")]
use super::rule::parse_port;
use super::{Access, MAX_LOOPBACK_FORWARDS, NetworkPolicy};
use crate::error::{Error, Result};

/// Access between the guest and host loopback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostLoopback {
    /// Neither direction: the guest reaches no host loopback service other than a
    /// [`NetworkProxy`], and the host reaches none of the guest's listeners.
    ///
    /// [`NetworkProxy`]: crate::host::network::NetworkProxy
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

/// Checks the host-loopback forwards of a policy whose rules lower, and whose host-loopback
/// access is allowed when it has forwards, against the rules of
/// [`NetworkPolicy::host_loopback_forwards`]. `deny` holds the parsed deny rules.
pub(super) fn check_loopback_forwards(
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
