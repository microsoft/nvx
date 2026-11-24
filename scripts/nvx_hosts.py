#!/usr/bin/env python3
"""Resolve NVX host profiles and run commands through their SSH connections.

The developer-local inventory selects a configured SSH destination and backend.
Commands and scripts use non-interactive SSH calls that fail instead of prompting,
and linked worktrees can reuse the ignored inventory from the main worktree. The
probe, worktree, and benchmark tools run on a host from this checkout, since the
host's checkout can predate them. `select` chooses where to run a backend: a
reachable host, or else this workstation or, for KVM on Windows, its WSL.
"""

from __future__ import annotations

import argparse
import base64
import io
import json
import os
import re
import shlex
import subprocess
import sys
import zipfile
from collections.abc import Sequence
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol, cast

import host_probe

INVENTORY_FILENAME = ".nvx-hosts.json"
REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = Path(__file__).resolve().parent
PROFILE_NAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
BACKEND_PLATFORMS = {
    "kvm": "linux-kvm",
    "mshv": "linux-mshv",
    "whp": "windows-whp",
}
HOST_TYPES = frozenset({"baremetal", "virtual-machine"})
PROFILE_KEYS = frozenset({"ssh_target", "backend", "host_type", "remote_repo", "notes"})
SSH_OPTIONS = ("-o", "BatchMode=yes", "-o", "ConnectTimeout=20")
COMMANDS = frozenset(
    {"list", "show", "select", "exec", "run", "probe", "worktree", "benchmark"}
)
# `exit` ends the session successfully in sh, cmd, and PowerShell alike, so it
# checks that a host accepts a non-interactive login whatever its shell. One
# connection attempt overrides an SSH configuration that retries, which would
# multiply the connect timeout for every unreachable host.
REACHABILITY_OPTIONS = ("-o", "ConnectionAttempts=1")
REACHABILITY_COMMAND = "exit"
# The backends that this workstation runs natively, by sys.platform.
LOCAL_BACKENDS = {"linux": ("kvm", "mshv"), "win32": ("whp",)}
LINUX_DEVICES = {"kvm": "/dev/kvm", "mshv": "/dev/mshv"}
# WSL 2 runs KVM only with nested virtualization, which exposes /dev/kvm; the
# check also prints the checkout's WSL path so one wsl.exe launch answers both.
WSL_KVM_CHECK = (
    "test -r /dev/kvm && test -w /dev/kvm && command -v python3 >/dev/null && "
    'wslpath -a -u "$1"'
)
# The tools that run on a host from this checkout, since a host's checkout can
# predate them, with the modules that each imports.
REMOTE_TOOLS = {
    "host_probe": ("host_probe.py",),
    "host_worktree": ("host_worktree.py", "openvmm_git.py"),
    "benchmark_series": ("benchmark_series.py", "host_probe.py"),
}
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


def _decode_output(data: bytes | None) -> str:
    """Decode captured output as text mode does: UTF-8 with replacement
    characters and LF for every newline, since a Windows host ends its lines
    with CRLF and the callers print and parse the text."""
    text = (data or b"").decode("utf-8", errors="replace")
    return text.replace("\r\n", "\n").replace("\r", "\n")


class SubprocessRunner:
    """Run commands while inheriting output unless stdout must be inspected."""

    def run(
        self,
        args: Sequence[str],
        *,
        input_text: str | None = None,
        capture_stdout: bool = False,
    ) -> CommandResult:
        # The input travels as bytes, since text mode would write each LF of an
        # uploaded script as os.linesep, which is CRLF on Windows.
        completed = subprocess.run(
            list(args),
            input=None if input_text is None else input_text.encode("utf-8"),
            stdout=subprocess.PIPE if capture_stdout else None,
            check=False,
        )
        return CommandResult(completed.returncode, _decode_output(completed.stdout))


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


def load_inventory(
    path: Path, *, in_inventory_order: bool = False
) -> list[HostProfile] | None:
    """Load and validate an inventory, or return None when it does not exist.
    Profiles come sorted by name, or in the order that the inventory declares
    them when `in_inventory_order` is set."""
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
    names = list(hosts) if in_inventory_order else sorted(hosts)
    return [_parse_profile(name, hosts[name]) for name in names]


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
            # ssh joins its words into the command that the remote shell
            # splits again.
            result = runner.run(
                [
                    *_ssh_prefix(profile, no_stdin=True),
                    f"rm -f -- {shlex.quote(remote_path)}",
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


def _tool_archive(tool: str) -> bytes:
    """Return a zip application of a tool and the modules that it imports,
    which Python runs as `python ARCHIVE ARGUMENTS`."""
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        for name in REMOTE_TOOLS[tool]:
            archive.write(SCRIPTS / name, name)
        archive.writestr(
            "__main__.py", f"import sys\n\nimport {tool}\n\nsys.exit({tool}.main())\n"
        )
    return buffer.getvalue()


def _upload_archive(profile: HostProfile, data: bytes, runner: Runner) -> str:
    """Write data to a new temporary file on the host, and return its path."""
    encoded = base64.b64encode(data).decode("ascii")
    if profile.backend == "whp":
        bootstrap = (
            "$path = Join-Path ([IO.Path]::GetTempPath()) "
            "('nvx-' + [guid]::NewGuid() + '.pyz'); "
            "[IO.File]::WriteAllBytes($path, "
            "[Convert]::FromBase64String([Console]::In.ReadToEnd())); "
            "[Console]::Out.Write($path)"
        )
        upload = runner.run(
            _powershell_ssh(profile, bootstrap, no_stdin=False),
            input_text=encoded,
            capture_stdout=True,
        )
    else:
        upload = runner.run(
            [
                *_ssh_prefix(profile, no_stdin=False),
                'f=$(mktemp) && base64 -d > "$f" && printf %s "$f"',
            ],
            input_text=encoded,
            capture_stdout=True,
        )
    return _uploaded_path(upload, profile)


def _run_tool(
    profile: HostProfile,
    directory: str,
    tool: str,
    arguments: Sequence[str],
    runner: Runner,
    *,
    capture_stdout: bool = False,
) -> CommandResult:
    """Run this checkout's copy of a tool on the host from a temporary file,
    which it removes afterwards."""
    remote_path = _upload_archive(profile, _tool_archive(tool), runner)
    try:
        python = "python" if profile.backend == "whp" else "python3"
        return _exec_remote(
            profile,
            directory,
            [python, remote_path, *arguments],
            runner,
            capture_stdout=capture_stdout,
        )
    finally:
        _remove_remote_script(profile, remote_path, runner)


def _probe(
    profile: HostProfile,
    directory: str,
    no_verify: bool,
    runner: Runner,
) -> int:
    arguments = ["--repo", directory, "--json", "--backend", profile.backend]
    if no_verify:
        arguments.append("--no-verify")
    result = _run_tool(
        profile, directory, "host_probe", arguments, runner, capture_stdout=True
    )
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


def _reachable(profile: HostProfile, runner: Runner) -> bool:
    """Return whether the host accepts a non-interactive SSH session."""
    try:
        result = runner.run(
            [
                "ssh",
                *SSH_OPTIONS,
                *REACHABILITY_OPTIONS,
                "-n",
                profile.ssh_target,
                REACHABILITY_COMMAND,
            ],
            capture_stdout=True,
        )
    except OSError:
        return False
    return result.returncode == 0


def _native_backend_problem(backend: str, platform: str) -> str | None:
    """Return why this workstation cannot run the backend natively, or None
    when it can. WHP needs the Windows Hypervisor Platform API to report a
    hypervisor, which `nvx.py doctor` checks too without elevation."""
    if backend not in LOCAL_BACKENDS.get(platform, ()):
        return f"this workstation cannot run {backend}"
    if backend == "whp":
        state = host_probe.whp_api_state()
        if state.get("available") is not True:
            return f"the Windows Hypervisor Platform is unusable: {state.get('detail')}"
        if state.get("hypervisor_present") is not True:
            return "the Windows Hypervisor Platform reports no hypervisor"
        return None
    device = LINUX_DEVICES[backend]
    if not os.access(device, os.R_OK | os.W_OK):
        return f"this workstation cannot read and write {device}"
    return None


def _wsl_checkout(runner: Runner) -> str | None:
    """Return this checkout's path in the default WSL distribution when that
    distribution can run KVM with Python, or None otherwise."""
    try:
        result = runner.run(
            [
                "wsl.exe",
                "--exec",
                "sh",
                "-c",
                WSL_KVM_CHECK,
                "sh",
                str(REPOSITORY_ROOT),
            ],
            capture_stdout=True,
        )
    except OSError:
        return None
    path = result.stdout.strip()
    if result.returncode != 0 or not path.startswith("/"):
        return None
    return path


def select_execution(
    profiles: Sequence[HostProfile],
    backend: str,
    host_type: str | None,
    runner: Runner,
    platform: str | None = None,
) -> dict[str, object]:
    """Choose where to run a backend: the first reachable profile, in inventory
    order, that serves it, then this workstation natively, then, for KVM on
    Windows, its default WSL distribution. A requested host type rules out the
    local fallbacks, whose host type the inventory does not record."""
    platform = sys.platform if platform is None else platform
    candidates = [
        profile
        for profile in profiles
        if profile.backend == backend
        and (host_type is None or profile.host_type == host_type)
    ]
    selection: dict[str, object] = {
        "backend": backend,
        "platform": BACKEND_PLATFORMS[backend],
        "host_type": host_type,
        "execution": None,
        "host": None,
        "checkout": None,
        "unreachable": [],
    }

    def reachable_with_runner(profile: HostProfile) -> bool:
        return _reachable(profile, runner)

    # Probe concurrently, so unreachable hosts cost one connect timeout in
    # total, and keep the inventory's order for the choice.
    with ThreadPoolExecutor(max_workers=max(1, len(candidates))) as executor:
        reachable = list(executor.map(reachable_with_runner, candidates))
    for profile, accepted in zip(candidates, reachable, strict=True):
        if accepted:
            selection.update(
                execution="ssh",
                host=profile.to_json(),
                host_type=profile.host_type,
                checkout=profile.remote_repo,
                reason=f"host profile {profile.name!r} is reachable",
            )
            break
    selection["unreachable"] = [
        profile.name
        for profile, accepted in zip(candidates, reachable, strict=True)
        if not accepted
    ]
    if selection["execution"] is not None:
        return selection

    if candidates:
        remote = f"no configured {backend} host is reachable"
    else:
        remote = f"no configured host serves {backend}"
    if host_type is not None:
        selection["reason"] = (
            f"{remote} with host type {host_type}; local execution has no "
            "recorded host type"
        )
        return selection
    native_problem = _native_backend_problem(backend, platform)
    if native_problem is None:
        selection.update(
            execution="local",
            checkout=str(REPOSITORY_ROOT),
            reason=f"{remote}; this workstation runs {backend} natively",
        )
        return selection
    if backend == "kvm" and platform == "win32":
        wsl_checkout = _wsl_checkout(runner)
        if wsl_checkout is not None:
            selection.update(
                execution="wsl",
                checkout=wsl_checkout,
                reason=f"{remote}; the default WSL distribution exposes /dev/kvm",
            )
            return selection
        selection["reason"] = (
            f"{remote}, and the default WSL distribution does not start or "
            "lacks a usable /dev/kvm or python3"
        )
        return selection
    selection["reason"] = f"{remote}, and {native_problem}"
    return selection


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="subcommand", required=True)

    list_parser = subparsers.add_parser("list", help="list configured profiles")
    list_parser.add_argument("--file", type=Path, help="inventory path")

    show_parser = subparsers.add_parser("show", help="show one profile")
    show_parser.add_argument("profile", help="host profile name")
    show_parser.add_argument("--file", type=Path, help="inventory path")

    select_parser = subparsers.add_parser(
        "select",
        help="choose a reachable host for a backend, or a local fallback",
    )
    select_parser.add_argument("backend", choices=sorted(BACKEND_PLATFORMS))
    select_parser.add_argument(
        "--host-type",
        choices=sorted(HOST_TYPES),
        help="require this host type, which rules out the local fallbacks",
    )
    select_parser.add_argument("--file", type=Path, help="inventory path")

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

    worktree_parser = subparsers.add_parser(
        "worktree", help="add or remove a temporary worktree of a host's checkout"
    )
    worktree_parser.add_argument("profile", help="host profile name")
    worktree_parser.add_argument("--file", type=Path, help="inventory path")
    worktree_parser.add_argument("--cwd", help="remote working directory")
    worktree_parser.add_argument("tool_arguments", nargs="+")

    benchmark_parser = subparsers.add_parser(
        "benchmark", help="run a benchmark series in a host's checkout"
    )
    benchmark_parser.add_argument("profile", help="host profile name")
    benchmark_parser.add_argument("--file", type=Path, help="inventory path")
    benchmark_parser.add_argument("--cwd", help="remote working directory")
    benchmark_parser.add_argument("tool_arguments", nargs="+")
    return parser


def _arguments(argv: Sequence[str] | None) -> list[str]:
    arguments = list(sys.argv[1:] if argv is None else argv)
    if not arguments:
        return ["list"]
    if arguments[0] not in COMMANDS and not arguments[0].startswith("-"):
        arguments.insert(0, "show")
    if arguments[0] == "run":
        arguments = _normalize_run_arguments(arguments)
    return arguments


def _normalize_run_arguments(arguments: list[str]) -> list[str]:
    option_arguments: list[str] = []
    other_arguments: list[str] = []
    index = 1
    while index < len(arguments):
        argument = arguments[index]
        if argument == "--":
            other_arguments.extend(arguments[index:])
            break
        option = argument.partition("=")[0]
        matches = [name for name in ("--file", "--cwd") if name.startswith(option)]
        if len(matches) == 1:
            option_arguments.append(argument)
            if "=" not in argument:
                if index + 1 == len(arguments) or arguments[index + 1].startswith("-"):
                    return arguments
                option_arguments.append(arguments[index + 1])
                index += 2
                continue
        else:
            other_arguments.append(argument)
        index += 1
    # Older argparse versions stop collecting nargs="*" positionals after
    # matching the profile and script, so keep run options ahead of them.
    return [arguments[0], *option_arguments, *other_arguments]


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
        profiles = load_inventory(
            inventory_path, in_inventory_order=subcommand == "select"
        )
        if subcommand == "select":
            selection = select_execution(
                profiles or [],
                cast(str, namespace.backend),
                cast(str | None, namespace.host_type),
                runner,
            )
            _write_json({"inventory": str(inventory_path), **selection})
            return 0 if selection["execution"] is not None else 1
        if not profiles:
            reason = (
                f"{INVENTORY_FILENAME} is missing or has no hosts; "
                "only local NVX development is enabled"
            )
            if subcommand in ("exec", "run", "probe", "worktree", "benchmark"):
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
        if subcommand in ("worktree", "benchmark"):
            tool = "host_worktree" if subcommand == "worktree" else "benchmark_series"
            arguments = _remainder(cast(list[str], namespace.tool_arguments))
            return _run_tool(
                profile,
                directory,
                tool,
                ["--repo", directory, *arguments],
                runner,
            ).returncode
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
