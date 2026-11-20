"""Create the Linux corresponding-source archive from pinned inputs."""

from __future__ import annotations

import argparse
from pathlib import Path

from nvx_tools.archive import create_reproducible_tar_gz
from nvx_tools.build import prepare_kernel_source
from nvx_tools.build_config import KernelBuildConfig
from nvx_tools.build_constants import (
    BuildConstants,
    KernelBuildConstants,
)
from nvx_tools.common import ScriptError
from nvx_tools.kernel_config import parse_config


def configure_parser(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--architecture", choices=("x86_64", "aarch64"), default="x86_64"
    )
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.set_defaults(handler=command_create_linux_source_archive)


def command_create_linux_source_archive(args: argparse.Namespace) -> None:
    architecture = getattr(args, "architecture", "x86_64")
    config = KernelBuildConfig.for_architecture(architecture)
    generated_config = args.config.resolve()
    if not generated_config.is_file():
        raise FileNotFoundError(
            f"generated kernel config not found: {generated_config}"
        )
    if architecture == "aarch64":
        values = parse_config(
            generated_config.read_text(encoding="utf-8"), str(generated_config)
        )
        if values.get("ARM64") != "y" or values.get("X86_64") == "y":
            raise ScriptError(
                "ARM corresponding source requires its generated ARM64 configuration"
            )
    source, _ = prepare_kernel_source(config)
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    root = KernelBuildConstants.SOURCE_ARCHIVE_ROOT + (
        "-aarch64" if architecture == "aarch64" else ""
    )
    inputs = (
        (source, f"{root}/{KernelBuildConstants.SOURCE_NAME}"),
        (generated_config, f"{root}/{config.output.name}.config"),
        (BuildConstants.REPO_ROOT / "kernel", f"{root}/kernel"),
        (BuildConstants.REPO_ROOT / "scripts", f"{root}/scripts"),
        (BuildConstants.REPO_ROOT / "docker", f"{root}/docker"),
        (BuildConstants.REPO_ROOT / "README.md", f"{root}/README.md"),
        (
            BuildConstants.REPO_ROOT / "SOURCE-MANIFEST.json",
            f"{root}/SOURCE-MANIFEST.json",
        ),
        (
            BuildConstants.REPO_ROOT / "THIRD_PARTY_NOTICES.md",
            f"{root}/THIRD_PARTY_NOTICES.md",
        ),
        (BuildConstants.REPO_ROOT / "LICENSE", f"{root}/LICENSE"),
    )
    create_reproducible_tar_gz(output, inputs)
    print(f">> created {output}")
