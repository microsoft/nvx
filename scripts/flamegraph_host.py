#!/usr/bin/env python3
# Copyright(c) The microvm authors.
# Licensed under the MIT License.

"""
Platform-specific host stack extraction.

Windows: ETL -> xperf dumper -> folded (Nanvix-style text parsing).
Linux:   perf.data -> perf script -> inferno-collapse-perf -> folded.
"""

from __future__ import annotations

import contextlib
import hashlib
import os
import re
import shutil
import subprocess
import sys
import tempfile

from collections import Counter
from pathlib import Path


def _find_xperf() -> str | None:
    executable = shutil.which("xperf")
    if executable is not None or sys.platform != "win32":
        return executable

    relative_path = Path(
        "Windows Kits", "10", "Windows Performance Toolkit", "xperf.exe"
    )
    for variable in ("ProgramFiles(x86)", "ProgramW6432", "ProgramFiles"):
        root = os.environ.get(variable)
        if root:
            candidate = Path(root) / relative_path
            if candidate.is_file():
                return str(candidate)
    return None


def extract_host_stacks(
    guest_folded: Path,
    out_dir: Path,
    process_regex: str = r"microvm(?:\.exe)?",
    diag: dict | None = None,
    expected_run: str | None = None,
    expected_pid: int | None = None,
) -> Path | None:
    if sys.platform == "win32":
        return _extract_windows_etl(
            guest_folded,
            out_dir,
            process_regex,
            diag,
            expected_run,
            expected_pid,
        )
    if sys.platform.startswith("linux"):
        return _extract_linux_perf(
            guest_folded, out_dir, process_regex, diag, expected_run
        )
    print(f"[host] unsupported platform: {sys.platform}")
    return None


def _extract_linux_perf(
    guest_folded: Path,
    out_dir: Path,
    process_regex: str,
    diag: dict | None = None,
    expected_run: str | None = None,
) -> Path | None:
    perf_data = guest_folded.with_suffix(".host.perf.data")
    if not perf_data.exists():
        print(f"[host] missing host trace: {perf_data}")
        return None
    if not _provenance_ok(guest_folded, perf_data, expected_run):
        return None
    if shutil.which("perf") is None:
        print("[host] perf not found")
        return None
    if shutil.which("inferno-collapse-perf") is None:
        print("[host] inferno-collapse-perf not found (cargo install inferno)")
        return None

    # `perf record -p <pid>` already scoped the capture to the VMM process, so every collapsed
    # stack belongs to it — including its worker threads (`vcpu-0`, `vcpu-1`, ...). Do NOT filter by
    # a comm/image regex here: the collapsed comm column carries the per-thread name, so a
    # `microvm`-only regex would silently drop the vCPU AP threads and lose most guest-adjacent host
    # activity. `process_regex` is retained only for the Windows extractor, where system-wide ETW
    # providers require a PID/image filter. Keep all non-empty collapsed lines.
    #
    # Capture each stage's stderr to a *temporary file* (not a pipe) so the underlying tool's
    # diagnostics can be reported on failure without risking a deadlock: with a pipe, a stage that
    # writes more stderr than the pipe buffer holds would block forever, since these pipes are only
    # drained after the processes are waited on. Temp files never block the writer.
    perf_err_file = tempfile.TemporaryFile()
    collapse_err_file = tempfile.TemporaryFile()
    perf_proc = subprocess.Popen(
        [
            "perf",
            "script",
            "-i",
            str(perf_data),
            "-F",
            "comm,pid,tid,time,event,ip,sym,dso",
        ],
        stdout=subprocess.PIPE,
        stderr=perf_err_file,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    collapse_proc = subprocess.Popen(
        ["inferno-collapse-perf"],
        stdin=perf_proc.stdout,
        stdout=subprocess.PIPE,
        stderr=collapse_err_file,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    # Let perf_proc receive SIGPIPE if the collapse stage exits early.
    assert perf_proc.stdout is not None
    perf_proc.stdout.close()

    kept: list[str] = []
    assert collapse_proc.stdout is not None
    for line in collapse_proc.stdout:
        line = line.rstrip("\n")
        if line:
            kept.append(line)
    collapse_proc.wait()
    perf_proc.wait()

    def _read_temp(f) -> str:
        try:
            f.seek(0)
            return f.read().decode("utf-8", "replace").strip()
        finally:
            f.close()

    perf_err = _read_temp(perf_err_file)
    collapse_err = _read_temp(collapse_err_file)

    if perf_proc.returncode not in (0, None):
        detail = f": {perf_err}" if perf_err else ""
        print(f"[host] perf script failed (exit {perf_proc.returncode}){detail}")
        return None
    if collapse_proc.returncode not in (0, None):
        detail = f": {collapse_err}" if collapse_err else ""
        print(
            f"[host] inferno-collapse-perf failed (exit {collapse_proc.returncode}){detail}"
        )
        return None
    if not kept:
        detail = f" ({perf_err})" if perf_err else ""
        print(f"[host] no host stacks in perf trace{detail}")
        return None

    out = out_dir / "host.folded"
    out.write_text("\n".join(kept) + "\n", encoding="utf-8")
    return out


# xperf `-a dumper` stack record: Stack, TimeStamp, ThreadId, FrameNo, ?, Symbol
_WIN_STACK_RE = re.compile(
    r"^\s*Stack,\s*(\d+),\s*(\d+),\s*(\d+),\s*([^,]+),\s*(.+?)\s*$"
)

# ETW loss is reported in several phrasings across tools/versions, e.g. "Events were lost",
# "5 events were lost", "buffers lost = 5", or "lost 5 buffers". Match a count in any position so a
# zero count (e.g. "0 events lost" in a summary) is not misread as loss, and fall back to a bare
# notice with no number, which a tool only prints when data was actually dropped.
_LOSS_TRAILING_RE = re.compile(
    r"\b(?:events?|buffers?)\s+(?:were\s+)?lost\b\s*[:=]?\s*(\d+)", re.IGNORECASE
)
_LOSS_LEADING_RE = re.compile(
    r"\b(\d+)\s+(?:events?|buffers?)\s+(?:were\s+)?lost\b", re.IGNORECASE
)
_LOSS_MID_RE = re.compile(
    r"\blost\b\s*[:=]?\s*(\d+)\s+(?:events?|buffers?)\b", re.IGNORECASE
)
_LOSS_WORD_RE = re.compile(
    r"\b(?:events?|buffers?)\s+(?:were\s+)?lost\b|\blost\s+(?:events?|buffers?)\b",
    re.IGNORECASE,
)


def _line_reports_loss(line: str) -> bool:
    """Whether one line reports lost ETW events or buffers.

    A line carrying an explicit count is loss only if that count is positive, so a "0 events lost"
    summary is not misread. A bare notice with no number (e.g. "Events were lost") is treated as
    loss because a tool only prints it when data was dropped.
    """
    saw_count = False
    for regex in (_LOSS_TRAILING_RE, _LOSS_LEADING_RE, _LOSS_MID_RE):
        for m in regex.finditer(line):
            saw_count = True
            if int(m.group(1)) > 0:
                return True
    if saw_count:
        return False
    return bool(_LOSS_WORD_RE.search(line))


def _reports_event_loss(text: str) -> bool:
    """Whether any line of `text` reports lost ETW events or buffers."""
    return any(_line_reports_loss(line) for line in text.splitlines())


def _xperf_env(symbol_path: str | None) -> dict | None:
    """Environment for the xperf child, or ``None`` to inherit the parent unchanged.

    When `symbol_path` is given it is prepended to `_NT_SYMBOL_PATH` (any ambient value — e.g. a
    user's system-module symbol-server cache — is preserved after it) so xperf resolves the VMM's
    own frames from the PDB in that directory.
    """
    if not symbol_path:
        return None
    env = os.environ.copy()
    ambient = env.get("_NT_SYMBOL_PATH", "")
    env["_NT_SYMBOL_PATH"] = f"{symbol_path};{ambient}" if ambient else symbol_path
    return env


def _xperf_lines(etl: Path, diag: dict | None = None, symbol_path: str | None = None):
    """Streams `xperf -a dumper` output line by line, raising on a nonzero exit.

    When `diag` is supplied, `diag["events_lost"]` is set to whether xperf reported dropped events
    (on either stdout or stderr). `-tle` tolerates lost events so extraction still completes, but a
    partial trace must be surfaced rather than silently accepted, so the loss notice is captured
    instead of discarded.

    When `symbol_path` is supplied it is prepended to `_NT_SYMBOL_PATH` for the xperf child only, so
    xperf resolves the VMM's own frames from the PDB in that directory.
    """
    # `-stacktimeshifting` shifts each stack-walk record onto the timestamp of the event it
    # describes, so the kernel and user stack fragments of one CPU sample share the sampled-profile
    # event's (timestamp, thread) key and are both folded into that sample instead of being lost.
    executable = _find_xperf()
    if executable is None:
        raise RuntimeError("xperf not found (Windows Performance Toolkit required)")
    cmd = [
        executable,
        "-i",
        str(etl),
        "-tle",
        "-tti",
        "-symbols",
        "-a",
        "dumper",
        "-stacktimeshifting",
    ]
    # Capture stderr to a temp file (not a pipe) so a large diagnostic stream cannot deadlock
    # against the stdout reader; scan it for the lost-events notice after the process exits.
    err_file = tempfile.TemporaryFile()
    proc = subprocess.Popen(
        cmd,
        stdout=subprocess.PIPE,
        stderr=err_file,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=_xperf_env(symbol_path),
    )
    assert proc.stdout is not None
    err = ""
    try:
        for line in proc.stdout:
            if diag is not None and _line_reports_loss(line):
                diag["events_lost"] = True
            yield line
    finally:
        proc.stdout.close()
        proc.wait()
        try:
            err_file.seek(0)
            err = err_file.read().decode("utf-8", "replace")
        finally:
            err_file.close()
        if diag is not None:
            if _reports_event_loss(err):
                diag["events_lost"] = True
            diag.setdefault("events_lost", False)
    if proc.returncode != 0:
        detail = f": {err.strip()}" if err.strip() else ""
        raise RuntimeError(
            f"xperf dumper failed with exit code {proc.returncode}{detail}"
        )


# The process field of a dumper record is `Image Name ( PID)`; capture the PID for exact filtering.
_WIN_PID_RE = re.compile(r"\(\s*(\d+)\s*\)")


def _win_match_process(field: str, proc_re: re.Pattern, target_pid: int | None) -> bool:
    """Whether a dumper process field belongs to the profiled run.

    When the recorder published its exact PID (via the `.pid` sidecar), match on that PID so a
    system-wide trace is narrowed to this run alone — even if another process of the same image
    name ran concurrently. Without a sidecar, fall back to the image-name regex.
    """
    if target_pid is not None:
        m = _WIN_PID_RE.search(field)
        return m is not None and int(m.group(1)) == target_pid
    return bool(proc_re.search(field))


def _win_fold_dumper(
    lines, proc_re: re.Pattern, target_pid: int | None
) -> tuple[Counter, int]:
    """Folds `xperf -a dumper` output into CPU sampled-profile stacks in a single streaming pass.

    `lines` is an iterable of dumper text lines. The parser is a sequential state machine: a
    `SampledProfile`/`SampledProfileNMI` record for the profiled process starts a sample and its
    following `Stack` records (shifted onto the same timestamp/thread by `-stacktimeshifting`)
    accumulate its frames; the sample is flushed on the next event, a mismatched stack record, or
    end of input. Only one sample's frames are held at a time, so memory is bounded by the number
    of *distinct* folded stacks rather than by every stack-bearing event in the trace.

    Only CPU sampled-profile events feed the flamegraph; scheduler events are never mixed in
    (weighting them equally with samples would distort the graph). Context switches for the process
    are counted and returned separately for a scheduling summary.
    """
    folded: Counter = Counter()
    cswitch = 0
    cur_ts: int | None = None
    cur_tid: int | None = None
    active = False
    frames: list[str] = []

    def flush() -> None:
        nonlocal frames, active
        if active and frames:
            # xperf emits frames leaf-first; the folded format expects root-first.
            folded[";".join(reversed(frames))] += 1
        frames = []
        active = False

    for line in lines:
        s = line.strip()
        if not s or s == "BeginHeader" or s == "EndHeader":
            continue

        m = _WIN_STACK_RE.match(line)
        if m and active:
            ts = int(m.group(1))
            tid = int(m.group(2))
            if ts == cur_ts and tid == cur_tid:
                func = m.group(5).strip().strip('"')
                # Keep unresolved frames as a placeholder rather than dropping them: dropping an
                # "Unknown" frame would silently merge distinct stacks (a;Unknown;b -> a;b) and make
                # an all-unknown sample vanish entirely, under-counting the profile and skewing the
                # relative widths of the remaining stacks in the merged flamegraph.
                if not func or func == "Unknown":
                    func = "[unknown]"
                frames.append(func)
                continue
            # A stack record for a different (timestamp, thread) ends the current sample.
            flush()
            continue

        # Any non-stack record (or a stack while no sample is active) ends the current sample; a
        # sampled-profile record for the process then starts the next one.
        flush()
        parts = [p.strip() for p in s.split(",")]
        ev = parts[0].lower() if parts else ""
        if ev in {"sampledprofile", "sampledprofilenmi"} and len(parts) >= 4:
            try:
                ts = int(parts[1])
                tid = int(parts[3])
            except ValueError:
                continue
            if _win_match_process(parts[2], proc_re, target_pid):
                cur_ts, cur_tid, active = ts, tid, True
        elif ev == "cswitch" and len(parts) >= 10:
            if _win_match_process(parts[2], proc_re, target_pid) or _win_match_process(
                parts[8], proc_re, target_pid
            ):
                cswitch += 1

    flush()
    return folded, cswitch


def _parse_manifest(text: str) -> dict[str, str]:
    """Parses `key=value` provenance lines into a dict, ignoring blanks."""
    out: dict[str, str] = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or "=" not in line:
            continue
        key, _, value = line.partition("=")
        out[key.strip()] = value.strip()
    return out


def _read_run_id(guest_folded: Path) -> str | None:
    """Reads the guest profile's provenance run id from `<guest_folded>.run`, if present."""
    sidecar = guest_folded.with_name(guest_folded.name + ".run")
    try:
        return _parse_manifest(sidecar.read_text(encoding="utf-8")).get("run")
    except OSError:
        return None


def _read_host_manifest(trace: Path) -> dict[str, str]:
    """Reads the host trace's `.pid` manifest (`pid=<n>` / `run=<id>` lines), if present."""
    sidecar = trace.with_name(trace.name + ".pid")
    try:
        return _parse_manifest(sidecar.read_text(encoding="utf-8"))
    except OSError:
        return {}


def _provenance_ok(
    guest_folded: Path, trace: Path, expected_run: str | None = None
) -> bool:
    """Whether `trace` provably belongs to the same run as the guest profile.

    Requires the guest profile's run id and the host trace's manifest run id (`<trace>.pid`) to both
    be present and identical. A host trace is only ever renamed onto its stable path after the
    recorder finalizes it and writes a manifest tagged with the run id, so a matching id proves the
    trace was produced by *this* run. This refuses a stale host trace left by a previous run (e.g.
    after a subsequent guest-only run) instead of merging it as if current.

    `expected_run`, when provided, is the run id the caller pinned from the source profile *before*
    copying its bytes; it is used instead of re-reading `<guest_folded>.run`, so a concurrent run
    that atomically replaced the source and its sidecars cannot make a mismatched host trace pass by
    substituting a fresh, self-consistent set of sidecars. When `None` (standalone callers), the id
    is read from `<guest_folded>.run`.
    """
    guest_run = expected_run if expected_run is not None else _read_run_id(guest_folded)
    if guest_run is None:
        print(
            f"[host] missing guest run-id sidecar {guest_folded.name}.run; "
            "cannot verify that the host trace belongs to this run"
        )
        return False
    host_run = _read_host_manifest(trace).get("run")
    if host_run is None:
        print(
            f"[host] host trace {trace.name} has no run-id manifest; "
            "refusing to merge an unverified trace"
        )
        return False
    if host_run != guest_run:
        print(
            f"[host] host trace {trace.name} belongs to a different run "
            f"(guest run={guest_run}, host run={host_run}); refusing a stale trace"
        )
        return False
    return True


def _extract_windows_etl(
    guest_folded: Path,
    out_dir: Path,
    process_regex: str,
    diag: dict | None = None,
    expected_run: str | None = None,
    expected_pid: int | None = None,
) -> Path | None:
    etl = guest_folded.with_suffix(".host.etl")
    if not etl.exists():
        print(f"[host] missing host trace: {etl}")
        return None
    guest_run = expected_run if expected_run is not None else _read_run_id(guest_folded)
    if guest_run is None:
        print(
            f"[host] missing guest run-id sidecar {guest_folded.name}.run; "
            "cannot verify that the host trace belongs to this run"
        )
        return None
    manifest_path = etl.with_name(etl.name + ".pid")
    try:
        manifest_bytes = manifest_path.read_bytes()
    except OSError:
        print(
            f"[host] host trace {etl.name} has no run-id manifest; "
            "refusing to merge an unverified trace"
        )
        return None
    manifest = _parse_manifest(manifest_bytes.decode("utf-8", errors="replace"))
    host_run = manifest.get("run")
    if host_run != guest_run:
        print(
            f"[host] host trace {etl.name} belongs to a different run "
            f"(guest run={guest_run}, host run={host_run}); refusing a stale trace"
        )
        return None
    if _find_xperf() is None:
        print("[host] xperf not found (Windows Performance Toolkit required)")
        return None

    proc_re = re.compile(process_regex, re.IGNORECASE)
    pid_text = manifest.get("pid")
    if pid_text is None or not pid_text.isdigit() or int(pid_text) <= 0:
        print(
            f"[host] host trace {etl.name} has no valid recorder PID; "
            "refusing image-name fallback"
        )
        return None
    target_pid = int(pid_text)
    if expected_pid is not None and target_pid != expected_pid:
        print(
            f"[host] host trace {etl.name} belongs to PID {target_pid}, "
            f"expected launched NVX PID {expected_pid}; refusing a stale trace"
        )
        return None
    print(f"[host] filtering host trace to recorder PID {target_pid}")
    expected_sha256 = manifest.get("sha256", "")
    if re.fullmatch(r"[0-9a-fA-F]{64}", expected_sha256) is None:
        print(
            f"[host] host trace {etl.name} has no valid SHA-256 provenance; "
            "refusing mutable trace input"
        )
        return None
    pinned_etl = _pin_windows_trace(
        etl,
        out_dir,
        manifest_path,
        manifest_bytes,
        expected_sha256.lower(),
    )
    if pinned_etl is None:
        return None

    # The recorder records its own image directory (where `microvm.pdb` sits) so xperf can resolve
    # the VMM's own frames to function names. Prepend it to `_NT_SYMBOL_PATH` for the xperf child;
    # a missing directory is simply not added, and system-module symbols still come from the
    # ambient path/cache.
    symbol_path: str | None = None
    image_dir = manifest.get("image_dir")
    if image_dir and Path(image_dir).is_dir():
        symbol_path = image_dir
        print(f"[host] resolving VMM symbols from {image_dir}")

    # Single streaming pass over the dumper output: fold sampled-profile stacks incrementally while
    # counting context switches. `xperf_diag` receives whether xperf reported dropped events.
    xperf_diag: dict = {}
    try:
        try:
            folded, cswitch_count = _win_fold_dumper(
                _xperf_lines(pinned_etl, xperf_diag, symbol_path), proc_re, target_pid
            )
        except RuntimeError as e:
            print(f"[host] {e}")
            return None
    finally:
        # Never let cleanup of the scratch copy mask the extraction result.
        with contextlib.suppress(OSError):
            pinned_etl.unlink(missing_ok=True)
    lost = bool(xperf_diag.get("events_lost"))
    if diag is not None:
        diag["lost"] = lost
        diag["pid"] = target_pid
        diag["samples"] = sum(folded.values())
        diag["context_switches"] = cswitch_count
    if lost:
        print(
            "[host] warning: xperf reported lost events; the host trace is incomplete "
            "(increase WPR buffers or reduce system load for a complete capture)"
        )
    if not folded:
        print("[host] no CPU sampled-profile stacks matched process filter")
        return None
    if cswitch_count:
        print(
            f"[host] note: {cswitch_count} context switches observed for the process "
            "(scheduling data, reported separately and not included in the CPU flamegraph)"
        )

    out = out_dir / "host.folded"
    with out.open("w", encoding="utf-8", newline="\n") as f:
        for stack, count in folded.most_common():
            f.write(f"{stack} {count}\n")
    return out


def _pin_windows_trace(
    etl: Path,
    out_dir: Path,
    manifest_path: Path,
    manifest_bytes: bytes,
    expected_sha256: str,
) -> Path | None:
    out_dir.mkdir(parents=True, exist_ok=True)
    descriptor, pinned_name = tempfile.mkstemp(
        prefix="host-input-", suffix=".etl", dir=out_dir
    )
    pinned = Path(pinned_name)
    try:
        with etl.open("rb") as source, os.fdopen(descriptor, "wb") as destination:
            source_before = os.fstat(source.fileno())
            actual_sha256 = _copy_trace_and_hash(source, destination)
            source_after = os.fstat(source.fileno())
        path_after = etl.stat()
        manifest_after = manifest_path.read_bytes()
    except OSError as error:
        print(f"[host] failed to pin host trace {etl.name}: {error}")
        pinned.unlink(missing_ok=True)
        return None

    identity_before = _file_identity(source_before)
    if (
        _file_identity(source_after) != identity_before
        or _file_identity(path_after) != identity_before
        or manifest_after != manifest_bytes
    ):
        print(f"[host] host trace {etl.name} changed while it was being pinned")
        pinned.unlink(missing_ok=True)
        return None
    if actual_sha256 != expected_sha256:
        print(
            f"[host] host trace {etl.name} failed SHA-256 provenance validation "
            f"(expected {expected_sha256}, got {actual_sha256})"
        )
        pinned.unlink(missing_ok=True)
        return None
    return pinned


def _copy_trace_and_hash(source, destination) -> str:
    hasher = hashlib.sha256()
    while chunk := source.read(1024 * 1024):
        destination.write(chunk)
        hasher.update(chunk)
    destination.flush()
    return hasher.hexdigest()


def _file_identity(stat: os.stat_result) -> tuple[int, int, int, int]:
    return (stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns)
