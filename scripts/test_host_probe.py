#!/usr/bin/env python3
"""Tests for the read-only NVX host probe."""

from __future__ import annotations

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import cast
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import host_probe  # noqa: E402
from powershell_testing import run_powershell  # noqa: E402


@dataclass(frozen=True)
class Call:
    args: tuple[str, ...]
    cwd: Path | None
    timeout: float | None
    merge_stderr: bool


class FakeRunner:
    def __init__(
        self,
        responses: dict[tuple[str, ...], host_probe.CommandResult],
    ) -> None:
        self.responses = responses
        self.calls: list[Call] = []

    def run(
        self,
        args: Sequence[str],
        *,
        cwd: Path | None = None,
        timeout: float | None = None,
        merge_stderr: bool = False,
    ) -> host_probe.CommandResult:
        key = tuple(args)
        self.calls.append(Call(key, cwd, timeout, merge_stderr))
        if key not in self.responses:
            raise AssertionError(f"unexpected command: {list(args)}")
        return self.responses[key]


def _git(root: Path, *arguments: str) -> tuple[str, ...]:
    return ("git", "-C", str(root), *arguments)


def _repository_responses(
    candidate: Path,
    root: Path,
    *,
    initialized: bool,
) -> dict[tuple[str, ...], host_probe.CommandResult]:
    gitlink = "a" * 40
    responses = {
        _git(candidate, "rev-parse", "--show-toplevel"): host_probe.CommandResult(
            0, str(root) + "\n"
        ),
        _git(root, "rev-parse", "HEAD"): host_probe.CommandResult(0, "b" * 40 + "\n"),
        _git(root, "branch", "--show-current"): host_probe.CommandResult(
            0, "feature/probe\n"
        ),
        _git(root, "status", "--short"): host_probe.CommandResult(
            0, " M scripts/example.py\n M openvmm\n"
        ),
        _git(root, "rev-parse", "HEAD:openvmm"): host_probe.CommandResult(
            0, gitlink + "\n"
        ),
    }
    if initialized:
        openvmm = root / "openvmm"
        responses[_git(openvmm, "rev-parse", "HEAD")] = host_probe.CommandResult(
            0, gitlink + "\n"
        )
        responses[_git(openvmm, "status", "--short")] = host_probe.CommandResult(
            0, " M vm.rs\n"
        )
    return responses


class HostProbeTests(unittest.TestCase):
    def make_checkout(self, root: Path, *, initialized: bool) -> None:
        (root / "openvmm").mkdir()
        if initialized:
            (root / "openvmm" / ".git").write_text(
                "gitdir: elsewhere", encoding="utf-8"
            )
        (root / "nvx-rs" / "target" / "release").mkdir(parents=True)
        build = root / "build"
        (build / "nested").mkdir(parents=True)
        (build / "one").write_text("1", encoding="utf-8")
        (build / "nested" / "two").write_text("2", encoding="utf-8")

    def test_linux_probe_collects_repository_devices_verify_and_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            self.make_checkout(root, initialized=True)
            (root / "nvx-rs" / "target" / "release" / "nvx-rs").write_text(
                "binary", encoding="utf-8"
            )
            responses = _repository_responses(root, root, initialized=True)
            responses[("pgrep", "-af", "openvmm")] = host_probe.CommandResult(
                0, "42 /usr/bin/openvmm --run\n"
            )
            responses[(sys.executable, "scripts/nvx.py", "verify")] = (
                host_probe.CommandResult(
                    3, "\n".join(f"line {number}" for number in range(25)) + "\n"
                )
            )
            runner = FakeRunner(responses)
            real_exists = Path.exists

            def exists(path: Path) -> bool:
                if path.as_posix() == "/dev/kvm":
                    return True
                if path.as_posix() == "/dev/mshv":
                    return False
                return real_exists(path)

            def access(path: os.PathLike[str], mode: int) -> bool:
                del mode
                return Path(path).as_posix() == "/dev/kvm"

            with (
                patch.object(host_probe.platform, "system", return_value="Linux"),
                patch.object(host_probe.platform, "release", return_value="6.8"),
                patch.object(host_probe.platform, "machine", return_value="x86_64"),
                patch.object(
                    host_probe.platform, "python_version", return_value="3.12.7"
                ),
                patch.object(Path, "exists", autospec=True, side_effect=exists),
                patch.object(host_probe.os, "access", side_effect=access),
            ):
                document = host_probe.collect_probe("kvm", False, runner, root)

            os_info = cast(dict[str, object], document["os"])
            self.assertEqual(
                os_info,
                {
                    "system": "Linux",
                    "release": "6.8",
                    "machine": "x86_64",
                },
            )
            repository = cast(dict[str, object], document["repository"])
            self.assertEqual(
                repository["dirty"], [" M scripts/example.py", " M openvmm"]
            )
            openvmm = cast(dict[str, object], document["openvmm"])
            self.assertTrue(openvmm["matches"])
            self.assertTrue(openvmm["dirty"])
            devices = cast(dict[str, dict[str, bool]], document["devices"])
            self.assertEqual(
                devices["/dev/kvm"],
                {"exists": True, "readable": True, "writable": True},
            )
            self.assertEqual(
                devices["/dev/mshv"],
                {"exists": False, "readable": False, "writable": False},
            )
            self.assertEqual(
                document["openvmm_processes"], ["42 /usr/bin/openvmm --run"]
            )
            self.assertTrue(document["nvx_rs"])
            self.assertEqual(document["artifacts"], {"exists": True, "files": 2})
            verify = cast(dict[str, object], document["verify"])
            self.assertEqual(verify["exit_code"], 3)
            self.assertEqual(
                verify["last_lines"], [f"line {number}" for number in range(5, 25)]
            )
            backend = cast(dict[str, object], document["backend"])
            self.assertTrue(backend["usable"])
            verify_call = next(
                call for call in runner.calls if call.args[0] == sys.executable
            )
            self.assertEqual(verify_call.cwd, root)
            self.assertEqual(verify_call.timeout, 300)
            self.assertTrue(verify_call.merge_stderr)

    def test_windows_probe_collects_host_state_without_verification(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            self.make_checkout(root, initialized=False)
            executable = root / "nvx-rs" / "target" / "release" / "nvx-rs.exe"
            executable.write_text("binary", encoding="utf-8")
            responses = _repository_responses(root, root, initialized=False)
            responses[
                host_probe.powershell_arguments(host_probe.HYPERVISOR_PRESENT)
            ] = host_probe.CommandResult(0, "True\r\n")
            responses[
                host_probe.powershell_arguments(host_probe.HYPERVISOR_PLATFORM)
            ] = host_probe.CommandResult(1, stderr="Access is denied")
            responses[host_probe.powershell_arguments(host_probe.OPENVMM_PROCESSES)] = (
                host_probe.CommandResult(
                    0, '[{"Id": 42, "Path": "C:\\\\openvmm.exe"}]\n'
                )
            )
            runner = FakeRunner(responses)
            with (
                patch.object(host_probe.platform, "system", return_value="Windows"),
                patch.object(host_probe.platform, "release", return_value="11"),
                patch.object(host_probe.platform, "machine", return_value="AMD64"),
                patch.object(
                    host_probe,
                    "whp_api_state",
                    return_value={"available": True, "hypervisor_present": True},
                ),
            ):
                document = host_probe.collect_probe("whp", True, runner, root)

            self.assertEqual(document["openvmm"], "not initialized")
            devices = cast(dict[str, object], document["devices"])
            self.assertIs(devices["HypervisorPresent"], True)
            self.assertEqual(devices["HypervisorPlatform"], "unknown (needs elevation)")
            processes = cast(list[dict[str, object]], document["openvmm_processes"])
            self.assertEqual(processes[0]["Id"], 42)
            self.assertTrue(document["nvx_rs"])
            self.assertNotIn("verify", document)
            backend = cast(dict[str, object], document["backend"])
            self.assertTrue(backend["usable"])
            self.assertIn(
                "smoke run is the definitive check", cast(str, backend["reason"])
            )
            self.assertFalse(
                any(call.args[0] == sys.executable for call in runner.calls)
            )

    def test_backend_verdicts_reject_the_other_operating_system(self) -> None:
        linux_devices: dict[str, object] = {
            "/dev/kvm": {
                "exists": True,
                "readable": False,
                "writable": True,
            }
        }
        kvm = host_probe.backend_verdict("kvm", "Linux", linux_devices)
        self.assertFalse(kvm["usable"])
        self.assertIn("readable and writable", cast(str, kvm["reason"]))

        whp_on_linux = host_probe.backend_verdict("whp", "Linux", linux_devices)
        self.assertFalse(whp_on_linux["usable"])
        self.assertIn("only on Windows", cast(str, whp_on_linux["reason"]))

        kvm_on_windows = host_probe.backend_verdict(
            "kvm", "Windows", {"HypervisorPresent": True}
        )
        self.assertFalse(kvm_on_windows["usable"])
        self.assertIn("only on Linux", cast(str, kvm_on_windows["reason"]))

    def test_the_whp_verdict_follows_the_hypervisor_platform_api(self) -> None:
        # A running hypervisor is not enough: the API that OpenVMM calls must
        # serve it, as doctor's H1 check requires.
        for name, api, usable, reason in (
            (
                "serves a hypervisor",
                {"available": True, "hypervisor_present": True},
                True,
                "definitive check",
            ),
            (
                "serves none",
                {"available": True, "hypervisor_present": False},
                False,
                "reports no hypervisor",
            ),
            (
                "is missing",
                {"available": False, "detail": "WinHvPlatform.dll is unavailable"},
                False,
                "WinHvPlatform.dll is unavailable; enabling",
            ),
            ("was not queried", None, False, "was not queried"),
        ):
            with self.subTest(api=name):
                devices: dict[str, object] = {"HypervisorPresent": True}
                if api is not None:
                    devices["WinHvPlatform"] = api
                verdict = host_probe.backend_verdict("whp", "Windows", devices)
                self.assertIs(verdict["usable"], usable)
                self.assertIn(reason, cast(str, verdict["reason"]))

    @unittest.skipUnless(os.name == "nt", "requires Windows")
    def test_the_whp_api_query_runs_without_elevation(self) -> None:
        state = host_probe.whp_api_state()
        self.assertIsInstance(state["available"], bool)
        if state["available"]:
            self.assertIsInstance(state["hypervisor_present"], bool)
        else:
            self.assertIn("detail", state)

    def test_json_cli_prints_the_probe_document(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            self.make_checkout(root, initialized=False)
            responses = _repository_responses(root, root, initialized=False)
            responses[("pgrep", "-af", "openvmm")] = host_probe.CommandResult(1)
            runner = FakeRunner(responses)
            stdout = io.StringIO()
            stderr = io.StringIO()
            with (
                patch.object(host_probe, "REPOSITORY_ROOT", root),
                patch.object(host_probe.platform, "system", return_value="Linux"),
                patch.object(host_probe.platform, "release", return_value="test"),
                patch.object(host_probe.platform, "machine", return_value="test"),
                patch.object(Path, "exists", autospec=True, side_effect=Path.exists),
                patch.object(host_probe.os, "access", return_value=False),
                contextlib.redirect_stdout(stdout),
                contextlib.redirect_stderr(stderr),
            ):
                status = host_probe.run_cli(["--no-verify", "--json"], runner)
            self.assertEqual(status, 0)
            self.assertEqual(stderr.getvalue(), "")
            document = cast(dict[str, object], json.loads(stdout.getvalue()))
            self.assertNotIn("verify", document)
            self.assertEqual(cast(dict[str, object], document["os"])["system"], "Linux")

    def test_cli_reports_probe_errors_without_a_traceback(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            runner = FakeRunner(
                {
                    _git(root, "rev-parse", "--show-toplevel"): (
                        host_probe.CommandResult(128, stderr="not a repository")
                    )
                }
            )
            stdout = io.StringIO()
            stderr = io.StringIO()
            with (
                patch.object(host_probe, "REPOSITORY_ROOT", root),
                contextlib.redirect_stdout(stdout),
                contextlib.redirect_stderr(stderr),
            ):
                status = host_probe.run_cli(["--json"], runner)
            self.assertEqual(status, 1)
            self.assertEqual(stdout.getvalue(), "")
            self.assertIn("error: git rev-parse", stderr.getvalue())
            self.assertNotIn("Traceback", stderr.getvalue())

    @unittest.skipUnless(os.name == "nt", "requires Windows PowerShell")
    def test_the_windows_process_query_succeeds_whether_or_not_openvmm_runs(
        self,
    ) -> None:
        # Get-Process alone exits with 1 when no process matches, which once
        # failed every probe of a Windows host that ran no VM.
        result = run_powershell(
            list(host_probe.powershell_arguments(host_probe.OPENVMM_PROCESSES))
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")
        output = result.stdout.strip()
        if output:
            self.assertIsInstance(json.loads(output), list)


if __name__ == "__main__":
    unittest.main()
