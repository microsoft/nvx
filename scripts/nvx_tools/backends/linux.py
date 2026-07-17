"""Linux/KVM host backend."""

from __future__ import annotations

import os
from pathlib import Path
import re
import shutil

from .base import HostBackend
from ..common import require_tool, run_capture


def _is_root() -> bool:
    get_effective_uid = getattr(os, "geteuid", None)
    return get_effective_uid is not None and get_effective_uid() == 0


class LinuxBackend(HostBackend):
    def __init__(self) -> None:
        super().__init__(
            name="linux-kvm",
            executable_suffix="",
            artifact_dir=Path(
                os.environ.get("NVX_ARTIFACT_DIR", Path.home() / "build")
            ),
            clocksource="kvm-clock",
            supports_vcpus=True,
        )

    @property
    def build_hint(self) -> str:
        return "run 'python3 scripts/nvx.py build-kernel' and 'build-initramfs'"

    def prepare_virtfs_benchmark(self) -> None:
        require_tool("mke2fs", "missing mke2fs (install e2fsprogs)")

    def cleanup_network(self) -> None:
        ip = shutil.which("ip")
        if ip is None:
            return
        try:
            result = run_capture([ip, "-o", "link", "show"])
        except OSError:
            return
        interfaces = sorted(set(re.findall(r"\bllx[0-9a-z]+\b", result.text)))
        if _is_root():
            prefix = [ip]
        else:
            sudo = shutil.which("sudo")
            if sudo is None:
                return
            prefix = [sudo, "-n", ip]
        for interface in interfaces:
            try:
                run_capture(
                    [
                        *prefix,
                        "tuntap",
                        "del",
                        "dev",
                        interface,
                        "mode",
                        "tap",
                    ]
                )
            except OSError:
                continue

    @property
    def network_description(self) -> str:
        return "host TAP"