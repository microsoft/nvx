#!/usr/bin/env python3
"""Resolve NVX host profiles and run commands through their SSH connections.

The developer-local inventory selects a configured SSH destination and backend.
Commands and scripts use non-interactive SSH calls that fail instead of prompting,
and linked worktrees can reuse the ignored inventory from the main worktree.
"""

from __future__ import annotations

import argparse
import base64
import json
import re
import shlex
import subprocess
import sys
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol, cast

INVENTORY_FILENAME = ".nvx-hosts.json"
REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
PROFILE_NAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
BACKEND_PLATFORMS = {
    "kvm": "linux-kvm",
    "mshv": "linux-mshv",
    "whp": "windows-whp",
}
HOST_TYPES = frozenset({"baremetal", "virtual-machine"})
PROFILE_KEYS = frozenset({"ssh_target", "backend", "host_type", "remote_repo", "notes"})
SSH_OPTIONS = ("-o", "BatchMode=yes", "-o", "ConnectTimeout=20")
COMMANDS = frozenset({"list", "show", "exec", "run", "probe"})
# With redirected output, PowerShell writes its progress records, such as those
# of a module's first load, to stderr as CLIXML.
QUIET_PROGRESS = "$ProgressPreference = 'SilentlyContinue'; "


class InventoryError(ValueError):
    """Raised when a host inventory is malformed."""


class HostCommandError(RuntimeError):
    """Raised when a host command cannot be started safely."""

    def __init__(self, message: str, status: int = 1) -> None:
        super().__init__(message)
        self.status = status


@dataclass(frozen=True)
class HostProfile:
    """One validated host profile from the developer-local inventory."""

    name: str
    ssh_target: str
    backend: str
    host_type: str
    remote_repo: str | None
    notes: str | None

    def to_json(self) -> dict[str, object]:
        return {
            "name": self.name,
            "ssh_target": self.ssh_target,
            "backend": self.backend,
            "platform": BACKEND_PLATFORMS[self.backend],
            "host_type": self.host_type,
            "remote_repo": self.remote_repo,
            "notes": self.notes,
        }


@dataclass(frozen=True)
class CommandResult:
    """The status and optional captured standard output of a subprocess."""

    returncode: int
    stdout: str = ""


class Runner(Protocol):
    """Run local commands, allowing tests to replace SSH and Git."""

    def run(
        self,
        args: Sequence[str],
        *,
        input_text: str | None = None,
        capture_stdout: bool = False,
    ) -> CommandResult: ...


class SubprocessRunner:
    """Run commands while inheriting output unless stdout must be inspected."""

    def run(
        self,
        args: Sequence[str],
        *,
        input_text: str | None = None,
        capture_stdout: bool = False,
    ) -> CommandResult:
        completed: subprocess.CompletedProcess[str] = subprocess.run(
            list(args),
            input=input_text,
            stdout=subprocess.PIPE if capture_stdout else None,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
        return CommandResult(completed.returncode, completed.stdout or "")


def _mapping(value: object, context: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise InventoryError(f"{context} must be a JSON object with string keys")
    untyped = cast(dict[object, object], value)
    if not all(isinstance(key, str) for key in untyped):
        raise InventoryError(f"{context} must be a JSON object with string keys")
    return cast(dict[str, object], untyped)


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
    host_type = _required_string(profile, "host_type", context)
    if host_type not in HOST_TYPES:
        choices = ", ".join(sorted(HOST_TYPES))
        raise InventoryError(f"{context}.host_type must be one of: {choices}")

    return HostProfile(
        name=name,
        ssh_target=ssh_target,
        backend=backend,
        host_type=host_type,
        remote_repo=_optional_string(profile, "remote_repo", context),
        notes=_optional_string(profile, "notes", context),
    )


def load_inventory(path: Path) -> list[HostProfile] | None:
    """Load and validate an inventory, or return None when it does not exist."""
    if not path.is_file():
        return None
    try:
        raw: object = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise InventoryError(f"cannot read {path}: {error}") from error

    inventory = _mapping(raw, "inventory")
    unknown_keys = sorted(inventory.keys() - {"hosts"})
    if unknown_keys:
        raise InventoryError(
            f"inventory has unsupported fields: {', '.join(unknown_keys)}"
        )

    hosts = _mapping(inventory.get("hosts"), "inventory.hosts")
    return [_parse_profile(name, hosts[name]) for name in sorted(hosts)]


def _default_inventory(runner: Runner) -> Path:
    inventory = REPOSITORY_ROOT / INVENTORY_FILENAME
    if inventory.is_file():
        return inventory.resolve()

    result = runner.run(
        [
            "git",
            "-C",
            str(REPOSITORY_ROOT),
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ],
        capture_stdout=True,
    )
    common_text = result.stdout.strip()
    if result.returncode == 0 and common_text:
        common_directory = Path(common_text)
        if not common_directory.is_absolute():
            common_directory = REPOSITORY_ROOT / common_directory
        fallback = common_directory.resolve().parent / INVENTORY_FILENAME
        if fallback.is_file():
            return fallback.resolve()
    return inventory.resolve()


def _write_json(payload: dict[str, object]) -> None:
    json.dump(payload, sys.stdout, indent=2)
    sys.stdout.write("\n")


def _find_profile(profiles: Sequence[HostProfile], name: str) -> HostProfile:
    selected = next((profile for profile in profiles if profile.name == name), None)
    if selected is None:
        available = ", ".join(profile.name for profile in profiles) or "none"
        raise HostCommandError(f"unknown host profile {name!r}; available: {available}")
    return selected


def _working_directory(profile: HostProfile, requested: str | None) -> str:
    directory = requested if requested is not None else profile.remote_repo
    if directory is None or not directory.strip():
        raise HostCommandError(
            f"host profile {profile.name!r} has no remote_repo; pass --cwd DIR"
        )
    return directory


def _ssh_prefix(profile: HostProfile, *, no_stdin: bool) -> list[str]:
    prefix = ["ssh", *SSH_OPTIONS]
    if no_stdin:
        prefix.append("-n")
    prefix.append(profile.ssh_target)
    return prefix


def _powershell_literal(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def _encoded_powershell(script: str) -> str:
    return base64.b64encode((QUIET_PROGRESS + script).encode("utf-16-le")).decode(
        "ascii"
    )


def _powershell_ssh(profile: HostProfile, script: str, *, no_stdin: bool) -> list[str]:
    return [
        *_ssh_prefix(profile, no_stdin=no_stdin),
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-EncodedCommand",
        _encoded_powershell(script),
    ]


def _exec_remote(
    profile: HostProfile,
    directory: str,
    command: Sequence[str],
    runner: Runner,
    *,
    capture_stdout: bool = False,
) -> CommandResult:
    if not command:
        raise HostCommandError("exec requires a command after --")
    if profile.backend == "whp":
        words = " ".join(_powershell_literal(word) for word in command)
        script = (
            "$ErrorActionPreference = 'Stop'; "
            f"Set-Location -LiteralPath {_powershell_literal(directory)}; "
            f"& {words}; exit $LASTEXITCODE"
        )
        argv = _powershell_ssh(profile, script, no_stdin=True)
    else:
        remote = "cd -- {} && {}".format(
            shlex.quote(directory), " ".join(shlex.quote(word) for word in command)
        )
        argv = [*_ssh_prefix(profile, no_stdin=True), remote]
    return runner.run(argv, capture_stdout=capture_stdout)


def _read_script(path: Path, *, linux: bool) -> str:
    try:
        data = path.read_bytes()
    except OSError as error:
        raise HostCommandError(f"cannot read script {path}: {error}") from error
    if not data:
        raise HostCommandError(f"script {path} is empty")
    try:
        text = data.decode("utf-8")
    except UnicodeError as error:
        raise HostCommandError(f"script {path} is not valid UTF-8: {error}") from error
    return text.replace("\r\n", "\n") if linux else text


def _uploaded_path(result: CommandResult, profile: HostProfile) -> str:
    if result.returncode != 0:
        raise HostCommandError(
            f"failed to upload script to host profile {profile.name!r}",
            result.returncode,
        )
    remote_path = result.stdout.strip()
    if not remote_path:
        raise HostCommandError(
            f"host profile {profile.name!r} returned no temporary script path"
        )
    if "\x00" in remote_path or "\n" in remote_path or "\r" in remote_path:
        raise HostCommandError(
            f"host profile {profile.name!r} returned an invalid temporary script path"
        )
    return remote_path


def _remove_remote_script(
    profile: HostProfile, remote_path: str, runner: Runner
) -> None:
    try:
        if profile.backend == "whp":
            script = (
                "$ErrorActionPreference = 'Stop'; "
                f"Remove-Item -LiteralPath {_powershell_literal(remote_path)} "
                "-Force -ErrorAction Stop"
            )
            result = runner.run(_powershell_ssh(profile, script, no_stdin=True))
        else:
            result = runner.run(
                [
                    *_ssh_prefix(profile, no_stdin=True),
                    "rm",
                    "-f",
                    "--",
                    remote_path,
                ]
            )
    except OSError as error:
        print(
            f"error: failed to remove remote script {remote_path}: {error}",
            file=sys.stderr,
        )
        return
    if result.returncode != 0:
        print(
            f"error: failed to remove remote script {remote_path} "
            f"from host profile {profile.name!r}",
            file=sys.stderr,
        )


def _run_remote_script(
    profile: HostProfile,
    directory: str,
    script_path: Path,
    arguments: Sequence[str],
    runner: Runner,
) -> int:
    linux = profile.backend != "whp"
    script_text = _read_script(script_path, linux=linux)
    if linux:
        upload_command = 'f=$(mktemp) && cat > "$f" && printf %s "$f"'
        upload = runner.run(
            [*_ssh_prefix(profile, no_stdin=False), upload_command],
            input_text=script_text,
            capture_stdout=True,
        )
    else:
        bootstrap = (
            "$path = Join-Path ([IO.Path]::GetTempPath()) "
            "('nvx-' + [guid]::NewGuid() + '.ps1'); "
            "[IO.File]::WriteAllText($path, [Console]::In.ReadToEnd()); "
            "[Console]::Out.Write($path)"
        )
        upload = runner.run(
            _powershell_ssh(profile, bootstrap, no_stdin=False),
            input_text=script_text,
            capture_stdout=True,
        )

    remote_path = _uploaded_path(upload, profile)
    try:
        if linux:
            remote = "cd -- {} && bash {}".format(
                shlex.quote(directory),
                " ".join(shlex.quote(word) for word in (remote_path, *arguments)),
            )
            result = runner.run([*_ssh_prefix(profile, no_stdin=True), remote])
        else:
            words = " ".join(
                _powershell_literal(word) for word in (remote_path, *arguments)
            )
            command = (
                "$ErrorActionPreference = 'Stop'; "
                f"Set-Location -LiteralPath {_powershell_literal(directory)}; "
                f"& {words}; exit $LASTEXITCODE"
            )
            result = runner.run(_powershell_ssh(profile, command, no_stdin=True))
        return result.returncode
    finally:
        _remove_remote_script(profile, remote_path, runner)


def _probe(
    profile: HostProfile,
    directory: str,
    no_verify: bool,
    runner: Runner,
) -> int:
    python = "python" if profile.backend == "whp" else "python3"
    probe_path = (
        r"scripts\host_probe.py"
        if profile.backend == "whp"
        else "scripts/host_probe.py"
    )
    command = [
        python,
        probe_path,
        "--json",
        "--backend",
        profile.backend,
    ]
    if no_verify:
        command.append("--no-verify")
    result = _exec_remote(profile, directory, command, runner, capture_stdout=True)
    try:
        parsed: object = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        print(
            f"error: host probe returned invalid JSON: {error}",
            file=sys.stderr,
        )
        return result.returncode if result.returncode != 0 else 1
    del parsed
    sys.stdout.write(result.stdout)
    if result.stdout and not result.stdout.endswith("\n"):
        sys.stdout.write("\n")
    return result.returncode


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="subcommand", required=True)

    list_parser = subparsers.add_parser("list", help="list configured profiles")
    list_parser.add_argument("--file", type=Path, help="inventory path")

    show_parser = subparsers.add_parser("show", help="show one profile")
    show_parser.add_argument("profile", help="host profile name")
    show_parser.add_argument("--file", type=Path, help="inventory path")

    exec_parser = subparsers.add_parser("exec", help="run one remote command")
    exec_parser.add_argument("profile", help="host profile name")
    exec_parser.add_argument("--file", type=Path, help="inventory path")
    exec_parser.add_argument("--cwd", help="remote working directory")
    exec_parser.add_argument("remote_command", nargs="+")

    run_parser = subparsers.add_parser("run", help="upload and run a script")
    run_parser.add_argument("profile", help="host profile name")
    run_parser.add_argument("script", type=Path, help="local script path")
    run_parser.add_argument("--file", type=Path, help="inventory path")
    run_parser.add_argument("--cwd", help="remote working directory")
    run_parser.add_argument("script_arguments", nargs="*")

    probe_parser = subparsers.add_parser("probe", help="probe a configured host")
    probe_parser.add_argument("profile", help="host profile name")
    probe_parser.add_argument("--file", type=Path, help="inventory path")
    probe_parser.add_argument("--cwd", help="remote working directory")
    probe_parser.add_argument("--no-verify", action="store_true")
    return parser


def _arguments(argv: Sequence[str] | None) -> list[str]:
    arguments = list(sys.argv[1:] if argv is None else argv)
    if not arguments:
        return ["list"]
    if arguments[0] not in COMMANDS and not arguments[0].startswith("-"):
        arguments.insert(0, "show")
    return arguments


def _remainder(words: Sequence[str]) -> list[str]:
    result = list(words)
    if result and result[0] == "--":
        return result[1:]
    return result


def run_cli(argv: Sequence[str] | None, runner: Runner) -> int:
    """Run the command-line interface with an injectable command runner."""
    namespace = _parser().parse_args(_arguments(argv))
    subcommand = cast(str, namespace.subcommand)
    requested_file = cast(Path | None, getattr(namespace, "file", None))
    try:
        inventory_path = (
            requested_file.expanduser().resolve()
            if requested_file is not None
            else _default_inventory(runner)
        )
        profiles = load_inventory(inventory_path)
        if not profiles:
            reason = (
                f"{INVENTORY_FILENAME} is missing or has no hosts; "
                "only local NVX development is enabled"
            )
            if subcommand in ("exec", "run", "probe"):
                raise HostCommandError(f"{inventory_path}: {reason}")
            _write_json(
                {
                    "remote_enabled": False,
                    "inventory": str(inventory_path),
                    "reason": reason,
                    "hosts": [],
                }
            )
            return 0

        if subcommand == "list":
            _write_json(
                {
                    "remote_enabled": True,
                    "inventory": str(inventory_path),
                    "hosts": [profile.to_json() for profile in profiles],
                }
            )
            return 0

        profile_name = cast(str, namespace.profile)
        profile = _find_profile(profiles, profile_name)
        if subcommand == "show":
            _write_json(
                {
                    "remote_enabled": True,
                    "inventory": str(inventory_path),
                    "host": profile.to_json(),
                }
            )
            return 0

        requested_directory = cast(str | None, namespace.cwd)
        directory = _working_directory(profile, requested_directory)
        if subcommand == "exec":
            command = _remainder(cast(list[str], namespace.remote_command))
            return _exec_remote(profile, directory, command, runner).returncode
        if subcommand == "run":
            script = cast(Path, namespace.script)
            script_arguments = _remainder(cast(list[str], namespace.script_arguments))
            return _run_remote_script(
                profile, directory, script, script_arguments, runner
            )
        if subcommand == "probe":
            no_verify = cast(bool, namespace.no_verify)
            return _probe(profile, directory, no_verify, runner)
        raise HostCommandError(f"unsupported subcommand {subcommand!r}")
    except (InventoryError, HostCommandError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        if isinstance(error, HostCommandError):
            return error.status
        return 1


def main(argv: Sequence[str] | None = None) -> int:
    """Run the host profile command-line tool."""
    return run_cli(argv, SubprocessRunner())


if __name__ == "__main__":
    raise SystemExit(main())
