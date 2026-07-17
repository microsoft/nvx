"""Windows/WHP host backend."""

from __future__ import annotations

from .base import HostBackend
from ..common import REPO_ROOT


class WindowsBackend(HostBackend):
    def __init__(self) -> None:
        super().__init__(
            name="windows-whp",
            executable_suffix=".exe",
            artifact_dir=REPO_ROOT / "build",
            clocksource="tsc",
            supports_vcpus=False,
        )

    @property
    def build_hint(self) -> str:
        return "run 'python scripts/nvx.py build-linux-artifacts'"