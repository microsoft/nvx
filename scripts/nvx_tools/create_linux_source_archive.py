"""Create the Linux corresponding-source archive from pinned inputs."""

from __future__ import annotations

import argparse
import gzip
from pathlib import Path, PurePosixPath
import tarfile

from nvx_tools.build import DEFAULT_KERNEL_VERSION, prepare_kernel_source


REPO_ROOT = Path(__file__).resolve().parents[2]


def _filter(member: tarfile.TarInfo) -> tarfile.TarInfo | None:
    path = PurePosixPath(member.name)
    if "__pycache__" in path.parts or path.suffix in (".pyc", ".pyo"):
        return None
    member.uid = 0
    member.gid = 0
    member.uname = ""
    member.gname = ""
    member.mtime = 0
    return member


def configure_parser(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.set_defaults(handler=command_create_linux_source_archive)


def command_create_linux_source_archive(args: argparse.Namespace) -> None:
    source, _ = prepare_kernel_source()
    config = args.config.resolve()
    if not config.is_file():
        raise FileNotFoundError(f"generated kernel config not found: {config}")
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    root = f"nvx-linux-source-{DEFAULT_KERNEL_VERSION}"
    inputs = (
        (source, f"{root}/linux-{DEFAULT_KERNEL_VERSION}"),
        (config, f"{root}/vmlinux.config"),
        (REPO_ROOT / "kernel", f"{root}/kernel"),
        (REPO_ROOT / "scripts", f"{root}/scripts"),
        (REPO_ROOT / "docker", f"{root}/docker"),
        (REPO_ROOT / "README.md", f"{root}/README.md"),
        (REPO_ROOT / "SOURCE-MANIFEST.json", f"{root}/SOURCE-MANIFEST.json"),
        (REPO_ROOT / "THIRD_PARTY_NOTICES.md", f"{root}/THIRD_PARTY_NOTICES.md"),
        (REPO_ROOT / "LICENSE-NVX", f"{root}/LICENSE-NVX"),
    )
    with output.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as archive:
                for path, arcname in inputs:
                    archive.add(path, arcname=arcname, filter=_filter)
    print(f">> created {output}")
