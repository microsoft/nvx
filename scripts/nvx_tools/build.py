"""Linux-native and Docker-backed artifact build workflows."""

from __future__ import annotations

import os
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path

from .backends.base import HostBackend
from .common import (
    REPO_ROOT,
    ScriptError,
    download,
    format_size,
    require_tool,
    run_capture,
    run_checked,
)


DEFAULT_KERNEL_VERSION = "6.18.38"
DEFAULT_ALPINE_VERSION = "3.24.1"
DEFAULT_ALPINE_BRANCH = "v3.24"
KCONFIG_XE9 = """

config HVC_XE9
\tbool "microvm portb (0xE9/0xEA) hypervisor console"
\tdepends on X86
\tselect HVC_DRIVER
\thelp
\t  Bidirectional hypervisor console for the microvm VMM: output is one
\t  outb per byte to I/O port 0xE9, input is polled from 0xEA/0xE9. It
\t  registers as hvc0 and replaces the 16550 UART. Select with console=hvc0.
"""


@dataclass(frozen=True)
class AlpineBuildConfig:
    version: str = DEFAULT_ALPINE_VERSION
    branch: str = DEFAULT_ALPINE_BRANCH
    work: Path = Path.home() / "build" / "initramfs"
    output: Path = Path.home() / "build" / "initramfs.cpio.gz"


@dataclass(frozen=True)
class KernelBuildConfig:
    version: str = DEFAULT_KERNEL_VERSION
    work: Path = Path.home() / "build" / "kernel"
    output: Path = Path.home() / "build" / "vmlinux"


@dataclass(frozen=True)
class DockerBuildConfig:
    destination: Path = Path("build")
    kernel_version: str = DEFAULT_KERNEL_VERSION
    alpine_version: str = DEFAULT_ALPINE_VERSION
    alpine_branch: str = DEFAULT_ALPINE_BRANCH


def _require_linux(backend: HostBackend, workflow: str) -> None:
    if backend.name != "linux-kvm":
        raise ScriptError(
            f"{workflow} requires a Linux host; use build-linux-artifacts with Docker"
        )


def _alpine_tarball(config: AlpineBuildConfig) -> Path:
    return config.work / f"alpine-minirootfs-{config.version}-x86_64.tar.gz"


def _prepare_alpine_root(config: AlpineBuildConfig) -> Path:
    config.work.mkdir(parents=True, exist_ok=True)
    tarball = _alpine_tarball(config)
    if not tarball.is_file():
        print(f">> downloading {tarball.name}")
        download(
            "https://dl-cdn.alpinelinux.org/alpine/"
            f"{config.branch}/releases/x86_64/{tarball.name}",
            tarball,
        )
    root = config.work / "root"
    shutil.rmtree(root, ignore_errors=True)
    root.mkdir(parents=True)
    require_tool("tar")
    run_checked(["tar", "-xzf", tarball, "-C", root])
    return root


def _install(source: Path, destination: Path) -> None:
    shutil.copy2(source, destination)
    destination.chmod(0o755)


def _install_hcs_plan9(root: Path) -> None:
    destination = root / "sbin" / "hcs-plan9"
    run_checked(
        [
            "cc",
            "-static",
            "-Os",
            "-s",
            "-Wall",
            "-Wextra",
            "-o",
            destination,
            REPO_ROOT / "alpine" / "hcs-plan9.c",
        ]
    )


def _apk_add(root: Path, *packages: str) -> None:
    loader = root / "lib" / "ld-musl-x86_64.so.1"
    environment = os.environ.copy()
    environment["LD_LIBRARY_PATH"] = f"{root / 'lib'}:{root / 'usr' / 'lib'}"
    run_checked(
        [
            loader,
            root / "sbin" / "apk",
            "--root",
            root,
            "--no-cache",
            "--no-interactive",
            "add",
            *packages,
        ],
        env=environment,
    )


def _pack_initramfs(root: Path, output: Path) -> None:
    require_tool("find")
    require_tool("cpio")
    require_tool("gzip")
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("wb") as archive:
        finder = subprocess.Popen(["find", "."], cwd=root, stdout=subprocess.PIPE)
        assert finder.stdout is not None
        cpio = subprocess.Popen(
            ["cpio", "--quiet", "-o", "-H", "newc"],
            cwd=root,
            stdin=finder.stdout,
            stdout=subprocess.PIPE,
        )
        finder.stdout.close()
        assert cpio.stdout is not None
        gzip = subprocess.Popen(["gzip", "-9"], stdin=cpio.stdout, stdout=archive)
        cpio.stdout.close()
        gzip_code = gzip.wait()
        cpio_code = cpio.wait()
        finder_code = finder.wait()
    if finder_code or cpio_code or gzip_code:
        output.unlink(missing_ok=True)
        raise ScriptError(
            "failed to pack initramfs "
            f"(find={finder_code}, cpio={cpio_code}, gzip={gzip_code})"
        )


def build_initramfs(config: AlpineBuildConfig, backend: HostBackend) -> None:
    _require_linux(backend, "build-initramfs")
    root = _prepare_alpine_root(config)
    print(">> installing busybox-extras into the rootfs")
    _apk_add(root, "busybox-extras")
    resolver = root / "etc" / "resolv.conf"
    resolver.unlink(missing_ok=True)
    resolver.touch()
    _install(REPO_ROOT / "alpine" / "init", root / "init")
    _install(REPO_ROOT / "alpine" / "nvx-snapshot", root / "sbin" / "nvx-snapshot")
    _install_hcs_plan9(root)
    _pack_initramfs(root, config.output)
    print(f">> built {config.output} ({format_size(config.output.stat().st_size)})")


def build_python_initramfs_native(
    config: AlpineBuildConfig, backend: HostBackend
) -> None:
    _require_linux(backend, "native build-python-initramfs")
    root = _prepare_alpine_root(config)
    print(">> installing python3 + pandas/numpy into the rootfs")
    _apk_add(root, "python3", "py3-numpy", "py3-pandas")
    for source_name, destination_name in (
        ("init.python", "init"),
        ("hello.py", "hello.py"),
        ("repl.py", "repl.py"),
        ("net-hello.py", "net-hello.py"),
        ("net-pandas.py", "net-pandas.py"),
    ):
        _install(REPO_ROOT / "alpine" / source_name, root / destination_name)
    _install(REPO_ROOT / "alpine" / "nvx-snapshot", root / "sbin" / "nvx-snapshot")
    _pack_initramfs(root, config.output)
    print(f">> built {config.output} ({format_size(config.output.stat().st_size)})")


def build_kernel(config: KernelBuildConfig, backend: HostBackend) -> None:
    _require_linux(backend, "build-kernel")
    for tool in ("tar", "patch", "make", "readelf"):
        require_tool(tool)
    config.work.mkdir(parents=True, exist_ok=True)
    source = config.work / f"linux-{config.version}"
    tarball = config.work / f"linux-{config.version}.tar.xz"
    if not source.is_dir():
        print(f">> downloading linux-{config.version}")
        download(
            f"https://cdn.kernel.org/pub/linux/kernel/v6.x/{tarball.name}", tarball
        )
        run_checked(["tar", "-xf", tarball], cwd=config.work)

    early_printk = source / "arch" / "x86" / "kernel" / "early_printk.c"
    if "early_xe9_write" not in early_printk.read_text(encoding="utf-8"):
        patch = REPO_ROOT / "kernel" / "patches" / "0001-microvm-xe9-earlycon.patch"
        print(f">> applying {patch.name}")
        run_checked(["patch", "-p1", "-i", patch], cwd=source)

    print(">> installing hvc_xe9 console driver")
    hvc_dir = source / "drivers" / "tty" / "hvc"
    shutil.copy2(REPO_ROOT / "kernel" / "hvc_xe9.c", hvc_dir / "hvc_xe9.c")
    makefile = hvc_dir / "Makefile"
    makefile_text = makefile.read_text(encoding="utf-8")
    if "hvc_xe9.o" not in makefile_text:
        with makefile.open("a", encoding="utf-8") as output:
            output.write("obj-$(CONFIG_HVC_XE9)\t\t+= hvc_xe9.o\n")
    kconfig = hvc_dir / "Kconfig"
    kconfig_text = kconfig.read_text(encoding="utf-8")
    if "HVC_XE9" not in kconfig_text:
        with kconfig.open("a", encoding="utf-8") as output:
            output.write(KCONFIG_XE9)

    shutil.copy2(REPO_ROOT / "kernel" / "config-microvm", source / ".config")
    run_checked(["make", "olddefconfig"], cwd=source)
    jobs = os.cpu_count() or 1
    print(f">> building vmlinux with {jobs} jobs")
    run_checked(["make", f"-j{jobs}", "vmlinux"], cwd=source)
    config.output.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source / "vmlinux", config.output)
    print(f">> built {config.output}")

    notes = run_capture(["readelf", "-n", source / "vmlinux"])
    if "Xen" in notes.text and "0x00000012" in notes.text:
        print(">> PVH entry note present")
    else:
        print("!! WARNING: PVH entry note not found - check CONFIG_PVH")


def docker_build_command(
    config: DockerBuildConfig, target: str
) -> list[str | Path]:
    destination = _docker_destination(config.destination)
    command: list[str | Path] = [
        "docker",
        "build",
        "-f",
        REPO_ROOT / "docker" / "Dockerfile",
        "--target",
        target,
    ]
    if target == "artifacts":
        command.extend(["--build-arg", f"KVER={config.kernel_version}"])
    command.extend(
        [
            "--build-arg",
            f"AVER={config.alpine_version}",
            "--build-arg",
            f"ABRANCH={config.alpine_branch}",
            "--output",
            f"type=local,dest={destination}",
            REPO_ROOT,
        ]
    )
    return command


def _docker_destination(destination: Path) -> Path:
    if not destination.is_absolute():
        destination = REPO_ROOT / destination
    return destination.resolve()


def build_docker_artifacts(
    config: DockerBuildConfig,
    python_only: bool,
    output: Path | None = None,
) -> None:
    require_tool(
        "docker",
        "docker was not found on PATH; install Docker with the Linux engine first",
    )
    target = "python-artifacts" if python_only else "artifacts"
    destination = _docker_destination(config.destination)
    if python_only:
        print(
            f">> building the Python initramfs into '{destination}' "
            f"(Alpine {config.alpine_version})"
        )
    else:
        print(
            f">> building Linux artifacts into '{destination}' "
            f"(kernel {config.kernel_version}, Alpine {config.alpine_version})"
        )
    run_checked(docker_build_command(config, target), cwd=REPO_ROOT)
    expected = (
        ("initramfs-python.cpio.gz",)
        if python_only
        else ("vmlinux", "initramfs.cpio.gz")
    )
    missing = [name for name in expected if not (destination / name).is_file()]
    if missing:
        raise ScriptError(f"Docker build did not produce: {', '.join(missing)}")
    requested_output: Path | None = None
    if output is not None:
        if not python_only:
            raise ScriptError("an explicit output file is only supported for Python artifacts")
        requested_output = output.expanduser().resolve()
        requested_output.parent.mkdir(parents=True, exist_ok=True)
        source = destination / expected[0]
        if source.resolve() != requested_output:
            shutil.copy2(source, requested_output)
    print(">> done:")
    for name in expected:
        path = destination / name
        print(f"  {path} ({format_size(path.stat().st_size)})")
    if requested_output is not None and requested_output != (destination / expected[0]):
        print(
            f"  {requested_output} "
            f"({format_size(requested_output.stat().st_size)})"
        )
