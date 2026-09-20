#!/usr/bin/env python3
"""Image-local native store operations, without an agent or GitHub client."""

import argparse
from collections import Counter
from pathlib import Path
import sys

from specula.ci_store import CIError, CIStore, git, read_json, write_json


def completed_snapshot(store, token):
    state = store.snapshot(token)
    parts = Path(token).parts
    if (len(parts) != 4 or parts[0] != "runs" or parts[2] != "ci-published"
            or state["run_id"] != parts[1]):
        raise CIError("publication token and native run identity differ")
    receipt = read_json(store.path(f"runs/{state['run_id']}/ci-result.json"))
    if (receipt.get("complete") is not True or receipt.get("snapshot") != token
            or receipt.get("run_id") != state["run_id"]
            or receipt.get("verdict") != state["verdict"]
            or state.get("verdict") not in {"PASS", "WARNING", "FAIL"}
            or state.get("verification") != "Completed verification workflow."
            or state.get("dirty") is not False):
        raise CIError("publication lacks a consistent complete clean-source receipt")
    return state, receipt


def describe(state):
    verdict = read_json(Path(state["model_path"]) / "ci-verdict.json")
    findings = verdict["findings"]
    return {
        key: state[key] for key in (
            "token", "run_id", "target", "source_commit", "snapshot_commit",
            "previous", "verdict", "check_key",
        )
    } | {"finding_counts": dict(Counter(item["status"] for item in findings))}


def operate(root, operation, *, token=None, revision=None, target=None, verdict=None):
    store = CIStore(Path(root))
    store.acquire()
    try:
        current = store.current_token()
        if operation == "inspect":
            selected = token or current
            return {"current": describe(completed_snapshot(store, selected)[0]) if selected else None}
        if not token or not revision or not target or verdict not in {"PASS", "WARNING", "FAIL"}:
            raise CIError("promotion requires the exact token, source, target and verdict")
        state, receipt = completed_snapshot(store, token)
        if state["source_commit"] != revision or state["target"] != target or state["verdict"] != verdict:
            raise CIError("publication does not match the outer runtime result")
        if operation == "verify-initialization":
            if receipt.get("candidate") is not False or state["previous"] is not None or current != token:
                raise CIError("initialization must be complete in its private, otherwise empty store")
        elif operation == "promote":
            if receipt.get("candidate") is not True or state["previous"] is None:
                raise CIError("only an incremental native candidate may be promoted")
            if current != token:
                if current != state["previous"]:
                    raise CIError("current advanced since candidate preparation")
                previous, _ = completed_snapshot(store, current)
                git(store.path(state["source"]), "merge-base", "--is-ancestor",
                    previous["source_commit"], state["source_commit"])
                store.advance(token)
        else:
            raise CIError("unsupported native store operation")
        return {"current": describe(state)}
    finally:
        store.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("inspect", "promote", "verify-initialization"))
    parser.add_argument("--ci-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--token")
    parser.add_argument("--revision")
    parser.add_argument("--target")
    parser.add_argument("--verdict")
    args = parser.parse_args()
    try:
        result = operate(args.ci_dir, args.operation, token=args.token, revision=args.revision,
                         target=args.target, verdict=args.verdict)
        write_json(args.output, result)
        return 0
    except (CIError, OSError, ValueError, KeyError, TypeError) as exc:
        print(f"Native store operation refused: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
