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
    "nvx-rs": "NVX_RS_RESULT",
    "build-nvx-rs-linux-musl": "BUILD_NVX_RS_LINUX_MUSL_RESULT",
    "build-nvx-rs-windows-msvc": "BUILD_NVX_RS_WINDOWS_MSVC_RESULT",
    "nvx-cli-linux": "NVX_CLI_LINUX_RESULT",
    "nvx-cli-windows": "NVX_CLI_WINDOWS_RESULT",
    "openvmm-changes": "CHANGES_RESULT",
    "artifacts": "ARTIFACTS_RESULT",
    "distro-artifacts": "DISTRO_ARTIFACTS_RESULT",
    "debug-kernel": "DEBUG_KERNEL_RESULT",
    "guest-artifact-reuse": "GUEST_ARTIFACT_REUSE_RESULT",
    "build-openvmm-linux-gnu": "BUILD_LINUX_GNU_RESULT",
    "build-openvmm-linux-musl": "BUILD_LINUX_MUSL_RESULT",
    "build-openvmm-windows-msvc": "BUILD_WINDOWS_MSVC_RESULT",
    "openvmm-ci": "OPENVMM_CI_RESULT",
    "nvx-microvm-tests-kvm": "MICROVM_KVM_RESULT",
    "nvx-microvm-tests-mshv": "MICROVM_MSHV_RESULT",
    "nvx-microvm-tests-whp": "MICROVM_WHP_RESULT",
    "nvx-microvm-guests-kvm": "MICROVM_GUESTS_KVM_RESULT",
    "nvx-microvm-guests-mshv": "MICROVM_GUESTS_MSHV_RESULT",
    "nvx-microvm-guests-whp": "MICROVM_GUESTS_WHP_RESULT",
    "nvx-microvm-debug-kvm": "MICROVM_DEBUG_KVM_RESULT",
    "nvx-microvm-debug-mshv": "MICROVM_DEBUG_MSHV_RESULT",
    "nvx-microvm-debug-whp": "MICROVM_DEBUG_WHP_RESULT",
    "platform-kvm": "PLATFORM_KVM_RESULT",
    "platform-mshv": "PLATFORM_MSHV_RESULT",
    "platform-whp": "PLATFORM_WHP_RESULT",
    "package-kvm": "PACKAGE_KVM_RESULT",
    "package-mshv": "PACKAGE_MSHV_RESULT",
    "package-whp": "PACKAGE_WHP_RESULT",
    "performance-gate": "PERFORMANCE_GATE_RESULT",
}
REQUIRED_CI_BUILD_JOBS = (
    "build-openvmm-linux-gnu",
    "build-openvmm-linux-musl",
    "build-openvmm-windows-msvc",
)
# GitHub-hosted, once per OS: each builds the nvx-rs that the microVM jobs of
# its OS run, and that they wait for, and the NVX CLI job of its OS tests with.
REQUIRED_CI_NVX_RS_BUILD_JOBS = (
    "build-nvx-rs-linux-musl",
    "build-nvx-rs-windows-msvc",
)
# GitHub-hosted, once per OS, after the nvx-rs build of its OS; no hypervisor
# job waits for it.
REQUIRED_CI_CLI_JOBS = ("nvx-cli-linux", "nvx-cli-windows")
# GitHub-hosted; checks that OpenVMM's own CI passed the pinned revision
# (doc/ci.md#openvmm-ci), which needs no secret, so it runs for pull requests
# from forks too.
REQUIRED_CI_OPENVMM_CI_JOB = "openvmm-ci"
# Each backend runs the default suite in one job and, beside it, the guest
# smoke and sandbox steps that follow the suite in another.
REQUIRED_CI_MICROVM_TEST_JOBS = (
    "nvx-microvm-tests-kvm",
    "nvx-microvm-tests-mshv",
    "nvx-microvm-tests-whp",
    "nvx-microvm-guests-kvm",
    "nvx-microvm-guests-mshv",
    "nvx-microvm-guests-whp",
)
# Pull requests run the CI debug kernel on MSHV, merge groups run it on KVM,
# and dev pushes and full runs run it on every backend.
REQUIRED_CI_MICROVM_DEBUG_JOBS = ("nvx-microvm-debug-mshv",)
REQUIRED_CI_MICROVM_DEBUG_MERGE_GROUP_JOBS = ("nvx-microvm-debug-kvm",)
REQUIRED_CI_MICROVM_DEBUG_PUSH_JOBS = (
    "nvx-microvm-debug-kvm",
    "nvx-microvm-debug-whp",
)
REQUIRED_CI_ARTIFACT_JOB = "artifacts"
# GitHub-hosted, beside the kernel and Alpine artifacts; only the microVM guest
# and sandbox jobs and the package jobs, which read the Ubuntu and Azure Linux
# images, wait for it.
REQUIRED_CI_DISTRO_ARTIFACT_JOB = "distro-artifacts"
# GitHub-hosted, beside the shared artifacts; only the debug jobs wait for it.
REQUIRED_CI_DEBUG_KERNEL_JOB = "debug-kernel"
# GitHub-hosted, after both guest artifact jobs of a same-repository pull
# request, whose artifacts it checks as the dev push that merges it would
# before it reuses them; only the required status check waits for it.
REQUIRED_CI_GUEST_ARTIFACT_REUSE_JOB = "guest-artifact-reuse"
REQUIRED_CI_PLATFORM_JOBS = (
    "platform-kvm",
    "platform-mshv",
    "platform-whp",
)
# GitHub-hosted, one per platform job, which they run beside: each packages
# the platform's release, and runs whenever its platform job does.
REQUIRED_CI_PACKAGE_JOBS = (
    "package-kvm",
    "package-mshv",
    "package-whp",
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

# The events whose runs check a change before it merges: a pull request, and
# the merge group in which the merge queue tests it on the latest dev and the
# pull requests ahead of it (doc/ci.md#merge-queue). Both run the platform and
# package jobs only when one of their inputs changed and gate on the benchmarks;
# pull requests run the debug kernel on MSHV and merge groups on KVM. Neither
# publishes nor persists anything.
CI_PRE_MERGE_EVENTS = ("pull_request", "merge_group")
# The events whose runs the change detector classifies, and those that run the
# full matrix instead.
CI_CLASSIFIED_EVENTS = (*CI_PRE_MERGE_EVENTS, "push")
CI_FULL_RUN_EVENTS = ("schedule", "workflow_dispatch")
CI_EVENTS = (*CI_CLASSIFIED_EVENTS, *CI_FULL_RUN_EVENTS)
# The workflow's change detector emits one output per entry, and runs the full
# matrix when it cannot classify a change.
CI_CHANGE_OUTPUTS = (
    "run-guest-artifacts",
    "run-openvmm-binaries",
    "run-openvmm-ci-check",
    "run-microvm-tests",
    "run-benchmarks",
)
CI_DOCUMENTATION_PATHS = re.compile(r"^(doc/|.*\.md$)", re.IGNORECASE)
# Inputs of the NVX microVM tests: the OpenVMM pin, the guests and every input
# of their cache keys, the nvx-rs executable that their scenarios run, the
# runtime adapter that they test, the workflows, actions, and CLI modules that
# their jobs and the jobs that build their artifacts run, the runners, and the
# attributes that check out their scripts on Windows. Every module of the CLI
# is an input but those that none of these jobs runs.
MICROVM_TEST_INPUT_PATHS = re.compile(
    r"^(openvmm($|/)|rust-toolchain\.toml$|scripts/setup/|\.gitattributes$|"
    r"nvx-rs/|tests/runtime-adapter/|"
    r"kernel/|guest/|ubuntu/|azurelinux/|"
    r"docker/|\.dockerignore$|SOURCE-MANIFEST\.json$|"
    r"\.github/workflows/(ci|build-openvmm-binary|run-nvx-microvm-tests)\.yml$|"
    r"\.github/actions/(build-guest-artifacts|build-openvmm|checkout-openvmm|"
    r"sccache|setup-curl|validate-runner)/|"
    r"scripts/nvx\.py$|"
    r"scripts/nvx_tools/(?!(adversarial|adversarial_broker|adversarial_executor|"
    r"crate_release|create_linux_source_archive|merge_queue|nightly_ci|"
    r"openvmm_ci|performance|performance_history)\.py$|adversarial_cases/))"
)
# Inputs of the platform jobs, which benchmark each backend, of the package
# jobs, which package its release, and of the regression gate that reads the
# benchmark results: the OpenVMM pin, the guests and every input of their cache
# keys, the nvx-rs executable that the packages ship, the performance history
# and expected hosts, the release version and license, the workflows, actions,
# and CLI modules that these jobs and the jobs that build their artifacts run,
# the runners, and the attributes that check out their inputs on Windows. Every
# module of the CLI is an input but those that none of these jobs runs.
BENCHMARK_INPUT_PATHS = re.compile(
    r"^(openvmm($|/)|rust-toolchain\.toml$|scripts/setup/|\.gitattributes$|"
    r"nvx-rs/|kernel/|guest/|ubuntu/|azurelinux/|"
    r"docker/|\.dockerignore$|SOURCE-MANIFEST\.json$|"
    r"data/|VERSION$|LICENSE$|"
    r"\.github/workflows/(ci|build-openvmm-binary|package-platform|run-platform)\.yml$|"
    r"\.github/actions/(build-guest-artifacts|build-openvmm|checkout-openvmm|"
    r"gate-performance|package-release|prepare-performance-results|"
    r"run-benchmark|sccache|setup-curl|validate-runner)/|"
    r"scripts/nvx\.py$|"
    r"scripts/nvx_tools/(?!(adversarial|adversarial_broker|adversarial_executor|"
    r"crate_release|create_linux_source_archive|managed_exec_tests|merge_queue|"
    r"microvm_durations|microvm_shards|microvm_tests|nightly_ci|openvmm_ci|"
    r"openvmm_process|performance_history|runtime_adapter_tests|"
    r"sandbox_lifecycle_tests|sandbox_proxy_tests|sandbox_ram_upper_tests|"
    r"shard_plans)\.py$|"
    r"microvm_shards\.json$|adversarial_cases/|"
    r"microvm_test_scripts/))"
)
# Inputs of the check that OpenVMM's own CI passed the pinned revision
# (doc/ci.md#openvmm-ci): the pin, and the workflow and the CLI modules that
# run the check.
OPENVMM_CI_CHECK_INPUT_PATHS = re.compile(
    r"^(openvmm($|/)|\.github/workflows/ci\.yml$|scripts/nvx\.py$|"
    r"scripts/nvx_tools/"
    r"(__init__|build_constants|common|nightly_ci|openvmm_ci)\.py$)"
)


@dataclass(frozen=True)
class CiChanges:
    """The suites that a change runs, as the change detector reports them."""

    run_guest_artifacts: bool
    run_openvmm_binaries: bool
    run_openvmm_ci_check: bool
    run_microvm_tests: bool
    run_benchmarks: bool

    def outputs(self) -> dict[str, bool]:
        return dict(
            zip(
                CI_CHANGE_OUTPUTS,
                (
                    self.run_guest_artifacts,
                    self.run_openvmm_binaries,
                    self.run_openvmm_ci_check,
                    self.run_microvm_tests,
                    self.run_benchmarks,
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


def _ci_inputs(changed_files: Sequence[str]) -> list[str]:
    """Return the changed files that can select a suite: every one but the
    documentation, which no suite builds, boots, or runs."""
    return [
        path
        for path in changed_files
        if path and not CI_DOCUMENTATION_PATHS.match(path)
    ]


def classify_ci_changes(event_name: str, changed_files: Sequence[str]) -> CiChanges:
    """Select the suites that a change between two commits runs.

    Documentation never selects a suite. Pull requests, merge groups, and
    pushes run each test suite only when one of its own inputs changed. A pull
    request or merge group runs the platform and package jobs only when one of
    their inputs changed, but a push runs them for every change but
    documentation, so that every such `dev` commit publishes its development
    release and records its performance results. The guest artifacts and the
    OpenVMM binaries are built exactly when a job that uses them runs, and
    OpenVMM's own CI is checked whenever the pin or the check changes.
    """
    if event_name not in CI_CLASSIFIED_EVENTS:
        raise ValueError(f"unsupported CI event {event_name!r}")

    files = [path for path in changed_files if path]
    inputs = _ci_inputs(files)

    def changed(pattern: re.Pattern[str]) -> bool:
        return any(pattern.match(path) for path in inputs)

    run_microvm_tests = changed(MICROVM_TEST_INPUT_PATHS)
    # An empty diff runs the platform and package jobs, as it ran every
    # workload before the classifier selected them separately.
    run_benchmarks = (
        not files
        or changed(BENCHMARK_INPUT_PATHS)
        or (event_name == "push" and bool(inputs))
    )
    return CiChanges(
        run_guest_artifacts=run_microvm_tests or run_benchmarks,
        run_openvmm_binaries=run_microvm_tests or run_benchmarks,
        run_openvmm_ci_check=changed(OPENVMM_CI_CHECK_INPUT_PATHS),
        run_microvm_tests=run_microvm_tests,
        run_benchmarks=run_benchmarks,
    )


def describe_ci_changes(
    event_name: str,
    changed_files: Sequence[str],
    changes: CiChanges,
) -> str:
    """Return the Markdown job summary that explains each decision."""
    files = [path for path in changed_files if path]
    inputs = _ci_inputs(files)

    def evidence(pattern: re.Pattern[str]) -> str:
        matches = [path for path in inputs if pattern.match(path)]
        shown = ", ".join(f"`{path}`" for path in matches[:3])
        if len(matches) > 3:
            shown += f", and {len(matches) - 3} more"
        return shown

    def section(title: str, selected: bool, reason: str) -> str:
        return f"### {title}: {'run' if selected else 'skip'}\n{reason}\n"

    def inputs_reason(selected: bool, pattern: re.Pattern[str]) -> str:
        if selected and any(pattern.match(path) for path in inputs):
            return f"Inputs changed: {evidence(pattern)}."
        if not files:
            return "No file changed."
        if not inputs:
            return "Only documentation files changed."
        return "No input changed."

    def users_reason(users: dict[str, bool]) -> str:
        names = [name for name, runs in users.items() if runs]
        if not names:
            return "No job that uses them runs."
        if len(names) > 2:
            names = [", ".join(names[:-1]) + ",", names[-1]]
        return f"Jobs that use them run: {' and '.join(names)}."

    if changes.run_benchmarks and not files:
        benchmarks_reason = "No file changed, so the platform and package jobs run."
    elif (
        changes.run_benchmarks
        and event_name == "push"
        and not any(BENCHMARK_INPUT_PATHS.match(path) for path in inputs)
    ):
        benchmarks_reason = (
            "A push runs the platform and package jobs for every change but "
            "documentation, "
            "so that it publishes its development release and records its "
            "performance results."
        )
    else:
        benchmarks_reason = inputs_reason(changes.run_benchmarks, BENCHMARK_INPUT_PATHS)
    return "".join(
        (
            section(
                "Guest artifacts",
                changes.run_guest_artifacts,
                users_reason(
                    {
                        "the NVX microVM tests": changes.run_microvm_tests,
                        "the platform benchmarks and packages": changes.run_benchmarks,
                    },
                ),
            ),
            section(
                "OpenVMM binaries",
                changes.run_openvmm_binaries,
                users_reason(
                    {
                        "the NVX microVM tests": changes.run_microvm_tests,
                        "the platform benchmarks and packages": changes.run_benchmarks,
                    },
                ),
            ),
            section(
                "OpenVMM CI check",
                changes.run_openvmm_ci_check,
                inputs_reason(
                    changes.run_openvmm_ci_check, OPENVMM_CI_CHECK_INPUT_PATHS
                ),
            ),
            section(
                "NVX microVM tests",
                changes.run_microvm_tests,
                inputs_reason(changes.run_microvm_tests, MICROVM_TEST_INPUT_PATHS),
            ),
            section(
                "Platform benchmarks and packages",
                changes.run_benchmarks,
                benchmarks_reason,
            ),
        )
    )


def required_ci_expected_results(
    event_name: str,
    *,
    same_repository: bool,
    changes: CiChanges,
) -> dict[str, str]:
    if event_name not in CI_EVENTS:
        raise ValueError(f"unsupported CI event {event_name!r}")

    # Each job runs only when its suite is selected and the jobs that build the
    # artifacts that it uses are selected too, as its condition in ci.yml says.
    run_guest_artifacts = changes.run_guest_artifacts
    run_openvmm_binaries = changes.run_openvmm_binaries
    run_microvm_tests = (
        changes.run_microvm_tests and run_guest_artifacts and run_openvmm_binaries
    )
    run_benchmarks = (
        changes.run_benchmarks and run_guest_artifacts and run_openvmm_binaries
    )
    # A merge group runs in the repository whatever repository its pull
    # requests come from, so it runs the repository's jobs, as a push does.
    repository_jobs_enabled = event_name != "pull_request" or same_repository
    expected = {
        "quality": "success",
        "nvx-rs": "success",
        "openvmm-changes": "success",
        REQUIRED_CI_ARTIFACT_JOB: "success" if run_guest_artifacts else "skipped",
        REQUIRED_CI_DISTRO_ARTIFACT_JOB: (
            "success" if run_guest_artifacts else "skipped"
        ),
        REQUIRED_CI_DEBUG_KERNEL_JOB: "success" if run_guest_artifacts else "skipped",
        # A merge group skips the rehearsal, which its pull request's run made.
        REQUIRED_CI_GUEST_ARTIFACT_REUSE_JOB: (
            "success"
            if event_name == "pull_request" and same_repository and run_guest_artifacts
            else "skipped"
        ),
        "performance-gate": (
            "success"
            if event_name in CI_PRE_MERGE_EVENTS
            and repository_jobs_enabled
            and run_benchmarks
            else "skipped"
        ),
        # The check needs no secret, so a pull request from a fork runs it too.
        REQUIRED_CI_OPENVMM_CI_JOB: (
            "success" if changes.run_openvmm_ci_check else "skipped"
        ),
    }
    expected.update(
        {
            job: "success" if repository_jobs_enabled else "skipped"
            for job in (*REQUIRED_CI_NVX_RS_BUILD_JOBS, *REQUIRED_CI_CLI_JOBS)
        }
    )
    expected.update(
        {
            job: (
                "success"
                if repository_jobs_enabled and run_openvmm_binaries
                else "skipped"
            )
            for job in REQUIRED_CI_BUILD_JOBS
        }
    )
    expected.update(
        {
            job: (
                "success"
                if repository_jobs_enabled and run_microvm_tests
                else "skipped"
            )
            for job in REQUIRED_CI_MICROVM_TEST_JOBS
        }
    )
    if event_name == "pull_request":
        enabled_debug_jobs = set(REQUIRED_CI_MICROVM_DEBUG_JOBS)
    elif event_name == "merge_group":
        enabled_debug_jobs = set(REQUIRED_CI_MICROVM_DEBUG_MERGE_GROUP_JOBS)
    else:
        enabled_debug_jobs = set(
            (*REQUIRED_CI_MICROVM_DEBUG_JOBS, *REQUIRED_CI_MICROVM_DEBUG_PUSH_JOBS)
        )
    all_debug_jobs = dict.fromkeys(
        (
            *REQUIRED_CI_MICROVM_DEBUG_JOBS,
            *REQUIRED_CI_MICROVM_DEBUG_MERGE_GROUP_JOBS,
            *REQUIRED_CI_MICROVM_DEBUG_PUSH_JOBS,
        )
    )
    expected.update(
        {
            job: (
                "success"
                if repository_jobs_enabled
                and run_microvm_tests
                and job in enabled_debug_jobs
                else "skipped"
            )
            for job in all_debug_jobs
        }
    )
    expected.update(
        {
            job: (
                "success" if repository_jobs_enabled and run_benchmarks else "skipped"
            )
            for job in (*REQUIRED_CI_PLATFORM_JOBS, *REQUIRED_CI_PACKAGE_JOBS)
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
# A nested test runs inside an L1 VM host whose test function, vm_host or a
# variant, appears in its name. The Linux runners support neither nested
# virtualization nor the aarch64-unknown-linux-musl archive of aarch64 hosts.
OPENVMM_NESTED_TEST_EXCLUSION = " & !test(vm_host)"


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
    f"{OPENVMM_NESTED_TEST_EXCLUSION}"
    " & !test(no_vmbus_prepped_boot_no_vmbus_windows)"
    " & !test(windows_datacenter_core)"
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
    f"({OPENVMM_LINUX_TEST_FILTER}){OPENVMM_NESTED_TEST_EXCLUSION}"
    " & !test(windows_datacenter_core)",
    OPENVMM_MSHV_EXCLUDED_TESTS,
)
# ttrpc::test_ttrpc_interface stays on the Linux backends: it boots Linux
# pipette, which flowey can build only on a Linux host. No test needs Hyper-V,
# which flowey reads from "hyperv" in a test's name, so that vmm-tests-run
# requires no Hyper-V on the WHP host.
OPENVMM_WHP_TESTS = (
    "multiarch::hibernate::openvmm_uefi_x64_guest_test_x64_hibernate_halts",
    "multiarch::ic::openvmm_uefi_x64_windows_datacenter_core_2022_x64_timesync_ic",
    "multiarch::ic::openvmm_uefi_x64_windows_datacenter_core_2025_x64_kvp_ic",
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


# MSVC's link.exe stamps every DLL and executable with the time of the link
# and a random PDB signature unless /Brepro derives both from the image's
# contents. Each Windows build therefore relinked every proc macro with new
# bytes, and sccache, which hashes the files that a compilation names with
# --extern, missed every crate that uses a proc macro directly (#693). The
# linker appends _LINK_ to each of its command lines; RUSTFLAGS would miss the
# proc macros of the builds that flowey runs with --target.
MSVC_REPRODUCIBLE_LINK_OPTION = "/Brepro"


def _link_reproducibly_with_msvc(environment: dict[str, str]) -> None:
    options = environment.get("_LINK_", "")
    if all(option.lstrip("/-").casefold() != "brepro" for option in options.split()):
        environment["_LINK_"] = f"{options} {MSVC_REPRODUCIBLE_LINK_OPTION}".strip()


def _prepare_openvmm_test_environment(
    backends: Sequence[str],
    rustup: str,
) -> dict[str, str]:
    toolchain = OpenVMMBuildConstants.RUST_TOOLCHAIN
    targets = list(
        dict.fromkeys(
            target
            for backend in backends
            for target in OpenVMMBuildConstants.TEST_RUST_TARGETS[backend]
        )
    )
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
    if os.name == "nt":
        _link_reproducibly_with_msvc(environment)
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

    _set_openvmm_test_cache(environment, backends)
    return environment


def _set_openvmm_test_cache(
    environment: dict[str, str],
    backends: Sequence[str],
) -> None:
    # Petri streams and layers guest disks in the XDG cache, and the Linux
    # runner account has no home directory to hold it.
    runner_temp_value = os.environ.get("RUNNER_TEMP")
    if "whp" not in backends and runner_temp_value:
        environment["XDG_CACHE_HOME"] = os.fspath(
            Path(runner_temp_value) / OpenVMMBuildConstants.CACHE_DIRECTORY_NAME
        )


def run_openvmm_unit_tests() -> None:
    require_file(
        OpenVMMBuildConstants.DIRECTORY / "Cargo.toml", "initialized OpenVMM submodule"
    )
    cargo = require_tool("cargo")
    environment = os.environ.copy()
    if os.name == "nt":
        _link_reproducibly_with_msvc(environment)

    fuzz_crates = run_capture(
        [cargo, "xtask", "fuzz", "list", "--crates"],
        cwd=OpenVMMBuildConstants.DIRECTORY,
        env=environment,
    )
    require_success(fuzz_crates, "OpenVMM fuzz crate query")

    run_checked(
        [cargo, "xflowey", "restore-packages", "--no-compat-igvm"],
        cwd=OpenVMMBuildConstants.DIRECTORY,
        env=environment,
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

    run_checked(command, cwd=OpenVMMBuildConstants.DIRECTORY, env=environment)
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
        env=environment,
    )


def _openvmm_microvm_test_guest() -> tuple[Path, Path]:
    kernel = require_file(
        artifact_path(KernelBuildConstants.BINARY_NAME),
        "microVM Linux direct kernel",
    )
    initrd = require_file(
        artifact_path(AlpineBuildConstants.INITRAMFS_NAME),
        "microVM Alpine initramfs",
    )
    return kernel, initrd


def run_openvmm_tests(backend: str) -> None:
    """Build and run the backend's VMM tests."""
    validate_openvmm_test_backend(backend)
    require_file(
        OpenVMMBuildConstants.DIRECTORY / "Cargo.toml", "initialized OpenVMM submodule"
    )
    cargo = require_tool("cargo")
    rustup = require_tool("rustup")
    kernel, initrd = _openvmm_microvm_test_guest()

    rust_environment = _prepare_openvmm_test_environment((backend,), rustup)
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
