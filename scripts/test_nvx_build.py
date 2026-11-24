#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import hashlib
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import call, patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import build, build_config, common  # noqa: E402
from nvx_tools.build_constants import (  # noqa: E402
    BuildConstants,
    KernelBuildConstants,
    OpenVMMBuildConstants,
)


class BuildOrchestrationTests(unittest.TestCase):
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

    def test_guest_build_adds_debug_kernel_only_when_requested(self):
        with (
            patch.object(build, "build_kernel") as kernel,
            patch.object(build, "build_initramfs"),
            patch.object(build, "build_docker_debug_kernel") as docker_debug,
        ):
            native = build_config.BuildConfig(native_guest=True, debug_kernel=True)
            build.build_guest(native)
            self.assertEqual(
                kernel.call_args_list,
                [
                    call(native.kernel),
                    call(build_config.KernelBuildConfig.debug_variant()),
                ],
            )
            docker_debug.assert_not_called()

        with (
            patch.object(build, "build_docker_artifacts") as docker,
            patch.object(build, "build_docker_debug_kernel") as docker_debug,
        ):
            portable = build_config.BuildConfig(debug_kernel=True)
            build.build_guest(portable)
            docker.assert_called_once_with(portable.docker, "alpine")
            docker_debug.assert_called_once_with(portable.docker)

        with (
            patch.object(build, "build_docker_artifacts"),
            patch.object(build, "build_docker_debug_kernel") as docker_debug,
        ):
            build.build_guest(build_config.BuildConfig())
            docker_debug.assert_not_called()

        debug = build_config.KernelBuildConfig.debug_variant()
        self.assertTrue(debug.debug)
        self.assertEqual(debug.output.name, KernelBuildConstants.DEBUG_BINARY_NAME)
        self.assertEqual(
            debug.work.name, KernelBuildConstants.DEBUG_WORK_DIRECTORY_NAME
        )


class OpenVmmBuildTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
