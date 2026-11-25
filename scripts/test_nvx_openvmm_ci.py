#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import contextlib
import io
import os
import re
import sys
import tempfile
import unittest
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path
from typing import Any, cast
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import (  # noqa: E402
    common,
    openvmm_ci,
)

_OPENVMM_REVISION = "3d7512af653717aa9123595e993a56cb8587106a"


def _openvmm_run(
    run_id: int,
    workflow: str = "openvmm-pr.yaml",
    *,
    status: str = "completed",
    head_sha: str = _OPENVMM_REVISION,
    head_repository: str = "microsoft/openvmm",
) -> dict[str, Any]:
    return {
        "id": run_id,
        "path": f".github/workflows/{workflow}",
        "status": status,
        "head_sha": head_sha,
        "head_repository": {"full_name": head_repository},
        "html_url": f"https://github.com/microsoft/openvmm/actions/runs/{run_id}",
    }


def _openvmm_job(
    run_id: int,
    name: str,
    status: str = "completed",
    conclusion: str | None = "success",
) -> dict[str, Any]:
    return {
        "name": name,
        "status": status,
        "conclusion": conclusion,
        "html_url": f"https://github.com/microsoft/openvmm/actions/runs/{run_id}/job/1",
    }


def _openvmm_gate_jobs(
    run_id: int, conclusion: str | None = "success", status: str = "completed"
) -> list[dict[str, Any]]:
    """Return a run's jobs that run OpenVMM's required jobs, under the names
    that the OpenVMM CI workflow gives them, and a job that none requires."""
    return [
        _openvmm_job(run_id, "clippy [x64-linux, macos], unit tests [x64-linux]"),
        _openvmm_job(run_id, "clippy [x64-windows], unit tests [x64-windows]"),
        *(
            _openvmm_job(run_id, f"run vmm-tests [{target}]", status, conclusion)
            for target in (
                "x64-linux-amd-kvm",
                "x64-linux-intel-mshv",
                "x64-windows-intel",
                "x64-windows-amd",
            )
        ),
        _openvmm_job(
            run_id, "run vmm-tests [x64-windows-intel-tdx]", conclusion="failure"
        ),
    ]


_OpenVMMLook = tuple[
    Sequence[Mapping[str, Any]], Mapping[int, Sequence[Mapping[str, Any]]]
]


class _FakeOpenVMMCiApi:
    """Answers the check's requests from the runs of _OPENVMM_REVISION and the
    jobs of each run, one look after another, raises each look that is an
    exception instead, and records each request."""

    def __init__(self, *looks: _OpenVMMLook | Exception) -> None:
        self.looks = list(looks)
        self.look = -1
        self.requests: list[str] = []

    def get_all(
        self,
        path: str,
        params: Mapping[str, str] | None = None,
        *,
        key: str | None = None,
        deadline: float | None = None,
    ) -> list[object]:
        assert deadline is None
        if path == "/repos/microsoft/openvmm/actions/runs":
            assert params == {"head_sha": _OPENVMM_REVISION}, params
            assert key == "workflow_runs", key
            self.look = min(self.look + 1, len(self.looks) - 1)
            self.requests.append("runs")
            look = self.looks[self.look]
            if isinstance(look, Exception):
                raise look
            return list(look[0])
        match = re.fullmatch(r"/repos/microsoft/openvmm/actions/runs/(\d+)/jobs", path)
        assert match is not None, path
        assert params == {"filter": "latest"} and key == "jobs", (params, key)
        self.requests.append(f"jobs {match.group(1)}")
        look = self.looks[self.look]
        assert not isinstance(look, Exception)
        return list(look[1][int(match.group(1))])


class OpenVMMCiTests(unittest.TestCase):
    """The check that OpenVMM's own CI passed the pinned OpenVMM revision
    (doc/usage.md#check-openvmm-ci)."""

    def test_a_required_job_is_a_whole_part_of_a_job_name(self):
        for name, job, covered in (
            ("clippy [macos], unit tests [x64-linux]", "unit tests [x64-linux]", True),
            (
                "clippy [x64-linux, macos], unit tests [x64-linux]",
                "unit tests [x64-linux]",
                True,
            ),
            (
                "clippy [x64-linux-musl, misc nostd], unit tests [x64-linux-musl]",
                "unit tests [x64-linux]",
                False,
            ),
            (
                "run vmm-tests [x64-windows-intel]",
                "run vmm-tests [x64-windows-intel]",
                True,
            ),
            (
                "run vmm-tests [x64-windows-intel-tdx]",
                "run vmm-tests [x64-windows-intel]",
                False,
            ),
            (
                "run vmm-perf [x64-linux-amd-kvm]",
                "run vmm-tests [x64-linux-amd-kvm]",
                False,
            ),
        ):
            with self.subTest(name=name, job=job):
                self.assertEqual(openvmm_ci.covers(name, job), covered)
        # Each required job is one of the x86-64 legs that cover what NVX
        # uses of OpenVMM: the unit tests and the VMM tests of each backend.
        self.assertEqual(
            openvmm_ci.OPENVMM_CI_REQUIRED_JOBS,
            (
                "unit tests [x64-linux]",
                "unit tests [x64-windows]",
                "run vmm-tests [x64-linux-amd-kvm]",
                "run vmm-tests [x64-linux-intel-mshv]",
                "run vmm-tests [x64-windows-intel]",
                "run vmm-tests [x64-windows-amd]",
            ),
        )

    def test_reads_only_the_checkin_gates_of_the_revision_in_openvmm(self):
        api = _FakeOpenVMMCiApi(
            (
                [
                    _openvmm_run(1, "openvmm-ci.yaml", status="in_progress"),
                    _openvmm_run(2),
                    # Other workflows, a fork's copy of the gates, and a run
                    # that GitHub lists for another head.
                    _openvmm_run(3, "openvmm-pr-release.yaml"),
                    _openvmm_run(4, "labeler.yml"),
                    _openvmm_run(5, head_repository="contributor/openvmm"),
                    _openvmm_run(6, head_sha="b" * 40),
                ],
                {
                    1: [_openvmm_job(1, "run vmm-tests [x64-linux-amd-kvm]")],
                    2: [
                        _openvmm_job(
                            2, "xtask fmt (windows)", "in_progress", conclusion=None
                        )
                    ],
                },
            )
        )

        runs = openvmm_ci.openvmm_ci_runs(api, _OPENVMM_REVISION)

        self.assertEqual(api.requests, ["runs", "jobs 1", "jobs 2"])
        self.assertEqual(
            runs,
            [
                openvmm_ci.OpenVMMRun(
                    url="https://github.com/microsoft/openvmm/actions/runs/1",
                    finished=False,
                    jobs=(
                        openvmm_ci.OpenVMMJob(
                            name="run vmm-tests [x64-linux-amd-kvm]",
                            status="completed",
                            conclusion="success",
                            url="https://github.com/microsoft/openvmm/actions/runs/1/job/1",
                        ),
                    ),
                ),
                openvmm_ci.OpenVMMRun(
                    url="https://github.com/microsoft/openvmm/actions/runs/2",
                    finished=True,
                    jobs=(
                        openvmm_ci.OpenVMMJob(
                            name="xtask fmt (windows)",
                            status="in_progress",
                            conclusion=None,
                            url="https://github.com/microsoft/openvmm/actions/runs/2/job/1",
                        ),
                    ),
                ),
            ],
        )
        # GitHub's answers must have the shape that the check reads.
        for runs in (cast(Any, [1]), [{**_openvmm_run(1), "id": "1"}]):
            with self.subTest(runs=runs), self.assertRaises(common.ScriptError):
                openvmm_ci.openvmm_ci_runs(
                    _FakeOpenVMMCiApi((runs, {})), _OPENVMM_REVISION
                )

    @staticmethod
    def _clock() -> tuple[Callable[[float], None], Callable[[], float], list[float]]:
        """Return a sleep that advances a clock instead, the clock, and the
        list of the sleeps."""
        now = [0.0]
        sleeps: list[float] = []

        def sleep(seconds: float) -> None:
            sleeps.append(seconds)
            now[0] += seconds

        return sleep, lambda: now[0], sleeps

    def test_any_run_that_passed_a_required_job_passes_it(self):
        def job(name: str, status: str, conclusion: str | None) -> Any:
            return openvmm_ci.OpenVMMJob(name, status, conclusion, f"url {name}")

        def run(finished: bool, *jobs: Any) -> Any:
            return openvmm_ci.OpenVMMRun(f"run {finished}", finished, tuple(jobs))

        # A pull request's run failed what the push's run passed, a job runs,
        # a job failed, and a job that no run ran.
        verdicts = openvmm_ci.assess_openvmm_ci(
            [
                run(
                    True,
                    job("passed elsewhere", "completed", "failure"),
                    job("failed", "completed", "failure"),
                    job("failed", "completed", "cancelled"),
                ),
                run(
                    True,
                    job("passed elsewhere", "completed", "success"),
                    job("running", "in_progress", None),
                ),
            ],
            ("passed elsewhere", "running", "failed", "absent"),
        )
        self.assertEqual(
            verdicts,
            [
                openvmm_ci.Verdict(
                    "passed elsewhere", "passed", "url passed elsewhere"
                ),
                openvmm_ci.Verdict("running", "pending", "url running"),
                openvmm_ci.Verdict(
                    "failed", "failed", "failure url failed, cancelled url failed"
                ),
                openvmm_ci.Verdict("absent", "missing", "no run of the gates ran it"),
            ],
        )
        # A run that has not finished may still create a job that it does not
        # list yet, once the jobs that the job needs finish.
        self.assertEqual(
            openvmm_ci.assess_openvmm_ci([run(False)], ("absent",)),
            [openvmm_ci.Verdict("absent", "pending", "run False")],
        )
        self.assertEqual(
            openvmm_ci.assess_openvmm_ci([], ("absent",)),
            [openvmm_ci.Verdict("absent", "missing", "no run of the gates ran it")],
        )

    def test_check_waits_while_a_required_job_may_still_pass(self):
        running = (
            [_openvmm_run(7, status="in_progress")],
            {7: _openvmm_gate_jobs(7, conclusion=None, status="in_progress")},
        )
        passed = (
            [_openvmm_run(7), _openvmm_run(8, "openvmm-ci.yaml")],
            {7: _openvmm_gate_jobs(7), 8: _openvmm_gate_jobs(8, "failure")},
        )
        api = _FakeOpenVMMCiApi(running, running, passed)
        sleep, clock, sleeps = self._clock()

        with contextlib.redirect_stdout(io.StringIO()) as output:
            verdicts = openvmm_ci.check_openvmm_ci(
                api,
                _OPENVMM_REVISION,
                wait_seconds=600,
                sleep=sleep,
                clock=clock,
            )

        self.assertEqual(sleeps, [openvmm_ci.POLL_SECONDS] * 2)
        self.assertEqual(api.requests.count("runs"), 3)
        self.assertEqual({verdict.state for verdict in verdicts}, {"passed"})
        self.assertEqual(
            [verdict.job for verdict in verdicts],
            list(openvmm_ci.OPENVMM_CI_REQUIRED_JOBS),
        )
        self.assertIn(
            f">> waiting for OpenVMM's CI of {_OPENVMM_REVISION}: "
            "run vmm-tests [x64-linux-amd-kvm], ",
            output.getvalue(),
        )

    def test_check_stops_waiting_once_a_job_cannot_pass_or_time_runs_out(self):
        running = (
            [_openvmm_run(7, status="in_progress")],
            {7: _openvmm_gate_jobs(7, conclusion=None, status="in_progress")},
        )
        failed = ([_openvmm_run(7)], {7: _openvmm_gate_jobs(7, "failure")})
        nothing: _OpenVMMLook = ([], {})
        for look, wait_seconds, polls in (
            # A failed or missing job never passes, so the check doesn't wait.
            (failed, 600, 1),
            (nothing, 600, 1),
            # Pending jobs wait for as long as another look fits in the time.
            (running, 0, 1),
            (running, openvmm_ci.POLL_SECONDS * 2.5, 3),
        ):
            api = _FakeOpenVMMCiApi(look)
            sleep, clock, _ = self._clock()
            with (
                self.subTest(look=look, wait_seconds=wait_seconds),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                verdicts = openvmm_ci.check_openvmm_ci(
                    api,
                    _OPENVMM_REVISION,
                    wait_seconds=wait_seconds,
                    sleep=sleep,
                    clock=clock,
                )
                self.assertEqual(api.requests.count("runs"), polls)
                self.assertNotIn(
                    "passed",
                    {
                        verdict.state
                        for verdict in verdicts
                        if verdict.job.startswith("run vmm-tests")
                    },
                )

        # A job that failed fails the check whatever the jobs that still run
        # do, so the check doesn't wait for them either.
        failing: _OpenVMMLook = (
            [_openvmm_run(7, status="in_progress")],
            {
                7: [
                    {**job, "conclusion": "failure"}
                    if "unit tests [x64-windows]" in job["name"]
                    else job
                    for job in _openvmm_gate_jobs(
                        7, conclusion=None, status="in_progress"
                    )
                ]
            },
        )
        api = _FakeOpenVMMCiApi(failing)
        sleep, clock, sleeps = self._clock()
        with contextlib.redirect_stdout(io.StringIO()) as output:
            verdicts = openvmm_ci.check_openvmm_ci(
                api, _OPENVMM_REVISION, wait_seconds=600, sleep=sleep, clock=clock
            )
        self.assertEqual(sleeps, [])
        self.assertEqual(api.requests.count("runs"), 1)
        self.assertNotIn(">> waiting", output.getvalue())
        self.assertEqual(
            {verdict.job: verdict.state for verdict in verdicts},
            {
                "unit tests [x64-linux]": "passed",
                "unit tests [x64-windows]": "failed",
                **{
                    job: "pending"
                    for job in openvmm_ci.OPENVMM_CI_REQUIRED_JOBS
                    if job.startswith("run vmm-tests")
                },
            },
        )

    def test_check_looks_again_after_a_failed_look_while_it_may_wait(self):
        passed: _OpenVMMLook = ([_openvmm_run(7)], {7: _openvmm_gate_jobs(7)})
        # GitHub's message may hold a newline, which would end a warning.
        unavailable = common.ScriptError(
            "GitHub GET /repos/microsoft/openvmm/actions/runs failed:\ntimed out"
        )
        api = _FakeOpenVMMCiApi(unavailable, unavailable, passed)
        sleep, clock, sleeps = self._clock()
        with contextlib.redirect_stdout(io.StringIO()) as output:
            verdicts = openvmm_ci.check_openvmm_ci(
                api, _OPENVMM_REVISION, wait_seconds=3600, sleep=sleep, clock=clock
            )
        self.assertEqual(sleeps, [openvmm_ci.POLL_SECONDS] * 2)
        self.assertEqual({verdict.state for verdict in verdicts}, {"passed"})
        warnings = [
            line
            for line in output.getvalue().splitlines()
            if line.startswith("::warning::")
        ]
        self.assertEqual(
            warnings,
            [
                f"::warning::could not read OpenVMM's CI of {_OPENVMM_REVISION}, so "
                "the check looks again: GitHub GET /repos/microsoft/openvmm/actions/"
                "runs failed:%0Atimed out"
            ]
            * 2,
        )
        # A failure that persists, or that leaves no time to look again, ends
        # the check.
        cases: tuple[tuple[tuple[_OpenVMMLook | Exception, ...], float, int], ...] = (
            (
                (unavailable,) * (openvmm_ci.FAILED_LOOK_RETRIES + 1) + (passed,),
                3600,
                openvmm_ci.FAILED_LOOK_RETRIES + 1,
            ),
            ((unavailable, passed), 0, 1),
        )
        for looks, wait_seconds, polls in cases:
            api = _FakeOpenVMMCiApi(*looks)
            sleep, clock, _ = self._clock()
            with self.subTest(looks=len(looks), wait_seconds=wait_seconds):
                with (
                    contextlib.redirect_stdout(io.StringIO()),
                    self.assertRaises(common.ScriptError) as raised,
                ):
                    openvmm_ci.check_openvmm_ci(
                        api,
                        _OPENVMM_REVISION,
                        wait_seconds=wait_seconds,
                        sleep=sleep,
                        clock=clock,
                    )
                self.assertIs(raised.exception, unavailable)
                self.assertEqual(api.requests.count("runs"), polls)

    def test_command_reports_each_verdict_and_fails_unless_every_job_passed(self):
        passed = ([_openvmm_run(7)], {7: _openvmm_gate_jobs(7)})
        failed = ([_openvmm_run(7)], {7: _openvmm_gate_jobs(7, "failure")})
        for look, succeeds in ((passed, True), (failed, False)):
            with (
                self.subTest(succeeds=succeeds),
                tempfile.TemporaryDirectory() as temporary,
            ):
                summary = Path(temporary) / "summary.md"
                args = nvx.parse_args(
                    [
                        "check-openvmm-ci",
                        "--revision",
                        _OPENVMM_REVISION,
                        "--summary",
                        os.fspath(summary),
                    ]
                )
                api = _FakeOpenVMMCiApi(look)
                with (
                    patch.object(
                        openvmm_ci, "GitHubClient", return_value=api
                    ) as client,
                    patch.dict(
                        os.environ,
                        {
                            "GH_TOKEN": "token",
                            "GITHUB_API_URL": "https://api.example",
                        },
                    ),
                    contextlib.redirect_stdout(io.StringIO()) as output,
                ):
                    if succeeds:
                        args.handler(args)
                    else:
                        with self.assertRaisesRegex(
                            common.ScriptError,
                            f"OpenVMM's CI has not passed 4 required job\\(s\\) for "
                            f"{_OPENVMM_REVISION}",
                        ):
                            args.handler(args)
                client.assert_called_once_with("token", "https://api.example")
                lines = output.getvalue().splitlines()
                errors = [line for line in lines if line.startswith("::error::")]
                self.assertEqual(len(errors), 0 if succeeds else 4)
                for line in errors:
                    self.assertRegex(
                        line,
                        r"^::error::OpenVMM's CI has not passed run vmm-tests "
                        r"\[[a-z0-9-]+\] \(failed: failure https://\S+\)$",
                    )
                self.assertIn(
                    "passed: unit tests [x64-linux]: "
                    "https://github.com/microsoft/openvmm/actions/runs/7/job/1",
                    lines,
                )
                text = summary.read_text(encoding="utf-8")
                self.assertTrue(
                    text.startswith(
                        f"### OpenVMM CI of `{_OPENVMM_REVISION}`\n"
                        "| Job | Result | Evidence |\n"
                    )
                )
                self.assertIn(
                    "| `run vmm-tests [x64-windows-amd]` | "
                    f"{'passed' if succeeds else 'failed'} |",
                    text,
                )

    def test_command_checks_the_pin_with_any_token(self):
        args = nvx.parse_args(["check-openvmm-ci"])
        api = _FakeOpenVMMCiApi(([_openvmm_run(7)], {7: _openvmm_gate_jobs(7)}))
        with (
            patch.object(
                openvmm_ci, "git_output", return_value=_OPENVMM_REVISION
            ) as git_output,
            patch.object(openvmm_ci, "GitHubClient", return_value=api) as client,
            patch.dict(os.environ, {"GH_TOKEN": "", "GITHUB_TOKEN": ""}),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            args.handler(args)
        git_output.assert_called_once_with("rev-parse", ":openvmm")
        # The repository is public, so the check reads it without a token too.
        self.assertIsNone(client.call_args.args[0])
        for revision in ("3d7512af6", "main", _OPENVMM_REVISION.upper()):
            with (
                self.subTest(revision=revision),
                patch.object(openvmm_ci, "GitHubClient") as client,
                self.assertRaisesRegex(
                    common.ScriptError, "is no full OpenVMM commit SHA"
                ),
            ):
                openvmm_ci.command_check_openvmm_ci(
                    nvx.parse_args(["check-openvmm-ci", "--revision", revision])
                )
            client.assert_not_called()


if __name__ == "__main__":
    unittest.main()
