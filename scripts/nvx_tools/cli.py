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
    DEFAULT_PYTHON_PROFILE,
    PYTHON_INITRAMFS_PROFILES,
    AlpineBuildConfig,
    DockerBuildConfig,
    KernelBuildConfig,
    build_docker_artifacts,
    build_initramfs,
    build_kernel,
    build_python_initramfs_native,
    python_initramfs_profile,
)
from .benchmarks import (
    ConsoleOutputConfig,
    ConsoleSnapshotConfig,
    NetworkPythonConfig,
    NetworkSnapshotConfig,
    VirtfsConfig,
    benchmark_console_output,
    benchmark_network_python,
    benchmark_network_snapshot,
    benchmark_virtfs,
    capture_console_snapshots,
    parse_output_sizes,
)
from .common import ScriptError
from .ci import setup_cross_os_cache, setup_windows_performance_toolkit
from .refactor_baseline import BaselineConfig, record_refactor_baseline
from .smoke import (
    ExecTestConfig,
    HotPagesTestConfig,
    InterruptionTestConfig,
    ProfilingTestConfig,
    test_exec,
    test_hot_pages,
    test_interruption,
    test_profiling,
)
from .vm import (
    DEFAULT_CMDLINE,
    BootTestConfig,
    HotSnapshotConfig,
    SnapshotConfig,
    VmConfig,
    benchmark_snapshot_prefetch,
    benchmark_shell_snapshot,
    boot_test,
    measure_coldstart,
    print_boot_test,
    run_vm,
    snapshot_boot,
    snapshot_hello_demo,
    snapshot_pandas_demo,
)


class NvxArgumentParser(argparse.ArgumentParser):
    def __init__(self, *args: Any, **kwargs: Any) -> None:
        kwargs.setdefault("allow_abbrev", False)
        super().__init__(*args, **kwargs)


def _path_default(environment: str, fallback: Path) -> Path:
    return Path(os.environ.get(environment, fallback))


def _int_default(environment: str, fallback: int) -> str:
    return os.environ.get(environment, str(fallback))


def _bounded_int(minimum: int, maximum: int) -> Any:
    def parse(value: str) -> int:
        try:
            parsed = int(value)
        except ValueError as error:
            raise argparse.ArgumentTypeError(
                f"must be an integer, got {value!r}"
            ) from error
        if not minimum <= parsed <= maximum:
            raise argparse.ArgumentTypeError(
                f"must be in the range {minimum}..{maximum}, got {parsed}"
            )
        return parsed

    return parse


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
    kernel.add_argument(
        "--kver", "-Kver", default=os.environ.get("KVER", DEFAULT_KERNEL_VERSION)
    )
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
        "--profile",
        choices=tuple(PYTHON_INITRAMFS_PROFILES),
        default=os.environ.get("PYTHON_PROFILE", DEFAULT_PYTHON_PROFILE),
        help="Python image profile (default: full)",
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
    run.add_argument(
        "--cmdline", "-Cmdline", default=os.environ.get("CMDLINE", DEFAULT_CMDLINE)
    )
    run.add_argument("--quiet", "-Quiet", action="store_true")
    run.add_argument("--exit-on-boot", "-ExitOnBoot", action="store_true")
    run.add_argument("--debug", "-Debug", action="store_true")
    run.add_argument("--mount", type=Path, default=os.environ.get("MOUNT"))
    run.add_argument(
        "--mount-target", default=os.environ.get("MOUNT_TARGET", "/mnt/host")
    )
    run.add_argument(
        "--mount-rw", action="store_true", default=bool(os.environ.get("MOUNT_RW"))
    )
    run.add_argument("--net", default=os.environ.get("NET"))
    egress = run.add_mutually_exclusive_group()
    egress.add_argument("--allow-host", action="append", default=[])
    egress.add_argument("--block-host", action="append", default=[])
    egress.add_argument("--allow-endpoint", action="append", default=[])

    measure = subparsers.add_parser(
        "measure-coldstart", help="benchmark cold-start configurations"
    )
    _add_vm_arguments(measure, python_initrd=False)
    measure.add_argument("--runs", "--n", "-N", type=int, default=_int_default("N", 6))

    console_output = subparsers.add_parser(
        "bench-console-output",
        help="benchmark restored merged-console throughput and latency",
    )
    console_output.add_argument("--microvm", type=Path)
    console_output.add_argument("--snapshot", "--snap", type=Path)
    console_output.add_argument("--portb-snapshot", type=Path)
    console_output.add_argument(
        "--mem", type=_bounded_int(64, 65536), default=_int_default("MEM", 512)
    )
    console_output.add_argument(
        "--runs", "--n", "-N", type=_bounded_int(1, 100), default=_int_default("N", 5)
    )
    console_output.add_argument(
        "--sizes",
        default="1024 4096 16384 65536",
        help="space-separated output byte counts in the range 1..65536",
    )
    console_output.add_argument("--timeout-sec", type=_bounded_int(1, 300), default=60)
    console_output.add_argument(
        "--nanvix-command",
        help="optional IKC benchmark command containing a {bytes} placeholder",
    )
    console_output.add_argument(
        "--nanvix-wxc",
        type=Path,
        help="optional wxc-exec path for an exact-size NanVix MicroVM comparison",
    )

    console_snapshots = subparsers.add_parser(
        "capture-console-snapshots",
        help="capture matched virtio-console and portb agent snapshots",
    )
    console_snapshots.add_argument("--kernel", "-Kernel", type=Path)
    console_snapshots.add_argument("--initrd", "-Initrd", type=Path)
    console_snapshots.add_argument("--snapshot", "--snap", type=Path)
    console_snapshots.add_argument("--portb-snapshot", type=Path)
    console_snapshots.add_argument(
        "--mem", type=_bounded_int(64, 65536), default=_int_default("MEM", 512)
    )
    console_snapshots.add_argument(
        "--timeout-sec", type=_bounded_int(1, 300), default=60
    )

    pandas_demo = subparsers.add_parser(
        "snapshot-demo-pandas",
        help="benchmark pandas after an untrained trampoline restore",
    )
    _add_snapshot_arguments(pandas_demo, default_name="pandas-snap", default_runs=8)

    hello_demo = subparsers.add_parser(
        "snapshot-demo-hello",
        help="benchmark pure hello after an untrained trampoline restore",
    )
    _add_snapshot_arguments(hello_demo, default_name="hello-snap", default_runs=8)

    shell_snapshot = subparsers.add_parser(
        "bench-snapshot-shell",
        help="benchmark shell-ready snapshots across memory sizes",
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

    hot_snapshot = subparsers.add_parser(
        "bench-snapshot-prefetch",
        help="train and A/B benchmark a WHP hot snapshot-page profile",
    )
    hot_snapshot.add_argument("--snapshot", "--snap", type=Path, required=True)
    hot_snapshot.add_argument("--marker", required=True)
    hot_snapshot.add_argument(
        "--mem", type=_bounded_int(1, 65536), default=_int_default("MEM", 512)
    )
    hot_snapshot.add_argument(
        "--runs", "--n", type=_bounded_int(1, 100), default=_int_default("N", 5)
    )
    hot_snapshot.add_argument(
        "--training-runs",
        type=_bounded_int(1, 100),
        default=_int_default("TRAINING_RUNS", 5),
    )

    snapshot_repl = subparsers.add_parser(
        "snapshot-boot", help="capture once and resume an interactive Python REPL"
    )
    _add_snapshot_arguments(
        snapshot_repl, default_name="pyrepl", default_runs=1, include_runs=False
    )
    snapshot_repl.add_argument("--smoke-test", "-SmokeTest", action="store_true")

    virtfs = subparsers.add_parser(
        "bench-virtfs", help="benchmark virt-fs I/O and persistence"
    )
    _add_vm_arguments(virtfs, python_initrd=False)
    virtfs.add_argument("--runs", "--n", "-N", type=int, default=_int_default("N", 5))
    virtfs.add_argument(
        "--payload-mib",
        "--payload-mb",
        "-PayloadMB",
        type=int,
        default=_int_default("PAYLOAD_MB", 64),
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
    boot.add_argument("--mem", "-Mem", type=int, default=_int_default("MEM", 512))
    boot.add_argument(
        "--vcpus",
        "--cores",
        type=_bounded_int(1, 254),
        default=_int_default("CORES", 1),
    )
    boot.add_argument(
        "--timeout-sec",
        "-TimeoutSec",
        type=int,
        default=_int_default("TIMEOUT_SEC", 90),
    )
    execute = subparsers.add_parser(
        "test-exec", help="validate cross-platform guest exec semantics"
    )
    execute.add_argument("--microvm", type=Path)
    execute.add_argument("--kernel", "-Kernel", type=Path)
    execute.add_argument("--initrd", "-Initrd", type=Path)
    execute.add_argument("--python-initrd", "-PythonInitrd", type=Path)
    execute.add_argument(
        "--mem",
        "--memory-mib",
        "-MemoryMiB",
        type=_bounded_int(64, 65536),
        default=_int_default("MEM", 128),
    )
    execute.add_argument(
        "--python-mem",
        type=_bounded_int(64, 65536),
        default=_int_default("PYTHON_MEM", 512),
    )
    execute.add_argument(
        "--timeout-sec",
        "--timeout-seconds",
        "-TimeoutSeconds",
        type=_bounded_int(1, 300),
        default=os.environ.get(
            "NVX_EXEC_TIMEOUT_SECONDS",
            os.environ.get("NVX_KVM_EXEC_TIMEOUT_SECONDS", "60"),
        ),
    )
    profiling = subparsers.add_parser(
        "test-profiling", help="validate guest and optional host profiling artifacts"
    )
    profiling.add_argument("--microvm", type=Path)
    profiling.add_argument("--kernel", "-Kernel", type=Path)
    profiling.add_argument("--initrd", "-Initrd", type=Path)
    profiling.add_argument(
        "--timeout-sec",
        type=int,
        default=_int_default("NVX_PROFILE_TIMEOUT_SECONDS", 120),
    )
    profiling.add_argument(
        "--require-host-profile",
        action="store_true",
        help="fail instead of falling back to guest-only profiling",
    )
    profiling.add_argument(
        "--wpr-profile",
        help="Windows WPR profile passed to the VMM host recorder",
    )
    profiling.add_argument(
        "--require-scheduling-events",
        action="store_true",
        help="require Windows context-switch events in the validated ETL",
    )
    interruption = subparsers.add_parser(
        "test-interruption", help="validate coordinated KVM/WHP host-interrupt teardown"
    )
    interruption.add_argument("--microvm", type=Path)
    interruption.add_argument("--kernel", "-Kernel", type=Path)
    interruption.add_argument("--initrd", "-Initrd", type=Path)
    interruption.add_argument(
        "--mem", type=_bounded_int(64, 65536), default=_int_default("MEM", 512)
    )
    interruption.add_argument(
        "--interrupt-after-sec",
        type=_bounded_int(1, 30),
        default=_int_default("NVX_INTERRUPT_AFTER_SECONDS", 2),
    )
    hot_pages = subparsers.add_parser(
        "test-hot-pages", help="validate native WHP hot-page training and consumption"
    )
    hot_pages.add_argument("--microvm", type=Path)
    hot_pages.add_argument("--kernel", "-Kernel", type=Path)
    hot_pages.add_argument("--initrd", "-Initrd", type=Path)
    hot_pages.add_argument(
        "--mem", type=_bounded_int(64, 65536), default=_int_default("MEM", 512)
    )
    hot_pages.add_argument("--timeout-sec", type=_bounded_int(1, 300), default=60)
    hot_pages.add_argument("--training-runs", type=_bounded_int(1, 20), default=5)

    subparsers.add_parser(
        "setup-cross-os-cache",
        help="install GNU tar and zstd for GitHub Actions cross-OS caches",
    )
    subparsers.add_parser(
        "setup-windows-performance-toolkit",
        help="stage xperf for GitHub Actions when Windows WPT is unavailable",
    )

    baseline = subparsers.add_parser(
        "refactor-baseline",
        help="record reproducible component-refactor baseline evidence",
    )
    baseline.add_argument("--role", choices=("baseline", "candidate"), required=True)
    baseline.add_argument("--base-rev", required=True)
    baseline.add_argument("--candidate-rev", required=True)
    baseline.add_argument("--source-root", type=Path)
    baseline.add_argument(
        "--output", type=Path, default=Path("build/refactor-baseline")
    )
    baseline.add_argument(
        "--suite",
        action="append",
        choices=("core", "backend", "snapshot", "benchmarks"),
        help="suite to record (repeatable; default: core)",
    )
    baseline.add_argument(
        "--artifact",
        type=Path,
        action="append",
        default=[],
        help="file or snapshot directory to hash (repeatable)",
    )
    baseline.add_argument("--console-snapshot", type=Path)
    baseline.add_argument("--portb-snapshot", type=Path)
    baseline.add_argument("--require-clean", action="store_true")

    return parser


def _add_alpine_build_arguments(
    parser: argparse.ArgumentParser,
    *,
    python_image: bool,
    optional_output: bool = False,
) -> None:
    default_work = "initramfs-python" if python_image else "initramfs"
    default_output = "initramfs-python.cpio.gz" if python_image else "initramfs.cpio.gz"
    if "WORK" in os.environ:
        work_default = Path(os.environ["WORK"])
    else:
        work_default = None if python_image else Path.home() / "build" / default_work
    if "OUT" in os.environ:
        output_default = Path(os.environ["OUT"])
    else:
        output_default = (
            None if optional_output else Path.home() / "build" / default_output
        )
    parser.add_argument(
        "--aver", "-Aver", default=os.environ.get("AVER", DEFAULT_ALPINE_VERSION)
    )
    parser.add_argument(
        "--abranch",
        "-Abranch",
        default=os.environ.get("ABRANCH", DEFAULT_ALPINE_BRANCH),
    )
    parser.add_argument(
        "--work",
        type=Path,
        default=work_default,
    )
    parser.add_argument(
        "--output",
        "--out",
        type=Path,
        default=output_default,
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
        "--abranch",
        "-Abranch",
        default=os.environ.get("ABRANCH", DEFAULT_ALPINE_BRANCH),
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
        parser.add_argument("--mem", "-Mem", type=int, default=_int_default("MEM", 512))
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
    _add_vm_path_arguments(parser, python_initrd=True, include_mem=include_mem)
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
    initrd_name = (
        "initramfs-python.cpio.gz" if args.python_initrd else "initramfs.cpio.gz"
    )
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
            profile = python_initramfs_profile(args.profile)
            work = args.work or Path.home() / "build" / profile.work_name
            output = args.output or Path.home() / "build" / profile.artifact_name
            if (backend.name == "linux-kvm" and not args.docker) or args.native:
                build_python_initramfs_native(
                    AlpineBuildConfig(args.aver, args.abranch, work, output),
                    backend,
                    args.profile,
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
                    python_profile_name=args.profile,
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
                net=args.net,
                allow_hosts=args.allow_host,
                block_hosts=args.block_host,
                allow_endpoints=args.allow_endpoint,
            )
        if args.command == "measure-coldstart":
            kernel, initrd = _vm_paths(args, backend)
            measure_coldstart(
                VmConfig(kernel, initrd, args.mem, args.vcpus), backend, args.runs
            )
            return 0
        if args.command == "bench-console-output":
            benchmark_console_output(
                ConsoleOutputConfig(
                    args.microvm or backend.executable(),
                    args.snapshot or backend.artifact("mxc-agent-snapshot"),
                    args.portb_snapshot or backend.artifact("mxc-agent-snapshot-hvc"),
                    args.mem,
                    args.runs,
                    parse_output_sizes(args.sizes),
                    args.timeout_sec,
                    args.nanvix_command,
                    args.nanvix_wxc,
                )
            )
            return 0
        if args.command == "capture-console-snapshots":
            capture_console_snapshots(
                ConsoleSnapshotConfig(
                    args.kernel or _path_default("KERNEL", backend.artifact("vmlinux")),
                    args.initrd
                    or _path_default(
                        "INITRD", backend.artifact("initramfs-python-agent.cpio.gz")
                    ),
                    args.snapshot or backend.artifact("mxc-agent-snapshot"),
                    args.portb_snapshot or backend.artifact("mxc-agent-snapshot-hvc"),
                    args.mem,
                    args.timeout_sec,
                ),
                backend,
            )
            return 0
        if args.command == "snapshot-demo-pandas":
            snapshot_pandas_demo(_snapshot_config(args, backend), backend)
            return 0
        if args.command == "snapshot-demo-hello":
            snapshot_hello_demo(_snapshot_config(args, backend), backend)
            return 0
        if args.command == "bench-snapshot-shell":
            try:
                memories = [int(value) for value in args.memories.split()]
            except ValueError as error:
                raise ScriptError(
                    "--memories must contain integer MiB values"
                ) from error
            benchmark_shell_snapshot(_snapshot_config(args, backend), backend, memories)
            return 0
        if args.command == "bench-snapshot-prefetch":
            benchmark_snapshot_prefetch(
                HotSnapshotConfig(
                    args.snapshot,
                    args.marker,
                    args.mem,
                    args.runs,
                    args.training_runs,
                ),
                backend,
            )
            return 0
        if args.command == "snapshot-boot":
            return snapshot_boot(
                _snapshot_config(args, backend), backend, smoke_test=args.smoke_test
            )
        if args.command == "bench-virtfs":
            kernel, initrd = _vm_paths(args, backend)
            benchmark_virtfs(
                VirtfsConfig(
                    kernel,
                    initrd,
                    args.mem,
                    args.runs,
                    args.vcpus,
                    args.payload_mib,
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
                kernel=args.kernel
                or _path_default("KERNEL", backend.artifact("vmlinux")),
                initrd=args.initrd
                or _path_default("INITRD", backend.artifact("initramfs.cpio.gz")),
                mem=args.mem,
                timeout=args.timeout_sec,
                vcpus=args.vcpus,
            )
            return print_boot_test(boot_test(config, backend))
        if args.command == "test-exec":
            exec_artifact = (
                backend.repo_root / "build"
                if backend.name == "linux-kvm"
                else backend.artifact_dir
            )
            kernel = args.kernel or _path_default("KERNEL", exec_artifact / "vmlinux")
            initrd = args.initrd or _path_default(
                "INITRD", exec_artifact / "initramfs.cpio.gz"
            )
            python_initrd = args.python_initrd
            if python_initrd is None and "PYTHON_INITRD" in os.environ:
                python_initrd = Path(os.environ["PYTHON_INITRD"])
            if python_initrd is None:
                default_python_initrd = exec_artifact / "initramfs-python.cpio.gz"
                if default_python_initrd.is_file():
                    python_initrd = default_python_initrd
            test_exec(
                ExecTestConfig(
                    microvm=args.microvm
                    or _path_default("MICROVM", backend.executable()),
                    kernel=kernel,
                    initrd=initrd,
                    mem=args.mem,
                    timeout=args.timeout_sec,
                    python_initrd=python_initrd,
                    python_mem=args.python_mem,
                ),
                backend,
            )
            return 0
        if args.command == "test-profiling":
            test_profiling(
                ProfilingTestConfig(
                    args.microvm or backend.executable(),
                    args.kernel
                    or _path_default("KERNEL", backend.artifact("vmlinux-profiling")),
                    args.initrd
                    or _path_default("INITRD", backend.artifact("initramfs.cpio.gz")),
                    timeout=args.timeout_sec,
                    require_host_profile=args.require_host_profile,
                    wpr_profile=args.wpr_profile,
                    require_scheduling_events=args.require_scheduling_events,
                ),
                backend,
            )
            return 0
        if args.command == "test-interruption":
            exec_artifact = (
                backend.repo_root / "build"
                if backend.name == "linux-kvm"
                else backend.artifact_dir
            )
            test_interruption(
                InterruptionTestConfig(
                    args.microvm or _path_default("MICROVM", backend.executable()),
                    args.kernel or _path_default("KERNEL", exec_artifact / "vmlinux"),
                    args.initrd
                    or _path_default("INITRD", exec_artifact / "initramfs.cpio.gz"),
                    args.mem,
                    args.interrupt_after_sec,
                ),
                backend,
            )
            return 0
        if args.command == "test-hot-pages":
            test_hot_pages(
                HotPagesTestConfig(
                    args.microvm or _path_default("MICROVM", backend.executable()),
                    args.kernel or _path_default("KERNEL", backend.artifact("vmlinux")),
                    args.initrd
                    or _path_default("INITRD", backend.artifact("initramfs.cpio.gz")),
                    args.mem,
                    args.timeout_sec,
                    args.training_runs,
                ),
                backend,
            )
            return 0
        if args.command == "setup-cross-os-cache":
            setup_cross_os_cache()
            return 0
        if args.command == "setup-windows-performance-toolkit":
            setup_windows_performance_toolkit()
            return 0
        if args.command == "refactor-baseline":
            record_refactor_baseline(
                BaselineConfig(
                    output=args.output,
                    suites=tuple(args.suite or ["core"]),
                    artifacts=tuple(args.artifact),
                    console_snapshot=args.console_snapshot,
                    portb_snapshot=args.portb_snapshot,
                    require_clean=args.require_clean,
                    role=args.role,
                    base_revision=args.base_rev,
                    candidate_revision=args.candidate_rev,
                    source_root=args.source_root,
                ),
                backend,
            )
            return 0
    except (ScriptError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    parser.error(f"unhandled command: {args.command}")
    return 2
