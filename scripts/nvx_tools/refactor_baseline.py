"""Capture reproducible evidence for the component-refactor baseline."""

from __future__ import annotations

import hashlib
import json
import os
import platform
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Sequence

from .backends.base import HostBackend
from .common import CommandResult, REPO_ROOT, ScriptError, run_capture


SNAPSHOT_FORMATS: dict[str, dict[str, str | list[str]]] = {
    "linux-kvm": {
        "current": "KVMSNAP",
        "readable": ["KVMSNAP"],
    },
    "windows-whp": {
        "current": "WHPSNAP",
        "readable": ["WHPSNAP"],
    },
}

Runner = Callable[..., CommandResult]


@dataclass(frozen=True)
class BaselineConfig:
    output: Path
    suites: tuple[str, ...]
    artifacts: tuple[Path, ...] = ()
    console_snapshot: Path | None = None
    portb_snapshot: Path | None = None
    require_clean: bool = False
    role: str = "candidate"
    base_revision: str = ""
    candidate_revision: str = ""
    source_root: Path | None = None


@dataclass(frozen=True)
class CommandSpec:
    label: str
    args: tuple[str | Path, ...]
    log_path: Path


def _utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _display_path(path: Path) -> str:
    resolved = path.expanduser().resolve()
    try:
        return resolved.relative_to(REPO_ROOT.resolve()).as_posix()
    except ValueError:
        return str(resolved)


def _file_record(path: Path, root: Path | None = None) -> dict[str, object]:
    record: dict[str, object] = {
        "path": path.relative_to(root).as_posix() if root else _display_path(path),
        "size": path.stat().st_size,
        "sha256": _sha256_file(path),
    }
    if path.name == "state.bin":
        with path.open("rb") as source:
            magic = source.read(7)
        record["snapshot_magic_ascii"] = magic.decode("ascii", errors="replace")
        record["snapshot_magic_hex"] = magic.hex()
    return record


def artifact_record(path: Path) -> dict[str, object]:
    resolved = path.expanduser().resolve()
    if not resolved.exists():
        return {"path": _display_path(resolved), "status": "missing"}
    if resolved.is_file():
        return {"kind": "file", "status": "present", **_file_record(resolved)}
    if not resolved.is_dir():
        return {
            "path": _display_path(resolved),
            "status": "unsupported",
        }

    files = [
        _file_record(child, resolved)
        for child in sorted(resolved.rglob("*"), key=lambda item: item.as_posix())
        if child.is_file()
    ]
    tree_digest = hashlib.sha256()
    for record in files:
        tree_digest.update(str(record["path"]).encode("utf-8"))
        tree_digest.update(b"\0")
        tree_digest.update(str(record["size"]).encode("ascii"))
        tree_digest.update(b"\0")
        tree_digest.update(bytes.fromhex(str(record["sha256"])))
    return {
        "path": _display_path(resolved),
        "kind": "directory",
        "status": "present",
        "tree_sha256": tree_digest.hexdigest(),
        "files": files,
    }


def _probe(runner: Runner, args: Sequence[str | Path], source_root: Path) -> str | None:
    try:
        result = runner(args, cwd=source_root)
    except OSError:
        return None
    if result.returncode != 0:
        return None
    return result.stdout.decode("utf-8", errors="replace").rstrip("\r\n")


def _git_output(runner: Runner, args: Sequence[str], source_root: Path) -> bytes:
    try:
        result = runner(["git", *args], cwd=source_root)
    except OSError as error:
        raise ScriptError(f"cannot run git {' '.join(args)}: {error}") from error
    if result.returncode != 0:
        raise ScriptError(f"git {' '.join(args)} exited {result.returncode}")
    return result.stdout


def _source_state(runner: Runner, source_root: Path) -> dict[str, object]:
    revision = _git_output(runner, ["rev-parse", "HEAD"], source_root).decode().strip()
    status = (
        _git_output(runner, ["status", "--porcelain=v1"], source_root)
        .decode("utf-8", errors="replace")
        .rstrip("\r\n")
    )
    tracked_diff = _git_output(runner, ["diff", "--binary", "HEAD"], source_root)
    untracked_paths = [
        Path(os.fsdecode(value))
        for value in _git_output(
            runner,
            ["ls-files", "--others", "--exclude-standard", "-z"],
            source_root,
        ).split(b"\0")
        if value
    ]
    untracked_files = [
        _file_record(source_root / path)
        for path in sorted(untracked_paths, key=lambda item: item.as_posix())
    ]
    changes = status.splitlines() if status else []
    fingerprint = hashlib.sha256()
    fingerprint.update(revision.encode("ascii"))
    fingerprint.update(b"\0")
    fingerprint.update(tracked_diff)
    for record in untracked_files:
        fingerprint.update(str(record["path"]).encode("utf-8"))
        fingerprint.update(b"\0")
        fingerprint.update(bytes.fromhex(str(record["sha256"])))
    return {
        "revision": revision,
        "clean": not changes,
        "worktree_changes": changes,
        "tracked_diff_sha256": hashlib.sha256(tracked_diff).hexdigest(),
        "untracked_files": untracked_files,
        "worktree_fingerprint": fingerprint.hexdigest(),
    }


def _host_state(
    runner: Runner, backend: HostBackend, source_root: Path
) -> dict[str, object]:
    uname = platform.uname()
    runner_environment = {
        name: os.environ[name]
        for name in (
            "GITHUB_RUN_ATTEMPT",
            "GITHUB_RUN_ID",
            "RUNNER_ARCH",
            "RUNNER_NAME",
            "RUNNER_OS",
        )
        if name in os.environ
    }
    return {
        "backend": backend.name,
        "hostname": uname.node,
        "system": uname.system,
        "release": uname.release,
        "version": uname.version,
        "machine": uname.machine,
        "processor": uname.processor,
        "logical_cpu_count": os.cpu_count(),
        "python": sys.version.replace("\n", " "),
        "rustc": _probe(runner, ["rustc", "--version", "--verbose"], source_root),
        "cargo": _probe(runner, ["cargo", "--version", "--verbose"], source_root),
        "runner_environment": runner_environment,
    }


def _workflow_prefix(backend: HostBackend) -> tuple[str | Path, ...]:
    backend_name = "kvm" if backend.name == "linux-kvm" else "whp"
    return (
        sys.executable,
        "scripts/nvx.py",
        "--backend",
        backend_name,
    )


def _core_commands(include_architecture: bool) -> list[CommandSpec]:
    python_tests = [
        sys.executable,
        "-m",
        "unittest",
        "scripts/test_performance.py",
        "scripts/test_flamegraph.py",
    ]
    if include_architecture:
        python_tests.append("scripts/tests/test_architecture.py")
    python_tests.extend(("scripts/tests/test_nvx_tools.py", "-v"))
    return [
        CommandSpec(
            "format-check",
            ("make", "format-check"),
            Path("checks/01-format-check.log"),
        ),
        CommandSpec(
            "release-build",
            ("cargo", "build", "--release"),
            Path("checks/02-release-build.log"),
        ),
        CommandSpec(
            "release-tests",
            ("cargo", "test", "--release"),
            Path("checks/03-release-tests.log"),
        ),
        CommandSpec(
            "python-compile",
            (sys.executable, "-m", "compileall", "-q", "scripts", "alpine"),
            Path("checks/04-python-compile.log"),
        ),
        CommandSpec(
            "python-tests",
            tuple(python_tests),
            Path("checks/05-python-tests.log"),
        ),
    ]


def _backend_commands(
    backend: HostBackend, include_interruption: bool
) -> list[CommandSpec]:
    workflow = _workflow_prefix(backend)
    kernel = backend.artifact("vmlinux")
    initrd = backend.artifact("initramfs.cpio.gz")
    commands = [
        CommandSpec(
            "backend-selftest",
            (backend.executable(), "--selftest"),
            Path("backend/01-selftest.log"),
        ),
        CommandSpec(
            "boot-smoke",
            (*workflow, "test-boot", "--kernel", kernel, "--initrd", initrd),
            Path("backend/02-boot.log"),
        ),
        CommandSpec(
            "exec-smoke",
            (*workflow, "test-exec", "--kernel", kernel, "--initrd", initrd),
            Path("backend/03-exec.log"),
        ),
        CommandSpec(
            "profiling-smoke",
            (
                *workflow,
                "test-profiling",
                "--kernel",
                kernel,
                "--initrd",
                initrd,
            ),
            Path("backend/04-profiling.log"),
        ),
    ]
    if backend.name == "linux-kvm" and include_interruption:
        commands.append(
            CommandSpec(
                "interruption-smoke",
                (
                    *workflow,
                    "test-interruption",
                    "--kernel",
                    kernel,
                    "--initrd",
                    initrd,
                ),
                Path("backend/05-interruption.log"),
            )
        )
    return commands


def _snapshot_commands(
    config: BaselineConfig, backend: HostBackend
) -> list[CommandSpec]:
    snapshot = config.output.expanduser().resolve() / "snapshots" / "python-hello"
    commands = [
        CommandSpec(
            "python-snapshot-capture-and-replay",
            (
                *_workflow_prefix(backend),
                "snapshot-demo-hello",
                "--kernel",
                backend.artifact("vmlinux"),
                "--initrd",
                backend.artifact("initramfs-python.cpio.gz"),
                "--snapshot",
                snapshot,
                "--runs",
                "2",
            ),
            Path("snapshot/python-hello.log"),
        ),
    ]
    return commands


def _benchmark_commands(
    config: BaselineConfig, backend: HostBackend, revision: str
) -> list[CommandSpec]:
    if config.console_snapshot is None or config.portb_snapshot is None:
        raise ScriptError(
            "the benchmarks suite requires --console-snapshot and --portb-snapshot"
        )
    workflow = _workflow_prefix(backend)
    output = config.output.expanduser().resolve()
    snapshots = output / "snapshots" / "benchmarks"
    kernel = backend.artifact("vmlinux")
    initrd = backend.artifact("initramfs.cpio.gz")
    python_initrd = backend.artifact("initramfs-python.cpio.gz")
    return [
        CommandSpec(
            "cold-start-benchmark",
            (
                *workflow,
                "measure-coldstart",
                "--kernel",
                kernel,
                "--initrd",
                initrd,
                "--runs",
                "5",
            ),
            Path("benchmarks/cold-start.log"),
        ),
        CommandSpec(
            "console-benchmark",
            (
                *workflow,
                "bench-console-output",
                "--snapshot",
                config.console_snapshot,
                "--portb-snapshot",
                config.portb_snapshot,
                "--runs",
                "5",
            ),
            Path("benchmarks/console-output.log"),
        ),
        CommandSpec(
            "virtfs-benchmark",
            (
                *workflow,
                "bench-virtfs",
                "--kernel",
                kernel,
                "--initrd",
                initrd,
                "--runs",
                "3",
            ),
            Path("benchmarks/virtfs.log"),
        ),
        CommandSpec(
            "pandas-snapshot-benchmark",
            (
                *workflow,
                "snapshot-demo-pandas",
                "--kernel",
                kernel,
                "--initrd",
                python_initrd,
                "--snapshot",
                snapshots / "pandas",
                "--runs",
                "5",
            ),
            Path("benchmarks/snapshot.log"),
        ),
        CommandSpec(
            "hello-snapshot-benchmark",
            (
                *workflow,
                "snapshot-demo-hello",
                "--kernel",
                kernel,
                "--initrd",
                python_initrd,
                "--snapshot",
                snapshots / "hello",
                "--runs",
                "5",
            ),
            Path("benchmarks/snapshot-hello.log"),
        ),
        CommandSpec(
            "shell-snapshot-benchmark",
            (
                *workflow,
                "bench-snapshot-shell",
                "--kernel",
                kernel,
                "--initrd",
                initrd,
                "--snapshot",
                snapshots / "shell",
                "--runs",
                "5",
                "--memories",
                "64 128 256 512",
            ),
            Path("benchmarks/shell-snapshot.log"),
        ),
        CommandSpec(
            "network-snapshot-benchmark",
            (
                *workflow,
                "bench-net-snapshot",
                "--kernel",
                kernel,
                "--initrd",
                initrd,
                "--snapshot",
                snapshots / "network",
                "--runs",
                "5",
            ),
            Path("benchmarks/network.log"),
        ),
        CommandSpec(
            "canonical-metric-collection",
            (
                sys.executable,
                "scripts/performance.py",
                "collect",
                "--platform",
                backend.name,
                "--commit",
                revision,
                "--input-dir",
                output / "benchmarks",
                "--output-dir",
                output / "results",
                "--require-network",
                "--require-shell-snapshot",
                "--require-shared-suite",
            ),
            Path("checks/06-canonical-metrics.log"),
        ),
    ]


def _commands(
    config: BaselineConfig, backend: HostBackend, revision: str
) -> list[CommandSpec]:
    commands: list[CommandSpec] = []
    for suite in config.suites:
        if suite == "core":
            commands.extend(_core_commands(config.role == "candidate"))
        elif suite == "backend":
            commands.extend(_backend_commands(backend, config.role == "candidate"))
        elif suite == "snapshot":
            commands.extend(_snapshot_commands(config, backend))
        elif suite == "benchmarks":
            commands.extend(_benchmark_commands(config, backend, revision))
        else:
            raise ScriptError(f"unknown baseline suite {suite!r}")
    return commands


def _write_command_log(path: Path, spec: CommandSpec, result: CommandResult) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    command = subprocess.list2cmdline([os.fspath(value) for value in spec.args])
    stdout = result.stdout.decode("utf-8", errors="replace")
    stderr = result.stderr.decode("utf-8", errors="replace")
    path.write_text(
        f"$ {command}\n\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        encoding="utf-8",
        newline="\n",
    )


def _run_commands(
    output: Path,
    commands: Sequence[CommandSpec],
    runner: Runner,
    role: str,
    source_root: Path,
) -> list[dict[str, object]]:
    records: list[dict[str, object]] = []
    for spec in commands:
        print(f">> {role}: {spec.label}")
        started_utc = _utc_now()
        started = time.perf_counter()
        try:
            result = runner(spec.args, cwd=source_root)
        except OSError as error:
            result = CommandResult(
                tuple(os.fspath(value) for value in spec.args),
                127,
                b"",
                str(error).encode("utf-8", errors="replace"),
            )
        duration_ms = round((time.perf_counter() - started) * 1000, 3)
        _write_command_log(output / spec.log_path, spec, result)
        records.append(
            {
                "label": spec.label,
                "command": [os.fspath(value) for value in spec.args],
                "started_utc": started_utc,
                "duration_ms": duration_ms,
                "returncode": result.returncode,
                "timed_out": result.timed_out,
                "passed": result.returncode == 0 and not result.timed_out,
                "log": spec.log_path.as_posix(),
            }
        )
    return records


def _artifact_paths(config: BaselineConfig, backend: HostBackend) -> list[Path]:
    candidates = [
        backend.executable(),
        backend.artifact("vmlinux"),
        backend.artifact("vmlinux-profiling"),
        backend.artifact("initramfs.cpio.gz"),
        backend.artifact("initramfs-python.cpio.gz"),
        backend.artifact("initramfs-python-agent.cpio.gz"),
        *config.artifacts,
    ]
    if config.console_snapshot is not None:
        candidates.append(config.console_snapshot)
    if config.portb_snapshot is not None:
        candidates.append(config.portb_snapshot)
    output = config.output.expanduser().resolve()
    if "snapshot" in config.suites:
        candidates.append(output / "snapshots" / "python-hello")
    if "benchmarks" in config.suites:
        snapshots = output / "snapshots" / "benchmarks"
        candidates.extend(
            (
                snapshots / "pandas",
                snapshots / "hello",
                snapshots / "network",
                *(snapshots / f"shell-{memory}" for memory in (64, 128, 256, 512)),
            )
        )
    unique: dict[Path, Path] = {}
    for candidate in candidates:
        resolved = candidate.expanduser().resolve()
        unique.setdefault(resolved, candidate)
    return list(unique.values())


def record_refactor_baseline(
    config: BaselineConfig,
    backend: HostBackend,
    *,
    runner: Runner = run_capture,
) -> Path:
    if config.role not in ("baseline", "candidate"):
        raise ScriptError(f"invalid evidence role {config.role!r}")
    if not config.base_revision or not config.candidate_revision:
        raise ScriptError("evidence requires both base and candidate revisions")
    source_root = (
        config.source_root.expanduser().resolve()
        if config.source_root is not None
        else backend.repo_root.resolve()
    )
    if not source_root.is_dir():
        raise ScriptError(f"evidence source root is not a directory: {source_root}")
    backend = backend.with_repo_root(source_root)
    output = config.output.expanduser().resolve()
    if output.exists() and (not output.is_dir() or any(output.iterdir())):
        raise ScriptError(f"baseline output directory must be empty: {output}")
    output.mkdir(parents=True, exist_ok=True)

    source_before = _source_state(runner, source_root)
    expected_revision = (
        config.base_revision if config.role == "baseline" else config.candidate_revision
    )
    if source_before["revision"] != expected_revision:
        raise ScriptError(
            f"{config.role} evidence expected revision {expected_revision}, "
            f"found {source_before['revision']}"
        )
    if config.require_clean and not source_before["clean"]:
        raise ScriptError(f"{config.role} evidence requires a clean Git worktree")

    artifact_paths = _artifact_paths(config, backend)
    artifacts_before = [artifact_record(artifact) for artifact in artifact_paths]
    commands = _commands(config, backend, str(source_before["revision"]))
    command_records = _run_commands(output, commands, runner, config.role, source_root)
    artifacts: list[dict[str, object]] = []
    for artifact in artifact_paths:
        resolved = artifact.expanduser().resolve()
        if output == resolved or output.is_relative_to(resolved):
            raise ScriptError(
                f"evidence output cannot be contained by recorded artifact {artifact}"
            )
        artifacts.append(artifact_record(artifact))

    artifacts_changed = [
        str(before["path"])
        for before, after in zip(artifacts_before, artifacts, strict=True)
        if before.get("status") == "present"
        and after.get("status") == "present"
        and before != after
    ]

    source_after = _source_state(runner, source_root)
    source_changed = (
        source_before["worktree_fingerprint"] != source_after["worktree_fingerprint"]
    )

    manifest: dict[str, object] = {
        "schema_version": 3,
        "evidence_role": config.role,
        "revisions": {
            "base": config.base_revision,
            "candidate": config.candidate_revision,
        },
        "created_utc": _utc_now(),
        "source": source_before,
        "source_before": source_before,
        "source_after": source_after,
        "source_changed": source_changed,
        "host": _host_state(runner, backend, source_root),
        "suites": list(config.suites),
        "snapshot_formats": SNAPSHOT_FORMATS,
        "commands": command_records,
        "artifacts_before": artifacts_before,
        "artifacts": artifacts,
        "artifacts_changed": artifacts_changed,
    }
    manifest_path = output / "manifest.json"
    temporary = output / "manifest.json.tmp"
    temporary.write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
        newline="\n",
    )
    temporary.replace(manifest_path)

    failures = [
        str(record["label"])
        for record in command_records
        if record["passed"] is not True
    ]
    if failures:
        raise ScriptError(
            f"{config.role} evidence recorded {len(failures)} failed command(s): "
            f"{', '.join(failures)}; "
            f"see {manifest_path}"
        )
    if config.require_clean and not source_after["clean"]:
        raise ScriptError(
            f"{config.role} evidence left a dirty Git worktree; see {manifest_path}"
        )
    if source_changed:
        raise ScriptError(
            f"source state changed during {config.role} evidence; see {manifest_path}"
        )
    if artifacts_changed:
        raise ScriptError(
            f"pre-existing artifacts changed during {config.role} evidence: "
            f"{', '.join(artifacts_changed)}; see {manifest_path}"
        )
    print(f"{config.role.title()} evidence: {manifest_path}")
    return manifest_path
