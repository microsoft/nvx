#!/usr/bin/env python3
"""Harmless regression tests: no Rust build, VM, model call, or guest execution."""
import hashlib
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

HARNESS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HARNESS))
import run as harness
from verify_cargo_tests import executed_tests


def summary(passed=0, ignored=0, filtered=0):
    return (f"test result: ok. {passed} passed; 0 failed; {ignored} ignored; "
            f"0 measured; {filtered} filtered out; finished in 0.00s\n")


MATCHING = "test microvm::tests::snapshot_requests_are_coalesced_until_acknowledged ... ok\n"


def cancellation_helper(root):
    """Run production main with explicit mock fixtures and one sleeping Python child."""
    fixture = root / "mock-fixture"
    fixture.write_bytes(b"not a guest or VM binary\n")
    fixture.chmod(0o700)
    fixture_hash = hashlib.sha256(fixture.read_bytes()).hexdigest()
    real_run_process = harness.run_process

    def derive(_base, _script, destination):
        destination.write_bytes(b"mock initrd: cancellation regression only")
        return {"preserved_cpio_entries": 1, "replaced_entries": ["init"]}

    def harmless_child(label, _args, output, timeout, env, cancellation):
        descendant = (
            "import os,pathlib,signal,time;"
            "signal.signal(signal.SIGTERM,signal.SIG_IGN);"
            f"pathlib.Path({str(root / 'descendant.pid')!r}).write_text(str(os.getpid()));"
            "time.sleep(60)"
        )
        code = (
            "import os,pathlib,subprocess,sys,time;"
            f"subprocess.Popen([sys.executable,'-c',{descendant!r}]);"
            f"pathlib.Path({str(root / 'child.pid')!r}).write_text(str(os.getpid()));"
            "time.sleep(60)"
        )
        return real_run_process(label, [sys.executable, "-c", code], output,
                                timeout, env, cancellation)

    sys.argv = [
        str(HARNESS / "run.py"), "--source", str(root), "--binary", str(fixture),
        "--output", str(root / "failed-mock-runs"), "--kernel", str(fixture),
        "--initrd", str(fixture), "--kernel-sha256", fixture_hash,
        "--initrd-sha256", fixture_hash, "--process-timeout", "30", "--json",
    ]
    with (
        mock.patch.object(harness, "inspect", return_value={"failures": [], "mock_fixture": True}),
        mock.patch.object(harness, "command_text", return_value="a" * 40),
        mock.patch.object(harness.subprocess, "check_output", return_value=b""),
        mock.patch.object(harness, "derive_initrd", side_effect=derive),
        mock.patch.object(harness, "run_process", side_effect=harmless_child),
    ):
        return harness.main()


class PreservedFiles(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix=f"{self._testMethodName}-"))


class CancellationTests(PreservedFiles):
    def test_term_and_int_record_failed_receipt_and_reap_child(self):
        for signum in (signal.SIGTERM, signal.SIGINT):
            with self.subTest(signal=signum.name):
                root = self.root / signum.name
                root.mkdir()
                sentinel = subprocess.Popen(
                    [sys.executable, "-c", "import time; time.sleep(60)"],
                    start_new_session=True,
                )
                runner = subprocess.Popen(
                    [sys.executable, str(Path(__file__).resolve()), "--cancel-helper", str(root)],
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                    start_new_session=True,
                )
                child_fd = None
                descendant_fd = None
                try:
                    import time
                    deadline = time.monotonic() + 10
                    while not all((root / name).exists() for name in ("child.pid", "descendant.pid")):
                        self.assertIsNone(runner.poll(), "mock runner exited before child startup")
                        self.assertLess(time.monotonic(), deadline, "mock child did not start")
                        time.sleep(0.02)
                    child_pid = int((root / "child.pid").read_text())
                    child_fd = os.pidfd_open(child_pid)
                    descendant_fd = os.pidfd_open(int((root / "descendant.pid").read_text()))
                    runner.send_signal(signum)
                    stdout, stderr = runner.communicate(timeout=10)
                    (root / "runner.stdout").write_text(stdout)
                    (root / "runner.stderr").write_text(stderr)
                    self.assertEqual(runner.returncode, 1, stderr)
                    receipt = json.loads(stdout)
                    self.assertEqual(receipt["status"], "failed")
                    evidence = json.loads(Path(receipt["evidence"]).read_text())
                    self.assertTrue(evidence["interrupted"])
                    self.assertEqual(evidence["termination_signal"], signum.name)
                    self.assertIn("RunInterrupted", evidence["error"])
                    self.assertEqual(len(evidence["processes"]), 1)
                    child = evidence["processes"][0]
                    self.assertEqual(child["pid"], child_pid)
                    self.assertTrue(child["interrupted"])
                    self.assertFalse(child["timed_out"])
                    self.assertEqual(child["exit_code"], -signal.SIGKILL)
                    self.assertTrue(select.select([child_fd], [], [], 0)[0], "child survived")
                    self.assertTrue(select.select([descendant_fd], [], [], 1)[0], "descendant survived")
                    self.assertIsNone(sentinel.poll(), "unrelated process was signalled")
                finally:
                    for fd in (child_fd, descendant_fd):
                        if fd is not None:
                            if not select.select([fd], [], [], 0)[0]:
                                signal.pidfd_send_signal(fd, signal.SIGKILL)
                            os.close(fd)
                    for process in (runner, sentinel):
                        if process.poll() is None:
                            process.kill()
                        process.wait()

    def test_cancellation_during_popen_does_not_orphan_child(self):
        cancellation = harness.Cancellation()
        real_popen = subprocess.Popen

        def popen_then_cancel(*args, **kwargs):
            process = real_popen(*args, **kwargs)
            cancellation.request(signal.SIGTERM, None)
            return process

        with mock.patch.object(harness.subprocess, "Popen", side_effect=popen_then_cancel):
            result = harness.run_process(
                "startup-cancel", [sys.executable, "-c", "import time; time.sleep(60)"],
                self.root, 30, os.environ.copy(), cancellation,
            )
        self.assertTrue(result["interrupted"])
        self.assertEqual(result["exit_code"], -signal.SIGKILL)
        with self.assertRaises(ChildProcessError):
            os.waitpid(result["pid"], os.WNOHANG)

    def test_pending_cancellation_never_launches_a_child(self):
        cancellation = harness.Cancellation()
        cancellation.request(signal.SIGINT, None)
        with mock.patch.object(harness.subprocess, "Popen") as popen:
            with self.assertRaises(harness.RunInterrupted):
                harness.run_process("not-started", [], self.root, 30, {}, cancellation)
        popen.assert_not_called()

    def test_completed_process_is_never_signalled(self):
        with mock.patch.object(harness.os, "killpg") as killpg:
            result = harness.run_process(
                "completed", [sys.executable, "-c", "raise SystemExit(37)"],
                self.root, 5, os.environ.copy(), harness.Cancellation(),
            )
        self.assertEqual(result["exit_code"], 37)
        self.assertFalse(result["interrupted"])
        killpg.assert_not_called()

    def test_deadline_still_kills_and_reaps(self):
        result = harness.run_process(
            "deadline", [sys.executable, "-c", "import time; time.sleep(60)"],
            self.root, 0.2, os.environ.copy(), harness.Cancellation(),
        )
        self.assertTrue(result["timed_out"])
        self.assertFalse(result["interrupted"])
        self.assertEqual(result["exit_code"], -signal.SIGKILL)
        with self.assertRaises(ChildProcessError):
            os.waitpid(result["pid"], os.WNOHANG)

    def test_already_reaped_pid_is_not_used_as_a_group(self):
        process = mock.Mock()
        process.poll.return_value = 0
        with mock.patch.object(harness.os, "killpg") as killpg:
            harness.stop_process_group(process)
        killpg.assert_not_called()
        process.wait.assert_not_called()

    def test_group_exit_race_is_reaped(self):
        process = mock.Mock()
        process.poll.return_value = None
        with mock.patch.object(harness.os, "killpg", side_effect=ProcessLookupError):
            harness.stop_process_group(process)
        process.wait.assert_called_once_with()

    def test_cancellation_during_delay_and_handler_restoration(self):
        previous = {signum: signal.getsignal(signum) for signum in (signal.SIGTERM, signal.SIGINT)}
        cancellation = harness.Cancellation()
        cancellation.install()
        try:
            os.kill(os.getpid(), signal.SIGTERM)
            with self.assertRaises(harness.RunInterrupted):
                cancellation.sleep(60)
        finally:
            cancellation.restore()
        self.assertEqual(previous, {signum: signal.getsignal(signum) for signum in previous})


class CargoCountsTests(unittest.TestCase):
    def test_positive_matching_execution(self):
        result = executed_tests(MATCHING + summary(passed=1), "snapshot_request")
        self.assertEqual(result["matching_passed"], 1)

    def test_empty_ignored_unrelated_and_incomplete_logs_rejected(self):
        for text in (
            summary(filtered=90),
            MATCHING.replace("... ok", "... ignored") + summary(ignored=1),
            "test unrelated_test ... ok\n" + summary(passed=1),
            MATCHING,
            MATCHING + summary(),
            MATCHING + "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out;\n",
        ):
            with self.subTest(text=text), self.assertRaises(ValueError):
                executed_tests(text, "snapshot_request")

    def test_empty_other_test_binaries_do_not_hide_matching_execution(self):
        result = executed_tests(summary() + MATCHING + summary(passed=1), "snapshot_request")
        self.assertEqual(result["matching_passed"], 1)


class CargoFallbackShellTests(PreservedFiles):
    def exercise(self, text, *, nextest=False):
        fake_bin = self.root / "bin"
        fake_bin.mkdir(exist_ok=True)
        fake_cargo = fake_bin / "cargo"
        fake_cargo.write_text(
            '#!/bin/sh\n'
            'if [ "$1" = nextest ]; then\n'
            '  [ "$FAKE_NEXTEST" = yes ] || exit 1\n'
            '  [ "$2" = --version ] && exit 0\n'
            '  echo "no tests run"; exit 4\n'
            'fi\n'
            '[ "$1" = test ] || exit 2\n'
            'cat "$FAKE_CARGO_LOG"\n'
        )
        fake_cargo.chmod(0o700)
        fake_git = fake_bin / "git"
        fake_git.write_text("#!/bin/sh\nprintf '%s\\n' aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n")
        fake_git.chmod(0o700)
        log = self.root / "fake-libtest.stdout"
        log.write_text(text)
        env = os.environ.copy()
        env.update(PATH=f"{fake_bin}:{env['PATH']}", FAKE_CARGO_LOG=str(log),
                   FAKE_NEXTEST="yes" if nextest else "no")
        output = self.root / "output"
        result = subprocess.run(
            ["bash", str(HARNESS / "component-tests.sh"), str(self.root),
             str(self.root / "unused-target"), str(output)],
            env=env, capture_output=True, text=True, timeout=15,
        )
        (self.root / "shell.stdout").write_text(result.stdout)
        (self.root / "shell.stderr").write_text(result.stderr)
        self.assertTrue((output / "tests.log").exists(), result.stderr)
        return result, output

    def test_shell_accepts_nonzero_matching_count(self):
        result, output = self.exercise(MATCHING + summary(passed=1))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads((output / "test-count.json").read_text())["matching_passed"], 1)

    def test_shell_rejects_zero_tests(self):
        result, _ = self.exercise(summary(filtered=90))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no confirmed passing matching tests", result.stderr)

    def test_shell_rejects_ignored_only_tests(self):
        result, _ = self.exercise(MATCHING.replace("... ok", "... ignored") + summary(ignored=1))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no confirmed passing matching tests", result.stderr)

    def test_nextest_empty_failure_is_not_converted_to_a_fallback_pass(self):
        result, output = self.exercise(MATCHING + summary(passed=1), nextest=True)
        self.assertEqual(result.returncode, 4)
        self.assertFalse((output / "test-count.json").exists())


if __name__ == "__main__":
    if sys.argv[1:2] == ["--cancel-helper"]:
        sys.exit(cancellation_helper(Path(sys.argv[2])))
    unittest.main()
