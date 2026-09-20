#!/usr/bin/env python3
"""Release CI orchestration: retained-asset bootstrap, then native incremental."""

import hashlib
import json
import os
from pathlib import Path
import sys
import time

import release


def dispatch(config, request, *, backend_factory=release.Backend, bundle_directory=release.HERE):
    config = {**config, "_deadline": time.monotonic() + config["timeout_seconds"]}
    options = {"backend_factory": backend_factory, "bundle_directory": bundle_directory}
    if request.mode != "incremental" or request.preflight or not config.get("bootstrap_revision"):
        return release.run_request(config, request, **options)
    probe_id = "readiness-" + hashlib.sha256(request.request_id.encode()).hexdigest()[:24]
    probe = release.Request("preflight", request.tag, request.revision, None, probe_id)
    readiness = release.run_request(config, probe, **options)
    record = readiness[2]
    if record["status"] == "preflight_ready":
        return release.run_request(config, request, **options)
    if record["status"] != "needs_initialization" or len(record.get("blockers", [])) != 1:
        return readiness
    revision = config["bootstrap_revision"]
    if not isinstance(revision, str) or not release.SHA.fullmatch(revision):
        raise release.ReleaseError("invalid_config", "bootstrap_revision must be a full commit SHA.")
    seed = release.initialization_seed(config)
    if seed is None:
        raise release.ReleaseError("invalid_config", "Automatic bootstrap requires pinned retained assets.")
    cache = Path(config["root"]) / "repos/release-cache.git"
    relation = release.git(cache, "merge-base", "--is-ancestor", revision, record["revision"], check=False)
    if relation.returncode:
        raise release.ReleaseError("unsupported_bootstrap_ancestry", "Configured bootstrap source is not an ancestor of this release.")
    identity = [revision, config["target"], config["image"], config["environment_id"], seed]
    bootstrap_id = "bootstrap-" + hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()[:24]
    work = Path(config["work"])
    directory = release.confined(work / "releases/requests" / bootstrap_id, work)
    run_id = None
    public = directory / "public/result.json"
    operation = directory / "operation.json"
    if public.exists():
        run_id = release.read_json(public).get("native_run")
    if operation.exists():
        launch = release.read_json(operation).get("launch_journal")
        if launch:
            launch_path = release.confined(Path(launch), directory / "launches")
            marker_path = release.confined(work / "releases/reconciled" / (launch_path.stem + ".json"), work)
            if marker_path.exists():
                run_id = release.read_json(marker_path).get("native_run") or run_id
    bootstrap = release.Request("resume" if run_id else "initialize", None, revision, run_id, bootstrap_id)
    result = release.run_request(config, bootstrap, **options)
    if result[0] not in (0, 2) or result[2].get("complete") is not True:
        failed = {**result[2], "requested_release_revision": record["revision"],
                  "requested_release_tag": request.tag, "requested_release": record["release"],
                  "requested_release_verified": False}
        return release.finish_report(result[1].parent, failed)
    result = release.run_request(config, request, **options)
    final = {**result[2], "bootstrap_request": bootstrap_id, "bootstrap_source": revision}
    return release.finish_report(result[1].parent, final)


def main(argv=None):
    args = release.parse_cli_args(argv)
    try:
        config = release.load_config(args.config)
        request = release.request_from_args(args, config)
        code, public, record = dispatch(config, request)
        if os.environ.get("GITHUB_OUTPUT"):
            with Path(os.environ["GITHUB_OUTPUT"]).open("a") as stream:
                stream.write(f"artifact_dir={public}\nstatus={record['status']}\n")
        print(json.dumps({"status": record["status"], "report": str(public), "exit_code": code}))
        return code
    except (release.ReleaseError, OSError, KeyError, TypeError, ValueError) as exc:
        print(f"CI preparation failed: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
