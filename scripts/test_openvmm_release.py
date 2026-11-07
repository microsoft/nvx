#!/usr/bin/env python3
"""Hermetic tests for the OpenVMM microVM release tool."""

from __future__ import annotations

import io
import json
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import TextIO, cast
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import openvmm_release  # noqa: E402
from openvmm_git import DEV_BRANCH, RELEASE_PREFIX, GitError, chain  # noqa: E402


def _run(
    args: Sequence[str],
    *,
    cwd: Path | None = None,
) -> str:
    result = subprocess.run(
        list(args),
        cwd=cwd,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    if result.returncode != 0:
        raise AssertionError(
            f"{' '.join(args)} failed with status {result.returncode}: "
            f"{result.stderr or result.stdout}"
        )
    return result.stdout.strip()


def _git(repo: Path, *args: str) -> str:
    return _run(("git", "-C", str(repo), *args))


def _remote_ref(repo: Path, remote: str, ref: str) -> str | None:
    result = subprocess.run(
        ["git", "-C", str(repo), "ls-remote", "--exit-code", remote, ref],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    if result.returncode == 2:
        return None
    if result.returncode != 0:
        raise AssertionError(result.stderr)
    return result.stdout.split()[0]


def _json_object(output: io.StringIO) -> dict[str, object]:
    value = cast(object, json.loads(output.getvalue()))
    if not isinstance(value, dict):
        raise AssertionError("output is not a JSON object")
    return cast(dict[str, object], value)


class Repository:
    """A temporary Git repository with unsigned commits and a local identity."""

    def __init__(self, path: Path) -> None:
        self.path = path
        path.mkdir()
        _run(("git", "init", "--quiet", str(path)))
        for key, value in (
            ("user.name", "NVX Test"),
            ("user.email", "nvx@example.com"),
            ("commit.gpgsign", "false"),
        ):
            _git(path, "config", key, value)
        _git(path, "branch", "-M", "main")

    def commit(self, name: str, content: str | None = None) -> str:
        target = self.path / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content if content is not None else name, encoding="utf-8")
        _git(self.path, "add", name)
        _git(self.path, "commit", "--quiet", "-m", f"add {name}")
        return _git(self.path, "rev-parse", "HEAD")

    def branch(self, name: str, start: str = "HEAD") -> None:
        _git(self.path, "branch", name, start)

    def checkout(self, name: str, start: str | None = None) -> None:
        if start is None:
            _git(self.path, "checkout", "--quiet", name)
        else:
            _git(self.path, "checkout", "--quiet", "-b", name, start)


@dataclass(frozen=True)
class RunnerCall:
    """One command observed by the fake runner."""

    args: tuple[str, ...]
    cwd: Path
    capture_output: bool


class FakeRunner:
    """Return configured GitHub JSON and fake all validation commands."""

    def __init__(self) -> None:
        self.openvmm_prs: dict[str, list[dict[str, object]]] = {}
        self.promotions: list[dict[str, object]] = []
        self.pr_views: dict[int, dict[str, object]] = {}
        self.workflow_batches: list[list[dict[str, object]]] = []
        self.command_returncodes: list[int] = []
        self.calls: list[RunnerCall] = []
        self.worktree_heads: list[str] = []

    @staticmethod
    def _after(args: Sequence[str], option: str) -> str:
        return args[args.index(option) + 1]

    def run(
        self,
        args: Sequence[str],
        *,
        cwd: Path,
        capture_output: bool,
        stream: TextIO | None = None,
    ) -> subprocess.CompletedProcess[str]:
        del stream
        command = tuple(args)
        self.calls.append(RunnerCall(command, cwd, capture_output))
        output: object = None
        if command[:3] == ("gh", "pr", "list"):
            repository = self._after(command, "--repo")
            if repository == openvmm_release.OPENVMM_REPOSITORY:
                base = self._after(command, "--base")
                output = self.openvmm_prs.get(base, [])
            else:
                output = self.promotions
        elif command[:3] == ("gh", "pr", "view"):
            output = self.pr_views[int(command[3])]
        elif command[:3] == ("gh", "run", "list"):
            output = self.workflow_batches.pop(0) if self.workflow_batches else []
        else:
            if (cwd / ".git").exists() or (cwd / ".git").is_file():
                self.worktree_heads.append(_git(cwd, "rev-parse", "HEAD"))
            status = self.command_returncodes.pop(0) if self.command_returncodes else 0
            return subprocess.CompletedProcess(list(args), status, "", "")
        return subprocess.CompletedProcess(list(args), 0, json.dumps(output), "")


def _pull_request(number: int, head: str, base: str) -> dict[str, object]:
    return {
        "number": number,
        "title": f"pull request {number}",
        "headRefName": head,
        "baseRefName": base,
        "isDraft": False,
        "url": f"https://example.test/pr/{number}",
    }


def _scope(base: str, tip: str, pin: str) -> str:
    return f"""# Roadmap

## Scope

- OpenVMM fork base: `{base}`, upstream main today.
- Mapped fork tip: `{tip}`, position 1.
- NVX's pinned OpenVMM revision: `{pin}`, position 2.
- Upstream main: `{base}`.

## Upstream status

### Open upstream pull requests

No pull requests.
"""


class TemporaryTestCase(unittest.TestCase):
    """Give each test an isolated directory and locally configured repositories."""

    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)

    def bare_remote(self, name: str = "remote.git") -> Path:
        remote = self.root / name
        _run(("git", "init", "--quiet", "--bare", str(remote)))
        return remote


@dataclass(frozen=True)
class StateFixture:
    """Repositories and revisions used by state tests."""

    openvmm: Repository
    nvx: Repository
    remote: Path
    roadmap: Path
    base: str
    old_head: str
    new_base: str
    previous_head: str


class StateTests(TemporaryTestCase):
    def fixture(self) -> StateFixture:
        repository = Repository(self.root / "openvmm")
        base = repository.commit("base.txt")
        repository.checkout(DEV_BRANCH, base)
        first = repository.commit("chain-1.txt")
        second = repository.commit("chain-2.txt")
        old_head = repository.commit("chain-3.txt")
        repository.branch(f"{RELEASE_PREFIX}v2.9", first)
        repository.branch(f"{RELEASE_PREFIX}v2.10", second)
        repository.checkout("main")
        new_base = repository.commit("upstream.txt")
        repository.branch(f"{RELEASE_PREFIX}v9.0", new_base)

        remote = self.bare_remote()
        _git(repository.path, "remote", "add", "upstream", str(remote))
        for branch in (
            "main",
            DEV_BRANCH,
            f"{RELEASE_PREFIX}v2.9",
            f"{RELEASE_PREFIX}v2.10",
            f"{RELEASE_PREFIX}v9.0",
        ):
            _git(repository.path, "push", "--quiet", "upstream", f"{branch}:{branch}")

        nvx = Repository(self.root / "nvx")
        nvx.commit("README")
        _git(
            nvx.path,
            "update-index",
            "--add",
            "--cacheinfo",
            f"160000,{old_head},openvmm",
        )
        _git(nvx.path, "commit", "--quiet", "-m", "pin OpenVMM")
        roadmap = self.root / "roadmap.md"
        roadmap.write_text(_scope(base, first, old_head), encoding="utf-8")
        return StateFixture(
            repository,
            nvx,
            remote,
            roadmap,
            base,
            old_head,
            new_base,
            second,
        )

    def test_state_selects_the_newest_ancestor_and_walks_the_pull_request_tree(
        self,
    ) -> None:
        fixture = self.fixture()
        runner = FakeRunner()
        runner.openvmm_prs[DEV_BRANCH] = [_pull_request(10, "stack-parent", DEV_BRANCH)]
        runner.openvmm_prs["stack-parent"] = [
            _pull_request(11, "stack-child", "stack-parent")
        ]
        runner.promotions = [
            {
                "number": 20,
                "title": "openvmm: pin candidate",
                "url": "https://example.test/nvx/20",
                "headRefName": "pin-candidate",
            }
        ]
        output = io.StringIO()
        with patch.object(openvmm_release, "require_remote"):
            status = openvmm_release.state_command(
                openvmm=fixture.openvmm.path,
                roadmap=fixture.roadmap,
                nvx=fixture.nvx.path,
                remote="upstream",
                kind="rebase",
                target="HEAD",
                no_fetch=False,
                json_output=True,
                runner=runner,
                out=output,
            )
        self.assertEqual(status, 0)
        data = _json_object(output)
        self.assertEqual(data["old_base"], fixture.base)
        self.assertEqual(data["old_head"], fixture.old_head)
        self.assertEqual(data["new_base"], fixture.new_base)
        self.assertEqual(data["next_version"], "v2.11")
        self.assertEqual(data["next_release"], f"{RELEASE_PREFIX}v2.11")
        previous = cast(dict[str, object], data["previous_release"])
        self.assertEqual(previous["head"], fixture.previous_head)
        tree = cast(list[dict[str, object]], data["open_openvmm_pull_requests"])
        self.assertEqual(tree[0]["number"], 10)
        children = cast(list[dict[str, object]], tree[0]["children"])
        self.assertEqual(children[0]["number"], 11)
        self.assertEqual(
            openvmm_release.next_version(f"{RELEASE_PREFIX}v2.10", "reorganization"),
            "v3.0",
        )

    def test_state_refuses_an_existing_next_release(self) -> None:
        fixture = self.fixture()
        _git(
            fixture.openvmm.path,
            "push",
            "--quiet",
            "upstream",
            f"{fixture.new_base}:refs/heads/{RELEASE_PREFIX}v2.11",
        )
        with (
            patch.object(openvmm_release, "require_remote"),
            self.assertRaisesRegex(openvmm_release.ReleaseError, "already exists"),
        ):
            openvmm_release.state_command(
                openvmm=fixture.openvmm.path,
                roadmap=fixture.roadmap,
                nvx=fixture.nvx.path,
                remote="upstream",
                kind="rebase",
                target="HEAD",
                no_fetch=False,
                json_output=True,
                runner=FakeRunner(),
                out=io.StringIO(),
            )


class DropsTests(TemporaryTestCase):
    def test_drops_parses_cherry_output_and_only_the_required_roadmap_section(
        self,
    ) -> None:
        repository = Repository(self.root / "openvmm")
        base = repository.commit("base.txt")
        repository.checkout("upstream", base)
        new_base = repository.commit("shared.rs", "same patch\n")
        repository.checkout("chain", base)
        equivalent = repository.commit("shared.rs", "same patch\n")
        _git(repository.path, "commit", "--amend", "--quiet", "-m", "chain equivalent")
        equivalent = _git(repository.path, "rev-parse", "HEAD")
        old_head = repository.commit("unique.rs", "chain only\n")
        roadmap = self.root / "roadmap.md"
        roadmap.write_text(
            """# Roadmap

## Scope

- OpenVMM fork base: `0000000000000000000000000000000000000000`.

## Upstream status

A quoted row mentions microsoft/openvmm#999.

### Open upstream pull requests

microsoft/openvmm#100 and microsoft/openvmm#101 are tracked.
microsoft/openvmm#100 is mentioned twice.

### A later section

microsoft/openvmm#998 is outside the section.
""",
            encoding="utf-8",
        )
        runner = FakeRunner()
        runner.pr_views = {
            100: {
                "number": 100,
                "state": "MERGED",
                "mergedAt": "2026-11-01T00:00:00Z",
                "mergeCommit": {"oid": new_base},
                "title": "merged change",
                "url": "https://example.test/pr/100",
            },
            101: {
                "number": 101,
                "state": "OPEN",
                "mergedAt": None,
                "mergeCommit": None,
                "title": "open change",
                "url": "https://example.test/pr/101",
            },
        }
        output = io.StringIO()
        status = openvmm_release.drops_command(
            openvmm=repository.path,
            roadmap=roadmap,
            nvx=self.root,
            old_base=base,
            old_head=old_head,
            new_base=new_base,
            json_output=True,
            runner=runner,
            out=output,
        )
        self.assertEqual(status, 0)
        data = _json_object(output)
        upstream = cast(dict[str, object], data["upstream_commits"])
        self.assertEqual(upstream["count"], 1)
        candidates = cast(
            list[dict[str, object]], data["patch_equivalent_chain_commits"]
        )
        self.assertEqual(candidates[0]["position"], 1)
        self.assertEqual(candidates[0]["commit"], equivalent[:9])
        pull_requests = cast(
            list[dict[str, object]], data["roadmap_open_upstream_pull_requests"]
        )
        self.assertEqual([item["number"] for item in pull_requests], [100, 101])
        merged = cast(
            list[dict[str, object]], data["merged_pull_request_drop_candidates"]
        )
        self.assertEqual([item["number"] for item in merged], [100])
        viewed = [
            int(call.args[3])
            for call in runner.calls
            if call.args[:3] == ("gh", "pr", "view")
        ]
        self.assertEqual(viewed, [100, 101])


class CheckChainTests(TemporaryTestCase):
    def test_check_chain_reports_faked_bad_signatures_and_range_diff(self) -> None:
        repository = Repository(self.root / "openvmm")
        base = repository.commit("base.txt")
        first = repository.commit("first.txt")
        head = repository.commit("second.txt")

        def signatures(
            repo: Path,
            queried_base: str,
            queried_head: str,
        ) -> Sequence[openvmm_release.Signature]:
            del repo
            self.assertEqual(queried_base, base)
            self.assertEqual(queried_head, head)
            return (
                # Signed, but this host cannot verify the signature.
                openvmm_release.Signature(first, "N", "add first.txt"),
                openvmm_release.Signature(head, "N", "add second.txt", signed=False),
            )

        output = io.StringIO()
        status = openvmm_release.check_chain_command(
            openvmm=repository.path,
            worktree=None,
            new_base=base,
            head=head,
            old_base=base,
            old_head=head,
            json_output=True,
            signature_query=signatures,
            out=output,
        )
        self.assertEqual(status, 1)
        data = _json_object(output)
        invalid = cast(list[dict[str, object]], data["invalid_signatures"])
        self.assertEqual([item["commit"] for item in invalid], [head])
        self.assertEqual(invalid[0]["state"], "unsigned")
        unverified = cast(list[dict[str, object]], data["unverified_signatures"])
        self.assertEqual([item["commit"] for item in unverified], [first])
        self.assertIsInstance(data["verification_hint"], str)
        self.assertIs(data["tree_identical"], True)
        range_diff = cast(dict[str, object], data["range_diff"])
        counts = cast(dict[str, int], range_diff["counts"])
        self.assertEqual(counts["="], 2)

    def test_check_chain_rejects_a_merge_even_when_every_signature_is_good(
        self,
    ) -> None:
        repository = Repository(self.root / "openvmm")
        base = repository.commit("base.txt")
        repository.checkout("side", base)
        repository.commit("side.txt")
        repository.checkout("main")
        repository.commit("main.txt")
        _git(repository.path, "merge", "--quiet", "--no-ff", "side", "-m", "merge side")
        head = _git(repository.path, "rev-parse", "HEAD")

        def signatures(
            repo: Path,
            queried_base: str,
            queried_head: str,
        ) -> Sequence[openvmm_release.Signature]:
            del queried_base, queried_head
            return tuple(
                openvmm_release.Signature(
                    commit,
                    "G",
                    _git(repo, "show", "-s", "--format=%s", commit),
                )
                for commit in chain(repo, base, head)
            )

        output = io.StringIO()
        status = openvmm_release.check_chain_command(
            openvmm=repository.path,
            worktree=None,
            new_base=base,
            head=head,
            old_base=None,
            old_head=None,
            json_output=True,
            signature_query=signatures,
            out=output,
        )
        self.assertEqual(status, 1)
        data = _json_object(output)
        self.assertEqual(len(cast(list[object], data["merges"])), 1)
        self.assertEqual(cast(list[object], data["invalid_signatures"]), [])

    def test_check_chain_rejects_a_head_that_does_not_descend_from_the_new_base(
        self,
    ) -> None:
        repository = Repository(self.root / "openvmm")
        base = repository.commit("base.txt")
        repository.commit("first.txt")
        old_head = repository.commit("second.txt")
        repository.checkout("upstream", base)
        new_main = repository.commit("upstream.txt")

        def signatures(
            repo: Path,
            queried_base: str,
            queried_head: str,
        ) -> Sequence[openvmm_release.Signature]:
            commits = _git(repo, "rev-list", f"{queried_base}..{queried_head}")
            return tuple(
                openvmm_release.Signature(commit, "G", "subject")
                for commit in commits.split()
            )

        # The chain before its replay onto the new main, whose signed commits
        # base..head still lists, and a base past the head, for which it lists
        # nothing.
        for case, new_base, head in (
            ("not replayed", new_main, old_head),
            ("past the head", old_head, base),
        ):
            with self.subTest(case=case):
                with self.assertRaisesRegex(GitError, "does not descend"):
                    openvmm_release.check_chain_command(
                        openvmm=repository.path,
                        worktree=None,
                        new_base=new_base,
                        head=head,
                        old_base=None,
                        old_head=None,
                        json_output=True,
                        signature_query=signatures,
                        out=io.StringIO(),
                    )


class CheckCommitsTests(TemporaryTestCase):
    def fixture(self) -> tuple[Repository, str, list[str]]:
        repository = Repository(self.root / "openvmm")
        base = repository.commit("base.txt")
        first = repository.commit("src/one.rs", "fn one() {}\n")
        second = repository.commit("README.md", "documentation\n")
        third = repository.commit("src/two.rs", "fn two() {}\n")
        return repository, base, [first, second, third]

    def test_check_commits_dry_run_prints_the_plan_without_running_commands(
        self,
    ) -> None:
        repository, base, commits = self.fixture()
        runner = FakeRunner()
        output = io.StringIO()
        status = openvmm_release.check_commits_command(
            openvmm=repository.path,
            worktree=None,
            new_base=base,
            head=commits[-1],
            commits=["all"],
            dry_run=True,
            keep_going=False,
            json_output=True,
            runner=runner,
            out=output,
            err=io.StringIO(),
        )
        self.assertEqual(status, 0)
        self.assertEqual(runner.calls, [])
        plans = cast(list[dict[str, object]], _json_object(output)["commits"])
        first_commands = cast(list[list[str]], plans[0]["commands"])
        self.assertEqual(
            first_commands,
            [
                ["rustfmt", "--check", "--edition", "2024", "src/one.rs"],
                ["cargo", "check", "--locked", "--workspace", "--all-targets"],
                ["cargo", "xtask", "fmt", "--pass", "lints"],
            ],
        )
        second_commands = cast(list[list[str]], plans[1]["commands"])
        self.assertEqual(second_commands[0][0:2], ["cargo", "check"])

    def test_check_commits_runs_each_commit_oldest_first_in_detached_worktrees(
        self,
    ) -> None:
        repository, base, commits = self.fixture()
        runner = FakeRunner()
        status = openvmm_release.check_commits_command(
            openvmm=repository.path,
            worktree=None,
            new_base=base,
            head=commits[-1],
            commits=["all"],
            dry_run=False,
            keep_going=False,
            json_output=True,
            runner=runner,
            out=io.StringIO(),
            err=io.StringIO(),
        )
        self.assertEqual(status, 0)
        self.assertEqual(
            [call.args[0] for call in runner.calls],
            [
                "rustfmt",
                "cargo",
                "cargo",
                "cargo",
                "cargo",
                "rustfmt",
                "cargo",
                "cargo",
            ],
        )
        self.assertEqual(
            runner.worktree_heads,
            [
                commits[0],
                commits[0],
                commits[0],
                commits[1],
                commits[1],
                commits[2],
                commits[2],
                commits[2],
            ],
        )
        worktrees = _git(repository.path, "worktree", "list", "--porcelain")
        self.assertEqual(worktrees.count("worktree "), 1)

    def test_check_commits_stops_at_the_first_failed_command(self) -> None:
        repository, base, commits = self.fixture()
        runner = FakeRunner()
        runner.command_returncodes = [0, 7]
        output = io.StringIO()
        status = openvmm_release.check_commits_command(
            openvmm=repository.path,
            worktree=None,
            new_base=base,
            head=commits[-1],
            commits=["all"],
            dry_run=False,
            keep_going=False,
            json_output=True,
            runner=runner,
            out=output,
            err=io.StringIO(),
        )
        self.assertEqual(status, 1)
        self.assertEqual(len(runner.calls), 2)
        results = cast(list[dict[str, object]], _json_object(output)["commits"])
        self.assertEqual(len(results), 1)
        self.assertEqual(
            results[0]["failed_command"],
            ["cargo", "check", "--locked", "--workspace", "--all-targets"],
        )


@dataclass(frozen=True)
class LandFixture:
    """A release line, new head, and bare remote for landing tests."""

    repository: Repository
    remote: Path
    base: str
    previous_head: str
    old_head: str
    new_head: str
    previous_release: str


class PublishTests(TemporaryTestCase):
    def test_publish_supports_dry_run_and_push_without_real_github(self) -> None:
        repository = Repository(self.root / "openvmm")
        head = repository.commit("release.txt")
        remote = self.bare_remote()
        _git(repository.path, "remote", "add", "upstream", str(remote))

        dry_output = io.StringIO()
        with patch.object(openvmm_release, "require_remote"):
            dry_status = openvmm_release.publish_command(
                directory=repository.path,
                nvx=self.root,
                remote="upstream",
                version="v7.1",
                push=False,
                wait_minutes=300,
                appear_timeout=1800,
                json_output=True,
                runner=FakeRunner(),
                out=dry_output,
                err=io.StringIO(),
            )
        self.assertEqual(dry_status, 0)
        self.assertIsNone(
            _remote_ref(
                repository.path,
                "upstream",
                f"refs/heads/{RELEASE_PREFIX}v7.1",
            )
        )
        self.assertIs(_json_object(dry_output)["dry_run"], True)

        runner = FakeRunner()
        runner.workflow_batches = [
            [
                {
                    "databaseId": 42,
                    "status": "in_progress",
                    "conclusion": "",
                    "workflowName": "OpenVMM CI",
                    "url": "https://example.test/run/42",
                }
            ]
        ]
        output = io.StringIO()
        with patch.object(openvmm_release, "require_remote"):
            status = openvmm_release.publish_command(
                directory=repository.path,
                nvx=self.root,
                remote="upstream",
                version="v7.2",
                push=True,
                wait_minutes=12,
                appear_timeout=60,
                json_output=True,
                runner=runner,
                out=output,
                err=io.StringIO(),
            )
        self.assertEqual(status, 0)
        self.assertEqual(
            _remote_ref(
                repository.path,
                "upstream",
                f"refs/heads/{RELEASE_PREFIX}v7.2",
            ),
            head,
        )
        self.assertEqual(runner.calls[0].args[:3], ("gh", "run", "list"))
        self.assertEqual(
            runner.calls[1].args[-4:],
            ("--revision", head, "--wait-minutes", "12"),
        )
        self.assertIs(_json_object(output)["passed"], True)


class LandTests(TemporaryTestCase):
    def fixture(self) -> LandFixture:
        repository = Repository(self.root / "openvmm")
        base = repository.commit("base.txt")
        repository.checkout(DEV_BRANCH, base)
        previous_head = repository.commit("previous.txt")
        old_head = repository.commit("old.txt")
        previous_release = f"{RELEASE_PREFIX}v5.0"
        repository.branch(previous_release, previous_head)
        repository.checkout("new-release", base)
        new_head = repository.commit("new.txt")
        remote = self.bare_remote()
        _git(repository.path, "remote", "add", "upstream", str(remote))
        for branch in ("main", DEV_BRANCH, previous_release):
            _git(repository.path, "push", "--quiet", "upstream", f"{branch}:{branch}")
        return LandFixture(
            repository,
            remote,
            base,
            previous_head,
            old_head,
            new_head,
            previous_release,
        )

    def call_land(
        self,
        fixture: LandFixture,
        *,
        push: bool,
        runner: FakeRunner | None = None,
    ) -> tuple[int, dict[str, object]]:
        output = io.StringIO()
        with patch.object(openvmm_release, "require_remote"):
            status = openvmm_release.land_command(
                openvmm=fixture.repository.path,
                nvx=self.root,
                remote="upstream",
                old_head=fixture.old_head,
                new_head=fixture.new_head,
                previous_release=fixture.previous_release,
                push=push,
                json_output=True,
                runner=runner or FakeRunner(),
                out=output,
            )
        return status, _json_object(output)

    def test_land_supports_dry_run_and_the_two_guarded_pushes(self) -> None:
        fixture = self.fixture()
        status, data = self.call_land(fixture, push=False)
        self.assertEqual(status, 0)
        self.assertIs(data["dry_run"], True)
        self.assertEqual(
            _remote_ref(
                fixture.repository.path,
                "upstream",
                f"refs/heads/{fixture.previous_release}",
            ),
            fixture.previous_head,
        )
        self.assertEqual(
            _remote_ref(
                fixture.repository.path,
                "upstream",
                f"refs/heads/{DEV_BRANCH}",
            ),
            fixture.old_head,
        )

        runner = FakeRunner()
        runner.openvmm_prs[DEV_BRANCH] = [_pull_request(30, "needs-rebase", DEV_BRANCH)]
        status, data = self.call_land(fixture, push=True, runner=runner)
        self.assertEqual(status, 0)
        self.assertIs(data["passed"], True)
        self.assertEqual(
            _remote_ref(
                fixture.repository.path,
                "upstream",
                f"refs/heads/{fixture.previous_release}",
            ),
            fixture.old_head,
        )
        self.assertEqual(
            _remote_ref(
                fixture.repository.path,
                "upstream",
                f"refs/heads/{DEV_BRANCH}",
            ),
            fixture.new_head,
        )
        pulls = cast(list[dict[str, object]], data["open_pull_requests_needing_rebase"])
        self.assertEqual(pulls[0]["number"], 30)

    def test_land_reports_when_development_moved_forward(self) -> None:
        fixture = self.fixture()
        fixture.repository.checkout(DEV_BRANCH)
        moved = fixture.repository.commit("moved.txt")
        _git(
            fixture.repository.path,
            "push",
            "--quiet",
            "upstream",
            f"{DEV_BRANCH}:{DEV_BRANCH}",
        )
        with (
            patch.object(openvmm_release, "require_remote"),
            self.assertRaisesRegex(
                openvmm_release.ReleaseError,
                "Carry these commits onto the release",
            ) as raised,
        ):
            openvmm_release.land_command(
                openvmm=fixture.repository.path,
                nvx=self.root,
                remote="upstream",
                old_head=fixture.old_head,
                new_head=fixture.new_head,
                previous_release=fixture.previous_release,
                push=False,
                json_output=True,
                runner=FakeRunner(),
                out=io.StringIO(),
            )
        self.assertEqual(raised.exception.exit_code, 3)
        self.assertIn(moved[:7], str(raised.exception))

    def test_land_refuses_a_rewritten_development_branch(self) -> None:
        fixture = self.fixture()
        fixture.repository.checkout("rewrite", fixture.base)
        rewritten = fixture.repository.commit("rewritten.txt")
        _git(
            fixture.repository.path,
            "push",
            "--quiet",
            "--force",
            "upstream",
            f"{rewritten}:refs/heads/{DEV_BRANCH}",
        )
        with (
            patch.object(openvmm_release, "require_remote"),
            self.assertRaisesRegex(
                openvmm_release.ReleaseError, "was rewritten"
            ) as raised,
        ):
            openvmm_release.land_command(
                openvmm=fixture.repository.path,
                nvx=self.root,
                remote="upstream",
                old_head=fixture.old_head,
                new_head=fixture.new_head,
                previous_release=fixture.previous_release,
                push=False,
                json_output=True,
                runner=FakeRunner(),
                out=io.StringIO(),
            )
        self.assertEqual(raised.exception.exit_code, 2)

    def test_land_never_forces_a_previous_release_that_is_not_an_ancestor(
        self,
    ) -> None:
        fixture = self.fixture()
        _git(
            fixture.repository.path,
            "push",
            "--quiet",
            "--force",
            "upstream",
            f"{fixture.new_head}:refs/heads/{fixture.previous_release}",
        )
        with (
            patch.object(openvmm_release, "require_remote"),
            self.assertRaisesRegex(
                openvmm_release.ReleaseError,
                "not an ancestor.*never force",
            ) as raised,
        ):
            openvmm_release.land_command(
                openvmm=fixture.repository.path,
                nvx=self.root,
                remote="upstream",
                old_head=fixture.old_head,
                new_head=fixture.new_head,
                previous_release=fixture.previous_release,
                push=False,
                json_output=True,
                runner=FakeRunner(),
                out=io.StringIO(),
            )
        self.assertEqual(raised.exception.exit_code, 2)


if __name__ == "__main__":
    unittest.main()
