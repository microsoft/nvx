#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import argparse
import contextlib
import hashlib
import io
import json
import os
import sys
import tempfile
import unittest
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import (  # noqa: E402
    azurelinux,
    benchmark,
    build,
    build_config,
    common,
    guest_binaries,
    guests,
    ubuntu,
)
from nvx_tools.build_constants import (  # noqa: E402
    BuildConstants,
    GuestRustBuildConstants,
    InitramfsBuildConstants,
    UbuntuBuildConstants,
)


class GuestBuildTests(unittest.TestCase):
    def test_guest_artifact_inputs_cover_every_rust_binary_source(self):
        rust_sources = set(guest_binaries.guest_binary_source_files())
        inputs = {
            "alpine provenance": set(build._initramfs_source_files()),
            "alpine customization": set(
                build._guest_customization_files(guests.ALPINE_GUEST)
            ),
            "ubuntu customization": set(ubuntu.customization_files()),
            "azure linux": set(azurelinux.input_files()),
        }
        for name, files in inputs.items():
            with self.subTest(inputs=name):
                self.assertTrue(rust_sources.issubset(files))
        self.assertIn(
            BuildConstants.REPO_ROOT / "scripts" / "nvx_tools" / "guest_binaries.py",
            inputs["alpine provenance"],
        )

    def test_guest_artifact_identities_normalize_source_line_endings(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "nvx" / "src" / "lib.rs"
            source.parent.mkdir(parents=True)
            source.write_bytes(b"pub fn fixture() {}\n")
            with (
                patch.object(BuildConstants, "REPO_ROOT", root),
                patch.object(build, "_initramfs_source_files", return_value=(source,)),
                patch.object(ubuntu, "load_package_lock", return_value=()),
                patch.object(azurelinux, "input_files", return_value=(source,)),
            ):
                lf = {
                    "alpine": build.initramfs_provenance_inputs(),
                    "ubuntu": ubuntu.converter_input_sha256((source,)),
                    "azurelinux": azurelinux.input_sha256(),
                }
                source.write_bytes(b"pub fn fixture() {}\r\n")
                crlf = {
                    "alpine": build.initramfs_provenance_inputs(),
                    "ubuntu": ubuntu.converter_input_sha256((source,)),
                    "azurelinux": azurelinux.input_sha256(),
                }

        self.assertEqual(crlf, lf)

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
        self.assertEqual(ubuntu_guest.default_memory_mib, 512)
        self.assertFalse(ubuntu_guest.sandbox_control)
        self.assertEqual(
            azurelinux_guest.initramfs_name,
            "initramfs-azurelinux.cpio.gz",
        )
        self.assertEqual(azurelinux_guest.default_memory_mib, 512)
        self.assertFalse(azurelinux_guest.sandbox_control)

    def test_guest_init_runs_virtfs_helper_from_installed_path(self):
        installed: list[Path] = []

        def install(_source: Path, destination: Path) -> dict[str, str]:
            installed.append(destination)
            return {}

        def stage_binaries(output: Path) -> guest_binaries.GuestBinaryBuildResult:
            output.mkdir()
            binaries: dict[str, str] = {}
            for name in GuestRustBuildConstants.BINARIES:
                path = output / name
                path.write_bytes(name.encode("ascii"))
                binaries[name] = common.sha256_file(path)
            return guest_binaries.GuestBinaryBuildResult(
                source_sha256="a" * 64,
                binaries=binaries,
            )

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "rootfs"
            work = Path(temporary) / "work"
            (root / "sbin").mkdir(parents=True)
            (root / "etc").mkdir()
            work.mkdir()
            with (
                patch.object(build, "_install", side_effect=install),
                patch.object(build, "build_guest_binaries", side_effect=stage_binaries),
            ):
                build._install_guest_files(
                    build_config.InitramfsBuildConfig(work=work),
                    root,
                    guests.ALPINE_GUEST,
                )
            self.assertIn(root / "sbin" / "nvx-hostmount", installed)

        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        azure_stage = dockerfile.split("FROM guest-base AS azurelinux-initramfs", 1)[1]
        azure_sbin_scripts = azure_stage.split(
            "install -m 0755 /repo/guest/common/init /rootfs/init\n", 1
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

    def test_device_io_provenance_uses_shared_rust_source_digest(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            checkout = root / "nvx"
            crate = checkout / "nvx"
            (crate / "src").mkdir(parents=True)
            sources = {
                crate / "Cargo.toml": "[package]\nname = 'fixture'\n",
                crate / "Cargo.lock": "# lock\n",
                crate / "src" / "lib.rs": "pub fn fixture() {}\n",
                checkout / "rust-toolchain.toml": (
                    f'[toolchain]\nchannel = "{GuestRustBuildConstants.TOOLCHAIN}"\n'
                ),
                checkout / "scripts" / "nvx_tools" / "build_constants.py": (
                    "PROFILE = 'guest'\n"
                ),
                checkout / "scripts" / "nvx_tools" / "guest_binaries.py": (
                    "def build(): pass\n"
                ),
            }
            for path, contents in sources.items():
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(contents, encoding="utf-8")
            source_sha256 = guest_binaries.guest_binary_source_sha256(checkout)
            initrd = root / "initramfs.cpio.gz"
            manifest = initrd.with_name(f"{initrd.name}.packages.json")
            manifest.write_text(
                json.dumps(
                    {
                        "helpers": {
                            "nvx-device-io": {
                                "source_sha256": source_sha256,
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

            self.assertEqual(provenance["source"], str(crate.resolve()))
            self.assertEqual(provenance["source_sha256"], source_sha256)
            self.assertEqual(provenance["toolchain"], GuestRustBuildConstants.TOOLCHAIN)
            self.assertEqual(provenance["target"], GuestRustBuildConstants.TARGET)

            with (
                patch.object(GuestRustBuildConstants, "TOOLCHAIN", "9.9.9"),
                self.assertRaisesRegex(
                    common.ScriptError,
                    "Rust guest build toolchain mismatch",
                ),
            ):
                benchmark._device_io_helper_provenance(
                    argparse.Namespace(nvx_dir=checkout),
                    initrd,
                )

            (crate / "src" / "lib.rs").write_text("changed\n", encoding="utf-8")
            with self.assertRaisesRegex(
                ValueError,
                "Rust guest binary sources do not match",
            ):
                benchmark._device_io_helper_provenance(
                    argparse.Namespace(nvx_dir=checkout),
                    initrd,
                )

    def test_guest_files_install_one_checked_rust_build(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "root"
            work = Path(temporary) / "work"
            for directory in (root / "sbin", root / "etc", work):
                directory.mkdir(parents=True)
            payloads = {
                name: f"rust-guest-binary {name}".encode()
                for name in GuestRustBuildConstants.BINARIES
            }
            result = guest_binaries.GuestBinaryBuildResult(
                source_sha256="a" * 64,
                binaries={
                    name: hashlib.sha256(payload).hexdigest()
                    for name, payload in payloads.items()
                },
            )

            def stage_binaries(output: Path) -> guest_binaries.GuestBinaryBuildResult:
                output.mkdir()
                for name, payload in payloads.items():
                    (output / name).write_bytes(payload)
                return result

            with patch.object(
                build, "build_guest_binaries", side_effect=stage_binaries
            ) as rust_build:
                helpers = build._install_guest_files(
                    build_config.InitramfsBuildConfig(work=work),
                    root,
                    guests.guest_descriptor("alpine"),
                )

            rust_build.assert_called_once_with(
                work / GuestRustBuildConstants.OUTPUT_DIRECTORY_NAME
            )
            for name, payload in payloads.items():
                with self.subTest(helper=name):
                    installed = root / "sbin" / name
                    self.assertEqual(installed.read_bytes(), payload)
                    if os.name != "nt":
                        self.assertEqual(installed.stat().st_mode & 0o777, 0o755)
                    self.assertEqual(
                        helpers[name],
                        {
                            "source_sha256": result.source_sha256,
                            "binary_sha256": result.binaries[name],
                        },
                    )
        for name in (
            "STATIC_HELPER_CFLAGS",
            "DEVICE_IO_CFLAGS",
            "STATIC_HELPERS",
            "MUSL_COMPILER",
            "MUSL_STATIC_HELPERS",
            "TIME_PROBE_CFLAGS",
        ):
            with self.subTest(helper=name):
                self.assertFalse(hasattr(InitramfsBuildConstants, name))

    def test_guest_files_reuse_the_configured_prebuilt_rust_binaries(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "root"
            work = Path(temporary) / "work"
            prebuilt = Path(temporary) / "prebuilt"
            for directory in (root / "sbin", root / "etc", work, prebuilt):
                directory.mkdir(parents=True)
            binaries: dict[str, str] = {}
            for name in GuestRustBuildConstants.BINARIES:
                path = prebuilt / name
                path.write_bytes(name.encode("ascii"))
                binaries[name] = common.sha256_file(path)
            result = guest_binaries.GuestBinaryBuildResult(
                source_sha256="a" * 64,
                binaries=binaries,
            )
            environment = GuestRustBuildConstants.BINARIES_VARIABLE
            with (
                patch.dict(os.environ, {environment: str(prebuilt)}),
                patch.object(
                    build, "inspect_guest_binaries", return_value=result
                ) as inspect,
                patch.object(build, "build_guest_binaries") as rust_build,
            ):
                helpers = build._install_guest_files(
                    build_config.InitramfsBuildConfig(work=work),
                    root,
                    guests.guest_descriptor("alpine"),
                )

            inspect.assert_called_once_with(prebuilt)
            rust_build.assert_not_called()
            for name in GuestRustBuildConstants.BINARIES:
                with self.subTest(helper=name):
                    self.assertEqual(
                        (root / "sbin" / name).read_bytes(), name.encode("ascii")
                    )
                    self.assertEqual(helpers[name]["source_sha256"], "a" * 64)
                    self.assertEqual(helpers[name]["binary_sha256"], binaries[name])


class ScratchTemplateBuildTests(unittest.TestCase):
    def test_scratch_template_refuses_existing_output_without_replace(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "scratch.ext4"
            output.write_bytes(b"earlier template")
            with (
                patch.object(build.sys, "platform", "linux"),
                patch.object(build, "run_checked") as run_checked,
                self.assertRaisesRegex(common.ScriptError, "refusing to replace"),
            ):
                build.build_scratch_template(
                    build_config.ScratchTemplateBuildConfig(output=output)
                )
            run_checked.assert_not_called()
            self.assertEqual(output.read_bytes(), b"earlier template")

    def test_scratch_template_passes_every_parameter_to_mkfs(self):
        # Neither mke2fs.conf nor the caller's e2fsprogs variables shape the
        # template: mkfs.ext4 gets every parameter, a fixed clock, and the UUID
        # as its hash seed, and formats a temporary file of the template's size.
        uuid_text = "4f3c8bf7-1959-47b4-8811-62b17bbddaf4"
        features = (
            "none,has_journal,ext_attr,resize_inode,dir_index,filetype,extent,"
            "64bit,flex_bg,sparse_super,large_file,huge_file,dir_nlink,"
            "extra_isize,metadata_csum"
        )
        calls: list[tuple[list[str], dict[str, str], int]] = []

        def mkfs(command: Sequence[str], *, env: Mapping[str, str]) -> None:
            image = Path(command[-1])
            calls.append(
                ([str(part) for part in command], dict(env), image.stat().st_size)
            )
            with image.open("r+b") as template:
                template.write(b"formatted")

        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "build" / "scratch.ext4"
            stderr = io.StringIO()
            with (
                patch.object(build.sys, "platform", "linux"),
                patch.object(build, "_mkfs_ext4", return_value="mkfs.ext4"),
                patch.object(build, "run_checked", side_effect=mkfs),
                patch.dict(
                    build.os.environ,
                    {
                        "PATH": "/usr/sbin",
                        "E2FSPROGS_FAKE_TIME": "1700000000",
                        "MKE2FS_CONFIG": "/etc/other.conf",
                    },
                    clear=True,
                ),
                contextlib.redirect_stdout(io.StringIO()),
                contextlib.redirect_stderr(stderr),
            ):
                build.build_scratch_template(
                    build_config.ScratchTemplateBuildConfig(output=output)
                )
            temporary_output = output.resolve().with_name("scratch.ext4.part")
            self.assertEqual(
                calls,
                [
                    (
                        [
                            "mkfs.ext4",
                            "-q",
                            "-F",
                            "-b",
                            "4096",
                            "-I",
                            "256",
                            "-N",
                            "16384",
                            "-m",
                            "0",
                            "-O",
                            features,
                            "-U",
                            uuid_text,
                            "-E",
                            f"lazy_itable_init=0,lazy_journal_init=0,"
                            f"hash_seed={uuid_text}",
                            str(temporary_output),
                        ],
                        {"PATH": "/usr/sbin", "E2FSPROGS_FAKE_TIME": "1"},
                        64 * 1024 * 1024,
                    )
                ],
            )
            self.assertFalse(temporary_output.exists())
            self.assertTrue(output.read_bytes().startswith(b"formatted"))
            # Other bytes than those of the pinned template are only a warning.
            self.assertIn(
                f"not the {UbuntuBuildConstants.SCRATCH_TEMPLATE_SHA256} that "
                "UbuntuBuildConstants.SCRATCH_TEMPLATE_SHA256 pins",
                stderr.getvalue(),
            )

            stderr = io.StringIO()
            with (
                patch.object(build.sys, "platform", "linux"),
                patch.object(build, "_mkfs_ext4", return_value="mkfs.ext4"),
                patch.object(build, "run_checked", side_effect=mkfs),
                patch.object(
                    UbuntuBuildConstants,
                    "SCRATCH_TEMPLATE_SHA256",
                    common.sha256_file(output),
                ),
                contextlib.redirect_stdout(io.StringIO()),
                contextlib.redirect_stderr(stderr),
            ):
                build.build_scratch_template(
                    build_config.ScratchTemplateBuildConfig(output=output, replace=True)
                )
            self.assertEqual(stderr.getvalue(), "")

    def test_scratch_template_names_where_else_to_get_it_off_linux(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "scratch.ext4"
            with (
                patch.object(build.sys, "platform", "win32"),
                patch.object(build, "_mkfs_ext4") as mkfs,
                patch.object(build, "run_checked") as run_checked,
                self.assertRaises(common.ScriptError) as raised,
            ):
                build.build_scratch_template(
                    build_config.ScratchTemplateBuildConfig(output=output)
                )
            self.assertEqual(
                str(raised.exception),
                "build-scratch-template requires Linux; install the template with "
                "`nvx download`, or build it, with the Ubuntu distro layer, in "
                "Docker with `build-guest --guest all`",
            )
            mkfs.assert_not_called()
            run_checked.assert_not_called()
            self.assertEqual(list(Path(temporary).iterdir()), [])

    def test_scratch_template_leaves_no_output_when_mkfs_fails(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "scratch.ext4"
            with (
                patch.object(build.sys, "platform", "linux"),
                patch.object(build, "_mkfs_ext4", return_value="mkfs.ext4"),
                patch.object(
                    build,
                    "run_checked",
                    side_effect=common.ScriptError("mkfs.ext4 failed"),
                ),
                self.assertRaisesRegex(common.ScriptError, "mkfs.ext4 failed"),
            ):
                build.build_scratch_template(
                    build_config.ScratchTemplateBuildConfig(output=output)
                )
            self.assertEqual(list(Path(temporary).iterdir()), [])

    def test_mkfs_ext4_is_also_found_where_path_leaves_out_sbin(self):
        # Debian leaves /usr/sbin and /sbin, where e2fsprogs installs
        # mkfs.ext4, out of an unprivileged user's PATH.
        searches: list[str | None] = []

        def which(name: str, path: str | None = None) -> str | None:
            self.assertEqual(name, "mkfs.ext4")
            searches.append(path)
            if path is not None and "/usr/sbin" in path.split(os.pathsep):
                return "/usr/sbin/mkfs.ext4"
            return None

        with (
            patch.dict(
                build.os.environ,
                {"PATH": os.pathsep.join(("/usr/local/bin", "/usr/bin"))},
            ),
            patch.object(build.shutil, "which", side_effect=which),
        ):
            self.assertEqual(build._mkfs_ext4(), "/usr/sbin/mkfs.ext4")
        self.assertEqual(
            searches,
            [os.pathsep.join(("/usr/local/bin", "/usr/bin", "/usr/sbin", "/sbin"))],
        )
        with (
            patch.object(build.shutil, "which", return_value=None),
            self.assertRaisesRegex(
                common.ScriptError,
                "mkfs.ext4 was not found on PATH, in /usr/sbin, or in /sbin; "
                "install e2fsprogs",
            ),
        ):
            build._mkfs_ext4()

    def test_guest_determinism_builds_the_scratch_template_twice(self):
        # Both attempts format the template, which must be the pinned one.
        def initramfs(config: build_config.InitramfsBuildConfig) -> None:
            assert config.output is not None
            config.output.write_bytes(b"initramfs")
            config.output.with_name(f"{config.output.name}.packages.json").write_bytes(
                b"{}"
            )

        def distro(config: build_config.DistroLayerBuildConfig) -> None:
            config.output.write_bytes(b"erofs")
            config.output.with_name(f"{config.output.name}.manifest.json").write_bytes(
                b"{}"
            )

        def formatter(
            templates: tuple[bytes, ...], formatted: list[Path]
        ) -> Callable[[build_config.ScratchTemplateBuildConfig], None]:
            attempts = iter(templates)

            def template(config: build_config.ScratchTemplateBuildConfig) -> None:
                formatted.append(config.output)
                config.output.write_bytes(next(attempts))

            return template

        for templates, pinned, message in (
            ((b"template", b"template"), b"template", None),
            ((b"template", b"template"), b"other", "is not reproducible"),
            ((b"template", b"other"), b"template", "are not deterministic"),
        ):
            formatted: list[Path] = []
            with (
                self.subTest(message=message),
                tempfile.TemporaryDirectory() as temporary,
                patch.object(build.sys, "platform", "linux"),
                patch.object(build, "build_initramfs", side_effect=initramfs),
                patch.object(build, "build_distro_layer", side_effect=distro),
                patch.object(
                    build,
                    "build_scratch_template",
                    side_effect=formatter(templates, formatted),
                ),
                patch.object(ubuntu, "rootfs_inventory", return_value={}),
                patch.object(
                    UbuntuBuildConstants,
                    "SCRATCH_TEMPLATE_SHA256",
                    hashlib.sha256(pinned).hexdigest(),
                ),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                work = Path(temporary)
                if message is None:
                    build.verify_guest_determinism(work, "ubuntu")
                else:
                    with self.assertRaisesRegex(common.ScriptError, message):
                        build.verify_guest_determinism(work, "ubuntu")
                self.assertEqual(
                    formatted,
                    [
                        work / f"attempt-{attempt}" / "ubuntu-smoke-scratch.ext4"
                        for attempt in (1, 2)
                    ],
                )


if __name__ == "__main__":
    unittest.main()
