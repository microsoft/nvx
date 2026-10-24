#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import io
import json
import os
import re
import sys
import tarfile
import tempfile
import unittest
import zipfile
from collections.abc import Mapping, Sequence
from pathlib import Path
from unittest.mock import call, patch

sys.path.insert(0, str(Path(__file__).parent))
import publish_development_release  # noqa: E402
from nvx_tools import development_release  # noqa: E402
from nvx_tools.build_constants import BuildConstants  # noqa: E402
from nvx_tools.common import ScriptError, sha256_file  # noqa: E402


class _FakeReleaseCli:
    """Model publication's GitHub CLI calls and fail the first upload lookup."""

    def __init__(self) -> None:
        self.tag: str | None = None
        self.target = ""
        self.draft = False
        self.prerelease = False
        self.assets: list[dict[str, object]] = []
        self.verbs: list[str] = []
        self.upload_failed = False

    def __call__(
        self,
        args: Sequence[str],
        *,
        timeout_seconds: float,
    ) -> development_release._GitHubCommandResult:
        del timeout_seconds
        verb = args[1]
        self.verbs.append(verb)
        not_found = development_release._GitHubCommandResult(
            1,
            "",
            "release not found\n",
            0.1,
        )
        stdout = ""
        if verb == "view":
            if self.tag is None:
                return not_found
            stdout = json.dumps(
                {
                    "tagName": self.tag,
                    "targetCommitish": self.target,
                    "isDraft": self.draft,
                    "isPrerelease": self.prerelease,
                    "assets": self.assets,
                }
            )
        elif verb == "create":
            self.tag = args[2]
            self.target = args[args.index("--target") + 1]
            self.draft = "--draft" in args
            self.prerelease = "--prerelease" in args
        elif verb == "upload":
            if not self.upload_failed:
                self.upload_failed = True
                return not_found
            path = Path(args[3])
            self.assets.append(
                {
                    "name": path.name,
                    "size": path.stat().st_size,
                    "digest": f"sha256:{sha256_file(path)}",
                    "state": "uploaded",
                }
            )
        elif verb == "edit":
            self.draft = "--draft=false" not in args
            self.prerelease = "--prerelease" in args
        else:
            raise AssertionError(f"unexpected gh release command: {verb}")
        return development_release._GitHubCommandResult(0, stdout, "", 0.1)


class DevelopmentReleaseTests(unittest.TestCase):
    def _package(
        self,
        root: Path,
        name: str,
        contents: bytes,
    ) -> development_release._LocalDevelopmentReleaseAsset:
        path = root / name
        path.write_bytes(contents)
        return development_release._local_asset(path)

    def _remote_asset(
        self,
        local: development_release._LocalDevelopmentReleaseAsset,
        *,
        digest: str | None = None,
        size: int | None = None,
        state: str = "uploaded",
    ) -> development_release._DevelopmentReleaseAsset:
        return development_release._DevelopmentReleaseAsset(
            local.name,
            local.size if size is None else size,
            f"sha256:{local.sha256}" if digest is None else digest,
            state,
        )

    def _release(
        self,
        tag: str,
        target: str,
        assets: list[development_release._DevelopmentReleaseAsset],
        *,
        draft: bool = True,
        prerelease: bool = True,
    ) -> development_release._DevelopmentRelease:
        return development_release._DevelopmentRelease(
            tag,
            target,
            draft,
            prerelease,
            tuple(assets),
        )

    def _result(
        self,
        returncode: int = 0,
        *,
        stderr: str = "",
        elapsed: float = 1.0,
        timed_out: bool = False,
    ) -> development_release._GitHubCommandResult:
        return development_release._GitHubCommandResult(
            returncode,
            "",
            stderr,
            elapsed,
            timed_out,
        )

    def test_transient_upload_failure_retries_and_succeeds(self):
        with tempfile.TemporaryDirectory() as temporary:
            local = self._package(Path(temporary), "nvx-linux.tar.gz", b"package")
            empty = self._release("v1", "a" * 40, [])
            complete = self._release(
                "v1",
                "a" * 40,
                [self._remote_asset(local)],
            )
            with (
                patch.object(
                    development_release,
                    "_query_development_release",
                    side_effect=[empty, empty, complete],
                ),
                patch.object(
                    development_release,
                    "_run_gh",
                    side_effect=[
                        self._result(1, stderr="HTTP 500: Error saving asset"),
                        self._result(),
                    ],
                ) as run,
                patch.object(development_release.time, "sleep") as sleep,
                patch("sys.stdout", io.StringIO()),
            ):
                development_release._upload_expected_asset(
                    "example/nvx",
                    "v1",
                    local,
                    attempts=3,
                    timeout_seconds=180,
                    retry_backoff_seconds=10,
                )

        self.assertEqual(run.call_count, 2)
        self.assertEqual(
            [item.kwargs["timeout_seconds"] for item in run.call_args_list],
            [180, 180],
        )
        sleep.assert_called_once_with(10)

    def test_upload_retry_exhaustion_fails_with_bounded_attempts(self):
        with tempfile.TemporaryDirectory() as temporary:
            local = self._package(Path(temporary), "nvx-linux.tar.gz", b"package")
            empty = self._release("v1", "a" * 40, [])
            timeout = self._result(124, elapsed=180, timed_out=True)
            with (
                patch.object(
                    development_release,
                    "_query_development_release",
                    side_effect=[empty, empty, empty, empty],
                ),
                patch.object(
                    development_release,
                    "_run_gh",
                    side_effect=[timeout, timeout, timeout],
                ) as run,
                patch.object(development_release.time, "sleep") as sleep,
                patch("sys.stdout", io.StringIO()),
                self.assertRaisesRegex(
                    development_release.ScriptError,
                    "after 3 attempts",
                ),
            ):
                development_release._upload_expected_asset(
                    "example/nvx",
                    "v1",
                    local,
                    attempts=3,
                    timeout_seconds=180,
                    retry_backoff_seconds=10,
                )

        self.assertEqual(run.call_count, 3)
        self.assertEqual(sleep.call_args_list, [call(10), call(20)])

    def test_release_not_found_upload_failure_retries_while_draft_exists(self):
        with tempfile.TemporaryDirectory() as temporary:
            local = self._package(Path(temporary), "nvx-linux.tar.gz", b"package")
            empty = self._release("v1", "a" * 40, [])
            complete = self._release(
                "v1",
                "a" * 40,
                [self._remote_asset(local)],
            )
            stdout = io.StringIO()
            with (
                patch.object(
                    development_release,
                    "_query_development_release",
                    side_effect=[empty, empty, complete],
                ),
                patch.object(
                    development_release,
                    "_run_gh",
                    side_effect=[
                        self._result(1, stderr="release not found\n"),
                        self._result(),
                    ],
                ) as run,
                patch.object(development_release.time, "sleep") as sleep,
                patch("sys.stdout", stdout),
            ):
                development_release._upload_expected_asset(
                    "example/nvx",
                    "v1",
                    local,
                    attempts=3,
                    timeout_seconds=180,
                    retry_backoff_seconds=10,
                )

        self.assertEqual(
            [tuple(item.args[0])[:2] for item in run.call_args_list],
            [("release", "upload"), ("release", "upload")],
        )
        self.assertEqual(
            [item.kwargs["timeout_seconds"] for item in run.call_args_list],
            [180, 180],
        )
        sleep.assert_called_once_with(10)
        self.assertIn("retrying in 10s: release not found", stdout.getvalue())

    def test_release_not_found_upload_failure_exhausts_bounded_attempts(self):
        with tempfile.TemporaryDirectory() as temporary:
            local = self._package(Path(temporary), "nvx-linux.tar.gz", b"package")
            empty = self._release("v1", "a" * 40, [])
            not_found = self._result(1, stderr="release not found\n")
            with (
                patch.object(
                    development_release,
                    "_query_development_release",
                    side_effect=[empty, empty, empty, empty],
                ),
                patch.object(
                    development_release,
                    "_run_gh",
                    side_effect=[not_found, not_found, not_found],
                ) as run,
                patch.object(development_release.time, "sleep") as sleep,
                patch("sys.stdout", io.StringIO()),
                self.assertRaisesRegex(
                    development_release.ScriptError,
                    "after 3 attempts: release not found",
                ),
            ):
                development_release._upload_expected_asset(
                    "example/nvx",
                    "v1",
                    local,
                    attempts=3,
                    timeout_seconds=180,
                    retry_backoff_seconds=10,
                )

        self.assertEqual(run.call_count, 3)
        self.assertEqual(sleep.call_args_list, [call(10), call(20)])

    def test_release_not_found_upload_failure_stops_when_release_disappears(self):
        with tempfile.TemporaryDirectory() as temporary:
            local = self._package(Path(temporary), "nvx-linux.tar.gz", b"package")
            empty = self._release("v1", "a" * 40, [])
            with (
                patch.object(
                    development_release,
                    "_query_development_release",
                    side_effect=[empty, None],
                ),
                patch.object(
                    development_release,
                    "_run_gh",
                    return_value=self._result(1, stderr="release not found\n"),
                ) as run,
                patch.object(development_release.time, "sleep") as sleep,
                patch("sys.stdout", io.StringIO()),
                self.assertRaisesRegex(
                    development_release.ScriptError,
                    "development release v1 disappeared",
                ),
            ):
                development_release._upload_expected_asset(
                    "example/nvx",
                    "v1",
                    local,
                    attempts=3,
                    timeout_seconds=180,
                    retry_backoff_seconds=10,
                )

        run.assert_called_once()
        sleep.assert_not_called()

    def test_release_not_found_upload_failure_stops_when_release_was_published(self):
        with tempfile.TemporaryDirectory() as temporary:
            local = self._package(Path(temporary), "nvx-linux.tar.gz", b"package")
            empty = self._release("v1", "a" * 40, [])
            published = self._release("v1", "a" * 40, [], draft=False)
            with (
                patch.object(
                    development_release,
                    "_query_development_release",
                    side_effect=[empty, published],
                ),
                patch.object(
                    development_release,
                    "_run_gh",
                    return_value=self._result(1, stderr="release not found\n"),
                ) as run,
                patch.object(development_release.time, "sleep") as sleep,
                patch("sys.stdout", io.StringIO()),
                self.assertRaisesRegex(
                    development_release.ScriptError,
                    "release v1 was published before",
                ),
            ):
                development_release._upload_expected_asset(
                    "example/nvx",
                    "v1",
                    local,
                    attempts=3,
                    timeout_seconds=180,
                    retry_backoff_seconds=10,
                )

        run.assert_called_once()
        sleep.assert_not_called()

    def test_permanent_upload_failure_is_not_retried(self):
        with tempfile.TemporaryDirectory() as temporary:
            local = self._package(Path(temporary), "nvx-linux.tar.gz", b"package")
            empty = self._release("v1", "a" * 40, [])
            with (
                patch.object(
                    development_release,
                    "_query_development_release",
                    side_effect=[empty, empty],
                ),
                patch.object(
                    development_release,
                    "_run_gh",
                    return_value=self._result(
                        1,
                        stderr="HTTP 403: Resource not accessible by integration",
                    ),
                ) as run,
                patch.object(development_release.time, "sleep") as sleep,
                patch("sys.stdout", io.StringIO()),
                self.assertRaisesRegex(
                    development_release.ScriptError,
                    "uploading release asset nvx-linux.tar.gz exited 1",
                ),
            ):
                development_release._upload_expected_asset(
                    "example/nvx",
                    "v1",
                    local,
                    attempts=3,
                    timeout_seconds=180,
                    retry_backoff_seconds=10,
                )

        run.assert_called_once()
        sleep.assert_not_called()

    def test_release_lookup_failure_matches_only_bare_not_found(self):
        for output in ("release not found", "release not found\n", "Release not found"):
            with self.subTest(output=output):
                self.assertTrue(
                    development_release._is_release_lookup_failure(
                        self._result(1, stderr=output)
                    )
                )

        non_matches = (
            self._result(124, stderr="release not found", timed_out=True),
            self._result(1, stderr="HTTP 404: Not Found"),
            self._result(
                1,
                stderr="HTTP 403: Resource not accessible by integration",
            ),
            self._result(1, stderr="release not found: v1"),
            self._result(1),
        )
        for result in non_matches:
            with self.subTest(result=result):
                self.assertFalse(development_release._is_release_lookup_failure(result))

    def test_publication_recovers_from_release_not_found_upload(self):
        with tempfile.TemporaryDirectory() as temporary:
            packages = publish_development_release.release_packages(
                Path(temporary),
                "1.2.3",
            )
            for platform, package in packages.items():
                package.write_bytes(platform.encode("ascii"))
            fake = _FakeReleaseCli()
            stdout = io.StringIO()
            with (
                patch.object(development_release, "_run_gh", fake),
                patch.object(development_release.time, "sleep") as sleep,
                patch("sys.stdout", stdout),
            ):
                development_release.publish_development_release(
                    "example/nvx",
                    "a" * 40,
                    "1.2.3",
                    tuple(packages.values()),
                )

        self.assertEqual(
            fake.verbs,
            [
                "view",
                "create",
                "view",
                "view",
                "upload",
                "view",
                "upload",
                "view",
                "view",
                "upload",
                "view",
                "view",
                "upload",
                "view",
                "view",
                "edit",
                "view",
            ],
        )
        self.assertFalse(fake.draft)
        self.assertTrue(fake.prerelease)
        self.assertEqual(
            {asset["name"] for asset in fake.assets},
            {package.name for package in packages.values()},
        )
        self.assertEqual(len(fake.assets), 3)
        sleep.assert_called_once_with(10)
        output = stdout.getvalue()
        self.assertIn("retrying in 10s: release not found", output)
        self.assertIn(
            "published development prerelease v1.2.3-dev.aaaaaaaaaaaa",
            output,
        )

    def test_partial_draft_resumes_without_reuploading_valid_asset(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            assets = [
                self._package(root, "nvx-linux-kvm.tar.gz", b"kvm"),
                self._package(root, "nvx-linux-mshv.tar.gz", b"mshv"),
                self._package(root, "nvx-windows-whp.zip", b"whp"),
            ]
            target = "a" * 40
            tag = "v1.2.3-dev.aaaaaaaaaaaa"
            remote = [self._remote_asset(asset) for asset in assets]
            initial = self._release(tag, target, remote[:1])
            first_upload = self._release(tag, target, remote[:2])
            complete = self._release(tag, target, remote)
            published = self._release(tag, target, remote, draft=False)
            with (
                patch.object(
                    development_release,
                    "_query_development_release",
                    side_effect=[
                        initial,
                        initial,
                        initial,
                        first_upload,
                        first_upload,
                        complete,
                        complete,
                        published,
                    ],
                ),
                patch.object(
                    development_release,
                    "_run_gh",
                    side_effect=[self._result(), self._result(), self._result()],
                ) as run,
                patch("sys.stdout", io.StringIO()),
            ):
                development_release.publish_development_release(
                    "example/nvx",
                    target,
                    "1.2.3",
                    [asset.path for asset in assets],
                )

        commands = [tuple(item.args[0]) for item in run.call_args_list]
        uploads = [
            Path(command[3]).name
            for command in commands
            if command[:2] == ("release", "upload")
        ]
        self.assertEqual(
            uploads,
            ["nvx-linux-mshv.tar.gz", "nvx-windows-whp.zip"],
        )
        self.assertFalse(
            any(command[:2] == ("release", "delete-asset") for command in commands)
        )
        self.assertEqual(commands[-1][:2], ("release", "edit"))

    def test_mismatched_existing_asset_is_rejected_without_mutation(self):
        with tempfile.TemporaryDirectory() as temporary:
            local = self._package(Path(temporary), "nvx-linux.tar.gz", b"package")
            target = "a" * 40
            cases = (
                (
                    "digest",
                    development_release._DevelopmentReleaseAsset(
                        local.name,
                        local.size,
                        f"sha256:{'0' * 64}",
                        "uploaded",
                    ),
                ),
                (
                    "missing-digest",
                    development_release._DevelopmentReleaseAsset(
                        local.name,
                        local.size,
                        None,
                        "uploaded",
                    ),
                ),
                (
                    "size",
                    development_release._DevelopmentReleaseAsset(
                        local.name,
                        local.size + 1,
                        f"sha256:{local.sha256}",
                        "uploaded",
                    ),
                ),
                (
                    "state",
                    development_release._DevelopmentReleaseAsset(
                        local.name,
                        local.size,
                        f"sha256:{local.sha256}",
                        "new",
                    ),
                ),
            )
            for name, remote_asset in cases:
                with self.subTest(name=name):
                    mismatched = self._release(
                        "v1",
                        target,
                        [remote_asset],
                    )
                    with (
                        patch.object(
                            development_release,
                            "_query_development_release",
                            return_value=mismatched,
                        ),
                        patch.object(development_release, "_run_gh") as run,
                        patch("sys.stdout", io.StringIO()),
                        self.assertRaisesRegex(
                            development_release.ScriptError,
                            "already exists but is invalid.*refusing to replace",
                        ),
                    ):
                        development_release._upload_expected_asset(
                            "example/nvx",
                            "v1",
                            local,
                            attempts=3,
                            timeout_seconds=180,
                            retry_backoff_seconds=10,
                        )

                    run.assert_not_called()

    def test_failed_asset_created_by_successful_attempt_is_cleaned_up(self):
        with tempfile.TemporaryDirectory() as temporary:
            local = self._package(Path(temporary), "nvx-linux.tar.gz", b"package")
            target = "a" * 40
            empty = self._release("v1", target, [])
            mismatched = self._release(
                "v1",
                target,
                [self._remote_asset(local, state="new")],
            )
            complete = self._release(
                "v1",
                target,
                [self._remote_asset(local)],
            )
            with (
                patch.object(
                    development_release,
                    "_query_development_release",
                    side_effect=[empty, mismatched, empty, complete],
                ),
                patch.object(
                    development_release,
                    "_run_gh",
                    side_effect=[self._result(), self._result(), self._result()],
                ) as run,
                patch.object(development_release.time, "sleep") as sleep,
                patch("sys.stdout", io.StringIO()),
            ):
                development_release._upload_expected_asset(
                    "example/nvx",
                    "v1",
                    local,
                    attempts=3,
                    timeout_seconds=180,
                    retry_backoff_seconds=10,
                )

        commands = [tuple(item.args[0]) for item in run.call_args_list]
        self.assertEqual(
            [command[:2] for command in commands],
            [
                ("release", "upload"),
                ("release", "delete-asset"),
                ("release", "upload"),
            ],
        )
        sleep.assert_called_once_with(10)

    def test_failed_upload_does_not_delete_asset_with_uncertain_ownership(self):
        with tempfile.TemporaryDirectory() as temporary:
            local = self._package(Path(temporary), "nvx-linux.tar.gz", b"package")
            target = "a" * 40
            empty = self._release("v1", target, [])
            mismatched = self._release(
                "v1",
                target,
                [self._remote_asset(local, state="new")],
            )
            with (
                patch.object(
                    development_release,
                    "_query_development_release",
                    side_effect=[empty, mismatched],
                ),
                patch.object(
                    development_release,
                    "_run_gh",
                    return_value=self._result(124, timed_out=True),
                ) as run,
                patch.object(development_release.time, "sleep") as sleep,
                patch("sys.stdout", io.StringIO()),
                self.assertRaisesRegex(
                    development_release.ScriptError,
                    "ownership is uncertain",
                ),
            ):
                development_release._upload_expected_asset(
                    "example/nvx",
                    "v1",
                    local,
                    attempts=3,
                    timeout_seconds=180,
                    retry_backoff_seconds=10,
                )

        command = tuple(run.call_args.args[0])
        self.assertEqual(command[:2], ("release", "upload"))
        sleep.assert_not_called()

    def test_timed_out_client_detects_server_side_upload(self):
        with tempfile.TemporaryDirectory() as temporary:
            local = self._package(Path(temporary), "nvx-linux.tar.gz", b"package")
            empty = self._release("v1", "a" * 40, [])
            complete = self._release(
                "v1",
                "a" * 40,
                [self._remote_asset(local)],
            )
            with (
                patch.object(
                    development_release,
                    "_query_development_release",
                    side_effect=[empty, complete],
                ),
                patch.object(
                    development_release,
                    "_run_gh",
                    return_value=self._result(124, elapsed=180, timed_out=True),
                ) as run,
                patch.object(development_release.time, "sleep") as sleep,
                patch("sys.stdout", io.StringIO()),
            ):
                development_release._upload_expected_asset(
                    "example/nvx",
                    "v1",
                    local,
                    attempts=3,
                    timeout_seconds=180,
                    retry_backoff_seconds=10,
                )

        run.assert_called_once()
        sleep.assert_not_called()

    def test_final_asset_verification_gates_publication(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            assets = [
                self._package(root, "nvx-linux-kvm.tar.gz", b"kvm"),
                self._package(root, "nvx-linux-mshv.tar.gz", b"mshv"),
                self._package(root, "nvx-windows-whp.zip", b"whp"),
            ]
            target = "a" * 40
            tag = "v1.2.3-dev.aaaaaaaaaaaa"
            initial = self._release(tag, target, [])
            incomplete = self._release(
                tag,
                target,
                [self._remote_asset(asset) for asset in assets[:2]],
            )
            with (
                patch.object(
                    development_release,
                    "_query_development_release",
                    side_effect=[initial, incomplete],
                ),
                patch.object(
                    development_release,
                    "_upload_expected_asset",
                ) as upload,
                patch.object(development_release, "_run_gh") as run,
                patch("sys.stdout", io.StringIO()),
                self.assertRaisesRegex(
                    development_release.ScriptError,
                    "asset set",
                ),
            ):
                development_release.publish_development_release(
                    "example/nvx",
                    target,
                    "1.2.3",
                    [asset.path for asset in assets],
                )

        self.assertEqual(upload.call_count, 3)
        run.assert_not_called()

    def test_new_release_is_created_as_draft_without_assets(self):
        created = self._release("v1", "a" * 40, [])
        with (
            patch.object(
                development_release,
                "_run_gh",
                return_value=self._result(),
            ) as run,
            patch.object(
                development_release,
                "_query_development_release",
                return_value=created,
            ),
            patch("sys.stdout", io.StringIO()),
        ):
            development_release._create_development_release(
                "example/nvx",
                "v1",
                "a" * 40,
                "title",
                "notes",
            )

        command = tuple(run.call_args.args[0])
        self.assertEqual(command[:3], ("release", "create", "v1"))
        self.assertIn("--draft", command)
        self.assertIn("--prerelease", command)
        self.assertFalse(any(item.endswith((".zip", ".tar.gz")) for item in command))


def _write_package(path: Path, members: Mapping[str, bytes | None]) -> None:
    """Write a release archive whose package root holds `members`, files with
    their contents, or directories where the contents are None."""
    path.parent.mkdir(parents=True, exist_ok=True)
    root = path.name.removesuffix(".tar.gz").removesuffix(".zip")
    if path.name.endswith(".tar.gz"):
        with tarfile.open(path, "w:gz") as archive:
            for name, data in members.items():
                info = tarfile.TarInfo(f"{root}/{name}")
                if data is None:
                    info.type = tarfile.DIRTYPE
                    archive.addfile(info)
                    continue
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
        return
    with zipfile.ZipFile(path, "w") as archive:
        for name, data in members.items():
            archive.writestr(
                f"{root}/{name}" + ("/" if data is None else ""), data or b""
            )


class DevelopmentReleasePackageTests(unittest.TestCase):
    """The packages that publication checks: each ships OpenVMM and nvx-rs."""

    def _packages(self, directory: Path) -> dict[str, Path]:
        packages = publish_development_release.release_packages(directory, "1.2.3")
        for platform, package in packages.items():
            suffix = ".exe" if platform.startswith("windows-") else ""
            _write_package(
                package,
                {
                    "SHA256SUMS": b"sums",
                    f"bin/openvmm{suffix}": b"openvmm",
                    f"bin/nvx-rs{suffix}": b"nvx-rs",
                },
            )
        return packages

    def test_packages_are_named_after_their_platforms(self):
        self.assertEqual(
            publish_development_release.release_packages(Path("dist"), "1.2.3"),
            {
                "linux-kvm": Path("dist") / "nvx-1.2.3-linux-kvm.tar.gz",
                "linux-mshv": Path("dist") / "nvx-1.2.3-linux-mshv.tar.gz",
                "windows-whp": Path("dist") / "nvx-1.2.3-windows-whp.zip",
            },
        )

    def test_every_package_must_ship_openvmm_and_nvx_rs(self):
        with tempfile.TemporaryDirectory() as temporary:
            packages = self._packages(Path(temporary))
            publish_development_release.check_release_packages(packages)
            for platform, members, missing in (
                ("linux-kvm", {"bin/openvmm": b"openvmm"}, "bin/nvx-rs"),
                ("linux-mshv", {"bin/nvx-rs": b"nvx-rs"}, "bin/openvmm"),
                (
                    "linux-kvm",
                    {"bin/openvmm": b"openvmm", "bin/nvx-rs": None},
                    "bin/nvx-rs",
                ),
                # Windows packages ship Windows executables.
                (
                    "windows-whp",
                    {"bin/openvmm.exe": b"openvmm", "bin/nvx-rs": b"nvx-rs"},
                    "bin/nvx-rs.exe",
                ),
            ):
                with self.subTest(platform=platform, members=sorted(members)):
                    package = packages[platform]
                    package.unlink()
                    _write_package(package, members)
                    with self.assertRaisesRegex(
                        ScriptError,
                        f"^{re.escape(package.name)} does not ship {missing}$",
                    ):
                        publish_development_release.check_release_packages(packages)
                    self._packages(Path(temporary))
            packages["linux-mshv"].unlink()
            with self.assertRaisesRegex(
                ScriptError, "development release package nvx-1.2.3-linux-mshv.tar.gz"
            ):
                publish_development_release.check_release_packages(packages)

    def test_publication_checks_the_packages_before_it_publishes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "VERSION").write_text("1.2.3\n", encoding="ascii")
            packages = self._packages(root / "dist")
            environment = {"GH_REPO": "example/nvx", "GITHUB_SHA": "a" * 40}
            previous = os.getcwd()
            os.chdir(root)
            try:
                with (
                    patch.dict(os.environ, environment),
                    patch.object(BuildConstants, "REPO_ROOT", root),
                    patch.object(
                        publish_development_release, "publish_development_release"
                    ) as publish,
                ):
                    publish_development_release.main()
                    publish.assert_called_once_with(
                        "example/nvx",
                        "a" * 40,
                        "1.2.3",
                        tuple(
                            Path("dist") / package.name for package in packages.values()
                        ),
                    )
                    publish.reset_mock()
                    _write_package(packages["windows-whp"], {"bin/openvmm.exe": b""})
                    with self.assertRaisesRegex(ScriptError, "bin/nvx-rs.exe"):
                        publish_development_release.main()
                    publish.assert_not_called()
            finally:
                os.chdir(previous)


if __name__ == "__main__":
    unittest.main()
