"""Block the Linux runner account from Azure instance metadata.

The runner service requires this IPv4 owner rule before every start. Updates
keep a verified block in place throughout: a pending rule protects the account
while stale and duplicate rules are replaced, and any failure stops with that
protection still installed.
"""

import os
import sys
import time
from collections.abc import Sequence
from pathlib import Path

from . import common
from .common import BootstrapError

LOCK_PATH = Path("/run/nvx-runner-metadata.lock")
LOCK_TIMEOUT_SECONDS = 30
RUNNER_ACCOUNT = "nvx-runner"
METADATA_ADDRESS = "169.254.169.254/32"
RULE_COMMENT = "nvx-runner-metadata"
PENDING_COMMENT = "nvx-runner-metadata-pending"
# A rule that a delete leaves in place would otherwise be deleted forever.
MAXIMUM_DELETIONS = 1000


def rule_exists(rule: Sequence[str]) -> bool:
    result = common.run_command(["iptables", "-w", "10", "-C", *rule], check=False)
    if result.returncode not in (0, 1):
        raise BootstrapError(
            f"could not inspect the metadata firewall rules (exit={result.returncode})"
        )
    return result.returncode == 0


def insert_rule(rule: Sequence[str]) -> None:
    common.run_command(["iptables", "-w", "10", "-I", rule[0], "1", *rule[1:]])


def delete_rules(rule: Sequence[str]) -> None:
    for _ in range(MAXIMUM_DELETIONS):
        if not rule_exists(rule):
            return
        common.run_command(["iptables", "-w", "10", "-D", *rule])
    raise BootstrapError("a metadata firewall rule survived its deletion")


def block_metadata(uid: int) -> None:
    """Install exactly one tagged block for ``uid``, never leaving it unprotected.

    The caller holds the update lock.
    """
    match = ["OUTPUT", "-d", METADATA_ADDRESS, "-m", "owner", "--uid-owner", str(uid)]
    untagged = [*match, "-j", "REJECT"]
    pending = [*match, "-m", "comment", "--comment", PENDING_COMMENT, "-j", "REJECT"]
    rule = [*match, "-m", "comment", "--comment", RULE_COMMENT, "-j", "REJECT"]
    insert_rule(pending)
    if not rule_exists(pending):
        raise BootstrapError(
            "the runner account is not blocked from Azure instance metadata"
        )
    delete_rules(untagged)
    delete_rules(rule)
    insert_rule(rule)
    if not rule_exists(rule):
        raise BootstrapError("the final metadata firewall rule was not verified")
    delete_rules(pending)


def runner_uid() -> int:
    if sys.platform == "win32":
        raise BootstrapError("The metadata firewall requires Linux")
    import pwd

    try:
        uid = pwd.getpwnam(RUNNER_ACCOUNT).pw_uid
    except KeyError as missing:
        raise BootstrapError(
            f"the {RUNNER_ACCOUNT} account does not exist"
        ) from missing
    if uid == 0:
        raise BootstrapError(f"the {RUNNER_ACCOUNT} account must not be root")
    return uid


def locked_block(uid: int, lock_path: Path = LOCK_PATH) -> None:
    """Serialize concurrent updates, as from the runner and the metadata unit."""
    if sys.platform == "win32":
        raise BootstrapError("The metadata firewall requires Linux")
    import fcntl

    previous = os.umask(0o077)
    try:
        descriptor = os.open(lock_path, os.O_WRONLY | os.O_CREAT, 0o600)
    finally:
        os.umask(previous)
    try:
        deadline = time.monotonic() + LOCK_TIMEOUT_SECONDS
        while True:
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise BootstrapError(
                        "could not acquire the metadata firewall update lock"
                    ) from None
                common.sleep(0.1)
        block_metadata(uid)
    finally:
        os.close(descriptor)


def main() -> int:
    if sys.platform == "win32":
        raise BootstrapError("The metadata firewall requires Linux")
    if os.geteuid() != 0:
        raise BootstrapError("the metadata firewall must be installed as root")
    uid = runner_uid()
    if common.which("iptables") is None:
        raise BootstrapError("the IPv4 firewall is unavailable")
    locked_block(uid)
    return 0
