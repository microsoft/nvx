"""Credential-free deterministic executor for adversarial campaign primitives."""

from __future__ import annotations

import json
import math
import os
import platform
import re
import subprocess
import sys
import time
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import cast

from nvx_tools.adversarial_broker import (
    SCHEMA_VERSION,
    CaseDefinition,
    catalog_digest,
    load_campaign,
    parse_json_object,
    write_json,
)
from nvx_tools.adversarial_oracles import (
    BoundedProcessResult,
    OracleSession,
    run_bounded_process,
    sha256_file,
    write_process_metadata,
)
from nvx_tools.build import INITRAMFS_PROVENANCE_NAME
from nvx_tools.common import (
    BUILD_DIR,
    REPO_ROOT,
    ScriptError,
    artifact_path,
    openvmm_binary_path,
    require_file,
)
from nvx_tools.release import validate_runtime_artifact_provenance

MAX_PROTOCOL_LINE_BYTES = 1024 * 1024
_RUN_ID = re.compile(r"[a-z0-9][a-z0-9-]{0,95}\Z")
_BACKENDS = frozenset({"kvm", "mshv", "whp"})
_HOST_TYPES = frozenset({"baremetal", "virtual-machine", "unspecified"})
_REQUIRED_OUTCOME_REPORTS = {
    "managed-lifecycle": frozenset({"managed-outcome.json"}),
    "structured-outcome": frozenset(
        {"structured-outcome.json", "structured-outcome-rejected.json"}
    ),
}
_ENVIRONMENT_ALLOWLIST = frozenset(
    {
        "APPDATA",
        "COMSPEC",
        "HOME",
        "HOMEDRIVE",
        "HOMEPATH",
        "LANG",
        "LC_ALL",
        "LD_LIBRARY_PATH",
        "LOCALAPPDATA",
        "NUMBER_OF_PROCESSORS",
        "PATH",
        "PATHEXT",
        "PROCESSOR_ARCHITECTURE",
        "SYSTEMDRIVE",
        "SYSTEMROOT",
        "TEMP",
        "TMP",
        "TMPDIR",
        "USER",
        "USERNAME",
        "USERPROFILE",
        "WINDIR",
    }
)


def _exact_keys(
    value: Mapping[str, object],
    expected: set[str],
    *,
    description: str,
) -> None:
    actual = set(value)
    if actual != expected:
        raise ScriptError(
            f"{description} properties are {sorted(actual)}, expected {sorted(expected)}"
        )


def _integer(
    value: object,
    *,
    description: str,
    minimum: int,
    maximum: int,
) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ScriptError(f"{description} must be an integer")
    if value < minimum or value > maximum:
        raise ScriptError(f"{description} is outside {minimum}..{maximum}")
    return value


def _number(
    value: object,
    *,
    description: str,
    minimum: float,
    maximum: float,
) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ScriptError(f"{description} must be numeric")
    result = float(value)
    if not math.isfinite(result) or result < minimum or result > maximum:
        raise ScriptError(f"{description} is outside {minimum}..{maximum}")
    return result


def _string(
    value: object,
    *,
    description: str,
    maximum: int,
) -> str:
    if not isinstance(value, str) or not value or len(value) > maximum:
        raise ScriptError(
            f"{description} must be a nonempty string up to {maximum} characters"
        )
    return value


def _git(*arguments: str) -> str:
    completed = subprocess.run(
        ["git", "-C", str(REPO_ROOT), *arguments],
        check=True,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="strict",
        timeout=30.0,
    )
    return completed.stdout.strip()


def _target_metadata() -> dict[str, object]:
    nvx_status = _git(
        "status",
        "--porcelain",
        "--untracked-files=normal",
    ).splitlines()
    openvmm_status = _git(
        "-C",
        str(REPO_ROOT / "openvmm"),
        "status",
        "--porcelain",
        "--untracked-files=normal",
    ).splitlines()
    return {
        "nvx_commit": _git("rev-parse", "HEAD"),
        "nvx_dirty": bool(nvx_status),
        "nvx_status": nvx_status,
        "openvmm_commit": _git(
            "-C",
            str(REPO_ROOT / "openvmm"),
            "rev-parse",
            "HEAD",
        ),
        "openvmm_dirty": bool(openvmm_status),
        "openvmm_status": openvmm_status,
        "os": platform.system(),
        "os_release": platform.release(),
        "machine": platform.machine(),
        "python": platform.python_version(),
    }


def _artifact_metadata() -> dict[str, object]:
    paths = {
        "kernel": require_file(artifact_path("vmlinux"), "PVH kernel"),
        "initramfs": require_file(
            artifact_path("initramfs.cpio.gz"),
            "initramfs",
        ),
        "initramfs_package_manifest": require_file(
            artifact_path("initramfs.cpio.gz.packages.json"),
            "initramfs package manifest",
        ),
        "initramfs_provenance": require_file(
            BUILD_DIR / INITRAMFS_PROVENANCE_NAME,
            "initramfs provenance",
        ),
        "openvmm": require_file(
            openvmm_binary_path(),
            "OpenVMM release binary",
        ),
        "kernel_provenance": require_file(
            BUILD_DIR / "vmlinux.provenance.json",
            "kernel provenance",
        ),
        "openvmm_provenance": require_file(
            BUILD_DIR / "openvmm.provenance.json",
            "OpenVMM provenance",
        ),
    }
    validate_runtime_artifact_provenance()
    return {
        name: {
            "path": str(path),
            "sha256": sha256_file(path),
            "size": path.stat().st_size,
        }
        for name, path in paths.items()
    }


def sanitized_environment() -> dict[str, str]:
    environment = {
        key: value
        for key, value in os.environ.items()
        if key.upper() in _ENVIRONMENT_ALLOWLIST
    }
    environment["PYTHONUNBUFFERED"] = "1"
    return environment


def _category(result: BoundedProcessResult) -> str:
    if result.timed_out:
        return "timeout"
    if not result.teardown_complete:
        return "availability-failure"
    if result.returncode != 0:
        return "test-failure"
    return "passed"


def _read_openvmm_pids(path: Path) -> tuple[int, ...]:
    if not path.is_file():
        return ()
    if path.stat().st_size > 64 * 1024:
        raise ScriptError("OpenVMM PID journal exceeds the size limit")
    pids: list[int] = []
    for index, line in enumerate(
        path.read_text(encoding="utf-8").splitlines(), start=1
    ):
        document = parse_json_object(
            line,
            description=f"OpenVMM PID journal entry {index}",
        )
        _exact_keys(
            document,
            {"pid", "recorded_at_ns"},
            description=f"OpenVMM PID journal entry {index}",
        )
        pid = _integer(
            document["pid"],
            description=f"OpenVMM PID journal entry {index} pid",
            minimum=1,
            maximum=2**32 - 1,
        )
        _integer(
            document["recorded_at_ns"],
            description=f"OpenVMM PID journal entry {index} timestamp",
            minimum=1,
            maximum=2**63 - 1,
        )
        pids.append(pid)
    return tuple(pids)


def _process_running(pid: int) -> bool:
    if os.name != "nt":
        try:
            os.kill(pid, 0)
            return True
        except ProcessLookupError:
            return False
        except PermissionError:
            return True

    import ctypes

    process_query_limited_information = 0x1000
    still_active = 259
    error_access_denied = 5
    error_invalid_parameter = 87
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.OpenProcess.argtypes = (ctypes.c_uint32, ctypes.c_int, ctypes.c_uint32)
    kernel32.OpenProcess.restype = ctypes.c_void_p
    kernel32.GetExitCodeProcess.argtypes = (
        ctypes.c_void_p,
        ctypes.POINTER(ctypes.c_uint32),
    )
    kernel32.GetExitCodeProcess.restype = ctypes.c_int
    kernel32.CloseHandle.argtypes = (ctypes.c_void_p,)
    kernel32.CloseHandle.restype = ctypes.c_int
    handle = kernel32.OpenProcess(process_query_limited_information, False, pid)
    if not handle:
        error = ctypes.get_last_error()
        if error == error_invalid_parameter:
            return False
        if error == error_access_denied:
            return True
        raise OSError(error, "OpenProcess failed")
    try:
        exit_code = ctypes.c_uint32()
        if not kernel32.GetExitCodeProcess(handle, ctypes.byref(exit_code)):
            raise OSError(ctypes.get_last_error(), "GetExitCodeProcess failed")
        return exit_code.value == still_active
    finally:
        kernel32.CloseHandle(handle)


def _live_openvmm_pids(pids: tuple[int, ...]) -> tuple[int, ...]:
    return tuple(pid for pid in pids if _process_running(pid))


def _inventory_outcomes(
    root: Path,
    *,
    required_names: frozenset[str] = frozenset(),
) -> tuple[list[dict[str, object]], bool]:
    reports: list[dict[str, object]] = []
    teardown_complete = True
    found_names: set[str] = set()
    for path in sorted(root.rglob("*.json")):
        lowered = path.name.lower()
        if "outcome" not in lowered and "report" not in lowered:
            continue
        found_names.add(path.name)
        if path.stat().st_size > 1024 * 1024:
            reports.append(
                {
                    "path": str(path.relative_to(root)),
                    "sha256": sha256_file(path),
                    "oversized": True,
                }
            )
            teardown_complete = False
            continue
        entry: dict[str, object] = {
            "path": str(path.relative_to(root)),
            "sha256": sha256_file(path),
            "size": path.stat().st_size,
        }
        try:
            raw = cast(object, json.loads(path.read_text(encoding="utf-8")))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
            entry["parse_error"] = str(error)
            teardown_complete = False
        else:
            if isinstance(raw, dict):
                document = cast(dict[str, object], raw)
                teardown = document.get("teardown")
                if isinstance(teardown, dict):
                    fields = cast(dict[str, object], teardown)
                    entry["teardown"] = fields
                    if not fields or not all(
                        value is True for value in fields.values()
                    ):
                        teardown_complete = False
                else:
                    teardown_complete = False
            else:
                teardown_complete = False
        reports.append(entry)
    for missing_name in sorted(required_names - found_names):
        reports.append({"path": missing_name, "missing": True})
        teardown_complete = False
    return reports, teardown_complete


@dataclass(slots=True)
class _InitializedSession:
    run_id: str
    backend: str
    host_type: str
    memory_mib: int
    phase_timeout: float
    root: Path
    cases: dict[str, CaseDefinition]
    oracles: OracleSession

    def _oracle_dispatch_block_reason(
        self,
        sample: Mapping[str, object],
    ) -> str | None:
        if (
            sample.get("host_canary_intact") is not True
            or sample.get("network_canary_connections") != 0
            or self.oracles.watchdog.canary_intact is not True
            or self.oracles.network_canary.connections != 0
        ):
            return "oracle-policy-violation"
        if (
            self.oracles.watchdog.error is not None
            or self.oracles.network_canary.error is not None
        ):
            return "oracle-unavailable"
        return None

    def run_case(
        self,
        case: CaseDefinition,
        *,
        sequence: int,
        timeout: float,
    ) -> dict[str, object]:
        case_root = self.root / "cases" / f"{sequence:03d}-{case.case_id}"
        case_root.mkdir(parents=True, exist_ok=False)
        started = time.monotonic()
        aggregate_returncode: int | None = 0
        aggregate_timed_out = False
        aggregate_teardown = True
        aggregate_stdout_truncated = False
        aggregate_stderr_truncated = False
        last_result: BoundedProcessResult | None = None
        remaining = timeout
        for repetition in range(1, case.repetitions + 1):
            if remaining <= 0:
                aggregate_timed_out = True
                aggregate_returncode = None
                break
            repetition_root = case_root / f"run-{repetition:02d}"
            resources_before = self.oracles.watchdog.sample(
                event=f"case-{sequence}-run-{repetition}-start"
            )
            dispatch_block_reason = self._oracle_dispatch_block_reason(resources_before)
            if dispatch_block_reason is not None:
                aggregate_returncode = None
                write_json(
                    case_root / "dispatch-blocked.json",
                    {
                        "schema_version": SCHEMA_VERSION,
                        "reason": dispatch_block_reason,
                        "resources": resources_before,
                    },
                )
                break
            command = [
                sys.executable,
                str(REPO_ROOT / "scripts" / "nvx.py"),
                "test-microvm",
                "--backend",
                self.backend,
                "--scenario",
                case.scenario,
                "--memory-mib",
                str(self.memory_mib),
                "--timeout",
                str(self.phase_timeout),
                "--output-dir",
                str(repetition_root / "microvm"),
            ]
            process_timeout = min(
                remaining,
                self.phase_timeout * case.timeout_multiplier,
            )
            pid_journal = repetition_root / "openvmm-pids.jsonl"
            environment = sanitized_environment()
            environment["NVX_ADVERSARIAL_OPENVMM_PID_JOURNAL"] = str(pid_journal)
            result = run_bounded_process(
                command,
                cwd=REPO_ROOT,
                output_dir=repetition_root / "process",
                timeout=process_timeout,
                environment=environment,
                contained_by_parent=True,
            )
            openvmm_pids = _read_openvmm_pids(pid_journal)
            live_openvmm_pids = _live_openvmm_pids(openvmm_pids)
            resources_after = self.oracles.watchdog.sample(
                event=f"case-{sequence}-run-{repetition}-stop"
            )
            write_process_metadata(
                repetition_root / "process.json",
                command_name=f"test-microvm:{case.scenario}",
                result=result,
                resources_before=resources_before,
                resources_after=resources_after,
                openvmm_pids=openvmm_pids,
                live_openvmm_pids=live_openvmm_pids,
            )
            last_result = result
            aggregate_returncode = result.returncode
            aggregate_timed_out = aggregate_timed_out or result.timed_out
            aggregate_teardown = aggregate_teardown and result.teardown_complete
            aggregate_teardown = (
                aggregate_teardown and bool(openvmm_pids) and not live_openvmm_pids
            )
            aggregate_stdout_truncated = (
                aggregate_stdout_truncated or result.stdout_truncated
            )
            aggregate_stderr_truncated = (
                aggregate_stderr_truncated or result.stderr_truncated
            )
            if (
                result.returncode != 0
                or result.timed_out
                or not result.teardown_complete
                or not self.oracles.watchdog.canary_intact
                or self.oracles.network_canary.connections != 0
                or self.oracles.watchdog.error is not None
                or self.oracles.network_canary.error is not None
            ):
                break
            remaining = timeout - (time.monotonic() - started)
        reports, reports_complete = _inventory_outcomes(
            case_root,
            required_names=_REQUIRED_OUTCOME_REPORTS.get(
                case.scenario,
                frozenset(),
            ),
        )
        write_json(
            case_root / "outcome-inventory.json",
            {
                "schema_version": SCHEMA_VERSION,
                "reports": reports,
            },
        )
        host_canary_intact = self.oracles.watchdog.canary_intact
        network_connections = self.oracles.network_canary.connections
        aggregate_teardown = aggregate_teardown and reports_complete
        if (
            self.oracles.watchdog.error is not None
            or self.oracles.network_canary.error is not None
        ):
            aggregate_teardown = False
        if not host_canary_intact or network_connections:
            category = "policy-violation"
        elif aggregate_timed_out:
            category = "timeout"
        elif not aggregate_teardown:
            category = "availability-failure"
        elif aggregate_returncode != 0:
            category = "test-failure"
        else:
            category = "passed"
        duration_ms = round((time.monotonic() - started) * 1000)
        return {
            "schema_version": SCHEMA_VERSION,
            "operation": "execute",
            "sequence": sequence,
            "case_id": case.case_id,
            "category": category,
            "returncode": aggregate_returncode,
            "timed_out": aggregate_timed_out,
            "duration_ms": duration_ms,
            "stdout_b64": "" if last_result is None else last_result.encoded_stdout(),
            "stderr_b64": "" if last_result is None else last_result.encoded_stderr(),
            "stdout_truncated": aggregate_stdout_truncated,
            "stderr_truncated": aggregate_stderr_truncated,
            "host_canary_intact": host_canary_intact,
            "network_canary_connections": network_connections,
            "teardown_complete": aggregate_teardown,
            "artifact_path": str(case_root),
            "error": (self.oracles.watchdog.error or self.oracles.network_canary.error),
        }

    def run_canary_boot(self, *, name: str, timeout: float) -> dict[str, object]:
        root = self.root / name
        started = time.monotonic()
        resources_before = self.oracles.watchdog.sample(event=f"{name}-start")
        dispatch_block_reason = self._oracle_dispatch_block_reason(resources_before)
        if dispatch_block_reason is not None:
            write_json(
                root / "dispatch-blocked.json",
                {
                    "schema_version": SCHEMA_VERSION,
                    "reason": dispatch_block_reason,
                    "resources": resources_before,
                },
            )
            return {
                "category": "availability-failure",
                "returncode": None,
                "timed_out": False,
                "duration_ms": round((time.monotonic() - started) * 1000),
                "teardown_complete": True,
                "openvmm_pids": [],
                "live_openvmm_pids": [],
                "artifact_path": str(root),
            }
        command = [
            sys.executable,
            str(REPO_ROOT / "scripts" / "nvx.py"),
            "test-microvm",
            "--backend",
            self.backend,
            "--scenario",
            "lifecycle",
            "--processors",
            "1",
            "--memory-mib",
            str(self.memory_mib),
            "--timeout",
            str(self.phase_timeout),
            "--output-dir",
            str(root / "microvm"),
        ]
        pid_journal = root / "openvmm-pids.jsonl"
        environment = sanitized_environment()
        environment["NVX_ADVERSARIAL_OPENVMM_PID_JOURNAL"] = str(pid_journal)
        result = run_bounded_process(
            command,
            cwd=REPO_ROOT,
            output_dir=root / "process",
            timeout=timeout,
            environment=environment,
            contained_by_parent=True,
        )
        openvmm_pids = _read_openvmm_pids(pid_journal)
        live_openvmm_pids = _live_openvmm_pids(openvmm_pids)
        resources_after = self.oracles.watchdog.sample(event=f"{name}-stop")
        write_process_metadata(
            root / "process.json",
            command_name="test-microvm:lifecycle",
            result=result,
            resources_before=resources_before,
            resources_after=resources_after,
            openvmm_pids=openvmm_pids,
            live_openvmm_pids=live_openvmm_pids,
        )
        teardown_complete = (
            result.teardown_complete and bool(openvmm_pids) and not live_openvmm_pids
        )
        category = _category(result)
        if category == "passed" and not teardown_complete:
            category = "availability-failure"
        return {
            "category": category,
            "returncode": result.returncode,
            "timed_out": result.timed_out,
            "duration_ms": result.duration_ms,
            "teardown_complete": teardown_complete,
            "openvmm_pids": list(openvmm_pids),
            "live_openvmm_pids": list(live_openvmm_pids),
            "artifact_path": str(root),
        }


class AdversarialExecutor:
    """Stateful stdio protocol endpoint intended for a forced launcher."""

    def __init__(self, state_root: Path) -> None:
        self._state_root = state_root.resolve()
        self._state_root.mkdir(parents=True, exist_ok=True)
        self._session: _InitializedSession | None = None
        self._quarantined = False

    def handle(self, request: Mapping[str, object]) -> dict[str, object]:
        operation = request.get("operation")
        if self._quarantined and operation != "shutdown":
            raise ScriptError("executor is quarantined and cannot be reused")
        if operation == "initialize":
            return self._initialize(request)
        if operation == "execute":
            return self._execute(request)
        if operation == "finalize":
            return self._finalize(request)
        if operation == "shutdown":
            return self._shutdown(request)
        raise ScriptError(f"unsupported executor operation: {operation!r}")

    def _initialize(self, request: Mapping[str, object]) -> dict[str, object]:
        if self._session is not None:
            raise ScriptError("executor is already initialized")
        _exact_keys(
            request,
            {
                "schema_version",
                "operation",
                "run_id",
                "expected_commit",
                "expected_openvmm_commit",
                "expected_catalog_sha256",
                "require_clean",
                "backend",
                "campaign",
                "host_type",
                "seed",
                "memory_mib",
                "phase_timeout_seconds",
                "baseline_timeout_seconds",
            },
            description="initialize request",
        )
        _integer(
            request["schema_version"],
            description="initialize request schema_version",
            minimum=SCHEMA_VERSION,
            maximum=SCHEMA_VERSION,
        )
        run_id = _string(
            request["run_id"],
            description="run_id",
            maximum=96,
        )
        if _RUN_ID.fullmatch(run_id) is None:
            raise ScriptError("run_id must be lowercase alphanumeric kebab-case")
        expected_commit = _string(
            request["expected_commit"],
            description="expected_commit",
            maximum=64,
        )
        expected_openvmm_commit = _string(
            request["expected_openvmm_commit"],
            description="expected_openvmm_commit",
            maximum=64,
        )
        expected_catalog_sha256 = _string(
            request["expected_catalog_sha256"],
            description="expected_catalog_sha256",
            maximum=64,
        )
        require_clean = request["require_clean"]
        if not isinstance(require_clean, bool):
            raise ScriptError("require_clean must be a boolean")
        backend = _string(
            request["backend"],
            description="backend",
            maximum=8,
        )
        if backend not in _BACKENDS:
            raise ScriptError(f"unsupported backend: {backend}")
        if (os.name == "nt" and backend != "whp") or (
            os.name != "nt" and backend == "whp"
        ):
            raise ScriptError(f"backend {backend} is unsupported on {os.name}")
        campaign = _string(
            request["campaign"],
            description="campaign",
            maximum=64,
        )
        host_type = _string(
            request["host_type"],
            description="host_type",
            maximum=32,
        )
        if host_type not in _HOST_TYPES:
            raise ScriptError(f"unsupported host_type: {host_type}")
        seed = _integer(
            request["seed"],
            description="seed",
            minimum=0,
            maximum=2**63 - 1,
        )
        memory_mib = _integer(
            request["memory_mib"],
            description="memory_mib",
            minimum=128,
            maximum=4096,
        )
        phase_timeout = _number(
            request["phase_timeout_seconds"],
            description="phase_timeout_seconds",
            minimum=1.0,
            maximum=3600.0,
        )
        baseline_timeout = _number(
            request["baseline_timeout_seconds"],
            description="baseline_timeout_seconds",
            minimum=1.0,
            maximum=7200.0,
        )
        metadata = _target_metadata()
        if metadata["nvx_commit"] != expected_commit:
            raise ScriptError(
                "executor NVX commit does not match the controller commit"
            )
        if metadata["openvmm_commit"] != expected_openvmm_commit:
            raise ScriptError(
                "executor OpenVMM commit does not match the controller commit"
            )
        if require_clean and (
            metadata["nvx_dirty"] is not False or metadata["openvmm_dirty"] is not False
        ):
            raise ScriptError("external executor source trees are not clean")
        cases = load_campaign(campaign)
        if catalog_digest(cases.values()) != expected_catalog_sha256:
            raise ScriptError("executor campaign catalog does not match the controller")
        root = self._state_root / run_id
        oracles = OracleSession.create(root / "oracles", seed=seed, run_id=run_id)
        session = _InitializedSession(
            run_id=run_id,
            backend=backend,
            host_type=host_type,
            memory_mib=memory_mib,
            phase_timeout=phase_timeout,
            root=root,
            cases=cases,
            oracles=oracles,
        )
        self._session = session
        try:
            verification = run_bounded_process(
                [
                    sys.executable,
                    str(REPO_ROOT / "scripts" / "nvx.py"),
                    "verify",
                ],
                cwd=REPO_ROOT,
                output_dir=root / "verify",
                timeout=min(120.0, baseline_timeout),
                environment=sanitized_environment(),
                contained_by_parent=True,
            )
            if (
                verification.returncode != 0
                or verification.timed_out
                or not verification.teardown_complete
            ):
                raise ScriptError(
                    f"NVX source verification failed; logs: {root / 'verify'}"
                )
            artifacts = _artifact_metadata()
            baseline = session.run_canary_boot(
                name="baseline",
                timeout=baseline_timeout,
            )
        except (
            OSError,
            ScriptError,
            subprocess.SubprocessError,
            ValueError,
        ) as error:
            self._session = None
            try:
                session.oracles.close()
            except ScriptError as cleanup_error:
                raise ScriptError(
                    f"{error}; oracle cleanup also failed: {cleanup_error}"
                ) from error
            raise
        passed = (
            verification.returncode == 0
            and not verification.timed_out
            and verification.teardown_complete
            and baseline["category"] == "passed"
            and session.oracles.watchdog.canary_intact
            and session.oracles.network_canary.connections == 0
        )
        response: dict[str, object] = {
            "schema_version": SCHEMA_VERSION,
            "operation": "initialize",
            "status": "passed" if passed else "failed",
            "run_id": run_id,
            "backend": backend,
            "host_type": host_type,
            "metadata": metadata,
            "artifacts": artifacts,
            "verify": {
                "returncode": verification.returncode,
                "timed_out": verification.timed_out,
                "duration_ms": verification.duration_ms,
            },
            "baseline": baseline,
            "oracles": {
                "host_canary_sha256": session.oracles.canary_hash,
                "network_canary_port": session.oracles.network_canary.port,
                "host_canary_intact": session.oracles.watchdog.canary_intact,
                "network_canary_connections": (
                    session.oracles.network_canary.connections
                ),
            },
            "artifact_root": str(root),
        }
        write_json(root / "initialize.json", response)
        return response

    def _execute(self, request: Mapping[str, object]) -> dict[str, object]:
        session = self._require_session()
        _exact_keys(
            request,
            {
                "schema_version",
                "operation",
                "run_id",
                "sequence",
                "case_id",
                "timeout_seconds",
            },
            description="execute request",
        )
        self._check_common_request(request, session)
        sequence = _integer(
            request["sequence"],
            description="sequence",
            minimum=1,
            maximum=100,
        )
        case_id = _string(
            request["case_id"],
            description="case_id",
            maximum=64,
        )
        case = session.cases.get(case_id)
        if case is None:
            raise ScriptError(f"executor rejected unknown case {case_id}")
        timeout = _number(
            request["timeout_seconds"],
            description="timeout_seconds",
            minimum=1.0,
            maximum=7200.0,
        )
        return session.run_case(case, sequence=sequence, timeout=timeout)

    def _finalize(self, request: Mapping[str, object]) -> dict[str, object]:
        session = self._require_session()
        _exact_keys(
            request,
            {
                "schema_version",
                "operation",
                "run_id",
                "timeout_seconds",
                "quarantine",
            },
            description="finalize request",
        )
        self._check_common_request(request, session)
        quarantine_value = request["quarantine"]
        if not isinstance(quarantine_value, bool):
            raise ScriptError("finalize quarantine must be a boolean")
        timeout = _number(
            request["timeout_seconds"],
            description="timeout_seconds",
            minimum=1.0,
            maximum=7200.0,
        )
        if quarantine_value:
            oracle_result = session.oracles.close()
            quarantine_response: dict[str, object] = {
                "schema_version": SCHEMA_VERSION,
                "operation": "finalize",
                "status": "quarantined",
                "run_id": session.run_id,
                "post_campaign": None,
                "oracles": oracle_result,
                "artifact_root": str(session.root),
            }
            write_json(session.root / "finalize.json", quarantine_response)
            self._session = None
            self._quarantined = True
            return quarantine_response
        try:
            post_campaign = session.run_canary_boot(
                name="post-campaign",
                timeout=timeout,
            )
        except (
            OSError,
            ScriptError,
            subprocess.SubprocessError,
            ValueError,
        ) as error:
            self._session = None
            try:
                session.oracles.close()
            except ScriptError as cleanup_error:
                raise ScriptError(
                    f"{error}; oracle cleanup also failed: {cleanup_error}"
                ) from error
            raise
        oracle_result = session.oracles.close()
        canary_violation = (
            oracle_result["host_canary_intact"] is False
            or oracle_result["network_canary_connections"] != 0
        )
        if canary_violation:
            status = "quarantined"
            self._quarantined = True
        elif (
            post_campaign["category"] == "passed"
            and oracle_result["watchdog_error"] is None
            and oracle_result["network_canary_error"] is None
        ):
            status = "passed"
        else:
            status = "failed"
        response: dict[str, object] = {
            "schema_version": SCHEMA_VERSION,
            "operation": "finalize",
            "status": status,
            "run_id": session.run_id,
            "post_campaign": post_campaign,
            "oracles": oracle_result,
            "artifact_root": str(session.root),
        }
        write_json(session.root / "finalize.json", response)
        self._session = None
        return response

    def _shutdown(self, request: Mapping[str, object]) -> dict[str, object]:
        _exact_keys(
            request,
            {"schema_version", "operation"},
            description="shutdown request",
        )
        _integer(
            request["schema_version"],
            description="shutdown request schema_version",
            minimum=SCHEMA_VERSION,
            maximum=SCHEMA_VERSION,
        )
        if self._session is not None:
            session = self._session
            self._session = None
            session.oracles.close()
        return {
            "schema_version": SCHEMA_VERSION,
            "operation": "shutdown",
            "status": "stopped",
        }

    def _require_session(self) -> _InitializedSession:
        if self._session is None:
            raise ScriptError("executor is not initialized")
        return self._session

    @staticmethod
    def _check_common_request(
        request: Mapping[str, object],
        session: _InitializedSession,
    ) -> None:
        _integer(
            request["schema_version"],
            description="request schema_version",
            minimum=SCHEMA_VERSION,
            maximum=SCHEMA_VERSION,
        )
        if request["run_id"] != session.run_id:
            raise ScriptError("request run_id does not match the active run")


def _response_error(operation: object, error: Exception) -> dict[str, object]:
    return {
        "schema_version": SCHEMA_VERSION,
        "operation": "error",
        "request_operation": str(operation),
        "error_type": type(error).__name__,
        "error": str(error),
    }


def _write_response(response: Mapping[str, object]) -> None:
    encoded = json.dumps(
        response,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    if len(encoded) > MAX_PROTOCOL_LINE_BYTES:
        raise ScriptError("executor response exceeds the protocol limit")
    sys.stdout.buffer.write(encoded)
    sys.stdout.buffer.write(b"\n")
    sys.stdout.buffer.flush()


def serve(state_root: Path | None = None) -> int:
    configured_root = state_root
    if configured_root is None:
        value = os.environ.get("NVX_ADVERSARIAL_STATE_ROOT")
        configured_root = (
            Path(value)
            if value
            else BUILD_DIR / "test-results" / "adversarial-executor"
        )
    executor = AdversarialExecutor(configured_root)
    while line := sys.stdin.buffer.readline(MAX_PROTOCOL_LINE_BYTES + 1):
        if len(line) > MAX_PROTOCOL_LINE_BYTES:
            _write_response(
                _response_error(
                    "unknown", ScriptError("request exceeds protocol limit")
                )
            )
            return 1
        request: dict[str, object] | None = None
        try:
            request = parse_json_object(
                line.decode("utf-8"),
                description="executor request",
            )
            response = executor.handle(request)
        except (
            OSError,
            ScriptError,
            subprocess.SubprocessError,
            UnicodeDecodeError,
            ValueError,
        ) as error:
            operation = request.get("operation") if request is not None else "unknown"
            _write_response(_response_error(operation, error))
            if operation in {"initialize", "finalize", "shutdown"}:
                return 1
            continue
        _write_response(response)
        if response.get("status") == "quarantined":
            return 0
        if response.get("operation") == "shutdown":
            return 0
    return 0
