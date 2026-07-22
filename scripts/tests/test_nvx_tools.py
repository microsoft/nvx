from __future__ import annotations

import contextlib
import io
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import call, patch


SCRIPTS_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_DIR))

from nvx_tools.backends.linux import LinuxBackend
from nvx_tools.backends.base import HostBackend
from nvx_tools.backends.windows import WindowsBackend
from nvx_tools.build import (
    DockerBuildConfig,
    build_docker_artifacts,
    docker_build_command,
)
from nvx_tools.benchmarks import (
    NetworkSnapshotConfig,
    benchmark_network_snapshot,
    format_rate_median,
    parse_data_checksum,
    parse_dd_rate,
)
from nvx_tools.common import CommandResult, REPO_ROOT, ScriptError, remove_tree
from nvx_tools.cli import build_parser
from nvx_tools.vm import (
    BOOT_MARKER,
    BootTestConfig,
    SnapshotConfig,
    VmConfig,
    cold_boot_args,
    format_median,
    invoke_vm_metric,
    boot_test,
)


class BackendTests(unittest.TestCase):
    def test_platform_defaults_are_explicit(self) -> None:
        linux = LinuxBackend()
        windows = WindowsBackend()

        self.assertEqual(linux.executable_suffix, "")
        self.assertEqual(linux.clocksource, "kvm-clock")
        self.assertTrue(linux.supports_vcpus)
        self.assertEqual(windows.executable_suffix, ".exe")
        self.assertEqual(windows.clocksource, "tsc")
        self.assertFalse(windows.supports_vcpus)

    def test_linux_artifact_root_can_use_runner_scratch(self) -> None:
        with patch.dict(os.environ, {"NVX_ARTIFACT_DIR": "/runner/temp"}):
            backend = LinuxBackend()

        self.assertEqual(backend.artifact_dir, Path("/runner/temp"))

    def test_network_snapshot_keeps_256_mib_default(self) -> None:
        args = build_parser().parse_args(["bench-net-snapshot"])

        self.assertEqual(args.mem, 256)

    def test_shell_snapshot_uses_base_initramfs(self) -> None:
        args = build_parser().parse_args(["bench-snapshot-shell"])

        self.assertFalse(args.python_initrd)

    def test_network_snapshot_exposes_vcpus(self) -> None:
        args = build_parser().parse_args(["bench-net-snapshot", "--vcpus", "3"])

        self.assertEqual(args.vcpus, 3)

    def test_snapshot_boot_rejects_ignored_run_count(self) -> None:
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            build_parser().parse_args(["snapshot-boot", "--runs", "2"])

    def test_shell_snapshot_rejects_ignored_memory_option(self) -> None:
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            build_parser().parse_args(["bench-snapshot-shell", "--mem", "777"])

    def test_snapshot_benchmarks_honor_environment_run_count(self) -> None:
        with patch.dict(os.environ, {"N": "3"}):
            args = build_parser().parse_args(["snapshot-demo"])

        self.assertEqual(args.runs, 3)

    def test_unrelated_invalid_numeric_environment_does_not_break_help(self) -> None:
        with patch.dict(os.environ, {"N": "not-a-number"}):
            with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(
                SystemExit
            ) as exit_info:
                build_parser().parse_args(["build-linux-artifacts", "--help"])

        self.assertEqual(exit_info.exception.code, 0)

    def test_virtfs_honors_image_size_environment(self) -> None:
        with patch.dict(os.environ, {"IMG_MB": "777"}):
            args = build_parser().parse_args(["bench-virtfs"])

        self.assertEqual(args.image_mib, 777)

    @patch("nvx_tools.backends.linux._is_root", return_value=True)
    @patch("nvx_tools.backends.linux.shutil.which", return_value="/usr/sbin/ip")
    @patch("nvx_tools.backends.linux.run_capture")
    def test_linux_root_tap_cleanup_does_not_use_sudo(
        self, run: object, which: object, is_root: object
    ) -> None:
        run.side_effect = [
            CommandResult(
                ("ip",),
                0,
                b"7: llx2: <BROADCAST>\n8: llx1: <BROADCAST>\n",
                b"",
            ),
            CommandResult(("ip",), 0, b"", b""),
            CommandResult(("ip",), 0, b"", b""),
        ]

        LinuxBackend().cleanup_network()

        self.assertEqual(
            run.call_args_list,
            [
                call(["/usr/sbin/ip", "-o", "link", "show"]),
                call(
                    [
                        "/usr/sbin/ip",
                        "tuntap",
                        "del",
                        "dev",
                        "llx1",
                        "mode",
                        "tap",
                    ]
                ),
                call(
                    [
                        "/usr/sbin/ip",
                        "tuntap",
                        "del",
                        "dev",
                        "llx2",
                        "mode",
                        "tap",
                    ]
                ),
            ],
        )


class BuildTests(unittest.TestCase):
    def test_docker_artifact_command_carries_all_version_inputs(self) -> None:
        config = DockerBuildConfig(Path("output"), "6.18.99", "3.24.9", "v3.24")
        command = [str(value) for value in docker_build_command(config, "artifacts")]

        self.assertEqual(command[:2], ["docker", "build"])
        self.assertIn("--target", command)
        self.assertEqual(command[command.index("--target") + 1], "artifacts")
        self.assertIn("KVER=6.18.99", command)
        self.assertIn("AVER=3.24.9", command)
        self.assertIn("ABRANCH=v3.24", command)
        self.assertIn(
            f"type=local,dest={(REPO_ROOT / 'output').resolve()}", command
        )

    def test_python_docker_command_omits_unused_kernel_version(self) -> None:
        config = DockerBuildConfig(Path("output"), "unused", "3.24.9", "edge")
        command = [str(value) for value in docker_build_command(config, "python-artifacts")]

        self.assertNotIn("KVER=unused", command)
        self.assertIn("AVER=3.24.9", command)

    @patch("nvx_tools.build.require_tool")
    @patch("nvx_tools.build.run_checked")
    def test_python_docker_build_copies_to_explicit_output(
        self, run: object, require: object
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            destination = root / "export"
            destination.mkdir()
            source = destination / "initramfs-python.cpio.gz"
            source.write_bytes(b"python-initramfs")
            output = root / "custom" / "python.cpio.gz"

            build_docker_artifacts(
                DockerBuildConfig(destination),
                python_only=True,
                output=output,
            )

            self.assertEqual(output.read_bytes(), b"python-initramfs")


class BenchmarkParserTests(unittest.TestCase):
    def test_remove_tree_rejects_protected_directories_and_ancestors(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            repository = root / "repo"
            repository.mkdir()
            with patch("nvx_tools.common.REPO_ROOT", repository):
                with self.assertRaisesRegex(ScriptError, "unsafe snapshot"):
                    remove_tree(repository, label="snapshot")
                with self.assertRaisesRegex(ScriptError, "unsafe snapshot"):
                    remove_tree(root, label="snapshot")
            self.assertTrue(repository.is_dir())

    def test_remove_tree_allows_snapshot_below_repository(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            repository = Path(temporary) / "repo"
            snapshot = repository / "build" / "snapshot"
            snapshot.mkdir(parents=True)
            with patch("nvx_tools.common.REPO_ROOT", repository):
                remove_tree(snapshot, label="snapshot")
            self.assertFalse(snapshot.exists())
            self.assertTrue(repository.is_dir())

    def test_network_snapshot_rejects_zero_runs_before_starting_vm(self) -> None:
        with self.assertRaisesRegex(ScriptError, "at least 1"):
            benchmark_network_snapshot(
                NetworkSnapshotConfig(
                    Path("kernel"), Path("initrd"), Path("snapshot"), runs=0
                ),
                WindowsBackend(),
            )

    def test_busybox_rates_are_normalized_to_megabytes_per_second(self) -> None:
        text = (
            "64+0 records out\n67108864 bytes copied, 0.2 s, 320.0MB/s\n"
            "64+0 records out\n67108864 bytes copied, 0.02 s, 3.2GB/s\n"
        )
        self.assertEqual(parse_dd_rate(text, 1), 320.0)
        self.assertEqual(parse_dd_rate(text, 2), 3200.0)

    def test_checksum_requires_data_bin_line(self) -> None:
        self.assertEqual(parse_data_checksum("3975907619 67108864 /mnt/host/data.bin\n"), "3975907619")
        self.assertIsNone(parse_data_checksum("3975907619 1 other.bin\n"))

    def test_rate_output_keeps_performance_parser_contract(self) -> None:
        formatted = format_rate_median([100.0, 300.0, 200.0])
        self.assertRegex(formatted, r"200\.0 MB/s")


class RetainedWorkflowTests(unittest.TestCase):
    def test_hcn_afxdp_snapshot_runs_whp_selftest_and_visible_guests(self) -> None:
        afxdp = (REPO_ROOT / "scripts" / "benchmark-hcn-afxdp-snapshot.ps1").read_text(
            encoding="utf-8"
        )
        self.assertIn("function Invoke-MicrovmSelfTest", afxdp)
        self.assertIn("RedirectStandardError = $true", afxdp)
        self.assertIn("if ($stderr) { Write-Output $stderr }", afxdp)
        self.assertIn("Invoke-MicrovmSelfTest", afxdp)
        snapshot_runs = afxdp.split(
            'Write-Output "HCN AF_XDP networking + snapshot benchmark', 1
        )[1]
        self.assertNotIn("'--quiet'", snapshot_runs)

        network_guest = (REPO_ROOT / "alpine" / "net-hello.py").read_text(
            encoding="utf-8"
        )
        self.assertIn("def link_ok(host, port, timeout=3, attempts=10):", network_guest)

    def test_shared_virtfs_allows_slow_hardware_runs(self) -> None:
        source = (REPO_ROOT / "scripts" / "nvx_tools" / "benchmarks.py").read_text(
            encoding="utf-8"
        )
        guest_run = source.split("def _guest_run(", 1)[1].split(
            "\ndef parse_dd_rate", 1
        )[0]
        self.assertIn("timeout=300", guest_run)

    def test_guest_network_mac_match_ignores_hcn_separators(self) -> None:
        for name in ("init", "init.python"):
            script = (REPO_ROOT / "alpine" / name).read_text(encoding="utf-8")
            self.assertIn("tr -d ':' | tr -d '-'", script)
            self.assertIn('fatal "virtnet: no non-loopback interface appeared', script)
            self.assertIn('while [ "$tries" -lt 600 ]', script)

class CiWorkflowParityTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = (REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )

    def job(self, name: str, next_name: str) -> str:
        return self.workflow.split(f"  {name}:\n", 1)[1].split(
            f"\n  {next_name}:\n", 1
        )[0]

    def test_kvm_and_whp_run_the_shared_test_and_benchmark_contract(self) -> None:
        linux = self.job("linux", "windows")
        windows = self.job("windows", "windows-hcn-afxdp")

        for job in (linux, windows):
            self.assertIn("cargo test --release", job)
            self.assertIn("scripts/test_performance.py", job)
            self.assertIn("test-boot", job)
            self.assertIn("measure-coldstart", job)
            self.assertIn("bench-virtfs", job)
            self.assertIn("snapshot-demo", job)
            self.assertIn("bench-snapshot-shell --runs 5", job)
            self.assertIn("bench-net-snapshot-py", job)
            self.assertIn("snapshot-boot", job)

        self.assertIn("scripts/nvx.py bench-net-snapshot |", linux)
        self.assertIn("scripts\\nvx.py bench-net-snapshot --runs 5", windows)

        self.assertNotIn("skipping networking benchmark", linux)
        self.assertNotIn("skipping networked Python smoke test", linux)

    def test_privileged_lane_runs_hcn_afxdp_benchmarks(self) -> None:
        hcn_afxdp = self.job("windows-hcn-afxdp", "performance-gate")

        self.assertIn("needs: [artifacts, windows]", hcn_afxdp)
        self.assertIn("github.event_name == 'pull_request'", hcn_afxdp)
        self.assertIn("--require-shared-suite", hcn_afxdp)
        for command in (
            "measure-coldstart --runs 5",
            "bench-virtfs --runs 3",
            "snapshot-demo --runs 5",
            "bench-snapshot-shell --runs 5",
            "benchmark-hcn-afxdp-snapshot.ps1",
        ):
            self.assertIn(command, hcn_afxdp)
        self.assertIn("-Runs 5", hcn_afxdp)
        self.assertIn("--platform windows-hcn-afxdp", hcn_afxdp)

    def test_each_backend_job_publishes_its_benchmark_table(self) -> None:
        jobs = {
            "linux-kvm": (
                "Linux / KVM",
                self.job("linux", "windows"),
            ),
            "windows-whp": (
                "Windows / WHP",
                self.job("windows", "windows-hcn-afxdp"),
            ),
            "windows-hcn-afxdp": (
                "Windows / HCN AF_XDP",
                self.job("windows-hcn-afxdp", "performance-gate"),
            ),
        }

        for platform, (step_name, job) in jobs.items():
            publish_step = job.split(
                f"- name: Publish {step_name} benchmark table\n", 1
            )[1].split("\n      - name:", 1)[0]
            self.assertIn("performance.py collect", publish_step)
            self.assertIn(f"--platform {platform}", publish_step)
            self.assertIn("--summary", publish_step)
            self.assertIn("--require-shared-suite", publish_step)
            self.assertIn("GITHUB_STEP_SUMMARY", publish_step)

    def test_main_persistence_includes_successful_privileged_lanes(self) -> None:
        persistence = self.workflow.split("  performance-persist:\n", 1)[1]

        self.assertIn(
            "needs: [linux, windows, windows-hcn-afxdp]",
            persistence,
        )
        self.assertIn("--platform windows-hcn-afxdp", persistence)

    def test_performance_gate_uses_absolute_latency_tolerance(self) -> None:
        gate = self.job("performance-gate", "performance-persist")

        self.assertIn("PERFORMANCE_REGRESSION_ABSOLUTE_TOLERANCE_MS: 5", self.workflow)
        self.assertIn(
            '--absolute-tolerance-ms "${PERFORMANCE_REGRESSION_ABSOLUTE_TOLERANCE_MS}"',
            gate,
        )


class BootTestTests(unittest.TestCase):
    def test_boot_contract_and_marker_parsing(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            backend = HostBackend(
                "windows-whp", ".exe", root / "build", "tsc", False, root
            )
            executable = backend.executable()
            kernel = root / "vmlinux"
            initrd = root / "initramfs.cpio.gz"
            executable.parent.mkdir(parents=True, exist_ok=True)
            executable.touch(exist_ok=True)
            kernel.touch()
            initrd.touch()
            calls: list[tuple[tuple[object, ...], dict[str, object]]] = []

            def fake_runner(*args: object, **kwargs: object) -> CommandResult:
                calls.append((args, kwargs))
                return CommandResult(
                    tuple(str(value) for value in args[0]),
                    0,
                    (
                        f"{BOOT_MARKER}\n3.24.1\n"
                        "Linux alpine 6.18.38 #1 x86_64 GNU/Linux\n"
                    ).encode(),
                    b"",
                )

            report = boot_test(
                BootTestConfig(kernel, initrd, mem=384, timeout=7),
                backend,
                runner=fake_runner,
            )

        self.assertTrue(report.passed)
        self.assertEqual(report.alpine_release, "3.24.1")
        self.assertIn("x86_64", report.uname or "")
        command = tuple(str(value) for value in calls[0][0][0])
        self.assertIn("--kernel", command)
        self.assertIn("--initrd", command)
        self.assertEqual(command[command.index("--mem") + 1], "384")
        self.assertEqual(calls[0][1]["timeout"], 7)
        self.assertEqual(
            calls[0][1]["input_text"],
            "cat /etc/alpine-release\nuname -a\nreboot -f\n",
        )

    def test_missing_marker_fails_even_when_process_exits_zero(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            backend = HostBackend(
                "linux-kvm", "", root / "build", "kvm-clock", True, root
            )
            executable = backend.executable()
            kernel = root / "vmlinux"
            initrd = root / "initramfs.cpio.gz"
            executable.parent.mkdir(parents=True, exist_ok=True)
            executable.touch(exist_ok=True)
            kernel.touch()
            initrd.touch()

            def fake_runner(*args: object, **kwargs: object) -> CommandResult:
                return CommandResult(("microvm",), 0, b"boot stopped early", b"")

            report = boot_test(
                BootTestConfig(kernel, initrd), backend, runner=fake_runner
            )

        self.assertFalse(report.passed)


class VmWorkflowTests(unittest.TestCase):
    def test_linux_cold_boot_adds_vcpus_but_windows_does_not(self) -> None:
        config = VmConfig(Path("kernel"), Path("initrd"), vcpus=2)
        linux = [str(value) for value in cold_boot_args(Path("microvm"), config, LinuxBackend())]
        self.assertEqual(linux[linux.index("--vcpus") + 1], "2")

        with self.assertRaisesRegex(ValueError, "exactly one vCPU"):
            cold_boot_args(Path("microvm.exe"), config, WindowsBackend())

    def test_metric_parser_accepts_stdout_or_stderr(self) -> None:
        def fake_runner(*args: object, **kwargs: object) -> CommandResult:
            return CommandResult(("microvm",), 0, b"", b"restore: 12.75 ms")

        self.assertEqual(invoke_vm_metric(["microvm"], runner=fake_runner), 12.75)

    def test_metric_parser_rejects_timeout_after_marker(self) -> None:
        def fake_runner(*args: object, **kwargs: object) -> CommandResult:
            return CommandResult(
                ("microvm",), None, b"cold-start: 12.5 ms", b"", timed_out=True
            )

        with self.assertRaisesRegex(ScriptError, "timed out"):
            invoke_vm_metric(["microvm"], runner=fake_runner)

    def test_metric_parser_rejects_nonzero_exit_after_marker(self) -> None:
        def fake_runner(*args: object, **kwargs: object) -> CommandResult:
            return CommandResult(("microvm",), 7, b"restore: 12.5 ms", b"crashed")

        with self.assertRaisesRegex(ScriptError, "exited 7"):
            invoke_vm_metric(["microvm"], runner=fake_runner)

    def test_shared_median_averages_even_sample_count(self) -> None:
        self.assertIn("2.5 ms", format_median([1.0, 2.0, 3.0, 100.0]))


if __name__ == "__main__":
    unittest.main()
