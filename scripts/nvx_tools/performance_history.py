"""Persistence of the performance results that the dev pushes recorded.

A push to dev outside the merge queue rebuilds every merge group in the queue
(doc/ci.md, "Pushes outside the queue"), so a dev push's CI run commits no
performance results to dev. Its `Record performance results` job checks the
p50 values of its platform jobs, and the hosts that measured them, against the
history, and uploads them as the artifact ``performance-results-RUN_ID``. A
full run of dev, such as the nightly one, runs
``nvx.py persist-recorded-performance``, which appends to the history in
data/*.csv the results of each completed dev push run whose commit is newer
than the newest one that the history holds, oldest first in dev's first-parent
order, for the run to commit to dev in one push. It reads the runs a day of
their creation at a time, newest first, down to the first page that holds none
of those, and no further back than GitHub can rerun a run whose results then
remain, so that it finds a rerun's results however late it ran, and skips a
run that last ran too long ago for its results to remain.

The results of each run must match their artifact's digest, come from the
run's own commit, and come from the hosts that the history expected at that
commit, so that a host that dev accepted since then refuses none of them. The
results of a commit before a reset of their series stay out of it, as when a
push persisted them, and so do those of a platform whose expected host changed
in a required field since their commit, which resets its series too, even when
the series held no rows to remove. A run's results go into the history whole or
not at all. A run whose results are refused is skipped, while one whose results
can't be fetched, as when GitHub fails, stops the persistence before it, so that
the next run persists it and every run after it, in order. See doc/ci.md,
"Development releases and performance history".
"""

from __future__ import annotations

import argparse
import csv
import os
import re
import shutil
import subprocess
import tempfile
import time
import urllib.parse
import zipfile
import zlib
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from pathlib import Path, PurePosixPath
from typing import IO, Protocol, cast

from . import benchmark_host
from .common import ScriptError, sha256_file, workflow_path
from .nightly_ci import GITHUB_API_URL, GitHubApiError, GitHubClient
from .performance import (
    PerformanceError,
    persist_results,
    read_results,
    write_results,
)

RESULTS_ARTIFACT_PREFIX = "performance-results-"
# How long a dev push's run keeps its recorded results, in days: the nightly
# run persists them within a day, and the rest outlasts a few failed nights.
RESULTS_RETENTION_DAYS = 7
# GitHub reruns a workflow run up to 30 days after it first ran, so no run
# created more than this and the retention ago holds results.
_RERUN_DAYS = 30
# GitHub lists at most 1,000 runs for a filtered search, so the runs are read
# in searches of a day of their creation each.
_SEARCH_LIMIT = 1000
_SEARCH_WINDOW = timedelta(days=1)
# The recorded results are a few CSV and host files of a few KiB each.
_ARCHIVE_LIMIT_BYTES = 16 * 1024 * 1024
_EXTRACTED_LIMIT_BYTES = 64 * 1024 * 1024
# The persistence reads GitHub for at most this long, well within its job's
# timeout, and stops at the first run that it can't fetch in time.
_FETCH_BUDGET_SECONDS = 15 * 60.0
_DIGEST = re.compile(r"sha256:([0-9a-f]{64})")
_REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
_RESULT_FILE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]*\.csv")
_HOST_FILE = re.compile(
    r"[A-Za-z0-9][A-Za-z0-9_.-]*" + re.escape(benchmark_host.HOST_FILE_SUFFIX)
)


def _decompression_errors() -> tuple[type[Exception], ...]:
    """Return the errors with which zipfile's decompressors refuse data that
    they can't decode, but bzip2's OSError, which _read_member tells apart."""
    try:
        import lzma
    except ImportError:
        # zipfile reads no LZMA member without it.
        return (zlib.error,)
    return (zlib.error, lzma.LZMAError)


# zipfile reports a corrupt member as BadZipFile, a truncated one as EOFError,
# an unknown compression as NotImplementedError, an encrypted one or one whose
# decompressor Python lacks as RuntimeError, and data that it can't decompress
# with its decompressor's error.
_ZIP_ERRORS = (
    zipfile.BadZipFile,
    EOFError,
    NotImplementedError,
    RuntimeError,
    *_decompression_errors(),
)


class RecordedResultsApi(Protocol):
    """The GitHub REST operations that the persistence uses."""

    def get_all(
        self,
        path: str,
        params: Mapping[str, str] | None = None,
        *,
        key: str | None = None,
        deadline: float | None = None,
        until: Callable[[Sequence[object]], bool] | None = None,
    ) -> list[object]: ...

    def download(
        self,
        path: str,
        destination: Path,
        *,
        limit: int,
        deadline: float | None = None,
    ) -> None: ...


@dataclass(frozen=True)
class RecordedRun:
    """A completed workflow run of a push to the branch."""

    id: int
    sha: str
    url: str
    # When the run last changed, which is after it uploaded any results, even
    # those of an attempt that reran it.
    updated: datetime | None = None

    def __str__(self) -> str:
        return f"run {self.id} of {self.sha[:12]}"


class RefusedResults(ScriptError):
    """Recorded results that the history must not take."""


class ExpiredResults(ScriptError):
    """Recorded results that expired before a full run persisted them."""


@dataclass
class Persistence:
    """What the persistence did with the runs that it found."""

    persisted: list[RecordedRun] = field(default_factory=lambda: [])
    # Runs whose commits are no newer than the newest one in the history.
    recorded: int = 0
    skipped: list[str] = field(default_factory=lambda: [])
    refused: list[str] = field(default_factory=lambda: [])
    stopped: str | None = None

    @property
    def failed(self) -> bool:
        return bool(self.refused) or self.stopped is not None


def _mapping(value: object) -> Mapping[str, object]:
    return cast(Mapping[str, object], value) if isinstance(value, dict) else {}


def _repository(value: object) -> str | None:
    name = _mapping(value).get("full_name")
    return name if isinstance(name, str) else None


def _timestamp(value: object) -> datetime | None:
    """Return the time that GitHub's ISO 8601 `value` names, if it is one."""
    if not isinstance(value, str):
        return None
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    return parsed if parsed.tzinfo is not None else None


def _git(checkout: Path, *arguments: str) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        ["git", "-C", str(checkout), *arguments], capture_output=True, check=False
    )


def first_parent_history(checkout: Path) -> list[str]:
    """Return the commits of HEAD's first-parent history in `checkout`,
    oldest first."""
    listed = _git(checkout, "rev-list", "--first-parent", "--reverse", "HEAD")
    if listed.returncode != 0:
        error = listed.stderr.decode("utf-8", errors="replace").strip()
        raise ScriptError(f"cannot list the history of {checkout}: {error}")
    return listed.stdout.decode("ascii").split()


def newest_recorded(history_dir: Path, positions: Mapping[str, int]) -> int | None:
    """Return the position of the newest commit of the first-parent history
    whose results the history in `history_dir` holds."""
    return max(
        (
            positions[result.commit]
            for path in sorted(history_dir.glob("*.csv"))
            for result in read_results(path)
            if result.commit in positions
        ),
        default=None,
    )


def recorded_runs(
    api: RecordedResultsApi,
    repository: str,
    workflow: str,
    branch: str,
    positions: Mapping[str, int],
    newest: int | None,
    now: datetime,
    deadline: float,
) -> list[RecordedRun]:
    """Return the completed runs of `workflow` for pushes to `branch` whose
    commits `positions` holds, oldest first: those created since a run could
    last be rerun and keep its results until `now`, and of those, when
    `newest` is a position, at least the runs of the commits after it."""
    name = urllib.parse.quote(PurePosixPath(workflow).name, safe="")
    reached = False

    def lacking(document: object) -> bool:
        sha = _mapping(document).get("head_sha")
        return (
            isinstance(sha, str)
            and sha in positions
            and (newest is None or positions[sha] > newest)
        )

    def settled(page: Sequence[object]) -> bool:
        # GitHub lists the newest runs first, and pushes, whose runs GitHub
        # creates as they come, follow the branch's first-parent history, so
        # the runs that the history lacks end with the first page of runs
        # that holds none. A rerun keeps its run's place, however late it ran.
        nonlocal reached
        reached = newest is not None and bool(page) and not any(map(lacking, page))
        return reached

    horizon = now - timedelta(days=_RERUN_DAYS + RESULTS_RETENTION_DAYS + 1)
    documents: list[object] = []
    end: datetime | None = None
    while not reached and (end is None or end > horizon):
        start = max((now if end is None else end) - _SEARCH_WINDOW, horizon)
        since = f"{start:%Y-%m-%dT%H:%M:%SZ}"
        listed = api.get_all(
            f"/repos/{repository}/actions/workflows/{name}/runs",
            {
                "branch": branch,
                "event": "push",
                "status": "completed",
                "created": f">={since}"
                if end is None
                else f"{since}..{end:%Y-%m-%dT%H:%M:%SZ}",
            },
            key="workflow_runs",
            deadline=deadline,
            until=settled,
        )
        if len(listed) >= _SEARCH_LIMIT:
            raise ScriptError(
                f"GitHub lists no more than {_SEARCH_LIMIT} of the runs created "
                f"since {since}"
            )
        documents.extend(listed)
        end = start
    runs: dict[int, RecordedRun] = {}
    for document in map(_mapping, documents):
        run_id = document.get("id")
        sha = document.get("head_sha")
        path = document.get("path")
        if (
            document.get("event") != "push"
            or document.get("head_branch") != branch
            or document.get("status") != "completed"
            or not isinstance(path, str)
            or path.partition("@")[0] != workflow
            or _repository(document.get("repository")) != repository
            or _repository(document.get("head_repository")) != repository
            or not isinstance(run_id, int)
            or not isinstance(sha, str)
            or sha not in positions
        ):
            continue
        url = document.get("html_url")
        # Searches share their bounds, so a run can come twice.
        runs[run_id] = RecordedRun(
            run_id,
            sha,
            url if isinstance(url, str) else "",
            _timestamp(document.get("updated_at")),
        )
    return sorted(runs.values(), key=lambda run: (positions[run.sha], run.id))


def _results_artifact(
    api: RecordedResultsApi, repository: str, run: RecordedRun, deadline: float
) -> Mapping[str, object] | None:
    """Return the newest artifact of `run` that holds its recorded results,
    expired or not, or None when the run recorded none."""
    name = f"{RESULTS_ARTIFACT_PREFIX}{run.id}"
    documents = api.get_all(
        f"/repos/{repository}/actions/runs/{run.id}/artifacts",
        {"name": name},
        key="artifacts",
        deadline=deadline,
    )
    candidates = [
        document
        for document in map(_mapping, documents)
        if document.get("name") == name
        and isinstance(document.get("id"), int)
        and _mapping(document.get("workflow_run")).get("id") == run.id
    ]
    return max(
        candidates,
        key=lambda document: (
            str(document.get("created_at", "")),
            cast(int, document["id"]),
        ),
        default=None,
    )


def _read_member(source: IO[bytes]) -> bytes:
    """Return the next chunk of a member of an archive, or b"" at its end."""
    try:
        return source.read(1024 * 1024)
    except OSError as error:
        # bzip2 refuses data that it can't decode with an OSError, which only
        # reading an archive that _fetch wrote can raise, unlike a write.
        raise RefusedResults(
            f"its artifact is no valid ZIP archive: {error}"
        ) from error


def _extract(archive: Path, destination: Path) -> None:
    """Extract the result and host files of `archive` into `destination`, and
    refuse an archive that holds anything else."""
    try:
        with zipfile.ZipFile(archive) as bundle:
            members = bundle.infolist()
            if sum(member.file_size for member in members) > _EXTRACTED_LIMIT_BYTES:
                raise RefusedResults("its artifact holds too much")
            for member in members:
                name = member.filename
                if member.is_dir() or not (
                    _RESULT_FILE.fullmatch(name) or _HOST_FILE.fullmatch(name)
                ):
                    raise RefusedResults(
                        f"its artifact holds {name!r}, which is no result or host file"
                    )
                with (
                    bundle.open(member) as source,
                    (destination / name).open("xb") as output,
                ):
                    while chunk := _read_member(source):
                        output.write(chunk)
    except RefusedResults:
        raise
    except FileExistsError as error:
        raise RefusedResults(
            f"its artifact holds {Path(error.filename).name} twice"
        ) from error
    except _ZIP_ERRORS as error:
        raise RefusedResults(
            f"its artifact is no valid ZIP archive: {error}"
        ) from error


def _fetch(
    api: RecordedResultsApi,
    repository: str,
    run: RecordedRun,
    artifact: Mapping[str, object],
    download_dir: Path,
    deadline: float,
) -> Path:
    """Return the directory of `run`'s recorded results, which an earlier
    attempt may have downloaded already."""
    directory = download_dir / str(run.id)
    if directory.is_dir():
        return directory
    if artifact.get("expired") is not False:
        raise ExpiredResults("its recorded results expired")
    # The digest that GitHub took when the run uploaded the artifact is the
    # only check of the archive that doesn't come from inside it.
    digest = _DIGEST.fullmatch(str(artifact.get("digest") or ""))
    if digest is None:
        raise RefusedResults("its artifact has no SHA-256 digest")
    # Downloading an archive that is too large fails it every time, which
    # would stop every later batch at this run until the archive expired.
    size = artifact.get("size_in_bytes")
    if isinstance(size, int) and size > _ARCHIVE_LIMIT_BYTES:
        raise RefusedResults(
            f"artifact {artifact['id']} holds {size} bytes, more than the "
            f"{_ARCHIVE_LIMIT_BYTES} that recorded results may"
        )
    download_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=download_dir) as temporary:
        staging = Path(temporary)
        archive = staging / "results.zip"
        api.download(
            f"/repos/{repository}/actions/artifacts/{artifact['id']}/zip",
            archive,
            limit=_ARCHIVE_LIMIT_BYTES,
            deadline=deadline,
        )
        if sha256_file(archive) != digest.group(1):
            raise RefusedResults(f"artifact {artifact['id']} doesn't match its digest")
        results = staging / "results"
        results.mkdir()
        _extract(archive, results)
        results.rename(directory)
    return directory


def _platforms(directory: Path, run: RecordedRun) -> set[str]:
    """Return the platforms of the results in `directory`, which must all
    come from `run`'s commit."""
    files = sorted(directory.glob("*.csv"))
    if not files:
        raise RefusedResults("its artifact holds no results")
    platforms: set[str] = set()
    for path in files:
        try:
            results = read_results(path)
        except (PerformanceError, OSError, csv.Error, UnicodeError) as error:
            raise RefusedResults(f"{path.name}: {error}") from error
        commits = sorted({result.commit for result in results})
        if commits != [run.sha]:
            raise RefusedResults(
                f"{path.name} holds the results of "
                f"{', '.join(commit[:12] for commit in commits) or 'no commit'}"
            )
        platforms.update(result.platform for result in results if result.platform)
    return platforms


def _expected_hosts(
    checkout: Path,
    history: PurePosixPath,
    commit: str,
    platforms: set[str],
    destination: Path,
) -> None:
    """Write into `destination` the host files of `platforms` that the history
    held at `commit`; persisting refuses the results of a platform without
    one."""
    for platform in sorted(platforms):
        name = benchmark_host.host_file(Path(), platform).name
        shown = _git(checkout, "show", f"{commit}:{(history / name).as_posix()}")
        if shown.returncode == 0:
            (destination / name).write_bytes(shown.stdout)


def _versions(checkout: Path, path: PurePosixPath, commits: Sequence[str]) -> list[str]:
    """Return the object ID of the file `path` at each of `commits`, or "" at
    those that lack it."""
    listed = subprocess.run(
        ["git", "-C", str(checkout), "cat-file", "--batch-check=%(objectname)"],
        input="".join(f"{commit}:{path}\n" for commit in commits).encode("utf-8"),
        capture_output=True,
        check=False,
    )
    lines = listed.stdout.decode("utf-8", errors="replace").splitlines()
    if listed.returncode != 0 or len(lines) != len(commits):
        error = listed.stderr.decode("utf-8", errors="replace").strip()
        raise ScriptError(f"cannot read the history of {path}: {error}")
    return ["" if line.endswith(" missing") else line for line in lines]


def _host(
    checkout: Path, blob: str, platform: str, directory: Path
) -> dict[str, object] | None:
    """Return the host of `platform` that the object `blob` describes, or None
    when it is no valid host file."""
    shown = _git(checkout, "cat-file", "blob", blob)
    if shown.returncode != 0:
        error = shown.stderr.decode("utf-8", errors="replace").strip()
        raise ScriptError(f"cannot read the host file {blob}: {error}")
    path = directory / f"{blob}.json"
    path.write_bytes(shown.stdout)
    try:
        return benchmark_host.read_host(path, platform)
    except benchmark_host.HostFileError:
        return None


def host_changes(
    checkout: Path,
    history: PurePosixPath,
    platform: str,
    commits: Sequence[str],
    start: int,
) -> list[int]:
    """Return the positions in `commits`, a first-parent history oldest
    first, after `start`, of the commits that changed the host that the
    history in `history` expects of `platform` in a required field, or
    removed or added the platform's host file."""
    path = history / benchmark_host.host_file(Path(), platform).name
    window = commits[start:]
    versions = _versions(checkout, path, window)
    changes: list[int] = []
    with tempfile.TemporaryDirectory() as temporary:
        directory = Path(temporary)
        for offset in range(1, len(window)):
            before, after = versions[offset - 1], versions[offset]
            if before == after:
                continue
            old = _host(checkout, before, platform, directory) if before else None
            new = _host(checkout, after, platform, directory) if after else None
            if (
                old is None
                or new is None
                or benchmark_host.mismatches(benchmark_host.compare_hosts(old, new))
            ):
                changes.append(start + offset)
    return changes


def _without_platforms(directory: Path, platforms: set[str], destination: Path) -> bool:
    """Copy the results and hosts in `directory` but those of `platforms` into
    `destination`, and return whether any result remains."""
    destination.mkdir()
    remains = False
    for path in sorted(directory.glob("*.csv")):
        results = [
            result for result in read_results(path) if result.platform not in platforms
        ]
        if results:
            write_results(destination / path.name, results)
            remains = True
    for path in sorted(directory.glob(f"*{benchmark_host.HOST_FILE_SUFFIX}")):
        if path.name.removesuffix(benchmark_host.HOST_FILE_SUFFIX) not in platforms:
            shutil.copyfile(path, destination / path.name)
    return remains


def _persist_whole(
    directory: Path,
    history_dir: Path,
    hosts: Path,
    checkout: Path,
    require_host: bool,
) -> bool:
    """Persist the results in `directory`, or leave the history as it was, and
    return whether the history changed."""
    snapshot = {path: path.read_bytes() for path in history_dir.glob("*.csv")}
    try:
        persist_results(
            directory,
            history_dir,
            require_host=require_host,
            git_checkout=checkout,
            host_dir=hosts,
        )
    except BaseException:
        for path in history_dir.glob("*.csv"):
            if path not in snapshot:
                path.unlink()
        for path, contents in snapshot.items():
            path.write_bytes(contents)
        raise
    return {path: path.read_bytes() for path in history_dir.glob("*.csv")} != snapshot


def persist_recorded_performance(
    api: RecordedResultsApi,
    *,
    repository: str,
    workflow: str,
    branch: str,
    checkout: Path,
    history_dir: Path,
    download_dir: Path,
    require_host: bool,
    now: datetime,
    deadline: float,
) -> Persistence:
    """Append to the history in `history_dir`, within the git checkout
    `checkout` of `branch`, the recorded results of each completed push run
    of `workflow` that the history lacks, oldest first."""
    commits = first_parent_history(checkout)
    positions = {commit: position for position, commit in enumerate(commits)}
    newest = newest_recorded(history_dir, positions)
    history = PurePosixPath(
        *history_dir.resolve().relative_to(checkout.resolve()).parts
    )
    outcome = Persistence()
    try:
        runs = recorded_runs(
            api, repository, workflow, branch, positions, newest, now, deadline
        )
    except (ScriptError, OSError) as error:
        outcome.stopped = f"could not list the runs of pushes to {branch}: {error}"
        return outcome
    # A run uploads its results before it last changes, and they expire a
    # retention period after they came, so a run that last changed before
    # then holds none.
    expired = now - timedelta(days=RESULTS_RETENTION_DAYS + 1)
    changes: dict[str, list[int]] = {}

    def replaced(platform: str, position: int) -> bool:
        """Return whether the history expects another host of `platform`
        than it did at the commit at `position`, the oldest one that it is
        asked about first."""
        if platform not in changes:
            changes[platform] = host_changes(
                checkout, history, platform, commits, position
            )
        return any(change > position for change in changes[platform])

    for run in runs:
        position = positions[run.sha]
        if newest is not None and position <= newest:
            outcome.recorded += 1
            continue
        if run.updated is not None and run.updated < expired:
            outcome.skipped.append(
                f"{run} last ran over {RESULTS_RETENTION_DAYS + 1} days ago, so "
                "any results that it recorded expired"
            )
            continue
        try:
            artifact = _results_artifact(api, repository, run, deadline)
            if artifact is None:
                outcome.skipped.append(f"{run} recorded no results")
                continue
            directory = _fetch(api, repository, run, artifact, download_dir, deadline)
            platforms = _platforms(directory, run)
            # dev resets a platform's series with each new host, which leaves
            # no row to remove while the series is empty, so the results of
            # an older host stay out of it either way.
            stale = {platform for platform in platforms if replaced(platform, position)}
            with tempfile.TemporaryDirectory() as temporary:
                hosts = Path(temporary) / "hosts"
                hosts.mkdir()
                _expected_hosts(checkout, history, run.sha, platforms - stale, hosts)
                source = Path(temporary) / "results" if stale else directory
                changed = (
                    not stale or _without_platforms(directory, stale, source)
                ) and _persist_whole(source, history_dir, hosts, checkout, require_host)
        except RefusedResults as error:
            outcome.refused.append(f"{run}: {error}")
            continue
        except ExpiredResults as error:
            outcome.skipped.append(f"{run}: {error}")
            continue
        except GitHubApiError as error:
            # GitHub deletes an artifact some time after it expires.
            if error.status in (404, 410):
                outcome.refused.append(f"{run}: its recorded results are gone")
                continue
            outcome.stopped = f"stopped at {run}: {error}"
            break
        except (PerformanceError, csv.Error, UnicodeError) as error:
            outcome.refused.append(f"{run}: {error}")
            continue
        except (ScriptError, OSError) as error:
            outcome.stopped = f"stopped at {run}: {error}"
            break
        if stale:
            outcome.skipped.append(
                f"{run}: the history expects another host of "
                f"{', '.join(sorted(stale))} since its commit"
            )
        if changed:
            outcome.persisted.append(run)
        elif stale != platforms:
            # As for the results of a commit before a reset of their series.
            outcome.skipped.append(f"{run} added nothing to the history")
    return outcome


def commit_message(persisted: Sequence[RecordedRun]) -> str:
    """Return the message of the commit that adds `persisted`'s results."""
    lines = [
        f"data: update performance baselines ({persisted[-1].sha}) [skip ci]",
        "",
        "Persist the performance results that these dev push runs recorded:",
        "",
        *(f"- {run.sha} {run.url}" for run in persisted),
    ]
    return "\n".join(lines) + "\n"


def _annotation(text: str) -> str:
    """Escape `text` for the message of a workflow command."""
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def report(outcome: Persistence) -> tuple[list[str], str]:
    """Return the log lines and the Markdown summary of `outcome`."""
    lines = [f"persisted the results of {run}" for run in outcome.persisted]
    if outcome.recorded:
        lines.append(
            f"{outcome.recorded} run(s) of commits that the history already "
            "reaches persist nothing"
        )
    lines.extend(outcome.skipped)
    lines.extend(f"::error::{_annotation(line)}" for line in outcome.refused)
    if outcome.stopped is not None:
        lines.append(f"::error::{_annotation(outcome.stopped)}")
    summary = ["### Performance history", ""]
    if outcome.persisted:
        summary.append("Persisted the recorded results of:")
        summary.extend(
            f"- [run {run.id}]({run.url}) of `{run.sha[:12]}`"
            for run in outcome.persisted
        )
    else:
        summary.append("Persisted no recorded results.")
    details = [
        *outcome.skipped,
        *(f"Refused: {line}" for line in outcome.refused),
        *([f"Stopped: {outcome.stopped}"] if outcome.stopped is not None else []),
    ]
    if details:
        summary.extend(("", *(f"- {line}" for line in details)))
    return lines, "\n".join(summary) + "\n"


def _toplevel(directory: Path) -> Path:
    shown = _git(directory, "rev-parse", "--show-toplevel")
    if shown.returncode != 0:
        raise ScriptError(f"{directory} is in no git checkout")
    return Path(shown.stdout.decode("utf-8").strip())


def command_persist_recorded_performance(args: argparse.Namespace) -> int:
    if not _REPOSITORY.fullmatch(args.repository):
        raise ScriptError("--repository must be OWNER/REPOSITORY")
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if not token:
        raise ScriptError(
            "set GH_TOKEN to a token that can read the repository's workflow runs"
        )
    history_dir: Path = args.history_dir
    if not history_dir.is_dir():
        raise ScriptError(f"--history-dir {history_dir} is no directory")
    outcome = persist_recorded_performance(
        GitHubClient(token, os.environ.get("GITHUB_API_URL") or GITHUB_API_URL),
        repository=args.repository,
        workflow=workflow_path(args.workflow_ref, args.repository),
        branch=args.branch,
        checkout=_toplevel(history_dir),
        history_dir=history_dir,
        download_dir=args.download_dir,
        require_host=args.require_host,
        now=datetime.now(timezone.utc),
        deadline=time.monotonic() + _FETCH_BUDGET_SECONDS,
    )
    lines, summary = report(outcome)
    for line in lines:
        print(line)
    if args.summary is not None:
        args.summary.write_text(summary, encoding="utf-8")
    if args.message is not None and outcome.persisted:
        args.message.write_text(commit_message(outcome.persisted), encoding="utf-8")
    return 1 if outcome.failed else 0


def configure_parser(parser: argparse.ArgumentParser) -> None:
    parser.description = (
        "Append to the performance history the results that the CI runs of "
        "pushes to a branch recorded, in the branch's first-parent order, for "
        "a full run of the branch to commit (doc/ci.md, 'Development releases "
        "and performance history')."
    )
    parser.add_argument(
        "--repository",
        required=True,
        metavar="OWNER/REPOSITORY",
        help="the GitHub repository of the runs",
    )
    parser.add_argument(
        "--workflow-ref",
        required=True,
        metavar="REF",
        help="the run's GITHUB_WORKFLOW_REF, which names the workflow whose "
        "push runs recorded the results",
    )
    parser.add_argument(
        "--branch",
        default="dev",
        help="the branch whose pushes' runs recorded the results, and whose "
        "checkout holds the history (default: dev)",
    )
    parser.add_argument(
        "--history-dir",
        type=Path,
        default=Path("data"),
        metavar="DIR",
        help="the history's directory in the checkout (default: data)",
    )
    parser.add_argument(
        "--download-dir",
        type=Path,
        required=True,
        metavar="DIR",
        help="directory that keeps the downloaded results, so that another "
        "attempt reuses them",
    )
    parser.add_argument(
        "--require-host",
        action="store_true",
        help="refuse results unless both they and the history record a host",
    )
    parser.add_argument(
        "--summary",
        type=Path,
        metavar="PATH",
        help="Markdown file to write the outcome to",
    )
    parser.add_argument(
        "--message",
        type=Path,
        metavar="PATH",
        help="file to write the message of the commit of the persisted "
        "results to, when the command persisted any",
    )
    parser.set_defaults(handler=command_persist_recorded_performance)
