# pyright: reportPrivateUsage=false
from __future__ import annotations

import argparse
import base64
import ctypes
import hashlib
import json
import os
import socket
import sys
import tempfile
import time
import unittest
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import cast
from unittest.mock import MagicMock, patch

from nvx_tools.adversarial import (
    LOCAL_EXECUTOR_STATE_ROOT,
    CampaignConfig,
    CopilotController,
    ExecutorClient,
    ExecutorUnavailableError,
    _action_failure_fingerprint,
    _finalize_executor,
    _initialize_executor,
    _minimize_failure,
    _positive_float,
    _prefix_reproduces,
    _validate_replay_manifest,
    local_executor_environment,
    run_campaign,
)
from nvx_tools.adversarial_broker import (
    CAMPAIGNS,
    ActionResult,
    AdversarialAction,
    CaseDefinition,
    TypedBroker,
    catalog_digest,
    load_campaign,
    read_replay,
)
from nvx_tools.adversarial_executor import (
    AdversarialExecutor,
    _InitializedSession,
    _inventory_outcomes,
    _live_openvmm_pids,
    _process_running,
    sanitized_environment,
)
from nvx_tools.adversarial_oracles import (
    BoundedProcessResult,
    OracleSession,
    _WindowsJob,
    run_bounded_process,
)
from nvx_tools.benchmark import InteractiveProcess
from nvx_tools.common import ScriptError
from nvx_tools.release import PROJECT_SOURCE_PATHS


def _executor_result(
    *,
    sequence: int = 1,
    case_id: str = "workload-identity-boundary",
    category: str = "passed",
    returncode: int | None = 0,
    timed_out: bool = False,
    host_canary_intact: bool = True,
    network_canary_connections: int = 0,
    teardown_complete: bool = True,
) -> dict[str, object]:
    return {
        "schema_version": 1,
        "operation": "execute",
        "sequence": sequence,
        "case_id": case_id,
        "category": category,
        "returncode": returncode,
        "timed_out": timed_out,
        "duration_ms": 12,
        "stdout_b64": base64.b64encode(b"untrusted stdout").decode(),
        "stderr_b64": "",
        "stdout_truncated": False,
        "stderr_truncated": False,
        "host_canary_intact": host_canary_intact,
        "network_canary_connections": network_canary_connections,
        "teardown_complete": teardown_complete,
        "artifact_path": "target/case",
        "error": None,
    }


def _boot_evidence(
    *,
    category: str = "passed",
    returncode: int | None = 0,
    timed_out: bool = False,
    teardown_complete: bool = True,
    openvmm_pids: list[int] | None = None,
    live_openvmm_pids: list[int] | None = None,
) -> dict[str, object]:
    return {
        "category": category,
        "returncode": returncode,
        "timed_out": timed_out,
        "duration_ms": 1,
        "teardown_complete": teardown_complete,
        "openvmm_pids": [42] if openvmm_pids is None else openvmm_pids,
        "live_openvmm_pids": ([] if live_openvmm_pids is None else live_openvmm_pids),
        "artifact_path": "target/post-campaign",
    }


def _write_replay_manifest(replay: Path, config: CampaignConfig) -> None:
    replay_bytes = replay.read_bytes()
    cases = load_campaign(config.campaign)
    (replay.parent / "replay-manifest.json").write_text(
        json.dumps(
            {
                "schema_version": 1,
                "campaign": config.campaign,
                "backend": config.backend,
                "host_type": config.host_type,
                "seed": config.seed,
                "budgets": {
                    "seconds": config.budget_seconds,
                    "actions": config.budget_actions,
                },
                "memory_mib": config.memory_mib,
                "phase_timeout_seconds": config.phase_timeout,
                "action_timeout_seconds": config.action_timeout,
                "external_executor": config.executor_command is not None,
                "case_catalog_sha256": catalog_digest(cases.values()),
                "files": {
                    replay.name: {
                        "sha256": hashlib.sha256(replay_bytes).hexdigest(),
                        "size": len(replay_bytes),
                    }
                },
            }
        ),
        encoding="utf-8",
    )


def _fake_executor_client_type(
    *,
    escape: bool,
    quarantine_requests: list[bool],
    unavailable_on_execute: bool = False,
    malformed_on_execute: bool = False,
    fail_quarantine: bool = False,
    late_escape: bool = False,
    abort_requests: list[bool] | None = None,
) -> type[object]:
    class FakeExecutorClient:
        def __init__(
            self,
            *,
            run_dir: Path,
            command: str | None,
            label: str,
        ) -> None:
            del run_dir, command, label

        def request(
            self,
            request: Mapping[str, object],
            *,
            timeout: float,
        ) -> dict[str, object]:
            del timeout
            operation = request["operation"]
            if operation == "initialize":
                return {
                    "schema_version": 1,
                    "operation": "initialize",
                    "status": "passed",
                    "run_id": request["run_id"],
                    "backend": request["backend"],
                    "host_type": request["host_type"],
                    "metadata": {
                        "nvx_commit": request["expected_commit"],
                        "nvx_dirty": False,
                        "nvx_status": [],
                        "openvmm_commit": request["expected_openvmm_commit"],
                        "openvmm_dirty": False,
                        "openvmm_status": [],
                        "os": "Windows",
                        "os_release": "test",
                        "machine": "AMD64",
                        "python": "3.10.0",
                    },
                    "artifacts": {
                        name: {
                            "path": f"target/{name}",
                            "sha256": "a" * 64,
                            "size": 1,
                        }
                        for name in (
                            "kernel",
                            "initramfs",
                            "initramfs_package_manifest",
                            "initramfs_provenance",
                            "openvmm",
                            "kernel_provenance",
                            "openvmm_provenance",
                        )
                    },
                    "verify": {
                        "returncode": 0,
                        "timed_out": False,
                        "duration_ms": 1,
                    },
                    "baseline": {
                        "category": "passed",
                        "returncode": 0,
                        "timed_out": False,
                        "duration_ms": 1,
                        "teardown_complete": True,
                        "openvmm_pids": [42],
                        "live_openvmm_pids": [],
                        "artifact_path": "target/baseline",
                    },
                    "oracles": {
                        "host_canary_sha256": "b" * 64,
                        "network_canary_port": 12345,
                        "host_canary_intact": True,
                        "network_canary_connections": 0,
                    },
                    "artifact_root": "target/run",
                }
            if operation == "execute":
                if unavailable_on_execute:
                    raise ExecutorUnavailableError("target disappeared")
                if malformed_on_execute:
                    return {"schema_version": 1, "operation": "execute"}
                return _executor_result(
                    sequence=cast(int, request["sequence"]),
                    case_id=cast(str, request["case_id"]),
                    category="policy-violation" if escape else "passed",
                    host_canary_intact=not escape,
                )
            if operation == "finalize":
                quarantine = cast(bool, request["quarantine"])
                quarantine_requests.append(quarantine)
                if quarantine and fail_quarantine:
                    raise ScriptError("quarantine failed")
                late_quarantine = late_escape and not quarantine
                return {
                    "schema_version": 1,
                    "operation": "finalize",
                    "status": (
                        "quarantined" if quarantine or late_quarantine else "passed"
                    ),
                    "run_id": request["run_id"],
                    "post_campaign": (
                        None
                        if quarantine
                        else {
                            "category": "passed",
                            "returncode": 0,
                            "timed_out": False,
                            "duration_ms": 1,
                            "teardown_complete": True,
                            "openvmm_pids": [43],
                            "live_openvmm_pids": [],
                            "artifact_path": "target/post-campaign",
                        }
                    ),
                    "oracles": {
                        "host_canary_intact": not late_quarantine,
                        "network_canary_connections": int(late_quarantine),
                        "watchdog_error": None,
                        "network_canary_error": None,
                    },
                    "artifact_root": "target/run",
                }
            raise AssertionError(f"unexpected operation: {operation}")

        def abort(self) -> None:
            if abort_requests is not None:
                abort_requests.append(True)

        def __enter__(self) -> FakeExecutorClient:
            return self

        def __exit__(
            self,
            _exception_type: object,
            _exception: object,
            _traceback: object,
        ) -> None:
            return None

    return FakeExecutorClient


class AdversarialBrokerTests(unittest.TestCase):
    def test_campaign_catalogs_are_strict_and_nonempty(self) -> None:
        for campaign in CAMPAIGNS:
            with self.subTest(campaign=campaign):
                cases = load_campaign(campaign)
                self.assertTrue(cases)
                self.assertEqual(
                    len(cases),
                    len({case.case_id for case in cases.values()}),
                )
                self.assertTrue(
                    all(case.campaign == campaign for case in cases.values())
                )

    def test_campaign_rejects_boolean_schema_version(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "workload-isolation.json"
            document = json.loads(
                (
                    Path(__file__).parent
                    / "nvx_tools"
                    / "adversarial_cases"
                    / "workload-isolation.json"
                ).read_text(encoding="utf-8")
            )
            document["schema_version"] = True
            path.write_text(json.dumps(document), encoding="utf-8")
            with (
                patch(
                    "nvx_tools.adversarial_broker.campaign_file",
                    return_value=path,
                ),
                self.assertRaisesRegex(ScriptError, "must be an integer"),
            ):
                load_campaign("workload-isolation")

    def test_action_rejects_extra_duplicate_unknown_and_repeated_fields(self) -> None:
        cases = load_campaign("workload-isolation")
        case_id = next(iter(cases))
        valid = json.dumps({"schema_version": 1, "case_id": case_id}).encode()
        action = AdversarialAction.from_response(
            valid,
            cases=cases,
            completed=set(),
        )
        self.assertEqual(action.case_id, case_id)
        with self.assertRaisesRegex(ScriptError, "unexpected"):
            AdversarialAction.from_response(
                json.dumps(
                    {
                        "schema_version": 1,
                        "case_id": case_id,
                        "command": "whoami",
                    }
                ).encode(),
                cases=cases,
                completed=set(),
            )
        with self.assertRaisesRegex(ScriptError, "duplicate"):
            AdversarialAction.from_response(
                (
                    '{"schema_version":1,"case_id":"'
                    + case_id
                    + '","case_id":"'
                    + case_id
                    + '"}'
                ).encode(),
                cases=cases,
                completed=set(),
            )
        with self.assertRaisesRegex(ScriptError, "unknown"):
            AdversarialAction.from_response(
                b'{"schema_version":1,"case_id":"not-a-case"}',
                cases=cases,
                completed=set(),
            )
        with self.assertRaisesRegex(ScriptError, "repeats"):
            AdversarialAction.from_response(
                valid,
                cases=cases,
                completed={case_id},
            )
        with self.assertRaisesRegex(ScriptError, "non-standard JSON"):
            AdversarialAction.from_response(
                b'{"schema_version":NaN,"case_id":"not-a-case"}',
                cases=cases,
                completed=set(),
            )
        with self.assertRaisesRegex(ScriptError, "must be an integer"):
            AdversarialAction.from_response(
                json.dumps({"schema_version": True, "case_id": case_id}).encode(),
                cases=cases,
                completed=set(),
            )

    def test_broker_records_action_before_dispatch_and_encodes_observation(
        self,
    ) -> None:
        cases = load_campaign("workload-isolation")
        case_id = next(iter(cases))
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            actions_path = root / "actions.jsonl"
            observations_path = root / "observations.jsonl"

            def execute(
                case: CaseDefinition,
                sequence: int,
                _timeout: float,
            ) -> Mapping[str, object]:
                self.assertTrue(actions_path.is_file())
                self.assertEqual(case.case_id, case_id)
                return _executor_result(
                    sequence=sequence,
                    case_id=case.case_id,
                )

            broker = TypedBroker(
                cases=cases,
                actions_path=actions_path,
                observations_path=observations_path,
                execute=execute,
            )
            result = broker.execute(
                AdversarialAction(case_id),
                timeout=10.0,
            )
            self.assertFalse(result.anomaly)
            prompt = result.prompt_object(cases[case_id])
            encoded = cast(dict[str, object], prompt["guest_output"])
            self.assertNotIn("untrusted stdout", json.dumps(prompt))
            self.assertEqual(
                base64.b64decode(cast(str, encoded["stdout"])),
                b"untrusted stdout",
            )
            self.assertEqual(len(actions_path.read_text().splitlines()), 1)
            self.assertEqual(len(observations_path.read_text().splitlines()), 1)

    def test_oracle_faults_are_campaign_anomalies(self) -> None:
        faults: Sequence[dict[str, object]] = (
            _executor_result(
                category="policy-violation",
                host_canary_intact=False,
            ),
            _executor_result(
                category="policy-violation",
                network_canary_connections=1,
            ),
            _executor_result(
                category="availability-failure",
                teardown_complete=False,
            ),
            _executor_result(
                category="timeout",
                returncode=None,
                timed_out=True,
            ),
        )
        for value in faults:
            with self.subTest(value=value):
                result = ActionResult.from_object(
                    value,
                    expected_sequence=1,
                    expected_case="workload-identity-boundary",
                )
                self.assertTrue(result.anomaly)
                if (
                    value["host_canary_intact"] is False
                    or value["network_canary_connections"] != 0
                ):
                    self.assertTrue(result.suspected_escape)

    def test_passed_result_cannot_report_incomplete_teardown(self) -> None:
        with self.assertRaisesRegex(ScriptError, "contradicts"):
            ActionResult.from_object(
                _executor_result(teardown_complete=False),
                expected_sequence=1,
                expected_case="workload-identity-boundary",
            )

    def test_failure_fingerprint_preserves_category_case_and_escape(self) -> None:
        timeout = ActionResult.from_object(
            _executor_result(
                category="timeout",
                returncode=None,
                timed_out=True,
            ),
            expected_sequence=1,
            expected_case="workload-identity-boundary",
        )
        escape = ActionResult.from_object(
            _executor_result(
                category="policy-violation",
                host_canary_intact=False,
            ),
            expected_sequence=1,
            expected_case="workload-identity-boundary",
        )
        self.assertEqual(
            _action_failure_fingerprint(timeout),
            ("timeout", "workload-identity-boundary", False),
        )
        self.assertEqual(
            _action_failure_fingerprint(escape),
            ("policy-violation", "workload-identity-boundary", True),
        )

    def test_replay_accepts_only_ordered_recorded_actions(self) -> None:
        cases = load_campaign("workload-isolation")
        selected = list(cases)[:2]
        with tempfile.TemporaryDirectory() as temporary:
            replay = Path(temporary) / "actions.jsonl"
            replay.write_text(
                "\n".join(
                    json.dumps(
                        {
                            "schema_version": 1,
                            "sequence": sequence,
                            "case_id": case_id,
                        }
                    )
                    for sequence, case_id in enumerate(selected, start=1)
                )
                + "\n",
                encoding="utf-8",
            )
            actions = read_replay(replay, cases=cases)
            self.assertEqual([action.case_id for action in actions], selected)
            replay.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "sequence": 2,
                        "case_id": selected[0],
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ScriptError, "expected 1"):
                read_replay(replay, cases=cases)


class AdversarialOracleTests(unittest.TestCase):
    def test_process_timeout_bounds_blocked_stdin_writer(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            result = run_bounded_process(
                [
                    sys.executable,
                    "-c",
                    "import time; time.sleep(2)",
                ],
                cwd=Path.cwd(),
                output_dir=Path(temporary) / "process",
                timeout=0.1,
                environment={},
                stdin_data=b"x" * (2 * 1024 * 1024),
            )
        self.assertTrue(result.timed_out)
        self.assertTrue(result.teardown_complete)
        self.assertLess(result.duration_ms, 1500)

    def test_contained_process_timeout_kills_descendants(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            child_pid_path = root / "child.pid"
            result = run_bounded_process(
                [
                    sys.executable,
                    "-c",
                    (
                        "import pathlib,subprocess,sys,time; "
                        "child=subprocess.Popen([sys.executable,'-c',"
                        "'import time; time.sleep(60)']); "
                        "pathlib.Path(sys.argv[1]).write_text(str(child.pid)); "
                        "time.sleep(60)"
                    ),
                    str(child_pid_path),
                ],
                cwd=Path.cwd(),
                output_dir=root / "process",
                timeout=1.0,
                environment=os.environ,
                contained_by_parent=True,
            )
            child_pid = int(child_pid_path.read_text(encoding="utf-8"))
            deadline = time.monotonic() + 2.0
            while _process_running(child_pid) and time.monotonic() < deadline:
                time.sleep(0.025)
        self.assertTrue(result.timed_out)
        self.assertTrue(result.teardown_complete)
        self.assertFalse(_process_running(child_pid))

    def test_contained_process_normal_exit_kills_descendants(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            child_pid_path = root / "child.pid"
            result = run_bounded_process(
                [
                    sys.executable,
                    "-c",
                    (
                        "import pathlib,subprocess,sys; "
                        "child=subprocess.Popen([sys.executable,'-c',"
                        "'import time; time.sleep(60)'], "
                        "stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, "
                        "stderr=subprocess.DEVNULL); "
                        "pathlib.Path(sys.argv[1]).write_text(str(child.pid))"
                    ),
                    str(child_pid_path),
                ],
                cwd=Path.cwd(),
                output_dir=root / "process",
                timeout=5.0,
                environment=os.environ,
                contained_by_parent=True,
            )
            child_pid = int(child_pid_path.read_text(encoding="utf-8"))
            deadline = time.monotonic() + 2.0
            while _process_running(child_pid) and time.monotonic() < deadline:
                time.sleep(0.025)
        self.assertFalse(result.timed_out)
        self.assertEqual(result.returncode, 0)
        self.assertTrue(result.teardown_complete)
        self.assertFalse(_process_running(child_pid))

    def test_failed_windows_job_assignment_does_not_resume_process(self) -> None:
        job = _WindowsJob.__new__(_WindowsJob)
        kernel32 = MagicMock()
        kernel32.AssignProcessToJobObject.return_value = 0
        job._kernel32 = kernel32
        job._handle = 123
        job._get_last_error = lambda: 5
        job._win_dll = MagicMock()
        process = MagicMock()
        process._handle = 456

        with self.assertRaisesRegex(OSError, "AssignProcessToJobObject"):
            job.assign_and_resume(process)

        job._win_dll.assert_not_called()

    def test_live_openvmm_pids_are_reported(self) -> None:
        def process_running(pid: int) -> bool:
            return pid == 22

        with patch(
            "nvx_tools.adversarial_executor._process_running",
            side_effect=process_running,
        ):
            self.assertEqual(_live_openvmm_pids((11, 22, 33)), (22,))

    def test_windows_process_access_denied_is_conservatively_live(self) -> None:
        kernel32 = MagicMock()
        kernel32.OpenProcess.return_value = 0
        with (
            patch("nvx_tools.adversarial_executor.os.name", "nt"),
            patch.object(ctypes, "WinDLL", return_value=kernel32, create=True),
            patch.object(ctypes, "get_last_error", return_value=5, create=True),
        ):
            self.assertTrue(_process_running(42))

    def test_required_outcome_report_cannot_be_absent(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            reports, teardown_complete = _inventory_outcomes(
                Path(temporary),
                required_names=frozenset({"managed-outcome.json"}),
            )
        self.assertFalse(teardown_complete)
        self.assertEqual(
            reports,
            [{"path": "managed-outcome.json", "missing": True}],
        )

    def test_openvmm_pid_is_flushed_to_opt_in_journal(self) -> None:
        process = MagicMock()
        process.pid = 4321
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            journal = root / "openvmm-pids.jsonl"
            with (
                patch("nvx_tools.benchmark.sys.platform", "win32"),
                patch(
                    "nvx_tools.benchmark.subprocess.Popen",
                    return_value=process,
                ),
            ):
                InteractiveProcess(
                    ["openvmm"],
                    {"NVX_ADVERSARIAL_OPENVMM_PID_JOURNAL": str(journal)},
                )
            records = [
                json.loads(line)
                for line in journal.read_text(encoding="utf-8").splitlines()
            ]
        self.assertEqual([record["pid"] for record in records], [4321])
        self.assertIsInstance(records[0]["recorded_at_ns"], int)

    def test_openvmm_pid_journal_failure_cleans_up_process(self) -> None:
        process = MagicMock()
        process.pid = 4321
        with tempfile.TemporaryDirectory() as temporary:
            missing = Path(temporary) / "missing" / "openvmm-pids.jsonl"
            with (
                patch(
                    "nvx_tools.benchmark.subprocess.Popen",
                    return_value=process,
                ),
                patch("nvx_tools.benchmark.sys.platform", "win32"),
                patch("nvx_tools.benchmark.terminate") as terminate,
                self.assertRaises(OSError),
            ):
                InteractiveProcess(
                    ["openvmm"],
                    {"NVX_ADVERSARIAL_OPENVMM_PID_JOURNAL": str(missing)},
                )
        terminate.assert_called_once_with(process)

    def test_filesystem_and_network_canaries_detect_policy_violations(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            session = OracleSession.create(
                Path(temporary) / "oracles",
                seed=1234,
                run_id="unit-test",
            )
            try:
                with socket.create_connection(
                    ("127.0.0.1", session.network_canary.port),
                    timeout=2.0,
                ):
                    pass
                session.canary_path.write_bytes(b"modified")
                sample = session.watchdog.sample(event="unit-test")
                self.assertFalse(sample["host_canary_intact"])
            finally:
                result = session.close()
            self.assertFalse(result["host_canary_intact"])
            self.assertGreaterEqual(
                cast(int, result["network_canary_connections"]),
                1,
            )

    def test_oracle_cleanup_runs_when_final_sample_fails(self) -> None:
        watchdog = MagicMock()
        watchdog.sample.side_effect = OSError("disk full")
        network_canary = MagicMock()
        session = OracleSession(
            root=Path("oracles"),
            canary_path=Path("canary"),
            canary_hash="hash",
            network_canary=network_canary,
            watchdog=watchdog,
        )
        with self.assertRaisesRegex(ScriptError, "final watchdog sample failed"):
            session.close()
        watchdog.close.assert_called_once_with()
        network_canary.close.assert_called_once_with()


class AdversarialCampaignTests(unittest.TestCase):
    def test_adaptive_campaign_reserves_preflight_and_action_credits(self) -> None:
        config = CampaignConfig(
            backend="whp",
            campaign="workload-isolation",
            budget_seconds=300.0,
            budget_actions=1,
            budget_ai_credits=30,
            seed=17,
            output_dir=Path("output"),
            replay=None,
            model=None,
            host_type="virtual-machine",
            memory_mib=256,
            phase_timeout=1.0,
            action_timeout=10.0,
            executor_command=None,
            minimize=False,
            minimize_attempts=1,
        )
        with self.assertRaisesRegex(ScriptError, "at least 60"):
            run_campaign(config)

    def test_minimization_retains_the_failing_action(self) -> None:
        cases = load_campaign("workload-isolation")
        selected = list(cases)[:3]
        actions = tuple(AdversarialAction(case_id) for case_id in selected)
        with tempfile.TemporaryDirectory() as temporary:
            config = CampaignConfig(
                backend="whp",
                campaign="workload-isolation",
                budget_seconds=300.0,
                budget_actions=3,
                budget_ai_credits=1,
                seed=17,
                output_dir=Path(temporary) / "output",
                replay=None,
                model=None,
                host_type="virtual-machine",
                memory_mib=256,
                phase_timeout=1.0,
                action_timeout=10.0,
                executor_command=None,
                minimize=True,
                minimize_attempts=2,
            )
            with patch(
                "nvx_tools.adversarial._prefix_reproduces",
                return_value=True,
            ) as reproduce:
                minimized = _minimize_failure(
                    config=config,
                    run_dir=Path(temporary),
                    original_run_id="run-id",
                    controller_commit="a" * 40,
                    controller_openvmm_commit="b" * 40,
                    actions=actions,
                    deadline=time.monotonic() + 1000.0,
                    expected_category="timeout",
                    expected_case=selected[-1],
                    expected_suspected_escape=False,
                )
        attempted = [
            tuple(action.case_id for action in call.kwargs["actions"])
            for call in reproduce.call_args_list
        ]
        self.assertEqual(attempted, [tuple(selected[1:]), (selected[-1],)])
        self.assertEqual(tuple(action.case_id for action in minimized), (selected[-1],))

    def test_minimization_reproduces_late_quarantine_as_policy_violation(
        self,
    ) -> None:
        cases = load_campaign("workload-isolation")
        case_id = next(iter(cases))
        quarantine_requests: list[bool] = []
        fake_executor_client = _fake_executor_client_type(
            escape=False,
            quarantine_requests=quarantine_requests,
            late_escape=True,
        )
        config = CampaignConfig(
            backend="whp",
            campaign="workload-isolation",
            budget_seconds=300.0,
            budget_actions=1,
            budget_ai_credits=1,
            seed=17,
            output_dir=Path("output"),
            replay=None,
            model=None,
            host_type="virtual-machine",
            memory_mib=256,
            phase_timeout=1.0,
            action_timeout=10.0,
            executor_command=None,
            minimize=True,
            minimize_attempts=1,
        )
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch(
                "nvx_tools.adversarial.ExecutorClient",
                fake_executor_client,
            ),
        ):
            reproduced = _prefix_reproduces(
                config=config,
                run_dir=Path(temporary),
                run_id="minimize-run",
                controller_commit="a" * 40,
                controller_openvmm_commit="b" * 40,
                actions=(AdversarialAction(case_id),),
                deadline=time.monotonic() + 1000.0,
                attempt=1,
                expected_category="policy-violation",
                expected_case=None,
                expected_suspected_escape=True,
            )
        self.assertTrue(reproduced)
        self.assertEqual(quarantine_requests, [False])

    def test_initialize_rejects_missing_baseline_evidence(self) -> None:
        client = MagicMock()
        client.request.return_value = {
            "schema_version": 1,
            "operation": "initialize",
            "status": "passed",
            "run_id": "run-id",
        }
        config = CampaignConfig(
            backend="whp",
            campaign="workload-isolation",
            budget_seconds=300.0,
            budget_actions=1,
            budget_ai_credits=1,
            seed=17,
            output_dir=Path("output"),
            replay=None,
            model=None,
            host_type="virtual-machine",
            memory_mib=256,
            phase_timeout=1.0,
            action_timeout=10.0,
            executor_command=None,
            minimize=False,
            minimize_attempts=1,
        )
        with self.assertRaisesRegex(ScriptError, "initialize response shape"):
            _initialize_executor(
                client,
                config=config,
                run_id="run-id",
                controller_commit="a" * 40,
                controller_openvmm_commit="b" * 40,
                timeout=10.0,
            )

    def test_finalize_rejects_unsolicited_quarantine(self) -> None:
        client = MagicMock()
        client.request.return_value = {
            "schema_version": 1,
            "operation": "finalize",
            "status": "quarantined",
            "run_id": "run-id",
            "post_campaign": None,
            "oracles": {
                "host_canary_intact": True,
                "network_canary_connections": 0,
                "watchdog_error": None,
                "network_canary_error": None,
            },
            "artifact_root": "target/run",
        }
        with self.assertRaisesRegex(ScriptError, "clean-finalize status"):
            _finalize_executor(
                client,
                run_id="run-id",
                timeout=10.0,
                quarantine=False,
            )

    def test_requested_quarantine_accepts_violation_evidence(self) -> None:
        client = MagicMock()
        client.request.return_value = {
            "schema_version": 1,
            "operation": "finalize",
            "status": "quarantined",
            "run_id": "run-id",
            "post_campaign": None,
            "oracles": {
                "host_canary_intact": False,
                "network_canary_connections": 1,
                "watchdog_error": None,
                "network_canary_error": None,
            },
            "artifact_root": "target/run",
        }
        response = _finalize_executor(
            client,
            run_id="run-id",
            timeout=10.0,
            quarantine=True,
        )
        self.assertEqual(response["status"], "quarantined")

    def test_finalize_rejects_missing_post_campaign_evidence(self) -> None:
        client = MagicMock()
        client.request.return_value = {
            "schema_version": 1,
            "operation": "finalize",
            "status": "passed",
            "run_id": "run-id",
            "post_campaign": None,
            "oracles": {
                "host_canary_intact": True,
                "network_canary_connections": 0,
                "watchdog_error": None,
                "network_canary_error": None,
            },
            "artifact_root": "target/run",
        }
        with self.assertRaisesRegex(ScriptError, "post-campaign evidence"):
            _finalize_executor(
                client,
                run_id="run-id",
                timeout=10.0,
                quarantine=False,
            )

    def test_finalize_rejects_malformed_failed_post_campaign_evidence(self) -> None:
        client = MagicMock()
        client.request.return_value = {
            "schema_version": 1,
            "operation": "finalize",
            "status": "failed",
            "run_id": "run-id",
            "post_campaign": None,
            "oracles": {
                "host_canary_intact": True,
                "network_canary_connections": 0,
                "watchdog_error": None,
                "network_canary_error": None,
            },
            "artifact_root": "target/run",
        }
        with self.assertRaisesRegex(ScriptError, "post-campaign evidence"):
            _finalize_executor(
                client,
                run_id="run-id",
                timeout=10.0,
                quarantine=False,
            )

    def test_finalize_accepts_nonclean_failed_post_campaign_evidence(self) -> None:
        client = MagicMock()
        client.request.return_value = {
            "schema_version": 1,
            "operation": "finalize",
            "status": "failed",
            "run_id": "run-id",
            "post_campaign": _boot_evidence(
                category="test-failure",
                returncode=1,
            ),
            "oracles": {
                "host_canary_intact": True,
                "network_canary_connections": 0,
                "watchdog_error": None,
                "network_canary_error": None,
            },
            "artifact_root": "target/run",
        }
        response = _finalize_executor(
            client,
            run_id="run-id",
            timeout=10.0,
            quarantine=False,
        )
        self.assertEqual(response["status"], "failed")

    def test_finalize_accepts_evidenced_late_quarantine(self) -> None:
        client = MagicMock()
        client.request.return_value = {
            "schema_version": 1,
            "operation": "finalize",
            "status": "quarantined",
            "run_id": "run-id",
            "post_campaign": _boot_evidence(),
            "oracles": {
                "host_canary_intact": False,
                "network_canary_connections": 1,
                "watchdog_error": None,
                "network_canary_error": None,
            },
            "artifact_root": "target/run",
        }
        response = _finalize_executor(
            client,
            run_id="run-id",
            timeout=10.0,
            quarantine=False,
        )
        self.assertEqual(response["status"], "quarantined")

    def test_finalize_rejects_live_post_campaign_process(self) -> None:
        client = MagicMock()
        client.request.return_value = {
            "schema_version": 1,
            "operation": "finalize",
            "status": "passed",
            "run_id": "run-id",
            "post_campaign": {
                "category": "passed",
                "returncode": 0,
                "timed_out": False,
                "duration_ms": 1,
                "teardown_complete": False,
                "openvmm_pids": [42],
                "live_openvmm_pids": [42],
                "artifact_path": "target/post-campaign",
            },
            "oracles": {
                "host_canary_intact": True,
                "network_canary_connections": 0,
                "watchdog_error": None,
                "network_canary_error": None,
            },
            "artifact_root": "target/run",
        }
        with self.assertRaisesRegex(ScriptError, "not a clean boot"):
            _finalize_executor(
                client,
                run_id="run-id",
                timeout=10.0,
                quarantine=False,
            )

    def test_replay_bypasses_copilot_and_quarantines_policy_violations(
        self,
    ) -> None:
        cases = load_campaign("workload-isolation")
        case_id = next(iter(cases))
        for escape in (False, True):
            with (
                self.subTest(escape=escape),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                replay = root / "replay.jsonl"
                replay.write_text(
                    json.dumps(
                        {
                            "schema_version": 1,
                            "sequence": 1,
                            "case_id": case_id,
                        }
                    )
                    + "\n",
                    encoding="utf-8",
                )
                quarantine_requests: list[bool] = []
                fake_executor_client = _fake_executor_client_type(
                    escape=escape,
                    quarantine_requests=quarantine_requests,
                )

                config = CampaignConfig(
                    backend="whp",
                    campaign="workload-isolation",
                    budget_seconds=300.0,
                    budget_actions=1,
                    budget_ai_credits=1,
                    seed=17,
                    output_dir=root / "output",
                    replay=replay,
                    model=None,
                    host_type="virtual-machine",
                    memory_mib=256,
                    phase_timeout=1.0,
                    action_timeout=10.0,
                    executor_command=None,
                    minimize=escape,
                    minimize_attempts=1,
                )
                _write_replay_manifest(replay, config)
                with (
                    patch("nvx_tools.adversarial.verify_source_tree"),
                    patch(
                        "nvx_tools.adversarial._controller_metadata",
                        return_value={
                            "nvx_commit": "a" * 40,
                            "openvmm_commit": "b" * 40,
                        },
                    ),
                    patch(
                        "nvx_tools.adversarial.ExecutorClient",
                        fake_executor_client,
                    ),
                    patch(
                        "nvx_tools.adversarial.CopilotController",
                        side_effect=AssertionError("replay invoked Copilot"),
                    ),
                    patch(
                        "nvx_tools.adversarial._minimize_failure",
                    ) as minimize,
                ):
                    outcome = run_campaign(config)
                minimize.assert_not_called()
                self.assertEqual(quarantine_requests, [escape])
                self.assertEqual(
                    outcome.status,
                    "failed" if escape else "passed",
                )
                self.assertEqual(
                    outcome.category,
                    "policy-violation" if escape else "replay-complete",
                )
                if escape:
                    run_dir = next((root / "output").glob("run-*"))
                    skipped = json.loads(
                        (run_dir / "minimization-skipped.json").read_text(
                            encoding="utf-8"
                        )
                    )
                    self.assertEqual(
                        skipped["reason"],
                        ("policy violations require an external fresh-target provider"),
                    )

    def test_late_canary_violation_is_a_policy_violation(self) -> None:
        cases = load_campaign("workload-isolation")
        case_id = next(iter(cases))
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            replay = root / "replay.jsonl"
            replay.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "sequence": 1,
                        "case_id": case_id,
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            quarantine_requests: list[bool] = []
            fake_executor_client = _fake_executor_client_type(
                escape=False,
                quarantine_requests=quarantine_requests,
                late_escape=True,
            )
            config = CampaignConfig(
                backend="whp",
                campaign="workload-isolation",
                budget_seconds=300.0,
                budget_actions=1,
                budget_ai_credits=1,
                seed=17,
                output_dir=root / "output",
                replay=replay,
                model=None,
                host_type="virtual-machine",
                memory_mib=256,
                phase_timeout=1.0,
                action_timeout=10.0,
                executor_command=None,
                minimize=False,
                minimize_attempts=1,
            )
            _write_replay_manifest(replay, config)
            with (
                patch("nvx_tools.adversarial.verify_source_tree"),
                patch(
                    "nvx_tools.adversarial._controller_metadata",
                    return_value={
                        "nvx_commit": "a" * 40,
                        "openvmm_commit": "b" * 40,
                    },
                ),
                patch(
                    "nvx_tools.adversarial.ExecutorClient",
                    fake_executor_client,
                ),
            ):
                outcome = run_campaign(config)
        self.assertEqual(quarantine_requests, [False])
        self.assertEqual(outcome.status, "failed")
        self.assertEqual(outcome.category, "policy-violation")

    def test_executor_loss_during_action_is_availability_failure(self) -> None:
        cases = load_campaign("workload-isolation")
        case_id = next(iter(cases))
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            replay = root / "replay.jsonl"
            replay.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "sequence": 1,
                        "case_id": case_id,
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            fake_executor_client = _fake_executor_client_type(
                escape=False,
                quarantine_requests=[],
                unavailable_on_execute=True,
            )
            config = CampaignConfig(
                backend="whp",
                campaign="workload-isolation",
                budget_seconds=300.0,
                budget_actions=1,
                budget_ai_credits=1,
                seed=17,
                output_dir=root / "output",
                replay=replay,
                model=None,
                host_type="virtual-machine",
                memory_mib=256,
                phase_timeout=1.0,
                action_timeout=10.0,
                executor_command=None,
                minimize=False,
                minimize_attempts=1,
            )
            _write_replay_manifest(replay, config)
            with (
                patch("nvx_tools.adversarial.verify_source_tree"),
                patch(
                    "nvx_tools.adversarial._controller_metadata",
                    return_value={
                        "nvx_commit": "a" * 40,
                        "openvmm_commit": "b" * 40,
                    },
                ),
                patch(
                    "nvx_tools.adversarial.ExecutorClient",
                    fake_executor_client,
                ),
                self.assertRaisesRegex(ScriptError, "target disappeared"),
            ):
                run_campaign(config)
            summary_path = next((root / "output").glob("*/summary.json"))
            summary = json.loads(summary_path.read_text(encoding="utf-8"))
        self.assertEqual(summary["status"], "failed")
        self.assertEqual(summary["category"], "availability-failure")
        self.assertEqual(summary["failed_case"], case_id)

    def test_malformed_execute_response_requests_quarantine(self) -> None:
        cases = load_campaign("workload-isolation")
        case_id = next(iter(cases))
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            replay = root / "actions.jsonl"
            replay.write_text(
                json.dumps({"schema_version": 1, "sequence": 1, "case_id": case_id})
                + "\n",
                encoding="utf-8",
            )
            quarantine_requests: list[bool] = []
            fake_executor_client = _fake_executor_client_type(
                escape=False,
                quarantine_requests=quarantine_requests,
                malformed_on_execute=True,
            )
            config = CampaignConfig(
                backend="whp",
                campaign="workload-isolation",
                budget_seconds=300.0,
                budget_actions=1,
                budget_ai_credits=1,
                seed=17,
                output_dir=root / "output",
                replay=replay,
                model=None,
                host_type="virtual-machine",
                memory_mib=256,
                phase_timeout=1.0,
                action_timeout=10.0,
                executor_command=None,
                minimize=False,
                minimize_attempts=1,
            )
            _write_replay_manifest(replay, config)
            with (
                patch("nvx_tools.adversarial.verify_source_tree"),
                patch(
                    "nvx_tools.adversarial._controller_metadata",
                    return_value={
                        "nvx_commit": "a" * 40,
                        "openvmm_commit": "b" * 40,
                    },
                ),
                patch(
                    "nvx_tools.adversarial.ExecutorClient",
                    fake_executor_client,
                ),
                self.assertRaisesRegex(ScriptError, "invalid properties"),
            ):
                run_campaign(config)
            summary_path = next((root / "output").glob("*/summary.json"))
            summary = json.loads(summary_path.read_text(encoding="utf-8"))
        self.assertEqual(quarantine_requests, [True])
        self.assertEqual(summary["category"], "availability-failure")

    def test_failed_policy_quarantine_preserves_escape_category(self) -> None:
        cases = load_campaign("workload-isolation")
        case_id = next(iter(cases))
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            replay = root / "actions.jsonl"
            replay.write_text(
                json.dumps({"schema_version": 1, "sequence": 1, "case_id": case_id})
                + "\n",
                encoding="utf-8",
            )
            quarantine_requests: list[bool] = []
            abort_requests: list[bool] = []
            fake_executor_client = _fake_executor_client_type(
                escape=True,
                quarantine_requests=quarantine_requests,
                fail_quarantine=True,
                abort_requests=abort_requests,
            )
            config = CampaignConfig(
                backend="whp",
                campaign="workload-isolation",
                budget_seconds=300.0,
                budget_actions=1,
                budget_ai_credits=1,
                seed=17,
                output_dir=root / "output",
                replay=replay,
                model=None,
                host_type="virtual-machine",
                memory_mib=256,
                phase_timeout=1.0,
                action_timeout=10.0,
                executor_command=None,
                minimize=False,
                minimize_attempts=1,
            )
            _write_replay_manifest(replay, config)
            with (
                patch("nvx_tools.adversarial.verify_source_tree"),
                patch(
                    "nvx_tools.adversarial._controller_metadata",
                    return_value={
                        "nvx_commit": "a" * 40,
                        "openvmm_commit": "b" * 40,
                    },
                ),
                patch(
                    "nvx_tools.adversarial.ExecutorClient",
                    fake_executor_client,
                ),
                self.assertRaisesRegex(ScriptError, "quarantine failed"),
            ):
                run_campaign(config)
            summary_path = next((root / "output").glob("*/summary.json"))
            summary = json.loads(summary_path.read_text(encoding="utf-8"))
        self.assertEqual(quarantine_requests, [True, True])
        self.assertEqual(abort_requests, [True])
        self.assertEqual(summary["category"], "policy-violation")

    def test_replay_manifest_detects_action_tampering(self) -> None:
        cases = load_campaign("workload-isolation")
        case_id = next(iter(cases))
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            replay = root / "actions.jsonl"
            replay.write_text(
                json.dumps({"schema_version": 1, "sequence": 1, "case_id": case_id})
                + "\n",
                encoding="utf-8",
            )
            config = CampaignConfig(
                backend="whp",
                campaign="workload-isolation",
                budget_seconds=300.0,
                budget_actions=1,
                budget_ai_credits=1,
                seed=17,
                output_dir=root / "output",
                replay=replay,
                model=None,
                host_type="virtual-machine",
                memory_mib=256,
                phase_timeout=1.0,
                action_timeout=10.0,
                executor_command=None,
                minimize=False,
                minimize_attempts=1,
            )
            _write_replay_manifest(replay, config)
            manifest_path = root / "replay-manifest.json"
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest["budgets"]["actions"] = 2
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            with self.assertRaisesRegex(ScriptError, "budgets do not match"):
                _validate_replay_manifest(replay, config=config, cases=cases)
            _write_replay_manifest(replay, config)
            replay.write_text("{}\n", encoding="utf-8")
            with self.assertRaisesRegex(ScriptError, "do not match"):
                _validate_replay_manifest(replay, config=config, cases=cases)


class CopilotContainmentTests(unittest.TestCase):
    def test_project_source_archive_includes_adversary_agent(self) -> None:
        self.assertIn(".github/agents/nvx-adversary.md", PROJECT_SOURCE_PATHS)

    def test_nonfinite_time_budget_is_rejected(self) -> None:
        for value in ("inf", "-inf", "nan"):
            with (
                self.subTest(value=value),
                self.assertRaises(argparse.ArgumentTypeError),
            ):
                _positive_float(value)

    def test_local_executor_uses_short_absolute_state_root(self) -> None:
        environment = local_executor_environment()
        state_root = Path(environment["NVX_ADVERSARIAL_STATE_ROOT"])
        self.assertTrue(state_root.is_absolute())
        self.assertEqual(state_root, LOCAL_EXECUTOR_STATE_ROOT.resolve())
        deepest = (
            state_root
            / ("f" * 32)
            / "cases"
            / "001-filesystem-alias-boundary"
            / "run-01"
            / "microvm"
            / "denied-filesystem-duplicate.log"
        )
        self.assertLess(len(str(deepest)), 260)

    def test_local_executor_protocol_starts_without_controller_secrets(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            with ExecutorClient(
                run_dir=Path(temporary),
                command=None,
                label="unit",
            ) as client:
                response = client.request(
                    {
                        "schema_version": 1,
                        "operation": "shutdown",
                    },
                    timeout=10.0,
                )
        self.assertEqual(response["status"], "stopped")

    def test_missing_copilot_is_a_preflight_failure(self) -> None:
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch("nvx_tools.adversarial.shutil.which", return_value=None),
        ):
            with self.assertRaisesRegex(ScriptError, "required"):
                CopilotController(
                    run_dir=Path(temporary),
                    model=None,
                    credit_budget=30,
                )

    def test_copilot_smoke_rejects_boolean_schema_version(self) -> None:
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch("nvx_tools.adversarial.shutil.which", return_value="copilot"),
            patch.object(
                CopilotController,
                "_version",
                return_value="GitHub Copilot CLI test",
            ),
        ):
            controller = CopilotController(
                run_dir=Path(temporary),
                model=None,
                credit_budget=30,
            )
            with (
                patch.object(
                    controller,
                    "_invoke",
                    return_value=b'{"schema_version":true,"nonce":"invalid"}',
                ),
                self.assertRaisesRegex(ScriptError, "invalid schema"),
            ):
                controller.smoke_test(timeout=1.0)

    def test_executor_environment_drops_controller_credentials(self) -> None:
        with patch.dict(
            os.environ,
            {
                "PATH": "test-path",
                "HOME": "test-home",
                "GITHUB_TOKEN": "github-secret",
                "COPILOT_GITHUB_TOKEN": "copilot-secret",
                "SSH_AUTH_SOCK": "agent-secret",
                "UNRELATED_SECRET": "other-secret",
            },
            clear=True,
        ):
            environment = sanitized_environment()
        self.assertEqual(environment["PATH"], "test-path")
        self.assertEqual(environment["HOME"], "test-home")
        self.assertEqual(environment["PYTHONUNBUFFERED"], "1")
        self.assertNotIn("GITHUB_TOKEN", environment)
        self.assertNotIn("COPILOT_GITHUB_TOKEN", environment)
        self.assertNotIn("SSH_AUTH_SOCK", environment)
        self.assertNotIn("UNRELATED_SECRET", environment)

    def test_external_executor_also_drops_controller_credentials(self) -> None:
        process = MagicMock()
        process.stdin = MagicMock()
        process.stdout = MagicMock()
        process.stdout.readline.return_value = b""
        process.poll.return_value = 0
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.dict(
                os.environ,
                {"PATH": "test-path", "GITHUB_TOKEN": "github-secret"},
                clear=True,
            ),
            patch("nvx_tools.adversarial.shutil.which", return_value="wrapper"),
            patch(
                "nvx_tools.adversarial.subprocess.Popen", return_value=process
            ) as popen,
        ):
            with ExecutorClient(
                run_dir=Path(temporary),
                command="wrapper",
                label="external",
            ):
                pass
        environment = popen.call_args.kwargs["env"]
        self.assertEqual(environment["PATH"], "test-path")
        self.assertNotIn("GITHUB_TOKEN", environment)

    def test_executor_rejects_boolean_schema_version(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            executor = AdversarialExecutor(Path(temporary))
            with self.assertRaisesRegex(ScriptError, "must be an integer"):
                executor.handle({"schema_version": True, "operation": "shutdown"})

    def test_external_executor_rejects_dirty_source_tree(self) -> None:
        campaign = "workload-isolation"
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch(
                "nvx_tools.adversarial_executor._target_metadata",
                return_value={
                    "nvx_commit": "a" * 40,
                    "nvx_dirty": True,
                    "openvmm_commit": "b" * 40,
                    "openvmm_dirty": False,
                },
            ),
        ):
            executor = AdversarialExecutor(Path(temporary))
            with self.assertRaisesRegex(ScriptError, "not clean"):
                executor.handle(
                    {
                        "schema_version": 1,
                        "operation": "initialize",
                        "run_id": "unit-test",
                        "expected_commit": "a" * 40,
                        "expected_openvmm_commit": "b" * 40,
                        "expected_catalog_sha256": catalog_digest(
                            load_campaign(campaign).values()
                        ),
                        "require_clean": True,
                        "backend": "whp" if os.name == "nt" else "kvm",
                        "campaign": campaign,
                        "host_type": "virtual-machine",
                        "seed": 1,
                        "memory_mib": 256,
                        "phase_timeout_seconds": 1.0,
                        "baseline_timeout_seconds": 1.0,
                    }
                )

    def test_executor_blocks_case_dispatch_on_sticky_oracle_violation(self) -> None:
        cases = load_campaign("workload-isolation")
        case = next(iter(cases.values()))
        violations = (
            {
                "sample_host_intact": True,
                "sticky_host_intact": False,
                "network_connections": 0,
                "watchdog_error": None,
                "network_error": None,
                "expected_category": "policy-violation",
            },
            {
                "sample_host_intact": True,
                "sticky_host_intact": True,
                "network_connections": 1,
                "watchdog_error": None,
                "network_error": None,
                "expected_category": "policy-violation",
            },
            {
                "sample_host_intact": True,
                "sticky_host_intact": True,
                "network_connections": 0,
                "watchdog_error": "watchdog stopped",
                "network_error": None,
                "expected_category": "availability-failure",
            },
            {
                "sample_host_intact": True,
                "sticky_host_intact": True,
                "network_connections": 0,
                "watchdog_error": None,
                "network_error": "network canary stopped",
                "expected_category": "availability-failure",
            },
        )
        for violation in violations:
            with (
                self.subTest(violation=violation),
                tempfile.TemporaryDirectory() as temporary,
                patch(
                    "nvx_tools.adversarial_executor.run_bounded_process"
                ) as run_process,
            ):
                oracles = MagicMock()
                oracles.watchdog.sample.return_value = {
                    "host_canary_intact": violation["sample_host_intact"],
                    "network_canary_connections": violation["network_connections"],
                }
                oracles.watchdog.canary_intact = violation["sticky_host_intact"]
                oracles.watchdog.error = violation["watchdog_error"]
                oracles.network_canary.connections = violation["network_connections"]
                oracles.network_canary.error = violation["network_error"]
                session = _InitializedSession(
                    run_id="unit-test",
                    backend="whp",
                    host_type="virtual-machine",
                    memory_mib=256,
                    phase_timeout=1.0,
                    root=Path(temporary),
                    cases=cases,
                    oracles=oracles,
                )
                result = session.run_case(case, sequence=1, timeout=1.0)
            run_process.assert_not_called()
            self.assertEqual(result["category"], violation["expected_category"])

    def test_executor_blocks_canary_boot_on_prior_oracle_failure(self) -> None:
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch("nvx_tools.adversarial_executor.run_bounded_process") as run_process,
        ):
            oracles = MagicMock()
            oracles.watchdog.sample.return_value = {
                "host_canary_intact": True,
                "network_canary_connections": 0,
            }
            oracles.watchdog.canary_intact = True
            oracles.watchdog.error = "watchdog stopped"
            oracles.network_canary.connections = 0
            oracles.network_canary.error = None
            session = _InitializedSession(
                run_id="unit-test",
                backend="whp",
                host_type="virtual-machine",
                memory_mib=256,
                phase_timeout=1.0,
                root=Path(temporary),
                cases={},
                oracles=oracles,
            )
            result = session.run_canary_boot(name="post-campaign", timeout=1.0)
        run_process.assert_not_called()
        self.assertEqual(result["category"], "availability-failure")
        self.assertEqual(result["openvmm_pids"], [])

    def test_quarantined_executor_cannot_be_reused(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            executor = AdversarialExecutor(Path(temporary))
            session = MagicMock()
            session.run_id = "unit-test"
            session.root = Path(temporary) / "unit-test"
            session.root.mkdir()
            session.oracles.close.return_value = {
                "host_canary_intact": False,
                "network_canary_connections": 0,
                "watchdog_error": None,
                "network_canary_error": None,
            }
            executor._session = session
            response = executor.handle(
                {
                    "schema_version": 1,
                    "operation": "finalize",
                    "run_id": "unit-test",
                    "timeout_seconds": 1.0,
                    "quarantine": True,
                }
            )
            self.assertEqual(response["status"], "quarantined")
            with self.assertRaisesRegex(ScriptError, "cannot be reused"):
                executor.handle({"schema_version": 1, "operation": "initialize"})

    def test_late_canary_violation_irreversibly_quarantines_executor(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            executor = AdversarialExecutor(Path(temporary))
            session = MagicMock()
            session.run_id = "unit-test"
            session.root = Path(temporary) / "unit-test"
            session.root.mkdir()
            session.run_canary_boot.return_value = _boot_evidence()
            session.oracles.close.return_value = {
                "host_canary_intact": False,
                "network_canary_connections": 1,
                "watchdog_error": None,
                "network_canary_error": None,
            }
            executor._session = session
            response = executor.handle(
                {
                    "schema_version": 1,
                    "operation": "finalize",
                    "run_id": "unit-test",
                    "timeout_seconds": 1.0,
                    "quarantine": False,
                }
            )
            self.assertEqual(response["status"], "quarantined")
            with self.assertRaisesRegex(ScriptError, "cannot be reused"):
                executor.handle({"schema_version": 1, "operation": "initialize"})

    def test_copilot_invocation_has_no_tools_and_fixed_credit_cap(self) -> None:
        cases = load_campaign("workload-isolation")
        case_id = next(iter(cases))
        observed_commands: list[list[str]] = []
        observed_prompts: list[bytes] = []

        def fake_run(
            command: Sequence[str],
            *,
            cwd: Path,
            output_dir: Path,
            timeout: float,
            environment: Mapping[str, str],
            stdin_data: bytes | None = None,
        ) -> BoundedProcessResult:
            del cwd, timeout, environment
            argv = list(command)
            observed_commands.append(argv)
            assert stdin_data is not None
            observed_prompts.append(stdin_data)
            output_dir.mkdir(parents=True)
            stdout = output_dir / "stdout.log"
            stderr = output_dir / "stderr.log"
            stdout.write_text(
                json.dumps({"schema_version": 1, "case_id": case_id}),
                encoding="utf-8",
            )
            stderr.write_bytes(b"")
            usage_index = argv.index("--usage-output-file") + 1
            usage = Path(argv[usage_index])
            usage.parent.mkdir(parents=True, exist_ok=True)
            usage.write_text('{"ai_credits":1}\n', encoding="utf-8")
            return BoundedProcessResult(
                returncode=0,
                timed_out=False,
                duration_ms=1,
                stdout_path=stdout,
                stderr_path=stderr,
                stdout_bytes=stdout.stat().st_size,
                stderr_bytes=0,
                stdout_truncated=False,
                stderr_truncated=False,
                teardown_complete=True,
            )

        with tempfile.TemporaryDirectory(
            dir=Path.cwd(),
            prefix=".nvx-adversarial-test-",
        ) as temporary:
            relative_run_dir = Path(temporary).relative_to(Path.cwd())
            with (
                patch("nvx_tools.adversarial.shutil.which", return_value="copilot"),
                patch.object(
                    CopilotController,
                    "_version",
                    return_value="GitHub Copilot CLI test",
                ),
                patch(
                    "nvx_tools.adversarial.run_bounded_process",
                    side_effect=fake_run,
                ),
            ):
                controller = CopilotController(
                    run_dir=relative_run_dir,
                    model=None,
                    credit_budget=60,
                )
                action = controller.select_action(
                    cases=cases,
                    completed=set(),
                    results=(),
                    remaining_seconds=10.0,
                    remaining_actions=1,
                    seed=7,
                )
        self.assertEqual(action.case_id, case_id)
        command = observed_commands[0]
        self.assertIn("--available-tools", command)
        self.assertEqual(
            command[command.index("--available-tools") + 1],
            "",
        )
        self.assertIn("--disable-builtin-mcps", command)
        self.assertIn("--no-custom-instructions", command)
        self.assertIn("--no-ask-user", command)
        self.assertIn("--disallow-temp-dir", command)
        self.assertTrue(Path(command[command.index("-C") + 1]).is_absolute())
        self.assertEqual(
            command[command.index("--max-autopilot-continues") + 1],
            "0",
        )
        self.assertEqual(
            command[command.index("--agent") + 1],
            "nvx-adversary",
        )
        self.assertEqual(
            command[command.index("--max-ai-credits") + 1],
            "30",
        )
        self.assertNotIn("--prompt", command)
        self.assertIn(b"Remaining budget", observed_prompts[0])
        self.assertNotIn("login", command)
        self.assertEqual(controller.credits_charged, 30)

    def test_unknown_case_gets_one_strict_budgeted_repair(self) -> None:
        cases = load_campaign("workload-isolation")
        case_id = next(iter(cases))
        responses = [
            {"schema_version": 1, "case_id": "baseline"},
            {"schema_version": 1, "case_id": case_id},
        ]
        observed_commands: list[list[str]] = []
        observed_prompts: list[bytes] = []

        def fake_run(
            command: Sequence[str],
            *,
            cwd: Path,
            output_dir: Path,
            timeout: float,
            environment: Mapping[str, str],
            stdin_data: bytes | None = None,
        ) -> BoundedProcessResult:
            del cwd, timeout, environment
            argv = list(command)
            observed_commands.append(argv)
            assert stdin_data is not None
            observed_prompts.append(stdin_data)
            output_dir.mkdir(parents=True)
            stdout = output_dir / "stdout.log"
            stderr = output_dir / "stderr.log"
            stdout.write_text(
                json.dumps(responses[len(observed_commands) - 1]),
                encoding="utf-8",
            )
            stderr.write_bytes(b"")
            usage = Path(argv[argv.index("--usage-output-file") + 1])
            usage.parent.mkdir(parents=True, exist_ok=True)
            usage.write_text(
                '{"totalNanoAiu":1000000000}\n',
                encoding="utf-8",
            )
            return BoundedProcessResult(
                returncode=0,
                timed_out=False,
                duration_ms=1,
                stdout_path=stdout,
                stderr_path=stderr,
                stdout_bytes=stdout.stat().st_size,
                stderr_bytes=0,
                stdout_truncated=False,
                stderr_truncated=False,
                teardown_complete=True,
            )

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with (
                patch("nvx_tools.adversarial.shutil.which", return_value="copilot"),
                patch.object(
                    CopilotController,
                    "_version",
                    return_value="GitHub Copilot CLI test",
                ),
                patch(
                    "nvx_tools.adversarial.run_bounded_process",
                    side_effect=fake_run,
                ),
            ):
                controller = CopilotController(
                    run_dir=root,
                    model=None,
                    credit_budget=60,
                )
                action = controller.select_action(
                    cases=cases,
                    completed=set(),
                    results=(),
                    remaining_seconds=10.0,
                    remaining_actions=1,
                    seed=7,
                )
            rejections = (root / "copilot" / "rejections.jsonl").read_text(
                encoding="utf-8"
            )
        self.assertEqual(action.case_id, case_id)
        self.assertEqual(len(observed_commands), 2)
        self.assertEqual(controller.credits_charged, 2)
        self.assertIn("unknown case baseline", rejections)
        repair_prompt = observed_prompts[1].decode("utf-8")
        self.assertIn("allowed_case_ids", repair_prompt)
        self.assertIn(case_id, repair_prompt)


if __name__ == "__main__":
    unittest.main()
