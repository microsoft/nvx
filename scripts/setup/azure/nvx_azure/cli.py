"""Command-line entry point of the Azure runner bootstrap.

Guests run it from the bootstrap directory, which holds a zipapp-style
``__main__.py`` and this package, as ``python -I -B <directory> <command>``.
Each command imports only its platform's modules, so a guest needs only them.
"""

import argparse
import traceback
from collections.abc import Sequence
from pathlib import Path

from .common import BootstrapError, fail

LINUX_CONFIG = Path("/opt/nvx-bootstrap/config.json")


def parser() -> argparse.ArgumentParser:
    commands = argparse.ArgumentParser(
        prog="nvx-azure-bootstrap", description="Bootstrap an NVX CI runner on Azure."
    )
    subcommands = commands.add_subparsers(dest="command", required=True)
    for name, summary in (
        ("linux-bootstrap", "Provision this Linux instance as a CI runner."),
        ("linux-refresh-metadata", "Refresh the compute snapshot before runner start."),
    ):
        command = subcommands.add_parser(name, help=summary, description=summary)
        command.add_argument("--config", type=Path, default=LINUX_CONFIG)
    for name, summary in (
        ("linux-block-metadata", "Block the runner account from instance metadata."),
        (
            "linux-verify-mshv",
            "Report whether this VM booted as the MSHV root partition.",
        ),
    ):
        subcommands.add_parser(name, help=summary, description=summary)
    return commands


def run(arguments: argparse.Namespace) -> int:
    command: str = arguments.command
    if command == "linux-bootstrap":
        from . import linux_bootstrap

        return linux_bootstrap.bootstrap_main(arguments.config)
    if command == "linux-refresh-metadata":
        from . import linux_bootstrap

        return linux_bootstrap.refresh_main(arguments.config)
    if command == "linux-block-metadata":
        from . import linux_firewall

        return linux_firewall.main()
    from . import linux_mshv

    return 0 if linux_mshv.verify() else 1


def main(argv: Sequence[str] | None = None) -> int:
    arguments = parser().parse_args(argv)
    try:
        return run(arguments)
    except BootstrapError as error:
        return fail(str(error))
    except Exception as error:  # Unexpected failures keep their traceback.
        traceback.print_exc()
        return fail(f"{type(error).__name__}: {error}")
