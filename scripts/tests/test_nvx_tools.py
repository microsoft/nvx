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
    HcsNetworkSnapshotConfig,
    NetworkSnapshotConfig,
    _capture_hcs_snapshot,
    _collect_hcs_timings,
    _validate_hcs_snapshot,
    benchmark_hcs_snapshot_python,
    benchmark_hcs_snapshot_shell,
    benchmark_hcs_network_snapshot_python,
    benchmark_network_snapshot,
    format_rate_median,
    hcs_cold_boot_args,
    hcs_snapshot_capture_args,
    hcs_snapshot_restore_args,
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

    def test_hcs_snapshot_benchmarks_select_expected_guest_images(self) -> None:
        shell = build_parser().parse_args(["bench-hcs-snapshot-shell"])
        coldstart = build_parser().parse_args(["bench-hcs-coldstart"])
        virtfs = build_parser().parse_args(["bench-hcs-virtfs"])
        python = build_parser().parse_args(["bench-hcs-snapshot-py"])
        network = build_parser().parse_args(["bench-hcs-net-snapshot-py"])

        self.assertFalse(shell.python_initrd)
        self.assertEqual(shell.memories, "64 128 256 512")
        self.assertEqual(shell.snapshot_name, "hcs-shellsnap")
        self.assertFalse(coldstart.python_initrd)
        self.assertFalse(virtfs.python_initrd)
        self.assertEqual(virtfs.payload_mib, 64)
        self.assertTrue(python.python_initrd)
        self.assertEqual(python.snapshot_name, "hcs-pysnap")
        self.assertTrue(network.python_initrd)
        self.assertEqual(network.snapshot_name, "hcs-net-pysnap")
        self.assertEqual(network.net, "10.0.0.2/24")

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
    def test_hcs_missing_timing_marker_preserves_child_diagnostics(self) -> None:
        result = CommandResult(("microvm",), 0, b"guest reached restore point", b"")
        output = io.StringIO()
        with (
            patch(
                "nvx_tools.benchmarks._run_timed",
                return_value=(None, 12.0, result),
            ),
            contextlib.redirect_stdout(output),
        ):
            with self.assertRaisesRegex(ScriptError, "did not report a timing marker"):
                _collect_hcs_timings(["microvm"], 1, "HCS restore", 30)
        self.assertIn("guest reached restore point", output.getvalue())

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

    def test_hcs_benchmarks_reject_non_windows_host_before_vm_setup(self) -> None:
        config = SnapshotConfig(
            Path("kernel"), Path("initrd"), Path("snapshot"), runs=1
        )
        with self.assertRaisesRegex(ScriptError, "Windows host"):
            benchmark_hcs_snapshot_shell(config, LinuxBackend(), [256])
        with self.assertRaisesRegex(ScriptError, "Windows host"):
            benchmark_hcs_snapshot_python(config, LinuxBackend())

    def test_hcs_snapshot_artifact_contract_and_failed_capture_cleanup(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            snapshot = root / "snapshot"
            snapshot.mkdir()
            (snapshot / "runtime.vmrs").write_bytes(b"opaque state")
            (snapshot / "manifest.json").write_text(
                '{"format":"NVXHCSS1","version":2,"backend":"hcs"}',
                encoding="utf-8",
            )
            manifest, state = _validate_hcs_snapshot(snapshot)
            self.assertEqual(manifest.name, "manifest.json")
            self.assertEqual(state.name, "runtime.vmrs")
            with self.assertRaisesRegex(ScriptError, "network identity"):
                _validate_hcs_snapshot(snapshot, require_network=True)

            (snapshot / "manifest.json").write_text("{}", encoding="utf-8")
            with self.assertRaisesRegex(ScriptError, "manifest contract"):
                _validate_hcs_snapshot(snapshot)

            (snapshot / "manifest.json").write_text("[]", encoding="utf-8")
            with self.assertRaisesRegex(ScriptError, "manifest contract"):
                _validate_hcs_snapshot(snapshot)

            with patch(
                "nvx_tools.benchmarks.run_capture",
                return_value=CommandResult(("microvm",), 0, b"", b""),
            ):
                with self.assertRaisesRegex(ScriptError, "incomplete"):
                    _capture_hcs_snapshot(["microvm"], snapshot, 1)
            self.assertFalse(snapshot.exists())

            output = io.StringIO()
            with (
                patch(
                    "nvx_tools.benchmarks.run_capture",
                    return_value=CommandResult(
                        ("microvm",), 9, b"", b"capture failed"
                    ),
                ),
                contextlib.redirect_stdout(output),
            ):
                with self.assertRaisesRegex(ScriptError, "snapshot capture exited 9"):
                    _capture_hcs_snapshot(["microvm"], snapshot, 1)
            self.assertIn("capture failed", output.getvalue())
            self.assertFalse(snapshot.exists())

    def test_hcs_snapshot_commands_use_hcs_native_contract(self) -> None:
        config = SnapshotConfig(
            Path("kernel"), Path("initrd"), Path("snapshot"), mem=384, runs=3
        )
        cold = [
            str(value)
            for value in hcs_cold_boot_args(
                Path("microvm.exe"), config, 384, "console=ttyS0", "READY"
            )
        ]
        capture = [
            str(value)
            for value in hcs_snapshot_capture_args(
                Path("microvm.exe"),
                config,
                Path("snapshot-384"),
                384,
                "console=ttyS0 shellsnap",
            )
        ]
        restore = [
            str(value)
            for value in hcs_snapshot_restore_args(
                Path("microvm.exe"), Path("snapshot-384"), "READY"
            )
        ]

        for command in (cold, capture, restore):
            self.assertEqual(command[1:3], ["--backend", "hcs"])
        self.assertIn("--kernel", cold)
        self.assertIn("--snapshot", capture)
        self.assertNotIn("--kernel", restore)
        self.assertNotIn("--initrd", restore)
        self.assertNotIn("--mem", restore)

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


class HcsBenchmarkWorkflowTests(unittest.TestCase):
    def test_hcs_plan9_guest_support_is_built_in_and_fail_fast(self) -> None:
        kernel = (REPO_ROOT / "kernel" / "config-microvm").read_text(
            encoding="utf-8"
        )
        for option in (
            "CONFIG_VSOCKETS=y",
            "CONFIG_HYPERV_VSOCKETS=y",
            "CONFIG_NET_9P=y",
            "CONFIG_NET_9P_FD=y",
            "CONFIG_NETFS_SUPPORT=y",
            "CONFIG_9P_FS=y",
        ):
            self.assertIn(option, kernel)

        init = (REPO_ROOT / "alpine" / "init").read_text(encoding="utf-8")
        helper = (REPO_ROOT / "alpine" / "hcs-plan9.c").read_text(
            encoding="utf-8"
        )
        build = (REPO_ROOT / "scripts" / "nvx_tools" / "build.py").read_text(
            encoding="utf-8"
        )
        self.assertIn('/sbin/hcs-plan9 "$vdir" "$vmode" "$vaname"', init)
        self.assertIn('fatal "virtfs: failed to mount HCS Plan9 share', init)
        self.assertIn("socket(AF_VSOCK, SOCK_STREAM, 0)", helper)
        self.assertIn("alarm(15)", helper)
        self.assertNotIn("noload", helper)
        self.assertIn('root / "sbin" / "hcs-plan9"', build)

        schema = (REPO_ROOT / "src" / "hcs" / "schema.rs").read_text(
            encoding="utf-8"
        )
        orchestration = (REPO_ROOT / "src" / "hcs" / "mod.rs").read_text(
            encoding="utf-8"
        )
        self.assertIn('resource_path: "VirtualMachine/Devices/Plan9/Shares"', schema)
        self.assertIn("Some(Plan9 { shares: Vec::new() })", schema)
        self.assertIn("system.start()?;", orchestration)
        self.assertLess(
            orchestration.index("system.start()?;"),
            orchestration.index('HcsModifyComputeSystem(add Plan9 share)'),
        )

    def test_shared_virtfs_allows_slow_hardware_runs(self) -> None:
        source = (REPO_ROOT / "scripts" / "nvx_tools" / "benchmarks.py").read_text(
            encoding="utf-8"
        )
        guest_run = source.split("def _guest_run(", 1)[1].split(
            "\ndef parse_dd_rate", 1
        )[0]
        self.assertIn("timeout=300", guest_run)

    def test_shell_and_python_workflows_run_preflight_capture_and_restore(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            backend = HostBackend(
                "windows-whp", ".exe", root / "build", "tsc", False, root
            )
            executable = backend.executable()
            executable.parent.mkdir(parents=True)
            executable.touch()
            kernel = root / "build" / "vmlinux"
            base_initrd = root / "build" / "initramfs.cpio.gz"
            python_initrd = root / "build" / "initramfs-python.cpio.gz"
            kernel.parent.mkdir(exist_ok=True)
            for artifact in (kernel, base_initrd, python_initrd):
                artifact.write_bytes(b"fixture")
            endpoint_config = root / "build" / "hcn-endpoint.json"
            endpoint_config.write_text("{}", encoding="utf-8")

            calls: list[tuple[str, ...]] = []
            timeouts: dict[tuple[str, ...], object] = {}
            graceful_timeouts: dict[tuple[str, ...], object] = {}

            def fake_runner(args: object, **kwargs: object) -> CommandResult:
                command = tuple(str(value) for value in args)
                calls.append(command)
                timeouts[command] = kwargs.get("timeout")
                graceful_timeouts[command] = kwargs.get("graceful_timeout")
                if "--snapshot" in command:
                    snapshot = Path(command[command.index("--snapshot") + 1])
                    snapshot.mkdir()
                    (snapshot / "manifest.json").write_text(
                        '{"format":"NVXHCSS1","version":3,"backend":"hcs",'
                        '"network":{"network_id":"n","endpoint_id":"e",'
                        '"adapter_id":"a","guest_ip":"10.0.0.2","prefix":24,'
                        '"gateway":"10.0.0.1","mac_address":"00-15-5D-00-00-01"}}',
                        encoding="utf-8",
                    )
                    (snapshot / "runtime.vmrs").write_bytes(b"opaque state")
                    return CommandResult(command, 0, b"", b"")
                if "--selftest" in command:
                    return CommandResult(command, 0, b"", b"")
                metric = (
                    b"restore: 5.0 ms"
                    if "--restore" in command
                    else b"cold-start: 20.0 ms\nHELLOPY-NET OK"
                )
                return CommandResult(command, 0, b"", metric)

            output = io.StringIO()
            with (
                patch("nvx_tools.benchmarks.run_capture", side_effect=fake_runner),
                patch.object(HostBackend, "allocated_size", return_value=12),
                patch(
                    "nvx_tools.benchmarks.helper_server",
                    return_value=contextlib.nullcontext("192.0.2.10"),
                ),
                contextlib.redirect_stdout(output),
            ):
                benchmark_hcs_snapshot_shell(
                    SnapshotConfig(
                        kernel,
                        base_initrd,
                        root / "build" / "hcs-shell",
                        mem=256,
                        runs=2,
                    ),
                    backend,
                    [256],
                )
                benchmark_hcs_snapshot_python(
                    SnapshotConfig(
                        kernel,
                        python_initrd,
                        root / "build" / "hcs-python",
                        mem=512,
                        runs=1,
                    ),
                    backend,
                )
                benchmark_hcs_network_snapshot_python(
                    HcsNetworkSnapshotConfig(
                        kernel,
                        python_initrd,
                        root / "build" / "hcs-network",
                        mem=512,
                        runs=1,
                        net="10.0.0.2/24",
                        port=8099,
                        endpoint_config=endpoint_config,
                    ),
                    backend,
                )

            captures = [command for command in calls if "--snapshot" in command]
            restores = [command for command in calls if "--restore" in command]
            preflights = [command for command in calls if "--selftest" in command]
            self.assertEqual(len(captures), 3)
            self.assertEqual(len(restores), 4)
            self.assertEqual(len(preflights), 3)
            network_capture = next(
                command
                for command in captures
                if command[command.index("--snapshot") + 1].endswith("hcs-network")
            )
            network_restore = next(
                command
                for command in restores
                if command[command.index("--restore") + 1].endswith("hcs-network")
            )
            network_cold = next(
                command
                for command in calls
                if "--net" in command and "--exit-on-boot" in command
            )
            self.assertIn("--net", network_capture)
            self.assertNotIn("--net", network_restore)
            for command in (network_cold, network_capture, network_restore):
                self.assertEqual(
                    command[command.index("--hcn-endpoint-config") + 1],
                    str(endpoint_config),
                )
            self.assertNotIn("--quiet", network_cold)
            self.assertNotIn("--quiet", network_capture)
            self.assertNotIn("--quiet", network_restore)
            for command in (network_cold, network_capture):
                cmdline = command[command.index("--cmdline") + 1]
                self.assertIn("netbench_host=192.0.2.10", cmdline)
                self.assertIn("netbench_hold=1", cmdline)
            self.assertEqual(
                network_cold[network_cold.index("--boot-marker") + 1],
                "NVX-HCS-NETWORK-DONE",
            )
            self.assertEqual(timeouts[network_cold], 300)
            self.assertEqual(timeouts[network_capture], 300)
            self.assertEqual(timeouts[network_restore], 300)
            self.assertIs(graceful_timeouts[network_cold], True)
            self.assertIs(graceful_timeouts[network_capture], True)
            self.assertIs(graceful_timeouts[network_restore], True)
            self.assertIn("cold guest latency", output.getvalue())
            self.assertIn("restore process wall", output.getvalue())
            self.assertIn("HELLOPY-NET OK", output.getvalue())

    def test_guest_network_mac_match_ignores_hcn_separators(self) -> None:
        for name in ("init", "init.python"):
            script = (REPO_ROOT / "alpine" / name).read_text(encoding="utf-8")
            self.assertIn("tr -d ':' | tr -d '-'", script)
            self.assertIn('fatal "virtnet: no non-loopback interface appeared', script)
            self.assertIn('while [ "$tries" -lt 600 ]', script)

    def test_hcs_network_guest_waits_for_host_teardown(self) -> None:
        script = (REPO_ROOT / "alpine" / "net-hello.py").read_text(encoding="utf-8")
        self.assertIn('"netbench_hold=1"', script)
        self.assertIn("restore_hcs_network()", script)
        self.assertIn("HELLOPY-NET RECONFIGURE-", script)
        self.assertIn("signal.pause()", script)


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
        windows = self.job("windows", "windows-hcs")

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

    def test_privileged_lanes_run_backend_specific_benchmarks(self) -> None:
        hcs = self.job("windows-hcs", "windows-hcn-afxdp")
        hcn_afxdp = self.job("windows-hcn-afxdp", "performance-gate")

        for job in (hcs, hcn_afxdp):
            self.assertIn("needs: [artifacts, windows]", job)
            self.assertIn("--require-shared-suite", job)
        self.assertIn("bench-hcs-coldstart --runs 5", hcs)
        self.assertIn("bench-hcs-virtfs --runs 3", hcs)
        self.assertIn("bench-hcs-snapshot-shell --runs 5", hcs)
        self.assertIn('--memories "64 128 256 512"', hcs)
        self.assertIn("bench-hcs-snapshot-py --runs 5", hcs)
        self.assertIn("bench-hcs-net-snapshot-py", hcs)
        self.assertIn("--require-network", hcs)
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
                self.job("windows", "windows-hcs"),
            ),
            "windows-hcs": (
                "Windows / HCS",
                self.job("windows-hcs", "windows-hcn-afxdp"),
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
            "needs: [linux, windows, windows-hcs, windows-hcn-afxdp]",
            persistence,
        )
        self.assertIn("--platform windows-hcs", persistence)
        self.assertIn("--platform windows-hcn-afxdp", persistence)


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
