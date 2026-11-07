#!/usr/bin/env python3
"""Tests for the developer-local NVX host command tool."""

from __future__ import annotations

import base64
import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
import zipfile
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import cast
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import nvx_hosts  # noqa: E402
from powershell_testing import run_powershell  # noqa: E402

SCRIPTS = Path(__file__).parent
SSH = ("ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=20")


@dataclass(frozen=True)
class Call:
    args: tuple[str, ...]
    input_text: str | None
    capture_stdout: bool


class FakeRunner:
    def __init__(self, *results: nvx_hosts.CommandResult) -> None:
        self.results = list(results)
        self.calls: list[Call] = []

    def run(
        self,
        args: Sequence[str],
        *,
        input_text: str | None = None,
        capture_stdout: bool = False,
    ) -> nvx_hosts.CommandResult:
        self.calls.append(Call(tuple(args), input_text, capture_stdout))
        if not self.results:
            raise AssertionError(f"unexpected command: {list(args)}")
        return self.results.pop(0)


def _git(repo: Path, *arguments: str) -> str:
    return subprocess.run(
        ["git", "-C", str(repo), *arguments],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()


def _decode_powershell(call: Call) -> str:
    encoded = call.args[call.args.index("-EncodedCommand") + 1]
    return base64.b64decode(encoded).decode("utf-16-le")


def _archive(call: Call) -> zipfile.ZipFile:
    """Open the tool archive that an upload sends in base64 on stdin."""
    return zipfile.ZipFile(io.BytesIO(base64.b64decode(cast(str, call.input_text))))


class LocalPowerShellRunner:
    """Runs the PowerShell part of each SSH command on this host."""

    def __init__(self) -> None:
        self.results: list[subprocess.CompletedProcess[str]] = []

    def run(
        self,
        args: Sequence[str],
        *,
        input_text: str | None = None,
        capture_stdout: bool = False,
    ) -> nvx_hosts.CommandResult:
        del capture_stdout
        command = list(args)
        result = run_powershell(
            command[command.index("powershell") :], input=input_text
        )
        self.results.append(result)
        return nvx_hosts.CommandResult(result.returncode, result.stdout)


class HostInventoryTests(unittest.TestCase):
    def write_inventory(
        self,
        root: Path,
        hosts: dict[str, dict[str, str]],
        **extra: object,
    ) -> Path:
        path = root / ".nvx-hosts.json"
        path.write_text(json.dumps({"hosts": hosts, **extra}), encoding="utf-8")
        return path

    def profile(
        self,
        *,
        backend: str = "kvm",
        host_type: str | None = "baremetal",
        remote_repo: str | None = "/srv/nvx",
    ) -> dict[str, str]:
        profile = {
            "ssh_target": f"{backend}-target",
            "backend": backend,
        }
        if host_type is not None:
            profile["host_type"] = host_type
        if remote_repo is not None:
            profile["remote_repo"] = remote_repo
        return profile

    def run_cli(
        self,
        arguments: Sequence[str],
        runner: FakeRunner,
    ) -> tuple[int, str, str]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with (
            contextlib.redirect_stdout(stdout),
            contextlib.redirect_stderr(stderr),
        ):
            status = nvx_hosts.run_cli(arguments, runner)
        return status, stdout.getvalue(), stderr.getvalue()

    def test_cli_lists_and_shorthand_shows_valid_host_types(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = self.write_inventory(
                Path(temporary),
                {
                    "bare": self.profile(host_type="baremetal"),
                    "virtual": self.profile(host_type="virtual-machine"),
                },
            )
            for arguments, key in (
                (["list", "--file", str(path)], "hosts"),
                (["virtual", "--file", str(path)], "host"),
            ):
                with self.subTest(arguments=arguments):
                    status, output, error = self.run_cli(arguments, FakeRunner())
                    self.assertEqual(status, 0)
                    self.assertEqual(error, "")
                    payload = cast(dict[str, object], json.loads(output))
                    self.assertTrue(payload["remote_enabled"])
                    self.assertEqual(payload["inventory"], str(path.resolve()))
                    if key == "hosts":
                        hosts = cast(list[dict[str, object]], payload[key])
                        self.assertEqual(
                            [host["host_type"] for host in hosts],
                            ["baremetal", "virtual-machine"],
                        )
                    else:
                        host = cast(dict[str, object], payload[key])
                        self.assertEqual(host["host_type"], "virtual-machine")

    def test_no_arguments_means_list(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            path = self.write_inventory(root, {"lab": self.profile()})
            with patch.object(nvx_hosts, "REPOSITORY_ROOT", root):
                status, output, error = self.run_cli([], FakeRunner())
            self.assertEqual(status, 0)
            self.assertEqual(error, "")
            payload = cast(dict[str, object], json.loads(output))
            self.assertEqual(payload["inventory"], str(path.resolve()))

    def test_missing_inventory_reports_remote_development_disabled(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            runner = FakeRunner(nvx_hosts.CommandResult(128))
            with patch.object(nvx_hosts, "REPOSITORY_ROOT", root):
                status, output, error = self.run_cli([], runner)
            self.assertEqual(status, 0)
            self.assertEqual(error, "")
            payload = cast(dict[str, object], json.loads(output))
            self.assertFalse(payload["remote_enabled"])
            self.assertEqual(payload["hosts"], [])
            self.assertIn("only local", cast(str, payload["reason"]))

    def test_linked_worktree_uses_the_main_worktree_inventory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            main = base / "main"
            linked = base / "linked"
            main.mkdir()
            _git(main, "init", "--quiet")
            for key, value in (
                ("user.name", "NVX Test"),
                ("user.email", "nvx@example.com"),
                ("commit.gpgsign", "false"),
            ):
                _git(main, "config", key, value)
            (main / "tracked").write_text("tracked", encoding="utf-8")
            _git(main, "add", "tracked")
            _git(main, "commit", "--quiet", "-m", "initial")
            _git(main, "worktree", "add", "--quiet", "--detach", str(linked), "HEAD")
            inventory = self.write_inventory(main, {"lab": self.profile()})
            common = _git(
                linked,
                "rev-parse",
                "--path-format=absolute",
                "--git-common-dir",
            )
            runner = FakeRunner(nvx_hosts.CommandResult(0, common + "\n"))
            with patch.object(nvx_hosts, "REPOSITORY_ROOT", linked):
                status, output, error = self.run_cli([], runner)
            self.assertEqual(status, 0)
            self.assertEqual(error, "")
            payload = cast(dict[str, object], json.loads(output))
            self.assertEqual(payload["inventory"], str(inventory.resolve()))
            self.assertEqual(
                runner.calls[0].args,
                (
                    "git",
                    "-C",
                    str(linked),
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-common-dir",
                ),
            )

    def test_rejects_missing_host_type(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = self.write_inventory(
                Path(temporary), {"lab": self.profile(host_type=None)}
            )
            with self.assertRaisesRegex(
                nvx_hosts.InventoryError,
                r"hosts\.lab\.host_type must be a non-empty string",
            ):
                nvx_hosts.load_inventory(path)

    def test_rejects_unsupported_host_type(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = self.write_inventory(
                Path(temporary), {"lab": self.profile(host_type="container")}
            )
            with self.assertRaisesRegex(
                nvx_hosts.InventoryError,
                r"host_type must be one of: baremetal, virtual-machine",
            ):
                nvx_hosts.load_inventory(path)

    def test_rejects_version_field(self) -> None:
        for version in (1, 2):
            with (
                self.subTest(version=version),
                tempfile.TemporaryDirectory() as temporary,
            ):
                path = self.write_inventory(
                    Path(temporary), {"lab": self.profile()}, version=version
                )
                with self.assertRaisesRegex(
                    nvx_hosts.InventoryError,
                    r"inventory has unsupported fields: version",
                ):
                    nvx_hosts.load_inventory(path)

    def test_exec_builds_the_exact_linux_ssh_command(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = self.write_inventory(Path(temporary), {"linux": self.profile()})
            runner = FakeRunner(nvx_hosts.CommandResult(23))
            status, _, error = self.run_cli(
                [
                    "exec",
                    "linux",
                    "--file",
                    str(path),
                    "--cwd",
                    "/srv/nvx tree",
                    "--",
                    "printf",
                    "a b",
                    "it's",
                ],
                runner,
            )
            self.assertEqual(status, 23)
            self.assertEqual(error, "")
            self.assertEqual(
                runner.calls[0],
                Call(
                    (
                        "ssh",
                        "-o",
                        "BatchMode=yes",
                        "-o",
                        "ConnectTimeout=20",
                        "-n",
                        "kvm-target",
                        "cd -- '/srv/nvx tree' && printf 'a b' 'it'\"'\"'s'",
                    ),
                    None,
                    False,
                ),
            )

    def test_exec_encodes_the_exact_windows_powershell_command(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = self.write_inventory(
                Path(temporary),
                {
                    "windows": self.profile(
                        backend="whp", remote_repo=r"C:\src\NVX's tree"
                    )
                },
            )
            runner = FakeRunner(nvx_hosts.CommandResult(0))
            status, _, error = self.run_cli(
                [
                    "exec",
                    "windows",
                    "--file",
                    str(path),
                    "--",
                    "tool.exe",
                    "a b",
                    "it's",
                ],
                runner,
            )
            self.assertEqual(status, 0)
            self.assertEqual(error, "")
            call = runner.calls[0]
            self.assertEqual(
                call.args[:-1],
                (
                    "ssh",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=20",
                    "-n",
                    "whp-target",
                    "powershell",
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-EncodedCommand",
                ),
            )
            self.assertEqual(
                _decode_powershell(call),
                "$ProgressPreference = 'SilentlyContinue'; "
                "$ErrorActionPreference = 'Stop'; "
                "Set-Location -LiteralPath 'C:\\src\\NVX''s tree'; "
                "& 'tool.exe' 'a b' 'it''s'; exit $LASTEXITCODE",
            )

    def test_exec_requires_a_working_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = self.write_inventory(
                Path(temporary),
                {"lab": self.profile(remote_repo=None)},
            )
            status, _, error = self.run_cli(
                ["exec", "lab", "--file", str(path), "--", "true"],
                FakeRunner(),
            )
            self.assertEqual(status, 1)
            self.assertIn("pass --cwd DIR", error)

    def test_host_commands_fail_without_an_inventory(self) -> None:
        # Listing reports that remote development is disabled, but a command
        # that runs nothing must not report success.
        with tempfile.TemporaryDirectory() as temporary:
            missing = Path(temporary) / ".nvx-hosts.json"
            script = Path(temporary) / "task.sh"
            script.write_text("true\n", encoding="utf-8")
            for arguments in (
                ["exec", "lab", "--file", str(missing), "--", "true"],
                ["run", "lab", str(script), "--file", str(missing)],
                ["probe", "lab", "--file", str(missing)],
                ["worktree", "lab", "--file", str(missing), "--", "add", "HEAD"],
            ):
                with self.subTest(command=arguments[0]):
                    status, output, error = self.run_cli(arguments, FakeRunner())
                    self.assertEqual(status, 1)
                    self.assertEqual(output, "")
                    self.assertIn("only local NVX development is enabled", error)

    @unittest.skipUnless(os.name == "nt", "requires Windows PowerShell")
    def test_windows_scripts_run_quietly_in_real_powershell(self) -> None:
        # Runs the PowerShell that the SSH commands carry on this host, as the
        # server's OpenSSH would, without its progress records on stderr.
        with tempfile.TemporaryDirectory() as temporary:
            # PowerShell reports the long form of an 8.3 name, such as the
            # RUNNER~1 of a Windows runner's temporary directory.
            root = Path(temporary).resolve()
            script = root / "task.ps1"
            script.write_text(
                "param($first, $second)\n"
                'Write-Output "$((Get-Location).Path)|$first|$second"\n'
                "exit 7\n",
                encoding="utf-8",
            )
            runner = LocalPowerShellRunner()
            profile = nvx_hosts.HostProfile(
                "windows", "whp-target", "whp", "virtual-machine", None, None
            )
            status = nvx_hosts._run_remote_script(  # pyright: ignore[reportPrivateUsage]
                profile, str(root), script, ["a b", "it's"], runner
            )
            self.assertEqual(status, 7)
            self.assertEqual(len(runner.results), 3)
            uploaded = Path(runner.results[0].stdout.strip())
            self.assertFalse(uploaded.exists())
            self.assertEqual(runner.results[1].stdout.strip(), f"{root}|a b|it's")
            for result in runner.results:
                self.assertEqual(result.stderr, "")

    def test_linux_run_uploads_executes_and_always_removes_the_script(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            inventory = self.write_inventory(root, {"lab": self.profile()})
            script = root / "task.sh"
            script.write_bytes(b"printf ok\r\n")
            # The host's temporary directory can hold a space, at which the
            # remote shell would split an unquoted path.
            runner = FakeRunner(
                nvx_hosts.CommandResult(0, "/tmp/nvx dir/nvx.123\n"),
                nvx_hosts.CommandResult(7),
                nvx_hosts.CommandResult(9),
            )
            status, _, error = self.run_cli(
                [
                    "run",
                    "lab",
                    str(script),
                    "--file",
                    str(inventory),
                    "--",
                    "a b",
                ],
                runner,
            )
            self.assertEqual(status, 7)
            self.assertIn("failed to remove remote script", error)
            self.assertEqual(len(runner.calls), 3)
            self.assertEqual(runner.calls[0].input_text, "printf ok\n")
            self.assertEqual(
                runner.calls[0].args,
                (
                    "ssh",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=20",
                    "kvm-target",
                    'f=$(mktemp) && cat > "$f" && printf %s "$f"',
                ),
            )
            self.assertEqual(
                runner.calls[1].args,
                (
                    "ssh",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=20",
                    "-n",
                    "kvm-target",
                    "cd -- /srv/nvx && bash '/tmp/nvx dir/nvx.123' 'a b'",
                ),
            )
            self.assertEqual(
                runner.calls[2].args,
                (
                    "ssh",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=20",
                    "-n",
                    "kvm-target",
                    "rm -f -- '/tmp/nvx dir/nvx.123'",
                ),
            )

    def test_windows_run_encodes_upload_execution_and_removal(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            inventory = self.write_inventory(
                root,
                {"lab": self.profile(backend="whp", remote_repo=r"C:\nvx")},
            )
            script = root / "task.ps1"
            # A Windows script keeps its line endings, which write_text would
            # choose by the platform that runs the test.
            script.write_bytes(b"Write-Output ok\r\n")
            runner = FakeRunner(
                nvx_hosts.CommandResult(0, "C:\\Temp\\nvx-1.ps1\r\n"),
                nvx_hosts.CommandResult(0),
                nvx_hosts.CommandResult(0),
            )
            status, _, error = self.run_cli(
                [
                    "run",
                    "lab",
                    str(script),
                    "--file",
                    str(inventory),
                    "--",
                    "a b",
                ],
                runner,
            )
            self.assertEqual(status, 0)
            self.assertEqual(error, "")
            self.assertEqual(len(runner.calls), 3)
            self.assertNotIn("-n", runner.calls[0].args)
            self.assertIn(
                "[IO.File]::WriteAllText", _decode_powershell(runner.calls[0])
            )
            self.assertEqual(runner.calls[0].input_text, "Write-Output ok\r\n")
            self.assertIn("-n", runner.calls[1].args)
            self.assertEqual(
                _decode_powershell(runner.calls[1]),
                "$ProgressPreference = 'SilentlyContinue'; "
                "$ErrorActionPreference = 'Stop'; "
                "Set-Location -LiteralPath 'C:\\nvx'; "
                "& 'C:\\Temp\\nvx-1.ps1' 'a b'; exit $LASTEXITCODE",
            )
            self.assertIn("-n", runner.calls[2].args)
            self.assertIn(
                "Remove-Item -LiteralPath 'C:\\Temp\\nvx-1.ps1'",
                _decode_powershell(runner.calls[2]),
            )

    def test_run_rejects_missing_and_empty_scripts_without_connecting(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            inventory = self.write_inventory(root, {"lab": self.profile()})
            empty = root / "empty.sh"
            empty.write_bytes(b"")
            for script, message in (
                (root / "missing.sh", "cannot read script"),
                (empty, "is empty"),
            ):
                with self.subTest(script=script):
                    runner = FakeRunner()
                    status, _, error = self.run_cli(
                        ["run", "lab", str(script), "--file", str(inventory)],
                        runner,
                    )
                    self.assertEqual(status, 1)
                    self.assertIn(message, error)
                    self.assertEqual(runner.calls, [])

    def test_run_reports_upload_failures_and_empty_remote_paths(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            inventory = self.write_inventory(root, {"lab": self.profile()})
            script = root / "task.sh"
            script.write_text("true\n", encoding="utf-8")
            for result, expected_status, message in (
                (nvx_hosts.CommandResult(19), 19, "failed to upload"),
                (nvx_hosts.CommandResult(0, "\n"), 1, "no temporary script path"),
            ):
                with self.subTest(result=result):
                    runner = FakeRunner(result)
                    status, _, error = self.run_cli(
                        ["run", "lab", str(script), "--file", str(inventory)],
                        runner,
                    )
                    self.assertEqual(status, expected_status)
                    self.assertIn(message, error)
                    self.assertEqual(len(runner.calls), 1)

    def test_probe_runs_this_checkouts_probe_against_the_host_checkout(self) -> None:
        # A host's checkout can predate the probe, so this checkout's copy
        # runs there from a temporary file.
        with tempfile.TemporaryDirectory() as temporary:
            inventory = self.write_inventory(
                Path(temporary), {"lab": self.profile(backend="mshv")}
            )
            runner = FakeRunner(
                nvx_hosts.CommandResult(0, "/tmp/nvx.1\n"),
                nvx_hosts.CommandResult(0, '{"backend": {}}'),
                nvx_hosts.CommandResult(0),
            )
            status, output, error = self.run_cli(
                [
                    "probe",
                    "lab",
                    "--file",
                    str(inventory),
                    "--no-verify",
                ],
                runner,
            )
            self.assertEqual(status, 0)
            self.assertEqual(error, "")
            self.assertEqual(output, '{"backend": {}}\n')
            self.assertEqual(
                runner.calls[0].args,
                (
                    *SSH,
                    "mshv-target",
                    'f=$(mktemp) && base64 -d > "$f" && printf %s "$f"',
                ),
            )
            archive = _archive(runner.calls[0])
            self.assertEqual(
                sorted(archive.namelist()), ["__main__.py", "host_probe.py"]
            )
            self.assertEqual(
                archive.read("host_probe.py"), (SCRIPTS / "host_probe.py").read_bytes()
            )
            self.assertEqual(
                runner.calls[1].args,
                (
                    *SSH,
                    "-n",
                    "mshv-target",
                    "cd -- /srv/nvx && python3 /tmp/nvx.1 --repo /srv/nvx --json "
                    "--backend mshv --no-verify",
                ),
            )
            self.assertTrue(runner.calls[1].capture_stdout)
            self.assertEqual(
                runner.calls[2].args,
                (*SSH, "-n", "mshv-target", "rm -f -- /tmp/nvx.1"),
            )

    def test_probe_rejects_invalid_json(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            inventory = self.write_inventory(Path(temporary), {"lab": self.profile()})
            runner = FakeRunner(
                nvx_hosts.CommandResult(0, "/tmp/nvx.1\n"),
                nvx_hosts.CommandResult(0, "not json"),
                nvx_hosts.CommandResult(0),
            )
            status, output, error = self.run_cli(
                ["probe", "lab", "--file", str(inventory)], runner
            )
            self.assertEqual(status, 1)
            self.assertEqual(output, "")
            self.assertIn("invalid JSON", error)
            self.assertEqual(len(runner.calls), 3)

    def test_worktree_runs_this_checkouts_tool_against_the_host_checkout(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            inventory = self.write_inventory(Path(temporary), {"lab": self.profile()})
            runner = FakeRunner(
                nvx_hosts.CommandResult(0, "/tmp/nvx.2\n"),
                nvx_hosts.CommandResult(3),
                nvx_hosts.CommandResult(0),
            )
            status, _, error = self.run_cli(
                [
                    "worktree",
                    "lab",
                    "--file",
                    str(inventory),
                    "--",
                    "add",
                    "FETCH_HEAD",
                    "--fetch",
                    "topic",
                ],
                runner,
            )
            self.assertEqual(status, 3, error)
            self.assertEqual(
                sorted(_archive(runner.calls[0]).namelist()),
                ["__main__.py", "host_worktree.py", "openvmm_git.py"],
            )
            self.assertEqual(
                runner.calls[1].args,
                (
                    *SSH,
                    "-n",
                    "kvm-target",
                    "cd -- /srv/nvx && python3 /tmp/nvx.2 --repo /srv/nvx add "
                    "FETCH_HEAD --fetch topic",
                ),
            )
            self.assertEqual(
                runner.calls[2].args,
                (*SSH, "-n", "kvm-target", "rm -f -- /tmp/nvx.2"),
            )

    def test_benchmark_runs_this_checkouts_coordinator_on_the_host_checkout(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            inventory = self.write_inventory(Path(temporary), {"lab": self.profile()})
            runner = FakeRunner(
                nvx_hosts.CommandResult(0, "/tmp/nvx.3\n"),
                nvx_hosts.CommandResult(0),
                nvx_hosts.CommandResult(0),
            )
            status, _, error = self.run_cli(
                [
                    "benchmark",
                    "lab",
                    "--file",
                    str(inventory),
                    "--",
                    "--profile",
                    "smoke",
                    "--backend",
                    "kvm",
                    "--host-type",
                    "baremetal",
                ],
                runner,
            )
            self.assertEqual(status, 0, error)
            self.assertEqual(
                sorted(_archive(runner.calls[0]).namelist()),
                ["__main__.py", "benchmark_series.py", "host_probe.py"],
            )
            self.assertEqual(
                runner.calls[1].args,
                (
                    *SSH,
                    "-n",
                    "kvm-target",
                    "cd -- /srv/nvx && python3 /tmp/nvx.3 --repo /srv/nvx --profile "
                    "smoke --backend kvm --host-type baremetal",
                ),
            )

    def test_tool_archives_run_their_tools(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            for tool, expected in (
                ("host_probe", "--repo"),
                ("host_worktree", "remove"),
                ("benchmark_series", "--repo"),
            ):
                with self.subTest(tool=tool):
                    archive = Path(temporary) / f"{tool}.pyz"
                    archive.write_bytes(
                        nvx_hosts._tool_archive(tool)  # pyright: ignore[reportPrivateUsage]
                    )
                    result = subprocess.run(
                        [sys.executable, str(archive), "--help"],
                        capture_output=True,
                        text=True,
                        check=False,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn(expected, result.stdout)

    @unittest.skipUnless(os.name == "nt", "requires Windows PowerShell")
    def test_windows_tools_run_from_a_removed_temporary_file(self) -> None:
        # Runs the PowerShell that the SSH commands carry on this host, as the
        # server's OpenSSH would.
        with tempfile.TemporaryDirectory() as temporary:
            runner = LocalPowerShellRunner()
            profile = nvx_hosts.HostProfile(
                "windows", "whp-target", "whp", "virtual-machine", None, None
            )
            result = nvx_hosts._run_tool(  # pyright: ignore[reportPrivateUsage]
                profile,
                temporary,
                "host_worktree",
                ["--help"],
                runner,
                capture_stdout=True,
            )
            self.assertEqual(result.returncode, 0)
            self.assertIn("remove", result.stdout)
            self.assertEqual(len(runner.results), 3)
            self.assertFalse(Path(runner.results[0].stdout.strip()).exists())
            for completed in runner.results:
                self.assertEqual(completed.stderr, "")


if __name__ == "__main__":
    unittest.main()
