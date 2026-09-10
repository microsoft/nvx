"""Linux-native and Docker-backed artifact build workflows."""

from __future__ import annotations

import base64
import gzip
import hashlib
import json
import os
import re
import shutil
import ssl
import stat
import struct
import sys
import tarfile
import tempfile
import zlib
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
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
    "CONFIG_CGROUPS=y",
    "CONFIG_CGROUP_BPF=y",
    "CONFIG_CGROUP_FREEZER=y",
    "CONFIG_CGROUP_PIDS=y",
    "CONFIG_CGROUP_SCHED=y",
    "CONFIG_DEVTMPFS=y",
    "CONFIG_EFI_PARTITION=y",
    "CONFIG_EROFS_FS=y",
    "CONFIG_EROFS_FS_ZIP=y",
    "CONFIG_EROFS_FS_ZIP_ZSTD=y",
    "CONFIG_EXT4_FS=y",
    "CONFIG_FAIR_GROUP_SCHED=y",
    "# CONFIG_CFS_BANDWIDTH is not set",
    "CONFIG_MEMCG=y",
    "CONFIG_NAMESPACES=y",
    "CONFIG_UTS_NS=y",
    "CONFIG_IPC_NS=y",
    "CONFIG_PID_NS=y",
    "CONFIG_OVERLAY_FS=y",
    "# CONFIG_OVERLAY_FS_REDIRECT_ALWAYS_FOLLOW is not set",
    "CONFIG_PROC_FS=y",
    "CONFIG_SECCOMP=y",
    "CONFIG_SECCOMP_FILTER=y",
    "CONFIG_SYSFS=y",
    "CONFIG_TMPFS=y",
    "CONFIG_UNIX=y",
    "CONFIG_UNIX98_PTYS=y",
    "CONFIG_VIRTIO_BLK=y",
    "CONFIG_VIRTIO_CONSOLE=y",
    "CONFIG_VIRTIO_MMIO=y",
    "CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y",
)
GUEST_AGENT_ARTIFACT_NAME = "nvx-agent"
GUEST_AGENT_SHA256_NAME = f"{GUEST_AGENT_ARTIFACT_NAME}.sha256"
AGENT_INITRAMFS_NAME = "initramfs-agent.cpio.gz"
GUEST_AGENT_ARTIFACT_PATH = f"guest/{GUEST_AGENT_ARTIFACT_NAME}"
GUEST_AGENT_INITRAMFS_ARTIFACT_PATH = f"guest/{AGENT_INITRAMFS_NAME}"
GUEST_AGENT_INSTALLED_PATH = f"/sbin/{GUEST_AGENT_ARTIFACT_NAME}"
GUEST_AGENT_TARGET = "x86_64-unknown-linux-musl"
GUEST_AGENT_MAXIMUM_BYTES = 16 * 1024 * 1024
GUEST_AGENT_SOURCE_REVISION = "b1b5ce6b16d4f98c6a41d751f033c4a46e03804e"
GUEST_AGENT_SHA256 = "a2a431c228f694070ff87c638ccfaae5331028c7ac1a79ea2bee9b151de35633"
GUEST_AGENT_SIZE_BYTES = 1_909_632
GUEST_AGENT_BUILD_ID = "1284802f47b24e4429ad0f0469c54da3ca6f37ff"
BROKER_TRANSPORT = "broker-ttrpc"
GUEST_AGENT_PROTOCOL_SCHEMA_VERSION = 1
MICROVM_ABI_VERSION = 2
CONTROL_SESSION_PROTOCOL_VERSION = 1
CONTROL_CONTRACT_REVISION = "nvx-microvm-v2-control-v1"
OPENVMM_PROVENANCE_NAME = "openvmm.provenance.json"
KERNEL_PROVENANCE_NAME = "vmlinux.provenance.json"
UNSAFE_INITRAMFS_FILESYSTEMS = frozenset(
    {
        "9p",
        "cifs",
        "drvfs",
        "exfat",
        "fuseblk",
        "msdos",
        "ntfs",
        "smb2",
        "v9fs",
        "vfat",
    }
)
INITRAMFS_MAX_COMPRESSED_BYTES = 512 * 1024 * 1024
INITRAMFS_MAX_UNCOMPRESSED_BYTES = 1024 * 1024 * 1024
INITRAMFS_MAX_MEMBERS = 200_000
INITRAMFS_MAX_MEMBER_BYTES = 512 * 1024 * 1024
INITRAMFS_MAX_NAME_BYTES = 4096
# Exact path/target pairs from the pinned Alpine rootfs after required APK installs.
_TRUSTED_INITRAMFS_SYMLINK_COUNT = 341
_TRUSTED_INITRAMFS_SYMLINKS_SHA256 = (
    "231e5e9d56f95831135f69559ec65cebf0ed111433176214be7a16b2bff76e32"
)
_TRUSTED_INITRAMFS_SYMLINKS_B85 = (
    "c-qBU+ji=@4t-|*3jqS8?c4c}^U&eg4)IB1d)W@RuRn5tKoW_TcI>f7*Cm-#GO{dLvT3gLf;nE!7CAiStv}`F_$N"
    "LykAOMzGTR_{&W$eAZpv^L5U^w+I1Jx8i@}W%7;Fw2fsOX|G{EQb7##B#8u<Xhf<#c{Gev=5+6&V#r8R4?VcN!44"
    "MRRw!VC~}=OiLru-Z!u4!AE1QPeHAh6ORcKWRRNap7I7&kDm)v@|~8zEQwn0irBtHDXPbi%*CJWTZ;f^%Sj@u28f"
    "+uG86~^{ykm3^57rWQ=i?MB=SBJB7g>!|=7L&jxW@!z$S*%xz62l*3?+<w;Wo*<j;a-1*IZifI{eVXZh~pjE;Y7_"
    "4_Vnqjb&pVk?OK#%y+I;Dfuh^-crJ|K>9vM>(!uxl_3^}1{I+%gxCPxVq@VDQo@hJ-7YuD+I72I8yXLpUL5i(qKB"
    "L!=|1X|VzkT#Goa#S^_Hrg2~a8~Z#d8#3@k6Ieb=5?FczFQ(POcewF$Cu<3{DY7wdYn4BsosjqSf=d??E1Ow-@KB"
    "rP@j(>|K2G|CyKoa-C9AviVyr)w@<Wk}RY5{*>B21bi+xC!%NhKeF8@T+;4e=a@X7r6o_%JU#G7O_tHpeb{qbEX="
    "8#WAcU*AszW~vD#Heu{?NA}@q>^?-k$YQDD*lD?yh@;crZbtw=OMaA(aiwe$w^dpc**L3z^cG{i7O>0CI3oD?Q6D"
    "|DA!`XP+Fm%t_=jlNuZR+y{FLeEfd0_#I!112`$yGL>Xt=AaSOxaU!%@%Nn67s94xEYQw~|QMppE3o}%S@JS=&9)"
    "l}keBn{1@RvAisu(>(bd92$DY`Ehwya=e%tf35@8*~f<5s2$j4(*i?-*+i(wPENa!d)<1_Sj1V`Xj(X1Qx{2U8IN"
    "<t28&28{~Hkq{WiV$7dB4>e8@0Rs_+tF)rzmLtTTOzo}^2!l~)K2ES(YxZ9PCUADBa3UUSrgf`lIe||qTF-RtaY~"
    "TW;&!J<Ene;kC0_ZqAt)$+j-VRCc5WJKG^-53IHO~2CdiWw_CSy=F)KkhT}W2fhEvWEYMgs_#?ea*n^M|pbtKr9!"
    "W-K)7#=-s-oz$NYEK+3(}LuPl`9yBO@!bSHC|zAigA+gN)as7?x%AD5!-$t(n7n7f4~klv2nM4<4VHZa#DwS1`d;"
    "o;MDp(o83-wJxB>_=&r}d$r~9Dnj{H56cL#9<Pg&{-V_wmdLpT6dK0P74Wk%iNwr37$$g|zkm0Z~5NuVb!cd%L-X"
    "91@Ya+=t;a=UyK*f?aWlL;nIvyAZ3Tm6FC`CiqZ7k$8;m2q51uhV|W~Ub|JvF`C=&4D&)-&;VOvIJ-oN0;*<aPjO"
    "Xkvqmz2UT}ePC<LLP@b+29i=|!Wb|AQbefai5nq_eDft+k&zB$y3L-^>u2P4l|G@(0W70PCj~{d#5#K82+zT3PEk"
    "~V$k<W7p^S&7ks=fkn}J)u7f(w=ph7LkT7OY_;J{AIFhEgyuBxa$ATow(NrEAAEjS8iTq1q?l+P#u(m`;F;EWtNo"
    "JfLBsY};Rn9|+rq|+MO>StXH^i+RI=9og;MX~gD0vuW4B1f%KtB+@jIa4Q$*3!Y!wnFmBW%Qkunr8Qkz@fJ#=Djy"
    "H-M}7*^gQgS&Gk?!UQ(1H3Dzi*Q^aasF0>}0b7YR%hmJ=gppJCkbHX<K>up<JS4v!O*&g5EXZa|{nVsuhq?WVcYy"
    "25Vn&wkF7rAscdQVo5lENAL$(meA9r}?c;COtQ+J5ZYbFv;t#Itli;QcLmAITj<TKM~5_t#|eC|3yOtKLWVx3Q19"
    "i`QQ6{x+7=ymfFiIz%#?je#V4l(nqsXL5T^-nT<X0`b`>&mSf4K=PhW#0R2;n6H?FT60~v?Q9B-m&P~D1#W8JJ7I"
    "{79pUfJzdjE6HZ;~>jO#{wd{5TH#cbP$IPPCSYB1ut7dhYfwBxVW#$xVEiS?8>zB`!FG=8GHT|7^|<mt)WP40_&j"
    "+a#De5Kvwe(X*<qbWrp>u0k8`pInKSG!~@(MiZqN;f~SN)kg9Kq&!f{>4B=j`qR+ddmJny9{57C<y5<M6PGsEn!B"
    "yATEritklp3#4C{}^ueh>IZEeZ&g-}#QUc>C-l_Qg8*+==^G>ipb8=5naIRtag~OF^%w9fCCNH&U&>s1ATl>VLz}"
    "99LuX723OP~D3s)nz_r)&M5$dt^<1+&!x4lsVfVj0gi3pmz6P|d-Avi?u4eDKKy>b^ryjN8q(@2igw!Fl35OKjVH"
    "efI;kUs{o^GA7sC<%?E~)le&3Qx@NvQo0f8Tb{q_iILi`Y={&3cuO}w@WZ1TVZ;9I-}l4!Y={#Qh0K58hetKchT0"
    "gtdm%EjoTbY`zG}%x4YQ<C)_*feW%j|@%jq#z!;G+Spxo5*dpXbZeDjvQyj2X<erZKq<D94KAB4uE8fJxqR(1c)u"
    "vjvlE<ZkAwPK|9TPs$X$k$8px-SN5m=%8gu{?fndF7^9y@{8jVyN~j8{*>fb<Tef6OU?`4RAF6&QKecvE}>b)eIf"
    "0{lWs+f7i0u{9xN2sQtnMaA%gub@BQt4Ack<KEIa||Lj71(}-=Ft+?c`uf<U9cSe9;w^D9DUSE!Z+MhjxV{5$h0)"
    "5x&?zscw?@3&+?$ujk>h7d>`?}dj`tm3KzSRwVknS39pyU7l2X5F9X#"
)


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


@dataclass(frozen=True)
class InitramfsEntry:
    order: int
    name: str
    inode: int
    mode: int
    file_type: int
    uid: int
    gid: int
    nlink: int
    devmajor: int
    devminor: int
    rdevmajor: int
    rdevminor: int
    data_start: int
    data_end: int
    data_sha256: str
    data: bytes
    symlink_target: str | None

    @property
    def identity(self) -> tuple[int, int, int]:
        return (self.devmajor, self.devminor, self.inode)


def _load_trusted_initramfs_symlinks() -> frozenset[tuple[str, str]]:
    try:
        raw = zlib.decompress(
            base64.b85decode(_TRUSTED_INITRAMFS_SYMLINKS_B85.encode("ascii"))
        )
    except (ValueError, zlib.error) as error:
        raise ScriptError("trusted initramfs symlink baseline is corrupt") from error
    if hashlib.sha256(raw).hexdigest() != _TRUSTED_INITRAMFS_SYMLINKS_SHA256:
        raise ScriptError("trusted initramfs symlink baseline has the wrong SHA-256")
    parsed_links: list[tuple[str, str]] = []
    for line in raw.decode("utf-8").splitlines():
        name, separator, target = line.partition("\t")
        if not separator:
            raise ScriptError("trusted initramfs symlink baseline has invalid entries")
        parsed_links.append((name, target))
    links = frozenset(parsed_links)
    if len(links) != _TRUSTED_INITRAMFS_SYMLINK_COUNT:
        raise ScriptError("trusted initramfs symlink baseline has invalid entries")
    return links


TRUSTED_INITRAMFS_SYMLINKS = _load_trusted_initramfs_symlinks()


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
    if expected_sha256 != GUEST_AGENT_SHA256:
        raise ScriptError(
            "NVX guest-agent SHA-256 does not match the required external input: "
            f"{expected_sha256}, expected {GUEST_AGENT_SHA256}"
        )
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
            shutil.copyfileobj(input_file, output_file)
            temporary_path = Path(output_file.name)
        actual_sha256 = sha256_file(temporary_path)
        if actual_sha256 != expected_sha256:
            raise ScriptError(
                f"NVX guest agent SHA-256 is {actual_sha256}, expected {expected_sha256}"
            )
        size = temporary_path.stat().st_size
        if size != GUEST_AGENT_SIZE_BYTES:
            raise ScriptError(
                f"NVX guest-agent size is {size} bytes, "
                f"expected {GUEST_AGENT_SIZE_BYTES} bytes"
            )
        if size > GUEST_AGENT_MAXIMUM_BYTES:
            raise ScriptError(
                f"NVX guest agent exceeds the 16-MiB release limit: {size} bytes"
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


def verified_staged_guest_agent() -> tuple[Path, str]:
    source = require_file(
        REPO_ROOT / "build" / GUEST_AGENT_ARTIFACT_NAME,
        "staged NVX guest agent",
    )
    pin = require_file(
        REPO_ROOT / "build" / GUEST_AGENT_SHA256_NAME,
        "staged NVX guest-agent SHA-256 pin",
    )
    expected = pin.read_text(encoding="ascii").strip().lower()
    if re.fullmatch(r"[0-9a-f]{64}", expected) is None:
        raise ScriptError("staged NVX guest-agent SHA-256 pin is invalid")
    if expected != GUEST_AGENT_SHA256:
        raise ScriptError(
            "staged NVX guest-agent SHA-256 pin does not match the required "
            f"external input: {expected}, expected {GUEST_AGENT_SHA256}"
        )
    actual = sha256_file(source)
    if actual != expected:
        raise ScriptError(
            f"staged NVX guest-agent SHA-256 is {actual}, expected {expected}"
        )
    size = source.stat().st_size
    if size != GUEST_AGENT_SIZE_BYTES:
        raise ScriptError(
            f"staged NVX guest-agent size is {size} bytes, "
            f"expected {GUEST_AGENT_SIZE_BYTES} bytes"
        )
    if size > GUEST_AGENT_MAXIMUM_BYTES:
        raise ScriptError(
            f"staged NVX guest agent exceeds the 16-MiB release limit: {size} bytes"
        )
    validate_static_x86_64_elf(source)
    return source, expected


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
    agent_enabled: bool = False


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
            "input_config_sha256": sha256_file(
                REPO_ROOT / "kernel" / "config-microvm"
            ),
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
    root = Path(tempfile.mkdtemp(prefix="root-", dir=config.work))
    require_tool("tar")
    run_checked(
        [
            "tar",
            "--extract",
            "--gzip",
            "--file",
            tarball,
            "--directory",
            root,
            "--numeric-owner",
            "--no-same-owner",
            "--same-permissions",
        ]
    )
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
    certificates = root / "etc" / "ssl" / "certs" / "ca-certificates.crt"
    if certificates.is_file():
        environment.setdefault("SSL_CERT_FILE", str(certificates))
        environment.setdefault("SSL_CERT_DIR", str(certificates.parent))
    else:
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


def native_initramfs_work_directory(profile: str) -> Path:
    configured = os.environ.get("NVX_NATIVE_WORK_DIR")
    base = (
        Path(configured).expanduser()
        if configured
        else Path.home() / ".cache" / "nvx" / "native-work"
    )
    return (base / profile).resolve()


def _prepare_agent_root(work: Path, agent_source: Path) -> Path:
    root = Path(tempfile.mkdtemp(prefix="agent-root-", dir=work))
    root.chmod(0o755)
    sbin = root / "sbin"
    sbin.mkdir(mode=0o755)
    agent = sbin / GUEST_AGENT_ARTIFACT_NAME
    shutil.copyfile(agent_source, agent)
    agent.chmod(0o755)
    (root / "init").symlink_to(f"sbin/{GUEST_AGENT_ARTIFACT_NAME}")
    return root


def _linux_filesystem_type(path: Path) -> str:
    result = run_capture(["stat", "-f", "-c", "%T", path])
    require_success(result, f"filesystem query for {path}")
    return result.stdout.decode("ascii", errors="replace").strip().lower()


def _require_metadata_preserving_work_directory(work: Path) -> Path:
    work.mkdir(parents=True, exist_ok=True)
    filesystem_type = _linux_filesystem_type(work)
    if filesystem_type in UNSAFE_INITRAMFS_FILESYSTEMS:
        raise ScriptError(
            f"initramfs work directory {work} is on {filesystem_type}, which cannot "
            "reliably preserve Unix modes and ownership; set NVX_NATIVE_WORK_DIR to "
            "a native Linux filesystem and rerun the native build"
        )
    probe = Path(tempfile.mkdtemp(prefix=".nvx-metadata-probe-", dir=work))
    try:
        probe.chmod(0o1701)
        file_probe = probe / "file"
        file_probe.write_bytes(b"mode probe")
        file_probe.chmod(0o640)
        symlink_probe = probe / "link"
        symlink_probe.symlink_to("file")
        directory_stat = probe.lstat()
        file_stat = file_probe.lstat()
        symlink_stat = symlink_probe.lstat()
        if (
            stat.S_IMODE(directory_stat.st_mode) != 0o1701
            or stat.S_IMODE(file_stat.st_mode) != 0o640
            or not stat.S_ISLNK(symlink_stat.st_mode)
        ):
            raise ScriptError(
                f"initramfs work directory {work} did not preserve a Unix metadata "
                "probe; set NVX_NATIVE_WORK_DIR to a native Linux filesystem"
            )
    except OSError as error:
        shutil.rmtree(probe, ignore_errors=True)
        raise ScriptError(
            f"initramfs work directory {work} cannot preserve required Unix "
            f"metadata: {error}; set NVX_NATIVE_WORK_DIR to a native Linux filesystem"
        ) from error
    return probe


def _trusted_alpine_owners(config: AlpineBuildConfig) -> dict[str, tuple[int, int]]:
    owners: dict[str, tuple[int, int]] = {}
    with tarfile.open(_alpine_tarball(config), "r:gz") as archive:
        for member in archive:
            name = member.name.removeprefix("./").rstrip("/") or "."
            canonical = _canonical_newc_name(_alpine_tarball(config), name)
            if canonical in owners:
                raise ScriptError(
                    f"Alpine minirootfs contains duplicate member {canonical!r}"
                )
            owners[canonical] = (member.uid, member.gid)
    return owners


def _pack_initramfs(
    root: Path,
    output: Path,
    trusted_owners: dict[str, tuple[int, int]],
) -> None:
    _normalize_initramfs_metadata(root)
    output.parent.mkdir(parents=True, exist_ok=True)
    paths = [root, *root.rglob("*")]
    paths.sort(
        key=lambda path: os.fsencode(
            "." if path == root else path.relative_to(root).as_posix()
        )
    )
    try:
        with output.open("wb") as raw:
            with gzip.GzipFile(
                filename="",
                mode="wb",
                fileobj=raw,
                compresslevel=9,
                mtime=0,
            ) as archive:
                written = 0

                def write(data: bytes) -> None:
                    nonlocal written
                    archive.write(data)
                    written += len(data)

                for inode, path in enumerate(paths, start=1):
                    metadata = path.lstat()
                    name = "." if path == root else path.relative_to(root).as_posix()
                    name_bytes = name.encode("utf-8") + b"\0"
                    file_type = stat.S_IFMT(metadata.st_mode)
                    if file_type == stat.S_IFREG:
                        content = path.read_bytes()
                    elif file_type == stat.S_IFLNK:
                        content = os.readlink(path).encode("utf-8")
                    elif file_type == stat.S_IFDIR:
                        content = b""
                    else:
                        raise ScriptError(
                            f"unsupported initramfs entry type for {path}"
                        )
                    uid, gid = trusted_owners.get(name, (0, 0))
                    fields = (
                        inode,
                        metadata.st_mode,
                        uid,
                        gid,
                        1,
                        0,
                        len(content),
                        0,
                        0,
                        0,
                        0,
                        len(name_bytes),
                        0,
                    )
                    write(
                        b"070701"
                        + b"".join(
                            f"{field & 0xFFFFFFFF:08x}".encode("ascii")
                            for field in fields
                        )
                    )
                    write(name_bytes)
                    write(b"\0" * (-written % 4))
                    write(content)
                    write(b"\0" * (-written % 4))

                trailer_name = b"TRAILER!!!\0"
                trailer = (
                    len(paths) + 1,
                    0,
                    0,
                    0,
                    1,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    len(trailer_name),
                    0,
                )
                write(
                    b"070701"
                    + b"".join(
                        f"{field & 0xFFFFFFFF:08x}".encode("ascii") for field in trailer
                    )
                )
                write(trailer_name)
                write(b"\0" * (-written % 4))
                write(b"\0" * (-written % 512))
    except OSError:
        output.unlink(missing_ok=True)
        raise


def _write_apk_manifest(
    root: Path,
    output: Path,
    config: AlpineBuildConfig,
    helpers: dict[str, dict[str, str]],
    agent_sha256: str | None,
) -> None:
    packages: list[ApkPackage] = []
    installed = root / "lib" / "apk" / "db" / "installed"
    if installed.is_file():
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
    elif not config.agent_enabled:
        raise ScriptError(f"legacy initramfs root lacks APK metadata: {installed}")
    packages.sort(key=lambda package: package["name"])
    manifest = output.with_name(f"{output.name}.packages.json")
    manifest.write_text(
        json.dumps(
            {
                "format": 1,
                "alpine_version": config.version,
                "alpine_branch": config.branch,
                "architecture": "x86_64",
                "profile": "broker-ttrpc" if config.agent_enabled else "legacy",
                "guest_agent": (
                    {
                        "path": GUEST_AGENT_INSTALLED_PATH,
                        "sha256": agent_sha256,
                        "size": GUEST_AGENT_SIZE_BYTES,
                        "source_revision": GUEST_AGENT_SOURCE_REVISION,
                        "build_id": GUEST_AGENT_BUILD_ID,
                    }
                    if agent_sha256 is not None
                    else None
                ),
                "packages": packages,
                "helpers": helpers,
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )


def _bind_apk_manifest_to_initramfs(output: Path) -> None:
    manifest_path = output.with_name(f"{output.name}.packages.json")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    manifest["artifact"] = {
        "name": output.name,
        "sha256": sha256_file(output),
        "size": output.stat().st_size,
    }
    manifest_path.write_text(
        json.dumps(manifest, indent=2) + "\n",
        encoding="utf-8",
    )


def _newc_entries(path: Path) -> tuple[InitramfsEntry, ...]:
    if path.stat().st_size > INITRAMFS_MAX_COMPRESSED_BYTES:
        raise ScriptError(f"{path} exceeds the compressed initramfs size limit")
    try:
        with gzip.open(path, "rb") as compressed:
            data = compressed.read(INITRAMFS_MAX_UNCOMPRESSED_BYTES + 1)
    except (gzip.BadGzipFile, EOFError, OSError) as error:
        raise ScriptError(f"{path} is not a valid gzip initramfs: {error}") from error
    if len(data) > INITRAMFS_MAX_UNCOMPRESSED_BYTES:
        raise ScriptError(f"{path} exceeds the uncompressed initramfs size limit")
    entries: list[InitramfsEntry] = []
    names: set[str] = set()
    casefolded_names: dict[str, str] = {}
    offset = 0
    found_trailer = False
    while offset < len(data):
        if bytes(data[offset : offset + 6]) != b"070701":
            raise ScriptError(f"{path} is not a reproducible newc initramfs")
        header = data[offset + 6 : offset + 110]
        if len(header) != 104:
            raise ScriptError(f"{path} has a truncated newc header")
        try:
            fields = [int(header[index : index + 8], 16) for index in range(0, 104, 8)]
        except ValueError as error:
            raise ScriptError(f"{path} has an invalid newc header") from error
        file_size = fields[6]
        name_size = fields[11]
        if file_size > INITRAMFS_MAX_MEMBER_BYTES:
            raise ScriptError(f"{path} contains an oversized newc member")
        if name_size > INITRAMFS_MAX_NAME_BYTES:
            raise ScriptError(f"{path} contains an oversized newc member name")
        name_start = offset + 110
        name_end = name_start + name_size
        if name_size < 1 or name_end > len(data) or data[name_end - 1] != 0:
            raise ScriptError(f"{path} has an invalid newc member name")
        try:
            name = data[name_start : name_end - 1].decode("utf-8")
        except UnicodeDecodeError as error:
            raise ScriptError(
                f"{path} contains a non-UTF-8 newc member name"
            ) from error
        content_start = (name_end + 3) & ~3
        content_end = content_start + file_size
        if content_end > len(data):
            raise ScriptError(f"{path} has a truncated newc member")
        if name == "TRAILER!!!":
            if file_size != 0:
                raise ScriptError(f"{path} has a newc trailer with data")
            next_offset = (content_end + 3) & ~3
            if any(data[next_offset:]):
                raise ScriptError(
                    f"{path} contains a second archive after the newc trailer"
                )
            found_trailer = True
            break
        canonical_name = _canonical_newc_name(path, name)
        if name != canonical_name:
            raise ScriptError(f"{path} contains non-canonical newc member {name!r}")
        if canonical_name in names:
            raise ScriptError(
                f"{path} contains duplicate newc member {canonical_name!r}"
            )
        names.add(canonical_name)
        casefolded = canonical_name.casefold()
        if previous_name := casefolded_names.get(casefolded):
            raise ScriptError(
                f"{path} contains case-colliding newc members "
                f"{previous_name!r} and {canonical_name!r}"
            )
        casefolded_names[casefolded] = canonical_name
        member_data = data[content_start:content_end]
        file_type = stat.S_IFMT(fields[1])
        symlink_target: str | None = None
        if file_type == stat.S_IFLNK:
            try:
                symlink_target = member_data.decode("utf-8")
            except UnicodeDecodeError as error:
                raise ScriptError(
                    f"{path} contains non-UTF-8 symlink {canonical_name!r}"
                ) from error
        entries.append(
            InitramfsEntry(
                order=len(entries),
                name=canonical_name,
                inode=fields[0],
                mode=fields[1],
                file_type=file_type,
                uid=fields[2],
                gid=fields[3],
                nlink=fields[4],
                devmajor=fields[7],
                devminor=fields[8],
                rdevmajor=fields[9],
                rdevminor=fields[10],
                data_start=content_start,
                data_end=content_end,
                data_sha256=hashlib.sha256(member_data).hexdigest(),
                data=member_data,
                symlink_target=symlink_target,
            )
        )
        if len(entries) > INITRAMFS_MAX_MEMBERS:
            raise ScriptError(f"{path} exceeds the newc member-count limit")
        offset = (content_end + 3) & ~3
    if not found_trailer:
        raise ScriptError(f"{path} has no newc trailer")
    return tuple(entries)


def _canonical_newc_name(path: Path, name: str) -> str:
    if name == ".":
        return name
    canonical = name.removeprefix("./")
    parts = canonical.split("/")
    if (
        not canonical
        or name.startswith("/")
        or "\0" in canonical
        or "\\" in canonical
        or any(part in ("", ".", "..") for part in parts)
    ):
        raise ScriptError(f"{path} contains non-canonical newc member {name!r}")
    return canonical


def _resolve_initramfs_symlink(path: Path, name: str, target: str) -> str:
    if not target or "\0" in target or "\\" in target:
        raise ScriptError(f"{path} contains unsafe symlink {name!r} -> {target!r}")
    target_path = PurePosixPath(target)
    resolved: list[str] = [] if target_path.is_absolute() else name.split("/")[:-1]
    for part in target_path.parts:
        if part in ("", ".", "/"):
            continue
        if part == "..":
            if not resolved:
                raise ScriptError(
                    f"{path} contains escaping symlink {name!r} -> {target!r}"
                )
            resolved.pop()
        else:
            resolved.append(part)
    return "/".join(resolved)


def _validated_initramfs_entries(
    path: Path,
    *,
    agent_profile: bool,
) -> dict[str, InitramfsEntry]:
    ordered_entries = _newc_entries(path)
    entries: dict[str, InitramfsEntry] = {}
    identities: dict[tuple[int, int, int], InitramfsEntry] = {}
    allowed_types = (stat.S_IFDIR, stat.S_IFREG, stat.S_IFLNK)
    allowed_symlinks = TRUSTED_INITRAMFS_SYMLINKS
    if agent_profile:
        allowed_symlinks |= frozenset({("init", "sbin/nvx-agent")})
    for entry in ordered_entries:
        name = entry.name
        file_type = entry.file_type
        permissions = stat.S_IMODE(entry.mode)
        if name == ".":
            if entry.order != 0:
                raise ScriptError(f"{path} root newc member is not first")
        else:
            parts = name.split("/")
            for index in range(1, len(parts)):
                ancestor_name = "/".join(parts[:index])
                ancestor = entries.get(ancestor_name)
                if ancestor is None:
                    raise ScriptError(
                        f"{path} contains {name!r} before directory ancestor "
                        f"{ancestor_name!r}"
                    )
                if ancestor.file_type != stat.S_IFDIR:
                    raise ScriptError(
                        f"{path} contains {name!r} below non-directory ancestor "
                        f"{ancestor_name!r}"
                    )
        if file_type not in allowed_types:
            raise ScriptError(f"{path} contains unsafe entry type for {name!r}")
        if entry.nlink < 1:
            raise ScriptError(f"{path} contains invalid link count for {name!r}")
        if previous := identities.get(entry.identity):
            raise ScriptError(
                f"{path} contains repeated newc inode identity for "
                f"{previous.name!r} and {name!r}"
            )
        identities[entry.identity] = entry
        if file_type == stat.S_IFLNK:
            if permissions != 0o777:
                raise ScriptError(f"{path} contains symlink {name!r} with wrong mode")
            if entry.nlink != 1:
                raise ScriptError(
                    f"{path} contains symlink {name!r} with unsafe link count"
                )
            target = entry.symlink_target
            if target is None:
                raise ScriptError(f"{path} contains invalid symlink {name!r}")
            resolved_target = _resolve_initramfs_symlink(path, name, target)
            if (name, target) not in allowed_symlinks:
                raise ScriptError(
                    f"{path} contains untrusted symlink {name!r} -> {target!r}"
                )
            if resolved_target in ("init", "sbin/nvx-agent") and (
                name,
                target,
            ) != ("init", "sbin/nvx-agent"):
                raise ScriptError(
                    f"{path} contains symlink alias to security-critical "
                    f"{resolved_target!r}"
                )
        elif file_type == stat.S_IFREG:
            if entry.nlink != 1:
                raise ScriptError(
                    f"{path} contains regular file {name!r} with unsafe link count"
                )
            if permissions & (stat.S_ISUID | stat.S_ISGID | stat.S_ISVTX | 0o002):
                raise ScriptError(
                    f"{path} contains unsafe regular-file mode for {name!r}"
                )
            if entry.data.startswith(b"\x7fELF") and permissions != 0o755:
                raise ScriptError(f"{path} contains ELF binary {name!r} without 0755")
        else:
            if permissions & (stat.S_ISUID | stat.S_ISGID):
                raise ScriptError(f"{path} contains unsafe directory mode for {name!r}")
            if permissions & 0o002 and permissions != 0o1777:
                raise ScriptError(
                    f"{path} contains unsafe world-writable directory {name!r}"
                )
            if entry.data:
                raise ScriptError(f"{path} contains directory {name!r} with data")
        entries[name] = entry

    critical = (
        {
            ".": (stat.S_IFDIR, 0o755, 0, 0),
            "sbin": (stat.S_IFDIR, 0o755, 0, 0),
        }
        if agent_profile
        else {
            ".": (stat.S_IFDIR, 0o755, 0, 0),
            "bin": (stat.S_IFDIR, 0o755, 0, 0),
            "bin/busybox": (stat.S_IFREG, 0o755, 0, 0),
            "etc": (stat.S_IFDIR, 0o755, 0, 0),
            "etc/group": (stat.S_IFREG, 0o644, 0, 0),
            "etc/passwd": (stat.S_IFREG, 0o644, 0, 0),
            "etc/shadow": (stat.S_IFREG, 0o640, 0, 42),
            "root": (stat.S_IFDIR, 0o700, 0, 0),
            "sbin": (stat.S_IFDIR, 0o755, 0, 0),
            "sbin/apk": (stat.S_IFREG, 0o755, 0, 0),
            "tmp": (stat.S_IFDIR, 0o1777, 0, 0),
            "var": (stat.S_IFDIR, 0o755, 0, 0),
            "var/tmp": (stat.S_IFDIR, 0o1777, 0, 0),
        }
    )
    for name, (file_type, permissions, uid, gid) in critical.items():
        try:
            entry = entries[name]
        except KeyError as error:
            raise ScriptError(
                f"{path} is missing critical rootfs entry {name!r}"
            ) from error
        actual = (
            stat.S_IFMT(entry.mode),
            stat.S_IMODE(entry.mode),
            entry.uid,
            entry.gid,
        )
        if actual != (file_type, permissions, uid, gid):
            raise ScriptError(
                f"{path} has incorrect metadata for {name!r}: "
                f"mode={entry.mode:#o} uid={entry.uid} gid={entry.gid}"
            )
    return entries


def verify_agent_initramfs(path: Path, expected_sha256: str) -> None:
    entries = _validated_initramfs_entries(path, agent_profile=True)
    expected_entries = {".", "init", "sbin", "sbin/nvx-agent"}
    if set(entries) != expected_entries:
        raise ScriptError(
            f"{path} agent profile contains unexpected entries: "
            f"{sorted(set(entries) - expected_entries)}"
        )
    try:
        agent_entry = entries["sbin/nvx-agent"]
        init_entry = entries["init"]
    except KeyError as error:
        raise ScriptError(f"{path} does not contain the NVX PID-1 layout") from error
    if (
        stat.S_IFMT(agent_entry.mode) != stat.S_IFREG
        or stat.S_IMODE(agent_entry.mode) != 0o755
        or agent_entry.uid != 0
        or agent_entry.gid != 0
        or agent_entry.nlink != 1
    ):
        raise ScriptError(f"{path} contains /sbin/nvx-agent with the wrong mode")
    actual_sha256 = agent_entry.data_sha256
    if actual_sha256 != expected_sha256:
        raise ScriptError(
            f"embedded NVX guest-agent SHA-256 is {actual_sha256}, "
            f"expected {expected_sha256}"
        )
    if (
        stat.S_IFMT(init_entry.mode) != stat.S_IFLNK
        or stat.S_IMODE(init_entry.mode) != 0o777
        or init_entry.uid != 0
        or init_entry.gid != 0
        or init_entry.nlink != 1
        or init_entry.symlink_target != "sbin/nvx-agent"
        or any(name.startswith("init/") for name in entries)
    ):
        raise ScriptError(f"{path} does not select /sbin/nvx-agent as kernel PID 1")


def verify_legacy_initramfs(path: Path) -> None:
    entries = _validated_initramfs_entries(path, agent_profile=False)
    if "sbin/nvx-agent" in entries:
        raise ScriptError(f"{path} legacy profile contains /sbin/nvx-agent")
    try:
        init_entry = entries["init"]
    except KeyError as error:
        raise ScriptError(f"{path} legacy profile is missing /init") from error
    expected_init = require_file(REPO_ROOT / "alpine" / "init", "legacy /init")
    if (
        stat.S_IFMT(init_entry.mode) != stat.S_IFREG
        or stat.S_IMODE(init_entry.mode) != 0o755
        or init_entry.uid != 0
        or init_entry.gid != 0
        or init_entry.nlink != 1
        or init_entry.data != expected_init.read_bytes().replace(b"\r\n", b"\n")
        or any(name.startswith("init/") for name in entries)
    ):
        raise ScriptError(f"{path} does not contain the exact legacy /init")


def _publish_initramfs_output(source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(f"{destination.name}.part")
    temporary.unlink(missing_ok=True)
    try:
        shutil.copyfile(source, temporary)
        temporary.chmod(0o644)
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)


def build_initramfs(config: AlpineBuildConfig) -> None:
    _require_linux("build-initramfs")
    metadata_probe = _require_metadata_preserving_work_directory(config.work)
    agent_source: Path | None = None
    agent_sha256: str | None = None
    if config.agent_enabled:
        agent_source, agent_sha256 = verified_staged_guest_agent()
    if config.agent_enabled:
        assert agent_source is not None
        root = _prepare_agent_root(config.work, agent_source)
        helpers: dict[str, dict[str, str]] = {}
        trusted_owners: dict[str, tuple[int, int]] = {}
    else:
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
            REPO_ROOT / "alpine" / "nvx-snapshot",
            root / "sbin" / "nvx-snapshot",
        )
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
        device_io = _build_device_io_helper(
            config.work,
            root / "sbin" / "nvx-device-io",
        )
        helpers = {"nvx-device-io": device_io}
        trusted_owners = _trusted_alpine_owners(config)
    native_output = config.work / "output" / config.output.name
    native_output.parent.mkdir(parents=True, exist_ok=True)
    _write_apk_manifest(
        root,
        native_output,
        config,
        helpers,
        agent_sha256,
    )
    _pack_initramfs(root, native_output, trusted_owners)
    _bind_apk_manifest_to_initramfs(native_output)
    if agent_sha256 is not None:
        verify_agent_initramfs(native_output, agent_sha256)
    else:
        verify_legacy_initramfs(native_output)
    _publish_initramfs_output(native_output, config.output)
    _publish_initramfs_output(
        native_output.with_name(f"{native_output.name}.packages.json"),
        config.output.with_name(f"{config.output.name}.packages.json"),
    )
    shutil.rmtree(root)
    shutil.rmtree(metadata_probe)
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

    provenance = {
        "format": 1,
        "source_fingerprint": json.loads(source_fingerprint),
        "kernel_sha256": sha256_file(config.output),
        "config_sha256": sha256_file(kernel_config),
    }
    config.output.with_name(KERNEL_PROVENANCE_NAME).write_text(
        json.dumps(provenance, indent=2) + "\n",
        encoding="utf-8",
    )


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


def docker_build_agent_initramfs_command(
    config: DockerBuildConfig,
    agent_sha256: str,
) -> list[str | Path]:
    command = docker_build_command(config, "agent-initramfs-artifacts")
    insert_at = len(command) - 3
    command[insert_at:insert_at] = [
        "--secret",
        f"id=nvx-agent,src={REPO_ROOT / 'build' / GUEST_AGENT_ARTIFACT_NAME}",
        "--build-arg",
        f"NVX_AGENT_SHA256={agent_sha256}",
    ]
    return command


def build_docker_agent_initramfs(config: DockerBuildConfig) -> Path:
    require_tool(
        "docker",
        "docker was not found on PATH; install Docker with the Linux engine first",
    )
    _agent, agent_sha256 = verified_staged_guest_agent()
    destination = _docker_destination(config.destination)
    run_checked(
        docker_build_agent_initramfs_command(config, agent_sha256),
        cwd=REPO_ROOT,
    )
    output = destination / AGENT_INITRAMFS_NAME
    require_file(output, "Docker agent initramfs output")
    verify_agent_initramfs(output, agent_sha256)
    print(f">> built {output} ({format_size(output.stat().st_size)})")
    return output


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
