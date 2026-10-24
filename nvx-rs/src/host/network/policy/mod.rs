//! Egress policy files, which `nvx.py run` and `nvx.py sandbox` accept with
//! `--network-egress-policy-file`, and their compilation into OpenVMM's native rules.
//!
//! This is a faithful port of the former `scripts/nvx_tools/egress_policy.py`, which it replaced
//! (microsoft/nvx#359): it accepts the same documents, compiles them to the same rules, and fails
//! with the same messages.
//!
//! # Format
//!
//! A policy is a JSON object of at most [`MAX_POLICY_FILE_SIZE`] bytes of UTF-8, without a
//! byte-order mark, whose only fields are `allow` and `deny`, each an array of rules. A field may
//! appear only once in an object, and an integer may have at most 64 digits. Each rule has one of
//! two forms:
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
//! `any` selects every protocol, or with a port both TCP and UDP. A `CIDR` is an IPv4 or IPv6
//! network as Python's `ipaddress.ip_interface` reads it: host bits are cleared, IPv4 netmasks
//! such as `/255.255.0.0` are accepted, and IPv6 scopes are refused.
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
//! # Fidelity
//!
//! Messages are Python's, byte for byte, with two representation choices: a string that Python
//! would hold with an unpaired surrogate, which JSON escapes such as `\ud800` and paths that are
//! not Unicode produce, shows each surrogate as `\udXXX`, which is how Python prints it, and
//! paths show as `str(pathlib.Path(path))`. Python's decoder raises `RecursionError` for documents
//! nested more deeply than its stack allows; this reader has no such limit, so those documents
//! fail validation like any other invalid policy.

#[cfg(test)]
mod tests;

use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde::Serialize;

use super::address::Family;
use super::lowering::{self, PortValue, Rule, RuleProtocol, Selector, invalid};
use crate::error::{Error, Result};
use crate::host::python::json::{self, Document, Hooks, Node, NodeId, ReadError};
use crate::host::python::path::python_path;
use crate::host::python::text::PyText;

/// Largest number of native rules that one action, allow or deny, compiles to: the number of
/// rules of each action that OpenVMM accepts, [`MAX_RULES`](super::MAX_RULES).
pub const MAX_RULES_PER_ACTION: usize = super::MAX_RULES;

/// Largest egress policy file, in bytes: 1 MiB.
pub const MAX_POLICY_FILE_SIZE: usize = 1024 * 1024;

/// Longest integer, in digits without its sign, that a policy file may hold.
const MAX_JSON_INTEGER_DIGITS: usize = 64;

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
/// Messages name the file as `str(pathlib.Path(path))`, which drops empty and `.` components and,
/// on Windows, writes `\` for `/`, and the file opened is that normalized path, as in Python.
///
/// # Errors
///
/// [`ErrorKind::Io`](crate::ErrorKind::Io) when the file cannot be read; otherwise
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) when the file exceeds
/// [`MAX_POLICY_FILE_SIZE`], is not UTF-8 JSON that Python reads, or holds an invalid policy;
/// see [`compile_policy_bytes`].
pub fn compile_policy_file(path: &Path) -> Result<CompiledEgressPolicy> {
    let path = python_path(path);
    let unreadable = |error: std::io::Error| {
        Error::io(format!(
            "failed to read egress policy file: {}",
            path.display
        ))
        .with_source(error)
    };
    // Reading one byte more than the limit detects oversized files whatever their metadata say.
    let mut data = Vec::new();
    File::open(&path.native)
        .and_then(|file| {
            file.take(MAX_POLICY_FILE_SIZE as u64 + 1)
                .read_to_end(&mut data)
        })
        .map_err(unreadable)?;
    compile_contents(&data, &path.display)
}

/// Compiles `data`, the contents of the egress policy file at `path`, which messages name as
/// [`compile_policy_file`] does, after it has read them.
///
/// # Errors
///
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) with Python's message:
///
/// - `egress policy file exceeds 1048576-byte limit: PATH`;
/// - `failed to read egress policy file: PATH` for data that is not UTF-8, or not JSON that
///   Python's `json.loads` reads;
/// - `duplicate JSON property: NAME` and `JSON integer exceeds 64-digit limit`; or
/// - an error of [`compile_policy`].
pub fn compile_policy_bytes(data: &[u8], path: &Path) -> Result<CompiledEgressPolicy> {
    compile_contents(data, &python_path(path).display)
}

/// Compiles the contents of the policy file that messages name `path`.
fn compile_contents(data: &[u8], path: &str) -> Result<CompiledEgressPolicy> {
    if data.len() > MAX_POLICY_FILE_SIZE {
        return Err(invalid(format!(
            "egress policy file exceeds {MAX_POLICY_FILE_SIZE}-byte limit: {path}"
        )));
    }
    let unreadable = || invalid(format!("failed to read egress policy file: {path}"));
    let text = std::str::from_utf8(data).map_err(|_| unreadable())?;
    let document = json::read(text, &mut StrictJson).map_err(|error| match error {
        ReadError::Syntax { .. } => unreadable(),
        ReadError::Hook(error) => error,
    })?;
    compile_document(&document)
}

/// Compiles an egress policy that has already been parsed, as `compile_policy` in `nvx.py` does.
///
/// A parsed value cannot repeat a field, and its integers have no length limit: those checks
/// belong to the file format, which [`compile_policy_bytes`] reads.
///
/// # Errors
///
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) with Python's message, such as
/// `allow[0].to[1].cidr is not a valid IPv4 or IPv6 CIDR: 10.0.0.256/8` or `deny emits at most
/// 256 native rules`.
pub fn compile_policy(value: &serde_json::Value) -> Result<CompiledEgressPolicy> {
    compile_document(&Document::from_value(value))
}

/// The checks of `nvx.py`'s policy reader: `strict_json_object` refuses repeated fields, and
/// `_bounded_json_integer` integers of more than 64 digits.
struct StrictJson;

impl Hooks for StrictJson {
    type Error = Error;

    fn object(&mut self, members: &[(PyText, NodeId)]) -> Result<()> {
        let mut seen = std::collections::HashSet::with_capacity(members.len());
        for (name, _) in members {
            if !seen.insert(name) {
                return Err(invalid(format!(
                    "duplicate JSON property: {}",
                    name.render()
                )));
            }
        }
        Ok(())
    }

    fn integer(&mut self, literal: &str) -> Result<()> {
        if literal.trim_start_matches('-').len() > MAX_JSON_INTEGER_DIGITS {
            return Err(invalid(format!(
                "JSON integer exceeds {MAX_JSON_INTEGER_DIGITS}-digit limit"
            )));
        }
        Ok(())
    }
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

/// A JSON object of a policy document.
struct Object<'a> {
    /// The document that holds the object.
    document: &'a Document,
    /// The object's members, whose names are distinct.
    members: &'a [(PyText, NodeId)],
}

impl<'a> Object<'a> {
    /// Returns the value of the field `name`, if the object has it.
    fn get(&self, name: &str) -> Option<NodeId> {
        self.members
            .iter()
            .find(|(field, _)| field.as_bytes() == name.as_bytes())
            .map(|&(_, value)| value)
    }

    /// Returns whether the object has the field `name`.
    fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Fails if the object has a field other than `fields`, naming the first unknown field in
    /// code-point order, as `sorted(set(rule) - FIELDS)[0]` does.
    fn check_fields(&self, fields: &[&str], description: &str) -> Result<()> {
        let unknown = self
            .members
            .iter()
            .map(|(name, _)| name)
            .filter(|name| {
                !fields
                    .iter()
                    .any(|field| name.as_bytes() == field.as_bytes())
            })
            .min();
        match unknown {
            Some(name) => Err(invalid(format!(
                "{description} has unknown field '{}'",
                name.render()
            ))),
            None => Ok(()),
        }
    }

    /// Returns whether the object has any of `fields`.
    fn has_any(&self, fields: &[&str]) -> bool {
        fields.iter().any(|field| self.has(field))
    }

    /// Returns the node of `id` in the object's document.
    fn node(&self, id: NodeId) -> &'a Node {
        self.document.node(id)
    }
}

/// Returns the object at `id`, as `_object` does.
fn object<'a>(document: &'a Document, id: NodeId, description: &str) -> Result<Object<'a>> {
    match document.node(id) {
        Node::Object(members) => Ok(Object { document, members }),
        _ => Err(invalid(format!("{description} must be an object"))),
    }
}

/// Returns the elements of the array at `id`, as `_array` does.
fn array<'a>(document: &'a Document, id: NodeId, description: &str) -> Result<&'a [NodeId]> {
    match document.node(id) {
        Node::Array(elements) => Ok(elements),
        _ => Err(invalid(format!("{description} must be an array"))),
    }
}

/// Returns the value of a field that should be a port number.
fn port_value(node: &Node) -> PortValue {
    match node {
        // Integers longer than an `i128` are out of range either way.
        Node::Integer(literal) => PortValue::Integer(literal.parse().unwrap_or_else(|_| {
            if literal.starts_with('-') {
                i128::MIN
            } else {
                i128::MAX
            }
        })),
        _ => PortValue::Other,
    }
}

/// Returns the protocol of the field at `id`, as the policy reader checks it.
fn protocol(document: &Document, id: NodeId, description: &str) -> Result<RuleProtocol> {
    match document.node(id) {
        Node::String(text) => RuleProtocol::parse(text),
        _ => None,
    }
    .ok_or_else(|| {
        invalid(format!(
            "{description}.protocol must be tcp, udp, icmp, or any"
        ))
    })
}

/// Compiles a policy document, as `compile_policy` does.
fn compile_document(document: &Document) -> Result<CompiledEgressPolicy> {
    let root = object(document, document.root(), "egress policy")?;
    root.check_fields(ROOT_FIELDS, "egress policy")?;
    Ok(CompiledEgressPolicy {
        allow: compile_category(document, root.get("allow"), "allow")?,
        deny: compile_category(document, root.get("deny"), "deny")?,
    })
}

/// Compiles the rules of one action, which a policy without the field does not have, as
/// `_compile_category` does: every rule is checked before any is lowered.
fn compile_category(
    document: &Document,
    id: Option<NodeId>,
    category: &str,
) -> Result<Vec<String>> {
    lowering::lower(&parse_category(document, id, category)?, category)
}

/// Parses the rules of one action, which a policy without the field does not have.
fn parse_category(document: &Document, id: Option<NodeId>, category: &str) -> Result<Vec<Rule>> {
    let elements = match id {
        Some(id) => array(document, id, category)?,
        None => &[],
    };
    let mut rules = Vec::new();
    for (index, &element) in elements.iter().enumerate() {
        rules.extend(parse_rule(
            document,
            element,
            &format!("{category}[{index}]"),
        )?);
    }
    Ok(rules)
}

/// Parses a rule of either form, as `_parse_rule` does.
fn parse_rule(document: &Document, id: NodeId, description: &str) -> Result<Vec<Rule>> {
    let rule = object(document, id, description)?;
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
fn parse_peer_fields(
    object: &Object<'_>,
    description: &str,
) -> Result<(Family, Vec<(u128, u128)>)> {
    let Some(cidr) = object.get("cidr") else {
        return Err(invalid(format!("{description}.cidr is required")));
    };
    let parent = parse_network(object.node(cidr), &format!("{description}.cidr"))?;
    let exceptions = match object.get("except") {
        Some(except) => array(object.document, except, &format!("{description}.except"))?,
        None => &[],
    };
    lowering::destination(
        parent,
        exceptions.iter().enumerate().map(|(index, &exception)| {
            parse_network(
                object.node(exception),
                &format!("{description}.except[{index}]"),
            )
        }),
        description,
    )
}

/// Parses the CIDR value of a field, as `_network` does.
fn parse_network(node: &Node, description: &str) -> Result<super::address::Network> {
    match node {
        Node::String(text) => lowering::network(text, description),
        _ => Err(lowering::not_a_cidr_string(description)),
    }
}

/// Parses a destination of an MXC rule, as `_parse_peer` does.
fn parse_peer(
    document: &Document,
    id: NodeId,
    description: &str,
) -> Result<(Family, Vec<(u128, u128)>)> {
    let peer = object(document, id, description)?;
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
    let protocol = self::protocol(rule.document, protocol, description)?;
    let selectors = lowering::port_selectors(
        protocol,
        port.map(|port| port_value(rule.node(port))),
        end_port.map(|end_port| port_value(rule.node(end_port))),
        description,
    )?;
    Ok(lowering::combine(destinations, Some(selectors)))
}

/// Parses a port selector of an MXC rule, as `_parse_mxc_port` does.
fn parse_mxc_port(document: &Document, id: NodeId, description: &str) -> Result<Vec<Selector>> {
    let port = object(document, id, description)?;
    port.check_fields(PORT_FIELDS, description)?;
    let protocol = match port.get("protocol") {
        Some(protocol) => self::protocol(document, protocol, description)?,
        None => RuleProtocol::Any,
    };
    lowering::port_selectors(
        protocol,
        port.get("port").map(|value| port_value(port.node(value))),
        port.get("endPort")
            .map(|value| port_value(port.node(value))),
        description,
    )
}

/// Parses an MXC rule, as `_parse_mxc_rule` does.
fn parse_mxc_rule(rule: &Object<'_>, description: &str) -> Result<Vec<Rule>> {
    rule.check_fields(MXC_RULE_FIELDS, description)?;
    let mut destinations = None;
    if let Some(to) = rule.get("to") {
        let description = format!("{description}.to");
        let peers = array(rule.document, to, &description)?;
        if peers.is_empty() {
            return Err(invalid(format!(
                "{description} must contain at least one destination"
            )));
        }
        let mut parsed = Vec::with_capacity(peers.len());
        for (index, &peer) in peers.iter().enumerate() {
            parsed.push(parse_peer(
                rule.document,
                peer,
                &format!("{description}[{index}]"),
            )?);
        }
        // A native rule names one family, so the peers of each family form their own
        // destination set.
        destinations = Some(lowering::group_destinations(&parsed));
    }
    let mut selectors = None;
    if let Some(ports) = rule.get("ports") {
        let description = format!("{description}.ports");
        let ports = array(rule.document, ports, &description)?;
        if ports.is_empty() {
            return Err(invalid(format!(
                "{description} must contain at least one selector"
            )));
        }
        let mut parsed = Vec::new();
        for (index, &port) in ports.iter().enumerate() {
            parsed.extend(parse_mxc_port(
                rule.document,
                port,
                &format!("{description}[{index}]"),
            )?);
        }
        selectors = Some(parsed);
    }
    Ok(lowering::combine(destinations, selectors))
}
