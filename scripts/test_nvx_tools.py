#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import argparse
import hashlib
import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path
from typing import cast
from unittest.mock import call, patch

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import (  # noqa: E402
    archive,
    benchmark,
    build,
    ci,
    common,
    release,
    sandbox,
)


class CliTests(unittest.TestCase):
    def test_benchmark_exposes_non_python_performance_suite(self):
        args = nvx.parse_args(
            [
                "benchmark",
                "--suite",
                "performance",
                "--shell-memories",
                "64",
                "256",
                "--output-dir",
                "results",
            ]
        )

        self.assertEqual(args.suite, "performance")
        self.assertEqual(args.shell_memories, [64, 256])
        self.assertEqual(args.virtfs_runs, 3)
        self.assertEqual(args.payload_mib, 64)
        self.assertEqual(args.network_memory_mib, 256)
        self.assertEqual(args.output_dir, Path("results"))
        self.assertIs(args.handler, benchmark.run)

    def test_release_commands_keep_their_cli_contract(self):
        download = nvx.parse_args(
            ["download", "--repository", "example/nvx", "--hypervisor", "auto"]
        )
        self.assertEqual(download.command, "download")
        self.assertEqual(download.repository, "example/nvx")
        self.assertEqual(download.hypervisor, "auto")
        self.assertIs(download.handler, nvx.command_download)

        collect = nvx.parse_args(["collect-sources"])
        self.assertEqual(collect.command, "collect-sources")
        self.assertIs(collect.handler, nvx.command_collect_sources)

        package = nvx.parse_args(
            [
                "package",
                "--version",
                "1.2.3",
                "--destination",
                "output",
                "--include-source",
                "--force",
            ]
        )
        self.assertEqual(package.command, "package")
        self.assertEqual(package.version, "1.2.3")
        self.assertEqual(package.destination, Path("output"))
        self.assertTrue(package.include_source)
        self.assertFalse(package.binary_only)
        self.assertTrue(package.force)
        self.assertIs(package.handler, nvx.command_package)

        verify = nvx.parse_args(["verify"])
        self.assertEqual(verify.command, "verify")
        self.assertIs(verify.handler, nvx.command_verify)

        openvmm_tests = nvx.parse_args(["test-openvmm", "--backend", "mshv"])
        self.assertEqual(openvmm_tests.backend, "mshv")
        self.assertIs(openvmm_tests.handler, nvx.command_test_openvmm)

    def test_sandbox_command_parses_typed_launch_contract(self):
        args = nvx.parse_args(
            [
                "sandbox",
                "--layer",
                "distro,distro.erofs,11111111-1111-1111-1111-111111111111",
                "--scratch",
                "scratch.ext4",
                "--entrypoint",
                "/bin/workload",
                "--arg=--serve",
                "--memory-max",
                "268435456",
                "--pids-max",
                "64",
            ]
        )

        self.assertEqual(args.layer[0].role, "distro")
        self.assertEqual(args.scratch, Path("scratch.ext4"))
        self.assertEqual(args.sandbox_arg, ["--serve"])
        self.assertEqual(args.memory_max, 268435456)
        self.assertEqual(args.pids_max, 64)
        self.assertIs(args.handler, nvx.command_sandbox)

    def test_network_requires_explicit_portable_profile(self):
        args = nvx.parse_args(
            [
                "run",
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
            ]
        )
        self.assertEqual(args.network_profile, "portable")

        missing_profile = nvx.parse_args(["run", "--net", "10.0.0.2/24"])
        with self.assertRaisesRegex(common.ScriptError, "--net and --network-profile"):
            nvx.command_run(missing_profile)

        missing_network = nvx.parse_args(["run", "--network-profile", "portable"])
        with self.assertRaisesRegex(common.ScriptError, "--net and --network-profile"):
            nvx.command_run(missing_network)

    def test_run_exposes_restore_readiness(self):
        args = nvx.parse_args(
            [
                "run",
                "--hypervisor",
                "mshv",
                "--restore-snapshot",
                "snapshot",
                "--restore-ready-path",
                "ready.sock",
                "--dry-run",
            ]
        )

        with (
            patch.object(nvx, "require_file", return_value=Path("openvmm")) as require,
            patch.object(
                nvx, "_format_command", return_value="formatted"
            ) as format_command,
        ):
            nvx.command_run(args)

        require.assert_called_once()
        command = format_command.call_args.args[0]
        self.assertEqual(
            command,
            [
                "openvmm",
                "--single-process",
                "--machine",
                "microvm",
                "--hypervisor",
                "mshv",
                "--restore-snapshot",
                "snapshot",
                "--restore-entropy",
                "--restore-ready-path",
                "ready.sock",
            ],
        )

        missing_snapshot = nvx.parse_args(
            ["run", "--restore-ready-path", "ready.sock", "--dry-run"]
        )
        with self.assertRaisesRegex(
            common.ScriptError, "--restore-ready-path requires --restore-snapshot"
        ):
            nvx.command_run(missing_snapshot)

    def test_openvmm_build_skips_compatibility_igvm(self):
        with (
            patch.object(nvx, "require_file"),
            patch.object(nvx, "_run") as run,
        ):
            nvx.command_build_openvmm(argparse.Namespace(skip_restore=False))

        self.assertEqual(
            run.call_args_list[0],
            call(
                [
                    "cargo",
                    "xflowey",
                    "restore-packages",
                    "--no-compat-igvm",
                ],
                cwd=common.OPENVMM_DIR,
            ),
        )


class CiTests(unittest.TestCase):
    def test_openvmm_tests_bind_guest_artifacts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            openvmm = root / "openvmm"
            artifacts = root / "build"
            openvmm.mkdir()
            artifacts.mkdir()
            (openvmm / "Cargo.toml").touch()
            kernel = artifacts / "vmlinux"
            initrd = artifacts / "initramfs.cpio.gz"
            kernel.touch()
            initrd.touch()
            backend = "whp" if os.name == "nt" else "kvm"

            with (
                patch.object(ci, "OPENVMM_DIR", openvmm),
                patch.object(ci, "BUILD_DIR", artifacts),
                patch.object(ci.os, "access", return_value=True),
                patch.object(ci.Path, "exists", return_value=False),
                patch.object(ci, "require_tool", return_value="cargo"),
                patch.object(ci, "run_checked") as run_checked,
                patch.dict(os.environ, {"PETRI_CAPABILITIES": "vpci"}),
            ):
                ci.run_openvmm_tests(backend)

            self.assertEqual(run_checked.call_count, 2)
            restore, tests = run_checked.call_args_list
            self.assertEqual(
                restore.args[0],
                ["cargo", "xflowey", "restore-packages", "--no-compat-igvm"],
            )
            command = tests.args[0]
            self.assertEqual(command[:3], ["cargo", "xflowey", "vmm-tests-run"])
            self.assertEqual(command[-2:], ["--filter", ci.OPENVMM_MICROVM_TEST_FILTER])
            self.assertEqual(tests.kwargs["cwd"], openvmm)
            env = tests.kwargs["env"]
            self.assertEqual(env["OPENVMM_MICROVM_PVH_KERNEL"], str(kernel.resolve()))
            self.assertEqual(env["OPENVMM_MICROVM_PVH_INITRD"], str(initrd.resolve()))
            self.assertEqual(env["PETRI_CAPABILITIES"], "vpci,microvm_pvh")

    def test_openvmm_tests_reject_unknown_backend(self):
        with self.assertRaisesRegex(common.ScriptError, "unsupported.*backend"):
            ci.run_openvmm_tests("unknown")


class BuildTests(unittest.TestCase):
    def test_sandbox_kernel_config_requires_every_feature(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / ".config"
            config.write_text(
                "\n".join(build.REQUIRED_SANDBOX_KERNEL_CONFIG) + "\n",
                encoding="utf-8",
            )
            build._assert_sandbox_kernel_config(config)

            config.write_text(
                "\n".join(build.REQUIRED_SANDBOX_KERNEL_CONFIG[:-1]) + "\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                common.ScriptError,
                build.REQUIRED_SANDBOX_KERNEL_CONFIG[-1],
            ):
                build._assert_sandbox_kernel_config(config)


class SandboxTests(unittest.TestCase):
    def test_launch_contract_orders_roles_and_builds_agent_command_line(self):
        custom = sandbox.SandboxLayer.parse(
            "custom,custom.erofs,22222222-2222-2222-2222-222222222222"
        )
        distro = sandbox.SandboxLayer.parse(
            "distro,distro.erofs,11111111-1111-1111-1111-111111111111"
        )
        launch = sandbox.SandboxLaunch(
            layers=(custom, distro),
            scratch=Path("scratch.ext4"),
            entrypoint="/bin/workload",
            args=("--serve",),
            hostname="example",
            memory_max=268435456,
            pids_max=64,
        )

        self.assertEqual(
            [layer.role for layer in launch.ordered_layers()],
            ["distro", "custom"],
        )
        self.assertEqual(
            launch.kernel_command_line("quiet"),
            (
                "quiet nvx_sandbox=1 "
                "nvx_layer=distro,0xd0003000,11111111-1111-1111-1111-111111111111 "
                "nvx_layer=custom,0xd0005000,22222222-2222-2222-2222-222222222222 "
                "nvx_scratch=0xd0006000,ext4 "
                "nvx_entrypoint=/bin/workload nvx_hostname=example "
                "nvx_arg=--serve nvx_memory_max=268435456 nvx_pids_max=65"
            ),
        )
        self.assertEqual(
            launch.openvmm_arguments(),
            [
                "--machine",
                "microvm-v2",
                "--microvm-sandbox-block",
                "distro:file:distro.erofs,ro",
                "--microvm-sandbox-block",
                "custom:file:custom.erofs,ro",
                "--microvm-sandbox-block",
                "scratch:file:scratch.ext4",
            ],
        )

    def test_launch_contract_rejects_duplicates_and_reserved_tokens(self):
        distro = sandbox.SandboxLayer.parse(
            "distro,distro.erofs,11111111-1111-1111-1111-111111111111"
        )
        with self.assertRaisesRegex(common.ScriptError, "duplicate.*distro"):
            sandbox.SandboxLaunch(
                layers=(distro, distro),
                scratch=Path("scratch.ext4"),
            )

        launch = sandbox.SandboxLaunch(
            layers=(distro,),
            scratch=Path("scratch.ext4"),
        )
        with self.assertRaisesRegex(common.ScriptError, "owned"):
            launch.kernel_command_line("nvx_sandbox=0")
        with self.assertRaisesRegex(common.ScriptError, "owned"):
            launch.kernel_command_line(r"foo=bar\ nvx_memory_max=max")
        with self.assertRaisesRegex(common.ScriptError, "1024-byte"):
            launch.kernel_command_line("x" * sandbox.SANDBOX_COMMAND_LINE_MAX_SIZE)

    def test_layer_parser_rejects_invalid_role_and_uuid(self):
        with self.assertRaisesRegex(common.ScriptError, "unsupported layer role"):
            sandbox.SandboxLayer.parse(
                "unknown,layer.erofs,11111111-1111-1111-1111-111111111111"
            )
        with self.assertRaisesRegex(common.ScriptError, "UUID is invalid"):
            sandbox.SandboxLayer.parse("distro,layer.erofs,not-a-uuid")

    def test_launch_contract_rejects_disk_option_delimiters(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            layer_path = root / "layer;create=1G"
            scratch = root / "scratch.ext4"
            layer_path.touch()
            scratch.touch()
            layer = sandbox.SandboxLayer(
                role="distro",
                path=layer_path,
                uuid="11111111-1111-1111-1111-111111111111",
            )

            with self.assertRaisesRegex(common.ScriptError, "semicolons"):
                sandbox.SandboxLaunch(
                    layers=(layer,),
                    scratch=scratch,
                ).validated()


class BenchmarkTests(unittest.TestCase):
    def test_builds_isolated_workload_command(self):
        command = benchmark.workload_boot_command(
            Path("openvmm"),
            "kvm",
            Path("vmlinux"),
            Path("initramfs.cpio.gz"),
            128,
            "quiet loglevel=0 nokaslr",
            command_prefix=("taskset", "-c", "2-3"),
            network="10.0.0.2/24",
            mount="/mnt/host,C:/work,rw",
        )

        self.assertEqual(command[:4], ["taskset", "-c", "2-3", "openvmm"])
        self.assertIn("quiet loglevel=0 nokaslr", command)
        self.assertEqual(
            command[-6:],
            [
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--mount",
                "/mnt/host,C:/work,rw",
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

    def test_network_snapshot_restore_selects_portable_profile(self):
        command = benchmark.snapshot_restore_command(
            Path("openvmm"),
            "mshv",
            Path("snapshot"),
            network_profile="portable",
        )

        self.assertEqual(
            command[-2:],
            [
                "--network-profile",
                "portable",
            ],
        )

    def test_parses_dd_rates_and_network_gateway(self):
        output = """
67108864 bytes copied, 0.25 s, 268.4 MB/s
67108864 bytes copied, 0.04 s, 1.6 GB/s
"""

        self.assertEqual(benchmark.parse_dd_rate(output, 1), 268.4)
        self.assertEqual(benchmark.parse_dd_rate(output, 2), 1600.0)
        self.assertEqual(benchmark.network_gateway("10.0.0.2/24"), "10.0.0.1")
        self.assertEqual(
            benchmark.clocksource_parameter("kvm"), "clocksource=kvm-clock"
        )
        self.assertEqual(benchmark.clocksource_parameter("mshv"), "clocksource=tsc")

    def test_benchmark_preserves_exact_process_wall_samples(self):
        samples = [
            (10.0, 1024, 2.0, 13.5),
            (12.0, 2048, 3.0, 16.5),
        ]
        with patch.object(benchmark, "measure_once", side_effect=samples):
            result = benchmark.benchmark(["openvmm"], warmups=0, runs=2, timeout=1)

        self.assertEqual(result["samples_ms"], [10.0, 12.0])
        self.assertEqual(result["wall_samples_ms"], [13.5, 16.5])
        self.assertEqual(result["wall_p50_ms"], 15.0)
        self.assertEqual(
            benchmark.SNAPSHOT_FILENAMES,
            ("manifest.bin", "state.bin", "memory.bin"),
        )

    def test_snapshot_capture_excludes_warmup_and_retains_last_snapshot(self):
        with tempfile.TemporaryDirectory() as temporary:
            retained = Path(temporary) / "retained"
            observed_paths: list[Path] = []

            def capture(
                _command: object,
                snapshot_path: Path,
                **_kwargs: object,
            ) -> tuple[float, float, float, int]:
                snapshot_path.mkdir()
                observed_paths.append(snapshot_path)
                sample = float(len(observed_paths))
                return sample, sample, sample / 10, len(observed_paths) * 1024

            args = argparse.Namespace(warmups=1, runs=2, timeout=1.0)
            with patch.object(benchmark, "capture_snapshot", side_effect=capture):
                result = benchmark.benchmark_snapshot_capture(
                    args,
                    ["openvmm"],
                    retained_snapshot_path=retained,
                )

            self.assertEqual(result["samples_ms"], [2.0, 3.0])
            self.assertEqual(result["peak_rss_samples_bytes"], [2048, 3072])
            self.assertEqual(result["peak_rss_p50_bytes"], 2560)
            self.assertEqual(result["peak_rss_max_bytes"], 3072)
            self.assertEqual(observed_paths[-1], retained)
            self.assertTrue(retained.is_dir())

    def test_snapshot_restore_reuses_provided_snapshot(self):
        with tempfile.TemporaryDirectory() as temporary:
            snapshot = Path(temporary) / "snapshot"
            snapshot.mkdir()
            args = argparse.Namespace(
                warmups=1,
                runs=2,
                timeout=3.0,
                teardown_mode="guest-exit",
                network_profile=None,
            )
            expected = {"p50_ms": 1.0}
            with (
                patch.object(benchmark, "capture_snapshot") as capture,
                patch.object(benchmark, "benchmark", return_value=expected) as run,
            ):
                result = benchmark.benchmark_snapshot_restore(
                    args,
                    Path("openvmm"),
                    "whp",
                    ["openvmm", "--kernel", "vmlinux"],
                    snapshot_path=snapshot,
                )

            self.assertIs(result, expected)
            capture.assert_not_called()
            self.assertIn(str(snapshot), run.call_args.args[0])
            self.assertEqual(run.call_args.kwargs["marker"], benchmark.RESTORE_MARKER)
            self.assertEqual(run.call_args.kwargs["teardown_mode"], "guest-exit")

    def test_native_e2e_measures_and_reuses_snapshot_capture(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            openvmm = root / "openvmm"
            executable = openvmm / "target" / "release" / "openvmm"
            build_dir = root / "build"
            executable.parent.mkdir(parents=True)
            build_dir.mkdir()
            (openvmm / "Cargo.toml").touch()
            executable.touch()
            (build_dir / "vmlinux").touch()
            (build_dir / "initramfs.cpio.gz").touch()
            output = root / "e2e.json"
            args = nvx.parse_args(
                [
                    "benchmark",
                    "--suite",
                    "e2e",
                    "--backend",
                    "kvm",
                    "--openvmm-dir",
                    str(openvmm),
                    "--nvx-dir",
                    str(root),
                    "--cpus",
                    "0",
                    "--warmups",
                    "1",
                    "--runs",
                    "1",
                    "--skip-build",
                    "--output",
                    str(output),
                ]
            )
            run_result: benchmark.BenchmarkResult = {
                "samples_ms": [10.0],
                "p50_ms": 10.0,
                "min_ms": 10.0,
                "max_ms": 10.0,
                "wall_samples_ms": [12.0],
                "wall_p50_ms": 12.0,
                "wall_min_ms": 12.0,
                "wall_max_ms": 12.0,
                "peak_rss_samples_bytes": [1024],
                "peak_rss_p50_bytes": 1024,
                "peak_rss_min_bytes": 1024,
                "peak_rss_max_bytes": 1024,
                "teardown_samples_ms": [2.0],
                "teardown_completed_samples_ms": [2.0],
                "teardown_timeout_count": 0,
                "teardown_timeout_seconds": 5.0,
                "teardown_p50_ms": 2.0,
                "teardown_min_ms": 2.0,
                "teardown_max_ms": 2.0,
            }
            capture_result: benchmark.SnapshotCaptureResult = {
                "samples_ms": [3.0],
                "p50_ms": 3.0,
                "min_ms": 3.0,
                "max_ms": 3.0,
                "request_to_publication_samples_ms": [3.0],
                "request_to_publication_p50_ms": 3.0,
                "request_to_publication_min_ms": 3.0,
                "request_to_publication_max_ms": 3.0,
                "post_publication_exit_samples_ms": [1.0],
                "post_publication_exit_p50_ms": 1.0,
                "post_publication_exit_min_ms": 1.0,
                "post_publication_exit_max_ms": 1.0,
                "peak_rss_samples_bytes": [2048],
                "peak_rss_p50_bytes": 2048,
                "peak_rss_min_bytes": 2048,
                "peak_rss_max_bytes": 2048,
            }
            with (
                patch.object(benchmark, "benchmark", return_value=run_result),
                patch.object(
                    benchmark,
                    "benchmark_snapshot_capture",
                    return_value=capture_result,
                ) as capture,
                patch.object(
                    benchmark,
                    "benchmark_snapshot_restore",
                    return_value=run_result,
                ) as restore,
            ):
                self.assertEqual(benchmark.run_native_linux(args), 0)

            retained = capture.call_args.kwargs["retained_snapshot_path"]
            self.assertIsInstance(retained, Path)
            self.assertEqual(restore.call_args.kwargs["snapshot_path"], retained)
            document = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(
                document["snapshot_capture"]["kvm"]["peak_rss_p50_bytes"],
                2048,
            )

    def test_kvm_worker_e2e_restores_the_measured_snapshot(self):
        args = argparse.Namespace(
            _stage_dir="/tmp/stage",
            cpus="0",
            memory_mib=128,
            net=None,
            suite="e2e",
            warmups=1,
            runs=1,
            timeout=1.0,
            teardown_mode="guest-exit",
        )
        run_result = cast(benchmark.BenchmarkResult, {"p50_ms": 10.0})
        capture_result = cast(benchmark.SnapshotCaptureResult, {"p50_ms": 3.0})
        with (
            patch.object(benchmark, "benchmark", return_value=run_result),
            patch.object(
                benchmark,
                "benchmark_snapshot_capture",
                return_value=capture_result,
            ) as capture,
            patch.object(
                benchmark,
                "benchmark_snapshot_restore",
                return_value=run_result,
            ) as restore,
            patch.object(benchmark, "print_summary"),
            patch.object(benchmark, "print_snapshot_summary"),
        ):
            self.assertEqual(benchmark.run_kvm_worker(args), 0)

        retained = capture.call_args.kwargs["retained_snapshot_path"]
        self.assertIsInstance(retained, Path)
        self.assertEqual(restore.call_args.kwargs["snapshot_path"], retained)

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
                {"cold-start.log", "virtfs.log", "shell-snapshot.log", "network.log"},
            )
            cold.assert_called_once()
            self.assertEqual(virtfs.call_args.kwargs["runs"], 3)
            shell.assert_called_once()
            network.assert_called_once()

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
                {"cold-start.log", "virtfs.log", "shell-snapshot.log", "network.log"},
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

            output = repository / "data" / "runs" / platform
            self.assertEqual(
                {path.name for path in output.iterdir()},
                {"cold-start.log", "virtfs.log", "shell-snapshot.log", "network.log"},
            )

    def test_package_command_forwards_parsed_options(self):
        args = nvx.parse_args(
            [
                "package",
                "--version",
                "2.0.0",
                "--destination",
                "dist/custom",
                "--binary-only",
                "--force",
            ]
        )

        with patch.object(nvx, "package_release") as package_release:
            args.handler(args)

        package_release.assert_called_once_with(
            version="2.0.0",
            destination=Path("dist/custom"),
            include_source=False,
            force=True,
        )

    def test_download_command_selects_host_release(self):
        args = nvx.parse_args(["download", "--repository", "example/nvx"])
        expected_platform = "windows-whp" if os.name == "nt" else "linux-kvm"

        with patch.object(nvx, "download_latest_release") as download_release:
            args.handler(args)

        download_release.assert_called_once_with("example/nvx", expected_platform)


class ReleaseTests(unittest.TestCase):
    def test_selects_latest_matching_prerelease_asset(self):
        releases = [
            {
                "draft": True,
                "tag_name": "v1.2.4-draft",
                "assets": [
                    {
                        "name": "nvx-1.2.4-linux-kvm.tar.gz",
                        "url": "https://api.example.invalid/draft",
                        "size": 100,
                    }
                ],
            },
            {
                "draft": False,
                "prerelease": True,
                "tag_name": "v1.2.3-dev.abc123",
                "assets": [
                    {
                        "name": "nvx-1.2.3-windows-whp.zip",
                        "url": "https://api.example.invalid/windows",
                        "size": 200,
                    },
                    {
                        "name": "nvx-1.2.3-linux-kvm.tar.gz",
                        "url": "https://api.example.invalid/linux",
                        "size": 300,
                    },
                ],
            },
        ]
        response = io.BytesIO(json.dumps(releases).encode("utf-8"))

        with patch(
            "nvx_tools.release.urllib.request.urlopen",
            return_value=response,
        ):
            asset = release._latest_release_asset(
                "example/nvx",
                "linux-kvm",
                "token",
            )

        self.assertEqual(asset.tag, "v1.2.3-dev.abc123")
        self.assertEqual(asset.name, "nvx-1.2.3-linux-kvm.tar.gz")
        self.assertEqual(asset.url, "https://api.example.invalid/linux")
        self.assertEqual(asset.size, 300)

    def test_binary_package_stages_files_and_checksums(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            build_dir = root / "build"
            source_dir = build_dir / "sources"
            openvmm_dir = root / "openvmm"
            binary = openvmm_dir / "target" / "release" / "openvmm.exe"
            guest_names = (
                "vmlinux",
                "vmlinux.config",
                "initramfs.cpio.gz",
                "initramfs.cpio.gz.packages.json",
            )
            release_files = (
                "LICENSE",
                "README.md",
                "SOURCE-MANIFEST.json",
                "THIRD_PARTY_NOTICES.md",
            )
            for path in (
                binary,
                *(build_dir / name for name in guest_names),
                *(root / name for name in release_files),
                openvmm_dir / "LICENSE",
                root / "kernel" / "COPYING-LINUX",
            ):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(path.name.encode("ascii"))
            destination = root / "staged"
            stderr = io.StringIO()

            def artifact_path(name: str) -> Path:
                return build_dir / name

            with (
                patch.object(release, "REPO_ROOT", root),
                patch.object(release, "SOURCE_DIR", source_dir),
                patch.object(release, "OPENVMM_DIR", openvmm_dir),
                patch.object(
                    release,
                    "artifact_path",
                    side_effect=artifact_path,
                ),
                patch.object(release, "openvmm_binary_path", return_value=binary),
                patch("sys.stderr", stderr),
            ):
                release.package_release(
                    version="1.0.0",
                    destination=destination,
                    include_source=False,
                    force=False,
                )

            self.assertTrue((destination / "bin" / binary.name).is_file())
            for name in guest_names:
                self.assertTrue((destination / "guest" / name).is_file())
            common.verify_sha256_sums(destination)
            self.assertIn("binary-only package", stderr.getvalue())

    def test_release_archive_installs_runtime_layout(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            package_root = root / "package" / "nvx-1.2.3-test"
            binary_name = "openvmm.exe" if os.name == "nt" else "openvmm"
            binary = package_root / "bin" / binary_name
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"openvmm")
            for name in release.GUEST_RELEASE_NAMES:
                guest = package_root / "guest" / name
                guest.parent.mkdir(parents=True, exist_ok=True)
                guest.write_bytes(name.encode("ascii"))
            common.write_sha256_sums(package_root)

            if os.name == "nt":
                archive_path = root / "nvx-1.2.3-windows-whp.zip"
                with zipfile.ZipFile(archive_path, "w") as package:
                    for path in package_root.rglob("*"):
                        package.write(path, path.relative_to(package_root.parent))
            else:
                archive_path = root / "nvx-1.2.3-linux-kvm.tar.gz"
                with tarfile.open(archive_path, "w:gz") as package:
                    package.add(package_root, arcname=package_root.name)

            build_dir = root / "runtime" / "build"
            binary_destination = (
                root / "runtime" / "openvmm" / "target" / "release" / binary_name
            )

            def artifact_path(name: str) -> Path:
                return build_dir / name

            with (
                patch.object(
                    release,
                    "artifact_path",
                    side_effect=artifact_path,
                ),
                patch.object(
                    release,
                    "openvmm_binary_path",
                    return_value=binary_destination,
                ),
            ):
                release._install_release_archive(archive_path)

            self.assertEqual(binary_destination.read_bytes(), b"openvmm")
            for name in release.GUEST_RELEASE_NAMES:
                self.assertEqual(
                    (build_dir / name).read_bytes(),
                    name.encode("ascii"),
                )


class SharedFileTests(unittest.TestCase):
    def test_checksum_manifest_detects_modified_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            payload = root / "nested" / "payload.txt"
            payload.parent.mkdir()
            payload.write_text("original", encoding="ascii")

            common.write_sha256_sums(root)
            common.verify_sha256_sums(root)
            payload.write_text("modified", encoding="ascii")

            with self.assertRaisesRegex(common.ScriptError, "checksum mismatch"):
                common.verify_sha256_sums(root)

    def test_source_archives_are_reproducible(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            (source / "payload.txt").write_text("payload", encoding="ascii")
            cache = source / "__pycache__"
            cache.mkdir()
            (cache / "ignored.pyc").write_bytes(b"cache")
            first = root / "first.tar.gz"
            second = root / "second.tar.gz"

            archive.create_reproducible_tar_gz(first, [(source, "bundle")])
            os.utime(source / "payload.txt", (1000, 1000))
            archive.create_reproducible_tar_gz(second, [(source, "bundle")])

            self.assertEqual(first.read_bytes(), second.read_bytes())
            with tarfile.open(first, "r:gz") as package:
                members = package.getmembers()
            self.assertNotIn("bundle/__pycache__", {member.name for member in members})
            self.assertTrue(all(member.mtime == 0 for member in members))
            self.assertTrue(all(member.uid == member.gid == 0 for member in members))


class DownloadTests(unittest.TestCase):
    def test_retries_checksum_mismatch(self):
        payload = b"verified archive"
        expected_sha256 = hashlib.sha256(payload).hexdigest()
        responses = [io.BytesIO(b"corrupt archive"), io.BytesIO(payload)]

        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "archive.tar.xz"
            with patch(
                "nvx_tools.common.urllib.request.urlopen", side_effect=responses
            ) as urlopen:
                common.download(
                    "https://example.invalid/archive.tar.xz",
                    destination,
                    expected_sha256=expected_sha256,
                )

            self.assertEqual(destination.read_bytes(), payload)
            self.assertEqual(urlopen.call_count, 2)
            self.assertFalse(destination.with_name("archive.tar.xz.part").exists())

    def test_does_not_publish_checksum_mismatch(self):
        payload = b"verified archive"
        expected_sha256 = hashlib.sha256(payload).hexdigest()
        responses = [io.BytesIO(b"corrupt archive") for _ in range(3)]

        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "archive.tar.xz"
            with (
                patch(
                    "nvx_tools.common.urllib.request.urlopen",
                    side_effect=responses,
                ),
                self.assertRaisesRegex(common.ScriptError, "SHA-256 is"),
            ):
                common.download(
                    "https://example.invalid/archive.tar.xz",
                    destination,
                    expected_sha256=expected_sha256,
                )

            self.assertFalse(destination.exists())
            self.assertFalse(destination.with_name("archive.tar.xz.part").exists())

    def test_replaces_mismatched_cached_archive(self):
        payload = b"verified archive"
        expected_sha256 = hashlib.sha256(payload).hexdigest()

        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "archive.tar.xz"
            destination.write_bytes(b"corrupt archive")

            def write_verified(
                _url: str,
                output: Path,
                *,
                expected_sha256: str,
            ) -> None:
                self.assertFalse(output.exists())
                self.assertEqual(
                    expected_sha256,
                    hashlib.sha256(payload).hexdigest(),
                )
                output.write_bytes(payload)

            with patch.object(build, "download", side_effect=write_verified):
                build._download_verified(
                    "https://example.invalid/archive.tar.xz",
                    destination,
                    expected_sha256,
                )

            self.assertEqual(destination.read_bytes(), payload)


if __name__ == "__main__":
    unittest.main()
