"""Typed Linux guest descriptors shared by build, run, and test workflows."""

from __future__ import annotations

from dataclasses import dataclass

from .build_constants import (
    AlpineBuildConstants,
    AzureLinuxBuildConstants,
    BuildConstants,
    DockerBuildConstants,
    UbuntuBuildConstants,
)
from .common import ScriptError


@dataclass(frozen=True)
class GuestDescriptor:
    name: str
    distribution: str
    release: str
    architecture: str
    initramfs_name: str
    package_manifest_format: str
    default_memory_mib: int
    boot_marker: bytes
    sandbox_control: bool
    native_build_supported: bool
    docker_artifacts_target: str
    docker_initramfs_artifacts_target: str | None
    os_release_id: str

    @property
    def package_manifest_name(self) -> str:
        return f"{self.initramfs_name}{BuildConstants.PACKAGE_MANIFEST_SUFFIX}"


ALPINE_GUEST = GuestDescriptor(
    name=AlpineBuildConstants.GUEST_NAME,
    distribution=AlpineBuildConstants.DISTRIBUTION,
    release=AlpineBuildConstants.VERSION,
    architecture=AlpineBuildConstants.ARCHITECTURE,
    initramfs_name=AlpineBuildConstants.INITRAMFS_NAME,
    package_manifest_format=AlpineBuildConstants.PACKAGE_MANIFEST_FORMAT,
    default_memory_mib=128,
    boot_marker=b"NVX-GUEST-BOOT-OK: alpine",
    sandbox_control=True,
    native_build_supported=True,
    docker_artifacts_target=DockerBuildConstants.ALPINE_TARGET,
    docker_initramfs_artifacts_target=None,
    os_release_id=AlpineBuildConstants.GUEST_NAME,
)
UBUNTU_GUEST = GuestDescriptor(
    name=UbuntuBuildConstants.GUEST_NAME,
    distribution=UbuntuBuildConstants.DISTRIBUTION,
    release=UbuntuBuildConstants.VERSION,
    architecture=UbuntuBuildConstants.ARCHITECTURE,
    initramfs_name=UbuntuBuildConstants.INITRAMFS_NAME,
    package_manifest_format=UbuntuBuildConstants.PACKAGE_MANIFEST_FORMAT,
    default_memory_mib=256,
    boot_marker=b"NVX-GUEST-BOOT-OK: ubuntu",
    sandbox_control=False,
    native_build_supported=True,
    docker_artifacts_target=DockerBuildConstants.UBUNTU_TARGET,
    docker_initramfs_artifacts_target=None,
    os_release_id=UbuntuBuildConstants.GUEST_NAME,
)
AZURELINUX_GUEST = GuestDescriptor(
    name=AzureLinuxBuildConstants.GUEST_NAME,
    distribution=AzureLinuxBuildConstants.DISTRIBUTION,
    release=AzureLinuxBuildConstants.VERSION,
    architecture=AzureLinuxBuildConstants.ARCHITECTURE,
    initramfs_name=AzureLinuxBuildConstants.INITRAMFS_NAME,
    package_manifest_format=AzureLinuxBuildConstants.PACKAGE_MANIFEST_FORMAT,
    default_memory_mib=128,
    boot_marker=b"NVX-GUEST-BOOT-OK: azurelinux",
    sandbox_control=False,
    native_build_supported=False,
    docker_artifacts_target=DockerBuildConstants.AZURELINUX_TARGET,
    docker_initramfs_artifacts_target=DockerBuildConstants.AZURELINUX_INITRAMFS_TARGET,
    os_release_id=AzureLinuxBuildConstants.GUEST_NAME,
)
GUEST_DESCRIPTORS = {
    descriptor.name: descriptor
    for descriptor in (ALPINE_GUEST, UBUNTU_GUEST, AZURELINUX_GUEST)
}
GUEST_NAMES = tuple(GUEST_DESCRIPTORS)


def guest_descriptor(name: str) -> GuestDescriptor:
    try:
        return GUEST_DESCRIPTORS[name]
    except KeyError as error:
        raise ScriptError(
            f"unsupported guest {name!r}; choose {', '.join(GUEST_NAMES)}"
        ) from error
