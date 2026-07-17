"""Host-specific paths and capabilities used by shared workflows."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from ..common import REPO_ROOT, run_capture


@dataclass(frozen=True)
class HostBackend:
    name: str
    executable_suffix: str
    artifact_dir: Path
    clocksource: str
    supports_vcpus: bool
    repo_root: Path = REPO_ROOT

    def executable(self, profile: str = "release") -> Path:
        return self.repo_root / "target" / profile / f"microvm{self.executable_suffix}"

    def artifact(self, filename: str) -> Path:
        return self.artifact_dir / filename

    def add_vcpus(self, args: list[str | Path], count: int) -> None:
        if count < 1:
            raise ValueError("vCPU count must be at least 1")
        if count != 1 and not self.supports_vcpus:
            raise ValueError(f"{self.name} supports exactly one vCPU")
        if self.supports_vcpus:
            args.extend(["--vcpus", str(count)])

    def allocated_size(self, path: Path) -> int:
        stat = path.stat()
        blocks = getattr(stat, "st_blocks", None)
        if blocks is not None:
            return blocks * 512
        result = run_capture(["fsutil", "sparse", "queryrange", path])
        allocated = 0
        for line in result.text.splitlines():
            marker = "length:"
            if marker in line.lower():
                value = line.lower().split(marker, 1)[1].strip().split()[0]
                try:
                    allocated += int(value, 16)
                except ValueError:
                    continue
        return allocated or stat.st_size

    def prepare_virtfs_benchmark(self) -> None:
        return None

    def cleanup_network(self) -> None:
        return None

    @property
    def network_description(self) -> str:
        return "user-mode NAT"

    @property
    def build_hint(self) -> str:
        return "build the required guest artifacts first"