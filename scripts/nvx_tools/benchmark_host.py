"""Compare benchmark hosts with the expected host of each performance series.

Each CI performance series is measured on one runner (doc/benchmarks.md, "CI
collection"), and ``data/<series>-host.json`` records the host that its
history comes from: the fields of the host provenance that decide what the
benchmarks measure. A platform job compares its runner with that file before
it measures, ``performance persist`` compares the host that each result
records, and the regression gate requires a pull request that changes a
required field to reset the series' history.

Required fields identify the CPU, the processors and memory, the Azure VM size,
the operating system build, the build of the Microsoft hypervisor of a
Linux/MSHV host, and the storage behind the benchmark's volumes; a difference
means that the series would continue on another host. Advisory fields change
with maintenance that keeps what the series measures, such as a replaced VM, a
microcode update, a Linux kernel patch release, or a Windows cumulative update;
a difference only warns.
"""

from __future__ import annotations

import json
import operator
import re
import tempfile
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import cast

from . import host_telemetry
from .build_constants import BuildConstants

HOST_FILE_SUFFIX = "-host.json"
VOLUME_ROLES = ("workspace", "temporary", "scratch")
MEMORY_TOLERANCE = 0.05
"""The relative memory difference that still matches: VMs of one size report
slightly different memory, and kernels reserve different amounts of it."""
MATCH = "match"
MISMATCH = "mismatch"
DRIFT = "drift"
_GIB = 1024**3
_OS_DISK_FIELDS: Mapping[str, type] = {
    "storage_account_type": str,
    "size_gib": int,
    "caching": str,
    "write_accelerator_enabled": bool,
    "ephemeral_option": str,
}
_DATA_DISK_FIELDS: Mapping[str, type] = {
    "lun": int,
    **_OS_DISK_FIELDS,
    "bytes_per_second_throttle": int,
    "ops_per_second_throttle": int,
}
_VOLUME_LABELS = {
    "workspace": "Workspace volume",
    "temporary": "Temporary-directory volume",
    "scratch": "Benchmark scratch volume",
}
_RELEASE_TOKEN = re.compile(r"[^.+~_-]+")
_HYPERVISOR_BUILD = "hypervisor_build"


class HostFileError(ValueError):
    """Raised when a benchmark host file is missing or malformed."""


@dataclass(frozen=True)
class HostField:
    """One compared property of a benchmark host and its result: ``match``,
    a required ``mismatch``, or advisory ``drift``."""

    label: str
    expected: str
    actual: str
    result: str

    def describe(self) -> str:
        return f"{self.label}: expected {self.expected}, found {self.actual}"


def host_file(directory: Path, platform: str) -> Path:
    """Return the path of ``platform``'s host file in ``directory``."""
    return directory / f"{platform}{HOST_FILE_SUFFIX}"


def records_hypervisor_build(platform: str) -> bool:
    """Return whether the hosts of ``platform``'s series record the build of
    the Microsoft hypervisor that runs them and their VMs: those of Linux/MSHV,
    whose hypervisor comes from its own package, apart from the kernel (#980).
    A Windows build and UBR already identify its hypervisor, and a KVM VM's
    hypervisor is its cloud host's."""
    return platform == "linux-mshv" or platform.startswith("linux-mshv-")


def benchmark_volumes(
    scratch: Path | None = None, temporary: Path | None = None
) -> dict[str, Path]:
    """Return the directories whose volumes a benchmark's host provenance
    records: the workspace, which is the checkout, the system temporary
    directory, and the benchmark scratch directory, which defaults to the
    temporary directory."""
    system_temporary = (temporary or Path(tempfile.gettempdir())).resolve()
    return {
        "workspace": BuildConstants.REPO_ROOT,
        "temporary": system_temporary,
        "scratch": system_temporary if scratch is None else scratch.resolve(),
    }


def host_identity(platform: str, host: Mapping[str, object]) -> dict[str, object]:
    """Return the identity of recorded host provenance, the format of a host
    file: the fields that the expected host of ``platform``'s series compares.

    Paths, device names, SCSI addresses, and volume sizes are left out: they
    differ between identical VMs. Of the Azure disks, it keeps those that can
    back the benchmark's volumes: the OS disk when a volume is the system
    volume, and the data and temporary disks when a volume is on another disk.
    A Linux/MSHV host also keeps its hypervisor build, which is unknown when
    the hypervisor couldn't be read; provenance that predates it records none.
    """
    cpu = _object(host.get("cpu"))
    volumes = _object(host.get("volumes"))
    memory = _integer(host.get("memory_bytes"))
    identity: dict[str, object] = {
        "platform": platform,
        "runner_name": host.get("runner_name"),
        "machine_name": host.get("machine_name"),
        "cpu": {**cpu, "brand": host.get("cpu_model")},
        "logical_processors": host.get("logical_processors"),
        "memory_gib": None if memory is None else round(memory / _GIB, 1),
        "os": host.get("os_version"),
        "azure": _benchmark_disks(host.get("azure"), volumes),
        "volumes": volumes,
    }
    if records_hypervisor_build(platform) and "hypervisor" in host:
        identity[_HYPERVISOR_BUILD] = _object(host["hypervisor"]).get("build")
    return _normalized_host(identity)


def read_host(path: Path, platform: str) -> dict[str, object]:
    """Read ``platform``'s host file, rejecting unknown or malformed fields. A
    Linux/MSHV host file may lack the hypervisor build if it predates it."""
    try:
        document: object = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise HostFileError(f"{path} does not exist") from error
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise HostFileError(f"cannot read {path}: {error}") from error
    host = _normalized_host(document)
    if host != document:
        field = _first_difference(document, host)
        raise HostFileError(
            f"{path} is not a benchmark host file: {field} is missing, "
            "unknown, or malformed"
        )
    if host["platform"] != platform:
        raise HostFileError(
            f"{path} describes platform {host['platform']!r}, not {platform!r}"
        )
    return host


def format_host(host: Mapping[str, object]) -> str:
    """Return a host file's text."""
    return json.dumps(host, indent=2) + "\n"


def write_host(path: Path, host: Mapping[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(format_host(host), encoding="utf-8", newline="\n")


def kernel_series(release: str) -> str:
    """Return a Linux kernel release's series: its major and minor version and
    its flavor, the tokens that contain letters. Patch releases of a series,
    such as ``6.6.157.1-1.azl3`` and ``6.6.160.1-1.azl3``, share it, while
    ``6.6.137.mshv2-2.azl3`` belongs to ``6.6 mshv2 azl3``."""
    tokens = _RELEASE_TOKEN.findall(release)
    flavor = [token for token in tokens[2:] if re.search("[A-Za-z]", token)]
    return " ".join([".".join(tokens[:2]), *flavor])


def compare_hosts(
    expected: Mapping[str, object], actual: Mapping[str, object]
) -> list[HostField]:
    """Compare every field of two host identities, in host-table order."""
    fields: list[HostField] = []

    def add(
        label: str,
        expected_value: object,
        actual_value: object,
        *,
        required: bool = True,
        describe: Callable[[object], str] = _describe,
        same: Callable[[object, object], bool] = operator.eq,
    ) -> None:
        if same(expected_value, actual_value):
            result = MATCH
        else:
            result = MISMATCH if required else DRIFT
        fields.append(
            HostField(label, describe(expected_value), describe(actual_value), result)
        )

    add(
        "Runner", expected.get("runner_name"), actual.get("runner_name"), required=False
    )
    add(
        "Machine",
        expected.get("machine_name"),
        actual.get("machine_name"),
        required=False,
    )
    expected_cpu = _object(expected.get("cpu"))
    actual_cpu = _object(actual.get("cpu"))
    add(
        "CPU signature",
        _cpu_signature(expected_cpu),
        _cpu_signature(actual_cpu),
    )
    add("CPU model", expected_cpu.get("brand"), actual_cpu.get("brand"))
    add(
        "CPU microcode",
        expected_cpu.get("microcode"),
        actual_cpu.get("microcode"),
        required=False,
    )
    add(
        "Logical processors",
        expected.get("logical_processors"),
        actual.get("logical_processors"),
    )
    add(
        "Memory",
        expected.get("memory_gib"),
        actual.get("memory_gib"),
        describe=lambda value: "unknown" if value is None else f"{value} GiB",
        same=_same_memory,
    )
    fields.extend(_compare_os(_object(expected.get("os")), _object(actual.get("os"))))
    if _HYPERVISOR_BUILD in expected or _HYPERVISOR_BUILD in actual:
        fields.append(_compare_hypervisor_build(expected, actual))
    fields.extend(_compare_azure(expected.get("azure"), actual.get("azure")))
    expected_volumes = _object(expected.get("volumes"))
    actual_volumes = _object(actual.get("volumes"))
    for role in VOLUME_ROLES:
        if role in expected_volumes or role in actual_volumes:
            add(
                _VOLUME_LABELS[role],
                expected_volumes.get(role),
                actual_volumes.get(role),
                describe=_describe_volume,
            )
    return fields


def mismatches(fields: Sequence[HostField]) -> list[HostField]:
    """Return the required fields that differ."""
    return [field for field in fields if field.result == MISMATCH]


def drift(fields: Sequence[HostField]) -> list[HostField]:
    """Return the advisory fields that differ."""
    return [field for field in fields if field.result == DRIFT]


def host_check_markdown(
    title: str,
    expected_path: Path,
    fields: Sequence[HostField],
    actual: Mapping[str, object],
) -> str:
    """Return the step summary of a host check, with the actual host to accept
    when it differs."""
    changed, drifted = mismatches(fields), drift(fields)
    if changed:
        status = f"**mismatch** ({len(changed)} required field(s) differ)"
    elif drifted:
        status = f"match with drift ({len(drifted)} advisory field(s) differ)"
    else:
        status = "match"
    lines = [
        f"## Benchmark host check: {title}",
        "",
        f"This runner compared with `{expected_path.as_posix()}`: {status}.",
        "",
        "| Property | Expected | Actual | Result |",
        "| --- | --- | --- | --- |",
    ]
    lines.extend(
        f"| {_cell(field.label)} | {_cell(field.expected)} | "
        f"{_cell(field.actual)} | "
        f"{'**mismatch**' if field.result == MISMATCH else field.result} |"
        for field in fields
    )
    lines.append("")
    if changed:
        lines.extend(
            (
                "This series' history comes from the expected host. To measure "
                f"it on this host, accept the host: replace `{expected_path.as_posix()}` "
                "with the actual host below and reset the series' history in the "
                "same pull request (doc/benchmarks.md, "
                '"Expected benchmark hosts").',
                "",
            )
        )
    elif drifted:
        lines.extend(
            (
                "Advisory fields don't change what the series measures. To "
                f"acknowledge them, replace `{expected_path.as_posix()}` with "
                "the actual host below; that needs no history reset.",
                "",
            )
        )
    lines.extend(actual_host_markdown(actual))
    return "\n".join(lines) + "\n"


def actual_host_markdown(actual: Mapping[str, object]) -> list[str]:
    """Return a collapsed block with the actual host file's text."""
    return [
        "<details><summary>Actual host</summary>",
        "",
        "```json",
        format_host(actual).rstrip("\n"),
        "```",
        "",
        "</details>",
        "",
    ]


def _compare_os(
    expected: Mapping[str, object], actual: Mapping[str, object]
) -> list[HostField]:
    system = expected.get("system")
    fields = [
        HostField(
            "Operating system",
            _describe(system),
            _describe(actual.get("system")),
            MATCH if system == actual.get("system") else MISMATCH,
        )
    ]
    release = _text(expected.get("release"))
    actual_release = _text(actual.get("release"))
    if system == actual.get("system") == "Linux":
        # Patch releases of a kernel series only warn; another major or minor
        # version or flavor, such as an MSHV build, is another host (#415).
        if release == actual_release:
            result = MATCH
        elif (
            release is not None
            and actual_release is not None
            and kernel_series(release) == kernel_series(actual_release)
        ):
            result = DRIFT
        else:
            result = MISMATCH
        fields.append(
            HostField(
                "Linux kernel", _describe(release), _describe(actual_release), result
            )
        )
        return fields
    revision = expected.get("revision")
    actual_revision = actual.get("revision")
    if release != actual_release:
        result = MISMATCH
    elif revision != actual_revision and system == actual.get("system") == "Windows":
        # A cumulative update only warns; another build is another host.
        result = DRIFT
    elif revision != actual_revision:
        result = MISMATCH
    else:
        result = MATCH
    label = "Windows build" if system == "Windows" else "OS release"
    fields.append(
        HostField(
            label,
            _describe_build(release, revision),
            _describe_build(actual_release, actual_revision),
            result,
        )
    )
    return fields


def _compare_hypervisor_build(
    expected: Mapping[str, object], actual: Mapping[str, object]
) -> HostField:
    """Compare the whole version of the hypervisor, service included: both
    10.0.26100.9444 and 10.0.26100.30000 are build 26100. An unknown build
    matches none, not even another unknown one, so a host whose hypervisor
    couldn't be read never passes. A host file that predates the field,
    which records none, only warns, so that adding the field to an expected
    host needs no history reset (#980)."""

    def describe(host: Mapping[str, object]) -> str:
        if _HYPERVISOR_BUILD not in host:
            return "not recorded"
        return _describe(host[_HYPERVISOR_BUILD])

    if _HYPERVISOR_BUILD not in expected or _HYPERVISOR_BUILD not in actual:
        result = DRIFT
    elif (
        expected[_HYPERVISOR_BUILD] is not None
        and expected[_HYPERVISOR_BUILD] == actual[_HYPERVISOR_BUILD]
    ):
        result = MATCH
    else:
        result = MISMATCH
    return HostField("Hypervisor build", describe(expected), describe(actual), result)


def _compare_azure(expected: object, actual: object) -> list[HostField]:
    """Compare the Azure VM sizes and the disks behind the benchmark's volumes;
    ``None`` is a host that isn't an Azure VM."""
    if expected is None or actual is None:
        same = expected is actual
        return [
            HostField(
                "Azure VM size",
                _describe_vm_size(expected),
                _describe_vm_size(actual),
                MATCH if same else MISMATCH,
            )
        ]
    expected_azure, actual_azure = _object(expected), _object(actual)
    fields = [
        HostField(
            "Azure VM size",
            _describe_vm_size(expected_azure),
            _describe_vm_size(actual_azure),
            MATCH
            if expected_azure.get("vm_size") == actual_azure.get("vm_size")
            else MISMATCH,
        )
    ]
    for key, label, describe in (
        ("os_disk", "Azure OS disk", _describe_disk),
        ("data_disks", "Azure data disks", _describe_data_disks),
        ("resource_disk_size_kib", "Azure temporary disk", _describe_resource_disk),
    ):
        if key not in expected_azure and key not in actual_azure:
            continue
        expected_value, actual_value = (
            describe(azure[key]) if key in azure else "not behind a benchmark volume"
            for azure in (expected_azure, actual_azure)
        )
        same = (
            key in expected_azure
            and key in actual_azure
            and expected_azure[key] == actual_azure[key]
        )
        fields.append(
            HostField(label, expected_value, actual_value, MATCH if same else MISMATCH)
        )
    return fields


def _same_memory(expected: object, actual: object) -> bool:
    if expected is None or actual is None:
        return expected is actual
    first, second = cast(float, expected), cast(float, actual)
    return abs(second - first) <= MEMORY_TOLERANCE * first


def _cpu_signature(cpu: Mapping[str, object]) -> object:
    values = [cpu.get(name) for name in ("vendor", "family", "model", "stepping")]
    return None if None in values else tuple(values)


def _describe(value: object) -> str:
    if value is None:
        return "unknown"
    if isinstance(value, tuple):
        vendor, *numbers = cast(tuple[object, ...], value)
        return f"{vendor} {'/'.join(str(number) for number in numbers)}"
    return str(value)


def _describe_vm_size(azure: object) -> str:
    if azure is None:
        return "not an Azure VM"
    return _describe(_object(azure).get("vm_size"))


def _describe_build(release: str | None, revision: object) -> str:
    if release is None:
        return "unknown"
    return release if revision is None else f"{release}.{revision}"


def _describe_disk(disk: object) -> str:
    if disk is None:
        return "unknown"
    return host_telemetry.describe_azure_disk(_object(disk))


def _describe_data_disks(disks: object) -> str:
    if not isinstance(disks, list):
        return "unknown"
    items = [_object(disk) for disk in cast(list[object], disks)]
    if not items:
        return "none"
    return "; ".join(
        f"LUN {_describe(disk.get('lun'))}: {_describe_disk(disk)}" for disk in items
    )


def _describe_resource_disk(size_kib: object) -> str:
    if size_kib is None:
        return "unknown"
    return f"{cast(int, size_kib) / (1024 * 1024):.1f} GiB" if size_kib else "none"


def _describe_volume(value: object) -> str:
    if value is None:
        return "not recorded"
    volume = _object(value)
    name = _text(volume.get("volume")) or "unknown volume"
    filesystem = _text(volume.get("filesystem"))
    text = name if filesystem is None else f"{name} {filesystem}"
    if volume.get("system_volume") is True:
        text += ", system volume"
    return text


def _cell(text: str) -> str:
    return text.replace("|", "\\|").replace("\n", " ")


def _benchmark_disks(azure: object, volumes: Mapping[str, object]) -> object:
    """Keep the VM size and the Azure disks that can back a benchmark volume.
    A disk that a volume needs but the metadata lacks, such as after an
    instance metadata error, stays as unknown."""
    if not isinstance(azure, dict):
        return azure
    metadata = cast(dict[str, object], azure)
    disks: dict[str, object] = {"vm_size": metadata.get("vm_size")}
    roles = [_object(volumes.get(role)) for role in VOLUME_ROLES]
    if any(role.get("system_volume") is True for role in roles):
        disks["os_disk"] = metadata.get("os_disk")
    if any(
        role.get("system_volume") is False
        and (role.get("disk") is not None or role.get("disk_number") is not None)
        for role in roles
    ):
        disks["data_disks"] = metadata.get("data_disks")
        disks["resource_disk_size_kib"] = metadata.get("resource_disk_size_kib")
    return disks


def _normalized_host(document: object) -> dict[str, object]:
    host = _object(document)
    cpu = _object(host.get("cpu"))
    system = _object(host.get("os"))
    volumes = _object(host.get("volumes"))
    azure = host.get("azure")
    platform = _text(host.get("platform"))
    normalized: dict[str, object] = {
        "platform": platform,
        "runner_name": _text(host.get("runner_name")),
        "machine_name": _text(host.get("machine_name")),
        "cpu": {
            "vendor": _text(cpu.get("vendor")),
            "family": _integer(cpu.get("family")),
            "model": _integer(cpu.get("model")),
            "stepping": _integer(cpu.get("stepping")),
            "brand": _text(cpu.get("brand")),
            "microcode": _text(cpu.get("microcode")),
        },
        "logical_processors": _integer(host.get("logical_processors")),
        "memory_gib": _number(host.get("memory_gib")),
        "os": {
            "system": _text(system.get("system")),
            "release": _text(system.get("release")),
            "revision": _integer(system.get("revision")),
        },
    }
    if _HYPERVISOR_BUILD in host and records_hypervisor_build(platform or ""):
        normalized[_HYPERVISOR_BUILD] = _text(host[_HYPERVISOR_BUILD])
    normalized["azure"] = None if azure is None else _normalized_azure(_object(azure))
    normalized["volumes"] = {
        role: _normalized_volume(volumes[role])
        for role in VOLUME_ROLES
        if role in volumes
    }
    return normalized


def _normalized_volume(value: object) -> dict[str, object]:
    volume = _object(value)
    return {
        "volume": _text(volume.get("volume")),
        "filesystem": _text(volume.get("filesystem")),
        "system_volume": _boolean(volume.get("system_volume")),
    }


def _normalized_azure(azure: Mapping[str, object]) -> dict[str, object]:
    result: dict[str, object] = {"vm_size": _text(azure.get("vm_size"))}
    if "os_disk" in azure:
        result["os_disk"] = _normalized_disk(azure["os_disk"], _OS_DISK_FIELDS)
    if "data_disks" in azure:
        disks = azure["data_disks"]
        result["data_disks"] = (
            sorted(
                (
                    _normalized_disk(disk, _DATA_DISK_FIELDS)
                    for disk in cast(list[object], disks)
                ),
                key=lambda disk: _integer(_object(disk).get("lun")) or 0,
            )
            if isinstance(disks, list)
            else None
        )
    if "resource_disk_size_kib" in azure:
        result["resource_disk_size_kib"] = _integer(azure["resource_disk_size_kib"])
    return result


def _normalized_disk(
    value: object, fields: Mapping[str, type]
) -> dict[str, object] | None:
    if not isinstance(value, dict):
        return None
    disk = cast(dict[str, object], value)
    converters: dict[type, Callable[[object], object]] = {
        str: _text,
        int: _integer,
        bool: _boolean,
    }
    return {name: converters[kind](disk.get(name)) for name, kind in fields.items()}


def _first_difference(document: object, normalized: object, path: str = "") -> str:
    if isinstance(document, dict) and isinstance(normalized, dict):
        first = cast(dict[str, object], document)
        second = cast(dict[str, object], normalized)
        for key in [*first, *second]:
            name = f"{path}.{key}" if path else key
            if key not in first or key not in second:
                return name
            if first[key] != second[key]:
                return _first_difference(first[key], second[key], name)
    if isinstance(document, list) and isinstance(normalized, list):
        first_items = cast(list[object], document)
        second_items = cast(list[object], normalized)
        for index, (item, normalized_item) in enumerate(
            zip(first_items, second_items, strict=False)
        ):
            if item != normalized_item:
                return _first_difference(item, normalized_item, f"{path}[{index}]")
    return path or "the document"


def _object(value: object) -> dict[str, object]:
    return cast(dict[str, object], value) if isinstance(value, dict) else {}


def _text(value: object) -> str | None:
    return value if isinstance(value, str) and value else None


def _integer(value: object) -> int | None:
    return value if isinstance(value, int) and not isinstance(value, bool) else None


def _number(value: object) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


def _boolean(value: object) -> bool | None:
    return value if isinstance(value, bool) else None
