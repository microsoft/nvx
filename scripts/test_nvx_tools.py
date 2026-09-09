#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import argparse
import gzip
import hashlib
import io
import json
import os
import queue
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest
import warnings
import zipfile
from collections.abc import Callable
from dataclasses import dataclass
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


def _static_x86_64_elf() -> bytes:
    image = bytearray(120)
    image[:6] = b"\x7fELF\x02\x01"
    image[16:18] = (2).to_bytes(2, "little")
    image[18:20] = (62).to_bytes(2, "little")
    image[24:32] = (0x400040).to_bytes(8, "little")
    image[32:40] = (64).to_bytes(8, "little")
    image[54:56] = (56).to_bytes(2, "little")
    image[56:58] = (1).to_bytes(2, "little")
    image[64:68] = (1).to_bytes(4, "little")
    image[68:72] = (5).to_bytes(4, "little")
    image[80:88] = (0x400000).to_bytes(8, "little")
    image[96:104] = len(image).to_bytes(8, "little")
    image[104:112] = len(image).to_bytes(8, "little")
    return bytes(image)


@dataclass(frozen=True)
class NewcTestMetadata:
    inode: int
    uid: int = 0
    gid: int = 0
    nlink: int = 1
    devmajor: int = 0
    devminor: int = 0
    rdevmajor: int = 0
    rdevminor: int = 0


NewcTestEntry = (
    tuple[str, int, bytes]
    | tuple[str, int, bytes, int, int]
    | tuple[str, int, bytes, NewcTestMetadata]
)


def _newc_archive(entries: list[NewcTestEntry]) -> bytes:
    archive = bytearray()

    def add(
        name: str,
        mode: int,
        content: bytes,
        inode: int,
        uid: int = 0,
        gid: int = 0,
        nlink: int = 1,
        devmajor: int = 0,
        devminor: int = 0,
        rdevmajor: int = 0,
        rdevminor: int = 0,
    ) -> None:
        name_bytes = name.encode("utf-8") + b"\0"
        fields = (
            inode,
            mode,
            uid,
            gid,
            nlink,
            0,
            len(content),
            devmajor,
            devminor,
            rdevmajor,
            rdevminor,
            len(name_bytes),
            0,
        )
        archive.extend(
            b"070701" + b"".join(f"{field:08x}".encode() for field in fields)
        )
        archive.extend(name_bytes)
        archive.extend(b"\0" * (-len(archive) % 4))
        archive.extend(content)
        archive.extend(b"\0" * (-len(archive) % 4))

    for default_inode, entry in enumerate(entries, start=1):
        name, mode, content = entry[:3]
        if len(entry) == 4:
            metadata = entry[3]
        elif len(entry) == 5:
            metadata = NewcTestMetadata(default_inode, entry[3], entry[4])
        else:
            metadata = NewcTestMetadata(default_inode)
        add(
            name,
            mode,
            content,
            metadata.inode,
            metadata.uid,
            metadata.gid,
            metadata.nlink,
            metadata.devmajor,
            metadata.devminor,
            metadata.rdevmajor,
            metadata.rdevminor,
        )
    add("TRAILER!!!", 0, b"", len(entries) + 1)
    return gzip.compress(bytes(archive), compresslevel=9, mtime=0)


def _agent_newc_entries(agent: bytes) -> list[NewcTestEntry]:
    return [
        (".", 0o040755, b""),
        ("bin", 0o040755, b""),
        ("bin/busybox", 0o100755, b"busybox"),
        ("etc", 0o040755, b""),
        ("etc/group", 0o100644, b"root:x:0:\n"),
        ("etc/passwd", 0o100644, b"root:x:0:0:root:/root:/bin/sh\n"),
        ("etc/shadow", 0o100640, b"root:*::0:::::\n", 0, 42),
        ("root", 0o040700, b""),
        ("sbin", 0o040755, b""),
        ("sbin/apk", 0o100755, b"apk"),
        ("tmp", 0o041777, b""),
        ("var", 0o040755, b""),
        ("var/tmp", 0o041777, b""),
        ("init", 0o120777, b"sbin/nvx-agent"),
        ("sbin/nvx-agent", 0o100755, agent),
    ]


def _agent_newc_archive(agent: bytes) -> bytes:
    return _newc_archive(_agent_newc_entries(agent))


def _legacy_newc_archive(init_script: bytes) -> bytes:
    return _newc_archive(
        [
            (".", 0o040755, b""),
            ("bin", 0o040755, b""),
            ("bin/busybox", 0o100755, b"busybox"),
            ("etc", 0o040755, b""),
            ("etc/group", 0o100644, b"root:x:0:\n"),
            ("etc/passwd", 0o100644, b"root:x:0:0:root:/root:/bin/sh\n"),
            ("etc/shadow", 0o100640, b"root:*::0:::::\n", 0, 42),
            ("root", 0o040700, b""),
            ("sbin", 0o040755, b""),
            ("sbin/apk", 0o100755, b"apk"),
            ("tmp", 0o041777, b""),
            ("var", 0o040755, b""),
            ("var/tmp", 0o041777, b""),
            ("init", 0o100755, init_script.replace(b"\r\n", b"\n")),
        ]
    )


def _initramfs_package_manifest(
    profile: str,
    image: bytes,
    agent: bytes | None = None,
) -> bytes:
    value: dict[str, object] = {
        "format": 1,
        "alpine_version": "3.24.1",
        "alpine_branch": "v3.24",
        "architecture": "x86_64",
        "profile": profile,
        "artifact": {
            "name": (
                build.AGENT_INITRAMFS_NAME
                if profile == "broker-ttrpc"
                else "initramfs.cpio.gz"
            ),
            "sha256": hashlib.sha256(image).hexdigest(),
            "size": len(image),
        },
        "guest_agent": None,
        "packages": [{"name": "busybox"}],
    }
    if agent is not None:
        value["guest_agent"] = {
            "path": "/sbin/nvx-agent",
            "sha256": hashlib.sha256(agent).hexdigest(),
            "size": len(agent),
            "source_revision": build.GUEST_AGENT_SOURCE_REVISION,
            "build_id": build.GUEST_AGENT_BUILD_ID,
        }
    return (json.dumps(value, indent=2) + "\n").encode("utf-8")


def _write_test_zip(
    path: Path,
    entries: list[tuple[str, bytes, int]],
) -> None:
    with zipfile.ZipFile(
        path,
        "w",
        compression=zipfile.ZIP_DEFLATED,
        compresslevel=9,
    ) as archive:
        for name, data, file_type in entries:
            member = zipfile.ZipInfo(name)
            member.create_system = 3
            member.compress_type = zipfile.ZIP_DEFLATED
            mode = 0o755 if file_type == stat.S_IFDIR else 0o644
            member.external_attr = (file_type | mode) << 16
            archive.writestr(member, data)


def _ustar_header(
    name: str,
    typeflag: bytes,
    size: int,
) -> bytes:
    member = tarfile.TarInfo(name)
    member.type = typeflag
    member.mode = 0o755 if typeflag == tarfile.DIRTYPE else 0o644
    member.uid = 0
    member.gid = 0
    member.uname = ""
    member.gname = ""
    member.mtime = 0
    member.size = size
    return member.tobuf(format=tarfile.USTAR_FORMAT)


def _canonical_ustar(entries: list[tuple[str, bytes, bytes]]) -> bytes:
    output = bytearray()
    for name, typeflag, content in entries:
        output.extend(_ustar_header(name, typeflag, len(content)))
        output.extend(content)
        output.extend(b"\0" * (-len(content) % tarfile.BLOCKSIZE))
    output.extend(bytes(2 * tarfile.BLOCKSIZE))
    output.extend(bytes(-len(output) % tarfile.RECORDSIZE))
    return bytes(output)


def _replace_tar_checksum(header: bytearray) -> None:
    header[148:156] = b" " * 8
    checksum = sum(header)
    header[148:156] = f"{checksum:06o}\0 ".encode("ascii")


def _make_test_release_package(
    root: Path,
    transport: str,
    agent_bytes: bytes | None = None,
) -> Path:
    package = root / f"nvx-test-{transport}"
    binary_name = "openvmm.exe" if os.name == "nt" else "openvmm"
    binary = package / "bin" / binary_name
    binary.parent.mkdir(parents=True)
    binary.write_bytes(b"openvmm")
    guest = package / "guest"
    guest.mkdir()
    (guest / "vmlinux").write_bytes(b"vmlinux")
    (guest / "vmlinux.config").write_bytes(b"vmlinux.config")
    agent = agent_bytes if agent_bytes is not None else _static_x86_64_elf()
    if transport == "broker-ttrpc":
        image = _agent_newc_archive(agent)
        package_manifest = _initramfs_package_manifest(transport, image, agent)
    else:
        image = _legacy_newc_archive(
            (common.REPO_ROOT / "alpine" / "init").read_bytes()
        )
        package_manifest = _initramfs_package_manifest(transport, image)
    (guest / "initramfs.cpio.gz").write_bytes(image)
    (guest / "initramfs.cpio.gz.packages.json").write_bytes(package_manifest)
    if transport == "broker-ttrpc":
        (guest / build.AGENT_INITRAMFS_NAME).write_bytes(image)
        (guest / f"{build.AGENT_INITRAMFS_NAME}.packages.json").write_bytes(
            package_manifest
        )
        (guest / build.GUEST_AGENT_ARTIFACT_NAME).write_bytes(agent)
    for name in ("LICENSE", "README.md", "THIRD_PARTY_NOTICES.md"):
        (package / name).write_text(name, encoding="ascii")
    for name in ("COPYING-LINUX", "LICENSE-OPENVMM"):
        license_path = package / "licenses" / name
        license_path.parent.mkdir(parents=True, exist_ok=True)
        license_path.write_text(name, encoding="ascii")
    agent_sha256 = hashlib.sha256(agent).hexdigest()
    agent_enabled = transport == "broker-ttrpc"
    manifest: dict[str, object] = {
        "format": 1,
        "runtime": {
            "transport": transport,
            "artifact_profile": transport,
            "immutable_bundle": True,
        },
        "openvmm": {
            "source_revision": json.loads(
                (common.REPO_ROOT / "SOURCE-MANIFEST.json").read_text(encoding="utf-8")
            )["openvmm"]["source_revision"],
            "executable_sha256": hashlib.sha256(b"openvmm").hexdigest(),
            "microvm_abi_version": 2,
            "control_session_protocol_version": 1,
            "control_contract_revision": "nvx-microvm-v2-control-v1",
        },
        "linux": {
            "kernel_sha256": hashlib.sha256(b"vmlinux").hexdigest(),
            "config_sha256": hashlib.sha256(b"vmlinux.config").hexdigest(),
        },
        "alpine": {
            "initramfs_sha256": hashlib.sha256(image).hexdigest(),
            "initramfs_package_manifest_sha256": hashlib.sha256(
                package_manifest
            ).hexdigest(),
            "profile": transport,
            "initramfs_artifact": "guest/initramfs.cpio.gz",
        },
        "guest_agent": {
            "artifact": build.GUEST_AGENT_ARTIFACT_PATH,
            "initramfs_artifact": build.GUEST_AGENT_INITRAMFS_ARTIFACT_PATH,
            "target": build.GUEST_AGENT_TARGET,
            "optional": not agent_enabled,
            "sha256": agent_sha256 if agent_enabled else None,
            "size": len(agent) if agent_enabled else None,
            "installed_in_initramfs": agent_enabled,
            "maximum_size_bytes": build.GUEST_AGENT_MAXIMUM_BYTES,
            "source_revision": build.GUEST_AGENT_SOURCE_REVISION,
            "build_id": build.GUEST_AGENT_BUILD_ID,
            "external_input_sha256": agent_sha256,
            "external_input_size_bytes": len(agent),
            "protocol_schema_version": build.GUEST_AGENT_PROTOCOL_SCHEMA_VERSION,
            "transport": build.BROKER_TRANSPORT,
        },
    }
    runtime = cast(dict[str, object], manifest["runtime"])
    identity = release._runtime_identity(manifest)
    runtime["fingerprint_inputs"] = identity
    runtime["guest_fingerprint_sha256"] = release._runtime_fingerprint(identity)
    (package / "SOURCE-MANIFEST.json").write_text(
        json.dumps(manifest),
        encoding="utf-8",
    )
    release._normalize_release_tree_modes(package)
    common.write_sha256_sums(package)
    (package / "SHA256SUMS").chmod(0o644)
    return package


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
            [
                "download",
                "--repository",
                "example/nvx",
                "--hypervisor",
                "auto",
                "--transport",
                "legacy",
            ]
        )
        self.assertEqual(download.command, "download")
        self.assertEqual(download.repository, "example/nvx")
        self.assertEqual(download.hypervisor, "auto")
        self.assertIs(download.handler, nvx.command_download)

        collect = nvx.parse_args(["collect-sources", "--transport", "legacy"])
        self.assertEqual(collect.command, "collect-sources")
        self.assertEqual(collect.transport, "legacy")
        self.assertIs(collect.handler, nvx.command_collect_sources)

        package = nvx.parse_args(
            [
                "package",
                "--version",
                "1.2.3",
                "--destination",
                "output",
                "--include-source",
                "--transport",
                "broker-ttrpc",
                "--force",
            ]
        )
        self.assertEqual(package.command, "package")
        self.assertEqual(package.version, "1.2.3")
        self.assertEqual(package.destination, Path("output"))
        self.assertTrue(package.include_source)
        self.assertEqual(package.transport, "broker-ttrpc")
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
                "distro,distro.erofs",
                "--scratch",
                "scratch.ext4",
                "--transport",
                "broker-ttrpc",
                "--control-socket",
                str(Path.cwd() / "control.sock"),
                "--boot-console-socket",
                str(Path.cwd() / "boot.sock"),
                "--control-auth-handle",
                "9",
            ]
        )

        self.assertEqual(args.layer[0].role, "distro")
        self.assertEqual(args.scratch, Path("scratch.ext4"))
        self.assertEqual(args.control_auth_handle, 9)
        self.assertIs(args.handler, nvx.command_sandbox)

    def test_sandbox_selects_agent_initramfs_and_dual_consoles(self):
        root = Path.cwd()
        args = nvx.parse_args(
            [
                "sandbox",
                "--layer",
                f"distro,{root / 'distro.erofs'}",
                "--scratch",
                str(root / "scratch.ext4"),
                "--transport",
                "broker-ttrpc",
                "--control-socket",
                str(root / "control.sock"),
                "--boot-console-socket",
                str(root / "boot.sock"),
                "--control-auth-handle",
                "9",
                "--dry-run",
            ]
        )

        def existing(path: Path, _label: str) -> Path:
            return path

        with (
            patch.object(sandbox, "require_file"),
            patch.object(nvx, "require_file", side_effect=existing),
            patch.object(nvx, "_format_command", return_value="formatted") as formatted,
        ):
            nvx.command_sandbox(args)

        command = [str(value) for value in formatted.call_args.args[0]]
        self.assertIn(str(common.BUILD_DIR / build.AGENT_INITRAMFS_NAME), command)
        self.assertIn("--virtio-console", command)
        self.assertIn(f"listen={root / 'boot.sock'}", command)
        self.assertIn("--microvm-control-console", command)
        self.assertIn(f"listen={root / 'control.sock'}", command)
        self.assertIn("--microvm-control-auth-handle", command)
        self.assertIn("9", command)

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

    def test_stage_agent_parses_pinned_input(self):
        args = nvx.parse_args(
            [
                "stage-agent",
                "--input",
                "nvx-agent",
                "--sha256",
                "a" * 64,
            ]
        )

        self.assertEqual(args.input, Path("nvx-agent"))
        self.assertEqual(args.sha256, "a" * 64)
        self.assertIs(args.handler, nvx.command_stage_agent)

        provenance = nvx.parse_args(["record-openvmm-provenance"])
        self.assertIs(
            provenance.handler,
            nvx.command_record_openvmm_provenance,
        )
        verify_agent = nvx.parse_args(
            [
                "verify-agent-initramfs",
                "--input",
                "initramfs-agent.cpio.gz",
                "--sha256",
                "a" * 64,
            ]
        )
        self.assertEqual(verify_agent.input, Path("initramfs-agent.cpio.gz"))
        self.assertIs(verify_agent.handler, nvx.command_verify_agent_initramfs)


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


class BuildTests(unittest.TestCase):
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

    def test_ci_kernel_cache_key_includes_patches(self):
        action = (
            build.REPO_ROOT
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        ).read_text(encoding="utf-8")
        self.assertIn(
            "hashFiles('kernel/config-microvm', 'kernel/patches/**')",
            action,
        )

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

    def test_build_initramfs_builds_reseed_with_static_helper(self):
        with tempfile.TemporaryDirectory() as temporary:
            temporary_root = Path(temporary)
            repository = temporary_root / "repo"
            work = temporary_root / "work"
            root = work / "root"
            metadata_probe = work / "metadata-probe"
            output = temporary_root / "artifacts" / "initramfs.cpio.gz"
            for directory in (root / "etc", root / "sbin", metadata_probe):
                directory.mkdir(parents=True)
            config = build.AlpineBuildConfig(work=work, output=output)
            observed_reseed: dict[str, object] = {}

            def install(_source: Path, destination: Path) -> None:
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(b"script")
                destination.chmod(0o755)

            def build_static_helper(
                _work: Path, source: Path, destination: Path
            ) -> None:
                destination.write_bytes(f"static:{source.name}".encode())
                destination.chmod(0o755)

            def build_device_io(
                _work: Path, destination: Path
            ) -> dict[str, str]:
                destination.write_bytes(b"device-io")
                destination.chmod(0o755)
                return {"source_sha256": "source", "binary_sha256": "binary"}

            def pack_initramfs(
                prepared_root: Path, native_output: Path, _owners: object
            ) -> None:
                reseed = prepared_root / "sbin" / "nvx-reseed"
                observed_reseed["content"] = reseed.read_bytes()
                native_output.write_bytes(b"initramfs")
                native_output.with_name(
                    f"{native_output.name}.packages.json"
                ).write_text("{}", encoding="ascii")

            with (
                patch.object(build, "REPO_ROOT", repository),
                patch.object(build, "_require_linux"),
                patch.object(
                    build,
                    "_require_metadata_preserving_work_directory",
                    return_value=metadata_probe,
                ),
                patch.object(build, "_prepare_alpine_root", return_value=root),
                patch.object(build, "_apk_add"),
                patch.object(build, "_install", side_effect=install),
                patch.object(
                    build,
                    "_build_static_helper",
                    side_effect=build_static_helper,
                ) as static_helper,
                patch.object(
                    build,
                    "_build_device_io_helper",
                    side_effect=build_device_io,
                ),
                patch.object(build, "_write_apk_manifest"),
                patch.object(build, "_trusted_alpine_owners", return_value={}),
                patch.object(
                    build, "_pack_initramfs", side_effect=pack_initramfs
                ),
                patch.object(build, "_bind_apk_manifest_to_initramfs"),
                patch.object(build, "verify_legacy_initramfs") as verify,
            ):
                build.build_initramfs(config)

            self.assertEqual(
                static_helper.call_args_list[0],
                call(
                    work,
                    repository / "alpine" / "nvx-reseed.c",
                    root / "sbin" / "nvx-reseed",
                ),
            )
            self.assertEqual(observed_reseed["content"], b"static:nvx-reseed.c")
            self.assertEqual(output.read_bytes(), b"initramfs")
            self.assertEqual(
                output.with_name(f"{output.name}.packages.json").read_text(
                    encoding="ascii"
                ),
                "{}",
            )
            verify.assert_called_once_with(work / "output" / output.name)
            self.assertFalse(root.exists())
            self.assertFalse(metadata_probe.exists())

    def test_apk_uses_the_extracted_alpine_trust_store(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            certificates = root / "etc" / "ssl" / "certs" / "ca-certificates.crt"
            certificates.parent.mkdir(parents=True)
            certificates.write_text("test CA", encoding="ascii")

            with (
                patch.dict(os.environ, {}, clear=True),
                patch.object(build, "run_checked") as run,
            ):
                build._apk_add(root, "blkid")

            environment = run.call_args.kwargs["env"]
            self.assertEqual(environment["SSL_CERT_FILE"], str(certificates))
            self.assertEqual(environment["SSL_CERT_DIR"], str(certificates.parent))
    def test_guest_agent_identity_is_exact_fused_startup_input(self):
        self.assertEqual(
            build.GUEST_AGENT_SOURCE_REVISION,
            "71cdad58d0066b07ec117c8113a25936f211637f",
        )
        self.assertEqual(
            build.GUEST_AGENT_SHA256,
            "64c4c381daa4af1188e751e49002aaf3089e22fab0fa047cc738d45d7851dc80",
        )
        self.assertEqual(build.GUEST_AGENT_SIZE_BYTES, 1_844_064)
        self.assertEqual(
            build.GUEST_AGENT_BUILD_ID,
            "ad2432720e08f7d925398c0e7334140434d6979c",
        )

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

    def test_stage_guest_agent_requires_exact_reviewed_static_x86_64_elf(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "input-agent"
            source.write_bytes(_static_x86_64_elf())
            expected = hashlib.sha256(source.read_bytes()).hexdigest()
            expected_size = source.stat().st_size

            with (
                patch.object(build, "REPO_ROOT", root),
                patch.object(build, "GUEST_AGENT_SHA256", expected),
                patch.object(build, "GUEST_AGENT_SIZE_BYTES", expected_size),
            ):
                destination = build.stage_guest_agent(source, expected.upper())

            self.assertEqual(destination.read_bytes(), source.read_bytes())
            self.assertEqual(
                (root / "build" / build.GUEST_AGENT_SHA256_NAME)
                .read_text(encoding="ascii")
                .strip(),
                expected,
            )

            alternate = source.read_bytes() + b"same-source-different-build-id"
            source.write_bytes(alternate)
            alternate_sha256 = hashlib.sha256(alternate).hexdigest()
            with (
                patch.object(build, "REPO_ROOT", root),
                patch.object(build, "GUEST_AGENT_SHA256", expected),
                patch.object(build, "GUEST_AGENT_SIZE_BYTES", expected_size),
                self.assertRaisesRegex(common.ScriptError, "required external input"),
            ):
                build.stage_guest_agent(source, alternate_sha256)

            malformed = bytearray(_static_x86_64_elf())
            malformed[72:80] = (4096).to_bytes(8, "little")
            source.write_bytes(malformed)
            malformed_sha256 = hashlib.sha256(malformed).hexdigest()
            with (
                patch.object(build, "REPO_ROOT", root),
                patch.object(build, "GUEST_AGENT_SHA256", malformed_sha256),
                patch.object(build, "GUEST_AGENT_SIZE_BYTES", len(malformed)),
                self.assertRaisesRegex(common.ScriptError, "beyond end of file"),
            ):
                build.stage_guest_agent(source, malformed_sha256)

            dynamic = bytearray(_static_x86_64_elf())
            dynamic[64:68] = (3).to_bytes(4, "little")
            source.write_bytes(dynamic)
            dynamic_sha256 = hashlib.sha256(dynamic).hexdigest()
            with (
                patch.object(build, "REPO_ROOT", root),
                patch.object(build, "GUEST_AGENT_SHA256", dynamic_sha256),
                patch.object(build, "GUEST_AGENT_SIZE_BYTES", len(dynamic)),
                self.assertRaisesRegex(common.ScriptError, "statically linked"),
            ):
                build.stage_guest_agent(source, dynamic_sha256)

            wrong_arch = bytearray(_static_x86_64_elf())
            wrong_arch[18:20] = (183).to_bytes(2, "little")
            source.write_bytes(wrong_arch)
            wrong_arch_sha256 = hashlib.sha256(wrong_arch).hexdigest()
            with (
                patch.object(build, "REPO_ROOT", root),
                patch.object(build, "GUEST_AGENT_SHA256", wrong_arch_sha256),
                patch.object(build, "GUEST_AGENT_SIZE_BYTES", len(wrong_arch)),
                self.assertRaisesRegex(common.ScriptError, "x86-64"),
            ):
                build.stage_guest_agent(source, wrong_arch_sha256)

            source.write_bytes(_static_x86_64_elf())
            oversized_sha256 = hashlib.sha256(source.read_bytes()).hexdigest()
            with (
                patch.object(build, "REPO_ROOT", root),
                patch.object(build, "GUEST_AGENT_SHA256", oversized_sha256),
                patch.object(build, "GUEST_AGENT_SIZE_BYTES", source.stat().st_size),
                patch.object(build, "GUEST_AGENT_MAXIMUM_BYTES", 64),
                self.assertRaisesRegex(common.ScriptError, "16-MiB"),
            ):
                build.stage_guest_agent(source, oversized_sha256)

    def test_agent_initramfs_inspection_requires_exact_embedded_pid1(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / build.AGENT_INITRAMFS_NAME
            agent = _static_x86_64_elf()
            path.write_bytes(_agent_newc_archive(agent))
            expected = hashlib.sha256(agent).hexdigest()
            build.verify_agent_initramfs(path, expected)
            with self.assertRaisesRegex(common.ScriptError, "embedded.*SHA-256"):
                build.verify_agent_initramfs(path, "0" * 64)
            first_archive = path.read_bytes()
            path.write_bytes(first_archive + first_archive)
            with self.assertRaisesRegex(common.ScriptError, "second archive"):
                build.verify_agent_initramfs(path, expected)
            path.write_bytes(
                _newc_archive(
                    [
                        *_agent_newc_entries(agent),
                        ("/sbin/nvx-agent", 0o100755, b"replacement"),
                    ]
                )
            )
            with self.assertRaisesRegex(common.ScriptError, "non-canonical"):
                build.verify_agent_initramfs(path, expected)

            path.write_bytes(
                _newc_archive(
                    [
                        *_agent_newc_entries(agent),
                        ("sbin/nvx-agent", 0o100755, b"replacement"),
                    ]
                )
            )
            with self.assertRaisesRegex(common.ScriptError, "duplicate"):
                build.verify_agent_initramfs(path, expected)

    def test_newc_parser_retains_extraction_identity_and_data_evidence(self):
        content = b"member contents"
        metadata = NewcTestMetadata(
            inode=0x1234,
            nlink=1,
            devmajor=8,
            devminor=9,
            rdevmajor=10,
            rdevminor=11,
        )
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "model.cpio.gz"
            path.write_bytes(
                _newc_archive(
                    [
                        (".", 0o040755, b""),
                        ("link", 0o120777, b"target", metadata),
                        ("target", 0o100644, content),
                    ]
                )
            )

            entries = build._newc_entries(path)

        link = entries[1]
        target = entries[2]
        self.assertEqual((link.order, link.name), (1, "link"))
        self.assertEqual(link.identity, (8, 9, 0x1234))
        self.assertEqual((link.rdevmajor, link.rdevminor), (10, 11))
        self.assertEqual((link.mode, link.nlink), (0o120777, 1))
        self.assertEqual(link.file_type, stat.S_IFLNK)
        self.assertEqual(link.symlink_target, "target")
        self.assertEqual(link.data_end - link.data_start, len(link.data))
        self.assertEqual(link.data_sha256, hashlib.sha256(link.data).hexdigest())
        self.assertEqual(target.data_sha256, hashlib.sha256(content).hexdigest())

    def test_initramfs_rejects_extraction_graph_aliases_and_reordering(self):
        agent = _static_x86_64_elf()
        expected = hashlib.sha256(agent).hexdigest()

        def verify(
            entries: list[NewcTestEntry],
            extra_trusted_links: set[tuple[str, str]] | None = None,
        ) -> None:
            with tempfile.TemporaryDirectory() as temporary:
                path = Path(temporary) / build.AGENT_INITRAMFS_NAME
                path.write_bytes(_newc_archive(entries))
                trusted = build.TRUSTED_INITRAMFS_SYMLINKS | frozenset(
                    extra_trusted_links or ()
                )
                with patch.object(build, "TRUSTED_INITRAMFS_SYMLINKS", trusted):
                    build.verify_agent_initramfs(path, expected)

        cases: list[
            tuple[
                str,
                list[NewcTestEntry],
                set[tuple[str, str]] | None,
                str,
            ]
        ] = [
            (
                "critical symlink alias",
                [
                    *_agent_newc_entries(agent),
                    ("zz", 0o120777, b"sbin"),
                    ("zz/nvx-agent", 0o100755, b"replacement"),
                ],
                {("zz", "sbin")},
                "below non-directory ancestor 'zz'",
            ),
            (
                "generic symlink ancestor",
                [
                    *_agent_newc_entries(agent),
                    ("usr", 0o040755, b""),
                    ("usr-link", 0o120777, b"usr"),
                    ("usr-link/child", 0o100644, b"child"),
                ],
                {("usr-link", "usr")},
                "below non-directory ancestor 'usr-link'",
            ),
            (
                "type replacement",
                [
                    *_agent_newc_entries(agent),
                    ("replace", 0o040755, b""),
                    ("replace", 0o100644, b"replacement"),
                ],
                None,
                "duplicate newc member 'replace'",
            ),
            (
                "child before ancestor",
                [
                    *_agent_newc_entries(agent),
                    ("late/child", 0o100644, b"child"),
                    ("late", 0o040755, b""),
                ],
                None,
                "before directory ancestor 'late'",
            ),
            (
                "case collision",
                [
                    *_agent_newc_entries(agent),
                    ("Case", 0o100644, b"first"),
                    ("case", 0o100644, b"second"),
                ],
                None,
                "case-colliding",
            ),
        ]
        for label, entries, trusted_links, error in cases:
            with (
                self.subTest(label=label),
                self.assertRaisesRegex(common.ScriptError, error),
            ):
                verify(entries, trusted_links)

    def test_initramfs_rejects_critical_and_benign_hardlinks(self):
        agent = _static_x86_64_elf()
        expected = hashlib.sha256(agent).hexdigest()

        def verify(entries: list[NewcTestEntry]) -> None:
            with tempfile.TemporaryDirectory() as temporary:
                path = Path(temporary) / build.AGENT_INITRAMFS_NAME
                path.write_bytes(_newc_archive(entries))
                build.verify_agent_initramfs(path, expected)

        critical = _agent_newc_entries(agent)
        critical_index = next(
            index
            for index, entry in enumerate(critical)
            if entry[0] == "sbin/nvx-agent"
        )
        critical[critical_index] = (
            "sbin/nvx-agent",
            0o100755,
            agent,
            NewcTestMetadata(inode=0xA11A5, nlink=2),
        )
        critical.append(
            (
                "sbin/nvx-agent-alias",
                0o100755,
                b"",
                NewcTestMetadata(inode=0xA11A5, nlink=2),
            )
        )
        with self.assertRaisesRegex(
            common.ScriptError,
            "nvx-agent.*unsafe link count",
        ):
            verify(critical)

        benign = [
            *_agent_newc_entries(agent),
            ("usr", 0o040755, b""),
            ("usr/share", 0o040755, b""),
            (
                "usr/share/one",
                0o100644,
                b"benign",
                NewcTestMetadata(inode=0xBEE, nlink=2),
            ),
            (
                "usr/share/two",
                0o100644,
                b"",
                NewcTestMetadata(inode=0xBEE, nlink=2),
            ),
        ]
        with self.assertRaisesRegex(
            common.ScriptError,
            "regular file 'usr/share/one'.*unsafe link count",
        ):
            verify(benign)

        repeated_identity = [
            *_agent_newc_entries(agent),
            ("usr", 0o040755, b""),
            ("usr/share", 0o040755, b""),
            (
                "usr/share/one",
                0o100644,
                b"first",
                NewcTestMetadata(inode=0xD00D),
            ),
            (
                "usr/share/two",
                0o100644,
                b"second",
                NewcTestMetadata(inode=0xD00D),
            ),
        ]
        with self.assertRaisesRegex(
            common.ScriptError,
            "repeated newc inode identity.*usr/share/one.*usr/share/two",
        ):
            verify(repeated_identity)

    def test_agent_initramfs_rejects_unsafe_metadata_mutations(self):
        agent = _static_x86_64_elf()
        expected = hashlib.sha256(agent).hexdigest()

        def mutate(name: str, replacement: NewcTestEntry) -> bytes:
            entries = _agent_newc_entries(agent)
            index = next(
                index for index, entry in enumerate(entries) if entry[0] == name
            )
            entries[index] = replacement
            return _newc_archive(entries)

        mutations = (
            (
                mutate(
                    "sbin/nvx-agent",
                    ("sbin/nvx-agent", 0o100755, agent, 1, 0),
                ),
                "nvx-agent with the wrong mode",
            ),
            (
                mutate("init", ("init", 0o100755, agent)),
                "does not select",
            ),
            (
                mutate(
                    "etc/shadow",
                    ("etc/shadow", 0o100644, b"shadow", 0, 42),
                ),
                "incorrect metadata.*shadow",
            ),
            (
                mutate("etc/passwd", ("etc/passwd", 0o100600, b"passwd")),
                "incorrect metadata.*passwd",
            ),
            (
                mutate("tmp", ("tmp", 0o040777, b"")),
                "unsafe world-writable directory",
            ),
            (
                mutate("bin", ("bin", 0o040777, b"")),
                "unsafe world-writable directory",
            ),
            (
                mutate(
                    "bin/busybox",
                    ("bin/busybox", 0o100750, _static_x86_64_elf()),
                ),
                "ELF binary.*without 0755",
            ),
            (
                _newc_archive(
                    [
                        *_agent_newc_entries(agent),
                        ("dev", 0o040755, b""),
                        ("dev/console", 0o020600, b""),
                    ]
                ),
                "unsafe entry type",
            ),
            (
                _newc_archive(
                    [
                        *_agent_newc_entries(agent),
                        ("bad-link", 0o120777, b"../../outside"),
                    ]
                ),
                "escaping symlink",
            ),
            (
                _newc_archive(
                    [
                        *_agent_newc_entries(agent),
                        ("unexpected-link", 0o120777, b"/bin/busybox"),
                    ]
                ),
                "untrusted symlink",
            ),
        )
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / build.AGENT_INITRAMFS_NAME
            for archive_bytes, error in mutations:
                with self.subTest(error=error):
                    path.write_bytes(archive_bytes)
                    with self.assertRaisesRegex(common.ScriptError, error):
                        build.verify_agent_initramfs(path, expected)

    def test_legacy_and_broker_initramfs_profiles_are_structurally_distinct(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            legacy = root / "initramfs.cpio.gz"
            broker = root / build.AGENT_INITRAMFS_NAME
            legacy.write_bytes(
                _legacy_newc_archive(
                    (common.REPO_ROOT / "alpine" / "init").read_bytes()
                )
            )
            agent = _static_x86_64_elf()
            broker.write_bytes(_agent_newc_archive(agent))

            build.verify_legacy_initramfs(legacy)
            build.verify_agent_initramfs(
                broker,
                hashlib.sha256(agent).hexdigest(),
            )
            with self.assertRaisesRegex(common.ScriptError, "NVX PID-1 layout"):
                build.verify_agent_initramfs(
                    legacy,
                    hashlib.sha256(agent).hexdigest(),
                )
            with self.assertRaisesRegex(
                common.ScriptError,
                "contains /sbin/nvx-agent|untrusted symlink",
            ):
                build.verify_legacy_initramfs(broker)

    def test_initramfs_work_directory_rejects_drvfs(self):
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary) / "work"
            with (
                patch.object(build, "_linux_filesystem_type", return_value="v9fs"),
                self.assertRaisesRegex(
                    common.ScriptError,
                    "NVX_NATIVE_WORK_DIR.*native Linux filesystem",
                ),
            ):
                build._require_metadata_preserving_work_directory(work)

    def test_agent_initramfs_requires_separate_verified_staging(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with (
                patch.object(build, "REPO_ROOT", root),
                self.assertRaisesRegex(common.ScriptError, "staged NVX guest agent"),
            ):
                build.verified_staged_guest_agent()

            legacy = build.AlpineBuildConfig()
            agent = build.AlpineBuildConfig(
                output=Path(build.AGENT_INITRAMFS_NAME),
                agent_enabled=True,
            )
            self.assertFalse(legacy.agent_enabled)
            self.assertEqual(legacy.output.name, "initramfs.cpio.gz")
            self.assertTrue(agent.agent_enabled)
            self.assertEqual(agent.output.name, build.AGENT_INITRAMFS_NAME)

    def test_agent_docker_input_uses_sha_but_ci_does_not_cache_image(self):
        command = [
            str(value)
            for value in build.docker_build_agent_initramfs_command(
                build.DockerBuildConfig(),
                "a" * 64,
            )
        ]
        self.assertIn("--secret", command)
        self.assertIn(f"NVX_AGENT_SHA256={'a' * 64}", command)
        action = (
            Path(__file__).parents[1]
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        ).read_text(encoding="utf-8")
        self.assertNotIn("agent-initramfs", action)
        self.assertNotIn("agent-cache", action)
        benchmark_action = (
            Path(__file__).parents[1]
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")
        self.assertNotIn("agent-initramfs", benchmark_action)
        self.assertNotIn("agent-cache", benchmark_action)
        package_action = (
            Path(__file__).parents[1]
            / ".github"
            / "actions"
            / "package-release"
            / "action.yml"
        ).read_text(encoding="utf-8")
        self.assertNotIn("--transport broker-ttrpc", package_action)
        self.assertIn("archive-release", package_action)
        publish_action = (
            Path(__file__).parents[1]
            / ".github"
            / "actions"
            / "publish-development-release"
            / "action.yml"
        ).read_text(encoding="utf-8")
        self.assertIn("broker-live-gate-contracts", publish_action)
        self.assertIn("verify-broker-live-gate", publish_action)
        self.assertIn("--archive-sha256", publish_action)
        self.assertIn("externally authenticated live-gate proof", publish_action)


class SandboxTests(unittest.TestCase):
    def test_launch_contract_orders_roles_and_keeps_config_off_command_line(self):
        custom = sandbox.SandboxLayer.parse("custom,custom.erofs")
        distro = sandbox.SandboxLayer.parse("distro,distro.erofs")
        launch = sandbox.SandboxLaunch(
            layers=(custom, distro),
            scratch=Path("scratch.ext4"),
        )

        self.assertEqual(
            [layer.role for layer in launch.ordered_layers()],
            ["distro", "custom"],
        )
        self.assertEqual(launch.kernel_command_line("quiet"), "quiet")
        self.assertEqual(
            launch.openvmm_arguments(
                Path.cwd() / "control.sock",
                Path.cwd() / "boot.sock",
                9,
            ),
            [
                "--machine",
                "microvm",
                "--virtio-console",
                f"listen={Path.cwd() / 'boot.sock'}",
                "--microvm-control-console",
                f"listen={Path.cwd() / 'control.sock'}",
                "--microvm-control-auth-handle",
                "9",
                "--microvm-control-auth-timeout-ms",
                "5000",
                "--microvm-sandbox-block",
                "distro:file:distro.erofs,ro",
                "--microvm-sandbox-block",
                "custom:file:custom.erofs,ro",
                "--microvm-sandbox-block",
                "scratch:file:scratch.ext4",
            ],
        )

    def test_launch_contract_rejects_duplicates_and_reserved_tokens(self):
        distro = sandbox.SandboxLayer.parse("distro,distro.erofs")
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
        for command_line in (
            "rdinit=/bin/sh",
            "init=/bin/sh",
            'rdinit="/bin/sh"',
            r"rdinit\=/bin/sh",
            "quiet\trdinit=/bin/sh",
            "nvx_control_tty=hvc9 nvx_control_tty=hvc2",
            "nvx_sandbox=0 nvx_sandbox=1",
            "virtnet_ip=192.0.2.1",
            "console=ttyS0",
        ):
            with (
                self.subTest(command_line=command_line),
                self.assertRaisesRegex(common.ScriptError, "owned"),
            ):
                launch.kernel_command_line(command_line)
        with self.assertRaisesRegex(common.ScriptError, "1024-byte"):
            launch.kernel_command_line("x" * sandbox.SANDBOX_COMMAND_LINE_MAX_SIZE)

    def test_layer_parser_rejects_invalid_role_and_shape(self):
        with self.assertRaisesRegex(common.ScriptError, "unsupported layer role"):
            sandbox.SandboxLayer.parse("unknown,layer.erofs")
        with self.assertRaisesRegex(common.ScriptError, "ROLE,PATH"):
            sandbox.SandboxLayer.parse("distro,layer.erofs,ignored")

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
        self.assertEqual(
            benchmark.lifecycle_tuning("mshv"),
            f"nolapic_timer {benchmark.BASE_TUNING}",
        )
        self.assertEqual(
            benchmark.lifecycle_tuning("mshv", 2),
            benchmark.BASE_TUNING,
        )
        self.assertEqual(
            benchmark.lifecycle_tuning("kvm"),
            f"clocksource=kvm-clock {benchmark.BASE_TUNING}",
        )
        self.assertEqual(
            benchmark.lifecycle_tuning("whp"),
            benchmark.BASE_TUNING,
        )

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
                "--transport",
                "legacy",
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
            transport="legacy",
            manifest_digest_output=None,
        )

    def test_download_command_selects_host_release(self):
        args = nvx.parse_args(
            [
                "download",
                "--repository",
                "example/nvx",
                "--transport",
                "legacy",
            ]
        )
        expected_platform = "windows-whp" if os.name == "nt" else "linux-kvm"

        with patch.object(nvx, "download_latest_release") as download_release:
            args.handler(args)

        download_release.assert_called_once_with(
            "example/nvx",
            expected_platform,
            "legacy",
            None,
        )


class ReleaseTests(unittest.TestCase):
    def test_transaction_rolls_back_base_exceptions_at_every_stage(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source_one = root / "source-one"
            source_two = root / "source-two"
            source_one.write_bytes(b"new-one")
            source_two.write_bytes(b"new-two")
            destination_one = root / "runtime" / "one"
            destination_two = root / "runtime" / "two"
            removed = root / "runtime" / "removed"
            transaction_parent = root / "runtime" / ".install-transactions"
            original_replace = release._atomic_replace

            def reset() -> dict[Path, tuple[bytes, int]]:
                values = {
                    destination_one: b"old-one",
                    destination_two: b"old-two",
                    removed: b"old-removed",
                }
                snapshot: dict[Path, tuple[bytes, int]] = {}
                for index, (path, value) in enumerate(values.items()):
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_bytes(value)
                    path.chmod(0o600 if index % 2 else 0o700)
                    snapshot[path] = (
                        path.read_bytes(),
                        stat.S_IMODE(path.stat().st_mode),
                    )
                return snapshot

            files = [
                release._InstallFile(source_one, destination_one, 0o644),
                release._InstallFile(source_two, destination_two, 0o644),
            ]
            for exception_type in (KeyboardInterrupt, SystemExit):
                for failure_call in range(1, 6):
                    prior = reset()
                    calls = 0

                    def fail_one(
                        source: Path,
                        destination: Path,
                        target_call: int = failure_call,
                        raised_exception: type[BaseException] = exception_type,
                    ) -> None:
                        nonlocal calls
                        calls += 1
                        if calls == target_call:
                            raise raised_exception()
                        original_replace(source, destination)

                    def transaction_artifact_path(name: str) -> Path:
                        return root / "runtime" / name

                    with (
                        self.subTest(
                            exception=exception_type.__name__,
                            failure_call=failure_call,
                        ),
                        patch.object(
                            release,
                            "artifact_path",
                            side_effect=transaction_artifact_path,
                        ),
                        patch.object(
                            release,
                            "_atomic_replace",
                            side_effect=fail_one,
                        ),
                        self.assertRaises(exception_type),
                    ):
                        release._transactional_install(files, [removed])
                    self.assertEqual(
                        {
                            path: (
                                path.read_bytes(),
                                stat.S_IMODE(path.stat().st_mode),
                            )
                            for path in prior
                        },
                        prior,
                    )
                    self.assertEqual(list(transaction_parent.iterdir()), [])

    def test_transaction_preserves_failed_rollback_for_next_recovery(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            destination = root / "runtime" / "destination"
            source.write_bytes(b"new")
            destination.parent.mkdir(parents=True)
            destination.write_bytes(b"prior")
            destination.chmod(0o700)
            prior = (
                destination.read_bytes(),
                stat.S_IMODE(destination.stat().st_mode),
            )
            original_replace = release._atomic_replace
            calls = 0

            def fail_install_and_rollback(source: Path, target: Path) -> None:
                nonlocal calls
                calls += 1
                if calls == 2:
                    raise KeyboardInterrupt()
                if calls == 3:
                    raise OSError("injected rollback failure")
                original_replace(source, target)

            def transaction_artifact_path(name: str) -> Path:
                return root / "runtime" / name

            with (
                patch.object(
                    release,
                    "artifact_path",
                    side_effect=transaction_artifact_path,
                ),
                patch.object(
                    release,
                    "_atomic_replace",
                    side_effect=fail_install_and_rollback,
                ),
                self.assertRaisesRegex(
                    common.ScriptError,
                    "journal and backups were preserved",
                ),
            ):
                release._transactional_install(
                    [release._InstallFile(source, destination, 0o644)],
                    [],
                )

            transaction_parent = root / "runtime" / ".install-transactions"
            transactions = list(transaction_parent.iterdir())
            self.assertEqual(len(transactions), 1)
            self.assertTrue((transactions[0] / "journal.json").is_file())
            self.assertTrue(any((transactions[0] / "backup").iterdir()))

            release._recover_install_transactions(transaction_parent)
            self.assertEqual(
                (
                    destination.read_bytes(),
                    stat.S_IMODE(destination.stat().st_mode),
                ),
                prior,
            )
            self.assertEqual(list(transaction_parent.iterdir()), [])

    def test_selects_latest_matching_prerelease_asset(self):
        releases = [
            {
                "draft": True,
                "tag_name": "v1.2.4-draft",
                "assets": [
                    {
                        "name": "nvx-1.2.4-linux-kvm-legacy.tar.gz",
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
                        "name": "nvx-1.2.3-windows-whp-legacy.zip",
                        "url": "https://api.example.invalid/windows",
                        "size": 200,
                    },
                    {
                        "name": "nvx-1.2.3-linux-kvm-legacy.tar.gz",
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
                "legacy",
                "token",
            )

        self.assertEqual(asset.tag, "v1.2.3-dev.abc123")
        self.assertEqual(asset.name, "nvx-1.2.3-linux-kvm-legacy.tar.gz")
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
                build.AGENT_INITRAMFS_NAME,
                f"{build.AGENT_INITRAMFS_NAME}.packages.json",
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
            agent_bytes = _static_x86_64_elf()
            agent_sha256 = hashlib.sha256(agent_bytes).hexdigest()
            legacy_init = b"#!/bin/sh\necho legacy\n"
            (root / "alpine" / "init").parent.mkdir()
            (root / "alpine" / "init").write_bytes(legacy_init)
            legacy_initramfs = _legacy_newc_archive(legacy_init)
            (build_dir / "initramfs.cpio.gz").write_bytes(legacy_initramfs)
            (build_dir / "initramfs.cpio.gz.packages.json").write_bytes(
                _initramfs_package_manifest("legacy", legacy_initramfs)
            )
            source_manifest = {
                "format": 1,
                "openvmm": {
                    "source_revision": "revision",
                    "microvm_abi_version": 2,
                    "control_session_protocol_version": 1,
                    "control_contract_revision": "nvx-microvm-v2-control-v1",
                },
                "linux": {},
                "alpine": {},
                "guest_agent": {
                    "artifact": f"guest/{build.GUEST_AGENT_ARTIFACT_NAME}",
                    "initramfs_artifact": f"guest/{build.AGENT_INITRAMFS_NAME}",
                    "target": build.GUEST_AGENT_TARGET,
                    "optional": True,
                    "installed_in_initramfs": False,
                    "maximum_size_bytes": build.GUEST_AGENT_MAXIMUM_BYTES,
                    "source_revision": build.GUEST_AGENT_SOURCE_REVISION,
                    "build_id": build.GUEST_AGENT_BUILD_ID,
                    "external_input_sha256": agent_sha256,
                    "external_input_size_bytes": len(agent_bytes),
                    "protocol_schema_version": 1,
                    "transport": "broker-ttrpc",
                },
            }
            (root / "SOURCE-MANIFEST.json").write_text(
                json.dumps(source_manifest),
                encoding="utf-8",
            )
            (build_dir / build.OPENVMM_PROVENANCE_NAME).write_text(
                json.dumps(
                    {
                        "format": 1,
                        "source_revision": "revision",
                        "source_clean": True,
                        "executable_sha256": hashlib.sha256(
                            binary.read_bytes()
                        ).hexdigest(),
                        "origin": "local-build",
                    }
                ),
                encoding="utf-8",
            )
            agent = build_dir / build.GUEST_AGENT_ARTIFACT_NAME
            agent.write_bytes(agent_bytes)
            agent_initramfs = _agent_newc_archive(agent.read_bytes())
            (build_dir / build.AGENT_INITRAMFS_NAME).write_bytes(agent_initramfs)
            (build_dir / f"{build.AGENT_INITRAMFS_NAME}.packages.json").write_bytes(
                _initramfs_package_manifest(
                    "broker-ttrpc",
                    agent_initramfs,
                    agent_bytes,
                )
            )
            (build_dir / build.GUEST_AGENT_SHA256_NAME).write_text(
                f"{agent_sha256}\n",
                encoding="ascii",
            )
            destination = root / "staged"
            legacy_destination = root / "legacy"
            digest_output = root / "trusted" / "manifest.sha256"
            stderr = io.StringIO()

            def artifact_path(name: str) -> Path:
                return build_dir / name

            with (
                patch.object(release, "REPO_ROOT", root),
                patch.object(release, "SOURCE_DIR", source_dir),
                patch.object(release, "OPENVMM_DIR", openvmm_dir),
                patch.object(build, "REPO_ROOT", root),
                patch.object(
                    release,
                    "artifact_path",
                    side_effect=artifact_path,
                ),
                patch.object(release, "openvmm_binary_path", return_value=binary),
                patch.object(release, "GUEST_AGENT_SHA256", agent_sha256),
                patch.object(release, "GUEST_AGENT_SIZE_BYTES", len(agent_bytes)),
                patch("sys.stderr", stderr),
            ):
                release.package_release(
                    version="1.0.0",
                    destination=destination,
                    include_source=False,
                    force=False,
                    transport="broker-ttrpc",
                    manifest_digest_output=digest_output,
                )
                release.package_release(
                    version="1.0.0",
                    destination=legacy_destination,
                    include_source=False,
                    force=False,
                    transport="legacy",
                    manifest_digest_output=None,
                )

            self.assertTrue((destination / "bin" / binary.name).is_file())
            for name in guest_names:
                self.assertTrue((destination / "guest" / name).is_file())
            self.assertEqual(
                (destination / "guest" / build.GUEST_AGENT_ARTIFACT_NAME).read_bytes(),
                agent.read_bytes(),
            )
            runtime_manifest = json.loads(
                (destination / "SOURCE-MANIFEST.json").read_text(encoding="utf-8")
            )
            self.assertEqual(
                runtime_manifest["guest_agent"]["sha256"],
                agent_sha256,
            )
            self.assertTrue(runtime_manifest["guest_agent"]["installed_in_initramfs"])
            self.assertFalse(runtime_manifest["guest_agent"]["optional"])
            self.assertEqual(
                runtime_manifest["guest_agent"]["source_revision"],
                build.GUEST_AGENT_SOURCE_REVISION,
            )
            self.assertEqual(
                runtime_manifest["guest_agent"]["build_id"],
                build.GUEST_AGENT_BUILD_ID,
            )
            self.assertEqual(
                runtime_manifest["runtime"]["transport"],
                "broker-ttrpc",
            )
            identity = runtime_manifest["runtime"]["fingerprint_inputs"]
            canonical_identity = json.dumps(
                identity,
                sort_keys=True,
                separators=(",", ":"),
            ).encode("utf-8")
            self.assertEqual(
                runtime_manifest["runtime"]["guest_fingerprint_sha256"],
                hashlib.sha256(canonical_identity).hexdigest(),
            )
            self.assertEqual(
                digest_output.read_text(encoding="ascii").strip(),
                hashlib.sha256(
                    (destination / "SOURCE-MANIFEST.json").read_bytes()
                ).hexdigest(),
            )
            self.assertEqual(
                runtime_manifest["openvmm"]["executable_sha256"],
                hashlib.sha256(binary.read_bytes()).hexdigest(),
            )
            self.assertEqual(
                (destination / "guest" / "initramfs.cpio.gz").read_bytes(),
                (build_dir / build.AGENT_INITRAMFS_NAME).read_bytes(),
            )
            self.assertEqual(
                (legacy_destination / "guest" / "initramfs.cpio.gz").read_bytes(),
                (build_dir / "initramfs.cpio.gz").read_bytes(),
            )
            self.assertFalse(
                (
                    legacy_destination / "guest" / build.GUEST_AGENT_ARTIFACT_NAME
                ).exists()
            )
            legacy_manifest = json.loads(
                (legacy_destination / "SOURCE-MANIFEST.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(legacy_manifest["runtime"]["transport"], "legacy")
            self.assertFalse(legacy_manifest["guest_agent"]["installed_in_initramfs"])
            common.verify_sha256_sums(destination)
            common.verify_sha256_sums(legacy_destination)
            self.assertIn("binary-only package", stderr.getvalue())

    def test_release_archive_rejects_unsafe_or_unexpected_modes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive_path = root / "unsafe.tar.gz"
            with tarfile.open(archive_path, "w:gz") as archive_file:
                for name, mode, kind, content in (
                    ("bundle", 0o755, tarfile.DIRTYPE, b""),
                    ("bundle/bin", 0o755, tarfile.DIRTYPE, b""),
                    ("bundle/bin/openvmm", 0o4755, tarfile.REGTYPE, b"binary"),
                ):
                    member = tarfile.TarInfo(name)
                    member.mode = mode
                    member.type = kind
                    member.size = len(content)
                    archive_file.addfile(
                        member,
                        io.BytesIO(content) if content else None,
                    )

            with self.assertRaisesRegex(common.ScriptError, "noncanonical metadata"):
                release._extract_release_archive(archive_path, root / "output")

    def test_release_archive_preflight_enforces_all_resource_bounds(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = ("bundle/SOURCE-MANIFEST.json", b"{}", stat.S_IFREG)

            cases: tuple[
                tuple[str, bytes, int, str, str, int],
                ...,
            ] = (
                (
                    "bundle/member-count",
                    b"x",
                    stat.S_IFREG,
                    "member-count",
                    "RELEASE_ARCHIVE_MAX_MEMBERS",
                    1,
                ),
                (
                    "bundle/oversized",
                    b"xxxx",
                    stat.S_IFREG,
                    "size limit",
                    "RELEASE_ARCHIVE_MAX_MEMBER_BYTES",
                    3,
                ),
                (
                    "bundle/total",
                    b"xx",
                    stat.S_IFREG,
                    "uncompressed-size",
                    "RELEASE_ARCHIVE_MAX_TOTAL_BYTES",
                    3,
                ),
                (
                    f"bundle/{'x' * 80}",
                    b"x",
                    stat.S_IFREG,
                    "unsafe path",
                    "RELEASE_ARCHIVE_MAX_PATH_BYTES",
                    64,
                ),
                (
                    "bundle/ratio",
                    b"0" * 4096,
                    stat.S_IFREG,
                    "compression-ratio",
                    "RELEASE_ARCHIVE_MAX_COMPRESSION_RATIO",
                    2,
                ),
                (
                    "bundle/link",
                    b"target",
                    stat.S_IFLNK,
                    "expected Unix type",
                    "RELEASE_ARCHIVE_MAX_MEMBERS",
                    10,
                ),
            )
            for index, (
                name,
                data,
                file_type,
                error,
                limit_name,
                limit,
            ) in enumerate(cases):
                archive_path = root / f"bounded-{index}.zip"
                _write_test_zip(archive_path, [manifest, (name, data, file_type)])
                with (
                    self.subTest(error=error),
                    patch.object(release, limit_name, limit),
                    self.assertRaisesRegex(common.ScriptError, error),
                ):
                    release._preflight_release_archive(archive_path)

            duplicate = root / "duplicate.zip"
            with warnings.catch_warnings():
                warnings.simplefilter("ignore", UserWarning)
                _write_test_zip(
                    duplicate,
                    [
                        manifest,
                        ("bundle/file", b"one", stat.S_IFREG),
                        ("bundle/file", b"two", stat.S_IFREG),
                    ],
                )
            with self.assertRaisesRegex(common.ScriptError, "duplicate"):
                release._preflight_release_archive(duplicate)

            collision = root / "collision.zip"
            _write_test_zip(
                collision,
                [
                    manifest,
                    ("bundle/File", b"one", stat.S_IFREG),
                    ("bundle/file", b"two", stat.S_IFREG),
                ],
            )
            with self.assertRaisesRegex(common.ScriptError, "case-colliding"):
                release._preflight_release_archive(collision)

            hardlink = root / "hardlink.tar.gz"
            with tarfile.open(hardlink, "w:gz") as archive:
                runtime_manifest = tarfile.TarInfo("bundle/SOURCE-MANIFEST.json")
                runtime_manifest.mode = 0o644
                runtime_manifest.size = 2
                archive.addfile(runtime_manifest, io.BytesIO(b"{}"))
                link = tarfile.TarInfo("bundle/link")
                link.mode = 0o644
                link.type = tarfile.LNKTYPE
                link.linkname = "bundle/SOURCE-MANIFEST.json"
                archive.addfile(link)
            with self.assertRaisesRegex(common.ScriptError, "unsupported metadata"):
                release._preflight_release_archive(hardlink)

    def test_raw_tar_preflight_rejects_large_extension_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            metadata_size = 16 * 1024 * 1024
            for typeflag in (
                tarfile.XGLTYPE,
                tarfile.XHDTYPE,
                tarfile.GNUTYPE_LONGNAME,
                tarfile.GNUTYPE_LONGLINK,
            ):
                path = root / f"extension-{typeflag.hex()}.tar.gz"
                with path.open("wb") as raw:
                    with gzip.GzipFile(
                        filename="",
                        mode="wb",
                        fileobj=raw,
                        mtime=0,
                    ) as compressed:
                        compressed.write(
                            _ustar_header("bundle/metadata", typeflag, metadata_size)
                        )
                        zero_chunk = bytes(1024 * 1024)
                        for _ in range(metadata_size // len(zero_chunk)):
                            compressed.write(zero_chunk)
                with (
                    self.subTest(typeflag=typeflag),
                    patch.object(
                        release,
                        "RELEASE_TAR_MAX_DECOMPRESSED_BYTES",
                        128 * 1024,
                    ),
                    self.assertRaisesRegex(
                        common.ScriptError,
                        "unsupported metadata",
                    ),
                ):
                    release._preflight_raw_tar(path, compressed=True)

    def test_raw_tar_preflight_enforces_canonical_ustar_encoding(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            canonical = _canonical_ustar(
                [
                    ("bundle", tarfile.DIRTYPE, b""),
                    ("bundle/SOURCE-MANIFEST.json", tarfile.REGTYPE, b"{}"),
                ]
            )
            raw_path = root / "canonical.tar"
            raw_path.write_bytes(canonical)
            compressed_path = root / "canonical.tar.gz"
            compressed_path.write_bytes(gzip.compress(canonical, mtime=0))
            release._preflight_raw_tar(raw_path, compressed=False)
            release._preflight_raw_tar(compressed_path, compressed=True)

            invalid_checksum = bytearray(canonical)
            invalid_checksum[0] ^= 1
            invalid_checksum_path = root / "invalid-checksum.tar.gz"
            invalid_checksum_path.write_bytes(gzip.compress(invalid_checksum, mtime=0))
            with self.assertRaisesRegex(common.ScriptError, "checksum is invalid"):
                release._preflight_raw_tar(
                    invalid_checksum_path,
                    compressed=True,
                )

            malformed_size = bytearray(canonical)
            malformed_size[124:136] = b"0000000000x\0"
            malformed_header = bytearray(malformed_size[: tarfile.BLOCKSIZE])
            _replace_tar_checksum(malformed_header)
            malformed_size[: tarfile.BLOCKSIZE] = malformed_header
            malformed_size_path = root / "malformed-size.tar.gz"
            malformed_size_path.write_bytes(gzip.compress(malformed_size, mtime=0))
            with self.assertRaisesRegex(common.ScriptError, "malformed size"):
                release._preflight_raw_tar(
                    malformed_size_path,
                    compressed=True,
                )

            sparse = _canonical_ustar(
                [
                    ("bundle", tarfile.DIRTYPE, b""),
                    ("bundle/sparse", tarfile.GNUTYPE_SPARSE, b""),
                ]
            )
            sparse_path = root / "sparse.tar.gz"
            sparse_path.write_bytes(gzip.compress(sparse, mtime=0))
            with self.assertRaisesRegex(common.ScriptError, "unsupported metadata"):
                release._preflight_raw_tar(sparse_path, compressed=True)

            trailing_gzip = root / "trailing-gzip.tar.gz"
            trailing_gzip.write_bytes(gzip.compress(canonical, mtime=0) + b"trailing")
            with self.assertRaisesRegex(common.ScriptError, "trailing data"):
                release._preflight_raw_tar(trailing_gzip, compressed=True)

            trailing_tar = root / "trailing.tar"
            trailing_tar.write_bytes(canonical + b"trailing")
            with self.assertRaisesRegex(common.ScriptError, "after its end blocks"):
                release._preflight_raw_tar(trailing_tar, compressed=False)

            excess_zero_padding = root / "excess-zero-padding.tar"
            excess_zero_padding.write_bytes(canonical + bytes(tarfile.RECORDSIZE))
            with self.assertRaisesRegex(common.ScriptError, "end padding"):
                release._preflight_raw_tar(excess_zero_padding, compressed=False)

            nonzero_padding = bytearray(canonical)
            nonzero_padding[tarfile.BLOCKSIZE * 2 + 2] = 1
            padding_path = root / "nonzero-padding.tar"
            padding_path.write_bytes(nonzero_padding)
            with self.assertRaisesRegex(common.ScriptError, "nonzero padding"):
                release._preflight_raw_tar(padding_path, compressed=False)

    def test_raw_tar_preflight_enforces_resource_and_collision_limits(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            canonical = _canonical_ustar(
                [
                    ("bundle", tarfile.DIRTYPE, b""),
                    ("bundle/SOURCE-MANIFEST.json", tarfile.REGTYPE, b"{}"),
                ]
            )
            path = root / "bounded.tar"
            path.write_bytes(canonical)
            cases = (
                ("RELEASE_ARCHIVE_MAX_BYTES", len(canonical) - 1, "archive-size"),
                (
                    "RELEASE_TAR_MAX_DECOMPRESSED_BYTES",
                    len(canonical) - 1,
                    "decompressed-size",
                ),
                ("RELEASE_ARCHIVE_MAX_MEMBERS", 1, "member-count"),
                ("RELEASE_ARCHIVE_MAX_MEMBER_BYTES", 1, "size limit"),
                ("RELEASE_ARCHIVE_MAX_TOTAL_BYTES", 1, "uncompressed-size"),
                ("RELEASE_ARCHIVE_MAX_PATH_BYTES", 10, "unsafe path"),
            )
            for limit_name, limit, error in cases:
                with (
                    self.subTest(limit=limit_name),
                    patch.object(release, limit_name, limit),
                    self.assertRaisesRegex(common.ScriptError, error),
                ):
                    release._preflight_raw_tar(path, compressed=False)

            for name, entries, error in (
                (
                    "duplicate",
                    [
                        ("bundle", tarfile.DIRTYPE, b""),
                        ("bundle/file", tarfile.REGTYPE, b"one"),
                        ("bundle/file", tarfile.REGTYPE, b"two"),
                    ],
                    "duplicate",
                ),
                (
                    "collision",
                    [
                        ("bundle", tarfile.DIRTYPE, b""),
                        ("bundle/File", tarfile.REGTYPE, b"one"),
                        ("bundle/file", tarfile.REGTYPE, b"two"),
                    ],
                    "case-colliding",
                ),
            ):
                collision_path = root / f"{name}.tar"
                collision_path.write_bytes(_canonical_ustar(entries))
                with (
                    self.subTest(name=name),
                    self.assertRaisesRegex(common.ScriptError, error),
                ):
                    release._preflight_raw_tar(
                        collision_path,
                        compressed=False,
                    )

    def test_complete_package_validator_rejects_missing_or_renamed_members(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = _make_test_release_package(root / "baseline", "broker-ttrpc")

            def remove_named_alias(package: Path) -> None:
                (package / "guest" / build.AGENT_INITRAMFS_NAME).unlink()

            def remove_kernel_config(package: Path) -> None:
                (package / "guest" / "vmlinux.config").unlink()

            def remove_package_manifest(package: Path) -> None:
                (package / "guest" / "initramfs.cpio.gz.packages.json").unlink()

            def rename_alias(package: Path) -> None:
                (package / "guest" / build.AGENT_INITRAMFS_NAME).rename(
                    package / "guest" / "renamed-initramfs.cpio.gz"
                )

            mutations: tuple[tuple[str, Callable[[Path], None]], ...] = (
                (
                    "missing named alias",
                    remove_named_alias,
                ),
                (
                    "missing kernel config",
                    remove_kernel_config,
                ),
                (
                    "missing package manifest",
                    remove_package_manifest,
                ),
                (
                    "renamed alias",
                    rename_alias,
                ),
            )
            for index, (description, mutate) in enumerate(mutations):
                package = root / f"mutation-{index}"
                shutil.copytree(baseline, package)
                mutate(package)
                common.write_sha256_sums(package)
                archive = root / f"mutation-{index}.zip"
                with (
                    self.subTest(description=description),
                    self.assertRaises(common.ScriptError),
                ):
                    release.create_release_archive(package, archive)
                self.assertFalse(archive.exists())

            valid_archive = root / "valid.zip"
            release.create_release_archive(baseline, valid_archive)
            release.validate_release_archive(
                valid_archive,
                "broker-ttrpc",
                hashlib.sha256(
                    (baseline / "SOURCE-MANIFEST.json").read_bytes()
                ).hexdigest(),
            )

    def test_pinned_broker_policy_rejects_all_substituted_identities(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            pinned_agent = common.REPO_ROOT / "build" / build.GUEST_AGENT_ARTIFACT_NAME
            if not pinned_agent.is_file():
                self.skipTest("pinned NVX guest-agent artifact is not staged")
            agent_bytes = pinned_agent.read_bytes()
            self.assertEqual(
                hashlib.sha256(agent_bytes).hexdigest(),
                build.GUEST_AGENT_SHA256,
            )
            exact = _make_test_release_package(
                root / "exact",
                build.BROKER_TRANSPORT,
                agent_bytes,
            )
            release._validate_release_package_tree(
                exact,
                build.BROKER_TRANSPORT,
                True,
            )

            substituted = _make_test_release_package(
                root / "substituted",
                build.BROKER_TRANSPORT,
            )
            with self.assertRaisesRegex(common.ScriptError, "pinned"):
                release._validate_release_package_tree(
                    substituted,
                    build.BROKER_TRANSPORT,
                    True,
                )
            substituted_archive = root / "substituted.zip"
            release.create_release_archive(substituted, substituted_archive)
            substituted_manifest_bytes = (
                substituted / "SOURCE-MANIFEST.json"
            ).read_bytes()
            substituted_manifest = json.loads(substituted_manifest_bytes)
            substituted_manifest_sha256 = hashlib.sha256(
                substituted_manifest_bytes
            ).hexdigest()
            substituted_manifest_digest = root / "substituted-manifest.sha256"
            substituted_manifest_digest.write_text(
                f"{substituted_manifest_sha256}\n",
                encoding="ascii",
            )
            substituted_guest = substituted / "guest"
            substituted_agent_sha256 = hashlib.sha256(_static_x86_64_elf()).hexdigest()
            substituted_proof = {
                "format": 1,
                "result": "passed",
                "archive_sha256": hashlib.sha256(
                    substituted_archive.read_bytes()
                ).hexdigest(),
                "bundle_manifest_sha256": substituted_manifest_sha256,
                "canonical_manifest_sha256": release._canonical_json_sha256(
                    substituted_manifest
                ),
                "platform": "windows-whp",
                "backend": "whp",
                "transport": build.BROKER_TRANSPORT,
                "artifact_sha256": release._release_artifact_hashes(substituted),
                "openvmm_sha256": hashlib.sha256(b"openvmm").hexdigest(),
                "kernel_sha256": hashlib.sha256(b"vmlinux").hexdigest(),
                "kernel_config_sha256": hashlib.sha256(b"vmlinux.config").hexdigest(),
                "initramfs_sha256": hashlib.sha256(
                    (substituted_guest / "initramfs.cpio.gz").read_bytes()
                ).hexdigest(),
                "package_manifest_sha256": hashlib.sha256(
                    (substituted_guest / "initramfs.cpio.gz.packages.json").read_bytes()
                ).hexdigest(),
                "agent_sha256": substituted_agent_sha256,
                "checks": {
                    "control_auth": True,
                    "get_guest_info": True,
                    "get_guest_info_self_sha256": substituted_agent_sha256,
                    "bootstrap": True,
                    "wait_ready": True,
                    "shutdown": True,
                },
            }
            substituted_proof_path = root / "substituted-proof.json"
            substituted_proof_path.write_text(
                json.dumps(substituted_proof),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(common.ScriptError, "pinned"):
                release.verify_broker_live_gate(
                    substituted_archive,
                    substituted_manifest_digest,
                    substituted_proof_path,
                    hashlib.sha256(substituted_proof_path.read_bytes()).hexdigest(),
                    "windows-whp",
                    hashlib.sha256(substituted_archive.read_bytes()).hexdigest(),
                )

            runtime_mutations: tuple[tuple[str, str, object], ...] = (
                ("runtime", "transport", "legacy"),
                ("runtime", "artifact_profile", "legacy"),
                ("guest_agent", "artifact", "guest/other-agent"),
                (
                    "guest_agent",
                    "initramfs_artifact",
                    "guest/other-initramfs.cpio.gz",
                ),
                ("guest_agent", "target", "x86_64-unknown-linux-gnu"),
                ("guest_agent", "optional", True),
                ("guest_agent", "installed_in_initramfs", False),
                (
                    "guest_agent",
                    "maximum_size_bytes",
                    build.GUEST_AGENT_MAXIMUM_BYTES - 1,
                ),
                ("guest_agent", "source_revision", "0" * 40),
                ("guest_agent", "build_id", "0" * 40),
                ("guest_agent", "external_input_sha256", "0" * 64),
                (
                    "guest_agent",
                    "external_input_size_bytes",
                    build.GUEST_AGENT_SIZE_BYTES - 1,
                ),
                ("guest_agent", "sha256", "0" * 64),
                ("guest_agent", "size", build.GUEST_AGENT_SIZE_BYTES - 1),
                ("guest_agent", "protocol_schema_version", 2),
                ("guest_agent", "transport", "legacy"),
                ("openvmm", "microvm_abi_version", 3),
                ("openvmm", "control_session_protocol_version", 2),
                ("openvmm", "control_contract_revision", "other-control"),
                ("alpine", "initramfs_artifact", "guest/other.cpio.gz"),
            )
            for index, (section_name, field, value) in enumerate(runtime_mutations):
                package = root / f"runtime-mismatch-{index}"
                shutil.copytree(exact, package)
                manifest_path = package / "SOURCE-MANIFEST.json"
                manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
                section = cast(dict[str, object], manifest[section_name])
                section[field] = value
                runtime = cast(dict[str, object], manifest["runtime"])
                identity = release._runtime_identity(manifest)
                runtime["fingerprint_inputs"] = identity
                runtime["guest_fingerprint_sha256"] = release._runtime_fingerprint(
                    identity
                )
                manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
                common.write_sha256_sums(package)
                with (
                    self.subTest(section=section_name, field=field),
                    self.assertRaises(common.ScriptError),
                ):
                    release._validate_release_package_tree(
                        package,
                        build.BROKER_TRANSPORT,
                        True,
                    )

            package_mutations: tuple[tuple[str, object], ...] = (
                ("path", "/sbin/other-agent"),
                ("sha256", "0" * 64),
                ("size", build.GUEST_AGENT_SIZE_BYTES - 1),
                ("source_revision", "0" * 40),
                ("build_id", "0" * 40),
            )
            for index, (field, value) in enumerate(package_mutations):
                package = root / f"package-mismatch-{index}"
                shutil.copytree(exact, package)
                guest = package / "guest"
                package_manifest_path = guest / "initramfs.cpio.gz.packages.json"
                package_manifest = json.loads(
                    package_manifest_path.read_text(encoding="utf-8")
                )
                package_agent = cast(
                    dict[str, object],
                    package_manifest["guest_agent"],
                )
                package_agent[field] = value
                package_manifest_bytes = (
                    json.dumps(package_manifest, indent=2) + "\n"
                ).encode("utf-8")
                package_manifest_path.write_bytes(package_manifest_bytes)
                (guest / f"{build.AGENT_INITRAMFS_NAME}.packages.json").write_bytes(
                    package_manifest_bytes
                )
                manifest_path = package / "SOURCE-MANIFEST.json"
                manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
                alpine = cast(dict[str, object], manifest["alpine"])
                alpine["initramfs_package_manifest_sha256"] = hashlib.sha256(
                    package_manifest_bytes
                ).hexdigest()
                runtime = cast(dict[str, object], manifest["runtime"])
                identity = release._runtime_identity(manifest)
                runtime["fingerprint_inputs"] = identity
                runtime["guest_fingerprint_sha256"] = release._runtime_fingerprint(
                    identity
                )
                manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
                common.write_sha256_sums(package)
                with (
                    self.subTest(package_manifest_field=field),
                    self.assertRaises(common.ScriptError),
                ):
                    release._validate_release_package_tree(
                        package,
                        build.BROKER_TRANSPORT,
                        True,
                    )

    def test_root_source_manifest_pins_every_broker_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            original = json.loads(
                (common.REPO_ROOT / "SOURCE-MANIFEST.json").read_text(encoding="utf-8")
            )
            manifest_path = root / "SOURCE-MANIFEST.json"
            manifest_path.write_text(json.dumps(original), encoding="utf-8")
            with patch.object(release, "REPO_ROOT", root):
                release._validate_root_broker_contract()

            mutations: tuple[tuple[str, str, object], ...] = (
                ("guest_agent", "artifact", "guest/other-agent"),
                (
                    "guest_agent",
                    "initramfs_artifact",
                    "guest/other-initramfs.cpio.gz",
                ),
                ("guest_agent", "target", "x86_64-unknown-linux-gnu"),
                ("guest_agent", "optional", False),
                ("guest_agent", "installed_in_initramfs", True),
                (
                    "guest_agent",
                    "maximum_size_bytes",
                    build.GUEST_AGENT_MAXIMUM_BYTES - 1,
                ),
                ("guest_agent", "source_revision", "0" * 40),
                ("guest_agent", "build_id", "0" * 40),
                ("guest_agent", "external_input_sha256", "0" * 64),
                (
                    "guest_agent",
                    "external_input_size_bytes",
                    build.GUEST_AGENT_SIZE_BYTES - 1,
                ),
                ("guest_agent", "protocol_schema_version", 2),
                ("guest_agent", "transport", "legacy"),
                ("openvmm", "microvm_abi_version", 3),
                ("openvmm", "control_session_protocol_version", 2),
                ("openvmm", "control_contract_revision", "other-control"),
            )
            for section_name, field, value in mutations:
                changed = json.loads(json.dumps(original))
                section = cast(dict[str, object], changed[section_name])
                section[field] = value
                manifest_path.write_text(json.dumps(changed), encoding="utf-8")
                with (
                    self.subTest(section=section_name, field=field),
                    patch.object(release, "REPO_ROOT", root),
                    self.assertRaisesRegex(common.ScriptError, "pinned broker"),
                ):
                    release._validate_root_broker_contract()

    def test_package_validator_rejects_cross_profile_initramfs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for declared, embedded in (
                ("broker-ttrpc", "legacy"),
                ("legacy", "broker-ttrpc"),
            ):
                package = _make_test_release_package(
                    root / declared,
                    declared,
                )
                agent = _static_x86_64_elf()
                image = (
                    _agent_newc_archive(agent)
                    if embedded == "broker-ttrpc"
                    else _legacy_newc_archive(
                        (common.REPO_ROOT / "alpine" / "init").read_bytes()
                    )
                )
                package_manifest = _initramfs_package_manifest(
                    declared,
                    image,
                    agent if declared == "broker-ttrpc" else None,
                )
                guest = package / "guest"
                (guest / "initramfs.cpio.gz").write_bytes(image)
                (guest / "initramfs.cpio.gz.packages.json").write_bytes(
                    package_manifest
                )
                if declared == "broker-ttrpc":
                    (guest / build.AGENT_INITRAMFS_NAME).write_bytes(image)
                    (guest / f"{build.AGENT_INITRAMFS_NAME}.packages.json").write_bytes(
                        package_manifest
                    )
                runtime_manifest = json.loads(
                    (package / "SOURCE-MANIFEST.json").read_text(encoding="utf-8")
                )
                alpine = runtime_manifest["alpine"]
                alpine["initramfs_sha256"] = hashlib.sha256(image).hexdigest()
                alpine["initramfs_package_manifest_sha256"] = hashlib.sha256(
                    package_manifest
                ).hexdigest()
                runtime = runtime_manifest["runtime"]
                identity = release._runtime_identity(runtime_manifest)
                runtime["fingerprint_inputs"] = identity
                runtime["guest_fingerprint_sha256"] = release._runtime_fingerprint(
                    identity
                )
                (package / "SOURCE-MANIFEST.json").write_text(
                    json.dumps(runtime_manifest),
                    encoding="utf-8",
                )
                common.write_sha256_sums(package)
                with (
                    self.subTest(declared=declared, embedded=embedded),
                    self.assertRaises(common.ScriptError),
                ):
                    release._validate_release_package_tree(package, declared)

    def test_release_archive_installs_runtime_layout(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            pinned_agent = common.REPO_ROOT / "build" / build.GUEST_AGENT_ARTIFACT_NAME
            if not pinned_agent.is_file():
                self.skipTest("pinned NVX guest-agent artifact is not staged")
            agent_bytes = pinned_agent.read_bytes()
            self.assertEqual(
                hashlib.sha256(agent_bytes).hexdigest(),
                build.GUEST_AGENT_SHA256,
            )
            self.assertEqual(len(agent_bytes), build.GUEST_AGENT_SIZE_BYTES)
            package_root = root / "package" / "nvx-1.2.3-test"
            binary_name = "openvmm.exe" if os.name == "nt" else "openvmm"
            binary = package_root / "bin" / binary_name
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"openvmm")
            for name in release.GUEST_RELEASE_NAMES:
                guest = package_root / "guest" / name
                guest.parent.mkdir(parents=True, exist_ok=True)
                guest.write_bytes(name.encode("ascii"))
            agent_initramfs = package_root / "guest" / build.AGENT_INITRAMFS_NAME
            agent_initramfs.write_bytes(_agent_newc_archive(agent_bytes))
            (
                package_root / "guest" / f"{build.AGENT_INITRAMFS_NAME}.packages.json"
            ).write_text("{}", encoding="ascii")
            (package_root / "guest" / "initramfs.cpio.gz").write_bytes(
                agent_initramfs.read_bytes()
            )
            agent = package_root / "guest" / build.GUEST_AGENT_ARTIFACT_NAME
            agent.write_bytes(agent_bytes)
            agent_sha256 = hashlib.sha256(agent.read_bytes()).hexdigest()
            package_manifest_bytes = _initramfs_package_manifest(
                "broker-ttrpc",
                agent_initramfs.read_bytes(),
                agent.read_bytes(),
            )
            (package_root / "guest" / "initramfs.cpio.gz.packages.json").write_bytes(
                package_manifest_bytes
            )
            (
                package_root / "guest" / f"{build.AGENT_INITRAMFS_NAME}.packages.json"
            ).write_bytes(package_manifest_bytes)
            for name in ("LICENSE", "README.md", "THIRD_PARTY_NOTICES.md"):
                (package_root / name).write_text(name, encoding="ascii")
            for name in ("COPYING-LINUX", "LICENSE-OPENVMM"):
                license_path = package_root / "licenses" / name
                license_path.parent.mkdir(parents=True, exist_ok=True)
                license_path.write_text(name, encoding="ascii")
            root_manifest = json.loads(
                (common.REPO_ROOT / "SOURCE-MANIFEST.json").read_text(encoding="utf-8")
            )
            source_revision = cast(
                str,
                root_manifest["openvmm"]["source_revision"],
            )
            manifest: dict[str, object] = {
                "format": 1,
                "runtime": {
                    "transport": "broker-ttrpc",
                    "artifact_profile": "broker-ttrpc",
                    "immutable_bundle": True,
                },
                "openvmm": {
                    "source_revision": source_revision,
                    "executable_sha256": hashlib.sha256(b"openvmm").hexdigest(),
                    "microvm_abi_version": 2,
                    "control_session_protocol_version": 1,
                    "control_contract_revision": "nvx-microvm-v2-control-v1",
                },
                "linux": {
                    "kernel_sha256": hashlib.sha256(b"vmlinux").hexdigest(),
                    "config_sha256": hashlib.sha256(b"vmlinux.config").hexdigest(),
                },
                "alpine": {
                    "initramfs_sha256": hashlib.sha256(
                        agent_initramfs.read_bytes()
                    ).hexdigest(),
                    "initramfs_package_manifest_sha256": hashlib.sha256(
                        package_manifest_bytes
                    ).hexdigest(),
                    "profile": "broker-ttrpc",
                    "initramfs_artifact": "guest/initramfs.cpio.gz",
                },
                "guest_agent": {
                    "artifact": build.GUEST_AGENT_ARTIFACT_PATH,
                    "initramfs_artifact": build.GUEST_AGENT_INITRAMFS_ARTIFACT_PATH,
                    "target": build.GUEST_AGENT_TARGET,
                    "optional": False,
                    "sha256": agent_sha256,
                    "size": agent.stat().st_size,
                    "installed_in_initramfs": True,
                    "maximum_size_bytes": build.GUEST_AGENT_MAXIMUM_BYTES,
                    "source_revision": build.GUEST_AGENT_SOURCE_REVISION,
                    "build_id": build.GUEST_AGENT_BUILD_ID,
                    "external_input_sha256": agent_sha256,
                    "external_input_size_bytes": agent.stat().st_size,
                    "protocol_schema_version": (
                        build.GUEST_AGENT_PROTOCOL_SCHEMA_VERSION
                    ),
                    "transport": build.BROKER_TRANSPORT,
                },
            }
            runtime = cast(dict[str, object], manifest["runtime"])
            identity = release._runtime_identity(manifest)
            runtime["fingerprint_inputs"] = identity
            runtime["guest_fingerprint_sha256"] = release._runtime_fingerprint(identity)
            (package_root / "SOURCE-MANIFEST.json").write_text(
                json.dumps(manifest),
                encoding="utf-8",
            )
            common.write_sha256_sums(package_root)
            trusted_manifest_sha256 = hashlib.sha256(
                (package_root / "SOURCE-MANIFEST.json").read_bytes()
            ).hexdigest()
            if os.name == "nt":
                archive_path = root / "nvx-1.2.3-windows-whp.zip"
            else:
                archive_path = root / "nvx-1.2.3-linux-kvm.tar.gz"
            release.create_release_archive(package_root, archive_path)
            archive_sha256 = hashlib.sha256(archive_path.read_bytes()).hexdigest()
            artifact_hashes = release._release_artifact_hashes(package_root)
            manifest_digest = root / "SOURCE-MANIFEST.sha256"
            manifest_digest.write_text(
                f"{trusted_manifest_sha256}\n",
                encoding="ascii",
            )
            platform = "windows-whp" if os.name == "nt" else "linux-kvm"
            proof = {
                "format": 1,
                "result": "passed",
                "archive_sha256": archive_sha256,
                "bundle_manifest_sha256": trusted_manifest_sha256,
                "canonical_manifest_sha256": release._canonical_json_sha256(manifest),
                "platform": platform,
                "backend": platform.rsplit("-", maxsplit=1)[-1],
                "transport": "broker-ttrpc",
                "artifact_sha256": artifact_hashes,
                "openvmm_sha256": hashlib.sha256(b"openvmm").hexdigest(),
                "kernel_sha256": hashlib.sha256(b"vmlinux").hexdigest(),
                "kernel_config_sha256": hashlib.sha256(b"vmlinux.config").hexdigest(),
                "initramfs_sha256": hashlib.sha256(
                    agent_initramfs.read_bytes()
                ).hexdigest(),
                "package_manifest_sha256": hashlib.sha256(
                    package_manifest_bytes
                ).hexdigest(),
                "agent_sha256": agent_sha256,
                "checks": {
                    "control_auth": True,
                    "get_guest_info": True,
                    "get_guest_info_self_sha256": agent_sha256,
                    "bootstrap": True,
                    "wait_ready": True,
                    "shutdown": True,
                },
            }
            proof_path = root / "live-gate.json"
            proof_path.write_text(json.dumps(proof), encoding="utf-8")
            proof_sha256 = hashlib.sha256(proof_path.read_bytes()).hexdigest()
            release.verify_broker_live_gate(
                archive_path,
                manifest_digest,
                proof_path,
                proof_sha256,
                platform,
                archive_sha256,
            )
            changed_archive = root / (
                "changed.zip" if os.name == "nt" else "changed.tar.gz"
            )
            shutil.copyfile(archive_path, changed_archive)
            if os.name == "nt":
                with zipfile.ZipFile(changed_archive, "a") as changed_zip:
                    changed_zip.comment = b"changed archive bytes"
            else:
                changed_bytes = bytearray(changed_archive.read_bytes())
                changed_bytes[9] ^= 1
                changed_archive.write_bytes(changed_bytes)
            changed_sha256 = hashlib.sha256(changed_archive.read_bytes()).hexdigest()
            with self.assertRaisesRegex(common.ScriptError, "archive_sha256"):
                release.verify_broker_live_gate(
                    changed_archive,
                    manifest_digest,
                    proof_path,
                    proof_sha256,
                    platform,
                    changed_sha256,
                )
            proof_checks = cast(dict[str, object], proof["checks"])
            for field, container in (
                ("agent_sha256", cast(dict[str, object], proof)),
                ("get_guest_info_self_sha256", proof_checks),
            ):
                original = container[field]
                container[field] = "0" * 64
                proof_path.write_text(json.dumps(proof), encoding="utf-8")
                with (
                    self.subTest(proof_field=field),
                    self.assertRaises(common.ScriptError),
                ):
                    release.verify_broker_live_gate(
                        archive_path,
                        manifest_digest,
                        proof_path,
                        hashlib.sha256(proof_path.read_bytes()).hexdigest(),
                        platform,
                        archive_sha256,
                    )
                container[field] = original
            proof_checks["shutdown"] = False
            proof_path.write_text(json.dumps(proof), encoding="utf-8")
            with self.assertRaisesRegex(common.ScriptError, "exact successful"):
                release.verify_broker_live_gate(
                    archive_path,
                    manifest_digest,
                    proof_path,
                    hashlib.sha256(proof_path.read_bytes()).hexdigest(),
                    platform,
                    archive_sha256,
                )

            build_dir = root / "runtime" / "build"
            build_dir.mkdir(parents=True)
            binary_destination = (
                root / "runtime" / "openvmm" / "target" / "release" / binary_name
            )
            prior_paths = [
                binary_destination,
                *(
                    build_dir / name
                    for name in (
                        "vmlinux",
                        "vmlinux.config",
                        "initramfs.cpio.gz",
                        "initramfs.cpio.gz.packages.json",
                        build.AGENT_INITRAMFS_NAME,
                        f"{build.AGENT_INITRAMFS_NAME}.packages.json",
                        build.GUEST_AGENT_ARTIFACT_NAME,
                        build.GUEST_AGENT_SHA256_NAME,
                        build.OPENVMM_PROVENANCE_NAME,
                    )
                ),
            ]
            for index, path in enumerate(prior_paths):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(f"prior-{index}".encode("ascii"))
                path.chmod(0o700 if index % 2 else 0o600)

            def snapshot() -> dict[Path, tuple[bytes, int] | None]:
                return {
                    path: (
                        (path.read_bytes(), stat.S_IMODE(path.stat().st_mode))
                        if path.exists()
                        else None
                    )
                    for path in prior_paths
                }

            prior_snapshot = snapshot()

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
                with self.assertRaisesRegex(
                    common.ScriptError,
                    "independently delivered",
                ):
                    release._install_release_archive(
                        archive_path,
                        "broker-ttrpc",
                    )
                with self.assertRaisesRegex(
                    common.ScriptError,
                    "expected 'legacy'",
                ):
                    release._install_release_archive(
                        archive_path,
                        "legacy",
                    )
                original_replace = release._atomic_replace
                for failure_call in range(1, 19):
                    calls = 0

                    def fail_one(
                        source: Path,
                        destination: Path,
                        target_call: int = failure_call,
                    ) -> None:
                        nonlocal calls
                        calls += 1
                        if calls == target_call:
                            raise common.ScriptError("injected promotion failure")
                        original_replace(source, destination)

                    with (
                        self.subTest(failure_call=failure_call),
                        patch.object(
                            release,
                            "_atomic_replace",
                            side_effect=fail_one,
                        ),
                        self.assertRaisesRegex(
                            common.ScriptError,
                            "injected promotion failure",
                        ),
                    ):
                        release._install_release_archive(
                            archive_path,
                            "broker-ttrpc",
                            trusted_manifest_sha256,
                        )
                    self.assertEqual(snapshot(), prior_snapshot)
                release._install_release_archive(
                    archive_path,
                    "broker-ttrpc",
                    trusted_manifest_sha256,
                )

            self.assertEqual(binary_destination.read_bytes(), b"openvmm")
            for name in ("vmlinux", "vmlinux.config"):
                self.assertEqual(
                    (build_dir / name).read_bytes(),
                    name.encode("ascii"),
                )
            self.assertEqual(
                (build_dir / build.AGENT_INITRAMFS_NAME).read_bytes(),
                agent_initramfs.read_bytes(),
            )
            self.assertFalse((build_dir / "initramfs.cpio.gz").exists())
            self.assertEqual(
                (build_dir / build.GUEST_AGENT_ARTIFACT_NAME).read_bytes(),
                agent.read_bytes(),
            )
            self.assertEqual(
                (build_dir / build.GUEST_AGENT_SHA256_NAME)
                .read_text(encoding="ascii")
                .strip(),
                agent_sha256,
            )
            provenance = json.loads(
                (build_dir / build.OPENVMM_PROVENANCE_NAME).read_text(encoding="utf-8")
            )
            self.assertEqual(provenance["source_revision"], source_revision)
            self.assertEqual(provenance["origin"], "release")
            self.assertTrue(release._runtime_mode_matches(binary_destination, 0o755))
            self.assertTrue(
                release._runtime_mode_matches(
                    build_dir / build.GUEST_AGENT_ARTIFACT_NAME,
                    0o755,
                )
            )
            for name in (
                "vmlinux",
                "vmlinux.config",
                build.AGENT_INITRAMFS_NAME,
                f"{build.AGENT_INITRAMFS_NAME}.packages.json",
                build.GUEST_AGENT_SHA256_NAME,
                build.OPENVMM_PROVENANCE_NAME,
            ):
                self.assertTrue(
                    release._runtime_mode_matches(
                        build_dir / name,
                        0o644,
                    ),
                )

            installed_snapshot = snapshot()
            tampered_entries = _agent_newc_entries(agent.read_bytes())
            group_index = next(
                index
                for index, entry in enumerate(tampered_entries)
                if entry[0] == "etc/group"
            )
            tampered_entries[group_index] = (
                "etc/group",
                0o100644,
                b"root:x:0:\nchanged:x:1:\n",
            )
            tampered_image = _newc_archive(tampered_entries)
            agent_initramfs.write_bytes(tampered_image)
            (package_root / "guest" / "initramfs.cpio.gz").write_bytes(tampered_image)
            common.write_sha256_sums(package_root)
            tampered_archive = root / (
                "tampered.zip" if os.name == "nt" else "tampered.tar.gz"
            )
            with self.assertRaisesRegex(common.ScriptError, "packaged identity"):
                release.create_release_archive(package_root, tampered_archive)
            self.assertEqual(snapshot(), installed_snapshot)
            agent_initramfs.write_bytes(_agent_newc_archive(agent.read_bytes()))
            (package_root / "guest" / "initramfs.cpio.gz").write_bytes(
                agent_initramfs.read_bytes()
            )
            common.write_sha256_sums(package_root)

            agent.unlink()
            common.write_sha256_sums(package_root)
            invalid_archive = root / (
                "invalid.zip" if os.name == "nt" else "invalid.tar.gz"
            )
            binary_destination.write_bytes(b"existing")
            with self.assertRaisesRegex(common.ScriptError, "artifact names"):
                release.create_release_archive(package_root, invalid_archive)
            self.assertEqual(binary_destination.read_bytes(), b"existing")

            agent_initramfs.unlink()
            (
                package_root / "guest" / f"{build.AGENT_INITRAMFS_NAME}.packages.json"
            ).unlink()
            legacy_initramfs = _legacy_newc_archive(
                (common.REPO_ROOT / "alpine" / "init").read_bytes()
            )
            legacy_package_manifest = _initramfs_package_manifest(
                "legacy",
                legacy_initramfs,
            )
            (package_root / "guest" / "initramfs.cpio.gz").write_bytes(legacy_initramfs)
            (package_root / "guest" / "initramfs.cpio.gz.packages.json").write_bytes(
                legacy_package_manifest
            )
            manifest_alpine = cast(dict[str, object], manifest["alpine"])
            manifest_alpine["initramfs_sha256"] = hashlib.sha256(
                legacy_initramfs
            ).hexdigest()
            manifest_alpine["initramfs_package_manifest_sha256"] = hashlib.sha256(
                legacy_package_manifest
            ).hexdigest()
            manifest_alpine["profile"] = "legacy"
            manifest_agent = cast(dict[str, object], manifest["guest_agent"])
            manifest_agent["sha256"] = None
            manifest_agent["size"] = None
            manifest_agent["installed_in_initramfs"] = False
            runtime["transport"] = "legacy"
            runtime["artifact_profile"] = "legacy"
            identity = release._runtime_identity(manifest)
            runtime["fingerprint_inputs"] = identity
            runtime["guest_fingerprint_sha256"] = release._runtime_fingerprint(identity)
            (package_root / "SOURCE-MANIFEST.json").write_text(
                json.dumps(manifest),
                encoding="utf-8",
            )
            common.write_sha256_sums(package_root)
            legacy_archive = root / (
                "legacy.zip" if os.name == "nt" else "legacy.tar.gz"
            )
            release.create_release_archive(package_root, legacy_archive)
            with (
                patch.object(release, "artifact_path", side_effect=artifact_path),
                patch.object(
                    release,
                    "openvmm_binary_path",
                    return_value=binary_destination,
                ),
            ):
                release._install_release_archive(legacy_archive, "legacy")
            self.assertEqual(
                (build_dir / "initramfs.cpio.gz").read_bytes(),
                legacy_initramfs,
            )
            self.assertFalse((build_dir / build.GUEST_AGENT_ARTIFACT_NAME).exists())
            self.assertFalse((build_dir / build.GUEST_AGENT_SHA256_NAME).exists())
            self.assertFalse((build_dir / build.AGENT_INITRAMFS_NAME).exists())
            self.assertTrue(
                release._runtime_mode_matches(
                    build_dir / "initramfs.cpio.gz",
                    0o644,
                )
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
