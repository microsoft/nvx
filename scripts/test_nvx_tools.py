#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import archive, benchmark, build, ci, common, release  # noqa: E402


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
                patch.object(ci, "setup_cargo_nextest") as setup_cargo_nextest,
                patch.object(ci, "run_checked") as run_checked,
            ):
                ci.run_openvmm_tests(backend)

            setup_cargo_nextest.assert_called_once()
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

    def test_cargo_nextest_setup_installs_verified_prebuilt_binary(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target, target_sha256 = ci._nextest_target()
            binary_name = "cargo-nextest.exe" if os.name == "nt" else "cargo-nextest"
            source_archive = root / "source.tar.gz"
            source_binary = root / binary_name
            source_binary.write_bytes(b"cargo-nextest")
            with tarfile.open(source_archive, "w:gz") as package:
                package.add(source_binary, arcname=f"package/{binary_name}")

            def copy_archive(
                url: str,
                destination: Path,
                attempts: int = 3,
                *,
                expected_sha256: str | None = None,
                headers: dict[str, str] | None = None,
            ) -> None:
                self.assertEqual(
                    url,
                    (
                        f"{ci.NEXTEST_RELEASE_URL}/cargo-nextest-"
                        f"{ci.NEXTEST_VERSION}-{target}.tar.gz"
                    ),
                )
                self.assertEqual(expected_sha256, target_sha256)
                self.assertEqual(attempts, 3)
                self.assertIsNone(headers)
                shutil.copyfile(source_archive, destination)

            env = {"RUNNER_TEMP": os.fspath(root / "runner-temp"), "PATH": "original"}
            with (
                patch.object(ci, "download", side_effect=copy_archive),
                patch.object(ci, "run_checked") as run_checked,
            ):
                ci.setup_cargo_nextest(env)

            destination = (
                root
                / "runner-temp"
                / f"cargo-nextest-{ci.NEXTEST_VERSION}-{target}"
                / binary_name
            )
            self.assertEqual(destination.read_bytes(), b"cargo-nextest")
            self.assertEqual(
                env["PATH"].split(os.pathsep),
                [os.fspath(destination.parent), "original"],
            )
            run_checked.assert_called_once_with([destination, "--version"], env=env)

    def test_openvmm_tests_reject_unknown_backend(self):
        with self.assertRaisesRegex(common.ScriptError, "unsupported.*backend"):
            ci.run_openvmm_tests("unknown")


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
            command[-4:], ["--net", "10.0.0.2/24", "--mount", "/mnt/host,C:/work,rw"]
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
