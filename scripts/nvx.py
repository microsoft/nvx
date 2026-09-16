#!/usr/bin/env python3
"""Build, run, benchmark, and package the OpenVMM/NVX distribution."""

from __future__ import annotations

import argparse
import os
import shlex
import stat
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path

from nvx_tools.benchmark import configure_parser as configure_benchmark_parser
from nvx_tools.build import (
    AGENT_INITRAMFS_NAME,
    SIMPLE_TRANSPORT,
    AlpineBuildConfig,
    DockerBuildConfig,
    KernelBuildConfig,
    build_docker_agent_initramfs,
    build_docker_artifacts,
    build_initramfs,
    build_kernel,
    native_initramfs_work_directory,
    record_openvmm_provenance,
    stage_guest_agent,
    verified_staged_guest_agent,
    verify_agent_initramfs,
)
from nvx_tools.ci import (
    OPENVMM_TEST_BACKENDS,
    run_openvmm_tests,
    setup_cross_os_cache,
)
from nvx_tools.collect_alpine_sources import (
    configure_parser as configure_alpine_sources_parser,
)
from nvx_tools.common import (
    BUILD_DIR,
    OPENVMM_DIR,
    REPO_ROOT,
    ScriptError,
    artifact_path,
    openvmm_binary_path,
    require_file,
)
from nvx_tools.create_linux_source_archive import (
    configure_parser as configure_linux_source_archive_parser,
)
from nvx_tools.microvm_tests import configure_parser as configure_microvm_test_parser
from nvx_tools.performance import configure_parser as configure_performance_parser
from nvx_tools.release import (
    RELEASE_TRANSPORTS,
    collect_release_sources,
    create_release_archive,
    download_latest_release,
    package_release,
    verify_broker_live_gate,
    verify_source_tree,
)
from nvx_tools.sandbox import SandboxLaunch, SandboxLayer

if sys.platform == "linux":
    import fcntl

DEFAULT_RELEASE_REPOSITORY = "nanvix/nvx"
HYPERVISORS = ("auto", "whp", "kvm", "mshv")
NETWORK_PROFILES = ("portable",)


def _run(args: list[str | os.PathLike[str]], *, cwd: Path = REPO_ROOT) -> None:
    command = [os.fspath(arg) for arg in args]
    print(f">> {shlex.join(command)}")
    subprocess.run(command, cwd=cwd, check=True)


def command_init(_: argparse.Namespace) -> None:
    _run(["git", "submodule", "update", "--init", "--recursive"])


def _native_kernel() -> None:
    build_kernel(
        KernelBuildConfig(
            work=BUILD_DIR / "linux",
            output=artifact_path("vmlinux"),
        )
    )


def _native_initramfs() -> None:
    build_initramfs(
        AlpineBuildConfig(
            work=native_initramfs_work_directory(SIMPLE_TRANSPORT),
            output=artifact_path("initramfs.cpio.gz"),
        )
    )


def _native_agent_initramfs() -> None:
    build_initramfs(
        AlpineBuildConfig(
            work=native_initramfs_work_directory("broker-ttrpc"),
            output=artifact_path(AGENT_INITRAMFS_NAME),
            agent_enabled=True,
        )
    )


def command_build_guest(args: argparse.Namespace) -> None:
    if args.native:
        _native_kernel()
        _native_initramfs()
        if args.with_agent:
            _native_agent_initramfs()
        return

    config = DockerBuildConfig(destination=BUILD_DIR)
    build_docker_artifacts(config)
    if args.with_agent:
        build_docker_agent_initramfs(config)


def command_build_kernel(_: argparse.Namespace) -> None:
    _native_kernel()


def command_build_initramfs(_: argparse.Namespace) -> None:
    _native_initramfs()


def command_build_agent_initramfs(args: argparse.Namespace) -> None:
    if args.native:
        _native_agent_initramfs()
    else:
        build_docker_agent_initramfs(DockerBuildConfig(destination=BUILD_DIR))


def command_stage_agent(args: argparse.Namespace) -> None:
    stage_guest_agent(args.input, args.sha256)


def command_verify_agent_initramfs(args: argparse.Namespace) -> None:
    _agent, staged_sha256 = verified_staged_guest_agent()
    expected_sha256 = args.sha256.lower()
    if staged_sha256 != expected_sha256:
        raise ScriptError(
            f"staged NVX guest-agent SHA-256 is {staged_sha256}, "
            f"expected {expected_sha256}"
        )
    verify_agent_initramfs(args.input, expected_sha256)


def command_build_openvmm(args: argparse.Namespace) -> None:
    require_file(OPENVMM_DIR / "Cargo.toml", "initialized OpenVMM submodule")
    if not args.skip_restore:
        _run(
            ["cargo", "xflowey", "restore-packages", "--no-compat-igvm"],
            cwd=OPENVMM_DIR,
        )
    _run(
        ["cargo", "build", "--release", "-p", "openvmm", "--bin", "openvmm"],
        cwd=OPENVMM_DIR,
    )
    record_openvmm_provenance(openvmm_binary_path())


def command_record_openvmm_provenance(_: argparse.Namespace) -> None:
    record_openvmm_provenance(openvmm_binary_path())


def command_setup_cross_os_cache(_: argparse.Namespace) -> None:
    setup_cross_os_cache()


def command_test_openvmm(args: argparse.Namespace) -> None:
    run_openvmm_tests(args.backend)


def command_build(args: argparse.Namespace) -> None:
    command_build_guest(args)
    command_build_openvmm(args)


def _hypervisor(selected: str) -> str:
    if selected != "auto":
        return selected
    return "whp" if os.name == "nt" else "kvm"


def _release_platform(hypervisor: str) -> str:
    selected = _hypervisor(hypervisor)
    if sys.platform == "win32":
        host = "windows"
        supported = ("whp",)
    elif sys.platform.startswith("linux"):
        host = "linux"
        supported = ("kvm", "mshv")
    else:
        raise ScriptError(f"release downloads are unsupported on {sys.platform}")
    if selected not in supported:
        raise ScriptError(f"{selected} is not supported on {host}")
    return f"{host}-{selected}"


def command_download(args: argparse.Namespace) -> None:
    download_latest_release(
        args.repository,
        _release_platform(args.hypervisor),
        args.transport,
        args.manifest_sha256,
    )


def _format_command(command: list[str]) -> str:
    return subprocess.list2cmdline(command) if os.name == "nt" else shlex.join(command)


def command_run(args: argparse.Namespace) -> None:
    if (args.net is None) != (args.network_profile is None):
        raise ScriptError("--net and --network-profile must be specified together")
    if args.restore_ready_path is not None and args.restore_snapshot is None:
        raise ScriptError("--restore-ready-path requires --restore-snapshot")
    if args.restore_processors is not None and args.restore_snapshot is None:
        raise ScriptError("--restore-processors requires --restore-snapshot")
    if args.restore_memory_mib is not None and args.restore_snapshot is None:
        raise ScriptError("--restore-memory-mib requires --restore-snapshot")
    if args.memory_capacity_mib is not None and args.restore_snapshot is not None:
        raise ScriptError("--memory-capacity-mib is only valid for a fresh boot")
    if (
        args.memory_capacity_mib is not None
        and args.memory_capacity_mib < args.memory_mib
    ):
        raise ScriptError("--memory-capacity-mib cannot be below --memory-mib")
    if args.restore_processors is not None:
        if args.restore_processors > args.processors:
            raise ScriptError(
                "--restore-processors cannot exceed --processors capacity"
            )
    executable = require_file(openvmm_binary_path(), "OpenVMM release binary")
    command = [
        str(executable),
        "--single-process",
        "--machine",
        args.machine,
        "--processors",
        str(args.processors),
        "--hypervisor",
        _hypervisor(args.hypervisor),
    ]
    if args.restore_snapshot is not None:
        command.extend(
            ["--restore-snapshot", str(args.restore_snapshot), "--restore-entropy"]
        )
        if args.restore_processors is not None:
            command.extend(["--restore-processors", str(args.restore_processors)])
        if args.restore_memory_mib is not None:
            command.extend(["--restore-memory", f"{args.restore_memory_mib}M"])
        if args.restore_ready_path is not None:
            command.extend(["--restore-ready-path", str(args.restore_ready_path)])
    else:
        kernel = require_file(artifact_path("vmlinux"), "PVH kernel")
        initrd = require_file(
            artifact_path("initramfs.cpio.gz"),
            "initramfs",
        )
        command.extend(
            [
                "--memory",
                f"{args.memory_mib}M",
                "--kernel",
                str(kernel),
                "--initrd",
                str(initrd),
            ]
        )
        if args.memory_capacity_mib is not None:
            command.extend(["--memory-capacity", f"{args.memory_capacity_mib}M"])
    if args.mount is not None:
        if args.mount.count(",") not in (1, 2):
            raise ScriptError("--mount must be GUEST_TARGET,HOST_PATH[,ro|rw]")
        command.extend(["--mount", args.mount])
    if args.net is not None:
        command.extend(["--net", args.net, "--network-profile", args.network_profile])
    if args.cmdline:
        command.extend(["--cmdline", args.cmdline])
    print(f">> {_format_command(command)}")
    if not args.dry_run:
        raise SystemExit(subprocess.run(command).returncode)


def command_sandbox(args: argparse.Namespace) -> None:
    if args.transport != "broker-ttrpc":
        raise ScriptError("--transport broker-ttrpc must be selected explicitly")
    if (args.net is None) != (args.network_profile is None):
        raise ScriptError("--net and --network-profile must be specified together")
    launch = SandboxLaunch(
        layers=tuple(args.layer),
        scratch=args.scratch,
    ).validated()
    executable = require_file(openvmm_binary_path(), "OpenVMM release binary")
    kernel = require_file(artifact_path("vmlinux"), "PVH kernel")
    initrd = require_file(
        artifact_path(AGENT_INITRAMFS_NAME),
        "broker-ttrpc agent initramfs",
    )
    command = [
        str(executable),
        *launch.openvmm_arguments(
            args.control_socket,
            args.boot_console_socket,
            args.control_auth_handle,
        ),
        "--single-process",
        "--hypervisor",
        _hypervisor(args.hypervisor),
        "--memory",
        f"{args.memory_mib}M",
        "--kernel",
        str(kernel),
        "--initrd",
        str(initrd),
        "--cmdline",
        launch.kernel_command_line(args.cmdline),
    ]
    if args.net is not None:
        command.extend(["--net", args.net, "--network-profile", args.network_profile])
    print(f">> {_format_command(command)}")
    if not args.dry_run:
        if sys.platform != "linux":
            raise ScriptError("broker-ttrpc sandbox launch requires Linux")
        try:
            auth_handle = os.fstat(args.control_auth_handle)
            auth_flags = fcntl.fcntl(args.control_auth_handle, fcntl.F_GETFL)
        except OSError as error:
            raise ScriptError(
                "control authentication handle is not open in this process"
            ) from error
        if (
            not stat.S_ISFIFO(auth_handle.st_mode)
            or auth_flags & os.O_ACCMODE == os.O_WRONLY
        ):
            raise ScriptError("control authentication handle must be a readable pipe")
        raise SystemExit(
            subprocess.run(
                command,
                stdin=args.control_auth_handle,
            ).returncode
        )


def command_collect_sources(args: argparse.Namespace) -> None:
    collect_release_sources(args.transport)


def command_package(args: argparse.Namespace) -> None:
    package_release(
        version=args.version,
        destination=args.destination,
        include_source=args.include_source,
        force=args.force,
        transport=args.transport,
        manifest_digest_output=args.manifest_digest_output,
    )


def command_archive_release(args: argparse.Namespace) -> None:
    create_release_archive(args.bundle, args.output)


def command_verify_broker_live_gate(args: argparse.Namespace) -> None:
    verify_broker_live_gate(
        args.archive,
        args.manifest_digest,
        args.proof,
        args.proof_sha256,
        args.platform,
        args.archive_sha256,
    )


def command_verify(_: argparse.Namespace) -> None:
    verify_source_tree()


def _add_guest_options(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--native",
        action="store_true",
        help="build directly on Linux instead of using Docker",
    )
    parser.add_argument(
        "--with-agent",
        action="store_true",
        help="also build the staged broker-ttrpc agent initramfs",
    )


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    init = subparsers.add_parser("init", help="initialize the private submodule")
    init.set_defaults(handler=command_init)

    guest = subparsers.add_parser("build-guest", help="build Linux guest artifacts")
    _add_guest_options(guest)
    guest.set_defaults(handler=command_build_guest)

    kernel = subparsers.add_parser(
        "build-kernel",
        help="fetch, patch, and build the pinned kernel natively on Linux",
    )
    kernel.set_defaults(handler=command_build_kernel)

    initramfs = subparsers.add_parser(
        "build-initramfs",
        help="build an Alpine initramfs natively on Linux",
    )
    initramfs.set_defaults(handler=command_build_initramfs)

    agent_initramfs = subparsers.add_parser(
        "build-agent-initramfs",
        help="build the explicit broker-ttrpc agent initramfs",
    )
    agent_initramfs.add_argument(
        "--native",
        action="store_true",
        help="build directly on Linux instead of using Docker",
    )
    agent_initramfs.set_defaults(handler=command_build_agent_initramfs)

    agent = subparsers.add_parser(
        "stage-agent",
        help="stage a pinned static NVX guest-agent build input",
    )
    agent.add_argument("--input", type=Path, required=True)
    agent.add_argument("--sha256", required=True)
    agent.set_defaults(handler=command_stage_agent)

    verify_agent = subparsers.add_parser(
        "verify-agent-initramfs",
        help="verify a broker initramfs against the staged agent and trusted SHA-256",
    )
    verify_agent.add_argument("--input", type=Path, required=True)
    verify_agent.add_argument("--sha256", required=True)
    verify_agent.set_defaults(handler=command_verify_agent_initramfs)

    openvmm = subparsers.add_parser("build-openvmm", help="build OpenVMM")
    openvmm.add_argument("--skip-restore", action="store_true")
    openvmm.set_defaults(handler=command_build_openvmm)

    provenance = subparsers.add_parser(
        "record-openvmm-provenance",
        help="bind an existing OpenVMM binary to the pinned source revision",
    )
    provenance.set_defaults(handler=command_record_openvmm_provenance)

    cache = subparsers.add_parser(
        "setup-cross-os-cache",
        help="install GNU tar and zstd for GitHub Actions cross-OS caches",
    )
    cache.set_defaults(handler=command_setup_cross_os_cache)

    openvmm_tests = subparsers.add_parser(
        "test-openvmm",
        help="run OpenVMM microVM integration tests",
    )
    openvmm_tests.add_argument(
        "--backend",
        choices=OPENVMM_TEST_BACKENDS,
        required=True,
    )
    openvmm_tests.set_defaults(handler=command_test_openvmm)

    microvm_tests = subparsers.add_parser(
        "test-microvm",
        help="run NVX-owned OpenVMM microVM correctness tests",
    )
    configure_microvm_test_parser(microvm_tests)

    build = subparsers.add_parser("build", help="build guest artifacts and OpenVMM")
    _add_guest_options(build)
    build.add_argument("--skip-restore", action="store_true")
    build.set_defaults(handler=command_build)

    download = subparsers.add_parser(
        "download",
        help="download and install the latest matching GitHub release",
    )
    download.add_argument(
        "--repository",
        default=DEFAULT_RELEASE_REPOSITORY,
        metavar="OWNER/REPOSITORY",
    )
    download.add_argument("--hypervisor", choices=HYPERVISORS, default="auto")
    download.add_argument(
        "--transport",
        choices=RELEASE_TRANSPORTS,
        required=True,
    )
    download.add_argument(
        "--manifest-sha256",
        help="independently delivered SOURCE-MANIFEST.json digest (broker only)",
    )
    download.set_defaults(handler=command_download)

    run = subparsers.add_parser("run", help="run an OpenVMM microVM")
    run.add_argument("--hypervisor", choices=HYPERVISORS, default="auto")
    run.add_argument(
        "--machine",
        choices=("microvm",),
        default="microvm",
    )
    run.add_argument("--memory-mib", type=int, default=128)
    run.add_argument("--memory-capacity-mib", type=int)
    run.add_argument("--processors", type=int, choices=(1, 2, 4, 8), default=1)
    run.add_argument("--mount", help="GUEST_TARGET,HOST_PATH,ro|rw")
    run.add_argument("--net", metavar="IPV4/PREFIX")
    run.add_argument("--network-profile", choices=NETWORK_PROFILES)
    run.add_argument("--cmdline", default="")
    run.add_argument("--restore-snapshot", type=Path)
    run.add_argument("--restore-processors", type=int, choices=(1, 2, 4, 8))
    run.add_argument("--restore-memory-mib", type=int)
    run.add_argument("--restore-ready-path", type=Path)
    run.add_argument("--dry-run", action="store_true")
    run.set_defaults(handler=command_run)

    sandbox = subparsers.add_parser(
        "sandbox",
        help="run one workload over EROFS layers and private ext4 scratch",
    )

    def sandbox_layer(value: str) -> SandboxLayer:
        try:
            return SandboxLayer.parse(value)
        except ScriptError as error:
            raise argparse.ArgumentTypeError(str(error)) from error

    sandbox.add_argument(
        "--layer",
        action="append",
        required=True,
        type=sandbox_layer,
        metavar="ROLE,PATH",
    )
    sandbox.add_argument("--scratch", required=True, type=Path)
    sandbox.add_argument("--memory-mib", type=int, default=256)
    sandbox.add_argument("--hypervisor", choices=HYPERVISORS, default="auto")
    sandbox.add_argument("--net", metavar="IPV4/PREFIX")
    sandbox.add_argument("--network-profile", choices=NETWORK_PROFILES)
    sandbox.add_argument("--cmdline", default="")
    sandbox.add_argument(
        "--transport",
        choices=("broker-ttrpc",),
        required=True,
    )
    sandbox.add_argument("--control-socket", required=True, type=Path)
    sandbox.add_argument("--boot-console-socket", required=True, type=Path)
    sandbox.add_argument("--control-auth-handle", required=True, type=int)
    sandbox.add_argument("--dry-run", action="store_true")
    sandbox.set_defaults(handler=command_sandbox)

    benchmark = subparsers.add_parser(
        "benchmark",
        help="run the OpenVMM-native benchmark coordinator",
    )
    configure_benchmark_parser(benchmark, REPO_ROOT)

    performance = subparsers.add_parser(
        "performance",
        help="collect, persist, and gate CI performance results",
    )
    configure_performance_parser(performance)

    sources = subparsers.add_parser(
        "collect-sources",
        help="materialize verified Linux and Alpine release-source artifacts",
    )
    sources.add_argument(
        "--transport",
        choices=RELEASE_TRANSPORTS,
        required=True,
    )
    sources.set_defaults(handler=command_collect_sources)

    alpine_sources = subparsers.add_parser(
        "collect-alpine-sources",
        help="collect exact Alpine recipes and upstream sources",
    )
    configure_alpine_sources_parser(alpine_sources)

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
    package.add_argument(
        "--transport",
        choices=RELEASE_TRANSPORTS,
        required=True,
    )
    package.add_argument(
        "--manifest-digest-output",
        type=Path,
        help="external broker manifest-digest artifact path",
    )
    package.set_defaults(handler=command_package)

    archive_release = subparsers.add_parser(
        "archive-release",
        help="create a release archive with canonical safe modes",
    )
    archive_release.add_argument("--bundle", type=Path, required=True)
    archive_release.add_argument("--output", type=Path, required=True)
    archive_release.set_defaults(handler=command_archive_release)

    live_gate = subparsers.add_parser(
        "verify-broker-live-gate",
        help="verify an externally authenticated live broker smoke proof",
    )
    live_gate.add_argument("--archive", type=Path, required=True)
    live_gate.add_argument("--manifest-digest", type=Path, required=True)
    live_gate.add_argument("--proof", type=Path, required=True)
    live_gate.add_argument("--proof-sha256", required=True)
    live_gate.add_argument("--archive-sha256", required=True)
    live_gate.add_argument("--platform", required=True)
    live_gate.set_defaults(handler=command_verify_broker_live_gate)

    verify = subparsers.add_parser("verify", help="verify source and submodule inputs")
    verify.set_defaults(handler=command_verify)
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
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
