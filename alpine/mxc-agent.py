#!/usr/bin/python3
"""Snapshot-resident MXC workload agent for NVX."""

import fcntl
import json
import os
import sys
import time
import traceback
import tty

SNAPSHOT_PORT = 0x605
EXIT_PORT = 0x604
OUTPUT_MARKER = "NVX-EXEC-START"
MAX_PAYLOAD = 16 * 1024 * 1024
ENTROPY_BYTES = 32
RNDRESEEDCRNG = 0x5207


def write_port(port, value):
    with open("/dev/port", "r+b", buffering=0) as device:
        device.seek(port)
        device.write(bytes([value]))


def read_exact(stream, size):
    chunks = bytearray()
    while len(chunks) < size:
        chunk = stream.read(size - len(chunks))
        if not chunk:
            raise EOFError("host closed the workload channel")
        chunks.extend(chunk)
    return bytes(chunks)


def exit_code(system_exit, stderr):
    code = system_exit.code
    if code is None:
        return 0
    if isinstance(code, int):
        return code & 0xFF
    try:
        print(code, file=stderr)
    except (BrokenPipeError, OSError, ValueError):
        pass
    return 1


def print_traceback(stderr):
    try:
        traceback.print_exc(file=stderr)
    except (BrokenPipeError, OSError, ValueError):
        pass


def configure_raw_input(stream):
    # stdout/stderr share this tty, so raw mode also drops output newline translation.
    tty.setraw(stream.fileno())


def reseed_random(entropy):
    if not isinstance(entropy, list) or len(entropy) != ENTROPY_BYTES:
        raise ValueError("invalid host entropy payload")
    seed = bytes(entropy)
    with open("/dev/urandom", "wb", buffering=0) as random:
        random.write(seed)
        fcntl.ioctl(random, RNDRESEEDCRNG)


def set_realtime(unix_time_ns):
    if not isinstance(unix_time_ns, int) or unix_time_ns < 0:
        raise ValueError("invalid host time payload")
    time.clock_settime_ns(time.CLOCK_REALTIME, unix_time_ns)


def run_request(request, stderr):
    sys.argv = ["<mxc>"]
    namespace = {"__name__": "__main__", "__file__": "<mxc>"}
    try:
        for entry in request.get("env", []):
            name, value = entry.split("=", 1)
            os.environ[name] = value
        exec(compile(request["script"], "<mxc>", "exec"), namespace, namespace)
        return 0
    except SystemExit as error:
        return exit_code(error, stderr)
    except BaseException:
        print_traceback(stderr)
        return 1


def wait_for_child(child):
    while True:
        try:
            return os.waitpid(child, 0)[1]
        except InterruptedError:
            continue


def run_workload(request, stdout, stderr):
    child = os.fork()
    if child == 0:
        status = run_request(request, sys.stderr)
        flush(stdout)
        flush(stderr)
        raise SystemExit(status)

    wait_status = wait_for_child(child)
    if os.WIFEXITED(wait_status):
        return os.WEXITSTATUS(wait_status)
    if os.WIFSIGNALED(wait_status):
        return 128 + os.WTERMSIG(wait_status)
    return 127


def flush(stream):
    try:
        stream.flush()
    except (BrokenPipeError, OSError, ValueError):
        pass


def main():
    stdout = sys.stdout
    stderr = sys.stderr

    # PID 1 starts the agent on /dev/console, which is hvc1 in generic virtio mode.
    stdin = sys.stdin.buffer
    configure_raw_input(stdin)
    write_port(SNAPSHOT_PORT, 1)
    print(OUTPUT_MARKER, end="", file=stdout, flush=True)

    status = 127
    try:
        size = int.from_bytes(read_exact(stdin, 8), "big")
        if size > MAX_PAYLOAD:
            raise ValueError(f"workload payload exceeds {MAX_PAYLOAD} bytes")
        request = json.loads(read_exact(stdin, size))
        reseed_random(request.get("entropy"))
        set_realtime(request.get("unixTimeNs"))
        status = run_workload(request, stdout, stderr)
    except SystemExit:
        raise
    except BaseException:
        print_traceback(stderr)

    flush(stdout)
    flush(stderr)
    write_port(EXIT_PORT, status)
    os._exit(status)


if __name__ == "__main__":
    main()
