"""Helpers used by nvx continuous-integration jobs."""

from __future__ import annotations

import hashlib
import os
import shutil
import zipfile
from pathlib import Path

from flamegraph_host import _find_xperf

from .common import (
    ScriptError,
    diagnostic_tail,
    download,
    require_file,
    require_tool,
    run_capture,
    run_checked,
)


ZSTD_VERSION = "1.5.7"
ZSTD_ARCHIVE = f"zstd-v{ZSTD_VERSION}-win64.zip"
ZSTD_URL = (
    f"https://github.com/facebook/zstd/releases/download/v{ZSTD_VERSION}/{ZSTD_ARCHIVE}"
)
ZSTD_SHA256 = "acb4e8111511749dc7a3ebedca9b04190e37a17afeb73f55d4425dbf0b90fad9"
LESSMSI_VERSION = "2.12.9"
LESSMSI_ARCHIVE = f"lessmsi-v{LESSMSI_VERSION}.zip"
LESSMSI_URL = (
    f"https://github.com/activescott/lessmsi/releases/download/v{LESSMSI_VERSION}/"
    f"{LESSMSI_ARCHIVE}"
)
LESSMSI_SHA256 = "5b4e187e74b184ad3a63ccf06c3d17dae2b8c4b6c298a996dbd51a9f6db29d21"
WINDOWS_WPT_VERSION = "10.1.26100.2454"
WINDOWS_WPT_BASE_URL = (
    "https://download.microsoft.com/download/2/d/9/"
    "2d9c8902-3fcd-48a6-a22a-432b08bed61e/ADK/Installers/"
)
WINDOWS_WPT_PACKAGES = (
    (
        f"wpt-x64-desktop-{WINDOWS_WPT_VERSION}.msi",
        f"{WINDOWS_WPT_BASE_URL}WPTx64%20%28DesktopEditions%29-x86_en-us.msi",
        "230590091a26cdf32ea924c99d33bcb9a68d155b60695e51a45713bc1124ada9",
    ),
    (
        f"wpt-x64-onecore-{WINDOWS_WPT_VERSION}.msi",
        f"{WINDOWS_WPT_BASE_URL}WPTx64%20%28OnecoreUAP%29-x86_en-us.msi",
        "30926e55bee618143a65199335d3798a51dd5916d2dc653167603df943c0501d",
    ),
)
WINDOWS_WPT_EXTRACT_TIMEOUT_SECONDS = 2 * 60
WINDOWS_WPT_RELATIVE_DIRECTORY = Path(
    "SourceDir", "Windows Kits", "10", "Windows Performance Toolkit"
)


def _require_sha256(path: Path, expected: str) -> None:
    observed = hashlib.sha256(path.read_bytes()).hexdigest()
    if observed.lower() != expected.lower():
        raise ScriptError(f"{path.name} SHA-256 mismatch: {observed}")


def setup_cross_os_cache() -> None:
    github_path_value = os.environ.get("GITHUB_PATH")
    runner_temp_value = os.environ.get("RUNNER_TEMP")
    if not github_path_value or not runner_temp_value:
        raise ScriptError(
            "GITHUB_PATH and RUNNER_TEMP are required; run this inside GitHub Actions"
        )
    if os.name != "nt":
        raise ScriptError("cross-OS cache setup requires Windows")

    github_path = Path(github_path_value)
    runner_temp = Path(runner_temp_value)
    git = Path(require_tool("git.exe", "Git for Windows is required"))
    gnu_tar = git.parent.parent / "usr" / "bin" / "tar.exe"
    require_file(gnu_tar, f"Git for Windows GNU tar is missing at {gnu_tar}")

    archive = runner_temp / ZSTD_ARCHIVE
    download(ZSTD_URL, archive)
    _require_sha256(archive, ZSTD_SHA256)

    destination = runner_temp / f"zstd-v{ZSTD_VERSION}-win64"
    shutil.rmtree(destination, ignore_errors=True)
    with zipfile.ZipFile(archive) as package:
        package.extractall(destination)
    zstd = destination / f"zstd-v{ZSTD_VERSION}-win64" / "zstd.exe"
    require_file(zstd, f"zstd.exe is missing from {destination}")

    with github_path.open("a", encoding="utf-8", newline="") as output:
        output.write(f"{gnu_tar.parent}{os.linesep}")
        output.write(f"{zstd.parent}{os.linesep}")

    run_checked([gnu_tar, "--version"])
    run_checked([zstd, "--version"])


def _is_windows() -> bool:
    return os.name == "nt"


def setup_windows_performance_toolkit() -> None:
    github_path_value = os.environ.get("GITHUB_PATH")
    runner_temp_value = os.environ.get("RUNNER_TEMP")
    if not github_path_value or not runner_temp_value:
        raise ScriptError(
            "GITHUB_PATH and RUNNER_TEMP are required; run this inside GitHub Actions"
        )
    if not _is_windows():
        raise ScriptError("Windows Performance Toolkit setup requires Windows")

    github_path = Path(github_path_value)
    runner_temp = Path(runner_temp_value)
    xperf = _find_xperf()
    if xperf is None:
        lessmsi_archive = runner_temp / LESSMSI_ARCHIVE
        lessmsi_root = runner_temp / f"lessmsi-v{LESSMSI_VERSION}"
        destination = runner_temp / f"windows-performance-toolkit-{WINDOWS_WPT_VERSION}"
        print(f">> downloading Windows Performance Toolkit {WINDOWS_WPT_VERSION}")
        download(LESSMSI_URL, lessmsi_archive)
        _require_sha256(lessmsi_archive, LESSMSI_SHA256)
        shutil.rmtree(lessmsi_root, ignore_errors=True)
        with zipfile.ZipFile(lessmsi_archive) as package:
            package.extractall(lessmsi_root)
        lessmsi = lessmsi_root / "lessmsi.exe"
        require_file(lessmsi, f"lessmsi.exe is missing from {lessmsi_root}")
        shutil.rmtree(destination, ignore_errors=True)
        for index, (filename, url, expected_sha256) in enumerate(WINDOWS_WPT_PACKAGES):
            package = runner_temp / filename
            download(url, package)
            _require_sha256(package, expected_sha256)
            mode = "x" if index == 0 else "xo"
            command = [lessmsi, mode, package, f"{destination}\\"]
            try:
                result = run_capture(
                    command,
                    timeout=WINDOWS_WPT_EXTRACT_TIMEOUT_SECONDS,
                )
            except OSError as error:
                raise ScriptError(f"failed to start lessmsi.exe: {error}") from error
            detail = diagnostic_tail(result.text)
            suffix = f"\n{detail}" if detail else ""
            if result.timed_out:
                raise ScriptError(
                    f"Windows Performance Toolkit extraction of {package.name} "
                    f"timed out after {WINDOWS_WPT_EXTRACT_TIMEOUT_SECONDS} seconds"
                    f"{suffix}"
                )
            if result.returncode != 0:
                raise ScriptError(
                    f"Windows Performance Toolkit extraction of {package.name} "
                    f"failed with exit {result.returncode}{suffix}"
                )
        xperf_path = destination / WINDOWS_WPT_RELATIVE_DIRECTORY / "xperf.exe"
        require_file(
            xperf_path,
            "Windows Performance Toolkit extraction did not produce xperf.exe",
        )
        xperf = str(xperf_path)

    xperf_path = Path(xperf)
    run_checked([xperf_path, "-help", "dumper"])
    with github_path.open("a", encoding="utf-8", newline="") as output:
        output.write(f"{xperf_path.parent}{os.linesep}")
    print(f">> Windows Performance Toolkit ready: {xperf_path}")
