"""Public CLI acceptance for the device contract of a sandbox with nested
virtualization.

The scenario runs `nvx-rs sandbox` as subprocesses against the real kernel,
the Alpine control initramfs, and the Ubuntu EROFS layer. `--nested-virt`,
which only `run` and `provision` take, gives the sandbox's microVM nested
virtualization, and its workload `/dev/kvm`: a character device that only root
and the workload's group may open, through which the workload, here the Ubuntu
layer's `nvx-kvm-probe`, runs a VM of its own until its vCPU halts. A sandbox
without it has no `/dev/kvm`, whatever the host offers.

A probe of the host, independent of `nvx-rs` and OpenVMM, decides what
`--nested-virt` must do there: where the host gives a microVM nested
virtualization, a one-shot run and a managed sandbox give the workload its
device; on MSHV, `nvx-rs` refuses it before it records or launches anything;
where KVM does not offer it, or the CPU is not Intel's, `nvx-rs` refuses the
run, and the start of a provisioned sandbox, before it launches anything; and
where WHP does not offer it, OpenVMM refuses it before the guest runs.
"""

from __future__ import annotations

import json
import shutil
import subprocess
import tempfile
from pathlib import Path

from .managed_exec_tests import load_workload_artifacts
from .nested_virtualization import (
    INTEL,
    OPENVMM_WHP_REFUSAL,
    NestedRefusal,
    host_cpu_vendor,
    host_refusal,
)
from .sandbox_lifecycle_tests import (
    CONFIG,
    PROVISIONED_ENTRIES,
    STOPPED_ENTRIES,
    LifecycleAcceptance,
    Refusal,
    check_managed_report,
    managed_report,
    run_acceptance,
)

# The workloads that show what a workload sees of KVM: its device, as `stat`
# shows it, and nvx-kvm-probe, which runs a VM through it. Their arguments are
# kernel command-line tokens in a one-shot run, which hold no whitespace.
STAT = "/usr/bin/stat"
KVM_DEVICE_ARGUMENTS = ("--arg=-c", "--arg=%A:%u:%g:%t:%T", "--arg=/dev/kvm")
PROBE = "/sbin/nvx-kvm-probe"
# The workload's KVM device: the character device 10:232, which root and the
# workload's group, 65534, may open.
WORKLOAD_KVM_DEVICE = b"crw-rw----:0:65534:a:e8"
KVM_PROBE_SUCCESS = b"nvx-kvm-probe: api=12 vm=created vcpu=created exit=hlt"
KVM_PROBE_ABSENT = b"open /dev/kvm: No such file or directory"
NESTED_OPTION = "--nested-virt"
# What a one-shot run's console holds once its guest has run: the kernel's
# banner, and the guest init's readiness, once the workload starts.
GUEST_RAN_MARKERS = (b"Linux version ", b"NVX-SANDBOX-READY:")
OPERATION_REFUSAL = Refusal("unsupported", (NESTED_OPTION, "run or provision"))
MSHV_REFUSAL = Refusal("unsupported", (NESTED_OPTION, "KVM or WHP"))


def require_line(output: bytes, line: bytes, context: str) -> None:
    """Checks that a workload printed ``line``, a whole line of its output."""
    if line not in (entry.rstrip(b"\r") for entry in output.splitlines()):
        raise RuntimeError(f"{context}: the workload did not print {line.decode()!r}")


def nvx_rs_refusal(backend: str) -> Refusal | None:
    """Returns how `nvx-rs` refuses `--nested-virt` before it launches anything
    on this host, which a probe of the host found gives a microVM no nested
    virtualization, or None where OpenVMM refuses it instead: on WHP with an
    Intel CPU, which `nvx-rs` leaves to OpenVMM."""
    if backend == "mshv":
        return MSHV_REFUSAL
    if host_cpu_vendor() != INTEL:
        return Refusal("unsupported", (NESTED_OPTION, "Intel CPU"))
    if backend == "kvm":
        return Refusal("unavailable", (NESTED_OPTION, "nested virtualization"))
    return None


class NestedSandboxAcceptance(LifecycleAcceptance):
    evidence_prefix = "sandbox-nested-virtualization-"

    def run(self) -> None:
        self.invoke(
            "start",
            NESTED_OPTION,
            state=self.state,
            expected=1,
            refusal=OPERATION_REFUSAL,
        )
        if self.state.exists():
            raise RuntimeError("a refused --nested-virt created sandbox state")
        self.check_one_shot(nested=False)
        refusal = host_refusal(self.backend)
        # The log names the path that the host takes.
        print(
            "sandbox-nested-virtualization: "
            + (
                "the host gives nested virtualization"
                if refusal is None
                else f"the host refuses nested virtualization: {refusal.reason}"
            ),
            flush=True,
        )
        if refusal is None:
            self.check_one_shot(nested=True)
            self.check_managed()
        else:
            self.check_refused(refusal)
        self.require_processes((), "after the nested-virtualization sandboxes")

    def one_shot(
        self,
        name: str,
        entrypoint: str,
        *arguments: str,
        nested: bool,
        expected: int | None = 0,
    ) -> subprocess.CompletedProcess[bytes]:
        """Runs ``entrypoint`` with ``arguments`` in a one-shot sandbox, with
        nested virtualization if ``nested``, on a copy of the scratch template
        whose name, ``name``, also names the run's OpenVMM to the process
        checks, and returns its console, the workload's output."""
        scratch = self.root / f"{name}.ext4"
        shutil.copyfile(self.artifacts.scratch_template, scratch)
        options = [
            *self.launch_options(scratch),
            "--entrypoint",
            entrypoint,
            *arguments,
        ]
        if nested:
            options.append(NESTED_OPTION)
        result = self.invoke("run", *options, state=None, expected=expected)
        self.save_evidence(f"{name}.log", result.stdout + result.stderr)
        lines = result.stdout.splitlines()
        command = lines[0].decode(errors="replace") if lines else ""
        if expected is not None and (
            not command.startswith(">> ") or (f" {NESTED_OPTION}" in command) != nested
        ):
            raise RuntimeError(
                f"the one-shot run {name} launched {command!r}, "
                f"{'without' if nested else 'with'} {NESTED_OPTION}"
            )
        self.require_processes((), f"after the one-shot run {name}")
        return result

    def check_one_shot(self, *, nested: bool) -> None:
        """A one-shot workload with nested virtualization runs a VM through its
        /dev/kvm, which only root and its group open; one without it has no
        /dev/kvm."""
        if not nested:
            result = self.one_shot("run-default", PROBE, nested=False, expected=1)
            require_line(
                result.stdout + result.stderr,
                KVM_PROBE_ABSENT,
                "the one-shot run without nested virtualization",
            )
            return
        result = self.one_shot("run-nested", PROBE, nested=True)
        require_line(
            result.stdout,
            KVM_PROBE_SUCCESS,
            "the one-shot run with nested virtualization",
        )
        result = self.one_shot(
            "run-nested-device", STAT, *KVM_DEVICE_ARGUMENTS, nested=True
        )
        require_line(
            result.stdout,
            WORKLOAD_KVM_DEVICE,
            "the one-shot run with nested virtualization",
        )

    def check_managed(self) -> None:
        """A managed sandbox records nested virtualization, and each start gives
        its workloads their device."""
        shutil.copyfile(self.artifacts.scratch_template, self.scratch)
        self.invoke(
            "provision",
            *self.launch_options(self.scratch),
            NESTED_OPTION,
            state=self.state,
        )
        self.require_entries(self.state, PROVISIONED_ENTRIES, "after provision")
        config = json.loads((self.state / CONFIG).read_text(encoding="utf-8"))
        recorded = {name: config.get(name) for name in ("format", "nested_virt")}
        if recorded != {"format": 9, "nested_virt": True}:
            raise RuntimeError(
                f"provision recorded nested virtualization as {recorded!r}"
            )
        self.start(description="nested start")
        context = "the managed sandbox with nested virtualization"
        result = self.exec(STAT, *KVM_DEVICE_ARGUMENTS)
        self.save_evidence("managed-device.log", result.stdout + result.stderr)
        require_line(result.stdout, WORKLOAD_KVM_DEVICE, context)
        result = self.exec(PROBE)
        self.save_evidence("managed-probe.log", result.stdout + result.stderr)
        require_line(result.stdout, KVM_PROBE_SUCCESS, context)
        self.invoke("stop", state=self.state)
        self.require_entries(self.state, STOPPED_ENTRIES, "after the nested stop")
        check_managed_report(managed_report(self.state), self.backend)
        self.invoke("deprovision", state=self.state)
        if self.state.exists():
            raise RuntimeError("deprovision kept the nested sandbox's state")

    def check_refused(self, refusal: NestedRefusal) -> None:
        """Where the host gives a microVM no nested virtualization, `nvx-rs`
        refuses it before it launches anything, or else OpenVMM refuses it
        before the guest runs."""
        expected = nvx_rs_refusal(self.backend)
        if expected is None:
            result = self.one_shot("run-refused", PROBE, nested=True, expected=None)
            console = result.stdout + result.stderr
            ran = [marker for marker in GUEST_RAN_MARKERS if marker in console]
            if ran:
                raise RuntimeError(
                    "OpenVMM refused nested virtualization only after the guest "
                    f"ran: the console holds {ran[0].decode()!r}"
                )
            if (
                result.returncode == 0
                or OPENVMM_WHP_REFUSAL.encode() not in console
                or KVM_PROBE_SUCCESS in console
            ):
                raise RuntimeError(
                    f"OpenVMM did not refuse nested virtualization ({refusal.reason}) "
                    f"with {OPENVMM_WHP_REFUSAL!r}: status {result.returncode}"
                )
            return
        scratch = self.root / "run-refused.ext4"
        shutil.copyfile(self.artifacts.scratch_template, scratch)
        self.invoke(
            "run",
            *self.launch_options(scratch),
            NESTED_OPTION,
            state=None,
            expected=1,
            refusal=expected,
        )
        shutil.copyfile(self.artifacts.scratch_template, self.scratch)
        if expected is MSHV_REFUSAL:
            self.invoke(
                "provision",
                *self.launch_options(self.scratch),
                NESTED_OPTION,
                state=self.state,
                expected=1,
                refusal=expected,
            )
            if self.state.exists():
                raise RuntimeError("a refused nested provision created sandbox state")
            return
        # A provision launches nothing, and records the request, which each
        # start checks against the host again.
        self.invoke(
            "provision",
            *self.launch_options(self.scratch),
            NESTED_OPTION,
            state=self.state,
        )
        self.invoke("start", state=self.state, expected=1, refusal=expected)
        self.require_entries(
            self.state, PROVISIONED_ENTRIES, "after the refused nested start"
        )
        self.invoke("deprovision", state=self.state)
        if self.state.exists():
            raise RuntimeError("deprovision kept the refused nested sandbox's state")


def run_sandbox_nested_virtualization(
    backend: str, *, timeout: float, output_dir: Path
) -> None:
    artifacts = load_workload_artifacts()
    output_dir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="nvx-public-nested-"))
    run_acceptance(
        NestedSandboxAcceptance(
            backend,
            timeout=timeout,
            output_dir=output_dir,
            artifacts=artifacts,
            root=root,
        )
    )
