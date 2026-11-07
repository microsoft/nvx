#!/usr/bin/env python3
"""Tests for local_gates.py, the local CI quality-gate selector and runner."""

from __future__ import annotations

import contextlib
import io
import json
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import TextIO

sys.path.insert(0, str(Path(__file__).parent))
import local_gates  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]


def _run_git(repository: Path, *arguments: str) -> str:
    return subprocess.run(
        ["git", "-C", str(repository), *arguments],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()


def _init_repository(path: Path) -> None:
    path.mkdir()
    _run_git(path, "init", "--quiet")
    for key, value in (
        ("user.name", "NVX Test"),
        ("user.email", "nvx@example.com"),
        ("commit.gpgsign", "false"),
    ):
        _run_git(path, "config", key, value)
    _commit(path, "initial")


def _commit(repository: Path, name: str) -> str:
    (repository / name).write_text(name, encoding="utf-8")
    _run_git(repository, "add", name)
    _run_git(repository, "commit", "--quiet", "-m", f"add {name}")
    return _run_git(repository, "rev-parse", "HEAD")


def _gate_ids(report: local_gates.Report) -> set[str]:
    return {gate.gate_id for gate in report.gates}


def _gate(report: local_gates.Report, gate_id: str) -> local_gates.Gate:
    return next(gate for gate in report.gates if gate.gate_id == gate_id)


class RecordingRunner:
    """A deterministic subprocess runner for git discovery and gate scripts."""

    def __init__(
        self,
        responses: Mapping[tuple[str, ...], tuple[int, str, str]] | None = None,
    ) -> None:
        self.responses = dict(responses or {})
        self.calls: list[tuple[str, ...]] = []

    def run(
        self,
        args: Sequence[str],
        *,
        cwd: Path,
        env: Mapping[str, str],
        capture_output: bool,
        stream: TextIO | None = None,
    ) -> subprocess.CompletedProcess[str]:
        del cwd, env, capture_output, stream
        command = tuple(args)
        self.calls.append(command)
        returncode, stdout, stderr = self.responses.get(command, (0, "", ""))
        return subprocess.CompletedProcess(command, returncode, stdout, stderr)


class ScriptRunner:
    """Return the exit code written in a fixture script without running it."""

    def __init__(self) -> None:
        self.calls: list[tuple[str, ...]] = []

    def run(
        self,
        args: Sequence[str],
        *,
        cwd: Path,
        env: Mapping[str, str],
        capture_output: bool,
        stream: TextIO | None = None,
    ) -> subprocess.CompletedProcess[str]:
        del cwd, env, capture_output, stream
        command = tuple(args)
        self.calls.append(command)
        if command[:2] == ("docker", "info"):
            return subprocess.CompletedProcess(command, 1, "", "unavailable")
        script = Path(command[-1]).read_text(encoding="utf-8")
        returncode = 7 if "exit 7" in script else 0
        return subprocess.CompletedProcess(command, returncode, "", "")


class GateTableTests(unittest.TestCase):
    def test_every_authoritative_scripted_step_is_mapped(self) -> None:
        actual = local_gates.scripted_step_keys(ROOT)
        mapped = {rule.key for rule in local_gates.GATE_RULES}
        self.assertEqual(mapped, actual)
        local_gates.validate_gate_table(ROOT)

    def test_ci_setup_steps_are_explicitly_skipped(self) -> None:
        setup_rules = {rule.key for rule in local_gates.GATE_RULES if rule.setup}
        self.assertTrue(setup_rules <= local_gates.CI_SETUP_STEPS)
        for job in ("nvx-cli-linux", "nvx-cli-windows"):
            self.assertIn(
                (local_gates.CI_WORKFLOW, job, "Download nvx-rs executable"),
                local_gates.CI_SETUP_STEPS,
            )


class SelectionTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.runner_temp = Path(temporary.name)

    def report(
        self,
        files: Sequence[str],
        operating_system: local_gates.OperatingSystem = "linux",
        only: Sequence[str] | None = None,
    ) -> local_gates.Report:
        return local_gates.create_report(
            base="origin/dev",
            files=files,
            operating_system=operating_system,
            root=ROOT,
            runner_temp=self.runner_temp,
            only=only,
        )

    def test_a_python_script_selects_the_python_and_always_run_gates(self) -> None:
        report = self.report(("scripts/example.py",))
        self.assertEqual(
            _gate_ids(report),
            {
                "quality-code",
                "nvx-cli",
                "markdown-links",
                "test-layout",
                "test-changes",
            },
        )

    def test_an_nvx_rs_change_selects_the_rust_gates_not_kernel_config(self) -> None:
        report = self.report(("nvx-rs/src/lib.rs",))
        gate_ids = _gate_ids(report)
        self.assertNotIn("kernel-config", gate_ids)
        self.assertIn("nvx-rs-format", gate_ids)
        self.assertIn("nvx-rs-adapter-guest", gate_ids)
        self.assertTrue(report.ci_suites.run_microvm_tests)

    def test_a_documentation_change_selects_only_the_always_run_gates(self) -> None:
        report = self.report(("doc/contribute.md",))
        self.assertEqual(
            _gate_ids(report),
            {"markdown-links", "test-layout", "test-changes", "nvx-cli"},
        )
        self.assertNotIn("quality-code", _gate_ids(report))

    def test_an_openvmm_gitlink_selects_comparisons_and_its_ci_check(self) -> None:
        report = self.report(("openvmm",))
        gate_ids = _gate_ids(report)
        self.assertIn("openvmm-vectors", gate_ids)
        self.assertIn("openvmm-cpu-profiles", gate_ids)
        self.assertIn("openvmm-ci", gate_ids)
        self.assertTrue(report.ci_suites.run_openvmm_ci_check)

    def test_only_restricts_the_selected_gate_ids(self) -> None:
        report = self.report(
            ("scripts/example.py",), only=("quality-code", "test-changes")
        )
        self.assertEqual(_gate_ids(report), {"quality-code", "test-changes"})

    def test_only_fails_for_a_gate_that_the_files_do_not_select(self) -> None:
        # A documentation change selects no quality-code step, so planning
        # nothing would pass a reproduction that runs no check.
        with self.assertRaisesRegex(local_gates.LocalGatesError, "quality-code"):
            self.report(("doc/contribute.md",), only=("quality-code",))
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            status = local_gates.main(
                ["--files", "doc/contribute.md", "--only", "quality-code"],
                root=ROOT,
            )
        self.assertEqual(status, 1)
        self.assertIn("--files", stderr.getvalue())

    def test_specula_wiring_is_blocked_on_windows(self) -> None:
        report = self.report((".github/specula/example.py",), "windows")
        self.assertIn("quality-code", _gate_ids(report))
        self.assertEqual(
            _gate(report, "specula-wiring").status,
            "blocked (Test Specula CI wiring imports POSIX fcntl)",
        )

    def test_windows_uses_the_windows_action_steps(self) -> None:
        report = self.report(("doc/contribute.md",), "windows")
        cli = _gate(report, "nvx-cli")
        self.assertEqual(cli.shell, "powershell")
        self.assertIn("python -m compileall", cli.script)
        self.assertNotIn("python3 -m compileall", cli.script)
        self.assertEqual(cli.environment["RUNNER_OS"], "Windows")

    def test_selected_paths_are_kept_for_json_and_shortened_only_in_text(self) -> None:
        files = tuple(f"scripts/change_{index}.py" for index in range(5))
        report = self.report(files, only=("quality-code",))
        quality = _gate(report, "quality-code")
        self.assertEqual(len(quality.selected_by), 5)
        output = io.StringIO()
        local_gates.print_report(report, output)
        self.assertIn("and 2 more", output.getvalue())


class DiscoveryTests(unittest.TestCase):
    def test_detection_unions_the_merge_base_diff_and_untracked_files(self) -> None:
        merge = "a" * 40
        runner = RecordingRunner(
            {
                ("git", "merge-base", "origin/dev", "HEAD"): (0, merge + "\n", ""),
                # The working tree's diff ignores the submodule's checkout...
                (
                    "git",
                    "diff",
                    "--name-only",
                    "--ignore-submodules=all",
                    merge,
                    "--",
                ): (
                    0,
                    "scripts/tracked.py\n",
                    "",
                ),
                # ...and the index's holds a staged or committed gitlink.
                ("git", "diff", "--cached", "--name-only", merge, "--"): (
                    0,
                    "openvmm\nscripts/tracked.py\n",
                    "",
                ),
                ("git", "ls-files", "--others", "--exclude-standard"): (
                    0,
                    "scripts/new.py\nscripts/tracked.py\n",
                    "",
                ),
            }
        )
        files = local_gates.detect_changed_files("origin/dev", root=ROOT, runner=runner)
        self.assertEqual(files, ("openvmm", "scripts/new.py", "scripts/tracked.py"))

    def test_a_submodule_checked_out_off_its_pin_is_no_change(self) -> None:
        # CI compares commits, so only a gitlink change selects OpenVMM's gates.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            library = root / "library"
            superproject = root / "superproject"
            for repository in (library, superproject):
                _init_repository(repository)
            first = _commit(library, "one")
            second = _commit(library, "two")
            _run_git(
                superproject,
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--quiet",
                library.as_posix(),
                "openvmm",
            )
            _run_git(superproject / "openvmm", "checkout", "--quiet", first)
            _run_git(superproject, "add", "openvmm")
            _run_git(superproject, "commit", "--quiet", "-m", "pin one")
            _run_git(superproject, "branch", "base")
            # Checking out another commit does not move the pin.
            _run_git(superproject / "openvmm", "checkout", "--quiet", second)
            self.assertEqual(
                local_gates.detect_changed_files("base", root=superproject), ()
            )
            # Staging the checkout does.
            _run_git(superproject, "add", "openvmm")
            self.assertEqual(
                local_gates.detect_changed_files("base", root=superproject),
                ("openvmm",),
            )

    def test_a_git_failure_is_an_expected_tool_error(self) -> None:
        runner = RecordingRunner(
            {
                ("git", "merge-base", "bad", "HEAD"): (
                    128,
                    "",
                    "unknown revision",
                )
            }
        )
        with self.assertRaisesRegex(local_gates.LocalGatesError, "unknown revision"):
            local_gates.detect_changed_files("bad", root=ROOT, runner=runner)


class SubstitutionTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.runner_temp = Path(temporary.name)

    def test_toolchain_and_msrv_come_from_the_repository_files(self) -> None:
        report = local_gates.create_report(
            base="origin/dev",
            files=("nvx-rs/src/lib.rs",),
            operating_system="linux",
            root=ROOT,
            runner_temp=self.runner_temp,
            only=("nvx-rs-format", "nvx-rs-msrv"),
        )
        self.assertIn('cargo "+1.97.1" fmt', _gate(report, "nvx-rs-format").script)
        self.assertIn('cargo "+1.89" check', _gate(report, "nvx-rs-msrv").script)
        self.assertNotIn("${{", _gate(report, "nvx-rs-format").script)

    def test_an_unknown_expression_is_reported_for_ci_context(self) -> None:
        substituted, unknown = local_gates.substitute_ci_expressions(
            "echo ${{ matrix.backend }}",
            root=ROOT,
            runner_temp=self.runner_temp,
            operating_system="linux",
        )
        self.assertEqual(substituted, "echo ${{ matrix.backend }}")
        self.assertEqual(unknown, ("matrix.backend",))

    def test_action_input_defaults_are_substituted(self) -> None:
        substituted, unknown = local_gates.substitute_ci_expressions(
            "echo ${{ inputs.mode }}",
            root=ROOT,
            runner_temp=self.runner_temp,
            operating_system="linux",
            input_defaults={"mode": "strict"},
        )
        self.assertEqual(substituted, "echo strict")
        self.assertEqual(unknown, ())


class EnvironmentTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        # The tool resolves the paths that it sets, which expands an 8.3 name
        # such as the RUNNER~1 of a Windows runner's temporary directory.
        self.runner_temp = Path(temporary.name).resolve()

    def report(
        self,
        operating_system: local_gates.OperatingSystem,
        only: Sequence[str],
    ) -> local_gates.Report:
        return local_gates.create_report(
            base="origin/dev",
            files=("nvx-rs/src/lib.rs",),
            operating_system=operating_system,
            root=ROOT,
            runner_temp=self.runner_temp,
            only=only,
        )

    def test_an_inherited_bundle_is_removed_unless_the_step_sets_it(self) -> None:
        report = self.report("linux", ("nvx-rs-bundling", "nvx-rs-lint"))
        inherited = {"PATH": "path", "NVX_BUNDLE": "inherited"}
        bundling = local_gates.prepare_execution_environment(
            _gate(report, "nvx-rs-bundling"), inherited
        )
        lint = local_gates.prepare_execution_environment(
            _gate(report, "nvx-rs-lint"), inherited
        )
        self.assertNotIn("NVX_BUNDLE", bundling)
        self.assertEqual(lint["NVX_BUNDLE"], "skip")

    def test_windows_nvx_cli_names_a_built_nvx_rs_and_removes_xdg_cache_home(
        self,
    ) -> None:
        report = local_gates.create_report(
            base="origin/dev",
            files=("doc/contribute.md",),
            operating_system="windows",
            root=ROOT,
            runner_temp=self.runner_temp,
            only=("nvx-cli",),
        )
        gate = _gate(report, "nvx-cli")
        built = self.runner_temp / "nvx-rs.exe"
        built.write_text("binary", encoding="utf-8")
        gate.environment["NVX_RS"] = str(built)
        environment = local_gates.prepare_execution_environment(
            gate,
            {"PATH": "path", "XDG_CACHE_HOME": "must-not-survive"},
        )
        self.assertNotIn("XDG_CACHE_HOME", environment)
        self.assertEqual(environment["NVX_RS"], str(built))

    def test_nvx_cli_leaves_nvx_rs_unset_when_it_is_not_built(self) -> None:
        # A missing executable fails the tests that run nvx-rs, as 50 of them
        # did, where an unset NVX_RS skips them.
        report = local_gates.create_report(
            base="origin/dev",
            files=("doc/contribute.md",),
            operating_system="windows",
            root=ROOT,
            runner_temp=self.runner_temp,
            only=("nvx-cli",),
        )
        gate = _gate(report, "nvx-cli")
        gate.environment["NVX_RS"] = str(self.runner_temp / "missing" / "nvx-rs.exe")
        environment = local_gates.prepare_execution_environment(gate, {"PATH": "p"})
        self.assertNotIn("NVX_RS", environment)
        # A caller's own choice survives.
        environment = local_gates.prepare_execution_environment(
            gate, {"PATH": "p", "NVX_RS": "C:/custom/nvx-rs.exe"}
        )
        self.assertEqual(environment["NVX_RS"], "C:/custom/nvx-rs.exe")

    def test_windows_prepends_git_usr_bin_to_path(self) -> None:
        report = local_gates.create_report(
            base="origin/dev",
            files=("doc/contribute.md",),
            operating_system="windows",
            root=ROOT,
            runner_temp=self.runner_temp,
            only=("nvx-cli",),
        )
        git_root = self.runner_temp / "Git"
        environment = local_gates.prepare_execution_environment(
            _gate(report, "nvx-cli"),
            {"PATH": "original"},
            git_root=git_root,
        )
        self.assertTrue(environment["PATH"].startswith(str(git_root / "usr" / "bin")))

    def test_required_ci_environment_points_into_the_runner_temp(self) -> None:
        report = self.report("linux", ("nvx-rs-format",))
        environment = _gate(report, "nvx-rs-format").environment
        for name in ("GITHUB_OUTPUT", "GITHUB_ENV", "GITHUB_STEP_SUMMARY"):
            self.assertTrue(Path(environment[name]).is_file())
            self.assertTrue(Path(environment[name]).is_relative_to(self.runner_temp))
        self.assertEqual(environment["GITHUB_WORKSPACE"], str(ROOT.resolve()))


class FixtureActionTests(unittest.TestCase):
    def setUp(self) -> None:
        root_directory = tempfile.TemporaryDirectory()
        runner_directory = tempfile.TemporaryDirectory()
        self.addCleanup(root_directory.cleanup)
        self.addCleanup(runner_directory.cleanup)
        self.root = Path(root_directory.name)
        self.runner_temp = Path(runner_directory.name)
        self.git_root = self.root / "Git"
        (self.git_root / "cmd").mkdir(parents=True)
        (self.git_root / "bin").mkdir(parents=True)
        (self.git_root / "usr" / "bin").mkdir(parents=True)
        (self.git_root / "cmd" / "git.exe").touch()
        (self.git_root / "bin" / "bash.exe").touch()
        action = self.root / "fixture" / "action.yml"
        action.parent.mkdir(parents=True)
        action.write_text(
            """name: Fixture
runs:
  using: composite
  steps:
    - name: Pass
      shell: bash
      run: echo passed
    - name: Fail
      shell: bash
      run: exit 7
    - name: Partial
      shell: bash
      env:
        SHELLCHECK: docker shellcheck
        SHFMT: docker shfmt
      run: |
        ${SHELLCHECK} --version
        ${SHFMT} --version
    - name: Block
      shell: bash
      run: 'echo ${{ matrix.value }}'
""",
            encoding="utf-8",
        )
        self.definitions = local_gates.DefinitionSet(("fixture/action.yml",), ())
        self.rules = (
            local_gates.GateRule(
                "fixture-pass", "fixture/action.yml", "Pass", local_gates.ALWAYS
            ),
            local_gates.GateRule(
                "fixture-fail", "fixture/action.yml", "Fail", local_gates.ALWAYS
            ),
            local_gates.GateRule(
                "quality-code", "fixture/action.yml", "Partial", local_gates.ALWAYS
            ),
            local_gates.GateRule(
                "fixture-block", "fixture/action.yml", "Block", local_gates.ALWAYS
            ),
        )

    def tool_finder(self, name: str, path: str | None = None) -> str | None:
        del path
        if name in {"git", "git.exe"}:
            return str(self.git_root / "cmd" / "git.exe")
        if name == "bash":
            return "fixture-bash"
        return None

    def report(self, only: Sequence[str]) -> local_gates.Report:
        return local_gates.create_report(
            base="base",
            files=("fixture/input",),
            operating_system="linux",
            root=self.root,
            runner_temp=self.runner_temp,
            only=only,
            rules=self.rules,
            definitions=self.definitions,
        )

    def execute(self, report: local_gates.Report) -> int:
        return local_gates.execute_report(
            report,
            root=self.root,
            runner_temp=self.runner_temp,
            runner=ScriptRunner(),
            tool_finder=self.tool_finder,
            keep_going=True,
            stream=io.StringIO(),
            inherited_environment={"PATH": "fixture"},
        )

    def test_fixture_steps_report_passed_failed_partial_and_blocked(self) -> None:
        report = self.report(
            ("fixture-pass", "fixture-fail", "quality-code", "fixture-block")
        )
        self.assertEqual(self.execute(report), 1)
        self.assertEqual(_gate(report, "fixture-pass").state, "passed")
        self.assertEqual(_gate(report, "fixture-fail").status, "failed (exit code 7)")
        self.assertEqual(
            _gate(report, "quality-code").status,
            "partial (skipped shellcheck, shfmt)",
        )
        self.assertEqual(
            _gate(report, "fixture-block").status,
            "blocked (needs CI context: matrix.value)",
        )

    def test_partial_counts_as_passed(self) -> None:
        report = self.report(("fixture-pass", "quality-code"))
        self.assertEqual(self.execute(report), 0)

    def test_a_blocked_gate_without_a_failure_returns_two(self) -> None:
        report = self.report(("fixture-block",))
        self.assertEqual(self.execute(report), 2)

    def test_execution_stops_after_the_first_failure_by_default(self) -> None:
        report = self.report(("fixture-fail", "fixture-pass"))
        report.gates.sort(key=lambda gate: gate.gate_id != "fixture-fail")
        result = local_gates.execute_report(
            report,
            root=self.root,
            runner_temp=self.runner_temp,
            runner=ScriptRunner(),
            tool_finder=self.tool_finder,
            stream=io.StringIO(),
            inherited_environment={"PATH": "fixture"},
        )
        self.assertEqual(result, 1)
        self.assertEqual(_gate(report, "fixture-pass").state, "not run")


class OutputTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.runner_temp = Path(temporary.name)

    def report(self, files: Sequence[str]) -> local_gates.Report:
        return local_gates.create_report(
            base="origin/dev",
            files=files,
            operating_system="linux",
            root=ROOT,
            runner_temp=self.runner_temp,
        )

    def test_json_has_the_documented_shape(self) -> None:
        document = json.loads(json.dumps(self.report(("doc/contribute.md",)).as_json()))
        self.assertEqual(set(document), {"base", "changed_files", "gates", "ci_suites"})
        gate = document["gates"][0]
        self.assertEqual(
            set(gate),
            {
                "id",
                "selected_by",
                "source",
                "os",
                "shell",
                "working_directory",
                "env",
                "script",
                "requirements",
                "status",
            },
        )
        self.assertEqual(set(gate["source"]), {"file", "job", "step"})

    def test_json_cli_writes_one_document(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()
        original_stdout, original_stderr = sys.stdout, sys.stderr
        try:
            sys.stdout, sys.stderr = stdout, stderr
            result = local_gates.main(
                ("--files", "doc/contribute.md", "--json"), root=ROOT
            )
        finally:
            sys.stdout, sys.stderr = original_stdout, original_stderr
        self.assertEqual(result, 0)
        self.assertEqual(stderr.getvalue(), "")
        self.assertEqual(json.loads(stdout.getvalue())["base"], "origin/dev")

    def test_test_changes_uses_the_requested_base(self) -> None:
        report = local_gates.create_report(
            base="feature/base",
            files=("doc/contribute.md",),
            operating_system="linux",
            root=ROOT,
            runner_temp=self.runner_temp,
            only=("test-changes",),
        )
        self.assertEqual(
            _gate(report, "test-changes").script,
            "python3 scripts/check_test_changes.py --base feature/base",
        )

    def test_microvm_suite_lists_each_scenario_for_the_local_backends(self) -> None:
        suites = self.report(("nvx-rs/src/lib.rs",)).ci_suites
        self.assertEqual(suites.backends, ("kvm", "mshv"))
        self.assertTrue(suites.scenarios)
        self.assertIn(
            "python3 scripts/nvx.py test-microvm --backend kvm --scenario lifecycle",
            suites.commands,
        )

    def test_text_names_the_base_files_gates_and_ci_suites(self) -> None:
        output = io.StringIO()
        local_gates.print_report(self.report(("doc/contribute.md",)), output)
        text = output.getvalue()
        self.assertIn("Base: origin/dev", text)
        self.assertIn("Changed files: 1", text)
        self.assertIn("Gate: markdown-links", text)
        self.assertIn("CI suites:", text)


if __name__ == "__main__":
    unittest.main()
