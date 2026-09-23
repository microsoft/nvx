"""Helpers used by nvx continuous-integration jobs."""

from __future__ import annotations

import os
import re
import shutil
import zipfile
from collections.abc import Mapping
from pathlib import Path

from .build_constants import (
    AlpineBuildConstants,
    KernelBuildConstants,
    OpenVMMBuildConstants,
    ZstdBuildConstants,
)
from .common import (
    ScriptError,
    artifact_path,
    download,
    require_file,
    require_success,
    require_tool,
    run_capture,
    run_checked,
)

OPENVMM_TEST_BACKENDS = ("kvm", "mshv", "whp")
REQUIRED_CI_RESULT_ENVIRONMENTS = {
    "quality": "QUALITY_RESULT",
    "openvmm-changes": "CHANGES_RESULT",
    "artifacts": "ARTIFACTS_RESULT",
    "build-openvmm-linux-gnu": "BUILD_LINUX_GNU_RESULT",
    "build-openvmm-linux-musl": "BUILD_LINUX_MUSL_RESULT",
    "build-openvmm-windows-msvc": "BUILD_WINDOWS_MSVC_RESULT",
    "openvmm-vmm-tests": "VMM_TESTS_RESULT",
    "openvmm-unit-tests": "UNIT_TESTS_RESULT",
    "nvx-microvm-tests-kvm": "MICROVM_KVM_RESULT",
    "nvx-microvm-tests-mshv": "MICROVM_MSHV_RESULT",
    "nvx-microvm-tests-whp": "MICROVM_WHP_RESULT",
    "platform-kvm": "PLATFORM_KVM_RESULT",
    "platform-mshv": "PLATFORM_MSHV_RESULT",
    "platform-whp": "PLATFORM_WHP_RESULT",
    "performance-gate": "PERFORMANCE_GATE_RESULT",
}
REQUIRED_CI_BUILD_JOBS = (
    "build-openvmm-linux-gnu",
    "build-openvmm-linux-musl",
    "build-openvmm-windows-msvc",
)
REQUIRED_CI_OPENVMM_TEST_JOBS = ("openvmm-unit-tests",)
# OpenVMM VMM tests boot the NVX guest artifacts, which only workload runs build.
REQUIRED_CI_OPENVMM_ARTIFACT_TEST_JOBS = ("openvmm-vmm-tests",)
REQUIRED_CI_MICROVM_TEST_JOBS = (
    "nvx-microvm-tests-kvm",
    "nvx-microvm-tests-mshv",
    "nvx-microvm-tests-whp",
)
REQUIRED_CI_ARTIFACT_JOB = "artifacts"
REQUIRED_CI_PLATFORM_JOBS = (
    "platform-kvm",
    "platform-mshv",
    "platform-whp",
)
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


def required_ci_expected_results(
    event_name: str,
    *,
    same_repository: bool,
    run_tests: bool,
    run_workloads: bool,
) -> dict[str, str]:
    if event_name not in ("pull_request", "push"):
        raise ValueError(f"unsupported CI event {event_name!r}")

    repository_jobs_enabled = event_name == "push" or same_repository
    expected = {
        "quality": "success",
        "openvmm-changes": "success",
        REQUIRED_CI_ARTIFACT_JOB: "success" if run_workloads else "skipped",
        "performance-gate": (
            "success"
            if event_name == "pull_request"
            and repository_jobs_enabled
            and run_workloads
            else "skipped"
        ),
    }
    expected.update(
        {
            job: (
                "success"
                if repository_jobs_enabled and (run_tests or run_workloads)
                else "skipped"
            )
            for job in REQUIRED_CI_BUILD_JOBS
        }
    )
    expected.update(
        {
            job: ("success" if repository_jobs_enabled and run_tests else "skipped")
            for job in REQUIRED_CI_OPENVMM_TEST_JOBS
        }
    )
    expected.update(
        {
            job: (
                "success"
                if repository_jobs_enabled and run_tests and run_workloads
                else "skipped"
            )
            for job in (
                *REQUIRED_CI_OPENVMM_ARTIFACT_TEST_JOBS,
                *REQUIRED_CI_MICROVM_TEST_JOBS,
            )
        }
    )
    expected.update(
        {
            job: ("success" if repository_jobs_enabled and run_workloads else "skipped")
            for job in REQUIRED_CI_PLATFORM_JOBS
        }
    )
    return expected


def required_ci_failures(
    event_name: str,
    *,
    same_repository: bool,
    run_tests: bool,
    run_workloads: bool,
    results: Mapping[str, str],
) -> list[str]:
    expected = required_ci_expected_results(
        event_name,
        same_repository=same_repository,
        run_tests=run_tests,
        run_workloads=run_workloads,
    )
    failures: list[str] = []
    for job, expected_result in expected.items():
        actual_result = results.get(job) or "missing"
        if actual_result != expected_result:
            failures.append(f"{job}: expected {expected_result}, got {actual_result}")
    return failures


def _exact_openvmm_test(test: str) -> str:
    return f"test(/^{re.escape(test)}$/)"


OPENVMM_REQUIRED_MICROVM_TESTS = (
    "ttrpc::test_ttrpc_interface",
    "ttrpc::test_ttrpc_microvm_linux_direct_lifecycle_and_snapshot",
    "x86_64::microvm::openvmm_linux_x64_phase_1_lifecycle",
)
OPENVMM_LINUX_TEST_FILTER = "test(openvmm) | test(ttrpc) | " + " | ".join(
    _exact_openvmm_test(test) for test in OPENVMM_REQUIRED_MICROVM_TESTS
)


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
    *OPENVMM_REQUIRED_MICROVM_TESTS,
    "ttrpc::test_ttrpc_uefi_boot",
    "x86_64::openvmm_uefi_x64_guest_test_x64_crash_dump_on_triple_fault",
    "x86_64::openvmm_uefi_x64_windows_datacenter_core_2022_x64_battery_capacity",
)
OPENVMM_WHP_EXCLUDED_TESTS: tuple[str, ...] = ()


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
    targets = OpenVMMBuildConstants.TEST_RUST_TARGETS[backend]
    installed = run_capture(
        [
            rustup,
            "target",
            "list",
            "--installed",
            "--toolchain",
            OpenVMMBuildConstants.RUST_TOOLCHAIN,
        ]
    )
    require_success(installed, "installed Rust target query")
    installed_targets = set(installed.stdout.decode("utf-8").splitlines())

    environment = os.environ.copy()
    environment["RUSTUP_TOOLCHAIN"] = OpenVMMBuildConstants.RUST_TOOLCHAIN
    runner_temp_value = os.environ.get("RUNNER_TEMP")
    if not installed_targets.issuperset(targets):
        if runner_temp_value:
            environment["RUSTUP_HOME"] = os.fspath(
                Path(runner_temp_value) / OpenVMMBuildConstants.RUSTUP_DIRECTORY_NAME
            )
            run_checked(
                [
                    rustup,
                    "toolchain",
                    "install",
                    OpenVMMBuildConstants.RUST_TOOLCHAIN,
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
                OpenVMMBuildConstants.RUST_TOOLCHAIN,
            ],
            env=environment,
        )

    if backend != "whp" and runner_temp_value:
        environment["XDG_CACHE_HOME"] = os.fspath(
            Path(runner_temp_value) / OpenVMMBuildConstants.CACHE_DIRECTORY_NAME
        )

    return environment


def run_openvmm_unit_tests() -> None:
    require_file(
        OpenVMMBuildConstants.DIRECTORY / "Cargo.toml", "initialized OpenVMM submodule"
    )
    cargo = require_tool("cargo")

    fuzz_crates = run_capture(
        [cargo, "xtask", "fuzz", "list", "--crates"],
        cwd=OpenVMMBuildConstants.DIRECTORY,
    )
    require_success(fuzz_crates, "OpenVMM fuzz crate query")

    run_checked(
        [cargo, "xflowey", "restore-packages", "--no-compat-igvm"],
        cwd=OpenVMMBuildConstants.DIRECTORY,
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

    run_checked(command, cwd=OpenVMMBuildConstants.DIRECTORY)
    run_checked(
        [
            cargo,
            "test",
            "--locked",
            "--doc",
            "--workspace",
            "--no-fail-fast",
        ],
        cwd=OpenVMMBuildConstants.DIRECTORY,
    )


def run_openvmm_tests(backend: str) -> None:
    validate_openvmm_test_backend(backend)

    require_file(
        OpenVMMBuildConstants.DIRECTORY / "Cargo.toml", "initialized OpenVMM submodule"
    )
    cargo = require_tool("cargo")
    rustup = require_tool("rustup")
    kernel = require_file(
        artifact_path(KernelBuildConstants.BINARY_NAME),
        "microVM Linux direct kernel",
    )
    initrd = require_file(
        artifact_path(AlpineBuildConstants.INITRAMFS_NAME),
        "microVM Alpine initramfs",
    )

    rust_environment = _prepare_openvmm_test_environment(backend, rustup)
    rust_environment["OPENVMM_MICROVM_TEST_KERNEL"] = os.fspath(kernel.resolve())
    rust_environment["OPENVMM_MICROVM_TEST_INITRD"] = os.fspath(initrd.resolve())
    run_checked(
        [cargo, "xflowey", "restore-packages", "--no-compat-igvm"],
        cwd=OpenVMMBuildConstants.DIRECTORY,
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
    run_checked(command, cwd=OpenVMMBuildConstants.DIRECTORY, env=rust_environment)


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

    archive = runner_temp / ZstdBuildConstants.ARCHIVE_NAME
    download(ZstdBuildConstants.URL, archive, expected_sha256=ZstdBuildConstants.SHA256)

    destination = runner_temp / ZstdBuildConstants.DIRECTORY_NAME
    shutil.rmtree(destination, ignore_errors=True)
    with zipfile.ZipFile(archive) as package:
        package.extractall(destination)
    zstd = destination / ZstdBuildConstants.DIRECTORY_NAME / "zstd.exe"
    require_file(zstd, "zstd.exe")

    with github_path.open("a", encoding="utf-8", newline="") as output:
        output.write(f"{gnu_tar.parent}{os.linesep}")
        output.write(f"{zstd.parent}{os.linesep}")

    run_checked([gnu_tar, "--version"])
    run_checked([zstd, "--version"])
