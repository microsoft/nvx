"""Host backend selection."""

from __future__ import annotations

import os
import sys

from .base import HostBackend
from .linux import LinuxBackend
from .windows import WindowsBackend


def select_backend(name: str | None = None) -> HostBackend:
    selected = (name or os.environ.get("NVX_BACKEND") or "auto").lower()
    if selected == "auto":
        selected = "windows" if sys.platform == "win32" else "linux"
    if selected in {"linux", "kvm"}:
        return LinuxBackend()
    if selected in {"windows", "whp"}:
        return WindowsBackend()
    raise ValueError(f"unsupported backend: {selected!r}")


__all__ = ["HostBackend", "LinuxBackend", "WindowsBackend", "select_backend"]