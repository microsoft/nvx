#!/usr/bin/env python3
# The boot-from-snapshot benchmark app: a small pandas/numpy program that demonstrates
# snapshot/restore.
#
# It imports the (expensive to load) pandas/numpy stack and runs the DataFrame computation once
# to warm every code path, THEN asks the VMM to take a snapshot by writing one byte to I/O port
# 0x605 through /dev/port (a single `outb`, which the VMM intercepts as a snapshot request). On a
# snapshot run the VMM captures the VM at exactly this fully warmed point and stops; on a later
# restore, execution resumes on the next line and re-runs the computation immediately -- now
# hitting warm code and data -- skipping the kernel boot, the Python + pandas/numpy import, and
# pandas' first-use lazy initialization.
import os

import pandas as pd, numpy as np


def work():
    df = pd.DataFrame({'x': np.arange(5), 'y': np.arange(5) ** 2})
    return df.sum().to_dict()


work()  # warm-up before snapshotting: exercise the pandas/numpy hot paths so they resume warm

try:
    fd = os.open("/dev/port", os.O_WRONLY)
    os.lseek(fd, 0x605, os.SEEK_SET)
    os.write(fd, b"\x01")
    os.close(fd)
except OSError:
    pass

print(work())
