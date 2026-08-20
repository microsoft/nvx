"""Cross-platform VM smoke-test workflows."""

from __future__ import annotations

import hashlib
import os
import struct
import tempfile
import threading
import uuid
from dataclasses import dataclass
from pathlib import Path
import shutil
from typing import Callable

from flamegraph_host import _find_xperf, _read_run_id, extract_host_stacks

from .backends.base import HostBackend
from .common import (
    CommandResult,
    REPO_ROOT,
    ScriptError,
    diagnostic_tail,
    require_file,
    require_tool,
    run_capture,
    run_checked,
)
from .vm import (
    BOOT_MARKER,
    DEFAULT_CMDLINE,
    inspect_hot_page_profile,
    require_hot_page_diagnostic,
    require_positive_diagnostic_count,
    require_profile_inspection,
)


Runner = Callable[..., CommandResult]
HOST_ERROR_PREFIX = "NVX-HOST-ERROR:"
KVM_TIMER_IDLE_MARKER = "NVX-KVM-TIMER-IDLE-RTC-OK"
WHP_SNAPSHOT_MAGIC = b"WHPSNAP"
WHP_SNAPSHOT_HEADER_SIZE = len(WHP_SNAPSHOT_MAGIC) + 4 * 8
WHP_SNAPSHOT_COMPONENT_COUNT = 7
KVM_TIMER_IDLE_SCRIPT = r"""set -eu
local_timer_sum() {
    awk '/^[[:space:]]*LOC:/ { total = 0; for (field = 2; field <= NF; field++) { if ($field !~ /^[0-9]+$/) break; total += $field } print total; exit }' /proc/interrupts
}
uptime_before=$(awk '{ print int($1); exit }' /proc/uptime)
idle_before=$(awk '/^cpu / { print $5; exit }' /proc/stat)
timer_before=$(local_timer_sum)
wall_clock=$(date +%s)
sleep 2
uptime_after=$(awk '{ print int($1); exit }' /proc/uptime)
idle_after=$(awk '/^cpu / { print $5; exit }' /proc/stat)
timer_after=$(local_timer_sum)
[ $((uptime_after - uptime_before)) -ge 1 ]
[ "$idle_after" -gt "$idle_before" ]
[ "$timer_after" -gt "$timer_before" ]
[ "$wall_clock" -ge 1500000000 ]
echo NVX-KVM-TIMER-IDLE-RTC-OK
"""


@dataclass(frozen=True)
class ExecTestConfig:
    microvm: Path
    kernel: Path
    initrd: Path
    mem: int = 128
    timeout: int = 60
    python_initrd: Path | None = None
    python_mem: int = 512


@dataclass(frozen=True)
class ProfilingTestConfig:
    microvm: Path
    kernel: Path
    initrd: Path
    timeout: int = 120
    require_host_profile: bool = False
    wpr_profile: str | None = None
    require_scheduling_events: bool = False


@dataclass(frozen=True)
class InterruptionTestConfig:
    microvm: Path
    kernel: Path
    initrd: Path
    mem: int = 512
    interrupt_after: int = 2


@dataclass(frozen=True)
class HotPagesTestConfig:
    microvm: Path
    kernel: Path
    initrd: Path
    mem: int = 512
    timeout: int = 60
    training_runs: int = 5


@dataclass(frozen=True)
class _HotPageMutation:
    name: str
    status: str
    reason: str
    directory: bool = False


@dataclass(frozen=True)
class _WhpSnapshotLayout:
    header: bytes
    registers: tuple[bytes, ...]
    xsave: bytes
    apic: bytes
    components: tuple[bytes, ...]

    @classmethod
    def parse(cls, data: bytes) -> _WhpSnapshotLayout:
        if len(data) < WHP_SNAPSHOT_HEADER_SIZE or not data.startswith(
            WHP_SNAPSHOT_MAGIC
        ):
            raise ScriptError("corruption matrix requires a current WHPSNAP state.bin")

        position = WHP_SNAPSHOT_HEADER_SIZE
        register_count, position = _read_snapshot_u32(data, position, "register count")
        registers = []
        for index in range(register_count):
            end = position + 20
            if end > len(data):
                raise ScriptError(
                    f"canonical snapshot register {index} is physically truncated"
                )
            registers.append(data[position:end])
            position = end

        xsave, position = _read_snapshot_blob(data, position, "XSAVE")
        apic, position = _read_snapshot_blob(data, position, "local-APIC")
        components = []
        for index in range(WHP_SNAPSHOT_COMPONENT_COUNT):
            component, position = _read_snapshot_blob(
                data, position, f"component {index}"
            )
            components.append(component)
        if position != len(data):
            raise ScriptError("canonical snapshot has trailing aggregate bytes")
        return cls(
            data[:WHP_SNAPSHOT_HEADER_SIZE],
            tuple(registers),
            xsave,
            apic,
            tuple(components),
        )

    def encode(
        self,
        *,
        registers: tuple[bytes, ...] | None = None,
        xsave: bytes | None = None,
        apic: bytes | None = None,
        components: tuple[bytes, ...] | None = None,
        trailing: bytes = b"",
    ) -> bytes:
        encoded_registers = self.registers if registers is None else registers
        encoded_components = self.components if components is None else components
        output = bytearray(self.header)
        output.extend(struct.pack("<I", len(encoded_registers)))
        output.extend(b"".join(encoded_registers))
        _append_snapshot_blob(output, self.xsave if xsave is None else xsave)
        _append_snapshot_blob(output, self.apic if apic is None else apic)
        for component in encoded_components:
            _append_snapshot_blob(output, component)
        output.extend(trailing)
        return bytes(output)


@dataclass(frozen=True)
class _WhpSnapshotMutation:
    name: str
    state: bytes
    diagnostic: str


@dataclass(frozen=True)
class _RestoreInputMutation:
    """One malformed snapshot member planted in place of a valid one."""

    name: str
    member: str
    kind: str
    diagnostic: str
    length: int = 0


def _restore_input_mutations(ram_size: int) -> tuple[_RestoreInputMutation, ...]:
    page = 4096
    lengths = (
        0,
        ram_size - page,
        ram_size - 1,
        ram_size + 1,
        ram_size + page,
    )
    mutations = [
        _RestoreInputMutation(
            f"mem-length-{length}",
            "mem.bin",
            "file",
            f"has logical length {length} bytes; state.bin requires {ram_size} bytes",
            length,
        )
        for length in lengths
    ]
    mutations.extend(
        (
            _RestoreInputMutation(
                "mem-fifo", "mem.bin", "fifo", "mem.bin is not a regular file"
            ),
            _RestoreInputMutation(
                "mem-directory",
                "mem.bin",
                "directory",
                "mem.bin is not a regular file",
            ),
            _RestoreInputMutation(
                "state-fifo", "state.bin", "fifo", "state.bin is not a regular file"
            ),
            _RestoreInputMutation(
                "lock-fifo",
                ".snapshot.lock",
                "fifo",
                ".snapshot.lock is not a regular file",
            ),
        )
    )
    return tuple(mutations)


def _plant_restore_input(
    snapshot: Path, malformed: Path, mutation: _RestoreInputMutation
) -> None:
    """Rebuilds `malformed` as a snapshot whose `mutation.member` is invalid."""
    if malformed.exists():
        shutil.rmtree(malformed)
    malformed.mkdir()
    for member in ("mem.bin", "state.bin"):
        if member != mutation.member:
            os.link(snapshot / member, malformed / member)
    target = malformed / mutation.member
    if mutation.kind == "fifo":
        os.mkfifo(target)
    elif mutation.kind == "directory":
        target.mkdir()
    else:
        with open(target, "wb") as image:
            image.truncate(mutation.length)


def _read_snapshot_u32(data: bytes, position: int, label: str) -> tuple[int, int]:
    end = position + 4
    if end > len(data):
        raise ScriptError(f"canonical snapshot {label} is truncated")
    return struct.unpack_from("<I", data, position)[0], end


def _read_snapshot_blob(data: bytes, position: int, label: str) -> tuple[bytes, int]:
    length, position = _read_snapshot_u32(data, position, f"{label} length")
    end = position + length
    if end > len(data):
        raise ScriptError(f"canonical snapshot {label} payload is truncated")
    return data[position:end], end


def _append_snapshot_blob(output: bytearray, payload: bytes) -> None:
    output.extend(struct.pack("<I", len(payload)))
    output.extend(payload)


def _replace_register_name(record: bytes, raw_name: int) -> bytes:
    return struct.pack("<i", raw_name) + record[4:]


def _replace_component(
    components: tuple[bytes, ...], index: int, payload: bytes
) -> tuple[bytes, ...]:
    replaced = list(components)
    replaced[index] = payload
    return tuple(replaced)


def _replace_bytes(payload: bytes, *changes: tuple[int, int]) -> bytes:
    replaced = bytearray(payload)
    for offset, value in changes:
        replaced[offset] = value
    return bytes(replaced)


def _whp_snapshot_mutations(data: bytes) -> tuple[_WhpSnapshotMutation, ...]:
    layout = _WhpSnapshotLayout.parse(data)
    if len(layout.registers) != 54:
        raise ScriptError(
            f"canonical snapshot has {len(layout.registers)} registers; expected 54"
        )
    if not 1 < len(layout.xsave) < 8192:
        raise ScriptError(
            "canonical snapshot XSAVE size cannot exercise short and long mutations"
        )
    if len(layout.apic) != 4096:
        raise ScriptError(
            f"canonical snapshot local-APIC state is {len(layout.apic)} bytes; expected 4096"
        )
    expected_component_sizes = (16, 33, 1)
    for index, expected in enumerate(expected_component_sizes):
        actual = len(layout.components[index])
        if actual != expected:
            raise ScriptError(
                f"canonical snapshot component {index} is {actual} bytes; expected {expected}"
            )

    mutations: list[_WhpSnapshotMutation] = []

    def add(name: str, state: bytes, diagnostic: str) -> None:
        mutations.append(_WhpSnapshotMutation(name, state, diagnostic))

    add(
        "register-count-0",
        layout.encode(registers=()),
        "snapshot records 0 WHP registers",
    )
    add(
        "register-count-53",
        layout.encode(registers=layout.registers[:-1]),
        "snapshot records 53 WHP registers",
    )
    add(
        "register-count-55",
        layout.encode(registers=(*layout.registers, layout.registers[-1])),
        "snapshot records 55 WHP registers",
    )

    duplicate = list(layout.registers)
    duplicate[1] = _replace_register_name(duplicate[1], 0)
    add(
        "register-name-duplicate",
        layout.encode(registers=tuple(duplicate)),
        "snapshot WHP register 1",
    )
    unknown = list(layout.registers)
    unknown[17] = _replace_register_name(unknown[17], 0x7FFF_FFFF)
    add(
        "register-name-unknown",
        layout.encode(registers=tuple(unknown)),
        "snapshot WHP register 17",
    )
    reordered = list(layout.registers)
    reordered[16], reordered[17] = reordered[17], reordered[16]
    add(
        "register-name-reordered",
        layout.encode(registers=tuple(reordered)),
        "snapshot WHP register 16",
    )

    for name, payload, diagnostic in (
        ("xsave-empty", b"", "snapshot XSAVE state is empty"),
        ("xsave-short", layout.xsave[:-1], "snapshot XSAVE state is"),
        ("xsave-long", layout.xsave + b"\0", "snapshot XSAVE state is"),
        ("xsave-oversized", bytes(8193), "snapshot XSAVE state exceeds 8192 bytes"),
    ):
        add(name, layout.encode(xsave=payload), diagnostic)
    for name, payload in (
        ("apic-empty", b""),
        ("apic-short", layout.apic[:-1]),
        ("apic-long", layout.apic + b"\0"),
    ):
        add(name, layout.encode(apic=payload), "snapshot local-APIC state is")

    pic, pit, rtc, console = layout.components[:4]
    for name, payload in (
        ("pic-empty", b""),
        ("pic-short", pic[:-1]),
        ("pic-long", pic + b"\0"),
    ):
        add(
            name,
            layout.encode(components=_replace_component(layout.components, 0, payload)),
            "snapshot PIC state",
        )
    for name, payload, diagnostic in (
        ("pic-unaligned-base", _replace_bytes(pic, (3, pic[3] | 1)), "vector base"),
        ("pic-invalid-step", _replace_bytes(pic, (4, 4)), "initialization step"),
        ("pic-invalid-boolean", _replace_bytes(pic, (5, 2)), "invalid boolean"),
        ("pic-unsupported-request", _replace_bytes(pic, (1, 2)), "request register"),
        ("pic-slave-request", _replace_bytes(pic, (9, 1)), "request register"),
        ("pic-multiple-in-service", _replace_bytes(pic, (2, 3)), "multiple in-service"),
        (
            "pic-uninitialized-in-service",
            _replace_bytes(pic, (2, 1), (7, 0)),
            "uninitialized with an in-service interrupt",
        ),
        (
            "pic-icw4-sequence",
            _replace_bytes(pic, (4, 3), (5, 0)),
            "expects ICW4",
        ),
    ):
        add(
            name,
            layout.encode(components=_replace_component(layout.components, 0, payload)),
            diagnostic,
        )
    for name, size in (
        ("pit-empty", 0),
        ("pit-one-channel", 11),
        ("pit-two-channels", 22),
        ("pit-short", 32),
        ("pit-long", 34),
    ):
        add(
            name,
            layout.encode(
                components=_replace_component(
                    layout.components, 1, pit[:size] + bytes(max(0, size - len(pit)))
                )
            ),
            "snapshot PIT state",
        )
    for name, payload, diagnostic in (
        ("pit-invalid-access", _replace_bytes(pit, (2, 3)), "invalid PIT access mode"),
        ("pit-alias-mode-6", _replace_bytes(pit, (3, 6)), "invalid mode 6"),
        ("pit-alias-mode-7", _replace_bytes(pit, (3, 7)), "invalid mode 7"),
        ("pit-invalid-mode", _replace_bytes(pit, (3, 8)), "invalid mode"),
        ("pit-invalid-gate", _replace_bytes(pit, (4, 2)), "gate flag"),
        ("pit-invalid-read-phase", _replace_bytes(pit, (5, 2)), "invalid read phase"),
        ("pit-invalid-latch-flag", _replace_bytes(pit, (6, 2)), "latch flag"),
        ("pit-invalid-write-flag", _replace_bytes(pit, (9, 2)), "partial-write flag"),
        (
            "pit-latch-value-without-flag",
            _replace_bytes(pit, (6, 0), (7, 1), (8, 0)),
            "latch value without the latch flag",
        ),
        (
            "pit-write-value-without-flag",
            _replace_bytes(pit, (9, 0), (10, 1)),
            "partial-write byte without the flag",
        ),
        (
            "pit-partial-write-wrong-access",
            _replace_bytes(pit, (2, 0), (5, 0), (9, 1), (10, 1)),
            "partial write outside low/high access mode",
        ),
    ):
        add(
            name,
            layout.encode(components=_replace_component(layout.components, 1, payload)),
            diagnostic,
        )
    for name, payload in (("rtc-empty", b""), ("rtc-long", rtc + b"\0")):
        add(
            name,
            layout.encode(components=_replace_component(layout.components, 2, payload)),
            "snapshot RTC state",
        )
    add(
        "rtc-unrepresentable-index",
        layout.encode(
            components=_replace_component(layout.components, 2, bytes([rtc[0] | 0x80]))
        ),
        "is not representable",
    )
    for name, payload in (
        ("console-short-prefix", console[:3]),
        ("console-short-queue", struct.pack("<I", 1)),
        ("console-trailing", console + b"\0"),
    ):
        add(
            name,
            layout.encode(components=_replace_component(layout.components, 3, payload)),
            "snapshot port-console",
        )
    add(
        "console-oversized-queue",
        layout.encode(
            components=_replace_component(
                layout.components, 3, struct.pack("<I", (64 << 20) + 1)
            )
        ),
        "snapshot port-console queue has",
    )

    net, virtiofs, workload = layout.components[4:]
    if net:
        raise ScriptError(
            "exec corruption fixture unexpectedly contains virt-net state"
        )
    if len(virtiofs) < 101:
        raise ScriptError("exec corruption fixture has incomplete virtio-fs state")
    if len(workload) < 99:
        raise ScriptError("exec corruption fixture has incomplete virtio-console state")

    for name, payload, diagnostic in (
        ("net-short-header", b"\0", "virt-net snapshot header truncated"),
        ("net-invalid-prefix", bytes([10, 0, 0, 2, 0]), "prefix /0 out of range"),
        (
            "net-truncated-device",
            bytes([10, 0, 0, 2, 24]),
            "virt-net snapshot truncated",
        ),
        (
            "net-trailing",
            bytes([10, 0, 0, 2, 24]) + bytes(28 + 2 * 31) + b"\0",
            "virt-net snapshot has trailing bytes",
        ),
    ):
        add(
            name,
            layout.encode(components=_replace_component(layout.components, 4, payload)),
            diagnostic,
        )

    for name, payload, diagnostic in (
        ("virtiofs-short-header", b"\1", "unsupported virtio-fs snapshot header"),
        (
            "virtiofs-invalid-header",
            _replace_bytes(virtiofs, (0, 2)),
            "unsupported virtio-fs snapshot header",
        ),
        (
            "virtiofs-invalid-writable",
            _replace_bytes(virtiofs, (1, 2)),
            "writable flag has invalid value",
        ),
        (
            "virtiofs-invalid-status",
            _replace_bytes(virtiofs, (18, virtiofs[18] | 0x10)),
            "invalid status bits",
        ),
        (
            "virtiofs-invalid-ready",
            _replace_bytes(virtiofs, (30, 2)),
            "invalid ready value",
        ),
        (
            "virtiofs-protocol-version",
            _replace_bytes(virtiofs, (96, 3), (97, 0), (98, 0), (99, 0)),
            "unsupported virtio-fs protocol state version",
        ),
        (
            "virtiofs-invalid-initialized",
            _replace_bytes(virtiofs, (100, 2)),
            "initialized flag has invalid value",
        ),
        (
            "virtiofs-colliding-next-node",
            virtiofs[:101] + struct.pack("<Q", 1) + virtiofs[109:],
            "next node ID 1 collides with retained state",
        ),
        (
            "virtiofs-trailing",
            virtiofs + b"\0",
            "virtio-fs snapshot has trailing bytes",
        ),
    ):
        add(
            name,
            layout.encode(components=_replace_component(layout.components, 5, payload)),
            diagnostic,
        )

    workload_input_length = 95
    for name, payload, diagnostic in (
        (
            "workload-invalid-magic",
            _replace_bytes(workload, (0, workload[0] ^ 0xFF)),
            "invalid virtio-console snapshot magic",
        ),
        (
            "workload-invalid-version",
            _replace_bytes(workload, (4, 2)),
            "unsupported virtio-console snapshot version",
        ),
        (
            "workload-invalid-status",
            _replace_bytes(workload, (21, workload[21] | 0x10)),
            "invalid status bits",
        ),
        (
            "workload-invalid-ready",
            _replace_bytes(workload, (33, 2)),
            "invalid ready value",
        ),
        (
            "workload-oversized-input",
            workload[:workload_input_length]
            + struct.pack("<I", (4 << 20) + 1)
            + workload[workload_input_length + 4 :],
            "virtio-console snapshot input exceeds",
        ),
        (
            "workload-trailing",
            workload + b"\0",
            "virtio-console snapshot has trailing bytes",
        ),
    ):
        add(
            name,
            layout.encode(components=_replace_component(layout.components, 6, payload)),
            diagnostic,
        )

    unmapped_workload = bytearray(workload)
    unmapped_workload[33] = 1
    unmapped_workload[34:36] = struct.pack("<H", 8)
    unmapped_workload[36:44] = struct.pack("<Q", 0x1_0000_0000)
    unmapped_workload[44:52] = struct.pack("<Q", 0x1_0000_1000)
    unmapped_workload[52:60] = struct.pack("<Q", 0x1_0000_2000)
    add(
        "workload-unmapped-ready-queue",
        layout.encode(
            components=_replace_component(
                layout.components, 6, bytes(unmapped_workload)
            )
        ),
        "invalid virtqueue descriptor table",
    )

    swapped = list(layout.components)
    swapped[0], swapped[1] = swapped[1], swapped[0]
    add(
        "component-pic-pit-swapped",
        layout.encode(components=tuple(swapped)),
        "snapshot PIC state",
    )
    swapped = list(layout.components)
    swapped[5], swapped[6] = swapped[6], swapped[5]
    add(
        "component-virtiofs-workload-swapped",
        layout.encode(components=tuple(swapped)),
        "unsupported virtio-fs snapshot header",
    )
    add(
        "component-virtiofs-substituted-workload",
        layout.encode(components=_replace_component(layout.components, 5, workload)),
        "unsupported virtio-fs snapshot header",
    )
    add(
        "component-workload-substituted-virtiofs",
        layout.encode(components=_replace_component(layout.components, 6, virtiofs)),
        "invalid virtio-console snapshot magic",
    )
    add(
        "aggregate-trailing",
        layout.encode(trailing=b"\0"),
        "snapshot has trailing bytes",
    )
    return tuple(mutations)


class _WindowsNamedPipeObserver:
    def __init__(self) -> None:
        if os.name != "nt":
            raise ScriptError("restore-ready observation requires Windows")
        import ctypes
        from ctypes import wintypes

        self._ctypes = ctypes
        self._wintypes = wintypes
        self._kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        self.path = rf"\\.\pipe\nvx-corruption-{uuid.uuid4()}"
        self._data = b""
        self._error: str | None = None

        self._kernel32.CreateNamedPipeW.argtypes = (
            wintypes.LPCWSTR,
            wintypes.DWORD,
            wintypes.DWORD,
            wintypes.DWORD,
            wintypes.DWORD,
            wintypes.DWORD,
            wintypes.DWORD,
            wintypes.LPVOID,
        )
        self._kernel32.CreateNamedPipeW.restype = wintypes.HANDLE
        self._kernel32.ConnectNamedPipe.argtypes = (wintypes.HANDLE, wintypes.LPVOID)
        self._kernel32.ConnectNamedPipe.restype = wintypes.BOOL
        self._kernel32.ReadFile.argtypes = (
            wintypes.HANDLE,
            wintypes.LPVOID,
            wintypes.DWORD,
            wintypes.LPDWORD,
            wintypes.LPVOID,
        )
        self._kernel32.ReadFile.restype = wintypes.BOOL
        self._kernel32.CreateFileW.argtypes = (
            wintypes.LPCWSTR,
            wintypes.DWORD,
            wintypes.DWORD,
            wintypes.LPVOID,
            wintypes.DWORD,
            wintypes.DWORD,
            wintypes.HANDLE,
        )
        self._kernel32.CreateFileW.restype = wintypes.HANDLE
        self._kernel32.CloseHandle.argtypes = (wintypes.HANDLE,)
        self._kernel32.CloseHandle.restype = wintypes.BOOL

        self._handle = self._kernel32.CreateNamedPipeW(
            self.path,
            0x00000003,
            0,
            1,
            4096,
            4096,
            0,
            None,
        )
        if self._handle == wintypes.HANDLE(-1).value:
            raise ScriptError(
                f"creating restore-ready pipe failed with Windows error {ctypes.get_last_error()}"
            )
        self._started = threading.Event()
        self._thread = threading.Thread(target=self._observe, daemon=True)
        self._thread.start()
        if not self._started.wait(timeout=5):
            self._kernel32.CloseHandle(self._handle)
            raise ScriptError("restore-ready pipe observer did not start")

    def _observe(self) -> None:
        self._started.set()
        connected = self._kernel32.ConnectNamedPipe(self._handle, None)
        error = self._ctypes.get_last_error()
        if not connected and error != 535:
            self._error = (
                f"connecting restore-ready pipe failed with Windows error {error}"
            )
            return
        buffer = self._ctypes.create_string_buffer(4096)
        received = self._wintypes.DWORD()
        read = self._kernel32.ReadFile(
            self._handle,
            buffer,
            len(buffer),
            self._ctypes.byref(received),
            None,
        )
        error = self._ctypes.get_last_error()
        if read or received.value:
            self._data = buffer.raw[: received.value]
        elif error not in (109, 232):
            self._error = (
                f"reading restore-ready pipe failed with Windows error {error}"
            )

    def finish(self) -> bytes:
        if self._thread.is_alive():
            dummy = self._kernel32.CreateFileW(
                self.path,
                0xC0000000,
                0,
                None,
                3,
                0,
                None,
            )
            if dummy != self._wintypes.HANDLE(-1).value:
                self._kernel32.CloseHandle(dummy)
        self._thread.join(timeout=5)
        self._kernel32.CloseHandle(self._handle)
        if self._thread.is_alive():
            raise ScriptError("restore-ready pipe observer did not stop")
        if self._error is not None:
            raise ScriptError(self._error)
        return self._data


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


def _snapshot_before_exec_restore_args(
    config: ExecTestConfig,
    snapshot: Path,
    mount_root: Path | None,
    console: str,
) -> list[str | Path]:
    args: list[str | Path] = [
        config.microvm,
        "--restore",
        snapshot,
        "--console",
        console,
    ]
    if mount_root is not None:
        args.extend(["--mount", mount_root])
    args.extend(
        [
            "--output-after-marker",
            "NVX-EXEC-START",
            "--log-level",
            "off",
        ]
    )
    return args


def _test_snapshot_before_exec(
    config: ExecTestConfig,
    backend: HostBackend,
    mount_root: Path,
    snapshot_root: Path,
    script_path: Path,
    runner: Runner,
) -> None:
    initramfses = [("shell", config.initrd, config.mem)]
    if config.python_initrd is not None:
        initramfses.append(("python", config.python_initrd, config.python_mem))
    cpu_counts = (1, 2) if backend.supports_vcpus else (1,)

    for initramfs_name, initrd, mem in initramfses:
        for console in ("portb", "virtio"):
            for vcpus in cpu_counts:
                case = f"{initramfs_name}-{console}-{vcpus}vcpu"
                snapshot = snapshot_root.with_name(f"{snapshot_root.name}-{case}")
                original_marker = f"NVX-SNAPSHOT-ORIGINAL-{case.upper()}"
                script_path.write_text(
                    f"printf '{original_marker}'\nexit 99\n",
                    encoding="utf-8",
                    newline="\n",
                )
                capture_args: list[str | Path] = [
                    config.microvm,
                    "--kernel",
                    config.kernel,
                    "--initrd",
                    initrd,
                    "--mem",
                    str(mem),
                ]
                backend.add_vcpus(capture_args, vcpus)
                capture_args.extend(
                    [
                        "--console",
                        console,
                        "--mount",
                        mount_root,
                        "--exec",
                        "/mnt/host/workload.sh",
                        "--snapshot",
                        snapshot,
                        "--snapshot-before-exec",
                        "--log-level",
                        "off",
                    ]
                )
                capture = runner(capture_args, timeout=config.timeout)
                _require_result(capture, 0)
                if original_marker in capture.text:
                    raise ScriptError(
                        f"snapshot-before-exec ran the original script for {case}"
                    )

                members = ("mem.bin", "state.bin")
                for member in members:
                    require_file(
                        snapshot / member,
                        f"snapshot-before-exec did not write {member} for {case}",
                    )
                member_hashes = {
                    member: _sha256_file(snapshot / member) for member in members
                }

                for generation, expected_status in ((1, 40), (2, 41)):
                    marker = f"NVX-SNAPSHOT-RESTORE-{generation}-{case.upper()}"
                    lines = []
                    if vcpus > 1:
                        lines.extend(
                            [
                                "/mnt/host/kvm-pin-ap || exit 98",
                                "[ \"$(awk '{ print $39 }' /proc/$$/stat)\" = 1 ] || exit 98",
                            ]
                        )
                    lines.extend([f"printf '{marker}'", f"exit {expected_status}"])
                    script_path.write_text(
                        "\n".join(lines) + "\n",
                        encoding="utf-8",
                        newline="\n",
                    )
                    restored = runner(
                        _snapshot_before_exec_restore_args(
                            config, snapshot, mount_root, console
                        ),
                        timeout=config.timeout,
                    )
                    _require_result(restored, expected_status)
                    if restored.stdout != marker.encode("ascii"):
                        raise ScriptError(
                            f"{case} restore output was not byte-faithful: "
                            f"{restored.stdout!r}"
                        )
                    _require_snapshot_unchanged(snapshot, member_hashes)

                script_path.unlink()
                missing_script = runner(
                    _snapshot_before_exec_restore_args(
                        config, snapshot, mount_root, console
                    ),
                    timeout=config.timeout,
                )
                _require_result(
                    missing_script,
                    127,
                    required_output="executable script not found",
                )
                _require_snapshot_unchanged(snapshot, member_hashes)

                missing_attachment = runner(
                    _snapshot_before_exec_restore_args(config, snapshot, None, console),
                    timeout=config.timeout,
                )
                _require_result(
                    missing_attachment,
                    1,
                    required_error="supply the per-run host directory with --mount",
                )
                if "NVX-SNAPSHOT-RESTORE" in missing_attachment.text:
                    raise ScriptError(
                        f"missing attachment executed the restored guest for {case}"
                    )
                _require_snapshot_unchanged(snapshot, member_hashes)

        mount_failure_snapshot = snapshot_root.with_name(
            f"{snapshot_root.name}-{initramfs_name}-mount-failure"
        )
        mount_failure_marker = f"NVX-MOUNT-FAILURE-SCRIPT-{initramfs_name.upper()}"
        script_path.write_text(
            f"printf '{mount_failure_marker}'\nexit 99\n",
            encoding="utf-8",
            newline="\n",
        )
        mount_failure_capture: list[str | Path] = [
            config.microvm,
            "--kernel",
            config.kernel,
            "--initrd",
            initrd,
            "--mem",
            str(mem),
        ]
        backend.add_vcpus(mount_failure_capture, 1)
        mount_failure_capture.extend(
            [
                "--console",
                "portb",
                "--mount",
                mount_root,
                "--mount-target",
                "/proc/nvx-host",
                "--exec",
                "/proc/nvx-host/workload.sh",
                "--snapshot",
                mount_failure_snapshot,
                "--snapshot-before-exec",
                "--log-level",
                "off",
            ]
        )
        capture = runner(mount_failure_capture, timeout=config.timeout)
        _require_result(capture, 0)
        if mount_failure_marker in capture.text:
            raise ScriptError(
                f"snapshot-before-exec ran the mount-failure script for {initramfs_name}"
            )
        members = ("mem.bin", "state.bin")
        for member in members:
            require_file(
                mount_failure_snapshot / member,
                f"mount-failure snapshot did not write {member} for {initramfs_name}",
            )
        member_hashes = {
            member: _sha256_file(mount_failure_snapshot / member) for member in members
        }
        mount_failure = runner(
            _snapshot_before_exec_restore_args(
                config, mount_failure_snapshot, mount_root, "portb"
            ),
            timeout=config.timeout,
        )
        _require_result(mount_failure, 126)
        if mount_failure_marker in mount_failure.text:
            raise ScriptError(
                f"mount failure executed the restored script for {initramfs_name}"
            )
        _require_snapshot_unchanged(mount_failure_snapshot, member_hashes)

    print(f"{backend.name} snapshot-before-exec matrix passed")


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
    if "Linux version" in result.text or "virtfs: mounted" in result.text:
        raise ScriptError(
            f"guest exec leaked boot logs before workload output\n{result.text}"
        )
    print(f"guest exec status {expected_status} propagated")


def _require_no_snapshot_staging(snapshot: Path) -> None:
    staging = sorted(
        path.name
        for path in snapshot.iterdir()
        if path.name.startswith(".")
        and (path.name.endswith(".tmp") or path.name.endswith(".old"))
    )
    if staging:
        raise ScriptError(
            f"successful snapshot publication left staging files: {', '.join(staging)}"
        )


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
    if config.python_initrd is not None:
        require_file(
            config.python_initrd,
            f"missing Python initrd: {config.python_initrd}",
        )

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

        if backend.name == "linux-kvm":
            script_path.write_text(
                KVM_TIMER_IDLE_SCRIPT, encoding="utf-8", newline="\n"
            )
            _run_exec_case(
                config,
                backend,
                mount_root,
                "/mnt/host/workload.sh",
                0,
                KVM_TIMER_IDLE_MARKER,
                runner=runner,
            )

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

        _test_snapshot_before_exec(
            config,
            backend,
            mount_root,
            work_root / "snapshot-before-exec",
            script_path,
            runner,
        )

        if backend.name == "windows-whp":
            _test_windows_exec_extensions(
                config,
                mount_root,
                work_root / "snapshot-whp-integrity",
                script_path,
                runner,
            )
        elif backend.name == "linux-kvm":
            _test_kvm_restore_input_rejection(
                config,
                mount_root,
                work_root / "snapshot-kvm-integrity",
                script_path,
                runner,
            )

    print(f"{backend.name} exec smoke test passed")


def test_interruption(
    config: InterruptionTestConfig,
    backend: HostBackend,
    *,
    runner: Runner = run_capture,
) -> None:
    if backend.name not in {"linux-kvm", "windows-whp"}:
        raise ScriptError(
            f"interruption smoke testing is unavailable on {backend.name}"
        )
    require_file(config.microvm, f"missing VMM: {config.microvm}")
    require_file(config.kernel, f"missing kernel: {config.kernel}")
    require_file(config.initrd, f"missing initrd: {config.initrd}")

    if backend.name == "linux-kvm":
        backend.cleanup_network()
        if interfaces := backend.network_interfaces():
            raise ScriptError(
                f"cannot start with stale nvx TAP interfaces: {', '.join(interfaces)}"
            )
    wpr_instance = f"nvxinterrupt{os.getpid()}"
    host_profile = backend.name == "windows-whp" and os.environ.get(
        "NVX_WPR_INTERRUPTION", ""
    ).strip().lower() in {"1", "true", "yes"}
    if host_profile and runner is run_capture:
        require_tool("wpr", "NVX_WPR_INTERRUPTION requires WPR")
    interruption_env = None
    if host_profile:
        interruption_env = os.environ.copy()
        interruption_env["NVX_WPR_INSTANCE"] = wpr_instance

    with tempfile.TemporaryDirectory(prefix="nvx-interruption-") as mount:
        interruption_script = Path(mount) / "interruption.sh"
        interruption_script.write_text("sleep 30\n", encoding="utf-8", newline="\n")
        args: list[str | Path] = [
            config.microvm,
            "--kernel",
            config.kernel,
            "--initrd",
            config.initrd,
            "--mem",
            str(config.mem),
        ]
        backend.add_vcpus(args, 2 if backend.name == "linux-kvm" else 1)
        args.extend(
            [
                "--mount",
                Path(mount),
                "--mount-rw",
                "--net",
                "10.0.0.2/24",
                "--quiet",
                "--log-level",
                "info",
            ]
        )
        if backend.name == "windows-whp":
            args.extend(
                [
                    "--guest-profile",
                    Path(mount) / "interrupt.folded",
                    "--profile-hz",
                    "100",
                ]
            )
            if host_profile:
                args.append("--host-profile")

        def interrupt(
            command: list[str | Path],
            expected_status: int,
            label: str,
            required_markers: list[str] | None = None,
        ) -> CommandResult:
            runner_options: dict[str, object] = {
                "timeout": config.interrupt_after,
                "graceful_timeout": True,
            }
            if backend.name == "windows-whp":
                runner_options["interrupt_marker"] = (
                    "WHP runtime ready for host interruption"
                )
                if interruption_env is not None:
                    runner_options["env"] = interruption_env
            result: CommandResult | None = None
            failure: Exception | None = None
            cleanup_failures: list[str] = []
            try:
                result = runner(command, **runner_options)
                if required_markers is None:
                    required = [
                        "virtio-fs: exporting",
                        "host interrupt received - stopping guest",
                    ]
                    if backend.name == "linux-kvm":
                        required.extend(["virt-net: host TAP", "SMP: created 2 vCPUs"])
                    else:
                        required.extend(
                            [
                                "virt-net: standalone SLIRP NIC",
                                "starting guest",
                                "WHP runtime ready for host interruption",
                            ]
                        )
                        if host_profile:
                            required.append("host profiling active")
                        if expected_status != 0:
                            required.append(HOST_ERROR_PREFIX)
                else:
                    required = required_markers
                missing = [marker for marker in required if marker not in result.text]
                if not result.timed_out or not result.interrupted:
                    raise ScriptError(
                        f"{label} exited before the coordinated interruption"
                    )
                if result.killed:
                    raise ScriptError(f"{label} required forced termination")
                if result.returncode != expected_status:
                    raise ScriptError(
                        f"{label} exited {result.returncode}, expected {expected_status}"
                    )
                if missing:
                    raise ScriptError(f"{label} output missed: {', '.join(missing)}")
            except Exception as error:
                failure = error

            if backend.name == "linux-kvm":
                leaked = backend.network_interfaces()
                if leaked:
                    backend.cleanup_network()
                    cleanup_failures.append(
                        f"{label} leaked nvx TAP interfaces: {', '.join(leaked)}"
                    )
            if backend.name == "windows-whp" and runner is run_capture:
                if host_profile:
                    try:
                        _require_wpr_inactive(wpr_instance, runner=run_capture)
                    except Exception as error:
                        cleanup_failures.append(f"{label} WPR cleanup failed: {error}")
                leftovers = _interrupted_host_trace_leftovers(Path(mount))
                if leftovers:
                    cleanup_failures.append(
                        f"{label} leaked host-trace artifacts: {', '.join(leftovers)}"
                    )
                    for path in leftovers:
                        try:
                            (Path(mount) / path).unlink(missing_ok=True)
                        except OSError as error:
                            cleanup_failures.append(
                                f"{label} could not remove host-trace artifact {path}: {error}"
                            )
            if failure is not None:
                if cleanup_failures:
                    raise failure from ScriptError("; ".join(cleanup_failures))
                raise failure
            if cleanup_failures:
                raise ScriptError("; ".join(cleanup_failures))
            if result is None:
                raise ScriptError(f"{label} produced no process result")
            return result

        interrupt(args, 0, "interactive interruption smoke VM")
        if backend.name == "windows-whp":
            interrupt(args, 0, "immediate-repeat interruption smoke VM")
            interrupt(
                [*args, "--exec", "/mnt/host/interruption.sh"],
                1,
                "exec-gated interruption smoke VM",
            )
            if runner is run_capture:
                snapshot = Path(mount) / "snapshot"
                capture = run_capture(
                    [
                        config.microvm,
                        "--kernel",
                        config.kernel,
                        "--initrd",
                        config.initrd,
                        "--mem",
                        str(config.mem),
                        "--cmdline",
                        f"{DEFAULT_CMDLINE} shellsnap",
                        "--snapshot",
                        snapshot,
                        "--quiet",
                    ],
                    timeout=60,
                )
                _require_result(capture, 0)
                require_file(
                    snapshot / "state.bin",
                    "interruption restore setup missed state.bin",
                )
                require_file(
                    snapshot / "mem.bin", "interruption restore setup missed mem.bin"
                )
                training = run_capture(
                    [
                        config.microvm,
                        "--restore",
                        snapshot,
                        "--snapshot-prefetch",
                        "off",
                        "--snapshot-profile-generate",
                        "--exit-on-boot",
                        "--quiet",
                    ],
                    timeout=60,
                )
                _require_result(training, 0)
                require_file(
                    snapshot / "hot-pages.whp.v1",
                    "interruption restore setup missed the hot-page profile",
                )
                observer = _WindowsNamedPipeObserver()
                try:
                    restore_args: list[str | Path] = [
                        config.microvm,
                        "--restore",
                        snapshot,
                        "--snapshot-prefetch",
                        "auto",
                        "--restore-ready-pipe",
                        observer.path,
                        "--guest-profile",
                        Path(mount) / "interrupt-restore.folded",
                        "--profile-hz",
                        "100",
                        "--quiet",
                        "--log-level",
                        "info",
                    ]
                    if host_profile:
                        restore_args.append("--host-profile")
                    restore_markers = [
                        "resuming guest from snapshot",
                        "WHP runtime ready for host interruption",
                        "host interrupt received - stopping guest",
                        "snapshot-hot-pages: status=active",
                    ]
                    if host_profile:
                        restore_markers.append("host profiling active")
                    interrupt(
                        restore_args,
                        0,
                        "restored interruption smoke VM",
                        restore_markers,
                    )
                finally:
                    readiness = observer.finish()
                if b'"type":"RestoreReady"' not in readiness:
                    raise ScriptError(
                        "restored interruption smoke VM missed restore readiness"
                    )
            mount_path = Path(mount)
            released_path = mount_path.with_name(f"{mount_path.name}-released")
            mount_path.rename(released_path)
            released_path.rename(mount_path)

    if backend.name == "linux-kvm":
        print("PASS: coordinated SIGINT joined workers and removed the KVM TAP")
    else:
        print(
            "PASS: coordinated Ctrl+Break joined WHP workers and released host resources"
        )


def _interrupted_host_trace_leftovers(root: Path) -> list[str]:
    return sorted(
        path.name
        for path in root.iterdir()
        if ".partial" in path.name
        or path.name.startswith("nvx-wpr-")
        or path.name.endswith(".host.etl")
        or path.name.endswith(".host.etl.pid")
    )


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _require_snapshot_unchanged(snapshot: Path, expected: dict[str, str]) -> None:
    actual = {name: _sha256_file(snapshot / name) for name in expected}
    if actual != expected:
        changed = sorted(name for name in expected if actual[name] != expected[name])
        raise ScriptError(f"workflow modified snapshot members: {', '.join(changed)}")


def _require_no_hot_page_staging(snapshot: Path) -> None:
    staging = sorted(snapshot.glob(".hot-pages.whp.v1.*.tmp"))
    if staging:
        raise ScriptError(
            "hot-page workflow leaked staging files: "
            + ", ".join(path.name for path in staging)
        )


def _hot_page_profile_mutations() -> tuple[_HotPageMutation, ...]:
    return (
        _HotPageMutation("missing", "missing", "not-found"),
        _HotPageMutation("unreadable", "unreadable", "read-error", directory=True),
        _HotPageMutation("truncated", "invalid", "validation-error"),
        _HotPageMutation("checksum-corrupt", "invalid", "validation-error"),
        _HotPageMutation("wrong-identity", "invalid", "validation-error"),
        _HotPageMutation("wrong-ram-size", "invalid", "validation-error"),
        _HotPageMutation("wrong-layout", "invalid", "validation-error"),
        _HotPageMutation("overlapping-ranges", "invalid", "validation-error"),
        _HotPageMutation("out-of-range", "invalid", "validation-error"),
        _HotPageMutation("oversized", "unreadable", "read-error"),
        _HotPageMutation("unsupported-algorithm", "invalid", "validation-error"),
    )


def _replace_hot_page_profile(
    config: HotPagesTestConfig,
    snapshot: Path,
    path: Path,
    valid_profile: bytes,
    mutation: _HotPageMutation,
    runner: Runner,
) -> None:
    if path.is_dir():
        shutil.rmtree(path)
    else:
        path.unlink(missing_ok=True)
    if mutation.name == "missing":
        return
    if mutation.directory:
        path.mkdir()
        return
    path.write_bytes(valid_profile)
    result = runner(
        [
            config.microvm,
            "--restore",
            snapshot,
            "--snapshot-profile-test-mutate",
            mutation.name,
            "--quiet",
        ],
        timeout=config.timeout,
    )
    _require_result(result, 0)


def _require_hot_page_fallback(
    result: CommandResult,
    label: str,
    mutation: _HotPageMutation,
    mode: str,
) -> None:
    _require_result(result, 0)
    if mode == "auto":
        if "snapshot-hot-pages:" in result.text:
            raise ScriptError(f"{label} emitted an unexpected auto fallback diagnostic")
        return
    require_hot_page_diagnostic(
        result,
        label,
        {
            "status": mutation.status,
            "selected_pages": "0",
            "ranges": "0",
            "reason": mutation.reason,
            "fallback": "demand",
        },
    )


def _test_hot_page_fallbacks(
    config: HotPagesTestConfig,
    snapshot: Path,
    common: list[str | Path],
    profile: Path,
    runner: Runner,
) -> None:
    valid_profile = profile.read_bytes()
    mutations = _hot_page_profile_mutations()
    try:
        for mutation in mutations:
            _replace_hot_page_profile(
                config, snapshot, profile, valid_profile, mutation, runner
            )
            for mode in ("auto", "required", "eager", "full"):
                label = f"hot-page {mutation.name} {mode} fallback"
                result = runner(
                    [*common, "--snapshot-prefetch", mode],
                    timeout=config.timeout,
                )
                _require_hot_page_fallback(result, label, mutation, mode)
            _require_no_hot_page_staging(snapshot)

        unreadable = next(item for item in mutations if item.name == "unreadable")
        _replace_hot_page_profile(
            config, snapshot, profile, valid_profile, unreadable, runner
        )
        off = runner([*common, "--snapshot-prefetch", "off"], timeout=config.timeout)
        _require_result(off, 0)
        if "snapshot-hot-pages:" in off.text:
            raise ScriptError("hot-page off restore unexpectedly opened the sidecar")
    finally:
        if profile.is_dir():
            shutil.rmtree(profile)
        profile.write_bytes(valid_profile)

    injected = (
        (
            "full",
            "prefetch-error",
            {
                "population": "eager",
                "prefetch": "failed",
                "prefetch_reason": "prefetch-error",
                "populate": "ok",
                "populate_reason": "none",
            },
        ),
        (
            "required",
            "populate-error",
            {
                "population": "background",
                "prefetch": "skipped",
                "prefetch_reason": "none",
                "populate": "failed",
                "populate_reason": "populate-error",
            },
        ),
        (
            "required",
            "api-unavailable",
            {
                "population": "background",
                "prefetch": "skipped",
                "prefetch_reason": "none",
                "populate": "failed",
                "populate_reason": "api-unavailable",
            },
        ),
    )
    for mode, failure, expected in injected:
        label = f"hot-page injected {failure} restore"
        result = runner(
            [
                *common,
                "--snapshot-prefetch",
                mode,
                "--test-hot-page-failure",
                failure,
            ],
            timeout=config.timeout,
        )
        _require_result(result, 0)
        fields = require_hot_page_diagnostic(
            result, label, {"status": "active", **expected}
        )
        for key in ("selected_pages", "ranges"):
            require_positive_diagnostic_count(fields, key, label)

    panic_result = runner(
        [
            *common,
            "--snapshot-prefetch",
            "auto",
            "--test-hot-page-failure",
            "population-panic",
        ],
        timeout=config.timeout,
    )
    _require_result(panic_result, 1, required_error=HOST_ERROR_PREFIX)
    replay = runner(
        [*common, "--snapshot-prefetch", "required"], timeout=config.timeout
    )
    _require_result(replay, 0)
    fields = require_hot_page_diagnostic(
        replay,
        "hot-page replay after population panic",
        {
            "status": "active",
            "population": "background",
            "prefetch": "skipped",
            "prefetch_reason": "none",
            "populate": "ok",
            "populate_reason": "none",
        },
    )
    for key in ("selected_pages", "ranges"):
        require_positive_diagnostic_count(
            fields, key, "hot-page replay after population panic"
        )

    overlap_marker = "snapshot-hot-pages-test: population=running"
    overlap = runner(
        [
            *common,
            "--snapshot-prefetch",
            "auto",
            "--test-hot-page-failure",
            "population-wait",
        ],
        timeout=0,
        marker_timeout=config.timeout,
        graceful_timeout=True,
        interrupt_marker=overlap_marker,
    )
    if not overlap.timed_out or not overlap.interrupted or overlap.killed:
        raise ScriptError(
            "hot-page overlap worker did not stop through coordinated interruption"
        )
    if overlap.returncode != 0:
        raise ScriptError(
            f"hot-page overlap interruption exited {overlap.returncode}\n{overlap.text}"
        )
    for marker in (overlap_marker, "host interrupt received - stopping guest"):
        if marker not in overlap.text:
            raise ScriptError(f"hot-page overlap interruption missed {marker!r}")
    fields = require_hot_page_diagnostic(
        overlap,
        "hot-page population overlap interruption",
        {
            "status": "active",
            "population": "background",
            "prefetch": "skipped",
            "prefetch_reason": "none",
            "populate": "failed",
            "populate_reason": "populate-error",
        },
    )
    for key in ("selected_pages", "ranges"):
        require_positive_diagnostic_count(
            fields, key, "hot-page population overlap interruption"
        )
    replay = runner(
        [*common, "--snapshot-prefetch", "required"], timeout=config.timeout
    )
    _require_result(replay, 0)
    require_hot_page_diagnostic(
        replay,
        "hot-page replay after overlap interruption",
        {
            "status": "active",
            "population": "background",
            "prefetch": "skipped",
            "prefetch_reason": "none",
            "populate": "ok",
            "populate_reason": "none",
        },
    )


def _test_hot_page_training_failures(
    config: HotPagesTestConfig,
    snapshot: Path,
    common: list[str | Path],
    profile: Path,
    snapshot_identity: str,
    runner: Runner,
) -> None:
    failures = (
        "baseline-query-error",
        "sample-query-error",
        "no-observed-pages",
        "publish-create-error",
        "publish-write-error",
        "publish-sync-error",
        "publish-replace-error",
    )
    expected_profile = profile.read_bytes()
    for failure in failures:
        label = f"hot-page injected {failure} training"
        result = runner(
            [
                *common,
                "--snapshot-prefetch",
                "off",
                "--snapshot-profile-generate",
                "--test-hot-page-failure",
                failure,
            ],
            timeout=config.timeout,
        )
        _require_result(result, 1, required_error=HOST_ERROR_PREFIX)
        if "snapshot-hot-pages: status=trained" in result.text:
            raise ScriptError(f"{label} reported successful training")
        if profile.read_bytes() != expected_profile:
            raise ScriptError(f"{label} modified the previous valid profile")
        _require_no_hot_page_staging(snapshot)
        inspection = inspect_hot_page_profile(
            config.microvm, snapshot, timeout=config.timeout, runner=runner
        )
        require_profile_inspection(
            inspection,
            label,
            training_runs=config.training_runs,
            ram_size=config.mem << 20,
            snapshot_identity=snapshot_identity,
        )


def test_hot_pages(
    config: HotPagesTestConfig,
    backend: HostBackend,
    *,
    runner: Runner = run_capture,
) -> None:
    if backend.name != "windows-whp":
        raise ScriptError("hot-page smoke testing requires Windows/WHP")
    if config.training_runs < 1:
        raise ScriptError("hot-page training runs must be positive")
    require_file(config.microvm, "build the release VMM first: cargo build --release")
    require_file(config.kernel, "hot-page smoke test kernel is missing")
    require_file(config.initrd, "hot-page smoke test initramfs is missing")

    with tempfile.TemporaryDirectory(prefix="nvx-hot-pages-") as temporary:
        root = Path(temporary)
        snapshot = root / "snapshot"
        capture = runner(
            [
                config.microvm,
                "--kernel",
                config.kernel,
                "--initrd",
                config.initrd,
                "--mem",
                str(config.mem),
                "--cmdline",
                f"{DEFAULT_CMDLINE} shellsnap",
                "--snapshot",
                snapshot,
                "--quiet",
            ],
            timeout=config.timeout,
        )
        _require_result(capture, 0)
        members = ("state.bin", "mem.bin")
        for member in members:
            require_file(snapshot / member, f"hot-page snapshot missed {member}")
        member_hashes = {member: _sha256_file(snapshot / member) for member in members}
        profile = snapshot / "hot-pages.whp.v1"
        profile.unlink(missing_ok=True)

        common: list[str | Path] = [
            config.microvm,
            "--restore",
            snapshot,
            "--mem",
            str(config.mem),
            "--exit-on-boot",
            "--boot-marker",
            BOOT_MARKER,
            "--quiet",
            "--log-level",
            "info",
        ]
        for run in range(1, config.training_runs + 1):
            result = runner(
                [
                    *common,
                    "--snapshot-prefetch",
                    "off",
                    "--snapshot-profile-generate",
                ],
                timeout=config.timeout,
            )
            label = f"hot-page native training run {run}"
            _require_result(result, 0)
            require_file(profile, f"{label} did not publish its sidecar")
            fields = require_hot_page_diagnostic(
                result,
                label,
                {
                    "status": "trained",
                    "training_runs": str(run),
                    "identity": member_hashes["state.bin"][:16],
                },
            )
            for key in ("observed_pages", "selected_pages", "ranges"):
                require_positive_diagnostic_count(fields, key, label)
            inspection = inspect_hot_page_profile(
                config.microvm, snapshot, timeout=config.timeout, runner=runner
            )
            require_profile_inspection(
                inspection,
                label,
                training_runs=run,
                ram_size=config.mem << 20,
                snapshot_identity=member_hashes["state.bin"],
            )
            _require_snapshot_unchanged(snapshot, member_hashes)
            _require_no_hot_page_staging(snapshot)

        modes = {
            "off": None,
            "auto": ("background", "skipped"),
            "required": ("background", "skipped"),
            "eager": ("eager", "skipped"),
            "full": ("eager", "ok"),
        }
        positive_failure: Exception | None = None
        for mode, expected in modes.items():
            try:
                result = runner(
                    [*common, "--snapshot-prefetch", mode], timeout=config.timeout
                )
                label = f"hot-page {mode} restore"
                _require_result(result, 0)
                if expected is None:
                    if "snapshot-hot-pages:" in result.text:
                        raise ScriptError(f"{label} unexpectedly consumed the sidecar")
                else:
                    population, prefetch = expected
                    fields = require_hot_page_diagnostic(
                        result,
                        label,
                        {
                            "status": "active",
                            "population": population,
                            "prefetch": prefetch,
                            "prefetch_reason": "none",
                            "populate": "ok",
                            "populate_reason": "none",
                        },
                    )
                    for key in ("selected_pages", "ranges"):
                        require_positive_diagnostic_count(fields, key, label)
                _require_snapshot_unchanged(snapshot, member_hashes)
            except Exception as error:
                if positive_failure is None:
                    positive_failure = error

        if runner is run_capture:
            _test_hot_page_fallbacks(config, snapshot, common, profile, runner)
            _test_hot_page_training_failures(
                config,
                snapshot,
                common,
                profile,
                member_hashes["state.bin"],
                runner,
            )
            _require_snapshot_unchanged(snapshot, member_hashes)

        released = root / "snapshot-released"
        snapshot.rename(released)
        released.rename(snapshot)

        if positive_failure is not None:
            raise positive_failure

    print("PASS: WHP hot-page training, population, fallbacks, failures, and cleanup")


def _test_windows_exec_extensions(
    config: ExecTestConfig,
    mount_root: Path,
    snapshot: Path,
    script_path: Path,
    runner: Runner,
) -> None:
    generation_one_marker = "NVX-WHP-SNAPSHOT-GENERATION-1"
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
        f"echo {generation_one_marker}\nexit 40\n",
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
    _require_result(restore, 40, required_output=generation_one_marker)

    state_path = snapshot / "state.bin"
    memory_path = snapshot / "mem.bin"
    if state_path.is_file() and memory_path.is_file():
        repeated = runner(
            _whp_restore_args(config, snapshot, mount_root),
            timeout=config.timeout,
        )
        _require_result(repeated, 40, required_output=generation_one_marker)

        profile_path = snapshot / "hot-pages.whp.v1"
        profile_path.write_bytes(b"stale profile from generation one")
        script_path.write_text(
            "echo NVX-WHP-SNAPSHOT-RECAPTURE\nexit 0\n",
            encoding="utf-8",
            newline="\n",
        )
        recapture = runner(
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
        _require_result(recapture, 0)
        if profile_path.exists():
            raise ScriptError("WHP recapture retained a stale hot-page profile")
        _require_no_snapshot_staging(snapshot)

        script_path.write_text(
            "echo NVX-WHP-SNAPSHOT-RESTORED\nexit 41\n",
            encoding="utf-8",
            newline="\n",
        )
        for _ in range(2):
            restored = runner(
                _whp_restore_args(config, snapshot, mount_root),
                timeout=config.timeout,
            )
            _require_result(restored, 41, required_output="NVX-WHP-SNAPSHOT-RESTORED")

        _test_whp_snapshot_corruption(
            config,
            mount_root,
            snapshot,
            work_root=snapshot.parent,
            runner=runner,
        )

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


def _whp_restore_args(
    config: ExecTestConfig,
    snapshot: Path,
    mount_root: Path,
    *,
    restore_ready_pipe: str | None = None,
) -> list[str | Path]:
    args: list[str | Path] = [
        config.microvm,
        "--restore",
        snapshot,
        "--mount",
        mount_root,
        "--output-after-marker",
        "NVX-EXEC-START",
        "--log-level",
        "off",
    ]
    if restore_ready_pipe is not None:
        args.extend(["--restore-ready-pipe", restore_ready_pipe])
    return args


def _test_whp_snapshot_corruption(
    config: ExecTestConfig,
    mount_root: Path,
    snapshot: Path,
    *,
    work_root: Path,
    runner: Runner,
) -> None:
    canonical_state = (snapshot / "state.bin").read_bytes()
    mutations = _whp_snapshot_mutations(canonical_state)
    malformed = work_root / "snapshot-malformed"
    if malformed.exists():
        shutil.rmtree(malformed)
    malformed.mkdir()
    try:
        os.link(snapshot / "mem.bin", malformed / "mem.bin")
        malformed_state = malformed / "state.bin"
        for mutation in mutations:
            malformed_state.write_bytes(mutation.state)
            observer = (
                _WindowsNamedPipeObserver()
                if os.name == "nt" and runner is run_capture
                else None
            )
            try:
                result = runner(
                    _whp_restore_args(
                        config,
                        malformed,
                        mount_root,
                        restore_ready_pipe=observer.path
                        if observer is not None
                        else None,
                    ),
                    timeout=config.timeout,
                )
            finally:
                readiness = observer.finish() if observer is not None else b""
            _require_result(result, 1, required_error=HOST_ERROR_PREFIX)
            if mutation.diagnostic not in result.text:
                raise ScriptError(
                    f"malformed WHP snapshot {mutation.name!r} missed diagnostic "
                    f"{mutation.diagnostic!r}\n{result.text}"
                )
            if "NVX-WHP-SNAPSHOT-RESTORED" in result.text:
                raise ScriptError(
                    f"malformed WHP snapshot {mutation.name!r} executed the guest"
                )
            if readiness:
                raise ScriptError(
                    f"malformed WHP snapshot {mutation.name!r} wrote restore-readiness data: "
                    f"{readiness!r}"
                )

            valid = runner(
                _whp_restore_args(config, snapshot, mount_root),
                timeout=config.timeout,
            )
            _require_result(
                valid,
                41,
                required_output="NVX-WHP-SNAPSHOT-RESTORED",
            )
            print(
                f"rejected malformed WHP snapshot and replayed valid state: {mutation.name}"
            )
    finally:
        shutil.rmtree(malformed, ignore_errors=True)


def _kvm_restore_args(
    config: ExecTestConfig, snapshot: Path, mount_root: Path
) -> list[str | Path]:
    return [
        config.microvm,
        "--restore",
        snapshot,
        "--mount",
        mount_root,
        "--output-after-marker",
        "NVX-KVM-RESUME-START",
        "--log-level",
        "off",
    ]


def _test_kvm_restore_input_rejection(
    config: ExecTestConfig,
    mount_root: Path,
    snapshot: Path,
    script_path: Path,
    runner: Runner,
) -> None:
    """Requires malformed restore inputs to fail as host errors before any VM exists."""
    generation_one_marker = "NVX-KVM-SNAPSHOT-GENERATION-1"
    restored_marker = "NVX-KVM-SNAPSHOT-RESTORED"

    def capture_generation(original_marker: str, resume_marker: str) -> None:
        # Everything after the request runs only on a resumed guest, so the resume marker gates
        # restore output and makes that generation's exit status observable.
        script_path.write_text(
            f"echo {original_marker}\n"
            "/sbin/nvx-snapshot\n"
            "echo NVX-KVM-RESUME-START\n"
            f"echo {resume_marker}\n"
            "/sbin/nvx-exit 41\n"
            "exit 99\n",
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
                "--log-level",
                "off",
            ],
            timeout=config.timeout,
        )
        _require_result(capture, 0, required_output=original_marker)

    capture_generation("NVX-KVM-SNAPSHOT-ORIGINAL", generation_one_marker)
    memory_path = snapshot / "mem.bin"
    state_path = snapshot / "state.bin"
    require_file(memory_path, "snapshot capture did not write mem.bin")
    require_file(state_path, "snapshot capture did not write state.bin")

    def replay_valid_state(marker: str) -> None:
        _require_result(
            runner(
                _kvm_restore_args(config, snapshot, mount_root), timeout=config.timeout
            ),
            41,
            required_output=marker,
        )

    replay_valid_state(generation_one_marker)
    if not (memory_path.is_file() and state_path.is_file()):
        return
    replay_valid_state(generation_one_marker)

    capture_generation("NVX-KVM-SNAPSHOT-RECAPTURE", restored_marker)
    _require_no_snapshot_staging(snapshot)
    replay_valid_state(restored_marker)
    replay_valid_state(restored_marker)

    malformed = snapshot.parent / "snapshot-malformed"
    try:
        for mutation in _restore_input_mutations(memory_path.stat().st_size):
            _plant_restore_input(snapshot, malformed, mutation)
            result = runner(
                _kvm_restore_args(config, malformed, mount_root), timeout=config.timeout
            )
            if result.returncode is not None and result.returncode < 0:
                raise ScriptError(
                    f"malformed restore input {mutation.name!r} killed NVX with signal "
                    f"{-result.returncode}\n{result.text}"
                )
            _require_result(result, 1, required_error=HOST_ERROR_PREFIX)
            if mutation.diagnostic not in result.text:
                raise ScriptError(
                    f"malformed restore input {mutation.name!r} missed diagnostic "
                    f"{mutation.diagnostic!r}\n{result.text}"
                )
            if restored_marker in result.text:
                raise ScriptError(
                    f"malformed restore input {mutation.name!r} executed the guest"
                )
            replay_valid_state(restored_marker)
            print(
                f"rejected malformed restore input and replayed valid state: {mutation.name}"
            )
    finally:
        shutil.rmtree(malformed, ignore_errors=True)


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


def _host_profile_is_usable(
    work_root: Path,
    backend_name: str,
    *,
    runner: Runner = run_capture,
    request_windows: bool = False,
) -> bool:
    if backend_name == "linux-kvm":
        return _perf_record_is_usable(work_root, runner=runner)
    if backend_name != "windows-whp" or not request_windows:
        return False
    if shutil.which("wpr") is None:
        print("::notice::WPR unavailable; Windows host-trace validation skipped")
        return False
    if _find_xperf() is None:
        print("::notice::xperf unavailable; Windows host-trace validation skipped")
        return False
    return True


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


def _require_wpr_inactive(instance: str, *, runner: Runner) -> None:
    status = runner(
        ["wpr", "-status", "-instancename", instance],
        timeout=15,
    )
    if _wpr_reports_inactive(status):
        return
    cancel = runner(
        ["wpr", "-cancel", "-instancename", instance],
        timeout=15,
    )
    after_cancel = runner(
        ["wpr", "-status", "-instancename", instance],
        timeout=15,
    )
    inactive = _wpr_reports_inactive(after_cancel)
    cleanup = (
        "cleanup succeeded"
        if inactive
        else (
            f"cleanup exit {cancel.returncode}; post-cleanup status: {after_cancel.text.strip()}"
        )
    )
    raise ScriptError(
        f"WPR instance {instance} remained active after profiling ({cleanup})\n{status.text}"
    )


def _wpr_reports_inactive(result: CommandResult) -> bool:
    diagnostics = result.text.lower()
    return "not recording" in diagnostics or "no trace profiles running" in diagnostics


def _check_windows_host_profile_cleanup(work_root: Path, *, runner: Runner) -> None:
    errors: list[str] = []
    instance = os.environ.get("NVX_WPR_INSTANCE", "nvxprofile").strip() or "nvxprofile"
    try:
        _require_wpr_inactive(instance, runner=runner)
    except ScriptError as error:
        errors.append(str(error))
    leftovers = sorted(
        path.name
        for path in work_root.iterdir()
        if ".partial" in path.name or path.name.startswith("nvx-wpr-")
    )
    if leftovers:
        errors.append(
            f"host profiling leaked temporary artifacts: {', '.join(leftovers)}"
        )
    if errors:
        raise ScriptError("\n".join(errors))


def _validate_windows_host_trace_contents(
    folded: Path,
    work_root: Path,
    result: CommandResult,
    *,
    require_scheduling_events: bool,
) -> int:
    if result.pid is None:
        raise ScriptError("profiling runner did not report the launched NVX process ID")
    expected_run = _read_run_id(folded)
    if expected_run is None:
        raise ScriptError(
            f"guest profile run-id sidecar is missing or invalid: {folded}.run"
        )

    extraction_dir = work_root / "host-extract"
    extraction_dir.mkdir()
    diagnostics: dict[str, object] = {}
    host_folded = extract_host_stacks(
        folded,
        extraction_dir,
        diag=diagnostics,
        expected_run=expected_run,
        expected_pid=result.pid,
    )
    if host_folded is None:
        stderr_tail = diagnostic_tail(
            result.stderr.decode("utf-8", errors="replace"), lines=30
        )
        stdout_tail = diagnostic_tail(
            result.stdout.decode("utf-8", errors="replace"), lines=10
        )
        process_diagnostics = "\n".join(
            section for section in (stderr_tail, stdout_tail) if section
        )
        suffix = f"\n{process_diagnostics}" if process_diagnostics else ""
        raise ScriptError(
            f"xperf did not validate a fresh ETL for the launched NVX process{suffix}"
        )
    if diagnostics.get("lost"):
        raise ScriptError("xperf reported lost events in the host trace")
    if diagnostics.get("pid") != result.pid:
        raise ScriptError("xperf validation did not retain exact NVX PID provenance")
    samples = diagnostics.get("samples")
    if not isinstance(samples, int) or samples <= 0:
        raise ScriptError("host ETL contains no sampled-profile events for NVX")
    context_switches = diagnostics.get("context_switches")
    if require_scheduling_events and (
        not isinstance(context_switches, int) or context_switches <= 0
    ):
        raise ScriptError("host ETL contains no scheduling events for NVX")
    return samples


def test_profiling(
    config: ProfilingTestConfig,
    backend: HostBackend | None = None,
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
        backend_name = (
            backend.name
            if backend is not None
            else "windows-whp"
            if os.name == "nt"
            else "linux-kvm"
        )
        if backend_name != "windows-whp":
            if config.wpr_profile is not None:
                raise ScriptError("--wpr-profile is only supported on windows-whp")
            if config.require_scheduling_events:
                raise ScriptError(
                    "--require-scheduling-events is only supported on windows-whp"
                )
        host_required = config.require_host_profile or config.require_scheduling_events
        if config.wpr_profile is not None and not host_required:
            raise ScriptError("--wpr-profile requires --require-host-profile")
        host_capable = _host_profile_is_usable(
            work_root,
            backend_name,
            runner=runner,
            request_windows=host_required,
        )
        if host_required and not host_capable:
            raise ScriptError(
                f"required host profiling is unavailable on {backend_name}"
            )
        if host_capable:
            print(
                f">> host profiling is usable on {backend_name}; enabling host tracing"
            )
        else:
            print(
                "::notice::host profiling unavailable/unpermitted; "
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
            "--defer-stdin-until-boot",
        ]
        if host_capable:
            args.append("--host-profile")
            if backend_name == "windows-whp" and config.wpr_profile is not None:
                args.extend(["--wpr-profile", config.wpr_profile])
        run_error: Exception | None = None
        try:
            result = runner(
                args,
                input_text="cat /etc/alpine-release\nreboot -f\n",
                timeout=config.timeout,
            )
            if BOOT_MARKER not in result.text:
                raise ScriptError(
                    f"guest did not reach userspace ({BOOT_MARKER})\n{result.text}"
                )
            if result.timed_out or result.returncode != 0:
                raise ScriptError(
                    f"profiling VM did not exit successfully (exit={result.returncode}, "
                    f"timed_out={result.timed_out})\n{result.text}"
                )

            samples = _folded_sample_count(folded)
            print(f"PASS: guest profile has {samples} sample(s)")
            if host_capable and backend_name == "linux-kvm":
                host_trace = work_root / "smoke.host.perf.data"
                if not host_trace.is_file():
                    raise ScriptError(f"host trace was not published: {host_trace}")
                if host_trace.read_bytes()[:8] != b"PERFILE2":
                    raise ScriptError(
                        "published host trace is not a finalized perf.data "
                        "(missing PERFILE2 header)"
                    )
                print("PASS: host trace published with valid PERFILE2 header")
            elif host_capable and backend_name == "windows-whp":
                samples = _validate_windows_host_trace_contents(
                    folded,
                    work_root,
                    result,
                    require_scheduling_events=config.require_scheduling_events,
                )
                print(
                    f"PASS: host ETL contains {samples} sampled event(s) for PID {result.pid}"
                )
        except Exception as error:
            run_error = error

        cleanup_error: Exception | None = None
        if host_capable and backend_name == "windows-whp":
            try:
                _check_windows_host_profile_cleanup(work_root, runner=runner)
            except Exception as error:
                cleanup_error = error
        if run_error is not None:
            if cleanup_error is not None:
                raise ScriptError(
                    f"{run_error}\nhost-profile cleanup also failed: {cleanup_error}"
                ) from run_error
            raise run_error
        if cleanup_error is not None:
            raise cleanup_error
    print("PASS: profiling smoke test")
