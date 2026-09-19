"""Helpers for creating reproducible source archives."""

from __future__ import annotations

import gzip
import hashlib
import os
import shutil
import stat
import tarfile
import zipfile
from collections.abc import Sequence
from pathlib import Path, PurePosixPath
from typing import Protocol

from .common import ScriptError

ArchiveInput = tuple[Path, str]
_ZIP_TIMESTAMP = (1980, 1, 1, 0, 0, 0)
_CANONICAL_SYMLINK_MODE = 0o777


class _Digest(Protocol):
    def update(self, value: bytes, /) -> None: ...


def _canonical_source_mode(mode: int, *, is_directory: bool) -> int:
    if is_directory:
        return 0o755
    return 0o755 if mode & 0o111 else 0o644


def _normalize_member(member: tarfile.TarInfo) -> tarfile.TarInfo | None:
    path = PurePosixPath(member.name)
    if "__pycache__" in path.parts or path.suffix in (".pyc", ".pyo"):
        return None
    member.uid = 0
    member.gid = 0
    member.uname = ""
    member.gname = ""
    member.mtime = 0
    if member.isdir():
        member.mode = _canonical_source_mode(member.mode, is_directory=True)
    return member


def _normalize_source_member(member: tarfile.TarInfo) -> tarfile.TarInfo | None:
    normalized = _normalize_member(member)
    if normalized is not None and normalized.isfile():
        normalized.mode = _canonical_source_mode(
            normalized.mode,
            is_directory=False,
        )
    elif normalized is not None and normalized.issym():
        normalized.mode = _CANONICAL_SYMLINK_MODE
    return normalized


def _add_digest_field(digest: _Digest, value: bytes) -> None:
    digest.update(len(value).to_bytes(8, "big"))
    digest.update(value)


def _source_tree_digest(
    entries: Sequence[tuple[str, str, int, bytes | None]],
) -> str:
    digest = hashlib.sha256()
    for relative, kind, mode, content_digest in sorted(entries):
        _add_digest_field(digest, relative.encode("utf-8"))
        _add_digest_field(digest, f"{mode:o}".encode("ascii"))
        _add_digest_field(digest, kind.encode("ascii"))
        if content_digest is not None:
            _add_digest_field(digest, content_digest)
    return digest.hexdigest()


def _same_file_identity(first: os.stat_result, second: os.stat_result) -> bool:
    return first.st_dev == second.st_dev and first.st_ino == second.st_ino


def _canonical_symlink_target(relative: str, target: str) -> bytes:
    if (
        not relative
        or not target
        or "\0" in target
        or "\\" in target
        or PurePosixPath(target).is_absolute()
        or (
            len(target) >= 2
            and target[0].isascii()
            and target[0].isalpha()
            and target[1] == ":"
        )
    ):
        raise ScriptError(f"unsafe symlink target in Linux source tree: {relative}")

    resolved = list(PurePosixPath(relative).parent.parts)
    for part in PurePosixPath(target).parts:
        if part == ".":
            continue
        if part == "..":
            if not resolved:
                raise ScriptError(
                    f"symlink target escapes Linux source tree: {relative}"
                )
            resolved.pop()
        else:
            resolved.append(part)
    try:
        return target.encode("utf-8")
    except UnicodeEncodeError as error:
        raise ScriptError(
            f"unsafe symlink target in Linux source tree: {relative}"
        ) from error


def canonical_source_tree_digest(source: Path) -> str:
    """Digest a complete source tree using deterministic archive modes."""

    entries: list[tuple[str, str, int, bytes | None]] = []

    def visit(path: Path, relative: str) -> None:
        before = path.lstat()
        mode = before.st_mode
        if stat.S_ISDIR(mode):
            entries.append(
                (
                    relative,
                    "directory",
                    _canonical_source_mode(mode, is_directory=True),
                    None,
                )
            )
            with os.scandir(path) as children:
                child_names = sorted(entry.name for entry in children)
            for child_name in child_names:
                if "\\" in child_name or ":" in child_name or child_name in (".", ".."):
                    raise ScriptError(f"unsafe path in Linux source tree: {child_name}")
                child_relative = f"{relative}/{child_name}" if relative else child_name
                visit(path / child_name, child_relative)
            after = path.lstat()
            if (
                not stat.S_ISDIR(after.st_mode)
                or not _same_file_identity(before, after)
                or before.st_mtime_ns != after.st_mtime_ns
                or before.st_ctime_ns != after.st_ctime_ns
            ):
                raise ScriptError(
                    f"Linux source tree changed while hashing: {relative or source}"
                )
            return
        if stat.S_ISLNK(mode):
            target = os.readlink(path)
            after = path.lstat()
            if (
                not stat.S_ISLNK(after.st_mode)
                or not _same_file_identity(before, after)
                or before.st_mtime_ns != after.st_mtime_ns
                or before.st_ctime_ns != after.st_ctime_ns
            ):
                raise ScriptError(
                    f"Linux source tree changed while hashing: {relative}"
                )
            entries.append(
                (
                    relative,
                    "symlink",
                    _CANONICAL_SYMLINK_MODE,
                    _canonical_symlink_target(relative, target),
                )
            )
            return
        if not stat.S_ISREG(mode):
            raise ScriptError(f"unsupported file in Linux source tree: {relative}")

        flags = os.O_RDONLY | getattr(os, "O_BINARY", 0)
        flags |= getattr(os, "O_NOFOLLOW", 0)
        descriptor = os.open(path, flags)
        try:
            opened = os.fstat(descriptor)
            after_open = path.lstat()
            if (
                not stat.S_ISREG(opened.st_mode)
                or not stat.S_ISREG(after_open.st_mode)
                or not _same_file_identity(before, opened)
                or not _same_file_identity(opened, after_open)
            ):
                raise ScriptError(
                    f"Linux source tree changed while opening: {relative}"
                )
            file_digest = hashlib.sha256()
            with os.fdopen(descriptor, "rb", closefd=False) as source_file:
                while chunk := source_file.read(1024 * 1024):
                    file_digest.update(chunk)
            after_read = os.fstat(descriptor)
            after = path.lstat()
            if (
                not _same_file_identity(opened, after_read)
                or not _same_file_identity(after_read, after)
                or opened.st_size != after_read.st_size
                or opened.st_mtime_ns != after_read.st_mtime_ns
                or opened.st_ctime_ns != after_read.st_ctime_ns
            ):
                raise ScriptError(
                    f"Linux source tree changed while hashing: {relative}"
                )
            entries.append(
                (
                    relative,
                    "file",
                    _canonical_source_mode(mode, is_directory=False),
                    file_digest.digest(),
                )
            )
        finally:
            os.close(descriptor)

    visit(source, "")
    return _source_tree_digest(entries)


def _canonical_tar_path(name: str, *, is_directory: bool) -> str:
    normalized = name[:-1] if is_directory and name.endswith("/") else name
    path = PurePosixPath(normalized)
    if (
        not normalized
        or "\\" in normalized
        or path.is_absolute()
        or not path.parts
        or "." in path.parts
        or ".." in path.parts
        or any(":" in part for part in path.parts)
        or path.as_posix() != normalized
        or (not is_directory and name.endswith("/"))
    ):
        raise ScriptError(f"unsafe path in Linux source archive: {name}")
    return normalized


def canonical_source_archive_tree_digest(
    archive_path: Path,
    *,
    package_root: str,
    tree_root: str,
) -> str:
    """Digest one complete source subtree after strict tar layout validation."""

    entries: list[tuple[str, str, int, bytes | None]] = []
    layout: dict[str, bool] = {}
    validated_members: list[tuple[tarfile.TarInfo, str, int]] = []
    tree_prefix = f"{tree_root}/"
    package_prefix = f"{package_root}/"
    try:
        with tarfile.open(archive_path, "r:gz") as archive:
            for member in archive.getmembers():
                if not (member.isdir() or member.isfile() or member.issym()):
                    raise ScriptError(
                        f"unsupported entry in Linux source archive: {member.name}"
                    )
                canonical = _canonical_tar_path(
                    member.name,
                    is_directory=member.isdir(),
                )
                if canonical != package_root and not canonical.startswith(
                    package_prefix
                ):
                    raise ScriptError(
                        "Linux source archive contains a path outside "
                        f"{package_root}: {member.name}"
                    )
                if canonical in layout:
                    raise ScriptError(
                        f"duplicate path in Linux source archive: {canonical}"
                    )
                layout[canonical] = member.isdir()
                canonical_mode = (
                    _CANONICAL_SYMLINK_MODE
                    if member.issym()
                    else _canonical_source_mode(
                        member.mode,
                        is_directory=member.isdir(),
                    )
                )
                if member.mode != canonical_mode:
                    raise ScriptError(
                        f"non-canonical mode in Linux source archive: {member.name}"
                    )
                if member.issym() and member.size != 0:
                    raise ScriptError(
                        "non-canonical symlink size in Linux source archive: "
                        f"{member.name}"
                    )
                package_relative = (
                    ""
                    if canonical == package_root
                    else canonical[len(package_prefix) :]
                )
                if member.issym():
                    _canonical_symlink_target(package_relative, member.linkname)
                validated_members.append((member, canonical, canonical_mode))

            if package_root in layout and layout[package_root] is not True:
                raise ScriptError(
                    "Linux source archive package root is not a directory"
                )
            for name in layout:
                if name == package_root:
                    continue
                parent = PurePosixPath(name).parent
                while parent != PurePosixPath("."):
                    parent_name = parent.as_posix()
                    if parent_name == package_root:
                        if parent_name in layout and layout[parent_name] is not True:
                            raise ScriptError(
                                f"colliding path in Linux source archive: {name}"
                            )
                        break
                    if parent_name not in layout:
                        raise ScriptError(
                            f"Linux source archive omits directory entry {parent_name}"
                        )
                    if layout[parent_name] is not True:
                        raise ScriptError(
                            f"colliding path in Linux source archive: {name}"
                        )
                    parent = parent.parent

            for member, canonical, canonical_mode in validated_members:
                if canonical != tree_root and not canonical.startswith(tree_prefix):
                    continue
                relative = (
                    "" if canonical == tree_root else canonical[len(tree_prefix) :]
                )
                if member.isdir():
                    entries.append((relative, "directory", canonical_mode, None))
                    continue
                if member.issym():
                    entries.append(
                        (
                            relative,
                            "symlink",
                            canonical_mode,
                            _canonical_symlink_target(relative, member.linkname),
                        )
                    )
                    continue
                extracted = archive.extractfile(member)
                if extracted is None:
                    raise ScriptError(
                        f"could not read Linux source archive entry: {member.name}"
                    )
                file_digest = hashlib.sha256()
                with extracted:
                    while chunk := extracted.read(1024 * 1024):
                        file_digest.update(chunk)
                entries.append((relative, "file", canonical_mode, file_digest.digest()))
    except tarfile.TarError as error:
        raise ScriptError(
            f"invalid Linux source archive {archive_path.name}: {error}"
        ) from error

    if layout.get(tree_root) is not True:
        raise ScriptError("Linux source archive omits its Linux tree root directory")
    return _source_tree_digest(entries)


def create_reproducible_tar_gz(
    output: Path,
    inputs: Sequence[ArchiveInput],
    *,
    normalize_file_modes: bool = False,
) -> None:
    missing = [str(path) for path, _ in inputs if not path.exists()]
    if missing:
        raise ScriptError(
            "cannot create source archive; missing: " + ", ".join(missing)
        )
    output.parent.mkdir(parents=True, exist_ok=True)
    layout: dict[str, bool] = {}
    with output.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as archive:
                archive.dereference = False
                for source, arcname in inputs:
                    source_metadata = source.lstat()
                    source_is_directory = stat.S_ISDIR(source_metadata.st_mode)
                    if not (
                        source_is_directory or stat.S_ISREG(source_metadata.st_mode)
                    ):
                        raise ScriptError(f"unsupported source archive input: {source}")
                    input_root = _canonical_tar_path(
                        arcname,
                        is_directory=source_is_directory,
                    )

                    def normalize(
                        member: tarfile.TarInfo,
                        input_root: str = input_root,
                    ) -> tarfile.TarInfo | None:
                        normalized = (
                            _normalize_source_member(member)
                            if normalize_file_modes
                            else _normalize_member(member)
                        )
                        if normalized is None:
                            return None
                        if not (
                            normalized.isdir()
                            or normalized.isfile()
                            or normalized.issym()
                        ):
                            raise ScriptError(
                                "unsupported entry in source archive: "
                                f"{normalized.name}"
                            )
                        canonical = _canonical_tar_path(
                            normalized.name,
                            is_directory=normalized.isdir(),
                        )
                        if canonical in layout:
                            raise ScriptError(
                                f"duplicate path in source archive: {canonical}"
                            )
                        parent = PurePosixPath(canonical).parent
                        while parent != PurePosixPath("."):
                            parent_name = parent.as_posix()
                            if layout.get(parent_name) is False:
                                raise ScriptError(
                                    f"colliding path in source archive: {canonical}"
                                )
                            parent = parent.parent
                        if not normalized.isdir() and any(
                            name.startswith(f"{canonical}/") for name in layout
                        ):
                            raise ScriptError(
                                f"colliding path in source archive: {canonical}"
                            )
                        if normalized.issym():
                            prefix = f"{input_root}/"
                            if not canonical.startswith(prefix):
                                raise ScriptError(
                                    f"unsafe symlink path in source archive: {canonical}"
                                )
                            relative = canonical[len(prefix) :]
                            _canonical_symlink_target(relative, normalized.linkname)
                            normalized.mode = _CANONICAL_SYMLINK_MODE
                            normalized.size = 0
                        layout[canonical] = normalized.isdir()
                        return normalized

                    archive.add(
                        source,
                        arcname=arcname,
                        recursive=True,
                        filter=normalize,
                    )


def _release_members(source: Path) -> list[tuple[Path, str, int, bool]]:
    if not source.is_dir() or source.is_symlink():
        raise ScriptError(f"release package directory not found: {source}")
    root_name = source.name
    root_path = PurePosixPath(root_name)
    if (
        not root_name
        or "\\" in root_name
        or root_path.is_absolute()
        or root_path.as_posix() != root_name
        or root_name in (".", "..")
    ):
        raise ScriptError(f"unsafe release package name: {root_name}")

    members: list[tuple[Path, str, int, bool]] = [(source, root_name, 0o755, True)]
    for path in sorted(
        source.rglob("*"),
        key=lambda item: item.relative_to(source).as_posix(),
    ):
        metadata = path.lstat()
        relative_path = path.relative_to(source)
        relative = relative_path.as_posix()
        archive_name = f"{root_name}/{relative}"
        if stat.S_ISLNK(metadata.st_mode):
            raise ScriptError(f"symlink is not allowed in release archive: {relative}")
        if stat.S_ISDIR(metadata.st_mode):
            members.append((path, archive_name, 0o755, True))
            continue
        if not stat.S_ISREG(metadata.st_mode):
            raise ScriptError(
                f"special file is not allowed in release archive: {relative}"
            )
        mode = (
            0o755
            if relative_path.parts[0] == "bin" or metadata.st_mode & 0o111
            else 0o644
        )
        members.append((path, archive_name, mode, False))
    return members


def create_reproducible_release_archive(source: Path, output: Path) -> None:
    members = _release_members(source)
    output.parent.mkdir(parents=True, exist_ok=True)
    if output.name.endswith(".tar.gz"):
        with output.open("wb") as raw:
            with gzip.GzipFile(
                filename="",
                mode="wb",
                fileobj=raw,
                mtime=0,
            ) as gzip_file:
                with tarfile.open(fileobj=gzip_file, mode="w") as archive:
                    for path, name, mode, is_directory in members:
                        info = tarfile.TarInfo(f"{name}/" if is_directory else name)
                        info.mode = mode
                        info.uid = 0
                        info.gid = 0
                        info.uname = ""
                        info.gname = ""
                        info.mtime = 0
                        if is_directory:
                            info.type = tarfile.DIRTYPE
                            archive.addfile(info)
                        else:
                            info.size = path.stat().st_size
                            with path.open("rb") as source_file:
                                archive.addfile(info, source_file)
        return
    if output.suffix == ".zip":
        with zipfile.ZipFile(
            output,
            "w",
            compression=zipfile.ZIP_DEFLATED,
            compresslevel=9,
        ) as archive:
            for path, name, mode, is_directory in members:
                member_name = f"{name}/" if is_directory else name
                info = zipfile.ZipInfo(member_name, date_time=_ZIP_TIMESTAMP)
                info.create_system = 3
                info.compress_type = zipfile.ZIP_DEFLATED
                file_type = stat.S_IFDIR if is_directory else stat.S_IFREG
                info.external_attr = (file_type | mode) << 16
                if is_directory:
                    info.external_attr |= 0x10
                    archive.writestr(info, b"")
                else:
                    with path.open("rb") as source_file:
                        with archive.open(info, "w") as destination_file:
                            shutil.copyfileobj(
                                source_file,
                                destination_file,
                                length=1024 * 1024,
                            )
        return
    raise ScriptError(f"release archive must end in .tar.gz or .zip: {output}")
