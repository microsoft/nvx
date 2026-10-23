//! Egress policy files, which `nvx.py run` and `nvx.py sandbox` accept with
//! `--network-egress-policy-file`, and their compilation into OpenVMM's native rules.
//!
//! The compiler replaced the former `scripts/nvx_tools/egress_policy.py` (microsoft/nvx#359): it
//! compiles the policies that it compiled to the same rules, but reads policy files as strict JSON
//! (microsoft/nvx#729).
//!
//! # Format
//!
//! A policy is at most [`MAX_POLICY_FILE_SIZE`] bytes of UTF-8 JSON, as RFC 8259 defines it,
//! without a byte-order mark: `NaN`, `Infinity`, and numbers beyond the range of a double are not
//! JSON, a field may appear only once in an object, and arrays and objects nest at most
//! [`MAX_JSON_DEPTH`] deep. Its root is an object whose only fields are `allow` and `deny`, each an
//! array of rules. Each rule has one of two forms:
//!
//! - The MXC form, `{"to": [PEER, ...], "ports": [PORT, ...]}`, whose fields are both optional:
//!   - each `PEER` is `{"cidr": CIDR, "except": [CIDR, ...]}`, a destination network without
//!     the networks of its optional `except`, which must lie within it; a rule without `to`
//!     selects every address of both families;
//!   - each `PORT` is `{"protocol": PROTOCOL, "port": FIRST, "endPort": LAST}`, all optional,
//!     where `PROTOCOL` is `tcp`, `udp`, `icmp`, or `any`, the default; `port` selects one TCP or
//!     UDP port, and with `endPort` the ports from `port` through `endPort`, from 1 to 65535; a
//!     rule without `ports` selects every protocol and port.
//! - The legacy flat form, `{"cidr": CIDR, "except": [CIDR, ...], "protocol": PROTOCOL, "port":
//!   FIRST, "endPort": LAST}`, which needs `cidr`, and `protocol` with `port`.
//!
//! `any` selects every protocol, or with a port both TCP and UDP. A port is an integer, written
//! without a fraction or an exponent. A `CIDR` is an IPv4 or IPv6 network as Python's
//! `ipaddress.ip_interface` reads it: host bits are cleared, IPv4 netmasks such as `/255.255.0.0`
//! are accepted, and IPv6 scopes are refused.
//!
//! # Compilation
//!
//! Each action compiles to at most [`MAX_RULES_PER_ACTION`] native rules: `CIDR`, `CIDR:PROTOCOL`,
//! `CIDR:PROTOCOL:PORT`, or `CIDR:PROTOCOL:FIRST-LAST`, sorted by family, address, prefix length,
//! protocol, and ports. Destinations are merged into the fewest networks that cover them, rules
//! for every port of a protocol subsume the narrower rules that they cover, and overlapping and
//! adjacent port ranges merge. OpenVMM applies deny rules before allow rules, so each action is
//! compiled on its own.
//!
//! # Errors
//!
//! A refusal names the field that breaks the format by its path, such as `allow[0].to[1].cidr`,
//! or the file, and says why, quoting values as Rust's `{:?}` does, as in
//! `allow[0].ports[0] has duplicate field "port"` or
//! `egress policy file policy.json is not valid JSON: expected value at line 1 column 1`. Paths
//! show as [`Path::display`] shows them.

mod json;
#[cfg(test)]
mod tests;

use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde::Serialize;
use serde_json::{Map, Value};

use self::json::ReadError;
use super::address::{Family, Interval, Network};
use super::lowering::{self, PortValue, Rule, RuleProtocol, Selector, invalid};
use crate::error::{Error, Result};

/// Largest number of native rules that one action, allow or deny, compiles to: the number of
/// rules of each action that OpenVMM accepts, [`MAX_RULES`](super::MAX_RULES).
pub const MAX_RULES_PER_ACTION: usize = super::MAX_RULES;

/// Largest egress policy file, in bytes: 1 MiB.
pub const MAX_POLICY_FILE_SIZE: usize = 1024 * 1024;

/// Deepest nesting of arrays and objects in a policy file, the root included. A policy needs six:
/// the root, an action, a rule, its `to`, a destination, and its `except`.
pub const MAX_JSON_DEPTH: usize = 32;

/// The native OpenVMM rules of an egress policy.
///
/// Each rule is an `--network-egress-allow` or `--network-egress-deny` value: `CIDR`,
/// `CIDR:PROTOCOL`, `CIDR:PROTOCOL:PORT`, or `CIDR:PROTOCOL:FIRST-LAST`. Its serde form is
/// `{"allow": [...], "deny": [...]}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CompiledEgressPolicy {
    /// Rules of traffic to permit, at most [`MAX_RULES_PER_ACTION`].
    pub allow: Vec<String>,
    /// Rules of traffic to block, which OpenVMM applies first, at most [`MAX_RULES_PER_ACTION`].
    pub deny: Vec<String>,
}

/// Compiles the egress policy file at `path`, as `compile_policy_file` of the former
/// `egress_policy.py` did for `nvx.py`.
///
/// Messages name the file as [`Path::display`] shows `path`.
///
/// # Errors
///
/// [`ErrorKind::Io`](crate::ErrorKind::Io) when the file cannot be read; otherwise
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) when the file exceeds
/// [`MAX_POLICY_FILE_SIZE`], is not UTF-8 JSON of the format, or holds an invalid policy; see
/// [`compile_policy_bytes`].
pub fn compile_policy_file(path: &Path) -> Result<CompiledEgressPolicy> {
    let unreadable = |error: std::io::Error| {
        Error::io(format!(
            "failed to read egress policy file: {}",
            path.display()
        ))
        .with_source(error)
    };
    // Reading one byte more than the limit detects oversized files whatever their metadata say.
    let mut data = Vec::new();
    File::open(path)
        .and_then(|file| {
            file.take(MAX_POLICY_FILE_SIZE as u64 + 1)
                .read_to_end(&mut data)
        })
        .map_err(unreadable)?;
    compile_policy_bytes(&data, path)
}

/// Compiles `data`, the contents of the egress policy file at `path`, which messages name as
/// [`compile_policy_file`] does, after it has read them.
///
/// # Errors
///
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) when the file:
///
/// - exceeds [`MAX_POLICY_FILE_SIZE`]: `egress policy file exceeds 1048576-byte limit: PATH`;
/// - is not UTF-8: `egress policy file PATH is not UTF-8: REASON`;
/// - is not JSON: `egress policy file PATH is not valid JSON: REASON`, where `serde_json` gives
///   the reason and its position;
/// - nests arrays and objects more than [`MAX_JSON_DEPTH`] deep: `egress policy file PATH nests
///   arrays and objects more than 32 deep at line LINE column COLUMN`;
/// - repeats a field in an object: `OBJECT has duplicate field "NAME"`, where `OBJECT` is the
///   path of the object, or `egress policy` for the root; or
/// - holds an invalid policy: see [`compile_policy`].
pub fn compile_policy_bytes(data: &[u8], path: &Path) -> Result<CompiledEgressPolicy> {
    let path = path.display();
    if data.len() > MAX_POLICY_FILE_SIZE {
        return Err(invalid(format!(
            "egress policy file exceeds {MAX_POLICY_FILE_SIZE}-byte limit: {path}"
        )));
    }
    let text = std::str::from_utf8(data)
        .map_err(|error| invalid(format!("egress policy file {path} is not UTF-8: {error}")))?;
    let value = json::read(text).map_err(|error| {
        invalid(match error {
            ReadError::Syntax(error) => {
                format!("egress policy file {path} is not valid JSON: {error}")
            }
            ReadError::TooDeep { line, column } => format!(
                "egress policy file {path} nests arrays and objects more than {MAX_JSON_DEPTH} \
                 deep at line {line} column {column}"
            ),
            ReadError::Repeated { object, field } => {
                format!("{object} has duplicate field {field:?}")
            }
        })
    })?;
    compile_policy(&value)
}

/// Compiles an egress policy that has already been parsed, as `compile_policy` in `nvx.py` does.
///
/// A parsed value cannot repeat a field, and its nesting has no limit: those checks belong to the
/// file format, which [`compile_policy_bytes`] reads.
///
/// # Errors
///
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) with a message that names the field
/// path and says why, such as `allow[0].to[1].cidr is not a valid IPv4 or IPv6 CIDR:
/// "10.0.0.256/8"`, or that an action needs too many native rules, as in `deny emits at most 256
/// native rules`.
pub fn compile_policy(value: &Value) -> Result<CompiledEgressPolicy> {
    let root = object(value, "egress policy")?;
    root.check_fields(ROOT_FIELDS, "egress policy")?;
    Ok(CompiledEgressPolicy {
        allow: compile_category(root.get("allow"), "allow")?,
        deny: compile_category(root.get("deny"), "deny")?,
    })
}

/// Field names of a policy.
const ROOT_FIELDS: &[&str] = &["allow", "deny"];
/// Field names of an MXC rule.
const MXC_RULE_FIELDS: &[&str] = &["to", "ports"];
/// Field names of a destination of an MXC rule.
const PEER_FIELDS: &[&str] = &["cidr", "except"];
/// Field names of a port selector of an MXC rule.
const PORT_FIELDS: &[&str] = &["protocol", "port", "endPort"];
/// Field names of a legacy flat rule.
const LEGACY_RULE_FIELDS: &[&str] = &["cidr", "except", "protocol", "port", "endPort"];

/// A JSON object of a policy.
struct Object<'a> {
    /// The object's fields.
    fields: &'a Map<String, Value>,
}

impl<'a> Object<'a> {
    /// Returns the value of the field `name`, if the object has it.
    fn get(&self, name: &str) -> Option<&'a Value> {
        self.fields.get(name)
    }

    /// Returns whether the object has any of `names`.
    fn has_any(&self, names: &[&str]) -> bool {
        names.iter().any(|name| self.fields.contains_key(*name))
    }

    /// Fails if the object has a field other than `names`, naming the first unknown field in
    /// code-point order, as `sorted(set(rule) - FIELDS)[0]` does.
    fn check_fields(&self, names: &[&str], description: &str) -> Result<()> {
        let unknown = self
            .fields
            .keys()
            .filter(|name| !names.contains(&name.as_str()))
            .min();
        match unknown {
            Some(name) => Err(invalid(format!("{description} has unknown field {name:?}"))),
            None => Ok(()),
        }
    }
}

/// Returns the object `value`, as `_object` does.
fn object<'a>(value: &'a Value, description: &str) -> Result<Object<'a>> {
    match value {
        Value::Object(fields) => Ok(Object { fields }),
        _ => Err(invalid(format!("{description} must be an object"))),
    }
}

/// Returns the elements of the array `value`, as `_array` does.
fn array<'a>(value: &'a Value, description: &str) -> Result<&'a [Value]> {
    match value {
        Value::Array(elements) => Ok(elements),
        _ => Err(invalid(format!("{description} must be an array"))),
    }
}

/// Returns the value of a field that should be a port number.
fn port_value(value: &Value) -> PortValue {
    let Value::Number(number) = value else {
        return PortValue::Other;
    };
    if let Some(port) = number.as_u64() {
        return PortValue::Integer(port.into());
    }
    if let Some(port) = number.as_i64() {
        return PortValue::Integer(port.into());
    }
    // `serde_json` reads an integer beyond 64 bits as a double, which is out of range either way,
    // as it reads a number with a fraction or an exponent, or `-0`, which are not integers.
    const BEYOND_64_BITS: f64 = 9_223_372_036_854_775_808.0;
    match number.as_f64() {
        Some(double) if double >= BEYOND_64_BITS => PortValue::Integer(i128::MAX),
        Some(double) if double <= -BEYOND_64_BITS => PortValue::Integer(i128::MIN),
        _ => PortValue::Other,
    }
}

/// Returns the protocol of the field `value`, as the policy reader checks it.
fn protocol(value: &Value, description: &str) -> Result<RuleProtocol> {
    value.as_str().and_then(RuleProtocol::parse).ok_or_else(|| {
        invalid(format!(
            "{description}.protocol must be tcp, udp, icmp, or any"
        ))
    })
}

/// Compiles the rules of one action, which a policy without the field does not have, as
/// `_compile_category` does: every rule is checked before any is lowered.
fn compile_category(value: Option<&Value>, category: &str) -> Result<Vec<String>> {
    lowering::lower(&parse_category(value, category)?, category)
}

/// Parses the rules of one action, which a policy without the field does not have.
fn parse_category(value: Option<&Value>, category: &str) -> Result<Vec<Rule>> {
    let elements = match value {
        Some(value) => array(value, category)?,
        None => &[],
    };
    let mut rules = Vec::new();
    for (index, element) in elements.iter().enumerate() {
        rules.extend(parse_rule(element, &format!("{category}[{index}]"))?);
    }
    Ok(rules)
}

/// Parses a rule of either form, as `_parse_rule` does.
fn parse_rule(value: &Value, description: &str) -> Result<Vec<Rule>> {
    let rule = object(value, description)?;
    if rule.has_any(LEGACY_RULE_FIELDS) {
        if rule.has_any(MXC_RULE_FIELDS) {
            return Err(invalid(format!(
                "{description} cannot mix MXC to/ports fields with legacy flat fields"
            )));
        }
        return parse_legacy_rule(&rule, description);
    }
    parse_mxc_rule(&rule, description)
}

/// Parses a destination network and its exceptions, the required `cidr` and optional `except`
/// fields of `description`, as the rest of `_parse_peer` does.
fn parse_peer_fields(object: &Object<'_>, description: &str) -> Result<(Family, Vec<Interval>)> {
    let Some(cidr) = object.get("cidr") else {
        return Err(invalid(format!("{description}.cidr is required")));
    };
    let parent = parse_network(cidr, &format!("{description}.cidr"))?;
    let exceptions = match object.get("except") {
        Some(except) => array(except, &format!("{description}.except"))?,
        None => &[],
    };
    lowering::destination(
        parent,
        exceptions.iter().enumerate().map(|(index, exception)| {
            parse_network(exception, &format!("{description}.except[{index}]"))
        }),
        description,
    )
}

/// Parses the CIDR value of a field, as `_network` does.
fn parse_network(value: &Value, description: &str) -> Result<Network> {
    match value {
        Value::String(text) => lowering::network(text, description),
        _ => Err(lowering::not_a_cidr_string(description)),
    }
}

/// Parses a destination of an MXC rule, as `_parse_peer` does.
fn parse_peer(value: &Value, description: &str) -> Result<(Family, Vec<Interval>)> {
    let peer = object(value, description)?;
    peer.check_fields(PEER_FIELDS, description)?;
    parse_peer_fields(&peer, description)
}

/// Parses a legacy flat rule, as `_parse_legacy_rule` does.
fn parse_legacy_rule(rule: &Object<'_>, description: &str) -> Result<Vec<Rule>> {
    rule.check_fields(LEGACY_RULE_FIELDS, description)?;
    let destinations = Some(vec![parse_peer_fields(rule, description)?]);
    let port = rule.get("port");
    let end_port = rule.get("endPort");
    if end_port.is_some() && port.is_none() {
        return Err(invalid(format!("{description}.endPort requires port")));
    }
    let Some(protocol) = rule.get("protocol") else {
        if port.is_some() {
            return Err(invalid(format!("{description}.port requires protocol")));
        }
        return Ok(lowering::combine(destinations, None));
    };
    let protocol = self::protocol(protocol, description)?;
    let selectors = lowering::port_selectors(
        protocol,
        port.map(port_value),
        end_port.map(port_value),
        description,
    )?;
    Ok(lowering::combine(destinations, Some(selectors)))
}

/// Parses a port selector of an MXC rule, as `_parse_mxc_port` does.
fn parse_mxc_port(value: &Value, description: &str) -> Result<Vec<Selector>> {
    let port = object(value, description)?;
    port.check_fields(PORT_FIELDS, description)?;
    let protocol = match port.get("protocol") {
        Some(protocol) => self::protocol(protocol, description)?,
        None => RuleProtocol::Any,
    };
    lowering::port_selectors(
        protocol,
        port.get("port").map(port_value),
        port.get("endPort").map(port_value),
        description,
    )
}

/// Parses an MXC rule, as `_parse_mxc_rule` does.
fn parse_mxc_rule(rule: &Object<'_>, description: &str) -> Result<Vec<Rule>> {
    rule.check_fields(MXC_RULE_FIELDS, description)?;
    let mut destinations = None;
    if let Some(to) = rule.get("to") {
        let description = format!("{description}.to");
        let peers = array(to, &description)?;
        if peers.is_empty() {
            return Err(invalid(format!(
                "{description} must contain at least one destination"
            )));
        }
        let mut parsed = Vec::with_capacity(peers.len());
        for (index, peer) in peers.iter().enumerate() {
            parsed.push(parse_peer(peer, &format!("{description}[{index}]"))?);
        }
        // A native rule names one family, so the peers of each family form their own
        // destination set.
        destinations = Some(lowering::group_destinations(&parsed));
    }
    let mut selectors = None;
    if let Some(ports) = rule.get("ports") {
        let description = format!("{description}.ports");
        let ports = array(ports, &description)?;
        if ports.is_empty() {
            return Err(invalid(format!(
                "{description} must contain at least one selector"
            )));
        }
        let mut parsed = Vec::new();
        for (index, port) in ports.iter().enumerate() {
            parsed.extend(parse_mxc_port(port, &format!("{description}[{index}]"))?);
        }
        selectors = Some(parsed);
    }
    Ok(lowering::combine(destinations, selectors))
}
