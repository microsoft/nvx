"""Balanced shards of a suite that CI splits across parallel jobs.

`microvm_shards` plans the shards of the default microVM suite, whose
scenarios run one after another, and passes the time that a shard takes.
"""

from __future__ import annotations

import argparse
import itertools
import math
import re
from collections import Counter
from collections.abc import Callable, Collection, Iterable, Iterator, Mapping, Sequence
from pathlib import Path
from typing import cast

from .ci import OPENVMM_TEST_BACKENDS
from .common import ScriptError

# A change must shorten the longest shard, or the next longest where the
# longest stay alike, by at least this many tenths of a second, so that the
# noise of new medians does not reshuffle a partition that is balanced already.
MINIMUM_GAIN_TENTHS = 10


def tenths(seconds: float) -> int:
    """Return a duration in whole tenths of a second, as the plans count it."""
    return round(seconds * 10)


def parse_shard(value: str) -> tuple[int, int]:
    """Parse a --shard value, INDEX/COUNT, such as 1/2 for the first of two
    shards."""
    match = re.fullmatch(r"([1-9][0-9]*)/([1-9][0-9]*)", value)
    if match is None:
        raise argparse.ArgumentTypeError("must be INDEX/COUNT, such as 1/2")
    index, count = int(match.group(1)), int(match.group(2))
    if index > count:
        raise argparse.ArgumentTypeError("INDEX must be at most COUNT")
    return index, count


def named_seconds(name: str, example: str) -> Callable[[str], tuple[str, float]]:
    """Return the parser of a NAME=SECONDS value, such as a duration that a
    planner takes as an estimate, which `example` shows."""

    def parse(value: str) -> tuple[str, float]:
        item, separator, seconds = value.partition("=")
        try:
            duration = float(seconds)
        except ValueError:
            duration = -1.0
        if not separator or not item or not 0 <= duration < math.inf:
            raise argparse.ArgumentTypeError(
                f"must be {name}=SECONDS, such as {example}"
            )
        return item, duration

    return parse


def backend_count(value: str) -> tuple[str, int]:
    """Parse a BACKEND=COUNT value, such as whp=2."""
    backend, separator, count = value.partition("=")
    if (
        not separator
        or backend not in OPENVMM_TEST_BACKENDS
        or not count.isdigit()
        or int(count) < 1
    ):
        raise argparse.ArgumentTypeError(
            "must be BACKEND=COUNT, such as whp=2, for a backend of "
            + ", ".join(OPENVMM_TEST_BACKENDS)
        )
    return backend, int(count)


def json_number(value: object) -> float | None:
    """Return a JSON number, or None for any other value."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


def json_strings(value: object) -> tuple[str, ...] | None:
    """Return a JSON array of strings, or None for any other value."""
    if not isinstance(value, list):
        return None
    items = cast(list[object], value)
    if not all(isinstance(item, str) for item in items):
        return None
    return tuple(cast(list[str], items))


def json_object(value: object, keys: Collection[str]) -> dict[str, object] | None:
    """Return a JSON object with exactly these keys, or None for any other
    value."""
    if not isinstance(value, dict):
        return None
    document = cast(dict[str, object], value)
    return document if set(document) == set(keys) else None


def report_paths(paths: Iterable[Path], name: str) -> list[Path]:
    """Return each report that the paths name, or that the directories among
    them hold under `name`, once."""
    found: dict[Path, None] = {}
    for path in paths:
        if path.is_dir():
            for report in sorted(path.rglob(name)):
                found[report.resolve()] = None
        elif path.is_file():
            found[path.resolve()] = None
        else:
            raise ScriptError(f"{path} is neither a report nor a directory")
    return list(found)


def run_order(run: str) -> tuple[int, ...]:
    """Return the sort key of a CI run, `<run-id>-<attempt>`."""
    return tuple(int(part) if part.isdigit() else 0 for part in run.split("-"))


def _changes(
    items: Sequence[str], shard_of: Mapping[str, int], count: int
) -> Iterator[tuple[tuple[str, int], ...]]:
    """Yield every move of an item to another shard, but of the last item of
    a shard, and every swap of two items of different shards, as the items'
    new shards."""
    sizes = Counter(shard_of.values())
    for item in items:
        if sizes[shard_of[item]] == 1:
            continue
        for shard in range(count):
            if shard != shard_of[item]:
                yield ((item, shard),)
    for first, second in itertools.combinations(items, 2):
        if shard_of[first] != shard_of[second]:
            yield ((first, shard_of[second]), (second, shard_of[first]))


def _gain(current: Sequence[int], candidate: Sequence[int]) -> int:
    """Return by how much a candidate's totals, longest first, undercut the
    current ones where they first differ; negative if they exceed them."""
    for before, after in zip(current, candidate, strict=True):
        if before != after:
            return before - after
    return 0


def plan_shards(
    seconds: Mapping[str, float],
    count: int,
    previous: Sequence[Collection[str]] = (),
    *,
    cost: Callable[[Sequence[str]], int] | None = None,
    noun: str = "items",
) -> tuple[tuple[str, ...], ...]:
    """Split items of the given durations into `count` balanced shards.

    A shard takes the time, in tenths of a second, that `cost` gives for its
    items in name order, by default the sum of their durations. Each item
    starts in its shard of `previous`, the partition to update; the others go,
    longest first, to the shard that takes the least time so far. A shard that
    is still empty then takes the longest item of the longest shard that has
    more than one, since no move into it may shorten a shard enough, as when
    its candidates take no time. The plan then moves an item to another shard,
    or swaps two of different shards, while that shortens the longest shard by
    MINIMUM_GAIN_TENTHS or more, comparing the next longest where the longest
    stay alike, and takes the best such change each time; no change empties a
    shard. Each shard lists its items in name order. `noun` names the items in
    errors.
    """
    durations = {item: tenths(duration) for item, duration in seconds.items()}
    items = sorted(durations)
    if not 1 <= count <= len(items):
        raise ScriptError(f"cannot split {len(items)} {noun} into {count} shards")

    def total(members: Sequence[str]) -> int:
        return sum(durations[item] for item in members)

    shard_cost = total if cost is None else cost

    def members(shard_of: Mapping[str, int], shard: int) -> tuple[str, ...]:
        return tuple(item for item in items if shard_of.get(item) == shard)

    shard_of: dict[str, int] = {}
    for shard, previous_members in enumerate(previous[:count]):
        for item in sorted(previous_members):
            if item in durations and item not in shard_of:
                shard_of[item] = shard
    totals = [shard_cost(members(shard_of, shard)) for shard in range(count)]
    for item in sorted(
        (item for item in items if item not in shard_of),
        key=lambda item: (-durations[item], item),
    ):
        shard = min(range(count), key=lambda index: (totals[index], index))
        shard_of[item] = shard
        totals[shard] = shard_cost(members(shard_of, shard))
    for shard in range(count):
        sizes = Counter(shard_of.values())
        if sizes[shard]:
            continue
        donor = max(
            (index for index in range(count) if sizes[index] > 1),
            key=lambda index: (totals[index], -index),
        )
        item = min(
            (item for item in items if shard_of[item] == donor),
            key=lambda item: (-durations[item], item),
        )
        shard_of[item] = shard
        totals[donor] = shard_cost(members(shard_of, donor))
        totals[shard] = shard_cost(members(shard_of, shard))
    while True:
        current = sorted(totals, reverse=True)
        best: tuple[list[int], tuple[tuple[str, int], ...], list[int]] | None = None
        for change in _changes(items, shard_of, count):
            moved = {**shard_of, **dict(change)}
            trial = list(totals)
            for shard in {shard_of[item] for item, _ in change} | {
                shard for _, shard in change
            }:
                trial[shard] = shard_cost(members(moved, shard))
            candidate = sorted(trial, reverse=True)
            if _gain(current, candidate) >= MINIMUM_GAIN_TENTHS and (
                best is None or candidate < best[0]
            ):
                best = (candidate, change, trial)
        if best is None:
            break
        for item, shard in best[1]:
            shard_of[item] = shard
        totals = best[2]
    return tuple(members(shard_of, shard) for shard in range(count))
