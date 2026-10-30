"""Reuse of the guest artifact components that a pull request's CI run built.

GitHub scopes the caches that a pull request's runs save to the pull request,
so the merge group and the dev push that merge it miss them and would rebuild
the guest images that the pull request already built. Each guest artifact job
therefore records the exact cache keys of its components and the SHA-256 of
each of their files in a guest artifact record, which the build-guest-artifacts
action uploads beside the components' own artifacts. When a file of its
components missed the cache, the job of a push or a merge group looks up the
latest run of its workflow for the head of the pull request that its merge
commit merged, from the same repository and never from a fork, and installs
every file of its components from that run's artifacts once each artifact
matches the digest that GitHub recorded for it, the record's keys equal the
job's own, and the files match the record, their provenance, and the job's
sources. Any other outcome, or a time limit, leaves the components to the
action's builds, so reuse never fails a run and never installs an unverified
file.
"""

from __future__ import annotations

import contextlib
import hashlib
import http.client
import json
import os
import re
import shutil
import signal
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from collections.abc import Generator, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from types import FrameType
from typing import Protocol, cast

from .common import (
    ScriptError,
    credential_safe_opener,
    require_file,
    sha256_file,
    strict_json_object,
)
from .common import workflow_path as github_workflow_path
from .release import (
    GITHUB_API_VERSION,
    GUEST_ARTIFACT_COMPONENTS,
    validate_guest_artifacts,
)

RECORD_NAME = "guest-artifacts.json"
RECORD_FORMAT = 1
# The outputs of the action's key step that key the cache entries of each
# component that a record covers: those that a package ships.
COMPONENT_CACHE_KEYS: Mapping[str, tuple[str, ...]] = {
    "kernel": ("kernel",),
    "alpine": ("alpine",),
    "ubuntu": ("ubuntu", "ubuntu-distro"),
    "azurelinux": ("azurelinux",),
}
# The workflow artifacts in which the guest artifact jobs hand over the files
# of each component that a record covers.
COMPONENT_ARTIFACTS: Mapping[str, tuple[str, ...]] = {
    "kernel": ("guest-kernel",),
    "alpine": ("guest-alpine",),
    "ubuntu": ("guest-ubuntu", "guest-ubuntu-distro"),
    "azurelinux": ("guest-azurelinux",),
}
GITHUB_API_URL = "https://api.github.com"
# A reuse takes seconds, and the cold builds that it replaces take from about
# 1.3 minutes, for the Alpine initramfs alone, to several minutes, so a reuse
# that stalls for longer than this saves little.
REUSE_TIMEOUT_SECONDS = 120.0
_REQUEST_TIMEOUT_SECONDS = 30.0
_CHUNK_BYTES = 1024 * 1024
_RECORD_MAX_BYTES = 1024 * 1024
_SHA256 = re.compile(r"[0-9a-f]{64}")
_SHA256_DIGEST = re.compile(r"sha256:([0-9a-f]{64})")
_REUSE = "Guest artifact reuse"
_REHEARSAL = "Guest artifact reuse rehearsal"


class ReuseRefused(ScriptError):
    """A reason to build guest artifact components rather than reuse a run's.

    An expected reason, such as a merge commit that merged no same-repository
    pull request run or different cache keys, is a normal outcome; any other
    suggests a fault, which CI reports as a warning.
    """

    def __init__(self, reason: str, *, expected: bool = False) -> None:
        super().__init__(reason)
        self.expected = expected


class GitHubApi(Protocol):
    """The GitHub REST API requests that reuse makes."""

    def get(self, endpoint: str) -> object:
        """Return the JSON document at `endpoint`, relative to the API root."""
        ...

    def download(self, endpoint: str, destination: Path) -> None:
        """Write the content at `endpoint` to `destination`."""
        ...


class RestApi:
    """Reads the GitHub REST API with a token, until a monotonic deadline.

    Its opener drops the token when a download redirects to another origin,
    as artifact downloads do to their storage.
    """

    def __init__(
        self,
        token: str,
        deadline: float,
        *,
        url: str = GITHUB_API_URL,
    ) -> None:
        self._token = token
        self._deadline = deadline
        self._url = url.rstrip("/")
        self._opener = credential_safe_opener()

    def _timeout(self, endpoint: str) -> float:
        remaining = self._deadline - time.monotonic()
        if remaining <= 0:
            raise ReuseRefused(f"reuse ran out of time before GET {endpoint}")
        return min(_REQUEST_TIMEOUT_SECONDS, remaining)

    def _request(self, endpoint: str) -> urllib.request.Request:
        return urllib.request.Request(
            f"{self._url}/{endpoint}",
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {self._token}",
                "User-Agent": "nvx-guest-artifact-reuse",
                "X-GitHub-Api-Version": GITHUB_API_VERSION,
            },
        )

    def get(self, endpoint: str) -> object:
        try:
            with self._opener.open(
                self._request(endpoint), timeout=self._timeout(endpoint)
            ) as response:
                return json.load(response)
        except (OSError, ValueError, http.client.HTTPException) as error:
            raise _request_refusal(endpoint, error) from error

    def download(self, endpoint: str, destination: Path) -> None:
        try:
            with (
                self._opener.open(
                    self._request(endpoint), timeout=self._timeout(endpoint)
                ) as response,
                destination.open("wb") as output,
            ):
                while chunk := response.read(_CHUNK_BYTES):
                    output.write(chunk)
                    if time.monotonic() > self._deadline:
                        raise ReuseRefused(
                            f"reuse ran out of time while downloading {endpoint}"
                        )
        except (OSError, http.client.HTTPException) as error:
            raise _request_refusal(endpoint, error) from error


def _request_refusal(
    endpoint: str,
    error: OSError | ValueError | http.client.HTTPException,
) -> ReuseRefused:
    # A response that ends early, or a malformed one, raises an HTTPException,
    # which is no OSError, but fails the request all the same.
    if isinstance(error, urllib.error.HTTPError):
        # An HTTP error holds the response open.
        error.close()
        return ReuseRefused(f"GET {endpoint} failed with HTTP {error.code}")
    return ReuseRefused(f"GET {endpoint} failed: {error}")


@dataclass(frozen=True)
class PullRequestRun:
    """A pull_request run of the workflow, from the repository itself."""

    id: int
    url: str


@dataclass(frozen=True)
class RunArtifact:
    """An unexpired artifact of a workflow run."""

    id: int
    name: str
    digest: str | None


@dataclass(frozen=True)
class ReuseRequest:
    """What to reuse, and for which push, merge group, or rehearsal.

    A push or a merge group names its merge commit, whose second parent is the
    head of the pull request that it merged; a rehearsal names that head
    itself. `keys` gives, by name, the exact keys of the cache entries of
    `components`, and `record` names the artifact of their record.
    """

    repository: str
    workflow_ref: str
    record: str
    components: tuple[str, ...]
    keys: Mapping[str, str]
    commit: str | None = None
    head_sha: str | None = None
    dry_run: bool = False


@dataclass(frozen=True)
class ReuseOutcome:
    """What reuse did, and why; `level` annotates a fallback to building."""

    reused: bool
    heading: str
    status: str
    detail: str
    level: str | None = None

    def summary(self) -> str:
        return f"### {self.heading}: {self.status}\n{self.detail}\n"


def _object(value: object, description: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ReuseRefused(f"GitHub returned an invalid {description}")
    return cast(dict[str, object], value)


def _objects(value: object, description: str) -> list[dict[str, object]]:
    if not isinstance(value, list):
        raise ReuseRefused(f"GitHub returned an invalid {description} list")
    return [_object(item, description) for item in cast(list[object], value)]


def _text(document: dict[str, object], field: str, description: str) -> str:
    value = document.get(field)
    if not isinstance(value, str) or not value:
        raise ReuseRefused(f"GitHub returned a {description} without a {field}")
    return value


def _integer(document: dict[str, object], field: str, description: str) -> int:
    value = document.get(field)
    if not isinstance(value, int) or isinstance(value, bool):
        raise ReuseRefused(f"GitHub returned a {description} without a {field}")
    return value


def _repository_name(value: object) -> str | None:
    """Return the full name of a repository document; a deleted fork's is
    null."""
    if not isinstance(value, dict):
        return None
    name = cast(dict[str, object], value).get("full_name")
    return name if isinstance(name, str) else None


def parse_components(text: str) -> tuple[str, ...]:
    """Parse the space-separated guest artifact components of a record, as
    the action's components input lists them."""
    names = set(text.split())
    unknown = sorted(names - COMPONENT_CACHE_KEYS.keys())
    if unknown:
        raise ScriptError(
            f"no guest artifact record covers {', '.join(unknown)}, only "
            f"{', '.join(COMPONENT_CACHE_KEYS)}"
        )
    if not names:
        raise ScriptError("no guest artifact component is selected")
    return tuple(name for name in COMPONENT_CACHE_KEYS if name in names)


def component_files(components: Sequence[str]) -> tuple[str, ...]:
    """Return the names of the files of `components` in build/."""
    return tuple(
        name
        for component in components
        for name in GUEST_ARTIFACT_COMPONENTS[component]
    )


def _cache_keys(document: object) -> dict[str, str]:
    if not isinstance(document, dict) or not document:
        raise ScriptError("cache keys must be a nonempty JSON object")
    keys: dict[str, str] = {}
    for name, value in cast(dict[str, object], document).items():
        if not name or not isinstance(value, str) or not value:
            raise ScriptError(f"cache key {name!r} must be a nonempty string")
        keys[name] = value
    return keys


def parse_cache_keys(text: str) -> dict[str, str]:
    """Parse a JSON object of exact cache keys by name."""
    try:
        document: object = json.loads(text, object_pairs_hook=strict_json_object)
    except ValueError as error:
        raise ScriptError(f"cache keys are not JSON: {error}") from error
    return _cache_keys(document)


def component_keys(
    components: Sequence[str],
    keys: Mapping[str, str],
) -> dict[str, str]:
    """Return, by name, the exact cache keys among `keys` of the cache entries
    of `components`."""
    selected: dict[str, str] = {}
    for component in components:
        for name in COMPONENT_CACHE_KEYS[component]:
            if not keys.get(name):
                raise ScriptError(f"the cache keys give no {name} key")
            selected[name] = keys[name]
    return dict(sorted(selected.items()))


def record_guest_artifacts(
    directory: Path,
    components: Sequence[str],
    keys: Mapping[str, str],
) -> Path:
    """Write the guest artifact record of `components` in `directory`: the
    exact cache keys of their cache entries, from `keys`, and the SHA-256 of
    each of their files, which a later run checks before it reuses them."""
    files = {
        name: sha256_file(require_file(directory / name, f"guest artifact {name}"))
        for name in component_files(components)
    }
    document = {
        "format": RECORD_FORMAT,
        "keys": component_keys(components, _cache_keys(dict(keys))),
        "files": files,
    }
    path = directory / RECORD_NAME
    path.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    return path


def _parse_record(contents: bytes) -> tuple[dict[str, str], dict[str, str]]:
    """Return the cache keys and the file digests, by name, of a record."""
    try:
        document: object = json.loads(
            contents.decode("utf-8"), object_pairs_hook=strict_json_object
        )
        if not isinstance(document, dict):
            raise ScriptError("it is not a JSON object")
        record = cast(dict[str, object], document)
        if type(record.get("format")) is not int or record["format"] != RECORD_FORMAT:
            raise ScriptError(f"its format is not {RECORD_FORMAT}")
        keys = _cache_keys(record.get("keys"))
        files = record.get("files")
        if not isinstance(files, dict) or not files:
            raise ScriptError("it lists no files")
        digests: dict[str, str] = {}
        for name, digest in cast(dict[str, object], files).items():
            if not isinstance(digest, str) or _SHA256.fullmatch(digest) is None:
                raise ScriptError(f"it gives no SHA-256 of {name}")
            digests[name] = digest
    except (ScriptError, ValueError) as error:
        raise ReuseRefused(f"the {RECORD_NAME} record is invalid: {error}") from error
    return keys, digests


def workflow_path(workflow_ref: str, repository: str) -> str:
    """Return the path of the workflow that `workflow_ref`, the
    GITHUB_WORKFLOW_REF of a run of `repository`, names."""
    try:
        return github_workflow_path(workflow_ref, repository)
    except ScriptError as error:
        raise ReuseRefused(str(error)) from error


def merged_head(api: GitHubApi, repository: str, commit: str) -> str:
    """Return the head of the pull request that the merge commit `commit`
    merged: its second parent."""
    document = _object(api.get(f"repos/{repository}/git/commits/{commit}"), "commit")
    parents = [
        _text(parent, "sha", "commit parent")
        for parent in _objects(document.get("parents"), "commit parent")
    ]
    if len(parents) != 2:
        raise ReuseRefused(
            f"`{commit}` is not the merge commit of a pull request", expected=True
        )
    return parents[1]


def latest_pull_request_run(
    api: GitHubApi,
    repository: str,
    workflow: str,
    head_sha: str,
) -> PullRequestRun:
    """Return the latest pull_request run of `workflow` for `head_sha` whose
    head is in `repository`; a fork's run never qualifies."""
    query = urllib.parse.urlencode(
        {"event": "pull_request", "head_sha": head_sha, "per_page": 100}
    )
    name = urllib.parse.quote(PurePosixPath(workflow).name, safe="")
    document = _object(
        api.get(f"repos/{repository}/actions/workflows/{name}/runs?{query}"),
        "workflow run list",
    )
    candidates: list[tuple[int, int, PullRequestRun]] = []
    other_repositories = 0
    for run in _objects(document.get("workflow_runs"), "workflow run"):
        if (
            run.get("event") != "pull_request"
            or run.get("head_sha") != head_sha
            or _text(run, "path", "workflow run").partition("@")[0] != workflow
        ):
            continue
        if (
            _repository_name(run.get("head_repository")) != repository
            or _repository_name(run.get("repository")) != repository
        ):
            other_repositories += 1
            continue
        run_id = _integer(run, "id", "workflow run")
        candidates.append(
            (
                _integer(run, "run_number", "workflow run"),
                run_id,
                PullRequestRun(run_id, _text(run, "html_url", "workflow run")),
            )
        )
    if not candidates:
        ignored = (
            f"; {other_repositories} from other repositories never qualify"
            if other_repositories
            else ""
        )
        raise ReuseRefused(
            f"no pull request run of `{workflow}` from {repository} built "
            f"`{head_sha}`{ignored}",
            expected=True,
        )
    return max(candidates, key=lambda candidate: candidate[:2])[2]


def retained_artifact(
    api: GitHubApi,
    repository: str,
    run: PullRequestRun,
    name: str,
) -> RunArtifact:
    """Return the newest unexpired artifact named `name` of `run`."""
    query = urllib.parse.urlencode({"name": name, "per_page": 100})
    document = _object(
        api.get(f"repos/{repository}/actions/runs/{run.id}/artifacts?{query}"),
        "artifact list",
    )
    candidates: list[tuple[str, int, RunArtifact]] = []
    for artifact in _objects(document.get("artifacts"), "artifact"):
        workflow_run = _object(artifact.get("workflow_run"), "artifact run")
        if (
            artifact.get("name") != name
            or artifact.get("expired") is not False
            or workflow_run.get("id") != run.id
        ):
            continue
        artifact_id = _integer(artifact, "id", "artifact")
        digest = artifact.get("digest")
        candidates.append(
            (
                _text(artifact, "created_at", "artifact"),
                artifact_id,
                RunArtifact(
                    artifact_id, name, digest if isinstance(digest, str) else None
                ),
            )
        )
    if not candidates:
        raise ReuseRefused(
            f"[run {run.id}]({run.url}) has no unexpired `{name}` artifact",
            expected=True,
        )
    return max(candidates, key=lambda candidate: candidate[:2])[2]


def _download(
    api: GitHubApi,
    repository: str,
    artifact: RunArtifact,
    archive: Path,
) -> None:
    # The digest that GitHub took when the run uploaded the artifact is the
    # only check of the archive that doesn't come from inside it.
    digest = _SHA256_DIGEST.fullmatch(artifact.digest or "")
    if digest is None:
        raise ReuseRefused(
            f"artifact {artifact.id} has no SHA-256 digest, but {artifact.digest!r}"
        )
    api.download(f"repos/{repository}/actions/artifacts/{artifact.id}/zip", archive)
    if sha256_file(archive) != digest.group(1):
        raise ReuseRefused(
            f"artifact {artifact.id} does not match its digest {artifact.digest}"
        )


# zipfile reports a corrupt member as BadZipFile, a truncated one as EOFError,
# an unknown compression as NotImplementedError, and an encrypted one as
# RuntimeError.
_ZIP_ERRORS = (
    zipfile.BadZipFile,
    EOFError,
    NotImplementedError,
    RuntimeError,
    ValueError,
)


def _read_record(archive: Path, name: str) -> tuple[dict[str, str], dict[str, str]]:
    """Return the cache keys and the file digests of the record that the
    artifact `name`, downloaded as `archive`, holds."""
    try:
        with zipfile.ZipFile(archive) as bundle:
            members = [
                member for member in bundle.infolist() if member.filename == RECORD_NAME
            ]
            if len(members) != 1:
                raise ReuseRefused(f"artifact `{name}` holds no single {RECORD_NAME}")
            if members[0].file_size > _RECORD_MAX_BYTES:
                raise ReuseRefused(f"the {RECORD_NAME} record is too large")
            contents = bundle.read(members[0])
    except ScriptError:
        raise
    except _ZIP_ERRORS as error:
        raise ReuseRefused(
            f"artifact `{name}` is not a valid ZIP archive: {error}"
        ) from error
    return _parse_record(contents)


def _extract(
    archive: Path,
    name: str,
    staging: Path,
    digests: Mapping[str, str],
    extracted: set[str],
) -> None:
    """Extract into `staging` each file that `digests` names from the artifact
    `name`, downloaded as `archive`, check it against its SHA-256 there, and
    add its name to `extracted`."""
    try:
        with zipfile.ZipFile(archive) as bundle:
            for member in bundle.infolist():
                if member.is_dir() or member.filename not in digests:
                    continue
                if member.filename in extracted:
                    raise ReuseRefused(
                        f"the artifacts hold {member.filename} more than once"
                    )
                digest = hashlib.sha256()
                with (
                    bundle.open(member) as source,
                    (staging / member.filename).open("wb") as output,
                ):
                    while chunk := source.read(_CHUNK_BYTES):
                        output.write(chunk)
                        digest.update(chunk)
                if digest.hexdigest() != digests[member.filename]:
                    raise ReuseRefused(
                        f"{member.filename} in artifact `{name}` does not match "
                        "the SHA-256 that the record gives"
                    )
                extracted.add(member.filename)
    except ScriptError:
        raise
    except _ZIP_ERRORS as error:
        raise ReuseRefused(
            f"artifact `{name}` is not a valid ZIP archive: {error}"
        ) from error


def _key_differences(
    recorded: Mapping[str, str],
    expected: Mapping[str, str],
) -> list[str]:
    return sorted(
        name
        for name in recorded.keys() | expected.keys()
        if recorded.get(name) != expected.get(name)
    )


def _install(staging: Path, destination: Path, names: Sequence[str]) -> None:
    """Move the files `names` from `staging` into `destination`, in place of
    any there, or leave `destination` as it was.

    Each move is noted before it happens, so that the rollback also undoes a
    move that an exception interrupted right after the rename.
    """
    replaced = staging / ".replaced"
    replaced.mkdir()
    moving: list[str] = []
    installing: list[str] = []
    try:
        for name in names:
            target = destination / name
            if os.path.lexists(target):
                moving.append(name)
                os.replace(target, replaced / name)
            installing.append(name)
            os.replace(staging / name, target)
    except BaseException as error:
        # Whatever is at a target being installed is either its new file or
        # nothing, since its old one, if any, has moved aside.
        for name in installing:
            (destination / name).unlink(missing_ok=True)
        for name in moving:
            if os.path.lexists(replaced / name):
                os.replace(replaced / name, destination / name)
        if isinstance(error, OSError):
            raise ReuseRefused(f"the images could not be installed: {error}") from error
        raise


@contextlib.contextmanager
def _time_limit(deadline: float | None) -> Generator[None]:
    """Interrupt the work in the block with ReuseRefused once the
    time.monotonic() value `deadline` passes.

    The request timeouts bound each wait for the network, but not a download
    that keeps trickling in or the checks after it, so on POSIX an interval
    timer raises wherever the main thread is. Elsewhere, only the request
    timeouts and the download's own deadline checks apply.
    """
    if deadline is None:
        yield
        return
    seconds = deadline - time.monotonic()
    if seconds <= 0:
        raise ReuseRefused("reuse ran out of time")
    if sys.platform == "win32":
        yield
        return
    if threading.current_thread() is not threading.main_thread():
        yield
        return

    def expire(_signal: int, _frame: FrameType | None) -> None:
        if armed:
            raise ReuseRefused("reuse ran out of time")

    armed = True
    previous = signal.signal(signal.SIGALRM, expire)
    signal.setitimer(signal.ITIMER_REAL, seconds)
    try:
        yield
    finally:
        # A signal that arrives once the block has ended must not raise in
        # place of the restoration of the previous handler.
        armed = False
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous)


def _sentence(text: str) -> str:
    text = text.rstrip(".")
    return f"{text[:1].upper()}{text[1:]}"


def _fallback(dry_run: bool) -> str:
    if dry_run:
        return (
            "a merge group or a push that merged this head would build the "
            "guest images that missed its cache"
        )
    return "the action builds the guest images that missed the cache"


def _series(names: Sequence[str]) -> str:
    if len(names) < 3:
        return " and ".join(names)
    return f"{', '.join(names[:-1])}, and {names[-1]}"


def _quoted(names: Sequence[str]) -> list[str]:
    return [f"`{name}`" for name in names]


def _heading(heading: str, components: Sequence[str]) -> str:
    return f"{heading} ({', '.join(components)})"


def _checked_artifacts(
    api: GitHubApi,
    request: ReuseRequest,
    staging: Path,
) -> str:
    """Select the latest run for the pull request's head, download into
    `staging` its record and the artifacts of the request's components, check
    the components' files, and describe them."""
    workflow = workflow_path(request.workflow_ref, request.repository)
    if request.head_sha is not None:
        head_sha = request.head_sha
    elif request.commit is not None:
        head_sha = merged_head(api, request.repository, request.commit)
    else:
        raise ReuseRefused("neither a merge commit nor a head was given")
    run = latest_pull_request_run(api, request.repository, workflow, head_sha)
    source = (
        f"[run {run.id}]({run.url}), the latest run of `{workflow}` for pull "
        f"request head `{head_sha}`"
    )
    record = retained_artifact(api, request.repository, run, request.record)
    archive = staging / f"{record.name}.zip"
    _download(api, request.repository, record, archive)
    keys, digests = _read_record(archive, record.name)
    archive.unlink()
    differences = _key_differences(keys, request.keys)
    if differences:
        raise ReuseRefused(
            f"the `{record.name}` record of {source} gives other cache keys for "
            f"{', '.join(differences)}",
            expected=True,
        )
    files = component_files(request.components)
    if set(digests) != set(files):
        raise ReuseRefused(
            f"the `{record.name}` record of {source} describes other files than "
            f"those of {_series(request.components)}"
        )
    names = [
        name
        for component in request.components
        for name in COMPONENT_ARTIFACTS[component]
    ]
    extracted: set[str] = set()
    for name in names:
        artifact = retained_artifact(api, request.repository, run, name)
        archive = staging / f"{artifact.name}.zip"
        _download(api, request.repository, artifact, archive)
        _extract(archive, artifact.name, staging, digests, extracted)
        archive.unlink()
    missing = [name for name in files if name not in extracted]
    if missing:
        raise ReuseRefused(
            f"no artifact of {source} holds {_series(missing)}",
        )
    try:
        validate_guest_artifacts(staging, request.components)
    except ReuseRefused:
        raise
    except (ScriptError, OSError) as error:
        raise ReuseRefused(
            f"the files of {source} do not match their provenance or this "
            f"checkout: {error}"
        ) from error
    return f"the {_series(_quoted(names))} artifacts of {source}"


def reuse_guest_artifacts(
    api: GitHubApi,
    request: ReuseRequest,
    destination: Path,
    *,
    deadline: float | None = None,
) -> ReuseOutcome:
    """Install the files of the request's components from the artifacts of
    the latest run for the pull request's head when `destination` lacks any
    of them, or explain why not, giving up on the artifacts at the
    time.monotonic() value `deadline`.

    A cache restore that misses leaves its files absent, but one that fails
    partway can leave a truncated file and report a miss too, so reuse
    replaces every file of the components with its checked copy rather than
    only the absent ones; the files that the cache did restore have the same
    keys. A rehearsal checks the artifacts in the same way but installs
    nothing.
    """
    heading = _heading(_REHEARSAL if request.dry_run else _REUSE, request.components)
    files = component_files(request.components)
    absent = [name for name in files if not (destination / name).exists()]
    if not absent and not request.dry_run:
        return ReuseOutcome(
            False,
            heading,
            "not needed",
            "The cache restored every file of these components.",
        )
    try:
        destination.mkdir(parents=True, exist_ok=True)
        # Beside the destination, so that installing a file renames it.
        staging = Path(
            tempfile.mkdtemp(prefix=".guest-artifact-reuse-", dir=destination)
        )
        try:
            with _time_limit(deadline):
                source = _checked_artifacts(api, request, staging)
            if request.dry_run:
                return ReuseOutcome(
                    False,
                    heading,
                    "would reuse",
                    "A merge group or a push that merged this head would "
                    "install every file of these components from "
                    f"{source}: the cache keys that its `{request.record}` "
                    "record gives, the SHA-256 of each file, and their "
                    "provenance check out.",
                )
            # Outside the time limit, whose timer must not cut a rollback short.
            _install(staging, destination, files)
        finally:
            shutil.rmtree(staging, ignore_errors=True)
    except (ScriptError, OSError) as error:
        expected = isinstance(error, ReuseRefused) and error.expected
        return ReuseOutcome(
            False,
            heading,
            "build",
            f"{_sentence(str(error))}, so {_fallback(request.dry_run)}.",
            "notice" if expected and not request.dry_run else "warning",
        )
    return ReuseOutcome(
        True,
        heading,
        "reused",
        f"Installed every file of these components from {source}: the cache "
        f"keys that its `{request.record}` record gives, the SHA-256 of each "
        "file, and their provenance check out.",
    )


def _escape_annotation(text: str, *, property_value: bool = False) -> str:
    text = text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
    if property_value:
        text = text.replace(":", "%3A").replace(",", "%2C")
    return text


def report(outcome: ReuseOutcome, environment: Mapping[str, str]) -> None:
    """Print `outcome`, annotate a fallback to building, and give CI the
    `reused` step output and the job summary."""
    print(f">> {outcome.heading}: {outcome.status}. {outcome.detail}")
    if outcome.level is not None:
        title = _escape_annotation(outcome.heading, property_value=True)
        print(f"::{outcome.level} title={title}::{_escape_annotation(outcome.detail)}")
    output = environment.get("GITHUB_OUTPUT")
    if output:
        with Path(output).open("a", encoding="utf-8", newline="\n") as stream:
            stream.write(f"reused={'true' if outcome.reused else 'false'}\n")
    summary = environment.get("GITHUB_STEP_SUMMARY")
    if summary:
        with Path(summary).open("a", encoding="utf-8", newline="\n") as stream:
            stream.write(outcome.summary())


def reuse_in_ci(
    *,
    repository: str,
    workflow_ref: str,
    record: str,
    components: str,
    keys: str,
    commit: str | None,
    head_sha: str | None,
    dry_run: bool,
    destination: Path,
    environment: Mapping[str, str],
) -> ReuseOutcome:
    """Reuse the guest artifact `components`, as the action's components
    input lists them, with the token in GH_TOKEN or GITHUB_TOKEN, giving up on
    the artifacts after REUSE_TIMEOUT_SECONDS, and report the outcome; every
    failure falls back to building."""
    token = environment.get("GH_TOKEN") or environment.get("GITHUB_TOKEN")
    deadline = time.monotonic() + REUSE_TIMEOUT_SECONDS
    heading = _REHEARSAL if dry_run else _REUSE
    try:
        selected = parse_components(components)
        heading = _heading(heading, selected)
        if not token:
            raise ScriptError("no GitHub token is in GH_TOKEN or GITHUB_TOKEN")
        request = ReuseRequest(
            repository=repository,
            workflow_ref=workflow_ref,
            record=record,
            components=selected,
            keys=component_keys(selected, parse_cache_keys(keys)),
            commit=commit,
            head_sha=head_sha,
            dry_run=dry_run,
        )
    except ScriptError as error:
        outcome = ReuseOutcome(
            False,
            heading,
            "build",
            f"{_sentence(str(error))}, so {_fallback(dry_run)}.",
            "warning",
        )
    else:
        outcome = reuse_guest_artifacts(
            RestApi(token, deadline), request, destination, deadline=deadline
        )
    report(outcome, environment)
    return outcome
