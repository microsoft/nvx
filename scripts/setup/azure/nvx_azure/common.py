"""Helpers that the Linux and Windows bootstrap commands share."""

import json
import re
import shutil
import subprocess
import sys
import time
import urllib.request
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
from io import BufferedReader
from pathlib import Path
from typing import BinaryIO, Protocol, cast

IMDS_ENDPOINT = "http://169.254.169.254"
COMPUTE_METADATA_URL = (
    IMDS_ENDPOINT + "/metadata/instance/compute?api-version=2021-02-01"
)
VAULT_TOKEN_URL = (
    IMDS_ENDPOINT
    + "/metadata/identity/oauth2/token?api-version=2019-08-01"
    + "&resource=https%3A%2F%2Fvault.azure.net"
)
HTTP_TIMEOUT_SECONDS = 30
ROLE_LABELS = ("general", "benchmark")
# Bounded buffer of the full-device zero scans: they never hold more than
# one chunk of a disk in memory.
ZERO_SCAN_CHUNK_BYTES = 1024 * 1024

JsonObject = dict[str, object]


class BootstrapError(Exception):
    """A bootstrap step failed. Messages never contain credentials."""


class Readable(Protocol):
    def read(self, size: int, /) -> bytes: ...


class Writable(Protocol):
    def write(self, data: bytes, /) -> object: ...

    def flush(self) -> object: ...


@dataclass(frozen=True)
class Completed:
    """The exit status and captured output of a command."""

    returncode: int
    stdout: bytes = b""
    stderr: bytes = b""

    def text(self) -> str:
        return self.stdout.decode("utf-8", "replace")


def json_object(value: object, what: str) -> JsonObject:
    """Return a decoded JSON object, or fail when the value is not one."""
    if not isinstance(value, dict):
        raise BootstrapError(f"{what} is not a JSON object")
    items = cast(dict[object, object], value)
    if any(not isinstance(key, str) for key in items):
        raise BootstrapError(f"{what} has a key that is not a string")
    return cast(JsonObject, items)


def load_json(path: Path, what: str) -> JsonObject:
    with path.open(encoding="utf-8-sig") as stream:
        return json_object(json.load(stream), what)


def text(source: JsonObject, key: str, what: str = "configuration") -> str:
    """Return a nonempty single-line string field."""
    value = source.get(key)
    if not isinstance(value, str) or not value or "\n" in value or "\r" in value:
        raise BootstrapError(f"The {what} has no valid {key}")
    return value


def integer(source: JsonObject, key: str, what: str = "configuration") -> int:
    value = source.get(key)
    if not isinstance(value, int) or isinstance(value, bool):
        raise BootstrapError(f"The {what} has no valid {key}")
    return value


def strings(source: JsonObject, key: str, what: str = "configuration") -> list[str]:
    value = source.get(key)
    if not isinstance(value, list):
        raise BootstrapError(f"The {what} has no valid {key}")
    items = cast(list[object], value)
    if any(not isinstance(item, str) for item in items):
        raise BootstrapError(f"The {what} has no valid {key}")
    return cast(list[str], items)


class Log:
    """Write timestamped lines to standard output and an optional log file."""

    def __init__(self, path: Path | None = None) -> None:
        self.path = path

    def __call__(self, message: str) -> None:
        stamp = datetime.now(timezone.utc).isoformat(timespec="seconds")
        lines = [f"[{stamp}] {line}" for line in message.splitlines() or [""]]
        for line in lines:
            print(line, flush=True)
        if self.path is not None:
            with self.path.open("a", encoding="utf-8") as stream:
                stream.writelines(line + "\n" for line in lines)


def marker(name: str) -> None:
    """Report a provisioning state on a line of its own."""
    print(f"{name}=1", flush=True)


def which(command: str) -> str | None:
    return shutil.which(command)


def sleep(seconds: float) -> None:
    time.sleep(seconds)


def run_command(
    args: Sequence[str],
    *,
    stdin: bytes | None = None,
    check: bool = True,
    capture: bool = True,
    output: BinaryIO | None = None,
    outputs: Sequence[Writable] | None = None,
    env: Mapping[str, str] | None = None,
    cwd: Path | None = None,
) -> Completed:
    """Run a command from an argument list, never through a shell.

    Without ``stdin`` the command reads end of file, so a prompt cannot
    stall provisioning. Uncaptured output goes to this process's streams, or
    both child streams are merged into ``output`` when it is provided.
    """
    if capture and (output is not None or outputs is not None):
        raise ValueError("a captured command cannot stream output")
    if output is not None and outputs is not None:
        raise ValueError("a command cannot use two output streaming modes")
    if not capture:
        sys.stdout.flush()
        sys.stderr.flush()
    try:
        if outputs is not None:
            process = subprocess.Popen(
                list(args),
                stdin=subprocess.PIPE if stdin is not None else subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                env=None if env is None else dict(env),
                cwd=cwd,
            )
            if stdin is not None:
                assert process.stdin is not None
                try:
                    process.stdin.write(stdin)
                except BrokenPipeError:
                    pass
                finally:
                    process.stdin.close()
            raw_stdout = process.stdout
            assert raw_stdout is not None
            stdout = cast(BufferedReader, raw_stdout)
            for chunk in iter(lambda: stdout.read1(64 * 1024), b""):
                for stream in outputs:
                    stream.write(chunk)
                    stream.flush()
            stdout.close()
            completed = Completed(process.wait())
        else:
            stdout = subprocess.PIPE if capture else output
            stderr = (
                subprocess.PIPE if capture else (subprocess.STDOUT if output else None)
            )
            if stdin is None:
                result = subprocess.run(
                    list(args),
                    stdin=subprocess.DEVNULL,
                    stdout=stdout,
                    stderr=stderr,
                    env=None if env is None else dict(env),
                    cwd=cwd,
                    check=False,
                )
            else:
                result = subprocess.run(
                    list(args),
                    input=stdin,
                    stdout=stdout,
                    stderr=stderr,
                    env=None if env is None else dict(env),
                    cwd=cwd,
                    check=False,
                )
            completed = Completed(
                result.returncode, result.stdout or b"", result.stderr or b""
            )
    except FileNotFoundError as missing:
        raise BootstrapError(f"required command not found: {args[0]}") from missing
    if check and completed.returncode != 0:
        detail = completed.stderr.decode("utf-8", "replace").strip()[-500:]
        raise BootstrapError(
            f"{Path(args[0]).name} failed with exit code {completed.returncode}"
            + (f": {detail}" if detail else "")
        )
    return completed


def zero_filled(stream: Readable, length: int) -> bool:
    """Return whether the first ``length`` bytes of a stream are all zero.

    The stream is read in bounded chunks and compared in C, so a whole disk
    can be verified without holding it in memory or interpreting each byte.
    A stream that ends early fails rather than passing as blank.
    """
    if length <= 0:
        raise BootstrapError("A blankness scan needs a positive device size")
    zeros = bytes(ZERO_SCAN_CHUNK_BYTES)
    remaining = length
    while remaining:
        data = stream.read(min(ZERO_SCAN_CHUNK_BYTES, remaining))
        if not data:
            raise BootstrapError("The disk ended before its reported size")
        if data != (zeros if len(data) == len(zeros) else zeros[: len(data)]):
            return False
        remaining -= len(data)
    return True


def request_json(
    url: str, headers: Mapping[str, str], *, method: str = "GET"
) -> JsonObject:
    """Request a JSON object. Instance metadata bypasses any proxy."""
    handlers = (
        [urllib.request.ProxyHandler({})] if url.startswith(IMDS_ENDPOINT + "/") else []
    )
    opener = urllib.request.build_opener(*handlers)
    request = urllib.request.Request(url, headers=dict(headers), method=method)
    with opener.open(request, timeout=HTTP_TIMEOUT_SECONDS) as response:
        return json_object(json.load(response), "The response")


def compute_metadata() -> JsonObject:
    return request_json(COMPUTE_METADATA_URL, {"Metadata": "true"})


def registration_token(config: JsonObject) -> str:
    """Exchange the instance identity for a runner registration token.

    The managed identity reads the GitHub credential from Key Vault, which
    requests a short-lived registration token. Callers pass the result to the
    runner setup on standard input only.
    """
    identity = request_json(VAULT_TOKEN_URL, {"Metadata": "true"})
    access_token = identity.get("access_token")
    if not isinstance(access_token, str) or not access_token:
        raise BootstrapError("Managed identity returned no Key Vault access token")
    if "\n" in access_token or "\r" in access_token:
        raise BootstrapError(
            "Managed identity returned an invalid Key Vault access token"
        )
    secret = request_json(
        f"https://{text(config, 'keyVaultName')}.vault.azure.net/secrets/"
        f"{text(config, 'bootstrapSecretName')}?api-version=7.4",
        {"Authorization": "Bearer " + access_token},
    )
    del access_token
    github_token = secret.get("value")
    if not isinstance(github_token, str) or not github_token:
        raise BootstrapError("Key Vault returned no runner bootstrap credential")
    if "\n" in github_token or "\r" in github_token:
        raise BootstrapError(
            "Key Vault returned an invalid runner bootstrap credential"
        )
    registration = request_json(
        f"https://api.github.com/repos/{text(config, 'repository')}"
        "/actions/runners/registration-token",
        {
            "Authorization": "Bearer " + github_token,
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        },
        method="POST",
    )
    del github_token
    token = registration.get("token")
    if not isinstance(token, str) or not token or "\n" in token or "\r" in token:
        raise BootstrapError("GitHub returned an invalid runner registration token")
    return token


def runner_name(config: JsonObject, metadata: JsonObject) -> str:
    """Derive the runner name from the verified scale-set instance identity."""
    vmss = text(config, "vmssName")
    prefix = vmss + "_"
    name = metadata.get("name")
    identity = (
        (metadata.get("subscriptionId"), text(config, "subscriptionId")),
        (metadata.get("resourceGroupName"), text(config, "resourceGroup")),
        (metadata.get("vmScaleSetName"), vmss),
    )
    if (
        not isinstance(name, str)
        or any(
            not isinstance(actual, str) or actual.lower() != expected.lower()
            for actual, expected in identity
        )
        or not name.lower().startswith(prefix.lower())
    ):
        raise BootstrapError(
            "Azure instance metadata does not identify the configured VMSS"
        )
    instance = name[len(prefix) :]
    if not re.fullmatch(r"[0-9]+", instance):
        raise BootstrapError("Azure instance metadata has no numeric VMSS instance ID")
    offset = integer(config, "runnerNumberOffset")
    return f"{text(config, 'runnerNamePrefix')}-{int(instance) + offset}"


def instance_labels(config: JsonObject, name: str) -> list[str]:
    """Return this instance's labels: one named runner swaps general for benchmark."""
    labels = strings(config, "runnerLabels")
    if config.get("benchmarkRunnerName") == name:
        labels = [label for label in labels if label.lower() != "general"]
        labels.append("benchmark")
    return labels


def split_labels(labels: Sequence[str], required: Sequence[str]) -> tuple[str, bool]:
    """Return the comma-separated extra labels and whether this is a benchmark runner.

    The generic setup adds the required platform labels and exactly one role
    itself, so only the remaining labels pass through as extra labels.
    """
    if [label.lower() for label in labels[: len(required)]] != list(required):
        raise BootstrapError(
            "Runner labels must preserve the platform and backend labels"
        )
    additional = labels[len(required) :]
    roles = {label.lower() for label in additional if label.lower() in ROLE_LABELS}
    if len(roles) > 1:
        raise BootstrapError(
            "Runner labels must not contain both general and benchmark"
        )
    extra = [label for label in additional if label.lower() not in ROLE_LABELS]
    if any(not re.fullmatch(r"[A-Za-z0-9_.-]+", label) for label in extra):
        raise BootstrapError("Runner labels contain an invalid label")
    return ",".join(extra), "benchmark" in roles


def _metadata_field(source: object, name: str, pattern: str) -> str:
    value = cast(JsonObject, source).get(name, "") if isinstance(source, dict) else None
    if not isinstance(value, str) or not re.fullmatch(pattern, value):
        raise BootstrapError(f"Azure compute metadata contains an invalid {name}")
    return value


def _safe_disk(disk: object, data: bool) -> JsonObject:
    if not isinstance(disk, dict):
        raise BootstrapError("Azure compute metadata has no disk profile")
    values = cast(JsonObject, disk)
    safe: JsonObject = {
        "diskSizeGB": _metadata_field(values, "diskSizeGB", r"[0-9]{1,10}"),
        "managedDisk": {
            "storageAccountType": _metadata_field(
                values.get("managedDisk"), "storageAccountType", r"[A-Za-z0-9_]{1,64}"
            )
        },
        "caching": _metadata_field(values, "caching", r"None|ReadOnly|ReadWrite"),
        "writeAcceleratorEnabled": _metadata_field(
            values, "writeAcceleratorEnabled", r"(?i:true|false)"
        ),
    }
    if data:
        lun = _metadata_field(values, "lun", r"[0-9]{1,2}")
        if int(lun) > 63:
            raise BootstrapError("Azure compute metadata contains an invalid lun")
        safe["lun"] = lun
        for name in ("bytesPerSecondThrottle", "opsPerSecondThrottle"):
            safe[name] = _metadata_field(values, name, r"[0-9]{0,20}")
    else:
        safe["diffDiskSettings"] = {
            "option": _metadata_field(
                values.get("diffDiskSettings", {}), "option", r"[A-Za-z]{0,32}"
            )
        }
    return safe


def safe_compute_metadata(metadata: JsonObject) -> JsonObject:
    """Project instance metadata onto the VM size and disk performance settings.

    Benchmark host checks read this projection instead of instance metadata,
    which jobs cannot reach. It holds no identity, tag, or user data.
    """
    profile = metadata.get("storageProfile")
    if not isinstance(profile, dict):
        raise BootstrapError("Azure compute metadata has no disk profile")
    disks = cast(JsonObject, profile).get("dataDisks")
    if not isinstance(disks, list) or not disks:
        raise BootstrapError("Azure compute metadata has no managed data disks")
    storage = cast(JsonObject, profile)
    return {
        "vmSize": _metadata_field(metadata, "vmSize", r"Standard_[A-Za-z0-9_-]{1,80}"),
        "storageProfile": {
            "osDisk": _safe_disk(storage.get("osDisk"), data=False),
            "dataDisks": [
                _safe_disk(disk, data=True) for disk in cast(list[object], disks)
            ],
            "resourceDisk": {
                "size": _metadata_field(
                    storage.get("resourceDisk", {}), "size", r"[0-9]{0,16}"
                )
            },
        },
    }


def snapshot_bytes(safe: JsonObject) -> bytes:
    return (json.dumps(safe, separators=(",", ":")) + "\n").encode()


def fail(message: str) -> int:
    """Report a failed command on standard error and return its exit status."""
    sys.stdout.flush()
    print(f"ERROR: {message}", file=sys.stderr, flush=True)
    return 1
