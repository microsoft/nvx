#!/usr/bin/env python3
"""Local and GitHub release entrypoint; native Specula owns verification."""

import argparse
from contextlib import contextmanager
from dataclasses import asdict, dataclass, replace
import fcntl
import hashlib
import html
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
import uuid

from controller import run as runtime

HERE = Path(__file__).resolve().parent
MODES = {"incremental", "initialize", "resume", "preflight"}
ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]{0,95}")
SHA = re.compile(r"[0-9a-f]{40}")
COMPLETE = {"native_complete_pass", "native_complete_warning", "native_complete_bug_fail"}


class ReleaseError(ValueError):
    def __init__(self, status, message):
        super().__init__(message)
        self.status = status


def read_json(path):
    if path.is_symlink() or not path.is_file():
        raise ReleaseError("invalid_state", f"Not a regular JSON file: {path}")
    value = json.loads(path.read_text())
    if not isinstance(value, dict):
        raise ReleaseError("invalid_state", f"Expected a JSON object: {path}")
    return value


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def confined(path, root):
    path, root = Path(path).absolute(), Path(root).absolute()
    if not path.is_relative_to(root) or ".." in path.parts:
        raise ReleaseError("unsafe_path", f"Path escapes its data root: {path}")
    current = root
    for part in ("", *path.relative_to(root).parts):
        current /= part
        if current.is_symlink():
            raise ReleaseError("unsafe_path", f"Unexpected symlink in data path: {current}")
    return path


def git_env():
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    for key in ("GH_TOKEN", "GITHUB_TOKEN", "COPILOT_GITHUB_TOKEN", "SSH_AUTH_SOCK"):
        env.pop(key, None)
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null",
               GIT_TERMINAL_PROMPT="0", GIT_LFS_SKIP_SMUDGE="1")
    return env


def git(path, *args, check=True):
    command = ["git", "-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false",
               "-c", "credential.helper="]
    if path is not None:
        command += ["-C", str(path)]
    result = subprocess.run(command + list(args), env=git_env(), text=True,
                            capture_output=True, timeout=300)
    if check and result.returncode:
        raise ReleaseError("source_error", result.stderr.strip() or "Git command failed.")
    return result


@contextmanager
def lock(path):
    with path.open("a") as stream:
        try:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            raise ReleaseError("busy", "Another release operation owns this data store.") from exc
        yield stream


@dataclass(frozen=True)
class Request:
    mode: str
    tag: str | None
    revision: str | None
    run_id: str | None
    request_id: str
    preflight: bool = False

    def validate(self):
        if (not isinstance(self.mode, str) or self.mode not in MODES
                or not isinstance(self.request_id, str) or not ID.fullmatch(self.request_id)):
            raise ReleaseError("invalid_request", "Invalid mode or request ID.")
        if bool(self.tag) == bool(self.revision):
            raise ReleaseError("invalid_request", "Specify exactly one release tag or local revision.")
        if self.revision and not SHA.fullmatch(self.revision):
            raise ReleaseError("invalid_request", "Local revision must be a full lowercase commit SHA.")
        if self.tag:
            if self.tag.startswith("-") or git(None, "check-ref-format", f"refs/tags/{self.tag}",
                                               check=False).returncode:
                raise ReleaseError("invalid_request", "Invalid release tag.")
        if self.mode == "resume":
            if not isinstance(self.run_id, str) or not ID.fullmatch(self.run_id):
                raise ReleaseError("invalid_request", "Resume requires an exact native run ID.")
        elif self.run_id:
            raise ReleaseError("invalid_request", "run_id is only valid in resume mode.")
        return self


def request_from_args(args, config):
    mode, tag, revision, run_id = args.mode, args.tag, args.revision, args.run_id
    if args.event_file:
        if any((mode, tag, revision, run_id)):
            raise ReleaseError("invalid_request", "Event input cannot be mixed with local selection flags.")
        event = read_json(Path(args.event_file))
        repository = event.get("repository")
        if not isinstance(repository, dict) or repository.get("full_name") != config["repository"]:
            raise ReleaseError("invalid_request", "Event repository does not match the configured workflow repository.")
        if args.event_name == "release":
            release = event.get("release", {})
            if not isinstance(release, dict) or event.get("action") != "published" or release.get("draft") is not False:
                raise ReleaseError("invalid_request", "Only published, non-draft releases are supported.")
            mode, tag = "incremental", release.get("tag_name")
        elif args.event_name == "workflow_dispatch":
            inputs = event.get("inputs", {})
            if not isinstance(inputs, dict):
                raise ReleaseError("invalid_request", "Malformed workflow inputs.")
            mode, tag, run_id = inputs.get("mode", "incremental"), inputs.get("release_tag"), inputs.get("run_id") or None
        else:
            raise ReleaseError("invalid_request", "Unsupported GitHub event.")
    elif args.event_name:
        raise ReleaseError("invalid_request", "event_name requires event_file.")
    if not isinstance(tag, (str, type(None))) or not isinstance(mode, (str, type(None))):
        raise ReleaseError("invalid_request", "Mode and tag must be strings.")
    return Request(mode or "incremental", tag, revision, run_id,
                   args.request_id or "local-" + uuid.uuid4().hex, args.preflight_only).validate()


def load_config(path):
    config = read_json(path)
    if config.get("version") != 1 or config.get("repository") != "microsoft/nvx":
        raise ReleaseError("invalid_config", "Unsupported configuration or repository.")
    if config.get("release_source") != {
        "url": "https://github.com/microsoft/nvx.git",
        "trusted_branch": "dev",
        "submodule": "openvmm",
    }:
        raise ReleaseError("invalid_config", "Releases must select microsoft/nvx dev's openvmm submodule.")
    if config.get("source_url") != "https://github.com/nanvix/openvmm.git":
        raise ReleaseError("invalid_config", "Only nanvix/openvmm is an allowed verification source.")
    if config.get("specula_commit") != runtime.PIN or Path(config["root"]) != runtime.NATIVE:
        raise ReleaseError("invalid_config", "Runtime pin or data root differs from the bounded launcher.")
    root, work = Path(config["root"]).resolve(), Path(config["work"]).resolve()
    if not work.is_relative_to(root) or work == root:
        raise ReleaseError("invalid_config", "Work directory must be below the native data root.")
    for key in ("kernel", "initramfs", "authorization"):
        if not Path(config[key]).resolve().is_relative_to("/mnt/data"):
            raise ReleaseError("invalid_config", f"{key} must be on the data filesystem.")
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", config["image"]):
        raise ReleaseError("invalid_config", "Runtime image must be pinned by immutable digest.")
    if type(config["timeout_seconds"]) is not int or not 1 <= config["timeout_seconds"] <= 21600:
        raise ReleaseError("invalid_config", "Timeout must be between one second and six hours.")
    return config


def fetch_revision(config, request, cache_name, source_url, branch):
    root = Path(config["root"])
    cache = confined(root / "repos" / cache_name, root)
    cache.parent.mkdir(parents=True, exist_ok=True)
    if not cache.exists():
        git(None, "init", "--bare", "--quiet", str(cache))
        git(cache, "remote", "add", "origin", source_url)
    if git(cache, "remote", "get-url", "origin").stdout.strip() != source_url:
        raise ReleaseError("source_error", "Source-cache origin changed.")
    git(None, "check-ref-format", f"refs/heads/{branch}")
    refs = [f"+refs/heads/{branch}:refs/heads/specula-trusted"]
    if request.tag:
        # No forced tag updates: a moved tag is an explicit error.
        refs.append(f"refs/tags/{request.tag}:refs/tags/{request.tag}")
    git(cache, "fetch", "--quiet", "--no-tags", "origin", *refs)
    ref = f"refs/tags/{request.tag}" if request.tag else request.revision
    revision = git(cache, "rev-parse", "--verify", f"{ref}^{{commit}}").stdout.strip()
    if git(cache, "merge-base", "--is-ancestor", revision, "refs/heads/specula-trusted",
           check=False).returncode:
        raise ReleaseError("untrusted_revision", "Release is not on the configured trusted branch ancestry.")
    return cache, revision


def source_for(config, request):
    root = Path(config["root"])
    selection = {}
    target_request = request
    if request.tag and config.get("release_source"):
        origin = config["release_source"]
        nvx_cache, nvx_revision = fetch_revision(
            config, request, "nvx-release-cache.git", origin["url"], origin["trusted_branch"],
        )
        submodule = origin["submodule"]
        entry = git(nvx_cache, "ls-tree", nvx_revision, "--", submodule).stdout.strip().split()
        if (len(entry) != 4 or entry[:2] != ["160000", "commit"]
                or not SHA.fullmatch(entry[2]) or entry[3] != submodule):
            raise ReleaseError("invalid_submodule", "NVX release does not pin the configured OpenVMM submodule.")
        url = git(nvx_cache, "config", f"--blob={nvx_revision}:.gitmodules",
                  "--get", f"submodule.{submodule}.url").stdout.strip()
        if url != config["source_url"]:
            raise ReleaseError("invalid_submodule", "NVX release's OpenVMM URL differs from the allowed source.")
        target_request = replace(request, tag=None, revision=entry[2])
        selection = {"repository": config["repository"], "tag": request.tag,
                     "revision": nvx_revision, "submodule": submodule}
    cache, revision = fetch_revision(
        config, target_request, "release-cache.git", config["source_url"], config["trusted_branch"],
    )
    source = confined(root / "repos/releases" / request.request_id / revision, root)
    if not source.exists():
        source.parent.mkdir(parents=True, exist_ok=True)
        git(None, "clone", "--quiet", "--no-local", "--no-checkout", str(cache), str(source))
        git(source, "remote", "set-url", "origin", config["source_url"])
        git(source, "checkout", "--quiet", "--detach", revision)
    if (git(source, "rev-parse", "HEAD").stdout.strip() != revision
            or git(source, "status", "--porcelain").stdout.strip()
            or list((source / ".git").rglob("*.promisor")) or (source / ".gitmodules").exists()):
        raise ReleaseError("source_error", "Source must be a clean, complete ordinary clone at the pinned SHA.")
    return source, revision, cache, selection


def bundle_files(directory):
    files = {}
    for path in sorted(directory.rglob("*")):
        relative = path.relative_to(directory)
        if "__pycache__" in relative.parts or path.suffix == ".pyc" or relative.as_posix() == "controller/image.json":
            continue
        if path.is_symlink():
            raise ReleaseError("invalid_bundle", f"Unexpected tooling symlink: {relative}")
        if path.is_file():
            files[relative.as_posix()] = digest(path)
    return files


def stage_bundle(config, directory=HERE):
    files = bundle_files(directory)
    identity = bundle_identity(files)
    destination = confined(Path(config["root"]) / "bundles" / identity, config["root"])
    if not destination.exists():
        staging = destination.with_name(identity + "-" + uuid.uuid4().hex)
        staging.mkdir(parents=True)
        for relative in files:
            target = staging / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(directory / relative, target)
        staging.rename(destination)
    if bundle_files(destination) != files:
        raise ReleaseError("invalid_bundle", "Retained control bundle changed outside its content identity.")
    return destination, identity


def bundle_identity(files):
    return hashlib.sha256(json.dumps(files, sort_keys=True).encode()).hexdigest()


def authorization_blocker(config):
    path = Path(config["authorization"])
    if not path.exists():
        return "Operator execution approval is absent; no model call is authorized."
    stat = path.stat()
    if path.is_symlink() or stat.st_uid != os.getuid() or stat.st_mode & 0o077:
        return "Provider approval must be an owner-only regular file owned by the runtime user."
    approval = read_json(path)
    incident = config["policy_incident"]
    hold = confined(Path(config["work"]) / "releases/provider-hold.json", config["work"])
    if hold.exists():
        incident = read_json(hold)["incident"]
    if approval.get("version") == 2:
        if (approval.get("execution_authorized") is not True
                or approval.get("repository") != config["repository"]
                or approval.get("target") != config["target"]
                or approval.get("acknowledged_incident") != incident
                or any(not isinstance(approval.get(key), str) or not approval[key].strip()
                       for key in ("approval_reference", "approved_at", "approved_by"))):
            return "Operator approval must cover this repository, target and latest recorded interruption."
        return None
    if (approval.get("version") != 1 or approval.get("provider_authorized") is not True
            or approval.get("policy_incident") != incident
            or any(not isinstance(approval.get(key), str) or not approval[key].strip()
                   for key in ("resolution_reference", "approved_at", "approved_by"))):
        return "Provider approval must document legitimate resolution of the recorded incident."
    return None


def initialization_seed(config):
    seed = config.get("initialization_seed")
    if seed is None:
        return None
    if not isinstance(seed, dict) or set(seed) != {"path", "manifest", "manifest_sha256"}:
        raise ReleaseError("invalid_seed", "Initialization seed must pin its directory and native manifest.")
    root = Path(config["root"])
    path = confined(Path(seed["path"]), root)
    manifest = confined(Path(seed["manifest"]), root)
    if digest(manifest) != seed["manifest_sha256"]:
        raise ReleaseError("invalid_seed", "Initialization seed manifest hash changed.")
    expected = read_json(manifest).get("files_sha256")
    if not isinstance(expected, dict) or not {"spec/base.tla", "harness/run.sh"} <= expected.keys():
        raise ReleaseError("invalid_seed", "Initialization seed lacks model/harness provenance.")
    actual = {}
    for asset in path.rglob("*"):
        if asset.is_symlink():
            raise ReleaseError("invalid_seed", "Initialization seed must not contain symlinks.")
        if asset.is_file():
            actual[asset.relative_to(path).as_posix()] = digest(asset)
    if actual != expected:
        raise ReleaseError("invalid_seed", "Initialization seed differs from its frozen manifest.")
    return seed


class Backend:
    def __init__(self, config, bundle):
        self.config, self.bundle = config, bundle
        self.work = Path(config.get("_native_work", config["work"]))

    def invoke(self, mode, arguments, *, seed=None):
        before = set((self.work / "runtime").glob("*/status.json"))
        timeout = self.config["timeout_seconds"]
        if self.config.get("_deadline") is not None:
            timeout = min(timeout, int(self.config["_deadline"] - time.monotonic()))
            if timeout < 1:
                raise ReleaseError("job_timeout", "The combined preparation/verification time budget is exhausted.")
        args = ["--work", str(self.work), "--image", self.config["image"],
                "--harness", str(self.bundle / "harness"),
                "--control", str(self.bundle / "controller"),
                "--vmlinux", self.config["kernel"], "--initramfs", self.config["initramfs"],
                "--environment-id", self.config["environment_id"],
                "--timeout-seconds", str(timeout)]
        if seed:
            args += ["--seed", str(seed)]
        if mode == "native":
            if not self.config.get("_launch_id"):
                raise ReleaseError("invalid_state", "Native execution requires a durable launch journal.")
            args += ["--launch-id", self.config["_launch_id"]]
        code = runtime.main(args + [mode, "--", *arguments], host_lock=self.config.get("_host_lock"))
        paths = set((self.work / "runtime").glob("*/status.json")) - before
        if len(paths) != 1:
            raise ReleaseError("runtime_error", f"Expected one runtime receipt, got {len(paths)} (exit {code}).")
        path = paths.pop()
        return code, read_json(path), str(path)

    def store(self, operation, **values):
        token = uuid.uuid4().hex
        output = confined(self.work / "releases/internal" / f"{token}.json", self.work)
        output.parent.mkdir(parents=True, exist_ok=True)
        arguments = ["python3", "/control/store.py", operation, "--ci-dir", "/work/ci",
                     "--output", "/work/" + str(output.relative_to(self.work))]
        for key, value in values.items():
            if value is not None:
                arguments += ["--" + key.replace("_", "-"), str(value)]
        code, record, _ = self.invoke("exec", arguments)
        if code or record["status"] != "command_success":
            raise ReleaseError("store_error", "Native store inspection/promotion failed; see retained runtime logs.")
        return read_json(output)


def prerequisites(config):
    problems = []
    for name, expected in (("kernel", "kernel_sha256"), ("initramfs", "initramfs_sha256")):
        path = Path(config[name])
        if not path.is_file() or digest(path) != config[expected]:
            problems.append(f"Pinned {name} fixture is absent or its hash differs.")
    secret = runtime.ROOT / "private/copilot-auth.json"
    if not secret.is_file() or secret.is_symlink() or secret.stat().st_mode & 0o077:
        problems.append("Dedicated owner-only Copilot credential file is not provisioned.")
    approval = authorization_blocker(config)
    if approval:
        problems.append(approval)
    return problems


def public_report(directory, record):
    public = confined(directory / "public", directory)
    public.mkdir(exist_ok=True)
    result_path = confined(public / "result.json", directory)
    summary = confined(public / "summary.md", directory)
    runtime.write_json(result_path, record)
    escape = lambda value: html.escape(str(value)).replace("`", "&#96;")
    lines = ["# Specula release verification", "",
             f"Status: **{escape(record['status'])}**", "",
             f"Request: `{escape(record['request_id'])}`",
             f"Source: `{escape(record.get('revision', 'unresolved'))}`",
             f"Native run: `{escape(record.get('native_run', 'not started'))}`", "",
             "Native completion is not human model acceptance or a bug-free guarantee.",
             "No issues, PRs, product fixes or remote configuration are created."]
    selection = record.get("release") or record.get("requested_release")
    if selection:
        lines += ["", f"NVX release: `{escape(selection['tag'])}` at `{escape(selection['revision'])}`",
                  f"Target submodule: `{escape(selection['submodule'])}`"]
    for problem in record.get("blockers", []):
        lines += ["", "- " + escape(problem)]
    if record.get("error"):
        lines += ["", escape(record["error"])]
    lines += ["", "Full model, traces, source diffs and raw logs remain on the dedicated host.", ""]
    with runtime.atomic_output(summary) as stream:
        stream.write("\n".join(lines))
    return public


def managed_record(work, run_id):
    return confined(work / "releases/native-runs" / f"{run_id}.json", work)


def save_managed(work, managed):
    path = managed_record(work, managed["native_run"])
    if path.exists():
        old = read_json(path)
        if any(old.get(key) != managed.get(key) for key in (
            "revision", "image", "environment_id", "target", "native_work", "initialization", "bundle",
            "initialization_seed",
        )):
            raise ReleaseError("ambiguous_recovery", "Native run ID is already bound to a different execution identity.")
    path.parent.mkdir(parents=True, exist_ok=True)
    runtime.write_json(path, managed)


def record_hold(work, run_id, receipt_path, *, unknown=False):
    hold = {"version": 1, "incident": ("interrupted-runtime-" if unknown else "policy-stop-") + digest(receipt_path),
            "native_run": run_id, "runtime_receipt": str(receipt_path), "recorded_at": runtime.now()}
    runtime.write_json(confined(work / "releases/provider-hold.json", work), hold)
    return hold


def reconcile_journal(config, journal_path):
    """Process each launch once, including a newer attempt of an existing run."""
    work = Path(config["work"])
    journal_path = confined(journal_path, work / "releases/requests")
    journal = read_json(journal_path)
    launch_id = journal.get("launch_id")
    if journal.get("version") != 2 or launch_id != journal_path.stem or not re.fullmatch(r"[0-9a-f]{32}", launch_id):
        raise ReleaseError("invalid_state", "Unsupported or inconsistent launch journal.")
    marker_path = confined(work / "releases/reconciled" / f"{launch_id}.json", work)
    journal_hash = digest(journal_path)
    if marker_path.exists():
        marker = read_json(marker_path)
        if marker["journal_sha256"] != journal_hash:
            raise ReleaseError("invalid_state", "A reconciled launch journal was modified.")
        return marker
    native_work = native_directory(work, journal["native_work"])
    receipts = []
    for path in (native_work / "runtime").glob("*/status.json"):
        receipt = read_json(confined(path, native_work))
        if receipt.get("launch_id") != launch_id:
            continue
        if (receipt.get("mode") != "native" or receipt.get("requested_work") != str(native_work)
                or receipt.get("requested_image") != journal["image"]
                or receipt.get("environment_id") != journal["environment_id"]
                or receipt.get("arguments") != journal["native_args"]):
            raise ReleaseError("invalid_state", "Runtime identity differs from its durable launch declaration.")
        if journal.get("initialization_seed") and (
            receipt.get("requested_seed") != journal["initialization_seed"]["path"]
            or (receipt.get("execution_started") is not False
                and receipt.get("seed") != journal["initialization_seed"]["path"])
        ):
            raise ReleaseError("invalid_state", "Runtime seed mount differs from its durable launch declaration.")
        if receipt.get("execution_started") is not False and (
            receipt.get("image_id") != journal["image"] or receipt.get("work") != str(native_work)
        ):
            raise ReleaseError("invalid_state", "Started runtime lacks the verified image identity.")
        receipts.append((path, receipt))
    if len(receipts) > 1:
        raise ReleaseError("ambiguous_recovery", "Multiple runtime receipts claim one launch ID.")
    candidates = []
    for path in (native_work / "ci/runs").glob("*/ci-input.json"):
        run_id = path.parent.name
        if journal["requested_run"]:
            if run_id != journal["requested_run"]:
                continue
        elif run_id in journal["before_runs"]:
            continue
        inputs = read_json(confined(path, native_work))
        if (inputs.get("artifact") == journal["artifact"]
                and inputs.get("source_commit") == journal["revision"]
                and inputs.get("target") == journal["target"]
                and inputs.get("previous") == journal["previous"]
                and inputs.get("dirty") is False
                and isinstance(inputs.get("guidance"), str)
                and hashlib.sha256(inputs.get("guidance", "").encode()).hexdigest() == journal["guidance_sha256"]):
            candidates.append(run_id)
    if len(candidates) > 1:
        raise ReleaseError("ambiguous_recovery", "Multiple native runs match an interrupted launch.")
    run_id = candidates[0] if candidates else None
    marker = {"version": 1, "launch_id": launch_id, "journal_sha256": journal_hash,
              "native_run": run_id, "status": "not_started"}
    if not receipts:
        if run_id and not journal["requested_run"]:
            raise ReleaseError("ambiguous_recovery", "A native run exists without its declared runtime receipt.")
        marker["native_run"] = None
    else:
        receipt_path, receipt = receipts[0]
        status = receipt["status"]
        marker.update(status=status, runtime_receipt=str(receipt_path))
        unstarted_failure = receipt.get("execution_started") is False and status == "wrapper_error"
        if unstarted_failure and run_id and not journal["requested_run"]:
            raise ReleaseError("ambiguous_recovery", "An unstarted runtime cannot own a new native run.")
        if unstarted_failure:
            marker["native_run"] = None
            run_id = None
        if status in {"starting", "preparing"} or (status == "wrapper_error" and not unstarted_failure):
            record_hold(work, run_id, receipt_path, unknown=True)
            raise ReleaseError("runtime_unresolved", "An interrupted or active observer has no final health receipt; reconcile it before any new model execution.")
        if status == "policy_stop" or receipt.get("container_exit_code") == 76:
            record_hold(work, run_id, receipt_path)
        if run_id:
            command_path = receipt_path.with_name("command.json")
            command = read_json(command_path)
            identity = f"--run-id={journal['requested_run']}" if journal["requested_run"] else f"--artifact={journal['artifact']}"
            if (command.get("mode") != "native" or identity not in command.get("argv", [])
                    or ("--ci-candidate" in command["argv"]) is not (not journal["initialization"])):
                raise ReleaseError("invalid_state", "Executed native command differs from its launch journal.")
            path = managed_record(work, run_id)
            previous = read_json(path) if path.exists() else None
            if previous and previous.get("last_launch") != launch_id and (
                previous["runtime_receipt"] != journal["prior_runtime_receipt"]
            ):
                raise ReleaseError("ambiguous_recovery", "Pending launch does not follow the retained runtime receipt.")
            managed = {key: journal[key] for key in (
                "revision", "image", "environment_id", "target", "bundle", "native_work", "initialization", "request_id",
            )}
            managed.update(version=1, native_run=run_id, runtime_receipt=str(receipt_path),
                           recovered_from=str(journal_path), last_launch=launch_id,
                           initialization_seed=journal.get("initialization_seed"))
            results = receipt.get("native_results", [])
            if status in COMPLETE:
                if (len(results) != 1 or results[0].get("run_id") != run_id
                        or results[0].get("complete") is not True
                        or results[0].get("candidate") is not (not journal["initialization"])
                        or results[0].get("verdict") not in {"PASS", "WARNING", "FAIL"}
                        or receipt.get("exit_code") != (2 if results[0]["verdict"] == "FAIL" else 0)):
                    raise ReleaseError("invalid_result", "Completed runtime receipt is inconsistent.")
                managed["eligible_publication"] = results[0]
            if previous and previous.get("last_launch") == launch_id and previous.get("accepted_publication"):
                managed["accepted_publication"] = previous["accepted_publication"]
            save_managed(work, managed)
    marker_path.parent.mkdir(parents=True, exist_ok=True)
    runtime.write_json(marker_path, marker)
    return marker


def reconcile_pending(config):
    """Global gate: a different release ID cannot hide a retained refusal."""
    work = Path(config["work"])
    for path in sorted((work / "releases/requests").glob("*/launches/*.json")):
        reconcile_journal(config, path)


def require_target_resume(config, request):
    work = Path(config["work"])
    pending = []
    for path in sorted((work / "releases/native-runs").glob("*.json")):
        managed = read_json(confined(path, work))
        if (managed["target"] == config["target"] and not managed["initialization"]
                and not managed.get("accepted_publication")):
            pending.append(managed)
    if not pending or (request.mode == "resume" and any(
        managed["native_run"] == request.run_id for managed in pending
    )):
        return
    runs = "; ".join(f"{managed['native_run']} at {managed['revision']}" for managed in pending)
    raise ReleaseError(
        "resume_required",
        f"Unresolved target verification: {runs}. Explicitly resume a listed native run with its original source; a new request ID cannot replace it.",
    )


def recover_launch(config, operation, requested_run=None):
    pointer = operation.get("launch_journal")
    if not pointer:
        return None
    marker = reconcile_journal(config, Path(pointer))
    run_id = marker.get("native_run")
    if not run_id or (requested_run and requested_run != run_id):
        return None
    return read_json(managed_record(Path(config["work"]), run_id))


def eligible_receipt(managed):
    result = managed.get("eligible_publication")
    if not result:
        raise ReleaseError("invalid_result", "No outer-runtime-eligible publication exists.")
    native_work = Path(managed["native_work"])
    receipt = read_json(confined(Path(managed["runtime_receipt"]), native_work / "runtime"))
    if (receipt.get("status") not in COMPLETE or receipt.get("native_results") != [result]
            or receipt.get("exit_code") != (2 if result["verdict"] == "FAIL" else 0)
            or receipt.get("image_id") != managed["image"]
            or receipt.get("environment_id") != managed["environment_id"]
            or receipt.get("work") != str(native_work)):
        raise ReleaseError("invalid_result", "Saved runtime evidence does not authorize this publication.")
    return result


def finish_report(directory, record):
    record["finished_at"] = runtime.now()
    history = confined(directory / "history", directory)
    history.mkdir(exist_ok=True)
    runtime.write_json(history / f"{uuid.uuid4().hex}.json", record)
    public = public_report(directory, record)
    return record["exit_code"], public, record


def native_directory(work, selected):
    path = confined(Path(selected), work)
    if path != work and not path.is_relative_to(work / "initializations"):
        raise ReleaseError("invalid_state", "Native work must be the original store or a private initialization.")
    return path


def active_work(work):
    selector = confined(work / "releases/active-work.json", work)
    return native_directory(work, read_json(selector)["work"]) if selector.exists() else work


def run_request(config, request, *, backend_factory=Backend, bundle_directory=HERE):
    request.validate()
    root, work = Path(config["root"]), Path(config["work"])
    confined(work, root)
    work.mkdir(parents=True, exist_ok=True)
    requests = work / "releases/requests"
    requests.mkdir(parents=True, exist_ok=True)
    directory = confined(requests / request.request_id, root)
    directory.mkdir(exist_ok=True)
    record = {"version": 1, **asdict(request), "status": "preparing", "complete": False,
              "started_at": runtime.now(), "remote_mutations": False,
              "target": config["target"], "image": config["image"],
              "environment_id": config["environment_id"]}
    try:
        with lock(confined(root / ".release.lock", root)), lock(root.parent / ".host.lock") as host_lock:
            config["_host_lock"] = host_lock
            reconcile_pending(config)
            source, revision, cache, selection = source_for(config, request)
            record["revision"] = revision
            record["release"] = selection
            old_path = confined(directory / "public/result.json", root)
            bundle, bundle_id = stage_bundle(config, bundle_directory)
            record["control_bundle"] = bundle_id
            native_work = active_work(work)
            config["_native_work"] = str(native_work)
            backend = backend_factory(config, bundle)
            operation = directory / "operation.json"
            if operation.exists():
                previous_operation = read_json(operation)
                if (previous_operation["revision"] != revision
                        or previous_operation.get("release", {}) != selection):
                    raise ReleaseError("request_identity_changed", "Interrupted request has a different source identity.")
                recovered = recover_launch(config, previous_operation)
                if not recovered and previous_operation.get("native_run"):
                    binding = managed_record(work, previous_operation["native_run"])
                    if binding.exists():
                        recovered = read_json(binding)
                if recovered and request.mode != "resume" and not (
                    old_path.exists() and read_json(old_path).get("complete") is True
                ):
                    record["native_run"] = recovered["native_run"]
                    raise ReleaseError("resume_required", f"Explicitly resume recovered native run {recovered['native_run']}.")
                if (not recovered and (previous_operation.get("launch_journal") or not old_path.exists())
                        and request.mode != "resume"
                        and not (old_path.exists() and read_json(old_path).get("complete") is True)):
                    pointer = previous_operation.get("launch_journal")
                    pending = reconcile_journal(config, Path(pointer)) if pointer else None
                    if not pending or pending.get("native_run"):
                        raise ReleaseError("launch_unresolved", "Invocation preparation was interrupted; inspect its retained runtime before another launch.")
            if old_path.exists():
                old = read_json(old_path)
                if old.get("release", {}) != selection:
                    raise ReleaseError("request_identity_changed", "Request selects a different NVX release.")
                if any(old.get(key) != record[key] for key in ("revision", "target", "image", "environment_id")):
                    raise ReleaseError("request_identity_changed", "Request source or verification configuration changed.")
                if old.get("complete") is True:
                    binding = read_json(managed_record(work, old["native_run"]))
                    accepted = eligible_receipt(binding)
                    if (binding.get("accepted_publication") != accepted or accepted["snapshot"] != old["snapshot"]
                            or any(binding[key] != old[key] for key in ("revision", "image", "environment_id", "target", "native_work"))
                            or (binding["initialization"] and active_work(work) != Path(binding["native_work"]))):
                        raise ReleaseError("invalid_result", "Cached completion has no matching outer acceptance and activation evidence.")
                    config["_native_work"] = str(native_directory(work, old["native_work"]))
                    backend = backend_factory(config, bundle)
                    published = backend.store("inspect", token=old["snapshot"])["current"]
                    if (published["source_commit"] != revision or published["target"] != old["target"]
                            or published["run_id"] != old["native_run"] or published["verdict"] != old["verdict"]
                            or old["exit_code"] != (2 if published["verdict"] == "FAIL" else 0)
                            or old["status"] != "complete_" + published["verdict"].lower()):
                        raise ReleaseError("invalid_result", "Cached public metadata differs from the immutable native publication.")
                    public_report(directory, old)
                    return old["exit_code"], directory / "public", old
                if old.get("native_run") and request.mode != "resume":
                    raise ReleaseError("resume_required", f"Explicitly resume native run {old['native_run']}; do not start another run.")
            managed = None
            if request.mode == "resume":
                path = managed_record(work, request.run_id)
                if not path.exists():
                    recovered = {}
                    for operation_path in requests.glob("*/operation.json"):
                        found = recover_launch(config, read_json(operation_path), request.run_id)
                        if found:
                            recovered[found["native_run"]] = found
                    if len(recovered) != 1 or not path.exists():
                        raise ReleaseError("unmanaged_resume", "No unique managed launch binds this run. Preserve legacy runs and use their original launcher after provider clearance.")
                managed = read_json(path)
                if (managed["revision"] != revision or any(managed.get(key) != config[key]
                        for key in ("image", "environment_id", "target"))):
                    raise ReleaseError("resume_identity_changed", "Resume source or verification environment differs from the saved run.")
                if managed.get("runtime_unresolved"):
                    raise ReleaseError("runtime_unresolved", "The interrupted runtime has no final health receipt; operator reconciliation is required before resume or promotion.")
                completed_path = native_directory(work, managed["native_work"]) / "ci/runs" / request.run_id / "ci-result.json"
                if (not managed.get("eligible_publication") and completed_path.exists()
                        and read_json(completed_path).get("complete") is True):
                    raise ReleaseError("completed_but_unaccepted", "Native execution completed but its outer runtime was rejected; do not promote it or restart a completed conversation.")
                record["native_run"] = request.run_id
                native_work = native_directory(work, managed["native_work"])
                config["_native_work"] = str(native_work)
                if managed["bundle"] != bundle_id:
                    if not re.fullmatch(r"[0-9a-f]{64}", managed["bundle"]):
                        raise ReleaseError("invalid_bundle", "Invalid retained bundle identity.")
                    bundle = confined(root / "bundles" / managed["bundle"], root)
                    if not bundle.is_dir() or bundle_identity(bundle_files(bundle)) != managed["bundle"]:
                        raise ReleaseError("invalid_bundle", "Saved resume tooling is missing or modified.")
                    bundle_id = managed["bundle"]
                    record["control_bundle"] = bundle_id
                backend = backend_factory(config, bundle)
            current = backend.store("inspect").get("current")
            record["previous"] = current["token"] if current else None
            blockers = prerequisites(config)
            gate = "provider_approval_required"
            if current and current["target"] != config["target"]:
                raise ReleaseError("incompatible_baseline", "Baseline belongs to a different target.")
            if current and git(cache, "merge-base", "--is-ancestor", current["source_commit"], revision,
                               check=False).returncode:
                raise ReleaseError("unsupported_ancestry", "Release does not descend from the current baseline.")
            if request.mode in {"incremental", "preflight"} and not current:
                gate = "needs_initialization"
                blockers.insert(0, "No complete current baseline. Finish explicit initialization first.")
            initializing = request.mode == "initialize" or bool(managed and managed["initialization"])
            seed = None
            if initializing:
                saved_seed = managed.get("initialization_seed") if managed else config.get("initialization_seed")
                if managed and config.get("initialization_seed") not in (None, saved_seed):
                    raise ReleaseError("resume_identity_changed", "Resume cannot replace its original initialization seed.")
                seed = initialization_seed({**config, "initialization_seed": saved_seed})
                record["initialization_seed"] = seed
            if request.mode == "initialize":
                if current:
                    raise ReleaseError("already_initialized", "Use incremental; initialization must not replace current.")
                if list((native_work / "ci/runs").glob("*/ci-input.json")):
                    raise ReleaseError("initialization_incomplete", "Existing initialization evidence must be resumed, not replaced.")
                if list((work / "initializations").glob("*/ci/runs/*/ci-input.json")):
                    raise ReleaseError("initialization_incomplete", "An unselected private initialization already exists; resume or reconcile it.")
                native_work = native_directory(work, work / "initializations" / request.request_id)
                if list((native_work / "ci/runs").glob("*/ci-input.json")):
                    raise ReleaseError("initialization_incomplete", "This private initialization must be resumed.")
                config["_native_work"] = str(native_work)
                backend = backend_factory(config, bundle)
            record["native_work"] = str(native_work)
            record["blockers"] = blockers
            if not blockers:
                require_target_resume(config, request)
            if blockers:
                record.update(status=gate, exit_code=3)
            elif request.preflight or request.mode == "preflight":
                record.update(status="preflight_ready", exit_code=0)
            else:
                record.update(status="running", exit_code=1)
                runtime.write_json(directory / "operation.json", record)
                if managed and managed.get("eligible_publication"):
                    result = eligible_receipt(managed)
                else:
                    before = set((native_work / "ci/runs").glob("*/ci-input.json"))
                    if request.mode == "resume":
                        args = [f"--run-id={request.run_id}"]
                        if not initializing:
                            args.append("--ci-candidate")
                    else:
                        guidance = confined(native_work / "releases/guidance" / f"{bundle_id}.md", root)
                        guidance.parent.mkdir(parents=True, exist_ok=True)
                        shutil.copy2(bundle / "guidance.md", guidance)
                        if seed:
                            with guidance.open("a") as stream:
                                stream.write(
                                    "\n\n## Retained model seed for current-source initialization\n\n"
                                    "The read-only /seed contains historical analysis, a model, harness and evidence "
                                    "from an interrupted experiment. Reuse this work rather than restarting source "
                                    "archaeology, but adapt it to the source revision in this run's CI inputs. "
                                    "This is a new initialization on the current Git history, not a resumed conversation "
                                    "or a claim that historical traces validate this revision. Produce fresh current-source "
                                    "execution, replay and final reporting; retain historical findings as prior evidence, "
                                    "not newly discovered bugs or automatically finalized classifications.\n"
                                )
                        artifact = "/sources/" + str(source.relative_to(root / "repos"))
                        args = ["--ci-init" if initializing else "--incremental",
                                f"--artifact={artifact}", f"--revision={revision}",
                                "--guidance=/work/" + str(guidance.relative_to(native_work))]
                        if initializing:
                            args.append(config["target"])
                            if seed:
                                args.append("--byom=/seed")
                        else:
                            args.append("--ci-candidate")
                    inputs = read_json(native_work / "ci/runs" / request.run_id / "ci-input.json") if request.mode == "resume" else None
                    launch_id = uuid.uuid4().hex
                    launch = {
                        "version": 2, "launch_id": launch_id,
                        "request_id": request.request_id, "revision": revision,
                        "image": config["image"], "environment_id": config["environment_id"],
                        "target": config["target"], "bundle": bundle_id,
                        "native_work": str(native_work), "initialization": initializing,
                        "initialization_seed": seed,
                        "requested_run": request.run_id,
                        "prior_runtime_receipt": managed["runtime_receipt"] if managed else None,
                        "native_args": args,
                        "artifact": inputs["artifact"] if inputs else artifact,
                        "previous": inputs.get("previous") if inputs else record["previous"],
                        "guidance_sha256": hashlib.sha256(inputs["guidance"].encode()).hexdigest() if inputs else digest(guidance),
                        "before_runs": sorted(path.parent.name for path in before),
                        "before_attempts": sorted(path.parent.name for path in (native_work / "runtime").glob("*/status.json")),
                    }
                    launch_path = confined(directory / "launches" / f"{launch_id}.json", work)
                    launch_path.parent.mkdir(exist_ok=True)
                    runtime.write_json(launch_path, launch)
                    runtime.write_json(directory / "operation.json", {**record, "launch_journal": str(launch_path)})
                    config["_launch_id"] = launch_id
                    code, attempt, receipt_path = backend.invoke(
                        "native", args, seed=Path(seed["path"]) if seed else None,
                    )
                    reconciliation = reconcile_journal(config, launch_path)
                    run_id = reconciliation.get("native_run")
                    record.update(native_run=run_id, runtime_status=attempt["status"],
                                  runtime_receipt=receipt_path)
                    if not run_id:
                        raise ReleaseError("runtime_error", "Native run identity was not established.")
                    managed = read_json(managed_record(work, run_id))
                    if attempt["status"] not in COMPLETE or code not in (0, 2):
                        if attempt["status"] == "policy_stop":
                            hold = read_json(work / "releases/provider-hold.json")
                            record["policy_incident"] = hold["incident"]
                        record.update(status=attempt["status"], exit_code=code or 1)
                        result = None
                    else:
                        result = eligible_receipt(managed)
                if result is not None:
                    promoted = backend.store("verify-initialization" if initializing else "promote",
                                             token=result["snapshot"],
                                             revision=revision, target=config["target"],
                                             verdict=result["verdict"])
                    if initializing:
                        selector = confined(work / "releases/active-work.json", work)
                        selected = active_work(work)
                        if selected not in (work, native_work):
                            raise ReleaseError("current_changed", "Another initialization already became active.")
                        if selected != native_work:
                            config["_native_work"] = str(selected)
                            if backend_factory(config, bundle).store("inspect").get("current") is not None:
                                raise ReleaseError("current_changed", "Another baseline appeared during initialization.")
                            config["_native_work"] = str(native_work)
                        runtime.write_json(selector, {"version": 1, "work": str(native_work),
                                                      "snapshot": result["snapshot"], "selected_at": runtime.now()})
                    managed["accepted_publication"] = result
                    save_managed(work, managed)
                    record.update(status="complete_" + result["verdict"].lower(), complete=True,
                                  verdict=result["verdict"], snapshot=promoted["current"]["token"],
                                  native_run=result["run_id"], exit_code=2 if result["verdict"] == "FAIL" else 0)
            return finish_report(directory, record)
    except ReleaseError as exc:
        record.update(status=exc.status, error=str(exc), exit_code=1)
    except (OSError, KeyError, TypeError, subprocess.SubprocessError, json.JSONDecodeError) as exc:
        record.update(status="integration_error", error=str(exc), exit_code=1)
    directory = confined(work / "releases/rejections" / (request.request_id + "-" + uuid.uuid4().hex), root)
    directory.mkdir(parents=True)
    return finish_report(directory, record)


def parse_cli_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=HERE / "config.json")
    parser.add_argument("--mode", choices=sorted(MODES))
    parser.add_argument("--tag")
    parser.add_argument("--revision")
    parser.add_argument("--run-id")
    parser.add_argument("--request-id")
    parser.add_argument("--event-file")
    parser.add_argument("--event-name")
    parser.add_argument("--preflight-only", action="store_true")
    return parser.parse_args(argv)


def main(argv=None):
    args = parse_cli_args(argv)
    try:
        config = load_config(args.config)
        request = request_from_args(args, config)
        code, public, record = run_request(config, request)
        if os.environ.get("GITHUB_OUTPUT"):
            with Path(os.environ["GITHUB_OUTPUT"]).open("a") as stream:
                stream.write(f"artifact_dir={public}\nstatus={record['status']}\n")
        print(json.dumps({"status": record["status"], "report": str(public), "exit_code": code}))
        return code
    except (ReleaseError, OSError, KeyError, TypeError, json.JSONDecodeError) as exc:
        print(f"Release input/configuration failed: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
