"""Typed Linux guest descriptors shared by build, run, and test workflows."""

from __future__ import annotations

from dataclasses import dataclass

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
        return f"{self.initramfs_name}.packages.json"


ALPINE_GUEST = GuestDescriptor(
    name="alpine",
    distribution="Alpine Linux",
    release="3.24.1",
    architecture="x86_64",
    initramfs_name="initramfs.cpio.gz",
    package_manifest_format="apk-v1",
    default_memory_mib=128,
    boot_marker=b"NVX-GUEST-BOOT-OK: alpine",
    sandbox_control=True,
    native_build_supported=True,
    docker_artifacts_target="artifacts",
    docker_initramfs_artifacts_target=None,
    os_release_id="alpine",
)
UBUNTU_GUEST = GuestDescriptor(
    name="ubuntu",
    distribution="Ubuntu Base",
    release="26.04.1",
    architecture="amd64",
    initramfs_name="initramfs-ubuntu.cpio.gz",
    package_manifest_format="ubuntu-v1",
    default_memory_mib=256,
    boot_marker=b"NVX-GUEST-BOOT-OK: ubuntu",
    sandbox_control=False,
    native_build_supported=True,
    docker_artifacts_target="ubuntu-guest-artifacts",
    docker_initramfs_artifacts_target=None,
    os_release_id="ubuntu",
)
AZURELINUX_GUEST = GuestDescriptor(
    name="azurelinux",
    distribution="Azure Linux",
    release="3.0",
    architecture="x86_64",
    initramfs_name="initramfs-azurelinux.cpio.gz",
    package_manifest_format="azurelinux-v1",
    default_memory_mib=128,
    boot_marker=b"NVX-GUEST-BOOT-OK: azurelinux",
    sandbox_control=False,
    native_build_supported=False,
    docker_artifacts_target="azurelinux-guest-artifacts",
    docker_initramfs_artifacts_target="azurelinux-initramfs-artifacts",
    os_release_id="azurelinux",
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
