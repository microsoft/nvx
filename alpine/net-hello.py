#!/usr/bin/env python3
# Networked hello-world snapshot app.
#
# It proves the guest NIC survives snapshot/restore without any heavy imports, so it measures
# resuming a bare CPython interpreter that has a *live* link. It warms the link (so ARP for the
# gateway is populated when the snapshot is taken), asks the VMM for a snapshot with a single outb
# to I/O port 0x605 via /dev/port, and -- after a later restore, where execution resumes on the
# very next line -- re-checks the link over the freshly recreated host TAP and prints a marker the
# benchmark greps for. On a plain cold boot (no --snapshot) the port write is ignored and the app
# simply runs straight through.
import os
import socket

# Host HTTP port the benchmark's helper server listens on (see scripts/bench-net-snapshot-py.sh).
PORT = 8099


def gateway():
    """The host side of the link, taken from the virtnet_gw kernel-command-line token."""
    try:
        for tok in open("/proc/cmdline").read().split():
            if tok.startswith("virtnet_gw="):
                return tok.split("=", 1)[1]
    except OSError:
        pass
    return "10.0.0.1"


def link_ok(host, port=PORT, timeout=3):
    """True if a real HTTP request to the host over the NIC round-trips."""
    try:
        s = socket.create_connection((host, port), timeout)
        s.sendall(b"GET / HTTP/1.0\r\n\r\n")
        ok = b"HELLO-HOST" in s.recv(256)
        s.close()
        return ok
    except OSError:
        return False


gw = gateway()
link_ok(gw)  # warm the link (ARP + a round-trip) before snapshotting

try:
    fd = os.open("/dev/port", os.O_WRONLY)
    os.lseek(fd, 0x605, os.SEEK_SET)
    os.write(fd, b"\x01")
    os.close(fd)
except OSError:
    pass

# ---- on restore, execution resumes here ----
ok = link_ok(gw)
print("hello world")
print("HELLOPY-NET " + ("OK" if ok else "FAIL"))
