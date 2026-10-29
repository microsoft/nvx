"""Block the Windows runner account from Azure instance metadata.

Jobs run as Network Service. The block counts only when the firewall
services run, every profile enforces local rules on all interfaces, and the
effective rule in ActiveStore, not merely a stored one, is enforced and
applies to Network Service without any filter that could narrow it.
"""

from typing import cast

from . import windows_system
from .common import BootstrapError, JsonObject

RULE_NAME = "NvxRunnerDenyMetadata"
METADATA_ADDRESS = "169.254.169.254"
# Network Service may connect; the rule blocks it.
PRINCIPAL = "D:(A;;CC;;;S-1-5-20)"
FAILURE = "The runner account is not blocked from Azure instance metadata."
PROFILES = ("Domain", "Private", "Public")
# Rule enforcement statuses that the firewall provider reports. A rule for
# every profile reports ProfileInactive for the profiles of no current network
# alongside Enforced for the active one, and applies to the others as soon as
# a network uses them; any other status means that it is not enforced.
ENFORCED = 1
PROFILE_INACTIVE = 5


def _object(value: object) -> JsonObject:
    if not isinstance(value, dict):
        raise BootstrapError(f"{FAILURE} The firewall facts are malformed.")
    return cast(JsonObject, value)


def _items(value: object) -> list[object]:
    if not isinstance(value, list):
        raise BootstrapError(f"{FAILURE} The firewall facts are malformed.")
    return cast(list[object], value)


def check_environment(facts: JsonObject) -> None:
    """Require running firewall services and enforcing effective profiles."""
    services = _object(facts.get("services"))
    for name in ("BFE", "MpsSvc"):
        if services.get(name) != "Running":
            raise BootstrapError(
                f"{FAILURE} Windows Firewall service {name} is not running."
            )
    profiles = [_object(profile) for profile in _items(facts.get("profiles"))]
    if sorted(str(profile.get("name")) for profile in profiles) != list(PROFILES):
        raise BootstrapError(f"{FAILURE} Effective firewall profiles are unavailable.")
    for profile in profiles:
        disabled = [
            alias
            for alias in _items(profile.get("disabledInterfaceAliases"))
            if alias and alias != "NotConfigured"
        ]
        if (
            profile.get("enabled") != "True"
            or profile.get("allowLocalFirewallRules") not in ("True", "NotConfigured")
            or disabled
        ):
            raise BootstrapError(
                f"{FAILURE} Profile {profile.get('name')} does not enforce local "
                "rules on all interfaces."
            )


def check_rule(facts: JsonObject) -> None:
    """Require one enforced, unrestricted effective rule for Network Service."""
    effective = _items(facts.get("effective"))
    if len(effective) != 1:
        raise BootstrapError(
            f"{FAILURE} No unique effective metadata rule is available."
        )
    rule = _object(effective[0])
    enforcement = _items(rule.get("enforcement"))
    statuses = [
        status
        for status in enforcement
        if isinstance(status, int) and not isinstance(status, bool)
    ]
    if (
        rule.get("primaryStatus") != "OK"
        or len(statuses) != len(enforcement)
        or ENFORCED not in statuses
        or any(status not in (ENFORCED, PROFILE_INACTIVE) for status in statuses)
    ):
        raise BootstrapError(
            f"{FAILURE} The effective metadata rule is not enforced (status "
            f"{rule.get('primaryStatus')}, enforcement {enforcement})."
        )
    restrictions = _object(rule.get("restrictions"))
    filters = _object(rule.get("filters"))
    if (
        rule.get("direction") != "Outbound"
        or rule.get("action") != "Block"
        or rule.get("enabled") != "True"
        or rule.get("profile") != "Any"
        or rule.get("platform") != ""
        or any(value != "" for value in restrictions.values())
        or rule.get("package") not in ("", "Any")
        or rule.get("protocol") not in ("Any", "256")
        or rule.get("authentication") != "NotRequired"
        or rule.get("encryption") != "NotRequired"
        or any(value != "Any" for value in filters.values())
        or len(filters) != 11
        or rule.get("remoteAddresses") != [METADATA_ADDRESS]
        or rule.get("localUser") != facts.get("principal")
    ):
        raise BootstrapError(FAILURE)


def inspect() -> JsonObject:
    facts = windows_system.adapter(
        "firewall.ps1", "Inspect", {"name": RULE_NAME, "principal": PRINCIPAL}
    )
    return _object(facts)


def ensure_metadata_block() -> None:
    """Install the block when no stored rule exists, then verify what is enforced."""
    facts = inspect()
    check_environment(facts)
    if not facts.get("persistent"):
        windows_system.adapter(
            "firewall.ps1",
            "Create",
            {"name": RULE_NAME, "address": METADATA_ADDRESS, "principal": PRINCIPAL},
        )
        facts = inspect()
        check_environment(facts)
    check_rule(facts)
