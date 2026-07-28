from __future__ import annotations

import contextlib
import io
import json
import os
import sys
import tempfile
import threading
import time
import unittest
import uuid
from pathlib import Path
from unittest.mock import call, patch


SCRIPTS_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_DIR))

from nvx_tools.backends.linux import LinuxBackend
from nvx_tools.backends.base import HostBackend
from nvx_tools.backends.windows import WindowsBackend
from nvx_tools.build import (
    DockerBuildConfig,
    _install,
    build_docker_artifacts,
    docker_build_command,
)
from nvx_tools.benchmarks import (
    NetworkSnapshotConfig,
    benchmark_network_snapshot,
    format_rate_median,
    network_gateway,
    parse_dd_rate,
)
from nvx_tools.common import CommandResult, REPO_ROOT, ScriptError, remove_tree
from nvx_tools.ci import _require_sha256
from nvx_tools.cli import build_parser, main as cli_main
from nvx_tools.hcn_afxdp import (
    AfxdpConfig,
    AfxdpVmResult,
    WindowsNamedPipeServer,
    benchmark_hcn_afxdp_snapshot,
    build_manifest,
    test_hcn_afxdp,
    validate_ready,
)
from nvx_tools.smoke import (
    ExecTestConfig,
    ProfilingTestConfig,
    _folded_sample_count,
    test_exec,
    test_profiling,
)
from nvx_tools.vm import (
    BOOT_MARKER,
    BootTestConfig,
    SnapshotConfig,
    VmConfig,
    cold_boot_args,
    format_median,
    invoke_vm_metric,
    boot_test,
    run_vm,
)
from nvx_tools.windows_hcn import (
    HcnEndpointConfig,
    create_hcn_endpoint,
    validate_hcn_topology,
)


class BackendTests(unittest.TestCase):
    @unittest.skipUnless(os.name == "nt", "Windows named pipes are required")
    def test_windows_control_pipe_round_trip(self) -> None:
        pipe_path = rf"\\.\pipe\nvx-python-test-{uuid.uuid4().hex}"
        server = WindowsNamedPipeServer(pipe_path)
        observed: list[dict[str, str]] = []

        def client() -> None:
            with open(pipe_path, "r+b", buffering=0) as stream:
                stream.write(
                    b'{"type":"DataPlaneReady","queues":[0],"interfaceLuid":11}\n'
                )
                observed.append(json.loads(stream.readline()))

        thread = threading.Thread(target=client)
        thread.start()
        try:
            deadline = time.monotonic() + 5
            server.connect(deadline)
            ready = json.loads(server.read_line(deadline))
            server.write_line({"type": "StartVm"}, deadline)
            thread.join(5)
        finally:
            server.close()

        self.assertFalse(thread.is_alive())
        self.assertEqual(ready["type"], "DataPlaneReady")
        self.assertEqual(observed, [{"type": "StartVm"}])

    @unittest.skipUnless(os.name == "nt", "Windows named pipes are required")
    def test_windows_control_pipe_timeout_drains_cancellation(self) -> None:
        pipe_path = rf"\\.\pipe\nvx-python-timeout-{uuid.uuid4().hex}"
        server = WindowsNamedPipeServer(pipe_path)
        try:
            with self.assertRaises(TimeoutError):
                server.connect(time.monotonic() + 0.02)
        finally:
            server.close()

    def test_hcn_afxdp_manifest_preserves_network_contract(self) -> None:
        config = AfxdpConfig(
            Path("microvm.exe"),
            Path("vmlinux"),
            Path("initrd"),
            Path("endpoint.json"),
        )
        manifest = build_manifest(
            {
                "interfaceIndex": 7,
                "interfaceLuid": 11,
                "gatewayMac": "00-15-5D-00-00-01",
                "macAddress": "00-15-5D-52-C1-02",
            },
            config,
            r"\\.\pipe\nvx-test",
        )

        self.assertEqual(manifest["version"], 2)
        self.assertEqual(
            manifest["attachment"]["backend"], "hcn-afxdp-l2bridge"
        )
        self.assertEqual(
            manifest["attachment"]["queueSelection"], {"mode": "auto"}
        )
        self.assertEqual(
            manifest["guestBootstrap"]["ipv4"]["gateway"], "192.168.240.1"
        )
        self.assertEqual(manifest["runtime"]["controlPipe"], r"\\.\pipe\nvx-test")

    def test_hcn_afxdp_ready_requires_queue_zero(self) -> None:
        validate_ready(
            {"type": "DataPlaneReady", "interfaceLuid": 11, "queues": [0, 1]}
        )
        with self.assertRaisesRegex(ScriptError, "invalid DataPlaneReady"):
            validate_ready(
                {"type": "DataPlaneReady", "interfaceLuid": 11, "queues": [1]}
            )

    def test_migrated_hcn_ranges_reject_invalid_values(self) -> None:
        invalid = (
            ["setup-hcn-endpoint", "--output", "endpoint.json", "--prefix-length", "0"],
            ["test-hcn-afxdp", "--endpoint-config", "endpoint.json", "--mtu", "1"],
            ["test-hcn-afxdp", "--endpoint-config", "endpoint.json", "--web-port", "0"],
            ["bench-hcn-afxdp-snapshot", "--endpoint-config", "endpoint.json", "--runs", "0"],
        )
        for arguments in invalid:
            with self.subTest(arguments=arguments), contextlib.redirect_stderr(
                io.StringIO()
            ), self.assertRaises(SystemExit):
                build_parser().parse_args(arguments)

    def test_exec_timeout_preserves_kvm_environment_name(self) -> None:
        with patch.dict(
            os.environ,
            {"NVX_KVM_EXEC_TIMEOUT_SECONDS": "77"},
            clear=False,
        ):
            os.environ.pop("NVX_EXEC_TIMEOUT_SECONDS", None)
            args = build_parser().parse_args(["test-exec"])

        self.assertEqual(args.timeout_sec, 77)

    @patch("nvx_tools.cli.select_backend", return_value=LinuxBackend())
    @patch("nvx_tools.cli.test_exec")
    def test_exec_preserves_microvm_environment_override(
        self, execute: object, select: object
    ) -> None:
        with patch.dict(
            os.environ,
            {
                "MICROVM": "target/wsl/release/microvm",
                "KERNEL": "build/vmlinux",
                "INITRD": "build/initramfs.cpio.gz",
            },
        ):
            result = cli_main(["test-exec"])

        self.assertEqual(result, 0)
        config = execute.call_args.args[0]
        self.assertEqual(config.microvm, Path("target/wsl/release/microvm"))

    def test_hcn_topology_requires_first_usable_gateway(self) -> None:
        config = HcnEndpointConfig(gateway="192.168.240.7")

        with self.assertRaisesRegex(ScriptError, "first usable address"):
            validate_hcn_topology(config)

    def test_hcn_endpoint_descriptor_preserves_external_contract(self) -> None:
        class FakeHcnApi:
            def __init__(self) -> None:
                self.deleted: list[str] = []

            def enumerate_networks(self) -> list[str]:
                return []

            def create_network(self, identifier: object, document: object) -> object:
                return object()

            def create_endpoint(
                self, network: object, identifier: object, document: object
            ) -> object:
                return object()

            def endpoint_properties(
                self, handle: object, identifier: object
            ) -> dict[str, str]:
                return {"MacAddress": "00-15-5D-52-C1-02"}

            def host_default_namespace(self) -> object:
                return uuid.UUID("12345678-1234-1234-1234-123456789abc")

            def modify_namespace(self, *args: object, **kwargs: object) -> None:
                return None

            def close_endpoint(self, handle: object) -> None:
                return None

            def close_network(self, handle: object) -> None:
                return None

            def delete_endpoint(self, identifier: object) -> None:
                self.deleted.append("endpoint")

            def delete_network(self, identifier: object) -> None:
                self.deleted.append("network")

        api = FakeHcnApi()
        descriptor = create_hcn_endpoint(
            HcnEndpointConfig(attach_to_host=True),
            api=api,
            interface_resolver=lambda *args: {
                "interfaceIndex": 7,
                "interfaceLuid": 11,
                "interfaceAlias": "vEthernet (nvx-test)",
                "gatewayMac": "00-15-5D-00-00-01",
            },
        )

        self.assertEqual(descriptor["version"], 1)
        self.assertEqual(descriptor["networkAddress"], "192.168.240.0")
        self.assertTrue(descriptor["hostAttached"])
        self.assertEqual(descriptor["interfaceIndex"], 7)
        self.assertEqual(descriptor["interfaceLuid"], 11)
        self.assertEqual(api.deleted, [])

    def test_hcn_attach_failure_rolls_back_namespace(self) -> None:
        class FailingAttachApi:
            def __init__(self) -> None:
                self.requests: list[str] = []

            def enumerate_networks(self) -> list[str]:
                return []

            def create_network(self, identifier: object, document: object) -> object:
                return object()

            def create_endpoint(
                self, network: object, identifier: object, document: object
            ) -> object:
                return object()

            def endpoint_properties(
                self, handle: object, identifier: object
            ) -> dict[str, str]:
                return {"MacAddress": "00-15-5D-52-C1-02"}

            def host_default_namespace(self) -> uuid.UUID:
                return uuid.UUID("12345678-1234-1234-1234-123456789abc")

            def modify_namespace(
                self,
                namespace: object,
                endpoint: object,
                request: str,
                **kwargs: object,
            ) -> None:
                self.requests.append(request)
                if request == "Add":
                    raise RuntimeError("namespace close failed after attachment")

            def close_endpoint(self, handle: object) -> None:
                return None

            def close_network(self, handle: object) -> None:
                return None

            def delete_endpoint(self, identifier: object) -> None:
                return None

            def delete_network(self, identifier: object) -> None:
                return None

        api = FailingAttachApi()
        with self.assertRaisesRegex(ScriptError, "namespace close failed"):
            create_hcn_endpoint(
                HcnEndpointConfig(attach_to_host=True),
                api=api,
            )

        self.assertEqual(api.requests, ["Add", "Remove"])

    def test_cache_archive_hash_validation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "archive.zip"
            archive.write_bytes(b"archive")
            expected = "0eb3e36bfb24dcd9bb1d1bece1531216b59539a8fde17ee80224af0653c92aa3"

            _require_sha256(archive, expected)
            with self.assertRaisesRegex(ScriptError, "SHA-256 mismatch"):
                _require_sha256(archive, "0" * 64)

    def test_folded_sample_parser_sums_positive_samples(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            folded = Path(temporary) / "smoke.folded"
            folded.write_text("idle;foo 2\ninvalid x\nmain;bar 3\nzero 0\n")

            self.assertEqual(_folded_sample_count(folded), 5)

    @patch("nvx_tools.smoke.require_file")
    @patch("nvx_tools.smoke._perf_record_is_usable", return_value=False)
    def test_profiling_smoke_uses_shared_process_runner(
        self, perf_usable: object, require: object
    ) -> None:
        commands: list[list[str | Path]] = []

        def runner(args: list[str | Path], **kwargs: object) -> CommandResult:
            commands.append(args)
            folded = Path(args[args.index("--guest-profile") + 1])
            folded.write_text("guest;stack 7\n", encoding="utf-8")
            return CommandResult((), 0, BOOT_MARKER.encode(), b"")

        with contextlib.redirect_stdout(io.StringIO()):
            test_profiling(
                ProfilingTestConfig(
                    Path("microvm"), Path("kernel"), Path("initrd")
                ),
                runner=runner,
            )

        self.assertEqual(len(commands), 1)
        self.assertIn("--profile-hz", commands[0])
        self.assertNotIn("--host-profile", commands[0])

    @patch("nvx_tools.smoke.require_file")
    @patch("nvx_tools.smoke._build_kvm_ap_helper")
    def test_exec_uses_shared_cases_and_kvm_vcpus(
        self, build_helper: object, require: object
    ) -> None:
        results = [
            CommandResult((), 0, b"NVX-EXEC-SMOKE-0", b""),
            CommandResult((), 37, b"NVX-EXEC-SMOKE-37", b""),
            CommandResult((), 0, b"NVX-LEGACY-SHUTDOWN", b""),
            CommandResult((), 127, b"executable script not found", b""),
        ]
        runner = unittest.mock.Mock(side_effect=results)

        with contextlib.redirect_stdout(io.StringIO()):
            test_exec(
                ExecTestConfig(Path("microvm"), Path("kernel"), Path("initrd")),
                LinuxBackend(),
                runner=runner,
            )

        self.assertEqual(runner.call_count, 4)
        for invocation in runner.call_args_list:
            command = invocation.args[0]
            self.assertEqual(command[command.index("--vcpus") + 1], "2")
        build_helper.assert_called_once()

    @patch("nvx_tools.smoke.require_file")
    def test_exec_adds_whp_snapshot_and_host_error_cases(self, require: object) -> None:
        results = [
            CommandResult((), 0, b"NVX-EXEC-SMOKE-0", b""),
            CommandResult((), 37, b"NVX-EXEC-SMOKE-37", b""),
            CommandResult((), 0, b"NVX-LEGACY-SHUTDOWN", b""),
            CommandResult((), 127, b"executable script not found", b""),
            CommandResult((), 0, b"", b""),
            CommandResult((), 41, b"NVX-WHP-SNAPSHOT-RESTORED", b""),
            CommandResult((), 1, b"", b"NVX-HOST-ERROR:"),
            CommandResult((), 1, b"", b"NVX-HOST-ERROR:"),
        ]
        runner = unittest.mock.Mock(side_effect=results)

        with contextlib.redirect_stdout(io.StringIO()):
            test_exec(
                ExecTestConfig(Path("microvm.exe"), Path("kernel"), Path("initrd")),
                WindowsBackend(),
                runner=runner,
            )

        self.assertEqual(runner.call_count, 8)
        self.assertNotIn("--vcpus", runner.call_args_list[0].args[0])
        capture = runner.call_args_list[4].args[0]
        mount = Path(capture[capture.index("--mount") + 1])
        snapshot = Path(capture[capture.index("--snapshot") + 1])
        self.assertEqual(mount.name, "mount")
        self.assertEqual(snapshot.parent, mount.parent)
        self.assertFalse(snapshot.is_relative_to(mount))

    def test_guest_script_install_normalizes_windows_line_endings(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "source"
            destination = Path(directory) / "destination"
            source.write_bytes(b"#!/bin/sh\r\necho ready\r\n")

            _install(source, destination)

            self.assertEqual(destination.read_bytes(), b"#!/bin/sh\necho ready\n")
            if os.name != "nt":
                self.assertTrue(destination.stat().st_mode & 0o111)

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

    def test_network_snapshot_gateway_is_first_usable_address(self) -> None:
        self.assertEqual(network_gateway("10.42.7.23/20"), "10.42.0.1")

    def test_network_snapshot_rejects_non_ipv4_network(self) -> None:
        with self.assertRaisesRegex(ScriptError, "invalid --net IPv4 CIDR"):
            network_gateway("fd00::2/64")

    def test_run_parser_collects_mutually_exclusive_egress_rules(self) -> None:
        args = build_parser().parse_args(
            ["run", "--allow-host", "10.0.0.0/8", "--allow-host", "192.0.2.10"]
        )
        self.assertEqual(args.allow_host, ["10.0.0.0/8", "192.0.2.10"])
        self.assertEqual(args.block_host, [])

        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            build_parser().parse_args(
                ["run", "--allow-host", "10.0.0.0/8", "--block-host", "192.0.2.10"]
            )

    @patch("nvx_tools.vm.require_file")
    @patch("nvx_tools.vm.subprocess.call", return_value=0)
    def test_run_forwards_egress_rules(self, invoke: object, require: object) -> None:
        result = run_vm(
            VmConfig(Path("kernel"), Path("initrd")),
            LinuxBackend(),
            net="10.0.0.2/24",
            allow_hosts=("10.0.0.0/8", "192.0.2.10"),
        )

        self.assertEqual(result, 0)
        command = invoke.call_args.args[0]
        self.assertEqual(command.count("--allow-host"), 2)
        self.assertEqual(
            [
                command[index + 1]
                for index, value in enumerate(command)
                if value == "--allow-host"
            ],
            ["10.0.0.0/8", "192.0.2.10"],
        )

    def test_network_snapshot_times_one_gateway_probe_on_cold_and_restore(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            snapshot = Path(temporary) / "snapshot"
            snapshot.mkdir()
            (snapshot / "state.bin").write_bytes(b"state")
            (snapshot / "mem.bin").write_bytes(b"memory")
            timed_results = [(12.0, 20.0, object()), (3.0, 10.0, object())]
            with (
                patch(
                    "nvx_tools.benchmarks.require_vm_inputs",
                    return_value=Path("microvm.exe"),
                ),
                patch(
                    "nvx_tools.benchmarks._run_timed",
                    side_effect=timed_results,
                ) as run_timed,
                patch("nvx_tools.benchmarks.capture_snapshot") as capture,
                patch.object(WindowsBackend, "allocated_size", return_value=6),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                benchmark_network_snapshot(
                    NetworkSnapshotConfig(
                        Path("kernel"),
                        Path("initrd"),
                        snapshot,
                        runs=1,
                        net="10.42.7.23/20",
                    ),
                    WindowsBackend(),
                )

        cold_args = run_timed.call_args_list[0].args[0]
        capture_args = capture.call_args.args[0]
        restore_args = run_timed.call_args_list[1].args[0]
        probe_cmdline = (
            "earlycon=xe9 console=hvc0 quiet loglevel=0 reboot=t panic=-1 "
            "virtnet_probe=10.42.0.1"
        )
        self.assertEqual(cold_args[cold_args.index("--cmdline") + 1], probe_cmdline)
        self.assertEqual(
            cold_args[cold_args.index("--boot-marker") + 1],
            "VIRTNET-PROBE-OK: 10.42.0.1",
        )
        self.assertEqual(
            capture_args[capture_args.index("--cmdline") + 1],
            f"{probe_cmdline} netsnap",
        )
        self.assertEqual(
            restore_args[restore_args.index("--boot-marker") + 1],
            "NETSNAP-RESTORE-PROBE-OK: 10.42.0.1",
        )

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

    def test_run_rejects_removed_virtfs_image_options(self) -> None:
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            build_parser().parse_args(["run", "--mount-image", "host.img"])

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
    def test_profiling_kernel_uses_distinct_default_output(self) -> None:
        with (
            patch("nvx_tools.cli.select_backend", return_value=LinuxBackend()),
            patch("nvx_tools.cli.build_kernel") as build_kernel,
        ):
            result = cli_main(["build-kernel", "--profiling"])

        self.assertEqual(result, 0)
        config = build_kernel.call_args.args[0]
        self.assertTrue(config.profiling)
        self.assertEqual(config.output.name, "vmlinux-profiling")

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
    def test_profiling_docker_build_selects_distinct_target(
        self, run: object, require: object
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary)
            (destination / "vmlinux-profiling").write_bytes(b"kernel")
            (destination / "initramfs.cpio.gz").write_bytes(b"initrd")

            build_docker_artifacts(
                DockerBuildConfig(
                    destination,
                    "6.18.99",
                    "3.24.9",
                    "v3.24",
                    profiling=True,
                ),
                python_only=False,
            )

        command = [str(value) for value in run.call_args.args[0]]
        self.assertEqual(
            command[command.index("--target") + 1], "artifacts-profiling"
        )
        self.assertIn("KVER=6.18.99", command)

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

    def test_rate_output_keeps_performance_parser_contract(self) -> None:
        formatted = format_rate_median([100.0, 300.0, 200.0])
        self.assertRegex(formatted, r"200\.0 MB/s")


class RetainedWorkflowTests(unittest.TestCase):
    def test_hcn_afxdp_smoke_runs_a_mounted_exec_workload(self) -> None:
        observed: dict[str, object] = {}

        class FakeHttpServer:
            logs: list[str] = []

            def __init__(self, port: int) -> None:
                observed["port"] = port

            def __enter__(self) -> "FakeHttpServer":
                return self

            def __exit__(self, *args: object) -> None:
                return None

        def invoke(
            config: AfxdpConfig,
            endpoint: object,
            arguments: list[str | Path],
            **kwargs: object,
        ) -> AfxdpVmResult:
            observed["arguments"] = arguments
            mount = Path(arguments[arguments.index("--mount") + 1])
            observed["workload"] = (mount / "hcn-afxdp-smoke.sh").read_text()
            return AfxdpVmResult(
                None,
                12.0,
                "NVX-HCN-AFXDP-SMOKE-OK\n",
                "",
                {"type": "DataPlaneReady", "interfaceLuid": 11, "queues": [0]},
            )

        with tempfile.TemporaryDirectory() as temporary, patch(
            "nvx_tools.hcn_afxdp.HostHttpServer", FakeHttpServer
        ), contextlib.redirect_stdout(io.StringIO()):
            test_hcn_afxdp(
                AfxdpConfig(
                    Path("microvm.exe"),
                    Path("kernel"),
                    Path("initrd"),
                    Path("endpoint.json"),
                ),
                log_path=Path(temporary) / "smoke.log",
                endpoint={"interfaceIndex": 7},
                invoker=invoke,
            )

        arguments = observed["arguments"]
        self.assertIn("--mount", arguments)
        self.assertIn("/mnt/host/hcn-afxdp-smoke.sh", arguments)
        self.assertIn("exit 42", observed["workload"])
        self.assertEqual(observed["port"], 8099)

    def test_hcn_afxdp_snapshot_runs_whp_selftest_and_visible_guests(self) -> None:
        invocations: list[tuple[list[str | Path], dict[str, object]]] = []

        def invoke(
            config: AfxdpConfig,
            endpoint: object,
            arguments: list[str | Path],
            **kwargs: object,
        ) -> AfxdpVmResult:
            invocations.append((arguments, kwargs))
            if "--snapshot" in arguments:
                snapshot = Path(arguments[arguments.index("--snapshot") + 1])
                snapshot.mkdir()
                (snapshot / "state.bin").write_bytes(b"state")
                (snapshot / "mem.bin").write_bytes(b"memory")
            return AfxdpVmResult(
                3.0 if kwargs.get("require_metric") else None,
                5.0,
                str(kwargs.get("required_marker", "")),
                "",
                {"type": "DataPlaneReady", "interfaceLuid": 11, "queues": [0]},
            )

        with tempfile.TemporaryDirectory() as temporary, contextlib.redirect_stdout(
            io.StringIO()
        ):
            benchmark_hcn_afxdp_snapshot(
                AfxdpConfig(
                    Path("microvm.exe"),
                    Path("kernel"),
                    Path("initrd"),
                    Path("endpoint.json"),
                ),
                Path(temporary) / "snapshot",
                runs=1,
                endpoint={"interfaceIndex": 7},
                invoker=invoke,
            )

        self.assertEqual(len(invocations), 3)
        cold, capture, restore = invocations
        self.assertNotIn("--quiet", cold[0])
        self.assertEqual(
            cold[0][cold[0].index("--boot-marker") + 1],
            "VIRTNET-PROBE-OK: 192.168.240.1",
        )
        self.assertIn("netsnap: pre-snapshot link OK", capture[1]["required_marker"])
        self.assertEqual(
            restore[0][restore[0].index("--boot-marker") + 1],
            "NETSNAP-RESTORE-PROBE-OK: 192.168.240.1",
        )

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

    def test_network_snapshot_uses_one_echo_for_cold_and_restore(self) -> None:
        script = (REPO_ROOT / "alpine" / "init").read_text(encoding="utf-8")

        self.assertEqual(script.count('ping -c 1 -W 1 "$nprobe"'), 2)
        self.assertNotIn('ping -c 3 -W 2 "$nprobe"', script)

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
        self.assertIn("scripts/nvx.py test-exec", linux)
        self.assertIn('if [ "$RUN_BENCH" = true ]', linux)
        self.assertIn("--initrd build/initramfs-python.cpio.gz --mem 512", linux)
        self.assertIn("scripts\\nvx.py test-exec", windows)
        self.assertIn("bidirectional visibility verified\\s+: 3/3", windows)
        self.assertNotIn("payload survived across runs", windows)
        self.assertIn("if ($env:RUN_BENCH -eq 'true')", windows)
        self.assertIn("--initrd build\\initramfs-python.cpio.gz", windows)
        self.assertIn("--mem 512", windows)

        self.assertNotIn("skipping networking benchmark", linux)
        self.assertNotIn("skipping networked Python smoke test", linux)

    def test_privileged_lane_runs_hcn_afxdp_benchmarks(self) -> None:
        hcn_afxdp = self.job("windows-hcn-afxdp", "performance-gate")

        self.assertIn("needs: [artifacts, windows]", hcn_afxdp)
        self.assertIn("github.event_name == 'pull_request'", hcn_afxdp)
        self.assertIn("--require-shared-suite", hcn_afxdp)
        exec_smoke = hcn_afxdp.split(
            "- name: Smoke — external HCN vNIC + AF_XDP\n", 1
        )[1].split("\n      - name:", 1)[0]
        self.assertIn("scripts\\nvx.py test-hcn-afxdp", exec_smoke)
        self.assertNotIn("if:", exec_smoke)
        for command in (
            "measure-coldstart --runs 5",
            "bench-virtfs --runs 3",
            "snapshot-demo --runs 5",
            "bench-snapshot-shell --runs 5",
            "bench-hcn-afxdp-snapshot",
        ):
            self.assertIn(command, hcn_afxdp)
        self.assertIn("--runs 5", hcn_afxdp)
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

        self.assertIn("PERFORMANCE_REGRESSION_ABSOLUTE_TOLERANCE_MS: 10", self.workflow)
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
