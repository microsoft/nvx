#!/usr/bin/env python3
"""Remove only this job's rootless Docker client configuration."""

from __future__ import annotations

import os
import re
import shutil
from pathlib import Path


def main() -> None:
    runner_temp = os.environ.get("RUNNER_TEMP")
    config_dir = os.environ.get("NVX_DOCKER_CONFIG_TO_CLEAN")
    if not runner_temp or not config_dir:
        raise SystemExit("RUNNER_TEMP and NVX_DOCKER_CONFIG_TO_CLEAN are required")

    root = Path(runner_temp).resolve(strict=True)
    config = Path(config_dir)
    if root.name != "_temp" or config.is_symlink() or not config.is_dir():
        raise SystemExit(
            "refusing to remove an unexpected Docker configuration directory"
        )
    target = config.resolve(strict=True)
    if (
        target.parent != root
        or re.fullmatch(r"nvx-docker-config\.[A-Za-z0-9]{6}", target.name) is None
    ):
        raise SystemExit(
            "Docker configuration must be a per-job directory in RUNNER_TEMP"
        )
    if os.name == "posix" and not shutil.rmtree.avoids_symlink_attacks:
        raise SystemExit(
            "safe Docker configuration cleanup is unavailable on this host"
        )
    shutil.rmtree(target)


if __name__ == "__main__":
    main()
