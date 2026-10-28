//! Tests of the OpenVMM options of network postures, and of the parsers of their rules,
//! forwards, and proxies.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

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
    let https = NetworkPolicy::proxy_only(NetworkProxy::new(3128).with_scheme(ProxyScheme::Https));
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
        NetworkProxy::parse_endpoint("192.168.7.5:8080", Ipv4Addr::new(192, 168, 7, 5)).unwrap(),
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

#[test]
fn templates_attach_their_nic_under_the_complete_deny_all_policy() {
    assert_eq!(
        template_arguments("10.0.0.2/24"),
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
            "deny",
        ]
    );
}

#[test]
fn the_dns_server_is_the_first_gateway_whose_dns_service_the_policy_reaches() {
    let gateway = Ipv4Addr::new(10, 0, 0, 1);
    let ipv6_gateway: Ipv6Addr = "fd00::a00:1".parse().unwrap();
    let v4 = Some(IpAddr::V4(gateway));
    let v6 = Some(IpAddr::V6(ipv6_gateway));
    let allow = || NetworkPolicy::new(Access::Allow);
    let deny = || NetworkPolicy::new(Access::Deny);
    let rule = |cidr: &str| EgressRule::to(cidr);
    let cases = [
        ("egress allowed", allow(), v4, v4),
        ("egress denied", deny(), None, None),
        ("a proxy-only policy", proxy_only(), None, None),
        (
            "an allow rule of TCP port 53 at the gateway",
            deny().with_allow(rule("10.0.0.1/32").on_port(Protocol::Tcp, 53)),
            v4,
            v4,
        ),
        (
            "an allow rule of UDP to every port",
            deny().with_allow(rule("0.0.0.0/0").on_protocol(Protocol::Udp)),
            v4,
            v4,
        ),
        (
            "an allow rule of a range of ports that holds 53",
            deny().with_allow(rule("10.0.0.0/24").on_port_range(Protocol::Any, 50, 60)),
            v4,
            v4,
        ),
        (
            "an allow rule of every protocol at the gateway",
            deny().with_allow(rule("10.0.0.1")),
            v4,
            v4,
        ),
        (
            "an allow rule of another port at the gateway",
            deny().with_allow(rule("10.0.0.1/32").on_port(Protocol::Tcp, 8080)),
            None,
            None,
        ),
        (
            "an allow rule of ICMP at the gateway",
            deny().with_allow(rule("10.0.0.1/32").on_protocol(Protocol::Icmp)),
            None,
            None,
        ),
        (
            "an allow rule of another destination",
            deny().with_allow(rule("10.0.0.2/32").on_port(Protocol::Udp, 53)),
            None,
            None,
        ),
        (
            "a deny rule of UDP alone, which leaves TCP",
            allow().with_deny(rule("10.0.0.1/32").on_port(Protocol::Udp, 53)),
            v4,
            v4,
        ),
        (
            "a deny rule of the IPv4 gateway that an allow rule overlaps",
            deny()
                .with_allow(rule("0.0.0.0/0").on_port(Protocol::Any, 53))
                .with_allow(rule("::/0").on_port(Protocol::Any, 53))
                .with_deny(rule("10.0.0.0/8")),
            v6,
            None,
        ),
        (
            "an allow rule of the IPv6 gateway alone",
            deny().with_allow(rule("fd00::a00:1/128").on_port(Protocol::Udp, 53)),
            v6,
            None,
        ),
        (
            "an exception that removes the IPv4 gateway",
            deny().with_allow(EgressRule {
                to: vec![Destination {
                    cidr: "10.0.0.0/24".to_owned(),
                    except: vec!["10.0.0.0/30".to_owned()],
                }],
                ports: Vec::new(),
            }),
            None,
            None,
        ),
    ];
    for (name, policy, dual_stack, ipv4_only) in cases {
        assert_eq!(
            dns_server(&policy, gateway, Some(ipv6_gateway)).unwrap(),
            dual_stack,
            "{name}"
        );
        assert_eq!(
            dns_server(&policy, gateway, None).unwrap(),
            ipv4_only,
            "{name}"
        );
    }
    // Host-loopback access, ingress, and forwards leave the egress policy's choice.
    let mut open = deny();
    open.ingress = Access::Allow;
    open.host_loopback = HostLoopback::Allow;
    open.host_loopback_forwards = vec![LoopbackForward {
        protocol: ForwardProtocol::Tcp,
        host_port: 18080,
        guest_port: 8080,
    }];
    assert_eq!(
        dns_server(&open, gateway, Some(ipv6_gateway)).unwrap(),
        None
    );
    // A rule that does not parse fails as the lowering fails.
    let invalid = deny().with_allow(rule("10.0.0.0/33"));
    let error = dns_server(&invalid, gateway, None).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
}
