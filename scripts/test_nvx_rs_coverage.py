#!/usr/bin/env python3
# pyright: reportPrivateUsage=false
"""Tests for nvx_rs_coverage.py, which measures the golden-test families' reach.

The script runs the nvx crate's tests under cargo-llvm-cov once for each
selection of tests and summarizes, for each family of golden tests, what only
it reaches (#726). These tests cover its selections, its families, its reading
of test listings, test-only modules, and LCOV reports, its analysis and
summary, and the commands that it runs, with a fake in place of Cargo. They
also check the triggers of the workflows that measure coverage and run
cargo-mutants, and, on POSIX hosts, run the mutants workflow's steps.
"""

from __future__ import annotations

import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Collection, Iterable, Mapping, Sequence
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path, PurePosixPath
from typing import Any, cast
from unittest.mock import patch

import yaml

sys.path.insert(0, str(Path(__file__).parent))
import nvx_rs_coverage as coverage  # noqa: E402

FAMILIES = (
    coverage.Family("first", "First family", ("module::first::",)),
    coverage.Family("second", "Second family", ("second_test", "third_test")),
)
LISTING = "module::first::case: test\nsecond_test: test\nx::third_test: test\n"
# Each test-only module of the measurements below.
TEST_MODULES = ("src/a/tests.rs", "src/a/tests/")


def _write(root: Path, files: Mapping[str, str]) -> None:
    for name, text in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")


def _lcov(lines: Collection[int], branches: Collection[int] = ()) -> str:
    """Returns the LCOV report of a run that reached `lines` of src/a.rs, of
    lines 1 to 10, and the outcomes `branches` of its two-way branch on line 2.

    Lines of test code, which the analysis ignores, are always reached.
    """
    records = ["TN:", "SF:src\\a.rs"]
    records += [f"DA:{line},{int(line in lines)}" for line in range(1, 11)]
    records += [
        f"BRDA:2,0,{outcome},{'1' if outcome in branches else '-'}"
        for outcome in (0, 1)
    ]
    records += ["end_of_record", "SF:src/a/tests.rs", "DA:1,1", "end_of_record"]
    records += ["SF:tests/cli.rs", "DA:1,1", "end_of_record"]
    return "\n".join(records) + "\n"


# What each selection of FAMILIES reaches, as the lines and branch outcomes of
# _lcov: the first family alone reaches lines 2 and 3 and the first outcome of
# the branch, the second family lines 4 and 5, and the other tests lines 1 and
# 6 and the second outcome.
REACHED = {
    "all": ({1, 2, 3, 4, 5, 6}, {0, 1}),
    "only-first": ({1, 2, 3}, {0}),
    "only-second": ({4, 5}, set[int]()),
    "without-first": ({1, 4, 5, 6}, {1}),
    "without-second": ({1, 2, 3, 6}, {0, 1}),
    "without-families": ({1, 6}, {1}),
}


def _measurement(
    directory: Path,
    host: str = "Linux",
    reports: Mapping[str, str] | None = None,
    failed: Iterable[str] = (),
    retried: Iterable[str] = (),
) -> Path:
    """Writes a measurement of FAMILIES to `directory`, with the reports of
    REACHED unless `reports` replaces them, whose selections `failed` failed in
    both runs and `retried` failed and then passed."""
    directory.mkdir(parents=True, exist_ok=True)
    texts = {name: _lcov(*reached) for name, reached in REACHED.items()} | dict(
        reports or {}
    )
    for name, text in texts.items():
        (directory / f"{name}.lcov").write_text(text, encoding="utf-8")
    statuses = {name: [101, 101] for name in failed} | {
        name: [101, 0] for name in retried
    }
    measurement = {
        "format": 1,
        "host": host,
        "commit": "0123456789abcdef",
        "toolchain": "nightly-2026-07-09",
        "rustc": "rustc 1.99.0-nightly",
        "cargo_llvm_cov": "cargo-llvm-cov 0.9.1",
        "families": [
            {"slug": family.slug, "title": family.title, "filters": family.filters}
            for family in FAMILIES
        ],
        "test_modules": TEST_MODULES,
        "selections": [
            {
                "name": selection.name,
                "arguments": selection.arguments,
                "statuses": statuses.get(selection.name, [0]),
                "seconds": 1.0,
                "report": f"{selection.name}.lcov",
            }
            for selection in coverage.selections(FAMILIES)
        ],
    }
    (directory / coverage.MEASUREMENT_FILE).write_text(
        json.dumps(measurement), encoding="utf-8"
    )
    return directory


class SelectionTests(unittest.TestCase):
    def test_each_family_runs_alone_and_the_other_tests_without_it(self):
        self.assertEqual(
            [
                (selection.name, selection.arguments)
                for selection in coverage.selections(FAMILIES)
            ],
            [
                ("all", ()),
                ("only-first", ("module::first::",)),
                ("only-second", ("second_test", "third_test")),
                ("without-first", ("--skip", "module::first::")),
                (
                    "without-second",
                    ("--skip", "second_test", "--skip", "third_test"),
                ),
                (
                    "without-families",
                    (
                        "--skip",
                        "module::first::",
                        "--skip",
                        "second_test",
                        "--skip",
                        "third_test",
                    ),
                ),
            ],
        )

    def test_listings_name_the_tests_of_every_binary_once(self):
        listing = (
            "a::b: test\nc: test\nd: benchmark\n\n2 tests, 1 benchmark\n"
            "c: test\n0 tests, 0 benchmarks\n"
        )
        self.assertEqual(coverage.listed_tests(listing), ["a::b", "c"])

    def test_every_family_filter_has_to_select_a_test(self):
        tests = coverage.listed_tests(LISTING)
        coverage.check_families(FAMILIES, tests)
        with self.assertRaisesRegex(
            coverage.MeasurementError, r"select no test: third_test \(Second family\)$"
        ):
            coverage.check_families(FAMILIES, tests[:-1])


class FamilyTests(unittest.TestCase):
    """The families name tests that the crate holds, so that renaming one of
    them fails here, before a measurement does."""

    def test_the_families_name_tests_of_the_crate(self):
        crate = coverage.CRATE_DIR
        for family in coverage.FAMILIES:
            for name in family.filters:
                with self.subTest(family=family.title, filter=name):
                    *modules, function = name.split("::")
                    if modules:
                        stem = crate.joinpath("src", *modules)
                        files = [stem.with_suffix(".rs"), stem / "mod.rs"]
                    else:
                        files = sorted((crate / "tests").glob("*.rs"))
                    sources = [
                        path.read_text(encoding="utf-8")
                        for path in files
                        if path.is_file()
                    ]
                    self.assertTrue(sources, f"{name} names no module of the crate")
                    if function:
                        self.assertTrue(
                            any(f"fn {function}(" in text for text in sources),
                            f"{name} names no test of the crate",
                        )

    def test_the_families_have_distinct_slugs(self):
        slugs = [family.slug for family in coverage.FAMILIES]
        self.assertEqual(len(slugs), len(set(slugs)))


class TestModuleTests(unittest.TestCase):
    def test_test_only_modules_are_those_that_cfg_test_declares(self):
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory)
            _write(
                crate,
                {
                    "src/lib.rs": "#[cfg(test)]\nmod tests;\n"
                    '#[cfg(feature = "testing")]\npub mod testing;\n'
                    "#[cfg(not(test))]\nmod real;\nmod host;\n",
                    "src/tests.rs": "mod nested;\n",
                    "src/tests/nested.rs": "",
                    "src/testing.rs": "",
                    "src/real.rs": "",
                    "src/host/mod.rs": '#[cfg(all(test, feature = "host"))]\n'
                    "/// A harness.\n\npub(crate) mod golden;\n"
                    "#[cfg(test)]\nmod inline {\n}\nmod plan;\n",
                    "src/host/golden.rs": "",
                    "src/host/plan.rs": "#[cfg(all(test, any("
                    'target_os = "linux", windows)))]\nmod tests;\n',
                    "src/host/plan/tests/mod.rs": "",
                },
            )
            self.assertEqual(
                coverage.test_modules(crate),
                [
                    "src/host/golden.rs",
                    "src/host/golden/",
                    "src/host/plan/tests/",
                    "src/host/plan/tests/mod.rs",
                    "src/tests.rs",
                    "src/tests/",
                ],
            )

    def test_a_test_module_without_a_file_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory)
            _write(crate, {"src/lib.rs": "#[cfg(test)]\nmod missing;\n"})
            with self.assertRaisesRegex(
                coverage.MeasurementError,
                r"^src/lib\.rs: the test module missing has no file$",
            ):
                coverage.test_modules(crate)

    def test_the_crate_keeps_its_golden_tests_in_test_modules(self):
        modules = coverage.test_modules(coverage.CRATE_DIR)
        for module in (
            "src/exec/tests/",
            "src/guest/managed_agent/loopback.rs",
            "src/host/network/policy/tests.rs",
            "src/host/python/golden.rs",
            "src/host/sandbox/specification.rs",
            "src/host/test_support.rs",
        ):
            self.assertIn(module, modules)
        self.assertFalse([module for module in modules if "testing" in module])

    def test_production_code_is_outside_the_test_modules_and_tests(self):
        modules = ["src/host/golden.rs", "src/host/golden/", "src/tests.rs"]
        for path in ("src/host/plan.rs", "src/host/golden_rules.rs", "src/lib.rs"):
            self.assertTrue(coverage.is_production(path, modules), path)
        for path in (
            "src/host/golden.rs",
            "src/host/golden/cases.rs",
            "src/tests.rs",
            "tests/cli.rs",
        ):
            self.assertFalse(coverage.is_production(path, modules), path)


class LcovTests(unittest.TestCase):
    def test_reports_give_whether_runs_reached_each_line_and_branch(self):
        report = coverage.read_lcov(
            "TN:\nSF:src\\host\\cli.rs\nFN:3,main\nFNDA:1,main\nDA:3,2\nDA:4,0\n"
            "BRDA:4,0,0,-\nBRDA:4,0,1,-\nBRDA:5,0,0,3\nBRDA:5,0,1,0\n"
            "LF:2\nLH:1\nend_of_record\n"
            "SF:./src/lib.rs\nDA:1,0\nDA:1,4\nBRDA:1,0,0,0\nBRDA:1,0,0,2\n"
            "end_of_record\n"
        )
        self.assertEqual(
            report.lines,
            {
                ("src/host/cli.rs", 3): True,
                ("src/host/cli.rs", 4): False,
                ("src/lib.rs", 1): True,
            },
        )
        self.assertEqual(
            report.branches,
            {
                ("src/host/cli.rs", 4, 0, 0): False,
                ("src/host/cli.rs", 4, 0, 1): False,
                ("src/host/cli.rs", 5, 0, 0): True,
                ("src/host/cli.rs", 5, 0, 1): False,
                ("src/lib.rs", 1, 0, 0): True,
            },
        )

    def test_malformed_records_fail_with_their_line(self):
        with self.assertRaisesRegex(
            coverage.MeasurementError, r"^line 3: malformed BRDA record"
        ):
            coverage.read_lcov("SF:src/a.rs\nDA:1,1\nBRDA:1,0,1\n")

    def test_paths_have_to_be_relative_to_the_crate(self):
        for path in ("C:\\work\\nvx-rs\\src\\a.rs", "/work/nvx-rs/src/a.rs"):
            with self.subTest(path=path):
                with self.assertRaisesRegex(
                    coverage.MeasurementError, r"--remap-path-prefix"
                ):
                    coverage.read_lcov(f"SF:{path}\nDA:1,1\nend_of_record\n")


class AnalysisTests(unittest.TestCase):
    def test_each_family_reaches_what_its_selections_differ_by(self):
        with tempfile.TemporaryDirectory() as directory:
            analysis = coverage.analyze(_measurement(Path(directory)))
        source = "src/a.rs"
        first, second = analysis.families
        self.assertEqual(
            first.only,
            coverage.Reach(
                frozenset({(source, 2), (source, 3)}), frozenset({(source, 2, 0, 0)})
            ),
        )
        self.assertEqual(first.beyond, first.only)
        self.assertEqual(
            second.only,
            coverage.Reach(frozenset({(source, 4), (source, 5)}), frozenset()),
        )
        self.assertEqual(second.beyond, second.only)
        self.assertEqual(
            analysis.together,
            coverage.Reach(
                frozenset((source, line) for line in (2, 3, 4, 5)),
                frozenset({(source, 2, 0, 0)}),
            ),
        )
        self.assertEqual(len(analysis.instrumented.lines), 10)
        self.assertEqual(len(analysis.instrumented.branches), 2)
        self.assertEqual(len(analysis.reached.lines), 6)
        self.assertEqual(analysis.failed, ())

    def test_what_other_families_reach_too_is_not_only_a_familys(self):
        # The second family reaches line 2 too, so only the first reaches line 3.
        reports = {"without-first": _lcov({1, 2, 4, 5, 6}, {1})}
        with tempfile.TemporaryDirectory() as directory:
            analysis = coverage.analyze(_measurement(Path(directory), reports=reports))
        first = analysis.families[0]
        self.assertEqual(first.only.lines, frozenset({("src/a.rs", 3)}))
        self.assertEqual(
            first.beyond.lines, frozenset({("src/a.rs", 2), ("src/a.rs", 3)})
        )

    def test_reports_have_to_instrument_the_same_lines(self):
        reports = {"only-second": _lcov({4, 5}).replace("DA:10,0\n", "")}
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(
                coverage.MeasurementError, r"only-second and all instrument different"
            ):
                coverage.analyze(_measurement(Path(directory), reports=reports))

    def test_measurements_need_every_selection(self):
        with tempfile.TemporaryDirectory() as directory:
            measurement = _measurement(Path(directory))
            path = measurement / coverage.MEASUREMENT_FILE
            document = json.loads(path.read_text(encoding="utf-8"))
            document["selections"].pop()
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(coverage.MeasurementError, "the selections"):
                coverage.analyze(measurement)

    def test_measurements_of_another_format_fail(self):
        with tempfile.TemporaryDirectory() as directory:
            measurement = _measurement(Path(directory))
            path = measurement / coverage.MEASUREMENT_FILE
            path.write_text('{"format": 2}', encoding="utf-8")
            with self.assertRaisesRegex(coverage.MeasurementError, "format 1"):
                coverage.analyze(measurement)


class SummaryTests(unittest.TestCase):
    def test_summaries_count_and_locate_what_each_family_reaches(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            linux = _measurement(root / "linux", retried=["only-second"])
            windows = _measurement(root / "windows", host="Windows", failed=["all"])
            json_path = root / "summary.json"
            markdown = coverage.summarize([linux, windows], json_path)
            document = json.loads(json_path.read_text(encoding="utf-8"))
        self.assertIn(
            "| Family | Linux lines | Linux branches | Windows lines | "
            "Windows branches |\n| --- | ---: | ---: | ---: | ---: |\n"
            "| First family | 2 | 1 | 2 | 1 |\n"
            "| Second family | 2 | 0 | 2 | 0 |\n"
            "| All families | 4 | 1 | 4 | 1 |\n",
            markdown,
        )
        self.assertIn(
            "- Linux, commit `0123456789abcdef`, `nightly-2026-07-09`: the tests "
            "reach 6 of 10 lines and 2 of 2 branches of production code.",
            markdown,
        )
        self.assertIn("  Tests failed in all, whose reports lack", markdown)
        self.assertIn(
            "  Tests failed in only-second, and passed when they ran again.\n",
            markdown,
        )
        self.assertIn(
            "<summary>Windows: what only First family reach (2 lines, 1 "
            "branch)</summary>\n\n- `src/a.rs`: lines 2-3; branches on lines 2\n",
            markdown,
        )
        self.assertIn(
            "<summary>Linux: what only all families reach (4 lines, 1 branch)"
            "</summary>\n\n- `src/a.rs`: lines 2-5; branches on lines 2\n",
            markdown,
        )
        first = document["measurements"][0]["families"][0]
        self.assertEqual(first["only"]["lines"], [["src/a.rs", 2], ["src/a.rs", 3]])
        self.assertEqual(first["only"]["branches"], [["src/a.rs", 2, 0, 0]])
        self.assertEqual(document["measurements"][1]["failed"], ["all"])
        self.assertEqual(document["measurements"][0]["retried"], ["only-second"])

    def test_measurements_of_one_host_are_told_apart_by_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            markdown = coverage.summarize(
                [_measurement(root / "before"), _measurement(root / "after")], None
            )
        self.assertIn("| Family | Linux (before) lines |", markdown)
        self.assertIn("| Linux (after) branches |", markdown)

    def test_line_ranges_join_consecutive_lines(self):
        self.assertEqual(coverage._ranges([7, 1, 2, 3, 9, 10, 2]), "1-3, 7, 9-10")


class FakeCargo:
    """Runs the commands of a measurement as Cargo would, without building.

    Its target directory is `target`. The tests of each selection in `failing`
    fail in every run, and those of each selection in `flaky` in its first run
    only. The run of each selection in `unreadable` leaves a profile there that
    its owner can only write, as a test's process with a restrictive umask does,
    and a report fails while a profile is so.
    """

    def __init__(
        self,
        target: Path,
        listing: str = LISTING,
        failing: Collection[str] = (),
        flaky: Collection[str] = (),
        unreadable: Collection[str] = (),
    ):
        self.target = target
        self.listing = listing
        self.failing = failing
        self.flaky = flaky
        self.unreadable = unreadable
        self.commands: list[tuple[str, ...]] = []
        self.runs: dict[str, int] = {}

    def __call__(
        self, command: Sequence[str], capture: bool
    ) -> subprocess.CompletedProcess[str]:
        self.commands.append(tuple(command))
        status, stdout = 0, ""
        profiles = self.target / "llvm-cov-target"
        if command[0] == "git":
            stdout = "0123456789abcdef\n"
        elif "metadata" in command:
            stdout = json.dumps({"target_directory": str(self.target)})
        elif "--version" in command:
            stdout = f"{command[0]} 1.0\n"
        elif "--list" in command:
            stdout = self.listing
        elif "--profraw-only" in command:
            for profile in profiles.glob("*.profraw"):
                profile.unlink()
        elif "report" in command:
            if any(
                profile.stat().st_mode & 0o600 != 0o600
                for profile in profiles.glob("*.profraw")
            ):
                status = 1
            else:
                output = Path(command[command.index("--output-path") + 1])
                output.write_text(_lcov(*REACHED[output.stem]), encoding="utf-8")
        elif "--no-report" in command:
            arguments = tuple(command[command.index("--") + 1 :])
            for selection in coverage.selections(FAMILIES):
                if selection.arguments != arguments:
                    continue
                runs = self.runs[selection.name] = self.runs.get(selection.name, 0) + 1
                if selection.name in self.failing or (
                    selection.name in self.flaky and runs == 1
                ):
                    status = 101
                if selection.name in self.unreadable:
                    profiles.mkdir(parents=True, exist_ok=True)
                    profile = profiles / f"{selection.name}.profraw"
                    profile.write_bytes(b"profile")
                    profile.chmod(0o200)
        return subprocess.CompletedProcess(command, status, stdout if capture else None)


class MeasureTests(unittest.TestCase):
    def _measure(self, cargo: FakeCargo) -> tuple[int, str]:
        """Measures with `cargo` beside its target directory, and returns the
        status and what the measurement printed on standard error."""
        root = cargo.target.parent
        crate = root / "crate"
        _write(crate, {"src/lib.rs": "#[cfg(test)]\nmod tests;\n", "src/tests.rs": ""})
        stderr = io.StringIO()
        with (
            redirect_stderr(stderr),
            patch.dict("os.environ", {"CARGO_LLVM_COV_TARGET_DIR": ""}),
        ):
            status = coverage.measure(
                root / "out", "nightly-x", run=cargo, crate=crate, families=FAMILIES
            )
        return status, stderr.getvalue()

    def test_measurements_run_every_selection_on_one_build(self):
        with tempfile.TemporaryDirectory() as directory:
            cargo = FakeCargo(Path(directory) / "target")
            self.assertEqual(self._measure(cargo)[0], 0)
            output = Path(directory) / "out"
            measurement = json.loads(
                (output / coverage.MEASUREMENT_FILE).read_text(encoding="utf-8")
            )
            tests = (output / coverage.TESTS_FILE).read_text(encoding="utf-8")
            analysis = coverage.analyze(output)
        prefix = ("cargo", "+nightly-x", "llvm-cov")
        run = (*prefix, "--no-report", *coverage.RUN_OPTIONS)
        self.assertEqual(cargo.commands[0], (*prefix, "clean", "--workspace"))
        self.assertEqual(
            cargo.commands[1],
            ("cargo", "+nightly-x", "metadata", "--format-version", "1", "--no-deps"),
        )
        self.assertEqual(cargo.commands[2], (*run, "--", "--list"))
        names = [selection.name for selection in coverage.selections(FAMILIES)]
        for index, selection in enumerate(coverage.selections(FAMILIES)):
            clean, test, report = cargo.commands[3 + 3 * index : 6 + 3 * index]
            self.assertEqual(clean, (*prefix, "clean", "--profraw-only"))
            self.assertEqual(test, (*run, "--no-fail-fast", "--", *selection.arguments))
            self.assertEqual(
                report[:-1],
                (
                    *prefix,
                    "report",
                    *coverage.REPORT_OPTIONS,
                    "--lcov",
                    "--output-path",
                ),
            )
            self.assertEqual(Path(report[-1]).name, f"{selection.name}.lcov")
        self.assertEqual(tests, "module::first::case\nsecond_test\nx::third_test\n")
        self.assertEqual(measurement["toolchain"], "nightly-x")
        self.assertEqual(measurement["commit"], "0123456789abcdef")
        self.assertEqual(measurement["rustc"], "rustc 1.0")
        self.assertEqual(measurement["test_modules"], ["src/tests.rs", "src/tests/"])
        self.assertEqual([item["name"] for item in measurement["selections"]], names)
        self.assertEqual(
            [item["statuses"] for item in measurement["selections"]], [[0]] * len(names)
        )
        self.assertEqual(len(analysis.families[0].only.lines), 2)

    def test_failed_tests_fail_the_measurement_after_every_selection(self):
        with tempfile.TemporaryDirectory() as directory:
            cargo = FakeCargo(Path(directory) / "target", failing=["only-first"])
            status, messages = self._measure(cargo)
            output = Path(directory) / "out"
            measurement = json.loads(
                (output / coverage.MEASUREMENT_FILE).read_text(encoding="utf-8")
            )
            reports = sorted(path.name for path in output.glob("*.lcov"))
        self.assertEqual(status, 1)
        self.assertIn("Running the tests of only-first again\n", messages)
        self.assertTrue(messages.endswith("error: tests failed in only-first\n"))
        statuses = {
            item["name"]: item["statuses"] for item in measurement["selections"]
        }
        self.assertEqual(statuses.pop("only-first"), [101, 101])
        self.assertEqual(list(statuses.values()), [[0]] * len(statuses))
        self.assertEqual(len(reports), len(coverage.selections(FAMILIES)))

    def test_tests_that_fail_once_pass_when_their_selection_runs_again(self):
        names = [selection.name for selection in coverage.selections(FAMILIES)]
        with tempfile.TemporaryDirectory() as directory:
            cargo = FakeCargo(Path(directory) / "target", flaky=["without-first"])
            status, messages = self._measure(cargo)
            analysis = coverage.analyze(Path(directory) / "out")
        self.assertEqual(status, 0)
        self.assertEqual(messages, "Running the tests of without-first again\n")
        self.assertEqual(
            cargo.runs, {name: 1 + (name == "without-first") for name in names}
        )
        # Each run starts from no profiles, so the report holds the last one's.
        cleans = [command for command in cargo.commands if "--profraw-only" in command]
        self.assertEqual(len(cleans), len(names) + 1)
        self.assertEqual((analysis.failed, analysis.retried), ((), ("without-first",)))

    def test_families_that_select_no_test_fail_before_any_selection(self):
        with tempfile.TemporaryDirectory() as directory:
            cargo = FakeCargo(
                Path(directory) / "target",
                listing="second_test: test\nthird_test: test\n",
            )
            with self.assertRaisesRegex(coverage.MeasurementError, "module::first::"):
                self._measure(cargo)
        self.assertEqual(len(cargo.commands), 3)

    @unittest.skipIf(sys.platform == "win32", "Windows has no owner permissions")
    def test_profiles_that_tests_leave_unreadable_become_readable(self):
        with tempfile.TemporaryDirectory() as directory:
            cargo = FakeCargo(
                Path(directory) / "target", unreadable=["all", "without-second"]
            )
            status, messages = self._measure(cargo)
            analysis = coverage.analyze(Path(directory) / "out")
        self.assertEqual(status, 0)
        self.assertIn("Let the owner read and write 1 profile of all\n", messages)
        self.assertIn("1 profile of without-second\n", messages)
        self.assertEqual(messages.count("Let the owner"), 2)
        self.assertEqual(analysis.failed, ())

    def test_cargo_llvm_cov_target_dir_holds_the_profiles_if_set(self):
        with tempfile.TemporaryDirectory() as directory:
            cargo = FakeCargo(Path(directory) / "target")
            crate = Path(directory) / "crate"
            # cargo-llvm-cov runs in the crate, where a relative path points.
            for override, profiles in (
                (directory, Path(directory)),
                ("coverage-target", crate / "coverage-target"),
            ):
                with patch.dict("os.environ", {"CARGO_LLVM_COV_TARGET_DIR": override}):
                    self.assertEqual(
                        coverage.profile_directory(cargo, "nightly-x", crate), profiles
                    )
            self.assertEqual(cargo.commands, [])
            with patch.dict("os.environ", {"CARGO_LLVM_COV_TARGET_DIR": ""}):
                self.assertEqual(
                    coverage.profile_directory(cargo, "nightly-x", crate),
                    Path(directory) / "target" / "llvm-cov-target",
                )


class CommandLineTests(unittest.TestCase):
    def test_summarize_prints_the_summary(self):
        with tempfile.TemporaryDirectory() as directory:
            measurement = _measurement(Path(directory))
            stdout = io.StringIO()
            with redirect_stdout(stdout):
                status = coverage.main(["summarize", str(measurement)])
        self.assertEqual(status, 0)
        self.assertTrue(stdout.getvalue().startswith("## nvx-rs coverage\n"))

    def test_errors_print_their_message(self):
        with tempfile.TemporaryDirectory() as directory:
            stderr = io.StringIO()
            with redirect_stderr(stderr):
                status = coverage.main(["summarize", directory])
        self.assertEqual(status, 1)
        self.assertTrue(stderr.getvalue().startswith("error: "))


def _workflow(name: str) -> dict[Any, Any]:
    path = coverage.REPO_ROOT / ".github" / "workflows" / name
    return cast(dict[Any, Any], yaml.safe_load(path.read_text(encoding="utf-8")))


def _step(workflow: Mapping[Any, Any], job: str, name: str) -> dict[str, Any]:
    steps = cast(list[dict[str, Any]], workflow["jobs"][job]["steps"])
    return next(step for step in steps if step.get("name") == name)


def _artifact(name: str) -> str:
    """Returns an artifact name or pattern of a workflow, with sample values in
    place of the expressions that it holds."""
    for expression, value in (
        ("${{ github.run_id }}", "1234"),
        ("${{ runner.os }}", "Linux"),
        ("${{ matrix.index }}", "0"),
    ):
        name = name.replace(expression, value)
    if "${{" in name:
        raise AssertionError(f"{name} holds an expression without a sample value")
    return name


class WorkflowTests(unittest.TestCase):
    def test_reruns_summarize_the_latest_artifact_of_every_job(self):
        for name, uploads, download in (
            (
                "nvx-rs-coverage.yml",
                (
                    ("measure", "Upload the measurement"),
                    ("summarize", "Upload the summary"),
                ),
                ("summarize", "Download the measurements"),
            ),
            (
                "nvx-rs-mutants.yml",
                (
                    ("mutants", "Upload the outcomes"),
                    ("summarize", "Upload the merged outcomes"),
                ),
                ("summarize", "Download the outcomes"),
            ),
        ):
            with self.subTest(workflow=name):
                workflow = _workflow(name)
                job, summary = (_step(workflow, *upload)["with"] for upload in uploads)
                pattern = _artifact(_step(workflow, *download)["with"]["pattern"])
                # Names hold no attempt, so that a rerun of a job replaces the
                # artifact of its earlier attempt, and the summary downloads the
                # latest artifact of every job, but not its own.
                for artifact in (job, summary):
                    self.assertNotIn("run_attempt", artifact["name"])
                    self.assertIs(artifact["overwrite"], True)
                self.assertTrue(PurePosixPath(_artifact(job["name"])).match(pattern))
                self.assertFalse(
                    PurePosixPath(_artifact(summary["name"])).match(pattern)
                )

    def test_the_workflows_never_run_for_pull_requests(self):
        for name, paths in (
            (
                "nvx-rs-coverage.yml",
                [".github/workflows/nvx-rs-coverage.yml", "scripts/nvx_rs_coverage.py"],
            ),
            ("nvx-rs-mutants.yml", [".github/workflows/nvx-rs-mutants.yml"]),
        ):
            with self.subTest(workflow=name):
                workflow = _workflow(name)
                # YAML 1.1 reads the key `on` as true.
                triggers = cast(dict[str, Any], workflow.get("on", workflow.get(True)))
                self.assertNotIn("pull_request", triggers)
                self.assertNotIn("pull_request_target", triggers)
                self.assertIn("workflow_dispatch", triggers)
                # A push revalidates the workflow when it changes.
                self.assertEqual(triggers["push"], {"paths": paths})
                self.assertEqual(workflow["permissions"], {"contents": "read"})

    def test_the_coverage_workflow_measures_both_systems_with_pinned_tools(self):
        workflow = _workflow("nvx-rs-coverage.yml")
        self.assertRegex(
            workflow["env"]["COVERAGE_TOOLCHAIN"], r"^nightly-\d{4}-\d{2}-\d{2}$"
        )
        self.assertRegex(workflow["env"]["CARGO_LLVM_COV_VERSION"], r"^\d+\.\d+\.\d+$")
        measure = workflow["jobs"]["measure"]
        self.assertEqual(
            measure["strategy"]["matrix"]["os"], ["ubuntu-latest", "windows-latest"]
        )
        step = _step(
            workflow, "measure", "Measure the coverage of each selection of tests"
        )
        self.assertEqual(step["env"], {"NVX_BUNDLE": "skip"})
        self.assertIn("scripts/nvx_rs_coverage.py measure", step["run"])
        self.assertIn('--toolchain "${COVERAGE_TOOLCHAIN}"', step["run"])

    def test_the_mutants_workflow_pins_cargo_mutants(self):
        workflow = _workflow("nvx-rs-mutants.yml")
        self.assertRegex(workflow["env"]["CARGO_MUTANTS_VERSION"], r"^\d+\.\d+\.\d+$")


BASH = shutil.which("bash")
# The inputs of a dispatch of the mutants workflow, as its plan step reads them.
DISPATCH = {
    "EVENT_NAME": "workflow_dispatch",
    "FILES": "src/host/a.rs  src/host/b/*.rs",
    "TESTS": "host::a:: --skip slow",
    "TARGETS": "--lib",
    "OS": "windows-latest",
    "SHARDS": "4",
}


@unittest.skipIf(
    os.name == "nt" or BASH is None, "runs the workflow's bash steps on POSIX hosts"
)
class MutantsWorkflowStepTests(unittest.TestCase):
    """Runs the steps of the mutants workflow, with a stub of Cargo."""

    def _run(
        self, job: str, step: str, environment: Mapping[str, str], directory: Path
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, str], str]:
        """Runs the step as GitHub runs a bash step, in `directory`, and returns
        its result, its outputs, and the summary that it wrote."""
        script = _step(_workflow("nvx-rs-mutants.yml"), job, step)["run"]
        outputs = directory / "github-output"
        summary = directory / "github-summary"
        outputs.write_text("", encoding="utf-8")
        summary.write_text("", encoding="utf-8")
        result = subprocess.run(
            [cast(str, BASH), "--noprofile", "--norc", "-eo", "pipefail", "-c", script],
            cwd=directory,
            env={
                **os.environ,
                **environment,
                "GITHUB_OUTPUT": str(outputs),
                "GITHUB_STEP_SUMMARY": str(summary),
            },
            capture_output=True,
            text=True,
            check=False,
        )
        values = dict(
            line.split("=", 1)
            for line in outputs.read_text(encoding="utf-8").splitlines()
        )
        return result, values, summary.read_text(encoding="utf-8")

    def _plan(
        self, environment: Mapping[str, str]
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, str]]:
        with tempfile.TemporaryDirectory() as directory:
            result, outputs, _ = self._run(
                "plan",
                "Check the inputs and plan the shards",
                environment,
                Path(directory),
            )
        return result, outputs

    def test_a_push_checks_the_workflow_on_a_small_file(self):
        result, outputs = self._plan({"EVENT_NAME": "push"})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            outputs,
            {
                "files": "src/gpt/crc.rs",
                "tests": "gpt::crc::",
                "targets": "--lib",
                "os": "ubuntu-latest",
                "shards": "2",
                "indexes": "[0,1]",
            },
        )

    def test_dispatches_plan_one_job_for_each_shard(self):
        for shards, indexes in (("1", "[0]"), ("4", "[0,1,2,3]"), ("16", None)):
            with self.subTest(shards=shards):
                result, outputs = self._plan(DISPATCH | {"SHARDS": shards})
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(
                    outputs["indexes"],
                    indexes or "[" + ",".join(map(str, range(16))) + "]",
                )
                self.assertEqual(outputs["files"], DISPATCH["FILES"])
                self.assertEqual(outputs["tests"], DISPATCH["TESTS"])
                self.assertEqual(outputs["os"], "windows-latest")
        result, outputs = self._plan(DISPATCH | {"TESTS": "", "TARGETS": ""})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((outputs["tests"], outputs["targets"]), ("", ""))

    def test_dispatches_with_invalid_inputs_fail_before_any_job(self):
        for change in (
            {"SHARDS": "0"},
            {"SHARDS": "17"},
            {"SHARDS": "04"},
            {"SHARDS": "2 "},
            {"FILES": " \t"},
            {"FILES": "src/a.rs\nsrc/b.rs"},
            {"TESTS": "a\r"},
            {"TARGETS": "--lib\n--test cli"},
            {"OS": "macos-latest"},
        ):
            with self.subTest(change=change):
                result, outputs = self._plan(DISPATCH | change)
                self.assertEqual(result.returncode, 1)
                self.assertIn("::error::", result.stdout)
                self.assertEqual(outputs, {})

    def _mutate(
        self, environment: Mapping[str, str], status: int
    ) -> tuple[subprocess.CompletedProcess[str], list[str]]:
        """Runs the cargo-mutants step in a checkout's nvx-rs directory, as the
        workflow does, with a Cargo that exits with `status`, and returns its
        result and Cargo's arguments. That Cargo exits with 9 instead if the
        parent of its output directory is missing, as cargo-mutants fails."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            crate = root / "nvx-rs"
            stubs = root / "bin"
            stubs.mkdir()
            cargo = stubs / "cargo"
            cargo.write_text(
                '#!/bin/sh\nprintf "%s\\n" "$@" > "$CARGO_ARGUMENTS"\n'
                '[ -d ../build/mutants ] || exit 9\nexit "$CARGO_STATUS"\n',
                encoding="utf-8",
            )
            cargo.chmod(0o755)
            # A file that a glob matches, which the step leaves to cargo-mutants.
            (crate / "src" / "host" / "b").mkdir(parents=True)
            (crate / "src" / "host" / "b" / "c.rs").write_text("", encoding="utf-8")
            result, _, _ = self._run(
                "mutants",
                "Run cargo-mutants",
                {
                    "PATH": f"{stubs}{os.pathsep}{os.environ['PATH']}",
                    "CARGO_ARGUMENTS": str(root / "arguments"),
                    "CARGO_STATUS": str(status),
                    "SHARD": "1/4",
                    **environment,
                },
                crate,
            )
            arguments = (root / "arguments").read_text(encoding="utf-8").splitlines()
        return result, arguments

    def test_shards_pass_the_inputs_to_cargo_mutants(self):
        # In place, since the CLI's tests need the checkout that holds the
        # crate, which a copy lacks.
        common = ["mutants", "--all-features", "--in-place", "--shard", "1/4"]
        common += ["--output", "../build/mutants"]
        environment = {key: DISPATCH[key] for key in ("FILES", "TESTS", "TARGETS")}
        result, arguments = self._mutate(environment, 0)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            arguments,
            [
                *common,
                "--file",
                "src/host/a.rs",
                "--file",
                "src/host/b/*.rs",
                "--",
                "--lib",
                "--",
                "host::a::",
                "--skip",
                "slow",
            ],
        )
        # Without test or target arguments, every test of every target runs.
        result, arguments = self._mutate(
            {"FILES": "src/a.rs", "TESTS": "", "TARGETS": ""}, 0
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(arguments, [*common, "--file", "src/a.rs", "--", "--"])

    def test_missed_mutants_and_timeouts_are_outcomes_and_failures_fail(self):
        environment = {"FILES": "src/a.rs", "TESTS": "", "TARGETS": ""}
        for status, expected in ((0, 0), (1, 1), (2, 0), (3, 0), (4, 4), (70, 70)):
            with self.subTest(status=status):
                result, _ = self._mutate(environment, status)
                self.assertEqual(result.returncode, expected)

    def _merge(
        self, shards: Mapping[str, Mapping[str, str]], count: int
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, str], str]:
        """Merges the outcome lists of `shards` of `count` jobs, and returns the
        result, the merged lists, and the summary."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for shard, lists in shards.items():
                _write(root / "shards" / shard, lists)
            result, _, summary = self._run(
                "summarize",
                "Merge the outcomes of the shards",
                {
                    "FILES": "src/a.rs",
                    "TESTS": "a::",
                    "TARGETS": "",
                    "OS": "ubuntu-latest",
                    "SHARDS": str(count),
                },
                root,
            )
            merged = {
                path.name: path.read_text(encoding="utf-8")
                for path in sorted((root / "mutants").glob("*.txt"))
            }
        return result, merged, summary

    def test_the_outcomes_of_every_shard_merge(self):
        result, merged, summary = self._merge(
            {
                "nvx-rs-mutants-shard-0-7": {
                    "caught.txt": "src/a.rs:2:1: b\nsrc/a.rs:1:1: a\n",
                    "missed.txt": "src/a.rs:9:1: m\n",
                    "timeout.txt": "",
                    "unviable.txt": "",
                },
                "nvx-rs-mutants-shard-1-7": {"caught.txt": "src/a.rs:3:1: c\n"},
            },
            2,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            merged,
            {
                "caught.txt": "src/a.rs:1:1: a\nsrc/a.rs:2:1: b\nsrc/a.rs:3:1: c\n",
                "missed.txt": "src/a.rs:9:1: m\n",
                "timeout.txt": "",
                "unviable.txt": "",
            },
        )
        self.assertIn("on ubuntu-latest: 2 of 2 shards reported.", summary)
        self.assertIn("| caught | 3 |\n| missed | 1 |\n| timeout | 0 |\n", summary)
        self.assertIn("<summary>Missed mutants</summary>", summary)
        self.assertIn("src/a.rs:9:1: m\n", summary)

    def test_shards_that_report_nothing_fail_the_merge(self):
        result, merged, summary = self._merge(
            {"nvx-rs-mutants-shard-0-7": {"caught.txt": "src/a.rs:1:1: a\n"}}, 3
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("::error::Only 1 of 3 shards reported", result.stdout)
        self.assertEqual(merged["caught.txt"], "src/a.rs:1:1: a\n")
        self.assertNotIn("Missed mutants", summary)


if __name__ == "__main__":
    unittest.main()
