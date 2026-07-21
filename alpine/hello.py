#!/usr/bin/env python3
# The boot-from-snapshot benchmark app: a small pandas/numpy program that demonstrates
# snapshot/restore.
#
# It imports the (expensive to load) pandas/numpy stack and runs the DataFrame computation once
# to warm every code path, THEN asks the active backend to take a snapshot through the shared
# /sbin/nvx-snapshot helper. On a
# snapshot run the VMM captures the VM at exactly this fully warmed point and stops; on a later
# restore, execution resumes on the next line and re-runs the computation immediately -- now
# hitting warm code and data -- skipping the kernel boot, the Python + pandas/numpy import, and
# pandas' first-use lazy initialization.
import subprocess

import pandas as pd, numpy as np


def work():
    df = pd.DataFrame({'x': np.arange(5), 'y': np.arange(5) ** 2})
    return df.sum().to_dict()


work()  # warm-up before snapshotting: exercise the pandas/numpy hot paths so they resume warm

subprocess.run(["/sbin/nvx-snapshot"], check=False)

print(work())
