"""Command-line interface for nvx development workflows."""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path
from typing import Any, Sequence

from .backends import select_backend
from .build import (
    DEFAULT_ALPINE_BRANCH,
    DEFAULT_ALPINE_VERSION,
    DEFAULT_KERNEL_VERSION,
    AlpineBuildConfig,
    DockerBuildConfig,
    KernelBuildConfig,
    build_docker_artifacts,
    build_initramfs,
    build_kernel,
    build_python_initramfs_native,
)
from .benchmarks import (
    NetworkPythonConfig,
    NetworkSnapshotConfig,
    VirtfsConfig,
    benchmark_network_python,
    benchmark_network_snapshot,
    benchmark_virtfs,
)
from .common import ScriptError
from .vm import (
    DEFAULT_CMDLINE,
    BootTestConfig,
    SnapshotConfig,
    VmConfig,
    benchmark_shell_snapshot,
    boot_test,
    measure_coldstart,
    print_boot_test,
    run_vm,
    snapshot_boot,
    snapshot_demo,
)


class NvxArgumentParser(argparse.ArgumentParser):
    def __init__(self, *args: Any, **kwargs: Any) -> None:
        kwargs.setdefault("allow_abbrev", False)
        super().__init__(*args, **kwargs)


def _path_default(environment: str, fallback: Path) -> Path:
    return Path(os.environ.get(environment, fallback))


def _int_default(environment: str, fallback: int) -> str:
    return os.environ.get(environment, str(fallback))


def build_parser() -> argparse.ArgumentParser:
    parser = NvxArgumentParser(
        description="Cross-platform build, test, and benchmark workflows for nvx."
    )
    parser.add_argument(
        "--backend",
        choices=("auto", "linux", "kvm", "windows", "whp"),
        default="auto",
        help="host backend (default: detect current platform)",
    )
    subparsers = parser.add_subparsers(
        dest="command", required=True, parser_class=NvxArgumentParser
    )

    kernel = subparsers.add_parser(
        "build-kernel", help="build the PVH Linux kernel on a Linux host"
    )
    kernel.add_argument("--kver", "-Kver", default=os.environ.get("KVER", DEFAULT_KERNEL_VERSION))
    kernel.add_argument(
        "--work",
        type=Path,
        default=_path_default("WORK", Path.home() / "build" / "kernel"),
    )
    kernel.add_argument(
        "--output",
        "--out",
        type=Path,
        default=Path(os.environ["OUT"]) if "OUT" in os.environ else None,
    )
    kernel.add_argument(
        "--profiling",
        "-Profiling",
        action="store_true",
        default=os.environ.get("PROFILE") == "1",
        help="build a frame-pointer kernel for guest profiling",
    )

    initramfs = subparsers.add_parser(
        "build-initramfs", help="build the base Alpine initramfs on a Linux host"
    )
    _add_alpine_build_arguments(initramfs, python_image=False)

    python_initramfs = subparsers.add_parser(
        "build-python-initramfs", help="build the Python initramfs"
    )
    _add_alpine_build_arguments(
        python_initramfs, python_image=True, optional_output=True
    )
    python_initramfs.add_argument(
        "--native",
        action="store_true",
        help="force the native Linux builder instead of Docker",
    )
    python_initramfs.add_argument(
        "--docker",
        action="store_true",
        help="force the portable Docker builder",
    )
    python_initramfs.add_argument(
        "--dest",
        "-Dest",
        type=Path,
        default=_path_default("DEST", Path("build")),
        help="Docker export directory (Windows default: build)",
    )

    artifacts = subparsers.add_parser(
        "build-linux-artifacts", help="build kernel and initramfs through Docker"
    )
    _add_docker_build_arguments(artifacts)

    run = subparsers.add_parser("run", aliases=["boot"], help="launch the micro-VM")
    _add_vm_arguments(run, python_initrd=False)
    run.add_argument("--cmdline", "-Cmdline", default=os.environ.get("CMDLINE", DEFAULT_CMDLINE))
    run.add_argument("--quiet", "-Quiet", action="store_true")
    run.add_argument("--exit-on-boot", "-ExitOnBoot", action="store_true")
    run.add_argument("--debug", "-Debug", action="store_true")
    run.add_argument("--mount", type=Path, default=os.environ.get("MOUNT"))
    run.add_argument("--mount-target", default=os.environ.get("MOUNT_TARGET", "/mnt/host"))
    run.add_argument("--mount-rw", action="store_true", default=bool(os.environ.get("MOUNT_RW")))
    run.add_argument("--mount-image", type=Path, default=os.environ.get("MOUNT_IMAGE"))
    run.add_argument("--mount-size", type=int)
    run.add_argument("--net", default=os.environ.get("NET"))
    egress = run.add_mutually_exclusive_group()
    egress.add_argument("--allow-host", action="append", default=[])
    egress.add_argument("--block-host", action="append", default=[])

    measure = subparsers.add_parser("measure-coldstart", help="benchmark cold-start configurations")
    _add_vm_arguments(measure, python_initrd=False)
    measure.add_argument(
        "--runs", "--n", "-N", type=int, default=_int_default("N", 6)
    )

    demo = subparsers.add_parser("snapshot-demo", help="benchmark a warmed Python snapshot")
    _add_snapshot_arguments(demo, default_name="pysnap", default_runs=8)

    shell_snapshot = subparsers.add_parser(
        "bench-snapshot-shell", help="benchmark shell-ready snapshots across memory sizes"
    )
    _add_snapshot_arguments(
        shell_snapshot,
        default_name="shellsnap",
        default_runs=15,
        include_mem=False,
    )
    shell_snapshot.set_defaults(python_initrd=False)
    shell_snapshot.add_argument(
        "--memories",
        default=os.environ.get("MEMS", "64 128 256 512"),
        help="space-separated guest memory sizes in MiB",
    )

    snapshot_repl = subparsers.add_parser(
        "snapshot-boot", help="capture once and resume an interactive Python REPL"
    )
    _add_snapshot_arguments(
        snapshot_repl, default_name="pyrepl", default_runs=1, include_runs=False
    )
    snapshot_repl.add_argument("--smoke-test", "-SmokeTest", action="store_true")

    virtfs = subparsers.add_parser("bench-virtfs", help="benchmark virt-fs I/O and persistence")
    _add_vm_arguments(virtfs, python_initrd=False)
    virtfs.add_argument(
        "--runs", "--n", "-N", type=int, default=_int_default("N", 5)
    )
    virtfs.add_argument(
        "--payload-mib",
        "--payload-mb",
        "-PayloadMB",
        type=int,
        default=_int_default("PAYLOAD_MB", 64),
    )
    virtfs.add_argument(
        "--image-mib",
        "--img-mb",
        "-ImgMB",
        type=int,
        default=os.environ.get("IMG_MB"),
    )

    network = subparsers.add_parser(
        "bench-net-snapshot", help="benchmark networking across snapshot restore"
    )
    _add_snapshot_arguments(
        network, default_name="netsnap", default_runs=10, include_vcpus=True
    )
    network.set_defaults(python_initrd=False, mem=_int_default("MEM", 256))
    network.add_argument("--net", "-Net", default=os.environ.get("NET", "10.0.0.2/24"))

    network_python = subparsers.add_parser(
        "bench-net-snapshot-py", help="benchmark networked Python snapshot workloads"
    )
    _add_vm_arguments(network_python, python_initrd=True)
    network_python.add_argument(
        "--runs", "--n", "-N", type=int, default=_int_default("N", 8)
    )
    network_python.add_argument(
        "--net", "-Net", default=os.environ.get("NET", "10.0.0.2/24")
    )
    network_python.add_argument(
        "--port", "-Port", type=int, default=_int_default("PORT", 8099)
    )

    boot = subparsers.add_parser(
        "test-boot", help="boot the VM and require the userspace marker"
    )
    boot.add_argument("--kernel", "-Kernel", type=Path)
    boot.add_argument("--initrd", "-Initrd", type=Path)
    boot.add_argument(
        "--mem", "-Mem", type=int, default=_int_default("MEM", 512)
    )
    boot.add_argument(
        "--timeout-sec",
        "-TimeoutSec",
        type=int,
        default=_int_default("TIMEOUT_SEC", 90),
    )
    return parser


def _add_alpine_build_arguments(
    parser: argparse.ArgumentParser,
    *,
    python_image: bool,
    optional_output: bool = False,
) -> None:
    default_work = "initramfs-python" if python_image else "initramfs"
    default_output = (
        "initramfs-python.cpio.gz" if python_image else "initramfs.cpio.gz"
    )
    parser.add_argument(
        "--aver", "-Aver", default=os.environ.get("AVER", DEFAULT_ALPINE_VERSION)
    )
    parser.add_argument(
        "--abranch", "-Abranch", default=os.environ.get("ABRANCH", DEFAULT_ALPINE_BRANCH)
    )
    parser.add_argument(
        "--work",
        type=Path,
        default=_path_default("WORK", Path.home() / "build" / default_work),
    )
    parser.add_argument(
        "--output",
        "--out",
        type=Path,
        default=(
            os.environ.get("OUT")
            if optional_output
            else _path_default("OUT", Path.home() / "build" / default_output)
        ),
    )


def _add_docker_build_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--dest", "-Dest", type=Path, default=_path_default("DEST", Path("build"))
    )
    parser.add_argument(
        "--kver", "-Kver", default=os.environ.get("KVER", DEFAULT_KERNEL_VERSION)
    )
    parser.add_argument(
        "--aver", "-Aver", default=os.environ.get("AVER", DEFAULT_ALPINE_VERSION)
    )
    parser.add_argument(
        "--abranch", "-Abranch", default=os.environ.get("ABRANCH", DEFAULT_ALPINE_BRANCH)
    )
    parser.add_argument(
        "--profiling",
        "-Profiling",
        action="store_true",
        default=os.environ.get("PROFILE") == "1",
        help="build a profiling kernel as vmlinux-profiling",
    )


def _add_vm_path_arguments(
    parser: argparse.ArgumentParser,
    *,
    python_initrd: bool,
    include_mem: bool = True,
) -> None:
    parser.add_argument("--kernel", "-Kernel", type=Path)
    parser.add_argument("--initrd", "-Initrd", type=Path)
    if include_mem:
        parser.add_argument(
            "--mem", "-Mem", type=int, default=_int_default("MEM", 512)
        )
    else:
        parser.set_defaults(mem=512)
    parser.set_defaults(python_initrd=python_initrd)


def _add_vm_arguments(parser: argparse.ArgumentParser, *, python_initrd: bool) -> None:
    _add_vm_path_arguments(parser, python_initrd=python_initrd)
    parser.add_argument(
        "--vcpus", "--cores", type=int, default=_int_default("CORES", 1)
    )


def _add_snapshot_arguments(
    parser: argparse.ArgumentParser,
    *,
    default_name: str,
    default_runs: int,
    include_vcpus: bool = False,
    include_runs: bool = True,
    include_mem: bool = True,
) -> None:
    _add_vm_path_arguments(
        parser, python_initrd=True, include_mem=include_mem
    )
    if include_vcpus:
        parser.add_argument(
            "--vcpus", "--cores", type=int, default=_int_default("CORES", 1)
        )
    parser.add_argument("--snapshot", "--snap", "-Snap", type=Path)
    if include_runs:
        parser.add_argument(
            "--runs",
            "--n",
            "-N",
            type=int,
            default=_int_default("N", default_runs),
        )
    else:
        parser.set_defaults(runs=default_runs)
    parser.set_defaults(snapshot_name=default_name)


def _vm_paths(args: argparse.Namespace, backend: object) -> tuple[Path, Path]:
    artifact = backend.artifact
    initrd_name = "initramfs-python.cpio.gz" if args.python_initrd else "initramfs.cpio.gz"
    return (
        args.kernel or _path_default("KERNEL", artifact("vmlinux")),
        args.initrd or _path_default("INITRD", artifact(initrd_name)),
    )


def _snapshot_config(args: argparse.Namespace, backend: object) -> SnapshotConfig:
    kernel, initrd = _vm_paths(args, backend)
    snapshot = args.snapshot or _path_default(
        "SNAP" if args.snapshot_name != "shellsnap" else "SNAPDIR",
        backend.artifact(args.snapshot_name),
    )
    return SnapshotConfig(kernel, initrd, snapshot, args.mem, args.runs)


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        backend = select_backend(args.backend)
        if args.command == "build-kernel":
            output = args.output or Path.home() / "build" / (
                "vmlinux-profiling" if args.profiling else "vmlinux"
            )
            build_kernel(
                KernelBuildConfig(args.kver, args.work, output, args.profiling),
                backend,
            )
            return 0
        if args.command == "build-initramfs":
            build_initramfs(
                AlpineBuildConfig(args.aver, args.abranch, args.work, args.output),
                backend,
            )
            return 0
        if args.command == "build-python-initramfs":
            if args.native and args.docker:
                raise ScriptError("--native and --docker are mutually exclusive")
            if (backend.name == "linux-kvm" and not args.docker) or args.native:
                output = (
                    args.output
                    or Path.home() / "build" / "initramfs-python.cpio.gz"
                )
                build_python_initramfs_native(
                    AlpineBuildConfig(args.aver, args.abranch, args.work, output),
                    backend,
                )
            else:
                build_docker_artifacts(
                    DockerBuildConfig(
                        args.dest,
                        DEFAULT_KERNEL_VERSION,
                        args.aver,
                        args.abranch,
                    ),
                    python_only=True,
                    output=args.output,
                )
            return 0
        if args.command == "build-linux-artifacts":
            build_docker_artifacts(
                DockerBuildConfig(
                    args.dest,
                    args.kver,
                    args.aver,
                    args.abranch,
                    args.profiling,
                ),
                python_only=False,
            )
            return 0
        if args.command in {"run", "boot"}:
            kernel, initrd = _vm_paths(args, backend)
            return run_vm(
                VmConfig(kernel, initrd, args.mem, args.vcpus, args.cmdline),
                backend,
                profile="debug" if args.debug else "release",
                quiet=args.quiet,
                exit_on_boot=args.exit_on_boot,
                mount=args.mount,
                mount_target=args.mount_target,
                mount_rw=args.mount_rw,
                mount_image=args.mount_image,
                mount_size=args.mount_size,
                net=args.net,
                allow_hosts=args.allow_host,
                block_hosts=args.block_host,
            )
        if args.command == "measure-coldstart":
            kernel, initrd = _vm_paths(args, backend)
            measure_coldstart(
                VmConfig(kernel, initrd, args.mem, args.vcpus), backend, args.runs
            )
            return 0
        if args.command == "snapshot-demo":
            snapshot_demo(_snapshot_config(args, backend), backend)
            return 0
        if args.command == "bench-snapshot-shell":
            try:
                memories = [int(value) for value in args.memories.split()]
            except ValueError as error:
                raise ScriptError("--memories must contain integer MiB values") from error
            benchmark_shell_snapshot(_snapshot_config(args, backend), backend, memories)
            return 0
        if args.command == "snapshot-boot":
            return snapshot_boot(
                _snapshot_config(args, backend), backend, smoke_test=args.smoke_test
            )
        if args.command == "bench-virtfs":
            kernel, initrd = _vm_paths(args, backend)
            image_mib = args.image_mib or (args.payload_mib * 2 + 64)
            benchmark_virtfs(
                VirtfsConfig(
                    kernel,
                    initrd,
                    args.mem,
                    args.runs,
                    args.vcpus,
                    args.payload_mib,
                    image_mib,
                ),
                backend,
            )
            return 0
        if args.command == "bench-net-snapshot":
            snapshot = _snapshot_config(args, backend)
            benchmark_network_snapshot(
                NetworkSnapshotConfig(
                    snapshot.kernel,
                    snapshot.initrd,
                    snapshot.snapshot,
                    snapshot.mem,
                    snapshot.runs,
                    args.vcpus,
                    args.net,
                ),
                backend,
            )
            return 0
        if args.command == "bench-net-snapshot-py":
            kernel, initrd = _vm_paths(args, backend)
            benchmark_network_python(
                NetworkPythonConfig(
                    kernel,
                    initrd,
                    args.mem,
                    args.runs,
                    args.vcpus,
                    args.net,
                    args.port,
                ),
                backend,
            )
            return 0
        if args.command == "test-boot":
            config = BootTestConfig(
                kernel=args.kernel or _path_default("KERNEL", backend.artifact("vmlinux")),
                initrd=args.initrd
                or _path_default("INITRD", backend.artifact("initramfs.cpio.gz")),
                mem=args.mem,
                timeout=args.timeout_sec,
            )
            return print_boot_test(boot_test(config, backend))
    except (ScriptError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    parser.error(f"unhandled command: {args.command}")
    return 2
