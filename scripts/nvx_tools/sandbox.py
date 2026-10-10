"""Validated host-side launch contract for the experimental sandbox profile."""

from __future__ import annotations

import hashlib
import os
import re
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from uuid import UUID

from .common import ScriptError, require_file

LAYER_ROLES = ("distro", "runtime", "custom")
BLOCK_MMIO_BASES = {
    "distro": 0xD000_3000,
    "runtime": 0xD000_4000,
    "custom": 0xD000_5000,
    "scratch": 0xD000_6000,
}
KERNEL_COMMAND_LINE_MAX_SIZE = 2048
OPENVMM_COMMAND_LINE_RESERVE = 1024
SANDBOX_COMMAND_LINE_MAX_SIZE = (
    KERNEL_COMMAND_LINE_MAX_SIZE - OPENVMM_COMMAND_LINE_RESERVE
)
_HOSTNAME = re.compile(r"(?=^.{1,63}$)(?!-)[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$")
DEFAULT_WORKLOAD_IDENTITY = (65534, 65534)
MOUNT_ACCESS_MODES = ("ro", "rw")
# OpenVMM attaches one virtio-fs device, tag `microvm`. One share uses it
# directly. Several share it as the children of an aggregate, named by
# aggregate_child_name, which the guest mounts at AGGREGATE_MOUNT_TARGET, a
# directory that only the guest's root can enter, before it binds each child at
# its target.
MOUNT_TAG = "microvm"
AGGREGATE_MOUNT_TARGET = "/run/nvx/shares"
# `vmm` performs every share operation as OpenVMM; `caller` performs each as the
# guest caller's identity, with guest root squashed to the share owner.
MOUNT_OWNERS = ("vmm", "caller")
# The container runtime owns these guest paths: it mounts procfs, sysfs, and a
# private /dev over them, stages its tools under /.nvx-agent, and bind-mounts the
# workload machine ID into /etc. A live share must not shadow or be shadowed by
# them.
RESERVED_MOUNT_TARGETS = ("/proc", "/sys", "/dev", "/.nvx-agent")
RESERVED_EXACT_MOUNT_TARGETS = ("/etc",)
# OpenVMM accepts at most this many denied, allowed, and writable paths each.
MAX_MOUNT_POLICY_PATHS = 128
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


def parse_workload_identity(value: str) -> tuple[int, int]:
    fields = value.split(":")
    if len(fields) != 2:
        raise ScriptError("--workload-user must be UID:GID")
    try:
        uid, gid = (int(field, 10) for field in fields)
    except ValueError as error:
        raise ScriptError("--workload-user must contain numeric UID and GID") from error
    if not 1 <= uid <= 0xFFFF_FFFF:
        raise ScriptError("--workload-user UID must be between 1 and 4294967295")
    if not 1 <= gid <= 0xFFFF_FFFF:
        raise ScriptError("--workload-user GID must be between 1 and 4294967295")
    return uid, gid


@dataclass(frozen=True)
class SandboxLayer:
    role: str
    path: Path
    uuid: str

    @classmethod
    def parse(cls, value: str) -> SandboxLayer:
        fields = value.split(",")
        if len(fields) != 3:
            raise ScriptError("--layer must be ROLE,PATH,EROFS_UUID")
        role, raw_path, raw_uuid = fields
        if role not in LAYER_ROLES:
            raise ScriptError(
                f"unsupported layer role {role!r}; choose {', '.join(LAYER_ROLES)}"
            )
        if not raw_path:
            raise ScriptError(f"{role} layer path is empty")
        try:
            uuid = str(UUID(raw_uuid))
        except ValueError as error:
            raise ScriptError(f"{role} layer UUID is invalid: {raw_uuid!r}") from error
        return cls(role=role, path=Path(raw_path), uuid=uuid)


def validate_mount_target(target: str) -> str:
    """Validate a live-share guest target inside the sandbox container rootfs."""
    if (
        not target.startswith("/")
        or target == "/"
        or len(target.encode("utf-8")) > 4096
        or any(character.isspace() or character in "\0\\=," for character in target)
        or any(component in ("", ".", "..") for component in target.split("/")[1:])
    ):
        raise ScriptError(
            f"invalid sandbox mount target {target!r}: expected an absolute non-root "
            "path without empty, dot, or parent components, whitespace, '\\', '=', "
            "or ','"
        )
    for reserved in RESERVED_MOUNT_TARGETS:
        if target == reserved or target.startswith(f"{reserved}/"):
            raise ScriptError(
                f"sandbox mount target {target} overlaps the reserved {reserved} tree"
            )
    if target in RESERVED_EXACT_MOUNT_TARGETS:
        raise ScriptError(f"sandbox mount target {target} is reserved")
    return target


def require_mount_owner_supported(owner: str) -> None:
    """Reject an ownership mode that the host cannot enforce."""
    if owner == "caller" and os.name == "nt":
        raise ScriptError(
            "--mount-owner caller requires a Linux host; Windows has no "
            "per-request POSIX identity for OpenVMM to switch to"
        )


@dataclass(frozen=True)
class SandboxMount:
    """A live virtio-fs share mounted inside the sandbox container rootfs.

    `denied_paths` hide paths in the share, `allowed_paths` expose paths
    inside denied paths again, and `writable_paths`, when present, are the
    only paths of a read-write share that the workload can modify.
    """

    guest_target: str
    host_path: Path
    access: str = "ro"
    denied_paths: tuple[str, ...] = ()
    owner: str = "vmm"
    allowed_paths: tuple[str, ...] = ()
    writable_paths: tuple[str, ...] = ()

    @classmethod
    def parse(
        cls,
        value: str,
        denied_paths: tuple[str, ...] = (),
        owner: str = "vmm",
        *,
        allowed_paths: tuple[str, ...] = (),
        writable_paths: tuple[str, ...] = (),
    ) -> SandboxMount:
        fields = value.split(",")
        if len(fields) not in (2, 3):
            raise ScriptError("--mount must be GUEST_TARGET,HOST_PATH[,ro|rw]")
        guest_target, raw_path = fields[:2]
        if not raw_path:
            raise ScriptError("sandbox mount host path is empty")
        access = fields[2] if len(fields) == 3 else "ro"
        return cls(
            guest_target=guest_target,
            host_path=Path(raw_path),
            access=access,
            denied_paths=denied_paths,
            owner=owner,
            allowed_paths=allowed_paths,
            writable_paths=writable_paths,
        )

    def __post_init__(self) -> None:
        validate_mount_target(self.guest_target)
        if self.access not in MOUNT_ACCESS_MODES:
            raise ScriptError(
                f"unsupported sandbox mount mode {self.access!r}; choose ro or rw"
            )
        if self.owner not in MOUNT_OWNERS:
            raise ScriptError(
                f"unsupported sandbox mount owner {self.owner!r}; choose vmm or caller"
            )
        raw_path = os.fspath(self.host_path)
        if any(character in raw_path for character in ",\0"):
            raise ScriptError(
                f"sandbox mount host paths containing commas are unsupported: {raw_path}"
            )
        if ".." in self.host_path.parts:
            raise ScriptError(
                f"sandbox mount host path contains a parent component: {raw_path}"
            )
        for kind, paths in self.policy_paths():
            if len(paths) > MAX_MOUNT_POLICY_PATHS:
                raise ScriptError(
                    f"a sandbox mount permits at most {MAX_MOUNT_POLICY_PATHS} "
                    f"{kind} paths"
                )
            for path in paths:
                if not path or "\0" in path:
                    raise ScriptError(f"sandbox mount {kind} paths must be nonempty")
            if len(set(paths)) != len(paths):
                raise ScriptError(f"sandbox mount {kind} paths must be unique")
        if self.allowed_paths and not self.denied_paths:
            raise ScriptError(
                "--mount-allow exposes a path inside a --mount-deny path of the "
                "same share, which has none"
            )
        if self.writable_paths and self.access != "rw":
            raise ScriptError(
                "--mount-write narrows the writes of a read-write share; "
                f"--mount {self.guest_target} is read-only"
            )

    def policy_paths(self) -> tuple[tuple[str, tuple[str, ...]], ...]:
        """Return each kind of access-policy path with its requested paths."""
        return (
            ("denied", self.denied_paths),
            ("allowed", self.allowed_paths),
            ("writable", self.writable_paths),
        )

    def validated(self) -> SandboxMount:
        require_mount_owner_supported(self.owner)
        if self.host_path.is_symlink() or not self.host_path.is_dir():
            raise ScriptError(
                f"sandbox mount host path is not a plain directory: {self.host_path}"
            )
        if self.owner == "caller" and os.name != "nt":
            status = self.host_path.stat()
            if status.st_uid == 0 or status.st_gid == 0:
                raise ScriptError(
                    "--mount-owner caller squashes guest root to the owner of the "
                    f"shared directory, so {self.host_path} must not be owned by "
                    "UID 0 or GID 0"
                )
        return self

    def absolute(self) -> SandboxMount:
        # Join instead of normalizing so OpenVMM still sees, and rejects, every
        # symbolic-link component of the requested path.
        return SandboxMount(
            guest_target=self.guest_target,
            host_path=(
                self.host_path
                if self.host_path.is_absolute()
                else Path.cwd() / self.host_path
            ),
            access=self.access,
            denied_paths=self.denied_paths,
            owner=self.owner,
            allowed_paths=self.allowed_paths,
            writable_paths=self.writable_paths,
        )

    def _absolute_paths(self, paths: tuple[str, ...]) -> tuple[str, ...]:
        host_path = self.absolute().host_path
        return tuple(
            path if Path(path).is_absolute() else os.fspath(host_path / path)
            for path in paths
        )

    def absolute_policy_paths(self) -> tuple[tuple[str, tuple[str, ...]], ...]:
        """Return each kind of access-policy path, with relative paths joined
        to the share."""
        return tuple(
            (kind, self._absolute_paths(paths)) for kind, paths in self.policy_paths()
        )

    def command_line_fragment(self) -> str:
        return _MOUNT_COMMAND_LINE_FRAGMENT.format(
            self.guest_target, MOUNT_TAG, self.access
        )


def _targets_overlap(left: str, right: str) -> bool:
    return left == right or left.startswith(f"{right}/") or right.startswith(f"{left}/")


def validate_mounts(mounts: tuple[SandboxMount, ...]) -> None:
    """Validate the live shares that one sandbox attaches, in child order."""
    for index, mount in enumerate(mounts):
        for other in mounts[:index]:
            if _targets_overlap(other.guest_target, mount.guest_target):
                raise ScriptError(
                    f"sandbox mount targets {other.guest_target} and "
                    f"{mount.guest_target} overlap; a share cannot hide another"
                )
    if len({mount.owner for mount in mounts}) > 1:
        raise ScriptError("every sandbox --mount must use the same --mount-owner")


def _directory_identity(path: Path) -> tuple[int, int]:
    status = path.stat()
    return status.st_dev, status.st_ino


def validate_mount_host_paths(mounts: tuple[SandboxMount, ...]) -> None:
    """Reject shares whose host directories overlap, or deny another's paths."""
    if len(mounts) < 2:
        return
    roots = [mount.host_path.resolve() for mount in mounts]
    # Resolving follows symbolic links but not bind mounts, so also compare the
    # file identities that OpenVMM pins for each share: a root with the identity
    # of another root, or of one of its ancestors, is that directory or lies
    # inside a bind mount of it. On Linux, OpenVMM also compares the mount
    # sources that each share reaches, which catches a bind mount that exposes
    # part of one share inside the other.
    lineages = [
        [_directory_identity(path) for path in (root, *root.parents)] for root in roots
    ]
    for index, root in enumerate(roots):
        for other_index, other in enumerate(roots[:index]):
            if root == other or other in root.parents or root in other.parents:
                raise ScriptError(
                    f"sandbox mount host directories {other} and {root} overlap; "
                    "a share could reach files of another under its access mode"
                )
            if (
                lineages[index][0] in lineages[other_index]
                or lineages[other_index][0] in lineages[index]
            ):
                raise ScriptError(
                    f"sandbox mount host directories {other} and {root} are the "
                    "same directory, or one is inside the other through a bind mount"
                )
    for mount, root in zip(mounts, roots, strict=True):
        for kind, paths in mount.absolute_policy_paths():
            option = MOUNT_POLICY_OPTIONS[kind]
            for path in paths:
                resolved = Path(path).resolve()
                # A denied root hides the share behind its allowed paths, which
                # OpenVMM requires; it accepts no other policy path at a root.
                if root in resolved.parents or (kind == "denied" and resolved == root):
                    continue
                raise ScriptError(
                    f"{option} {path} is not inside the host directory of "
                    f"--mount {mount.guest_target}; with several shares, each "
                    f"{option} follows the --mount whose directory it names"
                )


def aggregate_child_name(index: int, target: str) -> str:
    """Return the name of the aggregate child at `index` that the guest binds
    at `target`.

    The name holds a digest of the target. A snapshot records every child's
    name, so it pins each target, and OpenVMM refuses a restore that requests
    other targets before the guest runs, rather than keep the snapshot's binds.
    """
    digest = hashlib.sha256(target.encode()).hexdigest()[:32]
    return f"{index}-{digest}"


def mounts_openvmm_arguments(mounts: tuple[SandboxMount, ...]) -> list[str]:
    """Return the OpenVMM arguments that attach `mounts`: one share directly,
    and several as the children of an aggregate, in order."""
    arguments: list[str] = []
    if len(mounts) == 1:
        mount = mounts[0]
        arguments.extend(
            (
                "--mount",
                f"{mount.guest_target},{os.fspath(mount.host_path)},{mount.access}",
            )
        )
    elif mounts:
        arguments.extend(("--mount-aggregate", AGGREGATE_MOUNT_TARGET))
        for index, mount in enumerate(mounts):
            name = aggregate_child_name(index, mount.guest_target)
            arguments.extend(
                (
                    "--mount-child",
                    f"{name},{os.fspath(mount.host_path)},{mount.access}",
                )
            )
    for mount in mounts:
        # OpenVMM resolves a relative policy path in the only share, and
        # attributes absolute policy paths to the child that contains them.
        policy_paths = (
            mount.policy_paths() if len(mounts) == 1 else mount.absolute_policy_paths()
        )
        for kind, paths in policy_paths:
            for path in paths:
                arguments.extend((MOUNT_POLICY_OPTIONS[kind], path))
    if mounts and mounts[0].owner != "vmm":
        arguments.extend(("--mount-owner", mounts[0].owner))
    return arguments


def aggregate_share_tokens(shares: Sequence[tuple[str, str]]) -> list[str]:
    """Return the kernel command-line tokens that tell the guest where to bind
    each child of an aggregate, given each share's guest target and access, or
    none for a single share."""
    if len(shares) < 2:
        return []
    return [
        f"nvx_share={aggregate_child_name(index, target)},{target},{access}"
        for index, (target, access) in enumerate(shares)
    ]


def aggregate_command_line_fragment(accesses: Sequence[str]) -> str:
    """Return the bootstrap tokens that OpenVMM appends for an aggregate of
    shares with `accesses`, which it mounts read-write if any child is."""
    access = "rw" if "rw" in accesses else "ro"
    return (
        _MOUNT_COMMAND_LINE_FRAGMENT.format(AGGREGATE_MOUNT_TARGET, MOUNT_TAG, access)
        + _AGGREGATE_COMMAND_LINE_SUFFIX
    )


def mounts_command_line_fragment(mounts: tuple[SandboxMount, ...]) -> str:
    """Return the bootstrap tokens that OpenVMM appends for `mounts`."""
    if len(mounts) == 1:
        return mounts[0].command_line_fragment()
    if not mounts:
        return ""
    return aggregate_command_line_fragment([mount.access for mount in mounts])


@dataclass(frozen=True)
class SandboxLaunch:
    layers: tuple[SandboxLayer, ...]
    scratch: Path
    entrypoint: str = "/bin/sh"
    args: tuple[str, ...] = ()
    hostname: str = "nvx-sandbox"
    workload_identity: tuple[int, int] = DEFAULT_WORKLOAD_IDENTITY
    memory_max: int | None = None
    pids_max: int | None = None
    mounts: tuple[SandboxMount, ...] = ()

    def __post_init__(self) -> None:
        if not 1 <= len(self.layers) <= len(LAYER_ROLES):
            raise ScriptError("a sandbox requires one to three read-only layers")
        roles = [layer.role for layer in self.layers]
        duplicates = sorted({role for role in roles if roles.count(role) > 1})
        if duplicates:
            raise ScriptError(
                f"duplicate sandbox layer role(s): {', '.join(duplicates)}"
            )
        if not self.entrypoint.startswith("/") or any(
            character.isspace() for character in self.entrypoint
        ):
            raise ScriptError(
                "sandbox entrypoint must be an absolute path without spaces"
            )
        for argument in self.args:
            if not argument or any(character.isspace() for character in argument):
                raise ScriptError(
                    "sandbox arguments must be nonempty and contain no spaces"
                )
        if _HOSTNAME.fullmatch(self.hostname) is None:
            raise ScriptError(
                "sandbox hostname must be a lowercase RFC 1123 label up to 63 characters"
            )
        uid, gid = self.workload_identity
        if not 1 <= uid <= 0xFFFF_FFFF or not 1 <= gid <= 0xFFFF_FFFF:
            raise ScriptError("sandbox workload identity must be a non-root UID:GID")
        for name, value in (
            ("memory-max", self.memory_max),
            ("pids-max", self.pids_max),
        ):
            if value is not None and value <= 0:
                raise ScriptError(f"--{name} must be positive")
        validate_mounts(self.mounts)

    def validated(self) -> SandboxLaunch:
        for layer in self.layers:
            require_file(layer.path, f"{layer.role} EROFS layer")
            _reject_disk_path(layer.path)
        require_file(self.scratch, "ext4 scratch image")
        _reject_disk_path(self.scratch)
        for mount in self.mounts:
            mount.validated()
        validate_mount_host_paths(self.mounts)
        return self

    def ordered_layers(self) -> tuple[SandboxLayer, ...]:
        by_role = {layer.role: layer for layer in self.layers}
        return tuple(by_role[role] for role in LAYER_ROLES if role in by_role)

    def openvmm_arguments(self) -> list[str]:
        arguments = ["--machine", "microvm"]
        for layer in self.ordered_layers():
            arguments.extend(
                (
                    "--microvm-sandbox-block",
                    f"{layer.role}:file:{os.fspath(layer.path)},ro",
                )
            )
        arguments.extend(
            (
                "--microvm-sandbox-block",
                f"scratch:file:{os.fspath(self.scratch)}",
                "--microvm-workload-identity",
                f"{self.workload_identity[0]}:{self.workload_identity[1]}",
            )
        )
        arguments.extend(mounts_openvmm_arguments(self.mounts))
        return arguments

    def kernel_command_line(self, user_command_line: str = "") -> str:
        if "\0" in user_command_line:
            raise ScriptError("kernel command line contains an embedded NUL")
        for token in user_command_line.split():
            if token.startswith("nvx_"):
                raise ScriptError(
                    f"{token.split('=', 1)[0]} is owned by the sandbox profile"
                )
            if token.startswith("virtfs_"):
                raise ScriptError(
                    f"{token.split('=', 1)[0]} is owned by the sandbox --mount option"
                )
        tokens = [user_command_line.strip(), "nvx_sandbox=1"]
        for layer in self.ordered_layers():
            tokens.append(
                "nvx_layer="
                f"{layer.role},0x{BLOCK_MMIO_BASES[layer.role]:x},{layer.uuid}"
            )
        tokens.extend(
            (
                f"nvx_scratch=0x{BLOCK_MMIO_BASES['scratch']:x},ext4",
                f"nvx_entrypoint={self.entrypoint}",
                f"nvx_hostname={self.hostname}",
            )
        )
        tokens.extend(f"nvx_arg={argument}" for argument in self.args)
        if self.memory_max is not None:
            tokens.append(f"nvx_memory_max={self.memory_max}")
        if self.pids_max is not None:
            tokens.append(f"nvx_pids_max={self.pids_max + 1}")
        tokens.extend(
            aggregate_share_tokens(
                [(mount.guest_target, mount.access) for mount in self.mounts]
            )
        )
        command_line = " ".join(token for token in tokens if token)
        # OpenVMM appends the live-share bootstrap tokens after this command line,
        # so they consume the same x86 budget.
        mount_fragment = mounts_command_line_fragment(self.mounts)
        if (
            len(command_line.encode("utf-8")) + len(mount_fragment.encode("utf-8")) + 1
            > SANDBOX_COMMAND_LINE_MAX_SIZE
        ):
            raise ScriptError(
                "sandbox kernel command line exceeds its 1024-byte x86 budget; "
                "attach fewer --mount shares or use shorter targets and arguments"
            )
        return command_line


def _reject_disk_path(path: Path) -> None:
    value = os.fspath(path)
    if any(character in value for character in ",;"):
        raise ScriptError(
            f"disk paths containing commas or semicolons are unsupported: {value}"
        )
