#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import contextlib
import http.client
import io
import json
import os
import re
import shutil
import sys
import tarfile
import tempfile
import unittest
import urllib.request
from pathlib import Path
from typing import cast
from unittest.mock import call, patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import archive, build, common, nvx_cli, release, ubuntu  # noqa: E402
from nvx_tools.build_constants import (  # noqa: E402
    AlpineBuildConstants,
    AzureLinuxBuildConstants,
    BuildConstants,
    DockerBuildConstants,
    GuestRustBuildConstants,
    InitramfsBuildConstants,
    KernelBuildConstants,
    OpenVMMBuildConstants,
    ReleaseBuildConstants,
    UbuntuBuildConstants,
)
from nvx_tools_testing import elf_file as _elf_file  # noqa: E402
from nvx_tools_testing import (
    without_nvx_variable as _without_nvx_variable,  # noqa: E402
)
from nvx_tools_testing import (
    write_release_fixture as _write_release_fixture,
)

AUTHORIZATION_VALUE = "Bearer placeholder-value"


class ReleaseTests(unittest.TestCase):
    def setUp(self) -> None:
        super().setUp()
        # Packaging reads nvx; no test may read or replace the nvx of
        # this checkout.
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.nvx_path = Path(temporary.name) / nvx_cli.binary_path().name
        patcher = patch.object(nvx_cli, "binary_path", return_value=self.nvx_path)
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_nvx_installs_the_scratch_template_that_packages_declare(self):
        # nvx download installs the sandbox scratch template of a package
        # whose manifest gives its digest under the field and the name that
        # package writes.
        crate = BuildConstants.REPO_ROOT / "nvx" / "src" / "host"
        install = (crate / "cli" / "download" / "install.rs").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            'const SCRATCH_TEMPLATE_FIELD: &str = "scratch_template_sha256";',
            install,
        )
        guest = (crate / "artifacts" / "guest.rs").read_text(encoding="utf-8")
        self.assertIn(
            f'Self::Ubuntu => Some("{UbuntuBuildConstants.SCRATCH_TEMPLATE_NAME}"),',
            guest,
        )
        self.assertIn(
            UbuntuBuildConstants.SCRATCH_TEMPLATE_NAME,
            ReleaseBuildConstants.GUEST_ARTIFACT_NAMES,
        )

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

    def test_alpine_source_validation_checks_recorded_executables(self):
        with tempfile.TemporaryDirectory() as temporary:
            source_dir = Path(temporary)
            alpine_dir = source_dir / "alpine"
            recipe = alpine_dir / "recipes" / "c0ffee" / "main" / "foo"
            recipe.mkdir(parents=True)
            (recipe / "APKBUILD").write_bytes(b"pkgname=foo\n")
            (recipe / "foo.post-install").write_bytes(b"#!/bin/sh\n")
            installer = "recipes/c0ffee/main/foo/foo.post-install"
            current = AlpineBuildConstants.SOURCE_MANIFEST_FORMAT
            cases: dict[str, tuple[dict[str, object], str | None]] = {
                "recorded executable": (
                    {"format": current, "executables": [installer]},
                    None,
                ),
                "older format": (
                    {"format": 1, "executables": [installer]},
                    "unsupported manifest format",
                ),
                "missing executables": ({"format": current}, "invalid executables"),
                "unlisted executable": (
                    {"format": current, "executables": [f"{installer}.missing"]},
                    "not a checksummed recipe file",
                ),
                "executable outside recipes": (
                    {"format": current, "executables": ["manifest.json"]},
                    "not a checksummed recipe file",
                ),
            }
            for name, (document, error) in cases.items():
                (alpine_dir / "manifest.json").write_text(
                    json.dumps({"packages": [], **document}), encoding="utf-8"
                )
                common.write_sha256_sums(alpine_dir)
                with (
                    self.subTest(case=name),
                    patch.object(BuildConstants, "SOURCE_DIR", source_dir),
                ):
                    if error is None:
                        self.assertEqual(
                            release._validate_alpine_sources([]), {installer}
                        )
                        continue
                    with self.assertRaisesRegex(common.ScriptError, error):
                        release._validate_alpine_sources([])

    def test_alpine_source_archive_applies_recorded_modes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source_dir = root / "sources"
            recipe = source_dir / "alpine" / "recipes" / "c0ffee" / "main" / "foo"
            recipe.mkdir(parents=True)
            for name in ("APKBUILD", "foo.post-install"):
                (recipe / name).write_bytes(b"#!/bin/sh\n")
            package_manifest = root / "initramfs.cpio.gz.packages.json"
            package_manifest.write_text("{}", encoding="utf-8")
            # Report every mode as Windows does, whatever the host stores.
            for path in (*source_dir.rglob("*"), package_manifest):
                path.chmod(0o777 if path.is_dir() else 0o666)
            output = root / "alpine-source.tar.gz"

            with (
                patch.object(BuildConstants, "SOURCE_DIR", source_dir),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                release._alpine_source_archive(
                    output,
                    "1.0.0",
                    [package_manifest],
                    frozenset({"recipes/c0ffee/main/foo/foo.post-install"}),
                )

            with tarfile.open(output, "r:gz") as package:
                modes = {member.name: member.mode for member in package.getmembers()}
            prefix = "nvx-alpine-source-1.0.0"
            self.assertEqual(
                modes,
                {
                    f"{prefix}/sources": 0o755,
                    f"{prefix}/sources/recipes": 0o755,
                    f"{prefix}/sources/recipes/c0ffee": 0o755,
                    f"{prefix}/sources/recipes/c0ffee/main": 0o755,
                    f"{prefix}/sources/recipes/c0ffee/main/foo": 0o755,
                    f"{prefix}/sources/recipes/c0ffee/main/foo/APKBUILD": 0o644,
                    f"{prefix}/sources/recipes/c0ffee/main/foo/foo.post-install": (
                        0o755
                    ),
                    f"{prefix}/manifests/initramfs.cpio.gz.packages.json": 0o644,
                },
            )

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
                patch.object(nvx_cli, "binary_path", return_value=paths["nvx"]),
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
            # The release build of nvx ships beside OpenVMM, under the name
            # that Cargo gives it on this OS.
            packaged_nvx = destination / "bin" / nvx_cli.binary_path().name
            self.assertEqual(packaged_nvx.read_bytes(), paths["nvx"].read_bytes())
            self.assertEqual(
                sorted(path.name for path in (destination / "bin").iterdir()),
                sorted((binary.name, packaged_nvx.name)),
            )
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
                1,
            )
            self.assertEqual(
                manifest["openvmm"]["control_session_protocol_version"],
                1,
            )
            self.assertEqual(
                manifest["openvmm"]["control_contract_revision"],
                "nvx-microvm-v2-control-v2",
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
            # The package ships the sandbox scratch template, which the
            # manifest declares with its digest.
            self.assertEqual(
                manifest["ubuntu"]["scratch_template_sha256"],
                common.sha256_file(
                    destination / "guest" / UbuntuBuildConstants.SCRATCH_TEMPLATE_NAME
                ),
            )
            # The package ships the provenance of nvx, which binds it to the
            # sources that build it, the Rust release, the target, and the
            # features, and the manifest records them with its digest.
            packaged_provenance = json.loads(
                (destination / "provenance" / nvx_cli.PROVENANCE_NAME).read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(
                packaged_provenance,
                {
                    "format": 1,
                    "source_sha256": nvx_cli.source_sha256(root),
                    "toolchain": GuestRustBuildConstants.TOOLCHAIN,
                    "target": nvx_cli.release_target(),
                    "features": ["cli"],
                    "executable_sha256": common.sha256_file(packaged_nvx),
                },
            )
            self.assertEqual(
                packaged_provenance["source_sha256"], nvx_cli.source_sha256()
            )
            self.assertEqual(
                manifest["nvx"],
                {
                    "executable_sha256": common.sha256_file(packaged_nvx),
                    "features": ["cli"],
                    "source_sha256": nvx_cli.source_sha256(root),
                    "target": nvx_cli.release_target(),
                    "toolchain": GuestRustBuildConstants.TOOLCHAIN,
                },
            )
            inventory = dict(common.verify_sha256_sums(destination).files)
            self.assertEqual(
                inventory[f"bin/{packaged_nvx.name}"],
                common.sha256_file(paths["nvx"]),
            )
            self.assertIn(f"provenance/{nvx_cli.PROVENANCE_NAME}", inventory)
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
            patch.object(nvx_cli, "binary_path", return_value=paths["nvx"]),
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
        # Source-inclusive packages ship nvx too; the project source
        # archive holds the sources that build it.
        packaged_nvx = destination / "bin" / paths["nvx"].name
        self.assertEqual(packaged_nvx.read_bytes(), paths["nvx"].read_bytes())
        self.assertEqual(
            manifest["nvx"]["executable_sha256"],
            common.sha256_file(packaged_nvx),
        )
        common.verify_sha256_sums(destination)
        self.assertIn("omits the Azure Linux guest", stderr.getvalue())

    def test_packages_require_the_release_build_of_nvx(self):
        # A package ships the nvx build for the release target of its
        # operating system, with the provenance that binds it to the current
        # sources. On Linux, that build is the static musl executable that
        # runs on every host, which a build for the host's own target is not.
        linux = nvx_cli.RELEASE_TARGETS["linux"]
        windows = nvx_cli.RELEASE_TARGETS["windows"]
        dynamic = _elf_file(
            ((1, b"code"), (3, b"/lib64/ld-linux-x86-64.so.2\0")), elf_type=3
        )
        mismatch = "from the current nvx sources"
        with tempfile.TemporaryDirectory() as temporary:
            build_dir = Path(temporary) / "build"
            build_dir.mkdir()
            executable = Path(temporary) / "nvx"
            provenance = build_dir / nvx_cli.PROVENANCE_NAME
            cases: tuple[
                tuple[str, bytes | None, dict[str, object] | None, str | None], ...
            ] = (
                (linux, _elf_file(), {}, None),
                (linux, _elf_file(elf_type=3), {}, None),
                (windows, b"MZ", {}, None),
                (linux, dynamic, {}, "is not static"),
                (linux, b"MZ", {}, f"invalid {linux} nvx executable"),
                (linux, _elf_file(), {"target": windows}, f"as a {linux} build"),
                (windows, b"MZ", {"target": linux}, f"as a {windows} build"),
                (linux, _elf_file(), {"source_sha256": "0" * 64}, mismatch),
                (linux, _elf_file(), {"executable_sha256": "0" * 64}, mismatch),
                (linux, _elf_file(), {"toolchain": "1.0.0"}, mismatch),
                (linux, _elf_file(), {"features": []}, mismatch),
                (linux, _elf_file(), {"format": 2}, mismatch),
                (linux, _elf_file(), None, "nvx build provenance not found"),
                (linux, None, {}, "nvx release executable not found"),
            )
            for target, contents, changes, error in cases:
                with self.subTest(target=target, changes=changes, error=error):
                    executable.unlink(missing_ok=True)
                    provenance.unlink(missing_ok=True)
                    if contents is not None:
                        executable.write_bytes(contents)
                    if contents is not None and changes is not None:
                        record = nvx_cli.provenance_record(executable, target)
                        record.update(changes)
                        provenance.write_text(json.dumps(record), encoding="utf-8")
                    with (
                        patch.object(nvx_cli, "binary_path", return_value=executable),
                        patch.object(nvx_cli, "release_target", return_value=target),
                        patch.object(
                            release, "artifact_path", side_effect=build_dir.joinpath
                        ),
                    ):
                        if error is None:
                            self.assertEqual(
                                release._require_release_nvx(),
                                (
                                    executable,
                                    provenance,
                                    json.loads(provenance.read_text(encoding="utf-8")),
                                ),
                            )
                            continue
                        with self.assertRaisesRegex(
                            common.ScriptError, re.escape(error)
                        ) as raised:
                            release._require_release_nvx()
                    if error == "is not static":
                        # The message names the build that packages need.
                        self.assertIn(
                            f"--features cli --manifest-path nvx/Cargo.toml "
                            f"--target {linux}",
                            str(raised.exception),
                        )
                    if error == mismatch:
                        self.assertIn(
                            "nvx.py record-nvx-provenance", str(raised.exception)
                        )

    def test_runtime_provenance_validation_uses_required_paths(self):
        build_dir = Path("build")
        binary = Path("openvmm")
        nvx_binary = Path("nvx")

        def artifact_path(name: str) -> Path:
            return build_dir / name

        def require_file(path: Path, _description: str) -> Path:
            return path

        with (
            patch.object(release, "artifact_path", side_effect=artifact_path),
            patch.object(release, "openvmm_binary_path", return_value=binary),
            patch.object(nvx_cli, "find_binary", return_value=nvx_binary),
            patch.object(
                release,
                "require_file",
                side_effect=require_file,
            ) as require,
            patch.object(release, "_validate_openvmm_provenance") as openvmm,
            patch.object(release, "_validate_kernel_provenance"),
            patch.object(release, "_validate_initramfs_provenance"),
            patch.object(release, "_validate_nvx_provenance") as nvx_check,
        ):
            release.validate_runtime_artifact_provenance()

        self.assertEqual(
            require.call_args_list[-4:],
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
                call(build_dir / nvx_cli.PROVENANCE_NAME, "nvx build provenance"),
            ],
        )
        openvmm.assert_called_once_with(
            binary, build_dir / OpenVMMBuildConstants.PROVENANCE_NAME
        )
        # The nvx that nvx.py runs, of any target.
        nvx_check.assert_called_once_with(
            nvx_binary, build_dir / nvx_cli.PROVENANCE_NAME, target=None
        )

    def test_runtime_provenance_validation_rejects_a_stale_nvx(self):
        # nvx.py never rebuilds an nvx that exists, so one left from
        # before a change of its sources, or that its provenance does not
        # describe, would run. The adversarial executor validates it first.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            build_dir = root / "build"
            build_dir.mkdir()
            provenance = build_dir / nvx_cli.PROVENANCE_NAME
            other = root / "other-nvx"
            other.write_bytes(b"another nvx")

            def require_file(path: Path, _description: str) -> Path:
                return path

            def validate() -> None:
                with (
                    patch.object(
                        release, "artifact_path", side_effect=build_dir.joinpath
                    ),
                    patch.object(release, "openvmm_binary_path"),
                    patch.object(release, "require_file", side_effect=require_file),
                    patch.object(release, "_validate_openvmm_provenance"),
                    patch.object(release, "_validate_kernel_provenance"),
                    patch.object(release, "_validate_initramfs_provenance"),
                ):
                    release.validate_runtime_artifact_provenance()

            self.nvx_path.write_bytes(b"nvx")
            provenance.write_text(
                json.dumps(
                    nvx_cli.provenance_record(self.nvx_path, "x86_64-unknown-linux-gnu")
                ),
                encoding="utf-8",
            )
            with _without_nvx_variable():
                validate()
                # A rebuild after the record.
                self.nvx_path.write_bytes(b"rebuilt nvx")
                with self.assertRaisesRegex(
                    common.ScriptError, "does not describe .* as a build from the"
                ):
                    validate()
            # The nvx that NVX names is the one that nvx.py runs.
            with (
                patch.dict(os.environ, {nvx_cli.VARIABLE: str(other)}),
                self.assertRaisesRegex(common.ScriptError, re.escape(str(other))),
            ):
                validate()
            provenance.write_text(
                json.dumps(
                    nvx_cli.provenance_record(other, "x86_64-unknown-linux-gnu")
                ),
                encoding="utf-8",
            )
            with patch.dict(os.environ, {nvx_cli.VARIABLE: str(other)}):
                validate()
            # A record of other sources.
            record = nvx_cli.provenance_record(other, "x86_64-unknown-linux-gnu")
            record["source_sha256"] = "0" * 64
            provenance.write_text(json.dumps(record), encoding="utf-8")
            with (
                patch.dict(os.environ, {nvx_cli.VARIABLE: str(other)}),
                self.assertRaisesRegex(common.ScriptError, "current nvx sources"),
            ):
                validate()

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

    def test_guest_artifact_validation_checks_the_files_of_each_component(self):
        # The components hold every guest image file once.
        files = [
            name
            for names in release.GUEST_ARTIFACT_COMPONENTS.values()
            for name in names
        ]
        self.assertEqual(len(files), len(set(files)))
        self.assertEqual(set(files), set(DockerBuildConstants.ALL_GUEST_ARTIFACT_NAMES))
        components = list(release.GUEST_ARTIFACT_COMPONENTS)
        with tempfile.TemporaryDirectory() as temporary:
            paths, kernel_inputs, _revision = _write_release_fixture(Path(temporary))
            images = paths["build"]
            with patch.object(
                release, "kernel_provenance_inputs", return_value=kernel_inputs
            ):
                release.validate_guest_artifacts(images, components)
                for component, name, message in (
                    ("kernel", "vmlinux", "kernel build provenance does not match"),
                    (
                        "kernel",
                        "vmlinux.config",
                        "kernel build provenance does not match",
                    ),
                    (
                        "alpine",
                        "initramfs.cpio.gz",
                        "initramfs build provenance does not match",
                    ),
                    (
                        "alpine",
                        "initramfs.cpio.gz.packages.json",
                        "initramfs build provenance does not match",
                    ),
                    (
                        "ubuntu",
                        "initramfs-ubuntu.cpio.gz",
                        "Ubuntu artifact manifest does not match "
                        "initramfs-ubuntu.cpio.gz: initramfs-ubuntu.cpio.gz."
                        "packages.json differs in artifact_sha256",
                    ),
                    (
                        "ubuntu",
                        "ubuntu-distro.erofs",
                        "Ubuntu artifact manifest does not match ubuntu-distro."
                        "erofs: ubuntu-distro.erofs.manifest.json differs in "
                        "artifact_sha256",
                    ),
                    (
                        "azurelinux",
                        "initramfs-azurelinux.cpio.gz",
                        "Azure Linux initramfs manifest is invalid",
                    ),
                ):
                    with self.subTest(file=name):
                        original = (images / name).read_bytes()
                        (images / name).write_bytes(b"stale " + original)
                        try:
                            with self.assertRaisesRegex(
                                common.ScriptError, re.escape(message)
                            ):
                                release.validate_guest_artifacts(images, [component])
                            # Only its own component checks a file.
                            release.validate_guest_artifacts(
                                images,
                                [other for other in components if other != component],
                            )
                        finally:
                            (images / name).write_bytes(original)
                # A component needs only its own files.
                for component, names in release.GUEST_ARTIFACT_COMPONENTS.items():
                    with (
                        self.subTest(component=component),
                        tempfile.TemporaryDirectory() as alone,
                    ):
                        for name in names:
                            shutil.copyfile(images / name, Path(alone) / name)
                        release.validate_guest_artifacts(Path(alone), [component])
                for selection, message in (
                    ([], "no guest artifact component to validate"),
                    (
                        ["kernel", "debug-kernel"],
                        "unknown guest artifact component: debug-kernel",
                    ),
                ):
                    with (
                        self.subTest(components=selection),
                        self.assertRaisesRegex(common.ScriptError, message),
                    ):
                        release.validate_guest_artifacts(images, selection)
                (images / KernelBuildConstants.PROVENANCE_NAME).unlink()
                with self.assertRaisesRegex(
                    common.ScriptError,
                    "guest artifact vmlinux.provenance.json not found",
                ):
                    release.validate_guest_artifacts(images, ["kernel"])

    def test_guest_release_inputs_reject_stale_ubuntu_artifacts(self):
        for artifact_name, manifest_name in (
            ("initramfs-ubuntu.cpio.gz", "initramfs-ubuntu.cpio.gz.packages.json"),
            ("ubuntu-distro.erofs", "ubuntu-distro.erofs.manifest.json"),
        ):
            with (
                self.subTest(artifact=artifact_name),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                paths, _kernel_inputs, _revision = _write_release_fixture(root)
                build_dir = paths["build"]
                recorded = json.loads(
                    (build_dir / manifest_name).read_text(encoding="utf-8")
                )["artifact_sha256"]
                (build_dir / artifact_name).write_bytes(b"stale artifact")
                current = common.sha256_file(build_dir / artifact_name)
                with (
                    patch.object(
                        release,
                        "artifact_path",
                        side_effect=build_dir.joinpath,
                    ),
                    self.assertRaises(common.ScriptError) as raised,
                ):
                    release._guest_release_inputs(include_azurelinux=True)
                self.assertEqual(
                    str(raised.exception),
                    f"Ubuntu artifact manifest does not match {artifact_name}: "
                    f'{manifest_name} differs in artifact_sha256 (manifest "{recorded}", '
                    f'expected "{current}")',
                )

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
            manifest_name = "initramfs-ubuntu.cpio.gz.packages.json"
            recorded = json.loads(
                (build_dir / manifest_name).read_text(encoding="utf-8")
            )["input_sha256"]
            guest_file = next(
                path
                for path in sorted(
                    (root / UbuntuBuildConstants.GUEST_DIRECTORY).iterdir()
                )
                if path.is_file()
            )
            guest_file.write_bytes(guest_file.read_bytes() + b"\n# changed\n")
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
                current = ubuntu.converter_input_sha256(ubuntu.customization_files())
                with self.assertRaises(common.ScriptError) as raised:
                    release._guest_release_inputs(include_azurelinux=True)

        self.assertNotEqual(current, recorded)
        self.assertEqual(
            str(raised.exception),
            "Ubuntu artifact manifest does not match initramfs-ubuntu.cpio.gz: "
            f'{manifest_name} differs in input_sha256 (manifest "{recorded}", '
            f'expected "{current}")',
        )

    def test_guest_release_inputs_name_every_mismatched_ubuntu_manifest_field(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths, _kernel_inputs, _revision = _write_release_fixture(root)
            build_dir = paths["build"]
            manifest_path = build_dir / "ubuntu-distro.erofs.manifest.json"
            valid = manifest_path.read_text(encoding="utf-8")
            with patch.object(
                release,
                "artifact_path",
                side_effect=build_dir.joinpath,
            ):
                release._guest_release_inputs(include_azurelinux=True)
                for case, updates, removed, details in (
                    (
                        "two wrong fields",
                        {"format": 2, "guest": "alpine"},
                        (),
                        "format (manifest 2, expected 1), "
                        'guest (manifest "alpine", expected "ubuntu")',
                    ),
                    (
                        "missing field",
                        {},
                        ("artifact",),
                        'artifact (manifest missing, expected "ubuntu-distro.erofs")',
                    ),
                ):
                    with self.subTest(case=case):
                        document = json.loads(valid)
                        document.update(updates)
                        for field in removed:
                            del document[field]
                        manifest_path.write_text(json.dumps(document), encoding="utf-8")
                        with self.assertRaises(common.ScriptError) as raised:
                            release._guest_release_inputs(include_azurelinux=True)
                        self.assertEqual(
                            str(raised.exception),
                            "Ubuntu artifact manifest does not match "
                            "ubuntu-distro.erofs: ubuntu-distro.erofs.manifest.json "
                            f"differs in {details}",
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
            (
                "alpine",
                "guest_sources",
                ["guest/common", "guest/alpine"],
                "Alpine guest_sources",
            ),
            (
                "guest_rust",
                "toolchain",
                "stable",
                "Rust guest toolchain",
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
                    patch.object(nvx_cli, "binary_path", return_value=paths["nvx"]),
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
        for corrupted, error in (
            ("binary", "packaged OpenVMM executable"),
            ("nvx", "packaged nvx executable does not match its build"),
        ):
            with self.subTest(corrupted=corrupted):
                self._check_failed_staged_verification(corrupted, error)

    def _check_failed_staged_verification(self, corrupted: str, error: str) -> None:
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
                if target.name == paths[corrupted].name:
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
                patch.object(nvx_cli, "binary_path", return_value=paths["nvx"]),
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
                self.assertRaisesRegex(common.ScriptError, error),
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

    def test_arm_source_metadata_binds_arm_inputs_without_replacing_x86_metadata(self):
        manifest = json.loads(
            (BuildConstants.REPO_ROOT / "SOURCE-MANIFEST.json").read_bytes()
        )
        inputs = build.kernel_provenance_inputs("aarch64")
        paths = release._validate_source_manifest_metadata(
            manifest, inputs, architecture="aarch64"
        )
        self.assertTrue(any("/arm64/" in path for path in paths))
        manifest["linux"]["architectures"]["aarch64"]["input_config"] = (
            "kernel/config-microvm"
        )
        with self.assertRaisesRegex(common.ScriptError, "ARM Linux inputs"):
            release._validate_source_manifest_metadata(
                manifest, inputs, architecture="aarch64"
            )

    def test_arm_guest_admission_refuses_azurelinux_before_reading_any_file(self):
        with patch.object(release, "require_file") as require_file:
            with self.assertRaisesRegex(
                common.ScriptError, "unknown guest artifact component: azurelinux"
            ):
                release.validate_guest_artifacts(
                    Path("owned-empty"), ("azurelinux",), architecture="aarch64"
                )
        require_file.assert_not_called()

    def test_project_source_archive_keeps_selected_config_and_manifest_namespace(self):
        for architecture in ("x86_64", "aarch64"):
            with (
                self.subTest(architecture=architecture),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                build_dir = root / "build"
                (build_dir / "aarch64").mkdir(parents=True)
                (root / "SOURCE-MANIFEST.json").write_bytes(b"source metadata\n")
                (build_dir / "vmlinux.config").write_bytes(b"legacy x86 config\n")
                (build_dir / "aarch64" / "Image.config").write_bytes(
                    b"selected ARM config\n"
                )
                selected = (
                    build_dir / "aarch64" if architecture == "aarch64" else build_dir
                )
                manifest = selected / "initramfs.cpio.gz.packages.json"
                manifest.write_bytes(b"selected guest manifest\n")
                output = root / "project.tar.gz"

                def artifact_path(
                    name: str,
                    *,
                    architecture: str = "x86_64",
                    directory: Path = build_dir,
                ) -> Path:
                    return (
                        directory / architecture / name
                        if architecture == "aarch64"
                        else directory / name
                    )

                with (
                    patch.object(BuildConstants, "REPO_ROOT", root),
                    patch.object(
                        ReleaseBuildConstants,
                        "PROJECT_SOURCE_PATHS",
                        ("SOURCE-MANIFEST.json",),
                    ),
                    patch.object(release, "artifact_path", side_effect=artifact_path),
                ):
                    release._project_source_archive(
                        output,
                        "fixture",
                        [manifest],
                        **common.architecture_kwargs(architecture),
                    )
                config_name = (
                    "Image.config" if architecture == "aarch64" else "vmlinux.config"
                )
                prefix = (
                    "nvx-project-source-fixture/"
                    + selected.relative_to(root).as_posix()
                )
                with tarfile.open(output, "r:gz") as source:
                    self.assertEqual(
                        set(source.getnames()),
                        {
                            "nvx-project-source-fixture/SOURCE-MANIFEST.json",
                            f"{prefix}/{config_name}",
                            f"{prefix}/{manifest.name}",
                        },
                    )
                    stream = source.extractfile(f"{prefix}/{config_name}")
                    assert stream is not None
                    self.assertEqual(
                        stream.read(), (selected / config_name).read_bytes()
                    )

    def test_source_package_validates_only_its_selected_linux_archive(self):
        class SourceValidated(Exception):
            pass

        for architecture in ("x86_64", "aarch64"):
            with (
                self.subTest(architecture=architecture),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                build_dir = root / "build"
                source_dir = build_dir / "sources"
                (build_dir / "aarch64").mkdir(parents=True)
                (build_dir / "vmlinux.config").write_bytes(b"x86 final configuration\n")
                (build_dir / "aarch64" / "Image.config").write_bytes(
                    b"ARM final configuration\n"
                )
                metadata: dict[str, object] = {
                    "linux": {
                        "patches": [],
                        "architectures": {"aarch64": {"patches": []}},
                    }
                }
                (root / "SOURCE-MANIFEST.json").write_text(json.dumps(metadata))
                for relative in (
                    KernelBuildConstants.ARM_INPUT_CONFIG,
                    KernelBuildConstants.ARM_DERIVED_CONFIG,
                ):
                    path = root / relative
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_bytes(relative.as_posix().encode())
                archives: dict[str, Path] = {}
                for isa in ("x86_64", "aarch64"):
                    arm = isa == "aarch64"
                    config = (
                        build_dir / "aarch64" / "Image.config"
                        if arm
                        else build_dir / "vmlinux.config"
                    )
                    inputs = [
                        (config, f"source/{config.name}"),
                        (root / "SOURCE-MANIFEST.json", "source/SOURCE-MANIFEST.json"),
                    ]
                    if arm:
                        inputs.extend(
                            (root / relative, f"source/{relative.as_posix()}")
                            for relative in (
                                KernelBuildConstants.ARM_INPUT_CONFIG,
                                KernelBuildConstants.ARM_DERIVED_CONFIG,
                            )
                        )
                    driver = "hvc_nvx_mmio.c" if arm else "hvc_xe9.c"
                    driver_path = root / isa / driver
                    driver_path.parent.mkdir()
                    driver_path.write_bytes(b"owned source fixture\n")
                    inputs.append(
                        (
                            driver_path,
                            f"source/{KernelBuildConstants.SOURCE_NAME}/drivers/tty/hvc/{driver}",
                        )
                    )
                    directory = source_dir / KernelBuildConstants.SOURCE_DIRECTORY_NAME
                    name = KernelBuildConstants.SOURCE_ARCHIVE_NAME
                    if arm:
                        directory /= isa
                        name = (
                            f"{KernelBuildConstants.SOURCE_ARCHIVE_ROOT}-{isa}.tar.gz"
                        )
                    directory.mkdir(parents=True, exist_ok=True)
                    archives[isa] = directory / name
                    archive.create_reproducible_tar_gz(archives[isa], inputs)
                binary = root / "openvmm.fixture"
                binary.write_bytes(
                    b"packaging reaches nvx admission only after source validation"
                )

                def artifact_path(
                    name: str,
                    *,
                    architecture: str = "x86_64",
                    directory: Path = build_dir,
                ) -> Path:
                    return (
                        directory / architecture / name
                        if architecture == "aarch64"
                        else directory / name
                    )

                with (
                    patch.object(BuildConstants, "REPO_ROOT", root),
                    patch.object(BuildConstants, "SOURCE_DIR", source_dir),
                    patch.object(release, "artifact_path", side_effect=artifact_path),
                    patch.object(
                        release, "_guest_release_inputs", return_value=([], [], [], [])
                    ),
                    patch.object(
                        release, "_validate_alpine_sources", return_value=frozenset()
                    ),
                    patch.object(release, "_validate_ubuntu_sources"),
                    patch.object(release, "openvmm_binary_path", return_value=binary),
                    patch.object(
                        release, "_require_release_nvx", side_effect=SourceValidated
                    ),
                    patch.object(
                        release,
                        "_validate_linux_source_archive",
                        wraps=release._validate_linux_source_archive,
                    ) as validate,
                ):
                    with self.assertRaises(SourceValidated):
                        release.package_release(
                            version="fixture",
                            destination=root / "package",
                            include_source=True,
                            force=False,
                            **common.architecture_kwargs(architecture),
                        )
                    validate.assert_called_once_with(
                        archives[architecture],
                        **common.architecture_kwargs(architecture),
                    )
                    archives[architecture].write_bytes(
                        archives[
                            "aarch64" if architecture == "x86_64" else "x86_64"
                        ].read_bytes()
                    )
                    with self.assertRaisesRegex(common.ScriptError, "HVC driver"):
                        release.package_release(
                            version="fixture",
                            destination=root / "package",
                            include_source=True,
                            force=False,
                            **common.architecture_kwargs(architecture),
                        )


if __name__ == "__main__":
    unittest.main()
