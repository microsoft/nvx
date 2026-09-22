#!/usr/bin/env python3
"""Publish the current NVX development release from CI artifacts."""

from __future__ import annotations

import os
import sys
from pathlib import Path

from nvx_tools.common import REPO_ROOT, ScriptError, require_file
from nvx_tools.development_release import publish_development_release


def _required_environment(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise ScriptError(f"{name} is required to publish a development release")
    return value


def main() -> None:
    version_path = require_file(REPO_ROOT / "VERSION", "NVX version")
    try:
        version = version_path.read_text(encoding="ascii").strip()
    except OSError as error:
        raise ScriptError(f"failed to read {version_path}: {error}") from error
    packages = (
        Path(f"dist/nvx-{version}-linux-kvm.tar.gz"),
        Path(f"dist/nvx-{version}-linux-mshv.tar.gz"),
        Path(f"dist/nvx-{version}-windows-whp.zip"),
    )
    publish_development_release(
        _required_environment("GH_REPO"),
        _required_environment("GITHUB_SHA"),
        version,
        packages,
    )


if __name__ == "__main__":
    try:
        main()
    except ScriptError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
