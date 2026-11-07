#!/usr/bin/env python3
"""Select and optionally run the local equivalents of NVX's CI quality gates.

The tool reads each command from the composite actions and workflow jobs that
own it, combines those commands with the repository's CI change classifier,
and explains why a change selected each gate.  It can print the resulting plan
or execute it without duplicating CI's scripts in this module.
"""

from __future__ import annotations

import argparse
import fnmatch
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal, Protocol, TextIO, cast

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))
from nvx_tools.ci import (  # noqa: E402
    OPENVMM_CI_CHECK_INPUT_PATHS,
    classify_ci_changes,
)
from nvx_tools.microvm_tests import MICROVM_TEST_SCENARIOS  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_BASE = "origin/dev"
OperatingSystem = Literal["linux", "windows"]
BOTH: tuple[OperatingSystem, ...] = ("linux", "windows")
LINUX: tuple[OperatingSystem, ...] = ("linux",)
WINDOWS: tuple[OperatingSystem, ...] = ("windows",)

CHECK_QUALITY = ".github/actions/check-quality/action.yml"
VALIDATE_NVX = ".github/actions/validate-nvx/action.yml"
CHECK_NVX_RS = ".github/actions/check-nvx-rs/action.yml"
CI_WORKFLOW = ".github/workflows/ci.yml"
ACTION_FILES = (CHECK_QUALITY, VALIDATE_NVX, CHECK_NVX_RS)
WORKFLOW_JOBS = ((CI_WORKFLOW, "nvx-cli-linux"), (CI_WORKFLOW, "nvx-cli-windows"))

ALWAYS = ("<always>",)
QUALITY_INPUTS = ("<quality-files>",)
KERNEL_INPUTS = (
    "kernel/",
    "scripts/nvx_tools/kconfig.py",
    "scripts/nvx_tools/kernel_config.py",
    "scripts/nvx_tools/kernel_features.py",
)
NVX_RS_INPUTS = (
    "nvx-rs/",
    "tests/runtime-adapter/",
    "rust-toolchain.toml",
    "scripts/public_api.py",
    "scripts/nvx_tools/guest_binaries.py",
    CHECK_NVX_RS,
)
OPENVMM_COMPARISON_INPUTS = (
    "openvmm",
    "openvmm/",
    "nvx-rs/tests/vectors/",
    "nvx-rs/tests/openvmm_*.rs",
    "nvx-rs/src/host/cli/cpu_generations.json",
    "nvx-rs/src/contract/",
    "scripts/nvx_tools/time_abi.py",
    CI_WORKFLOW,
)


class LocalGatesError(RuntimeError):
    """A gate definition or local prerequisite cannot be used."""


class CommandRunner(Protocol):
    """The subprocess interface used by discovery and execution."""

    def run(
        self,
        args: Sequence[str],
        *,
        cwd: Path,
        env: Mapping[str, str],
        capture_output: bool,
        stream: TextIO | None = None,
    ) -> subprocess.CompletedProcess[str]: ...


class SubprocessRunner:
    """Run commands through subprocess, capturing or streaming their output."""

    def run(
        self,
        args: Sequence[str],
        *,
        cwd: Path,
        env: Mapping[str, str],
        capture_output: bool,
        stream: TextIO | None = None,
    ) -> subprocess.CompletedProcess[str]:
        if capture_output:
            return subprocess.run(
                list(args),
                cwd=cwd,
                env=dict(env),
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                check=False,
            )
        return subprocess.run(
            list(args),
            cwd=cwd,
            env=dict(env),
            stdout=stream,
            stderr=stream,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )


ToolFinder = Callable[[str, str | None], str | None]


def find_tool(name: str, path: str | None = None) -> str | None:
    """Find a command on path."""
    return shutil.which(name, path=path)


@dataclass(frozen=True)
class DefinitionSet:
    """The action files and workflow jobs whose scripted steps must be mapped."""

    action_files: tuple[str, ...] = ACTION_FILES
    workflow_jobs: tuple[tuple[str, str], ...] = WORKFLOW_JOBS


DEFAULT_DEFINITIONS = DefinitionSet()


@dataclass(frozen=True)
class GateRule:
    """Map one authoritative step to a local gate or skipped CI setup."""

    gate_id: str
    source_file: str
    step: str
    triggers: tuple[str, ...]
    operating_systems: tuple[OperatingSystem, ...] = BOTH
    job: str | None = None
    environment_job: str | None = None
    requirements: tuple[str, ...] = ()
    setup: bool = False

    @property
    def key(self) -> tuple[str, str | None, str]:
        return (self.source_file, self.job, self.step)


PYTHON_REQUIREMENT = ("Python and the repository development dependencies",)
RUST_REQUIREMENT = (
    "bash, Cargo, and the pinned and minimum-supported Rust toolchains",
    "CI setup step 'Install Rust toolchains' is skipped locally",
)

GATE_RULES = (
    GateRule(
        "ci-setup-development-tools",
        CHECK_QUALITY,
        "Install development tools",
        ALWAYS,
        requirements=("Python and pip",),
        setup=True,
    ),
    GateRule(
        "quality-code",
        CHECK_QUALITY,
        "Check code quality",
        QUALITY_INPUTS,
        requirements=(
            "Python and the repository development dependencies",
            "bash",
            "Docker, or local shellcheck and shfmt; missing tools are skipped",
        ),
    ),
    GateRule(
        "markdown-links",
        CHECK_QUALITY,
        "Check Markdown links",
        ALWAYS,
        requirements=PYTHON_REQUIREMENT,
    ),
    GateRule(
        "test-layout",
        CHECK_QUALITY,
        "Check test layout",
        ALWAYS,
        requirements=PYTHON_REQUIREMENT,
    ),
    GateRule(
        "kernel-config",
        CHECK_QUALITY,
        "Check the kernel configuration against its feature inventory",
        KERNEL_INPUTS,
        requirements=PYTHON_REQUIREMENT,
    ),
    GateRule(
        "specula-wiring",
        CHECK_QUALITY,
        "Test Specula CI wiring",
        (".github/specula/",),
        requirements=("Python on a POSIX host (the tests import fcntl)", "bash"),
    ),
    GateRule(
        "powershell-syntax",
        CHECK_QUALITY,
        "Check PowerShell syntax",
        ("*.ps1",),
        requirements=("PowerShell",),
    ),
    GateRule(
        "nvx-cli",
        VALIDATE_NVX,
        "Validate NVX CLI on Linux",
        ALWAYS,
        LINUX,
        environment_job="nvx-cli-linux",
        requirements=(*PYTHON_REQUIREMENT, "initialized OpenVMM submodule"),
    ),
    GateRule(
        "nvx-cli",
        VALIDATE_NVX,
        "Validate NVX CLI on Windows",
        ALWAYS,
        WINDOWS,
        environment_job="nvx-cli-windows",
        requirements=(*PYTHON_REQUIREMENT, "initialized OpenVMM submodule"),
    ),
    GateRule(
        "ci-setup-rust-toolchains",
        CHECK_NVX_RS,
        "Install Rust toolchains",
        NVX_RS_INPUTS,
        requirements=("rustup",),
        setup=True,
    ),
    GateRule(
        "nvx-rs-format",
        CHECK_NVX_RS,
        "Check formatting",
        NVX_RS_INPUTS,
        requirements=RUST_REQUIREMENT,
    ),
    GateRule(
        "nvx-rs-lint",
        CHECK_NVX_RS,
        "Lint",
        NVX_RS_INPUTS,
        requirements=RUST_REQUIREMENT,
    ),
    GateRule(
        "nvx-rs-test",
        CHECK_NVX_RS,
        "Test",
        NVX_RS_INPUTS,
        requirements=RUST_REQUIREMENT,
    ),
    GateRule(
        "nvx-rs-bundling",
        CHECK_NVX_RS,
        "Check artifact bundling",
        NVX_RS_INPUTS,
        requirements=RUST_REQUIREMENT,
    ),
    GateRule(
        "nvx-rs-doc",
        CHECK_NVX_RS,
        "Build documentation",
        NVX_RS_INPUTS,
        requirements=RUST_REQUIREMENT,
    ),
    GateRule(
        "nvx-rs-public-api",
        CHECK_NVX_RS,
        "Check the public API",
        NVX_RS_INPUTS,
        LINUX,
        requirements=RUST_REQUIREMENT,
    ),
    GateRule(
        "nvx-rs-msrv",
        CHECK_NVX_RS,
        "Check the minimum supported Rust version",
        NVX_RS_INPUTS,
        requirements=RUST_REQUIREMENT,
    ),
    GateRule(
        "nvx-rs-macos",
        CHECK_NVX_RS,
        "Check the macOS build",
        NVX_RS_INPUTS,
        LINUX,
        requirements=RUST_REQUIREMENT,
    ),
    GateRule(
        "nvx-rs-guest-binaries-lint",
        CHECK_NVX_RS,
        "Lint and test the guest binaries",
        NVX_RS_INPUTS,
        LINUX,
        requirements=RUST_REQUIREMENT,
    ),
    GateRule(
        "nvx-rs-guest-binaries-build",
        CHECK_NVX_RS,
        "Build and check the guest binaries",
        NVX_RS_INPUTS,
        LINUX,
        requirements=RUST_REQUIREMENT,
    ),
    GateRule(
        "nvx-rs-adapter-protocol",
        CHECK_NVX_RS,
        "Check the runtime adapter's protocol-only configuration",
        NVX_RS_INPUTS,
        requirements=RUST_REQUIREMENT,
    ),
    GateRule(
        "nvx-rs-adapter-host",
        CHECK_NVX_RS,
        "Check the runtime adapter's host-only configuration",
        NVX_RS_INPUTS,
        requirements=RUST_REQUIREMENT,
    ),
    GateRule(
        "nvx-rs-adapter-guest",
        CHECK_NVX_RS,
        "Check the runtime adapter's guest-only configuration",
        NVX_RS_INPUTS,
        LINUX,
        requirements=RUST_REQUIREMENT,
    ),
    GateRule(
        "ci-setup-rust-linux",
        CI_WORKFLOW,
        "Install the pinned Rust release",
        ALWAYS,
        LINUX,
        job="nvx-cli-linux",
        requirements=("rustup",),
        setup=True,
    ),
    GateRule(
        "ci-setup-nvx-rs-executable",
        CI_WORKFLOW,
        "Make nvx-rs executable",
        ALWAYS,
        LINUX,
        job="nvx-cli-linux",
        setup=True,
    ),
    GateRule(
        "openvmm-vectors",
        CI_WORKFLOW,
        "Compare OpenVMM's protocol vectors",
        OPENVMM_COMPARISON_INPUTS,
        LINUX,
        job="nvx-cli-linux",
        requirements=(
            "Cargo and the pinned Rust toolchain",
            "initialized OpenVMM submodule",
        ),
    ),
    GateRule(
        "openvmm-cpu-profiles",
        CI_WORKFLOW,
        "Compare OpenVMM's CPU profiles",
        OPENVMM_COMPARISON_INPUTS,
        LINUX,
        job="nvx-cli-linux",
        requirements=(
            "Cargo and the pinned Rust toolchain",
            "initialized OpenVMM submodule",
        ),
    ),
    GateRule(
        "ci-setup-rust-windows",
        CI_WORKFLOW,
        "Install the pinned Rust release",
        ALWAYS,
        WINDOWS,
        job="nvx-cli-windows",
        requirements=("rustup",),
        setup=True,
    ),
    GateRule(
        "openvmm-vectors",
        CI_WORKFLOW,
        "Compare OpenVMM's protocol vectors",
        OPENVMM_COMPARISON_INPUTS,
        WINDOWS,
        job="nvx-cli-windows",
        requirements=(
            "Cargo and the pinned Rust toolchain",
            "initialized OpenVMM submodule",
        ),
    ),
    GateRule(
        "openvmm-cpu-profiles",
        CI_WORKFLOW,
        "Compare OpenVMM's CPU profiles",
        OPENVMM_COMPARISON_INPUTS,
        WINDOWS,
        job="nvx-cli-windows",
        requirements=(
            "Cargo and the pinned Rust toolchain",
            "initialized OpenVMM submodule",
        ),
    ),
)

# These setup steps matter to CI but are intentionally not local gates.
CI_SETUP_STEPS = frozenset(
    {
        (CHECK_QUALITY, None, "Set up Python"),
        (CHECK_QUALITY, None, "Install development tools"),
        (CHECK_NVX_RS, None, "Install Rust toolchains"),
        (CI_WORKFLOW, "nvx-cli-linux", "Checkout"),
        (CI_WORKFLOW, "nvx-cli-linux", "Checkout OpenVMM"),
        (CI_WORKFLOW, "nvx-cli-linux", "Set up Python"),
        (CI_WORKFLOW, "nvx-cli-linux", "Install the pinned Rust release"),
        (CI_WORKFLOW, "nvx-cli-linux", "Download nvx-rs executable"),
        (CI_WORKFLOW, "nvx-cli-linux", "Download nvx-rs provenance"),
        (CI_WORKFLOW, "nvx-cli-linux", "Make nvx-rs executable"),
        (CI_WORKFLOW, "nvx-cli-windows", "Checkout"),
        (CI_WORKFLOW, "nvx-cli-windows", "Checkout OpenVMM"),
        (CI_WORKFLOW, "nvx-cli-windows", "Set up Python"),
        (CI_WORKFLOW, "nvx-cli-windows", "Install the pinned Rust release"),
        (CI_WORKFLOW, "nvx-cli-windows", "Download nvx-rs executable"),
        (CI_WORKFLOW, "nvx-cli-windows", "Download nvx-rs provenance"),
    }
)

GATE_ORDER = (
    "test-changes",
    "markdown-links",
    "test-layout",
    "powershell-syntax",
    "kernel-config",
    "quality-code",
    "specula-wiring",
    "openvmm-vectors",
    "openvmm-cpu-profiles",
    "nvx-rs-format",
    "nvx-rs-lint",
    "nvx-rs-test",
    "nvx-rs-bundling",
    "nvx-rs-doc",
    "nvx-rs-public-api",
    "nvx-rs-msrv",
    "nvx-rs-macos",
    "nvx-rs-guest-binaries-lint",
    "nvx-rs-guest-binaries-build",
    "nvx-rs-adapter-protocol",
    "nvx-rs-adapter-host",
    "nvx-rs-adapter-guest",
    "nvx-cli",
    "openvmm-ci",
)
LOCAL_GATE_IDS = frozenset(GATE_ORDER)


@dataclass(frozen=True)
class LoadedStep:
    """A run step loaded from an action or workflow."""

    source_file: str
    job: str | None
    name: str
    shell: str
    working_directory: str
    environment: dict[str, str]
    script: str
    condition: str | None
    input_defaults: dict[str, str]
    action_directory: Path


@dataclass(frozen=True)
class GateSource:
    """The CI definition from which a gate was loaded."""

    file: str
    job: str | None
    step: str

    def as_json(self) -> dict[str, str | None]:
        return {"file": self.file, "job": self.job, "step": self.step}


@dataclass
class Gate:
    """One selected, fully substituted local gate."""

    gate_id: str
    selected_by: tuple[str, ...]
    source: GateSource
    operating_system: OperatingSystem
    shell: str
    working_directory: str
    environment: dict[str, str]
    declared_environment: frozenset[str]
    script: str
    requirements: tuple[str, ...]
    state: str = "selected"
    detail: str = ""
    skipped: list[str] = field(default_factory=list[str])
    adaptations: list[str] = field(default_factory=list[str])
    duration_seconds: float | None = None

    @property
    def status(self) -> str:
        if self.detail:
            return f"{self.state} ({self.detail})"
        return self.state

    def block(self, reason: str) -> None:
        self.state = "blocked"
        self.detail = reason

    def as_json(self) -> dict[str, object]:
        return {
            "id": self.gate_id,
            "selected_by": list(self.selected_by),
            "source": self.source.as_json(),
            "os": self.operating_system,
            "shell": self.shell,
            "working_directory": self.working_directory,
            "env": dict(sorted(self.environment.items())),
            "script": self.script,
            "requirements": list(self.requirements),
            "status": self.status,
        }


@dataclass(frozen=True)
class CiSuites:
    """The workload suites selected by the repository's CI classifier."""

    run_microvm_tests: bool
    run_benchmarks: bool
    run_openvmm_ci_check: bool
    backends: tuple[str, ...]
    scenarios: tuple[str, ...]
    commands: tuple[str, ...]

    def as_json(self) -> dict[str, object]:
        return {
            "run_microvm_tests": self.run_microvm_tests,
            "run_benchmarks": self.run_benchmarks,
            "run_openvmm_ci_check": self.run_openvmm_ci_check,
            "microvm": {
                "backends": list(self.backends),
                "scenarios": list(self.scenarios),
                "commands": list(self.commands),
                "local_or_ci": (
                    "run on a host with the selected backend, or leave to CI"
                    if self.run_microvm_tests
                    else "not selected"
                ),
            },
            "benchmarks": {
                "ci_runs": self.run_benchmarks,
                "local_collection": "use the nvx-collect-benchmarks skill",
            },
        }


@dataclass
class Report:
    """A local gate plan and the CI suites associated with its files."""

    base: str
    changed_files: tuple[str, ...]
    gates: list[Gate]
    ci_suites: CiSuites

    def as_json(self) -> dict[str, object]:
        return {
            "base": self.base,
            "changed_files": list(self.changed_files),
            "gates": [gate.as_json() for gate in self.gates],
            "ci_suites": self.ci_suites.as_json(),
        }


class DefinitionLoader:
    """Load and validate YAML definitions below an injectable repository root."""

    def __init__(self, root: Path) -> None:
        self.root = root
        self._documents: dict[str, Mapping[object, object]] = {}

    def document(self, relative: str) -> Mapping[object, object]:
        cached = self._documents.get(relative)
        if cached is not None:
            return cached
        path = self.root / relative
        try:
            loaded = cast(object, yaml.safe_load(path.read_text(encoding="utf-8")))
        except (OSError, yaml.YAMLError) as error:
            raise LocalGatesError(f"cannot read {relative}: {error}") from error
        document = _mapping(loaded, relative)
        self._documents[relative] = document
        return document

    def action_steps(self, relative: str) -> list[Mapping[object, object]]:
        document = self.document(relative)
        runs = _mapping(document.get("runs"), f"{relative}: runs")
        return _mapping_sequence(runs.get("steps"), f"{relative}: runs.steps")

    def workflow_job(self, relative: str, job_name: str) -> Mapping[object, object]:
        document = self.document(relative)
        jobs = _mapping(document.get("jobs"), f"{relative}: jobs")
        return _mapping(jobs.get(job_name), f"{relative}: job {job_name}")

    def workflow_steps(
        self, relative: str, job_name: str
    ) -> list[Mapping[object, object]]:
        job = self.workflow_job(relative, job_name)
        return _mapping_sequence(job.get("steps"), f"{relative}: job {job_name}.steps")

    def load_step(self, rule: GateRule) -> LoadedStep:
        if rule.job is None:
            document = self.document(rule.source_file)
            steps = self.action_steps(rule.source_file)
            inputs = _input_defaults(document.get("inputs"), rule.source_file)
            job_environment: dict[str, str] = {}
            if rule.environment_job is not None:
                job = self.workflow_job(CI_WORKFLOW, rule.environment_job)
                job_environment = _environment(
                    job.get("env"), f"{CI_WORKFLOW}: job {rule.environment_job}.env"
                )
        else:
            steps = self.workflow_steps(rule.source_file, rule.job)
            inputs = {}
            job = self.workflow_job(rule.source_file, rule.job)
            job_environment = _environment(
                job.get("env"), f"{rule.source_file}: job {rule.job}.env"
            )
        step = _named_step(steps, rule.step, rule.source_file, rule.job)
        script = _required_string(step.get("run"), f"{rule.step}: run")
        shell = _required_string(step.get("shell"), f"{rule.step}: shell")
        environment = dict(job_environment)
        environment.update(_environment(step.get("env"), f"{rule.step}: env"))
        condition_value = step.get("if")
        condition = None
        if condition_value is not None:
            condition = _required_string(condition_value, f"{rule.step}: if")
        working_directory_value = step.get("working-directory", ".")
        working_directory = _required_string(
            working_directory_value, f"{rule.step}: working-directory"
        )
        return LoadedStep(
            source_file=rule.source_file,
            job=rule.job or rule.environment_job,
            name=rule.step,
            shell=shell,
            working_directory=working_directory,
            environment=environment,
            script=script,
            condition=condition,
            input_defaults=inputs,
            action_directory=(self.root / rule.source_file).parent,
        )


def _mapping(value: object, context: str) -> Mapping[object, object]:
    if not isinstance(value, dict):
        raise LocalGatesError(f"{context} must be a mapping")
    return cast(dict[object, object], value)


def _mapping_sequence(value: object, context: str) -> list[Mapping[object, object]]:
    if not isinstance(value, list):
        raise LocalGatesError(f"{context} must be a sequence")
    result: list[Mapping[object, object]] = []
    for index, item in enumerate(cast(list[object], value)):
        result.append(_mapping(item, f"{context}[{index}]"))
    return result


def _required_string(value: object, context: str) -> str:
    if not isinstance(value, str):
        raise LocalGatesError(f"{context} must be a string")
    return value


def _environment(value: object, context: str) -> dict[str, str]:
    if value is None:
        return {}
    mapping = _mapping(value, context)
    result: dict[str, str] = {}
    for key, raw_value in mapping.items():
        if not isinstance(key, str) or not isinstance(raw_value, (str, int, float)):
            raise LocalGatesError(f"{context} must contain scalar string values")
        result[key] = str(raw_value)
    return result


def _input_defaults(value: object, context: str) -> dict[str, str]:
    if value is None:
        return {}
    inputs = _mapping(value, f"{context}: inputs")
    result: dict[str, str] = {}
    for name, raw_input in inputs.items():
        if not isinstance(name, str):
            raise LocalGatesError(f"{context}: input names must be strings")
        declaration = _mapping(raw_input, f"{context}: input {name}")
        default = declaration.get("default", "")
        if not isinstance(default, (str, int, float, bool)):
            raise LocalGatesError(f"{context}: input {name} has a non-scalar default")
        result[name] = (
            str(default).lower() if isinstance(default, bool) else str(default)
        )
    return result


def _named_step(
    steps: Sequence[Mapping[object, object]],
    name: str,
    source_file: str,
    job: str | None,
) -> Mapping[object, object]:
    matches = [step for step in steps if step.get("name") == name]
    if len(matches) != 1:
        place = f" job {job}" if job is not None else ""
        raise LocalGatesError(
            f"{source_file}{place} must contain exactly one step named {name!r}"
        )
    return matches[0]


def _step_operating_systems(
    condition: str | None, job: str | None
) -> tuple[OperatingSystem, ...]:
    if job == "nvx-cli-linux":
        return LINUX
    if job == "nvx-cli-windows":
        return WINDOWS
    if condition is None:
        return BOTH
    normalized = " ".join(condition.strip().split())
    if normalized == "runner.os == 'Linux'":
        return LINUX
    if normalized in ("runner.os != 'Windows'", "runner.os == 'Linux'"):
        return LINUX
    if normalized == "runner.os == 'Windows'":
        return WINDOWS
    raise LocalGatesError(f"unsupported step OS condition: {condition}")


def scripted_step_keys(
    root: Path = REPO_ROOT, definitions: DefinitionSet = DEFAULT_DEFINITIONS
) -> set[tuple[str, str | None, str]]:
    """Return every run step that the gate table must cover."""
    loader = DefinitionLoader(root)
    keys: set[tuple[str, str | None, str]] = set()
    for relative in definitions.action_files:
        for step in loader.action_steps(relative):
            if "run" in step:
                name = _required_string(step.get("name"), f"{relative}: step name")
                keys.add((relative, None, name))
    for relative, job in definitions.workflow_jobs:
        for step in loader.workflow_steps(relative, job):
            if "run" in step:
                name = _required_string(step.get("name"), f"{relative}: step name")
                keys.add((relative, job, name))
    return keys


def validate_gate_table(
    root: Path = REPO_ROOT,
    rules: Sequence[GateRule] = GATE_RULES,
    definitions: DefinitionSet = DEFAULT_DEFINITIONS,
) -> None:
    """Fail when an authoritative scripted step is absent from the table."""
    actual = scripted_step_keys(root, definitions)
    mapped = {rule.key for rule in rules}
    missing = sorted(actual - mapped)
    stale = sorted(mapped - actual)
    if missing or stale:
        details: list[str] = []
        if missing:
            details.append(f"unmapped steps: {missing}")
        if stale:
            details.append(f"steps no longer present: {stale}")
        raise LocalGatesError("; ".join(details))
    loader = DefinitionLoader(root)
    for rule in rules:
        loaded = loader.load_step(rule)
        actual_os = _step_operating_systems(loaded.condition, rule.job)
        if actual_os != rule.operating_systems:
            raise LocalGatesError(
                f"{rule.gate_id} maps {rule.operating_systems}, but its step selects "
                f"{actual_os}"
            )


_EXPRESSION = re.compile(r"\$\{\{\s*(.*?)\s*\}\}", re.DOTALL)
_REPOSITORY_FILE = re.compile(r"(?:\.github|guest|scripts)/[A-Za-z0-9_./-]+")


def _pinned_toolchain(root: Path, default: str) -> str:
    path = root / "rust-toolchain.toml"
    if path.is_file():
        match = re.search(
            r'^\s*channel\s*=\s*"([^"]+)"',
            path.read_text(encoding="utf-8"),
            re.MULTILINE,
        )
        if match is not None:
            return match.group(1)
    if default:
        return default
    raise LocalGatesError(
        "rust-toolchain.toml has no channel and the action input has no default"
    )


def _minimum_rust_version(root: Path) -> str:
    path = root / "nvx-rs" / "Cargo.toml"
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise LocalGatesError(f"cannot read {path}: {error}") from error
    match = re.search(r'^\s*rust-version\s*=\s*"([^"]+)"', text, re.MULTILINE)
    if match is None:
        raise LocalGatesError("nvx-rs/Cargo.toml has no rust-version")
    return match.group(1)


def substitute_ci_expressions(
    text: str,
    *,
    root: Path,
    runner_temp: Path,
    operating_system: OperatingSystem,
    input_defaults: Mapping[str, str] | None = None,
) -> tuple[str, tuple[str, ...]]:
    """Substitute the supported Actions expressions and return unknown ones."""
    defaults = dict(input_defaults or {})
    toolchain: str | None = None
    msrv: str | None = None
    unknown: list[str] = []

    def replacement(match: re.Match[str]) -> str:
        nonlocal toolchain, msrv
        expression = match.group(1).strip()
        values = {
            "github.workspace": str(root.resolve()),
            "runner.temp": str(runner_temp.resolve()),
            "runner.os": "Linux" if operating_system == "linux" else "Windows",
        }
        if expression in values:
            return values[expression]
        if expression == "steps.toolchains.outputs.toolchain":
            if toolchain is None:
                toolchain = _pinned_toolchain(root, defaults.get("toolchain", ""))
            return toolchain
        if expression == "steps.toolchains.outputs.msrv":
            if msrv is None:
                msrv = _minimum_rust_version(root)
            return msrv
        input_match = re.fullmatch(r"inputs\.([A-Za-z_][A-Za-z0-9_-]*)", expression)
        if input_match is not None and input_match.group(1) in defaults:
            return defaults[input_match.group(1)]
        if expression not in unknown:
            unknown.append(expression)
        return match.group(0)

    return _EXPRESSION.sub(replacement, text), tuple(unknown)


def _normalize_file(path: str | Path, root: Path) -> str:
    value = Path(path)
    if value.is_absolute():
        try:
            value = value.resolve().relative_to(root.resolve())
        except ValueError:
            pass
    normalized = value.as_posix().replace("\\", "/")
    while normalized.startswith("./"):
        normalized = normalized[2:]
    return normalized.rstrip("/")


def _quality_file_patterns(step_script: str, root: Path) -> tuple[str, ...]:
    patterns = [
        "scripts/*.py",
        ".github/specula/*.py",
        "pyproject.toml",
        "requirements-dev.txt",
        CHECK_QUALITY,
    ]
    for match in _REPOSITORY_FILE.finditer(step_script):
        candidate = match.group(0).rstrip("\\")
        if (root / candidate).is_file() and candidate not in patterns:
            patterns.append(candidate)
    return tuple(patterns)


def _matches_pattern(path: str, pattern: str) -> bool:
    if pattern.endswith("/"):
        return path.startswith(pattern)
    if pattern.startswith("*."):
        return path.lower().endswith(pattern[1:].lower())
    if any(character in pattern for character in "*?["):
        return fnmatch.fnmatchcase(path, pattern)
    return path == pattern


def _matching_files(
    files: Sequence[str], rule: GateRule, loaded: LoadedStep, root: Path
) -> tuple[str, ...]:
    if rule.triggers == ALWAYS:
        return ("always",)
    patterns: list[str] = []
    for trigger in rule.triggers:
        if trigger == "<quality-files>":
            patterns.extend(_quality_file_patterns(loaded.script, root))
        else:
            patterns.append(trigger)
    return tuple(
        path for path in files if any(_matches_pattern(path, item) for item in patterns)
    )


def _required_environment(
    gate_id: str,
    step: LoadedStep,
    root: Path,
    runner_temp: Path,
    operating_system: OperatingSystem,
) -> dict[str, str]:
    runner_temp.mkdir(parents=True, exist_ok=True)
    files = {
        "GITHUB_OUTPUT": runner_temp / f"{gate_id}-output.txt",
        "GITHUB_ENV": runner_temp / f"{gate_id}-env.txt",
        "GITHUB_STEP_SUMMARY": runner_temp / f"{gate_id}-summary.md",
    }
    for path in files.values():
        path.touch()
    environment = {
        "RUNNER_OS": "Linux" if operating_system == "linux" else "Windows",
        "RUNNER_TEMP": str(runner_temp.resolve()),
        "GITHUB_WORKSPACE": str(root.resolve()),
        "GITHUB_ACTION_PATH": str(step.action_directory.resolve()),
    }
    environment.update({name: str(path.resolve()) for name, path in files.items()})
    return environment


def _load_gate(
    rule: GateRule,
    loaded: LoadedStep,
    selected_by: tuple[str, ...],
    root: Path,
    runner_temp: Path,
    operating_system: OperatingSystem,
) -> Gate:
    script, unknown = substitute_ci_expressions(
        loaded.script,
        root=root,
        runner_temp=runner_temp,
        operating_system=operating_system,
        input_defaults=loaded.input_defaults,
    )
    environment = _required_environment(
        rule.gate_id, loaded, root, runner_temp, operating_system
    )
    all_unknown = list(unknown)
    for name, value in loaded.environment.items():
        substituted, environment_unknown = substitute_ci_expressions(
            value,
            root=root,
            runner_temp=runner_temp,
            operating_system=operating_system,
            input_defaults=loaded.input_defaults,
        )
        environment[name] = substituted
        for expression in environment_unknown:
            if expression not in all_unknown:
                all_unknown.append(expression)
    working_directory, directory_unknown = substitute_ci_expressions(
        loaded.working_directory,
        root=root,
        runner_temp=runner_temp,
        operating_system=operating_system,
        input_defaults=loaded.input_defaults,
    )
    shell, shell_unknown = substitute_ci_expressions(
        loaded.shell,
        root=root,
        runner_temp=runner_temp,
        operating_system=operating_system,
        input_defaults=loaded.input_defaults,
    )
    for expression in (*directory_unknown, *shell_unknown):
        if expression not in all_unknown:
            all_unknown.append(expression)
    gate = Gate(
        gate_id=rule.gate_id,
        selected_by=selected_by,
        source=GateSource(rule.source_file, loaded.job, loaded.name),
        operating_system=operating_system,
        shell=shell,
        working_directory=working_directory,
        environment=environment,
        declared_environment=frozenset(loaded.environment),
        script=script,
        requirements=rule.requirements,
    )
    if all_unknown:
        gate.block(f"needs CI context: {', '.join(all_unknown)}")
    if gate.gate_id == "specula-wiring" and operating_system == "windows":
        gate.block("Test Specula CI wiring imports POSIX fcntl")
    return gate


def _shell_quote(value: str, operating_system: OperatingSystem) -> str:
    if operating_system == "windows":
        return "'" + value.replace("'", "''") + "'"
    if re.fullmatch(r"[A-Za-z0-9_./:@+-]+", value):
        return value
    return "'" + value.replace("'", "'\"'\"'") + "'"


def _synthetic_step(
    *,
    root: Path,
    script_file: str,
    step_name: str,
    shell: str,
    script: str,
    source_job: str | None = None,
) -> LoadedStep:
    return LoadedStep(
        source_file=script_file,
        job=source_job,
        name=step_name,
        shell=shell,
        working_directory=".",
        environment={},
        script=script,
        condition=None,
        input_defaults={},
        action_directory=(root / script_file).parent,
    )


def _synthetic_gate(
    *,
    gate_id: str,
    selected_by: tuple[str, ...],
    source_file: str,
    step_name: str,
    command: str,
    requirements: tuple[str, ...],
    source_job: str | None = None,
    root: Path,
    runner_temp: Path,
    operating_system: OperatingSystem,
) -> Gate:
    shell = "powershell" if operating_system == "windows" else "bash"
    loaded = _synthetic_step(
        root=root,
        script_file=source_file,
        step_name=step_name,
        shell=shell,
        script=command,
        source_job=source_job,
    )
    rule = GateRule(
        gate_id,
        source_file,
        step_name,
        ALWAYS,
        (operating_system,),
        requirements=requirements,
    )
    return _load_gate(rule, loaded, selected_by, root, runner_temp, operating_system)


def _ci_suites(files: Sequence[str], operating_system: OperatingSystem) -> CiSuites:
    changes = classify_ci_changes("pull_request", files)
    if not changes.run_microvm_tests:
        return CiSuites(
            False,
            changes.run_benchmarks,
            changes.run_openvmm_ci_check,
            (),
            (),
            (),
        )
    backends = ("kvm", "mshv") if operating_system == "linux" else ("whp",)
    scenarios = MICROVM_TEST_SCENARIOS
    python = "python3" if operating_system == "linux" else "python"
    commands = tuple(
        f"{python} scripts/nvx.py test-microvm --backend {backend} --scenario {scenario}"
        for backend in backends
        for scenario in scenarios
    )
    return CiSuites(
        True,
        changes.run_benchmarks,
        changes.run_openvmm_ci_check,
        backends,
        scenarios,
        commands,
    )


def create_report(
    *,
    base: str,
    files: Sequence[str | Path],
    operating_system: OperatingSystem,
    root: Path = REPO_ROOT,
    runner_temp: Path,
    only: Sequence[str] | None = None,
    rules: Sequence[GateRule] = GATE_RULES,
    definitions: DefinitionSet = DEFAULT_DEFINITIONS,
) -> Report:
    """Build a gate report from explicit files and injectable definitions."""
    validate_gate_table(root, rules, definitions)
    requested = set(only or ())
    custom_gate_ids = {rule.gate_id for rule in rules if not rule.setup}
    unknown_ids = sorted(requested - LOCAL_GATE_IDS - custom_gate_ids)
    if unknown_ids:
        raise LocalGatesError(f"unknown gate id: {', '.join(unknown_ids)}")
    normalized = tuple(
        sorted({_normalize_file(path, root) for path in files if str(path)})
    )
    loader = DefinitionLoader(root)
    selected: dict[str, Gate] = {}
    for rule in rules:
        if rule.setup or operating_system not in rule.operating_systems:
            continue
        if requested and rule.gate_id not in requested:
            continue
        loaded = loader.load_step(rule)
        matches = _matching_files(normalized, rule, loaded, root)
        if not matches:
            continue
        if rule.gate_id in selected:
            raise LocalGatesError(
                f"more than one {operating_system} step maps to {rule.gate_id}"
            )
        selected[rule.gate_id] = _load_gate(
            rule, loaded, matches, root, runner_temp, operating_system
        )

    if not requested or "test-changes" in requested:
        python = "python3" if operating_system == "linux" else "python"
        quoted_base = _shell_quote(base, operating_system)
        selected["test-changes"] = _synthetic_gate(
            gate_id="test-changes",
            selected_by=("always",),
            source_file="scripts/check_test_changes.py",
            step_name="List changes to existing tests",
            command=f"{python} scripts/check_test_changes.py --base {quoted_base}",
            requirements=PYTHON_REQUIREMENT,
            root=root,
            runner_temp=runner_temp,
            operating_system=operating_system,
        )

    suites = _ci_suites(normalized, operating_system)
    if suites.run_openvmm_ci_check and (not requested or "openvmm-ci" in requested):
        python = "python3" if operating_system == "linux" else "python"
        selected_by = tuple(
            path for path in normalized if OPENVMM_CI_CHECK_INPUT_PATHS.match(path)
        )
        selected["openvmm-ci"] = _synthetic_gate(
            gate_id="openvmm-ci",
            selected_by=selected_by,
            source_file=CI_WORKFLOW,
            source_job="openvmm-ci",
            step_name="Check OpenVMM's CI of the pinned revision",
            command=f"{python} scripts/nvx.py check-openvmm-ci",
            requirements=("Python, gh authentication, and network access to GitHub",),
            root=root,
            runner_temp=runner_temp,
            operating_system=operating_system,
        )

    order = {gate_id: index for index, gate_id in enumerate(GATE_ORDER)}
    gates = sorted(selected.values(), key=lambda gate: order.get(gate.gate_id, 999))
    return Report(base, normalized, gates, suites)


def _command_output(result: subprocess.CompletedProcess[str], command: str) -> str:
    if result.returncode != 0:
        detail = (result.stderr or result.stdout or "").strip()
        suffix = f": {detail}" if detail else ""
        raise LocalGatesError(
            f"{command} failed with exit code {result.returncode}{suffix}"
        )
    return (result.stdout or "").strip()


def detect_changed_files(
    base: str,
    *,
    root: Path = REPO_ROOT,
    runner: CommandRunner | None = None,
) -> tuple[str, ...]:
    """Detect committed, staged, unstaged, and untracked paths from merge base.

    The `openvmm` submodule counts as changed when its gitlink, which the index
    holds, differs, as CI sees it, but not when only its checkout does."""
    command_runner = runner or SubprocessRunner()
    environment = os.environ.copy()
    merge_result = command_runner.run(
        ("git", "merge-base", base, "HEAD"),
        cwd=root,
        env=environment,
        capture_output=True,
    )
    merge_base = _command_output(merge_result, f"git merge-base {base} HEAD")
    if not merge_base:
        raise LocalGatesError(f"git merge-base {base} HEAD returned no commit")
    diff_result = command_runner.run(
        (
            "git",
            "diff",
            "--name-only",
            "--ignore-submodules=all",
            merge_base,
            "--",
        ),
        cwd=root,
        env=environment,
        capture_output=True,
    )
    tracked = _command_output(diff_result, "git diff --name-only").splitlines()
    index_result = command_runner.run(
        ("git", "diff", "--cached", "--name-only", merge_base, "--"),
        cwd=root,
        env=environment,
        capture_output=True,
    )
    indexed = _command_output(
        index_result, "git diff --cached --name-only"
    ).splitlines()
    untracked_result = command_runner.run(
        ("git", "ls-files", "--others", "--exclude-standard"),
        cwd=root,
        env=environment,
        capture_output=True,
    )
    untracked = _command_output(untracked_result, "git ls-files --others").splitlines()
    return tuple(
        sorted(
            {
                _normalize_file(path, root)
                for path in (*tracked, *indexed, *untracked)
                if path.strip()
            }
        )
    )


def _git_root(git: str) -> Path | None:
    executable = Path(git).resolve()
    parent = executable.parent
    if parent.name.lower() in {"cmd", "bin"}:
        if parent.parent.name.lower() == "mingw64":
            return parent.parent.parent
        return parent.parent
    return None


def git_for_windows_root(
    tool_finder: ToolFinder = find_tool, path: str | None = None
) -> Path | None:
    """Return the Git for Windows installation root of git on PATH."""
    git = tool_finder("git.exe", path) or tool_finder("git", path)
    if git is None:
        return None
    root = _git_root(git)
    if root is None:
        return None
    candidates = (root / "bin" / "bash.exe", root / "usr" / "bin" / "bash.exe")
    if not any(candidate.is_file() for candidate in candidates):
        return None
    return root


def _prepend_path(environment: dict[str, str], directory: Path) -> None:
    current = environment.get("PATH", "")
    environment["PATH"] = str(directory) + (os.pathsep + current if current else "")


def prepare_execution_environment(
    gate: Gate,
    inherited: Mapping[str, str],
    *,
    git_root: Path | None = None,
) -> dict[str, str]:
    """Apply the CI environment and the gate-specific inheritance rules."""
    environment = dict(inherited)
    environment.update(gate.environment)
    # CI's CLI jobs name a built nvx-rs. Naming a missing one fails the tests
    # that run nvx-rs instead of skipping them, so keep the caller's choice or
    # leave the variable unset.
    executable = gate.environment.get("NVX_RS")
    if executable is not None and not Path(executable).is_file():
        if "NVX_RS" in inherited:
            environment["NVX_RS"] = inherited["NVX_RS"]
        else:
            environment.pop("NVX_RS", None)
    if "NVX_BUNDLE" not in gate.declared_environment:
        environment.pop("NVX_BUNDLE", None)
    if os.name == "nt" or gate.operating_system == "windows":
        if gate.operating_system == "windows" and gate.gate_id == "nvx-cli":
            environment.pop("XDG_CACHE_HOME", None)
        if git_root is not None:
            _prepend_path(environment, git_root / "usr" / "bin")
    return environment


def _git_bash_path(path: Path) -> str:
    resolved = str(path.resolve()).replace("\\", "/")
    match = re.fullmatch(r"([A-Za-z]):/(.*)", resolved)
    if match is None:
        return resolved
    return f"/{match.group(1).lower()}/{match.group(2)}"


def _single_quote_shell(value: str) -> str:
    return "'" + value.replace("'", "'\"'\"'") + "'"


def _create_python3_shim(directory: Path) -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    shim = directory / "python3"
    executable = _git_bash_path(Path(sys.executable))
    shim.write_text(
        "#!/usr/bin/env sh\nexec " + _single_quote_shell(executable) + ' "$@"\n',
        encoding="utf-8",
        newline="\n",
    )
    shim.chmod(0o755)
    return shim


def _create_tool_stub(directory: Path, tool: str) -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    stub = directory / tool
    stub.write_text(
        f"#!/usr/bin/env sh\nprintf '%s\\n' 'NVX-GATE-SKIPPED: {tool}'\nexit 0\n",
        encoding="utf-8",
        newline="\n",
    )
    stub.chmod(0o755)
    return stub


def _docker_usable(
    root: Path, environment: Mapping[str, str], runner: CommandRunner
) -> bool:
    try:
        result = runner.run(
            ("docker", "info"),
            cwd=root,
            env=environment,
            capture_output=True,
        )
    except OSError:
        return False
    return result.returncode == 0


def _adapt_quality_tools(
    gate: Gate,
    *,
    root: Path,
    runner_temp: Path,
    environment: dict[str, str],
    runner: CommandRunner,
    tool_finder: ToolFinder,
) -> None:
    path = environment.get("PATH")
    docker = tool_finder("docker", path)
    if docker is not None and _docker_usable(root, environment, runner):
        return
    stub_directory = runner_temp / "quality-tool-stubs"
    for tool, variable in (("shellcheck", "SHELLCHECK"), ("shfmt", "SHFMT")):
        if tool_finder(tool, path) is not None:
            environment[variable] = tool
            gate.adaptations.append(
                f"using local {tool}; its version may differ from CI's pinned image"
            )
            continue
        _create_tool_stub(stub_directory, tool)
        environment[variable] = tool
        gate.skipped.append(tool)
    if gate.skipped:
        _prepend_path(environment, stub_directory)


def _windows_bash(root: Path | None) -> Path | None:
    if root is None:
        return None
    for candidate in (root / "bin" / "bash.exe", root / "usr" / "bin" / "bash.exe"):
        if candidate.is_file():
            return candidate
    return None


def _shell_command(
    gate: Gate,
    script: Path,
    *,
    environment: Mapping[str, str],
    git_root: Path | None,
    tool_finder: ToolFinder,
) -> tuple[list[str] | None, str | None]:
    path = environment.get("PATH")
    if gate.shell == "bash":
        if os.name == "nt" or gate.operating_system == "windows":
            bash = _windows_bash(git_root)
            if bash is None:
                return None, "Git for Windows bash is unavailable"
            executable = str(bash)
        else:
            executable = tool_finder("bash", path) or ""
            if not executable:
                return None, "bash is unavailable"
        return [
            executable,
            "--noprofile",
            "--norc",
            "-eo",
            "pipefail",
            str(script),
        ], None
    if gate.shell == "powershell":
        executable = tool_finder("powershell", path)
        if executable is None:
            return None, "Windows PowerShell is unavailable"
        command = ". '" + str(script).replace("'", "''") + "'"
        return [
            executable,
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            command,
        ], None
    if gate.shell == "pwsh":
        executable = tool_finder("pwsh", path)
        if executable is not None:
            command = ". '" + str(script).replace("'", "''") + "'"
            return [
                executable,
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                command,
            ], None
        if os.name == "nt" or gate.operating_system == "windows":
            fallback = tool_finder("powershell", path)
            if fallback is not None:
                command = ". '" + str(script).replace("'", "''") + "'"
                gate.adaptations.append("pwsh missing; using Windows PowerShell parser")
                return [
                    fallback,
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    command,
                ], None
        return None, "pwsh is unavailable"
    return None, f"unsupported shell {gate.shell!r}"


def _warn_missing_nvx_rs(gate: Gate, stream: TextIO) -> None:
    if gate.gate_id != "nvx-cli":
        return
    executable = gate.environment.get("NVX_RS")
    if executable is None or Path(executable).is_file():
        return
    message = (
        "NVX_RS is left unset because nvx-rs is not built, so the tests that run "
        "nvx-rs skip; build it with cargo build --release --locked --features cli "
        "--manifest-path nvx-rs/Cargo.toml"
    )
    gate.adaptations.append(message)
    print(f"warning: {message}", file=stream)


def _print_adaptations(gate: Gate, stream: TextIO) -> None:
    for adaptation in gate.adaptations:
        print(f"adaptation: {adaptation}", file=stream)


def execute_report(
    report: Report,
    *,
    root: Path = REPO_ROOT,
    runner_temp: Path,
    runner: CommandRunner | None = None,
    tool_finder: ToolFinder = find_tool,
    keep_going: bool = False,
    stream: TextIO = sys.stdout,
    inherited_environment: Mapping[str, str] | None = None,
) -> int:
    """Execute selected gates in order and return the documented exit code."""
    command_runner = runner or SubprocessRunner()
    inherited = dict(inherited_environment or os.environ)
    git_root = (
        git_for_windows_root(tool_finder, inherited.get("PATH"))
        if os.name == "nt"
        or any(gate.operating_system == "windows" for gate in report.gates)
        else None
    )
    stopped = False
    for gate in report.gates:
        if stopped:
            gate.state = "not run"
            gate.detail = "stopped after the first failure"
            continue
        print(f"\n==> {gate.gate_id}", file=stream, flush=True)
        if gate.state == "blocked":
            print(f"blocked: {gate.detail}", file=stream, flush=True)
            continue
        working_directory = (root / gate.working_directory).resolve()
        if not working_directory.is_dir():
            gate.block(f"working directory does not exist: {gate.working_directory}")
            print(f"blocked: {gate.detail}", file=stream, flush=True)
            continue
        environment = prepare_execution_environment(gate, inherited, git_root=git_root)
        if gate.gate_id == "quality-code":
            _adapt_quality_tools(
                gate,
                root=root,
                runner_temp=runner_temp,
                environment=environment,
                runner=command_runner,
                tool_finder=tool_finder,
            )
        if (
            os.name == "nt" or gate.operating_system == "windows"
        ) and gate.shell == "bash":
            python3 = tool_finder("python3", environment.get("PATH"))
            if python3 is None or "windowsapps" in python3.lower():
                shim_directory = runner_temp / "python3-shim"
                _create_python3_shim(shim_directory)
                _prepend_path(environment, shim_directory)
                gate.adaptations.append(
                    f"python3 shim invokes {sys.executable} because python3 is unavailable"
                )
        suffix = ".ps1" if gate.shell in {"powershell", "pwsh"} else ".sh"
        script = runner_temp / f"{gate.gate_id}{suffix}"
        script.write_text(gate.script + "\n", encoding="utf-8", newline="\n")
        command, blocked = _shell_command(
            gate,
            script,
            environment=environment,
            git_root=git_root,
            tool_finder=tool_finder,
        )
        if blocked is not None or command is None:
            gate.block(blocked or "the shell command could not be built")
            print(f"blocked: {gate.detail}", file=stream, flush=True)
            continue
        _warn_missing_nvx_rs(gate, stream)
        _print_adaptations(gate, stream)
        started = time.monotonic()
        try:
            result = command_runner.run(
                command,
                cwd=working_directory,
                env=environment,
                capture_output=False,
                stream=stream,
            )
        except OSError as error:
            gate.block(str(error))
            print(f"blocked: {gate.detail}", file=stream, flush=True)
            continue
        gate.duration_seconds = time.monotonic() - started
        if result.returncode != 0:
            gate.state = "failed"
            gate.detail = f"exit code {result.returncode}"
            if not keep_going:
                stopped = True
        elif gate.skipped:
            gate.state = "partial"
            gate.detail = "skipped " + ", ".join(gate.skipped)
        else:
            gate.state = "passed"
            gate.detail = ""
        print(
            f"{gate.status} in {gate.duration_seconds:.2f}s",
            file=stream,
            flush=True,
        )

    print("\nGate results:", file=stream)
    width = max((len(gate.gate_id) for gate in report.gates), default=4)
    for gate in report.gates:
        print(f"  {gate.gate_id:<{width}}  {gate.status}", file=stream)
    if any(gate.state == "failed" for gate in report.gates):
        return 1
    if any(gate.state == "blocked" for gate in report.gates):
        return 2
    return 0


def _selection_reason(selected_by: Sequence[str]) -> str:
    if selected_by == ("always",) or list(selected_by) == ["always"]:
        return "always runs"
    shown = ", ".join(selected_by[:3])
    if len(selected_by) > 3:
        shown += f", and {len(selected_by) - 3} more"
    return shown


def _source_text(source: GateSource) -> str:
    parts = [source.file]
    if source.job is not None:
        parts.append(f"job {source.job}")
    parts.append(f"step {source.step}")
    return ", ".join(parts)


def print_report(report: Report, stream: TextIO = sys.stdout) -> None:
    """Print the human-readable gate plan."""
    print(f"Base: {report.base}", file=stream)
    print(f"Changed files: {len(report.changed_files)}", file=stream)
    for gate in report.gates:
        print(f"\nGate: {gate.gate_id}", file=stream)
        print(f"  Selected by: {_selection_reason(gate.selected_by)}", file=stream)
        print(f"  Source: {_source_text(gate.source)}", file=stream)
        print(f"  OS: {gate.operating_system}", file=stream)
        print(f"  Shell: {gate.shell}", file=stream)
        print(f"  Working directory: {gate.working_directory}", file=stream)
        print("  Environment:", file=stream)
        for name, value in sorted(gate.environment.items()):
            print(f"    {name}={value}", file=stream)
        print("  Requirements:", file=stream)
        for requirement in gate.requirements:
            print(f"    - {requirement}", file=stream)
        print("  Script:", file=stream)
        for line in gate.script.splitlines() or [""]:
            print(f"    {line}", file=stream)
        print(f"  Status: {gate.status}", file=stream)

    suites = report.ci_suites
    print("\nCI suites:", file=stream)
    print(
        f"  MicroVM tests: {'selected' if suites.run_microvm_tests else 'not selected'}",
        file=stream,
    )
    if suites.run_microvm_tests:
        for command in suites.commands:
            print(f"    {command}", file=stream)
        print(
            "    Run these on a host with the named backend, or leave them to CI.",
            file=stream,
        )
    print(
        f"  Benchmarks: {'CI runs them' if suites.run_benchmarks else 'not selected'}",
        file=stream,
    )
    if suites.run_benchmarks:
        print(
            "    Local collection uses the nvx-collect-benchmarks skill.",
            file=stream,
        )
    print(
        "  OpenVMM CI check: "
        + ("selected" if suites.run_openvmm_ci_check else "not selected"),
        file=stream,
    )


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Explain or run the CI quality gates selected by a change."
    )
    parser.add_argument("--base", default=DEFAULT_BASE, metavar="REF")
    parser.add_argument("--files", nargs="+", metavar="PATH")
    parser.add_argument(
        "--os",
        choices=("linux", "windows"),
        default="windows" if os.name == "nt" else "linux",
        dest="operating_system",
    )
    parser.add_argument("--only", nargs="+", metavar="GATE")
    parser.add_argument("--run", action="store_true", dest="run_gates")
    parser.add_argument("--keep-going", action="store_true")
    parser.add_argument("--json", action="store_true", dest="json_output")
    return parser


def main(
    argv: Sequence[str] | None = None,
    *,
    root: Path = REPO_ROOT,
    runner: CommandRunner | None = None,
    tool_finder: ToolFinder = find_tool,
) -> int:
    """Print or execute the local gate plan."""
    args = _parser().parse_args(argv)
    command_runner = runner or SubprocessRunner()
    try:
        files = (
            tuple(args.files)
            if args.files is not None
            else detect_changed_files(args.base, root=root, runner=command_runner)
        )
        with tempfile.TemporaryDirectory(prefix="nvx-local-gates-") as temporary:
            runner_temp = Path(temporary)
            report = create_report(
                base=args.base,
                files=files,
                operating_system=cast(OperatingSystem, args.operating_system),
                root=root,
                runner_temp=runner_temp,
                only=args.only,
            )
            if not args.json_output:
                print_report(report)
            result = 0
            if args.run_gates:
                result = execute_report(
                    report,
                    root=root,
                    runner_temp=runner_temp,
                    runner=command_runner,
                    tool_finder=tool_finder,
                    keep_going=args.keep_going,
                    stream=sys.stderr if args.json_output else sys.stdout,
                )
            if args.json_output:
                print(json.dumps(report.as_json(), indent=2, sort_keys=True))
            return result
    except LocalGatesError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except OSError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
