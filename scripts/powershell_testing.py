"""Launches PowerShell for the tooling's unit tests.

The test_*.py modules run the PowerShell of CI steps and setup scripts in
Windows PowerShell, and in PowerShell 7 where it is installed. A launch takes
well under a second on a workstation and a few seconds on a GitHub-hosted
runner, but a hosted runner has held one up for more than 30 s (#1030). So
every launch gets one default limit, which stops only a script that hangs; a
test whose script may take longer passes a limit of its own. A launch that
reaches its limit fails its test with what PowerShell printed until it was
killed, which shows how far it got. unittest reports a failure only once the
whole run ends, which a CI job that its own time limit cancels never reaches,
so the report also goes to stderr as soon as the launch is killed.
"""

from __future__ import annotations

import subprocess
import sys
from collections.abc import Mapping, Sequence
from pathlib import Path, PureWindowsPath

# The seconds that a PowerShell launch gets unless its test passes its own.
POWERSHELL_TIMEOUT = 120


def run_powershell(
    command: Sequence[str],
    *,
    cwd: Path | None = None,
    env: Mapping[str, str] | None = None,
    timeout: float = POWERSHELL_TIMEOUT,
) -> subprocess.CompletedProcess[str]:
    """Runs a PowerShell command line and captures its output as text.

    A launch that outlasts timeout writes what it printed until it was killed
    to stderr, then raises an AssertionError that holds the same report.
    """
    try:
        return subprocess.run(
            command,
            cwd=cwd,
            env=env,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired as error:
        report = _timeout_report(command[0], error)
        print(f"\n{report}", file=sys.stderr, flush=True)
        raise AssertionError(report) from error


def _captured_text(output: str | bytes | None) -> str:
    # run() leaves a killed child's output undecoded except on Windows.
    if isinstance(output, bytes):
        return output.decode("utf-8", errors="replace")
    return output or ""


def _timeout_report(program: str, error: subprocess.TimeoutExpired) -> str:
    lines = [
        f"{PureWindowsPath(program).name} did not exit within {error.timeout:g} s;"
        " until it was killed, it printed:"
    ]
    for name, output in (("stdout", error.stdout), ("stderr", error.stderr)):
        text = _captured_text(output).rstrip()
        lines.append(f"{name}:\n{text}" if text else f"{name}: nothing")
    return "\n".join(lines)
