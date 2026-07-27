"""Cross-platform VM smoke-test workflows."""

from __future__ import annotations

import tempfile
from dataclasses import dataclass
from pathlib import Path
import shutil
from typing import Callable

from .backends.base import HostBackend
from .common import (
    CommandResult,
    REPO_ROOT,
    ScriptError,
    require_file,
    require_tool,
    run_capture,
    run_checked,
)
from .vm import BOOT_MARKER, DEFAULT_CMDLINE


Runner = Callable[..., CommandResult]
HOST_ERROR_PREFIX = "NVX-HOST-ERROR:"


@dataclass(frozen=True)
class ExecTestConfig:
    microvm: Path
    kernel: Path
    initrd: Path
    mem: int = 128
    timeout: int = 60


@dataclass(frozen=True)
class ProfilingTestConfig:
    microvm: Path
    kernel: Path
    initrd: Path
    timeout: int = 120


def _require_result(
    result: CommandResult,
    expected_status: int,
    *,
    required_output: str = "",
    required_error: str = "",
) -> None:
    if result.timed_out:
        raise ScriptError("microvm timed out")
    if result.returncode != expected_status:
        raise ScriptError(
            f"microvm exited {result.returncode}, expected {expected_status}\n{result.text}"
        )
    output = result.stdout.decode("utf-8", errors="replace")
    error = result.stderr.decode("utf-8", errors="replace")
    if required_output and required_output not in output:
        raise ScriptError(
            f"microvm stdout did not contain {required_output!r}\n{result.text}"
        )
    if required_error and required_error not in error:
        raise ScriptError(
            f"microvm stderr did not contain {required_error!r}\n{result.text}"
        )


def _run_exec_case(
    config: ExecTestConfig,
    backend: HostBackend,
    mount: Path,
    guest_path: str,
    expected_status: int,
    marker: str,
    *,
    runner: Runner,
) -> None:
    args: list[str | Path] = [
        config.microvm,
        "--kernel",
        config.kernel,
        "--initrd",
        config.initrd,
        "--mem",
        str(config.mem),
    ]
    backend.add_vcpus(args, 2 if backend.supports_vcpus else 1)
    args.extend(
        [
            "--mount",
            mount,
            "--exec",
            guest_path,
            "--log-level",
            "off",
        ]
    )
    result = runner(args, timeout=config.timeout)
    _require_result(result, expected_status, required_output=marker)
    print(f"guest exec status {expected_status} propagated")


def _build_kvm_ap_helper(destination: Path) -> None:
    compiler = require_tool("cc", "KVM exec smoke testing requires cc")
    run_checked(
        [
            compiler,
            "-nostdlib",
            "-static",
            "-Os",
            "-s",
            "-o",
            destination,
            REPO_ROOT / "scripts" / "tests" / "fixtures" / "kvm-pin-ap.c",
        ]
    )


def test_exec(
    config: ExecTestConfig,
    backend: HostBackend,
    *,
    runner: Runner = run_capture,
) -> None:
    require_file(config.microvm, f"missing VMM: {config.microvm}")
    require_file(config.kernel, f"missing kernel: {config.kernel}")
    require_file(config.initrd, f"missing initrd: {config.initrd}")

    with tempfile.TemporaryDirectory(prefix="nvx-exec-") as temporary:
        work_root = Path(temporary)
        mount_root = work_root / "mount"
        mount_root.mkdir()
        script_path = mount_root / "workload.sh"
        if backend.supports_vcpus:
            _build_kvm_ap_helper(mount_root / "kvm-pin-ap")

        for exit_code in (0, 37):
            marker = f"NVX-EXEC-SMOKE-{exit_code}"
            lines = [f"echo {marker}"]
            if exit_code == 37 and backend.supports_vcpus:
                lines.extend(
                    [
                        "if ! /mnt/host/kvm-pin-ap; then",
                        "    echo NVX-KVM-EXEC-AP-PIN-FAIL",
                        "    exit 98",
                        "fi",
                        "/sbin/nvx-exit 37",
                        "exit 99",
                    ]
                )
            else:
                lines.append(f"exit {exit_code}")
            script_path.write_text(
                "\n".join(lines) + "\n", encoding="utf-8", newline="\n"
            )
            _run_exec_case(
                config,
                backend,
                mount_root,
                "/mnt/host/workload.sh",
                exit_code,
                marker,
                runner=runner,
            )

        legacy_marker = "NVX-LEGACY-SHUTDOWN"
        script_path.write_text(
            f"echo {legacy_marker}\nexit 37\n", encoding="utf-8", newline="\n"
        )
        legacy_args: list[str | Path] = [
            config.microvm,
            "--kernel",
            config.kernel,
            "--initrd",
            config.initrd,
            "--mem",
            str(config.mem),
        ]
        backend.add_vcpus(legacy_args, 2 if backend.supports_vcpus else 1)
        legacy_args.extend(
            [
                "--mount",
                mount_root,
                "--cmdline",
                f"{DEFAULT_CMDLINE} nvx_exec=/mnt/host/workload.sh",
                "--log-level",
                "off",
            ]
        )
        result = runner(legacy_args, timeout=config.timeout)
        _require_result(result, 0, required_output=legacy_marker)
        print("legacy shutdown payload ignored")

        script_path.unlink()
        _run_exec_case(
            config,
            backend,
            mount_root,
            "/mnt/host/missing.sh",
            127,
            "executable script not found",
            runner=runner,
        )

        if backend.name == "windows-whp":
            _test_windows_exec_extensions(
                config,
                mount_root,
                work_root / "snapshot",
                script_path,
                runner,
            )

    print(f"{backend.name} exec smoke test passed")


def _test_windows_exec_extensions(
    config: ExecTestConfig,
    mount_root: Path,
    snapshot: Path,
    script_path: Path,
    runner: Runner,
) -> None:
    script_path.write_text(
        "echo NVX-WHP-SNAPSHOT-ORIGINAL\nexit 0\n",
        encoding="utf-8",
        newline="\n",
    )
    capture = runner(
        [
            config.microvm,
            "--kernel",
            config.kernel,
            "--initrd",
            config.initrd,
            "--mem",
            str(config.mem),
            "--mount",
            mount_root,
            "--exec",
            "/mnt/host/workload.sh",
            "--snapshot",
            snapshot,
            "--snapshot-before-exec",
            "--log-level",
            "off",
        ],
        timeout=config.timeout,
    )
    _require_result(capture, 0)

    script_path.write_text(
        "echo NVX-WHP-SNAPSHOT-RESTORED\nexit 41\n",
        encoding="utf-8",
        newline="\n",
    )
    restore = runner(
        [
            config.microvm,
            "--restore",
            snapshot,
            "--mount",
            mount_root,
            "--output-after-marker",
            "NVX-EXEC-START",
            "--log-level",
            "off",
        ],
        timeout=config.timeout,
    )
    _require_result(restore, 41, required_output="NVX-WHP-SNAPSHOT-RESTORED")

    semantic = runner(
        [config.microvm, "--mount", mount_root, "--exec", "relative.sh"],
        timeout=config.timeout,
    )
    _require_result(semantic, 1, required_error=HOST_ERROR_PREFIX)

    runtime = runner(
        [
            config.microvm,
            "--kernel",
            mount_root.parent / "missing-vmlinux",
            "--initrd",
            config.initrd,
        ],
        timeout=config.timeout,
    )
    _require_result(runtime, 1, required_error=HOST_ERROR_PREFIX)


def _perf_record_is_usable(work_root: Path, *, runner: Runner = run_capture) -> bool:
    perf = shutil.which("perf")
    if perf is None:
        return False
    probe = work_root / "probe.perf.data"
    result = runner([perf, "record", "-o", probe, "--", "true"])
    return (
        result.returncode == 0
        and not result.timed_out
        and probe.is_file()
        and probe.read_bytes()[:8] == b"PERFILE2"
    )


def _folded_sample_count(path: Path) -> int:
    if not path.is_file():
        raise ScriptError(f"guest folded profile was not written: {path}")
    samples = 0
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        fields = line.split()
        if len(fields) < 2:
            continue
        try:
            count = int(fields[-1])
        except ValueError:
            continue
        if count > 0:
            samples += count
    if samples == 0:
        raise ScriptError(f"guest folded profile has no sampled stacks: {path}")
    return samples


def test_profiling(
    config: ProfilingTestConfig,
    *,
    runner: Runner = run_capture,
) -> None:
    require_file(config.microvm, "build the VMM first: cargo build --release")
    require_file(
        config.kernel,
        f"missing profiling kernel: {config.kernel} (run build-kernel --profiling)",
    )
    require_file(
        config.initrd,
        f"missing initrd: {config.initrd} (run build-initramfs)",
    )

    with tempfile.TemporaryDirectory(prefix="nvx-profiling-") as temporary:
        work_root = Path(temporary)
        folded = work_root / "smoke.folded"
        host_capable = _perf_record_is_usable(work_root, runner=runner)
        if host_capable:
            print(">> perf record is usable; enabling host tracing")
        else:
            print(
                "::notice::perf record unavailable/unpermitted; "
                "running guest-only profiling smoke test"
            )

        args: list[str | Path] = [
            config.microvm,
            "--kernel",
            config.kernel,
            "--initrd",
            config.initrd,
            "--mem",
            "512",
            "--cmdline",
            DEFAULT_CMDLINE,
            "--guest-profile",
            folded,
            "--profile-hz",
            "997",
            "--kernel-symbols",
            config.kernel,
        ]
        if host_capable:
            args.append("--host-profile")
        result = runner(
            args,
            input_text="cat /etc/alpine-release\nreboot -f\n",
            timeout=config.timeout,
        )
        if BOOT_MARKER not in result.text:
            raise ScriptError(
                f"guest did not reach userspace ({BOOT_MARKER})\n{result.text}"
            )

        samples = _folded_sample_count(folded)
        print(f"PASS: guest profile has {samples} sample(s)")
        if host_capable:
            host_trace = work_root / "smoke.host.perf.data"
            if not host_trace.is_file():
                raise ScriptError(f"host trace was not published: {host_trace}")
            if host_trace.read_bytes()[:8] != b"PERFILE2":
                raise ScriptError(
                    "published host trace is not a finalized perf.data "
                    "(missing PERFILE2 header)"
                )
            print("PASS: host trace published with valid PERFILE2 header")
    print("PASS: profiling smoke test")