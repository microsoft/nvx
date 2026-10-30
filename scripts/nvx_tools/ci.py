"""Helpers used by nvx continuous-integration jobs."""

from __future__ import annotations

import hashlib
import json
import math
import os
import re
import shlex
import shutil
import stat
import sys
import zipfile
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import cast
from xml.etree import ElementTree

from .build_constants import (
    AlpineBuildConstants,
    BuildConstants,
    GuestRustBuildConstants,
    KernelBuildConstants,
    OpenVMMBuildConstants,
    ZstdBuildConstants,
)
from .common import (
    ScriptError,
    artifact_path,
    bytes_to_mib,
    download,
    github_job,
    is_link_or_junction,
    openvmm_git_state,
    path_exists,
    require_file,
    require_success,
    require_tool,
    run_capture,
    run_checked,
    sha256_file,
    strict_json_object,
)

OPENVMM_TEST_BACKENDS = ("kvm", "mshv", "whp")
# The operating system that builds and runs each backend's VMM tests.
OPENVMM_TEST_OPERATING_SYSTEMS = {"kvm": "linux", "mshv": "linux", "whp": "windows"}
# `build-openvmm-tests` describes the test content that it builds in this file,
# which `test-openvmm --content` reads. Format 2 added the target, the build
# options, and the SHA-256 of each file.
OPENVMM_TEST_CONTENT_MANIFEST = "nvx-openvmm-tests.json"
OPENVMM_TEST_CONTENT_FORMAT = 2
# The vmm-tests-run target of each operating system's content. NVX supports
# only x86-64 hosts, and the WHP tests run only windows-x64 content, so the
# build names its target rather than taking the host's.
OPENVMM_TEST_TARGETS = {"linux": "linux-x64", "windows": "windows-x64"}
# The vmm-tests-run options that build the content: a release build without
# optional features, whose run script runs the tests with the nextest CI
# profile and without the interactive VHD prompt.
OPENVMM_TEST_BUILD_OPTIONS = (
    "--release",
    "--ci-profile",
    "--skip-vhd-prompt",
    "--build-only",
)
# CI caches the test content of each operating system across runs under a key
# of everything that determines it (doc/ci.md#openvmm-vmm-test-cache): this
# version, the target, the OpenVMM revision, and the SHA-256 of every field of
# the content's manifest that describes what built it. Increment the version
# when what a build makes, or what a reuse of the content checks, changes in a
# way that none of those fields captures, such as the environment in which the
# tests build.
OPENVMM_TEST_CACHE_VERSION = 1
# The script that `vmm-tests-run --build-only` writes to run the content.
OPENVMM_TEST_RUN_SCRIPTS = {"linux": "run.sh", "windows": "run.ps1"}
# The nextest archive of the tests, in the content's directory of its target.
OPENVMM_TEST_ARCHIVE_NAME = "vmm_tests.tar.zst"
# Petri writes the results of a content run into this directory of the
# content, which also holds the run's test duration report.
OPENVMM_TEST_RESULTS = "test_results"
# A content run takes the remapped workspace from the content, so nextest
# writes the JUnit report that OpenVMM's nextest configuration gives the ci
# profile, which the content's runs use, under the content's target directory.
OPENVMM_TEST_JUNIT = PurePosixPath("target", "nextest", "ci", "junit.xml")
# `test-openvmm --content` records each test's duration in this report in its
# results, from which `plan-openvmm-test-shards` plans the shards
# (doc/usage.md#test-duration-report).
OPENVMM_TEST_DURATIONS_NAME = "openvmm-test-durations.json"
OPENVMM_TEST_DURATIONS_SCHEMA = "nvx-openvmm-test-durations"
# Changes only when a reader of the previous version would misread a report;
# added fields keep it.
OPENVMM_TEST_DURATIONS_SCHEMA_VERSION = 1
# The images that the pinned vmm-tests-run-target downloads before it runs the
# tests, by the name that a run script passes with --artifacts, with the file
# and the size that petri_artifacts_vmm_test declares for each. They come from
# OpenVMM's public test image container, from which Petri streams every other
# image on demand.
OPENVMM_TEST_IMAGE_URL = "https://hvlitetestvhds.blob.core.windows.net/vhds"
OPENVMM_TEST_IMAGES = {"VmgsWithBootEntry": ("sample-vmgs.vhd", 4194816)}
# The vmm-tests-run-target options that the pinned run scripts pass, and
# whether each takes a value. The consumer passes its own --dir and --filter.
OPENVMM_TEST_RUN_TARGET_OPTIONS = {
    "--target": True,
    "--repetitions": True,
    "--artifacts": True,
    "--prep-steps": True,
    "--skip-vhd-prompt": False,
    "--ci-profile": False,
    "--no-reuse-prepped-vhds": False,
    "--needs-hardware-isolation": False,
    "--needs-igvm-agent": False,
}
REQUIRED_CI_RESULT_ENVIRONMENTS = {
    "quality": "QUALITY_RESULT",
    "nvx-rs": "NVX_RS_RESULT",
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
    "build-openvmm-vmm-tests-linux": "BUILD_VMM_TESTS_LINUX_RESULT",
    "build-openvmm-vmm-tests-windows": "BUILD_VMM_TESTS_WINDOWS_RESULT",
    "openvmm-vmm-tests-linux": "VMM_TESTS_LINUX_RESULT",
    "openvmm-vmm-tests-windows": "VMM_TESTS_WINDOWS_RESULT",
    "openvmm-unit-tests": "UNIT_TESTS_RESULT",
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
# OpenVMM VMM tests boot the NVX guest artifacts, so they need the job that
# builds them.
REQUIRED_CI_OPENVMM_ARTIFACT_TEST_JOBS = (
    "openvmm-vmm-tests-linux",
    "openvmm-vmm-tests-windows",
)
# Each builds, without the guest artifacts, the VMM tests that the test jobs of
# its OS run, so it runs whenever a change selects the VMM tests.
REQUIRED_CI_OPENVMM_TEST_BUILD_JOBS = (
    "build-openvmm-vmm-tests-linux",
    "build-openvmm-vmm-tests-windows",
)
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
# The CI debug kernel runs on KVM for every pull request and merge group and on
# every backend for dev pushes and full runs.
REQUIRED_CI_MICROVM_DEBUG_JOBS = ("nvx-microvm-debug-kvm",)
REQUIRED_CI_MICROVM_DEBUG_PUSH_JOBS = (
    "nvx-microvm-debug-mshv",
    "nvx-microvm-debug-whp",
)
REQUIRED_CI_ARTIFACT_JOB = "artifacts"
# GitHub-hosted, beside the kernel and Alpine artifacts; only the microVM guest
# and sandbox jobs and the platform jobs, which read the Ubuntu and Azure Linux
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
# pull requests ahead of it (doc/ci.md#merge-queue). Both run the platform jobs
# only when one of their inputs changed, gate on their results, and run the
# debug kernel on KVM alone, and neither publishes nor persists anything.
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
    "run-openvmm-unit-tests",
    "run-openvmm-vmm-tests",
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
    r"performance)\.py$|adversarial_cases/))"
)
# Inputs of the platform jobs, which benchmark each backend and package its
# release, and of the regression gate that reads their results: the OpenVMM
# pin, the guests and every input of their cache keys, the nvx-rs executable
# that the packages ship, the performance history and expected hosts, the
# release version and license, the workflows, actions, and CLI modules that
# these jobs and the jobs that build their artifacts run, the runners, and the
# attributes that check out their inputs on Windows. Every module of the CLI is
# an input but those that none of these jobs runs.
BENCHMARK_INPUT_PATHS = re.compile(
    r"^(openvmm($|/)|rust-toolchain\.toml$|scripts/setup/|\.gitattributes$|"
    r"nvx-rs/|kernel/|guest/|ubuntu/|azurelinux/|"
    r"docker/|\.dockerignore$|SOURCE-MANIFEST\.json$|"
    r"data/|VERSION$|LICENSE$|"
    r"\.github/workflows/(ci|build-openvmm-binary|run-platform)\.yml$|"
    r"\.github/actions/(build-guest-artifacts|build-openvmm|checkout-openvmm|"
    r"gate-performance|package-release|prepare-performance-results|"
    r"run-benchmark|sccache|setup-curl|validate-runner)/|"
    r"scripts/nvx\.py$|"
    r"scripts/nvx_tools/(?!(adversarial|adversarial_broker|adversarial_executor|"
    r"crate_release|create_linux_source_archive|managed_exec_tests|merge_queue|"
    r"microvm_durations|microvm_shards|microvm_tests|nightly_ci|"
    r"openvmm_process|openvmm_test_shards|runtime_adapter_tests|"
    r"sandbox_lifecycle_tests|sandbox_proxy_tests|sandbox_ram_upper_tests|"
    r"shard_plans)\.py$|"
    r"(microvm|openvmm_test)_shards\.json$|adversarial_cases/|"
    r"microvm_test_scripts/))"
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
# add the inputs of those artifacts' cache keys and of the job that builds them,
# and the modules and the partition that split the WHP tests into shards.
# Of nvx-rs, the Alpine key covers only the files that the guest binaries' build
# reads, through their source identity, so a change to its host side or tests
# leaves the VMM tests out.
OPENVMM_VMM_TEST_INPUT_PATHS = re.compile(
    r"^(openvmm($|/)|rust-toolchain\.toml$|scripts/setup/|"
    r"\.github/workflows/ci\.yml$|"
    r"\.github/actions/(build-guest-artifacts|checkout-openvmm|sccache|"
    r"setup-curl|validate-runner)/|"
    r"scripts/nvx\.py$|"
    r"scripts/nvx_tools/"
    r"(__init__|build|build_config|build_constants|ci|common|elf|"
    r"guest_binaries|guests|openvmm_test_shards|shard_plans)\.py$|"
    r"scripts/nvx_tools/openvmm_test_shards\.json$|"
    r"SOURCE-MANIFEST\.json$|docker/Dockerfile$|\.dockerignore$|kernel/|guest/(alpine|common)/|"
    rf"(?:{GuestRustBuildConstants.SOURCE_PATTERN.pattern})$)"
)


@dataclass(frozen=True)
class CiChanges:
    """The suites that a change runs, as the change detector reports them."""

    run_guest_artifacts: bool
    run_openvmm_binaries: bool
    run_openvmm_unit_tests: bool
    run_openvmm_vmm_tests: bool
    run_microvm_tests: bool
    run_benchmarks: bool

    def outputs(self) -> dict[str, bool]:
        return dict(
            zip(
                CI_CHANGE_OUTPUTS,
                (
                    self.run_guest_artifacts,
                    self.run_openvmm_binaries,
                    self.run_openvmm_unit_tests,
                    self.run_openvmm_vmm_tests,
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
    request or a merge group runs the platform jobs only when one of their
    inputs changed, but a push runs them for every change but documentation,
    so that every such `dev` commit publishes its development release and
    records its performance results. The guest artifacts and the OpenVMM
    binaries are built exactly when a job that uses them runs.
    """
    if event_name not in CI_CLASSIFIED_EVENTS:
        raise ValueError(f"unsupported CI event {event_name!r}")

    files = [path for path in changed_files if path]
    inputs = _ci_inputs(files)

    def changed(pattern: re.Pattern[str]) -> bool:
        return any(pattern.match(path) for path in inputs)

    run_openvmm_vmm_tests = changed(OPENVMM_VMM_TEST_INPUT_PATHS)
    run_microvm_tests = changed(MICROVM_TEST_INPUT_PATHS)
    # An empty diff runs the platform jobs, as it ran every workload before
    # the classifier selected them separately.
    run_benchmarks = (
        not files
        or changed(BENCHMARK_INPUT_PATHS)
        or (event_name == "push" and bool(inputs))
    )
    return CiChanges(
        run_guest_artifacts=run_openvmm_vmm_tests
        or run_microvm_tests
        or run_benchmarks,
        run_openvmm_binaries=run_microvm_tests or run_benchmarks,
        run_openvmm_unit_tests=changed(OPENVMM_UNIT_TEST_INPUT_PATHS),
        run_openvmm_vmm_tests=run_openvmm_vmm_tests,
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
        benchmarks_reason = "No file changed, so the platform jobs run."
    elif (
        changes.run_benchmarks
        and event_name == "push"
        and not any(BENCHMARK_INPUT_PATHS.match(path) for path in inputs)
    ):
        benchmarks_reason = (
            "A push runs the platform jobs for every change but documentation, "
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
                        "the OpenVMM VMM tests": changes.run_openvmm_vmm_tests,
                        "the NVX microVM tests": changes.run_microvm_tests,
                        "the platform benchmarks": changes.run_benchmarks,
                    },
                ),
            ),
            section(
                "OpenVMM binaries",
                changes.run_openvmm_binaries,
                users_reason(
                    {
                        "the NVX microVM tests": changes.run_microvm_tests,
                        "the platform benchmarks": changes.run_benchmarks,
                    },
                ),
            ),
            section(
                "OpenVMM unit tests",
                changes.run_openvmm_unit_tests,
                inputs_reason(
                    changes.run_openvmm_unit_tests, OPENVMM_UNIT_TEST_INPUT_PATHS
                ),
            ),
            section(
                "OpenVMM VMM tests",
                changes.run_openvmm_vmm_tests,
                inputs_reason(
                    changes.run_openvmm_vmm_tests, OPENVMM_VMM_TEST_INPUT_PATHS
                ),
            ),
            section(
                "NVX microVM tests",
                changes.run_microvm_tests,
                inputs_reason(changes.run_microvm_tests, MICROVM_TEST_INPUT_PATHS),
            ),
            section("Platform benchmarks", changes.run_benchmarks, benchmarks_reason),
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
                if repository_jobs_enabled and changes.run_openvmm_vmm_tests
                else "skipped"
            )
            for job in REQUIRED_CI_OPENVMM_TEST_BUILD_JOBS
        }
    )
    expected.update(
        {
            job: (
                "success"
                if repository_jobs_enabled
                and changes.run_openvmm_vmm_tests
                and run_guest_artifacts
                else "skipped"
            )
            for job in REQUIRED_CI_OPENVMM_ARTIFACT_TEST_JOBS
        }
    )
    expected.update(
        {
            job: (
                "success"
                if repository_jobs_enabled and run_microvm_tests
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
                if event_name not in CI_PRE_MERGE_EVENTS and run_microvm_tests
                else "skipped"
            )
            for job in REQUIRED_CI_MICROVM_DEBUG_PUSH_JOBS
        }
    )
    expected.update(
        {
            job: (
                "success" if repository_jobs_enabled and run_benchmarks else "skipped"
            )
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
# which flowey reads from "hyperv" in a test's name, because test-openvmm runs
# the WHP tests that a build hands over without vmm-tests-run-target, which
# checks for Hyper-V on Windows.
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


def _select_openvmm_tests(
    tests: tuple[str, ...],
    excluded_tests: tuple[str, ...],
) -> tuple[str, ...]:
    return tuple(test for test in tests if test not in excluded_tests)


OPENVMM_TEST_FILTERS = {
    "kvm": OPENVMM_KVM_TEST_FILTER,
    "mshv": OPENVMM_MSHV_TEST_FILTER,
    "whp": _join_openvmm_tests(OPENVMM_WHP_TESTS, OPENVMM_WHP_EXCLUDED_TESTS),
}
# The tests that a backend's filter selects by their exact names, which is
# what a partition of its tests into shards lists. The KVM and MSHV filters
# select tests by pattern, so only the WHP tests have shards
# (doc/ci.md#openvmm-vmm-test-shards).
OPENVMM_TEST_SELECTIONS = {
    "whp": _select_openvmm_tests(OPENVMM_WHP_TESTS, OPENVMM_WHP_EXCLUDED_TESTS),
}


@dataclass(frozen=True)
class OpenVMMTestShard:
    """Shard INDEX of the COUNT shards of a backend's VMM tests, with the
    tests that it runs."""

    index: int
    count: int
    tests: tuple[str, ...]


def openvmm_test_filter(backend: str, shard: OpenVMMTestShard | None = None) -> str:
    """Return the filter of the backend's tests, or of those of its tests that
    a shard runs: only tests that the backend's filter selects, whatever the
    shard lists."""
    test_filter = OPENVMM_TEST_FILTERS[backend]
    if shard is None:
        return test_filter
    if not shard.tests:
        raise ScriptError(
            f"shard {shard.index} of {shard.count} of the {backend.upper()} "
            "OpenVMM tests runs no test"
        )
    return f"({test_filter}) & {_join_openvmm_tests(shard.tests)}"


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


def run_openvmm_tests(
    backend: str,
    content: Path | None = None,
    *,
    shard: OpenVMMTestShard | None = None,
) -> None:
    """Run the backend's VMM tests, or those of a shard of them, building them
    unless ``content`` names a test content directory that
    ``build_openvmm_tests`` built."""
    validate_openvmm_test_backend(backend)
    test_filter = openvmm_test_filter(backend, shard)
    if content is not None:
        _run_openvmm_test_content(backend, content, shard, test_filter)
        return

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
        test_filter,
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


def openvmm_test_build_filter(backends: Sequence[str]) -> str:
    """Return the filter that selects every test of the backends' filters."""
    filters = [
        OPENVMM_TEST_FILTERS[backend]
        for backend in OPENVMM_TEST_BACKENDS
        if backend in backends
    ]
    if len(filters) == 1:
        return filters[0]
    return " | ".join(f"({test_filter})" for test_filter in filters)


def _openvmm_test_content_backends(backends: Sequence[str]) -> tuple[str, ...]:
    if not backends:
        raise ScriptError("select at least one OpenVMM test backend")
    for backend in backends:
        if backend not in OPENVMM_TEST_BACKENDS:
            choices = ", ".join(OPENVMM_TEST_BACKENDS)
            raise ScriptError(
                f"unsupported OpenVMM test backend {backend!r}; choose {choices}"
            )
    host = "windows" if os.name == "nt" else "linux"
    for backend in backends:
        if OPENVMM_TEST_OPERATING_SYSTEMS[backend] != host:
            raise ScriptError(
                f"{backend.upper()} OpenVMM tests build on "
                f"{OPENVMM_TEST_OPERATING_SYSTEMS[backend].capitalize()}"
            )
    return tuple(backend for backend in OPENVMM_TEST_BACKENDS if backend in backends)


def _content_relative_path(value: object) -> PurePosixPath | None:
    """Return a path inside a test content directory, or None if ``value``
    names none.

    Manifests and run scripts name files with forward slashes, so a backslash
    or a drive, which Windows would resolve elsewhere, names no such path.
    """
    if not isinstance(value, str) or "\\" in value or ":" in value:
        return None
    path = PurePosixPath(value)
    if not path.parts or path.is_absolute() or ".." in path.parts:
        return None
    return path


def _openvmm_test_run_target(script: Path, build_filter: str) -> tuple[str, list[str]]:
    """Return the runner and the options other than --dir and --filter of the
    vmm-tests-run-target command that a build-only run script runs.

    The pinned flowey writes the command after a `cd` to the script's
    directory, as its arguments joined with spaces, and quotes only the filter.
    """
    text = require_file(script, "OpenVMM test run script").read_text(encoding="utf-8")
    quoted_filter = f" --filter '{build_filter}'"
    _, separator, command = text.strip().partition(" ; ")
    if not separator or command.count(quoted_filter) != 1:
        raise ScriptError(f"{script} does not run the filter that it was built for")
    tokens = command.replace(quoted_filter, "", 1).split()
    if tokens[1:4] != ["pipeline", "run", "vmm-tests-run-target"]:
        raise ScriptError(f"{script} does not run vmm-tests-run-target")
    runner = tokens[0]
    runner_path = _content_relative_path(runner[2:].replace("\\", "/"))
    if not runner.startswith(("./", ".\\")) or runner_path is None:
        raise ScriptError(f"{script} runs {runner}, outside its directory")
    arguments: list[str] = []
    directory: str | None = None
    options = iter(tokens[4:])
    for option in options:
        if option == "--dir":
            directory = next(options, None)
            continue
        takes_value = OPENVMM_TEST_RUN_TARGET_OPTIONS.get(option)
        if takes_value is None:
            raise ScriptError(f"{script} passes unsupported option {option}")
        arguments.append(option)
        if takes_value:
            value = next(options, None)
            if value is None or value.startswith("--"):
                raise ScriptError(f"{script} passes {option} without a value")
            arguments.append(value)
    if directory != "." or "--target" not in arguments:
        raise ScriptError(f"{script} does not run the tests in its own directory")
    return runner_path.as_posix(), arguments


def _openvmm_test_content_files(directory: Path) -> list[dict[str, object]]:
    files: list[dict[str, object]] = []
    paths = {
        path.relative_to(directory).as_posix(): path for path in directory.rglob("*")
    }
    for relative, path in sorted(paths.items()):
        metadata = path.lstat()
        if stat.S_ISDIR(metadata.st_mode) or relative == OPENVMM_TEST_CONTENT_MANIFEST:
            continue
        if not stat.S_ISREG(metadata.st_mode):
            raise ScriptError(f"OpenVMM test content has a non-regular file {relative}")
        files.append(
            {
                "path": relative,
                "size": metadata.st_size,
                # Artifact uploads drop file modes, so the consumer restores
                # these.
                "executable": os.name != "nt" and bool(metadata.st_mode & 0o111),
                "sha256": sha256_file(path),
            }
        )
    return files


def _openvmm_gitlink() -> str:
    """Return the OpenVMM revision that the checkout's index pins."""
    gitlink = run_capture(
        ["git", "-C", BuildConstants.REPO_ROOT, "rev-parse", ":openvmm"]
    )
    require_success(gitlink, "OpenVMM gitlink query")
    return gitlink.stdout.decode("ascii").strip()


def _read_openvmm_test_manifest(path: Path) -> dict[str, object]:
    try:
        document = cast(
            object,
            json.loads(
                path.read_text(encoding="utf-8"),
                object_pairs_hook=strict_json_object,
            ),
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ScriptError(f"{path} is not valid JSON: {error}") from error
    if not isinstance(document, dict):
        raise ScriptError(f"{path} is not a JSON object")
    return cast(dict[str, object], document)


def _openvmm_test_content_inputs(
    selected: Sequence[str], revision: str, clean: bool
) -> dict[str, object]:
    """Return the fields of the manifest of the content that
    ``build_openvmm_tests`` builds for the selected backends from an OpenVMM
    checkout at ``revision`` that describe what built it: everything that
    determines the content but the build itself."""
    operating_system = OPENVMM_TEST_OPERATING_SYSTEMS[selected[0]]
    return {
        "format": OPENVMM_TEST_CONTENT_FORMAT,
        "openvmm_revision": revision,
        "openvmm_clean": clean,
        "rust_toolchain": OpenVMMBuildConstants.RUST_TOOLCHAIN,
        "operating_system": operating_system,
        "target": OPENVMM_TEST_TARGETS[operating_system],
        "build_options": list(OPENVMM_TEST_BUILD_OPTIONS),
        "filters": {backend: OPENVMM_TEST_FILTERS[backend] for backend in selected},
        "build_filter": openvmm_test_build_filter(selected),
    }


def build_openvmm_tests(backends: Sequence[str], output: Path) -> None:
    """Build, without running them, the VMM tests of every named backend into
    a test content directory that ``run_openvmm_tests`` runs on another host.

    The directory holds what OpenVMM's `vmm-tests-run-target` needs, and a
    manifest that binds it to the OpenVMM revision, the Rust release, the
    target, the build options, and the backends' filters that built it, and
    lists the size and SHA-256 of each of its files.
    """
    selected = _openvmm_test_content_backends(backends)
    require_file(
        OpenVMMBuildConstants.DIRECTORY / "Cargo.toml", "initialized OpenVMM submodule"
    )
    if path_exists(output) and (not output.is_dir() or any(output.iterdir())):
        raise ScriptError(f"{output} is not an empty directory")
    cargo = require_tool("cargo")
    rustup = require_tool("rustup")
    revision, clean = openvmm_git_state(OpenVMMBuildConstants.DIRECTORY)
    operating_system = OPENVMM_TEST_OPERATING_SYSTEMS[selected[0]]
    target = OPENVMM_TEST_TARGETS[operating_system]
    build_filter = openvmm_test_build_filter(selected)
    if "'" in build_filter:
        raise ScriptError("OpenVMM run scripts cannot quote a filter with a quote")

    environment = _prepare_openvmm_test_environment(selected, rustup)
    run_checked(
        [cargo, "xflowey", "restore-packages", "--no-compat-igvm"],
        cwd=OpenVMMBuildConstants.DIRECTORY,
        env=environment,
    )
    output.mkdir(parents=True, exist_ok=True)
    run_checked(
        [
            cargo,
            "xflowey",
            "vmm-tests-run",
            "--target",
            target,
            *OPENVMM_TEST_BUILD_OPTIONS,
            "--filter",
            build_filter,
            "--dir",
            os.fspath(output.resolve()),
        ],
        cwd=OpenVMMBuildConstants.DIRECTORY,
        env=environment,
    )

    script = output / OPENVMM_TEST_RUN_SCRIPTS[operating_system]
    runner, arguments = _openvmm_test_run_target(script, build_filter)
    if arguments[arguments.index("--target") + 1] != target:
        raise ScriptError(f"{script} does not run the {target} tests that it built")
    files = _openvmm_test_content_files(output)
    manifest = {
        **_openvmm_test_content_inputs(selected, revision, clean),
        "runner": runner,
        "arguments": arguments,
        "files": files,
    }
    (output / OPENVMM_TEST_CONTENT_MANIFEST).write_text(
        json.dumps(manifest, indent=2) + "\n", encoding="utf-8"
    )
    size = sum(cast(int, entry["size"]) for entry in files)
    print(
        f"Built the {', '.join(backend.upper() for backend in selected)} OpenVMM "
        f"tests into {output}: {len(files)} files, {bytes_to_mib(size):.1f} MiB"
    )


@dataclass(frozen=True)
class _OpenVMMTestCacheEntry:
    """The content of some backends that CI caches: a clean build of the
    OpenVMM revision that the checkout pins, the fields of its manifest that
    describe what built it, and its cache key."""

    backends: tuple[str, ...]
    inputs: Mapping[str, object]
    key: str

    def describe(self) -> str:
        return ", ".join(backend.upper() for backend in self.backends)


def _openvmm_test_cache_entry(backends: Sequence[str]) -> _OpenVMMTestCacheEntry:
    selected = _openvmm_test_content_backends(backends)
    inputs = _openvmm_test_content_inputs(selected, _openvmm_gitlink(), True)
    canonical = json.dumps(inputs, sort_keys=True, separators=(",", ":"))
    digest = hashlib.sha256(canonical.encode("utf-8")).hexdigest()
    return _OpenVMMTestCacheEntry(
        backends=selected,
        inputs=inputs,
        key=(
            f"openvmm-vmm-tests-v{OPENVMM_TEST_CACHE_VERSION}-{inputs['target']}-"
            f"{inputs['openvmm_revision']}-{digest}"
        ),
    )


def _set_step_output(environment: Mapping[str, str], name: str, value: str) -> None:
    """Give the GitHub Actions step that runs the command an output."""
    path = environment.get("GITHUB_OUTPUT")
    if path:
        with Path(path).open("a", encoding="utf-8", newline="\n") as stream:
            stream.write(f"{name}={value}\n")


def openvmm_test_cache_key(
    backends: Sequence[str], environment: Mapping[str, str] | None = None
) -> str:
    """Print and return the key under which CI caches the content that
    ``build_openvmm_tests`` builds for the backends from this checkout, and
    give the step that asks for it the `key` output."""
    key = _openvmm_test_cache_entry(backends).key
    print(key)
    _set_step_output(environment or {}, "key", key)
    return key


def _brief(value: object) -> str:
    text = json.dumps(value)
    return text if len(text) <= 80 else f"{text[:77]}..."


def _openvmm_test_file_refusal(
    listed: object, files: Sequence[Mapping[str, object]]
) -> str | None:
    """Return how a manifest's list of files differs from the files of its
    content, as _openvmm_test_content_files lists them, or None if it lists
    exactly those files."""
    if listed == files:
        return None
    present = {str(entry["path"]): entry for entry in files}
    recorded = {
        str(cast(dict[str, object], entry).get("path")): cast(dict[str, object], entry)
        for entry in (cast(list[object], listed) if isinstance(listed, list) else [])
        if isinstance(entry, dict)
    }
    missing = sorted(recorded.keys() - present.keys())
    if missing:
        return f"it lacks {missing[0]}, which its manifest lists"
    unlisted = sorted(present.keys() - recorded.keys())
    if unlisted:
        return f"its manifest does not list {unlisted[0]}"
    for path, entry in sorted(present.items()):
        for field in ("size", "sha256", "executable"):
            if recorded[path].get(field) != entry[field]:
                return f"the {field} of {path} differs from its manifest's"
    return "its manifest does not list exactly its files"


def _openvmm_test_content_refusal(
    entry: _OpenVMMTestCacheEntry, content: Path
) -> str | None:
    """Return why ``content`` does not hold the test content that ``entry``
    describes, or None if it holds exactly that: a manifest that records the
    entry's fields, and the runner and options that the content's run script
    runs, and exactly the files that the manifest lists, with their sizes,
    SHA-256, and executable bits."""
    path = content / OPENVMM_TEST_CONTENT_MANIFEST
    if not path.is_file():
        return f"it has no {OPENVMM_TEST_CONTENT_MANIFEST}"
    try:
        manifest = _read_openvmm_test_manifest(path)
        for field, expected in entry.inputs.items():
            if manifest.get(field) != expected:
                return (
                    f"its manifest records the {field} {_brief(manifest.get(field))}"
                    f", not {_brief(expected)}"
                )
        operating_system = cast(str, entry.inputs["operating_system"])
        script = content / OPENVMM_TEST_RUN_SCRIPTS[operating_system]
        runner, arguments = _openvmm_test_run_target(
            script, cast(str, entry.inputs["build_filter"])
        )
        if arguments[arguments.index("--target") + 1] != entry.inputs["target"]:
            return f"{script.name} runs the tests of another target"
        if manifest.get("runner") != runner or manifest.get("arguments") != arguments:
            return (
                f"its manifest records another runner or other options than "
                f"{script.name} runs"
            )
        files = _openvmm_test_content_files(content)
    except (OSError, ScriptError, UnicodeDecodeError) as error:
        return str(error)
    return _openvmm_test_file_refusal(manifest.get("files"), files)


def check_openvmm_tests(backends: Sequence[str], content: Path) -> None:
    """Fail unless ``content`` holds exactly the content that
    ``build_openvmm_tests`` builds for the backends from this checkout, which
    CI caches under ``openvmm_test_cache_key``."""
    entry = _openvmm_test_cache_entry(backends)
    refusal = _openvmm_test_content_refusal(entry, content)
    if refusal is not None:
        raise ScriptError(
            f"{content} does not hold the {entry.describe()} OpenVMM tests of "
            f"cache key {entry.key}: {refusal}"
        )
    print(
        f">> {content} holds the {entry.describe()} OpenVMM tests of cache key "
        f"{entry.key}"
    )


def _annotation(text: str) -> str:
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def reuse_openvmm_tests(
    backends: Sequence[str],
    content: Path,
    environment: Mapping[str, str] | None = None,
) -> bool:
    """Keep ``content``, which a cache restored, if it holds exactly the
    content that ``build_openvmm_tests`` builds for the backends from this
    checkout, and remove whatever it holds otherwise, so that a build can take
    its place. Report the outcome, warn of a refusal, give the step the
    `reused` output, and return whether the content is reused."""
    entry = _openvmm_test_cache_entry(backends)
    if not path_exists(content):
        reused = False
        print(
            f">> {content} holds no {entry.describe()} OpenVMM tests of cache key "
            f"{entry.key} to reuse"
        )
    else:
        refusal = _openvmm_test_content_refusal(entry, content)
        reused = refusal is None
        if reused:
            print(
                f">> reusing the {entry.describe()} OpenVMM tests of cache key "
                f"{entry.key} in {content}"
            )
        else:
            detail = (
                f"{content} does not hold the {entry.describe()} OpenVMM tests of "
                f"cache key {entry.key}: {refusal}. Removed it, so that the tests "
                "build instead."
            )
            print(f"::warning title=OpenVMM test cache::{_annotation(detail)}")
            if content.is_dir() and not is_link_or_junction(content):
                shutil.rmtree(content)
            else:
                content.unlink()
    _set_step_output(environment or {}, "reused", "true" if reused else "false")
    return reused


@dataclass(frozen=True)
class _OpenVMMTestContent:
    """What runs the tests of a checked test content directory."""

    runner: Path
    arguments: tuple[str, ...]
    files: tuple[PurePosixPath, ...]


def _openvmm_test_content(content: Path, backend: str) -> _OpenVMMTestContent:
    """Check that the content runs this checkout's tests of the backend, and
    return its runner, its vmm-tests-run-target options, and its files."""
    path = require_file(
        content / OPENVMM_TEST_CONTENT_MANIFEST, "OpenVMM test content manifest"
    )
    manifest = _read_openvmm_test_manifest(path)
    if manifest.get("format") != OPENVMM_TEST_CONTENT_FORMAT:
        raise ScriptError(
            f"{path} has format {manifest.get('format')!r}, "
            f"expected {OPENVMM_TEST_CONTENT_FORMAT}"
        )
    revision = _openvmm_gitlink()
    if manifest.get("openvmm_revision") != revision:
        raise ScriptError(
            f"{path} describes tests of OpenVMM "
            f"{manifest.get('openvmm_revision')}, but this checkout pins {revision}"
        )
    filters = manifest.get("filters")
    if (
        not isinstance(filters, dict)
        or cast(dict[str, object], filters).get(backend)
        != OPENVMM_TEST_FILTERS[backend]
    ):
        raise ScriptError(
            f"{path} describes no tests of this checkout's {backend.upper()} filter"
        )

    runner = _content_relative_path(manifest.get("runner"))
    arguments = manifest.get("arguments")
    files = manifest.get("files")
    if (
        runner is None
        or not isinstance(arguments, list)
        or not all(isinstance(item, str) for item in cast(list[object], arguments))
        or not isinstance(files, list)
    ):
        raise ScriptError(f"{path} is malformed")
    listed: list[PurePosixPath] = []
    for entry in cast(list[object], files):
        details = cast(dict[str, object], entry) if isinstance(entry, dict) else {}
        relative = _content_relative_path(details.get("path"))
        if relative is None:
            raise ScriptError(f"{path} lists a file outside its directory")
        file = content.joinpath(*relative.parts)
        if not file.is_file() or file.stat().st_size != details.get("size"):
            raise ScriptError(f"OpenVMM test content in {content} lacks {relative}")
        # Artifact downloads drop the file modes that the build recorded.
        if details.get("executable") is True:
            file.chmod(file.stat().st_mode | 0o111)
        listed.append(relative)
    return _OpenVMMTestContent(
        runner=content.joinpath(*runner.parts),
        arguments=tuple(cast(list[str], arguments)),
        files=tuple(listed),
    )


def _run_openvmm_test_content(
    backend: str,
    content: Path,
    shard: OpenVMMTestShard | None,
    test_filter: str,
) -> None:
    kernel, initrd = _openvmm_microvm_test_guest()
    root = content.resolve()
    built = _openvmm_test_content(root, backend)

    environment = os.environ.copy()
    environment["RUSTUP_TOOLCHAIN"] = OpenVMMBuildConstants.RUST_TOOLCHAIN
    _set_openvmm_test_cache(environment, (backend,))
    environment["OPENVMM_MICROVM_TEST_KERNEL"] = os.fspath(kernel.resolve())
    environment["OPENVMM_MICROVM_TEST_INITRD"] = os.fspath(initrd.resolve())
    # An earlier run's reports must not pass for this run's.
    junit = root.joinpath(*OPENVMM_TEST_JUNIT.parts)
    durations = root / OPENVMM_TEST_RESULTS / OPENVMM_TEST_DURATIONS_NAME
    for report in (junit, durations):
        report.unlink(missing_ok=True)
    try:
        if OPENVMM_TEST_OPERATING_SYSTEMS[backend] == "windows":
            _run_openvmm_test_archive(root, built, environment, test_filter)
        else:
            run_checked(
                [
                    built.runner,
                    "pipeline",
                    "run",
                    "vmm-tests-run-target",
                    *built.arguments,
                    "--dir",
                    os.fspath(root),
                    "--filter",
                    test_filter,
                ],
                cwd=root,
                env=environment,
            )
    finally:
        _record_openvmm_test_durations(junit, durations, backend, shard, test_filter)


def _openvmm_test_junit_durations(junit: Path) -> list[dict[str, object]]:
    """Return each test case of a nextest JUnit report: its binary, its test,
    the seconds that it ran, and its outcome, which is passed, failed, flaky
    for a test that passed only when retried, or skipped."""
    tests: list[dict[str, object]] = []
    for _, element in ElementTree.iterparse(junit):
        if element.tag != "testcase":
            continue
        children = {child.tag for child in element}
        if children & {"failure", "error"}:
            outcome = "failed"
        elif "skipped" in children:
            outcome = "skipped"
        elif children & {"flakyFailure", "flakyError"}:
            outcome = "flaky"
        else:
            outcome = "passed"
        name = element.get("name")
        seconds = float(element.get("time", "nan"))
        if not name or not math.isfinite(seconds) or seconds < 0:
            raise ValueError(f"{junit} has a test case without a name or a time")
        tests.append(
            {
                "binary": element.get("classname"),
                "test": name,
                "seconds": round(seconds, 3),
                "outcome": outcome,
            }
        )
        # The cases keep the output of the tests, which the report drops.
        element.clear()
    return tests


def _record_openvmm_test_durations(
    junit: Path,
    report: Path,
    backend: str,
    shard: OpenVMMTestShard | None,
    test_filter: str,
) -> None:
    """Write the test duration report of a content run from the JUnit report
    of its nextest run, if it wrote one, as the run ends, whether its tests
    passed or not. A report that cannot be written only warns, so that it
    never hides the run's own outcome."""
    if not junit.is_file():
        print(f">> recorded no test durations: nextest wrote no {junit}", flush=True)
        return
    try:
        tests = _openvmm_test_junit_durations(junit)
        document = {
            "schema": OPENVMM_TEST_DURATIONS_SCHEMA,
            "schema_version": OPENVMM_TEST_DURATIONS_SCHEMA_VERSION,
            "backend": backend,
            "shard": (
                None if shard is None else {"index": shard.index, "count": shard.count}
            ),
            "filter": test_filter,
            # nextest runs as many test threads by default.
            "logical_processors": os.cpu_count(),
            "github": github_job(os.environ),
            "tests": tests,
        }
        report.parent.mkdir(parents=True, exist_ok=True)
        report.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    except (OSError, ValueError, ElementTree.ParseError) as error:
        print(
            f"warning: could not record the test durations in {report}: {error}",
            file=sys.stderr,
            flush=True,
        )
        return
    print(f">> recorded the durations of {len(tests)} tests in {report}", flush=True)


@dataclass(frozen=True)
class _OpenVMMTestArchiveRun:
    """What a run script's vmm-tests-run-target options ask of a run."""

    profile: str
    images: tuple[str, ...]
    reuse_prepped_vhds: bool


def _openvmm_test_archive_run(arguments: Sequence[str]) -> _OpenVMMTestArchiveRun:
    """Return what the vmm-tests-run-target options of Windows test content ask
    of a run of its archive, refusing any that only the runner carries out."""
    target: str | None = None
    profile = "default"
    images: list[str] = []
    reuse_prepped_vhds = True
    options = iter(arguments)
    for option in options:
        takes_value = OPENVMM_TEST_RUN_TARGET_OPTIONS.get(option)
        if takes_value is None:
            raise ScriptError(
                f"OpenVMM test content passes unsupported option {option}"
            )
        value = next(options, None) if takes_value else ""
        if value is None or value.startswith("--"):
            raise ScriptError(f"OpenVMM test content passes {option} without a value")
        if option == "--target":
            target = value
        elif option == "--repetitions":
            if value != "1":
                raise ScriptError(
                    f"OpenVMM test content repeats its tests {value} times; "
                    "test-openvmm runs them once"
                )
        elif option == "--artifacts":
            images.extend(value.split(","))
        elif option == "--ci-profile":
            profile = "ci"
        elif option == "--no-reuse-prepped-vhds":
            reuse_prepped_vhds = False
        elif option != "--skip-vhd-prompt":
            raise ScriptError(
                f"OpenVMM test content passes {option}, which only its "
                "vmm-tests-run-target carries out"
            )
    if target != "windows-x64":
        raise ScriptError(
            f"OpenVMM test content targets {target}; test-openvmm runs the "
            "archive of windows-x64 content"
        )
    return _OpenVMMTestArchiveRun(
        profile=profile,
        images=tuple(images),
        reuse_prepped_vhds=reuse_prepped_vhds,
    )


def _download_openvmm_test_images(names: Sequence[str], directory: Path) -> None:
    """Download each named image into the directory, as vmm-tests-run-target
    does, unless the directory holds it at its declared size."""
    for name in names:
        image = OPENVMM_TEST_IMAGES.get(name)
        if image is None:
            known = ", ".join(sorted(OPENVMM_TEST_IMAGES))
            raise ScriptError(
                f"OpenVMM test content needs the {name} image; "
                f"test-openvmm downloads only {known}"
            )
        filename, size = image
        path = directory / filename
        if path.is_file() and path.stat().st_size == size:
            continue
        print(f">> downloading {name} to {path}", flush=True)
        download(f"{OPENVMM_TEST_IMAGE_URL}/{filename}", path)
        actual = path.stat().st_size
        if actual != size:
            path.unlink()
            raise ScriptError(
                f"{filename} has {actual} bytes, but OpenVMM declares {size}"
            )


def _run_openvmm_test_archive(
    root: Path,
    content: _OpenVMMTestContent,
    environment: dict[str, str],
    test_filter: str,
) -> None:
    """Run a filter of the WHP tests, or of a shard of them, from the nextest
    archive of Windows test content, as its vmm-tests-run-target does after it
    checks the host.

    The pinned vmm-tests-run-target requires Hyper-V on every Windows target,
    whatever tests it runs: `Get-VM` must succeed and two registry values must
    be set, or the account must be an administrator, which the WHP runners'
    Network Service account is not. The WHP filter selects no Hyper-V test, so
    `vmm-tests-run`, which derives the dependencies from the selected tests,
    checks nothing there. This takes the runner's remaining steps: it downloads
    the images that the content names, sets the environment that Petri reads,
    and runs the archive's tests with cargo-nextest, the content's nextest
    configuration, and the content as the workspace.
    """
    run = _openvmm_test_archive_run(content.arguments)
    archives = [
        path for path in content.files if path.name == OPENVMM_TEST_ARCHIVE_NAME
    ]
    if len(archives) != 1:
        raise ScriptError(
            f"OpenVMM test content in {root} holds {len(archives)} "
            f"{OPENVMM_TEST_ARCHIVE_NAME} archives, expected 1"
        )
    configuration = PurePosixPath(".config", "nextest.toml")
    if configuration not in content.files:
        raise ScriptError(f"OpenVMM test content in {root} lacks {configuration}")
    nextest = require_tool("cargo-nextest")

    # The runner downloads the images where VMM_TEST_IMAGES names, by default
    # in the content, and recreates the test results that Petri writes.
    images = Path(os.environ.get("VMM_TEST_IMAGES") or root / "images").resolve()
    _download_openvmm_test_images(run.images, images)
    results = root / OPENVMM_TEST_RESULTS
    if path_exists(results):
        shutil.rmtree(results)
    results.mkdir()
    petri = {
        "VMM_TESTS_CONTENT_DIR": os.fspath(root),
        "TEST_OUTPUT_PATH": os.fspath(results),
        "VMM_TEST_IMAGES": os.fspath(images),
        # The runner sets these whatever its caller's environment holds.
        "RUST_LOG": "trace",
        "RUST_BACKTRACE": "1",
    }
    if run.reuse_prepped_vhds:
        petri["PETRI_REUSE_PREPPED_VHDS"] = "1"
    environment.update(petri)
    command = [
        nextest,
        "nextest",
        "run",
        "--profile",
        run.profile,
        "--config-file",
        os.fspath(root.joinpath(*configuration.parts)),
        "--workspace-remap",
        os.fspath(root),
        "--archive-file",
        os.fspath(root.joinpath(*archives[0].parts)),
        "--filter-expr",
        test_filter,
    ]
    # Log what the runner logs before it runs the tests.
    for name, value in sorted(petri.items()):
        print(f">> {name}={value}", flush=True)
    print(f">> {shlex.join(command)}", flush=True)
    run_checked(command, cwd=root, env=environment)


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
