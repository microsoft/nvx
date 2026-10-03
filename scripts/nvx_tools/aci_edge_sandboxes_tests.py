"""Real-hypervisor end-to-end test of the aci_edge_sandboxes crate."""

from __future__ import annotations

import argparse
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import cast

from .build_constants import (
    AlpineBuildConstants,
    BuildConstants,
    KernelBuildConstants,
)
from .ci import OPENVMM_TEST_BACKENDS, validate_openvmm_test_backend
from .common import artifact_path, openvmm_binary_path, require_file

CRATE_DIRECTORY = BuildConstants.REPO_ROOT / "aci_edge_sandboxes"
E2E_TEST_NAME = "openvmm_e2e"


def configure_parser(parser: argparse.ArgumentParser) -> None:
    parser.description = (
        "Run the aci_edge_sandboxes lifecycle test against a real hypervisor with the Alpine "
        "guest initramfs."
    )
    parser.add_argument("--backend", choices=OPENVMM_TEST_BACKENDS, required=True)
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=BuildConstants.BUILD_DIR / "test-results" / "aci_edge_sandboxes",
        help="directory that receives the OpenVMM log on failure",
    )
    parser.add_argument(
        "--cargo",
        default="cargo",
        help="cargo executable used to build and run the test (default: cargo)",
    )
    parser.set_defaults(handler=command_test_aci_edge_sandboxes)


def e2e_environment(backend: str, state_root: Path, output_dir: Path) -> dict[str, str]:
    paths = {
        "ACI_EDGE_SANDBOXES_E2E_OPENVMM": require_file(
            openvmm_binary_path(), "OpenVMM release binary"
        ),
        "ACI_EDGE_SANDBOXES_E2E_KERNEL": require_file(
            artifact_path(KernelBuildConstants.BINARY_NAME), "Linux direct kernel"
        ),
        "ACI_EDGE_SANDBOXES_E2E_INITRD": require_file(
            artifact_path(AlpineBuildConstants.INITRAMFS_NAME), "Alpine initramfs"
        ),
        "ACI_EDGE_SANDBOXES_E2E_STATE_ROOT": state_root,
        "ACI_EDGE_SANDBOXES_E2E_OUTPUT_DIR": output_dir,
    }
    environment = {name: os.fspath(path.resolve()) for name, path in paths.items()}
    environment["ACI_EDGE_SANDBOXES_E2E_HYPERVISOR"] = backend
    return environment


def e2e_command(cargo: str) -> list[str]:
    return [
        cargo,
        "test",
        "--manifest-path",
        os.fspath(CRATE_DIRECTORY / "Cargo.toml"),
        "--locked",
        "--test",
        E2E_TEST_NAME,
        "--",
        "--ignored",
        "--nocapture",
    ]


def remaining_sandboxes(state_root: Path) -> list[Path]:
    """Returns the sandboxes that the tests left provisioned.

    Deprovisioning deletes a sandbox's directory, including its `sandbox.json`, only
    once its VM has stopped, so every remaining record marks a VM that may still run.
    """
    if not state_root.is_dir():
        return []
    return sorted(record.parent for record in state_root.rglob("sandbox.json"))


def recorded_pid(sandbox: Path) -> int | None:
    """Returns the OpenVMM process ID in a sandbox's runtime or launch record."""
    for name, keys in (("runtime.json", ("pid",)), ("launch.json", ("process", "pid"))):
        try:
            value: object = json.loads((sandbox / name).read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        for key in keys:
            if not isinstance(value, dict):
                value = None
                break
            value = cast(dict[str, object], value).get(key)
        if isinstance(value, int):
            return value
    return None


def release_state(directory: Path, state_root: Path) -> None:
    """Deletes the sandbox state once no VM may still need it, or keeps and reports it.

    A test process that is interrupted, or whose cleanup fails to stop or deprovision a
    sandbox, leaves a detached VM running. Deleting its state would discard the process
    identity and control capability that stopping it requires.
    """
    remaining = remaining_sandboxes(state_root)
    if not remaining:
        shutil.rmtree(directory, ignore_errors=True)
        return
    print(
        f"warning: kept {state_root} because {len(remaining)} sandbox(es) were not "
        "deprovisioned, so their VMs may still run:",
        file=sys.stderr,
    )
    for sandbox in remaining:
        pid = recorded_pid(sandbox)
        process = "no recorded OpenVMM process" if pid is None else f"OpenVMM pid {pid}"
        print(f"  {sandbox} ({process})", file=sys.stderr)
    print("Stop those processes, then delete the directory.", file=sys.stderr)


def command_test_aci_edge_sandboxes(args: argparse.Namespace) -> int:
    backend = cast(str, args.backend)
    validate_openvmm_test_backend(backend)
    output_dir = cast(Path, args.output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix="aci-edge-sandboxes-e2e-"))
    state_root = directory / "state"
    try:
        environment = os.environ.copy()
        environment.update(e2e_environment(backend, state_root, output_dir))
        command = e2e_command(cast(str, args.cargo))
        print(f">> {shlex.join(command)}", flush=True)
        return subprocess.run(command, env=environment, check=False).returncode
    finally:
        release_state(directory, state_root)
