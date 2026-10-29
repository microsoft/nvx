#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Save the storage events of a Windows CI job's window, and warn of slow I/O.

A disk that stops completing I/O for a while stalls whatever waits for it, such
as a benchmark guest's virtio-fs requests, and leaves a trace in the host's
event logs. Storport's Operational channel records each request that timed out
(event 500), each reset (550), each I/O that took longer than its threshold of
30 s by default (557), and periodic summaries of each device's I/O latencies
(505); the System log records the resets that storage miniports such as
storvsc issue (129). A reimage of the runner erases these logs, so a failed
benchmark job saves the events of its window in its diagnostics artifact.

Storport identifies a device by its SCSI port, path, target, and LUN, the
address that host provenance records for each benchmark volume, so the summary
names the volume of each device it knows. CI jobs run as Network Service,
which, as a service account, may read both logs.

The disk that stalled may still stall, so each step that can wait for it is
bounded and left behind when it overruns: each query, the naming of the
volumes, which probes them and so follows the export, and each write of the
output, whose directory, in the workspace, can share the stalled volume.
"""

from __future__ import annotations

import contextlib
import datetime as dt
import re
import subprocess
import threading
import xml.etree.ElementTree as ElementTree
from collections import Counter
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import TypeVar

from . import host_telemetry

STORPORT_CHANNEL = "Microsoft-Windows-Storage-Storport/Operational"
SYSTEM_CHANNEL = "System"
# The System log's storage providers: the storage miniports, the disk and
# volume drivers, and the file systems.
SYSTEM_STORAGE_PROVIDERS = (
    "storvsc",
    "stornvme",
    "storahci",
    "disk",
    "partmgr",
    "volmgr",
    "volsnap",
    "Ntfs",
    "Microsoft-Windows-Ntfs",
    "Microsoft-Windows-ReFS",
)
REQUEST_TIMEOUT_EVENT = 500
LATENCY_SUMMARY_EVENT = 505
RESET_EVENT = 550
SLOW_IO_EVENT = 557
MINIPORT_RESET_EVENT = 129
# Bounds each query, as the event log service can stall with the disks.
EVENT_QUERY_TIMEOUT_SECONDS = 120
# Bounds the naming of the devices by the volumes that they hold.
VOLUME_NAMING_TIMEOUT_SECONDS = 30
# Bounds the creation of the output directory and each write into it.
WRITE_TIMEOUT_SECONDS = 30
SUMMARY_FILENAME = "storage-events.txt"
# Latency summaries report the I/Os that took longer than a second.
SLOW_LATENCY_BUCKET_MS = 1000
_EVENT_NAMESPACE = "{http://schemas.microsoft.com/win/2004/08/events/event}"
_UTC_TIME = re.compile(
    r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(?:\.(\d+))?(Z|[+-]\d\d:\d\d)", re.ASCII
)
_LATENCY_BUCKET = re.compile(r"(\d+)(\+?)(us|ms)", re.ASCII)
_T = TypeVar("_T")


@dataclass(frozen=True)
class EventSource:
    """An event log channel, from all providers or from ``providers`` only,
    and the file of the artifact that keeps its events."""

    name: str
    channel: str
    filename: str
    providers: tuple[str, ...] = ()


EVENT_SOURCES = (
    EventSource("Storport", STORPORT_CHANNEL, "storport-operational.xml"),
    EventSource(
        "System", SYSTEM_CHANNEL, "system-storage.xml", SYSTEM_STORAGE_PROVIDERS
    ),
)


@dataclass(frozen=True)
class StorageEvent:
    """An event as Windows renders it: its source and provider, ID, level,
    UTC time, message, and named event data."""

    source: str
    provider: str
    event_id: int
    level: str
    time: str
    message: str
    data: Mapping[str, str] = field(default_factory=dict[str, str])

    @property
    def device(self) -> str | None:
        """The SCSI address of the Storport device that the event concerns."""
        names = ("PortNumber", "PathID", "TargetID", "LUN")
        if not all(name in self.data for name in names):
            return None
        return ":".join(self.data[name] for name in names)

    @property
    def first_line(self) -> str:
        """The first line of the event's message."""
        lines = self.message.strip().splitlines()
        return lines[0].strip() if lines else ""

    def summary(self) -> str:
        """Return the event's time, source, ID, level, and first message
        line."""
        provider = "" if self.source == "Storport" else f"{self.provider} "
        return (
            f"{self.time} {self.source} {provider}{self.event_id} {self.level}: "
            f"{self.first_line}"
        )


def parse_utc(text: str) -> dt.datetime:
    """Parse an ISO 8601 time with a UTC offset or Z, such as PowerShell's
    round-trip format, which has seven fractional digits."""
    match = _UTC_TIME.fullmatch(text.strip())
    if match is None:
        raise ValueError(f"not an ISO 8601 time with a zone: {text!r}")
    moment = dt.datetime.strptime(match[1], "%Y-%m-%dT%H:%M:%S")
    microseconds = int((match[2] or "")[:6].ljust(6, "0"))
    zone = dt.timezone.utc
    if match[3] != "Z":
        sign = -1 if match[3].startswith("-") else 1
        hours, minutes = (int(part) for part in match[3][1:].split(":"))
        zone = dt.timezone(sign * dt.timedelta(hours=hours, minutes=minutes))
    return moment.replace(microsecond=microseconds, tzinfo=zone).astimezone(
        dt.timezone.utc
    )


def format_utc(moment: dt.datetime) -> str:
    """Format a time as the UTC SystemTime of an event query, to the ms."""
    utc = moment.astimezone(dt.timezone.utc)
    return utc.strftime("%Y-%m-%dT%H:%M:%S.") + f"{utc.microsecond // 1000:03d}Z"


def event_query(
    since: dt.datetime, until: dt.datetime, providers: Sequence[str] = ()
) -> str:
    """Return the XPath query of a channel's events from ``since`` to
    ``until``, from ``providers`` only when there are any."""
    conditions = [
        f"TimeCreated[@SystemTime>='{format_utc(since)}' and "
        f"@SystemTime<='{format_utc(until)}']"
    ]
    if providers:
        names = " or ".join(f"@Name='{provider}'" for provider in providers)
        conditions.insert(0, f"Provider[{names}]")
    return f"*[System[{' and '.join(conditions)}]]"


def _within(timeout: float, function: Callable[[], _T], description: str) -> _T:
    """Return what ``function`` returns, or raise ``TimeoutError`` once it has
    run for ``timeout`` seconds, leaving it to a daemon thread: what waits for
    a stalled disk may wait until the disk completes its I/O, even after a
    kill."""
    results: list[_T] = []
    errors: list[BaseException] = []

    def run() -> None:
        try:
            results.append(function())
        except BaseException as error:
            errors.append(error)

    thread = threading.Thread(target=run, name=description, daemon=True)
    thread.start()
    thread.join(timeout)
    if thread.is_alive():
        raise TimeoutError(f"{description} did not finish within {timeout:g}s")
    if errors:
        raise errors[0]
    return results[0]


def read_events(
    channel: str, query: str, timeout: float = EVENT_QUERY_TIMEOUT_SECONDS
) -> bytes:
    """Return the rendered XML of the events of ``channel`` that ``query``
    selects, oldest first, as wevtutil writes it in UTF-16. A query that
    runs for ``timeout`` seconds is killed, and not waited for."""
    process = subprocess.Popen(
        [
            "wevtutil",
            "query-events",
            channel,
            f"/q:{query}",
            "/f:RenderedXml",
            "/e:Events",
            "/uni:true",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        stdout, stderr = _within(timeout, process.communicate, "wevtutil")
    except TimeoutError:
        with contextlib.suppress(OSError):
            process.kill()
        raise
    if process.returncode != 0:
        reason = _decode(stderr).strip().splitlines()
        raise OSError(
            f"wevtutil failed with status {process.returncode}"
            + (f": {reason[0].strip()}" if reason else "")
        )
    return stdout


def _decode(output: bytes) -> str:
    if output.startswith(b"\xff\xfe") or b"\x00" in output[:4]:
        return output.decode("utf-16", "replace")
    return output.decode("utf-8", "replace")


def parse_events(source: str, document: bytes | str) -> list[StorageEvent]:
    """Return the events of a rendered XML document of wevtutil."""
    text = _decode(document) if isinstance(document, bytes) else document
    events: list[StorageEvent] = []
    for element in ElementTree.fromstring(text).iter(f"{_EVENT_NAMESPACE}Event"):
        system = element.find(f"{_EVENT_NAMESPACE}System")
        if system is None:
            continue
        provider = system.find(f"{_EVENT_NAMESPACE}Provider")
        created = system.find(f"{_EVENT_NAMESPACE}TimeCreated")
        rendering = element.find(f"{_EVENT_NAMESPACE}RenderingInfo")
        level = (
            rendering.findtext(f"{_EVENT_NAMESPACE}Level")
            if rendering is not None
            else None
        ) or system.findtext(f"{_EVENT_NAMESPACE}Level", "")
        message = (
            rendering.findtext(f"{_EVENT_NAMESPACE}Message", "")
            if rendering is not None
            else ""
        )
        data = {
            name: item.text or ""
            for item in element.iter(f"{_EVENT_NAMESPACE}Data")
            if (name := item.get("Name"))
        }
        events.append(
            StorageEvent(
                source=source,
                provider="" if provider is None else provider.get("Name", ""),
                event_id=int(system.findtext(f"{_EVENT_NAMESPACE}EventID", "0")),
                level=level,
                time="" if created is None else created.get("SystemTime", ""),
                message=message,
                data=data,
            )
        )
    return events


def volume_devices(volumes: Mapping[str, Path]) -> dict[str, str]:
    """Map the SCSI address of each disk behind ``volumes`` to the volumes
    that it holds, each with its roles, such as ``workspace/scratch F:``."""
    devices: dict[str, dict[str, list[str]]] = {}
    for role, path in volumes.items():
        provenance = host_telemetry.volume_provenance(path)
        address = provenance.get("scsi_address")
        volume = provenance.get("volume")
        if not isinstance(address, str) or not isinstance(volume, str):
            continue
        devices.setdefault(address, {}).setdefault(volume, []).append(role)
    return {
        address: ", ".join(
            f"{'/'.join(roles)} {volume}" for volume, roles in held.items()
        )
        for address, held in devices.items()
    }


def slow_latency_counts(event: StorageEvent) -> list[tuple[str, int]]:
    """Return the I/Os of a latency summary in each bucket above a second,
    labeled by its bounds, such as ``2000ms-10000ms``."""
    buckets = [
        bucket.strip()
        for bucket in event.data.get("IoLatencyBuckets", "").split(",")
        if bucket.strip()
    ]
    counts: list[tuple[str, int]] = []
    previous, lower_ms = "0ms", 0.0
    for index, bucket in enumerate(buckets, 1):
        match = _LATENCY_BUCKET.fullmatch(bucket)
        if match is None:
            break
        bound_ms = int(match[1]) / (1000 if match[3] == "us" else 1)
        count = sum(
            int(event.data.get(f"BucketIo{outcome}{index}", "") or 0)
            for outcome in ("Success", "Failed")
        )
        # A bucket such as 10000+ms counts the I/Os above its bound, and the
        # others those up to their bound from the previous one.
        above = bool(match[2])
        if (bound_ms if above else lower_ms) >= SLOW_LATENCY_BUCKET_MS and count:
            counts.append((bucket if above else f"{previous}-{bucket}", count))
        previous, lower_ms = bucket, bound_ms
    return counts


@dataclass
class StorageEventReport:
    """The storage events of a window, the sources that couldn't be read
    with why, why the devices' volumes are unknown when they are, and the
    files that couldn't be saved with why."""

    since: str
    until: str
    events: list[StorageEvent]
    failures: dict[str, str]
    devices: Mapping[str, str] = field(default_factory=dict[str, str])
    volume_failure: str | None = None
    unsaved: dict[str, str] = field(default_factory=dict[str, str])

    def device(self, event: StorageEvent) -> str:
        address = event.device
        if address is None:
            return "an unknown device"
        known = self.devices.get(address)
        return f"device {address}" + (f" ({known})" if known else "")

    def slow_ios(self) -> list[StorageEvent]:
        return [
            event
            for event in self.events
            if event.source == "Storport" and event.event_id == SLOW_IO_EVENT
        ]

    def lines(self) -> list[str]:
        """Summarize the window: the events of each source by ID, why the
        devices' volumes are unknown when they are, each file that couldn't
        be saved, then each slow I/O, timeout, reset, and latency summary
        with I/Os above a second."""
        lines = [f"Storage events from {self.since} to {self.until}"]
        for source in EVENT_SOURCES:
            if source.name in self.failures:
                lines.append(
                    f"{source.name}: unavailable: {self.failures[source.name]}"
                )
                continue
            counts = Counter(
                (event.provider if source.providers else "", event.event_id)
                for event in self.events
                if event.source == source.name
            )
            detail = ", ".join(
                f"{count} of {provider + ' ' if provider else ''}{event_id}"
                for (provider, event_id), count in sorted(counts.items())
            )
            total = sum(counts.values())
            lines.append(
                f"{source.name}: {total} event{'s' if total != 1 else ''}"
                + (f": {detail}" if detail else "")
            )
        if self.volume_failure is not None:
            lines.append(f"Volumes: unavailable: {self.volume_failure}")
        for filename, reason in self.unsaved.items():
            lines.append(f"Not saved: {filename}: {reason}")
        for event in self.events:
            line = self._notable(event)
            if line is not None:
                lines.append(line)
        return lines

    def _notable(self, event: StorageEvent) -> str | None:
        if event.source == "System":
            if event.event_id == MINIPORT_RESET_EVENT:
                return f"{event.time} {event.provider} reset: {event.first_line}"
            return None
        if event.event_id == SLOW_IO_EVENT:
            return (
                f"{event.time} slow I/O on {self.device(event)}: took "
                f"{event.data.get('RequestDuration_ms', '?')} ms, waited "
                f"{event.data.get('WaitDuration_ms', '?')} ms, threshold "
                f"{event.data.get('Threshold_ms', '?')} ms"
            )
        if event.event_id == REQUEST_TIMEOUT_EVENT:
            return (
                f"{event.time} request timeout on {self.device(event)} after "
                f"{event.data.get('SrbTimeout', '?')} s"
            )
        if event.event_id == RESET_EVENT:
            return f"{event.time} reset of {self.device(event)}"
        if event.event_id == LATENCY_SUMMARY_EVENT:
            counts = slow_latency_counts(event)
            if counts:
                detail = ", ".join(f"{count} in {label}" for label, count in counts)
                return f"{event.time} latency summary of {self.device(event)}: {detail}"
        return None

    def warnings(self) -> list[tuple[str, str]]:
        """Return the title and message of a warning when an I/O took longer
        than its threshold, and of one for each source that couldn't be
        read and each file that couldn't be saved."""
        warnings = [
            ("Storage events", f"Could not read the {source} events: {reason}")
            for source, reason in self.failures.items()
        ]
        warnings.extend(
            ("Storage events", f"Could not save {filename}: {reason}")
            for filename, reason in self.unsaved.items()
        )
        slow = self.slow_ios()
        if slow:
            slowest = max(
                slow,
                key=lambda event: int(event.data.get("RequestDuration_ms", "") or 0),
            )
            warnings.append(
                (
                    "Slow storage I/O",
                    f"Storport recorded {len(slow)} I/O"
                    f"{'s' if len(slow) != 1 else ''} slower than its threshold "
                    f"of {slowest.data.get('Threshold_ms', '?')} ms in this job: "
                    f"the slowest took {slowest.data.get('RequestDuration_ms', '?')} "
                    f"ms on {self.device(slowest)} at {slowest.time}. The "
                    "diagnostics artifact keeps the job's storage events.",
                )
            )
        return warnings


def _write(path: Path, contents: bytes) -> None:
    path.write_bytes(contents)


def save_storage_events(
    since: dt.datetime,
    output_dir: Path,
    volumes: Callable[[], Mapping[str, Path]] | None = None,
    *,
    now: dt.datetime | None = None,
) -> StorageEventReport:
    """Save the storage events from ``since`` to now in ``output_dir``: each
    source's rendered XML, and a summary that also lists every event. Once
    the events are saved, the devices are named by the benchmark's volumes
    that ``volumes`` returns, unless that takes too long, as it can when one
    of them has stalled. A file that can't be written in time is left
    unsaved, which the report records, so the other sources are still
    read."""
    until = now or dt.datetime.now(dt.timezone.utc)
    events: list[StorageEvent] = []
    failures: dict[str, str] = {}
    unsaved: dict[str, str] = {}
    unwritable: str | None = None
    try:
        _within(
            WRITE_TIMEOUT_SECONDS,
            lambda: output_dir.mkdir(parents=True, exist_ok=True),
            "creating the directory",
        )
    except Exception as error:
        unwritable = str(error)

    def save(filename: str, contents: bytes) -> None:
        if unwritable is not None:
            unsaved[filename] = unwritable
            return
        try:
            _within(
                WRITE_TIMEOUT_SECONDS,
                lambda: _write(output_dir / filename, contents),
                "the write",
            )
        except Exception as error:
            unsaved[filename] = str(error)

    for source in EVENT_SOURCES:
        try:
            document = read_events(
                source.channel, event_query(since, until, source.providers)
            )
        except (OSError, ValueError) as error:
            failures[source.name] = str(error)
            continue
        save(source.filename, document)
        try:
            events.extend(parse_events(source.name, document))
        except ElementTree.ParseError as error:
            failures[source.name] = f"unreadable XML: {error}"
        except ValueError as error:
            failures[source.name] = str(error)
    events.sort(key=lambda event: event.time)
    devices: dict[str, str] = {}
    volume_failure: str | None = None
    if volumes is not None:
        try:
            devices = _within(
                VOLUME_NAMING_TIMEOUT_SECONDS,
                lambda: volume_devices(volumes()),
                "probing them",
            )
        except Exception as error:
            volume_failure = str(error)
    report = StorageEventReport(
        since=format_utc(since),
        until=format_utc(until),
        events=events,
        failures=failures,
        devices=devices,
        volume_failure=volume_failure,
        unsaved=unsaved,
    )
    summary = [*report.lines(), "", "All events:"]
    summary.extend(f"  {event.summary()}" for event in events)
    save(SUMMARY_FILENAME, ("\n".join(summary) + "\n").encode("utf-8"))
    return report
