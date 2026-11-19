#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import argparse
import hashlib
import os
import re
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import archive, common, release  # noqa: E402
from nvx_tools.build_constants import BuildConstants  # noqa: E402


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

    def test_accepts_custom_error_message(self):
        with self.assertRaisesRegex(argparse.ArgumentTypeError, "^must be at least 1$"):
            common.positive_int("0", message="must be at least 1")


class JsonValidationTests(unittest.TestCase):
    def test_valid_containers_preserve_contents(self):
        self.assertEqual(
            common.require_object({"labels": ["guest"]}, "Issue"),
            {"labels": ["guest"]},
        )
        self.assertEqual(common.require_object({}, "Issue"), {})
        self.assertEqual(
            common.require_array(["guest", None], "Labels"), ["guest", None]
        )
        self.assertEqual(common.require_array([], "Labels"), [])

    def test_wrong_container_types_include_context(self):
        invalid_objects: tuple[object, ...] = (None, "text", 1, [])
        invalid_arrays: tuple[object, ...] = (None, "text", 1, {})
        for value in invalid_objects:
            with self.subTest(helper="object", value=value):
                with self.assertRaisesRegex(
                    common.ScriptError, "^Issue is not a JSON object$"
                ):
                    common.require_object(value, "Issue")
        for value in invalid_arrays:
            with self.subTest(helper="array", value=value):
                with self.assertRaisesRegex(
                    common.ScriptError, "^Labels is not a JSON array$"
                ):
                    common.require_array(value, "Labels")

    def test_object_rejects_non_string_keys(self):
        with self.assertRaisesRegex(
            common.ScriptError, "^Issue has non-string JSON keys$"
        ):
            common.require_object({1: "guest"}, "Issue")


class RepositoryValidationTests(unittest.TestCase):
    def test_valid_repository_is_returned_unchanged(self):
        for value in ("microsoft/nvx", "Owner_1/repo.name-2"):
            with self.subTest(value=value):
                self.assertEqual(common.require_repository(value), value)

    def test_malformed_and_non_ascii_names_are_rejected(self):
        for value in (
            "",
            "nvx",
            "/nvx",
            "microsoft/",
            "microsoft/nvx/extra",
            "microsoft/nvx ",
            "microsoft/nvx\n",
            "micro\u00f6soft/nvx",
            "microsoft/nvx\u0661",
        ):
            with self.subTest(value=value):
                with self.assertRaisesRegex(
                    common.ScriptError, "^Repository must have the form owner/repo$"
                ):
                    common.require_repository(value)


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


class GitHubTokenTests(unittest.TestCase):
    def test_explicit_tokens_take_precedence_without_gh(self):
        cases: tuple[tuple[dict[str, str], str | None], ...] = (
            ({"GH_TOKEN": "first", "GITHUB_TOKEN": "second"}, "first"),
            ({"GITHUB_TOKEN": "second"}, "second"),
            ({"GH_TOKEN": "", "GITHUB_TOKEN": "second"}, "second"),
            ({}, None),
        )
        for environment, expected in cases:
            with self.subTest(environment=environment):
                with patch.object(common, "run_capture") as run:
                    self.assertEqual(common.github_token(environment), expected)
                    run.assert_not_called()

    def test_local_login_is_opt_in_and_failures_are_explicit(self):
        for status, output in ((0, b"test-token\n"), (1, b""), (0, b"")):
            with self.subTest(status=status, output=output):
                result = common.CommandResult(
                    ("gh", "auth", "token"), status, output, b""
                )
                with patch.object(common, "run_capture", return_value=result) as run:
                    if status or not output:
                        with self.assertRaisesRegex(
                            common.ScriptError, "authentication required"
                        ):
                            common.github_token({}, use_gh_login=True)
                    else:
                        self.assertEqual(
                            common.github_token({}, use_gh_login=True), "test-token"
                        )
                    run.assert_called_once_with(["gh", "auth", "token"], env={})

    def test_opt_in_still_prefers_environment_token(self):
        with patch.object(common, "run_capture") as run:
            self.assertEqual(
                common.github_token({"GH_TOKEN": "explicit"}, use_gh_login=True),
                "explicit",
            )
        run.assert_not_called()


class SharedCommandTests(unittest.TestCase):
    def test_commands_that_cannot_start_raise_script_errors(self):
        with tempfile.TemporaryDirectory() as temporary:
            missing = Path(temporary) / "missing-command"
            for run in (common.run_checked, common.run_capture):
                with self.subTest(helper=run.__name__):
                    with self.assertRaisesRegex(
                        common.ScriptError,
                        f"^failed to run command {re.escape(str(missing))} --version: ",
                    ) as raised:
                        run([missing, "--version"])
                    self.assertIsInstance(raised.exception.__cause__, FileNotFoundError)


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

    def test_checksum_writer_rejects_trees_the_verifier_rejects(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            payload = root / "payload"
            payload.write_bytes(b"payload")
            nested = root / "nested" / "SHA256SUMS"
            nested.parent.mkdir()
            nested.write_bytes(b"nested")
            common.write_sha256_sums(root)
            self.assertIn(
                "nested/SHA256SUMS",
                dict(common.verify_sha256_sums(root).files),
            )
            checksums = (root / "SHA256SUMS").read_bytes()

            link = root / "payload-link"
            try:
                link.symlink_to(payload)
            except OSError as error:
                self.skipTest(f"symlinks are unavailable: {error}")
            with self.assertRaisesRegex(
                common.ScriptError,
                "symlink is not allowed in checksummed tree: payload-link",
            ):
                common.write_sha256_sums(root)
            self.assertEqual((root / "SHA256SUMS").read_bytes(), checksums)

            link.unlink()
            (root / "SHA256SUMS").unlink()
            (root / "SHA256SUMS").symlink_to(payload)
            with self.assertRaisesRegex(
                common.ScriptError,
                "symlink is not allowed in checksummed tree: SHA256SUMS",
            ):
                common.write_sha256_sums(root)
            self.assertEqual(payload.read_bytes(), b"payload")

    def test_checksum_writer_rejects_special_files(self):
        if sys.platform == "win32":
            self.skipTest("FIFOs are unavailable on Windows")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            os.mkfifo(root / "pipe")

            with self.assertRaisesRegex(
                common.ScriptError,
                "special file is not allowed in checksummed tree: pipe",
            ):
                common.write_sha256_sums(root)

            self.assertFalse((root / "SHA256SUMS").exists())

    def test_zip_release_archive_streams_members_past_zip64_limit(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "nvx-1.0.0-test"
            payload = source / "bin" / "openvmm"
            payload.parent.mkdir(parents=True)
            payload.write_bytes(b"x" * 4096)
            payload.chmod(0o755)
            common.write_sha256_sums(source)
            output = root / "nvx-1.0.0-test.zip"

            with patch.object(zipfile, "ZIP64_LIMIT", 1024):
                archive.create_reproducible_release_archive(source, output)

            with zipfile.ZipFile(output) as package:
                self.assertEqual(
                    package.read(f"{source.name}/bin/openvmm"), b"x" * 4096
                )

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

    def test_release_archives_ship_both_executables_executable(self):
        # Every package ships OpenVMM and nvx in bin/, executable whatever
        # mode the staging host stored, and publication finds both.
        for suffix, platform, extension in (
            (".tar.gz", "linux-kvm", ""),
            (".zip", "windows-whp", ".exe"),
        ):
            with (
                self.subTest(suffix=suffix),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                source = root / f"nvx-1.0.0-{platform}"
                for name in (f"openvmm{extension}", f"nvx{extension}"):
                    executable = source / "bin" / name
                    executable.parent.mkdir(parents=True, exist_ok=True)
                    executable.write_bytes(name.encode("ascii"))
                    executable.chmod(0o644)
                common.write_sha256_sums(source)
                package = root / f"nvx-1.0.0-{platform}{suffix}"
                release.create_release_archive(source, package)

                if suffix == ".tar.gz":
                    with tarfile.open(package, "r:gz") as archive_file:
                        modes = {
                            member.name: member.mode
                            for member in archive_file.getmembers()
                        }
                else:
                    with zipfile.ZipFile(package) as archive_file:
                        modes = {
                            member.filename: (member.external_attr >> 16) & 0o777
                            for member in archive_file.infolist()
                        }
                for path in release.release_package_executables(platform):
                    self.assertEqual(modes[f"{source.name}/{path}"], 0o755)
                self.assertEqual(modes[f"{source.name}/SHA256SUMS"], 0o644)
                release.require_release_package_executables(package, platform)

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


if __name__ == "__main__":
    unittest.main()
