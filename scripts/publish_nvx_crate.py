#!/usr/bin/env python3
"""Publish the nvx crate to crates.io from a full CI run of dev.

The package job runs `package`. It decides whether the checkout's commit
needs a new version of the crate, and if so, it gives the checkout that
version, packs the crate, checks the package against the commit, builds it
with every feature, and installs nvx-ingress from it with the guest feature
and profile, outside the checkout, and runs it. It outputs the version and the
package's SHA-256.

The publish job, which never builds, runs `prepare` with those outputs. It
checks that crates.io still needs the version and that its checkout packs the
same bytes. Then, after the crates.io authentication step, it runs `publish`,
which uploads the package and checks that crates.io serves the same bytes. See
doc/ci.md, "crates.io releases".
"""

from __future__ import annotations

import argparse
import sys
from collections.abc import Sequence
from pathlib import Path

from nvx_tools.build_constants import BuildConstants
from nvx_tools.common import ScriptError
from nvx_tools.crate_release import (
    Version,
    package_release,
    prepare_release,
    publish_release,
)


def parse_arguments(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Publish the nvx crate to crates.io from a full CI run of dev."
    )
    parser.add_argument(
        "command",
        choices=("package", "prepare", "publish"),
        help="decide, pack, check, build, and install in the package job; or "
        "check the package again, and publish it, in the publish job",
    )
    parser.add_argument(
        "--crate-dir",
        type=Path,
        default=BuildConstants.REPO_ROOT / "nvx-rs",
        help="the crate's directory in a Git checkout (default: nvx-rs)",
    )
    parser.add_argument(
        "--work-dir",
        type=Path,
        default=BuildConstants.BUILD_DIR / "nvx-crate",
        help="where a step leaves its packages and state (default: build/nvx-crate)",
    )
    parser.add_argument(
        "--crate-version",
        help="for prepare: the version that the package job packed",
    )
    parser.add_argument(
        "--sha256",
        help="for prepare: the SHA-256 of the package that the package job built",
    )
    arguments = parser.parse_args(argv)
    if arguments.command == "prepare" and not (
        arguments.crate_version and arguments.sha256
    ):
        parser.error("prepare requires --crate-version and --sha256")
    return arguments


def main(argv: Sequence[str] | None = None) -> None:
    arguments = parse_arguments(argv)
    crate_dir = Path(arguments.crate_dir).resolve()
    work_dir = Path(arguments.work_dir).resolve()
    if arguments.command == "package":
        package_release(crate_dir, work_dir)
    elif arguments.command == "prepare":
        prepare_release(
            crate_dir,
            work_dir,
            Version.parse(arguments.crate_version),
            arguments.sha256,
        )
    else:
        publish_release(crate_dir, work_dir)


if __name__ == "__main__":
    try:
        main()
    except ScriptError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
