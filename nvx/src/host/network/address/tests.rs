use super::*;

/// Counters that tests read to check how much work the compiler does, as the Python tests check
/// by mocking `ipaddress.summarize_address_range` and `_merge_intervals`.
pub(crate) mod probe;

fn network(text: &str) -> Result<String, InterfaceError> {
    parse_interface(text).map(|network| network.to_string())
}

#[test]
fn interfaces_parse_as_python_does() {
    for (text, expected) in [
        ("10.0.0.5/24", "10.0.0.0/24"),
        ("10.0.0.0/255.0.0.0", "10.0.0.0/8"),
        ("10.0.0.0/0.255.255.255", "10.0.0.0/8"),
        ("10.0.0.0/255.255.255.255", "10.0.0.0/32"),
        ("10.0.0.0/0.0.0.0", "0.0.0.0/0"),
        ("10.0.0.0/08", "10.0.0.0/8"),
        ("10.0.0.0/0008", "10.0.0.0/8"),
        ("192.0.2.7", "192.0.2.7/32"),
        ("::/0", "::/0"),
        ("::/000128", "::/128"),
        ("::1.2.3.4", "::102:304/128"),
        ("1:2:3:4:5:6:7:8", "1:2:3:4:5:6:7:8/128"),
        ("1:2:3:4:5:6:7::", "1:2:3:4:5:6:7:0/128"),
        ("::2:3:4:5:6:7:8", "0:2:3:4:5:6:7:8/128"),
        ("1:2:3:4:5:6:1.2.3.4", "1:2:3:4:5:6:102:304/128"),
        ("1:2:3:4:5::1.2.3.4", "1:2:3:4:5:0:102:304/128"),
        ("0000::", "::/128"),
        ("::FFFF:1.2.3.4", "::ffff:1.2.3.4/128"),
        ("::ffff:c000:201/120", "::ffff:192.0.2.0/120"),
        ("::c000:200/120", "::c000:200/120"),
        ("::1.2.3.4/96", "::/96"),
        ("2001:DB8::/32", "2001:db8::/32"),
        ("::ffff:0:0/95", "::fffe:0:0/95"),
        ("1:0:0:2:0:0:0:3", "1:0:0:2::3/128"),
        ("0:0:1::", "0:0:1::/128"),
        ("::1:2:3:4:5:6:7", "0:1:2:3:4:5:6:7/128"),
        ("fe80::1/64", "fe80::/64"),
    ] {
        assert_eq!(network(text).as_deref(), Ok(expected), "{text}");
    }
    let long_prefix = format!("10.0.0.0/{}8", "0".repeat(4299));
    assert_eq!(network(&long_prefix).as_deref(), Ok("10.0.0.0/8"));
}

#[test]
fn invalid_and_scoped_interfaces_are_refused() {
    for text in [
        "10.0.0.0/+8",
        "10.0.0.0/-8",
        " 10.0.0.0/8",
        "10.0.0.0/8 ",
        "10.0.0.0/",
        "10.0.0.0//8",
        "10.0.0.0/8/8",
        "010.0.0.0/8",
        "1.2.3",
        "1.2.3.4.5",
        "256.0.0.0",
        "1.2.3.4/33",
        "1.2.3.4/255.0.255.0",
        "1.2.3.4/255.255.255.000",
        "::/129",
        "fe80::1%/64",
        "fe80::1%a%b",
        "fe80::1%%",
        "1::2::3",
        ":1::2",
        "1::2:",
        "1:2:3:4:5:6:7:8:9",
        "1:2:3:4:5:6:7:1.2.3.4",
        "1:2:3:4:5:6::1.2.3.4",
        "00000::",
        "::ffff:1.2.3.04",
        "::1.2.3",
        "1.2.3.4/255.255.255.0x",
        "\u{FF11}.2.3.4",
        "::/12\u{FF18}",
        "0:0:0:0:0:0:0:0:0",
        ":::",
        "1:::2",
        "",
        "/8",
        "1.2.3.4/0x8",
        "1.2.3.4/8.0",
        "::/0x8",
        "1.2.3.4/\u{0668}",
        "fe80::1%eth0/129",
        "::1:2:3:4:5:6:7:8",
        "1:2:3:4:5:6:7:8:",
        ":1:2:3:4:5:6:7",
    ] {
        assert_eq!(network(text), Err(InterfaceError::Invalid), "{text:?}");
    }
    let long_prefix = format!("10.0.0.0/{}8", "0".repeat(4300));
    assert_eq!(network(&long_prefix), Err(InterfaceError::Invalid));
    for text in [
        "fe80::1%eth0/64",
        "fe80::%eth0/64",
        "fe80::1%1",
        "::ffff:192.0.2.1%eth0/120",
        "fe80::1%\u{FF11}",
        "1:2:3:4:5:6:7:8%x/128",
    ] {
        assert_eq!(network(text), Err(InterfaceError::Scoped), "{text:?}");
    }
}

#[test]
fn ipv6_addresses_print_as_python_does() {
    for (address, expected) in [
        (0u128, "::/128"),
        (1, "::1/128"),
        (0xFFFF_0000_0000, "::ffff:0.0.0.0/128"),
        (0xFFFF_C000_0201, "::ffff:192.0.2.1/128"),
        (0xFFFE_C000_0201, "::fffe:c000:201/128"),
        (0x2001_0db8 << 96, "2001:db8::/128"),
        ((0x2001_0db8 << 96) | 1, "2001:db8::1/128"),
        // The longest run of zero groups becomes `::`, the first of two equally long runs,
        // and a single zero group stays.
        ((1 << 112) | (1 << 64) | 1, "1:0:0:1::1/128"),
        ((1 << 112) | (1 << 64) | (1 << 16) | 1, "1::1:0:0:1:1/128"),
        (
            (1 << 112) | (2 << 96) | (3 << 80) | (4 << 64) | (5 << 48) | (6 << 32) | (7 << 16),
            "1:2:3:4:5:6:7:0/128",
        ),
        (u128::MAX, "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/128"),
    ] {
        let network = Network::new(Family::V6, address, 128);
        assert_eq!(network.to_string(), expected, "{address:#x}");
    }
}

#[test]
fn summaries_match_python() {
    let summary = |family, first, last| -> Vec<String> {
        summarize(family, (first, last))
            .map(|network| network.to_string())
            .collect()
    };
    let ipv4 = |text: &str| u128::from(u32::from(text.parse::<Ipv4Addr>().unwrap()));
    assert_eq!(
        summary(Family::V4, ipv4("192.0.2.0"), ipv4("192.0.2.130")),
        ["192.0.2.0/25", "192.0.2.128/31", "192.0.2.130/32"]
    );
    assert_eq!(summary(Family::V4, 0, u128::from(u32::MAX)), ["0.0.0.0/0"]);
    assert_eq!(
        summary(Family::V4, 1, u128::from(u32::MAX)).len(),
        32,
        "every power of two below 2^32"
    );
    assert_eq!(summary(Family::V6, 0, u128::MAX), ["::/0"]);
    assert_eq!(
        summary(Family::V6, u128::MAX, u128::MAX),
        ["ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/128"]
    );
    assert_eq!(summary(Family::V6, 1, u128::MAX).len(), 128);
    assert_eq!(summary(Family::V6, 1 << 127, u128::MAX), ["8000::/1"]);
}

#[test]
fn intervals_merge_and_subtract_at_the_family_bounds() {
    assert_eq!(
        merge([(5, 9), (0, 2), (3, 4), (20, 30), (25, 26)]),
        [(0, 9), (20, 30)]
    );
    assert_eq!(merge([(0, u128::MAX), (7, 9)]), [(0, u128::MAX)]);
    assert_eq!(merge([(10, u128::MAX), (0, 9)]), [(0, u128::MAX)]);
    assert!(merge([]).is_empty());
    assert_eq!(
        subtract(&[(0, 100)], &[(10, 20), (30, 40)]),
        [(0, 9), (21, 29), (41, 100)]
    );
    assert_eq!(subtract(&[(0, u128::MAX)], &[(1, u128::MAX)]), [(0, 0)]);
    assert!(subtract(&[(5, u128::MAX)], &[(0, u128::MAX)]).is_empty());
    assert_eq!(
        subtract(&[(0, 9), (20, 29)], &[(5, 24)]),
        [(0, 4), (25, 29)]
    );
    assert_eq!(subtract(&[(0, 9)], &[]), [(0, 9)]);
}

#[test]
fn containment_is_family_aware() {
    let parse = |text: &str| parse_interface(text).unwrap();
    assert!(parse("10.0.0.0/8").contains(parse("10.1.0.0/16")));
    assert!(!parse("10.0.0.0/8").contains(parse("11.0.0.0/16")));
    assert!(!parse("::/96").contains(parse("0.0.0.0/0")));
    assert!(!parse("0.0.0.0/0").contains(parse("::/96")));
    assert_eq!(parse("10.0.0.0/8").interval(), (0x0A00_0000, 0x0AFF_FFFF));
}
