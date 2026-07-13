#!/usr/bin/env python3
# Copyright(c) The microvm authors.
# Licensed under the MIT License.

"""
Merge folded stacks and render flamegraph SVGs.

Takes guest.folded + optional host.folded, applies [GUEST]/[HOST] roots, and
renders:
  - guest.svg
  - host.svg (if host stacks exist)
  - flamegraph.svg (combined)
"""

from __future__ import annotations

import subprocess

from pathlib import Path


def _read_folded(path: Path) -> list[str]:
    if not path.exists() or path.stat().st_size == 0:
        return []
    return [line.strip() for line in path.read_text(encoding="utf-8", errors="replace").splitlines() if line.strip()]


def _prefix(lines: list[str], root: str) -> list[str]:
    out: list[str] = []
    marker = f"{root};"
    for line in lines:
        stack, sep, count = line.rpartition(" ")
        if sep and stack and count:
            # Idempotent: guest folded output produced by older `microvm` builds may already carry
            # the root frame. Never prefix twice.
            if stack == root or stack.startswith(marker):
                out.append(f"{stack} {count}")
            else:
                out.append(f"{root};{stack} {count}")
    return out


def _render(lines: list[str], out_svg: Path, title: str) -> bool:
    """Renders an SVG flamegraph. Returns whether a file was actually written."""
    if not lines:
        return False
    result = subprocess.run(
        ["inferno-flamegraph", "--title", title],
        input="\n".join(lines) + "\n",
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"inferno-flamegraph failed for {out_svg.name}: {result.stderr.strip()}")
    out_svg.write_text(result.stdout, encoding="utf-8")
    return True


def merge_and_render(
    guest_folded: Path,
    host_folded: Path | None,
    out_dir: Path,
    title_prefix: str = "NVX",
) -> dict[str, Path]:
    out_dir.mkdir(parents=True, exist_ok=True)

    guest_lines = _read_folded(guest_folded)
    guest_prefixed = _prefix(guest_lines, "[GUEST]")

    host_lines: list[str] = []
    host_prefixed: list[str] = []
    if host_folded is not None:
        host_lines = _read_folded(host_folded)
        host_prefixed = _prefix(host_lines, "[HOST]")

    # Remove stale artifacts from a previous run in a reused output directory, so a render that is
    # now skipped (e.g. host stacks disappeared this run) never leaves a misleading old graph or
    # folded file behind. Inputs were already read into memory above, so removing them here is safe.
    stale_paths = [
        out_dir / name
        for name in ("guest.svg", "host.svg", "flamegraph.svg", "merged.folded")
    ]
    # A host.folded left by a previous full run is misleading on a guest-only or failed-host run;
    # drop it unless it is this run's actual host input.
    host_folded_out = out_dir / "host.folded"
    if host_folded is None or host_folded.resolve() != host_folded_out.resolve():
        stale_paths.append(host_folded_out)
    for stale_path in stale_paths:
        if stale_path.exists():
            stale_path.unlink()

    merged = guest_prefixed + host_prefixed
    outputs: dict[str, Path] = {}

    # Guest and host samples are kept under separate `[GUEST]` and `[HOST]` roots and concatenated.
    # Within a single root the widths are directly comparable: that source samples at one fixed rate
    # (the guest sampler at `--profile-hz`; `microvm` aligns the host recorder to it via
    # `xperf -SetProfInt` on Windows / `perf -F` on Linux), so each sample represents the same time
    # slice. Across the two roots the totals are NOT directly comparable and must be read
    # independently: the guest sampler snapshots only the boot vCPU each interval, whereas the host
    # recorder samples every scheduled VMM thread, so the `[HOST]` root aggregates several threads'
    # time per interval while `[GUEST]` aggregates one. The separate roots keep each side
    # self-consistent; do not infer a guest-vs-host time ratio from their relative widths.

    # Only report artifacts that were actually written, so callers never see a path for a file that
    # does not exist (empty input renders nothing).
    if merged:
        merged_folded = out_dir / "merged.folded"
        merged_folded.write_text("\n".join(merged) + "\n", encoding="utf-8")
        outputs["merged_folded"] = merged_folded

    guest_svg = out_dir / "guest.svg"
    if _render(guest_prefixed, guest_svg, f"{title_prefix} Guest Flamegraph"):
        outputs["guest_svg"] = guest_svg

    host_svg = out_dir / "host.svg"
    if _render(host_prefixed, host_svg, f"{title_prefix} Host Flamegraph"):
        outputs["host_svg"] = host_svg

    combined_svg = out_dir / "flamegraph.svg"
    if _render(merged, combined_svg, f"{title_prefix} Combined Flamegraph"):
        outputs["combined_svg"] = combined_svg

    return outputs
