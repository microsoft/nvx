"""Linux-native and Docker-backed artifact build workflows."""

from __future__ import annotations

import json
import os
import re
import shutil
import ssl
import struct
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import TypedDict

from .common import (
    REPO_ROOT,
    ScriptError,
    download,
    format_size,
    require_file,
    require_success,
    require_tool,
    run_capture,
    run_checked,
    sha256_file,
)

DEFAULT_KERNEL_VERSION = "6.18.38"
DEFAULT_KERNEL_URL = "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.18.38.tar.xz"
DEFAULT_KERNEL_SHA256 = (
    "ac26e508abd56e9f8b89872b6e10c49fc823bcc70d8068a5d8504c1a7c4ff045"
)
DEFAULT_ALPINE_VERSION = "3.24.1"
DEFAULT_ALPINE_BRANCH = "v3.24"
DEFAULT_ALPINE_MINIROOTFS_SHA256 = (
    "41f73e3cf5fa919b8aa5ca6b30dc48f0da2720776d7423e2a7748211456fe081"
)
REQUIRED_VIRTIO_CONSOLE_CONFIG = (
    "CONFIG_HVC_DRIVER=y",
    "CONFIG_VIRTIO=y",
    "CONFIG_VIRTIO_CONSOLE=y",
    "CONFIG_VIRTIO_MMIO=y",
    "CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y",
)
REQUIRED_SHARED_STATUS_KERNEL_CONFIG = ("CONFIG_VIRTIO_MMIO_SHARED_STATUS=y",)
REQUIRED_SANDBOX_KERNEL_CONFIG = (
    "CONFIG_BPF_SYSCALL=y",
    "CONFIG_CGROUP_BPF=y",
    "CONFIG_EROFS_FS=y",
    "CONFIG_EROFS_FS_ZIP=y",
    "CONFIG_EROFS_FS_ZIP_ZSTD=y",
    "CONFIG_EXT4_FS=y",
    "CONFIG_MEMCG=y",
    "CONFIG_OVERLAY_FS=y",
    "# CONFIG_OVERLAY_FS_REDIRECT_ALWAYS_FOLLOW is not set",
    "CONFIG_SECCOMP_FILTER=y",
    "CONFIG_VIRTIO_BLK=y",
)
GUEST_AGENT_ARTIFACT_NAME = "nvx-agent"
GUEST_AGENT_SHA256_NAME = f"{GUEST_AGENT_ARTIFACT_NAME}.sha256"
GUEST_AGENT_TARGET = "x86_64-unknown-linux-musl"
OPENVMM_PROVENANCE_NAME = "openvmm.provenance.json"


class ApkPackage(TypedDict):
    name: str
    version: str | None
    architecture: str | None
    license: str | None
    origin: str | None
    url: str | None
    description: str | None
    aports_commit: str | None
    build_time: str | None


def _assert_virtio_console_kernel_config(path: Path) -> None:
    configured = set(path.read_text(encoding="utf-8").splitlines())
    missing = [
        setting
        for setting in REQUIRED_VIRTIO_CONSOLE_CONFIG
        if setting not in configured
    ]
    if missing:
        raise ScriptError(
            "kernel configuration cannot provide /dev/hvc1: " + ", ".join(missing)
        )


def _assert_sandbox_kernel_config(path: Path) -> None:
    configured = set(path.read_text(encoding="utf-8").splitlines())
    missing = [
        setting
        for setting in REQUIRED_SANDBOX_KERNEL_CONFIG
        if setting not in configured
    ]
    if missing:
        raise ScriptError(
            "kernel configuration cannot run sandbox filesystems: " + ", ".join(missing)
        )


def _assert_shared_status_kernel_config(path: Path) -> None:
    configured = set(path.read_text(encoding="utf-8").splitlines())
    missing = [
        setting
        for setting in REQUIRED_SHARED_STATUS_KERNEL_CONFIG
        if setting not in configured
    ]
    if missing:
        raise ScriptError(
            "kernel configuration cannot consume shared virtio interrupt status: "
            + ", ".join(missing)
        )


def validate_static_x86_64_elf(path: Path) -> None:
    data = path.read_bytes()
    if len(data) < 64 or data[:6] != b"\x7fELF\x02\x01":
        raise ScriptError("NVX guest agent must be a little-endian ELF64 binary")
    if struct.unpack_from("<H", data, 16)[0] not in (2, 3):
        raise ScriptError("NVX guest agent must be an ELF executable")
    if struct.unpack_from("<H", data, 18)[0] != 62:
        raise ScriptError("NVX guest agent must target x86-64")
    program_offset = struct.unpack_from("<Q", data, 32)[0]
    program_entry_size = struct.unpack_from("<H", data, 54)[0]
    program_count = struct.unpack_from("<H", data, 56)[0]
    if program_entry_size != 56 or program_count == 0:
        raise ScriptError("NVX guest agent has an invalid ELF program header")
    program_bytes = program_entry_size * program_count
    if program_offset > len(data) or program_bytes > len(data) - program_offset:
        raise ScriptError("NVX guest agent has truncated ELF program headers")
    has_load_segment = False
    entry_point = struct.unpack_from("<Q", data, 24)[0]
    entry_is_executable = False
    for index in range(program_count):
        offset = program_offset + index * program_entry_size
        segment_type = struct.unpack_from("<I", data, offset)[0]
        flags = struct.unpack_from("<I", data, offset + 4)[0]
        file_offset = struct.unpack_from("<Q", data, offset + 8)[0]
        virtual_address = struct.unpack_from("<Q", data, offset + 16)[0]
        file_size = struct.unpack_from("<Q", data, offset + 32)[0]
        memory_size = struct.unpack_from("<Q", data, offset + 40)[0]
        if file_size > memory_size:
            raise ScriptError("NVX guest agent has an invalid ELF segment size")
        if file_offset > len(data) or file_size > len(data) - file_offset:
            raise ScriptError("NVX guest agent has an ELF segment beyond end of file")
        has_load_segment |= segment_type == 1
        if (
            segment_type == 1
            and flags & 1
            and virtual_address <= entry_point < virtual_address + memory_size
        ):
            entry_is_executable = True
        if segment_type == 3:
            raise ScriptError("NVX guest agent must be statically linked")
        if segment_type == 2:
            if file_size % 16:
                raise ScriptError("NVX guest agent has an invalid dynamic section")
            dynamic_end = file_offset + file_size
            dynamic_offset = file_offset
            terminated = False
            while dynamic_offset + 16 <= dynamic_end:
                dynamic_tag = struct.unpack_from("<q", data, dynamic_offset)[0]
                if dynamic_tag == 0:
                    terminated = True
                    break
                if dynamic_tag == 1:
                    raise ScriptError(
                        "NVX guest agent must not have dynamic dependencies"
                    )
                dynamic_offset += 16
            if not terminated:
                raise ScriptError("NVX guest agent has an invalid dynamic section")
    if not has_load_segment:
        raise ScriptError("NVX guest agent has no loadable ELF segment")
    if not entry_is_executable:
        raise ScriptError("NVX guest agent entry point is not executable")


def stage_guest_agent(source: Path, expected_sha256: str) -> Path:
    """Stage a pinned guest-agent binary without enabling it in the initramfs."""
    source = source.expanduser().resolve()
    if not source.is_file():
        raise ScriptError(f"NVX guest agent not found: {source}")
    expected_sha256 = expected_sha256.lower()
    if re.fullmatch(r"[0-9a-f]{64}", expected_sha256) is None:
        raise ScriptError("NVX guest agent SHA-256 must contain 64 hexadecimal digits")
    destination = REPO_ROOT / "build" / GUEST_AGENT_ARTIFACT_NAME
    pin = REPO_ROOT / "build" / GUEST_AGENT_SHA256_NAME
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary_path: Path | None = None
    try:
        with (
            source.open("rb") as input_file,
            tempfile.NamedTemporaryFile(
                dir=destination.parent,
                prefix=f"{destination.name}.",
                suffix=".part",
                delete=False,
            ) as output_file,
        ):
            temporary_path = Path(output_file.name)
            shutil.copyfileobj(input_file, output_file)
        actual_sha256 = sha256_file(temporary_path)
        if actual_sha256 != expected_sha256:
            raise ScriptError(
                f"NVX guest agent SHA-256 is {actual_sha256}, expected {expected_sha256}"
            )
        validate_static_x86_64_elf(temporary_path)
        temporary_path.chmod(0o755)
        temporary_path.replace(destination)
        temporary_path = None
        pin.write_text(f"{expected_sha256}\n", encoding="ascii")
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)
    print(f">> staged {destination} for {GUEST_AGENT_TARGET}")
    return destination


def record_openvmm_provenance(executable: Path) -> None:
    """Bind an OpenVMM executable to the exact clean submodule revision."""
    openvmm_dir = REPO_ROOT / "openvmm"
    head = run_capture(["git", "-C", openvmm_dir, "rev-parse", "HEAD"])
    require_success(head, "OpenVMM revision query")
    gitlink = run_capture(["git", "-C", REPO_ROOT, "rev-parse", ":openvmm"])
    require_success(gitlink, "OpenVMM gitlink query")
    status = run_capture(["git", "-C", openvmm_dir, "status", "--porcelain"])
    require_success(status, "OpenVMM status query")
    source_revision = head.stdout.decode("ascii").strip()
    expected_revision = gitlink.stdout.decode("ascii").strip()
    if source_revision != expected_revision:
        raise ScriptError(
            f"OpenVMM submodule is at {source_revision}, expected {expected_revision}"
        )
    require_file(executable, "OpenVMM release binary")
    provenance = {
        "format": 1,
        "source_revision": source_revision,
        "source_clean": not status.stdout.strip(),
        "executable_sha256": sha256_file(executable),
        "origin": "local-build",
    }
    path = REPO_ROOT / "build" / OPENVMM_PROVENANCE_NAME
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(provenance, indent=2) + "\n", encoding="utf-8")


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


def _require_linux(workflow: str) -> None:
    if sys.platform != "linux":
        raise ScriptError(
            f"{workflow} requires Linux; use build-guest without --native for Docker"
        )


def _alpine_tarball(config: AlpineBuildConfig) -> Path:
    return config.work / f"alpine-minirootfs-{config.version}-x86_64.tar.gz"


def _download_verified(url: str, destination: Path, expected_sha256: str) -> None:
    if destination.is_file():
        actual_sha256 = sha256_file(destination)
        if actual_sha256 == expected_sha256:
            return
        print(
            f">> discarding {destination.name}: SHA-256 is {actual_sha256}, "
            f"expected {expected_sha256}"
        )
        destination.unlink()
    print(f">> downloading {destination.name}")
    download(url, destination, expected_sha256=expected_sha256)


def _cache_root() -> Path:
    configured = os.environ.get("NVX_CACHE_DIR")
    return (
        Path(configured).expanduser().resolve()
        if configured
        else (REPO_ROOT / ".cache").resolve()
    )


def _kernel_patch_files() -> tuple[Path, ...]:
    patches = tuple(sorted((REPO_ROOT / "kernel" / "patches").glob("*.patch")))
    if not patches:
        raise ScriptError("no kernel patches were found")
    return patches


def _kernel_source_fingerprint() -> str:
    return json.dumps(
        {
            "archive_sha256": DEFAULT_KERNEL_SHA256,
            "patches": [
                {"name": patch.name, "sha256": sha256_file(patch)}
                for patch in _kernel_patch_files()
            ],
        },
        sort_keys=True,
    )


def prepare_kernel_source(version: str = DEFAULT_KERNEL_VERSION) -> tuple[Path, str]:
    """Download, verify, extract, and patch the pinned Linux source."""
    if version != DEFAULT_KERNEL_VERSION:
        raise ScriptError(
            f"this source tree pins Linux {DEFAULT_KERNEL_VERSION}; requested {version}"
        )
    for tool in ("patch", "tar"):
        require_tool(tool)
    cache = _cache_root()
    downloads = cache / "downloads"
    source_parent = cache / "linux"
    tarball = downloads / f"linux-{version}.tar.xz"
    source = source_parent / f"linux-{version}"
    stamp = source_parent / f"linux-{version}.nvx-source.json"
    fingerprint = _kernel_source_fingerprint()

    downloads.mkdir(parents=True, exist_ok=True)
    source_parent.mkdir(parents=True, exist_ok=True)
    _download_verified(DEFAULT_KERNEL_URL, tarball, DEFAULT_KERNEL_SHA256)

    cached_fingerprint = stamp.read_text(encoding="utf-8") if stamp.is_file() else None
    if source.is_dir() and cached_fingerprint != fingerprint:
        shutil.rmtree(source)
    if not source.is_dir():
        stamp.unlink(missing_ok=True)
        print(f">> extracting and patching Linux {version}")
        run_checked(["tar", "-xf", tarball, "-C", source_parent])
        if not (source / "Makefile").is_file():
            raise ScriptError(f"Linux archive did not produce {source}")
        for patch in _kernel_patch_files():
            print(f">> applying {patch.name}")
            run_checked(
                ["patch", "--batch", "--forward", "-p1", "-i", patch],
                cwd=source,
            )
        stamp.write_text(fingerprint, encoding="utf-8")
    return source, fingerprint


def _prepare_alpine_root(config: AlpineBuildConfig) -> Path:
    if config.version != DEFAULT_ALPINE_VERSION:
        raise ScriptError(
            "this source tree pins Alpine "
            f"{DEFAULT_ALPINE_VERSION}; requested {config.version}"
        )
    config.work.mkdir(parents=True, exist_ok=True)
    tarball = _alpine_tarball(config)
    _download_verified(
        "https://dl-cdn.alpinelinux.org/alpine/"
        f"{config.branch}/releases/x86_64/{tarball.name}",
        tarball,
        DEFAULT_ALPINE_MINIROOTFS_SHA256,
    )
    root = config.work / "root"
    shutil.rmtree(root, ignore_errors=True)
    root.mkdir(parents=True)
    require_tool("tar")
    run_checked(["tar", "-xzf", tarball, "-C", root])
    return root


def _install(source: Path, destination: Path) -> None:
    destination.write_bytes(source.read_bytes().replace(b"\r\n", b"\n"))
    destination.chmod(0o755)


def _build_static_helper(work: Path, source: Path, destination: Path) -> None:
    compiler = require_tool("cc")
    output = work / source.stem
    run_checked(
        [
            compiler,
            "-static",
            "-Os",
            "-s",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-o",
            output,
            source,
        ]
    )
    shutil.copyfile(output, destination)
    destination.chmod(0o755)


def _build_device_io_helper(work: Path, destination: Path) -> dict[str, str]:
    compiler = require_tool("cc")
    source = REPO_ROOT / "alpine" / "nvx-device-io.c"
    output = work / "nvx-device-io"
    run_checked(
        [
            compiler,
            "-nostdlib",
            "-static",
            "-Os",
            "-fno-builtin",
            "-fno-pie",
            "-fno-stack-protector",
            "-no-pie",
            "-Wl,--build-id=none",
            "-Wl,-z,noexecstack",
            "-s",
            "-o",
            output,
            source,
        ]
    )
    shutil.copyfile(output, destination)
    destination.chmod(0o755)
    return {
        "source_sha256": sha256_file(source),
        "binary_sha256": sha256_file(output),
    }


def _apk_add(root: Path, *packages: str) -> None:
    loader = root / "lib" / "ld-musl-x86_64.so.1"
    environment = os.environ.copy()
    environment["LD_LIBRARY_PATH"] = f"{root / 'lib'}:{root / 'usr' / 'lib'}"
    host_ca_file = ssl.get_default_verify_paths().cafile
    if host_ca_file:
        environment.setdefault("SSL_CERT_FILE", host_ca_file)
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


def _normalize_initramfs_metadata(root: Path) -> None:
    (root / "var" / "log" / "apk.log").unlink(missing_ok=True)
    for path in (*root.rglob("*"), root):
        try:
            os.utime(path, (0, 0), follow_symlinks=False)
        except (NotImplementedError, OSError) as error:
            if not path.is_symlink():
                raise ScriptError(
                    f"failed to normalize initramfs timestamp for {path}: {error}"
                ) from error


def _pack_initramfs(root: Path, output: Path) -> None:
    require_tool("find")
    require_tool("sort")
    require_tool("cpio")
    require_tool("gzip")
    _normalize_initramfs_metadata(root)
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("wb") as archive:
        finder = subprocess.Popen(
            ["find", ".", "-print0"], cwd=root, stdout=subprocess.PIPE
        )
        assert finder.stdout is not None
        sort_environment = os.environ.copy()
        sort_environment["LC_ALL"] = "C"
        sorter = subprocess.Popen(
            ["sort", "-z"],
            cwd=root,
            stdin=finder.stdout,
            stdout=subprocess.PIPE,
            env=sort_environment,
        )
        finder.stdout.close()
        assert sorter.stdout is not None
        cpio = subprocess.Popen(
            [
                "cpio",
                "--null",
                "--quiet",
                "--reproducible",
                "--owner=0:0",
                "-o",
                "-H",
                "newc",
            ],
            cwd=root,
            stdin=sorter.stdout,
            stdout=subprocess.PIPE,
        )
        sorter.stdout.close()
        assert cpio.stdout is not None
        gzip = subprocess.Popen(["gzip", "-n", "-9"], stdin=cpio.stdout, stdout=archive)
        cpio.stdout.close()
        gzip_code = gzip.wait()
        cpio_code = cpio.wait()
        sorter_code = sorter.wait()
        finder_code = finder.wait()
    if finder_code or sorter_code or cpio_code or gzip_code:
        output.unlink(missing_ok=True)
        raise ScriptError(
            "failed to pack initramfs "
            f"(find={finder_code}, sort={sorter_code}, cpio={cpio_code}, "
            f"gzip={gzip_code})"
        )


def _write_apk_manifest(
    root: Path,
    output: Path,
    config: AlpineBuildConfig,
    helpers: dict[str, dict[str, str]],
) -> None:
    installed = root / "lib" / "apk" / "db" / "installed"
    packages: list[ApkPackage] = []
    for record in installed.read_text(encoding="utf-8").split("\n\n"):
        fields: dict[str, str] = {}
        for line in record.splitlines():
            if len(line) >= 2 and line[1] == ":":
                fields[line[0]] = line[2:]
        if "P" not in fields:
            continue
        packages.append(
            {
                "name": fields["P"],
                "version": fields.get("V"),
                "architecture": fields.get("A"),
                "license": fields.get("L"),
                "origin": fields.get("o"),
                "url": fields.get("U"),
                "description": fields.get("T"),
                "aports_commit": fields.get("c"),
                "build_time": fields.get("t"),
            }
        )
    packages.sort(key=lambda package: package["name"])
    manifest = output.with_name(f"{output.name}.packages.json")
    manifest.write_text(
        json.dumps(
            {
                "format": 1,
                "alpine_version": config.version,
                "alpine_branch": config.branch,
                "architecture": "x86_64",
                "packages": packages,
                "helpers": helpers,
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )


def build_initramfs(config: AlpineBuildConfig) -> None:
    _require_linux("build-initramfs")
    root = _prepare_alpine_root(config)
    print(">> installing sandbox utilities into the rootfs")
    _apk_add(
        root,
        "blkid",
        "busybox-extras",
        "e2fsprogs",
        "util-linux",
        "util-linux-misc",
    )
    resolver = root / "etc" / "resolv.conf"
    resolver.unlink(missing_ok=True)
    resolver.touch()
    _install(REPO_ROOT / "alpine" / "init", root / "init")
    _install(REPO_ROOT / "alpine" / "nvx-exit", root / "sbin" / "nvx-exit")
    _install(
        REPO_ROOT / "alpine" / "nvx-hostmount",
        root / "sbin" / "nvx-hostmount",
    )
    _install(
        REPO_ROOT / "alpine" / "nvx-container-enter",
        root / "sbin" / "nvx-container-enter",
    )
    _install(
        REPO_ROOT / "alpine" / "nvx-container-launch",
        root / "sbin" / "nvx-container-launch",
    )
    _install(
        REPO_ROOT / "alpine" / "nvx-init-agent",
        root / "sbin" / "nvx-init-agent",
    )
    _install(
        REPO_ROOT / "alpine" / "nvx-identity-probe",
        root / "sbin" / "nvx-identity-probe",
    )
    _install(REPO_ROOT / "alpine" / "nvx-snapshot", root / "sbin" / "nvx-snapshot")
    _install(
        REPO_ROOT / "alpine" / "nvx-virtio-restore-probe",
        root / "sbin" / "nvx-virtio-restore-probe",
    )
    _build_static_helper(
        config.work,
        REPO_ROOT / "alpine" / "nvx-reseed.c",
        root / "sbin" / "nvx-reseed",
    )
    _build_static_helper(
        config.work,
        REPO_ROOT / "alpine" / "nvx-mmio-write.c",
        root / "sbin" / "nvx-mmio-write",
    )
    _build_static_helper(
        config.work,
        REPO_ROOT / "alpine" / "nvx-port-io.c",
        root / "sbin" / "nvx-port-io",
    )
    _build_static_helper(
        config.work,
        REPO_ROOT / "alpine" / "nvx-console-pending.c",
        root / "sbin" / "nvx-console-pending",
    )
    _build_static_helper(
        config.work,
        REPO_ROOT / "alpine" / "nvx-managed-agent.c",
        root / "sbin" / "nvx-managed-agent",
    )
    device_io = _build_device_io_helper(config.work, root / "sbin" / "nvx-device-io")
    config.output.parent.mkdir(parents=True, exist_ok=True)
    _write_apk_manifest(
        root,
        config.output,
        config,
        {"nvx-device-io": device_io},
    )
    _pack_initramfs(root, config.output)
    print(f">> built {config.output} ({format_size(config.output.stat().st_size)})")


def build_kernel(config: KernelBuildConfig) -> None:
    _require_linux("build-kernel")
    for tool in ("make", "readelf"):
        require_tool(tool)
    source, source_fingerprint = prepare_kernel_source(config.version)
    build_fingerprint = json.dumps(
        {
            "source": source_fingerprint,
        },
        sort_keys=True,
    )
    build_stamp = config.work / ".nvx-build.json"
    cached_build_fingerprint = (
        build_stamp.read_text(encoding="utf-8") if build_stamp.is_file() else None
    )
    if config.work.is_dir() and cached_build_fingerprint != build_fingerprint:
        shutil.rmtree(config.work)
    config.work.mkdir(parents=True, exist_ok=True)
    build_stamp.write_text(build_fingerprint, encoding="utf-8")
    kernel_config = config.work / ".config"
    shutil.copy2(REPO_ROOT / "kernel" / "config-microvm", kernel_config)
    make = ["make", "-C", source, f"O={config.work}"]
    run_checked([*make, "olddefconfig"])
    _assert_virtio_console_kernel_config(kernel_config)
    _assert_sandbox_kernel_config(kernel_config)
    _assert_shared_status_kernel_config(kernel_config)
    jobs = os.cpu_count() or 1
    print(f">> building vmlinux with {jobs} jobs")
    run_checked([*make, f"-j{jobs}", "vmlinux"])
    config.output.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(config.work / "vmlinux", config.output)
    shutil.copy2(kernel_config, config.output.with_name(f"{config.output.name}.config"))
    print(f">> built {config.output}")

    notes = run_capture(["readelf", "-n", config.output])
    if "Xen" in notes.text and "0x00000012" in notes.text:
        print(">> PVH entry note present")
    else:
        config.output.unlink(missing_ok=True)
        raise ScriptError("PVH entry note 0x12 is missing from the built vmlinux")


def docker_build_command(config: DockerBuildConfig, target: str) -> list[str | Path]:
    if (
        config.kernel_version != DEFAULT_KERNEL_VERSION
        or config.alpine_version != DEFAULT_ALPINE_VERSION
        or config.alpine_branch != DEFAULT_ALPINE_BRANCH
    ):
        raise ScriptError(
            "Docker builds are pinned to Linux "
            f"{DEFAULT_KERNEL_VERSION} and Alpine {DEFAULT_ALPINE_VERSION} "
            f"({DEFAULT_ALPINE_BRANCH})"
        )
    destination = _docker_destination(config.destination)
    command: list[str | Path] = [
        "docker",
        "build",
        "-f",
        REPO_ROOT / "docker" / "Dockerfile",
        "--target",
        target,
    ]
    command.extend(
        [
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


def build_docker_linux_source(config: DockerBuildConfig) -> Path:
    """Build and export the pinned Linux corresponding-source archive."""
    require_tool(
        "docker",
        "docker was not found on PATH; install Docker with the Linux engine first",
    )
    destination = _docker_destination(config.destination)
    print(f">> building Linux corresponding source into '{destination}'")
    run_checked(
        docker_build_command(config, "linux-source-artifacts"),
        cwd=REPO_ROOT,
    )
    archive = destination / f"nvx-linux-source-{DEFAULT_KERNEL_VERSION}.tar.gz"
    if not archive.is_file():
        raise ScriptError(f"Docker build did not produce {archive.name}")
    print(f">> built {archive} ({format_size(archive.stat().st_size)})")
    return archive


def build_docker_artifacts(
    config: DockerBuildConfig,
) -> None:
    require_tool(
        "docker",
        "docker was not found on PATH; install Docker with the Linux engine first",
    )
    destination = _docker_destination(config.destination)
    print(
        f">> building Linux artifacts into '{destination}' "
        f"(kernel {config.kernel_version}, Alpine {config.alpine_version})"
    )
    run_checked(docker_build_command(config, "artifacts"), cwd=REPO_ROOT)
    expected = ("vmlinux", "initramfs.cpio.gz")
    missing = [name for name in expected if not (destination / name).is_file()]
    if missing:
        raise ScriptError(f"Docker build did not produce: {', '.join(missing)}")
    print(">> done:")
    for name in expected:
        path = destination / name
        print(f"  {path} ({format_size(path.stat().st_size)})")
