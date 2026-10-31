"""Stage the Windows runner bootstrap from the Custom Script Extension.

The extension's entry shim installs the pinned interpreter into the trusted
bootstrap directory, verifies the package digest, and runs this command from
the package. It configures OpenSSH for the administrator, verifies and
extracts the package next to the interpreter, and hands the long-running
bootstrap to an independent SYSTEM task, so the extension's execution budget
never limits provisioning.
"""

import hashlib
import os
import re
import shutil
import sys
import tarfile
import uuid
from pathlib import Path, PureWindowsPath

from . import common, windows_system
from .common import BootstrapError, JsonObject, Log

LOG_PATH = Path(r"C:\WindowsAzure\Logs\vmss-bootstrap.log")
# The shim runs this module from its separately extracted, verified copy.
STAGING_ROOT = Path(__file__).resolve().parents[1]
OPENSSH_KEY = r"SOFTWARE\OpenSSH"
MODULES = (
    "__init__.py",
    "cli.py",
    "common.py",
    "windows_bootstrap.py",
    "windows_disk.py",
    "windows_firewall.py",
    "windows_stage.py",
    "windows_system.py",
)
PACKAGE_FILES = frozenset(
    {
        "__main__.py",
        "config.json",
        "setup-windows-whp.ps1",
        "tool-versions.conf",
        "firewall.ps1",
        "openssh.ps1",
        "storage.ps1",
        "system.ps1",
        *(f"nvx_azure/{module}" for module in MODULES),
    }
)
SHELLS = {
    "powershell": r"System32\WindowsPowerShell\v1.0\powershell.exe",
    "cmd": r"System32\cmd.exe",
}
TASK_HOURS = 4
TASK_RESTARTS = 60
TASK_RESTART_MINUTES = 2
LEGACY_DISK_TASK = "NvxRunner-SetupDataDisk"


def public_keys(payload: JsonObject) -> list[str]:
    """Return the deployed keys, one per line, without comments or duplicates."""
    keys = common.strings(payload, "publicKeys", "payload")
    lines = (line.strip() for key in keys for line in key.splitlines())
    return list(
        dict.fromkeys(line for line in lines if line and not line.startswith("#"))
    )


def verify_package(package: Path, digest: str) -> None:
    """Check the package digest and that it holds exactly the expected files."""
    if not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise BootstrapError("The payload has no valid package digest")
    hasher = hashlib.sha256()
    with package.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            hasher.update(chunk)
    if hasher.hexdigest() != digest:
        raise BootstrapError("Runner package checksum mismatch.")
    with tarfile.open(package, mode="r:xz") as archive:
        members = archive.getmembers()
    names = [member.name for member in members]
    if (
        len(names) != len(set(names))
        or frozenset(names) != PACKAGE_FILES
        or not all(member.isreg() for member in members)
    ):
        raise BootstrapError(
            "Runner package contains unexpected or duplicate file names."
        )


def extract_package(package: Path, root: Path) -> None:
    """Replace each staged file atomically with the package's copy.

    Targets must be regular files and directories, so a link planted before
    the directory was protected cannot redirect a write.
    """
    windows_system.require_regular_directory(root)
    with tarfile.open(package, mode="r:xz") as archive:
        for member in archive.getmembers():
            source = archive.extractfile(member)
            if member.name not in PACKAGE_FILES or source is None:
                raise BootstrapError(f"Unexpected package member: {member.name}")
            target = root.joinpath(*member.name.split("/"))
            windows_system.require_regular_directory(target.parent)
            windows_system.require_regular_file(target)
            temporary = target.with_name(f"{uuid.uuid4().hex}.tmp")
            try:
                with source, temporary.open("xb") as output:
                    shutil.copyfileobj(source, output)
                os.replace(temporary, target)
            finally:
                temporary.unlink(missing_ok=True)


def shell_path(shell: str, log: Log) -> str:
    system_root = os.environ.get("SystemRoot", r"C:\Windows")
    if shell == "pwsh":
        found = shutil.which("pwsh.exe")
        if found:
            return found
        log("WARN: pwsh.exe not found, falling back to Windows PowerShell")
        shell = "powershell"
    if shell not in SHELLS:
        raise BootstrapError(f"Unsupported SSH default shell: {shell}")
    return str(PureWindowsPath(system_root) / SHELLS[shell])


def configure_openssh(payload: JsonObject, root: Path, log: Log) -> None:
    prepared = common.json_object(
        windows_system.adapter("openssh.ps1", "Prepare", root=root), "OpenSSH state"
    )
    if prepared.get("warning"):
        log(f"WARN: OpenSSH capability check failed: {prepared['warning']}")
    if not prepared.get("sshd"):
        raise BootstrapError(
            "OpenSSH Server is not available on this image. Use an image that "
            "supports OpenSSH Server."
        )
    log(
        f"OpenSSH capability {prepared.get('capability')}; services {prepared.get('services')}"
    )
    shell = common.text(payload, "defaultShell", "payload")
    path = shell_path(shell, log)
    option = "/c" if shell == "cmd" else "-Command"
    windows_system.set_registry_value(OPENSSH_KEY, "DefaultShell", path)
    windows_system.set_registry_value(OPENSSH_KEY, "DefaultShellCommandOption", option)
    log(f"Default SSH shell set to {path} (command option {option})")
    keys = public_keys(payload)
    if keys:
        # The deployed key set is authoritative: replacing the file is what
        # revokes a key removed from the configuration.
        ssh_data = Path(os.environ.get("ProgramData", r"C:\ProgramData")) / "ssh"
        # sshd creates its data directory with a protected ACL when it first
        # starts. One created here would inherit ProgramData's ACL, which lets
        # local users add configuration files.
        if not ssh_data.is_dir() or windows_system.is_reparse_point(ssh_data):
            raise BootstrapError(
                f"OpenSSH Server's data directory {ssh_data} is missing or not "
                "a regular directory"
            )
        target = ssh_data / "administrators_authorized_keys"
        windows_system.require_regular_file(target)
        temporary = target.with_name(target.name + ".new")
        # sshd rejects key files with a byte order mark.
        temporary.write_bytes(("\n".join(keys) + "\n").encode())
        windows_system.adapter(
            "system.ps1",
            "ProtectAuthorizedKeys",
            {"path": str(temporary)},
            root=root,
        )
        os.replace(temporary, target)
        windows_system.adapter(
            "system.ps1", "ProtectAuthorizedKeys", {"path": str(target)}, root=root
        )
        log(f"Authorized keys file set to the {len(keys)} deployed key(s)")
    else:
        log(
            "WARN: No public keys supplied; existing authorized keys left untouched "
            "and password authentication remains the only option"
        )
    rule = common.json_object(
        windows_system.adapter("openssh.ps1", "AllowInbound", root=root), "rule"
    )
    log(
        "Created inbound firewall rule for TCP 22"
        if rule.get("created")
        else "Inbound firewall rule for TCP 22 already present and enabled"
    )
    restarted = common.json_object(
        windows_system.adapter("openssh.ps1", "Restart", root=root), "sshd"
    )
    log(f"sshd status: {restarted.get('status')}")


def hand_off(python: str, root: Path, log: Log) -> None:
    """Register and start the SYSTEM task that runs the runner bootstrap."""
    # Earlier bootstraps continued disk setup through a task of their own;
    # the runner bootstrap now resumes it, so that task must not run as well.
    if (
        windows_system.adapter(
            "system.ps1", "TaskExists", {"name": LEGACY_DISK_TASK}, root=root
        )
        is True
    ):
        windows_system.adapter(
            "system.ps1", "UnregisterTask", {"name": LEGACY_DISK_TASK}, root=root
        )
        log(f"Removed the legacy {LEGACY_DISK_TASK} task")
    directory = PureWindowsPath(root)
    windows_system.adapter(
        "system.ps1",
        "RegisterTask",
        {
            "name": windows_system.CONTINUATION_TASK,
            "execute": python,
            "arguments": (
                f'-I -B "{directory}" windows-bootstrap '
                f'--config "{directory / "config.json"}"'
            ),
            "workingDirectory": str(directory),
            "executionHours": TASK_HOURS,
            "restartCount": TASK_RESTARTS,
            "restartMinutes": TASK_RESTART_MINUTES,
        },
        root=root,
    )
    windows_system.adapter(
        "system.ps1", "StartTask", {"name": windows_system.CONTINUATION_TASK}, root=root
    )
    log.marker("NVX_CLOUD_RUNNER_BOOTSTRAP_STAGED")


def stage(payload_path: Path, package: Path, root: Path, log: Log, python: str) -> None:
    payload = common.load_json(payload_path, "The payload")
    log(
        f"=== vmss bootstrap staging (admin={common.text(payload, 'adminUsername', 'payload')}, "
        f"shell={common.text(payload, 'defaultShell', 'payload')}) ==="
    )
    descriptor = common.json_object(payload.get("package"), "The package descriptor")
    verify_package(package, common.text(descriptor, "sha256", "package descriptor"))
    # The shim already extracted this verified package into STAGING_ROOT. Use
    # those adapters first, so a stale persistent target cannot prevent SSH
    # from being available to diagnose the extraction failure.
    configure_openssh(payload, STAGING_ROOT, log)
    extract_package(package, root)
    log(f"Staged the verified runner package in {root}")
    hand_off(python, root, log)


def main(payload_path: Path, package: Path) -> int:
    LOG_PATH.parent.mkdir(parents=True, exist_ok=True)
    log = Log(LOG_PATH)
    root = package.parent
    try:
        stage(payload_path, package, root, log, sys.executable)
    except BaseException as error:
        log(f"ERROR: {error}")
        raise
    return 0
