#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import argparse
import contextlib
import io
import socket
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import call, patch

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import (  # noqa: E402
    benchmark,
    performance,
)


class BenchmarkTests(unittest.TestCase):
    def test_kvm_workers_forward_translated_scratch_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary).resolve()
            args = argparse.Namespace(
                scratch_dir=scratch,
                warmups=1,
                runs=2,
                memory_mib=128,
                processors=4,
                host_cpu_reserve=1,
                cpus="0-3",
                timeout=1.0,
                teardown_mode="guest-exit",
                net=None,
                network_profile=None,
                keep_kvm_stage=True,
            )
            workers = (
                (benchmark.benchmark_kvm, ""),
                (benchmark.benchmark_e2e_kvm, "e2e"),
                (benchmark.benchmark_snapshot_restore_kvm, "restore"),
                (benchmark.benchmark_snapshot_kvm, "snapshot"),
            )

            def translate(path: Path) -> str:
                if path == benchmark.NVX_SCRIPT:
                    return "/workspace/scripts/nvx.py"
                self.assertEqual(path, scratch)
                return "/mnt/data/nvx-benchmark-scratch"

            with (
                patch.object(benchmark, "stage_kvm"),
                patch.object(
                    benchmark,
                    "windows_to_wsl",
                    side_effect=translate,
                ) as translate_path,
                patch.object(
                    benchmark,
                    "_run_kvm_worker",
                    return_value={},
                ) as run_worker,
            ):
                for worker, result_kind in workers:
                    with self.subTest(result_kind=result_kind or "boot"):
                        translate_path.reset_mock()
                        run_worker.reset_mock()
                        worker(
                            args,
                            Path("openvmm"),
                            Path("kernel"),
                            Path("initrd"),
                        )

                        command = run_worker.call_args.args[0]
                        scratch_index = command.index("--scratch-dir")
                        self.assertEqual(
                            command[scratch_index : scratch_index + 2],
                            [
                                "--scratch-dir",
                                "/mnt/data/nvx-benchmark-scratch",
                            ],
                        )
                        self.assertEqual(
                            translate_path.call_args_list,
                            [call(benchmark.NVX_SCRIPT), call(scratch)],
                        )
                        self.assertEqual(
                            run_worker.call_args.args[1],
                            result_kind,
                        )

    def test_kvm_worker_defaults_to_translated_system_temporary_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            system_temporary = Path(temporary).resolve()
            args = argparse.Namespace(scratch_dir=None)
            with (
                patch.object(
                    benchmark.tempfile,
                    "gettempdir",
                    return_value=str(system_temporary),
                ),
                patch.object(
                    benchmark,
                    "windows_to_wsl",
                    return_value="/mnt/c/system-temp",
                ) as translate,
            ):
                self.assertEqual(
                    benchmark._kvm_worker_scratch_arguments(args),
                    ["--scratch-dir", "/mnt/c/system-temp"],
                )

            translate.assert_called_once_with(system_temporary)

    def test_builds_isolated_workload_command(self):
        command = benchmark.workload_boot_command(
            Path("openvmm"),
            "kvm",
            Path("vmlinux"),
            Path("initramfs.cpio.gz"),
            128,
            "quiet loglevel=0 nokaslr",
            processors=8,
            command_prefix=("taskset", "-c", "2-3"),
            network="10.0.0.2/24",
            mount="/mnt/host,C:/work,rw",
        )

        self.assertEqual(command[:4], ["taskset", "-c", "2-3", "openvmm"])
        self.assertEqual(
            command[4:10],
            [
                "--single-process",
                "--machine",
                "microvm",
                "--processors",
                "8",
                "--hypervisor",
            ],
        )
        self.assertIn("quiet loglevel=0 nokaslr", command)
        self.assertEqual(
            command[-8:],
            [
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--mount",
                "/mnt/host,C:/work,rw",
                "--virtio-console",
                "console",
            ],
        )

    def test_benchmark_network_requires_explicit_profile(self):
        args = nvx.parse_args(
            [
                "benchmark",
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
            ]
        )
        self.assertEqual(args.network_profile, "portable")

        args.network_profile = None
        with self.assertRaisesRegex(ValueError, "--net and --network-profile"):
            benchmark.run(args)

    def test_benchmark_scratch_directory_routes_temporary_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary).resolve() / "scratch"
            scratch.mkdir()
            args = nvx.parse_args(["benchmark", "--scratch-dir", str(scratch)])
            previous = tempfile.tempdir

            with (
                benchmark.benchmark_scratch_directory(args),
                patch.object(benchmark, "_git_revision", return_value="revision"),
            ):
                self.assertEqual(Path(tempfile.gettempdir()), scratch)
                with tempfile.TemporaryDirectory(prefix="openvmm-e2e-") as snapshot:
                    self.assertEqual(Path(snapshot).parent, scratch)
                document = benchmark.result_document(args, None, None)

            self.assertEqual(tempfile.tempdir, previous)
            self.assertEqual(args.scratch_dir, scratch)
            self.assertEqual(
                document["controls"]["scratch_directory"],
                str(scratch),
            )

    def test_benchmark_scratch_directory_defaults_to_system_temporary(self):
        args = nvx.parse_args(["benchmark"])
        previous = tempfile.tempdir

        with (
            benchmark.benchmark_scratch_directory(args),
            patch.object(benchmark, "_git_revision", return_value="revision"),
        ):
            self.assertEqual(tempfile.tempdir, previous)
            document = benchmark.result_document(args, None, None)

        self.assertIsNone(args.scratch_dir)
        self.assertEqual(
            document["controls"]["scratch_directory"],
            str(Path(tempfile.gettempdir()).resolve()),
        )

    def test_benchmark_scratch_directory_must_exist(self):
        with tempfile.TemporaryDirectory() as temporary:
            missing = Path(temporary) / "missing"
            args = nvx.parse_args(["benchmark", "--scratch-dir", str(missing)])
            previous = tempfile.tempdir

            with self.assertRaisesRegex(ValueError, "scratch directory does not exist"):
                benchmark.run(args)

            self.assertEqual(tempfile.tempdir, previous)

    def test_workload_commands_leave_out_the_boot_console_on_request(self):
        boot = benchmark.workload_boot_command(
            Path("openvmm"),
            "kvm",
            Path("vmlinux"),
            Path("initrd"),
            128,
            "quiet loglevel=0",
            boot_console=False,
        )
        restore = benchmark.snapshot_restore_command(
            Path("openvmm"), "kvm", Path("snapshot"), boot_console=False
        )

        self.assertNotIn("--virtio-console", boot)
        self.assertNotIn("--virtio-console", restore)

    def test_parses_dd_rates_and_network_gateway(self):
        output = """
67108864 bytes copied, 0.25 s, 268.4 MB/s
67108864 bytes copied, 0.04 s, 1.6 GB/s
"""

        self.assertEqual(benchmark.parse_dd_rate(output, 1), 268.4)
        self.assertEqual(benchmark.parse_dd_rate(output, 2), 1600.0)
        self.assertEqual(benchmark.network_gateway("10.0.0.2/24"), "10.0.0.1")

    def test_benchmark_command_lines_carry_no_clock_tuning(self):
        # Under the time ABI the hypervisor identity gives Linux its TSC and
        # LAPIC rates and trust, so the lifecycle line tunes no clock and the
        # cold-start clocksource variant selects tsc on every backend.
        for token in (
            "tsc=",
            "no_timer_check",
            "clocksource=",
            "tsc_early_khz=",
            "lapic_timer_hz=",
        ):
            self.assertNotIn(token, benchmark.BASE_TUNING)
        args = argparse.Namespace(
            runs=1,
            warmups=0,
            memory_mib=128,
            processors=1,
            timeout=10.0,
            teardown_mode="guest-exit",
        )
        for backend in ("kvm", "mshv", "whp"):
            commands: list[list[str]] = []

            def fake_benchmark(
                command: list[str],
                commands: list[list[str]] = commands,
                **_kwargs: object,
            ):
                commands.append(command)
                return {"samples_ms": [100.0]}

            with (
                self.subTest(backend=backend),
                patch.object(benchmark, "benchmark", side_effect=fake_benchmark),
                contextlib.redirect_stdout(io.StringIO()) as stdout,
            ):
                benchmark.benchmark_cold_start_workload(
                    args, Path("openvmm"), Path("vmlinux"), Path("initrd"), backend
                )
            cmdlines = [command[command.index("--cmdline") + 1] for command in commands]
            self.assertIn("quiet loglevel=0 clocksource=tsc", cmdlines)
            self.assertFalse(any("kvm-clock" in cmdline for cmdline in cmdlines))
            self.assertIn("  clocksource=tsc          :", stdout.getvalue())

    def test_smp_probe_uses_explicit_topology_and_worker_rendezvous(self):
        script = benchmark.smp_probe_script(4)

        self.assertIn("getconf _NPROCESSORS_ONLN", script)
        self.assertIn("physical_package_id", script)
        self.assertIn("die_id", script)
        self.assertIn("thread_siblings_list", script)
        self.assertIn('taskset -c "$cpu"', script)
        self.assertIn("for pid in $worker_pids", script)
        self.assertIn("loc_before", script)
        self.assertIn("SMP-LAPIC-FAIL", script)
        self.assertIn("SMP-IPI-FAIL", script)
        self.assertIn("read_loc_counter", script)
        self.assertNotIn("while :; do", script)
        self.assertIn("timer_attempts=10000", script)
        self.assertIn('[ "$after" -gt "$before" ]', script)
        self.assertIn('[ "$after" -ge "$current" ]', script)
        self.assertNotIn("sleep 0.1", script)
        self.assertIn("apic_ids=0,1,2,3 bsp=0 workers=$workers", script)
        self.assertIn("NVX-SMP-PROBE-OK", script)
        self.assertTrue(script.endswith("nvx-exit 0\n"))
        network_script = benchmark.smp_probe_script(
            4,
            network_gateway="10.0.0.1",
            ioapic_irq=16,
        )
        self.assertIn('ping -c 2 -W 1 "10.0.0.1"', network_script)
        self.assertIn("SMP-IOAPIC-FAIL", network_script)
        self.assertIn("SMP-IOAPIC-OK", network_script)
        with self.assertRaises(ValueError):
            benchmark.smp_probe_script(3)
        with self.assertRaises(ValueError):
            benchmark.smp_probe_script(4, network_gateway="10.0.0.1")

    def test_benchmark_cpu_set_reserves_host_worker_capacity(self):
        benchmark.validate_benchmark_cpu_set(set(range(10)), 8)
        with self.assertRaisesRegex(ValueError, "requires at least 10"):
            benchmark.validate_benchmark_cpu_set(set(range(9)), 8)
        benchmark.validate_benchmark_cpu_set({0, 2, 4}, 1)
        benchmark.validate_benchmark_cpu_set(set(range(8)), 8, 0)
        with self.assertRaisesRegex(ValueError, "requires at least 8"):
            benchmark.validate_benchmark_cpu_set(set(range(7)), 8, 0)

    def test_network_workload_runs_unmeasured_smp_interrupt_preflight(self):
        args = argparse.Namespace(
            net=None,
            network_memory_mib=128,
            processors=4,
            warmups=0,
            runs=1,
            timeout=1.0,
            teardown_mode="guest-exit",
        )
        result: benchmark.BenchmarkResult = {
            "samples_ms": [1.0],
            "p50_ms": 1.0,
            "p95_ms": 1.0,
            "min_ms": 1.0,
            "max_ms": 1.0,
            "wall_samples_ms": [1.0],
            "wall_p50_ms": 1.0,
            "wall_p95_ms": 1.0,
            "wall_min_ms": 1.0,
            "wall_max_ms": 1.0,
            "peak_rss_samples_bytes": [1],
            "peak_rss_p50_bytes": 1,
            "peak_rss_min_bytes": 1,
            "peak_rss_max_bytes": 1,
            "peak_rss_remeasured_count": 0,
            "teardown_samples_ms": [1.0],
            "teardown_completed_samples_ms": [1.0],
            "teardown_timeout_count": 0,
            "teardown_timeout_seconds": 5.0,
            "teardown_p50_ms": 1.0,
            "teardown_p95_ms": 1.0,
            "teardown_min_ms": 1.0,
            "teardown_max_ms": 1.0,
        }
        # The NIC interrupts on IOAPIC input 16 on every backend.
        for backend in ("kvm", "mshv", "whp"):
            with (
                patch.object(benchmark, "run_guest_script") as probe,
                patch.object(benchmark, "benchmark", return_value=result),
                patch.object(benchmark, "capture_automatic_snapshot"),
            ):
                benchmark.benchmark_network_snapshot_workload(
                    args,
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    backend,
                )

            self.assertEqual(probe.call_count, 1)
            self.assertIn("--network-profile", probe.call_args.args[0])
            self.assertIn("SMP-IOAPIC-OK", probe.call_args.args[1])
            self.assertIn("ioapic_irq=16\n", probe.call_args.args[1])
            self.assertEqual(
                probe.call_args.args[2], benchmark.SMP_PROBE_COMPLETION_MARKER
            )

    def test_virtfs_honors_host_termination_mode(self):
        guest_exit = benchmark._virtfs_script(1, "guest-exit")
        host_terminate = benchmark._virtfs_script(1, "host-terminate")
        roundtrip = benchmark._virtfs_roundtrip_script("host-terminate")

        self.assertIn("nvx-exit 0", guest_exit)
        self.assertNotIn("nvx-exit 0", host_terminate)
        self.assertNotIn("nvx-exit 0", roundtrip)

    def test_virtfs_exchange_uses_lf_only_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)

            def emulate_guest(
                *_args: object, **_kwargs: object
            ) -> benchmark.GuestCommandResult:
                (root / "guest-visible").write_bytes(benchmark.VIRTFS_GUEST_TO_HOST)
                deadline = benchmark.time.monotonic() + 1
                while benchmark.time.monotonic() < deadline:
                    if (
                        root / "host-visible"
                    ).read_bytes() == benchmark.VIRTFS_HOST_TO_GUEST:
                        break
                    benchmark.time.sleep(0.001)
                else:
                    self.fail("guest did not observe the byte-exact host update")
                return {"text": "", "wall_ms": 1.0, "peak_rss_bytes": 1}

            with patch.object(benchmark, "run_guest_script", side_effect=emulate_guest):
                benchmark._run_virtfs_roundtrip(
                    ["openvmm"],
                    root,
                    1,
                    1,
                    timeout=1,
                    windows_cpus=None,
                    teardown_mode="guest-exit",
                )

            self.assertEqual(
                (root / "host-visible").read_bytes(),
                b"host-to-guest\n",
            )

    def test_virtfs_excludes_declared_warmup(self):
        args = argparse.Namespace(
            virtfs_memory_mib=128,
            payload_mib=1,
            processors=1,
            warmups=1,
            timeout=1.0,
            teardown_mode="guest-exit",
        )
        io_result: benchmark.GuestCommandResult = {
            "text": "1 byte copied, 1 s, 1 MB/s\n1 byte copied, 1 s, 2 MB/s\n",
            "wall_ms": 1.0,
            "peak_rss_bytes": 1,
        }
        roundtrip_result: benchmark.GuestCommandResult = {
            "text": "",
            "wall_ms": 2.0,
            "peak_rss_bytes": 1,
        }
        with (
            patch.object(benchmark, "run_guest_script", return_value=io_result) as io,
            patch.object(
                benchmark, "_run_virtfs_roundtrip", return_value=roundtrip_result
            ) as roundtrip,
        ):
            benchmark.benchmark_virtfs_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
                runs=3,
            )

        self.assertEqual(io.call_count, 4)
        self.assertEqual(roundtrip.call_count, 4)

    def test_virtfs_lists_every_retained_sample(self):
        args = argparse.Namespace(
            virtfs_memory_mib=128,
            payload_mib=1,
            processors=1,
            warmups=1,
            timeout=1.0,
            teardown_mode="guest-exit",
        )
        # The warmup's rates and exchange time must not appear in the samples.
        io_results = [
            {
                "text": (
                    f"1 byte copied, 1 s, {write} MB/s\n"
                    f"1 byte copied, 1 s, {read} MB/s\n"
                ),
                "wall_ms": 1.0,
                "peak_rss_bytes": 1,
            }
            for write, read in ((99, 99), (11, 21), (12.5, 22), (13, 23))
        ]
        roundtrip_results = [
            {"text": "", "wall_ms": wall_ms, "peak_rss_bytes": 1}
            for wall_ms in (99.0, 2.5, 3.25, 4.125)
        ]
        with (
            patch.object(benchmark, "run_guest_script", side_effect=io_results),
            patch.object(
                benchmark, "_run_virtfs_roundtrip", side_effect=roundtrip_results
            ),
            patch("sys.stdout", new_callable=io.StringIO) as output,
        ):
            benchmark.benchmark_virtfs_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
                runs=3,
            )

        lines = output.getvalue().splitlines()
        self.assertIn(
            "  write samples                     : 11.0, 12.5, 13.0 MB/s", lines
        )
        self.assertIn(
            "  read samples                      : 21.0, 22.0, 23.0 MB/s", lines
        )
        self.assertIn(
            "  exchange samples                  : 2.500, 3.250, 4.125 ms", lines
        )
        # The listed samples must leave the summaries that collection parses
        # intact.
        metrics = performance._parse_virtfs(output.getvalue())
        self.assertEqual(metrics["virtfs_live_write"], ("MB/s", "higher", 12.5))
        self.assertEqual(metrics["virtfs_live_read"], ("MB/s", "higher", 22.0))
        self.assertEqual(metrics["virtfs_live_roundtrip"], ("ms", "lower", 3.2))

    def test_device_io_udp_echo_server_echoes_at_its_address_alone(self):
        with benchmark.UdpEchoServer("127.0.0.1", 0) as server:
            address = server.socket.getsockname()
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as client:
                client.settimeout(5)
                client.sendto(b"NVX-DEVICE-IO-ECHO", address)
                self.assertEqual(client.recvfrom(64), (b"NVX-DEVICE-IO-ECHO", address))
        self.assertEqual(address[0], "127.0.0.1")

    def test_managed_tap_cleanup_uses_openvmm_pid(self):
        query: subprocess.CompletedProcess[str] = subprocess.CompletedProcess(
            ["ip"], 0, "", ""
        )
        removed: subprocess.CompletedProcess[str] = subprocess.CompletedProcess(
            ["sudo", "-n", "ip"], 0, "", ""
        )
        with (
            patch.object(benchmark.sys, "platform", "linux"),
            patch.object(benchmark.os, "geteuid", return_value=1000, create=True),
            patch.object(
                benchmark.subprocess, "run", side_effect=(query, removed)
            ) as run,
        ):
            benchmark.cleanup_managed_tap(1234)

        self.assertEqual(
            run.call_args_list[1].args[0],
            [
                "sudo",
                "-n",
                "ip",
                "tuntap",
                "del",
                "dev",
                "ovm1234",
                "mode",
                "tap",
            ],
        )


if __name__ == "__main__":
    unittest.main()
