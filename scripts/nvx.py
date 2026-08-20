#!/usr/bin/env python3
"""Build, run, benchmark, and package the OpenVMM/NVX distribution."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shlex
import shutil
import subprocess
import sys
import tarfile

from nvx_tools.backends import LinuxBackend
from nvx_tools.build import (
    AlpineBuildConfig,
    DEFAULT_ALPINE_BRANCH,
    DEFAULT_ALPINE_MINIROOTFS_SHA256,
    DEFAULT_ALPINE_VERSION,
    DEFAULT_KERNEL_SHA256,
    DEFAULT_KERNEL_VERSION,
    DEFAULT_KERNEL_URL,
    DockerBuildConfig,
    KernelBuildConfig,
    build_docker_artifacts,
    build_docker_linux_source,
    build_initramfs,
    build_kernel,
    build_python_initramfs_native,
)
from nvx_tools.common import ScriptError


REPO_ROOT = Path(__file__).resolve().parents[1]
BUILD_DIR = REPO_ROOT / "build"
SOURCE_DIR = BUILD_DIR / "sources"
OPENVMM_DIR = REPO_ROOT / "openvmm"
PROJECT_SOURCE_PATHS = (
    "alpine",
    "benchmarks",
    "docker",
    "kernel",
    "licenses",
    "scripts",
    ".dockerignore",
    ".gitattributes",
    ".gitmodules",
    ".gitignore",
    "LICENSE",
    "LICENSE-NVX",
    "Makefile",
    "README.md",
    "SOURCE-MANIFEST.json",
    "THIRD_PARTY_NOTICES.md",
    "VERSION",
)


def _run(args: list[str | os.PathLike[str]], *, cwd: Path = REPO_ROOT) -> None:
    command = [os.fspath(arg) for arg in args]
    print(f">> {shlex.join(command)}")
    subprocess.run(command, cwd=cwd, check=True)


def _require_file(path: Path, description: str) -> Path:
    if not path.is_file():
        raise ScriptError(f"{description} not found: {path}")
    return path


def _openvmm_binary() -> Path:
    suffix = ".exe" if os.name == "nt" else ""
    return OPENVMM_DIR / "target" / "release" / f"openvmm{suffix}"


def _artifact(name: str) -> Path:
    return BUILD_DIR / name


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _write_checksums(directory: Path) -> None:
    files = sorted(
        path
        for path in directory.rglob("*")
        if path.is_file() and path.name != "SHA256SUMS"
    )
    lines = [
        f"{_sha256(path)}  {path.relative_to(directory).as_posix()}" for path in files
    ]
    (directory / "SHA256SUMS").write_text("\n".join(lines) + "\n", encoding="ascii")


def command_init(_: argparse.Namespace) -> None:
    _run(["git", "submodule", "update", "--init", "--recursive"])


def _native_kernel(profiling: bool) -> None:
    name = "vmlinux-profiling" if profiling else "vmlinux"
    work = BUILD_DIR / ("linux-profiling" if profiling else "linux")
    build_kernel(
        KernelBuildConfig(
            work=work,
            output=_artifact(name),
            profiling=profiling,
        ),
        LinuxBackend(),
    )


def _native_initramfs(profile: str) -> None:
    if profile == "base":
        config = AlpineBuildConfig(
            work=BUILD_DIR / "initramfs-work",
            output=_artifact("initramfs.cpio.gz"),
        )
        build_initramfs(config, LinuxBackend())
        return
    name = (
        "initramfs-python.cpio.gz"
        if profile == "full"
        else "initramfs-python-agent.cpio.gz"
    )
    build_python_initramfs_native(
        AlpineBuildConfig(
            work=BUILD_DIR / f"initramfs-{profile}-work",
            output=_artifact(name),
        ),
        LinuxBackend(),
        profile,
    )


def command_build_guest(args: argparse.Namespace) -> None:
    profiles = ("full", "agent") if args.python == "all" else (args.python,)
    if args.native:
        _native_kernel(False)
        if args.profiling:
            _native_kernel(True)
        _native_initramfs("base")
        for profile in profiles:
            if profile != "none":
                _native_initramfs(profile)
        return

    config = DockerBuildConfig(destination=BUILD_DIR)
    build_docker_artifacts(config, python_only=False)
    if args.profiling:
        build_docker_artifacts(
            DockerBuildConfig(destination=BUILD_DIR, profiling=True),
            python_only=False,
        )
    for profile in profiles:
        if profile != "none":
            build_docker_artifacts(
                config,
                python_only=True,
                python_profile_name=profile,
            )


def command_build_kernel(args: argparse.Namespace) -> None:
    _native_kernel(args.profiling)


def command_build_initramfs(args: argparse.Namespace) -> None:
    _native_initramfs(args.profile)


def command_build_openvmm(args: argparse.Namespace) -> None:
    _require_file(OPENVMM_DIR / "Cargo.toml", "initialized OpenVMM submodule")
    if not args.skip_restore:
        _run(["cargo", "xflowey", "restore-packages"], cwd=OPENVMM_DIR)
    _run(
        ["cargo", "build", "--release", "-p", "openvmm", "--bin", "openvmm"],
        cwd=OPENVMM_DIR,
    )


def command_build(args: argparse.Namespace) -> None:
    command_build_guest(args)
    command_build_openvmm(args)


def _initrd_name(profile: str) -> str:
    return {
        "base": "initramfs.cpio.gz",
        "python": "initramfs-python.cpio.gz",
        "python-agent": "initramfs-python-agent.cpio.gz",
    }[profile]


def _hypervisor(selected: str) -> str:
    if selected != "auto":
        return selected
    return "whp" if os.name == "nt" else "kvm"


def _format_command(command: list[str]) -> str:
    return subprocess.list2cmdline(command) if os.name == "nt" else shlex.join(command)


def command_run(args: argparse.Namespace) -> None:
    executable = _require_file(_openvmm_binary(), "OpenVMM release binary")
    kernel = _require_file(_artifact("vmlinux"), "PVH kernel")
    initrd = _require_file(
        _artifact(_initrd_name(args.initrd)),
        f"{args.initrd} initramfs",
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
        command.extend(["--net", args.net])
    if args.cmdline:
        command.extend(["--cmdline", args.cmdline])
    print(f">> {_format_command(command)}")
    if not args.dry_run:
        raise SystemExit(subprocess.run(command).returncode)


def command_benchmark(args: argparse.Namespace) -> None:
    arguments = args.arguments[1:] if args.arguments[:1] == ["--"] else args.arguments
    command = [
        sys.executable,
        REPO_ROOT / "benchmarks" / "openvmm.py",
        "--openvmm-dir",
        OPENVMM_DIR,
        "--nvx-dir",
        REPO_ROOT,
        *arguments,
    ]
    _run(command)


def _copy_release_file(source: Path, destination: Path) -> None:
    _require_file(source, source.name)
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination)


def _verify_checksums(directory: Path) -> None:
    checksum_file = _require_file(directory / "SHA256SUMS", "source checksums")
    for line in checksum_file.read_text(encoding="ascii").splitlines():
        expected, separator, relative = line.partition("  ")
        if not separator:
            raise ScriptError(f"malformed checksum line in {checksum_file}: {line}")
        path = (directory / relative).resolve()
        if directory.resolve() not in path.parents or not path.is_file():
            raise ScriptError(f"invalid checksum path in {checksum_file}: {relative}")
        actual = _sha256(path)
        if actual != expected:
            raise ScriptError(
                f"source checksum mismatch for {relative}: {actual}, expected {expected}"
            )


def _validate_alpine_sources(package_manifests: list[Path]) -> None:
    source_root = SOURCE_DIR / "alpine"
    source_manifest_path = _require_file(
        source_root / "manifest.json",
        "collected Alpine source manifest",
    )
    source_manifest = json.loads(source_manifest_path.read_text(encoding="utf-8"))
    collected = {
        (package["package"], package["version"], package["commit"]): package
        for package in source_manifest["packages"]
    }
    missing = []
    for manifest_path in package_manifests:
        package_manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        for package in package_manifest["packages"]:
            key = (
                package["name"],
                package["version"],
                package["aports_commit"],
            )
            if key not in collected:
                missing.append(f"{key[0]}-{key[1]} ({key[2]})")
    if missing:
        raise ScriptError(
            "collected Alpine sources do not cover packaged APKs: "
            + ", ".join(sorted(missing))
        )
    for package in collected.values():
        for field in ("recipe", "source_directory"):
            path = source_root / package[field]
            if not path.exists():
                raise ScriptError(
                    f"collected Alpine {field} is missing for "
                    f"{package['package']}: {path}"
                )
    _verify_checksums(source_root)


def _source_filter(member: tarfile.TarInfo) -> tarfile.TarInfo | None:
    path = PurePosixPath(member.name)
    if "__pycache__" in path.parts or path.suffix in (".pyc", ".pyo"):
        return None
    member.uid = 0
    member.gid = 0
    member.uname = ""
    member.gname = ""
    member.mtime = 0
    return member


def _create_source_archive(
    output: Path,
    inputs: list[tuple[Path, str]],
) -> None:
    missing = [str(path) for path, _ in inputs if not path.exists()]
    if missing:
        raise ScriptError(
            "cannot create source archive; missing: "
            + ", ".join(missing)
        )
    print(f">> creating source archive {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as archive:
                for source, arcname in inputs:
                    archive.add(
                        source,
                        arcname=arcname,
                        recursive=True,
                        filter=_source_filter,
                    )


def _project_source_archive(
    output: Path,
    version: str,
    package_manifests: list[Path],
) -> None:
    root = f"nvx-project-source-{version}"
    inputs = [
        (REPO_ROOT / relative, f"{root}/{relative}")
        for relative in PROJECT_SOURCE_PATHS
    ]
    inputs.extend(
        (
            manifest,
            f"{root}/build/{manifest.name}",
        )
        for manifest in package_manifests
    )
    inputs.append((_artifact("vmlinux.config"), f"{root}/build/vmlinux.config"))
    _create_source_archive(output, inputs)


def _alpine_source_archive(
    output: Path,
    version: str,
    package_manifests: list[Path],
) -> None:
    root = f"nvx-alpine-source-{version}"
    inputs = [(SOURCE_DIR / "alpine", f"{root}/sources")]
    inputs.extend(
        (
            manifest,
            f"{root}/manifests/{manifest.name}",
        )
        for manifest in package_manifests
    )
    _create_source_archive(output, inputs)


def _validate_linux_source_archive(path: Path) -> None:
    expected_members = {
        "vmlinux.config": _artifact("vmlinux.config").read_bytes(),
        "SOURCE-MANIFEST.json": (REPO_ROOT / "SOURCE-MANIFEST.json").read_bytes(),
        "kernel/patches/0001-microvm-xe9-earlycon.patch": (
            REPO_ROOT / "kernel" / "patches" / "0001-microvm-xe9-earlycon.patch"
        ).read_bytes(),
        "kernel/patches/0002-microvm-hvc-xe9.patch": (
            REPO_ROOT / "kernel" / "patches" / "0002-microvm-hvc-xe9.patch"
        ).read_bytes(),
    }
    with tarfile.open(path, "r:gz") as archive:
        members = archive.getmembers()
        names = [member.name for member in members]
        if not any(
            name.endswith(
                f"linux-{DEFAULT_KERNEL_VERSION}/drivers/tty/hvc/hvc_xe9.c"
            )
            for name in names
        ):
            raise ScriptError(f"{path} does not contain the patched xe9 HVC driver")
        for suffix, expected in expected_members.items():
            matches = [member for member in members if member.name.endswith(suffix)]
            if len(matches) != 1:
                raise ScriptError(
                    f"{path} contains {len(matches)} members ending in {suffix}"
                )
            extracted = archive.extractfile(matches[0])
            if extracted is None or extracted.read() != expected:
                raise ScriptError(f"{path} has stale contents for {suffix}")


def _guest_release_inputs() -> tuple[list[str], list[Path]]:
    required_guest_names = (
        "vmlinux",
        "vmlinux.config",
        "initramfs.cpio.gz",
        "initramfs.cpio.gz.packages.json",
    )
    for name in required_guest_names:
        _require_file(_artifact(name), f"required guest artifact {name}")
    guest_names = list(required_guest_names)
    package_manifests = [_artifact("initramfs.cpio.gz.packages.json")]
    for artifact_name in (
        "initramfs-python.cpio.gz",
        "initramfs-python-agent.cpio.gz",
    ):
        manifest_name = f"{artifact_name}.packages.json"
        artifact_exists = _artifact(artifact_name).is_file()
        manifest_exists = _artifact(manifest_name).is_file()
        if artifact_exists != manifest_exists:
            raise ScriptError(
                f"{artifact_name} and {manifest_name} must be packaged together"
            )
        if artifact_exists:
            guest_names.extend((artifact_name, manifest_name))
            package_manifests.append(_artifact(manifest_name))
    profiling_names = ("vmlinux-profiling", "vmlinux-profiling.config")
    profiling_exists = tuple(_artifact(name).is_file() for name in profiling_names)
    if any(profiling_exists) and not all(profiling_exists):
        raise ScriptError("profiling kernel and generated config must be packaged together")
    if all(profiling_exists):
        guest_names.extend(profiling_names)
    return guest_names, package_manifests


def command_collect_sources(_: argparse.Namespace) -> None:
    _, package_manifests = _guest_release_inputs()
    collector = [
        sys.executable,
        REPO_ROOT / "scripts" / "collect_alpine_sources.py",
        *package_manifests,
        "--output",
        SOURCE_DIR / "alpine",
    ]
    _run(collector)
    build_docker_linux_source(
        DockerBuildConfig(destination=SOURCE_DIR / "linux")
    )
    print(f">> collected release sources under {SOURCE_DIR}")


def command_package(args: argparse.Namespace) -> None:
    guest_names, package_manifests = _guest_release_inputs()
    linux_source_archive = (
        SOURCE_DIR
        / "linux"
        / f"nvx-linux-source-{DEFAULT_KERNEL_VERSION}.tar.gz"
    )
    if args.include_source:
        _validate_alpine_sources(package_manifests)
        _require_file(linux_source_archive, "Linux corresponding-source archive")
        _validate_linux_source_archive(linux_source_archive)
    else:
        print(
            "!! binary-only package: publish matching Linux and Alpine "
            "corresponding source separately",
            file=sys.stderr,
        )

    version = args.version or (REPO_ROOT / "VERSION").read_text(encoding="ascii").strip()
    destination = (args.destination or REPO_ROOT / "dist" / version).resolve()
    if destination.exists():
        if not args.force:
            raise ScriptError(
                f"release directory already exists: {destination}; pass --force to replace it"
            )
        release_root = (REPO_ROOT / "dist").resolve()
        if destination == release_root or release_root not in destination.parents:
            raise ScriptError("--force may only replace a version directory below dist/")
        shutil.rmtree(destination)
    binary_name = "openvmm.exe" if os.name == "nt" else "openvmm"
    _copy_release_file(_openvmm_binary(), destination / "bin" / binary_name)
    for name in guest_names:
        _copy_release_file(_artifact(name), destination / "guest" / name)
    for name in (
        "LICENSE",
        "LICENSE-NVX",
        "README.md",
        "SOURCE-MANIFEST.json",
        "THIRD_PARTY_NOTICES.md",
    ):
        _copy_release_file(REPO_ROOT / name, destination / name)
    _copy_release_file(
        OPENVMM_DIR / "LICENSE",
        destination / "licenses" / "LICENSE-OPENVMM",
    )
    _copy_release_file(
        REPO_ROOT / "licenses" / "COPYING-LINUX",
        destination / "licenses" / "COPYING-LINUX",
    )
    if args.include_source:
        source_destination = destination / "source"
        _copy_release_file(
            linux_source_archive,
            source_destination / linux_source_archive.name,
        )
        _project_source_archive(
            source_destination / f"nvx-project-source-{version}.tar.gz",
            version,
            package_manifests,
        )
        _alpine_source_archive(
            source_destination / f"nvx-alpine-source-{version}.tar.gz",
            version,
            package_manifests,
        )
    _write_checksums(destination)
    print(f">> packaged {destination}")


def command_verify(_: argparse.Namespace) -> None:
    manifest = json.loads(
        (REPO_ROOT / "SOURCE-MANIFEST.json").read_text(encoding="utf-8")
    )
    required = {
        REPO_ROOT / "kernel" / "config-microvm": "kernel build configuration",
        REPO_ROOT
        / "kernel"
        / "patches"
        / "0001-microvm-xe9-earlycon.patch": "xe9 early console patch",
        REPO_ROOT
        / "kernel"
        / "patches"
        / "0002-microvm-hvc-xe9.patch": "xe9 HVC patch",
        REPO_ROOT / "licenses" / "COPYING-LINUX": "Linux copyright notice",
        OPENVMM_DIR / "Cargo.toml": "initialized OpenVMM submodule",
    }
    for path, description in required.items():
        _require_file(path, description)
    forbidden = (
        REPO_ROOT / "third_party" / "linux",
        REPO_ROOT / "third_party" / "alpine-sources",
    )
    present = [str(path) for path in forbidden if path.exists()]
    if present:
        raise ScriptError(
            "generated third-party sources must not be checked out here: "
            + ", ".join(present)
        )
    linux_manifest = manifest["linux"]
    expected_linux = {
        "version": DEFAULT_KERNEL_VERSION,
        "upstream_url": DEFAULT_KERNEL_URL,
        "upstream_archive_sha256": DEFAULT_KERNEL_SHA256,
    }
    for field, expected in expected_linux.items():
        if linux_manifest.get(field) != expected:
            raise ScriptError(
                f"SOURCE-MANIFEST.json Linux {field} does not match the build pin"
            )
    alpine_manifest = manifest["alpine"]
    expected_alpine = {
        "version": DEFAULT_ALPINE_VERSION,
        "branch": DEFAULT_ALPINE_BRANCH,
        "minirootfs_sha256": DEFAULT_ALPINE_MINIROOTFS_SHA256,
    }
    for field, expected in expected_alpine.items():
        if alpine_manifest.get(field) != expected:
            raise ScriptError(
                f"SOURCE-MANIFEST.json Alpine {field} does not match the build pin"
            )
    config_path = REPO_ROOT / "kernel" / "config-microvm"
    config = config_path.read_text(encoding="utf-8")
    for setting in (
        "CONFIG_PVH=y",
        "CONFIG_HVC_XE9=y",
        "CONFIG_VIRTIO_FS=y",
        "CONFIG_FUSE_FS=y",
    ):
        if setting not in config.splitlines():
            raise ScriptError(f"{config_path} is missing {setting}")
    hvc_patch_text = (
        REPO_ROOT / "kernel" / "patches" / "0002-microvm-hvc-xe9.patch"
    ).read_text(encoding="utf-8")
    for marker in ("config HVC_XE9", "hvc_xe9.o", "hvc_xe9.c"):
        if marker not in hvc_patch_text:
            raise ScriptError(f"xe9 HVC patch is missing {marker}")
    generated_config = _artifact("vmlinux.config")
    if generated_config.is_file():
        generated = generated_config.read_text(encoding="utf-8").splitlines()
        for setting in ("CONFIG_PVH=y", "CONFIG_HVC_XE9=y"):
            if setting not in generated:
                raise ScriptError(f"{generated_config} is missing {setting}")
    head = subprocess.run(
        ["git", "-C", OPENVMM_DIR, "rev-parse", "HEAD"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    expected = manifest["openvmm"]["commit"]
    if head != expected:
        raise ScriptError(f"OpenVMM submodule is at {head}, expected {expected}")
    print(">> source tree and submodule metadata are consistent")


def _add_guest_options(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--native",
        action="store_true",
        help="build directly on Linux instead of using Docker",
    )
    parser.add_argument(
        "--profiling",
        action="store_true",
        help="build vmlinux-profiling with frame pointers",
    )
    parser.add_argument(
        "--python",
        choices=("none", "full", "agent", "all"),
        default="none",
        help="also build a Python initramfs profile",
    )


def parse_args() -> argparse.Namespace:
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
    kernel.add_argument("--profiling", action="store_true")
    kernel.set_defaults(handler=command_build_kernel)

    initramfs = subparsers.add_parser(
        "build-initramfs",
        help="build an Alpine initramfs natively on Linux",
    )
    initramfs.add_argument(
        "--profile",
        choices=("base", "full", "agent"),
        default="base",
    )
    initramfs.set_defaults(handler=command_build_initramfs)

    openvmm = subparsers.add_parser("build-openvmm", help="build OpenVMM")
    openvmm.add_argument("--skip-restore", action="store_true")
    openvmm.set_defaults(handler=command_build_openvmm)

    build = subparsers.add_parser("build", help="build guest artifacts and OpenVMM")
    _add_guest_options(build)
    build.add_argument("--skip-restore", action="store_true")
    build.set_defaults(handler=command_build)

    run = subparsers.add_parser("run", help="run an OpenVMM microVM")
    run.add_argument("--hypervisor", choices=("auto", "whp", "kvm"), default="auto")
    run.add_argument("--memory-mib", type=int, default=128)
    run.add_argument(
        "--initrd",
        choices=("base", "python", "python-agent"),
        default="base",
    )
    run.add_argument("--mount", help="GUEST_TARGET,HOST_PATH,ro|rw")
    run.add_argument("--net", metavar="IPV4/PREFIX")
    run.add_argument("--cmdline", default="")
    run.add_argument("--dry-run", action="store_true")
    run.set_defaults(handler=command_run)

    benchmark = subparsers.add_parser(
        "benchmark",
        help="run the OpenVMM-native benchmark coordinator",
    )
    benchmark.add_argument("arguments", nargs=argparse.REMAINDER)
    benchmark.set_defaults(handler=command_benchmark)

    sources = subparsers.add_parser(
        "collect-sources",
        help="materialize verified Linux and Alpine release-source artifacts",
    )
    sources.set_defaults(handler=command_collect_sources)

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
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        args.handler(args)
    except (ScriptError, FileNotFoundError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
