#!/opt/venv/bin/python
"""Initialize the isolated data home, validate limits, then invoke native Specula."""

import asyncio
import importlib
import importlib.metadata
import json
import os
from pathlib import Path
import subprocess
import sys

from common import SECRET_NAMES, clean_env, now, write_json

ROOT = Path("/opt/specula-native")
PIN = "088049c5b3474340213cded2664cdb674bff1e1a"
POLICY = {
    "ci-dir": "/work/ci", "agent": "copilot-cli", "model": "gpt-5.6-sol-fast",
    "effort": "xhigh", "max-parallel": "1", "policy-retries": "2",
    "transient-resumes": "3", "tlc-memory-limit": "12G", "tlc-worker-limit": "4",
    "max-turns": "0",
}


def native_args(arguments):
    args = list(arguments)
    resume = any(arg.startswith("--run-id=") for arg in args)
    incremental = "--incremental" in args
    policy = dict(POLICY)
    if incremental or resume:
        policy.pop("max-parallel")
    if resume:
        policy.pop("policy-retries")
        policy.pop("transient-resumes")
    if sum(arg.startswith("--artifact=") for arg in args) > 1:
        raise ValueError("Specify --artifact exactly once so validation and execution use the same source.")
    if "--ci-init" not in args and not incremental and not resume:
        raise ValueError("Choose --ci-init, --incremental, or an existing --run-id.")
    for arg in args:
        if arg.startswith("--skip-") or arg in {"--fresh-context", "--no-isolate", "--enable-reviews"}:
            raise ValueError("Persistent native CI does not support skip/review/fresh-context flags.")
        if arg.startswith(("--agent-config", "--claude-alias", "--findings-from")):
            raise ValueError("Alternate agent routing or historical finding imports are not enabled.")
        if arg.startswith("--") and "=" in arg:
            key, value = arg[2:].split("=", 1)
            if key == "max-parallel" and (incremental or resume):
                raise ValueError("Incremental mode is one conversation; resume restores its saved parallelism.")
            if key in {"policy-retries", "transient-resumes"} and resume:
                raise ValueError("Resume must preserve the run's saved recovery budgets.")
            if key in POLICY and value != POLICY[key]:
                raise ValueError(f"Runtime policy fixes --{key}={POLICY[key]}.")
    additions = [
        f"--{key}={value}" for key, value in policy.items()
        if not any(arg.startswith(f"--{key}=") for arg in args)
    ]
    if not any(arg.startswith(("--artifact=", "--run-id=")) for arg in args):
        additions.append("--artifact=/source")
    for arg in args:
        if arg.startswith("--artifact="):
            value = arg.split("=", 1)[1]
            path = Path(value)
            if value != "/source" and (
                not path.is_absolute() or ".." in path.parts or not path.is_relative_to("/sources")
                or value == "/sources"
            ):
                raise ValueError("Native source must be /source or a clone below read-only /sources.")
    return [str(ROOT / "specula"), "run", *additions, *args]


def setup():
    for key in SECRET_NAMES:
        os.environ.pop(key, None)
    settings = {
        "HOME": "/work/home", "COPILOT_HOME": "/work/home/.copilot",
        "TMPDIR": "/work/scratch", "TMP": "/work/scratch", "TEMP": "/work/scratch",
        "TLC_STATE_DIR": "/work/tlc-states", "SPECULA_TLC_RESOURCE_DIR": "/work/tlc-resources",
        "CARGO_HOME": "/cache/cargo", "CARGO_TARGET_DIR": "/cache/target",
        "CARGO_BUILD_JOBS": "4", "CARGO_PROFILE_DEV_DEBUG": "0", "CARGO_PROFILE_TEST_DEBUG": "0",
        "XDG_CACHE_HOME": "/cache/xdg", "UV_CACHE_DIR": "/cache/uv",
        "PIP_CACHE_DIR": "/cache/pip", "NPM_CONFIG_CACHE": "/cache/npm",
        "SPECULA_TLC_MEMORY_LIMIT": "12G", "SPECULA_TLC_WORKER_LIMIT": "4",
        "GIT_TERMINAL_PROMPT": "0", "GH_PROMPT_DISABLED": "1", "TERM": "dumb",
        "CI": "true", "COPILOT_AUTO_UPDATE": "false", "PYTHONDONTWRITEBYTECODE": "1",
        "JAVA_TOOL_OPTIONS": (
            "-Xmx4g -XX:MaxDirectMemorySize=1g -XX:ActiveProcessorCount=4 "
            "-Djava.io.tmpdir=/work/scratch -Duser.home=/work/home"
        ),
        "MAVEN_OPTS": "-Dmaven.repo.local=/cache/maven",
        "PROTOC": "/opt/native-runtime/protoc/bin/protoc",
        "PROTOC_INCLUDE": "/opt/native-runtime/protoc/include",
    }
    os.environ.update(settings)
    for key in (
        "HOME", "COPILOT_HOME", "TMPDIR", "TLC_STATE_DIR", "SPECULA_TLC_RESOURCE_DIR",
        "CARGO_HOME", "CARGO_TARGET_DIR", "XDG_CACHE_HOME", "UV_CACHE_DIR",
        "PIP_CACHE_DIR", "NPM_CONFIG_CACHE",
    ):
        Path(settings[key]).mkdir(parents=True, exist_ok=True)
    Path("/cache/maven").mkdir(exist_ok=True)
    limits = {
        key: (Path("/sys/fs/cgroup") / key).read_text().strip()
        for key in ("memory.max", "memory.swap.max", "cpu.max", "pids.max")
    }
    quota, period = map(int, limits["cpu.max"].split())
    process_status = dict(
        line.split(":", 1) for line in Path("/proc/self/status").read_text().splitlines()
        if ":" in line
    )
    run_fs = os.statvfs("/run")
    if (
        limits["memory.max"] != str(26 * 1024**3)
        or limits["memory.swap.max"] != "0"
        or quota != 6 * period or limits["pids.max"] != "1024"
        or os.getuid() != 1001 or os.getgid() != 1003 or 998 not in os.getgroups()
        or not os.access("/dev/mshv", os.R_OK | os.W_OK)
        or int(process_status["CapEff"].strip(), 16) != 0
        or process_status["NoNewPrivs"].strip() != "1"
        or run_fs.f_blocks * run_fs.f_frsize != 256 * 1024**2
    ):
        raise ValueError("Required cgroup limits, identity, or real MSHV device access differ.")
    if Path("/opt/native-runtime/specula-commit").read_text().strip() != PIN:
        raise ValueError("Specula image pin differs.")
    protoc_version = subprocess.check_output([settings["PROTOC"], "--version"], text=True).strip()
    if protoc_version != "libprotoc 27.1":
        raise ValueError("Runtime protobuf compiler differs from the source's pinned version.")
    for module in ("mcp", "jsonschema", "copilot", "specula.ci_workflow", "specula.tlc_tasks"):
        importlib.import_module(module)
    from copilot import CopilotClient, RuntimeConnection
    if not hasattr(RuntimeConnection, "for_stdio") or not hasattr(CopilotClient, "resume_session"):
        raise ValueError("Copilot SDK lacks native compaction prerequisites.")
    from specula.skill_install import install_skills
    result = install_skills(ROOT / "skills", Path("/work/home/.agents/skills"))
    if not result.complete:
        raise ValueError("Current Specula skills conflict with existing runtime-home skills.")
    servers = {}
    for name, tool in (
        ("tracedebugger", "trace_debugger"), ("spec_analyzer", "spec_analyzer"),
        ("inv_checking_tool", "inv_checking_tool"),
    ):
        servers[name] = {
            "type": "local", "command": "/opt/venv/bin/python",
            "args": [str(ROOT / "tools" / tool / "mcp_server.py")],
            "tools": ["*"], "env": {"SPECULA_ROOT": str(ROOT)},
        }
    for tool in ("trace_debugger", "spec_analyzer", "inv_checking_tool", "tlc_tools", "context_control"):
        if not (ROOT / "tools" / tool / ".venv/bin/python").is_file():
            raise ValueError(f"Required tool environment missing: {tool}")
    write_json("/work/home/.copilot/mcp-config.json", {"mcpServers": servers})
    record = {
        "at": now(), "specula_commit": PIN, "limits": limits,
        "uid": os.getuid(), "gid": os.getgid(), "groups": os.getgroups(),
        "effective_capabilities": process_status["CapEff"].strip(),
        "no_new_privileges": process_status["NoNewPrivs"].strip(),
        "run_tmpfs_bytes": run_fs.f_blocks * run_fs.f_frsize,
        "mshv_access": True, "skill_count": len(result.linked) + len(result.existing),
        "mcp_version": importlib.metadata.version("mcp"),
        "copilot_sdk_version": importlib.metadata.version("github-copilot-sdk"),
        "protoc": settings["PROTOC"],
        "protoc_version": protoc_version,
        "tlc_tools": "installed; registered by native phase launcher",
        "context_compaction": "installed; optional, native/model operation not preflight-tested",
    }
    write_json(Path(os.environ["NATIVE_ATTEMPT_DIR"]) / "readiness.json", record)
    return record


def probe():
    from copilot_wrapper import CLI, SAFETY
    subprocess.run([CLI, "--help", *SAFETY], check=True, stdout=subprocess.DEVNULL)
    for command in (
        ["copilot", "--version"], ["java", "-version"], ["rustc", "--version"],
        ["cargo", "nextest", "--version"], ["git", "--version"],
        [str(ROOT / "specula"), "--version"],
        ["/opt/venv/bin/python", "-c",
         "from specula import ci_workflow, persistent_findings, context_runner; print('Native imports OK')"],
    ):
        subprocess.run(command, check=True)
    asyncio.run(probe_mcp())
    print("Runtime preflight passed. No model or OpenVMM test was invoked.", flush=True)
    return 0


async def probe_mcp():
    from mcp import ClientSession, StdioServerParameters
    from mcp.client.stdio import stdio_client
    inventories = {}
    for tool in ("trace_debugger", "spec_analyzer", "inv_checking_tool", "tlc_tools", "context_control"):
        params = StdioServerParameters(
            command="/opt/venv/bin/python",
            args=[str(ROOT / "tools" / tool / "mcp_server.py")],
            env=clean_env(),
        )
        async with stdio_client(params) as (reader, writer):
            async with ClientSession(reader, writer) as session:
                await asyncio.wait_for(session.initialize(), timeout=30)
                listing = await asyncio.wait_for(session.list_tools(), timeout=30)
                inventories[tool] = [item.name for item in listing.tools]
                if not inventories[tool]:
                    raise ValueError(f"MCP helper has no tools: {tool}")
    write_json(Path(os.environ["NATIVE_ATTEMPT_DIR"]) / "mcp-tools.json", inventories)
    print("All five local MCP servers initialized and listed tools.", flush=True)


def auth_probe():
    """Explicit opt-in model call; ordinary preflight never calls this."""
    command = [
        "copilot", "-p", "Reply exactly AUTH_OK. Do not use tools or access files.",
        "--model", POLICY["model"], "--reasoning-effort", POLICY["effort"],
        "--silent", "--available-tools=",
    ]
    env = clean_env()
    env["SPECULA_AGENT_TIMEOUT_SECONDS"] = str(min(120, int(env.get("SPECULA_AGENT_TIMEOUT_SECONDS", "120"))))
    result = subprocess.run(command, text=True, capture_output=True, env=env)
    directory = Path(os.environ["NATIVE_ATTEMPT_DIR"])
    (directory / "auth-response.log").write_text(result.stdout + result.stderr)
    accepted = result.returncode == 0 and result.stdout.strip() == "AUTH_OK"
    write_json(directory / "auth-result.json", {"accepted": accepted, "exit_code": result.returncode})
    print("AUTH_OK" if accepted else "Authentication probe did not return the exact requested response.", flush=True)
    return 0 if accepted else result.returncode or 1


def main():
    try:
        mode, *args = sys.argv[1:]
        if args[:1] == ["--"]:
            args.pop(0)
        setup()
        if mode == "probe":
            return probe()
        if mode == "auth-probe":
            return auth_probe()
        if mode == "native":
            command = native_args(args)
            explicit_source = next((arg.split("=", 1)[1] for arg in command if arg.startswith("--artifact=")), None)
            if explicit_source is not None:
                source = Path(explicit_source)
                if not (source / ".git").is_dir() or (source / ".gitmodules").exists():
                    raise ValueError("Native input requires a normal clone without submodules.")
                dirty = subprocess.check_output(
                    ["git", "-c", f"safe.directory={source}", "-C", str(source),
                     "status", "--porcelain", "--untracked-files=normal"], text=True,
                )
                if dirty:
                    raise ValueError("Native input source must be clean.")
            args = command
        elif mode != "exec" or not args:
            raise ValueError("Choose probe, native, or exec with a command.")
        write_json(Path(os.environ["NATIVE_ATTEMPT_DIR"]) / "command.json",
                   {"mode": mode, "argv": args, "at": now()})
        os.execvpe(args[0], args, clean_env())
    except (OSError, ValueError, subprocess.CalledProcessError) as exc:
        print(f"Native runtime preflight failed: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
