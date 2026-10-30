"""Fixed build inputs and defaults shared by NVX tooling."""

from __future__ import annotations

import re
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
    DEBUG_CONFIG_FRAGMENT: Final = Path("kernel") / "config-microvm-debug"
    PATCH_DIRECTORY: Final = Path("kernel") / "patches"
    BINARY_NAME: Final = "vmlinux"
    CONFIG_NAME: Final = f"{BINARY_NAME}.config"
    PROVENANCE_NAME: Final = "vmlinux.provenance.json"
    PROVENANCE_FORMAT: Final = 1
    DEBUG_WORK_DIRECTORY_NAME: Final = "linux-debug"
    DEBUG_BINARY_NAME: Final = "vmlinux-debug"
    DEBUG_CONFIG_NAME: Final = f"{DEBUG_BINARY_NAME}.config"
    DEBUG_PROVENANCE_NAME: Final = f"{DEBUG_BINARY_NAME}.provenance.json"
    REQUIRED_VIRTIO_CONSOLE_CONFIG: Final = (
        "CONFIG_HVC_DRIVER=y",
        "CONFIG_VIRTIO=y",
        "CONFIG_VIRTIO_CONSOLE=y",
        "CONFIG_VIRTIO_MMIO=y",
        "CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y",
    )
    REQUIRED_SHARED_STATUS_CONFIG: Final = ("CONFIG_VIRTIO_MMIO_SHARED_STATUS=y",)
    REQUIRED_DIRECT_BOOT_CONFIG: Final = (
        "# CONFIG_ACPI is not set",
        "# CONFIG_PVH is not set",
        "CONFIG_X86_MPPARSE=y",
        "CONFIG_X86_LOCAL_APIC=y",
        "CONFIG_X86_IO_APIC=y",
        "CONFIG_VIRTIO_MMIO=y",
        "CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y",
    )
    REQUIRED_SANDBOX_CONFIG: Final = (
        "CONFIG_BPF_SYSCALL=y",
        "CONFIG_CGROUP_BPF=y",
        "CONFIG_CGROUP_PIDS=y",
        "CONFIG_EROFS_FS=y",
        "CONFIG_EROFS_FS_ZIP=y",
        "CONFIG_EROFS_FS_ZIP_ZSTD=y",
        "CONFIG_EXT4_FS=y",
        "CONFIG_MEMCG=y",
        "CONFIG_NET_NS=y",
        "CONFIG_OVERLAY_FS=y",
        "# CONFIG_OVERLAY_FS_REDIRECT_ALWAYS_FOLLOW is not set",
        "CONFIG_SECCOMP_FILTER=y",
        "CONFIG_UNIX=y",
        "CONFIG_VETH=y",
        "CONFIG_VIRTIO_BLK=y",
    )
    # The portable network profile configures an IPv6 identity next to the
    # IPv4 one. SIT would add an sit0 device that decapsulates IPv6 from IPv4.
    REQUIRED_NETWORK_CONFIG: Final = (
        "CONFIG_INET=y",
        "CONFIG_IPV6=y",
        "# CONFIG_IPV6_SIT is not set",
        "CONFIG_VIRTIO_NET=y",
    )
    REQUIRED_TIME_ABI_CONFIG: Final = (
        "CONFIG_HYPERVISOR_GUEST=y",
        "CONFIG_PARAVIRT=y",
        "# CONFIG_HYPERV is not set",
        "# CONFIG_KVM_GUEST is not set",
        "# CONFIG_CPU_FREQ is not set",
    )
    # x86 makes runtime-generated code, such as the ITS mitigation thunks,
    # read-only only through STRICT_MODULE_RWX (ARCH_HAS_EXECMEM_ROX), which
    # needs MODULES. No module is ever built or shipped: there is no autoload
    # helper, no symbol is exported, and init disables module loading before
    # anything else runs.
    REQUIRED_HARDENING_CONFIG: Final = (
        "CONFIG_STRICT_KERNEL_RWX=y",
        "CONFIG_MODULES=y",
        "CONFIG_STRICT_MODULE_RWX=y",
        "CONFIG_ARCH_HAS_EXECMEM_ROX=y",
        'CONFIG_MODPROBE_PATH=""',
        "CONFIG_TRIM_UNUSED_KSYMS=y",
        "CONFIG_MITIGATION_ITS=y",
        "CONFIG_DEBUG_WX=y",
    )
    DEBUG_WATCHDOG_CONFIG: Final = (
        "CONFIG_SOFTLOCKUP_DETECTOR=y",
        "CONFIG_DETECT_HUNG_TASK=y",
    )
    REQUIRED_DEBUG_CONFIG: Final = (
        "CONFIG_DEBUG_KERNEL=y",
        *DEBUG_WATCHDOG_CONFIG,
        "CONFIG_RCU_CPU_STALL_CPUTIME=y",
    )


class OpenVMMBuildConstants:
    DIRECTORY: Final = BuildConstants.REPO_ROOT / "openvmm"
    PACKAGE_NAME: Final = "openvmm"
    BINARY_NAME: Final = "openvmm"
    WINDOWS_BINARY_NAME: Final = f"{BINARY_NAME}.exe"
    TARGET_DIRECTORY_NAME: Final = "target"
    BUILD_PROFILE: Final = "release"
    MUSL_SYSROOT: Final = Path(".packages") / "extracted" / "x86_64-sysroot"
    RUST_TOOLCHAIN: Final = "1.95.0"
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
    CONTROL_CONTRACT_REVISION: Final = "nvx-microvm-v2-control-v2"


class GuestRustBuildConstants:
    """Pinned inputs and outputs of the Rust guest-binary build."""

    DIRECTORY: Final = BuildConstants.REPO_ROOT / "nvx-rs"
    # The inputs of every guest executable, relative to a checkout's root: these
    # files, and every file under SOURCE_DIRECTORIES that SOURCE_PATTERN matches.
    SOURCE_FILES: Final = (
        Path("nvx-rs") / "Cargo.toml",
        Path("nvx-rs") / "Cargo.lock",
        Path("nvx-rs") / "build.rs",
        Path("rust-toolchain.toml"),
        Path("scripts") / "nvx_tools" / "build_constants.py",
        Path("scripts") / "nvx_tools" / "guest_binaries.py",
    )
    SOURCE_DIRECTORY: Final = Path("nvx-rs") / "src"
    # The Cargo configuration that a build in the crate reads, and the sources.
    SOURCE_DIRECTORIES: Final = (Path("nvx-rs") / ".cargo", SOURCE_DIRECTORY)
    # Test-only modules that no guest build compiles, besides `tests.rs` files and
    # `tests` directories.
    TEST_MODULES: Final = (
        SOURCE_DIRECTORY / "guest" / "managed_agent" / "loopback.rs",
    )
    TOOLCHAIN_FILE: Final = BuildConstants.REPO_ROOT / "rust-toolchain.toml"
    TOOLCHAIN: Final = OpenVMMBuildConstants.RUST_TOOLCHAIN
    TARGET: Final = OpenVMMBuildConstants.MUSL_RUST_TARGET
    PROFILE: Final = "guest"
    FEATURE: Final = "guest"
    BINARIES_VARIABLE: Final = "NVX_GUEST_BINARIES"
    SOURCE_DIGEST_FORMAT: Final = 2
    SOURCE_DIGEST_DOMAIN: Final = "nvx-rust-guest-binaries"
    BUILD_MANIFEST_NAME: Final = "manifest.json"
    BUILD_MANIFEST_FORMAT: Final = 1
    BUILD_MANIFEST_DOMAIN: Final = "nvx-rust-guest-binary-build"
    OUTPUT_DIRECTORY_NAME: Final = "guest-binaries"
    SOURCE_PATHS: Final = ("nvx-rs", "rust-toolchain.toml")
    BINARIES: Final = (
        "nvx-console-pending",
        "nvx-mmio-write",
        "nvx-reseed",
        "nvx-exit",
        "nvx-device-io",
        "nvx-managed-agent",
        "nvx-ingress",
        "nvx-time",
        "nvx-time-probe",
    )
    # Executables that earlier builds produced. A build removes them from the output
    # of an earlier build whose manifest records them.
    RETIRED_BINARIES: Final = ("nvx-port-io",)
    # Executables that earlier builds lacked. A build adds them to the output of an
    # earlier build whose manifest does not record them.
    ADDED_BINARIES: Final = (
        "nvx-managed-agent",
        "nvx-ingress",
        "nvx-time",
        "nvx-time-probe",
    )
    # Matches the path, relative to a checkout's root, of every input of the guest
    # executables: SOURCE_FILES, the Cargo configuration, and every source file
    # but the host side, that is, the `host` and `testing` modules and the
    # binaries other than BINARIES, and the test modules. A guest build fails when
    # Cargo's dep-info lists a file outside the set that it selects.
    SOURCE_PATTERN: Final = re.compile(
        "|".join(re.escape(path.as_posix()) for path in SOURCE_FILES)
        + r"|nvx-rs/\.cargo/.+"
        + r"|nvx-rs/src/bin/(?:"
        + "|".join(re.escape(binary) for binary in BINARIES)
        + r")\.rs"
        + r"|(?!(?:"
        + "|".join(re.escape(path.as_posix()) for path in TEST_MODULES)
        + r")$)"
        + r"nvx-rs/src/(?!bin/|(?:host|testing)(?:/|\.rs$))"
        + r"(?!(?:[^/]+/)*tests(?:/|\.rs$)).+"
    )


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
    SOURCE_MANIFEST_FORMAT: Final = 2
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
    SOURCE_OUTPUT_ENTRIES: Final = frozenset(
        ("manifest.json", "recipes", "upstream", "SHA256SUMS")
    )


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
    # The sandbox scratch template: an empty ext4 file system, of which the
    # sandbox scenarios boot a copy as their scratch. Every parameter that
    # shapes its bytes is given to mkfs.ext4, rather than taken from
    # mke2fs.conf and the defaults that distributions patch, and
    # E2FSPROGS_FAKE_TIME and the hash seed replace its clock and its random
    # seed, so e2fsprogs 1.46.5 and 1.47.0 of Debian 12, Ubuntu 22.04 and
    # 24.04, and Azure Linux 3 format the bytes of SCRATCH_TEMPLATE_SHA256.
    SCRATCH_TEMPLATE_NAME: Final = "ubuntu-smoke-scratch.ext4"
    SCRATCH_TEMPLATE_SIZE: Final = 64 * 1024 * 1024
    SCRATCH_TEMPLATE_UUID: Final = "4f3c8bf7-1959-47b4-8811-62b17bbddaf4"
    SCRATCH_TEMPLATE_BLOCK_SIZE: Final = 4096
    SCRATCH_TEMPLATE_INODE_SIZE: Final = 256
    SCRATCH_TEMPLATE_INODES: Final = 16384
    SCRATCH_TEMPLATE_RESERVED_PERCENT: Final = 0
    SCRATCH_TEMPLATE_FEATURES: Final = (
        "has_journal",
        "ext_attr",
        "resize_inode",
        "dir_index",
        "filetype",
        "extent",
        "64bit",
        "flex_bg",
        "sparse_super",
        "large_file",
        "huge_file",
        "dir_nlink",
        "extra_isize",
        "metadata_csum",
    )
    # mke2fs writes this time, in seconds since the epoch, for its clock; it
    # ignores 0.
    SCRATCH_TEMPLATE_TIMESTAMP: Final = 1
    SCRATCH_TEMPLATE_SHA256: Final = (
        "1d0d8c8994eb023d34e03d1cc6f591b223dfd576c8b8c0e07ccb8b2293f05f5f"
    )
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


class AzureLinuxBuildConstants:
    GUEST_NAME: Final = "azurelinux"
    GUEST_SOURCE_DIRECTORIES: Final = (BuildConstants.COMMON_GUEST_DIRECTORY,)
    DISTRIBUTION: Final = "Azure Linux"
    VERSION: Final = "3.0"
    ARCHITECTURE: Final = "x86_64"
    IMAGE: Final = (
        "mcr.microsoft.com/azurelinux/base/core@"
        "sha256:c877612270d1ee2d6ab2bc1f64bfe38ab697ac50be325154ee5129fce89c17e4"
    )
    REPOSITORY_URL: Final = (
        f"https://packages.microsoft.com/azurelinux/{VERSION}/prod/base/{ARCHITECTURE}"
    )
    PACKAGE_LOCK_RELATIVE_PATH: Final = Path(GUEST_NAME) / "packages.lock.json"
    PACKAGE_LOCK_FORMAT: Final = 1
    INITRAMFS_NAME: Final = "initramfs-azurelinux.cpio.gz"
    PACKAGE_MANIFEST_NAME: Final = (
        f"{INITRAMFS_NAME}{BuildConstants.PACKAGE_MANIFEST_SUFFIX}"
    )
    PACKAGE_MANIFEST_FORMAT: Final = "azurelinux-v1"
    PACKAGE_MANIFEST_VERSION: Final = 1
    INPUT_DIGEST_DOMAIN: Final = "nvx-azurelinux-initramfs-inputs"
    INPUT_DIGEST_FORMAT: Final = 1


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


class DockerBuildConstants:
    DOCKERFILE: Final = Path("docker") / "Dockerfile"
    RUST_BUILDER_IMAGE: Final = (
        f"rust:{GuestRustBuildConstants.TOOLCHAIN}-slim-bookworm@"
        "sha256:d7482085ff5b415f84dba5647ae71606650bdef00db7aeb69f4b3d170c3e4082"
    )
    ALPINE_TARGET: Final = "artifacts"
    UBUNTU_TARGET: Final = "ubuntu-guest-artifacts"
    AZURELINUX_TARGET: Final = "azurelinux-guest-artifacts"
    AZURELINUX_INITRAMFS_TARGET: Final = "azurelinux-initramfs-artifacts"
    ALL_GUESTS_TARGET: Final = "all-guest-artifacts"
    DEBUG_KERNEL_TARGET: Final = "kernel-debug-artifacts"
    LINUX_SOURCE_TARGET: Final = "linux-source-artifacts"
    OUTPUT_TYPE: Final = "local"
    DEBUG_KERNEL_ARTIFACT_NAMES: Final = (
        KernelBuildConstants.DEBUG_BINARY_NAME,
        KernelBuildConstants.DEBUG_CONFIG_NAME,
        KernelBuildConstants.DEBUG_PROVENANCE_NAME,
    )
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
        UbuntuBuildConstants.SCRATCH_TEMPLATE_NAME,
        AzureLinuxBuildConstants.INITRAMFS_NAME,
        AzureLinuxBuildConstants.PACKAGE_MANIFEST_NAME,
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
        "nvx-rs",
        "ubuntu",
        "azurelinux",
        "data/linux-kvm-virtual-machine.csv",
        "data/linux-mshv-virtual-machine.csv",
        "data/windows-whp-virtual-machine.csv",
        "docker",
        "kernel",
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
        "rust-toolchain.toml",
    )
    PROJECT_SOURCE_EXCLUDES: Final = (Path("nvx-rs") / "target",)
    GUEST_ARTIFACT_NAMES: Final = (
        KernelBuildConstants.BINARY_NAME,
        KernelBuildConstants.CONFIG_NAME,
        AlpineBuildConstants.INITRAMFS_NAME,
        AlpineBuildConstants.PACKAGE_MANIFEST_NAME,
        UbuntuBuildConstants.INITRAMFS_NAME,
        UbuntuBuildConstants.PACKAGE_MANIFEST_NAME,
        UbuntuBuildConstants.DISTRO_NAME,
        UbuntuBuildConstants.DISTRO_MANIFEST_NAME,
        UbuntuBuildConstants.SCRATCH_TEMPLATE_NAME,
        AzureLinuxBuildConstants.INITRAMFS_NAME,
        AzureLinuxBuildConstants.PACKAGE_MANIFEST_NAME,
    )
    AZURELINUX_ARTIFACT_NAMES: Final = (
        AzureLinuxBuildConstants.INITRAMFS_NAME,
        AzureLinuxBuildConstants.PACKAGE_MANIFEST_NAME,
    )
    TAR_TIMESTAMP: Final = 0
    ZIP_TIMESTAMP: Final = (1980, 1, 1, 0, 0, 0)
    ZIP_COMPRESSION_LEVEL: Final = 9
