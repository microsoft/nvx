#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import contextlib
import io
import os
import sys
import tempfile
import unittest
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any
from unittest.mock import patch

import yaml

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import (  # noqa: E402
    common,
    merge_queue,
)
from nvx_tools.build_constants import (  # noqa: E402
    BuildConstants,
)

_MERGE_GROUP_PARENT = "a" * 40
_MERGE_GROUP_BRANCH = f"gh-readonly-queue/dev/pr-7-{_MERGE_GROUP_PARENT}"
_MERGE_GROUP_WORKFLOW_REF = (
    f"microsoft/nvx/.github/workflows/ci.yml@refs/heads/{_MERGE_GROUP_BRANCH}"
)


def _merge_group_run(
    run_id: int,
    branch: str,
    *,
    head: str | None = None,
    status: str = "in_progress",
    event: str = "merge_group",
    workflow: str = ".github/workflows/ci.yml",
) -> dict[str, Any]:
    """Return a workflow run whose head, unless given, is its ID in hex."""
    return {
        "id": run_id,
        "event": event,
        "status": status,
        "head_branch": branch,
        "head_sha": format(run_id, "040x") if head is None else head,
        "path": f"{workflow}@refs/heads/{branch}",
        "html_url": f"https://github.com/microsoft/nvx/actions/runs/{run_id}",
    }


class _FakeMergeQueueApi:
    """Answers the cancellation's requests from a list of runs and of the
    branches that exist, by their heads, and records each request and
    cancellation."""

    def __init__(
        self,
        runs: Sequence[Mapping[str, Any]],
        branches: Mapping[str, str],
        *,
        cancel_errors: Mapping[int, Exception] | None = None,
    ) -> None:
        self.runs = list(runs)
        self.branches = dict(branches)
        self.cancel_errors = dict(cancel_errors or {})
        self.requests: list[str] = []
        self.cancelled: list[int] = []

    def get(self, path: str, params: Mapping[str, str] | None = None) -> object:
        self.requests.append(f"GET {path}")
        prefix = "/repos/microsoft/nvx/git/matching-refs/heads/"
        assert path.startswith(prefix) and params is None, path
        start = path.removeprefix(prefix)
        return [
            {
                "ref": f"refs/heads/{branch}",
                "object": {"sha": head, "type": "commit"},
            }
            for branch, head in self.branches.items()
            if branch.startswith(start)
        ]

    def get_all(
        self,
        path: str,
        params: Mapping[str, str] | None = None,
        *,
        key: str | None = None,
        deadline: float | None = None,
    ) -> list[object]:
        assert params is not None and key == "workflow_runs" and deadline is None
        self.requests.append(f"GET {path} {params['event']} {params['status']}")
        assert path == "/repos/microsoft/nvx/actions/workflows/ci.yml/runs", path
        return [
            run
            for run in self.runs
            if run["status"] == params["status"] and run["event"] == params["event"]
        ]

    def post(self, path: str, body: Mapping[str, object]) -> object:
        self.requests.append(f"POST {path}")
        assert body == {}, body
        run_id = int(path.split("/")[-2])
        if run_id in self.cancel_errors:
            raise self.cancel_errors[run_id]
        self.cancelled.append(run_id)
        return None


class MergeQueueTests(unittest.TestCase):
    def test_merge_group_reads_the_branch_of_a_group(self):
        group = merge_queue.MergeGroup("dev", 7, _MERGE_GROUP_PARENT)
        self.assertEqual(merge_queue.merge_group(_MERGE_GROUP_BRANCH), group)
        self.assertEqual(
            merge_queue.merge_group(f"refs/heads/{_MERGE_GROUP_BRANCH}"), group
        )
        self.assertEqual(group.branch, _MERGE_GROUP_BRANCH)
        self.assertEqual(group.prefix, "gh-readonly-queue/dev/pr-7-")
        self.assertEqual(
            merge_queue.merge_group(f"gh-readonly-queue/release/1.0/pr-12-{'b' * 40}"),
            merge_queue.MergeGroup("release/1.0", 12, "b" * 40),
        )
        for ref in (
            "refs/heads/dev",
            "refs/pull/7/merge",
            "gh-readonly-queue/dev/pr-7",
            f"gh-readonly-queue/dev/pr-07-{_MERGE_GROUP_PARENT}",
            f"gh-readonly-queue/dev/pr-7-{'A' * 40}",
            f"gh-readonly-queue/dev/pr-7-{_MERGE_GROUP_PARENT}0",
            f"refs/tags/gh-readonly-queue/dev/pr-7-{_MERGE_GROUP_PARENT}",
        ):
            with self.subTest(ref=ref):
                self.assertIsNone(merge_queue.merge_group(ref))

    def test_cancels_the_unfinished_runs_of_the_earlier_groups(self):
        # The groups that the queue replaced, whose branches are gone or hold
        # another head, lose their runs, whether they run or wait; nothing
        # else does.
        replaced = f"gh-readonly-queue/dev/pr-7-{'b' * 40}"
        older = f"gh-readonly-queue/dev/pr-7-{'c' * 40}"
        # A live group of the pull request, whose run a late event may have
        # started before this one.
        live = f"gh-readonly-queue/dev/pr-7-{'d' * 40}"
        other_pull_request = f"gh-readonly-queue/dev/pr-8-{'b' * 40}"
        other_base = f"gh-readonly-queue/main/pr-7-{'b' * 40}"
        # A run whose head GitHub didn't report is left alone while its
        # branch exists.
        unreported = _merge_group_run(112, live)
        del unreported["head_sha"]
        runs = [
            _merge_group_run(102, older, status="queued"),
            _merge_group_run(101, replaced),
            _merge_group_run(103, _MERGE_GROUP_BRANCH),
            _merge_group_run(104, live),
            _merge_group_run(105, other_pull_request),
            _merge_group_run(106, other_base),
            _merge_group_run(107, replaced, status="completed"),
            _merge_group_run(108, replaced, event="push"),
            _merge_group_run(
                109, replaced, workflow=".github/workflows/module-diagrams.yml"
            ),
            _merge_group_run(110, replaced, status="waiting"),
            # The pull request left the queue and returned on the same parent,
            # so this run's group has the branch of the earlier group, whose
            # run is obsolete although its branch exists.
            _merge_group_run(111, _MERGE_GROUP_BRANCH),
            unreported,
        ]
        api = _FakeMergeQueueApi(
            runs,
            {
                _MERGE_GROUP_BRANCH: format(103, "040x"),
                live: format(104, "040x"),
            },
        )

        report = merge_queue.cancel_superseded_merge_groups(
            api,
            repository="microsoft/nvx",
            workflow_ref=_MERGE_GROUP_WORKFLOW_REF,
            run_id=103,
            ref=f"refs/heads/{_MERGE_GROUP_BRANCH}",
        )

        self.assertEqual(api.cancelled, [101, 102, 110, 111])
        self.assertEqual(
            report,
            [
                f"cancelled run {run_id} of `{branch}`: "
                f"https://github.com/microsoft/nvx/actions/runs/{run_id}"
                for run_id, branch in (
                    (101, replaced),
                    (102, older),
                    (110, replaced),
                    (111, _MERGE_GROUP_BRANCH),
                )
            ],
        )
        # It lists the runs of every unfinished status before the branches,
        # so that a listed run of a live group finds its branch.
        runs_path = "/repos/microsoft/nvx/actions/workflows/ci.yml/runs merge_group"
        self.assertEqual(
            api.requests,
            [
                *(
                    f"GET {runs_path} {status}"
                    for status in merge_queue.UNFINISHED_RUN_STATUSES
                ),
                "GET /repos/microsoft/nvx/git/matching-refs/heads/"
                "gh-readonly-queue/dev/pr-7-",
                "POST /repos/microsoft/nvx/actions/runs/101/cancel",
                "POST /repos/microsoft/nvx/actions/runs/102/cancel",
                "POST /repos/microsoft/nvx/actions/runs/110/cancel",
                "POST /repos/microsoft/nvx/actions/runs/111/cancel",
            ],
        )

    def test_a_late_run_of_a_replaced_group_cancels_itself_last(self):
        # GitHub can deliver a replaced group's event late, so that its run
        # starts after the run of the group that replaced it looked. Only the
        # late run then finds that the queue replaced its group, and it
        # cancels itself after the other runs that it finds replaced.
        live = _merge_group_run(102, _MERGE_GROUP_BRANCH)
        branches = {_MERGE_GROUP_BRANCH: format(102, "040x")}
        older = f"gh-readonly-queue/dev/pr-7-{'c' * 40}"
        for name, replaced, late in (
            # The queue deleted the late run's branch.
            (
                "deleted",
                f"gh-readonly-queue/dev/pr-7-{'b' * 40}",
                _merge_group_run(101, f"gh-readonly-queue/dev/pr-7-{'b' * 40}"),
            ),
            # The pull request returned on the same parent, so the late run's
            # branch is the live group's, with another head.
            (
                "reused",
                _MERGE_GROUP_BRANCH,
                _merge_group_run(101, _MERGE_GROUP_BRANCH),
            ),
        ):
            with self.subTest(branch=name):
                # The live group's run looks before the late run exists.
                api = _FakeMergeQueueApi([live], branches)
                report = merge_queue.cancel_superseded_merge_groups(
                    api,
                    repository="microsoft/nvx",
                    workflow_ref=_MERGE_GROUP_WORKFLOW_REF,
                    run_id=102,
                    ref=f"refs/heads/{_MERGE_GROUP_BRANCH}",
                )
                self.assertEqual((api.cancelled, report), ([], []))

                api = _FakeMergeQueueApi(
                    [late, live, _merge_group_run(105, older, status="queued")],
                    branches,
                )
                report = merge_queue.cancel_superseded_merge_groups(
                    api,
                    repository="microsoft/nvx",
                    workflow_ref=_MERGE_GROUP_WORKFLOW_REF,
                    run_id=101,
                    ref=f"refs/heads/{replaced}",
                )
                self.assertEqual(api.cancelled, [105, 101])
                self.assertEqual(
                    report,
                    [
                        f"cancelled run 105 of `{older}`: "
                        "https://github.com/microsoft/nvx/actions/runs/105",
                        f"cancelled run 101 of `{replaced}` (this run): "
                        "https://github.com/microsoft/nvx/actions/runs/101",
                    ],
                )

    def test_reports_the_runs_that_it_could_not_cancel(self):
        branches = [f"gh-readonly-queue/dev/pr-7-{digit * 40}" for digit in "bcdef"]
        runs = [
            _merge_group_run(run_id, branch)
            for run_id, branch in zip(range(101, 106), branches, strict=True)
        ]
        api = _FakeMergeQueueApi(
            runs,
            {_MERGE_GROUP_BRANCH: format(200, "040x")},
            cancel_errors={
                101: merge_queue.GitHubApiError(409, "Cannot cancel"),
                102: merge_queue.GitHubApiError(403, "Resource not accessible"),
                # GitHubClient reports a request that the connection lost as
                # a ScriptError, and a socket may fail with an OSError.
                103: common.ScriptError(
                    "GitHub POST /x failed: [Errno 104] Connection reset"
                ),
                104: TimeoutError("timed out"),
            },
        )

        report = merge_queue.cancel_superseded_merge_groups(
            api,
            repository="microsoft/nvx",
            workflow_ref=_MERGE_GROUP_WORKFLOW_REF,
            run_id=200,
            ref=_MERGE_GROUP_BRANCH,
        )

        # One failure never stops the others, nor loses their reports.
        self.assertEqual(api.cancelled, [105])
        self.assertEqual(
            report,
            [
                f"run 101 of `{branches[0]}` had completed",
                f"could not cancel run 102 of `{branches[1]}`: Resource not accessible",
                f"could not cancel run 103 of `{branches[2]}`: "
                "GitHub POST /x failed: [Errno 104] Connection reset",
                f"could not cancel run 104 of `{branches[3]}`: timed out",
                f"cancelled run 105 of `{branches[4]}`: "
                "https://github.com/microsoft/nvx/actions/runs/105",
            ],
        )

    def test_refuses_what_names_no_merge_group_or_workflow(self):
        api = _FakeMergeQueueApi([], {})
        for ref, workflow_ref, message in (
            ("refs/heads/dev", _MERGE_GROUP_WORKFLOW_REF, "names no merge group"),
            (
                _MERGE_GROUP_BRANCH,
                "other/repository/.github/workflows/ci.yml@refs/heads/dev",
                "names no workflow of microsoft/nvx",
            ),
            (
                _MERGE_GROUP_BRANCH,
                "microsoft/nvx/scripts/ci.yml@refs/heads/dev",
                "names no workflow of microsoft/nvx",
            ),
        ):
            with (
                self.subTest(ref=ref, workflow_ref=workflow_ref),
                self.assertRaisesRegex(common.ScriptError, message),
            ):
                merge_queue.cancel_superseded_merge_groups(
                    api,
                    repository="microsoft/nvx",
                    workflow_ref=workflow_ref,
                    run_id=1,
                    ref=ref,
                )
        self.assertEqual(api.requests, [])
        group = merge_queue.MergeGroup("dev", 7, _MERGE_GROUP_PARENT)
        with self.assertRaisesRegex(common.ScriptError, "invalid workflow run"):
            merge_queue.superseded_runs(
                ["run"],
                {},
                group=group,
                workflow=".github/workflows/ci.yml",
                run_id=1,
            )
        # A reference without its head can't tell a live group from a
        # replaced one.
        invalid: tuple[object, ...] = (
            {"ref": f"refs/heads/{_MERGE_GROUP_BRANCH}"},
            [
                {
                    "ref": f"refs/heads/{_MERGE_GROUP_BRANCH}",
                    "object": dict[str, str](),
                }
            ],
            [{"object": {"sha": "1" * 40}}],
        )
        for references in invalid:
            api = _FakeMergeQueueApi([], {})
            with (
                self.subTest(references=references),
                patch.object(api, "get", return_value=references),
                self.assertRaisesRegex(common.ScriptError, "invalid"),
            ):
                merge_queue.cancel_superseded_merge_groups(
                    api,
                    repository="microsoft/nvx",
                    workflow_ref=_MERGE_GROUP_WORKFLOW_REF,
                    run_id=1,
                    ref=_MERGE_GROUP_BRANCH,
                )
            self.assertEqual(api.cancelled, [])

    def test_command_warns_instead_of_failing(self):
        # Cancelling only frees runners, so a GitHub failure warns but never
        # fails the merge group's run.
        argv = [
            "cancel-superseded-merge-groups",
            "--repository",
            "microsoft/nvx",
            "--workflow-ref",
            _MERGE_GROUP_WORKFLOW_REF,
            "--run-id",
            "103",
            "--ref",
            f"refs/heads/{_MERGE_GROUP_BRANCH}",
        ]
        replaced = f"gh-readonly-queue/dev/pr-7-{'b' * 40}"
        for outcome, expected_output, expected_summary in (
            (
                common.ScriptError("GitHub GET /x failed with HTTP 502"),
                "::warning::could not look for superseded runs: "
                "GitHub GET /x failed with HTTP 502\n",
                "- could not look for superseded runs: "
                "GitHub GET /x failed with HTTP 502\n",
            ),
            # GitHub's error text can't end the warning or start another
            # workflow command.
            (
                common.ScriptError("GitHub GET /x failed: 100%\r\n::error::text"),
                "::warning::could not look for superseded runs: "
                "GitHub GET /x failed: 100%25%0D%0A::error::text\n",
                "- could not look for superseded runs: "
                "GitHub GET /x failed: 100%\r\n::error::text\n",
            ),
            (
                [f"cancelled run 101 of `{replaced}`: https://example/101"],
                f"cancelled run 101 of `{replaced}`: https://example/101\n",
                f"- cancelled run 101 of `{replaced}`: https://example/101\n",
            ),
            (
                [],
                "no earlier group of pull request #7 has a run left\n",
                "No earlier group of the pull request has a run left.\n",
            ),
        ):
            with (
                self.subTest(outcome=outcome),
                tempfile.TemporaryDirectory() as temporary,
            ):
                summary = Path(temporary) / "summary.md"
                args = nvx.parse_args([*argv, "--summary", os.fspath(summary)])
                cancel = (
                    patch.object(
                        merge_queue,
                        "cancel_superseded_merge_groups",
                        side_effect=outcome,
                    )
                    if isinstance(outcome, Exception)
                    else patch.object(
                        merge_queue,
                        "cancel_superseded_merge_groups",
                        return_value=outcome,
                    )
                )
                stdout = io.StringIO()
                with (
                    patch.dict(os.environ, {"GH_TOKEN": "token"}),
                    cancel as cancelled,
                    contextlib.redirect_stdout(stdout),
                ):
                    args.handler(args)
                self.assertEqual(stdout.getvalue(), expected_output)
                self.assertEqual(
                    summary.read_bytes().decode("utf-8"),
                    f"### Superseded merge groups of #7\n{expected_summary}",
                )
                self.assertEqual(cancelled.call_args.kwargs["run_id"], 103)
                self.assertEqual(
                    cancelled.call_args.kwargs["workflow_ref"],
                    _MERGE_GROUP_WORKFLOW_REF,
                )

    def test_command_needs_a_token_and_a_merge_group(self):
        argv = [
            "cancel-superseded-merge-groups",
            "--repository",
            "microsoft/nvx",
            "--workflow-ref",
            _MERGE_GROUP_WORKFLOW_REF,
            "--run-id",
            "1",
            "--ref",
        ]
        environment = {
            name: value
            for name, value in os.environ.items()
            if name not in ("GH_TOKEN", "GITHUB_TOKEN")
        }
        for ref, token, message in (
            (_MERGE_GROUP_BRANCH, None, "set GH_TOKEN"),
            ("refs/heads/dev", "token", "names no merge group"),
        ):
            args = nvx.parse_args([*argv, ref])
            with (
                self.subTest(ref=ref, token=token),
                patch.dict(
                    os.environ,
                    environment | ({"GH_TOKEN": token} if token else {}),
                    clear=True,
                ),
                patch.object(merge_queue, "cancel_superseded_merge_groups") as cancel,
                self.assertRaisesRegex(common.ScriptError, message),
            ):
                args.handler(args)
            cancel.assert_not_called()

    def test_merge_groups_cancel_the_runs_of_superseded_groups(self):
        workflow = yaml.safe_load(
            (BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text(
                encoding="utf-8"
            )
        )
        job = workflow["jobs"]["cancel-superseded-merge-groups"]
        self.assertEqual(job["name"], "Cancel superseded merge group runs")
        self.assertEqual(job["if"], "github.event_name == 'merge_group'")
        self.assertEqual(job["runs-on"], "ubuntu-latest")
        self.assertNotIn("needs", job)
        # Listing and cancelling the workflow's runs needs `actions: write`,
        # and reading the groups' branches `contents: read`.
        self.assertEqual(job["permissions"], {"actions": "write", "contents": "read"})
        # Nothing waits for it, so that a failure never holds the queue.
        for name, other in workflow["jobs"].items():
            needs = other.get("needs", [])
            with self.subTest(job=name):
                self.assertNotIn(
                    "cancel-superseded-merge-groups",
                    [needs] if isinstance(needs, str) else needs,
                )
        [checkout, cancel] = job["steps"]
        self.assertRegex(checkout["uses"], r"^actions/checkout@")
        # The token can cancel any of the repository's runs, so the job runs
        # the command that the group's base branch has merged rather than the
        # group's copy, although its steps still come from the group's ci.yml.
        self.assertEqual(
            checkout["with"],
            {
                "ref": "${{ github.event.merge_group.base_ref }}",
                "persist-credentials": False,
            },
        )
        self.assertEqual(
            cancel["env"],
            {
                "GH_TOKEN": "${{ github.token }}",
                "MERGE_GROUP_REF": "${{ github.event.merge_group.head_ref }}",
            },
        )
        self.assertEqual(
            cancel["run"].split(),
            [
                "python3",
                "scripts/nvx.py",
                "cancel-superseded-merge-groups",
                "--repository",
                '"${GITHUB_REPOSITORY}"',
                "--workflow-ref",
                '"${GITHUB_WORKFLOW_REF}"',
                "--run-id",
                '"${GITHUB_RUN_ID}"',
                "--ref",
                '"${MERGE_GROUP_REF}"',
                "--summary",
                '"${GITHUB_STEP_SUMMARY}"',
            ],
        )


if __name__ == "__main__":
    unittest.main()
