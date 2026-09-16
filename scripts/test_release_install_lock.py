#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

from __future__ import annotations

import errno
import multiprocessing
import stat
import sys
import tempfile
import unittest
from collections.abc import Generator
from contextlib import contextmanager
from itertools import product
from multiprocessing.process import BaseProcess
from multiprocessing.synchronize import Event
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import common, release  # noqa: E402

PROCESS_TIMEOUT_SECONDS = 30
PROMOTION_FAILURE = "injected promotion failure"
INSTALL_PHASES = ("recovery", "staging", "promotion", "commit", "rollback", "cleanup")


def _runtime_inputs(root: Path) -> tuple[Path, Path, Path]:
    runtime = root / "runtime"
    runtime.mkdir()
    current = runtime / "current"
    current.write_bytes(b"prior")
    current.chmod(0o700)
    (runtime / "removed").write_bytes(b"retired")
    first = root / "first-source"
    first.write_bytes(b"first")
    second = root / "second-source"
    second.write_bytes(b"second")
    return runtime, first, second


def _runtime_snapshot(
    root: Path, *, lock_suffix: str = ".install.lock"
) -> dict[str, tuple[bytes, int] | None]:
    return {
        str(path.relative_to(root)): (
            (path.read_bytes(), stat.S_IMODE(path.stat().st_mode))
            if path.is_file() and not path.name.endswith(lock_suffix)
            else None
        )
        for path in root.rglob("*")
    }


def _paused_install_worker(
    runtime: Path,
    source: Path,
    phase: str,
    paused: Event,
    resume: Event,
) -> None:
    original_recover = release._recover_install_transactions
    original_copy = release.shutil.copyfile
    original_replace = release._atomic_replace
    original_journal = release._write_transaction_journal
    original_rollback = release._rollback_transaction
    original_cleanup = release.shutil.rmtree

    def pause() -> None:
        paused.set()
        if not resume.wait(PROCESS_TIMEOUT_SECONDS):
            raise TimeoutError(f"test installer was not resumed during {phase}")

    def recover(parent: Path) -> None:
        if phase == "recovery":
            pause()
        original_recover(parent)

    def copy(source: Path, destination: Path) -> Path:
        if phase == "staging":
            pause()
        return original_copy(source, destination)

    def replace(source: Path, destination: Path) -> None:
        if source.parent.name == "staged":
            if phase == "rollback":
                raise OSError(PROMOTION_FAILURE)
            if phase == "promotion":
                pause()
        original_replace(source, destination)

    def write_journal(transaction: Path, journal: dict[str, object]) -> None:
        if phase == "commit" and journal["phase"] == "committed":
            pause()
        original_journal(transaction, journal)

    def rollback(transaction: Path, journal: dict[str, object]) -> list[str]:
        if phase == "rollback":
            pause()
        return original_rollback(transaction, journal)

    def cleanup(transaction: Path) -> None:
        if phase == "cleanup":
            pause()
        original_cleanup(transaction)

    def artifact_path(name: str) -> Path:
        return runtime / name

    with (
        patch.object(release, "artifact_path", side_effect=artifact_path),
        patch.object(release, "_recover_install_transactions", side_effect=recover),
        patch.object(release.shutil, "copyfile", side_effect=copy),
        patch.object(release, "_atomic_replace", side_effect=replace),
        patch.object(release, "_write_transaction_journal", side_effect=write_journal),
        patch.object(release, "_rollback_transaction", side_effect=rollback),
        patch.object(release.shutil, "rmtree", side_effect=cleanup),
    ):
        try:
            release._transactional_install(
                [release._InstallFile(source, runtime / "current", 0o644)],
                [runtime / "removed"],
            )
        except OSError as error:
            if phase != "rollback" or str(error) != PROMOTION_FAILURE:
                raise
        else:
            if phase == "rollback":
                raise AssertionError("the injected promotion failure did not occur")


class RuntimeInstallLockTests(unittest.TestCase):
    @contextmanager
    def paused_installer(
        self, runtime: Path, source: Path, phase: str
    ) -> Generator[tuple[BaseProcess, Event], None, None]:
        context = multiprocessing.get_context("spawn")
        paused = context.Event()
        resume = context.Event()
        process = context.Process(
            target=_paused_install_worker,
            args=(runtime, source, phase, paused, resume),
        )
        process.start()
        try:
            self.assertTrue(
                paused.wait(PROCESS_TIMEOUT_SECONDS),
                f"installer did not reach {phase}; exit code: {process.exitcode}",
            )
            yield process, resume
        finally:
            resume.set()
            process.join(PROCESS_TIMEOUT_SECONDS)
            if process.is_alive():
                process.terminate()
                process.join(PROCESS_TIMEOUT_SECONDS)
            self.assertFalse(process.is_alive(), "test installer did not exit")
            exitcode = process.exitcode
            process.close()
            self.assertEqual(exitcode, 0, f"installer failed during {phase}")

    def test_competing_install_cannot_touch_any_active_transaction_phase(self):
        for phase in INSTALL_PHASES:
            with (
                self.subTest(phase=phase),
                tempfile.TemporaryDirectory() as temporary,
            ):
                runtime, first, second = _runtime_inputs(Path(temporary))

                def artifact_path(name: str, directory: Path = runtime) -> Path:
                    return directory / name

                with self.paused_installer(runtime, first, phase):
                    before = _runtime_snapshot(runtime)
                    with (
                        patch.object(
                            release, "artifact_path", side_effect=artifact_path
                        ),
                        patch.object(release, "LOCK_TIMEOUT_SECONDS", 0),
                        self.assertRaisesRegex(
                            common.ScriptError,
                            "timed out waiting for runtime install lock",
                        ),
                    ):
                        release._transactional_install(
                            [release._InstallFile(second, runtime / "current", 0o644)],
                            [runtime / "removed"],
                        )
                    self.assertEqual(_runtime_snapshot(runtime), before)

                self.assertEqual(
                    (runtime / "current").read_bytes(),
                    b"prior" if phase == "rollback" else b"first",
                )
                self.assertEqual((runtime / "removed").exists(), phase == "rollback")
                with patch.object(release, "artifact_path", side_effect=artifact_path):
                    release._transactional_install(
                        [release._InstallFile(second, runtime / "current", 0o644)],
                        [runtime / "removed"],
                    )
                self.assertEqual((runtime / "current").read_bytes(), b"second")
                self.assertEqual(
                    list((runtime / ".install-transactions").iterdir()), []
                )
                self.assertTrue((runtime / ".install.lock").is_file())

    def test_waiting_installer_proceeds_after_lock_owner_finishes(self):
        with tempfile.TemporaryDirectory() as temporary:
            runtime, first, second = _runtime_inputs(Path(temporary))

            def artifact_path(name: str) -> Path:
                return runtime / name

            with self.paused_installer(runtime, first, "staging") as (process, resume):

                def release_owner(_delay: float) -> None:
                    resume.set()
                    process.join(PROCESS_TIMEOUT_SECONDS)
                    self.assertFalse(process.is_alive())

                with (
                    patch.object(release, "artifact_path", side_effect=artifact_path),
                    patch.object(
                        release.time, "sleep", side_effect=release_owner
                    ) as wait,
                ):
                    release._transactional_install(
                        [release._InstallFile(second, runtime / "current", 0o644)], []
                    )
                wait.assert_called_once()
            self.assertEqual((runtime / "current").read_bytes(), b"second")

    def test_different_runtime_directories_do_not_share_a_lock(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            runtime, first, second = _runtime_inputs(root)
            other_runtime = root / "other-runtime"

            def artifact_path(name: str) -> Path:
                return other_runtime / name

            with (
                self.paused_installer(runtime, first, "staging"),
                patch.object(release, "artifact_path", side_effect=artifact_path),
                patch.object(release, "LOCK_TIMEOUT_SECONDS", 0),
            ):
                release._transactional_install(
                    [release._InstallFile(second, other_runtime / "current", 0o644)], []
                )
                self.assertEqual((other_runtime / "current").read_bytes(), b"second")

    def test_lock_wait_is_bounded(self):
        with tempfile.TemporaryDirectory() as temporary:
            lock_path = Path(temporary) / ".install.lock"
            lock_api = (
                "nvx_tools.release.msvcrt.locking"
                if sys.platform == "win32"
                else "nvx_tools.release.fcntl.flock"
            )
            with (
                patch.object(release, "artifact_path", return_value=lock_path),
                patch(
                    lock_api,
                    side_effect=BlockingIOError(errno.EACCES, "lock is held"),
                ),
                patch.object(
                    release.time,
                    "monotonic",
                    side_effect=(0.0, 0.0, release.LOCK_TIMEOUT_SECONDS),
                ),
                patch.object(release.time, "sleep") as wait,
                self.assertRaisesRegex(common.ScriptError, "timed out waiting"),
                release._runtime_install_lock(),
            ):
                self.fail("the contended lock was acquired")
            wait.assert_called_once_with(release.LOCK_POLL_INTERVAL_SECONDS)

    def test_unexpected_lock_error_is_visible_without_retry(self):
        with tempfile.TemporaryDirectory() as temporary:
            lock_path = Path(temporary) / ".install.lock"
            lock_api = (
                "nvx_tools.release.msvcrt.locking"
                if sys.platform == "win32"
                else "nvx_tools.release.fcntl.flock"
            )
            with (
                patch.object(release, "artifact_path", return_value=lock_path),
                patch(lock_api, side_effect=OSError(errno.EIO, "lock I/O failure")),
                patch.object(release.time, "sleep") as wait,
                self.assertRaisesRegex(
                    common.ScriptError, "cannot acquire runtime install lock"
                ),
                release._runtime_install_lock(),
            ):
                self.fail("the failed lock was acquired")
            wait.assert_not_called()

    def test_commit_journal_failure_rolls_back_all_runtime_changes(self):
        original_journal = release._write_transaction_journal
        for exception_type, publish_commit in product(
            (OSError, KeyboardInterrupt, SystemExit), (False, True)
        ):
            with (
                self.subTest(
                    exception=exception_type.__name__, publish_commit=publish_commit
                ),
                tempfile.TemporaryDirectory() as temporary,
            ):
                runtime, first, second = _runtime_inputs(Path(temporary))
                prior = _runtime_snapshot(runtime)
                phases: list[str] = []

                def artifact_path(name: str, directory: Path = runtime) -> Path:
                    return directory / name

                def fail_commit(
                    transaction: Path,
                    journal: dict[str, object],
                    raised_exception: type[BaseException] = exception_type,
                    recorded_phases: list[str] = phases,
                    published: bool = publish_commit,
                ) -> None:
                    recorded_phases.append(str(journal["phase"]))
                    if journal["phase"] == "committed":
                        if published:
                            original_journal(transaction, journal)
                        else:
                            (transaction / "journal.json.part").write_bytes(b"partial")
                        raise raised_exception("injected commit journal failure")
                    original_journal(transaction, journal)

                with (
                    patch.object(release, "artifact_path", side_effect=artifact_path),
                    patch.object(
                        release, "_write_transaction_journal", side_effect=fail_commit
                    ),
                    self.assertRaisesRegex(exception_type, "commit journal failure"),
                ):
                    release._transactional_install(
                        [
                            release._InstallFile(first, runtime / "current", 0o644),
                            release._InstallFile(second, runtime / "new", 0o644),
                        ],
                        [runtime / "removed"],
                    )
                after = _runtime_snapshot(runtime)
                self.assertEqual({name: after[name] for name in prior}, prior)
                self.assertFalse((runtime / "new").exists())
                self.assertEqual(
                    list((runtime / ".install-transactions").iterdir()), []
                )
                self.assertEqual(
                    phases, ["active", "committed", "rolling-back", "rolled-back"]
                )

    def test_failed_rollback_state_preserves_runtime_and_backups_for_recovery(self):
        original_journal = release._write_transaction_journal
        for exception_type, publish_rollback in product(
            (OSError, KeyboardInterrupt, SystemExit), (False, True)
        ):
            with (
                self.subTest(
                    exception=exception_type.__name__, publish_rollback=publish_rollback
                ),
                tempfile.TemporaryDirectory() as temporary,
            ):
                runtime, first, second = _runtime_inputs(Path(temporary))
                prior = _runtime_snapshot(runtime)

                def artifact_path(name: str, directory: Path = runtime) -> Path:
                    return directory / name

                def fail_journal(
                    transaction: Path,
                    journal: dict[str, object],
                    raised_exception: type[BaseException] = exception_type,
                    published: bool = publish_rollback,
                ) -> None:
                    if journal["phase"] == "committed":
                        original_journal(transaction, journal)
                        raise KeyboardInterrupt("interrupted committed write")
                    if journal["phase"] == "rolling-back":
                        if published:
                            original_journal(transaction, journal)
                        else:
                            (transaction / "journal.json.part").write_bytes(b"partial")
                        raise raised_exception("injected rollback state failure")
                    original_journal(transaction, journal)

                with (
                    patch.object(release, "artifact_path", side_effect=artifact_path),
                    patch.object(
                        release, "_write_transaction_journal", side_effect=fail_journal
                    ),
                    patch.object(release, "_remove_runtime_path") as undo,
                    self.assertRaisesRegex(
                        common.ScriptError, "cannot persist rollback state"
                    ),
                ):
                    release._transactional_install(
                        [
                            release._InstallFile(first, runtime / "current", 0o644),
                            release._InstallFile(second, runtime / "new", 0o644),
                        ],
                        [runtime / "removed"],
                    )
                undo.assert_not_called()
                self.assertEqual((runtime / "current").read_bytes(), b"first")
                self.assertEqual((runtime / "new").read_bytes(), b"second")
                self.assertFalse((runtime / "removed").exists())
                transaction_parent = runtime / ".install-transactions"
                transactions = list(transaction_parent.iterdir())
                self.assertEqual(len(transactions), 1)
                transaction = transactions[0]
                self.assertEqual(
                    release._read_transaction_journal(transaction)["phase"],
                    "rolling-back" if publish_rollback else "committed",
                )
                self.assertEqual(
                    _runtime_snapshot(transaction / "backup"),
                    {"file-0": prior["current"], "removed-0": prior["removed"]},
                )

                with patch.object(
                    release,
                    "_rollback_transaction",
                    wraps=release._rollback_transaction,
                ) as rollback:
                    release._recover_install_transactions(transaction_parent)
                if publish_rollback:
                    rollback.assert_called_once()
                    after = _runtime_snapshot(runtime)
                    self.assertEqual({name: after[name] for name in prior}, prior)
                    self.assertFalse((runtime / "new").exists())
                else:
                    rollback.assert_not_called()
                    self.assertEqual((runtime / "current").read_bytes(), b"first")
                    self.assertEqual((runtime / "new").read_bytes(), b"second")
                    self.assertFalse((runtime / "removed").exists())
                self.assertEqual(list(transaction_parent.iterdir()), [])

    def test_partial_rollback_after_committed_write_is_recoverable(self):
        original_journal = release._write_transaction_journal
        original_replace = release._atomic_replace
        for exception_type, publish_restore in product(
            (OSError, KeyboardInterrupt, SystemExit), (False, True)
        ):
            with (
                self.subTest(
                    exception=exception_type.__name__, publish_restore=publish_restore
                ),
                tempfile.TemporaryDirectory() as temporary,
            ):
                runtime, first, second = _runtime_inputs(Path(temporary))
                prior = _runtime_snapshot(runtime)

                def artifact_path(name: str, directory: Path = runtime) -> Path:
                    return directory / name

                def fail_commit(transaction: Path, journal: dict[str, object]) -> None:
                    original_journal(transaction, journal)
                    if journal["phase"] == "committed":
                        raise KeyboardInterrupt("interrupted committed write")

                def fail_restore(
                    source: Path,
                    destination: Path,
                    raised_exception: type[BaseException] = exception_type,
                    published: bool = publish_restore,
                ) -> None:
                    if source.parent.name == "backup":
                        self.assertEqual(
                            release._read_transaction_journal(source.parent.parent)[
                                "phase"
                            ],
                            "rolling-back",
                        )
                        if source.name == "file-0":
                            if published:
                                original_replace(source, destination)
                            raise raised_exception("interrupted backup restore")
                    original_replace(source, destination)

                with (
                    patch.object(release, "artifact_path", side_effect=artifact_path),
                    patch.object(
                        release, "_write_transaction_journal", side_effect=fail_commit
                    ),
                    patch.object(release, "_atomic_replace", side_effect=fail_restore),
                    self.assertRaisesRegex(
                        common.ScriptError, "journal and backups were preserved"
                    ),
                ):
                    release._transactional_install(
                        [
                            release._InstallFile(first, runtime / "current", 0o644),
                            release._InstallFile(second, runtime / "new", 0o644),
                        ],
                        [runtime / "removed"],
                    )
                transaction_parent = runtime / ".install-transactions"
                transactions = list(transaction_parent.iterdir())
                self.assertEqual(len(transactions), 1)
                transaction = transactions[0]
                self.assertEqual(
                    release._read_transaction_journal(transaction)["phase"],
                    "rolling-back",
                )
                self.assertEqual((runtime / "removed").read_bytes(), b"retired")
                self.assertFalse((runtime / "new").exists())
                self.assertEqual((runtime / "current").exists(), publish_restore)
                self.assertEqual(
                    (transaction / "backup" / "file-0").exists(), not publish_restore
                )

                release._recover_install_transactions(transaction_parent)
                release._recover_install_transactions(transaction_parent)
                after = _runtime_snapshot(runtime)
                self.assertEqual({name: after[name] for name in prior}, prior)
                self.assertFalse((runtime / "new").exists())
                self.assertEqual(list(transaction_parent.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
