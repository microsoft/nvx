"""Linux-only supervisor for a single explicitly owned process tree."""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from nvx_tools.adversarial_oracles import supervise_linux_process


def main() -> int:
    if not sys.platform.startswith("linux"):
        raise RuntimeError("process supervisor is supported only on Linux")
    arguments = sys.argv[1:]
    parent = None
    if arguments and arguments[0] != "--":
        if not arguments[0].isdecimal():
            raise ValueError(f"invalid parent process ID: {arguments[0]!r}")
        parent = int(arguments.pop(0))
    if len(arguments) < 2 or arguments[0] != "--":
        raise ValueError("usage: process_supervisor.py [PARENT] -- COMMAND [ARG ...]")
    return supervise_linux_process(arguments[1:], parent=parent)


if __name__ == "__main__":
    raise SystemExit(main())
