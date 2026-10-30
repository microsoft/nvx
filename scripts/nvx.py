#!/usr/bin/env python3
"""Build, test, benchmark, and package the OpenVMM/NVX distribution."""

from __future__ import annotations

import argparse
import json
import os
import shlex
import signal
import subprocess
import sys
import threading
from collections.abc import Generator, Sequence
from contextlib import contextmanager
from pathlib import Path

from nvx_tools import nvx_rs
from nvx_tools.adversarial import configure_parser as configure_adversarial_parser
from nvx_tools.benchmark import configure_parser as configure_benchmark_parser
from nvx_tools.benchmark import run as run_benchmark
from nvx_tools.build import (
    build_all,
    build_distro_layer,
    build_docker_initramfs,
    build_guest,
    build_initramfs,
    build_kernel,
    build_openvmm,
    build_scratch_template,
    materialize_kernel_provenance_inputs,
    record_openvmm_provenance,
    verify_guest_determinism,
)
from nvx_tools.build_config import (
    BuildConfig,
    DistroLayerBuildConfig,
    DockerBuildConfig,
    KernelBuildConfig,
    OpenVmmBuildConfig,
    ScratchTemplateBuildConfig,
)
from nvx_tools.build_constants import (
    BuildConstants,
    InitramfsBuildConstants,
    KernelBuildConstants,
    UbuntuBuildConstants,
)
from nvx_tools.ci import (
    CI_CLASSIFIED_EVENTS,
    CI_EVENTS,
    OPENVMM_TEST_BACKENDS,
    REQUIRED_CI_RESULT_ENVIRONMENTS,
    CiChanges,
    build_openvmm_tests,
    check_openvmm_tests,
    classify_ci_changes,
    describe_ci_changes,
    kernel_patch_identity,
    openvmm_test_cache_key,
    required_ci_failures,
    reuse_openvmm_tests,
    run_openvmm_tests,
    run_openvmm_unit_tests,
    setup_cross_os_cache,
)
from nvx_tools.collect_alpine_sources import (
    configure_parser as configure_alpine_sources_parser,
)
from nvx_tools.collect_ubuntu_sources import (
    configure_parser as configure_ubuntu_sources_parser,
)
from nvx_tools.common import (
    ScriptError,
    artifact_path,
)
from nvx_tools.create_linux_source_archive import (
    configure_parser as configure_linux_source_archive_parser,
)
from nvx_tools.doctor import configure_parser as configure_doctor_parser
from nvx_tools.doctor import host_cpu_signature
from nvx_tools.elf import check_static_executables
from nvx_tools.guest_artifact_reuse import (
    parse_cache_keys,
    parse_components,
    record_guest_artifacts,
    reuse_in_ci,
)
from nvx_tools.guest_binaries import (
    build_guest_binaries,
    check_guest_binaries,
    guest_binary_source_sha256,
    test_guest_binaries,
)
from nvx_tools.guests import GUEST_NAMES, guest_descriptor
from nvx_tools.merge_queue import configure_parser as configure_merge_queue_parser
from nvx_tools.microvm_tests import configure_parser as configure_microvm_test_parser
from nvx_tools.microvm_tests import (
    configure_shard_parser as configure_microvm_shard_parser,
)
from nvx_tools.nightly_ci import configure_parser as configure_nightly_ci_parser
from nvx_tools.openvmm_test_shards import (
    configure_parser as configure_openvmm_test_shard_parser,
)
from nvx_tools.openvmm_test_shards import select_shard as select_openvmm_test_shard
from nvx_tools.performance import configure_parser as configure_performance_parser
from nvx_tools.release import (
    collect_release_sources,
    create_release_archive,
    package_release,
    verify_source_tree,
)
from nvx_tools.runtime_adapter_tests import (
    configure_parser as configure_runtime_adapter_test_parser,
)
from nvx_tools.shard_plans import parse_shard
from nvx_tools.time_abi import (
    benchmark_cpu_profile_refusal,
    openvmm_cpu_generations,
)


def _run(
    args: list[str | os.PathLike[str]], *, cwd: Path = BuildConstants.REPO_ROOT
) -> None:
    command = [os.fspath(arg) for arg in args]
    print(f">> {shlex.join(command)}")
    subprocess.run(command, cwd=cwd, check=True)


def command_init(_: argparse.Namespace) -> None:
    _run(["git", "submodule", "update", "--init", "--recursive"])


def _openvmm_build_config(args: argparse.Namespace) -> OpenVmmBuildConfig:
    return OpenVmmBuildConfig(
        skip_restore=getattr(args, "skip_restore", False),
        backend=getattr(args, "backend", None),
    )


def _build_config(args: argparse.Namespace) -> BuildConfig:
    return BuildConfig(
        guest=getattr(args, "guest", InitramfsBuildConstants.DEFAULT_GUEST),
        native_guest=getattr(args, "native", False),
        debug_kernel=getattr(args, "debug_kernel", False),
        openvmm=_openvmm_build_config(args),
    )


def command_build_guest(args: argparse.Namespace) -> None:
    build_guest(_build_config(args))


def command_check_guest_binaries(args: argparse.Namespace) -> None:
    check_guest_binaries(args.toolchain)


def command_build_guest_binaries(args: argparse.Namespace) -> None:
    result = build_guest_binaries(args.output)
    print(
        json.dumps(
            {
                "source_sha256": result.source_sha256,
                "binaries": result.binaries,
            },
            indent=2,
            sort_keys=True,
        )
    )


def command_test_guest_binaries(args: argparse.Namespace) -> None:
    test_guest_binaries(args.binaries)


def command_build_kernel(args: argparse.Namespace) -> None:
    build_kernel(
        KernelBuildConfig.debug_variant()
        if getattr(args, "debug", False)
        else KernelBuildConfig()
    )


def command_build_initramfs(args: argparse.Namespace) -> None:
    if not guest_descriptor(args.guest).native_build_supported:
        build_docker_initramfs(DockerBuildConfig(), args.guest)
        return
    build_initramfs(BuildConfig.initramfs_config(args.guest))


def command_build_distro_layer(args: argparse.Namespace) -> None:
    build_distro_layer(
        DistroLayerBuildConfig(
            guest=args.guest,
            work=(
                BuildConstants.BUILD_DIR
                / InitramfsBuildConstants.DISTRO_WORK_DIRECTORY_TEMPLATE.format(
                    guest=args.guest
                )
            ),
            output=args.output,
            replace=args.replace,
        )
    )


def command_build_scratch_template(args: argparse.Namespace) -> None:
    build_scratch_template(
        ScratchTemplateBuildConfig(output=args.output, replace=args.replace)
    )


def command_verify_guest_determinism(args: argparse.Namespace) -> None:
    verify_guest_determinism(args.work_dir, args.guest)


def command_build_openvmm(args: argparse.Namespace) -> None:
    build_openvmm(_openvmm_build_config(args))


def command_record_openvmm_provenance(_: argparse.Namespace) -> None:
    record_openvmm_provenance(OpenVmmBuildConfig())


def command_record_nvx_rs_provenance(args: argparse.Namespace) -> None:
    path = nvx_rs.record_provenance(args.target)
    print(f">> recorded the provenance of nvx-rs in {path}")


def command_materialize_kernel_provenance_inputs(_: argparse.Namespace) -> None:
    materialize_kernel_provenance_inputs()


def command_setup_cross_os_cache(_: argparse.Namespace) -> None:
    setup_cross_os_cache()


def command_classify_ci_changes(args: argparse.Namespace) -> None:
    contents = args.changed_files.read_bytes().decode("utf-8")
    changed_files = contents.split("\0") if args.null else contents.splitlines()
    changes = classify_ci_changes(args.event_name, changed_files)
    if args.summary is not None:
        with args.summary.open("a", encoding="utf-8", newline="") as summary:
            summary.write(describe_ci_changes(args.event_name, changed_files, changes))
    for name, selected in changes.outputs().items():
        print(f"{name}={str(selected).lower()}")


def command_kernel_patch_identity(args: argparse.Namespace) -> None:
    print(kernel_patch_identity(args.root))


def command_guest_binary_source_identity(args: argparse.Namespace) -> None:
    print(guest_binary_source_sha256(args.root))


def command_record_guest_artifacts(args: argparse.Namespace) -> None:
    path = record_guest_artifacts(
        args.directory,
        parse_components(args.components),
        parse_cache_keys(args.keys),
    )
    print(f">> recorded the guest artifact keys and digests in {path}")


def command_reuse_guest_artifacts(args: argparse.Namespace) -> None:
    reuse_in_ci(
        repository=args.repository,
        workflow_ref=args.workflow_ref,
        record=args.record_artifact,
        components=args.components,
        keys=args.keys,
        commit=args.commit,
        head_sha=args.head_sha,
        dry_run=args.dry_run,
        destination=args.destination,
        environment=os.environ,
    )


def command_check_required_ci(args: argparse.Namespace) -> None:
    results = {
        job: os.environ.get(environment, "")
        for job, environment in REQUIRED_CI_RESULT_ENVIRONMENTS.items()
    }
    failures = required_ci_failures(
        args.event_name,
        same_repository=args.same_repository == "true",
        changes=CiChanges(
            run_guest_artifacts=args.run_guest_artifacts == "true",
            run_openvmm_binaries=args.run_openvmm_binaries == "true",
            run_openvmm_unit_tests=args.run_openvmm_unit_tests == "true",
            run_openvmm_vmm_tests=args.run_openvmm_vmm_tests == "true",
            run_microvm_tests=args.run_microvm_tests == "true",
            run_benchmarks=args.run_benchmarks == "true",
        ),
        results=results,
    )
    if failures:
        for failure in failures:
            print(f"::error::{failure}")
        raise ScriptError(f"{len(failures)} required CI job result(s) did not match")


def command_check_static_elf(args: argparse.Namespace) -> None:
    check_static_executables(args.paths)


def command_build_openvmm_tests(args: argparse.Namespace) -> None:
    build_openvmm_tests(args.backend, args.output)


def command_openvmm_test_cache_key(args: argparse.Namespace) -> None:
    openvmm_test_cache_key(args.backend, os.environ)


def command_check_openvmm_tests(args: argparse.Namespace) -> None:
    check_openvmm_tests(args.backend, args.content)


def command_reuse_openvmm_tests(args: argparse.Namespace) -> None:
    reuse_openvmm_tests(args.backend, args.content, os.environ)


def command_test_openvmm(args: argparse.Namespace) -> None:
    shard = (
        None
        if args.shard is None
        else select_openvmm_test_shard(args.backend, args.shard)
    )
    run_openvmm_tests(args.backend, args.content, shard=shard)


def command_test_openvmm_unit(_: argparse.Namespace) -> None:
    run_openvmm_unit_tests()


def command_benchmark(args: argparse.Namespace) -> int:
    """Run the benchmark coordinator, after refusing a host whose CPU no
    built-in CPU profile of the selected OpenVMM checkout serves before it
    builds or measures anything. Only the phase 2 suite, which boots no
    microVM, runs on any host. A KVM worker, which the Windows coordinator
    reinvokes inside WSL without its --openvmm-dir, runs the binaries that the
    coordinator staged after this check, so it skips the check."""
    if args.suite != "phase2" and not args._kvm_worker:
        try:
            generations = openvmm_cpu_generations(args.openvmm_dir)
        except ValueError as error:
            raise ScriptError(
                "benchmark cannot confirm that a built-in CPU profile serves this "
                f"host's CPU: {error}"
            ) from error
        hypervisor = "whp" if args.backend == "both" else args.backend
        refusal = benchmark_cpu_profile_refusal(
            host_cpu_signature(), "openvmm", hypervisor, generations
        )
        if refusal is not None:
            raise ScriptError(refusal)
    return run_benchmark(args)


def command_build(args: argparse.Namespace) -> None:
    build_all(_build_config(args))


def command_collect_sources(_: argparse.Namespace) -> None:
    collect_release_sources(DockerBuildConfig())


def command_package(args: argparse.Namespace) -> None:
    package_release(
        version=args.version,
        destination=args.destination,
        include_source=args.include_source,
        force=args.force,
    )


def command_archive_release(args: argparse.Namespace) -> None:
    create_release_archive(args.source, args.destination)


def command_verify(_: argparse.Namespace) -> None:
    verify_source_tree()


def _add_guest_options(
    parser: argparse.ArgumentParser,
    *,
    allow_all: bool,
) -> None:
    choices = (*GUEST_NAMES, "all") if allow_all else GUEST_NAMES
    parser.add_argument(
        "--guest",
        choices=choices,
        default=InitramfsBuildConstants.DEFAULT_GUEST,
        help=(
            "guest userland to build "
            f"(default: {InitramfsBuildConstants.DEFAULT_GUEST})"
        ),
    )
    parser.add_argument(
        "--native",
        action="store_true",
        help="build directly on Linux instead of using Docker",
    )
    parser.add_argument(
        "--debug-kernel",
        action="store_true",
        help=(
            "also build the CI debug kernel (build/vmlinux-debug), which adds "
            "the soft-lockup, hung-task, and RCU stall diagnostics"
        ),
    )


def _add_openvmm_options(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--skip-restore", action="store_true")
    parser.add_argument(
        "--backend",
        choices=OPENVMM_TEST_BACKENDS,
        help=(
            "select build target: kvm=GNU, mshv=musl, whp=MSVC "
            "(default: native target for the host OS)"
        ),
    )


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    init = subparsers.add_parser("init", help="initialize the private submodule")
    init.set_defaults(handler=command_init)

    guest = subparsers.add_parser("build-guest", help="build Linux guest artifacts")
    _add_guest_options(guest, allow_all=True)
    guest.set_defaults(handler=command_build_guest)

    guest_binary_checks = subparsers.add_parser(
        "check-guest-binaries",
        help="lint and test the Rust guest binaries for musl",
    )
    guest_binary_checks.add_argument(
        "--toolchain",
        help="Rust toolchain to use (default: the rust-toolchain.toml pin)",
    )
    guest_binary_checks.set_defaults(handler=command_check_guest_binaries)

    guest_binaries = subparsers.add_parser(
        "build-guest-binaries",
        help="build the pinned Rust executables for Linux guests",
    )
    guest_binaries.add_argument("--output", type=Path, required=True)
    guest_binaries.set_defaults(handler=command_build_guest_binaries)

    guest_binary_tests = subparsers.add_parser(
        "test-guest-binaries",
        help="test prebuilt Rust guest executables",
    )
    guest_binary_tests.add_argument("--binaries", type=Path, required=True)
    guest_binary_tests.set_defaults(handler=command_test_guest_binaries)

    kernel = subparsers.add_parser(
        "build-kernel",
        help="fetch, patch, and build the pinned kernel natively on Linux",
    )
    kernel.add_argument(
        "--debug",
        action="store_true",
        help=(
            "build the CI debug variant (build/vmlinux-debug) by applying "
            "kernel/config-microvm-debug"
        ),
    )
    kernel.set_defaults(handler=command_build_kernel)

    initramfs = subparsers.add_parser(
        "build-initramfs",
        help=(
            "build a selected guest initramfs (natively on Linux, or via "
            "Docker for guests that require it)"
        ),
    )
    initramfs.add_argument(
        "--guest",
        choices=GUEST_NAMES,
        default=InitramfsBuildConstants.DEFAULT_GUEST,
        help=(
            "guest userland to build "
            f"(default: {InitramfsBuildConstants.DEFAULT_GUEST})"
        ),
    )
    initramfs.set_defaults(handler=command_build_initramfs)

    distro_layer = subparsers.add_parser(
        "build-distro-layer",
        help="build a deterministic EROFS distro layer natively on Linux",
    )
    distro_layer.add_argument("--guest", choices=GUEST_NAMES, required=True)
    distro_layer.add_argument(
        "--output",
        type=Path,
        default=artifact_path(UbuntuBuildConstants.DISTRO_NAME),
    )
    distro_layer.add_argument("--replace", action="store_true")
    distro_layer.set_defaults(handler=command_build_distro_layer)

    scratch_template = subparsers.add_parser(
        "build-scratch-template",
        help=(
            "format the reproducible ext4 sandbox scratch template natively on Linux"
        ),
    )
    scratch_template.add_argument(
        "--output",
        type=Path,
        default=artifact_path(UbuntuBuildConstants.SCRATCH_TEMPLATE_NAME),
    )
    scratch_template.add_argument("--replace", action="store_true")
    scratch_template.set_defaults(handler=command_build_scratch_template)

    determinism = subparsers.add_parser(
        "verify-guest-determinism",
        help="build Ubuntu guest artifacts twice and compare them",
    )
    determinism.add_argument("--guest", choices=GUEST_NAMES, required=True)
    determinism.add_argument(
        "--work-dir",
        type=Path,
        default=(
            BuildConstants.BUILD_DIR
            / InitramfsBuildConstants.DETERMINISM_DIRECTORY_NAME
        ),
    )
    determinism.set_defaults(handler=command_verify_guest_determinism)

    openvmm = subparsers.add_parser("build-openvmm", help="build OpenVMM")
    _add_openvmm_options(openvmm)
    openvmm.set_defaults(handler=command_build_openvmm)

    provenance = subparsers.add_parser(
        "record-openvmm-provenance",
        help="bind an existing OpenVMM binary to the pinned source revision",
    )
    provenance.set_defaults(handler=command_record_openvmm_provenance)

    nvx_rs_provenance = subparsers.add_parser(
        "record-nvx-rs-provenance",
        help="bind an existing nvx-rs executable to the nvx-rs sources",
    )
    nvx_rs_provenance.add_argument(
        "--target",
        help=(
            "Rust target that built nvx-rs (default: the host target that "
            "rustc reports)"
        ),
    )
    nvx_rs_provenance.set_defaults(handler=command_record_nvx_rs_provenance)

    kernel_provenance = subparsers.add_parser(
        "materialize-kernel-provenance-inputs",
        help="write kernel provenance inputs from raw run-head blobs",
    )
    kernel_provenance.set_defaults(handler=command_materialize_kernel_provenance_inputs)

    cache = subparsers.add_parser(
        "setup-cross-os-cache",
        help="install GNU tar and zstd for GitHub Actions cross-OS caches",
    )
    cache.set_defaults(handler=command_setup_cross_os_cache)

    classify_changes = subparsers.add_parser(
        "classify-ci-changes",
        help="select the CI suites that a list of changed files runs",
    )
    classify_changes.add_argument(
        "--event-name",
        choices=CI_CLASSIFIED_EVENTS,
        required=True,
    )
    classify_changes.add_argument(
        "--changed-files",
        type=Path,
        required=True,
        help="file listing one changed repository path per line",
    )
    classify_changes.add_argument(
        "--null",
        action="store_true",
        help="read NUL-delimited paths, as emitted by git diff --name-only -z",
    )
    classify_changes.add_argument(
        "--summary",
        type=Path,
        help="Markdown file to append the reasons for each decision to",
    )
    classify_changes.set_defaults(handler=command_classify_ci_changes)

    patch_identity = subparsers.add_parser(
        "kernel-patch-identity",
        help="print the digest of the kernel patch paths and contents that keys the kernel cache",
    )
    patch_identity.add_argument(
        "--root",
        type=Path,
        default=BuildConstants.REPO_ROOT / KernelBuildConstants.PATCH_DIRECTORY,
    )
    patch_identity.set_defaults(handler=command_kernel_patch_identity)

    source_identity = subparsers.add_parser(
        "guest-binary-source-identity",
        help=(
            "print the digest of the Rust guest binary source paths and contents "
            "that keys the guest image caches"
        ),
    )
    source_identity.add_argument(
        "--root",
        type=Path,
        default=BuildConstants.REPO_ROOT,
    )
    source_identity.set_defaults(handler=command_guest_binary_source_identity)

    guest_record = subparsers.add_parser(
        "record-guest-artifacts",
        help=(
            "record the cache keys and the SHA-256 of each file of guest "
            "artifact components, which a merge group or a dev push checks "
            "before it reuses them"
        ),
    )
    guest_record.add_argument(
        "--components",
        required=True,
        help=(
            "space-separated guest artifact components to record: kernel, "
            "alpine, ubuntu, or azurelinux"
        ),
    )
    guest_record.add_argument(
        "--keys",
        required=True,
        help="JSON object of the exact guest cache keys by name",
    )
    guest_record.add_argument(
        "--directory",
        type=Path,
        default=BuildConstants.BUILD_DIR,
        help="directory of the guest artifacts (default: build)",
    )
    guest_record.set_defaults(handler=command_record_guest_artifacts)

    guest_reuse = subparsers.add_parser(
        "reuse-guest-artifacts",
        help=(
            "install guest artifact components that missed the cache from the "
            "latest same-repository pull request run that built the merged head"
        ),
    )
    guest_reuse.add_argument("--repository", required=True, help="OWNER/REPOSITORY")
    guest_reuse.add_argument(
        "--workflow-ref",
        required=True,
        help="the run's GITHUB_WORKFLOW_REF, which names its workflow",
    )
    guest_reuse.add_argument(
        "--record-artifact",
        required=True,
        help="name of the artifact that holds the record of the components",
    )
    guest_reuse.add_argument(
        "--components",
        required=True,
        help=(
            "space-separated guest artifact components to reuse: kernel, "
            "alpine, ubuntu, or azurelinux"
        ),
    )
    guest_reuse.add_argument(
        "--keys",
        required=True,
        help="JSON object of this run's exact guest cache keys by name",
    )
    guest_reuse_head = guest_reuse.add_mutually_exclusive_group(required=True)
    guest_reuse_head.add_argument(
        "--commit",
        help=(
            "the pushed or queued merge commit, whose second parent is the pull "
            "request head"
        ),
    )
    guest_reuse_head.add_argument("--head-sha", help="the pull request head itself")
    guest_reuse.add_argument(
        "--dry-run",
        action="store_true",
        help="check the artifacts as a push would, but install nothing",
    )
    guest_reuse.add_argument(
        "--destination",
        type=Path,
        default=BuildConstants.BUILD_DIR,
        help="directory of the guest artifacts (default: build)",
    )
    guest_reuse.set_defaults(handler=command_reuse_guest_artifacts)

    required_ci = subparsers.add_parser(
        "check-required-ci",
        help="validate required GitHub Actions job results",
    )
    required_ci.add_argument(
        "--event-name",
        choices=CI_EVENTS,
        required=True,
    )
    required_ci.add_argument(
        "--same-repository",
        choices=("false", "true"),
        required=True,
    )
    required_ci.add_argument(
        "--run-guest-artifacts", choices=("false", "true"), required=True
    )
    required_ci.add_argument(
        "--run-openvmm-binaries", choices=("false", "true"), required=True
    )
    required_ci.add_argument(
        "--run-openvmm-unit-tests", choices=("false", "true"), required=True
    )
    required_ci.add_argument(
        "--run-openvmm-vmm-tests", choices=("false", "true"), required=True
    )
    required_ci.add_argument(
        "--run-microvm-tests", choices=("false", "true"), required=True
    )
    required_ci.add_argument(
        "--run-benchmarks", choices=("false", "true"), required=True
    )
    required_ci.set_defaults(handler=command_check_required_ci)

    nightly_ci = subparsers.add_parser(
        "report-nightly-ci",
        help="open, update, or close the report of failed nightly CI runs",
    )
    configure_nightly_ci_parser(nightly_ci)

    superseded_groups = subparsers.add_parser(
        "cancel-superseded-merge-groups",
        help="cancel the CI runs of the merge groups that the merge queue replaced",
    )
    configure_merge_queue_parser(superseded_groups)

    static_elf = subparsers.add_parser(
        "check-static-elf",
        help=(
            "check that executables are static ELF files without an interpreter "
            "or an executable stack"
        ),
    )
    static_elf.add_argument("paths", nargs="+", type=Path, metavar="PATH")
    static_elf.set_defaults(handler=command_check_static_elf)

    openvmm_unit_tests = subparsers.add_parser(
        "test-openvmm-unit",
        help="run OpenVMM unit and documentation tests",
    )
    openvmm_unit_tests.set_defaults(handler=command_test_openvmm_unit)

    openvmm_test_build = subparsers.add_parser(
        "build-openvmm-tests",
        help="build OpenVMM Petri VMM tests without running them",
    )
    openvmm_test_build.add_argument(
        "--backend",
        choices=OPENVMM_TEST_BACKENDS,
        action="append",
        required=True,
        help="build this backend's tests; repeat for each backend of this host's OS",
    )
    openvmm_test_build.add_argument(
        "--output",
        type=Path,
        required=True,
        help="empty or new directory to build the test content into",
    )
    openvmm_test_build.set_defaults(handler=command_build_openvmm_tests)

    openvmm_test_cache_key = subparsers.add_parser(
        "openvmm-test-cache-key",
        help=(
            "print the key under which CI caches the OpenVMM VMM tests that "
            "build-openvmm-tests builds"
        ),
    )
    openvmm_test_cache_key.add_argument(
        "--backend",
        choices=OPENVMM_TEST_BACKENDS,
        action="append",
        required=True,
        help="key this backend's tests; repeat for each backend that the build builds",
    )
    openvmm_test_cache_key.set_defaults(handler=command_openvmm_test_cache_key)

    openvmm_test_check = subparsers.add_parser(
        "check-openvmm-tests",
        help=(
            "check that a directory holds the OpenVMM VMM tests that "
            "build-openvmm-tests builds from this checkout"
        ),
    )
    openvmm_test_check.add_argument(
        "--backend",
        choices=OPENVMM_TEST_BACKENDS,
        action="append",
        required=True,
        help="check for this backend's tests; repeat for each backend of the build",
    )
    openvmm_test_check.add_argument(
        "--content",
        type=Path,
        required=True,
        help="directory of the test content to check",
    )
    openvmm_test_check.set_defaults(handler=command_check_openvmm_tests)

    openvmm_test_reuse = subparsers.add_parser(
        "reuse-openvmm-tests",
        help=(
            "keep OpenVMM VMM tests that a cache restored if build-openvmm-tests "
            "builds them from this checkout, and remove them otherwise"
        ),
    )
    openvmm_test_reuse.add_argument(
        "--backend",
        choices=OPENVMM_TEST_BACKENDS,
        action="append",
        required=True,
        help="reuse this backend's tests; repeat for each backend of the build",
    )
    openvmm_test_reuse.add_argument(
        "--content",
        type=Path,
        required=True,
        help="directory into which the cache restored the test content",
    )
    openvmm_test_reuse.set_defaults(handler=command_reuse_openvmm_tests)

    openvmm_tests = subparsers.add_parser(
        "test-openvmm",
        help="run OpenVMM Petri VMM tests",
    )
    openvmm_tests.add_argument(
        "--backend",
        choices=OPENVMM_TEST_BACKENDS,
        required=True,
    )
    openvmm_tests.add_argument(
        "--content",
        type=Path,
        help=(
            "run the tests from the directory that build-openvmm-tests built, "
            "instead of building them"
        ),
    )
    openvmm_tests.add_argument(
        "--shard",
        type=parse_shard,
        metavar="INDEX/COUNT",
        help=(
            "run shard INDEX of the COUNT shards of the backend's tests, as "
            "scripts/nvx_tools/openvmm_test_shards.json splits them, such as "
            "1/2; COUNT must be their number of shards"
        ),
    )
    openvmm_tests.set_defaults(handler=command_test_openvmm)

    openvmm_test_shards = subparsers.add_parser(
        "plan-openvmm-test-shards",
        help=(
            "split the OpenVMM VMM tests into shards that nextest runs in "
            "balanced times for CI"
        ),
    )
    configure_openvmm_test_shard_parser(openvmm_test_shards)

    microvm_tests = subparsers.add_parser(
        "test-microvm",
        help="run NVX-owned OpenVMM microVM correctness tests",
    )
    configure_microvm_test_parser(microvm_tests)

    microvm_shards = subparsers.add_parser(
        "plan-microvm-shards",
        help=(
            "split the default microVM suite into shards of balanced scenario "
            "durations for CI"
        ),
    )
    configure_microvm_shard_parser(microvm_shards)

    doctor = subparsers.add_parser(
        "doctor",
        help="qualify this host for the NVX time ABI",
    )
    configure_doctor_parser(doctor)

    runtime_adapter_tests = subparsers.add_parser(
        "test-runtime-adapter",
        help="run the runtime adapter's consolidated suite by name on a real hypervisor",
    )
    configure_runtime_adapter_test_parser(runtime_adapter_tests)

    adversarial_tests = subparsers.add_parser(
        "test-adversarial",
        help="run a brokered Copilot-driven adversarial campaign",
    )
    configure_adversarial_parser(adversarial_tests)

    build = subparsers.add_parser("build", help="build guest artifacts and OpenVMM")
    _add_guest_options(build, allow_all=True)
    _add_openvmm_options(build)
    build.set_defaults(handler=command_build)

    benchmark = subparsers.add_parser(
        "benchmark",
        help="run the OpenVMM-native benchmark coordinator",
    )
    configure_benchmark_parser(benchmark, BuildConstants.REPO_ROOT)
    benchmark.set_defaults(handler=command_benchmark)

    performance = subparsers.add_parser(
        "performance",
        help="collect, persist, and gate CI performance results",
    )
    configure_performance_parser(performance)

    sources = subparsers.add_parser(
        "collect-sources",
        help="materialize verified Linux, Alpine, and Ubuntu release sources",
    )
    sources.set_defaults(handler=command_collect_sources)

    alpine_sources = subparsers.add_parser(
        "collect-alpine-sources",
        help="collect exact Alpine recipes and upstream sources",
    )
    configure_alpine_sources_parser(alpine_sources)

    ubuntu_sources = subparsers.add_parser(
        "collect-ubuntu-sources",
        help="collect exact Ubuntu source packages",
    )
    configure_ubuntu_sources_parser(ubuntu_sources)

    linux_source_archive = subparsers.add_parser(
        "create-linux-source-archive",
        help="create the Linux corresponding-source archive from pinned inputs",
    )
    configure_linux_source_archive_parser(linux_source_archive)

    package = subparsers.add_parser("package", help="stage a binary distribution")
    package.add_argument("--version")
    package.add_argument("--destination", type=Path)
    source_mode = package.add_mutually_exclusive_group(required=True)
    source_mode.add_argument("--include-source", action="store_true")
    source_mode.add_argument(
        "--binary-only",
        action="store_true",
        help="stage binaries only; corresponding source must be published separately",
    )
    package.add_argument("--force", action="store_true")
    package.set_defaults(handler=command_package)

    archive_release = subparsers.add_parser(
        "archive-release",
        help="create a deterministic archive from a staged distribution",
    )
    archive_release.add_argument("--source", type=Path, required=True)
    archive_release.add_argument("--destination", type=Path, required=True)
    archive_release.set_defaults(handler=command_archive_release)

    verify = subparsers.add_parser("verify", help="verify source and submodule inputs")
    verify.set_defaults(handler=command_verify)
    return parser.parse_args(argv)


@contextmanager
def _sigterm_interrupts() -> Generator[None]:
    """Let SIGTERM interrupt a command as SIGINT does, so that its cleanup runs.

    A cancelled CI step gets SIGINT and, if it still runs 7.5 s later, SIGTERM
    (see doc/ci.md#cancellation). On SIGTERM, Python exits without unwinding,
    which would leave the command's OpenVMM processes and scratch directories
    behind. As Python does for SIGINT, an ignored SIGTERM stays ignored.
    """
    if (
        os.name != "posix"
        or threading.current_thread() is not threading.main_thread()
        or signal.getsignal(signal.SIGTERM) != signal.SIG_DFL
    ):
        yield
        return
    signal.signal(signal.SIGTERM, signal.default_int_handler)
    try:
        yield
    finally:
        signal.signal(signal.SIGTERM, signal.SIG_DFL)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        with _sigterm_interrupts():
            result = args.handler(args)
    except KeyboardInterrupt:
        print("Interrupted", file=sys.stderr)
        return 130
    except (
        ScriptError,
        OSError,
        RuntimeError,
        ValueError,
        subprocess.CalledProcessError,
    ) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return result if result is not None else 0


if __name__ == "__main__":
    raise SystemExit(main())
