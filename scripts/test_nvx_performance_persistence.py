#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import cast

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools.build_constants import (  # noqa: E402
    BuildConstants,
)
from nvx_tools_testing import (
    composite_action_script as _composite_action_script,
)

# The nvx.py of the repository that PersistPerformanceResultsActionTests
# persists to. Its persist-recorded-performance logs its arguments and whether
# reports of an earlier attempt remain, pushes the commit of the clone
# STUB_RACE to dev at its first call, adds a row to the history and writes the
# message of its commit when STUB_PERSIST is set, stages the file STUB_STAGE,
# writes its summary, and exits with STUB_STATUS.
_PERSIST_STUB = r"""
import json
import os
import subprocess
import sys
from pathlib import Path

arguments = sys.argv[1:]
summary = Path(arguments[arguments.index("--summary") + 1])
message = Path(arguments[arguments.index("--message") + 1])
log = Path(os.environ["STUB_LOG"])
attempt = len(log.read_text().splitlines()) + 1 if log.exists() else 1
with log.open("a") as stream:
    reports = {"summary": summary.exists(), "message": message.exists()}
    stream.write(json.dumps({"arguments": arguments, **reports}) + "\n")
race = os.environ.get("STUB_RACE")
if race and attempt == 1:
    subprocess.run(["git", "-C", race, "push", "origin", "HEAD:dev"], check=True)
if os.environ.get("STUB_PERSIST"):
    with open("data/linux-kvm.csv", "a") as stream:
        stream.write(f"row {attempt}\n")
    message.write_text(
        f"data: update performance baselines ({attempt}) [skip ci]\n\nRun {attempt}.\n"
    )
stage = os.environ.get("STUB_STAGE")
if stage:
    Path(stage).parent.mkdir(parents=True, exist_ok=True)
    Path(stage).write_text("staged\n")
    subprocess.run(["git", "add", stage], check=True)
summary.write_text(f"summary {attempt}\n")
sys.exit(int(os.environ.get("STUB_STATUS", "0")))
"""
_BASH = shutil.which("bash")
_GIT = shutil.which("git")
_PYTHON3 = shutil.which("python3")


@unittest.skipIf(
    os.name == "nt" or _BASH is None or _GIT is None or _PYTHON3 is None,
    "runs the action's bash step with Git and python3 on POSIX hosts",
)
class PersistPerformanceResultsActionTests(unittest.TestCase):
    """Runs the step of the persist-performance-results action, which commits
    the results of the dev pushes that a full run of dev persists, in a clone
    of a local repository whose nvx.py stubs persist-recorded-performance."""

    WORKFLOW_REF = "microsoft/nvx/.github/workflows/ci.yml@refs/heads/dev"

    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        config = self.root / "gitconfig"
        config.write_text("", encoding="utf-8")
        # Git reads neither the host's configuration nor an identity from the
        # environment, as on a runner, where the step sets its own.
        self.environment = {
            **{
                name: value
                for name, value in os.environ.items()
                if not name.startswith(("GIT_AUTHOR_", "GIT_COMMITTER_", "STUB_"))
            },
            "GIT_CONFIG_GLOBAL": str(config),
            "GIT_CONFIG_NOSYSTEM": "1",
        }
        self.origin = self.root / "origin.git"
        self.git("init", "--bare", "--initial-branch=dev", str(self.origin))
        self.seed = self.root / "seed"
        self.git("clone", str(self.origin), str(self.seed))
        for name, text in (
            ("scripts/nvx.py", _PERSIST_STUB),
            ("data/linux-kvm.csv", "header\n"),
            ("data/linux-kvm-host.json", "{}\n"),
        ):
            (self.seed / name).parent.mkdir(parents=True, exist_ok=True)
            (self.seed / name).write_text(text, encoding="utf-8")
        self.seeded = self.commit("seed")
        self.git("-C", str(self.seed), "push", "origin", "HEAD:dev")
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "persist-performance-results"
            / "action.yml"
        )
        self.script = _composite_action_script(
            action.read_text(encoding="utf-8"), "Persist results"
        )

    def git(self, *arguments: str) -> str:
        return subprocess.run(
            [cast(str, _GIT), *arguments],
            env=self.environment,
            capture_output=True,
            text=True,
            check=True,
        ).stdout

    def commit(self, subject: str) -> str:
        """Commits the seed clone's files, and returns the commit."""
        self.git("-C", str(self.seed), "add", "--all")
        self.git(
            "-C",
            str(self.seed),
            "-c",
            "user.name=Seed",
            "-c",
            "user.email=seed@example.com",
            "commit",
            "--allow-empty",
            "-m",
            subject,
        )
        return self.git("-C", str(self.seed), "rev-parse", "HEAD").strip()

    def persist(self, **stub: str) -> subprocess.CompletedProcess[str]:
        """Runs the step in a new clone of dev, as a new job does."""
        run = Path(tempfile.mkdtemp(dir=self.root))
        checkout = run / "checkout"
        self.git("clone", "--branch", "dev", str(self.origin), str(checkout))
        self.temporary = run / "temp"
        self.temporary.mkdir()
        self.summary = run / "summary.md"
        self.log = run / "calls.jsonl"
        return subprocess.run(
            [cast(str, _BASH), "--noprofile", "--norc", "-eo", "pipefail"]
            + ["-c", self.script],
            cwd=checkout,
            env={
                **self.environment,
                "GH_TOKEN": "token",
                "GITHUB_REPOSITORY": "microsoft/nvx",
                "GITHUB_STEP_SUMMARY": str(self.summary),
                "GITHUB_WORKFLOW_REF": self.WORKFLOW_REF,
                "RUNNER_TEMP": str(self.temporary),
                "STUB_LOG": str(self.log),
                "TARGET_BRANCH": "dev",
                **stub,
            },
            capture_output=True,
            text=True,
            check=False,
        )

    def calls(self) -> list[dict[str, object]]:
        """The calls of persist-recorded-performance: their arguments, and
        whether the reports of an earlier attempt remained."""
        lines = self.log.read_text(encoding="utf-8").splitlines()
        return [json.loads(line) for line in lines]

    def call(self) -> dict[str, object]:
        """A call of a new attempt."""
        temporary = self.temporary
        arguments = [
            "persist-recorded-performance",
            "--repository",
            "microsoft/nvx",
            "--workflow-ref",
            self.WORKFLOW_REF,
            "--branch",
            "dev",
            "--history-dir",
            "data",
            "--download-dir",
            f"{temporary}/performance-results",
            "--require-host",
            "--summary",
            f"{temporary}/performance-history.md",
            "--message",
            f"{temporary}/performance-history-message.txt",
        ]
        return {"arguments": arguments, "summary": False, "message": False}

    def dev(self, revision: str = "dev") -> str:
        return self.git("-C", str(self.origin), "rev-parse", revision).strip()

    def show(self, *arguments: str) -> str:
        return self.git("-C", str(self.origin), *arguments)

    def assert_persisted(self, attempt: int, parent: str) -> None:
        """Asserts that dev's newest commit, after `parent`, persists the
        results of the attempt `attempt` alone, with its message."""
        self.assertEqual(self.dev("dev~1"), parent)
        self.assertEqual(
            self.show("log", "-1", "--format=%an%n%B", "dev").rstrip("\n"),
            "github-actions[bot]\n"
            f"data: update performance baselines ({attempt}) [skip ci]\n"
            f"\nRun {attempt}.",
        )
        self.assertEqual(
            self.show("diff", "--name-only", "dev~1", "dev").split(),
            ["data/linux-kvm.csv"],
        )
        self.assertEqual(
            self.show("show", "dev:data/linux-kvm.csv"), f"header\nrow {attempt}\n"
        )

    def test_new_results_are_committed_to_dev_in_one_commit(self):
        result = self.persist(STUB_PERSIST="1")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.calls(), [self.call()])
        self.assert_persisted(1, self.seeded)
        self.assertEqual(self.summary.read_text(encoding="utf-8"), "summary 1\n")

    def test_refused_runs_fail_the_step_once_the_others_are_committed(self):
        result = self.persist(STUB_PERSIST="1", STUB_STATUS="1")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assert_persisted(1, self.seeded)
        self.assertEqual(self.summary.read_text(encoding="utf-8"), "summary 1\n")

    def test_no_new_results_commit_nothing_and_keep_the_status(self):
        for status in (0, 1):
            with self.subTest(status=status):
                result = self.persist(STUB_STATUS=str(status))
                self.assertEqual(
                    result.returncode, status, result.stdout + result.stderr
                )
                self.assertIn("No new performance results to persist", result.stdout)
                self.assertEqual(self.dev(), self.seeded)
                self.assertEqual(
                    self.summary.read_text(encoding="utf-8"), "summary 1\n"
                )

    def test_an_attempt_that_dev_outran_is_redone_on_the_newest_dev(self):
        # Another commit reaches dev while the first attempt persists, so that
        # dev refuses its push.
        newer = self.commit("newer")
        result = self.persist(STUB_PERSIST="1", STUB_RACE=str(self.seed))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        # The second attempt persists again from the newer commit, without
        # the reports of the first.
        self.assertEqual(self.calls(), [self.call(), self.call()])
        self.assert_persisted(2, newer)
        self.assertEqual(self.summary.read_text(encoding="utf-8"), "summary 2\n")

    def test_three_refused_pushes_fail_the_step(self):
        hook = self.origin / "hooks" / "pre-receive"
        hook.write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
        hook.chmod(0o755)
        result = self.persist(STUB_PERSIST="1")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(
            "::error::Failed to persist performance results after 3 attempts",
            result.stdout,
        )
        self.assertEqual(self.calls(), [self.call()] * 3)
        self.assertEqual(self.dev(), self.seeded)
        self.assertEqual(self.summary.read_text(encoding="utf-8"), "summary 3\n")

    def test_changes_outside_the_history_are_refused(self):
        for path in (
            "README.md",
            "data/linux-kvm-host.json",
            "data/nested/linux-kvm.csv",
        ):
            with self.subTest(path=path):
                result = self.persist(STUB_PERSIST="1", STUB_STAGE=path)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn(
                    "::error::Refusing to persist changes outside data/*.csv:\n"
                    f"{path}\n",
                    result.stdout,
                )
                self.assertEqual(self.dev(), self.seeded)
                self.assertEqual(
                    self.summary.read_text(encoding="utf-8"), "summary 1\n"
                )


if __name__ == "__main__":
    unittest.main()
