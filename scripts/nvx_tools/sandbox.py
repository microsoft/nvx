"""The live-share arguments of `nvx.py run`: its --mount values, the
aggregate of several shares, and the kernel command-line budget that they
share.

nvx-rs owns the launch profile of `nvx.py sandbox`, its shares included
(nvx-rs/README.md); `nvx.py run` builds its own OpenVMM command line.
"""

from __future__ import annotations

import hashlib
import os
from collections.abc import Sequence
from pathlib import Path

from .common import ScriptError

KERNEL_COMMAND_LINE_MAX_SIZE = 2048
# OpenVMM keeps this much of the kernel command line for its own tokens.
OPENVMM_COMMAND_LINE_RESERVE = 1024
SANDBOX_COMMAND_LINE_MAX_SIZE = (
    KERNEL_COMMAND_LINE_MAX_SIZE - OPENVMM_COMMAND_LINE_RESERVE
)
MOUNT_ACCESS_MODES = ("ro", "rw")
# OpenVMM attaches one virtio-fs device, tag `microvm`. One directory share
# uses it directly. Several shares, or a file, share it as the children of an
# aggregate, named by aggregate_child_name, which the guest mounts at
# AGGREGATE_MOUNT_TARGET, a directory that only the guest's root can enter,
# before it binds each child at its target.
MOUNT_TAG = "microvm"
AGGREGATE_MOUNT_TARGET = "/run/nvx/shares"
# `vmm` performs every share operation as OpenVMM; `caller` performs each as the
# guest caller's identity, with guest root squashed to the share owner.
MOUNT_OWNERS = ("vmm", "caller")
# The option that requests each kind of share access-policy path. `--mount-deny`
# hides a path, `--mount-allow` exposes a path inside a denied path again, and
# `--mount-write` makes a path one of the only writable parts of its share.
MOUNT_POLICY_OPTIONS = {
    "denied": "--mount-deny",
    "allowed": "--mount-allow",
    "writable": "--mount-write",
}
# OpenVMM appends exactly these virtio-fs bootstrap tokens for its share, and
# the suffix when the share is an aggregate.
_MOUNT_COMMAND_LINE_FRAGMENT = " virtfs_dir={} virtfs_tag={} virtfs_mode={}"
_AGGREGATE_COMMAND_LINE_SUFFIX = " virtfs_aggregate=1"


def parse_mount_value(value: str) -> tuple[str, str, str]:
    """Split a --mount value, GUEST_TARGET,HOST_PATH[,ro|rw], into its guest
    target, host path, and access mode, which defaults to ro.

    The guest target ends at the first comma, and with another comma, the
    mode follows the last one, so a host path may contain commas when the mode
    is given. OpenVMM reads its --mount and --mount-child values the same way.
    """
    target, separator, rest = value.partition(",")
    if not separator:
        raise ScriptError("--mount must be GUEST_TARGET,HOST_PATH[,ro|rw]")
    host_path, separator, access = rest.rpartition(",")
    if not separator:
        host_path, access = rest, "ro"
    if not host_path:
        raise ScriptError("--mount host path is empty")
    if access not in MOUNT_ACCESS_MODES:
        raise ScriptError(
            f"unsupported --mount mode {access!r}; choose ro or rw (if HOST_PATH "
            "contains commas, end the value with ,ro or ,rw)"
        )
    return target, host_path, access


def require_mount_owner_supported(owner: str) -> None:
    """Reject an ownership mode that the host cannot enforce."""
    if owner == "caller" and os.name == "nt":
        raise ScriptError(
            "--mount-owner caller requires a Linux host; Windows has no "
            "per-request POSIX identity for OpenVMM to switch to"
        )


def mount_kind(host_path: Path) -> str:
    """Return the kind of share that `host_path` requests: a file for a
    regular file, and otherwise a directory, which OpenVMM then requires."""
    return "file" if host_path.is_file() else "directory"


def aggregate_child_name(index: int, target: str) -> str:
    """Return the name of the aggregate child at `index` that the guest binds
    at `target`.

    The name holds a digest of the target. A snapshot records every child's
    name, so it pins each target, and OpenVMM refuses a restore that requests
    other targets before the guest runs, rather than keep the snapshot's binds.
    """
    digest = hashlib.sha256(target.encode()).hexdigest()[:32]
    return f"{index}-{digest}"


def aggregate_child_argument(name: str, host_path: str, access: str, kind: str) -> str:
    """Return the `--mount-child` value that attaches `host_path` as the child
    `name`. It always gives the mode, which OpenVMM reads after the last comma
    or before the `file` flag, so the host path may contain commas. A file
    child carries OpenVMM's `file` flag, so OpenVMM refuses a directory that
    replaced the file instead of exporting all of it."""
    return f"{name},{host_path},{access}" + (",file" if kind == "file" else "")


def mount_argument(target: str, host_path: str, access: str) -> str:
    """Return the `--mount` value that attaches `host_path` at `target`. It
    always gives the mode, which OpenVMM reads after the last comma, so the
    host path may contain commas."""
    return f"{target},{host_path},{access}"


def aggregate_share_tokens(shares: Sequence[tuple[str, str, str]]) -> list[str]:
    """Return the kernel command-line tokens that tell the guest where to bind
    each child of an aggregate, given each share's guest target, access, and
    kind. A file child's token ends in `,file`, so the guest binds it at a
    file."""
    return [
        f"nvx_share={aggregate_child_name(index, target)},{target},{access}"
        + (",file" if kind == "file" else "")
        for index, (target, access, kind) in enumerate(shares)
    ]


def aggregate_command_line_fragment(accesses: Sequence[str]) -> str:
    """Return the bootstrap tokens that OpenVMM appends for an aggregate of
    shares with `accesses`, which it mounts read-write if any child is."""
    access = "rw" if "rw" in accesses else "ro"
    return (
        _MOUNT_COMMAND_LINE_FRAGMENT.format(AGGREGATE_MOUNT_TARGET, MOUNT_TAG, access)
        + _AGGREGATE_COMMAND_LINE_SUFFIX
    )
