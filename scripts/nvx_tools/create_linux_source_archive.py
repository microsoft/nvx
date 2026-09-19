"""Create the Linux corresponding-source archive from pinned inputs."""

from __future__ import annotations

import argparse
import json
from pathlib import Path

from nvx_tools.archive import (
    canonical_source_archive_tree_digest,
    canonical_source_tree_digest,
    create_reproducible_tar_gz,
)
from nvx_tools.build import DEFAULT_KERNEL_VERSION, prepare_kernel_source
from nvx_tools.common import REPO_ROOT, ScriptError


def configure_parser(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.set_defaults(handler=command_create_linux_source_archive)


def command_create_linux_source_archive(args: argparse.Namespace) -> None:
    source, source_fingerprint = prepare_kernel_source()
    accepted_source = json.loads(source_fingerprint)
    expected_tree_sha256 = accepted_source.get("tree_sha256")
    if (
        not isinstance(expected_tree_sha256, str)
        or canonical_source_tree_digest(source) != expected_tree_sha256
    ):
        raise ScriptError("accepted Linux source tree digest is invalid")
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
        (REPO_ROOT / "LICENSE", f"{root}/LICENSE"),
    )
    try:
        create_reproducible_tar_gz(output, inputs, normalize_file_modes=True)
        archived_tree_sha256 = canonical_source_archive_tree_digest(
            output,
            package_root=root,
            tree_root=f"{root}/linux-{DEFAULT_KERNEL_VERSION}",
        )
        if archived_tree_sha256 != expected_tree_sha256:
            raise ScriptError(
                "Linux corresponding-source archive does not match the accepted "
                "source tree"
            )
    except BaseException:
        output.unlink(missing_ok=True)
        raise
    print(f">> created {output}")
