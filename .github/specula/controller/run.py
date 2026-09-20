#!/usr/bin/env python3
"""One bounded local container attempt; native Specula owns orchestration/state."""

import argparse
from contextlib import contextmanager
import datetime
import fcntl
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import uuid

ROOT = Path("/mnt/data/openvmm-verification")
NATIVE = ROOT / "native-ci"
PIN = "088049c5b3474340213cded2664cdb674bff1e1a"
DEFAULT_IMAGE = "local/openvmm-specula-native:088049c-policy2"
MEMORY = 26 * 1024**3


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


@contextmanager
def atomic_output(path):
    staging = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, prefix=f".{path.name}.",
                                         delete=False) as stream:
            staging = Path(stream.name)
            yield stream
            stream.flush()
            os.fsync(stream.fileno())
        staging.replace(path)
    finally:
        if staging is not None:
            staging.unlink(missing_ok=True)


def write_json(path, data):
    with atomic_output(path) as stream:
        json.dump(data, stream, indent=2, sort_keys=True)
        stream.write("\n")


def docker(*args, check=True):
    env = os.environ.copy()
    for key in (
        "GH_TOKEN", "GITHUB_TOKEN", "COPILOT_GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "SSH_AUTH_SOCK",
    ):
        env.pop(key, None)
    return subprocess.run(["docker", *args], text=True, capture_output=True, check=check, env=env)


def inside(path, parent, *, create=False):
    path = Path(path).expanduser().resolve()
    if not path.is_relative_to(parent.resolve()) or path == parent.resolve():
        raise ValueError(f"Path must be below {parent}: {path}")
    if create:
        path.mkdir(parents=True, exist_ok=True)
    if not path.is_dir():
        raise ValueError(f"Missing directory: {path}")
    if "," in str(path) or "\n" in str(path):
        raise ValueError("Mount paths cannot contain commas or newlines.")
    return path


def mounts_and_limits(work, cache, source=None, seed=None, *, sources=None, harness=None,
                      vmlinux=None, initramfs=None, control=None):
    args = [
        "--init", "--pull=never", "--memory=26g", "--memory-swap=26g", "--cpus=6",
        "--pids-limit=1024", "--user=1001:1003", "--group-add=998",
        "--device=/dev/mshv:/dev/mshv", "--cap-drop=ALL",
        "--security-opt=no-new-privileges", "--read-only",
        "--tmpfs=/run:rw,nosuid,nodev,size=256m",
        "--mount", f"type=bind,src={work},dst=/work",
        "--mount", f"type=bind,src={cache},dst=/cache",
    ]
    if source:
        args += ["--mount", f"type=bind,src={source},dst=/source,readonly"]
    if seed:
        args += ["--mount", f"type=bind,src={seed},dst=/seed,readonly"]
    for path, destination in (
        (sources, "/sources"), (harness, "/harness"), (control, "/control"),
        (vmlinux, "/fixtures/vmlinux"), (initramfs, "/fixtures/initramfs.cpio.gz"),
    ):
        if path:
            args += ["--mount", f"type=bind,src={path},dst={destination},readonly"]
    return args


def verify_config(info):
    config = info["HostConfig"]
    if (
        config["Memory"] != MEMORY or config["MemorySwap"] != MEMORY
        or config["NanoCpus"] != 6_000_000_000 or config["PidsLimit"] != 1024
        or not config["ReadonlyRootfs"] or info["Config"]["User"] != "1001:1003"
        or "ALL" not in config["CapDrop"]
        or not any("no-new-privileges" in item for item in config["SecurityOpt"])
        or "998" not in config["GroupAdd"]
        or config["Tmpfs"].get("/run") != "rw,nosuid,nodev,size=256m"
        or not any(device["PathOnHost"] == "/dev/mshv"
                   and device["PathInContainer"] == "/dev/mshv" for device in config["Devices"])
    ):
        raise ValueError("Docker container policy differs from the requested resource/security envelope.")


def receipts(work):
    result = {}
    for path in (work / "ci/runs").glob("*/ci-result.json"):
        if path.parent.name == "latest":
            continue
        result[str(path)] = path.read_bytes()
    return result


def current_results(work, before):
    rows = []
    for name, content in receipts(work).items():
        if before.get(name) == content:
            continue
        record = json.loads(content)
        snapshot = record.get("snapshot")
        if (
            record.get("complete") is True and record.get("verdict") in {"PASS", "WARNING", "FAIL"}
            and isinstance(snapshot, str) and not Path(snapshot).is_absolute()
            and ".." not in Path(snapshot).parts
            and (work / "ci" / snapshot / "state.json").is_file()
        ):
            state = json.loads((work / "ci" / snapshot / "state.json").read_text())
            if (
                record.get("run_id") != Path(name).parent.name
                or state.get("run_id") != record["run_id"]
                or state.get("verdict") != record["verdict"]
            ):
                raise ValueError("Native publication receipt and state identities differ.")
            rows.append(record)
    return rows


def outcome(mode, code, results, *, timed_out=False, interrupted=False, oom=False, phase_timeout=False):
    if oom:
        return "wrapper_oom"
    if timed_out:
        return "wrapper_timeout"
    if interrupted:
        return "wrapper_interrupted"
    if phase_timeout:
        return "phase_timeout"
    if mode != "native":
        return "command_success" if code == 0 else "command_failure"
    if len(results) == 1:
        verdict = results[0]["verdict"]
        if code != (2 if verdict == "FAIL" else 0):
            return "completed_receipt_exit_mismatch"
        return {"PASS": "native_complete_pass", "WARNING": "native_complete_warning",
                "FAIL": "native_complete_bug_fail"}[verdict]
    return "policy_stop" if code == 76 else "native_incomplete"


def completed_exit_code(state):
    started_at = state.get("StartedAt")
    if (
        state.get("Status") != "exited" or state.get("Running")
        or state.get("Restarting") or not isinstance(started_at, str)
        or not started_at or started_at.startswith("0001-")
        or type(state.get("ExitCode")) is not int
    ):
        raise ValueError(
            f"Container command completion is not established (state={state.get('Status')!r})."
        )
    return state["ExitCode"]


def cgroup_path(pid):
    try:
        for line in Path(f"/proc/{pid}/cgroup").read_text().splitlines():
            if line.startswith("0::"):
                return Path("/sys/fs/cgroup") / line[3:].lstrip("/")
    except OSError:
        pass
    return None


def sample(path):
    values = {}
    if path:
        for name in ("memory.current", "memory.peak", "memory.events", "cpu.stat", "pids.current"):
            try:
                values[name] = (path / name).read_text().strip()
            except OSError:
                pass
    return values


def oom_count(values):
    return dict(line.split() for line in values.get("memory.events", "").splitlines()).get("oom_kill", "0")


def main(argv=None, *, host_lock=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", required=True)
    parser.add_argument("--cache", default=str(NATIVE / "cache/runtime"))
    parser.add_argument("--source")
    parser.add_argument("--sources", default=str(NATIVE / "repos"))
    parser.add_argument("--harness")
    parser.add_argument("--control")
    parser.add_argument("--vmlinux")
    parser.add_argument("--initramfs")
    parser.add_argument("--seed")
    parser.add_argument("--image", default=DEFAULT_IMAGE)
    parser.add_argument("--timeout-seconds", type=int, default=21600)
    parser.add_argument("--phase-timeout-seconds", type=int, default=21600)
    parser.add_argument("--environment-id", default="native-088049c-protoc27-policy2-transient3-v4")
    parser.add_argument("--launch-id")
    parser.add_argument("mode", choices=("probe", "auth-probe", "exec", "native"))
    parser.add_argument("arguments", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    if min(args.timeout_seconds, args.phase_timeout_seconds) < 1:
        parser.error("Timeouts must be positive.")
    owned_lock = host_lock is None
    lock = host_lock if host_lock is not None else (ROOT / ".host.lock").open("a")
    if host_lock is not None:
        expected, actual = (ROOT / ".host.lock").stat(), os.fstat(lock.fileno())
        if (expected.st_dev, expected.st_ino) != (actual.st_dev, actual.st_ino):
            raise ValueError("Inherited host lock does not identify the verification lock.")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        print("Another verification build/run holds .host.lock.", file=sys.stderr)
        if owned_lock:
            lock.close()
        return 1
    container = None
    child = None
    interrupted = False
    timed_out = False
    code = 1
    directory = None
    command = list(args.arguments)
    if command[:1] == ["--"]:
        command.pop(0)
    record = {"started_at": now(), "mode": args.mode, "specula_commit": PIN,
              "environment_id": args.environment_id, "launch_id": args.launch_id,
              "requested_image": args.image, "requested_work": args.work,
              "requested_seed": args.seed, "arguments": command, "execution_started": False}

    def interrupt(_signum, _frame):
        nonlocal interrupted
        interrupted = True

    old_handlers = {sig: signal.signal(sig, interrupt) for sig in (signal.SIGINT, signal.SIGTERM)}
    try:
        work = inside(args.work, NATIVE, create=True)
        cache = inside(args.cache, NATIVE / "cache", create=True)
        source = inside(args.source, NATIVE / "repos") if args.source else None
        sources = Path(args.sources).expanduser().resolve()
        if sources != NATIVE / "repos":
            sources = inside(sources, NATIVE / "repos")
        if not sources.is_dir():
            raise ValueError("The read-only sources directory is unavailable.")
        harness = inside(args.harness, NATIVE) if args.harness else None
        control = inside(args.control, NATIVE) if args.control else None
        guest_files = {}
        for field in ("vmlinux", "initramfs"):
            value = getattr(args, field)
            if value:
                path = Path(value).expanduser().resolve()
                if not path.is_file() or "," in str(path) or "\n" in str(path):
                    raise ValueError(f"--{field} requires an existing regular file with a safe mount path.")
                if path.is_relative_to(ROOT / "private"):
                    raise ValueError("Credentials cannot be mounted as guest fixtures.")
                guest_files[field] = path
        seed = inside(args.seed, ROOT) if args.seed else None
        if seed and (seed.is_relative_to(ROOT / "private") or (ROOT / "private").is_relative_to(seed)):
            raise ValueError("Credential directories cannot be mounted as seed assets.")
        paths = [work, cache, sources, *([harness] if harness else []),
                 *([control] if control else []), *([seed] if seed else [])]
        if any(a.is_relative_to(b) or b.is_relative_to(a)
               for index, a in enumerate(paths) for b in paths[index + 1:]):
            raise ValueError("Work/cache/source/seed mounts must be disjoint, not nested.")
        if source is not None and not source.is_relative_to(sources):
            raise ValueError("--source must be within the mounted --sources directory.")
        if args.mode == "native" and source is None and not any(
            arg.startswith(("--artifact=/sources/", "--run-id=")) for arg in args.arguments
        ):
            raise ValueError("Native mode requires --source or an explicit --artifact=/sources/CLONE.")
        attempt = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ-") + uuid.uuid4().hex[:8]
        directory = work / "runtime" / attempt
        directory.mkdir(parents=True)
        record.update(attempt=attempt, work=str(work), cache=str(cache), source=str(source) if source else None,
                      seed=str(seed) if seed else None,
                      sources=str(sources), harness=str(harness) if harness else None,
                      control=str(control) if control else None,
                      guest_files={key: str(value) for key, value in guest_files.items()},
                      timeout_seconds=args.timeout_seconds, phase_timeout_seconds=args.phase_timeout_seconds)
        print(f"Attempt: {directory}", flush=True)
        image = json.loads(docker("image", "inspect", args.image).stdout)[0]
        if image["Config"]["Labels"].get("io.specula.commit") != PIN:
            raise ValueError("Image is not the pinned current Specula runtime.")
        record["image_id"] = image["Id"]
        before = receipts(work)
        flags = mounts_and_limits(work, cache, source, seed, sources=sources,
                                  harness=harness, control=control, **guest_files)
        if args.mode in {"native", "auth-probe"}:
            secret = ROOT / "private/copilot-auth.json"
            if not secret.is_file():
                raise ValueError("Dedicated credential file is unavailable.")
            flags += ["--mount", f"type=bind,src={secret},dst=/run/secrets/copilot-auth.json,readonly"]
        flags += [
            "--env", f"NATIVE_ATTEMPT_DIR=/work/runtime/{attempt}",
            "--env", f"SPECULA_AGENT_TIMEOUT_SECONDS={args.phase_timeout_seconds}",
            "--env", f"SPECULA_CI_ENVIRONMENT={args.environment_id}",
            "--env", "HOME=/work/home", "--env", "TMPDIR=/work/scratch",
        ]
        container = docker("create", "--name", f"specula-native-{attempt}", *flags,
                           image["Id"], args.mode, *command).stdout.strip()
        info = json.loads(docker("inspect", container).stdout)[0]
        verify_config(info)
        # Do not store the full environment: retain only explicit non-secret controls.
        write_json(directory / "container-policy.json", info["HostConfig"])
        record["container_id"] = container
        record["execution_started"] = True
        write_json(directory / "status.json", {**record, "status": "starting"})
        started = time.monotonic()
        latest = {}
        initial_oom = 0
        group = None
        with (directory / "console.log").open("x") as log:
            child = subprocess.Popen(["docker", "start", "-a", container], stdout=log, stderr=subprocess.STDOUT)
            while child.poll() is None:
                if interrupted or time.monotonic() - started >= args.timeout_seconds:
                    timed_out = not interrupted
                    docker("stop", "--time", "15", container, check=False)
                    try:
                        child.wait(timeout=25)
                    except subprocess.TimeoutExpired:
                        docker("kill", container, check=False)
                        child.wait(timeout=15)
                    break
                if group is None:
                    state = json.loads(docker("inspect", container).stdout)[0]["State"]
                    if state["Running"]:
                        group = cgroup_path(state["Pid"])
                        initial_oom = int(oom_count(sample(group)))
                observed = sample(group)
                if observed:
                    latest = observed
                    with (directory / "resources.jsonl").open("a") as stream:
                        stream.write(json.dumps({"at": now(), **observed}) + "\n")
                time.sleep(1 if group is None else 5)
        record["docker_client_exit_code"] = child.wait()
        final = json.loads(docker("inspect", container).stdout)[0]["State"]
        write_json(directory / "container-state.json", final)
        container_code = completed_exit_code(final)
        record["container_exit_code"] = container_code
        code = 124 if timed_out else 130 if interrupted else container_code
        results = current_results(work, before) if args.mode == "native" else []
        events_path = directory / "agent-events.jsonl"
        events = [json.loads(line) for line in events_path.read_text().splitlines()] if events_path.exists() else []
        state = outcome(
            args.mode, code, results, timed_out=timed_out, interrupted=interrupted,
            oom=final["OOMKilled"] or int(oom_count(latest)) > initial_oom,
            phase_timeout=any(event["kind"] == "phase_timeout" for event in events),
        )
        record.update(status=state, exit_code=code, native_results=results,
                      finished_at=now(), last_resources=latest, duration_seconds=round(time.monotonic() - started, 2))
        if state in {"native_incomplete", "completed_receipt_exit_mismatch", "wrapper_oom", "phase_timeout"} and code == 0:
            code = 1
            record["exit_code"] = code
    except (OSError, ValueError, subprocess.SubprocessError) as exc:
        print(f"Runtime failed: {exc}", file=sys.stderr)
        record.update(status="wrapper_error", error=str(exc), exit_code=1, finished_at=now())
        code = 1
    finally:
        if container:
            state_probe = docker("inspect", container, check=False)
            if state_probe.returncode == 0 and json.loads(state_probe.stdout)[0]["State"]["Running"]:
                docker("stop", "--time", "15", container, check=False)
            docker("rm", container, check=False)
        if child is not None and child.poll() is None:
            child.wait(timeout=20)
        if directory:
            write_json(directory / "status.json", record)
            print(json.dumps({"attempt": str(directory), "status": record["status"], "exit_code": code}), flush=True)
        for sig, handler in old_handlers.items():
            signal.signal(sig, handler)
        if owned_lock:
            lock.close()
    return code


if __name__ == "__main__":
    raise SystemExit(main())
