#!/usr/bin/env python3

import hashlib
import io
import os
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import archive, build, common, release  # noqa: E402


class CliTests(unittest.TestCase):
    def test_release_commands_keep_their_cli_contract(self):
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


class ReleaseTests(unittest.TestCase):
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
                "LICENSE-NVX",
                "README.md",
                "SOURCE-MANIFEST.json",
                "THIRD_PARTY_NOTICES.md",
            )
            for path in (
                binary,
                *(build_dir / name for name in guest_names),
                *(root / name for name in release_files),
                openvmm_dir / "LICENSE",
                root / "licenses" / "COPYING-LINUX",
            ):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(path.name.encode("ascii"))
            destination = root / "staged"
            stderr = io.StringIO()

            with (
                patch.object(release, "REPO_ROOT", root),
                patch.object(release, "SOURCE_DIR", source_dir),
                patch.object(release, "OPENVMM_DIR", openvmm_dir),
                patch.object(
                    release,
                    "artifact_path",
                    side_effect=lambda name: build_dir / name,
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
