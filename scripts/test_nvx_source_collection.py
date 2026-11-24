#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import contextlib
import hashlib
import io
import json
import lzma
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import (  # noqa: E402
    build_config,
    collect_alpine_sources,
    collect_ubuntu_sources,
    common,
    release,
)
from nvx_tools.build_constants import (  # noqa: E402
    AlpineBuildConstants,
    BuildConstants,
    UbuntuBuildConstants,
)


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


class UbuntuSourceCollectionTests(unittest.TestCase):
    def test_malformed_package_manifest_is_reported_as_script_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            manifest = Path(temporary) / "packages.json"
            manifest.write_text("{", encoding="utf-8")

            with self.assertRaises(collect_ubuntu_sources.ScriptError) as context:
                collect_ubuntu_sources._source_requirements([manifest])

        self.assertIn("cannot read Ubuntu package manifest", str(context.exception))


class AlpineSourceCollectionTests(unittest.TestCase):
    INSTALL_SCRIPT = b"#!/bin/sh\nexit 0\n"

    @staticmethod
    def _aports_commit(cache: Path, files: dict[str, tuple[str, bytes]]) -> str:
        environment = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("GIT_CONFIG_")
        }
        environment.update(
            GIT_AUTHOR_NAME="NVX Tests",
            GIT_AUTHOR_EMAIL="nvx-tests@example.com",
            GIT_COMMITTER_NAME="NVX Tests",
            GIT_COMMITTER_EMAIL="nvx-tests@example.com",
        )

        def git(*args: str, data: bytes | None = None) -> str:
            result = subprocess.run(
                ["git", "-C", str(cache), *args],
                input=data,
                env=environment,
                capture_output=True,
                check=True,
            )
            return result.stdout.decode("ascii").strip()

        cache.mkdir(parents=True, exist_ok=True)
        git("init", "-q")
        git("config", "core.autocrlf", "false")
        for path, (mode, contents) in files.items():
            blob = git("hash-object", "-w", "--stdin", data=contents)
            git("update-index", "--add", "--cacheinfo", f"{mode},{blob},{path}")
        return git("commit-tree", git("write-tree"), "-m", "aports fixture")

    def _symlinked_recipe_commit(self, cache: Path) -> str:
        return self._aports_commit(
            cache,
            {
                "main/foo/APKBUILD": ("100644", b"pkgname=foo\n"),
                "main/foo/foo.post-install": ("100755", self.INSTALL_SCRIPT),
                "main/foo/foo.post-upgrade": ("120000", b"foo.post-install"),
            },
        )

    @staticmethod
    def _package_manifest(root: Path, commit: str) -> Path:
        manifest = root / "packages.json"
        manifest.write_text(
            json.dumps(
                {
                    "guest": AlpineBuildConstants.GUEST_NAME,
                    "alpine_branch": AlpineBuildConstants.BRANCH,
                    "architecture": AlpineBuildConstants.ARCHITECTURE,
                    "packages": [
                        {
                            "name": "foo",
                            "version": "1.0-r0",
                            "origin": "foo",
                            "license": "MIT",
                            "aports_commit": commit,
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )
        return manifest

    @staticmethod
    def _member(
        name: str,
        kind: bytes = tarfile.REGTYPE,
        data: bytes = b"",
        *,
        linkname: str = "",
        mode: int = 0o664,
    ) -> tuple[tarfile.TarInfo, bytes]:
        member = tarfile.TarInfo(name)
        member.type = kind
        member.linkname = linkname
        member.mode = mode
        member.size = len(data) if kind == tarfile.REGTYPE else 0
        return member, data

    @staticmethod
    def _archive(*members: tuple[tarfile.TarInfo, bytes]) -> bytes:
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode="w") as archive:
            for member, data in members:
                archive.addfile(member, io.BytesIO(data) if member.isfile() else None)
        return buffer.getvalue()

    def test_malformed_package_manifest_is_reported_as_source_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            for contents in ("{", "[]"):
                manifest = Path(temporary) / "packages.json"
                manifest.write_text(contents, encoding="utf-8")

                with (
                    self.subTest(contents=contents),
                    self.assertRaises(collect_alpine_sources.SourceError) as context,
                ):
                    collect_alpine_sources._load_packages([manifest])

                self.assertIn("package manifest", str(context.exception))

    def test_recipe_collection_preserves_blob_line_endings(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            cache = root / "aports"
            apkbuild = b"pkgname=foo\npkgver=1.0\npkgrel=0\n"
            commit = self._aports_commit(
                cache,
                {"main/foo/APKBUILD": ("100644", apkbuild)},
            )
            subprocess.run(
                ["git", "-C", str(cache), "config", "core.autocrlf", "true"],
                check=True,
            )
            output = root / "alpine"

            with (
                patch.object(collect_alpine_sources, "_prepare_aports"),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                collect_alpine_sources.collect_alpine_sources(
                    [self._package_manifest(root, commit)],
                    output,
                    cache,
                    skip_upstream=True,
                )

            collected = output / "recipes" / commit / "main/foo/APKBUILD"
            self.assertEqual(collected.read_bytes(), apkbuild)

    def test_recipe_symlink_is_collected_as_verified_regular_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            cache = root / "aports"
            commit = self._symlinked_recipe_commit(cache)
            output = root / "alpine"

            with (
                patch.object(collect_alpine_sources, "_prepare_aports"),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                collect_alpine_sources.collect_alpine_sources(
                    [self._package_manifest(root, commit)],
                    output,
                    cache,
                    skip_upstream=True,
                )

            upgrade = output / "recipes" / commit / "main/foo/foo.post-upgrade"
            self.assertFalse(upgrade.is_symlink())
            self.assertEqual(upgrade.read_bytes(), self.INSTALL_SCRIPT)
            source_manifest = json.loads(
                (output / "manifest.json").read_text(encoding="utf-8")
            )
            self.assertEqual(
                source_manifest["format"], AlpineBuildConstants.SOURCE_MANIFEST_FORMAT
            )
            self.assertEqual(
                source_manifest["executables"],
                [
                    f"recipes/{commit}/main/foo/foo.post-install",
                    f"recipes/{commit}/main/foo/foo.post-upgrade",
                ],
            )
            self.assertIn(
                f"recipes/{commit}/main/foo/foo.post-upgrade",
                dict(common.verify_sha256_sums(output).files),
            )
            if os.name == "posix":
                self.assertEqual(upgrade.stat().st_mode & 0o777, 0o755)

    def test_collection_replaces_stale_output_and_passes_release_validation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            cache = root / "aports"
            commit = self._symlinked_recipe_commit(cache)
            manifest = self._package_manifest(root, commit)
            source_dir = root / "sources"
            output = source_dir / "alpine"
            stale_files = (
                output / "recipes" / ("0" * 40) / "main/stale/APKBUILD",
                output / "upstream" / ("0" * 40) / "main/stale/stale.tar.gz",
            )
            for stale in stale_files:
                stale.parent.mkdir(parents=True)
                stale.write_bytes(b"stale")
            (output / "manifest.json").write_text("{}", encoding="utf-8")
            (output / "SHA256SUMS").write_text("", encoding="ascii")

            def fetch_upstream(bundle: Path, _alpine_version: str) -> None:
                upstream = bundle / "upstream" / commit / "main" / "foo"
                upstream.mkdir(parents=True)
                (upstream / "foo-1.0.tar.gz").write_bytes(b"upstream")

            with (
                patch.object(collect_alpine_sources, "_prepare_aports"),
                patch.object(
                    collect_alpine_sources,
                    "_fetch_upstream_sources",
                    side_effect=fetch_upstream,
                ),
                patch.object(BuildConstants, "SOURCE_DIR", source_dir),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                collect_alpine_sources.collect_alpine_sources([manifest], output, cache)
                executables = release._validate_alpine_sources([manifest])

            self.assertEqual(
                executables,
                {
                    f"recipes/{commit}/main/foo/foo.post-install",
                    f"recipes/{commit}/main/foo/foo.post-upgrade",
                },
            )
            for stale in stale_files:
                self.assertFalse(stale.parent.parent.exists(), stale)
            self.assertEqual(
                sorted(path.name for path in output.iterdir()),
                ["SHA256SUMS", "manifest.json", "recipes", "upstream"],
            )

    def test_recipe_extraction_materializes_symlinks_inside_recipe(self):
        member = self._member
        archive = self._archive(
            member("main/", tarfile.DIRTYPE, mode=0o775),
            member("main/foo/", tarfile.DIRTYPE, mode=0o775),
            member("main/foo/patches/", tarfile.DIRTYPE, mode=0o775),
            member("main/foo/APKBUILD", data=b"pkgname=foo\n"),
            member("main/foo/foo.post-install", data=self.INSTALL_SCRIPT, mode=0o775),
            member(
                "main/foo/foo.post-upgrade",
                tarfile.SYMTYPE,
                linkname="foo.post-install",
            ),
            member(
                "main/foo/foo.pre-upgrade",
                tarfile.SYMTYPE,
                linkname="./foo.post-upgrade",
            ),
            member("main/foo/patches/fix.patch", data=b"patch\n"),
            member(
                "main/foo/fix.patch",
                tarfile.SYMTYPE,
                linkname="patches/../patches/fix.patch",
            ),
            member(
                "main/foo/patches/APKBUILD", tarfile.SYMTYPE, linkname="../APKBUILD"
            ),
        )
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary)

            executables = collect_alpine_sources._safe_extract(
                archive, destination, "main/foo"
            )

            self.assertEqual(
                executables,
                [
                    "main/foo/foo.post-install",
                    "main/foo/foo.post-upgrade",
                    "main/foo/foo.pre-upgrade",
                ],
            )
            recipe = destination / "main" / "foo"
            expected = {
                "APKBUILD": (b"pkgname=foo\n", 0o644),
                "foo.post-install": (self.INSTALL_SCRIPT, 0o755),
                "foo.post-upgrade": (self.INSTALL_SCRIPT, 0o755),
                "foo.pre-upgrade": (self.INSTALL_SCRIPT, 0o755),
                "fix.patch": (b"patch\n", 0o644),
                "patches/fix.patch": (b"patch\n", 0o644),
                "patches/APKBUILD": (b"pkgname=foo\n", 0o644),
            }
            for relative, (contents, mode) in expected.items():
                with self.subTest(path=relative):
                    path = recipe / relative
                    self.assertFalse(path.is_symlink())
                    self.assertEqual(path.read_bytes(), contents)
                    if os.name == "posix":
                        self.assertEqual(path.stat().st_mode & 0o777, mode)

    def test_recipe_extraction_accepts_implicit_directories(self):
        member = self._member
        archive = self._archive(
            member("main/foo/APKBUILD", data=b"pkgname=foo\n"),
            member("main/foo/scripts/install", data=self.INSTALL_SCRIPT, mode=0o775),
            member(
                "main/foo/foo.post-install",
                tarfile.SYMTYPE,
                linkname="scripts/install",
            ),
            member(
                "main/foo/scripts/APKBUILD", tarfile.SYMTYPE, linkname="../APKBUILD"
            ),
        )
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary)

            executables = collect_alpine_sources._safe_extract(
                archive, destination, "main/foo"
            )

            self.assertEqual(
                executables,
                ["main/foo/foo.post-install", "main/foo/scripts/install"],
            )
            recipe = destination / "main" / "foo"
            for relative, contents in (
                ("foo.post-install", self.INSTALL_SCRIPT),
                ("scripts/APKBUILD", b"pkgname=foo\n"),
            ):
                with self.subTest(path=relative):
                    path = recipe / relative
                    self.assertFalse(path.is_symlink())
                    self.assertEqual(path.read_bytes(), contents)
            if os.name == "posix":
                for directory in (destination / "main", recipe, recipe / "scripts"):
                    with self.subTest(directory=directory.name):
                        self.assertEqual(directory.stat().st_mode & 0o777, 0o755)

    def test_recipe_extraction_rejects_unsafe_members(self):
        member = self._member
        recipe = (
            member("main/", tarfile.DIRTYPE),
            member("main/foo/", tarfile.DIRTYPE),
            member("main/foo/sub/", tarfile.DIRTYPE),
            member("main/foo/APKBUILD", data=b"pkgname=foo\n"),
            member("main/foo/sub/file", data=b"file\n"),
        )
        cases: dict[str, tuple[tuple[tarfile.TarInfo, bytes], ...]] = {
            "absolute path": (member("/main/foo/file"),),
            "parent path": (member("main/foo/../../file"),),
            "outside recipe": (member("main/bar/file"),),
            "duplicate path": (member("main/foo/APKBUILD"),),
            "member below a file": (member("main/foo/APKBUILD/file"),),
            "member below a symlink": (
                member("main/foo/alias", tarfile.SYMTYPE, linkname="sub"),
                member("main/foo/alias/file"),
            ),
            "hard link": (
                member("main/foo/link", tarfile.LNKTYPE, linkname="main/foo/APKBUILD"),
            ),
            "fifo": (member("main/foo/pipe", tarfile.FIFOTYPE),),
            "character device": (member("main/foo/device", tarfile.CHRTYPE),),
            "absolute symlink": (
                member("main/foo/link", tarfile.SYMTYPE, linkname="/etc/passwd"),
            ),
            "escaping symlink": (
                member("main/foo/link", tarfile.SYMTYPE, linkname="../../../passwd"),
            ),
            "sibling recipe symlink": (
                member("main/foo/link", tarfile.SYMTYPE, linkname="../bar/APKBUILD"),
            ),
            "dangling symlink": (
                member("main/foo/link", tarfile.SYMTYPE, linkname="missing"),
            ),
            "symlink through a missing directory": (
                member(
                    "main/foo/link",
                    tarfile.SYMTYPE,
                    linkname="missing/../APKBUILD",
                ),
            ),
            "directory symlink": (
                member("main/foo/link", tarfile.SYMTYPE, linkname="sub"),
            ),
            "symlink loop": (
                member("main/foo/first", tarfile.SYMTYPE, linkname="second"),
                member("main/foo/second", tarfile.SYMTYPE, linkname="first"),
            ),
            "symlinked directory traversal": (
                member("main/foo/link", tarfile.SYMTYPE, linkname="alias/file"),
                member("main/foo/alias", tarfile.SYMTYPE, linkname="sub"),
            ),
        }
        for name, members in cases.items():
            with self.subTest(case=name), tempfile.TemporaryDirectory() as temporary:
                destination = Path(temporary) / "recipes"

                with self.assertRaises(collect_alpine_sources.SourceError):
                    collect_alpine_sources._safe_extract(
                        self._archive(*recipe, *members),
                        destination,
                        "main/foo",
                    )

                self.assertEqual(list(destination.iterdir()), [])

    def test_collection_guards_output_before_replacing_it(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = self._package_manifest(root, "0" * 40)
            output = root / "alpine"
            (output / "recipes").mkdir(parents=True)
            notes = output / "notes.txt"
            notes.write_text("keep", encoding="utf-8")
            not_directory = root / "file"
            not_directory.write_text("keep", encoding="utf-8")

            for target, message in (
                (output, "unexpected entries: notes.txt"),
                (not_directory, "is not a directory"),
            ):
                with (
                    self.subTest(output=target.name),
                    patch.object(collect_alpine_sources, "_prepare_aports") as prepare,
                    self.assertRaisesRegex(
                        collect_alpine_sources.SourceError,
                        message,
                    ),
                ):
                    collect_alpine_sources.collect_alpine_sources(
                        [manifest],
                        target,
                        root / "aports",
                        skip_upstream=True,
                    )
                prepare.assert_not_called()
            self.assertTrue(notes.is_file())
            self.assertTrue((output / "recipes").is_dir())

            notes.unlink()
            with (
                patch.object(collect_alpine_sources, "_prepare_aports"),
                patch.object(
                    collect_alpine_sources.shutil,
                    "rmtree",
                    side_effect=PermissionError(13, "Permission denied"),
                ),
                self.assertRaisesRegex(
                    collect_alpine_sources.SourceError,
                    "cannot remove previous Alpine source output.*remove it manually",
                ),
            ):
                collect_alpine_sources.collect_alpine_sources(
                    [manifest],
                    output,
                    root / "aports",
                    skip_upstream=True,
                )

    def test_collection_refuses_symlinked_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "alpine"
            (output / "recipes").mkdir(parents=True)
            link = root / "alpine-link"
            try:
                link.symlink_to(output, target_is_directory=True)
            except OSError as error:
                self.skipTest(f"symlinks are unavailable: {error}")

            with (
                patch.object(collect_alpine_sources, "_prepare_aports") as prepare,
                self.assertRaisesRegex(
                    collect_alpine_sources.SourceError,
                    "Alpine source output is not a directory",
                ),
            ):
                collect_alpine_sources.collect_alpine_sources(
                    [self._package_manifest(root, "0" * 40)],
                    link,
                    root / "aports",
                    skip_upstream=True,
                )

            prepare.assert_not_called()
            self.assertTrue((output / "recipes").is_dir())

    def test_collection_refuses_output_junctions(self):
        if sys.platform != "win32":
            self.skipTest("directory junctions exist only on Windows")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = self._package_manifest(root, "0" * 40)
            elsewhere = root / "elsewhere"
            (elsewhere / "recipes").mkdir(parents=True)
            kept = elsewhere / "recipes" / "kept"
            kept.write_bytes(b"kept")
            output = root / "alpine"
            output.mkdir()
            cases = (
                (
                    root / "junction",
                    elsewhere,
                    root / "junction",
                    "Alpine source output is not a directory",
                ),
                (
                    output / "recipes",
                    elsewhere / "recipes",
                    output,
                    "Alpine source generated path is not a directory",
                ),
            )
            for link, target, collected, message in cases:
                subprocess.run(
                    ["cmd", "/c", "mklink", "/J", str(link), str(target)],
                    capture_output=True,
                    check=True,
                )
                with (
                    self.subTest(link=link.name),
                    patch.object(collect_alpine_sources, "_prepare_aports"),
                    self.assertRaisesRegex(collect_alpine_sources.SourceError, message),
                ):
                    collect_alpine_sources.collect_alpine_sources(
                        [manifest],
                        collected,
                        root / "aports",
                        skip_upstream=True,
                    )
                self.assertEqual(kept.read_bytes(), b"kept")

    def test_upstream_fetch_uses_mirror_and_restores_bundle_ownership(self):
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(collect_alpine_sources, "_run") as run,
        ):
            collect_alpine_sources._fetch_upstream_sources(Path(temporary), "3.24")

        command = run.call_args.args[0]
        self.assertEqual(command[:4], ["docker", "run", "--rm", "--volume"])
        self.assertEqual(
            command[5:7],
            [
                "--env",
                "DISTFILES_MIRROR=https://distfiles.alpinelinux.org/distfiles/v3.24",
            ],
        )
        self.assertEqual(command[7], "alpine:3.24")
        script = command[-1]
        self.assertIn("owner=$(stat -c '%u:%g' /bundle)", script)
        self.assertIn('chown -R "$owner" /bundle/upstream', script)
        self.assertLess(
            script.index("trap restore_owner EXIT"),
            script.index("mkdir -p /bundle/upstream"),
        )


if __name__ == "__main__":
    unittest.main()
