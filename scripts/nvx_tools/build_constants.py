"""Fixed build inputs and defaults shared by NVX tooling."""

from __future__ import annotations

from pathlib import Path
from typing import Final


class BuildConstants:
    REPO_ROOT: Final = Path(__file__).resolve().parents[2]
    BUILD_DIRECTORY_NAME: Final = "build"
    SOURCE_RELATIVE_DIRECTORY: Final = Path(BUILD_DIRECTORY_NAME) / "sources"
    BUILD_DIR: Final = REPO_ROOT / BUILD_DIRECTORY_NAME
    SOURCE_DIR: Final = REPO_ROOT / SOURCE_RELATIVE_DIRECTORY
    COMMON_GUEST_DIRECTORY: Final = Path("guest") / "common"
    CACHE_DIRECTORY_NAME: Final = ".cache"
    CACHE_ENVIRONMENT_VARIABLE: Final = "NVX_CACHE_DIR"
    DOWNLOAD_DIRECTORY_NAME: Final = "downloads"
    PACKAGE_MANIFEST_SUFFIX: Final = ".packages.json"
    DISTRO_MANIFEST_SUFFIX: Final = ".manifest.json"
    DISTRO_NAME_TEMPLATE: Final = "{guest}-distro.erofs"


class KernelBuildConstants:
    VERSION: Final = "6.18.38"
    URL: Final = "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.18.38.tar.xz"
    SHA256: Final = "ac26e508abd56e9f8b89872b6e10c49fc823bcc70d8068a5d8504c1a7c4ff045"
    WORK_DIRECTORY_NAME: Final = "linux"
    SOURCE_DIRECTORY_NAME: Final = "linux"
    SOURCE_NAME: Final = f"linux-{VERSION}"
    UPSTREAM_ARCHIVE_NAME: Final = f"{SOURCE_NAME}.tar.xz"
    SOURCE_ARCHIVE_ROOT: Final = f"nvx-linux-source-{VERSION}"
    SOURCE_ARCHIVE_NAME: Final = f"{SOURCE_ARCHIVE_ROOT}.tar.gz"
    SOURCE_STAMP_NAME: Final = f"{SOURCE_NAME}.nvx-source.json"
    BUILD_STAMP_NAME: Final = ".nvx-build.json"
    BUILD_CONFIG_NAME: Final = ".config"
    INPUT_CONFIG: Final = Path("kernel") / "config-microvm"
    PATCH_DIRECTORY: Final = Path("kernel") / "patches"
    BINARY_NAME: Final = "vmlinux"
    CONFIG_NAME: Final = f"{BINARY_NAME}.config"
    PROVENANCE_NAME: Final = "vmlinux.provenance.json"
    PROVENANCE_FORMAT: Final = 1
    REQUIRED_VIRTIO_CONSOLE_CONFIG: Final = (
        "CONFIG_HVC_DRIVER=y",
        "CONFIG_VIRTIO=y",
        "CONFIG_VIRTIO_CONSOLE=y",
        "CONFIG_VIRTIO_MMIO=y",
        "CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y",
    )
    REQUIRED_SHARED_STATUS_CONFIG: Final = ("CONFIG_VIRTIO_MMIO_SHARED_STATUS=y",)
    REQUIRED_SANDBOX_CONFIG: Final = (
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
        "CONFIG_UNIX=y",
        "CONFIG_VIRTIO_BLK=y",
    )


class OpenVMMBuildConstants:
    DIRECTORY: Final = BuildConstants.REPO_ROOT / "openvmm"
    PACKAGE_NAME: Final = "openvmm"
    BINARY_NAME: Final = "openvmm"
    WINDOWS_BINARY_NAME: Final = f"{BINARY_NAME}.exe"
    TARGET_DIRECTORY_NAME: Final = "target"
    BUILD_PROFILE: Final = "release"
    MUSL_SYSROOT: Final = Path(".packages") / "extracted" / "x86_64-sysroot"
    RUST_TOOLCHAIN: Final = "stable"
    GNU_RUST_TARGET: Final = "x86_64-unknown-linux-gnu"
    MUSL_RUST_TARGET: Final = "x86_64-unknown-linux-musl"
    WINDOWS_RUST_TARGET: Final = "x86_64-pc-windows-msvc"
    GUEST_RUST_TARGET: Final = "x86_64-unknown-none"
    UEFI_RUST_TARGET: Final = "x86_64-unknown-uefi"
    BUILD_TARGETS: Final[dict[str, str]] = {
        "linux-gnu": GNU_RUST_TARGET,
        "linux-musl": MUSL_RUST_TARGET,
        "windows-msvc": WINDOWS_RUST_TARGET,
    }
    TEST_RUST_TARGETS: Final[dict[str, tuple[str, ...]]] = {
        "kvm": (GUEST_RUST_TARGET, UEFI_RUST_TARGET, MUSL_RUST_TARGET),
        "mshv": (GUEST_RUST_TARGET, UEFI_RUST_TARGET, MUSL_RUST_TARGET),
        "whp": (GUEST_RUST_TARGET, UEFI_RUST_TARGET),
    }
    RUSTUP_DIRECTORY_NAME: Final = "openvmm-rustup"
    CACHE_DIRECTORY_NAME: Final = "openvmm-cache"
    PROVENANCE_NAME: Final = "openvmm.provenance.json"
    PROVENANCE_FORMAT: Final = 1
    MICROVM_ABI_VERSION: Final = 2
    CONTROL_SESSION_PROTOCOL_VERSION: Final = 1
    CONTROL_CONTRACT_REVISION: Final = "nvx-microvm-v2-control-v1"


class AlpineBuildConstants:
    GUEST_NAME: Final = "alpine"
    GUEST_DIRECTORY: Final = Path("guest") / GUEST_NAME
    GUEST_SOURCE_DIRECTORIES: Final = (
        BuildConstants.COMMON_GUEST_DIRECTORY,
        GUEST_DIRECTORY,
    )
    DISTRIBUTION: Final = "Alpine Linux"
    VERSION: Final = "3.24.1"
    BRANCH: Final = "v3.24"
    ARCHITECTURE: Final = "x86_64"
    MINIROOTFS_NAME: Final = f"alpine-minirootfs-{VERSION}-{ARCHITECTURE}.tar.gz"
    MINIROOTFS_URL: Final = (
        f"https://dl-cdn.alpinelinux.org/alpine/{BRANCH}/releases/"
        f"{ARCHITECTURE}/{MINIROOTFS_NAME}"
    )
    MINIROOTFS_SHA256: Final = (
        "41f73e3cf5fa919b8aa5ca6b30dc48f0da2720776d7423e2a7748211456fe081"
    )
    INITRAMFS_NAME: Final = "initramfs.cpio.gz"
    PACKAGE_MANIFEST_NAME: Final = (
        f"{INITRAMFS_NAME}{BuildConstants.PACKAGE_MANIFEST_SUFFIX}"
    )
    PACKAGE_MANIFEST_FORMAT: Final = "apk-v1"
    PACKAGE_MANIFEST_VERSION: Final = 1
    SOURCE_MANIFEST_FORMAT: Final = 1
    PACKAGES: Final = (
        "blkid",
        "busybox-extras",
        "e2fsprogs",
        "util-linux",
        "util-linux-misc",
    )
    APORTS_URL: Final = "https://gitlab.alpinelinux.org/alpine/aports.git"
    APORTS_CACHE_DIRECTORY_NAME: Final = "aports"
    PACKAGE_INDEX_URL: Final = "https://pkgs.alpinelinux.org/packages"
    REPOSITORIES: Final = ("main", "community", "testing")


class UbuntuBuildConstants:
    GUEST_NAME: Final = "ubuntu"
    GUEST_DIRECTORY: Final = Path("guest") / GUEST_NAME
    GUEST_SOURCE_DIRECTORIES: Final = (
        BuildConstants.COMMON_GUEST_DIRECTORY,
        GUEST_DIRECTORY,
    )
    DISTRIBUTION: Final = "Ubuntu Base"
    VERSION: Final = "26.04.1"
    CODENAME: Final = "resolute"
    ARCHITECTURE: Final = "amd64"
    BASE_URL: Final = (
        "https://cdimage.ubuntu.com/ubuntu-base/releases/26.04/release/"
        "ubuntu-base-26.04.1-base-amd64.tar.gz"
    )
    BASE_SHA256: Final = (
        "a496a960472ce474a59590b8987d3a1135d3cbef1991f3b1abe8cacfea8bf85a"
    )
    BASE_ARCHIVE_NAME: Final = f"ubuntu-base-{VERSION}-base-{ARCHITECTURE}.tar.gz"
    PACKAGE_LOCK_RELATIVE_PATH: Final = Path("ubuntu") / "packages.lock.json"
    PACKAGE_LOCK: Final = BuildConstants.REPO_ROOT / PACKAGE_LOCK_RELATIVE_PATH
    PACKAGE_LOCK_FORMAT: Final = 1
    INITRAMFS_NAME: Final = "initramfs-ubuntu.cpio.gz"
    PACKAGE_MANIFEST_NAME: Final = (
        f"{INITRAMFS_NAME}{BuildConstants.PACKAGE_MANIFEST_SUFFIX}"
    )
    PACKAGE_MANIFEST_FORMAT: Final = "ubuntu-v1"
    PACKAGE_MANIFEST_VERSION: Final = 1
    DISTRO_NAME: Final = BuildConstants.DISTRO_NAME_TEMPLATE.format(guest=GUEST_NAME)
    DISTRO_MANIFEST_NAME: Final = (
        f"{DISTRO_NAME}{BuildConstants.DISTRO_MANIFEST_SUFFIX}"
    )
    EROFS_FORMAT: Final = 1
    EROFS_COMPRESSION: Final = "lz4hc"
    EROFS_TIMESTAMP: Final = 0
    SOURCE_MANIFEST_FORMAT: Final = 1
    SOURCE_CACHE_DIRECTORY_NAME: Final = "ubuntu-source-indexes"
    PACKAGE_CACHE_DIRECTORY_NAME: Final = "ubuntu-packages"
    EROFS_DIGEST_DOMAIN: Final = "nvx-ubuntu-erofs"
    ARCHIVE_KEYRING_URL: Final = (
        "https://archive.ubuntu.com/ubuntu/project/ubuntu-archive-keyring.gpg"
    )
    ARCHIVE_KEYRING_SHA256: Final = (
        "80a36b0a6de2f69f49d2df75ef473ccde121e9e190b9ea01d20a4f63778d5c31"
    )
    SNAPSHOT_ARCHIVE_URL: Final = "https://snapshot.ubuntu.com/ubuntu"
    SOURCE_INDEXES: Final = (
        ("https://archive.ubuntu.com/ubuntu", f"{CODENAME}-updates", "main"),
        ("https://archive.ubuntu.com/ubuntu", f"{CODENAME}-updates", "universe"),
        ("https://security.ubuntu.com/ubuntu", f"{CODENAME}-security", "main"),
        ("https://security.ubuntu.com/ubuntu", f"{CODENAME}-security", "universe"),
        ("https://archive.ubuntu.com/ubuntu", CODENAME, "main"),
        ("https://archive.ubuntu.com/ubuntu", CODENAME, "universe"),
    )
    POCKET_SUITES: Final[dict[str, str]] = {
        "Release": CODENAME,
        "Updates": f"{CODENAME}-updates",
        "Security": f"{CODENAME}-security",
        "Backports": f"{CODENAME}-backports",
        "Proposed": f"{CODENAME}-proposed",
    }
    COMPONENTS: Final = frozenset(("main", "restricted", "universe", "multiverse"))
    LAUNCHPAD_ARCHIVE_API: Final = (
        "https://api.launchpad.net/1.0/ubuntu/+archive/primary"
    )
    LAUNCHPAD_SERIES_API: Final = f"https://api.launchpad.net/1.0/ubuntu/{CODENAME}"
    SOURCE_OUTPUT_ENTRIES: Final = frozenset(
        ("manifest.json", "metadata", "packages", "SHA256SUMS")
    )
    KNOWN_PACKAGE_CONTROL_ACTIONS: Final[dict[str, frozenset[str]]] = {
        "iputils-ping": frozenset({"postinst"}),
        "libcap2": frozenset({"triggers"}),
        "libidn2-0": frozenset({"triggers"}),
        "libunistring5": frozenset({"triggers"}),
        "netcat-openbsd": frozenset({"postinst", "prerm"}),
    }


class InitramfsBuildConstants:
    DEFAULT_GUEST: Final = AlpineBuildConstants.GUEST_NAME
    WORK_DIRECTORY_TEMPLATE: Final = "initramfs-{guest}-work"
    DISTRO_WORK_DIRECTORY_TEMPLATE: Final = "{guest}-distro-work"
    DETERMINISM_DIRECTORY_NAME: Final = "guest-determinism"
    DETERMINISM_INITRAMFS_WORK_DIRECTORY_NAME: Final = "initramfs-work"
    DETERMINISM_DISTRO_WORK_DIRECTORY_NAME: Final = "distro-work"
    ROOT_DIRECTORY_NAME: Final = "root"
    PROVENANCE_NAME: Final = "initramfs.provenance.json"
    PROVENANCE_FORMAT: Final = 1
    TIMESTAMP: Final = 0
    CPIO_FORMAT: Final = "newc"
    GZIP_COMPRESSION_LEVEL: Final = 9
    STATIC_HELPER_CFLAGS: Final = (
        "-static",
        "-Os",
        "-s",
        "-Wall",
        "-Wextra",
        "-Werror",
    )
    DEVICE_IO_CFLAGS: Final = (
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
    )
    STATIC_HELPERS: Final = (
        "nvx-reseed",
        "nvx-mmio-write",
        "nvx-port-io",
        "nvx-console-pending",
        "nvx-managed-agent",
    )


class DockerBuildConstants:
    DOCKERFILE: Final = Path("docker") / "Dockerfile"
    ALPINE_TARGET: Final = "artifacts"
    UBUNTU_TARGET: Final = "ubuntu-guest-artifacts"
    ALL_GUESTS_TARGET: Final = "all-guest-artifacts"
    LINUX_SOURCE_TARGET: Final = "linux-source-artifacts"
    OUTPUT_TYPE: Final = "local"
    ALL_GUEST_ARTIFACT_NAMES: Final = (
        KernelBuildConstants.BINARY_NAME,
        KernelBuildConstants.CONFIG_NAME,
        KernelBuildConstants.PROVENANCE_NAME,
        AlpineBuildConstants.INITRAMFS_NAME,
        AlpineBuildConstants.PACKAGE_MANIFEST_NAME,
        InitramfsBuildConstants.PROVENANCE_NAME,
        UbuntuBuildConstants.INITRAMFS_NAME,
        UbuntuBuildConstants.PACKAGE_MANIFEST_NAME,
        UbuntuBuildConstants.DISTRO_NAME,
        UbuntuBuildConstants.DISTRO_MANIFEST_NAME,
    )


class ZstdBuildConstants:
    VERSION: Final = "1.5.7"
    DIRECTORY_NAME: Final = f"zstd-v{VERSION}-win64"
    ARCHIVE_NAME: Final = f"{DIRECTORY_NAME}.zip"
    URL: Final = (
        f"https://github.com/facebook/zstd/releases/download/v{VERSION}/{ARCHIVE_NAME}"
    )
    SHA256: Final = "acb4e8111511749dc7a3ebedca9b04190e37a17afeb73f55d4425dbf0b90fad9"


class ReleaseBuildConstants:
    DIRECTORY_NAME: Final = "dist"
    SOURCE_MANIFEST_FORMAT: Final = 1
    PROJECT_SOURCE_PATHS: Final = (
        ".github/agents/nvx-adversary.md",
        "guest",
        "ubuntu",
        "data/linux-kvm-virtual-machine.csv",
        "data/linux-mshv-virtual-machine.csv",
        "data/windows-whp-virtual-machine.csv",
        "docker",
        "kernel",
        "licenses",
        "scripts",
        ".dockerignore",
        ".gitattributes",
        ".gitmodules",
        ".gitignore",
        "LICENSE",
        "README.md",
        "SOURCE-MANIFEST.json",
        "THIRD_PARTY_NOTICES.md",
        "VERSION",
        "pyproject.toml",
        "requirements-dev.txt",
    )
    GUEST_ARTIFACT_NAMES: Final = (
        KernelBuildConstants.BINARY_NAME,
        KernelBuildConstants.CONFIG_NAME,
        AlpineBuildConstants.INITRAMFS_NAME,
        AlpineBuildConstants.PACKAGE_MANIFEST_NAME,
        UbuntuBuildConstants.INITRAMFS_NAME,
        UbuntuBuildConstants.PACKAGE_MANIFEST_NAME,
        UbuntuBuildConstants.DISTRO_NAME,
        UbuntuBuildConstants.DISTRO_MANIFEST_NAME,
    )
    TAR_TIMESTAMP: Final = 0
    ZIP_TIMESTAMP: Final = (1980, 1, 1, 0, 0, 0)
    ZIP_COMPRESSION_LEVEL: Final = 9
