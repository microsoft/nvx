#!/usr/bin/env python3
# Networked numpy/pandas snapshot app.
#
# Like net-hello.py, but first imports the (expensive to load) numpy/pandas stack and runs a small
# DataFrame computation to warm every hot path, so the snapshot captures a fully warmed interpreter
# *and* a live NIC. After a restore it re-runs the computation (now hitting warm code and data) and
# re-checks the link over the recreated host TAP, then prints the result -- demonstrating a heavy,
# network-connected Python service resuming in milliseconds. The shared snapshot helper is a no-op
# from the guest's perspective when the active VMM was not configured to capture.
import socket
import subprocess

import numpy as np
import pandas as pd

# Host HTTP port the benchmark's helper server listens on.
DEFAULT_PORT = 8099


def gateway():
    """The host side of the link, taken from the virtnet_gw kernel-command-line token."""
    try:
        for tok in open("/proc/cmdline").read().split():
            if tok.startswith("virtnet_gw="):
                return tok.split("=", 1)[1]
    except OSError:
        pass
    return "10.0.0.1"


def helper_port():
    """The host helper port, taken from the optional benchmark command-line token."""
    try:
        for tok in open("/proc/cmdline").read().split():
            if tok.startswith("netbench_port="):
                return int(tok.split("=", 1)[1])
    except (OSError, ValueError):
        pass
    return DEFAULT_PORT


def is_cold_measurement():
    """True when the benchmark wants one cold-path link check without taking a snapshot."""
    try:
        return "netbench_cold=1" in open("/proc/cmdline").read().split()
    except OSError:
        return False


def link_ok(host, port, timeout=3, attempts=3):
    """True if a real HTTP request to the host over the NIC round-trips."""
    for _ in range(attempts):
        try:
            with socket.create_connection((host, port), timeout) as sock:
                sock.sendall(b"GET / HTTP/1.0\r\n\r\n")
                response = bytearray()
                while len(response) < 4096:
                    chunk = sock.recv(512)
                    if not chunk:
                        break
                    response.extend(chunk)
                if b"HELLO-HOST" in response:
                    return True
        except OSError:
            pass
    return False


def work():
    df = pd.DataFrame({"x": np.arange(5), "y": np.arange(5) ** 2})
    return df.sum().to_dict()


gw = gateway()
port = helper_port()
if not is_cold_measurement():
    work()       # warm the numpy/pandas hot paths before snapshotting
    link_ok(gw, port)  # warm the link (ARP + a round-trip)
    subprocess.run(["/sbin/nvx-snapshot"], check=False)

# ---- on restore, execution resumes here ----
result = work()
ok = link_ok(gw, port)
print("PANDASPY-NET %s %s" % ("OK" if ok else "FAIL", result))
