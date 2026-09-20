#!/usr/bin/env python3
"""Fail closed before Rust or VM work; write the actual in-container limits."""
import json
import os
from pathlib import Path
import sys


def inspect():
    root = Path("/sys/fs/cgroup")
    names = ("memory.max", "memory.swap.max", "cpu.max", "pids.max",
             "memory.current", "memory.peak", "memory.events")
    limits = {name: (root / name).read_text().strip() for name in names
              if (root / name).exists()}
    mounts = Path("/proc/self/mountinfo").read_text().splitlines()
    root_mount = next(line for line in mounts if line.split()[4] == "/")
    result = {
        "uid": os.getuid(), "gid": os.getgid(), "groups": os.getgroups(),
        "cgroup": limits, "root_mount": root_mount,
        "mshv_access": os.access("/dev/mshv", os.R_OK | os.W_OK),
        "docker_socket_present": Path("/var/run/docker.sock").exists(),
    }
    failures = []
    if (result["uid"], result["gid"]) != (1001, 1003):
        failures.append("requires uid1001/gid1003")
    if 998 not in result["groups"]:
        failures.append("requires MSHV supplementary group998")
    if limits.get("memory.max") != str(26 * 1024**3):
        failures.append("requires exact26GiB memory.max")
    if limits.get("memory.swap.max") != "0":
        failures.append("requires zero extra swap (26GiB memory+swap total)")
    quota, period = limits.get("cpu.max", "0 1").split()
    if quota == "max" or int(quota) != 6 * int(period):
        failures.append("requires exactly6 CPUs quota")
    if limits.get("pids.max") != "1024":
        failures.append("requires1024 PID limit")
    if "ro" not in root_mount.split()[5].split(","):
        failures.append("requires read-only rootfs")
    if not result["mshv_access"]:
        failures.append("requires readable/writable /dev/mshv; no KVM fallback")
    if result["docker_socket_present"]:
        failures.append("Docker socket must not be mounted")
    if not Path("/.dockerenv").exists():
        failures.append("requires bounded Docker container")
    result["failures"] = failures
    return result


if __name__ == "__main__":
    data = inspect()
    print(json.dumps(data, indent=2))
    sys.exit(bool(data["failures"]))
