"""Small shared helpers; no credentials are included in runtime records."""

import datetime
import json
import os
from pathlib import Path

SECRET_NAMES = (
    "COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "SSH_AUTH_SOCK",
)


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def clean_env():
    env = os.environ.copy()
    for name in SECRET_NAMES:
        env.pop(name, None)
    return env


def write_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    staging = path.with_name(f".{path.name}.{os.getpid()}")
    with staging.open("w") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    staging.replace(path)


def agent_event(kind, **values):
    root = os.environ.get("NATIVE_ATTEMPT_DIR")
    if root:
        with (Path(root) / "agent-events.jsonl").open("a") as stream:
            stream.write(json.dumps({"at": now(), "kind": kind, **values}) + "\n")
