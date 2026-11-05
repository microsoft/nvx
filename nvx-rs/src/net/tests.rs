use super::*;

#[test]
fn guest_networks_follow_openvmm_rules() {
    for valid in ["10.0.0.2/24", "192.168.7.10/30", "172.16.0.200/16"] {
        assert!(is_valid_guest_network(valid), "{valid}");
    }
    for invalid in [
        "10.0.0.0/24",
        "10.0.0.255/24",
        "10.0.0.1/24",
        "10.0.0.5/30",
        "10.0.0.2",
        "10.0.0.2/31",
        "10.0.0.2/024",
        "fd00::2/64",
    ] {
        assert!(!is_valid_guest_network(invalid), "{invalid}");
        assert_eq!(guest_gateway(invalid), None, "{invalid}");
    }
}

#[test]
fn the_gateway_is_the_first_address_of_the_guest_network() {
    for (network, gateway) in [
        ("10.0.0.2/24", [10, 0, 0, 1]),
        ("192.168.7.10/30", [192, 168, 7, 9]),
        ("172.16.0.200/16", [172, 16, 0, 1]),
        ("10.255.255.254/8", [10, 0, 0, 1]),
    ] {
        assert_eq!(
            guest_gateway(network),
            Some(std::net::Ipv4Addr::from(gateway)),
            "{network}"
        );
    }
}

#[test]
fn hostnames_are_lowercase_labels() {
    assert!(is_valid_hostname("nvx-sandbox"));
    for invalid in ["", "-a", "a-", "A", "a.b", &"a".repeat(64)] {
        assert!(!is_valid_hostname(invalid), "{invalid:?}");
    }
}

#[test]
fn guest_addresses_keep_their_address_and_prefix_through_every_form() {
    for (text, address, prefix_len) in [
        ("10.0.0.2/24", [10, 0, 0, 2], 24),
        ("192.168.7.10/30", [192, 168, 7, 10], 30),
        ("172.16.0.200/16", [172, 16, 0, 200], 16),
        ("10.255.255.254/8", [10, 255, 255, 254], 8),
        ("128.0.0.2/1", [128, 0, 0, 2], 1),
    ] {
        let address = std::net::Ipv4Addr::from(address);
        let parsed = Ipv4Cidr::parse(text).unwrap();
        assert_eq!(parsed.address(), address, "{text}");
        assert_eq!(parsed.prefix_len(), prefix_len, "{text}");
        assert_eq!(
            Ipv4Cidr::new(address, prefix_len).unwrap(),
            parsed,
            "{text}"
        );
        assert_eq!(parsed.to_string(), text);
        #[cfg(feature = "serde")]
        {
            let json = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, format!("\"{text}\""));
            assert_eq!(serde_json::from_str::<Ipv4Cidr>(&json).unwrap(), parsed);
        }
    }
}

#[test]
fn guest_addresses_refuse_what_openvmm_refuses() {
    for text in [
        "10.0.0.2",
        "10.0.0.2/",
        "10.0.0.2/x",
        "10.0.0.2/+24",
        "10.0.0.2/024",
        "10.0.0.2/0",
        "10.0.0.2/31",
        "10.0.0.2/32",
        "/24",
        "fd00::2/64",
        "10.0.0.0/24",
        "10.0.0.255/24",
        "10.0.0.1/24",
        "10.0.0.5/30",
    ] {
        let error = Ipv4Cidr::parse(text).unwrap_err();
        assert_eq!(
            error,
            format!(
                "guest network {text:?} must be an IPv4 address with a /1 to /30 prefix that is \
                 not its network's network, broadcast, or gateway (first) address"
            )
        );
        #[cfg(feature = "serde")]
        {
            let error = serde_json::from_str::<Ipv4Cidr>(&format!("\"{text}\"")).unwrap_err();
            assert!(
                error.to_string().contains("must be an IPv4 address"),
                "{error}"
            );
        }
    }
    for (address, prefix_len) in [
        ([10, 0, 0, 2], 0),
        ([10, 0, 0, 2], 31),
        ([10, 0, 0, 2], 32),
        ([10, 0, 0, 0], 24),
        ([10, 0, 0, 255], 24),
        ([10, 0, 0, 1], 24),
    ] {
        let error = Ipv4Cidr::new(std::net::Ipv4Addr::from(address), prefix_len).unwrap_err();
        let [a, b, c, d] = address;
        assert!(
            error.contains(&format!("\"{a}.{b}.{c}.{d}/{prefix_len}\"")),
            "{error}"
        );
    }
}
