"""Source collection, verification, and release packaging workflows."""

from __future__ import annotations

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
import uuid
import zipfile
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import cast

from .archive import (
    canonical_source_archive_tree_digest,
    create_reproducible_release_archive,
    create_reproducible_tar_gz,
)
from .build import (
    CONTROL_CONTRACT_REVISION,
    CONTROL_SESSION_PROTOCOL_VERSION,
    DEFAULT_ALPINE_BRANCH,
    DEFAULT_ALPINE_MINIROOTFS_SHA256,
    DEFAULT_ALPINE_VERSION,
    DEFAULT_KERNEL_SHA256,
    DEFAULT_KERNEL_URL,
    DEFAULT_KERNEL_VERSION,
    KERNEL_PROVENANCE_NAME,
    MICROVM_ABI_VERSION,
    OPENVMM_PROVENANCE_NAME,
    REQUIRED_SANDBOX_KERNEL_CONFIG,
    DockerBuildConfig,
    assert_required_kernel_config,
    build_docker_linux_source,
    kernel_provenance_inputs,
)
from .collect_alpine_sources import collect_alpine_sources
from .common import (
    OPENVMM_DIR,
    REPO_ROOT,
    SOURCE_DIR,
    ScriptError,
    VerifiedChecksumInventory,
    artifact_path,
    credential_safe_opener,
    download,
    openvmm_binary_path,
    require_file,
    require_success,
    run_capture,
    sha256_file,
    verify_sha256_sums,
    write_sha256_sums,
)

PROJECT_SOURCE_PATHS = (
    "alpine",
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

GUEST_RELEASE_NAMES = (
    "vmlinux",
    "vmlinux.config",
    "initramfs.cpio.gz",
    "initramfs.cpio.gz.packages.json",
)
GITHUB_API_VERSION = "2022-11-28"


@dataclass(frozen=True)
class _ReleaseAsset:
    tag: str
    name: str
    url: str
    size: int


@dataclass(frozen=True)
class _SourceTreeInventory:
    directories: tuple[str, ...]
    files: tuple[tuple[str, str, int], ...]


@dataclass(frozen=True)
class _PackagingSourceSnapshot:
    root: Path
    project: Path
    project_inventory: _SourceTreeInventory
    alpine: Path
    alpine_inventory: _SourceTreeInventory
    linux_archive: Path
    linux_archive_sha256: str
    package_manifests: tuple[Path, ...]
    openvmm_license: Path


@dataclass(frozen=True)
class _PackagingFileSnapshot:
    root: Path
    project: Path
    openvmm_license: Path


class _GitHubReleaseQueryError(ScriptError):
    def __init__(self, status: int, message: str) -> None:
        self.status = status
        super().__init__(message)


class _ReleaseRestoreError(ScriptError):
    """Raised when release publication and restoration both fail."""


def create_release_archive(source: Path, destination: Path) -> None:
    if source.is_symlink():
        raise ScriptError(f"release package directory must not be a symlink: {source}")
    source = source.resolve()
    destination = destination.resolve()
    if destination == source or source in destination.parents:
        raise ScriptError("release archive destination must be outside its source")
    accepted_inventory = verify_sha256_sums(source)
    destination.parent.mkdir(parents=True, exist_ok=True)
    staging = destination.with_name(f".staging-{uuid.uuid4().hex}-{destination.name}")
    snapshot_parent = Path(
        tempfile.mkdtemp(
            prefix=f".snapshot-{uuid.uuid4().hex}-",
            dir=destination.parent,
        )
    )
    snapshot_parent.chmod(0o700)
    snapshot = snapshot_parent / source.name
    try:
        _capture_release_snapshot(source, snapshot, accepted_inventory)
        create_reproducible_release_archive(snapshot, staging)
        _verify_release_archive(staging, source.name, accepted_inventory)
        staging.replace(destination)
    finally:
        staging.unlink(missing_ok=True)
        shutil.rmtree(snapshot_parent, ignore_errors=True)
    print(f">> archived {source} as {destination}")


def _github_headers(token: str | None, accept: str) -> dict[str, str]:
    headers = {
        "Accept": accept,
        "X-GitHub-Api-Version": GITHUB_API_VERSION,
        "User-Agent": "nvx-release-downloader",
    }
    if token is not None:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def _github_error_message(error: urllib.error.HTTPError) -> str:
    try:
        payload = error.read().decode("utf-8", "replace").strip()
    except (AttributeError, OSError, ValueError):
        return ""
    if not payload:
        return ""
    try:
        document: object = json.loads(payload)
    except json.JSONDecodeError:
        return " ".join(payload.split())[:200]
    if isinstance(document, dict):
        message = cast(dict[str, object], document).get("message")
        if isinstance(message, str):
            return message
    return ""


def _github_error_hint(error: urllib.error.HTTPError, token: str | None) -> str:
    if token is None and error.code in (401, 403, 404):
        return "set GH_TOKEN to a token that can read the repository"
    if error.code == 401:
        return "the configured GitHub token was rejected; refresh or replace it"
    if error.code == 403:
        if error.headers.get("x-ratelimit-remaining") == "0":
            return "the GitHub API rate limit is exhausted; retry later"
        return (
            "the configured GitHub token lacks access; grant it read access to "
            "the repository contents and authorize it for organization single sign-on"
        )
    if error.code == 404:
        return (
            "verify --repository and that the configured GitHub token can read "
            "that repository"
        )
    return ""


def _latest_release_asset(
    repository: str,
    platform: str,
    token: str | None,
) -> _ReleaseAsset:
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
        message = _github_error_message(error)
        hint = _github_error_hint(error, token)
        detail = f": {message.rstrip('.')}" if message else ""
        advice = f"; {hint}" if hint else ""
        raise _GitHubReleaseQueryError(
            error.code,
            f"GitHub release query failed with HTTP {error.code}{detail}{advice}",
        ) from error
    except (OSError, urllib.error.URLError) as error:
        raise ScriptError(f"GitHub release query failed: {error}") from error
    if not isinstance(releases, list):
        raise ScriptError("GitHub release query returned an invalid response")

    extension = ".zip" if platform.startswith("windows-") else ".tar.gz"
    asset_pattern = re.compile(rf"^nvx-.+-{re.escape(platform)}{re.escape(extension)}$")
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
                and type(size) is int
            ):
                return _ReleaseAsset(tag, name, asset_url, size)
    raise ScriptError(f"no GitHub release contains an NVX package for {platform}")


def _latest_release_asset_with_fallback(
    repository: str,
    platform: str,
    token: str | None,
) -> tuple[_ReleaseAsset, str | None]:
    if token is None:
        return _latest_release_asset(repository, platform, None), None

    try:
        return _latest_release_asset(repository, platform, token), token
    except _GitHubReleaseQueryError as authenticated_error:
        if authenticated_error.status not in (401, 403):
            raise
        print(
            f">> {authenticated_error}; retrying without credentials",
            file=sys.stderr,
        )
        try:
            return _latest_release_asset(repository, platform, None), None
        except _GitHubReleaseQueryError as public_error:
            raise authenticated_error from public_error


def _same_file_identity(first: os.stat_result, second: os.stat_result) -> bool:
    return first.st_dev == second.st_dev and first.st_ino == second.st_ino


def _require_safe_source_parents(root: Path, relative: PurePosixPath) -> None:
    root_metadata = root.lstat()
    if stat.S_ISLNK(root_metadata.st_mode) or not stat.S_ISDIR(root_metadata.st_mode):
        raise ScriptError(f"release snapshot source is not a directory: {root}")
    current = root
    for part in relative.parts[:-1]:
        current /= part
        metadata = current.lstat()
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
            raise ScriptError(
                f"release snapshot source has an unsafe directory: {relative}"
            )


def _copy_pinned_regular_file(
    source_root: Path,
    snapshot_root: Path,
    relative_name: str,
    expected_sha256: str,
    *,
    expected_mode: int | None = None,
) -> None:
    relative = PurePosixPath(relative_name)
    _require_safe_source_parents(source_root, relative)
    source = source_root.joinpath(*relative.parts)
    before = source.lstat()
    if stat.S_ISLNK(before.st_mode) or not stat.S_ISREG(before.st_mode):
        raise ScriptError(
            f"release snapshot source is not a regular file: {relative_name}"
        )
    if expected_mode is not None and stat.S_IMODE(before.st_mode) != expected_mode:
        raise ScriptError(f"release snapshot source mode changed for {relative_name}")

    flags = os.O_RDONLY | getattr(os, "O_BINARY", 0)
    flags |= getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(source, flags)
    try:
        opened = os.fstat(descriptor)
        after_open = source.lstat()
        if (
            not stat.S_ISREG(opened.st_mode)
            or not stat.S_ISREG(after_open.st_mode)
            or not _same_file_identity(before, opened)
            or not _same_file_identity(opened, after_open)
        ):
            raise ScriptError(
                f"release snapshot source changed while opening: {relative_name}"
            )

        destination = snapshot_root.joinpath(*relative.parts)
        destination.parent.mkdir(parents=True, exist_ok=True)
        digest = hashlib.sha256()
        with os.fdopen(descriptor, "rb", closefd=False) as source_file:
            with destination.open("xb") as destination_file:
                while chunk := source_file.read(1024 * 1024):
                    destination_file.write(chunk)
                    digest.update(chunk)
            after_read = os.fstat(descriptor)
        after = source.lstat()
        if (
            not _same_file_identity(opened, after_read)
            or not _same_file_identity(after_read, after)
            or opened.st_size != after_read.st_size
            or opened.st_mtime_ns != after_read.st_mtime_ns
            or opened.st_ctime_ns != after_read.st_ctime_ns
        ):
            raise ScriptError(
                f"release snapshot source changed while reading: {relative_name}"
            )
        actual_sha256 = digest.hexdigest()
        if actual_sha256 != expected_sha256:
            raise ScriptError(
                f"release snapshot source changed for {relative_name}: "
                f"{actual_sha256}, expected {expected_sha256}"
            )
        destination.chmod(
            expected_mode
            if expected_mode is not None
            else (
                0o755 if relative.parts[0] == "bin" or before.st_mode & 0o111 else 0o644
            )
        )
    finally:
        os.close(descriptor)


def _pinned_regular_file_metadata(
    source_root: Path,
    relative_name: str,
) -> tuple[str, int]:
    relative = PurePosixPath(relative_name)
    _require_safe_source_parents(source_root, relative)
    source = source_root.joinpath(*relative.parts)
    before = source.lstat()
    if stat.S_ISLNK(before.st_mode) or not stat.S_ISREG(before.st_mode):
        raise ScriptError(
            f"release snapshot source is not a regular file: {relative_name}"
        )
    flags = os.O_RDONLY | getattr(os, "O_BINARY", 0)
    flags |= getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(source, flags)
    try:
        opened = os.fstat(descriptor)
        after_open = source.lstat()
        if (
            not stat.S_ISREG(opened.st_mode)
            or not stat.S_ISREG(after_open.st_mode)
            or not _same_file_identity(before, opened)
            or not _same_file_identity(opened, after_open)
        ):
            raise ScriptError(
                f"release snapshot source changed while opening: {relative_name}"
            )
        digest = hashlib.sha256()
        with os.fdopen(descriptor, "rb", closefd=False) as source_file:
            while chunk := source_file.read(1024 * 1024):
                digest.update(chunk)
        after_read = os.fstat(descriptor)
        after = source.lstat()
        if (
            not _same_file_identity(opened, after_read)
            or not _same_file_identity(after_read, after)
            or opened.st_size != after_read.st_size
            or opened.st_mtime_ns != after_read.st_mtime_ns
            or opened.st_ctime_ns != after_read.st_ctime_ns
        ):
            raise ScriptError(
                f"release snapshot source changed while reading: {relative_name}"
            )
        return digest.hexdigest(), stat.S_IMODE(opened.st_mode)
    finally:
        os.close(descriptor)


def _selected_tree_inventory(
    source_root: Path,
    selections: Sequence[str],
) -> _SourceTreeInventory:
    directories: set[str] = set()
    files: dict[str, tuple[str, int]] = {}

    def visit(relative: PurePosixPath) -> None:
        if "__pycache__" in relative.parts or relative.suffix in (".pyc", ".pyo"):
            return
        relative_name = relative.as_posix()
        path = source_root.joinpath(*relative.parts)
        before = path.lstat()
        if stat.S_ISLNK(before.st_mode):
            raise ScriptError(
                f"symlink is not allowed in release source: {relative_name}"
            )
        if stat.S_ISREG(before.st_mode):
            metadata = _pinned_regular_file_metadata(source_root, relative_name)
            prior = files.setdefault(relative_name, metadata)
            if prior != metadata:
                raise ScriptError(
                    f"overlapping release source changed: {relative_name}"
                )
            return
        if not stat.S_ISDIR(before.st_mode):
            raise ScriptError(
                f"special file is not allowed in release source: {relative_name}"
            )
        directories.add(relative_name)
        with os.scandir(path) as entries:
            children = sorted(entries, key=lambda entry: entry.name)
        for child in children:
            visit(relative / child.name)
        after = path.lstat()
        if (
            not stat.S_ISDIR(after.st_mode)
            or not _same_file_identity(before, after)
            or before.st_mtime_ns != after.st_mtime_ns
            or before.st_ctime_ns != after.st_ctime_ns
        ):
            raise ScriptError(f"release source changed while scanning: {relative_name}")

    for selection in selections:
        relative = PurePosixPath(selection)
        if (
            not relative.parts
            or relative.is_absolute()
            or "." in relative.parts
            or ".." in relative.parts
            or relative.as_posix() != selection
        ):
            raise ScriptError(f"unsafe release source path: {selection}")
        _require_safe_source_parents(source_root, relative)
        parent = relative.parent
        while parent.parts:
            directories.add(parent.as_posix())
            parent = parent.parent
        visit(relative)
    return _SourceTreeInventory(
        directories=tuple(sorted(directories)),
        files=tuple(
            (relative, digest, mode)
            for relative, (digest, mode) in sorted(files.items())
        ),
    )


def _capture_selected_tree(
    source_root: Path,
    snapshot_root: Path,
    selections: Sequence[str],
) -> _SourceTreeInventory:
    accepted = _selected_tree_inventory(source_root, selections)
    snapshot_root.mkdir(mode=0o700)
    for relative_name in accepted.directories:
        snapshot_root.joinpath(*PurePosixPath(relative_name).parts).mkdir(
            parents=True,
            exist_ok=True,
        )
    for relative_name, expected_sha256, expected_mode in accepted.files:
        _copy_pinned_regular_file(
            source_root,
            snapshot_root,
            relative_name,
            expected_sha256,
            expected_mode=expected_mode,
        )
    if _selected_tree_inventory(source_root, selections) != accepted:
        raise ScriptError("release source tree changed while its snapshot was captured")
    if _selected_tree_inventory(snapshot_root, selections) != accepted:
        raise ScriptError(
            "captured release source snapshot does not match its accepted inventory"
        )
    return accepted


def _capture_release_snapshot(
    source: Path,
    snapshot: Path,
    accepted: VerifiedChecksumInventory,
) -> None:
    snapshot.mkdir(mode=0o700)
    expected_files = dict(accepted.files)
    expected_files["SHA256SUMS"] = accepted.checksum_sha256
    for relative_name, expected_sha256 in sorted(expected_files.items()):
        _copy_pinned_regular_file(
            source,
            snapshot,
            relative_name,
            expected_sha256,
        )
    captured = verify_sha256_sums(snapshot)
    if captured != accepted:
        raise ScriptError(
            "captured release snapshot does not match the accepted source inventory"
        )


def _canonical_archive_member(name: str, *, is_directory: bool) -> str:
    normalized_name = name[:-1] if is_directory and name.endswith("/") else name
    path = PurePosixPath(normalized_name)
    if (
        not normalized_name
        or "\\" in normalized_name
        or path.is_absolute()
        or not path.parts
        or "." in path.parts
        or ".." in path.parts
        or any(":" in part for part in path.parts)
        or path.as_posix() != normalized_name
        or (not is_directory and name.endswith("/"))
    ):
        raise ScriptError(f"unsafe path in release archive: {name}")
    return normalized_name


def _validate_archive_layout(
    entries: Sequence[tuple[str, bool]],
    *,
    expected_root: str | None = None,
) -> tuple[str, dict[str, bool]]:
    layout: dict[str, bool] = {}
    for name, is_directory in entries:
        canonical = _canonical_archive_member(name, is_directory=is_directory)
        if canonical in layout:
            raise ScriptError(f"duplicate path in release archive: {canonical}")
        layout[canonical] = is_directory
    roots = {PurePosixPath(name).parts[0] for name in layout}
    if len(roots) != 1:
        raise ScriptError("release archive must contain exactly one package root")
    root = roots.pop()
    if expected_root is not None and root != expected_root:
        raise ScriptError(f"release archive root is {root}, expected {expected_root}")
    if root in layout and layout[root] is not True:
        raise ScriptError("release archive package root must be a directory")
    for name in layout:
        if name == root:
            continue
        parent = PurePosixPath(name).parent
        while parent != PurePosixPath("."):
            parent_name = parent.as_posix()
            if parent_name == root:
                if parent_name in layout and layout[parent_name] is not True:
                    raise ScriptError(f"colliding path in release archive: {name}")
                break
            if parent_name not in layout:
                raise ScriptError(
                    f"release archive omits directory entry {parent_name}"
                )
            if layout[parent_name] is not True:
                raise ScriptError(f"colliding path in release archive: {name}")
            parent = parent.parent
    return root, layout


def _release_archive_layout(
    archive_path: Path,
    *,
    expected_root: str | None = None,
) -> tuple[str, dict[str, bool]]:
    try:
        if archive_path.name.endswith(".tar.gz"):
            with tarfile.open(archive_path, "r:gz") as archive:
                entries: list[tuple[str, bool]] = []
                for member in archive.getmembers():
                    if not (member.isfile() or member.isdir()):
                        raise ScriptError(
                            f"unsupported entry in release archive: {member.name}"
                        )
                    entries.append((member.name, member.isdir()))
            return _validate_archive_layout(entries, expected_root=expected_root)
        if archive_path.suffix == ".zip":
            with zipfile.ZipFile(archive_path) as archive:
                entries = []
                for member in archive.infolist():
                    is_directory = member.is_dir()
                    file_type = (member.external_attr >> 16) & 0o170000
                    allowed_types = (
                        (0, stat.S_IFDIR) if is_directory else (0, stat.S_IFREG)
                    )
                    if file_type not in allowed_types:
                        raise ScriptError(
                            f"unsupported entry in release archive: {member.filename}"
                        )
                    entries.append((member.filename, is_directory))
            return _validate_archive_layout(entries, expected_root=expected_root)
    except (tarfile.TarError, zipfile.BadZipFile) as error:
        raise ScriptError(
            f"invalid release archive {archive_path.name}: {error}"
        ) from error
    raise ScriptError(f"unsupported release archive: {archive_path.name}")


def _extract_release_archive(archive_path: Path, destination: Path) -> None:
    _release_archive_layout(archive_path)
    try:
        if archive_path.name.endswith(".tar.gz"):
            with tarfile.open(archive_path, "r:gz") as archive:
                for member in archive.getmembers():
                    canonical = _canonical_archive_member(
                        member.name,
                        is_directory=member.isdir(),
                    )
                    target = destination.joinpath(*PurePosixPath(canonical).parts)
                    if member.isdir():
                        target.mkdir(parents=True, exist_ok=True)
                        target.chmod(member.mode & 0o777)
                        continue
                    target.parent.mkdir(parents=True, exist_ok=True)
                    source = archive.extractfile(member)
                    if source is None:
                        raise ScriptError(
                            f"could not read release archive entry: {member.name}"
                        )
                    with source, target.open("xb") as output:
                        shutil.copyfileobj(source, output, length=1024 * 1024)
                    target.chmod(member.mode & 0o777)
            return
        if archive_path.suffix == ".zip":
            with zipfile.ZipFile(archive_path) as archive:
                for member in archive.infolist():
                    is_directory = member.is_dir()
                    canonical = _canonical_archive_member(
                        member.filename,
                        is_directory=is_directory,
                    )
                    target = destination.joinpath(*PurePosixPath(canonical).parts)
                    mode = (member.external_attr >> 16) & 0o777
                    if is_directory:
                        target.mkdir(parents=True, exist_ok=True)
                        if mode:
                            target.chmod(mode)
                        continue
                    target.parent.mkdir(parents=True, exist_ok=True)
                    with archive.open(member) as source, target.open("xb") as output:
                        shutil.copyfileobj(source, output, length=1024 * 1024)
                    if mode:
                        target.chmod(mode)
            return
    except (tarfile.TarError, zipfile.BadZipFile) as error:
        raise ScriptError(
            f"invalid release archive {archive_path.name}: {error}"
        ) from error
    raise ScriptError(f"unsupported release archive: {archive_path.name}")


def _filesystem_archive_layout(destination: Path) -> dict[str, bool]:
    layout: dict[str, bool] = {}
    for path in destination.rglob("*"):
        metadata = path.lstat()
        relative = path.relative_to(destination).as_posix()
        if stat.S_ISLNK(metadata.st_mode):
            raise ScriptError(f"release archive extracted a symlink: {relative}")
        if stat.S_ISDIR(metadata.st_mode):
            layout[relative] = True
        elif stat.S_ISREG(metadata.st_mode):
            layout[relative] = False
        else:
            raise ScriptError(f"release archive extracted a special file: {relative}")
    return layout


def _verify_release_archive(
    archive_path: Path,
    expected_root: str,
    accepted: VerifiedChecksumInventory,
) -> None:
    root, archived_layout = _release_archive_layout(
        archive_path,
        expected_root=expected_root,
    )
    extraction = Path(
        tempfile.mkdtemp(
            prefix=f".{archive_path.name}.verify-",
            dir=archive_path.parent,
        )
    )
    try:
        _extract_release_archive(archive_path, extraction)
        extracted_layout = _filesystem_archive_layout(extraction)
        if extracted_layout != archived_layout:
            raise ScriptError(
                "release archive extraction did not preserve its exact member layout"
            )
        archived = verify_sha256_sums(extraction / root)
        if archived != accepted:
            raise ScriptError(
                "release archive does not match the accepted source inventory"
            )
    finally:
        shutil.rmtree(extraction, ignore_errors=True)


def _verify_source_archive_inventory(
    archive_path: Path,
    expected_root: str,
    accepted: _SourceTreeInventory,
) -> None:
    expected_files = {
        f"{expected_root}/{relative}": (digest, mode)
        for relative, digest, mode in accepted.files
    }
    expected_directories = {
        expected_root,
        *(f"{expected_root}/{relative}" for relative in accepted.directories),
    }
    actual_files: dict[str, tuple[str, int]] = {}
    actual_directories: set[str] = set()
    try:
        with tarfile.open(archive_path, "r:gz") as archive:
            for member in archive.getmembers():
                canonical = _canonical_archive_member(
                    member.name,
                    is_directory=member.isdir(),
                )
                if member.isdir():
                    if member.mode & 0o777 != 0o755:
                        raise ScriptError(
                            f"non-canonical directory mode in source archive: "
                            f"{member.name}"
                        )
                    if canonical in actual_directories or canonical in actual_files:
                        raise ScriptError(
                            f"duplicate path in source archive: {canonical}"
                        )
                    actual_directories.add(canonical)
                    continue
                if not member.isfile():
                    raise ScriptError(
                        f"unsupported entry in source archive: {member.name}"
                    )
                if canonical in actual_files or canonical in actual_directories:
                    raise ScriptError(f"duplicate path in source archive: {canonical}")
                source = archive.extractfile(member)
                if source is None:
                    raise ScriptError(
                        f"could not read source archive entry: {member.name}"
                    )
                digest = hashlib.sha256()
                with source:
                    while chunk := source.read(1024 * 1024):
                        digest.update(chunk)
                actual_files[canonical] = (
                    digest.hexdigest(),
                    member.mode & 0o777,
                )
    except tarfile.TarError as error:
        raise ScriptError(
            f"invalid source archive {archive_path.name}: {error}"
        ) from error
    if actual_files != expected_files or actual_directories != expected_directories:
        raise ScriptError(
            f"{archive_path} does not match the accepted source inventory"
        )


def _replace_runtime_file(source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(f"{destination.name}.part")
    temporary.unlink(missing_ok=True)
    try:
        shutil.copy2(source, temporary)
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)


def _install_release_archive(archive_path: Path) -> None:
    with tempfile.TemporaryDirectory(prefix="nvx-release-") as temporary:
        extraction_root = Path(temporary)
        _extract_release_archive(archive_path, extraction_root)
        checksum_files = list(extraction_root.glob("*/SHA256SUMS"))
        if len(checksum_files) != 1:
            raise ScriptError(
                "release archive must contain one package root with SHA256SUMS"
            )
        package_root = checksum_files[0].parent
        verify_sha256_sums(package_root)

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
        provenance_sources = {
            name: require_file(
                package_root / "provenance" / name,
                f"packaged provenance artifact {name}",
            )
            for name in (OPENVMM_PROVENANCE_NAME, KERNEL_PROVENANCE_NAME)
        }
        _replace_runtime_file(binary_source, binary_destination)
        for name, source in guest_sources.items():
            _replace_runtime_file(source, artifact_path(name))
        for name, source in provenance_sources.items():
            _replace_runtime_file(source, artifact_path(name))


def download_latest_release(repository: str, platform: str) -> None:
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    asset, download_token = _latest_release_asset_with_fallback(
        repository,
        platform,
        token,
    )
    print(f">> downloading {asset.name} from {asset.tag}")
    with tempfile.TemporaryDirectory(prefix="nvx-download-") as temporary:
        archive_path = Path(temporary) / asset.name
        download(
            asset.url,
            archive_path,
            headers=_github_headers(download_token, "application/octet-stream"),
            opener=credential_safe_opener(),
        )
        actual_size = archive_path.stat().st_size
        if actual_size != asset.size:
            raise ScriptError(
                f"downloaded {asset.name} is {actual_size} bytes, expected {asset.size}"
            )
        _install_release_archive(archive_path)
    print(f">> installed {asset.tag} for {platform}")


def _copy_release_file(source: Path, destination: Path) -> None:
    require_file(source, source.name)
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination)


def _validate_alpine_sources(
    package_manifests: Sequence[Path],
    *,
    source_root: Path | None = None,
) -> None:
    source_root = source_root or SOURCE_DIR / "alpine"
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
    package_manifests: Sequence[Path],
    *,
    snapshot: Path | None = None,
) -> None:
    root = f"nvx-project-source-{version}"
    if snapshot is not None:
        _create_source_archive(output, [(snapshot, root)])
        return
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
    package_manifests: Sequence[Path],
    *,
    snapshot: Path | None = None,
) -> None:
    root = f"nvx-alpine-source-{version}"
    if snapshot is not None:
        _create_source_archive(output, [(snapshot, root)])
        return
    inputs = [(SOURCE_DIR / "alpine", f"{root}/sources")]
    inputs.extend(
        (
            manifest,
            f"{root}/manifests/{manifest.name}",
        )
        for manifest in package_manifests
    )
    _create_source_archive(output, inputs)


def _validate_linux_source_archive(
    path: Path,
    *,
    repo_root: Path | None = None,
    kernel_config: Path | None = None,
    expected_tree_sha256: str,
) -> None:
    repo_root = repo_root or REPO_ROOT
    kernel_config = kernel_config or artifact_path("vmlinux.config")
    expected_members = {
        "vmlinux.config": kernel_config.read_bytes(),
        "SOURCE-MANIFEST.json": (repo_root / "SOURCE-MANIFEST.json").read_bytes(),
    }
    manifest = json.loads(expected_members["SOURCE-MANIFEST.json"])
    expected_members.update(
        {
            patch: (repo_root / patch).read_bytes()
            for patch in manifest["linux"]["patches"]
        }
    )
    package_root = f"nvx-linux-source-{DEFAULT_KERNEL_VERSION}"
    archived_tree_sha256 = canonical_source_archive_tree_digest(
        path,
        package_root=package_root,
        tree_root=f"{package_root}/linux-{DEFAULT_KERNEL_VERSION}",
    )
    if archived_tree_sha256 != expected_tree_sha256:
        raise ScriptError(
            f"{path} Linux tree does not match the kernel compilation source snapshot"
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


def _capture_packaging_source_snapshot(
    package_manifests: Sequence[Path],
    linux_source_archive: Path,
    parent: Path,
    root_manifest: dict[str, object],
    kernel_provenance: dict[str, object],
) -> _PackagingSourceSnapshot:
    snapshot_root = Path(
        tempfile.mkdtemp(
            prefix=".nvx-source-snapshot-",
            dir=parent,
        )
    )
    snapshot_root.chmod(0o700)
    try:
        project = snapshot_root / "project"
        project_selections = [
            *PROJECT_SOURCE_PATHS,
            *(path.relative_to(REPO_ROOT).as_posix() for path in package_manifests),
            artifact_path("vmlinux.config").relative_to(REPO_ROOT).as_posix(),
        ]
        project_inventory = _capture_selected_tree(
            REPO_ROOT,
            project,
            project_selections,
        )
        captured_manifest = _read_json_object(
            project / "SOURCE-MANIFEST.json",
            "captured source manifest",
        )
        if captured_manifest != root_manifest:
            raise ScriptError(
                "source manifest changed while release sources were captured"
            )
        captured_config = project / artifact_path("vmlinux.config").relative_to(
            REPO_ROOT
        )
        if sha256_file(captured_config) != kernel_provenance["config_sha256"]:
            raise ScriptError(
                "kernel config changed while release sources were captured"
            )
        captured_manifests = tuple(
            project / path.relative_to(REPO_ROOT) for path in package_manifests
        )

        alpine = snapshot_root / "alpine"
        alpine.mkdir(mode=0o700)
        alpine_sources = alpine / "sources"
        accepted_alpine = verify_sha256_sums(SOURCE_DIR / "alpine")
        _capture_release_snapshot(
            SOURCE_DIR / "alpine",
            alpine_sources,
            accepted_alpine,
        )
        alpine_manifests = alpine / "manifests"
        alpine_manifests.mkdir()
        captured_alpine_manifests: list[Path] = []
        for manifest in captured_manifests:
            destination = alpine_manifests / manifest.name
            shutil.copy2(manifest, destination)
            captured_alpine_manifests.append(destination)
        _validate_alpine_sources(
            captured_alpine_manifests,
            source_root=alpine_sources,
        )
        alpine_inventory = _selected_tree_inventory(
            alpine,
            ("sources", "manifests"),
        )

        linux = snapshot_root / "linux"
        linux.mkdir(mode=0o700)
        linux_archive_sha256, linux_archive_mode = _pinned_regular_file_metadata(
            linux_source_archive.parent,
            linux_source_archive.name,
        )
        _copy_pinned_regular_file(
            linux_source_archive.parent,
            linux,
            linux_source_archive.name,
            linux_archive_sha256,
            expected_mode=linux_archive_mode,
        )
        captured_linux_archive = linux / linux_source_archive.name
        _validate_linux_source_archive(
            captured_linux_archive,
            repo_root=project,
            kernel_config=captured_config,
            expected_tree_sha256=cast(
                str,
                cast(dict[str, object], kernel_provenance["source"])["tree_sha256"],
            ),
        )
        openvmm = snapshot_root / "openvmm"
        openvmm.mkdir(mode=0o700)
        openvmm_license_sha256, openvmm_license_mode = _pinned_regular_file_metadata(
            OPENVMM_DIR, "LICENSE"
        )
        _copy_pinned_regular_file(
            OPENVMM_DIR,
            openvmm,
            "LICENSE",
            openvmm_license_sha256,
            expected_mode=openvmm_license_mode,
        )
        return _PackagingSourceSnapshot(
            root=snapshot_root,
            project=project,
            project_inventory=project_inventory,
            alpine=alpine,
            alpine_inventory=alpine_inventory,
            linux_archive=captured_linux_archive,
            linux_archive_sha256=linux_archive_sha256,
            package_manifests=captured_manifests,
            openvmm_license=openvmm / "LICENSE",
        )
    except Exception:
        shutil.rmtree(snapshot_root, ignore_errors=True)
        raise


def _capture_packaging_file_snapshot(parent: Path) -> _PackagingFileSnapshot:
    snapshot_root = Path(
        tempfile.mkdtemp(
            prefix=".nvx-package-files-",
            dir=parent,
        )
    )
    snapshot_root.chmod(0o700)
    try:
        project = snapshot_root / "project"
        _capture_selected_tree(
            REPO_ROOT,
            project,
            (
                "LICENSE",
                "README.md",
                "THIRD_PARTY_NOTICES.md",
                "kernel/COPYING-LINUX",
            ),
        )
        openvmm = snapshot_root / "openvmm"
        openvmm.mkdir(mode=0o700)
        license_sha256, license_mode = _pinned_regular_file_metadata(
            OPENVMM_DIR,
            "LICENSE",
        )
        _copy_pinned_regular_file(
            OPENVMM_DIR,
            openvmm,
            "LICENSE",
            license_sha256,
            expected_mode=license_mode,
        )
        return _PackagingFileSnapshot(
            root=snapshot_root,
            project=project,
            openvmm_license=openvmm / "LICENSE",
        )
    except Exception:
        shutil.rmtree(snapshot_root, ignore_errors=True)
        raise


def _guest_release_inputs() -> tuple[list[str], list[Path]]:
    for name in GUEST_RELEASE_NAMES:
        require_file(artifact_path(name), f"required guest artifact {name}")
    guest_names: list[str] = list(GUEST_RELEASE_NAMES)
    package_manifests = [artifact_path("initramfs.cpio.gz.packages.json")]
    return guest_names, package_manifests


def _read_json_object(path: Path, description: str) -> dict[str, object]:
    try:
        value: object = json.loads(path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError) as error:
        raise ScriptError(f"invalid {description}: {error}") from error
    if not isinstance(value, dict):
        raise ScriptError(f"invalid {description}: expected a JSON object")
    return cast(dict[str, object], value)


def _openvmm_git_state() -> tuple[str, bool]:
    head = run_capture(["git", "-C", OPENVMM_DIR, "rev-parse", "HEAD"])
    require_success(head, "OpenVMM revision query")
    gitlink = run_capture(["git", "-C", REPO_ROOT, "rev-parse", ":openvmm"])
    require_success(gitlink, "OpenVMM gitlink query")
    status = run_capture(["git", "-C", OPENVMM_DIR, "status", "--porcelain"])
    require_success(status, "OpenVMM status query")
    revision = head.stdout.decode("ascii").strip()
    expected_revision = gitlink.stdout.decode("ascii").strip()
    if revision != expected_revision:
        raise ScriptError(
            f"OpenVMM submodule is at {revision}, expected {expected_revision}"
        )
    return revision, not status.stdout.strip()


def _validate_openvmm_provenance(
    binary: Path,
    provenance_path: Path,
) -> dict[str, object]:
    provenance = _read_json_object(provenance_path, "OpenVMM build provenance")
    revision, source_clean = _openvmm_git_state()
    if (
        type(provenance.get("format")) is not int
        or provenance.get("format") != 1
        or provenance.get("source_revision") != revision
        or provenance.get("source_clean") is not True
        or not source_clean
        or provenance.get("executable_sha256") != sha256_file(binary)
    ):
        raise ScriptError(
            "OpenVMM build provenance does not match the current clean pinned "
            "source and executable"
        )
    return provenance


def _validate_kernel_provenance(
    kernel: Path,
    kernel_config: Path,
    provenance_path: Path,
) -> dict[str, object]:
    provenance = _read_json_object(provenance_path, "kernel build provenance")
    expected_inputs = kernel_provenance_inputs()
    if (
        type(provenance.get("format")) is not int
        or provenance.get("format") != 1
        or not isinstance(provenance.get("source"), dict)
        or any(
            cast(dict[str, object], provenance["source"]).get(field) != expected
            for field, expected in cast(
                dict[str, object],
                expected_inputs["source"],
            ).items()
        )
        or not isinstance(
            cast(dict[str, object], provenance["source"]).get("tree_sha256"),
            str,
        )
        or re.fullmatch(
            r"[0-9a-f]{64}",
            cast(
                str,
                cast(dict[str, object], provenance["source"]).get("tree_sha256"),
            ),
        )
        is None
        or provenance.get("input_config") != expected_inputs["input_config"]
        or provenance.get("kernel_sha256") != sha256_file(kernel)
        or provenance.get("config_sha256") != sha256_file(kernel_config)
    ):
        raise ScriptError(
            "kernel build provenance does not match the current source, config, "
            "and vmlinux"
        )
    assert_required_kernel_config(kernel_config)
    return provenance


def _packaged_source_manifest(
    root_manifest: dict[str, object],
    release_version: str,
    release_root: Path,
    openvmm_provenance: dict[str, object],
    binary_name: str,
) -> bytes:
    distribution = root_manifest.get("distribution")
    openvmm = root_manifest.get("openvmm")
    linux = root_manifest.get("linux")
    alpine = root_manifest.get("alpine")
    if not all(
        isinstance(section, dict) for section in (distribution, openvmm, linux, alpine)
    ):
        raise ScriptError("SOURCE-MANIFEST.json is missing a required object")
    distribution_section = cast(dict[str, object], distribution)
    openvmm_section = cast(dict[str, object], openvmm)
    linux_section = cast(dict[str, object], linux)
    alpine_section = cast(dict[str, object], alpine)
    distribution_section["version"] = release_version
    openvmm_section["source_revision"] = openvmm_provenance["source_revision"]
    openvmm_section["executable_sha256"] = sha256_file(
        release_root / "bin" / binary_name
    )
    linux_section["kernel_sha256"] = sha256_file(release_root / "guest" / "vmlinux")
    linux_section["config_sha256"] = sha256_file(
        release_root / "guest" / "vmlinux.config"
    )
    alpine_section["initramfs_sha256"] = sha256_file(
        release_root / "guest" / "initramfs.cpio.gz"
    )
    alpine_section["initramfs_package_manifest_sha256"] = sha256_file(
        release_root / "guest" / "initramfs.cpio.gz.packages.json"
    )
    return (json.dumps(root_manifest, indent=2, sort_keys=True) + "\n").encode("utf-8")


def _validate_root_manifest_contract(manifest: dict[str, object]) -> None:
    distribution = manifest.get("distribution")
    openvmm = manifest.get("openvmm")
    if not isinstance(distribution, dict):
        raise ScriptError("SOURCE-MANIFEST.json distribution must identify nvx")
    distribution_section = cast(dict[str, object], distribution)
    if distribution_section.get("name") != "nvx":
        raise ScriptError("SOURCE-MANIFEST.json distribution must identify nvx")
    expected_openvmm = {
        "microvm_abi_version": MICROVM_ABI_VERSION,
        "control_session_protocol_version": CONTROL_SESSION_PROTOCOL_VERSION,
        "control_contract_revision": CONTROL_CONTRACT_REVISION,
    }
    if not isinstance(openvmm, dict):
        raise ScriptError(
            "SOURCE-MANIFEST.json OpenVMM contract does not match the build contract"
        )
    openvmm_section = cast(dict[str, object], openvmm)
    for field, expected in expected_openvmm.items():
        actual = openvmm_section.get(field)
        if (
            type(expected) is int and (type(actual) is not int or actual != expected)
        ) or (type(expected) is not int and actual != expected):
            raise ScriptError(
                "SOURCE-MANIFEST.json OpenVMM contract does not match the "
                "build contract"
            )
    if "source_revision" in openvmm_section:
        raise ScriptError(
            "root SOURCE-MANIFEST.json must not duplicate the OpenVMM gitlink revision"
        )
    if "guest_agent" in manifest:
        raise ScriptError(
            "root SOURCE-MANIFEST.json must not contain product guest-agent metadata"
        )


def _validate_source_manifest_metadata(
    manifest: dict[str, object],
    kernel_inputs: dict[str, object],
) -> list[str]:
    _validate_root_manifest_contract(manifest)
    if type(manifest.get("format")) is not int or manifest.get("format") != 1:
        raise ScriptError("SOURCE-MANIFEST.json format must be 1")
    linux_value = manifest.get("linux")
    alpine_value = manifest.get("alpine")
    source_value = kernel_inputs.get("source")
    config_value = kernel_inputs.get("input_config")
    if not all(
        isinstance(section, dict)
        for section in (linux_value, alpine_value, source_value, config_value)
    ):
        raise ScriptError("SOURCE-MANIFEST.json is missing source metadata")
    linux = cast(dict[str, object], linux_value)
    alpine = cast(dict[str, object], alpine_value)
    source = cast(dict[str, object], source_value)
    input_config = cast(dict[str, object], config_value)
    source_patches = source.get("patches")
    if not isinstance(source_patches, list):
        raise ScriptError("kernel provenance source patches must be a list")
    patch_paths: list[str] = []
    for patch_value in cast(list[object], source_patches):
        if not isinstance(patch_value, dict):
            raise ScriptError("kernel provenance source patch must be an object")
        patch = cast(dict[str, object], patch_value)
        path = patch.get("path")
        if not isinstance(path, str):
            raise ScriptError("kernel provenance source patch path must be a string")
        patch_paths.append(path)
    expected_linux: dict[str, object] = {
        "version": DEFAULT_KERNEL_VERSION,
        "upstream_url": DEFAULT_KERNEL_URL,
        "upstream_archive_sha256": DEFAULT_KERNEL_SHA256,
        "source_cache": f".cache/linux/linux-{DEFAULT_KERNEL_VERSION}",
        "source_archive": (
            f"build/sources/linux/nvx-linux-source-{DEFAULT_KERNEL_VERSION}.tar.gz"
        ),
        "generated_final_config": "build/vmlinux.config",
        "input_config": input_config.get("path"),
        "patches": patch_paths,
    }
    for field, expected in expected_linux.items():
        if linux.get(field) != expected:
            raise ScriptError(
                f"SOURCE-MANIFEST.json Linux {field} does not match the build pin"
            )
    expected_alpine: dict[str, object] = {
        "version": DEFAULT_ALPINE_VERSION,
        "branch": DEFAULT_ALPINE_BRANCH,
        "architecture": "x86_64",
        "minirootfs_url": (
            "https://dl-cdn.alpinelinux.org/alpine/"
            f"{DEFAULT_ALPINE_BRANCH}/releases/x86_64/"
            f"alpine-minirootfs-{DEFAULT_ALPINE_VERSION}-x86_64.tar.gz"
        ),
        "minirootfs_sha256": DEFAULT_ALPINE_MINIROOTFS_SHA256,
        "guest_sources": "alpine",
        "package_manifests": "build/*.packages.json",
        "source_output": "build/sources/alpine",
    }
    for field, expected in expected_alpine.items():
        if alpine.get(field) != expected:
            raise ScriptError(
                f"SOURCE-MANIFEST.json Alpine {field} does not match the build pin"
            )
    return patch_paths


def _validate_release_replacement(destination: Path, force: bool) -> None:
    if not destination.exists():
        return
    if not force:
        raise ScriptError(
            f"release directory already exists: {destination}; "
            "pass --force to replace it"
        )
    release_root = (REPO_ROOT / "dist").resolve()
    if destination == release_root or release_root not in destination.parents:
        raise ScriptError("--force may only replace a version directory below dist/")


def _publish_release_directory(
    staging: Path,
    destination: Path,
    *,
    force: bool,
) -> None:
    backup = destination.with_name(f".{destination.name}.backup-{uuid.uuid4().hex}")
    moved_prior = False
    try:
        _validate_release_replacement(destination, force)
        if destination.exists():
            destination.replace(backup)
            moved_prior = True
        staging.replace(destination)
    except ScriptError:
        shutil.rmtree(staging)
        raise
    except OSError as publish_error:
        if moved_prior:
            try:
                backup.replace(destination)
            except OSError as restore_error:
                raise _ReleaseRestoreError(
                    "failed to publish the staged release and restore the prior "
                    f"release; prior release remains at {backup} and staged "
                    f"release remains at {staging}: {restore_error}"
                ) from publish_error
        shutil.rmtree(staging, ignore_errors=True)
        raise ScriptError(f"failed to publish staged release: {publish_error}") from (
            publish_error
        )
    if moved_prior:
        shutil.rmtree(backup)


def collect_release_sources() -> None:
    _guest_names, package_manifests = _guest_release_inputs()
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
) -> None:
    guest_names, package_manifests = _guest_release_inputs()
    linux_source_archive = (
        SOURCE_DIR / "linux" / f"nvx-linux-source-{DEFAULT_KERNEL_VERSION}.tar.gz"
    )
    if not include_source:
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
    _validate_release_replacement(release_destination, force)
    binary = require_file(openvmm_binary_path(), "OpenVMM release binary")
    kernel = require_file(artifact_path("vmlinux"), "required guest artifact vmlinux")
    kernel_config = require_file(
        artifact_path("vmlinux.config"),
        "required guest artifact vmlinux.config",
    )
    openvmm_provenance_path = require_file(
        artifact_path(OPENVMM_PROVENANCE_NAME),
        "OpenVMM build provenance",
    )
    kernel_provenance_path = require_file(
        artifact_path(KERNEL_PROVENANCE_NAME),
        "kernel build provenance",
    )
    openvmm_provenance = _validate_openvmm_provenance(
        binary,
        openvmm_provenance_path,
    )
    kernel_provenance = _validate_kernel_provenance(
        kernel,
        kernel_config,
        kernel_provenance_path,
    )
    root_manifest_path = require_file(
        REPO_ROOT / "SOURCE-MANIFEST.json",
        "source manifest",
    )
    root_manifest = _read_json_object(root_manifest_path, "source manifest")
    _validate_source_manifest_metadata(root_manifest, kernel_provenance)
    for name in ("LICENSE", "README.md", "THIRD_PARTY_NOTICES.md"):
        require_file(REPO_ROOT / name, name)
    require_file(OPENVMM_DIR / "LICENSE", "OpenVMM license")
    require_file(REPO_ROOT / "kernel" / "COPYING-LINUX", "Linux copyright notice")

    release_destination.parent.mkdir(parents=True, exist_ok=True)
    source_snapshot: _PackagingSourceSnapshot | None = None
    file_snapshot: _PackagingFileSnapshot | None = None
    if include_source:
        require_file(linux_source_archive, "Linux corresponding-source archive")
        source_snapshot = _capture_packaging_source_snapshot(
            package_manifests,
            linux_source_archive,
            artifact_path("vmlinux.config").parent,
            root_manifest,
            kernel_provenance,
        )
    else:
        file_snapshot = _capture_packaging_file_snapshot(
            artifact_path("vmlinux.config").parent
        )
    try:
        staging = Path(
            tempfile.mkdtemp(
                prefix=f".{release_destination.name}.staging-",
                dir=release_destination.parent,
            )
        )
    except OSError:
        if source_snapshot is not None:
            shutil.rmtree(source_snapshot.root, ignore_errors=True)
        if file_snapshot is not None:
            shutil.rmtree(file_snapshot.root, ignore_errors=True)
        raise
    preserve_staging = False
    try:
        _copy_release_file(binary, staging / "bin" / binary.name)
        for name in guest_names:
            _copy_release_file(artifact_path(name), staging / "guest" / name)
        _copy_release_file(
            openvmm_provenance_path,
            staging / "provenance" / OPENVMM_PROVENANCE_NAME,
        )
        _copy_release_file(
            kernel_provenance_path,
            staging / "provenance" / KERNEL_PROVENANCE_NAME,
        )
        project_snapshot = (
            source_snapshot.project
            if source_snapshot is not None
            else cast(_PackagingFileSnapshot, file_snapshot).project
        )
        for name in ("LICENSE", "README.md", "THIRD_PARTY_NOTICES.md"):
            _copy_release_file(project_snapshot / name, staging / name)
        openvmm_license = (
            source_snapshot.openvmm_license
            if source_snapshot is not None
            else cast(_PackagingFileSnapshot, file_snapshot).openvmm_license
        )
        _copy_release_file(
            openvmm_license,
            staging / "licenses" / "LICENSE-OPENVMM",
        )
        _copy_release_file(
            project_snapshot / "kernel" / "COPYING-LINUX",
            staging / "licenses" / "COPYING-LINUX",
        )
        if include_source:
            assert source_snapshot is not None
            source_destination = staging / "source"
            _copy_release_file(
                source_snapshot.linux_archive,
                source_destination / source_snapshot.linux_archive.name,
            )
            packaged_linux_source = (
                source_destination / source_snapshot.linux_archive.name
            )
            if (
                sha256_file(packaged_linux_source)
                != source_snapshot.linux_archive_sha256
            ):
                raise ScriptError("packaged Linux source archive changed while staging")
            project_archive = (
                source_destination / f"nvx-project-source-{release_version}.tar.gz"
            )
            _project_source_archive(
                project_archive,
                release_version,
                source_snapshot.package_manifests,
                snapshot=source_snapshot.project,
            )
            _verify_source_archive_inventory(
                project_archive,
                f"nvx-project-source-{release_version}",
                source_snapshot.project_inventory,
            )
            alpine_archive = (
                source_destination / f"nvx-alpine-source-{release_version}.tar.gz"
            )
            _alpine_source_archive(
                alpine_archive,
                release_version,
                source_snapshot.package_manifests,
                snapshot=source_snapshot.alpine,
            )
            _verify_source_archive_inventory(
                alpine_archive,
                f"nvx-alpine-source-{release_version}",
                source_snapshot.alpine_inventory,
            )
        packaged_binary = staging / "bin" / binary.name
        packaged_kernel = staging / "guest" / "vmlinux"
        packaged_config = staging / "guest" / "vmlinux.config"
        packaged_openvmm_provenance = staging / "provenance" / OPENVMM_PROVENANCE_NAME
        packaged_kernel_provenance = staging / "provenance" / KERNEL_PROVENANCE_NAME
        if source_snapshot is not None:
            for manifest in source_snapshot.package_manifests:
                packaged_manifest = staging / "guest" / manifest.name
                if sha256_file(packaged_manifest) != sha256_file(manifest):
                    raise ScriptError(
                        f"packaged source manifest changed while staging: "
                        f"{manifest.name}"
                    )
        if sha256_file(packaged_binary) != openvmm_provenance["executable_sha256"]:
            raise ScriptError(
                "packaged OpenVMM executable does not match its build provenance"
            )
        if sha256_file(packaged_kernel) != kernel_provenance["kernel_sha256"]:
            raise ScriptError("packaged kernel does not match its build provenance")
        if sha256_file(packaged_config) != kernel_provenance["config_sha256"]:
            raise ScriptError(
                "packaged kernel config does not match its build provenance"
            )
        if (
            _read_json_object(
                packaged_openvmm_provenance,
                "packaged OpenVMM build provenance",
            )
            != openvmm_provenance
        ):
            raise ScriptError("packaged OpenVMM provenance changed while staging")
        if (
            _read_json_object(
                packaged_kernel_provenance,
                "packaged kernel build provenance",
            )
            != kernel_provenance
        ):
            raise ScriptError("packaged kernel provenance changed while staging")
        (staging / "SOURCE-MANIFEST.json").write_bytes(
            _packaged_source_manifest(
                root_manifest,
                release_version,
                staging,
                openvmm_provenance,
                binary.name,
            )
        )
        write_sha256_sums(staging)
        verify_sha256_sums(staging)
        _publish_release_directory(staging, release_destination, force=force)
    except _ReleaseRestoreError:
        preserve_staging = True
        raise
    finally:
        if staging.exists() and not preserve_staging:
            shutil.rmtree(staging)
        if source_snapshot is not None:
            shutil.rmtree(source_snapshot.root, ignore_errors=True)
        if file_snapshot is not None:
            shutil.rmtree(file_snapshot.root, ignore_errors=True)
    print(f">> packaged {release_destination}")


def verify_source_tree() -> None:
    manifest = _read_json_object(
        REPO_ROOT / "SOURCE-MANIFEST.json",
        "source manifest",
    )
    patch_paths = _validate_source_manifest_metadata(
        manifest,
        kernel_provenance_inputs(),
    )
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
        {REPO_ROOT / patch: f"kernel patch {patch}" for patch in patch_paths}
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
    config_path = REPO_ROOT / "kernel" / "config-microvm"
    config = config_path.read_text(encoding="utf-8")
    for setting in (
        "CONFIG_PVH=y",
        "CONFIG_HVC_XE9=y",
        "CONFIG_VIRTIO_FS=y",
        "CONFIG_FUSE_FS=y",
        *REQUIRED_SANDBOX_KERNEL_CONFIG,
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
    print(">> source tree and submodule metadata are consistent")
