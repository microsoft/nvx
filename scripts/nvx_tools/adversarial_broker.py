"""Typed action catalog and broker for Copilot-driven adversarial campaigns."""

from __future__ import annotations

import base64
import binascii
import hashlib
import json
import os
import re
from collections.abc import Callable, Iterable, Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import cast

from nvx_tools.common import ScriptError

SCHEMA_VERSION = 1
CAMPAIGNS = (
    "workload-isolation",
    "guest-isolation",
    "snapshot-isolation",
)
CASE_DIRECTORY = Path(__file__).with_name("adversarial_cases")
MAX_ACTION_BYTES = 4096
MAX_RESULT_OUTPUT_BYTES = 8192
MAX_REPLAY_ACTIONS = 100

_IDENTIFIER = re.compile(r"[a-z][a-z0-9-]{0,63}\Z")
_SAFE_SCENARIOS = frozenset(
    {
        "console-exit",
        "console-snapshot",
        "directional-network-policy",
        "denied-filesystem-paths",
        "endpoint-policy-snapshot",
        "filesystem-snapshot",
        "host-loopback-policy",
        "l3-l4-egress-policy",
        "lifecycle",
        "managed-lifecycle",
        "network-snapshot",
        "restore-memory",
        "restore-processors",
        "sandbox-blocks",
        "scratch-snapshot",
        "smp-lapic",
        "smp-snapshot",
        "snapshot-core",
        "snapshot-tiers",
        "structured-outcome",
        "virtio-net",
        "workload-identity",
    }
)
_RESULT_CATEGORIES = frozenset(
    {
        "passed",
        "test-failure",
        "timeout",
        "executor-error",
        "policy-violation",
        "availability-failure",
    }
)


def _strict_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise ScriptError(f"duplicate JSON property: {key}")
        result[key] = value
    return result


def _reject_json_constant(value: str) -> object:
    raise ScriptError(f"non-standard JSON constant is not allowed: {value}")


def parse_json_object(text: str, *, description: str) -> dict[str, object]:
    try:
        value = cast(
            object,
            json.loads(
                text,
                object_pairs_hook=_strict_object,
                parse_constant=_reject_json_constant,
            ),
        )
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise ScriptError(f"{description} is not valid JSON: {error}") from error
    if not isinstance(value, dict):
        raise ScriptError(f"{description} must be one JSON object")
    return cast(dict[str, object], value)


def _require_exact_keys(
    value: Mapping[str, object],
    expected: set[str],
    *,
    description: str,
) -> None:
    actual = set(value)
    if actual != expected:
        missing = sorted(expected - actual)
        extra = sorted(actual - expected)
        details: list[str] = []
        if missing:
            details.append(f"missing {', '.join(missing)}")
        if extra:
            details.append(f"unexpected {', '.join(extra)}")
        raise ScriptError(
            f"{description} has invalid properties ({'; '.join(details)})"
        )


def _require_identifier(value: object, *, description: str) -> str:
    if not isinstance(value, str) or _IDENTIFIER.fullmatch(value) is None:
        raise ScriptError(f"{description} must be a lowercase kebab-case identifier")
    return value


def _require_int(
    value: object,
    *,
    description: str,
    minimum: int,
    maximum: int,
) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ScriptError(f"{description} must be an integer")
    if value < minimum or value > maximum:
        raise ScriptError(
            f"{description} must be between {minimum} and {maximum}, got {value}"
        )
    return value


def _require_bool(value: object, *, description: str) -> bool:
    if not isinstance(value, bool):
        raise ScriptError(f"{description} must be a boolean")
    return value


def _require_optional_int(value: object, *, description: str) -> int | None:
    if value is None:
        return None
    return _require_int(
        value,
        description=description,
        minimum=-2_147_483_648,
        maximum=2_147_483_647,
    )


def _require_string(value: object, *, description: str, maximum: int = 4096) -> str:
    if not isinstance(value, str):
        raise ScriptError(f"{description} must be a string")
    if len(value) > maximum:
        raise ScriptError(f"{description} exceeds {maximum} characters")
    return value


def _decode_bounded_base64(value: object, *, description: str) -> str:
    encoded = _require_string(
        value,
        description=description,
        maximum=((MAX_RESULT_OUTPUT_BYTES + 2) // 3) * 4,
    )
    try:
        decoded = base64.b64decode(encoded, validate=True)
    except (binascii.Error, ValueError) as error:
        raise ScriptError(f"{description} is not valid base64") from error
    if len(decoded) > MAX_RESULT_OUTPUT_BYTES:
        raise ScriptError(f"{description} exceeds the decoded output limit")
    return encoded


@dataclass(frozen=True, slots=True)
class CaseDefinition:
    """One trusted, deterministic primitive exposed to the strategist."""

    case_id: str
    campaign: str
    scenario: str
    description: str
    coverage: tuple[str, ...]
    repetitions: int
    timeout_multiplier: int

    @classmethod
    def from_object(
        cls,
        value: object,
        *,
        campaign: str,
        index: int,
    ) -> CaseDefinition:
        if not isinstance(value, dict):
            raise ScriptError(f"campaign case {index} must be an object")
        case = cast(dict[str, object], value)
        _require_exact_keys(
            case,
            {
                "id",
                "scenario",
                "description",
                "coverage",
                "repetitions",
                "timeout_multiplier",
            },
            description=f"campaign case {index}",
        )
        case_id = _require_identifier(
            case["id"],
            description=f"campaign case {index} id",
        )
        scenario = _require_identifier(
            case["scenario"],
            description=f"campaign case {case_id} scenario",
        )
        if scenario not in _SAFE_SCENARIOS:
            raise ScriptError(
                f"campaign case {case_id} selects unsupported scenario {scenario}"
            )
        description = _require_string(
            case["description"],
            description=f"campaign case {case_id} description",
            maximum=512,
        )
        raw_coverage = case["coverage"]
        if not isinstance(raw_coverage, list) or not raw_coverage:
            raise ScriptError(
                f"campaign case {case_id} coverage must be a nonempty list"
            )
        coverage = tuple(
            _require_identifier(
                item,
                description=f"campaign case {case_id} coverage entry",
            )
            for item in cast(list[object], raw_coverage)
        )
        if len(set(coverage)) != len(coverage):
            raise ScriptError(f"campaign case {case_id} has duplicate coverage entries")
        return cls(
            case_id=case_id,
            campaign=campaign,
            scenario=scenario,
            description=description,
            coverage=coverage,
            repetitions=_require_int(
                case["repetitions"],
                description=f"campaign case {case_id} repetitions",
                minimum=1,
                maximum=4,
            ),
            timeout_multiplier=_require_int(
                case["timeout_multiplier"],
                description=f"campaign case {case_id} timeout multiplier",
                minimum=1,
                maximum=8,
            ),
        )

    def prompt_object(self) -> dict[str, object]:
        return {
            "case_id": self.case_id,
            "description": self.description,
            "coverage": list(self.coverage),
            "repetitions": self.repetitions,
        }


def campaign_file(campaign: str) -> Path:
    if campaign not in CAMPAIGNS:
        raise ScriptError(f"unsupported adversarial campaign: {campaign}")
    return CASE_DIRECTORY / f"{campaign}.json"


def load_campaign(campaign: str) -> dict[str, CaseDefinition]:
    path = campaign_file(campaign)
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise ScriptError(
            f"cannot read adversarial campaign {path}: {error}"
        ) from error
    document = parse_json_object(text, description=f"campaign {campaign}")
    _require_exact_keys(
        document,
        {"schema_version", "campaign", "description", "cases"},
        description=f"campaign {campaign}",
    )
    _require_int(
        document["schema_version"],
        description=f"campaign {campaign} schema_version",
        minimum=SCHEMA_VERSION,
        maximum=SCHEMA_VERSION,
    )
    if document["campaign"] != campaign:
        raise ScriptError(f"campaign file {path} names {document['campaign']!r}")
    _require_string(
        document["description"],
        description=f"campaign {campaign} description",
        maximum=1024,
    )
    raw_cases = document["cases"]
    if not isinstance(raw_cases, list) or not raw_cases:
        raise ScriptError(f"campaign {campaign} must contain at least one case")
    result: dict[str, CaseDefinition] = {}
    for index, raw_case in enumerate(cast(list[object], raw_cases), start=1):
        case = CaseDefinition.from_object(raw_case, campaign=campaign, index=index)
        if case.case_id in result:
            raise ScriptError(
                f"campaign {campaign} contains duplicate case {case.case_id}"
            )
        result[case.case_id] = case
    return result


def catalog_digest(cases: Iterable[CaseDefinition]) -> str:
    payload = [
        {
            "case_id": case.case_id,
            "campaign": case.campaign,
            "scenario": case.scenario,
            "description": case.description,
            "coverage": list(case.coverage),
            "repetitions": case.repetitions,
            "timeout_multiplier": case.timeout_multiplier,
        }
        for case in sorted(cases, key=lambda item: item.case_id)
    ]
    encoded = json.dumps(
        payload,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


@dataclass(frozen=True, slots=True)
class AdversarialAction:
    """The complete action surface accepted from Copilot or a replay."""

    case_id: str

    @classmethod
    def from_object(
        cls,
        value: Mapping[str, object],
        *,
        cases: Mapping[str, CaseDefinition],
        completed: set[str],
        description: str,
    ) -> AdversarialAction:
        _require_exact_keys(
            value,
            {"schema_version", "case_id"},
            description=description,
        )
        _require_int(
            value["schema_version"],
            description=f"{description} schema_version",
            minimum=SCHEMA_VERSION,
            maximum=SCHEMA_VERSION,
        )
        case_id = _require_identifier(
            value["case_id"],
            description=f"{description} case_id",
        )
        if case_id not in cases:
            raise ScriptError(f"{description} selects unknown case {case_id}")
        if case_id in completed:
            raise ScriptError(f"{description} repeats completed case {case_id}")
        return cls(case_id=case_id)

    @classmethod
    def from_response(
        cls,
        response: bytes,
        *,
        cases: Mapping[str, CaseDefinition],
        completed: set[str],
    ) -> AdversarialAction:
        if len(response) > MAX_ACTION_BYTES:
            raise ScriptError("Copilot response exceeds the action size limit")
        try:
            text = response.decode("utf-8")
        except UnicodeDecodeError as error:
            raise ScriptError("Copilot response is not UTF-8") from error
        document = parse_json_object(text, description="Copilot response")
        return cls.from_object(
            document,
            cases=cases,
            completed=completed,
            description="Copilot response",
        )

    def as_object(self) -> dict[str, object]:
        return {
            "schema_version": SCHEMA_VERSION,
            "case_id": self.case_id,
        }


@dataclass(frozen=True, slots=True)
class ActionResult:
    """Validated executor observation, with guest text kept encoded."""

    sequence: int
    case_id: str
    category: str
    returncode: int | None
    timed_out: bool
    duration_ms: int
    stdout_b64: str
    stderr_b64: str
    stdout_truncated: bool
    stderr_truncated: bool
    host_canary_intact: bool
    network_canary_connections: int
    teardown_complete: bool
    artifact_path: str
    error: str | None

    @classmethod
    def from_object(
        cls,
        value: Mapping[str, object],
        *,
        expected_sequence: int,
        expected_case: str,
    ) -> ActionResult:
        _require_exact_keys(
            value,
            {
                "schema_version",
                "operation",
                "sequence",
                "case_id",
                "category",
                "returncode",
                "timed_out",
                "duration_ms",
                "stdout_b64",
                "stderr_b64",
                "stdout_truncated",
                "stderr_truncated",
                "host_canary_intact",
                "network_canary_connections",
                "teardown_complete",
                "artifact_path",
                "error",
            },
            description="executor result",
        )
        _require_int(
            value["schema_version"],
            description="executor result schema_version",
            minimum=SCHEMA_VERSION,
            maximum=SCHEMA_VERSION,
        )
        if value["operation"] != "execute":
            raise ScriptError("executor result has an invalid operation")
        sequence = _require_int(
            value["sequence"],
            description="executor result sequence",
            minimum=1,
            maximum=MAX_REPLAY_ACTIONS,
        )
        if sequence != expected_sequence:
            raise ScriptError(
                f"executor result sequence is {sequence}, expected {expected_sequence}"
            )
        case_id = _require_identifier(
            value["case_id"],
            description="executor result case_id",
        )
        if case_id != expected_case:
            raise ScriptError(
                f"executor result names case {case_id}, expected {expected_case}"
            )
        category = _require_string(
            value["category"],
            description="executor result category",
            maximum=32,
        )
        if category not in _RESULT_CATEGORIES:
            raise ScriptError(f"executor result has unknown category {category}")
        returncode = _require_optional_int(
            value["returncode"],
            description="executor result returncode",
        )
        timed_out = _require_bool(
            value["timed_out"],
            description="executor result timed_out",
        )
        host_canary_intact = _require_bool(
            value["host_canary_intact"],
            description="executor result host_canary_intact",
        )
        network_canary_connections = _require_int(
            value["network_canary_connections"],
            description="executor result network_canary_connections",
            minimum=0,
            maximum=1_000_000,
        )
        teardown_complete = _require_bool(
            value["teardown_complete"],
            description="executor result teardown_complete",
        )
        if category == "passed" and (
            returncode != 0
            or timed_out
            or not host_canary_intact
            or network_canary_connections != 0
            or not teardown_complete
        ):
            raise ScriptError("executor result category contradicts its outcome fields")
        error_value = value["error"]
        error = (
            None
            if error_value is None
            else _require_string(
                error_value,
                description="executor result error",
                maximum=4096,
            )
        )
        return cls(
            sequence=sequence,
            case_id=case_id,
            category=category,
            returncode=returncode,
            timed_out=timed_out,
            duration_ms=_require_int(
                value["duration_ms"],
                description="executor result duration_ms",
                minimum=0,
                maximum=86_400_000,
            ),
            stdout_b64=_decode_bounded_base64(
                value["stdout_b64"],
                description="executor result stdout_b64",
            ),
            stderr_b64=_decode_bounded_base64(
                value["stderr_b64"],
                description="executor result stderr_b64",
            ),
            stdout_truncated=_require_bool(
                value["stdout_truncated"],
                description="executor result stdout_truncated",
            ),
            stderr_truncated=_require_bool(
                value["stderr_truncated"],
                description="executor result stderr_truncated",
            ),
            host_canary_intact=host_canary_intact,
            network_canary_connections=network_canary_connections,
            teardown_complete=teardown_complete,
            artifact_path=_require_string(
                value["artifact_path"],
                description="executor result artifact_path",
                maximum=4096,
            ),
            error=error,
        )

    @property
    def anomaly(self) -> bool:
        return (
            self.category != "passed"
            or self.returncode != 0
            or self.timed_out
            or not self.host_canary_intact
            or self.network_canary_connections != 0
            or not self.teardown_complete
        )

    @property
    def suspected_escape(self) -> bool:
        return (
            self.category == "policy-violation"
            or not self.host_canary_intact
            or self.network_canary_connections != 0
        )

    def prompt_object(self, case: CaseDefinition) -> dict[str, object]:
        return {
            "sequence": self.sequence,
            "case_id": self.case_id,
            "category": self.category,
            "duration_ms": self.duration_ms,
            "coverage": list(case.coverage),
            "returncode": self.returncode,
            "timed_out": self.timed_out,
            "host_canary_intact": self.host_canary_intact,
            "network_canary_connections": self.network_canary_connections,
            "teardown_complete": self.teardown_complete,
            "guest_output": {
                "encoding": "base64",
                "stdout": self.stdout_b64,
                "stderr": self.stderr_b64,
                "stdout_truncated": self.stdout_truncated,
                "stderr_truncated": self.stderr_truncated,
            },
        }

    def as_object(self) -> dict[str, object]:
        return {
            "schema_version": SCHEMA_VERSION,
            "sequence": self.sequence,
            "case_id": self.case_id,
            "category": self.category,
            "returncode": self.returncode,
            "timed_out": self.timed_out,
            "duration_ms": self.duration_ms,
            "stdout_b64": self.stdout_b64,
            "stderr_b64": self.stderr_b64,
            "stdout_truncated": self.stdout_truncated,
            "stderr_truncated": self.stderr_truncated,
            "host_canary_intact": self.host_canary_intact,
            "network_canary_connections": self.network_canary_connections,
            "teardown_complete": self.teardown_complete,
            "artifact_path": self.artifact_path,
            "error": self.error,
        }


ExecutorCallback = Callable[
    [CaseDefinition, int, float],
    Mapping[str, object],
]


class TypedBroker:
    """Records accepted actions before dispatching only catalogued primitives."""

    def __init__(
        self,
        *,
        cases: Mapping[str, CaseDefinition],
        actions_path: Path,
        observations_path: Path,
        execute: ExecutorCallback,
    ) -> None:
        self._cases = dict(cases)
        self._actions_path = actions_path
        self._observations_path = observations_path
        self._execute = execute
        self._completed: set[str] = set()
        self._results: list[ActionResult] = []

    @property
    def completed(self) -> set[str]:
        return set(self._completed)

    @property
    def results(self) -> tuple[ActionResult, ...]:
        return tuple(self._results)

    def execute(self, action: AdversarialAction, *, timeout: float) -> ActionResult:
        if action.case_id not in self._cases:
            raise ScriptError(f"broker rejected unknown case {action.case_id}")
        if action.case_id in self._completed:
            raise ScriptError(f"broker rejected repeated case {action.case_id}")
        sequence = len(self._results) + 1
        action_record = {
            "schema_version": SCHEMA_VERSION,
            "sequence": sequence,
            "case_id": action.case_id,
        }
        append_json_line(self._actions_path, action_record)
        raw_result = self._execute(self._cases[action.case_id], sequence, timeout)
        result = ActionResult.from_object(
            raw_result,
            expected_sequence=sequence,
            expected_case=action.case_id,
        )
        append_json_line(self._observations_path, result.as_object())
        self._completed.add(action.case_id)
        self._results.append(result)
        return result


def append_json_line(path: Path, value: Mapping[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    line = json.dumps(value, sort_keys=True, separators=(",", ":"))
    with path.open("a", encoding="utf-8", newline="\n") as stream:
        stream.write(line)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())


def write_json(path: Path, value: Mapping[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    text = json.dumps(value, indent=2, sort_keys=True) + "\n"
    try:
        temporary.write_text(text, encoding="utf-8", newline="\n")
        temporary.replace(path)
    except OSError:
        temporary.unlink(missing_ok=True)
        raise


def read_replay(
    path: Path,
    *,
    cases: Mapping[str, CaseDefinition],
) -> tuple[AdversarialAction, ...]:
    try:
        lines = path.read_bytes().splitlines()
    except OSError as error:
        raise ScriptError(f"cannot read replay actions {path}: {error}") from error
    if not lines:
        raise ScriptError("replay action file is empty")
    if len(lines) > MAX_REPLAY_ACTIONS:
        raise ScriptError(
            f"replay action file exceeds the {MAX_REPLAY_ACTIONS}-action limit"
        )
    actions: list[AdversarialAction] = []
    completed: set[str] = set()
    for sequence, line in enumerate(lines, start=1):
        if len(line) > MAX_ACTION_BYTES:
            raise ScriptError(f"replay action {sequence} exceeds the size limit")
        try:
            text = line.decode("utf-8")
        except UnicodeDecodeError as error:
            raise ScriptError(f"replay action {sequence} is not UTF-8") from error
        value = parse_json_object(text, description=f"replay action {sequence}")
        if set(value) == {"schema_version", "sequence", "case_id"}:
            recorded_sequence = _require_int(
                value.pop("sequence"),
                description=f"replay action {sequence} sequence",
                minimum=1,
                maximum=MAX_REPLAY_ACTIONS,
            )
            if recorded_sequence != sequence:
                raise ScriptError(
                    f"replay action sequence is {recorded_sequence}, expected {sequence}"
                )
        action = AdversarialAction.from_object(
            value,
            cases=cases,
            completed=completed,
            description=f"replay action {sequence}",
        )
        completed.add(action.case_id)
        actions.append(action)
    return tuple(actions)
