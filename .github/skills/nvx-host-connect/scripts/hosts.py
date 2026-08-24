#!/usr/bin/env python3
"""Resolve developer-local NVX SSH host profiles."""

from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import cast

INVENTORY_FILENAME = ".nvx-hosts.json"
INVENTORY_VERSION = 1
PROFILE_NAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
BACKEND_PLATFORMS = {
    "kvm": "linux-kvm",
    "mshv": "linux-mshv",
    "whp": "windows-whp",
}
PROFILE_KEYS = frozenset({"ssh_target", "backend", "remote_repo", "notes"})


class InventoryError(ValueError):
    """Raised when a host inventory is malformed."""


@dataclass(frozen=True)
class HostProfile:
    name: str
    ssh_target: str
    backend: str
    remote_repo: str | None
    notes: str | None

    def to_json(self) -> dict[str, object]:
        return {
            "name": self.name,
            "ssh_target": self.ssh_target,
            "backend": self.backend,
            "platform": BACKEND_PLATFORMS[self.backend],
            "remote_repo": self.remote_repo,
            "notes": self.notes,
        }


def _mapping(value: object, context: str) -> dict[str, object]:
    if not isinstance(value, dict) or not all(isinstance(key, str) for key in value):
        raise InventoryError(f"{context} must be a JSON object with string keys")
    return cast(dict[str, object], value)


def _required_string(mapping: dict[str, object], key: str, context: str) -> str:
    value = mapping.get(key)
    if not isinstance(value, str) or not value.strip():
        raise InventoryError(f"{context}.{key} must be a non-empty string")
    return value


def _optional_string(mapping: dict[str, object], key: str, context: str) -> str | None:
    value = mapping.get(key)
    if value is None:
        return None
    if not isinstance(value, str) or not value.strip():
        raise InventoryError(f"{context}.{key} must be a non-empty string when set")
    return value


def _parse_profile(name: str, value: object) -> HostProfile:
    context = f"hosts.{name}"
    if not PROFILE_NAME.fullmatch(name):
        raise InventoryError(
            f"host profile name {name!r} must contain only letters, digits, '.', '_', or '-'"
        )
    profile = _mapping(value, context)
    unknown_keys = sorted(profile.keys() - PROFILE_KEYS)
    if unknown_keys:
        raise InventoryError(
            f"{context} has unsupported fields: {', '.join(unknown_keys)}"
        )

    ssh_target = _required_string(profile, "ssh_target", context)
    if ssh_target.startswith("-") or any(
        character.isspace() for character in ssh_target
    ):
        raise InventoryError(
            f"{context}.ssh_target must be one SSH destination or configured alias"
        )
    backend = _required_string(profile, "backend", context)
    if backend not in BACKEND_PLATFORMS:
        choices = ", ".join(BACKEND_PLATFORMS)
        raise InventoryError(f"{context}.backend must be one of: {choices}")

    return HostProfile(
        name=name,
        ssh_target=ssh_target,
        backend=backend,
        remote_repo=_optional_string(profile, "remote_repo", context),
        notes=_optional_string(profile, "notes", context),
    )


def load_inventory(path: Path) -> list[HostProfile] | None:
    if not path.is_file():
        return None
    try:
        raw: object = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise InventoryError(f"cannot read {path}: {error}") from error

    inventory = _mapping(raw, "inventory")
    unknown_keys = sorted(inventory.keys() - {"version", "hosts"})
    if unknown_keys:
        raise InventoryError(
            f"inventory has unsupported fields: {', '.join(unknown_keys)}"
        )
    version = inventory.get("version")
    if version != INVENTORY_VERSION or isinstance(version, bool):
        raise InventoryError(f"inventory.version must be {INVENTORY_VERSION}")

    hosts = _mapping(inventory.get("hosts"), "inventory.hosts")
    return [_parse_profile(name, hosts[name]) for name in sorted(hosts)]


def _default_inventory() -> Path:
    repository_root = Path(__file__).resolve().parents[4]
    return repository_root / INVENTORY_FILENAME


def _write_json(payload: dict[str, object]) -> None:
    json.dump(payload, sys.stdout, indent=2)
    sys.stdout.write("\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("profile", nargs="?", help="host profile to resolve")
    parser.add_argument(
        "--file",
        type=Path,
        default=_default_inventory(),
        help=f"inventory path (default: repository {INVENTORY_FILENAME})",
    )
    args = parser.parse_args()
    inventory_path = args.file.resolve()

    try:
        profiles = load_inventory(inventory_path)
    except InventoryError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    if not profiles:
        _write_json(
            {
                "remote_enabled": False,
                "inventory": str(inventory_path),
                "reason": (
                    f"{INVENTORY_FILENAME} is missing or has no hosts; "
                    "only local NVX development is enabled"
                ),
                "hosts": [],
            }
        )
        return 0

    if args.profile is None:
        _write_json(
            {
                "remote_enabled": True,
                "inventory": str(inventory_path),
                "hosts": [profile.to_json() for profile in profiles],
            }
        )
        return 0

    selected = next(
        (profile for profile in profiles if profile.name == args.profile), None
    )
    if selected is None:
        available = ", ".join(profile.name for profile in profiles)
        print(
            f"error: unknown host profile {args.profile!r}; available: {available}",
            file=sys.stderr,
        )
        return 1

    _write_json(
        {
            "remote_enabled": True,
            "inventory": str(inventory_path),
            "host": selected.to_json(),
        }
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
