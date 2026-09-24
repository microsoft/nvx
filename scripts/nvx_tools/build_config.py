"""Runtime configuration shared by NVX build workflows."""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal

from .build_constants import (
    BuildConstants,
    InitramfsBuildConstants,
    KernelBuildConstants,
    OpenVMMBuildConstants,
    UbuntuBuildConstants,
)
from .common import artifact_path, cache_root, openvmm_binary_path
from .guests import GUEST_NAMES, guest_descriptor

OpenVmmBackend = Literal["kvm", "mshv", "whp"]
OpenVmmPlatform = Literal["linux-gnu", "linux-musl", "windows-msvc"]
OpenVmmBuildMode = Literal["native", "musl"]


def _default_cache_directory() -> Path:
    return cache_root()


@dataclass(frozen=True, slots=True)
class DockerBuildConfig:
    artifact_destination: Path = BuildConstants.BUILD_DIR
    linux_source_destination: Path = (
        BuildConstants.SOURCE_DIR / KernelBuildConstants.SOURCE_DIRECTORY_NAME
    )


@dataclass(frozen=True, slots=True)
class InitramfsBuildConfig:
    guest: str = InitramfsBuildConstants.DEFAULT_GUEST
    work: Path = (
        BuildConstants.BUILD_DIR
        / InitramfsBuildConstants.WORK_DIRECTORY_TEMPLATE.format(
            guest=InitramfsBuildConstants.DEFAULT_GUEST
        )
    )
    output: Path | None = None


@dataclass(frozen=True, slots=True)
class DistroLayerBuildConfig:
    guest: str = UbuntuBuildConstants.GUEST_NAME
    work: Path = (
        BuildConstants.BUILD_DIR
        / InitramfsBuildConstants.DISTRO_WORK_DIRECTORY_TEMPLATE.format(
            guest=UbuntuBuildConstants.GUEST_NAME
        )
    )
    output: Path = artifact_path(UbuntuBuildConstants.DISTRO_NAME)
    replace: bool = False


@dataclass(frozen=True, slots=True)
class KernelBuildConfig:
    work: Path = BuildConstants.BUILD_DIR / KernelBuildConstants.WORK_DIRECTORY_NAME
    output: Path = artifact_path(KernelBuildConstants.BINARY_NAME)
    cache_directory: Path = field(default_factory=_default_cache_directory)


@dataclass(frozen=True, slots=True)
class OpenVmmBuildConfig:
    skip_restore: bool = False
    build_directory: Path = BuildConstants.BUILD_DIR
    directory: Path = OpenVMMBuildConstants.DIRECTORY
    output: Path = field(default_factory=openvmm_binary_path)
    backend: OpenVmmBackend | None = None

    @staticmethod
    def openvmm_target(platform: OpenVmmPlatform) -> str:
        return OpenVMMBuildConstants.BUILD_TARGETS[platform]

    @staticmethod
    def openvmm_build_mode(platform: OpenVmmPlatform) -> OpenVmmBuildMode:
        return "musl" if platform == "linux-musl" else "native"

    def openvmm_target_output(self, platform: OpenVmmPlatform) -> Path:
        executable = (
            OpenVMMBuildConstants.WINDOWS_BINARY_NAME
            if platform == "windows-msvc"
            else OpenVMMBuildConstants.BINARY_NAME
        )
        target_directory = self.directory / OpenVMMBuildConstants.TARGET_DIRECTORY_NAME
        if self.openvmm_build_mode(platform) == "musl":
            target_directory /= self.openvmm_target(platform)
        return target_directory / OpenVMMBuildConstants.BUILD_PROFILE / executable


@dataclass(frozen=True, slots=True)
class BuildConfig:
    guest: str = InitramfsBuildConstants.DEFAULT_GUEST
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
            work=(
                BuildConstants.BUILD_DIR
                / InitramfsBuildConstants.WORK_DIRECTORY_TEMPLATE.format(
                    guest=descriptor.name
                )
            ),
            output=artifact_path(descriptor.initramfs_name),
        )

    @staticmethod
    def distro_layer_config(
        guest: str = UbuntuBuildConstants.GUEST_NAME,
    ) -> DistroLayerBuildConfig:
        descriptor = guest_descriptor(guest)
        return DistroLayerBuildConfig(
            guest=descriptor.name,
            work=(
                BuildConstants.BUILD_DIR
                / InitramfsBuildConstants.DISTRO_WORK_DIRECTORY_TEMPLATE.format(
                    guest=descriptor.name
                )
            ),
            output=artifact_path(
                BuildConstants.DISTRO_NAME_TEMPLATE.format(guest=descriptor.name)
            ),
            replace=True,
        )
