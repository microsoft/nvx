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
from nvx_tools import benchmark, microvm_tests, openvmm_process  # noqa: E402


class MicrovmTestParserTests(unittest.TestCase):
    def test_parser_defaults_to_all_correctness_scenarios(self):
        args = nvx.parse_args(["test-microvm", "--backend", "mshv"])

        self.assertIsNone(args.scenario)
        self.assertEqual(args.processors, [1, 2, 4, 8])
        self.assertEqual(args.memory_mib, 128)
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


class MicrovmTests(unittest.TestCase):
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
        self.assertIn('current_clocksource)" = tsc', whp)
        self.assertNotIn("@SELECT_CLOCKSOURCE@", mshv)
        self.assertIn("/sbin/nvx-reseed", mshv)

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
        shell = shutil.which("sh")
        if shell is None:
            git = shutil.which("git")
            git_shell = Path(git).parent.parent / "bin" / "sh.exe" if git else None
            if git_shell is not None and git_shell.is_file():
                shell = str(git_shell)
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
        self.assertEqual(capture_snapshot.call_args.kwargs["processors"], 1)
        self.assertEqual(measure_once.call_count, 4)
        self.assertEqual(
            [entry.kwargs["marker"] for entry in measure_once.call_args_list],
            [
                b"NVX-RESTORE-PROCESSORS-OK count=1",
                b"NVX-RESTORE-PROCESSORS-OK count=2",
                b"NVX-RESTORE-PROCESSORS-OK count=4",
                b"NVX-RESTORE-PROCESSORS-OK count=8",
            ],
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
                scenario=["smp", "smp"],
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
            [2, 8],
        )
        self.assertEqual(
            [entry.kwargs["log_path"].name for entry in run_smp.call_args_list],
            ["smp-2.log", "smp-8.log"],
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


if __name__ == "__main__":
    unittest.main()
