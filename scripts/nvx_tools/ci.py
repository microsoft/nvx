"""Helpers used by nvx continuous-integration jobs."""

from __future__ import annotations

import os
import re
import shutil
import zipfile
from pathlib import Path

from .common import (
    OPENVMM_DIR,
    ScriptError,
    download,
    require_file,
    require_success,
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
OPENVMM_TEST_BACKENDS = ("kvm", "mshv", "whp")
OPENVMM_RUST_TOOLCHAIN = "stable"
OPENVMM_GUEST_RUST_TARGET = "x86_64-unknown-none"
OPENVMM_UEFI_RUST_TARGET = "x86_64-unknown-uefi"
OPENVMM_LINUX_MUSL_RUST_TARGET = "x86_64-unknown-linux-musl"
OPENVMM_RUST_TARGETS = {
    "kvm": (
        OPENVMM_GUEST_RUST_TARGET,
        OPENVMM_UEFI_RUST_TARGET,
        OPENVMM_LINUX_MUSL_RUST_TARGET,
    ),
    "mshv": (
        OPENVMM_GUEST_RUST_TARGET,
        OPENVMM_UEFI_RUST_TARGET,
        OPENVMM_LINUX_MUSL_RUST_TARGET,
    ),
    "whp": (
        OPENVMM_GUEST_RUST_TARGET,
        OPENVMM_UEFI_RUST_TARGET,
    ),
}
OPENVMM_UNIT_TEST_EXCLUDED_PACKAGES = (
    "vmm_tests",
    "cca_tests",
    "guest_test_uefi",
    "inspect_derive",
    "mesh_derive",
    "save_restore_derive",
    "test_with_tracing_macro",
    "pal_async_test",
    "vmm_test_macros",
    "flowey_core",
)
OPENVMM_LINUX_TEST_FILTER = "test(openvmm) | test(ttrpc)"


def _exact_openvmm_test(test: str) -> str:
    return f"test(/^{re.escape(test)}$/)"


def _exclude_openvmm_tests(
    test_filter: str,
    excluded_tests: tuple[str, ...],
) -> str:
    exclusions = "".join(f" & !{_exact_openvmm_test(test)}" for test in excluded_tests)
    return f"({test_filter}){exclusions}"


# KVM runs Linux guests; the prepped Windows test also stages a 30 GiB VHD.
OPENVMM_KVM_EXCLUDED_TESTS = (
    "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_boot_no_agent",
    "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_iso_boot_no_agent",
    "multiarch::openvmm_pcat_x64_ubuntu_2404_server_x64_boot",
    "multiarch::openvmm_pcat_x64_ubuntu_2504_server_x64_boot",
    "multiarch::openvmm_pcat_x64_ubuntu_2504_server_x64_boot_heavy",
)
OPENVMM_KVM_TEST_FILTER = _exclude_openvmm_tests(
    f"({OPENVMM_LINUX_TEST_FILTER})"
    " & !test(no_vmbus_prepped_boot_no_vmbus_windows)"
    " & !test(windows_datacenter_core_2022_x64)"
    " & !test(virtio_net_windows)",
    OPENVMM_KVM_EXCLUDED_TESTS,
)
# MSHV runs Linux guests; the remaining exact exclusions fail on the runner.
OPENVMM_MSHV_EXCLUDED_TESTS = (
    "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_boot_no_agent",
    "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_iso_boot_no_agent",
    "multiarch::openvmm_pcat_x64_ubuntu_2404_server_x64_boot",
    "multiarch::openvmm_pcat_x64_ubuntu_2504_server_x64_boot",
    "multiarch::openvmm_pcat_x64_ubuntu_2504_server_x64_boot_heavy",
    "multiarch::pcie::openvmm_linux_x64_pcie_save_restore",
    "x86_64::openvmm_linux_x64_virtio_blk_device",
)
OPENVMM_MSHV_TEST_FILTER = _exclude_openvmm_tests(
    f"({OPENVMM_LINUX_TEST_FILTER}) & !test(windows_datacenter_core_2022_x64)",
    OPENVMM_MSHV_EXCLUDED_TESTS,
)
OPENVMM_WHP_TESTS = (
    "multiarch::hibernate::openvmm_uefi_x64_guest_test_x64_hibernate_halts",
    "multiarch::ic::openvmm_uefi_x64_windows_datacenter_core_2022_x64_kvp_ic",
    "multiarch::ic::openvmm_uefi_x64_windows_datacenter_core_2022_x64_timesync_ic",
    "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_boot_no_agent",
    "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_iso_boot_no_agent",
    "multiarch::openvmm_pcat_x64_windows_datacenter_core_2022_x64_boot",
    "multiarch::openvmm_pcat_x64_windows_datacenter_core_2022_x64_boot_heavy",
    "multiarch::openvmm_uefi_x64_frontpage",
    "multiarch::openvmm_uefi_x64_guest_test_x64_guest_test_uefi",
    "multiarch::openvmm_uefi_x64_ubuntu_2504_server_x64_secure_boot_mismatched_template",
    "multiarch::openvmm_uefi_x64_windows_datacenter_core_2022_x64_boot",
    "multiarch::openvmm_uefi_x64_windows_datacenter_core_2022_x64_boot_heavy",
    "multiarch::openvmm_uefi_x64_windows_datacenter_core_2022_x64_boot_nvme",
    "multiarch::openvmm_uefi_x64_windows_datacenter_core_2022_x64_secure_boot",
    "multiarch::openvmm_uefi_x64_windows_datacenter_core_2022_x64_secure_boot_mismatched_template",
    "multiarch::pcie::openvmm_uefi_x64_windows_datacenter_core_2022_x64_pcie_hotplug",
    "multiarch::pcie::openvmm_uefi_x64_windows_datacenter_core_2022_x64_pcie_nvme_boot",
    "multiarch::pcie::openvmm_uefi_x64_windows_datacenter_core_2022_x64_pcie_root_emulation_multi_segment",
    "multiarch::pcie::openvmm_uefi_x64_windows_datacenter_core_2022_x64_pcie_root_emulation_single_segment",
    "multiarch::pcie::openvmm_uefi_x64_windows_datacenter_core_2022_x64_pcie_switches",
    "multiarch::vmgs::openvmm_uefi_x64_ubuntu_2504_server_x64_invalid_boot_entries",
    "multiarch::vmgs::openvmm_uefi_x64_windows_datacenter_core_2022_x64_clear_vmgs",
    "multiarch::vmgs::openvmm_uefi_x64_windows_datacenter_core_2022_x64_default_boot",
    "multiarch::vmgs::openvmm_uefi_x64_windows_datacenter_core_2022_x64_invalid_boot_entries",
    "ttrpc::test_ttrpc_microvm_pvh_snapshot",
    "ttrpc::test_ttrpc_uefi_boot",
    "x86_64::microvm::openvmm_microvm_test_pvh_x64_phase_1_lifecycle",
    "x86_64::openvmm_uefi_x64_guest_test_x64_crash_dump_on_triple_fault",
    "x86_64::openvmm_uefi_x64_windows_datacenter_core_2022_x64_battery_capacity",
)
OPENVMM_WHP_EXCLUDED_TESTS = (
    "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_iso_boot_no_agent",
    "multiarch::openvmm_pcat_x64_windows_datacenter_core_2022_x64_boot",
    "multiarch::openvmm_pcat_x64_windows_datacenter_core_2022_x64_boot_heavy",
)


def _join_openvmm_tests(
    tests: tuple[str, ...],
    excluded_tests: tuple[str, ...] = (),
) -> str:
    test_filter = " | ".join(_exact_openvmm_test(test) for test in tests)
    exclusions = "".join(f" & !{_exact_openvmm_test(test)}" for test in excluded_tests)
    return f"({test_filter}){exclusions}"


OPENVMM_TEST_FILTERS = {
    "kvm": OPENVMM_KVM_TEST_FILTER,
    "mshv": OPENVMM_MSHV_TEST_FILTER,
    "whp": _join_openvmm_tests(OPENVMM_WHP_TESTS, OPENVMM_WHP_EXCLUDED_TESTS),
}


def validate_openvmm_test_backend(backend: str) -> None:
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


def _prepare_openvmm_test_environment(
    backend: str,
    rustup: str,
) -> dict[str, str]:
    targets = OPENVMM_RUST_TARGETS[backend]
    installed = run_capture(
        [
            rustup,
            "target",
            "list",
            "--installed",
            "--toolchain",
            OPENVMM_RUST_TOOLCHAIN,
        ]
    )
    require_success(installed, "installed Rust target query")
    installed_targets = set(installed.stdout.decode("utf-8").splitlines())

    environment = os.environ.copy()
    environment["RUSTUP_TOOLCHAIN"] = OPENVMM_RUST_TOOLCHAIN
    runner_temp_value = os.environ.get("RUNNER_TEMP")
    if not installed_targets.issuperset(targets):
        if runner_temp_value:
            environment["RUSTUP_HOME"] = os.fspath(
                Path(runner_temp_value) / "openvmm-rustup"
            )
            run_checked(
                [
                    rustup,
                    "toolchain",
                    "install",
                    OPENVMM_RUST_TOOLCHAIN,
                    "--profile",
                    "minimal",
                ],
                env=environment,
            )

        run_checked(
            [
                rustup,
                "target",
                "add",
                *targets,
                "--toolchain",
                OPENVMM_RUST_TOOLCHAIN,
            ],
            env=environment,
        )

    if backend != "whp" and runner_temp_value:
        environment["XDG_CACHE_HOME"] = os.fspath(
            Path(runner_temp_value) / "openvmm-cache"
        )

    return environment


def run_openvmm_unit_tests() -> None:
    require_file(OPENVMM_DIR / "Cargo.toml", "initialized OpenVMM submodule")
    cargo = require_tool("cargo")

    fuzz_crates = run_capture(
        [cargo, "xtask", "fuzz", "list", "--crates"],
        cwd=OPENVMM_DIR,
    )
    require_success(fuzz_crates, "OpenVMM fuzz crate query")

    run_checked(
        [cargo, "xflowey", "restore-packages", "--no-compat-igvm"],
        cwd=OPENVMM_DIR,
    )
    command = [
        cargo,
        "nextest",
        "run",
        "--profile",
        "agent",
        "--workspace",
        "--tests",
        "--bins",
        "--features",
        "ci",
    ]
    excluded_packages = (
        *OPENVMM_UNIT_TEST_EXCLUDED_PACKAGES,
        *fuzz_crates.stdout.decode("utf-8").splitlines(),
    )
    for package in excluded_packages:
        command.extend(("--exclude", package))

    run_checked(command, cwd=OPENVMM_DIR)
    run_checked(
        [
            cargo,
            "test",
            "--locked",
            "--doc",
            "--workspace",
            "--no-fail-fast",
        ],
        cwd=OPENVMM_DIR,
    )


def run_openvmm_tests(backend: str) -> None:
    validate_openvmm_test_backend(backend)

    require_file(OPENVMM_DIR / "Cargo.toml", "initialized OpenVMM submodule")
    cargo = require_tool("cargo")
    rustup = require_tool("rustup")

    rust_environment = _prepare_openvmm_test_environment(backend, rustup)
    run_checked(
        [cargo, "xflowey", "restore-packages", "--no-compat-igvm"],
        cwd=OPENVMM_DIR,
        env=rust_environment,
    )
    command = [
        cargo,
        "xflowey",
        "vmm-tests-run",
        "--release",
        "--ci-profile",
        "--skip-vhd-prompt",
        "--filter",
        OPENVMM_TEST_FILTERS[backend],
    ]
    if os.name == "nt":
        command.extend(
            (
                "--dir",
                os.fspath(
                    Path(os.environ.get("RUNNER_TEMP", "C:/ovm-tests")) / backend
                ),
            )
        )
    run_checked(command, cwd=OPENVMM_DIR, env=rust_environment)


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
