//! Properties of the egress policy compiler, which every policy has, checked over the policies
//! that [`generator`] draws and over networks at the ends of each family's addresses, which it
//! draws only by chance.
//!
//! [`check_policy`] checks those of a policy value: the compiler compiles what the
//! [model](super::model) reads, each action lowers to canonical native rules, none redundant, that
//! match exactly what the model matches, and an action is refused exactly when it needs more than
//! [`MAX_RULES_PER_ACTION`] native rules; `allow` and `deny` lower independently; and compiling the
//! policy's text, its value, and `NetworkPolicy` rules agree, deterministically. [`check_file`]
//! checks those of the bytes of a policy file, and [`check_transformations`] that reordering rules,
//! repeating them, and splitting a network into its halves change no lowering.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use serde_json::{Map, Value, json};

use super::generator;
use super::model::{self, Native, Net};
use crate::ErrorCode;
use crate::host::network::lowering;
use crate::host::network::policy::{
    CompiledEgressPolicy, MAX_RULES_PER_ACTION, compile_policy, compile_policy_bytes, object,
    parse_category,
};
use crate::host::network::{
    Access, Destination, EgressRule, EgressRules, NetworkPolicy, PortSelector, Protocol,
    openvmm_arguments,
};

/// The result of a check.
type Checked = Result<(), TestCaseError>;

/// A compilation as the properties compare it: its rules, or its error's code and message.
type Outcome = Result<CompiledEgressPolicy, (ErrorCode, String)>;

/// The actions of a policy, in the order in which the compiler compiles them.
const ACTIONS: [&str; 2] = ["allow", "deny"];

/// The guest network of the `NetworkPolicy`s that the properties lower.
const GUEST_NETWORK: &str = "10.0.0.2/24";

/// Returns the outcome of a compilation.
fn outcome(result: crate::error::Result<CompiledEgressPolicy>) -> Outcome {
    result.map_err(|error| (error.code(), error.message().to_owned()))
}

/// Compiles the policy file `data`.
fn compile_file(data: &[u8]) -> Outcome {
    outcome(compile_policy_bytes(data, Path::new("policy.json")))
}

/// Compiles the policy value `value`.
fn compile_value(value: &Value) -> Outcome {
    outcome(compile_policy(value))
}

/// The limit of native rules up to which [`lower_lifted`] lowers an action: far above what
/// OpenVMM accepts, and what the generator's policies need, but low enough that a lowering that
/// emits too many rules fails fast.
const LIFTED_LIMIT: usize = 4 * MAX_RULES_PER_ACTION;

/// Returns the hosts of `count` rules that no two rules merge into one network.
fn hosts(count: usize) -> impl Iterator<Item = String> {
    (0..count).map(|index| format!("10.{}.0.{}", index / 128, 2 * (index % 128) + 1))
}

/// The errors with which the compiler refuses the rules of an action that need more native rules
/// than OpenVMM accepts, by the name of the rules: `allow` and `deny` for the actions of a
/// policy, and `egress.allow` and `egress.deny` for those of a `NetworkPolicy`. These are the
/// errors for one host more than the limit.
static LIMIT_ERRORS: LazyLock<BTreeMap<String, (ErrorCode, String)>> = LazyLock::new(|| {
    let mut errors = BTreeMap::new();
    for category in ACTIONS {
        let rules: Vec<Value> = hosts(MAX_RULES_PER_ACTION + 1)
            .map(|cidr| json!({"cidr": cidr}))
            .collect();
        let error = compile_value(&json!({category: rules})).unwrap_err();
        errors.insert(category.to_owned(), error);
        let mut policy = NetworkPolicy::new(Access::Allow);
        let rules = &mut action_rules_mut(&mut policy, category).rules;
        rules.extend(hosts(MAX_RULES_PER_ACTION + 1).map(EgressRule::to));
        let error = lower_network_policy(&policy).unwrap_err();
        errors.insert(format!("egress.{category}"), error);
    }
    errors
});

/// Returns whether `outcome` refuses the rules that its message calls `name` for needing more
/// native rules than OpenVMM accepts, as [`LIMIT_ERRORS`] holds the refusal.
fn refuses(outcome: &Outcome, name: &str) -> bool {
    outcome.as_ref().err() == Some(&LIMIT_ERRORS[name])
}

/// Returns the rules of the action `category` of `compiled`.
fn action<'a>(compiled: &'a CompiledEgressPolicy, category: &str) -> &'a [String] {
    match category {
        "allow" => &compiled.allow,
        "deny" => &compiled.deny,
        _ => panic!("unknown action {category}"),
    }
}

/// Returns the rules of the action `category` of `compiled`.
fn action_mut<'a>(compiled: &'a mut CompiledEgressPolicy, category: &str) -> &'a mut Vec<String> {
    match category {
        "allow" => &mut compiled.allow,
        "deny" => &mut compiled.deny,
        _ => panic!("unknown action {category}"),
    }
}

/// Returns the rules of the action `category` of `policy`.
fn action_rules_mut<'a>(policy: &'a mut NetworkPolicy, category: &str) -> &'a mut EgressRules {
    match category {
        "allow" => &mut policy.allow,
        "deny" => &mut policy.deny,
        _ => panic!("unknown action {category}"),
    }
}

/// Lowers the action `category` of `value`, whose root the compiler accepts, as the compiler
/// lowers it, but within [`LIFTED_LIMIT`] native rules: the rules that it needs, or the error of
/// [`LIMIT_ERRORS`] if it needs more.
fn lower_lifted(value: &Value, category: &str) -> Outcome {
    let lowered = object(value, "egress policy")
        .and_then(|root| parse_category(root.get(category), category))
        .and_then(|rules| lowering::lower_within(&rules, category, LIFTED_LIMIT));
    outcome(lowered.map(|rules| {
        let mut compiled = CompiledEgressPolicy::default();
        *action_mut(&mut compiled, category) = rules;
        compiled
    }))
}

/// Checks `lowered`, the native rules of an action whose rules the model reads as `rules`: they
/// are canonical, none is redundant, and they match exactly what `rules` match.
fn check_lowering(rules: &[model::Rule], lowered: &[String]) -> Result<(), String> {
    if let Some(reason) = model::not_canonical(lowered) {
        return Err(format!(
            "the rules are not canonical: {reason}: {lowered:?}"
        ));
    }
    let natives: Vec<Native> = lowered
        .iter()
        .map(|rule| Native::parse(rule).unwrap())
        .collect();
    if let Some((packet, matched)) = model::difference(rules, &natives) {
        let (policy, native) = if matched {
            ("matches", "do not")
        } else {
            ("does not match", "do")
        };
        return Err(format!(
            "the policy {policy} {packet}, but its native rules {native}: {lowered:?}"
        ));
    }
    if let Some(index) = model::redundant(&natives) {
        return Err(format!("{} is redundant: {lowered:?}", lowered[index]));
    }
    Ok(())
}

/// Checks the properties of the policy value `value`:
///
/// - compiling it is deterministic, and compiles its JSON text alike;
/// - the compiler refuses it, with [`ErrorCode::Unsupported`], if the model does not read it;
/// - otherwise each action lowers, within [`LIFTED_LIMIT`] native rules, to canonical native
///   rules, none redundant, that match exactly what the model matches;
/// - an action is refused, under either name, exactly when it needs more than
///   [`MAX_RULES_PER_ACTION`] native rules, and otherwise compiles, alone or beside the other, and
///   under either name, to those it needs, so `allow` and `deny` lower independently; and
/// - `NetworkPolicy` rules lower through `openvmm_arguments` as the policy compiles.
fn check_policy(value: &Value) -> Checked {
    let compiled = compile_value(value);
    prop_assert_eq!(
        &compiled,
        &compile_value(value),
        "compiling {} is not deterministic",
        value
    );
    let text = serde_json::to_vec(value).unwrap();
    prop_assert_eq!(
        &compile_file(&text),
        &compiled,
        "the text of {} compiles differently",
        value
    );
    let Some(policy) = model::read_policy(value) else {
        prop_assert!(
            matches!(compiled, Err((ErrorCode::Unsupported, _))),
            "the format refuses {}, but it compiles to {:?}",
            value,
            compiled
        );
        return Ok(());
    };
    let mut expected = CompiledEgressPolicy::default();
    let mut refused = None;
    for category in ACTIONS {
        // The rules that the action needs, or `None` if it needs more than the lifted limit.
        let lifted = lower_lifted(value, category);
        let lowered = match &lifted {
            Ok(lowered) => Some(action(lowered, category).to_vec()),
            Err(_) if refuses(&lifted, category) => None,
            Err((_, message)) => {
                return Err(TestCaseError::fail(format!(
                    "the format accepts {value}, but {category} does not lower: {message}"
                )));
            }
        };
        if let Some(lowered) = &lowered {
            check_lowering(policy.action(category), lowered).map_err(|reason| {
                TestCaseError::fail(format!("{category} of {value}: {reason}"))
            })?;
        }
        let needed = lowered.as_ref().map_or(LIFTED_LIMIT + 1, Vec::len);
        let rules = value.get(category).cloned().unwrap_or_else(|| json!([]));
        for name in ACTIONS {
            let alone = compile_value(&json!({name: rules.clone()}));
            match &lowered {
                Some(lowered) if needed <= MAX_RULES_PER_ACTION => {
                    let mut wanted = CompiledEgressPolicy::default();
                    *action_mut(&mut wanted, name) = lowered.clone();
                    prop_assert_eq!(alone, Ok(wanted), "{} of {} as {}", category, value, name);
                }
                _ => prop_assert!(
                    refuses(&alone, name),
                    "{} needs {} native rules, but {} gives {:?}",
                    category,
                    needed,
                    name,
                    alone
                ),
            }
        }
        match lowered {
            Some(lowered) if needed <= MAX_RULES_PER_ACTION => {
                *action_mut(&mut expected, category) = lowered;
            }
            _ => {
                refused.get_or_insert(category);
            }
        }
    }
    match refused {
        Some(category) => prop_assert!(
            refuses(&compiled, category),
            "{} needs more than {} native rules, but {} gives {:?}",
            category,
            MAX_RULES_PER_ACTION,
            value,
            compiled
        ),
        None => prop_assert_eq!(&compiled, &Ok(expected), "{}", value),
    }
    check_network_policy(value, &compiled, refused)
}

/// Returns the native rules of the OpenVMM options `arguments`: the values of
/// `--network-egress-allow` and `--network-egress-deny`.
fn egress_rules(arguments: &[String]) -> CompiledEgressPolicy {
    let rules = |option: &str| {
        arguments
            .windows(2)
            .filter(|pair| pair[0] == option)
            .map(|pair| pair[1].clone())
            .collect()
    };
    CompiledEgressPolicy {
        allow: rules("--network-egress-allow"),
        deny: rules("--network-egress-deny"),
    }
}

/// Returns the outcome of lowering `policy` through `openvmm_arguments`.
fn lower_network_policy(policy: &NetworkPolicy) -> Outcome {
    outcome(
        openvmm_arguments(Some(policy), GUEST_NETWORK).map(|arguments| egress_rules(&arguments)),
    )
}

/// Checks that the rules of `value`, a policy that the format accepts, lower through
/// `openvmm_arguments` as `value` compiles, to `compiled`, under the names `egress.allow` and
/// `egress.deny`, refusing the action `refused` if the compiler refuses it, and, with the `cli`
/// feature, that compiled rules that `nvx-rs sandbox` reads back as native rules lower to
/// themselves.
fn check_network_policy(value: &Value, compiled: &Outcome, refused: Option<&str>) -> Checked {
    let policy = network_policy(value)
        .ok_or_else(|| TestCaseError::fail(format!("{value} has no NetworkPolicy")))?;
    let lowered = lower_network_policy(&policy);
    match refused {
        None => prop_assert_eq!(&lowered, compiled, "the NetworkPolicy of {}", value),
        Some(category) => prop_assert!(
            refuses(&lowered, &format!("egress.{category}")),
            "{} refuses {}, but its NetworkPolicy gives {:?}",
            value,
            category,
            lowered
        ),
    }
    #[cfg(feature = "cli")]
    if let Ok(compiled) = compiled {
        check_read_back(compiled)?;
    }
    Ok(())
}

/// Checks that `compiled`'s rules, read back as native rules as `nvx-rs sandbox` reads them,
/// lower to themselves.
#[cfg(feature = "cli")]
fn check_read_back(compiled: &CompiledEgressPolicy) -> Checked {
    let mut policy = NetworkPolicy::new(Access::Allow);
    for (rules, native) in [
        (&mut policy.allow, &compiled.allow),
        (&mut policy.deny, &compiled.deny),
    ] {
        for rule in native {
            let rule = EgressRule::parse_native(rule).map_err(|error| {
                TestCaseError::fail(format!("{rule} does not read back: {}", error.message()))
            })?;
            rules.rules.push(rule);
        }
    }
    prop_assert_eq!(
        lower_network_policy(&policy),
        Ok(compiled.clone()),
        "compiled rules read back lower to other rules"
    );
    Ok(())
}

/// Returns the `NetworkPolicy` with the rules of `value`, a policy that the format accepts: a
/// flat rule is the `EgressRule` of its one destination and, if it names a protocol, its one port
/// selector, and an MXC rule has the fields of an `EgressRule`, empty where it has none.
fn network_policy(value: &Value) -> Option<NetworkPolicy> {
    let mut policy = NetworkPolicy::new(Access::Allow);
    for (category, rules) in [("allow", &mut policy.allow), ("deny", &mut policy.deny)] {
        for rule in elements(value.get(category)) {
            rules.rules.push(egress_rule(rule.as_object()?)?);
        }
    }
    Some(policy)
}

/// Returns the elements of `value` if it is an array, and none otherwise.
fn elements(value: Option<&Value>) -> &[Value] {
    value.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// Returns the `EgressRule` of `rule`, a rule of either form.
fn egress_rule(rule: &Map<String, Value>) -> Option<EgressRule> {
    if model::is_flat(rule) {
        let ports = match rule.contains_key("protocol") {
            true => vec![port_selector(rule)?],
            false => Vec::new(),
        };
        return Some(EgressRule {
            to: vec![destination(rule)?],
            ports,
        });
    }
    Some(EgressRule {
        to: elements(rule.get("to"))
            .iter()
            .map(|peer| destination(peer.as_object()?))
            .collect::<Option<_>>()?,
        ports: elements(rule.get("ports"))
            .iter()
            .map(|selector| port_selector(selector.as_object()?))
            .collect::<Option<_>>()?,
    })
}

/// Returns the `Destination` of the `cidr` and `except` fields of `fields`.
fn destination(fields: &Map<String, Value>) -> Option<Destination> {
    let text = |value: &Value| value.as_str().map(str::to_owned);
    Some(Destination {
        cidr: text(fields.get("cidr")?)?,
        except: elements(fields.get("except"))
            .iter()
            .map(text)
            .collect::<Option<_>>()?,
    })
}

/// Returns the `PortSelector` of the `protocol`, `port`, and `endPort` fields of `fields`.
fn port_selector(fields: &Map<String, Value>) -> Option<PortSelector> {
    let port = |name: &str| -> Option<Option<u16>> {
        match fields.get(name) {
            None => Some(None),
            Some(value) => u16::try_from(value.as_u64()?).ok().map(Some),
        }
    };
    let protocol = match fields.get("protocol").map(Value::as_str) {
        None | Some(Some("any")) => Protocol::Any,
        Some(Some("tcp")) => Protocol::Tcp,
        Some(Some("udp")) => Protocol::Udp,
        Some(Some("icmp")) => Protocol::Icmp,
        Some(_) => return None,
    };
    Some(PortSelector {
        protocol,
        port: port("port")?,
        end_port: port("endPort")?,
    })
}

/// Checks the properties of the policy file `data`: compiling it is deterministic, it is refused,
/// with [`ErrorCode::Unsupported`], unless the model reads the file, and otherwise it compiles as
/// its value does; and the value that it holds, if it is JSON, has the properties of
/// [`check_policy`].
fn check_file(data: &[u8]) -> Checked {
    let compiled = compile_file(data);
    prop_assert_eq!(
        &compiled,
        &compile_file(data),
        "compiling is not deterministic"
    );
    let value = model::read_file(data);
    if let Some(value) = &value {
        check_policy(value)?;
    }
    match value {
        Some(value) if model::read_policy(&value).is_some() => prop_assert_eq!(
            compiled,
            compile_value(&value),
            "the file and its value {} compile differently",
            value
        ),
        _ => prop_assert!(
            matches!(compiled, Err((ErrorCode::Unsupported, _))),
            "the format refuses the file, but it compiles to {:?}",
            compiled
        ),
    }
    Ok(())
}

/// Returns `value` with the rules of each action shuffled: each rule from the last swaps places
/// with an earlier one, or stays, at a position that `swaps`, cycled, chooses.
fn reordered(value: &Value, swaps: &[usize]) -> Value {
    let mut value = value.clone();
    for category in ACTIONS {
        if let Some(rules) = value.get_mut(category).and_then(Value::as_array_mut)
            && !swaps.is_empty()
        {
            for (position, swap) in (1..rules.len()).rev().zip(swaps.iter().cycle()) {
                rules.swap(position, swap % (position + 1));
            }
        }
    }
    value
}

/// Returns `value` with the rules of each action that `copies`, cycled, selects repeated after
/// them.
fn duplicated(value: &Value, copies: &[bool]) -> Value {
    let mut value = value.clone();
    for category in ACTIONS {
        if let Some(rules) = value.get_mut(category).and_then(Value::as_array_mut)
            && !copies.is_empty()
        {
            let repeated: Vec<Value> = rules
                .iter()
                .zip(copies.iter().cycle())
                .filter(|&(_, &copy)| copy)
                .map(|(rule, _)| rule.clone())
                .collect();
            rules.extend(repeated);
        }
    }
    value
}

/// Returns the `cidr` and `except` fields of the halves of the destination whose fields are
/// `fields`, if its network holds more than one address: each half keeps the exceptions within
/// it, and an exception that holds the half leaves it out whole.
fn halves(fields: &Map<String, Value>) -> Option<[Map<String, Value>; 2]> {
    let network = model::read_cidr(fields.get("cidr")?)?;
    let except: Vec<Net> = elements(fields.get("except"))
        .iter()
        .map(model::read_cidr)
        .collect::<Option<_>>()?;
    Some(network.halves()?.map(|half| {
        let except: Vec<Value> = except
            .iter()
            .filter_map(|&exception| {
                if half.contains(exception) {
                    Some(exception)
                } else {
                    exception.contains(half).then_some(half)
                }
            })
            .map(|exception| exception.to_string().into())
            .collect();
        let mut fields = Map::new();
        fields.insert("cidr".to_owned(), half.to_string().into());
        if !except.is_empty() {
            fields.insert("except".to_owned(), except.into());
        }
        fields
    }))
}

/// Returns `value`, a policy that the format accepts, with one destination, the `destination`th,
/// modulo their number, of those whose network holds more than one address, replaced by the
/// [`halves`] of its network, or `None` if no network holds more than one address. A flat rule
/// becomes two.
fn split(value: &Value, destination: usize) -> Option<Value> {
    let splittable = |peer: &Value| peer.as_object().and_then(halves).is_some();
    let mut destinations = Vec::new();
    for category in ACTIONS {
        for (index, rule) in elements(value.get(category)).iter().enumerate() {
            let Some(fields) = rule.as_object() else {
                continue;
            };
            if model::is_flat(fields) {
                if splittable(rule) {
                    destinations.push((category, index, None));
                }
                continue;
            }
            for (peer, fields) in elements(fields.get("to")).iter().enumerate() {
                if splittable(fields) {
                    destinations.push((category, index, Some(peer)));
                }
            }
        }
    }
    let &(category, index, peer) = destinations.get(destination % destinations.len().max(1))?;
    let mut value = value.clone();
    let rules = value[category].as_array_mut()?;
    match peer {
        None => {
            let rule = rules[index].as_object()?.clone();
            let split = halves(&rule)?.map(|half| {
                let mut rule = rule.clone();
                rule.remove("except");
                rule.extend(half);
                Value::Object(rule)
            });
            rules.splice(index..=index, split);
        }
        Some(peer) => {
            let to = rules[index]["to"].as_array_mut()?;
            let split = halves(to[peer].as_object()?)?.map(Value::Object);
            to.splice(peer..=peer, split);
        }
    }
    Some(value)
}

/// Checks that `transformed`, which `transformation` made of `value`, a policy that the format
/// accepts, compiles as `value` does.
fn check_same_lowering(value: &Value, transformed: &Value, transformation: &str) -> Checked {
    prop_assert_eq!(
        compile_value(value),
        compile_value(transformed),
        "{} changes the lowering of {} into {}",
        transformation,
        value,
        transformed
    );
    Ok(())
}

/// Checks that the rules of `value`, a policy that the format accepts, lower alike once
/// [`reordered`] with `swaps`, [`duplicated`] with `copies`, and [`split`] at `destination`.
fn check_transformations(
    value: &Value,
    swaps: &[usize],
    copies: &[bool],
    destination: usize,
) -> Checked {
    check_same_lowering(value, &reordered(value, swaps), "reordering the rules")?;
    check_same_lowering(value, &duplicated(value, copies), "repeating rules")?;
    if let Some(split) = split(value, destination) {
        check_same_lowering(value, &split, "splitting a network into its halves")?;
    }
    Ok(())
}

proptest! {
    /// The policies that the generator draws have the properties of [`check_policy`].
    #[test]
    fn policies_lower_to_canonical_rules_that_match_exactly(policy in generator::policy()) {
        check_policy(&policy)?;
    }

    /// The order of the rules, repeated rules, and splitting a network into its halves change no
    /// lowering.
    #[test]
    fn order_repetition_and_halves_change_no_lowering(
        policy in generator::policy(),
        swaps in prop::collection::vec(any::<usize>(), 1..32),
        copies in prop::collection::vec(any::<bool>(), 1..8),
        destination in any::<usize>(),
    ) {
        if model::read_policy(&policy).is_some() {
            check_transformations(&policy, &swaps, &copies, destination)?;
        }
    }

    /// Policies with a part broken compile or fail with an error, as the model reads them.
    #[test]
    fn broken_policies_compile_or_fail(policy in generator::mutated()) {
        check_policy(&policy)?;
    }

    /// Policy files, formatted as `json.dumps` formats them and sometimes broken, compile as their
    /// values, or fail with an error, as the model reads them.
    #[test]
    fn policy_files_compile_as_the_format_reads_them(data in generator::policy_file()) {
        check_file(&data)?;
    }

    /// Arbitrary bytes compile or fail with an error.
    #[test]
    fn arbitrary_bytes_compile_or_fail(data in prop::collection::vec(any::<u8>(), 0..512)) {
        check_file(&data)?;
    }
}

proptest! {
    // A crowd needs hundreds of native rules, which take long to check without optimizations.
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// An action that needs about as many native rules as OpenVMM accepts is refused exactly when
    /// it needs more.
    #[test]
    fn crowded_actions_are_refused_exactly_when_they_need_more_than_256_rules(
        policy in generator::crowded_policy()
    ) {
        check_policy(&policy)?;
    }
}

/// Networks at the ends of each family's addresses, where the arithmetic of the lowering meets the
/// limits of its integers, have the properties of [`check_policy`]: alone and less an address at
/// an end, in each action, without ports and with ports at the ends of the port range. The
/// generator draws networks there only by chance.
#[test]
fn networks_at_the_ends_of_each_family_have_the_properties() {
    let families = [
        (
            ["0.0.0.0/0", "128.0.0.0/1", "255.255.255.254/31"],
            ["0.0.0.0/32", "255.255.255.254/32", "255.255.255.255/32"],
        ),
        (
            [
                "::/0",
                "8000::/1",
                "ffff:ffff:ffff:ffff:ffff:ffff:ffff:fffe/127",
            ],
            [
                "::/128",
                "ffff:ffff:ffff:ffff:ffff:ffff:ffff:fffe/128",
                "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/128",
            ],
        ),
    ];
    let selectors = [
        json!({}),
        json!({"protocol": "tcp", "port": 65535}),
        json!({"protocol": "any", "port": 1, "endPort": 65535}),
    ];
    for (networks, ends) in families {
        for network in networks.into_iter().chain(ends) {
            for except in [None].into_iter().chain(ends.map(Some)) {
                for selector in &selectors {
                    let mut rule = selector.as_object().unwrap().clone();
                    rule.insert("cidr".to_owned(), json!(network));
                    if let Some(except) = except {
                        rule.insert("except".to_owned(), json!([except]));
                    }
                    for category in ACTIONS {
                        let policy = json!({category: [rule]});
                        if let Err(error) = check_policy(&policy) {
                            panic!("{error}");
                        }
                    }
                }
            }
        }
    }
}
