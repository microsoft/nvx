#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import argparse
import ast
import errno
import hashlib
import http.client
import http.server
import io
import json
import lzma
import os
import queue
import re
import select
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
import uuid
import zipfile
from pathlib import Path, PurePosixPath
from typing import cast
from unittest.mock import MagicMock, call, patch

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import (  # noqa: E402
    aci_edge_sandboxes_tests,
    archive,
    azurelinux,
    benchmark,
    build,
    build_config,
    ci,
    collect_alpine_sources,
    collect_ubuntu_sources,
    common,
    guests,
    release,
    sandbox,
    sandbox_lifecycle,
    ubuntu,
)
from nvx_tools.build_constants import (  # noqa: E402
    AlpineBuildConstants,
    AzureLinuxBuildConstants,
    BuildConstants,
    DockerBuildConstants,
    InitramfsBuildConstants,
    KernelBuildConstants,
    OpenVMMBuildConstants,
    ReleaseBuildConstants,
    UbuntuBuildConstants,
    ZstdBuildConstants,
)


def _workflow_job(workflow: str, job_name: str) -> str:
    lines = workflow.splitlines()
    start = lines.index(f"  {job_name}:")
    end = next(
        (
            index
            for index, line in enumerate(lines[start + 1 :], start + 1)
            if len(line) > 2 and line[:2] == "  " and not line[2].isspace()
        ),
        len(lines),
    )
    return "\n".join(lines[start:end])


def _composite_action_step(action: str, step_name: str) -> str:
    lines = action.splitlines()
    start = lines.index(f"    - name: {step_name}")
    end = next(
        (
            index
            for index, line in enumerate(lines[start + 1 :], start + 1)
            if line.startswith("    - ")
        ),
        len(lines),
    )
    return "\n".join(lines[start:end])


def _composite_action_script(action: str, step_name: str) -> str:
    lines = _composite_action_step(action, step_name).splitlines()
    start = lines.index("      run: |") + 1
    end = next(
        (
            index
            for index, line in enumerate(lines[start:], start)
            if line and not line.startswith("        ")
        ),
        len(lines),
    )
    return "\n".join(line[8:] for line in lines[start:end])


def _write_release_fixture(
    root: Path,
) -> tuple[dict[str, Path], dict[str, object], str]:
    build_dir = root / "build"
    source_dir = build_dir / "sources"
    openvmm_dir = root / "openvmm"
    binary_name = "openvmm.exe" if os.name == "nt" else "openvmm"
    binary = openvmm_dir / "target" / "release" / binary_name
    revision = "0bc357bbcf3a654b63dfb51f1103c5751bf3d31f"
    guest_names = ReleaseBuildConstants.GUEST_ARTIFACT_NAMES
    source_files = (
        set(build._initramfs_source_files())
        | set(ubuntu.customization_files())
        | set(azurelinux.input_files())
    )
    for source in source_files:
        destination = root / source.relative_to(BuildConstants.REPO_ROOT)
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, destination)
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
    for artifact_name, manifest_name in (
        (
            "initramfs-ubuntu.cpio.gz",
            "initramfs-ubuntu.cpio.gz.packages.json",
        ),
        ("ubuntu-distro.erofs", "ubuntu-distro.erofs.manifest.json"),
    ):
        artifact = build_dir / artifact_name
        input_sha256 = ubuntu.converter_input_sha256(ubuntu.customization_files())
        (build_dir / manifest_name).write_text(
            json.dumps(
                {
                    "format": 1,
                    "guest": "ubuntu",
                    "artifact": artifact.name,
                    "artifact_sha256": common.sha256_file(artifact),
                    "input_sha256": input_sha256,
                    "packages": [],
                }
            ),
            encoding="utf-8",
        )
    (build_dir / AzureLinuxBuildConstants.PACKAGE_MANIFEST_NAME).write_text(
        json.dumps(
            {
                "format": AzureLinuxBuildConstants.PACKAGE_MANIFEST_VERSION,
                "guest": AzureLinuxBuildConstants.GUEST_NAME,
                "artifact": AzureLinuxBuildConstants.INITRAMFS_NAME,
                "artifact_sha256": common.sha256_file(
                    build_dir / AzureLinuxBuildConstants.INITRAMFS_NAME
                ),
                "package_manifest_format": (
                    AzureLinuxBuildConstants.PACKAGE_MANIFEST_FORMAT
                ),
                "image": AzureLinuxBuildConstants.IMAGE,
                "input_sha256": azurelinux.input_sha256(),
            }
        ),
        encoding="utf-8",
    )
    generated_config = build_dir / "vmlinux.config"
    generated_config.write_text(
        "\n".join(
            (
                *KernelBuildConstants.REQUIRED_DIRECT_BOOT_CONFIG,
                *KernelBuildConstants.REQUIRED_VIRTIO_CONSOLE_CONFIG,
                *KernelBuildConstants.REQUIRED_SHARED_STATUS_CONFIG,
                *KernelBuildConstants.REQUIRED_SANDBOX_CONFIG,
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
            "upstream_archive_sha256": KernelBuildConstants.SHA256,
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
            "minirootfs_sha256": AlpineBuildConstants.MINIROOTFS_SHA256,
            "guest_sources": ["guest/common", "guest/alpine"],
            "package_manifests": ["build/initramfs.cpio.gz.packages.json"],
            "source_output": "build/sources/alpine",
        },
        "ubuntu": {
            "distribution": "Ubuntu Base",
            "version": UbuntuBuildConstants.VERSION,
            "codename": UbuntuBuildConstants.CODENAME,
            "architecture": UbuntuBuildConstants.ARCHITECTURE,
            "base_url": UbuntuBuildConstants.BASE_URL,
            "base_sha256": UbuntuBuildConstants.BASE_SHA256,
            "archive_keyring_url": (UbuntuBuildConstants.ARCHIVE_KEYRING_URL),
            "archive_keyring_sha256": (UbuntuBuildConstants.ARCHIVE_KEYRING_SHA256),
            "package_lock": "ubuntu/packages.lock.json",
            "package_lock_sha256": ubuntu.package_lock_sha256(),
            "guest_sources": [
                "guest/common",
                "guest/ubuntu",
                "ubuntu/packages.lock.json",
            ],
            "package_manifests": [
                "build/initramfs-ubuntu.cpio.gz.packages.json",
                "build/ubuntu-distro.erofs.manifest.json",
            ],
            "source_output": "build/sources/ubuntu",
            "erofs_converter_format": UbuntuBuildConstants.EROFS_FORMAT,
        },
        "azurelinux": {
            "distribution": "Azure Linux",
            "version": AzureLinuxBuildConstants.VERSION,
            "architecture": AzureLinuxBuildConstants.ARCHITECTURE,
            "image": AzureLinuxBuildConstants.IMAGE,
            "package_lock": "azurelinux/packages.lock.json",
            "package_lock_sha256": azurelinux.package_lock_sha256(),
            "guest_sources": [
                *(
                    path.as_posix()
                    for path in AzureLinuxBuildConstants.GUEST_SOURCE_DIRECTORIES
                ),
                "azurelinux/packages.lock.json",
            ],
            "package_manifests": [
                (
                    Path(BuildConstants.BUILD_DIRECTORY_NAME)
                    / AzureLinuxBuildConstants.PACKAGE_MANIFEST_NAME
                ).as_posix()
            ],
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
    (build_dir / OpenVMMBuildConstants.PROVENANCE_NAME).write_text(
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
    (build_dir / KernelBuildConstants.PROVENANCE_NAME).write_text(
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
    (build_dir / InitramfsBuildConstants.PROVENANCE_NAME).write_text(
        json.dumps(
            {
                "format": 1,
                "inputs": build.initramfs_provenance_inputs(),
                "initramfs_sha256": common.sha256_file(build_dir / "initramfs.cpio.gz"),
                "package_manifest_sha256": common.sha256_file(
                    build_dir / "initramfs.cpio.gz.packages.json"
                ),
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
    def test_guest_selection_includes_azure_linux(self):
        args = nvx.parse_args(["build-initramfs", "--guest", "azurelinux"])
        self.assertEqual(args.guest, "azurelinux")
        self.assertEqual(
            guests.guest_descriptor(args.guest).initramfs_name,
            "initramfs-azurelinux.cpio.gz",
        )

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
        self.assertEqual(args.shell_memories, [128, 256, 512, 1024])
        self.assertEqual(args.cache_state, "cold")
        self.assertFalse(args.snapshot_profile)
        self.assertIs(args.handler, benchmark.run)

    def test_benchmark_preserves_canonical_shell_memory_defaults(self):
        args = nvx.parse_args(["benchmark", "--suite", "performance"])

        benchmark.apply_benchmark_suite_defaults(args)

        self.assertEqual(args.shell_memories, [128, 256, 512])

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

        openvmm_unit_tests = nvx.parse_args(["test-openvmm-unit"])
        self.assertEqual(openvmm_unit_tests.command, "test-openvmm-unit")
        self.assertIs(openvmm_unit_tests.handler, nvx.command_test_openvmm_unit)

        openvmm_tests = nvx.parse_args(["test-openvmm", "--backend", "mshv"])
        self.assertEqual(openvmm_tests.backend, "mshv")
        self.assertIs(openvmm_tests.handler, nvx.command_test_openvmm)

    def test_openvmm_help_describes_petri_vmm_tests(self):
        with (
            patch("sys.stdout", new_callable=io.StringIO) as output,
            self.assertRaises(SystemExit) as exit_context,
        ):
            nvx.parse_args(["--help"])

        self.assertEqual(exit_context.exception.code, 0)
        help_text = output.getvalue()
        self.assertIn("run OpenVMM Petri VMM tests", help_text)
        self.assertNotIn("run OpenVMM microVM integration tests", help_text)

    def test_guest_selection_cli_contract(self):
        default_build = nvx.parse_args(["build-guest"])
        self.assertEqual(default_build.guest, "alpine")

        all_guests = nvx.parse_args(["build-guest", "--guest", "all"])
        self.assertEqual(all_guests.guest, "all")

        ubuntu_initramfs = nvx.parse_args(["build-initramfs", "--guest", "ubuntu"])
        self.assertEqual(ubuntu_initramfs.guest, "ubuntu")

        azurelinux_initramfs = nvx.parse_args(
            ["build-initramfs", "--guest", "azurelinux"]
        )
        with patch("nvx.build_docker_initramfs") as build_docker_initramfs:
            nvx.command_build_initramfs(azurelinux_initramfs)
        build_docker_initramfs.assert_called_once_with(
            build.DockerBuildConfig(artifact_destination=BuildConstants.BUILD_DIR),
            "azurelinux",
        )

        distro = nvx.parse_args(
            [
                "build-distro-layer",
                "--guest",
                "ubuntu",
                "--output",
                "ubuntu.erofs",
                "--replace",
            ]
        )
        self.assertEqual(distro.guest, "ubuntu")
        self.assertEqual(distro.output, Path("ubuntu.erofs"))
        self.assertTrue(distro.replace)

        with self.assertRaises(SystemExit):
            nvx.parse_args(["run", "--guest", "all"])

    def test_docker_initramfs_rejects_native_guest(self):
        with self.assertRaisesRegex(
            common.ScriptError, "Alpine Linux.*do not require Docker"
        ):
            build.build_docker_initramfs(build.DockerBuildConfig(), "alpine")

    def test_docker_initramfs_requires_expected_artifacts(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = build.DockerBuildConfig(artifact_destination=Path(temporary))
            with (
                patch.object(build, "require_tool"),
                patch.object(build, "run_checked") as run_checked,
                self.assertRaisesRegex(
                    common.ScriptError,
                    "initramfs-azurelinux.cpio.gz",
                ),
            ):
                build.build_docker_initramfs(config, "azurelinux")
        command = run_checked.call_args.args[0]
        self.assertEqual(
            command[command.index("--target") + 1],
            "azurelinux-initramfs-artifacts",
        )

    def test_ubuntu_run_selects_artifact_and_default_memory(self):
        args = nvx.parse_args(["run", "--guest", "ubuntu", "--dry-run"])

        def require(path: Path, _description: str) -> Path:
            return path

        with (
            patch.object(nvx, "require_file", side_effect=require),
            patch.object(
                nvx,
                "_format_command",
                return_value="formatted",
            ) as format_command,
        ):
            nvx.command_run(args)

        command = format_command.call_args.args[0]
        self.assertEqual(command[command.index("--memory") + 1], "256M")
        self.assertEqual(
            Path(command[command.index("--initrd") + 1]).name,
            "initramfs-ubuntu.cpio.gz",
        )

    def test_azurelinux_run_selects_artifact_and_default_memory(self):
        args = nvx.parse_args(["run", "--guest", "azurelinux", "--dry-run"])

        def require(path: Path, _description: str) -> Path:
            return path

        with (
            patch.object(nvx, "require_file", side_effect=require),
            patch.object(
                nvx,
                "_format_command",
                return_value="formatted",
            ) as format_command,
        ):
            nvx.command_run(args)

        command = format_command.call_args.args[0]
        self.assertEqual(command[command.index("--memory") + 1], "512M")
        self.assertEqual(
            Path(command[command.index("--initrd") + 1]).name,
            "initramfs-azurelinux.cpio.gz",
        )

    def test_restore_rejects_ubuntu_guest_selection(self):
        args = nvx.parse_args(
            [
                "run",
                "--guest",
                "ubuntu",
                "--restore-snapshot",
                "snapshot",
                "--dry-run",
            ]
        )
        with self.assertRaisesRegex(common.ScriptError, "snapshot already fixes"):
            nvx.command_run(args)

    def test_sandbox_rejects_systemd_entrypoint(self):
        for entrypoint in nvx.SYSTEMD_ENTRYPOINTS:
            with self.subTest(entrypoint=entrypoint):
                args = nvx.parse_args(
                    [
                        "sandbox",
                        "--entrypoint",
                        entrypoint,
                        "--layer",
                        "distro,distro.erofs,11111111-1111-1111-1111-111111111111",
                        "--scratch",
                        "scratch.ext4",
                    ]
                )
                with self.assertRaisesRegex(
                    common.ScriptError,
                    "systemd entrypoints",
                ):
                    nvx.command_sandbox(args)

    def test_sandbox_allows_non_systemd_sbin_init(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            layer = root / "distro.erofs"
            scratch = root / "scratch.ext4"
            layer.write_bytes(b"distro")
            scratch.write_bytes(b"scratch")
            args = nvx.parse_args(
                [
                    "sandbox",
                    "--entrypoint",
                    "/sbin/init",
                    "--layer",
                    f"distro,{layer},11111111-1111-1111-1111-111111111111",
                    "--scratch",
                    str(scratch),
                    "--dry-run",
                ]
            )

            def require(path: Path, _description: str) -> Path:
                return path

            with (
                patch.object(nvx, "require_file", side_effect=require),
                patch.object(nvx, "_format_command", return_value="formatted"),
            ):
                nvx.command_sandbox(args)

    def test_sandbox_rejects_systemd_from_bound_distro_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            layer = root / "distro.erofs"
            scratch = root / "scratch.ext4"
            layer.write_bytes(b"distro")
            scratch.write_bytes(b"scratch")
            layer.with_name(f"{layer.name}.manifest.json").write_text(
                json.dumps(
                    {
                        "format": 1,
                        "artifact": layer.name,
                        "artifact_sha256": common.sha256_file(layer),
                        "packages": [{"name": "systemd"}],
                    }
                ),
                encoding="utf-8",
            )
            args = nvx.parse_args(
                [
                    "sandbox",
                    "--entrypoint",
                    "/sbin/init",
                    "--layer",
                    f"distro,{layer},11111111-1111-1111-1111-111111111111",
                    "--scratch",
                    str(scratch),
                ]
            )
            with self.assertRaisesRegex(common.ScriptError, "systemd images"):
                nvx.command_sandbox(args)

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

    def test_run_and_sandbox_forward_network_arguments(self):
        network_arguments = (
            "--net 10.0.0.2/24 --network-profile portable "
            "--network-egress deny --network-ingress deny "
            "--network-egress-allow 140.82.112.0/20:tcp:443 "
            "--network-egress-deny 10.0.0.0/8 --host-loopback allow "
            "--network-proxy 10.0.0.1:3128 --host-loopback-forward tcp:8080:80"
        ).split()
        commands = [
            ["run", "--dry-run", *network_arguments],
            [
                "sandbox",
                "--dry-run",
                "--layer",
                "distro,distro.erofs,11111111-1111-1111-1111-111111111111",
                "--scratch",
                "scratch.ext4",
                *network_arguments,
            ],
        ]

        def require(path: Path, _description: str) -> Path:
            return path

        for arguments in commands:
            with self.subTest(command=arguments[0]):
                args = nvx.parse_args(arguments)
                with (
                    patch.object(nvx, "require_file", side_effect=require),
                    patch.object(sandbox, "require_file", side_effect=require),
                    patch.object(
                        nvx, "_format_command", return_value="formatted"
                    ) as format_command,
                ):
                    args.handler(args)

                command = format_command.call_args.args[0]
                self.assertEqual(
                    command[command.index("--net") :],
                    network_arguments,
                )

    def test_run_lowers_structured_egress_policy_before_launch(self):
        with tempfile.TemporaryDirectory() as temporary:
            policy = Path(temporary) / "policy.json"
            policy.write_text(
                json.dumps(
                    {
                        "allow": [
                            {
                                "cidr": "192.0.2.0/24",
                                "except": ["192.0.2.128/25"],
                                "protocol": "tcp",
                                "port": 8000,
                                "endPort": 8001,
                            },
                            {"cidr": "192.0.2.200/32"},
                        ],
                        "deny": [
                            {
                                "cidr": "192.0.2.0/24",
                                "protocol": "tcp",
                                "port": 8001,
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )
            args = nvx.parse_args(
                [
                    "run",
                    "--dry-run",
                    "--network-egress",
                    "deny",
                    "--network-egress-policy-file",
                    str(policy),
                ]
            )

            def require(path: Path, _description: str) -> Path:
                return path

            with (
                patch.object(nvx, "require_file", side_effect=require),
                patch.object(
                    nvx, "_format_command", return_value="formatted"
                ) as format_command,
            ):
                nvx.command_run(args)

        command = format_command.call_args.args[0]
        self.assertNotIn("--network-egress-policy-file", command)
        self.assertEqual(command.count("--network-egress-allow"), 3)
        self.assertIn("192.0.2.0/25:tcp:8000", command)
        self.assertIn("192.0.2.0/25:tcp:8001", command)
        self.assertIn("192.0.2.200/32", command)
        self.assertEqual(command.count("--network-egress-deny"), 1)
        self.assertIn("192.0.2.0/24:tcp:8001", command)

    def test_policy_file_rejects_ambiguous_flags_before_artifact_access(self):
        cases = (
            [
                "--network-egress",
                "deny",
                "--network-egress-policy-file",
                "policy.json",
                "--network-egress-allow",
                "192.0.2.1",
            ],
            ["--network-egress-policy-file", "policy.json"],
        )
        for extra in cases:
            with self.subTest(extra=extra):
                args = nvx.parse_args(["run", "--dry-run", *extra])
                with (
                    patch.object(nvx, "require_file") as require,
                    self.assertRaises(common.ScriptError),
                ):
                    nvx.command_run(args)
                require.assert_not_called()

    def test_sandbox_rejects_policy_mixing_before_validating_layer_files(self):
        for operation in ("run", "provision"):
            with self.subTest(operation=operation):
                args = nvx.parse_args(
                    [
                        "sandbox",
                        operation,
                        "--layer",
                        "distro,absent.erofs,00000000-0000-4000-8000-000000000001",
                        "--scratch",
                        "absent.ext4",
                        "--network-egress",
                        "deny",
                        "--network-egress-policy-file",
                        "absent.json",
                        "--network-egress-allow",
                        "192.0.2.1",
                    ]
                )
                with (
                    patch.object(nvx.SandboxLaunch, "validated") as validated,
                    self.assertRaisesRegex(common.ScriptError, "cannot be combined"),
                ):
                    nvx.command_sandbox(args)
                validated.assert_not_called()

    def test_managed_provision_persists_lowered_policy_not_source_path(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            layer = root / "distro.erofs"
            scratch = root / "scratch.ext4"
            policy = root / "policy.json"
            state = root / "state"
            layer.write_bytes(b"layer")
            scratch.write_bytes(b"scratch")
            policy.write_text(
                json.dumps(
                    {
                        "allow": [
                            {
                                "cidr": "192.0.2.0/24",
                                "except": ["192.0.2.128/25"],
                            }
                        ]
                    }
                ),
                encoding="utf-8",
            )
            args = nvx.parse_args(
                [
                    "sandbox",
                    "provision",
                    "--state-dir",
                    str(state),
                    "--layer",
                    f"distro,{layer},11111111-1111-1111-1111-111111111111",
                    "--scratch",
                    str(scratch),
                    "--network-egress",
                    "deny",
                    "--network-egress-policy-file",
                    str(policy),
                ]
            )

            nvx.command_sandbox(args)
            config = json.loads(
                (state / sandbox_lifecycle.CONFIG_NAME).read_text(encoding="utf-8")
            )
            policy.unlink()

            self.assertEqual(config["network_egress"], "deny")
            self.assertEqual(config["network_egress_allow"], ["192.0.2.0/25"])
            self.assertEqual(config["network_egress_deny"], [])
            self.assertNotIn("network_egress_policy_file", config)

    def test_public_cli_provision_persists_policy_after_source_removal(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            layer = root / "distro.erofs"
            scratch = root / "scratch.ext4"
            policy = root / "policy.json"
            state = root / "state"
            layer.write_bytes(b"layer")
            scratch.write_bytes(b"scratch")
            policy.write_text(
                json.dumps(
                    {
                        "allow": [
                            {
                                "cidr": "192.0.2.0/24",
                                "except": ["192.0.2.128/25"],
                                "protocol": "tcp",
                                "port": 8000,
                                "endPort": 8001,
                            }
                        ],
                        "deny": [
                            {
                                "cidr": "192.0.2.0/24",
                                "protocol": "tcp",
                                "port": 8001,
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )
            result = subprocess.run(
                [
                    sys.executable,
                    str(Path(nvx.__file__).resolve()),
                    "sandbox",
                    "provision",
                    "--state-dir",
                    str(state),
                    "--layer",
                    f"distro,{layer},11111111-1111-1111-1111-111111111111",
                    "--scratch",
                    str(scratch),
                    "--network-egress",
                    "deny",
                    "--network-egress-policy-file",
                    str(policy),
                ],
                cwd=BuildConstants.REPO_ROOT,
                capture_output=True,
                timeout=10,
            )
            self.assertEqual(
                result.returncode,
                0,
                result.stderr.decode("utf-8", "replace"),
            )
            policy.unlink()
            config = json.loads(
                (state / sandbox_lifecycle.CONFIG_NAME).read_text(encoding="utf-8")
            )

            self.assertEqual(
                config["network_egress_allow"],
                ["192.0.2.0/25:tcp:8000", "192.0.2.0/25:tcp:8001"],
            )
            self.assertEqual(
                config["network_egress_deny"],
                ["192.0.2.0/24:tcp:8001"],
            )
            self.assertNotIn(str(policy), json.dumps(config))

    def test_invalid_policy_has_no_managed_state_side_effect(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            layer = root / "distro.erofs"
            scratch = root / "scratch.ext4"
            policy = root / "policy.json"
            state = root / "state"
            layer.write_bytes(b"layer")
            scratch.write_bytes(b"scratch")
            policy.write_text(
                '{"allow":[{"cidr":"192.0.2.0/24","protocol":"tcp"}]}',
                encoding="utf-8",
            )
            args = nvx.parse_args(
                [
                    "sandbox",
                    "provision",
                    "--state-dir",
                    str(state),
                    "--layer",
                    f"distro,{layer},11111111-1111-1111-1111-111111111111",
                    "--scratch",
                    str(scratch),
                    "--network-egress",
                    "deny",
                    "--network-egress-policy-file",
                    str(policy),
                ]
            )

            with self.assertRaises(common.ScriptError):
                nvx.command_sandbox(args)
            self.assertFalse(state.exists())

    def test_managed_non_launch_operations_reject_network_policy_options(self):
        args = nvx.parse_args(
            [
                "sandbox",
                "start",
                "--state-dir",
                "state",
                "--network-egress",
                "deny",
                "--network-egress-policy-file",
                "policy.json",
            ]
        )
        with (
            patch.object(sandbox_lifecycle, "start") as start,
            self.assertRaisesRegex(
                common.ScriptError, "only valid for sandbox run or provision"
            ),
        ):
            nvx.command_sandbox(args)
        start.assert_not_called()

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

    def test_openvmm_command_passes_restore_choice_in_build_config(self):
        with patch.object(nvx, "build_openvmm") as build_openvmm:
            nvx.command_build_openvmm(argparse.Namespace(skip_restore=True))

        config = build_openvmm.call_args.args[0]
        self.assertIsInstance(config, build_config.OpenVmmBuildConfig)
        self.assertTrue(config.skip_restore)
        self.assertIsNone(config.backend)

    def test_build_commands_pass_backend_choice_in_config(self):
        for command in ("build-openvmm", "build"):
            for backend in (None, "kvm", "mshv", "whp"):
                with self.subTest(command=command, backend=backend):
                    arguments = [command, "--skip-restore"]
                    if backend is not None:
                        arguments.extend(["--backend", backend])
                    if command == "build":
                        arguments.append("--native")
                    args = nvx.parse_args(arguments)
                    with (
                        patch.object(nvx, "build_openvmm") as openvmm,
                        patch.object(nvx, "build_all") as combined,
                    ):
                        args.handler(args)

                    if command == "build":
                        aggregate = combined.call_args.args[0]
                        self.assertIsInstance(aggregate, build_config.BuildConfig)
                        self.assertTrue(aggregate.native_guest)
                        config = aggregate.openvmm
                        openvmm.assert_not_called()
                    else:
                        config = openvmm.call_args.args[0]
                        combined.assert_not_called()
                    self.assertIsInstance(config, build_config.OpenVmmBuildConfig)
                    self.assertEqual(config.backend, backend)
                    self.assertTrue(config.skip_restore)

    def test_build_commands_reject_unknown_backends(self):
        for command in ("build-openvmm", "build"):
            with self.subTest(command=command), self.assertRaises(SystemExit):
                nvx.parse_args([command, "--backend", "unknown"])

    def test_build_commands_pass_specialized_configs_to_consumers(self):
        with (
            patch.object(nvx, "build_kernel") as build_kernel,
            patch.object(nvx, "build_initramfs") as build_initramfs,
            patch.object(nvx, "record_openvmm_provenance") as provenance,
            patch.object(nvx, "collect_release_sources") as collect_sources,
        ):
            nvx.command_build_kernel(argparse.Namespace())
            nvx.command_build_initramfs(argparse.Namespace(guest="alpine"))
            nvx.command_record_openvmm_provenance(argparse.Namespace())
            nvx.command_collect_sources(argparse.Namespace())

        self.assertIsInstance(
            build_kernel.call_args.args[0],
            build_config.KernelBuildConfig,
        )
        self.assertIsInstance(
            build_initramfs.call_args.args[0],
            build_config.InitramfsBuildConfig,
        )
        self.assertIsInstance(
            provenance.call_args.args[0],
            build_config.OpenVmmBuildConfig,
        )
        self.assertIsInstance(
            collect_sources.call_args.args[0],
            build_config.DockerBuildConfig,
        )

    def test_record_openvmm_provenance_command_is_exposed(self):
        args = nvx.parse_args(["record-openvmm-provenance"])
        self.assertIs(args.handler, nvx.command_record_openvmm_provenance)


class CiTests(unittest.TestCase):
    def test_openvmm_unit_tests_run_nextest_and_doctests(self):
        with tempfile.TemporaryDirectory() as temporary:
            openvmm = Path(temporary) / "openvmm"
            openvmm.mkdir()
            (openvmm / "Cargo.toml").touch()
            fuzz_crates = common.CommandResult(
                args=("cargo", "xtask", "fuzz", "list", "--crates"),
                returncode=0,
                stdout=b"fuzz_alpha\nfuzz_beta\n",
                stderr=b"",
            )

            with (
                patch.object(OpenVMMBuildConstants, "DIRECTORY", openvmm),
                patch.object(ci, "require_tool", return_value="cargo"),
                patch.object(ci, "run_capture", return_value=fuzz_crates) as capture,
                patch.object(ci, "run_checked") as run_checked,
            ):
                ci.run_openvmm_unit_tests()

            capture.assert_called_once_with(
                ["cargo", "xtask", "fuzz", "list", "--crates"],
                cwd=openvmm,
            )
            self.assertEqual(run_checked.call_count, 3)
            self.assertEqual(
                run_checked.call_args_list[0],
                call(
                    [
                        "cargo",
                        "xflowey",
                        "restore-packages",
                        "--no-compat-igvm",
                    ],
                    cwd=openvmm,
                ),
            )
            command = run_checked.call_args_list[1].args[0]
            self.assertEqual(
                command[:10],
                [
                    "cargo",
                    "nextest",
                    "run",
                    "--profile",
                    "agent",
                    "--workspace",
                    "--tests",
                    "--bins",
                    "--features",
                    "ci",
                ],
            )
            excluded_packages = [
                command[index + 1]
                for index, argument in enumerate(command)
                if argument == "--exclude"
            ]
            self.assertEqual(
                excluded_packages,
                [
                    *ci.OPENVMM_UNIT_TEST_EXCLUDED_PACKAGES,
                    "fuzz_alpha",
                    "fuzz_beta",
                ],
            )
            self.assertEqual(run_checked.call_args_list[1].kwargs["cwd"], openvmm)
            self.assertEqual(
                run_checked.call_args_list[2],
                call(
                    [
                        "cargo",
                        "test",
                        "--locked",
                        "--doc",
                        "--workspace",
                        "--no-fail-fast",
                    ],
                    cwd=openvmm,
                ),
            )

    def test_openvmm_unit_tests_stop_when_fuzz_crate_query_fails(self):
        with tempfile.TemporaryDirectory() as temporary:
            openvmm = Path(temporary) / "openvmm"
            openvmm.mkdir()
            (openvmm / "Cargo.toml").touch()
            failed_query = common.CommandResult(
                args=("cargo", "xtask", "fuzz", "list", "--crates"),
                returncode=2,
                stdout=b"",
                stderr=b"query failed",
            )

            with (
                patch.object(OpenVMMBuildConstants, "DIRECTORY", openvmm),
                patch.object(ci, "require_tool", return_value="cargo"),
                patch.object(ci, "run_capture", return_value=failed_query),
                patch.object(ci, "run_checked") as run_checked,
                self.assertRaisesRegex(
                    common.ScriptError,
                    "OpenVMM fuzz crate query exited 2",
                ),
            ):
                ci.run_openvmm_unit_tests()

            run_checked.assert_not_called()

    def test_openvmm_tests_use_nvx_guest_artifacts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            openvmm = root / "openvmm"
            openvmm.mkdir()
            (openvmm / "Cargo.toml").touch()
            kernel = root / "vmlinux"
            initrd = root / "initramfs.cpio.gz"
            kernel.touch()
            initrd.touch()
            backend = "whp" if os.name == "nt" else "kvm"
            installed_targets = common.CommandResult(
                args=("rustup", "target", "list"),
                returncode=0,
                stdout=(
                    "\n".join(OpenVMMBuildConstants.TEST_RUST_TARGETS[backend]) + "\n"
                ).encode(),
                stderr=b"",
            )

            with (
                patch.object(OpenVMMBuildConstants, "DIRECTORY", openvmm),
                patch.object(
                    ci,
                    "artifact_path",
                    side_effect=[kernel, initrd],
                ),
                patch.object(ci.os, "access", return_value=True),
                patch.object(ci.Path, "exists", return_value=False),
                patch.object(ci, "require_tool", side_effect=["cargo", "rustup"]),
                patch.object(
                    ci,
                    "run_capture",
                    return_value=installed_targets,
                ) as run_capture,
                patch.object(ci, "run_checked") as run_checked,
                patch.dict(
                    os.environ,
                    {
                        "PETRI_CAPABILITIES": "vpci",
                        "RUNNER_TEMP": os.fspath(root),
                    },
                ),
            ):
                ci.run_openvmm_tests(backend)

            run_capture.assert_called_once_with(
                [
                    "rustup",
                    "target",
                    "list",
                    "--installed",
                    "--toolchain",
                    OpenVMMBuildConstants.RUST_TOOLCHAIN,
                ]
            )
            self.assertEqual(run_checked.call_count, 2)
            restore, tests = run_checked.call_args_list
            self.assertEqual(
                restore.args[0],
                ["cargo", "xflowey", "restore-packages", "--no-compat-igvm"],
            )
            environment = restore.kwargs["env"]
            self.assertEqual(
                environment["RUSTUP_TOOLCHAIN"],
                OpenVMMBuildConstants.RUST_TOOLCHAIN,
            )
            if os.name == "nt":
                self.assertNotIn("XDG_CACHE_HOME", environment)
            else:
                self.assertEqual(
                    environment["XDG_CACHE_HOME"],
                    os.fspath(root / "openvmm-cache"),
                )
            command = tests.args[0]
            self.assertEqual(command[:3], ["cargo", "xflowey", "vmm-tests-run"])
            filter_index = command.index("--filter")
            self.assertEqual(
                command[filter_index + 1],
                ci.OPENVMM_TEST_FILTERS[backend],
            )
            self.assertEqual(tests.kwargs["cwd"], openvmm)
            self.assertIs(tests.kwargs["env"], environment)
            self.assertEqual(
                environment["OPENVMM_MICROVM_TEST_KERNEL"],
                os.fspath(kernel.resolve()),
            )
            self.assertEqual(
                environment["OPENVMM_MICROVM_TEST_INITRD"],
                os.fspath(initrd.resolve()),
            )
            self.assertEqual(environment["PETRI_CAPABILITIES"], "vpci")
            if os.name == "nt":
                self.assertEqual(
                    command[command.index("--dir") + 1],
                    os.fspath(root / backend),
                )

    def test_openvmm_tests_pin_stable_toolchain_without_runner_temp(self):
        installed_targets = common.CommandResult(
            args=("rustup", "target", "list"),
            returncode=0,
            stdout=(
                "\n".join(OpenVMMBuildConstants.TEST_RUST_TARGETS["whp"]) + "\n"
            ).encode(),
            stderr=b"",
        )

        with (
            patch.object(ci, "run_capture", return_value=installed_targets),
            patch.object(ci, "run_checked") as run_checked,
            patch.dict(os.environ, {}, clear=True),
        ):
            environment = ci._prepare_openvmm_test_environment("whp", "rustup")

        self.assertEqual(
            environment,
            {"RUSTUP_TOOLCHAIN": OpenVMMBuildConstants.RUST_TOOLCHAIN},
        )
        run_checked.assert_not_called()

    def test_openvmm_tests_prepare_job_local_toolchain_for_missing_targets(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            openvmm = root / "openvmm"
            openvmm.mkdir()
            (openvmm / "Cargo.toml").touch()
            kernel = root / "vmlinux"
            initrd = root / "initramfs.cpio.gz"
            kernel.touch()
            initrd.touch()
            backend = "whp" if os.name == "nt" else "kvm"
            installed_targets = common.CommandResult(
                args=("rustup", "target", "list"),
                returncode=0,
                stdout=f"{OpenVMMBuildConstants.GUEST_RUST_TARGET}\n".encode(),
                stderr=b"",
            )

            with (
                patch.object(OpenVMMBuildConstants, "DIRECTORY", openvmm),
                patch.object(
                    ci,
                    "artifact_path",
                    side_effect=[kernel, initrd],
                ),
                patch.object(ci.os, "access", return_value=True),
                patch.object(ci.Path, "exists", return_value=False),
                patch.object(ci, "require_tool", side_effect=["cargo", "rustup"]),
                patch.object(ci, "run_capture", return_value=installed_targets),
                patch.object(ci, "run_checked") as run_checked,
                patch.dict(os.environ, {"RUNNER_TEMP": os.fspath(root)}),
            ):
                ci.run_openvmm_tests(backend)

            self.assertEqual(run_checked.call_count, 4)
            install_toolchain, install_targets, restore, tests = (
                run_checked.call_args_list
            )
            environment = install_toolchain.kwargs["env"]
            self.assertEqual(
                environment["RUSTUP_HOME"],
                os.fspath(root / "openvmm-rustup"),
            )
            self.assertEqual(
                environment["RUSTUP_TOOLCHAIN"],
                OpenVMMBuildConstants.RUST_TOOLCHAIN,
            )
            if os.name != "nt":
                self.assertEqual(
                    environment["XDG_CACHE_HOME"],
                    os.fspath(root / "openvmm-cache"),
                )
            self.assertEqual(
                install_toolchain.args[0],
                [
                    "rustup",
                    "toolchain",
                    "install",
                    OpenVMMBuildConstants.RUST_TOOLCHAIN,
                    "--profile",
                    "minimal",
                ],
            )
            self.assertEqual(
                install_targets.args[0],
                [
                    "rustup",
                    "target",
                    "add",
                    *OpenVMMBuildConstants.TEST_RUST_TARGETS[backend],
                    "--toolchain",
                    OpenVMMBuildConstants.RUST_TOOLCHAIN,
                ],
            )
            for command in (install_targets, restore, tests):
                self.assertIs(command.kwargs["env"], environment)

    def test_openvmm_tests_define_each_backend_filter(self):
        self.assertEqual(
            set(ci.OPENVMM_TEST_FILTERS),
            set(ci.OPENVMM_TEST_BACKENDS),
        )
        self.assertEqual(
            set(OpenVMMBuildConstants.TEST_RUST_TARGETS),
            set(ci.OPENVMM_TEST_BACKENDS),
        )
        self.assertEqual(
            ci.OPENVMM_TEST_FILTERS["kvm"],
            ci.OPENVMM_KVM_TEST_FILTER,
        )
        self.assertIn(
            "!test(no_vmbus_prepped_boot_no_vmbus_windows)",
            ci.OPENVMM_KVM_TEST_FILTER,
        )
        self.assertIn(
            "!test(windows_datacenter_core_2022_x64)",
            ci.OPENVMM_KVM_TEST_FILTER,
        )
        self.assertIn("!test(virtio_net_windows)", ci.OPENVMM_KVM_TEST_FILTER)
        self.assertEqual(
            set(ci.OPENVMM_KVM_EXCLUDED_TESTS),
            {
                "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_boot_no_agent",
                "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_iso_boot_no_agent",
                "multiarch::openvmm_pcat_x64_ubuntu_2404_server_x64_boot",
                "multiarch::openvmm_pcat_x64_ubuntu_2504_server_x64_boot",
                "multiarch::openvmm_pcat_x64_ubuntu_2504_server_x64_boot_heavy",
            },
        )
        for excluded_test in ci.OPENVMM_KVM_EXCLUDED_TESTS:
            self.assertIn(
                f"!{ci._exact_openvmm_test(excluded_test)}",
                ci.OPENVMM_KVM_TEST_FILTER,
            )
        self.assertNotIn("!test(openvmm_pcat_x64)", ci.OPENVMM_KVM_TEST_FILTER)
        self.assertNotIn(
            "openvmm_linux_x64_apicid_offset",
            ci.OPENVMM_KVM_TEST_FILTER,
        )
        self.assertNotIn(
            "openvmm_linux_x64_legacy_xapic",
            ci.OPENVMM_KVM_TEST_FILTER,
        )
        self.assertEqual(
            ci.OPENVMM_TEST_FILTERS["mshv"],
            ci.OPENVMM_MSHV_TEST_FILTER,
        )
        self.assertIn(
            "!test(windows_datacenter_core_2022_x64)",
            ci.OPENVMM_MSHV_TEST_FILTER,
        )
        self.assertEqual(
            set(ci.OPENVMM_MSHV_EXCLUDED_TESTS),
            {
                "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_boot_no_agent",
                "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_iso_boot_no_agent",
                "multiarch::openvmm_pcat_x64_ubuntu_2404_server_x64_boot",
                "multiarch::openvmm_pcat_x64_ubuntu_2504_server_x64_boot",
                "multiarch::openvmm_pcat_x64_ubuntu_2504_server_x64_boot_heavy",
                "multiarch::pcie::openvmm_linux_x64_pcie_save_restore",
            },
        )
        self.assertNotIn(
            "x86_64::openvmm_linux_x64_virtio_blk_device",
            ci.OPENVMM_MSHV_EXCLUDED_TESTS,
        )
        for excluded_test in ci.OPENVMM_MSHV_EXCLUDED_TESTS:
            self.assertIn(
                f"!{ci._exact_openvmm_test(excluded_test)}",
                ci.OPENVMM_MSHV_TEST_FILTER,
            )
        self.assertNotIn("!test(openvmm_pcat_x64)", ci.OPENVMM_MSHV_TEST_FILTER)
        for backend, test_filter in ci.OPENVMM_TEST_FILTERS.items():
            for required_test in ci.OPENVMM_REQUIRED_MICROVM_TESTS:
                with self.subTest(backend=backend, required_test=required_test):
                    self.assertIn(
                        ci._exact_openvmm_test(required_test),
                        test_filter,
                    )
        self.assertEqual(len(ci.OPENVMM_WHP_TESTS), 29)
        self.assertEqual(
            len(set(ci.OPENVMM_WHP_TESTS)),
            len(ci.OPENVMM_WHP_TESTS),
        )
        for required_test in ci.OPENVMM_REQUIRED_MICROVM_TESTS:
            self.assertIn(required_test, ci.OPENVMM_WHP_TESTS)
        # Only Linux hosts can build the Linux pipette this TTRPC test boots.
        self.assertNotIn("test_ttrpc_interface", ci.OPENVMM_TEST_FILTERS["whp"])
        for backend in ("kvm", "mshv"):
            with self.subTest(backend=backend):
                self.assertIn("test(ttrpc)", ci.OPENVMM_TEST_FILTERS[backend])
                self.assertNotIn(
                    "test_ttrpc_interface",
                    ci.OPENVMM_TEST_FILTERS[backend],
                )
        self.assertEqual(ci.OPENVMM_WHP_EXCLUDED_TESTS, ())
        self.assertNotIn(
            "multiarch::openvmm_pcat_x64_windows_datacenter_core_2022_x64_boot_heavy",
            ci.OPENVMM_WHP_EXCLUDED_TESTS,
        )
        self.assertNotIn(
            "multiarch::openvmm_pcat_x64_windows_datacenter_core_2022_x64_boot",
            ci.OPENVMM_WHP_EXCLUDED_TESTS,
        )
        self.assertNotIn(
            "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_iso_boot_no_agent",
            ci.OPENVMM_WHP_EXCLUDED_TESTS,
        )
        self.assertNotIn(
            "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_boot_no_agent",
            ci.OPENVMM_WHP_EXCLUDED_TESTS,
        )
        self.assertNotIn(
            "multiarch::pcie::openvmm_uefi_x64_windows_datacenter_core_2022_x64_pcie_nvme_boot",
            ci.OPENVMM_WHP_EXCLUDED_TESTS,
        )
        self.assertTrue(
            set(ci.OPENVMM_WHP_EXCLUDED_TESTS).issubset(ci.OPENVMM_WHP_TESTS)
        )
        self.assertEqual(
            ci.OPENVMM_TEST_FILTERS["whp"],
            ci._join_openvmm_tests(
                ci.OPENVMM_WHP_TESTS,
                ci.OPENVMM_WHP_EXCLUDED_TESTS,
            ),
        )
        self.assertEqual(
            ci._join_openvmm_tests(
                ("suite::boot", "suite::boot_heavy"),
                ("suite::boot_heavy",),
            ),
            (
                "(test(/^suite::boot$/) | test(/^suite::boot_heavy$/))"
                " & !test(/^suite::boot_heavy$/)"
            ),
        )

    def test_openvmm_tests_reject_unknown_backend(self):
        with self.assertRaisesRegex(common.ScriptError, "unsupported.*backend"):
            ci.run_openvmm_tests("unknown")


class CiConfigurationTests(unittest.TestCase):
    def test_ci_needs_reference_declared_jobs(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        jobs = set(
            re.findall(
                r"^  ([A-Za-z0-9_-]+):$",
                workflow.split("jobs:\n", 1)[1],
                re.MULTILINE,
            )
        )
        for job_name in sorted(jobs):
            job = _workflow_job(workflow, job_name)
            match = re.search(r"^    needs:(.*)$", job, re.MULTILINE)
            if match is None:
                continue
            inline = match.group(1).strip()
            if inline:
                dependencies = {name.strip() for name in inline.strip("[]").split(",")}
            else:
                dependencies = set(
                    re.findall(
                        r"^      - ([A-Za-z0-9_-]+)$",
                        job.split("    if:", 1)[0],
                        re.MULTILINE,
                    )
                )
            with self.subTest(job=job_name):
                self.assertLessEqual(
                    dependencies,
                    jobs,
                    f"undefined job dependencies: {dependencies - jobs}",
                )
        required = _workflow_job(workflow, "required-status-check")
        self.assertIn("      - aci-edge-sandboxes\n", required)

    def test_development_release_requires_successful_crate_checks(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        release_job = _workflow_job(workflow, "release")
        self.assertIn("      - aci-edge-sandboxes\n", release_job)
        predicate = release_job.split("    if:", 1)[1].split("    runs-on:", 1)[0]
        self.assertIn("needs.aci-edge-sandboxes.result == 'success' &&", predicate)
        self.assertNotIn("needs.aci-edge-sandboxes.result == 'skipped'", predicate)

    def test_required_ci_result_policy(self):
        always_successful = {"quality", "aci-edge-sandboxes", "openvmm-changes"}
        builds = set(ci.REQUIRED_CI_BUILD_JOBS)
        openvmm_tests = set(ci.REQUIRED_CI_OPENVMM_TEST_JOBS)
        openvmm_artifact_tests = set(ci.REQUIRED_CI_OPENVMM_ARTIFACT_TEST_JOBS)
        microvm_tests = set(ci.REQUIRED_CI_MICROVM_TEST_JOBS)
        artifacts = {ci.REQUIRED_CI_ARTIFACT_JOB}
        platforms = set(ci.REQUIRED_CI_PLATFORM_JOBS)
        cases = (
            ("pull_request", True, False, False, always_successful),
            (
                "pull_request",
                True,
                False,
                True,
                always_successful
                | artifacts
                | builds
                | platforms
                | {"performance-gate"},
            ),
            (
                "pull_request",
                True,
                True,
                False,
                always_successful | builds | openvmm_tests,
            ),
            (
                "pull_request",
                True,
                True,
                True,
                always_successful
                | builds
                | openvmm_tests
                | openvmm_artifact_tests
                | microvm_tests
                | artifacts
                | platforms
                | {"performance-gate"},
            ),
            ("pull_request", False, False, False, always_successful),
            (
                "pull_request",
                False,
                False,
                True,
                always_successful | artifacts,
            ),
            ("pull_request", False, True, False, always_successful),
            (
                "pull_request",
                False,
                True,
                True,
                always_successful | artifacts,
            ),
            ("push", True, False, False, always_successful),
            (
                "push",
                True,
                False,
                True,
                always_successful | artifacts | builds | platforms,
            ),
            (
                "push",
                True,
                True,
                False,
                always_successful | builds | openvmm_tests,
            ),
            (
                "push",
                True,
                True,
                True,
                always_successful
                | artifacts
                | builds
                | openvmm_tests
                | openvmm_artifact_tests
                | microvm_tests
                | platforms,
            ),
        )

        for (
            event_name,
            same_repository,
            run_tests,
            run_workloads,
            successful_jobs,
        ) in cases:
            with self.subTest(
                event_name=event_name,
                same_repository=same_repository,
                run_tests=run_tests,
                run_workloads=run_workloads,
            ):
                expected = ci.required_ci_expected_results(
                    event_name,
                    same_repository=same_repository,
                    run_tests=run_tests,
                    run_workloads=run_workloads,
                )
                self.assertEqual(
                    {job for job, result in expected.items() if result == "success"},
                    successful_jobs,
                )
                self.assertEqual(
                    ci.required_ci_failures(
                        event_name,
                        same_repository=same_repository,
                        run_tests=run_tests,
                        run_workloads=run_workloads,
                        results=expected,
                    ),
                    [],
                )

                for job, expected_result in expected.items():
                    unexpected = expected.copy()
                    unexpected[job] = (
                        "skipped" if expected_result == "success" else "success"
                    )
                    with self.subTest(job=job, actual=unexpected[job]):
                        self.assertEqual(
                            ci.required_ci_failures(
                                event_name,
                                same_repository=same_repository,
                                run_tests=run_tests,
                                run_workloads=run_workloads,
                                results=unexpected,
                            ),
                            [
                                f"{job}: expected {expected_result}, "
                                f"got {unexpected[job]}"
                            ],
                        )

                for job in expected:
                    failed = expected.copy()
                    failed[job] = "failure"
                    with self.subTest(job=job, actual="failure"):
                        self.assertEqual(
                            ci.required_ci_failures(
                                event_name,
                                same_repository=same_repository,
                                run_tests=run_tests,
                                run_workloads=run_workloads,
                                results=failed,
                            ),
                            [f"{job}: expected {expected[job]}, got failure"],
                        )

    def test_required_ci_job_uses_tested_policy(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        job = _workflow_job(workflow, "required-status-check")

        self.assertIn("python3 scripts/nvx.py check-required-ci", job)
        self.assertIn('--same-repository "${SAME_REPOSITORY}"', job)
        self.assertIn(
            "github.event.pull_request.head.repo.full_name == github.repository",
            job,
        )
        self.assertLess(
            job.index("uses: actions/checkout@v5"),
            job.index("python3 scripts/nvx.py check-required-ci"),
        )
        self.assertIn("persist-credentials: false", job)
        for environment in ci.REQUIRED_CI_RESULT_ENVIRONMENTS.values():
            with self.subTest(environment=environment):
                self.assertIn(f"          {environment}:", job)

    def test_flowey_downloads_use_retrying_curl(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "setup-curl"
            / "action.yml"
        ).read_text(encoding="utf-8")
        windows_shim = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "setup-curl"
            / "curl-shim.rs"
        ).read_text(encoding="utf-8")
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        microvm_workflow = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "workflows"
            / "run-nvx-microvm-tests.yml"
        ).read_text(encoding="utf-8")
        build_action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-openvmm"
            / "action.yml"
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
        for job_name in (
            "openvmm-vmm-tests",
            "openvmm-unit-tests",
        ):
            with self.subTest(job_name=job_name):
                self.assertEqual(
                    _workflow_job(workflow, job_name).count(
                        "uses: ./.github/actions/setup-curl"
                    ),
                    1,
                )
        self.assertEqual(
            microvm_workflow.count("uses: ./.github/actions/setup-curl"),
            1,
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
                    BuildConstants.REPO_ROOT
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
                        os.fspath(shim),
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
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        build_workflow = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "workflows"
            / "build-openvmm-binary.yml"
        ).read_text(encoding="utf-8")
        microvm_workflow = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "workflows"
            / "run-nvx-microvm-tests.yml"
        ).read_text(encoding="utf-8")
        platform_workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "run-platform.yml"
        ).read_text(encoding="utf-8")
        build_action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-openvmm"
            / "action.yml"
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
        producers = {
            "build-openvmm-linux-gnu": (
                "kvm",
                "openvmm-linux-gnu",
            ),
            "build-openvmm-linux-musl": (
                "mshv",
                "openvmm-linux-musl",
            ),
            "build-openvmm-windows-msvc": (
                "whp",
                "openvmm-windows-msvc",
            ),
        }
        self.assertEqual(
            workflow.count("uses: ./.github/workflows/build-openvmm-binary.yml"),
            len(producers),
        )
        for job_name, (backend, artifact) in producers.items():
            with self.subTest(producer=job_name):
                job = _workflow_job(workflow, job_name)
                self.assertIn(f"backend: {backend}", job)
                self.assertIn(f"artifact: {artifact}", job)
                self.assertNotIn("target:", job)
                self.assertNotIn("build-mode:", job)
                self.assertNotIn("strategy:", job)

        consumers = {
            "nvx-microvm-tests-kvm": (
                "build-openvmm-linux-gnu",
                "openvmm-linux-gnu",
                "run-nvx-microvm-tests.yml",
            ),
            "nvx-microvm-tests-mshv": (
                "build-openvmm-linux-gnu",
                "openvmm-linux-gnu",
                "run-nvx-microvm-tests.yml",
            ),
            "nvx-microvm-tests-whp": (
                "build-openvmm-windows-msvc",
                "openvmm-windows-msvc",
                "run-nvx-microvm-tests.yml",
            ),
            "platform-kvm": (
                "build-openvmm-linux-gnu",
                "openvmm-linux-gnu",
                "run-platform.yml",
            ),
            "platform-mshv": (
                "build-openvmm-linux-musl",
                "openvmm-linux-musl",
                "run-platform.yml",
            ),
            "platform-whp": (
                "build-openvmm-windows-msvc",
                "openvmm-windows-msvc",
                "run-platform.yml",
            ),
        }
        for job_name, (producer, artifact, reusable_workflow) in consumers.items():
            with self.subTest(consumer=job_name):
                job = _workflow_job(workflow, job_name)
                self.assertIn(
                    f"needs: [artifacts, {producer}, openvmm-changes]",
                    job,
                )
                self.assertIn(f"needs.{producer}.result == 'success'", job)
                self.assertIn(f"uses: ./.github/workflows/{reusable_workflow}", job)
                self.assertIn(f"openvmm-artifact: {artifact}", job)
                for unrelated_producer in producers.keys() - {producer}:
                    self.assertNotIn(unrelated_producer, job)

        self.assertIn("build/openvmm.provenance.json", build_workflow)
        self.assertIn(
            "name: ${{ inputs.artifact }}-executable",
            build_workflow,
        )
        self.assertIn(
            "name: ${{ inputs.artifact }}-provenance",
            build_workflow,
        )
        self.assertEqual(
            build_workflow.count("uses: ./.github/actions/build-openvmm"),
            1,
        )
        self.assertIn("backend: ${{ inputs.backend }}", build_workflow)
        self.assertNotIn("target: ${{ inputs.target }}", build_workflow)
        self.assertNotIn("build-mode: ${{ inputs.build-mode }}", build_workflow)
        self.assertEqual(build_workflow.count("overwrite: true"), 2)
        self.assertEqual(build_workflow.count("retention-days: 1"), 2)
        for consumer_workflow, download_count in (
            (microvm_workflow, 3),
            (platform_workflow, 2),
        ):
            self.assertIn("path: openvmm/target/release", consumer_workflow)
            self.assertIn("path: build", consumer_workflow)
            self.assertEqual(
                consumer_workflow.count("uses: actions/download-artifact@v8"),
                download_count,
            )
            self.assertIn(
                "run: chmod +x openvmm/target/release/openvmm",
                consumer_workflow,
            )
        for configuration in (
            workflow,
            build_workflow,
            microvm_workflow,
            platform_workflow,
        ):
            self.assertNotIn("path: .", configuration)
        self.assertIn("openvmm-binary-v6-${{ inputs.backend }}-", build_action)
        self.assertNotIn("openvmm-binary-v5-", build_action)
        self.assertNotIn("inputs.target", build_action)
        self.assertNotIn("inputs.build-mode", build_action)
        self.assertNotIn("rustup target add", build_action)
        self.assertNotIn("X86_64_UNKNOWN_LINUX_MUSL", build_action)
        self.assertEqual(
            build_action.count(
                'python3 scripts/nvx.py build-openvmm --backend "${{ inputs.backend }}"'
            ),
            1,
        )
        self.assertEqual(
            build_action.count(
                'python scripts\\nvx.py build-openvmm --backend "${{ inputs.backend }}"'
            ),
            1,
        )
        self.assertIn("Verify OpenVMM provenance on Linux", build_action)
        self.assertIn("Verify OpenVMM provenance on Windows", build_action)
        self.assertNotIn("nvx-microvm-tests-v1", workflow)
        self.assertNotIn("cargo-v2-", build_action)
        self.assertNotIn("openvmm-tests-v2-", workflow)
        self.assertNotIn("Restore OpenVMM test build", workflow)
        self.assertNotIn("Save OpenVMM test build", workflow)
        self.assertEqual(
            build_workflow.count("uses: ./.github/actions/sccache"),
            2,
        )
        for job_name, cache_namespace in (
            ("openvmm-vmm-tests", "vmm-tests"),
            ("openvmm-unit-tests", "unit-tests"),
        ):
            with self.subTest(job_name=job_name):
                job = _workflow_job(workflow, job_name)
                self.assertEqual(
                    job.count("uses: ./.github/actions/sccache"),
                    2,
                )
                self.assertIn(
                    f"key: openvmm-inputs-v1-${{{{ runner.os }}}}-"
                    f"${{{{ runner.arch }}}}-{cache_namespace}-"
                    "${{ steps.openvmm.outputs.sha }}",
                    job,
                )
                self.assertIn(
                    f"key: openvmm-cargo-v1-${{{{ runner.os }}}}-"
                    f"${{{{ runner.arch }}}}-{cache_namespace}-"
                    "${{ hashFiles('openvmm/Cargo.lock') }}",
                    job,
                )
                self.assertIn(
                    "restore-keys: |\n"
                    "            openvmm-inputs-v1-${{ runner.os }}-"
                    "${{ runner.arch }}-",
                    job,
                )
                self.assertIn(
                    "restore-keys: |\n"
                    "            openvmm-cargo-v1-${{ runner.os }}-"
                    "${{ runner.arch }}-",
                    job,
                )
        self.assertNotIn("uses: actions/cache@v5", workflow)
        self.assertNotIn("uses: actions/cache@v5", build_workflow)
        self.assertNotIn("uses: actions/cache@v5", build_action)

        release_job = _workflow_job(workflow, "release")
        performance_gate_job = _workflow_job(workflow, "performance-gate")
        performance_persist_job = _workflow_job(workflow, "performance-persist")
        for job_name in consumers:
            self.assertIn(f"      - {job_name}", release_job)
            self.assertIn(f"needs.{job_name}.result", release_job)
            self.assertIn(f"      - {job_name}", performance_persist_job)
            self.assertIn(f"needs.{job_name}.result", performance_persist_job)
        for job_name in ("platform-kvm", "platform-mshv", "platform-whp"):
            self.assertIn(job_name, performance_gate_job)
            self.assertIn(f"needs.{job_name}.result", performance_gate_job)

    def test_ci_runs_openvmm_tests_and_unit_tests_on_each_backend(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        vmm_tests_job = _workflow_job(workflow, "openvmm-vmm-tests")
        unit_tests_job = _workflow_job(workflow, "openvmm-unit-tests")

        self.assertIn(
            """      - name: Run OpenVMM unit tests on Linux
        if: runner.os != 'Windows'
        shell: bash
        run: python3 scripts/nvx.py test-openvmm-unit""",
            unit_tests_job,
        )
        self.assertIn(
            """      - name: Run OpenVMM unit tests on Windows
        if: runner.os == 'Windows'
        shell: powershell
        run: python scripts\\nvx.py test-openvmm-unit""",
            unit_tests_job,
        )
        self.assertEqual(
            vmm_tests_job.count(
                'scripts/nvx.py test-openvmm --backend "${{ matrix.backend }}"'
            ),
            2,
        )
        self.assertEqual(
            vmm_tests_job.count(
                'scripts\\nvx.py test-openvmm --backend "${{ matrix.backend }}"'
            ),
            1,
        )
        self.assertNotIn("test-openvmm-unit", vmm_tests_job)
        self.assertEqual(unit_tests_job.count("scripts/nvx.py test-openvmm-unit"), 1)
        self.assertEqual(
            unit_tests_job.count("scripts\\nvx.py test-openvmm-unit"),
            1,
        )
        self.assertNotIn("test-openvmm --backend", unit_tests_job)

    def test_ci_preserves_failed_openvmm_test_diagnostics(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        job = _workflow_job(workflow, "openvmm-vmm-tests")
        step_name = "      - name: Upload OpenVMM test diagnostics"
        self.assertIn(step_name, job)
        upload = job.split(step_name, 1)[1].split("\n      - name:", 1)[0]

        self.assertIn("\n        if: failure()", upload)
        self.assertIn("uses: actions/upload-artifact@v7", upload)
        self.assertIn(
            "name: openvmm-vmm-tests-${{ runner.os }}-${{ matrix.backend }}-"
            "${{ github.run_id }}-${{ github.run_attempt }}",
            upload,
        )
        self.assertIn(
            "path: ${{ runner.os == 'Windows' && "
            "format('{0}/{1}/test_results', runner.temp, matrix.backend) || "
            "'openvmm/target/vmm_tests/test_results' }}",
            upload,
        )
        self.assertIn("if-no-files-found: warn", upload)
        self.assertIn("retention-days: 7", upload)
        self.assertLess(
            job.index(step_name),
            job.index("      - name: Report sccache"),
        )

    def test_runner_setup_pins_and_validates_sccache(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        action = (
            BuildConstants.REPO_ROOT / ".github" / "actions" / "sccache" / "action.yml"
        ).read_text(encoding="utf-8")
        validate_runner = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "validate-runner"
            / "action.yml"
        ).read_text(encoding="utf-8")
        windows_setup = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-windows-whp.ps1"
        ).read_text(encoding="utf-8")
        linux_setup = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-linux-runner.sh"
        ).read_text(encoding="utf-8")

        self.assertIn('$SccacheVersion = "0.18.0"', windows_setup)
        self.assertIn(
            '$SccacheSha256 = "1a63c1be2beab3f04d27e4cc145443e092e02d3dd83a51030989829d7023091b"',
            windows_setup,
        )
        self.assertIn("SCCACHE_VERSION=0.18.0", linux_setup)
        self.assertIn(
            "SCCACHE_SHA256=45f1447fbe231e3037bde351ef70677dd212216c8d62ae7ca409fecc4d6acc89",
            linux_setup,
        )
        for configuration in (windows_setup, linux_setup):
            self.assertIn("RUSTC_WRAPPER", configuration)
            self.assertIn("CARGO_INCREMENTAL", configuration)
            self.assertIn("SCCACHE_DIR", configuration)
            self.assertIn("SCCACHE_CACHE_SIZE", configuration)
        self.assertIn("sccache --zero-stats", action)
        self.assertIn("sccache --show-stats", action)
        self.assertIn("sccache --stop-server", action)
        self.assertIn("SCCACHE_IDLE_TIMEOUT", action)
        self.assertIn("sccache --version", validate_runner)
        self.assertNotRegex(
            workflow,
            r"(?m)^\s+path: openvmm/target\s*$",
        )
        self.assertLess(
            linux_setup.index('test -f "${runner_directory}/.runner"'),
            linux_setup.index('test -w "$runner_sccache_dir"'),
        )

    def test_linux_unnamed_setup_defers_existing_runner_validation(self):
        linux_setup = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-linux-runner.sh"
        ).read_text(encoding="utf-8")
        environment_check = linux_setup.split("check_environment() {\n", 1)[1].split(
            "\n}\n", 1
        )[0]
        runner_checks = environment_check.split(
            "    validate_runner_state_paths\n    validate_runner_work_paths\n", 1
        )[1]
        guard = 'if [ "$configure_runner" = false ] && [ "$check_only" = false ]; then'
        self.assertIn(guard, runner_checks)
        self.assertLess(
            runner_checks.index(guard),
            runner_checks.index('test -w "$runner_sccache_dir"'),
        )
        if os.name != "posix":
            return

        harness = (
            "set -eu\n"
            "runner_directory=/missing-runner\n"
            "runner_sccache_dir=/missing-runner/_work/_sccache\n"
            "validate_runner_state_paths() { echo state-checked; }\n"
            "validate_runner_work_paths() { echo work-checked; }\n"
            'run_as_root() { [ "$1" = test ] && [ "$2" = -f ]; }\n'
            "check_runner_state() {\n"
            "    validate_runner_state_paths\n"
            "    validate_runner_work_paths\n"
            f"{runner_checks}\n"
            "}\n"
        )
        for configure_runner, check_only in (
            ("false", "false"),
            ("false", "true"),
            ("true", "false"),
        ):
            with self.subTest(configure_runner=configure_runner, check_only=check_only):
                result = subprocess.run(
                    [
                        "sh",
                        "-c",
                        harness + f"{configure_runner=}\n{check_only=}\n"
                        "check_runner_state\n",
                    ],
                    capture_output=True,
                    text=True,
                    timeout=10,
                    check=False,
                )
                self.assertEqual(
                    result.returncode == 0,
                    configure_runner == check_only == "false",
                    result.stderr,
                )
                if result.returncode == 0:
                    self.assertIn("state-checked", result.stdout)
                    self.assertIn("work-checked", result.stdout)

    def test_linux_setup_installs_openvmm_perl_modules(self):
        setup_directory = BuildConstants.REPO_ROOT / "scripts" / "setup"
        configurations = (
            (setup_directory / "setup-linux-runner.sh").read_text(encoding="utf-8"),
            (setup_directory / "setup-linux-mshv.sh").read_text(encoding="utf-8"),
        )

        for configuration in configurations:
            for package in (
                "perl-FindBin",
                "perl-IPC-Cmd",
                "perl-Time-Piece",
                "perl-lib",
            ):
                self.assertIn(package, configuration)

    def test_windows_runner_requires_inbox_pcat_firmware(self):
        windows_setup = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-windows-whp.ps1"
        ).read_text(encoding="utf-8")
        validate_runner = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "validate-runner"
            / "action.yml"
        ).read_text(encoding="utf-8")

        self.assertIn('"Microsoft-Hyper-V"', windows_setup)
        for configuration in (windows_setup, validate_runner):
            for firmware in (
                "vmfirmwarepcat.dll",
                "vmfirmware.dll",
                "VmEmulatedDevices.dll",
            ):
                self.assertIn(firmware, configuration)

    def test_linux_runners_require_an_invariant_tsc(self):
        validate_runner = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "validate-runner"
            / "action.yml"
        ).read_text(encoding="utf-8")
        linux_setup = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-linux-runner.sh"
        ).read_text(encoding="utf-8")

        step = validate_runner.split("    - name: Validate host TSC\n", 1)[1]
        step = step.split("\n\n    - name: ", 1)[0]
        self.assertIn("      if: runner.os != 'Windows'\n", step)
        check = "\n".join(
            line.removeprefix("        ")
            for line in step.split("      run: |\n", 1)[1].splitlines()
        )
        function = linux_setup.split("require_invariant_tsc() {\n", 1)[1]
        function = "require_invariant_tsc() {\n" + function.split("\n}\n", 1)[0]
        function += "\n}\n"
        self.assertLess(
            linux_setup.index("require_invariant_tsc /proc/cpuinfo\n"),
            linux_setup.index('sudo -n true || die "passwordless sudo is required"'),
        )
        if os.name != "posix":
            return

        with tempfile.TemporaryDirectory() as temporary:
            cpuinfo = Path(temporary) / "cpuinfo"
            for flags, invariant in (
                ("fpu tsc constant_tsc nonstop_tsc tsc_known_freq", True),
                ("fpu tsc constant_tsc tsc_known_freq", False),
                ("fpu tsc constant_tsc nonstop_tsc_x", False),
            ):
                cpuinfo.write_text(
                    f"model name\t: Test CPU\nflags\t\t: {flags}\n",
                    encoding="utf-8",
                )
                with self.subTest(flags=flags, check="validate-runner"):
                    result = subprocess.run(
                        ["bash", "-c", check.replace("/proc/cpuinfo", str(cpuinfo))],
                        capture_output=True,
                        text=True,
                        timeout=10,
                        check=False,
                    )
                    self.assertEqual(result.returncode == 0, invariant, result.stderr)
                    self.assertIn("CPU: Test CPU", result.stdout)
                    self.assertEqual(
                        "::error::Runner host does not expose an invariant TSC"
                        in result.stderr,
                        not invariant,
                    )
                with self.subTest(flags=flags, check="setup-linux-runner"):
                    result = subprocess.run(
                        [
                            "sh",
                            "-c",
                            'die() { echo "$*" >&2; exit 1; }\n'
                            f"{function}"
                            f"require_invariant_tsc '{cpuinfo}'\n",
                        ],
                        capture_output=True,
                        text=True,
                        timeout=10,
                        check=False,
                    )
                    self.assertEqual(result.returncode == 0, invariant, result.stderr)

    def test_runner_setups_install_backend_native_openvmm_targets(self):
        linux_setup = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-linux-runner.sh"
        ).read_text(encoding="utf-8")
        windows_setup = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-windows-whp.ps1"
        ).read_text(encoding="utf-8")

        for target in (
            "x86_64-unknown-none",
            "x86_64-unknown-uefi",
        ):
            self.assertIn(target, linux_setup)
            self.assertIn(target, windows_setup)
        self.assertIn("x86_64-unknown-linux-musl", linux_setup)
        for cross_platform_tool in (
            "x86_64-pc-windows-gnu",
            "gcc-mingw-w64-x86-64-win32",
            "mingw64-gcc",
            "x86_64-w64-mingw32-gcc",
            "x86_64-w64-mingw32-dlltool",
        ):
            self.assertNotIn(cross_platform_tool, linux_setup)

    def test_release_actions_use_deterministic_immutable_tooling(self):
        package_action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "package-release"
            / "action.yml"
        ).read_text(encoding="utf-8")
        publish_action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "publish-development-release"
            / "action.yml"
        ).read_text(encoding="utf-8")

        self.assertEqual(package_action.count("archive-release"), 2)
        self.assertIn(
            "python scripts\\nvx.py materialize-kernel-provenance-inputs",
            package_action,
        )
        self.assertLess(
            package_action.index(
                "python scripts\\nvx.py materialize-kernel-provenance-inputs"
            ),
            package_action.index("- name: Package Windows release"),
        )
        self.assertNotIn("git checkout-index", package_action)
        self.assertNotIn("tar -czf", package_action)
        self.assertNotIn("Compress-Archive", package_action)
        self.assertIn(
            "python3 -u scripts/publish_development_release.py",
            publish_action,
        )
        self.assertNotIn("--clobber", publish_action)

    def test_ci_artifact_actions_use_node24(self):
        configurations = "\n".join(
            path.read_text(encoding="utf-8")
            for pattern in ("*.yml", "*.yaml")
            for path in (BuildConstants.REPO_ROOT / ".github").rglob(pattern)
        )

        self.assertNotRegex(configurations, r"actions/upload-artifact@v[1-5]\b")
        self.assertNotRegex(configurations, r"actions/download-artifact@v[1-6]\b")

    def test_benchmark_diagnostics_preserve_each_workflow_attempt(self):
        action = (
            Path(__file__).parents[1]
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")
        diagnostics = _composite_action_step(action, "Upload benchmark diagnostics")
        name = (
            "benchmark-diagnostics-${{ inputs.platform }}-${{ github.run_id }}"
            "-attempt-${{ github.run_attempt }}"
        )

        self.assertIn("      if: always()", diagnostics)
        self.assertIn("      uses: actions/upload-artifact@v7", diagnostics)
        self.assertIn("        path: data/runs/${{ inputs.platform }}", diagnostics)
        self.assertIn(f"        name: {name}", diagnostics)
        self.assertIn("        if-no-files-found: error", diagnostics)
        self.assertIn("        overwrite: false", diagnostics)
        self.assertIn("        retention-days: 1", diagnostics)
        self.assertLess(
            action.index("- name: Upload benchmark diagnostics"),
            action.index("- name: Upload benchmark results"),
        )

        artifacts: dict[str, tuple[str, int]] = {}
        for attempt in (1, 2):
            for platform in (
                "linux-kvm-virtual-machine",
                "linux-mshv-virtual-machine",
                "windows-whp-virtual-machine",
            ):
                artifact = (
                    name.replace("${{ inputs.platform }}", platform)
                    .replace("${{ github.run_id }}", "35771477786")
                    .replace("${{ github.run_attempt }}", str(attempt))
                )
                self.assertNotIn("${{", artifact)
                self.assertNotIn(artifact, artifacts)
                artifacts[artifact] = (platform, attempt)
        self.assertEqual(len(artifacts), 6)

    def test_benchmark_result_handoff_keeps_run_scoped_names(self):
        actions = Path(__file__).parents[1] / ".github" / "actions"
        action = (actions / "run-benchmark" / "action.yml").read_text(encoding="utf-8")
        prepare = (actions / "prepare-performance-results" / "action.yml").read_text(
            encoding="utf-8"
        )
        results = _composite_action_step(action, "Upload benchmark results")
        name = "benchmark-${{ inputs.platform }}-${{ github.run_id }}"

        self.assertIn(f"        name: {name}", results)
        self.assertIn("        path: data/runs/${{ inputs.platform }}", results)
        self.assertIn("        overwrite: true", results)
        self.assertNotIn("github.run_attempt", results)
        self.assertNotIn("github.run_attempt", prepare)
        for platform in (
            "linux-kvm-virtual-machine",
            "linux-mshv-virtual-machine",
            "windows-whp-virtual-machine",
        ):
            with self.subTest(platform=platform):
                artifact = name.replace("${{ inputs.platform }}", platform)
                self.assertIn(f"        name: {artifact}", prepare)

    def test_windows_ci_remeasures_only_unstable_lifecycle_results(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")

        self.assertEqual(action.count("performance validate-openvmm"), 1)
        self.assertNotIn("for attempt in 1 2", action)
        self.assertEqual(action.count("foreach ($Attempt in 1, 2)"), 1)
        self.assertEqual(action.count("$ValidationStatus -ne 75"), 1)
        self.assertEqual(
            action.count("Lifecycle snapshot generation was unstable"),
            1,
        )

    def test_windows_benchmarks_use_provisioned_data_volume_scratch(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")
        setup = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-windows-whp.ps1"
        ).read_text(encoding="utf-8")
        prepare = _composite_action_step(action, "Prepare Windows benchmark scratch")
        cleanup = _composite_action_step(action, "Remove Windows benchmark scratch")

        self.assertIn('$BenchmarkScratchVariable = "NVX_BENCHMARK_SCRATCH"', setup)
        self.assertIn(
            '[Environment]::GetEnvironmentVariable("NVX_BENCHMARK_SCRATCH", "Machine")',
            prepare,
        )
        self.assertIn('"NVX_BENCHMARK_SCRATCH_DIR=$Scratch"', prepare)
        self.assertIn("::warning::NVX_BENCHMARK_SCRATCH is not provisioned", prepare)
        self.assertIn("      if: always() && runner.os == 'Windows'", cleanup)
        self.assertLess(
            action.index("- name: Remove Windows benchmark scratch"),
            action.index("- name: Upload benchmark diagnostics"),
        )
        for step in (
            "Run Windows acceptance test",
            "Run Windows performance suite",
            "Run Windows multi-vCPU shell restore",
            "Run Windows device operation rates",
        ):
            with self.subTest(step=step):
                script = _composite_action_script(action, step)
                self.assertIn(
                    '$ScratchArgs = @("--scratch-dir", $env:NVX_BENCHMARK_SCRATCH_DIR)',
                    script,
                )
                self.assertIn("@ScratchArgs", script)
                self.assertLess(
                    action.index("- name: Prepare Windows benchmark scratch"),
                    action.index(f"- name: {step}"),
                )
        self.assertIn(
            "    Configure-SccacheEnvironment\n    Configure-BenchmarkScratch\n",
            setup,
        )
        self.assertIn(
            "Assert-ServiceDirectoryAcl -Path $SccacheDirectory -Writable\n"
            "        Assert-BenchmarkScratch\n",
            setup,
        )

    @unittest.skipUnless(os.name == "nt", "requires Windows PowerShell")
    def test_windows_acceptance_forwards_benchmark_scratch(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")
        script = _composite_action_script(action, "Run Windows acceptance test")
        for name, value in (
            ("backend", "whp"),
            ("platform", "windows-whp-virtual-machine"),
            ("warmups", "1"),
            ("runs", "10"),
            ("teardown-mode", "guest-exit"),
        ):
            script = script.replace("${{ inputs." + name + " }}", value)
        stub = """
function python {
    $global:LASTEXITCODE = 0
    if ($args[1] -ne "benchmark") {
        return
    }
    $output = $args[[Array]::IndexOf($args, "--output") + 1]
    $index = [Array]::IndexOf($args, "--scratch-dir")
    $scratch = ""
    if ($index -ge 0) {
        $scratch = $args[$index + 1]
    }
    New-Item -ItemType Directory -Path (Split-Path $output) -Force | Out-Null
    Set-Content -LiteralPath $output -Encoding UTF8 -Value (
        ConvertTo-Json -Compress @{ scratch = $scratch }
    )
}
"""
        for scratch in ("", r"F:\nvx-benchmark-scratch\job"):
            with (
                self.subTest(scratch=scratch),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary).resolve()
                environment = os.environ.copy()
                environment.pop("NVX_BENCHMARK_SCRATCH_DIR", None)
                if scratch:
                    environment["NVX_BENCHMARK_SCRATCH_DIR"] = scratch
                result = subprocess.run(
                    [
                        "powershell.exe",
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        stub + script,
                    ],
                    cwd=root,
                    env=environment,
                    capture_output=True,
                    text=True,
                    timeout=30,
                    check=False,
                )

                self.assertEqual(result.returncode, 0, result.stderr)
                accepted = (
                    root
                    / "data"
                    / "runs"
                    / "windows-whp-virtual-machine"
                    / "microvm-v2"
                    / "1vcpu"
                    / "acceptance.json"
                )
                self.assertEqual(
                    json.loads(accepted.read_text(encoding="utf-8-sig")),
                    {"scratch": scratch},
                )

    @unittest.skipUnless(os.name == "nt", "requires Windows PowerShell")
    def test_windows_benchmark_scratch_steps_manage_per_job_directories(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")
        machine_lookup = (
            '[Environment]::GetEnvironmentVariable("NVX_BENCHMARK_SCRATCH", "Machine")'
        )
        prepare = _composite_action_script(
            action, "Prepare Windows benchmark scratch"
        ).replace("${{ inputs.platform }}", "windows-whp-virtual-machine")
        cleanup = _composite_action_script(action, "Remove Windows benchmark scratch")
        self.assertIn(machine_lookup, prepare)

        def run_step(script: str, environment: dict[str, str]):
            return subprocess.run(
                ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", script],
                env=environment,
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve() / "scratch"
            root.mkdir()
            stale = root / "windows-whp-virtual-machine-1-1"
            fresh = root / "windows-whp-virtual-machine-2-1"
            for directory in (stale, fresh):
                directory.mkdir()
                (directory / "memory.bin").write_bytes(b"ram")
            old = time.time() - 7 * 60 * 60
            os.utime(stale, (old, old))
            github_env = Path(temporary) / "github-env.txt"
            github_env.write_text("", encoding="utf-8")
            environment = os.environ.copy()
            environment.update(
                {
                    "GITHUB_ENV": str(github_env),
                    "GITHUB_RUN_ID": "35805840174",
                    "GITHUB_RUN_ATTEMPT": "2",
                }
            )

            result = run_step(
                prepare.replace(machine_lookup, f"'{root}'"),
                environment,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            scratch = root / "windows-whp-virtual-machine-35805840174-2"
            self.assertTrue(scratch.is_dir())
            self.assertFalse(stale.exists())
            self.assertTrue(fresh.is_dir())
            self.assertEqual(
                github_env.read_text(encoding="utf-8-sig").splitlines(),
                [f"NVX_BENCHMARK_SCRATCH_DIR={scratch}"],
            )

            locked = scratch / "locked.bin"
            with locked.open("wb"):
                result = run_step(
                    cleanup,
                    {**environment, "NVX_BENCHMARK_SCRATCH_DIR": str(scratch)},
                )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(
                "::warning::Could not remove benchmark scratch", result.stdout
            )

            result = run_step(
                cleanup,
                {**environment, "NVX_BENCHMARK_SCRATCH_DIR": str(scratch)},
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(scratch.exists())

            github_env.write_text("", encoding="utf-8")
            result = run_step(prepare.replace(machine_lookup, "''"), environment)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(
                "::warning::NVX_BENCHMARK_SCRATCH is not provisioned", result.stdout
            )
            self.assertEqual(github_env.read_text(encoding="utf-8"), "")

    @unittest.skipUnless(os.name == "nt", "requires Windows PowerShell")
    def test_windows_setup_selects_largest_data_volume_for_benchmark_scratch(self):
        setup = BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-windows-whp.ps1"
        harness = f"""
Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    '{setup}',
    [ref]$tokens,
    [ref]$errors
)
if ($errors.Count -ne 0) {{
    throw "setup script has parse errors"
}}
foreach ($name in "Test-SystemVolumePath", "Get-BenchmarkScratchDirectory") {{
    $definition = $ast.Find({{
            param($node)
            $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
            $node.Name -eq $name
        }}, $true)
    . ([scriptblock]::Create($definition.Extent.Text))
}}
$BenchmarkScratchVariable = "NVX_TEST_UNSET_{uuid.uuid4().hex}"
$BenchmarkScratchName = "nvx-benchmark-scratch"
$BenchmarkScratchDirectory = $null
$env:SystemDrive = "C:"
$Volumes = @(
    [pscustomobject]@{{ DriveLetter = [char]"C"; DriveType = "Fixed"; FileSystem = "NTFS"; Size = 900GB }},
    [pscustomobject]@{{ DriveLetter = [char]"D"; DriveType = "Fixed"; FileSystem = "NTFS"; Size = 64GB }},
    [pscustomobject]@{{ DriveLetter = $null; DriveType = "Fixed"; FileSystem = "NTFS"; Size = 2TB }},
    [pscustomobject]@{{ DriveLetter = [char]"E"; DriveType = "CD-ROM"; FileSystem = ""; Size = 1TB }},
    [pscustomobject]@{{ DriveLetter = [char]"F"; DriveType = "Fixed"; FileSystem = "NTFS"; Size = 512GB }},
    [pscustomobject]@{{ DriveLetter = [char]"G"; DriveType = "Fixed"; FileSystem = "FAT32"; Size = 1TB }}
)
function Get-Volume {{
    $script:Volumes
}}
Write-Output (Get-BenchmarkScratchDirectory)
$Volumes = @($Volumes[0])
Write-Output ("none=" + ($null -eq (Get-BenchmarkScratchDirectory)))
$BenchmarkScratchDirectory = "H:\\explicit\\scratch"
Write-Output (Get-BenchmarkScratchDirectory)
"""

        result = subprocess.run(
            ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", harness],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.splitlines(),
            [
                r"F:\nvx-benchmark-scratch",
                "none=True",
                r"H:\explicit\scratch",
            ],
        )

    @unittest.skipUnless(os.name == "nt", "requires Windows PowerShell")
    def test_windows_cli_validation_stops_at_each_failed_command(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "validate-nvx"
            / "action.yml"
        ).read_text(encoding="utf-8")
        script = _composite_action_script(action, "Validate NVX CLI on Windows")
        command_count = sum(line.startswith("python ") for line in script.splitlines())
        self.assertGreater(command_count, 0)
        stub = """
$CommandCount = 0
function python {
    $script:CommandCount += 1
    Write-Output "python-$script:CommandCount"
    $global:LASTEXITCODE = 0
    if ($script:CommandCount -eq $script:FailureAt) {
        $global:LASTEXITCODE = 37
    }
}
"""
        for failure_at in range(command_count + 1):
            with self.subTest(failure_at=failure_at):
                result = subprocess.run(
                    [
                        "powershell.exe",
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        f"$FailureAt = {failure_at}\n{stub}\n{script}",
                    ],
                    capture_output=True,
                    text=True,
                    timeout=30,
                    check=False,
                )
                self.assertEqual(
                    result.returncode, 37 if failure_at else 0, result.stderr
                )
                self.assertEqual(
                    result.stdout.splitlines(),
                    [
                        f"python-{index}"
                        for index in range(1, (failure_at or command_count) + 1)
                    ],
                )

    @unittest.skipUnless(os.name == "nt", "requires Windows PowerShell")
    def test_windows_acceptance_preserves_attempts_and_publishes_only_valid_data(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")
        script = _composite_action_script(action, "Run Windows acceptance test")
        for name, value in (
            ("backend", "whp"),
            ("platform", "windows-whp-virtual-machine"),
            ("warmups", "1"),
            ("runs", "10"),
            ("teardown-mode", "guest-exit"),
        ):
            script = script.replace("${{ inputs." + name + " }}", value)
        stub = """
$BenchmarkCount = 0
$ValidationCount = 0
function python {
    if ($args[1] -eq "benchmark") {
        $script:BenchmarkCount += 1
        $output = $args[[Array]::IndexOf($args, "--output") + 1]
        New-Item -ItemType Directory -Path (Split-Path $output) -Force | Out-Null
        Set-Content -LiteralPath $output -Encoding UTF8 -Value (
            '{"attempt":' + $script:BenchmarkCount + '}'
        )
        $global:LASTEXITCODE = $script:BenchmarkStatus
    }
    elseif ($args[1] -eq "performance") {
        $global:LASTEXITCODE = $script:ValidationStatuses[$script:ValidationCount]
        $script:ValidationCount += 1
    }
    else {
        throw "unexpected command: $args"
    }
}
"""
        for name, statuses, benchmark_status in (
            ("first-success", (0,), 0),
            ("remeasured-success", (75, 0), 0),
            ("both-unstable", (75, 75), 0),
            ("invalid-data", (1,), 0),
            ("benchmark-failure", (0,), 42),
        ):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                output = (
                    root
                    / "data"
                    / "runs"
                    / "windows-whp-virtual-machine"
                    / "microvm-v2"
                    / "1vcpu"
                )
                output.mkdir(parents=True)
                accepted = output / "acceptance.json"
                accepted.write_text('{"attempt":"stale"}', encoding="utf-8")
                setup = (
                    "$ValidationStatuses = @("
                    + ", ".join(str(status) for status in statuses)
                    + f")\n$BenchmarkStatus = {benchmark_status}\n"
                )
                result = subprocess.run(
                    [
                        "powershell.exe",
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        setup + stub + script,
                    ],
                    cwd=root,
                    capture_output=True,
                    text=True,
                    timeout=30,
                    check=False,
                )
                expected_status = benchmark_status or statuses[-1]
                self.assertEqual(result.returncode, expected_status, result.stderr)
                self.assertEqual(
                    sorted(path.name for path in output.glob("acceptance-attempt-*")),
                    [
                        f"acceptance-attempt-{index}.json"
                        for index in range(1, len(statuses) + 1)
                    ],
                )
                for index in range(1, len(statuses) + 1):
                    attempt = output / f"acceptance-attempt-{index}.json"
                    self.assertEqual(
                        json.loads(attempt.read_text(encoding="utf-8-sig")),
                        {"attempt": index},
                    )
                self.assertEqual(accepted.exists(), expected_status == 0)
                if expected_status == 0:
                    self.assertEqual(
                        json.loads(accepted.read_text(encoding="utf-8-sig")),
                        {"attempt": len(statuses)},
                    )


class BuildConstantsTests(unittest.TestCase):
    def test_constants_are_grouped_class_namespaces(self):
        for namespace in (
            BuildConstants,
            KernelBuildConstants,
            OpenVMMBuildConstants,
            AlpineBuildConstants,
            UbuntuBuildConstants,
            InitramfsBuildConstants,
            DockerBuildConstants,
            ZstdBuildConstants,
            ReleaseBuildConstants,
        ):
            with self.subTest(namespace=namespace.__name__):
                self.assertEqual(namespace.__module__, "nvx_tools.build_constants")
                self.assertTrue(
                    all(
                        name.isupper()
                        for name in vars(namespace)
                        if not name.startswith("__")
                    )
                )

    def test_build_pins_and_artifact_names_have_one_literal_owner(self):
        centralized = {
            KernelBuildConstants.VERSION,
            KernelBuildConstants.URL,
            KernelBuildConstants.SHA256,
            KernelBuildConstants.BINARY_NAME,
            KernelBuildConstants.CONFIG_NAME,
            KernelBuildConstants.PROVENANCE_NAME,
            OpenVMMBuildConstants.GNU_RUST_TARGET,
            OpenVMMBuildConstants.MUSL_RUST_TARGET,
            OpenVMMBuildConstants.WINDOWS_RUST_TARGET,
            OpenVMMBuildConstants.PROVENANCE_NAME,
            OpenVMMBuildConstants.CONTROL_CONTRACT_REVISION,
            AlpineBuildConstants.VERSION,
            AlpineBuildConstants.BRANCH,
            AlpineBuildConstants.MINIROOTFS_SHA256,
            AlpineBuildConstants.INITRAMFS_NAME,
            AlpineBuildConstants.PACKAGE_MANIFEST_NAME,
            UbuntuBuildConstants.VERSION,
            UbuntuBuildConstants.BASE_URL,
            UbuntuBuildConstants.BASE_SHA256,
            UbuntuBuildConstants.INITRAMFS_NAME,
            UbuntuBuildConstants.PACKAGE_MANIFEST_NAME,
            UbuntuBuildConstants.DISTRO_NAME,
            UbuntuBuildConstants.DISTRO_MANIFEST_NAME,
            InitramfsBuildConstants.PROVENANCE_NAME,
            ZstdBuildConstants.VERSION,
            ZstdBuildConstants.SHA256,
        }
        duplicates: list[tuple[str, int, str]] = []
        for path in (BuildConstants.REPO_ROOT / "scripts").rglob("*.py"):
            if path.name == "build_constants.py" or path.name.startswith("test_"):
                continue
            tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
            duplicates.extend(
                (path.name, node.lineno, node.value)
                for node in ast.walk(tree)
                if isinstance(node, ast.Constant)
                and isinstance(node.value, str)
                and node.value in centralized
            )
        self.assertEqual(duplicates, [])

    def test_old_module_level_constants_are_not_reexported(self):
        names = (
            "REPO_ROOT",
            "BUILD_DIR",
            "SOURCE_DIR",
            "OPENVMM_DIR",
            "DEFAULT_KERNEL_VERSION",
            "DEFAULT_KERNEL_URL",
            "DEFAULT_KERNEL_SHA256",
            "DEFAULT_ALPINE_VERSION",
            "DEFAULT_ALPINE_BRANCH",
            "DEFAULT_ALPINE_MINIROOTFS_SHA256",
            "DEFAULT_UBUNTU_VERSION",
            "DEFAULT_UBUNTU_CODENAME",
            "DEFAULT_UBUNTU_ARCHITECTURE",
            "DEFAULT_UBUNTU_BASE_URL",
            "DEFAULT_UBUNTU_BASE_SHA256",
            "UBUNTU_PACKAGE_LOCK",
            "UBUNTU_EROFS_FORMAT",
            "MICROVM_ABI_VERSION",
            "CONTROL_SESSION_PROTOCOL_VERSION",
            "CONTROL_CONTRACT_REVISION",
            "OPENVMM_PROVENANCE_NAME",
            "KERNEL_PROVENANCE_NAME",
            "INITRAMFS_PROVENANCE_NAME",
            "REQUIRED_VIRTIO_CONSOLE_CONFIG",
            "REQUIRED_SHARED_STATUS_KERNEL_CONFIG",
            "REQUIRED_SANDBOX_KERNEL_CONFIG",
            "ZSTD_VERSION",
            "ZSTD_ARCHIVE",
            "ZSTD_URL",
            "ZSTD_SHA256",
            "OPENVMM_RUST_TOOLCHAIN",
            "OPENVMM_GUEST_RUST_TARGET",
            "OPENVMM_UEFI_RUST_TARGET",
            "OPENVMM_LINUX_MUSL_RUST_TARGET",
            "OPENVMM_RUST_TARGETS",
            "APORTS_URL",
            "REPOSITORIES",
            "UBUNTU_ARCHIVE_KEYRING_URL",
            "UBUNTU_ARCHIVE_KEYRING_SHA256",
            "UBUNTU_SNAPSHOT_ARCHIVE_URL",
            "UBUNTU_SOURCE_INDEXES",
            "_UBUNTU_POCKET_SUITES",
            "_UBUNTU_COMPONENTS",
            "_LAUNCHPAD_ARCHIVE_API",
            "_LAUNCHPAD_SERIES_API",
            "_GENERATED_OUTPUT_ENTRIES",
            "_KNOWN_PACKAGE_CONTROL_ACTIONS",
            "PROJECT_SOURCE_PATHS",
            "GUEST_RELEASE_NAMES",
            "_ZIP_TIMESTAMP",
        )
        for module in (
            build,
            build_config,
            common,
            ubuntu,
            ci,
            collect_alpine_sources,
            collect_ubuntu_sources,
            release,
            benchmark,
            archive,
        ):
            with self.subTest(module=module.__name__):
                self.assertFalse(set(names).intersection(vars(module)))

    def test_guest_descriptors_and_source_manifest_share_build_pins(self):
        manifest = json.loads(
            (BuildConstants.REPO_ROOT / "SOURCE-MANIFEST.json").read_text(
                encoding="utf-8"
            )
        )
        self.assertEqual(KernelBuildConstants.VERSION, manifest["linux"]["version"])
        self.assertEqual(KernelBuildConstants.URL, manifest["linux"]["upstream_url"])
        self.assertEqual(
            KernelBuildConstants.SHA256,
            manifest["linux"]["upstream_archive_sha256"],
        )
        self.assertEqual(
            AlpineBuildConstants.MINIROOTFS_URL, manifest["alpine"]["minirootfs_url"]
        )
        self.assertEqual(
            AlpineBuildConstants.MINIROOTFS_SHA256,
            manifest["alpine"]["minirootfs_sha256"],
        )
        for descriptor in (guests.ALPINE_GUEST, guests.UBUNTU_GUEST):
            with self.subTest(guest=descriptor.name):
                pinned = manifest[descriptor.name]
                self.assertEqual(descriptor.release, pinned["version"])
                self.assertEqual(descriptor.architecture, pinned["architecture"])
                self.assertEqual(
                    f"build/{descriptor.package_manifest_name}",
                    pinned["package_manifests"][0],
                )
        self.assertEqual(
            OpenVMMBuildConstants.MICROVM_ABI_VERSION,
            manifest["openvmm"]["microvm_abi_version"],
        )
        self.assertEqual(
            OpenVMMBuildConstants.CONTROL_SESSION_PROTOCOL_VERSION,
            manifest["openvmm"]["control_session_protocol_version"],
        )
        self.assertEqual(
            OpenVMMBuildConstants.CONTROL_CONTRACT_REVISION,
            manifest["openvmm"]["control_contract_revision"],
        )

    def test_cache_defaults_are_resolved_for_each_configuration(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for configured in ("", str(root / "first"), str(root / "second")):
                with (
                    self.subTest(configured=configured),
                    patch.dict(os.environ, {"NVX_CACHE_DIR": configured}),
                ):
                    expected = (
                        Path(configured)
                        if configured
                        else BuildConstants.REPO_ROOT / ".cache"
                    ).resolve()
                    self.assertEqual(
                        build_config.KernelBuildConfig().cache_directory, expected
                    )
                    self.assertEqual(common.cache_root(), expected)
                    self.assertEqual(
                        build_config.KernelBuildConfig(
                            cache_directory=root / "explicit"
                        ).cache_directory,
                        root / "explicit",
                    )

    def test_openvmm_executable_default_is_resolved_at_configuration_time(self):
        for host, binary in (("nt", "openvmm.exe"), ("posix", "openvmm")):
            expected = OpenVMMBuildConstants.DIRECTORY / "target" / "release" / binary
            with self.subTest(host=host), patch.object(common.os, "name", host):
                self.assertEqual(common.openvmm_binary_path(), expected)
                self.assertEqual(build_config.OpenVmmBuildConfig().output, expected)

    def test_initramfs_provenance_tracks_constants_source(self):
        relative = "scripts/nvx_tools/build_constants.py"
        source = BuildConstants.REPO_ROOT / relative
        original = build.initramfs_provenance_inputs()
        records = cast(list[dict[str, str]], original["source_files"])
        self.assertEqual(
            [record for record in records if record["path"] == relative],
            [{"path": relative, "sha256": common.sha256_file(source)}],
        )

        def changed_sha256(path: Path) -> str:
            return "f" * 64 if path == source else common.sha256_file(path)

        with patch.object(build, "sha256_file", side_effect=changed_sha256):
            changed = build.initramfs_provenance_inputs()
        self.assertNotEqual(original, changed)
        self.assertEqual(original["alpine"], changed["alpine"])

    def test_constants_are_in_docker_and_release_source_inputs(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        self.assertIn("COPY scripts/ /repo/scripts/", dockerfile)
        self.assertIn("scripts", ReleaseBuildConstants.PROJECT_SOURCE_PATHS)
        self.assertTrue(
            (
                BuildConstants.REPO_ROOT
                / "scripts"
                / "nvx_tools"
                / "build_constants.py"
            ).is_file()
        )

    def test_constants_consumers_import_in_either_order(self):
        modules = (
            "nvx_tools.build_constants",
            "nvx_tools.guests",
            "nvx_tools.common",
            "nvx_tools.build_config",
            "nvx_tools.build",
            "nvx_tools.release",
            "nvx",
        )
        for ordered in (modules, tuple(reversed(modules))):
            with self.subTest(first=ordered[0]):
                result = subprocess.run(
                    [
                        sys.executable,
                        "-c",
                        "import importlib, sys; "
                        "[importlib.import_module(name) for name in sys.argv[1:]]",
                        *ordered,
                    ],
                    cwd=BuildConstants.REPO_ROOT / "scripts",
                    capture_output=True,
                    text=True,
                    timeout=30,
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stderr)


class UbuntuSourceCollectionTests(unittest.TestCase):
    def test_malformed_package_manifest_is_reported_as_script_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            manifest = Path(temporary) / "packages.json"
            manifest.write_text("{", encoding="utf-8")

            with self.assertRaises(collect_ubuntu_sources.ScriptError) as context:
                collect_ubuntu_sources._source_requirements([manifest])

        self.assertIn("cannot read Ubuntu package manifest", str(context.exception))


class BuildTests(unittest.TestCase):
    def test_build_config_owns_standard_runtime_paths(self):
        config = build_config.BuildConfig()
        alpine = config.initramfs_config("alpine")
        ubuntu_config = config.initramfs_config("ubuntu")
        distro = config.distro_layer_config()

        self.assertIsInstance(config.docker, build_config.DockerBuildConfig)
        self.assertIsInstance(config.kernel, build_config.KernelBuildConfig)
        self.assertIsInstance(config.openvmm, build_config.OpenVmmBuildConfig)
        self.assertIsInstance(alpine, build_config.InitramfsBuildConfig)
        self.assertIsInstance(ubuntu_config, build_config.InitramfsBuildConfig)
        self.assertIsInstance(distro, build_config.DistroLayerBuildConfig)
        self.assertIsNone(config.openvmm.backend)
        self.assertEqual(config.selected_guests(), ("alpine",))
        self.assertEqual(config.kernel.work, BuildConstants.BUILD_DIR / "linux")
        self.assertEqual(config.kernel.output, BuildConstants.BUILD_DIR / "vmlinux")
        self.assertEqual(
            alpine.work,
            BuildConstants.BUILD_DIR / "initramfs-alpine-work",
        )
        self.assertEqual(
            alpine.output,
            BuildConstants.BUILD_DIR / "initramfs.cpio.gz",
        )
        self.assertEqual(
            ubuntu_config.output,
            BuildConstants.BUILD_DIR / "initramfs-ubuntu.cpio.gz",
        )
        self.assertEqual(
            distro.output,
            BuildConstants.BUILD_DIR / "ubuntu-distro.erofs",
        )
        self.assertEqual(config.docker.artifact_destination, BuildConstants.BUILD_DIR)
        self.assertEqual(
            config.docker.linux_source_destination,
            BuildConstants.SOURCE_DIR / "linux",
        )

    def test_detects_openvmm_build_platform_without_runtime_devices(self):
        cases: tuple[
            tuple[
                str,
                build_config.OpenVmmBackend | None,
                build_config.OpenVmmPlatform,
                str,
                build_config.OpenVmmBuildMode,
            ],
            ...,
        ] = (
            (
                "win32",
                None,
                "windows-msvc",
                "x86_64-pc-windows-msvc",
                "native",
            ),
            (
                "win32",
                "whp",
                "windows-msvc",
                "x86_64-pc-windows-msvc",
                "native",
            ),
            (
                "linux",
                None,
                "linux-gnu",
                "x86_64-unknown-linux-gnu",
                "native",
            ),
            (
                "linux",
                "kvm",
                "linux-gnu",
                "x86_64-unknown-linux-gnu",
                "native",
            ),
            (
                "linux",
                "mshv",
                "linux-musl",
                "x86_64-unknown-linux-musl",
                "musl",
            ),
        )
        for host, backend, expected_platform, expected_target, expected_mode in cases:
            with self.subTest(host=host, backend=backend):
                with (
                    patch.object(build.sys, "platform", host),
                    patch.object(build.os, "access") as access,
                ):
                    config = build_config.OpenVmmBuildConfig()
                    platform = build.detect_openvmm_platform(backend)

                self.assertEqual(platform, expected_platform)
                self.assertEqual(config.openvmm_target(platform), expected_target)
                self.assertEqual(config.openvmm_build_mode(platform), expected_mode)
                access.assert_not_called()

    def test_rejects_unsupported_openvmm_build_platforms(self):
        cases: tuple[tuple[str, build_config.OpenVmmBackend | None], ...] = (
            ("linux", "whp"),
            ("win32", "kvm"),
            ("win32", "mshv"),
            ("darwin", None),
            ("darwin", "kvm"),
            ("darwin", "mshv"),
            ("darwin", "whp"),
        )
        for host, backend in cases:
            with self.subTest(host=host, backend=backend):
                with (
                    patch.object(build.sys, "platform", host),
                    patch.object(build.os, "access") as access,
                    self.assertRaisesRegex(
                        common.ScriptError,
                        "unsupported",
                    ),
                ):
                    build.detect_openvmm_platform(backend)
                access.assert_not_called()

    def test_linux_openvmm_builds_do_not_require_hypervisor_devices(self):
        for devices_usable in (False, True):
            for combined in (False, True):
                with (
                    self.subTest(devices_usable=devices_usable, combined=combined),
                    tempfile.TemporaryDirectory() as temporary,
                ):
                    openvmm_dir = Path(temporary) / "openvmm"
                    output = openvmm_dir / "target" / "release" / "openvmm"
                    output.parent.mkdir(parents=True)
                    output.write_bytes(b"openvmm")
                    (openvmm_dir / "Cargo.toml").touch()
                    config = build_config.OpenVmmBuildConfig(
                        skip_restore=True,
                        directory=openvmm_dir,
                        output=output,
                    )

                    with (
                        patch.object(build.sys, "platform", "linux"),
                        patch.object(
                            build.os, "access", return_value=devices_usable
                        ) as access,
                        patch.object(build, "run_checked") as run_checked,
                        patch.object(build, "record_openvmm_provenance"),
                        patch.object(build, "build_guest") as guest,
                    ):
                        if combined:
                            aggregate = build_config.BuildConfig(openvmm=config)
                            build.build_all(aggregate)
                            guest.assert_called_once_with(aggregate)
                        else:
                            build.build_openvmm(config)
                            guest.assert_not_called()

                    access.assert_not_called()
                    run_checked.assert_called_once_with(
                        [
                            "cargo",
                            "build",
                            "--release",
                            "-p",
                            "openvmm",
                            "--bin",
                            "openvmm",
                        ],
                        cwd=openvmm_dir,
                    )

    def test_openvmm_target_output_matches_build_mode(self):
        config = build_config.OpenVmmBuildConfig(directory=Path("checkout"))
        cases: tuple[tuple[build_config.OpenVmmPlatform, Path], ...] = (
            ("linux-gnu", Path("target") / "release" / "openvmm"),
            (
                "linux-musl",
                Path("target") / "x86_64-unknown-linux-musl" / "release" / "openvmm",
            ),
            ("windows-msvc", Path("target") / "release" / "openvmm.exe"),
        )
        for platform, relative_output in cases:
            with self.subTest(platform=platform):
                self.assertEqual(
                    config.openvmm_target_output(platform),
                    config.directory / relative_output,
                )

    def test_native_openvmm_build_normalizes_output_before_recording_provenance(self):
        platforms: tuple[tuple[build_config.OpenVmmPlatform, str], ...] = (
            ("linux-gnu", "openvmm"),
            ("windows-msvc", "openvmm.exe"),
        )
        for platform, executable in platforms:
            for existing_output in (False, True):
                with (
                    self.subTest(platform=platform, existing_output=existing_output),
                    tempfile.TemporaryDirectory() as temporary,
                ):
                    root = Path(temporary)
                    openvmm_dir = root / "openvmm"
                    native_output = openvmm_dir / "target" / "release" / executable
                    native_output.parent.mkdir(parents=True)
                    binary = b"newly-built-openvmm"
                    native_output.write_bytes(binary)
                    (openvmm_dir / "Cargo.toml").touch()
                    output = root / "artifacts" / executable
                    if existing_output:
                        output.parent.mkdir(parents=True)
                        output.write_bytes(b"stale-openvmm")
                    config = build_config.OpenVmmBuildConfig(
                        skip_restore=True,
                        build_directory=root / "build",
                        directory=openvmm_dir,
                        output=output,
                    )
                    revision = b"0bc357bbcf3a654b63dfb51f1103c5751bf3d31f\n"
                    results = [
                        common.CommandResult(("git",), 0, revision, b""),
                        common.CommandResult(("git",), 0, revision, b""),
                        common.CommandResult(("git",), 0, b"", b""),
                    ]

                    with (
                        patch.object(build, "run_checked"),
                        patch.object(common, "run_capture", side_effect=results),
                    ):
                        build.build_openvmm(config, platform=platform)

                    self.assertEqual(output.read_bytes(), binary)
                    provenance = json.loads(
                        (
                            config.build_directory
                            / OpenVMMBuildConstants.PROVENANCE_NAME
                        ).read_text(encoding="utf-8")
                    )
                    self.assertEqual(
                        provenance["executable_sha256"],
                        hashlib.sha256(binary).hexdigest(),
                    )

    def test_native_openvmm_build_restores_and_records_provenance(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            openvmm_dir = root / "openvmm"
            output = openvmm_dir / "target" / "release" / "openvmm"
            output.parent.mkdir(parents=True)
            output.write_bytes(b"openvmm")
            (openvmm_dir / "Cargo.toml").touch()
            config = build_config.OpenVmmBuildConfig(
                build_directory=root / "build",
                directory=openvmm_dir,
                output=output,
            )

            with (
                patch.object(build, "run_checked") as run_checked,
                patch.object(build, "record_openvmm_provenance") as provenance,
            ):
                build.build_openvmm(config, platform="linux-gnu")

            self.assertEqual(
                run_checked.call_args_list,
                [
                    call(
                        [
                            "cargo",
                            "xflowey",
                            "restore-packages",
                            "--no-compat-igvm",
                        ],
                        cwd=openvmm_dir,
                    ),
                    call(
                        [
                            "cargo",
                            "build",
                            "--release",
                            "-p",
                            "openvmm",
                            "--bin",
                            "openvmm",
                        ],
                        cwd=openvmm_dir,
                    ),
                ],
            )
            provenance.assert_called_once_with(config)

    def test_openvmm_build_honors_skip_restore(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            openvmm_dir = root / "openvmm"
            output = openvmm_dir / "target" / "release" / "openvmm"
            output.parent.mkdir(parents=True)
            output.write_bytes(b"openvmm")
            (openvmm_dir / "Cargo.toml").touch()
            config = build_config.OpenVmmBuildConfig(
                skip_restore=True,
                build_directory=root / "build",
                directory=openvmm_dir,
                output=output,
            )

            with (
                patch.object(build, "run_checked") as run_checked,
                patch.object(build, "record_openvmm_provenance"),
            ):
                build.build_openvmm(config, platform="linux-gnu")

            self.assertEqual(run_checked.call_count, 1)
            self.assertEqual(run_checked.call_args.args[0][:2], ["cargo", "build"])

    def test_musl_openvmm_build_sets_sysroot_and_normalizes_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            openvmm_dir = root / "openvmm"
            (openvmm_dir / "Cargo.toml").parent.mkdir(parents=True)
            (openvmm_dir / "Cargo.toml").touch()
            sysroot = openvmm_dir / ".packages" / "extracted" / "x86_64-sysroot"
            (sysroot / "lib").mkdir(parents=True)
            (sysroot / "lib" / "libsymcrypt.a").touch()
            output = openvmm_dir / "target" / "release" / "openvmm"
            config = build_config.OpenVmmBuildConfig(
                build_directory=root / "build",
                directory=openvmm_dir,
                output=output,
                backend="mshv",
            )
            target_output = config.openvmm_target_output("linux-musl")
            target_output.parent.mkdir(parents=True)
            target_output.write_bytes(b"musl-openvmm")

            with (
                patch.object(build.sys, "platform", "linux"),
                patch.object(build.os, "access", return_value=False) as access,
                patch.object(build, "run_checked") as run_checked,
                patch.object(build, "record_openvmm_provenance") as provenance,
                patch.object(Path, "chmod") as chmod,
            ):
                build.build_openvmm(config)

            access.assert_not_called()
            self.assertEqual(run_checked.call_count, 3)
            self.assertEqual(
                run_checked.call_args_list[1],
                call(["rustup", "target", "add", "x86_64-unknown-linux-musl"]),
            )
            cargo_call = run_checked.call_args_list[2]
            self.assertIn("x86_64-unknown-linux-musl", cargo_call.args[0])
            environment = cargo_call.kwargs["env"]
            resolved_sysroot = (
                openvmm_dir.resolve() / ".packages" / "extracted" / "x86_64-sysroot"
            )
            self.assertEqual(
                environment["X86_64_UNKNOWN_LINUX_MUSL_OPENSSL_DIR"],
                os.fspath(resolved_sysroot),
            )
            self.assertEqual(
                environment["X86_64_UNKNOWN_LINUX_MUSL_SYMCRYPT_LIB_PATH"],
                os.fspath(resolved_sysroot / "lib"),
            )
            self.assertEqual(output.read_bytes(), b"musl-openvmm")
            chmod.assert_called_once()
            provenance.assert_called_once_with(config)

    def test_guest_build_routes_one_config_to_native_or_docker_consumers(self):
        with (
            patch.object(build, "build_kernel") as kernel,
            patch.object(build, "build_initramfs") as initramfs,
            patch.object(build, "build_docker_artifacts") as docker,
        ):
            native = build_config.BuildConfig(native_guest=True)
            build.build_guest(native)
            kernel.assert_called_once_with(native.kernel)
            initramfs.assert_called_once_with(native.initramfs_config("alpine"))
            docker.assert_not_called()

        with (
            patch.object(build, "build_kernel") as kernel,
            patch.object(build, "build_initramfs") as initramfs,
            patch.object(build, "build_docker_artifacts") as docker,
        ):
            portable = build_config.BuildConfig()
            build.build_guest(portable)
            docker.assert_called_once_with(portable.docker, "alpine")
            kernel.assert_not_called()
            initramfs.assert_not_called()

        with (
            patch.object(build, "build_kernel") as kernel,
            patch.object(build, "build_initramfs") as initramfs,
            patch.object(build, "build_distro_layer") as distro,
        ):
            all_guests = build_config.BuildConfig(guest="all", native_guest=True)
            with self.assertRaisesRegex(
                common.ScriptError,
                "Azure Linux initramfs builds require Docker",
            ):
                build.build_guest(all_guests)
            kernel.assert_not_called()
            initramfs.assert_not_called()
            distro.assert_not_called()

    def test_combined_build_passes_explicit_backend_to_openvmm_build(self):
        config = build_config.BuildConfig(
            openvmm=build_config.OpenVmmBuildConfig(backend="mshv")
        )
        with (
            patch.object(build.sys, "platform", "linux"),
            patch.object(build.os, "access", return_value=False) as access,
            patch.object(build, "build_guest") as guest,
            patch.object(build, "build_openvmm") as openvmm,
        ):
            build.build_all(config)

        access.assert_not_called()
        guest.assert_called_once_with(config)
        openvmm.assert_called_once_with(config.openvmm, platform="linux-musl")

    def test_combined_build_validates_platform_before_building_guest(self):
        config = build_config.BuildConfig(
            openvmm=build_config.OpenVmmBuildConfig(backend="whp")
        )
        with (
            patch.object(build.sys, "platform", "linux"),
            patch.object(build, "build_guest") as guest,
            patch.object(build, "build_openvmm") as openvmm,
            self.assertRaisesRegex(
                common.ScriptError, "backend 'whp' is unsupported on linux"
            ),
        ):
            build.build_all(config)

        guest.assert_not_called()
        openvmm.assert_not_called()

    def test_release_source_collection_passes_docker_build_config(self):
        config = build_config.DockerBuildConfig(
            linux_source_destination=Path("custom-linux-source")
        )
        with (
            patch.object(
                release,
                "_guest_release_inputs",
                return_value=((), (), (), ()),
            ) as guest_release_inputs,
            patch.object(release, "collect_alpine_sources"),
            patch.object(release, "collect_ubuntu_sources"),
            patch.object(release, "build_docker_linux_source") as linux_source,
        ):
            release.collect_release_sources(config)

        guest_release_inputs.assert_called_once_with(include_azurelinux=False)
        linux_source.assert_called_once_with(config)

    def test_records_openvmm_revision_cleanliness_and_executable_hash(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            openvmm_dir = root / "openvmm"
            executable = openvmm_dir / "target" / "release" / "openvmm"
            executable.parent.mkdir(parents=True)
            executable.write_bytes(b"openvmm")
            revision = b"0bc357bbcf3a654b63dfb51f1103c5751bf3d31f\n"
            results = [
                common.CommandResult(("git",), 0, revision, b""),
                common.CommandResult(("git",), 0, revision, b""),
                common.CommandResult(("git",), 0, b" M src/main.rs\n", b""),
            ]

            with (
                patch.object(BuildConstants, "REPO_ROOT", root),
                patch.object(common, "run_capture", side_effect=results),
            ):
                build.record_openvmm_provenance(
                    build_config.OpenVmmBuildConfig(
                        build_directory=root / "build",
                        directory=openvmm_dir,
                        output=executable,
                    )
                )

            provenance = json.loads(
                (root / "build" / OpenVMMBuildConstants.PROVENANCE_NAME).read_text(
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
                        *KernelBuildConstants.REQUIRED_DIRECT_BOOT_CONFIG,
                        *KernelBuildConstants.REQUIRED_VIRTIO_CONSOLE_CONFIG,
                        *KernelBuildConstants.REQUIRED_SHARED_STATUS_CONFIG,
                        *KernelBuildConstants.REQUIRED_SANDBOX_CONFIG,
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
            prior_provenance = output.with_name(KernelBuildConstants.PROVENANCE_NAME)
            prior_provenance.parent.mkdir()
            prior_provenance.write_text("stale", encoding="utf-8")

            with patch.object(BuildConstants, "REPO_ROOT", root):
                source_fingerprint = build._kernel_source_fingerprint()

            def run_build(command: object, **_kwargs: object) -> None:
                if isinstance(command, list) and command[-1] == "vmlinux":
                    (work / "vmlinux").write_bytes(b"kernel")
                    input_config.write_text("CONFIG_CHANGED=y\n", encoding="utf-8")

            with (
                patch.object(BuildConstants, "REPO_ROOT", root),
                patch.object(build, "_require_linux"),
                patch.object(build, "require_tool", return_value="tool"),
                patch.object(
                    build,
                    "prepare_kernel_source",
                    return_value=(source, source_fingerprint),
                ),
                patch.object(build, "run_checked", side_effect=run_build),
                self.assertRaisesRegex(
                    common.ScriptError,
                    "inputs changed during the build",
                ),
            ):
                build.build_kernel(
                    build_config.KernelBuildConfig(
                        work=work,
                        output=output,
                    )
                )

            self.assertFalse(output.exists())
            self.assertFalse(output.with_name("vmlinux.config").exists())
            self.assertFalse(prior_provenance.exists())

    def test_materialize_kernel_provenance_inputs_bypasses_mutable_index(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            git_environment = {
                key: value
                for key, value in os.environ.items()
                if not key.startswith("GIT_CONFIG_")
            }
            config = root / "kernel" / "config-microvm"
            patch_path = root / "kernel" / "patches" / "example.patch"
            stale_patch = root / "kernel" / "patches" / "stale.patch"
            config.parent.mkdir(parents=True)
            patch_path.parent.mkdir()
            config.write_bytes(b"CONFIG_EXAMPLE=y\n")
            patch_path.write_bytes(b"patch\n")
            (root / ".gitattributes").write_text(
                "\n".join(
                    (
                        "kernel/config-microvm text eol=lf",
                        "kernel/patches/*.patch text eol=lf",
                    )
                )
                + "\n",
                encoding="ascii",
            )
            subprocess.run(
                ["git", "init", "-q"],
                cwd=root,
                env=git_environment,
                check=True,
            )
            subprocess.run(
                ["git", "config", "user.name", "NVX Tests"],
                cwd=root,
                env=git_environment,
                check=True,
            )
            subprocess.run(
                ["git", "config", "user.email", "nvx-tests@example.com"],
                cwd=root,
                env=git_environment,
                check=True,
            )
            subprocess.run(
                [
                    "git",
                    "add",
                    ".gitattributes",
                    "kernel/config-microvm",
                    "kernel/patches/example.patch",
                ],
                cwd=root,
                env=git_environment,
                check=True,
            )
            subprocess.run(
                ["git", "commit", "-qm", "kernel provenance fixture"],
                cwd=root,
                env=git_environment,
                check=True,
            )
            git_dir = Path(
                subprocess.run(
                    ["git", "rev-parse", "--git-dir"],
                    cwd=root,
                    env=git_environment,
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout.strip()
            )
            (root / git_dir / "info" / "attributes").write_text(
                "\n".join(
                    (
                        "kernel/config-microvm -text",
                        "kernel/patches/*.patch -text",
                    )
                )
                + "\n",
                encoding="ascii",
            )
            config.write_bytes(b"CONFIG_EXAMPLE=y\r\n")
            patch_path.write_bytes(b"patch\r\n")
            stale_patch.write_bytes(b"stale\r\n")
            subprocess.run(
                [
                    "git",
                    "add",
                    "kernel/config-microvm",
                    "kernel/patches/example.patch",
                    "kernel/patches/stale.patch",
                ],
                cwd=root,
                env=git_environment,
                check=True,
            )
            subprocess.run(
                [
                    "git",
                    "checkout-index",
                    "--force",
                    "--",
                    "kernel/config-microvm",
                    "kernel/patches/example.patch",
                    "kernel/patches/stale.patch",
                ],
                cwd=root,
                env=git_environment,
                check=True,
            )
            self.assertEqual(config.read_bytes(), b"CONFIG_EXAMPLE=y\r\n")
            self.assertEqual(patch_path.read_bytes(), b"patch\r\n")
            self.assertEqual(stale_patch.read_bytes(), b"stale\r\n")

            with (
                patch.dict(os.environ, git_environment, clear=True),
                patch.object(BuildConstants, "REPO_ROOT", root),
            ):
                build.materialize_kernel_provenance_inputs()

            self.assertEqual(config.read_bytes(), b"CONFIG_EXAMPLE=y\n")
            self.assertEqual(patch_path.read_bytes(), b"patch\n")
            self.assertFalse(stale_patch.exists())

    def test_manifest_tracks_every_kernel_patch(self):
        manifest = json.loads(
            (BuildConstants.REPO_ROOT / "SOURCE-MANIFEST.json").read_text(
                encoding="utf-8"
            )
        )
        self.assertEqual(
            manifest["linux"]["patches"],
            [
                path.relative_to(BuildConstants.REPO_ROOT).as_posix()
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

    def test_guest_descriptors_preserve_alpine_and_select_guest_outputs(self):
        alpine = guests.guest_descriptor("alpine")
        ubuntu_guest = guests.guest_descriptor("ubuntu")
        azurelinux_guest = guests.guest_descriptor("azurelinux")

        self.assertEqual(alpine.initramfs_name, "initramfs.cpio.gz")
        self.assertEqual(alpine.default_memory_mib, 128)
        self.assertTrue(alpine.sandbox_control)
        self.assertEqual(
            ubuntu_guest.initramfs_name,
            "initramfs-ubuntu.cpio.gz",
        )
        self.assertEqual(ubuntu_guest.default_memory_mib, 256)
        self.assertFalse(ubuntu_guest.sandbox_control)
        self.assertEqual(
            azurelinux_guest.initramfs_name,
            "initramfs-azurelinux.cpio.gz",
        )
        self.assertEqual(azurelinux_guest.default_memory_mib, 512)
        self.assertFalse(azurelinux_guest.sandbox_control)

    def test_azurelinux_manifest_and_package_lock_match_build_pins(self):
        manifest = json.loads(
            (BuildConstants.REPO_ROOT / "SOURCE-MANIFEST.json").read_text(
                encoding="utf-8"
            )
        )["azurelinux"]
        self.assertEqual(manifest["image"], AzureLinuxBuildConstants.IMAGE)
        self.assertEqual(manifest["package_lock"], "azurelinux/packages.lock.json")
        self.assertEqual(
            manifest["package_lock_sha256"],
            azurelinux.package_lock_sha256(),
        )
        packages = azurelinux.load_package_lock()
        self.assertEqual(
            {"busybox", "util-linux"} - {package["name"] for package in packages},
            set(),
        )
        for package in packages:
            self.assertTrue(
                package["url"].startswith(
                    f"{AzureLinuxBuildConstants.REPOSITORY_URL}/Packages/"
                )
            )
        attributes = (BuildConstants.REPO_ROOT / ".gitattributes").read_text(
            encoding="utf-8"
        )
        self.assertIn("azurelinux/packages.lock.json text eol=lf", attributes)

    def test_azurelinux_package_lock_rejects_unpinned_records(self):
        document = json.loads(
            azurelinux.package_lock_path().read_text(encoding="utf-8")
        )
        first = document["packages"][0]
        cases: tuple[tuple[str, dict[str, object], str], ...] = (
            ("image", {"image": "mcr.microsoft.com/azurelinux/base/core:3.0"}, "image"),
            (
                "url",
                {
                    "packages": [
                        {**first, "url": first["url"].replace("/prod/", "/preview/")},
                        *document["packages"][1:],
                    ]
                },
                "must be fetched from",
            ),
            (
                "sha256",
                {
                    "packages": [
                        {**first, "sha256": "0" * 63},
                        *document["packages"][1:],
                    ]
                },
                "invalid SHA-256",
            ),
            (
                "order",
                {"packages": list(reversed(document["packages"]))},
                "sorted by name",
            ),
        )
        for name, update, message in cases:
            with (
                self.subTest(case=name),
                tempfile.TemporaryDirectory() as temporary,
            ):
                path = Path(temporary) / "packages.lock.json"
                path.write_text(json.dumps({**document, **update}), encoding="utf-8")
                with self.assertRaisesRegex(common.ScriptError, message):
                    azurelinux.load_package_lock(path)

    def test_azurelinux_download_packages_verifies_each_locked_rpm(self):
        destination = Path("rpms")
        with patch.object(azurelinux, "download_verified") as download_verified:
            downloaded = azurelinux.download_packages(destination)

        packages = azurelinux.load_package_lock()
        self.assertEqual(
            download_verified.call_args_list,
            [
                call(
                    package["url"],
                    destination / package["url"].rsplit("/", maxsplit=1)[-1],
                    package["sha256"],
                )
                for package in packages
            ],
        )
        self.assertEqual(len(downloaded), len(packages))

    def test_ubuntu_manifest_and_package_lock_match_build_pins(self):
        manifest = json.loads(
            (BuildConstants.REPO_ROOT / "SOURCE-MANIFEST.json").read_text(
                encoding="utf-8"
            )
        )
        ubuntu_manifest = manifest["ubuntu"]
        self.assertEqual(
            ubuntu_manifest["version"],
            UbuntuBuildConstants.VERSION,
        )
        self.assertEqual(
            ubuntu_manifest["base_url"],
            UbuntuBuildConstants.BASE_URL,
        )
        self.assertEqual(
            ubuntu_manifest["base_sha256"],
            UbuntuBuildConstants.BASE_SHA256,
        )
        self.assertEqual(
            ubuntu_manifest["archive_keyring_url"],
            UbuntuBuildConstants.ARCHIVE_KEYRING_URL,
        )
        self.assertEqual(
            ubuntu_manifest["archive_keyring_sha256"],
            UbuntuBuildConstants.ARCHIVE_KEYRING_SHA256,
        )
        self.assertEqual(
            ubuntu_manifest["package_lock_sha256"],
            ubuntu.package_lock_sha256(),
        )
        self.assertEqual(
            ubuntu_manifest["package_lock_sha256"],
            "fcdd30223b96fc26e24fcf4b6763a4a2504e9cd25882e68e89fbf6bf45277ef0",
        )
        packages = ubuntu.load_package_lock()
        self.assertEqual(
            [package["name"] for package in packages],
            [
                "busybox-static",
                "iputils-ping",
                "libcap2",
                "libidn2-0",
                "libunistring5",
                "net-tools",
                "netcat-openbsd",
            ],
        )

    def test_ubuntu_package_lock_read_errors_are_actionable(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "packages.json"
            path.write_text("{", encoding="utf-8")

            with self.assertRaisesRegex(
                common.ScriptError, "failed to read Ubuntu package lock"
            ):
                ubuntu.load_package_lock(path)

    def test_ubuntu_safe_extractor_rejects_archive_symlink_escape(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive_path = root / "unsafe.tar.gz"
            with tarfile.open(archive_path, "w:gz") as archive_file:
                symlink = tarfile.TarInfo("etc")
                symlink.type = tarfile.SYMTYPE
                symlink.linkname = "../../outside"
                archive_file.addfile(symlink)
                payload = b"nameserver 192.0.2.1\n"
                resolver = tarfile.TarInfo("etc/resolv.conf")
                resolver.size = len(payload)
                archive_file.addfile(resolver, io.BytesIO(payload))

            with self.assertRaisesRegex(common.ScriptError, "escapes"):
                ubuntu.safe_extract_tar(
                    archive_path,
                    root / "extracted",
                    label="test archive",
                )
            self.assertFalse((root / "outside").exists())

    def test_ubuntu_safe_extractor_roots_absolute_symlinks(self):
        self.assertEqual(
            ubuntu._virtual_symlink_target(
                PurePosixPath("var/run"),
                "/run",
                "test archive",
            ),
            "../run",
        )
        self.assertEqual(
            ubuntu._virtual_symlink_target(
                PurePosixPath("etc/alternatives/awk"),
                "/usr/bin/mawk",
                "test archive",
            ),
            "../../usr/bin/mawk",
        )

    def test_ubuntu_safe_extractor_rejects_absolute_and_parent_paths(self):
        for member_name in ("/absolute", "../parent"):
            with self.subTest(member_name=member_name):
                with tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    archive_path = root / "unsafe.tar"
                    with tarfile.open(archive_path, "w") as archive_file:
                        payload = b"x"
                        member = tarfile.TarInfo(member_name)
                        member.size = len(payload)
                        archive_file.addfile(member, io.BytesIO(payload))
                    with self.assertRaisesRegex(
                        common.ScriptError,
                        "absolute path|path traversal",
                    ):
                        ubuntu.safe_extract_tar(
                            archive_path,
                            root / "extracted",
                            label="test archive",
                        )

    def test_ubuntu_rootfs_digest_and_erofs_uuid_are_deterministic(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first = root / "first"
            second = root / "second"
            for directory in (first, second):
                (directory / "etc").mkdir(parents=True)
                (directory / "etc" / "os-release").write_text(
                    "ID=ubuntu\n",
                    encoding="ascii",
                )
            os.utime(second / "etc" / "os-release", (1234, 1234))

            first_digest = ubuntu.rootfs_sha256(first)
            second_digest = ubuntu.rootfs_sha256(second)
            self.assertEqual(first_digest, second_digest)
            self.assertEqual(
                ubuntu.erofs_uuid(first_digest),
                ubuntu.erofs_uuid(second_digest),
            )

    def test_ubuntu_prepare_root_normalizes_root_directory_mode(self):
        with tempfile.TemporaryDirectory() as temporary:
            temporary_root = Path(temporary)
            work = temporary_root / "work"
            expected_root = work / "root"

            def extract(
                archive: Path,
                destination: Path,
                *,
                label: str,
            ) -> None:
                self.assertEqual(
                    archive.name,
                    f"ubuntu-base-{UbuntuBuildConstants.VERSION}-base-amd64.tar.gz",
                )
                self.assertEqual(label, "Ubuntu Base archive")
                status = destination / "var" / "lib" / "dpkg" / "status"
                status.parent.mkdir(parents=True)
                status.write_text("", encoding="utf-8")

            with (
                patch.object(
                    ubuntu,
                    "cache_root",
                    return_value=temporary_root / "cache",
                ),
                patch.object(ubuntu, "download_verified"),
                patch.object(ubuntu, "safe_extract_tar", side_effect=extract),
                patch.object(ubuntu, "_validate_ubuntu_identity"),
                patch.object(ubuntu, "load_package_lock", return_value=()),
                patch.object(ubuntu, "_validate_package_closure"),
                patch.object(ubuntu, "_customize_root"),
                patch.object(ubuntu.Path, "chmod", autospec=True) as chmod,
            ):
                self.assertEqual(ubuntu.prepare_root(work), expected_root)

            chmod.assert_called_once_with(expected_root, 0o755)

    def test_ubuntu_customization_installs_busybox_wget(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for relative in ("etc", "usr/bin", "usr/sbin"):
                (root / relative).mkdir(parents=True)
            with (
                patch.object(ubuntu, "_clear_directory"),
                patch.object(ubuntu, "_validate_accounts"),
                patch.object(ubuntu, "_validate_usr_merge"),
                patch.object(ubuntu, "_ensure_symlink") as ensure_symlink,
            ):
                ubuntu._customize_root(root)

            ensure_symlink.assert_any_call(root, "usr/bin/wget", "busybox")

    def test_distro_layer_refuses_existing_output_without_replace(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "ubuntu.erofs"
            output.touch()
            with (
                patch.object(build.sys, "platform", "linux"),
                self.assertRaisesRegex(common.ScriptError, "refusing to replace"),
            ):
                build.build_distro_layer(
                    build_config.DistroLayerBuildConfig(output=output)
                )

    def test_ubuntu_initramfs_manifest_binds_artifact_digest(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "initramfs-ubuntu.cpio.gz"
            output.write_bytes(b"ubuntu initramfs")
            with patch.object(
                ubuntu,
                "package_manifest",
                return_value={"format": 1, "guest": "ubuntu"},
            ):
                build._write_ubuntu_manifest(root, output, {}, "a" * 64)

            manifest = json.loads(
                output.with_name(f"{output.name}.packages.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(manifest["artifact"], output.name)
            self.assertEqual(
                manifest["artifact_sha256"],
                common.sha256_file(output),
            )
            self.assertEqual(manifest["input_sha256"], "a" * 64)

    def test_ubuntu_source_requirements_deduplicate_binary_manifests(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifests: list[Path] = []
            for name in ("initramfs.json", "layer.json"):
                path = root / name
                path.write_text(
                    json.dumps(
                        {
                            "guest": "ubuntu",
                            "release": UbuntuBuildConstants.VERSION,
                            "architecture": UbuntuBuildConstants.ARCHITECTURE,
                            "packages": [
                                {
                                    "source_name": "glibc",
                                    "source_version": "2.43-2ubuntu2.3",
                                }
                            ],
                        }
                    ),
                    encoding="utf-8",
                )
                manifests.append(path)

            requirements = collect_ubuntu_sources._source_requirements(manifests)

        self.assertEqual(
            requirements,
            (
                {
                    "source_name": "glibc",
                    "source_version": "2.43-2ubuntu2.3",
                },
            ),
        )

    def test_ubuntu_dsc_validation_matches_source_index(self):
        payload = b"source"
        digest = hashlib.sha256(payload).hexdigest()
        with tempfile.TemporaryDirectory() as temporary:
            dsc = Path(temporary) / "example_1.0.dsc"
            dsc.write_text(
                (
                    "Format: 3.0 (quilt)\n"
                    "Source: example\n"
                    "Version: 1.0\n"
                    "Checksums-Sha256:\n"
                    f" {digest} {len(payload)} example_1.0.orig.tar.xz\n"
                ),
                encoding="utf-8",
            )
            record: collect_ubuntu_sources.SourceRecord = {
                "source_name": "example",
                "source_version": "1.0",
                "directory": "pool/main/e/example",
                "index_url": "https://archive.invalid/Sources.xz",
                "index_sha256": "0" * 64,
                "index_release_url": "https://archive.invalid/InRelease",
                "index_release_sha256": "1" * 64,
                "files": [
                    {
                        "name": "example_1.0.dsc",
                        "size": dsc.stat().st_size,
                        "sha256": common.sha256_file(dsc),
                        "url": "https://archive.invalid/example_1.0.dsc",
                    },
                    {
                        "name": "example_1.0.orig.tar.xz",
                        "size": len(payload),
                        "sha256": digest,
                        "url": "https://archive.invalid/example_1.0.orig.tar.xz",
                    },
                ],
            }

            collect_ubuntu_sources._validate_dsc(dsc, record)
            record["files"][1]["sha256"] = "f" * 64
            with self.assertRaisesRegex(
                common.ScriptError,
                "do not match",
            ):
                collect_ubuntu_sources._validate_dsc(dsc, record)

    def test_ubuntu_source_index_is_authenticated_by_signed_release(self):
        dsc_payload = b"dsc"
        source_payload = b"source"
        sources = (
            "Package: example\n"
            "Version: 1.0\n"
            "Directory: pool/main/e/example\n"
            "Checksums-Sha256:\n"
            f" {hashlib.sha256(dsc_payload).hexdigest()} "
            f"{len(dsc_payload)} example_1.0.dsc\n"
            f" {hashlib.sha256(source_payload).hexdigest()} "
            f"{len(source_payload)} example_1.0.orig.tar.xz\n"
        ).encode()
        compressed = lzma.compress(sources)
        index_sha256 = hashlib.sha256(compressed).hexdigest()
        inrelease = (
            "-----BEGIN PGP SIGNED MESSAGE-----\n"
            "Hash: SHA256\n"
            "\n"
            "Origin: Ubuntu\n"
            f"Codename: {UbuntuBuildConstants.CODENAME}\n"
            f"Suite: {UbuntuBuildConstants.CODENAME}\n"
            "SHA256:\n"
            f" {index_sha256} {len(compressed)} main/source/Sources.xz\n"
            "-----BEGIN PGP SIGNATURE-----\n"
            "test\n"
            "-----END PGP SIGNATURE-----\n"
        )

        def download_release(_url: str, destination: Path) -> None:
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text(inrelease, encoding="utf-8")

        def download_index(
            _url: str,
            destination: Path,
            expected_sha256: str,
        ) -> None:
            self.assertEqual(expected_sha256, index_sha256)
            destination.write_bytes(compressed)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            keyring = root / "ubuntu-archive-keyring.gpg"
            keyring.write_bytes(b"keyring")
            with (
                patch.object(
                    collect_ubuntu_sources,
                    "download",
                    side_effect=download_release,
                ),
                patch.object(
                    collect_ubuntu_sources,
                    "download_verified",
                    side_effect=download_index,
                ),
                patch.object(
                    collect_ubuntu_sources,
                    "require_tool",
                    return_value="gpgv",
                ),
                patch.object(collect_ubuntu_sources, "run_checked") as run_checked,
            ):
                records, metadata = (
                    collect_ubuntu_sources._load_authenticated_source_index(
                        root / "cache",
                        keyring,
                        "https://archive.invalid/ubuntu",
                        UbuntuBuildConstants.CODENAME,
                        "main",
                    )
                )

        run_checked.assert_called_once()
        self.assertEqual(run_checked.call_args.args[0][0], "gpgv")
        self.assertIn("--homedir", run_checked.call_args.args[0])
        record = records[("example", "1.0")]
        self.assertEqual(
            record["index_release_url"],
            "https://archive.invalid/ubuntu/dists/resolute/InRelease",
        )
        self.assertEqual(
            [item["role"] for item in metadata],
            ["ubuntu-inrelease", "ubuntu-source-index"],
        )
        self.assertEqual(
            metadata[1]["authenticated_by"],
            record["index_release_url"],
        )

    def test_launchpad_source_record_retains_verifiable_raw_metadata(self):
        source_payload = b"source archive"
        source_sha256 = hashlib.sha256(source_payload).hexdigest()
        self_link = f"{UbuntuBuildConstants.LAUNCHPAD_ARCHIVE_API}/+sourcepub/123"
        query = {
            "entries": [
                {
                    "source_package_name": "example",
                    "source_package_version": "1.0",
                    "status": "Superseded",
                    "date_published": "2026-01-01T00:00:00Z",
                    "date_superseded": "2026-01-03T00:00:00Z",
                    "pocket": "Updates",
                    "component_name": "main",
                    "self_link": self_link,
                }
            ]
        }
        source_urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+sourcefiles/"
            "example/1.0/example_1.0.dsc",
            "https://launchpad.net/ubuntu/+archive/primary/+sourcefiles/"
            "example/1.0/example_1.0.orig.tar.xz",
        ]

        def download_json(url: str, destination: Path) -> object:
            document: object = source_urls if "sourceFileUrls" in url else query
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text(
                json.dumps(document, separators=(",", ":")) + "\n",
                encoding="utf-8",
            )
            return document

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            snapshot_release = root / "cache" / "snapshot.InRelease"
            snapshot_release.parent.mkdir(parents=True)
            snapshot_release.write_text("signed release", encoding="utf-8")
            snapshot_record: collect_ubuntu_sources.SourceRecord = {
                "source_name": "example",
                "source_version": "1.0",
                "directory": "pool/main/e/example",
                "index_url": (
                    "https://snapshot.ubuntu.com/ubuntu/20260102T000000Z/"
                    "dists/resolute-updates/main/source/Sources.xz"
                ),
                "index_sha256": "2" * 64,
                "index_release_url": (
                    "https://snapshot.ubuntu.com/ubuntu/20260102T000000Z/"
                    "dists/resolute-updates/InRelease"
                ),
                "index_release_sha256": common.sha256_file(snapshot_release),
                "files": [
                    {
                        "name": "example_1.0.dsc",
                        "size": 3,
                        "sha256": "4" * 64,
                        "url": (
                            "https://snapshot.ubuntu.com/ubuntu/"
                            "20260102T000000Z/pool/main/e/example/"
                            "example_1.0.dsc"
                        ),
                    },
                    {
                        "name": "example_1.0.orig.tar.xz",
                        "size": len(source_payload),
                        "sha256": source_sha256,
                        "url": (
                            "https://snapshot.ubuntu.com/ubuntu/"
                            "20260102T000000Z/pool/main/e/example/"
                            "example_1.0.orig.tar.xz"
                        ),
                    },
                ],
            }
            snapshot_metadata: list[collect_ubuntu_sources.SourceMetadata] = [
                {
                    "role": "ubuntu-inrelease",
                    "url": snapshot_record["index_release_url"],
                    "sha256": snapshot_record["index_release_sha256"],
                    "cache_path": snapshot_release,
                    "output_name": "snapshot.InRelease",
                    "authenticated_by": (UbuntuBuildConstants.ARCHIVE_KEYRING_URL),
                },
                {
                    "role": "ubuntu-source-index",
                    "url": snapshot_record["index_url"],
                    "sha256": snapshot_record["index_sha256"],
                    "cache_path": None,
                    "output_name": None,
                    "authenticated_by": snapshot_record["index_release_url"],
                },
            ]
            with (
                patch.object(
                    collect_ubuntu_sources,
                    "_download_json",
                    side_effect=download_json,
                ),
                patch.object(
                    collect_ubuntu_sources,
                    "_load_authenticated_source_index",
                    return_value=(
                        {("example", "1.0"): snapshot_record},
                        snapshot_metadata,
                    ),
                ),
            ):
                record, metadata = collect_ubuntu_sources._launchpad_source_record(
                    root / "cache",
                    root / "ubuntu-archive-keyring.gpg",
                    "example",
                    "1.0",
                )

            self.assertEqual(record, snapshot_record)
            self.assertEqual(
                [item["role"] for item in metadata],
                [
                    "ubuntu-inrelease",
                    "ubuntu-source-index",
                    "launchpad-publishing-history",
                    "launchpad-source-file-urls",
                ],
            )
            for item in metadata:
                if item["cache_path"] is None:
                    continue
                self.assertEqual(
                    item["sha256"],
                    common.sha256_file(item["cache_path"]),
                )

            output = root / "output"
            output.mkdir()
            manifest_metadata = collect_ubuntu_sources._materialize_source_metadata(
                output,
                metadata,
            )
            for item, manifest_item in zip(metadata, manifest_metadata, strict=True):
                if item["cache_path"] is None:
                    self.assertNotIn("path", manifest_item)
                    continue
                retained = output / manifest_item["path"]
                self.assertEqual(
                    retained.read_bytes(),
                    item["cache_path"].read_bytes(),
                )
                self.assertEqual(
                    common.sha256_file(retained),
                    manifest_item["sha256"],
                )

    def test_ubuntu_source_collection_rejects_unexpected_output_entries(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "output"
            output.mkdir()
            stale = output / "unrelated.txt"
            stale.write_text("do not archive", encoding="utf-8")

            with self.assertRaisesRegex(
                common.ScriptError,
                "unexpected entries: unrelated.txt",
            ):
                collect_ubuntu_sources.collect_ubuntu_sources(
                    [],
                    output,
                    root / "cache",
                )

            self.assertEqual(stale.read_text(encoding="utf-8"), "do not archive")

    def test_ubuntu_source_collection_validates_output_before_resolving(self):
        output = MagicMock(spec=Path)
        with patch.object(
            collect_ubuntu_sources,
            "_validate_source_output",
        ) as validate:

            def resolve() -> Path:
                validate.assert_called_once_with(output)
                raise RuntimeError("stop after ordering assertion")

            output.resolve.side_effect = resolve
            with self.assertRaisesRegex(RuntimeError, "ordering assertion"):
                collect_ubuntu_sources.collect_ubuntu_sources(
                    [],
                    output,
                    Path("cache"),
                )

    def test_ci_kernel_cache_key_includes_patches(self):
        action = (
            BuildConstants.REPO_ROOT
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
            "scripts/nvx_tools/common.py",
        ):
            self.assertIn(cache_input, action)
        self.assertIn("linux-kernel-v1-", action)
        self.assertEqual(action.count("build/vmlinux.provenance.json"), 2)
        self.assertEqual(action.count("build/initramfs.provenance.json"), 2)

    def test_ci_guest_cache_keys_include_build_configuration(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        ).read_text(encoding="utf-8")
        for cache_name in (
            "KERNEL_INPUT_HASH",
            "ALPINE_INPUT_HASH",
            "UBUNTU_INPUT_HASH",
            "AZURELINUX_INPUT_HASH",
        ):
            with self.subTest(cache=cache_name):
                cache_input = next(
                    line
                    for line in action.splitlines()
                    if line.strip().startswith(f"{cache_name}:")
                )
                self.assertIn("'scripts/nvx_tools/build_config.py'", cache_input)
                self.assertIn("'scripts/nvx_tools/build_constants.py'", cache_input)

    def test_ci_azurelinux_cache_key_covers_build_input_digest(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        ).read_text(encoding="utf-8")
        cache_input = next(
            line
            for line in action.splitlines()
            if line.strip().startswith("AZURELINUX_INPUT_HASH:")
        )
        patterns = cache_input.split("'")[1::2]
        self.assertIn("scripts/nvx_tools/azurelinux.py", patterns)
        for path in azurelinux.input_files():
            relative = path.relative_to(BuildConstants.REPO_ROOT).as_posix()
            with self.subTest(path=relative):
                self.assertTrue(
                    any(
                        relative == pattern
                        or (
                            pattern.endswith("/**")
                            and relative.startswith(pattern[:-2])
                        )
                        for pattern in patterns
                    )
                )

    def test_kernel_input_config_uses_canonical_lf_line_endings(self):
        attributes = (BuildConstants.REPO_ROOT / ".gitattributes").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            "kernel/config-microvm text eol=lf",
            attributes.splitlines(),
        )

    def test_ci_guest_cache_keys_include_shared_build_modules(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        ).read_text(encoding="utf-8")
        for variable in ("ALPINE_INPUT_HASH", "UBUNTU_INPUT_HASH"):
            assignment = next(line for line in action.splitlines() if variable in line)
            self.assertIn("'scripts/nvx_tools/common.py'", assignment)
            self.assertIn("'scripts/nvx_tools/guests.py'", assignment)

    def test_quality_checks_include_shared_identity_probe(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "check-quality"
            / "action.yml"
        ).read_text(encoding="utf-8")

        self.assertEqual(action.count("guest/common/nvx-identity-probe"), 2)

    def test_docker_guest_builder_pins_erofs_toolchain(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            "ARG DEBIAN_IMAGE=debian:12-slim@sha256:"
            "3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251",
            dockerfile,
        )
        self.assertIn("ARG EROFS_UTILS_VERSION=1.5-1", dockerfile)
        self.assertIn("erofs-utils=${EROFS_UTILS_VERSION}", dockerfile)

    def test_azurelinux_initramfs_uses_locked_busybox_applets(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        base_stage = dockerfile.split("FROM base AS kernel", 1)[0]
        self.assertNotIn("busybox", base_stage)
        azure_stage = dockerfile.split("FROM base AS azurelinux-initramfs", 1)[1]
        self.assertIn("ln -sf ../sbin/busybox /rootfs/usr/bin/busybox", azure_stage)
        applets = (
            azure_stage.split("for utility in ", 1)[1]
            .split("; do", 1)[0]
            .replace("\\\n", " ")
            .split()
        )
        for applet in ("sh", "arp", "nc", "wget", "mdev", "ifconfig", "route"):
            self.assertIn(applet, applets)
        # util-linux provides these; the BusyBox applets lack required options.
        self.assertNotIn("setpriv", applets)
        self.assertNotIn("unshare", applets)
        launcher = (
            BuildConstants.REPO_ROOT
            / "guest"
            / "common"
            / "nvx-container-launch-azurelinux"
        ).read_text(encoding="utf-8")
        self.assertIn("exec unshare --mount --pid --uts --fork --kill-child", launcher)
        self.assertIn(
            "busybox", [package["name"] for package in azurelinux.load_package_lock()]
        )

    def test_azurelinux_initramfs_gives_nobody_a_home(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        azure_stage = dockerfile.split("FROM base AS azurelinux-initramfs", 1)[1]
        self.assertIn(
            "sed -i 's#^\\(nobody:[^:]*:65534:65534:[^:]*:\\)/dev/null:"
            "#\\1/nonexistent:#'",
            azure_stage,
        )
        self.assertIn(
            "grep -q '^nobody:[^:]*:65534:65534:[^:]*:/nonexistent:' "
            "/rootfs/etc/passwd",
            azure_stage,
        )
        self.assertIn("install -d -m 0755 /rootfs/nonexistent", azure_stage)

    def test_azurelinux_rootfs_installs_only_locked_rpms(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("tdnf install", dockerfile)
        packages_stage = dockerfile.split("FROM base AS azurelinux-packages", 1)[1]
        self.assertIn(
            "from nvx_tools.azurelinux import download_packages",
            packages_stage.split("\nFROM ", 1)[0],
        )
        rootfs_stage = dockerfile.split(
            "FROM ${AZURELINUX_IMAGE} AS azurelinux-rootfs", 1
        )[1].split("\nFROM ", 1)[0]
        self.assertIn(
            "COPY --from=azurelinux-packages /out/rpms/ /tmp/azurelinux-rpms/",
            rootfs_stage,
        )
        checksig = rootfs_stage.index("rpm --checksig /tmp/azurelinux-rpms/*.rpm")
        install = rootfs_stage.index("rpm --upgrade --verbose --hash")
        self.assertLess(checksig, install)

    def test_azurelinux_manifest_uses_package_metadata(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        self.assertIn("rpm -qa --queryformat", dockerfile)
        self.assertIn("if name == 'gpg-pubkey':", dockerfile)
        self.assertNotIn("'type': 'deb'", dockerfile)
        self.assertIn("'packages': json.loads", dockerfile)

    def test_azurelinux_initramfs_packs_normalized_rootfs(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        azure_stage = dockerfile.split("FROM base AS azurelinux-initramfs", 1)[1]
        cleanup = azure_stage.index(
            "rm -rf /rootfs/usr/lib/sysimage/tdnf /rootfs/var/cache/tdnf \\\n"
            "    /rootfs/var/cache/ldconfig /rootfs/var/lib/rpm"
        )
        normalize = azure_stage.index(
            "find . -exec touch --no-dereference "
            f"--date=@{InitramfsBuildConstants.TIMESTAMP} {{}} +"
        )
        pack = azure_stage.index("find . -print0 | LC_ALL=C sort -z | cpio")
        self.assertLess(cleanup, normalize)
        self.assertLess(normalize, pack)

    def test_azurelinux_manifest_records_shared_input_digest(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        azure_stage = dockerfile.split("FROM base AS azurelinux-initramfs", 1)[1]
        self.assertIn("from nvx_tools.azurelinux import input_sha256", azure_stage)
        self.assertIn(
            "'input_sha256': input_sha256(\n"
            "        image=os.environ['AZURELINUX_IMAGE'],\n"
            "        version=os.environ['AZURELINUX_VERSION'],\n"
            "    ),",
            azure_stage,
        )

    def test_azurelinux_input_digest_tracks_guest_sources_and_dockerfile(self):
        checkout_digest = azurelinux.input_sha256()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for source in azurelinux.input_files():
                destination = root / source.relative_to(BuildConstants.REPO_ROOT)
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(source, destination)
            with patch.object(BuildConstants, "REPO_ROOT", root):
                baseline = azurelinux.input_sha256()
                self.assertEqual(baseline, checkout_digest)
                self.assertNotEqual(
                    azurelinux.input_sha256(image="example.invalid/other@sha256:0"),
                    baseline,
                )
                self.assertNotEqual(azurelinux.input_sha256(version="3.1"), baseline)
                for relative in (
                    "guest/common/init",
                    "docker/Dockerfile",
                    "azurelinux/packages.lock.json",
                    "scripts/nvx_tools/azurelinux.py",
                    "scripts/nvx_tools/build_constants.py",
                    "scripts/nvx_tools/common.py",
                ):
                    with self.subTest(path=relative):
                        path = root / relative
                        original = path.read_bytes()
                        path.write_bytes(original + b"\n# changed\n")
                        self.assertNotEqual(azurelinux.input_sha256(), baseline)
                        path.write_bytes(original)
                        self.assertEqual(azurelinux.input_sha256(), baseline)
                unrelated = root / "guest" / "ubuntu" / "nvx-bashrc"
                unrelated.parent.mkdir(parents=True, exist_ok=True)
                unrelated.write_text("changed\n", encoding="utf-8")
                self.assertEqual(azurelinux.input_sha256(), baseline)

    def test_guest_init_runs_virtfs_helper_from_installed_path(self):
        installed: list[Path] = []

        def install(_source: Path, destination: Path) -> dict[str, str]:
            installed.append(destination)
            return {}

        root = Path("rootfs")
        with (
            patch.object(build, "_install", side_effect=install),
            patch.object(build, "_build_static_helper", return_value={}),
            patch.object(build, "_build_device_io_helper", return_value={}),
        ):
            build._install_guest_files(
                build_config.InitramfsBuildConfig(work=Path("work")),
                root,
                guests.ALPINE_GUEST,
            )
        self.assertIn(root / "sbin" / "nvx-hostmount", installed)

        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        azure_stage = dockerfile.split("FROM base AS azurelinux-initramfs", 1)[1]
        azure_sbin_scripts = azure_stage.split(
            "install -m 0755 /repo/guest/common/nvx-exit", 1
        )[1].split("/rootfs/sbin/", 1)[0]
        self.assertIn("/repo/guest/common/nvx-hostmount", azure_sbin_scripts)
        init_script = (
            BuildConstants.REPO_ROOT / "guest" / "common" / "init"
        ).read_text(encoding="utf-8")
        self.assertIn(
            '/sbin/nvx-hostmount || fatal "virtfs: failed to mount live host directory"',
            init_script,
        )
        self.assertNotIn("/usr/sbin/nvx-hostmount", init_script)

    def test_openvmm_ci_downloads_guest_artifacts(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        openvmm_tests = _workflow_job(workflow, "openvmm-vmm-tests")
        self.assertIn("needs: [artifacts, openvmm-changes]", openvmm_tests)
        self.assertIn("needs.artifacts.result == 'success'", openvmm_tests)
        self.assertIn("- name: Download guest artifacts", openvmm_tests)
        self.assertIn("name: guest-artifacts", openvmm_tests)
        self.assertIn("path: build", openvmm_tests)
        self.assertIn("openvmm-vmm-tests", ci.REQUIRED_CI_OPENVMM_ARTIFACT_TEST_JOBS)

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

    def test_device_io_provenance_uses_shared_helper_source(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            checkout = root / "nvx"
            source = checkout / "guest" / "common" / "nvx-device-io.c"
            source.parent.mkdir(parents=True)
            source.write_bytes(b"shared helper source")
            initrd = root / "initramfs.cpio.gz"
            manifest = initrd.with_name(f"{initrd.name}.packages.json")
            manifest.write_text(
                json.dumps(
                    {
                        "helpers": {
                            "nvx-device-io": {
                                "source_sha256": common.sha256_file(source),
                                "binary_sha256": "1" * 64,
                            }
                        }
                    }
                ),
                encoding="utf-8",
            )

            provenance = benchmark._device_io_helper_provenance(
                argparse.Namespace(nvx_dir=checkout),
                initrd,
            )

            self.assertEqual(provenance["source"], str(source.resolve()))
            self.assertEqual(
                provenance["source_sha256"],
                common.sha256_file(source),
            )

    def test_sandbox_kernel_config_requires_every_feature(self):
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / ".config"
            config.write_text(
                "\n".join(KernelBuildConstants.REQUIRED_SANDBOX_CONFIG) + "\n",
                encoding="utf-8",
            )
            build._assert_sandbox_kernel_config(config)

            for missing in KernelBuildConstants.REQUIRED_SANDBOX_CONFIG:
                with self.subTest(missing=missing):
                    config.write_text(
                        "\n".join(
                            setting
                            for setting in KernelBuildConstants.REQUIRED_SANDBOX_CONFIG
                            if setting != missing
                        )
                        + "\n",
                        encoding="utf-8",
                    )
                    with self.assertRaisesRegex(common.ScriptError, missing):
                        build._assert_sandbox_kernel_config(config)

    def test_checked_in_config_preserves_generic_sandbox_capabilities(self):
        config = BuildConstants.REPO_ROOT / "kernel" / "config-microvm"
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
                "\n".join(KernelBuildConstants.REQUIRED_SHARED_STATUS_CONFIG) + "\n",
                encoding="utf-8",
            )
            build._assert_shared_status_kernel_config(config)

            config.write_text("", encoding="utf-8")
            with self.assertRaisesRegex(
                common.ScriptError,
                KernelBuildConstants.REQUIRED_SHARED_STATUS_CONFIG[0],
            ):
                build._assert_shared_status_kernel_config(config)


def _posix_shell() -> str | None:
    shell = shutil.which("sh")
    if shell is None:
        git = shutil.which("git")
        git_shell = Path(git).parent.parent / "bin" / "sh.exe" if git else None
        if git_shell is not None and git_shell.is_file():
            shell = str(git_shell)
    return shell


def _shell_function(source: str, name: str) -> str:
    start = source.index(f"\n{name}() {{\n") + 1
    return source[start : source.index("\n}\n", start) + 3]


class SandboxShareAgentTests(unittest.TestCase):
    AGENT = Path(__file__).parents[1] / "guest" / "common" / "nvx-init-agent"

    def setUp(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        self.shell = shell
        self.source = self.AGENT.read_text(encoding="utf-8")
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.rootfs = self.root / "rootfs"
        self.rootfs.mkdir()

    def _run(self, cmdline: str) -> tuple[subprocess.CompletedProcess[str], str]:
        mount_log = self.root / "mount.log"
        mount_log.unlink(missing_ok=True)
        functions = "".join(
            _shell_function(self.source, name)
            for name in ("cmdline_value", "validate_share_target", "mount_live_share")
        )
        script = (
            "set -eu\n"
            "rootfs=$1\ncmdline=$2\nmount_log=$3\nshare_mountpoint=\n"
            'fatal() { echo "FATAL: $*" >&2; exit 125; }\n'
            'mount() { printf "%s\\n" "$*" >>"$mount_log"; }\n'
            f"{functions}"
            "mount_live_share\n"
            'echo "mountpoint=$share_mountpoint"\n'
        )
        result = subprocess.run(
            [
                self.shell,
                "-s",
                "--",
                self.rootfs.as_posix(),
                cmdline,
                mount_log.as_posix(),
            ],
            input=script,
            text=True,
            capture_output=True,
            check=False,
        )
        log = mount_log.read_text(encoding="utf-8") if mount_log.exists() else ""
        return result, log

    def _run_teardown(
        self, script: str, *, share: bool, failing: str = ""
    ) -> tuple[subprocess.CompletedProcess[str], list[str]]:
        runtime = self.root / "run"
        runtime.mkdir(exist_ok=True)
        for name in ("container.pid", "workload-machine-id"):
            (runtime / name).write_text("x\n", encoding="utf-8")
        log = self.root / "teardown.log"
        log.unlink(missing_ok=True)
        functions = "".join(
            _shell_function(self.source, name)
            for name in ("unmount_live_share", "fatal", "teardown")
        ).replace("/sbin/nvx-exit", "nvx_exit")
        result = subprocess.run(
            [
                self.shell,
                "-s",
                "--",
                runtime.as_posix(),
                log.as_posix(),
                f"{self.rootfs.as_posix()}/workspace" if share else "",
                failing,
            ],
            input=(
                "set -eu\n"
                "runtime=$1\nlog=$2\nshare_mountpoint=$3\nfailing=$4\n"
                "rootfs=$runtime/rootfs\nlayers=$runtime/layers\n"
                "scratch=$runtime/scratch\n"
                'umount() { printf "umount %s\\n" "$1" >>"$log"; '
                '[ "$1" != "$failing" ]; }\n'
                'mountpoint() { [ "$2" = "$layers/distro" ]; }\n'
                'nvx_exit() { printf "exit %s\\n" "$1" >>"$log"; exit "$1"; }\n'
                f"{functions}{script}\n"
            ),
            text=True,
            capture_output=True,
            check=False,
        )
        lines = log.read_text(encoding="utf-8").splitlines() if log.exists() else []
        return result, lines

    def test_agent_mounts_share_after_lifecycle_checks_for_both_lifecycles(self):
        mount_call = self.source.index("\nmount_live_share\n")
        self.assertLess(
            self.source.index('fatal "configured workload home is unavailable"'),
            mount_call,
        )
        self.assertLess(
            self.source.index('fatal "unsupported workload lifecycle'), mount_call
        )
        machine_id = self.source.index('>"$runtime/workload-machine-id"')
        managed_agent = self.source.index("\n    /sbin/nvx-managed-agent \\\n")
        self.assertLess(mount_call, machine_id)
        self.assertLess(machine_id, managed_agent)
        self.assertLess(managed_agent, self.source.index("/sbin/nvx-container-launch"))
        self.assertNotIn("exec /sbin/nvx-managed-agent", self.source)
        self.assertEqual(self.source.count('\nteardown "$status"\n'), 1)
        self.assertEqual(self.source.count('\n    teardown "$status"\n'), 1)
        self.assertLess(managed_agent, self.source.index('\n    teardown "$status"\n'))

    def test_agent_teardown_unmounts_share_before_overlay_layers_and_scratch(self):
        result, log = self._run_teardown("teardown 7", share=True)
        runtime = (self.root / "run").as_posix()
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(
            log,
            [
                f"umount {self.rootfs.as_posix()}/workspace",
                f"umount {runtime}/rootfs",
                f"umount {runtime}/layers/distro",
                f"umount {runtime}/scratch",
                "exit 7",
            ],
        )
        self.assertIn("NVX-SANDBOX-EXIT: status=7", result.stdout)
        self.assertFalse((self.root / "run" / "workload-machine-id").exists())
        self.assertFalse((self.root / "run" / "container.pid").exists())

        result, log = self._run_teardown("teardown 0", share=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(log[0], f"umount {runtime}/rootfs")

    def test_agent_teardown_reports_share_unmount_failure(self):
        share = f"{self.rootfs.as_posix()}/workspace"
        result, log = self._run_teardown("teardown 0", share=True, failing=share)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("failed to unmount the live share", result.stderr)
        self.assertEqual(log[0], f"umount {share}")
        self.assertEqual(log[-1], "exit 1")
        self.assertIn("NVX-SANDBOX-EXIT: status=1", result.stdout)

    def test_agent_fatal_unmounts_share_before_power_off(self):
        result, log = self._run_teardown('fatal "synthetic failure"', share=True)
        self.assertEqual(result.returncode, 125, result.stdout)
        self.assertIn("NVX-SANDBOX-ERROR: synthetic failure", result.stderr)
        self.assertEqual(
            log, [f"umount {self.rootfs.as_posix()}/workspace", "exit 125"]
        )

        result, log = self._run_teardown('fatal "early failure"', share=False)
        self.assertEqual(result.returncode, 125, result.stdout)
        self.assertEqual(log, ["exit 125"])

    def test_agent_skips_share_without_bootstrap_tokens(self):
        result, log = self._run("console=hvc0 nvx_sandbox=1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("mountpoint=\n", result.stdout)
        self.assertEqual(log, "")

    def test_agent_mounts_share_inside_container_rootfs(self):
        (self.rootfs / "opt").mkdir()
        for mode in ("rw", "ro"):
            with self.subTest(mode=mode):
                result, log = self._run(
                    "nvx_sandbox=1 virtfs_dir=/opt/hostedtoolcache "
                    f"virtfs_tag=microvm virtfs_mode={mode}"
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertTrue((self.rootfs / "opt" / "hostedtoolcache").is_dir())
                self.assertIn(
                    f"NVX-SANDBOX-SHARE: target=/opt/hostedtoolcache mode={mode}",
                    result.stdout,
                )
                self.assertEqual(
                    log.split(),
                    [
                        "-t",
                        "virtiofs",
                        "-o",
                        f"{mode},nosuid,nodev",
                        "microvm",
                        f"{self.rootfs.as_posix()}/opt/hostedtoolcache",
                    ],
                )
                self.assertIn(
                    f"mountpoint={self.rootfs.as_posix()}/opt/hostedtoolcache",
                    result.stdout,
                )

    def test_agent_fails_closed_for_invalid_share_bootstrap(self):
        (self.rootfs / "file").write_text("x", encoding="utf-8")
        for cmdline, message in (
            ("virtfs_dir=/workspace", "incomplete"),
            ("virtfs_dir=/workspace virtfs_tag=microvm", "incomplete"),
            ("virtfs_dir=/workspace virtfs_tag=bad/tag virtfs_mode=rw", "tag"),
            ("virtfs_dir=/workspace virtfs_tag=microvm virtfs_mode=rx", "mode"),
            ("virtfs_dir=workspace virtfs_tag=microvm virtfs_mode=rw", "absolute"),
            ("virtfs_dir=/ virtfs_tag=microvm virtfs_mode=rw", "canonical"),
            ("virtfs_dir=/a//b virtfs_tag=microvm virtfs_mode=rw", "canonical"),
            ("virtfs_dir=/a/ virtfs_tag=microvm virtfs_mode=rw", "canonical"),
            ("virtfs_dir=/a/./b virtfs_tag=microvm virtfs_mode=rw", "canonical"),
            ("virtfs_dir=/a/../b virtfs_tag=microvm virtfs_mode=rw", "canonical"),
            ("virtfs_dir=/a=b virtfs_tag=microvm virtfs_mode=rw", "reserved char"),
            ("virtfs_dir=/proc virtfs_tag=microvm virtfs_mode=rw", "reserved"),
            ("virtfs_dir=/sys/fs virtfs_tag=microvm virtfs_mode=rw", "reserved"),
            ("virtfs_dir=/dev/shm virtfs_tag=microvm virtfs_mode=rw", "reserved"),
            ("virtfs_dir=/.nvx-agent virtfs_tag=microvm virtfs_mode=ro", "reserved"),
            ("virtfs_dir=/etc virtfs_tag=microvm virtfs_mode=ro", "reserved"),
            ("virtfs_dir=/file virtfs_tag=microvm virtfs_mode=ro", "not a directory"),
        ):
            with self.subTest(cmdline=cmdline):
                result, log = self._run(cmdline)
                self.assertEqual(result.returncode, 125, result.stdout)
                self.assertIn("FATAL: live share", result.stderr)
                self.assertIn(message, result.stderr)
                self.assertEqual(log, "")

    def test_agent_rejects_symbolic_link_in_share_target(self):
        outside = self.root / "outside"
        outside.mkdir()
        try:
            (self.rootfs / "workspace").symlink_to(outside, target_is_directory=True)
        except OSError as error:
            self.skipTest(f"symbolic links are unavailable: {error}")
        result, log = self._run(
            "virtfs_dir=/workspace/project virtfs_tag=microvm virtfs_mode=rw"
        )
        self.assertEqual(result.returncode, 125, result.stdout)
        self.assertIn("crosses a symbolic link", result.stderr)
        self.assertEqual(log, "")
        self.assertFalse((outside / "project").exists())


@unittest.skipIf(os.name == "nt", "requires POSIX symbolic links")
class SandboxSmokeShareTests(unittest.TestCase):
    SMOKE = Path(__file__).parents[1] / "guest" / "common" / "nvx-sandbox-smoke"

    def setUp(self):
        shell = _posix_shell()
        if shell is None:
            self.skipTest("POSIX shell is unavailable")
        self.shell = shell
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.share = self.root / "share"
        self.links = self.share / "nvx-links"
        self.links.mkdir(parents=True)
        (self.share / "nvx-host-marker").write_text("host-to-guest\n", encoding="utf-8")
        # The guest resolves nvx-links/outside to this path through ../..
        self.outside = self.root / "nvx-outside-marker"
        source = self.SMOKE.read_text(encoding="utf-8")
        self.functions = "".join(
            _shell_function(source, name)
            for name in ("check_share", "check_share_symlinks")
        ).replace("/tmp/nvx-tool", '"$scratch/nvx-tool"')

    def _run(self, call: str) -> subprocess.CompletedProcess[str]:
        scratch = self.root / "scratch"
        scratch.mkdir(exist_ok=True)
        return subprocess.run(
            [self.shell, "-s", "--", self.share.as_posix(), scratch.as_posix()],
            input=f"set -eu\nshare=$1\nscratch=$2\n{self.functions}{call}\n",
            text=True,
            capture_output=True,
            check=False,
        )

    def test_rw_share_creates_links_with_exact_targets(self):
        result = self._run('check_share "$share" rw')

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            f"NVX-UBUNTU-SANDBOX-SYMLINK-OK target={self.share.as_posix()}",
            result.stdout,
        )
        self.assertIn("NVX-UBUNTU-SANDBOX-SHARE-OK", result.stdout)
        self.assertEqual(
            {name: os.readlink(self.links / name) for name in os.listdir(self.links)},
            {
                "nvx-tool": "../nvx-tool",
                "absolute": "/nvx/absolute/target",
                "outside": "../../nvx-outside-marker",
                "denied": "../nvx-denied/secret",
                "denied-directory": "../nvx-denied",
            },
        )
        self.assertEqual(
            (self.share / "nvx-guest-marker").read_text(encoding="utf-8"),
            "guest-to-host\n",
        )
        self.assertTrue((self.share / "nvx-guest-directory").is_dir())

    def test_links_fail_when_they_expose_host_data(self):
        (self.share / "nvx-guest-marker").write_text(
            "guest-to-host\n", encoding="utf-8"
        )
        self.outside.write_text("host-outside\n", encoding="utf-8")
        result = self._run('check_share_symlinks "$share"')
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertNotIn("NVX-UBUNTU-SANDBOX-SYMLINK-OK", result.stdout)

        for path in self.links.iterdir():
            path.unlink()
        self.outside.unlink()
        denied = self.share / "nvx-denied"
        denied.mkdir()
        (denied / "secret").write_text("secret\n", encoding="utf-8")
        result = self._run('check_share_symlinks "$share"')
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertNotIn("NVX-UBUNTU-SANDBOX-SYMLINK-OK", result.stdout)

    def test_ro_share_rejects_links(self):
        geteuid = getattr(os, "geteuid", None)
        if geteuid is not None and geteuid() == 0:
            self.skipTest("root ignores directory write permissions")
        self.share.chmod(0o555)
        self.addCleanup(self.share.chmod, 0o755)

        result = self._run('check_share "$share" ro')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("NVX-UBUNTU-SANDBOX-SHARE-OK", result.stdout)
        self.assertFalse(os.path.lexists(self.share / "nvx-guest-link"))

        self.share.chmod(0o755)
        result = self._run('check_share "$share" ro')
        self.assertEqual(result.returncode, 1, result.stdout)


class ManagedAgentCgroupTests(unittest.TestCase):
    SOURCE = Path(__file__).parents[1] / "guest" / "common" / "nvx-managed-agent.c"

    def setUp(self):
        if sys.platform != "linux":
            self.skipTest("the managed agent requires Linux")
        compiler = shutil.which("cc") or shutil.which("gcc")
        if compiler is None:
            self.skipTest("C compiler is unavailable")
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        self.cgroup = root / "cgroup"
        self.cgroup.mkdir()
        (self.cgroup / "cgroup.procs").touch()
        self.execution = self.cgroup / "nvx-exec"
        self.execution.mkdir()
        self.events = self.execution / "cgroup.events"
        self.events.write_bytes(b"populated 0\nfrozen 0\n")
        (self.execution / "cgroup.kill").touch()
        source = root / "cgroup-test.c"
        source.write_text(
            f"""#define CGROUP_ROOT {json.dumps(str(self.cgroup))}
#define main managed_agent_main
#include {json.dumps(str(self.SOURCE))}
#undef main
int main(int argc, char **argv)
{{
    int result;
    if (argc != 2) {{
        return 2;
    }}
    errno = 0;
    if (strcmp(argv[1], "retry") == 0) {{
        struct control_session session = {{.fd = STDOUT_FILENO}};
        struct agent_config config = {{.direct = 1}};
        char *command[] = {{"/bin/true", NULL}};
        result = run_exec(&session, &config, 42, 0, command);
        return result == 0 ? run_exec(&session, &config, 43, 0, command) : result;
    }}
    if (strcmp(argv[1], "population") == 0) {{
        result = exec_cgroup_populated();
    }} else if (strcmp(argv[1], "settle") == 0) {{
        result = settle_exec_cgroup();
    }} else {{
        return 2;
    }}
    printf("%d %d\\n", result, errno);
    return 0;
}}
""",
            encoding="utf-8",
        )
        self.helper = root / "cgroup-test"
        flags = [
            flag
            for flag in InitramfsBuildConstants.STATIC_HELPER_CFLAGS
            if flag != "-static"
        ]
        result = subprocess.run(
            [compiler, *flags, "-o", str(self.helper), str(source)],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def _check(self, mode: str) -> tuple[int, int]:
        result = subprocess.run(
            [str(self.helper), mode],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        value, status = result.stdout.split()
        return int(value), int(status)

    def _assert_further_exec_refused(self):
        result = subprocess.run(
            [str(self.helper), "retry"],
            capture_output=True,
            timeout=5,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        frames = result.stdout
        for expected_id in (42, 43):
            outer = ManagedAgentStopTests.OUTER
            app = ManagedAgentStopTests.APP
            *_, length = outer.unpack(frames[: outer.size])
            frame = frames[outer.size : outer.size + length]
            _, _, kind, _, request_id, status, payload_len = app.unpack(
                frame[: app.size]
            )
            payload = frame[app.size :]
            self.assertEqual((kind, request_id, status), (0xFF, expected_id, 125))
            self.assertEqual(payload_len, len(payload))
            self.assertEqual(payload, b"containment-failed")
            frames = frames[outer.size + length :]
        self.assertEqual(frames, b"")

    def test_population_requires_a_valid_unique_field(self):
        for value in (0, 1):
            with self.subTest(value=value):
                self.events.write_text(f"populated {value}\nfrozen 0\n")
                self.assertEqual(self._check("population")[0], value)
        for contents in (
            b"",
            b"frozen 0\n",
            b"populated 0",
            b"populated 2\n",
            b"populated 01\n",
            b"populated 0\npopulated 1\n",
            b"populated 0\n\x00frozen 0\n",
        ):
            with self.subTest(contents=contents):
                self.events.write_bytes(contents)
                self.assertEqual(self._check("population"), (-1, errno.EPROTO))
        self.events.write_bytes(b"populated 0\n" + b"x" * 256)
        self.assertEqual(self._check("population"), (-1, errno.EOVERFLOW))

    def test_missing_and_unreadable_events_are_errors(self):
        self.events.unlink()
        self.assertEqual(self._check("population"), (-1, errno.ENOENT))
        self._assert_further_exec_refused()
        self.events.mkdir()
        self.assertEqual(self._check("population"), (-1, errno.EISDIR))
        self._assert_further_exec_refused()

    def test_settlement_verifies_emptiness(self):
        self.assertEqual(self._check("settle")[0], 0)
        self.assertEqual((self.execution / "cgroup.kill").read_bytes(), b"1")

    def test_settlement_does_not_hide_kill_or_verification_failures(self):
        self.events.write_bytes(b"populated 1\n")
        (self.execution / "cgroup.kill").unlink()
        self.assertEqual(self._check("settle"), (-1, errno.ENOENT))
        self._assert_further_exec_refused()
        (self.execution / "cgroup.kill").touch()
        self.events.unlink()
        self.assertEqual(self._check("settle"), (-1, errno.ENOENT))
        self._assert_further_exec_refused()

    def test_populated_cgroup_at_deadline_is_not_settled(self):
        self.events.write_bytes(b"populated 1\n")
        started = time.monotonic()
        self.assertEqual(self._check("settle"), (-1, errno.ETIMEDOUT))
        self.assertGreaterEqual(time.monotonic() - started, 1.9)
        self._assert_further_exec_refused()

    def test_malformed_events_refuse_further_exec_until_verified_empty(self):
        self.events.write_bytes(b"frozen 0\n")
        self.assertEqual(self._check("settle"), (-1, errno.EPROTO))
        self._assert_further_exec_refused()
        self.events.write_bytes(b"populated 0\nfrozen 0\n")
        self.assertEqual(self._check("population")[0], 0)
        self.assertEqual(self._check("settle")[0], 0)


class ManagedAgentStopTests(unittest.TestCase):
    SOURCE = Path(__file__).parents[1] / "guest" / "common" / "nvx-managed-agent.c"
    OUTER = struct.Struct("<4sHBB16sQQI")
    APP = struct.Struct("<4sBBHQiI")

    def setUp(self):
        if sys.platform != "linux":
            self.skipTest("the managed agent requires a Linux pseudo-terminal")
        compiler = shutil.which("cc") or shutil.which("gcc")
        if compiler is None:
            self.skipTest("C compiler is unavailable")
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.agent = Path(temporary.name) / "nvx-managed-agent"
        flags = [
            flag
            for flag in InitramfsBuildConstants.STATIC_HELPER_CFLAGS
            if flag != "-static"
        ]
        result = subprocess.run(
            [compiler, *flags, "-o", str(self.agent), str(self.SOURCE)],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def _record(
        self, record_type: int, instance: bytes, sequence: int, payload: bytes = b""
    ) -> bytes:
        header = self.OUTER.pack(
            b"NVXS", 1, record_type, 0, instance, 1, sequence, len(payload)
        )
        return header + payload

    def _read_exact(self, fd: int, length: int, deadline: float) -> bytes:
        data = b""
        while len(data) < length:
            remaining = deadline - time.monotonic()
            self.assertGreater(remaining, 0, "managed agent did not respond")
            readable, _, _ = select.select([fd], [], [], remaining)
            if readable:
                chunk = os.read(fd, length - len(data))
                self.assertTrue(chunk, "managed agent closed the control tty")
                data += chunk
        return data

    def _read_record(self, fd: int, deadline: float) -> tuple[int, int, bytes]:
        header = self._read_exact(fd, self.OUTER.size, deadline)
        magic, version, record_type, flags, _, _, sequence, length = self.OUTER.unpack(
            header
        )
        self.assertEqual((magic, version, flags), (b"NVXS", 1, 0))
        return record_type, sequence, self._read_exact(fd, length, deadline)

    @staticmethod
    def _reap(process: subprocess.Popen[bytes]) -> None:
        if process.poll() is None:
            process.kill()
        process.wait(timeout=5)
        if process.stderr is not None:
            process.stderr.close()

    def _open_session(
        self, rootfs: str
    ) -> tuple[subprocess.Popen[bytes], int, bytes, float]:
        """Starts an agent and completes the attach and reset handshake."""
        if sys.platform != "linux":
            self.skipTest("the managed agent requires a Linux pseudo-terminal")
        import pty

        master, slave = pty.openpty()
        self.addCleanup(os.close, master)
        self.addCleanup(os.close, slave)
        process = subprocess.Popen(
            [
                str(self.agent),
                os.ttyname(slave),
                rootfs,
                "nvx-sandbox",
                "65534",
                "65534",
                "nobody",
                "/nonexistent",
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        self.addCleanup(self._reap, process)
        deadline = time.monotonic() + 10
        instance = bytes(range(1, 17))

        self.assertEqual(self._read_record(master, deadline)[0], 1)
        os.write(master, self._record(3, instance, 7))
        record_type, sequence, credit = self._read_record(master, deadline)
        self.assertEqual((record_type, sequence, len(credit)), (4, 0, 4))
        return process, master, instance, deadline

    def _request(
        self, rootfs: str, kind: int, payload: bytes = b""
    ) -> tuple[int, int, int, bytes]:
        """Sends one request to a fresh agent; returns the kind, request ID, status, and payload of its answer."""
        _, master, instance, deadline = self._open_session(rootfs)
        frame = self.APP.pack(b"NVXC", 1, kind, 0, 42, 0, len(payload)) + payload
        os.write(master, self._record(5, instance, 8, frame))
        self.assertEqual(self._read_record(master, deadline)[0], 9)
        record_type, _, answer = self._read_record(master, deadline)
        self.assertEqual(record_type, 5)
        _, _, answered, _, request_id, status, length = self.APP.unpack(
            answer[: self.APP.size]
        )
        body = answer[self.APP.size :]
        self.assertEqual(length, len(body))
        return answered, request_id, status, body

    def test_sandbox_agent_returns_to_init_agent_after_stop(self):
        if sys.platform != "linux":
            self.skipTest("the managed agent requires a Linux pseudo-terminal")
        process, master, instance, deadline = self._open_session("/run/nvx/rootfs")
        stop = self.APP.pack(b"NVXC", 1, 3, 0, 42, 0, 0)
        os.write(master, self._record(5, instance, 8, stop))
        self.assertEqual(self._read_record(master, deadline)[0], 9)
        record_type, _, payload = self._read_record(master, deadline)
        self.assertEqual(record_type, 5)
        _, _, kind, _, request_id, status, length = self.APP.unpack(payload)
        self.assertEqual((kind, request_id, status, length), (0x85, 42, 0, 0))

        _, stderr = process.communicate(timeout=10)
        self.assertEqual(process.returncode, 0, stderr)

    def test_agent_advertises_the_control_features_of_its_mode(self):
        cancel, host_mappings, workload_account, exec_cgroup = 1, 2, 4, 8
        for rootfs, expected in (
            # Only the direct agent maps host paths and gives each workload a cgroup.
            ("-", cancel | host_mappings | workload_account | exec_cgroup),
            ("/run/nvx/rootfs", cancel | workload_account),
        ):
            with self.subTest(rootfs=rootfs):
                kind, request_id, status, body = self._request(rootfs, 5)
                self.assertEqual((kind, request_id, status), (0x81, 42, 0))
                self.assertEqual(struct.unpack("<I", body), (expected,))

    def test_agent_refuses_a_features_request_with_a_payload(self):
        kind, request_id, status, body = self._request("-", 5, b"abc")
        self.assertEqual((kind, request_id, status), (0xFF, 42, 22))
        self.assertEqual(body, b"invalid-request")


class AciSandboxRunnerTests(unittest.TestCase):
    def _artifacts(self, root: Path) -> dict[str, Path]:
        names = {
            KernelBuildConstants.BINARY_NAME: b"kernel",
            AlpineBuildConstants.INITRAMFS_NAME: b"initrd",
            "openvmm": b"vmm",
        }
        paths: dict[str, Path] = {}
        for name, contents in names.items():
            paths[name] = root / name
            paths[name].write_bytes(contents)
        return paths

    def _environment(self, root: Path, paths: dict[str, Path]) -> dict[str, str]:
        def artifact(name: str) -> Path:
            return root / name

        def openvmm() -> Path:
            return paths["openvmm"]

        with (
            patch.object(aci_edge_sandboxes_tests, "artifact_path", artifact),
            patch.object(aci_edge_sandboxes_tests, "openvmm_binary_path", openvmm),
        ):
            return aci_edge_sandboxes_tests.e2e_environment(
                "kvm", root / "state", root / "out"
            )

    def test_cli_registers_the_lifecycle_test(self):
        args = nvx.parse_args(["test-aci-edge-sandboxes", "--backend", "mshv"])

        self.assertEqual(args.backend, "mshv")
        self.assertEqual(args.cargo, "cargo")
        self.assertFalse(hasattr(args, "scratch_template"))
        self.assertIs(
            args.handler, aci_edge_sandboxes_tests.command_test_aci_edge_sandboxes
        )

    def test_environment_resolves_repository_artifacts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths = self._artifacts(root)
            environment = self._environment(root, paths)

            self.assertEqual(environment["ACI_EDGE_SANDBOXES_E2E_HYPERVISOR"], "kvm")
            self.assertEqual(
                Path(environment["ACI_EDGE_SANDBOXES_E2E_OPENVMM"]),
                paths["openvmm"].resolve(),
            )
            self.assertEqual(
                Path(environment["ACI_EDGE_SANDBOXES_E2E_INITRD"]),
                paths[AlpineBuildConstants.INITRAMFS_NAME].resolve(),
            )
            self.assertEqual(
                Path(environment["ACI_EDGE_SANDBOXES_E2E_STATE_ROOT"]),
                (root / "state").resolve(),
            )
            self.assertFalse(
                any(
                    name.startswith("ACI_EDGE_SANDBOXES_E2E_DISTRO")
                    for name in environment
                )
            )
            self.assertNotIn("ACI_EDGE_SANDBOXES_E2E_SCRATCH", environment)

    def test_environment_requires_the_alpine_initramfs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths = self._artifacts(root)
            paths[AlpineBuildConstants.INITRAMFS_NAME].unlink()
            with self.assertRaisesRegex(common.ScriptError, "Alpine initramfs"):
                self._environment(root, paths)

    def test_command_runs_only_the_ignored_lifecycle_test(self):
        command = aci_edge_sandboxes_tests.e2e_command("cargo")

        self.assertEqual(command[:2], ["cargo", "test"])
        self.assertEqual(
            Path(command[command.index("--manifest-path") + 1]),
            BuildConstants.REPO_ROOT / "aci_edge_sandboxes" / "Cargo.toml",
        )
        self.assertIn("--locked", command)
        self.assertEqual(command[command.index("--test") + 1], "openvmm_e2e")
        self.assertEqual(command[-2:], ["--ignored", "--nocapture"])

    def test_handler_returns_the_test_status(self):
        observed: list[str] = []

        def fake_run(
            command: list[str], *, env: dict[str, str], check: bool
        ) -> subprocess.CompletedProcess[bytes]:
            self.assertFalse(check)
            self.assertEqual(command, aci_edge_sandboxes_tests.e2e_command("cargo"))
            observed.append(env["ACI_EDGE_SANDBOXES_E2E_STATE_ROOT"])
            return subprocess.CompletedProcess(command, 3)

        def fake_environment(
            backend: str, state_root: Path, output_dir: Path
        ) -> dict[str, str]:
            self.assertEqual(backend, "whp")
            return {"ACI_EDGE_SANDBOXES_E2E_STATE_ROOT": os.fspath(state_root)}

        with tempfile.TemporaryDirectory() as temporary:
            args = argparse.Namespace(
                backend="whp",
                output_dir=Path(temporary) / "results",
                cargo="cargo",
            )
            with (
                patch.object(aci_edge_sandboxes_tests, "validate_openvmm_test_backend"),
                patch.object(
                    aci_edge_sandboxes_tests, "e2e_environment", fake_environment
                ),
                patch.object(aci_edge_sandboxes_tests.subprocess, "run", fake_run),
            ):
                self.assertEqual(
                    aci_edge_sandboxes_tests.command_test_aci_edge_sandboxes(args), 3
                )
            self.assertTrue((Path(temporary) / "results").is_dir())

        self.assertEqual(len(observed), 1)
        self.assertEqual(Path(observed[0]).name, "state")

    def _run_leaving(
        self, records: dict[str, object] | None, *, interrupt: bool = False
    ) -> tuple[Path, str]:
        """Runs the handler with a test command that leaves one sandbox provisioned.

        The sandbox gets `records` as extra state files; `None` leaves only the lock
        directories that deprovisioning keeps. Returns the state root that the command
        used and the handler's standard error.
        """
        state_roots: list[Path] = []

        def fake_environment(
            backend: str, state_root: Path, output_dir: Path
        ) -> dict[str, str]:
            state_roots.append(state_root)
            return {}

        def fake_run(
            command: list[str], *, env: dict[str, str], check: bool
        ) -> subprocess.CompletedProcess[bytes]:
            (state_roots[0] / "filesystem" / ".locks").mkdir(parents=True)
            if records is not None:
                sandbox = state_roots[0] / "network" / ("a" * 32)
                sandbox.mkdir(parents=True)
                (sandbox / "sandbox.json").write_text("{}", encoding="utf-8")
                for name, record in records.items():
                    (sandbox / name).write_text(json.dumps(record), encoding="utf-8")
            if interrupt:
                raise KeyboardInterrupt
            return subprocess.CompletedProcess(command, 0)

        with tempfile.TemporaryDirectory() as temporary:
            args = argparse.Namespace(
                backend="kvm", output_dir=Path(temporary) / "results", cargo="cargo"
            )
            with (
                patch.object(aci_edge_sandboxes_tests, "validate_openvmm_test_backend"),
                patch.object(
                    aci_edge_sandboxes_tests, "e2e_environment", fake_environment
                ),
                patch.object(aci_edge_sandboxes_tests.subprocess, "run", fake_run),
                patch("sys.stderr", io.StringIO()) as stderr,
            ):
                if interrupt:
                    with self.assertRaises(KeyboardInterrupt):
                        aci_edge_sandboxes_tests.command_test_aci_edge_sandboxes(args)
                else:
                    self.assertEqual(
                        aci_edge_sandboxes_tests.command_test_aci_edge_sandboxes(args),
                        0,
                    )
        state_root = state_roots[0]
        if state_root.parent.exists():
            self.addCleanup(shutil.rmtree, state_root.parent, True)
        return state_root, stderr.getvalue()

    def test_handler_deletes_state_once_every_sandbox_is_deprovisioned(self):
        state_root, stderr = self._run_leaving(None)

        self.assertFalse(state_root.parent.exists())
        self.assertEqual(stderr, "")

    def test_handler_keeps_state_of_sandboxes_left_provisioned(self):
        state_root, stderr = self._run_leaving({"runtime.json": {"pid": 4242}})

        sandbox = state_root / "network" / ("a" * 32)
        self.assertTrue((sandbox / "runtime.json").is_file())
        self.assertIn(f"kept {state_root}", stderr)
        self.assertIn(f"{sandbox} (OpenVMM pid 4242)", stderr)

    def test_handler_keeps_state_when_the_test_is_interrupted(self):
        state_root, stderr = self._run_leaving(
            {"launch.json": {"process": {"pid": 77}}}, interrupt=True
        )

        sandbox = state_root / "network" / ("a" * 32)
        self.assertTrue((sandbox / "launch.json").is_file())
        self.assertIn(f"{sandbox} (OpenVMM pid 77)", stderr)


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

    def test_mount_parser_defaults_to_read_only_and_accepts_rw(self):
        default = sandbox.SandboxMount.parse("/workspace,host-dir")
        self.assertEqual(default.guest_target, "/workspace")
        self.assertEqual(default.host_path, Path("host-dir"))
        self.assertEqual(default.access, "ro")
        self.assertEqual(default.denied_paths, ())

        writable = sandbox.SandboxMount.parse(
            "/opt/hostedtoolcache,host-dir,rw", ("logs", "secrets")
        )
        self.assertEqual(writable.access, "rw")
        self.assertEqual(writable.denied_paths, ("logs", "secrets"))
        self.assertEqual(
            writable.openvmm_arguments(),
            [
                "--mount",
                f"/opt/hostedtoolcache,{os.fspath(Path('host-dir'))},rw",
                "--mount-deny",
                "logs",
                "--mount-deny",
                "secrets",
            ],
        )
        self.assertEqual(
            writable.command_line_fragment(),
            " virtfs_dir=/opt/hostedtoolcache virtfs_tag=microvm virtfs_mode=rw",
        )

    def test_mount_parser_rejects_malformed_specifications(self):
        for value, message in (
            ("/workspace", "GUEST_TARGET,HOST_PATH"),
            ("/workspace,host,rw,extra", "GUEST_TARGET,HOST_PATH"),
            ("/workspace,", "host path is empty"),
            ("/workspace,host,rx", "unsupported sandbox mount mode"),
        ):
            with self.subTest(value=value):
                with self.assertRaisesRegex(common.ScriptError, message):
                    sandbox.SandboxMount.parse(value)
        with self.assertRaisesRegex(common.ScriptError, "commas"):
            sandbox.SandboxMount(guest_target="/workspace", host_path=Path("a,b"))
        with self.assertRaisesRegex(common.ScriptError, "parent component"):
            sandbox.SandboxMount.parse("/workspace,link/../share")

    def test_mount_absolute_path_keeps_symbolic_link_components(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "link-target" / "share").mkdir(parents=True)
            try:
                (root / "link").symlink_to(
                    root / "link-target", target_is_directory=True
                )
            except OSError as error:
                self.skipTest(f"symbolic links are unavailable: {error}")
            previous = Path.cwd()
            os.chdir(root)
            try:
                mount = sandbox.SandboxMount.parse("/workspace,link/share,rw")
                absolute = mount.absolute()
                expected = Path.cwd() / "link" / "share"
            finally:
                os.chdir(previous)

        self.assertEqual(absolute.host_path, expected)
        self.assertNotIn("link-target", absolute.host_path.parts)
        self.assertEqual(absolute.absolute(), absolute)
        with self.assertRaisesRegex(common.ScriptError, "unique"):
            sandbox.SandboxMount.parse("/workspace,host", ("logs", "logs"))
        with self.assertRaisesRegex(common.ScriptError, "nonempty"):
            sandbox.SandboxMount.parse("/workspace,host", ("",))
        with self.assertRaisesRegex(common.ScriptError, "at most 128"):
            sandbox.SandboxMount.parse(
                "/workspace,host", tuple(f"path-{index}" for index in range(129))
            )

    def test_mount_target_validation_rejects_unsafe_and_reserved_targets(self):
        for target in (
            "",
            "/",
            "workspace",
            "/workspace/",
            "/work//space",
            "/work/./space",
            "/work/../space",
            "/work space",
            "/work=space",
            "/work\\space",
            "/" + "x" * 4096,
        ):
            with self.subTest(target=target):
                with self.assertRaisesRegex(common.ScriptError, "invalid sandbox"):
                    sandbox.validate_mount_target(target)
        for target in (
            "/proc",
            "/proc/self",
            "/sys",
            "/sys/fs",
            "/dev",
            "/dev/shm",
            "/.nvx-agent",
            "/.nvx-agent/tools",
            "/etc",
        ):
            with self.subTest(target=target):
                with self.assertRaisesRegex(common.ScriptError, "reserved"):
                    sandbox.validate_mount_target(target)
        for target in ("/workspace", "/procfs", "/etc/app", "/opt/hostedtoolcache"):
            with self.subTest(target=target):
                self.assertEqual(sandbox.validate_mount_target(target), target)

    def test_launch_contract_attaches_mount_and_reserves_bootstrap_tokens(self):
        distro = sandbox.SandboxLayer.parse(
            "distro,distro.erofs,11111111-1111-1111-1111-111111111111"
        )
        launch = sandbox.SandboxLaunch(
            layers=(distro,),
            scratch=Path("scratch.ext4"),
            mount=sandbox.SandboxMount.parse("/workspace,share,rw", ("secrets",)),
        )

        arguments = launch.openvmm_arguments()
        self.assertEqual(
            arguments[arguments.index("--mount") :],
            [
                "--mount",
                f"/workspace,{os.fspath(Path('share'))},rw",
                "--mount-deny",
                "secrets",
            ],
        )
        self.assertNotIn("virtfs_", launch.kernel_command_line("quiet"))
        for token in ("virtfs_dir=/other", "virtfs_tag=other", "virtfs_mode=rw"):
            with self.subTest(token=token):
                with self.assertRaisesRegex(common.ScriptError, "--mount option"):
                    launch.kernel_command_line(token)

        unmounted = sandbox.SandboxLaunch(layers=(distro,), scratch=Path("s.ext4"))
        base = unmounted.kernel_command_line()
        fragment = launch.mount.command_line_fragment() if launch.mount else ""
        fitting = "x" * (
            sandbox.SANDBOX_COMMAND_LINE_MAX_SIZE - len(base) - len(fragment) - 2
        )
        launch.kernel_command_line(fitting)
        with self.assertRaisesRegex(common.ScriptError, "1024-byte"):
            launch.kernel_command_line(fitting + "x")

    def test_launch_validation_requires_plain_mount_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            layer = root / "distro.erofs"
            scratch = root / "scratch.ext4"
            share = root / "share"
            layer.write_bytes(b"layer")
            scratch.write_bytes(b"scratch")

            def launch(host_path: Path) -> sandbox.SandboxLaunch:
                return sandbox.SandboxLaunch(
                    layers=(
                        sandbox.SandboxLayer(
                            role="distro",
                            path=layer,
                            uuid="11111111-1111-1111-1111-111111111111",
                        ),
                    ),
                    scratch=scratch,
                    mount=sandbox.SandboxMount(
                        guest_target="/workspace", host_path=host_path
                    ),
                )

            with self.assertRaisesRegex(common.ScriptError, "plain directory"):
                launch(share).validated()
            share.write_bytes(b"file")
            with self.assertRaisesRegex(common.ScriptError, "plain directory"):
                launch(share).validated()
            share.unlink()
            share.mkdir()
            validated = launch(share).validated()
            assert validated.mount is not None
            self.assertEqual(validated.mount.host_path, share)

    def test_sandbox_command_forwards_mount_to_openvmm(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            layer = root / "distro.erofs"
            scratch = root / "scratch.ext4"
            share = root / "share"
            layer.write_bytes(b"distro")
            scratch.write_bytes(b"scratch")
            share.mkdir()
            args = nvx.parse_args(
                [
                    "sandbox",
                    "--layer",
                    f"distro,{layer},11111111-1111-1111-1111-111111111111",
                    "--scratch",
                    str(scratch),
                    "--mount",
                    f"/workspace,{share},rw",
                    "--mount-deny",
                    "secrets",
                    "--dry-run",
                ]
            )

            def require(path: Path, _description: str) -> Path:
                return path

            with (
                patch.object(nvx, "require_file", side_effect=require),
                patch.object(
                    nvx, "_format_command", return_value="formatted"
                ) as format_command,
            ):
                nvx.command_sandbox(args)

            command = format_command.call_args.args[0]
            self.assertEqual(
                command[command.index("--mount") + 1], f"/workspace,{share},rw"
            )
            self.assertEqual(command[command.index("--mount-deny") + 1], "secrets")

    def test_sandbox_command_rejects_misplaced_mount_options(self):
        deny_only = nvx.parse_args(
            [
                "sandbox",
                "--layer",
                "distro,distro.erofs,11111111-1111-1111-1111-111111111111",
                "--scratch",
                "scratch.ext4",
                "--mount-deny",
                "secrets",
            ]
        )
        with self.assertRaisesRegex(common.ScriptError, "requires --mount"):
            nvx.command_sandbox(deny_only)
        for operation in ("start", "exec", "stop", "deprovision"):
            with self.subTest(operation=operation):
                args = nvx.parse_args(
                    [
                        "sandbox",
                        operation,
                        "--state-dir",
                        "state",
                        "--mount",
                        "/workspace,share,rw",
                    ]
                )
                with self.assertRaisesRegex(common.ScriptError, "only valid"):
                    nvx.command_sandbox(args)

    def test_managed_lifecycle_persists_and_replays_mount(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            layer_path = root / "distro.erofs"
            scratch_path = root / "scratch.ext4"
            share = root / "share"
            layer_path.write_bytes(b"layer")
            scratch_path.write_bytes(b"scratch")
            share.mkdir()
            state = root / "state"
            previous = Path.cwd()
            os.chdir(root)
            try:
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
                        mount=sandbox.SandboxMount.parse(
                            "/workspace,share,rw", ("secrets",)
                        ),
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
            finally:
                os.chdir(previous)

            config = json.loads(
                (state / sandbox_lifecycle.CONFIG_NAME).read_text(encoding="utf-8")
            )
            self.assertEqual(config["format"], sandbox_lifecycle.MOUNT_CONFIG_FORMAT)
            with self.assertRaisesRegex(common.ScriptError, "unsupported format"):
                sandbox_lifecycle._read_json(
                    state / sandbox_lifecycle.CONFIG_NAME,
                    "sandbox configuration",
                    version=1,
                )
            self.assertEqual(
                config["mount"],
                {
                    "guest_target": "/workspace",
                    "host_path": os.fspath(share),
                    "access": "rw",
                    "denied_paths": ["secrets"],
                },
            )

            process = MagicMock()
            process.pid = 123
            process.stdin = io.BytesIO()
            context = MagicMock()

            def require(path: Path, _description: str) -> Path:
                return path

            with (
                patch.object(sandbox_lifecycle, "require_file", side_effect=require),
                patch.object(
                    sandbox_lifecycle.subprocess, "Popen", return_value=process
                ) as popen,
                patch.object(
                    sandbox_lifecycle.ControlSession, "connect", return_value=context
                ),
            ):
                sandbox_lifecycle.start(state, 10)

            command = popen.call_args.args[0]
            self.assertEqual(
                command[command.index("--mount") + 1], f"/workspace,{share},rw"
            )
            self.assertEqual(command[command.index("--mount-deny") + 1], "secrets")

    def test_managed_lifecycle_accepts_configuration_without_mount(self):
        config: dict[str, object] = {
            "format": sandbox_lifecycle.CONFIG_FORMAT,
            "layers": [
                {
                    "role": "distro",
                    "path": "distro.erofs",
                    "uuid": "11111111-1111-1111-1111-111111111111",
                }
            ],
            "scratch": "scratch.ext4",
            "hostname": "nvx-sandbox",
            "workload_uid": 65534,
            "workload_gid": 65534,
            "memory_max": None,
            "pids_max": None,
        }

        def require(path: Path, _description: str) -> Path:
            return path

        with (
            patch.object(sandbox, "require_file", side_effect=require),
        ):
            self.assertIsNone(sandbox_lifecycle._deserialize_launch(config).mount)
            config["format"] = sandbox_lifecycle.MOUNT_CONFIG_FORMAT
            with self.assertRaisesRegex(common.ScriptError, "does not match"):
                sandbox_lifecycle._deserialize_launch(config)
            config["mount"] = {"guest_target": "/workspace"}
            with self.assertRaisesRegex(common.ScriptError, "malformed"):
                sandbox_lifecycle._deserialize_launch(config)
            config["mount"] = {
                "guest_target": "/proc",
                "host_path": "share",
                "access": "rw",
                "denied_paths": [],
            }
            with self.assertRaisesRegex(common.ScriptError, "reserved"):
                sandbox_lifecycle._deserialize_launch(config)
            config["mount"] = {
                "guest_target": "/workspace",
                "host_path": "share",
                "access": "rw",
                "denied_paths": [],
            }
            config["format"] = sandbox_lifecycle.CONFIG_FORMAT
            with self.assertRaisesRegex(common.ScriptError, "does not match"):
                sandbox_lifecycle._deserialize_launch(config)

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
            self.assertEqual(config["format"], sandbox_lifecycle.CONFIG_FORMAT)
            self.assertIsNone(config["mount"])
            self.assertEqual(config["workload_uid"], 65534)
            self.assertEqual(config["hypervisor"], "whp")
            self.assertFalse((state / sandbox_lifecycle.RUNTIME_NAME).exists())

            config["network_egress_allow"] = None
            (state / sandbox_lifecycle.CONFIG_NAME).write_text(
                json.dumps(config), encoding="utf-8"
            )

            def require(path: Path, _description: str) -> Path:
                return path

            with (
                patch.object(
                    sandbox_lifecycle,
                    "require_file",
                    side_effect=require,
                ),
                self.assertRaisesRegex(
                    common.ScriptError, "sandbox configuration is malformed"
                ),
            ):
                sandbox_lifecycle.start(state, 10)

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
            root = Path(temporary).resolve()
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

    def test_versioned_json_reader_supports_custom_version_field(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "outcome.json"
            payload = {"schema_version": sandbox_lifecycle.OUTCOME_SCHEMA_VERSION}
            path.write_text(json.dumps(payload), encoding="utf-8")

            self.assertEqual(
                sandbox_lifecycle._read_json(
                    path,
                    "OpenVMM outcome report",
                    version_field="schema_version",
                    version=sandbox_lifecycle.OUTCOME_SCHEMA_VERSION,
                ),
                payload,
            )
            with self.assertRaisesRegex(common.ScriptError, "unsupported format"):
                sandbox_lifecycle._read_json(
                    path,
                    "OpenVMM outcome report",
                    version_field="schema_version",
                    version=sandbox_lifecycle.OUTCOME_SCHEMA_VERSION + 1,
                )

    def test_versioned_json_reader_rejects_invalid_utf8(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "config.json"
            path.write_bytes(b"\xff")

            with self.assertRaisesRegex(common.ScriptError, "failed to read"):
                sandbox_lifecycle._read_json(path, "sandbox configuration")

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

    def test_require_file_preserves_resolved_path_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            missing = root / "nested" / ".." / "missing"
            resolved_missing = missing.resolve()

            with self.assertRaises(FileNotFoundError) as raised:
                benchmark.require_file(missing, "benchmark artifact")

            self.assertEqual(
                str(raised.exception),
                f"benchmark artifact not found: {resolved_missing}",
            )

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

            def read_stderr(self, _chunks: object) -> None:
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
                ("console", benchmark.BOOT_MARKER + b"\n", 0, False),
                ("console", benchmark.SMP_PROBE_COMPLETION_MARKER + b"\n", 1, False),
                (
                    "console",
                    benchmark.SNAPSHOT_GUEST_DISPATCH_MARKER + b"\n",
                    delay_ms,
                    False,
                ),
                (
                    "stderr",
                    b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture "
                    b"phase=input_gate exclusive=1 duration_ns=500000 "
                    b"process_elapsed_ns=500000000 pid=42\n"
                    b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture "
                    b"phase=publication_commit exclusive=1 duration_ns=100000 "
                    b"process_elapsed_ns=502500000 pid=42\n",
                    3,
                    True,
                ),
                ("console", None, 0, False),
                ("stderr", None, 0, False),
            )
        )
        profiles: list[dict[str, object]] = []
        with tempfile.TemporaryDirectory() as temporary:
            snapshot = Path(temporary) / "snapshot"

            def read_chunk(*, timeout: float) -> tuple[str, bytes | None]:
                nonlocal clock_ns
                del timeout
                stream, chunk, advance_ms, publish = next(chunks)
                clock_ns += advance_ms * 1_000_000
                if publish:
                    snapshot.mkdir()
                return stream, chunk

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
        self.assertIs(interaction.call_args.kwargs["separate_stderr"], True)
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

    # OpenVMM's capture profile records from a Linux/MSHV acceptance run, in
    # which the records reached the shared terminal in the middle of the
    # guest's dispatch marker line.
    INCIDENT_PID = 621736
    INCIDENT_CAPTURE_PROFILE = (
        ("input_gate", 1, 263787, 1422094434, ""),
        ("vp_stop_at_io_boundary", 1, 120994, 1422235827, ""),
        ("quiesce", 1, 393680, 1422716902, ""),
        ("save_state", 1, 647567, 1423376968, ""),
        ("mapped_memory_flush", 1, 2700, 1423391067, ""),
        ("memory_handle_flush", 1, 1400, 1423507161, " logical_bytes=134217728"),
        (
            "publication_state",
            1,
            21499,
            1423577158,
            " logical_bytes=2455 allocated_bytes=4096",
        ),
        (
            "publication_manifest",
            1,
            8899,
            1423643854,
            " logical_bytes=1454 allocated_bytes=4096",
        ),
        (
            "publication_memory",
            1,
            39798,
            1423694652,
            " logical_bytes=134217728 allocated_bytes=65011712",
        ),
        ("publication_staging_sync", 1, 3000, 1423709951, ""),
        ("publication_commit", 1, 12700, 1423734150, ""),
        ("publication_parent_sync", 1, 2700, 1423748449, ""),
        ("publication", 0, 235088, 1423760848, " logical_bytes=134217728"),
    )

    @classmethod
    def incident_capture_records(cls) -> bytes:
        return b"".join(
            (
                f"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture phase={phase} "
                f"exclusive={exclusive} duration_ns={duration} "
                f"process_elapsed_ns={elapsed} pid={cls.INCIDENT_PID}{extra}\n"
            ).encode()
            for phase, exclusive, duration, elapsed, extra in (
                cls.INCIDENT_CAPTURE_PROFILE
            )
        )

    def test_capture_observes_dispatch_marker_split_by_profile_records(self):
        records = self.incident_capture_records()
        committed = b"INFO microVM snapshot committed; terminating source process\n"
        snapshot_requested = threading.Event()
        console_split = threading.Event()
        stderr_written = threading.Event()

        class FakeProcess:
            pid = BenchmarkTests.INCIDENT_PID
            returncode = 0

            def poll(self) -> None:
                return None

            def wait(self) -> int:
                return 0

        class FakeInteraction:
            def __init__(self, snapshot: Path) -> None:
                self.process = FakeProcess()
                self.snapshot = snapshot
                self.writes: list[bytes] = []

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(benchmark.BOOT_MARKER + b"\r\n")
                chunks.put(benchmark.SMP_PROBE_COMPLETION_MARKER + b"\r\n")
                if snapshot_requested.wait(5):
                    # The console relay delivers the dispatch line in two
                    # writes, around OpenVMM's capture records on stderr.
                    chunks.put(b"nvx-snapshot\r\nNVX-S")
                    console_split.set()
                    if stderr_written.wait(5):
                        chunks.put(b"NAPSHOT-DISPATCHED\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                if console_split.wait(5):
                    chunks.put(records)
                    self.snapshot.mkdir()
                    chunks.put(committed)
                    stderr_written.set()
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                self.writes.append(data)
                if data == b"nvx-snapshot\n":
                    snapshot_requested.set()

            def close(self) -> None:
                pass

        profiles: list[dict[str, object]] = []
        with tempfile.TemporaryDirectory() as temporary:
            snapshot = Path(temporary) / "snapshot"
            log_path = Path(temporary) / "capture.log"
            interaction = FakeInteraction(snapshot)
            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=interaction
                ) as spawn,
                patch.object(benchmark, "_try_peak_rss", return_value=1024),
                patch.object(benchmark, "process_resource_counters", return_value={}),
                patch.object(benchmark, "terminate"),
            ):
                result = benchmark.capture_snapshot(
                    ["openvmm"],
                    snapshot,
                    backend="mshv",
                    processors=1,
                    timeout=10,
                    snapshot_profile=True,
                    profile_sink=profiles,
                    log_path=log_path,
                )
            log = log_path.read_bytes()

        self.assertIs(spawn.call_args.kwargs["separate_stderr"], True)
        self.assertEqual(result[0], 1_903_503 / 1_000_000)
        self.assertEqual(result[3], 1024)
        self.assertEqual(interaction.writes[1:], [b"nvx-snapshot\n"])
        sample = cast(list[dict[str, object]], profiles[0]["records"])
        self.assertEqual(
            [record["phase"] for record in sample if record["source"] == "openvmm"],
            [phase for phase, *_ in self.INCIDENT_CAPTURE_PROFILE],
        )
        self.assertTrue(
            benchmark.contains_output_line(
                log, benchmark.SNAPSHOT_GUEST_DISPATCH_MARKER
            )
        )
        for line in [*records.splitlines(), committed.removesuffix(b"\n")]:
            self.assertTrue(benchmark.contains_output_line(log, line), line)

    def test_measure_once_observes_restore_marker_split_by_profile_record(self):
        record = (
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
            b"phase=guest_repair_gate exclusive=0 duration_ns=1500 "
            b"process_elapsed_ns=90000000 pid=4321\n"
        )
        console_split = threading.Event()
        stderr_written = threading.Event()

        class FakeInteraction:
            def __init__(self) -> None:
                self.process = MagicMock(pid=4321)
                self.process.poll.return_value = None

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(b"OPENVMM-SNAP")
                console_split.set()
                if stderr_written.wait(5):
                    chunks.put(b"SHOT-RESTORE-OK\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                if console_split.wait(5):
                    chunks.put(record)
                    stderr_written.set()
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self) -> None:
                pass

        profiles: list[dict[str, object]] = []
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "restore.log"
            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=FakeInteraction()
                ) as spawn,
                patch.object(benchmark, "live_peak_rss_bytes", return_value=1024),
                patch.object(benchmark, "wait_for_process_exit", return_value=0),
                patch.object(benchmark, "process_resource_counters", return_value={}),
            ):
                result = benchmark.measure_once(
                    ["openvmm"],
                    environment={},
                    timeout=10,
                    marker=benchmark.RESTORE_MARKER,
                    marker_must_be_line=True,
                    guest_exit_prequeued=True,
                    snapshot_profile=True,
                    profile_sink=profiles,
                    log_path=log_path,
                )
            log = log_path.read_bytes()

        self.assertIs(spawn.call_args.kwargs["separate_stderr"], True)
        self.assertEqual(result[1], 1024)
        records = cast(list[dict[str, object]], profiles[0]["records"])
        self.assertIn("guest_repair_gate", [item["phase"] for item in records])
        self.assertTrue(benchmark.contains_output_line(log, benchmark.RESTORE_MARKER))
        self.assertTrue(benchmark.contains_output_line(log, record.removesuffix(b"\n")))

    def test_measure_once_keeps_profile_records_observed_after_the_marker(self):
        records = (
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=startup "
            b"phase=worker_launch exclusive=0 duration_ns=2000000 "
            b"process_elapsed_ns=5000000 pid=4321\n"
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
            b"phase=device_start exclusive=1 duration_ns=300000 "
            b"process_elapsed_ns=80000000 pid=4321\n"
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
            b"phase=guest_repair_gate exclusive=0 duration_ns=1500 "
            b"process_elapsed_ns=90000000 pid=4321\n"
        )
        torn_down = threading.Event()

        class FakeInteraction:
            def __init__(self) -> None:
                self.process = MagicMock(pid=4321)
                self.process.poll.return_value = None

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(benchmark.RESTORE_MARKER + b"\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                # The stderr reader falls behind: OpenVMM's records arrive
                # only once the guest marker has been handled.
                if torn_down.wait(5):
                    chunks.put(records[:100])
                    chunks.put(records[100:])
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self) -> None:
                pass

        def counters(_pid: int) -> dict[str, int]:
            return {} if torn_down.is_set() else {"rss_bytes": 7}

        def wait_for_exit(_process: object, _timeout: float) -> int:
            torn_down.set()
            return 0

        profiles: list[dict[str, object]] = []
        with (
            patch.object(
                benchmark, "InteractiveProcess", return_value=FakeInteraction()
            ),
            patch.object(benchmark, "live_peak_rss_bytes", return_value=1024),
            patch.object(benchmark, "wait_for_process_exit", side_effect=wait_for_exit),
            patch.object(benchmark, "process_resource_counters", side_effect=counters),
        ):
            benchmark.measure_once(
                ["openvmm"],
                environment={},
                timeout=10,
                marker=benchmark.RESTORE_MARKER,
                marker_must_be_line=True,
                guest_exit_prequeued=True,
                snapshot_profile=True,
                profile_sink=profiles,
            )

        by_phase = {
            str(record["phase"]): record
            for record in cast(list[dict[str, object]], profiles[0]["records"])
        }
        self.assertLessEqual(
            {"process_startup", "worker_launch", "device_start", "guest_repair_gate"},
            set(by_phase),
        )
        self.assertEqual(by_phase["process_startup"]["duration_ns"], 3_000_000)
        # The device_start observation followed the marker, so it cannot bound
        # the interval to readiness.
        self.assertNotIn("resume_to_readiness", by_phase)
        readiness = by_phase["process_launch_to_readiness"]
        self.assertEqual(readiness["host_counters"], {"rss_bytes": 7})

    def test_measure_once_reads_output_to_its_end_before_finishing(self):
        record = (
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
            b"phase=guest_repair_gate exclusive=0 duration_ns=1500 "
            b"process_elapsed_ns=90000000 pid=4321\n"
        )
        split = record.index(b"duration_ns=") + len(b"duration_ns=")

        class FakeInteraction:
            def __init__(self, exited: threading.Event) -> None:
                self.process = MagicMock(pid=4321)
                self.process.poll.return_value = None
                self.exited = exited

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(benchmark.RESTORE_MARKER + b"\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                if self.exited.wait(5):
                    chunks.put(record[:split])
                    # The rest of the record outlasts the bounded drain that
                    # error paths use.
                    time.sleep(0.2)
                    chunks.put(record[split:])
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self) -> None:
                pass

        for profiled in (False, True):
            with self.subTest(profiled=profiled):
                exited = threading.Event()

                def wait_for_exit(
                    _process: object, _timeout: float, exited: threading.Event = exited
                ) -> int:
                    exited.set()
                    return 0

                profiles: list[dict[str, object]] = []
                with tempfile.TemporaryDirectory() as temporary:
                    log_path = Path(temporary) / "restore.log"
                    with (
                        patch.object(
                            benchmark,
                            "InteractiveProcess",
                            return_value=FakeInteraction(exited),
                        ),
                        patch.object(benchmark, "OUTPUT_DRAIN_TIMEOUT_SECONDS", 0.05),
                        patch.object(
                            benchmark, "live_peak_rss_bytes", return_value=1024
                        ),
                        patch.object(
                            benchmark,
                            "wait_for_process_exit",
                            side_effect=wait_for_exit,
                        ),
                        patch.object(
                            benchmark, "process_resource_counters", return_value={}
                        ),
                    ):
                        benchmark.measure_once(
                            ["openvmm"],
                            environment={},
                            timeout=10,
                            marker=benchmark.RESTORE_MARKER,
                            marker_must_be_line=True,
                            guest_exit_prequeued=True,
                            snapshot_profile=profiled,
                            profile_sink=profiles if profiled else None,
                            log_path=log_path,
                        )
                    log = log_path.read_bytes()

                self.assertTrue(
                    benchmark.contains_output_line(log, record.removesuffix(b"\n"))
                )
                if profiled:
                    records = cast(list[dict[str, object]], profiles[0]["records"])
                    repair = next(
                        item for item in records if item["phase"] == "guest_repair_gate"
                    )
                    self.assertEqual(repair["duration_ns"], 1500)
                    self.assertEqual(repair["process_elapsed_ns"], 90_000_000)
                else:
                    self.assertEqual(profiles, [])

    def test_measure_once_reports_output_that_does_not_reach_eof(self):
        release = threading.Event()
        self.addCleanup(release.set)

        class FakeInteraction:
            def __init__(self) -> None:
                self.process = MagicMock(pid=4321)
                self.process.poll.return_value = None

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(benchmark.RESTORE_MARKER + b"\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(
                    b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
                    b"phase=guest_repair_gate exclusive=0 duration_ns="
                )
                release.wait(10)
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self) -> None:
                pass

        profiles: list[dict[str, object]] = []
        with (
            patch.object(
                benchmark, "InteractiveProcess", return_value=FakeInteraction()
            ),
            patch.object(benchmark, "OUTPUT_DRAIN_TIMEOUT_SECONDS", 0.05),
            patch.object(benchmark, "live_peak_rss_bytes", return_value=1024),
            patch.object(benchmark, "wait_for_process_exit", return_value=0),
            patch.object(benchmark, "process_resource_counters", return_value={}),
            patch.object(benchmark, "terminate") as terminate,
            self.assertRaisesRegex(
                RuntimeError, r"did not reach EOF within 0\.5s"
            ) as raised,
        ):
            benchmark.measure_once(
                ["openvmm"],
                environment={},
                timeout=0.5,
                marker=benchmark.RESTORE_MARKER,
                marker_must_be_line=True,
                guest_exit_prequeued=True,
                snapshot_profile=True,
                profile_sink=profiles,
            )

        self.assertEqual(profiles, [])
        terminate.assert_called_once()
        self.assertIn("phase=guest_repair_gate", str(raised.exception))

    def test_guest_failure_report_includes_stderr_written_after_the_marker(self):
        terminated = threading.Event()

        class FakeInteraction:
            def __init__(self) -> None:
                self.process = MagicMock(pid=4321)
                self.process.poll.return_value = None

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(b"NVX-RESTORE-PROCESSORS-FAIL unstable-tsc\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                if terminated.wait(5):
                    chunks.put(b"WARN host diagnostics after the guest failure\n")
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self) -> None:
                pass

        def terminate_openvmm(_process: object) -> None:
            terminated.set()

        with (
            patch.object(
                benchmark, "InteractiveProcess", return_value=FakeInteraction()
            ),
            patch.object(
                benchmark, "terminate", side_effect=terminate_openvmm
            ) as terminate,
            self.assertRaises(benchmark.GuestFailureReported) as raised,
        ):
            benchmark.measure_once(
                ["openvmm"],
                environment={},
                timeout=10,
                marker=b"NVX-RESTORE-PROCESSORS-OK count=8",
                marker_must_be_line=True,
                guest_exit_prequeued=True,
                failure_marker=b"NVX-RESTORE-PROCESSORS-FAIL",
            )

        terminate.assert_called_once()
        self.assertEqual(
            raised.exception.line, "NVX-RESTORE-PROCESSORS-FAIL unstable-tsc"
        )
        self.assertIn("host diagnostics after the guest failure", str(raised.exception))

    def test_separated_output_keeps_process_output_lines_whole(self):
        record = (
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture phase=input_gate "
            b"exclusive=1 duration_ns=263787 process_elapsed_ns=1422094434 pid=1\n"
        )
        child = (
            "import os, time; "
            "os.write(1, b'NVX-S'); time.sleep(0.05); "
            f"os.write(2, {record!r}); time.sleep(0.05); "
            "os.write(1, b'NAPSHOT-DISPATCHED\\n~ # ')"
        )
        interaction = benchmark.InteractiveProcess(
            [sys.executable, "-I", "-c", child],
            dict(os.environ),
            separate_stderr=True,
        )
        stderr = bytearray()
        try:
            output = benchmark.SeparatedOutput(interaction)
            deadline = time.monotonic() + 30
            while not output.closed:
                stream, chunk = output.get(max(0.0, deadline - time.monotonic()))
                if stream == "stderr" and chunk is not None:
                    stderr.extend(chunk)
            self.assertEqual(interaction.process.wait(timeout=30), 0)
        finally:
            benchmark.terminate(interaction.process)
            interaction.close()

        marker = benchmark.SNAPSHOT_GUEST_DISPATCH_MARKER
        self.assertTrue(benchmark.contains_output_line(output.console, marker))
        self.assertNotIn(b"OPENVMM_SNAPSHOT_PROFILE_V1", output.console)
        self.assertIn(record, bytes(stderr))
        contents = output.contents()
        self.assertTrue(benchmark.contains_output_line(contents, marker))
        self.assertTrue(
            benchmark.contains_output_line(contents, record.removesuffix(b"\n"))
        )
        self.assertTrue(contents.endswith(b"~ # "))

    def test_interactive_process_gives_stderr_its_own_pipe_on_request(self):
        for platform in ("linux", "win32"):
            for separate_stderr in (False, True):
                with self.subTest(platform=platform, separate=separate_stderr):
                    terminal: list[int] = []

                    def open_terminal(
                        terminal: list[int] = terminal,
                    ) -> tuple[int, int]:
                        terminal.extend(os.pipe())
                        return terminal[0], terminal[1]

                    process = MagicMock(pid=4321)
                    if not separate_stderr:
                        process.stderr = None
                    with (
                        patch.object(benchmark.sys, "platform", platform),
                        patch.object(
                            benchmark.os,
                            "openpty",
                            side_effect=open_terminal,
                            create=True,
                        ),
                        patch.object(
                            benchmark.subprocess, "Popen", return_value=process
                        ) as popen,
                    ):
                        interaction = benchmark.InteractiveProcess(
                            ["openvmm"], {}, separate_stderr=separate_stderr
                        )
                    try:
                        stderr = popen.call_args.kwargs["stderr"]
                        if separate_stderr:
                            self.assertEqual(stderr, subprocess.PIPE)
                        elif platform == "linux":
                            self.assertEqual(stderr, terminal[1])
                        else:
                            self.assertEqual(stderr, subprocess.STDOUT)
                        if not separate_stderr:
                            chunks: queue.Queue[bytes | None] = queue.Queue()
                            with self.assertRaisesRegex(
                                RuntimeError, "shares the console stream"
                            ):
                                interaction.read_stderr(chunks)
                            self.assertIsNone(chunks.get_nowait())
                    finally:
                        interaction.close()
                    if separate_stderr:
                        process.stderr.close.assert_called_once_with()

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

            def read_stderr(self, chunks: queue.Queue[bytes | None]):
                chunks.put(None)

            def write_input(self, data: bytes):
                self.writes.append(data)

            def close(self):
                pass

        interaction = FakeInteraction()
        with (
            patch.object(benchmark, "InteractiveProcess", return_value=interaction),
            patch.object(benchmark, "live_peak_rss_bytes", return_value=1024),
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

    def test_measure_once_reports_missing_rss_when_openvmm_exits_first(self):
        class FakeProcess:
            pid = 123

            def __init__(self, status: int):
                self.status = status
                self.exited = False
                self.returncode: int | None = None

            def poll(self):
                if self.exited:
                    self.returncode = self.status
                return self.returncode

            def wait(self, timeout: float | None = None) -> int:
                del timeout
                return self.status

            def terminate(self):
                raise AssertionError("unexpected process termination")

        class FakeInteraction:
            def __init__(self, status: int):
                self.process = FakeProcess(status)

            def read_output(self, chunks: queue.Queue[bytes | None]):
                self.process.exited = True
                chunks.put(benchmark.RESTORE_MARKER + b"\n")

            def read_stderr(self, chunks: queue.Queue[bytes | None]):
                chunks.put(None)

            def write_input(self, data: bytes):
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self):
                pass

        for status in (0, 1):
            with self.subTest(status=status):
                interaction = FakeInteraction(status)

                def linux_peak_rss_bytes(
                    pid: int, interaction: FakeInteraction = interaction
                ) -> int | None:
                    self.assertEqual(pid, 123)
                    # A zombie's /proc status no longer reports VmHWM.
                    return None if interaction.process.exited else 1024

                with (
                    patch.object(
                        benchmark, "InteractiveProcess", return_value=interaction
                    ),
                    patch.object(
                        benchmark,
                        "_linux_live_peak_rss_bytes",
                        side_effect=linux_peak_rss_bytes,
                    ),
                    patch.object(benchmark, "windows_peak_rss_bytes", return_value=1),
                    patch.object(
                        benchmark, "wait_for_process_exit", return_value=status
                    ) as wait,
                    patch.object(
                        benchmark.time,
                        "perf_counter_ns",
                        side_effect=[0, 10_000_000, 17_000_000],
                    ),
                ):
                    if status == 0:
                        result = benchmark.measure_once(
                            ["openvmm"],
                            environment={},
                            timeout=1,
                            marker=benchmark.RESTORE_MARKER,
                            marker_must_be_line=True,
                            guest_exit_prequeued=True,
                        )
                        self.assertEqual(result, (10.0, None, 7.0, 17.0))
                    else:
                        with self.assertRaisesRegex(
                            RuntimeError, "status 1 during teardown"
                        ):
                            benchmark.measure_once(
                                ["openvmm"],
                                environment={},
                                timeout=1,
                                marker=benchmark.RESTORE_MARKER,
                                marker_must_be_line=True,
                                guest_exit_prequeued=True,
                            )

                wait.assert_called_once_with(
                    interaction.process, benchmark.TEARDOWN_TIMEOUT_SECONDS
                )

    def test_completed_output_line_with_prefix_requires_a_whole_line(self):
        prefix = b"NVX-RESTORE-PROCESSORS-FAIL"
        self.assertIsNone(
            benchmark.completed_output_line_with_prefix(
                b"NVX-RESTORE-PROCESSORS-FAIL unst", prefix
            )
        )
        self.assertIsNone(
            benchmark.completed_output_line_with_prefix(
                b'> echo "NVX-RESTORE-PROCESSORS-FAIL count=$n"\r\n', prefix
            )
        )
        self.assertEqual(
            benchmark.completed_output_line_with_prefix(
                b"guest\r\nNVX-RESTORE-PROCESSORS-FAIL unstable-tsc\r\nmore", prefix
            ),
            b"NVX-RESTORE-PROCESSORS-FAIL unstable-tsc",
        )

    def test_measure_once_stops_at_a_completed_guest_failure_marker(self):
        class FakeInteraction:
            def __init__(self):
                self.process = MagicMock(pid=123)
                self.process.poll.return_value = None

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(b'> echo "NVX-RESTORE-PROCESSORS-FAIL count=$n"\r\n')
                chunks.put(b"Measured 6 cycles TSC warp between CPUs\r\n")
                chunks.put(b"NVX-RESTORE-PROCESSORS-FAIL unst")
                chunks.put(b"able-tsc\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]):
                chunks.put(None)

            def write_input(self, data: bytes):
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self):
                pass

        interaction = FakeInteraction()
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "restore.log"
            with (
                patch.object(benchmark, "InteractiveProcess", return_value=interaction),
                patch.object(benchmark, "terminate") as terminate,
                patch.object(benchmark, "wait_for_process_exit") as wait,
            ):
                with self.assertRaises(benchmark.GuestFailureReported) as raised:
                    benchmark.measure_once(
                        ["openvmm"],
                        environment={},
                        timeout=60,
                        marker=b"NVX-RESTORE-PROCESSORS-OK count=8",
                        marker_must_be_line=True,
                        guest_exit_prequeued=True,
                        log_path=log_path,
                        failure_marker=b"NVX-RESTORE-PROCESSORS-FAIL",
                    )
            log = log_path.read_bytes()

        self.assertEqual(
            raised.exception.line, "NVX-RESTORE-PROCESSORS-FAIL unstable-tsc"
        )
        self.assertIn("TSC warp", raised.exception.output_tail)
        self.assertIn("--- OpenVMM output ---", str(raised.exception))
        terminate.assert_called_once_with(interaction.process)
        wait.assert_not_called()
        self.assertTrue(log.endswith(b"NVX-RESTORE-PROCESSORS-FAIL unstable-tsc\r\n"))

    def test_guest_runner_preserves_primary_and_close_failures_with_bounded_tail(self):
        class FakeProcess:
            pid = 123

            def poll(self):
                return 7

            def wait(self):
                return 7

        class FakeInteraction:
            process = FakeProcess()
            containment = None

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(benchmark.BOOT_MARKER + b"\n" + b"x" * 5000 + b"\nDONE\n")
                chunks.put(None)

            def write_input(self, _data: bytes):
                pass

            def close(self):
                raise OSError("close failed")

        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "guest.log"
            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=FakeInteraction()
                ),
                patch.object(benchmark, "_try_peak_rss", return_value=1024),
                patch.object(benchmark, "terminate") as terminate,
            ):
                with self.assertRaisesRegex(
                    RuntimeError, "OpenVMM exited with status 7"
                ) as raised:
                    benchmark.run_guest_script(
                        ["openvmm"],
                        "guest command\n",
                        b"DONE",
                        timeout=1,
                        log_path=log_path,
                    )

            error = raised.exception
            cleanup_errors = cast(
                tuple[BaseException, ...],
                error.cleanup_errors,  # type: ignore[attr-defined]
            )
            self.assertEqual([str(item) for item in cleanup_errors], ["close failed"])
            self.assertIn("--- OpenVMM output ---", str(error))
            self.assertIn("--- cleanup failures ---", str(error))
            self.assertNotIn(benchmark.BOOT_MARKER.decode(), str(error))
            self.assertGreater(len(log_path.read_bytes()), 4096)
            self.assertTrue(log_path.read_bytes().endswith(b"\nDONE\n"))
            terminate.assert_called_once()

    def test_guest_runner_propagates_success_only_close_failure(self):
        class FakeProcess:
            pid = 123

            def poll(self):
                return 0

            def wait(self):
                return 0

        class FakeInteraction:
            process = FakeProcess()
            containment = None

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(benchmark.BOOT_MARKER + b"\nDONE\n")
                chunks.put(None)

            def write_input(self, _data: bytes):
                pass

            def close(self):
                raise OSError("close after success")

        with (
            patch.object(
                benchmark, "InteractiveProcess", return_value=FakeInteraction()
            ),
            patch.object(benchmark, "_try_peak_rss", return_value=1024),
        ):
            with self.assertRaisesRegex(OSError, "close after success"):
                benchmark.run_guest_script(
                    ["openvmm"],
                    "guest command\n",
                    b"DONE",
                    timeout=1,
                )

    def test_live_peak_rss_samples_linux_process_without_reaping(self):
        process = MagicMock(pid=123)
        with (
            patch.object(benchmark.os, "name", "posix"),
            patch.object(benchmark.sys, "platform", "linux"),
            patch.object(
                benchmark, "_linux_live_peak_rss_bytes", side_effect=[4096, None, 0]
            ) as read,
        ):
            self.assertEqual(benchmark.live_peak_rss_bytes(process), 4096)
            self.assertIsNone(benchmark.live_peak_rss_bytes(process))
            with self.assertRaisesRegex(RuntimeError, "peak RSS 0 bytes"):
                benchmark.live_peak_rss_bytes(process)

        self.assertEqual(read.call_args_list, [call(123)] * 3)
        process.poll.assert_not_called()

    def test_live_peak_rss_accepts_only_running_windows_samples(self):
        running = MagicMock(pid=7)
        running.poll.return_value = None
        exited = MagicMock(pid=8)
        exited.poll.return_value = 0
        racing = MagicMock(pid=9)
        racing.poll.return_value = None

        def read_counters(pid: int) -> int:
            if pid == racing.pid:
                racing.poll.return_value = 0
            return 8192

        with (
            patch.object(benchmark.os, "name", "nt"),
            patch.object(
                benchmark, "windows_peak_rss_bytes", side_effect=read_counters
            ) as read,
        ):
            self.assertEqual(benchmark.live_peak_rss_bytes(running), 8192)
            # A retained handle still reports counters, including teardown.
            self.assertIsNone(benchmark.live_peak_rss_bytes(exited))
            self.assertIsNone(benchmark.live_peak_rss_bytes(racing))

        self.assertEqual(read.call_args_list, [call(7), call(8), call(9)])
        with (
            patch.object(benchmark.os, "name", "nt"),
            patch.object(
                benchmark, "windows_peak_rss_bytes", side_effect=OSError("closed")
            ),
        ):
            self.assertIsNone(benchmark.live_peak_rss_bytes(exited))
            with self.assertRaisesRegex(OSError, "closed"):
                benchmark.live_peak_rss_bytes(running)

    def test_linux_live_peak_rss_requires_process_address_space(self):
        running = "Name:\topenvmm\nState:\tS (sleeping)\nVmHWM:\t   59008 kB\n"
        zombie = "Name:\topenvmm\nState:\tZ (zombie)\nThreads:\t1\n"
        with patch.object(
            Path,
            "read_text",
            autospec=True,
            side_effect=[running, zombie, FileNotFoundError(), ProcessLookupError()],
        ) as read:
            self.assertEqual(benchmark._linux_live_peak_rss_bytes(123), 59008 * 1024)
            for _ in range(3):
                self.assertIsNone(benchmark._linux_live_peak_rss_bytes(123))

        self.assertEqual(read.call_args.args[0], Path("/proc/123/status"))
        with (
            patch.object(Path, "read_text", side_effect=PermissionError()),
            self.assertRaises(PermissionError),
        ):
            benchmark._linux_live_peak_rss_bytes(123)

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

    def test_mshv_and_whp_capture_after_linux_leaves_tsc_early(self):
        wait = benchmark.stable_clocksource_wait_script()
        for backend, waits in (("mshv", True), ("whp", True), ("kvm", False)):
            with self.subTest(backend=backend):
                script = benchmark.prepare_snapshot_capture_script(
                    1, backend=backend, teardown_mode="guest-exit"
                )
                probe = script.split("<<'NVX_SMP_PROBE_SCRIPT'\n", 1)[1]
                probe = probe.split("NVX_SMP_PROBE_SCRIPT\n", 1)[0]
                self.assertEqual(probe.startswith(wait), waits)
                self.assertEqual("SMP-CLOCKSOURCE-FAIL expected=stable" in probe, waits)
                if waits:
                    # The wait and its check run before the probe completes, so
                    # the host requests the snapshot only after tsc-early is gone.
                    self.assertLess(
                        probe.index("SMP-CLOCKSOURCE-FAIL"),
                        probe.index("NVX-SMP-PROBE-OK"),
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
                "peak_rss_remeasured_count": 0,
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

    def test_archive_release_command_forwards_paths(self):
        args = nvx.parse_args(
            [
                "archive-release",
                "--source",
                "dist/package",
                "--destination",
                "dist/package.zip",
            ]
        )

        with patch.object(nvx, "create_release_archive") as create_archive:
            args.handler(args)

        create_archive.assert_called_once_with(
            Path("dist/package"),
            Path("dist/package.zip"),
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
    def test_alpine_source_validation_rejects_malformed_manifest(self):
        with tempfile.TemporaryDirectory() as temporary:
            source_dir = Path(temporary)
            alpine_dir = source_dir / "alpine"
            alpine_dir.mkdir()
            (alpine_dir / "manifest.json").write_text("{", encoding="utf-8")

            with (
                patch.object(BuildConstants, "SOURCE_DIR", source_dir),
                self.assertRaisesRegex(
                    common.ScriptError,
                    "invalid collected Alpine source manifest",
                ),
            ):
                release._validate_alpine_sources([])

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
                        "name": "nvx-invalid-linux-kvm.tar.gz",
                        "url": "https://api.example.invalid/invalid",
                        "size": True,
                    },
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
            "nvx_tools.release.urllib.request.OpenerDirector.open",
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
            patch(
                "nvx_tools.release.urllib.request.OpenerDirector.open",
                side_effect=error,
            ),
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
            patch(
                "nvx_tools.release.urllib.request.OpenerDirector.open",
                side_effect=error,
            ),
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
            patch(
                "nvx_tools.release.urllib.request.OpenerDirector.open",
                side_effect=error,
            ),
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
                "nvx_tools.release.urllib.request.OpenerDirector.open",
                side_effect=[error, public_response],
            ) as opener_open,
            patch("sys.stderr", io.StringIO()) as stderr,
        ):
            asset, download_token = release._latest_release_asset_with_fallback(
                "example/nvx",
                "linux-kvm",
                "token",
            )

        self.assertEqual(opener_open.call_count, 2)
        authenticated_request = opener_open.call_args_list[0].args[0]
        public_request = opener_open.call_args_list[1].args[0]
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
                patch.object(BuildConstants, "REPO_ROOT", root),
                patch.object(BuildConstants, "SOURCE_DIR", source_dir),
                patch.object(
                    UbuntuBuildConstants,
                    "PACKAGE_LOCK",
                    root / "ubuntu" / "packages.lock.json",
                ),
                patch.object(OpenVMMBuildConstants, "DIRECTORY", openvmm_dir),
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
                    "openvmm_git_state",
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
            for name in ReleaseBuildConstants.GUEST_ARTIFACT_NAMES:
                self.assertTrue((destination / "guest" / name).is_file())
            self.assertTrue(
                (
                    destination / "provenance" / OpenVMMBuildConstants.PROVENANCE_NAME
                ).is_file()
            )
            self.assertTrue(
                (
                    destination / "provenance" / KernelBuildConstants.PROVENANCE_NAME
                ).is_file()
            )
            self.assertTrue(
                (
                    destination / "provenance" / InitramfsBuildConstants.PROVENANCE_NAME
                ).is_file()
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
            self.assertEqual(
                manifest["azurelinux"]["initramfs_sha256"],
                common.sha256_file(
                    destination / "guest" / AzureLinuxBuildConstants.INITRAMFS_NAME
                ),
            )
            self.assertEqual(
                manifest["azurelinux"]["initramfs_package_manifest_sha256"],
                common.sha256_file(
                    destination
                    / "guest"
                    / AzureLinuxBuildConstants.PACKAGE_MANIFEST_NAME
                ),
            )
            common.verify_sha256_sums(destination)
            self.assertIn("binary-only package", stderr.getvalue())

    def test_source_package_omits_azure_linux_artifacts(self):
        for azurelinux_built in (True, False):
            with (
                self.subTest(azurelinux_built=azurelinux_built),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                paths, kernel_inputs, revision = _write_release_fixture(root)
                build_dir = paths["build"]
                if not azurelinux_built:
                    for name in ReleaseBuildConstants.AZURELINUX_ARTIFACT_NAMES:
                        (build_dir / name).unlink()
                self._package_source_release_without_azure_linux(
                    root, paths, kernel_inputs, revision
                )

    def _package_source_release_without_azure_linux(
        self,
        root: Path,
        paths: dict[str, Path],
        kernel_inputs: dict[str, object],
        revision: str,
    ) -> None:
        build_dir = paths["build"]
        source_dir = paths["source"]
        openvmm_dir = paths["openvmm"]
        binary = paths["binary"]
        destination = root / "staged"
        stderr = io.StringIO()
        linux_source_archive = (
            source_dir
            / KernelBuildConstants.SOURCE_DIRECTORY_NAME
            / KernelBuildConstants.SOURCE_ARCHIVE_NAME
        )
        linux_source_archive.parent.mkdir(parents=True, exist_ok=True)
        linux_source_archive.write_bytes(b"linux-source")

        def artifact_path(name: str) -> Path:
            return build_dir / name

        def write_source_archive(output: Path, *_args: object) -> None:
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_bytes(output.name.encode("ascii"))

        with (
            patch.object(BuildConstants, "REPO_ROOT", root),
            patch.object(BuildConstants, "SOURCE_DIR", source_dir),
            patch.object(
                UbuntuBuildConstants,
                "PACKAGE_LOCK",
                root / "ubuntu" / "packages.lock.json",
            ),
            patch.object(OpenVMMBuildConstants, "DIRECTORY", openvmm_dir),
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
                "openvmm_git_state",
                return_value=(revision, True),
            ),
            patch.object(release, "_validate_alpine_sources"),
            patch.object(release, "_validate_ubuntu_sources"),
            patch.object(release, "_validate_linux_source_archive"),
            patch.object(
                release,
                "_project_source_archive",
                side_effect=write_source_archive,
            ) as project_source_archive,
            patch.object(
                release,
                "_alpine_source_archive",
                side_effect=write_source_archive,
            ),
            patch.object(
                release,
                "_ubuntu_source_archive",
                side_effect=write_source_archive,
            ),
            patch("sys.stderr", stderr),
        ):
            release.package_release(
                version="1.0.0",
                destination=destination,
                include_source=True,
                force=False,
            )

        for name in ReleaseBuildConstants.AZURELINUX_ARTIFACT_NAMES:
            self.assertFalse((destination / "guest" / name).exists())
        for name in ReleaseBuildConstants.GUEST_ARTIFACT_NAMES:
            if name in ReleaseBuildConstants.AZURELINUX_ARTIFACT_NAMES:
                continue
            self.assertTrue((destination / "guest" / name).is_file())
        self.assertNotIn(
            build_dir / AzureLinuxBuildConstants.PACKAGE_MANIFEST_NAME,
            project_source_archive.call_args.args[2],
        )
        manifest = json.loads(
            (destination / "SOURCE-MANIFEST.json").read_text(encoding="utf-8")
        )
        self.assertNotIn("azurelinux", manifest)
        common.verify_sha256_sums(destination)
        self.assertIn("omits the Azure Linux guest", stderr.getvalue())

    def test_runtime_provenance_validation_uses_required_paths(self):
        build_dir = Path("build")
        binary = Path("openvmm")

        def artifact_path(name: str) -> Path:
            return build_dir / name

        def require_file(path: Path, _description: str) -> Path:
            return path

        with (
            patch.object(release, "artifact_path", side_effect=artifact_path),
            patch.object(release, "openvmm_binary_path", return_value=binary),
            patch.object(
                release,
                "require_file",
                side_effect=require_file,
            ) as require,
            patch.object(release, "_validate_openvmm_provenance") as openvmm,
            patch.object(release, "_validate_kernel_provenance"),
            patch.object(release, "_validate_initramfs_provenance"),
        ):
            release.validate_runtime_artifact_provenance()

        self.assertEqual(
            require.call_args_list[-3:],
            [
                call(
                    build_dir / OpenVMMBuildConstants.PROVENANCE_NAME,
                    "OpenVMM build provenance",
                ),
                call(
                    build_dir / KernelBuildConstants.PROVENANCE_NAME,
                    "kernel build provenance",
                ),
                call(
                    build_dir / InitramfsBuildConstants.PROVENANCE_NAME,
                    "initramfs build provenance",
                ),
            ],
        )
        openvmm.assert_called_once_with(
            binary, build_dir / OpenVMMBuildConstants.PROVENANCE_NAME
        )

    def test_package_rejects_dirty_openvmm_provenance(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths, _kernel_inputs, revision = _write_release_fixture(root)

            with (
                patch.object(
                    release,
                    "openvmm_git_state",
                    return_value=(revision, False),
                ),
                self.assertRaisesRegex(
                    common.ScriptError,
                    "current clean pinned source",
                ),
            ):
                release._validate_openvmm_provenance(
                    paths["binary"],
                    paths["build"] / OpenVMMBuildConstants.PROVENANCE_NAME,
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
                    paths["build"] / KernelBuildConstants.PROVENANCE_NAME,
                )

    def test_package_rejects_stale_initramfs_provenance(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths, _kernel_inputs, _revision = _write_release_fixture(root)
            initramfs = paths["build"] / "initramfs.cpio.gz"
            initramfs.write_bytes(b"new initramfs bytes")

            with self.assertRaisesRegex(
                common.ScriptError,
                "initramfs build provenance",
            ):
                release._validate_initramfs_provenance(
                    initramfs,
                    paths["build"] / "initramfs.cpio.gz.packages.json",
                    paths["build"] / InitramfsBuildConstants.PROVENANCE_NAME,
                )

    def test_package_rejects_stale_initramfs_package_manifest_provenance(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths, _kernel_inputs, _revision = _write_release_fixture(root)
            package_manifest = paths["build"] / "initramfs.cpio.gz.packages.json"
            package_manifest.write_bytes(b"new package manifest bytes")

            with self.assertRaisesRegex(
                common.ScriptError,
                "initramfs build provenance",
            ):
                release._validate_initramfs_provenance(
                    paths["build"] / "initramfs.cpio.gz",
                    package_manifest,
                    paths["build"] / InitramfsBuildConstants.PROVENANCE_NAME,
                )

    def test_guest_release_inputs_reject_stale_ubuntu_artifacts(self):
        for artifact_name in (
            "initramfs-ubuntu.cpio.gz",
            "ubuntu-distro.erofs",
        ):
            with (
                self.subTest(artifact=artifact_name),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                paths, _kernel_inputs, _revision = _write_release_fixture(root)
                build_dir = paths["build"]
                (build_dir / artifact_name).write_bytes(b"stale artifact")
                with (
                    patch.object(
                        release,
                        "artifact_path",
                        side_effect=build_dir.joinpath,
                    ),
                    self.assertRaisesRegex(
                        common.ScriptError,
                        "Ubuntu artifact manifest does not match",
                    ),
                ):
                    release._guest_release_inputs(include_azurelinux=True)

    def test_guest_release_inputs_reject_stale_azurelinux_initramfs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths, _kernel_inputs, _revision = _write_release_fixture(root)
            build_dir = paths["build"]
            (build_dir / AzureLinuxBuildConstants.INITRAMFS_NAME).write_bytes(
                b"stale artifact"
            )
            with (
                patch.object(
                    release,
                    "artifact_path",
                    side_effect=build_dir.joinpath,
                ),
                self.assertRaisesRegex(
                    common.ScriptError,
                    "Azure Linux initramfs manifest is invalid",
                ),
            ):
                release._guest_release_inputs(include_azurelinux=True)

    def test_guest_release_inputs_reject_unpinned_azurelinux_image(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths, _kernel_inputs, _revision = _write_release_fixture(root)
            build_dir = paths["build"]
            manifest_path = build_dir / AzureLinuxBuildConstants.PACKAGE_MANIFEST_NAME
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest["image"] = "mcr.microsoft.com/azurelinux/base/core@sha256:" + (
                "0" * 64
            )
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            with (
                patch.object(
                    release,
                    "artifact_path",
                    side_effect=build_dir.joinpath,
                ),
                self.assertRaisesRegex(
                    common.ScriptError,
                    "Azure Linux initramfs manifest is invalid",
                ),
            ):
                release._guest_release_inputs(include_azurelinux=True)

    def test_guest_release_inputs_reject_stale_azurelinux_build_inputs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths, _kernel_inputs, _revision = _write_release_fixture(root)
            build_dir = paths["build"]
            manifest_path = build_dir / AzureLinuxBuildConstants.PACKAGE_MANIFEST_NAME
            current_manifest = manifest_path.read_text(encoding="utf-8")
            with (
                patch.object(BuildConstants, "REPO_ROOT", root),
                patch.object(
                    UbuntuBuildConstants,
                    "PACKAGE_LOCK",
                    root / "ubuntu" / "packages.lock.json",
                ),
                patch.object(
                    release,
                    "artifact_path",
                    side_effect=build_dir.joinpath,
                ),
            ):
                release._guest_release_inputs(include_azurelinux=True)
                with self.subTest(case="missing digest"):
                    document = json.loads(current_manifest)
                    del document["input_sha256"]
                    manifest_path.write_text(json.dumps(document), encoding="utf-8")
                    with self.assertRaisesRegex(
                        common.ScriptError,
                        "Azure Linux initramfs manifest does not match the "
                        "current build inputs",
                    ):
                        release._guest_release_inputs(include_azurelinux=True)
                manifest_path.write_text(current_manifest, encoding="utf-8")
                with self.subTest(case="edited Dockerfile"):
                    dockerfile = root / "docker" / "Dockerfile"
                    dockerfile.write_text(
                        dockerfile.read_text(encoding="utf-8") + "# changed\n",
                        encoding="utf-8",
                    )
                    with self.assertRaisesRegex(
                        common.ScriptError,
                        "Azure Linux initramfs manifest does not match the "
                        "current build inputs",
                    ):
                        release._guest_release_inputs(include_azurelinux=True)

    def test_guest_release_inputs_require_azurelinux_only_when_included(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths, _kernel_inputs, _revision = _write_release_fixture(root)
            build_dir = paths["build"]
            for name in ReleaseBuildConstants.AZURELINUX_ARTIFACT_NAMES:
                (build_dir / name).unlink()
            with patch.object(
                release,
                "artifact_path",
                side_effect=build_dir.joinpath,
            ):
                guest_names, _alpine, _ubuntu, azurelinux_manifests = (
                    release._guest_release_inputs(include_azurelinux=False)
                )
                with self.assertRaisesRegex(
                    common.ScriptError,
                    "required guest artifact initramfs-azurelinux.cpio.gz not found",
                ):
                    release._guest_release_inputs(include_azurelinux=True)

        self.assertEqual(
            guest_names,
            [
                name
                for name in ReleaseBuildConstants.GUEST_ARTIFACT_NAMES
                if name not in ReleaseBuildConstants.AZURELINUX_ARTIFACT_NAMES
            ],
        )
        self.assertEqual(azurelinux_manifests, [])

    def test_guest_release_inputs_reject_stale_ubuntu_source_inputs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths, _kernel_inputs, _revision = _write_release_fixture(root)
            build_dir = paths["build"]
            manifest_path = build_dir / "initramfs-ubuntu.cpio.gz.packages.json"
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest["input_sha256"] = "0" * 64
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            with (
                patch.object(
                    release,
                    "artifact_path",
                    side_effect=build_dir.joinpath,
                ),
                self.assertRaisesRegex(
                    common.ScriptError,
                    "Ubuntu artifact manifest does not match",
                ),
            ):
                release._guest_release_inputs(include_azurelinux=True)

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
                    patch.object(BuildConstants, "REPO_ROOT", root),
                    patch.object(BuildConstants, "SOURCE_DIR", paths["source"]),
                    patch.object(
                        UbuntuBuildConstants,
                        "PACKAGE_LOCK",
                        root / "ubuntu" / "packages.lock.json",
                    ),
                    patch.object(OpenVMMBuildConstants, "DIRECTORY", paths["openvmm"]),
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
                        "openvmm_git_state",
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
                patch.object(BuildConstants, "REPO_ROOT", root),
                patch.object(BuildConstants, "SOURCE_DIR", paths["source"]),
                patch.object(
                    UbuntuBuildConstants,
                    "PACKAGE_LOCK",
                    root / "ubuntu" / "packages.lock.json",
                ),
                patch.object(OpenVMMBuildConstants, "DIRECTORY", paths["openvmm"]),
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
                    "openvmm_git_state",
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
            root = Path(temporary).resolve()
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
                patch.object(BuildConstants, "REPO_ROOT", root),
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
            root = Path(temporary).resolve()
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
                patch.object(BuildConstants, "REPO_ROOT", root),
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
                patch.object(BuildConstants, "REPO_ROOT", root),
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
            for name in ReleaseBuildConstants.GUEST_ARTIFACT_NAMES:
                guest = package_root / "guest" / name
                guest.parent.mkdir(parents=True, exist_ok=True)
                guest.write_bytes(name.encode("ascii"))
            for name in (
                OpenVMMBuildConstants.PROVENANCE_NAME,
                KernelBuildConstants.PROVENANCE_NAME,
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
            for name in ReleaseBuildConstants.GUEST_ARTIFACT_NAMES:
                self.assertEqual(
                    (build_dir / name).read_bytes(),
                    name.encode("ascii"),
                )
            for name in (
                OpenVMMBuildConstants.PROVENANCE_NAME,
                KernelBuildConstants.PROVENANCE_NAME,
            ):
                self.assertEqual(
                    (build_dir / name).read_bytes(),
                    name.encode("ascii"),
                )

    def test_source_release_archive_removes_stale_azure_artifacts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            package_root = root / "package" / "nvx-1.2.3-test"
            binary_name = "openvmm.exe" if os.name == "nt" else "openvmm"
            binary = package_root / "bin" / binary_name
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"openvmm")
            for name in ReleaseBuildConstants.GUEST_ARTIFACT_NAMES:
                if name in ReleaseBuildConstants.AZURELINUX_ARTIFACT_NAMES:
                    continue
                guest = package_root / "guest" / name
                guest.parent.mkdir(parents=True, exist_ok=True)
                guest.write_bytes(name.encode("ascii"))
            for name in (
                OpenVMMBuildConstants.PROVENANCE_NAME,
                KernelBuildConstants.PROVENANCE_NAME,
            ):
                provenance = package_root / "provenance" / name
                provenance.parent.mkdir(parents=True, exist_ok=True)
                provenance.write_bytes(name.encode("ascii"))
            (package_root / "SOURCE-MANIFEST.json").write_text(
                json.dumps({"linux": {}, "alpine": {}, "ubuntu": {}}),
                encoding="utf-8",
            )
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
            build_dir.mkdir(parents=True)
            for name in ReleaseBuildConstants.GUEST_ARTIFACT_NAMES:
                (build_dir / name).write_bytes(b"stale " + name.encode("ascii"))

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
            for name in ReleaseBuildConstants.GUEST_ARTIFACT_NAMES:
                destination = build_dir / name
                if name in ReleaseBuildConstants.AZURELINUX_ARTIFACT_NAMES:
                    self.assertFalse(destination.exists())
                else:
                    self.assertEqual(destination.read_bytes(), name.encode("ascii"))


class PositiveIntTests(unittest.TestCase):
    def test_accepts_positive_integer(self):
        self.assertEqual(common.positive_int("1"), 1)

    def test_rejects_zero_and_negative_values(self):
        for value in ("0", "-1"):
            with self.subTest(value=value):
                with self.assertRaisesRegex(
                    argparse.ArgumentTypeError, "^must be greater than zero$"
                ):
                    common.positive_int(value)

    def test_rejects_non_integer_input(self):
        with self.assertRaises(ValueError):
            common.positive_int("not-an-int")


class GitOutputTests(unittest.TestCase):
    def test_runs_git_from_repository_root_and_strips_output(self):
        completed = MagicMock(stdout=" output \n")
        with patch.object(common.subprocess, "run", return_value=completed) as run:
            self.assertEqual(common.git_output("status", "--short"), "output")
        run.assert_called_once_with(
            ["git", "-C", str(BuildConstants.REPO_ROOT), "status", "--short"],
            check=True,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="strict",
            timeout=30.0,
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

    def test_checksum_manifest_rejects_unlisted_tampered_provenance(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            payload = root / "bin" / "openvmm"
            provenance = root / "provenance" / "openvmm.provenance.json"
            payload.parent.mkdir()
            provenance.parent.mkdir()
            payload.write_bytes(b"openvmm")
            provenance.write_bytes(b"original provenance")
            common.write_sha256_sums(root)
            checksum_file = root / "SHA256SUMS"
            checksum_file.write_text(
                "\n".join(
                    line
                    for line in checksum_file.read_text(encoding="ascii").splitlines()
                    if not line.endswith("provenance/openvmm.provenance.json")
                )
                + "\n",
                encoding="ascii",
            )
            provenance.write_bytes(b"tampered provenance")

            with self.assertRaisesRegex(
                common.ScriptError,
                "unlisted file.*openvmm.provenance.json",
            ):
                common.verify_sha256_sums(root)

    def test_checksum_manifest_rejects_missing_and_duplicate_paths(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            payload = root / "payload"
            payload.write_bytes(b"payload")
            common.write_sha256_sums(root)
            checksum_file = root / "SHA256SUMS"
            line = checksum_file.read_text(encoding="ascii")

            payload.unlink()
            with self.assertRaisesRegex(common.ScriptError, "invalid checksum path"):
                common.verify_sha256_sums(root)

            payload.write_bytes(b"payload")
            checksum_file.write_text(line + line, encoding="ascii")
            with self.assertRaisesRegex(common.ScriptError, "duplicate checksum path"):
                common.verify_sha256_sums(root)

    def test_checksum_manifest_rejects_non_ascii_text(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "SHA256SUMS").write_bytes(b"\xff")

            with self.assertRaisesRegex(common.ScriptError, "only ASCII text"):
                common.verify_sha256_sums(root)

    def test_checksum_manifest_rejects_unsafe_paths_and_symlinks(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            payload = root / "payload"
            payload.write_bytes(b"payload")
            common.write_sha256_sums(root)
            checksum_file = root / "SHA256SUMS"
            checksum = hashlib.sha256(b"payload").hexdigest()
            checksum_file.write_text(
                f"{checksum}  ../payload\n",
                encoding="ascii",
            )
            with self.assertRaisesRegex(common.ScriptError, "malformed checksum line"):
                common.verify_sha256_sums(root)

            common.write_sha256_sums(root)
            link = root / "payload-link"
            try:
                link.symlink_to(payload)
            except OSError as error:
                self.skipTest(f"symlinks are unavailable: {error}")
            with self.assertRaisesRegex(common.ScriptError, "symlink is not allowed"):
                common.verify_sha256_sums(root)

    def test_release_archives_are_reproducible_with_fixed_layout_and_modes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "nvx-1.0.0-test"
            executable = source / "bin" / "openvmm"
            data = source / "provenance" / "openvmm.provenance.json"
            executable.parent.mkdir(parents=True)
            data.parent.mkdir()
            executable.write_bytes(b"openvmm")
            data.write_bytes(b"provenance")
            executable.chmod(0o755)
            data.chmod(0o644)
            common.write_sha256_sums(source)

            for suffix in (".tar.gz", ".zip"):
                with self.subTest(suffix=suffix):
                    first = root / f"first{suffix}"
                    second = root / f"second{suffix}"
                    release.create_release_archive(source, first)
                    os.utime(executable, (1000, 1000))
                    release.create_release_archive(source, second)
                    self.assertEqual(first.read_bytes(), second.read_bytes())

                    if suffix == ".tar.gz":
                        with tarfile.open(first, "r:gz") as package:
                            member_list = package.getmembers()
                            members = {member.name: member for member in member_list}
                        self.assertEqual(
                            [member.name for member in member_list],
                            sorted(member.name for member in member_list),
                        )
                        self.assertEqual(
                            {member.name for member in member_list if member.isfile()},
                            {
                                f"{source.name}/SHA256SUMS",
                                f"{source.name}/bin/openvmm",
                                (f"{source.name}/provenance/openvmm.provenance.json"),
                            },
                        )
                        self.assertEqual(
                            members[f"{source.name}/bin/openvmm"].mode,
                            0o755,
                        )
                        self.assertEqual(
                            members[
                                f"{source.name}/provenance/openvmm.provenance.json"
                            ].mode,
                            0o644,
                        )
                        self.assertTrue(
                            all(member.mtime == 0 for member in members.values())
                        )
                    else:
                        with zipfile.ZipFile(first) as package:
                            member_list = package.infolist()
                            members = {
                                member.filename: member for member in member_list
                            }
                        self.assertEqual(
                            [member.filename for member in member_list],
                            sorted(member.filename for member in member_list),
                        )
                        self.assertEqual(
                            {
                                member.filename
                                for member in member_list
                                if not member.is_dir()
                            },
                            {
                                f"{source.name}/SHA256SUMS",
                                f"{source.name}/bin/openvmm",
                                (f"{source.name}/provenance/openvmm.provenance.json"),
                            },
                        )
                        executable_mode = (
                            members[f"{source.name}/bin/openvmm"].external_attr >> 16
                        ) & 0o777
                        data_mode = (
                            members[
                                f"{source.name}/provenance/openvmm.provenance.json"
                            ].external_attr
                            >> 16
                        ) & 0o777
                        self.assertEqual(executable_mode, 0o755)
                        self.assertEqual(data_mode, 0o644)
                        self.assertTrue(
                            all(
                                member.date_time == (1980, 1, 1, 0, 0, 0)
                                for member in members.values()
                            )
                        )

    def test_release_archive_rejects_internally_inconsistent_staging(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "nvx-1.0.0-test"
            payload = source / "bin" / "openvmm"
            payload.parent.mkdir(parents=True)
            payload.write_bytes(b"original")
            common.write_sha256_sums(source)
            destination = root / "release.tar.gz"
            destination.write_bytes(b"prior archive")
            create_archive = archive.create_reproducible_release_archive

            def archive_mutated_source(source: Path, output: Path) -> None:
                snapshot_payload = source / "bin" / "openvmm"
                snapshot_payload.write_bytes(b"replacement")
                create_archive(source, output)
                snapshot_payload.write_bytes(b"original")

            with (
                patch.object(
                    release,
                    "create_reproducible_release_archive",
                    side_effect=archive_mutated_source,
                ),
                self.assertRaisesRegex(common.ScriptError, "checksum mismatch"),
            ):
                release.create_release_archive(source, destination)

            self.assertEqual(destination.read_bytes(), b"prior archive")
            self.assertEqual(payload.read_bytes(), b"original")
            self.assertEqual(
                list(root.glob(".staging-*-release.tar.gz")),
                [],
            )

    def test_release_snapshot_rejects_coherent_source_replacement(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "nvx-1.0.0-test"
            payload = source / "bin" / "openvmm"
            payload.parent.mkdir(parents=True)
            payload.write_bytes(b"original")
            common.write_sha256_sums(source)
            checksum_file = source / "SHA256SUMS"
            original_checksum = checksum_file.read_bytes()
            replacement = b"coherent replacement"
            replacement_checksum = (
                f"{hashlib.sha256(replacement).hexdigest()}  bin/openvmm\n"
            ).encode("ascii")
            destination = root / "release.tar.gz"
            destination.write_bytes(b"prior archive")
            copy_pinned_file = release._copy_pinned_regular_file

            def replace_between_copies(
                source_root: Path,
                snapshot_root: Path,
                relative_name: str,
                expected_sha256: str,
            ) -> None:
                copy_pinned_file(
                    source_root,
                    snapshot_root,
                    relative_name,
                    expected_sha256,
                )
                if relative_name == "SHA256SUMS":
                    payload.write_bytes(replacement)
                    checksum_file.write_bytes(replacement_checksum)

            try:
                with (
                    patch.object(
                        release,
                        "_copy_pinned_regular_file",
                        side_effect=replace_between_copies,
                    ),
                    self.assertRaisesRegex(
                        common.ScriptError,
                        "release snapshot source changed",
                    ),
                ):
                    release.create_release_archive(source, destination)
            finally:
                if payload.exists():
                    payload.write_bytes(b"original")
                if checksum_file.exists():
                    checksum_file.write_bytes(original_checksum)

            self.assertEqual(destination.read_bytes(), b"prior archive")
            self.assertEqual(payload.read_bytes(), b"original")
            self.assertEqual(checksum_file.read_bytes(), original_checksum)
            self.assertEqual(list(root.glob(".snapshot-*")), [])
            self.assertEqual(list(root.glob(".staging-*-release.tar.gz")), [])

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
    def test_rejects_nonpositive_attempts(self):
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "archive.tar.xz"
            with (
                patch("nvx_tools.common.urllib.request.urlopen") as urlopen,
                self.assertRaisesRegex(
                    common.ScriptError, "download attempts must be positive"
                ),
            ):
                common.download(
                    "https://example.invalid/archive.tar.xz",
                    destination,
                    attempts=0,
                )

            urlopen.assert_not_called()
            self.assertFalse(destination.exists())

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

            with patch.object(common, "download", side_effect=write_verified):
                common.download_verified(
                    "https://example.invalid/archive.tar.xz",
                    destination,
                    expected_sha256,
                )

            self.assertEqual(destination.read_bytes(), payload)


if __name__ == "__main__":
    unittest.main()
