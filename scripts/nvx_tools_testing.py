# pyright: reportPrivateUsage=false

"""Shared helpers for the host tooling's unit tests."""

import contextlib
import json
import os
import shutil
import struct
from collections.abc import Generator
from pathlib import Path
from unittest.mock import patch

from nvx_tools import azurelinux, build, common, nvx_cli, ubuntu
from nvx_tools.build_constants import (
    AlpineBuildConstants,
    AzureLinuxBuildConstants,
    BuildConstants,
    GuestRustBuildConstants,
    InitramfsBuildConstants,
    KernelBuildConstants,
    OpenVMMBuildConstants,
    ReleaseBuildConstants,
    UbuntuBuildConstants,
)


@contextlib.contextmanager
def without_nvx_variable() -> Generator[None]:
    with patch.dict(os.environ):
        os.environ.pop(nvx_cli.VARIABLE, None)
        yield


def elf_file(
    segments: tuple[tuple[int, bytes], ...] = ((1, b"code"),),
    *,
    elf_type: int = 2,
    elf_class: int = 2,
    stack_flags: int | None = None,
) -> bytes:
    """Returns a minimal ELF file with one program header for each segment, and
    a ``PT_GNU_STACK`` header with ``stack_flags`` unless it is ``None``."""
    headers = [(segment_type, 4, payload) for segment_type, payload in segments]
    if stack_flags is not None:
        headers.append((0x6474E551, stack_flags, b""))
    header_size, entry_size = 64, 56
    payload_offset = header_size + entry_size * len(headers)
    program_headers = b""
    payloads = b""
    for segment_type, flags, payload in headers:
        program_headers += struct.pack(
            "<IIQQQQQQ",
            segment_type,
            flags,
            payload_offset + len(payloads),
            0,
            0,
            len(payload),
            len(payload),
            8,
        )
        payloads += payload
    identity = b"\x7fELF" + bytes([elf_class, 1, 1]) + bytes(9)
    header = struct.pack(
        "<16sHHIQQQIHHHHHH",
        identity,
        elf_type,
        62,
        1,
        0,
        header_size,
        0,
        0,
        header_size,
        entry_size,
        len(headers),
        64,
        0,
        0,
    )
    return header + program_headers + payloads


def write_release_fixture(
    root: Path,
) -> tuple[dict[str, Path], dict[str, object], str]:
    build_dir = root / "build"
    source_dir = build_dir / "sources"
    openvmm_dir = root / "openvmm"
    binary_name = "openvmm.exe" if os.name == "nt" else "openvmm"
    binary = openvmm_dir / "target" / "release" / binary_name
    revision = "0bc357bbcf3a654b63dfb51f1103c5751bf3d31f"
    guest_names = ReleaseBuildConstants.GUEST_ARTIFACT_NAMES
    # The guest artifacts' inputs, and the sources of the nvx executable that
    # the release ships, whose host side no guest artifact reads.
    source_files = (
        set(build._initramfs_source_files())
        | set(ubuntu.customization_files())
        | set(azurelinux.input_files())
        | set(nvx_cli.source_files())
    )
    for source in source_files:
        destination = root / source.relative_to(BuildConstants.REPO_ROOT)
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, destination)
    for path in (
        binary,
        *(build_dir / name for name in guest_names),
        root / "LICENSE",
        root / "README.md",
        root / "THIRD_PARTY_NOTICES.md",
        openvmm_dir / "LICENSE",
        root / "kernel" / "COPYING-LINUX",
    ):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(path.name.encode("ascii"))
    # The release build of nvx, static as the Linux packages require, and
    # its provenance.
    nvx_binary = root / "nvx" / "target" / "release" / nvx_cli.binary_path().name
    nvx_binary.parent.mkdir(parents=True, exist_ok=True)
    nvx_binary.write_bytes(elf_file())
    (build_dir / nvx_cli.PROVENANCE_NAME).write_text(
        json.dumps(nvx_cli.provenance_record(nvx_binary, nvx_cli.release_target())),
        encoding="utf-8",
    )
    for artifact_name, manifest_name in (
        (
            "initramfs-ubuntu.cpio.gz",
            "initramfs-ubuntu.cpio.gz.packages.json",
        ),
        ("ubuntu-distro.erofs", "ubuntu-distro.erofs.manifest.json"),
    ):
        artifact = build_dir / artifact_name
        input_sha256 = ubuntu.converter_input_sha256(ubuntu.customization_files())
        (build_dir / manifest_name).write_text(
            json.dumps(
                {
                    "format": 1,
                    "guest": "ubuntu",
                    "artifact": artifact.name,
                    "artifact_sha256": common.sha256_file(artifact),
                    "input_sha256": input_sha256,
                    "packages": [],
                }
            ),
            encoding="utf-8",
        )
    (build_dir / AzureLinuxBuildConstants.PACKAGE_MANIFEST_NAME).write_text(
        json.dumps(
            {
                "format": AzureLinuxBuildConstants.PACKAGE_MANIFEST_VERSION,
                "guest": AzureLinuxBuildConstants.GUEST_NAME,
                "artifact": AzureLinuxBuildConstants.INITRAMFS_NAME,
                "artifact_sha256": common.sha256_file(
                    build_dir / AzureLinuxBuildConstants.INITRAMFS_NAME
                ),
                "package_manifest_format": (
                    AzureLinuxBuildConstants.PACKAGE_MANIFEST_FORMAT
                ),
                "image": AzureLinuxBuildConstants.IMAGE,
                "input_sha256": azurelinux.input_sha256(),
            }
        ),
        encoding="utf-8",
    )
    generated_config = build_dir / "vmlinux.config"
    generated_config.write_text(
        "\n".join(
            (
                *KernelBuildConstants.REQUIRED_DIRECT_BOOT_CONFIG,
                *KernelBuildConstants.REQUIRED_VIRTIO_CONSOLE_CONFIG,
                *KernelBuildConstants.REQUIRED_SHARED_STATUS_CONFIG,
                *KernelBuildConstants.REQUIRED_SANDBOX_CONFIG,
                *KernelBuildConstants.REQUIRED_NETWORK_CONFIG,
                *KernelBuildConstants.REQUIRED_TIME_ABI_CONFIG,
                *KernelBuildConstants.REQUIRED_HARDENING_CONFIG,
            )
        )
        + "\n",
        encoding="utf-8",
    )
    manifest = {
        "format": 1,
        "distribution": {"name": "nvx", "version": "0.1.0"},
        "openvmm": {
            "microvm_abi_version": OpenVMMBuildConstants.MICROVM_ABI_VERSION,
            "time_abi_version": 1,
            "restore_packet_version": 1,
            "control_session_protocol_version": 1,
            "control_contract_revision": "nvx-microvm-v2-control-v2",
        },
        "guest_rust": {
            "toolchain": GuestRustBuildConstants.TOOLCHAIN,
            "target": GuestRustBuildConstants.TARGET,
        },
        "linux": {
            "version": "6.18.38",
            "upstream_url": (
                "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.18.38.tar.xz"
            ),
            "upstream_archive_sha256": KernelBuildConstants.SHA256,
            "source_cache": ".cache/linux/linux-6.18.38",
            "source_archive": ("build/sources/linux/nvx-linux-source-6.18.38.tar.gz"),
            "generated_final_config": "build/vmlinux.config",
            "input_config": "kernel/config-microvm",
            "patches": ["kernel/patches/example.patch"],
        },
        "alpine": {
            "version": "3.24.1",
            "branch": "v3.24",
            "architecture": "x86_64",
            "minirootfs_url": (
                "https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/"
                "x86_64/alpine-minirootfs-3.24.1-x86_64.tar.gz"
            ),
            "minirootfs_sha256": AlpineBuildConstants.MINIROOTFS_SHA256,
            "guest_sources": [
                "guest/common",
                *GuestRustBuildConstants.SOURCE_PATHS,
            ],
            "package_manifests": ["build/initramfs.cpio.gz.packages.json"],
            "source_output": "build/sources/alpine",
        },
        "ubuntu": {
            "distribution": "Ubuntu Base",
            "version": UbuntuBuildConstants.VERSION,
            "codename": UbuntuBuildConstants.CODENAME,
            "architecture": UbuntuBuildConstants.ARCHITECTURE,
            "base_url": UbuntuBuildConstants.BASE_URL,
            "base_sha256": UbuntuBuildConstants.BASE_SHA256,
            "archive_keyring_url": (UbuntuBuildConstants.ARCHIVE_KEYRING_URL),
            "archive_keyring_sha256": (UbuntuBuildConstants.ARCHIVE_KEYRING_SHA256),
            "package_lock": "ubuntu/packages.lock.json",
            "package_lock_sha256": ubuntu.package_lock_sha256(),
            "guest_sources": [
                "guest/common",
                "guest/ubuntu",
                "ubuntu/packages.lock.json",
                *GuestRustBuildConstants.SOURCE_PATHS,
            ],
            "package_manifests": [
                "build/initramfs-ubuntu.cpio.gz.packages.json",
                "build/ubuntu-distro.erofs.manifest.json",
            ],
            "source_output": "build/sources/ubuntu",
            "erofs_converter_format": UbuntuBuildConstants.EROFS_FORMAT,
        },
        "azurelinux": {
            "distribution": "Azure Linux",
            "version": AzureLinuxBuildConstants.VERSION,
            "architecture": AzureLinuxBuildConstants.ARCHITECTURE,
            "image": AzureLinuxBuildConstants.IMAGE,
            "package_lock": "azurelinux/packages.lock.json",
            "package_lock_sha256": azurelinux.package_lock_sha256(),
            "guest_sources": [
                *(
                    path.as_posix()
                    for path in AzureLinuxBuildConstants.GUEST_SOURCE_DIRECTORIES
                ),
                "azurelinux/packages.lock.json",
                *GuestRustBuildConstants.SOURCE_PATHS,
            ],
            "package_manifests": [
                (
                    Path(BuildConstants.BUILD_DIRECTORY_NAME)
                    / AzureLinuxBuildConstants.PACKAGE_MANIFEST_NAME
                ).as_posix()
            ],
        },
    }
    (root / "SOURCE-MANIFEST.json").write_text(
        json.dumps(manifest),
        encoding="utf-8",
    )
    kernel_inputs: dict[str, object] = {
        "source": {
            "version": "6.18.38",
            "patches": [
                {
                    "path": "kernel/patches/example.patch",
                    "sha256": "2" * 64,
                }
            ],
        },
        "input_config": {
            "path": "kernel/config-microvm",
            "sha256": "1" * 64,
        },
    }
    (build_dir / OpenVMMBuildConstants.PROVENANCE_NAME).write_text(
        json.dumps(
            {
                "format": 1,
                "source_revision": revision,
                "source_clean": True,
                "executable_sha256": common.sha256_file(binary),
            }
        ),
        encoding="utf-8",
    )
    (build_dir / KernelBuildConstants.PROVENANCE_NAME).write_text(
        json.dumps(
            {
                "format": 1,
                **kernel_inputs,
                "kernel_sha256": common.sha256_file(build_dir / "vmlinux"),
                "config_sha256": common.sha256_file(generated_config),
            }
        ),
        encoding="utf-8",
    )
    (build_dir / InitramfsBuildConstants.PROVENANCE_NAME).write_text(
        json.dumps(
            {
                "format": 1,
                "inputs": build.initramfs_provenance_inputs(),
                "initramfs_sha256": common.sha256_file(build_dir / "initramfs.cpio.gz"),
                "package_manifest_sha256": common.sha256_file(
                    build_dir / "initramfs.cpio.gz.packages.json"
                ),
            }
        ),
        encoding="utf-8",
    )
    paths = {
        "build": build_dir,
        "source": source_dir,
        "openvmm": openvmm_dir,
        "binary": binary,
        "nvx": nvx_binary,
    }
    return paths, kernel_inputs, revision


def posix_shell() -> str | None:
    shell = shutil.which("sh")
    if shell is None:
        git = shutil.which("git")
        git_shell = Path(git).parent.parent / "bin" / "sh.exe" if git else None
        if git_shell is not None and git_shell.is_file():
            shell = str(git_shell)
    return shell


def shell_function(source: str, name: str) -> str:
    start = source.index(f"\n{name}() {{\n") + 1
    return source[start : source.index("\n}\n", start) + 3]
