//! Egress policy files, which `nvx-rs run` and `nvx-rs sandbox` accept with
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

pub(crate) mod json;
#[cfg(test)]
mod tests;

use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde::Serialize;
use serde_json::{Map, Value};

use self::json::ReadError;
use super::{Access, Destination, EgressRule, EgressRules, NetworkPolicy, PortSelector, Protocol};
use crate::error::{Error, Result};

/// Largest number of native rules that one action, allow or deny, compiles to: the number of
/// rules of each action that OpenVMM accepts, [`MAX_RULES`](super::MAX_RULES).
pub(crate) const MAX_RULES_PER_ACTION: usize = super::MAX_RULES;

/// Largest egress policy file, in bytes: 1 MiB.
pub(crate) const MAX_POLICY_FILE_SIZE: usize = 1024 * 1024;

/// Deepest nesting of arrays and objects in a policy file, the root included. A policy needs six:
/// the root, an action, a rule, its `to`, a destination, and its `except`.
pub(crate) const MAX_JSON_DEPTH: usize = 32;

/// The native OpenVMM rules of an egress policy.
///
/// Each rule is an `--network-egress-allow` or `--network-egress-deny` value: `CIDR`,
/// `CIDR:PROTOCOL`, `CIDR:PROTOCOL:PORT`, or `CIDR:PROTOCOL:FIRST-LAST`. Its serde form is
/// `{"allow": [...], "deny": [...]}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct CompiledEgressPolicy {
    /// Rules of traffic to permit, at most [`MAX_RULES_PER_ACTION`].
    pub(crate) allow: Vec<String>,
    /// Rules of traffic to block, which OpenVMM applies first, at most [`MAX_RULES_PER_ACTION`].
    pub(crate) deny: Vec<String>,
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
pub(crate) fn compile_policy_file(path: &Path) -> Result<CompiledEgressPolicy> {
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
pub(crate) fn compile_policy_bytes(data: &[u8], path: &Path) -> Result<CompiledEgressPolicy> {
    let path = path.display();
    if data.len() > MAX_POLICY_FILE_SIZE {
        return Err(Error::unsupported(format!(
            "egress policy file exceeds {MAX_POLICY_FILE_SIZE}-byte limit: {path}"
        )));
    }
    let text = std::str::from_utf8(data).map_err(|error| {
        Error::unsupported(format!("egress policy file {path} is not UTF-8: {error}"))
    })?;
    let value = json::read(text).map_err(|error| {
        Error::unsupported(match error {
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
pub(crate) fn compile_policy(value: &Value) -> Result<CompiledEgressPolicy> {
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
            Some(name) => Err(Error::unsupported(format!(
                "{description} has unknown field {name:?}"
            ))),
            None => Ok(()),
        }
    }
}

/// Returns the object `value`, as `_object` does.
fn object<'a>(value: &'a Value, description: &str) -> Result<Object<'a>> {
    match value {
        Value::Object(fields) => Ok(Object { fields }),
        _ => Err(Error::unsupported(format!(
            "{description} must be an object"
        ))),
    }
}

/// Returns the elements of the array `value`, as `_array` does.
fn array<'a>(value: &'a Value, description: &str) -> Result<&'a [Value]> {
    match value {
        Value::Array(elements) => Ok(elements),
        _ => Err(Error::unsupported(format!(
            "{description} must be an array"
        ))),
    }
}

/// Returns the value of a field that should be a port number.
fn port_value(value: &Value, description: &str) -> Result<u16> {
    let not_integer = || Error::unsupported(format!("{description} must be an integer"));
    let out_of_range = || Error::unsupported(format!("{description} must be between 1 and 65535"));
    let Value::Number(number) = value else {
        return Err(not_integer());
    };
    if let Some(port) = number.as_u64() {
        return u16::try_from(port)
            .ok()
            .filter(|&port| port != 0)
            .ok_or_else(out_of_range);
    }
    // `serde_json` reads an integer beyond 64 bits as a double, which is out of range either way,
    // as it reads a number with a fraction or an exponent, or `-0`, which are not integers.
    const BEYOND_64_BITS: f64 = 9_223_372_036_854_775_808.0;
    if number.as_i64().is_some()
        || number
            .as_f64()
            .is_some_and(|double| double >= BEYOND_64_BITS || double <= -BEYOND_64_BITS)
    {
        Err(out_of_range())
    } else {
        Err(not_integer())
    }
}

/// Returns the protocol of the field `value`, as the policy reader checks it.
fn protocol(value: &Value, description: &str) -> Result<Protocol> {
    match value.as_str() {
        Some("tcp") => Ok(Protocol::Tcp),
        Some("udp") => Ok(Protocol::Udp),
        Some("icmp") => Ok(Protocol::Icmp),
        Some("any") => Ok(Protocol::Any),
        _ => Err(Error::unsupported(format!(
            "{description}.protocol must be tcp, udp, icmp, or any"
        ))),
    }
}

/// Compiles the rules of one action, which a policy without the field does not have, as
/// `_compile_category` does: every rule is checked before any is lowered.
fn compile_category(value: Option<&Value>, category: &str) -> Result<Vec<String>> {
    native_category(parse_category(value, category)?, value, category)
}

/// Compiles public rules, retaining the field paths of legacy flat rules in refusals.
fn native_category(
    rules: Vec<EgressRule>,
    value: Option<&Value>,
    category: &str,
) -> Result<Vec<String>> {
    let mut policy = NetworkPolicy::new(Access::Deny);
    let action = if category == "allow" {
        &mut policy.allow
    } else {
        &mut policy.deny
    };
    *action = EgressRules {
        name: category.to_owned(),
        rules,
    };
    let native = policy.native_egress_rules().map_err(|error| {
        if let Some(elements) = value.and_then(Value::as_array) {
            for (index, element) in elements.iter().enumerate() {
                if element.get("cidr").is_some() {
                    let path = format!("{category}[{index}]");
                    if let Some(rest) = error.message().strip_prefix(&format!("{path}.to[0]")) {
                        return Error::unsupported(format!("{path}{rest}"));
                    }
                }
            }
        }
        error
    })?;
    Ok(if category == "allow" {
        native.allow
    } else {
        native.deny
    })
}

/// Parses the rules of one action, which a policy without the field does not have.
fn parse_category(value: Option<&Value>, category: &str) -> Result<Vec<EgressRule>> {
    let elements = match value {
        Some(value) => array(value, category)?,
        None => &[],
    };
    let mut rules = Vec::new();
    for (index, element) in elements.iter().enumerate() {
        let mut rule = EgressRule::default();
        let parsed = parse_rule(element, &format!("{category}[{index}]"), &mut rule);
        rules.push(rule);
        if let Err(error) = parsed {
            // Earlier CIDRs still precede this JSON error, but native budgets follow parsing.
            if let Err(earlier) = native_category(rules, value, category)
                && earlier.message()
                    != format!("{category} emits at most {MAX_RULES_PER_ACTION} native rules")
            {
                return Err(earlier);
            }
            return Err(error);
        }
    }
    Ok(rules)
}

/// Parses a rule of either form, as `_parse_rule` does.
fn parse_rule(value: &Value, description: &str, parsed: &mut EgressRule) -> Result<()> {
    let rule = object(value, description)?;
    if rule.has_any(LEGACY_RULE_FIELDS) {
        if rule.has_any(MXC_RULE_FIELDS) {
            return Err(Error::unsupported(format!(
                "{description} cannot mix MXC to/ports fields with legacy flat fields"
            )));
        }
        return parse_legacy_rule(&rule, description, parsed);
    }
    parse_mxc_rule(&rule, description, parsed)
}

/// Parses a destination network and its exceptions, the required `cidr` and optional `except`
/// fields of `description`, as the rest of `_parse_peer` does.
fn parse_peer_fields(
    object: &Object<'_>,
    description: &str,
    destinations: &mut Vec<Destination>,
) -> Result<()> {
    let Some(cidr) = object.get("cidr") else {
        return Err(Error::unsupported(format!(
            "{description}.cidr is required"
        )));
    };
    let index = destinations.len();
    destinations.push(Destination::new(cidr_string(
        cidr,
        &format!("{description}.cidr"),
    )?));
    let destination = &mut destinations[index];
    let exceptions = match object.get("except") {
        Some(except) => array(except, &format!("{description}.except"))?,
        None => &[],
    };
    for (index, exception) in exceptions.iter().enumerate() {
        destination.except.push(cidr_string(
            exception,
            &format!("{description}.except[{index}]"),
        )?);
    }
    Ok(())
}

/// Reads a CIDR string, whose address syntax the public rule compiler checks.
fn cidr_string(value: &Value, description: &str) -> Result<String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        _ => Err(Error::unsupported(format!(
            "{description} must be an IPv4 or IPv6 CIDR string"
        ))),
    }
}

/// Parses a destination of an MXC rule, as `_parse_peer` does.
fn parse_peer(value: &Value, description: &str, destinations: &mut Vec<Destination>) -> Result<()> {
    let peer = object(value, description)?;
    peer.check_fields(PEER_FIELDS, description)?;
    parse_peer_fields(&peer, description, destinations)
}

/// Parses a legacy flat rule, as `_parse_legacy_rule` does.
fn parse_legacy_rule(rule: &Object<'_>, description: &str, parsed: &mut EgressRule) -> Result<()> {
    rule.check_fields(LEGACY_RULE_FIELDS, description)?;
    parse_peer_fields(rule, description, &mut parsed.to)?;
    let port = rule.get("port");
    let end_port = rule.get("endPort");
    if end_port.is_some() && port.is_none() {
        return Err(Error::unsupported(format!(
            "{description}.endPort requires port"
        )));
    }
    let Some(protocol) = rule.get("protocol") else {
        if port.is_some() {
            return Err(Error::unsupported(format!(
                "{description}.port requires protocol"
            )));
        }
        return Ok(());
    };
    let protocol = self::protocol(protocol, description)?;
    parsed
        .ports
        .push(port_selector(rule, protocol, description)?);
    Ok(())
}

/// Parses a port selector of an MXC rule, as `_parse_mxc_port` does.
fn parse_mxc_port(value: &Value, description: &str) -> Result<PortSelector> {
    let port = object(value, description)?;
    port.check_fields(PORT_FIELDS, description)?;
    let protocol = match port.get("protocol") {
        Some(protocol) => self::protocol(protocol, description)?,
        None => Protocol::Any,
    };
    port_selector(&port, protocol, description)
}

/// Reads the JSON ports into the public selector without losing their type diagnostics.
fn port_selector(
    fields: &Object<'_>,
    protocol: Protocol,
    description: &str,
) -> Result<PortSelector> {
    let port = fields.get("port");
    let end_port = fields.get("endPort");
    if end_port.is_some() && port.is_none() {
        return Err(Error::unsupported(format!(
            "{description}.endPort requires port"
        )));
    }
    if port.is_some() && protocol == Protocol::Icmp {
        return Err(Error::unsupported(format!(
            "{description}.port is not supported with icmp"
        )));
    }
    let port = port
        .map(|value| port_value(value, &format!("{description}.port")))
        .transpose()?;
    let end_port = end_port
        .map(|value| port_value(value, &format!("{description}.endPort")))
        .transpose()?;
    if let (Some(start), Some(end)) = (port, end_port)
        && end < start
    {
        return Err(Error::unsupported(format!(
            "{description}.endPort {end} cannot be below port {start}"
        )));
    }
    Ok(PortSelector {
        protocol,
        port,
        end_port,
    })
}

/// Parses an MXC rule, as `_parse_mxc_rule` does.
fn parse_mxc_rule(rule: &Object<'_>, description: &str, parsed: &mut EgressRule) -> Result<()> {
    rule.check_fields(MXC_RULE_FIELDS, description)?;
    if let Some(to) = rule.get("to") {
        let description = format!("{description}.to");
        let peers = array(to, &description)?;
        if peers.is_empty() {
            return Err(Error::unsupported(format!(
                "{description} must contain at least one destination"
            )));
        }
        for (index, peer) in peers.iter().enumerate() {
            parse_peer(peer, &format!("{description}[{index}]"), &mut parsed.to)?;
        }
    }
    if let Some(ports) = rule.get("ports") {
        let description = format!("{description}.ports");
        let ports = array(ports, &description)?;
        if ports.is_empty() {
            return Err(Error::unsupported(format!(
                "{description} must contain at least one selector"
            )));
        }
        for (index, port) in ports.iter().enumerate() {
            parsed
                .ports
                .push(parse_mxc_port(port, &format!("{description}[{index}]"))?);
        }
    }
    Ok(())
}
