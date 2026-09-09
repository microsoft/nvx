"""Source collection, verification, and release packaging workflows."""

from __future__ import annotations

import gzip
import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.parse
import urllib.request
import zipfile
import zlib
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import BinaryIO, cast

from .archive import create_reproducible_tar_gz
from .build import (
    AGENT_INITRAMFS_NAME,
    BROKER_TRANSPORT,
    CONTROL_CONTRACT_REVISION,
    CONTROL_SESSION_PROTOCOL_VERSION,
    DEFAULT_ALPINE_BRANCH,
    DEFAULT_ALPINE_MINIROOTFS_SHA256,
    DEFAULT_ALPINE_VERSION,
    DEFAULT_KERNEL_SHA256,
    DEFAULT_KERNEL_URL,
    DEFAULT_KERNEL_VERSION,
    GUEST_AGENT_ARTIFACT_NAME,
    GUEST_AGENT_ARTIFACT_PATH,
    GUEST_AGENT_BUILD_ID,
    GUEST_AGENT_INITRAMFS_ARTIFACT_PATH,
    GUEST_AGENT_INSTALLED_PATH,
    GUEST_AGENT_MAXIMUM_BYTES,
    GUEST_AGENT_PROTOCOL_SCHEMA_VERSION,
    GUEST_AGENT_SHA256,
    GUEST_AGENT_SHA256_NAME,
    GUEST_AGENT_SIZE_BYTES,
    GUEST_AGENT_SOURCE_REVISION,
    GUEST_AGENT_TARGET,
    KERNEL_PROVENANCE_NAME,
    MICROVM_ABI_VERSION,
    OPENVMM_PROVENANCE_NAME,
    REQUIRED_SANDBOX_KERNEL_CONFIG,
    DockerBuildConfig,
    _assert_shared_status_kernel_config,
    _kernel_source_fingerprint,
    build_docker_linux_source,
    validate_static_x86_64_elf,
    verify_agent_initramfs,
    verify_legacy_initramfs,
)
from .collect_alpine_sources import collect_alpine_sources
from .common import (
    OPENVMM_DIR,
    REPO_ROOT,
    SOURCE_DIR,
    ScriptError,
    artifact_path,
    download,
    openvmm_binary_path,
    require_file,
    sha256_file,
    verify_sha256_sums,
    write_sha256_sums,
)

PROJECT_SOURCE_PATHS = (
    "alpine",
    "data/linux-kvm-baremetal.csv",
    "data/linux-kvm-virtual-machine.csv",
    "data/linux-mshv-baremetal.csv",
    "data/linux-mshv-virtual-machine.csv",
    "data/windows-whp-baremetal.csv",
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
KERNEL_PATCH_PATHS = tuple(
    path.relative_to(REPO_ROOT).as_posix()
    for path in sorted((REPO_ROOT / "kernel" / "patches").glob("*.patch"))
)

GUEST_RELEASE_NAMES = (
    "vmlinux",
    "vmlinux.config",
    "initramfs.cpio.gz",
    "initramfs.cpio.gz.packages.json",
)
RELEASE_TRANSPORTS = ("legacy", BROKER_TRANSPORT)
GITHUB_API_VERSION = "2022-11-28"
RELEASE_ARCHIVE_MAX_BYTES = 4 * 1024 * 1024 * 1024
RELEASE_ARCHIVE_MAX_MEMBERS = 256
RELEASE_ARCHIVE_MAX_MEMBER_BYTES = 2 * 1024 * 1024 * 1024
RELEASE_ARCHIVE_MAX_TOTAL_BYTES = 4 * 1024 * 1024 * 1024
RELEASE_ARCHIVE_MAX_PATH_BYTES = 240
RELEASE_ARCHIVE_MAX_COMPRESSION_RATIO = 200
RELEASE_MANIFEST_MAX_BYTES = 1024 * 1024
RELEASE_PACKAGE_MANIFEST_MAX_BYTES = 8 * 1024 * 1024
LIVE_GATE_PROOF_MAX_BYTES = 1024 * 1024
RELEASE_TAR_MAX_DECOMPRESSED_BYTES = (
    RELEASE_ARCHIVE_MAX_TOTAL_BYTES
    + RELEASE_ARCHIVE_MAX_MEMBERS * tarfile.BLOCKSIZE * 2
    + tarfile.RECORDSIZE
)
_TAR_STREAM_CHUNK_BYTES = 64 * 1024


@dataclass(frozen=True)
class _ReleaseAsset:
    tag: str
    name: str
    url: str
    size: int


@dataclass(frozen=True)
class _InstallFile:
    source: Path
    destination: Path
    mode: int


@dataclass(frozen=True)
class _ArchiveInventory:
    root_name: str
    manifest_bytes: bytes
    archive_sha256: str


@dataclass(frozen=True)
class _ValidatedReleasePackage:
    root: Path
    manifest: dict[str, object]
    manifest_bytes: bytes
    manifest_sha256: str
    canonical_manifest_sha256: str
    transport: str
    binary: Path
    artifact_hashes: dict[str, str]


class _BoundedTarStream:
    def __init__(self, path: Path, compressed: bool) -> None:
        self._input = path.open("rb")
        self._decompressor = (
            zlib.decompressobj(16 + zlib.MAX_WBITS) if compressed else None
        )
        self._compressed_pending = b""
        self._buffer = bytearray()
        self._finished = False
        self.total_compressed = 0
        self.total_decompressed = 0
        self.total_consumed = 0

    def __enter__(self) -> _BoundedTarStream:
        return self

    def __exit__(
        self,
        _exception_type: object,
        _exception: object,
        _traceback: object,
    ) -> None:
        self._input.close()

    def _read_input(self, size: int = _TAR_STREAM_CHUNK_BYTES) -> bytes:
        data = self._input.read(size)
        self.total_compressed += len(data)
        if self.total_compressed > RELEASE_ARCHIVE_MAX_BYTES:
            raise ScriptError("release archive exceeds the archive-size limit")
        return data

    def _append(self, data: bytes) -> None:
        self.total_decompressed += len(data)
        if self.total_decompressed > RELEASE_TAR_MAX_DECOMPRESSED_BYTES:
            raise ScriptError("release tar exceeds the decompressed-size limit")
        self._buffer.extend(data)

    def _pump(self) -> None:
        if self._finished:
            return
        if self._decompressor is None:
            data = self._read_input()
            if data:
                self._append(data)
            else:
                self._finished = True
            return
        while not self._finished:
            if self._decompressor.eof:
                if self._read_input(1):
                    raise ScriptError(
                        "release gzip contains trailing data or multiple streams"
                    )
                self._finished = True
                return
            compressed = self._compressed_pending or self._read_input()
            self._compressed_pending = b""
            if not compressed:
                raise ScriptError("release gzip stream is truncated")
            try:
                data = self._decompressor.decompress(
                    compressed,
                    _TAR_STREAM_CHUNK_BYTES,
                )
            except zlib.error as error:
                raise ScriptError(f"invalid release gzip stream: {error}") from error
            self._compressed_pending = self._decompressor.unconsumed_tail
            if self._decompressor.unused_data:
                raise ScriptError(
                    "release gzip contains trailing data or multiple streams"
                )
            if data:
                self._append(data)
                return

    def read_exact(self, size: int, description: str) -> bytes:
        while len(self._buffer) < size and not self._finished:
            self._pump()
        if len(self._buffer) < size:
            raise ScriptError(f"release tar is truncated in {description}")
        data = bytes(self._buffer[:size])
        del self._buffer[:size]
        self.total_consumed += size
        return data

    def discard_exact(
        self,
        size: int,
        description: str,
        *,
        require_zero: bool = False,
    ) -> None:
        remaining = size
        while remaining:
            chunk = self.read_exact(
                min(remaining, _TAR_STREAM_CHUNK_BYTES),
                description,
            )
            if require_zero and any(chunk):
                raise ScriptError(f"release tar has nonzero {description}")
            remaining -= len(chunk)

    def finish_zero_padding(self) -> None:
        while not self._finished or self._buffer:
            if not self._buffer:
                self._pump()
                continue
            chunk = bytes(self._buffer)
            self._buffer.clear()
            if any(chunk):
                raise ScriptError("release tar has payload after its end blocks")


def _parse_tar_octal(field: bytes, description: str) -> int:
    if (
        len(field) < 2
        or field[-1] != 0
        or any(value < ord("0") or value > ord("7") for value in field[:-1])
    ):
        raise ScriptError(f"release tar has malformed {description}")
    return int(field[:-1], 8)


def _tar_text_field(field: bytes, description: str) -> str:
    terminator = field.find(b"\0")
    if terminator < 0:
        value = field
    else:
        value = field[:terminator]
        if any(field[terminator:]):
            raise ScriptError(f"release tar has malformed {description}")
    try:
        return value.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ScriptError(f"release tar has non-UTF-8 {description}") from error


def _tar_text_bytes(value: str, size: int, description: str) -> bytes:
    encoded = value.encode("utf-8")
    if len(encoded) > size or b"\0" in encoded:
        raise ScriptError(f"release tar {description} does not fit POSIX ustar")
    return encoded.ljust(size, b"\0")


def _canonical_ustar_name_fields(name: str) -> tuple[bytes, bytes]:
    encoded = name.encode("utf-8")
    if len(encoded) <= 100:
        return _tar_text_bytes(name, 100, "name"), bytes(155)
    components = name.split("/")
    for index in range(1, len(components)):
        prefix = "/".join(components[:index])
        suffix = "/".join(components[index:])
        if len(prefix.encode("utf-8")) <= 155 and len(suffix.encode("utf-8")) <= 100:
            return (
                _tar_text_bytes(suffix, 100, "name"),
                _tar_text_bytes(prefix, 155, "prefix"),
            )
    raise ScriptError(f"release tar path does not fit POSIX ustar: {name}")


def _preflight_raw_tar(path: Path, *, compressed: bool) -> None:
    roots: set[str] = set()
    seen: set[str] = set()
    seen_casefolded: set[str] = set()
    total = [0]
    with _BoundedTarStream(path, compressed) as stream:
        while True:
            header = stream.read_exact(tarfile.BLOCKSIZE, "a header")
            if not any(header):
                second_end_block = stream.read_exact(
                    tarfile.BLOCKSIZE,
                    "the second end block",
                )
                if any(second_end_block):
                    raise ScriptError("release tar has only one zero end block")
                expected_size = (
                    (stream.total_consumed + tarfile.RECORDSIZE - 1)
                    // tarfile.RECORDSIZE
                ) * tarfile.RECORDSIZE
                stream.finish_zero_padding()
                if stream.total_decompressed != expected_size:
                    raise ScriptError("release tar has noncanonical end padding")
                if (
                    total[0]
                    > stream.total_compressed * RELEASE_ARCHIVE_MAX_COMPRESSION_RATIO
                ):
                    raise ScriptError(
                        "release archive exceeds the compression-ratio limit"
                    )
                break

            expected_checksum = _parse_tar_octal(
                header[148:154] + b"\0",
                "header checksum",
            )
            if header[154:156] != b"\0 ":
                raise ScriptError("release tar has malformed header checksum")
            actual_checksum = sum(header[:148]) + 8 * ord(" ") + sum(header[156:])
            if actual_checksum != expected_checksum:
                raise ScriptError("release tar header checksum is invalid")
            if header[257:263] != b"ustar\0" or header[263:265] != b"00":
                raise ScriptError("release tar is not canonical POSIX ustar")

            typeflag = header[156:157]
            if typeflag not in (tarfile.REGTYPE, tarfile.DIRTYPE):
                raise ScriptError(
                    f"release tar has unsupported metadata type {typeflag!r}"
                )
            is_directory = typeflag == tarfile.DIRTYPE
            name_part = _tar_text_field(header[:100], "name")
            prefix = _tar_text_field(header[345:500], "prefix")
            name = f"{prefix}/{name_part}" if prefix else name_part
            if is_directory:
                if not name.endswith("/"):
                    raise ScriptError("release tar directory name is not canonical")
            elif name.endswith("/"):
                raise ScriptError("release tar file name is not canonical")
            expected_name, expected_prefix = _canonical_ustar_name_fields(name)
            if header[:100] != expected_name or header[345:500] != expected_prefix:
                raise ScriptError("release tar path encoding is not canonical")

            mode = _parse_tar_octal(header[100:108], "mode")
            uid = _parse_tar_octal(header[108:116], "uid")
            gid = _parse_tar_octal(header[116:124], "gid")
            size = _parse_tar_octal(header[124:136], "size")
            mtime = _parse_tar_octal(header[136:148], "mtime")
            canonical = _record_archive_member(
                name,
                size,
                seen,
                seen_casefolded,
                roots,
                total,
            )
            if (
                mode != _expected_release_member_mode(canonical, is_directory)
                or uid != 0
                or gid != 0
                or mtime != 0
                or any(header[157:257])
                or any(header[265:345])
                or any(header[500:512])
            ):
                raise ScriptError(f"release tar entry {name} has noncanonical metadata")
            if is_directory and size:
                raise ScriptError(f"release tar directory {name} has data")
            stream.discard_exact(size, f"entry {name}")
            padding = -size % tarfile.BLOCKSIZE
            stream.discard_exact(
                padding,
                f"padding for {name}",
                require_zero=True,
            )

    if len(roots) != 1:
        raise ScriptError("release archive must contain exactly one package root")


def _runtime_identity(manifest: dict[str, object]) -> dict[str, object]:
    runtime = cast(dict[str, object], manifest["runtime"])
    openvmm = cast(dict[str, object], manifest["openvmm"])
    linux = cast(dict[str, object], manifest["linux"])
    alpine = cast(dict[str, object], manifest["alpine"])
    agent = cast(dict[str, object], manifest["guest_agent"])
    return {
        "transport": runtime["transport"],
        "openvmm_source_revision": openvmm["source_revision"],
        "openvmm_sha256": openvmm["executable_sha256"],
        "kernel_sha256": linux["kernel_sha256"],
        "kernel_config_sha256": linux["config_sha256"],
        "agent_initramfs_sha256": alpine["initramfs_sha256"],
        "initramfs_package_manifest_sha256": alpine[
            "initramfs_package_manifest_sha256"
        ],
        "agent_sha256": agent["sha256"],
        "agent_size": agent["size"],
        "agent_external_input_sha256": agent["external_input_sha256"],
        "agent_external_input_size_bytes": agent["external_input_size_bytes"],
        "agent_source_revision": agent["source_revision"],
        "agent_build_id": agent["build_id"],
        "agent_target": agent["target"],
        "agent_maximum_size_bytes": agent["maximum_size_bytes"],
        "agent_artifact": agent["artifact"],
        "agent_initramfs_artifact": agent["initramfs_artifact"],
        "agent_transport": agent["transport"],
        "protocol_schema_version": agent["protocol_schema_version"],
        "microvm_abi_version": openvmm["microvm_abi_version"],
        "control_session_protocol_version": openvmm["control_session_protocol_version"],
        "control_contract_revision": openvmm["control_contract_revision"],
    }


def _runtime_fingerprint(identity: dict[str, object]) -> str:
    canonical = json.dumps(
        identity,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return hashlib.sha256(canonical).hexdigest()


def _verify_runtime_fingerprint(manifest: dict[str, object]) -> None:
    runtime = cast(dict[str, object], manifest["runtime"])
    identity = _runtime_identity(manifest)
    if runtime.get("fingerprint_inputs") != identity:
        raise ScriptError(
            "packaged guest fingerprint inputs do not match runtime identities"
        )
    if runtime.get("guest_fingerprint_sha256") != _runtime_fingerprint(identity):
        raise ScriptError("packaged guest fingerprint digest is invalid")


def _root_broker_agent_contract() -> dict[str, object]:
    return {
        "artifact": GUEST_AGENT_ARTIFACT_PATH,
        "initramfs_artifact": GUEST_AGENT_INITRAMFS_ARTIFACT_PATH,
        "target": GUEST_AGENT_TARGET,
        "optional": True,
        "installed_in_initramfs": False,
        "maximum_size_bytes": GUEST_AGENT_MAXIMUM_BYTES,
        "source_revision": GUEST_AGENT_SOURCE_REVISION,
        "build_id": GUEST_AGENT_BUILD_ID,
        "external_input_sha256": GUEST_AGENT_SHA256,
        "external_input_size_bytes": GUEST_AGENT_SIZE_BYTES,
        "protocol_schema_version": GUEST_AGENT_PROTOCOL_SCHEMA_VERSION,
        "transport": BROKER_TRANSPORT,
    }


def _runtime_broker_agent_contract() -> dict[str, object]:
    return {
        **_root_broker_agent_contract(),
        "optional": False,
        "installed_in_initramfs": True,
        "sha256": GUEST_AGENT_SHA256,
        "size": GUEST_AGENT_SIZE_BYTES,
    }


def _package_broker_agent_contract() -> dict[str, object]:
    return {
        "path": GUEST_AGENT_INSTALLED_PATH,
        "sha256": GUEST_AGENT_SHA256,
        "size": GUEST_AGENT_SIZE_BYTES,
        "source_revision": GUEST_AGENT_SOURCE_REVISION,
        "build_id": GUEST_AGENT_BUILD_ID,
    }


def _kernel_patch_paths() -> list[str]:
    return list(KERNEL_PATCH_PATHS)


def _validate_root_broker_contract() -> dict[str, object]:
    manifest = _json_object(
        _read_limited_file(
            REPO_ROOT / "SOURCE-MANIFEST.json",
            RELEASE_MANIFEST_MAX_BYTES,
            "root SOURCE-MANIFEST.json",
        ),
        "root SOURCE-MANIFEST.json",
    )
    openvmm = manifest.get("openvmm")
    if not isinstance(openvmm, dict):
        raise ScriptError("root SOURCE-MANIFEST.json lacks OpenVMM identity")
    typed_openvmm = cast(dict[str, object], openvmm)
    linux = manifest.get("linux")
    if not isinstance(linux, dict):
        raise ScriptError("root SOURCE-MANIFEST.json lacks Linux identity")
    typed_linux = cast(dict[str, object], linux)
    expected_control = {
        "microvm_abi_version": MICROVM_ABI_VERSION,
        "control_session_protocol_version": CONTROL_SESSION_PROTOCOL_VERSION,
        "control_contract_revision": CONTROL_CONTRACT_REVISION,
    }
    if (
        manifest.get("format") != 1
        or manifest.get("guest_agent") != _root_broker_agent_contract()
        or typed_linux.get("patches") != _kernel_patch_paths()
        or any(
            typed_openvmm.get(field) != value
            for field, value in expected_control.items()
        )
    ):
        raise ScriptError(
            "root SOURCE-MANIFEST.json does not match the pinned broker contract"
        )
    return manifest


def _github_headers(token: str | None, accept: str) -> dict[str, str]:
    headers = {
        "Accept": accept,
        "X-GitHub-Api-Version": GITHUB_API_VERSION,
        "User-Agent": "nvx-release-downloader",
    }
    if token is not None:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def _latest_release_asset(
    repository: str,
    platform: str,
    transport: str,
    token: str | None,
) -> _ReleaseAsset:
    if transport not in RELEASE_TRANSPORTS:
        raise ScriptError(f"unsupported release transport: {transport}")
    repository_parts = repository.split("/")
    if len(repository_parts) != 2 or not all(repository_parts):
        raise ScriptError("GitHub repository must be OWNER/REPOSITORY")
    encoded_repository = "/".join(
        urllib.parse.quote(part, safe="") for part in repository_parts
    )
    url = f"https://api.github.com/repos/{encoded_repository}/releases?per_page=100"
    request = urllib.request.Request(
        url,
        headers=_github_headers(token, "application/vnd.github+json"),
    )
    try:
        with urllib.request.urlopen(request) as response:
            releases: object = json.load(response)
    except urllib.error.HTTPError as error:
        hint = ""
        if token is None and error.code in (401, 403, 404):
            hint = "; set GH_TOKEN to access private releases"
        raise ScriptError(
            f"GitHub release query failed with HTTP {error.code}{hint}"
        ) from error
    except (OSError, urllib.error.URLError) as error:
        raise ScriptError(f"GitHub release query failed: {error}") from error
    if not isinstance(releases, list):
        raise ScriptError("GitHub release query returned an invalid response")

    extension = ".zip" if platform.startswith("windows-") else ".tar.gz"
    asset_pattern = re.compile(
        rf"^nvx-.+-{re.escape(platform)}-{re.escape(transport)}"
        rf"{re.escape(extension)}$"
    )
    for release_value in cast(list[object], releases):
        if not isinstance(release_value, dict):
            continue
        release = cast(dict[str, object], release_value)
        if release.get("draft") is True:
            continue
        tag = release.get("tag_name")
        assets = release.get("assets")
        if not isinstance(tag, str) or not isinstance(assets, list):
            continue
        for asset_value in cast(list[object], assets):
            if not isinstance(asset_value, dict):
                continue
            asset = cast(dict[str, object], asset_value)
            name = asset.get("name")
            asset_url = asset.get("url")
            size = asset.get("size")
            if (
                isinstance(name, str)
                and asset_pattern.fullmatch(name) is not None
                and isinstance(asset_url, str)
                and isinstance(size, int)
            ):
                return _ReleaseAsset(tag, name, asset_url, size)
    raise ScriptError(f"no GitHub release contains an NVX package for {platform}")


def _validate_archive_member(name: str) -> None:
    canonical = name.rstrip("/")
    parts = canonical.split("/")
    if (
        not canonical
        or canonical == "."
        or "\\" in name
        or name.startswith("/")
        or any(part in ("", ".", "..") for part in parts)
        or any(":" in part or "\0" in part for part in parts)
        or len(name.encode("utf-8")) > RELEASE_ARCHIVE_MAX_PATH_BYTES
    ):
        raise ScriptError(f"unsafe path in release archive: {name}")


def _expected_release_member_mode(name: str, is_directory: bool) -> int:
    path = PurePosixPath(name)
    relative = path.parts[1:]
    if is_directory:
        return 0o755
    if relative and relative[0] == "bin":
        return 0o755
    if relative == ("guest", GUEST_AGENT_ARTIFACT_NAME):
        return 0o755
    return 0o644


def _write_extracted_file(
    destination: Path,
    content: BinaryIO,
    expected_size: int,
    mode: int,
    extracted_total: list[int],
) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    written = 0
    try:
        with destination.open("xb") as output:
            while written < expected_size:
                chunk = content.read(min(1024 * 1024, expected_size - written))
                if not chunk:
                    break
                written += len(chunk)
                extracted_total[0] += len(chunk)
                if extracted_total[0] > RELEASE_ARCHIVE_MAX_TOTAL_BYTES:
                    raise ScriptError(
                        "release archive exceeds the uncompressed-size limit"
                    )
                output.write(chunk)
            if content.read(1):
                raise ScriptError(
                    f"release archive entry {destination.name} exceeds its header size"
                )
    except BaseException:
        destination.unlink(missing_ok=True)
        raise
    if written != expected_size:
        destination.unlink(missing_ok=True)
        raise ScriptError(f"release archive truncated {destination.name}")
    destination.chmod(mode)


def _read_archive_member(
    content: BinaryIO,
    expected_size: int,
    description: str,
) -> bytes:
    if expected_size > RELEASE_MANIFEST_MAX_BYTES:
        raise ScriptError(f"{description} exceeds the manifest-size limit")
    data = content.read(expected_size + 1)
    if len(data) > expected_size:
        raise ScriptError(f"{description} exceeds its archive header size")
    if len(data) != expected_size:
        raise ScriptError(f"{description} is truncated")
    return data


def _record_archive_member(
    name: str,
    size: int,
    seen: set[str],
    seen_casefolded: set[str],
    roots: set[str],
    total: list[int],
) -> str:
    _validate_archive_member(name)
    canonical = name.rstrip("/")
    folded = canonical.casefold()
    if canonical in seen:
        raise ScriptError(f"duplicate entry in release archive: {name}")
    if folded in seen_casefolded:
        raise ScriptError(f"case-colliding entry in release archive: {name}")
    path = PurePosixPath(canonical)
    relative = path.parts[1:]
    metadata_limit = (
        RELEASE_MANIFEST_MAX_BYTES
        if relative in (("SOURCE-MANIFEST.json",), ("SHA256SUMS",))
        else (
            RELEASE_PACKAGE_MANIFEST_MAX_BYTES
            if relative and relative[-1].endswith(".packages.json")
            else RELEASE_ARCHIVE_MAX_MEMBER_BYTES
        )
    )
    if size < 0 or size > min(RELEASE_ARCHIVE_MAX_MEMBER_BYTES, metadata_limit):
        raise ScriptError(f"release archive entry {name} exceeds the size limit")
    seen.add(canonical)
    seen_casefolded.add(folded)
    if len(seen) > RELEASE_ARCHIVE_MAX_MEMBERS:
        raise ScriptError("release archive exceeds the member-count limit")
    roots.add(path.parts[0])
    total[0] += size
    if total[0] > RELEASE_ARCHIVE_MAX_TOTAL_BYTES:
        raise ScriptError("release archive exceeds the uncompressed-size limit")
    return canonical


def _preflight_release_archive(archive_path: Path) -> _ArchiveInventory:
    archive_path = require_file(archive_path, "release archive")
    archive_size = archive_path.stat().st_size
    if archive_size <= 0 or archive_size > RELEASE_ARCHIVE_MAX_BYTES:
        raise ScriptError("release archive exceeds the archive-size limit")
    roots: set[str] = set()
    seen: set[str] = set()
    seen_casefolded: set[str] = set()
    total = [0]
    manifest_bytes: bytes | None = None
    try:
        if archive_path.name.endswith(".tar.gz"):
            _preflight_raw_tar(archive_path, compressed=True)
            with tarfile.open(archive_path, "r:gz") as archive:
                for member in archive:
                    if member.type not in (
                        tarfile.REGTYPE,
                        tarfile.AREGTYPE,
                        tarfile.DIRTYPE,
                    ):
                        raise ScriptError(
                            f"unsupported entry in release archive: {member.name}"
                        )
                    canonical = _record_archive_member(
                        member.name,
                        member.size,
                        seen,
                        seen_casefolded,
                        roots,
                        total,
                    )
                    if member.isdir() and member.size != 0:
                        raise ScriptError(
                            f"release archive directory {member.name} has data"
                        )
                    expected_mode = _expected_release_member_mode(
                        canonical,
                        member.isdir(),
                    )
                    if (
                        member.mode != expected_mode
                        or member.uid != 0
                        or member.gid != 0
                        or member.uname
                        or member.gname
                    ):
                        raise ScriptError(
                            f"release archive entry {member.name} has unexpected "
                            "ownership or mode"
                        )
                    if PurePosixPath(canonical).parts[1:] == ("SOURCE-MANIFEST.json",):
                        if not member.isfile() or manifest_bytes is not None:
                            raise ScriptError(
                                "release archive has an invalid runtime manifest"
                            )
                        source = archive.extractfile(member)
                        if source is None:
                            raise ScriptError(
                                "release archive could not read its runtime manifest"
                            )
                        with source:
                            manifest_bytes = _read_archive_member(
                                cast(BinaryIO, source),
                                member.size,
                                "release runtime manifest",
                            )
        elif archive_path.suffix == ".zip":
            with zipfile.ZipFile(archive_path) as archive:
                for member in archive.infolist():
                    canonical = _record_archive_member(
                        member.filename,
                        member.file_size,
                        seen,
                        seen_casefolded,
                        roots,
                        total,
                    )
                    if member.is_dir() and member.file_size != 0:
                        raise ScriptError(
                            f"release archive directory {member.filename} has data"
                        )
                    file_type = (member.external_attr >> 16) & 0o170000
                    expected_type = stat.S_IFDIR if member.is_dir() else stat.S_IFREG
                    expected_mode = _expected_release_member_mode(
                        canonical,
                        member.is_dir(),
                    )
                    archived_mode = (member.external_attr >> 16) & 0o7777
                    if (
                        member.create_system != 3
                        or file_type != expected_type
                        or archived_mode != expected_mode
                        or member.flag_bits & 0x1
                    ):
                        raise ScriptError(
                            f"release archive entry {member.filename} lacks the "
                            f"expected Unix type/mode {expected_type | expected_mode:#o}"
                        )
                    if (
                        member.file_size
                        and member.file_size
                        > max(member.compress_size, 1)
                        * RELEASE_ARCHIVE_MAX_COMPRESSION_RATIO
                    ):
                        raise ScriptError(
                            f"release archive entry {member.filename} exceeds the "
                            "compression-ratio limit"
                        )
                    if PurePosixPath(canonical).parts[1:] == ("SOURCE-MANIFEST.json",):
                        if member.is_dir() or manifest_bytes is not None:
                            raise ScriptError(
                                "release archive has an invalid runtime manifest"
                            )
                        with archive.open(member) as source:
                            manifest_bytes = _read_archive_member(
                                cast(BinaryIO, source),
                                member.file_size,
                                "release runtime manifest",
                            )
        else:
            raise ScriptError(f"unsupported release archive: {archive_path.name}")
    except (tarfile.TarError, zipfile.BadZipFile) as error:
        raise ScriptError(
            f"invalid release archive {archive_path.name}: {error}"
        ) from error
    if len(roots) != 1:
        raise ScriptError("release archive must contain exactly one package root")
    if total[0] > archive_size * RELEASE_ARCHIVE_MAX_COMPRESSION_RATIO:
        raise ScriptError("release archive exceeds the compression-ratio limit")
    if manifest_bytes is None:
        raise ScriptError("release archive is missing SOURCE-MANIFEST.json")
    return _ArchiveInventory(
        root_name=next(iter(roots)),
        manifest_bytes=manifest_bytes,
        archive_sha256=sha256_file(archive_path),
    )


def _extract_release_archive(archive_path: Path, destination: Path) -> None:
    _preflight_release_archive(archive_path)
    destination.mkdir(parents=True, exist_ok=True)
    extracted_total = [0]
    try:
        if archive_path.name.endswith(".tar.gz"):
            with tarfile.open(archive_path, "r:gz") as archive:
                for member in archive:
                    canonical = member.name.rstrip("/")
                    output = destination / canonical
                    expected_mode = _expected_release_member_mode(
                        canonical,
                        member.isdir(),
                    )
                    if member.isdir():
                        output.mkdir(parents=True, exist_ok=True)
                        output.chmod(expected_mode)
                        continue
                    source = archive.extractfile(member)
                    if source is None:
                        raise ScriptError(
                            f"release archive could not read {member.name}"
                        )
                    with source:
                        _write_extracted_file(
                            output,
                            cast(BinaryIO, source),
                            member.size,
                            expected_mode,
                            extracted_total,
                        )
            return
        with zipfile.ZipFile(archive_path) as archive:
            for member in archive.infolist():
                canonical = member.filename.rstrip("/")
                output = destination / canonical
                expected_mode = _expected_release_member_mode(
                    canonical,
                    member.is_dir(),
                )
                if member.is_dir():
                    output.mkdir(parents=True, exist_ok=True)
                    output.chmod(expected_mode)
                    continue
                with archive.open(member) as source:
                    _write_extracted_file(
                        output,
                        cast(BinaryIO, source),
                        member.file_size,
                        expected_mode,
                        extracted_total,
                    )
    except (tarfile.TarError, zipfile.BadZipFile) as error:
        raise ScriptError(
            f"invalid release archive {archive_path.name}: {error}"
        ) from error


def _atomic_replace(source: Path, destination: Path) -> None:
    source.replace(destination)


def _remove_runtime_path(path: Path) -> None:
    if path.is_dir() and not path.is_symlink():
        shutil.rmtree(path)
    else:
        path.unlink(missing_ok=True)


def _runtime_mode_matches(path: Path, expected: int) -> bool:
    actual = stat.S_IMODE(path.stat().st_mode)
    if os.name == "nt":
        return actual & (stat.S_ISUID | stat.S_ISGID | stat.S_ISVTX) == 0 and bool(
            actual & 0o200
        )
    return actual == expected


def _read_limited_file(path: Path, maximum: int, description: str) -> bytes:
    path = require_file(path, description)
    size = path.stat().st_size
    if size > maximum:
        raise ScriptError(f"{description} exceeds the size limit")
    with path.open("rb") as source:
        data = source.read(maximum + 1)
    if len(data) != size:
        raise ScriptError(f"{description} changed while it was being read")
    return data


def _json_object(data: bytes, description: str) -> dict[str, object]:
    def reject_duplicate_keys(
        pairs: list[tuple[str, object]],
    ) -> dict[str, object]:
        value: dict[str, object] = {}
        for key, item in pairs:
            if key in value:
                raise ValueError(f"duplicate JSON key {key!r}")
            value[key] = item
        return value

    try:
        value: object = json.loads(
            data.decode("utf-8"),
            object_pairs_hook=reject_duplicate_keys,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        raise ScriptError(f"invalid {description}: {error}") from error
    if not isinstance(value, dict):
        raise ScriptError(f"{description} must be a JSON object")
    return cast(dict[str, object], value)


def _canonical_json_sha256(value: dict[str, object]) -> str:
    canonical = json.dumps(
        value,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return hashlib.sha256(canonical).hexdigest()


def _release_artifact_hashes(package_root: Path) -> dict[str, str]:
    checksum_bytes = _read_limited_file(
        package_root / "SHA256SUMS",
        RELEASE_MANIFEST_MAX_BYTES,
        "release checksum manifest",
    )
    try:
        lines = checksum_bytes.decode("ascii").splitlines()
    except UnicodeDecodeError as error:
        raise ScriptError("release checksum manifest is not ASCII") from error
    hashes: dict[str, str] = {}
    folded: set[str] = set()
    for line in lines:
        expected, separator, relative = line.partition("  ")
        if (
            not separator
            or re.fullmatch(r"[0-9a-f]{64}", expected) is None
            or not relative
        ):
            raise ScriptError(f"malformed release checksum line: {line}")
        _validate_archive_member(f"{package_root.name}/{relative}")
        if relative in hashes:
            raise ScriptError(f"duplicate release checksum path: {relative}")
        if relative.casefold() in folded:
            raise ScriptError(f"case-colliding release checksum path: {relative}")
        folded.add(relative.casefold())
        path = package_root / PurePosixPath(relative)
        if not path.is_file() or path.is_symlink():
            raise ScriptError(f"invalid release checksum path: {relative}")
        actual = sha256_file(path)
        if actual != expected:
            raise ScriptError(
                f"release checksum mismatch for {relative}: {actual}, "
                f"expected {expected}"
            )
        hashes[relative] = actual
    actual_files = {
        path.relative_to(package_root).as_posix()
        for path in package_root.rglob("*")
        if path.is_file() and path.name != "SHA256SUMS"
    }
    if set(hashes) != actual_files:
        missing = sorted(actual_files - set(hashes))
        stale = sorted(set(hashes) - actual_files)
        raise ScriptError(
            f"release checksum manifest is incomplete; missing={missing}; stale={stale}"
        )
    hashes["SHA256SUMS"] = hashlib.sha256(checksum_bytes).hexdigest()
    return hashes


def _validate_package_manifest(
    manifest_path: Path,
    initramfs: Path,
    transport: str,
    runtime_agent: dict[str, object],
) -> dict[str, object]:
    manifest = _json_object(
        _read_limited_file(
            manifest_path,
            RELEASE_PACKAGE_MANIFEST_MAX_BYTES,
            "initramfs package manifest",
        ),
        "initramfs package manifest",
    )
    expected_name = (
        AGENT_INITRAMFS_NAME if transport == "broker-ttrpc" else "initramfs.cpio.gz"
    )
    artifact = manifest.get("artifact")
    if not isinstance(artifact, dict):
        raise ScriptError("initramfs package manifest lacks artifact identity")
    typed_artifact = cast(dict[str, object], artifact)
    expected_artifact = {
        "name": expected_name,
        "sha256": sha256_file(initramfs),
        "size": initramfs.stat().st_size,
    }
    if (
        manifest.get("format") != 1
        or manifest.get("profile") != transport
        or manifest.get("architecture") != "x86_64"
        or typed_artifact != expected_artifact
        or not isinstance(manifest.get("packages"), list)
        or (
            transport == "legacy"
            and not cast(list[object], manifest["packages"])
        )
    ):
        raise ScriptError(
            f"initramfs package manifest does not describe the {transport} artifact"
        )
    packaged_agent = manifest.get("guest_agent")
    if transport == "legacy":
        if packaged_agent is not None:
            raise ScriptError("legacy package manifest declares a guest agent")
    else:
        if not isinstance(packaged_agent, dict):
            raise ScriptError("broker package manifest lacks guest-agent identity")
        typed_agent = cast(dict[str, object], packaged_agent)
        expected_agent = {
            "path": GUEST_AGENT_INSTALLED_PATH,
            "sha256": runtime_agent.get("sha256"),
            "size": runtime_agent.get("size"),
            "source_revision": runtime_agent.get("source_revision"),
            "build_id": runtime_agent.get("build_id"),
        }
        if typed_agent != expected_agent:
            raise ScriptError(
                "broker package manifest guest-agent identity is inconsistent"
            )
    return manifest


def _validate_pinned_broker_package(
    manifest: dict[str, object],
    package_manifest: dict[str, object],
    agent_path: Path,
    initramfs: Path,
) -> None:
    root_manifest = _validate_root_broker_contract()
    root_openvmm = cast(dict[str, object], root_manifest["openvmm"])
    runtime = cast(dict[str, object], manifest["runtime"])
    openvmm = cast(dict[str, object], manifest["openvmm"])
    alpine = cast(dict[str, object], manifest["alpine"])
    expected_openvmm = {
        "microvm_abi_version": MICROVM_ABI_VERSION,
        "control_session_protocol_version": CONTROL_SESSION_PROTOCOL_VERSION,
        "control_contract_revision": CONTROL_CONTRACT_REVISION,
    }
    if (
        runtime.get("transport") != BROKER_TRANSPORT
        or runtime.get("artifact_profile") != BROKER_TRANSPORT
        or manifest.get("guest_agent") != _runtime_broker_agent_contract()
        or package_manifest.get("guest_agent") != _package_broker_agent_contract()
        or alpine.get("profile") != BROKER_TRANSPORT
        or alpine.get("initramfs_artifact") != "guest/initramfs.cpio.gz"
        or any(openvmm.get(field) != value for field, value in expected_openvmm.items())
        or any(
            openvmm.get(field) != root_openvmm.get(field)
            for field in (
                "source_revision",
                "microvm_abi_version",
                "control_session_protocol_version",
                "control_contract_revision",
            )
        )
    ):
        raise ScriptError(
            "broker release metadata does not match the pinned agent contract"
        )
    if (
        agent_path.stat().st_size != GUEST_AGENT_SIZE_BYTES
        or sha256_file(agent_path) != GUEST_AGENT_SHA256
    ):
        raise ScriptError(
            "broker sibling guest agent does not match the pinned artifact"
        )
    validate_static_x86_64_elf(agent_path)
    verify_agent_initramfs(initramfs, GUEST_AGENT_SHA256)


def _validate_release_package_tree(
    package_root: Path,
    expected_transport: str | None = None,
    require_pinned_broker: bool = False,
) -> _ValidatedReleasePackage:
    package_root = require_file(
        package_root / "SHA256SUMS",
        "release bundle checksums",
    ).parent
    known_root_files = {
        "LICENSE",
        "README.md",
        "SHA256SUMS",
        "SOURCE-MANIFEST.json",
        "THIRD_PARTY_NOTICES.md",
    }
    for path in [package_root, *sorted(package_root.rglob("*"))]:
        if path.is_symlink() or not (path.is_file() or path.is_dir()):
            raise ScriptError(f"unsupported path in release bundle: {path}")
        relative = path.relative_to(package_root.parent).as_posix()
        expected_mode = _expected_release_member_mode(relative, path.is_dir())
        if not _runtime_mode_matches(path, expected_mode):
            raise ScriptError(f"release package path {path} has an unexpected mode")
    root_names = {path.name for path in package_root.iterdir()}
    if not known_root_files.issubset(root_names):
        raise ScriptError("release package is missing required root files")
    if not all((package_root / name).is_dir() for name in ("bin", "guest", "licenses")):
        raise ScriptError("release package is missing required directories")

    artifact_hashes = _release_artifact_hashes(package_root)
    manifest_bytes = _read_limited_file(
        package_root / "SOURCE-MANIFEST.json",
        RELEASE_MANIFEST_MAX_BYTES,
        "release runtime manifest",
    )
    manifest = _json_object(manifest_bytes, "release runtime manifest")
    manifest_sections: dict[str, dict[str, object]] = {}
    for name in ("runtime", "openvmm", "linux", "alpine", "guest_agent"):
        section = manifest.get(name)
        if not isinstance(section, dict):
            raise ScriptError(f"release runtime manifest {name} must be an object")
        manifest_sections[name] = cast(dict[str, object], section)
    try:
        runtime = manifest_sections["runtime"]
        openvmm = manifest_sections["openvmm"]
        linux = manifest_sections["linux"]
        alpine = manifest_sections["alpine"]
        agent = manifest_sections["guest_agent"]
        transport = cast(str, runtime["transport"])
        _verify_runtime_fingerprint(manifest)
    except (KeyError, TypeError) as error:
        raise ScriptError(
            "release runtime manifest is missing identity fields"
        ) from error
    if transport not in RELEASE_TRANSPORTS:
        raise ScriptError("release runtime manifest has an unsupported transport")
    if expected_transport is not None and transport != expected_transport:
        raise ScriptError(
            f"packaged runtime transport is {transport!r}, "
            f"expected {expected_transport!r}"
        )
    if (
        manifest.get("format") != 1
        or runtime.get("artifact_profile") != transport
        or runtime.get("immutable_bundle") is not True
    ):
        raise ScriptError("release runtime manifest has an invalid profile")

    binary_directory = package_root / "bin"
    binaries = [
        path
        for path in binary_directory.iterdir()
        if path.is_file() and path.name in ("openvmm", "openvmm.exe")
    ]
    if len(binaries) != 1 or len(list(binary_directory.iterdir())) != 1:
        raise ScriptError("release package must contain one named OpenVMM executable")
    binary = binaries[0]
    guest = package_root / "guest"
    common_guest_names = {
        "vmlinux",
        "vmlinux.config",
        "initramfs.cpio.gz",
        "initramfs.cpio.gz.packages.json",
    }
    profile_guest_names: set[str] = (
        {
            AGENT_INITRAMFS_NAME,
            f"{AGENT_INITRAMFS_NAME}.packages.json",
            GUEST_AGENT_ARTIFACT_NAME,
        }
        if transport == "broker-ttrpc"
        else set()
    )
    if {path.name for path in guest.iterdir()} != (
        common_guest_names | profile_guest_names
    ):
        raise ScriptError("release package guest artifact names are not canonical")
    required_paths = [
        package_root / name for name in known_root_files - {"SHA256SUMS"}
    ] + [
        package_root / "licenses" / "COPYING-LINUX",
        package_root / "licenses" / "LICENSE-OPENVMM",
        *(guest / name for name in common_guest_names | profile_guest_names),
    ]
    for path in required_paths:
        require_file(path, f"required release package path {path.name}")

    initramfs = guest / "initramfs.cpio.gz"
    package_manifest_path = guest / "initramfs.cpio.gz.packages.json"
    expected_hashes = {
        binary: openvmm.get("executable_sha256"),
        guest / "vmlinux": linux.get("kernel_sha256"),
        guest / "vmlinux.config": linux.get("config_sha256"),
        initramfs: alpine.get("initramfs_sha256"),
        package_manifest_path: alpine.get("initramfs_package_manifest_sha256"),
    }
    for path, expected in expected_hashes.items():
        actual = sha256_file(path)
        if expected != actual:
            raise ScriptError(
                f"packaged identity for {path.name} is {expected}, actual {actual}"
            )
    package_manifest = _validate_package_manifest(
        package_manifest_path,
        initramfs,
        transport,
        agent,
    )
    if alpine.get("profile") != transport:
        raise ScriptError("runtime manifest does not bind the initramfs profile")

    agent_path = guest / GUEST_AGENT_ARTIFACT_NAME
    if transport == "broker-ttrpc":
        named_initramfs = guest / AGENT_INITRAMFS_NAME
        named_package_manifest = guest / f"{AGENT_INITRAMFS_NAME}.packages.json"
        if (
            named_initramfs.read_bytes() != initramfs.read_bytes()
            or named_package_manifest.read_bytes() != package_manifest_path.read_bytes()
        ):
            raise ScriptError("broker initramfs aliases are not byte-identical")
        _validate_package_manifest(
            named_package_manifest,
            named_initramfs,
            transport,
            agent,
        )
        if (
            agent.get("installed_in_initramfs") is not True
            or agent.get("sha256") != sha256_file(agent_path)
            or agent.get("size") != agent_path.stat().st_size
            or agent.get("sha256") != agent.get("external_input_sha256")
            or agent.get("size") != agent.get("external_input_size_bytes")
        ):
            raise ScriptError("broker guest-agent identity is inconsistent")
        validate_static_x86_64_elf(agent_path)
        verify_agent_initramfs(initramfs, sha256_file(agent_path))
        if require_pinned_broker:
            _validate_pinned_broker_package(
                manifest,
                package_manifest,
                agent_path,
                initramfs,
            )
    else:
        if require_pinned_broker:
            raise ScriptError("pinned broker validation requires broker-ttrpc")
        if (
            agent.get("installed_in_initramfs") is not False
            or agent.get("sha256") is not None
            or agent.get("size") is not None
            or package_manifest.get("guest_agent") is not None
        ):
            raise ScriptError("legacy package declares an active guest agent")
        verify_legacy_initramfs(initramfs)

    return _ValidatedReleasePackage(
        root=package_root,
        manifest=manifest,
        manifest_bytes=manifest_bytes,
        manifest_sha256=hashlib.sha256(manifest_bytes).hexdigest(),
        canonical_manifest_sha256=_canonical_json_sha256(manifest),
        transport=transport,
        binary=binary,
        artifact_hashes=artifact_hashes,
    )


def _validate_release_archive_into(
    archive_path: Path,
    extraction_root: Path,
    expected_transport: str | None = None,
    trusted_manifest_sha256: str | None = None,
    require_pinned_broker: bool = False,
) -> tuple[_ArchiveInventory, _ValidatedReleasePackage]:
    inventory = _preflight_release_archive(archive_path)
    manifest_sha256 = hashlib.sha256(inventory.manifest_bytes).hexdigest()
    if (
        trusted_manifest_sha256 is not None
        and manifest_sha256 != trusted_manifest_sha256.lower()
    ):
        raise ScriptError(
            "packaged SOURCE-MANIFEST.json does not match the trusted digest"
        )
    _extract_release_archive(archive_path, extraction_root)
    package = _validate_release_package_tree(
        extraction_root / inventory.root_name,
        expected_transport,
        require_pinned_broker,
    )
    if package.manifest_bytes != inventory.manifest_bytes:
        raise ScriptError("release runtime manifest changed during extraction")
    return inventory, package


def validate_release_archive(
    archive_path: Path,
    expected_transport: str | None = None,
    trusted_manifest_sha256: str | None = None,
) -> None:
    with tempfile.TemporaryDirectory(prefix="nvx-package-validation-") as temporary:
        _validate_release_archive_into(
            archive_path,
            Path(temporary),
            expected_transport,
            trusted_manifest_sha256,
        )


def _write_transaction_journal(
    transaction: Path,
    journal: dict[str, object],
) -> None:
    path = transaction / "journal.json"
    temporary = transaction / "journal.json.part"
    temporary.write_text(
        json.dumps(journal, sort_keys=True, separators=(",", ":")) + "\n",
        encoding="utf-8",
    )
    temporary.replace(path)


def _read_transaction_journal(transaction: Path) -> dict[str, object]:
    path = require_file(transaction / "journal.json", "install transaction journal")
    try:
        value: object = json.loads(path.read_text(encoding="utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ScriptError(
            f"invalid recoverable install journal {path}: {error}"
        ) from error
    if not isinstance(value, dict):
        raise ScriptError(f"invalid recoverable install journal {path}")
    journal = cast(dict[str, object], value)
    if (
        journal.get("format") != 1
        or journal.get("phase") not in ("active", "committed", "rolled-back")
        or not isinstance(journal.get("operations"), list)
    ):
        raise ScriptError(f"invalid recoverable install journal {path}")
    return journal


def _transaction_operations(
    journal: dict[str, object],
) -> list[dict[str, object]]:
    operations_value = journal["operations"]
    if not isinstance(operations_value, list):
        raise ScriptError("invalid recoverable install transaction operations")
    operations: list[dict[str, object]] = []
    for value in cast(list[object], operations_value):
        if not isinstance(value, dict):
            raise ScriptError("invalid recoverable install transaction operation")
        operation = cast(dict[str, object], value)
        if (
            operation.get("kind") not in ("replace", "remove")
            or not isinstance(operation.get("destination"), str)
            or not isinstance(operation.get("backup"), str)
            or not isinstance(operation.get("prior_exists"), bool)
        ):
            raise ScriptError("invalid recoverable install transaction operation")
        backup = cast(str, operation["backup"])
        if not backup or "/" in backup or "\\" in backup or backup in (".", ".."):
            raise ScriptError("invalid recoverable install transaction backup")
        operations.append(operation)
    return operations


def _rollback_transaction(
    transaction: Path,
    journal: dict[str, object],
) -> list[str]:
    errors: list[str] = []
    for operation in reversed(_transaction_operations(journal)):
        destination = Path(cast(str, operation["destination"]))
        backup = transaction / "backup" / cast(str, operation["backup"])
        prior_exists = cast(bool, operation["prior_exists"])
        try:
            if prior_exists:
                if os.path.lexists(backup):
                    if os.path.lexists(destination):
                        _remove_runtime_path(destination)
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    _atomic_replace(backup, destination)
            elif os.path.lexists(destination):
                _remove_runtime_path(destination)
        except BaseException as error:
            errors.append(f"{destination}: {type(error).__name__}: {error}")
    if not errors:
        try:
            journal["phase"] = "rolled-back"
            _write_transaction_journal(transaction, journal)
            shutil.rmtree(transaction)
        except BaseException as error:
            errors.append(
                f"{transaction}: rollback cleanup failed: "
                f"{type(error).__name__}: {error}"
            )
    return errors


def _recover_install_transactions(transaction_parent: Path) -> None:
    if not transaction_parent.exists():
        return
    for transaction in sorted(transaction_parent.iterdir()):
        if not transaction.is_dir():
            raise ScriptError(
                f"unexpected entry in install transaction directory: {transaction}"
            )
        journal_path = transaction / "journal.json"
        if not journal_path.exists():
            shutil.rmtree(transaction)
            continue
        journal = _read_transaction_journal(transaction)
        if journal["phase"] in ("committed", "rolled-back"):
            shutil.rmtree(transaction)
            continue
        errors = _rollback_transaction(transaction, journal)
        if errors:
            raise ScriptError(
                f"failed to recover install transaction {transaction}; "
                "journal and backups were preserved: " + "; ".join(errors)
            )


def _transactional_install(
    files: list[_InstallFile],
    removals: list[Path],
) -> None:
    transaction_parent = artifact_path(".install-transactions")
    transaction_parent.mkdir(parents=True, exist_ok=True)
    _recover_install_transactions(transaction_parent)
    transaction = Path(
        tempfile.mkdtemp(
            prefix="nvx-install-",
            dir=transaction_parent,
        )
    )
    journal_written = False
    try:
        staged_root = transaction / "staged"
        backup_root = transaction / "backup"
        staged_root.mkdir()
        backup_root.mkdir()
        staged: list[tuple[Path, _InstallFile]] = []
        for index, item in enumerate(files):
            source = require_file(item.source, f"install source {item.source.name}")
            staged_path = staged_root / str(index)
            shutil.copyfile(source, staged_path)
            staged_path.chmod(item.mode)
            if sha256_file(staged_path) != sha256_file(
                source
            ) or not _runtime_mode_matches(staged_path, item.mode):
                raise ScriptError(f"failed to stage {item.destination.name} exactly")
            staged.append((staged_path, item))

        destinations = [item.destination for _, item in staged]
        if len(set(destinations)) != len(destinations):
            raise ScriptError("release install plan contains duplicate destinations")
        removal_set = set(removals)
        if removal_set.intersection(destinations):
            raise ScriptError("release install plan both replaces and removes a file")

        operations: list[dict[str, object]] = []
        for index, (_staged_path, item) in enumerate(staged):
            operations.append(
                {
                    "kind": "replace",
                    "destination": os.fspath(item.destination.absolute()),
                    "backup": f"file-{index}",
                    "prior_exists": os.path.lexists(item.destination),
                }
            )
        for index, destination in enumerate(removals):
            operations.append(
                {
                    "kind": "remove",
                    "destination": os.fspath(destination.absolute()),
                    "backup": f"removed-{index}",
                    "prior_exists": os.path.lexists(destination),
                }
            )
        journal: dict[str, object] = {
            "format": 1,
            "phase": "active",
            "operations": operations,
        }
        _write_transaction_journal(transaction, journal)
        journal_written = True
        try:
            for index, (staged_path, item) in enumerate(staged):
                destination = item.destination
                destination.parent.mkdir(parents=True, exist_ok=True)
                if os.path.lexists(destination):
                    _atomic_replace(destination, backup_root / f"file-{index}")
                _atomic_replace(staged_path, destination)
            for index, destination in enumerate(removals):
                if not os.path.lexists(destination):
                    continue
                _atomic_replace(destination, backup_root / f"removed-{index}")

            for _staged_path, item in staged:
                if sha256_file(item.destination) != sha256_file(
                    item.source
                ) or not _runtime_mode_matches(item.destination, item.mode):
                    raise ScriptError(
                        f"installed artifact readback failed for {item.destination}"
                    )
        except BaseException as error:
            rollback_errors = _rollback_transaction(transaction, journal)
            if rollback_errors:
                raise ScriptError(
                    f"release install failed ({type(error).__name__}: {error}); "
                    "rollback also failed; journal and backups were preserved: "
                    + "; ".join(rollback_errors)
                ) from error
            raise
        journal["phase"] = "committed"
        _write_transaction_journal(transaction, journal)
        shutil.rmtree(transaction)
    except BaseException:
        if not journal_written and transaction.exists():
            shutil.rmtree(transaction)
        raise


def _normalize_release_tree_modes(bundle: Path) -> None:
    for path in [bundle, *sorted(bundle.rglob("*"))]:
        if path.is_symlink() or not (path.is_file() or path.is_dir()):
            raise ScriptError(f"unsupported path in release bundle: {path}")
        relative = path.relative_to(bundle.parent).as_posix()
        path.chmod(_expected_release_member_mode(relative, path.is_dir()))


def create_release_archive(bundle: Path, output: Path) -> None:
    bundle = require_file(bundle / "SHA256SUMS", "release bundle checksums").parent
    _normalize_release_tree_modes(bundle)
    _validate_release_package_tree(bundle)
    paths = [bundle, *sorted(bundle.rglob("*"))]
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_name(f"{output.name}.part")
    temporary.unlink(missing_ok=True)
    try:
        if output.name.endswith(".tar.gz"):
            with temporary.open("wb") as raw:
                with gzip.GzipFile(
                    filename="",
                    mode="wb",
                    fileobj=raw,
                    mtime=0,
                ) as compressed:
                    with tarfile.open(
                        fileobj=compressed,
                        mode="w",
                        format=tarfile.USTAR_FORMAT,
                    ) as archive:
                        for path in paths:
                            arcname = path.relative_to(bundle.parent).as_posix()
                            _validate_archive_member(arcname)
                            is_directory = path.is_dir()
                            member = tarfile.TarInfo(arcname)
                            member.type = (
                                tarfile.DIRTYPE if is_directory else tarfile.REGTYPE
                            )
                            member.mode = _expected_release_member_mode(
                                arcname,
                                is_directory,
                            )
                            member.uid = 0
                            member.gid = 0
                            member.uname = ""
                            member.gname = ""
                            member.mtime = 0
                            member.size = 0 if is_directory else path.stat().st_size
                            if is_directory:
                                archive.addfile(member)
                            else:
                                with path.open("rb") as source:
                                    archive.addfile(member, source)
        elif output.suffix == ".zip":
            with zipfile.ZipFile(
                temporary,
                "w",
                compression=zipfile.ZIP_DEFLATED,
                compresslevel=9,
            ) as archive:
                for path in paths:
                    arcname = path.relative_to(bundle.parent).as_posix()
                    is_directory = path.is_dir()
                    if is_directory:
                        arcname += "/"
                    _validate_archive_member(arcname)
                    mode = _expected_release_member_mode(arcname, is_directory)
                    member = zipfile.ZipInfo(arcname, date_time=(1980, 1, 1, 0, 0, 0))
                    member.create_system = 3
                    member.compress_type = zipfile.ZIP_DEFLATED
                    member.external_attr = (
                        (stat.S_IFDIR if is_directory else stat.S_IFREG) | mode
                    ) << 16
                    if is_directory:
                        member.external_attr |= 0x10
                        archive.writestr(member, b"")
                    else:
                        with (
                            path.open("rb") as source,
                            archive.open(member, "w", force_zip64=True) as destination,
                        ):
                            shutil.copyfileobj(source, destination)
        else:
            raise ScriptError(f"unsupported release archive: {output.name}")
        temporary.replace(output)
        try:
            validate_release_archive(output)
        except BaseException:
            output.unlink(missing_ok=True)
            raise
    finally:
        temporary.unlink(missing_ok=True)


def verify_broker_live_gate(
    archive_path: Path,
    manifest_digest_path: Path,
    proof_path: Path,
    trusted_proof_sha256: str,
    expected_platform: str,
    expected_archive_sha256: str | None = None,
) -> None:
    if archive_path.resolve() in (
        manifest_digest_path.resolve(),
        proof_path.resolve(),
    ):
        raise ScriptError("live-gate trust metadata must be external to the archive")
    trusted_proof_sha256 = trusted_proof_sha256.lower()
    if re.fullmatch(r"[0-9a-f]{64}", trusted_proof_sha256) is None:
        raise ScriptError(
            "trusted live-gate proof SHA-256 must be 64 hexadecimal digits"
        )
    proof_bytes = _read_limited_file(
        proof_path,
        LIVE_GATE_PROOF_MAX_BYTES,
        "broker live-gate proof",
    )
    actual_proof_sha256 = hashlib.sha256(proof_bytes).hexdigest()
    if actual_proof_sha256 != trusted_proof_sha256:
        raise ScriptError(
            f"broker live-gate proof SHA-256 is {actual_proof_sha256}, "
            f"expected {trusted_proof_sha256}"
        )
    proof = _json_object(proof_bytes, "broker live-gate proof")
    digest_bytes = _read_limited_file(
        manifest_digest_path,
        128,
        "external broker manifest digest",
    )
    try:
        delivered_manifest_sha256 = digest_bytes.decode("ascii").strip().lower()
    except UnicodeDecodeError as error:
        raise ScriptError("external broker manifest digest is not ASCII") from error
    if re.fullmatch(r"[0-9a-f]{64}", delivered_manifest_sha256) is None:
        raise ScriptError("external broker manifest digest is invalid")

    with tempfile.TemporaryDirectory(prefix="nvx-live-gate-") as temporary:
        inventory, package = _validate_release_archive_into(
            archive_path,
            Path(temporary),
            BROKER_TRANSPORT,
            delivered_manifest_sha256,
            True,
        )
        if expected_archive_sha256 is not None and (
            re.fullmatch(r"[0-9a-fA-F]{64}", expected_archive_sha256) is None
            or inventory.archive_sha256 != expected_archive_sha256.lower()
        ):
            raise ScriptError(
                "publish-computed archive SHA-256 does not match the broker archive"
            )
        guest = package.root / "guest"
        actual_hashes = {
            "openvmm_sha256": sha256_file(package.binary),
            "kernel_sha256": sha256_file(guest / "vmlinux"),
            "kernel_config_sha256": sha256_file(guest / "vmlinux.config"),
            "initramfs_sha256": sha256_file(guest / "initramfs.cpio.gz"),
            "package_manifest_sha256": sha256_file(
                guest / "initramfs.cpio.gz.packages.json"
            ),
            "agent_sha256": GUEST_AGENT_SHA256,
        }

    expected_backend = expected_platform.rsplit("-", maxsplit=1)[-1]
    required_proof: dict[str, object] = {
        "format": 1,
        "result": "passed",
        "archive_sha256": inventory.archive_sha256,
        "bundle_manifest_sha256": package.manifest_sha256,
        "canonical_manifest_sha256": package.canonical_manifest_sha256,
        "platform": expected_platform,
        "backend": expected_backend,
        "transport": BROKER_TRANSPORT,
        "artifact_sha256": package.artifact_hashes,
        **actual_hashes,
    }
    required_checks: dict[str, object] = {
        "control_auth": True,
        "get_guest_info": True,
        "get_guest_info_self_sha256": GUEST_AGENT_SHA256,
        "bootstrap": True,
        "wait_ready": True,
        "shutdown": True,
    }
    required_proof["checks"] = required_checks
    if set(proof) != set(required_proof):
        raise ScriptError("broker live-gate proof fields are not canonical")
    if proof.get("checks") != required_checks:
        raise ScriptError(
            "broker live-gate proof does not contain the exact successful "
            "control-auth/GetGuestInfo/Bootstrap/WaitReady/Shutdown checks"
        )
    for field, expected in required_proof.items():
        if proof.get(field) != expected:
            raise ScriptError(
                f"broker live-gate proof {field} does not match the package: "
                f"{proof.get(field)!r}, expected {expected!r}"
            )


def _install_release_archive(
    archive_path: Path,
    expected_transport: str,
    trusted_manifest_sha256: str | None = None,
) -> None:
    if expected_transport not in RELEASE_TRANSPORTS:
        raise ScriptError(f"unsupported release transport: {expected_transport}")
    if expected_transport == "broker-ttrpc" and (
        trusted_manifest_sha256 is None
        or re.fullmatch(r"[0-9a-fA-F]{64}", trusted_manifest_sha256) is None
    ):
        raise ScriptError(
            "broker-ttrpc install requires an independently delivered "
            "SOURCE-MANIFEST.json SHA-256"
        )
    if expected_transport == "legacy" and trusted_manifest_sha256 is not None:
        raise ScriptError("legacy install does not accept broker trust metadata")
    with tempfile.TemporaryDirectory(prefix="nvx-release-") as temporary:
        extraction_root = Path(temporary)
        _inventory, package = _validate_release_archive_into(
            archive_path,
            extraction_root,
            expected_transport,
            trusted_manifest_sha256,
        )
        package_root = package.root

        binary_destination = openvmm_binary_path()
        binary_source = require_file(
            package_root / "bin" / binary_destination.name,
            "packaged OpenVMM binary",
        )
        guest_sources = {
            name: require_file(
                package_root / "guest" / name,
                f"packaged guest artifact {name}",
            )
            for name in GUEST_RELEASE_NAMES
        }
        try:
            openvmm_manifest = cast(
                dict[str, object],
                package.manifest["openvmm"],
            )
            agent_manifest = cast(
                dict[str, object],
                package.manifest["guest_agent"],
            )
            source_revision = openvmm_manifest["source_revision"]
        except (KeyError, TypeError) as error:
            raise ScriptError(
                "packaged source manifest is missing required runtime identity fields"
            ) from error
        if (
            not isinstance(source_revision, str)
            or len(source_revision) != 40
            or re.fullmatch(r"[0-9a-f]{40}", source_revision) is None
        ):
            raise ScriptError("packaged OpenVMM source revision is invalid")
        agent_source = package_root / "guest" / GUEST_AGENT_ARTIFACT_NAME
        agent_sha256 = (
            cast(str, agent_manifest["sha256"])
            if package.transport == "broker-ttrpc"
            else None
        )
        provenance = {
            "format": 1,
            "source_revision": source_revision,
            "source_clean": True,
            "executable_sha256": sha256_file(binary_source),
            "origin": "release",
        }

        generated = extraction_root / ".install-generated"
        generated.mkdir()
        provenance_source = generated / OPENVMM_PROVENANCE_NAME
        provenance_source.write_text(
            json.dumps(provenance, indent=2) + "\n",
            encoding="utf-8",
        )
        install_files = [
            _InstallFile(binary_source, binary_destination, 0o755),
            _InstallFile(guest_sources["vmlinux"], artifact_path("vmlinux"), 0o644),
            _InstallFile(
                guest_sources["vmlinux.config"],
                artifact_path("vmlinux.config"),
                0o644,
            ),
            _InstallFile(
                provenance_source,
                artifact_path(OPENVMM_PROVENANCE_NAME),
                0o644,
            ),
        ]
        if package.transport == "legacy":
            install_files.extend(
                [
                    _InstallFile(
                        guest_sources["initramfs.cpio.gz"],
                        artifact_path("initramfs.cpio.gz"),
                        0o644,
                    ),
                    _InstallFile(
                        guest_sources["initramfs.cpio.gz.packages.json"],
                        artifact_path("initramfs.cpio.gz.packages.json"),
                        0o644,
                    ),
                ]
            )
            removals = [
                artifact_path(AGENT_INITRAMFS_NAME),
                artifact_path(f"{AGENT_INITRAMFS_NAME}.packages.json"),
                artifact_path(GUEST_AGENT_ARTIFACT_NAME),
                artifact_path(GUEST_AGENT_SHA256_NAME),
            ]
        else:
            agent_pin_source = generated / GUEST_AGENT_SHA256_NAME
            assert agent_sha256 is not None
            agent_pin_source.write_text(f"{agent_sha256}\n", encoding="ascii")
            install_files.extend(
                [
                    _InstallFile(
                        package_root / "guest" / AGENT_INITRAMFS_NAME,
                        artifact_path(AGENT_INITRAMFS_NAME),
                        0o644,
                    ),
                    _InstallFile(
                        guest_sources["initramfs.cpio.gz.packages.json"],
                        artifact_path(f"{AGENT_INITRAMFS_NAME}.packages.json"),
                        0o644,
                    ),
                    _InstallFile(
                        agent_source,
                        artifact_path(GUEST_AGENT_ARTIFACT_NAME),
                        0o755,
                    ),
                    _InstallFile(
                        agent_pin_source,
                        artifact_path(GUEST_AGENT_SHA256_NAME),
                        0o644,
                    ),
                ]
            )
            removals = [
                artifact_path("initramfs.cpio.gz"),
                artifact_path("initramfs.cpio.gz.packages.json"),
            ]
        _transactional_install(install_files, removals)


def download_latest_release(
    repository: str,
    platform: str,
    transport: str,
    trusted_manifest_sha256: str | None,
) -> None:
    if transport == "broker-ttrpc" and (
        trusted_manifest_sha256 is None
        or re.fullmatch(r"[0-9a-fA-F]{64}", trusted_manifest_sha256) is None
    ):
        raise ScriptError(
            "broker-ttrpc download requires an independently delivered "
            "SOURCE-MANIFEST.json SHA-256"
        )
    if transport == "legacy" and trusted_manifest_sha256 is not None:
        raise ScriptError(
            "a trusted manifest digest applies only to broker-ttrpc downloads"
        )
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    asset = _latest_release_asset(repository, platform, transport, token)
    print(f">> downloading {asset.name} from {asset.tag}")
    with tempfile.TemporaryDirectory(prefix="nvx-download-") as temporary:
        archive_path = Path(temporary) / asset.name
        download(
            asset.url,
            archive_path,
            headers=_github_headers(token, "application/octet-stream"),
        )
        actual_size = archive_path.stat().st_size
        if actual_size != asset.size:
            raise ScriptError(
                f"downloaded {asset.name} is {actual_size} bytes, expected {asset.size}"
            )
        _install_release_archive(
            archive_path,
            transport,
            trusted_manifest_sha256,
        )
    print(f">> installed {asset.tag} for {platform}/{transport}")


def _copy_release_file(source: Path, destination: Path) -> None:
    require_file(source, source.name)
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    mode = (
        0o755
        if destination.parent.name == "bin"
        or (
            destination.parent.name == "guest"
            and destination.name == GUEST_AGENT_ARTIFACT_NAME
        )
        else 0o644
    )
    destination.chmod(mode)


def _validate_alpine_sources(package_manifests: list[Path]) -> None:
    source_root = SOURCE_DIR / "alpine"
    source_manifest_path = require_file(
        source_root / "manifest.json",
        "collected Alpine source manifest",
    )
    source_manifest = json.loads(source_manifest_path.read_text(encoding="utf-8"))
    collected = {
        (package["package"], package["version"], package["commit"]): package
        for package in source_manifest["packages"]
    }
    missing: list[str] = []
    for manifest_path in package_manifests:
        package_manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        for package in package_manifest["packages"]:
            key = (
                package["name"],
                package["version"],
                package["aports_commit"],
            )
            if key not in collected:
                missing.append(f"{key[0]}-{key[1]} ({key[2]})")
    if missing:
        raise ScriptError(
            "collected Alpine sources do not cover packaged APKs: "
            + ", ".join(sorted(missing))
        )
    for package in collected.values():
        for field in ("recipe", "source_directory"):
            path = source_root / package[field]
            if not path.exists():
                raise ScriptError(
                    f"collected Alpine {field} is missing for "
                    f"{package['package']}: {path}"
                )
    verify_sha256_sums(source_root)


def _create_source_archive(
    output: Path,
    inputs: list[tuple[Path, str]],
) -> None:
    print(f">> creating source archive {output}")
    create_reproducible_tar_gz(output, inputs)


def _project_source_archive(
    output: Path,
    version: str,
    package_manifests: list[Path],
) -> None:
    root = f"nvx-project-source-{version}"
    inputs = [
        (REPO_ROOT / relative, f"{root}/{relative}")
        for relative in PROJECT_SOURCE_PATHS
    ]
    inputs.extend(
        (
            manifest,
            f"{root}/build/{manifest.name}",
        )
        for manifest in package_manifests
    )
    inputs.append((artifact_path("vmlinux.config"), f"{root}/build/vmlinux.config"))
    _create_source_archive(output, inputs)


def _alpine_source_archive(
    output: Path,
    version: str,
    package_manifests: list[Path],
) -> None:
    root = f"nvx-alpine-source-{version}"
    inputs = [(SOURCE_DIR / "alpine", f"{root}/sources")]
    inputs.extend(
        (
            manifest,
            f"{root}/manifests/{manifest.name}",
        )
        for manifest in package_manifests
    )
    _create_source_archive(output, inputs)


def _validate_linux_source_archive(path: Path) -> None:
    expected_members = {
        "vmlinux.config": artifact_path("vmlinux.config").read_bytes(),
        "SOURCE-MANIFEST.json": (REPO_ROOT / "SOURCE-MANIFEST.json").read_bytes(),
    }
    expected_members.update(
        {
            relative: (REPO_ROOT / relative).read_bytes()
            for relative in _kernel_patch_paths()
        }
    )
    with tarfile.open(path, "r:gz") as archive:
        members = archive.getmembers()
        names = [member.name for member in members]
        if not any(
            name.endswith(f"linux-{DEFAULT_KERNEL_VERSION}/drivers/tty/hvc/hvc_xe9.c")
            for name in names
        ):
            raise ScriptError(f"{path} does not contain the patched xe9 HVC driver")
        for suffix, expected in expected_members.items():
            matches = [member for member in members if member.name.endswith(suffix)]
            if len(matches) != 1:
                raise ScriptError(
                    f"{path} contains {len(matches)} members ending in {suffix}"
                )
            extracted = archive.extractfile(matches[0])
            if extracted is None or extracted.read() != expected:
                raise ScriptError(f"{path} has stale contents for {suffix}")


def _guest_release_inputs(
    transport: str,
) -> tuple[dict[str, Path], list[Path]]:
    if transport not in RELEASE_TRANSPORTS:
        raise ScriptError(f"unsupported release transport: {transport}")
    common = {
        name: require_file(artifact_path(name), f"required guest artifact {name}")
        for name in ("vmlinux", "vmlinux.config")
    }
    _validate_kernel_provenance(
        common["vmlinux"],
        common["vmlinux.config"],
        require_file(
            artifact_path(KERNEL_PROVENANCE_NAME),
            "kernel build provenance",
        ),
    )
    if transport == "legacy":
        inputs = {
            **common,
            "initramfs.cpio.gz": require_file(
                artifact_path("initramfs.cpio.gz"),
                "legacy initramfs",
            ),
            "initramfs.cpio.gz.packages.json": require_file(
                artifact_path("initramfs.cpio.gz.packages.json"),
                "legacy initramfs package manifest",
            ),
        }
        verify_legacy_initramfs(inputs["initramfs.cpio.gz"])
        _validate_package_manifest(
            inputs["initramfs.cpio.gz.packages.json"],
            inputs["initramfs.cpio.gz"],
            "legacy",
            {},
        )
        return inputs, [inputs["initramfs.cpio.gz.packages.json"]]

    agent_initramfs = require_file(
        artifact_path(AGENT_INITRAMFS_NAME),
        "broker-ttrpc agent initramfs",
    )
    agent_packages = require_file(
        artifact_path(f"{AGENT_INITRAMFS_NAME}.packages.json"),
        "broker-ttrpc agent initramfs package manifest",
    )
    agent = require_file(
        artifact_path(GUEST_AGENT_ARTIFACT_NAME),
        "staged NVX guest agent",
    )
    expected = (
        require_file(
            artifact_path(GUEST_AGENT_SHA256_NAME),
            "staged NVX guest-agent SHA-256 pin",
        )
        .read_text(encoding="ascii")
        .strip()
        .lower()
    )
    actual = sha256_file(agent)
    if expected != GUEST_AGENT_SHA256:
        raise ScriptError(
            "staged NVX guest-agent SHA-256 pin does not match the required "
            f"external input: {expected}, expected {GUEST_AGENT_SHA256}"
        )
    if actual != expected:
        raise ScriptError(
            f"staged NVX guest-agent SHA-256 is {actual}, expected {expected}"
        )
    validate_static_x86_64_elf(agent)
    size = agent.stat().st_size
    if size != GUEST_AGENT_SIZE_BYTES:
        raise ScriptError(
            f"staged NVX guest-agent size is {size} bytes, "
            f"expected {GUEST_AGENT_SIZE_BYTES} bytes"
        )
    if size > GUEST_AGENT_MAXIMUM_BYTES:
        raise ScriptError("staged NVX guest agent exceeds the 16-MiB release limit")
    verify_agent_initramfs(agent_initramfs, expected)
    _validate_package_manifest(
        agent_packages,
        agent_initramfs,
        "broker-ttrpc",
        {
            "sha256": expected,
            "size": size,
            "source_revision": GUEST_AGENT_SOURCE_REVISION,
            "build_id": GUEST_AGENT_BUILD_ID,
        },
    )
    inputs = {
        **common,
        "initramfs.cpio.gz": agent_initramfs,
        "initramfs.cpio.gz.packages.json": agent_packages,
        AGENT_INITRAMFS_NAME: agent_initramfs,
        f"{AGENT_INITRAMFS_NAME}.packages.json": agent_packages,
        GUEST_AGENT_ARTIFACT_NAME: agent,
    }
    return inputs, [agent_packages]


def _validate_kernel_provenance(
    kernel: Path,
    kernel_config: Path,
    provenance_path: Path,
) -> None:
    try:
        provenance = json.loads(provenance_path.read_text(encoding="utf-8"))
        expected_source = json.loads(_kernel_source_fingerprint())
    except (json.JSONDecodeError, OSError) as error:
        raise ScriptError(f"invalid kernel build provenance: {error}") from error
    if (
        provenance.get("format") != 1
        or provenance.get("source_fingerprint") != expected_source
        or provenance.get("kernel_sha256") != sha256_file(kernel)
        or provenance.get("config_sha256") != sha256_file(kernel_config)
    ):
        raise ScriptError(
            "kernel build provenance does not match the current source, config, and vmlinux"
        )
    _assert_shared_status_kernel_config(kernel_config)


def _runtime_source_manifest(
    release_destination: Path,
    binary_name: str,
    transport: str,
) -> bytes:
    if transport == BROKER_TRANSPORT:
        _validate_root_broker_contract()
    manifest = json.loads(
        (REPO_ROOT / "SOURCE-MANIFEST.json").read_text(encoding="utf-8")
    )
    manifest["openvmm"]["executable_sha256"] = sha256_file(
        release_destination / "bin" / binary_name
    )
    manifest["linux"]["kernel_sha256"] = sha256_file(
        release_destination / "guest" / "vmlinux"
    )
    manifest["linux"]["config_sha256"] = sha256_file(
        release_destination / "guest" / "vmlinux.config"
    )
    manifest["alpine"]["initramfs_sha256"] = sha256_file(
        release_destination / "guest" / "initramfs.cpio.gz"
    )
    manifest["alpine"]["initramfs_package_manifest_sha256"] = sha256_file(
        release_destination / "guest" / "initramfs.cpio.gz.packages.json"
    )
    manifest["alpine"]["profile"] = transport
    agent_enabled = transport == "broker-ttrpc"
    if agent_enabled:
        agent = release_destination / "guest" / GUEST_AGENT_ARTIFACT_NAME
        agent_sha256 = sha256_file(agent)
        agent_size = agent.stat().st_size
        if agent_sha256 != GUEST_AGENT_SHA256 or agent_size != GUEST_AGENT_SIZE_BYTES:
            raise ScriptError(
                "packaged NVX guest agent does not match the required external input"
            )
        manifest["guest_agent"]["sha256"] = agent_sha256
        manifest["guest_agent"]["size"] = agent_size
    else:
        manifest["guest_agent"]["sha256"] = None
        manifest["guest_agent"]["size"] = None
    manifest["guest_agent"]["installed_in_initramfs"] = agent_enabled
    manifest["guest_agent"]["optional"] = not agent_enabled
    manifest["alpine"]["initramfs_artifact"] = "guest/initramfs.cpio.gz"
    manifest["runtime"] = {
        "transport": transport,
        "artifact_profile": transport,
        "immutable_bundle": True,
    }
    identity = _runtime_identity(manifest)
    manifest["runtime"]["guest_fingerprint_sha256"] = _runtime_fingerprint(identity)
    manifest["runtime"]["fingerprint_inputs"] = identity
    return (json.dumps(manifest, sort_keys=True, separators=(",", ":")) + "\n").encode(
        "utf-8"
    )


def collect_release_sources(transport: str) -> None:
    _guest_inputs, package_manifests = _guest_release_inputs(transport)
    collect_alpine_sources(
        package_manifests,
        SOURCE_DIR / "alpine",
        REPO_ROOT / ".cache" / "aports",
    )
    build_docker_linux_source(DockerBuildConfig(destination=SOURCE_DIR / "linux"))
    print(f">> collected release sources under {SOURCE_DIR}")


def package_release(
    *,
    version: str | None,
    destination: Path | None,
    include_source: bool,
    force: bool,
    transport: str,
    manifest_digest_output: Path | None,
) -> None:
    guest_inputs, package_manifests = _guest_release_inputs(transport)
    linux_source_archive = (
        SOURCE_DIR / "linux" / f"nvx-linux-source-{DEFAULT_KERNEL_VERSION}.tar.gz"
    )
    if include_source:
        _validate_alpine_sources(package_manifests)
        require_file(linux_source_archive, "Linux corresponding-source archive")
        _validate_linux_source_archive(linux_source_archive)
    else:
        print(
            "!! binary-only package: publish matching Linux and Alpine "
            "corresponding source separately",
            file=sys.stderr,
        )

    release_version = (
        version or (REPO_ROOT / "VERSION").read_text(encoding="ascii").strip()
    )
    release_destination = (
        destination or REPO_ROOT / "dist" / release_version
    ).resolve()
    digest_output: Path | None = None
    if transport == "broker-ttrpc":
        digest_output = manifest_digest_output or release_destination.with_name(
            f"{release_destination.name}.SOURCE-MANIFEST.sha256"
        )
        if not digest_output.is_absolute():
            digest_output = REPO_ROOT / digest_output
        digest_output = digest_output.resolve()
        if (
            digest_output == release_destination
            or release_destination in digest_output.parents
        ):
            raise ScriptError(
                "external manifest digest output must be outside the runtime bundle"
            )
    elif manifest_digest_output is not None:
        raise ScriptError(
            "--manifest-digest-output is only valid for broker-ttrpc packages"
        )
    if release_destination.exists():
        if not force:
            raise ScriptError(
                f"release directory already exists: {release_destination}; "
                "pass --force to replace it"
            )
        release_root = (REPO_ROOT / "dist").resolve()
        if (
            release_destination == release_root
            or release_root not in release_destination.parents
        ):
            raise ScriptError(
                "--force may only replace a version directory below dist/"
            )
        shutil.rmtree(release_destination)
    binary = openvmm_binary_path()
    provenance_path = require_file(
        artifact_path(OPENVMM_PROVENANCE_NAME),
        "OpenVMM build provenance",
    )
    provenance = json.loads(provenance_path.read_text(encoding="utf-8"))
    source_manifest = json.loads(
        (REPO_ROOT / "SOURCE-MANIFEST.json").read_text(encoding="utf-8")
    )
    expected_revision = source_manifest["openvmm"]["source_revision"]
    actual_binary_sha256 = sha256_file(binary)
    if (
        provenance.get("format") != 1
        or provenance.get("source_revision") != expected_revision
        or provenance.get("source_clean") is not True
        or provenance.get("executable_sha256") != actual_binary_sha256
    ):
        raise ScriptError(
            "OpenVMM build provenance does not match the clean pinned source and binary"
        )
    _copy_release_file(binary, release_destination / "bin" / binary.name)
    packaged_binary = release_destination / "bin" / binary.name
    if sha256_file(packaged_binary) != provenance["executable_sha256"]:
        raise ScriptError("packaged OpenVMM binary does not match its build provenance")
    for name, source in guest_inputs.items():
        _copy_release_file(source, release_destination / "guest" / name)
    if transport == "broker-ttrpc":
        packaged_agent = release_destination / "guest" / GUEST_AGENT_ARTIFACT_NAME
        expected_agent_sha256 = (
            require_file(
                artifact_path(GUEST_AGENT_SHA256_NAME),
                "staged NVX guest-agent SHA-256 pin",
            )
            .read_text(encoding="ascii")
            .strip()
        )
        if sha256_file(packaged_agent) != expected_agent_sha256:
            raise ScriptError("packaged NVX guest agent does not match its staged pin")
    for name in ("LICENSE", "README.md", "THIRD_PARTY_NOTICES.md"):
        _copy_release_file(REPO_ROOT / name, release_destination / name)
    runtime_manifest_path = release_destination / "SOURCE-MANIFEST.json"
    runtime_manifest_path.write_bytes(
        _runtime_source_manifest(release_destination, binary.name, transport)
    )
    runtime_manifest_path.chmod(0o644)
    _copy_release_file(
        OPENVMM_DIR / "LICENSE",
        release_destination / "licenses" / "LICENSE-OPENVMM",
    )
    _copy_release_file(
        REPO_ROOT / "kernel" / "COPYING-LINUX",
        release_destination / "licenses" / "COPYING-LINUX",
    )
    if include_source:
        source_destination = release_destination / "source"
        _copy_release_file(
            linux_source_archive,
            source_destination / linux_source_archive.name,
        )
        _project_source_archive(
            source_destination / f"nvx-project-source-{release_version}.tar.gz",
            release_version,
            package_manifests,
        )
        _alpine_source_archive(
            source_destination / f"nvx-alpine-source-{release_version}.tar.gz",
            release_version,
            package_manifests,
        )
    runtime_manifest = json.loads(runtime_manifest_path.read_text(encoding="utf-8"))
    _verify_runtime_fingerprint(runtime_manifest)
    packaged_initramfs = release_destination / "guest" / "initramfs.cpio.gz"
    if runtime_manifest["alpine"]["initramfs_sha256"] != sha256_file(
        packaged_initramfs
    ):
        raise ScriptError("packaged initramfs readback does not match runtime manifest")
    if transport == "broker-ttrpc":
        packaged_agent_sha256 = runtime_manifest["guest_agent"]["sha256"]
        verify_agent_initramfs(packaged_initramfs, packaged_agent_sha256)
    _normalize_release_tree_modes(release_destination)
    write_sha256_sums(release_destination)
    (release_destination / "SHA256SUMS").chmod(0o644)
    _validate_release_package_tree(
        release_destination,
        transport,
        transport == BROKER_TRANSPORT,
    )
    if transport == BROKER_TRANSPORT:
        assert digest_output is not None
        manifest_digest = sha256_file(runtime_manifest_path)
        digest_output.parent.mkdir(parents=True, exist_ok=True)
        temporary_digest = digest_output.with_name(f"{digest_output.name}.part")
        temporary_digest.write_text(f"{manifest_digest}\n", encoding="ascii")
        temporary_digest.chmod(0o644)
        temporary_digest.replace(digest_output)
        print(f">> external manifest digest: {digest_output}")
    print(f">> packaged {release_destination}")


def verify_source_tree() -> None:
    manifest = _validate_root_broker_contract()
    required = {
        REPO_ROOT / "kernel" / "config-microvm": "kernel build configuration",
        REPO_ROOT
        / "kernel"
        / "patches"
        / "0001-microvm-xe9-earlycon.patch": "xe9 early console patch",
        REPO_ROOT
        / "kernel"
        / "patches"
        / "0002-microvm-hvc-xe9.patch": "xe9 HVC patch",
        REPO_ROOT / "kernel" / "COPYING-LINUX": "Linux copyright notice",
        OPENVMM_DIR / "Cargo.toml": "initialized OpenVMM submodule",
    }
    required.update(
        {
            REPO_ROOT / patch: f"kernel patch {patch}"
            for patch in manifest["linux"]["patches"]
        }
    )
    for path, description in required.items():
        require_file(path, description)
    forbidden = (
        REPO_ROOT / "third_party" / "linux",
        REPO_ROOT / "third_party" / "alpine-sources",
    )
    present = [str(path) for path in forbidden if path.exists()]
    if present:
        raise ScriptError(
            "generated third-party sources must not be checked out here: "
            + ", ".join(present)
        )
    linux_manifest = cast(dict[str, object], manifest["linux"])
    expected_linux = {
        "version": DEFAULT_KERNEL_VERSION,
        "upstream_url": DEFAULT_KERNEL_URL,
        "upstream_archive_sha256": DEFAULT_KERNEL_SHA256,
    }
    for field, expected in expected_linux.items():
        if linux_manifest.get(field) != expected:
            raise ScriptError(
                f"SOURCE-MANIFEST.json Linux {field} does not match the build pin"
            )
    alpine_manifest = cast(dict[str, object], manifest["alpine"])
    expected_alpine = {
        "version": DEFAULT_ALPINE_VERSION,
        "branch": DEFAULT_ALPINE_BRANCH,
        "minirootfs_sha256": DEFAULT_ALPINE_MINIROOTFS_SHA256,
    }
    for field, expected in expected_alpine.items():
        if alpine_manifest.get(field) != expected:
            raise ScriptError(
                f"SOURCE-MANIFEST.json Alpine {field} does not match the build pin"
            )
    config_path = REPO_ROOT / "kernel" / "config-microvm"
    config = config_path.read_text(encoding="utf-8")
    for setting in (
        "CONFIG_PVH=y",
        "CONFIG_HVC_XE9=y",
        "CONFIG_VIRTIO_FS=y",
        "CONFIG_FUSE_FS=y",
    ):
        if setting not in config.splitlines():
            raise ScriptError(f"{config_path} is missing {setting}")
    hvc_patch_text = (
        REPO_ROOT / "kernel" / "patches" / "0002-microvm-hvc-xe9.patch"
    ).read_text(encoding="utf-8")
    for marker in ("config HVC_XE9", "hvc_xe9.o", "hvc_xe9.c"):
        if marker not in hvc_patch_text:
            raise ScriptError(f"xe9 HVC patch is missing {marker}")
    generated_config = artifact_path("vmlinux.config")
    if generated_config.is_file():
        generated = generated_config.read_text(encoding="utf-8").splitlines()
        for setting in (
            "CONFIG_PVH=y",
            "CONFIG_HVC_XE9=y",
            *REQUIRED_SANDBOX_KERNEL_CONFIG,
        ):
            if setting not in generated:
                raise ScriptError(f"{generated_config} is missing {setting}")
    head = subprocess.run(
        ["git", "-C", OPENVMM_DIR, "rev-parse", "HEAD"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    expected = subprocess.run(
        ["git", "-C", REPO_ROOT, "rev-parse", ":openvmm"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    if head != expected:
        raise ScriptError(f"OpenVMM submodule is at {head}, expected {expected}")
    openvmm_manifest = cast(dict[str, object], manifest["openvmm"])
    if openvmm_manifest.get("source_revision") != expected:
        raise ScriptError(
            "SOURCE-MANIFEST.json OpenVMM source revision does not match the submodule"
        )
    expected_agent = _root_broker_agent_contract()
    agent_manifest = cast(dict[str, object], manifest["guest_agent"])
    for field, value in expected_agent.items():
        if agent_manifest.get(field) != value:
            raise ScriptError(
                f"SOURCE-MANIFEST.json guest-agent {field} does not match the build contract"
            )
    print(">> source tree and submodule metadata are consistent")
