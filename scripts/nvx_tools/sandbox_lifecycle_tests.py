"""Public CLI acceptance for the managed sandbox lifecycle and security profile.

The scenario runs `nvx-rs sandbox` as subprocesses against the real kernel, the
Alpine control initramfs, the Ubuntu EROFS layer, and ext4 scratch copies. It
checks identity admission, fail-closed lifecycle transitions, that overlapping
starts launch one VM, warm-guest state across exec requests and restarts, the
guest security profile and resource limits, bounded output and outcome reports,
that the CLI of artifacts of another runtime identity drives no VM that these
started and its stop terminates OpenVMM, and that stop, a crash, and
deprovision leave no OpenVMM process, control endpoint, capability, or runtime
record behind while the supplied layer and scratch artifacts stay intact.
"""

from __future__ import annotations

import concurrent.futures
import contextlib
import hashlib
import json
import os
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, cast

from . import nvx_rs
from .build_constants import (
    BuildConstants,
    InitramfsBuildConstants,
    KernelBuildConstants,
    OpenVMMBuildConstants,
)
from .common import sha256_file
from .managed_exec_tests import (
    WorkloadArtifacts,
    bounded_text,
    evidence_argv,
    format_errors,
    load_workload_artifacts,
    read_exec_outcome,
)

PROBE = "/sbin/nvx-sandbox-smoke"
HOSTNAME = "nvx-lifecycle"
MEMORY_MIB = 256
MEMORY_MAX = 32 * 1024 * 1024
PIDS_MAX = 8
# An identity that the Ubuntu layer's user database lacks.
UNAVAILABLE_IDENTITY = "4242:4242"
# The guest agent forwards at most this much output for one exec request, and
# reads it in chunks of at most OUTPUT_CHUNK_BYTES. It drops the chunk that
# would exceed the bound, so more than OUTPUT_LIMIT_BYTES - OUTPUT_CHUNK_BYTES
# bytes reach the host.
OUTPUT_LIMIT_BYTES = 1024 * 1024
OUTPUT_CHUNK_BYTES = 32 * 1024
STATE_PATH = "/tmp/nvx lifecycle state"
STATE_ARGUMENTS = ("  leading and trailing  ", "tab\tseparated", "line one\nline two")
STATE_CONTENT = "".join(f"{argument}\n" for argument in STATE_ARGUMENTS).encode()
REFUSED_MARKER = "/tmp/nvx-refused-request"
FOREIGN_NAME = "operator-note.txt"
FOREIGN_CONTENT = "operator data\n"
EVIDENCE_PREFIX = "sandbox-lifecycle-"
EVIDENCE_LIMIT = 64 * 1024
# ext4 sets this incompatible feature while its journal needs recovery.
EXT4_INCOMPAT_RECOVER = 0x4
MANAGED_SUCCESS = {"operation": "managed", "category": "success", "status_code": 0}
# The guest powers off with status 125 when it refuses the workload identity,
# and a managed start reports the status from OpenVMM's outcome report.
REFUSAL_CATEGORY = "guest-exit"
MANAGED_REFUSAL = {
    "operation": "managed",
    "category": REFUSAL_CATEGORY,
    "status_code": 125,
}


@dataclass(frozen=True)
class Refusal:
    """An error that a public operation must report: the stable code of
    nvx-rs's error, and stable fragments of its message, such as the names of
    options, files, and parts, which the wording around them may change."""

    code: str
    fragments: tuple[str, ...] = ()

    def matches(self, stderr: bytes) -> bool:
        reported = nvx_rs.reported_error(stderr)
        return (
            reported is not None
            and reported.code == self.code
            and all(fragment in reported.message for fragment in self.fragments)
        )

    def __str__(self) -> str:
        named = "".join(f" naming {fragment!r}" for fragment in self.fragments)
        return f"error {self.code!r}{named}"


# A start that OpenVMM ended names the category of its outcome report.
REFUSED_START = Refusal("exited", (REFUSAL_CATEGORY,))

# The files of a state directory, as nvx-rs sandbox documents them.
CONFIG = "config.json"
RUNTIME = "runtime.json"
CAPABILITY = "control.capability"
LOG = "openvmm.log"
SOCKET = "control.sock"
OUTCOME = "outcome.json"
LOCK = "sandbox.lock"
# The fields of the store's runtime record.
RUNTIME_FIELDS = frozenset({"format", "launch", "process", "endpoint", "runtime"})
PROVISIONED_ENTRIES = frozenset({CONFIG, LOCK})
# Windows hosts serve the control endpoint as a named pipe outside the state.
RUNNING_ENTRIES = frozenset(
    {CONFIG, LOCK, RUNTIME, CAPABILITY, LOG}
    if os.name == "nt"
    else {CONFIG, LOCK, RUNTIME, CAPABILITY, LOG, SOCKET}
)
STOPPED_ENTRIES = frozenset({CONFIG, LOCK, LOG, OUTCOME})
# OpenVMM that exits without a stop publishes no outcome report, and the
# transition that finds it exited clears its runtime record.
CRASHED_ENTRIES = frozenset({CONFIG, LOCK, LOG})
ALREADY_RUNNING = Refusal("sandbox-running")
STALE = Refusal("sandbox-exited")
NOT_RUNNING = Refusal("sandbox-not-running")
NOT_PROVISIONED = Refusal("sandbox-not-provisioned")
# nvx-rs reads the runtime identity of a checkout from the build provenance of
# its OpenVMM, kernel, and initramfs alone, without hashing an artifact.
PROVENANCE_NAMES = (
    OpenVMMBuildConstants.PROVENANCE_NAME,
    KernelBuildConstants.PROVENANCE_NAME,
    InitramfsBuildConstants.PROVENANCE_NAME,
)
INITRAMFS_DIGEST = "initramfs_sha256"


def other_runtime_refusal(pid: int) -> Refusal:
    """Returns the error of a start or exec of a VM that the artifacts of another
    initramfs started, whose OpenVMM process is pid."""
    return Refusal("runtime-mismatch", ("initramfs", str(pid)))


def is_other_runtime_termination(stderr: bytes, pid: int) -> bool:
    """Returns whether `stderr` is only the warning of a stop that terminated
    the OpenVMM process pid of a VM that the artifacts of another initramfs
    started, which names the part that differs and the process."""
    text = stderr.replace(b"\r\n", b"\n").decode("utf-8", errors="replace")
    return (
        text.startswith("warning: ")
        and text.count("\n") == 1
        and text.endswith("\n")
        and "initramfs" in text
        and str(pid) in text
    )


def read_build_provenance() -> dict[str, dict[str, Any]]:
    """Returns the build provenance from which nvx-rs reads the runtime identity
    of the checkout, by file name."""
    provenance: dict[str, dict[str, Any]] = {}
    for name in PROVENANCE_NAMES:
        path = BuildConstants.BUILD_DIR / name
        try:
            value: object = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
            raise RuntimeError(f"build provenance is unreadable: {path}") from error
        if not isinstance(value, dict):
            raise RuntimeError(f"build provenance is not an object: {path}")
        provenance[name] = cast(dict[str, Any], value)
    return provenance


def openvmm_processes(marker: str) -> tuple[int, ...]:
    """Returns the IDs of the OpenVMM processes whose arguments contain marker."""
    processes: list[int] = []
    if sys.platform == "win32":
        script = (
            "Get-CimInstance Win32_Process -Filter \"Name = 'openvmm.exe'\" | "
            "ForEach-Object { [string]$_.ProcessId + [char]9 + "
            "[string]$_.CommandLine }"
        )
        result = subprocess.run(
            ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", script],
            capture_output=True,
            timeout=120,
            check=False,
        )
        if result.returncode != 0:
            raise RuntimeError(
                f"cannot list OpenVMM processes: {bounded_text(result.stderr)}"
            )
        for line in result.stdout.decode(errors="replace").splitlines():
            pid, _, command_line = line.partition("\t")
            if marker in command_line:
                processes.append(int(pid))
        return tuple(sorted(processes))
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            arguments = (entry / "cmdline").read_bytes().split(b"\0")
        except OSError:
            continue
        if Path(os.fsdecode(arguments[0])).name == "openvmm" and any(
            marker.encode() in argument for argument in arguments
        ):
            processes.append(int(entry.name))
    return tuple(sorted(processes))


def recorded_pid(runtime: dict[str, Any]) -> int:
    """Returns the ID of the OpenVMM process that a runtime record names."""
    return int(runtime["process"]["pid"])


def openvmm_running(runtime: dict[str, Any], marker: str) -> bool:
    """Returns whether the OpenVMM process of a runtime record still runs: an
    OpenVMM process with its ID whose arguments contain marker, the name of
    the fixture, which no other process names."""
    return recorded_pid(runtime) in openvmm_processes(marker)


def control_endpoint_present(runtime: dict[str, Any]) -> bool:
    endpoint = str(runtime["endpoint"])
    if sys.platform == "win32":
        return endpoint.rsplit("/", 1)[-1] in os.listdir("//./pipe/")
    return os.path.lexists(endpoint)


def terminate_process(pid: int) -> None:
    if sys.platform == "win32":
        os.kill(pid, signal.SIGTERM)
    else:
        os.kill(pid, signal.SIGKILL)


def openvmm_threads_remain(runtime: dict[str, Any]) -> bool:
    """Returns whether a thread of the OpenVMM process of a runtime record
    remains on Linux, where nvx-rs counts a process as running until its last
    thread exits: a killed process's main thread can be a zombie, whose command
    line is empty, while its other threads still exit."""
    if sys.platform == "win32":
        return False
    pid = recorded_pid(runtime)
    try:
        stat = Path(f"/proc/{pid}/stat").read_bytes()
    except (FileNotFoundError, ProcessLookupError):
        return False
    fields = stat[stat.rindex(b")") + 1 :].split()
    if int(fields[19]) != int(runtime["process"]["startTime"]):
        # The ID names another process.
        return False
    if fields[0] == b"X":
        return False
    if fields[0] != b"Z":
        return True
    try:
        return len(os.listdir(f"/proc/{pid}/task")) > 1
    except (FileNotFoundError, ProcessLookupError):
        return False


def kill_openvmm(runtime: dict[str, Any], timeout: float, marker: str) -> None:
    """Ends the OpenVMM process of a runtime record, as a crash would, and waits
    until it has exited as nvx-rs tells; marker names the fixture, as for
    openvmm_running."""
    if openvmm_running(runtime, marker):
        try:
            terminate_process(recorded_pid(runtime))
        except OSError:
            # The process may have exited since the check.
            if openvmm_running(runtime, marker):
                raise
    deadline = time.monotonic() + timeout
    while openvmm_running(runtime, marker) or openvmm_threads_remain(runtime):
        if time.monotonic() >= deadline:
            raise RuntimeError(
                f"OpenVMM process {recorded_pid(runtime)} survived its kill"
            )
        time.sleep(0.05)


def scratch_journal_clean(path: Path) -> bool:
    with path.open("rb") as image:
        image.seek(1024 + 0x60)
        incompatible = image.read(4)
    if len(incompatible) != 4:
        raise RuntimeError(f"scratch superblock is truncated: {path}")
    return not struct.unpack("<I", incompatible)[0] & EXT4_INCOMPAT_RECOVER


def state_entries(state: Path) -> list[str] | None:
    if not state.is_dir():
        return None
    return sorted(path.name for path in state.iterdir())


def managed_outcome(state: Path) -> object:
    """Returns the outcome section of the OpenVMM report in a state directory."""
    return managed_report(state).get("outcome")


def managed_report(state: Path) -> dict[str, object]:
    """Returns the OpenVMM report in a state directory, or an empty mapping if
    it is not an object."""
    report: object = json.loads((state / OUTCOME).read_text(encoding="utf-8"))
    if not isinstance(report, dict):
        return {}
    return cast(dict[str, object], report)


def check_managed_report(
    report: dict[str, object],
    backend: str,
    network_policy: dict[str, object] | None = None,
) -> None:
    """Checks the report of a managed sandbox that stopped cleanly.

    These are the report checks of the direct-mode managed lifecycle
    (nvx-rs/tests/managed_lifecycle.rs) that hold for the layered root too: the
    report names the backend, reports `network_policy`, or that no network
    policy was requested when that is None, and every teardown step completed.
    """
    outcome = report.get("outcome")
    if outcome != MANAGED_SUCCESS:
        raise RuntimeError(f"the managed outcome is {outcome!r}")
    if report.get("backend") != backend:
        raise RuntimeError(
            f"the managed report names backend {report.get('backend')!r}"
        )
    policy = report.get("network_policy")
    if network_policy is not None:
        if policy != network_policy:
            raise RuntimeError(f"the managed report has network policy {policy!r}")
    else:
        status = (
            cast(dict[str, object], policy).get("status")
            if isinstance(policy, dict)
            else None
        )
        if status != "not-requested":
            raise RuntimeError(
                f"the managed report has network policy status {status!r}"
            )
    # microvm_tests, which runs this scenario, imports this module.
    from .microvm_tests import OUTCOME_TEARDOWN_FIELDS

    teardown = report.get("teardown")
    if (
        not isinstance(teardown, dict)
        or set(cast(dict[str, object], teardown)) != set(OUTCOME_TEARDOWN_FIELDS)
        or not all(
            value is True for value in cast(dict[str, object], teardown).values()
        )
    ):
        raise RuntimeError(f"the managed report has incomplete teardown: {teardown!r}")


def bounded_evidence(data: bytes, omitted: int = 0) -> bytes:
    """Keeps the end of data, where a failure shows, within EVIDENCE_LIMIT.

    `omitted` counts bytes that preceded data in its source.
    """
    omitted += max(0, len(data) - EVIDENCE_LIMIT)
    data = data[-EVIDENCE_LIMIT:]
    if omitted:
        return f"... ({omitted} earlier bytes omitted)\n".encode() + data
    return data


def read_runtime(state: Path) -> dict[str, Any]:
    path = state / RUNTIME
    try:
        value: object = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RuntimeError(f"sandbox runtime record is unreadable: {path}") from error
    if not isinstance(value, dict):
        raise RuntimeError(f"sandbox runtime record is not an object: {path}")
    runtime = cast(dict[str, Any], value)
    if frozenset(runtime) != RUNTIME_FIELDS:
        raise RuntimeError(f"sandbox runtime record has unexpected fields: {path}")
    return runtime


class LifecycleAcceptance:
    # The prefix of the evidence files in the output directory.
    evidence_prefix = EVIDENCE_PREFIX

    def __init__(
        self,
        backend: str,
        *,
        timeout: float,
        output_dir: Path,
        artifacts: WorkloadArtifacts,
        root: Path,
    ) -> None:
        self.backend = backend
        self.timeout = timeout
        self.output_dir = output_dir
        self.artifacts = artifacts
        self.root = root
        self.state = root / "state"
        self.identity_state = root / "identity-state"
        self.scratch = root / "scratch.ext4"
        # An artifact root whose runtime identity differs in the initramfs.
        self.other_root = root / "other-runtime"
        self.checks: list[dict[str, object]] = []

    def launch_options(self, scratch: Path) -> list[str]:
        return [
            "--hypervisor",
            self.backend,
            "--layer",
            f"distro,{self.artifacts.distro},{self.artifacts.layer_uuid}",
            "--scratch",
            str(scratch),
            "--memory-mib",
            str(MEMORY_MIB),
        ]

    def run_cli(
        self,
        operation: str,
        *arguments: str,
        state: Path | None,
        root: Path | None = None,
    ) -> subprocess.CompletedProcess[bytes]:
        """Runs one public operation with `nvx-rs sandbox`, under the artifact
        root `root`, or else under this checkout."""
        command = nvx_rs.command_line(("sandbox", operation), root=root)
        if state is not None:
            command.extend(("--state-dir", str(state)))
        command.extend(("--timeout", str(self.timeout), *arguments))
        return subprocess.run(
            command,
            cwd=BuildConstants.REPO_ROOT,
            capture_output=True,
            timeout=2 * self.timeout + 30,
        )

    def record(
        self,
        operation: str,
        result: subprocess.CompletedProcess[bytes],
        *,
        state: Path | None,
        expected: int | None,
    ) -> None:
        reported = nvx_rs.reported_error(result.stderr)
        self.checks.append(
            {
                "operation": operation,
                "argv": evidence_argv([str(value) for value in result.args]),
                "expected_returncode": expected,
                "returncode": result.returncode,
                "error_code": None if reported is None else reported.code,
                "stdout_bytes": len(result.stdout),
                "stderr_bytes": len(result.stderr),
                "state_entries": None if state is None else state_entries(state),
            }
        )

    def invoke(
        self,
        operation: str,
        *arguments: str,
        state: Path | None,
        expected: int | None = 0,
        refusal: Refusal | None = None,
        root: Path | None = None,
    ) -> subprocess.CompletedProcess[bytes]:
        """Runs one public operation and records only its bounded observations.

        It requires status `expected` unless that is None, and the error of
        `refusal` on standard error when given. With `root`, the operation runs
        under that artifact root.
        """
        result = self.run_cli(operation, *arguments, state=state, root=root)
        self.record(operation, result, state=state, expected=expected)
        if (expected is not None and result.returncode != expected) or (
            refusal is not None and not refusal.matches(result.stderr)
        ):
            wanted = "" if refusal is None else f" with {refusal}"
            raise RuntimeError(
                f"public sandbox {operation} returned {result.returncode}, expected "
                f"{expected}{wanted}; stdout={bounded_text(result.stdout)!r}; "
                f"stderr={bounded_text(result.stderr)!r}"
            )
        return result

    def exec(
        self,
        entrypoint: str,
        *arguments: str,
        expected: int | None = 0,
        refusal: Refusal | None = None,
    ) -> subprocess.CompletedProcess[bytes]:
        return self.invoke(
            "exec",
            "--entrypoint",
            entrypoint,
            *arguments,
            state=self.state,
            expected=expected,
            refusal=refusal,
        )

    def require_entries(
        self, state: Path, expected: frozenset[str], description: str
    ) -> None:
        entries = state_entries(state)
        if entries is None or frozenset(entries) != expected:
            raise RuntimeError(
                f"{description}: the state directory holds {entries}, expected "
                f"{sorted(expected)}"
            )

    def require_output(
        self, result: subprocess.CompletedProcess[bytes], stdout: bytes, what: str
    ) -> None:
        if result.returncode != 0 or result.stdout != stdout or result.stderr:
            raise RuntimeError(
                f"{what} returned status {result.returncode}, "
                f"stdout={bounded_text(result.stdout)!r}, "
                f"stderr={bounded_text(result.stderr)!r}"
            )

    def require_processes(self, expected: tuple[int, ...], description: str) -> None:
        processes = openvmm_processes(self.root.name)
        if processes != expected:
            raise RuntimeError(
                f"{description}: OpenVMM processes {list(processes)} run for the "
                f"fixture, expected {list(expected)}"
            )

    def save_evidence(self, name: str, data: bytes, omitted: int = 0) -> None:
        (self.output_dir / f"{self.evidence_prefix}{name}").write_bytes(
            bounded_evidence(data, omitted)
        )

    def copy_evidence(self, source: Path, name: str) -> None:
        if not source.is_file():
            return
        with source.open("rb") as stream:
            size = stream.seek(0, os.SEEK_END)
            stream.seek(max(0, size - EVIDENCE_LIMIT))
            data = stream.read(EVIDENCE_LIMIT)
        self.save_evidence(name, data, size - len(data))

    def write_checks(self) -> None:
        (self.output_dir / f"{self.evidence_prefix}checks.json").write_text(
            json.dumps(self.checks, indent=2) + "\n", encoding="utf-8"
        )

    def run(self) -> None:
        layer_digest = sha256_file(self.artifacts.distro)
        self.check_identity_admission()
        self.check_unprovisioned()
        self.provision()
        runtime = self.start(description="first start")
        self.check_running_transitions(runtime)
        self.check_warm_state()
        self.check_security_profile()
        self.check_bounded_results()
        self.check_deprovision_while_running(runtime)
        self.stop(runtime, layer_digest)
        restarted = self.start_overlapping()
        result = self.exec("/bin/cat", f"--arg={STATE_PATH}", expected=None)
        self.require_output(result, STATE_CONTENT, "the restarted sandbox's state")
        self.check_other_runtime(restarted)
        restarted = self.start(description="start after the termination")
        self.check_crash(restarted)
        self.check_deprovision()
        if not self.scratch.is_file():
            raise RuntimeError("deprovision removed the supplied scratch image")
        if sha256_file(self.artifacts.distro) != layer_digest:
            raise RuntimeError("the sandbox lifecycle modified the EROFS layer")
        self.require_processes((), "after deprovision")

    def check_identity_admission(self) -> None:
        # The CLI rejects a root identity before it creates any state.
        self.invoke(
            "provision",
            *self.launch_options(self.scratch),
            "--workload-user",
            "0:0",
            state=self.state,
            expected=2,
            refusal=Refusal("argument-invalid", ("--workload-user",)),
        )
        if self.state.exists():
            raise RuntimeError("a rejected root identity created sandbox state")

        # The guest refuses an identity that the image lacks before it starts
        # the workload.
        run_scratch = self.root / "run-scratch.ext4"
        shutil.copyfile(self.artifacts.scratch_template, run_scratch)
        result = self.invoke(
            "run",
            *self.launch_options(run_scratch),
            "--workload-user",
            UNAVAILABLE_IDENTITY,
            "--entrypoint",
            PROBE,
            state=None,
            expected=125,
        )
        console = result.stdout + result.stderr
        self.save_evidence("identity-run.log", console)
        if (
            b"configured workload UID is unavailable" not in console
            or b"NVX-UBUNTU-SANDBOX-PROFILE-OK" in console
        ):
            raise RuntimeError("an unavailable one-shot identity was not refused")

        # A managed start with that identity fails closed. OpenVMM exits on its
        # own and publishes the guest's status, which the start reports, and
        # nothing else remains.
        identity_scratch = self.root / "identity-scratch.ext4"
        shutil.copyfile(self.artifacts.scratch_template, identity_scratch)
        self.invoke(
            "provision",
            *self.launch_options(identity_scratch),
            "--workload-user",
            UNAVAILABLE_IDENTITY,
            state=self.identity_state,
        )
        self.invoke(
            "start", state=self.identity_state, expected=1, refusal=REFUSED_START
        )
        self.copy_evidence(self.identity_state / LOG, "identity-openvmm.log")
        self.copy_evidence(self.identity_state / OUTCOME, "identity-outcome.json")
        # OpenVMM publishes its report before it exits, rather than being ended
        # while it writes it, which left only the report's staging file (#438).
        if not (self.identity_state / OUTCOME).is_file():
            raise RuntimeError("the refused managed start published no outcome report")
        self.require_entries(
            self.identity_state, STOPPED_ENTRIES, "after a refused managed start"
        )
        outcome = managed_outcome(self.identity_state)
        if outcome != MANAGED_REFUSAL:
            raise RuntimeError(f"the refused managed start's outcome is {outcome!r}")
        self.require_processes((), "after a refused managed identity")
        self.invoke("deprovision", state=self.identity_state)
        if self.identity_state.exists():
            raise RuntimeError("deprovision kept the refused sandbox's state")

    def check_unprovisioned(self) -> None:
        for operation in ("start", "exec", "stop", "deprovision"):
            self.invoke(
                operation,
                state=self.state,
                expected=1,
                refusal=NOT_PROVISIONED,
            )
            if self.state.exists():
                raise RuntimeError(f"{operation} created unprovisioned sandbox state")

    def provision(self) -> None:
        shutil.copyfile(self.artifacts.scratch_template, self.scratch)
        options = (
            *self.launch_options(self.scratch),
            "--hostname",
            HOSTNAME,
            "--memory-max",
            str(MEMORY_MAX),
            "--pids-max",
            str(PIDS_MAX),
        )
        self.invoke("provision", *options, state=self.state)
        self.require_entries(self.state, PROVISIONED_ENTRIES, "after provision")
        config = (self.state / CONFIG).read_bytes()
        self.invoke(
            "provision",
            *options,
            state=self.state,
            expected=1,
            refusal=Refusal("sandbox-provisioned"),
        )
        if (self.state / CONFIG).read_bytes() != config:
            raise RuntimeError("a repeated provision changed the configuration")
        for operation in ("exec", "stop"):
            self.invoke(
                operation,
                state=self.state,
                expected=1,
                refusal=NOT_RUNNING,
            )
            self.require_entries(
                self.state, PROVISIONED_ENTRIES, f"after {operation} before start"
            )

    def start(self, *, description: str) -> dict[str, Any]:
        self.invoke("start", state=self.state)
        return self.require_started(description)

    def start_overlapping(self) -> dict[str, Any]:
        """Restarts the sandbox with two overlapping public starts.

        Exactly one of them launches OpenVMM. The other waits for its lifecycle
        lock and then fails, leaving its runtime record, capability, and VM.
        """
        barrier = threading.Barrier(2, timeout=self.timeout + 30)

        def start() -> subprocess.CompletedProcess[bytes]:
            barrier.wait()
            return self.run_cli("start", state=self.state)

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            starts = [pool.submit(start) for _ in range(2)]
            results = [future.result() for future in starts]
        for result in results:
            self.record("start", result, state=self.state, expected=None)
        refused = [result for result in results if result.returncode != 0]
        statuses = sorted(result.returncode for result in results)
        if statuses != [0, 1] or not ALREADY_RUNNING.matches(refused[0].stderr):
            raise RuntimeError(
                "overlapping public starts returned "
                + "; ".join(
                    f"status {result.returncode}, "
                    f"stderr={bounded_text(result.stderr)!r}"
                    for result in results
                )
                + f", expected one success and one {ALREADY_RUNNING}"
            )
        return self.require_started("overlapping restart")

    def require_started(self, description: str) -> dict[str, Any]:
        self.require_entries(self.state, RUNNING_ENTRIES, f"after the {description}")
        runtime = read_runtime(self.state)
        self.save_evidence(
            "runtime.json", (json.dumps(runtime, indent=2) + "\n").encode()
        )
        if not openvmm_running(runtime, self.root.name):
            raise RuntimeError(f"OpenVMM is not running after the {description}")
        if not control_endpoint_present(runtime):
            raise RuntimeError(
                f"the control endpoint is absent after the {description}"
            )
        self.require_processes((recorded_pid(runtime),), f"after the {description}")
        return runtime

    def check_running_transitions(self, runtime: dict[str, Any]) -> None:
        def capability_digest() -> bytes:
            return hashlib.sha256((self.state / CAPABILITY).read_bytes()).digest()

        record = (self.state / RUNTIME).read_bytes()
        capability = capability_digest()
        self.invoke(
            "start",
            state=self.state,
            expected=1,
            refusal=ALREADY_RUNNING,
        )
        if (self.state / RUNTIME).read_bytes() != record:
            raise RuntimeError("a repeated start changed the runtime record")
        if capability_digest() != capability:
            raise RuntimeError("a repeated start changed the control capability")
        self.require_entries(self.state, RUNNING_ENTRIES, "after a repeated start")
        self.require_processes((recorded_pid(runtime),), "after a repeated start")

    def check_warm_state(self) -> None:
        # Managed arguments keep their whitespace, and a later request sees the
        # file that an earlier one wrote.
        result = self.exec(
            "/bin/sh",
            "--arg=-c",
            '--arg=printf \'%s\\n\' "$@" >"$0"',
            f"--arg={STATE_PATH}",
            *(f"--arg={argument}" for argument in STATE_ARGUMENTS),
            expected=None,
        )
        self.require_output(result, b"", "the state-writing request")
        result = self.exec("/bin/cat", f"--arg={STATE_PATH}", expected=None)
        self.require_output(result, STATE_CONTENT, "the state-reading request")

    def check_security_profile(self) -> None:
        # The earlier requests already entered their own namespaces, so the
        # probe's check that each container mount appears once also shows
        # that they did not mount into the agent's namespace.
        result = self.exec(
            PROBE,
            "--arg=entry",
            "--arg=limits",
            f"--arg={MEMORY_MAX}",
            f"--arg={PIDS_MAX}",
        )
        self.save_evidence("probe.log", result.stdout + result.stderr)
        for marker in (
            b"NVX-UBUNTU-SANDBOX-PROFILE-OK uid=65534 gid=65534",
            b"NVX-UBUNTU-SANDBOX-ENTRY-OK",
            f"NVX-UBUNTU-SANDBOX-LIMITS-OK memory_max={MEMORY_MAX} "
            f"pids_max={PIDS_MAX}".encode(),
            b"NVX-UBUNTU-SANDBOX-OK uid=65534 gid=65534",
        ):
            if marker not in result.stdout:
                raise RuntimeError(
                    f"the sandbox security probe did not report {marker.decode()}"
                )

    def check_bounded_results(self) -> None:
        # The agent stops forwarding output at its bound, and the outcome
        # report carries only the operation, category, and status.
        report = self.root / "output-limit-outcome.json"
        result = self.exec(
            "/usr/bin/head",
            "--arg=-c",
            f"--arg={2 * OUTPUT_LIMIT_BYTES}",
            "--arg=/dev/zero",
            "--outcome-report",
            str(report),
            expected=125,
        )
        if (
            not OUTPUT_LIMIT_BYTES - OUTPUT_CHUNK_BYTES
            < len(result.stdout)
            <= OUTPUT_LIMIT_BYTES
            or result.stdout.strip(b"\0")
            or result.stderr
        ):
            raise RuntimeError(
                "the output limit forwarded "
                f"{len(result.stdout)} stdout and {len(result.stderr)} stderr bytes"
            )
        read_exec_outcome(report, category="output-limit", status_code=125)
        self.copy_evidence(report, "output-limit-outcome.json")

        # An existing report path fails the request before the workload runs.
        existing = self.root / "existing-outcome.json"
        existing.write_text(FOREIGN_CONTENT, encoding="utf-8")
        self.exec(
            "/bin/touch",
            f"--arg={REFUSED_MARKER}",
            "--outcome-report",
            str(existing),
            expected=1,
            refusal=Refusal("outcome-report-exists", (existing.name,)),
        )
        if existing.read_text(encoding="utf-8") != FOREIGN_CONTENT:
            raise RuntimeError("a refused request overwrote an existing report")
        result = self.exec(
            "/bin/sh", "--arg=-c", f"--arg=test ! -e {REFUSED_MARKER}", expected=None
        )
        self.require_output(result, b"", "the refused request's marker check")

    def check_deprovision_while_running(self, runtime: dict[str, Any]) -> None:
        record = (self.state / RUNTIME).read_bytes()
        self.invoke(
            "deprovision",
            state=self.state,
            expected=1,
            refusal=ALREADY_RUNNING,
        )
        self.require_entries(self.state, RUNNING_ENTRIES, "after deprovision refused")
        if (self.state / RUNTIME).read_bytes() != record:
            raise RuntimeError("a refused deprovision changed the runtime record")
        self.require_processes((recorded_pid(runtime),), "after deprovision refused")
        result = self.exec("/bin/cat", f"--arg={STATE_PATH}", expected=None)
        self.require_output(
            result, STATE_CONTENT, "the state after deprovision refused"
        )

    def stop(self, runtime: dict[str, Any], layer_digest: str) -> None:
        self.invoke("stop", state=self.state)
        self.require_entries(self.state, STOPPED_ENTRIES, "after stop")
        if openvmm_running(runtime, self.root.name):
            raise RuntimeError("OpenVMM still runs after stop")
        if control_endpoint_present(runtime):
            raise RuntimeError("stop left the control endpoint behind")
        self.require_processes((), "after stop")
        self.copy_evidence(self.state / OUTCOME, "outcome.json")
        check_managed_report(managed_report(self.state), self.backend)
        # A clean journal shows that stop unmounted the overlay and scratch
        # before the VM powered off.
        if not scratch_journal_clean(self.scratch):
            raise RuntimeError("stop left the scratch file system mounted")
        if sha256_file(self.artifacts.distro) != layer_digest:
            raise RuntimeError("the running sandbox modified the EROFS layer")
        for operation in ("exec", "stop"):
            self.invoke(
                operation,
                state=self.state,
                expected=1,
                refusal=NOT_RUNNING,
            )
            self.require_entries(
                self.state, STOPPED_ENTRIES, f"after {operation} after stop"
            )

    def write_other_runtime(self) -> None:
        """Writes the checkout's build provenance into the other artifact root,
        with another initramfs digest, so that nvx-rs reads a runtime identity
        there that differs from the checkout's in the initramfs."""
        build = self.other_root / BuildConstants.BUILD_DIRECTORY_NAME
        build.mkdir(parents=True)
        for name, provenance in read_build_provenance().items():
            if name == InitramfsBuildConstants.PROVENANCE_NAME:
                digest = provenance.get(INITRAMFS_DIGEST)
                if not isinstance(digest, str):
                    raise RuntimeError(f"build provenance {name} names no initramfs")
                provenance = {
                    **provenance,
                    INITRAMFS_DIGEST: hashlib.sha256(digest.encode()).hexdigest(),
                }
            (build / name).write_text(json.dumps(provenance), encoding="utf-8")

    def check_other_runtime(self, runtime: dict[str, Any]) -> None:
        # The CLI of other artifacts, as after an upgrade, reattaches to the VM
        # but drives it through no control session, since the VM may speak
        # other contracts: it refuses to start or exec, and its stop terminates
        # OpenVMM without a guest shutdown, so OpenVMM reports no outcome.
        self.write_other_runtime()
        pid = recorded_pid(runtime)
        record = (self.state / RUNTIME).read_bytes()
        for operation, arguments, refusal in (
            ("exec", ("--entrypoint", "/bin/true"), other_runtime_refusal(pid)),
            ("start", (), other_runtime_refusal(pid)),
            ("deprovision", (), ALREADY_RUNNING),
        ):
            self.invoke(
                operation,
                *arguments,
                state=self.state,
                expected=1,
                refusal=refusal,
                root=self.other_root,
            )
        self.require_entries(
            self.state, RUNNING_ENTRIES, "after the refusals of another runtime"
        )
        if (self.state / RUNTIME).read_bytes() != record:
            raise RuntimeError("another runtime changed the runtime record")
        self.require_processes((pid,), "after the refusals of another runtime")
        # The artifacts that started the VM still drive it.
        result = self.exec("/bin/cat", f"--arg={STATE_PATH}", expected=None)
        self.require_output(
            result, STATE_CONTENT, "the state after the refusals of another runtime"
        )

        result = self.invoke("stop", state=self.state, root=self.other_root)
        if result.stdout or not is_other_runtime_termination(result.stderr, pid):
            raise RuntimeError(
                "the stop of another runtime printed "
                f"stdout={bounded_text(result.stdout)!r}, "
                f"stderr={bounded_text(result.stderr)!r}, expected a warning that "
                f"names the initramfs and OpenVMM process {pid}"
            )
        self.require_entries(
            self.state, CRASHED_ENTRIES, "after the stop of another runtime"
        )
        if openvmm_running(runtime, self.root.name):
            raise RuntimeError("OpenVMM still runs after the stop of another runtime")
        if control_endpoint_present(runtime):
            raise RuntimeError(
                "the stop of another runtime left the control endpoint behind"
            )
        self.require_processes((), "after the stop of another runtime")
        self.invoke(
            "stop",
            state=self.state,
            expected=1,
            refusal=NOT_RUNNING,
            root=self.other_root,
        )
        self.require_entries(
            self.state, CRASHED_ENTRIES, "after a repeated stop of another runtime"
        )

    def check_crash(self, runtime: dict[str, Any]) -> None:
        # OpenVMM exits without a stop. The next transition finds that it
        # exited, reports it once, and clears its runtime record.
        kill_openvmm(runtime, self.timeout, self.root.name)
        self.require_processes((), "after OpenVMM was killed")
        if sys.platform == "win32" and control_endpoint_present(runtime):
            raise RuntimeError("the control pipe outlived OpenVMM")
        for operation, refusal in (
            ("exec", STALE),
            ("stop", NOT_RUNNING),
        ):
            self.invoke(operation, state=self.state, expected=1, refusal=refusal)
            self.require_entries(
                self.state, CRASHED_ENTRIES, f"after {operation} of a crashed sandbox"
            )
        if control_endpoint_present(runtime):
            raise RuntimeError("the crashed sandbox kept its control endpoint")
        self.require_processes((), "after the requests of a crashed sandbox")

    def check_deprovision(self) -> None:
        self.copy_evidence(self.state / LOG, "openvmm.log")
        foreign = self.state / FOREIGN_NAME
        foreign.write_text(FOREIGN_CONTENT, encoding="utf-8")
        self.invoke(
            "deprovision",
            state=self.state,
            expected=1,
            refusal=Refusal("foreign-files", (FOREIGN_NAME,)),
        )
        self.require_entries(
            self.state, frozenset({FOREIGN_NAME}), "after deprovision kept foreign data"
        )
        if foreign.read_text(encoding="utf-8") != FOREIGN_CONTENT:
            raise RuntimeError("deprovision changed a file that NVX does not own")
        foreign.unlink()
        self.invoke("deprovision", state=self.state)
        if self.state.exists():
            raise RuntimeError("deprovision kept the sandbox state directory")
        self.invoke(
            "exec",
            "--entrypoint",
            "/bin/true",
            state=self.state,
            expected=1,
            refusal=NOT_PROVISIONED,
        )
        if self.state.exists():
            raise RuntimeError("exec recreated a deprovisioned sandbox's state")

    def clean_up(self) -> list[Exception]:
        """Stops and deprovisions what a failed acceptance left behind."""
        errors: list[Exception] = []
        states = [
            (state, label)
            for state, label in ((self.state, ""), (self.identity_state, "identity-"))
            if state.is_dir()
        ]
        for state, _ in states:
            try:
                if (state / RUNTIME).is_file():
                    runtime = read_runtime(state)
                    self.run_cli("stop", state=state)
                    kill_openvmm(runtime, self.timeout, self.root.name)
            except Exception as error:
                errors.append(error)
        # Every OpenVMM process whose arguments name the fixture is the
        # acceptance's own, even one that no runtime record names.
        try:
            for pid in openvmm_processes(self.root.name):
                with contextlib.suppress(OSError):
                    terminate_process(pid)
            deadline = time.monotonic() + self.timeout
            while strays := openvmm_processes(self.root.name):
                if time.monotonic() >= deadline:
                    raise RuntimeError(f"OpenVMM processes {list(strays)} survived")
                time.sleep(0.05)
        except Exception as error:
            errors.append(error)
        for state, label in states:
            try:
                self.copy_evidence(state / LOG, f"{label}openvmm.log")
                (state / FOREIGN_NAME).unlink(missing_ok=True)
                # Deprovision ends a start that was interrupted, and clears the
                # runtime record of an OpenVMM that exited.
                result = self.run_cli("deprovision", state=state)
                if result.returncode != 0:
                    raise RuntimeError(
                        f"cannot deprovision {state}: "
                        f"{bounded_text(result.stderr).strip()}"
                    )
            except Exception as error:
                errors.append(error)
        return errors


def run_acceptance(acceptance: LifecycleAcceptance) -> None:
    """Runs an acceptance, then removes its fixture, which a failure keeps only
    if its cleanup leaves a sandbox's state behind.

    An interruption, as when CI cancels the job (#946), stops the sandboxes
    that the acceptance started, as a failure does, and then propagates as it
    is, after a warning that names any cleanup errors."""
    try:
        acceptance.run()
    except BaseException as error:
        errors = acceptance.clean_up()
        try:
            acceptance.write_checks()
        except Exception as evidence_error:
            errors.append(evidence_error)
        if acceptance.state.exists() or acceptance.identity_state.exists():
            preserved = bounded_text(
                str(acceptance.root).encode("utf-8", errors="replace")
            )
            errors.append(RuntimeError(f"fixture preserved for recovery: {preserved}"))
        else:
            shutil.rmtree(acceptance.root, ignore_errors=True)
        if errors and isinstance(error, Exception):
            raise RuntimeError(f"{error}; cleanup: {format_errors(errors)}") from error
        if errors:
            print(
                f"warning: cleanup after the interruption: {format_errors(errors)}",
                file=sys.stderr,
                flush=True,
            )
        raise
    acceptance.write_checks()
    shutil.rmtree(acceptance.root)


def run_sandbox_lifecycle(backend: str, *, timeout: float, output_dir: Path) -> None:
    artifacts = load_workload_artifacts()
    output_dir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="nvx-public-lifecycle-"))
    run_acceptance(
        LifecycleAcceptance(
            backend,
            timeout=timeout,
            output_dir=output_dir,
            artifacts=artifacts,
            root=root,
        )
    )
