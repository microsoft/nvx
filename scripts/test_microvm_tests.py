#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import argparse
import ast
import errno
import functools
import hashlib
import inspect
import io
import ipaddress
import itertools
import json
import os
import queue
import re
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from collections.abc import Callable, Collection, Sequence
from contextlib import ExitStack, nullcontext, redirect_stderr, redirect_stdout
from datetime import datetime
from pathlib import Path
from typing import Any, cast
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import (  # noqa: E402
    adversarial_executor,
    benchmark,
    common,
    managed_exec_tests,
    microvm_durations,
    microvm_shards,
    microvm_tests,
    nvx_rs,
    openvmm_process,
    sandbox_lifecycle_tests,
    sandbox_proxy_tests,
    sandbox_ram_upper_tests,
    time_abi,
)
from nvx_tools.build_constants import (  # noqa: E402
    BuildConstants,
)

# When this process loaded the tests, on the monotonic clock.
_LOADED = time.monotonic()


def _compile(policy: dict[str, object]) -> nvx_rs.EgressRules:
    """Compiles an egress policy with nvx-rs, as nvx-rs run compiles its
    --network-egress-policy-file."""
    with tempfile.TemporaryDirectory(prefix="nvx-egress-policy-") as temporary:
        path = Path(temporary) / "policy.json"
        path.write_text(json.dumps(policy), encoding="utf-8")
        return nvx_rs.compile_egress_policy(path)


# The nvx-rs that a scenario runs, in place of the one that nvx_rs finds or
# builds, where the test compiles no egress policy, and the start of the
# command line of the scenario's public runs.
_FAKE_NVX_RS = Path("nvx-rs")
_FAKE_PUBLIC_RUN = [os.fspath(_FAKE_NVX_RS), "--root", os.fspath(nvx_rs.ROOT), "run"]


def _posix_shell() -> str | None:
    shell = shutil.which("sh")
    if shell is None:
        git = shutil.which("git")
        git_shell = Path(git).parent.parent / "bin" / "sh.exe" if git else None
        if git_shell is not None and git_shell.is_file():
            shell = str(git_shell)
    return shell


def _probe_environment(probe_dir: Path) -> dict[str, str]:
    """Return the environment of a run of an egress script in the host's
    shell, which then keeps its probes' results in `probe_dir`.

    The scripts keep the results in TMPDIR, or else in /tmp, which is private
    to a guest but shared on the host: runs there at once would delete and
    overwrite each other's results (#1007)."""
    return {**os.environ, "TMPDIR": str(probe_dir)}


def _bdi_stats(dirty: int, io: int = 0, more_io: int = 0) -> str:
    """Return a backing device's debugfs statistics whose writeback lists hold
    `dirty`, `io`, and `more_io` inodes, in the kernel's layout."""
    return (
        "BdiWriteback:                0 kB\n"
        f"BdiReclaimable:     {4 * (dirty + io + more_io):10d} kB\n"
        f"b_dirty:            {dirty:10d}\n"
        f"b_io:               {io:10d}\n"
        f"b_more_io:          {more_io:10d}\n"
        "b_dirty_time:                0\n"
    )


# A mount table with one host share, bound below its root and mounted whole,
# and a block device's mount, which is no host share.
_ONE_SHARE_MOUNTINFO = (
    "22 1 0:20 / /proc rw,nosuid shared:5 - proc proc rw\n"
    "30 25 0:42 /0 /work\\040space rw,nosuid,nodev - virtiofs microvm rw\n"
    "31 25 0:42 / /mnt/share rw,nosuid,nodev shared:9 - virtiofs microvm rw\n"
    "60 25 8:1 / /data rw - ext4 /dev/vda rw\n"
)
# The debugfs statistics of the share of _ONE_SHARE_MOUNTINFO.
_ONE_SHARE_STATS = "/sys/kernel/debug/bdi/0:42/stats"


# The line that releases the private-network ingress guest after the host's
# probes.
PRIVATE_INGRESS_RELEASE = microvm_tests.PRIVATE_INGRESS_CHECKED + b"\n"


# What `/usr/bin/env` prints in the emulated workload without a supplied environment
# or working directory.
WORKLOAD_DEFAULT_ENVIRONMENT = (
    b"PATH=/usr/sbin:/usr/bin:/sbin:/bin\n"
    b"TERM=linux\nHOME=/nonexistent\nUSER=nobody\nLOGNAME=nobody\n"
    b"SHLVL=1\nPWD=/\nnvx_workload_uid=65534\nnvx_hostname=nvx\n"
)


def _layered(environment: bytes, entries: list[str]) -> bytes:
    """Applies entries over an `env` listing the way the guest agent's putenv does."""
    variables: dict[str, str] = {}
    for entry in [*environment.decode().splitlines(), *entries]:
        name, _, value = entry.partition("=")
        variables[name] = value
    return "".join(f"{name}={value}\n" for name, value in variables.items()).encode()


def _vp_binding_profile(vp_indices: list[int]) -> bytes:
    fields = "duration_ns=1 process_elapsed_ns=2 pid=3"
    phases = [
        "vp_bind_bsp" if index == 0 else f"vp_bind_ap_{index}" for index in vp_indices
    ]
    lines = [
        f"OPENVMM_SNAPSHOT_PROFILE_V1 operation=startup phase={phase} "
        f"exclusive=0 {fields}"
        for phase in phases
    ]
    lines.append(
        "OPENVMM_SNAPSHOT_PROFILE_V1 operation=startup phase=vp_thread_bind "
        f"exclusive=1 {fields}"
    )
    return "".join(f"{line}\r\n" for line in lines).encode()


def _restore_target(command: list[str]) -> int | None:
    if "--restore-processors" not in command:
        return None
    return int(command[command.index("--restore-processors") + 1])


def _scenario_runners() -> list[str]:
    """Return the names of the microVM test module's run_* functions, among
    them every scenario function that its runner calls."""
    return [
        name
        for name, value in vars(microvm_tests).items()
        if name.startswith("run_") and callable(value)
    ]


def _read_report(path: Path) -> list[dict[str, Any]]:
    """Return the records of a scenario duration report, in order."""
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


# OpenVMM's warning of a cold boot whose --cpu-profile auto falls back to a
# host profile (openvmm_core's time_abi host_profile_warning), on stderr.
FALLBACK_WARNING = (
    "   0.012345s  WARN openvmm_core::worker::dispatch::time_abi: "
    "NVX-CPU-PROFILE-FALLBACK: no built-in CPU profile serves this host's CPU, "
    "GenuineIntel 6/106/6 (Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz), so "
    f"--cpu-profile auto fell back to intel.host.v1 (sha256:{'cd' * 32}), a "
    "profile derived from this host's kvm backend for development.\n"
    "It is not pinned: a microcode, firmware, hypervisor, or OS update can change "
    "it. Each cold boot fingerprints the backend first, which took 14.2 ms here, "
    "and its snapshots restore only on hosts of the same CPU model and stepping "
    "whose hypervisor supports it.\n"
    "Help NVX add a built-in profile for this CPU: run\n"
    "  openvmm --hypervisor kvm --cpu-fingerprint fingerprint.json\n"
    "and attach fingerprint.json to a CPU profile request:\n"
    f"  {time_abi.CPU_PROFILE_REQUEST_URL}&signature=GenuineIntel%206%2F106%2F6\n"
).encode()


def _declared(cpu_profile: bytes) -> bytes:
    """Return OpenVMM's record of the rates that a partition with
    ``cpu_profile`` declares, on stderr."""
    return (
        b"   0.023456s  INFO openvmm_core::worker::dispatch::time_abi: time ABI "
        b'rates declared hypervisor="kvm" cpu_profile="' + cpu_profile + b'" '
        b"msr_route=ExitToVmm sync=CommonOffset native_tsc_hz=0xa680b7c8\n"
    )


def _warp_probe_output(cpus: int, *, offset_ns: int = 40) -> bytes:
    """Return the console output of the idle-inducing warp schedule."""
    pairs = cpus * (cpus - 1) // 2
    probe_round = (
        f"NVX-TIME-PROBE warp max_backward_cycles=0 max_backward_ns=0 "
        f"max_abs_offset_ns={offset_ns} pairs={pairs} verdict=PASS bound_ns=1000\r\n"
        "NVX-TIME-PROBE warp-detail max_abs_offset_cycles=1.0 "
        "max_uncertainty_ns=30 max_skew_bound_ns=50 total_warps=0 "
        f"inconsistent_pairs=0 stalled_pairs=0 cpus=0-{cpus - 1} duration_ms=100 "
        "tsc_hz=2793437000 tsc_hz_source=cpuid conclusive=1\r\n"
    )
    return (probe_round * time_abi.warp_rounds(cpus) + "NVX-WARP-PROBE-OK\r\n").encode()


def _guest_script_result(
    output: str, *, transcript: str = ""
) -> microvm_tests.GuestScriptResult:
    """Return what run_guest_script returns for a staged script that printed
    ``output``, after the console printed ``transcript`` and the start line."""
    start = microvm_tests.GUEST_SCRIPT_START_MARKER.decode()
    return microvm_tests.GuestScriptResult(
        text=f"{transcript}{start}\r\n{output}",
        wall_ms=0.0,
        peak_rss_bytes=0,
        output=output,
    )


def _restore_status_output(
    backend: str, cpus: int, *, boot_cpus: int | None = None
) -> bytes:
    """Return what the post-restore ``nvx-time status`` query prints: every
    recorded phase, oldest first, with the values from when its check ran,
    then the runtime line."""
    boot_cpus = cpus if boot_cpus is None else boot_cpus
    rates = f"tsc_hz=2793437000 lapic_hz={time_abi.LAPIC_HZ[backend]}"
    return (
        f"NVX-TIME-ABI: v=1 phase=boot status=ok cpus={boot_cpus} {rates} "
        "generation=0 elapsed_us=2390\r\n"
        f"NVX-TIME-ABI: v=1 phase=capture status=ok cpus={boot_cpus} {rates} "
        "generation=0 elapsed_us=95\r\n"
        f"NVX-TIME-ABI: v=1 phase=restore status=ok cpus={cpus} {rates} "
        "generation=1 elapsed_us=310\r\n"
        "NVX-TIME-ABI: v=1 phase=runtime status=synchronized generation=1 "
        "discontinuities=1 offset_ns=-1200 uncertainty_ns=900 rejected_samples=0 "
        "last_sample_error=none\r\n"
        "NVX-TIME-STATUS-EXIT status=0\r\n"
    ).encode()


def _canceled_capture_output(
    backend: str,
    *,
    rcu: str | None = "0",
    restored: bool = False,
    generation: int = 0,
    status: int | None = 0,
) -> bytes:
    """Return what the guest prints for the checks after a released snapshot
    request: the status query's lines, then the RCU suppression flag."""
    rates = f"tsc_hz=2793437000 lapic_hz={time_abi.LAPIC_HZ[backend]}"
    lines = [
        f"NVX-TIME-ABI: v=1 phase=boot status=ok cpus=1 {rates} "
        f"generation={generation} elapsed_us=2390"
    ]
    if restored:
        lines.append(
            f"NVX-TIME-ABI: v=1 phase=restore status=ok cpus=1 {rates} "
            "generation=1 elapsed_us=310"
        )
    lines.append(
        "NVX-TIME-ABI: v=1 phase=runtime status=synchronized generation=0 "
        "discontinuities=0 offset_ns=-1200 uncertainty_ns=900 rejected_samples=0 "
        "last_sample_error=none"
    )
    if status is not None:
        lines.append(f"NVX-TIME-STATUS-EXIT status={status}")
    if rcu is not None:
        lines.append(f"NVX-CANCELED-CAPTURE-RCU {rcu}")
    lines.append("NVX-CANCELED-CAPTURE-DONE")
    return "".join(f"{line}\r\n" for line in lines).encode()


def _restore_processors_measure(
    backend: str,
    *,
    mshv_prefix: bool = True,
    warp_offset_ns: int = 40,
    capacity: int = 8,
    prefix: bytes = b"",
) -> MagicMock:
    def measure(command: list[str], **kwargs: object) -> None:
        target = _restore_target(command)
        online = 1 if target is None else target
        vp_count = capacity
        if mshv_prefix and backend == "mshv" and target is not None:
            vp_count = target
        log_path = cast(Path, kwargs["log_path"])
        log_path.parent.mkdir(parents=True, exist_ok=True)
        log_path.write_bytes(
            prefix
            + f"NVX-RESTORE-PROCESSORS-OK count={online}\r\n".encode()
            + _warp_probe_output(online, offset_ns=warp_offset_ns)
            # The snapshot's guest booted with one online CPU.
            + _restore_status_output(backend, online, boot_cpus=1)
            + benchmark.RESTORE_MARKER
            + b"\r\n"
            + _vp_binding_profile(list(range(vp_count)))
        )

    return MagicMock(side_effect=measure)


def _fresh_openvmm_command(*_args: object, **_kwargs: object) -> list[str]:
    # A new list for every call, which the scenario extends with its policy.
    return ["openvmm"]


class HostLoopbackHttpFixtureTests(unittest.TestCase):
    def _exchange(self, pieces: list[bytes]) -> tuple[bytes, list[Exception]]:
        """Send the request in pieces with a gap, and return the reply and fixture errors."""
        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.addCleanup(listener.close)
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        errors: list[Exception] = []
        server = threading.Thread(
            target=microvm_tests._http_server,
            args=(listener, b"/proxy", b"NVX-BODY", 2.0, errors),
            daemon=True,
        )
        server.start()
        response = b""
        with socket.create_connection(listener.getsockname(), timeout=5) as client:
            for index, piece in enumerate(pieces):
                if index:
                    time.sleep(0.25)
                client.sendall(piece)
            while chunk := client.recv(4096):
                response += chunk
        server.join(5)
        return response, errors

    def test_a_single_send_request_is_answered(self) -> None:
        response, errors = self._exchange(
            [b"GET /proxy HTTP/1.1\r\nHost: localhost\r\n\r\n"]
        )
        self.assertTrue(response.startswith(b"HTTP/1.1 200 OK"), response)
        self.assertEqual(errors, [])

    def test_a_request_split_across_reads_is_answered(self) -> None:
        response, errors = self._exchange(
            [b"GET ", b"/proxy HTTP/1.1\r\nHost: localhost\r\n\r\n"]
        )
        self.assertTrue(response.startswith(b"HTTP/1.1 200 OK"), response)
        self.assertEqual(errors, [])


class MicrovmTestParserTests(unittest.TestCase):
    def test_parser_defaults_to_all_correctness_scenarios(self):
        args = nvx.parse_args(["test-microvm", "--backend", "mshv"])

        self.assertIsNone(args.scenario)
        self.assertEqual(args.processors, [1, 2, 4, 8])
        self.assertEqual(args.guest, "alpine")
        self.assertIsNone(args.memory_mib)
        self.assertEqual(args.timeout, 60.0)
        self.assertIs(args.handler, microvm_tests.run)

    def test_parser_accepts_selected_scenarios_and_processors(self):
        args = nvx.parse_args(
            [
                "test-microvm",
                "--backend",
                "whp",
                "--scenario",
                "smp",
                "--processors",
                "2",
                "8",
                "--output-dir",
                "results",
            ]
        )

        self.assertEqual(args.scenario, ["smp"])
        self.assertEqual(args.processors, [2, 8])
        self.assertEqual(args.output_dir, Path("results"))

    def test_parser_accepts_ubuntu_guest_profile(self):
        args = nvx.parse_args(
            [
                "test-microvm",
                "--backend",
                "whp",
                "--guest",
                "ubuntu",
                "--scenario",
                "guest-boot",
            ]
        )

        self.assertEqual(args.guest, "ubuntu")
        self.assertIsNone(args.memory_mib)


class PublicManagedExecAcceptanceTests(unittest.TestCase):
    def _run_acceptance(
        self,
        root: Path,
        *,
        bad_pwd_output: bool = False,
        malformed_outcome: bool = False,
        outcome_mutation: str | None = None,
        start_returncode: int = 0,
        stop_returncode: int = 0,
        default_environment: bytes = WORKLOAD_DEFAULT_ENVIRONMENT,
        layered_environment: bytes | None = None,
        working_environment: bytes | None = None,
        evidence_failure: bool = False,
        command_log: list[list[str]] | None = None,
    ) -> tuple[list[list[str]], list[dict[str, object]]]:
        distro = root / "ubuntu-distro.erofs"
        distro.write_bytes(b"distro")
        distro.with_name("ubuntu-distro.erofs.manifest.json").write_text(
            json.dumps({"uuid": "12345678-1234-1234-1234-123456789abc"}),
            encoding="utf-8",
        )
        scratch = root / "ubuntu-smoke-scratch.ext4"
        scratch.write_bytes(b"scratch")
        output_dir = root / "results"
        commands: list[list[str]] = command_log if command_log is not None else []
        options: list[dict[str, object]] = []
        real_copyfile = shutil.copyfile

        def artifact(name: str) -> Path:
            return {
                "ubuntu-distro.erofs": distro,
                "ubuntu-smoke-scratch.ext4": scratch,
            }[name]

        def require(path: Path, _description: str) -> Path:
            return path

        def copyfile(source: Path | str, destination: Path | str) -> Path | str:
            if (
                evidence_failure
                and Path(destination) == output_dir / "public-exec-openvmm.log"
            ):
                raise OSError("injected evidence failure")
            return real_copyfile(source, destination)

        def run(
            command: list[str], **kwargs: object
        ) -> subprocess.CompletedProcess[bytes]:
            commands.append(command)
            options.append(kwargs)
            operation = command[4]
            state = Path(command[command.index("--state-dir") + 1])
            if operation == "provision":
                state.mkdir(parents=True, exist_ok=True)
            if operation == "start":
                state.mkdir(parents=True, exist_ok=True)
                (state / "openvmm.log").write_text("bounded log\n", encoding="utf-8")
                if start_returncode:
                    return subprocess.CompletedProcess(
                        command, start_returncode, b"", b"start failed"
                    )
            if operation == "stop":
                return subprocess.CompletedProcess(
                    command, stop_returncode, b"", b"stop failed"
                )
            if operation == "deprovision":
                if stop_returncode:
                    return subprocess.CompletedProcess(
                        command,
                        1,
                        b"",
                        b"error[sandbox-running]: sandbox must be stopped before "
                        b"deprovision\n",
                    )
                shutil.rmtree(state)
            if operation != "exec":
                return subprocess.CompletedProcess(command, 0, b"", b"")

            cwd = command[command.index("--cwd") + 1] if "--cwd" in command else "/"
            timeout_ms = (
                command[command.index("--exec-timeout-ms") + 1]
                if "--exec-timeout-ms" in command
                else "0"
            )
            if not cwd.startswith("/"):
                return subprocess.CompletedProcess(
                    command,
                    1,
                    b"",
                    b"error[unsupported]: managed exec working directory must be an "
                    b"absolute path of at most 4096 bytes\n",
                )
            if int(timeout_ms) < 0 or int(timeout_ms) > 0xFFFFFFFF:
                return subprocess.CompletedProcess(
                    command,
                    2,
                    b"",
                    b"usage: nvx-rs sandbox [{run,provision,start,exec,stop,"
                    b"deprovision}] [OPTIONS]\n"
                    b"nvx-rs sandbox: error[argument-invalid]: argument "
                    b"--exec-timeout-ms: expected an ASCII decimal integer between 0 "
                    b"and 4294967295, not " + f'"{timeout_ms}"'.encode() + b"\n",
                )

            entrypoint = command[command.index("--entrypoint") + 1]
            stdout = b""
            stderr = b""
            returncode = 0
            category = "exit"
            if entrypoint == "/bin/pwd":
                stdout = (
                    b"x" * (managed_exec_tests.DIAGNOSTIC_LIMIT + 10)
                    if bad_pwd_output
                    else f"{cwd}\n".encode()
                )
            elif entrypoint == "/usr/bin/env":
                entries: list[str] | None = None
                if "--environment-file" in command:
                    environment_file = Path(
                        command[command.index("--environment-file") + 1]
                    )
                    entries = json.loads(environment_file.read_text(encoding="utf-8"))
                elif "--environment" in command:
                    entries = [
                        command[index + 1]
                        for index, value in enumerate(command)
                        if value == "--environment"
                    ]
                # Default and layered environments name the working directory in
                # PWD before any layered entry applies; a replacement controls it.
                if entries is None and "--cwd" not in command:
                    stdout = default_environment
                elif entries is None:
                    stdout = (
                        working_environment
                        if working_environment is not None
                        else _layered(default_environment, [f"PWD={cwd}"])
                    )
                elif "--inherit-default-environment" in command:
                    stdout = (
                        layered_environment
                        if layered_environment is not None
                        else _layered(
                            WORKLOAD_DEFAULT_ENVIRONMENT, [f"PWD={cwd}", *entries]
                        )
                    )
                else:
                    stdout = (
                        b"" if not entries else ("\n".join(entries) + "\n").encode()
                    )
            elif entrypoint == "/usr/bin/getent":
                stdout = b"nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n"
            elif entrypoint == "/bin/sleep":
                returncode = 124
                category = "timeout"
            elif cwd in ("/does-not-exist", "/etc/passwd", "/root"):
                returncode = 125
                stderr = b"cannot use working directory\n"
            elif entrypoint == "/bin/sh":
                stdout = b"public stdout"
                stderr = b"public stderr"
                returncode = 7

            if "--outcome-report" in command:
                report = Path(command[command.index("--outcome-report") + 1])
                payload: dict[str, object] = {
                    "schema_version": 1,
                    "operation_id": "1" * 32,
                    "outcome": {
                        "operation": "exec",
                        "category": category,
                        "status_code": True if malformed_outcome else returncode,
                    },
                }
                if outcome_mutation == "extra-top-level":
                    payload["extra"] = "unexpected"
                elif outcome_mutation == "missing-top-level":
                    del payload["operation_id"]
                elif outcome_mutation == "extra-outcome":
                    cast(dict[str, object], payload["outcome"])["extra"] = "unexpected"
                elif outcome_mutation == "missing-outcome":
                    del cast(dict[str, object], payload["outcome"])["category"]
                elif outcome_mutation == "float-schema":
                    payload["schema_version"] = 1.0
                elif outcome_mutation == "float-status":
                    cast(dict[str, object], payload["outcome"])["status_code"] = float(
                        returncode
                    )
                report.write_text(
                    json.dumps(payload),
                    encoding="utf-8",
                )
            return subprocess.CompletedProcess(command, returncode, stdout, stderr)

        with (
            patch.object(managed_exec_tests, "artifact_path", side_effect=artifact),
            patch.object(managed_exec_tests, "require_file", side_effect=require),
            patch.object(managed_exec_tests.subprocess, "run", side_effect=run),
            patch.object(managed_exec_tests.shutil, "copyfile", side_effect=copyfile),
            patch.object(
                managed_exec_tests.nvx_rs, "require_binary", return_value=Path("nvx-rs")
            ),
        ):
            managed_exec_tests.run_managed_exec_configuration(
                "whp", timeout=2, output_dir=output_dir
            )
        return commands, options

    def test_public_acceptance_observes_full_cli_contract(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            commands, options = self._run_acceptance(root)
            records = json.loads(
                (root / "results" / "public-exec-checks.json").read_text(
                    encoding="utf-8"
                )
            )
            exit_outcome = json.loads(
                (root / "results" / "public-exec-exit-outcome.json").read_text(
                    encoding="utf-8"
                )
            )
            timeout_outcome = json.loads(
                (root / "results" / "public-exec-timeout-outcome.json").read_text(
                    encoding="utf-8"
                )
            )

        exec_commands = [command for command in commands if command[4] == "exec"]
        self.assertTrue(
            all(
                command[:4] == ["nvx-rs", "--root", str(nvx_rs.ROOT), "sandbox"]
                for command in commands
            )
        )
        self.assertTrue(
            all(
                option
                == {
                    "cwd": BuildConstants.REPO_ROOT,
                    "capture_output": True,
                    "timeout": 12,
                }
                for option in options
            )
        )
        state_directories = {
            command[command.index("--state-dir") + 1] for command in commands
        }
        self.assertEqual(len(state_directories), 1)
        self.assertTrue(
            any(
                "--cwd" in command and command[command.index("--cwd") + 1] == "relative"
                for command in exec_commands
            )
        )
        for value in ("-1", str(0x100000000)):
            self.assertTrue(
                any(
                    "--exec-timeout-ms" in command
                    and command[command.index("--exec-timeout-ms") + 1] == value
                    for command in exec_commands
                )
            )
        self.assertTrue(any("--outcome-report" in command for command in exec_commands))
        self.assertTrue(
            any("--inherit-default-environment" in command for command in exec_commands)
        )
        self.assertEqual(
            [record["operation"] for record in records[:2]], ["provision", "start"]
        )
        self.assertEqual(records[-1]["operation"], "deprovision")
        self.assertTrue(all(record["state_exists"] for record in records[:-1]))
        self.assertFalse(records[-1]["state_exists"])
        self.assertTrue(all(isinstance(record.get("argv"), list) for record in records))
        recorded_argv = [value for record in records for value in record["argv"]]
        self.assertIn("<redacted>", recorded_argv)
        self.assertNotIn("SECOND=inline value", recorded_argv)
        self.assertNotIn("LAYERED=layered value", recorded_argv)
        self.assertEqual(exit_outcome["outcome"]["category"], "exit")
        self.assertEqual(exit_outcome["outcome"]["status_code"], 7)
        self.assertEqual(timeout_outcome["outcome"]["category"], "timeout")
        self.assertEqual(timeout_outcome["outcome"]["status_code"], 124)
        fixture_root = Path(state_directories.pop()).parent
        self.assertFalse(fixture_root.exists())

    def test_public_acceptance_rejects_unreadable_layer_manifest(self):
        with tempfile.TemporaryDirectory() as temporary:
            manifest = Path(temporary) / "ubuntu-distro.erofs.manifest.json"
            manifest.write_text("{", encoding="utf-8")
            paths = iter(
                (
                    Path(temporary) / "ubuntu-distro.erofs",
                    manifest,
                    Path(temporary) / "ubuntu-smoke-scratch.ext4",
                )
            )

            def artifact(_: str) -> Path:
                return next(paths)

            def require(path: Path, _: str) -> Path:
                return path

            with (
                patch.object(
                    managed_exec_tests,
                    "artifact_path",
                    side_effect=artifact,
                ),
                patch.object(
                    managed_exec_tests,
                    "require_file",
                    side_effect=require,
                ),
                self.assertRaisesRegex(
                    common.ScriptError, "invalid Ubuntu layer manifest"
                ),
            ):
                managed_exec_tests.run_managed_exec_configuration(
                    "whp", timeout=2, output_dir=Path(temporary) / "results"
                )

    def test_public_acceptance_preserves_test_and_cleanup_failures(self):
        commands: list[list[str]] = []
        fixture_root: Path | None = None
        try:
            with tempfile.TemporaryDirectory() as temporary:
                with self.assertRaisesRegex(
                    RuntimeError,
                    "unexpected output.*cleanup.*stop failed",
                ) as raised:
                    self._run_acceptance(
                        Path(temporary),
                        bad_pwd_output=True,
                        stop_returncode=9,
                        command_log=commands,
                    )
            fixture_root = Path(
                commands[0][commands[0].index("--state-dir") + 1]
            ).parent
            self.assertIn("10 bytes omitted", str(raised.exception))
            self.assertLess(
                len(str(raised.exception)), managed_exec_tests.DIAGNOSTIC_LIMIT + 512
            )
        finally:
            if fixture_root is None and commands:
                fixture_root = Path(
                    commands[0][commands[0].index("--state-dir") + 1]
                ).parent
            if fixture_root is not None and fixture_root.exists():
                shutil.rmtree(fixture_root)

    def test_public_acceptance_rejects_malformed_outcome_packet(self):
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaisesRegex(
                RuntimeError, "outcome has unexpected typed fields"
            ):
                self._run_acceptance(Path(temporary), malformed_outcome=True)

    def test_public_acceptance_rejects_outcome_shape_mutations(self):
        for mutation in (
            "extra-top-level",
            "missing-top-level",
            "extra-outcome",
            "missing-outcome",
            "float-schema",
            "float-status",
        ):
            with (
                self.subTest(mutation=mutation),
                tempfile.TemporaryDirectory() as temporary,
            ):
                with self.assertRaisesRegex(
                    RuntimeError, "outcome has unexpected typed fields"
                ):
                    self._run_acceptance(Path(temporary), outcome_mutation=mutation)

    def test_public_acceptance_rejects_default_environment_mutations(self):
        valid = {
            "PATH": "/usr/sbin:/usr/bin:/sbin:/bin",
            "TERM": "linux",
            "HOME": "/nonexistent",
            "USER": "nobody",
            "LOGNAME": "nobody",
            "SHLVL": "1",
            "PWD": "/",
            "nvx_workload_uid": "65534",
        }
        required_names = ("PATH", "TERM", "HOME", "USER", "LOGNAME", "PWD")
        mutations = [
            *[
                (f"wrong-{name}", {**valid, name: f"wrong-{value}"})
                for name, value in valid.items()
                if name in required_names
            ],
            *[
                (
                    f"missing-{missing}",
                    {name: value for name, value in valid.items() if name != missing},
                )
                for missing in required_names
            ],
            *[
                (f"leaked-{name}", {**valid, name: "leaked"})
                for name in (
                    "EMPTY",
                    "COMPLEX",
                    "SECOND",
                    "ORDER",
                    "LAYERED",
                )
            ],
        ]
        for mutation, environment in mutations:
            output = "".join(
                f"{name}={value}\n" for name, value in environment.items()
            ).encode()
            with (
                self.subTest(mutation=mutation),
                tempfile.TemporaryDirectory() as temporary,
            ):
                with self.assertRaisesRegex(
                    RuntimeError, "did not match workload defaults"
                ):
                    self._run_acceptance(Path(temporary), default_environment=output)

    def test_public_acceptance_allows_unrelated_bootstrap_environment(self):
        environment = (
            b"PATH=/usr/sbin:/usr/bin:/sbin:/bin\n"
            b"TERM=linux\nHOME=/nonexistent\nUSER=nobody\nLOGNAME=nobody\n"
            b"SHLVL=1\nPWD=/\nnvx_layer=distro\nnvx_workload_uid=65534\n"
            b"nvx_workload_gid=65534\nnvx_hostname=nvx\n"
        )
        with tempfile.TemporaryDirectory() as temporary:
            self._run_acceptance(Path(temporary), default_environment=environment)

    def test_public_acceptance_rejects_layered_environment_mutations(self):
        entries = ["LAYERED=layered value", "TERM=layered"]
        layered = _layered(WORKLOAD_DEFAULT_ENVIRONMENT, entries)
        mutations = {
            "replaced-defaults": ("\n".join(entries) + "\n").encode(),
            "kept-replaced-default": _layered(
                WORKLOAD_DEFAULT_ENVIRONMENT, entries[:1]
            ),
            "missing-entry": _layered(WORKLOAD_DEFAULT_ENVIRONMENT, entries[1:]),
            "wrong-entry": _layered(
                WORKLOAD_DEFAULT_ENVIRONMENT, ["LAYERED=other", "TERM=layered"]
            ),
            "leaked-earlier-entry": layered + b"SECOND=leaked\n",
        }
        for mutation, environment in mutations.items():
            with (
                self.subTest(mutation=mutation),
                tempfile.TemporaryDirectory() as temporary,
            ):
                with self.assertRaisesRegex(
                    RuntimeError, "did not layer entries over workload defaults"
                ):
                    self._run_acceptance(
                        Path(temporary), layered_environment=environment
                    )
        with tempfile.TemporaryDirectory() as temporary:
            self._run_acceptance(Path(temporary), layered_environment=layered)

    def test_public_acceptance_rejects_working_directory_environment_mutations(self):
        working = _layered(WORKLOAD_DEFAULT_ENVIRONMENT, ["PWD=/tmp"])
        mutations = {
            # With sandbox layers, the launch helper once left PWD at `/` (#373).
            "root-PWD": WORKLOAD_DEFAULT_ENVIRONMENT,
            "missing-PWD": working.replace(b"PWD=/tmp\n", b""),
            "replaced-defaults": b"PWD=/tmp\n",
        }
        for mutation, environment in mutations.items():
            with (
                self.subTest(mutation=mutation),
                tempfile.TemporaryDirectory() as temporary,
            ):
                with self.assertRaisesRegex(
                    RuntimeError, "did not point PWD at the working directory"
                ):
                    self._run_acceptance(
                        Path(temporary), working_environment=environment
                    )
        with tempfile.TemporaryDirectory() as temporary:
            self._run_acceptance(Path(temporary), working_environment=working)

    def test_evidence_failure_still_deprovisions_stopped_sandbox(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            commands: list[list[str]] = []
            with self.assertRaisesRegex(RuntimeError, "injected evidence failure"):
                self._run_acceptance(
                    root,
                    evidence_failure=True,
                    command_log=commands,
                )
            records = json.loads(
                (root / "results" / "public-exec-checks.json").read_text(
                    encoding="utf-8"
                )
            )

        self.assertIn("deprovision", [command[4] for command in commands])
        self.assertEqual(records[-1]["operation"], "deprovision")
        fixture_root = Path(commands[0][commands[0].index("--state-dir") + 1]).parent
        self.assertFalse(fixture_root.exists())

    def test_failed_start_deprovisions_safely_stopped_sandbox(self):
        with tempfile.TemporaryDirectory() as temporary:
            commands: list[list[str]] = []
            with self.assertRaisesRegex(RuntimeError, "start failed"):
                self._run_acceptance(
                    Path(temporary),
                    start_returncode=9,
                    command_log=commands,
                )

        self.assertIn("deprovision", [command[4] for command in commands])
        fixture_root = Path(commands[0][commands[0].index("--state-dir") + 1]).parent
        self.assertFalse(fixture_root.exists())

    def test_failed_stop_attempts_guarded_deprovision(self):
        with tempfile.TemporaryDirectory() as temporary:
            commands: list[list[str]] = []
            with self.assertRaisesRegex(
                RuntimeError,
                "stop failed.*sandbox must be stopped before deprovision.*"
                "managed fixture preserved for recovery",
            ) as raised:
                self._run_acceptance(
                    Path(temporary),
                    stop_returncode=9,
                    command_log=commands,
                )

        self.assertIn("deprovision", [command[4] for command in commands])
        fixture_root = Path(commands[0][commands[0].index("--state-dir") + 1]).parent
        self.assertIn(str(fixture_root), str(raised.exception))
        self.assertTrue((fixture_root / "state" / "openvmm.log").is_file())
        self.assertTrue((fixture_root / "scratch.ext4").is_file())
        shutil.rmtree(fixture_root)
        self.assertFalse(fixture_root.exists())


_LIFECYCLE_STATE_SCRIPT = 'printf \'%s\\n\' "$@" >"$0"'
_CliResult = tuple[int, bytes, bytes]
# Each part of a runtime identity, as nvx-rs names it, its key in the runtime
# record, and the build provenance that names its digest, by its field.
_RUNTIME_PROVENANCE = (
    ("OpenVMM", "openvmm", "openvmm.provenance.json", "executable_sha256"),
    ("kernel", "kernel", "vmlinux.provenance.json", "kernel_sha256"),
    ("initramfs", "initramfs", "initramfs.provenance.json", "initramfs_sha256"),
)


class _FakeSandboxCli:
    """Emulates `nvx-rs sandbox` and the OpenVMM processes that it starts.

    Each fault names one regression of the public lifecycle that the
    acceptance must detect.
    """

    def __init__(self, *faults: str) -> None:
        self.faults = set(faults)
        self.running: dict[int, bool] = {}
        self.endpoints: dict[int, str] = {}
        self.guest_files: dict[Path, dict[str, bytes]] = {}
        self.operations: list[str] = []
        self.capabilities: list[bytes] = []
        self.probed = False
        self.next_pid = 4000
        # Operations take effect one at a time, as the lifecycle lock makes
        # them, and without it, overlapping starts meet after their checks.
        self.lifecycle_lock = threading.Lock()
        self.race = threading.Barrier(2, timeout=1)

    def __call__(
        self, command: list[str], **_: object
    ) -> subprocess.CompletedProcess[bytes]:
        with (
            nullcontext()
            if "unserialized-starts" in self.faults
            else self.lifecycle_lock
        ):
            return self._dispatch(command)

    def _dispatch(self, command: list[str]) -> subprocess.CompletedProcess[bytes]:
        # The acceptance runs `nvx-rs --root ROOT sandbox OPERATION`, under this
        # checkout, or under another artifact root, whose runtime identity may
        # differ from that of the sandbox's VM.
        position = command.index("sandbox")
        operation = command[position + 1]
        self.operations.append(operation)
        root = command[command.index("--root") + 1]
        values: dict[str, str] = (
            {} if root == os.fspath(nvx_rs.ROOT) else {"--root": root}
        )
        arguments: list[str] = []
        index = position + 2
        while index < len(command):
            if command[index].startswith("--arg="):
                arguments.append(command[index].removeprefix("--arg="))
                index += 1
            else:
                values[command[index]] = command[index + 1]
                index += 2
        state = Path(values["--state-dir"]) if "--state-dir" in values else None
        handlers: dict[
            str,
            Callable[[Path | None, dict[str, str], list[str]], _CliResult],
        ] = {
            "run": self._run,
            "provision": self._provision,
            "start": self._start,
            "exec": self._exec,
            "stop": self._stop,
            "deprovision": self._deprovision,
        }
        returncode, stdout, stderr = handlers[operation](state, values, arguments)
        return subprocess.CompletedProcess(command, returncode, stdout, stderr)

    @staticmethod
    def _error(code: str, message: str) -> _CliResult:
        return 1, b"", f"error[{code}]: {message}\n".encode()

    def _missing(self, state: Path) -> _CliResult | None:
        if not state.is_dir():
            return self._error(
                "sandbox-not-provisioned",
                f"sandbox state path is not a plain directory: {state}",
            )
        return None

    def _running(self, state: Path) -> tuple[dict[str, object], _CliResult | None]:
        refusal = self._missing(state)
        if refusal is not None:
            return {}, refusal
        if not (state / "runtime.json").is_file():
            return {}, self._error("sandbox-not-running", "sandbox is not running")
        runtime = json.loads((state / "runtime.json").read_text(encoding="utf-8"))
        if not self.running.get(runtime["process"]["pid"], False):
            # The transition that finds that OpenVMM exited clears its record.
            if "crash-keeps-runtime" not in self.faults:
                self._clear_runtime(state)
            return {}, self._error(
                "sandbox-exited",
                "sandbox runtime state is stale because the OpenVMM process is "
                "not running",
            )
        return runtime, None

    @staticmethod
    def _clear_runtime(state: Path) -> None:
        for name in ("control.capability", "control.sock", "runtime.json"):
            (state / name).unlink(missing_ok=True)

    def _differences(
        self, values: dict[str, str], runtime: dict[str, object]
    ) -> list[str]:
        """Returns the parts in which the runtime identity of the VM differs
        from that of the artifacts of the command's root, which nvx-rs reads
        from their build provenance."""
        if "--root" not in values or "other-runtime-driven" in self.faults:
            return []
        recorded = cast(dict[str, str], runtime["runtime"])
        build = Path(values["--root"]) / "build"
        return [
            part
            for part, key, name, field in _RUNTIME_PROVENANCE
            if recorded[key]
            != "sha256:" + json.loads((build / name).read_text(encoding="utf-8"))[field]
        ]

    def _other_runtime(
        self, runtime: dict[str, object], parts: list[str]
    ) -> _CliResult:
        pid = cast(dict[str, object], runtime["process"])["pid"]
        return self._error(
            "runtime-mismatch",
            "sandbox runs under another runtime identity than these artifacts, "
            f"which differ in {', '.join(parts)}, so it is not driven: stop it to "
            f"terminate its OpenVMM process {pid} without a guest shutdown",
        )

    def _run(
        self, state: Path | None, values: dict[str, str], _: list[str]
    ) -> _CliResult:
        assert state is None
        assert values["--workload-user"] == sandbox_lifecycle_tests.UNAVAILABLE_IDENTITY
        if "run-starts-workload" in self.faults:
            return 0, b"NVX-UBUNTU-SANDBOX-PROFILE-OK uid=65534 gid=65534\n", b""
        return (
            125,
            b">> openvmm\nNVX-SANDBOX-ERROR: configured workload UID is unavailable\n",
            b"",
        )

    def _provision(
        self, state: Path | None, values: dict[str, str], _: list[str]
    ) -> _CliResult:
        assert state is not None
        if values.get("--workload-user") == "0:0":
            return (
                2,
                b"",
                b"nvx-rs sandbox: error[argument-invalid]: argument --workload-user: "
                b"--workload-user UID must be between 1 and 4294967294\n",
            )
        if (state / "config.json").exists():
            return self._error("sandbox-provisioned", "sandbox is already provisioned")
        state.mkdir(parents=True, exist_ok=True)
        (state / "sandbox.lock").touch()
        (state / "config.json").write_text(
            json.dumps(
                {
                    "user": values.get("--workload-user", "65534:65534"),
                    "hypervisor": values["--hypervisor"],
                }
            ),
            encoding="utf-8",
        )
        return 0, b"", b""

    def _start(
        self, state: Path | None, values: dict[str, str], _: list[str]
    ) -> _CliResult:
        assert state is not None
        refusal = self._missing(state)
        if refusal is not None:
            return refusal
        runtime_path = state / "runtime.json"
        if runtime_path.exists() and "duplicate-start" not in self.faults:
            runtime = json.loads(runtime_path.read_text(encoding="utf-8"))
            if self.running.get(runtime["process"]["pid"], False):
                if parts := self._differences(values, runtime):
                    return self._other_runtime(runtime, parts)
                return self._error(
                    "sandbox-running",
                    "sandbox is already running or has stale runtime state",
                )
            # A start replaces the record of an OpenVMM that exited.
            self._clear_runtime(state)
        if "unserialized-starts" in self.faults:
            # Without the lock, both overlapping starts pass the check above
            # before either records its OpenVMM, and both launch one (#464).
            try:
                self.race.wait()
            except threading.BrokenBarrierError:
                self.race.reset()
            # Threads of one process cannot replace and remove the same files at
            # once on Windows, so the launches themselves take turns.
            with self.lifecycle_lock:
                return self._launch(state)
        return self._launch(state)

    def _launch(self, state: Path) -> _CliResult:
        (state / "outcome.json").unlink(missing_ok=True)
        with (state / "openvmm.log").open("a", encoding="utf-8", newline="\n") as log:
            if "noisy-log" in self.faults:
                log.write("noise\n" * 40_000)
            log.write("OpenVMM started\n")
        config = json.loads((state / "config.json").read_text(encoding="utf-8"))
        if config["user"] != "65534:65534":
            # The guest refuses the identity and powers off, and OpenVMM reports
            # its status before it exits.
            if "failed-start-kills-reporter" in self.faults:
                # OpenVMM ended while it published its report (issue #438).
                (state / ".tmpAbC123").write_text("{", encoding="utf-8")
            else:
                (state / "outcome.json").write_text(
                    json.dumps(
                        {
                            "schema_version": 1,
                            "outcome": sandbox_lifecycle_tests.MANAGED_REFUSAL,
                            "network_policy": {},
                            "teardown": {},
                        }
                    ),
                    encoding="utf-8",
                )
            if "failed-start-keeps-runtime" in self.faults:
                (state / "runtime.json").write_text("{}", encoding="utf-8")
            if "failed-start-hides-status" in self.faults:
                return self._error("endpoint-closed", "managed control endpoint closed")
            return self._error(
                "exited", "OpenVMM exited during startup: guest-exit status 125"
            )
        pid = self.next_pid
        self.next_pid += 1
        self.running[pid] = True
        endpoint = (
            f"//./pipe/openvmm-microvm-{pid}"
            if os.name == "nt"
            else str(state / "control.sock")
        )
        self.endpoints[pid] = endpoint
        if os.name != "nt":
            (state / "control.sock").touch()
        capability = os.urandom(32)
        self.capabilities.append(capability)
        (state / "control.capability").write_bytes(capability)
        (state / "runtime.json").write_text(
            json.dumps(
                {
                    "format": 4,
                    "launch": f"{pid:032x}",
                    "process": {"pid": pid, "startTime": pid * 7, "boot": "b" * 32},
                    "endpoint": endpoint,
                    "runtime": {
                        "openvmm": "sha256:" + "1" * 64,
                        "kernel": "sha256:" + "2" * 64,
                        "initramfs": "sha256:" + "3" * 64,
                        "bootContract": 3,
                        "controlContract": "nvx-microvm-v2-control-v2",
                        "controlSession": 1,
                    },
                }
            ),
            encoding="utf-8",
        )
        if "restart-loses-state" in self.faults:
            self.guest_files.pop(state, None)
        return 0, b"", b""

    def _exec(
        self, state: Path | None, values: dict[str, str], arguments: list[str]
    ) -> _CliResult:
        assert state is not None
        runtime, refusal = self._running(state)
        if refusal is not None:
            return refusal
        if parts := self._differences(values, runtime):
            return self._other_runtime(runtime, parts)
        report = (
            Path(values["--outcome-report"]) if "--outcome-report" in values else None
        )
        if (
            report is not None
            and report.exists()
            and "overwrite-report" not in self.faults
        ):
            return self._error(
                "outcome-report-exists", f"outcome report already exists: {report}"
            )
        files = self.guest_files.setdefault(state, {})
        entrypoint = values["--entrypoint"]
        returncode, category, stdout, stderr = 0, "exit", b"", b""
        if entrypoint == "/bin/sh" and arguments[:2] == ["-c", _LIFECYCLE_STATE_SCRIPT]:
            if "exec-loses-state" not in self.faults:
                files[arguments[2]] = "".join(
                    f"{argument}\n" for argument in arguments[3:]
                ).encode()
        elif entrypoint == "/bin/sh" and arguments[1].startswith("test ! -e "):
            returncode = int(arguments[1].removeprefix("test ! -e ") in files)
        elif entrypoint == "/bin/cat":
            if arguments[0] in files:
                stdout = files[arguments[0]]
            else:
                returncode, stderr = 1, b"cat: No such file or directory\n"
        elif entrypoint == "/bin/touch":
            files[arguments[0]] = b""
        elif entrypoint == sandbox_lifecycle_tests.PROBE:
            self.probed = True
            if "probe-interrupted" in self.faults:
                # SIGINT reaches the harness while it waits for the probe.
                raise KeyboardInterrupt
            if "probe-fails" in self.faults:
                returncode = 1
                stderr = b"NVX-UBUNTU-SANDBOX-FAIL: the workload shares a namespace\n"
            else:
                if arguments[:2] != ["entry", "limits"]:
                    raise AssertionError(f"unexpected probe arguments {arguments}")
                stdout = (
                    b"NVX-UBUNTU-SANDBOX-PROFILE-OK uid=65534 gid=65534\n"
                    + b"NVX-UBUNTU-SANDBOX-ENTRY-OK\n"
                    + f"NVX-UBUNTU-SANDBOX-LIMITS-OK memory_max={arguments[2]} "
                    f"pids_max={arguments[3]}\n".encode()
                    + b"NVX-UBUNTU-SANDBOX-OK uid=65534 gid=65534\n"
                )
        elif entrypoint == "/usr/bin/head":
            size = sandbox_lifecycle_tests.OUTPUT_LIMIT_BYTES
            if "unbounded-output" in self.faults:
                size = int(arguments[1])
            elif "short-output" in self.faults:
                size -= sandbox_lifecycle_tests.OUTPUT_CHUNK_BYTES
            returncode, category, stdout = 125, "output-limit", bytes(size)
        elif entrypoint != "/bin/true":
            raise AssertionError(f"unexpected workload {entrypoint}")
        if report is not None:
            report.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "operation_id": "a" * 32,
                        "outcome": {
                            "operation": "exec",
                            "category": category,
                            "status_code": returncode,
                        },
                    }
                ),
                encoding="utf-8",
            )
        return returncode, stdout, stderr

    def _stop(
        self, state: Path | None, values: dict[str, str], _: list[str]
    ) -> _CliResult:
        assert state is not None
        runtime, refusal = self._running(state)
        if refusal is not None:
            return refusal
        if parts := self._differences(values, runtime):
            return self._terminate_other_runtime(state, runtime, parts)
        pid = cast(int, cast(dict[str, object], runtime["process"])["pid"])
        if "stop-leaks-process" not in self.faults:
            self.terminate(pid)
        (state / "runtime.json").unlink()
        if "stop-leaks-capability" not in self.faults:
            (state / "control.capability").unlink()
        (state / "control.sock").unlink(missing_ok=True)
        config = json.loads((state / "config.json").read_text(encoding="utf-8"))
        teardown = {name: True for name in microvm_tests.OUTCOME_TEARDOWN_FIELDS}
        if "stop-reports-incomplete-teardown" in self.faults:
            teardown["vm_stopped"] = False
        if "stop-reports-partial-teardown" in self.faults:
            # Every step that it names completed, but it names one of seven.
            teardown = {"vm_stopped": True}
        (state / "outcome.json").write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "instance_id": "22" * 16,
                    "backend": (
                        "auto"
                        if "stop-reports-another-backend" in self.faults
                        else config["hypervisor"]
                    ),
                    "outcome": sandbox_lifecycle_tests.MANAGED_SUCCESS,
                    "network_policy": {
                        "status": (
                            "applied"
                            if "stop-reports-a-network-policy" in self.faults
                            else "not-requested"
                        )
                    },
                    "teardown": teardown,
                }
            ),
            encoding="utf-8",
        )
        return 0, b"", b""

    def _terminate_other_runtime(
        self, state: Path, runtime: dict[str, object], parts: list[str]
    ) -> _CliResult:
        # A VM of another runtime identity is asked nothing: its OpenVMM is
        # terminated, and publishes no outcome report.
        if "other-runtime-stop-refused" in self.faults:
            return self._other_runtime(runtime, parts)
        pid = cast(int, cast(dict[str, object], runtime["process"])["pid"])
        if "other-runtime-stop-leaks-process" not in self.faults:
            self.terminate(pid)
        if "other-runtime-stop-keeps-record" not in self.faults:
            self._clear_runtime(state)
        if "other-runtime-stop-is-silent" in self.faults:
            return 0, b"", b""
        return (
            0,
            b"",
            "warning: sandbox ran under another runtime identity than these "
            f"artifacts, which differ in {', '.join(parts)}: its OpenVMM process "
            f"{pid} was terminated without a guest shutdown\n".encode(),
        )

    def _deprovision(
        self, state: Path | None, _: dict[str, str], __: list[str]
    ) -> _CliResult:
        assert state is not None
        refusal = self._missing(state)
        if refusal is not None:
            return refusal
        runtime_path = state / "runtime.json"
        if runtime_path.is_file():
            runtime = json.loads(runtime_path.read_text(encoding="utf-8"))
            if self.running.get(runtime.get("process", {}).get("pid"), False):
                return self._error(
                    "sandbox-running", "sandbox must be stopped before deprovision"
                )
        if "cleanup-deprovision-fails" in self.faults and self.probed:
            return self._error("other", "injected deprovision failure")
        for name in (
            "launch.json",
            "runtime.json",
            "control.capability",
            "control.sock",
            "outcome.json",
            "openvmm.log",
            "config.json",
            "sandbox.lock",
        ):
            (state / name).unlink(missing_ok=True)
        # The store owns the file in which OpenVMM stages its outcome report.
        for path in state.iterdir():
            if re.fullmatch(r"\.tmp[A-Za-z0-9]{6}", path.name):
                path.unlink(missing_ok=True)
        unknown = sorted(path.name for path in state.iterdir())
        if unknown and "deprovision-removes-foreign" not in self.faults:
            return self._error(
                "foreign-files",
                "sandbox state directory contains files not owned by NVX: "
                + ", ".join(unknown),
            )
        shutil.rmtree(state)
        self.guest_files.pop(state, None)
        return 0, b"", b""

    def processes(self, _: str) -> tuple[int, ...]:
        return tuple(sorted(pid for pid, running in self.running.items() if running))

    def endpoint_present(self, runtime: dict[str, object]) -> bool:
        return runtime["endpoint"] in self.endpoints.values()

    def terminate(self, pid: int) -> None:
        self.running[pid] = False
        self.endpoints.pop(pid, None)


class PublicSandboxLifecycleTests(unittest.TestCase):
    EXPECTED_OPERATIONS = [
        # Identity admission.
        "provision",
        "run",
        "provision",
        "start",
        "deprovision",
        # Every operation fails before provision.
        "start",
        "exec",
        "stop",
        "deprovision",
        # Provision, and the operations that need a running sandbox.
        "provision",
        "provision",
        "exec",
        "stop",
        # A running sandbox.
        "start",
        "start",
        "exec",
        "exec",
        "exec",
        "exec",
        "exec",
        "exec",
        "deprovision",
        "exec",
        # Stop, and the operations that need a running sandbox.
        "stop",
        "exec",
        "stop",
        # Restart with overlapping starts.
        "start",
        "start",
        "exec",
        # The CLI of another runtime identity refuses to drive the VM, which
        # the CLI that started it still drives, and its stop terminates it.
        "exec",
        "start",
        "deprovision",
        "exec",
        "stop",
        "stop",
        # Start again, then a crash, which the next transition reports.
        "start",
        "exec",
        "stop",
        # Deprovision.
        "deprovision",
        "deprovision",
        "exec",
    ]

    # The checkout's build provenance, which names the identities that the
    # fake CLI records for the VMs that it starts.
    PROVENANCE = {
        name: {"format": 1, field: digit * 64}
        for (_, _, name, field), digit in zip(_RUNTIME_PROVENANCE, "123", strict=True)
    }

    def _run(self, root: Path, fake: _FakeSandboxCli) -> Path:
        distro = root / "ubuntu-distro.erofs"
        distro.write_bytes(b"distro")
        template = root / "ubuntu-smoke-scratch.ext4"
        template.write_bytes(bytes(4096))
        artifacts = managed_exec_tests.WorkloadArtifacts(
            distro, "12345678-1234-1234-1234-123456789abc", template
        )
        output_dir = root / "results"
        mkdtemp = tempfile.mkdtemp

        def fixture(prefix: str) -> str:
            return mkdtemp(prefix=prefix, dir=root)

        with (
            patch.object(
                sandbox_lifecycle_tests,
                "load_workload_artifacts",
                return_value=artifacts,
            ),
            patch.object(
                sandbox_lifecycle_tests.tempfile, "mkdtemp", side_effect=fixture
            ),
            patch.object(sandbox_lifecycle_tests.subprocess, "run", side_effect=fake),
            patch.object(
                sandbox_lifecycle_tests,
                "read_build_provenance",
                return_value=self.PROVENANCE,
            ),
            patch.object(
                sandbox_lifecycle_tests.nvx_rs,
                "require_binary",
                return_value=Path("nvx-rs"),
            ),
            patch.object(
                sandbox_lifecycle_tests,
                "openvmm_processes",
                side_effect=fake.processes,
            ),
            patch.object(
                sandbox_lifecycle_tests,
                "control_endpoint_present",
                side_effect=fake.endpoint_present,
            ),
            patch.object(
                sandbox_lifecycle_tests,
                "terminate_process",
                side_effect=fake.terminate,
            ),
            # The fake's processes end at once, and name no process of this host.
            patch.object(
                sandbox_lifecycle_tests, "openvmm_threads_remain", return_value=False
            ),
        ):
            sandbox_lifecycle_tests.run_sandbox_lifecycle(
                "whp", timeout=2, output_dir=output_dir
            )
        return output_dir

    def test_acceptance_drives_the_complete_public_lifecycle(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fake = _FakeSandboxCli()
            output_dir = self._run(root, fake)
            checks = json.loads(
                (output_dir / "sandbox-lifecycle-checks.json").read_text(
                    encoding="utf-8"
                )
            )
            evidence = {path.name: path.read_bytes() for path in output_dir.iterdir()}
            fixtures = list(root.glob("nvx-public-lifecycle-*"))

        self.assertEqual(fake.operations, self.EXPECTED_OPERATIONS)
        self.assertEqual(fixtures, [])
        self.assertFalse(any(fake.running.values()))
        self.assertEqual(
            set(evidence),
            {
                "sandbox-lifecycle-checks.json",
                "sandbox-lifecycle-identity-run.log",
                "sandbox-lifecycle-identity-openvmm.log",
                "sandbox-lifecycle-identity-outcome.json",
                "sandbox-lifecycle-runtime.json",
                "sandbox-lifecycle-probe.log",
                "sandbox-lifecycle-output-limit-outcome.json",
                "sandbox-lifecycle-outcome.json",
                "sandbox-lifecycle-openvmm.log",
            },
        )
        self.assertEqual(
            [record["operation"] for record in checks], self.EXPECTED_OPERATIONS
        )
        for record in checks:
            self.assertEqual(
                set(record),
                {
                    "operation",
                    "argv",
                    "expected_returncode",
                    "returncode",
                    "error_code",
                    "stdout_bytes",
                    "stderr_bytes",
                    "state_entries",
                },
            )
        # Managed arguments keep their whitespace.
        self.assertTrue(
            any(
                [
                    f"--arg={argument}"
                    for argument in sandbox_lifecycle_tests.STATE_ARGUMENTS
                ]
                == record["argv"][-3:]
                for record in checks
            )
        )
        # The bounded reports hold neither workload output nor the capability.
        self.assertNotIn(
            b"NVX-UBUNTU-SANDBOX", evidence["sandbox-lifecycle-checks.json"]
        )
        for capability in fake.capabilities:
            for content in evidence.values():
                self.assertNotIn(capability, content)
                self.assertNotIn(capability.hex().encode(), content)
        self.assertIn(
            b"NVX-UBUNTU-SANDBOX-LIMITS-OK", evidence["sandbox-lifecycle-probe.log"]
        )

    def test_acceptance_detects_lifecycle_regressions(self):
        regressions = {
            "run-starts-workload": "run returned 0, expected 125",
            "failed-start-keeps-runtime": (
                "after a refused managed start: the state directory holds "
                "['config.json', 'openvmm.log', 'outcome.json', 'runtime.json', "
                "'sandbox.lock']"
            ),
            "failed-start-hides-status": (
                "start returned 1, expected 1 with error 'exited' naming 'guest-exit'"
            ),
            # OpenVMM ended while it published its report (#438), which left
            # only the report's staging file.
            "failed-start-kills-reporter": (
                "the refused managed start published no outcome report"
            ),
            "duplicate-start": "start returned 0, expected 1",
            "exec-loses-state": "the state-reading request returned status 1",
            "probe-fails": "exec returned 1, expected 0",
            "unbounded-output": "the output limit forwarded 2097152 stdout",
            "short-output": "the output limit forwarded 1015808 stdout",
            "overwrite-report": "exec returned 0, expected 1",
            "stop-leaks-capability": "after stop: the state directory holds",
            "stop-leaks-process": "OpenVMM still runs after stop",
            "stop-reports-another-backend": "the managed report names backend 'auto'",
            "stop-reports-a-network-policy": (
                "the managed report has network policy status 'applied'"
            ),
            "stop-reports-incomplete-teardown": (
                "the managed report has incomplete teardown"
            ),
            "stop-reports-partial-teardown": (
                "the managed report has incomplete teardown: {'vm_stopped': True}"
            ),
            "restart-loses-state": "the restarted sandbox's state returned status 1",
            "unserialized-starts": "overlapping public starts returned status 0",
            "other-runtime-driven": "exec returned 0, expected 1",
            "other-runtime-stop-refused": "stop returned 1, expected 0",
            "other-runtime-stop-is-silent": (
                "the stop of another runtime printed stdout='', stderr=''"
            ),
            "other-runtime-stop-leaks-process": (
                "OpenVMM still runs after the stop of another runtime"
            ),
            "other-runtime-stop-keeps-record": (
                "after the stop of another runtime: the state directory holds"
            ),
            "crash-keeps-runtime": (
                "after exec of a crashed sandbox: the state directory holds"
            ),
            "deprovision-removes-foreign": "deprovision returned 0, expected 1",
        }
        for fault, message in regressions.items():
            with (
                self.subTest(fault=fault),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                fake = _FakeSandboxCli(fault)
                with self.assertRaisesRegex(RuntimeError, re.escape(message)):
                    self._run(root, fake)
                # Cleanup ends every OpenVMM process and removes the fixture.
                self.assertFalse(any(fake.running.values()))
                self.assertEqual(list(root.glob("nvx-public-lifecycle-*")), [])
                self.assertTrue(
                    (root / "results" / "sandbox-lifecycle-checks.json").is_file()
                )

    def test_evidence_keeps_the_end_of_long_logs(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = self._run(Path(temporary), _FakeSandboxCli("noisy-log"))
            log = (output_dir / "sandbox-lifecycle-openvmm.log").read_bytes()

        limit = sandbox_lifecycle_tests.EVIDENCE_LIMIT
        header, separator, tail = log.partition(b"\n")
        self.assertRegex(header, rb"^\.\.\. \(\d+ earlier bytes omitted\)$")
        self.assertEqual(separator, b"\n")
        self.assertEqual(len(tail), limit)
        self.assertTrue(tail.endswith(b"noise\nOpenVMM started\n"))

    def test_bounded_evidence_keeps_the_end_of_its_data(self):
        bounded = sandbox_lifecycle_tests.bounded_evidence
        limit = sandbox_lifecycle_tests.EVIDENCE_LIMIT
        self.assertEqual(bounded(b"short"), b"short")
        self.assertEqual(
            bounded(b"a" * 10 + b"b" * limit),
            b"... (10 earlier bytes omitted)\n" + b"b" * limit,
        )
        self.assertEqual(bounded(b"tail", 7), b"... (7 earlier bytes omitted)\ntail")

    def test_failed_cleanup_preserves_the_fixture(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fake = _FakeSandboxCli("probe-fails", "cleanup-deprovision-fails")
            with self.assertRaisesRegex(
                RuntimeError,
                "(?s)exec returned 1, expected 0.*fixture preserved for recovery",
            ) as raised:
                self._run(root, fake)
            fixtures = list(root.glob("nvx-public-lifecycle-*"))

            self.assertEqual(len(fixtures), 1)
            self.assertIn(str(fixtures[0]), str(raised.exception))
            self.assertTrue((fixtures[0] / "state" / "config.json").is_file())
            self.assertFalse(any(fake.running.values()))
            self.assertTrue(
                (root / "results" / "sandbox-lifecycle-openvmm.log").is_file()
            )

    def test_an_interrupted_acceptance_stops_its_sandboxes(self):
        # A cancelled CI job interrupts the harness (#946), which stops the
        # sandboxes that the acceptance started, as a failure does, and still
        # reports the interruption, not a failure.
        for faults, preserved in (
            (("probe-interrupted",), False),
            (("probe-interrupted", "cleanup-deprovision-fails"), True),
        ):
            with (
                self.subTest(faults=faults),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                fake = _FakeSandboxCli(*faults)
                stderr = io.StringIO()
                with (
                    redirect_stderr(stderr),
                    self.assertRaises(KeyboardInterrupt),
                ):
                    self._run(root, fake)
                fixtures = list(root.glob("nvx-public-lifecycle-*"))

                # The cleanup stopped and deprovisioned the running sandbox.
                self.assertEqual(fake.operations[-3:], ["exec", "stop", "deprovision"])
                self.assertFalse(any(fake.running.values()))
                self.assertEqual(len(fixtures), int(preserved))
                self.assertTrue(
                    (root / "results" / "sandbox-lifecycle-checks.json").is_file()
                )
                if preserved:
                    self.assertIn(str(fixtures[0]), stderr.getvalue())
                    self.assertRegex(
                        stderr.getvalue(),
                        "^warning: cleanup after the interruption: .*"
                        "fixture preserved for recovery",
                    )
                else:
                    self.assertEqual(stderr.getvalue(), "")

    def test_scratch_journal_check_reads_the_ext4_recovery_flag(self):
        with tempfile.TemporaryDirectory() as temporary:
            image = Path(temporary) / "scratch.ext4"
            superblock = bytearray(2048)
            image.write_bytes(superblock)
            self.assertTrue(sandbox_lifecycle_tests.scratch_journal_clean(image))
            superblock[1024 + 0x60] = sandbox_lifecycle_tests.EXT4_INCOMPAT_RECOVER
            image.write_bytes(superblock)
            self.assertFalse(sandbox_lifecycle_tests.scratch_journal_clean(image))
            image.write_bytes(bytes(1030))
            with self.assertRaisesRegex(RuntimeError, "truncated"):
                sandbox_lifecycle_tests.scratch_journal_clean(image)

    def test_kill_ends_the_recorded_process(self):
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])

        def processes(marker: str) -> tuple[int, ...]:
            # The scan finds the child under its fixture's name while it runs.
            self.assertEqual(marker, "nvx-public-lifecycle-kill")
            return (child.pid,) if child.poll() is None else ()

        start_time = 1
        if sys.platform != "win32":
            stat = Path(f"/proc/{child.pid}/stat").read_bytes()
            start_time = int(stat[stat.rindex(b")") + 1 :].split()[19])
        runtime: dict[str, object] = {
            "format": 4,
            "process": {"pid": child.pid, "startTime": start_time, "boot": "b" * 32},
            "endpoint": "unused",
        }
        try:
            with patch.object(
                sandbox_lifecycle_tests, "openvmm_processes", side_effect=processes
            ):
                self.assertTrue(
                    sandbox_lifecycle_tests.openvmm_running(
                        runtime, "nvx-public-lifecycle-kill"
                    )
                )
                sandbox_lifecycle_tests.kill_openvmm(
                    runtime, 30, "nvx-public-lifecycle-kill"
                )
                self.assertFalse(
                    sandbox_lifecycle_tests.openvmm_running(
                        runtime, "nvx-public-lifecycle-kill"
                    )
                )
                self.assertFalse(
                    sandbox_lifecycle_tests.openvmm_threads_remain(runtime)
                )
            self.assertNotEqual(child.wait(timeout=30), 0)
        finally:
            if child.poll() is None:
                child.kill()
                child.wait(timeout=30)

    @unittest.skipIf(sys.platform == "win32", "Windows has no /proc")
    def test_a_killed_openvmm_runs_until_its_last_thread_exits(self):
        # As nvx-rs tells, a process runs while a thread of it remains, even
        # once its main thread is a zombie, whose command line is empty.
        runtime: dict[str, object] = {
            "process": {"pid": 4242, "startTime": 99, "boot": "b" * 32}
        }

        def stat(state: str, start_time: int) -> bytes:
            fields = [state, *["0"] * 18, str(start_time), *["0"] * 30]
            return b"4242 (open vmm) " + " ".join(fields).encode()

        for line, threads, remain in (
            (stat("S", 99), 40, True),
            (stat("Z", 99), 3, True),
            (stat("Z", 99), 1, False),
            (stat("X", 99), 1, False),
            # Another process has the ID.
            (stat("S", 100), 40, False),
        ):
            with (
                self.subTest(line=line, threads=threads),
                patch.object(Path, "read_bytes", return_value=line),
                patch.object(
                    sandbox_lifecycle_tests.os, "listdir", return_value=["t"] * threads
                ),
            ):
                self.assertEqual(
                    sandbox_lifecycle_tests.openvmm_threads_remain(runtime), remain
                )
        with patch.object(Path, "read_bytes", side_effect=FileNotFoundError):
            self.assertFalse(sandbox_lifecycle_tests.openvmm_threads_remain(runtime))

    def test_control_endpoint_presence_follows_the_endpoint(self):
        if sys.platform == "win32":
            from multiprocessing.connection import Listener

            name = f"openvmm-microvm-test-{os.getpid()}"
            runtime: dict[str, object] = {"endpoint": f"//./pipe/{name}"}
            listener = Listener(rf"\\.\pipe\{name}", family="AF_PIPE")
            try:
                self.assertTrue(
                    sandbox_lifecycle_tests.control_endpoint_present(runtime)
                )
            finally:
                listener.close()
            self.assertFalse(sandbox_lifecycle_tests.control_endpoint_present(runtime))
            return
        with tempfile.TemporaryDirectory() as temporary:
            endpoint = Path(temporary) / "control.sock"
            runtime = {"endpoint": str(endpoint)}
            self.assertFalse(sandbox_lifecycle_tests.control_endpoint_present(runtime))
            endpoint.touch()
            self.assertTrue(sandbox_lifecycle_tests.control_endpoint_present(runtime))

    @unittest.skipUnless(sys.platform == "linux", "scans the Linux process table")
    def test_process_scan_finds_openvmm_processes_naming_the_fixture(self):
        shell = shutil.which("sh")
        assert shell is not None
        with tempfile.TemporaryDirectory() as temporary:
            openvmm = Path(temporary) / "openvmm"
            shutil.copyfile(shell, openvmm)
            openvmm.chmod(0o755)
            # The shell blocks in its read builtin, so it starts no other
            # process and keeps its arguments.
            child = subprocess.Popen(
                [str(openvmm), "-c", "read -r line", "nvx-public-lifecycle-scan"],
                stdin=subprocess.PIPE,
            )
            try:
                deadline = time.monotonic() + 30
                while child.pid not in sandbox_lifecycle_tests.openvmm_processes(
                    "nvx-public-lifecycle-scan"
                ):
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.05)
                self.assertEqual(
                    sandbox_lifecycle_tests.openvmm_processes("nvx-other-fixture"), ()
                )
            finally:
                child.kill()
                child.wait(timeout=30)
                if child.stdin is not None:
                    child.stdin.close()


class _FakeProxySandboxCli(_FakeSandboxCli):
    """Emulates `nvx-rs sandbox` with the scenario's open and proxy-only
    networks, and the guest's probes, whose traffic reaches the host's
    listeners as OpenVMM would carry it: from the gateway to host loopback,
    and elsewhere unchanged.

    The open network lets every probe through unless an `open-blocks-NAME`
    fault names it, and the proxy-only network lets none through unless a
    fault names it: `leaks-NAME` connects and reports the connection, and
    `hides-NAME` connects or sends but reports what a denied probe reports.
    Each other fault names a regression of the planner or of the proxy that
    the acceptance must detect.
    """

    def __init__(self, *faults: str) -> None:
        super().__init__(*faults)
        # The network of each guest that probed, and the names of its probes.
        self.probe_runs: list[tuple[str, list[str]]] = []

    def _provision(
        self, state: Path | None, values: dict[str, str], _: list[str]
    ) -> _CliResult:
        gateway = sandbox_proxy_tests.GATEWAY
        proxy = values.get("--network-proxy")
        if proxy is None:
            assert values["--network-egress"] == "allow"
            assert values["--host-loopback"] == "allow"
            network = "open"
        elif (
            values["--network-egress"] != "deny" and "accepts-egress" not in self.faults
        ):
            return self._error(
                "unsupported",
                "a network proxy needs egress denied by default, since the proxy "
                "is the guest's only way out",
            )
        elif (
            not proxy.startswith(f"{gateway}:")
            and "accepts-other-address" not in self.faults
        ):
            return self._error(
                "unsupported",
                f"--network-proxy {proxy}: the address must be the guest's "
                f"gateway, {gateway}",
            )
        else:
            network = "proxy-only"
        result = super()._provision(state, values, _)
        if result[0] == 0:
            assert state is not None
            path = state / "config.json"
            config = json.loads(path.read_text(encoding="utf-8"))
            config["network"] = network
            path.write_text(json.dumps(config), encoding="utf-8")
        return result

    @staticmethod
    def _network(state: Path) -> str:
        config = json.loads((state / "config.json").read_text(encoding="utf-8"))
        return cast(str, config["network"])

    def _exec(
        self, state: Path | None, values: dict[str, str], arguments: list[str]
    ) -> _CliResult:
        assert state is not None
        _, refusal = self._running(state)
        if refusal is not None:
            return refusal
        assert values["--entrypoint"] == "/bin/sh"
        assert arguments[:3] == ["-c", sandbox_proxy_tests.PROBE_SCRIPT, "probe"]
        network = self._network(state)
        probes = arguments[5:]
        self.probe_runs.append((network, probes[1::4]))
        lines = [self._request(network, arguments[3], arguments[4])]
        for index in range(0, len(probes), 4):
            protocol, name, host, port = probes[index : index + 4]
            status = self._probe(network, protocol, name, host, int(port))
            lines.append(f"{name} {status}")
        return 0, "".join(f"{line}\n" for line in lines).encode(), b""

    def _request(self, network: str, proxy: str, target: str) -> str:
        if network == "proxy-only" and "proxy-unreachable" in self.faults:
            return "proxy 1 wget: can't connect to remote host: Connection refused"
        gateway, _, port = proxy.removeprefix("http://").rpartition(":")
        assert gateway == sandbox_proxy_tests.GATEWAY
        if "direct-request" in self.faults:
            target = "/probe"
        with socket.create_connection(("127.0.0.1", int(port)), timeout=2) as client:
            client.sendall(
                f"GET {target} HTTP/1.1\r\nHost: nvx-proxy-only.invalid\r\n"
                "Connection: close\r\n\r\n".encode()
            )
            response = b""
            while chunk := client.recv(4096):
                response += chunk
        body = response.partition(b"\r\n\r\n")[2].decode()
        return f"proxy 0 {body}"

    def _probe(
        self, network: str, protocol: str, name: str, host: str, port: int
    ) -> int:
        denied = 1 if protocol == "tcp" else 0
        hidden = network == "proxy-only" and f"hides-{name}" in self.faults
        if network == "open":
            reached = f"open-blocks-{name}" not in self.faults
        else:
            reached = hidden or f"leaks-{name}" in self.faults
        if not reached:
            return denied
        destination = {
            sandbox_proxy_tests.GATEWAY: "127.0.0.1",
            sandbox_proxy_tests.GATEWAY_IPV6: "::1",
        }.get(host, host)
        family = socket.AF_INET6 if ":" in destination else socket.AF_INET
        if protocol == "udp":
            with socket.socket(family, socket.SOCK_DGRAM) as sender:
                sender.sendto(f"{name}\n".encode(), (destination, port))
        else:
            with socket.create_connection((destination, port), timeout=2):
                pass
        return denied if hidden else 0

    def _stop(
        self, state: Path | None, values: dict[str, str], _: list[str]
    ) -> _CliResult:
        result = super()._stop(state, values, _)
        if result[0] == 0:
            assert state is not None
            network = self._network(state)
            path = state / "outcome.json"
            report = json.loads(path.read_text(encoding="utf-8"))
            policy = dict(
                sandbox_proxy_tests.OPEN_NETWORK_POLICY
                if network == "open"
                else sandbox_proxy_tests.NETWORK_POLICY
            )
            if network == "proxy-only" and "stop-reports-allowed-egress" in self.faults:
                policy["mode"] = "allow-all"
            report["network_policy"] = policy
            path.write_text(json.dumps(report), encoding="utf-8")
        return result


class PublicSandboxProxyOnlyTests(unittest.TestCase):
    # A loopback address stands in for the host's own address, which a test
    # cannot rely on.
    HOST_ADDRESS = "127.0.0.2"
    PROBES = ["gateway-port", "gateway-udp", "host-address"]
    EXPECTED_OPERATIONS = [
        # The planner refuses a proxy beside allowed egress, and one at an
        # address other than the gateway.
        "provision",
        "provision",
        # The open network, whose guest reaches every destination.
        "provision",
        "start",
        "exec",
        "stop",
        "deprovision",
        # The proxy-only network, whose guest reaches only the proxy.
        "provision",
        "start",
        "exec",
        "stop",
        "deprovision",
    ]

    def _run(
        self, root: Path, fake: _FakeProxySandboxCli, *, ipv6: bool = False
    ) -> Path:
        distro = root / "ubuntu-distro.erofs"
        distro.write_bytes(b"distro")
        template = root / "ubuntu-smoke-scratch.ext4"
        template.write_bytes(bytes(4096))
        artifacts = managed_exec_tests.WorkloadArtifacts(
            distro, "12345678-1234-1234-1234-123456789abc", template
        )
        output_dir = root / "results"
        mkdtemp = tempfile.mkdtemp

        def fixture(prefix: str) -> str:
            return mkdtemp(prefix=prefix, dir=root)

        with (
            patch.object(
                sandbox_proxy_tests, "load_workload_artifacts", return_value=artifacts
            ),
            patch.object(sandbox_proxy_tests.tempfile, "mkdtemp", side_effect=fixture),
            patch.object(sandbox_lifecycle_tests.subprocess, "run", side_effect=fake),
            patch.object(
                sandbox_lifecycle_tests,
                "openvmm_processes",
                side_effect=fake.processes,
            ),
            patch.object(
                sandbox_lifecycle_tests,
                "control_endpoint_present",
                side_effect=fake.endpoint_present,
            ),
            patch.object(
                sandbox_lifecycle_tests,
                "terminate_process",
                side_effect=fake.terminate,
            ),
            patch.object(
                sandbox_proxy_tests, "host_address", return_value=self.HOST_ADDRESS
            ),
            patch.object(sandbox_proxy_tests, "ipv6_loopback", return_value=ipv6),
            patch.object(
                sandbox_lifecycle_tests.nvx_rs,
                "require_binary",
                return_value=Path("nvx-rs"),
            ),
        ):
            sandbox_proxy_tests.run_sandbox_proxy_only(
                "kvm", timeout=2, output_dir=output_dir
            )
        return output_dir

    def test_acceptance_reaches_only_the_proxy(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fake = _FakeProxySandboxCli()
            output_dir = self._run(root, fake)
            checks = json.loads(
                (output_dir / "sandbox-proxy-only-checks.json").read_text(
                    encoding="utf-8"
                )
            )
            evidence = {path.name: path.read_bytes() for path in output_dir.iterdir()}
            fixtures = list(root.glob("nvx-public-proxy-*"))

        self.assertEqual(fake.operations, self.EXPECTED_OPERATIONS)
        self.assertEqual(fixtures, [])
        self.assertFalse(any(fake.running.values()))
        self.assertEqual(
            fake.probe_runs, [("open", self.PROBES), ("proxy-only", self.PROBES)]
        )
        self.assertEqual(
            set(evidence),
            {
                "sandbox-proxy-only-checks.json",
                "sandbox-proxy-only-runtime.json",
                *(
                    f"sandbox-proxy-only-{network}-{name}"
                    for network in ("open", "proxy-only")
                    for name in (
                        "probe.log",
                        "proxy.log",
                        "outcome.json",
                        "openvmm.log",
                    )
                ),
            },
        )
        self.assertEqual(
            [record["operation"] for record in checks], self.EXPECTED_OPERATIONS
        )
        egress, address, open_network, proxy_only = (
            record["argv"][record["argv"].index("--net") :]
            for record in (checks[0], checks[1], checks[2], checks[7])
        )
        port = proxy_only[-1].rpartition(":")[2]
        network = [
            "--net",
            "192.0.2.2/24",
            "--network-profile",
            "portable",
            "--network-egress",
            "deny",
            "--host-loopback",
            "deny",
            "--network-proxy",
            f"192.0.2.1:{port}",
        ]
        self.assertEqual(proxy_only, network)
        self.assertEqual(open_network[:8], [*network[:5], "allow", network[6], "allow"])
        self.assertEqual(len(open_network), 8)
        self.assertEqual(egress, [*network[:5], "allow", *network[6:]])
        self.assertEqual(address, [*network[:9], f"198.51.100.1:{port}"])
        for name in ("open", "proxy-only"):
            self.assertEqual(
                evidence[f"sandbox-proxy-only-{name}-proxy.log"],
                b"GET http://nvx-proxy-only.invalid/probe HTTP/1.1\n",
            )
        self.assertEqual(
            evidence["sandbox-proxy-only-open-probe.log"],
            b"proxy 0 NVX-PROXY-ONLY-RESPONSE\n"
            b"gateway-port 0\ngateway-udp 0\nhost-address 0\n",
        )
        self.assertEqual(
            evidence["sandbox-proxy-only-proxy-only-probe.log"],
            b"proxy 0 NVX-PROXY-ONLY-RESPONSE\n"
            b"gateway-port 1\ngateway-udp 0\nhost-address 1\n",
        )

    @unittest.skipUnless(
        sandbox_proxy_tests.ipv6_loopback(), "IPv6 loopback is unavailable"
    )
    def test_acceptance_probes_the_gateway_over_ipv6(self):
        for faults, message in (
            ((), None),
            (
                ("open-blocks-gateway-ipv6",),
                "the gateway-ipv6 probe to fd00::c000:201 port",
            ),
            (("hides-gateway-ipv6",), "the gateway-ipv6 probe reached the host"),
        ):
            with (
                self.subTest(faults=faults),
                tempfile.TemporaryDirectory() as temporary,
            ):
                fake = _FakeProxySandboxCli(*faults)
                if message is None:
                    self._run(Path(temporary), fake, ipv6=True)
                else:
                    with self.assertRaisesRegex(RuntimeError, re.escape(message)):
                        self._run(Path(temporary), fake, ipv6=True)
                self.assertEqual(fake.probe_runs[-1][1][-1], "gateway-ipv6")
                self.assertFalse(any(fake.running.values()))

    def test_acceptance_detects_proxy_regressions(self):
        regressions = {
            "accepts-egress": "provision returned 0, expected 1",
            "accepts-other-address": "provision returned 0, expected 1",
            "open-blocks-gateway-udp": (
                "the gateway-udp probe did not reach the host at port"
            ),
            "open-blocks-host-address": "the host-address probe to 127.0.0.2 port",
            "proxy-unreachable": (
                "the guest of the proxy-only network did not reach the proxy"
            ),
            "direct-request": (
                "the test proxy received [b'GET /probe HTTP/1.1'] in the open network"
            ),
            "leaks-gateway-port": "the gateway-port probe to 192.0.2.1 port",
            "hides-gateway-port": "the gateway-port probe reached the host",
            "hides-gateway-udp": "the gateway-udp probe reached the host at port",
            "hides-host-address": "the host-address probe reached the host",
            "stop-reports-allowed-egress": "the managed report has network policy",
        }
        for fault, message in regressions.items():
            with (
                self.subTest(fault=fault),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                fake = _FakeProxySandboxCli(fault)
                with self.assertRaisesRegex(RuntimeError, re.escape(message)):
                    self._run(root, fake)
                # Cleanup ends every OpenVMM process and removes the fixture.
                self.assertFalse(any(fake.running.values()))
                self.assertEqual(list(root.glob("nvx-public-proxy-*")), [])
                self.assertTrue(
                    (root / "results" / "sandbox-proxy-only-checks.json").is_file()
                )

    def test_probe_script_reports_each_status(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        result = subprocess.run(
            [
                shell,
                "-c",
                'wget() { [ "$http_proxy" = http://192.0.2.1:8443 ] && '
                '[ "$*" = "-T 5 -qO- http://nvx-proxy-only.invalid/probe" ] && '
                "printf NVX-PROXY-ONLY-RESPONSE; }\n"
                'nc() { case "$*" in\n'
                '"-z -w 2 192.0.2.1 8444") return 1 ;;\n'
                '"-u -w 1 192.0.2.1 8443") read -r datagram; '
                '[ "$datagram" = gateway-udp ] ;;\n'
                "*) return 127 ;;\n"
                "esac; }\n" + sandbox_proxy_tests.PROBE_SCRIPT,
                "probe",
                "http://192.0.2.1:8443",
                sandbox_proxy_tests.PROXY_TARGET,
                *("tcp", "gateway-port", "192.0.2.1", "8444"),
                *("udp", "gateway-udp", "192.0.2.1", "8443"),
                *("tcp", "host-address", "198.51.100.7", "8443"),
            ],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout,
            "proxy 0 NVX-PROXY-ONLY-RESPONSE\n"
            "gateway-port 1\n"
            "gateway-udp 0\n"
            "host-address 127\n",
        )


class _ExitingProcess:
    """An OpenVMM process that exits with status 0 once its console ends."""

    pid = 123

    def __init__(self) -> None:
        self.exited = False

    def poll(self) -> int | None:
        return 0 if self.exited else None

    def wait(self, timeout: float | None = None) -> int:
        del timeout
        self.exited = True
        return 0


class _EchoingGuestConsole:
    """A guest console whose input arrives in pieces of ``piece`` characters
    while commands run, as a slow guest's did in #785. The terminal echoes
    each piece as it arrives, into what the running command prints, before
    the newline of its line.

    The shell reads one line at a time and echoes it after its prompt, and it
    waits for the rest of a line that has not arrived. It runs ``echo``,
    ``cat >PATH <<'DELIMITER'``, ``. PATH``, and ``nvx-exit``, with ``; ``
    between commands, and ignores other commands.
    """

    def __init__(self, piece: int) -> None:
        self.process = _ExitingProcess()
        self.containment = None
        self._piece = piece
        self._input: queue.Queue[bytes] = queue.Queue()

    def write_input(self, data: bytes) -> None:
        self._input.put(data)

    def close(self) -> None:
        pass

    def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
        chunks.put(b"NVX-GUEST-BOOT-OK: alpine\r\n")
        typed = self._input.get(timeout=5).decode()
        files: dict[str, list[str]] = {}
        arrived = 0
        consumed = 0

        def emit(text: str) -> None:
            if text:
                chunks.put(text.encode())

        def read_line(prompt: str) -> str:
            nonlocal arrived, consumed
            end = typed.index("\n", consumed)
            arrived = max(arrived, end + 1)
            line = typed[consumed:end]
            consumed = end + 1
            emit(f"{prompt}{line}\r\n")
            return line

        def run(line: str) -> bool:
            """Run the commands of ``line``, and return whether the guest
            still runs."""
            nonlocal arrived
            for command in line.split("; "):
                heredoc = re.fullmatch(r"cat >(\S+) <<'(\w+)'", command)
                if heredoc is not None:
                    # The shell reads the here-document before cat runs.
                    path, delimiter = heredoc.groups()
                    files[path] = []
                    while (body := read_line("> ")) != delimiter:
                        files[path].append(body)
                piece = typed[arrived : arrived + self._piece]
                arrived += len(piece)
                echo = piece.replace("\n", "\r\n")
                words = shlex.split(command, comments=True)
                if words[:1] == ["echo"]:
                    emit(" ".join(words[1:]) + echo + "\r\n")
                    continue
                emit(echo)
                if words[:1] == ["."]:
                    if not all(run(stored) for stored in files[words[1]]):
                        return False
                elif words[:1] == ["nvx-exit"]:
                    return False
            return True

        while consumed < len(typed) and run(read_line("/ # ")):
            pass
        self.process.exited = True
        chunks.put(None)


class GuestConsoleEchoTests(unittest.TestCase):
    """The guest's terminal echoes console input that arrives while a command
    runs into what the command prints (#785); no scenario may parse it."""

    # Typed line by line, this script printed its DNS marker while the guest
    # was still receiving the UDP probe's line, as in CI.
    ECHOED_SCRIPT = (
        "set -eu\n"
        "echo NVX-IPV6-EGRESS-DNS none\n"
        "probe_udp udp6-named fd00::c000:201 20000 &\n"
        "probe_icmp icmp6 fd00::c000:201 &\n"
        "echo NVX-IPV6-EGRESS-OK\n"
        "nvx-exit 0\n"
    )
    # The piece that arrives while the script's first line runs ends where
    # CI's did, inside the UDP probe's port.
    ECHOED_PIECE = ECHOED_SCRIPT.index("0000 &") - len("set -eu\n")

    def test_typed_script_lets_the_echo_cut_into_its_dns_marker(self):
        console = _EchoingGuestConsole(self.ECHOED_PIECE)
        with patch.object(benchmark, "InteractiveProcess", return_value=console):
            result = benchmark.run_guest_script(
                ["nvx-rs", "run"],
                self.ECHOED_SCRIPT,
                microvm_tests.IPV6_EGRESS_COMPLETION_MARKER,
                timeout=5,
                boot_marker=microvm_tests.BOOT_MARKER,
            )
        self.assertIn("NVX-IPV6-EGRESS-DNS none0000 &\r\n", result["text"])
        self.assertEqual(
            microvm_tests._ipv6_egress_dns_report(result["text"]), "none0000 &"
        )

    def test_staged_script_keeps_the_echo_out_of_its_output(self):
        console = _EchoingGuestConsole(self.ECHOED_PIECE)
        with patch.object(benchmark, "InteractiveProcess", return_value=console):
            result = microvm_tests.run_guest_script(
                ["nvx-rs", "run"],
                self.ECHOED_SCRIPT,
                microvm_tests.IPV6_EGRESS_COMPLETION_MARKER,
                timeout=5,
            )
        self.assertEqual(
            result["output"], "NVX-IPV6-EGRESS-DNS none\r\nNVX-IPV6-EGRESS-OK\r\n"
        )
        self.assertEqual(
            microvm_tests._ipv6_egress_dns_report(result["output"]), "none"
        )
        # The terminal still echoed the input that arrived while cat ran, but
        # before the start line.
        before, _, _ = result["text"].partition("NVX-GUEST-SCRIPT-START\r\n")
        self.assertIn('\r\necho NVX-GUEST-SCRIPT-"START"; . /tmp/', before)

    def test_scripts_run_once_the_guest_has_consumed_them(self):
        script = 'echo "NVX-OUT ${value:-none}"\necho NVX-DONE\nnvx-exit 0\n'
        transcript = (
            "NVX-GUEST-BOOT-OK: alpine\r\n"
            # An echoed fragment of the staged script, cut before a marker.
            "NVX-DONE\r\n"
            '/ # echo NVX-GUEST-SCRIPT-"START"; . /tmp/nvx-guest-script\r\n'
            "NVX-GUEST-SCRIPT-START\r\n"
            "NVX-OUT none\r\n"
            "NVX-DONE\r\n"
        )
        with patch.object(
            microvm_tests,
            "_run_guest_script",
            return_value={"text": transcript, "wall_ms": 5.0, "peak_rss_bytes": 7},
        ) as run:
            result = microvm_tests.run_guest_script(
                ["openvmm"],
                script,
                b"NVX-DONE",
                timeout=30,
                log_path=Path("guest.log"),
                contain_process_tree=True,
            )
        run.assert_called_once_with(
            ["openvmm"],
            "cat >/tmp/nvx-guest-script <<'NVX_GUEST_SCRIPT'\n"
            + script
            + "NVX_GUEST_SCRIPT\n"
            + 'echo NVX-GUEST-SCRIPT-"START"; . /tmp/nvx-guest-script\n',
            b"NVX-DONE",
            timeout=30,
            windows_cpus=None,
            teardown_mode="guest-exit",
            log_path=Path("guest.log"),
            boot_marker=microvm_tests.BOOT_MARKER,
            contain_process_tree=True,
            time_abi_status=True,
        )
        self.assertEqual(result["output"], "NVX-OUT none\r\nNVX-DONE\r\n")
        self.assertEqual(result["text"], transcript)
        self.assertEqual((result["wall_ms"], result["peak_rss_bytes"]), (5.0, 7))

    def test_staged_input_sources_the_script_in_the_console_shell(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        script = 'value=staged\necho "NVX-OUT $value"\n'
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(
                microvm_tests,
                "_run_guest_script",
                return_value={
                    "text": "NVX-GUEST-SCRIPT-START\nNVX-OUT staged\n",
                    "wall_ms": 0.0,
                    "peak_rss_bytes": 0,
                },
            ) as run,
        ):
            microvm_tests.run_guest_script(
                ["openvmm"], script, b"NVX-OUT staged", timeout=1
            )
            staged = run.call_args.args[1].replace(
                microvm_tests.GUEST_SCRIPT_PATH,
                (Path(temporary) / "script").as_posix(),
            )
            # The shell that reads the input keeps the script's variables.
            result = subprocess.run(
                [shell],
                input=(staged + 'echo "NVX-AFTER $value"\n').encode(),
                capture_output=True,
                timeout=10,
                check=False,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.decode().replace("\r\n", "\n"),
            "NVX-GUEST-SCRIPT-START\nNVX-OUT staged\nNVX-AFTER staged\n",
        )

    def test_scripts_cannot_end_their_staging_early(self):
        for script, message in (
            ("echo NVX-DONE", "must end with a newline"),
            ("echo one\nNVX_GUEST_SCRIPT\necho NVX-DONE\n", "cannot hold the line"),
        ):
            with (
                self.subTest(message=message),
                patch.object(microvm_tests, "_run_guest_script") as run,
                self.assertRaisesRegex(ValueError, message),
            ):
                microvm_tests.run_guest_script(
                    ["openvmm"], script, b"NVX-DONE", timeout=1
                )
            run.assert_not_called()

    def test_markers_count_only_after_the_start_line(self):
        for text, message in (
            (
                "NVX-DONE\r\nNVX-GUEST-SCRIPT-START\r\nNVX-OTHER\r\n",
                "did not print 'NVX-DONE' after its start line",
            ),
            ('NVX-GUEST-SCRIPT-"START"\r\nNVX-DONE\r\n', "never printed the start"),
        ):
            with (
                self.subTest(message=message),
                patch.object(
                    microvm_tests,
                    "_run_guest_script",
                    return_value={"text": text, "wall_ms": 0.0, "peak_rss_bytes": 0},
                ),
                self.assertRaisesRegex(RuntimeError, message),
            ):
                microvm_tests.run_guest_script(
                    ["openvmm"], "echo NVX-DONE\n", b"NVX-DONE", timeout=1
                )

    def test_scripts_awaiting_a_release_turn_the_echo_off_first(self):
        # The host's release arrives while the script runs.
        for name, ready, release in (
            (
                "directional-network-policy.sh.in",
                'echo NVX-DIRECTIONAL-INGRESS-"READY"',
                "read -r ingress_release",
            ),
            (
                "host-loopback-policy.sh.in",
                f"echo {microvm_tests.HOST_LOOPBACK_INGRESS_READY_MARKER.decode()}",
                "read -r release",
            ),
            (
                "private-network-ingress.sh.in",
                f"echo {microvm_tests.PRIVATE_INGRESS_LISTENING_MARKER.decode()}",
                "read -r release",
            ),
            (
                "platform-network-policy.sh.in",
                f"echo {microvm_tests.PLATFORM_NETWORK_LISTENING_MARKER.decode()}",
                "read -r release exchange",
            ),
        ):
            with self.subTest(name=name):
                script = microvm_tests._read_script(name)
                self.assertEqual(script.count("stty -echo"), 1)
                self.assertLess(script.index("stty -echo"), script.index(ready))
                self.assertLess(script.index(ready), script.index(release))


class _FakeRamUpperSandboxCli(_FakeSandboxCli):
    """Emulates `nvx-rs sandbox --upper ram` and its guest: no scratch image,
    a distro layer alone, the RAM upper's token, and an upper that each start
    begins empty.

    Each fault names a regression of the planner, the guest, or the record
    that the acceptance must detect.
    """

    SMOKE_RUN = (
        b"NVX-UBUNTU-SANDBOX-PROFILE-OK uid=65534 gid=65534\n"
        + f"NVX-UBUNTU-SANDBOX-LIMITS-OK "
        f"memory_max={sandbox_ram_upper_tests.MEMORY_MAX} "
        f"pids_max={sandbox_ram_upper_tests.PIDS_MAX}\n".encode()
        + b"NVX-UBUNTU-SANDBOX-OK uid=65534 gid=65534\n"
    )

    def _command_line(self) -> str:
        upper = (
            "nvx_scratch=0xd0006000,ext4"
            if "declares-scratch" in self.faults
            else sandbox_ram_upper_tests.RAM_UPPER_TOKEN
        )
        return f"nvx_sandbox=1 nvx_layer=distro,0xd0003000,erofs {upper}"

    def _run(
        self, state: Path | None, values: dict[str, str], _: list[str]
    ) -> _CliResult:
        assert state is None
        assert values["--upper"] == "ram"
        assert values["--entrypoint"] == sandbox_lifecycle_tests.PROBE
        assert _ == [
            "limits",
            str(sandbox_ram_upper_tests.MEMORY_MAX),
            str(sandbox_ram_upper_tests.PIDS_MAX),
        ]
        blocks = f"--microvm-sandbox-block distro:file:{values['--layer']},ro"
        if "attaches-scratch" in self.faults:
            blocks += " --microvm-sandbox-block scratch:file:scratch.ext4"
        command = f">> openvmm {blocks}"
        if "drops-report" not in self.faults:
            report = Path(values["--outcome-report"])
            command += f" --microvm-report {report}"
            outcome = (
                {"operation": "run", "category": "guest-exit", "status_code": 1}
                if "run-reports-failure" in self.faults
                else sandbox_ram_upper_tests.RUN_SUCCESS
            )
            teardown = dict.fromkeys(microvm_tests.OUTCOME_TEARDOWN_FIELDS, True)
            if "partial-teardown" in self.faults:
                del teardown["virtiofs_released"]
            if "incomplete-teardown" in self.faults:
                teardown["openvmm_process_terminated"] = False
            report.write_text(
                json.dumps(
                    {
                        "backend": values["--hypervisor"],
                        "outcome": outcome,
                        "teardown": teardown,
                    }
                ),
                encoding="utf-8",
            )
        command += f" --cmdline '{self._command_line()}'\n"
        output = b"" if "probe-fails" in self.faults else self.SMOKE_RUN
        return 0, command.encode() + output, b""

    def _provision(
        self, state: Path | None, values: dict[str, str], _: list[str]
    ) -> _CliResult:
        if "--scratch" in values and "accepts-scratch" not in self.faults:
            return self._error(
                "unsupported",
                "--upper ram and --scratch are mutually exclusive: an upper in RAM "
                "has no scratch image",
            )
        result = super()._provision(state, values, _)
        if result[0] == 0:
            assert state is not None
            path = state / "config.json"
            config = json.loads(path.read_text(encoding="utf-8"))
            config.update(format=8, upper="ram", scratch=None, scratch_binding=None)
            if "records-scratch" in self.faults:
                config.update(upper="scratch", scratch="scratch.ext4")
            path.write_text(json.dumps(config), encoding="utf-8")
        return result

    def _start(
        self, state: Path | None, values: dict[str, str], _: list[str]
    ) -> _CliResult:
        if "--upper" in values and "start-takes-upper" not in self.faults:
            return self._error(
                "unsupported", "--upper is only valid for sandbox run or provision"
            )
        result = super()._start(state, values, _)
        # A RAM upper begins empty at each start.
        if result[0] == 0 and state is not None and "keeps-state" not in self.faults:
            self.guest_files.pop(state, None)
        return result

    def _exec(
        self, state: Path | None, values: dict[str, str], arguments: list[str]
    ) -> _CliResult:
        assert state is not None
        state_path = sandbox_ram_upper_tests.STATE_PATH
        if values["--entrypoint"] != "/bin/sh" or arguments[1].startswith("test ! -e "):
            return super()._exec(state, values, arguments)
        _, refusal = self._running(state)
        if refusal is not None:
            return refusal
        script = arguments[1]
        if script == sandbox_ram_upper_tests.VIEW_SCRIPT:
            devices = "vda\nvdb\n" if "has-scratch-device" in self.faults else "vda\n"
            return 0, f"loop0\n{devices}{self._command_line()}\n".encode(), b""
        if script == f"printf 'kept in RAM\\n' >{state_path}":
            self.guest_files.setdefault(state, {})[state_path] = (
                sandbox_ram_upper_tests.STATE_CONTENT
            )
            return 0, b"", b""
        if script == sandbox_ram_upper_tests.CHARGE_SCRIPT:
            if "uncharged" in self.faults:
                return 11, b"", b""
            return 0, sandbox_ram_upper_tests.CHARGE_MARKER + b"\n", b""
        raise AssertionError(f"unexpected script {script!r}")


class PublicSandboxRamUpperTests(unittest.TestCase):
    EXPECTED_OPERATIONS = [
        # The planner refuses a scratch image beside a RAM upper, and an upper
        # outside run and provision.
        "provision",
        "start",
        # A one-shot run.
        "run",
        # A managed sandbox: the probe, the guest's view, a written and a read
        # file, and the memory charge of a file in the upper.
        "provision",
        "start",
        "exec",
        "exec",
        "exec",
        "exec",
        "exec",
        "stop",
        # Its next start begins with an empty upper.
        "start",
        "exec",
        "stop",
        "deprovision",
    ]

    def _run(self, root: Path, fake: _FakeRamUpperSandboxCli) -> Path:
        distro = root / "ubuntu-distro.erofs"
        distro.write_bytes(b"distro")
        template = root / "ubuntu-smoke-scratch.ext4"
        template.write_bytes(bytes(4096))
        artifacts = managed_exec_tests.WorkloadArtifacts(
            distro, "12345678-1234-1234-1234-123456789abc", template
        )
        output_dir = root / "results"
        mkdtemp = tempfile.mkdtemp

        def fixture(prefix: str) -> str:
            return mkdtemp(prefix=prefix, dir=root)

        with (
            patch.object(
                sandbox_ram_upper_tests,
                "load_workload_artifacts",
                return_value=artifacts,
            ),
            patch.object(
                sandbox_ram_upper_tests.tempfile, "mkdtemp", side_effect=fixture
            ),
            patch.object(sandbox_lifecycle_tests.subprocess, "run", side_effect=fake),
            patch.object(
                sandbox_lifecycle_tests.nvx_rs,
                "require_binary",
                return_value=Path("nvx-rs"),
            ),
            patch.object(
                sandbox_lifecycle_tests,
                "openvmm_processes",
                side_effect=fake.processes,
            ),
            patch.object(
                sandbox_lifecycle_tests,
                "control_endpoint_present",
                side_effect=fake.endpoint_present,
            ),
            patch.object(
                sandbox_lifecycle_tests,
                "terminate_process",
                side_effect=fake.terminate,
            ),
            patch.object(
                sandbox_lifecycle_tests, "openvmm_threads_remain", return_value=False
            ),
        ):
            sandbox_ram_upper_tests.run_sandbox_ram_upper(
                "mshv", timeout=2, output_dir=output_dir
            )
        return output_dir

    def test_acceptance_runs_sandboxes_whose_upper_is_in_ram(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fake = _FakeRamUpperSandboxCli()
            output_dir = self._run(root, fake)
            checks = json.loads(
                (output_dir / "sandbox-ram-upper-checks.json").read_text(
                    encoding="utf-8"
                )
            )
            evidence = {path.name for path in output_dir.iterdir()}
            fixtures = list(root.glob("nvx-public-ram-upper-*"))

        self.assertEqual(fake.operations, self.EXPECTED_OPERATIONS)
        self.assertEqual(fixtures, [])
        self.assertFalse(any(fake.running.values()))
        self.assertEqual(
            evidence,
            {
                "sandbox-ram-upper-checks.json",
                "sandbox-ram-upper-runtime.json",
                "sandbox-ram-upper-run.log",
                "sandbox-ram-upper-run-outcome.json",
                "sandbox-ram-upper-probe.log",
                "sandbox-ram-upper-view.log",
                "sandbox-ram-upper-charge.log",
                "sandbox-ram-upper-first-stop-outcome.json",
                "sandbox-ram-upper-second-stop-outcome.json",
            },
        )
        self.assertEqual(
            [record["operation"] for record in checks], self.EXPECTED_OPERATIONS
        )
        # Every launch keeps its upper in RAM, and only the refused one names a
        # scratch image.
        launches = [
            record["argv"]
            for record in checks
            if record["operation"] in {"run", "provision"}
        ]
        for argv in launches:
            self.assertEqual(argv[argv.index("--upper") + 1], "ram")
        self.assertEqual(
            ["--scratch" in argv for argv in launches], [True, False, False]
        )

    def test_acceptance_detects_ram_upper_regressions(self):
        regressions = {
            "accepts-scratch": "provision returned 0, expected 1",
            "start-takes-upper": "with error 'unsupported'",
            "attaches-scratch": "did not attach its distro layer alone",
            "declares-scratch": "does not declare the RAM upper alone",
            "probe-fails": "the one-shot RAM-upper probe did not report",
            "drops-report": "the one-shot run's OpenVMM command does not name",
            "run-reports-failure": "the one-shot run reported",
            "partial-teardown": "the one-shot run's teardown is",
            "incomplete-teardown": "the one-shot run's teardown is",
            "records-scratch": "provision recorded the RAM upper as",
            "has-scratch-device": "has the virtio block devices ['vda', 'vdb']",
            "uncharged": "the memory charge of the RAM upper returned status 11",
            "keeps-state": "the restarted sandbox's empty upper returned status 1",
        }
        for fault, message in regressions.items():
            with (
                self.subTest(fault=fault),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                fake = _FakeRamUpperSandboxCli(fault)
                with self.assertRaisesRegex(RuntimeError, re.escape(message)):
                    self._run(root, fake)
                # Cleanup ends every OpenVMM process and removes the fixture.
                self.assertFalse(any(fake.running.values()))
                self.assertEqual(list(root.glob("nvx-public-ram-upper-*")), [])
                self.assertTrue(
                    (root / "results" / "sandbox-ram-upper-checks.json").is_file()
                )

    def test_charge_script_needs_a_kill_that_only_the_upper_file_causes(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        # The stand-in dd is killed as the memory controller kills the real one:
        # while the file holds the upper's pages, or always, or never.
        for dd, expected in (
            ('[ -e "$fill" ] && return 137; return 0', (0, "NVX-RAM-UPPER-CHARGED\n")),
            ("return 0", (11, "")),
            ("return 137", (12, "")),
        ):
            with self.subTest(dd=dd), tempfile.TemporaryDirectory() as temporary:
                fill = f"{Path(temporary).as_posix()}/fill"
                script = sandbox_ram_upper_tests.CHARGE_SCRIPT.replace(
                    "/tmp/nvx-ram-upper-fill", fill
                )
                result = subprocess.run(
                    [
                        shell,
                        "-c",
                        f"fill='{fill}'\nhead() {{ :; }}\ndd() {{ {dd}; }}\n{script}",
                    ],
                    capture_output=True,
                    text=True,
                    timeout=5,
                    check=False,
                )
                self.assertEqual(
                    (result.returncode, result.stdout), expected, result.stderr
                )
                self.assertFalse(Path(fill).exists())


class GuestIdentityScriptTests(unittest.TestCase):
    def test_guest_identity_checks_fail_before_success_markers(self):
        descriptor = microvm_tests.guest_descriptor("ubuntu")
        with (
            patch.object(
                microvm_tests,
                "workload_boot_command",
                return_value=["openvmm"],
            ),
            patch.object(microvm_tests, "run_guest_script") as run_guest_script,
        ):
            for runner in (
                microvm_tests.run_guest_boot,
                microvm_tests.run_guest_identity,
            ):
                with self.subTest(runner=runner.__name__):
                    runner(
                        Path("openvmm"),
                        Path("kernel"),
                        Path("initrd"),
                        "whp",
                        descriptor,
                        memory_mib=256,
                        timeout=60,
                        log_path=Path("guest.log"),
                    )
                    self.assertTrue(
                        run_guest_script.call_args.args[1].startswith("set -e\n")
                    )
                    run_guest_script.reset_mock()


def _create_wsl_symlink(path: Path, target: str) -> None:
    """Create the WSL-style link that OpenVMM stores for a guest on Windows."""
    if sys.platform != "win32":
        raise unittest.SkipTest("WSL-style links exist only on Windows")
    import ctypes
    from ctypes import wintypes

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.CreateFileW.restype = wintypes.HANDLE
    kernel32.CreateFileW.argtypes = [
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.HANDLE,
    ]
    kernel32.DeviceIoControl.restype = wintypes.BOOL
    kernel32.DeviceIoControl.argtypes = [
        wintypes.HANDLE,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
        ctypes.POINTER(wintypes.DWORD),
        wintypes.LPVOID,
    ]
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    generic_write = 0x40000000
    create_new = 1
    open_reparse_point = 0x00200000
    fsctl_set_reparse_point = 0x000900A4
    handle = kernel32.CreateFileW(
        str(path), generic_write, 0, None, create_new, open_reparse_point, None
    )
    if handle in (None, wintypes.HANDLE(-1).value):
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        # Version 2 of the LX link layout stores the UTF-8 target after a
        # 32-bit version field.
        data = (2).to_bytes(4, "little") + target.encode()
        reparse = (
            microvm_tests.IO_REPARSE_TAG_LX_SYMLINK.to_bytes(4, "little")
            + len(data).to_bytes(2, "little")
            + bytes(2)
            + data
        )
        buffer = ctypes.create_string_buffer(reparse, len(reparse))
        returned = wintypes.DWORD()
        if not kernel32.DeviceIoControl(
            handle,
            fsctl_set_reparse_point,
            buffer,
            len(reparse),
            None,
            0,
            ctypes.byref(returned),
            None,
        ):
            raise ctypes.WinError(ctypes.get_last_error())
    finally:
        kernel32.CloseHandle(handle)


class GuestSymlinkTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.target = self.root / "target"
        self.target.write_text("host data\n", encoding="utf-8")

    @unittest.skipIf(sys.platform == "win32", "Windows hosts store WSL-style links")
    def test_posix_link_must_keep_the_exact_target(self):
        link = self.root / "link"
        link.symlink_to("../target")
        microvm_tests.assert_guest_symlink(link, "../target")

        with self.assertRaisesRegex(RuntimeError, "does not point to"):
            microvm_tests.assert_guest_symlink(link, "target")
        with self.assertRaisesRegex(RuntimeError, "does not point to"):
            microvm_tests.assert_guest_symlink(self.target, "target")

    @unittest.skipUnless(sys.platform == "win32", "WSL-style links exist on Windows")
    def test_windows_link_must_be_an_inert_wsl_link(self):
        link = self.root / "link"
        _create_wsl_symlink(link, "../target")
        self.assertEqual(microvm_tests.read_wsl_symlink(link), b"../target")
        microvm_tests.assert_guest_symlink(link, "../target")

        with self.assertRaisesRegex(RuntimeError, "does not point to"):
            microvm_tests.assert_guest_symlink(link, "target")
        with self.assertRaisesRegex(RuntimeError, "not a WSL-style link"):
            microvm_tests.assert_guest_symlink(self.target, "target")
        followable = self.root / "followable"
        try:
            followable.symlink_to(self.target)
        except OSError as error:
            self.skipTest(f"NT symbolic links are unavailable: {error}")
        with self.assertRaisesRegex(RuntimeError, "not a WSL-style link"):
            microvm_tests.assert_guest_symlink(followable, str(self.target))


class LiveExchangeTests(unittest.TestCase):
    @staticmethod
    def _layout(root: Path, files: tuple[str, str], entries: tuple[str, str]) -> Path:
        root.mkdir()
        for name, marker in zip(("exchange-a", "exchange-b"), files, strict=True):
            (root / name).write_bytes(f"NVX-EXCHANGE-{marker}".encode())
        for name, marker in zip(
            ("exchange-dir-a", "exchange-dir-b"), entries, strict=True
        ):
            (root / name).mkdir()
            (root / name / "entry").write_bytes(f"NVX-EXCHANGE-DIR-{marker}".encode())
        return root

    def test_each_host_requires_the_outcome_of_its_platform(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            swapped = self._layout(root / "swapped", ("B", "A"), ("B", "A"))
            kept = self._layout(root / "kept", ("A", "B"), ("A", "B"))
            # Only the files traded names.
            files_only = self._layout(root / "files-only", ("B", "A"), ("A", "B"))
            if sys.platform == "linux":
                expected, other, wrong = swapped, kept, "exchange-dir-a/entry"
            else:
                expected, other, wrong = kept, swapped, "exchange-a"
            microvm_tests.assert_live_exchange(expected)
            with self.assertRaisesRegex(RuntimeError, "left exchange-a with"):
                microvm_tests.assert_live_exchange(other)
            with self.assertRaisesRegex(RuntimeError, f"left {wrong} with"):
                microvm_tests.assert_live_exchange(files_only)


class FilesystemOwnerTests(unittest.TestCase):
    def test_identity_capabilities_come_from_ambient_or_bounding_sets(self):
        def status(ambient: int, bounding: int) -> str:
            return (
                "Name:\tpython3\nCapInh:\t0000000000000000\n"
                f"CapAmb:\t{ambient:016x}\nCapBnd:\t{bounding:016x}\n"
            )

        both = (1 << 6) | (1 << 7)
        inherited = microvm_tests.identity_capabilities_inherited
        self.assertEqual(inherited(status(both, both), root=False), (True, True))
        self.assertEqual(inherited(status(1 << 7, both), root=False), (True, False))
        self.assertEqual(inherited(status(1 << 6, both), root=False), (False, True))
        self.assertEqual(inherited(status(0, both), root=False), (False, False))
        self.assertEqual(inherited(status(0, both), root=True), (True, True))
        self.assertEqual(inherited(status(both, 1 << 6), root=True), (False, True))
        self.assertEqual(inherited(status(both, 1 << 7), root=True), (True, False))

    def test_other_groups_exclude_the_effective_gid(self):
        with (
            patch.object(microvm_tests.sys, "platform", "linux"),
            patch.object(microvm_tests.os, "getegid", create=True, return_value=1000),
            patch.object(
                microvm_tests.os,
                "getgroups",
                create=True,
                return_value=[27, 1000, 4, 27],
            ),
        ):
            self.assertEqual(microvm_tests.openvmm_other_groups(), [4, 27])

    def _render(
        self,
        owner: str,
        foreign: str,
        root_result: str,
        foreign_result: str,
        other_group: str,
        share: Path,
    ) -> str:
        return (
            microvm_tests._render_script(
                "filesystem-owner.sh.in",
                OWNER=owner,
                FOREIGN=foreign,
                OTHER_GROUP=other_group,
                ROOT_RESULT=root_result,
                FOREIGN_RESULT=foreign_result,
            )
            .replace("share=/mnt/share", f"share={share.as_posix()}")
            .replace("nvx-exit", "nvx_exit")
        )

    def _run_script(self, script: str, stubs: str) -> subprocess.CompletedProcess[str]:
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        return subprocess.run(
            [shell, "-s"],
            input='nvx_exit() { exit "$1"; }\n' + stubs + script,
            text=True,
            capture_output=True,
            timeout=10,
            check=False,
        )

    def _run_in_share(
        self,
        owner: str,
        foreign: str,
        results: tuple[str, str],
        stubs: str,
        *,
        other_group: str = "none",
        read_only: bool = False,
    ) -> tuple[subprocess.CompletedProcess[str], Path]:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        share = Path(temporary.name)
        if read_only:
            share.chmod(0o500)
            self.addCleanup(share.chmod, 0o700)
        root_result, foreign_result = results
        script = self._render(
            owner, foreign, root_result, foreign_result, other_group, share
        )
        return self._run_script(script, stubs), share

    def test_script_checks_root_squash_and_both_foreign_results(self):
        if sys.platform != "linux":
            self.skipTest("the script uses Linux semantics")
        if os.geteuid() == 0:
            self.skipTest("root may chown and create device nodes")
        identity = f"{os.getuid()}:{os.getgid()}"

        # Without privilege, setpriv itself fails with EPERM, as OpenVMM does.
        denied, share = self._run_in_share(
            identity,
            "4242:4242",
            ("owned", "denied"),
            'setpriv() { echo "setpriv: Operation not permitted" >&2; return 1; }\n',
        )
        self.assertEqual(denied.returncode, 0, denied.stdout + denied.stderr)
        self.assertIn("NVX-FILESYSTEM-OWNER-OK", denied.stdout)
        self.assertTrue((share / "root-file").stat().st_mode & 0o4000)
        self.assertFalse((share / "foreign").exists())

        wrong_error, _ = self._run_in_share(
            identity,
            "4242:4242",
            ("owned", "denied"),
            'setpriv() { echo "Permission denied" >&2; return 1; }\n',
        )
        self.assertNotEqual(wrong_error.returncode, 0)
        self.assertNotIn("NVX-FILESYSTEM-OWNER-OK", wrong_error.stdout)

        wrong_owner, _ = self._run_in_share(
            "4242:4242", "4243:4243", ("owned", "denied"), "setpriv() { return 1; }\n"
        )
        self.assertEqual(wrong_owner.returncode, 100, wrong_owner.stdout)

        # Squashed root must not give a file one of OpenVMM's other groups.
        regrouped, _ = self._run_in_share(
            identity,
            "4242:4242",
            ("owned", "denied"),
            "chgrp() { return 0; }\nsetpriv() { return 1; }\n",
            other_group="4444",
        )
        self.assertEqual(regrouped.returncode, 119, regrouped.stdout)

        # The foreign caller here is this test's own identity, which may chown
        # to itself, so stub chown as the squashed root identity sees it.
        owned, share = self._run_in_share(
            identity,
            identity,
            ("owned", "owned"),
            "chown() { return 1; }\n"
            'setpriv() { while [ "$1" != -- ]; do shift; done; shift; "$@"; }\n',
        )
        self.assertEqual(owned.returncode, 0, owned.stdout + owned.stderr)
        self.assertTrue((share / "foreign" / "nested" / "file").is_file())
        self.assertEqual((share / "foreign").stat().st_mode & 0o777, 0o777)

    def test_script_requires_guest_root_to_fail_closed_when_denied(self):
        if sys.platform != "linux":
            self.skipTest("the script uses Linux semantics")
        if os.geteuid() == 0:
            self.skipTest("root may write to a read-only directory")
        identity = f"{os.getuid()}:{os.getgid()}"
        setpriv = (
            'setpriv() { echo "setpriv: Operation not permitted" >&2; return 1; }\n'
        )

        def stat(message: str) -> str:
            return f'stat() {{ echo "stat: {message}" >&2; return 1; }}\n'

        def run(
            stubs: str, *, read_only: bool = True
        ) -> tuple[subprocess.CompletedProcess[str], Path]:
            return self._run_in_share(
                identity,
                "4242:4242",
                ("denied", "denied"),
                setpriv + stubs,
                read_only=read_only,
            )

        # OpenVMM fails every request with EPERM, so nothing reaches the share.
        denied, share = run(stat("Operation not permitted"))
        self.assertEqual(denied.returncode, 0, denied.stdout + denied.stderr)
        self.assertIn("NVX-FILESYSTEM-OWNER-OK", denied.stdout)
        self.assertEqual(list(share.iterdir()), [])

        wrong_error, _ = run(stat("Permission denied"))
        self.assertEqual(wrong_error.returncode, 121, wrong_error.stdout)
        readable, _ = run("")
        self.assertEqual(readable.returncode, 120, readable.stdout)
        writable, _ = run(stat("Operation not permitted"), read_only=False)
        self.assertEqual(writable.returncode, 122, writable.stdout)

    def test_scenario_runs_as_caller_and_checks_host_ownership(self):
        if sys.platform == "linux":
            if os.geteuid() == 0:
                self.skipTest("root would chown the share to another owner")
        scripts: list[str] = []

        def guest(command: list[str], script: str, *_args: object, **_kwargs: object):
            scripts.append(script)
            mount = command[command.index("--mount") + 1]
            share = Path(mount.split(",")[1])
            (share / "root-file").write_text("root\n", encoding="utf-8")
            (share / "root-file").chmod(0o4755)
            (share / "root-directory").mkdir()

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(microvm_tests, "run_guest_script", side_effect=guest) as run,
            patch.object(microvm_tests, "OpenvmmProcess") as process,
            patch.object(
                microvm_tests,
                "openvmm_inherits_identity_capabilities",
                return_value=(False, False),
            ),
            patch.object(microvm_tests, "openvmm_other_groups", return_value=[]),
        ):
            expected = (
                b"must not be owned by UID 0 or GID 0"
                if sys.platform == "linux"
                else b"--mount-owner caller requires a Linux host"
            )
            process.return_value.__enter__.return_value.wait.return_value = (
                openvmm_process.OpenvmmProcessResult(2, expected + b"\n")
            )
            microvm_tests.run_filesystem_owner(
                Path("openvmm"),
                Path("kernel"),
                Path("initrd"),
                "kvm" if sys.platform == "linux" else "whp",
                memory_mib=128,
                timeout=40,
                output_dir=Path(temporary),
            )

        commands = [call.args[0] for call in run.call_args_list] + [
            call.args[0] for call in process.call_args_list
        ]
        self.assertTrue(commands)
        for command in commands:
            self.assertEqual(command[command.index("--mount-owner") + 1], "caller")
        if sys.platform != "linux":
            run.assert_not_called()
            self.assertEqual(process.call_count, 1)
            return
        self.assertEqual(len(scripts), 1)
        self.assertIn(f"owner={os.getuid()}:{os.getgid()}", scripts[0])
        self.assertIn("case owned in", scripts[0])
        self.assertIn("case denied in", scripts[0])
        self.assertIn("other_group=none", scripts[0])
        rejected = process.call_args_list[-1].args[0]
        self.assertEqual(rejected[rejected.index("--mount") + 1], "/mnt/share,/,ro")

    def test_scenario_requires_every_caller_to_fail_without_dropping_groups(self):
        if sys.platform != "linux":
            self.skipTest("caller ownership requires a Linux host")
        if os.geteuid() == 0:
            self.skipTest("root would chown the share to another owner")
        scripts: list[str] = []

        def run_scenario(*, guest_writes: bool) -> None:
            def guest(
                command: list[str], script: str, *_args: object, **_kwargs: object
            ):
                scripts.append(script)
                if guest_writes:
                    mount = command[command.index("--mount") + 1]
                    share = Path(mount.split(",")[1])
                    (share / "root-file").write_text("root\n", encoding="utf-8")

            with (
                tempfile.TemporaryDirectory() as temporary,
                patch.object(microvm_tests, "run_guest_script", side_effect=guest),
                patch.object(microvm_tests, "OpenvmmProcess") as process,
                patch.object(
                    microvm_tests,
                    "openvmm_inherits_identity_capabilities",
                    return_value=(False, False),
                ),
                patch.object(
                    microvm_tests, "openvmm_other_groups", return_value=[4444]
                ),
            ):
                process.return_value.__enter__.return_value.wait.return_value = (
                    openvmm_process.OpenvmmProcessResult(
                        2, b"must not be owned by UID 0 or GID 0\n"
                    )
                )
                microvm_tests.run_filesystem_owner(
                    Path("openvmm"),
                    Path("kernel"),
                    Path("initrd"),
                    "kvm",
                    memory_mib=128,
                    timeout=40,
                    output_dir=Path(temporary),
                )

        run_scenario(guest_writes=False)
        self.assertNotIn("case owned in", scripts[0])
        self.assertEqual(scripts[0].count("case denied in"), 2)
        self.assertIn("other_group=4444", scripts[0])
        with self.assertRaisesRegex(
            RuntimeError, "cannot drop its supplementary groups"
        ):
            run_scenario(guest_writes=True)

    def test_expectations_derive_guest_root_from_each_capability(self):
        expect = microvm_tests.filesystem_owner_expectations
        cases: list[
            tuple[
                tuple[int, int],
                tuple[int, int],
                tuple[bool, bool],
                list[int],
                tuple[bool, bool],
            ]
        ] = [
            # (export, openvmm, (cap_setuid, cap_setgid), groups, (root, foreign))
            # Export owned by OpenVMM's own identity.
            ((1000, 1000), (1000, 1000), (False, False), [4444], (False, False)),
            ((1000, 1000), (1000, 1000), (False, True), [4444], (True, False)),
            ((1000, 1000), (1000, 1000), (True, False), [4444], (False, False)),
            ((1000, 1000), (1000, 1000), (True, True), [4444], (True, True)),
            ((1000, 1000), (1000, 1000), (False, False), [], (True, False)),
            ((1000, 1000), (1000, 1000), (False, True), [], (True, False)),
            ((1000, 1000), (1000, 1000), (True, False), [], (True, False)),
            ((1000, 1000), (1000, 1000), (True, True), [], (True, True)),
            # Export owned by 65533:65533 while OpenVMM runs as root.
            ((65533, 65533), (0, 0), (False, False), [4444], (False, False)),
            ((65533, 65533), (0, 0), (False, True), [4444], (False, False)),
            ((65533, 65533), (0, 0), (True, False), [4444], (False, False)),
            ((65533, 65533), (0, 0), (True, True), [4444], (True, True)),
            ((65533, 65533), (0, 0), (False, False), [], (False, False)),
            ((65533, 65533), (0, 0), (False, True), [], (False, False)),
            ((65533, 65533), (0, 0), (True, False), [], (False, False)),
            ((65533, 65533), (0, 0), (True, True), [], (True, True)),
            # Only the export UID differs from OpenVMM's.
            ((65533, 1000), (1000, 1000), (False, False), [4444], (False, False)),
            ((65533, 1000), (1000, 1000), (False, True), [4444], (False, False)),
            ((65533, 1000), (1000, 1000), (True, False), [4444], (False, False)),
            ((65533, 1000), (1000, 1000), (True, True), [4444], (True, True)),
            ((65533, 1000), (1000, 1000), (False, False), [], (False, False)),
            ((65533, 1000), (1000, 1000), (False, True), [], (False, False)),
            ((65533, 1000), (1000, 1000), (True, False), [], (True, False)),
            ((65533, 1000), (1000, 1000), (True, True), [], (True, True)),
            # Only the export GID differs from OpenVMM's.
            ((1000, 65533), (1000, 1000), (False, False), [4444], (False, False)),
            ((1000, 65533), (1000, 1000), (False, True), [4444], (True, False)),
            ((1000, 65533), (1000, 1000), (True, False), [4444], (False, False)),
            ((1000, 65533), (1000, 1000), (True, True), [4444], (True, True)),
            ((1000, 65533), (1000, 1000), (False, False), [], (False, False)),
            ((1000, 65533), (1000, 1000), (False, True), [], (True, False)),
            ((1000, 65533), (1000, 1000), (True, False), [], (False, False)),
            ((1000, 65533), (1000, 1000), (True, True), [], (True, True)),
        ]
        for export, openvmm, caps, groups, expected in cases:
            with self.subTest(export=export, openvmm=openvmm, caps=caps, groups=groups):
                self.assertEqual(
                    expect(
                        export_uid=export[0],
                        export_gid=export[1],
                        openvmm_uid=openvmm[0],
                        openvmm_gid=openvmm[1],
                        cap_setuid=caps[0],
                        cap_setgid=caps[1],
                        supplementary_groups=groups,
                    ),
                    expected,
                )


class FilesystemSharesScenarioTests(unittest.TestCase):
    """The filesystem-shares scenario with OpenVMM and the guest simulated."""

    @staticmethod
    def _children(command: list[str]) -> list[str]:
        return [
            command[index + 1]
            for index, value in enumerate(command)
            if value == "--mount-child"
        ]

    @staticmethod
    def _host(child: str) -> Path:
        value = child.removesuffix(",file")
        return Path(value.split(",", 1)[1].rsplit(",", 1)[0])

    def _run(
        self,
        *,
        guest_writes_toolcache: bool = False,
        guest_links_children: bool = False,
        guest_writes_settings: bool = False,
    ) -> list[list[str]]:
        launched: list[list[str]] = []
        children = self._children
        host = self._host
        captured: dict[str, tuple[int, int]] = {}

        def identity(path: Path) -> tuple[int, int]:
            status = path.stat()
            return status.st_dev, status.st_ino

        def guest(
            command: list[str], script: str, marker: bytes, **_kwargs: object
        ) -> None:
            launched.append(command)
            self.assertEqual(marker, microvm_tests.FILESYSTEM_SHARES_MARKER)
            self.assertIn("touch /run/nvx/shares/1/mutation", script)
            self.assertIn("ln /run/nvx/shares/1/seed /run/nvx/shares/0/linked", script)
            self.assertIn("ls /run/nvx/shares/3/secret.json", script)
            workspace, toolcache, data, settings, output = (
                host(child) for child in children(command)
            )
            (workspace / "from-guest").write_bytes(b"NVX-GUEST-WRITE\n")
            (workspace / "guest-directory").mkdir()
            with (data / "notes one.txt").open("ab") as notes:
                notes.write(b"NVX-GUEST-NOTE\n")
            output.write_bytes(b"NVX-GUEST-OUTPUT\n")
            if guest_writes_toolcache:
                (toolcache / "mutation").write_bytes(b"")
            if guest_links_children:
                (workspace / "linked").write_bytes(b"NVX-TOOLCACHE")
            if guest_writes_settings:
                with settings.open("ab") as file:
                    file.write(b"append\n")

        class FakeProcess:
            def __init__(self, command: list[str], _log_path: Path) -> None:
                launched.append(command)
                self.command = command

            def __enter__(self) -> "FakeProcess":
                return self

            def __exit__(self, *_exception: object) -> None:
                return None

            def wait_for(self, _marker: bytes, _timeout: float) -> None:
                return None

            def send_bytes(self, _data: bytes) -> None:
                return None

            def wait(self, _timeout: float) -> openvmm_process.OpenvmmProcessResult:
                command = self.command
                requested = children(command)
                denied = [
                    command[index + 1]
                    for index, value in enumerate(command)
                    if value == "--mount-deny"
                ]
                if "--snapshot-destination" in command:
                    snapshot = Path(
                        command[command.index("--snapshot-destination") + 1]
                    )
                    snapshot.mkdir()
                    for name in ("manifest.bin", "state.bin", "memory.bin"):
                        (snapshot / name).write_bytes(name.encode())
                    (host(requested[0]) / "journal").write_bytes(b"NVX-BEFORE")
                    host(requested[4]).write_bytes(b"NVX-FILE-BEFORE")
                    captured["settings"] = identity(host(requested[3]))
                    return openvmm_process.OpenvmmProcessResult(
                        0, b"NVX-FILESYSTEM-SHARES-BEFORE\n"
                    )
                if "--restore-snapshot" in command:
                    names = [child.split(",", 1)[0] for child in requested]
                    if "--mount" in command:
                        error = (
                            b"requires the --mount-aggregate and --mount-child options"
                        )
                    elif (
                        len(requested) != 5
                        or host(requested[0]).name != "workspace"
                        or names != ["0", "1", "2", "3", "4"]
                    ):
                        error = b"do not match the snapshot contract"
                    elif identity(host(requested[3])) != captured["settings"]:
                        error = (
                            b"restore-time filesystem root identity does not match "
                            b"the snapshot attachment"
                        )
                    else:
                        with (host(requested[0]) / "journal").open("ab") as journal:
                            journal.write(b"NVX-AFTER")
                        with host(requested[4]).open("ab") as output:
                            output.write(b"NVX-FILE-AFTER")
                        return openvmm_process.OpenvmmProcessResult(
                            0, b"NVX-FILESYSTEM-SHARES-AFTER\n"
                        )
                    return openvmm_process.OpenvmmProcessResult(1, error + b"\n")
                if command.count("--mount") == 2:
                    error = b"the argument '--mount' cannot be used multiple times"
                elif "--mount" in command and "--mount-aggregate" in command:
                    error = b"'--mount-aggregate' cannot be used with '--mount'"
                elif "--mount" in command:
                    error = b"microVM filesystem root is not a plain directory"
                elif any(
                    child.endswith(",file") and host(child).is_dir()
                    for child in requested
                ):
                    error = b"microVM filesystem root is not a regular file"
                elif any(
                    not child.endswith(",file") and host(child).is_file()
                    for child in requested
                ):
                    error = b"microVM filesystem root is not a plain directory"
                elif "secrets" in denied:
                    error = b"--mount-deny requires an absolute host path"
                elif any(Path(path).name == "settings.json" for path in denied):
                    error = b"--mount-child 3 exposes the file settings.json"
                else:
                    error = b"host paths must not overlap"
                return openvmm_process.OpenvmmProcessResult(2, error + b"\n")

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(microvm_tests, "run_guest_script", side_effect=guest),
            patch.object(microvm_tests, "OpenvmmProcess", FakeProcess),
            patch.object(microvm_tests, "assert_guest_symlink") as symlink,
        ):
            microvm_tests.run_filesystem_shares(
                Path("openvmm"),
                Path("kernel"),
                Path("initrd"),
                "kvm",
                memory_mib=128,
                timeout=40,
                output_dir=Path(temporary),
            )
        link, target = symlink.call_args.args
        self.assertEqual(
            (link.name, target), ("toolcache-seed", "/opt/hostedtoolcache/seed")
        )
        return launched

    def test_scenario_attaches_every_share_and_restores_them_in_order(self):
        launched = self._run()
        boot = launched[0]
        self.assertNotIn("--mount", boot)
        self.assertEqual(boot[boot.index("--mount-aggregate") + 1], "/run/nvx/shares")
        workspace, toolcache, data, settings, output = self._children(boot)
        self.assertTrue(workspace.startswith("0,") and workspace.endswith(",rw"))
        self.assertTrue(toolcache.startswith("1,") and toolcache.endswith(",ro"))
        self.assertTrue(data.startswith("2,") and data.endswith(",rw"))
        # The files are children of their own, without their directories,
        # which OpenVMM must find to be regular files.
        self.assertTrue(
            settings.startswith("3,") and settings.endswith("settings.json,ro,file")
        )
        self.assertTrue(
            output.startswith("4,") and output.endswith("output.txt,rw,file")
        )
        cmdline = boot[boot.index("--cmdline") + 1]
        self.assertIn(
            "nvx_share=0,/workspace,rw nvx_share=1,/opt/hostedtoolcache,ro "
            "nvx_share=2,/srv/data,rw nvx_share=3,/config/settings.json,ro,file "
            "nvx_share=4,/results/output.txt,rw,file",
            cmdline,
        )

        def policy(command: list[str], option: str) -> list[str]:
            return [
                Path(command[index + 1]).name
                for index, value in enumerate(command)
                if value == option
            ]

        # The data directory's root hides everything but two paths, one of
        # them writable, and each other share hides its own denied path.
        self.assertEqual(
            policy(boot, "--mount-deny"), ["data", "secrets", "credentials"]
        )
        self.assertEqual(policy(boot, "--mount-allow"), ["public", "notes one.txt"])
        self.assertEqual(policy(boot, "--mount-write"), ["notes one.txt"])

        # Nine invalid sets fail before boot, then a capture with every share,
        # four invalid restores, the restore with every share in order, and a
        # restore after the host replaced the read-only file.
        self.assertEqual(len(launched), 1 + 9 + 1 + 5 + 1)
        self.assertEqual(launched[1].count("--mount"), 2)
        self.assertEqual(len(self._children(launched[2])), 2)
        # A file inside a directory child, a directory with the `file` flag, a
        # file without it, a file as a lone share, and a file child named by a
        # policy path.
        inside = self._children(launched[3])
        self.assertEqual(self._host(inside[1]), self._host(settings))
        self.assertEqual(self._host(inside[0]), self._host(settings).parent)
        self.assertEqual(
            self._children(launched[4]),
            [f"0,{self._host(settings).parent},ro,file"],
        )
        self.assertEqual(self._children(launched[5]), [f"0,{self._host(settings)},ro"])
        self.assertTrue(
            launched[6][launched[6].index("--mount") + 1].endswith("settings.json,ro")
        )
        self.assertEqual(
            policy(launched[9], "--mount-deny")[-1], self._host(settings).name
        )
        capture = launched[10]
        self.assertIn("--snapshot-destination", capture)
        every = [workspace, toolcache, data, settings, output]
        self.assertEqual(self._children(capture), every)
        self.assertEqual(self._children(launched[11]), [workspace, toolcache])
        self.assertEqual(
            [self._host(child).name for child in self._children(launched[12])],
            ["toolcache", "workspace", "data", "settings.json", "output.txt"],
        )
        # Renaming a child, with the same host directory, mode, and order, is
        # enough to fail.
        self.assertEqual(
            self._children(launched[13]),
            [workspace, toolcache, data.replace("2,", "data,", 1), settings, output],
        )
        self.assertIn("--mount", launched[14])
        self.assertEqual(self._children(launched[15]), every)
        self.assertEqual(self._children(launched[16]), every)

    def test_scenario_rejects_a_write_to_the_read_only_share(self):
        with self.assertRaisesRegex(RuntimeError, "modified the read-only share"):
            self._run(guest_writes_toolcache=True)

    def test_scenario_rejects_a_write_to_the_read_only_file(self):
        with self.assertRaisesRegex(RuntimeError, "changed the shared files"):
            self._run(guest_writes_settings=True)

    def test_scenario_rejects_a_hard_link_between_children(self):
        with self.assertRaisesRegex(RuntimeError, "hard link joined two children"):
            self._run(guest_links_children=True)

    def test_scenario_is_a_default_correctness_scenario(self):
        self.assertIn("filesystem-shares", microvm_tests.MICROVM_TEST_SCENARIOS)
        for name in ("filesystem-shares.sh", "filesystem-shares-snapshot.sh"):
            script = microvm_tests._read_script(name)
            self.assertIn("nvx-exit 0", script)


class FilesystemPolicyScenarioTests(unittest.TestCase):
    """The filesystem-policy scenario with OpenVMM and the guest simulated."""

    @staticmethod
    def _values(command: list[str], option: str) -> list[str]:
        return [
            command[index + 1] for index, value in enumerate(command) if value == option
        ]

    def _run(self, *, guest_writes_outside: bool = False) -> list[list[str]]:
        launched: list[list[str]] = []
        values = self._values

        def share_of(command: list[str]) -> Path:
            return Path(values(command, "--mount")[0].split(",", 2)[1])

        def guest(
            command: list[str], script: str, marker: bytes, **_kwargs: object
        ) -> None:
            launched.append(command)
            self.assertEqual(marker, microvm_tests.FILESYSTEM_POLICY_MARKER)
            self.assertIn("mount -t virtiofs microvm /mnt/second", script)
            share = share_of(command)
            (share / "out" / "from-guest").write_bytes(b"NVX-GUEST-WRITE\n")
            (share / "out" / "renamed").write_bytes(b"nested\n")
            with (share / "build.log").open("ab") as log:
                log.write(b"APPEND")
            if guest_writes_outside:
                (share / "logs" / "payloads" / "mutation").write_bytes(b"")

        class FakeProcess:
            def __init__(self, command: list[str], _log_path: Path) -> None:
                launched.append(command)
                self.command = command

            def __enter__(self) -> "FakeProcess":
                return self

            def __exit__(self, *_exception: object) -> None:
                return None

            def wait_for(self, _marker: bytes, _timeout: float) -> None:
                return None

            def send_bytes(self, _data: bytes) -> None:
                return None

            def wait(self, _timeout: float) -> openvmm_process.OpenvmmProcessResult:
                command = self.command
                share = share_of(command)
                policy = microvm_tests._policy_arguments(share)
                if "--snapshot-destination" in command:
                    snapshot = Path(
                        command[command.index("--snapshot-destination") + 1]
                    )
                    snapshot.mkdir()
                    for name in ("manifest.bin", "state.bin", "memory.bin"):
                        (snapshot / name).write_bytes(name.encode())
                    (share / "out" / "journal").write_bytes(b"NVX-BEFORE")
                    return openvmm_process.OpenvmmProcessResult(
                        0, b"NVX-FILESYSTEM-POLICY-BEFORE\n"
                    )
                if "--restore-snapshot" in command:
                    if tuple(command[-len(policy) :]) != policy:
                        return openvmm_process.OpenvmmProcessResult(
                            1,
                            b"restore-time filesystem access policy does not "
                            b"match the snapshot contract\n",
                        )
                    with (share / "out" / "journal").open("ab") as journal:
                        journal.write(b"NVX-AFTER")
                    return openvmm_process.OpenvmmProcessResult(
                        0, b"NVX-FILESYSTEM-POLICY-AFTER\n"
                    )
                writes = values(command, "--mount-write")
                if values(command, "--mount")[0].endswith(",ro"):
                    error = b"a read-only microVM filesystem cannot have writable paths"
                elif values(command, "--mount-allow"):
                    error = b"allowed path 'out' must be inside a denied path"
                elif values(command, "--mount-deny"):
                    error = b"writable path 'logs/gateway' is hidden by a denied path"
                elif len(writes) == 2:
                    error = b"microVM filesystem writable paths overlap"
                else:
                    error = b"writable path cannot be the complete filesystem export"
                return openvmm_process.OpenvmmProcessResult(2, error + b"\n")

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(microvm_tests, "run_guest_script", side_effect=guest),
            patch.object(microvm_tests, "OpenvmmProcess", FakeProcess),
            patch.object(microvm_tests, "assert_guest_symlink") as symlink,
        ):
            microvm_tests.run_filesystem_policy(
                Path("openvmm"),
                Path("kernel"),
                Path("initrd"),
                "kvm",
                memory_mib=128,
                timeout=40,
                output_dir=Path(temporary),
            )
        link, target = symlink.call_args.args
        self.assertEqual((link.name, target), ("seed-link", "../seed"))
        return launched

    def test_scenario_narrows_writes_and_restores_the_policy(self):
        launched = self._run()
        boot = launched[0]
        (mount,) = self._values(boot, "--mount")
        self.assertTrue(mount.startswith("/workspace,") and mount.endswith(",rw"))
        share = Path(mount.split(",", 2)[1])
        self.assertEqual(
            [
                Path(path).relative_to(share).as_posix()
                for path in self._values(boot, "--mount-deny")
            ],
            ["logs", "logs/payloads/private"],
        )
        self.assertEqual(
            [
                Path(path).relative_to(share).as_posix()
                for path in self._values(boot, "--mount-allow")
            ],
            ["logs/payloads"],
        )
        self.assertEqual(
            [
                Path(path).relative_to(share).as_posix()
                for path in self._values(boot, "--mount-write")
            ],
            ["out", "build.log"],
        )

        # Five invalid policies fail before boot, then a capture with the
        # policy, two restores with another policy, and the restore with it.
        self.assertEqual(len(launched), 1 + 5 + 1 + 3)
        capture = launched[6]
        self.assertIn("--snapshot-destination", capture)
        self.assertEqual(
            self._values(capture, "--mount-write"), self._values(boot, "--mount-write")
        )
        self.assertEqual(self._values(launched[7], "--mount-deny"), [])
        self.assertEqual(len(self._values(launched[8], "--mount-deny")), 1)
        self.assertEqual(
            self._values(launched[9], "--mount-allow"),
            self._values(boot, "--mount-allow"),
        )

    def test_scenario_rejects_a_write_outside_the_writable_paths(self):
        with self.assertRaisesRegex(RuntimeError, "outside its writable paths"):
            self._run(guest_writes_outside=True)

    def test_scenario_is_a_default_correctness_scenario(self):
        self.assertIn("filesystem-policy", microvm_tests.MICROVM_TEST_SCENARIOS)
        for name in ("filesystem-policy.sh", "filesystem-policy-snapshot.sh"):
            script = microvm_tests._read_script(name)
            self.assertIn("nvx-exit 0", script)


class FilesystemCommaPathsScenarioTests(unittest.TestCase):
    """The filesystem-comma-paths scenario with `nvx-rs run` and the guest
    simulated."""

    @staticmethod
    def _values(command: list[str], option: str) -> list[str]:
        return [
            command[index + 1] for index, value in enumerate(command) if value == option
        ]

    def _run(
        self,
        *,
        guest_writes_read_only: bool = False,
        guest_skips_write: bool = False,
    ) -> list[tuple[list[str], str]]:
        launched: list[tuple[list[str], str]] = []

        def guest(
            command: list[str], script: str, marker: bytes, **kwargs: object
        ) -> None:
            launched.append((list(command), script))
            self.assertEqual(marker, microvm_tests.FILESYSTEM_COMMA_PATHS_MARKER)
            self.assertIs(kwargs["contain_process_tree"], True)
            for mount in self._values(command, "--mount"):
                # NVX and OpenVMM read the mode after the last comma.
                host, mode = mount.split(",", 1)[1].rsplit(",", 1)
                if mode == "rw" and not guest_skips_write:
                    (Path(host) / "from-guest").write_bytes(b"NVX-GUEST-WRITE\n")
                if mode == "ro" and guest_writes_read_only:
                    (Path(host) / "from-guest").write_bytes(b"")

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(microvm_tests, "run_guest_script", side_effect=guest),
            patch.object(nvx_rs, "require_binary", return_value=_FAKE_NVX_RS),
        ):
            microvm_tests.run_filesystem_comma_paths(
                "kvm", memory_mib=128, timeout=1.0, output_dir=Path(temporary)
            )
        return launched

    def test_scenario_shares_each_directory_alone_and_both_together(self):
        launched = self._run()
        self.assertEqual(len(launched), 3)
        alpha = ("/mnt/alpha", "projects", "alpha,beta", "rw", "NVX-ALPHA-BETA")
        reports = ("/mnt/reports", "reports", "2026, final", "ro", "NVX-REPORTS")
        for (command, script), shares in zip(
            launched, ((alpha,), (reports,), (alpha, reports)), strict=True
        ):
            self.assertEqual(command[:4], _FAKE_PUBLIC_RUN)
            self.assertEqual(command[command.index("--hypervisor") + 1], "kvm")
            mounts = self._values(command, "--mount")
            denied = self._values(command, "--mount-deny")
            self.assertEqual(len(mounts), len(shares))
            for mount, deny, (target, parent, name, mode, seed) in zip(
                mounts, denied, shares, strict=True
            ):
                # Each value names the exact directory and ends in its mode,
                # beside the directory that the text before the last comma
                # names.
                self.assertTrue(mount.startswith(f"{target},"), mount)
                self.assertTrue(mount.endswith(f",{mode}"), mount)
                host = Path(mount[len(target) + 1 : -len(mode) - 1])
                self.assertEqual((host.parent.name, host.name), (parent, name))
                self.assertEqual(Path(deny), host / "secret")
                self.assertIn(f"{target}:{mode}:{seed}", script)
            self.assertIn(f"\naggregate={int(len(shares) > 1)}\n", script)
            self.assertNotIn("@", script)

    def test_scenario_rejects_a_write_to_the_read_only_directory(self):
        with self.assertRaisesRegex(RuntimeError, "changed more than"):
            self._run(guest_writes_read_only=True)

    def test_scenario_requires_the_write_to_the_read_write_directory(self):
        with self.assertRaisesRegex(RuntimeError, "changed more than"):
            self._run(guest_skips_write=True)

    def test_runner_dispatches_the_scenario_with_the_guest(self):
        def require(path: Path, _description: str) -> Path:
            return path

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(microvm_tests, "validate_openvmm_test_backend"),
            patch.object(microvm_tests, "require_file", side_effect=require),
            patch.object(microvm_tests, "run_filesystem_comma_paths") as scenario,
        ):
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "whp",
                    "--scenario",
                    "filesystem-comma-paths",
                    "--output-dir",
                    temporary,
                ]
            )
            self.assertEqual(microvm_tests.run(args), 0)
            scenario.assert_called_once_with(
                "whp",
                memory_mib=128,
                timeout=60.0,
                output_dir=Path(temporary),
                guest="alpine",
            )
        self.assertIn("filesystem-comma-paths", microvm_tests.MICROVM_TEST_SCENARIOS)
        script = microvm_tests._read_script("filesystem-comma-paths.sh.in")
        self.assertIn("nvx-exit 0", script)


class _PortExclusionSockets:
    """Create sockets that refuse ports excluded per protocol, as Windows does."""

    def __init__(self, *, tcp: Collection[int] = (), udp: Collection[int] = ()) -> None:
        self._socket_type = socket.socket
        self._excluded: dict[int, Collection[int]] = {
            socket.SOCK_STREAM: tcp,
            socket.SOCK_DGRAM: udp,
        }
        self.created: dict[int, list[MagicMock]] = {
            socket.SOCK_STREAM: [],
            socket.SOCK_DGRAM: [],
        }
        self.errors: list[OSError] = []

    def __call__(self, _family: int, kind: int) -> MagicMock:
        endpoint = MagicMock(spec=self._socket_type)

        def bind(address: tuple[str, int]) -> None:
            if address[1] in self._excluded[kind]:
                error = PermissionError(errno.EACCES, f"port {address[1]} is excluded")
                self.errors.append(error)
                raise error

        endpoint.bind.side_effect = bind
        self.created[kind].append(endpoint)
        return endpoint


class _EphemeralPortSockets:
    """Create sockets to which the host assigns ``ports`` in turn on bind."""

    def __init__(self, ports: Collection[int]) -> None:
        self._socket_type = socket.socket
        self._ports = iter(ports)
        self.created: list[MagicMock] = []
        self.events: list[tuple[str, int]] = []

    def __call__(self, _family: int, _kind: int) -> MagicMock:
        endpoint = MagicMock(spec=self._socket_type)
        port = next(self._ports)

        def bind(_address: tuple[str, int]) -> None:
            self.events.append(("bind", port))

        def close() -> None:
            self.events.append(("close", port))

        endpoint.bind.side_effect = bind
        endpoint.close.side_effect = close
        endpoint.getsockname.return_value = ("127.0.0.1", port)
        self.created.append(endpoint)
        return endpoint


class MicrovmTests(unittest.TestCase):
    @staticmethod
    def _managed_report(backend: str = "kvm") -> dict[str, object]:
        return {
            "schema_version": 1,
            "instance_id": "33" * 16,
            "backend": backend,
            "outcome": {
                "operation": "managed",
                "category": "success",
                "status_code": 0,
            },
            "network_policy": {
                "status": "not-requested",
                "status_code": 0,
                "mode": "none",
                "allow_rule_count": 0,
                "deny_rule_count": 0,
                "host_loopback": False,
            },
            "teardown": {name: True for name in microvm_tests.OUTCOME_TEARDOWN_FIELDS},
        }

    def _run_managed_lifecycle(
        self,
        output_dir: Path,
        *,
        returncode: int = 0,
        console: bytes = b"NVX-GUEST-BOOT-OK: managed\n",
        report: dict[str, object] | None = None,
        rejected: int = 1,
    ) -> list[tuple[list[str], dict[str, str], object]]:
        """Runs the scenario with a stand-in for the nvx-rs test, which leaves
        `console`, `report`, and `returncode` as the real one would."""
        runs: list[tuple[list[str], dict[str, str], object]] = []

        def start_test(
            command: list[str], *, cwd: object, env: dict[str, str]
        ) -> MagicMock:
            runs.append((command, env, cwd))
            openvmm = cast(list[str], json.loads(env["NVX_MANAGED_LIFECYCLE_COMMAND"]))
            # The test copies the guest console into the output directory.
            output_dir = Path(env["NVX_MANAGED_LIFECYCLE_OUTPUT_DIR"])
            (output_dir / "managed-lifecycle-guest.log").write_bytes(console)
            report_path = Path(openvmm[openvmm.index("--microvm-report") + 1])
            report_path.write_text(
                json.dumps(self._managed_report() if report is None else report),
                encoding="utf-8",
            )
            test = MagicMock()
            test.wait.return_value = returncode
            return test

        with (
            patch.object(microvm_tests.shutil, "which", return_value="cargo"),
            patch.object(microvm_tests.subprocess, "Popen", side_effect=start_test),
            patch.object(microvm_tests, "OpenvmmProcess") as process,
        ):
            process.return_value.__enter__.return_value.wait.return_value = (
                openvmm_process.OpenvmmProcessResult(rejected, b"")
            )
            microvm_tests.run_managed_lifecycle(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initramfs.cpio.gz"),
                "kvm",
                memory_mib=256,
                timeout=5,
                output_dir=output_dir,
            )
        return runs

    def test_managed_lifecycle_runs_the_nvx_rs_test_and_checks_what_it_leaves(self):
        # nvx-rs/tests/managed_lifecycle.rs drives the control session; the
        # scenario hands it OpenVMM's command, then checks the outcome report
        # and the guest console.
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            runs = self._run_managed_lifecycle(output_dir)
            preserved = json.loads(
                (output_dir / "managed-outcome.json").read_text(encoding="utf-8")
            )
            # An adversarial campaign requires this report, with its teardown.
            reports, teardown_complete = adversarial_executor._inventory_outcomes(
                output_dir,
                required_names=adversarial_executor._REQUIRED_OUTCOME_REPORTS[
                    "managed-lifecycle"
                ],
            )

        self.assertTrue(teardown_complete, reports)
        self.assertEqual(
            [report["path"] for report in reports], ["managed-outcome.json"]
        )
        self.assertEqual(len(runs), 1)
        command, environment, cwd = runs[0]
        self.assertEqual(
            command,
            [
                "cargo",
                "test",
                "--manifest-path",
                os.fspath(BuildConstants.REPO_ROOT / "nvx-rs" / "Cargo.toml"),
                "--locked",
                "--test",
                "managed_lifecycle",
                "--",
                "--ignored",
                "--nocapture",
            ],
        )
        # The repository root selects the pinned Rust release.
        self.assertEqual(cwd, BuildConstants.REPO_ROOT)
        self.assertEqual(environment["OPENVMM_LOG"], "off")
        self.assertEqual(environment["NVX_MANAGED_LIFECYCLE_TIMEOUT"], "5")
        self.assertEqual(
            environment["NVX_MANAGED_LIFECYCLE_OUTPUT_DIR"],
            os.fspath(output_dir.resolve()),
        )
        openvmm = json.loads(environment["NVX_MANAGED_LIFECYCLE_COMMAND"])
        # Cargo runs the test in the nvx-rs directory, so every path is absolute.
        for option in ("--kernel", "--initrd", "--microvm-report"):
            self.assertTrue(Path(openvmm[openvmm.index(option) + 1]).is_absolute())
        self.assertTrue(Path(openvmm[0]).is_absolute())
        # The microVM console listens on TCP, at the address that the test
        # copies it from.
        self.assertEqual(
            openvmm[openvmm.index("--virtio-console") + 1],
            f"listen=tcp:{environment['NVX_MANAGED_LIFECYCLE_CONSOLE']}",
        )
        self.assertRegex(
            environment["NVX_MANAGED_LIFECYCLE_CONSOLE"], r"\A127\.0\.0\.1:[0-9]+\Z"
        )
        self.assertEqual(
            openvmm[openvmm.index("--microvm-control-console") + 1],
            f"listen={environment['NVX_MANAGED_LIFECYCLE_ENDPOINT']}",
        )
        for option, value in (
            ("--microvm-workload-identity", "65534:65534"),
            ("--microvm-lifecycle", "managed"),
        ):
            self.assertEqual(openvmm[openvmm.index(option) + 1], value)
        self.assertIn("--microvm-control-auth-stdin", openvmm)
        self.assertEqual(preserved, self._managed_report())

    def test_managed_lifecycle_hands_the_test_an_absolute_pid_journal(self):
        # The nvx-rs test records OpenVMM in an adversarial campaign's journal
        # from the nvx-rs directory, so a relative journal resolves here.
        variable = "NVX_ADVERSARIAL_OPENVMM_PID_JOURNAL"
        relative = os.path.join("journal", "openvmm-pids.jsonl")
        for value, expected in (
            (relative, os.path.abspath(relative)),
            (os.path.abspath(relative), os.path.abspath(relative)),
            (None, None),
        ):
            with (
                self.subTest(value=value),
                tempfile.TemporaryDirectory() as temporary,
                patch.dict(os.environ),
            ):
                os.environ.pop(variable, None)
                if value is not None:
                    os.environ[variable] = value
                runs = self._run_managed_lifecycle(Path(temporary))
                self.assertEqual(runs[0][1].get(variable), expected)

    def test_managed_lifecycle_requires_cargo(self):
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(microvm_tests.shutil, "which", return_value=None),
            patch.object(microvm_tests.subprocess, "Popen") as popen,
            self.assertRaisesRegex(
                common.ScriptError,
                "runs the nvx-rs test managed_lifecycle with Cargo, which is not "
                "on PATH",
            ),
        ):
            microvm_tests.run_managed_lifecycle(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initramfs.cpio.gz"),
                "kvm",
                memory_mib=256,
                timeout=5,
                output_dir=Path(temporary),
            )
        popen.assert_not_called()

    def test_an_interrupted_managed_lifecycle_ends_the_test_and_its_openvmm(self):
        # A SIGINT that reaches nvx.py alone leaves Cargo's descendants, the
        # test and its OpenVMM, running; the scenario ends and reaps the whole
        # tree while the temporary directory that they use still exists.
        test = MagicMock()
        test.wait.side_effect = KeyboardInterrupt
        roots: list[Path] = []

        def start_test(command: list[str], **options: Any) -> MagicMock:
            openvmm = json.loads(options["env"]["NVX_MANAGED_LIFECYCLE_COMMAND"])
            roots.append(Path(openvmm[openvmm.index("--microvm-report") + 1]).parent)
            return test

        def terminate(process: object, *, process_group: bool) -> None:
            self.assertIs(process, test)
            self.assertFalse(process_group)
            self.assertTrue(roots[0].is_dir())

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(microvm_tests.shutil, "which", return_value="cargo"),
            patch.object(microvm_tests.subprocess, "Popen", side_effect=start_test),
            patch.object(
                microvm_tests, "terminate_process_tree", side_effect=terminate
            ) as terminate_tree,
            patch.object(microvm_tests, "OpenvmmProcess") as process,
            self.assertRaises(KeyboardInterrupt),
        ):
            microvm_tests.run_managed_lifecycle(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initramfs.cpio.gz"),
                "kvm",
                memory_mib=256,
                timeout=5,
                output_dir=Path(temporary),
            )
        terminate_tree.assert_called_once()
        self.assertFalse(roots[0].exists())
        # The scenario ended there, before its other boot.
        process.assert_not_called()

    def test_managed_lifecycle_fails_with_the_test_and_with_the_report(self):
        violation = f"{time_abi.VIOLATION_PREFIX}code=bogus\n".encode()
        failures: tuple[tuple[dict[str, Any], type[Exception], str], ...] = (
            (
                {"returncode": 101},
                RuntimeError,
                "the managed lifecycle test managed_lifecycle failed with exit "
                "status 101",
            ),
            # The guest's own explanation comes before the test's failure.
            (
                {"returncode": 101, "console": violation},
                time_abi.TimeAbiFailure,
                "guest reported time ABI violation",
            ),
            (
                {"report": self._managed_report(backend="whp")},
                RuntimeError,
                "managed lifecycle outcome report was invalid",
            ),
            (
                {"report": {**self._managed_report(), "teardown": {}}},
                RuntimeError,
                "structured outcome report has an invalid teardown outcome",
            ),
            (
                {"rejected": 0},
                RuntimeError,
                "managed lifecycle without a control endpoint was not rejected",
            ),
        )
        for options, error, message in failures:
            with (
                self.subTest(message=message),
                tempfile.TemporaryDirectory() as temporary,
                self.assertRaisesRegex(error, re.escape(message)),
            ):
                self._run_managed_lifecycle(Path(temporary), **options)

    def test_host_loopback_port_candidates_are_spread_across_the_dynamic_range(self):
        first, last = microvm_tests.HOST_LOOPBACK_PORT_RANGE
        candidates = microvm_tests._host_loopback_port_candidates()

        self.assertEqual(
            len(set(candidates)), microvm_tests.HOST_LOOPBACK_PORT_BIND_ATTEMPTS
        )
        self.assertTrue(all(first <= port <= last for port in candidates))
        # Consecutive candidates could all fall in one 100-port exclusion (#473).
        # Independent draws fit in 100 ports with a probability below 1e-30.
        self.assertGreaterEqual(max(candidates) - min(candidates), 100)

    def test_host_loopback_listener_pair_escapes_a_tcp_only_exclusion_block(self):
        # Windows excludes ports per protocol, usually 100 at a time, so a block
        # that excludes only TCP can outlast the attempt budget (#473).
        excluded = range(58912, 59012)
        attempts = microvm_tests.HOST_LOOPBACK_PORT_BIND_ATTEMPTS
        self.assertGreater(len(excluded), attempts)
        candidates = [*excluded[: attempts - 1], 61000]
        sockets = _PortExclusionSockets(tcp=excluded)

        with (
            patch.object(
                microvm_tests,
                "_host_loopback_port_candidates",
                return_value=candidates,
            ),
            patch.object(microvm_tests.socket, "socket", side_effect=sockets),
        ):
            tcp_listener, udp_listener = microvm_tests._bind_tcp_udp_listener_pair(
                5.0, microvm_tests.NETWORK_NEGATIVE_OBSERVATION_TIMEOUT_SECONDS
            )

        udp_sockets = sockets.created[socket.SOCK_DGRAM]
        tcp_sockets = sockets.created[socket.SOCK_STREAM]
        self.assertEqual(
            [endpoint.bind.call_args.args[0] for endpoint in udp_sockets],
            [("127.0.0.1", port) for port in candidates],
        )
        self.assertEqual(
            [endpoint.bind.call_args.args[0] for endpoint in tcp_sockets],
            [("0.0.0.0", port) for port in candidates],
        )
        self.assertIs(tcp_listener, tcp_sockets[-1])
        self.assertIs(udp_listener, udp_sockets[-1])
        for partial in (*udp_sockets[:-1], *tcp_sockets[:-1]):
            partial.close.assert_called_once_with()
        tcp_sockets[-1].close.assert_not_called()
        udp_sockets[-1].close.assert_not_called()
        tcp_sockets[-1].listen.assert_called_once_with(1)
        tcp_sockets[-1].settimeout.assert_called_once_with(5.0)
        udp_sockets[-1].settimeout.assert_called_once_with(
            microvm_tests.NETWORK_NEGATIVE_OBSERVATION_TIMEOUT_SECONDS
        )

    def test_host_loopback_listener_pair_reports_every_refused_port(self):
        attempts = microvm_tests.HOST_LOOPBACK_PORT_BIND_ATTEMPTS
        candidates = [50000, *range(58912, 58912 + attempts - 1)]
        sockets = _PortExclusionSockets(tcp=range(58912, 59012), udp=(50000,))

        with (
            patch.object(
                microvm_tests,
                "_host_loopback_port_candidates",
                return_value=candidates,
            ),
            patch.object(microvm_tests.socket, "socket", side_effect=sockets),
            self.assertRaises(RuntimeError) as raised,
        ):
            microvm_tests._bind_tcp_udp_listener_pair(5.0, 1.0)

        refused = ", ".join(["50000/udp", *(f"{port}/tcp" for port in candidates[1:])])
        self.assertEqual(
            str(raised.exception),
            "failed to allocate a port available to both TCP and UDP after "
            f"{attempts} attempts; refused {refused}; "
            f"last error: {sockets.errors[-1]}",
        )
        self.assertIs(raised.exception.__cause__, sockets.errors[-1])
        self.assertEqual(len(sockets.errors), attempts)
        for endpoint in (
            *sockets.created[socket.SOCK_DGRAM],
            *sockets.created[socket.SOCK_STREAM],
        ):
            endpoint.close.assert_called_once_with()

    def test_host_loopback_rejections_cover_snapshots_narrow_networks_and_denial(
        self,
    ):
        with tempfile.TemporaryDirectory() as temporary:
            with patch.object(microvm_tests, "OpenvmmProcess") as process:
                process.return_value.__enter__.return_value.wait.side_effect = [
                    openvmm_process.OpenvmmProcessResult(2, message)
                    for message in (
                        b"microVM snapshots do not support --host-loopback allow",
                        b"--host-loopback allow requires an IPv4 network of "
                        b"/29 or larger",
                        b"--host-loopback-forward requires explicit --host-loopback allow",
                        b"must match the guest gateway",
                    )
                ]
                microvm_tests.run_host_loopback_rejections(
                    Path("openvmm"),
                    Path("kernel"),
                    Path("initrd"),
                    "whp",
                    memory_mib=128,
                    timeout=40,
                    output_dir=Path(temporary),
                )
        commands = [call.args[0] for call in process.call_args_list]
        self.assertEqual(len(commands), 4)
        self.assertEqual(
            [command[command.index("--host-loopback") + 1] for command in commands],
            ["allow", "allow", "deny", "deny"],
        )
        self.assertEqual(
            [command[command.index("--net") + 1] for command in commands],
            ["192.0.2.2/24", "192.0.2.2/30", "192.0.2.2/24", "192.0.2.2/24"],
        )
        self.assertIn("--snapshot-destination", commands[0])
        self.assertIn("--pidfile", commands[0])
        self.assertNotIn("--host-loopback-forward", commands[0])
        self.assertNotIn("--snapshot-destination", commands[1])
        self.assertNotIn("--host-loopback-forward", commands[1])
        self.assertIn("--host-loopback-forward", commands[2])
        self.assertIn("--network-proxy", commands[3])

    def test_host_loopback_rejection_requires_diagnostic_and_no_boot(self):
        diagnostic = b"microVM snapshots do not support --host-loopback allow"
        for result in (
            openvmm_process.OpenvmmProcessResult(0, diagnostic),
            openvmm_process.OpenvmmProcessResult(2, b"failed to create pidfile"),
            openvmm_process.OpenvmmProcessResult(
                2, diagnostic + b"\n" + microvm_tests.BOOT_MARKER
            ),
        ):
            with self.subTest(result=result):
                with tempfile.TemporaryDirectory() as temporary:
                    with patch.object(microvm_tests, "OpenvmmProcess") as process:
                        process.return_value.__enter__.return_value.wait.return_value = result
                        with self.assertRaisesRegex(RuntimeError, "before boot"):
                            microvm_tests.run_host_loopback_rejections(
                                Path("openvmm"),
                                Path("kernel"),
                                Path("initrd"),
                                "whp",
                                memory_mib=128,
                                timeout=40,
                                output_dir=Path(temporary),
                            )

    def test_host_loopback_scenario_detects_udp_proxy_leak(self):
        def send_forbidden_udp(
            command: list[str], script: str, *_args: object, **_kwargs: object
        ):
            if "--host-loopback" not in command:
                with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sender:
                    ports = {
                        int(line.split()[5])
                        for line in script.splitlines()
                        if line.strip().startswith("nc -u -w 1")
                    }
                    for port in ports:
                        sender.sendto(
                            b"NVX-HOST-LOOPBACK-UDP-CONTROL",
                            ("127.0.0.1", port),
                        )
                return
            self.assertEqual(command[command.index("--network-egress") + 1], "allow")
            self.assertEqual(command[command.index("--host-loopback") + 1], "deny")
            proxy = command[command.index("--network-proxy") + 1]
            port = int(proxy.rsplit(":", 1)[1])
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sender:
                sender.sendto(b"forbidden-proxy-udp", ("127.0.0.1", port))

        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(microvm_tests, "_http_server"),
                patch.object(
                    microvm_tests, "run_guest_script", side_effect=send_forbidden_udp
                ),
            ):
                with self.assertRaisesRegex(RuntimeError, "reached a host UDP service"):
                    microvm_tests.run_host_loopback_policy(
                        Path("openvmm"),
                        Path("kernel"),
                        Path("initrd"),
                        "whp",
                        memory_mib=128,
                        timeout=40,
                        output_dir=Path(temporary),
                    )

    def test_host_loopback_scenario_requires_observed_udp_control(self):
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(microvm_tests, "_http_server"),
                patch.object(microvm_tests, "run_guest_script") as guest,
                patch.object(microvm_tests, "OpenvmmProcess") as process,
                patch.object(microvm_tests.socket, "create_connection"),
                patch.object(microvm_tests.time, "sleep"),
                patch.object(microvm_tests, "run_host_loopback_rejections"),
            ):
                process.return_value.__enter__.return_value.wait.return_value = (
                    openvmm_process.OpenvmmProcessResult(0, b"")
                )
                with self.assertRaisesRegex(RuntimeError, "UDP positive control"):
                    microvm_tests.run_host_loopback_policy(
                        Path("openvmm"),
                        Path("kernel"),
                        Path("initrd"),
                        "whp",
                        memory_mib=128,
                        timeout=5,
                        output_dir=Path(temporary),
                    )
                self.assertEqual(guest.call_count, 1)
                self.assertNotIn("--host-loopback", guest.call_args.args[0])
                process.assert_not_called()

    def _run_private_ingress_script(
        self,
        *,
        ingress: str,
        expect: str,
        cmdline: str,
        ingress_running: bool,
        tcp_request: bool,
        udp_request: bool,
        outbound_status: int = 1,
        outbound_port: str = "0",
    ) -> subprocess.CompletedProcess[str]:
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        header = "  sl  local_address rem_address st\n"
        tcp6 = header + (
            f"   0: {'0' * 32}:1F90 {'0' * 32}:0000 0A 00000000:00000000 00:00000000\n"
        )
        udp6 = header + (
            f"   0: {'0' * 32}:1F90 {'0' * 32}:0000 07 00000000:00000000 00:00000000\n"
        )
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "proc" / "net").mkdir(parents=True)
            (root / "proc" / "cmdline").write_text(cmdline + "\n", encoding="utf-8")
            for name, table in (
                ("tcp", header),
                ("tcp6", tcp6),
                ("udp", header),
                ("udp6", udp6),
            ):
                (root / "proc" / "net" / name).write_text(table, encoding="utf-8")
            script = (
                microvm_tests._render_script(
                    "private-network-ingress.sh.in",
                    INGRESS=ingress,
                    EXPECT=expect,
                    TCP_PORT="8080",
                    UDP_PORT="8080",
                    GATEWAY_IPV4="192.0.2.1",
                    OUTBOUND_PORT=outbound_port,
                    COMPLETION_MARKER="NVX-TEST-OK",
                )
                .replace("nvx-exit", "nvx_exit")
                .replace("/proc/", f"{root.as_posix()}/proc/")
                .replace("/tmp/nvx-ingress-", f"{root.as_posix()}/nvx-ingress-")
            )
            tcp = "echo NVX-PRIVATE-INGRESS-TCP-REQUEST" if tcp_request else ":"
            udp = "echo NVX-PRIVATE-INGRESS-UDP-REQUEST" if udp_request else ":"
            # The stubbed servers finish at once; wait for their output before
            # releasing the guest.
            script = script.replace("read -r release", "wait; read -r release")
            path = root / "private-network-ingress.sh"
            path.write_text(
                "sleep() { :; }\n"
                # The script's input is a pipe here, not the guest's terminal.
                'stty() { [ "$*" = -echo ]; }\n'
                f"pidof() {{ return {0 if ingress_running else 1}; }}\n"
                'nc() { case "$*" in\n'
                f"'-l -p '*) {tcp} ;;\n"
                f"'-u -l -p '*) {udp} ;;\n"
                f"*) return {outbound_status} ;;\n"
                "esac; }\n"
                'nvx_exit() { exit "$1"; }\n' + script,
                encoding="utf-8",
                newline="\n",
            )
            result = subprocess.run(
                [shell, path.as_posix()],
                input=PRIVATE_INGRESS_RELEASE,
                capture_output=True,
                timeout=10,
                check=False,
            )
            return subprocess.CompletedProcess(
                result.args,
                result.returncode,
                result.stdout.decode(errors="replace"),
                result.stderr.decode(errors="replace"),
            )

    def test_private_network_ingress_script_checks_policy_and_traffic(self):
        token = "console=hvc0 virtnet_ip=192.0.2.2 virtnet_ingress=allow"
        plain = "console=hvc0 virtnet_ip=192.0.2.2"
        for label, run, expected in (
            (
                "reached",
                functools.partial(
                    self._run_private_ingress_script,
                    ingress="allow",
                    expect="reached",
                    cmdline=token,
                    ingress_running=True,
                    tcp_request=True,
                    udp_request=True,
                    outbound_port="4000",
                ),
                0,
            ),
            (
                "unpublished",
                functools.partial(
                    self._run_private_ingress_script,
                    ingress="deny",
                    expect="unreached",
                    cmdline=plain,
                    ingress_running=False,
                    tcp_request=False,
                    udp_request=False,
                ),
                0,
            ),
            (
                "token-missing",
                functools.partial(
                    self._run_private_ingress_script,
                    ingress="allow",
                    expect="reached",
                    cmdline=plain,
                    ingress_running=True,
                    tcp_request=True,
                    udp_request=True,
                ),
                72,
            ),
            (
                "announcer-missing",
                functools.partial(
                    self._run_private_ingress_script,
                    ingress="allow",
                    expect="reached",
                    cmdline=token,
                    ingress_running=False,
                    tcp_request=True,
                    udp_request=True,
                ),
                71,
            ),
            (
                "announcer-unexpected",
                functools.partial(
                    self._run_private_ingress_script,
                    ingress="deny",
                    expect="unreached",
                    cmdline=plain,
                    ingress_running=True,
                    tcp_request=False,
                    udp_request=False,
                ),
                73,
            ),
            (
                "tcp-missed",
                functools.partial(
                    self._run_private_ingress_script,
                    ingress="allow",
                    expect="reached",
                    cmdline=token,
                    ingress_running=True,
                    tcp_request=False,
                    udp_request=True,
                ),
                78,
            ),
            (
                "udp-leaked",
                functools.partial(
                    self._run_private_ingress_script,
                    ingress="allow",
                    expect="unreached",
                    cmdline=token,
                    ingress_running=True,
                    tcp_request=False,
                    udp_request=True,
                ),
                81,
            ),
            (
                "outbound-opened",
                functools.partial(
                    self._run_private_ingress_script,
                    ingress="allow",
                    expect="reached",
                    cmdline=token,
                    ingress_running=True,
                    tcp_request=True,
                    udp_request=True,
                    outbound_status=0,
                    outbound_port="4000",
                ),
                83,
            ),
            (
                "outbound-failed-to-run",
                functools.partial(
                    self._run_private_ingress_script,
                    ingress="allow",
                    expect="reached",
                    cmdline=token,
                    ingress_running=True,
                    tcp_request=True,
                    udp_request=True,
                    outbound_status=127,
                    outbound_port="4000",
                ),
                84,
            ),
        ):
            with self.subTest(label):
                result = run()
                output = result.stdout + result.stderr
                self.assertEqual(result.returncode, expected, output)
                # Policy mismatches fail before the servers start.
                self.assertEqual(
                    "NVX-PRIVATE-INGRESS-LISTENING" in result.stdout,
                    expected not in (71, 72, 73),
                    output,
                )
                if expected:
                    self.assertNotIn("NVX-TEST-OK", result.stdout)
                else:
                    self.assertIn("NVX-TEST-OK", result.stdout)

    def test_private_network_ingress_scenario_covers_three_policies(self):
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(microvm_tests, "_run_private_ingress_guest") as guest,
                patch.object(microvm_tests, "OpenvmmProcess") as process,
                patch.object(
                    microvm_tests,
                    "workload_boot_command",
                    side_effect=_fresh_openvmm_command,
                ),
            ):
                process.return_value.__enter__.return_value.wait.return_value = (
                    openvmm_process.OpenvmmProcessResult(
                        2, b"microVM snapshots do not support --network-ingress allow"
                    )
                )
                microvm_tests.run_private_network_ingress(
                    Path("openvmm"),
                    Path("kernel"),
                    Path("initrd"),
                    "whp",
                    memory_mib=128,
                    timeout=5,
                    output_dir=Path(temporary),
                )
        self.assertEqual(guest.call_count, 3)
        policies = [
            (
                call.args[0][call.args[0].index("--network-egress") + 1],
                call.args[0][call.args[0].index("--network-ingress") + 1],
                call.args[0][call.args[0].index("--host-loopback") + 1]
                if "--host-loopback" in call.args[0]
                else None,
                call.kwargs["ingress"],
                call.kwargs["expect"],
                call.kwargs["outbound_port"] != 0,
            )
            for call in guest.call_args_list
        ]
        self.assertEqual(
            policies,
            [
                ("deny", "allow", "allow", "allow", "reached", True),
                ("deny", "allow", "deny", "allow", "unreached", False),
                ("allow", "deny", None, "deny", "unreached", False),
            ],
        )
        snapshot = process.call_args.args[0]
        self.assertIn("--snapshot-destination", snapshot)
        self.assertEqual(snapshot[snapshot.index("--network-ingress") + 1], "allow")

    def test_private_network_ingress_snapshot_must_be_rejected_before_boot(self):
        diagnostic = b"microVM snapshots do not support --network-ingress allow"
        for result in (
            openvmm_process.OpenvmmProcessResult(0, diagnostic),
            openvmm_process.OpenvmmProcessResult(2, b"other failure"),
            openvmm_process.OpenvmmProcessResult(
                2, diagnostic + b"\n" + microvm_tests.BOOT_MARKER
            ),
        ):
            with (
                self.subTest(result=result),
                tempfile.TemporaryDirectory() as temporary,
                patch.object(microvm_tests, "_run_private_ingress_guest"),
                patch.object(microvm_tests, "OpenvmmProcess") as process,
                patch.object(
                    microvm_tests,
                    "workload_boot_command",
                    side_effect=_fresh_openvmm_command,
                ),
            ):
                process.return_value.__enter__.return_value.wait.return_value = result
                with self.assertRaisesRegex(RuntimeError, "before boot"):
                    microvm_tests.run_private_network_ingress(
                        Path("openvmm"),
                        Path("kernel"),
                        Path("initrd"),
                        "whp",
                        memory_mib=128,
                        timeout=5,
                        output_dir=Path(temporary),
                    )

    def test_publication_probe_detects_a_held_port(self):
        holder = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            holder.bind(("0.0.0.0", 0))
            port = int(holder.getsockname()[1])
            self.assertTrue(microvm_tests._publishes(port, socket.SOCK_STREAM))
        finally:
            holder.close()
        self.assertFalse(microvm_tests._publishes(port, socket.SOCK_STREAM))
        port = microvm_tests._private_ingress_port()
        self.assertFalse(microvm_tests._publishes(port, socket.SOCK_STREAM))
        self.assertFalse(microvm_tests._publishes(port, socket.SOCK_DGRAM))

    def test_host_loopback_udp_probes_reject_failed_sender(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        for mode, status, expected in (
            ("udp-control", 1, 99),
            ("udp-control", 127, 99),
            ("deny", 126, 99),
            ("deny", 127, 99),
            ("deny", 1, 0),
        ):
            with (
                self.subTest(mode=mode, status=status),
                tempfile.TemporaryDirectory() as temporary,
            ):
                cmdline = Path(temporary) / "cmdline"
                cmdline.write_text(
                    "console=hvc0 virtnet_ip=192.0.2.2\n", encoding="utf-8"
                )
                script = (
                    microvm_tests._render_script(
                        "host-loopback-policy.sh.in",
                        MODE=mode,
                        GATEWAY_IPV4="10.0.0.1",
                        GATEWAY_IPV6="fd00::a00:1",
                        GENERAL_PORT="8444",
                        PROXY_PORT="8443",
                        GUEST_PORT="0",
                        PRIVATE_GUEST_PORT="0",
                        UDP_IPV6_GUEST_PORT="0",
                        UDP_ECHO_PORT="0",
                        UDP_ECHO_IPV6_PORT="0",
                    )
                    .replace("nvx-exit", "nvx_exit")
                    .replace("/proc/cmdline", cmdline.as_posix())
                )
                result = subprocess.run(
                    [shell, "-s"],
                    input=(
                        f"nc() {{ return {status}; }}\n"
                        "pidof() { return 1; }\n"
                        'wget() { case "$*" in\n'
                        "*/general) return 1 ;;\n"
                        "*/proxy) echo NVX-HOST-LOOPBACK-PROXY ;;\n"
                        "esac; }\n"
                        'nvx_exit() { exit "$1"; }\n' + script
                    ),
                    text=True,
                    capture_output=True,
                    timeout=5,
                    check=False,
                )
                self.assertEqual(
                    result.returncode, expected, result.stdout + result.stderr
                )
                self.assertNotIn("UDP-CONTROL-OK", result.stdout)
                if expected:
                    self.assertNotIn("DENY-OK", result.stdout)

    def test_host_loopback_deny_requires_no_publication(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        for label, cmdline, announcer in (
            ("token", "virtnet_ip=192.0.2.2 virtnet_ingress=host-loopback", 1),
            ("announcer", "virtnet_ip=192.0.2.2", 0),
        ):
            with self.subTest(label), tempfile.TemporaryDirectory() as temporary:
                path = Path(temporary) / "cmdline"
                path.write_text(cmdline + "\n", encoding="utf-8")
                script = (
                    microvm_tests._render_script(
                        "host-loopback-policy.sh.in",
                        MODE="deny",
                        GATEWAY_IPV4="10.0.0.1",
                        GATEWAY_IPV6="fd00::a00:1",
                        GENERAL_PORT="8444",
                        PROXY_PORT="8443",
                        GUEST_PORT="0",
                        PRIVATE_GUEST_PORT="0",
                        UDP_IPV6_GUEST_PORT="0",
                        UDP_ECHO_PORT="0",
                        UDP_ECHO_IPV6_PORT="0",
                    )
                    .replace("nvx-exit", "nvx_exit")
                    .replace("/proc/cmdline", path.as_posix())
                )
                result = subprocess.run(
                    [shell, "-s"],
                    input=(
                        "nc() { return 1; }\n"
                        f"pidof() {{ return {announcer}; }}\n"
                        'wget() { case "$*" in\n'
                        "*/general) return 1 ;;\n"
                        "*/proxy) echo NVX-HOST-LOOPBACK-PROXY ;;\n"
                        "esac; }\n"
                        'nvx_exit() { exit "$1"; }\n' + script
                    ),
                    text=True,
                    capture_output=True,
                    timeout=5,
                    check=False,
                )
                self.assertEqual(result.returncode, 89, result.stdout + result.stderr)
                self.assertNotIn("DENY-OK", result.stdout)

    def test_host_loopback_script_probes_udp_on_proxy_and_general_ports(self):
        script = microvm_tests._render_script(
            "host-loopback-policy.sh.in",
            MODE="deny",
            GATEWAY_IPV4="10.0.0.1",
            GATEWAY_IPV6="fd00::a00:1",
            GENERAL_PORT="8444",
            PROXY_PORT="8443",
            GUEST_PORT="0",
            PRIVATE_GUEST_PORT="0",
            UDP_IPV6_GUEST_PORT="0",
            UDP_ECHO_PORT="0",
            UDP_ECHO_IPV6_PORT="0",
        )
        self.assertNotRegex(script, "@[A-Z0-9_]+@")
        self.assertIn("nc -u -w 1 10.0.0.1 8443", script)
        self.assertIn("nc -u -w 1 10.0.0.1 8444", script)
        self.assertIn("http://10.0.0.1:8443/proxy", script)

    def _run_host_loopback_allow_script(
        self,
        *,
        cmdline: str = "virtnet_ip=192.0.2.2 virtnet_ingress=host-loopback",
        announcer: bool = True,
        tcp_request: bool = True,
        udp_request: bool = True,
        udp_ipv6_request: bool = True,
        private_request: bool = False,
        listeners: bool = True,
        udp_ipv6_listener: bool = True,
        private_listeners: bool = True,
        udp_echo: bool = True,
        udp_echo_ipv6: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        header = "  sl  local_address rem_address st\n"
        tail = " 00000000:00000000 00:00000000\n"
        tcp = header
        tcp6 = header
        udp6 = header
        if listeners:
            tcp6 += f"   0: {'0' * 32}:1F90 {'0' * 32}:0000 0A{tail}"
            udp6 += f"   0: {'0' * 32}:1F90 {'0' * 32}:0000 07{tail}"
        if udp_ipv6_listener:
            udp6 += f"   1: {'0' * 32}:1F92 {'0' * 32}:0000 07{tail}"
        if private_listeners:
            tcp += f"   0: 0100007F:1F91 00000000:0000 0A{tail}"
            tcp6 += f"   1: {'0' * 24}01000000:1F91 {'0' * 32}:0000 0A{tail}"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "proc" / "net").mkdir(parents=True)
            (root / "proc" / "cmdline").write_text(cmdline + "\n", encoding="utf-8")
            for name, table in (
                ("tcp", tcp),
                ("tcp6", tcp6),
                ("udp", header),
                ("udp6", udp6),
            ):
                (root / "proc" / "net" / name).write_text(table, encoding="utf-8")
            script = (
                microvm_tests._render_script(
                    "host-loopback-policy.sh.in",
                    MODE="allow",
                    GATEWAY_IPV4="10.0.0.1",
                    GATEWAY_IPV6="fd00::a00:1",
                    GENERAL_PORT="8444",
                    PROXY_PORT="0",
                    GUEST_PORT="8080",
                    PRIVATE_GUEST_PORT="8081",
                    UDP_IPV6_GUEST_PORT="8082",
                    UDP_ECHO_PORT="8083",
                    UDP_ECHO_IPV6_PORT="8084",
                )
                .replace("nvx-exit", "nvx_exit")
                .replace("/proc/", f"{root.as_posix()}/proc/")
                .replace(
                    "/tmp/nvx-host-loopback-", f"{root.as_posix()}/nvx-host-loopback-"
                )
                # The stubbed servers finish at once; wait for their output
                # before releasing the guest.
                .replace("read -r release", "wait; read -r release")
            )
            tcp_server = "echo NVX-HOST-LOOPBACK-INBOUND" if tcp_request else ":"
            udp_server = "echo NVX-HOST-LOOPBACK-UDP" if udp_request else ":"
            udp_ipv6_server = "echo NVX-HOST-LOOPBACK-UDP" if udp_ipv6_request else ":"
            private_server = (
                "echo NVX-HOST-LOOPBACK-INBOUND" if private_request else ":"
            )
            echo_reply = "echo NVX-HOST-LOOPBACK-UDP-ECHO-REPLY"
            udp_echo_server = echo_reply if udp_echo else ":"
            udp_echo_ipv6_server = echo_reply if udp_echo_ipv6 else ":"
            path = root / "host-loopback-policy.sh"
            path.write_text(
                "sleep() { :; }\n"
                # The script's input is a pipe here, not the guest's terminal.
                'stty() { [ "$*" = -echo ]; }\n'
                f"pidof() {{ return {0 if announcer else 1}; }}\n"
                "wget() { echo NVX-HOST-LOOPBACK-GENERAL; }\n"
                'nc() { case "$*" in\n'
                f"'-l -p '*) {tcp_server} ;;\n"
                f"'-u -l -p 8082 '*) {udp_ipv6_server} ;;\n"
                f"'-u -l -p '*) {udp_server} ;;\n"
                f"'-l -s '*) {private_server} ;;\n"
                f"'-u -w 2 10.0.0.1 8083') {udp_echo_server} ;;\n"
                f"'-u -w 2 fd00::a00:1 8084') {udp_echo_ipv6_server} ;;\n"
                "*) return 1 ;;\n"
                "esac; }\n"
                'nvx_exit() { exit "$1"; }\n' + script,
                encoding="utf-8",
                newline="\n",
            )
            result = subprocess.run(
                [shell, path.as_posix()],
                input=microvm_tests.HOST_LOOPBACK_CHECKED + b"\n",
                capture_output=True,
                timeout=10,
                check=False,
            )
            return subprocess.CompletedProcess(
                result.args,
                result.returncode,
                result.stdout.decode(errors="replace"),
                result.stderr.decode(errors="replace"),
            )

    def test_host_loopback_probe_reports_held_loopback_ports(self):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as held:
            held.bind(("127.0.0.1", 0))
            port = int(held.getsockname()[1])
            self.assertTrue(
                microvm_tests._holds_loopback(socket.AF_INET, socket.SOCK_STREAM, port)
            )
        self.assertFalse(
            microvm_tests._holds_loopback(socket.AF_INET, socket.SOCK_STREAM, port)
        )

    def test_host_loopback_tcp_exchange_retries_until_the_guest_answers(self):
        listener = socket.create_server(("127.0.0.1", 0))
        port = int(listener.getsockname()[1])
        received: list[bytes] = []

        def serve() -> None:
            # The first client closes before the server's line, as between
            # two of the guest's servers.
            first, _ = listener.accept()
            first.close()
            second, _ = listener.accept()
            with second:
                second.sendall(microvm_tests.HOST_LOOPBACK_TCP_ACK)
                data = b""
                while not data.endswith(b"\n"):
                    chunk = second.recv(64)
                    if not chunk:
                        break
                    data += chunk
                received.append(data)

        server = threading.Thread(target=serve, daemon=True)
        server.start()
        try:
            with patch.object(microvm_tests.time, "sleep"):
                microvm_tests._host_loopback_tcp_exchange("127.0.0.1", port, 10)
            server.join(10)
        finally:
            listener.close()
        self.assertEqual(received, [microvm_tests.HOST_LOOPBACK_TCP_REQUEST])

    def test_host_loopback_tcp_exchange_rejects_an_unexpected_reply(self):
        listener = socket.create_server(("127.0.0.1", 0))
        port = int(listener.getsockname()[1])

        def serve() -> None:
            client, _ = listener.accept()
            with client:
                client.sendall(b"UNEXPECTED\n")

        server = threading.Thread(target=serve, daemon=True)
        server.start()
        try:
            with self.assertRaisesRegex(RuntimeError, "unexpected guest TCP reply"):
                microvm_tests._host_loopback_tcp_exchange("127.0.0.1", port, 10)
            server.join(10)
        finally:
            listener.close()

    def test_host_loopback_udp_exchange_repeats_until_the_guest_answers(self):
        for address, family in (
            ("127.0.0.1", socket.AF_INET),
            ("::1", socket.AF_INET6),
        ):
            with self.subTest(address):
                server = socket.socket(family, socket.SOCK_DGRAM)
                try:
                    server.bind((address, 0))
                except OSError:
                    server.close()
                    continue
                port = int(server.getsockname()[1])
                received: list[bytes] = []

                def serve(
                    server: socket.socket = server,
                    received: list[bytes] = received,
                ) -> None:
                    # The guest misses the first request, as while it resolves
                    # the address at which it sees the host.
                    server.recvfrom(64)
                    data, peer = server.recvfrom(64)
                    received.append(data)
                    server.sendto(microvm_tests.HOST_LOOPBACK_UDP_REPLY, peer)

                thread = threading.Thread(target=serve, daemon=True)
                thread.start()
                try:
                    microvm_tests._host_loopback_udp_exchange(address, port, 10)
                    thread.join(10)
                finally:
                    server.close()
                self.assertEqual(received, [microvm_tests.HOST_LOOPBACK_UDP_REQUEST])

    def test_host_loopback_udp_exchange_rejects_unexpected_replies(self):
        for label, reply, other_port in (
            ("payload", b"UNEXPECTED\n", False),
            ("source", microvm_tests.HOST_LOOPBACK_UDP_REPLY, True),
        ):
            with (
                self.subTest(label),
                socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as server,
                socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as other,
            ):
                server.bind(("127.0.0.1", 0))
                other.bind(("127.0.0.1", 0))
                port = int(server.getsockname()[1])

                def serve(
                    server: socket.socket = server,
                    sender: socket.socket = other if other_port else server,
                    reply: bytes = reply,
                ) -> None:
                    _, peer = server.recvfrom(64)
                    sender.sendto(reply, peer)

                thread = threading.Thread(target=serve, daemon=True)
                thread.start()
                with self.assertRaisesRegex(RuntimeError, "unexpected guest UDP reply"):
                    microvm_tests._host_loopback_udp_exchange("127.0.0.1", port, 10)
                thread.join(10)

    def test_host_loopback_udp_exchange_times_out_without_an_answer(self):
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as server:
            server.bind(("127.0.0.1", 0))
            port = int(server.getsockname()[1])
            with self.assertRaisesRegex(TimeoutError, "did not answer"):
                microvm_tests._host_loopback_udp_exchange("127.0.0.1", port, 0)

    def test_udp_echo_server_answers_each_request(self):
        for address, family in (
            ("127.0.0.1", socket.AF_INET),
            ("::1", socket.AF_INET6),
        ):
            with self.subTest(address):
                server = socket.socket(family, socket.SOCK_DGRAM)
                try:
                    server.bind((address, 0))
                except OSError:
                    server.close()
                    continue
                stop = threading.Event()
                errors: list[Exception] = []
                thread = threading.Thread(
                    target=microvm_tests._udp_echo_server,
                    args=(server, stop, errors),
                    daemon=True,
                )
                thread.start()
                try:
                    with socket.socket(family, socket.SOCK_DGRAM) as client:
                        client.settimeout(5)
                        client.connect(server.getsockname()[:2])
                        for _ in range(2):
                            client.send(microvm_tests.HOST_LOOPBACK_UDP_ECHO_REQUEST)
                            self.assertEqual(
                                client.recv(64),
                                microvm_tests.HOST_LOOPBACK_UDP_ECHO_REPLY,
                            )
                finally:
                    stop.set()
                    thread.join(10)
                    server.close()
                self.assertFalse(thread.is_alive())
                self.assertEqual(errors, [])

    def test_udp_echo_server_rejects_an_unexpected_request(self):
        with (
            socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as server,
            socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as client,
        ):
            server.bind(("127.0.0.1", 0))
            errors: list[Exception] = []
            thread = threading.Thread(
                target=microvm_tests._udp_echo_server,
                args=(server, threading.Event(), errors),
                daemon=True,
            )
            thread.start()
            client.sendto(b"UNEXPECTED\n", server.getsockname())
            thread.join(10)
            self.assertFalse(thread.is_alive())
        self.assertEqual(len(errors), 1)
        self.assertRegex(str(errors[0]), "unexpected host-loopback UDP echo request")

    def test_host_loopback_udp_echo_avoids_the_published_guest_ports(self):
        # The probes that drew the guest ports released them, so ephemeral binds
        # can draw them. Each rejected socket holds its port until one is free.
        sockets = _EphemeralPortSockets((8080, 8082, 50000))

        with patch.object(microvm_tests.socket, "socket", side_effect=sockets):
            udp_echo = microvm_tests._bind_host_loopback_udp_echo(
                socket.AF_INET6, "::1", (8080, 8082)
            )

        self.assertIs(udp_echo, sockets.created[-1])
        self.assertEqual(
            sockets.events,
            [
                ("bind", 8080),
                ("bind", 8082),
                ("bind", 50000),
                ("close", 8080),
                ("close", 8082),
            ],
        )
        for endpoint in sockets.created:
            endpoint.setsockopt.assert_called_once_with(
                socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1
            )
            endpoint.bind.assert_called_once_with(("::1", 0))

    def test_host_loopback_udp_echo_gives_up_after_the_attempt_budget(self):
        attempts = microvm_tests.HOST_LOOPBACK_PORT_BIND_ATTEMPTS
        published = range(50000, 50000 + attempts)
        sockets = _EphemeralPortSockets(published)

        with (
            patch.object(microvm_tests.socket, "socket", side_effect=sockets),
            self.assertRaises(RuntimeError) as raised,
        ):
            microvm_tests._bind_host_loopback_udp_echo(
                socket.AF_INET, "127.0.0.1", published
            )

        self.assertEqual(
            str(raised.exception),
            "failed to bind a UDP port on 127.0.0.1 outside guest ports "
            f"{list(published)} after {attempts} attempts",
        )
        self.assertEqual(len(sockets.created), attempts)
        for endpoint in sockets.created:
            endpoint.setsockopt.assert_not_called()
            endpoint.close.assert_called_once_with()

    def test_host_loopback_udp_echoes_avoid_both_published_guest_ports(self):
        # The guest's UDP servers are dual-stack, so OpenVMM publishes both of
        # their ports on both loopback addresses.
        def answer_udp_control(
            command: list[str], script: str, *_args: object, **_kwargs: object
        ) -> None:
            if "--host-loopback" in command:
                return
            ports = {
                int(line.split()[5])
                for line in script.splitlines()
                if line.strip().startswith("nc -u -w 1")
            }
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sender:
                for port in ports:
                    sender.sendto(b"NVX-HOST-LOOPBACK-UDP-CONTROL", ("127.0.0.1", port))

        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(microvm_tests, "_http_server"),
                patch.object(microvm_tests, "_udp_echo_server"),
                patch.object(
                    microvm_tests, "run_guest_script", side_effect=answer_udp_control
                ),
                patch.object(
                    microvm_tests,
                    "_host_loopback_publication_port",
                    side_effect=range(50001, 50101),
                ),
                patch.object(
                    microvm_tests, "_bind_host_loopback_udp_echo"
                ) as bind_udp_echo,
                patch.object(microvm_tests, "OpenvmmProcess") as process,
                patch.object(
                    microvm_tests, "_verify_host_loopback_publication"
                ) as verify,
                patch.object(microvm_tests, "run_host_loopback_rejections"),
            ):
                bind_udp_echo.return_value.getsockname.return_value = (
                    "127.0.0.1",
                    40000,
                )
                process.return_value.__enter__.return_value.wait.return_value = (
                    openvmm_process.OpenvmmProcessResult(0, b"")
                )
                microvm_tests.run_host_loopback_policy(
                    Path("openvmm"),
                    Path("kernel"),
                    Path("initrd"),
                    "whp",
                    memory_mib=128,
                    timeout=5,
                    output_dir=Path(temporary),
                )

        published = tuple(verify.call_args.args[:2])
        self.assertEqual(
            [bound.args for bound in bind_udp_echo.call_args_list],
            [
                (socket.AF_INET, "127.0.0.1", published),
                (socket.AF_INET6, "::1", published),
            ],
        )

    def test_host_loopback_allow_script_checks_publication_and_traffic(self):
        run = self._run_host_loopback_allow_script
        for label, check, expected in (
            ("published", run, 0),
            (
                "token-missing",
                functools.partial(run, cmdline="virtnet_ip=192.0.2.2"),
                86,
            ),
            (
                "ingress-token",
                functools.partial(
                    run, cmdline="virtnet_ip=192.0.2.2 virtnet_ingress=allow"
                ),
                86,
            ),
            ("announcer-missing", functools.partial(run, announcer=False), 87),
            ("listener-missing", functools.partial(run, listeners=False), 95),
            (
                "private-listener-missing",
                functools.partial(run, private_listeners=False),
                96,
            ),
            ("tcp-unreached", functools.partial(run, tcp_request=False), 97),
            ("udp-unreached", functools.partial(run, udp_request=False), 97),
            (
                "udp-ipv6-listener-missing",
                functools.partial(run, udp_ipv6_listener=False),
                95,
            ),
            (
                "udp-ipv6-unreached",
                functools.partial(run, udp_ipv6_request=False),
                97,
            ),
            ("private-reached", functools.partial(run, private_request=True), 96),
            ("udp-echo-unanswered", functools.partial(run, udp_echo=False), 84),
            (
                "udp-echo-ipv6-unanswered",
                functools.partial(run, udp_echo_ipv6=False),
                85,
            ),
        ):
            with self.subTest(label):
                result = check()
                self.assertEqual(
                    result.returncode, expected, result.stdout + result.stderr
                )
                if expected:
                    self.assertIn(
                        f"NVX-HOST-LOOPBACK-FAIL code={expected}", result.stdout
                    )
                    self.assertNotIn("NVX-HOST-LOOPBACK-ALLOW-OK", result.stdout)
                else:
                    self.assertIn("NVX-HOST-LOOPBACK-INGRESS-READY", result.stdout)
                    self.assertIn("NVX-HOST-LOOPBACK-ALLOW-OK", result.stdout)

    @staticmethod
    def _outcome_report(
        backend: str,
        *,
        outcome: dict[str, object],
        policy: dict[str, object],
    ) -> dict[str, object]:
        return {
            "schema_version": 1,
            "instance_id": "11" * 16,
            "backend": backend,
            "outcome": outcome,
            "network_policy": policy,
            "teardown": {name: True for name in microvm_tests.OUTCOME_TEARDOWN_FIELDS},
        }

    def test_workload_identity_scenario_checks_enforcement_and_rejection(self):
        with tempfile.TemporaryDirectory() as temporary:
            with patch.object(microvm_tests, "OpenvmmProcess") as process:
                process.return_value.__enter__.return_value.wait.side_effect = [
                    openvmm_process.OpenvmmProcessResult(
                        0, microvm_tests.WORKLOAD_IDENTITY_MARKER + b"\n"
                    ),
                    openvmm_process.OpenvmmProcessResult(
                        125, b"configured workload UID is unavailable\n"
                    ),
                    openvmm_process.OpenvmmProcessResult(
                        2, b"microVM workload UID must be nonzero\n"
                    ),
                ]
                microvm_tests.run_workload_identity(
                    Path("openvmm"),
                    Path("kernel"),
                    Path("initrd"),
                    "whp",
                    memory_mib=128,
                    timeout=40,
                    output_dir=Path(temporary),
                )

        self.assertEqual(process.call_count, 3)
        commands = [call.args[0] for call in process.call_args_list]
        self.assertEqual(
            [
                command[command.index("--microvm-workload-identity") + 1]
                for command in commands
            ],
            ["65534:65534", "12345:12345", "0:0"],
        )
        self.assertTrue(
            all(
                "nvx_exec=/sbin/nvx-identity-probe"
                in command[command.index("--cmdline") + 1]
                for command in commands
            )
        )

    def test_structured_outcome_scenario_covers_exit_policy_and_rejection(self):
        applied = self._outcome_report(
            "whp",
            outcome={
                "operation": "run",
                "category": "guest-exit",
                "status_code": 37,
            },
            policy={
                "status": "applied",
                "status_code": 0,
                "mode": "rules",
                "allow_rule_count": 2,
                "deny_rule_count": 1,
                "host_loopback": "deny",
            },
        )
        rejected = self._outcome_report(
            "whp",
            outcome={
                "operation": "run",
                "category": "vmm-failure",
                "status_code": 1,
            },
            policy={
                "status": "failed",
                "status_code": 1,
                "mode": "rules",
                "allow_rule_count": 1,
                "deny_rule_count": 0,
                "host_loopback": "allow",
            },
        )
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(microvm_tests, "OpenvmmProcess") as process,
                patch.object(
                    microvm_tests,
                    "_read_outcome_report",
                    side_effect=(applied, rejected),
                ),
            ):
                active = process.return_value.__enter__.return_value
                active.wait.side_effect = (
                    openvmm_process.OpenvmmProcessResult(
                        37, b"sensitive-output-value\n"
                    ),
                    openvmm_process.OpenvmmProcessResult(
                        2, b"--network-egress is required\n"
                    ),
                )
                microvm_tests.run_structured_outcome(
                    Path("openvmm"),
                    Path("kernel"),
                    Path("initrd"),
                    "whp",
                    memory_mib=128,
                    timeout=40,
                    output_dir=Path(temporary),
                )

        self.assertEqual(process.call_count, 2)
        applied_command = process.call_args_list[0].args[0]
        self.assertIn("--microvm-report", applied_command)
        self.assertEqual(
            applied_command.count("--network-egress-allow"),
            2,
        )
        self.assertEqual(
            applied_command.count("--network-egress-deny"),
            1,
        )
        self.assertEqual(
            applied_command[applied_command.index("--host-loopback") + 1],
            "deny",
        )
        rejected_command = process.call_args_list[1].args[0]
        self.assertNotIn("--network-egress", rejected_command)
        self.assertIn("--network-egress-allow", rejected_command)
        active.send_line.assert_called_once_with(
            "printf 'sensitive-output-value\\n'; /sbin/nvx-exit 37"
        )

    def test_structured_outcome_rejects_boolean_schema_version(self):
        report = self._outcome_report(
            "whp",
            outcome={
                "operation": "run",
                "category": "guest-exit",
                "status_code": 0,
            },
            policy={
                "status": "applied",
                "status_code": 0,
                "mode": "rules",
                "allow_rule_count": 0,
                "deny_rule_count": 0,
                "host_loopback": "deny",
            },
        )
        report["schema_version"] = True
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "outcome.json"
            path.write_text(json.dumps(report), encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "unsupported version"):
                microvm_tests._read_outcome_report(path)

    def test_structured_outcome_preserves_invalid_primary_report(self):
        report = self._outcome_report(
            "whp",
            outcome={
                "operation": "run",
                "category": "guest-exit",
                "status_code": 37,
            },
            policy={
                "status": "applied",
                "status_code": 0,
                "mode": "rules",
                "allow_rule_count": 2,
                "deny_rule_count": 1,
                "host_loopback": "deny",
            },
        )
        teardown = cast(dict[str, object], report["teardown"])
        teardown[next(iter(teardown))] = False
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            with (
                patch.object(microvm_tests, "OpenvmmProcess") as process,
                patch.object(
                    microvm_tests,
                    "_read_outcome_report",
                    return_value=report,
                ),
            ):
                active = process.return_value.__enter__.return_value
                active.wait.return_value = openvmm_process.OpenvmmProcessResult(
                    37, b"sensitive-output-value\n"
                )
                with self.assertRaisesRegex(RuntimeError, "incomplete teardown"):
                    microvm_tests.run_structured_outcome(
                        Path("openvmm"),
                        Path("kernel"),
                        Path("initrd"),
                        "whp",
                        memory_mib=128,
                        timeout=40,
                        output_dir=output,
                    )
            preserved = json.loads(
                (output / "structured-outcome.json").read_text(encoding="utf-8")
            )
        self.assertEqual(preserved, report)

    def test_structured_outcome_preserves_invalid_rejection_report(self):
        applied = self._outcome_report(
            "whp",
            outcome={
                "operation": "run",
                "category": "guest-exit",
                "status_code": 37,
            },
            policy={
                "status": "applied",
                "status_code": 0,
                "mode": "rules",
                "allow_rule_count": 2,
                "deny_rule_count": 1,
                "host_loopback": "deny",
            },
        )
        rejected = self._outcome_report(
            "whp",
            outcome={
                "operation": "run",
                "category": "vmm-failure",
                "status_code": 1,
            },
            policy={
                "status": "failed",
                "status_code": 1,
                "mode": "rules",
                "allow_rule_count": 1,
                "deny_rule_count": 0,
                "host_loopback": "allow",
            },
        )
        teardown = cast(dict[str, object], rejected["teardown"])
        teardown[next(iter(teardown))] = False
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            with (
                patch.object(microvm_tests, "OpenvmmProcess") as process,
                patch.object(
                    microvm_tests,
                    "_read_outcome_report",
                    side_effect=(applied, rejected),
                ),
            ):
                active = process.return_value.__enter__.return_value
                active.wait.side_effect = (
                    openvmm_process.OpenvmmProcessResult(
                        37, b"sensitive-output-value\n"
                    ),
                    openvmm_process.OpenvmmProcessResult(
                        2, b"--network-egress is required\n"
                    ),
                )
                with self.assertRaisesRegex(RuntimeError, "leaked host resources"):
                    microvm_tests.run_structured_outcome(
                        Path("openvmm"),
                        Path("kernel"),
                        Path("initrd"),
                        "whp",
                        memory_mib=128,
                        timeout=40,
                        output_dir=output,
                    )
            preserved = json.loads(
                (output / "structured-outcome-rejected.json").read_text(
                    encoding="utf-8"
                )
            )
        self.assertEqual(preserved, rejected)

    def test_console_exit_preserves_full_output_and_guest_status(self):
        expected = (
            b"x" * microvm_tests.CONSOLE_EXIT_PAYLOAD_BYTES
            + b"\n"
            + microvm_tests.CONSOLE_EXIT_COMPLETION_MARKER
            + b"\n"
        )
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(microvm_tests, "capture_snapshot") as capture,
                patch.object(microvm_tests, "OpenvmmProcess") as process,
            ):
                process.return_value.__enter__.return_value.wait.side_effect = [
                    openvmm_process.OpenvmmProcessResult(
                        code, expected.replace(b"\n", b"\r\n")
                    )
                    for code in (0, 37)
                ]
                microvm_tests.run_console_exit(
                    Path("openvmm"),
                    Path("kernel"),
                    Path("initrd"),
                    "mshv",
                    2,
                    memory_mib=512,
                    timeout=40,
                    output_dir=Path(temporary),
                )

        self.assertEqual(capture.call_count, 2)
        self.assertEqual(process.call_count, 2)
        for call, code in zip(capture.call_args_list, (0, 37), strict=True):
            self.assertEqual(call.kwargs["processors"], 2)
            self.assertIn("head -c 65536", call.kwargs["post_restore_script"])
            self.assertIn(
                f"/sbin/nvx-exit {code}\n", call.kwargs["post_restore_script"]
            )
        for call in process.call_args_list:
            self.assertEqual(call.kwargs["output_read_delay"], 2.0)
            self.assertIn("--restore-snapshot", call.args[0])
            self.assertEqual(call.args[0][call.args[0].index("--processors") + 1], "2")

    def test_console_exit_rejects_truncation_even_if_marker_survives(self):
        marker = b"\n" + microvm_tests.CONSOLE_EXIT_COMPLETION_MARKER + b"\n"
        for output in (
            marker,
            b"x" * (microvm_tests.CONSOLE_EXIT_PAYLOAD_BYTES - 1) + marker,
            b"y" * microvm_tests.CONSOLE_EXIT_PAYLOAD_BYTES + marker,
            b"x" * microvm_tests.CONSOLE_EXIT_PAYLOAD_BYTES,
        ):
            with self.subTest(length=len(output)):
                with tempfile.TemporaryDirectory() as temporary:
                    with (
                        patch.object(microvm_tests, "capture_snapshot"),
                        patch.object(microvm_tests, "OpenvmmProcess") as process,
                    ):
                        process.return_value.__enter__.return_value.wait.return_value = openvmm_process.OpenvmmProcessResult(
                            0, output
                        )
                        with self.assertRaisesRegex(
                            RuntimeError, "truncated or corrupt console output"
                        ):
                            microvm_tests.run_console_exit(
                                Path("openvmm"),
                                Path("kernel"),
                                Path("initrd"),
                                "kvm",
                                2,
                                memory_mib=128,
                                timeout=40,
                                output_dir=Path(temporary),
                            )

    def test_console_exit_rejects_wrong_guest_status(self):
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(microvm_tests, "capture_snapshot"),
                patch.object(microvm_tests, "OpenvmmProcess") as process,
            ):
                process.return_value.__enter__.return_value.wait.return_value = (
                    openvmm_process.OpenvmmProcessResult(1, b"")
                )
                with self.assertRaisesRegex(
                    RuntimeError, "expected exit status 0, got 1"
                ):
                    microvm_tests.run_console_exit(
                        Path("openvmm"),
                        Path("kernel"),
                        Path("initrd"),
                        "whp",
                        2,
                        memory_mib=128,
                        timeout=40,
                        output_dir=Path(temporary),
                    )

    def test_output_read_delay_precedes_reader_start(self):
        events: list[tuple[str, float | None]] = []

        def record_delay(delay: float) -> None:
            events.append(("delay", delay))

        def record_reader_start() -> None:
            events.append(("reader", None))

        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread") as thread,
                patch.object(
                    openvmm_process.time,
                    "sleep",
                    side_effect=record_delay,
                ),
            ):
                interaction.return_value.process.poll.return_value = 0
                thread.return_value.start.side_effect = record_reader_start
                with openvmm_process.OpenvmmProcess(
                    ["openvmm"],
                    Path(temporary) / "output.log",
                    output_read_delay=2,
                ):
                    pass
        self.assertEqual(events, [("delay", 2), ("reader", None)])

    def test_negative_output_read_delay_does_not_start_process(self):
        with patch.object(openvmm_process, "InteractiveProcess") as interaction:
            with self.assertRaisesRegex(ValueError, "cannot be negative"):
                openvmm_process.OpenvmmProcess(
                    ["openvmm"], Path("unused.log"), output_read_delay=-1
                )
        interaction.assert_not_called()

    def test_time_abi_monitor_failure_does_not_start_process(self):
        # #720: the monitor is built before OpenVMM starts, so that its
        # failure cannot leave OpenVMM running outside the cleanup.
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "output.log"
            with (
                patch.object(
                    openvmm_process,
                    "TimeAbiMonitor",
                    side_effect=RuntimeError("synthetic monitor failure"),
                ),
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                self.assertRaisesRegex(RuntimeError, "synthetic monitor failure"),
            ):
                openvmm_process.OpenvmmProcess(["openvmm"], log_path)
            self.assertFalse(log_path.exists())
        interaction.assert_not_called()

    def test_snapshot_restore_runs_the_time_abi_steps_through_nvx_time(self):
        snapshot = (
            Path(__file__).parents[1] / "guest" / "common" / "nvx-snapshot"
        ).read_text(encoding="utf-8")

        self.assertIn(
            "generation_id=$(/sbin/nvx-time generation-id)",
            snapshot,
        )
        self.assertIn(
            '/sbin/nvx-time capture "$capture_request" "$generation_id" "$entropy"',
            snapshot,
        )
        # An untiered restore with nothing to activate finishes inside the
        # capture helper (metadata 2), so no second process starts.
        self.assertIn(
            '[ "$snapshot_tier" = legacy ] && finish_option=--finish', snapshot
        )
        self.assertIn('${finish_option:+"$finish_option"}', snapshot)
        self.assertIn("2) stall_detectors_suppressed=false ;;", snapshot)
        self.assertIn(
            'generation_id=$(/sbin/nvx-reseed "$entropy" "$generation_id")',
            snapshot,
        )
        self.assertIn(
            '/sbin/nvx-reseed --generation-only "$entropy" "$generation_id"',
            snapshot,
        )
        self.assertIn('export NVX_VM_GENERATION_ID="$generation_id"', snapshot)
        self.assertNotIn("/dev/port", snapshot)
        self.assertIn('[ "$range_count" -eq 0 ]', snapshot)
        self.assertIn("PACKET_ACK_REQUIRED=4", snapshot)
        self.assertIn(
            '/sbin/nvx-time restore-finish --ack --new-cpus "$restore_new_cpus"',
            snapshot,
        )
        self.assertIn('console_status "NVX-SNAPSHOT-ERROR: $*"', snapshot)
        for stage in ("packet", "entropy", "identity", "runtime-hook", "acknowledge"):
            self.assertIn(
                f'console_status "NVX-POST-RESTORE-STAGE: {stage}"',
                snapshot,
            )
        self.assertIn(
            '"NVX-MEMORY-ONLINE-OK: added_bytes=0 '
            'memtotal_kib=$memtotal_kib elapsed_us=0"',
            snapshot,
        )
        # Guest preparation steps 1 and 2 (doc/design/5.3-snapshot-protocol.md)
        # precede the barriers and the request.
        pre_capture = snapshot.index("/sbin/nvx-time pre-capture || exit 1")
        freeze = snapshot.index('echo 1 >"$container_cgroup/cgroup.freeze"')
        capture = snapshot.index("/sbin/nvx-time capture")
        self.assertLess(pre_capture, freeze)
        self.assertLess(freeze, capture)
        # A rejected capture thaws the barriers before it restores the stall
        # detectors.
        cleanup_start = snapshot.index("cleanup() {")
        cleanup = snapshot[cleanup_start : snapshot.index("\n}\n", cleanup_start)]
        self.assertLess(
            cleanup.index("cgroup.freeze"), cleanup.index("nvx-time cancel-capture")
        )

    def test_snapshot_console_diagnostics_are_nonfatal_and_ordered(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        snapshot = (
            Path(__file__).parents[1] / "guest" / "common" / "nvx-snapshot"
        ).read_text(encoding="utf-8")

        markers = [
            'console_status "NVX-POST-RESTORE-STAGE: packet"',
            'console_status "NVX-POST-RESTORE-STAGE: entropy"',
            'console_status "NVX-POST-RESTORE-STAGE: identity"',
            'console_status "NVX-POST-RESTORE-STAGE: runtime-hook"',
            'console_status "NVX-POST-RESTORE-STAGE: acknowledge"',
        ]
        positions = [snapshot.index(marker) for marker in markers]
        self.assertEqual(positions, sorted(positions))

        functions_start = snapshot.index("console_status() {")
        functions_end = snapshot.index("\n}\n\ncleanup()", functions_start) + 3
        functions = snapshot[functions_start:functions_end]
        functions = functions.replace(">/dev/console", '>"$console_target"')
        functions = functions.replace("/sbin/nvx-exit", "nvx_exit")

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            console_target = root / "console-directory"
            console_target.mkdir()
            exit_record = root / "exit-record"
            result = subprocess.run(
                [shell, "-s", "--", str(console_target), str(exit_record)],
                input=(
                    "set -eu\n"
                    "console_target=$1\n"
                    "exit_record=$2\n"
                    "post_restore_pending=false\n"
                    'nvx_exit() { printf "%s\\n" "$1" >"$exit_record"; }\n'
                    f"{functions}\n"
                    'console_status "unavailable console is non-fatal"\n'
                    'fail_closed "synthetic restore failure"\n'
                ),
                text=True,
                capture_output=True,
                timeout=5,
                check=False,
            )

            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertEqual(exit_record.read_text(encoding="ascii"), "1\n")
            self.assertEqual(
                result.stderr,
                "nvx-snapshot: synthetic restore failure; terminating the VM\n",
            )

    def test_plain_untiered_restore_prints_nothing(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        snapshot = (
            Path(__file__).parents[1] / "guest" / "common" / "nvx-snapshot"
        ).read_text(encoding="utf-8")
        helpers_start = snapshot.index("console_status() {")
        helpers_end = snapshot.index("\n}\n\ncleanup()", helpers_start) + 3
        restore_start = snapshot.index("post_restore() {")
        restore_end = snapshot.index("\n}\n", snapshot.index("finish_restore() {")) + 3
        functions = (
            snapshot[helpers_start:helpers_end] + snapshot[restore_start:restore_end]
        )
        functions = functions.replace(">/dev/console", '>>"$console_log"')
        functions = functions.replace("/sbin/nvx-exit", "nvx_exit")
        functions = functions.replace("/sbin/nvx-time", "nvx_time")
        generation_id = "0123456789abcdef0123456789abcdef"

        def restore(flags: int) -> tuple[str, str, str]:
            with tempfile.TemporaryDirectory() as temporary:
                result = subprocess.run(
                    [shell, "-s", "--", temporary],
                    input=(
                        "set -eu\n"
                        'console_log="$1/console"\n'
                        'calls="$1/nvx-time"\n'
                        ': >"$console_log"\n'
                        ': >"$calls"\n'
                        "snapshot_tier=legacy\n"
                        "PACKET_MEMORY_TARGET=2\n"
                        "PACKET_ACK_REQUIRED=4\n"
                        "restore_new_cpus=none\n"
                        "post_restore_pending=false\n"
                        "nvx_exit() { :; }\n"
                        'nvx_time() { printf "%s\\n" "$*" >>"$calls"; }\n'
                        'activate_restore_memory() { echo "memory $*"; }\n'
                        f"{functions}\n"
                        f"post_restore {flags} 0 0 {generation_id}\n"
                        'echo "pending=$post_restore_pending"\n'
                    ),
                    text=True,
                    capture_output=True,
                    timeout=5,
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                root = Path(temporary)
                return (
                    (root / "console").read_text(encoding="ascii"),
                    result.stdout,
                    (root / "nvx-time").read_text(encoding="ascii"),
                )

        # No processors or memory to add: no console bytes on the restore path.
        self.assertEqual(
            restore(0), ("", "pending=false\n", "restore-finish --new-cpus none\n")
        )
        self.assertEqual(
            restore(4),
            ("", "pending=false\n", "restore-finish --ack --new-cpus none\n"),
        )
        # A memory target keeps its stage and completion lines.
        console, stdout, calls = restore(2)
        self.assertEqual(
            console,
            "NVX-POST-RESTORE-STAGE: packet\nNVX-POST-RESTORE-STAGE: acknowledge\n",
        )
        self.assertEqual(
            stdout, "memory 0\nNVX-POST-RESTORE-OK: tier=legacy\npending=false\n"
        )
        self.assertEqual(calls, "restore-finish --new-cpus none\n")

    def test_capture_metadata_2_leaves_no_restore_work_to_the_shell(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        snapshot = (
            Path(__file__).parents[1] / "guest" / "common" / "nvx-snapshot"
        ).read_text(encoding="utf-8")
        start = snapshot.index('case "${1:-}" in\n    0) ;;')
        dispatch = snapshot[start : snapshot.index("\nesac\n", start) + 6]
        generation_id = "0123456789abcdef0123456789abcdef"

        def dispatch_metadata(metadata: str) -> tuple[int, str]:
            result = subprocess.run(
                [shell, "-s"],
                input=(
                    "set -eu\n"
                    "stall_detectors_suppressed=true\n"
                    'post_restore() { echo "post_restore $*"; }\n'
                    'fail_closed() { echo "fail_closed $*"; exit 1; }\n'
                    f"set -- {metadata}\n"
                    f"{dispatch}"
                    'echo "suppressed=$stall_detectors_suppressed"\n'
                ),
                text=True,
                capture_output=True,
                timeout=5,
                check=False,
            )
            return result.returncode, result.stdout

        # No restore: the cleanup trap restores the stall detectors.
        self.assertEqual(dispatch_metadata("0"), (0, "suppressed=true\n"))
        # nvx-time finished the restore and handed it to the daemon.
        self.assertEqual(
            dispatch_metadata(f"2 4 0 0 {generation_id}"),
            (0, "suppressed=false\n"),
        )
        self.assertEqual(
            dispatch_metadata(f"1 4 2 0 {generation_id}"),
            (0, f"post_restore 4 2 0 {generation_id}\nsuppressed=false\n"),
        )
        self.assertEqual(
            dispatch_metadata("3"),
            (1, "fail_closed snapshot capture metadata is invalid\n"),
        )

    def _run_host_share_write_back(
        self,
        mountinfo: str,
        stats: Sequence[str | None],
        *,
        failing_sync: int | None = None,
        pass_limit: int | None = None,
        prelude: str = "",
        script: str = "write_back_host_shares\n",
    ) -> tuple[int, str, list[str]]:
        """Run nvx-snapshot's write-back of the host shares that `mountinfo`
        mounts, and return the shell's status, its stderr, and the calls of
        the stand-ins of debugfs and sync(1), which record them in order.

        Pass N reads `stats[N - 1]`, and cannot read a missing or None entry.
        The `failing_sync`th sync fails, and `pass_limit` replaces the limit of
        1,024 passes. `prelude` runs before the functions are defined, and
        `script` after the stand-ins are."""
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        snapshot = (
            Path(__file__).parents[1] / "guest" / "common" / "nvx-snapshot"
        ).read_text(encoding="utf-8")
        start = snapshot.index("list_host_shares() {")
        end = snapshot.index("\n}\n", snapshot.index("write_back_host_shares() {")) + 3
        functions = snapshot[start:end].replace(
            "/proc/self/mountinfo", '"$root/mountinfo"'
        )
        if pass_limit is not None:
            limit = '[ "$passes" -ge 1024 ]'
            self.assertIn(limit, functions)
            functions = functions.replace(limit, f'[ "$passes" -ge {pass_limit} ]')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "mountinfo").write_text(mountinfo, encoding="ascii", newline="\n")
            for index, text in enumerate(stats, 1):
                if text is not None:
                    (root / f"stats-{index}").write_text(
                        text, encoding="ascii", newline="\n"
                    )
            if failing_sync is not None:
                (root / f"sync-fail-{failing_sync}").touch()
            result = subprocess.run(
                [shell, "-s", "--", temporary],
                input=(
                    "set -eu\n"
                    'root="$1"\n'
                    'echo 0 >"$root/passes"\n'
                    'echo 0 >"$root/syncs"\n'
                    ': >"$root/calls"\n'
                    f"{prelude}"
                    f"{functions}"
                    "read_writeback_stats() {\n"
                    '    pass=$(($(cat "$root/passes") + 1))\n'
                    '    echo "$pass" >"$root/passes"\n'
                    '    printf \'%s\\n\' "stats $*" >>"$root/calls"\n'
                    '    cat "$root/stats-$pass" 2>/dev/null\n'
                    "}\n"
                    "sync() {\n"
                    '    count=$(($(cat "$root/syncs") + 1))\n'
                    '    echo "$count" >"$root/syncs"\n'
                    '    printf \'%s\\n\' "sync $*" >>"$root/calls"\n'
                    '    [ ! -e "$root/sync-fail-$count" ]\n'
                    "}\n"
                    f"{script}"
                ),
                text=True,
                capture_output=True,
                timeout=30,
                check=False,
            )
            calls = (root / "calls").read_text(encoding="ascii").splitlines()
        return result.returncode, result.stderr, calls

    def test_host_share_write_back_repeats_until_no_inode_is_dirty(self):
        status, stderr, calls = self._run_host_share_write_back(
            _ONE_SHARE_MOUNTINFO,
            [_bdi_stats(2, io=1), _bdi_stats(0, more_io=1), _bdi_stats(0)],
        )
        self.assertEqual((status, stderr), (0, ""))
        # Each pass syncs the share through its whole mount, and so does the
        # pass that counts no dirty inode, whose sync waits for the writes
        # still in flight.
        self.assertEqual(calls, [f"stats {_ONE_SHARE_STATS}", "sync -f /mnt/share"] * 3)

    def test_host_share_write_back_skips_a_guest_without_host_shares(self):
        status, stderr, calls = self._run_host_share_write_back(
            "22 1 0:20 / /proc rw - proc proc rw\n"
            "60 25 8:1 / /data rw - ext4 /dev/vda rw\n",
            [],
        )
        self.assertEqual((status, stderr, calls), (0, "", []))

    def test_host_share_write_back_names_one_mount_of_each_share(self):
        status, stderr, calls = self._run_host_share_write_back(
            # A share mounted whole, and one mounted only below its root, whose
            # mount points mountinfo escapes and one of which holds a newline.
            "31 25 0:42 / /mnt/share rw - virtiofs microvm rw\n"
            "40 25 0:50 /child /bound\\012line rw - virtiofs microvm rw\n"
            "41 25 0:50 /child /srv/my\\040data\\134x rw - virtiofs microvm rw\n"
            "42 25 0:42 /0 /mnt/share/again rw - virtiofs microvm rw\n",
            [_bdi_stats(1) + _bdi_stats(0), _bdi_stats(0) + _bdi_stats(0)],
        )
        self.assertEqual((status, stderr), (0, ""))
        stats = "/sys/kernel/debug/bdi/0:42/stats /sys/kernel/debug/bdi/0:50/stats"
        self.assertEqual(
            calls,
            [f"stats {stats}", "sync -f /mnt/share", "sync -f /srv/my data\\x"] * 2,
        )

        status, stderr, calls = self._run_host_share_write_back(
            "50 25 0:60 /x /only\\012newline rw - virtiofs microvm rw\n",
            [_bdi_stats(0)],
        )
        self.assertEqual(
            (status, stderr, calls),
            (1, "nvx-snapshot: no mount point of host share 0:60 can be named\n", []),
        )

    def test_host_share_write_back_continues_while_passes_make_progress(self):
        counts = [5, 3, 3, 2, 0]
        status, stderr, calls = self._run_host_share_write_back(
            _ONE_SHARE_MOUNTINFO, [_bdi_stats(count) for count in counts]
        )
        self.assertEqual((status, stderr), (0, ""))
        self.assertEqual(
            calls, [f"stats {_ONE_SHARE_STATS}", "sync -f /mnt/share"] * len(counts)
        )

    def test_host_share_write_back_fails_when_passes_stall(self):
        # Three passes in a row that count no fewer dirty inodes than the pass
        # before each fail, however many passes made progress before them.
        for counts in ([4, 4, 4, 4], [1, 2, 3, 4], [2, 1, 1, 1, 1]):
            status, stderr, calls = self._run_host_share_write_back(
                _ONE_SHARE_MOUNTINFO,
                [_bdi_stats(count) for count in counts] + [_bdi_stats(0)],
            )
            self.assertEqual(
                (status, stderr),
                (
                    1,
                    "nvx-snapshot: the host shares kept dirty pages through "
                    f"{len(counts)} write-back passes\n",
                ),
                counts,
            )
            self.assertEqual(
                calls,
                [f"stats {_ONE_SHARE_STATS}", "sync -f /mnt/share"] * len(counts),
                counts,
            )

    def test_host_share_write_back_bounds_its_passes(self):
        # Counts that fall in every other pass never stall three times in a
        # row, so only the limit of passes ends them.
        status, stderr, calls = self._run_host_share_write_back(
            _ONE_SHARE_MOUNTINFO,
            [_bdi_stats(count) for count in [2, 1] * 4],
            pass_limit=6,
        )
        self.assertEqual(
            (status, stderr),
            (
                1,
                "nvx-snapshot: the host shares kept dirty pages through 6 "
                "write-back passes\n",
            ),
        )
        self.assertEqual(calls.count("sync -f /mnt/share"), 6)

    def test_host_share_write_back_refuses_unreadable_or_malformed_lists(self):
        for stats in (
            None,
            "b_dirty: 1\nb_io: 0\n",
            "b_dirty: 1\nb_io: x\nb_more_io: 0\n",
            "b_dirty: -1\nb_io: 0\nb_more_io: 0\n",
            "b_dirty:\nb_io: 0\nb_more_io: 0\n",
            "b_dirty: 1\nb_io: 0\nb_more_io: 0\nb_io: 0\n",
        ):
            status, stderr, calls = self._run_host_share_write_back(
                _ONE_SHARE_MOUNTINFO, [stats, _bdi_stats(0)]
            )
            self.assertEqual(
                (status, stderr, calls),
                (
                    1,
                    "nvx-snapshot: cannot read the writeback lists of the host "
                    "shares\n",
                    [f"stats {_ONE_SHARE_STATS}"],
                ),
                stats,
            )

    def test_host_share_write_back_refuses_a_failed_sync(self):
        # The sync of the pass that counts no dirty inode waits for the
        # writes in flight, so its failure refuses the capture too.
        for failing_sync in (1, 2):
            status, stderr, calls = self._run_host_share_write_back(
                _ONE_SHARE_MOUNTINFO,
                [_bdi_stats(1), _bdi_stats(0)],
                failing_sync=failing_sync,
            )
            self.assertEqual(
                (status, stderr),
                (
                    1,
                    "nvx-snapshot: failed to write back the host share at /mnt/share\n",
                ),
            )
            self.assertEqual(
                calls,
                [f"stats {_ONE_SHARE_STATS}", "sync -f /mnt/share"] * failing_sync,
            )

    def test_failed_host_share_write_back_thaws_and_requests_no_capture(self):
        snapshot = (
            Path(__file__).parents[1] / "guest" / "common" / "nvx-snapshot"
        ).read_text(encoding="utf-8")
        # Every capture writes the shares back after the cleanup trap and the
        # paired barriers are in place, and before the request.
        write_back = snapshot.index("write_back_host_shares || exit 1")
        self.assertLess(snapshot.index("trap cleanup EXIT"), write_back)
        self.assertLess(snapshot.index('fsfreeze -f "$scratch_mount"'), write_back)
        self.assertLess(write_back, snapshot.index("/sbin/nvx-time capture"))

        helpers_start = snapshot.index("console_status() {")
        cleanup_end = snapshot.index("\n}\n", snapshot.index("cleanup() {")) + 3
        helpers = (
            snapshot[helpers_start:cleanup_end]
            .replace(">/dev/console", '>>"$root/console"')
            .replace("/sbin/nvx-exit", "nvx_exit")
            .replace("/sbin/nvx-time", "nvx_time")
        )
        status, stderr, calls = self._run_host_share_write_back(
            _ONE_SHARE_MOUNTINFO,
            [None],
            prelude=(
                'container_cgroup="$root/cgroup"\n'
                'scratch_mount="$root/scratch"\n'
                'mkdir "$container_cgroup"\n'
                'echo 1 >"$container_cgroup/cgroup.freeze"\n'
                "container_frozen=true\n"
                "scratch_frozen=true\n"
                "stall_detectors_suppressed=true\n"
                "post_restore_pending=false\n"
                "fsfreeze() {\n"
                '    printf \'%s\\n\' "fsfreeze $1" >>"$root/calls"\n'
                "}\n"
                "nvx_time() {\n"
                '    freeze=$(cat "$container_cgroup/cgroup.freeze")\n'
                '    printf \'%s\\n\' "nvx-time $* freeze=$freeze" >>"$root/calls"\n'
                "}\n"
                "nvx_exit() {\n"
                '    printf \'%s\\n\' "nvx-exit $*" >>"$root/calls"\n'
                "}\n"
                f"{helpers}"
            ),
            script=(
                "trap cleanup EXIT\n"
                "write_back_host_shares || exit 1\n"
                "nvx_time capture\n"
            ),
        )
        # The guest requests no capture, thaws the scratch filesystem and then
        # the workload, and only then restores the stall detectors.
        self.assertEqual(
            (status, stderr),
            (1, "nvx-snapshot: cannot read the writeback lists of the host shares\n"),
        )
        self.assertEqual(
            calls,
            [
                f"stats {_ONE_SHARE_STATS}",
                "fsfreeze -u",
                "nvx-time cancel-capture freeze=0",
            ],
        )

    def test_console_log_persists_buffered_and_completed_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            connection, peer = socket.socketpair()
            console = openvmm_process.TcpConsole(connection)
            peer.sendall(b"failure diagnostic\n")
            peer.close()

            failure_log = root / "failure.log"
            output = microvm_tests._persist_console_log(console, b"", failure_log)
            self.assertEqual(output, b"failure diagnostic\n")
            self.assertEqual(failure_log.read_bytes(), output)

            success_log = root / "success.log"
            output = microvm_tests._persist_console_log(
                None,
                b"completed output\n",
                success_log,
            )
            self.assertEqual(output, b"completed output\n")
            self.assertEqual(success_log.read_bytes(), output)

            connection_failure_log = root / "connection-failure.log"
            output = microvm_tests._persist_console_log(
                None,
                b"",
                connection_failure_log,
            )
            self.assertEqual(output, b"")
            self.assertEqual(connection_failure_log.read_bytes(), b"")

    def test_process_wait_reads_final_chunks_after_process_exit(self):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "output.log"
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread"),
                patch.object(openvmm_process.queue, "Queue") as queues,
            ):
                interaction.return_value.process.poll.return_value = 0
                interaction.return_value.process.wait.return_value = 0
                queues.return_value.get.side_effect = [
                    b"BEGIN-",
                    queue.Empty,
                    b"END\n",
                    None,
                ]
                queues.return_value.get_nowait.side_effect = queue.Empty
                with openvmm_process.OpenvmmProcess(["openvmm"], log_path) as process:
                    result = process.wait(1)
                self.assertEqual(result.returncode, 0)
                self.assertEqual(result.output, b"BEGIN-END\n")
                self.assertEqual(log_path.read_bytes(), result.output)
                self.assertEqual(queues.return_value.get.call_count, 4)

    def test_process_transcript_grows_while_the_process_runs(self):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "output.log"
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread"),
                patch.object(openvmm_process.queue, "Queue") as queues,
            ):
                interaction.return_value.process.poll.return_value = None
                interaction.return_value.process.wait.return_value = 0
                queues.return_value.get.side_effect = [b"BOOT\n", None]
                queues.return_value.get_nowait.side_effect = queue.Empty
                with openvmm_process.OpenvmmProcess(["openvmm"], log_path) as process:
                    process.wait_for(b"BOOT", 1)
                    # Consumed bytes are on disk while OpenVMM is still running.
                    self.assertEqual(log_path.read_bytes(), b"BOOT\n")
                    process.wait(1)
                self.assertEqual(log_path.read_bytes(), b"BOOT\n")

    def test_process_transcript_open_failure_surfaces_before_start(self):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "output.log"
            log_path.mkdir()
            with patch.object(openvmm_process, "InteractiveProcess") as interaction:
                with self.assertRaises(OSError):
                    openvmm_process.OpenvmmProcess(["openvmm"], log_path)
            interaction.assert_not_called()

    def test_process_close_writes_output_delivered_after_the_last_wait(self):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "output.log"
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread"),
                patch.object(openvmm_process.queue, "Queue") as queues,
            ):
                interaction.return_value.process.poll.return_value = None
                queues.return_value.get_nowait.side_effect = [b"LATE\n", queue.Empty]
                process = openvmm_process.OpenvmmProcess(["openvmm"], log_path)
                process.close()
            self.assertEqual(log_path.read_bytes(), b"LATE\n")

    def test_process_close_keeps_output_queued_during_interaction_close(self):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "output.log"
            closed: list[bool] = []
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread"),
                patch.object(openvmm_process.queue, "Queue") as queues,
            ):
                interaction.return_value.process.poll.return_value = None
                interaction.return_value.close.side_effect = lambda: closed.append(True)

                pending = [b"LAST\n"]

                def get_nowait() -> bytes:
                    # The reader queues its last chunk only while the interaction shuts down.
                    if closed and pending:
                        return pending.pop()
                    raise queue.Empty

                queues.return_value.get_nowait.side_effect = get_nowait
                process = openvmm_process.OpenvmmProcess(["openvmm"], log_path)
                process.close()
                self.assertEqual(process.output, b"LAST\n")
            self.assertEqual(log_path.read_bytes(), b"LAST\n")

    def test_process_close_keeps_queued_output_when_interaction_close_fails(self):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "output.log"
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread"),
                patch.object(openvmm_process.queue, "Queue") as queues,
            ):
                interaction.return_value.process.poll.return_value = None
                interaction.return_value.close.side_effect = OSError("reader stuck")
                queues.return_value.get_nowait.side_effect = [b"QUEUED\n", queue.Empty]
                process = openvmm_process.OpenvmmProcess(["openvmm"], log_path)
                with self.assertRaises(OSError):
                    process.close()
                self.assertEqual(process.output, b"QUEUED\n")
            self.assertEqual(log_path.read_bytes(), b"QUEUED\n")

    def test_process_close_releases_resources_when_the_transcript_write_fails(self):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "output.log"
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread"),
                patch.object(openvmm_process.queue, "Queue") as queues,
            ):
                interaction.return_value.process.poll.return_value = None
                queues.return_value.get_nowait.side_effect = [b"LATE\n", queue.Empty]
                process = openvmm_process.OpenvmmProcess(["openvmm"], log_path)
                failing = MagicMock()
                failing.closed = False
                failing.write.side_effect = OSError("no space left on device")
                process._log.close()
                process._log = failing
                with self.assertRaises(OSError):
                    process.close()
            interaction.return_value.close.assert_called_once_with()
            failing.close.assert_called_once_with()

    def test_process_close_keeps_every_queued_chunk_when_the_transcript_write_fails(
        self,
    ):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "output.log"
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread"),
                patch.object(openvmm_process.queue, "Queue") as queues,
            ):
                interaction.return_value.process.poll.return_value = None
                queues.return_value.get_nowait.side_effect = [
                    b"FIRST\n",
                    b"SECOND\n",
                    b"THIRD\n",
                    queue.Empty,
                ]
                process = openvmm_process.OpenvmmProcess(["openvmm"], log_path)
                failing = MagicMock()
                failing.closed = False
                failing.write.side_effect = OSError("no space left on device")
                process._log.close()
                process._log = failing
                with self.assertRaises(OSError):
                    process.close()
                self.assertEqual(bytes(process._output), b"FIRST\nSECOND\nTHIRD\n")
            interaction.return_value.close.assert_called_once_with()
            failing.close.assert_called_once_with()

    def test_process_wait_clamps_elapsed_process_timeout(self):
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread"),
                patch.object(openvmm_process.queue, "Queue") as queues,
                patch.object(
                    openvmm_process.time,
                    "monotonic",
                    side_effect=[0.0, 0.25, 2.0],
                ),
            ):
                interaction.return_value.process.poll.return_value = 0
                interaction.return_value.process.wait.return_value = 0
                queues.return_value.get.return_value = None
                queues.return_value.get_nowait.side_effect = queue.Empty
                with openvmm_process.OpenvmmProcess(
                    ["openvmm"], Path(temporary) / "output.log"
                ) as process:
                    process.wait(1.0)
                interaction.return_value.process.wait.assert_called_once_with(
                    timeout=0.0
                )

    def test_process_wait_for_accepts_marker_after_process_exit(self):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "output.log"
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread"),
                patch.object(openvmm_process.queue, "Queue") as queues,
            ):
                interaction.return_value.process.poll.return_value = 0
                queues.return_value.get.side_effect = [
                    b"MAR",
                    queue.Empty,
                    b"KER\n",
                ]
                queues.return_value.get_nowait.side_effect = queue.Empty
                with openvmm_process.OpenvmmProcess(["openvmm"], log_path) as process:
                    process.wait_for(b"MARKER", 1)
                self.assertEqual(log_path.read_bytes(), b"MARKER\n")

    def test_process_wait_for_line_ignores_marker_inside_echoed_script(self):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "output.log"
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread"),
                patch.object(openvmm_process.queue, "Queue") as queues,
            ):
                interaction.return_value.process.poll.return_value = None
                queues.return_value.get.side_effect = [
                    b"echo NVX-READY\n",
                    b"NVX-READY\n",
                ]
                queues.return_value.get_nowait.side_effect = queue.Empty
                with openvmm_process.OpenvmmProcess(["openvmm"], log_path) as process:
                    process.wait_for_line(b"NVX-READY", 1)
                self.assertEqual(
                    log_path.read_bytes(),
                    b"echo NVX-READY\nNVX-READY\n",
                )

    def test_process_wait_bounds_missing_output_eof_after_exit(self):
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(openvmm_process, "InteractiveProcess") as interaction,
                patch.object(openvmm_process.threading, "Thread"),
                patch.object(openvmm_process.queue, "Queue") as queues,
                patch.object(
                    openvmm_process.time,
                    "monotonic",
                    side_effect=[0.0, 0.0, 1.0],
                ),
            ):
                interaction.return_value.process.poll.return_value = 0
                interaction.return_value.process.wait.return_value = 0
                queues.return_value.get.side_effect = queue.Empty
                queues.return_value.get_nowait.side_effect = queue.Empty
                with openvmm_process.OpenvmmProcess(
                    ["openvmm"], Path(temporary) / "output.log"
                ) as process:
                    with self.assertRaisesRegex(TimeoutError, "did not reach EOF"):
                        process.wait(0.5)

    def test_openvmm_process_preserves_buffered_sequential_markers(self):
        class FakeProcess:
            pid = 123
            returncode = 0

            def poll(self):
                return 0

            def wait(self, timeout: float | None = None):
                del timeout
                return 0

        class FakeInteraction:
            def __init__(self):
                self.process = FakeProcess()

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(b"FIRST\nSECOND\n")
                chunks.put(None)

            def write_input(self, _data: bytes) -> None:
                pass

            def close(self) -> None:
                pass

        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "process.log"
            with patch.object(
                openvmm_process,
                "InteractiveProcess",
                return_value=FakeInteraction(),
            ):
                with openvmm_process.OpenvmmProcess(["openvmm"], log_path) as process:
                    process.wait_for(b"FIRST", 1)
                    process.wait_for(b"SECOND", 1)
                    result = process.wait(1)

            self.assertEqual(result.returncode, 0)
            self.assertEqual(log_path.read_bytes(), b"FIRST\nSECOND\n")

    def test_tcp_console_line_marker_ignores_echoed_command(self):
        connection, peer = socket.socketpair()
        console = openvmm_process.TcpConsole(connection)
        marker = b"NVX-CONSOLE-RX-READY"
        output = b"> echo " + marker + b"\r\n" + marker + b"\r\n"
        peer.sendall(output)

        console.wait_for_line(marker, 1.0)

        self.assertEqual(console.output, output)
        console.close()
        peer.close()

    def test_tcp_console_connect_closes_failed_connection(self):
        failed = MagicMock()
        failed.setsockopt.side_effect = OSError("configuration failed")
        connected = MagicMock()
        with patch.object(
            openvmm_process.socket,
            "create_connection",
            side_effect=(failed, connected),
        ):
            console = openvmm_process.TcpConsole.connect(("127.0.0.1", 1), 1.0)

        failed.close.assert_called_once_with()
        console.close()
        connected.close.assert_called_once_with()

    def test_every_openvmm_process_success_path_waits(self):
        # close() never raises, so a late violation or a 193-195 power-off is
        # only caught by wait(), which scans the output to EOF and checks the
        # exit status. Every scenario's OpenVMM context must therefore reach
        # wait() on its success path: unconditionally, with no early exit.
        source = Path(microvm_tests.__file__).read_text(encoding="utf-8")
        blocks = 0
        for node in ast.walk(ast.parse(source)):
            if not isinstance(node, ast.With):
                continue
            for item in node.items:
                call = item.context_expr
                if not (
                    isinstance(call, ast.Call)
                    and ast.unparse(call.func) == "OpenvmmProcess"
                ):
                    continue
                blocks += 1
                assert isinstance(item.optional_vars, ast.Name)
                name = item.optional_vars.id
                statements = list(node.body)
                # A try body whose only handler is finally is unconditional.
                while (
                    len(statements) == 1
                    and isinstance(statements[0], ast.Try)
                    and not statements[0].handlers
                ):
                    statements = list(statements[0].body)
                with self.subTest(line=node.lineno):
                    self.assertTrue(
                        any(
                            f"{name}.wait(" in ast.unparse(statement)
                            and not isinstance(
                                statement, (ast.If, ast.For, ast.While, ast.Try)
                            )
                            for statement in statements
                        )
                    )
                    self.assertFalse(
                        any(
                            isinstance(sub, (ast.Return, ast.Break, ast.Continue))
                            for sub in ast.walk(node)
                        )
                    )
        self.assertGreater(blocks, 30)

    def test_every_tcp_console_is_monitored_and_cold_boot_shells_are_queried(self):
        tree = ast.parse(Path(microvm_tests.__file__).read_text(encoding="utf-8"))
        connects = [
            {keyword.arg: ast.unparse(keyword.value) for keyword in node.keywords}
            for node in ast.walk(tree)
            if isinstance(node, ast.Call)
            and ast.unparse(node.func) == "TcpConsole.connect"
        ]
        # The console-snapshot capture and restore, and the snapshot-tier
        # capture, restore, and gate-timeout restore.
        self.assertEqual(len(connects), 5)
        for keywords in connects:
            self.assertRegex(
                keywords.get("monitor", ""), r"^TimeAbiMonitor\(\w*command\)$"
            )
        # The managed lifecycle's console is a file, which the same monitor
        # scans once the nvx-rs test that drives the guest has ended.
        managed = next(
            node
            for node in ast.walk(tree)
            if isinstance(node, ast.FunctionDef)
            and node.name == "run_managed_lifecycle"
        )
        self.assertIn(
            "TimeAbiMonitor(command)",
            [
                ast.unparse(node)
                for node in ast.walk(managed)
                if isinstance(node, ast.Call)
            ],
        )
        # Only the cold boots whose shell is on the virtio console ask for
        # nvx-time status. Restores never ask, and in the managed lifecycle
        # init starts the managed agent instead of a shell.
        self.assertEqual(
            [k["monitor"] for k in connects if k.get("time_abi_status") == "True"],
            ["TimeAbiMonitor(capture_command)"] * 2,
        )

    def test_snapshot_core_script_handles_no_clocksource(self):
        # The time ABI fixes the clocksource on every backend, so snapshot-core
        # neither selects nor waits for one.
        script = microvm_tests._read_script("snapshot-core.sh")

        for text in ("clocksource", "kvm-clock", "tsc-early", "@SELECT", "@VALIDATE"):
            self.assertNotIn(text, script)
        self.assertIn("/sbin/nvx-reseed", script)
        self.assertIn("/sbin/nvx-reseed --sample", script)
        self.assertIn("NVX-SNAPSHOT-GENERATION-ID-", script)
        self.assertIn("NVX-SNAPSHOT-UUID-", script)
        self.assertIn("NVX-SNAPSHOT-TEMP-ID-", script)

    def test_snapshot_core_checks_that_restored_processes_cannot_reach_the_ports(self):
        # nvx-time holds the port permission only for each port transaction, so
        # the processes that it starts on a restored VM cannot reach the ports.
        script = microvm_tests._read_script("snapshot-core.sh")

        self.assertIn(
            'port_access="$(/sbin/nvx-time test port-access)" || fail 55', script
        )
        self.assertIn('echo "NVX-SNAPSHOT-PORT-ACCESS-$port_access"', script)
        self.assertEqual(script.count("|| fail 55"), 1)

    def test_snapshot_core_reads_the_entropy_that_nvx_time_kept(self):
        # Restore packet v4 is consumed by nvx-time capture, which leaves the
        # entropy of an untiered restore in /run/nvx/restore-entropy.
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        script = microvm_tests._read_script("snapshot-core.sh")
        self.assertNotIn("OPENVMM_ENTROPY_V1", script)
        self.assertNotIn("/dev/port", script)
        section = (
            "generation_id_after="
            + script.split("generation_id_after=", 1)[1].split("rng=", 1)[0]
        )
        generation = bytes(range(16))
        cases = (
            (generation + bytes(48), "00" * 16, "reseeded", 0),
            (generation + bytes(47), "00" * 16, "FAIL 44", 44),
            (bytes(16) + bytes(48), "00" * 16, "FAIL 52", 52),
            (generation + bytes(48), generation.hex(), "FAIL 51", 51),
        )
        for entropy, before, expected, status in cases:
            with self.subTest(expected=expected):
                with tempfile.TemporaryDirectory() as temporary:
                    path = Path(temporary) / "restore-entropy"
                    path.write_bytes(entropy)
                    body = (
                        section.replace("/run/nvx/restore-entropy", path.as_posix())
                        .replace("/sbin/nvx-time", "nvx_time")
                        .replace("/sbin/nvx-reseed", "nvx_reseed")
                    )
                    result = subprocess.run(
                        [shell],
                        input=(
                            "set -eu\n"
                            'fail() { echo "FAIL $1"; exit "$1"; }\n'
                            f"nvx_time() {{ echo {generation.hex()}; }}\n"
                            'nvx_reseed() { echo "reseeded"; }\n'
                            f"generation_id_before={before}\n" + body
                        ),
                        text=True,
                        capture_output=True,
                        timeout=10,
                        check=False,
                    )
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertIn(expected, result.stdout)

    def test_smp_worker_requires_bounded_local_timer_progress(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        worker = (
            benchmark.smp_probe_script(2)
            .split("<<'NVX_SMP_WORKER'\n", 1)[1]
            .split("\nNVX_SMP_WORKER", 1)[0]
        )
        worker = worker.replace("/proc/interrupts", "interrupts").replace(
            "timer_attempts=10000", "timer_attempts=8"
        )
        for name, initial, advanced, advance_read, actual, status, reads in (
            ("frozen", "LOC: 100 100", "LOC: 100 100", 3, 1, 88, 9),
            ("other-cpu", "LOC: 100 100", "LOC: 101 100", 3, 1, 88, 9),
            ("delayed", "LOC: 100 100", "LOC: 100 101", 4, 1, 0, 4),
            ("last-attempt", "LOC: 100 100", "LOC: 100 101", 9, 1, 0, 9),
            ("too-late", "LOC: 100 100", "LOC: 100 101", 10, 1, 88, 9),
            ("backwards", "LOC: 100 100", "LOC: 100 99", 3, 1, 88, 9),
            ("missing", "RES: 1 1", "RES: 1 1", 0, 1, 87, 2),
            ("wrong-cpu", "LOC: 100 100", "LOC: 100 101", 3, 0, 87, 0),
        ):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / "interrupts").write_text(initial + "\n", encoding="ascii")
                result = subprocess.run(
                    [shell, "-s", "--", "1", "result", "1"],
                    cwd=root,
                    input=(
                        "loc_reads=0\n"
                        "trap 'echo NVX-LAPIC-READS-$loc_reads' EXIT\n"
                        f"awk() {{ echo {actual}; }}\n"
                        "read() {\n"
                        "    loc_reads=$((loc_reads + 1))\n"
                        f'    if [ "$loc_reads" -eq {advance_read} ]; then\n'
                        f"        printf '%s\\n' '{advanced}' >interrupts\n"
                        "    fi\n"
                        '    command read "$@"\n'
                        "}\n" + worker + "\n"
                    ),
                    text=True,
                    capture_output=True,
                    timeout=5,
                    check=False,
                )

                self.assertEqual(
                    result.returncode, status, result.stdout + result.stderr
                )
                self.assertIn(f"NVX-LAPIC-READS-{reads}\n", result.stdout)
                if status:
                    self.assertFalse((root / "result").exists())
                    self.assertIn(
                        "SMP-WORKER-FAIL" if name == "wrong-cpu" else "SMP-LAPIC-FAIL",
                        result.stdout,
                    )
                else:
                    self.assertEqual(
                        (root / "result").read_text(encoding="ascii").split(),
                        ["1", "101", "1"],
                    )

    def test_snapshot_core_waits_for_no_destination_marker_line_before_exit(self):
        events: list[tuple[str, bytes | str | None]] = []
        marker = b"NVX-SNAPSHOT-NO-DESTINATION-OK"
        command = ["openvmm", "--hypervisor", "whp", "--kernel", "vmlinux"]

        class StopAfterNoDestination(Exception):
            pass

        class FakeProcess:
            def __init__(self) -> None:
                self.console = bytearray(b"NVX-SNAPSHOT-NO-DESTINATION-OK\r\n")

            def __enter__(self):
                return self

            def __exit__(
                self,
                _exception_type: type[BaseException] | None,
                _exception: BaseException | None,
                _traceback: object | None,
            ) -> None:
                return None

            @property
            def output(self) -> bytes:
                return bytes(self.console)

            def wait_for(self, expected: bytes, _timeout: float) -> None:
                events.append(("wait_for", expected))

            def wait_for_line(self, expected: bytes, _timeout: float) -> None:
                events.append(("wait_for_line", expected))

            def send_line(self, line: str) -> None:
                events.append(("send_line", line))

            def send_bytes(self, data: bytes) -> None:
                events.append(("send_bytes", data.decode()))
                self.console.extend(_canceled_capture_output("whp"))

            def wait(self, _timeout: float) -> openvmm_process.OpenvmmProcessResult:
                events.append(("wait", None))
                return openvmm_process.OpenvmmProcessResult(0, marker + b"\n")

        with (
            patch.object(microvm_tests, "workload_boot_command", return_value=command),
            patch.object(microvm_tests, "OpenvmmProcess", return_value=FakeProcess()),
            patch.object(
                microvm_tests.tempfile,
                "TemporaryDirectory",
                side_effect=StopAfterNoDestination,
            ),
            self.assertRaises(StopAfterNoDestination),
        ):
            microvm_tests.run_snapshot_core(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initramfs"),
                "whp",
                memory_mib=128,
                timeout=1,
                output_dir=Path("logs"),
            )

        self.assertEqual(
            events,
            [
                ("wait_for", microvm_tests.BOOT_MARKER),
                ("send_line", "nvx-snapshot; echo NVX-SNAPSHOT-NO-DESTINATION-OK"),
                ("wait_for_line", marker),
                ("send_bytes", microvm_tests.canceled_capture_checks()),
                ("wait_for_line", microvm_tests.CANCELED_CAPTURE_DONE_MARKER),
                ("send_line", "nvx-exit 0"),
                ("wait", None),
            ],
        )

    def test_canceled_capture_checks_query_status_and_rcu_suppression(self):
        script = microvm_tests.canceled_capture_checks()
        query = time_abi.status_script().removesuffix("\n") + "; "
        self.assertTrue(script.startswith(query))
        # One console line, which the shell reads whole before it runs any of
        # it, so that the terminal echoes none of it into the status lines.
        self.assertEqual(script.count("\n"), 1)
        self.assertTrue(script.endswith("\n"))
        self.assertIn(
            "$(cat /sys/module/rcupdate/parameters/rcu_cpu_stall_suppress)", script
        )
        # The command never contains a whole marker, so the console's echo of
        # it, wrapped or not, can't stand in for the guest's answers.
        for marker in (
            microvm_tests.CANCELED_CAPTURE_RCU_PREFIX,
            microvm_tests.CANCELED_CAPTURE_DONE_MARKER,
        ):
            self.assertNotIn(marker.decode().strip(), script)
        # The shell prints both markers whole.
        shell = shutil.which("sh")
        if shell is None:
            self.skipTest("no POSIX shell")
        answers = subprocess.run(
            [shell, "-c", script.removeprefix(query)],
            capture_output=True,
            check=True,
        ).stdout
        lines = answers.splitlines()
        self.assertEqual(lines[-1], microvm_tests.CANCELED_CAPTURE_DONE_MARKER)
        self.assertTrue(lines[0].startswith(microvm_tests.CANCELED_CAPTURE_RCU_PREFIX))

    def test_canceled_capture_requires_generation_zero_and_no_suppression(self):
        command = ["openvmm", "--hypervisor", "kvm", "--kernel", "vmlinux"]
        checks = microvm_tests.canceled_capture_checks().encode()
        echoed = b"~ # " + checks.replace(b"\n", b"\r\n")
        microvm_tests.check_canceled_capture(
            echoed + _canceled_capture_output("kvm"), command
        )
        for output, message in (
            (_canceled_capture_output("kvm", rcu="1"), "rcu_cpu_stall_suppress is '1'"),
            (
                _canceled_capture_output("kvm", restored=True),
                "reported a restore at generation=1",
            ),
            (
                _canceled_capture_output("kvm", generation=1),
                "generation=1 is not 0 at cold boot",
            ),
            (_canceled_capture_output("kvm", status=1), "nvx-time status exited 1"),
            (
                _canceled_capture_output("kvm", status=None),
                "nvx-time status did not finish",
            ),
            (
                _canceled_capture_output("kvm", rcu=None),
                "expected exactly one b'NVX-CANCELED-CAPTURE-RCU ' marker, found 0",
            ),
        ):
            with self.subTest(message=message):
                with self.assertRaisesRegex(RuntimeError, re.escape(message)) as raised:
                    microvm_tests.check_canceled_capture(output, command)
                self.assertTrue(
                    str(raised.exception).startswith(
                        "after the released snapshot request: "
                    )
                )

    def test_snapshot_marker_parsers_require_single_well_formed_values(self):
        output = b"PREFIX-12\r\nPAIR-4-5\n"
        self.assertEqual(
            microvm_tests._single_marker_value(output, b"PREFIX-"),
            b"12",
        )
        self.assertEqual(
            microvm_tests._single_framed_marker_value(
                b"FRAME-17-END[kernel output]\n",
                b"FRAME-",
                b"-END",
            ),
            b"17",
        )
        self.assertEqual(
            microvm_tests._parse_marker_pair(output, b"PAIR-"),
            (4, 5),
        )
        with self.assertRaisesRegex(RuntimeError, "exactly one"):
            microvm_tests._single_marker_value(b"X-1\nX-2\n", b"X-")
        with self.assertRaisesRegex(RuntimeError, "exactly one"):
            microvm_tests._single_framed_marker_value(
                b"FRAME-17-ENDFRAME-34-END",
                b"FRAME-",
                b"-END",
            )
        with self.assertRaisesRegex(RuntimeError, "malformed"):
            microvm_tests._single_framed_marker_value(
                b"FRAME-17",
                b"FRAME-",
                b"-END",
            )
        with self.assertRaisesRegex(RuntimeError, "malformed"):
            microvm_tests._parse_marker_pair(b"PAIR-4\n", b"PAIR-")

    def test_console_snapshot_script_preserves_backend_specific_rx_and_tx(self):
        kvm, kvm_count, kvm_rx = microvm_tests._console_snapshot_script("kvm")
        mshv, mshv_count, mshv_rx = microvm_tests._console_snapshot_script("mshv")
        whp, whp_count, whp_rx = microvm_tests._console_snapshot_script("whp")

        self.assertEqual((kvm_count, mshv_count, whp_count), (10_000, 100, 1_000))
        self.assertEqual(kvm_rx, bytes((0, 1, 2, 127, 255)))
        self.assertEqual(whp_rx, kvm_rx)
        self.assertEqual(mshv_rx, b"NVX-CONSOLE-RX\n")
        self.assertIn("NVX-CONSOLE-RX-RESTORED", kvm)
        self.assertNotIn("NVX-CONSOLE-RX-RESTORED", mshv)
        self.assertIn("NVX-CONSOLE-TX-DONE", whp)
        self.assertIn("stty -F /dev/hvc1 raw -echo", whp)
        self.assertIn("nvx-console-pending /dev/hvc1", whp)
        self.assertIn(f'[ "$pending" -lt {len(whp_rx)} ]', whp)
        self.assertIn(f'[ "$pending" -lt {len(mshv_rx)} ]', mshv)
        self.assertIn('while [ "$snapshot_now" = 0 ]; do', whp)
        self.assertNotIn("sleep 1", whp)

    def test_console_snapshot_waits_until_rx_is_queued_before_snapshot(self):
        events: list[tuple[str, bytes] | tuple[str, bytes, float]] = []

        class RecordingConsole:
            def send_bytes(self, data: bytes) -> None:
                events.append(("send", data))

            def wait_for_line(self, marker: bytes, timeout: float) -> None:
                events.append(("wait", marker, timeout))

        console = cast(openvmm_process.TcpConsole, RecordingConsole())
        queued_rx = bytes((0, 1, 2, 127, 255))
        microvm_tests._send_console_rx_and_wait_until_queued(
            console,
            queued_rx,
            3.0,
        )

        self.assertEqual(
            events,
            [
                ("send", queued_rx),
                ("wait", microvm_tests.CONSOLE_RX_QUEUED_MARKER, 3.0),
            ],
        )

    def test_endpoint_policy_arguments_are_repeatable_and_ordered(self):
        command = ["openvmm"]
        microvm_tests._append_endpoint_policy(command, microvm_tests.ENDPOINT_POLICY)

        self.assertEqual(
            command,
            [
                "openvmm",
                "--allow-endpoint",
                "10.0.0.9:8443",
                "--allow-endpoint",
                "192.0.2.7:443",
                "--allow-endpoint",
                "10.0.0.9:443",
            ],
        )

    def test_network_snapshot_script_renders_host_ports(self):
        script = microvm_tests._render_script(
            "network-snapshot.sh.in",
            HTTP_PORT="1234",
            UDP_PORT="5678",
        )

        self.assertIn("10.0.0.1:1234/hold", script)
        self.assertIn("10.0.0.1 5678", script)
        self.assertNotIn("@HTTP_PORT@", script)
        self.assertNotIn("@UDP_PORT@", script)

    def test_snapshot_tier_scripts_preserve_tier_specific_policy(self):
        platform = microvm_tests._snapshot_tier_script("platform")
        workload = microvm_tests._snapshot_tier_script("workload-start")
        checkpoint = microvm_tests._snapshot_tier_script("instance-checkpoint")

        self.assertIn("/sbin/nvx-snapshot --tier platform", platform)
        self.assertIn("date -u -s 200001010000.00", platform)
        self.assertNotIn("mkfs.ext4", platform)
        self.assertIn("/sbin/nvx-snapshot --tier workload-start", workload)
        self.assertIn("mkfs.ext4 -F /dev/vdb", workload)
        self.assertIn("runtime-post-restore", workload)
        self.assertIn(": >/run/nvx/workload-ran", workload)
        self.assertIn("[ -e /run/nvx/workload-ran ]", workload)
        self.assertIn("/sbin/nvx-snapshot\n", checkpoint)
        self.assertIn("captured-workload-id", checkpoint)
        self.assertIn(
            "printf 'restored-direct-claimed\\n' "
            ">/run/nvx/scratch/direct-claimed\nsync",
            checkpoint,
        )
        self.assertNotIn("@CAPTURE_ACTION@", checkpoint)

    def test_restored_tier_guests_report_their_restore_checks_before_exiting(self):
        # The debug-kernel lane gates on snapshot-tiers, so every restored tier
        # guest waits for its deferred restore checks before it exits.
        status = microvm_tests.status_script().rstrip("\n")
        for tier in ("platform", "workload-start", "instance-checkpoint"):
            with self.subTest(tier=tier):
                script = microvm_tests._snapshot_tier_script(tier)
                self.assertNotIn("@STATUS_QUERY@", script)
                self.assertLess(
                    script.index(f"NVX-TIER-{tier.upper()}-LAYER-"),
                    script.index(f"set +e\n{status}\n"),
                )
                # The guest waits for the host's byte, so the query's lines
                # reach the console before nvx-exit stops the VM.
                self.assertTrue(
                    script.endswith(
                        f"{status}\ndd if=/dev/hvc1 bs=1 count=1 >/dev/null 2>&1\n"
                        "nvx-exit 0\n"
                    )
                )
        runner = inspect.getsource(microvm_tests._run_snapshot_tier)
        self.assertIn("_check_restore_status(\n            restore_console,", runner)
        # The status baseline is taken before the milestone reads, so a query
        # one of them consumes still counts for the wait.
        self.assertLess(
            runner.index("status_baseline = console.status_queries"),
            runner.index("console.wait_for(released_marker, timeout)"),
        )
        self.assertLess(
            runner.index("console.wait_for_time_abi_status("),
            runner.index('console.send_bytes(b"Z")\n                restored = '),
        )
        self.assertIn("since=status_baseline", runner)

    def test_snapshot_tier_entry_points_reject_unsupported_tiers(self):
        with self.assertRaisesRegex(ValueError, "unsupported snapshot tier 'invalid'"):
            microvm_tests._snapshot_tier_script("invalid")
        with self.assertRaisesRegex(ValueError, "unsupported snapshot tier 'invalid'"):
            microvm_tests._run_snapshot_tier(
                "invalid",
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
                memory_mib=128,
                timeout=60,
                output_dir=Path("logs"),
            )

    def test_snapshot_tier_runner_dispatches_all_tiers(self):
        with patch.object(microvm_tests, "_run_snapshot_tier") as run_tier:
            microvm_tests.run_snapshot_tiers(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
                memory_mib=128,
                timeout=60,
                output_dir=Path("logs"),
            )

        self.assertEqual(
            [entry.args[0] for entry in run_tier.call_args_list],
            ["platform", "workload-start", "instance-checkpoint"],
        )

    def test_snapshot_storage_policy_selects_generation_and_materialization(self):
        self.assertEqual(microvm_tests._snapshot_storage_policy("platform"), ())
        workload = microvm_tests._snapshot_storage_policy("workload-start")
        checkpoint = microvm_tests._snapshot_storage_policy("instance-checkpoint")

        self.assertEqual(
            workload,
            (
                "--snapshot-block-identity",
                "generation",
                "--snapshot-generation-id",
                "00112233445566778899aabbccddee01",
                "--snapshot-scratch-restore-mode",
                "private-copy",
            ),
        )
        self.assertEqual(checkpoint[-1], "direct-claimed")
        with self.assertRaisesRegex(ValueError, "unsupported snapshot tier"):
            microvm_tests._snapshot_storage_policy("other")

    def test_lifecycle_uses_one_vcpu_linux_guest(self):
        with (
            patch.object(
                microvm_tests,
                "workload_boot_command",
                return_value=["openvmm", "boot"],
            ) as boot_command,
            patch.object(microvm_tests, "run_guest_script") as run_guest_script,
        ):
            microvm_tests.run_lifecycle(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
                memory_mib=128,
                timeout=45,
                log_path=Path("lifecycle.log"),
            )

        boot_command.assert_called_once_with(
            Path("openvmm"),
            "kvm",
            Path("vmlinux"),
            Path("initrd"),
            128,
            "quiet loglevel=0",
        )
        script = run_guest_script.call_args.args[1]
        self.assertIn("NVX-LIFECYCLE-OK", script)
        self.assertIn("/^LOC:/", script)
        self.assertEqual(
            run_guest_script.call_args.args[2],
            microvm_tests.LIFECYCLE_COMPLETION_MARKER,
        )
        self.assertEqual(run_guest_script.call_args.kwargs["timeout"], 45)
        self.assertEqual(
            run_guest_script.call_args.kwargs["log_path"],
            Path("lifecycle.log"),
        )

    def test_lifecycle_script_exits_on_unexpected_command_failure(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")

        script = microvm_tests._read_script("lifecycle.sh")
        prologue, separator, _ = script.partition("\nprintf '\\013' | dd")
        self.assertTrue(separator)
        prologue = prologue.replace("nvx-exit", "record_exit")
        result = subprocess.run(
            [shell],
            input=(
                "record_exit() { printf 'NVX-EXIT %s\\n' \"$1\"; }\n"
                f"{prologue}\n"
                "false\n"
            ),
            text=True,
            capture_output=True,
            timeout=5,
            check=False,
        )

        self.assertEqual(result.returncode, 1)
        self.assertEqual(
            result.stdout.splitlines(),
            ["NVX-LIFECYCLE-FAIL code=1", "NVX-EXIT 1"],
        )

    def test_lifecycle_script_ends_its_debug_port_line(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        script = microvm_tests._read_script("lifecycle.sh")
        tail = script[script.index("for octal in") : script.index("nvx-exit 0")]
        # OpenVMM prints the debug port's bytes in the console stream, where
        # the staged script prints its marker right after them.
        result = subprocess.run(
            [shell],
            input=b"dd() { cat; }\n" + tail.encode(),
            capture_output=True,
            timeout=10,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(
            microvm_tests.contains_output_line(
                result.stdout, microvm_tests.LIFECYCLE_COMPLETION_MARKER
            ),
            result.stdout,
        )

    def test_smp_uses_requested_processor_count(self):
        text = (
            b"NVX-SMP-PROBE-OK\r\n" + _warp_probe_output(4) + b"nvx-exit 0\r\n"
        ).decode()
        with (
            patch.object(
                microvm_tests,
                "workload_boot_command",
                return_value=["openvmm", "boot"],
            ) as boot_command,
            patch.object(
                microvm_tests,
                "smp_probe_script",
                return_value="probe\n",
            ) as smp_probe_script,
            patch.object(
                microvm_tests,
                "run_guest_script",
                return_value=_guest_script_result(text),
            ) as run_guest_script,
        ):
            microvm_tests.run_smp(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "mshv",
                4,
                memory_mib=256,
                timeout=90,
                log_path=Path("smp-4.log"),
            )

        boot_command.assert_called_once_with(
            Path("openvmm"),
            "mshv",
            Path("vmlinux"),
            Path("initrd"),
            256,
            "quiet loglevel=0",
            processors=4,
        )
        smp_probe_script.assert_called_once_with(4, exit_guest=False)
        run_guest_script.assert_called_once_with(
            ["openvmm", "boot"],
            "probe\n" + time_abi.warp_probe_script() + "nvx-exit 0\n",
            time_abi.WARP_PROBE_COMPLETION_MARKER,
            timeout=90,
            log_path=Path("smp-4.log"),
        )

    def test_smp_requires_the_probe_marker_and_a_passing_warp_probe(self):
        cases = (
            (_warp_probe_output(4).decode(), "without the SMP probe marker"),
            (
                "NVX-SMP-PROBE-OK\n" + _warp_probe_output(2).decode(),
                "measured 1 CPU pairs instead of 6",
            ),
            (
                "NVX-SMP-PROBE-OK\n" + _warp_probe_output(4, offset_ns=1001).decode(),
                "max_abs_offset_ns=1001 exceeds 1000",
            ),
        )
        for text, message in cases:
            with self.subTest(message=message):
                with patch.object(
                    microvm_tests,
                    "run_guest_script",
                    return_value=_guest_script_result(text),
                ):
                    with self.assertRaisesRegex(RuntimeError, message):
                        microvm_tests.run_smp(
                            Path("openvmm"),
                            Path("vmlinux"),
                            Path("initrd"),
                            "kvm",
                            4,
                            memory_mib=128,
                            timeout=60,
                            log_path=Path("smp-4.log"),
                        )

    def test_smp_exercises_the_counting_lapic_without_a_duplicate_scenario(self):
        # The time ABI hides TSC-deadline on every backend, so the ordinary
        # smp scenario already exercises the one-shot counting LAPIC, and no
        # default suite also runs smp-lapic (#286).
        self.assertNotIn("smp-lapic", microvm_tests.MICROVM_TEST_SCENARIOS)
        self.assertNotIn("smp-lapic", microvm_tests.DEBUG_KERNEL_SCENARIOS)
        text = (b"NVX-SMP-PROBE-OK\r\n" + _warp_probe_output(4)).decode()
        with patch.object(
            microvm_tests, "run_guest_script", return_value=_guest_script_result(text)
        ) as run:
            microvm_tests.run_smp(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
                4,
                memory_mib=128,
                timeout=60,
                log_path=Path("smp-4.log"),
            )
        command, script, marker = run.call_args.args
        self.assertEqual(command[command.index("--cmdline") + 1], "quiet loglevel=0")
        self.assertTrue(
            script.startswith(benchmark.smp_probe_script(4, exit_guest=False))
        )
        self.assertNotIn("tsc_deadline_timer", script)
        self.assertEqual(marker, time_abi.WARP_PROBE_COMPLETION_MARKER)

    def test_smp_lapic_remains_an_explicit_scenario(self):
        # #286 keeps `test-microvm --scenario smp-lapic` for local use.
        self.assertEqual(microvm_tests.MICROVM_EXPLICIT_SCENARIOS, ("smp-lapic",))
        args = nvx.parse_args(
            ["test-microvm", "--backend", "kvm", "--scenario", "smp-lapic"]
        )
        self.assertEqual(args.scenario, ["smp-lapic"])

    def test_smp_lapic_asserts_the_counting_lapic_facts(self):
        rates = f"tsc_hz=2793437000 lapic_hz={time_abi.LAPIC_HZ['kvm']}"
        # The status query runs before the staged script, which prints the
        # markers.
        status = (
            f"NVX-TIME-ABI: v=1 phase=boot status=ok cpus=4 {rates} "
            "generation=0 elapsed_us=2390\r\n"
            "NVX-TIME-ABI: v=1 phase=runtime status=synchronized generation=0 "
            "discontinuities=0 offset_ns=-1200 uncertainty_ns=900 "
            "rejected_samples=0 last_sample_error=none\r\n"
            "NVX-TIME-STATUS-EXIT status=0\r\n"
        )
        counting = "NVX-SMP-LAPIC-COUNTING-OK\r\n"
        output = counting + "NVX-SMP-PROBE-OK\r\n" + _warp_probe_output(4).decode()

        def run_smp_lapic(transcript: str, output: str) -> MagicMock:
            with patch.object(
                microvm_tests,
                "run_guest_script",
                return_value=_guest_script_result(output, transcript=transcript),
            ) as run:
                microvm_tests.run_smp(
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "kvm",
                    4,
                    memory_mib=128,
                    timeout=60,
                    log_path=Path("smp-lapic-4.log"),
                    counting_lapic=True,
                )
            return run

        command, script, marker = run_smp_lapic(status, output).call_args.args
        self.assertEqual(command[command.index("--cmdline") + 1], "quiet loglevel=0")
        self.assertTrue(script.startswith(microvm_tests.counting_lapic_script(4)))
        self.assertIn(benchmark.smp_probe_script(4, exit_guest=False), script)
        self.assertEqual(marker, time_abi.WARP_PROBE_COMPLETION_MARKER)
        for label, transcript, printed, message in (
            (
                "missing",
                status,
                output.replace(counting, ""),
                "without the counting-LAPIC marker",
            ),
            # Only the script's own output counts.
            (
                "before the start line",
                status + counting,
                output.replace(counting, ""),
                "without the counting-LAPIC marker",
            ),
            (
                "two CPUs",
                status.replace("cpus=4 tsc_hz", "cpus=2 tsc_hz"),
                output,
                "cpus=2 is not 4",
            ),
        ):
            with (
                self.subTest(label),
                self.assertRaisesRegex(RuntimeError, message),
            ):
                run_smp_lapic(transcript, printed)

    def test_counting_lapic_check_requires_the_counting_lapic_on_every_cpu(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        script = (
            microvm_tests.counting_lapic_script(2)
            .replace("/proc/cpuinfo", "cpuinfo")
            .replace("/sys/devices/system/clockevents/", "clockevents/")
        )
        for name, flags, devices, status, expected in (
            ("counting", "fpu tsc apic", ("lapic", "lapic"), 0, "COUNTING-OK"),
            (
                "deadline-flag",
                "fpu tsc_deadline_timer apic",
                ("lapic", "lapic"),
                89,
                "SMP-LAPIC-FAIL tsc-deadline-exposed",
            ),
            (
                "deadline-device",
                "fpu tsc apic",
                ("lapic", "lapic-deadline"),
                89,
                "SMP-LAPIC-FAIL cpu=1 clockevent=lapic-deadline",
            ),
            (
                "missing-device",
                "fpu tsc apic",
                ("lapic",),
                89,
                "SMP-LAPIC-FAIL cpu=1 clockevent=none",
            ),
        ):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / "cpuinfo").write_text(
                    f"processor\t: 0\nflags\t\t: {flags}\n", encoding="ascii"
                )
                for cpu, device in enumerate(devices):
                    directory = root / "clockevents" / f"clockevent{cpu}"
                    directory.mkdir(parents=True)
                    (directory / "current_device").write_text(
                        device + "\n", encoding="ascii"
                    )
                result = subprocess.run(
                    [shell],
                    cwd=root,
                    input=script,
                    text=True,
                    capture_output=True,
                    timeout=10,
                    check=False,
                )
                self.assertEqual(
                    result.returncode, status, result.stdout + result.stderr
                )
                self.assertIn(expected, result.stdout)

    def test_virtio_net_uses_portable_endpoint_policy(self):
        with (
            patch.object(
                microvm_tests,
                "workload_boot_command",
                return_value=["openvmm", "boot"],
            ) as boot_command,
            patch.object(microvm_tests, "run_guest_script") as run_guest_script,
        ):
            microvm_tests.run_virtio_net(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
                memory_mib=128,
                timeout=60,
                log_path=Path("virtio-net.log"),
            )

        boot_command.assert_called_once_with(
            Path("openvmm"),
            "whp",
            Path("vmlinux"),
            Path("initrd"),
            128,
            "quiet loglevel=0",
            network="10.0.0.2/24",
        )
        command = run_guest_script.call_args.args[0]
        self.assertEqual(command.count("--allow-endpoint"), 3)
        self.assertIn("192.0.2.7:443", command)
        script = run_guest_script.call_args.args[1]
        self.assertIn("virtnet_ip=10.0.0.2", script)
        self.assertIn("10.0.0.10", script)
        self.assertEqual(
            run_guest_script.call_args.args[2],
            microvm_tests.VIRTIO_NET_COMPLETION_MARKER,
        )

    def test_command_line_limit_fills_the_guest_kernel_buffer(self):
        self.assertIn("command-line-limit", microvm_tests.MICROVM_TEST_SCENARIOS)
        # The tokens that OpenVMM reports around the caller's, ending with its
        # network bootstrap.
        head = "earlycon=xe9 console=hvc0 reboot=t panic=-1 quiet loglevel=0"
        tail = (
            "nr_cpus=1 virtio_mmio.device=0x1000@0xd0000000:5 virtnet_ip=10.0.0.2 "
            "virtnet_mask=255.255.255.0 virtnet_gw=10.0.0.1 "
            "virtnet_ip6=fd00::a00:2/120 virtnet_gw6=fd00::a00:1"
        )

        def line(padding: int) -> str:
            return f"{head} cmdline_limit.padding={'x' * padding} {tail}"

        padding = 2048 - len(line(1))
        self.assertEqual(len(line(padding)), 2047)
        refused = openvmm_process.OpenvmmProcessResult(
            1, b"Error: " + microvm_tests.COMMAND_LINE_LIMIT_ERROR + b"\n"
        )

        def guest(text: str) -> microvm_tests.GuestScriptResult:
            return _guest_script_result(
                f"NVX-COMMAND-LINE: {text}\r\nNVX-COMMAND-LINE-LIMIT-OK\r\n",
                # The echo of the staged script, cut before its marker.
                transcript='NVX-COMMAND-LINE: %s\\n\' "$(cat /proc/cmdline)"\r\n',
            )

        def run_scenario(
            longest: str, over: openvmm_process.OpenvmmProcessResult
        ) -> tuple[MagicMock, MagicMock]:
            with (
                tempfile.TemporaryDirectory() as temporary,
                patch.object(
                    microvm_tests,
                    "run_guest_script",
                    side_effect=[guest(line(1)), guest(longest)],
                ) as run_guest_script,
                patch.object(microvm_tests, "OpenvmmProcess") as process,
            ):
                process.return_value.__enter__.return_value.wait.return_value = over
                microvm_tests.run_command_line_limit(
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "whp",
                    memory_mib=128,
                    timeout=60,
                    output_dir=Path(temporary),
                )
            return run_guest_script, process

        run_guest_script, process = run_scenario(line(padding), refused)
        commands = [call.args[0] for call in run_guest_script.call_args_list]
        commands.append(process.call_args.args[0])
        self.assertEqual(
            [command[command.index("--cmdline") + 1] for command in commands],
            [
                f"quiet loglevel=0 cmdline_limit.padding={'x' * length}"
                for length in (1, padding, padding + 1)
            ],
        )
        for command in commands:
            self.assertEqual(command[command.index("--net") + 1], "10.0.0.2/24")
        for call in run_guest_script.call_args_list:
            self.assertEqual(
                call.args[1:],
                (
                    microvm_tests._read_script("command-line-limit.sh"),
                    microvm_tests.COMMAND_LINE_LIMIT_COMPLETION_MARKER,
                ),
            )
        self.assertEqual(
            [call.kwargs["log_path"].name for call in run_guest_script.call_args_list],
            ["command-line-limit-shortest.log", "command-line-limit.log"],
        )

        # A guest that lost a token, or an OpenVMM that boots or fails
        # otherwise one byte over the limit, fails the scenario.
        for longest, over, message in (
            (line(padding)[:-1], refused, "not the expected 2047-byte one"),
            (
                line(padding),
                openvmm_process.OpenvmmProcessResult(
                    0, microvm_tests.BOOT_MARKER + b" alpine\n"
                ),
                "did not refuse",
            ),
            (
                line(padding),
                openvmm_process.OpenvmmProcessResult(1, b"Error: other\n"),
                "did not refuse",
            ),
        ):
            with (
                self.subTest(message=message),
                self.assertRaisesRegex(RuntimeError, message),
            ):
                run_scenario(longest, over)

    def test_command_line_limit_script_requires_the_last_token_route(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        route = (
            "00000000000000000000000000000000 00 00000000000000000000000000000000 00 "
            "fd00000000000000000000000a000001 00000400 00000001 00000000 00000003 "
            "eth0\n"
        )
        command_line = "console=hvc0 virtnet_gw6=fd00::a00:1"
        for name, table, status, expected in (
            ("route", route, 0, "NVX-COMMAND-LINE-LIMIT-OK"),
            ("no-route", "", 20, "NVX-COMMAND-LINE-LIMIT-FAIL code=20"),
        ):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / "proc" / "net").mkdir(parents=True)
                (root / "proc" / "cmdline").write_text(
                    command_line + "\n", encoding="ascii"
                )
                (root / "proc" / "net" / "ipv6_route").write_text(
                    table, encoding="ascii"
                )
                path = root / "command-line-limit.sh"
                path.write_text(
                    "nvx_exit() { :; }\n"
                    + microvm_tests._read_script("command-line-limit.sh")
                    .replace("nvx-exit", "nvx_exit")
                    .replace("/proc/", f"{root.as_posix()}/proc/"),
                    encoding="utf-8",
                    newline="\n",
                )
                result = subprocess.run(
                    [shell, path.as_posix()],
                    capture_output=True,
                    text=True,
                    timeout=10,
                    check=False,
                )
                self.assertEqual(
                    result.returncode, status, result.stdout + result.stderr
                )
                self.assertIn(f"NVX-COMMAND-LINE: {command_line}\n", result.stdout)
                self.assertIn(expected, result.stdout)

    def test_runner_runs_the_command_line_limit_scenario(self):
        def require(path: Path, _description: str) -> Path:
            return path

        with tempfile.TemporaryDirectory() as temporary:
            args = nvx.parse_args(
                ["test-microvm", "--backend", "mshv", "--output-dir", temporary]
            )
            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(
                    microvm_tests, "MICROVM_TEST_SCENARIOS", ("command-line-limit",)
                ),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(microvm_tests, "run_command_line_limit") as run,
            ):
                self.assertEqual(microvm_tests.run(args), 0)

        run.assert_called_once()
        self.assertEqual(run.call_args.args[3], "mshv")
        self.assertEqual(run.call_args.kwargs["output_dir"], Path(temporary))

    def test_directional_network_commands_map_generic_default_actions(self):
        with patch.object(
            microvm_tests,
            "workload_boot_command",
            side_effect=[["openvmm", "boot"], ["openvmm", "boot"]],
        ) as boot_command:
            allow = microvm_tests._directional_network_command(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
                128,
                egress="allow",
                ingress="deny",
            )
            deny = microvm_tests._directional_network_command(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
                128,
                egress="deny",
                ingress="deny",
            )

        self.assertEqual(boot_command.call_count, 2)
        self.assertEqual(
            allow[-4:],
            ["--network-egress", "allow", "--network-ingress", "deny"],
        )
        self.assertEqual(
            deny[-4:],
            ["--network-egress", "deny", "--network-ingress", "deny"],
        )
        with self.assertRaisesRegex(ValueError, "actions must be"):
            microvm_tests._directional_network_command(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
                128,
                egress="block",
                ingress="deny",
            )

    def test_egress_port_reservation_closes_tcp_when_udp_fails(self):
        endpoints = [MagicMock(spec=socket.socket) for _ in range(5)]
        with (
            patch.object(
                microvm_tests,
                "_bind_consecutive_ports",
                side_effect=[endpoints, RuntimeError("UDP unavailable")],
            ),
            patch.object(microvm_tests, "compile_egress_policy") as compile_policy,
            self.assertRaisesRegex(RuntimeError, "UDP unavailable"),
        ):
            microvm_tests.run_l3_l4_egress_policy(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initramfs"),
                "whp",
                memory_mib=128,
                timeout=1,
                output_dir=Path("."),
            )
        for endpoint in endpoints:
            endpoint.close.assert_called_once_with()
        compile_policy.assert_not_called()

    def test_bounded_egress_acceptance_policy_lowers_ranges_and_exclusions(self):
        policy = microvm_tests._bounded_egress_policy(
            "192.0.2.1",
            (21001, 21002, 21003),
            (22001, 22002, 22003),
        )

        # The excluded gateway's own rule fills the exclusion, so each range
        # lowers to one rule for the whole subnet.
        self.assertEqual(
            policy.allow,
            ("192.0.2.0/24:tcp:21001-21003", "192.0.2.0/24:udp:22001-22003"),
        )
        self.assertIn("192.0.2.0/24:tcp:21002", policy.deny)
        self.assertIn("192.0.2.0/24:udp:22002", policy.deny)
        self.assertIn("192.0.2.128/25:tcp:21001-21003", policy.deny)
        self.assertIn("192.0.2.128/25:udp:22001-22003", policy.deny)

    def test_l3_l4_egress_acceptance_invokes_public_nvx_policy_file(self):
        class ImmediateThread:
            def __init__(
                self, *, target: Callable[[], None], **_kwargs: object
            ) -> None:
                self.target = target

            def start(self) -> None:
                self.target()

            def join(self, _timeout: float | None = None, **_kwargs: object) -> None:
                pass

            def is_alive(self) -> bool:
                return False

        for guest, memory_mib in (("alpine", 128), ("ubuntu", 256)):
            with self.subTest(guest=guest):
                tcp = [MagicMock(spec=socket.socket) for _ in range(5)]
                udp = [MagicMock(spec=socket.socket) for _ in range(5)]
                for index, endpoint in enumerate(tcp):
                    endpoint.getsockname.return_value = ("0.0.0.0", 21000 + index)
                for index, endpoint in enumerate(udp):
                    endpoint.getsockname.return_value = ("0.0.0.0", 22000 + index)
                for endpoint in (tcp[0], tcp[2], tcp[4]):
                    endpoint.accept.side_effect = TimeoutError
                for endpoint in (udp[0], udp[2], udp[4]):
                    endpoint.recvfrom.side_effect = TimeoutError
                for endpoint in (tcp[1], tcp[3]):
                    connection = MagicMock(spec=socket.socket)
                    connection.recv.return_value = b"GET /allowed HTTP/1.1\r\n\r\n"
                    endpoint.accept.return_value = (connection, ("127.0.0.1", 1))
                udp[1].recvfrom.return_value = (
                    b"NVX-L3-L4-UDP-ALLOW-START",
                    ("127.0.0.1", 1),
                )
                udp[3].recvfrom.return_value = (
                    b"NVX-L3-L4-UDP-ALLOW-END",
                    ("127.0.0.1", 1),
                )

                with (
                    tempfile.TemporaryDirectory() as temporary,
                    patch.object(
                        microvm_tests,
                        "_bind_egress_ports",
                        return_value=(tcp, udp),
                    ),
                    patch.object(microvm_tests, "run_guest_script") as run_guest_script,
                    patch.object(microvm_tests, "OpenvmmProcess") as openvmm_process,
                    patch.object(
                        microvm_tests,
                        "compile_egress_policy",
                        return_value=nvx_rs.EgressRules(allow=(), deny=()),
                    ),
                    patch.object(
                        microvm_tests.threading,
                        "Thread",
                        side_effect=ImmediateThread,
                    ),
                    patch.object(nvx_rs, "require_binary", return_value=_FAKE_NVX_RS),
                ):
                    wait = openvmm_process.return_value.__enter__.return_value.wait
                    wait.side_effect = (
                        MagicMock(
                            returncode=1,
                            output=b"--network-egress is required",
                        ),
                        MagicMock(
                            returncode=1,
                            output=b"invalid egress transport",
                        ),
                    )
                    output_dir = Path(temporary)
                    microvm_tests.run_l3_l4_egress_policy(
                        Path("openvmm"),
                        Path("vmlinux"),
                        Path("initramfs"),
                        "whp",
                        memory_mib=memory_mib,
                        timeout=1,
                        output_dir=output_dir,
                        guest=guest,
                    )

                    policy_path = output_dir / "l3-l4-requested-policy.json"
                    self.assertEqual(
                        run_guest_script.call_args.args[0],
                        [
                            *_FAKE_PUBLIC_RUN,
                            "--guest",
                            guest,
                            "--hypervisor",
                            "whp",
                            "--memory-mib",
                            str(memory_mib),
                            "--net",
                            microvm_tests.DIRECTIONAL_NETWORK_CIDR,
                            "--network-profile",
                            "portable",
                            "--network-egress",
                            "deny",
                            "--network-ingress",
                            "deny",
                            "--network-egress-policy-file",
                            str(policy_path),
                            "--cmdline",
                            "quiet loglevel=0",
                        ],
                    )
                    self.assertIs(
                        run_guest_script.call_args.kwargs["contain_process_tree"],
                        True,
                    )
                    requested = json.loads(policy_path.read_text(encoding="utf-8"))
                    self.assertEqual(requested["allow"][0]["port"], 21001)
                    self.assertEqual(requested["allow"][0]["endPort"], 21003)
                    self.assertEqual(requested["deny"][1]["port"], 21002)
                    results = json.loads(
                        (output_dir / "l3-l4-egress-policy-results.json").read_text(
                            encoding="utf-8"
                        )
                    )
                    self.assertEqual(results["interface"], "nvx-rs run")
                    self.assertEqual(
                        results["observed"]["allowed"],
                        ["tcp:start", "tcp:end", "udp:start", "udp:end"],
                    )
                    self.assertEqual(
                        results["observed"]["blocked"],
                        [
                            "tcp:adjacent-low",
                            "tcp:interior",
                            "tcp:adjacent-high",
                            "udp:adjacent-low",
                            "udp:interior",
                            "udp:adjacent-high",
                        ],
                    )

    def test_l3_l4_egress_does_not_report_unobserved_success(self):
        class ControlledThread:
            run_target = False

            def __init__(
                self, *, target: Callable[[], None], **_kwargs: object
            ) -> None:
                self.target = target

            def start(self) -> None:
                if self.run_target:
                    self.target()

            def join(self, _timeout: float | None = None, **_kwargs: object) -> None:
                pass

            def is_alive(self) -> bool:
                return False

        for failure in ("absent-positive", "unexpected-connection"):
            with self.subTest(failure=failure):
                tcp = [MagicMock(spec=socket.socket) for _ in range(5)]
                udp = [MagicMock(spec=socket.socket) for _ in range(5)]
                for index, endpoint in enumerate(tcp):
                    endpoint.getsockname.return_value = ("0.0.0.0", 21000 + index)
                for index, endpoint in enumerate(udp):
                    endpoint.getsockname.return_value = ("0.0.0.0", 22000 + index)
                for endpoint in (tcp[0], tcp[2], tcp[4]):
                    endpoint.accept.side_effect = TimeoutError
                for endpoint in (udp[0], udp[2], udp[4]):
                    endpoint.recvfrom.side_effect = TimeoutError

                ControlledThread.run_target = failure == "unexpected-connection"
                if ControlledThread.run_target:
                    for endpoint in (tcp[1], tcp[3]):
                        connection = MagicMock(spec=socket.socket)
                        connection.recv.return_value = b"GET /allowed HTTP/1.1\r\n\r\n"
                        endpoint.accept.return_value = (
                            connection,
                            ("127.0.0.1", 1),
                        )
                    udp[1].recvfrom.return_value = (
                        b"NVX-L3-L4-UDP-ALLOW-START",
                        ("127.0.0.1", 1),
                    )
                    udp[3].recvfrom.return_value = (
                        b"NVX-L3-L4-UDP-ALLOW-END",
                        ("127.0.0.1", 1),
                    )
                    tcp[0].accept.side_effect = None
                    tcp[0].accept.return_value = (
                        MagicMock(spec=socket.socket),
                        ("127.0.0.1", 1),
                    )

                with tempfile.TemporaryDirectory() as temporary:
                    output_dir = Path(temporary)
                    with (
                        patch.object(
                            microvm_tests,
                            "_bind_egress_ports",
                            return_value=(tcp, udp),
                        ),
                        patch.object(microvm_tests, "run_guest_script"),
                        patch.object(
                            microvm_tests,
                            "compile_egress_policy",
                            return_value=nvx_rs.EgressRules(allow=(), deny=()),
                        ),
                        patch.object(
                            microvm_tests.threading,
                            "Thread",
                            side_effect=ControlledThread,
                        ),
                        self.assertRaises(RuntimeError),
                    ):
                        microvm_tests.run_l3_l4_egress_policy(
                            Path("openvmm"),
                            Path("vmlinux"),
                            Path("initramfs"),
                            "whp",
                            memory_mib=128,
                            timeout=1,
                            output_dir=output_dir,
                        )
                    self.assertFalse(
                        (output_dir / "l3-l4-egress-policy-results.json").exists()
                    )

    def test_runner_dispatches_public_l3_l4_egress_acceptance(self):
        def require(path: Path, _description: str) -> Path:
            return path

        for guest, memory_mib in (("alpine", 128), ("ubuntu", 512)):
            with (
                self.subTest(guest=guest),
                tempfile.TemporaryDirectory() as temporary,
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(
                    microvm_tests, "run_l3_l4_egress_policy"
                ) as run_l3_l4_egress_policy,
            ):
                args = nvx.parse_args(
                    [
                        "test-microvm",
                        "--backend",
                        "whp",
                        "--guest",
                        guest,
                        "--scenario",
                        "l3-l4-egress-policy",
                        "--output-dir",
                        temporary,
                    ]
                )
                self.assertEqual(microvm_tests.run(args), 0)

                run_l3_l4_egress_policy.assert_called_once_with(
                    microvm_tests.openvmm_binary_path(),
                    microvm_tests.artifact_path(
                        microvm_tests.KernelBuildConstants.BINARY_NAME
                    ),
                    microvm_tests.artifact_path(
                        microvm_tests.guest_descriptor(guest).initramfs_name
                    ),
                    "whp",
                    memory_mib=memory_mib,
                    timeout=60.0,
                    output_dir=Path(temporary),
                    guest=guest,
                )

    def test_egress_protocol_cases_cover_each_selector_as_allow_and_deny(self):
        cases = {
            case.name: case
            for case in microvm_tests._egress_protocol_cases("192.0.2.1", 20000)
        }
        every_probe = frozenset(microvm_tests.L3_L4_EGRESS_PROBES)
        expected = {
            "tcp": ({"protocol": "tcp"}, {"tcp-named", "tcp-other"}),
            "udp": ({"protocol": "udp"}, {"udp-named", "udp-other"}),
            "icmp": ({"protocol": "icmp"}, {"icmp"}),
            "any-port": (
                {"protocol": "any", "port": 20000},
                {"tcp-named", "udp-named"},
            ),
            "any": ({"protocol": "any"}, set(every_probe)),
        }
        self.assertEqual(
            set(cases),
            {f"{action}-{name}" for action in ("allow", "deny") for name in expected},
        )
        for name, (selector, matched) in expected.items():
            rule = {"cidr": "192.0.2.1/32", **selector}
            with self.subTest(selector=name):
                allow = cases[f"allow-{name}"]
                self.assertEqual((allow.allow, allow.deny), ((rule,), ()))
                self.assertEqual(allow.reached, matched)
                deny = cases[f"deny-{name}"]
                self.assertEqual(
                    (deny.allow, deny.deny), (({"cidr": "192.0.2.1/32"},), (rule,))
                )
                self.assertEqual(deny.reached, every_probe - matched)

    def _run_egress_protocols(
        self,
        output_dir: Path,
        *,
        leaked: tuple[str, str] | None = None,
        guest_report: tuple[str, str, str] | None = None,
    ) -> tuple[MagicMock, MagicMock]:
        tcp = [MagicMock(spec=socket.socket) for _ in range(2)]
        udp = [MagicMock(spec=socket.socket) for _ in range(2)]
        for index, endpoint in enumerate((*tcp, *udp)):
            endpoint.getsockname.return_value = ("0.0.0.0", 20000 + index % 2)
        cases = {
            case.name: case
            for case in microvm_tests._egress_protocol_cases(
                microvm_tests.DIRECTIONAL_NETWORK_GATEWAY_IPV4, 20000
            )
        }
        current: list[str] = []

        def guest(
            command: list[str], _script: str, _marker: bytes, **_kwargs: object
        ) -> microvm_tests.GuestScriptResult:
            policy = Path(command[command.index("--network-egress-policy-file") + 1])
            name = policy.name.removeprefix("l3-l4-egress-").removesuffix(
                "-policy.json"
            )
            current.append(name)
            reports = {
                probe: "sent"
                if probe.startswith("udp")
                else "reached"
                if probe in cases[name].reached
                else "blocked"
                for probe in microvm_tests.L3_L4_EGRESS_PROBES
            }
            if guest_report is not None and guest_report[0] == name:
                reports[guest_report[1]] = guest_report[2]
            text = "".join(
                f"NVX-L3-L4-PROBE {probe}={value}\r\n"
                for probe, value in reports.items()
            )
            return _guest_script_result(
                text + "NVX-L3-L4-EGRESS-PROTOCOLS-OK\r\n",
                # The tty echoes script lines, which must not count as reports.
                transcript='> echo "NVX-L3-L4-PROBE icmp=echoed"\r\n',
            )

        class FakeServer:
            def __init__(
                self, endpoints: dict[str, socket.socket], _timeout: float
            ) -> None:
                assert list(endpoints.values()) == [*tcp, *udp]
                assert list(endpoints) == list(microvm_tests.L3_L4_EGRESS_PROBES[:4])

            def __enter__(self) -> "FakeServer":
                return self

            def __exit__(self, *_: object) -> None:
                pass

            def finish(self) -> frozenset[str]:
                name = current[-1]
                reached = set(cases[name].reached - {"icmp"})
                if leaked is not None and leaked[0] == name:
                    reached.add(leaked[1])
                return frozenset(reached)

        with (
            patch.object(
                microvm_tests, "_bind_dual_protocol_ports", return_value=(tcp, udp)
            ),
            patch.object(microvm_tests, "_EgressProbeServer", FakeServer),
            patch.object(
                microvm_tests, "run_guest_script", side_effect=guest
            ) as run_guest_script,
            patch.object(microvm_tests, "OpenvmmProcess") as openvmm_process,
        ):
            wait = openvmm_process.return_value.__enter__.return_value.wait
            wait.side_effect = (
                MagicMock(returncode=2, output=b"ICMP egress rules do not take a port"),
                MagicMock(returncode=2, output=b"invalid egress transport 'any'"),
            )
            try:
                microvm_tests.run_l3_l4_egress_protocols(
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initramfs"),
                    "whp",
                    memory_mib=128,
                    timeout=1,
                    output_dir=output_dir,
                )
            finally:
                for endpoint in (*tcp, *udp):
                    endpoint.close.assert_called_once_with()
        return run_guest_script, openvmm_process

    def test_l3_l4_egress_protocols_run_every_case_through_public_nvx_run(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            run_guest_script, openvmm_process = self._run_egress_protocols(output_dir)

            names = [
                case.name
                for case in microvm_tests._egress_protocol_cases(
                    microvm_tests.DIRECTIONAL_NETWORK_GATEWAY_IPV4, 20000
                )
            ]
            self.assertEqual(run_guest_script.call_count, len(names))
            public_run = nvx_rs.command_line(("run",))
            for call, name in zip(run_guest_script.call_args_list, names, strict=True):
                policy_path = output_dir / f"l3-l4-egress-{name}-policy.json"
                self.assertEqual(
                    call.args[0],
                    [
                        *public_run,
                        "--guest",
                        "alpine",
                        "--hypervisor",
                        "whp",
                        "--memory-mib",
                        "128",
                        "--net",
                        microvm_tests.DIRECTIONAL_NETWORK_CIDR,
                        "--network-profile",
                        "portable",
                        "--network-egress",
                        "deny",
                        "--network-ingress",
                        "deny",
                        "--network-egress-policy-file",
                        str(policy_path),
                        "--cmdline",
                        "quiet loglevel=0",
                    ],
                )
                self.assertNotIn("@", call.args[1])
                self.assertIn(" 20000 &\n", call.args[1])
                self.assertIn(" 20001 &\n", call.args[1])
                self.assertEqual(
                    call.args[2], microvm_tests.L3_L4_EGRESS_PROTOCOLS_COMPLETION_MARKER
                )
                self.assertIs(call.kwargs["contain_process_tree"], True)

            requested = json.loads(
                (output_dir / "l3-l4-egress-allow-any-port-policy.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(
                requested,
                {
                    "allow": [
                        {"cidr": "192.0.2.1/32", "protocol": "any", "port": 20000}
                    ],
                    "deny": [],
                },
            )
            rejected = [call.args[0][-1] for call in openvmm_process.call_args_list]
            self.assertEqual(rejected, ["192.0.2.1:icmp:443", "192.0.2.1:any"])

            results = json.loads(
                (output_dir / "l3-l4-egress-protocols-results.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(results["ports"], {"named": 20000, "other": 20001})
            by_case = {case["case"]: case for case in results["cases"]}
            self.assertEqual(list(by_case), names)
            self.assertEqual(
                by_case["deny-tcp"]["host_reached"], ["udp-named", "udp-other"]
            )
            self.assertEqual(by_case["deny-tcp"]["guest"]["icmp"], "reached")
            self.assertEqual(by_case["allow-icmp"]["host_reached"], [])
            self.assertEqual(by_case["allow-icmp"]["guest"]["icmp"], "reached")

    def test_l3_l4_egress_protocols_reject_leaks_and_missed_traffic(self):
        failures: tuple[
            tuple[tuple[str, str] | None, tuple[str, str, str] | None, str], ...
        ] = (
            (("allow-tcp", "udp-named"), None, "allow-tcp: udp-named"),
            (("deny-any-port", "tcp-named"), None, "deny-any-port: tcp-named"),
            (
                None,
                ("deny-icmp", "icmp", "reached"),
                "deny-icmp: icmp should be blocked",
            ),
            (
                None,
                ("allow-any", "tcp-other", "blocked"),
                "allow-any: tcp-other should be reached",
            ),
            (None, ("allow-udp", "udp-named", "failed-2"), "allow-udp: udp-named"),
            (None, ("allow-tcp", "tcp-named", "unexpected"), "allow-tcp: tcp-named"),
        )
        for leaked, guest_report, message in failures:
            with (
                self.subTest(message=message),
                tempfile.TemporaryDirectory() as temporary,
            ):
                output_dir = Path(temporary)
                with self.assertRaisesRegex(RuntimeError, re.escape(message)):
                    self._run_egress_protocols(
                        output_dir, leaked=leaked, guest_report=guest_report
                    )
                self.assertFalse(
                    (output_dir / "l3-l4-egress-protocols-results.json").exists()
                )

    def _run_egress_script(
        self, shell: str, script: str
    ) -> tuple[subprocess.CompletedProcess[str], list[str]]:
        """Run an L3/L4 egress script in the host's shell with a TMPDIR of its
        own, which holds a stale result, and return the shell's result and the
        names of the files that the run left in that TMPDIR."""
        with tempfile.TemporaryDirectory() as temporary:
            probe_dir = Path(temporary)
            (probe_dir / "nvx-egress-stale").write_text("stale\n", encoding="utf-8")
            result = subprocess.run(
                [shell],
                input=script,
                text=True,
                capture_output=True,
                timeout=10,
                check=False,
                env=_probe_environment(probe_dir),
            )
            return result, sorted(path.name for path in probe_dir.iterdir())

    def test_l3_l4_egress_protocol_script_reports_each_probe(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")

        script = microvm_tests._render_script(
            "l3-l4-egress-protocols.sh.in",
            GATEWAY_IPV4="192.0.2.1",
            NAMED_PORT="20000",
            OTHER_PORT="20001",
        ).replace("nvx-exit", "record_exit")
        stubs = (
            "record_exit() { printf 'NVX-EXIT %s\\n' \"$1\"; }\n"
            "wget() {\n"
            "    for url; do :; done\n"
            '    case "$url" in\n'
            "    */tcp-named) printf NVX-EGRESS-PROBE-tcp-named ;;\n"
            "    */tcp-other) printf WRONG ;;\n"
            "    esac\n"
            "}\n"
            "nc() {\n"
            "    cat >/dev/null\n"
            "    for port; do :; done\n"
            '    [ "$port" = 20000 ] || return 2\n'
            "}\n"
            "ping() { return 1; }\n"
        )
        result, probes = self._run_egress_script(shell, stubs + script)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.splitlines(),
            [
                "NVX-L3-L4-PROBE tcp-named=reached",
                "NVX-L3-L4-PROBE tcp-other=unexpected",
                "NVX-L3-L4-PROBE udp-named=sent",
                "NVX-L3-L4-PROBE udp-other=failed-2",
                "NVX-L3-L4-PROBE icmp=blocked",
                "NVX-L3-L4-EGRESS-PROTOCOLS-OK",
                "NVX-EXIT 0",
            ],
        )
        # The run replaced the stale result in its TMPDIR with its probes'.
        self.assertEqual(
            probes,
            [
                "nvx-egress-icmp",
                "nvx-egress-tcp-named",
                "nvx-egress-tcp-other",
                "nvx-egress-udp-named",
                "nvx-egress-udp-other",
            ],
        )

    def test_egress_probe_server_records_only_matching_probes(self):
        tcp, udp = microvm_tests._bind_dual_protocol_ports(2, "127.0.0.1")
        try:
            named, other = (int(endpoint.getsockname()[1]) for endpoint in tcp)
            self.assertEqual(other, named + 1)
            self.assertEqual(
                [int(endpoint.getsockname()[1]) for endpoint in udp], [named, other]
            )
            endpoints = {
                "tcp-named": tcp[0],
                "tcp-other": tcp[1],
                "udp-named": udp[0],
                "udp-other": udp[1],
            }

            with microvm_tests._EgressProbeServer(endpoints, 5) as server:
                with socket.create_connection(
                    ("127.0.0.1", named), timeout=5
                ) as client:
                    client.sendall(b"GET /tcp-named HTTP/1.1\r\nHost: test\r\n\r\n")
                    response = b""
                    while chunk := client.recv(4096):
                        response += chunk
                with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as client:
                    client.sendto(b"NVX-EGRESS-PROBE-udp-other", ("127.0.0.1", other))
                reached = server.finish()
            self.assertTrue(response.startswith(b"HTTP/1.1 200 OK\r\n"))
            self.assertTrue(response.endswith(b"\r\n\r\nNVX-EGRESS-PROBE-tcp-named"))
            self.assertEqual(reached, {"tcp-named", "udp-other"})

            with microvm_tests._EgressProbeServer(endpoints, 5) as server:
                with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as client:
                    client.sendto(b"NVX-EGRESS-PROBE-udp-other", ("127.0.0.1", named))
                with self.assertRaisesRegex(RuntimeError, "probe server failed"):
                    server.finish()
        finally:
            for endpoint in (*tcp, *udp):
                endpoint.close()

    def test_dual_protocol_port_reservation_skips_partially_free_ports(self):
        socket_type = socket.socket
        created: list[MagicMock] = []

        def endpoint(_family: int, kind: int) -> MagicMock:
            reserved = MagicMock(spec=socket_type)

            def bind(address: tuple[str, int]) -> None:
                if kind == socket.SOCK_DGRAM and address[1] == 20001:
                    raise OSError("address in use")

            reserved.bind.side_effect = bind
            created.append(reserved)
            return reserved

        with patch.object(microvm_tests.socket, "socket", side_effect=endpoint):
            tcp, udp = microvm_tests._bind_dual_protocol_ports(2)
        reserved_endpoints = cast(list[MagicMock], [*tcp, *udp])

        self.assertEqual(
            [
                call.args[0][1]
                for reserved in reserved_endpoints
                for call in reserved.bind.call_args_list
            ],
            [20002, 20003, 20002, 20003],
        )
        for reserved in created:
            if reserved in reserved_endpoints:
                reserved.close.assert_not_called()
            else:
                reserved.close.assert_called_once_with()
        self.assertEqual(len(created), 10)

    def test_runner_dispatches_l3_l4_egress_protocols(self):
        def require(path: Path, _description: str) -> Path:
            return path

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(microvm_tests, "validate_openvmm_test_backend"),
            patch.object(microvm_tests, "require_file", side_effect=require),
            patch.object(
                microvm_tests, "run_l3_l4_egress_protocols"
            ) as run_l3_l4_egress_protocols,
        ):
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "whp",
                    "--scenario",
                    "l3-l4-egress-protocols",
                    "--output-dir",
                    temporary,
                ]
            )
            self.assertEqual(microvm_tests.run(args), 0)

            run_l3_l4_egress_protocols.assert_called_once_with(
                microvm_tests.openvmm_binary_path(),
                microvm_tests.artifact_path(
                    microvm_tests.KernelBuildConstants.BINARY_NAME
                ),
                microvm_tests.artifact_path(
                    microvm_tests.guest_descriptor("alpine").initramfs_name
                ),
                "whp",
                memory_mib=128,
                timeout=60.0,
                output_dir=Path(temporary),
                guest="alpine",
            )
        self.assertIn("l3-l4-egress-protocols", microvm_tests.MICROVM_TEST_SCENARIOS)

    def test_egress_port_range_cases_cover_each_range_as_allow_and_deny(self):
        cases = {
            case.name: case
            for case in microvm_tests._egress_port_range_cases(
                "192.0.2.1", 20001, 20002, 20003
            )
        }
        every_probe = frozenset(microvm_tests.L3_L4_EGRESS_RANGE_PROBES)
        self.assertEqual(
            microvm_tests.L3_L4_EGRESS_RANGE_PROBES,
            tuple(
                f"{protocol}-{position}"
                for protocol in ("tcp", "udp")
                for position in ("below", "first", "inside", "last", "above")
            ),
        )

        def in_range(protocol: str) -> set[str]:
            return {
                f"{protocol}-{position}" for position in ("first", "inside", "last")
            }

        expected = {
            "tcp": in_range("tcp"),
            "udp": in_range("udp"),
            "any": in_range("tcp") | in_range("udp"),
        }
        self.assertEqual(
            set(cases),
            {f"{action}-{name}" for action in ("allow", "deny") for name in expected}
            | {"allow-tcp-deny-inside"},
        )
        destination = [{"cidr": "192.0.2.1/32"}]
        for name, matched in expected.items():
            rule = {
                "to": destination,
                "ports": [{"protocol": name, "port": 20001, "endPort": 20003}],
            }
            with self.subTest(protocol=name):
                allow = cases[f"allow-{name}"]
                self.assertEqual((allow.allow, allow.deny), ((rule,), ()))
                self.assertEqual(allow.reached, matched)
                deny = cases[f"deny-{name}"]
                self.assertEqual(
                    (deny.allow, deny.deny), (({"to": destination},), (rule,))
                )
                self.assertEqual(deny.reached, every_probe - matched)
        inside = cases["allow-tcp-deny-inside"]
        self.assertEqual(
            inside.deny,
            ({"to": destination, "ports": [{"protocol": "tcp", "port": 20002}]},),
        )
        self.assertEqual(inside.reached, {"tcp-first", "tcp-last"})

    def test_egress_port_range_cases_lower_each_range_to_one_native_rule(self):
        tcp = "192.0.2.1/32:tcp:20001-20003"
        udp = "192.0.2.1/32:udp:20001-20003"
        expected = {
            "allow-tcp": ((tcp,), ()),
            "deny-tcp": (("192.0.2.1/32",), (tcp,)),
            "allow-udp": ((udp,), ()),
            "deny-udp": (("192.0.2.1/32",), (udp,)),
            "allow-any": ((tcp, udp), ()),
            "deny-any": (("192.0.2.1/32",), (tcp, udp)),
            "allow-tcp-deny-inside": ((tcp,), ("192.0.2.1/32:tcp:20002",)),
        }
        for case in microvm_tests._egress_port_range_cases(
            "192.0.2.1", 20001, 20002, 20003
        ):
            with self.subTest(case=case.name):
                compiled = _compile(
                    {"allow": list(case.allow), "deny": list(case.deny)}
                )
                self.assertEqual((compiled.allow, compiled.deny), expected[case.name])

    def _run_egress_port_ranges(
        self,
        output_dir: Path,
        *,
        leaked: tuple[str, str] | None = None,
        guest_report: tuple[str, str, str] | None = None,
        openvmm_accepts: str | None = None,
        nvx_accepts: str | None = None,
    ) -> tuple[MagicMock, MagicMock, MagicMock]:
        tcp = [MagicMock(spec=socket.socket) for _ in range(5)]
        udp = [MagicMock(spec=socket.socket) for _ in range(5)]
        for index, endpoint in enumerate((*tcp, *udp)):
            endpoint.getsockname.return_value = ("0.0.0.0", 20000 + index % 5)
        cases = {
            case.name: case
            for case in microvm_tests._egress_port_range_cases(
                microvm_tests.DIRECTIONAL_NETWORK_GATEWAY_IPV4, 20001, 20002, 20003
            )
        }
        current: list[str] = []

        def policy_name(command: list[str]) -> str:
            policy = Path(command[command.index("--network-egress-policy-file") + 1])
            return policy.name.removeprefix("l3-l4-egress-range-").removesuffix(
                "-policy.json"
            )

        def guest(
            command: list[str], _script: str, _marker: bytes, **_kwargs: object
        ) -> microvm_tests.GuestScriptResult:
            name = policy_name(command)
            current.append(name)
            reports = {
                probe: "sent"
                if probe.startswith("udp")
                else "reached"
                if probe in cases[name].reached
                else "blocked"
                for probe in microvm_tests.L3_L4_EGRESS_RANGE_PROBES
            }
            if guest_report is not None and guest_report[0] == name:
                reports[guest_report[1]] = guest_report[2]
            text = "".join(
                f"NVX-L3-L4-PROBE {probe}={value}\r\n"
                for probe, value in reports.items()
            )
            return _guest_script_result(
                text + "NVX-L3-L4-EGRESS-PORT-RANGES-OK\r\n",
                # The tty echoes script lines, which must not count as reports.
                transcript='> echo "NVX-L3-L4-PROBE tcp-first=echoed"\r\n',
            )

        class FakeServer:
            def __init__(
                self, endpoints: dict[str, socket.socket], _timeout: float
            ) -> None:
                assert list(endpoints.values()) == [*tcp, *udp]
                assert list(endpoints) == list(microvm_tests.L3_L4_EGRESS_RANGE_PROBES)

            def __enter__(self) -> "FakeServer":
                return self

            def __exit__(self, *_: object) -> None:
                pass

            def finish(self) -> frozenset[str]:
                name = current[-1]
                reached = set(cases[name].reached)
                if leaked is not None and leaked[0] == name:
                    reached ^= {leaked[1]}
                return frozenset(reached)

        def openvmm_result(name: str, output: bytes) -> MagicMock:
            if openvmm_accepts == name:
                return MagicMock(returncode=0, output=microvm_tests.BOOT_MARKER)
            return MagicMock(returncode=2, output=output)

        run_process = subprocess.run

        def public_dry_run(command: list[str], **kwargs: Any) -> object:
            if "--dry-run" not in command:
                # nvx-rs, which compiles each policy.
                return cast(object, run_process(command, **kwargs))
            if nvx_accepts == policy_name(command):
                return MagicMock(returncode=0, stderr=b"")
            path = Path(command[command.index("--network-egress-policy-file") + 1])
            try:
                nvx_rs.compile_egress_policy(path)
            except nvx_rs.NvxRsError as error:
                return MagicMock(
                    returncode=1, stderr=f"error[{error.code}]: {error}\n".encode()
                )
            return MagicMock(returncode=0, stderr=b"")

        with (
            patch.object(
                microvm_tests, "_bind_dual_protocol_ports", return_value=(tcp, udp)
            ),
            patch.object(microvm_tests, "_EgressProbeServer", FakeServer),
            patch.object(
                microvm_tests, "run_guest_script", side_effect=guest
            ) as run_guest_script,
            patch.object(microvm_tests, "OpenvmmProcess") as openvmm_process,
            patch.object(
                microvm_tests.subprocess, "run", side_effect=public_dry_run
            ) as nvx_run,
        ):
            wait = openvmm_process.return_value.__enter__.return_value.wait
            wait.side_effect = (
                openvmm_result(
                    "reversed-range",
                    b"egress port range 20003-20001 ends below its first port",
                ),
                openvmm_result(
                    "missing-first-port",
                    b"invalid egress port range '-20003'; expected <FIRST>-<LAST>",
                ),
            )
            try:
                microvm_tests.run_l3_l4_egress_port_ranges(
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initramfs"),
                    "whp",
                    memory_mib=128,
                    timeout=1,
                    output_dir=output_dir,
                )
            finally:
                for endpoint in (*tcp, *udp):
                    endpoint.close.assert_called_once_with()
        return run_guest_script, openvmm_process, nvx_run

    def test_l3_l4_egress_port_ranges_run_every_case_through_public_nvx_run(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            run_guest_script, openvmm_process, nvx_run = self._run_egress_port_ranges(
                output_dir
            )

            names = [
                case.name
                for case in microvm_tests._egress_port_range_cases(
                    microvm_tests.DIRECTIONAL_NETWORK_GATEWAY_IPV4, 20001, 20002, 20003
                )
            ]
            self.assertEqual(run_guest_script.call_count, len(names))
            public_run = [
                *nvx_rs.command_line(("run",)),
                "--guest",
                "alpine",
                "--hypervisor",
                "whp",
                "--memory-mib",
                "128",
                "--net",
                microvm_tests.DIRECTIONAL_NETWORK_CIDR,
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-ingress",
                "deny",
            ]
            for call, name in zip(run_guest_script.call_args_list, names, strict=True):
                policy_path = output_dir / f"l3-l4-egress-range-{name}-policy.json"
                self.assertEqual(
                    call.args[0],
                    [
                        *public_run,
                        "--network-egress-policy-file",
                        str(policy_path),
                        "--cmdline",
                        "quiet loglevel=0",
                    ],
                )
                self.assertNotIn("@", call.args[1])
                for port in range(20000, 20005):
                    self.assertIn(f" {port} &\n", call.args[1])
                self.assertEqual(
                    call.args[2],
                    microvm_tests.L3_L4_EGRESS_PORT_RANGES_COMPLETION_MARKER,
                )
                self.assertIs(call.kwargs["contain_process_tree"], True)

            requested = json.loads(
                (
                    output_dir / "l3-l4-egress-range-allow-tcp-deny-inside-policy.json"
                ).read_text(encoding="utf-8")
            )
            destination = [{"cidr": "192.0.2.1/32"}]
            self.assertEqual(
                requested,
                {
                    "allow": [
                        {
                            "to": destination,
                            "ports": [
                                {"protocol": "tcp", "port": 20001, "endPort": 20003}
                            ],
                        }
                    ],
                    "deny": [
                        {
                            "to": destination,
                            "ports": [{"protocol": "tcp", "port": 20002}],
                        }
                    ],
                },
            )
            rejected = [call.args[0][-1] for call in openvmm_process.call_args_list]
            self.assertEqual(
                rejected, ["192.0.2.1:tcp:20003-20001", "192.0.2.1:udp:-20003"]
            )
            # The other calls are nvx-rs, which compiles each case's policy.
            dry_runs = [
                call.args[0]
                for call in nvx_run.call_args_list
                if "--dry-run" in call.args[0]
            ]
            self.assertEqual(len(dry_runs), 2)
            for command, name in zip(
                dry_runs, ("reversed-end-port", "end-port-without-port"), strict=True
            ):
                self.assertEqual(
                    command,
                    [
                        *public_run,
                        "--network-egress-policy-file",
                        str(output_dir / f"l3-l4-egress-range-{name}-policy.json"),
                        "--dry-run",
                    ],
                )

            results = json.loads(
                (output_dir / "l3-l4-egress-port-ranges-results.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(
                results["ports"],
                {
                    "below": 20000,
                    "first": 20001,
                    "inside": 20002,
                    "last": 20003,
                    "above": 20004,
                },
            )
            by_case = {case["case"]: case for case in results["cases"]}
            self.assertEqual(list(by_case), names)
            self.assertEqual(
                by_case["allow-any"]["native_rules"],
                {
                    "allow": [
                        "192.0.2.1/32:tcp:20001-20003",
                        "192.0.2.1/32:udp:20001-20003",
                    ],
                    "deny": [],
                },
            )
            self.assertEqual(
                by_case["allow-tcp-deny-inside"]["host_reached"],
                ["tcp-first", "tcp-last"],
            )
            self.assertEqual(
                by_case["deny-udp"]["host_reached"],
                [
                    "tcp-above",
                    "tcp-below",
                    "tcp-first",
                    "tcp-inside",
                    "tcp-last",
                    "udp-above",
                    "udp-below",
                ],
            )
            self.assertEqual(by_case["allow-udp"]["guest"]["udp-below"], "sent")

    def test_l3_l4_egress_port_ranges_reject_leaks_and_missed_traffic(self):
        failures: tuple[
            tuple[tuple[str, str] | None, tuple[str, str, str] | None, str], ...
        ] = (
            (
                ("allow-tcp", "tcp-above"),
                None,
                "allow-tcp: tcp-above should be blocked",
            ),
            (
                ("allow-tcp", "tcp-below"),
                None,
                "allow-tcp: tcp-below should be blocked",
            ),
            (("allow-udp", "udp-last"), None, "allow-udp: udp-last should be reached"),
            (
                ("allow-udp", "tcp-first"),
                None,
                "allow-udp: tcp-first should be blocked",
            ),
            (
                ("allow-tcp-deny-inside", "tcp-inside"),
                None,
                "allow-tcp-deny-inside: tcp-inside should be blocked",
            ),
            (("deny-any", "udp-first"), None, "deny-any: udp-first should be blocked"),
            (
                None,
                ("allow-tcp", "tcp-first", "blocked"),
                "allow-tcp: tcp-first should be reached",
            ),
            (
                None,
                ("deny-tcp", "tcp-last", "reached"),
                "deny-tcp: tcp-last should be blocked",
            ),
            (None, ("allow-any", "udp-inside", "failed-2"), "allow-any: udp-inside"),
        )
        for leaked, guest_report, message in failures:
            with (
                self.subTest(message=message),
                tempfile.TemporaryDirectory() as temporary,
            ):
                output_dir = Path(temporary)
                with self.assertRaisesRegex(
                    RuntimeError, "port-range case " + re.escape(message)
                ):
                    self._run_egress_port_ranges(
                        output_dir, leaked=leaked, guest_report=guest_report
                    )
                self.assertFalse(
                    (output_dir / "l3-l4-egress-port-ranges-results.json").exists()
                )

    def test_l3_l4_egress_port_ranges_require_rejection_before_launch(self):
        # Each case lets OpenVMM, or the public run, accept one invalid range.
        for openvmm_accepts, nvx_accepts, message in (
            (
                "reversed-range",
                None,
                "port range reversed-range was not rejected before boot",
            ),
            (
                "missing-first-port",
                None,
                "port range missing-first-port was not rejected before boot",
            ),
            (
                None,
                "reversed-end-port",
                "policy reversed-end-port was not rejected before launch",
            ),
            (
                None,
                "end-port-without-port",
                "policy end-port-without-port was not rejected before launch",
            ),
        ):
            with (
                self.subTest(message=message),
                tempfile.TemporaryDirectory() as temporary,
            ):
                output_dir = Path(temporary)
                with self.assertRaisesRegex(RuntimeError, re.escape(message)):
                    self._run_egress_port_ranges(
                        output_dir,
                        openvmm_accepts=openvmm_accepts,
                        nvx_accepts=nvx_accepts,
                    )
                self.assertFalse(
                    (output_dir / "l3-l4-egress-port-ranges-results.json").exists()
                )

    def test_l3_l4_egress_port_range_script_reports_each_probe(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")

        script = microvm_tests._render_script(
            "l3-l4-egress-port-ranges.sh.in",
            GATEWAY_IPV4="192.0.2.1",
            BELOW_PORT="20000",
            FIRST_PORT="20001",
            INSIDE_PORT="20002",
            LAST_PORT="20003",
            ABOVE_PORT="20004",
        ).replace("nvx-exit", "record_exit")
        stubs = (
            "record_exit() { printf 'NVX-EXIT %s\\n' \"$1\"; }\n"
            "wget() {\n"
            "    for url; do :; done\n"
            '    case "$url" in\n'
            "    *:20001/tcp-first) printf NVX-EGRESS-PROBE-tcp-first ;;\n"
            "    *:20002/tcp-inside) printf WRONG ;;\n"
            "    *) return 4 ;;\n"
            "    esac\n"
            "}\n"
            "nc() {\n"
            "    cat >/dev/null\n"
            "    for port; do :; done\n"
            '    case "$port" in\n'
            "    20000) return 1 ;;\n"
            "    20004) return 2 ;;\n"
            "    esac\n"
            "}\n"
        )
        result, probes = self._run_egress_script(shell, stubs + script)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.splitlines(),
            [
                "NVX-L3-L4-PROBE tcp-below=blocked",
                "NVX-L3-L4-PROBE tcp-first=reached",
                "NVX-L3-L4-PROBE tcp-inside=unexpected",
                "NVX-L3-L4-PROBE tcp-last=blocked",
                "NVX-L3-L4-PROBE tcp-above=blocked",
                "NVX-L3-L4-PROBE udp-below=sent",
                "NVX-L3-L4-PROBE udp-first=sent",
                "NVX-L3-L4-PROBE udp-inside=sent",
                "NVX-L3-L4-PROBE udp-last=sent",
                "NVX-L3-L4-PROBE udp-above=failed-2",
                "NVX-L3-L4-EGRESS-PORT-RANGES-OK",
                "NVX-EXIT 0",
            ],
        )
        # The run replaced the stale result in its TMPDIR with its probes'.
        self.assertEqual(
            probes,
            [
                "nvx-egress-tcp-above",
                "nvx-egress-tcp-below",
                "nvx-egress-tcp-first",
                "nvx-egress-tcp-inside",
                "nvx-egress-tcp-last",
                "nvx-egress-udp-above",
                "nvx-egress-udp-below",
                "nvx-egress-udp-first",
                "nvx-egress-udp-inside",
                "nvx-egress-udp-last",
            ],
        )

    def test_runner_dispatches_l3_l4_egress_port_ranges(self):
        def require(path: Path, _description: str) -> Path:
            return path

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(microvm_tests, "validate_openvmm_test_backend"),
            patch.object(microvm_tests, "require_file", side_effect=require),
            patch.object(
                microvm_tests, "run_l3_l4_egress_port_ranges"
            ) as run_l3_l4_egress_port_ranges,
        ):
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "kvm",
                    "--scenario",
                    "l3-l4-egress-port-ranges",
                    "--output-dir",
                    temporary,
                ]
            )
            self.assertEqual(microvm_tests.run(args), 0)

            run_l3_l4_egress_port_ranges.assert_called_once_with(
                microvm_tests.openvmm_binary_path(),
                microvm_tests.artifact_path(
                    microvm_tests.KernelBuildConstants.BINARY_NAME
                ),
                microvm_tests.artifact_path(
                    microvm_tests.guest_descriptor("alpine").initramfs_name
                ),
                "kvm",
                memory_mib=128,
                timeout=60.0,
                output_dir=Path(temporary),
                guest="alpine",
            )
        self.assertIn("l3-l4-egress-port-ranges", microvm_tests.MICROVM_TEST_SCENARIOS)

    def test_ipv6_egress_identity_embeds_the_directional_network(self):
        prefix = int(ipaddress.IPv6Address("fd00::"))
        for ipv4, ipv6 in (
            (
                microvm_tests.DIRECTIONAL_NETWORK_GUEST_IPV4,
                microvm_tests.DIRECTIONAL_NETWORK_GUEST_IPV6,
            ),
            (
                microvm_tests.DIRECTIONAL_NETWORK_GATEWAY_IPV4,
                microvm_tests.DIRECTIONAL_NETWORK_GATEWAY_IPV6,
            ),
        ):
            self.assertEqual(
                ipaddress.IPv6Address(prefix | int(ipaddress.IPv4Address(ipv4))),
                ipaddress.IPv6Address(ipv6),
            )
        ipv4_prefix_length = int(microvm_tests.DIRECTIONAL_NETWORK_CIDR.split("/")[1])
        self.assertEqual(
            microvm_tests.DIRECTIONAL_NETWORK_IPV6_PREFIX_LENGTH,
            96 + ipv4_prefix_length,
        )
        self.assertEqual(
            microvm_tests._ipv6_identity_values(),
            {
                "GUEST_IPV6": "fd00::c000:202",
                "GATEWAY_IPV6": "fd00::c000:201",
                "PREFIX_LENGTH": "120",
                "GUEST_IPV6_HEX": "fd0000000000000000000000c0000202",
                "GATEWAY_IPV6_HEX": "fd0000000000000000000000c0000201",
                "PREFIX_LENGTH_HEX": "78",
                "ANY_IPV6_HEX": "0" * 32,
            },
        )

    def test_ipv6_egress_cases_cover_families_and_precedence(self):
        cases = microvm_tests._ipv6_egress_cases(
            "192.0.2.1", "fd00::c000:201", 20000, 20001
        )
        gateway = "fd00::c000:201/128"
        named_tcp = f"{gateway}:tcp:20000"
        every_probe = set(microvm_tests.IPV6_EGRESS_PROBES)
        lowered = {}
        for case in cases:
            compiled = _compile({"allow": list(case.allow), "deny": list(case.deny)})
            lowered[case.name] = (
                case.default,
                compiled.allow,
                compiled.deny,
                set(case.reached),
                case.dns,
            )
        # The guest names the IPv4 gateway as its DNS server when the policy
        # allows DNS to it, and otherwise the IPv6 gateway when it allows DNS
        # to that one.
        self.assertEqual(
            lowered,
            {
                "allow-all": ("allow", (), (), every_probe, "192.0.2.1"),
                "deny-all": ("deny", (), (), set(), None),
                "allow-ipv6-tcp-port": (
                    "deny",
                    (named_tcp,),
                    (),
                    {"tcp6-named"},
                    None,
                ),
                # The range lowers to one native range rule.
                "allow-ipv6-tcp-range": (
                    "deny",
                    (f"{gateway}:tcp:20000-20001",),
                    (),
                    {"tcp6-named", "tcp6-other"},
                    None,
                ),
                "allow-ipv6-udp-icmp": (
                    "deny",
                    (f"{gateway}:icmp", f"{gateway}:udp"),
                    (),
                    {"udp6-named", "icmp6"},
                    "fd00::c000:201",
                ),
                "deny-ipv6-tcp-port": (
                    "deny",
                    ("::/0",),
                    (named_tcp,),
                    {"tcp6-other", "udp6-named", "icmp6"},
                    "fd00::c000:201",
                ),
                "allow-tcp-port-without-to": (
                    "deny",
                    ("0.0.0.0/0:tcp:20000", "::/0:tcp:20000"),
                    (),
                    {"tcp6-named", "tcp4-named"},
                    None,
                ),
                "allow-ipv4-only": (
                    "deny",
                    ("0.0.0.0/0",),
                    (),
                    {"tcp4-named"},
                    "192.0.2.1",
                ),
                "deny-ipv6-only": (
                    "allow",
                    (),
                    ("fd00::/8",),
                    {"tcp4-named"},
                    "192.0.2.1",
                ),
            },
        )

    def _run_ipv6_egress(
        self,
        output_dir: Path,
        *,
        leaked: tuple[str, str] | None = None,
        guest_report: tuple[str, str, str] | None = None,
        dns_report: tuple[str, str] | None = None,
        changed_restore: bytes = b"restore-time egress policy does not match",
        snapshot_reached: frozenset[str] = frozenset(("tcp6-named",)),
    ) -> tuple[MagicMock, MagicMock]:
        endpoints = {
            name: MagicMock(spec=socket.socket)
            for name in ("tcp6-named", "tcp6-other", "udp6-named", "tcp4-named")
        }
        for name, endpoint in endpoints.items():
            endpoint.getsockname.return_value = (
                "::1",
                20001 if name == "tcp6-other" else 20000,
            )
        cases = {
            case.name: case
            for case in microvm_tests._ipv6_egress_cases(
                microvm_tests.DIRECTIONAL_NETWORK_GATEWAY_IPV4,
                microvm_tests.DIRECTIONAL_NETWORK_GATEWAY_IPV6,
                20000,
                20001,
            )
        }
        current: list[str] = []

        def guest(
            command: list[str], _script: str, _marker: bytes, **_kwargs: object
        ) -> microvm_tests.GuestScriptResult:
            policy = Path(command[command.index("--network-egress-policy-file") + 1])
            name = policy.name.removeprefix("ipv6-egress-").removesuffix("-policy.json")
            current.append(name)
            reports = {
                probe: "sent"
                if probe.startswith("udp")
                else "reached"
                if probe in cases[name].reached
                else "blocked"
                for probe in microvm_tests.IPV6_EGRESS_PROBES
            }
            if guest_report is not None and guest_report[0] == name:
                reports[guest_report[1]] = guest_report[2]
            dns = cases[name].dns or "none"
            if dns_report is not None and dns_report[0] == name:
                dns = dns_report[1]
            text = f"NVX-IPV6-EGRESS-DNS {dns}\r\n"
            text += "".join(
                f"NVX-L3-L4-PROBE {probe}={value}\r\n"
                for probe, value in reports.items()
            )
            return _guest_script_result(
                text + "NVX-IPV6-EGRESS-OK\r\n",
                # The tty echoes script lines, which must not count as reports.
                transcript='> echo "NVX-IPV6-EGRESS-DNS ${dns:-none}"\r\n',
            )

        class FakeServer:
            def __init__(
                self, served: dict[str, socket.socket], _timeout: float
            ) -> None:
                self.snapshot = list(served) == ["tcp6-named"]
                if self.snapshot:
                    assert served["tcp6-named"] is endpoints["tcp6-named"]
                else:
                    assert served == endpoints

            def __enter__(self) -> "FakeServer":
                return self

            def __exit__(self, *_: object) -> None:
                pass

            def finish(self) -> frozenset[str]:
                if self.snapshot:
                    return snapshot_reached
                name = current[-1]
                reached = set(cases[name].reached - {"icmp6"})
                if leaked is not None and leaked[0] == name:
                    reached.add(leaked[1])
                return frozenset(reached)

        with (
            patch.object(
                microvm_tests, "_bind_ipv6_egress_ports", return_value=endpoints
            ),
            patch.object(microvm_tests, "_EgressProbeServer", FakeServer),
            patch.object(
                microvm_tests, "run_guest_script", side_effect=guest
            ) as run_guest_script,
            patch.object(microvm_tests, "OpenvmmProcess") as openvmm_process,
        ):
            wait = openvmm_process.return_value.__enter__.return_value.wait
            wait.side_effect = (
                MagicMock(returncode=0, output=b"NVX-IPV6-EGRESS-BEFORE\r\n"),
                MagicMock(returncode=1, output=changed_restore),
                MagicMock(returncode=0, output=b"NVX-IPV6-EGRESS-AFTER\r\n"),
                MagicMock(
                    returncode=2,
                    output=b"IPv6 prefix /129 is outside the supported range",
                ),
            )
            try:
                microvm_tests.run_ipv6_egress_policy(
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initramfs"),
                    "whp",
                    memory_mib=128,
                    timeout=1,
                    output_dir=output_dir,
                )
            finally:
                for endpoint in endpoints.values():
                    endpoint.close.assert_called_once_with()
        return run_guest_script, openvmm_process

    def test_ipv6_egress_policy_runs_every_case_through_public_nvx_run(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            run_guest_script, openvmm_process = self._run_ipv6_egress(output_dir)

            cases = microvm_tests._ipv6_egress_cases(
                microvm_tests.DIRECTIONAL_NETWORK_GATEWAY_IPV4,
                microvm_tests.DIRECTIONAL_NETWORK_GATEWAY_IPV6,
                20000,
                20001,
            )
            self.assertEqual(run_guest_script.call_count, len(cases))
            public_run = nvx_rs.command_line(("run",))
            for call, case in zip(run_guest_script.call_args_list, cases, strict=True):
                policy_path = output_dir / f"ipv6-egress-{case.name}-policy.json"
                self.assertEqual(
                    call.args[0],
                    [
                        *public_run,
                        "--guest",
                        "alpine",
                        "--hypervisor",
                        "whp",
                        "--memory-mib",
                        "128",
                        "--net",
                        microvm_tests.DIRECTIONAL_NETWORK_CIDR,
                        "--network-profile",
                        "portable",
                        "--network-egress",
                        case.default,
                        "--network-ingress",
                        "deny",
                        "--network-egress-policy-file",
                        str(policy_path),
                        "--cmdline",
                        "quiet loglevel=0",
                    ],
                )
                self.assertEqual(
                    json.loads(policy_path.read_text(encoding="utf-8")),
                    {"allow": list(case.allow), "deny": list(case.deny)},
                )
                script = call.args[1]
                self.assertNotIn("@", script)
                self.assertIn("'http://[fd00::c000:201]:20000/tcp6-named' &\n", script)
                self.assertIn("'http://[fd00::c000:201]:20001/tcp6-other' &\n", script)
                self.assertIn("'http://192.0.2.1:20000/tcp4-named' &\n", script)
                self.assertEqual(
                    call.args[2], microvm_tests.IPV6_EGRESS_COMPLETION_MARKER
                )
                self.assertIs(call.kwargs["contain_process_tree"], True)

            launched = [call.args[0] for call in openvmm_process.call_args_list]
            self.assertEqual(len(launched), 4)
            capture, changed, restore, invalid = launched
            ipv6_rule = "fd00::c000:201/128:tcp:20000"
            self.assertEqual(
                capture[capture.index("--snapshot-destination") + 2 :],
                ["--network-egress", "deny", "--network-egress-allow", ipv6_rule],
            )
            self.assertEqual(
                capture[capture.index("--net") + 1],
                microvm_tests.DIRECTIONAL_NETWORK_CIDR,
            )
            for restored, rule in (
                (changed, "192.0.2.1/32:tcp:20000"),
                (restore, ipv6_rule),
            ):
                self.assertIn("--restore-snapshot", restored)
                self.assertNotIn("--net", restored)
                self.assertEqual(
                    restored[restored.index("--network-profile") :],
                    [
                        "--network-profile",
                        "portable",
                        "--network-egress",
                        "deny",
                        "--network-egress-allow",
                        rule,
                    ],
                )
            self.assertEqual(
                invalid[-4:],
                [
                    "--network-egress",
                    "deny",
                    "--network-egress-allow",
                    "fd00::c000:201/129",
                ],
            )
            process = openvmm_process.return_value.__enter__.return_value
            process.wait_for.assert_any_call(microvm_tests.BOOT_MARKER, 1)
            process.wait_for.assert_any_call(microvm_tests.IPV6_EGRESS_AFTER_MARKER, 1)
            staged = process.send_bytes.call_args.args[0].decode()
            self.assertIn("http://[fd00::c000:201]:20000/tcp6-named", staged)
            self.assertIn("nvx-snapshot\n", staged)

            results = json.loads(
                (output_dir / "ipv6-egress-policy-results.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(results["guest_ipv6"], "fd00::c000:202/120")
            self.assertEqual(results["gateway_ipv6"], "fd00::c000:201")
            self.assertEqual(results["ports"], {"named": 20000, "other": 20001})
            by_case = {case["case"]: case for case in results["cases"]}
            self.assertEqual(list(by_case), [case.name for case in cases])
            self.assertEqual(
                by_case["deny-ipv6-tcp-port"]["host_reached"],
                ["tcp6-other", "udp6-named"],
            )
            self.assertEqual(by_case["deny-ipv6-tcp-port"]["guest"]["icmp6"], "reached")
            self.assertEqual(by_case["allow-ipv4-only"]["host_reached"], ["tcp4-named"])
            self.assertEqual(by_case["deny-all"]["host_reached"], [])
            tcp_range = by_case["allow-ipv6-tcp-range"]
            self.assertEqual(tcp_range["host_reached"], ["tcp6-named", "tcp6-other"])
            self.assertEqual(
                tcp_range["native_rules"],
                {"allow": ["fd00::c000:201/128:tcp:20000-20001"], "deny": []},
            )
            # A rule without destinations reaches both families.
            without_to = by_case["allow-tcp-port-without-to"]
            self.assertEqual(without_to["host_reached"], ["tcp4-named", "tcp6-named"])
            self.assertEqual(
                without_to["native_rules"],
                {"allow": ["0.0.0.0/0:tcp:20000", "::/0:tcp:20000"], "deny": []},
            )
            self.assertEqual(by_case["deny-ipv6-only"]["default"], "allow")
            self.assertEqual(
                {name: case["dns"] for name, case in by_case.items()},
                {
                    "allow-all": "192.0.2.1",
                    "deny-all": "none",
                    "allow-ipv6-tcp-port": "none",
                    "allow-ipv6-tcp-range": "none",
                    "allow-ipv6-udp-icmp": "fd00::c000:201",
                    "deny-ipv6-tcp-port": "fd00::c000:201",
                    "allow-tcp-port-without-to": "none",
                    "allow-ipv4-only": "192.0.2.1",
                    "deny-ipv6-only": "192.0.2.1",
                },
            )

    def test_ipv6_egress_policy_requires_the_expected_dns_server(self):
        for name, reported, expected in (
            ("allow-ipv6-udp-icmp", "none", "fd00::c000:201"),
            ("allow-ipv6-udp-icmp", "192.0.2.1", "fd00::c000:201"),
            ("allow-ipv6-tcp-port", "fd00::c000:201", "none"),
            ("allow-all", "fd00::c000:201", "192.0.2.1"),
        ):
            with (
                self.subTest(name=name, reported=reported),
                tempfile.TemporaryDirectory() as temporary,
            ):
                with self.assertRaisesRegex(
                    RuntimeError,
                    re.escape(
                        f"{name}: the guest named {reported!r} as its DNS server "
                        f"instead of {expected!r}"
                    ),
                ):
                    self._run_ipv6_egress(Path(temporary), dns_report=(name, reported))

    def test_ipv6_egress_policy_rejects_leaks_and_missed_traffic(self):
        failures: tuple[
            tuple[tuple[str, str] | None, tuple[str, str, str] | None, str], ...
        ] = (
            (
                ("allow-ipv6-tcp-port", "tcp6-other"),
                None,
                "L3/L4 IPv6 egress case allow-ipv6-tcp-port: tcp6-other",
            ),
            (("deny-all", "tcp6-named"), None, "deny-all: tcp6-named"),
            (
                ("allow-ipv6-tcp-range", "udp6-named"),
                None,
                "allow-ipv6-tcp-range: udp6-named",
            ),
            (
                None,
                ("allow-ipv6-tcp-range", "tcp6-other", "blocked"),
                "allow-ipv6-tcp-range: tcp6-other should be reached",
            ),
            (("allow-ipv4-only", "udp6-named"), None, "allow-ipv4-only: udp6-named"),
            (("deny-ipv6-only", "tcp6-named"), None, "deny-ipv6-only: tcp6-named"),
            (
                None,
                ("allow-all", "icmp6", "blocked"),
                "allow-all: icmp6 should be reached",
            ),
            (
                None,
                ("allow-ipv4-only", "tcp6-named", "reached"),
                "allow-ipv4-only: tcp6-named should be blocked",
            ),
            (
                None,
                ("allow-tcp-port-without-to", "tcp4-named", "blocked"),
                "allow-tcp-port-without-to: tcp4-named should be reached",
            ),
            (
                None,
                ("deny-ipv6-tcp-port", "tcp4-named", "unexpected"),
                "deny-ipv6-tcp-port: tcp4-named",
            ),
        )
        for leaked, guest_report, message in failures:
            with (
                self.subTest(message=message),
                tempfile.TemporaryDirectory() as temporary,
            ):
                output_dir = Path(temporary)
                with self.assertRaisesRegex(RuntimeError, re.escape(message)):
                    self._run_ipv6_egress(
                        output_dir, leaked=leaked, guest_report=guest_report
                    )
                self.assertFalse(
                    (output_dir / "ipv6-egress-policy-results.json").exists()
                )

    def test_ipv6_egress_policy_requires_the_restored_policy_and_flow(self):
        with (
            tempfile.TemporaryDirectory() as temporary,
            self.assertRaisesRegex(
                RuntimeError, "restore with a changed rule was not rejected"
            ),
        ):
            self._run_ipv6_egress(Path(temporary), changed_restore=b"restored")
        with (
            tempfile.TemporaryDirectory() as temporary,
            self.assertRaisesRegex(
                RuntimeError, "snapshot probes did not reach the host"
            ),
        ):
            self._run_ipv6_egress(Path(temporary), snapshot_reached=frozenset())

    def _run_ipv6_guest_script(
        self,
        shell: str,
        root: Path,
        name: str,
        stubs: str,
        **values: str,
    ) -> subprocess.CompletedProcess[str]:
        script = (
            microvm_tests._render_script(name, **values)
            .replace("nvx-exit", "record_exit")
            .replace("nvx-snapshot", "record_snapshot")
            .replace("/proc/", f"{root.as_posix()}/proc/")
            .replace("/etc/resolv.conf", f"{root.as_posix()}/etc/resolv.conf")
        )
        # The root's tmp stands for the guest's /tmp, where the probes keep
        # their results.
        (root / "tmp").mkdir(exist_ok=True)
        return subprocess.run(
            [shell],
            input="record_exit() { printf 'NVX-EXIT %s\\n' \"$1\"; }\n"
            + stubs
            + script,
            text=True,
            capture_output=True,
            timeout=10,
            check=False,
            env=_probe_environment(root / "tmp"),
        )

    def test_ipv6_egress_script_checks_the_identity_and_reports_each_probe(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        values = {
            **microvm_tests._ipv6_identity_values(),
            "GATEWAY_IPV4": "192.0.2.1",
            "NAMED_PORT": "20000",
            "OTHER_PORT": "20001",
        }
        any_ipv6 = "0" * 32
        route = (
            f"{any_ipv6} 00 {any_ipv6} 00 fd0000000000000000000000c0000201 "
            "00000400 00000001 00000000 00000003     eth0\n"
        )
        address = "fd0000000000000000000000c0000202 02 78 00 80     eth0\n"
        stubs = (
            "wget() {\n"
            "    for url; do :; done\n"
            '    case "$url" in\n'
            "    *]:20000/tcp6-named) printf NVX-EGRESS-PROBE-tcp6-named ;;\n"
            "    */tcp6-other) return 1 ;;\n"
            "    */tcp4-named) printf WRONG ;;\n"
            "    esac\n"
            "}\n"
            "nc() {\n"
            "    cat >/dev/null\n"
            '    [ "$4" = fd00::c000:201 ] && [ "$5" = 20000 ] || return 2\n'
            "}\n"
            'ping() { [ "$5" = fd00::c000:201 ]; }\n'
        )
        ipv6_dns = " virtnet_dns=fd00::c000:201"
        resolver = "nameserver fd00::c000:201\n"
        # Each case: kernel tables, DNS token and resolver, and the exit status
        # with the reported DNS server.
        cases: tuple[tuple[str, str, str, str, str, int, str], ...] = (
            ("complete", address, route, ipv6_dns, resolver, 0, "fd00::c000:201"),
            ("no dns", address, route, "", "", 0, "none"),
            (
                "stale resolver",
                address,
                route,
                ipv6_dns,
                "nameserver 192.0.2.1\n",
                64,
                "",
            ),
            ("no address", "", route, "", "", 62, ""),
            ("other prefix", address.replace(" 78 ", " 40 "), route, "", "", 62, ""),
            ("no route", address, "", "", "", 63, ""),
        )
        for label, if_inet6, ipv6_route, dns, resolv, status, reported in cases:
            with self.subTest(label), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / "proc" / "net").mkdir(parents=True)
                (root / "etc").mkdir()
                (root / "tmp").mkdir()
                (root / "tmp" / "nvx-egress-stale").write_text(
                    "stale\n", encoding="utf-8"
                )
                (root / "proc" / "cmdline").write_text(
                    "quiet virtnet_ip=192.0.2.2 virtnet_mask=255.255.255.0 "
                    f"virtnet_gw=192.0.2.1{dns} virtnet_ip6=fd00::c000:202/120 "
                    "virtnet_gw6=fd00::c000:201\n",
                    encoding="utf-8",
                )
                (root / "proc" / "net" / "if_inet6").write_text(
                    if_inet6, encoding="utf-8"
                )
                (root / "proc" / "net" / "ipv6_route").write_text(
                    ipv6_route, encoding="utf-8"
                )
                (root / "etc" / "resolv.conf").write_text(resolv, encoding="utf-8")
                result = self._run_ipv6_guest_script(
                    shell, root, "ipv6-egress-policy.sh.in", stubs, **values
                )
                self.assertEqual(result.returncode, status, result.stderr)
                if status:
                    self.assertEqual(
                        result.stdout.splitlines(),
                        [f"NVX-IPV6-EGRESS-FAIL code={status}", f"NVX-EXIT {status}"],
                    )
                    continue
                self.assertEqual(
                    result.stdout.splitlines(),
                    [
                        f"NVX-IPV6-EGRESS-DNS {reported}",
                        "NVX-L3-L4-PROBE tcp6-named=reached",
                        "NVX-L3-L4-PROBE tcp6-other=blocked",
                        "NVX-L3-L4-PROBE udp6-named=sent",
                        "NVX-L3-L4-PROBE icmp6=reached",
                        "NVX-L3-L4-PROBE tcp4-named=unexpected",
                        "NVX-IPV6-EGRESS-OK",
                        "NVX-EXIT 0",
                    ],
                )
                # The run replaced the stale result in the root's tmp with its
                # probes'.
                self.assertEqual(
                    sorted(path.name for path in (root / "tmp").iterdir()),
                    [
                        "nvx-egress-icmp6",
                        "nvx-egress-tcp4-named",
                        "nvx-egress-tcp6-named",
                        "nvx-egress-tcp6-other",
                        "nvx-egress-udp6-named",
                    ],
                )

    def test_ipv6_egress_snapshot_script_probes_across_the_snapshot(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        for label, wget, expected in (
            (
                "reached",
                "printf NVX-EGRESS-PROBE-tcp6-named",
                [
                    "NVX-IPV6-EGRESS-BEFORE",
                    "SNAPSHOT",
                    "NVX-IPV6-EGRESS-AFTER",
                    "NVX-EXIT 0",
                ],
            ),
            (
                "blocked after restore",
                '[ ! -e "$restored" ] && printf NVX-EGRESS-PROBE-tcp6-named',
                [
                    "NVX-IPV6-EGRESS-BEFORE",
                    "SNAPSHOT",
                    "NVX-IPV6-EGRESS-FAIL code=81",
                    "NVX-EXIT 81",
                ],
            ),
        ):
            with self.subTest(label), tempfile.TemporaryDirectory() as temporary:
                restored = (Path(temporary) / "restored").as_posix()
                stubs = (
                    f"restored={restored}\n"
                    "wget() {\n"
                    '    [ "$4" = "http://[fd00::c000:201]:20000/tcp6-named" ] ||'
                    " return 9\n"
                    f"    {wget}\n"
                    "}\n"
                    'record_snapshot() { echo SNAPSHOT; : >"$restored"; }\n'
                )
                result = self._run_ipv6_guest_script(
                    shell,
                    Path(temporary),
                    "ipv6-egress-snapshot.sh.in",
                    stubs,
                    GATEWAY_IPV6="fd00::c000:201",
                    NAMED_PORT="20000",
                )
                self.assertEqual(result.stdout.splitlines(), expected, result.stderr)

    def test_ipv6_egress_ports_reserve_both_loopback_families(self):
        try:
            endpoints = microvm_tests._bind_ipv6_egress_ports()
        except RuntimeError as error:
            self.skipTest(str(error))
        try:
            ports = {
                name: endpoint.getsockname()[:2] for name, endpoint in endpoints.items()
            }
            named = ports["tcp6-named"][1]
            self.assertEqual(
                ports,
                {
                    "tcp6-named": ("::1", named),
                    "tcp6-other": ("::1", named + 1),
                    "udp6-named": ("::1", named),
                    "tcp4-named": ("127.0.0.1", named),
                },
            )
            self.assertEqual(endpoints["udp6-named"].type, socket.SOCK_DGRAM)
        finally:
            for endpoint in endpoints.values():
                endpoint.close()

    def test_ipv6_egress_ports_require_host_ipv6_loopback(self):
        unavailable = MagicMock(spec=socket.socket)
        unavailable.__enter__.return_value = unavailable
        unavailable.bind.side_effect = OSError("address family not supported")
        with (
            patch.object(microvm_tests.socket, "socket", return_value=unavailable),
            self.assertRaisesRegex(RuntimeError, "no IPv6 loopback address"),
        ):
            microvm_tests._bind_ipv6_egress_ports()

    def test_runner_dispatches_ipv6_egress_policy(self):
        def require(path: Path, _description: str) -> Path:
            return path

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(microvm_tests, "validate_openvmm_test_backend"),
            patch.object(microvm_tests, "require_file", side_effect=require),
            patch.object(
                microvm_tests, "run_ipv6_egress_policy"
            ) as run_ipv6_egress_policy,
        ):
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "kvm",
                    "--scenario",
                    "ipv6-egress-policy",
                    "--output-dir",
                    temporary,
                ]
            )
            self.assertEqual(microvm_tests.run(args), 0)

            run_ipv6_egress_policy.assert_called_once_with(
                microvm_tests.openvmm_binary_path(),
                microvm_tests.artifact_path(
                    microvm_tests.KernelBuildConstants.BINARY_NAME
                ),
                microvm_tests.artifact_path(
                    microvm_tests.guest_descriptor("alpine").initramfs_name
                ),
                "kvm",
                memory_mib=128,
                timeout=60.0,
                output_dir=Path(temporary),
                guest="alpine",
            )
        self.assertIn("ipv6-egress-policy", microvm_tests.MICROVM_TEST_SCENARIOS)

    def test_sandbox_blocks_use_fixed_roles_and_access(self):
        with (
            patch.object(
                microvm_tests,
                "workload_boot_command",
                return_value=["openvmm", "boot"],
            ),
            patch.object(microvm_tests, "run_guest_script") as run_guest_script,
        ):
            microvm_tests.run_sandbox_blocks(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
                memory_mib=128,
                timeout=60,
                log_path=Path("sandbox-blocks.log"),
            )

        command = run_guest_script.call_args.args[0]
        values = [
            command[index + 1]
            for index, value in enumerate(command)
            if value == "--microvm-sandbox-block"
        ]
        self.assertEqual(len(values), 2)
        self.assertTrue(values[0].startswith("distro:file:"))
        self.assertTrue(values[0].endswith(",ro"))
        self.assertTrue(values[1].startswith("scratch:file:"))
        self.assertFalse(values[1].endswith(",ro"))
        self.assertEqual(
            run_guest_script.call_args.args[2],
            microvm_tests.SANDBOX_BLOCKS_COMPLETION_MARKER,
        )

    def test_smp_snapshot_runs_the_warp_probe_after_each_restore(self):
        def measure(command: list[str], **kwargs: object) -> None:
            processors = int(command[command.index("--processors") + 1])
            log_path = cast(Path, kwargs["log_path"])
            log_path.parent.mkdir(parents=True, exist_ok=True)
            log_path.write_bytes(
                _warp_probe_output(processors)
                + _restore_status_output("whp", processors)
            )

        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary) / "logs"
            with (
                patch.object(
                    microvm_tests,
                    "workload_boot_command",
                    return_value=["openvmm", "boot"],
                ),
                patch.object(
                    microvm_tests,
                    "smp_probe_script",
                    return_value="probe\n",
                ) as smp_probe_script,
                patch.object(microvm_tests, "capture_snapshot") as capture_snapshot,
                patch.object(
                    microvm_tests, "measure_once", side_effect=measure
                ) as measure_once,
                patch.object(
                    microvm_tests,
                    "_snapshot_fingerprint",
                    return_value=("manifest", "state", "memory"),
                ),
            ):
                microvm_tests.run_smp_snapshot(
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "whp",
                    [1, 2, 2, 4, 8],
                    memory_mib=128,
                    timeout=60,
                    output_dir=output_dir,
                )
            logs = sorted(path.name for path in output_dir.iterdir())

        self.assertEqual(
            [call.kwargs["processors"] for call in capture_snapshot.call_args_list],
            [1, 2, 4, 8],
        )
        self.assertEqual(
            [call.args for call in smp_probe_script.call_args_list],
            [(1,), (2,), (4,), (8,)],
        )
        self.assertTrue(
            all(
                call.kwargs["post_restore_script"]
                == "probe\n" + time_abi.warp_probe_script() + time_abi.status_script()
                for call in capture_snapshot.call_args_list
            )
        )
        # The first snapshot is restored twice.
        self.assertEqual(
            [
                int(call.args[0][call.args[0].index("--processors") + 1])
                for call in measure_once.call_args_list
            ],
            [1, 1, 2, 4, 8],
        )
        for entry in measure_once.call_args_list:
            self.assertTrue(entry.kwargs["guest_exit_prequeued"])
            self.assertEqual(entry.kwargs["marker"], benchmark.RESTORE_MARKER)
            self.assertEqual(
                entry.kwargs["failure_marker"], time_abi.WARP_PROBE_FAILURE_MARKER
            )
        self.assertEqual(
            logs,
            [
                "smp-snapshot-1-restore-0.log",
                "smp-snapshot-1-restore-1.log",
                "smp-snapshot-2-restore-0.log",
                "smp-snapshot-4-restore-0.log",
                "smp-snapshot-8-restore-0.log",
            ],
        )

    def test_smp_snapshot_rejects_a_restore_without_a_passing_warp_probe(self):
        def writer(output: bytes):
            def measure(_command: list[str], **kwargs: object) -> None:
                cast(Path, kwargs["log_path"]).write_bytes(output)

            return measure

        cases = (
            (b"", "did not finish its warp probe"),
            (_warp_probe_output(4, offset_ns=5000), "max_abs_offset_ns=5000"),
            (
                _warp_probe_output(4),
                "nvx-time status did not finish before the restore checks",
            ),
            (
                _warp_probe_output(4) + _restore_status_output("kvm", 2),
                "restore marker is invalid: cpus=2 is not 4",
            ),
            (
                _warp_probe_output(4)
                + _restore_status_output("kvm", 4).replace(
                    b"generation=1", b"generation=2"
                ),
                "restore marker is invalid: generation=2 is not 1",
            ),
            (
                _warp_probe_output(4) + b"NVX-TIME-STATUS-EXIT status=0\r\n",
                "did not report an NVX-TIME-ABI restore marker",
            ),
        )
        for output, message in cases:
            with self.subTest(message=message):
                with tempfile.TemporaryDirectory() as temporary:
                    with (
                        patch.object(microvm_tests, "capture_snapshot"),
                        patch.object(
                            microvm_tests, "measure_once", side_effect=writer(output)
                        ),
                        patch.object(
                            microvm_tests,
                            "_snapshot_fingerprint",
                            return_value=("manifest", "state", "memory"),
                        ),
                        self.assertRaisesRegex(
                            RuntimeError, rf"4-vCPU SMP restore 0: .*{message}"
                        ),
                    ):
                        microvm_tests.run_smp_snapshot(
                            Path("openvmm"),
                            Path("vmlinux"),
                            Path("initrd"),
                            "kvm",
                            [4],
                            memory_mib=128,
                            timeout=60,
                            output_dir=Path(temporary),
                        )

    def _run_cpu_profile_fallback(
        self, capture_log: bytes, restore_log: bytes
    ) -> tuple[MagicMock, MagicMock]:
        """Run the CPU profile fallback scenario on KVM, with a capture and a
        restore whose OpenVMM logs are ``capture_log`` and ``restore_log``."""

        def capture(_command: list[str], _snapshot: Path, **kwargs: object) -> None:
            cast(Path, kwargs["log_path"]).write_bytes(capture_log)

        def measure(_command: list[str], **kwargs: object) -> None:
            cast(Path, kwargs["log_path"]).write_bytes(restore_log)

        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(
                    microvm_tests,
                    "workload_boot_command",
                    return_value=["openvmm", "boot"],
                ),
                patch.object(
                    microvm_tests, "capture_snapshot", side_effect=capture
                ) as capture_snapshot,
                patch.object(
                    microvm_tests, "measure_once", side_effect=measure
                ) as measure_once,
            ):
                microvm_tests.run_cpu_profile_fallback(
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "kvm",
                    memory_mib=128,
                    timeout=60,
                    output_dir=Path(temporary),
                )
        return capture_snapshot, measure_once

    def test_cpu_profile_fallback_boots_and_restores_on_the_host_profile(self):
        capture_snapshot, measure_once = self._run_cpu_profile_fallback(
            FALLBACK_WARNING + _declared(b"intel.host.v1"),
            _declared(b"intel.host.v1")
            + _warp_probe_output(1)
            + _restore_status_output("kvm", 1),
        )
        # The cold boot takes the test hook, and its log keeps the time ABI's
        # warning and declared rates beside the capture's records.
        capture = capture_snapshot.call_args
        self.assertEqual(
            capture.args[0],
            [
                "openvmm",
                "boot",
                "--x-time-abi-test-hook",
                "host-cpu-unknown",
                "--snapshot-destination",
                str(capture.args[1]),
            ],
        )
        self.assertEqual(capture.kwargs["processors"], 1)
        self.assertEqual(
            capture.kwargs["post_restore_script"],
            time_abi.warp_probe_script() + time_abi.status_script(),
        )
        self.assertEqual(
            capture.kwargs["log_filter"],
            f"{benchmark.CAPTURE_LOG_FILTER},"
            "openvmm_core::worker::dispatch::time_abi=info",
        )
        # The restore takes auto, without the hook.
        restore = measure_once.call_args
        self.assertNotIn("--x-time-abi-test-hook", restore.args[0])
        self.assertNotIn("--cpu-profile", restore.args[0])
        self.assertEqual(
            restore.kwargs["environment"]["OPENVMM_LOG"],
            microvm_tests.TIME_ABI_RESTORE_LOG_FILTER,
        )
        self.assertTrue(restore.kwargs["guest_exit_prequeued"])
        self.assertEqual(
            restore.kwargs["failure_marker"], time_abi.WARP_PROBE_FAILURE_MARKER
        )

    def test_cpu_profile_fallback_rejects_a_missing_or_wrong_fallback(self):
        restored = (
            _declared(b"intel.host.v1")
            + _warp_probe_output(1)
            + _restore_status_output("kvm", 1)
        )
        cases = (
            (
                _declared(b"intel.icelake-sp.v1"),
                restored,
                "CPU profile fallback cold boot: OpenVMM logged no "
                "NVX-CPU-PROFILE-FALLBACK: warning",
            ),
            (
                FALLBACK_WARNING + FALLBACK_WARNING + _declared(b"intel.host.v1"),
                restored,
                "logged 2 NVX-CPU-PROFILE-FALLBACK: warnings",
            ),
            (
                FALLBACK_WARNING.replace(
                    time_abi.CPU_PROFILE_REQUEST_URL.encode(), b"https://example.com"
                )
                + _declared(b"intel.host.v1"),
                restored,
                "the warning does not link to",
            ),
            (
                FALLBACK_WARNING + _declared(b"intel.icelake-sp.v1"),
                restored,
                r"cold boot declared CPU profiles \['intel.icelake-sp.v1'\], not "
                "intel.host.v1",
            ),
            (
                FALLBACK_WARNING + _declared(b"intel.host.v1"),
                FALLBACK_WARNING + restored,
                "CPU profile fallback restore fell back to a host profile again",
            ),
            (
                FALLBACK_WARNING + _declared(b"intel.host.v1"),
                restored.replace(b"intel.host.v1", b"intel.icelake-sp.v1"),
                r"restore declared CPU profiles \['intel.icelake-sp.v1'\], not the "
                "snapshot's intel.host.v1",
            ),
            (
                FALLBACK_WARNING + _declared(b"intel.host.v1"),
                _warp_probe_output(1),
                "CPU profile fallback restore: nvx-time status did not finish",
            ),
        )
        for capture_log, restore_log, message in cases:
            with (
                self.subTest(message=message),
                self.assertRaisesRegex(RuntimeError, message),
            ):
                self._run_cpu_profile_fallback(capture_log, restore_log)

    def test_restore_processors_uses_capacity_eight_and_each_target(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary) / "logs"
            measure_once = _restore_processors_measure("mshv")
            with (
                patch.object(
                    microvm_tests,
                    "workload_boot_command",
                    return_value=["openvmm", "boot"],
                ) as workload_boot_command,
                patch.object(microvm_tests, "capture_snapshot") as capture_snapshot,
                patch.object(microvm_tests, "measure_once", measure_once),
                patch.object(
                    microvm_tests,
                    "_snapshot_fingerprint",
                    return_value=("manifest", "state", "memory"),
                ),
            ):
                microvm_tests.run_restore_processors(
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "mshv",
                    [1, 2, 4, 8],
                    memory_mib=128,
                    timeout=60,
                    output_dir=output_dir,
                )
            logs = sorted(path.name for path in output_dir.iterdir())

        self.assertEqual(workload_boot_command.call_args.kwargs["processors"], 8)
        self.assertEqual(
            workload_boot_command.call_args.args[5], "quiet loglevel=0 maxcpus=1"
        )
        self.assertEqual(capture_snapshot.call_args.kwargs["processors"], 1)
        self.assertEqual(measure_once.call_count, 5)
        self.assertTrue(
            all(
                entry.kwargs["guest_exit_prequeued"]
                for entry in measure_once.call_args_list
            )
        )
        self.assertTrue(
            all(
                entry.kwargs["environment"][benchmark.SNAPSHOT_PROFILE_ENV] == "1"
                for entry in measure_once.call_args_list
            )
        )
        self.assertEqual(
            [_restore_target(entry.args[0]) for entry in measure_once.call_args_list],
            [1, 2, 4, 8, None],
        )
        # Each restore runs to the end of its post-restore script, so the log
        # holds the processor check and the warp probe for every CPU.
        for entry in measure_once.call_args_list:
            self.assertEqual(entry.kwargs["marker"], benchmark.RESTORE_MARKER)
            self.assertEqual(
                entry.kwargs["failure_marker"], b"NVX-RESTORE-PROCESSORS-FAIL"
            )
        script = capture_snapshot.call_args.kwargs["post_restore_script"]
        self.assertEqual(
            script,
            microvm_tests._read_script("restore-processors.sh")
            + time_abi.warp_probe_script()
            + time_abi.status_script(),
        )
        self.assertEqual(
            logs,
            [
                "restore-processors-1.log",
                "restore-processors-2.log",
                "restore-processors-4.log",
                "restore-processors-8.log",
                "restore-processors-untargeted.log",
            ],
        )

    def test_restore_processors_rejects_full_capacity_mshv_prefix(self):
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(microvm_tests, "capture_snapshot"),
                patch.object(
                    microvm_tests,
                    "measure_once",
                    _restore_processors_measure("mshv", mshv_prefix=False),
                ),
                patch.object(
                    microvm_tests,
                    "_snapshot_fingerprint",
                    return_value=("manifest", "state", "memory"),
                ),
            ):
                with self.assertRaisesRegex(
                    RuntimeError,
                    r"restore target 2 bound VPs \[0, 1, 2, 3, 4, 5, 6, 7\] on mshv; "
                    r"expected exactly VPs 0\.\.1 of capacity 8",
                ):
                    microvm_tests.run_restore_processors(
                        Path("openvmm"),
                        Path("vmlinux"),
                        Path("initrd"),
                        "mshv",
                        [2],
                        memory_mib=128,
                        timeout=60,
                        output_dir=Path(temporary),
                    )

    def test_restore_processors_requires_full_capacity_off_mshv(self):
        for backend in ("kvm", "whp"):
            with self.subTest(backend=backend):
                with tempfile.TemporaryDirectory() as temporary:

                    def measure(
                        command: list[str], backend: str = backend, **kwargs: object
                    ) -> None:
                        log_path = cast(Path, kwargs["log_path"])
                        log_path.write_bytes(
                            b"NVX-RESTORE-PROCESSORS-OK count=2\r\n"
                            + _warp_probe_output(2)
                            + _restore_status_output(backend, 2, boot_cpus=1)
                            + _vp_binding_profile([0, 1])
                        )

                    with (
                        patch.object(microvm_tests, "capture_snapshot"),
                        patch.object(
                            microvm_tests, "measure_once", side_effect=measure
                        ),
                        patch.object(
                            microvm_tests,
                            "_snapshot_fingerprint",
                            return_value=("manifest", "state", "memory"),
                        ),
                    ):
                        with self.assertRaisesRegex(
                            RuntimeError,
                            rf"restore target 2 bound VPs \[0, 1\] on {backend}; "
                            r"expected exactly VPs 0\.\.7 of capacity 8",
                        ):
                            microvm_tests.run_restore_processors(
                                Path("openvmm"),
                                Path("vmlinux"),
                                Path("initrd"),
                                backend,
                                [2],
                                memory_mib=128,
                                timeout=60,
                                output_dir=Path(temporary),
                            )

    def test_restore_vp_bindings_require_one_complete_record_set(self):
        profile = _vp_binding_profile([0, 1])
        self.assertEqual(microvm_tests._restore_vp_bindings(profile), [0, 1])
        self.assertEqual(
            microvm_tests._restore_vp_bindings(b"guest output\n" + profile),
            [0, 1],
        )
        self.assertEqual(
            microvm_tests._restore_vp_bindings(
                profile.replace(
                    b"operation=startup phase=vp_thread_bind",
                    b"phase=vp_thread_bind operation=startup",
                )
            ),
            [0, 1],
        )
        self.assertEqual(
            microvm_tests._restore_vp_bindings(
                profile + profile.replace(b"operation=startup", b"operation=restore")
            ),
            [0, 1],
        )
        microvm_tests._check_restore_vp_bindings(profile, "mshv", target=2, capacity=8)
        with self.assertRaisesRegex(RuntimeError, r"bound VPs \[0, 1, 1\]"):
            microvm_tests._check_restore_vp_bindings(
                _vp_binding_profile([0, 1, 1]), "mshv", target=2, capacity=8
            )
        with self.assertRaisesRegex(RuntimeError, r"untargeted restore bound VPs"):
            microvm_tests._check_restore_vp_bindings(
                profile, "mshv", target=None, capacity=8
            )
        with self.assertRaisesRegex(RuntimeError, r"found 0"):
            microvm_tests._restore_vp_bindings(b"")
        with self.assertRaisesRegex(RuntimeError, r"found 2"):
            microvm_tests._restore_vp_bindings(profile + profile)
        with self.assertRaisesRegex(RuntimeError, r"malformed VP binding"):
            microvm_tests._restore_vp_bindings(
                profile.replace(b"vp_bind_ap_1", b"vp_bind_ap_x")
            )
        with self.assertRaises(ValueError):
            microvm_tests._restore_vp_bindings(
                profile.replace(b"exclusive=0", b"exclusive=2")
            )

    def test_restore_processors_script_checks_activation_without_the_kernel_log(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")

        script = microvm_tests._read_script("restore-processors.sh").replace(
            "nvx-exit", "nvx_exit"
        )
        self.assertNotIn("dmesg", script)
        for online, expected_status in (("0-3", 0), ("0-2", 93)):
            with self.subTest(online=online):
                result = subprocess.run(
                    [shell],
                    input=(
                        "getconf() { printf '4\\n'; }\n"
                        f"cat() {{ printf '%s\\n' '{online}'; }}\n"
                        'taskset() { printf "%s\\n" "$2"; }\n'
                        'nvx_exit() { printf "NVX-EXIT %s\\n" "$1"; }\n' + script
                    ),
                    text=True,
                    capture_output=True,
                    timeout=5,
                    check=False,
                )

                self.assertEqual(result.returncode, expected_status, result.stderr)
                if expected_status == 0:
                    self.assertIn(
                        "NVX-RESTORE-PROCESSOR-OK count=4 cpu=3", result.stdout
                    )
                    self.assertIn("NVX-RESTORE-PROCESSORS-OK count=4", result.stdout)
                    self.assertNotIn("NVX-EXIT", result.stdout)
                else:
                    self.assertIn(
                        "NVX-RESTORE-PROCESSORS-FAIL expected=0-3 actual=0-2",
                        result.stdout,
                    )
                    # A failed check powers the guest off with its status.
                    self.assertIn("NVX-EXIT 93", result.stdout)
                    self.assertNotIn("NVX-RESTORE-PROCESSORS-OK", result.stdout)

    def test_restore_processors_fail_fast_and_record_time_abi_logs(self):
        measure = _restore_processors_measure("mshv")
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(microvm_tests, "capture_snapshot") as capture,
                patch.object(microvm_tests, "measure_once", measure),
                patch.object(
                    microvm_tests,
                    "_snapshot_fingerprint",
                    return_value=("manifest", "state", "memory"),
                ),
            ):
                microvm_tests.run_restore_processors(
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "mshv",
                    [2, 8],
                    memory_mib=128,
                    timeout=60,
                    output_dir=Path(temporary),
                )

        command = capture.call_args.args[0]
        # No test-only CPU feature mask and no clock tuning on the command line.
        self.assertEqual(
            command[command.index("--cmdline") + 1], "quiet loglevel=0 maxcpus=1"
        )
        self.assertEqual(measure.call_count, 3)
        for entry in measure.call_args_list:
            self.assertEqual(
                entry.kwargs["failure_marker"],
                b"NVX-RESTORE-PROCESSORS-FAIL",
            )
            environment = entry.kwargs["environment"]
            self.assertEqual(
                environment["OPENVMM_LOG"],
                "off,openvmm_core::worker::dispatch::time_abi=info",
            )
            self.assertEqual(environment[benchmark.SNAPSHOT_PROFILE_ENV], "1")

    def test_restore_processors_require_the_count_and_a_passing_warp_probe(self):
        def wrong_count(command: list[str], **kwargs: object) -> None:
            _restore_processors_measure("kvm")(command, **kwargs)
            log_path = cast(Path, kwargs["log_path"])
            log_path.write_bytes(
                log_path.read_bytes().replace(
                    b"PROCESSORS-OK count=4", b"PROCESSORS-OK count=2"
                )
            )

        cases = (
            (wrong_count, "restore target 4 did not report"),
            (
                _restore_processors_measure("kvm", warp_offset_ns=1500),
                "restore target 4: cross-vCPU TSC skew check failed in round 1: "
                "max_abs_offset_ns=1500 exceeds 1000",
            ),
        )
        for measure, message in cases:
            with self.subTest(message=message):
                with tempfile.TemporaryDirectory() as temporary:
                    with (
                        patch.object(microvm_tests, "capture_snapshot"),
                        patch.object(
                            microvm_tests, "measure_once", side_effect=measure
                        ),
                        patch.object(
                            microvm_tests,
                            "_snapshot_fingerprint",
                            return_value=("manifest", "state", "memory"),
                        ),
                        self.assertRaisesRegex(RuntimeError, message),
                    ):
                        microvm_tests.run_restore_processors(
                            Path("openvmm"),
                            Path("vmlinux"),
                            Path("initrd"),
                            "kvm",
                            [4],
                            memory_mib=128,
                            timeout=60,
                            output_dir=Path(temporary),
                        )

    def test_restore_processors_report_guest_failures_with_the_target(self):
        failure = benchmark.GuestFailureReported(
            "NVX-RESTORE-PROCESSORS-FAIL expected=0-3 actual=0-2", "tail"
        )
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(microvm_tests, "capture_snapshot"),
                patch.object(microvm_tests, "measure_once", side_effect=failure),
                patch.object(
                    microvm_tests,
                    "_snapshot_fingerprint",
                    return_value=("manifest", "state", "memory"),
                ),
            ):
                with self.assertRaises(RuntimeError) as raised:
                    microvm_tests.run_restore_processors(
                        Path("openvmm"),
                        Path("vmlinux"),
                        Path("initrd"),
                        "kvm",
                        [4],
                        memory_mib=128,
                        timeout=60,
                        output_dir=Path(temporary),
                    )

        self.assertEqual(
            str(raised.exception),
            "restore target 4: guest reported "
            "NVX-RESTORE-PROCESSORS-FAIL expected=0-3 actual=0-2\n"
            "--- OpenVMM output ---\ntail",
        )
        self.assertIs(raised.exception.__cause__, failure)

    def _run_platform_restore_processors(
        self,
        backend: str,
        processor_counts: list[int],
        output_dir: Path,
        measure_once: MagicMock,
        *,
        fingerprints: list[tuple[str, str, str, str | None]] | None = None,
    ) -> tuple[MagicMock, MagicMock, MagicMock]:
        fingerprint = ("manifest", "state", "memory", None)
        with (
            patch.object(
                microvm_tests,
                "workload_boot_command",
                return_value=["openvmm", "boot"],
            ) as workload_boot_command,
            patch.object(microvm_tests, "capture_snapshot") as capture_snapshot,
            patch.object(microvm_tests, "measure_once", measure_once),
            patch.object(
                microvm_tests,
                "_scratch_snapshot_fingerprint",
                side_effect=fingerprints or itertools.repeat(fingerprint),
            ),
            patch.object(microvm_tests, "_expect_process_failure") as expect_failure,
        ):
            microvm_tests.run_platform_restore_processors(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                backend,
                processor_counts,
                memory_mib=128,
                timeout=60,
                output_dir=output_dir,
            )
        return workload_boot_command, capture_snapshot, expect_failure

    def test_platform_restore_processors_activate_each_target_of_one_capture(self):
        def blocks(command: list[str]) -> list[str]:
            return [
                command[index + 1]
                for index, argument in enumerate(command)
                if argument == "--microvm-sandbox-block"
            ]

        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary) / "logs"
            measure_once = _restore_processors_measure(
                "mshv",
                capacity=4,
                prefix=microvm_tests.PLATFORM_POST_RESTORE_MARKER + b"\r\n",
            )
            workload_boot_command, capture_snapshot, expect_failure = (
                self._run_platform_restore_processors(
                    "mshv", [1, 2, 4, 8], output_dir, measure_once
                )
            )
            logs = sorted(path.name for path in output_dir.iterdir())

        self.assertIn(
            "platform-restore-processors", microvm_tests.MICROVM_TEST_SCENARIOS
        )
        self.assertIn(
            "platform-restore-processors", microvm_tests.SANDBOX_CONTROL_SCENARIOS
        )
        self.assertEqual(workload_boot_command.call_args.kwargs["processors"], 4)
        # A platform command line carries no tenant configuration, so the
        # boot-online count is the capture's only caller token.
        self.assertEqual(workload_boot_command.call_args.args[5], "maxcpus=1")
        capture = capture_snapshot.call_args.args[0]
        self.assertEqual(capture[capture.index("--snapshot-tier") + 1], "platform")
        capture_blocks = blocks(capture)
        self.assertEqual(len(capture_blocks), 2)
        self.assertRegex(capture_blocks[0], r"^distro:file:.+,ro$")
        self.assertRegex(capture_blocks[1], r"^scratch:file:.+(?<!,ro)$")
        self.assertEqual(capture_snapshot.call_args.kwargs["processors"], 1)
        self.assertEqual(
            capture_snapshot.call_args.kwargs["post_restore_script"],
            microvm_tests._read_script("restore-processors.sh")
            + time_abi.warp_probe_script()
            + time_abi.status_script(),
        )
        # Counts above the capacity of 4 are not restore targets.
        self.assertEqual(
            [_restore_target(entry.args[0]) for entry in measure_once.call_args_list],
            [1, 2, 4, None],
        )
        restore_scratches: list[str] = []
        for entry in measure_once.call_args_list:
            command = entry.args[0]
            self.assertEqual(command[command.index("--processors") + 1], "4")
            restore_blocks = blocks(command)
            self.assertEqual(restore_blocks[0], capture_blocks[0])
            self.assertRegex(restore_blocks[1], r"^scratch:file:.+(?<!,ro)$")
            restore_scratches.append(restore_blocks[1])
            self.assertEqual(entry.kwargs["marker"], benchmark.RESTORE_MARKER)
            self.assertTrue(entry.kwargs["guest_exit_prequeued"])
            self.assertEqual(
                entry.kwargs["environment"][benchmark.SNAPSHOT_PROFILE_ENV], "1"
            )
        self.assertNotIn(capture_blocks[1], restore_scratches)
        self.assertEqual(len(set(restore_scratches)), len(restore_scratches))
        self.assertEqual(
            [
                (_restore_target(entry.args[0]), entry.args[3])
                for entry in expect_failure.call_args_list
            ],
            [
                (0, b"restore-online VP count 0 is not supported"),
                (3, b"restore-online VP count 3 is not supported"),
                (8, b"restore-online VP count 8 exceeds VP capacity 4"),
            ],
        )
        for entry in expect_failure.call_args_list:
            self.assertEqual(
                entry.kwargs["forbidden_markers"], (benchmark.RESTORE_MARKER,)
            )
        self.assertEqual(
            logs,
            [
                "platform-restore-processors-1.log",
                "platform-restore-processors-2.log",
                "platform-restore-processors-4.log",
                "platform-restore-processors-untargeted.log",
            ],
        )

    def test_platform_restore_processors_require_the_platform_repair(self):
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaisesRegex(
                RuntimeError,
                r"^platform restore target 2 did not run the platform repair$",
            ):
                self._run_platform_restore_processors(
                    "kvm",
                    [2],
                    Path(temporary),
                    _restore_processors_measure("kvm", capacity=4),
                )

    def test_platform_restore_processors_check_the_mshv_prefix(self):
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaisesRegex(
                RuntimeError,
                r"restore target 2 bound VPs \[0, 1, 2, 3\] on mshv; "
                r"expected exactly VPs 0\.\.1 of capacity 4",
            ):
                self._run_platform_restore_processors(
                    "mshv",
                    [2],
                    Path(temporary),
                    _restore_processors_measure(
                        "mshv",
                        mshv_prefix=False,
                        capacity=4,
                        prefix=microvm_tests.PLATFORM_POST_RESTORE_MARKER + b"\r\n",
                    ),
                )

    def test_platform_restore_processors_keep_the_snapshot_intact(self):
        original = ("manifest", "state", "memory", None)
        changed = ("manifest", "state", "changed", None)
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaisesRegex(
                RuntimeError,
                "^a refused platform restore modified snapshot artifacts$",
            ):
                self._run_platform_restore_processors(
                    "whp",
                    [1],
                    Path(temporary),
                    _restore_processors_measure(
                        "whp",
                        capacity=4,
                        prefix=microvm_tests.PLATFORM_POST_RESTORE_MARKER + b"\r\n",
                    ),
                    # The capture, the restore of 1, the untargeted restore,
                    # and the refused restores.
                    fingerprints=[original, original, original, changed],
                )

    def test_platform_zero_fill_script_digests_the_reads_around_the_image_end(self):
        capacity = microvm_tests.PLATFORM_ZERO_FILL_CAPACITY
        sectors = microvm_tests.PLATFORM_ZERO_FILL_IMAGE_SECTORS
        image = microvm_tests._zero_fill_image(sectors)
        # Every sector differs from its neighbors and from zeros.
        self.assertEqual(len(image), sectors * 512)
        self.assertEqual(image[:512], b"\x01" * 512)
        self.assertEqual(image[-512:], bytes((600 % 251 + 1,)) * 512)
        self.assertNotIn(0, image)
        script = microvm_tests.platform_zero_fill_script(
            image, capacity, microvm_tests.PLATFORM_ZERO_FILL_SCRATCH
        )
        self.assertNotRegex(script, r"@[A-Z0-9_]+@")

        # The device that the clone's guest reads: the image, then zeros.
        device = image + bytes(capacity - len(image))

        def digest(data: bytes) -> str:
            return hashlib.sha256(data).hexdigest()

        self.assertIn(f'= {capacity} ] || fail "distro size', script)
        self.assertIn(
            f'digest /dev/vda 512 0 {sectors})" = {digest(device[: sectors * 512])}',
            script,
        )
        # The image ends in the page of sectors 600 to 607.
        self.assertIn(
            f'digest /dev/vda 4096 75 1)" = {digest(device[75 * 4096 : 76 * 4096])}',
            script,
        )
        self.assertIn(
            f'digest /dev/vda 512 {sectors} {capacity // 512 - sectors})" = '
            f"{digest(device[sectors * 512 :])}",
            script,
        )
        scratch = digest(bytes(microvm_tests.PLATFORM_ZERO_FILL_SCRATCH))
        self.assertEqual(script.count(f'digest /dev/vdb 4096 0 256)" = {scratch}'), 2)
        for marker in (
            microvm_tests.PLATFORM_ZERO_FILL_IMAGE_MARKER,
            microvm_tests.PLATFORM_ZERO_FILL_SCRATCH_MARKER,
        ):
            self.assertIn(f"\necho {marker.decode()}\n", script)

        for invalid in (image[:-256], image[:512] * 8, bytes(capacity)):
            with self.assertRaises(ValueError):
                microvm_tests.platform_zero_fill_script(invalid, capacity, 4096)

    def _run_platform_zero_fill(
        self,
        output_dir: Path,
        measure_once: MagicMock,
        *,
        fingerprints: Sequence[tuple[str, str, str, str | None]] | None = None,
        capture: Callable[..., None] | None = None,
        refused: Callable[..., None] | None = None,
    ) -> tuple[MagicMock, MagicMock, MagicMock, MagicMock]:
        fingerprint = ("manifest", "state", "memory", None)
        with (
            patch.object(
                microvm_tests,
                "workload_boot_command",
                return_value=["openvmm", "boot"],
            ) as workload_boot_command,
            patch.object(
                microvm_tests, "capture_snapshot", side_effect=capture
            ) as capture_snapshot,
            patch.object(microvm_tests, "measure_once", measure_once),
            patch.object(
                microvm_tests,
                "_scratch_snapshot_fingerprint",
                side_effect=fingerprints or itertools.repeat(fingerprint),
            ),
            patch.object(
                microvm_tests, "_expect_process_failure", side_effect=refused
            ) as expect_failure,
            patch.object(microvm_tests, "_expect_boot_failure") as expect_boot_failure,
        ):
            microvm_tests.run_platform_zero_fill(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
                memory_mib=128,
                timeout=60,
                output_dir=output_dir,
            )
        return (
            workload_boot_command,
            capture_snapshot,
            expect_failure,
            expect_boot_failure,
        )

    @staticmethod
    def _zero_fill_measure(
        output: bytes | None = None,
        on_restore: Callable[[list[str]], None] | None = None,
    ) -> MagicMock:
        def measure(command: list[str], **kwargs: object) -> None:
            if on_restore is not None:
                on_restore(command)
            log_path = cast(Path, kwargs["log_path"])
            log_path.parent.mkdir(parents=True, exist_ok=True)
            log_path.write_bytes(
                output
                if output is not None
                else microvm_tests.PLATFORM_POST_RESTORE_MARKER
                + b"\r\n"
                + microvm_tests.PLATFORM_ZERO_FILL_IMAGE_MARKER
                + b"\r\n"
                + microvm_tests.PLATFORM_ZERO_FILL_SCRATCH_MARKER
                + b"\r\n"
                + benchmark.RESTORE_MARKER
                + b"\r\n"
            )

        return MagicMock(side_effect=measure)

    def test_platform_zero_fill_restores_a_shorter_image_with_an_absent_scratch(
        self,
    ):
        def blocks(command: list[str]) -> list[str]:
            return [
                command[index + 1]
                for index, argument in enumerate(command)
                if argument == "--microvm-sandbox-block"
            ]

        def block_path(block: str) -> Path:
            return Path(block.split(":file:", 1)[1].removesuffix(",ro"))

        capacity = microvm_tests.PLATFORM_ZERO_FILL_CAPACITY
        image = microvm_tests._zero_fill_image(
            microvm_tests.PLATFORM_ZERO_FILL_IMAGE_SECTORS
        )
        sizes: dict[str, int] = {}
        restore_images: list[bytes] = []

        def capture(command: list[str], *_args: object, **_kwargs: object) -> None:
            distro, scratch = blocks(command)
            sizes["placeholder"] = block_path(distro).stat().st_size
            sizes["capture-scratch"] = block_path(scratch).stat().st_size

        def on_restore(command: list[str]) -> None:
            (distro,) = blocks(command)
            restore_images.append(block_path(distro).read_bytes())

        def refused(command: list[str], *_args: object, **_kwargs: object) -> None:
            # The first refused restore is the longer image's.
            sizes.setdefault(
                "longer-image", block_path(blocks(command)[0]).stat().st_size
            )

        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary) / "logs"
            measure_once = self._zero_fill_measure(on_restore=on_restore)
            (
                workload_boot_command,
                capture_snapshot,
                expect_failure,
                expect_boot_failure,
            ) = self._run_platform_zero_fill(
                output_dir, measure_once, capture=capture, refused=refused
            )
            logs = sorted(path.name for path in output_dir.iterdir())

        self.assertIn("platform-zero-fill", microvm_tests.MICROVM_TEST_SCENARIOS)
        self.assertIn("platform-zero-fill", microvm_tests.SANDBOX_CONTROL_SCENARIOS)
        # A platform command line carries no caller tokens.
        self.assertEqual(workload_boot_command.call_args.args[5], "")
        capture_command = capture_snapshot.call_args.args[0]
        self.assertEqual(
            capture_command[capture_command.index("--snapshot-tier") + 1], "platform"
        )
        capture_blocks = blocks(capture_command)
        self.assertEqual(len(capture_blocks), 2)
        self.assertRegex(capture_blocks[0], r"^distro:file:.+,ro$")
        self.assertRegex(capture_blocks[1], r"^scratch:file:.+(?<!,ro)$")
        self.assertEqual(capture_snapshot.call_args.kwargs["processors"], 1)
        self.assertEqual(
            capture_snapshot.call_args.kwargs["post_restore_script"],
            microvm_tests.platform_zero_fill_script(
                image, capacity, microvm_tests.PLATFORM_ZERO_FILL_SCRATCH
            ),
        )
        # The placeholder fixes the capacity, and the longer image exceeds it.
        self.assertEqual(
            sizes,
            {
                "placeholder": capacity,
                "capture-scratch": microvm_tests.PLATFORM_ZERO_FILL_SCRATCH,
                "longer-image": capacity + 512,
            },
        )

        # One clone restores the shorter image with its scratch absent.
        measure_once.assert_called_once()
        restore = measure_once.call_args.args[0]
        (restore_distro,) = blocks(restore)
        self.assertRegex(restore_distro, r"^distro:file:.+,ro$")
        self.assertNotEqual(restore_distro, capture_blocks[0])
        self.assertIn("--restore-absent-scratch", restore)
        self.assertEqual(restore_images, [image])
        self.assertEqual(
            measure_once.call_args.kwargs["environment"]["OPENVMM_LOG"], "off"
        )
        self.assertTrue(measure_once.call_args.kwargs["guest_exit_prequeued"])
        self.assertEqual(
            measure_once.call_args.kwargs["failure_marker"],
            microvm_tests.PLATFORM_ZERO_FILL_FAILURE_MARKER,
        )
        self.assertEqual(logs, ["platform-zero-fill-restore.log"])

        # OpenVMM refuses the other restores before the guest runs.
        self.assertEqual(
            [entry.args[3] for entry in expect_failure.call_args_list],
            [reason for _, reason in microvm_tests.PLATFORM_ZERO_FILL_REJECTED],
        )
        longer, absent_and_file, no_scratch = (
            entry.args[0] for entry in expect_failure.call_args_list
        )
        self.assertEqual(len(blocks(longer)), 2)
        self.assertNotIn("--restore-absent-scratch", longer)
        self.assertEqual(len(blocks(absent_and_file)), 2)
        self.assertIn("--restore-absent-scratch", absent_and_file)
        self.assertEqual(blocks(no_scratch), [restore_distro])
        self.assertNotIn("--restore-absent-scratch", no_scratch)
        for entry in expect_failure.call_args_list:
            self.assertIn("--restore-snapshot", entry.args[0])
            self.assertEqual(
                entry.kwargs["forbidden_markers"], (benchmark.RESTORE_MARKER,)
            )
        # A cold boot has no captured scratch to declare absent.
        cold_boot = expect_boot_failure.call_args.args[0]
        self.assertEqual(cold_boot[:2], ["openvmm", "boot"])
        self.assertIn("--restore-absent-scratch", cold_boot)
        self.assertNotIn("--restore-snapshot", cold_boot)
        self.assertEqual(expect_boot_failure.call_args.args[3], b"--restore-snapshot")

    def test_platform_zero_fill_requires_both_checks_and_the_platform_repair(self):
        complete = (
            microvm_tests.PLATFORM_POST_RESTORE_MARKER,
            microvm_tests.PLATFORM_ZERO_FILL_IMAGE_MARKER,
            microvm_tests.PLATFORM_ZERO_FILL_SCRATCH_MARKER,
        )
        for missing in complete:
            output = b"".join(
                marker + b"\r\n" for marker in complete if marker != missing
            )
            with (
                self.subTest(missing=missing),
                tempfile.TemporaryDirectory() as temporary,
            ):
                with self.assertRaisesRegex(
                    RuntimeError,
                    "^platform zero-fill restore did not report "
                    + re.escape(repr(missing.decode()))
                    + "$",
                ):
                    self._run_platform_zero_fill(
                        Path(temporary), self._zero_fill_measure(output)
                    )

    def test_platform_zero_fill_reports_guest_failures(self):
        failure = benchmark.GuestFailureReported(
            "NVX-ZERO-FILL-FAIL distro reads past the image's end", "tail"
        )
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(RuntimeError) as raised:
                self._run_platform_zero_fill(
                    Path(temporary), MagicMock(side_effect=failure)
                )
        self.assertEqual(
            str(raised.exception),
            "platform zero-fill restore: guest reported "
            "NVX-ZERO-FILL-FAIL distro reads past the image's end\n"
            "--- OpenVMM output ---\ntail",
        )

    def test_platform_zero_fill_keeps_the_image_and_the_snapshot_intact(self):
        def overwrite_image(command: list[str]) -> None:
            image = command[command.index("--microvm-sandbox-block") + 1]
            Path(image.split(":file:", 1)[1].removesuffix(",ro")).write_bytes(b"x")

        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaisesRegex(
                RuntimeError,
                "^platform zero-fill restore changed the distro image$",
            ):
                self._run_platform_zero_fill(
                    Path(temporary),
                    self._zero_fill_measure(on_restore=overwrite_image),
                )

        original = ("manifest", "state", "memory", None)
        changed = ("manifest", "state", "changed", None)
        for fingerprints, message in (
            # The capture, then the clone.
            ([original, changed], "^platform zero-fill restore modified snapshot"),
            # The capture, the clone, then the refused restores.
            (
                [original, original, changed],
                "^a refused platform zero-fill restore modified snapshot",
            ),
        ):
            with (
                self.subTest(message=message),
                tempfile.TemporaryDirectory() as temporary,
            ):
                with self.assertRaisesRegex(RuntimeError, message):
                    self._run_platform_zero_fill(
                        Path(temporary),
                        self._zero_fill_measure(),
                        fingerprints=fingerprints,
                    )

    def test_platform_network_policy_script_probes_both_services_and_serves_the_host(
        self,
    ):
        script = microvm_tests.platform_network_policy_script(
            allowed_port=40001, other_port=40002, guest_port=18080
        )
        self.assertNotRegex(script, r"@[A-Z0-9_]+@")
        self.assertIn('"http://10.0.0.1:$2/$1"', script)
        self.assertIn("wget -T 2 -qO /tmp/nvx-platform-network-probe", script)
        self.assertIn("\nprobe allowed 40001\n", script)
        self.assertIn("\nprobe other 40002\n", script)
        # The guest's server answers the host's exchange through a forward.
        ack = microvm_tests.HOST_LOOPBACK_TCP_ACK.decode().removesuffix("\n")
        self.assertIn(f"printf '{ack}\\n' |\n    nc -l -p 18080 -w 20", script)
        self.assertIn("printf '%04X' 18080", script)
        self.assertIn(
            "grep -qx "
            + microvm_tests.HOST_LOOPBACK_TCP_REQUEST.decode().strip()
            + " /tmp/nvx-platform-network-inbound",
            script,
        )
        # After an exchange through a forward, the guest waits for the host's
        # request, which the release may overtake, before it stops its server.
        self.assertIn("read -r release exchange ||", script)
        self.assertIn(
            "forward)\n    tries=0\n    while ! grep -qx "
            + microvm_tests.HOST_LOOPBACK_TCP_REQUEST.decode().strip()
            + " /tmp/nvx-platform-network-inbound",
            script,
        )
        self.assertLess(
            script.index("forward)\n    tries=0"), script.index('kill "$listener"')
        )
        for marker in (
            microvm_tests.PLATFORM_NETWORK_LISTENING_MARKER,
            microvm_tests.PLATFORM_NETWORK_CHECKED,
        ):
            self.assertIn(marker.decode(), script)
        self.assertIn(
            'echo "NVX-PLATFORM-NETWORK-IDENTITY mac=$mac ipv4=$ipv4 ipv6=$ipv6"',
            script,
        )
        self.assertIn(
            'echo "'
            + microvm_tests.PLATFORM_NETWORK_PROBE_PREFIX.decode()
            + 'allowed=$allowed other=$other inbound=$inbound"',
            script,
        )
        self.assertIn(
            'echo "' + microvm_tests.PLATFORM_NETWORK_FAILURE_MARKER.decode() + ' $1"',
            script,
        )

    def test_platform_network_clones_reach_what_their_policies_admit(self):
        clones = microvm_tests.platform_network_clones(
            allowed_port=40001, other_port=40002, host_port=40003, guest_port=18080
        )
        self.assertEqual(
            [
                (clone.name, clone.allowed, clone.other, clone.forward)
                for clone in clones
            ],
            [
                ("deny-all", False, False, False),
                ("egress-rules", True, False, False),
                ("forward", True, True, True),
                ("proxy", False, True, False),
            ],
        )
        deny_all, rules, forward, proxy = clones
        # A clone without connectivity keeps the template's own policy.
        self.assertEqual(
            deny_all.policy, microvm_tests.PLATFORM_NETWORK_TEMPLATE_POLICY
        )
        self.assertEqual(
            microvm_tests.PLATFORM_NETWORK_TEMPLATE_POLICY,
            ("--network-egress", "deny", "--host-loopback", "deny"),
        )
        self.assertEqual(
            rules.policy,
            (
                "--network-egress",
                "deny",
                "--network-egress-allow",
                "10.0.0.1/32:tcp:40001",
            ),
        )
        self.assertEqual(
            forward.policy,
            (
                "--network-egress",
                "allow",
                "--host-loopback",
                "allow",
                "--host-loopback-forward",
                "tcp:40003:18080",
                "--network-ingress",
                "allow",
            ),
        )
        self.assertEqual(
            proxy.policy,
            (
                "--network-egress",
                "deny",
                "--host-loopback",
                "deny",
                "--network-proxy",
                "10.0.0.1:40002",
            ),
        )
        self.assertEqual(
            [clone.probe_report() for clone in clones],
            [
                b"NVX-PLATFORM-NETWORK-PROBE allowed=denied other=denied inbound=none",
                b"NVX-PLATFORM-NETWORK-PROBE allowed=reached other=denied inbound=none",
                b"NVX-PLATFORM-NETWORK-PROBE allowed=reached other=reached "
                b"inbound=reached",
                b"NVX-PLATFORM-NETWORK-PROBE allowed=denied other=reached inbound=none",
            ],
        )

    @staticmethod
    def _platform_network_reaches(policy: Sequence[str], port: int) -> bool:
        """Return whether OpenVMM lets a clone under ``policy`` reach a host
        service at ``port`` of the gateway."""

        def value(option: str) -> str | None:
            return policy[policy.index(option) + 1] if option in policy else None

        return (
            value("--network-egress") == "allow"
            or value("--network-egress-allow") == f"10.0.0.1/32:tcp:{port}"
            or value("--network-proxy") == f"10.0.0.1:{port}"
        )

    def _simulate_platform_network_guest(
        self, command: list[str], ports: dict[str, int]
    ) -> list[bytes]:
        """Return the lines of the clone that ``command`` restores, after it
        requests each host service in ``ports`` that its policy admits."""
        policy = command[command.index("--restore-network-policy") + 1 :]
        reached = {
            name: self._platform_network_reaches(policy, port)
            for name, port in ports.items()
        }
        for name, port in ports.items():
            if not reached[name]:
                continue
            with socket.create_connection(("127.0.0.1", port), timeout=5) as client:
                client.sendall(
                    f"GET /{name} HTTP/1.1\r\nHost: 10.0.0.1\r\n\r\n".encode()
                )
                answer = b""
                while chunk := client.recv(4096):
                    answer += chunk
            self.assertTrue(
                answer.endswith(f"\r\n\r\nNVX-PLATFORM-NETWORK-{name}".encode())
            )
        clone = microvm_tests.PlatformNetworkClone(
            "",
            tuple(policy),
            allowed=reached["allowed"],
            other=reached["other"],
            forward="--host-loopback-forward" in policy,
        )
        return [
            microvm_tests.PLATFORM_POST_RESTORE_MARKER,
            microvm_tests.PLATFORM_NETWORK_IDENTITY,
            clone.probe_report(),
            benchmark.RESTORE_MARKER,
        ]

    def _run_platform_network_policy(
        self,
        output_dir: Path,
        *,
        guest: Callable[[list[str], dict[str, int]], list[bytes]] | None = None,
        fingerprints: Sequence[tuple[str, str, str, str | None]] | None = None,
    ) -> dict[str, Any]:
        """Run the scenario with a simulated OpenVMM, whose clones request the
        scenario's real host services as their policies admit, and return its
        calls."""
        captures: list[tuple[list[str], dict[str, Any]]] = []
        restores: list[list[str]] = []
        releases: list[str] = []
        ports: dict[str, int] = {}
        busy: list[bool] = []

        def capture(command: list[str], *_args: object, **kwargs: Any) -> None:
            captures.append((command, kwargs))
            script = kwargs["post_restore_script"]
            if script is not None:
                for name in ("allowed", "other"):
                    match = re.search(rf"\nprobe {name} (\d+)\n", script)
                    assert match is not None
                    ports[name] = int(match[1])

        simulated_guest = guest or self._simulate_platform_network_guest

        class FakeProcess:
            def __init__(self, command: list[str], log_path: Path) -> None:
                restores.append(command)
                self.command = command
                self.log_path = log_path
                self.lines: list[bytes] = []
                self.released = False

            def __enter__(self) -> "FakeProcess":
                return self

            def __exit__(self, *_exception: object) -> None:
                return None

            def wait_for_line(self, marker: bytes, _timeout: float) -> None:
                assert marker == microvm_tests.PLATFORM_NETWORK_LISTENING_MARKER
                self.lines = simulated_guest(self.command, ports)

            def send_line(self, line: str) -> None:
                releases.append(line)
                self.released = True

            def wait(self, _timeout: float) -> openvmm_process.OpenvmmProcessResult:
                assert self.released
                output = b"".join(line + b"\r\n" for line in self.lines)
                self.log_path.write_bytes(output)
                return openvmm_process.OpenvmmProcessResult(0, output)

        def refused(command: list[str], *_args: object, **_kwargs: object) -> None:
            if "--host-loopback-forward" in command:
                port = int(
                    command[command.index("--host-loopback-forward") + 1].split(":")[1]
                )
                with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
                    try:
                        probe.bind(("127.0.0.1", port))
                    except OSError:
                        busy.append(True)
                    else:
                        busy.append(False)

        fingerprint = ("manifest", "state", "memory", None)
        output_dir.mkdir(parents=True, exist_ok=True)
        with (
            patch.object(
                microvm_tests,
                "workload_boot_command",
                return_value=["openvmm", "boot"],
            ) as workload_boot_command,
            patch.object(microvm_tests, "capture_snapshot", side_effect=capture),
            patch.object(microvm_tests, "OpenvmmProcess", FakeProcess),
            patch.object(microvm_tests, "_host_loopback_tcp_exchange") as exchange,
            patch.object(
                microvm_tests,
                "_scratch_snapshot_fingerprint",
                side_effect=fingerprints or itertools.repeat(fingerprint),
            ),
            patch.object(
                microvm_tests, "_expect_process_failure", side_effect=refused
            ) as expect_failure,
            patch.object(microvm_tests, "_expect_boot_failure") as expect_boot_failure,
        ):
            microvm_tests.run_platform_network_policy(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
                memory_mib=128,
                timeout=60,
                output_dir=output_dir,
            )
        return {
            "workload_boot_command": workload_boot_command,
            "captures": captures,
            "restores": restores,
            "releases": releases,
            "exchange": exchange,
            "expect_failure": expect_failure,
            "expect_boot_failure": expect_boot_failure,
            "ports": ports,
            "busy": busy,
        }

    def test_platform_network_policy_restores_one_template_with_four_policies(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary) / "logs"
            calls = self._run_platform_network_policy(output_dir)
            logs = sorted(path.name for path in output_dir.iterdir())

        self.assertIn("platform-network-policy", microvm_tests.MICROVM_TEST_SCENARIOS)
        self.assertIn(
            "platform-network-policy", microvm_tests.SANDBOX_CONTROL_SCENARIOS
        )
        # A platform command line carries no caller tokens, and the template
        # has a NIC.
        boot_call = calls["workload_boot_command"].call_args
        self.assertEqual(boot_call.args[5], "")
        self.assertEqual(
            boot_call.kwargs["network"], microvm_tests.PLATFORM_NETWORK_CIDR
        )

        # The template and the plain deny-all snapshot are platform captures
        # with sandbox blocks, and only the template's clones run the checks.
        (template, template_kwargs), (plain, plain_kwargs) = calls["captures"]
        for command in (template, plain):
            self.assertEqual(command[:2], ["openvmm", "boot"])
            self.assertEqual(command[command.index("--snapshot-tier") + 1], "platform")
            self.assertEqual(command.count("--microvm-sandbox-block"), 2)
        policy = template.index("--network-egress")
        self.assertEqual(
            template[policy : policy + 4],
            list(microvm_tests.PLATFORM_NETWORK_TEMPLATE_POLICY),
        )
        self.assertNotIn("--host-loopback", plain)
        self.assertEqual(plain[plain.index("--network-egress") + 1], "deny")
        ports = calls["ports"]
        self.assertEqual(
            template_kwargs["post_restore_script"],
            microvm_tests.platform_network_policy_script(
                allowed_port=ports["allowed"],
                other_port=ports["other"],
                guest_port=microvm_tests.PLATFORM_NETWORK_GUEST_PORT,
            ),
        )
        self.assertIsNone(plain_kwargs["post_restore_script"])
        self.assertEqual(template_kwargs["processors"], 1)

        # Each clone restores the template with its own policy.
        restores = calls["restores"]
        self.assertEqual(len(restores), 4)
        forward_port = None
        for command, name in zip(
            restores, ("deny-all", "egress-rules", "forward", "proxy"), strict=True
        ):
            with self.subTest(clone=name):
                self.assertIn("--restore-snapshot", command)
                self.assertEqual(
                    command[command.index("--network-profile") + 1], "portable"
                )
                self.assertIn("--restore-absent-scratch", command)
                self.assertEqual(command.count("--microvm-sandbox-block"), 1)
                self.assertRegex(
                    command[command.index("--microvm-sandbox-block") + 1],
                    r"^distro:file:.+,ro$",
                )
                self.assertIn("--restore-network-policy", command)
                if "--host-loopback-forward" in command:
                    forward_port = int(
                        command[command.index("--host-loopback-forward") + 1].split(
                            ":"
                        )[1]
                    )
        self.assertIsNotNone(forward_port)
        calls["exchange"].assert_called_once_with("127.0.0.1", forward_port, 60)
        # Each release tells the guest whether the host exchanged a line with
        # it through a forward, which only the forward clone has.
        self.assertEqual(
            calls["releases"],
            [
                "NVX-PLATFORM-NETWORK-CHECKED none",
                "NVX-PLATFORM-NETWORK-CHECKED none",
                "NVX-PLATFORM-NETWORK-CHECKED forward",
                "NVX-PLATFORM-NETWORK-CHECKED none",
            ],
        )
        self.assertEqual(
            logs,
            sorted(
                f"platform-network-policy-{name}.log"
                for name in ("deny-all", "egress-rules", "forward", "proxy")
            ),
        )

        # OpenVMM refuses the other restores before the guest runs.
        failures = calls["expect_failure"].call_args_list
        self.assertEqual(
            [entry.args[3] for entry in failures],
            [
                *(reason for _, reason in microvm_tests.PLATFORM_NETWORK_REJECTED),
                microvm_tests.PLATFORM_NETWORK_BUSY_FORWARD_REJECTION,
                microvm_tests.PLATFORM_NETWORK_NOT_TEMPLATE_REJECTION,
            ],
        )
        (
            kept_policy,
            kept_host_loopback,
            readdressed,
            foreign_proxy,
            busy_forward,
            plain_restore,
        ) = (entry.args[0] for entry in failures)
        self.assertNotIn("--restore-network-policy", kept_policy)
        self.assertNotIn("--restore-network-policy", kept_host_loopback)
        self.assertEqual(
            kept_host_loopback[kept_host_loopback.index("--host-loopback") + 1],
            "allow",
        )
        self.assertEqual(readdressed[readdressed.index("--net") + 1], "10.1.0.2/24")
        self.assertTrue(
            foreign_proxy[foreign_proxy.index("--network-proxy") + 1].startswith(
                "10.0.0.9:"
            )
        )
        # The busy forward's host port was held while OpenVMM bound it.
        self.assertEqual(calls["busy"], [True])
        for command in (readdressed, foreign_proxy, busy_forward, plain_restore):
            self.assertIn("--restore-network-policy", command)
        self.assertEqual(
            Path(plain_restore[plain_restore.index("--restore-snapshot") + 1]).name,
            "plain-deny-all",
        )
        for entry in failures:
            self.assertEqual(
                entry.kwargs["forbidden_markers"],
                microvm_tests.PLATFORM_NETWORK_GUEST_MARKERS,
            )
        # A refusal must come before the clone's guest prints anything, from
        # its platform repair's report on.
        self.assertEqual(
            microvm_tests.PLATFORM_NETWORK_GUEST_MARKERS,
            (
                microvm_tests.PLATFORM_POST_RESTORE_MARKER,
                microvm_tests.PLATFORM_NETWORK_LISTENING_MARKER,
                benchmark.RESTORE_MARKER,
            ),
        )
        # A cold boot has no captured policy to replace.
        cold_boot = calls["expect_boot_failure"].call_args.args[0]
        self.assertEqual(cold_boot[:2], ["openvmm", "boot"])
        self.assertIn("--restore-network-policy", cold_boot)
        self.assertNotIn("--restore-snapshot", cold_boot)
        self.assertEqual(
            calls["expect_boot_failure"].call_args.args[3], b"--restore-snapshot"
        )

    def test_platform_network_policy_requires_what_each_policy_admits(self):
        def guest_reporting(
            replace: bytes, by: bytes
        ) -> Callable[[list[str], dict[str, int]], list[bytes]]:
            def guest(command: list[str], ports: dict[str, int]) -> list[bytes]:
                return [
                    line.replace(replace, by)
                    for line in self._simulate_platform_network_guest(command, ports)
                ]

            return guest

        for replace, by, message in (
            # A clone whose NIC changed its identity.
            (
                b"mac=52:54:00:00:00:02",
                b"mac=52:54:00:00:00:03",
                "did not report 'NVX-PLATFORM-NETWORK-IDENTITY",
            ),
            # A clone without its platform repair.
            (b"tier=platform", b"tier=workload-start", "did not report 'NVX-POST"),
            # A clone that the template's deny-all let reach a service.
            (
                b"allowed=denied other=denied",
                b"allowed=reached other=denied",
                "^platform network policy clone deny-all did not report",
            ),
        ):
            with (
                self.subTest(message=message),
                tempfile.TemporaryDirectory() as temporary,
            ):
                with self.assertRaisesRegex(RuntimeError, message):
                    self._run_platform_network_policy(
                        Path(temporary), guest=guest_reporting(replace, by)
                    )

        # The host services must receive exactly the requests that a policy
        # admits, whatever the guest reports.
        def guest_reaching_more(
            command: list[str], ports: dict[str, int]
        ) -> list[bytes]:
            lines = self._simulate_platform_network_guest(command, ports)
            with socket.create_connection(
                ("127.0.0.1", ports["other"]), timeout=5
            ) as client:
                client.sendall(b"GET /other HTTP/1.1\r\n\r\n")
                while client.recv(4096):
                    pass
            return lines

        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaisesRegex(
                RuntimeError,
                r"^platform network policy clone deny-all: the allowed and other "
                r"host services received \(0, 1\) requests, not \(0, 0\)$",
            ):
                self._run_platform_network_policy(
                    Path(temporary), guest=guest_reaching_more
                )

    def test_platform_network_refusal_rejects_an_expected_error_after_guest_entry(
        self,
    ):
        error = b"fatal error: " + microvm_tests.PLATFORM_NETWORK_BUSY_FORWARD_REJECTION

        def refuse(output: bytes) -> None:
            process = MagicMock()
            process.__enter__.return_value.wait.return_value = (
                openvmm_process.OpenvmmProcessResult(1, output)
            )
            with (
                tempfile.TemporaryDirectory() as temporary,
                patch.object(microvm_tests, "OpenvmmProcess", return_value=process),
            ):
                microvm_tests._expect_process_failure(
                    ["openvmm", "restore"],
                    Path(temporary) / "refused.log",
                    60,
                    microvm_tests.PLATFORM_NETWORK_BUSY_FORWARD_REJECTION,
                    forbidden_markers=microvm_tests.PLATFORM_NETWORK_GUEST_MARKERS,
                )

        # OpenVMM refused the restore before its guest printed anything.
        refuse(error + b"\r\n")
        # The same error after any line of a guest that ran fails the check.
        for marker in microvm_tests.PLATFORM_NETWORK_GUEST_MARKERS:
            with (
                self.subTest(marker=marker),
                self.assertRaisesRegex(
                    RuntimeError, "^invalid restore entered the guest before failing$"
                ),
            ):
                refuse(marker + b"\r\n" + error + b"\r\n")

    def test_platform_network_policy_reports_guest_failures(self):
        def failing_guest(command: list[str], _ports: dict[str, int]) -> list[bytes]:
            log_path = output_dir / "platform-network-policy-deny-all.log"
            log_path.write_bytes(
                b"NVX-PLATFORM-NETWORK-FAIL the TCP server did not listen\r\n"
            )
            raise TimeoutError("line marker was not observed")

        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            with self.assertRaises(RuntimeError) as raised:
                self._run_platform_network_policy(output_dir, guest=failing_guest)
        self.assertEqual(
            str(raised.exception),
            "platform network policy clone deny-all failed: "
            "NVX-PLATFORM-NETWORK-FAIL the TCP server did not listen; see "
            f"{output_dir / 'platform-network-policy-deny-all.log'}",
        )
        self.assertIsInstance(raised.exception.__cause__, TimeoutError)

    def test_platform_network_policy_keeps_the_template_intact(self):
        original = ("manifest", "state", "memory", None)
        changed = ("manifest", "state", "changed", None)
        for fingerprints, message in (
            # The capture, then the first clone.
            ([original, original, changed], "^clone deny-all modified the NIC"),
            # The capture and the four clones, then the refused restores.
            (
                [original] * 6 + [changed],
                "^a refused restore modified the NIC template$",
            ),
        ):
            with (
                self.subTest(message=message),
                tempfile.TemporaryDirectory() as temporary,
            ):
                with self.assertRaisesRegex(RuntimeError, message):
                    self._run_platform_network_policy(
                        Path(temporary), fingerprints=fingerprints
                    )

    def test_platform_filesystem_script_uses_the_share_that_the_restore_attaches(
        self,
    ):
        script = microvm_tests._read_script("platform-filesystem.sh")
        self.assertIn("\nmount -t virtiofs microvm /mnt/late ||", script)
        self.assertIn('seed="$(cat /mnt/late/host-seed)"', script)
        self.assertIn(
            f'[ "$seed" = {microvm_tests.PLATFORM_FILESYSTEM_HOST_SEED.decode()} ]',
            script,
        )
        self.assertIn(
            f"\nprintf {microvm_tests.PLATFORM_FILESYSTEM_GUEST_RESULT.decode()} "
            ">/mnt/late/guest-result ||",
            script,
        )
        self.assertIn(
            f'echo "{microvm_tests.PLATFORM_FILESYSTEM_FAILURE_MARKER.decode()} $1"',
            script,
        )
        self.assertTrue(
            script.endswith(
                f"\necho {microvm_tests.PLATFORM_FILESYSTEM_MARKER.decode()}\n"
            )
        )

    def _run_platform_filesystem(
        self,
        output_dir: Path,
        measure_once: MagicMock,
        *,
        fingerprints: Sequence[tuple[str, str, str, str | None]] | None = None,
        capture: Callable[..., None] | None = None,
    ) -> tuple[MagicMock, MagicMock]:
        fingerprint = ("manifest", "state", "memory", None)
        with (
            patch.object(
                microvm_tests,
                "workload_boot_command",
                return_value=["openvmm", "boot"],
            ) as workload_boot_command,
            patch.object(
                microvm_tests, "capture_snapshot", side_effect=capture
            ) as capture_snapshot,
            patch.object(microvm_tests, "measure_once", measure_once),
            patch.object(
                microvm_tests,
                "_scratch_snapshot_fingerprint",
                side_effect=fingerprints or itertools.repeat(fingerprint),
            ),
        ):
            microvm_tests.run_platform_filesystem(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
                memory_mib=128,
                timeout=60,
                output_dir=output_dir,
            )
        return workload_boot_command, capture_snapshot

    @staticmethod
    def _platform_filesystem_share(command: list[str]) -> Path:
        mount = command[command.index("--mount") + 1]
        guest_target, rest = mount.split(",", 1)
        host_path, mode = rest.rsplit(",", 1)
        if (guest_target, mode) != ("/mnt/late", "rw"):
            raise AssertionError(f"unexpected --mount {mount!r}")
        return Path(host_path)

    @classmethod
    def _platform_filesystem_measure(
        cls,
        output: bytes | None = None,
        *,
        result: bytes | None = microvm_tests.PLATFORM_FILESYSTEM_GUEST_RESULT,
        on_restore: Callable[[list[str]], None] | None = None,
    ) -> MagicMock:
        def measure(command: list[str], **kwargs: object) -> None:
            if on_restore is not None:
                on_restore(command)
            if result is not None:
                share = cls._platform_filesystem_share(command)
                (share / "guest-result").write_bytes(result)
            log_path = cast(Path, kwargs["log_path"])
            log_path.parent.mkdir(parents=True, exist_ok=True)
            log_path.write_bytes(
                output
                if output is not None
                else microvm_tests.PLATFORM_POST_RESTORE_MARKER
                + b"\r\n"
                + microvm_tests.PLATFORM_FILESYSTEM_MARKER
                + b"\r\n"
                + benchmark.RESTORE_MARKER
                + b"\r\n"
            )

        return MagicMock(side_effect=measure)

    def test_platform_filesystem_attaches_the_share_at_the_restore(self):
        def blocks(command: list[str]) -> list[str]:
            return [
                command[index + 1]
                for index, argument in enumerate(command)
                if argument == "--microvm-sandbox-block"
            ]

        seeds: list[bytes] = []

        def on_restore(command: list[str]) -> None:
            share = self._platform_filesystem_share(command)
            seeds.append((share / "host-seed").read_bytes())

        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary) / "logs"
            measure_once = self._platform_filesystem_measure(on_restore=on_restore)
            workload_boot_command, capture_snapshot = self._run_platform_filesystem(
                output_dir, measure_once
            )
            logs = sorted(path.name for path in output_dir.iterdir())

        self.assertIn("platform-filesystem", microvm_tests.MICROVM_TEST_SCENARIOS)
        self.assertIn("platform-filesystem", microvm_tests.SANDBOX_CONTROL_SCENARIOS)
        # A platform command line carries no caller tokens.
        self.assertEqual(workload_boot_command.call_args.args[5], "")
        # The capture leaves the filesystem slot dormant.
        capture_command = capture_snapshot.call_args.args[0]
        self.assertNotIn("--mount", capture_command)
        self.assertEqual(
            capture_command[capture_command.index("--snapshot-tier") + 1], "platform"
        )
        capture_blocks = blocks(capture_command)
        self.assertEqual(len(capture_blocks), 2)
        self.assertRegex(capture_blocks[0], r"^distro:file:.+,ro$")
        self.assertRegex(capture_blocks[1], r"^scratch:file:.+(?<!,ro)$")
        self.assertEqual(capture_snapshot.call_args.kwargs["processors"], 1)
        self.assertEqual(
            capture_snapshot.call_args.kwargs["post_restore_script"],
            microvm_tests._read_script("platform-filesystem.sh"),
        )

        # One clone restores the snapshot with a fresh scratch and attaches the
        # share, which holds the host's seed, to the dormant slot.
        measure_once.assert_called_once()
        restore = measure_once.call_args.args[0]
        self.assertIn("--restore-snapshot", restore)
        restore_blocks = blocks(restore)
        self.assertEqual(restore_blocks[0], capture_blocks[0])
        self.assertRegex(restore_blocks[1], r"^scratch:file:.+(?<!,ro)$")
        self.assertNotEqual(restore_blocks[1], capture_blocks[1])
        self.assertEqual(seeds, [microvm_tests.PLATFORM_FILESYSTEM_HOST_SEED])
        self.assertEqual(
            measure_once.call_args.kwargs["environment"]["OPENVMM_LOG"], "off"
        )
        self.assertTrue(measure_once.call_args.kwargs["guest_exit_prequeued"])
        self.assertEqual(
            measure_once.call_args.kwargs["failure_marker"],
            microvm_tests.PLATFORM_FILESYSTEM_FAILURE_MARKER,
        )
        self.assertEqual(logs, ["platform-filesystem-restore.log"])

    def test_platform_filesystem_requires_the_platform_repair_and_the_guest_check(
        self,
    ):
        complete = (
            microvm_tests.PLATFORM_POST_RESTORE_MARKER,
            microvm_tests.PLATFORM_FILESYSTEM_MARKER,
        )
        for missing in complete:
            output = b"".join(
                marker + b"\r\n" for marker in complete if marker != missing
            )
            with (
                self.subTest(missing=missing),
                tempfile.TemporaryDirectory() as temporary,
            ):
                with self.assertRaisesRegex(
                    RuntimeError,
                    "^platform filesystem restore did not report "
                    + re.escape(repr(missing.decode()))
                    + "$",
                ):
                    self._run_platform_filesystem(
                        Path(temporary), self._platform_filesystem_measure(output)
                    )

    def test_platform_filesystem_requires_the_guest_result_on_the_host(self):
        for result in (None, b"NVX-GUEST-TO-HOS"):
            with (
                self.subTest(result=result),
                tempfile.TemporaryDirectory() as temporary,
            ):
                with self.assertRaisesRegex(
                    RuntimeError,
                    "^platform filesystem restore did not write the guest's result$",
                ):
                    self._run_platform_filesystem(
                        Path(temporary),
                        self._platform_filesystem_measure(result=result),
                    )

    def test_platform_filesystem_reports_guest_failures(self):
        failure = benchmark.GuestFailureReported(
            "NVX-PLATFORM-FILESYSTEM-FAIL read", "tail"
        )
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(RuntimeError) as raised:
                self._run_platform_filesystem(
                    Path(temporary), MagicMock(side_effect=failure)
                )
        self.assertEqual(
            str(raised.exception),
            "platform filesystem restore: guest reported "
            "NVX-PLATFORM-FILESYSTEM-FAIL read\n"
            "--- OpenVMM output ---\ntail",
        )

    def test_platform_filesystem_keeps_the_snapshot_intact(self):
        original = ("manifest", "state", "memory", None)
        changed = ("manifest", "state", "changed", None)
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaisesRegex(
                RuntimeError,
                "^platform filesystem restore modified snapshot artifacts$",
            ):
                self._run_platform_filesystem(
                    Path(temporary),
                    self._platform_filesystem_measure(),
                    # The capture, then the clone.
                    fingerprints=[original, changed],
                )

    def test_platform_aggregate_export_script_mounts_and_checks_the_aggregate(self):
        script = microvm_tests._read_script("platform-aggregate-export.sh")
        # The clone keeps its captured command line, which discovers the slot
        # but names no filesystem, so the guest mounts the aggregate itself.
        self.assertIn(
            "\ngrep -q 'virtio_mmio.device=0x1000@0xd0001000:6' /proc/cmdline ||",
            script,
        )
        self.assertIn(
            "\nif grep -Eq '(^| )(virtfs_|nvx_share=)' /proc/cmdline;", script
        )
        self.assertIn('\nmount -t virtiofs microvm "$shares" ||', script)
        self.assertIn(r"""[ "$(ls "$shares" | tr '\n' ' ')" = "0 1 2 3 4 " ]""", script)
        # It checks every child of the aggregate, and binds a directory and a
        # file child where it needs them.
        for child in range(5):
            self.assertIn(f'"$shares/{child}', script)
        self.assertIn('\nmount --bind "$shares/0" /workspace ||', script)
        self.assertIn('\nmount --bind "$shares/3" /config/settings.json ||', script)
        self.assertIn(
            "\n    echo "
            f'"{microvm_tests.PLATFORM_AGGREGATE_FAILURE_MARKER.decode()} $1"\n',
            script,
        )
        self.assertTrue(
            script.endswith(
                f"\necho {microvm_tests.PLATFORM_AGGREGATE_MARKER.decode()}\n"
            )
        )

    def _run_platform_aggregate_export(
        self,
        output_dir: Path,
        measure_once: MagicMock,
        *,
        fingerprints: Sequence[tuple[str, str, str, str | None]] | None = None,
        refused: Callable[..., None] | None = None,
        platform: str = "linux",
    ) -> tuple[MagicMock, MagicMock, MagicMock]:
        fingerprint = ("manifest", "state", "memory", None)
        with (
            patch.object(
                microvm_tests,
                "workload_boot_command",
                return_value=["openvmm", "boot"],
            ) as workload_boot_command,
            patch.object(microvm_tests, "capture_snapshot") as capture_snapshot,
            patch.object(microvm_tests, "measure_once", measure_once),
            patch.object(
                microvm_tests,
                "_scratch_snapshot_fingerprint",
                side_effect=fingerprints or itertools.repeat(fingerprint),
            ),
            patch.object(
                microvm_tests, "_expect_process_failure", side_effect=refused
            ) as expect_failure,
            patch.object(microvm_tests.sys, "platform", platform),
        ):
            microvm_tests.run_platform_aggregate_export(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
                memory_mib=128,
                timeout=60,
                output_dir=output_dir,
            )
        return workload_boot_command, capture_snapshot, expect_failure

    @staticmethod
    def _aggregate_children(command: list[str]) -> list[tuple[str, Path, str]]:
        """Return the name, host path, and flags of each `--mount-child` of
        `command`, in order."""
        children: list[tuple[str, Path, str]] = []
        for index, argument in enumerate(command):
            if argument == "--mount-child":
                name, rest = command[index + 1].split(",", 1)
                host_path, flags = rest.split(",", 1) if "," in rest else (rest, "")
                children.append((name, Path(host_path), flags))
        return children

    @staticmethod
    def _option_values(command: list[str], option: str) -> list[str]:
        return [
            command[index + 1]
            for index, argument in enumerate(command)
            if argument == option
        ]

    @staticmethod
    def _aggregate_guest(children: list[tuple[str, Path, str]]) -> None:
        """Change the host trees as the clone's guest does through the
        aggregate."""
        roots = {name: host_path for name, host_path, _ in children}
        (roots["0"] / "from-guest").write_bytes(b"NVX-GUEST-WRITE\n")
        (roots["0"] / "guest-directory").mkdir()
        with (roots["2"] / "notes one.txt").open("ab") as notes:
            notes.write(b"NVX-GUEST-NOTE\n")
        roots["4"].write_bytes(b"NVX-GUEST-OUTPUT\n")

    @classmethod
    def _platform_aggregate_measure(
        cls,
        output: bytes | None = None,
        *,
        guest: Callable[[list[tuple[str, Path, str]]], None] | None = None,
        on_restore: Callable[[list[str]], None] | None = None,
    ) -> MagicMock:
        def measure(command: list[str], **kwargs: object) -> None:
            if on_restore is not None:
                on_restore(command)
            (guest or cls._aggregate_guest)(cls._aggregate_children(command))
            log_path = cast(Path, kwargs["log_path"])
            log_path.parent.mkdir(parents=True, exist_ok=True)
            log_path.write_bytes(
                output
                if output is not None
                else microvm_tests.PLATFORM_POST_RESTORE_MARKER
                + b"\r\n"
                + microvm_tests.PLATFORM_AGGREGATE_MARKER
                + b"\r\n"
                + benchmark.RESTORE_MARKER
                + b"\r\n"
            )

        return MagicMock(side_effect=measure)

    def test_platform_aggregate_export_attaches_an_aggregate_at_the_restore(self):
        def blocks(command: list[str]) -> list[str]:
            return self._option_values(command, "--microvm-sandbox-block")

        trees: dict[str, dict[str, bytes | None]] = {}

        def on_restore(command: list[str]) -> None:
            for name, host_path, flags in self._aggregate_children(command):
                trees[name] = (
                    {"": host_path.read_bytes()}
                    if flags.endswith("file")
                    else microvm_tests._tree_contents(host_path)
                )

        # Each refused restore requests an aggregate that a cold boot refuses
        # for the reason that the scenario expects.
        refusals: list[str] = []

        def refused(command: list[str], *args: object, **_kwargs: object) -> None:
            reason = cast(bytes, args[2])
            children = self._aggregate_children(command)
            roots = [host_path for _, host_path, _ in children]
            denied = [
                Path(path) for path in self._option_values(command, "--mount-deny")
            ]
            if reason == b"must not overlap":
                (_, first, _), (_, second, _) = children
                self.assertTrue(second.is_relative_to(first))
                refusals.append(
                    "overlap" if first.is_dir() and second.is_dir() else "file"
                )
            elif reason == b"not a regular file":
                ((_, host_path, flags),) = children
                self.assertTrue(host_path.is_dir())
                self.assertEqual(flags, "ro,file")
                refusals.append("directory-as-file")
            elif reason == b"not a plain directory":
                ((_, host_path, flags),) = children
                self.assertTrue(host_path.is_file())
                self.assertEqual(flags, "ro")
                refusals.append("file-as-directory")
            elif reason == b"not unique":
                self.assertEqual([name for name, _, _ in children], ["same", "same"])
                self.assertTrue(all(root.is_dir() for root in roots))
                refusals.append("repeated-name")
            elif reason == b"exposes the file":
                file_children = [
                    host_path
                    for _, host_path, flags in children
                    if flags.endswith("file")
                ]
                self.assertIn(denied[-1], file_children)
                refusals.append("file-policy")
            elif reason == b"outside the filesystem export roots":
                self.assertTrue(denied[-1].is_dir())
                self.assertFalse(any(denied[-1].is_relative_to(root) for root in roots))
                refusals.append("outside-policy")
            elif reason == b"absolute host path":
                self.assertFalse(denied[-1].is_absolute())
                refusals.append("relative-policy")
            else:
                self.fail(f"unexpected refusal {reason!r}")

        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary) / "logs"
            measure_once = self._platform_aggregate_measure(on_restore=on_restore)
            (
                workload_boot_command,
                capture_snapshot,
                expect_failure,
            ) = self._run_platform_aggregate_export(
                output_dir, measure_once, refused=refused
            )
            logs = sorted(path.name for path in output_dir.iterdir())

        self.assertIn("platform-aggregate-export", microvm_tests.MICROVM_TEST_SCENARIOS)
        self.assertIn(
            "platform-aggregate-export", microvm_tests.SANDBOX_CONTROL_SCENARIOS
        )
        # A platform command line carries no caller tokens, and the capture
        # leaves the filesystem slot dormant.
        self.assertEqual(workload_boot_command.call_args.args[5], "")
        capture_command = capture_snapshot.call_args.args[0]
        self.assertNotIn("--mount", capture_command)
        self.assertNotIn("--mount-aggregate", capture_command)
        self.assertEqual(
            capture_command[capture_command.index("--snapshot-tier") + 1], "platform"
        )
        capture_blocks = blocks(capture_command)
        self.assertEqual(len(capture_blocks), 2)
        self.assertRegex(capture_blocks[0], r"^distro:file:.+,ro$")
        self.assertRegex(capture_blocks[1], r"^scratch:file:.+(?<!,ro)$")
        self.assertEqual(capture_snapshot.call_args.kwargs["processors"], 1)
        self.assertEqual(
            capture_snapshot.call_args.kwargs["post_restore_script"],
            microvm_tests._read_script("platform-aggregate-export.sh"),
        )

        # One clone restores the snapshot with a fresh scratch and attaches the
        # aggregate of the filesystem-shares scenario to the dormant slot.
        measure_once.assert_called_once()
        restore = measure_once.call_args.args[0]
        self.assertIn("--restore-snapshot", restore)
        restore_blocks = blocks(restore)
        self.assertEqual(restore_blocks[0], capture_blocks[0])
        self.assertRegex(restore_blocks[1], r"^scratch:file:.+(?<!,ro)$")
        self.assertNotEqual(restore_blocks[1], capture_blocks[1])
        self.assertEqual(
            self._option_values(restore, "--mount-aggregate"), ["/run/nvx/shares"]
        )
        children = self._aggregate_children(restore)
        self.assertEqual(
            [(name, flags) for name, _, flags in children],
            [("0", "rw"), ("1", "ro"), ("2", "rw"), ("3", "ro,file"), ("4", "rw,file")],
        )
        workspace, toolcache, data, settings, output = (
            host_path for _, host_path, _ in children
        )
        self.assertEqual(
            self._option_values(restore, "--mount-deny"),
            [str(data), str(workspace / "secrets"), str(toolcache / "credentials")],
        )
        self.assertEqual(
            self._option_values(restore, "--mount-allow"),
            [str(data / "public"), str(data / "notes one.txt")],
        )
        self.assertEqual(
            self._option_values(restore, "--mount-write"), [str(data / "notes one.txt")]
        )
        self.assertNotIn("--mount-owner", restore)
        self.assertEqual(
            trees,
            {
                "0": {
                    "secrets": None,
                    "secrets/token": b"NVX-SECRET",
                    "seed": b"NVX-WORKSPACE",
                },
                "1": {
                    "credentials": None,
                    "credentials/token": b"NVX-CREDENTIAL",
                    "seed": b"NVX-TOOLCACHE",
                    "tools": None,
                    "tools/node": b"NVX-TOOL",
                },
                "2": {
                    "notes one.txt": b"NVX-NOTES\n",
                    "private.txt": b"NVX-PRIVATE",
                    "public": None,
                    "public/readme": b"NVX-PUBLIC",
                },
                "3": {"": b"NVX-SETTINGS"},
                "4": {"": b"NVX-OUTPUT\n"},
            },
        )
        # Neither file child shares its directory, which holds another file.
        self.assertEqual(settings.name, "settings.json")
        self.assertEqual(output.name, "output.txt")
        self.assertEqual(
            measure_once.call_args.kwargs["environment"]["OPENVMM_LOG"], "off"
        )
        self.assertTrue(measure_once.call_args.kwargs["guest_exit_prequeued"])
        self.assertEqual(
            measure_once.call_args.kwargs["failure_marker"],
            microvm_tests.PLATFORM_AGGREGATE_FAILURE_MARKER,
        )
        self.assertEqual(logs, ["platform-aggregate-export-restore.log"])

        # OpenVMM refuses the other restores before the guest runs.
        self.assertEqual(
            [entry.args[3] for entry in expect_failure.call_args_list],
            [reason for _, reason in microvm_tests.PLATFORM_AGGREGATE_REJECTED],
        )
        self.assertEqual(
            [entry.args[1].name for entry in expect_failure.call_args_list],
            [
                f"platform-aggregate-export-rejected-{name}.log"
                for name, _ in microvm_tests.PLATFORM_AGGREGATE_REJECTED
            ],
        )
        self.assertEqual(
            refusals,
            [
                "overlap",
                "file",
                "directory-as-file",
                "file-as-directory",
                "repeated-name",
                "file-policy",
                "outside-policy",
                "relative-policy",
            ],
        )
        for entry in expect_failure.call_args_list:
            self.assertIn("--restore-snapshot", entry.args[0])
            self.assertEqual(blocks(entry.args[0]), restore_blocks)
            self.assertNotIn("--mount-owner", entry.args[0])
            self.assertEqual(
                entry.kwargs["forbidden_markers"],
                (
                    microvm_tests.PLATFORM_POST_RESTORE_MARKER,
                    microvm_tests.PLATFORM_AGGREGATE_MARKER,
                    benchmark.RESTORE_MARKER,
                ),
            )

    def test_platform_aggregate_export_refuses_caller_ownership_on_windows(self):
        with tempfile.TemporaryDirectory() as temporary:
            _, _, expect_failure = self._run_platform_aggregate_export(
                Path(temporary), self._platform_aggregate_measure(), platform="win32"
            )
        rejected = len(microvm_tests.PLATFORM_AGGREGATE_REJECTED)
        self.assertEqual(len(expect_failure.call_args_list), rejected + 1)
        owner = expect_failure.call_args_list[-1]
        self.assertEqual(
            owner.args[3], microvm_tests.PLATFORM_AGGREGATE_OWNER_REJECTION
        )
        self.assertEqual(
            owner.args[1].name, "platform-aggregate-export-rejected-caller-owner.log"
        )
        # The restore is the clone's, with caller ownership.
        self.assertEqual(owner.args[0][-2:], ["--mount-owner", "caller"])
        self.assertEqual(len(self._aggregate_children(owner.args[0])), 5)

    def test_platform_aggregate_export_requires_the_platform_repair_and_the_check(
        self,
    ):
        complete = (
            microvm_tests.PLATFORM_POST_RESTORE_MARKER,
            microvm_tests.PLATFORM_AGGREGATE_MARKER,
        )
        for missing in complete:
            output = b"".join(
                marker + b"\r\n" for marker in complete if marker != missing
            )
            with (
                self.subTest(missing=missing),
                tempfile.TemporaryDirectory() as temporary,
            ):
                with self.assertRaisesRegex(
                    RuntimeError,
                    "^platform aggregate export restore did not report "
                    + re.escape(repr(missing.decode()))
                    + "$",
                ):
                    self._run_platform_aggregate_export(
                        Path(temporary), self._platform_aggregate_measure(output)
                    )

    def test_platform_aggregate_export_requires_exactly_the_guest_changes(self):
        def guest_then(
            change: Callable[[dict[str, Path]], object],
        ) -> Callable[[list[tuple[str, Path, str]]], None]:
            def guest(children: list[tuple[str, Path, str]]) -> None:
                self._aggregate_guest(children)
                change({name: host_path for name, host_path, _ in children})

            return guest

        def skip_write(children: list[tuple[str, Path, str]]) -> None:
            roots = {name: host_path for name, host_path, _ in children}
            (roots["0"] / "guest-directory").mkdir()

        def skip_directory(children: list[tuple[str, Path, str]]) -> None:
            roots = {name: host_path for name, host_path, _ in children}
            (roots["0"] / "from-guest").write_bytes(b"NVX-GUEST-WRITE\n")

        for name, guest, message in (
            ("missing write", skip_write, "did not write to the read-write directory"),
            (
                "missing directory",
                skip_directory,
                "did not create the guest's directory",
            ),
            (
                "denied path",
                guest_then(
                    lambda roots: (roots["0"] / "secrets" / "token").write_bytes(b"x")
                ),
                "changed a denied path",
            ),
            (
                "read-only directory",
                guest_then(lambda roots: (roots["1"] / "mutation").write_bytes(b"x")),
                "changed the read-only directory",
            ),
            (
                "data beyond the notes",
                guest_then(
                    lambda roots: (roots["2"] / "private.txt").write_bytes(b"x")
                ),
                "changed the data directory beyond its writable path",
            ),
            (
                "read-only file",
                guest_then(lambda roots: roots["3"].write_bytes(b"x")),
                "changed the shared files' directories",
            ),
            (
                "file's directory",
                guest_then(
                    lambda roots: (roots["4"].parent / "added").write_bytes(b"x")
                ),
                "changed the shared files' directories",
            ),
        ):
            with (
                self.subTest(name=name),
                tempfile.TemporaryDirectory() as temporary,
            ):
                with self.assertRaisesRegex(
                    RuntimeError, "^platform aggregate export restore " + message
                ):
                    self._run_platform_aggregate_export(
                        Path(temporary), self._platform_aggregate_measure(guest=guest)
                    )

    def test_platform_aggregate_export_reports_guest_failures(self):
        failure = benchmark.GuestFailureReported(
            "NVX-PLATFORM-AGGREGATE-FAIL mount", "tail"
        )
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(RuntimeError) as raised:
                self._run_platform_aggregate_export(
                    Path(temporary), MagicMock(side_effect=failure)
                )
        self.assertEqual(
            str(raised.exception),
            "platform aggregate export restore: guest reported "
            "NVX-PLATFORM-AGGREGATE-FAIL mount\n"
            "--- OpenVMM output ---\ntail",
        )

    def test_platform_aggregate_export_keeps_the_snapshot_and_the_trees_intact(self):
        original = ("manifest", "state", "memory", None)
        changed = ("manifest", "state", "changed", None)
        for fingerprints, message in (
            # The capture, then the clone.
            (
                [original, changed],
                "^platform aggregate export restore modified snapshot artifacts$",
            ),
            # The capture, the clone, then the refused restores.
            (
                [original, original, changed],
                "^a refused platform aggregate export restore modified snapshot "
                "artifacts$",
            ),
        ):
            with (
                self.subTest(message=message),
                tempfile.TemporaryDirectory() as temporary,
            ):
                with self.assertRaisesRegex(RuntimeError, message):
                    self._run_platform_aggregate_export(
                        Path(temporary),
                        self._platform_aggregate_measure(),
                        fingerprints=fingerprints,
                    )

        def refused_then(
            change: Callable[[dict[str, Path]], object],
        ) -> Callable[..., None]:
            def refused(command: list[str], *_args: object, **_kwargs: object) -> None:
                # A refused restore of the whole aggregate that reached the
                # guest might write to a share.
                children = self._aggregate_children(command)
                if len(children) == 5:
                    change({name: host_path for name, host_path, _ in children})

            return refused

        changes: tuple[tuple[str, Callable[[dict[str, Path]], object]], ...] = (
            (
                "workspace",
                lambda roots: (roots["0"] / "secrets" / "token").write_bytes(b"x"),
            ),
            ("data", lambda roots: (roots["2"] / "private.txt").write_bytes(b"x")),
        )
        for tree, change in changes:
            with (
                self.subTest(tree=tree),
                tempfile.TemporaryDirectory() as temporary,
            ):
                with self.assertRaisesRegex(
                    RuntimeError,
                    "^a refused platform aggregate export restore changed a shared "
                    "tree$",
                ):
                    self._run_platform_aggregate_export(
                        Path(temporary),
                        self._platform_aggregate_measure(),
                        refused=refused_then(change),
                    )

    def test_restore_downtime_shares_one_window_and_waits_for_the_restore_marker(
        self,
    ):
        clock = [100.0]
        sleeps: list[float] = []

        def sleep(seconds: float) -> None:
            sleeps.append(seconds)
            clock[0] += seconds

        fake_time = MagicMock()
        fake_time.monotonic.side_effect = lambda: clock[0]
        fake_time.sleep.side_effect = sleep
        results = [
            openvmm_process.OpenvmmProcessResult(0, _warp_probe_output(processors))
            for processors in (1, 1, 8, 8)
        ]
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            with (
                patch.object(
                    microvm_tests,
                    "workload_boot_command",
                    return_value=["openvmm", "boot"],
                ) as boot_command,
                patch.object(microvm_tests, "capture_snapshot") as capture,
                patch.object(microvm_tests, "OpenvmmProcess") as process,
                patch.object(microvm_tests, "time", fake_time),
            ):
                active = process.return_value.__enter__.return_value
                active.wait.side_effect = results
                microvm_tests.run_restore_downtime(
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "kvm",
                    memory_mib=128,
                    timeout=60,
                    output_dir=output_dir,
                )

        self.assertEqual(
            [
                (call.args[5], call.kwargs["processors"])
                for call in boot_command.call_args_list
            ],
            [
                ("quiet loglevel=0", 1),
                ("quiet loglevel=0 rcupdate.rcu_expedited=1", 1),
                ("quiet loglevel=0", 8),
                ("quiet loglevel=0 rcupdate.rcu_expedited=1", 8),
            ],
        )
        for call in capture.call_args_list:
            self.assertEqual(call.kwargs["teardown_mode"], "host-terminate")
            self.assertEqual(
                call.kwargs["post_restore_script"],
                time_abi.warp_probe_script() + time_abi.status_script(),
            )
        self.assertEqual(
            [
                (call.kwargs["online_cpus"], call.kwargs["generation"])
                for call in active.time_abi.require_restore.call_args_list
            ],
            [(1, 1), (1, 1), (8, 1), (8, 1)],
        )
        self.assertEqual(active.time_abi.require_status.call_count, 4)
        # Every capture precedes one shared 30 s window.
        self.assertEqual(sleeps, [30.0, 0.0, 0.0, 0.0])
        self.assertEqual(
            [call.args[1].name for call in process.call_args_list],
            [
                "restore-downtime-1-vcpu.log",
                "restore-downtime-1-vcpu-expedited.log",
                "restore-downtime-8-vcpu.log",
                "restore-downtime-8-vcpu-expedited.log",
            ],
        )
        for call in process.call_args_list:
            self.assertEqual(
                call.kwargs["environment"],
                {"OPENVMM_LOG": "off,openvmm_core::worker::dispatch::time_abi=info"},
            )
            self.assertIn("--restore-snapshot", call.args[0])
        self.assertEqual(
            active.wait_for_time_abi.call_args_list[0].args, ("restore", 30.0)
        )
        self.assertEqual(
            [call.args[0] for call in active.wait_for_line.call_args_list[:2]],
            [benchmark.RESTORE_MARKER, b"NVX-RESTORE-DOWNTIME-OK"],
        )
        staged = active.send_bytes.call_args_list[0].args[0].decode()
        self.assertIn("sleep 2\n", staged)
        self.assertIn("minimum=30\n", staged)

    def test_restore_downtime_rejects_a_failed_check_or_warp_probe(self):
        fatal = "[E_TSC_SYNC_UNSUPPORTED] failed to launch vm worker"
        context = "1-vcpu restore after a 0 s downtime"
        passed = openvmm_process.OpenvmmProcessResult(0, _warp_probe_output(1))
        cases = (
            (
                openvmm_process.OpenvmmProcessResult(99, b""),
                None,
                b"",
                "downtime: OpenVMM exited with status 99$",
            ),
            (
                openvmm_process.OpenvmmProcessResult(1, b""),
                fatal,
                b"",
                f"downtime: OpenVMM exited with status 1: {re.escape(fatal)}$",
            ),
            (
                openvmm_process.OpenvmmProcessResult(
                    0, _warp_probe_output(1).replace(b"conclusive=1", b"conclusive=0")
                ),
                None,
                b"",
                f"{context}: .*inconclusive",
            ),
            (passed, None, b"", f"{context}: nvx-time status did not finish"),
            (
                passed,
                None,
                _restore_status_output("mshv", 2),
                f"{context}: guest time ABI restore marker is invalid: cpus=2 is not 1",
            ),
        )
        for result, fatal_error, status, message in cases:
            with self.subTest(message=message):
                with tempfile.TemporaryDirectory() as temporary:
                    with (
                        patch.object(microvm_tests, "capture_snapshot"),
                        patch.object(microvm_tests, "OpenvmmProcess") as process,
                        patch.object(microvm_tests, "RESTORE_DOWNTIME_SECONDS", 0.0),
                    ):
                        active = process.return_value.__enter__.return_value
                        active.wait.return_value = result
                        active.time_abi = time_abi.TimeAbiMonitor()
                        active.time_abi.fatal = fatal_error
                        active.time_abi.feed(status)
                        with self.assertRaisesRegex(RuntimeError, message):
                            microvm_tests.run_restore_downtime(
                                Path("openvmm"),
                                Path("vmlinux"),
                                Path("initrd"),
                                "mshv",
                                memory_mib=128,
                                timeout=60,
                                output_dir=Path(temporary),
                            )

    def test_restore_downtime_script_rejects_stalls_and_a_frozen_clock(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        script = microvm_tests._render_script(
            "restore-downtime.sh.in", SETTLE_SECONDS="0", MIN_UPTIME_SECONDS="30"
        ).replace("nvx-exit", "nvx_exit")
        for stalls, uptime, expected in (
            ("0", "41", 0),
            ("1", "41", 98),
            ("0", "12", 99),
        ):
            with self.subTest(stalls=stalls, uptime=uptime):
                result = subprocess.run(
                    [shell],
                    input=(
                        "sleep() { :; }\n"
                        f"cat() {{ printf '%s\\n' '{stalls}'; }}\n"
                        f"cut() {{ printf '%s\\n' '{uptime}'; }}\n"
                        'nvx_exit() { printf "NVX-EXIT %s\\n" "$1"; }\n' + script
                    ),
                    text=True,
                    capture_output=True,
                    timeout=5,
                    check=False,
                )
                self.assertEqual(result.returncode, expected, result.stderr)
                self.assertIn(f"NVX-EXIT {expected}", result.stdout)
                self.assertEqual(
                    "NVX-RESTORE-DOWNTIME-OK" in result.stdout.splitlines(),
                    expected == 0,
                )

    @staticmethod
    def _exhaustive_report(
        processors: int,
        *,
        exit_status: int = 0,
        failing: tuple[str, int] | None = None,
        skip: tuple[str, int] | None = None,
    ) -> str:
        lines = [
            '/sbin/nvx-time exhaustive; echo "NVX-EXHAUSTIVE-EXIT status=$?"; '
            "echo NVX-EXHAUSTIVE-DONE; nvx-exit 0"
        ]
        failures = 0
        for cpu in range(processors):
            for check in microvm_tests.TIME_ABI_EXHAUSTIVE_CHECKS:
                if (check, cpu) == skip:
                    continue
                status, detail = "pass", ""
                if (check, cpu) == failing:
                    status, detail = "fail", "MSR 0x40000118 accepted 2"
                    failures += 1
                lines.append(
                    f"NVX-TIME-ABI-EXHAUSTIVE: v=1 check={check} cpu={cpu} "
                    f'status={status} detail="{detail}"'
                )
        lines += [
            f"NVX-TIME-ABI-EXHAUSTIVE: v=1 status={'fail' if failures else 'ok'} "
            f"cpus={processors} failures={failures}",
            f"NVX-EXHAUSTIVE-EXIT status={exit_status}",
            "NVX-EXHAUSTIVE-DONE",
        ]
        return "\r\n".join(lines) + "\r\n"

    def test_exhaustive_report_requires_every_check_on_every_cpu(self):
        report = self._exhaustive_report(4)
        microvm_tests._check_exhaustive_report(report, processors=4)
        summary = "NVX-TIME-ABI-EXHAUSTIVE: v=1 status=ok cpus=4 failures=0\r\n"
        first = "NVX-TIME-ABI-EXHAUSTIVE: v=1 check=X1 cpu=0 status=pass"
        cases = (
            (
                self._exhaustive_report(4, failing=("X4", 2), exit_status=1),
                4,
                "X4 cpu=2 failed: MSR 0x40000118 accepted 2; summary reports "
                "status=fail failures=1 for 4 vCPUs; exit status 1$",
            ),
            (
                self._exhaustive_report(4, skip=("X2", 3)),
                4,
                "X2 reported nothing for cpu 3",
            ),
            (
                self._exhaustive_report(2),
                4,
                "X1 reported nothing for cpu 2, 3;.*summary reports cpus=2 for 4 vCPUs",
            ),
            (self._exhaustive_report(4, exit_status=1), 4, "exit status 1$"),
            (report.replace(summary, ""), 4, "no summary line"),
            (
                report.replace(first, first + "\r\n" + first),
                4,
                "X1 reported cpu=0 twice",
            ),
            (
                report.replace("status=pass", "status=fail"),
                4,
                "X1 cpu=0 failed: ;.*; 16 more failed checks",
            ),
            (
                report.replace("v=1 check=X3 cpu=1", "v=2 check=X3 cpu=1"),
                4,
                "malformed",
            ),
            (report.replace("check=X3 cpu=1 ", "check=X3 "), 4, "malformed"),
            (
                report.replace("NVX-EXHAUSTIVE-EXIT status=0\r\n", ""),
                4,
                "printed no exit status",
            ),
            (
                "nvx-time: unknown command\r\nNVX-EXHAUSTIVE-EXIT status=2\r\n",
                8,
                "status 2 and reported nothing; the guest image does not provide",
            ),
        )
        for text, processors, message in cases:
            with self.subTest(message=message):
                with self.assertRaisesRegex(RuntimeError, message):
                    microvm_tests._check_exhaustive_report(text, processors=processors)

    def test_conformance_runs_nvx_time_exhaustive_at_the_requested_vcpus(self):
        with patch.object(
            microvm_tests,
            "run_guest_script",
            return_value=_guest_script_result(self._exhaustive_report(8)),
        ) as run:
            microvm_tests.run_time_abi_conformance(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "mshv",
                8,
                memory_mib=128,
                timeout=60,
                log_path=Path("time-abi-conformance.log"),
            )
        command, script, marker = run.call_args.args
        self.assertEqual(command[command.index("--processors") + 1], "8")
        self.assertEqual(
            script,
            '/sbin/nvx-time exhaustive; echo "NVX-EXHAUSTIVE-EXIT status=$?"; '
            "echo NVX-EXHAUSTIVE-DONE; nvx-exit 0\n",
        )
        self.assertEqual(marker, b"NVX-EXHAUSTIVE-DONE")
        # The console echoes the input line, which must not complete the run.
        self.assertFalse(microvm_tests.contains_output_line(script.encode(), marker))

    def test_conformance_guest_line_reports_the_check_exit_status(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        with patch.object(
            microvm_tests,
            "run_guest_script",
            return_value=_guest_script_result(self._exhaustive_report(1)),
        ) as run:
            microvm_tests.run_time_abi_conformance(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
                1,
                memory_mib=128,
                timeout=60,
                log_path=Path("time-abi-conformance.log"),
            )
        script = (
            run.call_args.args[1]
            .replace("/sbin/nvx-time", "nvx_time")
            .replace("nvx-exit", "nvx_exit")
        )
        for status in (0, 1):
            with self.subTest(status=status):
                result = subprocess.run(
                    [shell],
                    input=(
                        f'nvx_time() {{ echo "nvx-time $1"; return {status}; }}\n'
                        'nvx_exit() { printf "NVX-EXIT %s\\n" "$1"; }\n' + script
                    ),
                    text=True,
                    capture_output=True,
                    timeout=5,
                    check=False,
                )
                self.assertEqual(
                    result.stdout.splitlines(),
                    [
                        "nvx-time exhaustive",
                        f"NVX-EXHAUSTIVE-EXIT status={status}",
                        "NVX-EXHAUSTIVE-DONE",
                        "NVX-EXIT 0",
                    ],
                    result.stderr,
                )

    def test_time_abi_evidence_sums_up_the_guest_logs(self):
        runtime = (
            "NVX-TIME-ABI: v=1 phase=runtime status=synchronized generation={g} "
            "discontinuities={g} offset_ns=0 uncertainty_ns=900 rejected_samples=0 "
            "last_sample_error=none\r\n"
        )

        def line(phase: str, generation: int, elapsed: int) -> str:
            return (
                f"NVX-TIME-ABI: v=1 phase={phase} status=ok cpus=8 tsc_hz=2793437000 "
                f"lapic_hz=1000000000 generation={generation} elapsed_us={elapsed}\r\n"
            )

        def restored(elapsed: int) -> bytes:
            # A restored guest repeats its source's boot and capture lines.
            return (
                line("boot", 0, 10894)
                + line("capture", 0, 95)
                + line("restore", 1, elapsed)
                + runtime.format(g=1)
            ).encode()

        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            (output_dir / "smp-8.log").write_bytes(
                (line("boot", 0, 10894) + runtime.format(g=0)).encode()
                + _warp_probe_output(8, offset_ns=41)
            )
            (output_dir / "restore-downtime-8-vcpu.log").write_bytes(
                _warp_probe_output(8, offset_ns=3)
                + restored(1401)
                + b"NVX-RESTORE-DOWNTIME stalls=0 uptime_s=37\r\n"
            )
            (output_dir / "smp-snapshot-8-restore-0.log").write_bytes(restored(6410))
            (output_dir / "time-abi-conformance.log").write_bytes(
                (line("boot", 0, 1384) + runtime.format(g=0)).encode()
                + b'/ # /sbin/nvx-time exhaustive; echo "NVX-EXHAUSTIVE-EXIT status=$?"\r\n'
                b'NVX-TIME-ABI-EXHAUSTIVE: v=1 check=X1 cpu=0 status=pass detail=""\r\n'
                b"NVX-TIME-ABI-EXHAUSTIVE: v=1 status=ok cpus=8 failures=0\r\n"
            )
            # A phase line outside a status query is not counted.
            (output_dir / "lifecycle.log").write_text(line("boot", 0, 7))
            evidence = microvm_tests.time_abi_evidence(output_dir)
        self.assertEqual(
            evidence,
            {
                "warp_runs": "4",
                "warp_max_abs_offset_ns": "41",
                "warp_max_backward_ns": "0",
                "boot_markers": "2",
                "boot_elapsed_us": "1384-10894",
                "restore_markers": "2",
                "restore_elapsed_us": "1401-6410",
                "exhaustive": "ok/8/0",
                "downtime_stalls": "0",
            },
        )

    def test_time_abi_evidence_reports_cpu_time_against_the_budget(self):
        def query(phase: str, cpus: int, cpu_us: int | None) -> str:
            cpu = "" if cpu_us is None else f" cpu_us={cpu_us}"
            generation = 0 if phase == "boot" else 1
            return (
                f"NVX-TIME-ABI: v=1 phase={phase} status=ok cpus={cpus} "
                f"tsc_hz=2793437000 lapic_hz=1000000000 generation={generation} "
                f"elapsed_us=25000{cpu}\r\n"
                f"NVX-TIME-ABI: v=1 phase=runtime status=synchronized "
                f"generation={generation} discontinuities={generation} offset_ns=0 "
                "uncertainty_ns=900 rejected_samples=0 last_sample_error=none\r\n"
            )

        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            # KVM's restore budget at 1 CPU is its base.
            restore_budget = time_abi.CHECK_CPU_BUDGET_US["kvm"]["restore"][0]
            over = restore_budget + 500
            # Within KVM's budget (20 ms for a boot at 8 CPUs), over it for a
            # restore at 1 CPU, and an older image without cpu_us, which only
            # reports elapsed_us.
            (output_dir / "smp-8.log").write_text(query("boot", 8, 4600))
            (output_dir / "restore-1.log").write_text(query("restore", 1, over))
            (output_dir / "restore-8.log").write_text(query("restore", 8, 2000))
            (output_dir / "older.log").write_text(query("boot", 1, None))
            evidence = microvm_tests.time_abi_evidence(output_dir, "kvm")
            unknown = microvm_tests.time_abi_evidence(output_dir)
            # Each phase has its own budget: a larger restore budget absorbs
            # the 1-CPU restore.
            phased = {
                **time_abi.CHECK_CPU_BUDGET_US["kvm"],
                "restore": (over + 500, 0),
            }
            with patch.dict(time_abi.CHECK_CPU_BUDGET_US, {"kvm": phased}):
                per_phase = microvm_tests.time_abi_evidence(output_dir, "kvm")
        self.assertEqual(evidence["boot_markers"], "2")
        self.assertEqual(evidence["boot_cpu_us"], "4600-4600")
        self.assertEqual(evidence["boot_cpu_over_budget"], "0")
        self.assertEqual(evidence["restore_cpu_us"], f"2000-{over}")
        self.assertEqual(evidence["restore_cpu_over_budget"], "1")
        self.assertEqual(per_phase["restore_cpu_over_budget"], "0")
        self.assertEqual(per_phase["boot_cpu_over_budget"], "0")
        # Without a backend there is no budget to compare with.
        self.assertEqual(unknown["restore_cpu_us"], f"2000-{over}")
        self.assertNotIn("restore_cpu_over_budget", unknown)

    def test_time_abi_evidence_line_goes_to_the_log_and_the_job_summary(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary) / "out"
            output_dir.mkdir()
            empty = Path(temporary) / "empty"
            empty.mkdir()
            summary = Path(temporary) / "summary.md"
            (output_dir / "smp-2.log").write_bytes(_warp_probe_output(2, offset_ns=7))
            with (
                patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(summary)}),
                redirect_stdout(io.StringIO()) as stdout,
            ):
                line = microvm_tests.report_time_abi_evidence(
                    output_dir, backend="mshv", guest="alpine", debug_kernel=True
                )
                nothing = microvm_tests.report_time_abi_evidence(
                    empty, backend="kvm", guest="alpine", debug_kernel=False
                )
            self.assertEqual(
                line,
                "NVX-TIME-ABI-EVIDENCE: backend=mshv guest=alpine kernel=debug "
                "warp_runs=2 warp_max_abs_offset_ns=7 warp_max_backward_ns=0",
            )
            self.assertIsNone(nothing)
            self.assertEqual(stdout.getvalue(), f"{line}\n")
            self.assertEqual(summary.read_text(encoding="utf-8"), f"\n`{line}`\n")

    def test_removed_tsc_guards_stay_removed(self):
        # The warp probe replaced the restore-tsc-sync guard, its
        # clearcpuid=tsc_adjust kernel option, and the fresh-boot TSC control.
        self.assertNotIn("restore-tsc-sync", microvm_tests.MICROVM_TEST_SCENARIOS)
        for name in ("restore-tsc-sync.sh", "tsc-sync-control.sh.in"):
            self.assertFalse((microvm_tests.MICROVM_TEST_SCRIPTS_DIR / name).exists())
        for name in (
            "run_fresh_boot_tsc_control",
            "_tsc_control_verdict",
            "_host_invariant_tsc_note",
        ):
            self.assertFalse(hasattr(microvm_tests, name), name)

    def test_restore_memory_reuses_one_base_snapshot_for_all_targets(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output_dir = root / "logs"
            snapshot_memory = root / "snapshot" / "memory.bin"
            snapshot_memory.parent.mkdir()
            with snapshot_memory.open("wb") as memory:
                memory.truncate(512 * 1024 * 1024)

            def fingerprint(snapshot_path: Path) -> tuple[str, str, str]:
                return ("manifest", "state", str(snapshot_path / "memory.bin"))

            def measure(command: list[str], **kwargs: object) -> None:
                target = int(
                    command[command.index("--restore-memory") + 1].removesuffix("M")
                )
                added = (target - 512) * 1024 * 1024
                log_path = cast(Path, kwargs["log_path"])
                log_path.parent.mkdir(parents=True, exist_ok=True)
                log_path.write_bytes(
                    f"NVX-MEMORY-ONLINE-OK: added_bytes={added} "
                    "memtotal_kib=1 elapsed_us=1\n"
                    "NVX-RESTORE-MEMORY-WORKLOAD-OK\n".encode()
                )

            with (
                patch.object(
                    microvm_tests,
                    "workload_boot_command",
                    return_value=["openvmm", "boot"],
                ),
                patch.object(microvm_tests, "capture_snapshot") as capture_snapshot,
                patch.object(
                    microvm_tests, "measure_once", side_effect=measure
                ) as measure_once,
                patch.object(
                    microvm_tests,
                    "_snapshot_fingerprint",
                    side_effect=fingerprint,
                ),
                patch.object(
                    microvm_tests,
                    "require_file",
                    return_value=snapshot_memory,
                ),
            ):
                microvm_tests.run_restore_memory(
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "whp",
                    timeout=60,
                    output_dir=output_dir,
                )

        capture_command = capture_snapshot.call_args.args[0]
        self.assertEqual(
            capture_command[capture_command.index("--memory-capacity") + 1],
            "2048M",
        )
        self.assertEqual(measure_once.call_count, 3)
        self.assertTrue(
            all(
                entry.kwargs["guest_exit_prequeued"]
                for entry in measure_once.call_args_list
            )
        )
        self.assertEqual(
            [
                entry.args[0][entry.args[0].index("--restore-memory") + 1]
                for entry in measure_once.call_args_list
            ],
            ["512M", "1024M", "2048M"],
        )

    def test_runner_dispatches_selected_scenarios_once(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary) / "logs"
            args = argparse.Namespace(
                backend="whp",
                guest="alpine",
                scenario=["smp", "smp", "smp-lapic", "smp-lapic"],
                processors=[2, 2, 8],
                memory_mib=128,
                timeout=60.0,
                output_dir=output_dir,
            )

            def require(path: Path, _description: str) -> Path:
                return path

            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(
                    microvm_tests,
                    "require_file",
                    side_effect=require,
                ),
                patch.object(microvm_tests, "run_lifecycle") as run_lifecycle,
                patch.object(microvm_tests, "run_smp") as run_smp,
            ):
                self.assertEqual(microvm_tests.run(args), 0)

        run_lifecycle.assert_not_called()
        self.assertEqual(
            [entry.args[4] for entry in run_smp.call_args_list],
            [2, 8, 2, 8],
        )
        self.assertEqual(
            [entry.kwargs["log_path"].name for entry in run_smp.call_args_list],
            ["smp-2.log", "smp-8.log", "smp-lapic-2.log", "smp-lapic-8.log"],
        )
        self.assertEqual(
            [entry.kwargs["counting_lapic"] for entry in run_smp.call_args_list],
            [False, False, True, True],
        )

    def test_runner_dispatches_public_managed_exec_configuration(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "whp",
                    "--scenario",
                    "managed-exec-config",
                    "--output-dir",
                    str(output_dir),
                ]
            )

            def require(path: Path, _description: str) -> Path:
                return path

            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(microvm_tests, "run_managed_exec_configuration") as run,
            ):
                self.assertEqual(microvm_tests.run(args), 0)
            run.assert_called_once_with(
                "whp", timeout=args.timeout, output_dir=output_dir
            )

    def test_runner_dispatches_public_sandbox_lifecycle(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "mshv",
                    "--scenario",
                    "sandbox-lifecycle",
                    "--output-dir",
                    str(output_dir),
                ]
            )

            def require(path: Path, _description: str) -> Path:
                return path

            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(microvm_tests, "run_sandbox_lifecycle") as run,
                patch.object(
                    microvm_tests, "run_managed_exec_configuration"
                ) as managed_exec,
            ):
                self.assertEqual(microvm_tests.run(args), 0)
            run.assert_called_once_with(
                "mshv", timeout=args.timeout, output_dir=output_dir
            )
            managed_exec.assert_not_called()

    def test_runner_dispatches_public_sandbox_proxy_only(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "kvm",
                    "--scenario",
                    "sandbox-proxy-only",
                    "--output-dir",
                    str(output_dir),
                ]
            )

            def require(path: Path, _description: str) -> Path:
                return path

            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(microvm_tests, "run_sandbox_proxy_only") as run,
                patch.object(microvm_tests, "run_sandbox_lifecycle") as lifecycle,
            ):
                self.assertEqual(microvm_tests.run(args), 0)
            run.assert_called_once_with(
                "kvm", timeout=args.timeout, output_dir=output_dir
            )
            lifecycle.assert_not_called()

    def test_runner_dispatches_public_sandbox_ram_upper(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "whp",
                    "--scenario",
                    "sandbox-ram-upper",
                    "--output-dir",
                    str(output_dir),
                ]
            )

            def require(path: Path, _description: str) -> Path:
                return path

            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(microvm_tests, "run_sandbox_ram_upper") as run,
                patch.object(microvm_tests, "run_sandbox_lifecycle") as lifecycle,
            ):
                self.assertEqual(microvm_tests.run(args), 0)
            run.assert_called_once_with(
                "whp", timeout=args.timeout, output_dir=output_dir
            )
            lifecycle.assert_not_called()

    def test_a_default_run_runs_the_public_sandbox_scenarios(self):
        def require(path: Path, _description: str) -> Path:
            return path

        with tempfile.TemporaryDirectory() as temporary:
            args = nvx.parse_args(
                ["test-microvm", "--backend", "kvm", "--output-dir", temporary]
            )
            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(
                    microvm_tests,
                    "MICROVM_TEST_SCENARIOS",
                    (
                        "managed-exec-config",
                        "sandbox-lifecycle",
                        "sandbox-proxy-only",
                        "sandbox-ram-upper",
                        "guest-boot",
                    ),
                ),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(microvm_tests, "run_guest_boot") as guest_boot,
                patch.object(microvm_tests, "run_sandbox_lifecycle") as lifecycle,
                patch.object(microvm_tests, "run_sandbox_proxy_only") as proxy_only,
                patch.object(microvm_tests, "run_sandbox_ram_upper") as ram_upper,
                patch.object(
                    microvm_tests, "run_managed_exec_configuration"
                ) as managed_exec,
                redirect_stdout(io.StringIO()),
            ):
                self.assertEqual(microvm_tests.run(args), 0)

        guest_boot.assert_called_once()
        lifecycle.assert_called_once()
        proxy_only.assert_called_once()
        ram_upper.assert_called_once()
        managed_exec.assert_called_once()
        self.assertIn("sandbox-lifecycle", microvm_tests.SANDBOX_CONTROL_SCENARIOS)
        self.assertIn("sandbox-proxy-only", microvm_tests.SANDBOX_CONTROL_SCENARIOS)
        self.assertIn("sandbox-ram-upper", microvm_tests.SANDBOX_CONTROL_SCENARIOS)
        self.assertIn("sandbox-ram-upper", microvm_tests.PUBLIC_SANDBOX_SCENARIOS)

    def test_public_sandbox_scenarios_need_their_artifacts_before_any_runs(self):
        # Without the scratch template, a default run fails before its first
        # scenario, and says where the template comes from.
        template = microvm_tests.workload_artifact_paths()[-1][0]
        self.assertEqual(template.name, "ubuntu-smoke-scratch.ext4")

        def require(path: Path, description: str) -> Path:
            if path == template:
                raise common.ScriptError(f"{description} not found: {path}")
            return path

        with tempfile.TemporaryDirectory() as temporary:
            args = nvx.parse_args(
                ["test-microvm", "--backend", "kvm", "--output-dir", temporary]
            )
            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(
                    microvm_tests,
                    "MICROVM_TEST_SCENARIOS",
                    ("guest-boot", "sandbox-lifecycle"),
                ),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(microvm_tests, "run_guest_boot") as guest_boot,
                patch.object(microvm_tests, "run_sandbox_lifecycle") as lifecycle,
                self.assertRaisesRegex(
                    common.ScriptError,
                    r"^sandbox scratch template not found: .*; sandbox-lifecycle "
                    r"reads .*: install them with `nvx-rs download` .*"
                    r"`build-scratch-template`",
                ),
            ):
                microvm_tests.run(args)
            self.assertFalse((Path(temporary) / microvm_durations.REPORT_NAME).exists())

        guest_boot.assert_not_called()
        lifecycle.assert_not_called()

    def test_managed_executions_enter_through_the_agent_not_the_container_helpers(self):
        root = Path(__file__).resolve().parent.parent
        bootstrap = (root / "guest" / "common" / "nvx-init-agent").read_text()
        launcher = (root / "guest" / "alpine" / "nvx-container-enter").read_text()
        # The managed agent generates the workload's machine ID itself, so the
        # init agent generates one only for a one-shot workload.
        self.assertLess(
            bootstrap.index("    /sbin/nvx-managed-agent \\"),
            bootstrap.index('>"$runtime/workload-machine-id"'),
        )
        self.assertLess(
            bootstrap.index('>"$runtime/workload-machine-id"'),
            bootstrap.index("/sbin/nvx-container-launch"),
        )
        self.assertNotIn("NVX_EXEC_CONFIG_FD", launcher)
        self.assertNotIn("nvx-managed-agent", launcher)

    def test_runner_uses_ubuntu_artifact_and_default_memory(self):
        requested: list[Path] = []

        def require(path: Path, _description: str) -> Path:
            requested.append(path)
            return path

        with tempfile.TemporaryDirectory() as temporary:
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "whp",
                    "--guest",
                    "ubuntu",
                    "--scenario",
                    "guest-boot",
                    "--output-dir",
                    temporary,
                ]
            )
            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(microvm_tests, "run_guest_boot") as run_guest_boot,
            ):
                self.assertEqual(microvm_tests.run(args), 0)

        self.assertIn(
            BuildConstants.BUILD_DIR / "initramfs-ubuntu.cpio.gz",
            requested,
        )
        self.assertEqual(run_guest_boot.call_args.kwargs["memory_mib"], 512)

    def test_runner_omits_console_snapshot_from_ubuntu_defaults(self):
        def require(path: Path, _description: str) -> Path:
            return path

        with tempfile.TemporaryDirectory() as temporary:
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "whp",
                    "--guest",
                    "ubuntu",
                    "--output-dir",
                    temporary,
                ]
            )
            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(
                    microvm_tests,
                    "MICROVM_TEST_SCENARIOS",
                    ("console-snapshot", "guest-boot"),
                ),
                patch.object(
                    microvm_tests,
                    "require_file",
                    side_effect=require,
                ),
                patch.object(microvm_tests, "run_guest_boot") as guest_boot,
                patch.object(
                    microvm_tests,
                    "run_console_snapshot",
                ) as console_snapshot,
            ):
                self.assertEqual(microvm_tests.run(args), 0)

        guest_boot.assert_called_once()
        console_snapshot.assert_not_called()

    def test_runner_rejects_ubuntu_unsupported_scenarios(self):
        def require(path: Path, _description: str) -> Path:
            return path

        for scenario in (
            "console-snapshot",
            "sandbox-blocks",
            "scratch-snapshot",
            "snapshot-tiers",
        ):
            with self.subTest(scenario=scenario):
                args = nvx.parse_args(
                    [
                        "test-microvm",
                        "--backend",
                        "whp",
                        "--guest",
                        "ubuntu",
                        "--scenario",
                        scenario,
                    ]
                )
                with (
                    patch.object(
                        microvm_tests,
                        "validate_openvmm_test_backend",
                    ),
                    patch.object(
                        microvm_tests,
                        "require_file",
                        side_effect=require,
                    ),
                    self.assertRaisesRegex(
                        common.ScriptError,
                        "Ubuntu guest does not support",
                    ),
                ):
                    microvm_tests.run(args)

    def test_runner_excludes_sandbox_scenarios_without_sandbox_control(self):
        def require(path: Path, _description: str) -> Path:
            return path

        with tempfile.TemporaryDirectory() as temporary:
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "kvm",
                    "--guest",
                    "azurelinux",
                    "--output-dir",
                    temporary,
                ]
            )
            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(
                    microvm_tests,
                    "MICROVM_TEST_SCENARIOS",
                    ("sandbox-blocks", "guest-boot"),
                ),
                patch.object(
                    microvm_tests,
                    "require_file",
                    side_effect=require,
                ),
                patch.object(microvm_tests, "run_guest_boot") as guest_boot,
                patch.object(microvm_tests, "run_sandbox_blocks") as sandbox_blocks,
            ):
                self.assertEqual(microvm_tests.run(args), 0)

        guest_boot.assert_called_once()
        sandbox_blocks.assert_not_called()

    def test_runner_rejects_sandbox_scenarios_without_sandbox_control(self):
        def require(path: Path, _description: str) -> Path:
            return path

        args = nvx.parse_args(
            [
                "test-microvm",
                "--backend",
                "kvm",
                "--guest",
                "azurelinux",
                "--scenario",
                "sandbox-blocks",
            ]
        )
        with (
            patch.object(microvm_tests, "validate_openvmm_test_backend"),
            patch.object(microvm_tests, "require_file", side_effect=require),
            self.assertRaisesRegex(
                common.ScriptError,
                "Azure Linux guest does not support",
            ),
        ):
            microvm_tests.run(args)

    def test_debug_kernel_runs_the_same_host_restore_scenarios_on_vmlinux_debug(
        self,
    ):
        def require(path: Path, _description: str) -> Path:
            return path

        dispatch = {
            "smp": "run_smp",
            "smp-snapshot": "run_smp_snapshot",
            "restore-processors": "run_restore_processors",
            "restore-downtime": "run_restore_downtime",
            "snapshot-tiers": "run_snapshot_tiers",
        }
        self.assertEqual(tuple(dispatch), microvm_tests.DEBUG_KERNEL_SCENARIOS)
        with tempfile.TemporaryDirectory() as temporary:
            build = Path(temporary)
            config = build / "vmlinux-debug.config"
            config.write_text(
                "CONFIG_SOFTLOCKUP_DETECTOR=y\nCONFIG_DETECT_HUNG_TASK=y\n",
                encoding="utf-8",
            )
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "kvm",
                    "--debug-kernel",
                    "--processors",
                    "2",
                    "--output-dir",
                    str(build / "logs"),
                ]
            )
            with ExitStack() as stack:
                stack.enter_context(
                    patch.object(microvm_tests, "validate_openvmm_test_backend")
                )
                stack.enter_context(
                    patch.object(microvm_tests, "require_file", side_effect=require)
                )
                stack.enter_context(
                    patch.object(
                        microvm_tests,
                        "artifact_path",
                        side_effect=build.joinpath,
                    )
                )
                runs = {
                    scenario: stack.enter_context(patch.object(microvm_tests, runner))
                    for scenario, runner in dispatch.items()
                }
                self.assertEqual(microvm_tests.run(args), 0)
                for scenario, run in runs.items():
                    with self.subTest(scenario=scenario):
                        run.assert_called()
                        self.assertEqual(run.call_args.args[1], build / "vmlinux-debug")
                header = _read_report(build / "logs" / microvm_durations.REPORT_NAME)[0]
                self.assertEqual(
                    (header["kernel"], header["scenarios"]),
                    ("debug", list(microvm_tests.DEBUG_KERNEL_SCENARIOS)),
                )
                config.write_text("CONFIG_DETECT_HUNG_TASK=y\n", encoding="utf-8")
                with self.assertRaisesRegex(
                    common.ScriptError, "lacks CONFIG_SOFTLOCKUP_DETECTOR=y"
                ):
                    microvm_tests.run(args)

    def test_runner_passes_processor_counts_to_the_restore_scenarios(self):
        def require(path: Path, _description: str) -> Path:
            return path

        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "whp",
                    "--scenario",
                    "smp-snapshot",
                    "--scenario",
                    "restore-processors",
                    "--scenario",
                    "platform-restore-processors",
                    "--scenario",
                    "time-abi-conformance",
                    "--processors",
                    "2",
                    "4",
                    "--output-dir",
                    str(output_dir),
                ]
            )
            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(microvm_tests, "run_smp_snapshot") as smp_snapshot,
                patch.object(microvm_tests, "run_restore_processors") as processors,
                patch.object(
                    microvm_tests, "run_platform_restore_processors"
                ) as platform_processors,
                patch.object(microvm_tests, "run_time_abi_conformance") as conformance,
            ):
                self.assertEqual(microvm_tests.run(args), 0)
        for run in (smp_snapshot, processors, platform_processors):
            run.assert_called_once()
            self.assertEqual(run.call_args.args[4], [2, 4])
            self.assertEqual(run.call_args.kwargs["output_dir"], output_dir)
        # The exhaustive check runs once, on the most CPUs requested.
        conformance.assert_called_once()
        self.assertEqual(conformance.call_args.args[4], 4)
        with self.assertRaises(SystemExit), patch("sys.stderr"):
            nvx.parse_args(
                ["test-microvm", "--backend", "kvm", "--scenario", "restore-tsc-sync"]
            )

    def test_runner_dispatches_console_exit_for_each_requested_cpu_count(self):
        def require(path: Path, _description: str) -> Path:
            return path

        with tempfile.TemporaryDirectory() as temporary:
            args = argparse.Namespace(
                backend="kvm",
                guest="alpine",
                scenario=["console-exit", "console-exit"],
                processors=[1, 2, 2, 4, 8],
                memory_mib=128,
                timeout=40.0,
                output_dir=Path(temporary),
            )
            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(microvm_tests, "run_console_exit") as run,
            ):
                self.assertEqual(microvm_tests.run(args), 0)
        self.assertEqual([call.args[4] for call in run.call_args_list], [1, 2, 4, 8])

    def test_runner_flushes_each_progress_line_before_its_scenario(self):
        # CI timestamps each log line when it leaves the process, so every
        # scenario's progress line must leave stdout's buffer before the
        # scenario starts (#694).
        class FlushRecorder(io.StringIO):
            flushed = ""

            def flush(self) -> None:
                super().flush()
                self.flushed = self.getvalue()

        def require(path: Path, _description: str) -> Path:
            return path

        stdout = FlushRecorder()
        progress: list[str] = []

        def scenario(*_args: object, **_kwargs: object) -> None:
            self.assertEqual(stdout.flushed, stdout.getvalue())
            progress.append(stdout.flushed.splitlines()[-1])

        scenarios = (
            *microvm_tests.MICROVM_TEST_SCENARIOS,
            *microvm_tests.MICROVM_EXPLICIT_SCENARIOS,
        )
        with tempfile.TemporaryDirectory() as temporary, ExitStack() as stack:
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "kvm",
                    *(f"--scenario={scenario}" for scenario in scenarios),
                    "--processors",
                    "1",
                    "2",
                    "--output-dir",
                    temporary,
                ]
            )
            stack.enter_context(
                patch.object(microvm_tests, "validate_openvmm_test_backend")
            )
            stack.enter_context(
                patch.object(microvm_tests, "require_file", side_effect=require)
            )
            for name in _scenario_runners():
                stack.enter_context(
                    patch.object(microvm_tests, name, side_effect=scenario)
                )
            stack.enter_context(redirect_stdout(stdout))
            self.assertEqual(microvm_tests.run(args), 0)

        # console-exit, smp, and smp-lapic run once per processor count.
        self.assertEqual(len(progress), len(scenarios) + 3)
        for line in progress:
            self.assertRegex(line, r"^Running .+ on OpenVMM/kvm$")

    def test_runner_reports_every_scenario_execution(self):
        def require(path: Path, _description: str) -> Path:
            return path

        scenarios = (
            *microvm_tests.MICROVM_TEST_SCENARIOS,
            *microvm_tests.MICROVM_EXPLICIT_SCENARIOS,
        )
        with tempfile.TemporaryDirectory() as temporary, ExitStack() as stack:
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "whp",
                    *(f"--scenario={scenario}" for scenario in scenarios),
                    "--processors",
                    "2",
                    "1",
                    "2",
                    "--output-dir",
                    temporary,
                ]
            )
            stack.enter_context(
                patch.object(microvm_tests, "validate_openvmm_test_backend")
            )
            stack.enter_context(
                patch.object(microvm_tests, "require_file", side_effect=require)
            )
            for name in _scenario_runners():
                stack.enter_context(patch.object(microvm_tests, name))
            stack.enter_context(redirect_stdout(io.StringIO()))
            self.assertEqual(microvm_tests.run(args), 0)
            records = _read_report(Path(temporary) / microvm_durations.REPORT_NAME)

        header, setup, *executions, teardown = records
        self.assertEqual(header["record"], "run")
        self.assertEqual(
            (header["guest"], header["backend"], header["kernel"]),
            ("alpine", "whp", "default"),
        )
        self.assertEqual(header["processors"], [2, 1])
        self.assertEqual(header["scenarios"], list(scenarios))
        self.assertEqual((setup["record"], setup["outcome"]), ("setup", "passed"))
        self.assertEqual(
            (teardown["record"], teardown["outcome"]), ("teardown", "passed")
        )
        # Each execution names the processor counts that it covers, if any.
        expected: list[tuple[str, list[int] | None]] = []
        for scenario in scenarios:
            if scenario in ("console-exit", "smp", "smp-lapic"):
                expected.extend((scenario, [processors]) for processors in (2, 1))
            elif scenario in (
                "smp-snapshot",
                "restore-processors",
                "platform-restore-processors",
            ):
                expected.append((scenario, [2, 1]))
            elif scenario == "time-abi-conformance":
                expected.append((scenario, [2]))
            else:
                expected.append((scenario, None))
        self.assertCountEqual(
            [(record["scenario"], record["processors"]) for record in executions],
            expected,
        )
        for record in executions:
            self.assertEqual(
                (record["record"], record["guest"], record["backend"]),
                ("scenario", "alpine", "whp"),
            )
            self.assertEqual(
                (record["kernel"], record["outcome"], record["error"]),
                ("default", "passed", None),
            )
        # The spans follow one another, so they add up to the run's time.
        for record, following in itertools.pairwise(records[1:]):
            started = datetime.fromisoformat(record["started_at"][:-1])
            ended = datetime.fromisoformat(following["started_at"][:-1])
            self.assertAlmostEqual(
                (ended - started).total_seconds(), record["seconds"], delta=0.002
            )

    def test_runner_report_keeps_the_executions_before_a_failure(self):
        def require(path: Path, _description: str) -> Path:
            return path

        with tempfile.TemporaryDirectory() as temporary:
            args = nvx.parse_args(
                [
                    "test-microvm",
                    "--backend",
                    "mshv",
                    "--scenario",
                    "virtio-net",
                    "--scenario",
                    "lifecycle",
                    "--output-dir",
                    temporary,
                ]
            )
            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(microvm_tests, "run_lifecycle"),
                patch.object(
                    microvm_tests,
                    "run_virtio_net",
                    side_effect=RuntimeError("no virtio-net marker"),
                ),
                redirect_stdout(io.StringIO()),
                self.assertRaisesRegex(RuntimeError, "no virtio-net marker"),
            ):
                microvm_tests.run(args)
            records = _read_report(Path(temporary) / microvm_durations.REPORT_NAME)

        error = "RuntimeError: no virtio-net marker"
        self.assertEqual(
            [
                (record["record"], record.get("scenario"), record.get("outcome"))
                for record in records
            ],
            [
                ("run", None, None),
                ("setup", None, "passed"),
                ("scenario", "lifecycle", "passed"),
                ("scenario", "virtio-net", "failed"),
                ("teardown", None, "failed"),
            ],
        )
        self.assertEqual(
            [record.get("error") for record in records],
            [None, None, None, error, error],
        )

    def test_guest_runner_persists_full_output_on_failure(self):
        class FakeProcess:
            pid = 123

            def poll(self):
                return 0

            def wait(self):
                return 0

        class FakeInteraction:
            def __init__(self):
                self.process = FakeProcess()

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(b"complete raw output\n")
                chunks.put(None)

            def write_input(self, _data: bytes) -> None:
                raise AssertionError("input should not be sent without a boot marker")

            def close(self) -> None:
                pass

        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "failure.log"
            with (
                patch.object(
                    benchmark,
                    "InteractiveProcess",
                    return_value=FakeInteraction(),
                ),
                patch.object(benchmark, "terminate"),
            ):
                with self.assertRaisesRegex(RuntimeError, "boot marker"):
                    benchmark.run_guest_script(
                        ["openvmm"],
                        "echo test\n",
                        b"DONE",
                        timeout=1,
                        log_path=log_path,
                    )

            self.assertEqual(log_path.read_bytes(), b"complete raw output\n")


class DurationReportTests(unittest.TestCase):
    # 2027-01-15T08:00:00Z on the wall clock, which the tests' monotonic
    # clock reads as 100.
    EPOCH = 1_800_000_000.0
    GITHUB = {
        "GITHUB_ACTIONS": "true",
        "GITHUB_REPOSITORY": "microsoft/nvx",
        "GITHUB_WORKFLOW": "CI",
        "GITHUB_RUN_ID": "39585909588",
        "GITHUB_RUN_ATTEMPT": "2",
        "GITHUB_JOB": "test",
        "GITHUB_EVENT_NAME": "pull_request",
        "GITHUB_REF": "refs/pull/694/merge",
        "GITHUB_SHA": "919a774c6" + "0" * 31,
        "RUNNER_NAME": "azure-kvm-2",
        "RUNNER_OS": "Linux",
    }

    def _report(
        self, path: Path, readings: list[float]
    ) -> microvm_durations.DurationReport:
        """Open a report on a run that started at 100 on a clock that
        returns `readings`, the first when the report opens."""
        return microvm_durations.DurationReport(
            path,
            backend="kvm",
            guest="alpine",
            kernel="debug",
            processors=[1, 8, 1],
            scenarios=("smp", "lifecycle"),
            clock=iter(readings).__next__,
            wall_clock=lambda: self.EPOCH + readings[0] - 100,
            started=100.0,
        )

    def test_spans_cover_the_run_from_the_process_start_without_gaps(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / microvm_durations.REPORT_NAME
            written: list[int] = []
            with (
                patch.dict(os.environ, self.GITHUB),
                redirect_stdout(io.StringIO()) as stdout,
            ):
                with self._report(path, [101.5, 102, 105.25, 110, 110.5]) as report:
                    # Each record is on disk as soon as its span ends.
                    written.append(len(_read_report(path)))
                    report.start("smp", "Running smp", processors=[8, 8])
                    written.append(len(_read_report(path)))
                    report.start("lifecycle", "Running lifecycle")
                    written.append(len(_read_report(path)))
                    report.finish()
                    written.append(len(_read_report(path)))
            records = _read_report(path)
            raw = path.read_bytes()

        self.assertEqual(written, [1, 2, 3, 4])
        self.assertNotIn(b"\r", raw)
        self.assertEqual(stdout.getvalue(), "Running smp\nRunning lifecycle\n")
        dimensions = {"guest": "alpine", "backend": "kvm", "kernel": "debug"}
        passed = {"outcome": "passed", "error": None}
        self.assertEqual(
            records,
            [
                {
                    "record": "run",
                    "schema": "nvx-microvm-durations",
                    "schema_version": 1,
                    "started_at": "2027-01-15T08:00:00.000Z",
                    **dimensions,
                    "processors": [1, 8],
                    "scenarios": ["smp", "lifecycle"],
                    "github": {
                        "repository": "microsoft/nvx",
                        "workflow": "CI",
                        "run_id": "39585909588",
                        "run_attempt": "2",
                        "job": "test",
                        "event_name": "pull_request",
                        "ref": "refs/pull/694/merge",
                        "sha": self.GITHUB["GITHUB_SHA"],
                        "runner_name": "azure-kvm-2",
                        "runner_os": "Linux",
                    },
                },
                {
                    "record": "setup",
                    "started_at": "2027-01-15T08:00:00.000Z",
                    "seconds": 2.0,
                    **passed,
                },
                {
                    "record": "scenario",
                    "scenario": "smp",
                    **dimensions,
                    "processors": [8],
                    "started_at": "2027-01-15T08:00:02.000Z",
                    "seconds": 3.25,
                    **passed,
                },
                {
                    "record": "scenario",
                    "scenario": "lifecycle",
                    **dimensions,
                    "processors": None,
                    "started_at": "2027-01-15T08:00:05.250Z",
                    "seconds": 4.75,
                    **passed,
                },
                {
                    "record": "teardown",
                    "started_at": "2027-01-15T08:00:10.000Z",
                    "seconds": 0.5,
                    **passed,
                },
            ],
        )
        self.assertEqual(sum(record["seconds"] for record in records[1:]), 10.5)

    def test_a_failure_ends_its_scenario_and_the_run(self):
        for error, outcome, message in (
            (RuntimeError("no marker\nin the log"), "failed", None),
            (TimeoutError(), "failed", "TimeoutError"),
            (RuntimeError("x" * 5000), "failed", "RuntimeError: " + "x" * 986),
            (KeyboardInterrupt(), "interrupted", None),
        ):
            with (
                self.subTest(error=type(error).__name__),
                tempfile.TemporaryDirectory() as temporary,
            ):
                path = Path(temporary) / microvm_durations.REPORT_NAME
                with (
                    redirect_stdout(io.StringIO()),
                    self.assertRaises(type(error)) as raised,
                    self._report(path, [101, 102, 103, 104, 105]) as report,
                ):
                    report.start("smp", "Running smp", processors=[1])
                    report.start("lifecycle", "Running lifecycle")
                    raise error
                records = _read_report(path)
                self.assertIs(raised.exception, error)
                if message is None and outcome == "failed":
                    message = f"RuntimeError: {error}"
                self.assertEqual(
                    [
                        (record["record"], record.get("scenario"), record["seconds"])
                        for record in records[1:]
                    ],
                    [
                        ("setup", None, 2),
                        ("scenario", "smp", 1),
                        ("scenario", "lifecycle", 1),
                        ("teardown", None, 1),
                    ],
                )
                self.assertEqual(
                    [(record["outcome"], record["error"]) for record in records[1:]],
                    [
                        ("passed", None),
                        ("passed", None),
                        (outcome, message),
                        (outcome, message),
                    ],
                )
                self.assertLessEqual(
                    len(records[-1]["error"] or ""),
                    microvm_durations.ERROR_CHARACTERS,
                )

    def test_a_failure_before_the_first_scenario_ends_the_setup(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / microvm_durations.REPORT_NAME
            with (
                self.assertRaisesRegex(ValueError, "no counts"),
                self._report(path, [101, 102, 103]),
            ):
                raise ValueError("no counts")
            records = _read_report(path)
        self.assertEqual(
            [
                (record["record"], record["seconds"], record["outcome"])
                for record in records[1:]
            ],
            [("setup", 2, "failed"), ("teardown", 1, "failed")],
        )

    def test_a_run_without_scenarios_has_only_setup_and_teardown(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / microvm_durations.REPORT_NAME
            with self._report(path, [101, 102, 103]) as report:
                report.finish()
            records = _read_report(path)
        self.assertEqual(
            [
                (record["record"], record["seconds"], record["outcome"])
                for record in records[1:]
            ],
            [("setup", 2, "passed"), ("teardown", 1, "passed")],
        )

    def test_a_report_replaces_the_previous_runs_report(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / microvm_durations.REPORT_NAME
            path.write_text('{"record":"stale"}\n' * 3, encoding="utf-8")
            with self._report(path, [101, 102, 103]) as report:
                report.finish()
            records = _read_report(path)
        self.assertEqual(
            [record["record"] for record in records], ["run", "setup", "teardown"]
        )

    def test_a_report_that_cannot_end_keeps_the_runs_own_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary) / "logs"
            directory.mkdir()
            path = directory / microvm_durations.REPORT_NAME
            with (
                redirect_stdout(io.StringIO()),
                patch("sys.stderr", new_callable=io.StringIO) as stderr,
                self.assertRaisesRegex(RuntimeError, "no marker"),
                self._report(path, [101, 102, 103, 104]) as report,
            ):
                report.start("smp", "Running smp")
                shutil.rmtree(directory)
                raise RuntimeError("no marker")
            self.assertIn(f"warning: could not complete {path}: ", stderr.getvalue())
            # Without an error of the run's own, the report's error stands.
            directory.mkdir()
            with (
                self.assertRaises(OSError),
                self._report(path, [101, 102]),
            ):
                shutil.rmtree(directory)

    def test_github_identifies_the_job_only_in_github_actions(self):
        self.assertIsNone(microvm_durations.github_job({}))
        self.assertIsNone(
            microvm_durations.github_job({**self.GITHUB, "GITHUB_ACTIONS": "false"})
        )
        self.assertEqual(
            microvm_durations.github_job(
                {"GITHUB_ACTIONS": "true", "GITHUB_RUN_ID": "39585909588"}
            ),
            {
                name: "39585909588" if name == "run_id" else None
                for name, _ in common.GITHUB_VARIABLES
            },
        )

    def test_proc_start_follows_any_command_name(self):
        # The command name may hold spaces and parentheses; starttime is the
        # 22nd field.
        fields = ["S", *map(str, range(4, 22)), "4321", *["0"] * 30]
        stat = "1234 (nvx) (python 3) " + " ".join(fields) + "\n"
        self.assertEqual(microvm_durations.proc_start_seconds(stat, 100), 43.21)

    @unittest.skipUnless(sys.platform in ("linux", "win32"), "needs Linux or Windows")
    def test_process_age_counts_from_the_process_start(self):
        loaded = time.monotonic() - _LOADED
        age = microvm_durations.process_age_seconds()
        assert age is not None
        # The process started before it loaded the tests, and not long ago.
        self.assertGreaterEqual(age, loaded)
        self.assertLess(age, 24 * 3600)


def _write_durations(
    path: Path,
    backend: str,
    executions: list[tuple[str, float, str]],
    *,
    guest: str = "alpine",
    kernel: str = "default",
    processors: list[int] | None = None,
    run: str | None = "39792079980-1",
    teardown: bool = True,
) -> Path:
    """Write a scenario duration report of (scenario, seconds, outcome)
    executions, as test-microvm writes it."""
    run_id, _, attempt = (run or "").partition("-")
    records: list[dict[str, object]] = [
        {
            "record": "run",
            "schema": microvm_durations.SCHEMA,
            "schema_version": microvm_durations.SCHEMA_VERSION,
            "started_at": "2026-10-22T14:10:03.792Z",
            "guest": guest,
            "backend": backend,
            "kernel": kernel,
            "processors": [1, 2, 4, 8] if processors is None else processors,
            "scenarios": list(dict.fromkeys(name for name, _, _ in executions)),
            "github": (
                None
                if run is None
                else {"run_id": run_id, "run_attempt": attempt, "job": "test"}
            ),
        },
        {"record": "setup", "seconds": 0.25, "outcome": "passed", "error": None},
        *(
            {
                "record": "scenario",
                "scenario": name,
                "guest": guest,
                "backend": backend,
                "kernel": kernel,
                "seconds": seconds,
                "outcome": outcome,
                "error": None,
            }
            for name, seconds, outcome in executions
        ),
    ]
    if teardown:
        records.append(
            {"record": "teardown", "seconds": 0.1, "outcome": "passed", "error": None}
        )
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "".join(json.dumps(record) + "\n" for record in records), encoding="utf-8"
    )
    return path


class MicrovmShardTests(unittest.TestCase):
    """The checked-in partition of the default suite into the shards that CI
    runs in parallel, the planner that writes it, and test-microvm --shard
    (#696)."""

    def _replace(
        self, backend: str, shards: tuple[tuple[str, ...], ...]
    ) -> microvm_shards.Partition:
        """Return the checked-in partition with other shards of a backend."""
        partition = microvm_shards.load_partition()
        old = partition.backends[backend]
        return microvm_shards.Partition(
            partition.runs,
            {
                **partition.backends,
                backend: microvm_shards.BackendShards(old.seconds, shards),
            },
        )

    def _run_shard(
        self,
        output: Path,
        *options: str,
        partition: microvm_shards.Partition | None = None,
    ) -> list[str]:
        """Run test-microvm with every scenario stubbed, and return the
        scenarios that its report times."""
        args = nvx.parse_args(["test-microvm", *options, "--output-dir", str(output)])

        def require(path: Path, _description: str) -> Path:
            return path

        with ExitStack() as stack:
            stack.enter_context(
                patch.object(microvm_tests, "validate_openvmm_test_backend")
            )
            stack.enter_context(
                patch.object(microvm_tests, "require_file", side_effect=require)
            )
            runners = [
                stack.enter_context(patch.object(microvm_tests, name))
                for name in _scenario_runners()
            ]
            if partition is not None:
                stack.enter_context(
                    patch.object(
                        microvm_tests, "load_partition", return_value=partition
                    )
                )
            stack.enter_context(redirect_stdout(io.StringIO()))
            self.assertEqual(microvm_tests.run(args), 0)
            self.assertTrue(any(runner.called for runner in runners))
        records = _read_report(output / microvm_durations.REPORT_NAME)
        executed = {
            record["scenario"] for record in records if record["record"] == "scenario"
        }
        self.assertEqual(executed, set(records[0]["scenarios"]))
        return records[0]["scenarios"]

    def test_partition_puts_each_default_scenario_in_one_shard_of_each_backend(
        self,
    ):
        partition = microvm_shards.load_partition()
        suite = microvm_tests.default_suite()
        self.assertEqual(microvm_shards.partition_problems(partition, suite), [])
        self.assertEqual(tuple(partition.backends), microvm_tests.OPENVMM_TEST_BACKENDS)
        self.assertTrue(partition.runs)
        for backend, shards in partition.backends.items():
            with self.subTest(backend=backend):
                self.assertGreaterEqual(len(shards.shards), 2)
                self.assertTrue(all(shards.shards))
                self.assertEqual(
                    sorted(scenario for shard in shards.shards for scenario in shard),
                    sorted(suite),
                )
                self.assertEqual(set(shards.seconds), set(suite))

    def test_partition_is_the_balanced_plan_of_its_durations(self):
        # The file is the planner's output for its own durations: no move or
        # swap shortens a longest shard by MINIMUM_GAIN_TENTHS, and each
        # shard's duration is the sum of its scenarios'.
        partition = microvm_shards.load_partition()
        self.assertEqual(
            microvm_shards.PARTITION_PATH.read_text(encoding="utf-8"),
            microvm_shards.partition_json(partition),
        )
        self.assertEqual(
            microvm_shards.plan_partition(
                partition,
                microvm_tests.default_suite(),
                samples={},
                runs=(),
                estimates={},
                counts={},
            ),
            partition,
        )

    def test_a_default_run_selects_the_default_suite_and_the_public_scenarios(self):
        # CI runs the public sandbox scenarios beside the Ubuntu artifacts that
        # they read, in its guest and sandbox jobs, and the rest of a default
        # run in the shards of the default suite.
        with tempfile.TemporaryDirectory() as temporary:
            selected = self._run_shard(Path(temporary), "--backend", "kvm")
        self.assertEqual(selected, list(microvm_tests.MICROVM_TEST_SCENARIOS))
        self.assertEqual(
            set(selected) - set(microvm_tests.default_suite()),
            microvm_tests.PUBLIC_SANDBOX_SCENARIOS,
        )
        self.assertIn("managed-exec-config", selected)
        self.assertNotIn("smp-lapic", selected)

    def test_shards_run_each_default_scenario_once_on_each_backend(self):
        partition = microvm_shards.load_partition()
        suite = microvm_tests.default_suite()
        for backend, shards in partition.backends.items():
            ran: list[str] = []
            for index, shard in enumerate(shards.shards, 1):
                spec = f"{index}/{len(shards.shards)}"
                with (
                    self.subTest(backend=backend, shard=spec),
                    tempfile.TemporaryDirectory() as temporary,
                ):
                    selected = self._run_shard(
                        Path(temporary), "--backend", backend, "--shard", spec
                    )
                    # In suite order, as an unsharded run orders them.
                    self.assertEqual(
                        selected, [scenario for scenario in suite if scenario in shard]
                    )
                    ran.extend(selected)
            with self.subTest(backend=backend):
                self.assertEqual(sorted(ran), sorted(suite))

    def test_shard_fails_before_any_scenario_unless_each_is_in_one_shard(self):
        partition = microvm_shards.load_partition()
        first, second, *_ = partition.backends["kvm"].shards
        scenarios = microvm_tests.MICROVM_TEST_SCENARIOS
        cases: list[tuple[str, dict[str, Any], str, str]] = [
            # A new default scenario that no shard runs.
            (
                "new scenario",
                {"scenarios": (*scenarios, "brand-new")},
                "1/2",
                r"kvm: brand-new is in no shard; .*whp: brand-new is in no shard",
            ),
            # A scenario that a change removes but a shard still lists.
            (
                "removed scenario",
                {"scenarios": tuple(s for s in scenarios if s != "lifecycle")},
                "1/2",
                r"kvm: lifecycle is not a default scenario",
            ),
            (
                "scenario in two shards",
                {"partition": self._replace("kvm", (first, (*second, first[0])))},
                "2/2",
                rf"kvm: {first[0]} is in more than one shard: 1, 2",
            ),
            (
                "scenario in no shard",
                {"partition": self._replace("kvm", (first[1:], second))},
                "1/2",
                rf"kvm: {first[0]} is in no shard",
            ),
            (
                "empty shard",
                {"partition": self._replace("kvm", (first, second, ()))},
                "1/3",
                r"kvm shard 3 is empty",
            ),
            # CI runs fewer or more jobs than the backend has shards.
            ("fewer jobs", {}, "1/1", r"splits the kvm suite into 2 shards, but"),
            ("more jobs", {}, "3/3", r"this run is shard 3 of 3"),
        ]

        def require(path: Path, _description: str) -> Path:
            return path

        for name, changes, spec, message in cases:
            with (
                self.subTest(case=name),
                tempfile.TemporaryDirectory() as temporary,
                ExitStack() as stack,
            ):
                if "scenarios" in changes:
                    stack.enter_context(
                        patch.object(
                            microvm_tests,
                            "MICROVM_TEST_SCENARIOS",
                            changes["scenarios"],
                        )
                    )
                if "partition" in changes:
                    stack.enter_context(
                        patch.object(
                            microvm_tests,
                            "load_partition",
                            return_value=changes["partition"],
                        )
                    )
                stack.enter_context(
                    patch.object(microvm_tests, "validate_openvmm_test_backend")
                )
                stack.enter_context(
                    patch.object(microvm_tests, "require_file", side_effect=require)
                )
                runners = [
                    stack.enter_context(patch.object(microvm_tests, runner))
                    for runner in _scenario_runners()
                ]
                args = nvx.parse_args(
                    [
                        "test-microvm",
                        "--backend",
                        "kvm",
                        "--shard",
                        spec,
                        "--output-dir",
                        temporary,
                    ]
                )
                with self.assertRaisesRegex(common.ScriptError, message) as raised:
                    microvm_tests.run(args)
                self.assertFalse(any(runner.called for runner in runners))
                self.assertFalse(
                    (Path(temporary) / microvm_durations.REPORT_NAME).exists()
                )
                if spec != "1/1" and spec != "3/3":
                    self.assertIn("plan-microvm-shards", str(raised.exception))

    def test_shard_takes_only_the_default_alpine_suite(self):
        for options in (
            ("--scenario", "smp"),
            ("--debug-kernel",),
            ("--guest", "ubuntu"),
        ):
            with self.subTest(options=options):
                args = nvx.parse_args(
                    ["test-microvm", "--backend", "kvm", "--shard", "1/2", *options]
                )
                with (
                    patch.object(microvm_tests, "validate_openvmm_test_backend"),
                    patch.object(microvm_tests, "require_file") as require,
                    self.assertRaisesRegex(
                        common.ScriptError, "--shard runs a shard of the default"
                    ),
                ):
                    microvm_tests.run(args)
                require.assert_not_called()
        self.assertEqual(
            nvx.parse_args(
                ["test-microvm", "--backend", "whp", "--shard", "2/2"]
            ).shard,
            (2, 2),
        )
        self.assertIsNone(nvx.parse_args(["test-microvm", "--backend", "whp"]).shard)
        for value in ("0/2", "3/2", "1", "1/0", "a/b", "1/2/3", "", "01/2"):
            with (
                self.subTest(value=value),
                self.assertRaises(SystemExit),
                patch("sys.stderr", new_callable=io.StringIO),
            ):
                nvx.parse_args(["test-microvm", "--backend", "kvm", "--shard", value])

    def test_plan_balances_the_shards_of_the_previous_partition(self):
        seconds = {"a": 10.0, "b": 9.0, "c": 2.0, "d": 1.0}
        # The best change swaps a and c, which ties with swapping b and d.
        self.assertEqual(
            microvm_shards.plan_shards(seconds, 2, [("a", "b"), ("c", "d")]),
            (("b", "c"), ("a", "d")),
        )
        # The order in which durations and shards list scenarios is no input.
        self.assertEqual(
            microvm_shards.plan_shards(
                dict(reversed(seconds.items())), 2, [("b", "a"), ("d", "c")]
            ),
            (("b", "c"), ("a", "d")),
        )

    def test_plan_keeps_shards_that_no_change_shortens_by_a_second(self):
        # Moving c shortens the longest shard by 0.8 s only.
        self.assertEqual(
            microvm_shards.plan_shards(
                {"a": 10.0, "b": 9.0, "c": 0.8}, 2, [("a", "c"), ("b",)]
            ),
            (("a", "c"), ("b",)),
        )
        # By 1 s, it moves; the move comes before the swap of a and b that
        # balances the shards as well.
        self.assertEqual(
            microvm_shards.plan_shards(
                {"a": 10.0, "b": 9.0, "c": 1.2}, 2, [("a", "c"), ("b",)]
            ),
            (("a",), ("b", "c")),
        )

    def test_plan_places_scenarios_without_a_shard_longest_first(self):
        # x is no longer a scenario; c, d, and e have no shard yet.
        self.assertEqual(
            microvm_shards.plan_shards(
                {"a": 5.0, "b": 4.0, "c": 3.0, "d": 3.0, "e": 1.0},
                2,
                [("x", "a"), ("b",)],
            ),
            (("a", "d"), ("b", "c", "e")),
        )
        # A new plan starts from no shards at all.
        self.assertEqual(
            microvm_shards.plan_shards({"a": 5.0, "b": 4.0, "c": 3.0, "d": 2.0}, 2),
            (("a", "d"), ("b", "c")),
        )

    def test_plan_fills_added_shards_and_rejects_more_than_scenarios(self):
        seconds = {"a": 3.0, "b": 3.0, "c": 3.0}
        self.assertEqual(
            microvm_shards.plan_shards(seconds, 3, [("a", "b", "c")]),
            (("c",), ("a",), ("b",)),
        )
        self.assertEqual(
            microvm_shards.plan_shards(seconds, 1, [("a",), ("b",), ("c",)]),
            (("a", "b", "c"),),
        )
        for count in (0, 4):
            with (
                self.subTest(count=count),
                self.assertRaisesRegex(common.ScriptError, "cannot split 3 scenarios"),
            ):
                microvm_shards.plan_shards(seconds, count)

    def test_plan_fills_every_shard_where_no_move_shortens_one_enough(self):
        # No move of a scenario that takes no time, or less than a second,
        # into an empty shard shortens a shard by a second, yet each shard
        # takes one: the longest of the longest shard that has more than one.
        self.assertEqual(
            microvm_shards.plan_shards(
                {"a": 5.0, "b": 0.5, "c": 0.0, "d": 0.3}, 4, [("a", "b", "c", "d")]
            ),
            (("c",), ("a",), ("b",), ("d",)),
        )
        # As many shards as the checked-in partition has scenarios.
        partition = microvm_shards.load_partition()
        suite = microvm_tests.default_suite()
        planned = microvm_shards.plan_partition(
            partition,
            suite,
            samples={},
            runs=(),
            estimates={},
            counts=dict.fromkeys(partition.backends, len(suite)),
        )
        self.assertEqual(microvm_shards.partition_problems(planned, suite), [])
        for backend, shards in planned.backends.items():
            with self.subTest(backend=backend):
                self.assertEqual(
                    [len(shard) for shard in shards.shards], [1] * len(suite)
                )

    def test_reports_give_samples_of_the_default_suite_only(self):
        suite = ("console-exit", "smp", "lifecycle")
        console_exit = [("console-exit", 2.0, "passed")] * 4
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first = _write_durations(
                root / "1" / microvm_durations.REPORT_NAME,
                "kvm",
                [*console_exit, ("smp", 5.0, "passed"), ("lifecycle", 3.0, "failed")],
                run="1-1",
            )
            _write_durations(
                root / "2" / "shard" / microvm_durations.REPORT_NAME,
                "kvm",
                [
                    ("console-exit", 3.0, "passed"),
                    ("console-exit", 3.0, "passed"),
                    ("console-exit", 3.0, "passed"),
                    ("console-exit", 3.0, "passed"),
                    ("smp", 7.0, "passed"),
                    ("lifecycle", 1.5, "passed"),
                ],
                run="2-1",
            )
            # A console-exit that failed at its second processor count.
            _write_durations(
                root / "3" / microvm_durations.REPORT_NAME,
                "kvm",
                [("console-exit", 2.0, "passed"), ("console-exit", 9.0, "failed")],
                run="3-1",
            )
            # Reports of other runs than the default suite's, and of a run
            # that was killed.
            smp = [("smp", 99.0, "passed")]
            _write_durations(
                root / "debug" / microvm_durations.REPORT_NAME,
                "kvm",
                smp,
                kernel="debug",
            )
            _write_durations(
                root / "ubuntu" / microvm_durations.REPORT_NAME,
                "kvm",
                smp,
                guest="ubuntu",
            )
            _write_durations(
                root / "one-processor" / microvm_durations.REPORT_NAME,
                "kvm",
                smp,
                processors=[1],
            )
            _write_durations(
                root / "killed" / microvm_durations.REPORT_NAME,
                "kvm",
                smp,
                teardown=False,
            )
            _write_durations(
                root / "whp" / microvm_durations.REPORT_NAME,
                "whp",
                [("smp", 9.0, "passed"), ("managed-exec-config", 5.0, "passed")],
                run=None,
            )
            samples, runs = microvm_shards.read_reports(
                # The same report, found twice, counts once.
                [root, first],
                suite=suite,
                processors=(1, 2, 4, 8),
            )
            self.assertEqual(
                {
                    backend: {name: sorted(values) for name, values in found.items()}
                    for backend, found in samples.items()
                },
                {
                    "kvm": {
                        "console-exit": [8.0, 12.0],
                        "smp": [5.0, 7.0],
                        "lifecycle": [1.5],
                    },
                    "whp": {"smp": [9.0]},
                },
            )
            self.assertEqual(runs, {"1-1", "2-1"})

            (root / "other").mkdir()
            (root / "other" / microvm_durations.REPORT_NAME).write_text(
                '{"record":"run","schema":"other","schema_version":1}\n',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(common.ScriptError, "is not a scenario"):
                microvm_shards.read_reports([root], suite=suite, processors=(1,))
            with self.assertRaisesRegex(common.ScriptError, "neither a report"):
                microvm_shards.read_reports(
                    [root / "missing"], suite=suite, processors=(1,)
                )

    def test_plan_command_writes_and_updates_a_partition(self):
        scenarios = ("console-exit", "smp", "lifecycle", "managed-exec-config")

        def plan(*options: str) -> str:
            args = nvx.parse_args(
                ["plan-microvm-shards", "--partition", str(path), *options]
            )
            with redirect_stdout(io.StringIO()) as stdout:
                self.assertEqual(args.handler(args), 0)
            return stdout.getvalue()

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(microvm_tests, "MICROVM_TEST_SCENARIOS", scenarios),
        ):
            root = Path(temporary)
            path = root / "shards.json"
            reports = root / "reports"
            for run, factor in (("7-1", 1.0), ("8-2", 1.5), ("9-1", 2.0)):
                for backend in ("kvm", "mshv", "whp"):
                    _write_durations(
                        reports / run / backend / microvm_durations.REPORT_NAME,
                        backend,
                        [
                            *[("console-exit", 2.0 * factor, "passed")] * 4,
                            ("smp", 6.0 * factor, "passed"),
                            ("lifecycle", 1.0 * factor, "passed"),
                        ],
                        run=run,
                    )
            with self.assertRaisesRegex(
                common.ScriptError, "the partition has no kvm shards"
            ):
                plan("--report", str(reports))
            output = plan(
                "--report",
                str(reports),
                "--shards",
                "kvm=2",
                "--shards",
                "mshv=1",
                "--shards",
                "whp=2",
            )
            self.assertIn("kvm: 2 shards, 22.5 s in all", output)
            text = path.read_text(encoding="utf-8")
            self.assertNotIn("\r", text)
            partition = microvm_shards.load_partition(path)
            self.assertEqual(partition.runs, ("7-1", "8-2", "9-1"))
            for backend, count in (("kvm", 2), ("mshv", 1), ("whp", 2)):
                shards = partition.backends[backend]
                self.assertEqual(
                    dict(shards.seconds),
                    {"console-exit": 12.0, "smp": 9.0, "lifecycle": 1.5},
                )
                self.assertEqual(len(shards.shards), count)
            self.assertEqual(
                partition.backends["kvm"].shards,
                (("console-exit",), ("lifecycle", "smp")),
            )
            self.assertEqual(partition.backends["kvm"].shard_seconds(), (12.0, 10.5))

            # Without reports, the plan keeps the durations, runs, and shards.
            plan()
            self.assertEqual(path.read_text(encoding="utf-8"), text)
            # An estimate replaces a duration on every backend.
            output = plan("--seconds", "smp=20")
            partition = microvm_shards.load_partition(path)
            self.assertEqual(
                {
                    backend: shards.seconds["smp"]
                    for backend, shards in partition.backends.items()
                },
                {"kvm": 20.0, "mshv": 20.0, "whp": 20.0},
            )
            self.assertEqual(
                partition.backends["kvm"].shards,
                (("console-exit", "lifecycle"), ("smp",)),
            )
            self.assertIn("  lifecycle: shard 2 -> shard 1", output)

            # A scenario that nothing measures needs an estimate.
            with patch.object(
                microvm_tests, "MICROVM_TEST_SCENARIOS", (*scenarios, "brand-new")
            ):
                with self.assertRaisesRegex(
                    common.ScriptError,
                    "no report measures, and the partition has no duration of, "
                    "brand-new on kvm, brand-new on mshv, brand-new on whp",
                ):
                    plan()
                output = plan("--seconds", "brand-new=4")
                self.assertIn("  brand-new: new -> shard", output)
            for options, message in (
                (("--seconds", "unknown=1"), "outside the default suite: unknown"),
                (("--report", str(root / "empty")), "neither a report"),
            ):
                with (
                    self.subTest(options=options),
                    self.assertRaisesRegex(common.ScriptError, message),
                ):
                    plan(*options)
            (root / "empty").mkdir()
            with self.assertRaisesRegex(common.ScriptError, "no report of the default"):
                plan("--report", str(root / "empty"))
        for value in ("kvm", "kvm=0", "arm=2", "kvm=x"):
            with (
                self.subTest(shards=value),
                self.assertRaises(SystemExit),
                patch("sys.stderr", new_callable=io.StringIO),
            ):
                nvx.parse_args(["plan-microvm-shards", "--shards", value])
        for value in ("smp", "smp=-1", "=2", "smp=x", "smp=inf", "smp=nan"):
            with (
                self.subTest(seconds=value),
                self.assertRaises(SystemExit),
                patch("sys.stderr", new_callable=io.StringIO),
            ):
                nvx.parse_args(["plan-microvm-shards", "--seconds", value])

    def test_partition_rejects_malformed_files(self):
        valid = json.loads(microvm_shards.PARTITION_PATH.read_text(encoding="utf-8"))
        cases: list[tuple[str, str, str]] = [
            ("not JSON", "{", "is not valid JSON"),
            (
                "duplicate key",
                '{"schema": 1, "schema": 2}',
                "duplicate JSON property: schema",
            ),
            (
                "other schema",
                json.dumps({**valid, "schema_version": 2}),
                "schema version 1",
            ),
            (
                "malformed shard",
                json.dumps(
                    {
                        **valid,
                        "backends": {
                            **valid["backends"],
                            "kvm": {
                                **valid["backends"]["kvm"],
                                "shards": [{"seconds": 1, "scenarios": [1]}],
                            },
                        },
                    }
                ),
                "malformed partition of kvm",
            ),
            (
                "negative duration",
                json.dumps(
                    {
                        **valid,
                        "backends": {
                            **valid["backends"],
                            "whp": {
                                **valid["backends"]["whp"],
                                "seconds": {"smp": -1},
                            },
                        },
                    }
                ),
                "malformed partition of whp",
            ),
        ]
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "shards.json"
            with self.assertRaisesRegex(common.ScriptError, "does not exist"):
                microvm_shards.load_partition(path)
            for name, text, message in cases:
                with self.subTest(case=name):
                    path.write_text(text, encoding="utf-8")
                    with self.assertRaisesRegex(common.ScriptError, message):
                        microvm_shards.load_partition(path)


if __name__ == "__main__":
    unittest.main()
