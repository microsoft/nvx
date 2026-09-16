#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

from __future__ import annotations

import multiprocessing
import tempfile
import unittest
from collections.abc import Generator
from contextlib import contextmanager
from multiprocessing.process import BaseProcess
from multiprocessing.synchronize import Event
from pathlib import Path
from unittest.mock import patch

from nvx_tools import common, release
from test_release_install_lock import PROCESS_TIMEOUT_SECONDS, _runtime_snapshot

PUBLICATION_FAILURE = "injected digest publication failure"
PUBLICATION_PHASES = ("preflight", "backup", "promotion", "rollback", "cleanup")


def _bundle(bundle: Path, digest: Path, contents: bytes) -> None:
    bundle.mkdir(parents=True)
    digest.parent.mkdir(parents=True, exist_ok=True)
    (bundle / "SOURCE-MANIFEST.json").write_bytes(contents)
    digest.write_text(
        common.sha256_file(bundle / "SOURCE-MANIFEST.json") + "\n", encoding="ascii"
    )


def _publication_inputs(
    root: Path, *, existing: bool = True
) -> tuple[Path, Path, Path, Path, Path, Path]:
    destination = root / "output" / "release"
    digest = root / "digests" / "release.sha256"
    first = root / "first" / "bundle"
    first_digest = root / "first" / "digest"
    second = root / "second" / "bundle"
    second_digest = root / "second" / "digest"
    if existing:
        _bundle(destination, digest, b"prior")
    _bundle(first, first_digest, b"first")
    _bundle(second, second_digest, b"second")
    return destination, digest, first, first_digest, second, second_digest


def _snapshot(root: Path) -> dict[str, tuple[bytes, int] | None]:
    return {
        name: value
        for name, value in _runtime_snapshot(root, lock_suffix=".publish.lock").items()
        if not name.endswith(".publish.lock")
    }


def _paused_publish_worker(
    staged: Path,
    destination: Path,
    staged_digest: Path,
    digest: Path,
    phase: str,
    paused: Event,
    resume: Event,
) -> None:
    original_publish = release._publish_release_locked
    original_replace = Path.replace
    original_remove = release._remove_runtime_path
    owned_backups: set[Path] = set()
    did_pause = False

    def pause() -> None:
        nonlocal did_pause
        if did_pause:
            return
        did_pause = True
        paused.set()
        if not resume.wait(PROCESS_TIMEOUT_SECONDS):
            raise TimeoutError(f"publisher was not resumed during {phase}")

    def publish(
        publications: list[tuple[Path, Path]], legacy_backups: dict[Path, Path]
    ) -> None:
        if phase == "preflight":
            pause()
        original_publish(publications, legacy_backups)

    def replace(source: Path, target: Path) -> Path:
        if source == staged_digest and phase == "rollback":
            raise OSError(PUBLICATION_FAILURE)
        if target.name == "content":
            owned_backups.add(target.parent)
        if source.name == "content" and source.parent not in owned_backups:
            raise AssertionError(
                f"publisher attempted to restore a foreign backup: {source}"
            )
        result = original_replace(source, target)
        if (phase == "backup" and source == destination) or (
            phase == "promotion" and source == staged
        ):
            pause()
        return result

    def remove(path: Path) -> None:
        if (phase == "rollback" and path == destination) or (
            phase == "cleanup" and path in owned_backups
        ):
            pause()
        original_remove(path)

    with (
        patch.object(release, "_publish_release_locked", side_effect=publish),
        patch.object(Path, "replace", autospec=True, side_effect=replace),
        patch.object(release, "_remove_runtime_path", side_effect=remove),
    ):
        try:
            release._publish_release_directory(
                staged, destination, (staged_digest, digest)
            )
        except OSError as error:
            if phase != "rollback" or str(error) != PUBLICATION_FAILURE:
                raise
        else:
            if phase == "rollback":
                raise AssertionError("the injected publication failure did not occur")


class ReleasePublicationLockTests(unittest.TestCase):
    @contextmanager
    def paused_publisher(
        self,
        staged: Path,
        destination: Path,
        staged_digest: Path,
        digest: Path,
        phase: str,
    ) -> Generator[tuple[BaseProcess, Event], None, None]:
        context = multiprocessing.get_context("spawn")
        paused = context.Event()
        resume = context.Event()
        process = context.Process(
            target=_paused_publish_worker,
            args=(staged, destination, staged_digest, digest, phase, paused, resume),
        )
        process.start()
        try:
            self.assertTrue(
                paused.wait(PROCESS_TIMEOUT_SECONDS),
                f"publisher did not reach {phase}; exit code: {process.exitcode}",
            )
            yield process, resume
        finally:
            resume.set()
            process.join(PROCESS_TIMEOUT_SECONDS)
            if process.is_alive():
                process.terminate()
                process.join(PROCESS_TIMEOUT_SECONDS)
            self.assertFalse(process.is_alive(), "test publisher did not exit")
            exitcode = process.exitcode
            process.close()
            self.assertEqual(exitcode, 0, f"publisher failed during {phase}")

    def assert_pair(self, bundle: Path, digest: Path, contents: bytes) -> None:
        manifest = bundle / "SOURCE-MANIFEST.json"
        self.assertEqual(manifest.read_bytes(), contents)
        self.assertEqual(
            digest.read_text(encoding="ascii"), common.sha256_file(manifest) + "\n"
        )

    def test_competing_publisher_cannot_touch_any_publication_phase(self):
        for phase in PUBLICATION_PHASES:
            with (
                self.subTest(phase=phase),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                destination, digest, first, first_digest, second, second_digest = (
                    _publication_inputs(root)
                )
                with self.paused_publisher(
                    first, destination, first_digest, digest, phase
                ):
                    before = _snapshot(root)
                    with (
                        patch.object(release, "LOCK_TIMEOUT_SECONDS", 0),
                        patch.object(release, "_publish_release_locked") as publish,
                        self.assertRaisesRegex(
                            common.ScriptError,
                            "timed out waiting for release publication lock",
                        ),
                    ):
                        release._publish_release_directory(
                            second, destination, (second_digest, digest)
                        )
                    publish.assert_not_called()
                    self.assertEqual(_snapshot(root), before)
                self.assert_pair(
                    destination, digest, b"prior" if phase == "rollback" else b"first"
                )
                self.assertEqual(list(root.rglob(".*.previous-*")), [])
                self.assertEqual(len(list(root.rglob("*.publish.lock"))), 2)

    def test_concurrent_publishers_restore_only_their_own_backups(self):
        for phase in ("promotion", "rollback"):
            with self.subTest(first_publisher_phase=phase):
                self.check_waiting_publisher_rollback(phase)

    def check_waiting_publisher_rollback(self, phase: str) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            destination, digest, first, first_digest, second, second_digest = (
                _publication_inputs(root)
            )
            original_replace = Path.replace
            second_backups: set[Path] = set()
            restored_backups: set[Path] = set()

            def replace(source: Path, target: Path) -> Path:
                if source == second_digest:
                    raise OSError(PUBLICATION_FAILURE)
                if target.name == "content":
                    second_backups.add(target.parent)
                if source.name == "content":
                    self.assertIn(source.parent, second_backups)
                    restored_backups.add(source.parent)
                return original_replace(source, target)

            with self.paused_publisher(
                first, destination, first_digest, digest, phase
            ) as (process, resume):
                first_backups = set(root.rglob(".*.previous-*"))
                self.assertEqual(len(first_backups), 2)

                def release_owner(_delay: float) -> None:
                    self.assertEqual(second_backups, set())
                    resume.set()
                    process.join(PROCESS_TIMEOUT_SECONDS)
                    self.assertFalse(process.is_alive())

                with (
                    patch.object(
                        release.time, "sleep", side_effect=release_owner
                    ) as wait,
                    patch.object(Path, "replace", autospec=True, side_effect=replace),
                    self.assertRaisesRegex(OSError, PUBLICATION_FAILURE),
                ):
                    release._publish_release_directory(
                        second, destination, (second_digest, digest)
                    )
                wait.assert_called_once()

            self.assertEqual(len(second_backups), 2)
            self.assertEqual(restored_backups, second_backups)
            self.assertTrue(first_backups.isdisjoint(second_backups))
            self.assertEqual(list(root.rglob(".*.previous-*")), [])
            self.assert_pair(
                destination, digest, b"prior" if phase == "rollback" else b"first"
            )

    def test_publishers_sharing_either_destination_or_an_alias_are_serialized(self):
        for shared in ("bundle", "digest", "alias"):
            with (
                self.subTest(shared=shared),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                destination, digest, first, first_digest, second, second_digest = (
                    _publication_inputs(root)
                )
                other_destination = (
                    destination.with_name("other")
                    if shared == "digest"
                    else destination
                )
                other_digest = (
                    digest.with_name("other.sha256") if shared == "bundle" else digest
                )
                if shared == "alias":
                    other_destination = (
                        destination.parent / "unused" / ".." / destination.name
                    )
                with self.paused_publisher(
                    first, destination, first_digest, digest, "preflight"
                ):
                    before = _snapshot(root)
                    with (
                        patch.object(release, "LOCK_TIMEOUT_SECONDS", 0),
                        self.assertRaisesRegex(
                            common.ScriptError, "release publication lock"
                        ),
                    ):
                        release._publish_release_directory(
                            second, other_destination, (second_digest, other_digest)
                        )
                    self.assertEqual(_snapshot(root), before)
                self.assert_pair(destination, digest, b"first")

    def test_force_preflight_is_rechecked_after_waiting_for_the_lock(self):
        for force in (False, True):
            with self.subTest(force=force):
                self.check_waiting_preflight(force)

    def check_waiting_preflight(self, force: bool) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            destination, digest, first, first_digest, second, second_digest = (
                _publication_inputs(root, existing=False)
            )
            with (
                patch.object(release, "REPO_ROOT", root),
                self.paused_publisher(
                    first, destination, first_digest, digest, "preflight"
                ) as (process, resume),
            ):
                release._check_release_destination(destination, force)
                self.assertFalse(destination.exists())

                def release_owner(_delay: float) -> None:
                    resume.set()
                    process.join(PROCESS_TIMEOUT_SECONDS)
                    self.assertFalse(process.is_alive())

                with (
                    patch.object(
                        release.time, "sleep", side_effect=release_owner
                    ) as wait,
                    self.assertRaisesRegex(
                        common.ScriptError,
                        "below dist/" if force else "already exists",
                    ),
                ):
                    release._publish_release_directory(
                        second, destination, (second_digest, digest), force=force
                    )
                wait.assert_called_once()
            self.assert_pair(destination, digest, b"first")
            self.assertTrue(second.is_dir())
            self.assertTrue(second_digest.is_file())
            self.assertEqual(list(root.rglob(".*.previous-*")), [])

    def test_foreign_backups_are_never_restored_or_cleaned_up(self):
        for point in ("allocation", "promotion"):
            with self.subTest(point=point):
                self.check_foreign_backups(point)

    def check_foreign_backups(self, point: str) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            destination, digest, _first, _first_digest, second, second_digest = (
                _publication_inputs(root)
            )
            foreign_bundle = destination.with_name(".release.previous-foreign")
            foreign_digest = digest.with_name(".release.sha256.previous-foreign")
            _bundle(foreign_bundle / "content", foreign_digest / "content", b"foreign")
            prior_bundle = _snapshot(foreign_bundle)
            prior_digest = _snapshot(foreign_digest)
            original_mkdtemp = release.tempfile.mkdtemp
            original_replace = Path.replace
            allocations = 0

            def mkdtemp(*, prefix: str, dir: Path) -> str:
                nonlocal allocations
                allocations += 1
                if point == "allocation" and allocations == 2:
                    raise OSError("injected backup allocation failure")
                return original_mkdtemp(prefix=prefix, dir=dir)

            def replace(source: Path, target: Path) -> Path:
                self.assertNotIn(source.parent, (foreign_bundle, foreign_digest))
                if point == "promotion" and source == second_digest:
                    raise OSError(PUBLICATION_FAILURE)
                return original_replace(source, target)

            with (
                patch.object(release.tempfile, "mkdtemp", side_effect=mkdtemp),
                patch.object(Path, "replace", autospec=True, side_effect=replace),
                self.assertRaisesRegex(OSError, "injected"),
            ):
                release._publish_release_directory(
                    second, destination, (second_digest, digest)
                )
            self.assert_pair(destination, digest, b"prior")
            self.assertEqual(_snapshot(foreign_bundle), prior_bundle)
            self.assertEqual(_snapshot(foreign_digest), prior_digest)
            self.assertEqual(
                set(root.rglob(".*.previous-*")), {foreign_bundle, foreign_digest}
            )

    def test_publication_rejects_lock_path_overlap_before_mutation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            destination, _digest, first, first_digest, _second, _second_digest = (
                _publication_inputs(root)
            )
            before = _snapshot(root)
            with self.assertRaisesRegex(common.ScriptError, "paths overlap"):
                release._publish_release_directory(
                    first,
                    destination,
                    (first_digest, destination.with_name(".release.publish.lock")),
                )
            self.assertEqual(_snapshot(root), before)
            self.assertEqual(list(root.rglob("*.publish.lock")), [])

    def test_legacy_backups_block_publication_without_adoption(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            destination, digest, first, first_digest, _second, _second_digest = (
                _publication_inputs(root)
            )
            legacy = destination.with_name(".release.previous")
            legacy.mkdir()
            (legacy / "marker").write_bytes(b"foreign")
            before = _snapshot(root)
            with self.assertRaisesRegex(common.ScriptError, "stale release backup"):
                release._publish_release_directory(
                    first, destination, (first_digest, digest)
                )
            self.assertEqual(_snapshot(root), before)


if __name__ == "__main__":
    unittest.main()
