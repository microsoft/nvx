"""Helpers for creating reproducible source archives."""

from __future__ import annotations

import gzip
import shutil
import stat
import tarfile
import zipfile
from collections.abc import Sequence
from pathlib import Path, PurePosixPath

from .common import ScriptError

ArchiveInput = tuple[Path, str]
_ZIP_TIMESTAMP = (1980, 1, 1, 0, 0, 0)


def _normalize_member(member: tarfile.TarInfo) -> tarfile.TarInfo | None:
    path = PurePosixPath(member.name)
    if "__pycache__" in path.parts or path.suffix in (".pyc", ".pyo"):
        return None
    member.uid = 0
    member.gid = 0
    member.uname = ""
    member.gname = ""
    member.mtime = 0
    return member


def create_reproducible_tar_gz(
    output: Path,
    inputs: Sequence[ArchiveInput],
) -> None:
    missing = [str(path) for path, _ in inputs if not path.exists()]
    if missing:
        raise ScriptError(
            "cannot create source archive; missing: " + ", ".join(missing)
        )
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as archive:
                for source, arcname in inputs:
                    archive.add(
                        source,
                        arcname=arcname,
                        recursive=True,
                        filter=_normalize_member,
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
