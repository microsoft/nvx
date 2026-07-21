#!/usr/bin/env python3
# Networked hello-world snapshot app.
#
# It proves the guest NIC survives snapshot/restore without any heavy imports, so it measures
# resuming a bare CPython interpreter that has a *live* link. It warms the link (so ARP for the
# gateway is populated when the snapshot is taken), asks the backend through /sbin/nvx-snapshot,
# and -- after a later restore, where execution resumes on the
# very next line -- re-checks the link over the freshly recreated host TAP and prints a marker the
# benchmark greps for. On a plain cold boot (no --snapshot) the port write is ignored and the app
# simply runs straight through.
import os
import signal
import socket
import subprocess
import time

# Host HTTP port the benchmark's helper server listens on.
DEFAULT_PORT = 8099


def cmdline_value(name):
    """Return one kernel-command-line value, or None when it is absent."""
    try:
        for tok in open("/proc/cmdline").read().split():
            if tok.startswith(name + "="):
                return tok.split("=", 1)[1]
    except OSError:
        pass
    return None


def target_host():
    """The benchmark helper address, falling back to the network gateway."""
    return cmdline_value("netbench_host") or cmdline_value("virtnet_gw") or "10.0.0.1"


def helper_port():
    """The host helper port, taken from the optional benchmark command-line token."""
    try:
        value = cmdline_value("netbench_port")
        if value is not None:
            return int(value)
    except ValueError:
        pass
    return DEFAULT_PORT


def is_cold_measurement():
    """True when the benchmark wants one cold-path link check without taking a snapshot."""
    try:
        return "netbench_cold=1" in open("/proc/cmdline").read().split()
    except OSError:
        return False


def host_controls_shutdown():
    """True when the host will terminate this PID 1 after observing the marker."""
    try:
        return "netbench_hold=1" in open("/proc/cmdline").read().split()
    except OSError:
        return False


def run_quiet(args):
    """Run one best-effort BusyBox networking command."""
    return subprocess.run(
        args,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    ).returncode == 0


def restore_hcs_network(timeout=5):
    """Reapply guest addressing after HCS recreates the external NetVSC adapter."""
    expected_mac = (cmdline_value("virtnet_mac") or "").lower().replace(":", "").replace("-", "")
    deadline = time.monotonic() + timeout
    interface = None
    while time.monotonic() < deadline and interface is None:
        try:
            names = [name for name in os.listdir("/sys/class/net") if name != "lo"]
        except OSError:
            names = []
        for name in names:
            try:
                observed_mac = open(f"/sys/class/net/{name}/address").read().strip()
            except OSError:
                continue
            normalized_mac = observed_mac.lower().replace(":", "").replace("-", "")
            if not expected_mac or normalized_mac == expected_mac:
                interface = name
                break
        if interface is None:
            time.sleep(0.05)

    address = cmdline_value("virtnet_ip")
    netmask = cmdline_value("virtnet_mask")
    gateway = cmdline_value("virtnet_gw")
    if interface is None or not address or not netmask or not gateway:
        return False

    mtu = cmdline_value("virtnet_mtu")
    if mtu and not run_quiet(["ifconfig", interface, "mtu", mtu]):
        return False
    if not run_quiet(["ifconfig", interface, address, "netmask", netmask, "up"]):
        return False
    run_quiet(["route", "del", "default", "dev", interface])
    run_quiet(["route", "del", "-host", gateway, "dev", interface])
    if not run_quiet(["route", "add", "-host", gateway, "dev", interface]):
        return False
    if not run_quiet(["route", "add", "default", "gw", gateway, "dev", interface]):
        return False

    carrier = f"/sys/class/net/{interface}/carrier"
    while time.monotonic() < deadline:
        try:
            if open(carrier).read().strip() == "1":
                return True
        except OSError:
            pass
        time.sleep(0.05)
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


gw = target_host()
port = helper_port()
print(f"HELLOPY-NET TARGET={gw}:{port}", flush=True)
if not is_cold_measurement():
    if not link_ok(gw, port):
        print("HELLOPY-NET PRECAPTURE-FAIL", flush=True)
        subprocess.run(["reboot", "-f"], check=False)
        raise SystemExit(1)
    print("HELLOPY-NET PRECAPTURE-OK")
    subprocess.run(["/sbin/nvx-snapshot"], check=True)
    if host_controls_shutdown():
        print(
            "HELLOPY-NET RECONFIGURE-" + ("OK" if restore_hcs_network() else "FAIL"),
            flush=True,
        )

# ---- on restore, execution resumes here ----
ok = link_ok(gw, port)
print("hello world")
print("HELLOPY-NET " + ("OK" if ok else "FAIL"), flush=True)
print("NVX-HCS-NETWORK-DONE", flush=True)
if not ok:
    subprocess.run(["reboot", "-f"], check=False)
    raise SystemExit(1)
if host_controls_shutdown():
    while True:
        signal.pause()
