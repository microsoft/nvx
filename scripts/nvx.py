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

from nvx_tools.benchmark import configure_parser as configure_benchmark_parser
from nvx_tools.build import (
    AlpineBuildConfig,
    DockerBuildConfig,
    KernelBuildConfig,
    build_docker_artifacts,
    build_initramfs,
    build_kernel,
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
from nvx_tools.performance import configure_parser as configure_performance_parser
from nvx_tools.release import (
    collect_release_sources,
    download_latest_release,
    package_release,
    verify_source_tree,
)
from nvx_tools.sandbox import SandboxLaunch, SandboxLayer

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
    download_latest_release(args.repository, _release_platform(args.hypervisor))


def _format_command(command: list[str]) -> str:
    return subprocess.list2cmdline(command) if os.name == "nt" else shlex.join(command)


def command_run(args: argparse.Namespace) -> None:
    if (args.net is None) != (args.network_profile is None):
        raise ScriptError("--net and --network-profile must be specified together")
    executable = require_file(openvmm_binary_path(), "OpenVMM release binary")
    kernel = require_file(artifact_path("vmlinux"), "PVH kernel")
    initrd = require_file(
        artifact_path("initramfs.cpio.gz"),
        "initramfs",
    )
    command = [
        str(executable),
        "--single-process",
        "--machine",
        "microvm",
        "--hypervisor",
        _hypervisor(args.hypervisor),
        "--memory",
        f"{args.memory_mib}M",
        "--kernel",
        str(kernel),
        "--initrd",
        str(initrd),
    ]
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
    if (args.net is None) != (args.network_profile is None):
        raise ScriptError("--net and --network-profile must be specified together")
    launch = SandboxLaunch(
        layers=tuple(args.layer),
        scratch=args.scratch,
        entrypoint=args.entrypoint,
        args=tuple(args.sandbox_arg),
        hostname=args.hostname,
        memory_max=args.memory_max,
        pids_max=args.pids_max,
    ).validated()
    executable = require_file(openvmm_binary_path(), "OpenVMM release binary")
    kernel = require_file(artifact_path("vmlinux"), "PVH kernel")
    initrd = require_file(
        artifact_path("initramfs.cpio.gz"),
        "initramfs",
    )
    command = [
        str(executable),
        *launch.openvmm_arguments(),
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
    run.add_argument("--memory-mib", type=int, default=128)
    run.add_argument("--mount", help="GUEST_TARGET,HOST_PATH,ro|rw")
    run.add_argument("--net", metavar="IPV4/PREFIX")
    run.add_argument("--network-profile", choices=NETWORK_PROFILES)
    run.add_argument("--cmdline", default="")
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
        metavar="ROLE,PATH,EROFS_UUID",
    )
    sandbox.add_argument("--scratch", required=True, type=Path)
    sandbox.add_argument("--entrypoint", default="/bin/sh")
    sandbox.add_argument("--arg", action="append", default=[], dest="sandbox_arg")
    sandbox.add_argument("--hostname", default="nvx-sandbox")
    sandbox.add_argument("--memory-max", type=int)
    sandbox.add_argument("--pids-max", type=int)
    sandbox.add_argument("--memory-mib", type=int, default=256)
    sandbox.add_argument("--hypervisor", choices=HYPERVISORS, default="auto")
    sandbox.add_argument("--net", metavar="IPV4/PREFIX")
    sandbox.add_argument("--network-profile", choices=NETWORK_PROFILES)
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
