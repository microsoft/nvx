#!/usr/bin/env python3
"""Publish NVX's crates to crates.io from a full CI run of dev.

The package job runs `package`. It decides whether the checkout's commit
needs a new version of the crates that nvx_tools.crate_release.CRATES lists,
and if so, it gives the checkout that version, packs the crates, checks the
packages against the commit, and builds them with every feature. It outputs
each package's version and SHA-256, as JSON.

The publish job, which never builds, runs `prepare` with those outputs. It
checks that crates.io still needs the version and that its checkout packs the
same bytes. Then, after the crates.io authentication step, it runs `publish`,
which uploads each package after those that it depends on, and checks that
crates.io serves the same bytes. See doc/ci.md, "crates.io releases".
"""

from __future__ import annotations

import argparse
import sys
from collections.abc import Sequence
from pathlib import Path

from nvx_tools.build_constants import BuildConstants
from nvx_tools.common import ScriptError
from nvx_tools.crate_release import (
    package_release,
    prepare_release,
    publish_release,
    read_states,
)


def parse_arguments(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Publish NVX's crates to crates.io from a full CI run of dev."
    )
    parser.add_argument(
        "command",
        choices=("package", "prepare", "publish"),
        help="decide, pack, check, and build in the package job; or check "
        "the packages again, and publish them, in the publish job",
    )
    parser.add_argument(
        "--root",
        type=Path,
        default=BuildConstants.REPO_ROOT,
        help="the root of the Git checkout that holds the crates (default: this "
        "checkout)",
    )
    parser.add_argument(
        "--work-dir",
        type=Path,
        default=BuildConstants.BUILD_DIR / "nvx-crate",
        help="where a step leaves its packages and state (default: build/nvx-crate)",
    )
    parser.add_argument(
        "--packages",
        help="for prepare: the packages that the package job checked and built, "
        "as the JSON that it outputs",
    )
    arguments = parser.parse_args(argv)
    if arguments.command == "prepare" and not arguments.packages:
        parser.error("prepare requires --packages")
    return arguments


def main(argv: Sequence[str] | None = None) -> None:
    arguments = parse_arguments(argv)
    root = Path(arguments.root).resolve()
    work_dir = Path(arguments.work_dir).resolve()
    if arguments.command == "package":
        package_release(root, work_dir)
    elif arguments.command == "prepare":
        prepare_release(root, work_dir, read_states(arguments.packages, "--packages"))
    else:
        publish_release(root, work_dir)


if __name__ == "__main__":
    try:
        main()
    except ScriptError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
