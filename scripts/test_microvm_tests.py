#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import argparse
import ast
import inspect
import io
import json
import os
import queue
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Callable
from contextlib import ExitStack, redirect_stdout
from pathlib import Path
from typing import cast
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import (  # noqa: E402
    benchmark,
    common,
    control_session,
    microvm_tests,
    openvmm_process,
    time_abi,
)
from nvx_tools.build_constants import (  # noqa: E402
    BuildConstants,
)


def _posix_shell() -> str | None:
    shell = shutil.which("sh")
    if shell is None:
        git = shutil.which("git")
        git_shell = Path(git).parent.parent / "bin" / "sh.exe" if git else None
        if git_shell is not None and git_shell.is_file():
            shell = str(git_shell)
    return shell


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
) -> MagicMock:
    def measure(command: list[str], **kwargs: object) -> None:
        target = _restore_target(command)
        online = 1 if target is None else target
        vp_count = 8
        if mshv_prefix and backend == "mshv" and target is not None:
            vp_count = target
        log_path = cast(Path, kwargs["log_path"])
        log_path.parent.mkdir(parents=True, exist_ok=True)
        log_path.write_bytes(
            f"NVX-RESTORE-PROCESSORS-OK count={online}\r\n".encode()
            + _warp_probe_output(online, offset_ns=warp_offset_ns)
            # The snapshot's guest booted with one online CPU.
            + _restore_status_output(backend, online, boot_cpus=1)
            + benchmark.RESTORE_MARKER
            + b"\r\n"
            + _vp_binding_profile(list(range(vp_count)))
        )

    return MagicMock(side_effect=measure)


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


class ControlSessionTests(unittest.TestCase):
    def test_named_pipe_connect_retries_transient_invalid_argument(self):
        error = OSError(control_session.errno.EINVAL, "Invalid argument")
        with (
            patch.object(
                control_session.os, "open", side_effect=[error, 123]
            ) as open_pipe,
            patch.object(
                control_session.time,
                "monotonic",
                side_effect=[0.0, 0.0],
            ),
            patch.object(control_session.time, "sleep") as sleep,
        ):
            stream = control_session._NamedPipeStream.connect(
                Path(r"\\.\pipe\nvx-test"),
                1.0,
            )

        self.assertEqual(stream._fd, 123)
        self.assertEqual(open_pipe.call_count, 2)
        sleep.assert_called_once_with(0.025)


class MicrovmTests(unittest.TestCase):
    def test_host_loopback_listener_pair_retries_protocol_port_conflict(self):
        first_udp = MagicMock()
        first_udp.getsockname.return_value = ("127.0.0.1", 50000)
        first_tcp = MagicMock()
        first_tcp.bind.side_effect = PermissionError("TCP port is excluded")
        second_udp = MagicMock()
        second_udp.getsockname.return_value = ("127.0.0.1", 50001)
        second_tcp = MagicMock()

        with patch.object(
            microvm_tests.socket,
            "socket",
            side_effect=[first_udp, first_tcp, second_udp, second_tcp],
        ) as create_socket:
            tcp_listener, udp_listener = microvm_tests._bind_tcp_udp_listener_pair(
                5.0, microvm_tests.NETWORK_NEGATIVE_OBSERVATION_TIMEOUT_SECONDS
            )

        self.assertIs(tcp_listener, second_tcp)
        self.assertIs(udp_listener, second_udp)
        self.assertEqual(create_socket.call_count, 4)
        first_udp.bind.assert_called_once_with(("127.0.0.1", 0))
        first_tcp.bind.assert_called_once_with(("0.0.0.0", 50000))
        first_tcp.close.assert_called_once_with()
        first_udp.close.assert_called_once_with()
        second_udp.bind.assert_called_once_with(("127.0.0.1", 0))
        second_tcp.bind.assert_called_once_with(("0.0.0.0", 50001))
        second_tcp.listen.assert_called_once_with(1)
        second_tcp.settimeout.assert_called_once_with(5.0)
        second_udp.settimeout.assert_called_once_with(
            microvm_tests.NETWORK_NEGATIVE_OBSERVATION_TIMEOUT_SECONDS
        )

    def test_host_loopback_rejections_cover_generic_allow_and_explicit_denial(self):
        with tempfile.TemporaryDirectory() as temporary:
            with patch.object(microvm_tests, "OpenvmmProcess") as process:
                process.return_value.__enter__.return_value.wait.side_effect = [
                    openvmm_process.OpenvmmProcessResult(2, message)
                    for message in (
                        b"does not support generic host-loopback connectivity",
                        b"does not support generic host-loopback connectivity",
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
        for command in commands[:2]:
            self.assertNotIn("--host-loopback-forward", command)
            self.assertIn("--pidfile", command)
        self.assertIn("--network-proxy", commands[1])
        self.assertIn("--host-loopback-forward", commands[2])

    def test_host_loopback_rejection_requires_diagnostic_and_no_boot(self):
        diagnostic = b"does not support generic host-loopback connectivity"
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
                        if line.strip().startswith("nc -u")
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
            with self.subTest(mode=mode, status=status):
                script = microvm_tests._render_script(
                    "host-loopback-policy.sh.in",
                    MODE=mode,
                    GATEWAY_IPV4="10.0.0.1",
                    GENERAL_PORT="8444",
                    PROXY_PORT="8443",
                    GUEST_PORT="0",
                ).replace("nvx-exit", "nvx_exit")
                result = subprocess.run(
                    [shell, "-s"],
                    input=(
                        f"nc() {{ return {status}; }}\n"
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

    def test_host_loopback_script_probes_udp_on_proxy_and_general_ports(self):
        script = microvm_tests._render_script(
            "host-loopback-policy.sh.in",
            MODE="deny",
            GATEWAY_IPV4="10.0.0.1",
            GENERAL_PORT="8444",
            PROXY_PORT="8443",
            GUEST_PORT="0",
        )
        self.assertIn("nc -u -w 1 10.0.0.1 8443", script)
        self.assertIn("nc -u -w 1 10.0.0.1 8444", script)
        self.assertIn("http://10.0.0.1:8443/proxy", script)

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
        self.assertNotIn("nvx-port-io", snapshot)
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
        # Capture steps 1 to 3 precede the barriers and the request.
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
        # The managed lifecycle, the console-snapshot capture and restore, and
        # the snapshot-tier capture, restore, and gate-timeout restore.
        self.assertEqual(len(connects), 6)
        for keywords in connects:
            self.assertRegex(
                keywords.get("monitor", ""), r"^TimeAbiMonitor\(\w*command\)$"
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
        self.assertTrue(script.startswith(time_abi.status_script()))
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
            [shell, "-c", script.removeprefix(time_abi.status_script())],
            capture_output=True,
            check=True,
        ).stdout
        lines = answers.splitlines()
        self.assertEqual(lines[-1], microvm_tests.CANCELED_CAPTURE_DONE_MARKER)
        self.assertTrue(lines[0].startswith(microvm_tests.CANCELED_CAPTURE_RCU_PREFIX))

    def test_canceled_capture_requires_generation_zero_and_no_suppression(self):
        command = ["openvmm", "--hypervisor", "kvm", "--kernel", "vmlinux"]
        echoed = (
            b'~ # echo "NVX-""CANCELED-CAPTURE-RCU $(cat /sys/module/rcupdate/'
            b'parameters/rcu_cpu_stall_suppress)"; echo "NVX-""CANCELED-CAPTURE-DONE"\r\n'
        )
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
        self.assertLess(
            runner.index("console.wait_for_time_abi_status(STATUS_TIMEOUT_SECONDS)"),
            runner.index('console.send_bytes(b"Z")\n                restored = '),
        )

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
                microvm_tests, "run_guest_script", return_value={"text": text}
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
                    microvm_tests, "run_guest_script", return_value={"text": text}
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
            microvm_tests, "run_guest_script", return_value={"text": text}
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
        passing = (
            f"NVX-TIME-ABI: v=1 phase=boot status=ok cpus=4 {rates} "
            "generation=0 elapsed_us=2390\r\n"
            "NVX-TIME-ABI: v=1 phase=runtime status=synchronized generation=0 "
            "discontinuities=0 offset_ns=-1200 uncertainty_ns=900 "
            "rejected_samples=0 last_sample_error=none\r\n"
            "NVX-TIME-STATUS-EXIT status=0\r\n"
            "NVX-SMP-LAPIC-COUNTING-OK\r\n"
            "NVX-SMP-PROBE-OK\r\n" + _warp_probe_output(4).decode()
        )

        def run_smp_lapic(text: str) -> MagicMock:
            with patch.object(
                microvm_tests, "run_guest_script", return_value={"text": text}
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

        command, script, marker = run_smp_lapic(passing).call_args.args
        self.assertEqual(command[command.index("--cmdline") + 1], "quiet loglevel=0")
        self.assertTrue(script.startswith(microvm_tests.counting_lapic_script(4)))
        self.assertIn(benchmark.smp_probe_script(4, exit_guest=False), script)
        self.assertEqual(marker, time_abi.WARP_PROBE_COMPLETION_MARKER)
        for text, message in (
            (
                passing.replace("NVX-SMP-LAPIC-COUNTING-OK\r\n", ""),
                "without the counting-LAPIC marker",
            ),
            (passing.replace("cpus=4 tsc_hz", "cpus=2 tsc_hz"), "cpus=2 is not 4"),
        ):
            with (
                self.subTest(message=message),
                self.assertRaisesRegex(RuntimeError, message),
            ):
                run_smp_lapic(text)

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

    def test_bounded_egress_acceptance_policy_lowers_ranges_and_exclusions(self):
        policy = microvm_tests._bounded_egress_policy(
            "192.0.2.1",
            (21001, 21002, 21003),
            (22001, 22002, 22003),
        )

        self.assertIn("192.0.2.0/24:tcp:21001", policy.allow)
        self.assertIn("192.0.2.0/24:tcp:21003", policy.allow)
        self.assertIn("192.0.2.0/24:udp:22001", policy.allow)
        self.assertIn("192.0.2.0/24:udp:22003", policy.allow)
        self.assertIn("192.0.2.0/24:tcp:21002", policy.deny)
        self.assertIn("192.0.2.0/24:udp:22002", policy.deny)
        self.assertNotIn("192.0.2.0/24:tcp:21000", policy.allow)
        self.assertNotIn("192.0.2.0/24:tcp:21004", policy.allow)

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
                        microvm_tests.threading,
                        "Thread",
                        side_effect=ImmediateThread,
                    ),
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
                    nvx_path = str(Path(microvm_tests.__file__).parents[1] / "nvx.py")
                    self.assertEqual(
                        run_guest_script.call_args.args[0],
                        [
                            sys.executable,
                            nvx_path,
                            "run",
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
                    self.assertEqual(results["interface"], "nvx.py run")
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
        self.assertEqual(len(values), 4)
        self.assertTrue(values[0].startswith("distro:file:"))
        self.assertTrue(values[1].startswith("runtime:file:"))
        self.assertTrue(values[2].startswith("custom:file:"))
        self.assertTrue(values[3].startswith("scratch:file:"))
        self.assertTrue(all(value.endswith(",ro") for value in values[:3]))
        self.assertFalse(values[3].endswith(",ro"))
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
            return_value={"text": self._exhaustive_report(8)},
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
            return_value={"text": self._exhaustive_report(1)},
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
                config.write_text("CONFIG_DETECT_HUNG_TASK=y\n", encoding="utf-8")
                with self.assertRaisesRegex(
                    common.ScriptError, "lacks CONFIG_SOFTLOCKUP_DETECTOR=y"
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
                patch.object(microvm_tests, "run_time_abi_conformance") as conformance,
            ):
                self.assertEqual(microvm_tests.run(args), 0)
        for run in (smp_snapshot, processors):
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


if __name__ == "__main__":
    unittest.main()
