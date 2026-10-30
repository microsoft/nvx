#!/usr/bin/env python3
"""Tests of how the tooling's unit tests launch PowerShell (#1030)."""

from __future__ import annotations

import ast
import contextlib
import io
import subprocess
import sys
import unittest
from pathlib import Path, PureWindowsPath
from typing import TypeGuard
from unittest.mock import call, patch

sys.path.insert(0, str(Path(__file__).parent))
from powershell_testing import POWERSHELL_TIMEOUT, run_powershell  # noqa: E402

SCRIPTS = Path(__file__).resolve().parent
# The subprocess functions that start a program.
LAUNCHERS = frozenset({"Popen", "call", "check_call", "check_output", "run"})
POWERSHELLS = frozenset({"powershell", "powershell.exe", "pwsh", "pwsh.exe"})


def _launches_powershell(node: ast.AST) -> TypeGuard[ast.Call]:
    """Whether node calls subprocess with an argument list that starts with
    PowerShell, named by a literal or by a variable."""
    if not (
        isinstance(node, ast.Call)
        and isinstance(node.func, ast.Attribute)
        and node.func.attr in LAUNCHERS
        and isinstance(node.func.value, ast.Name)
        and node.func.value.id == "subprocess"
    ):
        return False
    arguments = node.args[:1] or [
        keyword.value for keyword in node.keywords if keyword.arg == "args"
    ]
    if not arguments or not isinstance(arguments[0], (ast.List, ast.Tuple)):
        return False
    program = arguments[0].elts[0] if arguments[0].elts else None
    if isinstance(program, ast.Constant) and isinstance(program.value, str):
        return PureWindowsPath(program.value).name.lower() in POWERSHELLS
    return isinstance(program, ast.Name) and program.id.lower() in POWERSHELLS


class PowerShellLaunchTests(unittest.TestCase):
    def test_a_launch_captures_text_within_its_limit(self):
        command = ["pwsh", "-NoProfile", "-Command", "Write-Output launched"]
        completed = subprocess.CompletedProcess(command, 0, "launched\n", "")
        with patch.object(subprocess, "run", return_value=completed) as run:
            self.assertIs(
                run_powershell(command, cwd=SCRIPTS, env={"N": "1"}), completed
            )
            # A test may give a launch a limit of its own.
            self.assertIs(run_powershell(command, timeout=300), completed)

        self.assertEqual(
            run.call_args_list,
            [
                call(
                    command,
                    cwd=SCRIPTS,
                    env={"N": "1"},
                    capture_output=True,
                    text=True,
                    timeout=POWERSHELL_TIMEOUT,
                    check=False,
                ),
                call(
                    command,
                    cwd=None,
                    env=None,
                    capture_output=True,
                    text=True,
                    timeout=300,
                    check=False,
                ),
            ],
        )

    def test_a_launch_that_outlasts_its_limit_fails_with_what_it_printed(self):
        program = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
        command = [program, "-NoProfile", "-Command", "Start-Sleep 600"]
        for name, timeout, stdout, stderr, printed in (
            # On Windows, run() decodes what the child printed before the kill.
            (
                "text",
                POWERSHELL_TIMEOUT,
                "started\nwaiting\n",
                "warning\n",
                ("stdout:\nstarted\nwaiting", "stderr:\nwarning"),
            ),
            # Elsewhere, it leaves bytes, or None when nothing arrived.
            ("bytes", 300, b"started\n", None, ("stdout:\nstarted", "stderr: nothing")),
            ("nothing", 300, "", "", ("stdout: nothing", "stderr: nothing")),
        ):
            expired = subprocess.TimeoutExpired(command, timeout, stdout, stderr)
            logged = io.StringIO()
            with self.subTest(name=name):
                with (
                    patch.object(subprocess, "run", side_effect=expired),
                    contextlib.redirect_stderr(logged),
                    self.assertRaises(AssertionError) as raised,
                ):
                    run_powershell(command, timeout=timeout)

                report = str(raised.exception)
                self.assertIn(f"powershell.exe did not exit within {timeout} s", report)
                for section in printed:
                    self.assertIn(section, report)
                self.assertIs(raised.exception.__cause__, expired)
                # A job that its own time limit cancels keeps what reached stderr.
                self.assertEqual(logged.getvalue(), f"\n{report}\n")

    def test_the_tests_launch_powershell_only_through_run_powershell(self):
        for source, launches in (
            ('subprocess.run(["powershell.exe", "-Command", "exit"])', True),
            (r'subprocess.Popen(["C:\\Windows\\powershell.exe"])', True),
            ("subprocess.run(args=(powershell, '-Command', 'exit'))", True),
            ('subprocess.check_output(["pwsh", "-Command", "exit"])', True),
            ('run_powershell(["powershell.exe", "-Command", "exit"])', False),
            ('subprocess.run(["bash", "-c", "exit"])', False),
        ):
            with self.subTest(source=source):
                [statement] = ast.parse(source).body
                assert isinstance(statement, ast.Expr)
                self.assertEqual(_launches_powershell(statement.value), launches)

        # A launch outside run_powershell would get a limit of its own.
        self.assertEqual(
            [
                f"{path.name}:{node.lineno}"
                for path in sorted(SCRIPTS.glob("test_*.py"))
                for node in ast.walk(ast.parse(path.read_bytes(), path.name))
                if _launches_powershell(node)
            ],
            [],
        )


if __name__ == "__main__":
    unittest.main()
