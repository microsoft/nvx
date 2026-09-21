#!/usr/bin/env python3
"""Thin NVX release adapter for Specula's native incremental CI."""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import re
import shutil
import subprocess
import sys
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path

HERE = Path(__file__).resolve().parent
SHA = re.compile(r"[0-9a-f]{40}")
RUN_ID = re.compile(r"[A-Za-z0-9._-]+")
SEMVER = re.compile(r"^[^0-9]*(\d+)\.(\d+)\.(\d+)(?:[-+][0-9A-Za-z.-]+)?$")
EXPECTED_SPECULA_LINKS = {
    "tools/context_control/.venv",
    "tools/inv_checking_tool/.venv",
    "tools/spec_analyzer/.venv",
    "tools/tlc_tools/.venv",
    "tools/trace_debugger/.venv",
}


class CIError(RuntimeError):
    pass


def valid_id(value: str) -> bool:
    return value not in {".", ".."} and RUN_ID.fullmatch(value) is not None


def valid_tag(value: str) -> bool:
    return (
        bool(value)
        and subprocess.run(
            ["git", "check-ref-format", f"refs/tags/{value}"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        ).returncode
        == 0
    )


def parse_semver(value: str) -> tuple[int, int, int]:
    match = SEMVER.fullmatch(value.strip())
    if match is None:
        raise CIError(f"cannot parse semantic version: {value!r}")
    return tuple(int(component) for component in match.groups())


@contextmanager
def state_lock(config: dict) -> Iterator[None]:
    root = Path(config["state_root"])
    root.mkdir(parents=True, exist_ok=True)
    flags = os.O_CREAT | os.O_RDWR | os.O_CLOEXEC | os.O_NOFOLLOW
    descriptor = os.open(root / ".specula-ci.lock", flags, 0o600)
    with os.fdopen(descriptor, "r+"):
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            raise CIError(
                "another Specula CI operation owns the persistent state"
            ) from exc
        yield


def read_json(path: Path) -> dict:
    value = json.loads(path.read_text())
    if not isinstance(value, dict):
        raise CIError(f"expected a JSON object: {path}")
    return value


def git_repo_args(repo: Path) -> list[str]:
    if (
        (repo / "HEAD").is_file()
        and (repo / "objects").is_dir()
        and not (repo / ".git").exists()
    ):
        return [f"--git-dir={repo}"]
    return ["-C", str(repo)]


def git(repo: Path | None, *args: str) -> str:
    command = ["git", "-c", "core.hooksPath=/dev/null", "-c", "credential.helper="]
    if repo is not None:
        command += git_repo_args(repo)
    result = subprocess.run(command + list(args), text=True, capture_output=True)
    if result.returncode:
        raise CIError(f"git {args[0]} failed: {result.stderr.strip()[:500]}")
    return result.stdout.strip()


def ensure_bare(path: Path, url: str) -> None:
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        git(None, "init", "--bare", "--quiet", str(path))
        git(path, "remote", "add", "origin", url)
    if git(path, "remote", "get-url", "origin") != url:
        raise CIError(f"cached repository has an unexpected origin: {path}")


def resolve_nvx_tag(config: dict, tag: str) -> str:
    if not valid_tag(tag):
        raise CIError("NVX tag is not a valid Git tag name")
    root = Path(config["state_root"])
    cache = root / "repos/nvx.git"
    ensure_bare(cache, f"https://github.com/{config['repository']}.git")
    git(
        cache,
        "fetch",
        "--quiet",
        "--prune",
        "origin",
        f"+refs/heads/{config['trusted_branch']}:refs/remotes/origin/{config['trusted_branch']}",
        f"refs/tags/{tag}:refs/tags/{tag}",
    )
    revision = git(cache, "rev-parse", f"refs/tags/{tag}^{{commit}}")
    if not SHA.fullmatch(revision):
        raise CIError("NVX tag did not resolve to a full commit SHA")
    if subprocess.run(
        [
            "git",
            *git_repo_args(cache),
            "merge-base",
            "--is-ancestor",
            revision,
            f"refs/remotes/origin/{config['trusted_branch']}",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    ).returncode:
        raise CIError(f"NVX tag {tag} is not on {config['trusted_branch']} ancestry")
    entry = git(cache, "ls-tree", revision, "--", config["submodule"]).split()
    if (
        len(entry) != 4
        or entry[:2] != ["160000", "commit"]
        or entry[3] != config["submodule"]
    ):
        raise CIError(f"NVX tag {tag} does not pin the {config['submodule']} submodule")
    url = git(
        cache,
        "config",
        f"--blob={revision}:.gitmodules",
        "--get",
        f"submodule.{config['submodule']}.url",
    )
    if url != config["source_url"]:
        raise CIError(f"NVX tag {tag} uses an unexpected OpenVMM origin")
    return entry[2]


def prepare_source(config: dict, revision: str) -> Path:
    if not SHA.fullmatch(revision):
        raise CIError("OpenVMM revision must be a full commit SHA")
    root = Path(config["state_root"])
    cache = root / "repos/openvmm.git"
    source = root / "source/openvmm"
    ensure_bare(cache, config["source_url"])
    git(
        cache,
        "fetch",
        "--quiet",
        "--prune",
        "origin",
        f"+refs/heads/{config['source_branch']}:refs/remotes/origin/{config['source_branch']}",
    )
    if subprocess.run(
        ["git", *git_repo_args(cache), "cat-file", "-e", f"{revision}^{{commit}}"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    ).returncode:
        raise CIError("OpenVMM revision is not available from the trusted branch")
    if subprocess.run(
        [
            "git",
            *git_repo_args(cache),
            "merge-base",
            "--is-ancestor",
            revision,
            f"refs/remotes/origin/{config['source_branch']}",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    ).returncode:
        raise CIError(f"OpenVMM revision is not on {config['source_branch']} ancestry")
    if not source.exists():
        source.parent.mkdir(parents=True, exist_ok=True)
        git(None, "clone", "--quiet", "--no-local", str(cache), str(source))
        git(source, "remote", "set-url", "origin", config["source_url"])
    if not (source / ".git").is_dir() or git(source, "status", "--porcelain"):
        raise CIError(
            f"persistent OpenVMM checkout is not a clean ordinary clone: {source}"
        )
    git(source, "fetch", "--quiet", str(cache), revision)
    git(source, "checkout", "--quiet", "--detach", revision)
    return source


def event_request(path: Path, event_name: str) -> tuple[str, str | None, str | None]:
    event = read_json(path)
    if event.get("repository", {}).get("full_name") != "microsoft/nvx":
        raise CIError("event repository is not microsoft/nvx")
    if event_name == "release":
        release = event.get("release", {})
        if event.get("action") != "published" or release.get("draft"):
            raise CIError("release event is not a published non-draft release")
        return "incremental", release.get("tag_name"), None
    if event_name == "workflow_dispatch":
        inputs = event.get("inputs", {})
        return (
            inputs.get("mode", "incremental"),
            inputs.get("release_tag"),
            inputs.get("run_id") or None,
        )
    raise CIError(f"unsupported event: {event_name}")


def specula_command(
    config: dict, mode: str, source: Path, revision: str, run_id: str | None
) -> list[str]:
    binary = config["specula_binary"]
    ci_dir = Path(config["state_root"]) / "state/openvmm-snapshot-restore"
    common = [
        binary,
        "run",
        f"--ci-dir={ci_dir}",
        f"--agent-config={HERE / config['agent_config']}",
    ]
    if mode == "resume":
        if not run_id or not valid_id(run_id):
            raise CIError("resume requires an exact Specula run ID")
        return [
            binary,
            "run",
            f"--ci-dir={ci_dir}",
            f"--run-id={run_id}",
            f"--revision={revision}",
        ]
    if run_id:
        raise CIError("run_id is valid only in resume mode")
    source_args = [f"--artifact={source}", f"--revision={revision}"]
    if mode == "initialize":
        return common + [
            "--ci-init",
            f"--guidance={HERE / 'guidance.md'}",
            "--max-parallel=1",
            "--policy-retries=2",
            "--transient-resumes=3",
            "--max-turns=0",
            *source_args,
            config["target"],
        ]
    if mode == "incremental":
        return common + [
            "--incremental",
            "--policy-retries=2",
            "--transient-resumes=3",
            "--max-turns=0",
            *source_args,
        ]
    raise CIError(f"unsupported mode: {mode}")


def check_runner(config: dict) -> dict[str, str]:
    required = [
        [config["specula_binary"], "--version"],
        ["copilot", "--version"],
        ["gh", "--version"],
        ["java", "-version"],
        ["javac", "-version"],
        ["rustc", "--version"],
        ["cargo", "--version"],
    ]
    for command in required:
        try:
            subprocess.run(
                command,
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except (OSError, subprocess.CalledProcessError) as exc:
            raise CIError(f"runner prerequisite failed: {command[0]}") from exc

    identity = check_specula_source(config)
    device = Path("/dev/kvm")
    if not device.is_char_device() or not os.access(device, os.R_OK | os.W_OK):
        raise CIError("runner requires readable and writable /dev/kvm")
    return identity


def check_specula_source(config: dict) -> dict[str, str]:
    source = Path(config["specula_source"])
    revision = git(source, "rev-parse", "HEAD")
    minimum = config["specula_min_commit"]
    if not SHA.fullmatch(revision) or not SHA.fullmatch(minimum):
        raise CIError("Specula revisions must be full commit SHAs")
    if git(source, "remote", "get-url", "origin") != config["specula_repository"]:
        raise CIError("installed Specula source has an unexpected origin")
    if subprocess.run(
        [
            "git",
            *git_repo_args(source),
            "merge-base",
            "--is-ancestor",
            minimum,
            revision,
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    ).returncode:
        raise CIError(
            "installed Specula revision predates or diverges from the configured minimum"
        )
    venv = Path(config["specula_binary"]).resolve().parents[1]
    for entry in git(
        source, "status", "--porcelain=v1", "--untracked-files=all"
    ).splitlines():
        status, relative = entry[:2], entry[3:]
        path = source / relative
        if (
            status == "??"
            and relative in EXPECTED_SPECULA_LINKS
            and path.is_symlink()
            and path.resolve() == venv
        ):
            continue
        raise CIError(f"installed Specula source has an unexpected change: {entry}")
    try:
        completed = subprocess.run(
            [config["specula_binary"], "--version"],
            check=True,
            capture_output=True,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError) as exc:
        raise CIError("cannot determine installed Specula version") from exc
    version_text = completed.stdout.strip()
    version = parse_semver(version_text)
    minimum_version = parse_semver(config["specula_min_version"])
    maximum_version = parse_semver(config["specula_max_version_exclusive"])
    if not minimum_version <= version < maximum_version:
        raise CIError(
            "installed Specula version is outside the configured compatibility range"
        )
    return {"commit": revision, "version": ".".join(str(part) for part in version)}


def publish_report(
    config: dict,
    request_id: str,
    mode: str,
    revision: str,
    run_id: str | None,
    exit_code: int,
    before: set[str],
    specula_identity: dict[str, str] | None = None,
) -> Path:
    ci_dir = Path(config["state_root"]) / "state/openvmm-snapshot-restore"
    runs = ci_dir / "runs"
    if mode != "preflight" and run_id is None and runs.is_dir():
        created = sorted(
            (
                path
                for path in runs.iterdir()
                if path.is_dir() and path.name not in before
            ),
            key=lambda path: path.stat().st_mtime_ns,
        )
        run_id = created[-1].name if created else None
    report = Path(config["state_root"]) / "reports" / request_id
    if report.exists():
        if report.is_symlink() or not report.is_dir():
            raise CIError(f"report path is not a real directory: {report}")
        shutil.rmtree(report)
    report.mkdir(parents=True, exist_ok=True)
    selected = runs / run_id if run_id else None
    for name in (
        "summary.md",
        "ci-report.md",
        "ci-verdict.json",
        "resource-summary.json",
    ):
        matches = (
            sorted(selected.rglob(name), key=lambda path: len(path.parts))
            if selected and selected.is_dir()
            else []
        )
        if matches:
            shutil.copy2(matches[0], report / name)
    result = {
        "version": 1,
        "mode": mode,
        "revision": revision,
        "run_id": run_id,
        "exit_code": exit_code,
        "complete": exit_code in {0, 2},
        "specula": specula_identity,
    }
    (report / "result.json").write_text(
        json.dumps(result, indent=2, sort_keys=True) + "\n"
    )
    if not (report / "summary.md").exists():
        (report / "summary.md").write_text(
            "# Specula OpenVMM verification\n\n"
            f"- Mode: `{mode}`\n- Revision: `{revision}`\n"
            f"- Run: `{run_id or 'not started'}`\n- Exit code: `{exit_code}`\n"
        )
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with Path(output).open("a") as stream:
            stream.write(f"artifact_dir={report}\n")
    return report


def run_request(config: dict, args: argparse.Namespace) -> int:
    mode, tag, run_id = args.mode, args.tag, args.run_id
    if args.event_file:
        mode, tag, run_id = event_request(args.event_file, args.event_name or "")
    mode = mode or "incremental"
    if bool(tag) == bool(args.revision):
        raise CIError("specify exactly one of --tag or --revision")
    revision = resolve_nvx_tag(config, tag) if tag else args.revision
    assert revision is not None
    source = prepare_source(config, revision)
    specula_identity = check_runner(config)
    ci_dir = Path(config["state_root"]) / "state/openvmm-snapshot-restore"
    if mode == "preflight":
        report = publish_report(
            config,
            args.request_id,
            mode,
            revision,
            None,
            0,
            set(),
            specula_identity,
        )
        print(json.dumps({"status": "preflight_ready", "report": str(report)}))
        return 0
    if mode == "incremental" and not (ci_dir / "current").is_symlink():
        mode = "initialize"
    before = (
        {path.name for path in (ci_dir / "runs").iterdir()}
        if (ci_dir / "runs").is_dir()
        else set()
    )
    command = specula_command(config, mode, source, revision, run_id)
    result = subprocess.run(command)
    report = publish_report(
        config,
        args.request_id,
        mode,
        revision,
        run_id,
        result.returncode,
        before,
        specula_identity,
    )
    print(
        json.dumps(
            {
                "status": "complete" if result.returncode in {0, 2} else "incomplete",
                "report": str(report),
                "exit_code": result.returncode,
            }
        )
    )
    return result.returncode


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--mode", choices=("incremental", "initialize", "resume", "preflight")
    )
    parser.add_argument("--tag")
    parser.add_argument("--revision")
    parser.add_argument("--run-id")
    parser.add_argument("--request-id", default="local")
    parser.add_argument("--event-file", type=Path)
    parser.add_argument("--event-name")
    args = parser.parse_args(argv)
    try:
        config = read_json(HERE / "config.json")
        config["state_root"] = os.environ.get(
            "SPECULA_STATE_ROOT", config["state_root"]
        )
        config["specula_binary"] = os.environ.get(
            "SPECULA_BINARY", config["specula_binary"]
        )
        config["specula_source"] = os.environ.get(
            "SPECULA_SOURCE", config["specula_source"]
        )
        if not valid_id(args.request_id):
            raise CIError("request_id contains unsupported characters")
        with state_lock(config):
            return run_request(config, args)
    except (CIError, OSError, ValueError, json.JSONDecodeError) as exc:
        print(f"Specula CI preparation failed: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
