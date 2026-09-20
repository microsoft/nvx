#!/usr/bin/env python3
"""Execute real MSHV capture + two private restores, retaining native evidence."""
import argparse
from datetime import datetime, timezone
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time
import uuid

from resource_check import inspect

PINNED_KERNEL = "b2fdef133b4d75ea093abb69e97012eef0f5603b03a6157c5736d3afa1270cca"
PINNED_INITRD = "68aa21364eff8cd16f8cf1fb303172f46764a20d78580bbc57a95c18f4747faa"


class RunInterrupted(RuntimeError):
    pass


class Cancellation:
    def __init__(self):
        self.signum = None
        self.previous_handlers = {}

    def request(self, signum, _frame):
        # Do not raise here: a signal can arrive while Popen is creating the child.
        if self.signum is None:
            self.signum = signum

    def install(self):
        for signum in (signal.SIGTERM, signal.SIGINT):
            self.previous_handlers[signum] = signal.signal(signum, self.request)

    def restore(self):
        for signum, handler in self.previous_handlers.items():
            signal.signal(signum, handler)

    def check(self):
        if self.signum is not None:
            raise RunInterrupted(f"received {signal.Signals(self.signum).name}")

    def sleep(self, seconds):
        deadline = time.monotonic() + seconds
        while True:
            self.check()
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return
            time.sleep(min(0.1, remaining))


def stop_process_group(process):
    if process.poll() is not None:
        return
    # Until wait reaps this child, its PID/PGID cannot be reused. Kill the whole
    # group before reaping, including descendants that might ignore SIGTERM.
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        # The group exited between poll and killpg; the child still needs reaping.
        pass
    process.wait()


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def save(path, data):
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")


def require(value, message):
    if not value:
        raise RuntimeError(message)


def command_text(args, cwd):
    return subprocess.check_output(args, cwd=cwd, timeout=30).decode().strip()


def newc_entry(name, data, mode, ino):
    name = name.encode() + b"\0"
    fields = [ino, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(name), 0]
    result = b"070701" + b"".join(f"{f:08x}".encode() for f in fields) + name
    result += b"\0" * (-len(result) % 4)
    result += data
    return result + b"\0" * (-len(result) % 4)


def derive_initrd(base, script, destination):
    """Preserve entries without extracting paths; replace only the regular /init."""
    source = gzip.decompress(base.read_bytes())
    entries = []
    offset = 0
    replaced = 0
    while offset + 110 <= len(source):
        start = offset
        header = source[offset:offset + 110]
        require(header[:6] == b"070701", "fixture must be newc cpio")
        fields = [int(header[i:i + 8], 16) for i in range(6, 110, 8)]
        size, name_size = fields[6], fields[11]
        name = source[offset + 110:offset + 110 + name_size - 1]
        offset = (offset + 110 + name_size + 3) & ~3
        offset = (offset + size + 3) & ~3
        require(offset <= len(source), "truncated fixture cpio")
        if name == b"TRAILER!!!":
            break
        if name in (b"init", b"./init"):
            require(fields[1] & 0o170000 == 0o100000, "fixture init must be regular")
            replaced += 1
        else:
            entries.append(source[start:offset])
    else:
        raise RuntimeError("fixture cpio lacks trailer")
    require(replaced == 1, "fixture must contain exactly one init")
    entries.append(newc_entry("init", script.read_bytes(), 0o100755, 0x7ffffffe))
    entries.append(newc_entry("TRAILER!!!", b"", 0, 0x7fffffff))
    archive = b"".join(entries)
    archive += b"\0" * (-len(archive) % 512)
    destination.write_bytes(gzip.compress(archive, mtime=0))
    return {"preserved_cpio_entries": len(entries) - 2, "replaced_entries": ["init"]}


def artifacts(root):
    require(root.is_dir(), f"snapshot directory not published: {root}")
    result = {}
    for path in sorted(root.rglob("*")):
        require(not path.is_symlink(), f"unexpected snapshot symlink: {path}")
        if path.is_file():
            result[str(path.relative_to(root))] = {
                "sha256": digest(path), "bytes": path.stat().st_size,
            }
    require(bool(result), "snapshot contains no artifacts")
    return result


def run_process(label, args, output, timeout, env, cancellation):
    cancellation.check()
    stdout_path = output / f"{label}.stdout"
    stderr_path = output / f"{label}.stderr"
    result = {"argv": [str(arg) for arg in args], "stdout": str(stdout_path),
              "stderr": str(stderr_path), "timeout_seconds": timeout,
              "started_utc": datetime.now(timezone.utc).isoformat()}
    start = time.monotonic_ns()
    deadline = time.monotonic() + timeout
    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        process = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=stdout,
                                   stderr=stderr, env=env, start_new_session=True)
        result["pid"] = process.pid
        result["timed_out"] = False
        try:
            while cancellation.signum is None:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    result["timed_out"] = True
                    break
                try:
                    process.wait(timeout=min(0.1, remaining))
                    break
                except subprocess.TimeoutExpired:
                    continue
        finally:
            try:
                stop_process_group(process)
            finally:
                process.stdin.close()
            result["exit_code"] = process.returncode
            result["interrupted"] = cancellation.signum is not None
            if result["interrupted"]:
                result["termination_signal"] = signal.Signals(cancellation.signum).name
            result["host_elapsed_ns"] = time.monotonic_ns() - start
            save(output / f"{label}.process.json", result)
    # Only exact guest stdout lines count; never accept echoed argv or VMM log text.
    result["guest_lines"] = [
        line.decode("ascii", errors="replace").rstrip("\r")
        for line in stdout_path.read_bytes().split(b"\n")
        if line.startswith(b"NATIVE-")
    ]
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True, help="parent of unique run directory")
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--initrd", type=Path, required=True)
    parser.add_argument("--kernel-sha256", default=PINNED_KERNEL)
    parser.add_argument("--initrd-sha256", default=PINNED_INITRD)
    parser.add_argument("--process-timeout", type=int, default=120)
    parser.add_argument("--restore-delay", type=float, default=3)
    parser.add_argument("--json", action="store_true", help="emit a machine-readable final receipt")
    args = parser.parse_args()
    require(1 <= args.process_timeout <= 900, "process timeout must be 1..900 seconds")
    require(0 <= args.restore_delay <= 60, "restore delay must be 0..60 seconds")
    for key in ("source", "binary", "output", "kernel", "initrd"):
        path = getattr(args, key).resolve()
        roots = ("/mnt/data", "/work", "/cache")
        if key != "output":
            roots += ("/source", "/sources", "/seed", "/harness", "/fixtures")
        require(any(path.is_relative_to(root) for root in roots),
                f"--{key} must use an approved data mount: {', '.join(roots)}")
        setattr(args, key, path)
    os.umask(0o077)
    args.output.mkdir(parents=True, exist_ok=True)
    run_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + uuid.uuid4().hex[:12]
    output = args.output / run_id
    output.mkdir()
    evidence = {
        "schema": "native-openvmm-normal-evidence-v1", "run_id": run_id,
        "status": "incomplete", "backend": "mshv",
        "scenario": "blockless-untiered-capture-two-private-restores",
        "memory_bytes": 128 * 1024**2, "processes": [],
        "scope_excluded": ["network", "control broker", "sandbox blocks", "tiered snapshots"],
        "measurement_semantics": {
            "host_elapsed_ns": "actual monotonic process duration; not VM internal time",
            "guest_clock_lines": "actual integer-second guest wall/uptime and /proc CPU ticks",
            "model_state": "none; no fabricated trace, counters, clock state or boundary events",
            "entropy": "actual guest digest of fresh host restore entropy; not reseeded guest RNG",
        },
    }
    save(output / "evidence.json", evidence)
    cancellation = Cancellation()
    cancellation.install()
    try:
        cancellation.check()
        resources = inspect()
        save(output / "resources.json", resources)
        require(not resources["failures"], "; ".join(resources["failures"]))
        env = os.environ.copy()
        scratch = output / "scratch"
        scratch.mkdir()
        env["TMPDIR"] = str(scratch)
        env["OPENVMM_LOG"] = os.environ.get("OPENVMM_LOG", "info")
        source_sha = command_text(["git", "rev-parse", "HEAD"], args.source)
        status = command_text(["git", "status", "--porcelain=v1"], args.source)
        patch = subprocess.check_output(["git", "diff", "--binary", "HEAD"],
                                        cwd=args.source, timeout=30)
        (output / "source.diff").write_bytes(patch)
        evidence["source"] = {
            "path": str(args.source), "sha": source_sha, "status_porcelain": status,
            "tracked_diff_sha256": hashlib.sha256(patch).hexdigest(),
        }
        evidence["binary"] = {"path": str(args.binary), "sha256": digest(args.binary)}
        require(os.access(args.binary, os.X_OK), "binary is not executable")
        evidence["kernel"] = {"path": str(args.kernel), "sha256": digest(args.kernel)}
        evidence["base_initrd"] = {"path": str(args.initrd), "sha256": digest(args.initrd)}
        require(evidence["kernel"]["sha256"] == args.kernel_sha256, "kernel identity mismatch")
        require(evidence["base_initrd"]["sha256"] == args.initrd_sha256, "initrd identity mismatch")
        script = Path(__file__).resolve().with_name("guest-init.sh")
        (output / "guest-init.sh").write_bytes(script.read_bytes())
        derived = output / "guest-initramfs.cpio.gz"
        evidence["initrd_derivation"] = derive_initrd(args.initrd, script, derived)
        evidence["guest_init"] = {"path": str(output / "guest-init.sh"), "sha256": digest(script)}
        evidence["runtime_initrd"] = {"path": str(derived), "sha256": digest(derived)}
        save(output / "evidence.json", evidence)

        snapshot = output / "snapshot"
        common = [args.binary, "--single-process", "--machine", "microvm",
                  "--hypervisor", "mshv"]
        source = run_process("capture", common + [
            "--memory", "128M", "--kernel", args.kernel, "--initrd", derived,
            "--snapshot-destination", snapshot,
        ], output, args.process_timeout, env, cancellation)
        evidence["processes"].append(source)
        cancellation.check()
        require(not source["timed_out"] and source["exit_code"] == 0,
                "source capture failed; inspect capture.stdout and capture.stderr")
        require(source["guest_lines"].count("NATIVE-BOOT") == 1, "source did not boot once")
        require(source["guest_lines"].count("NATIVE-CAPTURE-REQUEST") == 1,
                "source did not issue one guest snapshot request")
        require("NATIVE-CONTINUED" not in source["guest_lines"],
                "source continued beyond snapshot boundary")
        original = artifacts(snapshot)
        save(output / "snapshot-hashes-before.json", original)
        evidence["snapshot_artifacts"] = original
        entropy_digests = []
        for index in range(2):
            cancellation.sleep(args.restore_delay)
            restored = run_process(f"restore-{index}", common + [
                "--restore-snapshot", snapshot, "--restore-entropy",
            ], output, args.process_timeout, env, cancellation)
            evidence["processes"].append(restored)
            cancellation.check()
            after = artifacts(snapshot)
            save(output / f"snapshot-hashes-after-{index}.json", after)
            require(after == original, f"restore {index} mutated source snapshot artifacts")
            require(not restored["timed_out"] and restored["exit_code"] == 37,
                    f"restore {index} failed; inspect its stdout/stderr")
            lines = restored["guest_lines"]
            for marker in ("NATIVE-CONTINUED", "NATIVE-PRIVATE-INITIAL-CLEAN",
                           "NATIVE-TIMER-DONE", "NATIVE-DONE"):
                require(lines.count(marker) == 1, f"restore {index}: expected one {marker}")
            require("NATIVE-BOOT" not in lines and "NATIVE-CAPTURE-REQUEST" not in lines,
                    f"restore {index} cold-booted instead of resuming")
            require(not any(line.startswith("NATIVE-FAIL-") for line in lines),
                    f"restore {index} reported guest failure")
            entropy = [line.removeprefix("NATIVE-ENTROPY-") for line in lines
                       if line.startswith("NATIVE-ENTROPY-")]
            require(len(entropy) == 1 and re.fullmatch("[0-9a-f]{64}", entropy[0]),
                    f"restore {index} missing actual entropy digest")
            require(lines.count("NATIVE-PRIVATE-WRITTEN-" + entropy[0]) == 1,
                    f"restore {index} private write not observed")
            entropy_digests.append(entropy[0])
            save(output / "evidence.json", evidence)
        require(entropy_digests[0] != entropy_digests[1],
                "two restores did not observe distinct fresh entropy")
        require(evidence["binary"]["sha256"] == digest(args.binary), "binary changed during run")
        require(source_sha == command_text(["git", "rev-parse", "HEAD"], args.source),
                "source HEAD changed during run")
        cancellation.check()
        evidence["checks"] = {
            "real_mshv_capture": True, "source_exited_without_continuation": True,
            "two_independent_restore_processes": True, "one_continuation_each": True,
            "private_marker_initially_clean_each": True, "private_guest_writes_each": True,
            "fresh_entropy_differs": True, "snapshot_artifacts_unchanged_each": True,
        }
        evidence["status"] = "passed"
    except Exception as error:
        evidence["status"] = "failed"
        evidence["error"] = f"{type(error).__name__}: {error}"
        print(evidence["error"], file=sys.stderr)
    finally:
        try:
            if cancellation.signum is not None:
                evidence["status"] = "failed"
                evidence["interrupted"] = True
                evidence["termination_signal"] = signal.Signals(cancellation.signum).name
                evidence["error"] = f"RunInterrupted: received {evidence['termination_signal']}"
            evidence["finished_utc"] = datetime.now(timezone.utc).isoformat()
            save(output / "resources-after.json", inspect())
            save(output / "evidence.json", evidence)
            receipt = {
                "schema": "native-openvmm-run-result-v1",
                "status": evidence["status"], "run_id": run_id, "backend": "mshv",
                "evidence": str(output / "evidence.json"),
            }
            if args.json:
                print(json.dumps(receipt))
            else:
                print(f"{evidence['status']}: {output / 'evidence.json'}")
        finally:
            cancellation.restore()
    return 0 if evidence["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
