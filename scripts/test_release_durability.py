#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

from __future__ import annotations

import errno
import json
import os
import stat
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import common, release  # noqa: E402
from test_release_install_lock import (  # noqa: E402
    _runtime_inputs,
    _runtime_snapshot,
)


def _install(runtime: Path, first: Path, second: Path) -> None:
    def artifact_path(name: str) -> Path:
        return runtime / name

    with patch.object(release, "artifact_path", side_effect=artifact_path):
        release._transactional_install(
            [
                release._InstallFile(first, runtime / "current", 0o644),
                release._InstallFile(second, runtime / "nested" / "new", 0o644),
            ],
            [runtime / "removed"],
        )


class DurableFlushTests(unittest.TestCase):
    def test_file_flush_precedes_fsync_on_linux_and_windows(self):
        events: list[str] = []

        def flush() -> None:
            events.append("flush")

        def sync(_descriptor: int) -> None:
            events.append("fsync")

        for platform, mode in (("linux", "rb"), ("win32", "r+b")):
            with (
                self.subTest(platform=platform),
                patch.object(release.sys, "platform", platform),
                patch.object(Path, "open") as open_file,
                patch.object(release.os, "fsync") as fsync,
            ):
                events.clear()
                contents = open_file.return_value.__enter__.return_value
                contents.fileno.return_value = 123
                contents.flush.side_effect = flush
                fsync.side_effect = sync
                release._sync_file(Path("staged"))
                open_file.assert_called_once_with(mode)
                fsync.assert_called_once_with(123)
                self.assertEqual(events, ["flush", "fsync"])
                open_file.return_value.__exit__.assert_called_once()

    def test_file_fsync_errors_are_never_ignored(self):
        for error_number in (errno.EINVAL, errno.ENOTSUP, errno.EIO, errno.EACCES):
            with (
                self.subTest(errno=error_number),
                tempfile.TemporaryDirectory() as temporary,
            ):
                path = Path(temporary) / "file"
                path.write_bytes(b"contents")
                failure = OSError(error_number, "file flush failure")
                with (
                    patch.object(release.os, "fsync", side_effect=failure),
                    self.assertRaises(OSError) as raised,
                ):
                    release._sync_file(path)
                self.assertIs(raised.exception, failure)
                path.unlink()

    def test_directory_fsync_handles_only_known_unsupported_errors(self):
        for error_number in (
            None,
            errno.EINVAL,
            errno.ENOTSUP,
            errno.EOPNOTSUPP,
            errno.EIO,
            errno.EBADF,
            errno.EACCES,
        ):
            with (
                self.subTest(errno=error_number),
                patch.object(release.os, "open", return_value=123) as open_directory,
                patch.object(release.os, "fsync") as fsync,
                patch.object(release.os, "close") as close,
            ):
                if error_number is not None:
                    fsync.side_effect = OSError(error_number, "directory flush failure")
                if error_number in (errno.EIO, errno.EBADF, errno.EACCES):
                    with self.assertRaises(OSError):
                        release._sync_directory(Path("directory"))
                else:
                    release._sync_directory(Path("directory"))
                open_directory.assert_called_once_with(
                    Path("directory"),
                    os.O_RDONLY | getattr(os, "O_DIRECTORY", 0),
                )
                fsync.assert_called_once_with(123)
                close.assert_called_once_with(123)

    def test_directory_open_exception_is_specific_to_windows_directories(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            root = Path(directory.anchor)
            file = directory / "file"
            file.touch()
            for platform, path, error_number in (
                ("win32", directory, errno.EACCES),
                ("win32", file, errno.EACCES),
                ("win32", directory, errno.EIO),
                ("win32", directory, errno.EINVAL),
                ("win32", root, errno.ENOENT),
                ("win32", directory, errno.ENOENT),
                ("linux", directory, errno.EACCES),
                ("linux", directory, errno.EIO),
                ("linux", root, errno.ENOENT),
            ):
                with (
                    self.subTest(platform=platform, path=path, errno=error_number),
                    patch.object(release.sys, "platform", platform),
                    patch.object(
                        release.os,
                        "open",
                        side_effect=OSError(error_number, "directory open failure"),
                    ),
                    patch.object(release.os, "fsync") as fsync,
                    patch.object(release.os, "close") as close,
                ):
                    if platform == "win32" and (
                        (path == directory and error_number == errno.EACCES)
                        or (path == root and error_number == errno.ENOENT)
                    ):
                        release._sync_directory(path)
                    else:
                        with self.assertRaises(OSError):
                            release._sync_directory(path)
                    fsync.assert_not_called()
                    close.assert_not_called()

    def test_mkdir_flushes_existing_and_new_ancestors(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "one" / "two"
            for _ in range(2):
                with patch.object(release, "_sync_directory") as sync:
                    release._make_directory(path)
                self.assertTrue(path.is_dir())
                self.assertEqual(
                    [call.args[0] for call in sync.call_args_list],
                    [path, *path.parents],
                )

    def test_replace_flushes_destination_then_source_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            other = root / "other"
            other.mkdir()
            events: list[Path] = []
            for parent in (root, other):
                source.write_bytes(b"durable")
                destination = parent / "destination"
                events.clear()

                def sync(path: Path, target: Path = destination) -> None:
                    self.assertFalse(source.exists())
                    self.assertEqual(target.read_bytes(), b"durable")
                    events.append(path)

                with patch.object(release, "_sync_directory", side_effect=sync):
                    release._atomic_replace(source, destination)
                self.assertEqual(events, [parent] if parent == root else [parent, root])

    def test_every_journal_transition_flushes_file_before_replace_and_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            transaction = Path(temporary)
            original_replace = Path.replace
            original_file_sync = release._sync_file
            original_directory_sync = release._sync_directory
            events: list[str] = []
            for phase in ("active", "rolling-back", "committed", "rolled-back"):
                events.clear()

                def sync_file(path: Path) -> None:
                    self.assertEqual(path, transaction / "journal.json.part")
                    original_file_sync(path)
                    events.append("file")

                def replace(source: Path, destination: Path) -> Path:
                    self.assertEqual(events, ["file"])
                    self.assertEqual(destination, transaction / "journal.json")
                    result = original_replace(source, destination)
                    events.append("replace")
                    return result

                def sync_directory(path: Path) -> None:
                    self.assertEqual(events, ["file", "replace"])
                    self.assertEqual(path, transaction)
                    original_directory_sync(path)
                    events.append("directory")

                with (
                    patch.object(release, "_sync_file", side_effect=sync_file),
                    patch.object(
                        release, "_sync_directory", side_effect=sync_directory
                    ),
                    patch.object(Path, "replace", autospec=True, side_effect=replace),
                ):
                    release._write_transaction_journal(
                        transaction, {"format": 1, "phase": phase, "operations": []}
                    )
                self.assertEqual(events, ["file", "replace", "directory"])
                self.assertEqual(
                    release._read_transaction_journal(transaction)["phase"], phase
                )


class RuntimeDurabilityTests(unittest.TestCase):
    def test_install_flush_order_precedes_promotion_commit_and_backup_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            runtime, first, second = _runtime_inputs(Path(temporary))
            events: list[tuple[str, Path, Path | None]] = []
            descriptor_modes: list[int] = []
            original_file_sync = release._sync_file
            original_directory_sync = release._sync_directory
            original_replace = Path.replace
            original_cleanup = release.shutil.rmtree
            original_fsync = os.fsync

            def fsync(descriptor: int) -> None:
                descriptor_modes.append(stat.S_IFMT(os.fstat(descriptor).st_mode))
                original_fsync(descriptor)

            def sync_file(path: Path) -> None:
                original_file_sync(path)
                events.append(("file", path, None))

            def sync_directory(path: Path) -> None:
                original_directory_sync(path)
                events.append(("directory", path, None))

            def replace(source: Path, destination: Path) -> Path:
                result = original_replace(source, destination)
                events.append(("replace", source, destination))
                return result

            def cleanup(path: Path) -> None:
                self.assertEqual(
                    release._read_transaction_journal(path)["phase"], "committed"
                )
                self.assertEqual((path / "backup" / "file-0").read_bytes(), b"prior")
                self.assertEqual(
                    (path / "backup" / "removed-0").read_bytes(), b"retired"
                )
                events.append(("cleanup", path, None))
                original_cleanup(path)

            with (
                patch.object(release.os, "fsync", side_effect=fsync),
                patch.object(release, "_sync_file", side_effect=sync_file),
                patch.object(release, "_sync_directory", side_effect=sync_directory),
                patch.object(Path, "replace", autospec=True, side_effect=replace),
                patch.object(release.shutil, "rmtree", side_effect=cleanup),
            ):
                _install(runtime, first, second)

            transaction = next(
                path for action, path, _ in events if action == "cleanup"
            )
            journal_replaces = [
                index
                for index, (action, _, destination) in enumerate(events)
                if action == "replace" and destination == transaction / "journal.json"
            ]
            self.assertEqual(len(journal_replaces), 2)
            active, committed = journal_replaces
            for index in (active, committed):
                self.assertEqual(
                    events[index - 1],
                    ("file", transaction / "journal.json.part", None),
                )
                self.assertEqual(events[index + 1], ("directory", transaction, None))
            for index in range(2):
                self.assertLess(
                    events.index(("file", transaction / "staged" / str(index), None)),
                    active,
                )
            self.assertLess(
                events.index(("directory", transaction / "staged", None)), active
            )
            self.assertLess(
                events.index(("directory", transaction.parent, None)), active
            )
            for index, (action, source, destination) in enumerate(events):
                if action != "replace" or source.name == "journal.json.part":
                    continue
                assert destination is not None
                self.assertGreater(index, active + 1)
                self.assertLess(index, committed - 1)
                self.assertEqual(
                    events[index + 1], ("directory", destination.parent, None)
                )
                self.assertEqual(events[index + 2], ("directory", source.parent, None))
            cleanup_index = events.index(("cleanup", transaction, None))
            self.assertGreater(cleanup_index, committed + 1)
            self.assertEqual(
                events[cleanup_index + 1], ("directory", transaction.parent, None)
            )
            self.assertEqual(descriptor_modes.count(stat.S_IFREG), 4)
            if sys.platform != "win32":
                self.assertEqual(
                    descriptor_modes.count(stat.S_IFDIR),
                    sum(action == "directory" for action, _, _ in events),
                )

    def test_sync_failures_before_durable_commit_roll_back(self):
        for point in (
            "staged-file",
            "staged-directory",
            "active-file",
            "active-directory",
            "backup-destination",
            "backup-source",
            "promotion-destination",
            "promotion-source",
            "removal-destination",
            "removal-source",
            "committed-file",
            "committed-directory",
        ):
            with self.subTest(point=point):
                self.check_install_sync_failure(point)

    def check_install_sync_failure(self, point: str) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            runtime, first, second = _runtime_inputs(Path(temporary))
            prior = _runtime_snapshot(runtime)
            original_file_sync = release._sync_file
            original_directory_sync = release._sync_directory
            original_replace = release._atomic_replace
            original_journal = release._write_transaction_journal
            operation = ""
            target_parent: Path | None = None
            failed = False

            def fail(label: str) -> None:
                nonlocal failed
                if label == point and not failed:
                    failed = True
                    raise OSError(errno.EIO, f"injected fsync failure: {point}")

            def sync_file(path: Path) -> None:
                fail(
                    "staged-file"
                    if path.parent.name == "staged"
                    else f"{operation}-file"
                )
                original_file_sync(path)

            def sync_directory(path: Path) -> None:
                if operation in ("backup", "promotion", "removal"):
                    suffix = "destination" if path == target_parent else "source"
                    fail(f"{operation}-{suffix}")
                elif operation:
                    fail(f"{operation}-directory")
                elif path.name == "staged":
                    fail("staged-directory")
                original_directory_sync(path)

            def replace(source: Path, destination: Path) -> None:
                nonlocal operation, target_parent
                previous = operation
                target_parent = destination.parent
                operation = (
                    "promotion"
                    if source.parent.name == "staged"
                    else "removal"
                    if destination.name.startswith("removed-")
                    else "backup"
                )
                try:
                    original_replace(source, destination)
                finally:
                    operation = previous

            def journal(transaction: Path, value: dict[str, object]) -> None:
                nonlocal operation
                previous = operation
                operation = str(value["phase"])
                try:
                    original_journal(transaction, value)
                finally:
                    operation = previous

            with (
                patch.object(release, "_sync_file", side_effect=sync_file),
                patch.object(release, "_sync_directory", side_effect=sync_directory),
                patch.object(release, "_atomic_replace", side_effect=replace),
                patch.object(
                    release, "_write_transaction_journal", side_effect=journal
                ),
                self.assertRaisesRegex(OSError, f"fsync failure: {point}"),
            ):
                _install(runtime, first, second)
            self.assertTrue(failed)
            after = _runtime_snapshot(runtime)
            self.assertEqual({name: after[name] for name in prior}, prior)
            self.assertFalse((runtime / "nested" / "new").exists())
            self.assertEqual(list((runtime / ".install-transactions").iterdir()), [])

    def test_rollback_state_flush_failures_remain_recoverable(self):
        for phase in ("rolling-back", "rolled-back"):
            for kind in ("file", "directory"):
                with self.subTest(phase=phase, kind=kind):
                    self.check_rollback_state_failure(phase, kind)

    def check_rollback_state_failure(self, phase: str, kind: str) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            runtime, first, second = _runtime_inputs(Path(temporary))
            prior = _runtime_snapshot(runtime)
            original_journal = release._write_transaction_journal
            original_sync = (
                release._sync_file if kind == "file" else release._sync_directory
            )
            failed = False

            def sync(path: Path) -> None:
                nonlocal failed
                journal_path = path if kind == "file" else path / "journal.json"
                if (
                    journal_path.name in ("journal.json", "journal.json.part")
                    and journal_path.is_file()
                    and json.loads(journal_path.read_text(encoding="utf-8"))["phase"]
                    == phase
                    and not failed
                ):
                    failed = True
                    raise OSError(errno.EIO, f"injected {phase} {kind} flush failure")
                original_sync(path)

            def journal(transaction: Path, value: dict[str, object]) -> None:
                original_journal(transaction, value)
                if value["phase"] == "committed":
                    raise OSError("injected commit interruption")

            with (
                patch.object(
                    release, "_write_transaction_journal", side_effect=journal
                ),
                patch.object(release, f"_sync_{kind}", side_effect=sync),
                self.assertRaisesRegex(
                    common.ScriptError, "journal and backups were preserved"
                ),
            ):
                _install(runtime, first, second)
            self.assertTrue(failed)
            parent = runtime / ".install-transactions"
            (transaction,) = parent.iterdir()
            keep_commit = phase == "rolling-back" and kind == "file"
            if phase == "rolling-back":
                self.assertEqual((runtime / "current").read_bytes(), b"first")
                self.assertEqual(
                    (transaction / "backup" / "file-0").read_bytes(), b"prior"
                )
                self.assertEqual((runtime / "nested" / "new").read_bytes(), b"second")
                self.assertFalse((runtime / "removed").exists())
            release._recover_install_transactions(parent)
            release._recover_install_transactions(parent)
            if keep_commit:
                self.assertEqual((runtime / "current").read_bytes(), b"first")
                self.assertEqual((runtime / "nested" / "new").read_bytes(), b"second")
                self.assertFalse((runtime / "removed").exists())
            else:
                after = _runtime_snapshot(runtime)
                self.assertEqual({name: after[name] for name in prior}, prior)
                self.assertFalse((runtime / "nested" / "new").exists())
            self.assertEqual(list(parent.iterdir()), [])

    def test_recovery_retries_directory_flush_after_restore_or_unlink_completed(self):
        for prior_exists in (False, True):
            with self.subTest(prior_exists=prior_exists):
                self.check_rollback_directory_retry(prior_exists)

    def check_rollback_directory_retry(self, prior_exists: bool) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            runtime, first, _second = _runtime_inputs(Path(temporary))
            destination = runtime / "current"
            if not prior_exists:
                destination.unlink()
            parent = runtime / ".install-transactions"
            original_journal = release._write_transaction_journal
            original_sync = release._sync_directory
            rollback_started = False
            failed = False

            def sync(path: Path) -> None:
                nonlocal failed
                restored = (
                    destination.is_file() and destination.read_bytes() == b"prior"
                    if prior_exists
                    else not destination.exists()
                )
                if path == runtime and rollback_started and restored and not failed:
                    failed = True
                    raise OSError(errno.EIO, "restore directory flush failed")
                original_sync(path)

            def journal(transaction: Path, value: dict[str, object]) -> None:
                nonlocal rollback_started
                original_journal(transaction, value)
                if value["phase"] == "committed":
                    raise OSError("injected commit interruption")
                if value["phase"] == "rolling-back":
                    rollback_started = True

            def artifact_path(name: str) -> Path:
                return runtime / name

            with (
                patch.object(release, "artifact_path", side_effect=artifact_path),
                patch.object(release, "_sync_directory", side_effect=sync),
                patch.object(
                    release, "_write_transaction_journal", side_effect=journal
                ),
                self.assertRaisesRegex(
                    common.ScriptError, "restore directory flush failed"
                ),
            ):
                release._transactional_install(
                    [release._InstallFile(first, destination, 0o644)], []
                )
            self.assertTrue(failed)
            (transaction,) = parent.iterdir()
            self.assertFalse((transaction / "backup" / "file-0").exists())
            self.assertEqual(
                release._read_transaction_journal(transaction)["phase"], "rolling-back"
            )

            failed = False
            with (
                patch.object(release, "_sync_directory", side_effect=sync),
                self.assertRaisesRegex(
                    common.ScriptError, "restore directory flush failed"
                ),
            ):
                release._recover_install_transactions(parent)
            self.assertTrue(failed)
            self.assertEqual(
                release._read_transaction_journal(transaction)["phase"], "rolling-back"
            )
            release._recover_install_transactions(parent)
            release._recover_install_transactions(parent)
            self.assertEqual(destination.exists(), prior_exists)
            if prior_exists:
                self.assertEqual(destination.read_bytes(), b"prior")
            self.assertEqual(list(parent.iterdir()), [])

    def test_committed_recovery_flushes_destinations_and_journal_before_cleanup(self):
        for point in (
            "destination-file",
            "destination-directory",
            "journal-file",
            "journal-directory",
        ):
            with self.subTest(point=point):
                self.check_committed_recovery_failure(point)

    def check_committed_recovery_failure(self, point: str) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            runtime, first, second = _runtime_inputs(Path(temporary))
            with (
                patch.object(
                    release.shutil, "rmtree", side_effect=OSError("cleanup interrupted")
                ),
                self.assertRaisesRegex(OSError, "cleanup interrupted"),
            ):
                _install(runtime, first, second)
            parent = runtime / ".install-transactions"
            (transaction,) = parent.iterdir()
            before = _runtime_snapshot(runtime)
            target = {
                "destination-file": runtime / "current",
                "destination-directory": runtime,
                "journal-file": transaction / "journal.json",
                "journal-directory": transaction,
            }[point]
            kind = point.split("-")[1]
            original_sync = (
                release._sync_file if kind == "file" else release._sync_directory
            )

            def sync(path: Path) -> None:
                if path == target:
                    raise OSError(errno.EIO, f"recovery flush failed: {point}")
                original_sync(path)

            with (
                patch.object(release, f"_sync_{kind}", side_effect=sync),
                patch.object(release, "_remove_runtime_path") as cleanup,
                self.assertRaisesRegex(OSError, f"recovery flush failed: {point}"),
            ):
                release._recover_install_transactions(parent)
            cleanup.assert_not_called()
            self.assertEqual(_runtime_snapshot(runtime), before)
            release._recover_install_transactions(parent)
            self.assertEqual((runtime / "current").read_bytes(), b"first")
            self.assertEqual((runtime / "nested" / "new").read_bytes(), b"second")
            self.assertFalse((runtime / "removed").exists())
            self.assertEqual(list(parent.iterdir()), [])

    def test_cleanup_directory_failure_does_not_undo_durable_commit(self):
        with tempfile.TemporaryDirectory() as temporary:
            runtime, first, second = _runtime_inputs(Path(temporary))
            parent = runtime / ".install-transactions"
            original_journal = release._write_transaction_journal
            original_sync = release._sync_directory
            committed = False

            def journal(transaction: Path, value: dict[str, object]) -> None:
                nonlocal committed
                original_journal(transaction, value)
                committed = value["phase"] == "committed"

            def sync(path: Path) -> None:
                if path == parent and committed:
                    self.assertEqual(list(parent.iterdir()), [])
                    raise OSError(errno.EIO, "cleanup directory flush failed")
                original_sync(path)

            with (
                patch.object(release, "_sync_directory", side_effect=sync),
                patch.object(
                    release, "_write_transaction_journal", side_effect=journal
                ),
                self.assertRaisesRegex(OSError, "cleanup directory flush failed"),
            ):
                _install(runtime, first, second)
            release._recover_install_transactions(parent)
            self.assertEqual((runtime / "current").read_bytes(), b"first")
            self.assertEqual((runtime / "nested" / "new").read_bytes(), b"second")
            self.assertFalse((runtime / "removed").exists())
            self.assertEqual(list(parent.iterdir()), [])

    def test_missing_prior_destination_and_backup_preserves_recovery_state(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            parent = root / "transactions"
            transaction = parent / "transaction"
            (transaction / "backup").mkdir(parents=True)
            release._write_transaction_journal(
                transaction,
                {
                    "format": 1,
                    "phase": "active",
                    "operations": [
                        {
                            "kind": "replace",
                            "destination": str(root / "missing"),
                            "backup": "file-0",
                            "prior_exists": True,
                        }
                    ],
                },
            )
            with self.assertRaisesRegex(
                common.ScriptError, "destination and backup are missing"
            ):
                release._recover_install_transactions(parent)
            self.assertEqual(
                release._read_transaction_journal(transaction)["phase"], "rolling-back"
            )


if __name__ == "__main__":
    unittest.main()
