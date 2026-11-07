#!/usr/bin/env python3
# pyright: reportPrivateUsage=false
"""Tests for the cross-platform NVX benchmark series coordinator."""

from __future__ import annotations

import contextlib
import datetime as dt
import io
import json
import sys
import tempfile
import unittest
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import benchmark_series  # noqa: E402

COMMIT = "a" * 40
SHORT_COMMIT = "a" * 7
NOW = dt.datetime(2026, 11, 7, 5, 6, 6, tzinfo=dt.timezone.utc)


@dataclass(frozen=True)
class Call:
    args: tuple[str, ...]
    cwd: Path
    capture_output: bool


Handler = Callable[[tuple[str, ...]], benchmark_series.CommandResult]


class FakeRunner:
    """Record commands and answer them without starting a subprocess."""

    def __init__(self, handler: Handler) -> None:
        self.handler = handler
        self.calls: list[Call] = []

    def run(
        self,
        args: Sequence[str],
        *,
        cwd: Path,
        capture_output: bool,
    ) -> benchmark_series.CommandResult:
        command = tuple(args)
        self.calls.append(Call(command, cwd, capture_output))
        return self.handler(command)


class StatusRunner(FakeRunner):
    """Return a fixed sequence of statuses."""

    def __init__(self, statuses: Sequence[int]) -> None:
        self.statuses = list(statuses)
        super().__init__(self._respond)

    def _respond(self, command: tuple[str, ...]) -> benchmark_series.CommandResult:
        del command
        if not self.statuses:
            raise AssertionError("unexpected command")
        return benchmark_series.CommandResult(self.statuses.pop(0))


def _git_handler(command: tuple[str, ...]) -> benchmark_series.CommandResult:
    if command == ("git", "rev-parse", "HEAD"):
        return benchmark_series.CommandResult(0, COMMIT + "\n")
    if command == ("git", "rev-parse", "--short", "HEAD"):
        return benchmark_series.CommandResult(0, SHORT_COMMIT + "\n")
    if command == ("git", "status", "--porcelain"):
        return benchmark_series.CommandResult(0, " M assigned-file\n")
    raise AssertionError(f"unexpected command: {command}")


def _write_action(
    path: Path,
    *,
    warmups: int = 1,
    runs: int = 10,
    timeout: str = "10",
    reserve: int = 2,
    cpus: str = "",
    teardown: str = "guest-exit",
) -> None:
    path.write_text(
        "\n".join(
            (
                "inputs:",
                "  teardown-mode:",
                f"    default: {teardown}",
                "  cpus:",
                f'    default: "{cpus}"',
                "  host-cpu-reserve:",
                f'    default: "{reserve}"',
                "  marker-timeout:",
                f'    default: "{timeout}"',
                "  runs:",
                f'    default: "{runs}"',
                "  warmups:",
                f'    default: "{warmups}"',
            )
        )
        + "\n",
        encoding="utf-8",
    )


def _options(
    root: Path,
    profile: benchmark_series.Profile,
    backend: benchmark_series.Backend,
    *,
    with_multi_vcpu: bool = False,
    scratch_dir: Path | None = None,
) -> benchmark_series.CliOptions:
    return benchmark_series.CliOptions(
        profile=profile,
        backend=backend,
        host_type="virtual-machine",
        output_root=root / "results",
        with_multi_vcpu=with_multi_vcpu,
        scratch_dir=scratch_dir,
        cpus=None,
        host_cpu_reserve=None,
        timeout=None,
        teardown_mode=None,
        dry_run=True,
        json_output=False,
    )


def _plan(
    root: Path,
    action: Path,
    profile: benchmark_series.Profile = "canonical",
    backend: benchmark_series.Backend = "whp",
    *,
    system: str = "Windows",
    with_multi_vcpu: bool = False,
    scratch_dir: Path | None = None,
    environment: Mapping[str, str] | None = None,
) -> benchmark_series.SeriesPlan:
    return benchmark_series._make_plan(
        _options(
            root,
            profile,
            backend,
            with_multi_vcpu=with_multi_vcpu,
            scratch_dir=scratch_dir,
        ),
        runner=FakeRunner(_git_handler),
        repository_root=root,
        action_path=action,
        system_name=system,
        now=NOW,
        environment=environment or {},
    )


def _argument(command: Sequence[str], option: str) -> str:
    index = command.index(option)
    return command[index + 1]


def _host() -> dict[str, object]:
    return {
        "runner_name": "runner",
        "machine_name": "machine",
        "volumes": {"scratch": {"mount_point": "scratch"}},
    }


def _write_json(path: Path, document: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(document) + "\n", encoding="utf-8")


def _metadata(
    plan: benchmark_series.SeriesPlan,
    suite: str,
    processors: int,
    warmups: int,
    runs: int,
    virtfs_runs: int,
) -> dict[str, object]:
    return {
        "schema_version": 1,
        "suite": suite,
        "platform": plan.platform,
        "backend": plan.backend,
        "microvm_abi_version": 3,
        "processors": processors,
        "warmups": warmups,
        "measured_runs": runs,
        "virtfs_measured_runs": virtfs_runs,
        "scratch_directory": str(plan.repository_root / "scratch"),
        "artifact_revisions": {"nvx": plan.commit},
        "host": _host(),
    }


def _write_result(
    directory: Path,
    plan: benchmark_series.SeriesPlan,
    processors: int,
    rows: int,
    metric: str | None = None,
) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    filename = f"{plan.platform}-microvm-v3-{processors}vcpu.csv"
    body = "".join(
        f"{plan.platform},{processors},{plan.commit},{metric or f'metric_{index}'}\n"
        for index in range(rows)
    )
    (directory / filename).write_text(
        "platform,processors,commit,metric\n" + body,
        encoding="utf-8",
    )
    _write_json(directory / f"{plan.platform}-host.json", _host())


def _populate_complete_result(plan: benchmark_series.SeriesPlan) -> None:
    host = _host()
    _write_json(
        plan.acceptance_output,
        {
            "controls": {
                "suite": "e2e",
                "platform": plan.platform,
                "backend": plan.backend,
                "processors": 1,
                "warmups": plan.settings.acceptance_warmups,
                "runs": plan.settings.acceptance_runs,
                "teardown_mode": plan.settings.teardown_mode,
                "snapshot_profile": True,
                "scratch_directory": str(plan.repository_root / "scratch"),
                "artifact_revisions": {"nvx": plan.commit},
            },
            "host": host,
        },
    )
    _write_json(plan.acceptance_attempt_log, {"schema_version": 1})
    plan.performance_dir.mkdir(parents=True, exist_ok=True)
    for filename in benchmark_series.PERFORMANCE_LOGS:
        (plan.performance_dir / filename).write_text("raw\n", encoding="utf-8")
    performance = _metadata(
        plan,
        "performance",
        1,
        plan.settings.performance_warmups,
        plan.settings.performance_runs,
        plan.settings.virtfs_runs,
    )
    _write_json(plan.performance_dir / "benchmark-metadata.json", performance)
    _write_result(
        plan.performance_results_dir,
        plan,
        1,
        29 if plan.profile == "smoke" else 34,
    )

    plan.device_dir.mkdir(parents=True, exist_ok=True)
    (plan.device_dir / "device-io.log").write_text("raw\n", encoding="utf-8")
    device = _metadata(
        plan,
        "device-io",
        1,
        plan.settings.device_warmups,
        plan.settings.device_runs,
        3,
    )
    device["device_io_duration_seconds"] = plan.settings.device_duration_seconds
    device["device_io_size_mib"] = plan.settings.device_size_mib or 512
    device["artifact_sha256"] = {
        name: "b" * 64
        for name in ("openvmm", "kernel", "initrd", "benchmark_coordinator")
    }
    _write_json(plan.device_dir / "benchmark-metadata.json", device)
    _write_result(plan.device_results_dir, plan, 1, 5)
    plan.summary_path.write_text("# Summary\n", encoding="utf-8")

    if plan.settings.with_multi_vcpu:
        for processors in (2, 4, 8):
            directory = plan.multi_dir(processors)
            directory.mkdir(parents=True, exist_ok=True)
            (directory / "shell-snapshot-restore.log").write_text(
                "raw\n", encoding="utf-8"
            )
            metadata = _metadata(
                plan,
                "shell-snapshot-restore",
                processors,
                plan.settings.multi_warmups,
                plan.settings.multi_runs,
                3,
            )
            metadata["shell_memories_mib"] = [512]
            _write_json(directory / "benchmark-metadata.json", metadata)
            _write_result(
                plan.multi_results_dir(processors),
                plan,
                processors,
                1,
                "shell_snapshot_restore_512_mib",
            )


class PlanTests(unittest.TestCase):
    def test_each_profile_and_backend_has_a_complete_dry_run_plan(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            action = root / "action.yml"
            _write_action(action)
            combinations: tuple[tuple[str, benchmark_series.Backend], ...] = (
                ("Windows", "whp"),
                ("Linux", "kvm"),
                ("Linux", "mshv"),
            )
            for profile in benchmark_series.PROFILES:
                for system, backend in combinations:
                    with self.subTest(profile=profile, backend=backend):
                        plan = _plan(
                            root,
                            action,
                            profile,
                            backend,
                            system=system,
                        )
                        commands = benchmark_series.planned_commands(plan)
                        self.assertEqual(commands[0].argv[2:], ("verify",))
                        acceptance = next(
                            command
                            for command in commands
                            if command.step == "acceptance benchmark"
                            and command.attempt == 1
                        )
                        self.assertEqual(
                            _argument(acceptance.argv, "--backend"), backend
                        )
                        self.assertEqual(
                            _argument(acceptance.argv, "--platform"), plan.platform
                        )
                        self.assertIn("--snapshot-profile", acceptance.argv)
                        self.assertEqual(
                            sum(
                                command.step == "acceptance benchmark"
                                for command in commands
                            ),
                            2,
                        )
                        self.assertEqual(
                            sum(
                                command.step == "performance benchmark"
                                for command in commands
                            ),
                            2,
                        )
                        if profile == "smoke":
                            self.assertEqual(
                                _argument(acceptance.argv, "--warmups"), "0"
                            )
                            self.assertFalse(
                                any(
                                    "persistence" in command.step
                                    for command in commands
                                )
                            )
                            self.assertFalse(plan.settings.with_multi_vcpu)
                        elif profile == "canonical":
                            performance = next(
                                command
                                for command in commands
                                if command.step == "performance benchmark"
                            )
                            device = next(
                                command
                                for command in commands
                                if command.step == "device-I/O benchmark"
                            )
                            self.assertEqual(_argument(performance.argv, "--runs"), "5")
                            self.assertEqual(
                                _argument(performance.argv, "--virtfs-runs"), "3"
                            )
                            self.assertEqual(
                                _argument(device.argv, "--device-io-duration-seconds"),
                                "10",
                            )
                            self.assertEqual(
                                _argument(device.argv, "--device-io-size-mib"),
                                "512",
                            )
                            self.assertFalse(plan.settings.with_multi_vcpu)
                        else:
                            collection = next(
                                command
                                for command in commands
                                if command.step == "performance collection"
                            )
                            self.assertTrue(plan.settings.with_multi_vcpu)
                            self.assertIn("--require-shared-suite", collection.argv)
                            for processors in (2, 4, 8):
                                self.assertTrue(
                                    any(
                                        command.step == f"{processors}-vCPU benchmark"
                                        for command in commands
                                    )
                                )

    def test_canonical_multi_vcpu_uses_the_sampling_that_the_collector_accepts(
        self,
    ) -> None:
        # --require-shell-snapshot-restore-512 accepts one warmup and ten runs,
        # so five would fail every collection.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            action = root / "action.yml"
            _write_action(action)
            plan = _plan(
                root,
                action,
                with_multi_vcpu=True,
            )
            commands = benchmark_series.planned_commands(plan)
            for processors in (2, 4, 8):
                command = next(
                    entry
                    for entry in commands
                    if entry.step == f"{processors}-vCPU benchmark"
                )
                self.assertEqual(_argument(command.argv, "--warmups"), "1")
                self.assertEqual(_argument(command.argv, "--runs"), "10")
                self.assertEqual(_argument(command.argv, "--shell-memories"), "512")

    def test_smoke_refuses_the_multi_vcpu_runs(self) -> None:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit) as exit_:
            benchmark_series.main(
                [
                    "--profile",
                    "smoke",
                    "--backend",
                    "kvm",
                    "--host-type",
                    "virtual-machine",
                    "--with-multi-vcpu",
                    "--dry-run",
                ]
            )
        self.assertEqual(exit_.exception.code, 2)
        self.assertIn("needs the ci or canonical profile", stderr.getvalue())

    def test_ci_values_are_read_from_the_action_fixture(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            action = root / "action.yml"
            _write_action(
                action,
                warmups=3,
                runs=7,
                timeout="12.5",
                reserve=4,
                cpus="1,3,5,7",
                teardown="host-terminate",
            )
            plan = _plan(root, action, "ci")
            settings = plan.settings
            self.assertEqual(settings.acceptance_warmups, 3)
            self.assertEqual(settings.performance_runs, 7)
            self.assertEqual(settings.virtfs_runs, 7)
            self.assertEqual(settings.device_runs, 7)
            self.assertEqual(settings.multi_runs, 7)
            self.assertEqual(settings.timeout, "12.5")
            self.assertEqual(settings.host_cpu_reserve, 4)
            self.assertEqual(settings.cpus, "1,3,5,7")
            self.assertEqual(settings.teardown_mode, "host-terminate")
            command = benchmark_series._acceptance_benchmark(plan, 1)
            self.assertEqual(_argument(command, "--cpus"), "1,3,5,7")
            self.assertEqual(_argument(command, "--host-cpu-reserve"), "4")
            self.assertEqual(_argument(command, "--timeout"), "12.5")

    def test_windows_uses_a_fresh_environment_scratch_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            action = root / "action.yml"
            scratch_root = root / "scratch-root"
            scratch_root.mkdir()
            _write_action(action)
            plan = _plan(
                root,
                action,
                environment={"NVX_BENCHMARK_SCRATCH": str(scratch_root)},
            )
            self.assertTrue(plan.automatic_scratch)
            self.assertEqual(plan.scratch_dir, scratch_root / plan.output_dir.name)
            command = benchmark_series._acceptance_benchmark(plan, 1)
            self.assertEqual(_argument(command, "--scratch-dir"), str(plan.scratch_dir))

    def test_dry_run_executes_only_git_and_creates_no_result_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            action = root / "action.yml"
            output = io.StringIO()
            error = io.StringIO()
            _write_action(action)
            runner = FakeRunner(_git_handler)
            status = benchmark_series.run_cli(
                (
                    "--profile",
                    "canonical",
                    "--backend",
                    "whp",
                    "--host-type",
                    "virtual-machine",
                    "--output-root",
                    str(root / "runs"),
                    "--dry-run",
                ),
                runner=runner,
                repository_root=root,
                action_path=action,
                system_name="Windows",
                now=NOW,
                environment={},
                output=output,
                error=error,
            )
            self.assertEqual(status, 0, error.getvalue())
            self.assertEqual(len(runner.calls), 3)
            self.assertTrue(all(call.args[0] == "git" for call in runner.calls))
            self.assertFalse((root / "runs").exists())
            self.assertIn("validation status 75", output.getvalue())
            self.assertIn("NVX_RESULTS=", output.getvalue())


class PreflightTests(unittest.TestCase):
    def test_incompatible_operating_system_and_backend_are_refused(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            action = root / "action.yml"
            _write_action(action)
            for system, backend in (
                ("Linux", "whp"),
                ("Windows", "kvm"),
                ("Windows", "mshv"),
            ):
                with self.subTest(system=system, backend=backend):
                    runner = FakeRunner(_git_handler)
                    error = io.StringIO()
                    status = benchmark_series.run_cli(
                        (
                            "--profile",
                            "smoke",
                            "--backend",
                            backend,
                            "--host-type",
                            "baremetal",
                            "--dry-run",
                        ),
                        runner=runner,
                        repository_root=root,
                        action_path=action,
                        system_name=system,
                        now=NOW,
                        environment={},
                        output=io.StringIO(),
                        error=error,
                    )
                    self.assertEqual(status, 1)
                    self.assertIn("unsupported", error.getvalue())
                    self.assertEqual(runner.calls, [])

    def test_a_running_openvmm_process_is_reported_and_never_terminated(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            runner = FakeRunner(
                lambda command: (
                    benchmark_series.CommandResult(0, "42 /usr/bin/openvmm --run\n")
                    if command == ("pgrep", "-af", "openvmm")
                    else (_ for _ in ()).throw(AssertionError(command))
                )
            )
            with self.assertRaisesRegex(
                benchmark_series.SeriesError,
                "42 /usr/bin/openvmm --run",
            ):
                benchmark_series._check_openvmm_processes("Linux", runner, root)
            self.assertEqual(
                [call.args for call in runner.calls],
                [("pgrep", "-af", "openvmm")],
            )


class RetryTests(unittest.TestCase):
    def run_file_retry(self, statuses: Sequence[int]) -> tuple[int, int, int, bool]:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            action = root / "action.yml"
            _write_action(action)
            plan = _plan(root, action, "smoke")
            plan.output_dir.mkdir(parents=True)
            for attempt in (1, 2):
                _write_json(
                    benchmark_series._acceptance_attempt(plan, attempt),
                    {"attempt": attempt},
                )
            runner = StatusRunner(statuses)
            report = benchmark_series.ExecutionReport()
            executor = benchmark_series.SeriesExecutor(
                plan,
                runner,
                report,
                io.StringIO(),
            )
            status, attempts = benchmark_series._run_validated_file(
                executor,
                name="acceptance",
                benchmark=lambda attempt: ("benchmark", str(attempt)),
                validation=lambda attempt: ("validate", str(attempt)),
                attempt_path=lambda attempt: benchmark_series._acceptance_attempt(
                    plan, attempt
                ),
                accepted_path=plan.acceptance_output,
            )
            return status, attempts, len(runner.calls), plan.acceptance_output.exists()

    def test_a_valid_first_attempt_is_accepted(self) -> None:
        self.assertEqual(self.run_file_retry((0, 0)), (0, 1, 2, True))

    def test_an_unstable_first_attempt_is_remeasured_once(self) -> None:
        self.assertEqual(self.run_file_retry((0, 75, 0, 0)), (0, 2, 4, True))

    def test_two_unstable_attempts_return_status_75(self) -> None:
        self.assertEqual(self.run_file_retry((0, 75, 0, 75)), (75, 2, 4, False))

    def test_a_nonretryable_validation_status_stops_immediately(self) -> None:
        self.assertEqual(self.run_file_retry((0, 23)), (23, 1, 2, False))

    def test_an_acceptance_command_failure_stops_before_performance(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            action = root / "action.yml"
            _write_action(action)
            plan = _plan(root, action, "smoke")
            statuses = [0, 9]

            def respond(command: tuple[str, ...]) -> benchmark_series.CommandResult:
                if "--output" in command:
                    output = Path(_argument(command, "--output"))
                    _write_json(output, {"partial": True})
                if not statuses:
                    raise AssertionError(f"unexpected command: {command}")
                return benchmark_series.CommandResult(statuses.pop(0))

            runner = FakeRunner(respond)
            error = io.StringIO()
            report = benchmark_series.execute_series(
                plan,
                runner,
                output=io.StringIO(),
                error=error,
            )
            self.assertEqual(report.status, 9)
            self.assertEqual(len(runner.calls), 2)
            self.assertFalse(any("performance" in call.args for call in runner.calls))
            self.assertTrue(benchmark_series._acceptance_attempt(plan, 1).is_file())
            self.assertIn("nvx-run-microvm", error.getvalue())
            self.assertIn("no performance data", error.getvalue())

    def test_an_unstable_directory_attempt_retains_only_the_valid_retry(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            action = root / "action.yml"
            _write_action(action)
            plan = _plan(root, action, "smoke")
            plan.output_dir.mkdir(parents=True)
            for attempt, value in ((1, "rejected"), (2, "accepted")):
                directory = benchmark_series._performance_attempt(plan, attempt)
                directory.mkdir(parents=True)
                (directory / "result.log").write_text(value, encoding="utf-8")
            runner = StatusRunner((0, 75, 0, 0))
            executor = benchmark_series.SeriesExecutor(
                plan,
                runner,
                benchmark_series.ExecutionReport(),
                io.StringIO(),
            )
            status, attempts = benchmark_series._run_validated_directory(
                executor,
                name="performance",
                benchmark=lambda attempt: ("benchmark", str(attempt)),
                validation=lambda attempt: ("validate", str(attempt)),
                attempt_path=lambda attempt: benchmark_series._performance_attempt(
                    plan, attempt
                ),
                accepted_path=plan.performance_dir,
            )
            self.assertEqual((status, attempts), (0, 2))
            self.assertEqual(
                (plan.performance_dir / "result.log").read_text(encoding="utf-8"),
                "accepted",
            )
            self.assertTrue(benchmark_series._performance_attempt(plan, 1).is_dir())
            self.assertTrue(benchmark_series._performance_attempt(plan, 2).is_dir())


class ResultVerificationTests(unittest.TestCase):
    def test_a_complete_fixture_result_set_passes_every_check(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            action = root / "action.yml"
            _write_action(action)
            plan = _plan(root, action, "ci")
            _populate_complete_result(plan)
            checks = benchmark_series.verify_result_set(plan)
            failures = [check for check in checks if not check.passed]
            self.assertEqual(failures, [])
            self.assertGreater(len(checks), 30)

    def test_missing_result_pieces_are_reported(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            action = root / "action.yml"
            _write_action(action)
            plan = _plan(root, action)
            _populate_complete_result(plan)
            (plan.device_dir / "device-io.log").unlink()
            plan.summary_path.unlink()
            (plan.performance_results_dir / f"{plan.platform}-host.json").unlink()
            checks = benchmark_series.verify_result_set(plan)
            failed_names = {check.name for check in checks if not check.passed}
            self.assertIn("device-I/O raw log", failed_names)
            self.assertIn("summary", failed_names)
            self.assertIn("performance collected host", failed_names)

    def test_wrong_commit_sampling_and_provenance_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            action = root / "action.yml"
            _write_action(action)
            plan = _plan(root, action)
            _populate_complete_result(plan)
            metadata_path = plan.performance_dir / "benchmark-metadata.json"
            broken = _metadata(plan, "performance", 1, 99, 5, 3)
            broken["artifact_revisions"] = {"nvx": "c" * 40}
            broken["host"] = {}
            _write_json(metadata_path, broken)
            checks = benchmark_series.verify_result_set(plan)
            failed_names = {check.name for check in checks if not check.passed}
            self.assertIn("performance metadata controls", failed_names)
            self.assertIn("performance metadata commit", failed_names)
            self.assertIn("performance metadata host provenance", failed_names)


if __name__ == "__main__":
    unittest.main()
