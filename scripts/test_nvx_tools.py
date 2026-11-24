#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import argparse
import ast
import contextlib
import errno
import hashlib
import http.server
import io
import itertools
import json
import os
import queue
import re
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import unittest
import uuid
import zipfile
from collections.abc import Callable, Generator, Mapping, Sequence
from pathlib import Path, PurePosixPath
from typing import Any, cast
from unittest.mock import MagicMock, call, patch

import yaml

sys.path.insert(0, str(Path(__file__).parent))
import check_test_layout  # noqa: E402
import nvx  # noqa: E402
from nvx_tools import (  # noqa: E402
    archive,
    benchmark,
    build,
    build_config,
    ci,
    collect_alpine_sources,
    collect_ubuntu_sources,
    common,
    guest_artifact_reuse,
    guest_binaries,
    guests,
    merge_queue,
    microvm_durations,
    microvm_shards,
    nvx_cli,
    openvmm_ci,
    openvmm_process,
    performance,
    performance_history,
    release,
    ubuntu,
)
from nvx_tools.build_constants import (  # noqa: E402
    AlpineBuildConstants,
    AzureLinuxBuildConstants,
    BuildConstants,
    DockerBuildConstants,
    GuestRustBuildConstants,
    InitramfsBuildConstants,
    KernelBuildConstants,
    OpenVMMBuildConstants,
    ReleaseBuildConstants,
    UbuntuBuildConstants,
    ZstdBuildConstants,
)
from nvx_tools.microvm_tests import (  # noqa: E402
    MICROVM_PROCESSOR_COUNTS,
    PUBLIC_SANDBOX_SCENARIOS,
    default_suite,
)
from nvx_tools_testing import (
    composite_action_script as _composite_action_script,
)
from nvx_tools_testing import (
    composite_action_step as _composite_action_step,
)
from nvx_tools_testing import (
    expression_conjuncts as _expression_conjuncts,
)
from nvx_tools_testing import (
    outer_parentheses_enclose as _outer_parentheses_enclose,
)
from nvx_tools_testing import (
    write_release_fixture as _write_release_fixture,
)
from nvx_tools_testing import (
    yaml_scalar as _yaml_scalar,
)
from powershell_testing import run_powershell  # noqa: E402

# The directory, under a platform's runs, of the benchmarks of the microVM ABI
# series that this checkout's benchmarks record, which the benchmark actions
# name.
MICROVM_RUN_DIRECTORY = (
    f"microvm-v{OpenVMMBuildConstants.MICROVM_PERFORMANCE_ABI_VERSION}"
)


def _workflow_job(workflow: str, job_name: str) -> str:
    lines = workflow.splitlines()
    start = lines.index(f"  {job_name}:")
    end = next(
        (
            index
            for index, line in enumerate(lines[start + 1 :], start + 1)
            if len(line) > 2 and line[:2] == "  " and not line[2].isspace()
        ),
        len(lines),
    )
    return "\n".join(lines[start:end])


def _composite_action_steps(action: str) -> list[str]:
    lines = action.splitlines()
    starts = [index for index, line in enumerate(lines) if line.startswith("    - ")]
    return [
        "\n".join(lines[start:end])
        for start, end in zip(starts, starts[1:] + [len(lines)], strict=True)
    ]


_FORWARD_SIGNALS_SOURCE = 'source "${GITHUB_ACTION_PATH}/forward-signals.sh"\n'


def _harness_in_foreground(script: str) -> str:
    """Return a Linux run-benchmark step script whose run_harness runs the
    harness in the foreground, so that a test can stub the harness with a
    shell function, which the exec in forward-signals.sh would bypass."""
    if script.count(_FORWARD_SIGNALS_SOURCE) != 1:
        raise AssertionError(f"the step doesn't source forward-signals.sh:\n{script}")
    return script.replace(_FORWARD_SIGNALS_SOURCE, 'run_harness() { "$@"; }\n')


_FORWARD_SIGNALS = (
    BuildConstants.REPO_ROOT
    / ".github"
    / "actions"
    / "run-benchmark"
    / "forward-signals.sh"
)

# A harness for run_harness that handles SIGTERM as SIGINT, as nvx.py does,
# and cleans up when either one interrupts it, unless it ignores both. Its
# arguments are the files that it creates as it starts and as it cleans up,
# how long it then runs, its exit status, and "handle" or "ignore".
_CANCELLABLE_HARNESS = (
    "import pathlib, signal, sys, time\n"
    "signal.signal(signal.SIGTERM, signal.default_int_handler)\n"
    "if sys.argv[5] == 'ignore':\n"
    "    signal.signal(signal.SIGINT, signal.SIG_IGN)\n"
    "    signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
    "try:\n"
    "    pathlib.Path(sys.argv[1]).touch()\n"
    "    time.sleep(float(sys.argv[3]))\n"
    "except KeyboardInterrupt:\n"
    "    pathlib.Path(sys.argv[2]).touch()\n"
    "    sys.exit(130)\n"
    "sys.exit(int(sys.argv[4]))\n"
)


def _workflow_step(workflow: str, job_name: str, step_name: str) -> str:
    lines = _workflow_job(workflow, job_name).splitlines()
    start = lines.index(f"      - name: {step_name}")
    end = next(
        (
            index
            for index, line in enumerate(lines[start + 1 :], start + 1)
            if line.startswith("      - ")
        ),
        len(lines),
    )
    return "\n".join(lines[start:end])


def _workflow_steps(workflow: str, job_name: str) -> list[str]:
    lines = _workflow_job(workflow, job_name).splitlines()
    starts = [index for index, line in enumerate(lines) if line.startswith("      - ")]
    return [
        "\n".join(lines[start:end])
        for start, end in zip(starts, starts[1:] + [len(lines)], strict=True)
    ]


def _yaml_field(configuration: str, field: str) -> list[str]:
    """Return the first value of a field, with one entry per literal block line."""
    lines = configuration.splitlines()
    for index, line in enumerate(lines):
        stripped = line.lstrip()
        if not stripped.startswith(f"{field}: "):
            continue
        value = stripped[len(field) + 2 :].rstrip()
        if value != "|":
            return [value]
        indent = len(line) - len(stripped)
        values: list[str] = []
        for block_line in lines[index + 1 :]:
            block_value = block_line.strip()
            if block_value and len(block_line) - len(block_line.lstrip()) <= indent:
                break
            if block_value:
                values.append(block_value)
        return values
    raise AssertionError(f"configuration has no {field} field")


def _yaml_uses(node: yaml.Node | None) -> list[yaml.Node]:
    """Return the value node of every `uses` key in a composed workflow or
    action."""
    if isinstance(node, yaml.MappingNode):
        uses: list[yaml.Node] = []
        for key, child in cast(list[tuple[yaml.Node, yaml.Node]], node.value):
            if isinstance(key, yaml.ScalarNode) and key.value == "uses":
                uses.append(child)
            else:
                uses.extend(_yaml_uses(child))
        return uses
    if isinstance(node, yaml.SequenceNode):
        children = cast(list[yaml.Node], node.value)
        return [use for child in children for use in _yaml_uses(child)]
    return []


def _action_references() -> list[tuple[str, str, str]]:
    """Return the location, reference, and trailing comment of every `uses`
    value of the workflows and actions under .github/. YAML drops comments,
    so the comment is the rest of the source line after the value."""
    root = BuildConstants.REPO_ROOT
    references: list[tuple[str, str, str]] = []
    for path in sorted(
        path
        for pattern in ("*.yml", "*.yaml")
        for path in (root / ".github").rglob(pattern)
    ):
        text = path.read_text(encoding="utf-8")
        lines = text.splitlines()
        for node in _yaml_uses(yaml.SafeLoader(text).get_single_node()):
            location = f"{path.relative_to(root).as_posix()}:{node.start_mark.line + 1}"
            if not isinstance(node, yaml.ScalarNode):
                raise AssertionError(f"{location}: uses: is not a scalar")
            end = node.end_mark
            references.append(
                (location, str(node.value), lines[end.line][end.column :])
            )
    return references


_EXPRESSION_TOKEN = re.compile(
    r"\s*(?:(?P<number>[0-9]+(?:\.[0-9]+)?)|(?P<string>'(?:[^']|'')*')|"
    r"(?P<operator>==|!=|<=|>=|&&|\|\||[!<>().,\[\]])|"
    r"(?P<name>[A-Za-z_][A-Za-z0-9_-]*))"
)


def _expression_truthy(value: Any) -> bool:
    """Return whether GitHub treats a value as true: all but false, 0, NaN,
    the empty string, and null."""
    if value is None or value is False or value == "":
        return False
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return value == value and value != 0
    return True


def _expression_number(value: Any) -> float:
    if value is None:
        return 0.0
    if isinstance(value, (bool, int, float)):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value) if value.strip() else 0.0
        except ValueError:
            return float("nan")
    return float("nan")


def _expressions_equal(left: Any, right: Any) -> bool:
    """Compare as GitHub's `==` does: strings ignoring case, values of one
    type directly, and values of different types as numbers."""
    if isinstance(left, str) and isinstance(right, str):
        return left.casefold() == right.casefold()
    if type(left) is type(right):
        return left == right
    return _expression_number(left) == _expression_number(right)


def _evaluate_expression(
    text: str,
    context: Mapping[str, Any],
    functions: Mapping[str, Callable[..., Any]],
) -> Any:
    """Evaluate a GitHub Actions expression, as far as the CI workflow's job
    conditions and settings use one: literals, contexts and their properties,
    `!`, `==` and `!=`, `&&` and `||`, which return an operand as GitHub's do,
    parentheses, and calls of `functions`."""
    expression = " ".join(text.split())
    if expression.startswith("${{") and expression.endswith("}}"):
        expression = expression[3:-2].strip()
    tokens: list[tuple[str, str]] = []
    position = 0
    while position < len(expression):
        match = _EXPRESSION_TOKEN.match(expression, position)
        if match is None or match.lastgroup is None:
            raise AssertionError(f"cannot read {expression[position:]!r}")
        tokens.append((match.lastgroup, match.group(match.lastgroup)))
        position = match.end()
    index = 0

    def peek() -> str | None:
        return tokens[index][1] if index < len(tokens) else None

    def take(expected: str | None = None) -> tuple[str, str]:
        nonlocal index
        if index == len(tokens):
            raise AssertionError(f"{expression!r} ends early")
        token = tokens[index]
        if expected is not None and token[1] != expected:
            raise AssertionError(f"expected {expected!r} in {expression!r}")
        index += 1
        return token

    def primary() -> Any:
        kind, value = take()
        if value == "(":
            result = disjunction()
            take(")")
            return result
        if kind == "number":
            return float(value)
        if kind == "string":
            return value[1:-1].replace("''", "'")
        if kind != "name":
            raise AssertionError(f"unexpected {value!r} in {expression!r}")
        if value in ("true", "false"):
            return value == "true"
        if value == "null":
            return None
        if peek() == "(":
            take("(")
            arguments: list[Any] = []
            while peek() != ")":
                if arguments:
                    take(",")
                arguments.append(disjunction())
            take(")")
            return functions[value](*arguments)
        result: Any = context.get(value)
        while peek() in (".", "["):
            if take()[1] == ".":
                key: Any = take()[1]
            else:
                key = disjunction()
                take("]")
            result = (
                cast(Mapping[Any, Any], result).get(key)
                if isinstance(result, Mapping)
                else None
            )
        return result

    def unary() -> Any:
        if peek() == "!":
            take("!")
            return not _expression_truthy(unary())
        return primary()

    def comparison() -> Any:
        left = unary()
        if peek() in ("==", "!="):
            operator = take()[1]
            equal = _expressions_equal(left, unary())
            return equal if operator == "==" else not equal
        return left

    def conjunction() -> Any:
        result = comparison()
        while peek() == "&&":
            take("&&")
            right = comparison()
            result = right if _expression_truthy(result) else result
        return result

    def disjunction() -> Any:
        result = conjunction()
        while peek() == "||":
            take("||")
            right = conjunction()
            result = result if _expression_truthy(result) else right
        return result

    result = disjunction()
    if index != len(tokens):
        raise AssertionError(f"unexpected {tokens[index][1]!r} in {expression!r}")
    return result


def _expression_string(value: Any) -> str:
    """Return a value as GitHub writes it into a string."""
    if value is None:
        return ""
    if isinstance(value, bool):
        return str(value).lower()
    if isinstance(value, float) and value.is_integer():
        return str(int(value))
    return str(value)


def _interpolate_expressions(
    text: str,
    context: Mapping[str, Any],
    functions: Mapping[str, Callable[..., Any]],
) -> Any:
    """Return the value of a workflow setting: that of its expression when the
    setting is one, and otherwise its text with each expression replaced by
    its value."""
    pieces = re.split(r"\$\{\{(.*?)\}\}", text.strip(), flags=re.DOTALL)
    if len(pieces) == 3 and not pieces[0] and not pieces[2]:
        return _evaluate_expression(pieces[1], context, functions)
    return "".join(
        _expression_string(_evaluate_expression(piece, context, functions))
        if index % 2
        else piece
        for index, piece in enumerate(pieces)
    )


def _job_status_functions(succeeded: bool) -> dict[str, Callable[[], Any]]:
    """Return the status functions of a job's condition in a run that nobody
    cancels and whose jobs never fail, once the jobs that it needs ended."""
    return {
        "always": lambda: True,
        "cancelled": lambda: False,
        "failure": lambda: False,
        "success": lambda: succeeded,
    }


def _simulate_workflow_jobs(
    workflow: Mapping[str, Any],
    github: Mapping[str, Any],
    outputs: Mapping[str, Mapping[str, str]],
) -> dict[str, str]:
    """Return the result of each job of a workflow run whose jobs all succeed
    when they run: `success` for each job whose condition holds once the jobs
    that it needs have their results, and `skipped` for the others. `outputs`
    gives the outputs of jobs by name."""
    jobs: Mapping[str, Mapping[str, Any]] = workflow["jobs"]

    def needs(name: str) -> list[str]:
        value = jobs[name].get("needs", [])
        return [value] if isinstance(value, str) else list(value)

    results: dict[str, str] = {}
    pending = list(jobs)
    while pending:
        name = next(
            (name for name in pending if all(need in results for need in needs(name))),
            None,
        )
        if name is None:
            raise AssertionError(f"these jobs wait for one another: {pending}")
        pending.remove(name)
        condition = " ".join(str(jobs[name].get("if", "success()")).split())
        if condition.startswith("${{") and condition.endswith("}}"):
            condition = condition[3:-2].strip()
        # A condition without a status function also needs every job that it
        # needs to succeed.
        if not re.search(r"\b(?:always|success|failure|cancelled)\(\)", condition):
            condition = f"success() && ({condition})"
        none: dict[str, str] = {}
        needed: dict[str, dict[str, Any]] = {
            need: {
                "result": results[need],
                "outputs": (
                    outputs.get(need, none) if results[need] == "success" else none
                ),
            }
            for need in needs(name)
        }
        context: dict[str, Any] = {"github": github, "needs": needed}
        functions = _job_status_functions(
            all(results[need] == "success" for need in needs(name))
        )
        runs = _expression_truthy(_evaluate_expression(condition, context, functions))
        results[name] = "success" if runs else "skipped"
    return results


def _matrix_legs(job: Mapping[str, Any]) -> list[dict[str, Any]]:
    """Return the inputs that each leg of a job that calls a reusable workflow
    passes, with the expressions of its matrix and strategy resolved; a job
    without a matrix has one leg."""
    strategy: Mapping[str, Any] = job.get("strategy", {})
    matrix: dict[str, list[Any]] = dict(strategy.get("matrix", {}))
    if {"include", "exclude"} & set(matrix):
        raise AssertionError(f"unsupported matrix: {matrix}")
    combinations = [
        dict(zip(matrix, values, strict=True))
        for values in itertools.product(*matrix.values())
    ]
    legs: list[dict[str, Any]] = []
    for combination in combinations:
        expressions: dict[str, Any] = {
            f"${{{{ matrix.{name} }}}}": value for name, value in combination.items()
        }
        expressions["${{ strategy.job-total }}"] = len(combinations)

        def resolve(value: Any, expressions: dict[str, Any] = expressions) -> Any:
            if not isinstance(value, str):
                return value
            if value in expressions:
                return expressions[value]
            for expression, replacement in expressions.items():
                value = value.replace(expression, str(replacement))
            return value

        legs.append(
            {name: resolve(value) for name, value in job.get("with", {}).items()}
        )
    return legs


def _resolve_inputs(text: str, inputs: Mapping[str, Any]) -> str:
    """Resolve the input expressions of a reusable workflow's step: an input,
    `inputs.NAME && 'VALUE' || 'OTHER'`, and `inputs.NAME && format(...)
    || 'OTHER'` with inputs as the arguments."""

    def value(name: str) -> Any:
        return inputs[name.removeprefix("inputs.")]

    def formatted(match: re.Match[str]) -> str:
        if not value(match.group(1)):
            return match.group(4)
        arguments = [str(value(name.strip())) for name in match.group(3).split(",")]
        return match.group(2).format(*arguments)

    text = re.sub(
        r"\$\{\{ (inputs\.[a-z-]+) && format\('([^']*)', ([^)]*)\) "
        r"\|\| '([^']*)' \}\}",
        formatted,
        text,
    )
    text = re.sub(
        r"\$\{\{ (inputs\.[a-z-]+) && '([^']*)' \|\| '([^']*)' \}\}",
        lambda match: match.group(2 if value(match.group(1)) else 3),
        text,
    )
    return re.sub(
        r"\$\{\{ (inputs\.[a-z-]+) \}\}", lambda match: str(value(match.group(1))), text
    )


def _nvx_command_modules() -> dict[str, set[str]]:
    """Map each nvx.py command to the scripts/nvx_tools modules that its parser
    configuration and handler, and the nvx.py functions that they call, name."""
    tree = ast.parse(
        (BuildConstants.REPO_ROOT / "scripts" / "nvx.py").read_text(encoding="utf-8")
    )
    imported: dict[str, str] = {}
    functions: dict[str, ast.FunctionDef] = {}
    for node in tree.body:
        if isinstance(node, ast.ImportFrom) and node.module is not None:
            package, _, module = node.module.partition(".")
            if package == "nvx_tools":
                for alias in node.names:
                    imported[alias.asname or alias.name] = module or alias.name
        elif isinstance(node, ast.FunctionDef):
            functions[node.name] = node
    parse_args = functions["parse_args"]
    functions.update(
        (node.name, node)
        for node in ast.walk(parse_args)
        if isinstance(node, ast.FunctionDef) and node is not parse_args
    )

    def names(node: ast.AST) -> set[str]:
        return {child.id for child in ast.walk(node) if isinstance(child, ast.Name)}

    def modules(node: ast.AST, seen: set[str]) -> set[str]:
        found: set[str] = set()
        for name in names(node):
            if name in imported:
                found.add(imported[name])
            elif name in functions and name not in seen:
                seen.add(name)
                found |= modules(functions[name], seen)
        return found

    parsers: dict[str, str] = {}
    commands: dict[str, set[str]] = {}
    for statement in parse_args.body:
        if (
            isinstance(statement, ast.Assign)
            and isinstance(statement.value, ast.Call)
            and isinstance(statement.value.func, ast.Attribute)
            and statement.value.func.attr == "add_parser"
            and isinstance(statement.value.func.value, ast.Name)
            and statement.value.func.value.id == "subparsers"
        ):
            [target] = statement.targets
            command = statement.value.args[0]
            assert isinstance(target, ast.Name)
            assert isinstance(command, ast.Constant)
            assert isinstance(command.value, str)
            parsers[target.id] = command.value
            commands[command.value] = set()
        for parser in names(statement) & parsers.keys():
            commands[parsers[parser]] |= modules(statement, set())
    return commands


# The CI steps that validate each shell-snapshot restore attempt and remeasure
# an unstable one once: their suite, processor counts, and scenarios of
# validation statuses and benchmark exit status.
RESTORE_REMEASUREMENT_STEPS = (
    (
        "performance suite",
        "performance",
        (1,),
        (
            ("first-success", (0,), 0),
            ("remeasured-success", (75, 0), 0),
            ("both-unstable", (75, 75), 0),
            ("invalid-data", (2,), 0),
            ("unstable-then-invalid", (75, 2), 0),
            ("benchmark-failure", (), 42),
        ),
    ),
    (
        "multi-vCPU shell restore",
        "shell-snapshot-restore",
        (2, 4, 8),
        (
            ("first-success", (0, 0, 0), 0),
            ("remeasured-success", (0, 75, 0, 0), 0),
            ("both-unstable", (0, 75, 75), 0),
            ("invalid-data", (0, 2), 0),
            ("unstable-then-invalid", (75, 2), 0),
            ("benchmark-failure", (), 42),
        ),
    ),
)


def _expected_restore_remeasurement(
    platform: str,
    suite: str,
    processor_counts: Sequence[int],
    statuses: Sequence[int],
    benchmark_status: int,
) -> tuple[int, list[str], dict[tuple[int, int], int], dict[int, int]]:
    """Return the exit status of a restore remeasurement step, the lines that
    its stubbed commands print, the benchmark number of each measured
    (processors, attempt), and the benchmark number that each processor count
    publishes."""
    lines: list[str] = []
    measured: dict[tuple[int, int], int] = {}
    published: dict[int, int] = {}
    validations = iter(statuses)
    for processors in processor_counts:
        run_dir = f"data/runs/{platform}/{MICROVM_RUN_DIRECTORY}/{processors}vcpu"
        for attempt in (1, 2):
            attempt_dir = f"{run_dir}/{suite}-attempt-{attempt}"
            measured[processors, attempt] = len(measured) + 1
            lines.append(f"benchmark {suite} {processors} {attempt_dir}")
            if benchmark_status:
                return benchmark_status, lines, measured, published
            lines.append(f"validate {platform} {attempt_dir}")
            status = next(validations)
            if status == 0:
                published[processors] = measured[processors, attempt]
                break
            if status != 75 or attempt == 2:
                return status, lines, measured, published
            vcpus = "" if suite == "performance" else f" with {processors} vCPUs"
            lines.append(
                f"::warning::Shell-snapshot restore{vcpus} was unstable on attempt "
                f"{attempt}; remeasuring once"
            )
    return 0, lines, measured, published


class CiTests(unittest.TestCase):
    def test_openvmm_unit_tests_run_nextest_and_doctests(self):
        with tempfile.TemporaryDirectory() as temporary:
            openvmm = Path(temporary) / "openvmm"
            openvmm.mkdir()
            (openvmm / "Cargo.toml").touch()
            fuzz_crates = common.CommandResult(
                args=("cargo", "xtask", "fuzz", "list", "--crates"),
                returncode=0,
                stdout=b"fuzz_alpha\nfuzz_beta\n",
                stderr=b"",
            )

            with (
                patch.object(OpenVMMBuildConstants, "DIRECTORY", openvmm),
                patch.object(ci, "require_tool", return_value="cargo"),
                patch.object(ci, "run_capture", return_value=fuzz_crates) as capture,
                patch.object(ci, "run_checked") as run_checked,
                patch.dict(os.environ, {"NVX_TEST_VARIABLE": "1"}, clear=True),
            ):
                ci.run_openvmm_unit_tests()

            environment = capture.call_args.kwargs["env"]
            expected_environment = {"NVX_TEST_VARIABLE": "1"}
            if os.name == "nt":
                expected_environment["_LINK_"] = ci.MSVC_REPRODUCIBLE_LINK_OPTION
            self.assertEqual(environment, expected_environment)
            capture.assert_called_once_with(
                ["cargo", "xtask", "fuzz", "list", "--crates"],
                cwd=openvmm,
                env=environment,
            )
            self.assertEqual(run_checked.call_count, 3)
            for command in run_checked.call_args_list:
                self.assertIs(command.kwargs["env"], environment)
            self.assertEqual(
                run_checked.call_args_list[0],
                call(
                    [
                        "cargo",
                        "xflowey",
                        "restore-packages",
                        "--no-compat-igvm",
                    ],
                    cwd=openvmm,
                    env=environment,
                ),
            )
            command = run_checked.call_args_list[1].args[0]
            self.assertEqual(
                command[:10],
                [
                    "cargo",
                    "nextest",
                    "run",
                    "--profile",
                    "agent",
                    "--workspace",
                    "--tests",
                    "--bins",
                    "--features",
                    "ci",
                ],
            )
            excluded_packages = [
                command[index + 1]
                for index, argument in enumerate(command)
                if argument == "--exclude"
            ]
            self.assertEqual(
                excluded_packages,
                [
                    *ci.OPENVMM_UNIT_TEST_EXCLUDED_PACKAGES,
                    "fuzz_alpha",
                    "fuzz_beta",
                ],
            )
            self.assertEqual(run_checked.call_args_list[1].kwargs["cwd"], openvmm)
            self.assertEqual(
                run_checked.call_args_list[2],
                call(
                    [
                        "cargo",
                        "test",
                        "--locked",
                        "--doc",
                        "--workspace",
                        "--no-fail-fast",
                    ],
                    cwd=openvmm,
                    env=environment,
                ),
            )

    def test_openvmm_windows_builds_link_reproducibly(self):
        for options, expected in (
            (None, "/Brepro"),
            ("", "/Brepro"),
            ("/DEBUG:FULL", "/DEBUG:FULL /Brepro"),
            ("/brepro", "/brepro"),
            ("-BREPRO /DEBUG:FULL", "-BREPRO /DEBUG:FULL"),
        ):
            with self.subTest(options=options):
                environment = {} if options is None else {"_LINK_": options}
                ci._link_reproducibly_with_msvc(environment)
                self.assertEqual(environment, {"_LINK_": expected})

    def test_openvmm_unit_tests_stop_when_fuzz_crate_query_fails(self):
        with tempfile.TemporaryDirectory() as temporary:
            openvmm = Path(temporary) / "openvmm"
            openvmm.mkdir()
            (openvmm / "Cargo.toml").touch()
            failed_query = common.CommandResult(
                args=("cargo", "xtask", "fuzz", "list", "--crates"),
                returncode=2,
                stdout=b"",
                stderr=b"query failed",
            )

            with (
                patch.object(OpenVMMBuildConstants, "DIRECTORY", openvmm),
                patch.object(ci, "require_tool", return_value="cargo"),
                patch.object(ci, "run_capture", return_value=failed_query),
                patch.object(ci, "run_checked") as run_checked,
                self.assertRaisesRegex(
                    common.ScriptError,
                    "OpenVMM fuzz crate query exited 2",
                ),
            ):
                ci.run_openvmm_unit_tests()

            run_checked.assert_not_called()

    def test_openvmm_tests_use_nvx_guest_artifacts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            openvmm = root / "openvmm"
            openvmm.mkdir()
            (openvmm / "Cargo.toml").touch()
            kernel = root / "vmlinux"
            initrd = root / "initramfs.cpio.gz"
            kernel.touch()
            initrd.touch()
            backend = "whp" if os.name == "nt" else "kvm"
            installed_targets = common.CommandResult(
                args=("rustup", "target", "list"),
                returncode=0,
                stdout=(
                    "\n".join(OpenVMMBuildConstants.TEST_RUST_TARGETS[backend]) + "\n"
                ).encode(),
                stderr=b"",
            )

            with (
                patch.object(OpenVMMBuildConstants, "DIRECTORY", openvmm),
                patch.object(
                    ci,
                    "artifact_path",
                    side_effect=[kernel, initrd],
                ),
                patch.object(ci.os, "access", return_value=True),
                patch.object(ci.Path, "exists", return_value=False),
                patch.object(ci, "require_tool", side_effect=["cargo", "rustup"]),
                patch.object(
                    ci,
                    "run_capture",
                    return_value=installed_targets,
                ) as run_capture,
                patch.object(ci, "run_checked") as run_checked,
                patch.dict(
                    os.environ,
                    {
                        "PETRI_CAPABILITIES": "vpci",
                        "RUNNER_TEMP": os.fspath(root),
                        "_LINK_": "",
                    },
                ),
            ):
                ci.run_openvmm_tests(backend)

            run_capture.assert_called_once_with(
                [
                    "rustup",
                    "target",
                    "list",
                    "--installed",
                    "--toolchain",
                    OpenVMMBuildConstants.RUST_TOOLCHAIN,
                ]
            )
            self.assertEqual(run_checked.call_count, 2)
            restore, tests = run_checked.call_args_list
            self.assertEqual(
                restore.args[0],
                ["cargo", "xflowey", "restore-packages", "--no-compat-igvm"],
            )
            environment = restore.kwargs["env"]
            self.assertEqual(
                environment["RUSTUP_TOOLCHAIN"],
                OpenVMMBuildConstants.RUST_TOOLCHAIN,
            )
            if os.name == "nt":
                self.assertNotIn("XDG_CACHE_HOME", environment)
            else:
                self.assertEqual(
                    environment["XDG_CACHE_HOME"],
                    os.fspath(root / "openvmm-cache"),
                )
            command = tests.args[0]
            self.assertEqual(command[:3], ["cargo", "xflowey", "vmm-tests-run"])
            filter_index = command.index("--filter")
            self.assertEqual(
                command[filter_index + 1],
                ci.OPENVMM_TEST_FILTERS[backend],
            )
            self.assertEqual(tests.kwargs["cwd"], openvmm)
            self.assertIs(tests.kwargs["env"], environment)
            self.assertEqual(
                environment["OPENVMM_MICROVM_TEST_KERNEL"],
                os.fspath(kernel.resolve()),
            )
            self.assertEqual(
                environment["OPENVMM_MICROVM_TEST_INITRD"],
                os.fspath(initrd.resolve()),
            )
            self.assertEqual(environment["PETRI_CAPABILITIES"], "vpci")
            self.assertEqual(
                environment["_LINK_"],
                ci.MSVC_REPRODUCIBLE_LINK_OPTION if os.name == "nt" else "",
            )
            if os.name == "nt":
                self.assertEqual(
                    command[command.index("--dir") + 1],
                    os.fspath(root / backend),
                )

    def test_openvmm_tests_pin_toolchain_without_runner_temp(self):
        installed_targets = common.CommandResult(
            args=("rustup", "target", "list"),
            returncode=0,
            stdout=(
                "\n".join(OpenVMMBuildConstants.TEST_RUST_TARGETS["whp"]) + "\n"
            ).encode(),
            stderr=b"",
        )

        with (
            patch.object(ci, "run_capture", return_value=installed_targets),
            patch.object(ci, "run_checked") as run_checked,
            patch.dict(os.environ, {}, clear=True),
        ):
            environment = ci._prepare_openvmm_test_environment(("whp",), "rustup")

        expected_environment = {
            "RUSTUP_TOOLCHAIN": OpenVMMBuildConstants.RUST_TOOLCHAIN,
        }
        if os.name == "nt":
            expected_environment["_LINK_"] = ci.MSVC_REPRODUCIBLE_LINK_OPTION
        self.assertEqual(environment, expected_environment)
        run_checked.assert_not_called()

    def test_openvmm_tests_prepare_job_local_toolchain_for_missing_targets(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            openvmm = root / "openvmm"
            openvmm.mkdir()
            (openvmm / "Cargo.toml").touch()
            kernel = root / "vmlinux"
            initrd = root / "initramfs.cpio.gz"
            kernel.touch()
            initrd.touch()
            backend = "whp" if os.name == "nt" else "kvm"
            installed_targets = common.CommandResult(
                args=("rustup", "target", "list"),
                returncode=0,
                stdout=f"{OpenVMMBuildConstants.GUEST_RUST_TARGET}\n".encode(),
                stderr=b"",
            )

            with (
                patch.object(OpenVMMBuildConstants, "DIRECTORY", openvmm),
                patch.object(
                    ci,
                    "artifact_path",
                    side_effect=[kernel, initrd],
                ),
                patch.object(ci.os, "access", return_value=True),
                patch.object(ci.Path, "exists", return_value=False),
                patch.object(ci, "require_tool", side_effect=["cargo", "rustup"]),
                patch.object(ci, "run_capture", return_value=installed_targets),
                patch.object(ci, "run_checked") as run_checked,
                patch.dict(os.environ, {"RUNNER_TEMP": os.fspath(root)}),
            ):
                ci.run_openvmm_tests(backend)

            self.assertEqual(run_checked.call_count, 4)
            install_toolchain, install_targets, restore, tests = (
                run_checked.call_args_list
            )
            environment = install_toolchain.kwargs["env"]
            self.assertEqual(
                environment["RUSTUP_HOME"],
                os.fspath(root / "openvmm-rustup"),
            )
            self.assertEqual(
                environment["RUSTUP_TOOLCHAIN"],
                OpenVMMBuildConstants.RUST_TOOLCHAIN,
            )
            if os.name != "nt":
                self.assertEqual(
                    environment["XDG_CACHE_HOME"],
                    os.fspath(root / "openvmm-cache"),
                )
            self.assertEqual(
                install_toolchain.args[0],
                [
                    "rustup",
                    "toolchain",
                    "install",
                    OpenVMMBuildConstants.RUST_TOOLCHAIN,
                    "--profile",
                    "minimal",
                ],
            )
            self.assertEqual(
                install_targets.args[0],
                [
                    "rustup",
                    "target",
                    "add",
                    *OpenVMMBuildConstants.TEST_RUST_TARGETS[backend],
                    "--toolchain",
                    OpenVMMBuildConstants.RUST_TOOLCHAIN,
                ],
            )
            for command in (install_targets, restore, tests):
                self.assertIs(command.kwargs["env"], environment)

    def test_openvmm_tests_install_missing_pinned_toolchain(self):
        missing_toolchain = common.CommandResult(
            args=("rustup", "target", "list"),
            returncode=1,
            stdout=b"",
            stderr=(
                f"error: toolchain '{OpenVMMBuildConstants.RUST_TOOLCHAIN}' "
                "is not installed\n"
            ).encode(),
        )
        install_toolchain = [
            "rustup",
            "toolchain",
            "install",
            OpenVMMBuildConstants.RUST_TOOLCHAIN,
            "--profile",
            "minimal",
        ]
        add_targets = [
            "rustup",
            "target",
            "add",
            *OpenVMMBuildConstants.TEST_RUST_TARGETS["kvm"],
            "--toolchain",
            OpenVMMBuildConstants.RUST_TOOLCHAIN,
        ]

        with tempfile.TemporaryDirectory() as temporary:
            for runner_temp in (None, temporary):
                with (
                    self.subTest(runner_temp=runner_temp),
                    patch.object(ci, "run_capture", return_value=missing_toolchain),
                    patch.object(ci, "run_checked") as run_checked,
                    patch.dict(
                        os.environ,
                        {} if runner_temp is None else {"RUNNER_TEMP": runner_temp},
                        clear=True,
                    ),
                ):
                    environment = ci._prepare_openvmm_test_environment(
                        ("kvm",), "rustup"
                    )

                    self.assertEqual(
                        [command.args[0] for command in run_checked.call_args_list],
                        [install_toolchain, add_targets],
                    )
                    for command in run_checked.call_args_list:
                        self.assertIs(command.kwargs["env"], environment)
                    self.assertEqual(
                        environment["RUSTUP_TOOLCHAIN"],
                        OpenVMMBuildConstants.RUST_TOOLCHAIN,
                    )
                    self.assertEqual("_LINK_" in environment, os.name == "nt")
                    if runner_temp is None:
                        self.assertNotIn("RUSTUP_HOME", environment)
                    else:
                        self.assertEqual(
                            environment["RUSTUP_HOME"],
                            os.fspath(
                                Path(runner_temp)
                                / OpenVMMBuildConstants.RUSTUP_DIRECTORY_NAME
                            ),
                        )

    def test_openvmm_tests_define_each_backend_filter(self):
        self.assertEqual(
            set(ci.OPENVMM_TEST_FILTERS),
            set(ci.OPENVMM_TEST_BACKENDS),
        )
        self.assertEqual(
            set(OpenVMMBuildConstants.TEST_RUST_TARGETS),
            set(ci.OPENVMM_TEST_BACKENDS),
        )
        self.assertEqual(
            ci.OPENVMM_TEST_FILTERS["kvm"],
            ci.OPENVMM_KVM_TEST_FILTER,
        )
        self.assertIn(
            "!test(no_vmbus_prepped_boot_no_vmbus_windows)",
            ci.OPENVMM_KVM_TEST_FILTER,
        )
        self.assertIn(
            "!test(windows_datacenter_core)",
            ci.OPENVMM_KVM_TEST_FILTER,
        )
        self.assertIn("!test(virtio_net_windows)", ci.OPENVMM_KVM_TEST_FILTER)
        self.assertEqual(
            set(ci.OPENVMM_KVM_EXCLUDED_TESTS),
            {
                "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_boot_no_agent",
                "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_iso_boot_no_agent",
                "multiarch::openvmm_pcat_x64_ubuntu_2404_server_x64_boot",
                "multiarch::openvmm_pcat_x64_ubuntu_2504_server_x64_boot",
                "multiarch::openvmm_pcat_x64_ubuntu_2504_server_x64_boot_heavy",
            },
        )
        for excluded_test in ci.OPENVMM_KVM_EXCLUDED_TESTS:
            self.assertIn(
                f"!{ci._exact_openvmm_test(excluded_test)}",
                ci.OPENVMM_KVM_TEST_FILTER,
            )
        self.assertNotIn("!test(openvmm_pcat_x64)", ci.OPENVMM_KVM_TEST_FILTER)
        self.assertNotIn(
            "openvmm_linux_x64_apicid_offset",
            ci.OPENVMM_KVM_TEST_FILTER,
        )
        self.assertNotIn(
            "openvmm_linux_x64_legacy_xapic",
            ci.OPENVMM_KVM_TEST_FILTER,
        )
        self.assertEqual(
            ci.OPENVMM_TEST_FILTERS["mshv"],
            ci.OPENVMM_MSHV_TEST_FILTER,
        )
        self.assertIn(
            "!test(windows_datacenter_core)",
            ci.OPENVMM_MSHV_TEST_FILTER,
        )
        self.assertEqual(
            set(ci.OPENVMM_MSHV_EXCLUDED_TESTS),
            {
                "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_boot_no_agent",
                "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_iso_boot_no_agent",
                "multiarch::openvmm_pcat_x64_ubuntu_2404_server_x64_boot",
                "multiarch::openvmm_pcat_x64_ubuntu_2504_server_x64_boot",
                "multiarch::openvmm_pcat_x64_ubuntu_2504_server_x64_boot_heavy",
                "multiarch::pcie::openvmm_linux_x64_pcie_save_restore",
            },
        )
        self.assertNotIn(
            "x86_64::openvmm_linux_x64_virtio_blk_device",
            ci.OPENVMM_MSHV_EXCLUDED_TESTS,
        )
        for excluded_test in ci.OPENVMM_MSHV_EXCLUDED_TESTS:
            self.assertIn(
                f"!{ci._exact_openvmm_test(excluded_test)}",
                ci.OPENVMM_MSHV_TEST_FILTER,
            )
        self.assertNotIn("!test(openvmm_pcat_x64)", ci.OPENVMM_MSHV_TEST_FILTER)
        for backend, test_filter in ci.OPENVMM_TEST_FILTERS.items():
            for required_test in ci.OPENVMM_REQUIRED_MICROVM_TESTS:
                with self.subTest(backend=backend, required_test=required_test):
                    self.assertIn(
                        ci._exact_openvmm_test(required_test),
                        test_filter,
                    )
        self.assertEqual(len(ci.OPENVMM_WHP_TESTS), 30)
        self.assertEqual(
            len(set(ci.OPENVMM_WHP_TESTS)),
            len(ci.OPENVMM_WHP_TESTS),
        )
        for required_test in ci.OPENVMM_REQUIRED_MICROVM_TESTS:
            self.assertIn(required_test, ci.OPENVMM_WHP_TESTS)
        # vmm-tests-run checks for Hyper-V when a selected test needs it, as
        # flowey reads from "hyperv" in a test's name, so no WHP test may need
        # Hyper-V, which WHP hosts need not run (#690).
        for test in ci.OPENVMM_WHP_TESTS:
            with self.subTest(whp_test=test):
                self.assertNotIn("hyperv", test)
        # Only Linux hosts can build the Linux pipette this TTRPC test boots.
        self.assertNotIn("test_ttrpc_interface", ci.OPENVMM_TEST_FILTERS["whp"])
        for backend in ("kvm", "mshv"):
            with self.subTest(backend=backend):
                self.assertIn("test(ttrpc)", ci.OPENVMM_TEST_FILTERS[backend])
                self.assertNotIn(
                    "test_ttrpc_interface",
                    ci.OPENVMM_TEST_FILTERS[backend],
                )
                self.assertIn("!test(vm_host)", ci.OPENVMM_TEST_FILTERS[backend])
        self.assertNotIn("vm_host", ci.OPENVMM_TEST_FILTERS["whp"])
        self.assertEqual(ci.OPENVMM_WHP_EXCLUDED_TESTS, ())
        self.assertNotIn(
            "multiarch::openvmm_pcat_x64_windows_datacenter_core_2022_x64_boot_heavy",
            ci.OPENVMM_WHP_EXCLUDED_TESTS,
        )
        self.assertNotIn(
            "multiarch::openvmm_pcat_x64_windows_datacenter_core_2022_x64_boot",
            ci.OPENVMM_WHP_EXCLUDED_TESTS,
        )
        self.assertNotIn(
            "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_iso_boot_no_agent",
            ci.OPENVMM_WHP_EXCLUDED_TESTS,
        )
        self.assertNotIn(
            "multiarch::openvmm_pcat_x64_freebsd_13_2_x64_boot_no_agent",
            ci.OPENVMM_WHP_EXCLUDED_TESTS,
        )
        self.assertNotIn(
            "multiarch::pcie::openvmm_uefi_x64_windows_datacenter_core_2022_x64_pcie_nvme_boot",
            ci.OPENVMM_WHP_EXCLUDED_TESTS,
        )
        self.assertTrue(
            set(ci.OPENVMM_WHP_EXCLUDED_TESTS).issubset(ci.OPENVMM_WHP_TESTS)
        )
        self.assertEqual(
            ci.OPENVMM_TEST_FILTERS["whp"],
            ci._join_openvmm_tests(
                ci.OPENVMM_WHP_TESTS,
                ci.OPENVMM_WHP_EXCLUDED_TESTS,
            ),
        )
        self.assertEqual(
            ci._join_openvmm_tests(
                ("suite::boot", "suite::boot_heavy"),
                ("suite::boot_heavy",),
            ),
            (
                "(test(/^suite::boot$/) | test(/^suite::boot_heavy$/))"
                " & !test(/^suite::boot_heavy$/)"
            ),
        )

    def test_linux_filters_leave_out_every_windows_server_test(self):
        # The Linux legs run Linux guests. A test's name holds the year of its
        # Windows Server image, so a test that moves to another image, as the
        # KVP IC test moved from 2022 to 2025 (#1035), must stay out of them.
        windows_tests = [
            test for test in ci.OPENVMM_WHP_TESTS if "windows_datacenter_core" in test
        ]
        self.assertIn(
            "multiarch::ic::openvmm_uefi_x64_windows_datacenter_core_2025_x64_kvp_ic",
            windows_tests,
        )
        for backend in ("kvm", "mshv"):
            test_filter = ci.OPENVMM_TEST_FILTERS[backend]
            for test in windows_tests:
                with self.subTest(backend=backend, test=test):
                    self.assertFalse(_nextest_filter_matches(test_filter, test))
            for test in (
                "multiarch::ic::openvmm_uefi_x64_ubuntu_2504_server_x64_timesync_ic",
                *ci.OPENVMM_REQUIRED_MICROVM_TESTS,
            ):
                with self.subTest(backend=backend, test=test):
                    self.assertTrue(_nextest_filter_matches(test_filter, test))

    def test_openvmm_tests_reject_unknown_backend(self):
        with self.assertRaisesRegex(common.ScriptError, "unsupported.*backend"):
            ci.run_openvmm_tests("unknown")


def _nextest_filter_matches(expression: str, test: str) -> bool:
    """Return whether a nextest filter that OpenVMM's test filters spell,
    with test(/REGEX/), test(SUBSTRING), !, &, |, and parentheses, selects a
    test's name."""
    tokens = re.findall(
        r"test\(/(?:[^/\\]|\\.)*/\)|test\([^/)][^)]*\)|[!&|()]", expression
    )
    if "".join(tokens) != "".join(expression.split()):
        raise AssertionError(f"unsupported filter {expression!r}")
    position = 0

    def take() -> str:
        nonlocal position
        position += 1
        return tokens[position - 1]

    def peek() -> str | None:
        return tokens[position] if position < len(tokens) else None

    def primary() -> bool:
        token = take()
        if token == "!":
            return not primary()
        if token == "(":
            value = union()
            if take() != ")":
                raise AssertionError(f"unbalanced filter {expression!r}")
            return value
        if token.startswith("test(/"):
            return re.search(token[6:-2], test) is not None
        # Without slashes, test() matches a substring of the name.
        return token[5:-1] in test

    def intersection() -> bool:
        value = primary()
        while peek() == "&":
            take()
            value = primary() and value
        return value

    def union() -> bool:
        value = intersection()
        while peek() == "|":
            take()
            value = intersection() or value
        return value

    selected = union()
    if position != len(tokens):
        raise AssertionError(f"unbalanced filter {expression!r}")
    return selected


_REUSE_REPOSITORY = "microsoft/nvx"
_REUSE_WORKFLOW = ".github/workflows/ci.yml"
_REUSE_WORKFLOW_REF = f"{_REUSE_REPOSITORY}/{_REUSE_WORKFLOW}@refs/heads/dev"
_REUSE_MERGE = "a" * 40
_REUSE_HEAD = "c" * 40
_REUSE_KEYS = {
    "kernel": "linux-kernel-v1-6.18.38-1111-2222",
    "kernel-debug": "linux-kernel-debug-v1-6.18.38-3333-2222",
    "alpine": "alpine-image-v2-3.24.1-4444-5555",
    "ubuntu": "ubuntu-initramfs-v2-26.04.1-6666-5555",
    "ubuntu-distro": "ubuntu-distro-v2-26.04.1-6666-5555",
    "azurelinux": "azurelinux-initramfs-v2-3.0-7777-5555",
}
# The components of each guest artifact job in ci.yml.
_REUSE_JOBS = (("kernel", "alpine"), ("ubuntu", "azurelinux"))
# The artifact of each job's record.
_REUSE_RECORDS: dict[tuple[str, ...], str] = {
    ("kernel", "alpine"): "kernel-alpine-record",
    ("ubuntu", "azurelinux"): "ubuntu-azurelinux-record",
}
_REUSE_RUNS = (
    f"repos/{_REUSE_REPOSITORY}/actions/workflows/ci.yml/runs"
    f"?event=pull_request&head_sha={_REUSE_HEAD}&per_page=100"
)
# The ID of each artifact of run 102 that the fixtures serve.
_REUSE_ARTIFACT_IDS = {
    "kernel-alpine-record": 9,
    "guest-kernel": 10,
    "guest-alpine": 11,
    "ubuntu-azurelinux-record": 19,
    "guest-ubuntu": 20,
    "guest-ubuntu-distro": 21,
    "guest-azurelinux": 22,
}


def _reuse_list(name: str) -> str:
    """Return the endpoint that lists the artifacts named `name` of run 102."""
    return (
        f"repos/{_REUSE_REPOSITORY}/actions/runs/102/artifacts?name={name}&per_page=100"
    )


def _reuse_download(name: str) -> str:
    """Return the endpoint that downloads the artifact `name` of run 102."""
    return (
        f"repos/{_REUSE_REPOSITORY}/actions/artifacts/{_REUSE_ARTIFACT_IDS[name]}/zip"
    )


def _guest_uploads() -> dict[str, tuple[str, set[str]]]:
    """Return the job that uploads each guest-* artifact of ci.yml, and the
    names of the files in it, by artifact name."""
    workflow = yaml.safe_load(
        (BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )
    )
    uploads: dict[str, tuple[str, set[str]]] = {}
    for job_name, job in workflow["jobs"].items():
        for step in job.get("steps", []):
            arguments = step.get("with", {})
            if str(step.get("uses", "")).startswith("actions/upload-artifact@") and str(
                arguments.get("name", "")
            ).startswith("guest-"):
                uploads[arguments["name"]] = (
                    job_name,
                    {PurePosixPath(path).name for path in arguments["path"].split()},
                )
    return uploads


class _FakeGitHub:
    """Answers reuse's GitHub API requests from canned documents."""

    def __init__(
        self,
        documents: Mapping[str, object],
        downloads: Mapping[str, bytes] | None = None,
    ) -> None:
        self.documents = dict(documents)
        self.downloads = dict(downloads or {})
        self.requests: list[str] = []

    def get(self, endpoint: str) -> object:
        self.requests.append(endpoint)
        if endpoint not in self.documents:
            raise guest_artifact_reuse.ReuseRefused(
                f"GET {endpoint} failed with HTTP 404"
            )
        return self.documents[endpoint]

    def download(self, endpoint: str, destination: Path) -> None:
        self.requests.append(endpoint)
        if endpoint not in self.downloads:
            raise guest_artifact_reuse.ReuseRefused(
                f"GET {endpoint} failed with HTTP 404"
            )
        destination.write_bytes(self.downloads[endpoint])


def _reuse_run(
    run_id: int,
    run_number: int,
    *,
    head_repository: str | None = _REUSE_REPOSITORY,
    event: str = "pull_request",
    head_sha: str = _REUSE_HEAD,
    path: str = _REUSE_WORKFLOW,
) -> dict[str, object]:
    return {
        "id": run_id,
        "run_number": run_number,
        "event": event,
        "head_sha": head_sha,
        "path": path,
        "html_url": f"https://github.com/{_REUSE_REPOSITORY}/actions/runs/{run_id}",
        "head_repository": (
            None if head_repository is None else {"full_name": head_repository}
        ),
        "repository": {"full_name": _REUSE_REPOSITORY},
    }


def _reuse_artifact(
    artifact_id: int,
    run_id: int,
    *,
    name: str,
    expired: bool = False,
    created_at: str = "2026-10-21T12:00:00Z",
    digest: str | None = None,
) -> dict[str, object]:
    return {
        "id": artifact_id,
        "name": name,
        "expired": expired,
        "created_at": created_at,
        "digest": digest,
        "workflow_run": {"id": run_id},
    }


def _reuse_zip(members: Mapping[str, bytes]) -> bytes:
    """Return a ZIP archive of `members`, as an artifact download is."""
    archive = io.BytesIO()
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as bundle:
        for name, contents in members.items():
            bundle.writestr(name, contents)
    return archive.getvalue()


def _reuse_request(
    components: tuple[str, ...] = ("kernel", "alpine"),
    *,
    keys: Mapping[str, str] = _REUSE_KEYS,
    dry_run: bool = False,
) -> guest_artifact_reuse.ReuseRequest:
    return guest_artifact_reuse.ReuseRequest(
        repository=_REUSE_REPOSITORY,
        workflow_ref=_REUSE_WORKFLOW_REF,
        record=_REUSE_RECORDS[components],
        components=components,
        keys=guest_artifact_reuse.component_keys(components, keys),
        commit=None if dry_run else _REUSE_MERGE,
        head_sha=_REUSE_HEAD if dry_run else None,
        dry_run=dry_run,
    )


class GuestArtifactReuseTests(unittest.TestCase):
    template: Path
    kernel_inputs: dict[str, object]
    _template_directory: tempfile.TemporaryDirectory[str]

    @classmethod
    def setUpClass(cls) -> None:
        super().setUpClass()
        # Valid guest artifacts and their provenance.
        cls._template_directory = tempfile.TemporaryDirectory()
        paths, cls.kernel_inputs, _revision = _write_release_fixture(
            Path(cls._template_directory.name)
        )
        cls.template = paths["build"]

    @classmethod
    def tearDownClass(cls) -> None:
        cls._template_directory.cleanup()
        super().tearDownClass()

    @staticmethod
    def _serve(
        api: _FakeGitHub,
        name: str,
        contents: bytes,
        *,
        digest: str | None = None,
    ) -> None:
        """Serve `contents` as the artifact `name` of run 102, with the
        SHA-256 digest of `contents` unless `digest` replaces it."""
        if digest is None:
            digest = f"sha256:{hashlib.sha256(contents).hexdigest()}"
        api.documents[_reuse_list(name)] = {
            "artifacts": [
                _reuse_artifact(
                    _REUSE_ARTIFACT_IDS[name], 102, name=name, digest=digest
                )
            ]
        }
        api.downloads[_reuse_download(name)] = contents

    def _fixture(
        self,
        root: Path,
        components: tuple[str, ...] = ("kernel", "alpine"),
    ) -> tuple[_FakeGitHub, dict[str, object], Path]:
        """Return a fake GitHub API whose merge of _REUSE_HEAD and the latest
        run for it lead to the record of `components` and to their artifacts,
        as ci.yml uploads them, the kernel provenance inputs that the files
        match, and a copy of the files and their record under `root`."""
        files = root / "files"
        files.mkdir()
        for name in guest_artifact_reuse.component_files(components):
            shutil.copyfile(self.template / name, files / name)
        record = guest_artifact_reuse.record_guest_artifacts(
            files, components, _REUSE_KEYS
        )
        api = _FakeGitHub(
            {
                f"repos/{_REUSE_REPOSITORY}/git/commits/{_REUSE_MERGE}": {
                    "sha": _REUSE_MERGE,
                    "parents": [{"sha": "b" * 40}, {"sha": _REUSE_HEAD}],
                },
                _REUSE_RUNS: {
                    "workflow_runs": [
                        _reuse_run(103, 1851, head_repository="fork/nvx"),
                        _reuse_run(102, 1850),
                        _reuse_run(101, 1849),
                    ]
                },
            }
        )
        self._serve(
            api,
            _REUSE_RECORDS[components],
            _reuse_zip({record.name: record.read_bytes()}),
        )
        uploads = _guest_uploads()
        for component in components:
            for name in guest_artifact_reuse.COMPONENT_ARTIFACTS[component]:
                self._serve(
                    api,
                    name,
                    _reuse_zip(
                        {
                            member: (files / member).read_bytes()
                            for member in sorted(uploads[name][1])
                        }
                    ),
                )
        return api, self.kernel_inputs, files

    def _reuse(
        self,
        api: _FakeGitHub,
        kernel_inputs: dict[str, object],
        destination: Path,
        request: guest_artifact_reuse.ReuseRequest | None = None,
    ) -> guest_artifact_reuse.ReuseOutcome:
        with patch.object(
            release, "kernel_provenance_inputs", return_value=kernel_inputs
        ):
            outcome = guest_artifact_reuse.reuse_guest_artifacts(
                api, request or _reuse_request(), destination
            )
        # Reuse leaves no staging directory behind.
        if destination.exists():
            self.assertEqual(
                [path.name for path in destination.iterdir() if path.is_dir()], []
            )
        return outcome

    def _assert_built(
        self,
        outcome: guest_artifact_reuse.ReuseOutcome,
        destination: Path,
        reason: str,
        level: str,
    ) -> None:
        self.assertFalse(outcome.reused)
        self.assertEqual(outcome.status, "build")
        self.assertEqual(outcome.level, level)
        self.assertRegex(outcome.detail, re.compile(reason, re.IGNORECASE))
        self.assertTrue(
            outcome.detail.endswith(
                ", so the action builds the guest images that missed the cache."
            ),
            outcome.detail,
        )
        installed = (
            [path.name for path in destination.iterdir()]
            if destination.exists()
            else []
        )
        self.assertEqual(installed, [])

    def test_record_names_the_cache_keys_and_the_digest_of_every_file(self):
        for components in _REUSE_JOBS:
            with (
                self.subTest(components=components),
                tempfile.TemporaryDirectory() as temporary,
            ):
                directory = Path(temporary)
                for name in guest_artifact_reuse.component_files(components):
                    shutil.copyfile(self.template / name, directory / name)
                # No record covers the CI debug kernel.
                (directory / KernelBuildConstants.DEBUG_BINARY_NAME).write_bytes(
                    b"debug kernel"
                )
                stdout = io.StringIO()
                args = nvx.parse_args(
                    [
                        "record-guest-artifacts",
                        "--components",
                        " ".join(reversed(components)),
                        "--keys",
                        json.dumps(_REUSE_KEYS, indent=2),
                        "--directory",
                        os.fspath(directory),
                    ]
                )
                with contextlib.redirect_stdout(stdout):
                    args.handler(args)

                record = json.loads(
                    (directory / guest_artifact_reuse.RECORD_NAME).read_text(
                        encoding="utf-8"
                    )
                )
                # Only the keys of the components' own cache entries.
                self.assertEqual(
                    record,
                    {
                        "format": 1,
                        "keys": {
                            name: _REUSE_KEYS[name]
                            for name in sorted(
                                name
                                for component in components
                                for name in guest_artifact_reuse.COMPONENT_CACHE_KEYS[
                                    component
                                ]
                            )
                        },
                        "files": {
                            name: common.sha256_file(directory / name)
                            for component in components
                            for name in release.GUEST_ARTIFACT_COMPONENTS[component]
                        },
                    },
                )
                # The Ubuntu component holds the sandbox scratch template too.
                self.assertEqual(
                    len(record["files"]),
                    {("kernel", "alpine"): 6, ("ubuntu", "azurelinux"): 7}[components],
                )
                self.assertNotIn("kernel-debug", record["keys"])
                self.assertIn(guest_artifact_reuse.RECORD_NAME, stdout.getvalue())

                first = guest_artifact_reuse.component_files(components)[1]
                (directory / first).unlink()
                with self.assertRaisesRegex(
                    common.ScriptError, f"guest artifact {re.escape(first)} not found"
                ):
                    guest_artifact_reuse.record_guest_artifacts(
                        directory, components, _REUSE_KEYS
                    )

    def test_record_requires_the_key_of_each_cache_entry(self):
        keys = {
            name: key for name, key in _REUSE_KEYS.items() if name != "ubuntu-distro"
        }
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            components = ("ubuntu", "azurelinux")
            for name in guest_artifact_reuse.component_files(components):
                shutil.copyfile(self.template / name, directory / name)
            guest_artifact_reuse.record_guest_artifacts(
                directory, ("azurelinux",), keys
            )
            with self.assertRaisesRegex(
                common.ScriptError, "the cache keys give no ubuntu-distro key"
            ):
                guest_artifact_reuse.record_guest_artifacts(directory, components, keys)

    def test_record_components_are_those_that_a_package_ships(self):
        self.assertEqual(
            list(guest_artifact_reuse.COMPONENT_CACHE_KEYS),
            list(release.GUEST_ARTIFACT_COMPONENTS),
        )
        for text, expected in (
            ("kernel alpine", ("kernel", "alpine")),
            (" alpine\nkernel  kernel ", ("kernel", "alpine")),
            ("azurelinux ubuntu", ("ubuntu", "azurelinux")),
        ):
            with self.subTest(components=text):
                self.assertEqual(guest_artifact_reuse.parse_components(text), expected)
        for text in (
            "",
            "   ",
            "debug-kernel",
            "kernel debug-kernel",
            "Kernel",
            "kernel,alpine",
        ):
            with (
                self.subTest(components=text),
                self.assertRaises(common.ScriptError),
            ):
                guest_artifact_reuse.parse_components(text)

    def test_cache_keys_must_be_a_json_object_of_nonempty_strings(self):
        self.assertEqual(
            guest_artifact_reuse.parse_cache_keys(json.dumps(_REUSE_KEYS)),
            _REUSE_KEYS,
        )
        for text in (
            "",
            "kernel",
            "[]",
            "{}",
            '{"kernel": ""}',
            '{"kernel": 1}',
            '{"": "key"}',
            '{"kernel": "a", "kernel": "b"}',
        ):
            with (
                self.subTest(keys=text),
                self.assertRaises(common.ScriptError),
            ):
                guest_artifact_reuse.parse_cache_keys(text)

    def test_push_installs_every_file_of_its_components_when_one_missed(self):
        for components, cached in (
            # The cache restored the kernel but not the Alpine initramfs.
            (
                ("kernel", "alpine"),
                (
                    KernelBuildConstants.BINARY_NAME,
                    KernelBuildConstants.CONFIG_NAME,
                    KernelBuildConstants.PROVENANCE_NAME,
                ),
            ),
            # A restore that failed partway left a truncated distro layer.
            (("ubuntu", "azurelinux"), (UbuntuBuildConstants.DISTRO_NAME,)),
        ):
            with (
                self.subTest(components=components),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                api, kernel_inputs, files = self._fixture(root, components)
                destination = root / "destination"
                destination.mkdir()
                for name in cached:
                    (destination / name).write_bytes(b"cached " + name.encode())
                # The files of the other components stay as they are.
                (destination / "other").write_bytes(b"other")

                outcome = self._reuse(
                    api, kernel_inputs, destination, _reuse_request(components)
                )

                self.assertTrue(outcome.reused, outcome.detail)
                self.assertEqual(outcome.status, "reused")
                self.assertIsNone(outcome.level)
                self.assertEqual(
                    outcome.heading,
                    f"Guest artifact reuse ({', '.join(components)})",
                )
                self.assertIn(
                    "Installed every file of these components", outcome.detail
                )
                self.assertIn(
                    "[run 102](https://github.com/microsoft/nvx/actions/runs/102)",
                    outcome.detail,
                )
                artifacts = [
                    name
                    for component in components
                    for name in guest_artifact_reuse.COMPONENT_ARTIFACTS[component]
                ]
                record = _REUSE_RECORDS[components]
                self.assertEqual(
                    api.requests,
                    [
                        f"repos/{_REUSE_REPOSITORY}/git/commits/{_REUSE_MERGE}",
                        _REUSE_RUNS,
                        _reuse_list(record),
                        _reuse_download(record),
                        *(
                            endpoint
                            for name in artifacts
                            for endpoint in (_reuse_list(name), _reuse_download(name))
                        ),
                    ],
                )
                # Every file of the components comes from the checked
                # artifacts, and no other file.
                names = guest_artifact_reuse.component_files(components)
                self.assertEqual(
                    sorted(path.name for path in destination.iterdir()),
                    sorted((*names, "other")),
                )
                for name in names:
                    with self.subTest(file=name):
                        self.assertEqual(
                            (destination / name).read_bytes(),
                            (files / name).read_bytes(),
                        )
                self.assertEqual((destination / "other").read_bytes(), b"other")

    def test_rehearsal_checks_the_artifacts_without_installing_them(self):
        for components in _REUSE_JOBS:
            with (
                self.subTest(components=components),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                api, kernel_inputs, _files = self._fixture(root, components)
                destination = root / "destination"
                destination.mkdir()
                # A rehearsal checks the artifacts even when the cache
                # restored every file.
                for name in guest_artifact_reuse.component_files(components):
                    (destination / name).write_bytes(b"cached")

                outcome = self._reuse(
                    api,
                    kernel_inputs,
                    destination,
                    _reuse_request(components, dry_run=True),
                )

                self.assertFalse(outcome.reused)
                self.assertEqual(
                    outcome.heading,
                    f"Guest artifact reuse rehearsal ({', '.join(components)})",
                )
                self.assertEqual(outcome.status, "would reuse", outcome.detail)
                self.assertIsNone(outcome.level)
                # The rehearsal names the head, so it needs no merge commit.
                self.assertEqual(api.requests[0], _REUSE_RUNS)
                self.assertIn(_reuse_download(_REUSE_RECORDS[components]), api.requests)
                for name in guest_artifact_reuse.component_files(components):
                    self.assertEqual((destination / name).read_bytes(), b"cached")

                # A rehearsal that its own run can't serve warns, even for a
                # reason that a push would only note.
                record = _REUSE_RECORDS[components]
                api.documents[_reuse_list(record)] = {
                    "artifacts": [
                        _reuse_artifact(
                            _REUSE_ARTIFACT_IDS[record], 102, name=record, expired=True
                        )
                    ]
                }

                outcome = self._reuse(
                    api,
                    kernel_inputs,
                    destination,
                    _reuse_request(components, dry_run=True),
                )

                self.assertFalse(outcome.reused)
                self.assertEqual(outcome.status, "build")
                self.assertEqual(outcome.level, "warning")
                self.assertTrue(
                    outcome.detail.endswith(
                        f"has no unexpired `{record}` artifact, so a merge group "
                        "or a push that merged this head would build the guest "
                        "images that missed its cache."
                    ),
                    outcome.detail,
                )

    def test_reuse_is_not_needed_when_the_cache_restored_every_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary)
            # Those of the other components don't count.
            for name in guest_artifact_reuse.component_files(("kernel", "alpine")):
                (destination / name).write_bytes(b"cached")
            api = _FakeGitHub({})

            outcome = guest_artifact_reuse.reuse_guest_artifacts(
                api, _reuse_request(), destination
            )

            self.assertFalse(outcome.reused)
            self.assertEqual(outcome.status, "not needed")
            self.assertIsNone(outcome.level)
            self.assertEqual(api.requests, [])

    def test_only_same_repository_pull_request_runs_of_the_workflow_qualify(self):
        def latest(*runs: dict[str, object]) -> guest_artifact_reuse.PullRequestRun:
            api = _FakeGitHub({_REUSE_RUNS: {"workflow_runs": list(runs)}})
            return guest_artifact_reuse.latest_pull_request_run(
                api, _REUSE_REPOSITORY, _REUSE_WORKFLOW, _REUSE_HEAD
            )

        ineligible = (
            _reuse_run(201, 1901, head_repository="fork/nvx"),
            _reuse_run(202, 1902, head_repository=None),
            _reuse_run(203, 1903, event="push"),
            _reuse_run(204, 1904, event="workflow_dispatch"),
            _reuse_run(205, 1905, head_sha="d" * 40),
            _reuse_run(206, 1906, path=".github/workflows/adversarial.yml"),
        )
        self.assertEqual(
            latest(_reuse_run(101, 1849), *ineligible, _reuse_run(102, 1850)),
            guest_artifact_reuse.PullRequestRun(
                102, "https://github.com/microsoft/nvx/actions/runs/102"
            ),
        )
        with self.assertRaises(guest_artifact_reuse.ReuseRefused) as raised:
            latest(*ineligible)
        self.assertTrue(raised.exception.expected)
        self.assertIn(
            f"no pull request run of `{_REUSE_WORKFLOW}` from microsoft/nvx built "
            f"`{_REUSE_HEAD}`; 2 from other repositories never qualify",
            str(raised.exception),
        )
        with self.assertRaises(guest_artifact_reuse.ReuseRefused):
            latest({"event": "pull_request", "head_sha": _REUSE_HEAD})

    def test_fork_runs_never_supply_guest_artifacts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            api, kernel_inputs, _files = self._fixture(root)
            api.documents[_REUSE_RUNS] = {
                "workflow_runs": [
                    _reuse_run(102, 1850, head_repository="fork/nvx"),
                ]
            }
            destination = root / "destination"

            outcome = self._reuse(api, kernel_inputs, destination)

            self._assert_built(
                outcome, destination, "from other repositories never qualify", "notice"
            )
            self.assertEqual(len(api.requests), 2)

    def test_a_push_that_merged_no_pull_request_builds(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            api, kernel_inputs, _files = self._fixture(root)
            api.documents[f"repos/{_REUSE_REPOSITORY}/git/commits/{_REUSE_MERGE}"] = {
                "sha": _REUSE_MERGE,
                "parents": [{"sha": "b" * 40}],
            }
            destination = root / "destination"

            outcome = self._reuse(api, kernel_inputs, destination)

            self._assert_built(
                outcome,
                destination,
                f"^`{_REUSE_MERGE}` is not the merge commit of a pull request",
                "notice",
            )
            self.assertEqual(len(api.requests), 1)

    def test_only_an_unexpired_artifact_of_the_run_qualifies(self):
        run = guest_artifact_reuse.PullRequestRun(102, "https://example.invalid/102")
        name = "kernel-alpine-record"

        def retained(*artifacts: dict[str, object]) -> guest_artifact_reuse.RunArtifact:
            api = _FakeGitHub({_reuse_list(name): {"artifacts": list(artifacts)}})
            return guest_artifact_reuse.retained_artifact(
                api, _REUSE_REPOSITORY, run, name
            )

        ineligible = (
            _reuse_artifact(
                1, 102, name=name, expired=True, created_at="2026-10-21T13:00:00Z"
            ),
            _reuse_artifact(2, 102, name="guest-kernel"),
            _reuse_artifact(3, 101, name=name, created_at="2026-10-21T14:00:00Z"),
        )
        self.assertEqual(
            retained(
                _reuse_artifact(4, 102, name=name, created_at="2026-10-21T11:00:00Z"),
                *ineligible,
                _reuse_artifact(5, 102, name=name, digest="sha256:" + "e" * 64),
            ),
            guest_artifact_reuse.RunArtifact(5, name, "sha256:" + "e" * 64),
        )
        with self.assertRaises(guest_artifact_reuse.ReuseRefused) as raised:
            retained(*ineligible)
        self.assertTrue(raised.exception.expected)
        self.assertEqual(
            str(raised.exception),
            "[run 102](https://example.invalid/102) has no unexpired "
            "`kernel-alpine-record` artifact",
        )

    def test_reuse_needs_the_record_and_every_artifact_of_the_components(self):
        for name in (
            "ubuntu-azurelinux-record",
            "guest-ubuntu",
            "guest-ubuntu-distro",
            "guest-azurelinux",
        ):
            with (
                self.subTest(artifact=name),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                components = ("ubuntu", "azurelinux")
                api, kernel_inputs, _files = self._fixture(root, components)
                # Expired, as after a day, or never uploaded.
                api.documents[_reuse_list(name)] = {
                    "artifacts": [
                        _reuse_artifact(
                            _REUSE_ARTIFACT_IDS[name], 102, name=name, expired=True
                        )
                    ]
                }
                destination = root / "destination"

                outcome = self._reuse(
                    api, kernel_inputs, destination, _reuse_request(components)
                )

                self._assert_built(
                    outcome,
                    destination,
                    re.escape(f"has no unexpired `{name}` artifact"),
                    "notice",
                )
                self.assertNotIn(_reuse_download(name), api.requests)

    def test_reuse_requires_the_recorded_cache_keys_to_equal_its_own(self):
        def record(keys: Mapping[str, str]) -> bytes:
            document = {
                "format": 1,
                "keys": dict(keys),
                "files": {
                    name: "0" * 64
                    for name in guest_artifact_reuse.component_files(
                        ("kernel", "alpine")
                    )
                },
            }
            return _reuse_zip({"guest-artifacts.json": json.dumps(document).encode()})

        own = guest_artifact_reuse.component_keys(("kernel", "alpine"), _REUSE_KEYS)
        for case, keys, record_keys, names in (
            (
                "changed",
                dict(_REUSE_KEYS, alpine="alpine-image-v2-3.24.1-other-5555"),
                own,
                "alpine",
            ),
            ("missing", _REUSE_KEYS, {"alpine": own["alpine"]}, "kernel"),
            ("extra", _REUSE_KEYS, dict(own, ubuntu=_REUSE_KEYS["ubuntu"]), "ubuntu"),
            ("other", _REUSE_KEYS, {"kernel": "other"}, "alpine, kernel"),
        ):
            with (
                self.subTest(case=case),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                api, kernel_inputs, _files = self._fixture(root)
                if record_keys is not own:
                    self._serve(api, "kernel-alpine-record", record(record_keys))
                destination = root / "destination"

                outcome = self._reuse(
                    api, kernel_inputs, destination, _reuse_request(keys=keys)
                )

                self._assert_built(
                    outcome,
                    destination,
                    f"gives other cache keys for {re.escape(names)},",
                    "notice",
                )
                # Other keys refuse the components before any of their
                # artifacts downloads.
                self.assertNotIn(_reuse_download("guest-kernel"), api.requests)

        # A change in the key of another job's component does not matter.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            api, kernel_inputs, _files = self._fixture(root)
            destination = root / "destination"

            outcome = self._reuse(
                api,
                kernel_inputs,
                destination,
                _reuse_request(keys=dict(_REUSE_KEYS, ubuntu="other", azurelinux="")),
            )

            self.assertTrue(outcome.reused, outcome.detail)

    def test_reuse_refuses_files_that_do_not_match_the_record_or_artifact(self):
        record_name = "kernel-alpine-record"

        def record(files: Path, update: Mapping[str, object]) -> bytes:
            document = json.loads(
                (files / guest_artifact_reuse.RECORD_NAME).read_bytes()
            )
            document.update(update)
            return _reuse_zip({"guest-artifacts.json": json.dumps(document).encode()})

        def record_files(
            files: Path, change: Callable[[dict[str, str]], object]
        ) -> bytes:
            document = json.loads(
                (files / guest_artifact_reuse.RECORD_NAME).read_bytes()
            )
            change(document["files"])
            return _reuse_zip({"guest-artifacts.json": json.dumps(document).encode()})

        def members(files: Path, name: str) -> dict[str, bytes]:
            return {
                member: (files / member).read_bytes()
                for member in sorted(_guest_uploads()[name][1])
            }

        cases: tuple[tuple[str, str, Callable[[Path], bytes], str, str], ...] = (
            (
                "tampered file",
                "guest-alpine",
                lambda files: _reuse_zip(
                    {**members(files, "guest-alpine"), "initramfs.cpio.gz": b"x"}
                ),
                "initramfs.cpio.gz in artifact `guest-alpine` does not match the "
                "SHA-256 that the record gives",
                "warning",
            ),
            (
                "missing file",
                "guest-kernel",
                lambda files: _reuse_zip(
                    {
                        name: contents
                        for name, contents in members(files, "guest-kernel").items()
                        if name != "vmlinux.config"
                    }
                ),
                "holds vmlinux.config",
                "warning",
            ),
            (
                "file in two artifacts",
                "guest-alpine",
                lambda files: _reuse_zip(
                    {
                        **members(files, "guest-alpine"),
                        "vmlinux": (files / "vmlinux").read_bytes(),
                    }
                ),
                "the artifacts hold vmlinux more than once",
                "warning",
            ),
            (
                "record artifact without a record",
                record_name,
                lambda _files: _reuse_zip({"other.json": b"{}"}),
                "artifact `kernel-alpine-record` holds no single guest-artifacts.json",
                "warning",
            ),
            (
                "record without a file",
                record_name,
                lambda files: record_files(
                    files, lambda digests: digests.pop("vmlinux.config")
                ),
                "describes other files than those of kernel and alpine",
                "warning",
            ),
            (
                "record with a file of another component",
                record_name,
                lambda files: record_files(
                    files,
                    lambda digests: digests.update({"ubuntu-distro.erofs": "0" * 64}),
                ),
                "describes other files than those of kernel and alpine",
                "warning",
            ),
            (
                "record without a digest",
                record_name,
                lambda files: record_files(
                    files, lambda digests: digests.update({"vmlinux": "MD5"})
                ),
                "record is invalid: it gives no SHA-256 of vmlinux",
                "warning",
            ),
            (
                "unknown record format",
                record_name,
                lambda files: record(files, {"format": 2}),
                "record is invalid: its format is not 1",
                "warning",
            ),
            (
                "boolean record format",
                record_name,
                lambda files: record(files, {"format": True}),
                "record is invalid: its format is not 1",
                "warning",
            ),
            (
                "malformed record",
                record_name,
                lambda _files: _reuse_zip({"guest-artifacts.json": b"{"}),
                "record is invalid",
                "warning",
            ),
            (
                "record not an archive",
                record_name,
                lambda _files: b"not a ZIP archive",
                "artifact `kernel-alpine-record` is not a valid ZIP archive",
                "warning",
            ),
            (
                "files not an archive",
                "guest-kernel",
                lambda _files: b"not a ZIP archive",
                "artifact `guest-kernel` is not a valid ZIP archive",
                "warning",
            ),
        )
        for case, name, contents, reason, level in cases:
            with (
                self.subTest(case=case),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                api, kernel_inputs, files = self._fixture(root)
                self._serve(api, name, contents(files))
                destination = root / "destination"

                outcome = self._reuse(api, kernel_inputs, destination)

                self._assert_built(outcome, destination, re.escape(reason), level)

        for name in (record_name, "guest-kernel", "guest-alpine"):
            with (
                self.subTest(digest_of=name),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                api, kernel_inputs, _files = self._fixture(root)
                contents = api.downloads[_reuse_download(name)]
                self._serve(api, name, contents, digest="sha256:" + "0" * 64)
                destination = root / "destination"

                outcome = self._reuse(api, kernel_inputs, destination)

                self._assert_built(
                    outcome,
                    destination,
                    re.escape(
                        f"artifact {_REUSE_ARTIFACT_IDS[name]} does not match its "
                        "digest sha256:" + "0" * 64
                    ),
                    "warning",
                )

        # Without the digest that GitHub took at the upload, nothing but the
        # archive itself would vouch for it, so reuse doesn't download it.
        for digest in (None, "md5:" + "0" * 32, "sha256:00", "SHA256:" + "0" * 64):
            for name in (record_name, "guest-alpine"):
                with (
                    self.subTest(digest=digest, artifact=name),
                    tempfile.TemporaryDirectory() as temporary,
                ):
                    root = Path(temporary)
                    api, kernel_inputs, _files = self._fixture(root)
                    api.documents[_reuse_list(name)] = {
                        "artifacts": [
                            _reuse_artifact(
                                _REUSE_ARTIFACT_IDS[name], 102, name=name, digest=digest
                            )
                        ]
                    }
                    destination = root / "destination"

                    outcome = self._reuse(api, kernel_inputs, destination)

                    self._assert_built(
                        outcome,
                        destination,
                        re.escape(
                            f"artifact {_REUSE_ARTIFACT_IDS[name]} has no SHA-256 "
                            f"digest, but {digest!r}"
                        ),
                        "warning",
                    )
                    self.assertNotIn(_reuse_download(name), api.requests)

    def test_reuse_requires_provenance_that_matches_this_checkout(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            components = ("ubuntu", "azurelinux")
            api, kernel_inputs, files = self._fixture(root, components)
            # A distro layer built from other inputs, recorded as such.
            manifest = files / UbuntuBuildConstants.DISTRO_MANIFEST_NAME
            document = json.loads(manifest.read_text(encoding="utf-8"))
            document["input_sha256"] = "f" * 64
            manifest.write_text(json.dumps(document), encoding="utf-8")
            record = guest_artifact_reuse.record_guest_artifacts(
                files, components, _REUSE_KEYS
            )
            self._serve(
                api,
                "ubuntu-azurelinux-record",
                _reuse_zip({record.name: record.read_bytes()}),
            )
            self._serve(
                api,
                "guest-ubuntu-distro",
                _reuse_zip(
                    {
                        name: (files / name).read_bytes()
                        for name in sorted(_guest_uploads()["guest-ubuntu-distro"][1])
                    }
                ),
            )
            destination = root / "destination"

            outcome = self._reuse(
                api, kernel_inputs, destination, _reuse_request(components)
            )

            self._assert_built(
                outcome,
                destination,
                "do not match their provenance or this checkout: Ubuntu artifact "
                "manifest does not match ubuntu-distro.erofs",
                "warning",
            )

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            api, _kernel_inputs, _files = self._fixture(root)
            destination = root / "destination"

            # The fixture's kernel provenance names inputs that this checkout
            # lacks.
            outcome = guest_artifact_reuse.reuse_guest_artifacts(
                api, _reuse_request(), destination
            )

            self._assert_built(
                outcome,
                destination,
                "kernel build provenance does not match",
                "warning",
            )

    def test_reuse_installs_every_file_or_leaves_the_destination_as_it_was(self):
        # The installation moves a restored file aside (calls 1 and 3) before
        # it installs its replacement (2 and 4). Each rename fails before it
        # happens, or right after it, before the installation can note it.
        replace = os.replace
        failure = OSError("disk full")
        interruption = guest_artifact_reuse.ReuseRefused("interrupted")
        for failing_call, error, after_rename in (
            *((number, failure, False) for number in (1, 2, 3, 4)),
            *((number, interruption, False) for number in (1, 2, 3, 4)),
            *((number, interruption, True) for number in (1, 2, 3, 4)),
        ):
            with (
                self.subTest(
                    failing_call=failing_call,
                    error=str(error),
                    after_rename=after_rename,
                ),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                api, kernel_inputs, _files = self._fixture(root)
                destination = root / "destination"
                destination.mkdir()
                restored = {
                    KernelBuildConstants.BINARY_NAME: b"cached kernel",
                    KernelBuildConstants.CONFIG_NAME: b"cached kernel config",
                }
                for name, contents in restored.items():
                    (destination / name).write_bytes(contents)
                calls: list[str] = []

                def fail(
                    source: Path,
                    target: Path,
                    calls: list[str] = calls,
                    failing_call: int = failing_call,
                    error: BaseException = error,
                    after_rename: bool = after_rename,
                ) -> None:
                    calls.append(Path(target).name)
                    if len(calls) != failing_call:
                        replace(source, target)
                        return
                    if after_rename:
                        replace(source, target)
                    raise error

                with patch.object(guest_artifact_reuse.os, "replace", fail):
                    outcome = self._reuse(api, kernel_inputs, destination)

                self.assertFalse(outcome.reused)
                self.assertEqual(outcome.status, "build")
                self.assertIn(str(error).lower(), outcome.detail.lower())
                self.assertEqual(
                    {
                        path.name: path.read_bytes()
                        for path in destination.iterdir()
                        if path.is_file()
                    },
                    restored,
                )

    def test_time_limit_refuses_an_expired_deadline_at_once(self):
        with guest_artifact_reuse._time_limit(None):
            pass
        with (
            self.assertRaisesRegex(
                guest_artifact_reuse.ReuseRefused, "reuse ran out of time"
            ),
            guest_artifact_reuse._time_limit(time.monotonic() - 1),
        ):
            self.fail("an expired time limit ran its block")

    def test_time_limit_interrupts_work_that_outlasts_it(self):
        if sys.platform == "win32":
            self.skipTest("Windows has no interval timer")
        previous = signal.getsignal(signal.SIGALRM)
        start = time.monotonic()
        with (
            self.assertRaisesRegex(
                guest_artifact_reuse.ReuseRefused, "reuse ran out of time"
            ),
            guest_artifact_reuse._time_limit(time.monotonic() + 0.2),
        ):
            time.sleep(10)
        self.assertLess(time.monotonic() - start, 5)
        self.assertEqual(signal.getitimer(signal.ITIMER_REAL), (0.0, 0.0))
        self.assertEqual(signal.getsignal(signal.SIGALRM), previous)

    def test_time_limit_spares_the_installation(self):
        if sys.platform == "win32":
            self.skipTest("Windows has no interval timer")
        install = guest_artifact_reuse._install
        timers: dict[str, tuple[float, float]] = {}

        class TimedGitHub(_FakeGitHub):
            def get(self, endpoint: str) -> object:
                if sys.platform != "win32":
                    timers.setdefault("request", signal.getitimer(signal.ITIMER_REAL))
                return super().get(endpoint)

        def timed_install(
            staging: Path, destination: Path, names: tuple[str, ...]
        ) -> None:
            if sys.platform != "win32":
                timers["installation"] = signal.getitimer(signal.ITIMER_REAL)
            install(staging, destination, names)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture, kernel_inputs, _files = self._fixture(root)
            api = TimedGitHub(fixture.documents, fixture.downloads)
            with (
                patch.object(
                    release, "kernel_provenance_inputs", return_value=kernel_inputs
                ),
                patch.object(guest_artifact_reuse, "_install", timed_install),
            ):
                outcome = guest_artifact_reuse.reuse_guest_artifacts(
                    api,
                    _reuse_request(),
                    root / "destination",
                    deadline=time.monotonic() + 300,
                )

        self.assertTrue(outcome.reused, outcome.detail)
        # The timer runs while reuse fetches and checks the artifact, but not
        # while it installs the images, so it can't cut a rollback short.
        self.assertGreater(timers["request"][0], 0)
        self.assertEqual(timers["installation"], (0.0, 0.0))
        self.assertEqual(signal.getitimer(signal.ITIMER_REAL), (0.0, 0.0))

    def test_time_limit_ignores_its_timer_once_its_block_ends(self):
        if sys.platform == "win32":
            self.skipTest("Windows has no interval timer")
        previous = signal.getsignal(signal.SIGALRM)
        with guest_artifact_reuse._time_limit(time.monotonic() + 300):
            expire = signal.getsignal(signal.SIGALRM)
        self.assertEqual(signal.getsignal(signal.SIGALRM), previous)
        # A signal that arrived just before the timer stopped runs the handler
        # after the block, where it must not raise in place of the restoration.
        assert callable(expire)
        expire(signal.SIGALRM, None)

    def test_reuse_cleans_up_when_its_time_limit_expires_as_the_checks_end(self):
        @contextlib.contextmanager
        def expiring(_deadline: float | None) -> Generator[None]:
            yield
            raise guest_artifact_reuse.ReuseRefused("reuse ran out of time")

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            api, kernel_inputs, _files = self._fixture(root)
            destination = root / "destination"
            with patch.object(guest_artifact_reuse, "_time_limit", expiring):
                outcome = self._reuse(api, kernel_inputs, destination)

            self._assert_built(
                outcome, destination, "^Reuse ran out of time, so ", "warning"
            )

    def test_reuse_command_falls_back_when_reuse_outlasts_its_time_limit(self):
        if sys.platform == "win32":
            self.skipTest("Windows has no interval timer")

        class SlowGitHub(_FakeGitHub):
            def get(self, endpoint: str) -> object:
                time.sleep(10)
                return super().get(endpoint)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "output"
            stdout = io.StringIO()
            start = time.monotonic()
            with (
                patch.object(guest_artifact_reuse, "REUSE_TIMEOUT_SECONDS", 0.2),
                patch.object(
                    guest_artifact_reuse, "RestApi", return_value=SlowGitHub({})
                ),
                contextlib.redirect_stdout(stdout),
            ):
                outcome = guest_artifact_reuse.reuse_in_ci(
                    repository=_REUSE_REPOSITORY,
                    workflow_ref=_REUSE_WORKFLOW_REF,
                    record="kernel-alpine-record",
                    components="kernel alpine",
                    keys=json.dumps(_REUSE_KEYS),
                    commit=_REUSE_MERGE,
                    head_sha=None,
                    dry_run=False,
                    destination=root / "destination",
                    environment={"GH_TOKEN": "token", "GITHUB_OUTPUT": str(output)},
                )

            self.assertLess(time.monotonic() - start, 5)
            self.assertFalse(outcome.reused)
            self.assertEqual(outcome.level, "warning")
            self.assertTrue(
                outcome.detail.startswith("Reuse ran out of time, so "),
                outcome.detail,
            )
            self.assertEqual(output.read_text(encoding="utf-8"), "reused=false\n")

    def test_api_failures_fall_back_to_building(self):
        for endpoint in (
            f"repos/{_REUSE_REPOSITORY}/git/commits/{_REUSE_MERGE}",
            _REUSE_RUNS,
            _reuse_list("kernel-alpine-record"),
            _reuse_download("kernel-alpine-record"),
            _reuse_list("guest-kernel"),
            _reuse_download("guest-alpine"),
        ):
            with (
                self.subTest(endpoint=endpoint),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                api, kernel_inputs, _files = self._fixture(root)
                api.documents.pop(endpoint, None)
                api.downloads.pop(endpoint, None)
                destination = root / "destination"

                outcome = self._reuse(api, kernel_inputs, destination)

                self._assert_built(
                    outcome,
                    destination,
                    re.escape(f"GET {endpoint} failed with HTTP 404"),
                    "warning",
                )

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            api, kernel_inputs, _files = self._fixture(root)
            api.documents[_REUSE_RUNS] = {"workflow_runs": "invalid"}
            destination = root / "destination"

            outcome = self._reuse(api, kernel_inputs, destination)

            self._assert_built(
                outcome,
                destination,
                "GitHub returned an invalid workflow run list",
                "warning",
            )

    def test_workflow_path_comes_from_the_workflow_ref(self):
        for workflow_ref in (
            f"{_REUSE_REPOSITORY}/{_REUSE_WORKFLOW}@refs/heads/dev",
            f"{_REUSE_REPOSITORY}/{_REUSE_WORKFLOW}@refs/pull/702/merge",
        ):
            with self.subTest(workflow_ref=workflow_ref):
                self.assertEqual(
                    guest_artifact_reuse.workflow_path(workflow_ref, _REUSE_REPOSITORY),
                    _REUSE_WORKFLOW,
                )
        for workflow_ref in (
            f"{_REUSE_REPOSITORY}/{_REUSE_WORKFLOW}",
            f"fork/nvx/{_REUSE_WORKFLOW}@refs/heads/dev",
            f"{_REUSE_REPOSITORY}/ci.yml@refs/heads/dev",
            f"{_REUSE_REPOSITORY}/.github/workflows/nested/ci.yml@refs/heads/dev",
        ):
            with self.subTest(workflow_ref=workflow_ref):
                with self.assertRaises(guest_artifact_reuse.ReuseRefused):
                    guest_artifact_reuse.workflow_path(workflow_ref, _REUSE_REPOSITORY)
                # The parser that the reuse shares refuses it as a plain error.
                with self.assertRaises(common.ScriptError) as refused:
                    common.workflow_path(workflow_ref, _REUSE_REPOSITORY)
                self.assertNotIsInstance(
                    refused.exception, guest_artifact_reuse.ReuseRefused
                )
        # The cancellation of superseded merge groups shares it too.
        self.assertIs(merge_queue.workflow_path, common.workflow_path)

    def test_reuse_command_reports_its_outcome_and_never_fails(self):
        base = [
            "reuse-guest-artifacts",
            "--repository",
            _REUSE_REPOSITORY,
            "--workflow-ref",
            _REUSE_WORKFLOW_REF,
            "--record-artifact",
            "kernel-alpine-record",
            "--commit",
            _REUSE_MERGE,
        ]
        keys = json.dumps(_REUSE_KEYS)
        heading = "Guest artifact reuse (kernel, alpine)"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            api, kernel_inputs, _files = self._fixture(root)
            output = root / "output"
            summary = root / "summary"
            destination = root / "destination"
            for case, components, keys_text, token, reused, level, title in (
                ("reused", "alpine kernel", keys, "token", "true", None, heading),
                ("restored", "kernel alpine", keys, "token", "false", None, heading),
                ("no token", "kernel alpine", keys, "", "false", "warning", heading),
                (
                    "invalid keys",
                    "kernel alpine",
                    "{",
                    "token",
                    "false",
                    "warning",
                    heading,
                ),
                (
                    "missing key",
                    "kernel alpine",
                    json.dumps({"kernel": "key"}),
                    "token",
                    "false",
                    "warning",
                    heading,
                ),
                (
                    "unknown component",
                    "kernel debug-kernel",
                    keys,
                    "token",
                    "false",
                    "warning",
                    "Guest artifact reuse",
                ),
            ):
                with self.subTest(case=case):
                    output.write_text("", encoding="utf-8")
                    summary.write_text("", encoding="utf-8")
                    args = nvx.parse_args(
                        [
                            *base,
                            "--components",
                            components,
                            "--keys",
                            keys_text,
                            "--destination",
                            os.fspath(destination),
                        ]
                    )
                    stdout = io.StringIO()
                    with (
                        patch.dict(
                            os.environ,
                            {
                                "GH_TOKEN": token,
                                "GITHUB_TOKEN": "",
                                "GITHUB_OUTPUT": os.fspath(output),
                                "GITHUB_STEP_SUMMARY": os.fspath(summary),
                            },
                        ),
                        patch.object(
                            guest_artifact_reuse, "RestApi", return_value=api
                        ) as rest_api,
                        patch.object(
                            release,
                            "kernel_provenance_inputs",
                            return_value=kernel_inputs,
                        ),
                        contextlib.redirect_stdout(stdout),
                    ):
                        self.assertIsNone(args.handler(args))
                    self.assertEqual(
                        output.read_text(encoding="utf-8"), f"reused={reused}\n"
                    )
                    self.assertTrue(
                        summary.read_text(encoding="utf-8").startswith(f"### {title}: ")
                    )
                    annotations = [
                        line
                        for line in stdout.getvalue().splitlines()
                        if line.startswith("::")
                    ]
                    if level is None:
                        self.assertEqual(annotations, [])
                    else:
                        self.assertEqual(len(annotations), 1)
                        self.assertTrue(
                            annotations[0].startswith(
                                f"::{level} title={title.replace(',', '%2C')}::"
                            ),
                            annotations[0],
                        )
                        rest_api.assert_not_called()
        with (
            patch("sys.stderr", new_callable=io.StringIO),
            self.assertRaises(SystemExit) as exit_context,
        ):
            nvx.parse_args(base[:-2] + ["--components", "kernel", "--keys", "{}"])
        self.assertEqual(exit_context.exception.code, 2)
        # A rehearsal names the head instead of the merge commit.
        with (
            patch("sys.stderr", new_callable=io.StringIO),
            self.assertRaises(SystemExit) as exit_context,
        ):
            nvx.parse_args(
                [
                    *base,
                    "--components",
                    "kernel",
                    "--keys",
                    "{}",
                    "--head-sha",
                    _REUSE_HEAD,
                    "--dry-run",
                ]
            )
        self.assertEqual(exit_context.exception.code, 2)
        args = nvx.parse_args(
            [
                *base[:-2],
                "--components",
                "kernel",
                "--keys",
                "{}",
                "--head-sha",
                _REUSE_HEAD,
                "--dry-run",
            ]
        )
        self.assertEqual(
            (args.commit, args.head_sha, args.dry_run), (None, _REUSE_HEAD, True)
        )

    def test_reports_escape_annotations_and_append_outputs(self):
        outcome = guest_artifact_reuse.ReuseOutcome(
            False,
            "Guest artifact reuse",
            "build",
            "100% of 2\nlines",
            "warning",
        )
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "output"
            output.write_text("before=1\n", encoding="utf-8")
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                guest_artifact_reuse.report(
                    outcome, {"GITHUB_OUTPUT": os.fspath(output)}
                )
            self.assertEqual(
                output.read_text(encoding="utf-8"), "before=1\nreused=false\n"
            )
        self.assertIn(
            "::warning title=Guest artifact reuse::100%25 of 2%0Alines\n",
            stdout.getvalue(),
        )
        self.assertEqual(
            outcome.summary(),
            "### Guest artifact reuse: build\n100% of 2\nlines\n",
        )

    def test_rest_api_reads_json_and_downloads_without_forwarding_the_token(self):
        authorizations: dict[str, str | None] = {}

        class StorageHandler(http.server.BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                authorizations["storage"] = self.headers.get("Authorization")
                body = b"archive"
                self.send_response(http.HTTPStatus.OK)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, format: str, *args: object) -> None:
                pass

        storage = http.server.ThreadingHTTPServer(("127.0.0.1", 0), StorageHandler)

        class ApiHandler(http.server.BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                authorizations[self.path] = self.headers.get("Authorization")
                if self.path == "/repos/o/r/runs":
                    body = b'{"workflow_runs": []}'
                    self.send_response(http.HTTPStatus.OK)
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                elif self.path == "/repos/o/r/zip":
                    self.send_response(http.HTTPStatus.FOUND)
                    self.send_header(
                        "Location",
                        f"http://127.0.0.1:{storage.server_address[1]}/blob",
                    )
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                else:
                    self.send_error(http.HTTPStatus.NOT_FOUND)

            def log_message(self, format: str, *args: object) -> None:
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), ApiHandler)
        threads = [
            threading.Thread(target=instance.serve_forever, daemon=True)
            for instance in (server, storage)
        ]
        for thread in threads:
            thread.start()
        try:
            url = f"http://127.0.0.1:{server.server_address[1]}"
            api = guest_artifact_reuse.RestApi("secret", time.monotonic() + 60, url=url)
            self.assertEqual(api.get("repos/o/r/runs"), {"workflow_runs": []})
            with tempfile.TemporaryDirectory() as temporary:
                archive = Path(temporary) / "artifact.zip"
                api.download("repos/o/r/zip", archive)
                self.assertEqual(archive.read_bytes(), b"archive")
            self.assertEqual(authorizations["/repos/o/r/runs"], "Bearer secret")
            self.assertEqual(authorizations["/repos/o/r/zip"], "Bearer secret")
            self.assertIsNone(authorizations["storage"])
            with self.assertRaisesRegex(
                guest_artifact_reuse.ReuseRefused,
                "GET repos/o/r/missing failed with HTTP 404",
            ):
                api.get("repos/o/r/missing")
            expired = guest_artifact_reuse.RestApi(
                "secret", time.monotonic() - 1, url=url
            )
            with self.assertRaisesRegex(
                guest_artifact_reuse.ReuseRefused, "ran out of time"
            ):
                expired.get("repos/o/r/runs")
        finally:
            for instance in (server, storage):
                instance.shutdown()
                instance.server_close()
            for thread in threads:
                thread.join()

    def test_rest_api_refuses_responses_cut_short(self):
        # A response that ends before its Content-Length, or within a chunk,
        # raises http.client.IncompleteRead, which is no OSError, so it would
        # fail the step instead of falling back to building.
        class TruncatingHandler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_GET(self) -> None:
                self.send_response(http.HTTPStatus.OK)
                if self.path == "/repos/o/r/runs":
                    self.send_header("Content-Length", "100")
                    self.end_headers()
                    self.wfile.write(b'{"workflow_runs": ')
                else:
                    self.send_header("Transfer-Encoding", "chunked")
                    self.end_headers()
                    self.wfile.write(b"64\r\n" + b"x" * 10)
                self.wfile.flush()
                self.close_connection = True

            def log_message(self, format: str, *args: object) -> None:
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), TruncatingHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            api = guest_artifact_reuse.RestApi(
                "secret",
                time.monotonic() + 60,
                url=f"http://127.0.0.1:{server.server_address[1]}",
            )
            with self.assertRaisesRegex(
                guest_artifact_reuse.ReuseRefused,
                "^GET repos/o/r/runs failed: IncompleteRead",
            ):
                api.get("repos/o/r/runs")
            with (
                tempfile.TemporaryDirectory() as temporary,
                self.assertRaisesRegex(
                    guest_artifact_reuse.ReuseRefused,
                    "^GET repos/o/r/zip failed: IncompleteRead",
                ),
            ):
                api.download("repos/o/r/zip", Path(temporary) / "artifact.zip")
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


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


class CiConfigurationTests(unittest.TestCase):
    def test_dependabot_covers_independent_dependencies_only(self):
        root = BuildConstants.REPO_ROOT
        configuration = yaml.safe_load(
            (root / ".github" / "dependabot.yml").read_text(encoding="utf-8")
        )
        entries = configuration["updates"]
        ecosystems = {entry["package-ecosystem"]: entry for entry in entries}
        self.assertEqual(len(entries), 3)
        self.assertEqual(set(ecosystems), {"github-actions", "pip", "cargo"})

        actions = ecosystems["github-actions"]
        directories = actions["directories"]
        self.assertIn("/", directories)
        covered_actions = {
            path.relative_to(root).as_posix()
            for directory in directories
            if directory != "/"
            for path in root.glob(directory.lstrip("/"))
            if path.is_dir()
        }
        expected_actions = {
            path.parent.relative_to(root).as_posix()
            for pattern in ("*/action.yml", "*/action.yaml")
            for path in (root / ".github" / "actions").glob(pattern)
        }
        self.assertTrue(expected_actions)
        self.assertEqual(covered_actions, expected_actions)
        self.assertEqual(
            set(ecosystems["pip"]["directories"]), {"/", "/scripts/setup/azure"}
        )
        cargo = ecosystems["cargo"]
        self.assertEqual(cargo["directory"], "/nvx")
        self.assertTrue((root / "nvx" / "Cargo.lock").is_file())
        self.assertEqual(cargo["versioning-strategy"], "lockfile-only")

    def test_dependabot_limits_routine_updates_without_grouping_major_updates(self):
        configuration = yaml.safe_load(
            (BuildConstants.REPO_ROOT / ".github" / "dependabot.yml").read_text(
                encoding="utf-8"
            )
        )
        entries = configuration["updates"]
        self.assertEqual(sum(entry["open-pull-requests-limit"] for entry in entries), 3)
        for entry in entries:
            with self.subTest(ecosystem=entry["package-ecosystem"]):
                self.assertEqual(entry["open-pull-requests-limit"], 1)
                self.assertEqual(entry["schedule"]["interval"], "weekly")
                self.assertEqual(entry["cooldown"], {"default-days": 7})
                self.assertNotIn("target-branch", entry)
                [group] = entry["groups"].values()
                self.assertEqual(group["applies-to"], "version-updates")
                self.assertEqual(group["patterns"], ["*"])
                self.assertEqual(set(group["update-types"]), {"minor", "patch"})

    def test_ci_needs_reference_declared_jobs(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        jobs = set(
            re.findall(
                r"^  ([A-Za-z0-9_-]+):$",
                workflow.split("jobs:\n", 1)[1],
                re.MULTILINE,
            )
        )
        for job_name in sorted(jobs):
            job = _workflow_job(workflow, job_name)
            match = re.search(r"^    needs:(.*)$", job, re.MULTILINE)
            if match is None:
                continue
            inline = match.group(1).strip()
            if inline:
                dependencies = {name.strip() for name in inline.strip("[]").split(",")}
            else:
                dependencies = set(
                    re.findall(
                        r"^      - ([A-Za-z0-9_-]+)$",
                        job.split("    if:", 1)[0],
                        re.MULTILINE,
                    )
                )
            with self.subTest(job=job_name):
                self.assertLessEqual(
                    dependencies,
                    jobs,
                    f"undefined job dependencies: {dependencies - jobs}",
                )
        required = _workflow_job(workflow, "required-status-check")
        self.assertIn("      - nvx\n", required)
        self.assertIn("${{ needs.nvx.result }}", required)

    def test_ci_jobs_download_exactly_the_guest_artifacts_that_they_read(self):
        # Each guest artifact component is its own workflow artifact. A job
        # that reads any file of a component must download that component and
        # wait for the job that uploads it, so no job lacks an artifact that it
        # boots, and a job downloads and waits for nothing else.
        github = BuildConstants.REPO_ROOT / ".github"

        def load(name: str) -> Any:
            return yaml.safe_load(
                (github / "workflows" / name).read_text(encoding="utf-8")
            )

        jobs: dict[str, dict[str, Any]] = load("ci.yml")["jobs"]
        scratch = UbuntuBuildConstants.SCRATCH_TEMPLATE_NAME
        components = {
            "guest-kernel": {
                KernelBuildConstants.BINARY_NAME,
                KernelBuildConstants.CONFIG_NAME,
                KernelBuildConstants.PROVENANCE_NAME,
            },
            "guest-alpine": {
                AlpineBuildConstants.INITRAMFS_NAME,
                AlpineBuildConstants.PACKAGE_MANIFEST_NAME,
                InitramfsBuildConstants.PROVENANCE_NAME,
            },
            "guest-ubuntu": {
                UbuntuBuildConstants.INITRAMFS_NAME,
                UbuntuBuildConstants.PACKAGE_MANIFEST_NAME,
            },
            # The sandbox scratch template is cached with the distro layer.
            "guest-ubuntu-distro": {
                UbuntuBuildConstants.DISTRO_NAME,
                UbuntuBuildConstants.DISTRO_MANIFEST_NAME,
                scratch,
            },
            "guest-azurelinux": {
                AzureLinuxBuildConstants.INITRAMFS_NAME,
                AzureLinuxBuildConstants.PACKAGE_MANIFEST_NAME,
            },
            "guest-debug-kernel": set(DockerBuildConstants.DEBUG_KERNEL_ARTIFACT_NAMES),
        }
        guest_files = set[str]().union(*components.values())
        # Every file that the guest builds produce is handed over.
        self.assertEqual(
            guest_files,
            {
                *DockerBuildConstants.ALL_GUEST_ARTIFACT_NAMES,
                *DockerBuildConstants.DEBUG_KERNEL_ARTIFACT_NAMES,
            },
        )

        producers: dict[str, str] = {}
        for job_name, job in jobs.items():
            for step in job.get("steps", []):
                arguments = step.get("with", {})
                if not str(step.get("uses", "")).startswith(
                    "actions/upload-artifact@"
                ) or not str(arguments.get("name")).startswith("guest-"):
                    continue
                name = arguments["name"]
                with self.subTest(upload=name):
                    self.assertNotIn(name, producers)
                    producers[name] = job_name
                    paths = str(arguments["path"]).split()
                    self.assertTrue(all(path.startswith("build/") for path in paths))
                    self.assertEqual(
                        {PurePosixPath(path).name for path in paths}, components[name]
                    )
                    self.assertEqual(arguments["if-no-files-found"], "error")
        self.assertEqual(set(producers), set(components))
        # Each guest artifact job builds exactly the components that it
        # uploads, and no component builds in two jobs.
        uploads = {
            "kernel": {"guest-kernel"},
            "alpine": {"guest-alpine"},
            "ubuntu": {"guest-ubuntu", "guest-ubuntu-distro"},
            "azurelinux": {"guest-azurelinux"},
            "debug-kernel": {"guest-debug-kernel"},
        }
        built: dict[str, str] = {}
        for job_name, job in jobs.items():
            for step in job.get("steps", []):
                if step.get("uses") != "./.github/actions/build-guest-artifacts":
                    continue
                selected = str(step.get("with", {}).get("components", "")).split()
                with self.subTest(producer=job_name):
                    self.assertTrue(selected)
                    for component in selected:
                        self.assertIn(component, uploads)
                        self.assertNotIn(component, built)
                        built[component] = job_name
                    self.assertEqual(
                        {
                            name
                            for name, producer in producers.items()
                            if producer == job_name
                        },
                        set[str]().union(*(uploads[name] for name in selected)),
                    )
        self.assertEqual(set(built), set(uploads))

        kernel_alpine = {
            KernelBuildConstants.BINARY_NAME,
            AlpineBuildConstants.INITRAMFS_NAME,
        }
        # The public sandbox scenarios run workloads from the Ubuntu layer
        # (managed_exec_tests.load_workload_artifacts).
        workload = {
            UbuntuBuildConstants.DISTRO_NAME,
            UbuntuBuildConstants.DISTRO_MANIFEST_NAME,
            scratch,
        }
        release_files = {
            *ReleaseBuildConstants.GUEST_ARTIFACT_NAMES,
            KernelBuildConstants.PROVENANCE_NAME,
            InitramfsBuildConstants.PROVENANCE_NAME,
        }
        actions = {
            "./.github/actions/checkout-openvmm": set[str](),
            "./.github/actions/setup-curl": set[str](),
            "./.github/actions/sccache": set[str](),
            # The doctor's preflight (H3) and guest warp probe (H6) boot its
            # default kernel and initramfs.
            "./.github/actions/validate-runner": kernel_alpine,
            "./.github/actions/run-benchmark": kernel_alpine,
            "./.github/actions/package-release": release_files,
        }
        # nvx.py commands, which boot the kernel and the Alpine initramfs
        # unless test-microvm selects another guest or the debug kernel.
        commands = {
            "setup-cross-os-cache": set[str](),
            "verify": set[str](),
            "test-openvmm": kernel_alpine,
            # A sandbox boots the Alpine control initramfs under its layers.
            "sandbox": kernel_alpine,
        }

        def reads(step: dict[str, Any], context: Mapping[str, object]) -> set[str]:
            uses = str(step.get("uses", ""))
            if uses.startswith("./"):
                self.assertIn(uses, actions)
                if (
                    uses == "./.github/actions/validate-runner"
                    and step.get("with", {}).get("verify-openvmm") != "true"
                ):
                    return set()
                return actions[uses]
            if uses:
                self.assertTrue(uses.startswith("actions/"), uses)
                return set()
            script = str(step["run"])
            files = {
                name
                for name in re.findall(r"\bbuild[/\\]([\w.-]+)", script)
                if name in guest_files
            }
            for command in re.findall(
                r"(?:nvx\.py|nvx[/\\]target[/\\]release[/\\]nvx(?:\.exe)?"
                r"|Invoke-Nvx)\s+([a-z][a-z-]*)",
                script,
            ):
                if command == "test-microvm":
                    guest = re.search(r"--guest (\S+)", script)
                    descriptor = guests.guest_descriptor(
                        guest.group(1) if guest else "alpine"
                    )
                    files.add(descriptor.initramfs_name)
                    if "inputs.debug-kernel && '--debug-kernel'" in script:
                        debug = context["inputs.debug-kernel"] is True
                    else:
                        debug = "--debug-kernel" in script
                    files |= (
                        {
                            KernelBuildConstants.DEBUG_BINARY_NAME,
                            KernelBuildConstants.DEBUG_CONFIG_NAME,
                        }
                        if debug
                        else {KernelBuildConstants.BINARY_NAME}
                    )
                    scenarios = set(re.findall(r"--scenario (\S+)", script))
                    if "inputs.shard && format('--shard" in script:
                        sharded = bool(context["inputs.shard"])
                    else:
                        sharded = "--shard" in script
                    # A default run of a guest that controls sandboxes runs
                    # the public sandbox scenarios too, but no shard does.
                    default_run = not (scenarios or debug or sharded)
                    if scenarios & PUBLIC_SANDBOX_SCENARIOS or (
                        default_run and descriptor.sandbox_control
                    ):
                        files |= workload
                else:
                    self.assertIn(command, commands)
                    files |= commands[command]
            return files

        def holds(condition: object, context: Mapping[str, object]) -> bool:
            # The conjunctions of atoms that the steps that read or download
            # guest artifacts use; anything else fails the test.
            for term in _expression_conjuncts(str(condition or "true")):
                if term == "true":
                    continue
                match = re.fullmatch(
                    r"(!?)\s*([\w.-]+)(?:\s*(==|!=)\s*'([^']*)')?", term
                )
                self.assertIsNotNone(match, term)
                assert match is not None
                negated, name, operator, literal = match.groups()
                value = context[name]
                result = (
                    bool(value)
                    if operator is None
                    else (str(value) == literal) == (operator == "==")
                )
                if result == bool(negated):
                    return False
            return True

        def consume(
            steps: list[dict[str, Any]], context: Mapping[str, object]
        ) -> tuple[set[str], set[str]] | None:
            # The guest artifacts that a job downloads and the guest files that
            # its steps read, or None if it downloads none; such a job reads
            # none from its build directory either.
            downloads: set[str] = set()
            for step in steps:
                arguments = step.get("with", {})
                if str(step.get("uses", "")).startswith(
                    "actions/download-artifact@"
                ) and str(arguments.get("name", arguments.get("pattern"))).startswith(
                    "guest-"
                ):
                    self.assertNotIn("pattern", arguments)
                    self.assertEqual(arguments["path"], "build")
                    if holds(step.get("if"), context):
                        downloads.add(arguments["name"])
            if not downloads:
                return None
            read: set[str] = set()
            for step in steps:
                if str(step.get("uses", "")).startswith("actions/download-artifact@"):
                    continue
                files = reads(step, context)
                if files and holds(step.get("if"), context):
                    read |= files
            return downloads, read

        consumers: set[str] = set()
        for job_name, job in jobs.items():
            callee = str(job.get("uses", ""))
            if callee.startswith("./.github/workflows/"):
                workflow = load(callee.removeprefix("./.github/workflows/"))
                triggers = workflow.get(True, workflow.get("on"))
                defaults = {
                    name: definition.get("default")
                    for name, definition in triggers["workflow_call"]["inputs"].items()
                }
                # Each leg of a matrix, such as each shard of a default
                # microVM suite, with the inputs that it passes.
                contexts: list[dict[str, object]] = []
                for leg in _matrix_legs(job):
                    inputs = {**defaults, **leg}
                    contexts.append(
                        {
                            "runner.os": (
                                "Windows"
                                if "windows" in json.loads(inputs["runner"])
                                else "Linux"
                            ),
                            **{
                                f"inputs.{name}": value
                                for name, value in inputs.items()
                            },
                        }
                    )
                steps = [
                    step
                    for definition in workflow["jobs"].values()
                    for step in definition["steps"]
                ]
            else:
                legs = job.get("strategy", {}).get("matrix", {}).get("include", [{}])
                contexts = [
                    {
                        "runner.os": (
                            "Windows"
                            if "windows" in json.loads(leg.get("runner", "[]"))
                            else "Linux"
                        ),
                        **{f"matrix.{name}": value for name, value in leg.items()},
                    }
                    for leg in legs
                ]
                steps = job.get("steps", [])
            for context in contexts:
                downloads = consume(steps, context)
                if downloads is None:
                    continue
                consumers.add(job_name)
                downloaded, read = downloads
                expected = {name for name, files in components.items() if files & read}
                with self.subTest(job=job_name, context=context):
                    self.assertEqual(downloaded, expected)
                    needs = job.get("needs", [])
                    needs = [needs] if isinstance(needs, str) else needs
                    self.assertEqual(
                        set(needs) & set(producers.values()),
                        {producers[name] for name in expected},
                    )
                    for producer in {producers[name] for name in expected}:
                        self.assertIn(
                            f"needs.{producer}.result == 'success'",
                            _expression_conjuncts(str(job["if"])),
                        )
        self.assertEqual(
            consumers,
            {
                *ci.REQUIRED_CI_MICROVM_TEST_JOBS,
                *ci.REQUIRED_CI_MICROVM_DEBUG_JOBS,
                *ci.REQUIRED_CI_MICROVM_DEBUG_PUSH_JOBS,
                *ci.REQUIRED_CI_PLATFORM_JOBS,
                *ci.REQUIRED_CI_PACKAGE_JOBS,
            },
        )
        # The microVM workflow's own default, an unsharded default run, which
        # no job of ci.yml selects, runs the public sandbox scenarios too, so
        # it downloads the Ubuntu distro layer that they read.
        microvm = load("run-nvx-microvm-tests.yml")
        microvm_inputs = microvm.get(True, microvm.get("on"))["workflow_call"]["inputs"]
        microvm_steps = [
            step
            for definition in microvm["jobs"].values()
            for step in definition["steps"]
        ]
        for system, backend in (("Linux", "kvm"), ("Windows", "whp")):
            context: dict[str, object] = {
                "runner.os": system,
                **{
                    f"inputs.{name}": definition.get("default", name)
                    for name, definition in microvm_inputs.items()
                },
                "inputs.backend": backend,
            }
            with self.subTest(unsharded=system):
                self.assertEqual(context["inputs.shard"], 0)
                result = consume(microvm_steps, context)
                assert result is not None
                downloaded, read = result
                self.assertEqual(
                    downloaded,
                    {name for name, files in components.items() if files & read},
                )
                self.assertIn("guest-ubuntu-distro", downloaded)

        # test-openvmm boots the kernel and the Alpine initramfs.
        requested: list[str] = []

        class Stop(Exception):
            pass

        def artifact(name: str) -> Path:
            requested.append(name)
            return Path(name)

        def present(path: Path, _description: str) -> Path:
            return path

        with (
            patch.object(ci, "validate_openvmm_test_backend"),
            patch.object(ci, "require_file", side_effect=present),
            patch.object(ci, "require_tool", return_value="tool"),
            patch.object(ci, "artifact_path", side_effect=artifact),
            patch.object(ci, "_prepare_openvmm_test_environment", side_effect=Stop),
            self.assertRaises(Stop),
        ):
            ci.run_openvmm_tests("kvm")
        self.assertEqual(set(requested), commands["test-openvmm"])

    def test_development_release_requires_successful_crate_checks(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        release_job = _workflow_job(workflow, "release")
        predicate = release_job.split("    if:", 1)[1].split("    runs-on:", 1)[0]
        self.assertIn("      - nvx\n", release_job)
        self.assertIn("needs.nvx.result == 'success' &&", predicate)
        self.assertNotIn("needs.nvx.result == 'skipped'", predicate)

    def test_crate_package_job_builds_the_package_without_publishing_it(self):
        # doc/ci.md, "crates.io releases": only a full run of dev packages the
        # nvx crate, after Required status check, which fails unless every
        # job of the full matrix succeeded. The job that builds the package,
        # and so runs build scripts and procedural macros, cannot publish.
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        job = yaml.safe_load(workflow)["jobs"]["crate-package"]
        self.assertEqual(job["needs"], ["required-status-check"])
        condition = " ".join(job["if"].split())
        for clause in (
            "${{ always() && !cancelled() &&",
            "( github.event_name == 'schedule' || "
            "github.event_name == 'workflow_dispatch' ) &&",
            "github.ref == 'refs/heads/dev' &&",
            "needs.required-status-check.result == 'success' }}",
        ):
            with self.subTest(clause=clause):
                self.assertIn(clause, condition)
        self.assertNotIn("push", condition)
        self.assertNotIn("pull_request", condition)
        self.assertEqual(job["runs-on"], "ubuntu-latest")
        self.assertEqual(job["permissions"], {"contents": "read"})
        self.assertNotIn("environment", job)
        self.assertEqual(
            job["outputs"],
            {
                output: f"${{{{ steps.crate.outputs.{output} }}}}"
                for output in ("publish", "version", "sha256", "url")
            },
        )
        steps = job["steps"]
        self.assertEqual(
            [step["name"] for step in steps],
            [
                "Checkout",
                "Install the pinned Rust release",
                "Package, check, and build the nvx crate",
            ],
        )
        checkout, toolchain, package = steps
        # The history holds the commits of the published versions.
        self.assertEqual(
            checkout["with"], {"fetch-depth": 0, "persist-credentials": False}
        )
        self.assertIn("rust-toolchain.toml", toolchain["run"])
        self.assertIn(
            '"RUSTUP_TOOLCHAIN=${toolchain}" >> "${GITHUB_ENV}"', toolchain["run"]
        )
        self.assertEqual(package["id"], "crate")
        self.assertIn("python3 -u scripts/publish_nvx_crate.py package", package["run"])

    def test_crate_release_job_publishes_the_package_without_building_it(self):
        # The publish job holds the right to publish, so it runs no build: it
        # packs the package job's bytes again, and only its publish step sees
        # the registry token.
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        job = yaml.safe_load(workflow)["jobs"]["crate-release"]
        self.assertEqual(job["needs"], ["crate-package"])
        condition = " ".join(job["if"].split())
        for clause in (
            "${{ always() && !cancelled() &&",
            "( github.event_name == 'schedule' || "
            "github.event_name == 'workflow_dispatch' ) &&",
            "github.ref == 'refs/heads/dev' &&",
            "needs.crate-package.result == 'success' &&",
            "needs.crate-package.outputs.publish == 'true' }}",
        ):
            with self.subTest(clause=clause):
                self.assertIn(clause, condition)
        self.assertEqual(job["runs-on"], "ubuntu-latest")
        self.assertEqual(job["permissions"], {"contents": "read", "id-token": "write"})
        self.assertEqual(
            job["environment"],
            {"name": "crates-io", "url": "${{ needs.crate-package.outputs.url }}"},
        )
        self.assertEqual(
            job["concurrency"], {"group": "crate-release", "cancel-in-progress": False}
        )
        steps = job["steps"]
        self.assertEqual(
            [step["name"] for step in steps],
            [
                "Checkout",
                "Install the pinned Rust release",
                "Check the nvx package",
                "Authenticate with crates.io",
                "Publish the nvx crate",
            ],
        )
        checkout, toolchain, check, authenticate, publish = steps
        self.assertEqual(
            checkout["with"], {"fetch-depth": 0, "persist-credentials": False}
        )
        self.assertIn("rust-toolchain.toml", toolchain["run"])
        self.assertEqual(check["id"], "check")
        # The package job's outputs reach the script as variables, never as
        # text that the shell would parse.
        self.assertEqual(
            check["env"],
            {
                "CRATE_VERSION": "${{ needs.crate-package.outputs.version }}",
                "CRATE_SHA256": "${{ needs.crate-package.outputs.sha256 }}",
            },
        )
        for argument in (
            "python3 -u scripts/publish_nvx_crate.py prepare",
            '--crate-version "${CRATE_VERSION}"',
            '--sha256 "${CRATE_SHA256}"',
        ):
            self.assertIn(argument, check["run"])
        self.assertRegex(
            authenticate["uses"], r"^rust-lang/crates-io-auth-action@[0-9a-f]{40}$"
        )
        self.assertEqual(authenticate["id"], "auth")
        self.assertIn("python3 -u scripts/publish_nvx_crate.py publish", publish["run"])
        for step in (authenticate, publish):
            self.assertEqual(step["if"], "steps.check.outputs.upload == 'true'")
        self.assertEqual(
            publish["env"], {"CARGO_REGISTRY_TOKEN": "${{ steps.auth.outputs.token }}"}
        )
        for step in steps:
            with self.subTest(step=step["name"]):
                self.assertNotIn("${{", step.get("run", ""))
                self.assertNotIn("cargo build", step.get("run", ""))
                self.assertNotIn(" package", step.get("run", ""))
                self.assertFalse(step.get("uses", "").startswith("./"))
                if step is not publish:
                    self.assertNotIn("CARGO_REGISTRY_TOKEN", step.get("env", {}))
        self.assertEqual(workflow.count("CARGO_REGISTRY_TOKEN"), 1)
        self.assertEqual(workflow.count("steps.auth.outputs.token"), 1)

    def test_only_the_crate_release_can_request_an_oidc_token(self):
        # crates.io trusts the OpenID Connect token of ci.yml's jobs in the
        # crates-io environment, so no other job may request one or deploy
        # to it.
        holders: set[str] = set()
        environments: set[str] = set()
        workflows = BuildConstants.REPO_ROOT / ".github" / "workflows"
        for path in sorted(workflows.glob("*.yml")):
            parsed = yaml.safe_load(path.read_text(encoding="utf-8"))
            scopes = {"(workflow)": parsed, **parsed.get("jobs", {})}
            for scope, definition in scopes.items():
                permissions = definition.get("permissions")
                if permissions == "write-all" or (
                    isinstance(permissions, dict) and "id-token" in permissions
                ):
                    holders.add(f"{path.name}:{scope}")
                if "environment" in definition:
                    environments.add(f"{path.name}:{scope}")
        self.assertEqual(holders, {"ci.yml:crate-release"})
        self.assertEqual(environments, {"ci.yml:crate-release"})

    def test_crate_checks_run_on_every_os(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        definition = _workflow_job(workflow, "nvx")
        self.assertIn("os: [ubuntu-latest, windows-latest]", definition)
        self.assertIn("uses: ./.github/actions/check-nvx", definition)
        self.assertEqual(ci.REQUIRED_CI_RESULT_ENVIRONMENTS["nvx"], "NVX_RESULT")

    def test_nvx_builds_and_checks_every_guest_binary_for_musl(self):
        crate = BuildConstants.REPO_ROOT / "nvx"
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "check-nvx"
            / "action.yml"
        ).read_text(encoding="utf-8")
        manifest = (crate / "Cargo.toml").read_text(encoding="utf-8")
        guest_binaries = set(
            re.findall(
                r'^\[\[bin\]\]\nname = "([^"]+)"\npath = "[^"]+"\n'
                r'required-features = \["guest"\]$',
                manifest,
                re.MULTILINE,
            )
        )
        self.assertIn("nvx-exit", guest_binaries)
        lint_and_test = "Lint and test the guest binaries"
        build_and_check = "Build and check the guest binaries"
        check = _composite_action_step(action, lint_and_test)
        build = _composite_action_step(action, build_and_check)
        for step in (check, build):
            self.assertIn("if: runner.os == 'Linux'", step)
            # nvx.py owns the Cargo and rustup commands, which developers reuse.
            self.assertNotIn("cargo ", step)
            self.assertNotIn("rustup", step)
        self.assertIn(
            "python3 scripts/nvx.py check-guest-binaries "
            '--toolchain "${{ steps.toolchains.outputs.toolchain }}"',
            " ".join(check.split()),
        )
        # The check adds the musl target that the build needs.
        self.assertLess(
            action.index(f"- name: {lint_and_test}"),
            action.index(f"- name: {build_and_check}"),
        )
        self.assertEqual(set(GuestRustBuildConstants.BINARIES), guest_binaries)
        for name in guest_binaries:
            module = name.removeprefix("nvx-").replace("-", "_")
            root = (crate / "src" / "bin" / f"{name}.rs").read_text(encoding="utf-8")
            with self.subTest(binary=name):
                # Every guest binary runs the main of its nvx::guest module.
                self.assertIn(f"nvx::guest::{module}::main()", root)
        self.assertIn(
            'python3 ../scripts/nvx.py build-guest-binaries --output "${directory}"',
            build,
        )
        self.assertIn(
            'python3 ../scripts/nvx.py test-guest-binaries --binaries "${directory}"',
            build,
        )

    def test_required_ci_result_policy(self):
        always_successful = {
            "quality",
            "nvx",
            "openvmm-changes",
        }
        # Jobs that succeed whenever the repository's jobs run.
        repository = (
            always_successful
            | set(ci.REQUIRED_CI_NVX_BUILD_JOBS)
            | set(ci.REQUIRED_CI_CLI_JOBS)
        )
        builds = set(ci.REQUIRED_CI_BUILD_JOBS)
        # The check of OpenVMM's CI needs no secret and no artifact, so it runs
        # whenever its flag is set, for a pull request from a fork too.
        openvmm_ci = {ci.REQUIRED_CI_OPENVMM_CI_JOB}
        microvm_tests = set(ci.REQUIRED_CI_MICROVM_TEST_JOBS)
        debug_kernel_pull_request = set(ci.REQUIRED_CI_MICROVM_DEBUG_JOBS)
        debug_kernel_merge_group = set(ci.REQUIRED_CI_MICROVM_DEBUG_MERGE_GROUP_JOBS)
        debug_kernel_push = set(ci.REQUIRED_CI_MICROVM_DEBUG_PUSH_JOBS)
        artifacts = {
            ci.REQUIRED_CI_ARTIFACT_JOB,
            ci.REQUIRED_CI_DISTRO_ARTIFACT_JOB,
            ci.REQUIRED_CI_DEBUG_KERNEL_JOB,
        }
        # Each platform's package job runs whenever its platform job does.
        platforms = set(ci.REQUIRED_CI_PLATFORM_JOBS) | set(ci.REQUIRED_CI_PACKAGE_JOBS)
        # A same-repository pull request that builds the guest artifacts also
        # rehearses their reuse.
        rehearsal = {ci.REQUIRED_CI_GUEST_ARTIFACT_REUSE_JOB}
        microvm_pull_request = microvm_tests | debug_kernel_pull_request
        microvm_merge_group = microvm_tests | debug_kernel_merge_group
        microvm_full = microvm_tests | debug_kernel_pull_request | debug_kernel_push
        full_matrix = (
            repository | artifacts | builds | openvmm_ci | microvm_full | platforms
        )
        gate = {"performance-gate"}
        # Event, same repository, the change detector's outputs (guest
        # artifacts, OpenVMM binaries, OpenVMM CI check, NVX microVM tests,
        # platform benchmarks and packages), and the jobs that must succeed;
        # every other required job must be skipped.
        everything = (True, True, True, True, True)
        cases = (
            # Documentation, or a change to no suite's inputs.
            ("pull_request", True, (False,) * 5, repository),
            # Only the benchmarks or the packages change.
            (
                "pull_request",
                True,
                (True, True, False, False, True),
                repository | artifacts | rehearsal | builds | platforms | gate,
            ),
            # Only the microVM tests change.
            (
                "pull_request",
                True,
                (True, True, False, True, False),
                repository | artifacts | rehearsal | builds | microvm_pull_request,
            ),
            # Only the OpenVMM CI check, which needs nothing built.
            (
                "pull_request",
                True,
                (False, False, True, False, False),
                repository | openvmm_ci,
            ),
            (
                "pull_request",
                True,
                everything,
                repository
                | artifacts
                | rehearsal
                | builds
                | openvmm_ci
                | microvm_pull_request
                | platforms
                | gate,
            ),
            # A pull request that changes neither the pin nor the check.
            (
                "pull_request",
                True,
                (True, True, False, True, True),
                repository
                | artifacts
                | rehearsal
                | builds
                | microvm_pull_request
                | platforms
                | gate,
            ),
            # A job runs only when the jobs that build its artifacts run, as
            # its condition in ci.yml says, whatever the flags claim.
            (
                "pull_request",
                True,
                (False, True, True, True, True),
                repository | builds | openvmm_ci,
            ),
            (
                "pull_request",
                True,
                (True, False, False, True, True),
                repository | artifacts | rehearsal,
            ),
            ("pull_request", False, (False,) * 5, always_successful),
            (
                "pull_request",
                False,
                (True, True, False, False, True),
                always_successful | artifacts,
            ),
            (
                "pull_request",
                False,
                (False, False, True, False, False),
                always_successful | openvmm_ci,
            ),
            (
                "pull_request",
                False,
                everything,
                always_successful | artifacts | openvmm_ci,
            ),
            # A merge group runs a same-repository pull request's jobs, whatever
            # repository its pull requests come from, but not the rehearsal of
            # the guest artifact reuse, which its pull request's run made.
            ("merge_group", True, (False,) * 5, repository),
            (
                "merge_group",
                True,
                (True, True, False, False, True),
                repository | artifacts | builds | platforms | gate,
            ),
            (
                "merge_group",
                True,
                (True, True, False, True, False),
                repository | artifacts | builds | microvm_merge_group,
            ),
            (
                "merge_group",
                True,
                (False, False, True, False, False),
                repository | openvmm_ci,
            ),
            (
                "merge_group",
                True,
                everything,
                repository
                | artifacts
                | builds
                | openvmm_ci
                | microvm_merge_group
                | platforms
                | gate,
            ),
            (
                "merge_group",
                False,
                everything,
                repository
                | artifacts
                | builds
                | openvmm_ci
                | microvm_merge_group
                | platforms
                | gate,
            ),
            ("push", True, (False,) * 5, repository),
            (
                "push",
                True,
                (True, True, False, False, True),
                repository | artifacts | builds | platforms,
            ),
            (
                "push",
                True,
                (True, True, False, True, True),
                repository | artifacts | builds | microvm_full | platforms,
            ),
            (
                "push",
                True,
                (False, False, True, False, False),
                repository | openvmm_ci,
            ),
            ("push", True, everything, full_matrix),
            ("schedule", False, everything, full_matrix),
            ("workflow_dispatch", False, everything, full_matrix),
        )
        for event_name, same_repository, flags, successful_jobs in cases:
            changes = ci.CiChanges(*flags)
            with self.subTest(
                event_name=event_name,
                same_repository=same_repository,
                changes=changes,
            ):
                expected = ci.required_ci_expected_results(
                    event_name,
                    same_repository=same_repository,
                    changes=changes,
                )
                self.assertEqual(set(expected), set(ci.REQUIRED_CI_RESULT_ENVIRONMENTS))
                self.assertEqual(
                    {job for job, result in expected.items() if result == "success"},
                    successful_jobs,
                )
                self.assertEqual(
                    ci.required_ci_failures(
                        event_name,
                        same_repository=same_repository,
                        changes=changes,
                        results=expected,
                    ),
                    [],
                )

                for job, expected_result in expected.items():
                    unexpected = expected.copy()
                    unexpected[job] = (
                        "skipped" if expected_result == "success" else "success"
                    )
                    with self.subTest(job=job, actual=unexpected[job]):
                        self.assertEqual(
                            ci.required_ci_failures(
                                event_name,
                                same_repository=same_repository,
                                changes=changes,
                                results=unexpected,
                            ),
                            [
                                f"{job}: expected {expected_result}, "
                                f"got {unexpected[job]}"
                            ],
                        )

                for job in expected:
                    failed = expected.copy()
                    failed[job] = "failure"
                    with self.subTest(job=job, actual="failure"):
                        self.assertEqual(
                            ci.required_ci_failures(
                                event_name,
                                same_repository=same_repository,
                                changes=changes,
                                results=failed,
                            ),
                            [f"{job}: expected {expected[job]}, got failure"],
                        )

    def test_required_ci_job_uses_tested_policy(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        job = _workflow_job(workflow, "required-status-check")

        self.assertIn("python3 scripts/nvx.py check-required-ci", job)
        self.assertIn('--same-repository "${SAME_REPOSITORY}"', job)
        self.assertIn(
            "github.event.pull_request.head.repo.full_name == github.repository",
            job,
        )
        self.assertLess(
            job.index("uses: actions/checkout@"),
            job.index("python3 scripts/nvx.py check-required-ci"),
        )
        self.assertIn("persist-credentials: false", job)
        # Each job that the policy checks reaches it as its own result.
        for job_name, environment in ci.REQUIRED_CI_RESULT_ENVIRONMENTS.items():
            with self.subTest(environment=environment):
                self.assertIn(f"      - {job_name}\n", job)
                self.assertIn(
                    f"          {environment}: ${{{{ needs.{job_name}.result }}}}\n",
                    job,
                )
        # Every suite flag reaches the policy as the detector reported it.
        detector = _workflow_job(workflow, "openvmm-changes")
        for output in ci.CI_CHANGE_OUTPUTS:
            environment = output.upper().replace("-", "_")
            with self.subTest(output=output):
                self.assertIn(
                    f"      {output}: ${{{{ steps.changes.outputs.{output} }}}}",
                    detector,
                )
                self.assertIn(
                    f"          {environment}: "
                    f"${{{{ needs.openvmm-changes.outputs.{output} }}}}",
                    job,
                )
                self.assertIn(f'--{output} "${{{environment}}}"', job)

    def test_ci_jobs_follow_their_change_detector_output(self):
        # Each job that the change detector selects runs when its output is
        # true, or when the detector did not succeed, so that a failed
        # classification runs the full matrix. Required status check passes
        # every output to check-required-ci, the release and the performance
        # persistence read whether the OpenVMM CI check runs, and no other job
        # reads one.
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        selections = {
            ci.REQUIRED_CI_ARTIFACT_JOB: "run-guest-artifacts",
            ci.REQUIRED_CI_DISTRO_ARTIFACT_JOB: "run-guest-artifacts",
            ci.REQUIRED_CI_DEBUG_KERNEL_JOB: "run-guest-artifacts",
            ci.REQUIRED_CI_GUEST_ARTIFACT_REUSE_JOB: "run-guest-artifacts",
            **dict.fromkeys(ci.REQUIRED_CI_BUILD_JOBS, "run-openvmm-binaries"),
            ci.REQUIRED_CI_OPENVMM_CI_JOB: "run-openvmm-ci-check",
            **dict.fromkeys(
                (
                    *ci.REQUIRED_CI_MICROVM_TEST_JOBS,
                    *ci.REQUIRED_CI_MICROVM_DEBUG_JOBS,
                    *ci.REQUIRED_CI_MICROVM_DEBUG_PUSH_JOBS,
                ),
                "run-microvm-tests",
            ),
            **dict.fromkeys(
                (*ci.REQUIRED_CI_PLATFORM_JOBS, *ci.REQUIRED_CI_PACKAGE_JOBS),
                "run-benchmarks",
            ),
        }
        self.assertEqual(set(selections.values()), set(ci.CI_CHANGE_OUTPUTS))
        for job_name, output in selections.items():
            with self.subTest(job=job_name):
                job = _workflow_job(workflow, job_name)
                needs = re.search(r"^    needs: \[(.+)\]$", job, re.MULTILINE)
                assert needs is not None
                self.assertIn("openvmm-changes", needs.group(1).split(", "))
                self.assertIn(
                    "        (\n"
                    "          needs.openvmm-changes.result != 'success' ||\n"
                    f"          needs.openvmm-changes.outputs.{output} == 'true'\n"
                    "        )\n",
                    job,
                )
                self.assertEqual(
                    re.findall(r"needs\.openvmm-changes\.outputs\.([a-z-]+)", job),
                    [output],
                )
        # The release and the recording of the performance results accept a
        # skipped OpenVMM CI check only when the change selected none (#690).
        gates = ("release", "performance-record")
        for job_name in gates:
            with self.subTest(job=job_name):
                self.assertEqual(
                    re.findall(
                        r"needs\.openvmm-changes\.outputs\.([a-z-]+)",
                        _workflow_job(workflow, job_name),
                    ),
                    ["run-openvmm-ci-check"],
                )
        jobs = re.findall(
            r"^  ([A-Za-z0-9_-]+):$", workflow.split("\njobs:\n", 1)[1], re.MULTILINE
        )
        self.assertEqual(
            {
                job_name
                for job_name in jobs
                if "needs.openvmm-changes.outputs." in _workflow_job(workflow, job_name)
            },
            set(selections) | {"required-status-check", *gates},
        )
        # The detector checks and forwards exactly the classifier's outputs.
        script = _workflow_step(workflow, "openvmm-changes", "Detect relevant changes")
        listed = re.search(r"\n +outputs=\(([^)]*)\)\n", script)
        assert listed is not None
        self.assertEqual(tuple(listed.group(1).split()), ci.CI_CHANGE_OUTPUTS)

    def test_ci_full_run_triggers_and_concurrency_are_separate(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        header = workflow.split("\njobs:\n", 1)[0]

        self.assertIn('  schedule:\n    - cron: "17 5 * * *"\n', header)
        self.assertIn("  workflow_dispatch:\n\nconcurrency:\n", header)
        self.assertIn(
            "(github.event_name == 'schedule' || "
            "github.event_name == 'workflow_dispatch') && "
            "format('full-{0}', github.run_id) || "
            "github.event_name == 'merge_group' && "
            "format('merge-group-{0}', github.run_id) || "
            "github.event.pull_request.number || github.ref",
            header,
        )
        self.assertIn(
            "cancel-in-progress: ${{ github.event_name == 'pull_request' }}",
            header,
        )

    def test_ci_runs_pull_requests_of_any_base(self):
        # A pull request stacked on another's branch gets its own checks, and
        # keeps them when GitHub retargets it to dev. GitHub reports a retarget
        # as an `edited` event, like each edit of a title or a body, so a run
        # for it would cancel the run in progress or replace its result with a
        # placeholder (doc/ci.md#stacked-pull-requests).
        workflow = yaml.safe_load(
            (BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text(
                encoding="utf-8"
            )
        )
        # PyYAML reads the `on` key as true.
        self.assertEqual(
            workflow[True]["pull_request"],
            {"types": ["opened", "synchronize", "reopened", "ready_for_review"]},
        )

    def test_ci_runs_the_merge_groups_of_dev(self):
        # The merge queue waits for the required check of each of its groups,
        # which only a run of this workflow for the group reports
        # (doc/ci.md#merge-queue).
        text = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        workflow = yaml.safe_load(text)
        # PyYAML reads the `on` key as true.
        triggers = workflow[True]
        self.assertEqual(
            triggers["merge_group"],
            {"types": ["checks_requested"], "branches": ["dev"]},
        )
        self.assertEqual(triggers["push"]["branches"], ["dev", "feature/nvx-refactor"])

        # The detector compares a merge group's head with the commit on which
        # the queue built it: dev, or the group of the pull request ahead.
        detector = next(
            step
            for step in workflow["jobs"]["openvmm-changes"]["steps"]
            if step.get("id") == "changes"
        )
        base, head, before, parent, tip = (
            "1" * 40,
            "2" * 40,
            "3" * 40,
            "4" * 40,
            "5" * 40,
        )
        group = f"refs/heads/gh-readonly-queue/dev/pr-7-{parent}"
        events = {
            "pull_request": (
                {
                    "sha": tip,
                    "event": {
                        "pull_request": {"base": {"sha": base}, "head": {"sha": head}}
                    },
                },
                base,
                head,
            ),
            "merge_group": (
                {
                    "sha": tip,
                    "ref": group,
                    "event": {
                        "merge_group": {
                            "base_sha": parent,
                            "head_sha": tip,
                            "head_ref": group,
                            "base_ref": "refs/heads/dev",
                        }
                    },
                },
                parent,
                tip,
            ),
            "push": ({"sha": tip, "event": {"before": before}}, before, tip),
        }
        for event_name, (github, expected_base, expected_head) in events.items():
            with self.subTest(event_name=event_name):
                context = {"github": {"event_name": event_name, **github}}
                for variable, expected in (
                    ("BASE_SHA", expected_base),
                    ("HEAD_SHA", expected_head),
                ):
                    self.assertEqual(
                        _evaluate_expression(detector["env"][variable], context, {}),
                        expected,
                    )

        # Each group's run has a concurrency group of its own, which neither
        # waits for nor cancels another run. A pull request that returns to
        # the queue on the same base gets its earlier group's ref again, whose
        # run must not hold the new group's.
        concurrency = workflow["concurrency"]

        def format_expression(template: str, *values: object) -> str:
            return template.format(*values)

        functions: dict[str, Callable[..., Any]] = {"format": format_expression}

        def settings(github: Mapping[str, Any], run_id: int) -> tuple[Any, ...]:
            context: dict[str, Any] = {
                "github": {"workflow": "CI", "run_id": run_id, **github}
            }
            return tuple(
                _interpolate_expressions(str(concurrency[key]), context, functions)
                for key in ("group", "cancel-in-progress", "queue")
            )

        def merge_group(ref: str, head: str) -> dict[str, Any]:
            return {
                "event_name": "merge_group",
                "ref": ref,
                "event": {"merge_group": {"head_ref": ref, "head_sha": head}},
            }

        first = settings(merge_group(group, tip), 101)
        # The returning group's run, on the same ref with another head, and a
        # second run of one group, which shares its ref and its head.
        returned = settings(merge_group(group, before), 102)
        repeated = settings(merge_group(group, tip), 103)
        push = settings({"event_name": "push", "ref": "refs/heads/dev"}, 104)
        # A push to feature/nvx-refactor queues as dev's do, in a group of its
        # own (doc/ci.md#feature-branch-pushes).
        feature_push = settings(
            {"event_name": "push", "ref": "refs/heads/feature/nvx-refactor"}, 106
        )
        pull_request = settings(
            {
                "event_name": "pull_request",
                "ref": "refs/pull/7/merge",
                "event": {"pull_request": {"number": 7}},
            },
            105,
        )
        self.assertEqual(first, ("ci-CI-merge-group-101", False, "max"))
        self.assertEqual(push, ("ci-CI-refs/heads/dev", False, "max"))
        self.assertEqual(
            feature_push, ("ci-CI-refs/heads/feature/nvx-refactor", False, "max")
        )
        self.assertEqual(pull_request, ("ci-CI-7", True, "single"))
        self.assertEqual(
            len(
                {
                    first[0],
                    returned[0],
                    repeated[0],
                    push[0],
                    feature_push[0],
                    pull_request[0],
                }
            ),
            6,
        )

    def test_ci_job_conditions_follow_the_required_result_policy(self):
        # Each job's condition, evaluated for each event and each selection of
        # the change detector in a run whose jobs succeed when they run, runs
        # exactly the jobs that check-required-ci expects, so that a condition
        # that a merge group, which no pull request's run exercises, would
        # take wrongly fails here rather than in the merge queue.
        workflow = yaml.safe_load(
            (BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text(
                encoding="utf-8"
            )
        )
        repository = "microsoft/nvx"

        def pull_request(full_name: str, base: str = "dev") -> dict[str, Any]:
            return {
                "ref": "refs/pull/7/merge",
                "base_ref": base,
                "event": {
                    "pull_request": {
                        "number": 7,
                        "draft": False,
                        "base": {"ref": base},
                        "head": {"repo": {"full_name": full_name}},
                    }
                },
            }

        # A pull request stacked on another's branch runs as one to dev does.
        scenarios: tuple[tuple[str, bool, dict[str, Any]], ...] = (
            ("pull_request", True, pull_request(repository)),
            ("pull_request", False, pull_request("contributor/nvx")),
            ("pull_request", True, pull_request(repository, "feature")),
            ("pull_request", False, pull_request("contributor/nvx", "feature")),
            (
                "merge_group",
                True,
                {
                    "ref": f"refs/heads/gh-readonly-queue/dev/pr-7-{'1' * 40}",
                    "event": {"merge_group": {"base_ref": "refs/heads/dev"}},
                },
            ),
            ("push", True, {"ref": "refs/heads/dev", "event": {}}),
            # A push to feature/nvx-refactor runs as a dev push does, but
            # publishes and records nothing (doc/ci.md#feature-branch-pushes).
            ("push", True, {"ref": "refs/heads/feature/nvx-refactor", "event": {}}),
            ("schedule", True, {"ref": "refs/heads/dev", "event": {}}),
            ("workflow_dispatch", True, {"ref": "refs/heads/dev", "event": {}}),
        )
        for event_name, same_repository, github in scenarios:
            selections = (
                itertools.product((False, True), repeat=len(ci.CI_CHANGE_OUTPUTS))
                if event_name in ci.CI_CLASSIFIED_EVENTS
                else [(True,) * len(ci.CI_CHANGE_OUTPUTS)]
            )
            context = {"event_name": event_name, "repository": repository, **github}
            for flags in selections:
                changes = ci.CiChanges(*flags)
                outputs = {
                    "openvmm-changes": {
                        output: str(selected).lower()
                        for output, selected in changes.outputs().items()
                    }
                }
                with self.subTest(
                    event_name=event_name,
                    same_repository=same_repository,
                    changes=changes,
                ):
                    results = _simulate_workflow_jobs(workflow, context, outputs)
                    expected = ci.required_ci_expected_results(
                        event_name, same_repository=same_repository, changes=changes
                    )
                    self.assertEqual({job: results[job] for job in expected}, expected)
                    self.assertEqual(results["required-status-check"], "success")
                    # Only a dev push publishes its release and records its
                    # performance results, and only once its platform jobs
                    # ran; only a full run publishes the crate and persists the
                    # results that the pushes recorded.
                    published = (
                        "success"
                        if event_name == "push"
                        and github["ref"] == "refs/heads/dev"
                        and expected["platform-kvm"] == "success"
                        else "skipped"
                    )
                    self.assertEqual(results["release"], published)
                    self.assertEqual(results["performance-record"], published)
                    self.assertEqual(
                        results["performance-persist"],
                        "success" if event_name in ci.CI_FULL_RUN_EVENTS else "skipped",
                    )
                    self.assertEqual(
                        results["crate-package"],
                        "success" if event_name in ci.CI_FULL_RUN_EVENTS else "skipped",
                    )
                    # Only a merge group cancels the runs of superseded groups.
                    self.assertEqual(
                        results["cancel-superseded-merge-groups"],
                        "success" if event_name == "merge_group" else "skipped",
                    )

    def test_ci_dev_pushes_queue_while_pull_requests_cancel(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        header = workflow.split("\njobs:\n", 1)[0]
        concurrency = header.split("\nconcurrency:\n", 1)[1].split("\n\n", 1)[0]

        # GitHub rejects `queue: max` with `cancel-in-progress: true`, so a
        # static queue mode would stop every pull-request run from starting.
        self.assertIn(
            "\n  cancel-in-progress: ${{ github.event_name == 'pull_request' }}\n",
            concurrency,
        )
        self.assertIn(
            "\n  queue: ${{ github.event_name == 'pull_request' && "
            "'single' || 'max' }}",
            concurrency,
        )
        self.assertNotRegex(concurrency, r"(?m)^\s*queue:\s*['\"]?max")

    def test_ci_full_runs_short_circuit_change_detection(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        step = _workflow_step(workflow, "openvmm-changes", "Detect relevant changes")
        full_run = (
            'if [[ "${EVENT_NAME}" == "schedule" ||\n'
            '                "${EVENT_NAME}" == "workflow_dispatch" ]]; then'
        )

        self.assertIn(full_run, step)
        self.assertIn(
            'run_everything "The ${EVENT_NAME} event runs the full matrix '
            'without change classification."',
            step,
        )
        self.assertLess(
            step.index(full_run),
            step.index('if [[ -z "${BASE_SHA}" || "${BASE_SHA}" =~ ^0+$ ]]'),
        )
        self.assertLess(
            step.index(full_run),
            step.index("python3 scripts/nvx.py classify-ci-changes"),
        )

    def test_ci_repository_jobs_run_for_every_non_pull_request_event(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        repository_gate = (
            "github.event_name != 'pull_request' ||\n"
            "          github.event.pull_request.head.repo.full_name "
            "== github.repository"
        )
        gated_jobs = (
            *ci.REQUIRED_CI_NVX_BUILD_JOBS,
            *ci.REQUIRED_CI_CLI_JOBS,
            *ci.REQUIRED_CI_BUILD_JOBS,
            *ci.REQUIRED_CI_MICROVM_TEST_JOBS,
            *ci.REQUIRED_CI_MICROVM_DEBUG_JOBS,
            *ci.REQUIRED_CI_PLATFORM_JOBS,
            *ci.REQUIRED_CI_PACKAGE_JOBS,
        )
        for job_name in gated_jobs:
            with self.subTest(job=job_name):
                self.assertIn(repository_gate, _workflow_job(workflow, job_name))
        # The check of OpenVMM's CI needs no secret, so a pull request from a
        # fork runs it too.
        self.assertNotIn(
            repository_gate, _workflow_job(workflow, ci.REQUIRED_CI_OPENVMM_CI_JOB)
        )
        self.assertNotIn("github.event_name == 'push' ||", workflow)
        for job_name in ci.REQUIRED_CI_MICROVM_DEBUG_PUSH_JOBS:
            with self.subTest(job=job_name):
                job = _workflow_job(workflow, job_name)
                self.assertIn("github.event_name != 'pull_request' &&", job)
                if job_name in ci.REQUIRED_CI_MICROVM_DEBUG_MERGE_GROUP_JOBS:
                    self.assertNotIn("github.event_name != 'merge_group' &&", job)
                else:
                    self.assertIn("github.event_name != 'merge_group' &&", job)
                self.assertNotIn("github.event_name == 'push'", job)

    def test_ci_full_runs_do_not_publish_record_or_save_caches(self):
        github = BuildConstants.REPO_ROOT / ".github"
        workflow = (github / "workflows" / "ci.yml").read_text(encoding="utf-8")
        push_to_dev = (
            "github.event_name == 'push' &&\n        github.ref == 'refs/heads/dev' &&"
        )
        for job_name in ("release", "performance-record"):
            with self.subTest(job=job_name):
                self.assertIn(push_to_dev, _workflow_job(workflow, job_name))
        # A full run of dev persists the results that the dev pushes recorded,
        # whatever its own jobs do, and never its own results.
        persist = yaml.safe_load(workflow)["jobs"]["performance-persist"]
        self.assertEqual(
            _expression_conjuncts(persist["if"]),
            [
                "( github.event_name == 'schedule' || "
                "github.event_name == 'workflow_dispatch' )",
                "github.ref == 'refs/heads/dev'",
            ],
        )
        self.assertNotIn("needs", persist)
        gate = yaml.safe_load(workflow)["jobs"]["performance-gate"]
        self.assertEqual(
            _expression_conjuncts(gate["if"])[:2],
            [
                "( github.event_name == 'pull_request' || "
                "github.event_name == 'merge_group' )",
                "( github.event_name != 'pull_request' || "
                "github.event.pull_request.draft == false )",
            ],
        )

        # Every cache write in ci.yml, and in the actions and reusable
        # workflows that it reaches, runs only for dev pushes or only while a
        # save-cache input is true, which only pushes and pull requests set.
        # setup-python saves its cache whenever it misses, so its cache input
        # follows save-cache too.
        save_cache = (
            "${{ github.event_name == 'pull_request' || github.event_name == 'push' }}"
        )
        writes: dict[str, int] = {}
        save_cache_actions: set[str] = set()
        calls: list[tuple[str, str, dict[str, object]]] = []
        visited: set[str] = set()
        pending = ["workflows/ci.yml"]
        while pending:
            source = pending.pop()
            if source in visited:
                continue
            visited.add(source)
            parsed = yaml.safe_load((github / source).read_text(encoding="utf-8"))
            if "save-cache" in parsed.get("inputs", {}):
                save_cache_actions.add(source)
            if "runs" in parsed:
                entries = list(parsed["runs"]["steps"])
            else:
                entries = [
                    entry
                    for job in parsed["jobs"].values()
                    for entry in ([job] if "uses" in job else job["steps"])
                ]
            for entry in entries:
                uses = str(entry.get("uses", ""))
                arguments = entry.get("with", {})
                name = f"{source}: {entry.get('name', uses)}"
                if uses.startswith("./.github/"):
                    callee = uses.removeprefix("./.github/")
                    if not callee.endswith(".yml"):
                        callee = f"{callee}/action.yml"
                    calls.append((name, callee, arguments))
                    pending.append(callee)
                    continue
                with self.subTest(step=name):
                    self.assertFalse(
                        uses.startswith("actions/cache@"),
                        "actions/cache saves after every miss",
                    )
                    if uses.startswith("actions/cache/save@"):
                        conjuncts = _expression_conjuncts(str(entry.get("if", "")))
                        for conjunct in conjuncts:
                            if "||" in conjunct:
                                self.assertTrue(
                                    _outer_parentheses_enclose(conjunct), conjunct
                                )
                        self.assertTrue(
                            "inputs.save-cache == 'true'" in conjuncts
                            or {
                                "github.event_name == 'push'",
                                "github.ref == 'refs/heads/dev'",
                            }
                            <= set(conjuncts),
                            conjuncts,
                        )
                    elif uses.startswith("actions/setup-") and "cache" in arguments:
                        self.assertRegex(
                            str(arguments["cache"]),
                            r"^\$\{\{ inputs\.save-cache == 'true' && '[a-z]+' "
                            r"\|\| '' \}\}$",
                        )
                    else:
                        # Only cache writes follow save-cache, so builds still
                        # run when an artifact misses the cache.
                        self.assertNotIn("save-cache", str(entry))
                        continue
                writes[source] = writes.get(source, 0) + 1

        self.assertEqual(
            writes,
            {
                "actions/build-openvmm/action.yml": 6,
                "actions/build-guest-artifacts/action.yml": 6,
                "actions/check-quality/action.yml": 1,
            },
        )
        self.assertEqual(
            save_cache_actions,
            {
                "actions/build-guest-artifacts/action.yml",
                "actions/check-quality/action.yml",
            },
        )
        save_cache_calls = [
            (name, arguments)
            for name, callee, arguments in calls
            if callee in save_cache_actions
        ]
        self.assertEqual(len(save_cache_calls), 4)
        for name, arguments in save_cache_calls:
            with self.subTest(call=name):
                self.assertEqual(arguments.get("save-cache"), save_cache)
        # No other push-only job condition is allowed in this workflow: only
        # the release, performance persistence, the four save-cache arguments,
        # and the two reuse arguments name the push event; ci.yml itself writes
        # no cache.
        self.assertEqual(workflow.count("github.event_name == 'push'"), 8)

    def test_ci_change_classification_selects_suites_by_input(self):
        guest, binaries, openvmm_ci, microvm, benchmarks = ci.CI_CHANGE_OUTPUTS

        def select(*outputs: str) -> ci.CiChanges:
            return ci.CiChanges(*(output in outputs for output in ci.CI_CHANGE_OUTPUTS))

        everything = select(*ci.CI_CHANGE_OUTPUTS)
        microvm_and_benchmarks = select(guest, binaries, microvm, benchmarks)
        microvm_only = select(guest, binaries, microvm)
        benchmarks_only = select(guest, binaries, benchmarks)
        openvmm_ci_only = select(openvmm_ci)
        openvmm_ci_and_benchmarks = select(guest, binaries, openvmm_ci, benchmarks)
        # Each class of changed path, the outputs that a pull request selects,
        # and those that a push selects when they differ: a push runs the
        # platform and package jobs, and the jobs that build their artifacts,
        # for every
        # change but documentation.
        cases: tuple[tuple[tuple[str, ...], ci.CiChanges, ci.CiChanges | None], ...] = (
            # An empty diff runs the platform and package jobs, as it ran every
            # workload.
            ((), benchmarks_only, None),
            (("doc/ci.md", "README.md", "DOC/Guide.MD"), select(), None),
            # Documentation selects no suite, even in a suite's directory.
            (("scripts/setup/README.md",), select(), None),
            (("nvx/README.md",), select(), None),
            (
                ("scripts/setup/README.md", "scripts/test_nvx_tools.py"),
                select(),
                benchmarks_only,
            ),
            # Inputs of every suite and of the OpenVMM CI check: the OpenVMM
            # pin, the workflow, the CLI, and the modules that the check runs.
            (("openvmm",), everything, None),
            ((".github/workflows/ci.yml",), everything, None),
            (("scripts/nvx.py",), everything, None),
            (("scripts/nvx_tools/__init__.py",), everything, None),
            (("scripts/nvx_tools/build_constants.py",), everything, None),
            (("scripts/nvx_tools/common.py",), everything, None),
            # Inputs of every suite but the check, which builds nothing: the
            # Rust pin, the runners, the actions that the suites' jobs use, and
            # the CLI modules that they run.
            (("rust-toolchain.toml",), microvm_and_benchmarks, None),
            (("scripts/setup/setup-windows-whp.ps1",), microvm_and_benchmarks, None),
            (("scripts/setup/azure/build_package.py",), microvm_and_benchmarks, None),
            (
                (".github/actions/validate-runner/action.yml",),
                microvm_and_benchmarks,
                None,
            ),
            (
                (".github/actions/checkout-openvmm/action.yml",),
                microvm_and_benchmarks,
                None,
            ),
            ((".github/actions/sccache/action.yml",), microvm_and_benchmarks, None),
            (
                (".github/actions/setup-curl/curl-shim.rs",),
                microvm_and_benchmarks,
                None,
            ),
            (("scripts/nvx_tools/ci.py",), microvm_and_benchmarks, None),
            # Inputs of the guests, which the microVM tests and the platform
            # jobs boot.
            ((".dockerignore",), microvm_and_benchmarks, None),
            (("docker/Dockerfile",), microvm_and_benchmarks, None),
            (("SOURCE-MANIFEST.json",), microvm_and_benchmarks, None),
            (("scripts/nvx_tools/build.py",), microvm_and_benchmarks, None),
            (("scripts/nvx_tools/guests.py",), microvm_and_benchmarks, None),
            (("kernel/config-microvm",), microvm_and_benchmarks, None),
            (("kernel/patches/0001-example.patch",), microvm_and_benchmarks, None),
            (("guest/common/init",), microvm_and_benchmarks, None),
            (
                (".github/actions/build-guest-artifacts/action.yml",),
                microvm_and_benchmarks,
                None,
            ),
            (("guest/ubuntu/nvx-bashrc",), microvm_and_benchmarks, None),
            (("ubuntu/packages.lock.json",), microvm_and_benchmarks, None),
            (("azurelinux/packages.lock.json",), microvm_and_benchmarks, None),
            # Inputs of the OpenVMM binaries, and the modules and attributes
            # that the microVM tests and the platform and package jobs take.
            (
                (".github/workflows/build-openvmm-binary.yml",),
                microvm_and_benchmarks,
                None,
            ),
            (
                (".github/actions/build-openvmm/action.yml",),
                microvm_and_benchmarks,
                None,
            ),
            (("scripts/nvx_tools/benchmark.py",), microvm_and_benchmarks, None),
            (
                ("scripts/nvx_tools/benchmark_scripts/guest-exit.sh",),
                microvm_and_benchmarks,
                None,
            ),
            (("scripts/nvx_tools/doctor.py",), microvm_and_benchmarks, None),
            (("scripts/nvx_tools/host_time_probe.rs",), microvm_and_benchmarks, None),
            (
                ("scripts/nvx_tools/process_supervisor.py",),
                microvm_and_benchmarks,
                None,
            ),
            (("scripts/nvx_tools/release.py",), microvm_and_benchmarks, None),
            ((".gitattributes",), microvm_and_benchmarks, None),
            # Every nvx change: the microVM tests run the nvx CLI, every
            # package ships it, and the guest images carry its guest binaries.
            (("nvx/src/lib.rs",), microvm_and_benchmarks, None),
            (("nvx/Cargo.lock",), microvm_and_benchmarks, None),
            (("nvx/src/guest/exit.rs",), microvm_and_benchmarks, None),
            (("nvx/src/bin/nvx-time.rs",), microvm_and_benchmarks, None),
            (("nvx/src/host/cli/args.rs",), microvm_and_benchmarks, None),
            (("nvx/src/bin/nvx.rs",), microvm_and_benchmarks, None),
            (("nvx/tests/cli.rs",), microvm_and_benchmarks, None),
            # Inputs of the microVM tests alone, such as the planner of their
            # shards (#691).
            (
                ("scripts/nvx_tools/microvm_tests.py",),
                microvm_only,
                microvm_and_benchmarks,
            ),
            (
                ("scripts/nvx_tools/microvm_durations.py",),
                microvm_only,
                microvm_and_benchmarks,
            ),
            (
                ("scripts/nvx_tools/microvm_shards.py",),
                microvm_only,
                microvm_and_benchmarks,
            ),
            (
                ("scripts/nvx_tools/microvm_shards.json",),
                microvm_only,
                microvm_and_benchmarks,
            ),
            (
                ("scripts/nvx_tools/shard_plans.py",),
                microvm_only,
                microvm_and_benchmarks,
            ),
            (
                ("scripts/nvx_tools/openvmm_process.py",),
                microvm_only,
                microvm_and_benchmarks,
            ),
            (
                ("scripts/nvx_tools/microvm_test_scripts/lifecycle.sh",),
                microvm_only,
                microvm_and_benchmarks,
            ),
            (
                (".github/workflows/run-nvx-microvm-tests.yml",),
                microvm_only,
                microvm_and_benchmarks,
            ),
            # Inputs of the OpenVMM CI check alone: the module that checks, and
            # the GitHub client that it shares with the nightly report.
            (
                ("scripts/nvx_tools/openvmm_ci.py",),
                openvmm_ci_only,
                openvmm_ci_and_benchmarks,
            ),
            (
                ("scripts/nvx_tools/nightly_ci.py",),
                openvmm_ci_only,
                openvmm_ci_and_benchmarks,
            ),
            # Inputs of the platform jobs, package jobs, and regression gate alone.
            (("scripts/nvx_tools/performance.py",), benchmarks_only, None),
            ((".github/workflows/run-platform.yml",), benchmarks_only, None),
            ((".github/workflows/package-platform.yml",), benchmarks_only, None),
            ((".github/actions/run-benchmark/action.yml",), benchmarks_only, None),
            ((".github/actions/package-release/action.yml",), benchmarks_only, None),
            ((".github/actions/gate-performance/action.yml",), benchmarks_only, None),
            (
                (".github/actions/prepare-performance-results/action.yml",),
                benchmarks_only,
                None,
            ),
            (("data/linux-kvm-virtual-machine.csv",), benchmarks_only, None),
            (("data/windows-whp-virtual-machine-host.json",), benchmarks_only, None),
            (("VERSION",), benchmarks_only, None),
            (("LICENSE",), benchmarks_only, None),
            # Inputs of jobs that every change runs, or that only a push runs,
            # after the platform and package jobs, or only a full run.
            ((".github/actions/validate-nvx/action.yml",), select(), benchmarks_only),
            ((".github/actions/check-quality/action.yml",), select(), benchmarks_only),
            (
                (".github/actions/persist-performance-results/action.yml",),
                select(),
                benchmarks_only,
            ),
            (
                (".github/actions/publish-development-release/action.yml",),
                select(),
                benchmarks_only,
            ),
            (("scripts/publish_development_release.py",), select(), benchmarks_only),
            (("scripts/test_nvx_tools.py",), select(), benchmarks_only),
            (("scripts/nvx_tools/adversarial.py",), select(), benchmarks_only),
            (
                ("scripts/nvx_tools/adversarial_cases/guest-isolation.json",),
                select(),
                benchmarks_only,
            ),
            (("scripts/test_nightly_ci.py",), select(), benchmarks_only),
            ((".github/workflows/nightly-ci-report.yml",), select(), benchmarks_only),
            # Only the merge groups' GitHub-hosted job that cancels the runs
            # of superseded groups runs the merge queue module.
            (("scripts/nvx_tools/merge_queue.py",), select(), benchmarks_only),
            # Only the full runs' job that persists the performance results
            # that the dev pushes recorded runs the performance history module.
            (("scripts/nvx_tools/performance_history.py",), select(), benchmarks_only),
            (("scripts/nvx_tools/crate_release.py",), select(), benchmarks_only),
            (("scripts/publish_nvx_crate.py",), select(), benchmarks_only),
            (("scripts/test_crate_release.py",), select(), benchmarks_only),
            (("requirements-dev.txt",), select(), benchmarks_only),
            ((".github/workflows/copilot-setup-steps.yml",), select(), benchmarks_only),
            # A change selects the union of its paths' suites.
            (
                ("scripts/nvx_tools/microvm_tests.py", "kernel/config-microvm"),
                microvm_and_benchmarks,
                None,
            ),
            (
                (
                    "scripts/nvx_tools/openvmm_ci.py",
                    "scripts/nvx_tools/microvm_tests.py",
                ),
                select(guest, binaries, openvmm_ci, microvm),
                everything,
            ),
            (
                (
                    "scripts/nvx_tools/microvm_tests.py",
                    "scripts/nvx_tools/performance.py",
                ),
                microvm_and_benchmarks,
                None,
            ),
        )
        for changed_files, pull_request, push in cases:
            with self.subTest(changed_files=changed_files):
                self.assertEqual(
                    ci.classify_ci_changes("pull_request", changed_files),
                    pull_request,
                )
                # A merge group checks its pull request's change as the
                # pull request's run did, on the latest dev.
                self.assertEqual(
                    ci.classify_ci_changes("merge_group", changed_files),
                    pull_request,
                )
                self.assertEqual(
                    ci.classify_ci_changes("push", changed_files),
                    push or pull_request,
                )
        self.assertEqual(
            ci.classify_ci_changes("pull_request", ["openvmm", ""]),
            ci.classify_ci_changes("pull_request", ["openvmm"]),
        )
        for event_name in ci.CI_FULL_RUN_EVENTS:
            with (
                self.subTest(event_name=event_name),
                self.assertRaisesRegex(ValueError, "unsupported CI event"),
            ):
                ci.classify_ci_changes(event_name, ["openvmm"])

    def test_ci_change_classification_builds_artifacts_for_the_jobs_that_use_them(self):
        # The guest artifacts and the OpenVMM binaries have no use of their
        # own: each is built exactly when a job that uses it runs, so no job
        # waits for an artifact that its run skips.
        paths = (
            "openvmm",
            "doc/ci.md",
            "kernel/config-microvm",
            "guest/ubuntu/nvx-bashrc",
            "nvx/src/host/mod.rs",
            "scripts/nvx_tools/ci.py",
            "scripts/nvx_tools/microvm_tests.py",
            "scripts/nvx_tools/performance.py",
            "scripts/test_nvx_tools.py",
            ".github/actions/validate-nvx/action.yml",
        )
        for count in range(len(paths) + 1):
            for changed_files in itertools.combinations(paths, count):
                for event_name in ci.CI_CLASSIFIED_EVENTS:
                    changes = ci.classify_ci_changes(event_name, changed_files)
                    with self.subTest(event_name=event_name, files=changed_files):
                        self.assertEqual(
                            changes.run_guest_artifacts,
                            changes.run_microvm_tests or changes.run_benchmarks,
                        )
                        self.assertEqual(
                            changes.run_openvmm_binaries,
                            changes.run_microvm_tests or changes.run_benchmarks,
                        )

    def test_ci_change_classification_covers_each_suite_input(self):
        github = BuildConstants.REPO_ROOT / ".github"
        tools = BuildConstants.REPO_ROOT / "scripts" / "nvx_tools"
        workflow = (github / "workflows" / "ci.yml").read_text(encoding="utf-8")

        def local_actions(configuration: str) -> set[str]:
            names = set(
                re.findall(r"uses: \./\.github/actions/([a-z0-9-]+)", configuration)
            )
            pending = list(names)
            while pending:
                action = (github / "actions" / pending.pop() / "action.yml").read_text(
                    encoding="utf-8"
                )
                for name in local_actions(action) - names:
                    names.add(name)
                    pending.append(name)
            return names

        def imported_modules(module: str) -> set[str]:
            modules = {module}
            pending = [module]
            while pending:
                tree = ast.parse(
                    (tools / f"{pending.pop()}.py").read_text(encoding="utf-8")
                )
                for node in ast.walk(tree):
                    if not isinstance(node, ast.ImportFrom) or node.level != 1:
                        continue
                    names = (
                        [node.module.split(".")[0]]
                        if node.module
                        else [alias.name for alias in node.names]
                    )
                    for name in set(names) - modules:
                        modules.add(name)
                        pending.append(name)
            return modules

        # Every cache key input of every guest that the microVM tests and the
        # platform jobs boot.
        guest_action = (
            github / "actions" / "build-guest-artifacts" / "action.yml"
        ).read_text(encoding="utf-8")
        guest_inputs = {
            match.group(1): {
                path.replace("**", "example").replace("*", "example")
                for path in re.findall(r"'([^']+)'", match.group(2))
                if not path.startswith("!")
            }
            for match in re.finditer(
                r"^\s+([A-Z_]+)_INPUT_HASH: \$\{\{ hashFiles\((.+)\) \}\}$",
                guest_action,
                re.MULTILINE,
            )
        }
        self.assertEqual(
            set(guest_inputs),
            {"KERNEL", "DEBUG_KERNEL", "ALPINE", "UBUNTU", "AZURELINUX"},
        )
        # The Alpine key also ends with the source identity of the Rust guest
        # binaries, whose every file is an input of the image too.
        self.assertIn(
            'echo "alpine=alpine-image-v2-${ALPINE_VERSION}-${ALPINE_INPUT_HASH}'
            '-${rust_source_identity}"',
            guest_action,
        )
        guest_binary_inputs = {
            path.relative_to(BuildConstants.REPO_ROOT).as_posix()
            for path in guest_binaries.guest_binary_source_files()
        }

        # The NVX microVM tests and the platform and package jobs take every
        # input of the
        # jobs that they run and of the jobs that build the artifacts that they
        # use: those jobs' workflows, the reusable workflows and actions that
        # they reach, the files that the cache keys there hash, the files of the
        # guest binaries' source identity, with which every guest image key
        # ends, the modules of the nvx.py commands that they run, with every
        # module that those import and every file that those read beside them,
        # and what every self-hosted job takes: the OpenVMM pin, the Rust pin
        # that validate-runner checks, the runner setup scripts, and the
        # attributes that check out their scripts on Windows.
        command_modules = _nvx_command_modules()

        def module_files(module: str) -> set[str]:
            source = (tools / f"{module}.py").read_text(encoding="utf-8")
            files = {f"scripts/nvx_tools/{module}.py"}
            for name in re.findall(r'Path\(__file__\)\.with_name\("([^"]+)"\)', source):
                data = tools / name
                self.assertTrue(data.exists(), data)
                files.add(
                    f"scripts/nvx_tools/{name}{'/example' if data.is_dir() else ''}"
                )
            return files

        closures: dict[str, set[str]] = {}

        def command_files(command: str) -> set[str]:
            files: set[str] = set()
            for module in command_modules[command]:
                if module not in closures:
                    closures[module] = set[str]().union(
                        *map(module_files, imported_modules(module))
                    )
                files |= closures[module]
            return files

        def job_inputs(
            *jobs: str, tests_only: frozenset[str] = frozenset()
        ) -> set[str]:
            configurations: list[str] = []
            inputs = {
                "scripts/nvx.py",
                "scripts/nvx_tools/__init__.py",
                ".github/workflows/ci.yml",
            }
            for job in jobs:
                definition = _workflow_job(workflow, job)
                configurations.append(definition)
                for reusable in re.findall(
                    r"uses: \./\.github/workflows/([a-z0-9-]+\.yml)", definition
                ):
                    inputs.add(f".github/workflows/{reusable}")
                    configurations.append(
                        (github / "workflows" / reusable).read_text(encoding="utf-8")
                    )
            # Actions whose effect on the job's outputs is none, such as the
            # CLI tests, contribute neither their files nor their commands.
            actions = set[str]().union(*map(local_actions, configurations))
            for name in actions - tests_only:
                inputs.add(f".github/actions/{name}/action.yml")
                configurations.append(
                    (github / "actions" / name / "action.yml").read_text(
                        encoding="utf-8"
                    )
                )
                if name == "checkout-openvmm":
                    inputs.add("openvmm")
                # Every self-hosted job validates its runner first.
                if name == "validate-runner":
                    inputs |= {
                        "rust-toolchain.toml",
                        "scripts/setup/setup-linux.sh",
                        ".gitattributes",
                    }
            commands: set[str] = set()
            for configuration in configurations:
                for expression in re.findall(r"hashFiles\(([^)]*)\)", configuration):
                    inputs |= {
                        path.replace("**", "example").replace("*", "example")
                        for path in re.findall(r"'([^']+)'", expression)
                        if not path.startswith("!")
                    }
                # A PowerShell wrapper passes its commands on as @args, and
                # every step that uses one repeats its commands on Linux.
                commands.update(
                    re.findall(
                        r"scripts[/\\]nvx\.py[ \t]+([a-z][a-z0-9-]*)", configuration
                    )
                )
            return inputs.union(*map(command_files, commands))

        # Every job that runs nvx or ships it takes the executable that the
        # nvx build jobs build from the nvx crate with the pinned Rust
        # release, and its provenance.
        nvx_inputs = job_inputs(*ci.REQUIRED_CI_NVX_BUILD_JOBS) | {
            "nvx/example",
            "rust-toolchain.toml",
        }
        for job in ci.REQUIRED_CI_NVX_BUILD_JOBS:
            with self.subTest(job=job):
                definition = _workflow_job(workflow, job)
                self.assertIn("rust-toolchain.toml", definition)
                self.assertIn("--manifest-path nvx/Cargo.toml", definition)
        all_guest_inputs = (
            set[str]().union(*guest_inputs.values()) | guest_binary_inputs
        )
        microvm_inputs = (
            job_inputs(
                *ci.REQUIRED_CI_MICROVM_TEST_JOBS,
                *ci.REQUIRED_CI_MICROVM_DEBUG_JOBS,
                *ci.REQUIRED_CI_MICROVM_DEBUG_PUSH_JOBS,
                ci.REQUIRED_CI_ARTIFACT_JOB,
                ci.REQUIRED_CI_DISTRO_ARTIFACT_JOB,
                ci.REQUIRED_CI_DEBUG_KERNEL_JOB,
                *ci.REQUIRED_CI_BUILD_JOBS,
            )
            | nvx_inputs
            | all_guest_inputs
        )
        # The platform jobs check their history's expected host in data/, the
        # package jobs name their packages with VERSION and ship LICENSE, and
        # the regression gate reads the platform jobs' results. Pushes alone
        # publish and persist them, after every change but documentation.
        run_benchmark = (github / "actions" / "run-benchmark" / "action.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("--history-dir data", run_benchmark)
        self.assertIn(
            "cat VERSION",
            (github / "actions" / "package-release" / "action.yml").read_text(
                encoding="utf-8"
            ),
        )
        self.assertIn(
            '("LICENSE", "README.md", "THIRD_PARTY_NOTICES.md")',
            (tools / "release.py").read_text(encoding="utf-8"),
        )
        benchmark_inputs = (
            job_inputs(
                *ci.REQUIRED_CI_PLATFORM_JOBS,
                *ci.REQUIRED_CI_PACKAGE_JOBS,
                "performance-gate",
                ci.REQUIRED_CI_ARTIFACT_JOB,
                ci.REQUIRED_CI_DISTRO_ARTIFACT_JOB,
                *ci.REQUIRED_CI_BUILD_JOBS,
            )
            | nvx_inputs
            | all_guest_inputs
            | {"data/example", "VERSION", "LICENSE"}
        )
        self.assertLessEqual(
            {
                "scripts/nvx_tools/microvm_tests.py",
                "scripts/nvx_tools/microvm_durations.py",
                "scripts/nvx_tools/microvm_test_scripts/example",
                "scripts/nvx_tools/microvm_shards.py",
                "scripts/nvx_tools/microvm_shards.json",
                "scripts/nvx_tools/doctor.py",
                "scripts/nvx_tools/host_time_probe.rs",
                "scripts/nvx_tools/benchmark_scripts/example",
                "scripts/nvx_tools/process_supervisor.py",
                "scripts/nvx_tools/release.py",
                ".github/actions/build-openvmm/action.yml",
                ".github/workflows/build-openvmm-binary.yml",
                ".github/workflows/run-nvx-microvm-tests.yml",
            },
            microvm_inputs,
        )
        self.assertLessEqual(
            {
                "scripts/nvx_tools/performance.py",
                "scripts/nvx_tools/benchmark_scripts/example",
                ".github/actions/gate-performance/action.yml",
                ".github/actions/package-release/action.yml",
                ".github/workflows/package-platform.yml",
                ".github/workflows/run-platform.yml",
            },
            benchmark_inputs,
        )
        for inputs in (microvm_inputs, benchmark_inputs, nvx_inputs):
            self.assertNotIn(".github/actions/validate-nvx/action.yml", inputs)
        self.assertNotIn("scripts/nvx_tools/performance.py", microvm_inputs)
        for path in (
            "scripts/nvx_tools/microvm_durations.py",
            "scripts/nvx_tools/microvm_shards.py",
            "scripts/nvx_tools/microvm_shards.json",
            "scripts/nvx_tools/microvm_tests.py",
            "scripts/nvx_tools/shard_plans.py",
        ):
            self.assertNotIn(path, benchmark_inputs)
        for inputs in (microvm_inputs, benchmark_inputs):
            self.assertNotIn("scripts/nvx_tools/nightly_ci.py", inputs)
            self.assertNotIn("scripts/nvx_tools/crate_release.py", inputs)
            self.assertNotIn("scripts/nvx_tools/merge_queue.py", inputs)
            self.assertNotIn("scripts/nvx_tools/performance_history.py", inputs)
            self.assertNotIn("scripts/nvx_tools/openvmm_ci.py", inputs)
        # The OpenVMM CI check reads the pin from the checkout's index, and
        # its command's modules read the public OpenVMM repository's runs.
        openvmm_ci_inputs = job_inputs(ci.REQUIRED_CI_OPENVMM_CI_JOB) | {"openvmm"}
        self.assertLessEqual(
            {
                "scripts/nvx_tools/openvmm_ci.py",
                "scripts/nvx_tools/nightly_ci.py",
                "scripts/nvx_tools/common.py",
            },
            openvmm_ci_inputs,
        )
        for pattern, inputs in (
            (ci.OPENVMM_CI_CHECK_INPUT_PATHS, openvmm_ci_inputs),
            (ci.MICROVM_TEST_INPUT_PATHS, microvm_inputs),
            (ci.BENCHMARK_INPUT_PATHS, benchmark_inputs),
        ):
            for path in sorted(inputs):
                with self.subTest(pattern=pattern.pattern, path=path):
                    self.assertIsNotNone(pattern.match(path))

    def test_only_guest_binary_inputs_rebuild_the_guest_images(self):
        # The guest images carry only the Rust guest binaries of nvx, so a
        # change to a file that their build reads rebuilds every guest image,
        # and no other nvx change does. The NVX microVM tests run the nvx
        # CLI, and every package ships it, so every nvx change runs them and
        # the platform and package jobs, and so the jobs that build their
        # artifacts, which restore unchanged guest images from the cache.
        root = BuildConstants.REPO_ROOT
        identity = {
            path.relative_to(root).as_posix()
            for path in guest_binaries.guest_binary_source_files()
        }
        crate: set[str] = set()
        for directory, names, files in os.walk(root / "nvx"):
            if Path(directory) == root / "nvx":
                names[:] = [name for name in names if name != "target"]
            crate.update(
                (Path(directory) / name).relative_to(root).as_posix() for name in files
            )
        self.assertLessEqual({"nvx/src/host/mod.rs", "nvx/src/lib.rs"}, crate)
        for path in sorted(crate | identity):
            with self.subTest(path=path):
                self.assertIsNotNone(ci.MICROVM_TEST_INPUT_PATHS.match(path))
                self.assertIsNotNone(ci.BENCHMARK_INPUT_PATHS.match(path))
                if path in crate:
                    self.assertIsNone(ci.OPENVMM_CI_CHECK_INPUT_PATHS.match(path))
                self.assertEqual(
                    GuestRustBuildConstants.SOURCE_PATTERN.fullmatch(path) is not None,
                    path in identity,
                )
        # A new module counts unless it belongs to the host side or the tests,
        # and a file beside an input doesn't.
        for path, selected in (
            ("nvx/src/added.rs", True),
            ("nvx/src/guest/added/mod.rs", True),
            ("nvx/.cargo/config.toml", True),
            ("nvx/src/host/added.rs", False),
            ("nvx/src/protocol/added/tests.rs", False),
            ("nvx/Cargo.toml.orig", False),
            ("nvx/src/bin/nvx-time.rs.orig", False),
        ):
            with self.subTest(path=path):
                self.assertIsNotNone(ci.MICROVM_TEST_INPUT_PATHS.match(path))
                self.assertIsNotNone(ci.BENCHMARK_INPUT_PATHS.match(path))
                self.assertEqual(
                    GuestRustBuildConstants.SOURCE_PATTERN.fullmatch(path) is not None,
                    selected,
                )

        # The guest image keys hash no nvx file and end with the identity.
        action = (
            root / ".github" / "actions" / "build-guest-artifacts" / "action.yml"
        ).read_text(encoding="utf-8")
        image_patterns = [
            re.compile(
                "".join(
                    ".*"
                    if part == "**"
                    else "[^/]*"
                    if part == "*"
                    else re.escape(part)
                    for part in re.split(r"(\*\*|\*)", pattern)
                )
            )
            for match in re.finditer(
                r"^\s+(ALPINE|UBUNTU|AZURELINUX)_INPUT_HASH: "
                r"\$\{\{ hashFiles\((.+)\) \}\}$",
                action,
                re.MULTILINE,
            )
            for pattern in re.findall(r"'([^']+)'", match.group(2))
        ]
        self.assertTrue(image_patterns)
        for path in sorted(crate):
            with self.subTest(hashed=path):
                self.assertFalse(
                    any(pattern.fullmatch(path) for pattern in image_patterns)
                )
        for key in ("alpine", "ubuntu", "ubuntu-distro", "azurelinux"):
            with self.subTest(key=key):
                line = next(
                    line.strip()
                    for line in action.splitlines()
                    if line.strip().startswith(f'echo "{key}=')
                )
                self.assertTrue(line.endswith('-${rust_source_identity}"'))

        host_side = (
            "nvx/src/host/cli/args.rs",
            "nvx/src/host/mod.rs",
            "nvx/src/testing/fake_openvmm.rs",
            "nvx/src/bin/nvx.rs",
            "nvx/src/bin/nvx-fake-openvmm.rs",
            "nvx/src/protocol/nvxs/tests/host.rs",
            "nvx/src/guest/managed_agent/tests/loopback.rs",
            "nvx/tests/guest_tools.rs",
        )
        guest_side = (
            "nvx/Cargo.toml",
            "nvx/Cargo.lock",
            "nvx/src/lib.rs",
            "nvx/src/protocol/nvxs/host.rs",
            "nvx/src/guest/time/checks.rs",
            "nvx/src/bin/nvx-managed-agent.rs",
        )
        with tempfile.TemporaryDirectory() as temporary:
            # A checkout that holds only the identity's files and the changed
            # ones has the identity of this one.
            checkout = Path(temporary)
            for path in (*identity, *host_side):
                destination = checkout / path
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(root / path, destination)
            original = guest_binaries.guest_binary_source_sha256(checkout)
            self.assertEqual(original, guest_binaries.guest_binary_source_sha256())
            for paths, guest in (
                *(((path,), False) for path in host_side),
                (host_side, False),
                *(((path,), True) for path in guest_side),
                (("nvx/src/host/mod.rs", "nvx/src/guest/mod.rs"), True),
            ):
                for event_name in ("pull_request", "push"):
                    with self.subTest(changed=paths, event_name=event_name):
                        changes = ci.classify_ci_changes(event_name, paths)
                        self.assertEqual(
                            changes,
                            ci.CiChanges(True, True, False, True, True),
                        )
                        expected = ci.required_ci_expected_results(
                            event_name, same_repository=True, changes=changes
                        )
                        for job in (
                            ci.REQUIRED_CI_ARTIFACT_JOB,
                            ci.REQUIRED_CI_DISTRO_ARTIFACT_JOB,
                            *ci.REQUIRED_CI_BUILD_JOBS,
                            *ci.REQUIRED_CI_MICROVM_TEST_JOBS,
                            *ci.REQUIRED_CI_PLATFORM_JOBS,
                            *ci.REQUIRED_CI_PACKAGE_JOBS,
                        ):
                            self.assertEqual(expected[job], "success", job)
                        self.assertEqual(
                            expected[ci.REQUIRED_CI_OPENVMM_CI_JOB], "skipped"
                        )
                with self.subTest(changed=paths):
                    contents = {path: (checkout / path).read_bytes() for path in paths}
                    for path, data in contents.items():
                        (checkout / path).write_bytes(data + b"\n// changed\n")
                    self.assertEqual(
                        guest_binaries.guest_binary_source_sha256(checkout) != original,
                        guest,
                    )
                    for path, data in contents.items():
                        (checkout / path).write_bytes(data)

    def test_guest_cache_keys_hash_the_docker_build_context_filter(self):
        # Docker excludes files by .dockerignore before COPY, so a change to that
        # file changes the guest build even when no other input changes.
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        )
        expressions = re.findall(
            r"hashFiles\(([^)]*)\)", action.read_text(encoding="utf-8")
        )
        hashed = [
            expression
            for expression in expressions
            if "docker/Dockerfile" in expression
        ]
        self.assertTrue(hashed)
        for expression in hashed:
            with self.subTest(expression=expression[:60]):
                self.assertIn("'.dockerignore'", expression)

    def test_ci_change_summary_explains_each_decision(self):
        changed_files = [
            "scripts/nvx_tools/microvm_tests.py",
            "guest/common/init",
            "guest/common/README.md",
            "guest/common/nvx-hostmount",
            "guest/common/nvx-snapshot",
            "guest/common/nvx-identity-probe",
        ]
        changes = ci.classify_ci_changes("pull_request", changed_files)
        summary = ci.describe_ci_changes("pull_request", changed_files, changes)
        self.assertEqual(
            re.findall(r"^### (.+): (?:run|skip)$", summary, re.MULTILINE),
            [
                "Guest artifacts",
                "OpenVMM binaries",
                "OpenVMM CI check",
                "NVX microVM tests",
                "Platform benchmarks and packages",
            ],
        )
        self.assertIn(
            "### Guest artifacts: run\nJobs that use them run: the NVX microVM "
            "tests and the platform benchmarks and packages.\n",
            summary,
        )
        self.assertIn(
            "### OpenVMM binaries: run\nJobs that use them run: the NVX microVM "
            "tests and the platform benchmarks and packages.\n",
            summary,
        )
        self.assertIn("### OpenVMM CI check: skip\nNo input changed.\n", summary)
        self.assertIn(
            "### NVX microVM tests: run\nInputs changed: "
            "`scripts/nvx_tools/microvm_tests.py`, `guest/common/init`, "
            "`guest/common/nvx-hostmount`, and 2 more.\n",
            summary,
        )
        pin = ci.describe_ci_changes(
            "pull_request",
            ["openvmm"],
            ci.classify_ci_changes("pull_request", ["openvmm"]),
        )
        self.assertIn("### OpenVMM CI check: run\nInputs changed: `openvmm`.\n", pin)
        push = ci.describe_ci_changes(
            "push", changed_files, ci.classify_ci_changes("push", changed_files)
        )
        self.assertEqual(push, summary)

        def describe(event_name: str, files: list[str]) -> str:
            return ci.describe_ci_changes(
                event_name, files, ci.classify_ci_changes(event_name, files)
            )

        tests_only = describe("pull_request", ["scripts/test_nvx_tools.py"])
        self.assertEqual(
            describe("merge_group", ["scripts/test_nvx_tools.py"]), tests_only
        )
        self.assertIn(
            "### Guest artifacts: skip\nNo job that uses them runs.\n", tests_only
        )
        self.assertIn(
            "### Platform benchmarks and packages: skip\nNo input changed.\n",
            tests_only,
        )
        push = describe("push", ["scripts/test_nvx_tools.py"])
        self.assertIn(
            "### OpenVMM binaries: run\nJobs that use them run: the platform "
            "benchmarks and packages.\n",
            push,
        )
        self.assertIn(
            "### Platform benchmarks and packages: run\nA push runs the platform "
            "and package jobs for every change but documentation, so that a "
            "`dev` push publishes its development "
            "release and records its performance results.\n",
            push,
        )
        self.assertIn("### NVX microVM tests: skip\nNo input changed.\n", push)
        documentation = describe("pull_request", ["doc/ci.md"])
        self.assertEqual(describe("push", ["doc/ci.md"]), documentation)
        self.assertIn(
            "### Guest artifacts: skip\nNo job that uses them runs.\n", documentation
        )
        self.assertIn(
            "### OpenVMM CI check: skip\nOnly documentation files changed.\n",
            documentation,
        )
        self.assertIn(
            "### Platform benchmarks and packages: skip\n"
            "Only documentation files changed.\n",
            documentation,
        )
        empty = describe("pull_request", [])
        self.assertIn("### NVX microVM tests: skip\nNo file changed.\n", empty)
        self.assertIn(
            "### Platform benchmarks and packages: run\nNo file changed, so the "
            "platform and package jobs run.\n",
            empty,
        )

    def test_classify_ci_changes_command_prints_every_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            changed_files = Path(temporary) / "changed-files"
            changed_files.write_text(
                "scripts/nvx_tools/microvm_tests.py\n", encoding="utf-8"
            )
            summary = Path(temporary) / "summary.md"
            summary.write_text("before\n", encoding="utf-8")
            args = nvx.parse_args(
                [
                    "classify-ci-changes",
                    "--event-name",
                    "pull_request",
                    "--changed-files",
                    os.fspath(changed_files),
                    "--summary",
                    os.fspath(summary),
                ]
            )
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                args.handler(args)
            self.assertEqual(
                stdout.getvalue().splitlines(),
                [
                    "run-guest-artifacts=true",
                    "run-openvmm-binaries=true",
                    "run-openvmm-ci-check=false",
                    "run-microvm-tests=true",
                    "run-benchmarks=false",
                ],
            )
            self.assertEqual(
                [line.split("=", 1)[0] for line in stdout.getvalue().splitlines()],
                list(ci.CI_CHANGE_OUTPUTS),
            )
            text = summary.read_text(encoding="utf-8")
            self.assertTrue(text.startswith("before\n### "))
            self.assertIn("### NVX microVM tests: run\n", text)
            self.assertIn("### Platform benchmarks and packages: skip\n", text)

    def test_classify_ci_changes_command_reads_null_delimited_paths(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            changed_files = root / "changed-files"
            changed_files.write_bytes(
                "doc/notes.md\0kernel/patches/é\r\n.patch\0".encode()
            )
            args = nvx.parse_args(
                [
                    "classify-ci-changes",
                    "--event-name",
                    "pull_request",
                    "--changed-files",
                    os.fspath(changed_files),
                    "--null",
                    "--summary",
                    os.fspath(root / "summary"),
                ]
            )
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                args.handler(args)
            self.assertEqual(
                stdout.getvalue().splitlines(),
                [
                    "run-guest-artifacts=true",
                    "run-openvmm-binaries=true",
                    "run-openvmm-ci-check=false",
                    "run-microvm-tests=true",
                    "run-benchmarks=true",
                ],
            )
            summary = (root / "summary").read_bytes().decode("utf-8")
            self.assertIn("kernel/patches/é\r\n.patch", summary)

    @unittest.skipUnless(os.name == "posix", "requires bash and POSIX filenames")
    def test_ci_change_detector_classifies_real_git_paths(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        step = _workflow_step(workflow, "openvmm-changes", "Detect relevant changes")
        lines = step.splitlines()
        start = lines.index("        run: |") + 1
        script = "\n".join(line[10:] for line in lines[start:])
        classifier = BuildConstants.REPO_ROOT / "scripts" / "nvx.py"
        shim = (
            f"python3() {{ {shlex.quote(sys.executable)} "
            f'{shlex.quote(os.fspath(classifier))} "${{@:2}}"; }}\n'
        )
        cases = (
            ("rename", "doc/input.patch", True),
            ("delete", None, True),
            ("unicode", "kernel/patches/é.patch", True),
            ("newline", "kernel/patches/line\nbreak.patch", True),
            ("documentation", "doc/notes.md", False),
        )
        for name, destination, test_input in cases:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)

                def git(*arguments: str, directory: Path = root) -> str:
                    result = subprocess.run(
                        ["git", *arguments],
                        cwd=directory,
                        capture_output=True,
                        text=True,
                        check=True,
                    )
                    return result.stdout.strip()

                git("init", "--quiet")
                git("config", "user.name", "Reproduction")
                git("config", "user.email", "repro@example.invalid")
                git("config", "commit.gpgsign", "false")
                source = root / "kernel" / "patches" / "input.patch"
                source.parent.mkdir(parents=True)
                source.write_text("suite input\n", encoding="utf-8")
                git("add", ".")
                git("commit", "--quiet", "-m", "baseline")
                base = git("rev-parse", "HEAD")
                if name == "delete":
                    source.unlink()
                else:
                    assert destination is not None
                    target = root / destination
                    target.parent.mkdir(parents=True, exist_ok=True)
                    if name == "rename":
                        source.rename(target)
                    else:
                        target.write_text("new input\n", encoding="utf-8")
                git("add", "--all")
                git("commit", "--quiet", "-m", "changed paths")
                for event in ci.CI_CLASSIFIED_EVENTS:
                    with self.subTest(event=event):
                        output = root / f"output-{event}"
                        environment = os.environ.copy()
                        environment.update(
                            EVENT_NAME=event,
                            BASE_SHA=base,
                            HEAD_SHA=git("rev-parse", "HEAD"),
                            GITHUB_OUTPUT=os.fspath(output),
                            GITHUB_STEP_SUMMARY=os.fspath(root / "summary"),
                            RUNNER_TEMP=temporary,
                        )
                        result = subprocess.run(
                            ["bash", "-c", shim + script],
                            cwd=root,
                            env=environment,
                            capture_output=True,
                            text=True,
                            check=False,
                        )
                        self.assertEqual(result.returncode, 0, result.stderr)
                        actual = dict(
                            line.split("=", 1)
                            for line in output.read_text(encoding="utf-8").splitlines()
                        )
                        # A kernel patch is an input of every suite but the
                        # OpenVMM CI check, and documentation of none.
                        expected = {
                            output: str(
                                test_input and output != "run-openvmm-ci-check"
                            ).lower()
                            for output in ci.CI_CHANGE_OUTPUTS
                        }
                        self.assertEqual(actual, expected)

    @unittest.skipUnless(os.name == "posix", "requires bash")
    def test_ci_change_detector_runs_the_full_matrix_unless_classified(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        step = _workflow_step(workflow, "openvmm-changes", "Detect relevant changes")
        lines = step.splitlines()
        start = lines.index("        run: |") + 1
        script = "\n".join(line[10:] for line in lines[start:])
        stub = """
git() {
    case "$1" in
    cat-file) return "${GIT_CAT_FILE_STATUS}" ;;
    diff)
        printf '%b' "${CHANGED_FILES}"
        return "${GIT_DIFF_STATUS}"
        ;;
    esac
    echo "unexpected git command: $*" >&2
    return 1
}
python3() {
    if [[ -n "${CLASSIFIER:-}" ]]; then
        "${CLASSIFIER}" "$@"
        return
    fi
    printf '%b' "${CLASSIFICATION}"
    return "${CLASSIFIER_STATUS}"
}
"""
        # The classification of a change only to the microVM tests.
        classified = {
            "run-guest-artifacts": "true",
            "run-openvmm-binaries": "true",
            "run-openvmm-ci-check": "false",
            "run-microvm-tests": "true",
            "run-benchmarks": "false",
        }
        self.assertEqual(list(classified), list(ci.CI_CHANGE_OUTPUTS))
        complete = "".join(
            f"{output}={value}\\n" for output, value in classified.items()
        )
        everything = dict.fromkeys(ci.CI_CHANGE_OUTPUTS, "true")
        cases = (
            (
                "schedule",
                {"EVENT_NAME": "schedule", "GIT_CAT_FILE_STATUS": "1"},
                everything,
            ),
            (
                "workflow_dispatch",
                {"EVENT_NAME": "workflow_dispatch", "GIT_CAT_FILE_STATUS": "1"},
                everything,
            ),
            ("missing base", {"BASE_SHA": ""}, everything),
            ("zero base", {"BASE_SHA": "0" * 40}, everything),
            ("missing commit", {"GIT_CAT_FILE_STATUS": "1"}, everything),
            ("failed diff", {"GIT_DIFF_STATUS": "128"}, everything),
            ("failed classifier", {"CLASSIFIER_STATUS": "1"}, everything),
            # Every output must appear exactly once.
            *(
                (
                    f"missing {output}",
                    {"CLASSIFICATION": complete.replace(f"{output}={value}\\n", "")},
                    everything,
                )
                for output, value in classified.items()
            ),
            *(
                (
                    f"duplicate {output}",
                    {
                        "CLASSIFICATION": complete
                        + f"{output}={'false' if value == 'true' else 'true'}\\n"
                    },
                    everything,
                )
                for output, value in classified.items()
            ),
            (
                "malformed output",
                {"CLASSIFICATION": complete.replace("=true", "=yes", 1)},
                everything,
            ),
            (
                "classified",
                {"CLASSIFICATION": "notice: ignored\\n" + complete},
                classified,
            ),
            (
                "real classifier",
                {
                    "CLASSIFIER": sys.executable,
                    "CHANGED_FILES": "scripts/nvx_tools/microvm_tests.py\\0",
                },
                classified,
            ),
        )
        for name, overrides, expected in cases:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                environment = os.environ.copy()
                environment.update(
                    {
                        "EVENT_NAME": "pull_request",
                        "BASE_SHA": "1" * 40,
                        "HEAD_SHA": "2" * 40,
                        "GITHUB_OUTPUT": os.fspath(root / "output"),
                        "GITHUB_STEP_SUMMARY": os.fspath(root / "summary"),
                        "RUNNER_TEMP": temporary,
                        "GIT_CAT_FILE_STATUS": "0",
                        "GIT_DIFF_STATUS": "0",
                        "CHANGED_FILES": "openvmm\\0",
                        "CLASSIFICATION": complete,
                        "CLASSIFIER_STATUS": "0",
                    }
                )
                environment.update(overrides)
                result = subprocess.run(
                    ["bash", "-c", stub + script],
                    cwd=BuildConstants.REPO_ROOT,
                    env=environment,
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                output = (root / "output").read_text(encoding="utf-8").splitlines()
                self.assertEqual(len(output), len(expected), output)
                self.assertEqual(dict(line.split("=", 1) for line in output), expected)
                summary = (root / "summary").read_text(encoding="utf-8")
                if expected is everything:
                    self.assertIn("### Full matrix: run\n", summary)
                    if name in ("schedule", "workflow_dispatch"):
                        self.assertIn(
                            f"The {name} event runs the full matrix without "
                            "change classification.",
                            summary,
                        )
                else:
                    self.assertNotIn("Full matrix", summary)

    def test_flowey_downloads_use_retrying_curl(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "setup-curl"
            / "action.yml"
        ).read_text(encoding="utf-8")
        windows_shim = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "setup-curl"
            / "curl-shim.rs"
        ).read_text(encoding="utf-8")
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        microvm_workflow = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "workflows"
            / "run-nvx-microvm-tests.yml"
        ).read_text(encoding="utf-8")
        build_action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-openvmm"
            / "action.yml"
        ).read_text(encoding="utf-8")

        for option in (
            "--retry 5",
            "--retry-all-errors",
            "--retry-delay 2",
            "--retry-max-time 90",
        ):
            self.assertEqual(action.count(option), 1)
        for argument in (
            '"--retry"',
            '"5"',
            '"--retry-all-errors"',
            '"--retry-delay"',
            '"2"',
            '"--retry-max-time"',
            '"90"',
        ):
            self.assertIn(argument, windows_shim)
        self.assertIn('system_curl="${NVX_SYSTEM_CURL:-$(command -v curl)}"', action)
        self.assertIn("Get-Command curl.exe", action)
        self.assertIn('Join-Path $ShimDirectory "curl.exe"', action)
        self.assertIn("Get-Command rustc.exe", action)
        self.assertNotIn("curl.cmd", action)
        # Every job that runs flowey retries its curl downloads: the OpenVMM
        # binary builds, through build-openvmm, and the microVM tests. No job
        # of ci.yml runs flowey itself, since OpenVMM's own CI runs its tests.
        self.assertNotIn("uses: ./.github/actions/setup-curl", workflow)
        self.assertEqual(
            microvm_workflow.count("uses: ./.github/actions/setup-curl"),
            1,
        )
        self.assertIn("uses: ./.github/actions/setup-curl", build_action)

    @unittest.skipUnless(os.name == "nt", "Windows-specific curl resolution")
    def test_windows_curl_shim_retries_http_500(self):
        rustc = shutil.which("rustc")
        system_curl = shutil.which("curl.exe")
        if rustc is None or system_curl is None:
            self.fail("Windows curl shim test requires rustc and curl.exe")

        class RetryHandler(http.server.BaseHTTPRequestHandler):
            request_count = 0

            def do_GET(self) -> None:
                type(self).request_count += 1
                if type(self).request_count == 1:
                    self.send_error(http.HTTPStatus.INTERNAL_SERVER_ERROR)
                    return
                body = b"ok"
                self.send_response(http.HTTPStatus.OK)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, format: str, *args: object) -> None:
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), RetryHandler)
        server_thread = threading.Thread(target=server.serve_forever, daemon=True)
        server_thread.start()
        try:
            with tempfile.TemporaryDirectory() as directory:
                shim = Path(directory) / "curl.exe"
                shim_source = (
                    BuildConstants.REPO_ROOT
                    / ".github"
                    / "actions"
                    / "setup-curl"
                    / "curl-shim.rs"
                )
                subprocess.run(
                    [
                        rustc,
                        "--edition=2021",
                        "-C",
                        "opt-level=s",
                        "-o",
                        os.fspath(shim),
                        os.fspath(shim_source),
                    ],
                    check=True,
                )
                environment = os.environ.copy()
                environment["NVX_SYSTEM_CURL"] = system_curl
                environment["PATH"] = directory + os.pathsep + environment["PATH"]
                port = server.server_address[1]
                result = subprocess.run(
                    [
                        os.fspath(shim),
                        "--fail",
                        "-L",
                        f"http://127.0.0.1:{port}/asset",
                    ],
                    check=False,
                    capture_output=True,
                    env=environment,
                )
        finally:
            server.shutdown()
            server.server_close()
            server_thread.join(timeout=5)

        self.assertFalse(
            server_thread.is_alive(),
            "HTTP test server did not stop within 5 seconds",
        )
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        self.assertEqual(result.stdout, b"ok")
        self.assertEqual(RetryHandler.request_count, 2)

    def test_ci_shares_openvmm_inputs_and_binary_artifacts(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        build_workflow = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "workflows"
            / "build-openvmm-binary.yml"
        ).read_text(encoding="utf-8")
        microvm_workflow = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "workflows"
            / "run-nvx-microvm-tests.yml"
        ).read_text(encoding="utf-8")
        platform_workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "run-platform.yml"
        ).read_text(encoding="utf-8")
        package_workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "package-platform.yml"
        ).read_text(encoding="utf-8")
        build_action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-openvmm"
            / "action.yml"
        ).read_text(encoding="utf-8")

        # The binary builds cache the downloads of flowey, which only they run,
        # and the Cargo registry. A dev push whose build missed the binary
        # cache saves both, under the keys and paths that the builds restore,
        # since no other job builds OpenVMM.
        self.assertIn(
            "openvmm/flowey-persist/flowey_lib_common__download_gh_release",
            build_action,
        )
        self.assertIn("openvmm/flowey-persist/flowey_lib_common__cache", build_action)
        self.assertIn(
            "openvmm-inputs-v1-${{ runner.os }}-${{ runner.arch }}-", build_action
        )
        self.assertNotIn("flowey-persist", workflow)
        for system in ("Linux", "Windows"):
            for name, restore_id in (
                ("OpenVMM inputs", f"restore-openvmm-inputs-{system.lower()}"),
                ("shared Cargo cache", f"restore-cargo-{system.lower()}"),
            ):
                with self.subTest(system=system, cache=name):
                    restore = _composite_action_step(
                        build_action, f"Restore {name} on {system}"
                    )
                    save = _composite_action_step(
                        build_action, f"Save {name} on {system}"
                    )
                    self.assertEqual(_yaml_field(restore, "id"), [restore_id])
                    for field in ("key", "path"):
                        self.assertEqual(
                            _yaml_field(save, field), _yaml_field(restore, field)
                        )
                    self.assertEqual(
                        _expression_conjuncts(_yaml_scalar(save, "if")),
                        [
                            "success()",
                            "github.event_name == 'push'",
                            "github.ref == 'refs/heads/dev'",
                            f"runner.os {'!=' if system == 'Linux' else '=='} "
                            "'Windows'",
                            f"steps.restore-openvmm-binary-{system.lower()}.outputs."
                            "cache-hit != 'true'",
                            f"steps.{restore_id}.outputs.cache-hit != 'true'",
                        ],
                    )
        producers = {
            "build-openvmm-linux-gnu": (
                "kvm",
                "openvmm-linux-gnu",
            ),
            "build-openvmm-linux-musl": (
                "mshv",
                "openvmm-linux-musl",
            ),
            "build-openvmm-windows-msvc": (
                "whp",
                "openvmm-windows-msvc",
            ),
        }
        self.assertEqual(
            workflow.count("uses: ./.github/workflows/build-openvmm-binary.yml"),
            len(producers),
        )
        for job_name, (backend, artifact) in producers.items():
            with self.subTest(producer=job_name):
                job = _workflow_job(workflow, job_name)
                self.assertIn(f"backend: {backend}", job)
                self.assertIn(f"artifact: {artifact}", job)
                self.assertNotIn("target:", job)
                self.assertNotIn("build-mode:", job)
                self.assertNotIn("strategy:", job)

        consumers = {
            "nvx-microvm-tests-kvm": (
                "build-openvmm-linux-gnu",
                "openvmm-linux-gnu",
                "run-nvx-microvm-tests.yml",
            ),
            "nvx-microvm-tests-mshv": (
                "build-openvmm-linux-gnu",
                "openvmm-linux-gnu",
                "run-nvx-microvm-tests.yml",
            ),
            "nvx-microvm-tests-whp": (
                "build-openvmm-windows-msvc",
                "openvmm-windows-msvc",
                "run-nvx-microvm-tests.yml",
            ),
            "nvx-microvm-guests-kvm": (
                "build-openvmm-linux-gnu",
                "openvmm-linux-gnu",
                "run-nvx-microvm-tests.yml",
            ),
            "nvx-microvm-guests-mshv": (
                "build-openvmm-linux-gnu",
                "openvmm-linux-gnu",
                "run-nvx-microvm-tests.yml",
            ),
            "nvx-microvm-guests-whp": (
                "build-openvmm-windows-msvc",
                "openvmm-windows-msvc",
                "run-nvx-microvm-tests.yml",
            ),
            "nvx-microvm-debug-kvm": (
                "build-openvmm-linux-gnu",
                "openvmm-linux-gnu",
                "run-nvx-microvm-tests.yml",
            ),
            "nvx-microvm-debug-mshv": (
                "build-openvmm-linux-gnu",
                "openvmm-linux-gnu",
                "run-nvx-microvm-tests.yml",
            ),
            "nvx-microvm-debug-whp": (
                "build-openvmm-windows-msvc",
                "openvmm-windows-msvc",
                "run-nvx-microvm-tests.yml",
            ),
            "platform-kvm": (
                "build-openvmm-linux-gnu",
                "openvmm-linux-gnu",
                "run-platform.yml",
            ),
            "platform-mshv": (
                "build-openvmm-linux-musl",
                "openvmm-linux-musl",
                "run-platform.yml",
            ),
            "platform-whp": (
                "build-openvmm-windows-msvc",
                "openvmm-windows-msvc",
                "run-platform.yml",
            ),
            "package-kvm": (
                "build-openvmm-linux-gnu",
                "openvmm-linux-gnu",
                "package-platform.yml",
            ),
            "package-mshv": (
                "build-openvmm-linux-musl",
                "openvmm-linux-musl",
                "package-platform.yml",
            ),
            "package-whp": (
                "build-openvmm-windows-msvc",
                "openvmm-windows-msvc",
                "package-platform.yml",
            ),
        }
        for job_name, (producer, artifact, reusable_workflow) in consumers.items():
            with self.subTest(consumer=job_name):
                job = _workflow_job(workflow, job_name)
                # Each waits for the nvx build of its OS, which it runs or
                # packages, but not for the CLI tests that run with it; the
                # platform jobs, which neither run nor package it, wait for
                # neither.
                nvx_cli = (
                    "build-nvx-windows-msvc, "
                    if producer == "build-openvmm-windows-msvc"
                    else "build-nvx-linux-musl, "
                )
                # Only the jobs that boot or ship the Ubuntu or Azure Linux
                # images wait for them: the default suites and the platform
                # jobs boot the kernel and the Alpine initramfs alone, and the
                # debug-kernel jobs the debug kernel.
                if job_name.startswith("nvx-microvm-debug-"):
                    guest_producer = "debug-kernel, "
                elif job_name.startswith("nvx-microvm-tests-"):
                    guest_producer = ""
                elif job_name.startswith("platform-"):
                    guest_producer = ""
                    nvx_cli = ""
                else:
                    guest_producer = "distro-artifacts, "
                self.assertIn(
                    f"needs: [artifacts, {guest_producer}{producer}, {nvx_cli}"
                    "openvmm-changes]",
                    job,
                )
                self.assertIn(f"needs.{producer}.result == 'success'", job)
                if nvx_cli:
                    self.assertIn(
                        f"needs.{nvx_cli.removesuffix(', ')}.result == 'success'", job
                    )
                self.assertNotIn("nvx-cli", job)
                self.assertIn(f"uses: ./.github/workflows/{reusable_workflow}", job)
                self.assertIn(f"openvmm-artifact: {artifact}", job)
                for unrelated_producer in producers.keys() - {producer}:
                    self.assertNotIn(unrelated_producer, job)

        self.assertIn("build/openvmm.provenance.json", build_workflow)
        self.assertIn(
            "name: ${{ inputs.artifact }}-executable",
            build_workflow,
        )
        self.assertIn(
            "name: ${{ inputs.artifact }}-provenance",
            build_workflow,
        )
        self.assertEqual(
            build_workflow.count("uses: ./.github/actions/build-openvmm"),
            1,
        )
        self.assertIn("backend: ${{ inputs.backend }}", build_workflow)
        self.assertNotIn("target: ${{ inputs.target }}", build_workflow)
        self.assertNotIn("build-mode: ${{ inputs.build-mode }}", build_workflow)
        self.assertEqual(build_workflow.count("overwrite: true"), 2)
        self.assertEqual(build_workflow.count("retention-days: 1"), 2)
        # The microVM workflow also downloads nvx, which the benchmarks
        # don't run, the debug kernel, which neither the platform nor package
        # jobs read, and the Ubuntu distro layer a second time for an
        # unsharded default run. The platform jobs download only what they
        # boot, and the package jobs every image that a package ships; their
        # action downloads nvx.
        for consumer_workflow, download_count in (
            (microvm_workflow, 10),
            (platform_workflow, 4),
            (package_workflow, 7),
        ):
            self.assertIn("path: openvmm/target/release", consumer_workflow)
            self.assertIn("path: build", consumer_workflow)
            self.assertEqual(
                consumer_workflow.count("uses: actions/download-artifact@"),
                download_count,
            )
            self.assertIn(
                "run: chmod +x openvmm/target/release/openvmm",
                consumer_workflow,
            )
        for configuration in (
            workflow,
            build_workflow,
            microvm_workflow,
            platform_workflow,
            package_workflow,
        ):
            self.assertNotIn("path: .", configuration)
        self.assertIn("openvmm-binary-v6-${{ inputs.backend }}-", build_action)
        self.assertNotIn("openvmm-binary-v5-", build_action)
        self.assertNotIn("inputs.target", build_action)
        self.assertNotIn("inputs.build-mode", build_action)
        self.assertNotIn("rustup target add", build_action)
        self.assertNotIn("X86_64_UNKNOWN_LINUX_MUSL", build_action)
        self.assertEqual(
            build_action.count(
                'python3 scripts/nvx.py build-openvmm --backend "${{ inputs.backend }}"'
            ),
            1,
        )
        self.assertEqual(
            build_action.count(
                'python scripts\\nvx.py build-openvmm --backend "${{ inputs.backend }}"'
            ),
            1,
        )
        self.assertIn("Verify OpenVMM provenance on Linux", build_action)
        self.assertIn("Verify OpenVMM provenance on Windows", build_action)
        self.assertNotIn("nvx-microvm-tests-v1", workflow)
        self.assertNotIn("cargo-v2-", build_action)
        self.assertNotIn("openvmm-tests-v2-", workflow)
        self.assertNotIn("Restore OpenVMM test build", workflow)
        self.assertNotIn("Save OpenVMM test build", workflow)
        self.assertEqual(
            build_workflow.count("uses: ./.github/actions/sccache"),
            2,
        )
        # Only the binary builds compile OpenVMM, through build-openvmm:
        # OpenVMM's own CI runs its tests (doc/ci.md#openvmm-ci), so no job of
        # ci.yml starts sccache or caches OpenVMM's inputs itself.
        self.assertNotIn("uses: ./.github/actions/sccache", workflow)
        self.assertNotIn("actions/cache/", workflow)
        self.assertNotIn("openvmm-inputs-v1-", workflow)
        self.assertNotIn("openvmm-cargo-v1-", workflow)
        self.assertNotIn("uses: actions/cache@", workflow)
        self.assertNotIn("uses: actions/cache@", build_workflow)
        self.assertNotIn("uses: actions/cache@", build_action)

        release_job = _workflow_job(workflow, "release")
        performance_gate_job = _workflow_job(workflow, "performance-gate")
        performance_record_job = _workflow_job(workflow, "performance-record")
        for job_name in consumers:
            self.assertIn(f"      - {job_name}", release_job)
            self.assertIn(f"needs.{job_name}.result", release_job)
            self.assertIn(f"      - {job_name}", performance_record_job)
            self.assertIn(f"needs.{job_name}.result", performance_record_job)
        for job_name in ("platform-kvm", "platform-mshv", "platform-whp"):
            self.assertIn(job_name, performance_gate_job)
            self.assertIn(f"needs.{job_name}.result", performance_gate_job)

    def test_copilot_setup_restores_ci_cache_entries(self):
        github = BuildConstants.REPO_ROOT / ".github"
        setup = (github / "workflows" / "copilot-setup-steps.yml").read_text(
            encoding="utf-8"
        )
        workflow = (github / "workflows" / "ci.yml").read_text(encoding="utf-8")
        build_workflow = (github / "workflows" / "build-openvmm-binary.yml").read_text(
            encoding="utf-8"
        )
        build_action = (github / "actions" / "build-openvmm" / "action.yml").read_text(
            encoding="utf-8"
        )
        checkout_action = (
            github / "actions" / "checkout-openvmm" / "action.yml"
        ).read_text(encoding="utf-8")

        # Guest artifact keys stay in sync because both sides use one action.
        guest_artifacts = _workflow_step(
            setup, "copilot-setup-steps", "Restore guest artifacts built by CI"
        )
        self.assertEqual(
            _yaml_field(guest_artifacts, "uses"),
            ["./.github/actions/build-guest-artifacts"],
        )
        self.assertEqual(_yaml_field(guest_artifacts, "restore-only"), ['"true"'])

        # The OpenVMM binary restore repeats the cache entry that the Linux GNU
        # producer saves. A different key, path list, or compression method
        # would make every session miss it without failing setup.
        producer = _workflow_job(workflow, "build-openvmm-linux-gnu")
        [backend] = _yaml_field(producer, "backend")
        [binary] = _yaml_field(producer, "binary")
        self.assertIn("binary-path: ${{ inputs.binary }}", build_workflow)
        saved = _composite_action_step(build_action, "Save OpenVMM binary on Linux")
        restored = _workflow_step(
            setup, "copilot-setup-steps", "Restore the OpenVMM binary built by CI"
        )
        self.assertEqual(
            _yaml_field(restored, "key"),
            [
                key.replace("${{ inputs.backend }}", backend).replace(
                    "steps.openvmm.outputs.sha", "steps.openvmm-cache.outputs.sha"
                )
                for key in _yaml_field(saved, "key")
            ],
        )
        self.assertEqual(
            _yaml_field(restored, "path"),
            [
                path.replace("${{ inputs.binary-path }}", binary)
                for path in _yaml_field(saved, "path")
            ],
        )

        prepare = _workflow_step(
            setup, "copilot-setup-steps", "Prepare OpenVMM cache restore"
        )
        for revision_source in (checkout_action, prepare):
            self.assertIn("sha=$(git rev-parse :openvmm)", revision_source)

        def tool_links(configuration: str) -> list[str]:
            return [
                line.strip()
                for line in configuration.splitlines()
                if line.strip().startswith("ln -sf ")
            ]

        self.assertEqual(
            _yaml_field(saved, "PATH"),
            ["${{ steps.cache-tools-linux.outputs.path }}"],
        )
        self.assertEqual(
            _yaml_field(restored, "PATH"),
            ["${{ steps.openvmm-cache.outputs.path }}"],
        )
        self.assertEqual(
            tool_links(prepare),
            tool_links(
                _composite_action_step(build_action, "Prepare Linux cache tools")
            ),
        )

    def test_guest_artifact_restore_only_guards_builds_and_cache_saves(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        ).read_text(encoding="utf-8")
        steps = _composite_action_steps(action)
        guarded_steps = [
            step
            for step in steps
            if "docker build" in step or "uses: actions/cache/save@" in step
        ]

        self.assertTrue(guarded_steps)
        for step in guarded_steps:
            try:
                name = _yaml_scalar(step, "name")
            except AssertionError:
                name = "<unnamed>"
            with self.subTest(step=name):
                condition = _yaml_scalar(step, "if")
                guard = "inputs.restore-only != 'true' && "
                self.assertTrue(condition.startswith(guard), condition)
                cache_condition = condition.removeprefix(guard)
                self.assertIn("cache-hit != 'true'", cache_condition)
                if "||" in cache_condition:
                    self.assertTrue(
                        _outer_parentheses_enclose(cache_condition),
                        cache_condition,
                    )

    def test_guest_artifact_restore_only_leaves_restore_steps_active(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "build-guest-artifacts"
            / "action.yml"
        ).read_text(encoding="utf-8")
        steps = _composite_action_steps(action)
        restore_steps = [
            step for step in steps if "uses: actions/cache/restore@" in step
        ]

        self.assertTrue(restore_steps)
        # The distro layer's cache entry holds the scratch template, which no
        # step formats on its own any more.
        self.assertNotIn("mkfs", action)
        for step in restore_steps:
            try:
                name = _yaml_scalar(step, "name")
            except AssertionError:
                name = "<unnamed>"
            with self.subTest(step=name):
                # Other inputs, such as debug-kernel, may select these steps,
                # but restore-only must leave them active.
                try:
                    condition = _yaml_scalar(step, "if")
                except AssertionError:
                    continue
                self.assertNotIn("restore-only", condition)

    def test_guest_artifact_jobs_record_their_components(self):
        github = BuildConstants.REPO_ROOT / ".github"
        action = yaml.safe_load(
            (github / "actions" / "build-guest-artifacts" / "action.yml").read_text(
                encoding="utf-8"
            )
        )
        steps = action["runs"]["steps"]
        # The cache entries of each recorded component, as its restore steps
        # name them, hold exactly its files under exactly the keys that its
        # record gives.
        restored: dict[str, tuple[set[str], set[str]]] = {}
        for step in steps:
            if not str(step.get("uses", "")).startswith("actions/cache/restore@"):
                continue
            [component] = re.findall(
                r"steps\.components\.outputs\.([a-z-]+) == 'true'", step["if"]
            )
            key = re.fullmatch(
                r"\$\{\{ steps\.guest-cache-keys\.outputs\.([a-z-]+) \}\}",
                step["with"]["key"],
            )
            assert key is not None
            keys, files = restored.setdefault(component, (set(), set()))
            keys.add(key.group(1))
            files.update(
                PurePosixPath(path).name for path in step["with"]["path"].split()
            )
        self.assertEqual(
            {
                component: entry
                for component, entry in restored.items()
                if component != "debug-kernel"
            },
            {
                component: (
                    set(keys),
                    set(release.GUEST_ARTIFACT_COMPONENTS[component]),
                )
                for component, keys in guest_artifact_reuse.COMPONENT_CACHE_KEYS.items()
            },
        )
        # The record follows every build, and restore-only, which builds
        # nothing, records nothing.
        record, upload = steps[-2:]
        self.assertEqual(record["name"], "Record guest artifact keys and digests")
        self.assertEqual(
            _expression_conjuncts(record["if"]),
            ["inputs.restore-only != 'true'", "inputs.record-artifact != ''"],
        )
        self.assertEqual(
            record["env"],
            {
                "COMPONENTS": "${{ inputs.components }}",
                "GUEST_CACHE_KEYS": "${{ toJSON(steps.guest-cache-keys.outputs) }}",
            },
        )
        self.assertEqual(
            record["run"].split(),
            [
                "python3",
                "scripts/nvx.py",
                "record-guest-artifacts",
                "--components",
                '"${COMPONENTS}"',
                "--keys",
                '"${GUEST_CACHE_KEYS}"',
            ],
        )
        self.assertEqual(upload["name"], "Upload guest artifact record")
        self.assertEqual(upload["if"], record["if"])
        self.assertRegex(upload["uses"], r"^actions/upload-artifact@")
        # It expires with the components' artifacts.
        self.assertEqual(
            upload["with"],
            {
                "name": "${{ inputs.record-artifact }}",
                "path": f"build/{guest_artifact_reuse.RECORD_NAME}",
                "if-no-files-found": "error",
                "retention-days": 1,
            },
        )

        # Each job that builds recorded components records them under a name
        # of its own, which the guest-* names of the handoff never take; the
        # debug kernel job records nothing.
        workflow = yaml.safe_load(
            (github / "workflows" / "ci.yml").read_text(encoding="utf-8")
        )
        records: dict[str, tuple[str, ...]] = {}
        for job_name, job in workflow["jobs"].items():
            for step in job.get("steps", []):
                if step.get("uses") != "./.github/actions/build-guest-artifacts":
                    continue
                arguments = step["with"]
                with self.subTest(job=job_name):
                    if arguments["components"].split() == ["debug-kernel"]:
                        self.assertNotIn("record-artifact", arguments)
                        continue
                    name = arguments["record-artifact"]
                    self.assertFalse(name.startswith("guest-"), name)
                    self.assertNotIn(name, records)
                    records[name] = guest_artifact_reuse.parse_components(
                        arguments["components"]
                    )
        self.assertEqual(
            records,
            {
                "kernel-alpine-record": ("kernel", "alpine"),
                "ubuntu-azurelinux-record": ("ubuntu", "azurelinux"),
            },
        )
        self.assertEqual(tuple(records.values()), _REUSE_JOBS)

    def test_dev_pushes_reuse_the_guest_artifacts_of_their_pull_requests(self):
        github = BuildConstants.REPO_ROOT / ".github"
        workflow = yaml.safe_load(
            (github / "workflows" / "ci.yml").read_text(encoding="utf-8")
        )
        jobs = workflow["jobs"]
        # Reading runs and downloading their artifacts needs actions: read,
        # which no other job but the rehearsal, the release, and the
        # persistence of the performance results that the dev pushes recorded
        # gets, and cancelling the runs of superseded merge groups actions:
        # write.
        self.assertEqual(workflow["permissions"], {"contents": "read"})
        self.assertEqual(
            {
                name: job["permissions"]["actions"]
                for name, job in jobs.items()
                if "actions" in (job.get("permissions") or {})
            },
            {
                "artifacts": "read",
                "distro-artifacts": "read",
                "guest-artifact-reuse": "read",
                "release": "read",
                "performance-persist": "read",
                "cancel-superseded-merge-groups": "write",
            },
        )
        reusing: set[str] = set()
        for job_name, job in jobs.items():
            for step in job.get("steps", []):
                if step.get("uses") != "./.github/actions/build-guest-artifacts":
                    continue
                arguments = step["with"]
                with self.subTest(job=job_name):
                    if "record-artifact" not in arguments:
                        self.assertNotIn("reuse", arguments)
                        continue
                    reusing.add(job_name)
                    self.assertEqual(
                        job["permissions"], {"actions": "read", "contents": "read"}
                    )
                    # Only pushes and merge groups reuse, since neither can
                    # restore the pull request's caches; pull requests and full
                    # runs build what misses the cache, as before.
                    self.assertEqual(
                        arguments["reuse"],
                        "${{ github.event_name == 'merge_group' || "
                        "github.event_name == 'push' }}",
                    )
        self.assertEqual(reusing, {"artifacts", "distro-artifacts"})

        action = yaml.safe_load(
            (github / "actions" / "build-guest-artifacts" / "action.yml").read_text(
                encoding="utf-8"
            )
        )
        self.assertEqual(action["inputs"]["reuse"]["default"], "false")
        steps = action["runs"]["steps"]
        position = next(
            index for index, step in enumerate(steps) if step.get("id") == "reuse"
        )
        reuse = steps[position]
        self.assertEqual(
            reuse["name"], "Reuse the merged pull request's guest artifacts"
        )
        # A failure falls back to building rather than failing the run.
        self.assertIs(reuse["continue-on-error"], True)
        self.assertEqual(
            _expression_conjuncts(reuse["if"]),
            [
                "inputs.restore-only != 'true'",
                "inputs.reuse == 'true'",
                "inputs.record-artifact != ''",
            ],
        )
        self.assertEqual(
            reuse["env"],
            {
                "GH_TOKEN": "${{ github.token }}",
                "COMPONENTS": "${{ inputs.components }}",
                "GUEST_CACHE_KEYS": "${{ toJSON(steps.guest-cache-keys.outputs) }}",
                "RECORD_ARTIFACT": "${{ inputs.record-artifact }}",
            },
        )
        self.assertEqual(
            reuse["run"].split(),
            [
                "python3",
                "scripts/nvx.py",
                "reuse-guest-artifacts",
                "--repository",
                '"${GITHUB_REPOSITORY}"',
                "--workflow-ref",
                '"${GITHUB_WORKFLOW_REF}"',
                "--commit",
                '"${GITHUB_SHA}"',
                "--components",
                '"${COMPONENTS}"',
                "--record-artifact",
                '"${RECORD_ARTIFACT}"',
                "--keys",
                '"${GUEST_CACHE_KEYS}"',
            ],
        )
        # Reuse fills in what the restores of the recorded components missed,
        # before any of their builds, which then skip. The saves still cache
        # what it installed, and the debug kernel never comes from it.
        restores = [
            index
            for index, step in enumerate(steps)
            if str(step.get("uses", "")).startswith("actions/cache/restore@")
            and "debug" not in step["name"]
        ]
        builds = [
            index
            for index, step in enumerate(steps)
            if "docker build" in str(step.get("run", ""))
            and "debug" not in step["name"]
        ]

        def required(expression: str) -> list[str]:
            # The conditions that an expression ANDs, through parentheses.
            conditions: list[str] = []
            for conjunct in _expression_conjuncts(expression):
                if _outer_parentheses_enclose(conjunct):
                    conditions.extend(required(conjunct.strip()[1:-1]))
                else:
                    conditions.append(conjunct)
            return conditions

        self.assertEqual(len(restores), 5)
        self.assertEqual(len(builds), 6)
        self.assertLess(max(restores), position)
        self.assertLess(position, min(builds))
        for index in builds:
            with self.subTest(step=steps[index]["name"]):
                self.assertIn(
                    "steps.reuse.outputs.reused != 'true'",
                    required(steps[index]["if"]),
                )
        for step in steps:
            if step is not reuse and (
                "debug" in step["name"]
                or str(step.get("uses", "")).startswith("actions/")
            ):
                with self.subTest(step=step["name"]):
                    self.assertNotIn("steps.reuse", str(step.get("if", "")))

    def test_pull_requests_rehearse_guest_artifact_reuse(self):
        github = BuildConstants.REPO_ROOT / ".github"
        workflow = yaml.safe_load(
            (github / "workflows" / "ci.yml").read_text(encoding="utf-8")
        )
        jobs = workflow["jobs"]
        rehearsal = jobs[ci.REQUIRED_CI_GUEST_ARTIFACT_REUSE_JOB]
        self.assertEqual(rehearsal["name"], "Rehearse guest artifact reuse")
        self.assertEqual(
            rehearsal["needs"], ["openvmm-changes", "artifacts", "distro-artifacts"]
        )
        # Same-repository pull requests only, once both guest artifact jobs
        # succeeded: a push reuses for real, and a fork's run never qualifies.
        self.assertEqual(
            _expression_conjuncts(rehearsal["if"]),
            [
                "always()",
                "!cancelled()",
                "github.event_name == 'pull_request'",
                "github.event.pull_request.head.repo.full_name == github.repository",
                "needs.artifacts.result == 'success'",
                "needs.distro-artifacts.result == 'success'",
                "( needs.openvmm-changes.result != 'success' || "
                "needs.openvmm-changes.outputs.run-guest-artifacts == 'true' )",
            ],
        )
        self.assertEqual(rehearsal["runs-on"], "ubuntu-latest")
        self.assertEqual(
            rehearsal["permissions"], {"actions": "read", "contents": "read"}
        )
        # One step for each guest artifact job, which checks that job's
        # record and artifacts with that job's keys.
        steps = [step for step in rehearsal["steps"] if "run" in step]
        records: dict[str, str] = {}
        for job_name in ("artifacts", "distro-artifacts"):
            job = jobs[job_name]
            build = next(
                step
                for step in job["steps"]
                if step.get("uses") == "./.github/actions/build-guest-artifacts"
            )
            records[job_name] = build["with"]["record-artifact"]
            # It compares the record with the keys that the action resolved.
            self.assertEqual(
                job["outputs"],
                {"guest-cache-keys": "${{ steps.guest-artifacts.outputs.cache-keys }}"},
            )
        self.assertEqual(len(steps), 2)
        for step, job_name in zip(
            steps, ("artifacts", "distro-artifacts"), strict=True
        ):
            build = next(
                step
                for step in jobs[job_name]["steps"]
                if step.get("uses") == "./.github/actions/build-guest-artifacts"
            )
            with self.subTest(job=job_name):
                self.assertEqual(
                    step["env"],
                    {
                        "GH_TOKEN": "${{ github.token }}",
                        "GUEST_CACHE_KEYS": (
                            f"${{{{ needs.{job_name}.outputs.guest-cache-keys }}}}"
                        ),
                        "HEAD_SHA": "${{ github.event.pull_request.head.sha }}",
                    },
                )
                run = step["run"].split()
                components = run[run.index("--components") + 1 :]
                components = components[: components.index("--record-artifact")]
                self.assertEqual(
                    " ".join(components).strip('"'), build["with"]["components"]
                )
                self.assertEqual(
                    run,
                    [
                        "python3",
                        "scripts/nvx.py",
                        "reuse-guest-artifacts",
                        "--dry-run",
                        "--repository",
                        '"${GITHUB_REPOSITORY}"',
                        "--workflow-ref",
                        '"${GITHUB_WORKFLOW_REF}"',
                        "--head-sha",
                        '"${HEAD_SHA}"',
                        "--components",
                        *components,
                        "--record-artifact",
                        records[job_name],
                        "--keys",
                        '"${GUEST_CACHE_KEYS}"',
                    ],
                )
        action = yaml.safe_load(
            (github / "actions" / "build-guest-artifacts" / "action.yml").read_text(
                encoding="utf-8"
            )
        )
        self.assertEqual(
            action["outputs"]["cache-keys"]["value"],
            "${{ toJSON(steps.guest-cache-keys.outputs) }}",
        )
        # The required status check takes its result; no other job waits for
        # it.
        for name, job in jobs.items():
            with self.subTest(job=name):
                if name == "required-status-check":
                    self.assertIn(ci.REQUIRED_CI_GUEST_ARTIFACT_REUSE_JOB, job["needs"])
                else:
                    self.assertNotIn(
                        ci.REQUIRED_CI_GUEST_ARTIFACT_REUSE_JOB,
                        str(job.get("needs", "")),
                    )
        # A change to the reuse, its record, or their commands rehearses it.
        for path in (
            "scripts/nvx_tools/guest_artifact_reuse.py",
            "scripts/nvx_tools/release.py",
            "scripts/nvx.py",
            ".github/actions/build-guest-artifacts/action.yml",
        ):
            with self.subTest(path=path):
                self.assertTrue(
                    ci.classify_ci_changes("pull_request", [path]).run_guest_artifacts
                )

    def test_reuse_downloads_the_artifacts_of_each_recorded_component(self):
        # The artifacts that reuse downloads for each component are those in
        # which the job that builds it hands its files over, and together they
        # hold every file of the component; the debug kernel is in none.
        github = BuildConstants.REPO_ROOT / ".github"
        workflow = yaml.safe_load(
            (github / "workflows" / "ci.yml").read_text(encoding="utf-8")
        )
        builders: dict[str, str] = {}
        for job_name, job in workflow["jobs"].items():
            for step in job.get("steps", []):
                if step.get("uses") == "./.github/actions/build-guest-artifacts":
                    for component in step["with"]["components"].split():
                        builders[component] = job_name
        uploads = _guest_uploads()
        self.assertEqual(
            set(guest_artifact_reuse.COMPONENT_ARTIFACTS),
            set(release.GUEST_ARTIFACT_COMPONENTS),
        )
        handed_over: set[str] = set()
        for component, names in guest_artifact_reuse.COMPONENT_ARTIFACTS.items():
            with self.subTest(component=component):
                files: list[str] = []
                for name in names:
                    job_name, members = uploads[name]
                    self.assertEqual(job_name, builders[component])
                    files.extend(members)
                    handed_over.add(name)
                self.assertEqual(
                    sorted(files),
                    sorted(release.GUEST_ARTIFACT_COMPONENTS[component]),
                )
        self.assertEqual(set(uploads) - handed_over, {"guest-debug-kernel"})

    def test_ci_runs_debug_kernel_on_mshv_for_prs_and_all_for_non_prs(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        microvm_workflow = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "workflows"
            / "run-nvx-microvm-tests.yml"
        ).read_text(encoding="utf-8")

        artifacts = _workflow_job(workflow, "artifacts")
        self.assertNotIn("debug-kernel", artifacts)
        self.assertNotIn("vmlinux-debug", artifacts)
        # The debug kernel builds on GitHub-hosted capacity beside the shared
        # artifacts, under the same condition, and only the debug jobs wait.
        debug_build = _workflow_job(workflow, "debug-kernel")
        self.assertIn("runs-on: ubuntu-latest", debug_build)
        self.assertIn("components: debug-kernel\n", debug_build)
        self.assertIn("name: guest-debug-kernel", debug_build)
        for name in (
            KernelBuildConstants.DEBUG_BINARY_NAME,
            KernelBuildConstants.DEBUG_CONFIG_NAME,
        ):
            self.assertIn(f"build/{name}\n", debug_build)

        def condition(job: str) -> str:
            return job[job.index("    if: >-\n") : job.index("    runs-on:")]

        self.assertEqual(condition(debug_build), condition(artifacts))
        # So do the Ubuntu and Azure Linux images, so that no job waits for a
        # guest artifact job behind another one.
        for producer in ("artifacts", "distro-artifacts", "debug-kernel"):
            with self.subTest(producer=producer):
                job = _workflow_job(workflow, producer)
                self.assertIn("    needs: [openvmm-changes]\n", job)
                self.assertIn("    runs-on: ubuntu-latest\n", job)
                self.assertEqual(condition(job), condition(artifacts))
        for consumer in ("release", "performance-record"):
            with self.subTest(consumer=consumer):
                job = _workflow_job(workflow, consumer)
                self.assertIn("      - debug-kernel\n", job)
                self.assertIn("needs.debug-kernel.result == 'success' &&", job)
        # Pull requests run the debug kernel on MSHV alone, merge groups run it
        # on KVM alone, and dev pushes and full runs run it on every backend.
        self.assertEqual(ci.REQUIRED_CI_MICROVM_DEBUG_JOBS, ("nvx-microvm-debug-mshv",))
        self.assertEqual(
            ci.REQUIRED_CI_MICROVM_DEBUG_MERGE_GROUP_JOBS,
            ("nvx-microvm-debug-kvm",),
        )
        self.assertEqual(
            ci.REQUIRED_CI_MICROVM_DEBUG_PUSH_JOBS,
            ("nvx-microvm-debug-kvm", "nvx-microvm-debug-whp"),
        )
        mshv = _workflow_job(workflow, "nvx-microvm-debug-mshv")
        self.assertIn("github.event.pull_request.head.repo.full_name", mshv)
        self.assertIn("github.event.pull_request.draft == false", mshv)
        self.assertIn("github.event_name != 'merge_group' &&", mshv)
        self.assertNotIn("github.event_name != 'pull_request' &&", mshv)
        self.assertIn(
            '      runner: \'["self-hosted", "linux", "mshv", '
            '"virtual-machine", "general"]\'\n',
            mshv,
        )
        kvm = _workflow_job(workflow, "nvx-microvm-debug-kvm")
        self.assertIn("github.event_name != 'pull_request' &&", kvm)
        self.assertNotIn("github.event_name != 'merge_group' &&", kvm)
        self.assertNotIn("github.event.pull_request", kvm)
        whp = _workflow_job(workflow, "nvx-microvm-debug-whp")
        self.assertIn("github.event_name != 'pull_request' &&", whp)
        self.assertIn("github.event_name != 'merge_group' &&", whp)
        self.assertNotIn("github.event.pull_request", whp)
        for job_name in dict.fromkeys(
            (
                *ci.REQUIRED_CI_MICROVM_DEBUG_JOBS,
                *ci.REQUIRED_CI_MICROVM_DEBUG_MERGE_GROUP_JOBS,
                *ci.REQUIRED_CI_MICROVM_DEBUG_PUSH_JOBS,
            )
        ):
            with self.subTest(job=job_name):
                job = _workflow_job(workflow, job_name)
                self.assertIn("debug-kernel: true", job)
                self.assertIn("needs.debug-kernel.result == 'success' &&", job)
                self.assertIn(job_name, ci.REQUIRED_CI_RESULT_ENVIRONMENTS)
                for consumer in ("release", "performance-record"):
                    self.assertIn(
                        f"needs.{job_name}.result == 'success' ||\n"
                        f"          needs.{job_name}.result == 'skipped'",
                        _workflow_job(workflow, consumer),
                    )

        self.assertIn("      debug-kernel:\n", microvm_workflow)
        self.assertIn("        type: boolean\n", microvm_workflow)
        download = _workflow_step(
            microvm_workflow, "test", "Download guest debug kernel"
        )
        self.assertIn("\n          name: guest-debug-kernel\n", download)
        self.assertEqual(_yaml_scalar(download, "if"), "inputs.debug-kernel")
        debug_flag = "${{ inputs.debug-kernel && '--debug-kernel' || '' }}"
        self.assertEqual(microvm_workflow.count(debug_flag), 2)
        # The debug run replaces every production-kernel workload rather than
        # adding to it: each step that drives nvx.py without the debug kernel
        # flag boots build/vmlinux, so it must skip the debug jobs (#371).
        guard = " && !inputs.debug-kernel"
        debug_steps: list[str] = []
        production_steps: list[str] = []
        for step in _workflow_steps(microvm_workflow, "test"):
            try:
                script = _yaml_scalar(step, "run")
            except AssertionError:
                continue
            if "nvx.py" not in script:
                continue
            name = _yaml_scalar(step, "name")
            try:
                step_condition = _yaml_scalar(step, "if")
            except AssertionError:
                step_condition = ""
            with self.subTest(step=name):
                if debug_flag in script:
                    debug_steps.append(name)
                    self.assertNotIn("debug-kernel", step_condition)
                    continue
                production_steps.append(name)
                self.assertTrue(step_condition.endswith(guard), step_condition)
                selector = step_condition.removesuffix(guard)
                if "||" in selector:
                    self.assertTrue(
                        _outer_parentheses_enclose(selector), step_condition
                    )
        self.assertEqual(len(debug_steps), 2)
        self.assertTrue(production_steps)

    def test_ci_runs_the_steps_after_the_default_microvm_suite_beside_it(self):
        # #695: the WHP job ran four minutes of guest smoke and sandbox steps
        # after its ten-minute default suite. Each backend now runs them in a
        # second job beside the suite, on the same runner pool and with the
        # same OpenVMM and nvx builds. Of the two, only that job reads the
        # Ubuntu and Azure Linux artifacts, so only it downloads and waits for
        # them. Together, the two jobs run every step of the former single job
        # exactly once, and the debug-kernel jobs, which never set the
        # selector, run as before.
        workflows = BuildConstants.REPO_ROOT / ".github" / "workflows"
        jobs = yaml.safe_load((workflows / "ci.yml").read_text(encoding="utf-8"))[
            "jobs"
        ]
        microvm = yaml.safe_load(
            (workflows / "run-nvx-microvm-tests.yml").read_text(encoding="utf-8")
        )
        # PyYAML reads the `on` key as true.
        selector = microvm[True]["workflow_call"]["inputs"]["guests-and-sandboxes"]
        self.assertEqual(selector["type"], "boolean")
        self.assertIs(selector["default"], False)
        backends = {
            "kvm": "Linux / KVM",
            "mshv": "Linux / MSHV",
            "whp": "Windows / WHP",
        }
        self.assertEqual(
            {
                name
                for name, job in jobs.items()
                if job.get("uses") == "./.github/workflows/run-nvx-microvm-tests.yml"
            },
            {
                f"nvx-microvm-{kind}-{backend}"
                for kind in ("tests", "guests", "debug")
                for backend in backends
            },
        )
        for backend, label in backends.items():
            suite = jobs[f"nvx-microvm-tests-{backend}"]
            guests = jobs[f"nvx-microvm-guests-{backend}"]
            debug = jobs[f"nvx-microvm-debug-{backend}"]
            name = f"{label} / Guests and sandboxes"
            shard = "Shard ${{ matrix.shard }} of ${{ strategy.job-total }}"
            with self.subTest(backend=backend):
                self.assertEqual(
                    suite["name"], f"NVX microVM tests / {label} / {shard}"
                )
                self.assertEqual(suite["needs"][0], "artifacts")
                self.assertNotIn("distro-artifacts", suite["needs"])
                # The guest and sandbox job also waits for the Ubuntu and Azure
                # Linux artifacts, and otherwise equals the suite's job but for
                # its name, the selector, and the suite's shards
                # (test_ci_runs_each_default_microvm_suite_in_its_shards).
                conjuncts = _expression_conjuncts(suite["if"])
                after = conjuncts.index("needs.artifacts.result == 'success'") + 1
                unsharded = {
                    key: value for key, value in suite.items() if key != "strategy"
                }
                inputs = {
                    key: value
                    for key, value in suite["with"].items()
                    if key not in ("shard", "shard-count")
                }
                self.assertEqual(
                    guests | {"if": _expression_conjuncts(guests["if"])},
                    unsharded
                    | {
                        "name": f"NVX microVM tests / {name}",
                        "needs": ["artifacts", "distro-artifacts", *suite["needs"][1:]],
                        "if": [
                            *conjuncts[:after],
                            "needs.distro-artifacts.result == 'success'",
                            *conjuncts[after:],
                        ],
                        "with": inputs
                        | {"display-name": name, "guests-and-sandboxes": True},
                    },
                )
                self.assertNotIn("guests-and-sandboxes", suite["with"])
                self.assertNotIn("guests-and-sandboxes", debug["with"])
                self.assertIs(debug["with"]["debug-kernel"], True)

        steps = microvm["jobs"]["test"]["steps"]
        names = [step["name"] for step in steps]
        setup = names[: names.index("Validate runner") + 1]
        downloads = {
            step["name"]: step["with"]["name"]
            for step in steps
            if str(step.get("uses", "")).startswith("actions/download-artifact@")
            and str(step["with"]["name"]).startswith("guest-")
        }
        uploads = [
            step
            for step in steps
            if str(step.get("uses", "")).startswith("actions/upload-artifact@")
        ]

        def selected(backend: str, *, debug: bool, guests: bool | None) -> list[str]:
            # A selector of None stands for the single job that ran every step
            # before the split: it ignores the guests-and-sandboxes conjuncts.
            # Only the default-suite jobs run a shard; the single job, the
            # guest and sandbox jobs, and the debug-kernel jobs pass none.
            windows = backend == "whp"
            sharded = guests is False and not debug
            values = {
                "runner.os != 'Windows'": not windows,
                "runner.os == 'Windows'": windows,
                "inputs.backend == 'kvm'": backend == "kvm",
                "inputs.debug-kernel": debug,
                "!inputs.debug-kernel": not debug,
                "inputs.guests-and-sandboxes": guests is not False,
                "!inputs.guests-and-sandboxes": guests is not True,
                "!inputs.shard": not sharded,
            }
            chosen: list[str] = []
            for step in steps:
                if step in uploads:
                    continue
                if "if" not in step:
                    chosen.append(step["name"])
                    continue
                conjuncts = _expression_conjuncts(str(step["if"]))
                for conjunct in conjuncts:
                    self.assertIn(conjunct, values, step["name"])
                if all(values[conjunct] for conjunct in conjuncts):
                    chosen.append(step["name"])
            return chosen

        def downloaded(selection: list[str]) -> set[str]:
            return {downloads[name] for name in selection if name in downloads}

        for backend in backends:
            system = "Windows" if backend == "whp" else "Linux"
            single = selected(backend, debug=False, guests=None)
            suite_steps = selected(backend, debug=False, guests=False)
            guest_steps = selected(backend, debug=False, guests=True)
            with self.subTest(backend=backend):
                # Both jobs take the same builds and validate the runner the
                # same way, and every other step of the single job runs in
                # exactly one of them. Each downloads the guest artifacts that
                # its steps read (test_ci_jobs_download_exactly_the_guest_
                # artifacts_that_they_read): the default suite only the kernel
                # and the Alpine initramfs, and the guest and sandbox steps
                # every guest artifact that the single job downloaded.
                for step_name in setup:
                    if step_name in downloads:
                        continue
                    self.assertEqual(step_name in suite_steps, step_name in single)
                    self.assertEqual(step_name in guest_steps, step_name in single)
                self.assertEqual(
                    downloaded(suite_steps), {"guest-kernel", "guest-alpine"}
                )
                self.assertEqual(downloaded(guest_steps), downloaded(single))
                self.assertEqual(
                    sorted(
                        step_name
                        for step_name in suite_steps + guest_steps
                        if step_name not in setup
                    ),
                    sorted(step_name for step_name in single if step_name not in setup),
                )
                self.assertEqual(
                    [step_name for step_name in suite_steps if step_name not in setup],
                    [f"Run NVX microVM tests on {system}"],
                )
                for step_name in (
                    f"Run public managed sandbox acceptance on {system}",
                    f"Run Ubuntu guest smoke tests on {system}",
                    f"Run Azure Linux guest smoke tests on {system}",
                    f"Run Ubuntu sandbox layer smoke test on {system}",
                ):
                    self.assertIn(step_name, guest_steps)
                self.assertEqual(
                    "Run extended Ubuntu guest tests on Linux/KVM" in guest_steps,
                    backend == "kvm",
                )
                self.assertEqual(
                    selected(backend, debug=True, guests=False),
                    selected(backend, debug=True, guests=None),
                )

        # A second upload of an artifact name in a run fails, so each shard of
        # the default suite, the guest and sandbox job, and the debug-kernel
        # job of every backend upload under names of their own.
        defaults = {
            name: definition.get("default")
            for name, definition in microvm[True]["workflow_call"]["inputs"].items()
        }
        legs = [
            {**defaults, **leg}
            for job in jobs.values()
            if job.get("uses") == "./.github/workflows/run-nvx-microvm-tests.yml"
            for leg in _matrix_legs(job)
        ]
        self.assertTrue(uploads)
        for upload in uploads:
            uploaded = [_resolve_inputs(upload["with"]["name"], leg) for leg in legs]
            with self.subTest(upload=upload["name"]):
                self.assertNotIn("inputs.", " ".join(uploaded))
                self.assertEqual(len(set(uploaded)), len(uploaded), uploaded)
        failure_logs = next(
            step
            for step in uploads
            if step["name"] == "Upload NVX microVM failure logs"
        )
        shards = len(microvm_shards.load_partition().backends["kvm"].shards)
        self.assertEqual(
            sorted(
                _resolve_inputs(failure_logs["with"]["name"], leg)
                for leg in legs
                if leg["backend"] == "kvm"
            ),
            [
                "nvx-microvm-tests-kvm-debug",
                "nvx-microvm-tests-kvm-guests",
                *(
                    f"nvx-microvm-tests-kvm-shard-{index}"
                    for index in range(1, shards + 1)
                ),
            ],
        )

    def test_ci_runs_each_default_microvm_suite_in_its_shards(self):
        # #696: each backend runs its default suite in the shards that
        # microvm_shards.json plans, one leg of the default-suite job's matrix
        # each, so that the WHP suite no longer bounds the runs that skip both
        # OpenVMM suites. The legs run every shard once, so every default
        # scenario runs exactly once per backend, on the Alpine guest and the
        # default kernel with every processor count, and the job's result,
        # which the required status check, the release, and the persistence
        # read, fails when a shard fails. Neither the guest and sandbox jobs
        # nor the debug-kernel jobs are sharded.
        workflows = BuildConstants.REPO_ROOT / ".github" / "workflows"
        jobs = yaml.safe_load((workflows / "ci.yml").read_text(encoding="utf-8"))[
            "jobs"
        ]
        microvm = yaml.safe_load(
            (workflows / "run-nvx-microvm-tests.yml").read_text(encoding="utf-8")
        )
        inputs = microvm[True]["workflow_call"]["inputs"]
        for name in ("shard", "shard-count"):
            with self.subTest(input=name):
                self.assertEqual(inputs[name]["type"], "number")
                self.assertEqual(inputs[name]["default"], 0)
                self.assertIs(inputs[name]["required"], False)
        defaults = {
            name: definition.get("default") for name, definition in inputs.items()
        }
        steps = microvm["jobs"]["test"]["steps"]
        sharded = {
            step["name"]: str(step["run"])
            for step in steps
            if "inputs.shard" in str(step.get("run", ""))
        }
        self.assertEqual(
            set(sharded),
            {"Run NVX microVM tests on Linux", "Run NVX microVM tests on Windows"},
        )
        partition = microvm_shards.load_partition()
        suite = default_suite()
        shard = "Shard ${{ matrix.shard }} of ${{ strategy.job-total }}"
        backends = {
            "kvm": "Linux / KVM",
            "mshv": "Linux / MSHV",
            "whp": "Windows / WHP",
        }
        for backend, label in backends.items():
            job = jobs[f"nvx-microvm-tests-{backend}"]
            count = len(partition.backends[backend].shards)
            system = "Windows" if backend == "whp" else "Linux"
            with self.subTest(backend=backend):
                # A failed shard does not cancel the others, which keep their
                # logs and durations; GitHub expands the matrix after it
                # evaluates the condition, so the shards run together or not
                # at all.
                self.assertEqual(
                    job["strategy"],
                    {
                        "fail-fast": False,
                        "matrix": {"shard": list(range(1, count + 1))},
                    },
                )
                self.assertNotIn("matrix.", job["if"])
                self.assertEqual(job["name"], f"NVX microVM tests / {label} / {shard}")
                self.assertEqual(job["with"]["display-name"], f"{label} / {shard}")
                self.assertEqual(job["with"]["shard"], "${{ matrix.shard }}")
                self.assertEqual(
                    job["with"]["shard-count"], "${{ strategy.job-total }}"
                )
                for kind in ("guests", "debug"):
                    other = jobs[f"nvx-microvm-{kind}-{backend}"]
                    self.assertNotIn("strategy", other)
                    self.assertFalse({"shard", "shard-count"} & set(other["with"]))

                legs = [{**defaults, **leg} for leg in _matrix_legs(job)]
                self.assertEqual(
                    [(leg["shard"], leg["shard-count"]) for leg in legs],
                    [(index, count) for index in range(1, count + 1)],
                )
                ran: list[str] = []
                for leg in legs:
                    command = _resolve_inputs(
                        sharded[f"Run NVX microVM tests on {system}"], leg
                    )
                    self.assertNotIn("${{", command)
                    args = nvx.parse_args(
                        shlex.split(command.split("nvx.py ", 1)[1].replace("\\", "/"))
                    )
                    self.assertEqual(
                        (args.backend, args.shard, args.scenario, args.debug_kernel),
                        (backend, (leg["shard"], count), None, False),
                    )
                    self.assertEqual(args.guest, "alpine")
                    self.assertEqual(args.processors, list(MICROVM_PROCESSOR_COUNTS))
                    ran.extend(
                        microvm_shards.shard_scenarios(
                            partition, backend, args.shard, suite
                        )
                    )
                self.assertEqual(sorted(ran), sorted(suite))
        # The guest and sandbox and debug-kernel jobs run test-microvm without
        # a shard.
        for kind in ("guests", "debug"):
            for backend in backends:
                leg = {**defaults, **jobs[f"nvx-microvm-{kind}-{backend}"]["with"]}
                for step_name, run in sharded.items():
                    with self.subTest(job=f"{kind}-{backend}", step=step_name):
                        self.assertNotIn("--shard", _resolve_inputs(run, leg))

    def test_benchmarks_rely_on_the_microvm_correctness_jobs(self):
        # #286: the benchmark action ran a second smp-lapic gate before
        # acceptance. The microVM correctness jobs already gate the required
        # status, the development release, and performance persistence.
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")

        self.assertNotIn("test-microvm", action)
        self.assertNotIn("lapic-correctness", action)
        for job_name in ci.REQUIRED_CI_MICROVM_TEST_JOBS:
            with self.subTest(job=job_name):
                self.assertIn(job_name, ci.REQUIRED_CI_RESULT_ENVIRONMENTS)
                for consumer in ("release", "performance-record"):
                    job = _workflow_job(workflow, consumer)
                    self.assertIn(f"      - {job_name}\n", job)
                    self.assertIn(
                        f"needs.{job_name}.result == 'success' ||\n"
                        f"          needs.{job_name}.result == 'skipped'",
                        job,
                    )

    def test_ci_checks_openvmm_ci_instead_of_running_openvmm_tests(self):
        workflow_text = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        workflow = yaml.safe_load(workflow_text)
        # OpenVMM's own checkin gates test the pinned revision
        # (doc/ci.md#openvmm-ci), so no job builds or runs OpenVMM's tests.
        for command in (
            "test-openvmm",
            "test-openvmm-unit",
            "build-openvmm-tests",
            "check-openvmm-tests",
            "reuse-openvmm-tests",
            "openvmm-test-cache-key",
        ):
            with self.subTest(command=command):
                self.assertNotRegex(workflow_text, rf"nvx\.py {command}(?![\w-])")
        job = workflow["jobs"]["openvmm-ci"]
        text = _workflow_job(workflow_text, "openvmm-ci")
        self.assertEqual(job["name"], "OpenVMM CI of the pinned revision")
        self.assertEqual(job["needs"], ["openvmm-changes"])
        self.assertEqual(job["runs-on"], "ubuntu-latest")
        self.assertIn(
            "needs.openvmm-changes.outputs.run-openvmm-ci-check == 'true'", text
        )
        # It reads only the public OpenVMM repository's runs, so it needs
        # neither an OpenVMM checkout nor a secret, and a pull request from a
        # fork runs it too.
        self.assertNotIn("secrets.", text)
        self.assertNotIn("checkout-openvmm", text)
        self.assertNotIn("head.repo.full_name", text)
        checkout, check = job["steps"]
        self.assertRegex(checkout["uses"], r"^actions/checkout@")
        self.assertEqual(checkout["with"], {"persist-credentials": False})
        self.assertEqual(check["env"]["GH_TOKEN"], "${{ github.token }}")
        self.assertEqual(
            check["run"],
            'python3 scripts/nvx.py check-openvmm-ci --wait-minutes "${WAIT_MINUTES}" '
            '--summary "${GITHUB_STEP_SUMMARY}"',
        )
        # A pull request, whose pin promotion passes once OpenVMM's CI does,
        # and a manual run wait for the gates; the other events check pins
        # that passed before they merged, and the dev pushes and merge groups
        # that queue behind them must not wait for theirs.
        waits = {
            event: _interpolate_expressions(
                check["env"]["WAIT_MINUTES"], {"github": {"event_name": event}}, {}
            )
            for event in ci.CI_EVENTS
        }
        self.assertEqual(
            waits,
            {
                "pull_request": "300",
                "merge_group": "0",
                "push": "0",
                "schedule": "0",
                "workflow_dispatch": "300",
            },
        )
        self.assertGreater(job["timeout-minutes"], 300)

    def test_ci_measures_each_performance_series_on_its_benchmark_runner(self):
        # Each backend's pool mixes VMs whose results differ by more than the
        # run-to-run noise (#383, #415). The platform jobs, and only they,
        # require the label of the one runner per pool that measures a series.
        github = BuildConstants.REPO_ROOT / ".github"
        workflow = (github / "workflows" / "ci.yml").read_text(encoding="utf-8")
        platform_workflow = (github / "workflows" / "run-platform.yml").read_text(
            encoding="utf-8"
        )
        pools = {
            "platform-kvm": ("linux", "kvm"),
            "platform-mshv": ("linux", "mshv"),
            "platform-whp": ("windows", "whp"),
        }
        self.assertEqual(set(pools), set(ci.REQUIRED_CI_PLATFORM_JOBS))
        jobs = re.findall(
            r"^  ([a-z0-9-]+):$", workflow.split("\njobs:\n", 1)[1], re.MULTILINE
        )
        self.assertLessEqual(set(pools), set(jobs))
        for job_name in jobs:
            job = _workflow_job(workflow, job_name)
            runners = [
                json.loads(labels)
                for labels in re.findall(r"^ +runner: '(.+)'$", job, re.MULTILINE)
            ]
            with self.subTest(job=job_name):
                if job_name in pools:
                    operating_system, backend = pools[job_name]
                    self.assertIn(
                        "    uses: ./.github/workflows/run-platform.yml\n", job
                    )
                    self.assertEqual(
                        runners,
                        [
                            [
                                "self-hosted",
                                operating_system,
                                backend,
                                "virtual-machine",
                                "benchmark",
                            ]
                        ],
                    )
                else:
                    self.assertNotIn("run-platform.yml", job)
                    for labels in runners:
                        self.assertNotIn("benchmark", labels)
                        if {"kvm", "mshv", "whp"} & set(labels):
                            self.assertIn("general", labels)
        self.assertIn(
            "    runs-on: ${{ fromJSON(inputs.runner) }}\n", platform_workflow
        )
        self.assertIn(
            "    - benchmark\n",
            (github / "actionlint.yaml").read_text(encoding="utf-8"),
        )
        self.assertIn(
            "    - general\n",
            (github / "actionlint.yaml").read_text(encoding="utf-8"),
        )

    def test_backend_workflow_runner_roles_are_exclusive(self):
        workflows = BuildConstants.REPO_ROOT / ".github" / "workflows"
        platform_jobs = {"platform-kvm", "platform-mshv", "platform-whp"}
        observed_platform_jobs: set[str] = set()
        observed_general_jobs: set[str] = set()

        for path in sorted(workflows.glob("*.yml")):
            parsed = yaml.safe_load(path.read_text(encoding="utf-8"))
            for job_name, job in parsed.get("jobs", {}).items():
                selectors: list[list[str]] = []
                runs_on = job.get("runs-on")
                if isinstance(runs_on, list):
                    selectors.append(cast(list[str], runs_on))
                runner_input = job.get("with", {}).get("runner")
                if isinstance(runner_input, str) and runner_input.startswith("["):
                    selectors.append(json.loads(runner_input))
                for leg in job.get("strategy", {}).get("matrix", {}).get("include", []):
                    runner = leg.get("runner")
                    if isinstance(runner, str) and runner.startswith("["):
                        selectors.append(json.loads(runner))

                for labels in selectors:
                    backends = {"kvm", "mshv", "whp"} & set(labels)
                    if not backends:
                        continue
                    with self.subTest(workflow=path.name, job=job_name, labels=labels):
                        self.assertEqual(len(backends), 1)
                        self.assertIn("self-hosted", labels)
                        self.assertIn("virtual-machine", labels)
                        if path.name == "ci.yml" and job_name in platform_jobs:
                            observed_platform_jobs.add(job_name)
                            self.assertEqual(
                                job.get("uses"),
                                "./.github/workflows/run-platform.yml",
                            )
                            self.assertIn("benchmark", labels)
                            self.assertNotIn("general", labels)
                        else:
                            observed_general_jobs.add(f"{path.name}:{job_name}")
                            self.assertIn("general", labels)
                            self.assertNotIn("benchmark", labels)

        self.assertEqual(observed_platform_jobs, platform_jobs)
        self.assertTrue(observed_general_jobs)

    def test_ci_validates_the_cli_once_per_os_after_checking_out_openvmm(self):
        # The CLI tests read OpenVMM's pinned CPU profiles from the submodule,
        # which a runner's workspace holds at the pinned revision only after
        # the job checks it out. The tests do not depend on the backend, so
        # one GitHub-hosted job per OS runs them instead of every hypervisor
        # job, with the nvx that the build job of its OS hands over. No
        # hypervisor job waits for them.
        workflows = BuildConstants.REPO_ROOT / ".github" / "workflows"
        workflow = (workflows / "ci.yml").read_text(encoding="utf-8")
        self.assertEqual(ci.REQUIRED_CI_CLI_JOBS, ("nvx-cli-linux", "nvx-cli-windows"))
        for job_name, runner, nvx_build in (
            ("nvx-cli-linux", "ubuntu-latest", "build-nvx-linux-musl"),
            ("nvx-cli-windows", "windows-latest", "build-nvx-windows-msvc"),
        ):
            with self.subTest(job=job_name):
                job = _workflow_job(workflow, job_name)
                self.assertIn(f"    runs-on: {runner}\n", job)
                self.assertIn(f"    needs: [{nvx_build}]\n", job)
                self.assertIn(
                    "github.event.pull_request.head.repo.full_name "
                    "== github.repository",
                    job,
                )
                self.assertEqual(job.count("uses: ./.github/actions/validate-nvx"), 1)
                for prerequisite in (
                    "uses: ./.github/actions/checkout-openvmm",
                    "uses: actions/setup-python@",
                    "rustup toolchain install",
                ):
                    self.assertLess(
                        job.index(prerequisite),
                        job.index("uses: ./.github/actions/validate-nvx"),
                    )
                # The release that the self-hosted runners provide, and the
                # Rust release that rust-toolchain.toml pins, with which the
                # curl shim test and the comparisons of OpenVMM's data build.
                self.assertIn('python-version: "3.12"', job)
                self.assertIn("rust-toolchain.toml", job)
                self.assertIn("--profile minimal --no-self-update", job)
                # It hands nothing over: it tests the build of its OS.
                self.assertNotIn("- name: Build nvx", job)
                self.assertNotIn("uses: actions/upload-artifact", job)
                self.assertIn(job_name, ci.REQUIRED_CI_RESULT_ENVIRONMENTS)

        configurations = {
            name: (workflows / name).read_text(encoding="utf-8")
            for name in (
                "ci.yml",
                "build-openvmm-binary.yml",
                "run-nvx-microvm-tests.yml",
                "run-platform.yml",
            )
        }
        self.assertEqual(
            sum(
                configuration.count("uses: ./.github/actions/validate-nvx")
                for configuration in configurations.values()
            ),
            len(ci.REQUIRED_CI_CLI_JOBS),
        )
        # A hypervisor job waits for the nvx build that it uses, if any,
        # but never for the CLI tests, so that it starts as soon as the build
        # is handed over.
        for job_name in (
            *ci.REQUIRED_CI_MICROVM_TEST_JOBS,
            *ci.REQUIRED_CI_MICROVM_DEBUG_JOBS,
            *ci.REQUIRED_CI_MICROVM_DEBUG_PUSH_JOBS,
            *ci.REQUIRED_CI_PLATFORM_JOBS,
        ):
            with self.subTest(job=job_name):
                self.assertNotIn("nvx-cli", _workflow_job(workflow, job_name))
        self.assertNotIn(
            "nvx-cli", _workflow_job(workflow, ci.REQUIRED_CI_OPENVMM_CI_JOB)
        )
        for consumer in ("release", "performance-record", "required-status-check"):
            job = _workflow_job(workflow, consumer)
            for cli in ci.REQUIRED_CI_CLI_JOBS:
                with self.subTest(consumer=consumer, cli=cli):
                    self.assertIn(f"      - {cli}\n", job)
                    if consumer != "required-status-check":
                        self.assertIn(
                            f"        needs.{cli}.result == 'success' &&\n", job
                        )

    def test_ci_compares_the_copies_of_openvmm_data_in_the_cli_jobs(self):
        # nvx's protocol tests read copies of OpenVMM's control-session
        # protocol vectors, and nvx run a copy of the identities of its
        # built-in CPU profiles, because the nvx job cannot check out
        # OpenVMM. Each NVX CLI job checks it out, and runs the ignored tests
        # that compare the copies with the submodule's files.
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        for step_name, test_name in (
            ("Compare OpenVMM's protocol vectors", "openvmm_vectors"),
            ("Compare OpenVMM's CPU profiles", "openvmm_cpu_profiles"),
        ):
            test = BuildConstants.REPO_ROOT / "nvx" / "tests" / f"{test_name}.rs"
            self.assertIn("#[ignore", test.read_text(encoding="utf-8"))
            for job_name in ci.REQUIRED_CI_CLI_JOBS:
                with self.subTest(job=job_name, step=step_name):
                    job = _workflow_job(workflow, job_name)
                    step = _workflow_step(workflow, job_name, step_name)
                    command = " ".join(
                        _yaml_scalar(step, "run").replace("`", " ").split()
                    )
                    for argument in (
                        "cargo test --locked",
                        "--manifest-path nvx/Cargo.toml",
                        f"--test {test_name} -- --ignored",
                    ):
                        self.assertIn(argument, command)
                    for prerequisite in (
                        "uses: ./.github/actions/checkout-openvmm",
                        "rustup toolchain install",
                    ):
                        self.assertLess(
                            job.index(prerequisite),
                            job.index(f"- name: {step_name}"),
                        )

    def test_ci_hands_nvx_to_the_microvm_and_package_jobs(self):
        # The microVM scenarios and steps run nvx, and find it where
        # `cargo build --release` puts it. One GitHub-hosted job per OS builds
        # it, records its provenance, and hands both to the microVM jobs,
        # which run it, to the package jobs, which package it for each
        # platform, and to the NVX CLI job of its OS, which tests with it, as
        # the OpenVMM build jobs hand over OpenVMM. The platform jobs, which
        # only measure, don't take it.
        github = BuildConstants.REPO_ROOT / ".github"
        workflow = (github / "workflows" / "ci.yml").read_text(encoding="utf-8")
        microvm = (github / "workflows" / "run-nvx-microvm-tests.yml").read_text(
            encoding="utf-8"
        )
        platform = (github / "workflows" / "run-platform.yml").read_text(
            encoding="utf-8"
        )
        packaging = (github / "workflows" / "package-platform.yml").read_text(
            encoding="utf-8"
        )
        package = (github / "actions" / "package-release" / "action.yml").read_text(
            encoding="utf-8"
        )
        builds = {
            "build-nvx-linux-musl": (
                "nvx-cli-linux",
                "nvx-linux-musl",
                "nvx/target/release/nvx",
                "x86_64-unknown-linux-musl",
            ),
            "build-nvx-windows-msvc": (
                "nvx-cli-windows",
                "nvx-windows-msvc",
                "nvx/target/release/nvx.exe",
                "x86_64-pc-windows-msvc",
            ),
        }
        self.assertEqual(ci.REQUIRED_CI_NVX_BUILD_JOBS, tuple(builds))
        current = builds[
            "build-nvx-windows-msvc" if os.name == "nt" else "build-nvx-linux-musl"
        ]
        self.assertEqual(
            nvx_cli.binary_path().relative_to(BuildConstants.REPO_ROOT).as_posix(),
            current[2],
        )
        # Packages ship the build for the release target of their OS.
        self.assertEqual(nvx_cli.release_target(), current[3])
        self.assertEqual(
            set(nvx_cli.RELEASE_TARGETS.values()),
            {target for *_, target in builds.values()},
        )
        for job_name, (cli, artifact, path, target) in builds.items():
            with self.subTest(job=job_name):
                job = _workflow_job(workflow, job_name)
                self.assertIn(job_name, ci.REQUIRED_CI_RESULT_ENVIRONMENTS)
                # It waits for no job, and runs whenever the CLI job does.
                self.assertNotIn("    needs:", job)
                self.assertIn(
                    "github.event.pull_request.head.repo.full_name "
                    "== github.repository",
                    job,
                )
                build = _workflow_step(workflow, job_name, "Build nvx")
                record = _workflow_step(workflow, job_name, "Record nvx provenance")
                upload = _workflow_step(workflow, job_name, "Upload nvx executable")
                provenance = _workflow_step(workflow, job_name, "Upload nvx provenance")
                for option in ("--release", "--locked", "--features cli"):
                    self.assertIn(option, build)
                self.assertIn("--manifest-path nvx/Cargo.toml", build)
                self.assertIn(
                    f"record-nvx-provenance --target {target}",
                    _yaml_scalar(record, "run"),
                )
                self.assertIn(f"name: {artifact}-executable\n", upload)
                self.assertIn(f"path: {path}\n", upload)
                self.assertIn(f"name: {artifact}-provenance\n", provenance)
                self.assertIn(f"path: build/{nvx_cli.PROVENANCE_NAME}\n", provenance)
                for step in (upload, provenance):
                    self.assertIn("if-no-files-found: error", step)
                # The build is recorded as it is built, and leaves the job
                # with its provenance without waiting for any test.
                self.assertLess(
                    job.index("- name: Build nvx"),
                    job.index("- name: Record nvx provenance"),
                )
                for step_name in (
                    "Upload nvx executable",
                    "Upload nvx provenance",
                ):
                    self.assertLess(
                        job.index("- name: Record nvx provenance"),
                        job.index(f"- name: {step_name}"),
                    )
                for unrelated in ("validate-nvx", "checkout-openvmm", "cargo test"):
                    self.assertNotIn(unrelated, job)

                # The CLI job of its OS tests with the build that it hands
                # over, which it downloads where the build put it, with its
                # provenance, before the tests.
                cli_job = _workflow_job(workflow, cli)
                validate = cli_job.index("uses: ./.github/actions/validate-nvx")
                executable = _workflow_step(workflow, cli, "Download nvx executable")
                record_download = _workflow_step(
                    workflow, cli, "Download nvx provenance"
                )
                self.assertIn(f"name: {artifact}-executable\n", executable)
                self.assertIn("path: nvx/target/release\n", executable)
                self.assertIn(f"name: {artifact}-provenance\n", record_download)
                self.assertIn("path: build\n", record_download)
                for step_name in (
                    "Download nvx executable",
                    "Download nvx provenance",
                ):
                    self.assertLess(cli_job.index(f"- name: {step_name}"), validate)
        # Self-contained executables run on every self-hosted runner.
        linux = _workflow_step(workflow, "build-nvx-linux-musl", "Build nvx")
        self.assertEqual(linux.count("x86_64-unknown-linux-musl"), 3)
        self.assertIn("rust-toolchain.toml", linux)
        self.assertIn(
            "install -D -m 0755 nvx/target/x86_64-unknown-linux-musl/release/"
            "nvx \\\n            nvx/target/release/nvx",
            linux,
        )
        windows = _workflow_job(workflow, "build-nvx-windows-msvc")
        windows_build = _workflow_step(workflow, "build-nvx-windows-msvc", "Build nvx")
        self.assertIn("RUSTFLAGS: -C target-feature=+crt-static", windows_build)
        self.assertLess(
            windows.index("- name: Install the pinned Rust release"),
            windows.index("- name: Build nvx"),
        )
        # The tests that run nvx directly need NVX to name the build,
        # which loses its executable permission on its way to Linux.
        linux_cli = _workflow_job(workflow, "nvx-cli-linux")
        self.assertIn(
            "    env:\n      NVX: ${{ github.workspace }}/nvx/target/release/nvx\n",
            linux_cli,
        )
        self.assertIn(
            "    env:\n"
            "      NVX: ${{ github.workspace }}\\nvx\\target\\release\\"
            "nvx.exe\n",
            _workflow_job(workflow, "nvx-cli-windows"),
        )
        self.assertIn(
            "run: chmod +x nvx/target/release/nvx\n",
            _workflow_step(workflow, "nvx-cli-linux", "Make nvx executable"),
        )
        self.assertLess(
            linux_cli.index("- name: Download nvx executable"),
            linux_cli.index("- name: Make nvx executable"),
        )
        self.assertLess(
            linux_cli.index("- name: Make nvx executable"),
            linux_cli.index("uses: ./.github/actions/validate-nvx"),
        )

        for job_name in (
            *ci.REQUIRED_CI_MICROVM_TEST_JOBS,
            *ci.REQUIRED_CI_MICROVM_DEBUG_JOBS,
            *ci.REQUIRED_CI_MICROVM_DEBUG_PUSH_JOBS,
            *ci.REQUIRED_CI_PACKAGE_JOBS,
        ):
            nvx_build = (
                "build-nvx-windows-msvc"
                if job_name.endswith("-whp")
                else "build-nvx-linux-musl"
            )
            with self.subTest(consumer=job_name):
                job = _workflow_job(workflow, job_name)
                self.assertIn(
                    "uses: ./.github/workflows/package-platform.yml"
                    if job_name in ci.REQUIRED_CI_PACKAGE_JOBS
                    else "uses: ./.github/workflows/run-nvx-microvm-tests.yml",
                    job,
                )
                needs = re.search(r"^    needs: \[(.+)\]$", job, re.MULTILINE)
                assert needs is not None
                self.assertIn(nvx_build, needs.group(1).split(", "))
                self.assertIn(
                    f"        needs.{nvx_build}.result == 'success' &&\n", job
                )
                for other in set(ci.REQUIRED_CI_NVX_BUILD_JOBS) - {nvx_build}:
                    self.assertNotIn(other, job)
                self.assertEqual(
                    re.findall(r"^      nvx-artifact: (\S+)$", job, re.MULTILINE),
                    [builds[nvx_build][1]],
                )
        # The platform jobs neither build nor download nvx.
        cli = r"build-nvx-|nvx-(artifact|linux-musl|windows-msvc)|nvx/"
        for job_name in ci.REQUIRED_CI_PLATFORM_JOBS:
            with self.subTest(platform=job_name):
                self.assertNotRegex(_workflow_job(workflow, job_name), cli)
        self.assertNotRegex(platform, cli)
        # Only a pushed commit whose builds succeeded is released or recorded.
        for consumer in ("release", "performance-record", "required-status-check"):
            job = _workflow_job(workflow, consumer)
            for nvx_build in ci.REQUIRED_CI_NVX_BUILD_JOBS:
                with self.subTest(consumer=consumer, build=nvx_build):
                    self.assertIn(f"      - {nvx_build}\n", job)
                    if consumer != "required-status-check":
                        self.assertIn(
                            f"        needs.{nvx_build}.result == 'success' &&\n",
                            job,
                        )

        for configuration in (microvm, packaging):
            self.assertIn(
                "      nvx-artifact:\n"
                "        description: Base name of the nvx artifacts for the "
                "runner's OS\n"
                "        required: true\n"
                "        type: string\n",
                configuration,
            )
        download = microvm.index("      - name: Download nvx executable\n")
        self.assertRegex(
            microvm[download:],
            r"        uses: actions/download-artifact@[^\n]+\n"
            + re.escape(
                "        with:\n"
                "          name: ${{ inputs.nvx-artifact }}-executable\n"
                "          path: nvx/target/release\n"
            ),
        )
        chmod = microvm.index("      - name: Make nvx executable\n")
        self.assertIn(
            "        if: runner.os != 'Windows'\n"
            "        shell: bash\n"
            "        run: chmod +x nvx/target/release/nvx\n",
            microvm[chmod:],
        )
        self.assertLess(download, chmod)
        self.assertLess(chmod, microvm.index("scripts/nvx.py"))

        # The package jobs package nvx with its provenance: they download
        # both where nvx.py package looks for them, before they package.
        self.assertIn(
            "          nvx-artifact: ${{ inputs.nvx-artifact }}",
            _workflow_step(packaging, "package", "Package release").splitlines(),
        )
        steps = _composite_action_steps(package)
        names = [_yaml_scalar(step, "name") for step in steps]
        executable = steps[names.index("Download nvx executable")]
        provenance = steps[names.index("Download nvx provenance")]
        self.assertIn("name: ${{ inputs.nvx-artifact }}-executable\n", executable)
        self.assertIn("path: nvx/target/release\n", executable)
        self.assertIn("name: ${{ inputs.nvx-artifact }}-provenance\n", provenance)
        self.assertIn("path: build\n", provenance)
        self.assertIn(
            "run: chmod +x nvx/target/release/nvx",
            steps[names.index("Make nvx executable")],
        )
        for name in ("Package Linux release", "Package Windows release"):
            self.assertLess(names.index("Download nvx executable"), names.index(name))
            self.assertLess(names.index("Download nvx provenance"), names.index(name))
            self.assertLess(names.index("Make nvx executable"), names.index(name))

    def test_pull_requests_package_releases_that_only_dev_uploads(self):
        # Pull requests package and archive each platform's release, which
        # proves that it builds; only a dev push uploads it for publication.
        github = BuildConstants.REPO_ROOT / ".github"
        packaging = (github / "workflows" / "package-platform.yml").read_text(
            encoding="utf-8"
        )
        package = (github / "actions" / "package-release" / "action.yml").read_text(
            encoding="utf-8"
        )
        step = _workflow_step(packaging, "package", "Package release")
        self.assertNotIn("        if:", step)
        self.assertIn(
            "          upload: ${{ github.event_name == 'push' && "
            "github.ref == 'refs/heads/dev' }}",
            step,
        )
        steps = _composite_action_steps(package)
        names = [_yaml_scalar(step, "name") for step in steps]
        for name, operating_system in (
            ("Upload Linux release", "runner.os != 'Windows'"),
            ("Upload Windows release", "runner.os == 'Windows'"),
        ):
            with self.subTest(step=name):
                self.assertEqual(
                    _yaml_scalar(steps[names.index(name)], "if"),
                    f"{operating_system} && inputs.upload == 'true'",
                )
        for name in ("Package Linux release", "Package Windows release"):
            self.assertNotIn("upload", _yaml_scalar(steps[names.index(name)], "if"))
            self.assertIn("--binary-only", steps[names.index(name)])
            self.assertIn("archive-release", steps[names.index(name)])

    def test_ci_packages_each_platform_beside_its_benchmarks(self):
        # Packaging takes no hypervisor, so a GitHub-hosted job of the
        # platform's OS packages each platform's release from the OpenVMM
        # build that its platform job measures, beside that job. The platform
        # job waits only for the kernel and the Alpine initramfs, which it
        # boots, and its OpenVMM producer, and not for the Ubuntu and Azure
        # Linux images or the nvx that only the package ships, so that its
        # benchmark runner starts as soon as it can (#982).
        github = BuildConstants.REPO_ROOT / ".github"
        workflow = (github / "workflows" / "ci.yml").read_text(encoding="utf-8")
        jobs = yaml.safe_load(workflow)["jobs"]
        packaging = (github / "workflows" / "package-platform.yml").read_text(
            encoding="utf-8"
        )
        platforms = {
            "platform-kvm": (
                "package-kvm",
                "linux-kvm",
                "ubuntu-latest",
                "build-openvmm-linux-gnu",
                "openvmm-linux-gnu",
                "build-nvx-linux-musl",
            ),
            "platform-mshv": (
                "package-mshv",
                "linux-mshv",
                "ubuntu-latest",
                "build-openvmm-linux-musl",
                "openvmm-linux-musl",
                "build-nvx-linux-musl",
            ),
            "platform-whp": (
                "package-whp",
                "windows-whp",
                "windows-latest",
                "build-openvmm-windows-msvc",
                "openvmm-windows-msvc",
                "build-nvx-windows-msvc",
            ),
        }
        self.assertEqual(tuple(platforms), ci.REQUIRED_CI_PLATFORM_JOBS)
        self.assertEqual(
            tuple(package for package, *_ in platforms.values()),
            ci.REQUIRED_CI_PACKAGE_JOBS,
        )
        for platform_job, (
            package_job,
            release_platform,
            runner,
            producer,
            artifact,
            nvx_build,
        ) in platforms.items():
            with self.subTest(platform=platform_job):
                self.assertIn(package_job, ci.REQUIRED_CI_RESULT_ENVIRONMENTS)
                platform = jobs[platform_job]
                package = jobs[package_job]
                self.assertEqual(
                    platform["needs"], ["artifacts", producer, "openvmm-changes"]
                )
                self.assertEqual(
                    package["needs"],
                    [
                        "artifacts",
                        "distro-artifacts",
                        producer,
                        nvx_build,
                        "openvmm-changes",
                    ],
                )
                self.assertEqual(platform["with"]["openvmm-artifact"], artifact)
                self.assertNotIn("release-platform", platform["with"])
                self.assertEqual(
                    package["uses"], "./.github/workflows/package-platform.yml"
                )
                self.assertEqual(json.loads(package["with"]["runner"]), [runner])
                self.assertEqual(package["with"]["release-platform"], release_platform)
                self.assertEqual(package["with"]["openvmm-artifact"], artifact)
                # A package job runs for the changes, events, and repositories
                # for which its platform job runs, once the images and the
                # nvx that only the package ships are ready too.
                self.assertEqual(
                    set(_expression_conjuncts(str(package["if"]))),
                    set(_expression_conjuncts(str(platform["if"])))
                    | {
                        "needs.distro-artifacts.result == 'success'",
                        f"needs.{nvx_build}.result == 'success'",
                    },
                )
        # The package jobs take no hypervisor and no benchmark runner.
        self.assertIn("    runs-on: ${{ fromJSON(inputs.runner) }}\n", packaging)
        self.assertNotIn("validate-runner", packaging)
        # The release ships the packages, and the performance results are
        # recorded for a commit only when it could be released.
        for consumer in ("release", "performance-record", "required-status-check"):
            job = _workflow_job(workflow, consumer)
            for package_job in ci.REQUIRED_CI_PACKAGE_JOBS:
                with self.subTest(consumer=consumer, package=package_job):
                    self.assertIn(f"      - {package_job}\n", job)
                    if consumer != "required-status-check":
                        self.assertIn(f"needs.{package_job}.result == 'success'", job)

    def test_runner_setup_pins_and_validates_sccache(self):
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        action = (
            BuildConstants.REPO_ROOT / ".github" / "actions" / "sccache" / "action.yml"
        ).read_text(encoding="utf-8")
        validate_runner = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "validate-runner"
            / "action.yml"
        ).read_text(encoding="utf-8")
        windows_setup = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-windows-whp.ps1"
        ).read_text(encoding="utf-8")
        linux_setup = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-linux.sh"
        ).read_text(encoding="utf-8")
        manifest = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "tool-versions.conf"
        ).read_text(encoding="utf-8")

        # The manifest pins sccache and its checksums, and both runner setups
        # install that release; test_tool_versions.py checks the manifest.
        self.assertRegex(manifest, r"(?m)^sccache\.version=[0-9]+\.[0-9]+\.[0-9]+$")
        for platform in ("linux-x86_64", "windows-x86_64"):
            self.assertRegex(
                manifest,
                rf"(?m)^sccache\.artifacts\.{platform}\.sha256=[0-9a-f]{{64}}$",
            )
        self.assertIn("\n    tool_manifest_artifact sccache\n", linux_setup)
        self.assertIn(
            '\n$sccacheRelease = Get-ToolArtifact $ToolManifest "sccache" $ToolPlatform\n',
            windows_setup,
        )
        for configuration in (windows_setup, linux_setup):
            self.assertIn("RUSTC_WRAPPER", configuration)
            self.assertIn("CARGO_INCREMENTAL", configuration)
            self.assertIn("SCCACHE_DIR", configuration)
            self.assertIn("SCCACHE_CACHE_SIZE", configuration)
        self.assertIn("sccache --zero-stats", action)
        self.assertIn("sccache --show-stats", action)
        self.assertIn("sccache --stop-server", action)
        self.assertIn("SCCACHE_IDLE_TIMEOUT", action)
        self.assertIn("sccache --version", validate_runner)
        self.assertNotRegex(
            workflow,
            r"(?m)^\s+path: openvmm/target\s*$",
        )
        self.assertLess(
            linux_setup.index('test -f "${runner_directory}/.runner"'),
            linux_setup.index('test -w "$runner_sccache_dir"'),
        )

    def test_linux_unnamed_setup_defers_existing_runner_validation(self):
        linux_setup = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-linux.sh"
        ).read_text(encoding="utf-8")
        environment_check = linux_setup.split("check_runner_host() {\n", 1)[1].split(
            "\n}\n", 1
        )[0]
        runner_checks = environment_check.split(
            "    validate_runner_state_paths\n    validate_runner_work_paths\n", 1
        )[1]
        guard = 'if [ "$configure_runner" = false ] && [ "$check_only" = false ]; then'
        self.assertIn(guard, runner_checks)
        self.assertLess(
            runner_checks.index(guard),
            runner_checks.index('test -w "$runner_sccache_dir"'),
        )
        if os.name != "posix":
            return

        harness = (
            "set -eu\n"
            "runner_directory=/missing-runner\n"
            "runner_sccache_dir=/missing-runner/_work/_sccache\n"
            "validate_runner_state_paths() { echo state-checked; }\n"
            "validate_runner_work_paths() { echo work-checked; }\n"
            'run_as_root() { [ "$1" = test ] && [ "$2" = -f ]; }\n'
            "check_runner_state() {\n"
            "    validate_runner_state_paths\n"
            "    validate_runner_work_paths\n"
            f"{runner_checks}\n"
            "}\n"
        )
        for configure_runner, check_only in (
            ("false", "false"),
            ("false", "true"),
            ("true", "false"),
        ):
            with self.subTest(configure_runner=configure_runner, check_only=check_only):
                result = subprocess.run(
                    [
                        "sh",
                        "-c",
                        harness + f"{configure_runner=}\n{check_only=}\n"
                        "check_runner_state\n",
                    ],
                    capture_output=True,
                    text=True,
                    timeout=10,
                    check=False,
                )
                self.assertEqual(
                    result.returncode == 0,
                    configure_runner == check_only == "false",
                    result.stderr,
                )
                if result.returncode == 0:
                    self.assertIn("state-checked", result.stdout)
                    self.assertIn("work-checked", result.stdout)

    def test_linux_setup_installs_openvmm_perl_modules(self):
        configuration = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-linux.sh"
        ).read_text(encoding="utf-8")

        for package in (
            "perl-FindBin",
            "perl-IPC-Cmd",
            "perl-Time-Piece",
            "perl-lib",
        ):
            self.assertIn(package, configuration)

    def test_windows_runner_requires_inbox_pcat_firmware(self):
        windows_setup = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-windows-whp.ps1"
        ).read_text(encoding="utf-8")
        validate_runner = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "validate-runner"
            / "action.yml"
        ).read_text(encoding="utf-8")

        self.assertIn('"Microsoft-Hyper-V"', windows_setup)
        for configuration in (windows_setup, validate_runner):
            for firmware in (
                "vmfirmwarepcat.dll",
                "vmfirmware.dll",
                "VmEmulatedDevices.dll",
            ):
                self.assertIn(firmware, configuration)

    def test_linux_runners_report_the_invariant_tsc_as_evidence(self):
        validate_runner = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "validate-runner"
            / "action.yml"
        ).read_text(encoding="utf-8")
        linux_setup = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-linux.sh"
        ).read_text(encoding="utf-8")

        # Neither CI nor provisioning rejects a host without an invariant
        # TSC: the guest warp probe measures what its guests observe (#265).
        step = validate_runner.split("    - name: Report host TSC\n", 1)[1]
        step = step.split("\n\n    - name: ", 1)[0]
        self.assertIn("      if: runner.os != 'Windows'\n", step)
        check = "\n".join(
            line.removeprefix("        ")
            for line in step.split("      run: |\n", 1)[1].splitlines()
        )
        function = linux_setup.split("report_invariant_tsc() {\n", 1)[1]
        function = "report_invariant_tsc() {\n" + function.split("\n}\n", 1)[0]
        function += "\n}\n"
        self.assertNotRegex(function, r"\bdie\b")
        self.assertLess(
            linux_setup.index("report_invariant_tsc /proc/cpuinfo\n"),
            linux_setup.index('sudo -n true || die "passwordless sudo is required"'),
        )
        if os.name != "posix":
            return

        with tempfile.TemporaryDirectory() as temporary:
            cpuinfo = Path(temporary) / "cpuinfo"
            for flags, invariant in (
                ("fpu tsc constant_tsc nonstop_tsc tsc_known_freq", True),
                ("fpu tsc constant_tsc tsc_known_freq", False),
                ("fpu tsc constant_tsc nonstop_tsc_x", False),
            ):
                cpuinfo.write_text(
                    f"model name\t: Test CPU\nflags\t\t: {flags}\n",
                    encoding="utf-8",
                )
                with self.subTest(flags=flags, check="validate-runner"):
                    result = subprocess.run(
                        ["bash", "-c", check.replace("/proc/cpuinfo", str(cpuinfo))],
                        capture_output=True,
                        text=True,
                        timeout=10,
                        check=False,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn("CPU: Test CPU", result.stdout)
                    self.assertIn(
                        "TSC flag nonstop_tsc: "
                        + ("present" if invariant else "absent"),
                        result.stdout,
                    )
                    self.assertEqual(
                        "::notice::Runner host does not expose an invariant TSC"
                        in result.stdout,
                        not invariant,
                    )
                    self.assertNotIn("::error::", result.stdout + result.stderr)
                with self.subTest(flags=flags, check="setup-linux"):
                    result = subprocess.run(
                        [
                            "sh",
                            "-c",
                            f"set -eu\n{function}report_invariant_tsc '{cpuinfo}'\n",
                        ],
                        capture_output=True,
                        text=True,
                        timeout=10,
                        check=False,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(
                        "warning: host does not expose an invariant TSC"
                        in result.stderr,
                        not invariant,
                    )

    def test_linux_runners_terminate_openvmm_left_by_earlier_jobs(self):
        validate_runner = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "validate-runner"
            / "action.yml"
        ).read_text(encoding="utf-8")

        # #768: a cancelled job's harness started OpenVMM after the runner's
        # orphan cleanup had listed the job's processes, and the VM ran for
        # eight days. The next job ends such a VM before it qualifies the
        # host's time.
        name = "Terminate OpenVMM left by earlier jobs"
        self.assertLess(
            validate_runner.index(f"    - name: {name}\n"),
            validate_runner.index("    - name: Report host TSC\n"),
        )
        step = validate_runner.split(f"    - name: {name}\n", 1)[1]
        step = step.split("\n\n", 1)[0]
        self.assertIn("      if: runner.os != 'Windows'\n", step)
        self.assertIn("      shell: bash\n", step)
        script = "\n".join(
            line.removeprefix("        ")
            for line in step.split("      run: |\n", 1)[1].splitlines()
        )
        self.assertIn("\nname=openvmm\n", script)
        if sys.platform != "linux":
            return

        with tempfile.TemporaryDirectory() as temporary:
            # A uniquely named stand-in, so that the test never ends a real
            # OpenVMM of the account that runs it: Python, through a link
            # with the stand-in's name, since the kernel names a process after
            # the file that it runs. A copy of sleep would not do: a
            # multi-call coreutils, such as Ubuntu 26.04's, knows no program
            # by that name.
            stand_in = f"nvx-leak-{os.getpid() % 100000}"
            executable = Path(temporary) / stand_in
            executable.symlink_to(sys.executable)
            sleeper = [str(executable), "-c", "import time; time.sleep(300)"]
            check = script.replace("\nname=openvmm\n", f"\nname={stand_in}\n", 1)

            def run(
                environment: dict[str, str] | None = None,
            ) -> subprocess.CompletedProcess[str]:
                return subprocess.run(
                    ["bash", "-c", check],
                    capture_output=True,
                    text=True,
                    timeout=30,
                    check=False,
                    env=environment,
                )

            # The second stand-in ignores SIGTERM, which leaves SIGKILL.
            for command, status in (
                (sleeper, -signal.SIGTERM),
                (
                    ["bash", "-c", f"trap '' TERM; exec {shlex.join(sleeper)}"],
                    -signal.SIGKILL,
                ),
            ):
                with self.subTest(status=status):
                    leaked = subprocess.Popen(command)
                    # Reap the stand-in as soon as it exits, as init reaps a
                    # leaked VM.
                    reaper = threading.Thread(target=leaked.wait, daemon=True)
                    reaper.start()
                    try:
                        # bash execs the stand-in after Popen returns.
                        comm = Path(f"/proc/{leaked.pid}/comm")
                        deadline = time.monotonic() + 10
                        while comm.read_text(encoding="utf-8").strip() != stand_in:
                            self.assertLess(time.monotonic(), deadline)
                            time.sleep(0.01)
                        result = run()
                        reaper.join(timeout=10)
                        self.assertEqual(result.returncode, 0, result.stderr)
                        self.assertIn(
                            f"::warning::Terminating OpenVMM process {leaked.pid}, "
                            "started ",
                            result.stdout,
                        )
                        self.assertIn(
                            ", which an earlier job left running\n", result.stdout
                        )
                        self.assertIn(
                            f"  command: {' '.join(sleeper)}\n", result.stdout
                        )
                        self.assertEqual(leaked.returncode, status)
                    finally:
                        if leaked.poll() is None:
                            leaked.kill()
                            leaked.wait()
            # A zombie has already exited and its parent reaps it, and without
            # a leftover process, the step says nothing.
            zombie = subprocess.Popen(sleeper)
            try:
                zombie.kill()
                stat = Path(f"/proc/{zombie.pid}/stat")
                deadline = time.monotonic() + 10
                while stat.read_text().rsplit(")", 1)[1].split()[0] != "Z":
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.01)
                result = run()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, "")
            finally:
                zombie.wait()
            result = run()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "")
            # A failure to list the processes fails the step rather than
            # passing as if none were left.
            stubs = Path(temporary) / "stubs"
            stubs.mkdir()
            (stubs / "ps").write_text("#!/bin/sh\nexit 2\n", encoding="utf-8")
            (stubs / "ps").chmod(0o755)
            environment = dict(os.environ)
            environment["PATH"] = f"{stubs}{os.pathsep}{environment['PATH']}"
            self.assertNotEqual(run(environment).returncode, 0)

    def test_linux_runners_remove_scratch_left_by_earlier_jobs(self):
        validate_runner = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "validate-runner"
            / "action.yml"
        ).read_text(encoding="utf-8")

        # #946: a harness that the runner's orphan cleanup killed left its
        # scenario's scratch directory behind, and the ten Linux runners held
        # 64 of them. The next job removes the account's harness directories,
        # once it has ended the OpenVMM processes that could still use them.
        name = "Remove scratch left by earlier jobs"
        self.assertLess(
            validate_runner.index(
                "    - name: Terminate OpenVMM left by earlier jobs\n"
            ),
            validate_runner.index(f"    - name: {name}\n"),
        )
        self.assertLess(
            validate_runner.index(f"    - name: {name}\n"),
            validate_runner.index("    - name: Report host TSC\n"),
        )
        step = validate_runner.split(f"    - name: {name}\n", 1)[1]
        step = step.split("\n\n", 1)[0]
        self.assertIn("      if: runner.os != 'Windows'\n", step)
        self.assertIn("      shell: bash\n", step)
        script = "\n".join(
            line.removeprefix("        ")
            for line in step.split("      run: |\n", 1)[1].splitlines()
        )
        temporary_line = (
            "temporary=$(python3 -c 'import tempfile; print(tempfile.gettempdir())')\n"
        )
        self.assertIn(temporary_line, script)
        # Each scratch directory that the harness creates in the temporary
        # directory has a prefix that the step removes.
        for module in (
            "benchmark",
            "managed_exec_tests",
            "microvm_tests",
            "sandbox_lifecycle_tests",
            "sandbox_proxy_tests",
        ):
            source = (
                BuildConstants.REPO_ROOT / "scripts" / "nvx_tools" / f"{module}.py"
            ).read_text(encoding="utf-8")
            calls = re.findall(
                r"(?:TemporaryDirectory|mkdtemp)\((.*?)\)", source, re.DOTALL
            )
            self.assertTrue(calls, module)
            for arguments in calls:
                with self.subTest(module=module, arguments=arguments):
                    self.assertRegex(arguments, r'^\s*prefix=f?"(nvx|openvmm)-')
                    self.assertNotIn("dir=", arguments)
        if sys.platform != "linux":
            return

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            scratch = root / "tmp"
            scratch.mkdir()
            # The step never sees the temporary directory of this host.
            check = script.replace(
                temporary_line, f"temporary={shlex.quote(str(scratch))}\n", 1
            )

            def run(
                environment: dict[str, str] | None = None,
            ) -> subprocess.CompletedProcess[str]:
                return subprocess.run(
                    ["bash", "-c", check],
                    capture_output=True,
                    text=True,
                    timeout=30,
                    check=False,
                    env=environment,
                )

            left = [
                scratch / "nvx-restore-downtime-duijv83o",
                scratch / "openvmm-device-io-ep0lqoc8",
            ]
            for directory in left:
                (directory / "snapshot").mkdir(parents=True)
                (directory / "snapshot" / "memory.bin").write_bytes(bytes(16))
            # A directory that even its owner can't empty as it is.
            locked = left[0] / "locked"
            locked.mkdir()
            (locked / "file").write_text("locked", encoding="utf-8")
            locked.chmod(0o500)
            # Neither a file nor a link to a directory is a harness directory.
            outside = root / "outside"
            outside.mkdir()
            (outside / "data").write_text("outside", encoding="utf-8")
            other = scratch / "other-directory"
            other.mkdir()
            file = scratch / "nvx-file"
            file.write_text("file", encoding="utf-8")
            link = scratch / "nvx-link"
            link.symlink_to(outside, target_is_directory=True)

            result = run()
            self.assertEqual(result.returncode, 0, result.stderr)
            for directory in left:
                self.assertFalse(directory.exists())
                self.assertIn(
                    f"::notice::Removing {directory}, last modified ", result.stdout
                )
            self.assertIn(", which an earlier job left behind\n", result.stdout)
            self.assertNotIn("::warning::", result.stdout)
            self.assertTrue(other.is_dir())
            self.assertEqual(file.read_text(encoding="utf-8"), "file")
            self.assertTrue(link.is_symlink())
            self.assertEqual((outside / "data").read_text(encoding="utf-8"), "outside")
            # Without leftover directories, the step says nothing.
            result = run()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "")
            # A directory that the step can't remove warns but doesn't fail the
            # job, and a failure to list the directories fails it.
            stubs = root / "stubs"
            stubs.mkdir()
            stuck = scratch / "nvx-stuck"
            stuck.mkdir()
            (stubs / "rm").write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
            (stubs / "rm").chmod(0o755)
            environment = dict(os.environ)
            environment["PATH"] = f"{stubs}{os.pathsep}{environment['PATH']}"
            result = run(environment)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(
                f"::warning::Could not remove {stuck}, which an earlier job left behind",
                result.stdout,
            )
            (stubs / "rm").unlink()
            (stubs / "find").write_text("#!/bin/sh\nexit 2\n", encoding="utf-8")
            (stubs / "find").chmod(0o755)
            self.assertNotEqual(run(environment).returncode, 0)

    def test_runners_qualify_their_host_time_before_every_job(self):
        validate_runner = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "validate-runner"
            / "action.yml"
        ).read_text(encoding="utf-8")

        # The doctor follows the host TSC report on Linux: the backend, the CPU
        # fingerprint and generation, and the TSC rate stability and the host's
        # clock correction. Jobs that haven't downloaded OpenVMM skip its CPU
        # profile check (H2) and preflight (H3), and only jobs that ask for it
        # run the guest warp probe (H6).
        self.assertLess(
            validate_runner.index("    - name: Report host TSC\n"),
            validate_runner.index("    - name: Qualify host time on Linux\n"),
        )
        for name, shell, command, summary in (
            (
                "Qualify host time on Linux",
                "bash",
                "python3 scripts/nvx.py doctor",
                '--summary "${GITHUB_STEP_SUMMARY}"',
            ),
            (
                "Qualify host time on Windows",
                "powershell",
                "python scripts\\nvx.py doctor",
                '--summary "$env:GITHUB_STEP_SUMMARY"',
            ),
        ):
            with self.subTest(step=name):
                step = validate_runner.split(f"    - name: {name}\n", 1)[1]
                step = step.split("\n\n", 1)[0]
                self.assertIn(f"      shell: {shell}\n", step)
                self.assertIn(command, step)
                self.assertIn('--backend "${{ inputs.backend }}"', step)
                self.assertIn("--checks H1 H2 H4\n", step)
                self.assertIn(
                    "${{ inputs.warp-probe == 'true' && 'H6' || '' }}\n", step
                )
                fingerprint = (
                    '"${RUNNER_TEMP}/nvx-cpu-fingerprint.json"'
                    if shell == "bash"
                    else '"$env:RUNNER_TEMP\\nvx-cpu-fingerprint.json"'
                )
                self.assertIn(
                    "${{ inputs.verify-openvmm == 'true' && "
                    f"'H3 --cpu-fingerprint {fingerprint}' || '--no-openvmm' }}}}\n",
                    step,
                )
                self.assertIn("--ci-schedule\n", step)
                self.assertIn(summary, step)
        args = nvx.parse_args(
            ["doctor", "--backend", "kvm", "--checks", "H1", "H2", "H4", "H3"]
            + ["--cpu-fingerprint", "fingerprint.json", "--ci-schedule"]
        )
        self.assertEqual(args.checks, ["H1", "H2", "H4", "H3"])
        self.assertEqual(args.cpu_fingerprint, Path("fingerprint.json"))
        args = nvx.parse_args(
            ["doctor", "--backend", "kvm", "--checks", "H1", "H2", "H4", "H6", "H3"]
            + ["--cpu-fingerprint", "fingerprint.json", "--ci-schedule"]
        )
        self.assertEqual(args.checks, ["H1", "H2", "H4", "H6", "H3"])
        args = nvx.parse_args(
            ["doctor", "--backend", "kvm", "--checks", "H1", "H2", "H4"]
            + ["--no-openvmm", "--ci-schedule"]
        )
        self.assertTrue(args.no_openvmm)
        self.assertTrue(args.ci_schedule)
        self.assertIn("  verify-openvmm:\n", validate_runner)
        self.assertIn('    default: "false"\n', validate_runner)
        warp_probe = validate_runner.split("  warp-probe:\n", 1)[1]
        warp_probe = warp_probe.split("\nruns:\n", 1)[0]
        self.assertIn("requires\n      verify-openvmm\n", warp_probe)
        self.assertIn('    default: "false"\n', warp_probe)
        # A failed CPU profile check keeps OpenVMM's fingerprint.
        upload = validate_runner.split("    - name: Upload the CPU fingerprint\n")[1]
        upload = upload.split("\n\n", 1)[0]
        self.assertIn("if: failure() && inputs.verify-openvmm == 'true'\n", upload)
        self.assertIn("path: ${{ runner.temp }}/nvx-cpu-fingerprint.json\n", upload)
        self.assertIn("if-no-files-found: ignore\n", upload)
        # Jobs that run OpenVMM qualify the runner after downloading it.
        benchmark = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")
        self.assertNotIn("download-artifact", benchmark)
        for workflow_name in ("run-nvx-microvm-tests.yml", "run-platform.yml"):
            with self.subTest(workflow=workflow_name):
                workflow = (
                    BuildConstants.REPO_ROOT / ".github" / "workflows" / workflow_name
                ).read_text(encoding="utf-8")
                validate = workflow.index("      - name: Validate runner\n")
                self.assertEqual(workflow.count("      - name: Validate runner\n"), 1)
                self.assertIn(
                    'verify-openvmm: "true"', workflow[validate : validate + 220]
                )
                # The microVM scenarios probe after every boot and restore;
                # the benchmarks boot guests without such probes.
                self.assertEqual(
                    'warp-probe: "true"' in workflow[validate : validate + 260],
                    workflow_name == "run-platform.yml",
                )
                for step in (
                    "      - name: Download guest kernel\n",
                    "      - name: Download Alpine initramfs\n",
                    "      - name: Download OpenVMM executable\n",
                    "      - name: Make OpenVMM executable\n",
                ):
                    self.assertLess(workflow.index(step), validate)
        self.assertLess(
            validate_runner.index("Validate Linux toolchain"),
            validate_runner.index("Qualify host time on Linux"),
        )

    def test_runner_setups_install_backend_native_openvmm_targets(self):
        linux_setup = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-linux.sh"
        ).read_text(encoding="utf-8")
        windows_setup = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-windows-whp.ps1"
        ).read_text(encoding="utf-8")

        for target in (
            "x86_64-unknown-none",
            "x86_64-unknown-uefi",
        ):
            self.assertIn(target, linux_setup)
            self.assertIn(target, windows_setup)
        self.assertIn("x86_64-unknown-linux-musl", linux_setup)
        for cross_platform_tool in (
            "x86_64-pc-windows-gnu",
            "gcc-mingw-w64-x86-64-win32",
            "mingw64-gcc",
            "x86_64-w64-mingw32-gcc",
            "x86_64-w64-mingw32-dlltool",
        ):
            self.assertNotIn(cross_platform_tool, linux_setup)

    def test_linux_runner_setup_selects_exactly_one_runner_role(self):
        # Provisioning replaces a runner's registration, so the benchmark
        # runner keeps the role that CI requires only when setup registers it.
        linux_setup = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-linux.sh"
        ).read_text(encoding="utf-8")
        self.assertIn(
            "        --benchmark-runner)\n"
            "            runner_option=$1\n"
            "            benchmark_runner=true\n"
            "            shift\n"
            "            ;;\n",
            linux_setup,
        )
        self.assertIn("\n    --benchmark-runner      ", linux_setup)
        # Registration and check mode both use the computed label set.
        self.assertIn('--labels "$expected_runner_labels"', linux_setup)
        self.assertIn(
            '"$expected_runner_labels" ] ||\n'
            '                die "GitHub Actions runner labels are not validated"',
            linux_setup,
        )

        def function(name: str) -> str:
            body = linux_setup.split(f"\n{name}() {{\n", 1)[1].split("\n}\n", 1)[0]
            return f"{name}() {{\n{body}\n}}\n"

        self.assertIn(
            "\n    expected_runner_labels=linux,${backend},virtual-machine\n",
            function("set_expected_runner_labels"),
        )
        self.assertIn(
            "\n    set_expected_runner_labels\n", function("validate_options")
        )
        if os.name != "posix":
            return

        harness = (
            "set -eu\n"
            + "".join(
                function(name)
                for name in (
                    "die",
                    "validate_runner_name",
                    "append_runner_labels",
                    "set_expected_runner_labels",
                    "validate_options",
                )
            )
            + "backend=$1\nrunner_name=$2\nbenchmark_runner=$3\n"
            "configure_runner=false\n"
            '[ -z "$runner_name" ] || configure_runner=true\n'
            "runner_only=true\ncheck_only=false\nbundle_only=false\n"
            "guest_bundle=\nextra_runner_labels=\n"
            "development_option=\nrunner_option=\n"
            "validate_options\n"
            'printf "%s\\n" "$expected_runner_labels"\n'
        )
        for backend, runner_name, benchmark_runner, expected in (
            ("kvm", "azure-kvm-1", "false", "linux,kvm,virtual-machine,general"),
            ("mshv", "", "false", "linux,mshv,virtual-machine,general"),
            ("kvm", "azure-kvm-3", "true", "linux,kvm,virtual-machine,benchmark"),
            ("mshv", "azure-mshv-4", "true", "linux,mshv,virtual-machine,benchmark"),
            ("kvm", "", "true", None),
        ):
            with self.subTest(
                backend=backend,
                runner_name=runner_name,
                benchmark_runner=benchmark_runner,
            ):
                result = subprocess.run(
                    ["sh", "-c", harness, "sh", backend, runner_name, benchmark_runner],
                    capture_output=True,
                    text=True,
                    timeout=10,
                    check=False,
                )
                if expected is None:
                    self.assertEqual(result.returncode, 1)
                    self.assertEqual(
                        result.stderr,
                        "error: --benchmark-runner requires --runner-name\n",
                    )
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, f"{expected}\n")

    def test_windows_runner_setup_selects_exactly_one_runner_role(self):
        setup = BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-windows-whp.ps1"
        windows_setup = setup.read_text(encoding="utf-8")
        self.assertIn("    [switch]$BenchmarkRunner,\n", windows_setup)
        self.assertIn(
            "if ($BenchmarkRunner -and\n"
            "    (-not $RunnerOnly -or [string]::IsNullOrWhiteSpace($RunnerName))) {\n"
            '    throw "-BenchmarkRunner requires -RunnerOnly and -RunnerName"\n'
            "}\n",
            windows_setup,
        )
        # Registration and check mode share one label set.
        self.assertEqual(
            windows_setup.count('@("windows", "whp", "virtual-machine")'), 1
        )
        for function_name, assignment in (
            ("Install-ActionsRunner", "\n    $labels = $ExpectedRunnerLabels\n"),
            (
                "Assert-ActionsRunner",
                "\n        $expectedLabels = $ExpectedRunnerLabels\n",
            ),
        ):
            with self.subTest(function=function_name):
                function = windows_setup.split(f"\nfunction {function_name} {{\n", 1)[1]
                self.assertIn(assignment, function.split("\n}\n", 1)[0])
        if os.name != "nt":
            return

        harness = f"""
Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    '{setup}',
    [ref]$tokens,
    [ref]$errors
)
if ($errors.Count -ne 0) {{
    throw "setup script has parse errors"
}}
$definition = $ast.Find({{
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
        $node.Name -eq "Get-ExpectedRunnerLabels"
    }}, $true)
. ([scriptblock]::Create($definition.Extent.Text))
Write-Output (Get-ExpectedRunnerLabels)
Write-Output (Get-ExpectedRunnerLabels -BenchmarkRunner)
"""
        result = run_powershell(
            ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", harness]
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.splitlines(),
            [
                "windows,whp,virtual-machine,general",
                "windows,whp,virtual-machine,benchmark",
            ],
        )

    def test_repository_pins_one_exact_rust_toolchain(self):
        root = BuildConstants.REPO_ROOT
        [toolchain] = re.findall(
            r'^channel = "([^"]*)"$',
            (root / "rust-toolchain.toml").read_text(encoding="utf-8"),
            re.MULTILINE,
        )
        # A floating channel such as `stable` lets runners drift apart (#402).
        self.assertRegex(toolchain, r"^[0-9]+\.[0-9]+\.[0-9]+$")
        self.assertEqual(OpenVMMBuildConstants.RUST_TOOLCHAIN, toolchain)
        self.assertIn("rust-toolchain.toml", ReleaseBuildConstants.PROJECT_SOURCE_PATHS)

        setup = root / "scripts" / "setup"
        # The setup scripts and the Specula runner setup take the release from
        # the setup tool manifest, which rustup cannot read.
        self.assertEqual(
            re.findall(
                r"^rust\.toolchain=(.*)$",
                (setup / "tool-versions.conf").read_text(encoding="utf-8"),
                re.MULTILINE,
            ),
            [toolchain],
        )
        for path, reader in (
            (setup / "setup-linux.sh", "\n    RUST_TOOLCHAIN=$tool_value\n"),
            (
                setup / "setup-windows-whp.ps1",
                '\n$RustToolchain = Get-ToolManifestValue $ToolManifest "rust.toolchain"\n',
            ),
            (
                root / ".github" / "specula" / "setup-runner.sh",
                '\nRUST_VERSION="$(tool_value rust.toolchain "$VERSION_PATTERN")"\n',
            ),
        ):
            with self.subTest(path=path.name):
                self.assertIn(reader, path.read_text(encoding="utf-8"))

    def test_ci_takes_rust_toolchain_from_repository_pin(self):
        github = BuildConstants.REPO_ROOT / ".github"
        workflows = github / "workflows"

        validate_runner = (
            github / "actions" / "validate-runner" / "action.yml"
        ).read_text(encoding="utf-8")
        for step_name in ("Validate Linux toolchain", "Validate Windows toolchain"):
            with self.subTest(step=step_name):
                step = _composite_action_step(validate_runner, step_name)
                self.assertIn("rust-toolchain.toml", step)
                # Report a missing toolchain without writing the trusted RUSTUP_HOME.
                self.assertEqual(_yaml_field(step, "RUSTUP_AUTO_INSTALL"), ['"0"'])

        crate_action = (github / "actions" / "check-nvx" / "action.yml").read_text(
            encoding="utf-8"
        )
        self.assertEqual(_yaml_field(crate_action, "default"), ['""'])
        install = _composite_action_step(crate_action, "Install Rust toolchains")
        self.assertIn("../rust-toolchain.toml", install)
        self.assertIn("REQUESTED_TOOLCHAIN: ${{ inputs.toolchain }}", install)
        self.assertEqual(crate_action.count("${{ inputs.toolchain }}"), 1)

        setup = (workflows / "copilot-setup-steps.yml").read_text(encoding="utf-8")
        self.assertIn("      - rust-toolchain.toml\n", setup)
        self.assertIn("      - nvx/**\n", setup)
        self.assertIn(
            "rust-toolchain.toml",
            _workflow_step(
                setup, "copilot-setup-steps", "Resolve pinned tool versions"
            ),
        )
        rust = _workflow_step(setup, "copilot-setup-steps", "Install Rust toolchains")
        self.assertIn('rustup default "${RUST_TOOLCHAIN}"', rust)
        self.assertIn("x86_64-unknown-linux-musl", rust)
        self.assertNotIn("stable", rust)

        # A binary built by another compiler must not satisfy the cache.
        build_action = (github / "actions" / "build-openvmm" / "action.yml").read_text(
            encoding="utf-8"
        )
        self.assertEqual(
            build_action.count(
                "${{ hashFiles('rust-toolchain.toml') }}-${{ steps.openvmm.outputs.sha }}"
            ),
            4,
        )

        # A new release reruns every suite that it builds; OpenVMM's own CI
        # builds its tests with OpenVMM's pin.
        for pattern in (
            ci.MICROVM_TEST_INPUT_PATHS,
            ci.BENCHMARK_INPUT_PATHS,
        ):
            self.assertIsNotNone(pattern.match("rust-toolchain.toml"))
        self.assertIsNone(ci.OPENVMM_CI_CHECK_INPUT_PATHS.match("rust-toolchain.toml"))

    def test_release_actions_use_deterministic_immutable_tooling(self):
        package_action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "package-release"
            / "action.yml"
        ).read_text(encoding="utf-8")
        publish_action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "publish-development-release"
            / "action.yml"
        ).read_text(encoding="utf-8")

        self.assertEqual(package_action.count("archive-release"), 2)
        self.assertIn(
            "python scripts\\nvx.py materialize-kernel-provenance-inputs",
            package_action,
        )
        self.assertLess(
            package_action.index(
                "python scripts\\nvx.py materialize-kernel-provenance-inputs"
            ),
            package_action.index("- name: Package Windows release"),
        )
        self.assertNotIn("git checkout-index", package_action)
        self.assertNotIn("tar -czf", package_action)
        self.assertNotIn("Compress-Archive", package_action)
        self.assertIn(
            "python3 -u scripts/publish_development_release.py",
            publish_action,
        )
        self.assertNotIn("--clobber", publish_action)

    def test_remote_actions_are_pinned_to_release_commits(self):
        # A tag or a branch is a pointer that whoever controls the action's
        # repository can move, so every remote action names a full commit SHA
        # and the release whose commit it is (doc/ci.md#action-references).
        # Local actions come from the checkout itself.
        references = [
            reference
            for reference in _action_references()
            if not reference[1].startswith("./")
        ]

        self.assertTrue(references)
        for location, reference, comment in references:
            with self.subTest(location=location):
                self.assertRegex(reference, r"^[\w.-]+/[^@\s]+@[0-9a-f]{40}$")
                self.assertRegex(comment, r"^ # v\d+\.\d+\.\d+$")

    def test_ci_artifact_actions_use_node24(self):
        # The artifact actions run on Node 24 from upload-artifact v6 and
        # download-artifact v7. A pin names a commit, so the release comes
        # from its version comment.
        minimum = {"actions/upload-artifact": 6, "actions/download-artifact": 7}
        found = dict.fromkeys(minimum, 0)
        for location, reference, comment in _action_references():
            action = reference.partition("@")[0]
            if action not in minimum:
                continue
            found[action] += 1
            with self.subTest(location=location):
                release = re.fullmatch(r" # v(\d+)\.\d+\.\d+", comment)
                if release is None:
                    self.fail(f"{reference} names no release: {comment!r}")
                self.assertGreaterEqual(int(release[1]), minimum[action])

        self.assertTrue(all(found.values()), found)

    def test_benchmark_diagnostics_preserve_each_workflow_attempt(self):
        action = (
            Path(__file__).parents[1]
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")
        diagnostics = _composite_action_step(action, "Upload benchmark diagnostics")
        name = (
            "benchmark-diagnostics-${{ inputs.platform }}-${{ github.run_id }}"
            "-attempt-${{ github.run_attempt }}"
        )

        self.assertIn("      if: always()", diagnostics)
        self.assertIn("      uses: actions/upload-artifact@", diagnostics)
        self.assertIn("        path: data/runs/${{ inputs.platform }}", diagnostics)
        self.assertIn(f"        name: {name}", diagnostics)
        self.assertIn("        if-no-files-found: error", diagnostics)
        self.assertIn("        overwrite: false", diagnostics)
        self.assertIn("        retention-days: 1", diagnostics)
        self.assertLess(
            action.index("- name: Upload benchmark diagnostics"),
            action.index("- name: Upload benchmark results"),
        )

        artifacts: dict[str, tuple[str, int]] = {}
        for attempt in (1, 2):
            for platform in (
                "linux-kvm-virtual-machine",
                "linux-mshv-virtual-machine",
                "windows-whp-virtual-machine",
            ):
                artifact = (
                    name.replace("${{ inputs.platform }}", platform)
                    .replace("${{ github.run_id }}", "35771477786")
                    .replace("${{ github.run_attempt }}", str(attempt))
                )
                self.assertNotIn("${{", artifact)
                self.assertNotIn(artifact, artifacts)
                artifacts[artifact] = (platform, attempt)
        self.assertEqual(len(artifacts), 6)

    def test_benchmark_result_handoff_keeps_run_scoped_names(self):
        actions = Path(__file__).parents[1] / ".github" / "actions"
        action = (actions / "run-benchmark" / "action.yml").read_text(encoding="utf-8")
        prepare = (actions / "prepare-performance-results" / "action.yml").read_text(
            encoding="utf-8"
        )
        results = _composite_action_step(action, "Upload benchmark results")
        name = "benchmark-${{ inputs.platform }}-${{ github.run_id }}"

        self.assertIn(f"        name: {name}", results)
        self.assertIn("        path: data/runs/${{ inputs.platform }}", results)
        self.assertIn("        overwrite: true", results)
        self.assertNotIn("github.run_attempt", results)
        self.assertNotIn("github.run_attempt", prepare)
        for platform in (
            "linux-kvm-virtual-machine",
            "linux-mshv-virtual-machine",
            "windows-whp-virtual-machine",
        ):
            with self.subTest(platform=platform):
                artifact = name.replace("${{ inputs.platform }}", platform)
                self.assertIn(f"        name: {artifact}", prepare)

    def test_benchmarks_check_the_expected_host_before_measuring(self):
        # A series' history comes from the host in data/<platform>-host.json,
        # so a platform job refuses another host before it measures (#526).
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")
        steps = re.findall(r"^    - name: (.+)$", action, re.MULTILINE)
        output = (
            '--output "data/runs/${{ inputs.platform }}/'
            '${{ inputs.platform }}-host.json"'
        )

        self.assertEqual(steps[0], "Check Linux benchmark host")
        self.assertLess(
            steps.index("Prepare Windows benchmark scratch"),
            steps.index("Check Windows benchmark host"),
        )
        for operating_system, condition in (
            ("Linux", "runner.os != 'Windows'"),
            ("Windows", "runner.os == 'Windows'"),
        ):
            check = f"Check {operating_system} benchmark host"
            with self.subTest(step=check):
                self.assertIn(
                    f"      if: {condition}", _composite_action_step(action, check)
                )
                script = _composite_action_script(action, check)
                self.assertEqual(script.count("performance check-host"), 1)
                self.assertIn('--platform "${{ inputs.platform }}"', script)
                self.assertIn("--history-dir data", script)
                self.assertIn(output, script)
                self.assertIn("--summary", script)
                measuring = [
                    step
                    for step in steps
                    if step.startswith(f"Run {operating_system} ")
                ]
                self.assertTrue(measuring)
                for step in measuring:
                    self.assertLess(steps.index(check), steps.index(step))
        windows = _composite_action_script(action, "Check Windows benchmark host")
        self.assertIn(
            '$ScratchArgs = @("--scratch-dir", $env:NVX_BENCHMARK_SCRATCH_DIR)',
            windows,
        )
        self.assertIn("@ScratchArgs", windows)
        self.assertTrue(
            windows.rstrip().endswith("if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }")
        )
        self.assertIn(
            "set -euo pipefail",
            _composite_action_script(action, "Check Linux benchmark host"),
        )

    def test_performance_history_tracks_the_expected_hosts(self):
        github = BuildConstants.REPO_ROOT / ".github"
        actions = github / "actions"
        workflow = (github / "workflows" / "ci.yml").read_text(encoding="utf-8")
        persist = (actions / "persist-performance-results" / "action.yml").read_text(
            encoding="utf-8"
        )
        gate = (actions / "gate-performance" / "action.yml").read_text(encoding="utf-8")

        # dev refuses results whose recorded host differs from data/: the push
        # that measures them checks them against its history, and the full run
        # that persists them against the hosts of their commit.
        check = _workflow_step(
            workflow, "performance-record", "Check the results against the history"
        )
        self.assertIn('cp data/*.csv data/*-host.json "${history}/"', check)
        self.assertIn(
            '            --history-dir "${history}" \\\n            --require-host\n',
            check,
        )
        command = _composite_action_step(persist, "Persist results")
        self.assertIn("persist-recorded-performance", command)
        self.assertIn("            --history-dir data \\\n", command)
        self.assertIn("            --require-host \\\n", command)
        # The gate compares the pull request's expected hosts with the base
        # branch's, so that a new host must reset its series.
        load = _composite_action_script(gate, "Load base branch performance history")
        self.assertIn("done < <(git ls-tree --name-only FETCH_HEAD data/)", load)
        self.assertIn("data/*-host.json)", load)
        self.assertIn(
            'git show "FETCH_HEAD:${path}" > "data/baseline/${path#data/}"', load
        )
        command = _composite_action_step(gate, "Gate on performance regressions")
        self.assertIn("--baseline-dir data/baseline", command)
        self.assertIn("--history-reset-dir data", command)

    def test_dev_pushes_record_the_results_that_full_runs_persist(self):
        # Every push to dev outside the merge queue rebuilds every queued
        # merge group, so a dev push uploads its performance results, and a
        # full run of dev commits those that the history lacks (#1053).
        github = BuildConstants.REPO_ROOT / ".github"
        text = (github / "workflows" / "ci.yml").read_text(encoding="utf-8")
        jobs = yaml.safe_load(text)["jobs"]
        record = jobs["performance-record"]
        self.assertEqual(record["permissions"], {"contents": "read"})
        self.assertNotIn("secrets.", _workflow_job(text, "performance-record"))
        steps = {step["name"]: step for step in record["steps"]}
        self.assertEqual(
            steps["Prepare performance results"]["uses"],
            "./.github/actions/prepare-performance-results",
        )
        self.assertEqual(
            steps["Upload performance results"]["with"],
            {
                "name": performance_history.RESULTS_ARTIFACT_PREFIX
                + "${{ github.run_id }}",
                "path": "data/results",
                "if-no-files-found": "error",
                "overwrite": True,
                "retention-days": performance_history.RESULTS_RETENTION_DAYS,
            },
        )
        persist = jobs["performance-persist"]
        self.assertEqual(
            persist["permissions"], {"actions": "read", "contents": "read"}
        )
        checkout, persistence = persist["steps"]
        self.assertEqual(checkout["with"]["ref"], "dev")
        self.assertEqual(
            checkout["with"]["ssh-key"],
            "${{ secrets.PERFORMANCE_PERSIST_DEPLOY_KEY }}",
        )
        self.assertEqual(
            persistence["uses"], "./.github/actions/persist-performance-results"
        )
        self.assertEqual(
            persistence["with"], {"branch": "dev", "token": "${{ github.token }}"}
        )
        # Only the persistence holds the key that pushes to dev.
        self.assertEqual(
            [
                name
                for name in jobs
                if "PERFORMANCE_PERSIST_DEPLOY_KEY" in _workflow_job(text, name)
            ],
            ["performance-persist"],
        )
        action = github / "actions" / "persist-performance-results" / "action.yml"
        script = _composite_action_script(
            action.read_text(encoding="utf-8"), "Persist results"
        )
        self.assertIn('--download-dir "${downloads}"', script)
        self.assertIn('--message "${message}" || status=$?', script)
        self.assertIn('git commit --no-verify -F "${message}"', script)
        self.assertNotIn("prepare-performance-results", action.read_text("utf-8"))

    def test_ci_remeasures_only_unstable_lifecycle_results(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")

        self.assertEqual(action.count("performance validate-openvmm"), 2)
        for step, loop, unstable in (
            (
                "Run Linux acceptance test",
                "for attempt in 1 2; do",
                '"${validation_status}" -ne 75',
            ),
            (
                "Run Windows acceptance test",
                "foreach ($Attempt in 1, 2)",
                "$ValidationStatus -ne 75",
            ),
        ):
            with self.subTest(step=step):
                script = _composite_action_script(action, step)
                self.assertEqual(script.count("performance validate-openvmm"), 1)
                self.assertEqual(script.count(loop), 1)
                self.assertEqual(script.count(unstable), 1)
                self.assertEqual(
                    script.count("Lifecycle snapshot generation was unstable"),
                    1,
                )
                self.assertEqual(script.count("--attempt-log"), 1)
                self.assertIn("acceptance-attempts.json", script)
        # Publication names the attempt that supplied the lifecycle metrics.
        for step, log in (
            (
                "Publish Linux performance results",
                '--lifecycle-attempt-log "${run_dir}/acceptance-attempts.json"',
            ),
            (
                "Publish Windows performance results",
                '--lifecycle-attempt-log "$RunDir/acceptance-attempts.json"',
            ),
        ):
            with self.subTest(step=step):
                script = _composite_action_script(action, step)
                self.assertEqual(script.count(log), 1)
                self.assertLess(script.index("--lifecycle-input"), script.index(log))

    def test_ci_remeasures_only_unstable_restore_results(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")

        self.assertEqual(action.count("performance validate-workload"), 4)
        for operating_system, loop, unstable, attempt_dir in (
            (
                "Linux",
                "for attempt in 1 2; do",
                '"${validation_status}" -ne 75',
                'attempt_dir="${run_dir}/{suite}-attempt-${attempt}"',
            ),
            (
                "Windows",
                "foreach ($Attempt in 1, 2)",
                "$ValidationStatus -ne 75",
                '$AttemptDir = "$RunDir/{suite}-attempt-$Attempt"',
            ),
        ):
            for step, suite, _, _ in RESTORE_REMEASUREMENT_STEPS:
                with self.subTest(step=f"Run {operating_system} {step}"):
                    script = _composite_action_script(
                        action, f"Run {operating_system} {step}"
                    )
                    self.assertEqual(script.count("performance validate-workload"), 1)
                    self.assertEqual(script.count(loop), 1)
                    self.assertEqual(script.count(unstable), 1)
                    self.assertEqual(script.count("was unstable on attempt"), 1)
                    self.assertIn(attempt_dir.replace("{suite}", suite), script)
                    # The suite writes only to its attempt's directory, which
                    # validation then checks.
                    self.assertEqual(script.count("--output-dir"), 1)
                    self.assertIn("--input-dir", script)
                    self.assertLess(
                        script.index("--output-dir"),
                        script.index("performance validate-workload"),
                    )

    def test_failed_windows_benchmarks_keep_the_jobs_storage_events(self):
        github = BuildConstants.REPO_ROOT / ".github"
        action = (github / "actions" / "run-benchmark" / "action.yml").read_text(
            encoding="utf-8"
        )
        platform = (github / "workflows" / "run-platform.yml").read_text(
            encoding="utf-8"
        )

        # The window starts with the job, which records its start first.
        start = _workflow_steps(platform, "run")[0]
        self.assertTrue(start.startswith("      - name: Record Windows job start\n"))
        self.assertIn("        if: runner.os == 'Windows'", start)
        self.assertIn(
            "\"NVX_JOB_STARTED_AT=$([DateTime]::UtcNow.ToString('o'))\"", start
        )
        self.assertIn("Out-File -FilePath $env:GITHUB_ENV", start)
        save = _composite_action_step(action, "Save Windows storage events")
        self.assertIn("      if: failure() && runner.os == 'Windows'", save)
        script = _composite_action_script(action, "Save Windows storage events")
        self.assertIn("python scripts\\nvx.py performance storage-events", script)
        self.assertIn("--since $env:NVX_JOB_STARTED_AT", script)
        self.assertIn(
            '--output-dir "data/runs/${{ inputs.platform }}/storage-events"', script
        )
        # The scratch directory still exists, and names its volume.
        self.assertIn(
            '$ScratchArgs = @("--scratch-dir", $env:NVX_BENCHMARK_SCRATCH_DIR)',
            script,
        )
        # The diagnostics upload, which runs whatever failed, keeps the events,
        # and both uploads precede the scratch removal, which a stalled volume
        # can hold up past the job's timeout, when no later step runs.
        steps = [
            action.index(f"- name: {name}")
            for name in (
                "Save Windows storage events",
                "Upload benchmark diagnostics",
                "Upload benchmark results",
                "Remove Windows benchmark scratch",
            )
        ]
        self.assertEqual(steps, sorted(steps))
        upload = _composite_action_step(action, "Upload benchmark diagnostics")
        self.assertIn("      if: always()", upload)
        self.assertIn("        path: data/runs/${{ inputs.platform }}", upload)

    def test_windows_benchmarks_use_provisioned_data_volume_scratch(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")
        setup = (
            BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-windows-whp.ps1"
        ).read_text(encoding="utf-8")
        prepare = _composite_action_step(action, "Prepare Windows benchmark scratch")
        cleanup = _composite_action_step(action, "Remove Windows benchmark scratch")

        self.assertIn('$BenchmarkScratchVariable = "NVX_BENCHMARK_SCRATCH"', setup)
        self.assertIn(
            '[Environment]::GetEnvironmentVariable("NVX_BENCHMARK_SCRATCH", "Machine")',
            prepare,
        )
        self.assertIn('"NVX_BENCHMARK_SCRATCH_DIR=$Scratch"', prepare)
        self.assertIn("::warning::NVX_BENCHMARK_SCRATCH is not provisioned", prepare)
        self.assertIn("      if: always() && runner.os == 'Windows'", cleanup)
        # The cleanup follows the uploads, as a stalled volume can hold it up.
        self.assertLess(
            action.index("- name: Upload benchmark results"),
            action.index("- name: Remove Windows benchmark scratch"),
        )
        for step in (
            "Run Windows acceptance test",
            "Run Windows performance suite",
            "Run Windows multi-vCPU shell restore",
            "Run Windows device operation rates",
        ):
            with self.subTest(step=step):
                script = _composite_action_script(action, step)
                self.assertIn(
                    '$ScratchArgs = @("--scratch-dir", $env:NVX_BENCHMARK_SCRATCH_DIR)',
                    script,
                )
                self.assertIn("@ScratchArgs", script)
                self.assertLess(
                    action.index("- name: Prepare Windows benchmark scratch"),
                    action.index(f"- name: {step}"),
                )
        self.assertIn(
            "    Configure-SccacheEnvironment\n    Configure-BenchmarkScratch\n",
            setup,
        )
        self.assertIn(
            "Assert-ServiceDirectoryAcl -Path $SccacheDirectory -Writable\n"
            "        Assert-BenchmarkScratch\n",
            setup,
        )

    @unittest.skipUnless(os.name == "nt", "requires Windows PowerShell")
    def test_windows_acceptance_forwards_benchmark_scratch(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")
        script = _composite_action_script(action, "Run Windows acceptance test")
        for name, value in (
            ("backend", "whp"),
            ("platform", "windows-whp-virtual-machine"),
            ("warmups", "1"),
            ("runs", "10"),
            ("teardown-mode", "guest-exit"),
        ):
            script = script.replace("${{ inputs." + name + " }}", value)
        stub = """
function python {
    $global:LASTEXITCODE = 0
    if ($args[1] -ne "benchmark") {
        return
    }
    $output = $args[[Array]::IndexOf($args, "--output") + 1]
    $index = [Array]::IndexOf($args, "--scratch-dir")
    $scratch = ""
    if ($index -ge 0) {
        $scratch = $args[$index + 1]
    }
    New-Item -ItemType Directory -Path (Split-Path $output) -Force | Out-Null
    Set-Content -LiteralPath $output -Encoding UTF8 -Value (
        ConvertTo-Json -Compress @{ scratch = $scratch }
    )
}
"""
        for scratch in ("", r"F:\nvx-benchmark-scratch\job"):
            with (
                self.subTest(scratch=scratch),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary).resolve()
                environment = os.environ.copy()
                environment.pop("NVX_BENCHMARK_SCRATCH_DIR", None)
                if scratch:
                    environment["NVX_BENCHMARK_SCRATCH_DIR"] = scratch
                result = run_powershell(
                    [
                        "powershell.exe",
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        stub + script,
                    ],
                    cwd=root,
                    env=environment,
                )

                self.assertEqual(result.returncode, 0, result.stderr)
                accepted = (
                    root
                    / "data"
                    / "runs"
                    / "windows-whp-virtual-machine"
                    / MICROVM_RUN_DIRECTORY
                    / "1vcpu"
                    / "acceptance.json"
                )
                self.assertEqual(
                    json.loads(accepted.read_text(encoding="utf-8-sig")),
                    {"scratch": scratch},
                )

    @unittest.skipUnless(os.name == "nt", "requires Windows PowerShell")
    def test_windows_storage_events_steps_save_the_jobs_window(self):
        github = BuildConstants.REPO_ROOT / ".github"
        action = (github / "actions" / "run-benchmark" / "action.yml").read_text(
            encoding="utf-8"
        )
        platform = yaml.safe_load(
            (github / "workflows" / "run-platform.yml").read_text(encoding="utf-8")
        )
        record = next(
            step["run"]
            for step in platform["jobs"]["run"]["steps"]
            if step.get("name") == "Record Windows job start"
        )
        save = _composite_action_script(action, "Save Windows storage events")
        output_argument = '"data/runs/${{ inputs.platform }}/storage-events"'
        self.assertEqual(save.count(output_argument), 1)

        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "storage-events"
            github_env = Path(temporary) / "github-env"
            environment = dict(os.environ)
            # The step's python is the interpreter that runs these tests.
            environment["PATH"] = os.pathsep.join(
                (str(Path(sys.executable).parent), environment.get("PATH", ""))
            )
            environment["GITHUB_ENV"] = str(github_env)
            environment["NVX_BENCHMARK_SCRATCH_DIR"] = temporary

            def run(script: str) -> subprocess.CompletedProcess[str]:
                return run_powershell(
                    [
                        "powershell.exe",
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        script,
                    ],
                    cwd=BuildConstants.REPO_ROOT,
                    env=environment,
                    # The save step queries the host's event logs for up to
                    # 120 s each, so it gets longer than the shared limit.
                    timeout=300,
                )

            recorded = run(record)
            self.assertEqual(recorded.returncode, 0, recorded.stderr)
            name, _, started = (
                github_env.read_text(encoding="utf-8-sig").strip().partition("=")
            )
            self.assertEqual(name, "NVX_JOB_STARTED_AT")
            environment[name] = started
            saved = run(save.replace(output_argument, f'"{output}"'))
            summary = (output / "storage-events.txt").read_text(encoding="utf-8")

        self.assertEqual(saved.returncode, 0, saved.stdout + saved.stderr)
        self.assertRegex(
            summary.splitlines()[0], r"^Storage events from \d{4}-\d\d-\d\dT"
        )
        self.assertIn(f"storage events in {output}", saved.stdout)

    @unittest.skipUnless(os.name == "nt", "requires Windows PowerShell")
    def test_windows_benchmark_scratch_steps_manage_per_job_directories(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")
        machine_lookup = (
            '[Environment]::GetEnvironmentVariable("NVX_BENCHMARK_SCRATCH", "Machine")'
        )
        prepare = _composite_action_script(
            action, "Prepare Windows benchmark scratch"
        ).replace("${{ inputs.platform }}", "windows-whp-virtual-machine")
        cleanup = _composite_action_script(action, "Remove Windows benchmark scratch")
        self.assertIn(machine_lookup, prepare)

        def run_step(script: str, environment: dict[str, str]):
            return run_powershell(
                ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", script],
                env=environment,
            )

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve() / "scratch"
            root.mkdir()
            stale = root / "windows-whp-virtual-machine-1-1"
            fresh = root / "windows-whp-virtual-machine-2-1"
            for directory in (stale, fresh):
                directory.mkdir()
                (directory / "memory.bin").write_bytes(b"ram")
            old = time.time() - 7 * 60 * 60
            os.utime(stale, (old, old))
            github_env = Path(temporary) / "github-env.txt"
            github_env.write_text("", encoding="utf-8")
            environment = os.environ.copy()
            environment.update(
                {
                    "GITHUB_ENV": str(github_env),
                    "GITHUB_RUN_ID": "35805840174",
                    "GITHUB_RUN_ATTEMPT": "2",
                }
            )

            result = run_step(
                prepare.replace(machine_lookup, f"'{root}'"),
                environment,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            scratch = root / "windows-whp-virtual-machine-35805840174-2"
            self.assertTrue(scratch.is_dir())
            self.assertFalse(stale.exists())
            self.assertTrue(fresh.is_dir())
            self.assertEqual(
                github_env.read_text(encoding="utf-8-sig").splitlines(),
                [f"NVX_BENCHMARK_SCRATCH_DIR={scratch}"],
            )

            locked = scratch / "locked.bin"
            with locked.open("wb"):
                result = run_step(
                    cleanup,
                    {**environment, "NVX_BENCHMARK_SCRATCH_DIR": str(scratch)},
                )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(
                "::warning::Could not remove benchmark scratch", result.stdout
            )

            result = run_step(
                cleanup,
                {**environment, "NVX_BENCHMARK_SCRATCH_DIR": str(scratch)},
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(scratch.exists())

            github_env.write_text("", encoding="utf-8")
            result = run_step(prepare.replace(machine_lookup, "''"), environment)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(
                "::warning::NVX_BENCHMARK_SCRATCH is not provisioned", result.stdout
            )
            self.assertEqual(github_env.read_text(encoding="utf-8"), "")

    @unittest.skipUnless(os.name == "nt", "requires Windows PowerShell")
    def test_windows_setup_selects_largest_data_volume_for_benchmark_scratch(self):
        setup = BuildConstants.REPO_ROOT / "scripts" / "setup" / "setup-windows-whp.ps1"
        harness = f"""
Set-StrictMode -Version 2.0
$ErrorActionPreference = "Stop"
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    '{setup}',
    [ref]$tokens,
    [ref]$errors
)
if ($errors.Count -ne 0) {{
    throw "setup script has parse errors"
}}
foreach ($name in "Test-SystemVolumePath", "Get-BenchmarkScratchDirectory") {{
    $definition = $ast.Find({{
            param($node)
            $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
            $node.Name -eq $name
        }}, $true)
    . ([scriptblock]::Create($definition.Extent.Text))
}}
$BenchmarkScratchVariable = "NVX_TEST_UNSET_{uuid.uuid4().hex}"
$BenchmarkScratchName = "nvx-benchmark-scratch"
$BenchmarkScratchDirectory = $null
$env:SystemDrive = "C:"
$Volumes = @(
    [pscustomobject]@{{ DriveLetter = [char]"C"; DriveType = "Fixed"; FileSystem = "NTFS"; Size = 900GB }},
    [pscustomobject]@{{ DriveLetter = [char]"D"; DriveType = "Fixed"; FileSystem = "NTFS"; Size = 64GB }},
    [pscustomobject]@{{ DriveLetter = $null; DriveType = "Fixed"; FileSystem = "NTFS"; Size = 2TB }},
    [pscustomobject]@{{ DriveLetter = [char]"E"; DriveType = "CD-ROM"; FileSystem = ""; Size = 1TB }},
    [pscustomobject]@{{ DriveLetter = [char]"F"; DriveType = "Fixed"; FileSystem = "NTFS"; Size = 512GB }},
    [pscustomobject]@{{ DriveLetter = [char]"G"; DriveType = "Fixed"; FileSystem = "FAT32"; Size = 1TB }}
)
function Get-Volume {{
    $script:Volumes
}}
Write-Output (Get-BenchmarkScratchDirectory)
$Volumes = @($Volumes[0])
Write-Output ("none=" + ($null -eq (Get-BenchmarkScratchDirectory)))
$BenchmarkScratchDirectory = "H:\\explicit\\scratch"
Write-Output (Get-BenchmarkScratchDirectory)
"""

        result = run_powershell(
            ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", harness]
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.splitlines(),
            [
                r"F:\nvx-benchmark-scratch",
                "none=True",
                r"H:\explicit\scratch",
            ],
        )

    def test_cli_validation_runs_every_script_test_module(self):
        # Each scripts/test_*.py module must run in both CLI validation jobs;
        # the retired test_control_session.py once ran in neither (#229).
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "validate-nvx"
            / "action.yml"
        ).read_text(encoding="utf-8")
        modules = {
            path.name
            for path in (BuildConstants.REPO_ROOT / "scripts").glob("test_*.py")
        }
        self.assertIn("test_nvx_tools.py", modules)
        for step in ("Validate NVX CLI on Linux", "Validate NVX CLI on Windows"):
            with self.subTest(step=step):
                script = _composite_action_script(action, step)
                invoked = re.findall(r"(?:^|\s)scripts[/\\](test_\w+\.py)", script)
                self.assertEqual(sorted(invoked), sorted(modules))

    def test_ci_runs_the_public_sandbox_acceptance_on_every_backend(self):
        workflows = BuildConstants.REPO_ROOT / ".github" / "workflows"
        workflow = (workflows / "ci.yml").read_text(encoding="utf-8")
        microvm_workflow = (workflows / "run-nvx-microvm-tests.yml").read_text(
            encoding="utf-8"
        )
        for backend in ("kvm", "mshv", "whp"):
            with self.subTest(backend=backend):
                # The acceptance runs in the job beside the default suite.
                job = _workflow_job(workflow, f"nvx-microvm-guests-{backend}")
                self.assertIn(
                    "uses: ./.github/workflows/run-nvx-microvm-tests.yml", job
                )
                self.assertIn(f"backend: {backend}", job)
                self.assertIn("guests-and-sandboxes: true", job)
                self.assertNotIn("debug-kernel: true", job)
        for system in ("Linux", "Windows"):
            with self.subTest(system=system):
                acceptance = _workflow_step(
                    microvm_workflow,
                    "test",
                    f"Run public managed sandbox acceptance on {system}",
                )
                self.assertIn(
                    "inputs.guests-and-sandboxes && !inputs.debug-kernel", acceptance
                )
                self.assertIn("--scenario managed-exec-config", acceptance)
                self.assertIn("--scenario sandbox-lifecycle", acceptance)
                self.assertIn("--scenario sandbox-proxy-only", acceptance)
                self.assertIn("--scenario sandbox-ram-upper", acceptance)
                self.assertIn("--scenario sandbox-gpt-layer", acceptance)
                self.assertIn("--scenario vm-lifecycle", acceptance)
                self.assertIn("--scenario snapshot-core", acceptance)
                self.assertIn("--scenario snapshot-tiers", acceptance)
                self.assertIn("--scenario untiered-network-policy", acceptance)
                self.assertIn(
                    '--output-dir "build/test-results/public-exec-${{ inputs.backend }}"',
                    acceptance,
                )
                smoke = _workflow_step(
                    microvm_workflow,
                    "test",
                    f"Run Ubuntu sandbox layer smoke test on {system}",
                )
                self.assertIn("--entrypoint /sbin/nvx-sandbox-smoke", smoke)
                # The VM is the limit of the sandbox's workload.
                for option in ("--arg limits", "--memory-max", "--pids-max"):
                    self.assertNotIn(option, smoke)
        upload = _workflow_step(
            microvm_workflow, "test", "Upload NVX microVM failure logs"
        )
        self.assertIn("build/test-results/public-exec-${{ inputs.backend }}", upload)

    def test_every_microvm_job_uploads_its_scenario_durations(self):
        # Each microVM job, the guest and sandbox and debug-kernel ones and
        # each shard of a default suite too, keeps the duration report of
        # every test-microvm step whether it passes, fails, or is cancelled,
        # under a name of its own in each attempt (#694, #695, #696).
        workflows = BuildConstants.REPO_ROOT / ".github" / "workflows"
        steps = {
            step["name"]: step
            for step in yaml.safe_load(
                (workflows / "run-nvx-microvm-tests.yml").read_text(encoding="utf-8")
            )["jobs"]["test"]["steps"]
        }
        upload = steps["Upload NVX microVM scenario durations"]
        self.assertEqual(upload["if"], "always()")
        self.assertRegex(upload["uses"], r"^actions/upload-artifact@")
        self.assertEqual(
            upload["with"]["path"],
            f"build/test-results/*/{microvm_durations.REPORT_NAME}",
        )
        # The path's pattern finds the report of every test-microvm step.
        microvm_runs = [
            step["run"]
            for step in steps.values()
            if "nvx.py test-microvm" in step.get("run", "")
        ]
        self.assertTrue(microvm_runs)
        for run in microvm_runs:
            with self.subTest(run=run):
                self.assertEqual(
                    len(re.findall(r'--output-dir "build/test-results/[^"/*]+"', run)),
                    1,
                )
        # The failure logs keep their upload, only when a step fails.
        self.assertEqual(steps["Upload NVX microVM failure logs"]["if"], "failure()")

        kernel = "${{ inputs.debug-kernel && 'debug' || 'default' }}"
        part = "${{ inputs.guests-and-sandboxes && '-guests' || '' }}"
        shard = "${{ inputs.shard && format('-shard-{0}', inputs.shard) || '' }}"
        name = upload["with"]["name"]
        self.assertEqual(
            name,
            "nvx-microvm-durations-${{ runner.os }}-${{ inputs.backend }}-"
            + kernel
            + part
            + shard
            + "-${{ github.run_id }}-${{ github.run_attempt }}",
        )
        jobs = {
            job_name: job
            for job_name, job in yaml.safe_load(
                (workflows / "ci.yml").read_text(encoding="utf-8")
            )["jobs"].items()
            if job.get("uses") == "./.github/workflows/run-nvx-microvm-tests.yml"
        }
        self.assertEqual(
            set(jobs),
            {
                *ci.REQUIRED_CI_MICROVM_TEST_JOBS,
                *ci.REQUIRED_CI_MICROVM_DEBUG_JOBS,
                *ci.REQUIRED_CI_MICROVM_DEBUG_PUSH_JOBS,
            },
        )
        artifacts: set[str] = set()
        legs = 0
        for job_name, job in jobs.items():
            # Each shard of a default suite is a leg of its job's matrix.
            for inputs in _matrix_legs(job):
                legs += 1
                system = (
                    "Windows" if "windows" in json.loads(inputs["runner"]) else "Linux"
                )
                debug = inputs.get("debug-kernel", False) is True
                guests = inputs.get("guests-and-sandboxes", False) is True
                index = inputs.get("shard", 0)
                for attempt in (1, 2):
                    artifact = (
                        name.replace("${{ runner.os }}", system)
                        .replace("${{ inputs.backend }}", inputs["backend"])
                        .replace(kernel, "debug" if debug else "default")
                        .replace(part, "-guests" if guests else "")
                        .replace(shard, f"-shard-{index}" if index else "")
                        .replace("${{ github.run_id }}", "39585909588")
                        .replace("${{ github.run_attempt }}", str(attempt))
                    )
                    with self.subTest(job=job_name, shard=index, attempt=attempt):
                        self.assertNotIn("${{", artifact)
                        self.assertNotIn(artifact, artifacts)
                        artifacts.add(artifact)
        self.assertIn("nvx-microvm-durations-Linux-kvm-debug-39585909588-2", artifacts)
        # Each shard of a backend's default suite and its guest and sandbox
        # steps keep their own reports.
        self.assertLessEqual(
            {
                "nvx-microvm-durations-Windows-whp-default-shard-1-39585909588-1",
                "nvx-microvm-durations-Windows-whp-default-shard-2-39585909588-1",
                "nvx-microvm-durations-Windows-whp-default-guests-39585909588-1",
            },
            artifacts,
        )
        self.assertEqual(len(artifacts), 2 * legs)

    @unittest.skipUnless(os.name == "nt", "requires Windows PowerShell")
    def test_windows_cli_validation_stops_at_each_failed_command(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "validate-nvx"
            / "action.yml"
        ).read_text(encoding="utf-8")
        script = _composite_action_script(action, "Validate NVX CLI on Windows")
        command_count = sum(line.startswith("python ") for line in script.splitlines())
        self.assertGreater(command_count, 0)
        stub = """
$CommandCount = 0
function python {
    $script:CommandCount += 1
    Write-Output "python-$script:CommandCount"
    $global:LASTEXITCODE = 0
    if ($script:CommandCount -eq $script:FailureAt) {
        $global:LASTEXITCODE = 37
    }
}
"""
        for failure_at in range(command_count + 1):
            with self.subTest(failure_at=failure_at):
                result = run_powershell(
                    [
                        "powershell.exe",
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        f"$FailureAt = {failure_at}\n{stub}\n{script}",
                    ]
                )
                self.assertEqual(
                    result.returncode, 37 if failure_at else 0, result.stderr
                )
                self.assertEqual(
                    result.stdout.splitlines(),
                    [
                        f"python-{index}"
                        for index in range(1, (failure_at or command_count) + 1)
                    ],
                )

    @unittest.skipUnless(os.name == "nt", "requires Windows PowerShell")
    def test_windows_acceptance_preserves_attempts_and_publishes_only_valid_data(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")
        script = _composite_action_script(action, "Run Windows acceptance test")
        for name, value in (
            ("backend", "whp"),
            ("platform", "windows-whp-virtual-machine"),
            ("warmups", "1"),
            ("runs", "10"),
            ("teardown-mode", "guest-exit"),
        ):
            script = script.replace("${{ inputs." + name + " }}", value)
        stub = """
$BenchmarkCount = 0
$ValidationCount = 0
function python {
    if ($args[1] -eq "benchmark") {
        $script:BenchmarkCount += 1
        $output = $args[[Array]::IndexOf($args, "--output") + 1]
        New-Item -ItemType Directory -Path (Split-Path $output) -Force | Out-Null
        Set-Content -LiteralPath $output -Encoding UTF8 -Value (
            '{"attempt":' + $script:BenchmarkCount + '}'
        )
        $global:LASTEXITCODE = $script:BenchmarkStatus
    }
    elseif ($args[1] -eq "performance") {
        $log = $args[[Array]::IndexOf($args, "--attempt-log") + 1]
        Add-Content -LiteralPath "attempt-logs.txt" -Value $log
        $global:LASTEXITCODE = $script:ValidationStatuses[$script:ValidationCount]
        $script:ValidationCount += 1
    }
    else {
        throw "unexpected command: $args"
    }
}
"""
        for name, statuses, benchmark_status in (
            ("first-success", (0,), 0),
            ("remeasured-success", (75, 0), 0),
            ("both-unstable", (75, 75), 0),
            ("invalid-data", (1,), 0),
            ("benchmark-failure", (0,), 42),
        ):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                output = (
                    root
                    / "data"
                    / "runs"
                    / "windows-whp-virtual-machine"
                    / MICROVM_RUN_DIRECTORY
                    / "1vcpu"
                )
                output.mkdir(parents=True)
                accepted = output / "acceptance.json"
                accepted.write_text('{"attempt":"stale"}', encoding="utf-8")
                # A rerun in the same directory must not keep a previous
                # run's attempt log or later attempts.
                attempt_log = output / "acceptance-attempts.json"
                attempt_log.write_text('{"attempts":"stale"}', encoding="utf-8")
                (output / "acceptance-attempt-2.json").write_text(
                    '{"attempt":"stale"}', encoding="utf-8"
                )
                setup = (
                    "$ValidationStatuses = @("
                    + ", ".join(str(status) for status in statuses)
                    + f")\n$BenchmarkStatus = {benchmark_status}\n"
                )
                result = run_powershell(
                    [
                        "powershell.exe",
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        setup + stub + script,
                    ],
                    cwd=root,
                )
                expected_status = benchmark_status or statuses[-1]
                self.assertEqual(result.returncode, expected_status, result.stderr)
                self.assertFalse(attempt_log.exists())
                validations = root / "attempt-logs.txt"
                self.assertEqual(
                    (
                        validations.read_text(encoding="utf-8").splitlines()
                        if validations.exists()
                        else []
                    ),
                    [
                        f"data/runs/windows-whp-virtual-machine/{MICROVM_RUN_DIRECTORY}/1vcpu/"
                        "acceptance-attempts.json"
                    ]
                    * (0 if benchmark_status else len(statuses)),
                )
                self.assertEqual(
                    sorted(path.name for path in output.glob("acceptance-attempt-*")),
                    [
                        f"acceptance-attempt-{index}.json"
                        for index in range(1, len(statuses) + 1)
                    ],
                )
                for index in range(1, len(statuses) + 1):
                    attempt = output / f"acceptance-attempt-{index}.json"
                    self.assertEqual(
                        json.loads(attempt.read_text(encoding="utf-8-sig")),
                        {"attempt": index},
                    )
                self.assertEqual(accepted.exists(), expected_status == 0)
                if expected_status == 0:
                    self.assertEqual(
                        json.loads(accepted.read_text(encoding="utf-8-sig")),
                        {"attempt": len(statuses)},
                    )

    @unittest.skipUnless(os.name == "posix", "requires bash")
    def test_linux_acceptance_preserves_attempts_and_publishes_only_valid_data(self):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")
        script = _harness_in_foreground(
            _composite_action_script(action, "Run Linux acceptance test")
        )
        platform = "linux-kvm-virtual-machine"
        for name, value in (
            ("backend", "kvm"),
            ("platform", platform),
            ("warmups", "1"),
            ("runs", "10"),
            ("teardown-mode", "guest-exit"),
        ):
            script = script.replace("${{ inputs." + name + " }}", value)
        stub = """
benchmark_count=0
validation_count=0
python3() {
    local argument output="" input="" platform="" attempt_log="" previous=""
    for argument in "$@"; do
        case "${previous}" in
        --output) output="${argument}" ;;
        --input) input="${argument}" ;;
        --platform) platform="${argument}" ;;
        --attempt-log) attempt_log="${argument}" ;;
        esac
        previous="${argument}"
    done
    if [[ "${2-}" == benchmark ]]; then
        benchmark_count=$((benchmark_count + 1))
        echo "benchmark ${platform} ${output}"
        mkdir -p "$(dirname "${output}")"
        printf '{"attempt":%d}' "${benchmark_count}" >"${output}"
        return "${BENCHMARK_STATUS}"
    fi
    if [[ "${2-} ${3-}" == "performance validate-openvmm" ]]; then
        validation_count=$((validation_count + 1))
        echo "validate ${platform} ${input} ${attempt_log}"
        return "${VALIDATION_STATUSES[validation_count - 1]}"
    fi
    echo "unexpected command: $*" >&2
    return 1
}
"""
        environment = os.environ.copy()
        environment.update(
            {
                "NVX_BENCHMARK_CPUS": "0-7",
                "NVX_BENCHMARK_HOST_CPU_RESERVE": "0",
                "NVX_BENCHMARK_MARKER_TIMEOUT": "40",
            }
        )
        run_dir = f"data/runs/{platform}/{MICROVM_RUN_DIRECTORY}/1vcpu"
        for name, statuses, benchmark_status in (
            ("first-success", (0,), 0),
            ("remeasured-success", (75, 0), 0),
            ("both-unstable", (75, 75), 0),
            ("invalid-data", (2,), 0),
            ("unstable-then-invalid", (75, 2), 0),
            ("benchmark-failure", (0,), 42),
        ):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                output = root / run_dir
                output.mkdir(parents=True)
                accepted = output / "acceptance.json"
                accepted.write_text('{"attempt":"stale"}', encoding="utf-8")
                # A rerun in the same directory must not keep a previous
                # run's attempt log or later attempts.
                attempt_log = output / "acceptance-attempts.json"
                attempt_log.write_text('{"attempts":"stale"}', encoding="utf-8")
                (output / "acceptance-attempt-2.json").write_text(
                    '{"attempt":"stale"}', encoding="utf-8"
                )
                setup = (
                    "VALIDATION_STATUSES=("
                    + " ".join(str(status) for status in statuses)
                    + f")\nBENCHMARK_STATUS={benchmark_status}\n"
                )
                result = subprocess.run(
                    [
                        "bash",
                        "--noprofile",
                        "--norc",
                        "-eo",
                        "pipefail",
                        "-c",
                        setup + stub + script,
                    ],
                    cwd=root,
                    env=environment,
                    capture_output=True,
                    text=True,
                    timeout=30,
                    check=False,
                )
                expected_status = benchmark_status or statuses[-1]
                self.assertEqual(result.returncode, expected_status, result.stderr)
                self.assertFalse(attempt_log.exists())
                expected_stdout: list[str] = []
                for index in range(1, len(statuses) + 1):
                    attempt = f"{run_dir}/acceptance-attempt-{index}.json"
                    if index > 1:
                        expected_stdout.append(
                            "::warning::Lifecycle snapshot generation was unstable "
                            f"on attempt {index - 1}; remeasuring once"
                        )
                    expected_stdout.append(f"benchmark {platform} {attempt}")
                    if not benchmark_status:
                        expected_stdout.append(
                            f"validate {platform} {attempt} "
                            f"{run_dir}/acceptance-attempts.json"
                        )
                self.assertEqual(result.stdout.splitlines(), expected_stdout)
                self.assertEqual(
                    sorted(path.name for path in output.glob("acceptance-attempt-*")),
                    [
                        f"acceptance-attempt-{index}.json"
                        for index in range(1, len(statuses) + 1)
                    ],
                )
                for index in range(1, len(statuses) + 1):
                    attempt = output / f"acceptance-attempt-{index}.json"
                    self.assertEqual(
                        json.loads(attempt.read_text(encoding="utf-8")),
                        {"attempt": index},
                    )
                self.assertEqual(accepted.exists(), expected_status == 0)
                if expected_status == 0:
                    self.assertEqual(
                        json.loads(accepted.read_text(encoding="utf-8")),
                        {"attempt": len(statuses)},
                    )

    def _assert_restore_attempts(
        self,
        root: Path,
        platform: str,
        suite: str,
        processor_counts: Sequence[int],
        measured: dict[tuple[int, int], int],
        published: dict[int, int],
    ) -> None:
        for processors in processor_counts:
            run_dir = root / "data" / "runs" / platform / MICROVM_RUN_DIRECTORY
            run_dir /= f"{processors}vcpu"
            attempts = sorted(
                attempt for count, attempt in measured if count == processors
            )
            # Every attempt keeps its logs, and none survives from an earlier run.
            self.assertEqual(
                sorted(path.name for path in run_dir.glob(f"{suite}-attempt-*")),
                [f"{suite}-attempt-{attempt}" for attempt in attempts],
            )
            for attempt in attempts:
                log = run_dir / f"{suite}-attempt-{attempt}" / f"{suite}.log"
                self.assertEqual(
                    log.read_text(encoding="utf-8-sig").strip(),
                    f"attempt {measured[processors, attempt]}",
                )
            # Only a validated attempt is published for collection.
            self.assertEqual(
                (run_dir / f"{suite}.log").exists(), processors in published
            )
            self.assertEqual(
                (run_dir / "benchmark-metadata.json").exists(),
                processors in published,
            )
            if processors in published:
                self.assertEqual(
                    (run_dir / f"{suite}.log").read_text(encoding="utf-8-sig").strip(),
                    f"attempt {published[processors]}",
                )
                self.assertEqual(
                    json.loads(
                        (run_dir / "benchmark-metadata.json").read_text(
                            encoding="utf-8-sig"
                        )
                    ),
                    {"attempt": published[processors]},
                )

    @unittest.skipUnless(os.name == "posix", "requires bash")
    def test_linux_restore_suites_preserve_attempts_and_publish_only_valid_data(
        self,
    ):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")
        platform = "linux-mshv-virtual-machine"
        stub = r"""
benchmark_count=0
validation_count=0
python3() {
    local argument previous="" suite="" processors="" output_dir="" input_dir=""
    local platform=""
    for argument in "$@"; do
        case "${previous}" in
        --suite) suite="${argument}" ;;
        --processors) processors="${argument}" ;;
        --output-dir) output_dir="${argument}" ;;
        --input-dir) input_dir="${argument}" ;;
        --platform) platform="${argument}" ;;
        esac
        previous="${argument}"
    done
    if [[ "${2-}" == benchmark ]]; then
        benchmark_count=$((benchmark_count + 1))
        echo "benchmark ${suite} ${processors} ${output_dir}"
        mkdir -p "${output_dir}"
        printf 'attempt %d\n' "${benchmark_count}" >"${output_dir}/${suite}.log"
        printf '{"attempt":%d}' "${benchmark_count}" \
            >"${output_dir}/benchmark-metadata.json"
        return "${BENCHMARK_STATUS}"
    fi
    if [[ "${2-} ${3-}" == "performance validate-workload" ]]; then
        validation_count=$((validation_count + 1))
        echo "validate ${platform} ${input_dir}"
        return "${VALIDATION_STATUSES[validation_count - 1]}"
    fi
    echo "unexpected command: $*" >&2
    return 1
}
"""
        environment = os.environ.copy()
        environment.update(
            {
                "NVX_BENCHMARK_CPUS": "0-7",
                "NVX_BENCHMARK_HOST_CPU_RESERVE": "0",
                "NVX_BENCHMARK_MARKER_TIMEOUT": "40",
            }
        )
        for step, suite, processor_counts, scenarios in RESTORE_REMEASUREMENT_STEPS:
            script = _harness_in_foreground(
                _composite_action_script(action, f"Run Linux {step}")
            )
            for name, value in (
                ("backend", "mshv"),
                ("platform", platform),
                ("warmups", "1"),
                ("runs", "10"),
                ("teardown-mode", "guest-exit"),
            ):
                script = script.replace("${{ inputs." + name + " }}", value)
            for name, statuses, benchmark_status in scenarios:
                with (
                    self.subTest(step=step, name=name),
                    tempfile.TemporaryDirectory() as temporary,
                ):
                    root = Path(temporary).resolve()
                    # A rerun in the same directory must not keep an attempt
                    # of the previous run.
                    stale = root / "data" / "runs" / platform / MICROVM_RUN_DIRECTORY
                    stale /= f"{processor_counts[0]}vcpu/{suite}-attempt-2"
                    stale.mkdir(parents=True)
                    (stale / f"{suite}.log").write_text("stale\n", encoding="utf-8")
                    setup = (
                        "VALIDATION_STATUSES=("
                        + " ".join(str(status) for status in statuses)
                        + f")\nBENCHMARK_STATUS={benchmark_status}\n"
                    )
                    result = subprocess.run(
                        [
                            "bash",
                            "--noprofile",
                            "--norc",
                            "-eo",
                            "pipefail",
                            "-c",
                            setup + stub + script,
                        ],
                        cwd=root,
                        env=environment,
                        capture_output=True,
                        text=True,
                        timeout=30,
                        check=False,
                    )

                    expected_status, lines, measured, published = (
                        _expected_restore_remeasurement(
                            platform,
                            suite,
                            processor_counts,
                            statuses,
                            benchmark_status,
                        )
                    )
                    self.assertEqual(result.returncode, expected_status, result.stderr)
                    self.assertEqual(result.stdout.splitlines(), lines)
                    self._assert_restore_attempts(
                        root, platform, suite, processor_counts, measured, published
                    )

    def test_a_cancellation_reaches_the_linux_test_harness(self):
        # #946: the runner signals only a cancelled step's shell, and Bash
        # runs a trap only once its foreground command ends, so the harness
        # ran on until the runner's orphan cleanup killed it, which left its
        # scratch directories behind. Each Linux step execs the harness, or
        # runs it with run_harness, which passes a cancellation on to it.
        github = BuildConstants.REPO_ROOT / ".github"
        workflow = yaml.safe_load(
            (github / "workflows" / "run-nvx-microvm-tests.yml").read_text(
                encoding="utf-8"
            )
        )
        harness = "python3 scripts/nvx.py test-microvm"
        steps = [
            step
            for job in workflow["jobs"].values()
            for step in job["steps"]
            if step.get("shell") == "bash" and harness in step.get("run", "")
        ]
        self.assertTrue(steps)
        for step in steps:
            with self.subTest(step=step["name"]):
                self.assertTrue(step["run"].startswith(f"exec {harness} "))

        action = yaml.safe_load(
            (github / "actions" / "run-benchmark" / "action.yml").read_text(
                encoding="utf-8"
            )
        )
        harness = "python3 scripts/nvx.py benchmark"
        scripts = {
            step["name"]: step["run"]
            for step in action["runs"]["steps"]
            if step.get("shell") == "bash" and harness in step.get("run", "")
        }
        self.assertEqual(
            set(scripts),
            {
                "Run Linux acceptance test",
                "Run Linux performance suite",
                "Run Linux multi-vCPU shell restore",
                "Run Linux device operation rates",
            },
        )
        for name, script in scripts.items():
            with self.subTest(step=name):
                commands = [
                    line.strip() for line in script.splitlines() if harness in line
                ]
                if commands == [f"exec {harness} \\"]:
                    continue
                self.assertTrue(commands)
                self.assertTrue(
                    all(command == f"run_harness {harness} \\" for command in commands)
                )
                self.assertLess(
                    script.index(_FORWARD_SIGNALS_SOURCE), script.index("run_harness")
                )
        # The quality checks lint the file that the steps source.
        quality = (github / "actions" / "check-quality" / "action.yml").read_text(
            encoding="utf-8"
        )
        for linter in ("${SHELLCHECK} --shell=bash", "${SHFMT} -d -ln bash"):
            with self.subTest(linter=linter):
                checked = quality.split(linter, 1)[1].split("\n\n", 1)[0]
                checked = checked.split("${", 1)[0]
                self.assertIn(
                    ".github/actions/run-benchmark/forward-signals.sh", checked
                )

    @unittest.skipUnless(sys.platform.startswith("linux"), "Linux signals")
    def test_run_harness_passes_a_cancellation_on_to_the_harness(self):
        if signal.getsignal(signal.SIGINT) == signal.SIG_IGN:
            self.skipTest("Bash cannot trap a SIGINT that this process ignores")
        for name, signum, delay, status, mode, expected in (
            # The runner's SIGINT, and its SIGTERM, reach the harness, which
            # cleans up, and the step then fails with the status of the
            # signal.
            ("sigint", signal.SIGINT, 60, 0, "handle", 130),
            ("sigterm", signal.SIGTERM, 60, 0, "handle", 143),
            # Without a cancellation, run_harness returns the harness's
            # status, which set -e turns into the step's.
            ("success", None, 0, 0, "handle", 0),
            ("failure", None, 0, 3, "handle", 3),
            # A cancellation that doesn't stop the harness, as one that
            # arrives as it exits, still ends the step once it exits.
            ("late", signal.SIGINT, 1, 0, "ignore", 130),
            # A cancellation between two harness runs ends the step.
            ("between", signal.SIGINT, 0, 0, "handle", 130),
        ):
            with (
                self.subTest(name=name),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                started, cleaned = root / "started", root / "cleaned"
                waiting, finished = root / "waiting", root / "finished"
                run = shlex.join(
                    [
                        "run_harness",
                        sys.executable,
                        "-c",
                        _CANCELLABLE_HARNESS,
                        str(started),
                        str(cleaned),
                        str(delay),
                        str(status),
                        mode,
                    ]
                )
                script = (
                    "set -euo pipefail\n"
                    f"source {shlex.quote(str(_FORWARD_SIGNALS))}\n"
                    + (
                        f"{run}\ntouch {shlex.quote(str(waiting))}\nsleep 2\n"
                        if name == "between"
                        else ""
                    )
                    + f"{run}\ntouch {shlex.quote(str(finished))}\n"
                )
                shell = subprocess.Popen(
                    ["bash", "--noprofile", "--norc", "-c", script]
                )
                try:
                    if signum is not None:
                        ready = waiting if name == "between" else started
                        deadline = time.monotonic() + 30
                        while not ready.exists():
                            self.assertLess(time.monotonic(), deadline)
                            time.sleep(0.01)
                        if name == "between":
                            started.unlink()
                        shell.send_signal(signum)
                    self.assertEqual(shell.wait(timeout=30), expected)
                finally:
                    if shell.poll() is None:
                        shell.kill()
                        shell.wait()
                self.assertEqual(finished.exists(), expected == 0)
                if name in ("sigint", "sigterm"):
                    self.assertTrue(cleaned.exists())
                if name == "between":
                    # The step ended before it started the second harness.
                    self.assertFalse(started.exists())

    def test_run_harness_passes_on_a_cancellation_that_arrives_as_it_starts(self):
        # A cancellation that arrived once run_harness had started the
        # harness, but before it knew the harness's PID, ended the step at
        # once and left the harness to run on. Passed on as SIGINT, it would
        # also be lost on a harness that had yet to restore the SIGINT that
        # Bash ignores in a background command.
        if sys.platform != "linux":
            self.skipTest("Linux signals")
        if signal.getsignal(signal.SIGINT) == signal.SIG_IGN:
            self.skipTest("Bash cannot trap a SIGINT that this process ignores")
        for signum, expected in ((signal.SIGINT, 130), (signal.SIGTERM, 143)):
            with (
                self.subTest(signal=signum.name),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                started, cleaned = root / "started", root / "cleaned"
                recorded, finished = root / "harness-pid", root / "finished"
                # With functrace, a DEBUG trap runs before each command of
                # run_harness too, so the shell signals itself right after it
                # starts the harness, before the first command that runs once
                # $! holds the harness's PID.
                script = (
                    "set -euo pipefail\n"
                    f"source {shlex.quote(str(_FORWARD_SIGNALS))}\n"
                    "cancel() {\n"
                    f'    if [[ -n "${{!:-}}" && ! -e {shlex.quote(str(recorded))} ]]; then\n'
                    f'        echo "$!" >{shlex.quote(str(recorded))}\n'
                    f'        kill -{signum.name.removeprefix("SIG")} "$$"\n'
                    "    fi\n"
                    "}\n"
                    "set -T\n"
                    "trap cancel DEBUG\n"
                    + shlex.join(
                        [
                            "run_harness",
                            sys.executable,
                            "-c",
                            _CANCELLABLE_HARNESS,
                            str(started),
                            str(cleaned),
                            "60",
                            "0",
                            "handle",
                        ]
                    )
                    + f"\ntouch {shlex.quote(str(finished))}\n"
                )
                shell = subprocess.Popen(
                    ["bash", "--noprofile", "--norc", "-c", script]
                )
                try:
                    # Unless the signal reached it, the harness runs for a
                    # minute.
                    self.assertEqual(shell.wait(timeout=30), expected)
                finally:
                    if shell.poll() is None:
                        shell.kill()
                        shell.wait()
                    outlived = False
                    if recorded.exists():
                        pid = int(recorded.read_text(encoding="utf-8"))
                        with contextlib.suppress(ProcessLookupError):
                            os.kill(pid, 0)
                            outlived = True
                            os.kill(pid, signal.SIGKILL)
                self.assertTrue(recorded.exists(), "the shell never cancelled itself")
                # The step ended only once the harness had exited.
                self.assertFalse(outlived)
                self.assertFalse(finished.exists())
                # The signal stopped the harness as it started, or it cleaned up.
                self.assertTrue(cleaned.exists() or not started.exists())

    @unittest.skipUnless(os.name == "nt", "requires Windows PowerShell")
    def test_windows_restore_suites_preserve_attempts_and_publish_only_valid_data(
        self,
    ):
        action = (
            BuildConstants.REPO_ROOT
            / ".github"
            / "actions"
            / "run-benchmark"
            / "action.yml"
        ).read_text(encoding="utf-8")
        platform = "windows-whp-virtual-machine"
        stub = """
$BenchmarkCount = 0
$ValidationCount = 0
function python {
    $Options = @{}
    for ($Index = 2; $Index -lt $args.Count - 1; $Index++) {
        $Options["$($args[$Index])"] = "$($args[$Index + 1])"
    }
    if ($args[1] -eq "benchmark") {
        $script:BenchmarkCount += 1
        $Output = $Options["--output-dir"]
        $Suite = $Options["--suite"]
        Write-Output "benchmark $Suite $($Options['--processors']) $Output"
        New-Item -ItemType Directory -Path $Output -Force | Out-Null
        Set-Content -LiteralPath "$Output/$Suite.log" `
            -Value "attempt $($script:BenchmarkCount)"
        Set-Content -LiteralPath "$Output/benchmark-metadata.json" `
            -Value ('{"attempt":' + $script:BenchmarkCount + '}')
        $global:LASTEXITCODE = $script:BenchmarkStatus
    }
    elseif ("$($args[1]) $($args[2])" -eq "performance validate-workload") {
        Write-Output "validate $($Options['--platform']) $($Options['--input-dir'])"
        $global:LASTEXITCODE = $script:ValidationStatuses[$script:ValidationCount]
        $script:ValidationCount += 1
    }
    else {
        throw "unexpected command: $args"
    }
}
"""
        for step, suite, processor_counts, scenarios in RESTORE_REMEASUREMENT_STEPS:
            script = _composite_action_script(action, f"Run Windows {step}")
            for name, value in (
                ("backend", "whp"),
                ("platform", platform),
                ("warmups", "1"),
                ("runs", "10"),
                ("teardown-mode", "guest-exit"),
            ):
                script = script.replace("${{ inputs." + name + " }}", value)
            for name, statuses, benchmark_status in scenarios:
                with (
                    self.subTest(step=step, name=name),
                    tempfile.TemporaryDirectory() as temporary,
                ):
                    root = Path(temporary).resolve()
                    # A rerun in the same directory must not keep an attempt
                    # of the previous run.
                    stale = root / "data" / "runs" / platform / MICROVM_RUN_DIRECTORY
                    stale /= f"{processor_counts[0]}vcpu/{suite}-attempt-2"
                    stale.mkdir(parents=True)
                    (stale / f"{suite}.log").write_text("stale\n", encoding="utf-8")
                    setup = (
                        "$ValidationStatuses = @("
                        + ", ".join(str(status) for status in statuses)
                        + f")\n$BenchmarkStatus = {benchmark_status}\n"
                    )
                    result = run_powershell(
                        [
                            "powershell.exe",
                            "-NoProfile",
                            "-NonInteractive",
                            "-Command",
                            setup + stub + script,
                        ],
                        cwd=root,
                    )

                    expected_status, lines, measured, published = (
                        _expected_restore_remeasurement(
                            platform,
                            suite,
                            processor_counts,
                            statuses,
                            benchmark_status,
                        )
                    )
                    self.assertEqual(result.returncode, expected_status, result.stderr)
                    self.assertEqual(result.stdout.splitlines(), lines)
                    self._assert_restore_attempts(
                        root, platform, suite, processor_counts, measured, published
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


class BuildConstantsTests(unittest.TestCase):
    def test_constants_are_grouped_class_namespaces(self):
        for namespace in (
            BuildConstants,
            KernelBuildConstants,
            OpenVMMBuildConstants,
            AlpineBuildConstants,
            UbuntuBuildConstants,
            InitramfsBuildConstants,
            DockerBuildConstants,
            ZstdBuildConstants,
            ReleaseBuildConstants,
        ):
            with self.subTest(namespace=namespace.__name__):
                self.assertEqual(namespace.__module__, "nvx_tools.build_constants")
                self.assertTrue(
                    all(
                        name.isupper()
                        for name in vars(namespace)
                        if not name.startswith("__")
                    )
                )

    def test_build_pins_and_artifact_names_have_one_literal_owner(self):
        centralized = {
            KernelBuildConstants.VERSION,
            KernelBuildConstants.URL,
            KernelBuildConstants.SHA256,
            KernelBuildConstants.BINARY_NAME,
            KernelBuildConstants.CONFIG_NAME,
            KernelBuildConstants.PROVENANCE_NAME,
            KernelBuildConstants.DEBUG_BINARY_NAME,
            KernelBuildConstants.DEBUG_CONFIG_NAME,
            KernelBuildConstants.DEBUG_PROVENANCE_NAME,
            OpenVMMBuildConstants.GNU_RUST_TARGET,
            OpenVMMBuildConstants.MUSL_RUST_TARGET,
            OpenVMMBuildConstants.WINDOWS_RUST_TARGET,
            OpenVMMBuildConstants.PROVENANCE_NAME,
            OpenVMMBuildConstants.CONTROL_CONTRACT_REVISION,
            AlpineBuildConstants.VERSION,
            AlpineBuildConstants.BRANCH,
            AlpineBuildConstants.MINIROOTFS_SHA256,
            AlpineBuildConstants.INITRAMFS_NAME,
            AlpineBuildConstants.PACKAGE_MANIFEST_NAME,
            UbuntuBuildConstants.VERSION,
            UbuntuBuildConstants.BASE_URL,
            UbuntuBuildConstants.BASE_SHA256,
            UbuntuBuildConstants.INITRAMFS_NAME,
            UbuntuBuildConstants.PACKAGE_MANIFEST_NAME,
            UbuntuBuildConstants.DISTRO_NAME,
            UbuntuBuildConstants.DISTRO_MANIFEST_NAME,
            InitramfsBuildConstants.PROVENANCE_NAME,
            ZstdBuildConstants.VERSION,
            ZstdBuildConstants.SHA256,
        }
        duplicates: list[tuple[str, int, str]] = []
        for path in (BuildConstants.REPO_ROOT / "scripts").rglob("*.py"):
            if (
                path.name == "build_constants.py"
                or path.name.startswith("test_")
                or path.relative_to(BuildConstants.REPO_ROOT).as_posix()
                in check_test_layout.PYTHON_TEST_SUPPORT
            ):
                continue
            tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
            duplicates.extend(
                (path.name, node.lineno, node.value)
                for node in ast.walk(tree)
                if isinstance(node, ast.Constant)
                and isinstance(node.value, str)
                and node.value in centralized
            )
        self.assertEqual(duplicates, [])

    def test_old_module_level_constants_are_not_reexported(self):
        names = (
            "REPO_ROOT",
            "BUILD_DIR",
            "SOURCE_DIR",
            "OPENVMM_DIR",
            "DEFAULT_KERNEL_VERSION",
            "DEFAULT_KERNEL_URL",
            "DEFAULT_KERNEL_SHA256",
            "DEFAULT_ALPINE_VERSION",
            "DEFAULT_ALPINE_BRANCH",
            "DEFAULT_ALPINE_MINIROOTFS_SHA256",
            "DEFAULT_UBUNTU_VERSION",
            "DEFAULT_UBUNTU_CODENAME",
            "DEFAULT_UBUNTU_ARCHITECTURE",
            "DEFAULT_UBUNTU_BASE_URL",
            "DEFAULT_UBUNTU_BASE_SHA256",
            "UBUNTU_PACKAGE_LOCK",
            "UBUNTU_EROFS_FORMAT",
            "MICROVM_ABI_VERSION",
            "CONTROL_SESSION_PROTOCOL_VERSION",
            "CONTROL_CONTRACT_REVISION",
            "OPENVMM_PROVENANCE_NAME",
            "KERNEL_PROVENANCE_NAME",
            "INITRAMFS_PROVENANCE_NAME",
            "REQUIRED_VIRTIO_CONSOLE_CONFIG",
            "REQUIRED_SHARED_STATUS_KERNEL_CONFIG",
            "REQUIRED_SANDBOX_KERNEL_CONFIG",
            "ZSTD_VERSION",
            "ZSTD_ARCHIVE",
            "ZSTD_URL",
            "ZSTD_SHA256",
            "OPENVMM_RUST_TOOLCHAIN",
            "OPENVMM_GUEST_RUST_TARGET",
            "OPENVMM_UEFI_RUST_TARGET",
            "OPENVMM_LINUX_MUSL_RUST_TARGET",
            "OPENVMM_RUST_TARGETS",
            "APORTS_URL",
            "REPOSITORIES",
            "UBUNTU_ARCHIVE_KEYRING_URL",
            "UBUNTU_ARCHIVE_KEYRING_SHA256",
            "UBUNTU_SNAPSHOT_ARCHIVE_URL",
            "UBUNTU_SOURCE_INDEXES",
            "_UBUNTU_POCKET_SUITES",
            "_UBUNTU_COMPONENTS",
            "_LAUNCHPAD_ARCHIVE_API",
            "_LAUNCHPAD_SERIES_API",
            "_GENERATED_OUTPUT_ENTRIES",
            "_KNOWN_PACKAGE_CONTROL_ACTIONS",
            "PROJECT_SOURCE_PATHS",
            "GUEST_RELEASE_NAMES",
            "_ZIP_TIMESTAMP",
        )
        for module in (
            build,
            build_config,
            common,
            ubuntu,
            ci,
            collect_alpine_sources,
            collect_ubuntu_sources,
            release,
            benchmark,
            archive,
        ):
            with self.subTest(module=module.__name__):
                self.assertFalse(set(names).intersection(vars(module)))

    def test_guest_descriptors_and_source_manifest_share_build_pins(self):
        manifest = json.loads(
            (BuildConstants.REPO_ROOT / "SOURCE-MANIFEST.json").read_text(
                encoding="utf-8"
            )
        )
        self.assertEqual(KernelBuildConstants.VERSION, manifest["linux"]["version"])
        self.assertEqual(KernelBuildConstants.URL, manifest["linux"]["upstream_url"])
        self.assertEqual(
            KernelBuildConstants.SHA256,
            manifest["linux"]["upstream_archive_sha256"],
        )
        self.assertEqual(
            AlpineBuildConstants.MINIROOTFS_URL, manifest["alpine"]["minirootfs_url"]
        )
        self.assertEqual(
            AlpineBuildConstants.MINIROOTFS_SHA256,
            manifest["alpine"]["minirootfs_sha256"],
        )
        for descriptor in (guests.ALPINE_GUEST, guests.UBUNTU_GUEST):
            with self.subTest(guest=descriptor.name):
                pinned = manifest[descriptor.name]
                self.assertEqual(descriptor.release, pinned["version"])
                self.assertEqual(descriptor.architecture, pinned["architecture"])
                self.assertEqual(
                    f"build/{descriptor.package_manifest_name}",
                    pinned["package_manifests"][0],
                )
        self.assertEqual(
            OpenVMMBuildConstants.MICROVM_ABI_VERSION,
            manifest["openvmm"]["microvm_abi_version"],
        )
        self.assertEqual(
            OpenVMMBuildConstants.CONTROL_SESSION_PROTOCOL_VERSION,
            manifest["openvmm"]["control_session_protocol_version"],
        )
        self.assertEqual(
            OpenVMMBuildConstants.CONTROL_CONTRACT_REVISION,
            manifest["openvmm"]["control_contract_revision"],
        )
        self.assertEqual(
            manifest["guest_rust"],
            {
                "toolchain": GuestRustBuildConstants.TOOLCHAIN,
                "target": GuestRustBuildConstants.TARGET,
            },
        )
        for guest in ("alpine", "ubuntu", "azurelinux"):
            with self.subTest(guest=guest):
                self.assertTrue(
                    set(GuestRustBuildConstants.SOURCE_PATHS).issubset(
                        manifest[guest]["guest_sources"]
                    )
                )

    def test_cache_defaults_are_resolved_for_each_configuration(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for configured in ("", str(root / "first"), str(root / "second")):
                with (
                    self.subTest(configured=configured),
                    patch.dict(os.environ, {"NVX_CACHE_DIR": configured}),
                ):
                    expected = (
                        Path(configured)
                        if configured
                        else BuildConstants.REPO_ROOT / ".cache"
                    ).resolve()
                    self.assertEqual(
                        build_config.KernelBuildConfig().cache_directory, expected
                    )
                    self.assertEqual(common.cache_root(), expected)
                    self.assertEqual(
                        build_config.KernelBuildConfig(
                            cache_directory=root / "explicit"
                        ).cache_directory,
                        root / "explicit",
                    )

    def test_openvmm_executable_default_is_resolved_at_configuration_time(self):
        for host, binary in (("nt", "openvmm.exe"), ("posix", "openvmm")):
            expected = OpenVMMBuildConstants.DIRECTORY / "target" / "release" / binary
            with self.subTest(host=host), patch.object(common.os, "name", host):
                self.assertEqual(common.openvmm_binary_path(), expected)
                self.assertEqual(build_config.OpenVmmBuildConfig().output, expected)

    def test_initramfs_provenance_tracks_constants_source(self):
        relative = "scripts/nvx_tools/build_constants.py"
        source = BuildConstants.REPO_ROOT / relative
        original = build.initramfs_provenance_inputs()
        records = cast(list[dict[str, str]], original["source_files"])
        self.assertEqual(
            [record for record in records if record["path"] == relative],
            [{"path": relative, "sha256": common.normalized_sha256_file(source)}],
        )

        def changed_sha256(path: Path) -> str:
            return "f" * 64 if path == source else common.normalized_sha256_file(path)

        with patch.object(build, "normalized_sha256_file", side_effect=changed_sha256):
            changed = build.initramfs_provenance_inputs()
        self.assertNotEqual(original, changed)
        self.assertEqual(original["alpine"], changed["alpine"])

    def test_constants_are_in_docker_and_release_source_inputs(self):
        dockerfile = (BuildConstants.REPO_ROOT / "docker" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        self.assertIn("COPY scripts/ /repo/scripts/", dockerfile)
        self.assertIn("scripts", ReleaseBuildConstants.PROJECT_SOURCE_PATHS)
        self.assertIn("nvx", ReleaseBuildConstants.PROJECT_SOURCE_PATHS)
        self.assertIn(
            Path("nvx") / "target",
            ReleaseBuildConstants.PROJECT_SOURCE_EXCLUDES,
        )
        self.assertTrue(
            (
                BuildConstants.REPO_ROOT
                / "scripts"
                / "nvx_tools"
                / "build_constants.py"
            ).is_file()
        )

    def test_project_source_paths_exist_and_are_tracked(self):
        for relative in ReleaseBuildConstants.PROJECT_SOURCE_PATHS:
            with self.subTest(path=relative):
                self.assertTrue((BuildConstants.REPO_ROOT / relative).exists())
                tracked = subprocess.run(
                    ["git", "ls-files", "--error-unmatch", "--", relative],
                    cwd=BuildConstants.REPO_ROOT,
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(tracked.returncode, 0, tracked.stderr)

    def test_no_c_or_assembler_sources_remain_outside_openvmm(self):
        tracked = subprocess.run(
            ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
            cwd=BuildConstants.REPO_ROOT,
            capture_output=True,
            check=True,
        ).stdout.split(b"\0")
        forbidden = sorted(
            path.decode("utf-8")
            for path in tracked
            if path
            and (BuildConstants.REPO_ROOT / path.decode("utf-8")).is_file()
            and not path.startswith(b"openvmm/")
            and PurePosixPath(path.decode("utf-8")).suffix.lower() in {".c", ".h", ".s"}
        )
        self.assertEqual(forbidden, [])

    def test_guest_binary_third_party_notices_cover_the_rust_runtime(self):
        notices = (BuildConstants.REPO_ROOT / "THIRD_PARTY_NOTICES.md").read_text(
            encoding="utf-8"
        )
        musl = next(
            line for line in notices.splitlines() if line.startswith("| musl libc |")
        )
        self.assertEqual(
            set(re.findall(r"`/sbin/(nvx-[a-z-]+)`", musl)),
            set(GuestRustBuildConstants.BINARIES),
        )
        for component in (
            "Rust core and standard libraries",
            "Rust `compiler_builtins`",
            "Rust `libc` crate",
            "Rust `nix` crate 0.31.3",
            "Rust `getrandom` crate 0.4.3",
            "Rust `bitflags` crate 2.13.2",
            "Rust `cfg-if` crate 1.0.5",
            "Rust `memoffset` crate 0.9.1",
            "Apache-2.0 WITH LLVM-exception",
        ):
            with self.subTest(component=component):
                self.assertIn(component, notices)

    def test_nvx_third_party_notices_cover_every_linked_crate(self):
        # The crates of `cargo tree --locked --features cli -e normal` for the
        # release targets, x86_64-unknown-linux-musl and x86_64-pc-windows-msvc:
        # what bin/nvx[.exe] links, and serde_derive/document-features with
        # the crates that they run on while nvx builds.
        shipped = {
            "bitflags",
            "block-buffer",
            "cfg-if",
            "cpufeatures",
            "crossterm",
            "crossterm_winapi",
            "crypto-common",
            "digest",
            "document-features",
            "generic-array",
            "getrandom",
            "itoa",
            "libc",
            "linux-raw-sys",
            "litrs",
            "lock_api",
            "memchr",
            "memoffset",
            "nix",
            "parking_lot",
            "parking_lot_core",
            "proc-macro2",
            "quote",
            "rustix",
            "scopeguard",
            "serde",
            "serde_core",
            "serde_derive",
            "serde_json",
            "sha2",
            "smallvec",
            "syn",
            "typenum",
            "unicode-ident",
            "winapi",
            "windows-link",
            "windows-sys",
            "zmij",
        }
        # Build scripts, other targets, and the tests, such as proptest and the
        # crates that it brings.
        unshipped = {
            "autocfg",
            "cfg_aliases",
            "chacha20",
            "core_detect",
            "errno",
            "fastrand",
            "num-traits",
            "once_cell",
            "proptest",
            "r-efi",
            "rand",
            "rand_core",
            "rand_xorshift",
            "redox_syscall",
            "regex-syntax",
            "tempfile",
            "unarray",
            "version_check",
            "winapi-i686-pc-windows-gnu",
            "winapi-x86_64-pc-windows-gnu",
        }
        # Versions of shipped crates that only the tests use: proptest's
        # chacha20 takes cpufeatures 0.3, and sha2 the cpufeatures that ships.
        unshipped_versions = {("cpufeatures", "0.3.1")}
        lock = (BuildConstants.REPO_ROOT / "nvx" / "Cargo.lock").read_text(
            encoding="utf-8"
        )
        packages = re.findall(
            r'^\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"$',
            lock,
            re.MULTILINE,
        )
        # A new dependency is one or the other, and a shipped one needs notices.
        self.assertEqual({name for name, _ in packages} - {"nvx"}, shipped | unshipped)
        self.assertFalse(shipped & unshipped)
        self.assertLessEqual(unshipped_versions, set(packages))
        notices = (BuildConstants.REPO_ROOT / "THIRD_PARTY_NOTICES.md").read_text(
            encoding="utf-8"
        )
        rows = {
            line.split(" | ", 1)[0]: line
            for line in notices.splitlines()
            if line.startswith("| ")
        }
        for name in sorted(shipped):
            with self.subTest(crate=name):
                # Each shipped crate ships one version, whose notices name it.
                versions = [
                    version
                    for crate, version in packages
                    if crate == name and (crate, version) not in unshipped_versions
                ]
                self.assertEqual(len(versions), 1, versions)
                row = rows[f"| Rust `{name}` crate {versions[0]}"]
                self.assertIn("bin/nvx", row)
                self.assertIn("nvx/Cargo.lock", row)
        # The runtimes that the release executables link statically.
        for component, location in (
            ("| Rust core and standard libraries", "`bin/nvx`"),
            ("| Rust `compiler_builtins`", "`bin/nvx[.exe]`"),
            ("| musl libc", "`bin/nvx` of the `linux-kvm` and `linux-mshv`"),
            ("| LLVM `libunwind`", "the Linux `bin/nvx`"),
            (
                "| Microsoft C runtime: the Visual C++ runtime and the Universal C "
                "Runtime",
                "`bin/nvx.exe`",
            ),
        ):
            with self.subTest(component=component):
                self.assertIn(location, rows[component])

    def test_nvx_ships_the_repository_license(self):
        # A published crate holds only the files under its directory, so
        # nvx/ keeps a copy of the repository's license. The checkout
        # writes nvx/ with LF line endings on every platform and the
        # repository's license with the platform's (.gitattributes), so
        # compare the text.
        root = BuildConstants.REPO_ROOT
        self.assertEqual(
            (root / "nvx" / "LICENSE").read_bytes().replace(b"\r\n", b"\n"),
            (root / "LICENSE").read_bytes().replace(b"\r\n", b"\n"),
        )

    def test_project_source_archive_excludes_the_nvx_target(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            crate = root / "nvx"
            (crate / "src").mkdir(parents=True)
            (crate / "src" / "lib.rs").write_text("source\n", encoding="utf-8")
            (crate / "target" / "debug").mkdir(parents=True)
            (crate / "target" / "debug" / "nvx").write_text(
                "build output\n", encoding="utf-8"
            )
            output = root / "source.tar.gz"
            archive.create_reproducible_tar_gz(
                output,
                [(crate, "nvx-project-source/nvx")],
            )
            with tarfile.open(output, "r:gz") as source_archive:
                names = source_archive.getnames()

        self.assertIn("nvx-project-source/nvx/src/lib.rs", names)
        self.assertFalse(any("/target" in name for name in names))

    def test_constants_consumers_import_in_either_order(self):
        modules = (
            "nvx_tools.build_constants",
            "nvx_tools.guests",
            "nvx_tools.common",
            "nvx_tools.build_config",
            "nvx_tools.build",
            "nvx_tools.release",
            "nvx",
        )
        for ordered in (modules, tuple(reversed(modules))):
            with self.subTest(first=ordered[0]):
                result = subprocess.run(
                    [
                        sys.executable,
                        "-c",
                        "import importlib, sys; "
                        "[importlib.import_module(name) for name in sys.argv[1:]]",
                        *ordered,
                    ],
                    cwd=BuildConstants.REPO_ROOT / "scripts",
                    capture_output=True,
                    text=True,
                    timeout=30,
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stderr)


class UbuntuSourceCollectionTests(unittest.TestCase):
    def test_malformed_package_manifest_is_reported_as_script_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            manifest = Path(temporary) / "packages.json"
            manifest.write_text("{", encoding="utf-8")

            with self.assertRaises(collect_ubuntu_sources.ScriptError) as context:
                collect_ubuntu_sources._source_requirements([manifest])

        self.assertIn("cannot read Ubuntu package manifest", str(context.exception))


class AlpineSourceCollectionTests(unittest.TestCase):
    INSTALL_SCRIPT = b"#!/bin/sh\nexit 0\n"

    @staticmethod
    def _aports_commit(cache: Path, files: dict[str, tuple[str, bytes]]) -> str:
        environment = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("GIT_CONFIG_")
        }
        environment.update(
            GIT_AUTHOR_NAME="NVX Tests",
            GIT_AUTHOR_EMAIL="nvx-tests@example.com",
            GIT_COMMITTER_NAME="NVX Tests",
            GIT_COMMITTER_EMAIL="nvx-tests@example.com",
        )

        def git(*args: str, data: bytes | None = None) -> str:
            result = subprocess.run(
                ["git", "-C", str(cache), *args],
                input=data,
                env=environment,
                capture_output=True,
                check=True,
            )
            return result.stdout.decode("ascii").strip()

        cache.mkdir(parents=True, exist_ok=True)
        git("init", "-q")
        git("config", "core.autocrlf", "false")
        for path, (mode, contents) in files.items():
            blob = git("hash-object", "-w", "--stdin", data=contents)
            git("update-index", "--add", "--cacheinfo", f"{mode},{blob},{path}")
        return git("commit-tree", git("write-tree"), "-m", "aports fixture")

    def _symlinked_recipe_commit(self, cache: Path) -> str:
        return self._aports_commit(
            cache,
            {
                "main/foo/APKBUILD": ("100644", b"pkgname=foo\n"),
                "main/foo/foo.post-install": ("100755", self.INSTALL_SCRIPT),
                "main/foo/foo.post-upgrade": ("120000", b"foo.post-install"),
            },
        )

    @staticmethod
    def _package_manifest(root: Path, commit: str) -> Path:
        manifest = root / "packages.json"
        manifest.write_text(
            json.dumps(
                {
                    "guest": AlpineBuildConstants.GUEST_NAME,
                    "alpine_branch": AlpineBuildConstants.BRANCH,
                    "architecture": AlpineBuildConstants.ARCHITECTURE,
                    "packages": [
                        {
                            "name": "foo",
                            "version": "1.0-r0",
                            "origin": "foo",
                            "license": "MIT",
                            "aports_commit": commit,
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )
        return manifest

    @staticmethod
    def _member(
        name: str,
        kind: bytes = tarfile.REGTYPE,
        data: bytes = b"",
        *,
        linkname: str = "",
        mode: int = 0o664,
    ) -> tuple[tarfile.TarInfo, bytes]:
        member = tarfile.TarInfo(name)
        member.type = kind
        member.linkname = linkname
        member.mode = mode
        member.size = len(data) if kind == tarfile.REGTYPE else 0
        return member, data

    @staticmethod
    def _archive(*members: tuple[tarfile.TarInfo, bytes]) -> bytes:
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode="w") as archive:
            for member, data in members:
                archive.addfile(member, io.BytesIO(data) if member.isfile() else None)
        return buffer.getvalue()

    def test_malformed_package_manifest_is_reported_as_source_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            for contents in ("{", "[]"):
                manifest = Path(temporary) / "packages.json"
                manifest.write_text(contents, encoding="utf-8")

                with (
                    self.subTest(contents=contents),
                    self.assertRaises(collect_alpine_sources.SourceError) as context,
                ):
                    collect_alpine_sources._load_packages([manifest])

                self.assertIn("package manifest", str(context.exception))

    def test_recipe_collection_preserves_blob_line_endings(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            cache = root / "aports"
            apkbuild = b"pkgname=foo\npkgver=1.0\npkgrel=0\n"
            commit = self._aports_commit(
                cache,
                {"main/foo/APKBUILD": ("100644", apkbuild)},
            )
            subprocess.run(
                ["git", "-C", str(cache), "config", "core.autocrlf", "true"],
                check=True,
            )
            output = root / "alpine"

            with (
                patch.object(collect_alpine_sources, "_prepare_aports"),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                collect_alpine_sources.collect_alpine_sources(
                    [self._package_manifest(root, commit)],
                    output,
                    cache,
                    skip_upstream=True,
                )

            collected = output / "recipes" / commit / "main/foo/APKBUILD"
            self.assertEqual(collected.read_bytes(), apkbuild)

    def test_recipe_symlink_is_collected_as_verified_regular_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            cache = root / "aports"
            commit = self._symlinked_recipe_commit(cache)
            output = root / "alpine"

            with (
                patch.object(collect_alpine_sources, "_prepare_aports"),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                collect_alpine_sources.collect_alpine_sources(
                    [self._package_manifest(root, commit)],
                    output,
                    cache,
                    skip_upstream=True,
                )

            upgrade = output / "recipes" / commit / "main/foo/foo.post-upgrade"
            self.assertFalse(upgrade.is_symlink())
            self.assertEqual(upgrade.read_bytes(), self.INSTALL_SCRIPT)
            source_manifest = json.loads(
                (output / "manifest.json").read_text(encoding="utf-8")
            )
            self.assertEqual(
                source_manifest["format"], AlpineBuildConstants.SOURCE_MANIFEST_FORMAT
            )
            self.assertEqual(
                source_manifest["executables"],
                [
                    f"recipes/{commit}/main/foo/foo.post-install",
                    f"recipes/{commit}/main/foo/foo.post-upgrade",
                ],
            )
            self.assertIn(
                f"recipes/{commit}/main/foo/foo.post-upgrade",
                dict(common.verify_sha256_sums(output).files),
            )
            if os.name == "posix":
                self.assertEqual(upgrade.stat().st_mode & 0o777, 0o755)

    def test_collection_replaces_stale_output_and_passes_release_validation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            cache = root / "aports"
            commit = self._symlinked_recipe_commit(cache)
            manifest = self._package_manifest(root, commit)
            source_dir = root / "sources"
            output = source_dir / "alpine"
            stale_files = (
                output / "recipes" / ("0" * 40) / "main/stale/APKBUILD",
                output / "upstream" / ("0" * 40) / "main/stale/stale.tar.gz",
            )
            for stale in stale_files:
                stale.parent.mkdir(parents=True)
                stale.write_bytes(b"stale")
            (output / "manifest.json").write_text("{}", encoding="utf-8")
            (output / "SHA256SUMS").write_text("", encoding="ascii")

            def fetch_upstream(bundle: Path, _alpine_version: str) -> None:
                upstream = bundle / "upstream" / commit / "main" / "foo"
                upstream.mkdir(parents=True)
                (upstream / "foo-1.0.tar.gz").write_bytes(b"upstream")

            with (
                patch.object(collect_alpine_sources, "_prepare_aports"),
                patch.object(
                    collect_alpine_sources,
                    "_fetch_upstream_sources",
                    side_effect=fetch_upstream,
                ),
                patch.object(BuildConstants, "SOURCE_DIR", source_dir),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                collect_alpine_sources.collect_alpine_sources([manifest], output, cache)
                executables = release._validate_alpine_sources([manifest])

            self.assertEqual(
                executables,
                {
                    f"recipes/{commit}/main/foo/foo.post-install",
                    f"recipes/{commit}/main/foo/foo.post-upgrade",
                },
            )
            for stale in stale_files:
                self.assertFalse(stale.parent.parent.exists(), stale)
            self.assertEqual(
                sorted(path.name for path in output.iterdir()),
                ["SHA256SUMS", "manifest.json", "recipes", "upstream"],
            )

    def test_recipe_extraction_materializes_symlinks_inside_recipe(self):
        member = self._member
        archive = self._archive(
            member("main/", tarfile.DIRTYPE, mode=0o775),
            member("main/foo/", tarfile.DIRTYPE, mode=0o775),
            member("main/foo/patches/", tarfile.DIRTYPE, mode=0o775),
            member("main/foo/APKBUILD", data=b"pkgname=foo\n"),
            member("main/foo/foo.post-install", data=self.INSTALL_SCRIPT, mode=0o775),
            member(
                "main/foo/foo.post-upgrade",
                tarfile.SYMTYPE,
                linkname="foo.post-install",
            ),
            member(
                "main/foo/foo.pre-upgrade",
                tarfile.SYMTYPE,
                linkname="./foo.post-upgrade",
            ),
            member("main/foo/patches/fix.patch", data=b"patch\n"),
            member(
                "main/foo/fix.patch",
                tarfile.SYMTYPE,
                linkname="patches/../patches/fix.patch",
            ),
            member(
                "main/foo/patches/APKBUILD", tarfile.SYMTYPE, linkname="../APKBUILD"
            ),
        )
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary)

            executables = collect_alpine_sources._safe_extract(
                archive, destination, "main/foo"
            )

            self.assertEqual(
                executables,
                [
                    "main/foo/foo.post-install",
                    "main/foo/foo.post-upgrade",
                    "main/foo/foo.pre-upgrade",
                ],
            )
            recipe = destination / "main" / "foo"
            expected = {
                "APKBUILD": (b"pkgname=foo\n", 0o644),
                "foo.post-install": (self.INSTALL_SCRIPT, 0o755),
                "foo.post-upgrade": (self.INSTALL_SCRIPT, 0o755),
                "foo.pre-upgrade": (self.INSTALL_SCRIPT, 0o755),
                "fix.patch": (b"patch\n", 0o644),
                "patches/fix.patch": (b"patch\n", 0o644),
                "patches/APKBUILD": (b"pkgname=foo\n", 0o644),
            }
            for relative, (contents, mode) in expected.items():
                with self.subTest(path=relative):
                    path = recipe / relative
                    self.assertFalse(path.is_symlink())
                    self.assertEqual(path.read_bytes(), contents)
                    if os.name == "posix":
                        self.assertEqual(path.stat().st_mode & 0o777, mode)

    def test_recipe_extraction_accepts_implicit_directories(self):
        member = self._member
        archive = self._archive(
            member("main/foo/APKBUILD", data=b"pkgname=foo\n"),
            member("main/foo/scripts/install", data=self.INSTALL_SCRIPT, mode=0o775),
            member(
                "main/foo/foo.post-install",
                tarfile.SYMTYPE,
                linkname="scripts/install",
            ),
            member(
                "main/foo/scripts/APKBUILD", tarfile.SYMTYPE, linkname="../APKBUILD"
            ),
        )
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary)

            executables = collect_alpine_sources._safe_extract(
                archive, destination, "main/foo"
            )

            self.assertEqual(
                executables,
                ["main/foo/foo.post-install", "main/foo/scripts/install"],
            )
            recipe = destination / "main" / "foo"
            for relative, contents in (
                ("foo.post-install", self.INSTALL_SCRIPT),
                ("scripts/APKBUILD", b"pkgname=foo\n"),
            ):
                with self.subTest(path=relative):
                    path = recipe / relative
                    self.assertFalse(path.is_symlink())
                    self.assertEqual(path.read_bytes(), contents)
            if os.name == "posix":
                for directory in (destination / "main", recipe, recipe / "scripts"):
                    with self.subTest(directory=directory.name):
                        self.assertEqual(directory.stat().st_mode & 0o777, 0o755)

    def test_recipe_extraction_rejects_unsafe_members(self):
        member = self._member
        recipe = (
            member("main/", tarfile.DIRTYPE),
            member("main/foo/", tarfile.DIRTYPE),
            member("main/foo/sub/", tarfile.DIRTYPE),
            member("main/foo/APKBUILD", data=b"pkgname=foo\n"),
            member("main/foo/sub/file", data=b"file\n"),
        )
        cases: dict[str, tuple[tuple[tarfile.TarInfo, bytes], ...]] = {
            "absolute path": (member("/main/foo/file"),),
            "parent path": (member("main/foo/../../file"),),
            "outside recipe": (member("main/bar/file"),),
            "duplicate path": (member("main/foo/APKBUILD"),),
            "member below a file": (member("main/foo/APKBUILD/file"),),
            "member below a symlink": (
                member("main/foo/alias", tarfile.SYMTYPE, linkname="sub"),
                member("main/foo/alias/file"),
            ),
            "hard link": (
                member("main/foo/link", tarfile.LNKTYPE, linkname="main/foo/APKBUILD"),
            ),
            "fifo": (member("main/foo/pipe", tarfile.FIFOTYPE),),
            "character device": (member("main/foo/device", tarfile.CHRTYPE),),
            "absolute symlink": (
                member("main/foo/link", tarfile.SYMTYPE, linkname="/etc/passwd"),
            ),
            "escaping symlink": (
                member("main/foo/link", tarfile.SYMTYPE, linkname="../../../passwd"),
            ),
            "sibling recipe symlink": (
                member("main/foo/link", tarfile.SYMTYPE, linkname="../bar/APKBUILD"),
            ),
            "dangling symlink": (
                member("main/foo/link", tarfile.SYMTYPE, linkname="missing"),
            ),
            "symlink through a missing directory": (
                member(
                    "main/foo/link",
                    tarfile.SYMTYPE,
                    linkname="missing/../APKBUILD",
                ),
            ),
            "directory symlink": (
                member("main/foo/link", tarfile.SYMTYPE, linkname="sub"),
            ),
            "symlink loop": (
                member("main/foo/first", tarfile.SYMTYPE, linkname="second"),
                member("main/foo/second", tarfile.SYMTYPE, linkname="first"),
            ),
            "symlinked directory traversal": (
                member("main/foo/link", tarfile.SYMTYPE, linkname="alias/file"),
                member("main/foo/alias", tarfile.SYMTYPE, linkname="sub"),
            ),
        }
        for name, members in cases.items():
            with self.subTest(case=name), tempfile.TemporaryDirectory() as temporary:
                destination = Path(temporary) / "recipes"

                with self.assertRaises(collect_alpine_sources.SourceError):
                    collect_alpine_sources._safe_extract(
                        self._archive(*recipe, *members),
                        destination,
                        "main/foo",
                    )

                self.assertEqual(list(destination.iterdir()), [])

    def test_collection_guards_output_before_replacing_it(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = self._package_manifest(root, "0" * 40)
            output = root / "alpine"
            (output / "recipes").mkdir(parents=True)
            notes = output / "notes.txt"
            notes.write_text("keep", encoding="utf-8")
            not_directory = root / "file"
            not_directory.write_text("keep", encoding="utf-8")

            for target, message in (
                (output, "unexpected entries: notes.txt"),
                (not_directory, "is not a directory"),
            ):
                with (
                    self.subTest(output=target.name),
                    patch.object(collect_alpine_sources, "_prepare_aports") as prepare,
                    self.assertRaisesRegex(
                        collect_alpine_sources.SourceError,
                        message,
                    ),
                ):
                    collect_alpine_sources.collect_alpine_sources(
                        [manifest],
                        target,
                        root / "aports",
                        skip_upstream=True,
                    )
                prepare.assert_not_called()
            self.assertTrue(notes.is_file())
            self.assertTrue((output / "recipes").is_dir())

            notes.unlink()
            with (
                patch.object(collect_alpine_sources, "_prepare_aports"),
                patch.object(
                    collect_alpine_sources.shutil,
                    "rmtree",
                    side_effect=PermissionError(13, "Permission denied"),
                ),
                self.assertRaisesRegex(
                    collect_alpine_sources.SourceError,
                    "cannot remove previous Alpine source output.*remove it manually",
                ),
            ):
                collect_alpine_sources.collect_alpine_sources(
                    [manifest],
                    output,
                    root / "aports",
                    skip_upstream=True,
                )

    def test_collection_refuses_symlinked_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "alpine"
            (output / "recipes").mkdir(parents=True)
            link = root / "alpine-link"
            try:
                link.symlink_to(output, target_is_directory=True)
            except OSError as error:
                self.skipTest(f"symlinks are unavailable: {error}")

            with (
                patch.object(collect_alpine_sources, "_prepare_aports") as prepare,
                self.assertRaisesRegex(
                    collect_alpine_sources.SourceError,
                    "Alpine source output is not a directory",
                ),
            ):
                collect_alpine_sources.collect_alpine_sources(
                    [self._package_manifest(root, "0" * 40)],
                    link,
                    root / "aports",
                    skip_upstream=True,
                )

            prepare.assert_not_called()
            self.assertTrue((output / "recipes").is_dir())

    def test_collection_refuses_output_junctions(self):
        if sys.platform != "win32":
            self.skipTest("directory junctions exist only on Windows")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = self._package_manifest(root, "0" * 40)
            elsewhere = root / "elsewhere"
            (elsewhere / "recipes").mkdir(parents=True)
            kept = elsewhere / "recipes" / "kept"
            kept.write_bytes(b"kept")
            output = root / "alpine"
            output.mkdir()
            cases = (
                (
                    root / "junction",
                    elsewhere,
                    root / "junction",
                    "Alpine source output is not a directory",
                ),
                (
                    output / "recipes",
                    elsewhere / "recipes",
                    output,
                    "Alpine source generated path is not a directory",
                ),
            )
            for link, target, collected, message in cases:
                subprocess.run(
                    ["cmd", "/c", "mklink", "/J", str(link), str(target)],
                    capture_output=True,
                    check=True,
                )
                with (
                    self.subTest(link=link.name),
                    patch.object(collect_alpine_sources, "_prepare_aports"),
                    self.assertRaisesRegex(collect_alpine_sources.SourceError, message),
                ):
                    collect_alpine_sources.collect_alpine_sources(
                        [manifest],
                        collected,
                        root / "aports",
                        skip_upstream=True,
                    )
                self.assertEqual(kept.read_bytes(), b"kept")

    def test_upstream_fetch_uses_mirror_and_restores_bundle_ownership(self):
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(collect_alpine_sources, "_run") as run,
        ):
            collect_alpine_sources._fetch_upstream_sources(Path(temporary), "3.24")

        command = run.call_args.args[0]
        self.assertEqual(command[:4], ["docker", "run", "--rm", "--volume"])
        self.assertEqual(
            command[5:7],
            [
                "--env",
                "DISTFILES_MIRROR=https://distfiles.alpinelinux.org/distfiles/v3.24",
            ],
        )
        self.assertEqual(command[7], "alpine:3.24")
        script = command[-1]
        self.assertIn("owner=$(stat -c '%u:%g' /bundle)", script)
        self.assertIn('chown -R "$owner" /bundle/upstream', script)
        self.assertLess(
            script.index("trap restore_owner EXIT"),
            script.index("mkdir -p /bundle/upstream"),
        )


class _TwoStreamInteraction:
    """An OpenVMM that prints the boot marker, waits for the guest's script,
    then writes each of ``steps``, a chunk or the end of its stream, to its
    console or to its stderr, and exits with ``status``.

    Its stderr shares the console stream, as InteractiveProcess's does by
    default, unless ``separate_stderr``, so that the same steps show what
    reaches the harness either way.
    """

    containment = None

    def __init__(
        self,
        steps: Sequence[tuple[benchmark.OutputStream, bytes | None]],
        status: int = 0,
        *,
        separate_stderr: bool,
    ) -> None:
        self.process = MagicMock(pid=123)
        self.process.poll.return_value = status
        self.process.wait.return_value = status
        self._steps = steps
        self._separate_stderr = separate_stderr
        self._stderr: benchmark.ChunkSink | None = None
        self._stderr_ready = threading.Event()
        self._typed = threading.Event()

    def read_stderr(self, chunks: benchmark.ChunkSink) -> None:
        self._stderr = chunks
        self._stderr_ready.set()

    def read_output(self, chunks: benchmark.ChunkSink) -> None:
        stderr = chunks
        if self._separate_stderr:
            self._stderr_ready.wait(5)
            assert self._stderr is not None
            stderr = self._stderr
        chunks.put(benchmark.BOOT_MARKER + b"\r\n")
        self._typed.wait(5)
        for stream, chunk in self._steps:
            # The shared stream ends once, after both of its writers.
            if chunk is not None or stderr is not chunks:
                (chunks if stream == "console" else stderr).put(chunk)
        if stderr is chunks:
            chunks.put(None)

    def write_input(self, _data: bytes) -> None:
        self._typed.set()

    def close(self) -> None:
        pass


class BenchmarkTests(unittest.TestCase):
    def test_kvm_worker_result_decoding(self):
        completed = subprocess.CompletedProcess(
            ["worker"],
            0,
            stdout='noise\nOPENVMM_KVM_E2E_RESULT={"p50_ms": 1.5}\n',
            stderr="",
        )
        with patch.object(benchmark.subprocess, "run", return_value=completed):
            result = benchmark._run_kvm_worker(["worker"], "e2e")

        self.assertEqual(result, {"p50_ms": 1.5})

    def test_kvm_workers_forward_translated_scratch_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary).resolve()
            args = argparse.Namespace(
                scratch_dir=scratch,
                warmups=1,
                runs=2,
                memory_mib=128,
                processors=4,
                host_cpu_reserve=1,
                cpus="0-3",
                timeout=1.0,
                teardown_mode="guest-exit",
                net=None,
                network_profile=None,
                keep_kvm_stage=True,
            )
            workers = (
                (benchmark.benchmark_kvm, ""),
                (benchmark.benchmark_e2e_kvm, "e2e"),
                (benchmark.benchmark_snapshot_restore_kvm, "restore"),
                (benchmark.benchmark_snapshot_kvm, "snapshot"),
            )

            def translate(path: Path) -> str:
                if path == benchmark.NVX_SCRIPT:
                    return "/workspace/scripts/nvx.py"
                self.assertEqual(path, scratch)
                return "/mnt/data/nvx-benchmark-scratch"

            with (
                patch.object(benchmark, "stage_kvm"),
                patch.object(
                    benchmark,
                    "windows_to_wsl",
                    side_effect=translate,
                ) as translate_path,
                patch.object(
                    benchmark,
                    "_run_kvm_worker",
                    return_value={},
                ) as run_worker,
            ):
                for worker, result_kind in workers:
                    with self.subTest(result_kind=result_kind or "boot"):
                        translate_path.reset_mock()
                        run_worker.reset_mock()
                        worker(
                            args,
                            Path("openvmm"),
                            Path("kernel"),
                            Path("initrd"),
                        )

                        command = run_worker.call_args.args[0]
                        scratch_index = command.index("--scratch-dir")
                        self.assertEqual(
                            command[scratch_index : scratch_index + 2],
                            [
                                "--scratch-dir",
                                "/mnt/data/nvx-benchmark-scratch",
                            ],
                        )
                        self.assertEqual(
                            translate_path.call_args_list,
                            [call(benchmark.NVX_SCRIPT), call(scratch)],
                        )
                        self.assertEqual(
                            run_worker.call_args.args[1],
                            result_kind,
                        )

    def test_kvm_worker_defaults_to_translated_system_temporary_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            system_temporary = Path(temporary).resolve()
            args = argparse.Namespace(scratch_dir=None)
            with (
                patch.object(
                    benchmark.tempfile,
                    "gettempdir",
                    return_value=str(system_temporary),
                ),
                patch.object(
                    benchmark,
                    "windows_to_wsl",
                    return_value="/mnt/c/system-temp",
                ) as translate,
            ):
                self.assertEqual(
                    benchmark._kvm_worker_scratch_arguments(args),
                    ["--scratch-dir", "/mnt/c/system-temp"],
                )

            translate.assert_called_once_with(system_temporary)

    def test_require_file_preserves_resolved_path_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            missing = root / "nested" / ".." / "missing"
            resolved_missing = missing.resolve()

            with self.assertRaises(FileNotFoundError) as raised:
                benchmark.require_file(missing, "benchmark artifact")

            self.assertEqual(
                str(raised.exception),
                f"benchmark artifact not found: {resolved_missing}",
            )

    def test_snapshot_profile_records_are_parsed_and_summarized(self):
        record = benchmark.parse_snapshot_profile_line(
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
            b"phase=artifact_open exclusive=1 duration_ns=1500000 "
            b"process_elapsed_ns=2000000 pid=42 logical_bytes=1024 "
            b"allocated_bytes=512\r"
        )

        self.assertIsNotNone(record)
        assert record is not None
        self.assertEqual(record["operation"], "restore")
        self.assertEqual(record["duration_ns"], 1_500_000)
        self.assertTrue(record["exclusive"])
        summary = benchmark.summarize_lifecycle_profiles(
            [{"records": [record, {**record, "duration_ns": 2_500_000}]}]
        )
        metric = summary["phases"]["restore.artifact_open"]
        self.assertEqual(metric["samples_ms"], [1.5, 2.5])
        self.assertEqual(metric["p50_ms"], 2.0)
        self.assertEqual(metric["p95_ms"], 2.5)
        self.assertTrue(metric["exclusive"])

    def test_snapshot_profile_rejects_incomplete_records(self):
        with self.assertRaisesRegex(ValueError, "missing"):
            benchmark.parse_snapshot_profile_line(
                b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore phase=open"
            )

    def test_snapshot_profile_records_console_input_dispatch(self):
        collector = benchmark.SnapshotProfileCollector(42, 100)

        with patch.object(benchmark, "process_resource_counters", return_value={}):
            collector.feed(
                b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture "
                b"phase=input_gate exclusive=1 duration_ns=20 "
                b"process_elapsed_ns=120 pid=42\n"
                b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture "
                b"phase=publication_commit exclusive=1 duration_ns=10 "
                b"process_elapsed_ns=310 pid=42\n"
            )
            sample = collector.finish_capture(200, 260, 300, 400, 450, 4096)

        records = cast(list[dict[str, object]], sample["records"])
        by_phase = {str(record["phase"]): record for record in records}
        generation = by_phase["snapshot_generation"]
        self.assertEqual(generation["duration_ns"], 210)
        self.assertEqual(generation["source"], "openvmm_clock")
        self.assertEqual(sample["generation_duration_ns"], 210)
        dispatch = by_phase["console_input_dispatch"]
        self.assertEqual(dispatch["operation"], "capture")
        self.assertEqual(dispatch["phase"], "console_input_dispatch")
        self.assertEqual(dispatch["duration_ns"], 60)
        self.assertEqual(dispatch["observer_elapsed_ns"], 160)
        self.assertEqual(dispatch["logical_bytes"], 4096)
        self.assertTrue(dispatch["exclusive"])
        round_trip = by_phase["console_command_round_trip"]
        self.assertEqual(round_trip["phase"], "console_command_round_trip")
        self.assertEqual(round_trip["duration_ns"], 100)
        guest_to_publication = by_phase["guest_dispatch_to_publication"]
        self.assertEqual(guest_to_publication["phase"], "guest_dispatch_to_publication")
        self.assertEqual(guest_to_publication["duration_ns"], 100)
        self.assertEqual(by_phase["request_to_publication"]["duration_ns"], 200)

    def test_snapshot_generation_requires_valid_process_clock_boundaries(self):
        gate: dict[str, object] = {
            "operation": "capture",
            "phase": "input_gate",
            "duration_ns": 20,
            "process_elapsed_ns": 120,
            "pid": 42,
        }
        publication: dict[str, object] = {
            "operation": "capture",
            "phase": "publication_commit",
            "duration_ns": 10,
            "process_elapsed_ns": 310,
            "pid": 42,
        }
        self.assertEqual(
            benchmark.snapshot_generation_duration_ns([gate, publication], 42), 210
        )
        for records, error in (
            ([publication], "exactly one 'input_gate'"),
            ([gate], "exactly one 'publication_commit'"),
            ([gate, gate, publication], "exactly one 'input_gate'"),
            ([gate, publication, publication], "exactly one 'publication_commit'"),
            ([gate, {**publication, "pid": 43}], "different process"),
            ([{**gate, "duration_ns": -1}, publication], "not monotonic"),
            ([{**gate, "process_elapsed_ns": 10}, publication], "not monotonic"),
            ([gate, {**publication, "process_elapsed_ns": 125}], "not monotonic"),
        ):
            with self.subTest(records=records):
                with self.assertRaisesRegex(ValueError, error):
                    benchmark.snapshot_generation_duration_ns(records, 42)

    def test_capture_excludes_polling_input_and_matches_profile_generation(self):
        for delay_ms in (0, 500, 2000):
            for profiled in (False, True):
                with self.subTest(delay_ms=delay_ms, profiled=profiled):
                    self.check_capture_timing(delay_ms, profiled)

    def check_capture_timing(self, delay_ms: int, profiled: bool) -> None:
        clock_ns = 0
        writes: list[bytes] = []

        class FakeProcess:
            pid = 42
            returncode = 0

            def poll(self) -> int:
                return 0

            def wait(self) -> int:
                nonlocal clock_ns
                clock_ns += 1_000_000
                return 0

        class FakeInteraction:
            process = FakeProcess()

            def read_output(self, _chunks: object) -> None:
                pass

            def read_stderr(self, _chunks: object) -> None:
                pass

            def write_input(self, data: bytes) -> None:
                nonlocal clock_ns
                writes.append(data)
                if data == b"nvx-snapshot\n":
                    clock_ns += 20_000_000

            def close(self) -> None:
                pass

        chunks = iter(
            (
                ("console", benchmark.BOOT_MARKER + b"\n", 0, False),
                ("console", benchmark.SMP_PROBE_COMPLETION_MARKER + b"\n", 1, False),
                (
                    "console",
                    benchmark.SNAPSHOT_GUEST_DISPATCH_MARKER + b"\n",
                    delay_ms,
                    False,
                ),
                (
                    "stderr",
                    b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture "
                    b"phase=input_gate exclusive=1 duration_ns=500000 "
                    b"process_elapsed_ns=500000000 pid=42\n"
                    b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture "
                    b"phase=publication_commit exclusive=1 duration_ns=100000 "
                    b"process_elapsed_ns=502500000 pid=42\n",
                    3,
                    True,
                ),
                ("console", None, 0, False),
                ("stderr", None, 0, False),
            )
        )
        profiles: list[dict[str, object]] = []
        with tempfile.TemporaryDirectory() as temporary:
            snapshot = Path(temporary) / "snapshot"

            def read_chunk(*, timeout: float) -> tuple[str, bytes | None]:
                nonlocal clock_ns
                del timeout
                stream, chunk, advance_ms, publish = next(chunks)
                clock_ns += advance_ms * 1_000_000
                if publish:
                    snapshot.mkdir()
                return stream, chunk

            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=FakeInteraction()
                ) as interaction,
                patch.object(benchmark.threading, "Thread"),
                patch.object(benchmark.queue, "Queue") as queues,
                patch.object(
                    benchmark.time, "perf_counter_ns", side_effect=lambda: clock_ns
                ),
                patch.object(benchmark, "_try_peak_rss", return_value=1024),
                patch.object(
                    benchmark, "process_resource_counters", return_value={}
                ) as counters,
            ):
                queues.return_value.get.side_effect = read_chunk
                result = benchmark.capture_snapshot(
                    ["openvmm"],
                    snapshot,
                    processors=1,
                    timeout=5,
                    snapshot_profile=profiled,
                    profile_sink=profiles,
                )

        self.assertEqual(result, (3.0, delay_ms + 23.0, 1.0, 1024))
        self.assertEqual(interaction.call_args.args[0], ["openvmm"])
        self.assertEqual(
            interaction.call_args.args[1][benchmark.SNAPSHOT_PROFILE_ENV], "1"
        )
        self.assertIs(interaction.call_args.kwargs["separate_stderr"], True)
        self.assertEqual(writes[1:], [b"nvx-snapshot\n"])
        self.assertIn(
            b"echo OPENVMM-SNAPSHOT-RESTORE-OK\nnvx-exit 0\n",
            writes[0],
        )
        if profiled:
            phases = benchmark.summarize_lifecycle_profiles(profiles)["phases"]
            self.assertEqual(phases["capture.snapshot_generation"]["p50_ms"], result[0])
            self.assertEqual(
                phases["capture.console_command_round_trip"]["p50_ms"],
                delay_ms + 20.0,
            )
        else:
            counters.assert_not_called()
            self.assertEqual(profiles, [])

    # OpenVMM's capture profile records from a Linux/MSHV acceptance run, in
    # which the records reached the shared terminal in the middle of the
    # guest's dispatch marker line.
    INCIDENT_PID = 621736
    INCIDENT_CAPTURE_PROFILE = (
        ("input_gate", 1, 263787, 1422094434, ""),
        ("vp_stop_at_io_boundary", 1, 120994, 1422235827, ""),
        ("quiesce", 1, 393680, 1422716902, ""),
        ("save_state", 1, 647567, 1423376968, ""),
        ("mapped_memory_flush", 1, 2700, 1423391067, ""),
        ("memory_handle_flush", 1, 1400, 1423507161, " logical_bytes=134217728"),
        (
            "publication_state",
            1,
            21499,
            1423577158,
            " logical_bytes=2455 allocated_bytes=4096",
        ),
        (
            "publication_manifest",
            1,
            8899,
            1423643854,
            " logical_bytes=1454 allocated_bytes=4096",
        ),
        (
            "publication_memory",
            1,
            39798,
            1423694652,
            " logical_bytes=134217728 allocated_bytes=65011712",
        ),
        ("publication_staging_sync", 1, 3000, 1423709951, ""),
        ("publication_commit", 1, 12700, 1423734150, ""),
        ("publication_parent_sync", 1, 2700, 1423748449, ""),
        ("publication", 0, 235088, 1423760848, " logical_bytes=134217728"),
    )

    @classmethod
    def incident_capture_records(cls) -> bytes:
        return b"".join(
            (
                f"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture phase={phase} "
                f"exclusive={exclusive} duration_ns={duration} "
                f"process_elapsed_ns={elapsed} pid={cls.INCIDENT_PID}{extra}\n"
            ).encode()
            for phase, exclusive, duration, elapsed, extra in (
                cls.INCIDENT_CAPTURE_PROFILE
            )
        )

    def test_capture_observes_dispatch_marker_split_by_profile_records(self):
        records = self.incident_capture_records()
        committed = b"INFO microVM snapshot committed; terminating source process\n"
        snapshot_requested = threading.Event()
        console_split = threading.Event()
        stderr_written = threading.Event()

        class FakeProcess:
            pid = BenchmarkTests.INCIDENT_PID
            returncode = 0

            def poll(self) -> None:
                return None

            def wait(self) -> int:
                return 0

        class FakeInteraction:
            def __init__(self, snapshot: Path) -> None:
                self.process = FakeProcess()
                self.snapshot = snapshot
                self.writes: list[bytes] = []

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(benchmark.BOOT_MARKER + b"\r\n")
                chunks.put(benchmark.SMP_PROBE_COMPLETION_MARKER + b"\r\n")
                if snapshot_requested.wait(5):
                    # The console relay delivers the dispatch line in two
                    # writes, around OpenVMM's capture records on stderr.
                    chunks.put(b"nvx-snapshot\r\nNVX-S")
                    console_split.set()
                    if stderr_written.wait(5):
                        chunks.put(b"NAPSHOT-DISPATCHED\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                if console_split.wait(5):
                    chunks.put(records)
                    self.snapshot.mkdir()
                    chunks.put(committed)
                    stderr_written.set()
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                self.writes.append(data)
                if data == b"nvx-snapshot\n":
                    snapshot_requested.set()

            def close(self) -> None:
                pass

        profiles: list[dict[str, object]] = []
        with tempfile.TemporaryDirectory() as temporary:
            snapshot = Path(temporary) / "snapshot"
            log_path = Path(temporary) / "capture.log"
            interaction = FakeInteraction(snapshot)
            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=interaction
                ) as spawn,
                patch.object(benchmark, "_try_peak_rss", return_value=1024),
                patch.object(benchmark, "process_resource_counters", return_value={}),
                patch.object(benchmark, "terminate"),
            ):
                result = benchmark.capture_snapshot(
                    ["openvmm"],
                    snapshot,
                    processors=1,
                    timeout=10,
                    snapshot_profile=True,
                    profile_sink=profiles,
                    log_path=log_path,
                )
            log = log_path.read_bytes()

        self.assertIs(spawn.call_args.kwargs["separate_stderr"], True)
        self.assertEqual(result[0], 1_903_503 / 1_000_000)
        self.assertEqual(result[3], 1024)
        self.assertEqual(interaction.writes[1:], [b"nvx-snapshot\n"])
        sample = cast(list[dict[str, object]], profiles[0]["records"])
        self.assertEqual(
            [record["phase"] for record in sample if record["source"] == "openvmm"],
            [phase for phase, *_ in self.INCIDENT_CAPTURE_PROFILE],
        )
        self.assertTrue(
            benchmark.contains_output_line(
                log, benchmark.SNAPSHOT_GUEST_DISPATCH_MARKER
            )
        )
        for line in [*records.splitlines(), committed.removesuffix(b"\n")]:
            self.assertTrue(benchmark.contains_output_line(log, line), line)

    def test_capture_samples_storage_from_the_request_through_source_exit(self):
        records = self.incident_capture_records()
        snapshot_requested = threading.Event()

        class FakeProcess:
            pid = BenchmarkTests.INCIDENT_PID
            returncode = 0

            def poll(self) -> None:
                return None

            def wait(self) -> int:
                return 0

        class FakeInteraction:
            def __init__(self, snapshot: Path) -> None:
                self.process = FakeProcess()
                self.snapshot = snapshot
                self.writes: list[bytes] = []

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(benchmark.BOOT_MARKER + b"\r\n")
                chunks.put(benchmark.SMP_PROBE_COMPLETION_MARKER + b"\r\n")
                if snapshot_requested.wait(5):
                    chunks.put(
                        b"nvx-snapshot\r\n"
                        + benchmark.SNAPSHOT_GUEST_DISPATCH_MARKER
                        + b"\r\n"
                    )
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                if snapshot_requested.wait(5):
                    chunks.put(records)
                    self.snapshot.mkdir()
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                self.writes.append(data)
                if data == b"nvx-snapshot\n":
                    snapshot_requested.set()

            def close(self) -> None:
                pass

        class FakeTelemetry:
            def __init__(self, interaction: FakeInteraction) -> None:
                self.interaction = interaction
                self.events: list[str] = []
                self.windows: dict[str, tuple[int, int]] = {}

            def start(self) -> None:
                self.events.append(f"start after {len(self.interaction.writes)}")

            def stop(self) -> None:
                self.events.append("stop")

            def result(self, windows: dict[str, tuple[int, int]]) -> dict[str, object]:
                self.events.append("result")
                self.windows = windows
                return {"phases": {}}

        for retained in (True, False):
            with (
                self.subTest(retained=retained),
                tempfile.TemporaryDirectory() as temporary,
            ):
                snapshot_requested.clear()
                snapshot = Path(temporary) / "snapshot"
                interaction = FakeInteraction(snapshot)
                telemetry = FakeTelemetry(interaction)
                profiles: list[dict[str, object]] = []
                with (
                    patch.object(
                        benchmark, "InteractiveProcess", return_value=interaction
                    ),
                    patch.object(benchmark, "_try_peak_rss", return_value=1024),
                    patch.object(
                        benchmark, "process_resource_counters", return_value={}
                    ),
                    patch.object(benchmark, "terminate"),
                    patch.object(
                        benchmark.host_telemetry,
                        "create_storage_telemetry",
                        return_value=telemetry,
                    ) as create,
                ):
                    benchmark.capture_snapshot(
                        ["openvmm"],
                        snapshot,
                        processors=1,
                        timeout=10,
                        snapshot_profile=True,
                        profile_sink=profiles if retained else None,
                    )

                if not retained:
                    # Warmups keep no profile, so they sample nothing.
                    create.assert_not_called()
                    continue
                volumes, origin = create.call_args.args
                self.assertEqual(volumes["scratch"], Path(temporary))
                self.assertEqual(volumes["workspace"], BuildConstants.REPO_ROOT)
                self.assertIsInstance(origin, int)
                # Sampling starts after the SMP probe script, before the request.
                self.assertEqual(
                    telemetry.events, ["start after 1", "stop", "result", "stop"]
                )
                self.assertEqual(interaction.writes[1:], [b"nvx-snapshot\n"])
                flush_start, flush_end = telemetry.windows[
                    "capture.mapped_memory_flush"
                ]
                self.assertEqual(flush_end - flush_start, 2700)
                generation_start, generation_end = telemetry.windows[
                    "capture.snapshot_generation"
                ]
                self.assertEqual(generation_end - generation_start, 1_903_503)
                self.assertEqual(profiles[0]["storage_telemetry"], {"phases": {}})

    def test_profile_phase_windows_use_the_tightest_clock_offset(self):
        records: list[dict[str, object]] = [
            {
                "operation": "capture",
                "phase": "save_state",
                "duration_ns": 1_000,
                "process_elapsed_ns": 5_000,
                "observer_elapsed_ns": 20_500,
                "source": "openvmm",
            },
            {
                "operation": "capture",
                "phase": "mapped_memory_flush",
                "duration_ns": 900_000,
                "process_elapsed_ns": 905_000,
                "observer_elapsed_ns": 920_000,
                "source": "openvmm",
            },
            {
                "operation": "capture",
                "phase": "snapshot_generation",
                "duration_ns": 950_000,
                "process_elapsed_ns": 955_000,
                "source": "openvmm_clock",
            },
            {
                "operation": "capture",
                "phase": "source_teardown",
                "duration_ns": 9_000,
                "observer_elapsed_ns": 990_000,
                "source": "benchmark_observer",
            },
        ]

        # The save_state record arrived 0.5 us late, so the flush record's
        # 15 us offset bounds OpenVMM's clock.
        self.assertEqual(
            benchmark.snapshot_profile_phase_windows(records),
            {
                "capture.save_state": (19_000, 20_000),
                "capture.mapped_memory_flush": (20_000, 920_000),
                "capture.snapshot_generation": (20_000, 970_000),
                "capture.source_teardown": (981_000, 990_000),
            },
        )

    def test_capture_telemetry_line_describes_the_ram_flush_window(self):
        records = [
            {
                "operation": "capture",
                "phase": "mapped_memory_flush",
                "duration_ns": 1_165_314_800,
            }
        ]
        sample: dict[str, object] = {
            "records": records,
            "storage_telemetry": {
                "volumes": {"F:": ["scratch"]},
                "phases": {
                    "capture.mapped_memory_flush": {
                        "volumes": {
                            "F:": {
                                "write_mib_per_second": 128.44,
                                "average_write_latency_ms": 31.2,
                                "average_queue_length": 3.94,
                                "busy_percent": 99.4,
                            }
                        },
                        "cpu": {"busy_percent": 6.2},
                    }
                },
            },
        }

        self.assertEqual(
            benchmark.describe_capture_telemetry(sample),
            "mapped_memory_flush 1165.315 ms: scratch F: write 128.4 MiB/s at "
            "31.2 ms, queue 3.9, 99% busy; CPU 6% busy",
        )
        self.assertIsNone(benchmark.describe_capture_telemetry({"records": records}))

    def test_result_documents_record_host_provenance(self):
        args = nvx.parse_args(["benchmark"])
        with (
            patch.object(benchmark, "_git_revision", return_value="revision"),
            patch.object(
                benchmark,
                "benchmark_host_provenance",
                return_value={"runner_name": "azure-windows-3"},
            ) as host,
        ):
            document = benchmark.result_document(args, None, None)

        host.assert_called_once_with(args)
        self.assertEqual(document["host"], {"runner_name": "azure-windows-3"})

    def test_host_provenance_names_workspace_temporary_and_scratch_volumes(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary).resolve() / "scratch"
            scratch.mkdir()
            args = nvx.parse_args(["benchmark", "--scratch-dir", str(scratch)])
            system_temporary = Path(tempfile.gettempdir()).resolve()
            with (
                benchmark.benchmark_scratch_directory(args),
                patch.object(
                    benchmark.host_telemetry,
                    "host_provenance",
                    return_value={"schema_version": 1},
                ) as provenance,
            ):
                host = benchmark.benchmark_host_provenance(args)

        self.assertEqual(host, {"schema_version": 1})
        provenance.assert_called_once_with(
            {
                "workspace": BuildConstants.REPO_ROOT,
                "temporary": system_temporary,
                "scratch": scratch,
            }
        )

    def test_device_io_resume_ignores_host_provenance(self):
        args = nvx.parse_args(
            [
                "benchmark",
                "--suite",
                "device-io",
                "--backend",
                "whp",
                "--platform",
                "windows-whp-baremetal",
            ]
        )
        benchmark.apply_benchmark_suite_defaults(args)
        hosts = iter(({"machine_name": "first"}, {"machine_name": "second"}))

        def host_provenance(_: argparse.Namespace) -> dict[str, str]:
            return next(hosts, {"machine_name": "third"})

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(
                benchmark, "benchmark_host_provenance", side_effect=host_provenance
            ),
            patch.object(benchmark, "sha256_file", return_value="0" * 64),
            patch.object(
                benchmark,
                "_device_io_helper_provenance",
                return_value={"source": "helper"},
            ),
            patch.object(benchmark, "_git_revision", return_value="revision"),
            patch.object(benchmark, "_git_status", return_value=[]),
        ):
            output = Path(temporary)

            def write() -> Path:
                return benchmark.write_benchmark_metadata(
                    args,
                    output,
                    Path("openvmm.exe"),
                    Path("vmlinux"),
                    Path("initramfs.cpio.gz"),
                    "whp",
                )

            path = write()
            written = path.read_text(encoding="utf-8")
            self.assertEqual(json.loads(written)["host"], {"machine_name": "first"})
            self.assertEqual(write(), path)
            self.assertEqual(path.read_text(encoding="utf-8"), written)
            args.device_io_port += 1
            with self.assertRaisesRegex(
                ValueError, "resume controls or provenance do not match"
            ):
                write()

    def test_measure_once_observes_restore_marker_split_by_profile_record(self):
        record = (
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
            b"phase=guest_repair_gate exclusive=0 duration_ns=1500 "
            b"process_elapsed_ns=90000000 pid=4321\n"
        )
        console_split = threading.Event()
        stderr_written = threading.Event()

        class FakeInteraction:
            def __init__(self) -> None:
                self.process = MagicMock(pid=4321)
                self.process.poll.return_value = None

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(b"OPENVMM-SNAP")
                console_split.set()
                if stderr_written.wait(5):
                    chunks.put(b"SHOT-RESTORE-OK\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                if console_split.wait(5):
                    chunks.put(record)
                    stderr_written.set()
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self) -> None:
                pass

        profiles: list[dict[str, object]] = []
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "restore.log"
            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=FakeInteraction()
                ) as spawn,
                patch.object(benchmark, "live_peak_rss_bytes", return_value=1024),
                patch.object(benchmark, "wait_for_process_exit", return_value=0),
                patch.object(benchmark, "process_resource_counters", return_value={}),
            ):
                result = benchmark.measure_once(
                    ["openvmm"],
                    environment={},
                    timeout=10,
                    marker=benchmark.RESTORE_MARKER,
                    marker_must_be_line=True,
                    guest_exit_prequeued=True,
                    snapshot_profile=True,
                    profile_sink=profiles,
                    log_path=log_path,
                )
            log = log_path.read_bytes()

        self.assertIs(spawn.call_args.kwargs["separate_stderr"], True)
        self.assertEqual(result[1], 1024)
        records = cast(list[dict[str, object]], profiles[0]["records"])
        self.assertIn("guest_repair_gate", [item["phase"] for item in records])
        self.assertTrue(benchmark.contains_output_line(log, benchmark.RESTORE_MARKER))
        self.assertTrue(benchmark.contains_output_line(log, record.removesuffix(b"\n")))

    def test_measure_once_keeps_profile_records_observed_after_the_marker(self):
        records = (
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=startup "
            b"phase=worker_launch exclusive=0 duration_ns=2000000 "
            b"process_elapsed_ns=5000000 pid=4321\n"
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
            b"phase=device_start exclusive=1 duration_ns=300000 "
            b"process_elapsed_ns=80000000 pid=4321\n"
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
            b"phase=guest_repair_gate exclusive=0 duration_ns=1500 "
            b"process_elapsed_ns=90000000 pid=4321\n"
        )
        torn_down = threading.Event()

        class FakeInteraction:
            def __init__(self) -> None:
                self.process = MagicMock(pid=4321)
                self.process.poll.return_value = None

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(benchmark.RESTORE_MARKER + b"\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                # The stderr reader falls behind: OpenVMM's records arrive
                # only once the guest marker has been handled.
                if torn_down.wait(5):
                    chunks.put(records[:100])
                    chunks.put(records[100:])
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self) -> None:
                pass

        def counters(_pid: int) -> dict[str, int]:
            return {} if torn_down.is_set() else {"rss_bytes": 7}

        def wait_for_exit(_process: object, _timeout: float) -> int:
            torn_down.set()
            return 0

        profiles: list[dict[str, object]] = []
        with (
            patch.object(
                benchmark, "InteractiveProcess", return_value=FakeInteraction()
            ),
            patch.object(benchmark, "live_peak_rss_bytes", return_value=1024),
            patch.object(benchmark, "wait_for_process_exit", side_effect=wait_for_exit),
            patch.object(benchmark, "process_resource_counters", side_effect=counters),
        ):
            benchmark.measure_once(
                ["openvmm"],
                environment={},
                timeout=10,
                marker=benchmark.RESTORE_MARKER,
                marker_must_be_line=True,
                guest_exit_prequeued=True,
                snapshot_profile=True,
                profile_sink=profiles,
            )

        by_phase = {
            str(record["phase"]): record
            for record in cast(list[dict[str, object]], profiles[0]["records"])
        }
        self.assertLessEqual(
            {"process_startup", "worker_launch", "device_start", "guest_repair_gate"},
            set(by_phase),
        )
        self.assertEqual(by_phase["process_startup"]["duration_ns"], 3_000_000)
        # The device_start observation followed the marker, so it cannot bound
        # the interval to readiness.
        self.assertNotIn("resume_to_readiness", by_phase)
        readiness = by_phase["process_launch_to_readiness"]
        self.assertEqual(readiness["host_counters"], {"rss_bytes": 7})

    def test_measure_once_reads_output_to_its_end_before_finishing(self):
        record = (
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
            b"phase=guest_repair_gate exclusive=0 duration_ns=1500 "
            b"process_elapsed_ns=90000000 pid=4321\n"
        )
        split = record.index(b"duration_ns=") + len(b"duration_ns=")

        class FakeInteraction:
            def __init__(self, exited: threading.Event) -> None:
                self.process = MagicMock(pid=4321)
                self.process.poll.return_value = None
                self.exited = exited

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(benchmark.RESTORE_MARKER + b"\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                if self.exited.wait(5):
                    chunks.put(record[:split])
                    # The rest of the record outlasts the bounded drain that
                    # error paths use.
                    time.sleep(0.2)
                    chunks.put(record[split:])
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self) -> None:
                pass

        for profiled in (False, True):
            with self.subTest(profiled=profiled):
                exited = threading.Event()

                def wait_for_exit(
                    _process: object, _timeout: float, exited: threading.Event = exited
                ) -> int:
                    exited.set()
                    return 0

                profiles: list[dict[str, object]] = []
                with tempfile.TemporaryDirectory() as temporary:
                    log_path = Path(temporary) / "restore.log"
                    with (
                        patch.object(
                            benchmark,
                            "InteractiveProcess",
                            return_value=FakeInteraction(exited),
                        ),
                        patch.object(benchmark, "OUTPUT_DRAIN_TIMEOUT_SECONDS", 0.05),
                        patch.object(
                            benchmark, "live_peak_rss_bytes", return_value=1024
                        ),
                        patch.object(
                            benchmark,
                            "wait_for_process_exit",
                            side_effect=wait_for_exit,
                        ),
                        patch.object(
                            benchmark, "process_resource_counters", return_value={}
                        ),
                    ):
                        benchmark.measure_once(
                            ["openvmm"],
                            environment={},
                            timeout=10,
                            marker=benchmark.RESTORE_MARKER,
                            marker_must_be_line=True,
                            guest_exit_prequeued=True,
                            snapshot_profile=profiled,
                            profile_sink=profiles if profiled else None,
                            log_path=log_path,
                        )
                    log = log_path.read_bytes()

                self.assertTrue(
                    benchmark.contains_output_line(log, record.removesuffix(b"\n"))
                )
                if profiled:
                    records = cast(list[dict[str, object]], profiles[0]["records"])
                    repair = next(
                        item for item in records if item["phase"] == "guest_repair_gate"
                    )
                    self.assertEqual(repair["duration_ns"], 1500)
                    self.assertEqual(repair["process_elapsed_ns"], 90_000_000)
                else:
                    self.assertEqual(profiles, [])

    def test_measure_once_reports_output_that_does_not_reach_eof(self):
        release = threading.Event()
        self.addCleanup(release.set)

        class FakeInteraction:
            def __init__(self) -> None:
                self.process = MagicMock(pid=4321)
                self.process.poll.return_value = None

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(benchmark.RESTORE_MARKER + b"\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(
                    b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=restore "
                    b"phase=guest_repair_gate exclusive=0 duration_ns="
                )
                release.wait(10)
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self) -> None:
                pass

        profiles: list[dict[str, object]] = []
        with (
            patch.object(
                benchmark, "InteractiveProcess", return_value=FakeInteraction()
            ),
            patch.object(benchmark, "OUTPUT_DRAIN_TIMEOUT_SECONDS", 0.05),
            patch.object(benchmark, "live_peak_rss_bytes", return_value=1024),
            patch.object(benchmark, "wait_for_process_exit", return_value=0),
            patch.object(benchmark, "process_resource_counters", return_value={}),
            patch.object(benchmark, "terminate") as terminate,
            self.assertRaisesRegex(
                RuntimeError, r"did not reach EOF within 0\.5s"
            ) as raised,
        ):
            benchmark.measure_once(
                ["openvmm"],
                environment={},
                timeout=0.5,
                marker=benchmark.RESTORE_MARKER,
                marker_must_be_line=True,
                guest_exit_prequeued=True,
                snapshot_profile=True,
                profile_sink=profiles,
            )

        self.assertEqual(profiles, [])
        terminate.assert_called_once()
        self.assertIn("phase=guest_repair_gate", str(raised.exception))

    def test_measure_once_returns_when_a_descendant_keeps_the_output_open(self):
        # The descendant inherits the console output and stderr and outlives
        # the launched process, so neither stream reaches EOF.
        child = (
            "import subprocess, sys\n"
            "holder = subprocess.Popen(\n"
            "    [sys.executable, '-c', 'import time; time.sleep(60)'],\n"
            "    stdin=subprocess.DEVNULL,\n"
            ")\n"
            "with open(sys.argv[1], 'w') as stream:\n"
            "    stream.write(str(holder.pid))\n"
            "print('NVX-OUTPUT-HOLDER-READY', flush=True)\n"
        )
        with tempfile.TemporaryDirectory() as temporary:
            holder_pid = Path(temporary) / "holder.pid"

            def kill_holder() -> None:
                try:
                    os.kill(int(holder_pid.read_text()), signal.SIGTERM)
                except (OSError, ValueError):
                    pass

            started = time.monotonic()
            try:
                with (
                    patch.object(benchmark, "OUTPUT_DRAIN_TIMEOUT_SECONDS", 0.05),
                    self.assertRaisesRegex(
                        RuntimeError, r"did not reach EOF within 3s of its exit"
                    ),
                ):
                    benchmark.measure_once(
                        [sys.executable, "-I", "-c", child, str(holder_pid)],
                        environment=dict(os.environ),
                        timeout=3,
                        marker=b"NVX-OUTPUT-HOLDER-READY",
                        marker_must_be_line=True,
                        guest_exit_prequeued=True,
                        log_path=Path(temporary) / "launch.log",
                    )
                elapsed = time.monotonic() - started
            finally:
                kill_holder()

        # Closing a pipe that a reader is blocked on would wait for the
        # descendant to exit.
        self.assertLess(elapsed, 20)

    def test_output_reader_reads_and_stops_above_fd_setsize(self):
        if sys.platform != "linux":
            self.skipTest("select() limits descriptors to FD_SETSIZE on POSIX")
        import fcntl
        import resource

        floor = 1100
        limit = floor + 64
        soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
        if soft < limit:
            if hard != resource.RLIM_INFINITY and hard < limit:
                self.skipTest(f"the RLIMIT_NOFILE hard limit {hard} is too low")
            resource.setrlimit(resource.RLIMIT_NOFILE, (limit, hard))
            self.addCleanup(resource.setrlimit, resource.RLIMIT_NOFILE, (soft, hard))
        read_fd, write_fd = os.pipe()
        self.addCleanup(os.close, write_fd)
        # F_DUPFD takes the lowest free descriptor at or above the floor, so it
        # cannot replace a descriptor that something else owns.
        high = fcntl.fcntl(read_fd, fcntl.F_DUPFD, floor)
        os.close(read_fd)
        os.set_inheritable(high, False)
        self.addCleanup(os.close, high)
        self.assertGreaterEqual(high, floor)

        reader = benchmark._OutputReader(high)
        chunks: queue.Queue[bytes | None] = queue.Queue()
        thread = threading.Thread(target=reader.run, args=(chunks,), daemon=True)
        thread.start()
        os.write(write_fd, b"console output")
        self.assertEqual(chunks.get(timeout=5), b"console output")
        # The write end stays open, so only stop() can end the read.
        self.assertTrue(reader.stop(5))
        self.assertIsNone(chunks.get(timeout=5))
        thread.join(5)
        self.assertFalse(thread.is_alive())

    def test_guest_failure_report_includes_stderr_written_after_the_marker(self):
        terminated = threading.Event()

        class FakeInteraction:
            def __init__(self) -> None:
                self.process = MagicMock(pid=4321)
                self.process.poll.return_value = None

            def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
                chunks.put(b"NVX-RESTORE-PROCESSORS-FAIL unstable-tsc\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
                if terminated.wait(5):
                    chunks.put(b"WARN host diagnostics after the guest failure\n")
                chunks.put(None)

            def write_input(self, data: bytes) -> None:
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self) -> None:
                pass

        def terminate_openvmm(_process: object) -> None:
            terminated.set()

        with (
            patch.object(
                benchmark, "InteractiveProcess", return_value=FakeInteraction()
            ),
            patch.object(
                benchmark, "terminate", side_effect=terminate_openvmm
            ) as terminate,
            self.assertRaises(benchmark.GuestFailureReported) as raised,
        ):
            benchmark.measure_once(
                ["openvmm"],
                environment={},
                timeout=10,
                marker=b"NVX-RESTORE-PROCESSORS-OK count=8",
                marker_must_be_line=True,
                guest_exit_prequeued=True,
                failure_marker=b"NVX-RESTORE-PROCESSORS-FAIL",
            )

        terminate.assert_called_once()
        self.assertEqual(
            raised.exception.line, "NVX-RESTORE-PROCESSORS-FAIL unstable-tsc"
        )
        self.assertIn("host diagnostics after the guest failure", str(raised.exception))

    def test_separated_output_keeps_process_output_lines_whole(self):
        record = (
            b"OPENVMM_SNAPSHOT_PROFILE_V1 operation=capture phase=input_gate "
            b"exclusive=1 duration_ns=263787 process_elapsed_ns=1422094434 pid=1\n"
        )
        child = (
            "import os, time; "
            "os.write(1, b'NVX-S'); time.sleep(0.05); "
            f"os.write(2, {record!r}); time.sleep(0.05); "
            "os.write(1, b'NAPSHOT-DISPATCHED\\n~ # ')"
        )
        interaction = benchmark.InteractiveProcess(
            [sys.executable, "-I", "-c", child],
            dict(os.environ),
            separate_stderr=True,
        )
        stderr = bytearray()
        try:
            output = benchmark.SeparatedOutput(interaction)
            deadline = time.monotonic() + 30
            while not output.closed:
                stream, chunk = output.get(max(0.0, deadline - time.monotonic()))
                if stream == "stderr" and chunk is not None:
                    stderr.extend(chunk)
            self.assertEqual(interaction.process.wait(timeout=30), 0)
        finally:
            benchmark.terminate(interaction.process)
            interaction.close()

        marker = benchmark.SNAPSHOT_GUEST_DISPATCH_MARKER
        self.assertTrue(benchmark.contains_output_line(output.console, marker))
        self.assertNotIn(b"OPENVMM_SNAPSHOT_PROFILE_V1", output.console)
        self.assertIn(record, bytes(stderr))
        contents = output.contents()
        self.assertTrue(benchmark.contains_output_line(contents, marker))
        self.assertTrue(
            benchmark.contains_output_line(contents, record.removesuffix(b"\n"))
        )
        self.assertTrue(contents.endswith(b"~ # "))

    def test_interactive_process_gives_stderr_its_own_pipe_on_request(self):
        for platform in ("linux", "win32"):
            for separate_stderr in (False, True):
                with self.subTest(platform=platform, separate=separate_stderr):
                    terminal: list[int] = []

                    def open_terminal(
                        terminal: list[int] = terminal,
                    ) -> tuple[int, int]:
                        terminal.extend(os.pipe())
                        return terminal[0], terminal[1]

                    process = MagicMock(pid=4321)
                    if not separate_stderr:
                        process.stderr = None
                    with (
                        patch.object(benchmark.sys, "platform", platform),
                        patch.object(
                            benchmark.os,
                            "openpty",
                            side_effect=open_terminal,
                            create=True,
                        ),
                        patch.object(
                            benchmark.subprocess, "Popen", return_value=process
                        ) as popen,
                    ):
                        interaction = benchmark.InteractiveProcess(
                            ["openvmm"], {}, separate_stderr=separate_stderr
                        )
                    try:
                        stderr = popen.call_args.kwargs["stderr"]
                        if separate_stderr:
                            self.assertEqual(stderr, subprocess.PIPE)
                        elif platform == "linux":
                            self.assertEqual(stderr, terminal[1])
                        else:
                            self.assertEqual(stderr, subprocess.STDOUT)
                        if not separate_stderr:
                            chunks: queue.Queue[bytes | None] = queue.Queue()
                            with self.assertRaisesRegex(
                                RuntimeError, "shares the console stream"
                            ):
                                interaction.read_stderr(chunks)
                            self.assertIsNone(chunks.get_nowait())
                    finally:
                        interaction.close()
                    if separate_stderr:
                        process.stderr.close.assert_called_once_with()

    def test_warm_snapshot_cache_reads_every_artifact(self):
        with tempfile.TemporaryDirectory() as temporary:
            snapshot = Path(temporary)
            for index, name in enumerate(benchmark.SNAPSHOT_FILENAMES):
                (snapshot / name).write_bytes(bytes([index]) * 32)

            benchmark.warm_snapshot_artifacts(snapshot)

    def test_snapshot_artifact_validation_preserves_error_prefix(self):
        with tempfile.TemporaryDirectory() as temporary:
            snapshot = Path(temporary)
            for prefix in ("snapshot", "console/active snapshot"):
                with self.subTest(prefix=prefix):
                    with self.assertRaises(RuntimeError) as context:
                        benchmark._require_snapshot_artifacts(snapshot, prefix)

                    self.assertEqual(
                        str(context.exception),
                        f"{prefix} did not publish {snapshot / 'manifest.bin'}",
                    )

    def test_measure_once_does_not_resend_prequeued_guest_exit(self):
        class FakeProcess:
            pid = 123
            returncode = 0

            def poll(self):
                return None

            def terminate(self):
                raise AssertionError("guest-exit teardown terminated the host process")

        class FakeInteraction:
            def __init__(self):
                self.process = FakeProcess()
                self.writes: list[bytes] = []

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(b"/ # echo OPENVMM-SNAPSHOT-RESTORE-OK\r\n")
                chunks.put(b"OPENVMM-SNAPSHOT-RESTORE-OK\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]):
                chunks.put(None)

            def write_input(self, data: bytes):
                self.writes.append(data)

            def close(self):
                pass

        interaction = FakeInteraction()
        with (
            patch.object(benchmark, "InteractiveProcess", return_value=interaction),
            patch.object(benchmark, "live_peak_rss_bytes", return_value=1024),
            patch.object(benchmark, "wait_for_process_exit", return_value=0),
            patch.object(
                benchmark.time,
                "perf_counter_ns",
                side_effect=[0, 10_000_000, 17_000_000],
            ),
        ):
            result = benchmark.measure_once(
                ["openvmm"],
                environment={},
                timeout=1,
                marker=benchmark.RESTORE_MARKER,
                marker_must_be_line=True,
                guest_exit_prequeued=True,
            )

        self.assertEqual(result, (10.0, 1024, 7.0, 17.0))
        self.assertEqual(interaction.writes, [])

    def test_measure_once_reports_missing_rss_when_openvmm_exits_first(self):
        class FakeProcess:
            pid = 123

            def __init__(self, status: int):
                self.status = status
                self.exited = False
                self.returncode: int | None = None

            def poll(self):
                if self.exited:
                    self.returncode = self.status
                return self.returncode

            def wait(self, timeout: float | None = None) -> int:
                del timeout
                return self.status

            def terminate(self):
                raise AssertionError("unexpected process termination")

        class FakeInteraction:
            def __init__(self, status: int):
                self.process = FakeProcess(status)

            def read_output(self, chunks: queue.Queue[bytes | None]):
                self.process.exited = True
                chunks.put(benchmark.RESTORE_MARKER + b"\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]):
                chunks.put(None)

            def write_input(self, data: bytes):
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self):
                pass

        for status in (0, 1):
            with self.subTest(status=status):
                interaction = FakeInteraction(status)

                def linux_peak_rss_bytes(
                    pid: int, interaction: FakeInteraction = interaction
                ) -> int | None:
                    self.assertEqual(pid, 123)
                    # A zombie's /proc status no longer reports VmHWM.
                    return None if interaction.process.exited else 1024

                with (
                    patch.object(
                        benchmark, "InteractiveProcess", return_value=interaction
                    ),
                    patch.object(
                        benchmark,
                        "_linux_live_peak_rss_bytes",
                        side_effect=linux_peak_rss_bytes,
                    ),
                    patch.object(benchmark, "windows_peak_rss_bytes", return_value=1),
                    patch.object(
                        benchmark, "wait_for_process_exit", return_value=status
                    ) as wait,
                    patch.object(
                        benchmark.time,
                        "perf_counter_ns",
                        side_effect=[0, 10_000_000, 17_000_000],
                    ),
                ):
                    if status == 0:
                        result = benchmark.measure_once(
                            ["openvmm"],
                            environment={},
                            timeout=1,
                            marker=benchmark.RESTORE_MARKER,
                            marker_must_be_line=True,
                            guest_exit_prequeued=True,
                        )
                        self.assertEqual(result, (10.0, None, 7.0, 17.0))
                    else:
                        with self.assertRaisesRegex(
                            RuntimeError, "status 1 during teardown"
                        ):
                            benchmark.measure_once(
                                ["openvmm"],
                                environment={},
                                timeout=1,
                                marker=benchmark.RESTORE_MARKER,
                                marker_must_be_line=True,
                                guest_exit_prequeued=True,
                            )

                wait.assert_called_once_with(
                    interaction.process, benchmark.TEARDOWN_TIMEOUT_SECONDS
                )

    def test_completed_output_line_with_prefix_requires_a_whole_line(self):
        prefix = b"NVX-RESTORE-PROCESSORS-FAIL"
        self.assertIsNone(
            benchmark.completed_output_line_with_prefix(
                b"NVX-RESTORE-PROCESSORS-FAIL unst", prefix
            )
        )
        self.assertIsNone(
            benchmark.completed_output_line_with_prefix(
                b'> echo "NVX-RESTORE-PROCESSORS-FAIL count=$n"\r\n', prefix
            )
        )
        self.assertEqual(
            benchmark.completed_output_line_with_prefix(
                b"guest\r\nNVX-RESTORE-PROCESSORS-FAIL unstable-tsc\r\nmore", prefix
            ),
            b"NVX-RESTORE-PROCESSORS-FAIL unstable-tsc",
        )

    def test_measure_once_stops_at_a_completed_guest_failure_marker(self):
        class FakeInteraction:
            def __init__(self):
                self.process = MagicMock(pid=123)
                self.process.poll.return_value = None

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(b'> echo "NVX-RESTORE-PROCESSORS-FAIL count=$n"\r\n')
                chunks.put(b"Measured 6 cycles TSC warp between CPUs\r\n")
                chunks.put(b"NVX-RESTORE-PROCESSORS-FAIL unst")
                chunks.put(b"able-tsc\r\n")
                chunks.put(None)

            def read_stderr(self, chunks: queue.Queue[bytes | None]):
                chunks.put(None)

            def write_input(self, data: bytes):
                raise AssertionError(f"unexpected input: {data!r}")

            def close(self):
                pass

        interaction = FakeInteraction()
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "restore.log"
            with (
                patch.object(benchmark, "InteractiveProcess", return_value=interaction),
                patch.object(benchmark, "terminate") as terminate,
                patch.object(benchmark, "wait_for_process_exit") as wait,
            ):
                with self.assertRaises(benchmark.GuestFailureReported) as raised:
                    benchmark.measure_once(
                        ["openvmm"],
                        environment={},
                        timeout=60,
                        marker=b"NVX-RESTORE-PROCESSORS-OK count=8",
                        marker_must_be_line=True,
                        guest_exit_prequeued=True,
                        log_path=log_path,
                        failure_marker=b"NVX-RESTORE-PROCESSORS-FAIL",
                    )
            log = log_path.read_bytes()

        self.assertEqual(
            raised.exception.line, "NVX-RESTORE-PROCESSORS-FAIL unstable-tsc"
        )
        self.assertIn("TSC warp", raised.exception.output_tail)
        self.assertIn("--- OpenVMM output ---", str(raised.exception))
        terminate.assert_called_once_with(interaction.process)
        wait.assert_not_called()
        self.assertTrue(log.endswith(b"NVX-RESTORE-PROCESSORS-FAIL unstable-tsc\r\n"))

    def test_terminate_waits_for_a_killed_process_to_exit(self):
        process = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)"]
        )
        try:
            benchmark.terminate(process)
            exited = process.returncode is not None
        finally:
            process.kill()
            process.wait()

        self.assertTrue(exited)
        # An exited process is not killed again.
        with patch.object(process, "kill") as kill:
            benchmark.terminate(process)
        kill.assert_not_called()

    def test_terminate_fails_when_a_killed_process_keeps_running(self):
        process = MagicMock(pid=4321)
        process.poll.return_value = None
        with (
            patch.object(benchmark, "_wait_for_exit", return_value=False) as wait,
            self.assertRaisesRegex(
                benchmark.ProcessExitTimeout,
                "process 4321 did not exit within 30s of being killed",
            ),
        ):
            benchmark.terminate(process)

        process.kill.assert_called_once_with()
        wait.assert_called_once_with(process, benchmark.TERMINATION_TIMEOUT_SECONDS)

    def test_terminate_warns_only_about_a_slow_exit(self):
        for times, warning in (
            ((10.0, 12.5), "process 4321 took 2.500s to exit after it was killed"),
            ((10.0, 10.05), None),
        ):
            process = MagicMock(pid=4321)
            process.poll.return_value = None
            stderr = io.StringIO()
            with (
                patch.object(benchmark, "_wait_for_exit", return_value=True),
                patch.object(benchmark.time, "perf_counter", side_effect=times),
                contextlib.redirect_stderr(stderr),
            ):
                benchmark.terminate(process)

            if warning is None:
                self.assertEqual(stderr.getvalue(), "")
            else:
                self.assertIn(warning, stderr.getvalue())

    def test_exit_wait_ignores_the_returncode_that_a_windows_kill_stores(self):
        # Popen.kill() of a terminating Windows process stores its exit code,
        # although the process still runs.
        process = MagicMock(pid=4321, returncode=1)
        process.poll.return_value = 1
        with (
            patch.object(benchmark.os, "name", "nt"),
            patch.object(
                benchmark, "_wait_for_windows_process", side_effect=[False, True]
            ) as wait,
        ):
            self.assertFalse(benchmark._wait_for_exit(process, 2.0))
            self.assertTrue(benchmark._wait_for_exit(process, 2.0))

        self.assertEqual(wait.call_args_list, [call(4321, 2.0)] * 2)
        process.wait.assert_not_called()

    def test_exit_wait_reports_a_posix_process_that_keeps_running(self):
        process = MagicMock(pid=4321)
        process.wait.side_effect = [subprocess.TimeoutExpired(["openvmm"], 2.0), 0]
        with patch.object(benchmark.os, "name", "posix"):
            self.assertFalse(benchmark._wait_for_exit(process, 2.0))
            self.assertTrue(benchmark._wait_for_exit(process, 2.0))

        self.assertEqual(process.wait.call_args_list, [call(timeout=2.0)] * 2)

    @unittest.skipUnless(sys.platform == "win32", "requires Windows")
    def test_windows_exit_wait_waits_for_the_process_object(self):
        process = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)"]
        )
        try:
            self.assertFalse(benchmark._wait_for_windows_process(process.pid, 0.05))
            process.kill()
            self.assertTrue(benchmark._wait_for_windows_process(process.pid, 30))
        finally:
            process.kill()
            process.wait()

    def test_guest_runner_preserves_primary_and_close_failures_with_bounded_tail(self):
        class FakeProcess:
            pid = 123

            def poll(self):
                return 7

            def wait(self):
                return 7

        class FakeInteraction:
            process = FakeProcess()
            containment = None

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(benchmark.BOOT_MARKER + b"\n" + b"x" * 5000 + b"\nDONE\n")
                chunks.put(None)

            def write_input(self, _data: bytes):
                pass

            def close(self):
                raise OSError("close failed")

        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "guest.log"
            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=FakeInteraction()
                ),
                patch.object(benchmark, "_try_peak_rss", return_value=1024),
                patch.object(benchmark, "terminate") as terminate,
            ):
                with self.assertRaisesRegex(
                    RuntimeError, "OpenVMM exited with status 7"
                ) as raised:
                    benchmark.run_guest_script(
                        ["openvmm"],
                        "guest command\n",
                        b"DONE",
                        timeout=1,
                        log_path=log_path,
                    )

            error = raised.exception
            cleanup_errors = cast(
                tuple[BaseException, ...],
                error.cleanup_errors,  # type: ignore[attr-defined]
            )
            self.assertEqual([str(item) for item in cleanup_errors], ["close failed"])
            self.assertIn("--- OpenVMM output ---", str(error))
            self.assertIn("--- cleanup failures ---", str(error))
            self.assertNotIn(benchmark.BOOT_MARKER.decode(), str(error))
            self.assertGreater(len(log_path.read_bytes()), 4096)
            self.assertTrue(log_path.read_bytes().endswith(b"\nDONE\n"))
            terminate.assert_called_once()

    def test_guest_runner_propagates_success_only_close_failure(self):
        class FakeProcess:
            pid = 123

            def poll(self):
                return 0

            def wait(self):
                return 0

        class FakeInteraction:
            process = FakeProcess()
            containment = None

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(benchmark.BOOT_MARKER + b"\nDONE\n")
                chunks.put(None)

            def write_input(self, _data: bytes):
                pass

            def close(self):
                raise OSError("close after success")

        with (
            patch.object(
                benchmark, "InteractiveProcess", return_value=FakeInteraction()
            ),
            patch.object(benchmark, "_try_peak_rss", return_value=1024),
        ):
            with self.assertRaisesRegex(OSError, "close after success"):
                benchmark.run_guest_script(
                    ["openvmm"],
                    "guest command\n",
                    b"DONE",
                    timeout=1,
                )

    def test_guest_runner_describes_the_host_before_killing_a_hung_guest(self):
        class FakeProcess:
            pid = 123

            def poll(self):
                return None

        process = FakeProcess()
        events: list[str] = []

        class HungInteraction:
            containment = None

            def __init__(self):
                self.process = process
                self.closed = threading.Event()

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(benchmark.BOOT_MARKER + b"\n/ # esac\n")
                self.closed.wait(5)
                chunks.put(None)

            def write_input(self, _data: bytes):
                pass

            def close(self):
                self.closed.set()

        def describe(observed: object) -> str:
            events.append("described")
            self.assertIs(observed, process)
            return "files: shared/device-io.bin 4096 bytes"

        def fail(_observed: object) -> str:
            raise OSError("volume stalled")

        def kill(_process: object) -> None:
            events.append("killed")

        for describer, state in (
            (describe, "files: shared/device-io.bin 4096 bytes"),
            (fail, "could not be described: volume stalled"),
        ):
            events.clear()
            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=HungInteraction()
                ),
                patch.object(benchmark, "_try_peak_rss", return_value=1024),
                patch.object(benchmark, "terminate", side_effect=kill),
            ):
                with self.assertRaises(RuntimeError) as raised:
                    benchmark.run_guest_script(
                        ["openvmm"],
                        "guest command\n",
                        b"DONE",
                        timeout=0.2,
                        host_state_on_timeout=describer,
                    )

            message = str(raised.exception)
            self.assertTrue(
                message.startswith(
                    "guest workload did not finish within 0.2s\n"
                    f"--- host state at the timeout ---\n{state}\n"
                    "--- OpenVMM output ---\n"
                ),
                message,
            )
            self.assertTrue(message.endswith("/ # esac\n"), message)
            if describer is describe:
                # The host is described while OpenVMM still runs.
                self.assertEqual(events, ["described", "killed"])

    def test_guest_runner_describes_the_host_only_for_a_timeout(self):
        class FakeProcess:
            pid = 123

            def poll(self):
                return 7

            def wait(self):
                return 7

        class ExitingInteraction:
            process = FakeProcess()
            containment = None

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(benchmark.BOOT_MARKER + b"\n")
                chunks.put(None)

            def write_input(self, _data: bytes):
                pass

            def close(self):
                pass

        describe = MagicMock(return_value="files: none")
        with (
            patch.object(
                benchmark, "InteractiveProcess", return_value=ExitingInteraction()
            ),
            patch.object(benchmark, "_try_peak_rss", return_value=1024),
            patch.object(benchmark, "terminate"),
        ):
            with self.assertRaisesRegex(RuntimeError, "OpenVMM exited with status 7"):
                benchmark.run_guest_script(
                    ["openvmm"],
                    "guest command\n",
                    b"DONE",
                    timeout=1,
                    host_state_on_timeout=describe,
                )

        describe.assert_not_called()

    def test_guest_runner_ends_a_hung_guest_when_its_description_is_interrupted(
        self,
    ):
        class FakeProcess:
            pid = 123

            def poll(self):
                return None

        events: list[str] = []

        class HungInteraction:
            containment = None

            def __init__(self):
                self.process = FakeProcess()
                self.closed = threading.Event()

            def read_output(self, chunks: queue.Queue[bytes | None]):
                chunks.put(benchmark.BOOT_MARKER + b"\n/ # esac\n")
                self.closed.wait(5)
                chunks.put(None)

            def write_input(self, _data: bytes):
                pass

            def close(self):
                events.append("closed")
                self.closed.set()

        def interrupt(_observed: object) -> str:
            raise KeyboardInterrupt

        def kill(_process: object) -> None:
            events.append("killed")

        with (
            patch.object(
                benchmark, "InteractiveProcess", return_value=HungInteraction()
            ),
            patch.object(benchmark, "_try_peak_rss", return_value=1024),
            patch.object(benchmark, "terminate", side_effect=kill),
        ):
            with self.assertRaises(KeyboardInterrupt) as raised:
                benchmark.run_guest_script(
                    ["openvmm"],
                    "guest command\n",
                    b"DONE",
                    timeout=0.2,
                    host_state_on_timeout=interrupt,
                )

        # The interruption ends the run once OpenVMM is ended and its console closed.
        self.assertEqual(events, ["killed", "closed"])
        self.assertIsInstance(raised.exception.__context__, TimeoutError)

    def test_guest_runner_keeps_stderr_out_of_console_lines_on_request(self):
        # In CI run 41973357177, OpenVMM's stderr delivered the byte channel's
        # bytes around the console's marker, whose line they split on the
        # shared stream (#1290).
        steps: list[tuple[benchmark.OutputStream, bytes | None]] = [
            ("console", b"NVX-GUEST-SCRIPT-START\r\n"),
            ("stderr", b"\x00\r\n\x7f\xff"),
            ("console", b"NVX-LIFECYCLE-OK\r\n"),
            ("stderr", b"\n"),
            ("console", None),
            ("stderr", None),
        ]
        boot = benchmark.BOOT_MARKER + b"\r\n"
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "guest.log"
            shared = _TwoStreamInteraction(steps, separate_stderr=False)
            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=shared
                ) as spawn,
                patch.object(benchmark, "_try_peak_rss", return_value=1024),
                patch.object(benchmark, "terminate"),
                self.assertRaisesRegex(
                    RuntimeError,
                    "^guest exited without completion marker 'NVX-LIFECYCLE-OK'",
                ),
            ):
                benchmark.run_guest_script(
                    ["openvmm"],
                    "lifecycle\n",
                    b"NVX-LIFECYCLE-OK",
                    timeout=5,
                    log_path=log_path,
                )
            self.assertIs(spawn.call_args.kwargs["separate_stderr"], False)
            self.assertTrue(
                log_path.read_bytes().endswith(b"\x7f\xffNVX-LIFECYCLE-OK\r\n\n")
            )

            separate = _TwoStreamInteraction(steps, separate_stderr=True)
            stderr = bytearray()
            with (
                patch.object(
                    benchmark, "InteractiveProcess", return_value=separate
                ) as spawn,
                patch.object(benchmark, "_try_peak_rss", return_value=1024),
            ):
                result = benchmark.run_guest_script(
                    ["openvmm"],
                    "lifecycle\n",
                    b"NVX-LIFECYCLE-OK",
                    timeout=5,
                    log_path=log_path,
                    on_stderr=stderr.extend,
                )
            self.assertIs(spawn.call_args.kwargs["separate_stderr"], True)
            self.assertEqual(
                result["text"],
                (boot + b"NVX-GUEST-SCRIPT-START\r\nNVX-LIFECYCLE-OK\r\n").decode(),
            )
            self.assertEqual(bytes(stderr), b"\x00\r\n\x7f\xff\n")
            # The log keeps both streams, interleaved by whole lines.
            self.assertEqual(
                log_path.read_bytes(),
                boot
                + b"NVX-GUEST-SCRIPT-START\r\n\x00\r\nNVX-LIFECYCLE-OK\r\n\x7f\xff\n",
            )

    def test_guest_runner_reads_a_separate_stderr_to_its_end(self):
        # OpenVMM writes a fatal error to stderr after its console has ended.
        fatal = b"fatal error: failed to launch vm worker\n"
        steps: list[tuple[benchmark.OutputStream, bytes | None]] = [
            ("console", None),
            ("stderr", fatal),
            ("stderr", None),
        ]
        interaction = _TwoStreamInteraction(steps, 1, separate_stderr=True)
        stderr = bytearray()
        with (
            patch.object(benchmark, "InteractiveProcess", return_value=interaction),
            patch.object(benchmark, "_try_peak_rss", return_value=1024),
            patch.object(benchmark, "terminate"),
            self.assertRaisesRegex(
                RuntimeError, "^OpenVMM exited with status 1"
            ) as raised,
        ):
            benchmark.run_guest_script(
                ["openvmm"], "true\n", b"DONE", timeout=5, on_stderr=stderr.extend
            )

        self.assertEqual(bytes(stderr), fatal)
        # The error report keeps both streams.
        self.assertTrue(
            str(raised.exception).endswith(
                "\n--- OpenVMM output ---\n"
                f"{benchmark.BOOT_MARKER.decode()}\r\n{fatal.decode()}"
            ),
            str(raised.exception),
        )

    def test_device_io_host_state_describes_files_processes_and_volume(self):
        from datetime import datetime, timezone

        whole_seconds = datetime(2026, 10, 7, 15, 40, 38, tzinfo=timezone.utc)
        modified_ns = int(whole_seconds.timestamp()) * 1_000_000_000 + 123_000_000
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "block.raw").write_bytes(b"")
            (root / "shared").mkdir()
            (root / "shared" / "device-io.bin").write_bytes(bytes(4096))
            for path in (root / "block.raw", root / "shared" / "device-io.bin"):
                os.utime(path, ns=(modified_ns, modified_ns))
            with (
                patch.object(benchmark, "_openvmm_pids", return_value=[77, 123]),
                patch.object(
                    benchmark.host_telemetry,
                    "describe_volume_activity",
                    return_value="scratch F: write 0.0 MiB/s at n/a ms, queue 32.0",
                ) as activity,
            ):
                state = benchmark._device_io_host_state(root, MagicMock(pid=123))

        activity.assert_called_once_with(
            {"scratch": root}, benchmark.DEVICE_IO_HOST_STATE_SAMPLE_SECONDS
        )
        self.assertEqual(
            state,
            "files: block.raw 0 bytes, modified 2026-10-07T15:40:38.123+00:00; "
            "shared/device-io.bin 4096 bytes, "
            "modified 2026-10-07T15:40:38.123+00:00\n"
            "OpenVMM processes: 77, 123 (this attempt)\n"
            "scratch volume: scratch F: write 0.0 MiB/s at n/a ms, queue 32.0",
        )

    def test_device_io_host_state_bounds_each_description(self):
        release = threading.Event()

        def stalled(_root: Path) -> str:
            release.wait(10)
            return "late"

        try:
            with (
                patch.object(benchmark, "DEVICE_IO_HOST_STATE_TIMEOUT_SECONDS", 0.2),
                patch.object(
                    benchmark, "_describe_volume_activity", side_effect=stalled
                ),
                patch.object(
                    benchmark, "_describe_files", side_effect=OSError("no listing")
                ),
                patch.object(benchmark, "_openvmm_pids", return_value=[]),
            ):
                started = time.monotonic()
                state = benchmark._device_io_host_state(Path("root"), MagicMock(pid=1))
                elapsed = time.monotonic() - started
        finally:
            release.set()

        self.assertLess(elapsed, 5)
        self.assertEqual(
            state,
            "files: failed: no listing\n"
            "OpenVMM processes: none\n"
            "scratch volume: did not finish within 0.2s",
        )

    def test_device_io_host_state_reports_a_directory_that_cannot_be_listed(self):
        real_scandir = os.scandir

        def scandir(path: str):
            if os.path.basename(path) == "shared":
                raise OSError("Input/output error")
            return real_scandir(path)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "block.raw").write_bytes(b"")
            (root / "shared").mkdir()
            (root / "shared" / "device-io.bin").write_bytes(b"")
            # The listing fails rather than leave out the files that it cannot list.
            with patch.object(benchmark.os, "scandir", side_effect=scandir):
                with self.assertRaisesRegex(OSError, "Input/output error"):
                    benchmark._describe_files(root)
            with self.assertRaises(FileNotFoundError):
                benchmark._describe_files(root / "missing")
            with (
                patch.object(benchmark, "_openvmm_pids", return_value=[]),
                patch.object(
                    benchmark, "_describe_volume_activity", return_value="idle"
                ),
            ):
                state = benchmark._device_io_host_state(
                    root / "missing", MagicMock(pid=1)
                )

        self.assertRegex(state.splitlines()[0], r"^files: failed: .*missing")

    def test_openvmm_pids_reads_the_windows_task_list(self):
        listing = (
            '"openvmm.exe","4321","Services","0","82,412 K"\r\n'
            '"openvmm.exe","17","Services","0","1,024 K"\r\n'
        )
        with (
            patch.object(benchmark.os, "name", "nt"),
            patch.object(
                benchmark.subprocess,
                "run",
                return_value=subprocess.CompletedProcess([], 0, listing, ""),
            ) as run,
        ):
            self.assertEqual(benchmark._openvmm_pids(), [17, 4321])
            run.return_value = subprocess.CompletedProcess(
                [], 0, "INFO: No tasks are running which match the criteria.\r\n", ""
            )
            self.assertEqual(benchmark._openvmm_pids(), [])

        self.assertEqual(
            run.call_args.args[0][:3],
            ["tasklist.exe", "/FI", "IMAGENAME eq openvmm.exe"],
        )

    def test_openvmm_pids_reads_linux_process_names(self):
        with tempfile.TemporaryDirectory() as temporary:
            proc = Path(temporary)
            for pid, name in ((12, "openvmm"), (7, "python3"), (40, "openvmm")):
                (proc / str(pid)).mkdir()
                (proc / str(pid) / "comm").write_text(f"{name}\n", encoding="utf-8")
            # A process that exited before its name was read, and a link.
            (proc / "99").mkdir()
            (proc / "self").mkdir()
            with patch.object(benchmark.os, "name", "posix"):
                self.assertEqual(benchmark._openvmm_pids(proc), [12, 40])

    def test_live_peak_rss_samples_linux_process_without_reaping(self):
        process = MagicMock(pid=123)
        with (
            patch.object(benchmark.os, "name", "posix"),
            patch.object(benchmark.sys, "platform", "linux"),
            patch.object(
                benchmark, "_linux_live_peak_rss_bytes", side_effect=[4096, None, 0]
            ) as read,
        ):
            self.assertEqual(benchmark.live_peak_rss_bytes(process), 4096)
            self.assertIsNone(benchmark.live_peak_rss_bytes(process))
            with self.assertRaisesRegex(RuntimeError, "peak RSS 0 bytes"):
                benchmark.live_peak_rss_bytes(process)

        self.assertEqual(read.call_args_list, [call(123)] * 3)
        process.poll.assert_not_called()

    def test_live_peak_rss_accepts_only_running_windows_samples(self):
        running = MagicMock(pid=7)
        running.poll.return_value = None
        exited = MagicMock(pid=8)
        exited.poll.return_value = 0
        racing = MagicMock(pid=9)
        racing.poll.return_value = None

        def read_counters(pid: int) -> int:
            if pid == racing.pid:
                racing.poll.return_value = 0
            return 8192

        with (
            patch.object(benchmark.os, "name", "nt"),
            patch.object(
                benchmark, "windows_peak_rss_bytes", side_effect=read_counters
            ) as read,
        ):
            self.assertEqual(benchmark.live_peak_rss_bytes(running), 8192)
            # A retained handle still reports counters, including teardown.
            self.assertIsNone(benchmark.live_peak_rss_bytes(exited))
            self.assertIsNone(benchmark.live_peak_rss_bytes(racing))

        self.assertEqual(read.call_args_list, [call(7), call(8), call(9)])
        with (
            patch.object(benchmark.os, "name", "nt"),
            patch.object(
                benchmark, "windows_peak_rss_bytes", side_effect=OSError("closed")
            ),
        ):
            self.assertIsNone(benchmark.live_peak_rss_bytes(exited))
            with self.assertRaisesRegex(OSError, "closed"):
                benchmark.live_peak_rss_bytes(running)

    def test_linux_live_peak_rss_requires_process_address_space(self):
        running = "Name:\topenvmm\nState:\tS (sleeping)\nVmHWM:\t   59008 kB\n"
        zombie = "Name:\topenvmm\nState:\tZ (zombie)\nThreads:\t1\n"
        with patch.object(
            Path,
            "read_text",
            autospec=True,
            side_effect=[running, zombie, FileNotFoundError(), ProcessLookupError()],
        ) as read:
            self.assertEqual(benchmark._linux_live_peak_rss_bytes(123), 59008 * 1024)
            for _ in range(3):
                self.assertIsNone(benchmark._linux_live_peak_rss_bytes(123))

        self.assertEqual(read.call_args.args[0], Path("/proc/123/status"))
        with (
            patch.object(Path, "read_text", side_effect=PermissionError()),
            self.assertRaises(PermissionError),
        ):
            benchmark._linux_live_peak_rss_bytes(123)

    def test_linux_process_counters_read_only_kernel_counters(self):
        # Profiles sample these counters inside measured intervals, where
        # reading smaps_rollup slowed MSHV snapshot generation (#383).
        files = {
            Path("/proc/123/status"): (
                "Name:\topenvmm\nVmHWM:\t   59008 kB\nVmRSS:\t   57344 kB\n"
            ),
            Path("/proc/123/stat"): (
                "123 (openvmm) S 1 123 123 0 -1 4194560 812 0 3 0 5 2 0 0 20 0 9\n"
            ),
        }

        def read_text(path: Path, encoding: str) -> str:
            self.assertEqual(encoding, "ascii")
            return files[path]

        with patch.object(
            Path, "read_text", autospec=True, side_effect=read_text
        ) as read:
            counters = benchmark._linux_process_resource_counters(123)

        self.assertEqual(
            counters,
            {
                "rss_bytes": 57344 * 1024,
                "peak_rss_bytes": 59008 * 1024,
                "minor_faults": 812,
                "major_faults": 3,
                "page_faults": 815,
            },
        )
        self.assertEqual(
            [entry.args[0] for entry in read.call_args_list],
            [Path("/proc/123/status"), Path("/proc/123/stat")],
        )

    def test_process_counters_use_linux_counters_and_tolerate_exit(self):
        with (
            patch.object(benchmark.os, "name", "posix"),
            patch.object(benchmark.sys, "platform", "linux"),
            patch.object(
                benchmark,
                "_linux_process_resource_counters",
                side_effect=[
                    {"rss_bytes": 4096},
                    FileNotFoundError(),
                    ProcessLookupError(),
                ],
            ) as read,
        ):
            self.assertEqual(
                benchmark.process_resource_counters(123), {"rss_bytes": 4096}
            )
            # OpenVMM can exit between a profile record and the read.
            self.assertEqual(benchmark.process_resource_counters(123), {})
            self.assertEqual(benchmark.process_resource_counters(123), {})

        self.assertEqual(read.call_args_list, [call(123)] * 3)

    def test_builds_isolated_workload_command(self):
        command = benchmark.workload_boot_command(
            Path("openvmm"),
            "kvm",
            Path("vmlinux"),
            Path("initramfs.cpio.gz"),
            128,
            "quiet loglevel=0 nokaslr",
            processors=8,
            command_prefix=("taskset", "-c", "2-3"),
            network="10.0.0.2/24",
            mount="/mnt/host,C:/work,rw",
        )

        self.assertEqual(command[:4], ["taskset", "-c", "2-3", "openvmm"])
        self.assertEqual(
            command[4:10],
            [
                "--single-process",
                "--machine",
                "microvm",
                "--processors",
                "8",
                "--hypervisor",
            ],
        )
        self.assertIn("quiet loglevel=0 nokaslr", command)
        self.assertEqual(
            command[-8:],
            [
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--mount",
                "/mnt/host,C:/work,rw",
                "--virtio-console",
                "console",
            ],
        )

    def test_benchmark_network_requires_explicit_profile(self):
        args = nvx.parse_args(
            [
                "benchmark",
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
            ]
        )
        self.assertEqual(args.network_profile, "portable")

        args.network_profile = None
        with self.assertRaisesRegex(ValueError, "--net and --network-profile"):
            benchmark.run(args)

    def test_benchmark_scratch_directory_routes_temporary_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary).resolve() / "scratch"
            scratch.mkdir()
            args = nvx.parse_args(["benchmark", "--scratch-dir", str(scratch)])
            previous = tempfile.tempdir

            with (
                benchmark.benchmark_scratch_directory(args),
                patch.object(benchmark, "_git_revision", return_value="revision"),
            ):
                self.assertEqual(Path(tempfile.gettempdir()), scratch)
                with tempfile.TemporaryDirectory(prefix="openvmm-e2e-") as snapshot:
                    self.assertEqual(Path(snapshot).parent, scratch)
                document = benchmark.result_document(args, None, None)

            self.assertEqual(tempfile.tempdir, previous)
            self.assertEqual(args.scratch_dir, scratch)
            self.assertEqual(
                document["controls"]["scratch_directory"],
                str(scratch),
            )

    def test_benchmark_scratch_directory_defaults_to_system_temporary(self):
        args = nvx.parse_args(["benchmark"])
        previous = tempfile.tempdir

        with (
            benchmark.benchmark_scratch_directory(args),
            patch.object(benchmark, "_git_revision", return_value="revision"),
        ):
            self.assertEqual(tempfile.tempdir, previous)
            document = benchmark.result_document(args, None, None)

        self.assertIsNone(args.scratch_dir)
        self.assertEqual(
            document["controls"]["scratch_directory"],
            str(Path(tempfile.gettempdir()).resolve()),
        )

    def test_benchmark_scratch_directory_must_exist(self):
        with tempfile.TemporaryDirectory() as temporary:
            missing = Path(temporary) / "missing"
            args = nvx.parse_args(["benchmark", "--scratch-dir", str(missing)])
            previous = tempfile.tempdir

            with self.assertRaisesRegex(ValueError, "scratch directory does not exist"):
                benchmark.run(args)

            self.assertEqual(tempfile.tempdir, previous)

    def test_network_snapshot_restore_selects_portable_profile(self):
        command = benchmark.snapshot_restore_command(
            Path("openvmm"),
            "mshv",
            Path("snapshot"),
            processors=4,
            network_profile="portable",
        )

        self.assertEqual(
            command[2:8],
            ["--machine", "microvm", "--processors", "4", "--hypervisor", "mshv"],
        )

        self.assertEqual(
            command[-4:],
            [
                "--network-profile",
                "portable",
                "--virtio-console",
                "console",
            ],
        )

    def test_workload_commands_leave_out_the_boot_console_on_request(self):
        boot = benchmark.workload_boot_command(
            Path("openvmm"),
            "kvm",
            Path("vmlinux"),
            Path("initrd"),
            128,
            "quiet loglevel=0",
            boot_console=False,
        )
        restore = benchmark.snapshot_restore_command(
            Path("openvmm"), "kvm", Path("snapshot"), boot_console=False
        )

        self.assertNotIn("--virtio-console", boot)
        self.assertNotIn("--virtio-console", restore)

    def test_snapshot_restore_command_separates_capacity_from_online_target(self):
        command = benchmark.snapshot_restore_command(
            Path("openvmm"),
            "kvm",
            Path("snapshot"),
            processors=8,
            restore_processors=4,
        )

        self.assertEqual(command[command.index("--processors") + 1], "8")
        self.assertEqual(command[command.index("--restore-processors") + 1], "4")
        # The restore packet gives every restore fresh entropy.
        self.assertNotIn("--restore-entropy", command)

    def test_snapshot_restore_command_sets_memory_target_separately(self):
        command = benchmark.snapshot_restore_command(
            Path("openvmm"),
            "whp",
            Path("snapshot"),
            restore_memory_mib=2048,
        )

        self.assertNotIn("--memory", command)
        self.assertEqual(command[command.index("--restore-memory") + 1], "2048M")

    def test_memory_online_latency_parser_validates_added_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "restore.log"
            log.write_bytes(
                b"NVX-MEMORY-ONLINE-OK: added_bytes=536870912 "
                b"memtotal_kib=1000000 elapsed_us=12345\n"
            )
            self.assertEqual(
                benchmark._memory_online_elapsed_ms(log, 536870912),
                12.345,
            )
            with self.assertRaisesRegex(RuntimeError, "expected 1"):
                benchmark._memory_online_elapsed_ms(log, 1)

    def test_benchmark_snapshot_restore_propagates_processors(self):
        args = argparse.Namespace(
            processors=4,
            network_profile=None,
            warmups=1,
            runs=1,
            timeout=1.0,
            teardown_mode="guest-exit",
        )
        result = cast(benchmark.BenchmarkResult, {"p50_ms": 10.0})
        with patch.object(benchmark, "benchmark", return_value=result) as run:
            self.assertIs(
                benchmark.benchmark_snapshot_restore(
                    args,
                    Path("openvmm"),
                    "kvm",
                    ["unused-cold-command"],
                    snapshot_path=Path("snapshot"),
                ),
                result,
            )

        command = run.call_args.args[0]
        self.assertEqual(
            command[2:8],
            ["--machine", "microvm", "--processors", "4", "--hypervisor", "kvm"],
        )
        self.assertTrue(run.call_args.kwargs["marker_must_be_line"])
        self.assertTrue(run.call_args.kwargs["guest_exit_prequeued"])

    def test_parses_dd_rates_and_network_gateway(self):
        output = """
67108864 bytes copied, 0.25 s, 268.4 MB/s
67108864 bytes copied, 0.04 s, 1.6 GB/s
"""

        self.assertEqual(benchmark.parse_dd_rate(output, 1), 268.4)
        self.assertEqual(benchmark.parse_dd_rate(output, 2), 1600.0)
        self.assertEqual(benchmark.network_gateway("10.0.0.2/24"), "10.0.0.1")

    def test_benchmark_command_lines_carry_no_clock_tuning(self):
        # Under the time ABI the hypervisor identity gives Linux its TSC and
        # LAPIC rates and trust, so the lifecycle line tunes no clock and the
        # cold-start clocksource variant selects tsc on every backend.
        for token in (
            "tsc=",
            "no_timer_check",
            "clocksource=",
            "tsc_early_khz=",
            "lapic_timer_hz=",
        ):
            self.assertNotIn(token, benchmark.BASE_TUNING)
        args = argparse.Namespace(
            runs=1,
            warmups=0,
            memory_mib=128,
            processors=1,
            timeout=10.0,
            teardown_mode="guest-exit",
        )
        for backend in ("kvm", "mshv", "whp"):
            commands: list[list[str]] = []

            def fake_benchmark(
                command: list[str],
                commands: list[list[str]] = commands,
                **_kwargs: object,
            ):
                commands.append(command)
                return {"samples_ms": [100.0]}

            with (
                self.subTest(backend=backend),
                patch.object(benchmark, "benchmark", side_effect=fake_benchmark),
                contextlib.redirect_stdout(io.StringIO()) as stdout,
            ):
                benchmark.benchmark_cold_start_workload(
                    args, Path("openvmm"), Path("vmlinux"), Path("initrd"), backend
                )
            cmdlines = [command[command.index("--cmdline") + 1] for command in commands]
            self.assertIn("quiet loglevel=0 clocksource=tsc", cmdlines)
            self.assertFalse(any("kvm-clock" in cmdline for cmdline in cmdlines))
            self.assertIn("  clocksource=tsc          :", stdout.getvalue())

    def test_smp_probe_uses_explicit_topology_and_worker_rendezvous(self):
        script = benchmark.smp_probe_script(4)

        self.assertIn("getconf _NPROCESSORS_ONLN", script)
        self.assertIn("physical_package_id", script)
        self.assertIn("die_id", script)
        self.assertIn("thread_siblings_list", script)
        self.assertIn('taskset -c "$cpu"', script)
        self.assertIn("for pid in $worker_pids", script)
        self.assertIn("loc_before", script)
        self.assertIn("SMP-LAPIC-FAIL", script)
        self.assertIn("SMP-IPI-FAIL", script)
        self.assertIn("read_loc_counter", script)
        self.assertNotIn("while :; do", script)
        self.assertIn("timer_attempts=10000", script)
        self.assertIn('[ "$after" -gt "$before" ]', script)
        self.assertIn('[ "$after" -ge "$current" ]', script)
        self.assertNotIn("sleep 0.1", script)
        self.assertIn("apic_ids=0,1,2,3 bsp=0 workers=$workers", script)
        self.assertIn("NVX-SMP-PROBE-OK", script)
        self.assertTrue(script.endswith("nvx-exit 0\n"))
        network_script = benchmark.smp_probe_script(
            4,
            network_gateway="10.0.0.1",
            ioapic_irq=16,
        )
        self.assertIn('ping -c 2 -W 1 "10.0.0.1"', network_script)
        self.assertIn("SMP-IOAPIC-FAIL", network_script)
        self.assertIn("SMP-IOAPIC-OK", network_script)
        with self.assertRaises(ValueError):
            benchmark.smp_probe_script(3)
        with self.assertRaises(ValueError):
            benchmark.smp_probe_script(4, network_gateway="10.0.0.1")

    def test_snapshot_request_triggers_waiting_capture_controller(self):
        script = benchmark.snapshot_request_script(
            4,
            teardown_mode="guest-exit",
        )
        self.assertEqual(script, "nvx-snapshot\n")

        no_controller = benchmark.snapshot_request_script(
            None,
            teardown_mode="guest-exit",
        )
        self.assertEqual(
            no_controller,
            "echo NVX-SNAPSHOT-DISPATCHED\n"
            "nvx-snapshot\n"
            "echo OPENVMM-SNAPSHOT-RESTORE-OK\n"
            "nvx-exit 0\n",
        )
        self.assertNotIn(
            "nvx-exit",
            benchmark.snapshot_request_script(None, teardown_mode="host-terminate"),
        )

    def test_prepare_snapshot_capture_stages_waiting_controller(self):
        script = benchmark.prepare_snapshot_capture_script(
            4,
            teardown_mode="guest-exit",
            network_gateway="10.0.0.1",
            ioapic_irq=16,
        )

        self.assertTrue(
            script.startswith(
                f"cat >{benchmark.SMP_PROBE_PATH} <<'NVX_SMP_PROBE_SCRIPT'\n"
            )
        )
        self.assertIn('ping -c 2 -W 1 "10.0.0.1"', script)
        self.assertIn("NVX-SMP-PROBE-OK", script)
        self.assertIn(
            f"cat >{benchmark.SNAPSHOT_CAPTURE_PATH} <<'NVX_SNAPSHOT_CAPTURE_SCRIPT'\n",
            script,
        )
        self.assertIn("IFS= read -r trigger\n", script)
        self.assertIn("echo NVX-SNAPSHOT-DISPATCHED\n", script)
        self.assertIn(
            "/sbin/nvx-snapshot\necho OPENVMM-SNAPSHOT-RESTORE-OK\nnvx-exit 0\n",
            script,
        )
        host_terminated = benchmark.prepare_snapshot_capture_script(
            4, teardown_mode="host-terminate"
        )
        self.assertNotIn("nvx-exit 0", host_terminated)
        self.assertTrue(
            script.endswith(
                "NVX_SNAPSHOT_CAPTURE_SCRIPT\n"
                f"chmod +x {benchmark.SMP_PROBE_PATH} "
                f"{benchmark.SNAPSHOT_CAPTURE_PATH}\n"
                f"{benchmark.SNAPSHOT_CAPTURE_PATH}\n"
            )
        )

    def test_prepare_snapshot_capture_runs_a_pre_capture_script_after_the_trigger(
        self,
    ):
        script = benchmark.prepare_snapshot_capture_script(
            1,
            teardown_mode="guest-exit",
            pre_capture_script="echo prepare\n",
            post_restore_script="echo repair\n",
        )

        controller = script.split("<<'NVX_SNAPSHOT_CAPTURE_SCRIPT'\n", 1)[1]
        controller = controller.split("NVX_SNAPSHOT_CAPTURE_SCRIPT\n", 1)[0]
        self.assertIn(
            "IFS= read -r trigger\n"
            'if [ "$trigger" != nvx-snapshot ]; then\n'
            "  /sbin/nvx-exit 90\n"
            "  exit 90\n"
            "fi\n"
            f"{benchmark.SNAPSHOT_PRE_CAPTURE_PATH}\n"
            "echo NVX-SNAPSHOT-DISPATCHED\n"
            "/sbin/nvx-snapshot\n"
            f"{benchmark.SNAPSHOT_POST_RESTORE_PATH}\n"
            "echo OPENVMM-SNAPSHOT-RESTORE-OK\n",
            controller,
        )
        self.assertTrue(
            script.endswith(
                f"cat >{benchmark.SNAPSHOT_PRE_CAPTURE_PATH} "
                "<<'NVX_PRE_CAPTURE_SCRIPT'\n"
                "echo prepare\nNVX_PRE_CAPTURE_SCRIPT\n"
                f"chmod +x {benchmark.SNAPSHOT_PRE_CAPTURE_PATH}\n"
                f"cat >{benchmark.SNAPSHOT_POST_RESTORE_PATH} "
                "<<'NVX_POST_RESTORE_SCRIPT'\n"
                "echo repair\nNVX_POST_RESTORE_SCRIPT\n"
                f"chmod +x {benchmark.SNAPSHOT_POST_RESTORE_PATH}\n"
                f"{benchmark.SNAPSHOT_CAPTURE_PATH}\n"
            ),
            script,
        )

    def test_capture_does_not_wait_for_a_clocksource(self):
        # The time ABI registers the tsc clocksource at device_initcall on
        # every backend, so no capture waits for Linux to leave tsc-early.
        script = benchmark.prepare_snapshot_capture_script(
            1, teardown_mode="guest-exit"
        )
        probe = script.split("<<'NVX_SMP_PROBE_SCRIPT'\n", 1)[1]
        probe = probe.split("NVX_SMP_PROBE_SCRIPT\n", 1)[0]
        self.assertEqual(probe, benchmark.smp_probe_script(1, exit_guest=False))
        for text in ("clocksource", "tsc-early", "clock_tries"):
            self.assertNotIn(text, script)
        self.assertFalse(hasattr(benchmark, "stable_clocksource_wait_script"))

    def test_output_marker_must_be_a_complete_line(self):
        marker = benchmark.RESTORE_MARKER
        self.assertFalse(
            benchmark.contains_output_line(
                b"/ # echo OPENVMM-SNAPSHOT-RESTORE-OK\r\n",
                marker,
            )
        )
        self.assertTrue(
            benchmark.contains_output_line(
                b"/ # echo OPENVMM-SNAPSHOT-RESTORE-OK\r\n"
                b"OPENVMM-SNAPSHOT-RESTORE-OK\r\n",
                marker,
            )
        )

    def test_device_restore_marker_and_trace_parsers(self):
        marker = benchmark.parse_device_restore_marker(
            "NVX-VIRTIO-RESTORE-PROBE phase=io-success device=net mode=deferred\r"
        )
        self.assertEqual(
            marker,
            {"phase": "io-success", "device": "net", "mode": "deferred"},
        )
        self.assertIsNone(benchmark.parse_device_restore_marker("unrelated"))

        event = benchmark.parse_virtio_restore_event(
            "DEBUG virtio_restore: virtio restore lifecycle "
            'event: "queue_start", device_type: 0x1a, trigger: "driver-ok", '
            "queue_index: 0, restored_progress: true, success: true"
        )
        self.assertEqual(
            event,
            {
                "event": "queue_start",
                "device_type": 0x1A,
                "trigger": "driver-ok",
                "queue_index": 0,
                "restored_progress": True,
                "success": True,
            },
        )
        self.assertIsNone(benchmark.parse_virtio_restore_event("unrelated"))

    def test_device_restore_commands_use_fixed_attachments(self):
        common = (
            Path("openvmm"),
            "whp",
            Path("vmlinux"),
            Path("initrd"),
            256,
        )
        console_boot, console_restore = benchmark.device_restore_commands(
            *common, "console", "deferred"
        )
        self.assertIn(
            "nvx_virtio_restore_probe=console,deferred",
            console_boot[console_boot.index("--cmdline") + 1],
        )
        self.assertEqual(console_boot[-2:], ["--virtio-console", "console"])
        self.assertEqual(console_restore[-2:], ["--virtio-console", "console"])

        net_boot, net_restore = benchmark.device_restore_commands(
            *common, "net", "active"
        )
        self.assertIn("--network-profile", net_boot)
        self.assertIn("--network-profile", net_restore)

        fs_boot, fs_restore = benchmark.device_restore_commands(
            *common,
            "virtiofs",
            "deferred",
            host_directory=Path("host-share"),
        )
        self.assertIn("--mount", fs_boot)
        self.assertIn("--mount", fs_restore)

    def test_device_restore_sample_rejects_premature_activation(self):
        markers = [
            {
                "phase": "restore-ready",
                "device": "net",
                "mode": "deferred",
                "observer_elapsed_ms": "8.0",
            },
            {
                "phase": "trigger",
                "device": "net",
                "mode": "deferred",
                "observer_elapsed_ms": "10.0",
            },
        ]
        events: list[dict[str, object]] = [
            {
                "event": "restore_staged",
                "device_type": 1,
                "trigger": "inactive-start",
                "queue_index": -1,
                "restored_progress": False,
                "success": True,
                "observer_elapsed_ms": 5.0,
            },
            {
                "event": "private_state_apply",
                "device_type": 1,
                "trigger": "kick",
                "queue_index": -1,
                "restored_progress": False,
                "success": True,
                "observer_elapsed_ms": 9.0,
            },
            {
                "event": "kick_staged",
                "device_type": 1,
                "trigger": "kick",
                "queue_index": 0,
                "restored_progress": False,
                "success": True,
                "observer_elapsed_ms": 10.5,
            },
        ]
        for queue_index in range(2):
            events.append(
                {
                    "event": "queue_start",
                    "device_type": 1,
                    "trigger": "driver-ok",
                    "queue_index": queue_index,
                    "restored_progress": False,
                    "success": True,
                    "observer_elapsed_ms": 11.0,
                }
            )
        events.append(
            {
                "event": "kick_dispatch",
                "device_type": 1,
                "trigger": "driver-ok",
                "queue_index": 0,
                "restored_progress": False,
                "success": True,
                "observer_elapsed_ms": 12.0,
            }
        )
        sample = cast(
            benchmark.DeviceRestoreSample,
            {
                "process_launch_to_ready_ms": 8.0,
                "trigger_to_first_successful_io_ms": 2.0,
                "peak_rss_bytes": 1,
                "guest_markers": markers,
                "events": events,
                "queue_start_count": 0,
                "staged_kick_dispatch_count": 0,
                "stale_premature_callback_count": 0,
                "log": "sample.log",
            },
        )
        with self.assertRaisesRegex(RuntimeError, "premature"):
            benchmark.validate_device_restore_sample(sample, "net", "deferred")
        events[1]["observer_elapsed_ms"] = 10.25
        benchmark.validate_device_restore_sample(sample, "net", "deferred")
        self.assertEqual(sample["queue_start_count"], 2)
        self.assertEqual(sample["staged_kick_dispatch_count"], 1)
        self.assertEqual(sample["stale_premature_callback_count"], 0)

    def test_device_restore_profile_writes_standalone_result(self):
        args = argparse.Namespace(
            processors=1,
            restore_devices=["console"],
            restore_modes=["active"],
            net=None,
            memory_mib=128,
            timeout=1.0,
            warmups=1,
            runs=2,
            platform="windows-whp-baremetal",
        )

        def sample() -> benchmark.DeviceRestoreSample:
            return {
                "process_launch_to_ready_ms": 10.0,
                "trigger_to_first_successful_io_ms": 2.0,
                "peak_rss_bytes": 1024,
                "guest_markers": [
                    {
                        "phase": "restore-ready",
                        "device": "console",
                        "mode": "active",
                        "observer_elapsed_ms": "10.0",
                    },
                    {
                        "phase": "trigger",
                        "device": "console",
                        "mode": "active",
                        "observer_elapsed_ms": "11.0",
                    },
                ],
                "events": [
                    {
                        "event": "restore_staged",
                        "device_type": 3,
                        "trigger": "active-start",
                        "queue_index": -1,
                        "restored_progress": False,
                        "success": True,
                    },
                    {
                        "event": "private_state_apply",
                        "device_type": 3,
                        "trigger": "active-start",
                        "queue_index": -1,
                        "restored_progress": False,
                        "success": True,
                    },
                    {
                        "event": "kick_dispatch",
                        "device_type": 3,
                        "trigger": "kick",
                        "queue_index": 1,
                        "restored_progress": False,
                        "success": True,
                    },
                    *[
                        {
                            "event": "queue_start",
                            "device_type": 3,
                            "trigger": "active-start",
                            "queue_index": queue_index,
                            "restored_progress": True,
                            "success": True,
                        }
                        for queue_index in range(2)
                    ],
                ],
                "queue_start_count": 0,
                "staged_kick_dispatch_count": 0,
                "stale_premature_callback_count": 0,
                "log": "",
            }

        def restore_sample(
            *_args: object, **_kwargs: object
        ) -> benchmark.DeviceRestoreSample:
            return sample()

        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            with (
                patch.object(
                    benchmark,
                    "capture_device_restore_snapshot",
                    return_value=[
                        {
                            "phase": "capture-ready",
                            "device": "console",
                            "mode": "active",
                        }
                    ],
                ) as capture,
                patch.object(
                    benchmark,
                    "run_device_restore_sample",
                    side_effect=restore_sample,
                ) as restore,
            ):
                path = benchmark.benchmark_device_restore_profile(
                    args,
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "whp",
                    output_dir,
                )

            self.assertEqual(capture.call_count, 1)
            self.assertEqual(restore.call_count, 3)
            document = json.loads(path.read_text(encoding="utf-8"))
            self.assertTrue(document["non_canonical"])
            self.assertTrue(document["assertions"]["zero_stale_premature_callbacks"])
            scenario = document["scenarios"]["console-active"]
            self.assertEqual(
                scenario["process_launch_to_ready_ms"]["samples"], [10.0, 10.0]
            )
            self.assertEqual(scenario["process_launch_to_ready_ms"]["p50"], 10.0)
            self.assertEqual(scenario["queue_start_counts"], [2, 2])

    def test_device_restore_profile_writes_lf_only_virtiofs_seed(self):
        args = argparse.Namespace(
            processors=1,
            restore_devices=["virtiofs"],
            restore_modes=["active"],
            net=None,
            memory_mib=128,
            timeout=1.0,
            warmups=1,
            runs=1,
            platform="windows-whp-baremetal",
        )
        observed_seeds: list[bytes] = []
        host_directories: list[Path] = []

        def commands(*_args: object, **kwargs: object) -> tuple[list[str], list[str]]:
            host_directory = cast(Path, kwargs["host_directory"])
            host_directories.append(host_directory)
            observed_seeds.append(
                (host_directory / ".nvx-virtio-restore-host-seed").read_bytes()
            )
            return ["boot"], ["restore"]

        sample = cast(
            benchmark.DeviceRestoreSample,
            {
                "process_launch_to_ready_ms": 1.0,
                "trigger_to_first_successful_io_ms": 1.0,
                "peak_rss_bytes": 1,
                "guest_markers": [],
                "events": [],
                "queue_start_count": 0,
                "staged_kick_dispatch_count": 0,
                "stale_premature_callback_count": 0,
                "log": "sample.log",
            },
        )

        def run_sample(
            *_args: object, **_kwargs: object
        ) -> benchmark.DeviceRestoreSample:
            (host_directories[-1] / ".nvx-virtio-restore-guest-result").write_bytes(
                benchmark.VIRTFS_GUEST_TO_HOST
            )
            return sample

        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(
                    benchmark, "device_restore_commands", side_effect=commands
                ),
                patch.object(
                    benchmark,
                    "capture_device_restore_snapshot",
                    return_value=[],
                ),
                patch.object(
                    benchmark,
                    "run_device_restore_sample",
                    side_effect=run_sample,
                ),
                patch.object(
                    benchmark,
                    "summarize_device_restore_samples",
                    return_value={"stale_premature_callback_count": 0},
                ),
            ):
                benchmark.benchmark_device_restore_profile(
                    args,
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "whp",
                    Path(temporary),
                )

        self.assertEqual(observed_seeds, [benchmark.VIRTFS_HOST_TO_GUEST])

    def test_device_restore_profile_rejects_reused_output(self):
        args = argparse.Namespace(
            suite="device-restore-profile",
            output=None,
            output_dir=None,
            platform="linux-kvm-baremetal",
        )
        with tempfile.TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            (output_dir / "stale.log").write_text("stale", encoding="ascii")
            args.output_dir = output_dir
            with self.assertRaisesRegex(FileExistsError, "empty output directory"):
                benchmark.run_workload_benchmarks(
                    args,
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    "kvm",
                )

    def test_benchmark_cpu_set_reserves_host_worker_capacity(self):
        benchmark.validate_benchmark_cpu_set(set(range(10)), 8)
        with self.assertRaisesRegex(ValueError, "requires at least 10"):
            benchmark.validate_benchmark_cpu_set(set(range(9)), 8)
        benchmark.validate_benchmark_cpu_set({0, 2, 4}, 1)
        benchmark.validate_benchmark_cpu_set(set(range(8)), 8, 0)
        with self.assertRaisesRegex(ValueError, "requires at least 8"):
            benchmark.validate_benchmark_cpu_set(set(range(7)), 8, 0)

    def test_snapshot_capture_runs_smp_probe_for_selected_count(self):
        args = argparse.Namespace(
            warmups=0,
            runs=1,
            timeout=1.0,
            processors=8,
            teardown_mode="guest-exit",
        )
        with patch.object(
            benchmark,
            "capture_snapshot",
            return_value=(1.0, 1.0, 1.0, 1024),
        ) as capture:
            benchmark.benchmark_snapshot_capture(args, ["openvmm"])

        self.assertNotIn("backend", capture.call_args.kwargs)
        self.assertEqual(capture.call_args.kwargs["processors"], 8)
        self.assertEqual(capture.call_args.kwargs["teardown_mode"], "guest-exit")

    def test_network_workload_runs_unmeasured_smp_interrupt_preflight(self):
        args = argparse.Namespace(
            net=None,
            network_memory_mib=128,
            processors=4,
            warmups=0,
            runs=1,
            timeout=1.0,
            teardown_mode="guest-exit",
        )
        result: benchmark.BenchmarkResult = {
            "samples_ms": [1.0],
            "p50_ms": 1.0,
            "p95_ms": 1.0,
            "min_ms": 1.0,
            "max_ms": 1.0,
            "wall_samples_ms": [1.0],
            "wall_p50_ms": 1.0,
            "wall_p95_ms": 1.0,
            "wall_min_ms": 1.0,
            "wall_max_ms": 1.0,
            "peak_rss_samples_bytes": [1],
            "peak_rss_p50_bytes": 1,
            "peak_rss_min_bytes": 1,
            "peak_rss_max_bytes": 1,
            "peak_rss_remeasured_count": 0,
            "teardown_samples_ms": [1.0],
            "teardown_completed_samples_ms": [1.0],
            "teardown_timeout_count": 0,
            "teardown_timeout_seconds": 5.0,
            "teardown_p50_ms": 1.0,
            "teardown_p95_ms": 1.0,
            "teardown_min_ms": 1.0,
            "teardown_max_ms": 1.0,
        }
        # The NIC interrupts on IOAPIC input 16 on every backend.
        for backend in ("kvm", "mshv", "whp"):
            with (
                patch.object(benchmark, "run_guest_script") as probe,
                patch.object(benchmark, "benchmark", return_value=result),
                patch.object(benchmark, "capture_automatic_snapshot"),
            ):
                benchmark.benchmark_network_snapshot_workload(
                    args,
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initrd"),
                    backend,
                )

            self.assertEqual(probe.call_count, 1)
            self.assertIn("--network-profile", probe.call_args.args[0])
            self.assertIn("SMP-IOAPIC-OK", probe.call_args.args[1])
            self.assertIn("ioapic_irq=16\n", probe.call_args.args[1])
            self.assertEqual(
                probe.call_args.args[2], benchmark.SMP_PROBE_COMPLETION_MARKER
            )

    def test_shell_snapshot_restore_suite_only_measures_restore(self):
        args = argparse.Namespace(
            processors=8,
            shell_memories=[512],
            warmups=1,
            runs=5,
            timeout=40.0,
            teardown_mode="guest-exit",
            network_profile=None,
            snapshot_profile=True,
        )
        result = cast(
            benchmark.BenchmarkResult,
            {
                "samples_ms": [10.0, 12.0],
                "profile": {
                    "schema_version": 1,
                    "raw_samples": [],
                    "phases": {
                        "startup.partition_build": {
                            "exclusive": True,
                            "source": "openvmm",
                            "samples_ms": [1.0],
                            "p50_ms": 1.0,
                            "p95_ms": 1.0,
                            "min_ms": 1.0,
                            "max_ms": 1.0,
                        }
                    },
                },
            },
        )
        with (
            patch.object(benchmark, "capture_snapshot") as capture,
            patch.object(benchmark, "benchmark", return_value=result) as run,
            patch("sys.stdout", new_callable=io.StringIO) as output,
        ):
            benchmark.benchmark_shell_snapshot_restore_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "whp",
            )

        capture.assert_called_once()
        capture_command = capture.call_args.args[0]
        self.assertNotIn("shellsnap", capture_command)
        self.assertEqual(capture.call_args.kwargs["processors"], 8)
        self.assertEqual(capture.call_args.kwargs["teardown_mode"], "guest-exit")
        run.assert_called_once()
        restore_command = run.call_args.args[0]
        self.assertIn("--restore-snapshot", restore_command)
        self.assertEqual(
            restore_command[restore_command.index("--processors") + 1], "8"
        )
        self.assertIn("== 512 MiB ==", output.getvalue())
        self.assertIn("snapshot restore", output.getvalue())
        self.assertIn("  restore samples     : 10.000, 12.000 ms\n", output.getvalue())
        self.assertNotIn("cold boot", output.getvalue())
        self.assertIn('marker : "OPENVMM-SNAPSHOT-RESTORE-OK"', output.getvalue())
        self.assertTrue(run.call_args.kwargs["snapshot_profile"])
        self.assertEqual(run.call_args.kwargs["marker"], benchmark.RESTORE_MARKER)
        self.assertTrue(run.call_args.kwargs["marker_must_be_line"])
        self.assertTrue(run.call_args.kwargs["guest_exit_prequeued"])
        self.assertIn(
            "shell-snapshot-restore/whp/8vcpu/512-mib lifecycle phases:",
            output.getvalue(),
        )

    def test_restore_vcpu_suite_reuses_one_capacity_snapshot_for_matrix(self):
        args = argparse.Namespace(
            processors=8,
            memory_mib=512,
            warmups=1,
            runs=5,
            timeout=40.0,
            teardown_mode="guest-exit",
            snapshot_profile=True,
        )
        result = cast(
            benchmark.BenchmarkResult,
            {
                "samples_ms": [10.0, 12.0],
                "peak_rss_samples_bytes": [1024, 2048],
                "profile": {
                    "schema_version": 1,
                    "raw_samples": [],
                    "phases": {
                        "restore.saved_state_restore": {
                            "exclusive": True,
                            "source": "openvmm",
                            "samples_ms": [2.0],
                            "p50_ms": 2.0,
                            "p95_ms": 2.0,
                            "min_ms": 2.0,
                            "max_ms": 2.0,
                        }
                    },
                },
            },
        )
        with (
            patch.object(benchmark, "capture_automatic_snapshot") as capture,
            patch.object(benchmark, "benchmark", return_value=result) as run,
            patch("sys.stdout", new_callable=io.StringIO) as output,
        ):
            benchmark.benchmark_snapshot_restore_vcpu_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "mshv",
            )

        capture.assert_called_once()
        self.assertEqual(capture.call_args.kwargs["timeout"], args.timeout)
        capture_command = capture.call_args.args[0]
        self.assertEqual(
            capture_command[capture_command.index("--processors") + 1], "8"
        )
        self.assertTrue(any("maxcpus=1" in argument for argument in capture_command))
        self.assertEqual(run.call_count, 4)
        targets: list[int] = []
        for invocation in run.call_args_list:
            command = invocation.args[0]
            self.assertEqual(invocation.kwargs["timeout"], args.timeout)
            self.assertTrue(invocation.kwargs["snapshot_profile"])
            self.assertEqual(command[command.index("--processors") + 1], "8")
            targets.append(int(command[command.index("--restore-processors") + 1]))
        self.assertEqual(targets, [1, 2, 4, 8])
        self.assertIn("restore-online 8 vCPU", output.getvalue())
        self.assertIn(
            "snapshot-restore-vcpu/mshv/capacity-8/online-1 lifecycle phases:",
            output.getvalue(),
        )

        args.processors = 4
        with self.assertRaisesRegex(ValueError, "requires --processors 8"):
            benchmark.benchmark_snapshot_restore_vcpu_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
            )

    def test_platform_restore_vcpu_suite_restores_one_platform_capture(self):
        args = argparse.Namespace(
            processors=8,
            memory_mib=512,
            warmups=1,
            runs=5,
            timeout=40.0,
            teardown_mode="guest-exit",
            snapshot_profile=True,
        )
        result = cast(
            benchmark.BenchmarkResult,
            {
                "samples_ms": [10.0, 12.0],
                "peak_rss_samples_bytes": [1024, 2048],
                "profile": {
                    "schema_version": 1,
                    "raw_samples": [],
                    "phases": {
                        "restore.guest_repair_gate": {
                            "exclusive": False,
                            "source": "openvmm",
                            "samples_ms": [2.0],
                            "p50_ms": 2.0,
                            "p95_ms": 2.0,
                            "min_ms": 2.0,
                            "max_ms": 2.0,
                        }
                    },
                },
            },
        )

        def blocks(command: list[str]) -> list[str]:
            return [
                command[index + 1]
                for index, argument in enumerate(command)
                if argument == "--microvm-sandbox-block"
            ]

        prefix = ("taskset", "-c", "2")
        with (
            patch.object(benchmark, "capture_snapshot") as capture,
            patch.object(benchmark, "benchmark", return_value=result) as run,
            patch("sys.stdout", new_callable=io.StringIO) as output,
        ):
            benchmark.benchmark_platform_snapshot_restore_vcpu_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "mshv",
                command_prefix=prefix,
            )

        capture.assert_called_once()
        self.assertEqual(capture.call_args.kwargs["timeout"], args.timeout)
        self.assertEqual(capture.call_args.kwargs["processors"], 1)
        # The restored guest waits for the host after the marker instead of
        # running a prequeued exit, so its peak RSS can be sampled.
        self.assertNotEqual(capture.call_args.kwargs["teardown_mode"], "guest-exit")
        capture_command = capture.call_args.args[0]
        self.assertEqual(tuple(capture_command[:3]), prefix)
        self.assertEqual(
            capture_command[capture_command.index("--processors") + 1], "8"
        )
        # The platform command line takes no tenant tokens, such as quiet or
        # the pre-banner capture's shellsnap.
        self.assertEqual(
            capture_command[capture_command.index("--cmdline") + 1], "maxcpus=1"
        )
        self.assertEqual(
            capture_command[capture_command.index("--snapshot-tier") + 1], "platform"
        )
        capture_blocks = blocks(capture_command)
        self.assertEqual(len(capture_blocks), 2)
        self.assertRegex(capture_blocks[0], r"^distro:file:.+,ro$")
        self.assertRegex(capture_blocks[1], r"^scratch:file:.+(?<!,ro)$")
        self.assertEqual(run.call_count, 4)
        targets: list[int] = []
        for invocation in run.call_args_list:
            command = invocation.args[0]
            self.assertEqual(tuple(command[:3]), prefix)
            self.assertEqual(command[command.index("--processors") + 1], "8")
            self.assertEqual(blocks(command), capture_blocks)
            self.assertEqual(invocation.kwargs["timeout"], args.timeout)
            self.assertEqual(invocation.kwargs["marker"], benchmark.RESTORE_MARKER)
            self.assertTrue(invocation.kwargs["marker_must_be_line"])
            self.assertEqual(invocation.kwargs["teardown_mode"], "guest-exit")
            self.assertFalse(invocation.kwargs.get("guest_exit_prequeued", False))
            self.assertTrue(invocation.kwargs["snapshot_profile"])
            targets.append(int(command[command.index("--restore-processors") + 1]))
        self.assertEqual(targets, [1, 2, 4, 8])
        self.assertIn("platform snapshot restore vCPU activation", output.getvalue())
        self.assertIn("restore-online 8 vCPU", output.getvalue())
        self.assertIn(
            "platform-snapshot-restore-vcpu/mshv/capacity-8/online-1 lifecycle phases:",
            output.getvalue(),
        )

        args.teardown_mode = "host-terminate"
        with (
            patch.object(benchmark, "capture_snapshot"),
            patch.object(benchmark, "benchmark", return_value=result) as run,
            patch("sys.stdout", new_callable=io.StringIO),
        ):
            benchmark.benchmark_platform_snapshot_restore_vcpu_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
            )
        self.assertTrue(
            all(
                entry.kwargs["teardown_mode"] == "host-terminate"
                for entry in run.call_args_list
            )
        )

        args.processors = 4
        with self.assertRaisesRegex(ValueError, "requires --processors 8"):
            benchmark.benchmark_platform_snapshot_restore_vcpu_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
            )

    def test_platform_restore_vcpu_metadata_records_the_targets(self):
        args = nvx.parse_args(
            [
                "benchmark",
                "--suite",
                "platform-snapshot-restore-vcpu",
                "--backend",
                "kvm",
                "--processors",
                "8",
            ]
        )
        benchmark.apply_benchmark_suite_defaults(args)
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(
                benchmark,
                "benchmark_host_provenance",
                return_value={"machine_name": "host"},
            ),
            patch.object(benchmark, "sha256_file", return_value="0" * 64),
            patch.object(benchmark, "_git_revision", return_value="revision"),
            patch.object(benchmark, "_git_status", return_value=[]),
        ):
            path = benchmark.write_benchmark_metadata(
                args,
                Path(temporary),
                Path("openvmm"),
                Path("vmlinux"),
                Path("initramfs.cpio.gz"),
                "kvm",
            )
            document = json.loads(path.read_text(encoding="utf-8"))

        self.assertEqual(document["suite"], "platform-snapshot-restore-vcpu")
        self.assertEqual(document["processors"], 8)
        self.assertEqual(document["restore_processor_targets"], [1, 2, 4, 8])

    def test_benchmark_preserves_exact_process_wall_samples(self):
        samples = [
            (10.0, 1024, 2.0, 13.5),
            (12.0, 2048, 3.0, 16.5),
        ]
        with patch.object(benchmark, "measure_once", side_effect=samples):
            result = benchmark.benchmark(["openvmm"], warmups=0, runs=2, timeout=1)

        self.assertEqual(result["samples_ms"], [10.0, 12.0])
        self.assertEqual(result["p95_ms"], 12.0)
        self.assertEqual(result["wall_samples_ms"], [13.5, 16.5])
        self.assertEqual(result["wall_p50_ms"], 15.0)
        self.assertEqual(result["wall_p95_ms"], 16.5)
        self.assertEqual(
            benchmark.SNAPSHOT_FILENAMES,
            ("manifest.bin", "state.bin", "memory.bin"),
        )

    def test_benchmark_remeasures_attempt_without_marker_rss(self):
        attempts = iter(
            (
                (5.0, None, 1.0, 6.0),
                (10.0, None, 7.0, 17.0),
                (11.0, 2048, 7.0, 18.0),
                (12.0, 4096, 8.0, 20.0),
            )
        )

        def measure_once(
            *_args: object,
            profile_sink: list[dict[str, object]] | None = None,
            **_kwargs: object,
        ) -> tuple[float, int | None, float | None, float]:
            sample = next(attempts)
            if profile_sink is not None:
                profile_sink.append({"elapsed_ms": sample[0]})
            return sample

        before_each = MagicMock()
        with (
            patch.object(benchmark, "measure_once", side_effect=measure_once),
            patch.object(
                benchmark, "summarize_lifecycle_profiles", return_value={}
            ) as summarize,
            patch("sys.stdout", new_callable=io.StringIO) as output,
        ):
            result = benchmark.benchmark(
                ["openvmm"],
                warmups=1,
                runs=2,
                timeout=1,
                marker=benchmark.RESTORE_MARKER,
                guest_exit_prequeued=True,
                snapshot_profile=True,
                before_each=before_each,
            )

        self.assertEqual(result["samples_ms"], [11.0, 12.0])
        self.assertEqual(result["peak_rss_samples_bytes"], [2048, 4096])
        self.assertEqual(result["peak_rss_min_bytes"], 2048)
        self.assertEqual(result["peak_rss_remeasured_count"], 1)
        self.assertEqual(result["teardown_samples_ms"], [7.0, 8.0])
        self.assertEqual(before_each.call_count, 4)
        summarize.assert_called_once_with([{"elapsed_ms": 11.0}, {"elapsed_ms": 12.0}])
        self.assertIn("warmup 1/1: 5.000 ms, peak RSS=unavailable", output.getvalue())
        self.assertIn(
            "sample 1/2: discarded attempt 1/3 (10.000 ms)", output.getvalue()
        )

    def test_benchmark_rejects_rss_missing_from_every_attempt(self):
        with (
            patch.object(
                benchmark, "measure_once", return_value=(10.0, None, 7.0, 17.0)
            ) as measure,
            patch("sys.stdout", new_callable=io.StringIO),
            self.assertRaisesRegex(
                RuntimeError,
                f"{benchmark.PEAK_RSS_SAMPLE_ATTEMPTS} consecutive attempts "
                "for sample 1/2",
            ),
        ):
            benchmark.benchmark(["openvmm"], warmups=0, runs=2, timeout=1)

        self.assertEqual(measure.call_count, benchmark.PEAK_RSS_SAMPLE_ATTEMPTS)

    def test_nearest_rank_percentile(self):
        self.assertEqual(benchmark.nearest_rank_percentile(range(1, 22), 95), 20)
        self.assertEqual(benchmark.nearest_rank_percentile([7.0], 95), 7.0)
        with self.assertRaisesRegex(ValueError, "without samples"):
            benchmark.nearest_rank_percentile([], 95)

    def test_snapshot_capture_excludes_warmup_and_retains_last_snapshot(self):
        with tempfile.TemporaryDirectory() as temporary:
            retained = Path(temporary) / "retained"
            observed_paths: list[Path] = []

            def capture(
                _command: object,
                snapshot_path: Path,
                **_kwargs: object,
            ) -> tuple[float, float, float, int]:
                snapshot_path.mkdir()
                observed_paths.append(snapshot_path)
                sample = float(len(observed_paths))
                return sample, sample, sample / 10, len(observed_paths) * 1024

            args = argparse.Namespace(
                warmups=1,
                runs=2,
                timeout=1.0,
                processors=1,
                teardown_mode="guest-exit",
            )
            with patch.object(benchmark, "capture_snapshot", side_effect=capture):
                result = benchmark.benchmark_snapshot_capture(
                    args,
                    ["openvmm"],
                    retained_snapshot_path=retained,
                )

            self.assertEqual(result["samples_ms"], [2.0, 3.0])
            self.assertEqual(result["peak_rss_samples_bytes"], [2048, 3072])
            self.assertEqual(result["peak_rss_p50_bytes"], 2560)
            self.assertEqual(result["peak_rss_max_bytes"], 3072)
            self.assertEqual(observed_paths[-1], retained)
            self.assertTrue(retained.is_dir())

    def test_snapshot_restore_reuses_provided_snapshot(self):
        with tempfile.TemporaryDirectory() as temporary:
            snapshot = Path(temporary) / "snapshot"
            snapshot.mkdir()
            args = argparse.Namespace(
                warmups=1,
                runs=2,
                timeout=3.0,
                teardown_mode="guest-exit",
                network_profile=None,
                processors=1,
            )
            expected = {"p50_ms": 1.0}
            with (
                patch.object(benchmark, "capture_snapshot") as capture,
                patch.object(benchmark, "benchmark", return_value=expected) as run,
            ):
                result = benchmark.benchmark_snapshot_restore(
                    args,
                    Path("openvmm"),
                    "whp",
                    ["openvmm", "--kernel", "vmlinux"],
                    snapshot_path=snapshot,
                )

            self.assertIs(result, expected)
            capture.assert_not_called()
            self.assertIn(str(snapshot), run.call_args.args[0])
            self.assertEqual(run.call_args.kwargs["marker"], benchmark.RESTORE_MARKER)
            self.assertTrue(run.call_args.kwargs["marker_must_be_line"])
            self.assertEqual(run.call_args.kwargs["teardown_mode"], "guest-exit")
            self.assertTrue(run.call_args.kwargs["guest_exit_prequeued"])

    def test_native_e2e_measures_and_reuses_snapshot_capture(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            openvmm = root / "openvmm"
            executable = openvmm / "target" / "release" / "openvmm"
            build_dir = root / "build"
            executable.parent.mkdir(parents=True)
            build_dir.mkdir()
            (openvmm / "Cargo.toml").touch()
            executable.touch()
            (build_dir / "vmlinux").touch()
            (build_dir / "initramfs.cpio.gz").touch()
            output = root / "e2e.json"
            args = nvx.parse_args(
                [
                    "benchmark",
                    "--suite",
                    "e2e",
                    "--backend",
                    "kvm",
                    "--openvmm-dir",
                    str(openvmm),
                    "--nvx-dir",
                    str(root),
                    "--cpus",
                    "0-2",
                    "--warmups",
                    "1",
                    "--runs",
                    "1",
                    "--skip-build",
                    "--output",
                    str(output),
                ]
            )
            run_result: benchmark.BenchmarkResult = {
                "samples_ms": [10.0],
                "p50_ms": 10.0,
                "p95_ms": 10.0,
                "min_ms": 10.0,
                "max_ms": 10.0,
                "wall_samples_ms": [12.0],
                "wall_p50_ms": 12.0,
                "wall_p95_ms": 12.0,
                "wall_min_ms": 12.0,
                "wall_max_ms": 12.0,
                "peak_rss_samples_bytes": [1024],
                "peak_rss_p50_bytes": 1024,
                "peak_rss_min_bytes": 1024,
                "peak_rss_max_bytes": 1024,
                "peak_rss_remeasured_count": 0,
                "teardown_samples_ms": [2.0],
                "teardown_completed_samples_ms": [2.0],
                "teardown_timeout_count": 0,
                "teardown_timeout_seconds": 5.0,
                "teardown_p50_ms": 2.0,
                "teardown_p95_ms": 2.0,
                "teardown_min_ms": 2.0,
                "teardown_max_ms": 2.0,
            }
            capture_result: benchmark.SnapshotCaptureResult = {
                "samples_ms": [3.0],
                "p50_ms": 3.0,
                "p95_ms": 3.0,
                "min_ms": 3.0,
                "max_ms": 3.0,
                "request_to_publication_samples_ms": [3.0],
                "request_to_publication_p50_ms": 3.0,
                "request_to_publication_p95_ms": 3.0,
                "request_to_publication_min_ms": 3.0,
                "request_to_publication_max_ms": 3.0,
                "post_publication_exit_samples_ms": [1.0],
                "post_publication_exit_p50_ms": 1.0,
                "post_publication_exit_p95_ms": 1.0,
                "post_publication_exit_min_ms": 1.0,
                "post_publication_exit_max_ms": 1.0,
                "peak_rss_samples_bytes": [2048],
                "peak_rss_p50_bytes": 2048,
                "peak_rss_min_bytes": 2048,
                "peak_rss_max_bytes": 2048,
            }
            with (
                patch.object(benchmark, "benchmark", return_value=run_result),
                patch.object(
                    benchmark,
                    "benchmark_snapshot_capture",
                    return_value=capture_result,
                ) as capture,
                patch.object(
                    benchmark,
                    "benchmark_snapshot_restore",
                    return_value=run_result,
                ) as restore,
            ):
                self.assertEqual(benchmark.run_native_linux(args), 0)

            retained = capture.call_args.kwargs["retained_snapshot_path"]
            self.assertIsInstance(retained, Path)
            self.assertEqual(restore.call_args.kwargs["snapshot_path"], retained)
            document = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(
                document["snapshot_capture"]["kvm"]["peak_rss_p50_bytes"],
                2048,
            )

    def test_kvm_worker_e2e_restores_the_measured_snapshot(self):
        args = argparse.Namespace(
            _stage_dir="/tmp/stage",
            cpus="0",
            memory_mib=128,
            processors=4,
            net=None,
            suite="e2e",
            warmups=1,
            runs=1,
            timeout=1.0,
            teardown_mode="guest-exit",
        )
        run_result = cast(benchmark.BenchmarkResult, {"p50_ms": 10.0})
        capture_result = cast(benchmark.SnapshotCaptureResult, {"p50_ms": 3.0})
        with (
            patch.object(benchmark, "benchmark", return_value=run_result),
            patch.object(
                benchmark,
                "benchmark_snapshot_capture",
                return_value=capture_result,
            ) as capture,
            patch.object(
                benchmark,
                "benchmark_snapshot_restore",
                return_value=run_result,
            ) as restore,
            patch.object(benchmark, "print_summary"),
            patch.object(benchmark, "print_snapshot_summary"),
        ):
            self.assertEqual(benchmark.run_kvm_worker(args), 0)

        self.assertEqual(
            capture.call_args.args[1][-2:], ["--virtio-console", "console"]
        )
        retained = capture.call_args.kwargs["retained_snapshot_path"]
        self.assertIsInstance(retained, Path)
        self.assertEqual(restore.call_args.kwargs["snapshot_path"], retained)

    def test_virtfs_honors_host_termination_mode(self):
        guest_exit = benchmark._virtfs_script(1, "guest-exit")
        host_terminate = benchmark._virtfs_script(1, "host-terminate")
        roundtrip = benchmark._virtfs_roundtrip_script("host-terminate")

        self.assertIn("nvx-exit 0", guest_exit)
        self.assertNotIn("nvx-exit 0", host_terminate)
        self.assertNotIn("nvx-exit 0", roundtrip)

    def test_virtfs_exchange_uses_lf_only_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)

            def emulate_guest(
                *_args: object, **_kwargs: object
            ) -> benchmark.GuestCommandResult:
                (root / "guest-visible").write_bytes(benchmark.VIRTFS_GUEST_TO_HOST)
                deadline = benchmark.time.monotonic() + 1
                while benchmark.time.monotonic() < deadline:
                    if (
                        root / "host-visible"
                    ).read_bytes() == benchmark.VIRTFS_HOST_TO_GUEST:
                        break
                    benchmark.time.sleep(0.001)
                else:
                    self.fail("guest did not observe the byte-exact host update")
                return {"text": "", "wall_ms": 1.0, "peak_rss_bytes": 1}

            with patch.object(benchmark, "run_guest_script", side_effect=emulate_guest):
                benchmark._run_virtfs_roundtrip(
                    ["openvmm"],
                    root,
                    1,
                    1,
                    timeout=1,
                    windows_cpus=None,
                    teardown_mode="guest-exit",
                )

            self.assertEqual(
                (root / "host-visible").read_bytes(),
                b"host-to-guest\n",
            )

    def test_virtfs_excludes_declared_warmup(self):
        args = argparse.Namespace(
            virtfs_memory_mib=128,
            payload_mib=1,
            processors=1,
            warmups=1,
            timeout=1.0,
            teardown_mode="guest-exit",
        )
        io_result: benchmark.GuestCommandResult = {
            "text": "1 byte copied, 1 s, 1 MB/s\n1 byte copied, 1 s, 2 MB/s\n",
            "wall_ms": 1.0,
            "peak_rss_bytes": 1,
        }
        roundtrip_result: benchmark.GuestCommandResult = {
            "text": "",
            "wall_ms": 2.0,
            "peak_rss_bytes": 1,
        }
        with (
            patch.object(benchmark, "run_guest_script", return_value=io_result) as io,
            patch.object(
                benchmark, "_run_virtfs_roundtrip", return_value=roundtrip_result
            ) as roundtrip,
        ):
            benchmark.benchmark_virtfs_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
                runs=3,
            )

        self.assertEqual(io.call_count, 4)
        self.assertEqual(roundtrip.call_count, 4)

    def test_virtfs_lists_every_retained_sample(self):
        args = argparse.Namespace(
            virtfs_memory_mib=128,
            payload_mib=1,
            processors=1,
            warmups=1,
            timeout=1.0,
            teardown_mode="guest-exit",
        )
        # The warmup's rates and exchange time must not appear in the samples.
        io_results = [
            {
                "text": (
                    f"1 byte copied, 1 s, {write} MB/s\n"
                    f"1 byte copied, 1 s, {read} MB/s\n"
                ),
                "wall_ms": 1.0,
                "peak_rss_bytes": 1,
            }
            for write, read in ((99, 99), (11, 21), (12.5, 22), (13, 23))
        ]
        roundtrip_results = [
            {"text": "", "wall_ms": wall_ms, "peak_rss_bytes": 1}
            for wall_ms in (99.0, 2.5, 3.25, 4.125)
        ]
        with (
            patch.object(benchmark, "run_guest_script", side_effect=io_results),
            patch.object(
                benchmark, "_run_virtfs_roundtrip", side_effect=roundtrip_results
            ),
            patch("sys.stdout", new_callable=io.StringIO) as output,
        ):
            benchmark.benchmark_virtfs_workload(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initrd"),
                "kvm",
                runs=3,
            )

        lines = output.getvalue().splitlines()
        self.assertIn(
            "  write samples                     : 11.0, 12.5, 13.0 MB/s", lines
        )
        self.assertIn(
            "  read samples                      : 21.0, 22.0, 23.0 MB/s", lines
        )
        self.assertIn(
            "  exchange samples                  : 2.500, 3.250, 4.125 ms", lines
        )
        # The listed samples must leave the summaries that collection parses
        # intact.
        metrics = performance._parse_virtfs(output.getvalue())
        self.assertEqual(metrics["virtfs_live_write"], ("MB/s", "higher", 12.5))
        self.assertEqual(metrics["virtfs_live_read"], ("MB/s", "higher", 22.0))
        self.assertEqual(metrics["virtfs_live_roundtrip"], ("ms", "lower", 3.2))

    def test_shell_snapshot_summary_lists_restore_samples_in_order(self):
        restored = [85.6894, 74.955, 107.1931, 36.2756]
        with patch("sys.stdout", new_callable=io.StringIO) as output:
            benchmark._print_shell_snapshot_summary(256, [308.7, 309.1], restored)

        lines = output.getvalue().splitlines()
        restore = next(
            index
            for index, line in enumerate(lines)
            if line.startswith("  snapshot restore    : median")
        )
        self.assertEqual(
            lines[restore + 1],
            "  restore samples     : 85.689, 74.955, 107.193, 36.276 ms",
        )
        self.assertEqual(
            performance._parse_shell_snapshot_restore(output.getvalue()),
            {"shell_snapshot_restore_256_mib": ("ms", "lower", 80.3)},
        )

    def test_device_io_retains_failure_and_resumes_completed_attempts(self):
        args = argparse.Namespace(
            processors=1,
            warmups=1,
            runs=1,
            device_io_duration_seconds=1.0,
            device_io_size_mib=64,
            device_io_port=5201,
            net="192.0.2.2/24",
            timeout=1.0,
            teardown_mode="guest-exit",
        )

        def guest_result(command: list[str], *_args: object, **_kwargs: object):
            if "--net" in command:
                payload = (
                    'NVX_DEVICE_IO_GUEST_RESULT={"device":"network",'
                    '"operation":"roundtrip","operations":100,'
                    '"bytes_per_operation":64,"elapsed_ns":1000000000}\n'
                )
            else:
                payload = (
                    'NVX_DEVICE_IO_GUEST_RESULT={"device":"file",'
                    '"operation":"write","operations":100,'
                    '"bytes_per_operation":4096,"elapsed_ns":1000000000}\n'
                    'NVX_DEVICE_IO_GUEST_RESULT={"device":"file",'
                    '"operation":"read","operations":100,'
                    '"bytes_per_operation":4096,"elapsed_ns":1000000000}\n'
                )
            return {"text": payload, "wall_ms": 1.0, "peak_rss_bytes": 1}

        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "device-io.log"
            with (
                patch.object(benchmark, "host_ipv4_address", return_value="192.0.2.1"),
                patch.object(benchmark, "UdpEchoServer") as echo_server,
                patch.object(
                    benchmark,
                    "run_guest_script",
                    side_effect=guest_result,
                ) as run_guest,
            ):
                echo_server.return_value.__enter__.return_value = None
                benchmark.benchmark_device_io_workload(
                    args,
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initramfs"),
                    "mshv",
                    output_path=output,
                )
                self.assertEqual(run_guest.call_count, 6)
                # The echo server binds the address to which the guest sends.
                echo_server.assert_called_once_with("192.0.2.1", 5201)
                # A hung attempt describes the host of its scratch directory.
                host_state = run_guest.call_args.kwargs["host_state_on_timeout"]
                self.assertIs(host_state.func, benchmark._device_io_host_state)
                self.assertTrue(
                    host_state.args[0].name.startswith("openvmm-device-io-")
                )
                network_commands = [
                    call.args[0]
                    for call in run_guest.call_args_list
                    if "--net" in call.args[0]
                ]
                self.assertEqual(len(network_commands), 2)
                self.assertIn("192.0.2.2/24", network_commands[0])
                self.assertIn("microvm", network_commands[0])
                self.assertNotIn("microvm-v2", network_commands[0])
                block_commands = [
                    call.args[0]
                    for call in run_guest.call_args_list
                    if "--microvm-sandbox-block" in call.args[0]
                ]
                self.assertEqual(len(block_commands), 2)
                self.assertNotIn("--virtio-blk", block_commands[0])
                block_argument = block_commands[0][
                    block_commands[0].index("--microvm-sandbox-block") + 1
                ]
                self.assertTrue(block_argument.startswith("scratch:file:"))

                benchmark.benchmark_device_io_workload(
                    args,
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initramfs"),
                    "mshv",
                    output_path=output,
                )
                self.assertEqual(run_guest.call_count, 6)

            records = benchmark._read_device_io_records(output)
            self.assertEqual(len(records), 6)
            retained = [record for record in records if not record["warmup"]]
            self.assertEqual([record["sample_index"] for record in retained], [0, 0, 0])

    def test_device_io_udp_echo_server_echoes_at_its_address_alone(self):
        with benchmark.UdpEchoServer("127.0.0.1", 0) as server:
            address = server.socket.getsockname()
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as client:
                client.settimeout(5)
                client.sendto(b"NVX-DEVICE-IO-ECHO", address)
                self.assertEqual(client.recvfrom(64), (b"NVX-DEVICE-IO-ECHO", address))
        self.assertEqual(address[0], "127.0.0.1")

    def test_device_io_missing_guest_result_becomes_failure(self):
        result: benchmark.GuestCommandResult = {
            "text": (
                'NVX_DEVICE_IO_GUEST_RESULT={"device":"file",'
                '"operation":"read","operations":1,'
                '"bytes_per_operation":4096,"elapsed_ns":1}\n'
            ),
            "wall_ms": 1.0,
            "peak_rss_bytes": 1,
        }

        record = benchmark._device_io_attempt_record("virtio-blk", 0, 0, result, None)

        self.assertEqual(record["status"], "failure")
        self.assertIn("missing", str(record["error"]))

    def test_managed_tap_cleanup_uses_openvmm_pid(self):
        query: subprocess.CompletedProcess[str] = subprocess.CompletedProcess(
            ["ip"], 0, "", ""
        )
        removed: subprocess.CompletedProcess[str] = subprocess.CompletedProcess(
            ["sudo", "-n", "ip"], 0, "", ""
        )
        with (
            patch.object(benchmark.sys, "platform", "linux"),
            patch.object(benchmark.os, "geteuid", return_value=1000, create=True),
            patch.object(
                benchmark.subprocess, "run", side_effect=(query, removed)
            ) as run,
        ):
            benchmark.cleanup_managed_tap(1234)

        self.assertEqual(
            run.call_args_list[1].args[0],
            [
                "sudo",
                "-n",
                "ip",
                "tuntap",
                "del",
                "dev",
                "ovm1234",
                "mode",
                "tap",
            ],
        )

    def test_performance_suite_writes_four_canonical_logs(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            (output / "snapshot.log").write_text("stale", encoding="ascii")
            (output / "snapshot-hello.log").write_text("stale", encoding="ascii")
            args = nvx.parse_args(
                [
                    "benchmark",
                    "--suite",
                    "performance",
                    "--backend",
                    "whp",
                    "--platform",
                    "windows-whp-baremetal",
                    "--processors",
                    "8",
                    "--output-dir",
                    str(output),
                ]
            )
            with (
                patch.object(benchmark, "benchmark_cold_start_workload") as cold,
                patch.object(benchmark, "benchmark_virtfs_workload") as virtfs,
                patch.object(benchmark, "benchmark_shell_snapshot_workload") as shell,
                patch.object(
                    benchmark, "benchmark_network_snapshot_workload"
                ) as network,
            ):
                result = benchmark.run_workload_benchmarks(
                    args,
                    Path("openvmm.exe"),
                    Path("vmlinux"),
                    Path("initramfs.cpio.gz"),
                    "whp",
                )

            self.assertEqual(result, 0)
            self.assertEqual(
                {path.name for path in output.iterdir()},
                {
                    "benchmark-metadata.json",
                    "cold-start.log",
                    "virtfs.log",
                    "shell-snapshot.log",
                    "network.log",
                },
            )
            metadata = json.loads(
                (output / "benchmark-metadata.json").read_text(encoding="utf-8")
            )
            self.assertEqual(metadata["platform"], "windows-whp-baremetal")
            self.assertEqual(metadata["backend"], "whp")
            self.assertEqual(metadata["microvm_abi_version"], 3)
            self.assertEqual(metadata["processors"], 8)
            self.assertEqual(metadata["host_affinity_set"], args.cpus)
            self.assertEqual(metadata["host_cpu_reserve"], args.host_cpu_reserve)
            self.assertEqual(
                metadata["scratch_directory"],
                str(Path(tempfile.gettempdir()).resolve()),
            )
            cold.assert_called_once()
            self.assertEqual(virtfs.call_args.kwargs["runs"], 3)
            shell.assert_called_once()
            network.assert_called_once()

    def test_restore_only_suite_writes_only_its_canonical_log(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            (output / "cold-start.log").write_text("stale", encoding="ascii")
            (output / "shell-snapshot.log").write_text("stale", encoding="ascii")
            (output / "acceptance.json").write_text("stale", encoding="ascii")
            args = nvx.parse_args(
                [
                    "benchmark",
                    "--suite",
                    "shell-snapshot-restore",
                    "--backend",
                    "whp",
                    "--platform",
                    "windows-whp-virtual-machine",
                    "--processors",
                    "8",
                    "--shell-memories",
                    "512",
                    "--output-dir",
                    str(output),
                ]
            )
            with patch.object(
                benchmark, "benchmark_shell_snapshot_restore_workload"
            ) as restore:
                result = benchmark.run_workload_benchmarks(
                    args,
                    Path("openvmm.exe"),
                    Path("vmlinux"),
                    Path("initramfs.cpio.gz"),
                    "whp",
                )

            self.assertEqual(result, 0)
            self.assertEqual(
                {path.name for path in output.iterdir()},
                {"benchmark-metadata.json", "shell-snapshot-restore.log"},
            )
            restore.assert_called_once()

    def test_mshv_performance_suite_includes_network(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            args = nvx.parse_args(
                [
                    "benchmark",
                    "--suite",
                    "performance",
                    "--backend",
                    "mshv",
                    "--output-dir",
                    str(output),
                ]
            )
            with (
                patch.object(benchmark, "benchmark_cold_start_workload"),
                patch.object(benchmark, "benchmark_virtfs_workload"),
                patch.object(benchmark, "benchmark_shell_snapshot_workload"),
                patch.object(
                    benchmark, "benchmark_network_snapshot_workload"
                ) as network,
            ):
                benchmark.run_workload_benchmarks(
                    args,
                    Path("openvmm"),
                    Path("vmlinux"),
                    Path("initramfs.cpio.gz"),
                    "mshv",
                )

            self.assertEqual(
                {path.name for path in output.iterdir()},
                {
                    "benchmark-metadata.json",
                    "cold-start.log",
                    "virtfs.log",
                    "shell-snapshot.log",
                    "network.log",
                },
            )
            network.assert_called_once()

    def test_mshv_runs_explicit_network_snapshot_suite(self):
        args = nvx.parse_args(
            ["benchmark", "--suite", "network-snapshot", "--backend", "mshv"]
        )

        with patch.object(benchmark, "benchmark_network_snapshot_workload") as network:
            benchmark.run_workload_benchmarks(
                args,
                Path("openvmm"),
                Path("vmlinux"),
                Path("initramfs.cpio.gz"),
                "mshv",
            )

        network.assert_called_once()

    def test_performance_suite_defaults_to_data_runs_platform(self):
        with tempfile.TemporaryDirectory() as temporary:
            repository = Path(temporary)
            backend = "whp" if os.name == "nt" else "kvm"
            platform = "windows-whp" if os.name == "nt" else "linux-kvm"
            args = nvx.parse_args(
                [
                    "benchmark",
                    "--suite",
                    "performance",
                    "--backend",
                    backend,
                    "--nvx-dir",
                    str(repository),
                ]
            )
            with (
                patch.object(benchmark, "benchmark_cold_start_workload"),
                patch.object(benchmark, "benchmark_virtfs_workload"),
                patch.object(benchmark, "benchmark_shell_snapshot_workload"),
                patch.object(benchmark, "benchmark_network_snapshot_workload"),
            ):
                benchmark.run_workload_benchmarks(
                    args,
                    Path("openvmm.exe"),
                    Path("vmlinux"),
                    Path("initramfs.cpio.gz"),
                    backend,
                )

            output = (
                repository
                / "data"
                / "runs"
                / f"{platform}-{MICROVM_RUN_DIRECTORY}-1vcpu"
            )
            self.assertEqual(
                {path.name for path in output.iterdir()},
                {
                    "benchmark-metadata.json",
                    "cold-start.log",
                    "virtfs.log",
                    "shell-snapshot.log",
                    "network.log",
                },
            )

    def test_device_io_defaults_to_dedicated_abi_v2_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            repository = Path(temporary).resolve()
            backend = "whp" if os.name == "nt" else "kvm"
            platform = "windows-whp" if os.name == "nt" else "linux-kvm"
            args = nvx.parse_args(
                [
                    "benchmark",
                    "--suite",
                    "device-io",
                    "--backend",
                    backend,
                    "--nvx-dir",
                    str(repository),
                ]
            )
            with (
                patch.object(
                    benchmark,
                    "write_benchmark_metadata",
                    return_value=Path("benchmark-metadata.json"),
                ) as metadata,
                patch.object(
                    benchmark, "benchmark_device_io_workload", return_value=0
                ) as device_io,
            ):
                benchmark.run_workload_benchmarks(
                    args,
                    Path("openvmm.exe"),
                    Path("vmlinux"),
                    Path("initramfs.cpio.gz"),
                    backend,
                )

            output = (
                repository
                / "data"
                / "runs"
                / platform
                / MICROVM_RUN_DIRECTORY
                / "1vcpu"
                / "device-io"
            )
            self.assertEqual(
                device_io.call_args.kwargs["output_path"],
                output / "device-io.log",
            )
            self.assertEqual(metadata.call_args.args[1], output)

    def test_package_command_forwards_parsed_options(self):
        args = nvx.parse_args(
            [
                "package",
                "--version",
                "2.0.0",
                "--destination",
                "dist/custom",
                "--binary-only",
                "--force",
            ]
        )

        with patch.object(nvx, "package_release") as package_release:
            args.handler(args)

        package_release.assert_called_once_with(
            version="2.0.0",
            destination=Path("dist/custom"),
            include_source=False,
            force=True,
        )

    def test_archive_release_command_forwards_paths(self):
        args = nvx.parse_args(
            [
                "archive-release",
                "--source",
                "dist/package",
                "--destination",
                "dist/package.zip",
            ]
        )

        with patch.object(nvx, "create_release_archive") as create_archive:
            args.handler(args)

        create_archive.assert_called_once_with(
            Path("dist/package"),
            Path("dist/package.zip"),
        )


class CleanupAfterKillTests(unittest.TestCase):
    """A failure path keeps its error when OpenVMM outlives its kill."""

    class StuckInteraction:
        """An OpenVMM that printed ``console`` and outlives its kill, whose
        output reader, with ``reader_stuck``, doesn't stop either."""

        def __init__(self, console: bytes, reader_stuck: bool = False) -> None:
            self.process = MagicMock(pid=4321)
            self.process.poll.return_value = None
            self.console = console
            self.reader_stuck = reader_stuck
            self.closed = False
            self.closes = 0

        def read_output(self, chunks: queue.Queue[bytes | None]) -> None:
            chunks.put(self.console)

        def read_stderr(self, chunks: queue.Queue[bytes | None]) -> None:
            del chunks

        def write_input(self, data: bytes) -> None:
            raise AssertionError(f"unexpected input: {data!r}")

        def close(self) -> None:
            self.closed = True
            self.closes += 1
            if self.reader_stuck:
                raise RuntimeError("OpenVMM output reader did not stop")

    EXIT_TIMEOUT = "process 4321 did not exit within 30s of being killed"
    READER_STUCK = "RuntimeError: OpenVMM output reader did not stop"

    def outliving_kill(self) -> Any:
        def outlive(*_args: object, **_kwargs: object) -> None:
            raise benchmark.ProcessExitTimeout(self.EXIT_TIMEOUT)

        return patch.object(benchmark, "terminate", side_effect=outlive)

    def test_a_launch_failure_keeps_its_error_when_openvmm_outlives_the_kill(self):
        with (
            patch.object(
                benchmark,
                "InteractiveProcess",
                return_value=self.StuckInteraction(b"booting\n"),
            ),
            patch.object(benchmark, "OUTPUT_DRAIN_TIMEOUT_SECONDS", 0.05),
            self.outliving_kill() as terminate,
            self.assertRaises(RuntimeError) as raised,
        ):
            benchmark.measure_once(["openvmm"], environment={}, timeout=0.2)

        terminate.assert_called_once()
        self.assertEqual(
            str(raised.exception),
            "guest marker was not observed within 0.2s\n"
            "--- OpenVMM output ---\nbooting\n\n"
            f"--- cleanup failures ---\n- ProcessExitTimeout: {self.EXIT_TIMEOUT}",
        )
        self.assertIsInstance(raised.exception.__cause__, TimeoutError)

    def test_a_guest_failure_report_survives_an_openvmm_that_outlives_the_kill(self):
        with (
            patch.object(
                benchmark,
                "InteractiveProcess",
                return_value=self.StuckInteraction(b"NVX-TEST-FAIL broken\n"),
            ),
            patch.object(benchmark, "OUTPUT_DRAIN_TIMEOUT_SECONDS", 0.05),
            self.outliving_kill() as terminate,
            self.assertRaises(benchmark.GuestFailureReported) as reported,
        ):
            benchmark.measure_once(
                ["openvmm"],
                environment={},
                timeout=5,
                failure_marker=b"NVX-TEST-FAIL",
            )

        terminate.assert_called_once()
        self.assertEqual(reported.exception.line, "NVX-TEST-FAIL broken")
        self.assertTrue(
            str(reported.exception).endswith(
                f"\n--- cleanup failures ---\n- ProcessExitTimeout: {self.EXIT_TIMEOUT}"
            ),
            str(reported.exception),
        )

    def test_a_teardown_timeout_kills_an_openvmm_that_outlives_the_kill_once(self):
        with (
            patch.object(
                benchmark,
                "InteractiveProcess",
                return_value=self.StuckInteraction(benchmark.BOOT_MARKER + b"\n"),
            ),
            patch.object(benchmark, "OUTPUT_DRAIN_TIMEOUT_SECONDS", 0.05),
            patch.object(benchmark, "live_peak_rss_bytes", return_value=1024),
            patch.object(
                benchmark,
                "wait_for_process_exit",
                side_effect=subprocess.TimeoutExpired("openvmm", 15),
            ),
            self.outliving_kill() as terminate,
            self.assertRaises(RuntimeError) as raised,
        ):
            benchmark.measure_once(
                ["openvmm"], environment={}, timeout=5, guest_exit_prequeued=True
            )

        terminate.assert_called_once()
        self.assertTrue(
            str(raised.exception).startswith(
                f"{self.EXIT_TIMEOUT}\n--- OpenVMM output ---\n"
            ),
            str(raised.exception),
        )

    def test_cleanup_runs_every_step_and_keeps_the_error_that_it_follows(self):
        calls: list[str] = []

        def step(name: str, error: Exception | None = None) -> Callable[[], None]:
            def run() -> None:
                calls.append(name)
                if error is not None:
                    raise error

            return run

        failure = ValueError("scenario failed")
        benchmark.append_cleanup_failures(
            failure, [benchmark.ProcessExitTimeout(self.EXIT_TIMEOUT)]
        )
        benchmark.finish_cleanup(
            failure,
            step("log", OSError("disk full")),
            None,
            step("close", RuntimeError("OpenVMM output reader did not stop")),
        )
        self.assertEqual(calls, ["log", "close"])
        # The failures join the kill's under one heading.
        self.assertEqual(
            str(failure),
            "scenario failed\n--- cleanup failures ---\n"
            f"- ProcessExitTimeout: {self.EXIT_TIMEOUT}\n- OSError: disk full\n"
            f"- {self.READER_STUCK}",
        )
        # Without an error to follow, the first failure surfaces with the others.
        with self.assertRaises(OSError) as raised:
            benchmark.finish_cleanup(
                None,
                step("log", OSError("disk full")),
                step("close", RuntimeError("OpenVMM output reader did not stop")),
            )
        self.assertEqual(
            str(raised.exception),
            f"disk full\n--- cleanup failures ---\n- {self.READER_STUCK}",
        )
        benchmark.finish_cleanup(None, step("close"))

    def test_cleanup_failures_reach_an_error_whose_message_has_an_errno(self):
        # An OSError with an errno takes its message from its errno, not its
        # arguments, so its failures go into an OSError that it caused.
        def full() -> OSError:
            return OSError(errno.ENOSPC, "No space left on device", "source.log")

        def fail(error: BaseException) -> Callable[[], None]:
            def run() -> None:
                raise error

            return run

        original = full()
        combined = benchmark.append_cleanup_failures(
            original, [RuntimeError("OpenVMM output reader did not stop")]
        )
        expected = f"{full()}\n--- cleanup failures ---\n- {self.READER_STUCK}"
        self.assertIs(type(combined), OSError)
        self.assertIs(combined.__cause__, original)
        self.assertEqual(str(combined), expected)
        self.assertEqual(str(original), str(full()))
        # A cleanup whose first failure is such an error raises the combined
        # one, and so does a cleanup that follows such an error.
        reader = RuntimeError("OpenVMM output reader did not stop")
        with self.assertRaises(OSError) as raised:
            benchmark.finish_cleanup(None, fail(full()), fail(reader))
        self.assertEqual(str(raised.exception), expected)
        with self.assertRaises(OSError) as raised:
            try:
                raise full()
            except OSError as failure:
                reader = RuntimeError("OpenVMM output reader did not stop")
                benchmark.finish_cleanup(failure, fail(reader))
                raise
        self.assertEqual(str(raised.exception), expected)
        self.assertIsInstance(raised.exception.__cause__, OSError)

    def test_a_launch_failure_keeps_its_error_when_the_reader_does_not_stop_either(
        self,
    ):
        with (
            patch.object(
                benchmark,
                "InteractiveProcess",
                return_value=self.StuckInteraction(b"booting\n", reader_stuck=True),
            ),
            patch.object(benchmark, "OUTPUT_DRAIN_TIMEOUT_SECONDS", 0.05),
            self.outliving_kill() as terminate,
            self.assertRaises(RuntimeError) as raised,
        ):
            benchmark.measure_once(["openvmm"], environment={}, timeout=0.2)

        terminate.assert_called_once()
        self.assertEqual(
            str(raised.exception),
            "guest marker was not observed within 0.2s\n"
            "--- OpenVMM output ---\nbooting\n\n"
            f"--- cleanup failures ---\n- ProcessExitTimeout: {self.EXIT_TIMEOUT}\n"
            f"- {self.READER_STUCK}",
        )
        self.assertIsInstance(raised.exception.__cause__, TimeoutError)

    def test_a_snapshot_capture_keeps_its_error_and_log_past_a_stuck_reader(self):
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "source.log"
            with (
                patch.object(
                    benchmark,
                    "InteractiveProcess",
                    return_value=self.StuckInteraction(b"booting\n", reader_stuck=True),
                ),
                self.outliving_kill() as terminate,
                self.assertRaises(RuntimeError) as raised,
            ):
                benchmark.capture_device_restore_snapshot(
                    ["openvmm"],
                    Path(temporary) / "snapshot",
                    "blk",
                    "cold",
                    timeout=0.2,
                    log_path=log_path,
                )

            terminate.assert_called_once()
            # The log is written before the interaction's shutdown fails.
            self.assertEqual(log_path.read_bytes(), b"booting\n")
        self.assertEqual(
            str(raised.exception),
            "blk/cold source snapshot did not finish within 0.2s\n"
            "--- OpenVMM output ---\nbooting\n\n"
            f"--- cleanup failures ---\n- ProcessExitTimeout: {self.EXIT_TIMEOUT}\n"
            f"- {self.READER_STUCK}",
        )
        self.assertIsInstance(raised.exception.__cause__, TimeoutError)

    def test_an_openvmm_process_closes_and_keeps_its_error_past_a_failed_kill(self):
        interaction = self.StuckInteraction(b"booting\n")
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "process.log"
            with (
                patch.object(
                    openvmm_process, "InteractiveProcess", return_value=interaction
                ),
                self.outliving_kill() as terminate,
                self.assertRaises(RuntimeError) as raised,
                openvmm_process.OpenvmmProcess(["openvmm"], log_path) as process,
            ):
                process.wait_for(b"NEVER", 0.2)

            terminate.assert_called_once()
            self.assertTrue(interaction.closed)
            self.assertEqual(log_path.read_bytes(), b"booting\n")
        self.assertEqual(
            str(raised.exception),
            "marker b'NEVER' was not observed within 0.2s\n"
            "--- OpenVMM output ---\nbooting\n\n"
            f"--- cleanup failures ---\n- ProcessExitTimeout: {self.EXIT_TIMEOUT}",
        )

    def test_an_openvmm_process_reports_a_failed_kill_after_its_block(self):
        interaction = self.StuckInteraction(b"booting\n")
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "process.log"
            with (
                patch.object(
                    openvmm_process, "InteractiveProcess", return_value=interaction
                ),
                self.outliving_kill(),
            ):
                # A block that leaves OpenVMM running fails only on the kill.
                with self.assertRaisesRegex(
                    benchmark.ProcessExitTimeout, f"^{re.escape(self.EXIT_TIMEOUT)}$"
                ):
                    with openvmm_process.OpenvmmProcess(["openvmm"], log_path):
                        pass
                # The error that leaves a block stays the one that surfaces.
                with self.assertRaises(ValueError) as raised:
                    with openvmm_process.OpenvmmProcess(["openvmm"], log_path):
                        raise ValueError("scenario failed")
        self.assertEqual(
            str(raised.exception),
            "scenario failed\n--- cleanup failures ---\n"
            f"- ProcessExitTimeout: {self.EXIT_TIMEOUT}",
        )

    def test_an_openvmm_process_keeps_its_error_past_a_failed_kill_and_shutdown(self):
        # An OpenVMM that outlives its kill can keep its output open, so its
        # reader doesn't stop either, and neither failure hides the other.
        interaction = self.StuckInteraction(b"booting\n", reader_stuck=True)
        with tempfile.TemporaryDirectory() as temporary:
            log_path = Path(temporary) / "process.log"
            with (
                patch.object(
                    openvmm_process, "InteractiveProcess", return_value=interaction
                ),
                self.outliving_kill() as terminate,
                self.assertRaises(RuntimeError) as raised,
                openvmm_process.OpenvmmProcess(["openvmm"], log_path) as process,
            ):
                process.wait_for(b"NEVER", 0.2)

            terminate.assert_called_once()
            # The failure path releases the process, and the block's exit
            # doesn't release it again.
            self.assertEqual(interaction.closes, 1)
            self.assertEqual(log_path.read_bytes(), b"booting\n")
        self.assertEqual(
            str(raised.exception),
            "marker b'NEVER' was not observed within 0.2s\n"
            "--- OpenVMM output ---\nbooting\n\n"
            f"--- cleanup failures ---\n- ProcessExitTimeout: {self.EXIT_TIMEOUT}\n"
            f"- {self.READER_STUCK}",
        )

    def test_an_openvmm_process_reports_a_failed_kill_and_shutdown_after_its_block(
        self,
    ):
        def stuck(*_arguments: object) -> CleanupAfterKillTests.StuckInteraction:
            return self.StuckInteraction(b"", reader_stuck=True)

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(openvmm_process, "InteractiveProcess", side_effect=stuck),
            self.outliving_kill(),
        ):
            log_path = Path(temporary) / "process.log"
            with self.assertRaises(benchmark.ProcessExitTimeout) as closed:
                with openvmm_process.OpenvmmProcess(["openvmm"], log_path):
                    pass
            with self.assertRaises(ValueError) as raised:
                with openvmm_process.OpenvmmProcess(["openvmm"], log_path):
                    raise ValueError("scenario failed")
        self.assertEqual(
            str(closed.exception),
            f"{self.EXIT_TIMEOUT}\n--- cleanup failures ---\n- {self.READER_STUCK}",
        )
        self.assertEqual(
            str(raised.exception),
            "scenario failed\n--- cleanup failures ---\n"
            f"- ProcessExitTimeout: {self.EXIT_TIMEOUT}\n- {self.READER_STUCK}",
        )

    def test_an_openvmm_process_reports_its_cleanup_after_an_errno_error(self):
        def stuck(*_arguments: object) -> CleanupAfterKillTests.StuckInteraction:
            return self.StuckInteraction(b"", reader_stuck=True)

        full = OSError(errno.ENOSPC, "No space left on device", "source.log")
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(openvmm_process, "InteractiveProcess", side_effect=stuck),
            self.outliving_kill(),
            self.assertRaises(OSError) as raised,
            openvmm_process.OpenvmmProcess(["openvmm"], Path(temporary) / "log"),
        ):
            raise full
        # The block's error can't show the failures, so the error that it
        # caused, which does, surfaces in its place.
        self.assertIs(raised.exception.__cause__, full)
        self.assertEqual(
            str(raised.exception),
            f"{full}\n--- cleanup failures ---\n"
            f"- ProcessExitTimeout: {self.EXIT_TIMEOUT}\n- {self.READER_STUCK}",
        )

    def test_an_openvmm_process_keeps_its_error_past_a_failed_transcript_close(
        self,
    ):
        class FullLog(io.BytesIO):
            """A transcript whose close fails to flush it."""

            def close(self) -> None:
                super().close()
                raise OSError(errno.ENOSPC, "No space left on device", "log")

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(
                openvmm_process,
                "InteractiveProcess",
                return_value=self.StuckInteraction(b"booting\n"),
            ),
            self.outliving_kill(),
            self.assertRaises(ValueError) as raised,
            openvmm_process.OpenvmmProcess(
                ["openvmm"], Path(temporary) / "log"
            ) as process,
        ):
            process._log.close()
            cast(Any, process)._log = FullLog()
            raise ValueError("scenario failed")
        full = OSError(errno.ENOSPC, "No space left on device", "log")
        self.assertEqual(
            str(raised.exception),
            "scenario failed\n--- cleanup failures ---\n"
            f"- ProcessExitTimeout: {self.EXIT_TIMEOUT}\n- OSError: {full}",
        )

    def test_a_failed_openvmm_launch_keeps_its_error_past_a_failed_kill_and_shutdown(
        self,
    ):
        interaction = self.StuckInteraction(b"", reader_stuck=True)
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(
                openvmm_process, "InteractiveProcess", return_value=interaction
            ),
            patch.object(
                openvmm_process.time, "sleep", side_effect=RuntimeError("interrupted")
            ),
            self.outliving_kill() as terminate,
            self.assertRaises(RuntimeError) as raised,
        ):
            log_path = Path(temporary) / "process.log"
            openvmm_process.OpenvmmProcess(["openvmm"], log_path, output_read_delay=1)

        terminate.assert_called_once()
        self.assertEqual(interaction.closes, 1)
        self.assertEqual(
            str(raised.exception),
            "interrupted\n--- cleanup failures ---\n"
            f"- ProcessExitTimeout: {self.EXIT_TIMEOUT}\n- {self.READER_STUCK}",
        )


class PositiveFloatTests(unittest.TestCase):
    def test_accepts_positive_finite_float(self):
        self.assertEqual(benchmark.positive_float("1.5"), 1.5)

    def test_rejects_nonpositive_and_nonfinite_values(self):
        for value in ("0", "-1", "nan", "inf"):
            with self.subTest(value=value):
                with self.assertRaisesRegex(
                    argparse.ArgumentTypeError,
                    "^must be finite and greater than 0$",
                ):
                    benchmark.positive_float(value)


if __name__ == "__main__":
    unittest.main()
