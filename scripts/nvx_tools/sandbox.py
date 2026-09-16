"""Validated host-side launch contract for the experimental sandbox profile."""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

from .common import ScriptError, require_file

LAYER_ROLES = ("distro", "runtime", "custom")
KERNEL_COMMAND_LINE_MAX_SIZE = 2048
OPENVMM_COMMAND_LINE_RESERVE = 1024
SANDBOX_COMMAND_LINE_MAX_SIZE = (
    KERNEL_COMMAND_LINE_MAX_SIZE - OPENVMM_COMMAND_LINE_RESERVE
)
CONTROL_AUTH_TIMEOUT_MS = 5_000
MINIMUM_CONTROL_AUTH_FD = 3  # POSIX descriptors 0-2 are standard streams.
HOST_OWNED_KERNEL_PARAMETERS = frozenset(
    {
        "console",
        "earlycon",
        "init",
        "panic",
        "rdinit",
        "reboot",
        "virtfs_dir",
        "virtfs_mode",
        "virtfs_tag",
        "virtnet_dns",
        "virtnet_gw",
        "virtnet_ip",
        "virtnet_mask",
        "virtio_mmio.device",
    }
)


@dataclass(frozen=True)
class SandboxLayer:
    role: str
    path: Path

    @classmethod
    def parse(cls, value: str) -> SandboxLayer:
        fields = value.split(",")
        if len(fields) != 2:
            raise ScriptError("--layer must be ROLE,PATH")
        role, raw_path = fields
        if role not in LAYER_ROLES:
            raise ScriptError(
                f"unsupported layer role {role!r}; choose {', '.join(LAYER_ROLES)}"
            )
        if not raw_path:
            raise ScriptError(f"{role} layer path is empty")
        return cls(role=role, path=Path(raw_path))


@dataclass(frozen=True)
class SandboxLaunch:
    layers: tuple[SandboxLayer, ...]
    scratch: Path

    def __post_init__(self) -> None:
        if not 1 <= len(self.layers) <= len(LAYER_ROLES):
            raise ScriptError("a sandbox requires one to three read-only layers")
        roles = [layer.role for layer in self.layers]
        duplicates = sorted({role for role in roles if roles.count(role) > 1})
        if duplicates:
            raise ScriptError(
                f"duplicate sandbox layer role(s): {', '.join(duplicates)}"
            )

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

    def openvmm_arguments(
        self,
        control_socket: Path,
        boot_console_socket: Path,
        control_auth_handle: int,
    ) -> list[str]:
        for label, path in (
            ("control", control_socket),
            ("boot console", boot_console_socket),
        ):
            if not path.is_absolute():
                raise ScriptError(f"{label} socket path must be absolute")
            if any(character in os.fspath(path) for character in ",;\0\r\n"):
                raise ScriptError(f"{label} socket path contains a reserved character")
        if control_auth_handle < MINIMUM_CONTROL_AUTH_FD:
            raise ScriptError(
                "control authentication handle must be an inherited pipe FD"
            )
        arguments = [
            "--machine",
            "microvm",
            "--virtio-console",
            f"listen={boot_console_socket}",
            "--microvm-control-console",
            f"listen={control_socket}",
            "--microvm-control-auth-stdin",
            "--microvm-control-auth-timeout-ms",
            str(CONTROL_AUTH_TIMEOUT_MS),
        ]
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
            )
        )
        return arguments

    def kernel_command_line(self, user_command_line: str = "") -> str:
        if "\0" in user_command_line:
            raise ScriptError("kernel command line contains an embedded NUL")
        for token in user_command_line.split():
            parameter = token.split("=", maxsplit=1)[0].replace("\\", "").strip("\"'")
            if (
                parameter.startswith("nvx_")
                or parameter in HOST_OWNED_KERNEL_PARAMETERS
            ):
                raise ScriptError(f"{parameter} is owned by the sandbox profile")
        command_line = user_command_line.strip()
        if len(command_line.encode("utf-8")) + 1 > SANDBOX_COMMAND_LINE_MAX_SIZE:
            raise ScriptError(
                "sandbox kernel command line exceeds its "
                f"{SANDBOX_COMMAND_LINE_MAX_SIZE}-byte x86 budget"
            )
        return command_line


def _reject_disk_path(path: Path) -> None:
    value = os.fspath(path)
    if any(character in value for character in ",;"):
        raise ScriptError(
            f"disk paths containing commas or semicolons are unsupported: {value}"
        )
