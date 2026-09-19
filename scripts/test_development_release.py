#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import io
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import call, patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import development_release  # noqa: E402


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
                    "boolean-size",
                    development_release._DevelopmentReleaseAsset(
                        local.name,
                        True,
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


if __name__ == "__main__":
    unittest.main()
