#!/usr/bin/env python3
"""Publish the current NVX development release from CI artifacts."""

from __future__ import annotations

import os
import sys
from pathlib import Path

from nvx_tools.build_constants import (
    BuildConstants,
    ReleaseBuildConstants,
)
from nvx_tools.common import ScriptError, require_file
from nvx_tools.development_release import publish_development_release
from nvx_tools.release import require_release_package_executables

# The release platforms and the archive formats of their packages.
PLATFORMS = (
    ("linux-kvm", ".tar.gz"),
    ("linux-mshv", ".tar.gz"),
    ("windows-whp", ".zip"),
)


def _required_environment(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise ScriptError(f"{name} is required to publish a development release")
    return value


def release_packages(directory: Path, version: str) -> dict[str, Path]:
    """Return the package archive of each release platform in `directory`."""
    return {
        platform: directory / f"nvx-{version}-{platform}{extension}"
        for platform, extension in PLATFORMS
    }


def check_release_packages(packages: dict[str, Path]) -> None:
    """Fail unless every package ships the executables of its platform,
    OpenVMM and nvx-rs, which `nvx.py download` installs."""
    for platform, package in packages.items():
        require_file(package, f"development release package {package.name}")
        require_release_package_executables(package, platform)


def main() -> None:
    version_path = require_file(BuildConstants.REPO_ROOT / "VERSION", "NVX version")
    try:
        version = version_path.read_text(encoding="ascii").strip()
    except OSError as error:
        raise ScriptError(f"failed to read {version_path}: {error}") from error
    repository = _required_environment("GH_REPO")
    target_sha = _required_environment("GITHUB_SHA")
    packages = release_packages(Path(ReleaseBuildConstants.DIRECTORY_NAME), version)
    check_release_packages(packages)
    publish_development_release(
        repository,
        target_sha,
        version,
        tuple(packages.values()),
    )


if __name__ == "__main__":
    try:
        main()
    except ScriptError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
