"""Compile structured IPv4 and IPv6 egress policies to the native OpenVMM rule
grammar."""

from __future__ import annotations

import ipaddress
import json
from collections import Counter
from collections.abc import Iterable, Iterator
from dataclasses import dataclass
from pathlib import Path
from typing import cast

from .common import ScriptError, strict_json_object

MAX_RULES_PER_ACTION = 256
MAX_POLICY_FILE_SIZE = 1024 * 1024
_MAX_JSON_INTEGER_DIGITS = 64
_ROOT_FIELDS = frozenset(("allow", "deny"))
_MXC_RULE_FIELDS = frozenset(("to", "ports"))
_PEER_FIELDS = frozenset(("cidr", "except"))
_PORT_FIELDS = frozenset(("protocol", "port", "endPort"))
_LEGACY_RULE_FIELDS = frozenset(("cidr", "except", "protocol", "port", "endPort"))
_RULE_PROTOCOLS = ("tcp", "udp", "icmp", "any")
# Protocols that a native rule can select on every port, and those with ports.
_NATIVE_PROTOCOLS = ("icmp", "tcp", "udp")
_PORT_PROTOCOLS = ("tcp", "udp")
_IP_VERSIONS = (4, 6)
_AddressInterval = tuple[int, int]
_AddressIntervals = tuple[_AddressInterval, ...]
_Network = ipaddress.IPv4Network | ipaddress.IPv6Network
_PortSelector = tuple[str | None, int | None, int | None]
# Each IP version with its addresses; a rule without destinations matches every
# address of both families.
_Destinations = tuple[tuple[int, _AddressIntervals], ...]
_ALL_ADDRESSES: _Destinations = (
    (4, ((0, (1 << 32) - 1),)),
    (6, ((0, (1 << 128) - 1),)),
)
# An inclusive range of destination ports.
_PortRange = tuple[int, int]
# A native rule: a network, and optionally a protocol and its port range.
_NativeRule = tuple[_Network, str | None, _PortRange | None]


@dataclass(frozen=True)
class CompiledEgressPolicy:
    allow: tuple[str, ...]
    deny: tuple[str, ...]


@dataclass(frozen=True)
class _Rule:
    # IP version of the addresses, each an interval of integers in its family.
    version: int
    addresses: _AddressIntervals
    # None matches every protocol.
    protocol: str | None
    # None matches every port of the protocol.
    start_port: int | None
    end_port: int | None


def _object(value: object, description: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ScriptError(f"{description} must be an object")
    mapping = cast(dict[object, object], value)
    if not all(isinstance(key, str) for key in mapping):
        raise ScriptError(f"{description} fields must be strings")
    return cast(dict[str, object], value)


def _array(value: object, description: str) -> list[object]:
    if not isinstance(value, list):
        raise ScriptError(f"{description} must be an array")
    return cast(list[object], value)


def _network(value: object, description: str) -> _Network:
    if not isinstance(value, str):
        raise ScriptError(f"{description} must be an IPv4 or IPv6 CIDR string")
    try:
        parsed = ipaddress.ip_network(value, strict=False)
    except ValueError as error:
        raise ScriptError(
            f"{description} is not a valid IPv4 or IPv6 CIDR: {value}"
        ) from error
    if isinstance(parsed, ipaddress.IPv6Network) and parsed.network_address.scope_id:
        raise ScriptError(f"{description} must not name an IPv6 scope: {value}")
    return parsed


def _contains(parent: _Network, child: _Network) -> bool:
    if isinstance(parent, ipaddress.IPv4Network) and isinstance(
        child, ipaddress.IPv4Network
    ):
        return child.subnet_of(parent)
    if isinstance(parent, ipaddress.IPv6Network) and isinstance(
        child, ipaddress.IPv6Network
    ):
        return child.subnet_of(parent)
    return False


def _summarize(start: int, end: int, version: int) -> Iterator[_Network]:
    if version == 4:
        return ipaddress.summarize_address_range(
            ipaddress.IPv4Address(start), ipaddress.IPv4Address(end)
        )
    return ipaddress.summarize_address_range(
        ipaddress.IPv6Address(start), ipaddress.IPv6Address(end)
    )


def _port(value: object, description: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ScriptError(f"{description} must be an integer")
    if not 1 <= value <= 65535:
        raise ScriptError(f"{description} must be between 1 and 65535")
    return value


def _bounded_json_integer(value: str) -> int:
    if len(value.removeprefix("-")) > _MAX_JSON_INTEGER_DIGITS:
        raise ScriptError(
            f"JSON integer exceeds {_MAX_JSON_INTEGER_DIGITS}-digit limit"
        )
    return int(value)


def _merge_intervals(intervals: Iterable[_AddressInterval]) -> _AddressIntervals:
    merged: list[_AddressInterval] = []
    for start, end in sorted(intervals):
        if merged and start <= merged[-1][1] + 1:
            previous_start, previous_end = merged[-1]
            merged[-1] = (previous_start, max(previous_end, end))
        else:
            merged.append((start, end))
    return tuple(merged)


def _subtract_intervals(
    sources: _AddressIntervals,
    exclusions: _AddressIntervals,
) -> _AddressIntervals:
    remaining: list[_AddressInterval] = []
    exclusion_index = 0
    for source_start, source_end in sources:
        while (
            exclusion_index < len(exclusions)
            and exclusions[exclusion_index][1] < source_start
        ):
            exclusion_index += 1

        cursor = source_start
        current_index = exclusion_index
        while (
            current_index < len(exclusions)
            and exclusions[current_index][0] <= source_end
        ):
            excluded_start, excluded_end = exclusions[current_index]
            if cursor < excluded_start:
                remaining.append((cursor, excluded_start - 1))
            cursor = max(cursor, excluded_end + 1)
            if cursor > source_end:
                break
            current_index += 1
        if cursor <= source_end:
            remaining.append((cursor, source_end))
    return tuple(remaining)


def _subtract_exclusions(
    parent: _Network,
    exclusions: list[object],
    description: str,
) -> _AddressIntervals:
    parsed: list[_AddressInterval] = []
    for index, value in enumerate(exclusions):
        exclusion = _network(value, f"{description}.except[{index}]")
        if not _contains(parent, exclusion):
            raise ScriptError(
                f"{description}.except[{index}] must be contained in {parent}"
            )
        parsed.append(
            (int(exclusion.network_address), int(exclusion.broadcast_address))
        )

    parent_interval = (
        int(parent.network_address),
        int(parent.broadcast_address),
    )
    return _subtract_intervals((parent_interval,), _merge_intervals(parsed))


def _parse_peer(value: object, description: str) -> tuple[int, _AddressIntervals]:
    """Return a destination's IP version and the addresses that it selects."""
    peer = _object(value, description)
    unknown = sorted(set(peer) - _PEER_FIELDS)
    if unknown:
        raise ScriptError(f"{description} has unknown field '{unknown[0]}'")
    if "cidr" not in peer:
        raise ScriptError(f"{description}.cidr is required")
    parent = _network(peer["cidr"], f"{description}.cidr")
    exclusions = _array(peer.get("except", []), f"{description}.except")
    return parent.version, _subtract_exclusions(parent, exclusions, description)


def _parse_legacy_rule(rule: dict[str, object], description: str) -> tuple[_Rule, ...]:
    unknown = sorted(set(rule) - _LEGACY_RULE_FIELDS)
    if unknown:
        raise ScriptError(f"{description} has unknown field '{unknown[0]}'")
    if "cidr" not in rule:
        raise ScriptError(f"{description}.cidr is required")
    version, addresses = _parse_peer(
        {field: rule[field] for field in ("cidr", "except") if field in rule},
        description,
    )
    if "endPort" in rule and "port" not in rule:
        raise ScriptError(f"{description}.endPort requires port")
    if "protocol" not in rule:
        if "port" in rule:
            raise ScriptError(f"{description}.port requires protocol")
        return (_Rule(version, addresses, None, None, None),)
    protocol = rule["protocol"]
    if not isinstance(protocol, str) or protocol not in _RULE_PROTOCOLS:
        raise ScriptError(f"{description}.protocol must be tcp, udp, icmp, or any")
    if "port" not in rule:
        return (
            _Rule(
                version,
                addresses,
                None if protocol == "any" else protocol,
                None,
                None,
            ),
        )
    if protocol == "icmp":
        raise ScriptError(f"{description}.port is not supported with icmp")
    start = _port(rule["port"], f"{description}.port")
    end = _port(rule.get("endPort", start), f"{description}.endPort")
    if end < start:
        raise ScriptError(f"{description}.endPort {end} cannot be below port {start}")
    protocols = _PORT_PROTOCOLS if protocol == "any" else (protocol,)
    return tuple(
        _Rule(version, addresses, selected, start, end) for selected in protocols
    )


def _parse_mxc_port(value: object, description: str) -> tuple[_PortSelector, ...]:
    port = _object(value, description)
    unknown = sorted(set(port) - _PORT_FIELDS)
    if unknown:
        raise ScriptError(f"{description} has unknown field '{unknown[0]}'")
    protocol = port.get("protocol", "any")
    if not isinstance(protocol, str) or protocol not in _RULE_PROTOCOLS:
        raise ScriptError(f"{description}.protocol must be tcp, udp, icmp, or any")
    if "port" not in port:
        if "endPort" in port:
            raise ScriptError(f"{description}.endPort requires port")
        return (
            (
                None if protocol == "any" else protocol,
                None,
                None,
            ),
        )
    if protocol == "icmp":
        raise ScriptError(f"{description}.port is not supported with icmp")
    start = _port(port["port"], f"{description}.port")
    end = _port(port.get("endPort", start), f"{description}.endPort")
    if end < start:
        raise ScriptError(f"{description}.endPort {end} cannot be below port {start}")
    protocols = _PORT_PROTOCOLS if protocol == "any" else (protocol,)
    return tuple((selected, start, end) for selected in protocols)


def _parse_mxc_rule(
    rule: dict[str, object],
    description: str,
) -> tuple[_Rule, ...]:
    unknown = sorted(set(rule) - _MXC_RULE_FIELDS)
    if unknown:
        raise ScriptError(f"{description} has unknown field '{unknown[0]}'")

    destinations = _ALL_ADDRESSES
    if "to" in rule:
        peers = _array(rule["to"], f"{description}.to")
        if not peers:
            raise ScriptError(f"{description}.to must contain at least one destination")
        parsed = [
            _parse_peer(peer, f"{description}.to[{index}]")
            for index, peer in enumerate(peers)
        ]
        # A native rule names one family, so the peers of each family form
        # their own destination set.
        destinations = tuple(
            (
                version,
                _merge_intervals(
                    interval
                    for peer_version, intervals in parsed
                    if peer_version == version
                    for interval in intervals
                ),
            )
            for version in _IP_VERSIONS
            if any(peer_version == version for peer_version, _ in parsed)
        )

    selectors: list[_PortSelector] = [(None, None, None)]
    if "ports" in rule:
        ports = _array(rule["ports"], f"{description}.ports")
        if not ports:
            raise ScriptError(f"{description}.ports must contain at least one selector")
        selectors = [
            selector
            for index, port in enumerate(ports)
            for selector in _parse_mxc_port(port, f"{description}.ports[{index}]")
        ]
    return tuple(
        _Rule(version, addresses, protocol, start_port, end_port)
        for version, addresses in destinations
        for protocol, start_port, end_port in selectors
    )


def _parse_rule(value: object, description: str) -> tuple[_Rule, ...]:
    rule = _object(value, description)
    legacy = set(rule) & _LEGACY_RULE_FIELDS
    mxc = set(rule) & _MXC_RULE_FIELDS
    if legacy:
        if mxc:
            raise ScriptError(
                f"{description} cannot mix MXC to/ports fields with legacy flat fields"
            )
        return _parse_legacy_rule(rule, description)
    return _parse_mxc_rule(rule, description)


def _intervals_to_networks(
    intervals: _AddressIntervals,
    version: int,
    maximum: int,
    category: str,
) -> tuple[_Network, ...]:
    networks: list[_Network] = []
    for start, end in intervals:
        summarized = _summarize(start, end, version)
        for network in summarized:
            if len(networks) >= maximum:
                raise ScriptError(
                    f"{category} emits at most {MAX_RULES_PER_ACTION} native rules"
                )
            networks.append(network)
    return tuple(networks)


def _protocol_intervals_to_networks(
    protocol_intervals: _AddressIntervals,
    covered: _AddressIntervals,
    version: int,
    maximum: int,
    category: str,
) -> tuple[_Network, ...]:
    # Rules over `covered` already match this selector, so widening prefixes
    # into it keeps the output compact without changing what matches.
    combined = _merge_intervals((*protocol_intervals, *covered))
    networks: list[_Network] = []
    for start, end in combined:
        summarized = _summarize(start, end, version)
        for network in summarized:
            network_interval = (
                int(network.network_address),
                int(network.broadcast_address),
            )
            if not _subtract_intervals((network_interval,), covered):
                continue
            if len(networks) >= maximum:
                raise ScriptError(
                    f"{category} emits at most {MAX_RULES_PER_ACTION} native rules"
                )
            networks.append(network)
    return tuple(networks)


def _lower_protocol_rules(
    rules: list[_Rule],
    protocol: str,
    version: int,
    category: str,
    covered: _AddressIntervals,
    remaining_budget: int,
) -> list[_NativeRule]:
    events: dict[int, list[tuple[int, _AddressIntervals]]] = {}
    for rule in rules:
        if (
            rule.protocol != protocol
            or rule.start_port is None
            or rule.end_port is None
            or not rule.addresses
        ):
            continue
        uncovered_addresses = _subtract_intervals(rule.addresses, covered)
        if not uncovered_addresses:
            continue
        events.setdefault(rule.start_port, []).append((1, rule.addresses))
        events.setdefault(rule.end_port + 1, []).append((-1, rule.addresses))

    active: Counter[_AddressIntervals] = Counter()
    port_ranges: dict[_Network, list[_PortRange]] = {}
    # Networks whose last range ends just before the current port segment.
    open_networks: list[_Network] = []
    rule_count = 0
    previous_port: int | None = None
    for port in sorted(events):
        if previous_port is not None and not active:
            open_networks = []
        if previous_port is not None and previous_port < port and active:
            addresses = _merge_intervals(
                interval for intervals in active for interval in intervals
            )
            available = _merge_intervals((*addresses, *covered))
            kept: list[_Network] = []
            for network in open_networks:
                interval = (
                    int(network.network_address),
                    int(network.broadcast_address),
                )
                if not _subtract_intervals((interval,), available):
                    ranges = port_ranges[network]
                    ranges[-1] = (ranges[-1][0], port - 1)
                    kept.append(network)
            kept_cover = _merge_intervals(
                (
                    *covered,
                    *(
                        (
                            int(network.network_address),
                            int(network.broadcast_address),
                        )
                        for network in kept
                    ),
                )
            )
            networks = _protocol_intervals_to_networks(
                addresses,
                kept_cover,
                version,
                remaining_budget - rule_count,
                category,
            )
            for network in networks:
                if rule_count >= remaining_budget:
                    raise ScriptError(
                        f"{category} emits at most {MAX_RULES_PER_ACTION} native rules"
                    )
                port_ranges.setdefault(network, []).append((previous_port, port - 1))
                rule_count += 1
                kept.append(network)
            open_networks = kept
        for direction, addresses in events[port]:
            active[addresses] += direction
            if active[addresses] == 0:
                del active[addresses]
        previous_port = port
    return [
        (network, protocol, port_range)
        for network, ranges in port_ranges.items()
        for port_range in ranges
    ]


def _native_rule(
    network: _Network, protocol: str | None, ports: _PortRange | None
) -> str:
    if protocol is None:
        return str(network)
    if ports is None:
        return f"{network}:{protocol}"
    start, end = ports
    if start == end:
        return f"{network}:{protocol}:{start}"
    return f"{network}:{protocol}:{start}-{end}"


def _compile_family(
    rules: list[_Rule],
    version: int,
    category: str,
    budget: int,
) -> list[_NativeRule]:
    address_only = _merge_intervals(
        interval
        for rule in rules
        if rule.protocol is None
        for interval in rule.addresses
    )
    address_only_networks = _intervals_to_networks(
        address_only,
        version,
        budget,
        category,
    )
    lowered: list[_NativeRule] = [
        (network, None, None) for network in address_only_networks
    ]
    # Addresses that already match every port of each protocol.
    covered = dict.fromkeys(_NATIVE_PROTOCOLS, address_only)
    for protocol in _NATIVE_PROTOCOLS:
        protocol_wide = _merge_intervals(
            interval
            for rule in rules
            if rule.protocol == protocol and rule.start_port is None
            for interval in rule.addresses
        )
        if not protocol_wide:
            continue
        lowered.extend(
            (network, protocol, None)
            for network in _protocol_intervals_to_networks(
                protocol_wide,
                address_only,
                version,
                budget - len(lowered),
                category,
            )
        )
        covered[protocol] = _merge_intervals((*address_only, *protocol_wide))
    for protocol in _PORT_PROTOCOLS:
        lowered.extend(
            _lower_protocol_rules(
                rules,
                protocol,
                version,
                category,
                covered[protocol],
                budget - len(lowered),
            )
        )
    return lowered


def _compile_category(value: object, category: str) -> tuple[str, ...]:
    values = _array(value, category)
    rules = [
        rule
        for index, item in enumerate(values)
        for rule in _parse_rule(item, f"{category}[{index}]")
    ]
    # A native rule matches only its own address family, so each family is
    # lowered on its own, within the action's shared rule limit.
    lowered: list[_NativeRule] = []
    for version in _IP_VERSIONS:
        family = [rule for rule in rules if rule.version == version]
        if family:
            lowered.extend(
                _compile_family(
                    family,
                    version,
                    category,
                    MAX_RULES_PER_ACTION - len(lowered),
                )
            )
    lowered.sort(
        key=lambda item: (
            item[0].version,
            int(item[0].network_address),
            item[0].prefixlen,
            "" if item[1] is None else item[1],
            (0, 0) if item[2] is None else item[2],
        )
    )
    return tuple(
        _native_rule(network, protocol, ports) for network, protocol, ports in lowered
    )


def compile_policy(value: object) -> CompiledEgressPolicy:
    root = _object(value, "egress policy")
    unknown = sorted(set(root) - _ROOT_FIELDS)
    if unknown:
        raise ScriptError(f"egress policy has unknown field '{unknown[0]}'")
    return CompiledEgressPolicy(
        allow=_compile_category(root.get("allow", []), "allow"),
        deny=_compile_category(root.get("deny", []), "deny"),
    )


def compile_policy_file(path: Path) -> CompiledEgressPolicy:
    try:
        with path.open("rb") as stream:
            data = stream.read(MAX_POLICY_FILE_SIZE + 1)
    except OSError as error:
        raise ScriptError(f"failed to read egress policy file: {path}") from error
    if len(data) > MAX_POLICY_FILE_SIZE:
        raise ScriptError(
            f"egress policy file exceeds {MAX_POLICY_FILE_SIZE}-byte limit: {path}"
        )
    try:
        value = json.loads(
            data.decode("utf-8"),
            object_pairs_hook=strict_json_object,
            parse_int=_bounded_json_integer,
        )
    except (UnicodeDecodeError, ValueError) as error:
        raise ScriptError(f"failed to read egress policy file: {path}") from error
    return compile_policy(value)
