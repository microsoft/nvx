//! A `proptest` generator of egress policies, ported from `Corpus`, the generator with which the
//! retired `scripts/nvx_rs_fixtures.py` (in `36c5bf7af^`) drew the seed-359 policies of the
//! retired egress policy fixture, with its probabilities.
//!
//! It favors what makes lowering hard: networks that overlap and merge, in dense IPv4 and IPv6
//! neighborhoods, beside networks of any size anywhere; exceptions within their networks; and
//! ports at the boundaries of the port range and of common services, in ranges that end near
//! them. Rules take the MXC and the flat forms, and networks every spelling that the format reads:
//! with host bits, IPv4 netmasks and hostmasks, prefix lengths with leading zeros, and IPv6
//! addresses exploded, in uppercase, or with an embedded IPv4 address. A few exceptions are not
//! CIDRs or lie outside their networks, and a few ICMP selectors have ports, which the format
//! refuses.
//!
//! [`policy`] draws policy values, [`crowded_policy`] policies with an action that needs about as
//! many native rules as OpenVMM accepts, [`mutated`] policies with one part broken, and
//! [`policy_file`] the bytes of policy files, formatted as `json.dumps` formats them and
//! sometimes broken, as `Corpus.cases` wrote them.

use std::fmt::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr};

use proptest::prelude::*;
use serde_json::{Map, Value, json};

use super::model::{Family, Net};

/// The first address of the dense IPv4 neighborhood, `10.0.0.0`.
const IPV4_NEIGHBORHOOD: u128 = 10 << 24;
/// The first address of the dense IPv6 neighborhood, `2001:db8::`.
const IPV6_NEIGHBORHOOD: u128 = 0x2001_0db8 << 96;
/// The number of addresses of each dense neighborhood.
const NEIGHBORHOOD: u128 = 1024;

/// A network, as `Corpus.network` draws it: 45 times in a hundred a network of `/22` to `/32` in
/// the first 1024 addresses of `10.0.0.0/8`, 30 times one of `/118` to `/128` in those of
/// `2001:db8::/32`, so that networks overlap and merge, and otherwise an IPv4 or IPv6 network
/// anywhere, three times in ten an IPv4-mapped one for IPv6.
pub(super) fn network() -> impl Strategy<Value = Net> {
    prop_oneof![
        45 => (0..NEIGHBORHOOD, 22u8..=32).prop_map(|(offset, prefix)| {
            Net::new(Family::V4, IPV4_NEIGHBORHOOD + offset, prefix)
        }),
        30 => (0..NEIGHBORHOOD, 118u8..=128).prop_map(|(offset, prefix)| {
            Net::new(Family::V6, IPV6_NEIGHBORHOOD + offset, prefix)
        }),
        13 => (
            any::<u32>(),
            prop::sample::select(&[0u8, 1, 2, 4, 8, 12, 16, 24, 31, 32][..]),
        )
            .prop_map(|(address, prefix)| Net::new(Family::V4, address.into(), prefix)),
        12 => (
            any::<u128>(),
            prop::sample::select(&[0u8, 1, 3, 32, 64, 96, 104, 120, 127, 128][..]),
            prop::bool::weighted(0.3),
        )
            .prop_map(|(address, prefix, mapped)| {
                let address = if mapped {
                    (0xffff << 32) | (address & 0xffff_ffff)
                } else {
                    address
                };
                Net::new(Family::V6, address, prefix)
            }),
    ]
}

/// How [`spell`] writes the prefix of an IPv4 network.
#[derive(Debug, Clone, Copy)]
enum Ipv4Prefix {
    /// The prefix length.
    Length,
    /// The netmask, such as `255.255.0.0`.
    Netmask,
    /// The hostmask, such as `0.0.255.255`.
    Hostmask,
}

/// How [`spell`] writes an IPv6 address.
#[derive(Debug, Clone, Copy)]
enum Ipv6Form {
    /// As `str(IPv6Address)` writes it: compressed, in lowercase.
    Compressed,
    /// Every group with four digits.
    Exploded,
    /// Compressed, in uppercase.
    Uppercase,
    /// Exploded, with its last 32 bits as an embedded IPv4 address.
    EmbeddedIpv4,
}

/// The ways of writing `network`, as `Corpus.spell` draws them: three times in ten with host
/// bits, one time in ten with leading zeros in the prefix length, for IPv4 one time in ten with a
/// netmask or, failing that, one time in twenty with a hostmask, and for IPv6 with an address
/// exploded, in uppercase, or with an embedded IPv4 address; a network of one address is written
/// as its address half of the time.
pub(super) fn spelling(network: Net) -> impl Strategy<Value = String> {
    // A network of one address has no host bits to set.
    (
        prop::option::weighted(0.3, any::<u128>()),
        prop::option::weighted(0.1, 1usize..=3),
        prop_oneof![
            855 => Just(Ipv4Prefix::Length),
            100 => Just(Ipv4Prefix::Netmask),
            45 => Just(Ipv4Prefix::Hostmask),
        ],
        prop_oneof![
            70 => Just(Ipv6Form::Compressed),
            15 => Just(Ipv6Form::Exploded),
            10 => Just(Ipv6Form::Uppercase),
            5 => Just(Ipv6Form::EmbeddedIpv4),
        ],
        any::<bool>(),
    )
        .prop_map(move |(host, zeros, ipv4_prefix, ipv6_form, bare)| {
            spell(network, host, zeros, ipv4_prefix, ipv6_form, bare)
        })
}

/// Writes `network`, with the host bits of `host`, if any, `zeros` leading zeros in its prefix
/// length, and its prefix or address written as `ipv4_prefix` or `ipv6_form` say, and as its
/// address alone if it holds one address and `bare` is set.
fn spell(
    network: Net,
    host: Option<u128>,
    zeros: Option<usize>,
    ipv4_prefix: Ipv4Prefix,
    ipv6_form: Ipv6Form,
    bare: bool,
) -> String {
    let address =
        network.first() | host.map_or(0, |host| host & (network.last() - network.first()));
    let mut prefix = format!("{}{}", "0".repeat(zeros.unwrap_or(0)), network.prefix);
    let text = match network.family {
        Family::V4 => {
            let netmask = u32::MAX
                .checked_shl(32 - u32::from(network.prefix))
                .unwrap_or(0);
            match ipv4_prefix {
                Ipv4Prefix::Length => {}
                Ipv4Prefix::Netmask => prefix = Ipv4Addr::from(netmask).to_string(),
                Ipv4Prefix::Hostmask => prefix = Ipv4Addr::from(!netmask).to_string(),
            }
            Ipv4Addr::from(address as u32).to_string()
        }
        Family::V6 => {
            let ipv6 = Ipv6Addr::from(address);
            let exploded = || {
                ipv6.segments()
                    .map(|group| format!("{group:04x}"))
                    .join(":")
            };
            match ipv6_form {
                Ipv6Form::Compressed => ipv6.to_string(),
                Ipv6Form::Exploded => exploded(),
                Ipv6Form::Uppercase => ipv6.to_string().to_uppercase(),
                Ipv6Form::EmbeddedIpv4 => {
                    format!("{}{}", &exploded()[..30], Ipv4Addr::from(address as u32))
                }
            }
        }
    };
    if bare && network.prefix == network.family.bits() {
        text
    } else {
        format!("{text}/{prefix}")
    }
}

/// An exception of `parent`, as `Corpus.exception` draws it: one time in a hundred a string that
/// is not a CIDR or an IPv6 network with a scope, one time in a hundred any network, which rarely
/// lies within `parent`, and otherwise a network within `parent`, up to eight bits longer.
fn exception(parent: Net) -> impl Strategy<Value = String> {
    let longest = (parent.family.bits() - parent.prefix).min(8);
    prop_oneof![
        1 => prop::sample::select(&["x", "10.0.0.0/33", "fe80::1%eth0", "::/0"][..])
            .prop_map(str::to_owned),
        1 => network().prop_flat_map(spelling),
        98 => (0..=longest, any::<u128>())
            .prop_flat_map(move |(longer, position)| spelling(subnet(parent, longer, position))),
    ]
}

/// Returns the network within `parent` that is `longer` bits longer, at `position`, modulo the
/// number of such networks.
fn subnet(parent: Net, longer: u8, position: u128) -> Net {
    let prefix = parent.prefix + longer;
    let positions = (1u128 << longer) - 1;
    let offset = (position & positions)
        .checked_shl(u32::from(parent.family.bits() - prefix))
        .unwrap_or(0);
    Net::new(parent.family, parent.first() + offset, prefix)
}

/// The `cidr` and `except` fields of a destination or a flat rule, as `Corpus.destination` draws
/// them: a network, and three times in ten up to three exceptions.
fn destination() -> impl Strategy<Value = Map<String, Value>> {
    network()
        .prop_flat_map(|network| {
            (
                spelling(network),
                prop::option::weighted(0.3, prop::collection::vec(exception(network), 0..=3)),
            )
        })
        .prop_map(|(cidr, except)| {
            let mut fields = Map::new();
            fields.insert("cidr".to_owned(), cidr.into());
            if let Some(except) = except {
                fields.insert("except".to_owned(), except.into());
            }
            fields
        })
}

/// A first port at a boundary, of the port range or of common services, and half the time a last
/// port, one time in ten anywhere above it, and otherwise at most six above it, as
/// `Corpus.selector` draws them.
fn port_range() -> impl Strategy<Value = (u16, Option<u16>)> {
    prop_oneof![1u16..=12, 78u16..=82, 440u16..=445, 65530u16..=65535].prop_flat_map(|first| {
        let last = prop_oneof![
            1 => first..=u16::MAX,
            9 => (0u16..=6).prop_map(move |more| first.saturating_add(more)),
        ];
        (Just(first), prop::option::of(last))
    })
}

/// The `protocol`, `port`, and `endPort` fields of a port selector, if `mxc`, or of a flat rule,
/// as `Corpus.selector` draws them: TCP, UDP, `any`, ICMP, or no protocol, which a flat rule
/// then selects every protocol and port without; and for a protocol other than ICMP seven times
/// in ten a port range, which an ICMP selector has two times in a hundred.
fn selector(mxc: bool) -> impl Strategy<Value = Map<String, Value>> {
    let protocol = prop_oneof![
        35 => Just(Some("tcp")),
        25 => Just(Some("udp")),
        20 => Just(Some("any")),
        10 => Just(Some("icmp")),
        10 => Just(None),
    ];
    (
        protocol,
        prop::bool::weighted(0.7),
        prop::bool::weighted(0.02),
        port_range(),
    )
        .prop_map(move |(protocol, ports, icmp_ports, (first, last))| {
            let mut fields = Map::new();
            if protocol.is_none() && !mxc {
                return fields;
            }
            if let Some(protocol) = protocol {
                fields.insert("protocol".to_owned(), protocol.into());
            }
            if !ports || (protocol == Some("icmp") && !icmp_ports) {
                return fields;
            }
            fields.insert("port".to_owned(), first.into());
            if let Some(last) = last {
                fields.insert("endPort".to_owned(), last.into());
            }
            fields
        })
}

/// A rule, as `Corpus.rule` draws it: 35 times in a hundred a flat rule, and otherwise an MXC
/// rule with, 85 times in a hundred, one to four destinations and, 75 times in a hundred, one to
/// four port selectors.
pub(super) fn rule() -> impl Strategy<Value = Value> {
    prop_oneof![
        35 => (destination(), selector(false)).prop_map(|(mut fields, selector)| {
            fields.extend(selector);
            Value::Object(fields)
        }),
        65 => (
            prop::option::weighted(0.85, prop::collection::vec(destination(), 1..=4)),
            prop::option::weighted(0.75, prop::collection::vec(selector(true), 1..=4)),
        )
            .prop_map(|(to, ports)| {
                let mut fields = Map::new();
                if let Some(to) = to {
                    fields.insert("to".to_owned(), to.into_iter().map(Value::Object).collect());
                }
                if let Some(ports) = ports {
                    let ports = ports.into_iter().map(Value::Object).collect();
                    fields.insert("ports".to_owned(), ports);
                }
                Value::Object(fields)
            }),
    ]
}

/// The rules of one action, as `Corpus.policy` draws them, which 15 policies in a hundred leave
/// out: up to eight rules, or three times in a hundred 20 to 60, and three times in a hundred a
/// crowd after them.
fn action() -> impl Strategy<Value = Option<Value>> {
    let count = prop_oneof![
        97 => prop::sample::select(&[0usize, 1, 2, 3, 4, 6, 8][..]),
        3 => 20usize..=60,
    ];
    let rules = (
        count.prop_flat_map(|count| prop::collection::vec(rule(), count)),
        prop::option::weighted(0.03, crowd()),
    )
        .prop_map(|(mut rules, crowd)| {
            rules.extend(crowd.into_iter().flatten());
            Value::Array(rules)
        });
    prop::option::weighted(0.85, rules)
}

/// A policy, as `Corpus.policy` draws it.
pub(super) fn policy() -> impl Strategy<Value = Value> {
    (action(), action()).prop_map(|(allow, deny)| {
        let mut policy = Map::new();
        for (category, rules) in [("allow", allow), ("deny", deny)] {
            if let Some(rules) = rules {
                policy.insert(category.to_owned(), rules);
            }
        }
        Value::Object(policy)
    })
}

/// A crowd, as `Corpus.crowd` draws it: 80 to 300 flat rules for one port, or a range of up to
/// five ports, of TCP, UDP, or both, on 200 scattered ports, of hosts and small networks of the
/// dense neighborhoods, which needs about as many native rules as OpenVMM accepts.
fn crowd() -> impl Strategy<Value = Vec<Value>> {
    let network = prop_oneof![
        7 => (0..NEIGHBORHOOD, 30u8..=32).prop_map(|(offset, prefix)| {
            Net::new(Family::V4, IPV4_NEIGHBORHOOD + offset, prefix)
        }),
        3 => (0..NEIGHBORHOOD).prop_map(|offset| {
            Net::new(Family::V6, IPV6_NEIGHBORHOOD + offset, 128)
        }),
    ];
    let rule = (
        network,
        0u16..200,
        prop::sample::select(&["tcp", "udp", "any"][..]),
        prop::option::weighted(0.3, 0u16..=4),
    )
        .prop_map(|(network, index, protocol, more)| {
            let port = 2 * index + 1;
            let mut rule = json!({"cidr": network.to_string(), "protocol": protocol, "port": port});
            if let Some(more) = more {
                rule["endPort"] = (port + more).into();
            }
            rule
        });
    prop::collection::vec(rule, 80..=300)
}

/// A crowd of rules without ports, which `Corpus` does not draw: 200 to 300 rules for every
/// protocol, or every port of TCP, UDP, or ICMP, of hosts of the dense neighborhoods that no two
/// rules merge into one network, which need about as many native rules as OpenVMM accepts at
/// each step of the lowering that precedes the sweep over ports.
fn wide_crowd() -> impl Strategy<Value = Vec<Value>> {
    let rule = (
        0..NEIGHBORHOOD,
        any::<bool>(),
        prop::sample::select(&[None, Some("tcp"), Some("udp"), Some("icmp")][..]),
    )
        .prop_map(|(index, ipv6, protocol)| {
            let network = match ipv6 {
                false => Net::new(Family::V4, IPV4_NEIGHBORHOOD + 4 * index + 1, 32),
                true => Net::new(Family::V6, IPV6_NEIGHBORHOOD + 4 * index + 1, 128),
            };
            let mut rule = json!({"cidr": network.to_string()});
            if let Some(protocol) = protocol {
                rule["protocol"] = protocol.into();
            }
            rule
        });
    prop::collection::vec(rule, 200..=300)
}

/// A policy with a crowd in one of its actions: of rules for ports, as `Corpus.crowd` draws
/// them, or of rules without ports.
pub(super) fn crowded_policy() -> impl Strategy<Value = Value> {
    let crowd = prop_oneof![crowd(), wide_crowd()];
    (policy(), any::<bool>(), crowd).prop_map(|(mut policy, deny, crowd)| {
        let category = if deny { "deny" } else { "allow" };
        let rules = policy
            .as_object_mut()
            .unwrap()
            .entry(category)
            .or_insert_with(|| Value::Array(Vec::new()));
        rules.as_array_mut().unwrap().extend(crowd);
        policy
    })
}

/// The breakages of `Corpus.mutate`: a field of a rule and the value that replaces it. One more
/// breakage removes the rule's `cidr`.
fn breakages() -> [(&'static str, Value); 12] {
    [
        ("unknown", json!(1)),
        ("cidr", json!("192.0.2.0/24")),
        ("to", json!([])),
        ("ports", json!([])),
        ("port", json!(true)),
        ("protocol", json!("TCP")),
        ("endPort", json!(0)),
        ("port", json!(99999)),
        ("to", json!("192.0.2.0/24")),
        ("ports", json!([{"protocol": "icmp", "port": 1}])),
        ("ports", json!([{"port": 9, "endPort": 8}])),
        (
            "to",
            json!([{"cidr": "192.0.2.0/24", "except": ["10.0.0.0/8"]}]),
        ),
    ]
}

/// A policy with one part broken, as `Corpus.mutate` breaks it: a field of one of its rules
/// replaced, or its `cidr` removed; or, for a policy without rules, the policy in an array, the
/// policy with an unknown field, or `null`. Some breakages leave a valid policy.
pub(super) fn mutated() -> impl Strategy<Value = Value> {
    (
        policy(),
        any::<prop::sample::Index>(),
        any::<prop::sample::Index>(),
    )
        .prop_map(|(mut policy, rule, breakage)| {
            let count: usize = policy
                .as_object()
                .unwrap()
                .values()
                .map(|rules| rules.as_array().unwrap().len())
                .sum();
            if count == 0 {
                return match breakage.index(3) {
                    0 => json!([policy]),
                    1 => {
                        policy["extra"] = json!([]);
                        policy
                    }
                    _ => Value::Null,
                };
            }
            let rule = policy
                .as_object_mut()
                .unwrap()
                .values_mut()
                .flat_map(|rules| rules.as_array_mut().unwrap())
                .nth(rule.index(count))
                .unwrap();
            let breakages = breakages();
            let breakage = breakage.index(breakages.len() + 1);
            match breakages.into_iter().nth(breakage) {
                Some((field, value)) => rule[field] = value,
                None => {
                    rule.as_object_mut().unwrap().remove("cidr");
                }
            }
            policy
        })
}

/// How `json.dumps` writes JSON.
#[derive(Debug, Clone, Copy)]
struct Format {
    /// The indentation of each level, or `None` to write everything on one line.
    indent: Option<&'static str>,
    /// The separators of items and of names and values, or `None` for those of `indent`.
    separators: Option<(&'static str, &'static str)>,
    /// Whether to escape every character that is not ASCII.
    ensure_ascii: bool,
}

/// Writes `value` as `json.dumps` writes it with `format`, at the nesting level `depth`.
fn write_json(value: &Value, format: Format, depth: usize, out: &mut String) {
    let default = if format.indent.is_some() {
        (",", ": ")
    } else {
        (", ", ": ")
    };
    let (item_separator, key_separator) = format.separators.unwrap_or(default);
    let newline = |out: &mut String, depth: usize| {
        if let Some(indent) = format.indent {
            out.push('\n');
            out.push_str(&indent.repeat(depth));
        }
    };
    match value {
        Value::Array(elements) if !elements.is_empty() => {
            out.push('[');
            for (index, element) in elements.iter().enumerate() {
                if index > 0 {
                    out.push_str(item_separator);
                }
                newline(out, depth + 1);
                write_json(element, format, depth + 1, out);
            }
            newline(out, depth);
            out.push(']');
        }
        Value::Object(members) if !members.is_empty() => {
            out.push('{');
            for (index, (name, member)) in members.iter().enumerate() {
                if index > 0 {
                    out.push_str(item_separator);
                }
                newline(out, depth + 1);
                write_string(name, format.ensure_ascii, out);
                out.push_str(key_separator);
                write_json(member, format, depth + 1, out);
            }
            newline(out, depth);
            out.push('}');
        }
        Value::String(text) => write_string(text, format.ensure_ascii, out),
        other => out.push_str(&other.to_string()),
    }
}

/// Writes `text` as a JSON string, with every character that is not ASCII escaped if
/// `ensure_ascii` is set.
fn write_string(text: &str, ensure_ascii: bool, out: &mut String) {
    let quoted = Value::from(text).to_string();
    if !ensure_ascii {
        out.push_str(&quoted);
        return;
    }
    for character in quoted.chars() {
        if character.is_ascii() {
            out.push(character);
        } else {
            for unit in character.encode_utf16(&mut [0; 2]) {
                write!(out, "\\u{unit:04x}").unwrap();
            }
        }
    }
}

/// Returns `text` with the integer of its first `port` or `endPort` field replaced by
/// `replacement`, as `Corpus.text` replaces it with a regular expression.
fn replace_first_port(text: &str, replacement: &str) -> String {
    let mut names: Vec<(usize, &str)> = ["\"port\"", "\"endPort\""]
        .into_iter()
        .flat_map(|name| text.match_indices(name))
        .collect();
    names.sort_unstable();
    for (start, name) in names {
        let after = |index: usize| {
            index
                + text[index..]
                    .bytes()
                    .take_while(u8::is_ascii_whitespace)
                    .count()
        };
        let colon = after(start + name.len());
        if !text[colon..].starts_with(':') {
            continue;
        }
        let digits = after(colon + 1);
        let length = text[digits..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        if length > 0 {
            return format!(
                "{}{replacement}{}",
                &text[..digits],
                &text[digits + length..]
            );
        }
    }
    text.to_owned()
}

/// The bytes of a policy file, as `Corpus.cases` and `Corpus.text` write them: one policy in
/// eight [`mutated`], written as `json.dumps` may write it, and a few times in a hundred with the
/// integer of a port replaced by another number or a constant, `"allow": []` added to the first
/// object, or the text cut short.
pub(super) fn policy_file() -> impl Strategy<Value = Vec<u8>> {
    let replacements = [
        "NaN",
        "Infinity",
        "1e400",
        "80.0",
        &"9".repeat(65),
        &"9".repeat(64),
        "true",
    ]
    .map(str::to_owned);
    (
        prop_oneof![88 => policy(), 12 => mutated()],
        prop::sample::select(&[None, None, None, Some(""), Some(" "), Some("\t")][..]),
        prop::sample::select(&[None, Some((",", ":")), Some((", ", ": "))][..]),
        prop::bool::weighted(0.8),
        prop::option::weighted(0.04, prop::sample::select(replacements.to_vec())),
        prop::bool::weighted(0.03),
        prop::option::weighted(0.03, any::<prop::sample::Index>()),
    )
        .prop_map(
            |(value, indent, separators, ensure_ascii, replacement, allow, cut)| {
                let format = Format {
                    indent,
                    separators,
                    ensure_ascii,
                };
                let mut text = String::new();
                write_json(&value, format, 0, &mut text);
                if let Some(replacement) = replacement {
                    text = replace_first_port(&text, &replacement);
                }
                if allow {
                    text = text.replacen('{', "{\"allow\": [], ", 1);
                }
                let mut data = text.into_bytes();
                if let Some(cut) = cut {
                    data.truncate(cut.index(data.len() + 1));
                }
                data
            },
        )
}

#[test]
fn files_are_written_as_json_dumps_writes_them() {
    let value = json!({"allow": [{"cidr": "\u{e9}", "port": 80}], "deny": []});
    let write = |indent, separators, ensure_ascii| {
        let mut text = String::new();
        let format = Format {
            indent,
            separators,
            ensure_ascii,
        };
        write_json(&value, format, 0, &mut text);
        text
    };
    assert_eq!(
        write(None, None, true),
        r#"{"allow": [{"cidr": "\u00e9", "port": 80}], "deny": []}"#
    );
    assert_eq!(
        write(Some("\t"), Some((",", ":")), false),
        "{\n\t\"allow\":[\n\t\t{\n\t\t\t\"cidr\":\"\u{e9}\",\n\t\t\t\"port\":80\n\t\t}\n\t],\n\t\"deny\":[]\n}"
    );
    assert_eq!(
        replace_first_port(r#"{"endPort" : 9, "port": 8}"#, "NaN"),
        r#"{"endPort" : NaN, "port": 8}"#
    );
    assert_eq!(
        replace_first_port(r#"{"port": true, "endPort": 8}"#, "1e400"),
        r#"{"port": true, "endPort": 1e400}"#
    );
}

proptest! {
    /// The model reads every spelling of a network as that network, but for the hostmasks of `/0`
    /// and `/32` networks, `255.255.255.255` and `0.0.0.0`, which it reads as netmasks, as Python's
    /// `ip_interface` read them when `Corpus` wrote them.
    #[test]
    fn the_model_reads_every_spelling_as_its_network(
        (network, text) in network().prop_flat_map(|network| (Just(network), spelling(network)))
    ) {
        let expected = match text.split_once('/') {
            Some((address, mask @ ("0.0.0.0" | "255.255.255.255"))) => {
                let address = u32::from(address.parse::<Ipv4Addr>().unwrap());
                let prefix = if mask == "0.0.0.0" { 0 } else { 32 };
                Net::new(Family::V4, address.into(), prefix)
            }
            _ => network,
        };
        prop_assert_eq!(Net::parse(&text), Ok(expected), "{}", text);
    }
}
