#!/usr/bin/env python3
"""Snapshots the Python image, then runs the workload the host supplies.

The host filesystem is mounted and the workload is read only after the snapshot resumes, so the
snapshot itself stays workload-agnostic and every restore can be handed a different directory.
Optional warmup is explicit so baseline benchmarks can measure an untrained interpreter.
"""

import ctypes
import os
import runpy
import sys

USAGE = "usage: snapshot-trampoline.py [--mount TAG DIR MODE] [--warmup] APP"
SNAPSHOT_PORT = 0x605
MS_RDONLY = 1


def parse_args(argv: list[str]) -> tuple[tuple[str, str, str] | None, bool, str]:
    mount: tuple[str, str, str] | None = None
    warmup = False
    app: str | None = None
    index = 0
    while index < len(argv):
        argument = argv[index]
        if argument == "--mount":
            values = argv[index + 1 : index + 4]
            if len(values) != 3:
                raise SystemExit(USAGE)
            mount = (values[0], values[1], values[2])
            index += 4
            continue
        if argument == "--warmup":
            warmup = True
            index += 1
            continue
        if app is not None:
            raise SystemExit(USAGE)
        app = argument
        index += 1
    if app is None:
        raise SystemExit(USAGE)
    return mount, warmup, app


def warm_interpreter() -> None:
    import numpy
    import pandas

    values = numpy.arange(5)
    pandas.DataFrame({"x": values, "y": values**2}).sum().to_dict()


def request_snapshot() -> None:
    with open("/dev/port", "r+b", buffering=0) as device:
        device.seek(SNAPSHOT_PORT)
        device.write(b"\x01")


def mount_hostfs(tag: str, directory: str, mode: str) -> None:
    if mode not in {"ro", "rw"}:
        raise SystemExit(f"virtfs: unsupported mount mode: {mode}")
    os.makedirs(directory, exist_ok=True)
    libc = ctypes.CDLL(None, use_errno=True)
    mount = libc.mount
    mount.argtypes = [
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_ulong,
        ctypes.c_void_p,
    ]
    mount.restype = ctypes.c_int
    flags = MS_RDONLY if mode == "ro" else 0
    if mount(tag.encode(), directory.encode(), b"virtiofs", flags, None) != 0:
        error = ctypes.get_errno()
        raise SystemExit(
            f"virtfs: failed to mount tag {tag} at {directory}: {os.strerror(error)}"
        )


def main(argv: list[str]) -> None:
    mount, warmup, app = parse_args(argv)
    if warmup:
        warm_interpreter()
    request_snapshot()
    if mount is not None:
        mount_hostfs(*mount)
    runpy.run_path(app, run_name="__main__")


if __name__ == "__main__":
    main(sys.argv[1:])
