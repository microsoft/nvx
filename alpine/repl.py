#!/usr/bin/env python3
# An *interactive* Python interpreter that is resumed from a snapshot.
#
# It warms up a full CPython interpreter, then asks the active backend to take a snapshot through
# /sbin/nvx-snapshot. On a snapshot run the VMM captures the VM at exactly this warmed point and
# stops; on a later restore, execution resumes on the next line and drops straight into an
# interactive ">>>" prompt on the console -- skipping the kernel boot and the entire
# Python startup.
import code
import os
import sys

# Warm the interpreter so these commonly used modules are already imported in the snapshot and
# are ready at the prompt the instant it is resumed.
import collections  # noqa: F401
import functools  # noqa: F401
import itertools  # noqa: F401
import json  # noqa: F401
import math  # noqa: F401
import re  # noqa: F401
import subprocess


def request_snapshot():
    """Ask the active backend to snapshot the VM at this execution boundary."""
    subprocess.run(["/sbin/nvx-snapshot"], check=False)


request_snapshot()

# Everything below runs only once the VM is resumed from the snapshot. Hand the interactive
# session a namespace that already has the warmed modules bound, so they are usable by name at
# the prompt the instant it is resumed (no `import` needed).
banner = (
    "Python %s on microvm (resumed from snapshot).\n"
    "Ready: collections, functools, itertools, json, math, os, re, sys. "
    "Ctrl-D or exit() to quit." % sys.version.split()[0]
)
namespace = {
    "__name__": "__console__",
    "__doc__": None,
    "collections": collections,
    "functools": functools,
    "itertools": itertools,
    "json": json,
    "math": math,
    "os": os,
    "re": re,
    "sys": sys,
}
code.interact(banner=banner, local=namespace, exitmsg="")
