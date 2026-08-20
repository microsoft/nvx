#!/usr/bin/env python3
# Copyright(c) The microvm authors.
# Licensed under the MIT License.

"""
NVX flamegraph post-processing (Nanvix-style).

This script consumes an existing guest folded profile produced by `microvm` and
optionally extracts host stacks from the sibling host trace, then renders:
  - guest.svg
  - host.svg (full mode, if host stacks are available)
  - flamegraph.svg (combined)

Usage:
    python scripts/flamegraph.py guest --guest-folded build/profiles/admin-cold.folded
    python scripts/flamegraph.py full  --guest-folded build/profiles/admin-cold.folded
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys

from pathlib import Path

from flamegraph_host import _find_xperf, _read_run_id, extract_host_stacks
from flamegraph_merge import merge_and_render


def _find(name: str) -> bool:
    return shutil.which(name) is not None


def _check_prereqs(mode: str) -> None:
    missing: list[str] = []
    for tool in ["rustfilt", "inferno-flamegraph"]:
        if not _find(tool):
            missing.append(tool)
    if mode == "full":
        if sys.platform == "win32" and _find_xperf() is None:
            missing.append("xperf")
        if sys.platform.startswith("linux") and not _find("perf"):
            missing.append("perf")
    if missing:
        raise SystemExit(
            "missing required tools: "
            + ", ".join(missing)
            + "\nInstall inferno/rustfilt with: cargo install inferno rustfilt"
        )


def _apply_demangle_to_line(line: str, mapping: dict[str, str]) -> str:
    """Rewrites one folded line by replacing each frame with its demangled form.

    Folded lines are `frame1;frame2;... <count>`. Each frame is looked up in `mapping` (which has
    already sanitized any `;` in demangled names to `,`); unmapped frames and non-folded lines are
    left untouched. This never changes the number of `;`-separated frames.
    """
    if not line.strip():
        return line
    stack, sep, count = line.rpartition(" ")
    if not sep or not count.isdigit():
        return line
    frames = stack.split(";")
    rebuilt = ";".join(mapping.get(f, f) if f else f for f in frames)
    return f"{rebuilt} {count}"


def _demangle_frames(frames: set[str]) -> dict[str, str]:
    """Batch-demangles unique frame names through rustfilt, sanitizing folded delimiters.

    Frames are fed one per line (a mangled Rust name contains no `;` or newline), so rustfilt's
    output maps 1:1 back to the inputs. Any `;` a demangled name contains — e.g. an array type such
    as `<[u8; 32] as Trait>::f` — is replaced with `,` so it is never mistaken for a folded frame
    separator. Returns an empty map (leaving frames unchanged) if rustfilt is unavailable or its
    output does not line up with the input.
    """
    if not frames:
        return {}
    ordered = list(frames)
    try:
        result = subprocess.run(
            ["rustfilt"], input="\n".join(ordered), capture_output=True, text=True
        )
    except OSError:
        return {}
    if result.returncode != 0:
        return {}
    out_lines = result.stdout.splitlines()
    if len(out_lines) != len(ordered):
        return {}
    return {src: dst.replace(";", ",") for src, dst in zip(ordered, out_lines)}


def _demangle_folded(path: Path) -> None:
    """Demangles Rust symbols in a folded stack file, one frame at a time.

    Demangling the whole file at once is unsafe: a Rust symbol can demangle to a string containing
    `;`, which Inferno would then treat as extra frame separators, splitting one real frame into
    several bogus ones. Instead, demangle each distinct frame individually and sanitize any `;` in
    the result (see `_demangle_frames`), preserving the folded frame count exactly.
    """
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    frames: set[str] = set()
    for line in lines:
        if not line.strip():
            continue
        stack, sep, count = line.rpartition(" ")
        if not sep or not count.isdigit():
            continue
        frames.update(f for f in stack.split(";") if f)
    mapping = _demangle_frames(frames)
    if not mapping:
        return
    rebuilt = [_apply_demangle_to_line(line, mapping) for line in lines]
    path.write_text("\n".join(rebuilt) + "\n", encoding="utf-8")


def _folded_has_samples(path: Path) -> bool:
    """Whether a folded file contains at least one stack with a positive sample count."""
    if not path.exists() or path.stat().st_size == 0:
        return False
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        line = line.strip()
        if not line:
            continue
        _, sep, count = line.rpartition(" ")
        if sep and count.isdigit() and int(count) > 0:
            return True
    return False


def main() -> None:
    parser = argparse.ArgumentParser(description="NVX flamegraph post-processing")
    parser.add_argument(
        "mode", choices=["guest", "full"], help="guest-only or guest+host"
    )
    parser.add_argument(
        "--guest-folded", type=Path, required=True, help="guest folded input file"
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=None,
        help="output directory (default: sibling <stem>-flamegraph)",
    )
    parser.add_argument(
        "--process",
        default=r"microvm(?:\.exe)?",
        help="process regex used to filter host stacks (full mode)",
    )
    args = parser.parse_args()

    _check_prereqs(args.mode)

    guest_folded = args.guest_folded.resolve()
    if not guest_folded.exists():
        raise SystemExit(f"guest folded file not found: {guest_folded}")

    out_dir = (
        args.output_dir.resolve()
        if args.output_dir
        else guest_folded.parent / f"{guest_folded.stem}-flamegraph"
    )
    out_dir.mkdir(parents=True, exist_ok=True)

    guest_copy = out_dir / "guest.folded"
    # Snapshot the source profile's run id *before* reading its bytes, so provenance is later checked
    # against the exact run whose samples we captured. A concurrent nvx run can atomically replace
    # the source profile and its `.run`/`.pid` sidecars; without pinning the id, guest bytes from run
    # A could be merged with a host trace from run B whose freshly written sidecars would still pass
    # validation.
    expected_run = _read_run_id(guest_folded)
    guest_copy.write_text(
        guest_folded.read_text(encoding="utf-8", errors="replace"),
        encoding="utf-8",
    )
    # Re-read the source run id after copying. If it changed, the source was replaced mid-copy and
    # the bytes now in `guest_copy` may be torn or belong to a different run — refuse rather than
    # silently mis-attribute the profile.
    if _read_run_id(guest_folded) != expected_run:
        raise SystemExit(
            f"[error] guest profile {guest_folded} changed during processing "
            "(a concurrent run replaced it); rerun post-processing"
        )
    _demangle_folded(guest_copy)

    host_folded: Path | None = None
    host_failed = False
    host_diag: dict = {}
    if args.mode == "full":
        host_folded = extract_host_stacks(
            guest_folded, out_dir, args.process, host_diag, expected_run
        )
        if host_folded is not None:
            _demangle_folded(host_folded)
        else:
            host_failed = True
    host_lost = bool(host_diag.get("lost"))

    outputs = merge_and_render(guest_copy, host_folded, out_dir, title_prefix="NVX")

    if not outputs:
        raise SystemExit(
            f"[error] no flamegraphs produced (empty guest profile: {guest_folded})"
        )

    # Guest stack samples are the primary deliverable. A host-only render (guest sampled nothing)
    # must never be reported as success, even though the host artifacts make `outputs` nonempty.
    guest_empty = not _folded_has_samples(guest_copy)

    status = "warn" if (host_failed or guest_empty or host_lost) else "ok"
    print(f"[{status}] output dir: {out_dir}")
    for k, v in outputs.items():
        print(f"  {k}: {v}")

    if guest_empty:
        raise SystemExit(
            f"[error] guest profile has no samples ({guest_folded}); "
            "any host outputs are not a full-profile success"
        )
    if host_failed:
        # `full` was requested but the host trace could not be turned into stacks. Guest-only
        # outputs were still produced; exit nonzero so callers do not treat this as full success.
        raise SystemExit("[warn] host extraction failed; produced guest-only outputs")
    if host_lost:
        # The host trace was rendered but xperf reported dropped ETW events, so it is incomplete.
        # Exit nonzero so `full` mode never presents a partial host profile as a complete success.
        raise SystemExit(
            "[warn] host trace reported lost ETW events; the host profile is incomplete "
            "(increase WPR buffers or reduce system load for a complete capture)"
        )


if __name__ == "__main__":
    main()
