#!/opt/venv/bin/python
"""Prevent Git publication and inherited credential-helper use."""

import os
import sys

from common import clean_env


def checked_args(args):
    if any(arg in {"push", "send-pack", "receive-pack", "http-push"} for arg in args):
        raise ValueError("Git publication is disabled in this local runtime.")
    if any("alias." in arg or "credential.helper" in arg for arg in args):
        raise ValueError("Git aliases and credential-helper overrides are disabled.")
    return [
        "-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false",
        "-c", "credential.helper=", *args,
    ]


def main():
    try:
        args = checked_args(sys.argv[1:])
    except ValueError as exc:
        print(str(exc), file=sys.stderr)
        return 126
    env = clean_env()
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null", GIT_TERMINAL_PROMPT="0")
    os.execve("/opt/native-runtime/libexec/git", ["git", *args], env)


if __name__ == "__main__":
    raise SystemExit(main())
