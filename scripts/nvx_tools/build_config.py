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
from .common import ScriptError, artifact_path, cache_root, openvmm_binary_path
from .guests import GUEST_NAMES, guest_descriptor

OpenVmmBackend = Literal["kvm", "mshv", "whp"]
OpenVmmPlatform = Literal["linux-gnu", "linux-musl", "windows-msvc"]
OpenVmmBuildMode = Literal["native", "musl"]


def _default_cache_directory() -> Path:
    return cache_root()


def _check_jobs(jobs: int | None) -> None:
    if jobs is not None and (isinstance(jobs, bool) or jobs < 1):
        raise ScriptError("kernel jobs must be a positive integer")


@dataclass(frozen=True, slots=True)
class DockerBuildConfig:
    kernel_jobs: int | None = None
    artifact_destination: Path = BuildConstants.BUILD_DIR
    linux_source_destination: Path = (
        BuildConstants.SOURCE_DIR / KernelBuildConstants.SOURCE_DIRECTORY_NAME
    )

    def __post_init__(self) -> None:
        _check_jobs(self.kernel_jobs)


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
    architecture: str = "x86_64"

    def __post_init__(self) -> None:
        if self.architecture not in ("x86_64", "aarch64"):
            raise ScriptError(
                f"unsupported initramfs architecture: {self.architecture}"
            )
        if self.architecture == "aarch64":
            if self.guest != "alpine":
                raise ScriptError(
                    "ARM initramfs production currently supports Alpine only"
                )
            default_work = (
                BuildConstants.BUILD_DIR
                / InitramfsBuildConstants.WORK_DIRECTORY_TEMPLATE.format(
                    guest=InitramfsBuildConstants.DEFAULT_GUEST
                )
            )
            if self.work == default_work:
                object.__setattr__(
                    self,
                    "work",
                    BuildConstants.BUILD_DIR / "aarch64" / default_work.name,
                )
            if self.output is None:
                object.__setattr__(
                    self,
                    "output",
                    BuildConstants.BUILD_DIR
                    / "aarch64"
                    / guest_descriptor(self.guest).initramfs_name,
                )


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
class ScratchTemplateBuildConfig:
    output: Path = artifact_path(UbuntuBuildConstants.SCRATCH_TEMPLATE_NAME)
    replace: bool = False


@dataclass(frozen=True, slots=True)
class KernelBuildConfig:
    work: Path = BuildConstants.BUILD_DIR / KernelBuildConstants.WORK_DIRECTORY_NAME
    output: Path = artifact_path(KernelBuildConstants.BINARY_NAME)
    cache_directory: Path = field(default_factory=_default_cache_directory)
    debug: bool = False
    jobs: int | None = None
    architecture: str = "x86_64"

    def __post_init__(self) -> None:
        _check_jobs(self.jobs)
        if self.architecture not in ("x86_64", "aarch64"):
            raise ScriptError(f"unsupported kernel architecture: {self.architecture}")
        if self.architecture == "aarch64" and (
            self.work
            == BuildConstants.BUILD_DIR / KernelBuildConstants.WORK_DIRECTORY_NAME
            or self.output == artifact_path(KernelBuildConstants.BINARY_NAME)
        ):
            raise ScriptError(
                "AArch64 kernel work/output must not overwrite x86 defaults; "
                "select KernelBuildConfig.for_architecture or explicit isolated paths"
            )

    @staticmethod
    def for_architecture(
        architecture: str, *, debug: bool = False, jobs: int | None = None
    ) -> KernelBuildConfig:
        if architecture == "x86_64":
            return (
                KernelBuildConfig.debug_variant(jobs=jobs)
                if debug
                else KernelBuildConfig(jobs=jobs)
            )
        if architecture != "aarch64":
            raise ScriptError(f"unsupported kernel architecture: {architecture}")
        suffix = "-debug" if debug else ""
        return KernelBuildConfig(
            architecture=architecture,
            work=BuildConstants.BUILD_DIR / architecture / f"linux{suffix}",
            output=BuildConstants.BUILD_DIR / architecture / f"Image{suffix}",
            debug=debug,
            jobs=jobs,
        )

    @property
    def input_config(self) -> Path:
        return (
            KernelBuildConstants.ARM_INPUT_CONFIG
            if self.architecture == "aarch64"
            else KernelBuildConstants.INPUT_CONFIG
        )

    @property
    def derived_config(self) -> Path:
        return (
            KernelBuildConstants.ARM_DERIVED_CONFIG
            if self.architecture == "aarch64"
            else KernelBuildConstants.DERIVED_CONFIG
        )

    @property
    def make_architecture(self) -> tuple[str, ...]:
        return ("ARCH=arm64",) if self.architecture == "aarch64" else ()

    @property
    def make_target(self) -> str:
        return (
            "Image"
            if self.architecture == "aarch64"
            else KernelBuildConstants.BINARY_NAME
        )

    @property
    def built_kernel(self) -> Path:
        return (
            Path("arch") / "arm64" / "boot" / "Image"
            if self.architecture == "aarch64"
            else Path(KernelBuildConstants.BINARY_NAME)
        )

    @staticmethod
    def debug_variant(*, jobs: int | None = None) -> KernelBuildConfig:
        """Return the CI debug kernel, which adds the debug config fragment."""
        return KernelBuildConfig(
            work=BuildConstants.BUILD_DIR
            / KernelBuildConstants.DEBUG_WORK_DIRECTORY_NAME,
            output=artifact_path(KernelBuildConstants.DEBUG_BINARY_NAME),
            debug=True,
            jobs=jobs,
        )

    @property
    def provenance_name(self) -> str:
        if self.architecture == "aarch64":
            return f"{self.output.name}.provenance.json"
        return (
            KernelBuildConstants.DEBUG_PROVENANCE_NAME
            if self.debug
            else KernelBuildConstants.PROVENANCE_NAME
        )


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
    debug_kernel: bool = False
    docker: DockerBuildConfig = field(default_factory=DockerBuildConfig)
    kernel: KernelBuildConfig = field(default_factory=KernelBuildConfig)
    openvmm: OpenVmmBuildConfig = field(default_factory=OpenVmmBuildConfig)

    def selected_guests(self) -> tuple[str, ...]:
        if self.guest == "all":
            return GUEST_NAMES
        return (guest_descriptor(self.guest).name,)

    @staticmethod
    def initramfs_config(
        guest: str, *, architecture: str = "x86_64"
    ) -> InitramfsBuildConfig:
        descriptor = guest_descriptor(guest)
        return InitramfsBuildConfig(
            guest=descriptor.name,
            work=(
                BuildConstants.BUILD_DIR
                / (Path("aarch64") if architecture == "aarch64" else Path())
                / InitramfsBuildConstants.WORK_DIRECTORY_TEMPLATE.format(
                    guest=descriptor.name
                )
            ),
            output=(
                BuildConstants.BUILD_DIR / "aarch64" / descriptor.initramfs_name
                if architecture == "aarch64"
                else artifact_path(descriptor.initramfs_name)
            ),
            architecture=architecture,
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
