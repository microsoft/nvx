"""Provision an Azure Windows scale-set instance as an NVX CI runner.

The SYSTEM task that staging registers runs this controller at every startup
until registration succeeds. It blocks instance metadata for jobs before it
reads any, prepares the data disk and the WHP and Hyper-V features across the
reboots that they need, and registers the Network Service runner with a token
that it passes to the generic setup on standard input only.
"""

import os
import re
import sys
import traceback
import uuid
from pathlib import Path, PureWindowsPath
from typing import cast

from . import common, windows_disk, windows_firewall, windows_system
from .common import BootstrapError, JsonObject, Log

ROOT = windows_system.ROOT
HOST_METADATA_ROOT = Path(r"C:\ProgramData\NvxCiHostMetadata")
FEATURES = ("HypervisorPlatform", "Microsoft-Hyper-V")
REQUIRED_LABELS = ("windows", "whp", "virtual-machine")
RESTART_REQUIRED = 3010
ENVIRONMENT_KEY = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment"
APPX_PATH = re.compile(r"\\WindowsApps\\Microsoft\.(VCLibs|UI\.Xaml)\.", re.I)


def write_host_snapshot(safe: JsonObject, root: Path) -> None:
    """Atomically replace the sanitized compute snapshot that jobs may read.

    The directory and any existing snapshot are protected before anything is
    written in it, so that no other account can change the new snapshot
    before it replaces the old one.
    """
    windows_system.require_regular_directory(root)
    target = root / "compute.json"
    windows_system.require_regular_file(target)
    existing = [str(root)] + ([str(target)] if target.exists() else [])
    windows_system.adapter("system.ps1", "ProtectHostMetadata", {"paths": existing})
    temporary = root / f"{uuid.uuid4().hex}.tmp"
    try:
        temporary.write_bytes(common.snapshot_bytes(safe))
        windows_system.adapter(
            "system.ps1", "ProtectHostMetadata", {"paths": [str(temporary)]}
        )
        os.replace(temporary, target)
        windows_system.adapter(
            "system.ps1", "ProtectHostMetadata", {"paths": [str(target)]}
        )
    finally:
        temporary.unlink(missing_ok=True)


def merge_machine_path(current: str, appx_paths: list[str]) -> str:
    """Put the winget dependencies that SYSTEM lacks first on a path.

    Older copies of those dependencies are dropped; other entries, including
    unexpanded variables, keep their order.
    """
    entries = [
        entry for entry in current.split(";") if entry and not APPX_PATH.search(entry)
    ]
    return ";".join([*appx_paths, *entries])


def update_machine_path(appx_paths: list[str]) -> str:
    """Update the machine path in place, keeping its registry value type.

    Returns the new expanded path, which this process and the setup inherit.
    """
    if sys.platform != "win32":
        raise BootstrapError("The machine path requires Windows")
    import winreg

    with winreg.OpenKey(
        winreg.HKEY_LOCAL_MACHINE,
        ENVIRONMENT_KEY,
        0,
        winreg.KEY_QUERY_VALUE | winreg.KEY_SET_VALUE,
    ) as key:
        current, kind = winreg.QueryValueEx(key, "Path")
        value = merge_machine_path(str(current), appx_paths)
        winreg.SetValueEx(key, "Path", 0, kind, value)
    expanded = winreg.ExpandEnvironmentStrings(value)
    _broadcast_environment_change()
    return expanded


def _broadcast_environment_change() -> None:
    if sys.platform != "win32":
        return
    import ctypes
    from ctypes import wintypes

    send = ctypes.WinDLL("user32").SendMessageTimeoutW
    send.argtypes = (
        wintypes.HWND,
        wintypes.UINT,
        wintypes.WPARAM,
        wintypes.LPCWSTR,
        wintypes.UINT,
        wintypes.UINT,
        ctypes.POINTER(ctypes.c_size_t),
    )
    send.restype = ctypes.c_ssize_t
    # The call writes a DWORD_PTR, which is pointer-sized.
    result = ctypes.c_size_t()
    # HWND_BROADCAST, WM_SETTINGCHANGE, SMTO_ABORTIFHUNG: what changing a
    # machine variable through .NET sends, so new logons see the path.
    send(0xFFFF, 0x001A, 0, "Environment", 0x0002, 5000, ctypes.byref(result))


def setup_arguments(config: JsonObject, extra_labels: str) -> list[str]:
    arguments = [
        "-RunnerOnly",
        "-RepositoryUrl",
        f"https://github.com/{common.text(config, 'repository')}",
        "-RunnerDirectory",
        common.text(config, "runnerDirectory"),
    ]
    if extra_labels:
        arguments += ["-ExtraRunnerLabels", extra_labels]
    scratch = config.get("benchmarkScratchDirectory")
    if isinstance(scratch, str) and scratch:
        arguments += ["-BenchmarkScratchDirectory", scratch]
    return arguments


def run_setup(
    arguments: list[str], output: Path, token: str | None = None, root: Path = ROOT
) -> int:
    """Run the generic setup, streaming its output; a token travels on stdin only."""
    # Truncate stale output before the child starts, then keep every line that
    # reaches either stream even if the task is stopped or the host reboots.
    with output.open("wb") as stream:
        result = common.run_command(
            [
                windows_system.powershell(),
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                str(PureWindowsPath(root) / "setup-windows-whp.ps1"),
                *arguments,
            ],
            stdin=b"" if token is None else (token + "\n").encode(),
            check=False,
            capture=False,
            output=stream,
            env=windows_system.powershell_environment(),
        )
    return result.returncode


def booted_since(marker: Path, boot: str) -> bool:
    """Return whether a reboot recorded in the marker has happened since."""
    return marker.exists() and marker.read_text(encoding="utf-8").strip() != boot


def enable_features(boot: str, root: Path, log: Log) -> bool:
    """Enable WHP and Hyper-V; False means that a reboot was scheduled."""
    marker = root / "feature-reboot-pending.txt"
    states = windows_system.adapter("system.ps1", "Features", {"names": list(FEATURES)})
    if not isinstance(states, list):
        raise BootstrapError("Windows reported no optional feature states")
    features = [
        common.json_object(state, "The feature state")
        for state in cast(list[object], states)
    ]
    if len(features) != len(FEATURES):
        raise BootstrapError("Windows reported no optional feature states")
    pending = [feature for feature in features if feature.get("state") != "Enabled"]
    if pending:
        if booted_since(marker, boot):
            raise BootstrapError(
                "WHP/Hyper-V features are still disabled after their reboot."
            )
        marker.write_text(boot, encoding="utf-8")
        names = [
            str(feature.get("name"))
            for feature in pending
            if feature.get("state") != "EnablePending"
        ]
        if names:
            windows_system.adapter("system.ps1", "EnableFeatures", {"names": names})
        windows_system.request_reboot(
            "Enabling WHP and Hyper-V for the CI runner.", log
        )
        return False
    if marker.exists():
        if not booted_since(marker, boot):
            windows_system.request_reboot(
                "Completing Windows virtualization feature enablement.", log
            )
            return False
        marker.unlink()
    return True


def bootstrap(config_path: Path, log: Log, root: Path = ROOT) -> None:
    config = common.load_json(config_path, "The configuration")
    if config.get("os") != "windows":
        raise BootstrapError("Expected a Windows runner configuration.")
    if not windows_system.task_exists():
        raise BootstrapError(
            "Runner continuation task is missing; stage the package before starting "
            "bootstrap."
        )
    # Jobs must never reach instance metadata, so block it before reading it.
    windows_firewall.ensure_metadata_block()
    metadata = common.compute_metadata()
    name = common.runner_name(config, metadata)
    write_host_snapshot(common.safe_compute_metadata(metadata), HOST_METADATA_ROOT)
    disk = common.json_object(config.get("dataDisk"), "The dataDisk configuration")
    letter = common.text(disk, "driveLetter", "dataDisk configuration")
    if letter.upper() == os.environ.get("SystemDrive", "C:").rstrip(":").upper():
        raise BootstrapError("Data disk must not use the system drive letter.")
    disk_log = Log(windows_disk.LOG_PATH)
    try:
        windows_disk.setup(
            common.integer(disk, "lun", "dataDisk configuration"),
            letter,
            common.text(disk, "temporaryDriveFallback", "dataDisk configuration"),
            disk_log,
        )
    except BootstrapError as error:
        disk_log(f"ERROR: {error}")
        raise BootstrapError(f"Managed data disk setup failed: {error}") from error
    if windows_system.registry_value(windows_disk.PENDING_KEY, "Completed") != 1:
        log("Data disk setup scheduled a reboot; runner setup continues at startup.")
        return
    volume = windows_disk.optional_result(
        windows_disk.storage("VolumeAtLetter", {"letter": letter})
    )
    if volume is None or volume.get("fileSystem") != "NTFS":
        raise BootstrapError("Managed NTFS data volume is unavailable.")
    boot = windows_system.boot_time()
    if not enable_features(boot, root, log):
        return
    windows_firewall.ensure_metadata_block()
    paths = windows_system.adapter("system.ps1", "AppxPaths")
    os.environ["PATH"] = update_machine_path(
        [str(path) for path in cast(list[object], paths)]
        if isinstance(paths, list)
        else []
    )
    extra_labels, benchmark = common.split_labels(
        common.instance_labels(config, name), REQUIRED_LABELS
    )
    arguments = setup_arguments(config, extra_labels)
    prerequisites = root / "prerequisite-reboot-pending.txt"
    result = run_setup(arguments, root / "setup-prewarm.log", root=root)
    if result == RESTART_REQUIRED:
        if booted_since(prerequisites, boot):
            raise BootstrapError(
                "Runner prerequisites still require a reboot after their previous reboot."
            )
        prerequisites.write_text(boot, encoding="utf-8")
        windows_system.request_reboot("Completing CI runner prerequisites.", log)
        return
    if result != 0:
        raise BootstrapError(
            f"Runner prerequisites failed: {result}; see setup-prewarm.log."
        )
    prerequisites.unlink(missing_ok=True)
    token = common.registration_token(config)
    registration = [*arguments, "-RunnerName", name, "-RunnerTokenStdin"]
    if benchmark:
        registration.append("-BenchmarkRunner")
    result = run_setup(registration, root / "setup-registration.log", token, root)
    del token
    if result == RESTART_REQUIRED:
        prerequisites.write_text(boot, encoding="utf-8")
        windows_system.request_reboot(
            "Completing CI runner registration prerequisites.", log
        )
        return
    if result != 0:
        raise BootstrapError(
            f"Runner registration failed: {result}; see setup-registration.log."
        )
    windows_system.adapter(
        "system.ps1", "UnregisterTask", {"name": windows_system.CONTINUATION_TASK}
    )
    log.marker("NVX_CLOUD_RUNNER_SETUP_COMPLETE")


def main(config_path: Path, root: Path = ROOT) -> int:
    log = Log(root / "setup-runner.log")
    log("=== Windows runner bootstrap starting ===")
    try:
        with windows_system.exclusive(root / "bootstrap.lock"):
            bootstrap(config_path, log, root)
    except BaseException as error:
        # The task fails, so its retries run bootstrap again; the extension's
        # staging result is already final.
        log.marker("NVX_CLOUD_RUNNER_SETUP_FAILED")
        if not isinstance(error, BootstrapError):
            log(
                "".join(
                    traceback.format_exception(type(error), error, error.__traceback__)
                ).rstrip()
            )
        log(f"ERROR: {error}")
        raise
    return 0
