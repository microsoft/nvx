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
