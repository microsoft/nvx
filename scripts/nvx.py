#!/usr/bin/env python3
"""Build, run, benchmark, and package the OpenVMM/NVX distribution."""

from __future__ import annotations

import argparse
import os
import shlex
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path

from nvx_tools import sandbox_lifecycle
from nvx_tools.benchmark import configure_parser as configure_benchmark_parser
from nvx_tools.build import (
    AlpineBuildConfig,
    DockerBuildConfig,
    KernelBuildConfig,
    build_docker_artifacts,
    build_initramfs,
    build_kernel,
    materialize_kernel_provenance_inputs,
    record_openvmm_provenance,
)
from nvx_tools.ci import (
    OPENVMM_TEST_BACKENDS,
    run_openvmm_tests,
    run_openvmm_unit_tests,
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
    collect_release_sources,
    create_release_archive,
    download_latest_release,
    package_release,
    verify_source_tree,
)
from nvx_tools.sandbox import SandboxLaunch, SandboxLayer, parse_workload_identity

DEFAULT_RELEASE_REPOSITORY = "microsoft/nvx"
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
            work=BUILD_DIR / "initramfs-work",
            output=artifact_path("initramfs.cpio.gz"),
        )
    )


def command_build_guest(args: argparse.Namespace) -> None:
    if args.native:
        _native_kernel()
        _native_initramfs()
        return

    config = DockerBuildConfig(destination=BUILD_DIR)
    build_docker_artifacts(config)


def command_build_kernel(_: argparse.Namespace) -> None:
    _native_kernel()


def command_build_initramfs(_: argparse.Namespace) -> None:
    _native_initramfs()


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


def command_materialize_kernel_provenance_inputs(_: argparse.Namespace) -> None:
    materialize_kernel_provenance_inputs()


def command_setup_cross_os_cache(_: argparse.Namespace) -> None:
    setup_cross_os_cache()


def command_test_openvmm(args: argparse.Namespace) -> None:
    run_openvmm_tests(args.backend)


def command_test_openvmm_unit(_: argparse.Namespace) -> None:
    run_openvmm_unit_tests()


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
    download_latest_release(args.repository, _release_platform(args.hypervisor))


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
    for denied_path in args.mount_deny:
        command.extend(["--mount-deny", str(denied_path)])
    if args.net is not None:
        command.extend(["--net", args.net, "--network-profile", args.network_profile])
    if args.network_egress is not None:
        command.extend(["--network-egress", args.network_egress])
    if args.network_ingress is not None:
        command.extend(["--network-ingress", args.network_ingress])
    for rule in args.network_egress_allow:
        command.extend(["--network-egress-allow", rule])
    for rule in args.network_egress_deny:
        command.extend(["--network-egress-deny", rule])
    if args.host_loopback is not None:
        command.extend(["--host-loopback", args.host_loopback])
    if args.network_proxy is not None:
        command.extend(["--network-proxy", args.network_proxy])
    for forward in args.host_loopback_forward:
        command.extend(["--host-loopback-forward", forward])
    if args.outcome_report is not None:
        command.extend(["--microvm-report", str(args.outcome_report)])
    if args.cmdline:
        command.extend(["--cmdline", args.cmdline])
    print(f">> {_format_command(command)}")
    if not args.dry_run:
        raise SystemExit(subprocess.run(command).returncode)


def command_sandbox(args: argparse.Namespace) -> None:
    operation = args.sandbox_operation
    if args.outcome_report is not None and operation not in ("run", "exec"):
        raise ScriptError(
            "--outcome-report is only valid for one-shot run or managed exec"
        )
    if operation in ("run", "provision"):
        if (args.net is None) != (args.network_profile is None):
            raise ScriptError("--net and --network-profile must be specified together")
        if not args.layer or args.scratch is None:
            raise ScriptError(f"sandbox {operation} requires --layer and --scratch")
        launch = SandboxLaunch(
            layers=tuple(args.layer),
            scratch=args.scratch,
            entrypoint=args.entrypoint,
            args=tuple(args.sandbox_arg),
            hostname=args.hostname,
            workload_identity=args.workload_user,
            memory_max=args.memory_max,
            pids_max=args.pids_max,
        ).validated()
    else:
        launch = None

    if operation == "provision":
        if args.state_dir is None:
            raise ScriptError("sandbox provision requires --state-dir")
        assert launch is not None
        sandbox_lifecycle.provision(
            args.state_dir,
            launch,
            hypervisor=_hypervisor(args.hypervisor),
            memory_mib=args.memory_mib,
            net=args.net,
            network_profile=args.network_profile,
            network_egress=args.network_egress,
            network_ingress=args.network_ingress,
            network_egress_allow=tuple(args.network_egress_allow),
            network_egress_deny=tuple(args.network_egress_deny),
            host_loopback=args.host_loopback,
            network_proxy=args.network_proxy,
            host_loopback_forward=tuple(args.host_loopback_forward),
            cmdline=args.cmdline,
        )
        return
    if operation == "start":
        if args.state_dir is None:
            raise ScriptError("sandbox start requires --state-dir")
        sandbox_lifecycle.start(args.state_dir, args.timeout)
        return
    if operation == "exec":
        if args.state_dir is None:
            raise ScriptError("sandbox exec requires --state-dir")
        if args.outcome_report is not None:
            sandbox_lifecycle.validate_outcome_destination(args.outcome_report)
        result = sandbox_lifecycle.exec_workload(
            args.state_dir,
            (args.entrypoint, *args.sandbox_arg),
            timeout_ms=args.exec_timeout_ms,
            response_timeout=args.timeout,
        )
        sys.stdout.buffer.write(result.stdout)
        sys.stdout.buffer.flush()
        sys.stderr.buffer.write(result.stderr)
        sys.stderr.buffer.flush()
        if args.outcome_report is not None:
            sandbox_lifecycle.write_exec_outcome(args.outcome_report, result)
        raise SystemExit(result.returncode)
    if operation == "stop":
        if args.state_dir is None:
            raise ScriptError("sandbox stop requires --state-dir")
        sandbox_lifecycle.stop(args.state_dir, args.timeout)
        return
    if operation == "deprovision":
        if args.state_dir is None:
            raise ScriptError("sandbox deprovision requires --state-dir")
        sandbox_lifecycle.deprovision(args.state_dir)
        return
    if args.state_dir is not None:
        raise ScriptError(
            "one-shot sandbox execution rejects persistent --state-dir settings"
        )
    assert operation == "run"
    assert launch is not None
    executable = require_file(openvmm_binary_path(), "OpenVMM release binary")
    kernel = require_file(artifact_path("vmlinux"), "PVH kernel")
    initrd = require_file(
        artifact_path("initramfs.cpio.gz"),
        "initramfs",
    )
    command = [
        str(executable),
        *launch.openvmm_arguments(),
        "--microvm-lifecycle",
        "one-shot",
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
    if args.network_egress is not None:
        command.extend(["--network-egress", args.network_egress])
    if args.network_ingress is not None:
        command.extend(["--network-ingress", args.network_ingress])
    for rule in args.network_egress_allow:
        command.extend(["--network-egress-allow", rule])
    for rule in args.network_egress_deny:
        command.extend(["--network-egress-deny", rule])
    if args.host_loopback is not None:
        command.extend(["--host-loopback", args.host_loopback])
    if args.network_proxy is not None:
        command.extend(["--network-proxy", args.network_proxy])
    for forward in args.host_loopback_forward:
        command.extend(["--host-loopback-forward", forward])
    if args.outcome_report is not None:
        command.extend(["--microvm-report", str(args.outcome_report)])
    print(f">> {_format_command(command)}")
    if not args.dry_run:
        raise SystemExit(subprocess.run(command).returncode)


def command_collect_sources(_: argparse.Namespace) -> None:
    collect_release_sources()


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


def _add_guest_options(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--native",
        action="store_true",
        help="build directly on Linux instead of using Docker",
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

    openvmm = subparsers.add_parser("build-openvmm", help="build OpenVMM")
    openvmm.add_argument("--skip-restore", action="store_true")
    openvmm.set_defaults(handler=command_build_openvmm)

    provenance = subparsers.add_parser(
        "record-openvmm-provenance",
        help="bind an existing OpenVMM binary to the pinned source revision",
    )
    provenance.set_defaults(handler=command_record_openvmm_provenance)

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

    openvmm_unit_tests = subparsers.add_parser(
        "test-openvmm-unit",
        help="run OpenVMM unit and documentation tests",
    )
    openvmm_unit_tests.set_defaults(handler=command_test_openvmm_unit)

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
    run.add_argument("--mount-deny", action="append", type=Path, default=[])
    run.add_argument("--net", metavar="IPV4/PREFIX")
    run.add_argument("--network-profile", choices=NETWORK_PROFILES)
    run.add_argument("--network-egress", choices=("allow", "deny"))
    run.add_argument("--network-ingress", choices=("allow", "deny"))
    run.add_argument("--network-egress-allow", action="append", default=[])
    run.add_argument("--network-egress-deny", action="append", default=[])
    run.add_argument("--host-loopback", choices=("allow", "deny"))
    run.add_argument("--network-proxy", metavar="IPV4:TCP-PORT")
    run.add_argument("--host-loopback-forward", action="append", default=[])
    run.add_argument(
        "--outcome-report",
        type=Path,
        help="write a bounded local JSON outcome report",
    )
    run.add_argument("--cmdline", default="")
    run.add_argument("--restore-snapshot", type=Path)
    run.add_argument("--restore-processors", type=int, choices=(1, 2, 4, 8))
    run.add_argument("--restore-memory-mib", type=int)
    run.add_argument("--restore-ready-path", type=Path)
    run.add_argument("--dry-run", action="store_true")
    run.set_defaults(handler=command_run)

    sandbox = subparsers.add_parser(
        "sandbox",
        help="run or manage workloads over EROFS layers and private ext4 scratch",
    )

    def sandbox_layer(value: str) -> SandboxLayer:
        try:
            return SandboxLayer.parse(value)
        except ScriptError as error:
            raise argparse.ArgumentTypeError(str(error)) from error

    def sandbox_identity(value: str) -> tuple[int, int]:
        try:
            return parse_workload_identity(value)
        except ScriptError as error:
            raise argparse.ArgumentTypeError(str(error)) from error

    sandbox.add_argument(
        "sandbox_operation",
        nargs="?",
        choices=("run", "provision", "start", "exec", "stop", "deprovision"),
        default="run",
    )
    sandbox.add_argument(
        "--layer",
        action="append",
        default=[],
        type=sandbox_layer,
        metavar="ROLE,PATH,EROFS_UUID",
    )
    sandbox.add_argument("--scratch", type=Path)
    sandbox.add_argument("--state-dir", type=Path)
    sandbox.add_argument("--entrypoint", default="/bin/sh")
    sandbox.add_argument("--arg", action="append", default=[], dest="sandbox_arg")
    sandbox.add_argument("--hostname", default="nvx-sandbox")
    sandbox.add_argument(
        "--workload-user",
        type=sandbox_identity,
        default=parse_workload_identity("65534:65534"),
        metavar="UID:GID",
        help="fixed non-root workload identity (default: 65534:65534)",
    )
    sandbox.add_argument("--memory-max", type=int)
    sandbox.add_argument("--pids-max", type=int)
    sandbox.add_argument("--memory-mib", type=int, default=256)
    sandbox.add_argument(
        "--timeout",
        type=float,
        default=60.0,
        help="control operation timeout in seconds (default: 60)",
    )
    sandbox.add_argument(
        "--exec-timeout-ms",
        type=int,
        default=0,
        help="guest workload timeout in milliseconds; zero disables it",
    )
    sandbox.add_argument("--hypervisor", choices=HYPERVISORS, default="auto")
    sandbox.add_argument("--net", metavar="IPV4/PREFIX")
    sandbox.add_argument("--network-profile", choices=NETWORK_PROFILES)
    sandbox.add_argument("--network-egress", choices=("allow", "deny"))
    sandbox.add_argument("--network-ingress", choices=("allow", "deny"))
    sandbox.add_argument("--network-egress-allow", action="append", default=[])
    sandbox.add_argument("--network-egress-deny", action="append", default=[])
    sandbox.add_argument("--host-loopback", choices=("allow", "deny"))
    sandbox.add_argument("--network-proxy", metavar="IPV4:TCP-PORT")
    sandbox.add_argument("--host-loopback-forward", action="append", default=[])
    sandbox.add_argument(
        "--outcome-report",
        type=Path,
        help="write a bounded local JSON outcome report for run or exec",
    )
    sandbox.add_argument("--cmdline", default="")
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
