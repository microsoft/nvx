"""Public CLI acceptance for managed workload environment, CWD and timeouts."""

from __future__ import annotations

import json
import shutil
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any, cast

from . import nvx_rs
from .build_constants import BuildConstants, UbuntuBuildConstants
from .common import ScriptError, artifact_path, require_file

DIAGNOSTIC_LIMIT = 4096
DEFAULT_PATH = "/usr/sbin:/usr/bin:/sbin:/bin"
DEFAULT_TERM = "linux"
FORBIDDEN_DEFAULT_ENVIRONMENT_NAMES = {
    "EMPTY",
    "COMPLEX",
    "SECOND",
    "ORDER",
    "LAYERED",
}


@dataclass(frozen=True)
class WorkloadArtifacts:
    """The Ubuntu workload layer and scratch template of the public scenarios."""

    distro: Path
    layer_uuid: str
    scratch_template: Path


def workload_artifact_paths() -> tuple[tuple[Path, str], ...]:
    """Return the paths of the Ubuntu workload layer, its manifest, and the
    sandbox scratch template, which the public scenarios read, each with its
    description."""
    distro = artifact_path(UbuntuBuildConstants.DISTRO_NAME)
    return (
        (distro, "Ubuntu workload layer"),
        (
            distro.with_name(UbuntuBuildConstants.DISTRO_MANIFEST_NAME),
            "Ubuntu layer manifest",
        ),
        (
            artifact_path(UbuntuBuildConstants.SCRATCH_TEMPLATE_NAME),
            "sandbox scratch template",
        ),
    )


def load_workload_artifacts() -> WorkloadArtifacts:
    distro, manifest_path, scratch_template = (
        require_file(path, description)
        for path, description in workload_artifact_paths()
    )
    try:
        manifest: object = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ScriptError(f"invalid Ubuntu layer manifest: {manifest_path}") from error
    if not isinstance(manifest, dict):
        raise ScriptError("Ubuntu layer manifest must contain a UUID string")
    layer_uuid = cast(dict[str, object], manifest).get("uuid")
    if not isinstance(layer_uuid, str):
        raise ScriptError("Ubuntu layer manifest must contain a UUID string")
    return WorkloadArtifacts(distro, layer_uuid, scratch_template)


def bounded_text(value: bytes) -> str:
    text = value[:DIAGNOSTIC_LIMIT].decode(errors="replace")
    if len(value) > DIAGNOSTIC_LIMIT:
        text += f"... ({len(value) - DIAGNOSTIC_LIMIT} bytes omitted)"
    return text


def evidence_argv(command: list[str]) -> list[str]:
    recorded = command.copy()
    for index, value in enumerate(recorded[:-1]):
        if value == "--environment":
            recorded[index + 1] = "<redacted>"
    return recorded


def read_exec_outcome(path: Path, *, category: str, status_code: int) -> dict[str, Any]:
    try:
        value: object = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RuntimeError(f"public exec outcome is unreadable: {path}") from error
    if not isinstance(value, dict):
        raise RuntimeError("public exec outcome must be a JSON object")
    typed = cast(dict[str, object], value)
    if set(typed) != {"schema_version", "operation_id", "outcome"}:
        raise RuntimeError("public exec outcome has unexpected typed fields")
    schema_version = typed.get("schema_version")
    operation_id = typed.get("operation_id")
    outcome = typed.get("outcome")
    typed_outcome = (
        cast(dict[str, object], outcome) if isinstance(outcome, dict) else {}
    )
    outcome_status_code = typed_outcome.get("status_code")
    if (
        not isinstance(schema_version, int)
        or isinstance(schema_version, bool)
        or schema_version != 1
        or not isinstance(operation_id, str)
        or len(operation_id) != 32
        or any(character not in "0123456789abcdef" for character in operation_id)
        or not isinstance(outcome, dict)
        or set(typed_outcome) != {"operation", "category", "status_code"}
        or typed_outcome.get("operation") != "exec"
        or typed_outcome.get("category") != category
        or not isinstance(outcome_status_code, int)
        or isinstance(outcome_status_code, bool)
        or outcome_status_code != status_code
    ):
        raise RuntimeError("public exec outcome has unexpected typed fields")
    return cast(dict[str, Any], typed)


def _read_workload_identity(output: bytes) -> tuple[str, str]:
    try:
        text = output.decode("utf-8")
    except UnicodeDecodeError as error:
        raise RuntimeError(
            "public workload identity probe returned invalid UTF-8"
        ) from error
    records = [line.split(":") for line in text.splitlines() if line]
    if (
        len(records) != 1
        or len(records[0]) != 7
        or records[0][2:4] != ["65534", "65534"]
        or not records[0][0]
        or not records[0][5].startswith("/")
    ):
        raise RuntimeError("public workload identity probe returned an invalid record")
    return records[0][0], records[0][5]


def _read_environment(output: bytes) -> dict[str, str]:
    environment: dict[str, str] = {}
    for line in output.splitlines():
        name, separator, value = line.partition(b"=")
        if not separator or not name:
            raise RuntimeError("public managed environment returned an invalid entry")
        try:
            decoded_name = name.decode("utf-8")
            decoded_value = value.decode("utf-8")
        except UnicodeDecodeError as error:
            raise RuntimeError(
                "public managed environment returned invalid UTF-8"
            ) from error
        if decoded_name in environment:
            raise RuntimeError("public managed environment returned a duplicate entry")
        environment[decoded_name] = decoded_value
    return environment


def _default_environment_matches(
    environment: dict[str, str], required: dict[str, str]
) -> bool:
    return all(environment.get(name) == value for name, value in required.items()) and (
        environment.keys().isdisjoint(FORBIDDEN_DEFAULT_ENVIRONMENT_NAMES)
    )


def format_errors(errors: list[Exception]) -> str:
    text = "; ".join(f"{type(error).__name__}: {error}" for error in errors)
    encoded = text.encode("utf-8", errors="replace")
    return bounded_text(encoded)


def run_managed_exec_configuration(
    backend: str, *, timeout: float, output_dir: Path
) -> None:
    artifacts = load_workload_artifacts()
    distro = artifacts.distro
    layer_uuid = artifacts.layer_uuid
    scratch_template = artifacts.scratch_template
    output_dir.mkdir(parents=True, exist_ok=True)
    checks: list[dict[str, object]] = []
    root = Path(tempfile.mkdtemp(prefix="nvx-public-exec-"))
    preservation_reported = False
    try:
        state = root / "state"
        scratch = root / "scratch.ext4"
        shutil.copyfile(scratch_template, scratch)
        base = nvx_rs.command_line(("sandbox",))

        def invoke(
            operation: str, *arguments: str, expected: int = 0
        ) -> subprocess.CompletedProcess[bytes]:
            result = subprocess.run(
                [
                    *base,
                    operation,
                    "--state-dir",
                    str(state),
                    "--timeout",
                    str(timeout),
                    *arguments,
                ],
                cwd=BuildConstants.REPO_ROOT,
                capture_output=True,
                timeout=timeout + 10,
            )
            checks.append(
                {
                    "operation": operation,
                    "argv": evidence_argv([str(value) for value in result.args]),
                    "expected_returncode": expected,
                    "returncode": result.returncode,
                    "stdout_bytes": len(result.stdout),
                    "stderr_bytes": len(result.stderr),
                    "state_exists": state.is_dir(),
                }
            )
            if result.returncode != expected:
                raise RuntimeError(
                    f"public sandbox {operation} returned {result.returncode}, "
                    f"expected {expected}; stdout={bounded_text(result.stdout)!r}; "
                    f"stderr={bounded_text(result.stderr)!r}"
                )
            return result

        def workload(
            entrypoint: str, *arguments: str, expected: int = 0
        ) -> subprocess.CompletedProcess[bytes]:
            return invoke(
                "exec", "--entrypoint", entrypoint, *arguments, expected=expected
            )

        def expect_output(
            result: subprocess.CompletedProcess[bytes], expected: bytes
        ) -> None:
            if result.stdout != expected or result.stderr:
                raise RuntimeError(
                    "public managed workload returned unexpected output: "
                    f"stdout={bounded_text(result.stdout)!r}, "
                    f"stderr={bounded_text(result.stderr)!r}"
                )

        def expect_rejection(
            result: subprocess.CompletedProcess[bytes], code: str
        ) -> None:
            # nvx-rs refuses the request before it runs anything, with the
            # stable code of its error, whatever the wording of its message.
            reported = nvx_rs.reported_error(result.stderr)
            if result.stdout or reported is None or reported.code != code:
                raise RuntimeError(
                    "public managed validation returned unexpected diagnostics, "
                    f"expected error {code!r}: "
                    f"stdout={bounded_text(result.stdout)!r}, "
                    f"stderr={bounded_text(result.stderr)!r}"
                )

        def persist_evidence() -> None:
            errors: list[Exception] = []
            for name in ("openvmm.log", "outcome.json"):
                source = state / name
                if source.is_file():
                    try:
                        shutil.copyfile(source, output_dir / f"public-exec-{name}")
                    except Exception as error:
                        errors.append(error)
            for name in ("exit-outcome.json", "timeout-outcome.json"):
                source = root / name
                if source.is_file():
                    try:
                        shutil.copyfile(source, output_dir / f"public-exec-{name}")
                    except Exception as error:
                        errors.append(error)
            try:
                (output_dir / "public-exec-checks.json").write_text(
                    json.dumps(checks, indent=2) + "\n", encoding="utf-8"
                )
            except Exception as error:
                errors.append(error)
            if errors:
                raise RuntimeError(
                    "public managed evidence persistence failed: "
                    f"{format_errors(errors)}"
                ) from errors[0]

        invoke(
            "provision",
            "--layer",
            f"distro,{distro},{layer_uuid}",
            "--scratch",
            str(scratch),
            "--hypervisor",
            backend,
            "--memory-mib",
            "256",
        )
        started = False
        acceptance_error: Exception | None = None
        cleanup_errors: list[Exception] = []
        deprovisioned = False
        try:
            invoke("start")
            started = True

            # Supplied values must remain request-scoped across sequential public calls.
            expect_output(workload("/bin/pwd", "--cwd", "/tmp"), b"/tmp\n")
            expect_output(workload("/bin/pwd", "--cwd", "/"), b"/\n")
            expect_output(workload("/bin/pwd"), b"/\n")

            empty_file = root / "empty-env.json"
            empty_file.write_text("[]", encoding="utf-8")
            expect_output(
                workload("/usr/bin/env", "--environment-file", str(empty_file)), b""
            )
            env_file = root / "env.json"
            entries = ["EMPTY=", "COMPLEX=space = \N{SNOWMAN}"]
            env_file.write_text(json.dumps(entries), encoding="utf-8")
            expected_environment = ("\n".join(entries) + "\n").encode()
            expect_output(
                workload("/usr/bin/env", "--environment-file", str(env_file)),
                expected_environment,
            )
            expect_output(
                workload(
                    "/usr/bin/env",
                    "--environment",
                    "SECOND=inline value",
                    "--environment",
                    "ORDER=two",
                ),
                b"SECOND=inline value\nORDER=two\n",
            )
            identity = workload("/usr/bin/getent", "--arg=passwd", "--arg=65534")
            if identity.stderr:
                raise RuntimeError(
                    "public workload identity probe returned diagnostics: "
                    f"{bounded_text(identity.stderr)!r}"
                )
            workload_name, workload_home = _read_workload_identity(identity.stdout)
            expected_defaults = {
                "PATH": DEFAULT_PATH,
                "TERM": DEFAULT_TERM,
                "HOME": workload_home,
                "USER": workload_name,
                "LOGNAME": workload_name,
                "PWD": "/",
            }
            # Layered entries add variables to the defaults and replace defaults of the
            # same name; the next execution must see none of them.
            layered = workload(
                "/usr/bin/env",
                "--environment",
                "LAYERED=layered value",
                "--environment",
                "TERM=layered",
                "--inherit-default-environment",
            )
            layered_environment = _read_environment(layered.stdout)
            if (
                layered.stderr
                or layered_environment.pop("LAYERED", None) != "layered value"
                or not _default_environment_matches(
                    layered_environment, {**expected_defaults, "TERM": "layered"}
                )
            ):
                raise RuntimeError(
                    "public managed environment did not layer entries over "
                    "workload defaults"
                )
            default_environment = workload("/usr/bin/env")
            if default_environment.stderr or not _default_environment_matches(
                _read_environment(default_environment.stdout), expected_defaults
            ):
                raise RuntimeError(
                    "public managed environment did not match workload defaults"
                )
            working_environment = workload(
                "/usr/bin/env",
                "--cwd",
                "/tmp",
            )
            if working_environment.stderr or not _default_environment_matches(
                _read_environment(working_environment.stdout),
                {**expected_defaults, "PWD": "/tmp"},
            ):
                raise RuntimeError(
                    "public managed environment did not point PWD at the "
                    "working directory"
                )

            relative = workload(
                "/bin/sh",
                "--arg=-c",
                "--arg=printf workload-must-not-run",
                "--cwd",
                "relative",
                expected=1,
            )
            expect_rejection(relative, "unsupported")
            # The parser refuses a timeout outside the unsigned 32-bit range as a
            # usage error, before it runs anything.
            for invalid_timeout in (-1, 0x100000000):
                rejected = workload(
                    "/bin/sh",
                    "--arg=-c",
                    "--arg=printf workload-must-not-run",
                    "--exec-timeout-ms",
                    str(invalid_timeout),
                    expected=2,
                )
                expect_rejection(rejected, "argument-invalid")

            for cwd in ("/does-not-exist", "/etc/passwd", "/root"):
                failed = workload(
                    "/bin/sh",
                    "--arg=-c",
                    "--arg=printf workload-must-not-run",
                    "--cwd",
                    cwd,
                    expected=125,
                )
                if (
                    failed.stdout
                    or b"cannot use working directory" not in failed.stderr
                ):
                    raise RuntimeError(
                        "invalid CWD did not fail before workload execution"
                    )

            exit_outcome = root / "exit-outcome.json"
            result = workload(
                "/bin/sh",
                "--arg=-c",
                "--arg=printf 'public stdout'; printf 'public stderr' >&2; exit 7",
                "--outcome-report",
                str(exit_outcome),
                expected=7,
            )
            if result.stdout != b"public stdout" or result.stderr != b"public stderr":
                raise RuntimeError("public managed stdout/stderr were not preserved")
            read_exec_outcome(exit_outcome, category="exit", status_code=7)

            for limit in (0, 3_600_001, 86_400_000, 0xFFFFFFFF):
                expect_output(
                    workload("/bin/true", "--exec-timeout-ms", str(limit)), b""
                )
            timeout_outcome = root / "timeout-outcome.json"
            timed_out = workload(
                "/bin/sleep",
                "--arg=5",
                "--exec-timeout-ms",
                "100",
                "--outcome-report",
                str(timeout_outcome),
                expected=124,
            )
            if timed_out.stdout or timed_out.stderr:
                raise RuntimeError(
                    "public timed-out workload returned unexpected output"
                )
            read_exec_outcome(timeout_outcome, category="timeout", status_code=124)
            expect_output(workload("/bin/pwd"), b"/\n")
        except Exception as error:
            acceptance_error = error
        finally:
            try:
                if started:
                    invoke("stop")
            except Exception as error:
                cleanup_errors.append(error)
            try:
                persist_evidence()
            except Exception as error:
                cleanup_errors.append(error)
            try:
                invoke("deprovision")
                deprovisioned = True
            except Exception as error:
                cleanup_errors.append(error)
            try:
                persist_evidence()
            except Exception as error:
                cleanup_errors.append(error)
            if deprovisioned:
                try:
                    shutil.rmtree(root)
                except Exception as error:
                    cleanup_errors.append(error)
            else:
                preserved_path = bounded_text(
                    str(root).encode("utf-8", errors="replace")
                )
                preservation_reported = True
                cleanup_errors.append(
                    RuntimeError(
                        f"managed fixture preserved for recovery: {preserved_path}"
                    )
                )
        if acceptance_error is not None:
            if cleanup_errors:
                raise RuntimeError(
                    f"{acceptance_error}; cleanup failed: "
                    f"{format_errors(cleanup_errors)}"
                ) from acceptance_error
            raise acceptance_error.with_traceback(acceptance_error.__traceback__)
        if cleanup_errors:
            raise RuntimeError(
                f"public managed cleanup failed: {format_errors(cleanup_errors)}"
            ) from cleanup_errors[0]
        if not scratch_template.is_file() or not distro.is_file():
            raise RuntimeError("managed cleanup removed a supplied workload artifact")
    except Exception as error:
        if root.exists() and not preservation_reported:
            preserved_path = bounded_text(str(root).encode("utf-8", errors="replace"))
            raise RuntimeError(
                f"{error}; managed fixture preserved for recovery: {preserved_path}"
            ) from error
        raise
