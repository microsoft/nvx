use super::*;

#[test]
fn the_rules_of_one_rule_share_its_addresses() {
    let ipv4: Vec<Interval> = vec![(1, 1), (3, 3)];
    let ipv6: Vec<Interval> = vec![(5, 6)];
    let selectors: Vec<Selector> = vec![
        (Some(NativeProtocol::Tcp), Some((80, 80))),
        (Some(NativeProtocol::Udp), Some((53, 60))),
        (None, None),
    ];
    let rules = combine(
        Some(vec![(Family::V4, ipv4.clone()), (Family::V6, ipv6.clone())]),
        Some(selectors.clone()),
    );
    assert_eq!(rules.len(), 6);
    for (family_rules, (family, addresses)) in rules
        .chunks(selectors.len())
        .zip([(Family::V4, &ipv4), (Family::V6, &ipv6)])
    {
        for (rule, &(protocol, ports)) in family_rules.iter().zip(&selectors) {
            assert_eq!(rule.family, family);
            assert_eq!(*rule.addresses, **addresses);
            assert_eq!((rule.protocol, rule.ports), (protocol, ports));
            assert!(Rc::ptr_eq(&rule.addresses, &family_rules[0].addresses));
        }
        assert_eq!(
            Rc::strong_count(&family_rules[0].addresses),
            selectors.len()
        );
    }

    let rules = combine(None, Some(selectors.clone()));
    assert_eq!(rules.len(), 6);
    for (family_rules, family) in rules.chunks(selectors.len()).zip(Family::ALL) {
        for rule in family_rules {
            assert_eq!(rule.family, family);
            assert_eq!(*rule.addresses, [family.everything()]);
            assert!(Rc::ptr_eq(&rule.addresses, &family_rules[0].addresses));
        }
    }

    let rules = combine(Some(vec![(Family::V6, ipv6.clone())]), None);
    assert_eq!(
        rules,
        [Rule {
            family: Family::V6,
            addresses: Rc::from(ipv6),
            protocol: None,
            ports: None,
        }]
    );
}

#[test]
fn address_sets_compare_by_value() {
    let addresses: Rc<[Interval]> = Rc::from([(1, 2), (4, 4)]);
    let copy: Rc<[Interval]> = Rc::from(&*addresses);
    let longer: Rc<[Interval]> = Rc::from([(1, 2), (4, 4), (6, 6)]);
    let later: Rc<[Interval]> = Rc::from([(1, 3)]);
    let empty: Rc<[Interval]> = Rc::from([]);
    assert_eq!(AddressSet(&addresses), AddressSet(&addresses));
    assert_eq!(AddressSet(&addresses), AddressSet(&copy));
    assert!(AddressSet(&empty) < AddressSet(&addresses));
    assert!(AddressSet(&addresses) < AddressSet(&longer));
    assert!(AddressSet(&longer) < AddressSet(&later));
    let sets: BTreeSet<AddressSet<'_>> = [&later, &addresses, &copy, &empty, &longer]
        .into_iter()
        .map(|addresses| AddressSet(addresses))
        .collect();
    let sorted: Vec<&[Interval]> = sets.iter().map(|set| set.0).collect();
    assert_eq!(sorted, [&*empty, &*addresses, &*longer, &*later]);
}

#[test]
fn shared_addresses_are_merged_once() {
    let addresses: Vec<Interval> = (0..64).map(|index| (2 * index, 2 * index)).collect();
    // The union of the 64 rules for every protocol reads their addresses once; for ICMP, so
    // do the cover of its networks and the addresses that it covers. Python merges the
    // addresses of each rule into the union: 64 times as many.
    for (protocol, merged) in [(None, 64), (Some(NativeProtocol::Icmp), 3 * 64)] {
        let rules = combine(
            Some(vec![(Family::V4, addresses.clone())]),
            Some(vec![(protocol, None); 64]),
        );
        assert_eq!(rules.len(), 64);
        let (lowered, _, merged_intervals) =
            address::tests::probe::count(|| lower(&rules, "allow"));
        assert_eq!(lowered.unwrap().len(), 64, "{protocol:?}");
        assert_eq!(merged_intervals, merged, "{protocol:?}");
    }
}
