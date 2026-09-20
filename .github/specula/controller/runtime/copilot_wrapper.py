#!/opt/venv/bin/python
"""Dedicated model authentication with report-only publication guards."""

import json
import os
from pathlib import Path
import signal
import subprocess
import sys

from common import SECRET_NAMES, agent_event, clean_env

CLI = "/opt/copilot/copilot"
SAFETY = [
    "--disable-builtin-mcps", "--no-remote-export", "--no-ask-user",
    "--no-auto-update", "--no-bash-env",
    "--secret-env-vars=" + ",".join(SECRET_NAMES),
    "--deny-tool=shell(git push)", "--deny-tool=shell(gh:*)",
    "--deny-tool=read(/run/secrets/**)", "--deny-tool=ask_user",
]


def safe_args(args):
    prohibited = {"login", "logout", "update", "--remote-export", "--share-gist"}
    if any(arg.split("=", 1)[0] in prohibited for arg in args):
        raise ValueError("Authentication changes and remote publication are disabled.")
    probe = any(arg in {"--help", "-h", "--version"} for arg in args)
    return args if probe else [*args, *SAFETY], probe


def main():
    try:
        args, probe = safe_args(sys.argv[1:])
        env = clean_env()
        if probe:
            os.execve(CLI, [CLI, *args], env)
        credential = json.loads(Path("/run/secrets/copilot-auth.json").read_text())
        identities = credential.get("authTokens", {})
        if not isinstance(identities, dict) or len(identities) != 1:
            raise ValueError("Exactly one dedicated Copilot identity is required.")
        token = next(iter(identities.values())).get("token")
        if not isinstance(token, str) or not token:
            raise ValueError("Dedicated Copilot credential is unavailable.")
        env["COPILOT_GITHUB_TOKEN"] = token
        from specula.resumelib import inherited_run_lock_fds
        timeout = int(env.get("SPECULA_AGENT_TIMEOUT_SECONDS", "21600"))
        child = subprocess.Popen(
            [CLI, *args], env=env, start_new_session=True,
            pass_fds=inherited_run_lock_fds(),
        )

        def interrupted(_signum, _frame):
            raise KeyboardInterrupt

        signal.signal(signal.SIGTERM, interrupted)
        try:
            code = child.wait(timeout=timeout)
        except (subprocess.TimeoutExpired, KeyboardInterrupt) as exc:
            kind = "phase_timeout" if isinstance(exc, subprocess.TimeoutExpired) else "interrupted"
            agent_event(kind, phase=os.environ.get("SPECULA_PHASE"))
            os.killpg(child.pid, signal.SIGTERM)
            try:
                child.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
            return 124 if kind == "phase_timeout" else 130
        agent_event("agent_exit", exit_code=code, phase=os.environ.get("SPECULA_PHASE"))
        return code if code >= 0 else 128 - code
    except (OSError, ValueError, KeyError, TypeError, AttributeError):
        # Never print exception values derived from the credential document.
        print("Copilot runtime configuration/authentication preflight failed.", file=sys.stderr)
        agent_event("auth_or_configuration_error")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
