#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import argparse
import queue
import shutil
import socket
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import cast
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import (  # noqa: E402
    benchmark,
    common,
    control_session,
    microvm_tests,
    openvmm_process,
)


def _posix_shell() -> str | None:
    shell = shutil.which("sh")
    if shell is None:
        git = shutil.which("git")
        git_shell = Path(git).parent.parent / "bin" / "sh.exe" if git else None
        if git_shell is not None and git_shell.is_file():
            shell = str(git_shell)
    return shell


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

    def test_snapshot_restore_uses_batched_port_io_and_zero_expansion_path(self):
        snapshot = (
            Path(__file__).parents[1] / "guest" / "common" / "nvx-snapshot"
        ).read_text(encoding="utf-8")

        self.assertIn(
            '/sbin/nvx-port-io read-restore-packet 233 234 "$restore_packet"',
            snapshot,
        )
        self.assertIn(
            "generation_id=$(/sbin/nvx-port-io read-generation-id 233 234)",
            snapshot,
        )
        self.assertIn(
            'generation_id=$(/sbin/nvx-reseed "$entropy" "$generation_id")',
            snapshot,
        )
        self.assertIn(
            '/sbin/nvx-reseed --generation-only "$entropy" "$generation_id"',
            snapshot,
        )
        self.assertIn('export NVX_VM_GENERATION_ID="$generation_id"', snapshot)
        self.assertNotIn("dd if=/dev/port", snapshot)
        self.assertNotIn("dd of=/dev/port", snapshot)
        self.assertIn('[ "$range_count" -eq 0 ]', snapshot)
        self.assertIn("RESTORE_MEMORY_EXPANSION_AVAILABLE=16", snapshot)
        self.assertIn(
            '"NVX-MEMORY-ONLINE-OK: added_bytes=0 '
            'memtotal_kib=$memtotal_kib elapsed_us=0"',
            snapshot,
        )
        zero_expansion_fast_path = snapshot.index(
            "[ $((restore_status & RESTORE_MEMORY_EXPANSION_AVAILABLE)) -eq 0 ]"
        )
        packet_restore = snapshot.index("    post_restore\n")
        self.assertLess(zero_expansion_fast_path, packet_restore)

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

    def test_snapshot_core_script_selects_backend_clocksource(self):
        kvm = microvm_tests._snapshot_core_script("kvm")
        whp = microvm_tests._snapshot_core_script("whp")
        mshv = microvm_tests._snapshot_core_script("mshv")

        self.assertIn("echo kvm-clock", kvm)
        self.assertIn('current_clocksource)" != tsc-early', whp)
        self.assertNotIn("@SELECT_CLOCKSOURCE@", mshv)
        self.assertIn("/sbin/nvx-reseed", mshv)
        self.assertIn("/sbin/nvx-reseed --sample", mshv)
        self.assertIn("NVX-SNAPSHOT-GENERATION-ID-", mshv)
        self.assertIn("NVX-SNAPSHOT-UUID-", mshv)
        self.assertIn("NVX-SNAPSHOT-TEMP-ID-", mshv)

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

    def test_snapshot_core_whp_waits_for_stable_clocksource(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        clocksource_script = microvm_tests._snapshot_core_script("whp").split(
            "generation_id_before=", 1
        )[0]
        for ready_after, stable_source, expected_returncode, expected_waits in (
            (0, "tsc", 0, 0),
            (2, "refined-jiffies", 0, 2),
            (101, "tsc", 46, 100),
        ):
            with self.subTest(
                ready_after=ready_after,
                stable_source=stable_source,
            ):
                result = subprocess.run(
                    [shell, "-s"],
                    input=(
                        "clock_waits=0\n"
                        "cat() {\n"
                        f'    if [ "$clock_waits" -ge {ready_after} ]; then\n'
                        f"        echo {stable_source}\n"
                        "    else\n"
                        "        echo tsc-early\n"
                        "    fi\n"
                        "}\n"
                        "sleep() { clock_waits=$((clock_waits + 1)); }\n"
                        "nvx_exit() {\n"
                        '    echo "NVX-CLOCKSOURCE-WAITS-$clock_waits"\n'
                        '    exit "$1"\n'
                        "}\n"
                        + clocksource_script.replace("nvx-exit", "nvx_exit")
                        + 'echo "NVX-CLOCKSOURCE-WAITS-$clock_waits"\n'
                    ),
                    text=True,
                    capture_output=True,
                    timeout=5,
                    check=False,
                )

                self.assertEqual(
                    result.returncode,
                    expected_returncode,
                    result.stdout + result.stderr,
                )
                self.assertIn(
                    f"NVX-CLOCKSOURCE-WAITS-{expected_waits}\n", result.stdout
                )
                if expected_returncode:
                    self.assertIn("NVX-SNAPSHOT-CORE-FAIL code=46", result.stdout)

    def test_snapshot_core_waits_for_no_destination_marker_before_exit(self):
        events: list[tuple[str, bytes | str | None]] = []
        marker = b"NVX-SNAPSHOT-NO-DESTINATION-OK"

        class StopAfterNoDestination(Exception):
            pass

        class FakeProcess:
            def __enter__(self):
                return self

            def __exit__(
                self,
                _exception_type: type[BaseException] | None,
                _exception: BaseException | None,
                _traceback: object | None,
            ) -> None:
                return None

            def wait_for(self, expected: bytes, _timeout: float) -> None:
                events.append(("wait_for", expected))

            def send_line(self, line: str) -> None:
                events.append(("send_line", line))

            def wait(self, _timeout: float) -> openvmm_process.OpenvmmProcessResult:
                events.append(("wait", None))
                return openvmm_process.OpenvmmProcessResult(0, marker + b"\n")

        with (
            patch.object(
                microvm_tests, "workload_boot_command", return_value=["openvmm"]
            ),
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
                ("wait_for", marker),
                ("send_line", "nvx-exit 0"),
                ("wait", None),
            ],
        )

    def test_snapshot_marker_parsers_require_single_well_formed_values(self):
        output = b"PREFIX-12\r\nPAIR-4-5\n"
        self.assertEqual(
            microvm_tests._single_marker_value(output, b"PREFIX-"),
            b"12",
        )
        self.assertEqual(
            microvm_tests._parse_marker_pair(output, b"PAIR-"),
            (4, 5),
        )
        with self.assertRaisesRegex(RuntimeError, "exactly one"):
            microvm_tests._single_marker_value(b"X-1\nX-2\n", b"X-")
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
            patch.object(microvm_tests, "run_guest_script") as run_guest_script,
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
        smp_probe_script.assert_called_once_with(4)
        run_guest_script.assert_called_once_with(
            ["openvmm", "boot"],
            "probe\n",
            benchmark.SMP_PROBE_COMPLETION_MARKER,
            timeout=90,
            log_path=Path("smp-4.log"),
        )

    def test_smp_lapic_exercises_counting_timer_without_weakening_probe(self):
        with patch.object(microvm_tests, "run_guest_script") as run:
            microvm_tests.run_smp(
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
                4,
                memory_mib=128,
                timeout=60,
                log_path=Path("smp-lapic-4.log"),
                force_lapic_timer=True,
            )
        command, script, marker = run.call_args.args
        self.assertEqual(
            command[command.index("--cmdline") + 1],
            "quiet loglevel=0 lapic=notscdeadline",
        )
        self.assertEqual(script, benchmark.smp_probe_script(4))
        self.assertEqual(marker, benchmark.SMP_PROBE_COMPLETION_MARKER)

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

    def test_smp_snapshot_reruns_probe_and_checks_two_restores(self):
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
                ),
                patch.object(microvm_tests, "capture_snapshot") as capture_snapshot,
                patch.object(microvm_tests, "measure_once") as measure_once,
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
                    memory_mib=128,
                    timeout=60,
                    output_dir=output_dir,
                )

        self.assertEqual(capture_snapshot.call_args.kwargs["processors"], 2)
        self.assertEqual(
            capture_snapshot.call_args.kwargs["post_restore_script"],
            "probe\n",
        )
        self.assertEqual(measure_once.call_count, 2)
        self.assertTrue(
            all(
                entry.kwargs["guest_exit_prequeued"]
                for entry in measure_once.call_args_list
            )
        )
        self.assertTrue(
            all(
                entry.kwargs["marker"] == benchmark.RESTORE_MARKER
                for entry in measure_once.call_args_list
            )
        )

    def test_restore_processors_uses_capacity_eight_and_each_target(self):
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary) / "logs"
            with (
                patch.object(
                    microvm_tests,
                    "workload_boot_command",
                    return_value=["openvmm", "boot"],
                ) as workload_boot_command,
                patch.object(microvm_tests, "capture_snapshot") as capture_snapshot,
                patch.object(microvm_tests, "measure_once") as measure_once,
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

        self.assertEqual(workload_boot_command.call_args.kwargs["processors"], 8)
        self.assertEqual(
            workload_boot_command.call_args.args[5], "quiet loglevel=0 maxcpus=1"
        )
        self.assertEqual(capture_snapshot.call_args.kwargs["processors"], 1)
        self.assertEqual(measure_once.call_count, 4)
        self.assertTrue(
            all(
                entry.kwargs["guest_exit_prequeued"]
                for entry in measure_once.call_args_list
            )
        )
        self.assertEqual(
            [entry.kwargs["marker"] for entry in measure_once.call_args_list],
            [
                b"NVX-RESTORE-PROCESSORS-OK count=1",
                b"NVX-RESTORE-PROCESSORS-OK count=2",
                b"NVX-RESTORE-PROCESSORS-OK count=4",
                b"NVX-RESTORE-PROCESSORS-OK count=8",
            ],
        )

    def test_restore_tsc_sync_forces_linux_warp_check_and_keeps_failure_guard(self):
        for backend in ("kvm", "mshv", "whp"):
            with self.subTest(backend=backend):
                with tempfile.TemporaryDirectory() as temporary:
                    with (
                        patch.object(microvm_tests, "capture_snapshot") as capture,
                        patch.object(microvm_tests, "measure_once") as measure,
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
                            backend,
                            [1, 2, 4, 8],
                            memory_mib=128,
                            timeout=60,
                            output_dir=Path(temporary),
                            check_tsc_sync=True,
                        )
                command = capture.call_args.args[0]
                self.assertEqual(
                    command[command.index("--cmdline") + 1],
                    "quiet loglevel=0 maxcpus=1 clearcpuid=tsc_adjust",
                )
                script = capture.call_args.kwargs["post_restore_script"]
                self.assertTrue(
                    script.startswith(microvm_tests._read_script("restore-tsc-sync.sh"))
                )
                self.assertTrue(
                    script.endswith(microvm_tests._read_script("restore-processors.sh"))
                )
                self.assertEqual(capture.call_args.kwargs["processors"], 1)
                self.assertEqual(measure.call_count, 4)
                self.assertEqual(
                    [
                        call.args[0][call.args[0].index("--restore-processors") + 1]
                        for call in measure.call_args_list
                    ],
                    ["1", "2", "4", "8"],
                )

    def test_restore_tsc_sync_rejects_an_ineffective_cpu_feature_mask(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        script = microvm_tests._read_script("restore-tsc-sync.sh")
        for cpuinfo, status, expected in (
            ("flags : tsc constant_tsc rdtscp", 0, 0),
            ("flags : tsc tsc_adjust constant_tsc", 0, 96),
            ("flags : tsc tsc_adjust", 0, 96),
            ("", 1, 1),
        ):
            with self.subTest(cpuinfo=cpuinfo, status=status):
                result = subprocess.run(
                    [shell],
                    input=(
                        f"cat() {{ printf '%s\\n' '{cpuinfo}'; return {status}; }}\n"
                        + script
                    ),
                    text=True,
                    capture_output=True,
                    timeout=5,
                    check=False,
                )
                self.assertEqual(result.returncode, expected, result.stderr)
                self.assertEqual(
                    "NVX-RESTORE-TSC-SYNC-CHECK-ENABLED" in result.stdout,
                    expected == 0,
                )

    def test_restore_processors_rejects_unstable_tsc_after_cpu_activation(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")

        script = microvm_tests._read_script("restore-processors.sh")
        for kernel_log, dmesg_status, expected_status in (
            ("clocksource: Switched to clocksource tsc", 0, 0),
            ("Measured 10992 cycles TSC warp between CPUs", 0, 95),
            ("tsc: Marking TSC unstable due to check_tsc_sync_source failed", 0, 95),
            ("TSC found unstable after boot", 0, 95),
            ("", 1, 1),
        ):
            with self.subTest(kernel_log=kernel_log, dmesg_status=dmesg_status):
                result = subprocess.run(
                    [shell],
                    input=(
                        "getconf() { printf '4\\n'; }\n"
                        "cat() {\n"
                        '  case "$1" in\n'
                        "    */cpu/online) printf '0-3\\n' ;;\n"
                        "    */current_clocksource) printf 'tsc\\n' ;;\n"
                        "    *) return 99 ;;\n"
                        "  esac\n"
                        "}\n"
                        'taskset() { printf "%s\\n" "$2"; }\n'
                        f"dmesg() {{ printf '%s\\n' '{kernel_log}'; "
                        f"return {dmesg_status}; }}\n" + script
                    ),
                    text=True,
                    capture_output=True,
                    timeout=5,
                    check=False,
                )

                self.assertEqual(result.returncode, expected_status, result.stderr)
                self.assertIn("NVX-RESTORE-PROCESSOR-OK count=4 cpu=3", result.stdout)
                if expected_status == 0:
                    self.assertIn(
                        "NVX-RESTORE-CLOCKSOURCE-OK source=tsc", result.stdout
                    )
                    self.assertIn("NVX-RESTORE-PROCESSORS-OK count=4", result.stdout)
                else:
                    self.assertNotIn("NVX-RESTORE-PROCESSORS-OK", result.stdout)
                if expected_status == 95:
                    self.assertIn(kernel_log, result.stdout)
                    self.assertIn(
                        "NVX-RESTORE-PROCESSORS-FAIL unstable-tsc", result.stdout
                    )

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
            [entry.kwargs["force_lapic_timer"] for entry in run_smp.call_args_list],
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
            common.BUILD_DIR / "initramfs-ubuntu.cpio.gz",
            requested,
        )
        self.assertEqual(run_guest_boot.call_args.kwargs["memory_mib"], 256)

    def test_runner_rejects_ubuntu_sandbox_control_scenarios(self):
        args = nvx.parse_args(
            [
                "test-microvm",
                "--backend",
                "whp",
                "--guest",
                "ubuntu",
                "--scenario",
                "sandbox-blocks",
            ]
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
            self.assertRaisesRegex(
                common.ScriptError,
                "Ubuntu guest does not support",
            ),
        ):
            microvm_tests.run(args)

    def test_runner_keeps_restore_tsc_logs_separate_from_processor_restore(self):
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
                    "restore-processors",
                    "--scenario",
                    "restore-tsc-sync",
                    "--scenario",
                    "restore-tsc-sync",
                    "--output-dir",
                    str(output_dir),
                ]
            )
            with (
                patch.object(microvm_tests, "validate_openvmm_test_backend"),
                patch.object(microvm_tests, "require_file", side_effect=require),
                patch.object(microvm_tests, "run_restore_processors") as run,
            ):
                self.assertEqual(microvm_tests.run(args), 0)
            self.assertTrue((output_dir / "restore-tsc-sync").is_dir())
        self.assertEqual(run.call_count, 2)
        self.assertEqual(run.call_args_list[0].kwargs["output_dir"], output_dir)
        self.assertNotIn("check_tsc_sync", run.call_args_list[0].kwargs)
        self.assertEqual(
            run.call_args_list[1].kwargs["output_dir"],
            output_dir / "restore-tsc-sync",
        )
        self.assertTrue(run.call_args_list[1].kwargs["check_tsc_sync"])

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
