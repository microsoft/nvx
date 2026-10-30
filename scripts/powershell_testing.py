"""Launches PowerShell for the tooling's unit tests.

The test_*.py modules run the PowerShell of CI steps and setup scripts in
Windows PowerShell, and in PowerShell 7 where it is installed. A launch takes
well under a second on a workstation and a few seconds on a GitHub-hosted
runner, but a hosted runner has held one up for more than 30 s (#1030). So
every launch gets one default limit, which stops only a script that hangs; a
test whose script may take longer passes a limit of its own.
"""

from __future__ import annotations

import subprocess
from collections.abc import Mapping, Sequence
from pathlib import Path

# The seconds that a PowerShell launch gets unless its test passes its own.
POWERSHELL_TIMEOUT = 120


def run_powershell(
    command: Sequence[str],
    *,
    cwd: Path | None = None,
    env: Mapping[str, str] | None = None,
    timeout: float = POWERSHELL_TIMEOUT,
) -> subprocess.CompletedProcess[str]:
    """Runs a PowerShell command line and captures its output as text."""
    return subprocess.run(
        command,
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )
