"""Validated host-side launch contract for the experimental sandbox profile."""

from __future__ import annotations

import os
import re
from dataclasses import dataclass
from pathlib import Path
from uuid import UUID

from .common import ScriptError, require_file

LAYER_ROLES = ("distro", "runtime", "custom")
BLOCK_MMIO_BASES = {
    "distro": 0xD000_3000,
    "runtime": 0xD000_4000,
    "custom": 0xD000_5000,
    "scratch": 0xD000_6000,
}
KERNEL_COMMAND_LINE_MAX_SIZE = 2048
OPENVMM_COMMAND_LINE_RESERVE = 1024
SANDBOX_COMMAND_LINE_MAX_SIZE = (
    KERNEL_COMMAND_LINE_MAX_SIZE - OPENVMM_COMMAND_LINE_RESERVE
)
_HOSTNAME = re.compile(r"(?=^.{1,63}$)(?!-)[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$")
DEFAULT_WORKLOAD_IDENTITY = (65534, 65534)


def parse_workload_identity(value: str) -> tuple[int, int]:
    fields = value.split(":")
    if len(fields) != 2:
        raise ScriptError("--workload-user must be UID:GID")
    try:
        uid, gid = (int(field, 10) for field in fields)
    except ValueError as error:
        raise ScriptError("--workload-user must contain numeric UID and GID") from error
    if not 1 <= uid <= 0xFFFF_FFFF:
        raise ScriptError("--workload-user UID must be between 1 and 4294967295")
    if not 1 <= gid <= 0xFFFF_FFFF:
        raise ScriptError("--workload-user GID must be between 1 and 4294967295")
    return uid, gid


@dataclass(frozen=True)
class SandboxLayer:
    role: str
    path: Path
    uuid: str

    @classmethod
    def parse(cls, value: str) -> SandboxLayer:
        fields = value.split(",")
        if len(fields) != 3:
            raise ScriptError("--layer must be ROLE,PATH,EROFS_UUID")
        role, raw_path, raw_uuid = fields
        if role not in LAYER_ROLES:
            raise ScriptError(
                f"unsupported layer role {role!r}; choose {', '.join(LAYER_ROLES)}"
            )
        if not raw_path:
            raise ScriptError(f"{role} layer path is empty")
        try:
            uuid = str(UUID(raw_uuid))
        except ValueError as error:
            raise ScriptError(f"{role} layer UUID is invalid: {raw_uuid!r}") from error
        return cls(role=role, path=Path(raw_path), uuid=uuid)


@dataclass(frozen=True)
class SandboxLaunch:
    layers: tuple[SandboxLayer, ...]
    scratch: Path
    entrypoint: str = "/bin/sh"
    args: tuple[str, ...] = ()
    hostname: str = "nvx-sandbox"
    workload_identity: tuple[int, int] = DEFAULT_WORKLOAD_IDENTITY
    memory_max: int | None = None
    pids_max: int | None = None

    def __post_init__(self) -> None:
        if not 1 <= len(self.layers) <= len(LAYER_ROLES):
            raise ScriptError("a sandbox requires one to three read-only layers")
        roles = [layer.role for layer in self.layers]
        duplicates = sorted({role for role in roles if roles.count(role) > 1})
        if duplicates:
            raise ScriptError(
                f"duplicate sandbox layer role(s): {', '.join(duplicates)}"
            )
        if not self.entrypoint.startswith("/") or any(
            character.isspace() for character in self.entrypoint
        ):
            raise ScriptError(
                "sandbox entrypoint must be an absolute path without spaces"
            )
        for argument in self.args:
            if not argument or any(character.isspace() for character in argument):
                raise ScriptError(
                    "sandbox arguments must be nonempty and contain no spaces"
                )
        if _HOSTNAME.fullmatch(self.hostname) is None:
            raise ScriptError(
                "sandbox hostname must be a lowercase RFC 1123 label up to 63 characters"
            )
        uid, gid = self.workload_identity
        if not 1 <= uid <= 0xFFFF_FFFF or not 1 <= gid <= 0xFFFF_FFFF:
            raise ScriptError("sandbox workload identity must be a non-root UID:GID")
        for name, value in (
            ("memory-max", self.memory_max),
            ("pids-max", self.pids_max),
        ):
            if value is not None and value <= 0:
                raise ScriptError(f"--{name} must be positive")

    def validated(self) -> SandboxLaunch:
        for layer in self.layers:
            require_file(layer.path, f"{layer.role} EROFS layer")
            _reject_disk_path(layer.path)
        require_file(self.scratch, "ext4 scratch image")
        _reject_disk_path(self.scratch)
        return self

    def ordered_layers(self) -> tuple[SandboxLayer, ...]:
        by_role = {layer.role: layer for layer in self.layers}
        return tuple(by_role[role] for role in LAYER_ROLES if role in by_role)

    def openvmm_arguments(self) -> list[str]:
        arguments = ["--machine", "microvm"]
        for layer in self.ordered_layers():
            arguments.extend(
                (
                    "--microvm-sandbox-block",
                    f"{layer.role}:file:{os.fspath(layer.path)},ro",
                )
            )
        arguments.extend(
            (
                "--microvm-sandbox-block",
                f"scratch:file:{os.fspath(self.scratch)}",
                "--microvm-workload-identity",
                f"{self.workload_identity[0]}:{self.workload_identity[1]}",
            )
        )
        return arguments

    def kernel_command_line(self, user_command_line: str = "") -> str:
        if "\0" in user_command_line:
            raise ScriptError("kernel command line contains an embedded NUL")
        for token in user_command_line.split():
            if token.startswith("nvx_"):
                raise ScriptError(
                    f"{token.split('=', 1)[0]} is owned by the sandbox profile"
                )
        tokens = [user_command_line.strip(), "nvx_sandbox=1"]
        for layer in self.ordered_layers():
            tokens.append(
                "nvx_layer="
                f"{layer.role},0x{BLOCK_MMIO_BASES[layer.role]:x},{layer.uuid}"
            )
        tokens.extend(
            (
                f"nvx_scratch=0x{BLOCK_MMIO_BASES['scratch']:x},ext4",
                f"nvx_entrypoint={self.entrypoint}",
                f"nvx_hostname={self.hostname}",
            )
        )
        tokens.extend(f"nvx_arg={argument}" for argument in self.args)
        if self.memory_max is not None:
            tokens.append(f"nvx_memory_max={self.memory_max}")
        if self.pids_max is not None:
            tokens.append(f"nvx_pids_max={self.pids_max + 1}")
        command_line = " ".join(token for token in tokens if token)
        if len(command_line.encode("utf-8")) + 1 > SANDBOX_COMMAND_LINE_MAX_SIZE:
            raise ScriptError(
                "sandbox kernel command line exceeds its 1024-byte x86 budget"
            )
        return command_line


def _reject_disk_path(path: Path) -> None:
    value = os.fspath(path)
    if any(character in value for character in ",;"):
        raise ScriptError(
            f"disk paths containing commas or semicolons are unsupported: {value}"
        )
