//! Translation of the contract's network posture into OpenVMM options.
//!
//! OpenVMM's portable profile enforces an egress default plus IPv4 allow and deny rules that
//! match a network and, optionally, one TCP or UDP port; deny rules take precedence. Contract
//! rules are expanded into that form exactly: exceptions are subtracted from their networks,
//! port ranges become one rule per port, and `any` protocol with a port becomes a TCP and a UDP
//! rule. Rules that cannot be expressed exactly are rejected.

use crate::cidr::{Cidr, Ipv4Cidr};
use crate::error::{Error, Result};
use crate::model::{Access, NetworkPolicy, NetworkRule, Protocol};

/// Largest number of allow rules, and of deny rules, that OpenVMM accepts.
const MAX_RULES: usize = 256;

/// Returns the OpenVMM options that enforce `policy`.
///
/// Without a network section, or when egress is denied without allow rules and ingress is
/// denied, no network device is attached. Otherwise the portable profile applies the egress
/// posture with ingress and host loopback denied; other postures are rejected before
/// provisioning by the backend's capabilities.
pub(crate) fn network_arguments(
    policy: Option<&NetworkPolicy>,
    guest_network: &str,
) -> Result<Vec<String>> {
    let Some(policy) = policy else {
        return Ok(Vec::new());
    };
    let egress = &policy.egress;
    if egress.default == Access::Deny
        && egress.allow.is_empty()
        && policy.ingress.default == Access::Deny
    {
        return Ok(Vec::new());
    }
    let mut arguments: Vec<String> = [
        "--net",
        guest_network,
        "--network-profile",
        "portable",
        "--network-egress",
        egress.default.as_str(),
        "--network-ingress",
        "deny",
        "--host-loopback",
        "deny",
    ]
    .map(str::to_owned)
    .to_vec();
    for (rules, option, field) in [
        (
            &egress.allow,
            "--network-egress-allow",
            "network.egress.allow",
        ),
        (&egress.deny, "--network-egress-deny", "network.egress.deny"),
    ] {
        let mut expanded = Vec::new();
        for (index, rule) in rules.iter().enumerate() {
            expand_rule(rule, &format!("{field}[{index}]"), &mut expanded)?;
        }
        expanded.sort_unstable();
        expanded.dedup();
        if expanded.len() > MAX_RULES {
            return Err(Error::policy_validation(format!(
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
    Ok(arguments)
}

/// Appends the OpenVMM rules equivalent to `rule`.
fn expand_rule(rule: &NetworkRule, field: &str, output: &mut Vec<String>) -> Result<()> {
    let unsupported = |message: String| Err(Error::policy_validation(format!("{field}{message}")));
    let mut networks = Vec::new();
    if rule.to.is_empty() {
        networks.push(ipv4(&Cidr::parse("0.0.0.0/0").expect("valid network")));
    }
    for (index, peer) in rule.to.iter().enumerate() {
        let Some(network) = Cidr::parse(&peer.cidr).ok().and_then(Cidr::ipv4) else {
            return unsupported(format!(
                ".to[{index}]: the openvmm backend supports only IPv4 destinations"
            ));
        };
        let mut excluded = Vec::new();
        for value in &peer.except {
            match Cidr::parse(value).ok().and_then(Cidr::ipv4) {
                Some(exclusion) => excluded.push(exclusion),
                None => {
                    return unsupported(format!(
                        ".to[{index}].except: the openvmm backend supports only IPv4 networks"
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

    let mut selectors: Vec<Option<(&str, u16)>> = Vec::new();
    if rule.ports.is_empty() {
        selectors.push(None);
    }
    for (index, port) in rule.ports.iter().enumerate() {
        let protocols: &[&str] = match port.protocol {
            Protocol::Tcp => &["tcp"],
            Protocol::Udp => &["udp"],
            Protocol::Any => &["tcp", "udp"],
            Protocol::Icmp => {
                return unsupported(format!(
                    ".ports[{index}]: the openvmm backend cannot select ICMP by itself"
                ));
            }
        };
        let Some(start) = port.port else {
            if port.protocol == Protocol::Any {
                selectors.push(None);
                continue;
            }
            return unsupported(format!(
                ".ports[{index}]: the openvmm backend cannot match every port of one protocol; \
                 name the ports"
            ));
        };
        let end = port.end_port.unwrap_or(start);
        if usize::from(end - start) >= MAX_RULES {
            return unsupported(format!(
                ".ports[{index}]: the openvmm backend accepts port ranges of at most {MAX_RULES} \
                 ports"
            ));
        }
        for protocol in protocols {
            for number in start..=end {
                selectors.push(Some((protocol, number)));
            }
        }
    }

    for network in &networks {
        for selector in &selectors {
            output.push(match selector {
                None => network.to_string(),
                Some((protocol, port)) => format!("{network}:{protocol}:{port}"),
            });
            if output.len() > MAX_RULES * 4 {
                return unsupported(format!(" expands to more than {MAX_RULES} OpenVMM rules"));
            }
        }
    }
    Ok(())
}

fn ipv4(cidr: &Cidr) -> Ipv4Cidr {
    cidr.ipv4().expect("an IPv4 network")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;
    use crate::model::{EgressPolicy, NetworkPeer, NetworkPort};

    fn policy(egress: EgressPolicy) -> NetworkPolicy {
        NetworkPolicy {
            egress,
            ..NetworkPolicy::deny_all()
        }
    }

    fn rules(arguments: &[String], option: &str) -> Vec<String> {
        arguments
            .windows(2)
            .filter(|pair| pair[0] == option)
            .map(|pair| pair[1].clone())
            .collect()
    }

    #[test]
    fn default_postures_map_to_portable_profile_options() {
        assert!(network_arguments(None, "10.0.0.2/24").unwrap().is_empty());
        assert!(
            network_arguments(Some(&NetworkPolicy::deny_all()), "10.0.0.2/24")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            network_arguments(Some(&NetworkPolicy::egress(Access::Allow)), "10.0.0.2/24").unwrap(),
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
        let denied = policy(EgressPolicy::new(Access::Deny).with_deny(NetworkRule::to("1.2.3.4")));
        assert!(
            network_arguments(Some(&denied), "10.0.0.2/24")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rules_expand_exactly() {
        let egress = EgressPolicy::new(Access::Deny)
            .with_allow(NetworkRule {
                to: vec![NetworkPeer {
                    cidr: "10.0.0.0/30".to_owned(),
                    except: vec!["10.0.0.1".to_owned()],
                }],
                ports: vec![NetworkPort {
                    protocol: Protocol::Tcp,
                    port: Some(80),
                    end_port: Some(81),
                }],
            })
            .with_allow(NetworkRule::to("192.0.2.7").on_port(Protocol::Any, 53))
            .with_allow(NetworkRule::to("198.51.100.0/24"));
        let arguments = network_arguments(Some(&policy(egress)), "10.0.0.2/24").unwrap();
        assert_eq!(
            rules(&arguments, "--network-egress-allow"),
            [
                "10.0.0.0/32:tcp:80",
                "10.0.0.0/32:tcp:81",
                "10.0.0.2/31:tcp:80",
                "10.0.0.2/31:tcp:81",
                "192.0.2.7/32:tcp:53",
                "192.0.2.7/32:udp:53",
                "198.51.100.0/24",
            ]
        );
        assert!(has_pair(&arguments, "--network-egress", "deny"));

        let egress = EgressPolicy::new(Access::Allow).with_deny(NetworkRule {
            to: Vec::new(),
            ports: vec![NetworkPort {
                protocol: Protocol::Udp,
                port: Some(53),
                end_port: None,
            }],
        });
        let arguments = network_arguments(Some(&policy(egress)), "10.0.0.2/24").unwrap();
        assert_eq!(
            rules(&arguments, "--network-egress-deny"),
            ["0.0.0.0/0:udp:53"]
        );
    }

    fn has_pair(arguments: &[String], name: &str, value: &str) -> bool {
        arguments
            .windows(2)
            .any(|pair| pair[0] == name && pair[1] == value)
    }

    #[test]
    fn inexpressible_rules_are_rejected() {
        let port = |protocol, port, end_port| NetworkRule {
            to: vec![NetworkPeer::new("10.0.0.0/8")],
            ports: vec![NetworkPort {
                protocol,
                port,
                end_port,
            }],
        };
        for rule in [
            NetworkRule::to("2001:db8::/32"),
            port(Protocol::Tcp, None, None),
            port(Protocol::Icmp, None, None),
            port(Protocol::Tcp, Some(1), Some(1024)),
            NetworkRule {
                to: (0..300)
                    .map(|index| NetworkPeer::new(format!("10.0.{}.0/24", index % 256)))
                    .collect(),
                ports: vec![NetworkPort {
                    protocol: Protocol::Tcp,
                    port: Some(1),
                    end_port: Some(2),
                }],
            },
        ] {
            let egress = EgressPolicy::new(Access::Deny).with_allow(rule.clone());
            assert_eq!(
                network_arguments(Some(&policy(egress)), "10.0.0.2/24")
                    .unwrap_err()
                    .code(),
                ErrorCode::PolicyValidation,
                "{rule:?}"
            );
        }
    }

    #[test]
    fn exceptions_that_split_a_network_too_finely_are_rejected() {
        // Without a limit, these exceptions would split the address space into 512 * 23 networks.
        let peer = NetworkPeer {
            cidr: "0.0.0.0/0".to_owned(),
            except: (0..512u32)
                .map(|index| std::net::Ipv4Addr::from(index << 23).to_string())
                .collect(),
        };
        let rule = NetworkRule {
            to: vec![peer],
            ports: Vec::new(),
        };
        let egress = EgressPolicy::new(Access::Deny).with_allow(rule);
        let error = network_arguments(Some(&policy(egress)), "10.0.0.2/24").unwrap_err();
        assert_eq!(error.code(), ErrorCode::PolicyValidation);
        assert!(
            error.message().contains("covers more than 256 networks"),
            "{error}"
        );
    }
}
