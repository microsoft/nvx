"""Helpers used by nvx continuous-integration jobs."""

from __future__ import annotations

import os
import platform
import shutil
import tarfile
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
NEXTEST_VERSION = "0.9.133"
NEXTEST_RELEASE_URL = (
    "https://github.com/nextest-rs/nextest/releases/download/"
    f"cargo-nextest-{NEXTEST_VERSION}"
)
NEXTEST_TARGETS = {
    ("linux", "aarch64"): (
        "aarch64-unknown-linux-gnu",
        "8e4d241c78f9cbf5ca8597b13004f0441c18af484ea105b8f83b44a716c82d3d",
    ),
    ("linux", "x86_64"): (
        "x86_64-unknown-linux-gnu",
        "a9f992321e8759818400d93abb9477b4b11422d18d216e8d208505bd73454103",
    ),
    ("windows", "aarch64"): (
        "aarch64-pc-windows-msvc",
        "7a9b714bd5879db124e8d8a4e4e5959295345678456af61741a516d17f6e4a69",
    ),
    ("windows", "x86_64"): (
        "x86_64-pc-windows-msvc",
        "7a2ecd620bb377255b1de642b6587b73efb4dc6d35beaf13cabf52bdb7ca2ba9",
    ),
}
OPENVMM_TEST_BACKENDS = ("kvm", "mshv", "whp")
OPENVMM_MICROVM_TEST_FILTER = "test(x86_64::microvm)"


def _nextest_target() -> tuple[str, str]:
    system = platform.system().lower()
    machine = platform.machine().lower()
    architecture = {
        "amd64": "x86_64",
        "arm64": "aarch64",
    }.get(machine, machine)
    try:
        return NEXTEST_TARGETS[(system, architecture)]
    except KeyError as error:
        raise ScriptError(
            f"cargo-nextest is unsupported on {system}/{architecture}"
        ) from error


def setup_cargo_nextest(env: dict[str, str]) -> None:
    target, expected_sha256 = _nextest_target()
    archive_name = f"cargo-nextest-{NEXTEST_VERSION}-{target}.tar.gz"
    tools_root = Path(env.get("RUNNER_TEMP", BUILD_DIR / "tools"))
    destination = tools_root / f"cargo-nextest-{NEXTEST_VERSION}-{target}"
    binary = destination / ("cargo-nextest.exe" if os.name == "nt" else "cargo-nextest")
    archive = destination / archive_name
    temporary_binary = binary.with_suffix(f"{binary.suffix}.part")

    destination.mkdir(parents=True, exist_ok=True)
    download(
        f"{NEXTEST_RELEASE_URL}/{archive_name}",
        archive,
        expected_sha256=expected_sha256,
    )
    try:
        with tarfile.open(archive, "r:gz") as package:
            members = [
                member
                for member in package.getmembers()
                if member.isfile() and Path(member.name).name == binary.name
            ]
            if len(members) != 1:
                raise ScriptError(
                    f"{archive_name} does not contain exactly one {binary.name}"
                )
            source = package.extractfile(members[0])
            if source is None:
                raise ScriptError(f"could not read {binary.name} from {archive_name}")
            with source, temporary_binary.open("wb") as output:
                shutil.copyfileobj(source, output)
    except tarfile.TarError as error:
        raise ScriptError(f"could not extract {archive_name}: {error}") from error
    finally:
        archive.unlink(missing_ok=True)

    temporary_binary.chmod(0o755)
    temporary_binary.replace(binary)
    env["PATH"] = os.pathsep.join((os.fspath(destination), env.get("PATH", "")))
    run_checked([binary, "--version"], env=env)


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
    cargo = require_tool("cargo")
    setup_cargo_nextest(env)

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
