//! Canonical ordering, merging, truncation, and framing tests, and the conformance vectors.

use super::*;
use crate::guest::ingress::{FAMILY_IPV4, FAMILY_IPV6, PROTOCOL_TCP, PROTOCOL_UDP};
use crate::tests::{NVXL_V1, of_kind, vector};

/// Encodes the listeners of each conformance vector, added in reverse, to its bytes.
#[test]
fn announcements_match_their_vectors() {
    let names = [
        "NONE",
        "UDP",
        "DUAL_STACK",
        "TWO_LISTENERS",
        "OMITTED_LISTENERS",
    ];
    for vector in of_kind(NVXL_V1, "ANNOUNCEMENT", &names) {
        let name = vector.name;
        let mut values = vector.values();
        let omitted: u8 = values.number();
        let listeners = values.list(|values| {
            let protocol = match values.word() {
                "TCP" => PROTOCOL_TCP,
                "UDP" => PROTOCOL_UDP,
                protocol => panic!("{name} names the unknown protocol {protocol}"),
            };
            let families = match values.word() {
                "IPV4" => FAMILY_IPV4,
                "IPV6" => FAMILY_IPV6,
                "BOTH" => FAMILY_IPV4 | FAMILY_IPV6,
                families => panic!("{name} names the unknown families {families}"),
            };
            (protocol, families, values.number::<u16>())
        });
        drop(values);
        let mut table = ListenerTable::default();
        for &(protocol, families, port) in listeners.iter().rev() {
            table.add(protocol, port, families);
        }
        match omitted {
            0 => {}
            // The guest omits the listeners above the lowest that an announcement carries.
            1 => {
                assert_eq!(listeners.len(), MAX_ANNOUNCED, "{name}");
                let &(protocol, _, port) = listeners.last().unwrap();
                assert!((protocol, port) < (PROTOCOL_UDP, u16::MAX), "{name}");
                table.add(PROTOCOL_UDP, u16::MAX, FAMILY_IPV4);
            }
            _ => panic!("{name} has the unknown OMITTED value {omitted}"),
        }
        assert_eq!(encode(table), vector.bytes, "{name}");
    }
}

/// Exercises hundreds of deterministic input sets against every payload invariant.
#[test]
fn encoding_is_always_sorted_bounded_and_well_formed() {
    let mut random = 0x4e56_584c_u64;
    for case in 0..512 {
        let mut table = ListenerTable::default();
        let additions = case * 5 % 1_400;
        for _ in 0..additions {
            random = random
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let protocol = if random & 1 == 0 {
                PROTOCOL_TCP
            } else {
                PROTOCOL_UDP
            };
            let port = ((random >> 16) as u16).max(1);
            let families = if random & 2 == 0 {
                FAMILY_IPV4
            } else {
                FAMILY_IPV6
            };
            table.add(protocol, port, families);
        }
        let total = table.entries.len();
        let payload = encode(table);
        assert!(payload.len() <= 8 + MAX_ANNOUNCED * 4);
        assert_eq!(&payload[..4], b"NVXL");
        assert_eq!(payload[4], 1);
        assert_eq!(payload[5], u8::from(total > MAX_ANNOUNCED));
        let count = u16::from_be_bytes([payload[6], payload[7]]) as usize;
        assert_eq!(count, total.min(MAX_ANNOUNCED));
        assert_eq!(payload.len(), 8 + count * 4);
        let mut previous = None;
        for entry in payload[8..].chunks_exact(4) {
            let key = (entry[0], u16::from_be_bytes([entry[2], entry[3]]));
            assert!(matches!(entry[0], PROTOCOL_TCP | PROTOCOL_UDP));
            assert!([FAMILY_IPV4, FAMILY_IPV6, FAMILY_IPV4 | FAMILY_IPV6].contains(&entry[1]));
            assert_ne!(key.1, 0);
            assert!(previous.is_none_or(|previous| previous < key));
            previous = Some(key);
        }
    }
}

/// Verifies duplicate protocol/port entries merge their family masks.
#[test]
fn duplicate_protocol_and_port_merge_families() {
    let mut table = ListenerTable::default();
    table.add(PROTOCOL_TCP, 8080, FAMILY_IPV4);
    table.add(PROTOCOL_TCP, 8080, FAMILY_IPV6);
    let dual_stack = vector(NVXL_V1, "ANNOUNCEMENT", "DUAL_STACK");
    assert_eq!(encode(table), dual_stack.bytes);
}
