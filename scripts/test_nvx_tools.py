#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import argparse
import hashlib
import http.client
import http.server
import io
import json
import os
import queue
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import unittest
import urllib.error
import urllib.request
import zipfile
from pathlib import Path
from typing import cast
from unittest.mock import MagicMock, call, patch

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
    sandbox_lifecycle,
)


def _write_release_fixture(
    root: Path,
) -> tuple[dict[str, Path], dict[str, object], str]:
    build_dir = root / "build"
    source_dir = build_dir / "sources"
    openvmm_dir = root / "openvmm"
    binary_name = "openvmm.exe" if os.name == "nt" else "openvmm"
    binary = openvmm_dir / "target" / "release" / binary_name
    revision = "fc0e30ee394fa2f6a251f527825492637ef5ac4a"
    guest_names = (
        "vmlinux",
        "vmlinux.config",
        "initramfs.cpio.gz",
        "initramfs.cpio.gz.packages.json",
    )
    for path in (
        binary,
        *(build_dir / name for name in guest_names),
        root / "LICENSE",
        root / "README.md",
        root / "THIRD_PARTY_NOTICES.md",
        openvmm_dir / "LICENSE",
        root / "kernel" / "COPYING-LINUX",
    ):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(path.name.encode("ascii"))
    generated_config = build_dir / "vmlinux.config"
    generated_config.write_text(
        "\n".join(
            (
                *build.REQUIRED_VIRTIO_CONSOLE_CONFIG,
                *build.REQUIRED_SHARED_STATUS_KERNEL_CONFIG,
                *build.REQUIRED_SANDBOX_KERNEL_CONFIG,
            )
        )
        + "\n",
        encoding="utf-8",
    )
    manifest = {
        "format": 1,
        "distribution": {"name": "nvx", "version": "0.1.0"},
        "openvmm": {
            "microvm_abi_version": 2,
            "control_session_protocol_version": 1,
            "control_contract_revision": "nvx-microvm-v2-control-v1",
        },
        "linux": {
            "version": "6.18.38",
            "upstream_url": (
                "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.18.38.tar.xz"
            ),
            "upstream_archive_sha256": build.DEFAULT_KERNEL_SHA256,
            "source_cache": ".cache/linux/linux-6.18.38",
            "source_archive": ("build/sources/linux/nvx-linux-source-6.18.38.tar.gz"),
            "generated_final_config": "build/vmlinux.config",
            "input_config": "kernel/config-microvm",
            "patches": ["kernel/patches/example.patch"],
        },
        "alpine": {
            "version": "3.24.1",
            "branch": "v3.24",
            "architecture": "x86_64",
            "minirootfs_url": (
                "https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/"
                "x86_64/alpine-minirootfs-3.24.1-x86_64.tar.gz"
            ),
            "minirootfs_sha256": build.DEFAULT_ALPINE_MINIROOTFS_SHA256,
            "guest_sources": "alpine",
            "package_manifests": "build/*.packages.json",
            "source_output": "build/sources/alpine",
        },
    }
    (root / "SOURCE-MANIFEST.json").write_text(
        json.dumps(manifest),
        encoding="utf-8",
    )
    kernel_inputs: dict[str, object] = {
        "source": {
            "version": "6.18.38",
            "patches": [
                {
                    "path": "kernel/patches/example.patch",
                    "sha256": "2" * 64,
                }
            ],
        },
        "input_config": {
            "path": "kernel/config-microvm",
            "sha256": "1" * 64,
        },
    }
    (build_dir / build.OPENVMM_PROVENANCE_NAME).write_text(
        json.dumps(
            {
                "format": 1,
                "source_revision": revision,
                "source_clean": True,
                "executable_sha256": common.sha256_file(binary),
            }
        ),
        encoding="utf-8",
    )
    (build_dir / build.KERNEL_PROVENANCE_NAME).write_text(
        json.dumps(
            {
                "format": 1,
                **kernel_inputs,
                "kernel_sha256": common.sha256_file(build_dir / "vmlinux"),
                "config_sha256": common.sha256_file(generated_config),
            }
        ),
        encoding="utf-8",
    )
    paths = {
        "build": build_dir,
        "source": source_dir,
        "openvmm": openvmm_dir,
        "binary": binary,
    }
    return paths, kernel_inputs, revision


class CliTests(unittest.TestCase):
    def test_benchmark_exposes_device_restore_profile(self):
        args = nvx.parse_args(
            [
                "benchmark",
                "--suite",
                "device-restore-profile",
                "--restore-devices",
                "console",
                "virtiofs",
                "--restore-modes",
                "deferred",
                "--output-dir",
                "results",
            ]
        )

        benchmark.apply_benchmark_suite_defaults(args)
        self.assertEqual(args.restore_devices, ["console", "virtiofs"])
        self.assertEqual(args.restore_modes, ["deferred"])
        self.assertEqual(args.warmups, 1)
        self.assertEqual(args.runs, 5)
        self.assertEqual(args.output_dir, Path("results"))
        self.assertIs(args.handler, benchmark.run)

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
        self.assertEqual(args.host_cpu_reserve, 2)
        self.assertEqual(args.output_dir, Path("results"))
        self.assertIs(args.handler, benchmark.run)

    def test_device_io_uses_canonical_sampling_defaults(self):
        args = nvx.parse_args(["benchmark", "--suite", "device-io"])

        benchmark.apply_benchmark_suite_defaults(args)

        self.assertEqual(args.warmups, 5)
        self.assertEqual(args.runs, 30)
        self.assertEqual(args.device_io_duration_seconds, 10.0)
        self.assertEqual(args.device_io_size_mib, 512)
        self.assertEqual(args.device_io_port, 5201)
        self.assertFalse(hasattr(args, "device_io_abi"))

        with self.assertRaises(SystemExit):
            nvx.parse_args(
                ["benchmark", "--suite", "device-io", "--device-io-abi", "1"]
            )

    def test_benchmark_exposes_restore_only_shell_suite(self):
        args = nvx.parse_args(
            [
                "benchmark",
                "--suite",
                "shell-snapshot-restore",
                "--shell-memories",
                "512",
                "--processors",
                "8",
                "--output-dir",
                "results",
            ]
        )

        self.assertEqual(args.suite, "shell-snapshot-restore")
        self.assertEqual(args.shell_memories, [512])
        self.assertEqual(args.processors, 8)
        self.assertIs(args.handler, benchmark.run)

    def test_benchmark_exposes_restore_vcpu_matrix(self):
        args = nvx.parse_args(
            [
                "benchmark",
                "--suite",
                "snapshot-restore-vcpu",
                "--processors",
                "8",
                "--memory-mib",
                "512",
                "--output-dir",
                "results",
            ]
        )

        self.assertEqual(args.suite, "snapshot-restore-vcpu")
        self.assertEqual(args.processors, 8)
        self.assertEqual(args.memory_mib, 512)
        self.assertIs(args.handler, benchmark.run)

    def test_benchmark_exposes_snapshot_profile_matrix(self):
        args = nvx.parse_args(
            [
                "benchmark",
                "--suite",
                "snapshot-profile",
                "--backend",
                "mshv",
                "--cache-state",
                "cold",
            ]
        )

        self.assertEqual(args.suite, "snapshot-profile")
        self.assertIsNone(args.shell_memories)
        benchmark.apply_benchmark_suite_defaults(args)
        self.assertEqual(args.shell_memories, [64, 128, 256, 512, 1024])
        self.assertEqual(args.cache_state, "cold")
        self.assertFalse(args.snapshot_profile)
        self.assertIs(args.handler, benchmark.run)

    def test_benchmark_preserves_canonical_shell_memory_defaults(self):
        args = nvx.parse_args(["benchmark", "--suite", "performance"])

        benchmark.apply_benchmark_suite_defaults(args)

        self.assertEqual(args.shell_memories, [64, 128, 256, 512])

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
                "--workload-user",
                "1000:1001",
            ]
        )

        self.assertEqual(args.layer[0].role, "distro")
        self.assertEqual(args.scratch, Path("scratch.ext4"))
        self.assertEqual(args.sandbox_arg, ["--serve"])
        self.assertEqual(args.memory_max, 268435456)
        self.assertEqual(args.pids_max, 64)
        self.assertEqual(args.workload_user, (1000, 1001))
        self.assertIs(args.handler, nvx.command_sandbox)

    def test_sandbox_command_parses_state_aware_operations(self):
        provision = nvx.parse_args(
            [
                "sandbox",
                "provision",
                "--state-dir",
                "state",
                "--layer",
                "distro,distro.erofs,11111111-1111-1111-1111-111111111111",
                "--scratch",
                "scratch.ext4",
            ]
        )
        self.assertEqual(provision.sandbox_operation, "provision")
        self.assertEqual(provision.state_dir, Path("state"))

        execute = nvx.parse_args(
            [
                "sandbox",
                "exec",
                "--state-dir",
                "state",
                "--entrypoint",
                "/bin/sh",
                "--arg=-c",
                "--arg=echo managed",
                "--exec-timeout-ms",
                "5000",
            ]
        )
        self.assertEqual(execute.sandbox_operation, "exec")
        self.assertEqual(execute.sandbox_arg, ["-c", "echo managed"])
        self.assertEqual(execute.exec_timeout_ms, 5000)

        report = nvx.parse_args(
            [
                "sandbox",
                "exec",
                "--state-dir",
                "state",
                "--outcome-report",
                "exec-outcome.json",
            ]
        )
        self.assertEqual(report.outcome_report, Path("exec-outcome.json"))

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

    def test_run_parses_denied_filesystem_paths(self):
        args = nvx.parse_args(
            [
                "run",
                "--mount",
                "/mnt/share,share,rw",
                "--mount-deny",
                "share/secrets",
                "--mount-deny",
                "share/private",
            ]
        )
        self.assertEqual(
            args.mount_deny,
            [Path("share/secrets"), Path("share/private")],
        )

    def test_run_forwards_bounded_outcome_report(self):
        args = nvx.parse_args(["run", "--outcome-report", "outcome.json", "--dry-run"])
        with (
            patch.object(nvx, "require_file", return_value=Path("artifact")),
            patch.object(
                nvx, "_format_command", return_value="formatted"
            ) as format_command,
        ):
            nvx.command_run(args)

        command = format_command.call_args.args[0]
        self.assertEqual(
            command[command.index("--microvm-report") + 1],
            "outcome.json",
        )

    def test_run_exposes_restore_readiness(self):
        args = nvx.parse_args(
            [
                "run",
                "--hypervisor",
                "mshv",
                "--restore-snapshot",
                "snapshot",
                "--processors",
                "4",
                "--restore-processors",
                "2",
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
                "--processors",
                "4",
                "--hypervisor",
                "mshv",
                "--restore-snapshot",
                "snapshot",
                "--restore-entropy",
                "--restore-processors",
                "2",
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

        missing_processor_snapshot = nvx.parse_args(
            ["run", "--restore-processors", "1", "--dry-run"]
        )
        with self.assertRaisesRegex(
            common.ScriptError, "--restore-processors requires --restore-snapshot"
        ):
            nvx.command_run(missing_processor_snapshot)

        invalid_restore_capacity = nvx.parse_args(
            [
                "run",
                "--processors",
                "2",
                "--restore-snapshot",
                "snapshot",
                "--restore-processors",
                "4",
                "--dry-run",
            ]
        )
        with self.assertRaisesRegex(common.ScriptError, "cannot exceed"):
            nvx.command_run(invalid_restore_capacity)

        legacy = nvx.parse_args(["run", "--restore-snapshot", "snapshot", "--dry-run"])
        with (
            patch.object(nvx, "require_file", return_value=Path("openvmm")),
            patch.object(
                nvx, "_format_command", return_value="formatted"
            ) as format_command,
        ):
            nvx.command_run(legacy)
        self.assertIn("microvm", format_command.call_args.args[0])
        self.assertNotIn("microvm-v2", format_command.call_args.args[0])

        capture_memory = nvx.parse_args(
            [
                "run",
                "--memory-mib",
                "512",
                "--memory-capacity-mib",
                "2048",
                "--dry-run",
            ]
        )
        with (
            patch.object(nvx, "require_file", return_value=Path("artifact")),
            patch.object(
                nvx, "_format_command", return_value="formatted"
            ) as format_command,
        ):
            nvx.command_run(capture_memory)
        command = format_command.call_args.args[0]
        self.assertEqual(command[command.index("--memory") + 1], "512M")
        self.assertEqual(command[command.index("--memory-capacity") + 1], "2048M")

        restore_memory = nvx.parse_args(
            [
                "run",
                "--restore-snapshot",
                "snapshot",
                "--restore-memory-mib",
                "1024",
                "--dry-run",
            ]
        )
        with (
            patch.object(nvx, "require_file", return_value=Path("openvmm")),
            patch.object(
                nvx, "_format_command", return_value="formatted"
            ) as format_command,
        ):
            nvx.command_run(restore_memory)
        command = format_command.call_args.args[0]
        self.assertEqual(command[command.index("--restore-memory") + 1], "1024M")
        self.assertNotIn("--memory-capacity", command)

        smp = nvx.parse_args(
            ["run", "--machine", "microvm", "--processors", "2", "--dry-run"]
        )
        self.assertEqual(smp.machine, "microvm")
        self.assertEqual(smp.processors, 2)

        with self.assertRaises(SystemExit):
            nvx.parse_args(
                ["run", "--machine", "microvm-v2", "--processors", "4", "--dry-run"]
            )

        with self.assertRaises(SystemExit):
            nvx.parse_args(["run", "--machine", "microvm-v3", "--dry-run"])

    def test_openvmm_build_skips_compatibility_igvm(self):
        with (
            patch.object(nvx, "require_file"),
            patch.object(nvx, "_run") as run,
            patch.object(nvx, "record_openvmm_provenance") as provenance,
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
        provenance.assert_called_once_with(common.openvmm_binary_path())

    def test_record_openvmm_provenance_command_is_exposed(self):
        args = nvx.parse_args(["record-openvmm-provenance"])
        self.assertIs(args.handler, nvx.command_record_openvmm_provenance)


class CiTests(unittest.TestCase):
    def test_openvmm_tests_are_independent_of_nvx_guest_artifacts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            openvmm = root / "openvmm"
            openvmm.mkdir()
            (openvmm / "Cargo.toml").touch()
            backend = "whp" if os.name == "nt" else "kvm"

            with (
                patch.object(ci, "OPENVMM_DIR", openvmm),
                patch.object(ci.os, "access", return_value=True),
                patch.object(ci.Path, "exists", return_value=False),
                patch.object(ci, "require_tool", side_effect=["cargo", "rustup"]),
                patch.object(ci, "run_checked") as run_checked,
                patch.dict(
                    os.environ,
                    {
                        "OPENVMM_MICROVM_PVH_KERNEL": "nvx-kernel",
                        "OPENVMM_MICROVM_PVH_INITRD": "nvx-initrd",
                        "PETRI_CAPABILITIES": "vpci",
                        "RUNNER_TEMP": os.fspath(root),
                    },
                ),
            ):
                ci.run_openvmm_tests(backend)

            self.assertEqual(run_checked.call_count, 3)
            install_target, restore, tests = run_checked.call_args_list
            self.assertEqual(
                install_target.args[0],
                ["rustup", "target", "add", "x86_64-unknown-none"],
            )
            self.assertEqual(
                restore.args[0],
                ["cargo", "xflowey", "restore-packages", "--no-compat-igvm"],
            )
            command = tests.args[0]
            self.assertEqual(command[:3], ["cargo", "xflowey", "vmm-tests-run"])
            filter_index = command.index("--filter")
            self.assertEqual(command[filter_index + 1], ci.OPENVMM_MICROVM_TEST_FILTER)
            self.assertIn(
                "test_ttrpc_microvm_pvh_snapshot",
                ci.OPENVMM_MICROVM_TEST_FILTER,
            )
            self.assertEqual(tests.kwargs["cwd"], openvmm)
            self.assertNotIn("env", tests.kwargs)
            if os.name == "nt":
                self.assertEqual(
                    command[command.index("--dir") + 1],
                    os.fspath(root / backend),
                )

    def test_openvmm_tests_reject_unknown_backend(self):
        with self.assertRaisesRegex(common.ScriptError, "unsupported.*backend"):
            ci.run_openvmm_tests("unknown")


class CiConfigurationTests(unittest.TestCase):
    def test_flowey_downloads_use_retrying_curl(self):
        action = (
            common.REPO_ROOT / ".github" / "actions" / "setup-curl" / "action.yml"
        ).read_text(encoding="utf-8")
        windows_shim = (
            common.REPO_ROOT / ".github" / "actions" / "setup-curl" / "curl-shim.rs"
        ).read_text(encoding="utf-8")
        workflow = (common.REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )
        build_action = (
            common.REPO_ROOT / ".github" / "actions" / "build-openvmm" / "action.yml"
        ).read_text(encoding="utf-8")

        for option in (
            "--retry 5",
            "--retry-all-errors",
            "--retry-delay 2",
            "--retry-max-time 90",
        ):
            self.assertEqual(action.count(option), 1)
        for argument in (
            '"--retry"',
            '"5"',
            '"--retry-all-errors"',
            '"--retry-delay"',
            '"2"',
            '"--retry-max-time"',
            '"90"',
        ):
            self.assertIn(argument, windows_shim)
        self.assertIn('system_curl="${NVX_SYSTEM_CURL:-$(command -v curl)}"', action)
        self.assertIn("Get-Command curl.exe", action)
        self.assertIn('Join-Path $ShimDirectory "curl.exe"', action)
        self.assertIn("Get-Command rustc.exe", action)
        self.assertNotIn("curl.cmd", action)
        self.assertEqual(
            workflow.count("uses: ./.github/actions/setup-curl"),
            2,
        )
        self.assertIn("uses: ./.github/actions/setup-curl", build_action)

    @unittest.skipUnless(os.name == "nt", "Windows-specific curl resolution")
    def test_windows_curl_shim_retries_http_500(self):
        rustc = shutil.which("rustc")
        system_curl = shutil.which("curl.exe")
        if rustc is None or system_curl is None:
            self.fail("Windows curl shim test requires rustc and curl.exe")

        class RetryHandler(http.server.BaseHTTPRequestHandler):
            request_count = 0

            def do_GET(self) -> None:
                type(self).request_count += 1
                if type(self).request_count == 1:
                    self.send_error(http.HTTPStatus.INTERNAL_SERVER_ERROR)
                    return
                body = b"ok"
                self.send_response(http.HTTPStatus.OK)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, format: str, *args: object) -> None:
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), RetryHandler)
        server_thread = threading.Thread(target=server.serve_forever, daemon=True)
        server_thread.start()
        try:
            with tempfile.TemporaryDirectory() as directory:
                shim = Path(directory) / "curl.exe"
                shim_source = (
                    common.REPO_ROOT
                    / ".github"
                    / "actions"
                    / "setup-curl"
                    / "curl-shim.rs"
                )
                subprocess.run(
                    [
                        rustc,
                        "--edition=2021",
                        "-C",
                        "opt-level=s",
                        "-o",
                        os.fspath(shim),
                        os.fspath(shim_source),
                    ],
                    check=True,
                )
                environment = os.environ.copy()
                environment["NVX_SYSTEM_CURL"] = system_curl
                environment["PATH"] = directory + os.pathsep + environment["PATH"]
                port = server.server_address[1]
                result = subprocess.run(
                    [
                        "curl",
                        "--fail",
                        "-L",
                        f"http://127.0.0.1:{port}/asset",
                    ],
                    check=False,
                    capture_output=True,
                    env=environment,
                )
        finally:
            server.shutdown()
            server.server_close()
            server_thread.join(timeout=5)

        self.assertFalse(
            server_thread.is_alive(),
            "HTTP test server did not stop within 5 seconds",
        )
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        self.assertEqual(result.stdout, b"ok")
        self.assertEqual(RetryHandler.request_count, 2)

    def test_ci_shares_openvmm_inputs_and_binary_artifacts(self):
        workflow = (common.REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )
        build_action = (
            common.REPO_ROOT / ".github" / "actions" / "build-openvmm" / "action.yml"
        ).read_text(encoding="utf-8")

        for configuration in (workflow, build_action):
            self.assertIn(
                "openvmm/flowey-persist/flowey_lib_common__download_gh_release",
                configuration,
            )
            self.assertIn(
                "openvmm/flowey-persist/flowey_lib_common__cache",
                configuration,
            )
            self.assertIn(
                "openvmm-inputs-v1-${{ runner.os }}-${{ runner.arch }}-",
                configuration,
            )
        self.assertIn("openvmm-binaries:", workflow)
        self.assertIn("artifact: openvmm-linux-gnu", workflow)
        self.assertIn("artifact: openvmm-linux-musl", workflow)
        self.assertIn("artifact: openvmm-windows-msvc", workflow)
        self.assertIn("uses: actions/download-artifact@v8", workflow)
        self.assertNotIn("nvx-microvm-tests-v1", workflow)
        self.assertNotIn("cargo-v2-", build_action)
        self.assertNotIn("uses: actions/cache@v5", workflow)
        self.assertNotIn("uses: actions/cache@v5", build_action)

    def test_ci_artifact_actions_use_node24(self):
        configurations = "\n".join(
            path.read_text(encoding="utf-8")
            for pattern in ("*.yml", "*.yaml")
            for path in (common.REPO_ROOT / ".github").rglob(pattern)
        )

        self.assertNotRegex(configurations, r"actions/upload-artifact@v[1-5]\b")
        self.assertNotRegex(configurations, r"actions/download-artifact@v[1-6]\b")

    def test_windows_ci_remeasures_only_unstable_lifecycle_results(self):
        action = (
            common.REPO_ROOT / ".github" / "actions" / "run-benchmark" / "action.yml"
        ).read_text(encoding="utf-8")

        self.assertEqual(action.count("performance validate-openvmm"), 1)
        self.assertNotIn("for attempt in 1 2", action)
        self.assertEqual(action.count("foreach ($Attempt in 1, 2)"), 1)
        self.assertEqual(action.count("$ValidationStatus -ne 75"), 1)
        self.assertEqual(
            action.count("Lifecycle snapshot generation was unstable"),
            1,
        )


class BuildTests(unittest.TestCase):
    def test_records_openvmm_revision_cleanliness_and_executable_hash(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            openvmm_dir = root / "openvmm"
            executable = openvmm_dir / "target" / "release" / "openvmm"
            executable.parent.mkdir(parents=True)
            executable.write_bytes(b"openvmm")
            revision = b"fc0e30ee394fa2f6a251f527825492637ef5ac4a\n"
            results = [
                common.CommandResult(("git",), 0, revision, b""),
                common.CommandResult(("git",), 0, revision, b""),
                common.CommandResult(("git",), 0, b" M src/main.rs\n", b""),
            ]

            with (
                patch.object(build, "REPO_ROOT", root),
                patch.object(build, "OPENVMM_DIR", openvmm_dir),
                patch.object(build, "run_capture", side_effect=results),
            ):
                build.record_openvmm_provenance(executable)

            provenance = json.loads(
                (root / "build" / build.OPENVMM_PROVENANCE_NAME).read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(provenance["source_revision"], revision.decode().strip())
            self.assertFalse(provenance["source_clean"])
            self.assertEqual(
                provenance["executable_sha256"],
                hashlib.sha256(b"openvmm").hexdigest(),
            )

    def test_kernel_build_rejects_input_config_mutation_during_build(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            input_config = root / "kernel" / "config-microvm"
            input_config.parent.mkdir(parents=True)
            input_config.write_text(
                "\n".join(
                    (
                        *build.REQUIRED_VIRTIO_CONSOLE_CONFIG,
                        *build.REQUIRED_SHARED_STATUS_KERNEL_CONFIG,
                        *build.REQUIRED_SANDBOX_KERNEL_CONFIG,
                    )
                )
                + "\n",
                encoding="utf-8",
            )
            patch_path = root / "kernel" / "patches" / "example.patch"
            patch_path.parent.mkdir()
            patch_path.write_text("patch", encoding="utf-8")
            source = root / "source"
            source.mkdir()
            work = root / "work"
            output = root / "output" / "vmlinux"
            prior_provenance = output.with_name(build.KERNEL_PROVENANCE_NAME)
            prior_provenance.parent.mkdir()
            prior_provenance.write_text("stale", encoding="utf-8")

            with patch.object(build, "REPO_ROOT", root):
                source_fingerprint = build._kernel_source_fingerprint()

            def run_build(command: object, **_kwargs: object) -> None:
                if isinstance(command, list) and command[-1] == "vmlinux":
                    (work / "vmlinux").write_bytes(b"kernel")
                    input_config.write_text("CONFIG_CHANGED=y\n", encoding="utf-8")

            notes = common.CommandResult(
                ("readelf",),
                0,
                b"Xen 0x00000012",
                b"",
            )
            with (
                patch.object(build, "REPO_ROOT", root),
                patch.object(build, "_require_linux"),
                patch.object(build, "require_tool", return_value="tool"),
                patch.object(
                    build,
                    "prepare_kernel_source",
                    return_value=(source, source_fingerprint),
                ),
                patch.object(build, "run_checked", side_effect=run_build),
                patch.object(build, "run_capture", return_value=notes),
                self.assertRaisesRegex(
                    common.ScriptError,
                    "inputs changed during the build",
                ),
            ):
                build.build_kernel(
                    build.KernelBuildConfig(
                        work=work,
                        output=output,
                    )
                )

            self.assertFalse(output.exists())
            self.assertFalse(output.with_name("vmlinux.config").exists())
            self.assertFalse(prior_provenance.exists())

    def test_manifest_tracks_every_kernel_patch(self):
        manifest = json.loads(
            (build.REPO_ROOT / "SOURCE-MANIFEST.json").read_text(encoding="utf-8")
        )
        self.assertEqual(
            manifest["linux"]["patches"],
            [
                path.relative_to(build.REPO_ROOT).as_posix()
                for path in build._kernel_patch_files()
            ],
        )
        self.assertNotIn("guest_agent", manifest)
        self.assertEqual(
            manifest["openvmm"],
            {
                "microvm_abi_version": 2,
                "control_session_protocol_version": 1,
                "control_contract_revision": "nvx-microvm-v2-control-v1",
            },
        )

    def test_ci_kernel_cache_key_includes_patches(self):
        action = (
            build.REPO_ROOT
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        ).read_text(encoding="utf-8")
        for cache_input in (
            "SOURCE-MANIFEST.json",
            "kernel/config-microvm",
            "kernel/patches/**",
            "scripts/nvx_tools/build.py",
        ):
            self.assertIn(cache_input, action)
        self.assertIn("linux-kernel-v2-", action)
        self.assertEqual(action.count("build/vmlinux.provenance.json"), 2)

    def test_apk_add_uses_host_ca_bundle_without_overriding_configuration(self):
        root = Path("root")
        with (
            patch.object(build.ssl, "get_default_verify_paths") as verify_paths,
            patch.object(build, "run_checked") as run,
        ):
            verify_paths.return_value.cafile = "/etc/host-ca.pem"
            with patch.dict(os.environ, {}, clear=True):
                build._apk_add(root, "example")

            environment = run.call_args.kwargs["env"]
            self.assertEqual(environment["SSL_CERT_FILE"], "/etc/host-ca.pem")
            self.assertEqual(
                environment["LD_LIBRARY_PATH"],
                f"{root / 'lib'}:{root / 'usr' / 'lib'}",
            )

            with patch.dict(
                os.environ,
                {"SSL_CERT_FILE": "/etc/configured-ca.pem"},
                clear=True,
            ):
                build._apk_add(root, "example")

            environment = run.call_args.kwargs["env"]
            self.assertEqual(environment["SSL_CERT_FILE"], "/etc/configured-ca.pem")

    def test_device_io_helper_is_static_with_nonexecutable_stack(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / "work"
            destination = root / "root" / "sbin" / "nvx-device-io"
            work.mkdir()
            destination.parent.mkdir(parents=True)

            def compile_helper(
                command: list[str | os.PathLike[str]], **_kwargs: object
            ) -> None:
                output = Path(command[command.index("-o") + 1])
                output.write_bytes(b"static-elf")

            with (
                patch.object(build, "require_tool", return_value="cc"),
                patch.object(build, "run_checked", side_effect=compile_helper) as run,
            ):
                provenance = build._build_device_io_helper(work, destination)

            command = run.call_args.args[0]
            self.assertIn("-nostdlib", command)
            self.assertIn("-static", command)
            self.assertIn("-Wl,-z,noexecstack", command)
            self.assertEqual(destination.read_bytes(), b"static-elf")
            self.assertEqual(
                provenance["binary_sha256"], common.sha256_file(destination)
            )
            self.assertEqual(len(provenance["source_sha256"]), 64)

    def test_sandbox_kernel_config_requires_every_feature(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / ".config"
            config.write_text(
                "\n".join(build.REQUIRED_SANDBOX_KERNEL_CONFIG) + "\n",
                encoding="utf-8",
            )
            build._assert_sandbox_kernel_config(config)

            for missing in build.REQUIRED_SANDBOX_KERNEL_CONFIG:
                with self.subTest(missing=missing):
                    config.write_text(
                        "\n".join(
                            setting
                            for setting in build.REQUIRED_SANDBOX_KERNEL_CONFIG
                            if setting != missing
                        )
                        + "\n",
                        encoding="utf-8",
                    )
                    with self.assertRaisesRegex(common.ScriptError, missing):
                        build._assert_sandbox_kernel_config(config)

    def test_checked_in_config_preserves_generic_sandbox_capabilities(self):
        config = build.REPO_ROOT / "kernel" / "config-microvm"
        build._assert_sandbox_kernel_config(config)
        configured = config.read_text(encoding="utf-8").splitlines()
        for setting in (
            "CONFIG_SECCOMP_FILTER=y",
            "CONFIG_UNIX=y",
            "# CONFIG_OVERLAY_FS_REDIRECT_ALWAYS_FOLLOW is not set",
        ):
            self.assertIn(setting, configured)

    def test_shared_status_kernel_config_is_required(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / ".config"
            config.write_text(
                "\n".join(build.REQUIRED_SHARED_STATUS_KERNEL_CONFIG) + "\n",
                encoding="utf-8",
            )
            build._assert_shared_status_kernel_config(config)

            config.write_text("", encoding="utf-8")
            with self.assertRaisesRegex(
                common.ScriptError,
                build.REQUIRED_SHARED_STATUS_KERNEL_CONFIG[0],
            ):
                build._assert_shared_status_kernel_config(config)


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
            workload_identity=(1000, 1001),
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
                "microvm",
                "--microvm-sandbox-block",
                "distro:file:distro.erofs,ro",
                "--microvm-sandbox-block",
                "custom:file:custom.erofs,ro",
                "--microvm-sandbox-block",
                "scratch:file:scratch.ext4",
                "--microvm-workload-identity",
                "1000:1001",
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
        with self.assertRaisesRegex(common.ScriptError, "between 1"):
            sandbox.parse_workload_identity("0:0")

    def test_layer_parser_rejects_invalid_role_and_uuid(self):
        with self.assertRaisesRegex(common.ScriptError, "unsupported layer role"):
            sandbox.SandboxLayer.parse(
                "unknown,layer.erofs,11111111-1111-1111-1111-111111111111"
            )
        with self.assertRaisesRegex(common.ScriptError, "UUID is invalid"):
            sandbox.SandboxLayer.parse("distro,layer.erofs,not-a-uuid")

    def test_managed_lifecycle_provisions_and_deprovisions_state(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            layer_path = root / "distro.erofs"
            scratch_path = root / "scratch.ext4"
            layer_path.write_bytes(b"layer")
            scratch_path.write_bytes(b"scratch")
            launch = sandbox.SandboxLaunch(
                layers=(
                    sandbox.SandboxLayer(
                        role="distro",
                        path=layer_path,
                        uuid="11111111-1111-1111-1111-111111111111",
                    ),
                ),
                scratch=scratch_path,
            )
            state = root / "state"

            sandbox_lifecycle.provision(
                state,
                launch,
                hypervisor="whp",
                memory_mib=256,
                net=None,
                network_profile=None,
                network_egress=None,
                network_ingress=None,
                network_egress_allow=(),
                network_egress_deny=(),
                host_loopback=None,
                network_proxy=None,
                host_loopback_forward=(),
                cmdline="quiet",
            )

            config = json.loads(
                (state / sandbox_lifecycle.CONFIG_NAME).read_text(encoding="utf-8")
            )
            self.assertEqual(config["workload_uid"], 65534)
            self.assertEqual(config["hypervisor"], "whp")
            self.assertFalse((state / sandbox_lifecycle.RUNTIME_NAME).exists())
            with self.assertRaisesRegex(common.ScriptError, "already provisioned"):
                sandbox_lifecycle.provision(
                    state,
                    launch,
                    hypervisor="whp",
                    memory_mib=256,
                    net=None,
                    network_profile=None,
                    network_egress=None,
                    network_ingress=None,
                    network_egress_allow=(),
                    network_egress_deny=(),
                    host_loopback=None,
                    network_proxy=None,
                    host_loopback_forward=(),
                    cmdline="quiet",
                )

            sandbox_lifecycle.deprovision(state)
            self.assertFalse(state.exists())

    def test_managed_lifecycle_start_requests_openvmm_outcome(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            layer_path = root / "distro.erofs"
            scratch_path = root / "scratch.ext4"
            layer_path.write_bytes(b"layer")
            scratch_path.write_bytes(b"scratch")
            state = root / "state"
            sandbox_lifecycle.provision(
                state,
                sandbox.SandboxLaunch(
                    layers=(
                        sandbox.SandboxLayer(
                            role="distro",
                            path=layer_path,
                            uuid="11111111-1111-1111-1111-111111111111",
                        ),
                    ),
                    scratch=scratch_path,
                ),
                hypervisor="whp",
                memory_mib=256,
                net=None,
                network_profile=None,
                network_egress=None,
                network_ingress=None,
                network_egress_allow=(),
                network_egress_deny=(),
                host_loopback=None,
                network_proxy=None,
                host_loopback_forward=(),
                cmdline="quiet",
            )
            process = MagicMock()
            process.pid = 123
            process.stdin = io.BytesIO()
            session = MagicMock()
            context = MagicMock()
            context.__enter__.return_value = session

            def require(path: Path, _description: str) -> Path:
                return path

            with (
                patch.object(
                    sandbox_lifecycle,
                    "require_file",
                    side_effect=require,
                ),
                patch.object(
                    sandbox_lifecycle.subprocess,
                    "Popen",
                    return_value=process,
                ) as popen,
                patch.object(
                    sandbox_lifecycle.ControlSession,
                    "connect",
                    return_value=context,
                ),
            ):
                sandbox_lifecycle.start(state, 10)

            command = popen.call_args.args[0]
            self.assertEqual(
                Path(command[command.index("--microvm-report") + 1]),
                state / sandbox_lifecycle.OUTCOME_NAME,
            )
            session.ping.assert_called_once_with(10)

    def test_managed_exec_outcome_excludes_workload_data(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "outcome.json"
            sandbox_lifecycle.write_exec_outcome(
                path,
                sandbox_lifecycle.ManagedExecResult(
                    124,
                    "timeout",
                    b"sensitive stdout",
                    b"sensitive stderr",
                ),
            )
            outcome = json.loads(path.read_text(encoding="utf-8"))

        self.assertEqual(outcome["schema_version"], 1)
        self.assertEqual(
            outcome["outcome"],
            {
                "operation": "exec",
                "category": "timeout",
                "status_code": 124,
            },
        )
        self.assertEqual(len(outcome["operation_id"]), 32)
        encoded = json.dumps(outcome)
        self.assertNotIn("stdout", encoded)
        self.assertNotIn("stderr", encoded)
        self.assertNotIn("sensitive", encoded)

    def test_managed_exec_outcome_does_not_overwrite(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "outcome.json"
            path.write_text("existing", encoding="utf-8")
            with self.assertRaisesRegex(common.ScriptError, "already exists"):
                sandbox_lifecycle.write_exec_outcome(
                    path,
                    sandbox_lifecycle.ManagedExecResult(0, "exit", b"", b""),
                )

    def test_managed_stop_cleans_runtime_state_if_report_is_invalid(self):
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            sandbox_lifecycle._write_json(
                state / sandbox_lifecycle.RUNTIME_NAME,
                {
                    "format": sandbox_lifecycle.STATE_FORMAT,
                    "pid": 123,
                    "control_endpoint": "control.sock",
                },
            )
            (state / sandbox_lifecycle.CAPABILITY_NAME).write_bytes(b"x" * 32)
            (state / sandbox_lifecycle.CONTROL_SOCKET_NAME).touch()
            (state / sandbox_lifecycle.OUTCOME_NAME).write_text(
                "invalid",
                encoding="utf-8",
            )
            context = MagicMock()
            with (
                patch.object(
                    sandbox_lifecycle,
                    "_process_running",
                    side_effect=(True, False),
                ),
                patch.object(
                    sandbox_lifecycle.ControlSession,
                    "connect",
                    return_value=context,
                ),
                self.assertRaisesRegex(common.ScriptError, "outcome report"),
            ):
                sandbox_lifecycle.stop(state, 10)

            self.assertFalse((state / sandbox_lifecycle.RUNTIME_NAME).exists())
            self.assertFalse((state / sandbox_lifecycle.CAPABILITY_NAME).exists())
            self.assertFalse((state / sandbox_lifecycle.CONTROL_SOCKET_NAME).exists())
            self.assertTrue((state / sandbox_lifecycle.OUTCOME_NAME).exists())

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
    def test_snapshot_profile_records_are_parsed_and_summarized(self):
        record = benchmark.parse_snapshot_profile_line(
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
            b"phase=artifact_open exclusive=1 duration_ns=1500000 "
            b"process_elapsed_ns=2000000 pid=42 logical_bytes=1024 "
            b"allocated_bytes=512\r"
        )

        self.assertIsNotNone(record)
        assert record is not None
        self.assertEqual(record["operation"], "restore")
        self.assertEqual(record["duration_ns"], 1_500_000)
        self.assertTrue(record["exclusive"])
        summary = benchmark.summarize_lifecycle_profiles(
            [{"records": [record, {**record, "duration_ns": 2_500_000}]}]
        )
        metric = summary["phases"]["restore.artifact_open"]
        self.assertEqual(metric["samples_ms"], [1.5, 2.5])
        self.assertEqual(metric["p50_ms"], 2.0)
        self.assertEqual(metric["p95_ms"], 2.5)
        self.assertTrue(metric["exclusive"])

    def test_snapshot_profile_rejects_incomplete_records(self):
        with self.assertRaisesRegex(ValueError, "missing"):
            benchmark.parse_snapshot_profile_line(
                b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore phase=open"
            )

    def test_snapshot_profile_records_console_input_dispatch(self):
        collector = benchmark.SnapshotProfileCollector(42, 100)

        with patch.object(benchmark, "process_resource_counters", return_value={}):
            collector.feed(
                b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture "
                b"phase=input_gate exclusive=1 duration_ns=20 "
                b"process_elapsed_ns=120 pid=42\n"
                b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture "
                b"phase=publication_commit exclusive=1 duration_ns=10 "
                b"process_elapsed_ns=310 pid=42\n"
            )
            sample = collector.finish_capture(200, 260, 300, 400, 450, 4096)

        records = cast(list[dict[str, object]], sample["records"])
        by_phase = {str(record["phase"]): record for record in records}
        generation = by_phase["snapshot_generation"]
        self.assertEqual(generation["duration_ns"], 210)
        self.assertEqual(generation["source"], "openvmm_clock")
        self.assertEqual(sample["generation_duration_ns"], 210)
        dispatch = by_phase["console_input_dispatch"]
        self.assertEqual(dispatch["operation"], "capture")
        self.assertEqual(dispatch["phase"], "console_input_dispatch")
        self.assertEqual(dispatch["duration_ns"], 60)
        self.assertEqual(dispatch["observer_elapsed_ns"], 160)
        self.assertEqual(dispatch["logical_bytes"], 4096)
        self.assertTrue(dispatch["exclusive"])
        round_trip = by_phase["console_command_round_trip"]
        self.assertEqual(round_trip["phase"], "console_command_round_trip")
        self.assertEqual(round_trip["duration_ns"], 100)
        guest_to_publication = by_phase["guest_dispatch_to_publication"]
        self.assertEqual(guest_to_publication["phase"], "guest_dispatch_to_publication")
        self.assertEqual(guest_to_publication["duration_ns"], 100)
        self.assertEqual(by_phase["request_to_publication"]["duration_ns"], 200)

    def test_snapshot_generation_requires_valid_process_clock_boundaries(self):
        gate: dict[str, object] = {
            "operation": "capture",
            "phase": "input_gate",
            "duration_ns": 20,
            "process_elapsed_ns": 120,
            "pid": 42,
        }
        publication: dict[str, object] = {
            "operation": "capture",
            "phase": "publication_commit",
            "duration_ns": 10,
            "process_elapsed_ns": 310,
            "pid": 42,
        }
        self.assertEqual(
            benchmark.snapshot_generation_duration_ns([gate, publication], 42), 210
        )
        for records, error in (
            ([publication], "exactly one 'input_gate'"),
            ([gate], "exactly one 'publication_commit'"),
            ([gate, gate, publication], "exactly one 'input_gate'"),
            ([gate, publication, publication], "exactly one 'publication_commit'"),
            ([gate, {**publication, "pid": 43}], "different process"),
            ([{**gate, "duration_ns": -1}, publication], "not monotonic"),
            ([{**gate, "process_elapsed_ns": 10}, publication], "not monotonic"),
            ([gate, {**publication, "process_elapsed_ns": 125}], "not monotonic"),
        ):
            with self.subTest(records=records):
                with self.assertRaisesRegex(ValueError, error):
                    benchmark.snapshot_generation_duration_ns(records, 42)

    def test_capture_excludes_polling_input_and_matches_profile_generation(self):
        for delay_ms in (0, 500, 2000):
            for profiled in (False, True):
                with self.subTest(delay_ms=delay_ms, profiled=profiled):
                    self.check_capture_timing(delay_ms, profiled)

    def check_capture_timing(self, delay_ms: int, profiled: bool) -> None:
        clock_ns = 0
        writes: list[bytes] = []

        class FakeProcess:
            pid = 42
            returncode = 0

            def poll(self) -> int:
                return 0

            def wait(self) -> int:
                nonlocal clock_ns
                clock_ns += 1_000_000
                return 0

        class FakeInteraction:
            process = FakeProcess()

            def read_output(self, _chunks: object) -> None:
                pass

            def write_input(self, data: bytes) -> None:
                nonlocal clock_ns
                writes.append(data)
                if data == b"nvx-snapshot\n":
                    clock_ns += 20_000_000

            def close(self) -> None:
                pass

        chunks = iter(
            (
                (benchmark.BOOT_MARKER + b"\n", 0, False),
                (benchmark.SMP_PROBE_COMPLETION_MARKER + b"\n", 1, False),
                (benchmark.SNAPSHOT_GUEST_DISPATCH_MARKER + b"\n", delay_ms, False),
                (
                    b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture "
                    b"phase=input_gate exclusive=1 duration_ns=500000 "
                    b"process_elapsed_ns=500000000 pid=42\n"
                    b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture "
                    b"phase=publication_commit exclusive=1 duration_ns=100000 "
                    b"process_elapsed_ns=502500000 pid=42\n",
                    3,
                    True,
                ),
                (None, 0, False),
            )
        )
        profiles: list[dict[str, object]] = []
        with tempfile.TemporaryDirectory() as temporary:
            snapshot = Path(temporary) / "snapshot"

            def read_chunk(*, timeout: float) -> bytes | None:
                nonlocal clock_ns
                del timeout
                chunk, advance_ms, publish = next(chunks)
                clock_ns += advance_ms * 1_000_000
                if publish:
                    snapshot.mkdir()
                return chunk

            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=FakeInteraction()
                ) as interaction,
                patch.object(benchmark.threading, "Thread"),
                patch.object(benchmark.queue, "Queue") as queues,
                patch.object(
                    benchmark.time, "perf_counter_ns", side_effect=lambda: clock_ns
                ),
                patch.object(benchmark, "_try_peak_rss", return_value=1024),
                patch.object(
                    benchmark, "process_resource_counters", return_value={}
                ) as counters,
            ):
                queues.return_value.get.side_effect = read_chunk
                result = benchmark.capture_snapshot(
                    ["openvmm"],
                    snapshot,
                    backend="whp",
                    processors=1,
                    timeout=5,
                    snapshot_profile=profiled,
                    profile_sink=profiles,
                )

        self.assertEqual(result, (3.0, delay_ms + 23.0, 1.0, 1024))
        self.assertEqual(interaction.call_args.args[0], ["openvmm"])
        self.assertEqual(
            interaction.call_args.args[1][benchmark.SNAPSHOT_PROFILE_ENV], "1"
        )
        self.assertEqual(writes[1:], [b"nvx-snapshot\n"])
        self.assertIn(
            b"echo OPENVMM-SNAPSHOT-RESTORE-OK\nnvx-exit 0\n",
            writes[0],
        )
        if profiled:
            phases = benchmark.summarize_lifecycle_profiles(profiles)["phases"]
            self.assertEqual(phases["capture.snapshot_generation"]["p50_ms"], result[0])
            self.assertEqual(
                phases["capture.console_command_round_trip"]["p50_ms"],
                delay_ms + 20.0,
            )
        else:
            counters.assert_not_called()
            self.assertEqual(profiles, [])

    def test_warm_snapshot_cache_reads_every_artifact(self):
        with tempfile.TemporaryDirectory() as temporary:
            snapshot = Path(temporary)
            for index, name in enumerate(benchmark.SNAPSHOT_FILENAMES):
                (snapshot / name).write_bytes(bytes([index]) * 32)

            benchmark.warm_snapshot_artifacts(snapshot)

    def test_measure_once_does_not_resend_prequeued_guest_exit(self):
        class FakeProcess:
            pid = 123
            returncode = 0

            def poll(self):
                return None

            def terminate(self):
                raise AssertionError("guest-exit teardown terminated the host process")

        class FakeInteraction:
            def __init__(self):
                self.process = FakeProcess()
                self.writes: list[bytes] = []

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(b"/ # echo OPENVMM-SNAPSHOT-RESTORE-OK\r\n")
                chunks.put(b"OPENVMM-SNAPSHOT-RESTORE-OK\r\n")

            def write_input(self, data: bytes):
                self.writes.append(data)

            def close(self):
                pass

        interaction = FakeInteraction()
        with (
            patch.object(benchmark, "InteractiveProcess", return_value=interaction),
            patch.object(benchmark, "peak_rss_bytes", return_value=1024),
            patch.object(benchmark, "wait_for_process_exit", return_value=0),
            patch.object(
                benchmark.time,
                "perf_counter_ns",
                side_effect=[0, 10_000_000, 17_000_000],
            ),
        ):
            result = benchmark.measure_once(
                ["openvmm"],
                environment={},
                timeout=1,
                marker=benchmark.RESTORE_MARKER,
                marker_must_be_line=True,
                guest_exit_prequeued=True,
            )

        self.assertEqual(result, (10.0, 1024, 7.0, 17.0))
        self.assertEqual(interaction.writes, [])

    def test_measure_once_retains_rss_when_process_exits_after_marker(self):
        class FakeProcess:
            pid = 123

            def __init__(self):
                self.exited = False
                self.returncode: int | None = None

            def poll(self):
                if self.exited:
                    self.returncode = 0
                return self.returncode

            def terminate(self):
                raise AssertionError("unexpected process termination")

        class FakeInteraction:
            def __init__(self):
                self.process = FakeProcess()

            def read_output(self, chunks: queue.Queue[bytes | None]):
                self.process.exited = True
                chunks.put(benchmark.RESTORE_MARKER + b"\n")

            def write_input(self, data: bytes):
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self):
                pass

        interaction = FakeInteraction()

        def peak_rss_bytes(_pid: int):
            if interaction.process.returncode is not None:
                raise ProcessLookupError(3, "No such process")
            return 1024

        with (
            patch.object(benchmark, "InteractiveProcess", return_value=interaction),
            patch.object(
                benchmark, "peak_rss_bytes", side_effect=peak_rss_bytes
            ) as read_peak_rss,
            patch.object(benchmark, "wait_for_process_exit", return_value=0),
        ):
            result = benchmark.measure_once(
                ["openvmm"],
                environment={},
                timeout=1,
                marker=benchmark.RESTORE_MARKER,
                marker_must_be_line=True,
                guest_exit_prequeued=True,
            )

        self.assertEqual(result[1], 1024)
        self.assertIsNone(interaction.process.returncode)
        read_peak_rss.assert_called_once_with(123)

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
            processors=4,
            network_profile="portable",
        )

        self.assertEqual(
            command[2:8],
            ["--machine", "microvm", "--processors", "4", "--hypervisor", "mshv"],
        )

        self.assertEqual(
            command[-2:],
            [
                "--network-profile",
                "portable",
            ],
        )

    def test_snapshot_restore_command_separates_capacity_from_online_target(self):
        command = benchmark.snapshot_restore_command(
            Path("openvmm"),
            "kvm",
            Path("snapshot"),
            processors=8,
            restore_processors=4,
        )

        self.assertEqual(command[command.index("--processors") + 1], "8")
        self.assertEqual(command[command.index("--restore-processors") + 1], "4")

    def test_snapshot_restore_command_sets_memory_target_separately(self):
        command = benchmark.snapshot_restore_command(
            Path("openvmm"),
            "whp",
            Path("snapshot"),
            restore_memory_mib=2048,
        )

        self.assertNotIn("--memory", command)
        self.assertEqual(command[command.index("--restore-memory") + 1], "2048M")

    def test_memory_online_latency_parser_validates_added_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "restore.log"
            log.write_bytes(
                b"NVX-MEMORY-ONLINE-OK: added_bytes=536870912 "
                b"memtotal_kib=1000000 elapsed_us=12345\n"
            )
            self.assertEqual(
                benchmark._memory_online_elapsed_ms(log, 536870912),
                12.345,
            )
            with self.assertRaisesRegex(RuntimeError, "expected 1"):
                benchmark._memory_online_elapsed_ms(log, 1)

    def test_benchmark_snapshot_restore_propagates_processors(self):
        args = argparse.Namespace(
            processors=4,
            network_profile=None,
            warmups=1,
            runs=1,
            timeout=1.0,
            teardown_mode="guest-exit",
        )
        result = cast(benchmark.BenchmarkResult, {"p50_ms": 10.0})
        with patch.object(benchmark, "benchmark", return_value=result) as run:
            self.assertIs(
                benchmark.benchmark_snapshot_restore(
                    args,
                    Path("openvmm"),
                    "kvm",
                    ["unused-cold-command"],
                    snapshot_path=Path("snapshot"),
                ),
                result,
            )

        command = run.call_args.args[0]
        self.assertEqual(
            command[2:8],
            ["--machine", "microvm", "--processors", "4", "--hypervisor", "kvm"],
        )
        self.assertTrue(run.call_args.kwargs["marker_must_be_line"])
        self.assertTrue(run.call_args.kwargs["guest_exit_prequeued"])

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
            ioapic_irq=10,
        )
        self.assertIn('ping -c 2 -W 1 "10.0.0.1"', network_script)
        self.assertIn("SMP-IOAPIC-FAIL", network_script)
        self.assertIn("SMP-IOAPIC-OK", network_script)
        with self.assertRaises(ValueError):
            benchmark.smp_probe_script(3)
        with self.assertRaises(ValueError):
            benchmark.smp_probe_script(4, network_gateway="10.0.0.1")

    def test_snapshot_request_triggers_waiting_capture_controller(self):
        script = benchmark.snapshot_request_script(
            4,
            teardown_mode="guest-exit",
        )
        self.assertEqual(script, "nvx-snapshot\n")

        no_controller = benchmark.snapshot_request_script(
            None,
            teardown_mode="guest-exit",
        )
        self.assertEqual(
            no_controller,
            "echo NVX-SNAPSHOT-DISPATCHED\n"
            "nvx-snapshot\n"
            "echo OPENVMM-SNAPSHOT-RESTORE-OK\n"
            "nvx-exit 0\n",
        )
        self.assertNotIn(
            "nvx-exit",
            benchmark.snapshot_request_script(None, teardown_mode="host-terminate"),
        )

    def test_prepare_snapshot_capture_stages_waiting_controller(self):
        script = benchmark.prepare_snapshot_capture_script(
            4,
            backend="whp",
            teardown_mode="guest-exit",
            network_gateway="10.0.0.1",
            ioapic_irq=10,
        )

        self.assertTrue(
            script.startswith(
                f"cat >{benchmark.SMP_PROBE_PATH} <<'NVX_SMP_PROBE_SCRIPT'\n"
            )
        )
        self.assertIn('ping -c 2 -W 1 "10.0.0.1"', script)
        self.assertIn("NVX-SMP-PROBE-OK", script)
        self.assertIn(
            f"cat >{benchmark.SNAPSHOT_CAPTURE_PATH} <<'NVX_SNAPSHOT_CAPTURE_SCRIPT'\n",
            script,
        )
        self.assertIn("IFS= read -r trigger\n", script)
        self.assertIn("echo NVX-SNAPSHOT-DISPATCHED\n", script)
        self.assertIn('while [ "$(cat "$clock_path")" = tsc-early ]', script)
        self.assertIn(
            "SMP-CLOCKSOURCE-FAIL expected=stable actual=$current_clocksource",
            script,
        )
        self.assertIn(
            "/sbin/nvx-snapshot\necho OPENVMM-SNAPSHOT-RESTORE-OK\nnvx-exit 0\n",
            script,
        )
        host_terminated = benchmark.prepare_snapshot_capture_script(
            4, backend="kvm", teardown_mode="host-terminate"
        )
        self.assertNotIn("clock_tries", host_terminated)
        self.assertNotIn("nvx-exit 0", host_terminated)
        self.assertTrue(
            script.endswith(
                "NVX_SNAPSHOT_CAPTURE_SCRIPT\n"
                f"chmod +x {benchmark.SMP_PROBE_PATH} "
                f"{benchmark.SNAPSHOT_CAPTURE_PATH}\n"
                f"{benchmark.SNAPSHOT_CAPTURE_PATH}\n"
            )
        )

    def test_output_marker_must_be_a_complete_line(self):
        marker = benchmark.RESTORE_MARKER
        self.assertFalse(
            benchmark.contains_output_line(
                b"/ # echo OPENVMM-SNAPSHOT-RESTORE-OK\r\n",
                marker,
            )
        )
        self.assertTrue(
            benchmark.contains_output_line(
                b"/ # echo OPENVMM-SNAPSHOT-RESTORE-OK\r\n"
                b"OPENVMM-SNAPSHOT-RESTORE-OK\r\n",
                marker,
            )
        )

    def test_device_restore_marker_and_trace_parsers(self):
        marker = benchmark.parse_device_restore_marker(
            "NVX-VIRTIO-RESTORE-PROBE phase=io-success device=net mode=deferred\r"
        )
        self.assertEqual(
            marker,
            {"phase": "io-success", "device": "net", "mode": "deferred"},
        )
        self.assertIsNone(benchmark.parse_device_restore_marker("unrelated"))

        event = benchmark.parse_virtio_restore_event(
            "DEBUG virtio_restore: virtio restore lifecycle "
            'event: "queue_start", device_type: 0x1a, trigger: "driver-ok", '
            "queue_index: 0, restored_progress: true, success: true"
        )
        self.assertEqual(
            event,
            {
                "event": "queue_start",
                "device_type": 0x1A,
                "trigger": "driver-ok",
                "queue_index": 0,
                "restored_progress": True,
                "success": True,
            },
        )
        self.assertIsNone(benchmark.parse_virtio_restore_event("unrelated"))

    def test_device_restore_commands_use_fixed_attachments(self):
        common = (
            Path("openvmm"),
            "whp",
            Path("vmlinux"),
            Path("initrd"),
            256,
        )
        console_boot, console_restore = benchmark.device_restore_commands(
            *common, "console", "deferred"
        )
        self.assertIn(
            "nvx_virtio_restore_probe=console,deferred",
            console_boot[console_boot.index("--cmdline") + 1],
        )
        self.assertEqual(console_boot[-2:], ["--virtio-console", "console"])
        self.assertEqual(console_restore[-2:], ["--virtio-console", "console"])

        net_boot, net_restore = benchmark.device_restore_commands(
            *common, "net", "active"
        )
        self.assertIn("--network-profile", net_boot)
        self.assertIn("--network-profile", net_restore)

        fs_boot, fs_restore = benchmark.device_restore_commands(
            *common,
            "virtiofs",
            "deferred",
            host_directory=Path("host-share"),
        )
        self.assertIn("--mount", fs_boot)
        self.assertIn("--mount", fs_restore)

    def test_device_restore_sample_rejects_premature_activation(self):
        markers = [
            {
                "phase": "restore-ready",
                "device": "net",
                "mode": "deferred",
                "observer_elapsed_ms": "8.0",
            },
            {
                "phase": "trigger",
                "device": "net",
                "mode": "deferred",
                "observer_elapsed_ms": "10.0",
            },
        ]
        events: list[dict[str, object]] = [
            {
                "event": "restore_staged",
                "device_type": 1,
                "trigger": "inactive-start",
                "queue_index": -1,
                "restored_progress": False,
                "success": True,
                "observer_elapsed_ms": 5.0,
            },
            {
                "event": "private_state_apply",
                "device_type": 1,
                "trigger": "kick",
                "queue_index": -1,
                "restored_progress": False,
                "success": True,
                "observer_elapsed_ms": 9.0,
            },
            {
                "event": "kick_staged",
                "device_type": 1,
                "trigger": "kick",
                "queue_index": 0,
                "restored_progress": False,
                "success": True,
                "observer_elapsed_ms": 10.5,
            },
        ]
        for queue_index in range(2):
            events.append(
                {
                    "event": "queue_start",
                    "device_type": 1,
                    "trigger": "driver-ok",
                    "queue_index": queue_index,
                    "restored_progress": False,
                    "success": True,
                    "observer_elapsed_ms": 11.0,
                }
            )
        events.append(
            {
                "event": "kick_dispatch",
                "device_type": 1,
                "trigger": "driver-ok",
                "queue_index": 0,
                "restored_progress": False,
                "success": True,
                "observer_elapsed_ms": 12.0,
            }
        )
        sample = cast(
            benchmark.DeviceRestoreSample,
            {
                "process_launch_to_ready_ms": 8.0,
                "trigger_to_first_successful_io_ms": 2.0,
                "peak_rss_bytes": 1,
                "guest_markers": markers,
                "events": events,
                "queue_start_count": 0,
                "staged_kick_dispatch_count": 0,
                "stale_premature_callback_count": 0,
                "log": "sample.log",
            },
        )
        with self.assertRaisesRegex(RuntimeError, "premature"):
            benchmark.validate_device_restore_sample(sample, "net", "deferred")
        events[1]["observer_elapsed_ms"] = 10.25
        benchmark.validate_device_restore_sample(sample, "net", "deferred")
        self.assertEqual(sample["queue_start_count"], 2)
        self.assertEqual(sample["staged_kick_dispatch_count"], 1)
        self.assertEqual(sample["stale_premature_callback_count"], 0)

    def test_device_restore_profile_writes_standalone_result(self):
        args = argparse.Namespace(
            processors=1,
            restore_devices=["console"],
            restore_modes=["active"],
            net=None,
            memory_mib=128,
            timeout=1.0,
            warmups=1,
            runs=2,
            platform="windows-whp-baremetal",
        )

        def sample() -> benchmark.DeviceRestoreSample:
            return {
                "process_launch_to_ready_ms": 10.0,
                "trigger_to_first_successful_io_ms": 2.0,
                "peak_rss_bytes": 1024,
                "guest_markers": [
                    {
                        "phase": "restore-ready",
                        "device": "console",
                        "mode": "active",
                        "observer_elapsed_ms": "10.0",
                    },
                    {
                        "phase": "trigger",
                        "device": "console",
                        "mode": "active",
                        "observer_elapsed_ms": "11.0",
                    },
                ],
                "events": [
                    {
                        "event": "restore_staged",
                        "device_type": 3,
                        "trigger": "active-start",
                        "queue_index": -1,
                        "restored_progress": False,
                        "success": True,
                    },
                    {
                        "event": "private_state_apply",
                        "device_type": 3,
                        "trigger": "active-start",
                        "queue_index": -1,
                        "restored_progress": False,
                        "success": True,
                    },
                    {
                        "event": "kick_dispatch",
                        "device_type": 3,
                        "trigger": "kick",
                        "queue_index": 1,
                        "restored_progress": False,
                        "success": True,
                    },
                    *[
                        {
                            "event": "queue_start",
                            "device_type": 3,
                            "trigger": "active-start",
                            "queue_index": queue_index,
                            "restored_progress": True,
                            "success": True,
                        }
                        for queue_index in range(2)
                    ],
                ],
                "queue_start_count": 0,
                "staged_kick_dispatch_count": 0,
                "stale_premature_callback_count": 0,
                "log": "",
            }

        def restore_sample(
            *_args: object, **_kwargs: object
        ) -> benchmark.DeviceRestoreSample:
            return sample()

        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            with (
                patch.object(
                    benchmark,
                    "capture_device_restore_snapshot",
                    return_value=[
                        {
                            "phase": "capture-ready",
                            "device": "console",
                            "mode": "active",
                        }
                    ],
                ) as capture,
                patch.object(
                    benchmark,
                    "run_device_restore_sample",
                    side_effect=restore_sample,
                ) as restore,
            ):
                path = benchmark.benchmark_device_restore_profile(
                    args,
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "whp",
                    output_dir,
                )

            self.assertEqual(capture.call_count, 1)
            self.assertEqual(restore.call_count, 3)
            document = json.loads(path.read_text(encoding="utf-8"))
            self.assertTrue(document["non_canonical"])
            self.assertTrue(document["assertions"]["zero_stale_premature_callbacks"])
            scenario = document["scenarios"]["console-active"]
            self.assertEqual(
                scenario["process_launch_to_ready_ms"]["samples"], [10.0, 10.0]
            )
            self.assertEqual(scenario["process_launch_to_ready_ms"]["p50"], 10.0)
            self.assertEqual(scenario["queue_start_counts"], [2, 2])

    def test_device_restore_profile_writes_lf_only_virtiofs_seed(self):
        args = argparse.Namespace(
            processors=1,
            restore_devices=["virtiofs"],
            restore_modes=["active"],
            net=None,
            memory_mib=128,
            timeout=1.0,
            warmups=1,
            runs=1,
            platform="windows-whp-baremetal",
        )
        observed_seeds: list[bytes] = []
        host_directories: list[Path] = []

        def commands(*_args: object, **kwargs: object) -> tuple[list[str], list[str]]:
            host_directory = cast(Path, kwargs["host_directory"])
            host_directories.append(host_directory)
            observed_seeds.append(
                (host_directory / ".nvx-virtio-restore-host-seed").read_bytes()
            )
            return ["boot"], ["restore"]

        sample = cast(
            benchmark.DeviceRestoreSample,
            {
                "process_launch_to_ready_ms": 1.0,
                "trigger_to_first_successful_io_ms": 1.0,
                "peak_rss_bytes": 1,
                "guest_markers": [],
                "events": [],
                "queue_start_count": 0,
                "staged_kick_dispatch_count": 0,
                "stale_premature_callback_count": 0,
                "log": "sample.log",
            },
        )

        def run_sample(
            *_args: object, **_kwargs: object
        ) -> benchmark.DeviceRestoreSample:
            (host_directories[-1] / ".nvx-virtio-restore-guest-result").write_bytes(
                benchmark.VIRTFS_GUEST_TO_HOST
            )
            return sample

        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(
                    benchmark, "device_restore_commands", side_effect=commands
                ),
                patch.object(
                    benchmark,
                    "capture_device_restore_snapshot",
                    return_value=[],
                ),
                patch.object(
                    benchmark,
                    "run_device_restore_sample",
                    side_effect=run_sample,
                ),
                patch.object(
                    benchmark,
                    "summarize_device_restore_samples",
                    return_value={"stale_premature_callback_count": 0},
                ),
            ):
                benchmark.benchmark_device_restore_profile(
                    args,
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "whp",
                    Path(temporary),
                )

        self.assertEqual(observed_seeds, [benchmark.VIRTFS_HOST_TO_GUEST])

    def test_device_restore_profile_rejects_reused_output(self):
        args = argparse.Namespace(
            suite="device-restore-profile",
            output=None,
            output_dir=None,
            platform="linux-kvm-baremetal",
        )
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            (output_dir / "stale.log").write_text("stale", encoding="ascii")
            args.output_dir = output_dir
            with self.assertRaisesRegex(FileExistsError, "empty output directory"):
                benchmark.run_workload_benchmarks(
                    args,
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "kvm",
                )

    def test_benchmark_cpu_set_reserves_host_worker_capacity(self):
        benchmark.validate_benchmark_cpu_set(set(range(10)), 8)
        with self.assertRaisesRegex(ValueError, "requires at least 10"):
            benchmark.validate_benchmark_cpu_set(set(range(9)), 8)
        benchmark.validate_benchmark_cpu_set({0, 2, 4}, 1)
        benchmark.validate_benchmark_cpu_set(set(range(8)), 8, 0)
        with self.assertRaisesRegex(ValueError, "requires at least 8"):
            benchmark.validate_benchmark_cpu_set(set(range(7)), 8, 0)

    def test_snapshot_capture_runs_smp_probe_for_selected_count(self):
        args = argparse.Namespace(
            warmups=0,
            runs=1,
            timeout=1.0,
            processors=8,
            teardown_mode="guest-exit",
        )
        with patch.object(
            benchmark,
            "capture_snapshot",
            return_value=(1.0, 1.0, 1.0, 1024),
        ) as capture:
            benchmark.benchmark_snapshot_capture(args, "whp", ["openvmm"])

        self.assertEqual(capture.call_args.kwargs["backend"], "whp")
        self.assertEqual(capture.call_args.kwargs["processors"], 8)
        self.assertEqual(capture.call_args.kwargs["teardown_mode"], "guest-exit")

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
            "teardown_samples_ms": [1.0],
            "teardown_completed_samples_ms": [1.0],
            "teardown_timeout_count": 0,
            "teardown_timeout_seconds": 5.0,
            "teardown_p50_ms": 1.0,
            "teardown_p95_ms": 1.0,
            "teardown_min_ms": 1.0,
            "teardown_max_ms": 1.0,
        }
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
                "kvm",
            )

        self.assertEqual(probe.call_count, 1)
        self.assertIn("--network-profile", probe.call_args.args[0])
        self.assertIn("SMP-IOAPIC-OK", probe.call_args.args[1])
        self.assertEqual(probe.call_args.args[2], benchmark.SMP_PROBE_COMPLETION_MARKER)

    def test_shell_snapshot_restore_suite_only_measures_restore(self):
        args = argparse.Namespace(
            processors=8,
            shell_memories=[512],
            warmups=1,
            runs=5,
            timeout=40.0,
            teardown_mode="guest-exit",
            network_profile=None,
            snapshot_profile=True,
        )
        result = cast(
            benchmark.BenchmarkResult,
            {
                "samples_ms": [10.0, 12.0],
                "profile": {
                    "schema_version": 1,
                    "raw_samples": [],
                    "phases": {
                        "startup.partition_build": {
                            "exclusive": True,
                            "source": "openvmm",
                            "samples_ms": [1.0],
                            "p50_ms": 1.0,
                            "p95_ms": 1.0,
                            "min_ms": 1.0,
                            "max_ms": 1.0,
                        }
                    },
                },
            },
        )
        with (
            patch.object(benchmark, "capture_snapshot") as capture,
            patch.object(benchmark, "benchmark", return_value=result) as run,
            patch("sys.stdout", new_callable=io.StringIO) as output,
        ):
            benchmark.benchmark_shell_snapshot_restore_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
            )

        capture.assert_called_once()
        capture_command = capture.call_args.args[0]
        self.assertNotIn("shellsnap", capture_command)
        self.assertEqual(capture.call_args.kwargs["processors"], 8)
        self.assertEqual(capture.call_args.kwargs["teardown_mode"], "guest-exit")
        run.assert_called_once()
        restore_command = run.call_args.args[0]
        self.assertIn("--restore-snapshot", restore_command)
        self.assertEqual(
            restore_command[restore_command.index("--processors") + 1], "8"
        )
        self.assertIn("== 512 MiB ==", output.getvalue())
        self.assertIn("snapshot restore", output.getvalue())
        self.assertNotIn("cold boot", output.getvalue())
        self.assertIn('marker : "OPENVMM-SNAPSHOT-RESTORE-OK"', output.getvalue())
        self.assertTrue(run.call_args.kwargs["snapshot_profile"])
        self.assertEqual(run.call_args.kwargs["marker"], benchmark.RESTORE_MARKER)
        self.assertTrue(run.call_args.kwargs["marker_must_be_line"])
        self.assertTrue(run.call_args.kwargs["guest_exit_prequeued"])
        self.assertIn(
            "shell-snapshot-restore/whp/8vcpu/512-mib lifecycle phases:",
            output.getvalue(),
        )

    def test_restore_vcpu_suite_reuses_one_capacity_snapshot_for_matrix(self):
        args = argparse.Namespace(
            processors=8,
            memory_mib=512,
            warmups=1,
            runs=5,
            timeout=40.0,
            teardown_mode="guest-exit",
            snapshot_profile=True,
        )
        result = cast(
            benchmark.BenchmarkResult,
            {
                "samples_ms": [10.0, 12.0],
                "peak_rss_samples_bytes": [1024, 2048],
                "profile": {
                    "schema_version": 1,
                    "raw_samples": [],
                    "phases": {
                        "restore.saved_state_restore": {
                            "exclusive": True,
                            "source": "openvmm",
                            "samples_ms": [2.0],
                            "p50_ms": 2.0,
                            "p95_ms": 2.0,
                            "min_ms": 2.0,
                            "max_ms": 2.0,
                        }
                    },
                },
            },
        )
        with (
            patch.object(benchmark, "capture_automatic_snapshot") as capture,
            patch.object(benchmark, "benchmark", return_value=result) as run,
            patch("sys.stdout", new_callable=io.StringIO) as output,
        ):
            benchmark.benchmark_snapshot_restore_vcpu_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "mshv",
            )

        capture.assert_called_once()
        self.assertEqual(capture.call_args.kwargs["timeout"], args.timeout)
        capture_command = capture.call_args.args[0]
        self.assertEqual(
            capture_command[capture_command.index("--processors") + 1], "8"
        )
        self.assertTrue(any("maxcpus=1" in argument for argument in capture_command))
        self.assertEqual(run.call_count, 4)
        targets: list[int] = []
        for invocation in run.call_args_list:
            command = invocation.args[0]
            self.assertEqual(invocation.kwargs["timeout"], args.timeout)
            self.assertTrue(invocation.kwargs["snapshot_profile"])
            self.assertEqual(command[command.index("--processors") + 1], "8")
            targets.append(int(command[command.index("--restore-processors") + 1]))
        self.assertEqual(targets, [1, 2, 4, 8])
        self.assertIn("restore-online 8 vCPU", output.getvalue())
        self.assertIn(
            "snapshot-restore-vcpu/mshv/capacity-8/online-1 lifecycle phases:",
            output.getvalue(),
        )

        args.processors = 4
        with self.assertRaisesRegex(ValueError, "requires --processors 8"):
            benchmark.benchmark_snapshot_restore_vcpu_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
            )

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

    def test_nearest_rank_percentile(self):
        self.assertEqual(benchmark.nearest_rank_percentile(range(1, 22), 95), 20)
        self.assertEqual(benchmark.nearest_rank_percentile([7.0], 95), 7.0)
        with self.assertRaisesRegex(ValueError, "without samples"):
            benchmark.nearest_rank_percentile([], 95)

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

            args = argparse.Namespace(
                warmups=1,
                runs=2,
                timeout=1.0,
                processors=1,
                teardown_mode="guest-exit",
            )
            with patch.object(benchmark, "capture_snapshot", side_effect=capture):
                result = benchmark.benchmark_snapshot_capture(
                    args,
                    "kvm",
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
                processors=1,
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
            self.assertTrue(run.call_args.kwargs["marker_must_be_line"])
            self.assertEqual(run.call_args.kwargs["teardown_mode"], "guest-exit")
            self.assertTrue(run.call_args.kwargs["guest_exit_prequeued"])

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
                    "0-2",
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
                "p95_ms": 10.0,
                "min_ms": 10.0,
                "max_ms": 10.0,
                "wall_samples_ms": [12.0],
                "wall_p50_ms": 12.0,
                "wall_p95_ms": 12.0,
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
                "teardown_p95_ms": 2.0,
                "teardown_min_ms": 2.0,
                "teardown_max_ms": 2.0,
            }
            capture_result: benchmark.SnapshotCaptureResult = {
                "samples_ms": [3.0],
                "p50_ms": 3.0,
                "p95_ms": 3.0,
                "min_ms": 3.0,
                "max_ms": 3.0,
                "request_to_publication_samples_ms": [3.0],
                "request_to_publication_p50_ms": 3.0,
                "request_to_publication_p95_ms": 3.0,
                "request_to_publication_min_ms": 3.0,
                "request_to_publication_max_ms": 3.0,
                "post_publication_exit_samples_ms": [1.0],
                "post_publication_exit_p50_ms": 1.0,
                "post_publication_exit_p95_ms": 1.0,
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
            processors=4,
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
            self.assertEqual(metadata["microvm_abi_version"], 2)
            self.assertEqual(metadata["processors"], 8)
            self.assertEqual(metadata["host_affinity_set"], args.cpus)
            self.assertEqual(metadata["host_cpu_reserve"], args.host_cpu_reserve)
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

            output = repository / "data" / "runs" / f"{platform}-microvm-v2-1vcpu"
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
            repository = Path(temporary)
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
                / "microvm-v2"
                / "1vcpu"
                / "device-io"
            )
            self.assertEqual(
                device_io.call_args.kwargs["output_path"],
                output / "device-io.log",
            )
            self.assertEqual(metadata.call_args.args[1], output)

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

    def test_download_command_defaults_to_integration_repository(self):
        args = nvx.parse_args(["download"])
        expected_platform = "windows-whp" if os.name == "nt" else "linux-kvm"

        with patch.object(nvx, "download_latest_release") as download_release:
            args.handler(args)

        download_release.assert_called_once_with("microsoft/nvx", expected_platform)


AUTHORIZATION_VALUE = "Bearer placeholder-value"


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

    def test_forbidden_release_query_reports_github_message_and_hint(self):
        error = urllib.error.HTTPError(
            "https://api.github.invalid/releases",
            403,
            "Forbidden",
            http.client.HTTPMessage(),
            io.BytesIO(
                json.dumps(
                    {"message": "Resource protected by organization SAML enforcement."}
                ).encode("utf-8")
            ),
        )

        with (
            patch("nvx_tools.release.urllib.request.urlopen", side_effect=error),
            self.assertRaises(release.ScriptError) as context,
        ):
            release._latest_release_asset("example/nvx", "linux-kvm", "token")

        message = str(context.exception)
        self.assertIn("HTTP 403", message)
        self.assertIn("organization SAML enforcement", message)
        self.assertIn("read access to the repository contents", message)

    def test_unauthenticated_release_query_reports_token_hint(self):
        error = urllib.error.HTTPError(
            "https://api.github.invalid/releases",
            404,
            "Not Found",
            http.client.HTTPMessage(),
            io.BytesIO(b"not json"),
        )

        with (
            patch("nvx_tools.release.urllib.request.urlopen", side_effect=error),
            self.assertRaisesRegex(release.ScriptError, "set GH_TOKEN"),
        ):
            release._latest_release_asset("example/nvx", "linux-kvm", None)

    def test_exhausted_rate_limit_reports_retry_hint(self):
        headers = http.client.HTTPMessage()
        headers["x-ratelimit-remaining"] = "0"
        error = urllib.error.HTTPError(
            "https://api.github.invalid/releases",
            403,
            "Forbidden",
            headers,
            io.BytesIO(b""),
        )

        with (
            patch("nvx_tools.release.urllib.request.urlopen", side_effect=error),
            self.assertRaisesRegex(release.ScriptError, "rate limit is exhausted"),
        ):
            release._latest_release_asset("example/nvx", "linux-kvm", "token")

    def test_forbidden_query_falls_back_to_public_release(self):
        error = urllib.error.HTTPError(
            "https://api.github.invalid/releases",
            403,
            "Forbidden",
            http.client.HTTPMessage(),
            io.BytesIO(
                json.dumps(
                    {"message": "Resource protected by organization SAML enforcement."}
                ).encode("utf-8")
            ),
        )
        public_response = io.BytesIO(
            json.dumps(
                [
                    {
                        "draft": False,
                        "tag_name": "v1.2.3",
                        "assets": [
                            {
                                "name": "nvx-1.2.3-linux-kvm.tar.gz",
                                "url": "https://api.example.invalid/linux",
                                "size": 300,
                            }
                        ],
                    }
                ]
            ).encode("utf-8")
        )

        with (
            patch(
                "nvx_tools.release.urllib.request.urlopen",
                side_effect=[error, public_response],
            ) as urlopen,
            patch("sys.stderr", io.StringIO()) as stderr,
        ):
            asset, download_token = release._latest_release_asset_with_fallback(
                "example/nvx",
                "linux-kvm",
                "token",
            )

        self.assertEqual(urlopen.call_count, 2)
        authenticated_request = urlopen.call_args_list[0].args[0]
        public_request = urlopen.call_args_list[1].args[0]
        self.assertIsNotNone(authenticated_request.get_header("Authorization"))
        self.assertIsNone(public_request.get_header("Authorization"))
        self.assertEqual(asset.tag, "v1.2.3")
        self.assertIsNone(download_token)
        self.assertIn("retrying without credentials", stderr.getvalue())

    def test_asset_download_drops_credentials_across_origins(self):
        handler = common._CrossOriginRedirectHandler()
        request = urllib.request.Request(
            "https://api.github.invalid/assets/1",
            headers={"Authorization": AUTHORIZATION_VALUE, "Accept": "*/*"},
        )

        redirected = handler.redirect_request(
            request,
            io.BytesIO(b""),
            302,
            "Found",
            http.client.HTTPMessage(),
            "https://objects.github.invalid/assets/1?signature=abc",
        )
        same_host = handler.redirect_request(
            request,
            io.BytesIO(b""),
            302,
            "Found",
            http.client.HTTPMessage(),
            "https://api.github.invalid/assets/2",
        )
        downgraded = handler.redirect_request(
            request,
            io.BytesIO(b""),
            302,
            "Found",
            http.client.HTTPMessage(),
            "http://api.github.invalid/assets/3",
        )

        self.assertIsNotNone(redirected)
        self.assertIsNotNone(same_host)
        self.assertIsNotNone(downgraded)
        assert (
            redirected is not None and same_host is not None and downgraded is not None
        )
        self.assertIsNone(redirected.get_header("Authorization"))
        self.assertEqual(redirected.get_header("Accept"), "*/*")
        self.assertEqual(same_host.get_header("Authorization"), AUTHORIZATION_VALUE)
        self.assertIsNone(downgraded.get_header("Authorization"))

    def test_rejected_token_falls_back_to_public_release(self):
        asset = release._ReleaseAsset(
            "v1.2.3",
            "nvx-1.2.3-linux-kvm.tar.gz",
            "https://api.github.invalid/assets/1",
            4,
        )
        authenticated_error = release._GitHubReleaseQueryError(
            403,
            "authenticated query failed",
        )
        stderr = io.StringIO()

        def write_archive(
            _url: str,
            destination: Path,
            **kwargs: object,
        ) -> None:
            self.assertIsInstance(
                kwargs["opener"],
                urllib.request.OpenerDirector,
            )
            self.assertNotIn("Authorization", cast(dict[str, str], kwargs["headers"]))
            destination.write_bytes(b"data")

        with (
            patch.dict(os.environ, {"GH_TOKEN": "token"}),
            patch.object(
                release,
                "_latest_release_asset",
                side_effect=[authenticated_error, asset],
            ) as latest_release_asset,
            patch.object(release, "download", side_effect=write_archive) as download,
            patch.object(release, "_install_release_archive") as install,
            patch("sys.stderr", stderr),
        ):
            release.download_latest_release("example/nvx", "linux-kvm")

        self.assertEqual(
            latest_release_asset.call_args_list,
            [
                call("example/nvx", "linux-kvm", "token"),
                call("example/nvx", "linux-kvm", None),
            ],
        )
        self.assertEqual(download.call_count, 1)
        install.assert_called_once()
        self.assertIn("retrying without credentials", stderr.getvalue())

    def test_failed_public_fallback_preserves_authenticated_error(self):
        authenticated_error = release._GitHubReleaseQueryError(
            403,
            "authenticated query failed",
        )
        public_error = release._GitHubReleaseQueryError(
            404,
            "public query failed",
        )

        with (
            patch.dict(os.environ, {"GH_TOKEN": "token"}),
            patch.object(
                release,
                "_latest_release_asset",
                side_effect=[authenticated_error, public_error],
            ),
            patch("sys.stderr", io.StringIO()),
            self.assertRaisesRegex(
                release.ScriptError,
                "authenticated query failed",
            ),
        ):
            release.download_latest_release("example/nvx", "linux-kvm")

    def test_authenticated_asset_download_uses_credential_safe_opener(self):
        asset = release._ReleaseAsset(
            "v1.2.3",
            "nvx-1.2.3-linux-kvm.tar.gz",
            "https://api.github.invalid/assets/1",
            4,
        )

        def write_archive(
            _url: str,
            destination: Path,
            **kwargs: object,
        ) -> None:
            self.assertIsInstance(
                kwargs["opener"],
                urllib.request.OpenerDirector,
            )
            self.assertEqual(
                kwargs["headers"],
                release._github_headers("token", "application/octet-stream"),
            )
            destination.write_bytes(b"data")

        with (
            patch.dict(os.environ, {"GH_TOKEN": "token"}),
            patch.object(release, "_latest_release_asset", return_value=asset),
            patch.object(release, "download", side_effect=write_archive) as download,
            patch.object(release, "_install_release_archive") as install,
        ):
            release.download_latest_release("example/nvx", "linux-kvm")

        self.assertEqual(download.call_count, 1)
        install.assert_called_once()

    def test_binary_package_stages_files_and_checksums(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths, kernel_inputs, revision = _write_release_fixture(root)
            build_dir = paths["build"]
            source_dir = paths["source"]
            openvmm_dir = paths["openvmm"]
            binary = paths["binary"]
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
                patch.object(
                    release,
                    "kernel_provenance_inputs",
                    return_value=kernel_inputs,
                ),
                patch.object(
                    release,
                    "_openvmm_git_state",
                    return_value=(revision, True),
                ),
                patch("sys.stderr", stderr),
            ):
                release.package_release(
                    version="1.0.0",
                    destination=destination,
                    include_source=False,
                    force=False,
                )

            self.assertTrue((destination / "bin" / binary.name).is_file())
            for name in release.GUEST_RELEASE_NAMES:
                self.assertTrue((destination / "guest" / name).is_file())
            self.assertTrue(
                (destination / "provenance" / build.OPENVMM_PROVENANCE_NAME).is_file()
            )
            self.assertTrue(
                (destination / "provenance" / build.KERNEL_PROVENANCE_NAME).is_file()
            )
            manifest = json.loads(
                (destination / "SOURCE-MANIFEST.json").read_text(encoding="utf-8")
            )
            self.assertEqual(manifest["distribution"]["version"], "1.0.0")
            self.assertNotIn("guest_agent", manifest)
            self.assertEqual(manifest["openvmm"]["source_revision"], revision)
            self.assertEqual(
                manifest["openvmm"]["executable_sha256"],
                common.sha256_file(destination / "bin" / binary.name),
            )
            self.assertEqual(
                manifest["openvmm"]["microvm_abi_version"],
                2,
            )
            self.assertEqual(
                manifest["openvmm"]["control_session_protocol_version"],
                1,
            )
            self.assertEqual(
                manifest["openvmm"]["control_contract_revision"],
                "nvx-microvm-v2-control-v1",
            )
            self.assertEqual(
                manifest["linux"]["kernel_sha256"],
                common.sha256_file(destination / "guest" / "vmlinux"),
            )
            self.assertEqual(
                manifest["linux"]["config_sha256"],
                common.sha256_file(destination / "guest" / "vmlinux.config"),
            )
            common.verify_sha256_sums(destination)
            self.assertIn("binary-only package", stderr.getvalue())

    def test_package_rejects_dirty_openvmm_provenance(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths, _kernel_inputs, revision = _write_release_fixture(root)

            with (
                patch.object(
                    release,
                    "_openvmm_git_state",
                    return_value=(revision, False),
                ),
                self.assertRaisesRegex(
                    common.ScriptError,
                    "current clean pinned source",
                ),
            ):
                release._validate_openvmm_provenance(
                    paths["binary"],
                    paths["build"] / build.OPENVMM_PROVENANCE_NAME,
                )

    def test_package_rejects_stale_kernel_provenance(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths, kernel_inputs, _revision = _write_release_fixture(root)
            kernel = paths["build"] / "vmlinux"
            kernel.write_bytes(b"new kernel bytes")

            with (
                patch.object(
                    release,
                    "kernel_provenance_inputs",
                    return_value=kernel_inputs,
                ),
                self.assertRaisesRegex(
                    common.ScriptError,
                    "kernel build provenance",
                ),
            ):
                release._validate_kernel_provenance(
                    kernel,
                    paths["build"] / "vmlinux.config",
                    paths["build"] / build.KERNEL_PROVENANCE_NAME,
                )

    def test_package_rejects_tampered_source_metadata_without_replacing_output(self):
        cases: tuple[tuple[str, str, object, str], ...] = (
            ("root", "format", 2, "format must be 1"),
            ("linux", "version", "6.18.37", "Linux version"),
            (
                "linux",
                "upstream_archive_sha256",
                "0" * 64,
                "Linux upstream_archive_sha256",
            ),
            ("linux", "patches", [], "Linux patches"),
            (
                "alpine",
                "minirootfs_sha256",
                "0" * 64,
                "Alpine minirootfs_sha256",
            ),
        )
        for section, field, invalid_value, error in cases:
            with (
                self.subTest(section=section, field=field),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                paths, kernel_inputs, revision = _write_release_fixture(root)
                fixture_build_dir = paths["build"]
                manifest_path = root / "SOURCE-MANIFEST.json"
                manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
                if section == "root":
                    manifest[field] = invalid_value
                else:
                    manifest_section = cast(dict[str, object], manifest[section])
                    manifest_section[field] = invalid_value
                manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
                destination = root / "dist" / "1.0.0"
                destination.mkdir(parents=True)
                marker = destination / "prior.txt"
                marker.write_text("prior release", encoding="utf-8")

                def artifact_path(
                    name: str,
                    build_dir: Path = fixture_build_dir,
                ) -> Path:
                    return build_dir / name

                with (
                    patch.object(release, "REPO_ROOT", root),
                    patch.object(release, "SOURCE_DIR", paths["source"]),
                    patch.object(release, "OPENVMM_DIR", paths["openvmm"]),
                    patch.object(
                        release,
                        "artifact_path",
                        side_effect=artifact_path,
                    ),
                    patch.object(
                        release,
                        "openvmm_binary_path",
                        return_value=paths["binary"],
                    ),
                    patch.object(
                        release,
                        "kernel_provenance_inputs",
                        return_value=kernel_inputs,
                    ),
                    patch.object(
                        release,
                        "_openvmm_git_state",
                        return_value=(revision, True),
                    ),
                    patch("sys.stderr", io.StringIO()),
                    self.assertRaisesRegex(
                        common.ScriptError,
                        error,
                    ),
                ):
                    release.package_release(
                        version="1.0.0",
                        destination=destination,
                        include_source=False,
                        force=True,
                    )

                self.assertEqual(
                    marker.read_text(encoding="utf-8"),
                    "prior release",
                )

    def test_failed_staged_verification_preserves_existing_release(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths, kernel_inputs, revision = _write_release_fixture(root)
            build_dir = paths["build"]
            destination = root / "dist" / "1.0.0"
            destination.mkdir(parents=True)
            marker = destination / "prior.txt"
            marker.write_text("prior release", encoding="utf-8")

            def artifact_path(name: str) -> Path:
                return build_dir / name

            original_copy = release._copy_release_file

            def corrupt_packaged_binary(source: Path, target: Path) -> None:
                original_copy(source, target)
                if target.name == paths["binary"].name:
                    target.write_bytes(b"corrupt")

            with (
                patch.object(release, "REPO_ROOT", root),
                patch.object(release, "SOURCE_DIR", paths["source"]),
                patch.object(release, "OPENVMM_DIR", paths["openvmm"]),
                patch.object(release, "artifact_path", side_effect=artifact_path),
                patch.object(
                    release,
                    "openvmm_binary_path",
                    return_value=paths["binary"],
                ),
                patch.object(
                    release,
                    "kernel_provenance_inputs",
                    return_value=kernel_inputs,
                ),
                patch.object(
                    release,
                    "_openvmm_git_state",
                    return_value=(revision, True),
                ),
                patch.object(
                    release,
                    "_copy_release_file",
                    side_effect=corrupt_packaged_binary,
                ),
                patch("sys.stderr", io.StringIO()),
                self.assertRaisesRegex(
                    common.ScriptError,
                    "packaged OpenVMM executable",
                ),
            ):
                release.package_release(
                    version="1.0.0",
                    destination=destination,
                    include_source=False,
                    force=True,
                )

            self.assertEqual(marker.read_text(encoding="utf-8"), "prior release")
            self.assertEqual(
                [path.name for path in destination.iterdir()], ["prior.txt"]
            )

    def test_publication_failure_restores_existing_release(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            destination = root / "dist" / "release"
            destination.mkdir(parents=True)
            (destination / "prior.txt").write_text("prior", encoding="utf-8")
            staging = root / "staging"
            staging.mkdir()
            (staging / "new.txt").write_text("new", encoding="utf-8")
            original_replace = Path.replace

            def fail_staging_replace(source: Path, target: Path) -> Path:
                if source == staging:
                    raise OSError("injected publication failure")
                return original_replace(source, target)

            with (
                patch.object(release, "REPO_ROOT", root),
                patch.object(Path, "replace", new=fail_staging_replace),
                self.assertRaisesRegex(
                    common.ScriptError,
                    "failed to publish staged release",
                ),
            ):
                release._publish_release_directory(
                    staging,
                    destination,
                    force=True,
                )

            self.assertEqual(
                (destination / "prior.txt").read_text(encoding="utf-8"),
                "prior",
            )
            self.assertFalse(staging.exists())

    def test_failed_rollback_preserves_prior_backup_and_staging(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            destination = root / "dist" / "release"
            destination.mkdir(parents=True)
            (destination / "prior.txt").write_text("prior", encoding="utf-8")
            staging = root / "staging"
            staging.mkdir()
            (staging / "new.txt").write_text("new", encoding="utf-8")
            original_replace = Path.replace

            def fail_publication_and_restore(source: Path, target: Path) -> Path:
                if source == staging or source.name.startswith(".release.backup-"):
                    raise OSError("injected rename failure")
                return original_replace(source, target)

            with (
                patch.object(release, "REPO_ROOT", root),
                patch.object(Path, "replace", new=fail_publication_and_restore),
                self.assertRaisesRegex(
                    common.ScriptError,
                    "prior release remains",
                ),
            ):
                release._publish_release_directory(
                    staging,
                    destination,
                    force=True,
                )

            backups = list(destination.parent.glob(".release.backup-*"))
            self.assertEqual(len(backups), 1)
            self.assertEqual(
                (backups[0] / "prior.txt").read_text(encoding="utf-8"),
                "prior",
            )
            self.assertEqual(
                (staging / "new.txt").read_text(encoding="utf-8"),
                "new",
            )

    def test_publication_rechecks_no_force_after_staging(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            destination = root / "release"
            destination.mkdir()
            (destination / "prior.txt").write_text("prior", encoding="utf-8")
            staging = root / "staging"
            staging.mkdir()
            (staging / "new.txt").write_text("new", encoding="utf-8")

            with self.assertRaisesRegex(
                common.ScriptError,
                "pass --force",
            ):
                release._publish_release_directory(
                    staging,
                    destination,
                    force=False,
                )

            self.assertEqual(
                (destination / "prior.txt").read_text(encoding="utf-8"),
                "prior",
            )
            self.assertFalse(staging.exists())

    def test_publication_rechecks_force_destination_scope(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            destination = root / "outside-dist"
            destination.mkdir()
            (destination / "prior.txt").write_text("prior", encoding="utf-8")
            staging = root / "staging"
            staging.mkdir()
            (staging / "new.txt").write_text("new", encoding="utf-8")

            with (
                patch.object(release, "REPO_ROOT", root),
                self.assertRaisesRegex(
                    common.ScriptError,
                    "below dist",
                ),
            ):
                release._publish_release_directory(
                    staging,
                    destination,
                    force=True,
                )

            self.assertEqual(
                (destination / "prior.txt").read_text(encoding="utf-8"),
                "prior",
            )
            self.assertFalse(staging.exists())

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
            for name in (
                build.OPENVMM_PROVENANCE_NAME,
                build.KERNEL_PROVENANCE_NAME,
            ):
                provenance = package_root / "provenance" / name
                provenance.parent.mkdir(parents=True, exist_ok=True)
                provenance.write_bytes(name.encode("ascii"))
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
            for name in (
                build.OPENVMM_PROVENANCE_NAME,
                build.KERNEL_PROVENANCE_NAME,
            ):
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
