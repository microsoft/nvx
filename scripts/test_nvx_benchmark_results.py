#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import argparse
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import (  # noqa: E402
    benchmark,
    performance,
)
from nvx_tools.build_constants import (  # noqa: E402
    BuildConstants,
)
from nvx_tools_testing import MICROVM_RUN_DIRECTORY  # noqa: E402


class BenchmarkTests(unittest.TestCase):
    def test_kvm_worker_result_decoding(self):
        completed = subprocess.CompletedProcess(
            ["worker"],
            0,
            stdout='noise\nOPENVMM_KVM_E2E_RESULT={"p50_ms": 1.5}\n',
            stderr="",
        )
        with patch.object(benchmark.subprocess, "run", return_value=completed):
            result = benchmark._run_kvm_worker(["worker"], "e2e")

        self.assertEqual(result, {"p50_ms": 1.5})

    def test_result_documents_record_host_provenance(self):
        args = nvx.parse_args(["benchmark"])
        with (
            patch.object(benchmark, "_git_revision", return_value="revision"),
            patch.object(
                benchmark,
                "benchmark_host_provenance",
                return_value={"runner_name": "azure-windows-3"},
            ) as host,
        ):
            document = benchmark.result_document(args, None, None)

        host.assert_called_once_with(args)
        self.assertEqual(document["host"], {"runner_name": "azure-windows-3"})

    def test_host_provenance_names_workspace_temporary_and_scratch_volumes(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary).resolve() / "scratch"
            scratch.mkdir()
            args = nvx.parse_args(["benchmark", "--scratch-dir", str(scratch)])
            system_temporary = Path(tempfile.gettempdir()).resolve()
            with (
                benchmark.benchmark_scratch_directory(args),
                patch.object(
                    benchmark.host_telemetry,
                    "host_provenance",
                    return_value={"schema_version": 1},
                ) as provenance,
            ):
                host = benchmark.benchmark_host_provenance(args)

        self.assertEqual(host, {"schema_version": 1})
        provenance.assert_called_once_with(
            {
                "workspace": BuildConstants.REPO_ROOT,
                "temporary": system_temporary,
                "scratch": scratch,
            }
        )

    def test_device_io_resume_ignores_host_provenance(self):
        args = nvx.parse_args(
            [
                "benchmark",
                "--suite",
                "device-io",
                "--backend",
                "whp",
                "--platform",
                "windows-whp-baremetal",
            ]
        )
        benchmark.apply_benchmark_suite_defaults(args)
        hosts = iter(({"machine_name": "first"}, {"machine_name": "second"}))

        def host_provenance(_: argparse.Namespace) -> dict[str, str]:
            return next(hosts, {"machine_name": "third"})

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(
                benchmark, "benchmark_host_provenance", side_effect=host_provenance
            ),
            patch.object(benchmark, "sha256_file", return_value="0" * 64),
            patch.object(
                benchmark,
                "_device_io_helper_provenance",
                return_value={"source": "helper"},
            ),
            patch.object(benchmark, "_git_revision", return_value="revision"),
            patch.object(benchmark, "_git_status", return_value=[]),
        ):
            output = Path(temporary)

            def write() -> Path:
                return benchmark.write_benchmark_metadata(
                    args,
                    output,
                    Path("openvmm.exe"),
                    Path("vmlinux"),
                    Path("initramfs.cpio.gz"),
                    "whp",
                )

            path = write()
            written = path.read_text(encoding="utf-8")
            self.assertEqual(json.loads(written)["host"], {"machine_name": "first"})
            self.assertEqual(write(), path)
            self.assertEqual(path.read_text(encoding="utf-8"), written)
            args.device_io_port += 1
            with self.assertRaisesRegex(
                ValueError, "resume controls or provenance do not match"
            ):
                write()

    def test_benchmark_preserves_exact_process_wall_samples(self):
        samples = [
            (10.0, 1024, 2.0, 13.5),
            (12.0, 2048, 3.0, 16.5),
        ]
        with patch.object(benchmark, "measure_once", side_effect=samples):
            result = benchmark.benchmark(["openvmm"], warmups=0, runs=2, timeout=1)

        self.assertEqual(result["samples_ms"], [10.0, 12.0])
        self.assertEqual(result["p95_ms"], 12.0)
        self.assertEqual(result["wall_samples_ms"], [13.5, 16.5])
        self.assertEqual(result["wall_p50_ms"], 15.0)
        self.assertEqual(result["wall_p95_ms"], 16.5)
        self.assertEqual(
            benchmark.SNAPSHOT_FILENAMES,
            ("manifest.bin", "state.bin", "memory.bin"),
        )

    def test_benchmark_remeasures_attempt_without_marker_rss(self):
        attempts = iter(
            (
                (5.0, None, 1.0, 6.0),
                (10.0, None, 7.0, 17.0),
                (11.0, 2048, 7.0, 18.0),
                (12.0, 4096, 8.0, 20.0),
            )
        )

        def measure_once(
            *_args: object,
            profile_sink: list[dict[str, object]] | None = None,
            **_kwargs: object,
        ) -> tuple[float, int | None, float | None, float]:
            sample = next(attempts)
            if profile_sink is not None:
                profile_sink.append({"elapsed_ms": sample[0]})
            return sample

        before_each = MagicMock()
        with (
            patch.object(benchmark, "measure_once", side_effect=measure_once),
            patch.object(
                benchmark, "summarize_lifecycle_profiles", return_value={}
            ) as summarize,
            patch("sys.stdout", new_callable=io.StringIO) as output,
        ):
            result = benchmark.benchmark(
                ["openvmm"],
                warmups=1,
                runs=2,
                timeout=1,
                marker=benchmark.RESTORE_MARKER,
                guest_exit_prequeued=True,
                snapshot_profile=True,
                before_each=before_each,
            )

        self.assertEqual(result["samples_ms"], [11.0, 12.0])
        self.assertEqual(result["peak_rss_samples_bytes"], [2048, 4096])
        self.assertEqual(result["peak_rss_min_bytes"], 2048)
        self.assertEqual(result["peak_rss_remeasured_count"], 1)
        self.assertEqual(result["teardown_samples_ms"], [7.0, 8.0])
        self.assertEqual(before_each.call_count, 4)
        summarize.assert_called_once_with([{"elapsed_ms": 11.0}, {"elapsed_ms": 12.0}])
        self.assertIn("warmup 1/1: 5.000 ms, peak RSS=unavailable", output.getvalue())
        self.assertIn(
            "sample 1/2: discarded attempt 1/3 (10.000 ms)", output.getvalue()
        )

    def test_benchmark_rejects_rss_missing_from_every_attempt(self):
        with (
            patch.object(
                benchmark, "measure_once", return_value=(10.0, None, 7.0, 17.0)
            ) as measure,
            patch("sys.stdout", new_callable=io.StringIO),
            self.assertRaisesRegex(
                RuntimeError,
                f"{benchmark.PEAK_RSS_SAMPLE_ATTEMPTS} consecutive attempts "
                "for sample 1/2",
            ),
        ):
            benchmark.benchmark(["openvmm"], warmups=0, runs=2, timeout=1)

        self.assertEqual(measure.call_count, benchmark.PEAK_RSS_SAMPLE_ATTEMPTS)

    def test_nearest_rank_percentile(self):
        self.assertEqual(benchmark.nearest_rank_percentile(range(1, 22), 95), 20)
        self.assertEqual(benchmark.nearest_rank_percentile([7.0], 95), 7.0)
        with self.assertRaisesRegex(ValueError, "without samples"):
            benchmark.nearest_rank_percentile([], 95)

    def test_shell_snapshot_summary_lists_restore_samples_in_order(self):
        restored = [85.6894, 74.955, 107.1931, 36.2756]
        with patch("sys.stdout", new_callable=io.StringIO) as output:
            benchmark._print_shell_snapshot_summary(256, [308.7, 309.1], restored)

        lines = output.getvalue().splitlines()
        restore = next(
            index
            for index, line in enumerate(lines)
            if line.startswith("  snapshot restore    : median")
        )
        self.assertEqual(
            lines[restore + 1],
            "  restore samples     : 85.689, 74.955, 107.193, 36.276 ms",
        )
        self.assertEqual(
            performance._parse_shell_snapshot_restore(output.getvalue()),
            {"shell_snapshot_restore_256_mib": ("ms", "lower", 80.3)},
        )

    def test_device_io_retains_failure_and_resumes_completed_attempts(self):
        args = argparse.Namespace(
            processors=1,
            warmups=1,
            runs=1,
            device_io_duration_seconds=1.0,
            device_io_size_mib=64,
            device_io_port=5201,
            net="192.0.2.2/24",
            timeout=1.0,
            teardown_mode="guest-exit",
        )

        def guest_result(command: list[str], *_args: object, **_kwargs: object):
            if "--net" in command:
                payload = (
                    'NVX_DEVICE_IO_GUEST_RESULT={"device":"network",'
                    '"operation":"roundtrip","operations":100,'
                    '"bytes_per_operation":64,"elapsed_ns":1000000000}\n'
                )
            else:
                payload = (
                    'NVX_DEVICE_IO_GUEST_RESULT={"device":"file",'
                    '"operation":"write","operations":100,'
                    '"bytes_per_operation":4096,"elapsed_ns":1000000000}\n'
                    'NVX_DEVICE_IO_GUEST_RESULT={"device":"file",'
                    '"operation":"read","operations":100,'
                    '"bytes_per_operation":4096,"elapsed_ns":1000000000}\n'
                )
            return {"text": payload, "wall_ms": 1.0, "peak_rss_bytes": 1}

        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "device-io.log"
            with (
                patch.object(benchmark, "host_ipv4_address", return_value="192.0.2.1"),
                patch.object(benchmark, "UdpEchoServer") as echo_server,
                patch.object(
                    benchmark,
                    "run_guest_script",
                    side_effect=guest_result,
                ) as run_guest,
            ):
                echo_server.return_value.__enter__.return_value = None
                benchmark.benchmark_device_io_workload(
                    args,
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initramfs"),
                    "mshv",
                    output_path=output,
                )
                self.assertEqual(run_guest.call_count, 6)
                # The echo server binds the address to which the guest sends.
                echo_server.assert_called_once_with("192.0.2.1", 5201)
                # A hung attempt describes the host of its scratch directory.
                host_state = run_guest.call_args.kwargs["host_state_on_timeout"]
                self.assertIs(host_state.func, benchmark._device_io_host_state)
                self.assertTrue(
                    host_state.args[0].name.startswith("openvmm-device-io-")
                )
                network_commands = [
                    call.args[0]
                    for call in run_guest.call_args_list
                    if "--net" in call.args[0]
                ]
                self.assertEqual(len(network_commands), 2)
                self.assertIn("192.0.2.2/24", network_commands[0])
                self.assertIn("microvm", network_commands[0])
                self.assertNotIn("microvm-v2", network_commands[0])
                block_commands = [
                    call.args[0]
                    for call in run_guest.call_args_list
                    if "--microvm-sandbox-block" in call.args[0]
                ]
                self.assertEqual(len(block_commands), 2)
                self.assertNotIn("--virtio-blk", block_commands[0])
                block_argument = block_commands[0][
                    block_commands[0].index("--microvm-sandbox-block") + 1
                ]
                self.assertTrue(block_argument.startswith("scratch:file:"))

                benchmark.benchmark_device_io_workload(
                    args,
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initramfs"),
                    "mshv",
                    output_path=output,
                )
                self.assertEqual(run_guest.call_count, 6)

            records = benchmark._read_device_io_records(output)
            self.assertEqual(len(records), 6)
            retained = [record for record in records if not record["warmup"]]
            self.assertEqual([record["sample_index"] for record in retained], [0, 0, 0])

    def test_device_io_missing_guest_result_becomes_failure(self):
        result: benchmark.GuestCommandResult = {
            "text": (
                'NVX_DEVICE_IO_GUEST_RESULT={"device":"file",'
                '"operation":"read","operations":1,'
                '"bytes_per_operation":4096,"elapsed_ns":1}\n'
            ),
            "wall_ms": 1.0,
            "peak_rss_bytes": 1,
        }

        record = benchmark._device_io_attempt_record("virtio-blk", 0, 0, result, None)

        self.assertEqual(record["status"], "failure")
        self.assertIn("missing", str(record["error"]))

    def test_performance_suite_writes_four_canonical_logs(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            (output / "snapshot.log").write_text("stale", encoding="ascii")
            (output / "snapshot-hello.log").write_text("stale", encoding="ascii")
            args = nvx.parse_args(
                [
                    "benchmark",
                    "--suite",
                    "performance",
                    "--backend",
                    "whp",
                    "--platform",
                    "windows-whp-baremetal",
                    "--processors",
                    "8",
                    "--output-dir",
                    str(output),
                ]
            )
            with (
                patch.object(benchmark, "benchmark_cold_start_workload") as cold,
                patch.object(benchmark, "benchmark_virtfs_workload") as virtfs,
                patch.object(benchmark, "benchmark_shell_snapshot_workload") as shell,
                patch.object(
                    benchmark, "benchmark_network_snapshot_workload"
                ) as network,
            ):
                result = benchmark.run_workload_benchmarks(
                    args,
                    Path("openvmm.exe"),
                    Path("vmlinux"),
                    Path("initramfs.cpio.gz"),
                    "whp",
                )

            self.assertEqual(result, 0)
            self.assertEqual(
                {path.name for path in output.iterdir()},
                {
                    "benchmark-metadata.json",
                    "cold-start.log",
                    "virtfs.log",
                    "shell-snapshot.log",
                    "network.log",
                },
            )
            metadata = json.loads(
                (output / "benchmark-metadata.json").read_text(encoding="utf-8")
            )
            self.assertEqual(metadata["platform"], "windows-whp-baremetal")
            self.assertEqual(metadata["backend"], "whp")
            self.assertEqual(metadata["microvm_abi_version"], 3)
            self.assertEqual(metadata["processors"], 8)
            self.assertEqual(metadata["host_affinity_set"], args.cpus)
            self.assertEqual(metadata["host_cpu_reserve"], args.host_cpu_reserve)
            self.assertEqual(
                metadata["scratch_directory"],
                str(Path(tempfile.gettempdir()).resolve()),
            )
            cold.assert_called_once()
            self.assertEqual(virtfs.call_args.kwargs["runs"], 3)
            shell.assert_called_once()
            network.assert_called_once()

    def test_restore_only_suite_writes_only_its_canonical_log(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            (output / "cold-start.log").write_text("stale", encoding="ascii")
            (output / "shell-snapshot.log").write_text("stale", encoding="ascii")
            (output / "acceptance.json").write_text("stale", encoding="ascii")
            args = nvx.parse_args(
                [
                    "benchmark",
                    "--suite",
                    "shell-snapshot-restore",
                    "--backend",
                    "whp",
                    "--platform",
                    "windows-whp-virtual-machine",
                    "--processors",
                    "8",
                    "--shell-memories",
                    "512",
                    "--output-dir",
                    str(output),
                ]
            )
            with patch.object(
                benchmark, "benchmark_shell_snapshot_restore_workload"
            ) as restore:
                result = benchmark.run_workload_benchmarks(
                    args,
                    Path("openvmm.exe"),
                    Path("vmlinux"),
                    Path("initramfs.cpio.gz"),
                    "whp",
                )

            self.assertEqual(result, 0)
            self.assertEqual(
                {path.name for path in output.iterdir()},
                {"benchmark-metadata.json", "shell-snapshot-restore.log"},
            )
            restore.assert_called_once()

    def test_mshv_performance_suite_includes_network(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            args = nvx.parse_args(
                [
                    "benchmark",
                    "--suite",
                    "performance",
                    "--backend",
                    "mshv",
                    "--output-dir",
                    str(output),
                ]
            )
            with (
                patch.object(benchmark, "benchmark_cold_start_workload"),
                patch.object(benchmark, "benchmark_virtfs_workload"),
                patch.object(benchmark, "benchmark_shell_snapshot_workload"),
                patch.object(
                    benchmark, "benchmark_network_snapshot_workload"
                ) as network,
            ):
                benchmark.run_workload_benchmarks(
                    args,
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initramfs.cpio.gz"),
                    "mshv",
                )

            self.assertEqual(
                {path.name for path in output.iterdir()},
                {
                    "benchmark-metadata.json",
                    "cold-start.log",
                    "virtfs.log",
                    "shell-snapshot.log",
                    "network.log",
                },
            )
            network.assert_called_once()

    def test_mshv_runs_explicit_network_snapshot_suite(self):
        args = nvx.parse_args(
            ["benchmark", "--suite", "network-snapshot", "--backend", "mshv"]
        )

        with patch.object(benchmark, "benchmark_network_snapshot_workload") as network:
            benchmark.run_workload_benchmarks(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initramfs.cpio.gz"),
                "mshv",
            )

        network.assert_called_once()

    def test_performance_suite_defaults_to_data_runs_platform(self):
        with tempfile.TemporaryDirectory() as temporary:
            repository = Path(temporary)
            backend = "whp" if os.name == "nt" else "kvm"
            platform = "windows-whp" if os.name == "nt" else "linux-kvm"
            args = nvx.parse_args(
                [
                    "benchmark",
                    "--suite",
                    "performance",
                    "--backend",
                    backend,
                    "--nvx-dir",
                    str(repository),
                ]
            )
            with (
                patch.object(benchmark, "benchmark_cold_start_workload"),
                patch.object(benchmark, "benchmark_virtfs_workload"),
                patch.object(benchmark, "benchmark_shell_snapshot_workload"),
                patch.object(benchmark, "benchmark_network_snapshot_workload"),
            ):
                benchmark.run_workload_benchmarks(
                    args,
                    Path("openvmm.exe"),
                    Path("vmlinux"),
                    Path("initramfs.cpio.gz"),
                    backend,
                )

            output = (
                repository
                / "data"
                / "runs"
                / f"{platform}-{MICROVM_RUN_DIRECTORY}-1vcpu"
            )
            self.assertEqual(
                {path.name for path in output.iterdir()},
                {
                    "benchmark-metadata.json",
                    "cold-start.log",
                    "virtfs.log",
                    "shell-snapshot.log",
                    "network.log",
                },
            )

    def test_device_io_defaults_to_dedicated_abi_v2_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            repository = Path(temporary).resolve()
            backend = "whp" if os.name == "nt" else "kvm"
            platform = "windows-whp" if os.name == "nt" else "linux-kvm"
            args = nvx.parse_args(
                [
                    "benchmark",
                    "--suite",
                    "device-io",
                    "--backend",
                    backend,
                    "--nvx-dir",
                    str(repository),
                ]
            )
            with (
                patch.object(
                    benchmark,
                    "write_benchmark_metadata",
                    return_value=Path("benchmark-metadata.json"),
                ) as metadata,
                patch.object(
                    benchmark, "benchmark_device_io_workload", return_value=0
                ) as device_io,
            ):
                benchmark.run_workload_benchmarks(
                    args,
                    Path("openvmm.exe"),
                    Path("vmlinux"),
                    Path("initramfs.cpio.gz"),
                    backend,
                )

            output = (
                repository
                / "data"
                / "runs"
                / platform
                / MICROVM_RUN_DIRECTORY
                / "1vcpu"
                / "device-io"
            )
            self.assertEqual(
                device_io.call_args.kwargs["output_path"],
                output / "device-io.log",
            )
            self.assertEqual(metadata.call_args.args[1], output)


if __name__ == "__main__":
    unittest.main()
