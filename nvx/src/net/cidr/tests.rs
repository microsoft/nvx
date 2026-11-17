use super::*;

fn cidr(value: &str) -> Cidr {
    Cidr::parse(value).unwrap()
}

fn from_ipv4_bits(bits: u32, prefix: u8) -> Cidr {
    Cidr {
        address: IpAddr::V4(Ipv4Addr::from(bits)),
        prefix,
    }
}

#[test]
fn parsing_requires_canonical_networks() {
    assert_eq!(cidr("192.0.2.1").to_string(), "192.0.2.1/32");
    assert_eq!(cidr("10.0.0.0/8").to_string(), "10.0.0.0/8");
    assert_eq!(cidr("0.0.0.0/0").to_string(), "0.0.0.0/0");
    assert_eq!(cidr("2001:db8::/32").to_string(), "2001:db8::/32");
    assert_eq!(cidr("2001:db8::1").to_string(), "2001:db8::1/128");
    assert_eq!(cidr("::/0").to_string(), "::/0");
    assert!(cidr("2001:db8::/32").is_ipv6() && !cidr("10.0.0.0/8").is_ipv6());
    assert_eq!(Cidr::everything(false), cidr("0.0.0.0/0"));
    assert_eq!(Cidr::everything(true), cidr("::/0"));
    for invalid in [
        "10.0.0.1/8",
        "10.0.0.0/33",
        "10.0.0.0/08",
        "0.0.0.0/00",
        "10.0.0.0/+8",
        "192.0.2.1/+32",
        "0.0.0.0/+0",
        "2001:db8::/+32",
        "2001:db8::/129",
        "example.com",
        "10.0.0.0/",
        "2001:db8::1/32",
        "fe80::1%eth0",
    ] {
        assert!(Cidr::parse(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn masked_parsing_clears_host_bits_as_openvmm_does() {
    let masked = |value: &str| Cidr::parse_masked(value).unwrap().to_string();
    assert_eq!(masked("10.0.0.1/8"), "10.0.0.0/8");
    assert_eq!(masked("192.0.2.7"), "192.0.2.7/32");
    assert_eq!(masked("2001:db8::1/32"), "2001:db8::/32");
    assert_eq!(masked("2001:db8::1"), "2001:db8::1/128");
    assert_eq!(masked("::ffff:192.0.2.1/96"), "::ffff:0.0.0.0/96");
    // OpenVMM reads the prefix length as `u8::from_str` does.
    assert_eq!(masked("10.0.0.0/08"), "10.0.0.0/8");
    assert_eq!(masked("10.0.0.0/+8"), "10.0.0.0/8");
    for invalid in [
        "10.0.0.0/33",
        "2001:db8::/129",
        "10.0.0.0/",
        "10.0.0.0/-1",
        "10.0.0.0/255.0.0.0",
        "10.0.0.0/8/8",
        "010.0.0.0/8",
        "example.com",
        "fe80::1%eth0",
    ] {
        assert!(Cidr::parse_masked(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn containment_is_family_aware() {
    let outer = cidr("10.0.0.0/8");
    assert!(outer.contains(cidr("10.1.0.0/16")));
    assert!(outer.contains(outer));
    assert!(!outer.contains(cidr("11.0.0.0/16")));
    assert!(!outer.contains(cidr("::/0")));
    let v6 = cidr("2001:db8::/32");
    assert!(v6.contains(cidr("2001:db8:1::/48")));
    assert!(!v6.contains(cidr("2001:db9::/48")));
    // ::/96 and 0.0.0.0/0 span the same integers but never contain each other.
    assert!(!cidr("::/96").contains(cidr("0.0.0.0/0")));
    assert!(!cidr("0.0.0.0/0").contains(cidr("::/96")));
}

#[test]
fn subtraction_covers_exactly_the_remainder() {
    let subtract =
        |network: &str, excluded: &[Cidr]| cidr(network).subtract(excluded, usize::MAX).unwrap();
    let rendered =
        |networks: Vec<Cidr>| -> Vec<String> { networks.iter().map(ToString::to_string).collect() };
    assert_eq!(
        rendered(subtract("10.0.0.0/8", &[cidr("10.0.0.0/9")])),
        ["10.128.0.0/9"]
    );
    assert_eq!(
        rendered(subtract("10.0.0.0/30", &[cidr("10.0.0.1")])),
        ["10.0.0.0/32", "10.0.0.2/31"]
    );
    assert!(subtract("10.1.0.0/16", &[cidr("10.0.0.0/8")]).is_empty());
    assert_eq!(
        subtract("10.0.0.0/8", &[cidr("11.0.0.0/8")]),
        [cidr("10.0.0.0/8")]
    );
    // An exclusion of the other family excludes nothing.
    assert_eq!(subtract("0.0.0.0/0", &[cidr("::/96")]), [cidr("0.0.0.0/0")]);
    assert_eq!(subtract("::/96", &[cidr("0.0.0.0/0")]), [cidr("::/96")]);
    // Every remaining address is outside the exclusions, and the sizes add up.
    let excluded = [cidr("10.0.0.0/24"), cidr("10.0.7.0/24")];
    let remainder = subtract("10.0.0.0/16", &excluded);
    let covered: u64 = remainder
        .iter()
        .map(|cidr| 1u64 << (32 - u32::from(cidr.prefix)))
        .sum();
    assert_eq!(covered, (1 << 16) - 2 * 256);
    assert!(
        remainder
            .iter()
            .all(|cidr| !excluded.iter().any(|exclusion| exclusion.overlaps(*cidr)))
    );
}

#[test]
fn ipv6_subtraction_covers_exactly_the_remainder() {
    let remainder = cidr("2001:db8::/32")
        .subtract(
            &[cidr("2001:db8:1::/48"), cidr("2001:db8:8000::/33")],
            usize::MAX,
        )
        .unwrap();
    let rendered: Vec<String> = remainder.iter().map(ToString::to_string).collect();
    assert_eq!(
        rendered,
        [
            "2001:db8::/48",
            "2001:db8:2::/47",
            "2001:db8:4::/46",
            "2001:db8:8::/45",
            "2001:db8:10::/44",
            "2001:db8:20::/43",
            "2001:db8:40::/42",
            "2001:db8:80::/41",
            "2001:db8:100::/40",
            "2001:db8:200::/39",
            "2001:db8:400::/38",
            "2001:db8:800::/37",
            "2001:db8:1000::/36",
            "2001:db8:2000::/35",
            "2001:db8:4000::/34",
        ]
    );
    let host = cidr("2001:db8::1:123");
    assert_eq!(
        cidr("2001:db8::1:122/127")
            .subtract(&[host], usize::MAX)
            .unwrap(),
        [cidr("2001:db8::1:122")]
    );
    assert_eq!(
        cidr("::/0").subtract(&[cidr("::/1")], usize::MAX).unwrap(),
        [cidr("8000::/1")]
    );
}

/// The direct recursion that rescans every exclusion at each split, as a reference.
fn reference_subtract(network: Cidr, excluded: &[Cidr]) -> Vec<Cidr> {
    if excluded.iter().any(|exclusion| exclusion.contains(network)) {
        return Vec::new();
    }
    if !excluded.iter().any(|exclusion| exclusion.overlaps(network)) {
        return vec![network];
    }
    network
        .halves()
        .into_iter()
        .flat_map(|half| reference_subtract(half, excluded))
        .collect()
}

#[test]
fn partitioned_subtraction_matches_the_direct_recursion() {
    // A linear congruential generator keeps the cases deterministic.
    let mut state = 0x2545_f491_u32;
    let mut next = move || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        state
    };
    for network in [cidr("10.0.0.0/16"), cidr("2001:db8::/112")] {
        let low_bits = network.bits() as u32;
        for _ in 0..500 {
            let count = next() % 32;
            let excluded: Vec<Cidr> = (0..count)
                .map(|_| {
                    let prefix = 12 + (next() % 21) as u8;
                    // Most exclusions fall inside the network; some cover it or lie
                    // elsewhere.
                    let bits = if next() % 4 == 0 {
                        next()
                    } else {
                        low_bits | (next() & 0xffff)
                    };
                    if network.is_ipv6() {
                        let prefix = prefix + 96;
                        let address = (network.bits() & !u128::from(u32::MAX)) | u128::from(bits);
                        network.with_bits(mask(address, prefix, 128), prefix)
                    } else {
                        from_ipv4_bits(mask(u128::from(bits), prefix, 32) as u32, prefix)
                    }
                })
                .collect();
            assert_eq!(
                network.subtract(&excluded, usize::MAX).unwrap(),
                reference_subtract(network, &excluded),
                "{excluded:?}"
            );
        }
    }
}

#[test]
fn subtraction_stops_once_the_remainder_exceeds_its_limit() {
    // 512 evenly spaced addresses split the address space into 512 * 23 networks.
    let excluded: Vec<Cidr> = (0..512u32)
        .map(|index| from_ipv4_bits(index << 23, 32))
        .collect();
    let everything = Cidr::everything(false);
    let size = |limit| {
        everything
            .subtract(&excluded, limit)
            .map(|remainder| remainder.len())
    };
    assert_eq!(size(usize::MAX), Some(512 * 23));
    assert_eq!(size(512 * 23), Some(512 * 23));
    assert_eq!(size(512 * 23 - 1), None);
    assert_eq!(size(256), None);
}
