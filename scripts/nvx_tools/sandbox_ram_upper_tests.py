"""Public CLI acceptance for a sandbox whose overlay upper is in RAM.

The scenario runs `nvx-rs sandbox --upper ram` as subprocesses against the
real kernel, the Alpine control initramfs, and the Ubuntu EROFS layer, with no
scratch image. It checks that the planner refuses `--upper ram` beside
`--scratch`, and `--upper` outside `run` and `provision`, before it creates
any state. A one-shot run attaches the distro layer alone, declares the RAM
upper on the kernel command line, passes the smoke probe, which requires the
overlay's upper on a tmpfs, and reports its success. A managed sandbox records
the RAM upper without a scratch image, and passes the probe's managed and
resource-limit checks. Its guest has one virtio block device and declares the
RAM upper, a later request reads a file that an earlier one wrote, and a file
in the upper counts against the workload's memory limit. Once the sandbox
stops and starts again, the file is gone.
"""

from __future__ import annotations

import json
import tempfile
from pathlib import Path
from typing import cast

from .managed_exec_tests import load_workload_artifacts
from .sandbox_lifecycle_tests import (
    CONFIG,
    OUTCOME,
    PROBE,
    PROVISIONED_ENTRIES,
    STOPPED_ENTRIES,
    LifecycleAcceptance,
    Refusal,
    check_managed_report,
    managed_report,
    run_acceptance,
)

MEMORY_MIB = 256
MEMORY_MAX = 32 * 1024 * 1024
PIDS_MAX = 8
RAM_UPPER_TOKEN = "nvx_overlay_upper=ramfs"
SCRATCH_TOKEN = "nvx_scratch="
# The outcome that OpenVMM reports for a one-shot run whose workload succeeded.
RUN_SUCCESS = {"operation": "run", "category": "success", "status_code": 0}
STATE_PATH = "/tmp/nvx-ram-upper-state"
STATE_CONTENT = b"kept in RAM\n"
# A file of FILL_MIB MiB in the RAM upper leaves too little of MEMORY_MAX for
# an allocation of ALLOCATION_MIB MiB, which the memory controller then kills,
# and the same allocation fits once the file is gone, since the guest has no
# swap to which the upper's pages could go.
FILL_MIB = 20
ALLOCATION_MIB = 20
CHARGE_MARKER = b"NVX-RAM-UPPER-CHARGED"
CHARGE_SCRIPT = f"""\
head -c {FILL_MIB}M /dev/zero >/tmp/nvx-ram-upper-fill || exit 10
status=0
{{ dd if=/dev/zero of=/dev/null bs={ALLOCATION_MIB}M count=1; }} 2>/dev/null ||
    status=$?
rm -f /tmp/nvx-ram-upper-fill
[ "$status" -eq 137 ] || exit 11
{{ dd if=/dev/zero of=/dev/null bs={ALLOCATION_MIB}M count=1; }} 2>/dev/null ||
    exit 12
echo {CHARGE_MARKER.decode()}
"""
# The guest's block devices, one per line, then its kernel command line.
VIEW_SCRIPT = "ls /sys/block && cat /proc/cmdline"


def probe_markers(*, entry: bool) -> tuple[bytes, ...]:
    """Returns what the smoke probe reports when its profile, limit, and, for
    a managed execution, `entry` checks pass."""
    markers = [b"NVX-UBUNTU-SANDBOX-PROFILE-OK uid=65534 gid=65534"]
    if entry:
        markers.append(b"NVX-UBUNTU-SANDBOX-ENTRY-OK")
    markers.extend(
        (
            f"NVX-UBUNTU-SANDBOX-LIMITS-OK memory_max={MEMORY_MAX} "
            f"pids_max={PIDS_MAX}".encode(),
            b"NVX-UBUNTU-SANDBOX-OK uid=65534 gid=65534",
        )
    )
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
    """Checks the outcome report of a one-shot run whose workload succeeded and
    whose VM was torn down."""
    try:
        report: object = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise RuntimeError("the one-shot run published no outcome report") from error
    if not isinstance(report, dict):
        raise RuntimeError(f"the one-shot run's outcome report is {report!r}")
    fields = cast(dict[str, object], report)
    outcome = fields.get("outcome")
    if outcome != RUN_SUCCESS or fields.get("backend") != backend:
        raise RuntimeError(
            f"the one-shot run reported {outcome!r} on {fields.get('backend')!r}"
        )
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
        raise RuntimeError(f"the one-shot run's teardown is {teardown!r}")


class RamUpperAcceptance(LifecycleAcceptance):
    evidence_prefix = "sandbox-ram-upper-"

    def ram_options(self) -> list[str]:
        return [
            "--hypervisor",
            self.backend,
            "--layer",
            f"distro,{self.artifacts.distro},{self.artifacts.layer_uuid}",
            "--upper",
            "ram",
            "--memory-mib",
            str(MEMORY_MIB),
            "--memory-max",
            str(MEMORY_MAX),
            "--pids-max",
            str(PIDS_MAX),
        ]

    def run(self) -> None:
        self.check_refusals()
        self.check_one_shot()
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

    def check_one_shot(self) -> None:
        """A one-shot run attaches the distro layer alone, declares the RAM
        upper, passes the smoke probe, and reports its success."""
        # The report's path, under the fixture, also names the run's OpenVMM to
        # the process checks and to cleanup, as no other argument does: the
        # distro layer is the build's, and the run has no scratch image.
        report = self.root / "run-outcome.json"
        result = self.invoke(
            "run",
            *self.ram_options(),
            "--outcome-report",
            str(report),
            "--entrypoint",
            PROBE,
            "--arg=limits",
            f"--arg={MEMORY_MAX}",
            f"--arg={PIDS_MAX}",
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
                f"the one-shot run did not attach its distro layer alone: {command!r}"
            )
        if self.root.name not in command:
            raise RuntimeError(
                f"the one-shot run's OpenVMM command does not name the fixture: "
                f"{command!r}"
            )
        check_ram_tokens(command, "the one-shot run's OpenVMM command")
        for marker in probe_markers(entry=False):
            if marker not in result.stdout:
                raise RuntimeError(
                    f"the one-shot RAM-upper probe did not report {marker.decode()}"
                )
        self.copy_evidence(report, "run-outcome.json")
        check_run_report(report, self.backend)
        self.require_processes((), "after the one-shot run")

    def check_managed(self) -> None:
        """A managed sandbox records the RAM upper without a scratch image, and
        its upper holds the workload's writes until the sandbox stops."""
        self.invoke("provision", *self.ram_options(), state=self.state)
        self.require_entries(self.state, PROVISIONED_ENTRIES, "after provision")
        config = json.loads((self.state / CONFIG).read_text(encoding="utf-8"))
        recorded = {
            name: config.get(name)
            for name in ("format", "upper", "scratch", "scratch_binding")
        }
        if recorded != {
            "format": 8,
            "upper": "ram",
            "scratch": None,
            "scratch_binding": None,
        }:
            raise RuntimeError(f"provision recorded the RAM upper as {recorded!r}")
        self.start(description="first start")
        result = self.exec(
            PROBE,
            "--arg=entry",
            "--arg=limits",
            f"--arg={MEMORY_MAX}",
            f"--arg={PIDS_MAX}",
        )
        self.save_evidence("probe.log", result.stdout + result.stderr)
        for marker in probe_markers(entry=True):
            if marker not in result.stdout:
                raise RuntimeError(
                    f"the managed RAM-upper probe did not report {marker.decode()}"
                )
        self.check_guest_view()
        result = self.exec(
            "/bin/sh",
            "--arg=-c",
            f"--arg=printf 'kept in RAM\\n' >{STATE_PATH}",
            expected=None,
        )
        self.require_output(result, b"", "the RAM upper's state-writing request")
        result = self.exec("/bin/cat", f"--arg={STATE_PATH}", expected=None)
        self.require_output(result, STATE_CONTENT, "the RAM upper's state")
        result = self.exec(
            "/bin/sh", "--arg=-c", f"--arg={CHARGE_SCRIPT}", expected=None
        )
        self.save_evidence("charge.log", result.stdout + result.stderr)
        self.require_output(
            result, CHARGE_MARKER + b"\n", "the memory charge of the RAM upper"
        )
        self.stop_sandbox("first stop")
        # A new start begins with an empty upper.
        self.start(description="start after the stop")
        result = self.exec(
            "/bin/sh", "--arg=-c", f"--arg=test ! -e {STATE_PATH}", expected=None
        )
        self.require_output(result, b"", "the restarted sandbox's empty upper")
        self.stop_sandbox("second stop")
        self.invoke("deprovision", state=self.state)
        if self.state.exists():
            raise RuntimeError("deprovision kept the RAM-upper sandbox's state")

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


def run_sandbox_ram_upper(backend: str, *, timeout: float, output_dir: Path) -> None:
    artifacts = load_workload_artifacts()
    output_dir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="nvx-public-ram-upper-"))
    run_acceptance(
        RamUpperAcceptance(
            backend,
            timeout=timeout,
            output_dir=output_dir,
            artifacts=artifacts,
            root=root,
        )
    )
