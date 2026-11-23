#!/usr/bin/env python3
"""Static and in-memory tests of native ARM CI admission and handoff."""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
import struct
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace
from typing import Any, cast
from unittest.mock import Mock, patch

import yaml

ROOT = Path(__file__).resolve().parent.parent
ARM_JOBS = (
    "artifacts-arm64",
    "build-nvx-windows-arm64",
    "nvx-cli-windows-arm64",
    "build-openvmm-windows-arm64",
    "nvx-microvm-tests-whp-arm64",
    "nvx-microvm-debug-whp-arm64",
    "package-whp-arm64",
)


def load(relative: str) -> dict[str, Any]:
    return cast(
        dict[str, Any],
        yaml.safe_load((ROOT / relative).read_text(encoding="utf-8")),
    )


def step(
    document: dict[str, Any], name: str, *, job: str | None = None
) -> dict[str, Any]:
    steps = document["runs"]["steps"] if job is None else document["jobs"][job]["steps"]
    return next(item for item in steps if item["name"] == name)


def execute(script: str, environment: dict[str, str]) -> None:
    with (
        patch.dict(os.environ, environment),
        patch.object(sys, "path", list(sys.path)),
        contextlib.redirect_stdout(io.StringIO()),
    ):
        exec(compile(script, "<CI step>", "exec"), {})


class ArmCiTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.ci = load(".github/workflows/ci.yml")
        cls.runner = load(".github/actions/validate-runner/action.yml")
        cls.package = load(".github/actions/package-release/action.yml")
        cls.cold = load(".github/workflows/run-arm64-whp.yml")

    def test_workflow_job_graph_has_no_missing_dependencies_or_cycles(self) -> None:
        jobs = self.ci["jobs"]
        visiting: set[str] = set()
        visited: set[str] = set()

        def visit(name: str) -> None:
            self.assertIn(name, jobs)
            self.assertNotIn(name, visiting, f"cyclic dependency at {name}")
            if name in visited:
                return
            visiting.add(name)
            needs = jobs[name].get("needs", [])
            for dependency in [needs] if isinstance(needs, str) else needs:
                visit(dependency)
            visiting.remove(name)
            visited.add(name)

        for name in jobs:
            visit(name)

    def test_arm_gate_consumes_all_jobs_and_blocks_publication(self) -> None:
        jobs = self.ci["jobs"]
        self.assertTrue(set(ARM_JOBS).issubset(jobs["required-status-check"]["needs"]))
        self.assertIn("required-status-check", jobs["release"]["needs"])
        self.assertIn("package-whp-arm64", jobs["release"]["needs"])
        self.assertIn(
            "needs.required-status-check.result == 'success'", jobs["release"]["if"]
        )
        self.assertIn(
            "needs.package-whp-arm64.result == 'success'", jobs["release"]["if"]
        )
        self.assertEqual(jobs["required-status-check"]["name"], "Required status check")

    def gate_script(self) -> str:
        return step(self.ci, "Verify selected ARM64 jobs", job="required-status-check")[
            "run"
        ]

    def needs(self, *, selected: bool, trusted: bool = True) -> dict[str, Any]:
        outputs = {
            key: str(selected).lower()
            for key in (
                "run-guest-artifacts",
                "run-openvmm-binaries",
                "run-microvm-tests",
                "run-benchmarks",
            )
        }
        needs = {"openvmm-changes": {"result": "success", "outputs": outputs}}
        for job in ARM_JOBS:
            required = (
                selected
                if job == "artifacts-arm64"
                else trusted
                if job in ("build-nvx-windows-arm64", "nvx-cli-windows-arm64")
                else selected and trusted
            )
            needs[job] = {"result": "success" if required else "skipped"}
        return needs

    def test_arm_required_gate_checks_selected_and_unselected_jobs(self) -> None:
        for trusted in (True, False):
            for selected in (True, False):
                with self.subTest(trusted=trusted, selected=selected):
                    execute(
                        self.gate_script(),
                        {
                            "NEEDS": json.dumps(
                                self.needs(selected=selected, trusted=trusted)
                            ),
                            "SAME_REPOSITORY": str(trusted).lower(),
                        },
                    )

    def test_selected_arm_job_cannot_be_missing_skipped_failed_or_cancelled(
        self,
    ) -> None:
        for job in ARM_JOBS:
            for result in ("missing", "skipped", "failure", "cancelled", ""):
                with self.subTest(job=job, result=result):
                    needs = self.needs(selected=True)
                    if result == "missing":
                        del needs[job]
                    else:
                        needs[job]["result"] = result
                    with self.assertRaises(SystemExit) as error:
                        execute(
                            self.gate_script(),
                            {"NEEDS": json.dumps(needs), "SAME_REPOSITORY": "true"},
                        )
                    self.assertIn(job, str(error.exception))

    def test_classification_failure_and_absent_output_select_arm_work(self) -> None:
        for result, outputs in (("failure", {}), ("success", {})):
            needs = self.needs(selected=False)
            needs["openvmm-changes"] = {"result": result, "outputs": outputs}
            with self.subTest(result=result), self.assertRaises(SystemExit):
                execute(
                    self.gate_script(),
                    {"NEEDS": json.dumps(needs), "SAME_REPOSITORY": "true"},
                )

    def test_arm_jobs_preserve_same_repository_secret_boundary(self) -> None:
        for job in ARM_JOBS:
            if job == "artifacts-arm64":
                continue
            with self.subTest(job=job):
                guard = self.ci["jobs"][job]["if"]
                self.assertIn(
                    "github.event.pull_request.head.repo.full_name == github.repository",
                    guard,
                )
                self.assertIn("github.event.pull_request.draft == false", guard)
        check = step(
            self.ci, "Check OpenVMM's CI of the pinned revision", job="openvmm-ci"
        )
        self.assertIn("--include-arm64", check["run"])

    def test_arm_producers_checkout_the_pin_before_verifying_source(self) -> None:
        guest = load(".github/actions/build-arm64-guest-artifacts/action.yml")
        windows = load(".github/workflows/build-nvx-windows-arm64.yml")
        for steps in (guest["runs"]["steps"], windows["jobs"]["build"]["steps"]):
            with self.subTest(producer=steps):
                checkout = next(
                    index
                    for index, item in enumerate(steps)
                    if item.get("uses") == "./.github/actions/checkout-openvmm"
                )
                verify = next(
                    index
                    for index, item in enumerate(steps)
                    if "verify" in item.get("run", "")
                )
                self.assertLess(checkout, verify)
        prerequisites = step(guest, "Install native guest prerequisites")
        self.assertLess(
            guest["runs"]["steps"].index(prerequisites),
            guest["runs"]["steps"].index(step(guest, "Checkout OpenVMM")),
        )
        helper = load(".github/actions/checkout-openvmm/action.yml")
        checkout = step(helper, "Checkout OpenVMM")["with"]
        self.assertEqual(checkout["repository"], "microsoft/openvmm")
        self.assertEqual(checkout["path"], "openvmm")
        self.assertFalse(checkout["persist-credentials"])
        for name in (
            "Resolve OpenVMM revision on Linux",
            "Resolve OpenVMM revision on Windows",
        ):
            self.assertIn("git rev-parse :openvmm", step(helper, name)["run"])
        self.assertIn("steps.revision-linux.outputs.sha", checkout["ref"])
        self.assertIn("steps.revision-windows.outputs.sha", checkout["ref"])

    def test_arm_guest_checkout_preserves_public_fork_authentication(self) -> None:
        guest = load(".github/actions/build-arm64-guest-artifacts/action.yml")
        helper = load(".github/actions/checkout-openvmm/action.yml")
        for document in (guest, helper):
            self.assertFalse(document["inputs"]["ssh-key"]["required"])
            self.assertEqual(document["inputs"]["ssh-key"]["default"], "")
        checkout = step(helper, "Checkout OpenVMM")["with"]
        self.assertEqual(checkout["token"], "${{ github.token }}")
        self.assertEqual(checkout["ssh-key"], "${{ inputs.ssh-key }}")
        producer = self.ci["jobs"]["artifacts-arm64"]
        self.assertNotIn(
            "github.event.pull_request.head.repo.full_name == github.repository",
            producer["if"],
        )
        key = step(self.ci, "Build ARM guest artifacts", job="artifacts-arm64")["with"][
            "ssh-key"
        ]
        self.assertEqual(
            key,
            "${{ (github.event_name != 'pull_request' || "
            "github.event.pull_request.head.repo.full_name == github.repository) "
            "&& secrets.OPENVMM_DEPLOY_KEY || '' }}",
        )

    def test_arm_windows_checkout_receives_only_the_declared_deploy_key(self) -> None:
        workflow = load(".github/workflows/build-nvx-windows-arm64.yml")
        call = next(
            value
            for key, value in workflow.items()
            if str(key).lower() in ("on", "true")
        )["workflow_call"]
        self.assertTrue(call["secrets"]["OPENVMM_DEPLOY_KEY"]["required"])
        checkout = step(workflow, "Checkout OpenVMM", job="build")
        self.assertEqual(
            checkout["with"]["ssh-key"], "${{ secrets.OPENVMM_DEPLOY_KEY }}"
        )
        caller = self.ci["jobs"]["build-nvx-windows-arm64"]
        self.assertEqual(
            caller["secrets"],
            {"OPENVMM_DEPLOY_KEY": "${{ secrets.OPENVMM_DEPLOY_KEY }}"},
        )
        self.assertIn(
            "github.event.pull_request.head.repo.full_name == github.repository",
            caller["if"],
        )

    def test_windows_runner_pools_do_not_accept_the_other_architecture(self) -> None:
        arm = set(self.cold["jobs"]["test"]["runs-on"])
        windows = load(".github/workflows/build-nvx-windows-arm64.yml")
        self.assertEqual(set(windows["jobs"]["build"]["runs-on"]), arm)
        arm_runner = arm | {"benchmark"}
        x86_runner = (arm_runner - {"ARM64"}) | {"X64"}
        selected: list[str] = []
        for name, job in self.ci["jobs"].items():
            value = job.get("with", {}).get("runner")
            if not value:
                continue
            labels = set(json.loads(value))
            if not {"self-hosted", "windows"}.issubset(labels):
                continue
            if "ARM64" in labels:
                self.assertTrue(labels.issubset(arm_runner))
                self.assertFalse(labels.issubset(x86_runner))
            else:
                selected.append(name)
                self.assertTrue(labels.issubset(x86_runner))
                self.assertFalse(labels.issubset(arm_runner), name)
        self.assertCountEqual(
            selected,
            [
                "build-openvmm-windows-msvc",
                "nvx-microvm-tests-whp",
                "nvx-microvm-guests-whp",
                "nvx-microvm-debug-whp",
                "platform-whp",
            ],
        )

    def test_x86_shards_and_default_cli_selection_remain_x86(self) -> None:
        for backend in ("kvm", "mshv", "whp"):
            job = self.ci["jobs"][f"nvx-microvm-tests-{backend}"]
            self.assertEqual(job["strategy"]["matrix"]["shard"], [1, 2])
            self.assertEqual(
                job["uses"], "./.github/workflows/run-nvx-microvm-tests.yml"
            )
            self.assertNotIn("architecture", job["with"])
        default = load(".github/workflows/run-nvx-microvm-tests.yml")
        for name in (
            "Run NVX microVM tests on Linux",
            "Run NVX microVM tests on Windows",
        ):
            run = step(default, name, job="test")["run"]
            self.assertNotIn("--architecture", run)
            self.assertNotIn("--cold-only", run)

    def test_arm_guest_cache_cannot_restore_an_x86_or_partial_key(self) -> None:
        action = load(".github/actions/build-arm64-guest-artifacts/action.yml")
        restore = step(action, "Restore ARM guest artifacts")["with"]
        save = step(action, "Save ARM guest artifacts")["with"]
        self.assertNotIn("restore-keys", restore)
        self.assertEqual(restore["key"], save["key"])
        self.assertEqual(restore["path"], save["path"])
        for path in restore["path"].splitlines():
            self.assertTrue(path.startswith("build/aarch64/"), path)
        self.assertIn("build/aarch64/Image-debug\n", restore["path"])
        producer = self.ci["jobs"]["artifacts-arm64"]
        upload = step(self.ci, "Upload ARM guest artifacts", job="artifacts-arm64")[
            "with"
        ]
        self.assertEqual(restore["path"], upload["path"])
        self.assertEqual(upload["name"], "guest-aarch64")
        self.assertEqual(producer["runs-on"], "ubuntu-24.04-arm")

    def test_arm_cache_identity_tracks_paths_content_and_recipe(self) -> None:
        action = load(".github/actions/build-arm64-guest-artifacts/action.yml")
        script = step(action, "Resolve ARM guest cache identity")["run"]
        contents = {
            "SOURCE-MANIFEST.json": b"manifest",
            "rust-toolchain.toml": b"rust",
            ".dockerignore": b"ignore",
            "kernel/a.patch": b"patch",
            "guest/init": b"init",
            "ubuntu/recipe": b"ubuntu",
            "docker/Dockerfile": b"image",
            "scripts/nvx_tools/build.py": b"builder",
            "nvx/src/guest.rs": b"guest",
            "nvx/Cargo.toml": b"manifest",
            "nvx/Cargo.lock": b"lock",
            "scripts/nvx.py": b"cli",
            ".github/actions/build-arm64-guest-artifacts/action.yml": b"action",
            ".github/workflows/ci.yml": b"workflow",
        }

        class FakePath:
            def __init__(self, value: str) -> None:
                self.value = value
                self.parts = tuple(value.split("/"))

            def __hash__(self) -> int:
                return hash(self.value)

            def __eq__(self, other: object) -> bool:
                return isinstance(other, FakePath) and self.value == other.value

            def __lt__(self, other: FakePath) -> bool:
                return self.value < other.value

            def rglob(self, _pattern: str) -> list[FakePath]:
                return [
                    FakePath(name)
                    for name in contents
                    if name.startswith(self.value + "/")
                ]

            def is_file(self) -> bool:
                return self.value in contents

            def as_posix(self) -> str:
                return self.value

            def read_bytes(self) -> bytes:
                return contents[self.value]

            def open(
                self, *_args: object, **_kwargs: object
            ) -> contextlib.AbstractContextManager[io.StringIO]:
                return contextlib.nullcontext(output)

        def identity() -> str:
            nonlocal output
            output = io.StringIO()
            with patch("pathlib.Path", FakePath):
                execute(script, {"GITHUB_OUTPUT": "output"})
            return output.getvalue()

        output = io.StringIO()
        original = identity()
        self.assertTrue(original.startswith("key=guest-artifacts-aarch64-v1-"))
        for name in tuple(contents):
            with self.subTest(changed=name):
                before = contents[name]
                contents[name] = before + b"changed"
                self.assertNotEqual(original, identity())
                contents[name] = before
        contents["kernel/renamed.patch"] = contents.pop("kernel/a.patch")
        self.assertNotEqual(original, identity())

    def test_arm_openvmm_isa_is_checked_before_saving_the_binary_cache(self) -> None:
        action = load(".github/actions/build-openvmm/action.yml")
        names = [item["name"] for item in action["runs"]["steps"]]
        self.assertLess(
            names.index("Verify ARM64 OpenVMM executable ISA"),
            names.index("Save OpenVMM binary on Windows"),
        )
        workflow = load(".github/workflows/build-openvmm-binary.yml")
        self.assertEqual(
            step(workflow, "Build OpenVMM", job="build")["with"]["architecture"],
            "${{ inputs.architecture }}",
        )

    def test_arm_cache_admission_runs_for_hits_and_precedes_saves(self) -> None:
        action = load(".github/actions/build-arm64-guest-artifacts/action.yml")
        admission = step(action, "Validate built or cached ARM guest artifacts")
        self.assertNotIn("if", admission)
        self.assertIn('architecture="aarch64"', admission["run"])
        names = [item["name"] for item in action["runs"]["steps"]]
        self.assertLess(
            names.index(admission["name"]), names.index("Save ARM guest artifacts")
        )

    def test_focused_ci_and_managed_suites_are_in_action_inventory(self) -> None:
        action = load(".github/actions/validate-nvx/action.yml")
        for name in (
            "Validate NVX CLI on Linux",
            "Validate NVX CLI on Windows",
        ):
            command = step(action, name)["run"].replace("\\", "/")
            self.assertEqual(command.count("scripts/test_arm_ci.py"), 1)
            self.assertEqual(command.count("scripts/test_stage3_managed.py"), 1)

    def test_native_arm_admission_rejects_emulated_python_and_x64_rust(self) -> None:
        script = step(self.runner, "Verify native Windows ARM64 runner")["run"]
        for python_platform, rust_target, accepted in (
            ("win-arm64", "aarch64-pc-windows-msvc", True),
            ("win-amd64", "aarch64-pc-windows-msvc", False),
            ("win-arm64", "x86_64-pc-windows-msvc", False),
        ):
            with (
                self.subTest(python=python_platform, rust=rust_target),
                patch("sys.platform", "win32"),
                patch("platform.machine", return_value="ARM64"),
                patch("sysconfig.get_platform", return_value=python_platform),
                patch("subprocess.run") as run,
            ):
                run.return_value.stdout = f"host: {rust_target}\n"
                environment = {"RUNNER_ARCH": "ARM64", "BACKEND": "whp"}
                if accepted:
                    execute(script, environment)
                else:
                    with self.assertRaises(SystemExit):
                        execute(script, environment)

    def arm_doctor_command(
        self, *, verify_openvmm: bool, warp_probe: bool = False
    ) -> tuple[list[str], argparse.Namespace, str]:
        from nvx_tools import doctor

        script = step(self.runner, "Qualify ARM counter time on Windows")["run"]
        summary = io.StringIO()
        with (
            patch("subprocess.run") as run,
            patch.object(Path, "open", return_value=contextlib.nullcontext(summary)),
        ):
            execute(
                script,
                {
                    "VERIFY_OPENVMM": str(verify_openvmm).lower(),
                    "WARP_PROBE": str(warp_probe).lower(),
                    "GITHUB_STEP_SUMMARY": r"build\ci-summary.md",
                    "RUNNER_TEMP": r"build\runner-temp",
                },
            )
        run.assert_called_once()
        command = cast(list[str], run.call_args.args[0])
        run.assert_called_once_with(command, check=True)
        parser = argparse.ArgumentParser()
        doctor.configure_parser(parser)
        arguments = parser.parse_args(command[command.index("doctor") + 1 :])
        return command, arguments, summary.getvalue()

    def run_arm_doctor_contract(
        self, arguments: argparse.Namespace, *, admitted: bool = True
    ) -> tuple[int, str, Mock]:
        from nvx_tools import doctor

        output = io.StringIO()
        with (
            patch.object(doctor, "host_is_windows", return_value=True),
            patch.object(doctor.platform, "machine", return_value="ARM64"),
            patch("nvx_tools.doctor._whp_hypervisor_present", return_value=True),
            patch.object(
                doctor,
                "run_probe",
                return_value=[
                    (
                        "cpu",
                        {
                            "architecture": "aarch64",
                            "counter": "cntvct_el0",
                            "counter_hz": "19200001",
                        },
                    )
                ],
            ),
            patch(
                "nvx_tools.doctor._check_openvmm_preflight",
                return_value=doctor.CheckResult(
                    "H3", admitted, "fixture built-product/profile verifier result"
                ),
            ) as verifier,
            patch.dict(
                doctor.CHECKS,
                {
                    "H4": Mock(
                        return_value=doctor.CheckResult("H4", True, "fixture rate")
                    ),
                    "H6": Mock(
                        return_value=doctor.CheckResult("H6", True, "fixture warp")
                    ),
                },
            ),
            patch.object(
                Path, "open", return_value=contextlib.nullcontext(io.StringIO())
            ),
            contextlib.redirect_stdout(output),
        ):
            result = doctor.run(arguments)
        return result, output.getvalue(), verifier

    def test_arm_prebuild_doctor_defers_profile_and_runs_only_host_checks(self) -> None:
        command, arguments, summary = self.arm_doctor_command(verify_openvmm=False)
        self.assertEqual(arguments.checks, ["H1", "H4"])
        self.assertTrue(arguments.no_openvmm)
        self.assertIsNone(arguments.cpu_fingerprint)
        self.assertNotIn("--cpu-fingerprint", command)
        self.assertIn("built-product/profile admission: DEFERRED", summary)
        self.assertIn("not profile admission", summary)
        result, _, verifier = self.run_arm_doctor_contract(arguments)
        self.assertEqual(result, 0)
        verifier.assert_not_called()
        arguments.checks.append("H2")
        result, output, verifier = self.run_arm_doctor_contract(arguments)
        self.assertEqual(result, 1)
        self.assertIn(
            "ARM64 CPU/profile admission is unsupported without OpenVMM", output
        )
        verifier.assert_not_called()
        with self.assertRaisesRegex(SystemExit, "requires verify-openvmm=true"):
            self.arm_doctor_command(verify_openvmm=False, warp_probe=True)

    def test_arm_postbuild_doctor_requires_real_profile_admission_without_fingerprint(
        self,
    ) -> None:
        for warp_probe in (False, True):
            with self.subTest(warp_probe=warp_probe):
                command, arguments, summary = self.arm_doctor_command(
                    verify_openvmm=True, warp_probe=warp_probe
                )
                self.assertEqual(
                    arguments.checks,
                    ["H1", "H2", "H3", "H4"] + (["H6"] if warp_probe else []),
                )
                self.assertFalse(arguments.no_openvmm)
                self.assertIsNone(arguments.cpu_fingerprint)
                self.assertNotIn("--cpu-fingerprint", command)
                self.assertNotIn("DEFERRED", summary)
                result, _, verifier = self.run_arm_doctor_contract(arguments)
                self.assertEqual(result, 0)
                verifier.assert_called_once()
                context = verifier.call_args.args[0]
                self.assertEqual(context.architecture, "aarch64")
                self.assertFalse(context.no_openvmm)
                self.assertIsNone(context.fingerprint)
                self.assertEqual(context.facts["host_counter_hz"], "19200001")
                result, output, verifier = self.run_arm_doctor_contract(
                    arguments, admitted=False
                )
                self.assertEqual(result, 1)
                self.assertIn("failed (H2, H3)", output)
                verifier.assert_called_once()

    def test_arm_profile_admission_follows_required_built_artifact_downloads(
        self,
    ) -> None:
        build = load(".github/workflows/build-openvmm-binary.yml")
        build_names = [item["name"] for item in build["jobs"]["build"]["steps"]]
        build_admission = step(build, "Admit built ARM64 OpenVMM profile", job="build")
        self.assertNotIn("continue-on-error", build_admission)
        self.assertEqual(build_admission["with"]["verify-openvmm"], "true")
        self.assertEqual(build_admission["with"]["architecture"], "aarch64")
        self.assertEqual(
            build_admission["if"],
            "runner.os == 'Windows' && inputs.architecture == 'aarch64'",
        )
        self.assertLess(
            build_names.index("Build OpenVMM"),
            build_names.index("Download ARM profile admission guest artifacts"),
        )
        self.assertLess(
            build_names.index("Download ARM profile admission guest artifacts"),
            build_names.index(build_admission["name"]),
        )
        for upload in ("Upload OpenVMM executable", "Upload OpenVMM provenance"):
            self.assertLess(
                build_names.index(build_admission["name"]), build_names.index(upload)
            )
        self.assertIn(
            "artifacts-arm64",
            self.ci["jobs"]["build-openvmm-windows-arm64"]["needs"],
        )
        steps = self.cold["jobs"]["test"]["steps"]
        names = [item["name"] for item in steps]
        admission = step(self.cold, "Validate native ARM64 runner", job="test")
        self.assertNotIn("continue-on-error", admission)
        self.assertEqual(admission["with"]["verify-openvmm"], "true")
        for name in (
            "Download ARM guest artifacts",
            "Download ARM64 OpenVMM executable",
            "Download ARM64 OpenVMM provenance",
        ):
            self.assertLess(names.index(name), names.index(admission["name"]))
        self.assertLess(
            names.index(admission["name"]), names.index("Run shared ARM cold scenarios")
        )
        for job in ("nvx-microvm-tests-whp-arm64", "nvx-microvm-debug-whp-arm64"):
            self.assertIn(job, self.ci["jobs"]["required-status-check"]["needs"])
            self.assertIn("build-openvmm-windows-arm64", self.ci["jobs"][job]["needs"])
            self.assertIn("artifacts-arm64", self.ci["jobs"][job]["needs"])

    def test_arm_cold_commands_are_explicit_and_never_use_x86_shards(self) -> None:
        with patch.object(sys, "path", [str(ROOT / "scripts"), *sys.path]):
            from nvx_tools.guests import guest_descriptor
            from nvx_tools.microvm_tests import (
                COLD_ONLY_SCENARIOS,
                COLD_PROJECTION_SCENARIOS,
                SANDBOX_CONTROL_SCENARIOS,
                UBUNTU_UNSUPPORTED_SCENARIOS,
            )

        script = step(self.cold, "Run shared ARM cold scenarios", job="test")["run"]
        for guest in ("alpine", "ubuntu"):
            for debug in (False, True):
                with (
                    self.subTest(guest=guest, debug=debug),
                    patch("subprocess.run") as run,
                    patch.object(Path, "mkdir"),
                    patch.object(Path, "write_text") as write_text,
                ):
                    execute(
                        script, {"GUEST": guest, "DEBUG_KERNEL": str(debug).lower()}
                    )
                    command = run.call_args.args[0]
                    self.assertEqual(
                        command[command.index("--architecture") + 1], "aarch64"
                    )
                    self.assertEqual(command[command.index("--guest") + 1], guest)
                    self.assertEqual(command[command.index("--backend") + 1], "whp")
                    self.assertIn("--cold-only", command)
                    self.assertNotIn("--shard", command)
                    self.assertIn("--scenario", command)
                    self.assertEqual("--debug-kernel" in command, debug)
                    scenarios = [
                        command[index + 1]
                        for index, value in enumerate(command)
                        if value == "--scenario"
                    ]
                    self.assertTrue(
                        {
                            "smp",
                            "time-host-correction",
                        }.issubset(scenarios)
                    )
                    expected = (
                        {"smp", "time-host-correction"}
                        if debug
                        else set(COLD_ONLY_SCENARIOS)
                    )
                    excluded: set[str] = set()
                    if guest == "ubuntu":
                        excluded.update(UBUNTU_UNSUPPORTED_SCENARIOS)
                    if not guest_descriptor(guest).sandbox_control:
                        excluded.update(SANDBOX_CONTROL_SCENARIOS)
                    omitted = expected & excluded
                    self.assertEqual(set(scenarios), expected - omitted)
                    self.assertEqual(len(scenarios), len(set(scenarios)))
                    self.assertEqual(
                        len(scenarios), 2 if debug else 31 if guest == "alpine" else 22
                    )
                    if not debug and guest == "alpine":
                        self.assertTrue(
                            {"interrupt-map", "managed-lifecycle"}.issubset(scenarios)
                        )
                    self.assertTrue(
                        {
                            "console-exit",
                            "time-abi-conformance",
                            "untiered-network-policy",
                        }.isdisjoint(scenarios)
                    )
                    self.assertFalse(
                        any(
                            name.startswith(("restore-", "snapshot-"))
                            for name in scenarios
                        )
                    )
                    selection = json.loads(write_text.call_args.args[0])
                    self.assertEqual(selection["scope"], "cold-only")
                    self.assertEqual(
                        selection["selection"],
                        "debug-cold-subset" if debug else "normal-cold",
                    )
                    self.assertEqual(selection["selected_scenarios"], scenarios)
                    self.assertEqual(set(selection["omitted_guest_scenarios"]), omitted)
                    for reason in selection["omitted_guest_scenarios"].values():
                        self.assertIn("sandbox-control channel", reason)
                    self.assertEqual(
                        set(selection["deferred_restore_tails"]),
                        set(scenarios) & COLD_PROJECTION_SCENARIOS,
                    )
                    self.assertIn(
                        "x86 CPUID/MSR X1-X6",
                        selection["architecture_exclusions"]["time-abi-conformance"],
                    )
        with patch("subprocess.run") as run, self.assertRaises(SystemExit):
            execute(script, {"GUEST": "azurelinux", "DEBUG_KERNEL": "false"})
        run.assert_not_called()

    def test_direct_native_windows_arm_steps_do_not_require_bash(self) -> None:
        for relative in (
            ".github/workflows/run-arm64-whp.yml",
            ".github/workflows/build-nvx-windows-arm64.yml",
        ):
            document = load(relative)
            for job in document["jobs"].values():
                self.assertIn("ARM64", job["runs-on"])
                for item in job["steps"]:
                    if "run" in item:
                        self.assertEqual(item["shell"], "python {0}")
                        self.assertIn("runner.os == 'Windows'", item["if"])
                        compile(item["run"], item["name"], "exec")

    def test_package_identity_refuses_cross_isa_names_and_runners(self) -> None:
        script = step(self.package, "Check package architecture selection")["run"]
        for architecture, platform, runner_arch, runner_os, accepted in (
            ("x86_64", "windows-whp", "X64", "Windows", True),
            ("x86_64", "linux-kvm", "X64", "Linux", True),
            ("aarch64", "windows-whp-aarch64", "ARM64", "Windows", True),
            ("aarch64", "windows-whp", "ARM64", "Windows", False),
            ("aarch64", "windows-whp-aarch64", "X64", "Windows", False),
            ("aarch64", "windows-whp-aarch64", "ARM64", "Linux", False),
            ("x86_64", "windows-whp-aarch64", "X64", "Windows", False),
        ):
            environment = {
                "ARCHITECTURE": architecture,
                "PLATFORM": platform,
                "RUNNER_ARCH": runner_arch,
                "RUNNER_OS": runner_os,
            }
            with self.subTest(environment=environment):
                if accepted:
                    execute(script, environment)
                else:
                    with self.assertRaises(SystemExit):
                        execute(script, environment)

    def test_arm_package_checks_pe_isa_and_provenance_before_packaging(self) -> None:
        script = step(self.package, "Package native Windows ARM64 release")["run"]

        def pe(machine: int) -> bytes:
            data = bytearray(256)
            data[:2] = b"MZ"
            struct.pack_into("<I", data, 0x3C, 128)
            data[128:132] = b"PE\0\0"
            struct.pack_into("<H", data, 132, machine)
            return bytes(data)

        for machine, target, accepted in (
            (0xAA64, "aarch64-pc-windows-msvc", True),
            (0x8664, "aarch64-pc-windows-msvc", False),
            (0xAA64, "x86_64-pc-windows-msvc", False),
        ):
            with (
                self.subTest(machine=machine, target=target),
                patch.object(Path, "read_bytes", return_value=pe(machine)),
                patch.object(
                    Path,
                    "read_text",
                    side_effect=[json.dumps({"target": target}), "1.2.3"],
                ),
                patch("subprocess.run") as run,
            ):
                if accepted:
                    execute(script, {"PLATFORM": "windows-whp-aarch64"})
                    calls = [call.args[0] for call in run.call_args_list]
                    package = next(command for command in calls if "package" in command)
                    self.assertIn("--binary-only", package)
                    self.assertEqual(
                        package[package.index("--architecture") + 1], "aarch64"
                    )
                    self.assertTrue(
                        calls[-1][-1].endswith("nvx-1.2.3-windows-whp-aarch64.zip")
                    )
                else:
                    with self.assertRaises(SystemExit):
                        execute(script, {"PLATFORM": "windows-whp-aarch64"})
                    run.assert_not_called()

    def test_arm_package_handoff_does_not_download_x86_guests(self) -> None:
        document = load(".github/workflows/package-platform.yml")
        for item in document["jobs"]["package"]["steps"]:
            artifact = item.get("with", {}).get("name", "")
            if artifact.startswith("guest-"):
                if artifact == "guest-aarch64":
                    self.assertEqual(item["if"], "inputs.architecture == 'aarch64'")
                    self.assertEqual(item["with"]["path"], "build/aarch64")
                else:
                    self.assertEqual(item["if"], "inputs.architecture == 'x86_64'")
        action = load(".github/actions/publish-development-release/action.yml")
        downloads = [
            item["with"]["name"]
            for item in action["runs"]["steps"]
            if item.get("uses", "").startswith("actions/download-artifact@")
        ]
        self.assertIn("release-windows-whp-aarch64", downloads)

    def test_publisher_cannot_silently_omit_the_arm_package(self) -> None:
        action = load(".github/actions/publish-development-release/action.yml")
        script = step(action, "Require ARM64 in the development release inventory")[
            "run"
        ]
        for includes_arm in (False, True):
            packages = {"linux-kvm": Path("dist/linux.tar.gz")}
            if includes_arm:
                packages["windows-whp-aarch64"] = Path("dist/windows-arm64.zip")

            module = SimpleNamespace(release_packages=Mock(return_value=packages))
            with (
                self.subTest(includes_arm=includes_arm),
                patch.dict(sys.modules, {"publish_development_release": module}),
                patch.object(sys, "path", list(sys.path)),
                patch.object(Path, "read_text", return_value="1.2.3"),
            ):
                if includes_arm:
                    execute(script, {})
                else:
                    with self.assertRaises(SystemExit):
                        execute(script, {})


if __name__ == "__main__":
    unittest.main()
