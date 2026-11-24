#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import hashlib
import json
import lzma
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import (  # noqa: E402
    build_config,
    collect_ubuntu_sources,
    common,
    release,
)
from nvx_tools.build_constants import UbuntuBuildConstants  # noqa: E402


class ReleaseSourceCollectionTests(unittest.TestCase):
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


class UbuntuSourceValidationTests(unittest.TestCase):
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

    @unittest.skipUnless(
        sys.platform == "win32", "junctions are Windows reparse points"
    )
    def test_ubuntu_source_output_refuses_a_junction(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "target"
            (target / "packages").mkdir(parents=True)
            (target / "packages" / "keep.txt").write_text("keep", encoding="utf-8")
            junction = root / "output"
            subprocess.run(
                ["cmd", "/c", "mklink", "/J", str(junction), str(target)],
                check=True,
                capture_output=True,
            )

            with self.assertRaisesRegex(common.ScriptError, "not a directory"):
                collect_ubuntu_sources._validate_source_output(junction)
            with self.assertRaisesRegex(common.ScriptError, "not a directory"):
                collect_ubuntu_sources._reset_source_output(junction)
            self.assertEqual(
                (target / "packages" / "keep.txt").read_text(encoding="utf-8"), "keep"
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


if __name__ == "__main__":
    unittest.main()
