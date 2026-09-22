"""Runtime configuration shared by NVX build workflows."""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal

from .common import (
    BUILD_DIR,
    OPENVMM_DIR,
    REPO_ROOT,
    SOURCE_DIR,
    artifact_path,
    openvmm_binary_path,
)
from .guests import GUEST_NAMES, guest_descriptor

OpenVmmBackend = Literal["kvm", "mshv", "whp"]
OpenVmmPlatform = Literal["linux-gnu", "linux-musl", "windows-msvc"]
OpenVmmBuildMode = Literal["native", "musl"]


def _default_cache_directory() -> Path:
    configured = os.environ.get("NVX_CACHE_DIR")
    return (
        Path(configured).expanduser().resolve()
        if configured
        else (REPO_ROOT / ".cache").resolve()
    )


@dataclass(frozen=True, slots=True)
class DockerBuildConfig:
    artifact_destination: Path = BUILD_DIR
    linux_source_destination: Path = SOURCE_DIR / "linux"


@dataclass(frozen=True, slots=True)
class InitramfsBuildConfig:
    guest: str = "alpine"
    work: Path = BUILD_DIR / "initramfs-alpine-work"
    output: Path | None = None


@dataclass(frozen=True, slots=True)
class DistroLayerBuildConfig:
    guest: str = "ubuntu"
    work: Path = BUILD_DIR / "ubuntu-distro-work"
    output: Path = BUILD_DIR / "ubuntu-distro.erofs"
    replace: bool = False


@dataclass(frozen=True, slots=True)
class KernelBuildConfig:
    work: Path = BUILD_DIR / "linux"
    output: Path = BUILD_DIR / "vmlinux"
    cache_directory: Path = field(default_factory=_default_cache_directory)


@dataclass(frozen=True, slots=True)
class OpenVmmBuildConfig:
    skip_restore: bool = False
    build_directory: Path = BUILD_DIR
    directory: Path = OPENVMM_DIR
    output: Path = field(default_factory=openvmm_binary_path)
    backend: OpenVmmBackend | None = None

    @staticmethod
    def openvmm_target(platform: OpenVmmPlatform) -> str:
        targets = {
            "linux-gnu": "x86_64-unknown-linux-gnu",
            "linux-musl": "x86_64-unknown-linux-musl",
            "windows-msvc": "x86_64-pc-windows-msvc",
        }
        return targets[platform]

    @staticmethod
    def openvmm_build_mode(platform: OpenVmmPlatform) -> OpenVmmBuildMode:
        return "musl" if platform == "linux-musl" else "native"

    def openvmm_target_output(self, platform: OpenVmmPlatform) -> Path:
        executable = "openvmm.exe" if platform == "windows-msvc" else "openvmm"
        target_directory = self.directory / "target"
        if self.openvmm_build_mode(platform) == "musl":
            target_directory /= self.openvmm_target(platform)
        return target_directory / "release" / executable


@dataclass(frozen=True, slots=True)
class BuildConfig:
    guest: str = "alpine"
    native_guest: bool = False
    docker: DockerBuildConfig = field(default_factory=DockerBuildConfig)
    kernel: KernelBuildConfig = field(default_factory=KernelBuildConfig)
    openvmm: OpenVmmBuildConfig = field(default_factory=OpenVmmBuildConfig)

    def selected_guests(self) -> tuple[str, ...]:
        if self.guest == "all":
            return GUEST_NAMES
        return (guest_descriptor(self.guest).name,)

    @staticmethod
    def initramfs_config(guest: str) -> InitramfsBuildConfig:
        descriptor = guest_descriptor(guest)
        return InitramfsBuildConfig(
            guest=descriptor.name,
            work=BUILD_DIR / f"initramfs-{descriptor.name}-work",
            output=artifact_path(descriptor.initramfs_name),
        )

    @staticmethod
    def distro_layer_config(guest: str = "ubuntu") -> DistroLayerBuildConfig:
        descriptor = guest_descriptor(guest)
        return DistroLayerBuildConfig(
            guest=descriptor.name,
            work=BUILD_DIR / f"{descriptor.name}-distro-work",
            output=artifact_path(f"{descriptor.name}-distro.erofs"),
            replace=True,
        )
