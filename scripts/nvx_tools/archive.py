"""Helpers for creating reproducible source archives."""

from __future__ import annotations

import gzip
import tarfile
from collections.abc import Sequence
from pathlib import Path, PurePosixPath

from .common import ScriptError

ArchiveInput = tuple[Path, str]


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
