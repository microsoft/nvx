#!/usr/bin/env python3
"""Probe whether the current host can run NVX without changing the host.

The probe records repository and artifact state, runs NVX verification unless it
is skipped, and inspects the selected platform's existing hypervisor facilities.
It never installs software, changes permissions, or requests elevation.
"""

from __future__ import annotations

import argparse
import ctypes
import json
import os
import platform
import subprocess
import sys
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol, cast

REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
BACKENDS = ("kvm", "mshv", "whp")
# WHvCapabilityCodeHypervisorPresent, which `nvx.py doctor`'s H1 check queries.
WHP_CAPABILITY_HYPERVISOR_PRESENT = 0
POWERSHELL = ("powershell", "-NoProfile", "-NonInteractive", "-Command")
# With redirected output, PowerShell writes its progress records, such as those
# of a module's first load, to stderr as CLIXML.
QUIET_PROGRESS = "$ProgressPreference = 'SilentlyContinue'; "
HYPERVISOR_PRESENT = "(Get-CimInstance Win32_ComputerSystem).HypervisorPresent"
HYPERVISOR_PLATFORM = (
    "(Get-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform).State"
)
# Get-Process sets $? when it finds no process, whatever its -ErrorAction, which
# makes powershell -Command exit with 1; collecting the processes first does not.
OPENVMM_PROCESSES = (
    "$processes = @(Get-Process -Name openvmm -ErrorAction Ignore | "
    "Select-Object Id,Path); "
    "if ($processes.Count) { ConvertTo-Json -InputObject $processes -Compress }"
)


class ProbeError(RuntimeError):
    """Raised when required probe information cannot be collected."""


@dataclass(frozen=True)
class CommandResult:
    """Captured output and status from one read-only probe command."""

    returncode: int
    stdout: str = ""
    stderr: str = ""


class Runner(Protocol):
    """Run probe commands, allowing unit tests to avoid the real host."""

    def run(
        self,
        args: Sequence[str],
        *,
        cwd: Path | None = None,
        timeout: float | None = None,
        merge_stderr: bool = False,
    ) -> CommandResult: ...


class SubprocessRunner:
    """Run a command with all output captured for the report."""

    def run(
        self,
        args: Sequence[str],
        *,
        cwd: Path | None = None,
        timeout: float | None = None,
        merge_stderr: bool = False,
    ) -> CommandResult:
        completed = subprocess.run(
            list(args),
            cwd=cwd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT if merge_stderr else subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
            check=False,
        )
        return CommandResult(
            completed.returncode,
            completed.stdout or "",
            "" if merge_stderr else completed.stderr or "",
        )


def _command_detail(result: CommandResult) -> str:
    return result.stderr.strip() or result.stdout.strip() or "no diagnostic output"


def _git(runner: Runner, directory: Path, *arguments: str) -> str:
    result = runner.run(["git", "-C", str(directory), *arguments])
    if result.returncode != 0:
        command = " ".join(("git", *arguments))
        raise ProbeError(f"{command} failed in {directory}: {_command_detail(result)}")
    return result.stdout.rstrip("\r\n")


def _repository(runner: Runner, candidate: Path) -> tuple[Path, dict[str, object]]:
    root_text = _git(runner, candidate, "rev-parse", "--show-toplevel")
    if not root_text:
        raise ProbeError("git rev-parse --show-toplevel returned an empty path")
    root = Path(root_text).resolve()
    head = _git(runner, root, "rev-parse", "HEAD")
    branch_text = _git(runner, root, "branch", "--show-current")
    dirty = _git(runner, root, "status", "--short").splitlines()
    return root, {
        "root": str(root),
        "head": head,
        "branch": branch_text or None,
        "dirty": dirty,
    }


def _openvmm(runner: Runner, root: Path) -> dict[str, object] | str:
    gitlink = _git(runner, root, "rev-parse", "HEAD:openvmm")
    checkout = root / "openvmm"
    if not (checkout / ".git").exists():
        return "not initialized"
    checked_out = _git(runner, checkout, "rev-parse", "HEAD")
    changes = _git(runner, checkout, "status", "--short").splitlines()
    return {
        "gitlink": gitlink,
        "checked_out": checked_out,
        "matches": checked_out == gitlink,
        "dirty": bool(changes),
        "status": changes,
    }


def _verify(runner: Runner, root: Path) -> dict[str, object]:
    result = runner.run(
        [sys.executable, "scripts/nvx.py", "verify"],
        cwd=root,
        timeout=300,
        merge_stderr=True,
    )
    return {
        "exit_code": result.returncode,
        "last_lines": result.stdout.splitlines()[-20:],
    }


def _linux_devices() -> dict[str, object]:
    devices: dict[str, object] = {}
    for name in ("/dev/kvm", "/dev/mshv"):
        path = Path(name)
        devices[name] = {
            "exists": path.exists(),
            "readable": os.access(path, os.R_OK),
            "writable": os.access(path, os.W_OK),
        }
    return devices


def powershell_arguments(command: str) -> tuple[str, ...]:
    """Return the Windows PowerShell command line that runs one probe command."""
    return (*POWERSHELL, QUIET_PROGRESS + command)


def _powershell(runner: Runner, command: str) -> CommandResult:
    return runner.run(powershell_arguments(command))


def whp_api_state() -> dict[str, object]:
    """Ask the Windows Hypervisor Platform API whether it serves a hypervisor,
    as `nvx.py doctor`'s H1 check does; it needs no elevation."""
    if sys.platform != "win32":
        return {"available": False, "detail": "the API exists only on Windows"}
    try:
        library = ctypes.WinDLL("WinHvPlatform.dll")
    except OSError as error:
        return {
            "available": False,
            "detail": f"WinHvPlatform.dll is unavailable: {error}",
        }
    present = ctypes.c_int32(0)
    written = ctypes.c_uint32(0)
    result = int(
        library.WHvGetCapability(
            WHP_CAPABILITY_HYPERVISOR_PRESENT,
            ctypes.byref(present),
            ctypes.sizeof(present),
            ctypes.byref(written),
        )
    )
    if result != 0:
        return {
            "available": False,
            "detail": (
                f"WHvGetCapability failed with HRESULT 0x{result & 0xFFFFFFFF:08x}"
            ),
        }
    return {"available": True, "hypervisor_present": bool(present.value)}


def _windows_devices(runner: Runner) -> dict[str, object]:
    present_result = _powershell(runner, HYPERVISOR_PRESENT)
    if present_result.returncode == 0:
        present_text = present_result.stdout.strip().lower()
        if present_text == "true":
            hypervisor_present: bool | str = True
        elif present_text == "false":
            hypervisor_present = False
        else:
            hypervisor_present = "unknown"
    else:
        hypervisor_present = "unknown"

    feature_result = _powershell(runner, HYPERVISOR_PLATFORM)
    if feature_result.returncode == 0 and feature_result.stdout.strip():
        feature_state = feature_result.stdout.strip()
    else:
        diagnostic = f"{feature_result.stdout}\n{feature_result.stderr}".lower()
        elevation_markers = (
            "access is denied",
            "access denied",
            "requires elevation",
            "requested operation requires elevation",
        )
        feature_state = (
            "unknown (needs elevation)"
            if any(marker in diagnostic for marker in elevation_markers)
            else "unknown"
        )
    return {
        "HypervisorPresent": hypervisor_present,
        "HypervisorPlatform": feature_state,
        "WinHvPlatform": whp_api_state(),
    }


def _linux_processes(runner: Runner) -> list[str]:
    result = runner.run(["pgrep", "-af", "openvmm"])
    if result.returncode not in (0, 1):
        raise ProbeError(f"pgrep -af openvmm failed: {_command_detail(result)}")
    return result.stdout.splitlines()


def _windows_processes(runner: Runner) -> list[object]:
    result = _powershell(runner, OPENVMM_PROCESSES)
    if result.returncode != 0:
        raise ProbeError(f"Get-Process openvmm failed: {_command_detail(result)}")
    text = result.stdout.strip()
    if not text:
        return []
    try:
        parsed: object = json.loads(text)
    except json.JSONDecodeError as error:
        raise ProbeError(
            f"Get-Process openvmm returned invalid JSON: {error}"
        ) from error
    if isinstance(parsed, dict):
        return [parsed]
    if isinstance(parsed, list):
        return cast(list[object], parsed)
    raise ProbeError("Get-Process openvmm returned an unexpected JSON value")


def _artifacts(root: Path) -> dict[str, object]:
    build = root / "build"
    exists = build.exists()
    try:
        files = sum(1 for path in build.rglob("*") if path.is_file()) if exists else 0
    except OSError as error:
        raise ProbeError(f"cannot inspect build artifacts: {error}") from error
    return {"exists": exists, "files": files}


def _linux_device_usable(devices: dict[str, object], name: str) -> bool:
    value = devices.get(name)
    if not isinstance(value, dict):
        return False
    entry = cast(dict[str, object], value)
    return all(entry.get(key) is True for key in ("exists", "readable", "writable"))


def backend_verdict(
    name: str, system: str, devices: dict[str, object]
) -> dict[str, object]:
    if name in ("kvm", "mshv"):
        if system != "Linux":
            return {
                "name": name,
                "usable": False,
                "reason": f"{name} is supported only on Linux",
            }
        device = f"/dev/{name}"
        usable = _linux_device_usable(devices, device)
        reason = (
            f"{device} exists and is readable and writable"
            if usable
            else f"{device} must exist and be readable and writable"
        )
        return {"name": name, "usable": usable, "reason": reason}

    if system != "Windows":
        return {
            "name": name,
            "usable": False,
            "reason": "whp is supported only on Windows",
        }
    api = devices.get("WinHvPlatform")
    state = cast(dict[str, object], api) if isinstance(api, dict) else {}
    if state.get("available") is not True:
        detail = state.get("detail", "the Windows Hypervisor Platform was not queried")
        return {
            "name": name,
            "usable": False,
            "reason": (
                f"{detail}; enabling the Windows Hypervisor Platform needs an "
                "administrator and a reboot"
            ),
        }
    usable = state.get("hypervisor_present") is True
    return {
        "name": name,
        "usable": usable,
        "reason": (
            "the Windows Hypervisor Platform reports a hypervisor; a smoke run is "
            "the definitive check"
            if usable
            else "the Windows Hypervisor Platform reports no hypervisor"
        ),
    }


def collect_probe(
    backend: str | None,
    no_verify: bool,
    runner: Runner,
    repository_root: Path | None = None,
) -> dict[str, object]:
    """Collect a host probe document without modifying the checkout or host."""
    system = platform.system()
    candidate = REPOSITORY_ROOT if repository_root is None else repository_root
    root, repository = _repository(runner, candidate)
    if system == "Linux":
        devices = _linux_devices()
        processes: object = _linux_processes(runner)
    elif system == "Windows":
        devices = _windows_devices(runner)
        processes = _windows_processes(runner)
    else:
        devices = {}
        processes = []

    executable = root / "nvx-rs" / "target" / "release" / "nvx-rs"
    if system == "Windows":
        executable = executable.with_suffix(".exe")

    document: dict[str, object] = {
        "os": {
            "system": system,
            "release": platform.release(),
            "machine": platform.machine(),
        },
        "python": {
            "version": platform.python_version(),
            "executable": sys.executable,
        },
        "repository": repository,
        "openvmm": _openvmm(runner, root),
        "devices": devices,
        "openvmm_processes": processes,
        "nvx_rs": executable.exists(),
        "artifacts": _artifacts(root),
    }
    if not no_verify:
        document["verify"] = _verify(runner, root)
    if backend is not None:
        document["backend"] = backend_verdict(backend, system, devices)
    return document


def _write_text(document: dict[str, object]) -> None:
    os_info = cast(dict[str, object], document["os"])
    python_info = cast(dict[str, object], document["python"])
    repository = cast(dict[str, object], document["repository"])
    print(f"OS: {os_info['system']} {os_info['release']} ({os_info['machine']})")
    print(f"Python: {python_info['version']} ({python_info['executable']})")
    print(f"Repository: {repository['root']}")
    print(f"HEAD: {repository['head']} (branch: {repository['branch']})")
    dirty = cast(list[str], repository["dirty"])
    print(f"Dirty entries: {len(dirty)}")
    print(f"OpenVMM: {json.dumps(document['openvmm'], sort_keys=True)}")
    if "verify" in document:
        verify = cast(dict[str, object], document["verify"])
        print(f"Verify exit code: {verify['exit_code']}")
    print(f"Devices: {json.dumps(document['devices'], sort_keys=True)}")
    print(f"OpenVMM processes: {json.dumps(document['openvmm_processes'])}")
    print(f"nvx-rs built: {document['nvx_rs']}")
    artifacts = cast(dict[str, object], document["artifacts"])
    print(f"Artifacts: exists={artifacts['exists']} files={artifacts['files']}")
    if "backend" in document:
        verdict = cast(dict[str, object], document["backend"])
        state = "usable" if verdict["usable"] else "not usable"
        print(f"Backend {verdict['name']}: {state} - {verdict['reason']}")


def run_cli(argv: Sequence[str] | None, runner: Runner) -> int:
    """Run the probe command-line interface with an injectable runner."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backend", choices=BACKENDS)
    parser.add_argument("--no-verify", action="store_true")
    parser.add_argument("--json", action="store_true")
    parser.add_argument(
        "--repo",
        type=Path,
        help="NVX checkout to probe (default: the checkout holding this script)",
    )
    namespace = parser.parse_args(argv)
    backend = cast(str | None, namespace.backend)
    no_verify = cast(bool, namespace.no_verify)
    use_json = cast(bool, namespace.json)
    repository_root = cast(Path | None, namespace.repo)
    try:
        document = collect_probe(backend, no_verify, runner, repository_root)
    except (OSError, ProbeError, subprocess.SubprocessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    if use_json:
        json.dump(document, sys.stdout, indent=2)
        sys.stdout.write("\n")
    else:
        _write_text(document)
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    """Run the host probe."""
    return run_cli(argv, SubprocessRunner())


if __name__ == "__main__":
    raise SystemExit(main())
