"""Public CLI acceptance for the managed sandbox lifecycle and security profile.

The scenario runs `nvx sandbox` as subprocesses against the real kernel, the
Alpine control initramfs, the Ubuntu EROFS layer, and ext4 scratch copies. It
checks identity admission, fail-closed lifecycle transitions, that overlapping
starts launch one VM, warm-guest state across exec requests and restarts, the
guest security profile, bounded output and outcome reports,
that the CLI of artifacts of another runtime identity drives no VM that these
started and its stop terminates OpenVMM, and that stop, a crash, and
deprovision leave no OpenVMM process, control endpoint, capability, or runtime
record behind while the supplied layer and scratch artifacts stay intact.
An image copied into the private fixture also exercises a stale staged binding:
start refuses without a runtime, then the same image is re-provisioned and
must run the real guest probe and stop with a successful teardown report.
"""

from __future__ import annotations

import concurrent.futures
import contextlib
import hashlib
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, cast

from . import nvx_cli
from .build_config import KernelBuildConfig
from .build_constants import (
    BuildConstants,
    InitramfsBuildConstants,
    KernelBuildConstants,
    OpenVMMBuildConstants,
)
from .common import ScriptError, artifact_path, sha256_file
from .managed_exec_tests import (
    WorkloadArtifacts,
    bounded_text,
    evidence_argv,
    format_errors,
    load_workload_artifacts,
    read_exec_outcome,
    workload_architecture_options,
)

PROBE = "/sbin/nvx-sandbox-smoke"
HOSTNAME = "nvx-lifecycle"
MEMORY_MIB = 256
# An identity that the Ubuntu layer's user database lacks.
UNAVAILABLE_IDENTITY = "4242:4242"
# More output than the 1 MiB that version 1 of NVXC forwarded for one exec
# request, which version 2 streams whole.
STREAMED_OUTPUT_BYTES = 2 * 1024 * 1024
STATE_PATH = "/tmp/nvx lifecycle state"
STATE_ARGUMENTS = ("  leading and trailing  ", "tab\tseparated", "line one\nline two")
STATE_CONTENT = "".join(f"{argument}\n" for argument in STATE_ARGUMENTS).encode()
REFUSED_MARKER = "/tmp/nvx-refused-request"
FOREIGN_NAME = "operator-note.txt"
FOREIGN_CONTENT = "operator data\n"
EVIDENCE_PREFIX = "sandbox-lifecycle-"
EVIDENCE_LIMIT = 64 * 1024


def check_changed_layer_start(
    result: subprocess.CompletedProcess[bytes], image: str
) -> None:
    """Check the actual pre-spawn refusal, not a successful mock observation."""
    refusal = Refusal(
        "unavailable",
        (
            image,
            "changed after its distro binding was made",
            "modification time differs",
            "deprovision the sandbox and provision it again",
        ),
    )
    if result.returncode != 1 or result.stdout or not refusal.matches(result.stderr):
        raise RuntimeError(
            f"the changed layer start returned {result.returncode}, "
            f"stdout={bounded_text(result.stdout)!r}, "
            f"stderr={bounded_text(result.stderr)!r}, expected {refusal}"
        )


def check_restage_bindings(before: bytes, after: bytes, image: str) -> None:
    """Require new accepted binding data for the very same owned image."""
    bindings: list[dict[str, object]] = []
    for raw in (before, after):
        try:
            config = json.loads(raw)
        except (UnicodeDecodeError, ValueError) as error:
            raise RuntimeError("the restaged configuration is malformed") from error
        if not isinstance(config, dict):
            raise RuntimeError("the restaged configuration is not an object")
        config = cast(dict[str, object], config)
        recorded_image = config.get("image")
        binding = config.get("distro_binding")
        policy = (
            cast(dict[str, object], binding).get("policy")
            if isinstance(binding, dict)
            else None
        )
        if (
            not isinstance(recorded_image, dict)
            or cast(dict[str, object], recorded_image).get("path") != image
            or not isinstance(binding, dict)
            or policy != {"revision": 1, "result": "accepted"}
            or not isinstance(policy, dict)
            or type(cast(dict[str, object], policy).get("revision")) is not int
        ):
            raise RuntimeError(
                "restage did not bind the same owned image with an accepted policy"
            )
        bindings.append(cast(dict[str, object], binding))
    if bindings[0] == bindings[1]:
        raise RuntimeError("re-provision kept the stale distro binding")


# A sandbox's VM starts without an OpenVMM lifecycle option, since nvx-init
# owns its lifecycle, so OpenVMM reports its operation as a run.
MANAGED_SUCCESS = {"operation": "run", "category": "success", "status_code": 0}
# The guest powers off with status 125 when it refuses the workload identity,
# and a managed start reports the status from OpenVMM's outcome report.
REFUSAL_CATEGORY = "guest-exit"
MANAGED_REFUSAL = {
    "operation": "run",
    "category": REFUSAL_CATEGORY,
    "status_code": 125,
}


@dataclass(frozen=True)
class Refusal:
    """An error that a public operation must report: the stable code of
    nvx's error, and stable fragments of its message, such as the names of
    options, files, and parts, which the wording around them may change."""

    code: str
    fragments: tuple[str, ...] = ()

    def matches(self, stderr: bytes) -> bool:
        reported = nvx_cli.reported_error(stderr)
        return (
            reported is not None
            and reported.code == self.code
            and all(fragment in reported.message for fragment in self.fragments)
        )

    def __str__(self) -> str:
        named = "".join(f" naming {fragment!r}" for fragment in self.fragments)
        return f"error {self.code!r}{named}"


# A start or a run that OpenVMM ended names the category of its outcome report.
REFUSED_START = Refusal("exited", (REFUSAL_CATEGORY,))

# The files of a state directory, as nvx sandbox documents them.
CONFIG = "config.json"
RUNTIME = "runtime.json"
CAPABILITY = "control.capability"
LOG = "openvmm.log"
SOCKET = "control.sock"
OUTCOME = "outcome.json"
LOCK = "sandbox.lock"
# The first exec of a provisioned sandbox creates its exec lock, even an exec
# that finds no VM, and the lock stays until deprovision removes the sandbox.
EXEC_LOCK = "exec.lock"
# The fields of the store's runtime record.
RUNTIME_FIELDS = frozenset({"capability", "endpoint", "process", "runtime", "vmDir"})
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
# nvx reads the runtime identity of a checkout from the build provenance of
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


def read_build_provenance(*, architecture: str = "x86_64") -> dict[str, dict[str, Any]]:
    """Returns the build provenance from which nvx reads the runtime identity
    of the checkout, by file name. Only guest provenance is ISA-qualified;
    OpenVMM's host provenance stays in the shared build directory."""
    provenance: dict[str, dict[str, Any]] = {}
    names = PROVENANCE_NAMES
    if architecture != "x86_64":
        names = (
            OpenVMMBuildConstants.PROVENANCE_NAME,
            KernelBuildConfig.for_architecture(architecture).provenance_name,
            InitramfsBuildConstants.PROVENANCE_NAME,
        )
    for name in names:
        path = (
            BuildConstants.BUILD_DIR / name
            if architecture == "x86_64" or name == OpenVMMBuildConstants.PROVENANCE_NAME
            else artifact_path(name, architecture=architecture)
        )
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
    remains on Linux, where nvx counts a process as running until its last
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
    until it has exited as nvx tells; marker names the fixture, as for
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
    (nvx/src/host/session/tests/managed_lifecycle.rs) that hold for the
    layered root too: the report names the backend, reports `network_policy`,
    or that no network policy was requested when that is None, and every
    teardown step completed.
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


def read_runtime(state: Path, *, architecture: str = "x86_64") -> dict[str, Any]:
    path = state / RUNTIME
    try:
        value: object = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RuntimeError(f"sandbox runtime record is unreadable: {path}") from error
    if not isinstance(value, dict):
        raise RuntimeError(f"sandbox runtime record is not an object: {path}")
    runtime = cast(dict[str, Any], value)
    fields = RUNTIME_FIELDS
    if "version" in runtime or "architecture" in runtime or architecture != "x86_64":
        fields |= {"version", "architecture"}
        if (
            type(runtime.get("version")) is not int
            or runtime.get("version") != 2
            or runtime.get("architecture") != architecture
        ):
            raise RuntimeError(
                f"sandbox runtime record has an invalid ISA/version: {path}"
            )
    if frozenset(runtime) != fields:
        raise RuntimeError(f"sandbox runtime record has unexpected fields: {path}")
    vm_dir = runtime["vmDir"]
    capability = runtime["capability"]
    if not isinstance(capability, list):
        raise RuntimeError(f"sandbox runtime record has an invalid capability: {path}")
    values = cast(list[object], capability)
    if len(values) != 32 or any(
        type(byte) is not int or not 0 <= byte <= 255 for byte in values
    ):
        raise RuntimeError(f"sandbox runtime record has an invalid capability: {path}")
    try:
        if (
            not isinstance(vm_dir, str)
            or not Path(vm_dir).is_absolute()
            or not Path(vm_dir).samefile(state)
        ):
            raise RuntimeError(
                f"sandbox runtime record names another VM directory: {path}"
            )
        installed = (state / CAPABILITY).read_bytes()
    except OSError as error:
        raise RuntimeError(
            f"sandbox runtime record ownership cannot be checked: {path}"
        ) from error
    if bytes(cast(list[int], values)) != installed:
        raise RuntimeError(
            f"sandbox runtime record capability differs from its owned file: {path}"
        )
    return runtime


def check_cycle_count(cycles: int, *, minimum: int) -> None:
    if type(cycles) is not int or cycles < minimum:
        raise ScriptError(f"start_stop_cycles must be at least {minimum}")


class LifecycleAcceptance:
    # The prefix of the evidence files in the output directory.
    evidence_prefix = EVIDENCE_PREFIX
    default_start_stop_cycles = 1

    def __init__(
        self,
        backend: str,
        *,
        timeout: float,
        output_dir: Path,
        artifacts: WorkloadArtifacts,
        root: Path,
        architecture: str = "x86_64",
        start_stop_cycles: int | None = None,
    ) -> None:
        workload_architecture_options(architecture)
        cycles = (
            self.default_start_stop_cycles
            if start_stop_cycles is None
            else start_stop_cycles
        )
        check_cycle_count(cycles, minimum=self.default_start_stop_cycles)
        self.architecture = architecture
        self.start_stop_cycles = cycles
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
        # The state directories that hold the exec lock of an earlier exec.
        self.exec_locks: set[Path] = set()

    def launch_options(self, scratch: Path, *, distro: Path | None = None) -> list[str]:
        return [
            *workload_architecture_options(self.architecture),
            "--hypervisor",
            self.backend,
            "--layer",
            f"distro,{self.artifacts.distro if distro is None else distro},{self.artifacts.layer_uuid}",
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
        """Runs one public operation with `nvx sandbox`, under the artifact
        root `root`, or else under this checkout."""
        command = nvx_cli.command_line(("sandbox", operation), root=root)
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
        reported = nvx_cli.reported_error(result.stderr)
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
        provisioned = state is not None and (state / CONFIG).is_file()
        result = self.run_cli(operation, *arguments, state=state, root=root)
        if state is not None:
            if operation == "exec" and provisioned:
                self.exec_locks.add(state)
            elif not (state / CONFIG).exists():
                # Deprovision removes the exec lock with the configuration.
                self.exec_locks.discard(state)
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
        if state in self.exec_locks:
            expected |= {EXEC_LOCK}
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
        self.check_restaged_layer()
        self.provision()
        runtime = self.start(description="first start")
        self.check_running_transitions(runtime)
        self.check_warm_state()
        self.check_security_profile()
        self.check_bounded_results()
        self.check_deprovision_while_running(runtime)
        self.check_start_stop_cycles(runtime, layer_digest)
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

    def check_start_stop_cycles(
        self, runtime: dict[str, Any], layer_digest: str
    ) -> None:
        """Stop/restart the same provision, retaining its scratch contents."""
        self.stop(runtime, layer_digest)
        for cycle in range(1, self.start_stop_cycles):
            runtime = self.start(description=f"cycle {cycle + 1} start")
            result = self.exec("/bin/cat", f"--arg={STATE_PATH}", expected=None)
            self.require_output(result, STATE_CONTENT, "the restarted sandbox's state")
            self.stop(runtime, layer_digest)
            self.copy_evidence(self.state / OUTCOME, f"cycle-{cycle + 1}-outcome.json")

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

        # The guest refuses an identity that the image lacks before it runs any
        # workload, so a run fails as a managed start does, below, with the
        # guest's status from the outcome report that OpenVMM publishes.
        run_scratch = self.root / "run-scratch.ext4"
        shutil.copyfile(self.artifacts.scratch_template, run_scratch)
        result = self.invoke(
            "run",
            *self.launch_options(run_scratch),
            "--workload-user",
            UNAVAILABLE_IDENTITY,
            "--entrypoint",
            PROBE,
            "--arg=init",
            state=None,
            expected=1,
            refusal=REFUSED_START,
        )
        console = result.stdout + result.stderr
        self.save_evidence("identity-run.log", console)
        if b"NVX-UBUNTU-SANDBOX-PROFILE-OK" in console:
            raise RuntimeError("a run of an unavailable identity ran its workload")

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

    def check_restaged_layer(self) -> None:
        """Refuse a changed private image, then actually boot its fresh binding."""
        source = self.artifacts.distro
        source_stat = source.stat()
        source_digest = sha256_file(source)
        source_manifest = source.with_name(source.name + ".manifest.json")
        manifest_stat = source_manifest.stat() if source_manifest.is_file() else None
        manifest_digest = (
            sha256_file(source_manifest) if manifest_stat is not None else None
        )

        def source_metadata(value: os.stat_result) -> tuple[int, ...]:
            return (
                value.st_dev,
                value.st_ino,
                value.st_mode,
                value.st_size,
                value.st_mtime_ns,
                value.st_ctime_ns,
            )

        def require_protected_source() -> None:
            current = source.stat()
            if (
                sha256_file(source) != source_digest
                or source_metadata(current) != source_metadata(source_stat)
                or (
                    manifest_stat is not None
                    and (
                        sha256_file(source_manifest) != manifest_digest
                        or source_metadata(source_manifest.stat())
                        != source_metadata(manifest_stat)
                    )
                )
                or (manifest_digest is None and source_manifest.exists())
            ):
                raise RuntimeError("restage modified a protected source")

        directory = self.root / "restage-image"
        directory.mkdir(mode=0o700)
        image = directory / source.name
        shutil.copyfile(source, image)
        manifest = image.with_name(image.name + ".manifest.json")
        if manifest_digest is not None:
            shutil.copyfile(source_manifest, manifest)
        shutil.copyfile(self.artifacts.scratch_template, self.scratch)
        options = (
            *self.launch_options(self.scratch, distro=image),
            "--hostname",
            HOSTNAME,
        )
        self.invoke("provision", *options, state=self.state)
        self.require_entries(self.state, PROVISIONED_ENTRIES, "before the image change")
        before = (self.state / CONFIG).read_bytes()
        self.save_evidence("restage-before-config.json", before)
        staged_stat = image.stat()
        os.utime(
            image, ns=(staged_stat.st_atime_ns, staged_stat.st_mtime_ns + 1_000_000_000)
        )
        if image.stat().st_mtime_ns == staged_stat.st_mtime_ns:
            raise RuntimeError("the owned image's modification time did not change")
        result = self.invoke("start", state=self.state, expected=None)
        self.save_evidence("restage-refusal.log", result.stdout + result.stderr)
        check_changed_layer_start(result, str(image))
        self.require_entries(
            self.state, PROVISIONED_ENTRIES, "after the stale image refusal"
        )
        if (self.state / CONFIG).read_bytes() != before:
            raise RuntimeError("a stale image start changed its saved configuration")
        self.require_processes((), "after the stale image refusal")
        self.exec(PROBE, "--arg=init", expected=1, refusal=NOT_RUNNING)
        require_protected_source()
        self.invoke("deprovision", state=self.state)
        if self.state.exists():
            raise RuntimeError("deprovision kept the stale sandbox state")
        self.invoke("provision", *options, state=self.state)
        after = (self.state / CONFIG).read_bytes()
        self.save_evidence("restage-after-config.json", after)
        check_restage_bindings(before, after, str(image))
        runtime = self.start(description="restaged image start")
        result = self.exec(PROBE, "--arg=init", "--arg=entry", expected=None)
        self.save_evidence("restage-probe.log", result.stdout + result.stderr)
        if result.returncode != 0 or result.stderr:
            raise RuntimeError("the restaged image's guest probe failed")
        for marker in (
            b"NVX-UBUNTU-SANDBOX-PROFILE-OK uid=65534 gid=65534",
            b"NVX-UBUNTU-SANDBOX-ENTRY-OK",
            b"NVX-UBUNTU-SANDBOX-OK uid=65534 gid=65534",
        ):
            if marker not in result.stdout:
                raise RuntimeError(
                    f"the sandbox security probe did not report {marker.decode()}"
                )
        self.require_entries(self.state, RUNNING_ENTRIES, "after restaged guest exec")
        self.stop(runtime, source_digest)
        self.copy_evidence(self.state / OUTCOME, "restage-outcome.json")
        self.invoke("deprovision", state=self.state)
        if self.state.exists():
            raise RuntimeError("deprovision kept the restaged sandbox state")
        self.require_processes((), "after the restaged image stop")
        require_protected_source()
        if sha256_file(image) != source_digest:
            raise RuntimeError("the restaged guest modified its owned image contents")
        image.unlink()
        if manifest_digest is not None:
            manifest.unlink()
        directory.rmdir()
        self.save_evidence(
            "restage-source.json",
            (
                json.dumps(
                    {
                        "protected_image_sha256": source_digest,
                        "protected_manifest_sha256": manifest_digest,
                        "owned_image": str(image),
                        "same_fixture_reprovision": True,
                    },
                    indent=2,
                )
                + "\n"
            ).encode(),
        )

    def provision(self) -> None:
        shutil.copyfile(self.artifacts.scratch_template, self.scratch)
        options = (
            *self.launch_options(self.scratch),
            "--hostname",
            HOSTNAME,
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
        runtime = (
            read_runtime(self.state)
            if self.architecture == "x86_64"
            else read_runtime(self.state, architecture=self.architecture)
        )
        evidence = {
            name: value for name, value in runtime.items() if name != "capability"
        }
        evidence["capabilitySha256"] = hashlib.sha256(
            bytes(runtime["capability"])
        ).hexdigest()
        self.save_evidence(
            "runtime.json", (json.dumps(evidence, indent=2) + "\n").encode()
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
        # nvx-init runs each request as a plain process in the VM's root, so
        # the probe's check that each mount appears once also shows that the
        # earlier requests mounted nothing there.
        result = self.exec(PROBE, "--arg=init", "--arg=entry")
        self.save_evidence("probe.log", result.stdout + result.stderr)
        for marker in (
            b"NVX-UBUNTU-SANDBOX-PROFILE-OK uid=65534 gid=65534",
            b"NVX-UBUNTU-SANDBOX-ENTRY-OK",
            b"NVX-UBUNTU-SANDBOX-OK uid=65534 gid=65534",
        ):
            if marker not in result.stdout:
                raise RuntimeError(
                    f"the sandbox security probe did not report {marker.decode()}"
                )

    def check_bounded_results(self) -> None:
        # The host receives all of the workload's output, and the outcome
        # report carries only the operation, category, and status.
        report = self.root / "streamed-output-outcome.json"
        result = self.exec(
            "/usr/bin/head",
            "--arg=-c",
            f"--arg={STREAMED_OUTPUT_BYTES}",
            "--arg=/dev/zero",
            "--outcome-report",
            str(report),
        )
        if (
            len(result.stdout) != STREAMED_OUTPUT_BYTES
            or result.stdout.strip(b"\0")
            or result.stderr
        ):
            raise RuntimeError(
                "the streamed output forwarded "
                f"{len(result.stdout)} stdout and {len(result.stderr)} stderr bytes"
            )
        read_exec_outcome(report, category="exit", status_code=0)
        self.copy_evidence(report, "streamed-output-outcome.json")

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
        # nvx-init syncs the file systems and powers the VM off with them
        # mounted, so the restart that reads the state back checks the stop.
        self.invoke("stop", state=self.state)
        self.require_entries(self.state, STOPPED_ENTRIES, "after stop")
        if openvmm_running(runtime, self.root.name):
            raise RuntimeError("OpenVMM still runs after stop")
        if control_endpoint_present(runtime):
            raise RuntimeError("stop left the control endpoint behind")
        self.require_processes((), "after stop")
        self.copy_evidence(self.state / OUTCOME, "outcome.json")
        check_managed_report(managed_report(self.state), self.backend)
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
        with another initramfs digest, so that nvx reads a runtime identity
        there that differs from the checkout's in the initramfs."""
        build = self.other_root / BuildConstants.BUILD_DIRECTORY_NAME
        build.mkdir(parents=True)
        records = (
            read_build_provenance()
            if self.architecture == "x86_64"
            else read_build_provenance(architecture=self.architecture)
        )
        for name, provenance in records.items():
            if name == InitramfsBuildConstants.PROVENANCE_NAME:
                digest = provenance.get(INITRAMFS_DIGEST)
                if not isinstance(digest, str):
                    raise RuntimeError(f"build provenance {name} names no initramfs")
                provenance = {
                    **provenance,
                    INITRAMFS_DIGEST: hashlib.sha256(digest.encode()).hexdigest(),
                }
            directory = build
            if (
                self.architecture != "x86_64"
                and name != OpenVMMBuildConstants.PROVENANCE_NAME
            ):
                directory /= self.architecture
                directory.mkdir(exist_ok=True)
            (directory / name).write_text(json.dumps(provenance), encoding="utf-8")

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
                    runtime = (
                        read_runtime(state)
                        if self.architecture == "x86_64"
                        else read_runtime(state, architecture=self.architecture)
                    )
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


def run_sandbox_lifecycle(
    backend: str,
    *,
    timeout: float,
    output_dir: Path,
    architecture: str = "x86_64",
    start_stop_cycles: int = 1,
) -> None:
    check_cycle_count(start_stop_cycles, minimum=1)
    artifacts = (
        load_workload_artifacts()
        if architecture == "x86_64"
        else load_workload_artifacts(architecture=architecture)
    )
    selection: dict[str, Any] = {}
    if architecture != "x86_64":
        selection["architecture"] = architecture
    if start_stop_cycles != 1:
        selection["start_stop_cycles"] = start_stop_cycles
    output_dir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="nvx-public-lifecycle-"))
    run_acceptance(
        LifecycleAcceptance(
            backend,
            timeout=timeout,
            output_dir=output_dir,
            artifacts=artifacts,
            root=root,
            **selection,
        )
    )
