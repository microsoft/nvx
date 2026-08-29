"""Helpers used by nvx continuous-integration jobs."""

from __future__ import annotations

import os
import shutil
import zipfile
from pathlib import Path

from .common import (
    BUILD_DIR,
    OPENVMM_DIR,
    ScriptError,
    download,
    require_file,
    require_tool,
    run_checked,
)

ZSTD_VERSION = "1.5.7"
ZSTD_ARCHIVE = f"zstd-v{ZSTD_VERSION}-win64.zip"
ZSTD_URL = (
    f"https://github.com/facebook/zstd/releases/download/v{ZSTD_VERSION}/{ZSTD_ARCHIVE}"
)
ZSTD_SHA256 = "acb4e8111511749dc7a3ebedca9b04190e37a17afeb73f55d4425dbf0b90fad9"
OPENVMM_TEST_BACKENDS = ("kvm", "mshv", "whp")
OPENVMM_MICROVM_TEST_FILTER = (
    "test(x86_64::microvm) + test(test_ttrpc_microvm_snapshot_restore) + "
    "test(test_ttrpc_microvm_v3_smp_snapshot_restore)"
)


def run_openvmm_tests(backend: str) -> None:
    if backend not in OPENVMM_TEST_BACKENDS:
        choices = ", ".join(OPENVMM_TEST_BACKENDS)
        raise ScriptError(
            f"unsupported OpenVMM test backend {backend!r}; choose {choices}"
        )

    if backend == "whp":
        if os.name != "nt":
            raise ScriptError("WHP OpenVMM tests require Windows")
    else:
        if os.name == "nt":
            raise ScriptError(f"{backend.upper()} OpenVMM tests require Linux")
        device = Path("/dev") / backend
        if not os.access(device, os.R_OK | os.W_OK):
            raise ScriptError(f"OpenVMM tests require read/write access to {device}")
        if backend == "kvm" and Path("/dev/mshv").exists():
            raise ScriptError(
                "/dev/mshv is present, so OpenVMM would select MSHV instead of KVM"
            )

    require_file(OPENVMM_DIR / "Cargo.toml", "initialized OpenVMM submodule")
    kernel = require_file(BUILD_DIR / "vmlinux", "microVM PVH kernel").resolve()
    initrd = require_file(
        BUILD_DIR / "initramfs.cpio.gz",
        "microVM Alpine initramfs",
    ).resolve()
    env = os.environ.copy()
    env["OPENVMM_MICROVM_PVH_KERNEL"] = os.fspath(kernel)
    env["OPENVMM_MICROVM_PVH_INITRD"] = os.fspath(initrd)
    capabilities = [
        capability.strip()
        for capability in env.get("PETRI_CAPABILITIES", "").split(",")
        if capability.strip()
    ]
    if "microvm_pvh" not in capabilities:
        capabilities.append("microvm_pvh")
    env["PETRI_CAPABILITIES"] = ",".join(capabilities)
    cargo = require_tool("cargo")

    run_checked(
        [cargo, "xflowey", "restore-packages", "--no-compat-igvm"],
        cwd=OPENVMM_DIR,
        env=env,
    )
    run_checked(
        [
            cargo,
            "xflowey",
            "vmm-tests-run",
            "--release",
            "--ci-profile",
            "--skip-vhd-prompt",
            "--filter",
            OPENVMM_MICROVM_TEST_FILTER,
        ],
        cwd=OPENVMM_DIR,
        env=env,
    )


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
    require_file(gnu_tar, "Git for Windows GNU tar")

    archive = runner_temp / ZSTD_ARCHIVE
    download(ZSTD_URL, archive, expected_sha256=ZSTD_SHA256)

    destination = runner_temp / f"zstd-v{ZSTD_VERSION}-win64"
    shutil.rmtree(destination, ignore_errors=True)
    with zipfile.ZipFile(archive) as package:
        package.extractall(destination)
    zstd = destination / f"zstd-v{ZSTD_VERSION}-win64" / "zstd.exe"
    require_file(zstd, "zstd.exe")

    with github_path.open("a", encoding="utf-8", newline="") as output:
        output.write(f"{gnu_tar.parent}{os.linesep}")
        output.write(f"{zstd.parent}{os.linesep}")

    run_checked([gnu_tar, "--version"])
    run_checked([zstd, "--version"])
