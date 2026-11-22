"""Public CLI acceptance for a sandbox whose overlay upper is in RAM.

The scenario runs `nvx sandbox --upper ram` as subprocesses against the
real kernel, the Alpine control initramfs, and the Ubuntu EROFS layer, with no
scratch image. It checks that the planner refuses `--upper ram` beside
`--scratch`, and `--upper` outside `run` and `provision`, before it creates
any state. A run, which keeps no state, attaches the distro layer alone,
declares the RAM upper and its default cap on the kernel command line of a VM
that nvx-init manages, runs the smoke probe once over NVXC, which requires the
overlay's upper on a tmpfs, and forwards the report of the VM's graceful stop.
A managed sandbox records the RAM upper without a scratch image, and passes the
probe's managed checks. Its guest has one virtio block device and declares the
RAM upper, and a later request reads a file that an earlier one wrote. Once the
sandbox stops and starts again, the file is gone.
The managed probe also fills the real default RAM upper to ENOSPC, checks its
half-memory capacity and actual byte accounting, deletes only its owned
filler, and proves that a subsequent write can be read.
"""

from __future__ import annotations

import json
import subprocess
import tempfile
from pathlib import Path
from typing import Any, cast

from .managed_exec_tests import load_workload_artifacts, workload_architecture_options
from .sandbox_lifecycle_tests import (
    CONFIG,
    OUTCOME,
    PROBE,
    PROVISIONED_ENTRIES,
    STOPPED_ENTRIES,
    LifecycleAcceptance,
    Refusal,
    check_cycle_count,
    check_managed_report,
    managed_report,
    run_acceptance,
)

MEMORY_MIB = 256
RAM_UPPER_TOKEN = "nvx_overlay_upper=ramfs"
# The cap of a RAM upper whose run gives none: half of the guest's RAM.
DEFAULT_CAP_TOKEN = "nvx_ram_upper_percent=50"
SCRATCH_TOKEN = "nvx_scratch="
# The tokens with which a run's VM boots its layered root through nvx-init,
# which owns the VM's lifecycle, so that the run passes OpenVMM no lifecycle.
MANAGED_TOKENS = ("nvx_sandbox=1", "nvx_mode=managed")
LIFECYCLE_OPTION = "--microvm-lifecycle"
# The outcome that OpenVMM reports for a run whose guest stopped its VM, as it
# does for a managed sandbox's stop.
RUN_SUCCESS = {"operation": "run", "category": "success", "status_code": 0}
STATE_PATH = "/tmp/nvx-ram-upper-state"
STATE_CONTENT = b"kept in RAM\n"
# The guest's block devices, one per line, then its kernel command line.
VIEW_SCRIPT = "ls /sys/block && cat /proc/cmdline"
RAM_FILL_MARKER = b"NVX-UBUNTU-SANDBOX-RAM-FILL-OK"


def check_ram_fill(result: subprocess.CompletedProcess[bytes]) -> dict[str, int | str]:
    """Validate actual guest fill counters; failed commands cannot supply credit."""
    lines = [
        line
        for line in result.stdout.splitlines()
        if line.startswith(RAM_FILL_MARKER + b" ")
    ]
    if result.returncode != 0 or result.stderr or len(lines) != 1:
        raise RuntimeError(
            f"the RAM-upper fill returned {result.returncode}, stderr "
            f"{result.stderr!r}, and {len(lines)} complete observations"
        )
    fields: dict[str, str] = {}
    try:
        for word in lines[0][len(RAM_FILL_MARKER) + 1 :].decode("ascii").split():
            name, value = word.split("=", 1)
            if name in fields:
                raise ValueError(f"duplicate {name}")
            fields[name] = value
    except (UnicodeDecodeError, ValueError) as error:
        raise RuntimeError("the RAM-upper fill observation is malformed") from error
    numeric = ("page", "memory", "total", "available", "written", "dd_status")
    if set(fields) != {*numeric, "error", "after", "cleanup"}:
        raise RuntimeError(f"the RAM-upper fill fields are {fields!r}")
    numbers: dict[str, int] = {}
    for name in numeric:
        value = fields[name]
        if not value.isascii() or not value.isdecimal() or int(value) > (1 << 64) - 1:
            raise RuntimeError(f"the RAM-upper fill {name} is not a u64: {value!r}")
        numbers[name] = int(value)
    if (
        numbers["dd_status"] != 1
        or fields["error"] != "ENOSPC"
        or fields["after"] != "nvx-ram-upper"
        or fields["cleanup"] != "complete"
    ):
        raise RuntimeError(
            f"the RAM-upper fill did not prove ENOSPC and recovery: {fields!r}"
        )
    page = numbers["page"]
    memory = numbers["memory"]
    total = numbers["total"]
    available = numbers["available"]
    written = numbers["written"]
    if (
        page != 4096
        or memory == 0
        or total != (memory // 4096 + 1) // 2 * 4096
        or total == 0
    ):
        raise RuntimeError(
            f"the RAM-upper fill capacity is not page-rounded half-memory: {fields!r}"
        )
    if not 0 < written <= available <= total or available - written >= 1 << 20:
        raise RuntimeError(
            f"the RAM-upper fill byte accounting exceeds its original slack: {fields!r}"
        )
    return {
        **numbers,
        "error": fields["error"],
        "after": fields["after"],
        "cleanup": fields["cleanup"],
    }


def probe_markers(*, entry: bool) -> tuple[bytes, ...]:
    """Returns what the smoke probe reports when its profile and, for a managed
    execution, `entry` checks pass."""
    markers = [b"NVX-UBUNTU-SANDBOX-PROFILE-OK uid=65534 gid=65534"]
    if entry:
        markers.append(b"NVX-UBUNTU-SANDBOX-ENTRY-OK")
    markers.append(b"NVX-UBUNTU-SANDBOX-OK uid=65534 gid=65534")
    return tuple(markers)


def check_ram_tokens(command_line: str, context: str) -> None:
    """Checks that `command_line`, or a quoted command that holds it, declares
    the RAM upper once, and no scratch upper."""
    tokens = [token.strip("'\"") for token in command_line.split()]
    if tokens.count(RAM_UPPER_TOKEN) != 1 or any(
        token.startswith(SCRATCH_TOKEN) for token in tokens
    ):
        raise RuntimeError(
            f"{context} does not declare the RAM upper alone: {command_line!r}"
        )


def check_run_report(path: Path, backend: str) -> None:
    """Checks the outcome report that a run forwards, of a VM that its guest
    stopped and that was torn down."""
    try:
        report: object = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise RuntimeError("the run forwarded no outcome report") from error
    if not isinstance(report, dict):
        raise RuntimeError(f"the run's outcome report is {report!r}")
    fields = cast(dict[str, object], report)
    outcome = fields.get("outcome")
    if outcome != RUN_SUCCESS or fields.get("backend") != backend:
        raise RuntimeError(f"the run reported {outcome!r} on {fields.get('backend')!r}")
    # microvm_tests, which runs this scenario, imports this module.
    from .microvm_tests import OUTCOME_TEARDOWN_FIELDS

    teardown = fields.get("teardown")
    if (
        not isinstance(teardown, dict)
        or set(cast(dict[str, object], teardown)) != set(OUTCOME_TEARDOWN_FIELDS)
        or not all(
            value is True for value in cast(dict[str, object], teardown).values()
        )
    ):
        raise RuntimeError(f"the run's teardown is {teardown!r}")


class RamUpperAcceptance(LifecycleAcceptance):
    evidence_prefix = "sandbox-ram-upper-"
    default_start_stop_cycles = 2

    def ram_options(self) -> list[str]:
        return [
            *workload_architecture_options(self.architecture),
            "--hypervisor",
            self.backend,
            "--layer",
            f"distro,{self.artifacts.distro},{self.artifacts.layer_uuid}",
            "--upper",
            "ram",
            "--memory-mib",
            str(MEMORY_MIB),
        ]

    def run(self) -> None:
        self.check_refusals()
        self.check_run()
        self.check_managed()
        self.require_processes((), "after the RAM-upper sandboxes")

    def check_refusals(self) -> None:
        """The planner refuses a scratch image for a RAM upper, and an upper
        for an operation other than `run` and `provision`, before it creates
        any state."""
        self.invoke(
            "provision",
            *self.ram_options(),
            "--scratch",
            str(self.artifacts.scratch_template),
            state=self.state,
            expected=1,
            refusal=Refusal("unsupported", ("--upper ram", "--scratch")),
        )
        self.invoke(
            "start",
            "--upper",
            "ram",
            state=self.state,
            expected=1,
            refusal=Refusal("unsupported", ("--upper", "run or provision")),
        )
        if self.state.exists():
            raise RuntimeError("a refused RAM-upper request created sandbox state")

    def check_run(self) -> None:
        """A run, which keeps no state, attaches the distro layer alone,
        declares the RAM upper and its default cap in the VM that nvx-init
        manages, passes the probe's managed checks once, and forwards the
        report of the VM's graceful stop."""
        # The report's path lies under the fixture, and the kernel command line
        # names the fixture, which names the run's OpenVMM to the process checks
        # and to cleanup, as no other argument does: the distro layer is the
        # build's, the run has no scratch image, and its VM directory and the
        # report that OpenVMM writes there are the run's own.
        report = self.root / "run-outcome.json"
        result = self.invoke(
            "run",
            *self.ram_options(),
            "--outcome-report",
            str(report),
            "--cmdline",
            f"sandbox_fixture={self.root.name}",
            "--entrypoint",
            PROBE,
            "--arg=init",
            "--arg=entry",
            state=None,
        )
        console = result.stdout + result.stderr
        self.save_evidence("run.log", console)
        lines = result.stdout.splitlines()
        command = lines[0].decode(errors="replace") if lines else ""
        if (
            not command.startswith(">> ")
            or command.count("--microvm-sandbox-block") != 1
            or "distro:file:" not in command
            or "scratch:file:" in command
        ):
            raise RuntimeError(
                f"the run did not attach its distro layer alone: {command!r}"
            )
        if self.root.name not in command:
            raise RuntimeError(
                f"the run's OpenVMM command does not name the fixture: {command!r}"
            )
        check_ram_tokens(command, "the run's OpenVMM command")
        tokens = [token.strip("'\"") for token in command.split()]
        if (
            LIFECYCLE_OPTION in tokens
            or DEFAULT_CAP_TOKEN not in tokens
            or not all(token in tokens for token in MANAGED_TOKENS)
        ):
            raise RuntimeError(
                f"the run's VM is not one that nvx-init manages with the RAM "
                f"upper's default cap: {command!r}"
            )
        for marker in probe_markers(entry=True):
            if marker not in result.stdout:
                raise RuntimeError(
                    f"the run's RAM-upper probe did not report {marker.decode()}"
                )
        self.copy_evidence(report, "run-outcome.json")
        check_run_report(report, self.backend)
        if self.state.exists():
            raise RuntimeError("the run created sandbox state")
        self.require_processes((), "after the run")

    def check_managed(self) -> None:
        """A managed sandbox records the RAM upper without a scratch image, and
        its upper holds the workload's writes until the sandbox stops."""
        self.invoke("provision", *self.ram_options(), state=self.state)
        self.require_entries(self.state, PROVISIONED_ENTRIES, "after provision")
        config = json.loads((self.state / CONFIG).read_text(encoding="utf-8"))
        recorded = {
            name: config[name] for name in ("format", "upper", "scratch_binding")
        }
        if recorded != {
            "format": 10 if self.architecture == "x86_64" else 11,
            "upper": {"kind": "ram", "max_memory_percent": 50},
            "scratch_binding": None,
        } or any(name in config for name in ("layers", "scratch")):
            raise RuntimeError(f"provision recorded the RAM upper as {recorded!r}")
        if (
            self.architecture != "x86_64"
            and config.get("architecture") != self.architecture
        ):
            raise RuntimeError("provision recorded another ISA for the RAM upper")
        self.start(description="first start")
        result = self.exec(
            PROBE, "--arg=init", "--arg=entry", "--arg=ram-fill", expected=None
        )
        self.save_evidence("probe.log", result.stdout + result.stderr)
        fill = check_ram_fill(result)
        self.save_evidence(
            "ram-fill.json", (json.dumps(fill, indent=2) + "\n").encode()
        )
        for marker in probe_markers(entry=True):
            if marker not in result.stdout:
                raise RuntimeError(
                    f"the managed RAM-upper probe did not report {marker.decode()}"
                )
        self.check_guest_view()
        self.check_warm_ram_state()
        self.stop_sandbox("first stop")
        for cycle in range(1, self.start_stop_cycles):
            # A new start begins with an empty upper.
            self.start(
                description="start after the stop"
                if cycle == 1
                else f"cycle {cycle + 1} start"
            )
            result = self.exec(
                "/bin/sh", "--arg=-c", f"--arg=test ! -e {STATE_PATH}", expected=None
            )
            self.require_output(result, b"", "the restarted sandbox's empty upper")
            if cycle + 1 < self.start_stop_cycles:
                self.check_warm_ram_state()
            self.stop_sandbox(
                "second stop" if cycle == 1 else f"cycle {cycle + 1} stop"
            )
        self.invoke("deprovision", state=self.state)
        if self.state.exists():
            raise RuntimeError("deprovision kept the RAM-upper sandbox's state")

    def check_warm_ram_state(self) -> None:
        result = self.exec(
            "/bin/sh",
            "--arg=-c",
            f"--arg=printf 'kept in RAM\\n' >{STATE_PATH}",
            expected=None,
        )
        self.require_output(result, b"", "the RAM upper's state-writing request")
        result = self.exec("/bin/cat", f"--arg={STATE_PATH}", expected=None)
        self.require_output(result, STATE_CONTENT, "the RAM upper's state")

    def check_guest_view(self) -> None:
        """The guest of a RAM upper has one virtio block device, the distro
        layer's, and declares the RAM upper on its kernel command line."""
        result = self.exec("/bin/sh", "--arg=-c", f"--arg={VIEW_SCRIPT}", expected=None)
        self.save_evidence("view.log", result.stdout + result.stderr)
        lines = result.stdout.decode(errors="replace").splitlines()
        if result.returncode != 0 or not lines:
            raise RuntimeError(
                f"the guest view of the RAM upper returned {result.returncode}"
            )
        devices = [name for name in lines[:-1] if name.startswith("vd")]
        if devices != ["vda"]:
            raise RuntimeError(
                f"the guest of a RAM upper has the virtio block devices {devices}"
            )
        check_ram_tokens(lines[-1], "the guest's kernel command line")

    def stop_sandbox(self, description: str) -> None:
        self.invoke("stop", state=self.state)
        self.require_entries(self.state, STOPPED_ENTRIES, f"after the {description}")
        self.copy_evidence(
            self.state / OUTCOME, f"{description.replace(' ', '-')}-outcome.json"
        )
        check_managed_report(managed_report(self.state), self.backend)
        self.require_processes((), f"after the {description}")


def run_sandbox_ram_upper(
    backend: str,
    *,
    timeout: float,
    output_dir: Path,
    architecture: str = "x86_64",
    start_stop_cycles: int = 2,
) -> None:
    check_cycle_count(start_stop_cycles, minimum=2)
    artifacts = (
        load_workload_artifacts()
        if architecture == "x86_64"
        else load_workload_artifacts(architecture=architecture)
    )
    selection: dict[str, Any] = {}
    if architecture != "x86_64":
        selection["architecture"] = architecture
    if start_stop_cycles != 2:
        selection["start_stop_cycles"] = start_stop_cycles
    output_dir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="nvx-public-ram-upper-"))
    run_acceptance(
        RamUpperAcceptance(
            backend,
            timeout=timeout,
            output_dir=output_dir,
            artifacts=artifacts,
            root=root,
            **selection,
        )
    )
