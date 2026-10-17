"""Helpers used by nvx continuous-integration jobs."""

from __future__ import annotations

import hashlib
import os
import re
import shutil
import zipfile
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
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
    "aci-edge-sandboxes": "ACI_EDGE_SANDBOXES_RESULT",
    "nvx-rs": "NVX_RS_RESULT",
    "nvx-cli-linux": "NVX_CLI_LINUX_RESULT",
    "nvx-cli-windows": "NVX_CLI_WINDOWS_RESULT",
    "openvmm-changes": "CHANGES_RESULT",
    "artifacts": "ARTIFACTS_RESULT",
    "debug-kernel": "DEBUG_KERNEL_RESULT",
    "build-openvmm-linux-gnu": "BUILD_LINUX_GNU_RESULT",
    "build-openvmm-linux-musl": "BUILD_LINUX_MUSL_RESULT",
    "build-openvmm-windows-msvc": "BUILD_WINDOWS_MSVC_RESULT",
    "openvmm-vmm-tests": "VMM_TESTS_RESULT",
    "openvmm-unit-tests": "UNIT_TESTS_RESULT",
    "nvx-microvm-tests-kvm": "MICROVM_KVM_RESULT",
    "nvx-microvm-tests-mshv": "MICROVM_MSHV_RESULT",
    "nvx-microvm-tests-whp": "MICROVM_WHP_RESULT",
    "nvx-microvm-debug-kvm": "MICROVM_DEBUG_KVM_RESULT",
    "nvx-microvm-debug-mshv": "MICROVM_DEBUG_MSHV_RESULT",
    "nvx-microvm-debug-whp": "MICROVM_DEBUG_WHP_RESULT",
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
# GitHub-hosted, once per OS; the microVM and platform jobs of each OS wait for
# its result.
REQUIRED_CI_CLI_JOBS = ("nvx-cli-linux", "nvx-cli-windows")
REQUIRED_CI_OPENVMM_TEST_JOBS = ("openvmm-unit-tests",)
# OpenVMM VMM tests boot the NVX guest artifacts, which only workload runs build.
REQUIRED_CI_OPENVMM_ARTIFACT_TEST_JOBS = ("openvmm-vmm-tests",)
REQUIRED_CI_MICROVM_TEST_JOBS = (
    "nvx-microvm-tests-kvm",
    "nvx-microvm-tests-mshv",
    "nvx-microvm-tests-whp",
)
# The CI debug kernel runs on KVM for every pull request and on every backend
# for dev pushes.
REQUIRED_CI_MICROVM_DEBUG_JOBS = ("nvx-microvm-debug-kvm",)
REQUIRED_CI_MICROVM_DEBUG_PUSH_JOBS = (
    "nvx-microvm-debug-mshv",
    "nvx-microvm-debug-whp",
)
REQUIRED_CI_ARTIFACT_JOB = "artifacts"
# GitHub-hosted, beside the shared artifacts; only the debug jobs wait for it.
REQUIRED_CI_DEBUG_KERNEL_JOB = "debug-kernel"
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

# The workflow's change detector emits one output per entry, and runs the full
# matrix when it cannot classify a change.
CI_CHANGE_OUTPUTS = (
    "run-tests",
    "run-workloads",
    "run-openvmm-unit-tests",
    "run-openvmm-vmm-tests",
)
CI_DOCUMENTATION_PATHS = re.compile(r"^(doc/|.*\.md$)", re.IGNORECASE)
# Inputs of the hypervisor test suites: OpenVMM, the guests that the microVM
# tests boot, the Rust crates that test-aci-edge-sandboxes runs, the CLI and
# workflows that drive the suites, and the runners.
CI_TEST_INPUT_PATHS = re.compile(
    r"^(openvmm($|/)|aci_edge_sandboxes/|nvx-rs/|azurelinux/|"
    r"\.github/workflows/"
    r"(ci|build-openvmm-binary|run-nvx-microvm-tests|run-platform)\.yml$|"
    r"\.github/actions/(build-guest-artifacts|build-openvmm|checkout-openvmm|"
    r"sccache|setup-curl|validate-runner)/|"
    r"scripts/nvx\.py$|scripts/nvx_tools/|scripts/setup/|"
    r"kernel/|alpine/|guest/|ubuntu/|"
    r"docker/Dockerfile$|\.dockerignore$|SOURCE-MANIFEST\.json$|rust-toolchain\.toml$)"
)
# Inputs of the OpenVMM unit and documentation tests: the pinned revision, the
# Rust release, the runner setup, and the code and workflow that run them.
OPENVMM_UNIT_TEST_INPUT_PATHS = re.compile(
    r"^(openvmm($|/)|rust-toolchain\.toml$|scripts/setup/|"
    r"\.github/workflows/ci\.yml$|"
    r"\.github/actions/(checkout-openvmm|sccache|setup-curl|validate-runner)/|"
    r"scripts/nvx\.py$|scripts/nvx_tools/(__init__|build_constants|ci|common)\.py$)"
)
# The OpenVMM VMM tests also boot the NVX kernel and Alpine initramfs, so they
# add the inputs of those artifacts' cache keys and of the job that builds them.
OPENVMM_VMM_TEST_INPUT_PATHS = re.compile(
    r"^(openvmm($|/)|nvx-rs/|rust-toolchain\.toml$|scripts/setup/|"
    r"\.github/workflows/ci\.yml$|"
    r"\.github/actions/(build-guest-artifacts|checkout-openvmm|sccache|"
    r"setup-curl|validate-runner)/|"
    r"scripts/nvx\.py$|"
    r"scripts/nvx_tools/"
    r"(__init__|build|build_config|build_constants|ci|common|elf|"
    r"guest_binaries|guests)\.py$|"
    r"SOURCE-MANIFEST\.json$|docker/Dockerfile$|\.dockerignore$|kernel/|guest/(alpine|common)/)"
)


@dataclass(frozen=True)
class CiChanges:
    """The suites that a change runs, as the change detector reports them."""

    run_tests: bool
    run_workloads: bool
    run_openvmm_unit_tests: bool
    run_openvmm_vmm_tests: bool

    def outputs(self) -> dict[str, bool]:
        return dict(
            zip(
                CI_CHANGE_OUTPUTS,
                (
                    self.run_tests,
                    self.run_workloads,
                    self.run_openvmm_unit_tests,
                    self.run_openvmm_vmm_tests,
                ),
                strict=True,
            )
        )


def kernel_patch_identity(patch_root: Path) -> str:
    """Return a digest of each patch's relative path and content, in path order.

    A content-only hash ignores names and order, so renaming a patch reused a
    kernel whose provenance still named the old path. Both are part of the key.
    The file set matches the top-level ``*.patch`` files that kernel provenance
    records, so unrelated files do not change the key and a missing or empty
    directory fails instead of producing the empty digest.
    """
    files = sorted(
        (path.relative_to(patch_root).as_posix(), path)
        for path in patch_root.glob("*.patch")
        if path.is_file()
    )
    if not files:
        raise ScriptError("no kernel patches were found")
    digest = hashlib.sha256()
    for relative, path in files:
        digest.update(relative.encode("utf-8") + b"\0")
        digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()[:16]


def classify_ci_changes(event_name: str, changed_files: Sequence[str]) -> CiChanges:
    """Select the suites that a change between two commits runs.

    Pull requests run each OpenVMM suite only when one of its own inputs
    changed. Pushes keep running both whenever any test input changed.
    """
    if event_name not in ("pull_request", "push"):
        raise ValueError(f"unsupported CI event {event_name!r}")

    files = [path for path in changed_files if path]
    run_workloads = not files or not all(
        CI_DOCUMENTATION_PATHS.match(path) for path in files
    )
    run_tests = any(CI_TEST_INPUT_PATHS.match(path) for path in files)
    if event_name == "push":
        return CiChanges(run_tests, run_workloads, run_tests, run_tests)
    return CiChanges(
        run_tests=run_tests,
        run_workloads=run_workloads,
        run_openvmm_unit_tests=run_tests
        and any(OPENVMM_UNIT_TEST_INPUT_PATHS.match(path) for path in files),
        run_openvmm_vmm_tests=run_tests
        and any(OPENVMM_VMM_TEST_INPUT_PATHS.match(path) for path in files),
    )


def describe_ci_changes(
    event_name: str,
    changed_files: Sequence[str],
    changes: CiChanges,
) -> str:
    """Return the Markdown job summary that explains each decision."""
    files = [path for path in changed_files if path]

    def evidence(pattern: re.Pattern[str]) -> str:
        matches = [path for path in files if pattern.match(path)]
        shown = ", ".join(f"`{path}`" for path in matches[:3])
        if len(matches) > 3:
            shown += f", and {len(matches) - 3} more"
        return shown

    def section(title: str, selected: bool, reason: str) -> str:
        return f"### {title}: {'run' if selected else 'skip'}\n{reason}\n"

    if not files:
        workloads_reason = "No file changed, so the workloads run."
    elif changes.run_workloads:
        workloads_reason = "At least one non-documentation file changed."
    else:
        workloads_reason = "Only documentation files changed."
    sections = [
        section(
            "Artifact-backed tests and benchmarks",
            changes.run_workloads,
            workloads_reason,
        ),
        section(
            "NVX microVM tests",
            changes.run_tests,
            f"Test inputs changed: {evidence(CI_TEST_INPUT_PATHS)}."
            if changes.run_tests
            else "No test input changed.",
        ),
    ]
    for title, selected, pattern in (
        (
            "OpenVMM unit tests",
            changes.run_openvmm_unit_tests,
            OPENVMM_UNIT_TEST_INPUT_PATHS,
        ),
        (
            "OpenVMM VMM tests",
            changes.run_openvmm_vmm_tests,
            OPENVMM_VMM_TEST_INPUT_PATHS,
        ),
    ):
        if event_name == "push":
            reason = "Pushes run every OpenVMM suite whenever the tests run."
        elif selected:
            reason = f"Suite inputs changed: {evidence(pattern)}."
        elif changes.run_tests:
            reason = "No input of this suite changed."
        else:
            reason = "No test input changed."
        sections.append(section(title, selected, reason))
    return "".join(sections)


def required_ci_expected_results(
    event_name: str,
    *,
    same_repository: bool,
    changes: CiChanges,
) -> dict[str, str]:
    if event_name not in ("pull_request", "push"):
        raise ValueError(f"unsupported CI event {event_name!r}")

    run_tests = changes.run_tests
    run_workloads = changes.run_workloads
    repository_jobs_enabled = event_name == "push" or same_repository
    expected = {
        "quality": "success",
        "aci-edge-sandboxes": "success",
        "nvx-rs": "success",
        "openvmm-changes": "success",
        REQUIRED_CI_ARTIFACT_JOB: "success" if run_workloads else "skipped",
        REQUIRED_CI_DEBUG_KERNEL_JOB: "success" if run_workloads else "skipped",
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
            job: "success" if repository_jobs_enabled else "skipped"
            for job in REQUIRED_CI_CLI_JOBS
        }
    )
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
            job: (
                "success"
                if repository_jobs_enabled and changes.run_openvmm_unit_tests
                else "skipped"
            )
            for job in REQUIRED_CI_OPENVMM_TEST_JOBS
        }
    )
    expected.update(
        {
            job: (
                "success"
                if repository_jobs_enabled
                and changes.run_openvmm_vmm_tests
                and run_workloads
                else "skipped"
            )
            for job in REQUIRED_CI_OPENVMM_ARTIFACT_TEST_JOBS
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
                *REQUIRED_CI_MICROVM_TEST_JOBS,
                *REQUIRED_CI_MICROVM_DEBUG_JOBS,
            )
        }
    )
    expected.update(
        {
            job: (
                "success"
                if event_name == "push" and run_tests and run_workloads
                else "skipped"
            )
            for job in REQUIRED_CI_MICROVM_DEBUG_PUSH_JOBS
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
    changes: CiChanges,
    results: Mapping[str, str],
) -> list[str]:
    expected = required_ci_expected_results(
        event_name,
        same_repository=same_repository,
        changes=changes,
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
# ttrpc::test_ttrpc_interface stays on the Linux backends: it boots Linux
# pipette, which flowey can build only on a Linux host.
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
    toolchain = OpenVMMBuildConstants.RUST_TOOLCHAIN
    targets = OpenVMMBuildConstants.TEST_RUST_TARGETS[backend]
    installed = run_capture(
        [
            rustup,
            "target",
            "list",
            "--installed",
            "--toolchain",
            toolchain,
        ]
    )
    # rustup rejects the query when the pinned toolchain is not installed.
    toolchain_installed = installed.returncode == 0
    installed_targets: set[str] = set()
    if toolchain_installed:
        installed_targets.update(installed.stdout.decode("utf-8").splitlines())

    environment = os.environ.copy()
    environment["RUSTUP_TOOLCHAIN"] = toolchain
    runner_temp_value = os.environ.get("RUNNER_TEMP")
    if not installed_targets.issuperset(targets):
        if runner_temp_value:
            environment["RUSTUP_HOME"] = os.fspath(
                Path(runner_temp_value) / OpenVMMBuildConstants.RUSTUP_DIRECTORY_NAME
            )
        if runner_temp_value or not toolchain_installed:
            run_checked(
                [
                    rustup,
                    "toolchain",
                    "install",
                    toolchain,
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
                toolchain,
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
