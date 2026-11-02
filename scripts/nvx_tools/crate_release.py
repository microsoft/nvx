"""Publication of NVX's crates to crates.io by CI's full runs of dev.

CRATES lists the crates that CI publishes, each after the crates that it
depends on, all with one version, a pre-release of the form
``X.Y.Z-LABEL.N``. A run publishes a new version whenever the crates'
directories, or their workspace's manifest or lockfile, changed since the
commit from which the newest version on crates.io was packed: the next number
of the series that the crates declare, and at least the declared number. A
crate requires each of the others by path and at exactly the declared
version. The new version exists only in the run's checkout, which gives it to
the crates and their requirements on each other, so each package's
``.cargo_vcs_info.json`` records the run's commit, as dirty when the version
differs from the declared one.

Before the upload, each package must hold the commit's files byte for byte,
but for the versions, with a manifest and a lockfile that Cargo generates from
the workspace's, which must mean what the commit's do. The packages must build
with every feature, ``bundled`` included, which downloads and verifies the
release that ``artifacts.json`` pins. The package job checks and builds the
packages without the right to publish them, so that no build script or
procedural macro can publish either. The publish job builds nothing: it packs
the same bytes again, and uploads each package after those that it depends
on, which crates.io must serve before Cargo packs it alone, and then
crates.io must serve those very bytes.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import sys
import tarfile
import time
import urllib.error
import urllib.request
from collections.abc import Callable, Collection, Iterable, Iterator, Mapping, Sequence
from copy import deepcopy
from dataclasses import asdict, dataclass, field
from dataclasses import fields as dataclass_fields
from http.client import HTTPException
from pathlib import Path, PurePosixPath
from typing import cast

from .cargo_workspace import DEPENDENCIES, Table, read_toml, table, workspace_root
from .common import (
    CommandResult,
    ScriptError,
    require_success,
    run_capture,
    run_checked,
    sha256_file,
)

INDEX_URL = "https://index.crates.io"
DOWNLOAD_URL = "https://static.crates.io/crates"
CRATE_PAGE_URL = "https://crates.io/crates"
CRATES_IO_SOURCE = "registry+https://github.com/rust-lang/crates.io-index"
USER_AGENT = "nvx-crate-release (+https://github.com/microsoft/nvx)"

FETCH_ATTEMPTS = 3
FETCH_TIMEOUT_SECONDS = 60.0
FETCH_RETRY_SECONDS = 10.0
# crates.io lists a new version in its index within seconds, and its CDN
# caches index files for at most ten minutes.
INDEX_WAIT_SECONDS = 900.0
INDEX_POLL_SECONDS = 15.0

# Files that Cargo writes into every package instead of copying them. The
# manifest and the lockfile, which it generates from the workspace's, are
# checked by meaning.
GENERATED_FILES = frozenset({".cargo_vcs_info.json", "Cargo.toml", "Cargo.lock"})
# The package's copy of the crate's manifest, whose versions a run that
# publishes another version than the declared one rewrites.
VERSIONED_FILES = {"Cargo.toml.orig": "Cargo.toml"}
# Files without which no package builds, documents, or licenses itself.
REQUIRED_FILES = (
    ".cargo_vcs_info.json",
    "Cargo.toml",
    "Cargo.toml.orig",
    "Cargo.lock",
    "LICENSE",
    "README.md",
)
# Environment variables that would make `bundled` stage something else than
# the release that artifacts.json pins.
BUNDLE_VARIABLES = ("NVX_BUNDLE", "NVX_BUNDLE_DIR", "NVX_BUNDLE_HYPERVISOR")
# The state that the package step hands to the publish step.
STATE_FILE = "state.json"
# The most problems that a failed package check lists.
MAXIMUM_REPORTED_PROBLEMS = 20


@dataclass(frozen=True)
class Crate:
    """A crate that CI publishes: its directory in the checkout, and the files
    beyond REQUIRED_FILES without which its package does not build."""

    directory: str
    required: tuple[str, ...] = ()


# The crates that CI publishes, with one version, each after the crates that
# it depends on.
CRATES = (Crate("nvx-rs", ("build.rs", "artifacts.json", "src/lib.rs")),)

_NUMERIC = r"(?:0|[1-9][0-9]*)"
_IDENTIFIER = rf"(?:{_NUMERIC}|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
_SEMVER = re.compile(
    rf"({_NUMERIC})\.({_NUMERIC})\.({_NUMERIC})"
    rf"(?:-({_IDENTIFIER}(?:\.{_IDENTIFIER})*))?"
    r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
)
_SHA256 = re.compile(r"[0-9a-f]{64}")
_COMMIT = re.compile(r"[0-9a-f]{40}|[0-9a-f]{64}")


@dataclass(frozen=True)
class Version:
    """A SemVer 2.0 version, as Cargo and crates.io read it."""

    major: int
    minor: int
    patch: int
    prerelease: tuple[str, ...] = ()
    build: str = ""

    @classmethod
    def parse(cls, text: str) -> Version:
        match = _SEMVER.fullmatch(text)
        if match is None:
            raise ScriptError(f"{text!r} is not a SemVer version")
        major, minor, patch, prerelease, build = match.groups()
        return cls(
            int(major),
            int(minor),
            int(patch),
            tuple(prerelease.split(".")) if prerelease else (),
            build or "",
        )

    def __str__(self) -> str:
        text = f"{self.major}.{self.minor}.{self.patch}"
        if self.prerelease:
            text += "-" + ".".join(self.prerelease)
        if self.build:
            text += "+" + self.build
        return text

    @property
    def core(self) -> tuple[int, int, int]:
        return self.major, self.minor, self.patch

    @property
    def precedence(
        self,
    ) -> tuple[int, int, int, int, tuple[tuple[int, int, str], ...]]:
        """The key that orders versions by SemVer precedence, which ignores
        build metadata: numeric identifiers sort numerically and before
        alphanumeric ones, and a release sorts after its pre-releases."""
        identifiers = tuple(
            (0, int(identifier), "") if identifier.isdigit() else (1, 0, identifier)
            for identifier in self.prerelease
        )
        return (*self.core, 0 if self.prerelease else 1, identifiers)

    @property
    def counter(self) -> tuple[str, int] | None:
        """The label and number of a pre-release ``X.Y.Z-LABEL.N``, or None."""
        if len(self.prerelease) != 2:
            return None
        label, number = self.prerelease
        if label.isdigit() or not number.isdigit():
            return None
        return label, int(number)


def next_version(declared: Version, published: Iterable[Version]) -> Version:
    """Return the version to publish: the number after the highest that
    crates.io has in the declared series, yanked versions included, since
    crates.io never reuses one, and at least the declared number."""
    counter = declared.counter
    if counter is None or declared.build:
        raise ScriptError(
            f"the crate declares version {declared}, but CI publishes only "
            "pre-releases of the form X.Y.Z-LABEL.N, such as 0.1.0-alpha.3"
        )
    label, number = counter
    versions = tuple(published)
    for version in versions:
        if version.core != declared.core:
            continue
        published_counter = version.counter
        if published_counter is not None and published_counter[0] == label:
            number = max(number, published_counter[1] + 1)
    candidate = Version(*declared.core, (label, str(number)))
    newest = max(versions, key=lambda version: version.precedence, default=None)
    if newest is not None and candidate.precedence <= newest.precedence:
        raise ScriptError(
            f"{candidate}, the next version of the declared {declared}, would "
            f"not be newer than {newest}, which crates.io has: declare a newer "
            "version in the crate's Cargo.toml"
        )
    return candidate


@dataclass(frozen=True)
class Manifest:
    """The name and version of a crate that CI publishes."""

    name: str
    version: Version


def _options(member: Table, specification: Table) -> str:
    """Return what the dependency `member` asks of its crate, as Cargo merges
    it with `specification`, the workspace's dependency that it inherits, or
    else itself: whether it is optional, as `member` says; whether it takes
    the crate's default features, which `member` may turn on, or else
    `specification` turn off; and the features that either names."""
    both = (member, specification)
    defaults = [t.get("default-features", t.get("default_features")) for t in both]
    features = {f for t in both for f in cast(list[str], t.get("features") or [])}
    default = defaults[0] is True or defaults[1] is not False
    optional = member.get("optional") is True
    return f"optional={optional} default-features={default} features={sorted(features)}"


def _requirements(
    manifest: Table, names: Collection[str], inherited: Table
) -> Iterator[tuple[str, Table, tuple[str, ...]]]:
    """Yield the crate, the specification, and the place of each dependency of
    `manifest` on one of the crates `names`, as it inherits it from the
    workspace's dependencies `inherited`: an empty specification for a
    requirement alone, and a place that names the platform that the dependency
    is for, if any, its kind, its key, the crate, and its _options()."""
    for platform, tables in [("", manifest), *table(manifest.get("target")).items()]:
        for kind in DEPENDENCIES:
            for key, value in table(table(tables).get(kind)).items():
                member = specification = table(value)
                if member.get("workspace") is True:
                    specification = table(inherited.get(key))
                name = specification.get("package", key)
                if isinstance(name, str) and name in names:
                    place = (platform, kind.replace("_", "-"), key, name)
                    yield name, specification, (*place, _options(member, specification))


def _give_version(
    manifest: Table, names: Collection[str], declared: Version, version: Version
) -> Table:
    """Return a copy of `manifest` with `version` instead of `declared` in its
    package's version and its workspace's, and in its own and its workspace's
    exact requirements on the crates `names`."""
    copy = deepcopy(manifest)
    workspace = table(copy.get("workspace"))
    inherited = table(workspace.get("dependencies"))
    for owner in (table(copy.get("package")), table(workspace.get("package"))):
        if owner.get("version") == str(declared):
            owner["version"] = str(version)
    workspace_dependencies: Table = {"dependencies": inherited}
    for dependencies in (copy, workspace_dependencies):
        for _, specification, _ in _requirements(dependencies, names, inherited):
            if specification.get("version") == f"={declared}":
                specification["version"] = f"={version}"
    return copy


def rewrite_manifest(
    data: bytes,
    source: str,
    names: Collection[str],
    declared: Version,
    version: Version,
) -> bytes:
    """Return a manifest that declares `version` wherever _give_version()
    puts it, every other byte unchanged: each version or exact requirement of
    `declared` that _give_version() changes, and no other, such as a version in
    the manifest's metadata."""
    text = data.decode("utf-8")
    original = read_toml(text, source)
    expected = _give_version(original, names, declared, version)
    pattern = rf"""(\bversion[ \t]*=[ \t]*)(["'])(=?){re.escape(str(declared))}\2"""
    rewritten = text
    for match in reversed([*re.finditer(pattern, text)]):
        new = f"{match[1]}{match[2]}{match[3]}{version}{match[2]}"
        read = read_toml(text[: match.start()] + new + text[match.end() :], source)
        given = _give_version(read, names, declared, version)
        if read != original and given == expected:
            rewritten = rewritten[: match.start()] + new + rewritten[match.end() :]
    if read_toml(rewritten, source) != expected:
        raise ScriptError(
            f"rewriting {declared} as {version} in {source} changes other values "
            "than the versions of the crates that CI publishes, or misses one"
        )
    return rewritten.encode("utf-8")


def rewrite_lockfile(
    data: bytes, source: str, name: str, declared: Version, version: Version
) -> bytes:
    """Return a lockfile in which the package `name` has `version` instead of
    `declared`, every other byte unchanged. Checkout.open() makes sure that
    the lockfile holds no other package of the name, so the references to it
    name it alone."""
    entry = re.compile(
        rf'^(\[\[package\]\]\nname = "{re.escape(name)}"\nversion = ")'
        rf'{re.escape(str(declared))}("\n)',
        re.MULTILINE,
    )
    rewritten, count = entry.subn(
        lambda match: f"{match.group(1)}{version}{match.group(2)}",
        data.decode("utf-8"),
    )
    if count != 1:
        raise ScriptError(f"{source} holds no single entry of {name} {declared}")
    return rewritten.encode("utf-8")


def versioned_file(
    path: str, data: bytes, names: Collection[str], declared: Version, version: Version
) -> bytes:
    """Return the checkout's manifest or lockfile `path` as a run that
    publishes `version` of the crates `names` rewrites it."""
    if version == declared:
        return data
    if PurePosixPath(path).name == "Cargo.toml":
        return rewrite_manifest(data, path, names, declared, version)
    for name in names:
        data = rewrite_lockfile(data, path, name, declared, version)
    return data


def git_blob_id(data: bytes, commit: str) -> str:
    """Return the ID of a Git blob that holds `data`, in the object format
    of `commit`."""
    digest = hashlib.new("sha1" if len(commit) == 40 else "sha256")
    digest.update(b"blob %d\0" % len(data))
    digest.update(data)
    return digest.hexdigest()


class Repository:
    """The Git checkout that holds the crates, at its root."""

    def __init__(self, root: Path) -> None:
        self.root = root
        if self._text("locating the checkout", "rev-parse", "--show-prefix"):
            raise ScriptError(f"{root} is not the root of its Git checkout")

    def _git(self, *arguments: str) -> CommandResult:
        return run_capture(["git", "-C", self.root, *arguments])

    def _text(self, label: str, *arguments: str) -> str:
        result = self._git(*arguments)
        require_success(result, f"git {label}")
        return result.stdout.decode("utf-8").strip()

    def head(self) -> str:
        return self._text("reading HEAD", "rev-parse", "--verify", "HEAD^{commit}")

    def has_commit(self, revision: str) -> bool:
        return self._git("cat-file", "-e", f"{revision}^{{commit}}").returncode == 0

    def _predicate(self, label: str, *arguments: str) -> bool:
        result = self._git(*arguments)
        if result.returncode not in (0, 1):
            require_success(result, f"git {label}")
        return result.returncode == 0

    def contains(self, ancestor: str, descendant: str) -> bool:
        """Return whether `descendant` is `ancestor` or one of its descendants."""
        return self._predicate(
            "comparing commits", "merge-base", "--is-ancestor", ancestor, descendant
        )

    def changed(self, base: str, head: str, paths: Sequence[str]) -> bool:
        """Return whether `paths` differ between two commits."""
        return not self._predicate(
            "comparing the crates' files",
            "diff",
            "--quiet",
            "--no-ext-diff",
            "--no-textconv",
            base,
            head,
            "--",
            *paths,
        )

    def status(self, paths: Sequence[str]) -> list[str]:
        """Return the changed and untracked files of `paths`, as
        `git status --porcelain` lists them."""
        result = self._git(
            "status", "--porcelain=v1", "-z", "--untracked-files=all", "--", *paths
        )
        require_success(result, "git status")
        return [entry for entry in result.stdout.decode("utf-8").split("\0") if entry]

    def tree(self, commit: str, directory: str) -> dict[str, str]:
        """Map each file of `directory` in `commit` to its blob."""
        tree = f"{commit}:{directory}"
        result = self._git("ls-tree", "--full-tree", "-r", "-z", tree)
        require_success(result, f"listing the crate's files in {commit}")
        files: dict[str, str] = {}
        for entry in result.stdout.split(b"\0"):
            if not entry:
                continue
            metadata, _, path = entry.partition(b"\t")
            _mode, kind, object_id = metadata.decode("ascii").split(" ")
            if kind == "blob":
                files[path.decode("utf-8")] = object_id
        return files

    def blob(self, object_id: str) -> bytes:
        result = self._git("cat-file", "blob", object_id)
        require_success(result, f"reading blob {object_id}")
        return result.stdout

    def file(self, commit: str, path: str) -> bytes:
        return self.blob(f"{commit}:{path}")


def require_checkout(
    repository: Repository, paths: Sequence[str], modified: Iterable[str]
) -> None:
    """Fail unless `paths` differ from their commit in exactly the `modified`
    files, which the working tree changed."""
    expected = sorted(f" M {path}" for path in modified)
    actual = sorted(repository.status(paths))
    if actual != expected:
        changes = "\n".join(actual) or "(none)"
        wanted = "\n".join(expected) or "(none)"
        raise ScriptError(
            f"the checkout of {', '.join(paths)} has these changes:\n"
            f"{changes}\nbut publication expects these:\n{wanted}"
        )


def fetch_url(
    url: str,
    *,
    missing_codes: Collection[int] = (404,),
    attempts: int = FETCH_ATTEMPTS,
    timeout_seconds: float = FETCH_TIMEOUT_SECONDS,
    retry_seconds: float = FETCH_RETRY_SECONDS,
    sleep: Callable[[float], None] = time.sleep,
) -> bytes | None:
    """Return the body of `url`, or None when the server answers with one of
    `missing_codes`, which say that it has no such file. Other client errors
    fail at once, and server errors, timeouts, and rate limits are retried."""
    request = urllib.request.Request(
        url, headers={"User-Agent": USER_AGENT, "Cache-Control": "no-cache"}
    )
    for attempt in range(1, attempts + 1):
        try:
            with urllib.request.urlopen(request, timeout=timeout_seconds) as response:
                body: bytes = response.read()
                return body
        except urllib.error.HTTPError as error:
            error.close()
            if error.code in missing_codes:
                return None
            failure = f"HTTP {error.code}"
            if error.code < 500 and error.code not in (408, 429):
                raise ScriptError(f"GET {url} failed: {failure}") from error
        except (OSError, HTTPException) as error:
            failure = str(error) or type(error).__name__
        if attempt == attempts:
            raise ScriptError(f"GET {url} failed {attempts} times: {failure}")
        print(f">> GET {url} failed ({attempt}/{attempts}); retrying: {failure}")
        sleep(retry_seconds)
    raise ScriptError(f"GET {url} was not attempted")


def index_path(name: str) -> str:
    """Return the path of a crate's file in the crates.io index."""
    name = name.lower()
    if len(name) <= 2:
        return f"{len(name)}/{name}"
    if len(name) == 3:
        return f"3/{name[0]}/{name}"
    return f"{name[:2]}/{name[2:4]}/{name}"


@dataclass(frozen=True)
class Release:
    """A version of a crate as the crates.io index lists it."""

    version: Version
    checksum: str
    yanked: bool


def parse_index(name: str, data: bytes) -> list[Release]:
    releases: list[Release] = []
    for number, line in enumerate(data.decode("utf-8").splitlines(), 1):
        if not line.strip():
            continue
        try:
            value: object = json.loads(line)
        except json.JSONDecodeError as error:
            raise ScriptError(
                f"line {number} of the index of {name} is not JSON: {error}"
            ) from error
        entry = cast(dict[str, object], value) if isinstance(value, dict) else {}
        entry_name = entry.get("name")
        text = entry.get("vers")
        checksum = entry.get("cksum")
        yanked = entry.get("yanked")
        if (
            not isinstance(entry_name, str)
            or entry_name.lower() != name.lower()
            or not isinstance(text, str)
            or not isinstance(checksum, str)
            or _SHA256.fullmatch(checksum) is None
            or not isinstance(yanked, bool)
        ):
            raise ScriptError(f"line {number} of the index of {name} is not a version")
        releases.append(Release(Version.parse(text), checksum, yanked))
    return releases


def fetch_package(url: str) -> bytes | None:
    """Fetch from the download server, which answers 403 for a package that it
    does not hold."""
    return fetch_url(url, missing_codes=(403, 404))


class Registry:
    """The crates.io index and download server. A fake's `fetch` serves both."""

    def __init__(
        self,
        fetch: Callable[[str], bytes | None] | None = None,
        *,
        sleep: Callable[[float], None] = time.sleep,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        # The index answers 404 for a crate that it does not list; any other
        # client error fails, so that it never passes for an empty index.
        self._fetch_index = fetch_url if fetch is None else fetch
        self._fetch_package = fetch_package if fetch is None else fetch
        self._sleep = sleep
        self._clock = clock

    def releases(self, name: str) -> list[Release]:
        data = self._fetch_index(f"{INDEX_URL}/{index_path(name)}")
        return [] if data is None else parse_index(name, data)

    def download(
        self,
        name: str,
        release: Release,
        destination: Path,
        *,
        attempts: int = FETCH_ATTEMPTS,
    ) -> Path:
        """Download a package, which must match the index's checksum. A
        package that crates.io published moments ago may take a few seconds
        to reach the download server."""
        url = f"{DOWNLOAD_URL}/{name}/{name}-{release.version}.crate"
        for attempt in range(1, attempts + 1):
            data = self._fetch_package(url)
            if data is not None:
                actual = hashlib.sha256(data).hexdigest()
                if actual != release.checksum:
                    raise ScriptError(
                        f"{url} has SHA-256 {actual}, but the crates.io index "
                        f"lists {release.checksum}"
                    )
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(data)
                return destination
            if attempt < attempts:
                print(f">> {url} is not available yet ({attempt}/{attempts})")
                self._sleep(FETCH_RETRY_SECONDS)
        raise ScriptError(f"crates.io serves no {url}")

    def wait_for(
        self,
        name: str,
        version: Version,
        *,
        timeout_seconds: float = INDEX_WAIT_SECONDS,
        interval_seconds: float = INDEX_POLL_SECONDS,
    ) -> Release:
        deadline = self._clock() + timeout_seconds
        while True:
            for release in self.releases(name):
                if release.version == version:
                    return release
            if self._clock() >= deadline:
                raise ScriptError(
                    f"the crates.io index did not list {name} {version} within "
                    f"{timeout_seconds:.0f} s"
                )
            print(f">> waiting for the crates.io index to list {name} {version}")
            self._sleep(interval_seconds)


@dataclass(frozen=True)
class VcsInfo:
    """What a package's .cargo_vcs_info.json records."""

    commit: str
    dirty: bool
    path_in_vcs: str


def read_crate(path: Path, root: str) -> dict[str, bytes]:
    """Return the files of a package by their paths beneath its root,
    `NAME-VERSION`, which must hold every one of its members."""
    files: dict[str, bytes] = {}
    try:
        with tarfile.open(path, "r:gz") as archive:
            for member in archive:
                parts = PurePosixPath(member.name).parts
                if (
                    not member.isfile()
                    or len(parts) < 2
                    or parts[0] != root
                    or ".." in parts
                ):
                    raise ScriptError(
                        f"{path.name} holds {member.name!r}, which is not a file "
                        f"beneath {root}/"
                    )
                relative = "/".join(parts[1:])
                stream = archive.extractfile(member)
                if relative in files or stream is None:
                    raise ScriptError(f"{path.name} holds {relative} twice")
                files[relative] = stream.read()
    except (OSError, EOFError, tarfile.TarError) as error:
        raise ScriptError(f"cannot read {path}: {error}") from error
    return files


def read_vcs_info(files: Mapping[str, bytes], source: str) -> VcsInfo | None:
    data = files.get(".cargo_vcs_info.json")
    if data is None:
        return None
    try:
        value: object = json.loads(data.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ScriptError(
            f"{source}: .cargo_vcs_info.json is invalid: {error}"
        ) from error
    document = cast(dict[str, object], value) if isinstance(value, dict) else {}
    git_value = document.get("git")
    git = cast(dict[str, object], git_value) if isinstance(git_value, dict) else {}
    commit = git.get("sha1")
    dirty = git.get("dirty", False)
    path_in_vcs = document.get("path_in_vcs", "")
    if (
        not isinstance(commit, str)
        or _COMMIT.fullmatch(commit) is None
        or not isinstance(dirty, bool)
        or not isinstance(path_in_vcs, str)
    ):
        raise ScriptError(f"{source}: .cargo_vcs_info.json records no Git commit")
    return VcsInfo(commit, dirty, path_in_vcs)


def _dependencies(
    entry: Table,
    keys: Sequence[tuple[object, ...]],
    run: Collection[tuple[object, ...]],
) -> set[tuple[object, ...]]:
    """Return the name, version, and source of each package that a lockfile's
    `entry` depends on, among the packages of the lockfile, `keys`: a reference
    names the package, then gives its version if the lockfile holds another of
    that name, and then its source if it holds another of that version too.
    One to a package of the run, whose name and version `run` holds, is to it
    from the checkout, from crates.io or not."""
    found: set[tuple[object, ...]] = set()
    references = entry.get("dependencies")
    references = cast(list[object], references) if isinstance(references, list) else []
    for reference in references:
        parts = str(reference).split(" ", 2)
        named = [
            key for key in keys if [*key[:2], f"({key[2]})"][: len(parts)] == parts
        ]
        crate, number, source = named[0] if len(named) == 1 else (reference, 0, 0)
        found.add((crate, number, None if (crate, number) in run else source))
    return found


def _lockfile_problems(
    data: bytes,
    workspace: Table,
    name: str,
    version: Version,
    checksums: Mapping[str, str],
) -> list[str]:
    """Return how the lockfile that Cargo generated for a package of `name`
    `version`, by pruning the workspace's lockfile to the package's
    dependencies, differs in meaning: it locks the run's packages of the other
    crates, of `version`, as crates.io's, with the SHA-256 digests
    `checksums`."""
    try:
        lockfile = read_toml(data, "Cargo.lock")
    except ScriptError as error:
        return [str(error)]
    problems: list[str] = []
    if (found := lockfile.get("version")) != (wanted := workspace.get("version")):
        problems.append(f"Cargo.lock has format {found}, not {wanted}")
    entries: dict[tuple[object, ...], Table] = {}
    keys: dict[bool, list[tuple[object, ...]]] = {False: [], True: []}
    # The crate packed is the root, from its path; Cargo locks the run's other
    # crates as crates.io's packages.
    run = {(crate, str(version)) for crate in checksums if crate != name}
    for document, packed in ((workspace, False), (lockfile, True)):
        packages = document.get("package")
        for value in cast(list[object], packages) if isinstance(packages, list) else []:
            entry = table(value)
            key = (entry.get("name"), entry.get("version"), entry.get("source"))
            keys[packed].append(key)
            entries[(*key, packed)] = entry
    for (crate, number, source, packed), entry in entries.items():
        if not packed:
            continue
        expected = entries.get((crate, number, source, False))
        if (crate, number) in run:
            # Cargo locks a crate of the run as the package that it packed.
            expected = entries.get((crate, number, None, False))
            if source != CRATES_IO_SOURCE:
                expected = None
            checksum = checksums[str(crate)]
        else:
            checksum = None if expected is None else expected.get("checksum")
        if expected is None or entry.get("checksum") != checksum:
            problems.append(
                f"Cargo.lock locks {crate} {number} from {source or 'its path'}, "
                "unlike the workspace's lockfile and this run's packages"
            )
        elif not _dependencies(entry, keys[True], run) <= _dependencies(
            expected, keys[False], run
        ):
            problems.append(
                f"Cargo.lock gives {crate} {number} dependencies that the "
                "workspace's lockfile does not"
            )
    if (name, str(version), None, True) not in entries:
        problems.append(f"Cargo.lock does not lock {name} {version}")
    return problems


def check_package(
    files: Mapping[str, bytes],
    *,
    source: str,
    checkout: Checkout,
    name: str,
    version: Version,
    checksums: Mapping[str, str],
) -> None:
    """Fail unless a package of `name` `version` was packed from the
    checkout's commit, with the run's packages of the other crates, whose
    SHA-256 digests are `checksums`: it records the commit, as dirty exactly
    when the version is not the declared one, and it holds the commit's files
    byte for byte, but for the rewritten version and the files that Cargo
    generates, whose meaning the workspace's manifest and lockfile give."""
    repository, commit, names = checkout.repository, checkout.commit, checkout.names
    declared = checkout.declared
    crate = checkout.crates[names.index(name)]
    problems: list[str] = []
    vcs = read_vcs_info(files, source)
    dirty = version != declared
    if vcs is None:
        problems.append(".cargo_vcs_info.json is missing")
    else:
        if vcs.commit != commit:
            problems.append(f".cargo_vcs_info.json records {vcs.commit}, not {commit}")
        if vcs.path_in_vcs != crate.directory:
            problems.append(
                f".cargo_vcs_info.json records the path {vcs.path_in_vcs!r}, not "
                f"{crate.directory!r}"
            )
        if vcs.dirty != dirty:
            problems.append(
                f".cargo_vcs_info.json records a {'dirty' if vcs.dirty else 'clean'} "
                f"checkout, but one that declares {declared} packs {version} from "
                f"a {'dirty' if dirty else 'clean'} one"
            )
    problems.extend(
        f"{path} is missing"
        for path in (*REQUIRED_FILES, *crate.required)
        if path not in files
    )
    tree = repository.tree(commit, crate.directory)
    for path, data in sorted(files.items()):
        if path in GENERATED_FILES:
            if b"\r" in data:
                problems.append(f"{path} holds CR line endings")
            continue
        original_path = VERSIONED_FILES.get(path, path)
        object_id = tree.get(original_path)
        if object_id is None:
            problems.append(f"{path} is not a file of {commit}")
            continue
        if path in VERSIONED_FILES:
            original = repository.blob(object_id)
            try:
                expected = versioned_file(
                    original_path, original, names, declared, version
                )
            except ScriptError as error:
                problems.append(str(error))
                continue
            matches = data == expected
        else:
            matches = git_blob_id(data, commit) == object_id
        if not matches:
            endings = ", and holds CR line endings" if b"\r" in data else ""
            problems.append(f"{path} differs from {original_path} in {commit}{endings}")
    manifest_data = files.get("Cargo.toml")
    if manifest_data is not None:
        try:
            manifest = read_toml(manifest_data, f"{source}: Cargo.toml")
        except (ScriptError, UnicodeDecodeError) as error:
            problems.append(str(error))
        else:
            package = table(manifest.get("package"))
            if [package.get("name"), package.get("version")] != [name, str(version)]:
                problems.append(
                    f"Cargo.toml declares {package.get('name')} "
                    f"{package.get('version')}, not {name} {version}"
                )
            for other, specification, _ in _requirements(manifest, names, {}):
                exact = specification.get("version") == f"={version}"
                if not exact or "path" in specification:
                    problems.append(
                        f"Cargo.toml requires {other} as {specification}, not as "
                        f"={version} from crates.io"
                    )
            given = sorted(place for *_, place in _requirements(manifest, names, {}))
            if given != (wanted := checkout.requirements[name]):
                problems.append(f"Cargo.toml requires the crates {given}, not {wanted}")
    lockfile = files.get("Cargo.lock")
    if lockfile is not None:
        problems += _lockfile_problems(
            lockfile, checkout.lockfile_table(version), name, version, checksums
        )
    if problems:
        shown = problems[:MAXIMUM_REPORTED_PROBLEMS]
        hidden = len(problems) - len(shown)
        more = f"\n... and {hidden} more" if hidden else ""
        raise ScriptError(
            f"{source} is not {name} {version} as packed from {commit}:\n"
            + "\n".join(shown)
            + more
        )
    print(
        f">> {source} holds the {len(files)} files of {name} {version} as packed "
        f"from {commit}"
    )


def crate_url(name: str, version: Version) -> str:
    return f"{CRATE_PAGE_URL}/{name}/{version}"


def cargo_package(
    checkout: Checkout,
    target_dir: Path,
    names: Sequence[str],
    version: Version,
    *,
    environment: Mapping[str, str],
    verify: bool = False,
) -> dict[str, Path]:
    """Pack the crates `names`, which have `version`, with Cargo, without
    building them unless `verify`, and return the packages by crate."""
    command: list[str | os.PathLike[str]] = [
        "cargo",
        "package",
        "--locked",
        "--all-features" if verify else "--no-verify",
        "--manifest-path",
        checkout.repository.root / checkout.manifest,
        "--target-dir",
        target_dir,
    ]
    for name in names:
        command += ["--package", name]
    if version != checkout.declared:
        command.append("--allow-dirty")
    run_checked(command, env=environment)
    packages = {name: target_dir / f"package/{name}-{version}.crate" for name in names}
    for package in packages.values():
        if not package.is_file():
            raise ScriptError(f"cargo package wrote no {package}")
    return packages


def bundle_hypervisors(platform: str = sys.platform) -> tuple[str | None, ...]:
    """Return the values of NVX_BUNDLE_HYPERVISOR with which to build the
    package: Linux has a KVM and an MSHV package, and Windows one."""
    return ("kvm", "mshv") if platform.startswith("linux") else (None,)


def build_packages(
    checkout: Checkout,
    checksums: Mapping[str, str],
    version: Version,
    work_dir: Path,
    environment: Mapping[str, str],
    *,
    platform: str = sys.platform,
) -> None:
    """Build the packages with every feature, as dependents would: `cargo
    package` packs the same bytes again, and builds each package from its
    files, with the crates that it depends on from the other packages.
    `bundled` downloads the release that the package's artifacts.json pins,
    which must match its digest and declare the crate's control contract."""
    base = {
        key: value for key, value in environment.items() if key not in BUNDLE_VARIABLES
    }
    for hypervisor in bundle_hypervisors(platform):
        build_environment = dict(base)
        description = "with every feature"
        if hypervisor is not None:
            build_environment["NVX_BUNDLE_HYPERVISOR"] = hypervisor
            description += f", bundling the {hypervisor} release"
        print(f">> building {', '.join(checksums)} {version} {description}")
        target, names = work_dir / "build-target", tuple(checksums)
        packages = cargo_package(
            checkout, target, names, version, environment=build_environment, verify=True
        )
        for name, package in packages.items():
            if sha256_file(package) != checksums[name]:
                raise ScriptError(f"cargo built {package.name} from other bytes")


@dataclass(frozen=True)
class Plan:
    """Whether a run publishes, and the version that it publishes or, when it
    does not, the newest version on crates.io."""

    publish: bool
    version: Version
    reason: str
    # The packages of the version that crates.io holds of this commit
    # already, by crate, which a rerun checks instead of publishing them.
    published: Mapping[str, Path] = field(default_factory=dict[str, Path])


def _packed_from(
    repository: Repository,
    crate: Crate,
    manifest: Manifest,
    newest: Release,
    registry: Registry,
    work_dir: Path,
) -> tuple[str, Path]:
    """Return the commit from which a crate's newest version on crates.io was
    packed, and its package."""
    label = f"{manifest.name} {newest.version}"
    package = registry.download(
        manifest.name,
        newest,
        work_dir / "published" / f"{manifest.name}-{newest.version}.crate",
    )
    vcs = read_vcs_info(
        read_crate(package, f"{manifest.name}-{newest.version}"), package.name
    )
    if vcs is None:
        raise ScriptError(
            f"{label}, the newest version on crates.io, records no commit, so "
            "this run cannot tell which changes it holds: publish the next "
            "version by hand (doc/distribution.md, The nvx crate)"
        )
    if vcs.path_in_vcs != crate.directory:
        raise ScriptError(
            f"{label} was packed from {vcs.path_in_vcs!r}, not from {crate.directory!r}"
        )
    if not repository.has_commit(vcs.commit):
        raise ScriptError(
            f"{label} was packed from {vcs.commit}, which this checkout lacks: "
            "check out the full history, or publish the next version by hand "
            "(doc/distribution.md, The nvx crate)"
        )
    return vcs.commit, package


def plan_release(checkout: Checkout, registry: Registry, work_dir: Path) -> Plan:
    """Decide whether the checkout's commit needs a new version of the crates:
    it does when their files changed since the commit from which the newest
    version on crates.io was packed, unless that commit already contains it.
    A rerun of that commit publishes its version of the crates that crates.io
    lacks, and while crates.io lacks any, a later commit publishes the next
    version of them all, even if their files did not change."""
    repository, commit = checkout.repository, checkout.commit
    releases = [registry.releases(name) for name in checkout.names]
    versions = [release.version for found in releases for release in found]
    if not versions:
        reason = f"crates.io has no version of {', '.join(checkout.names)}"
        return Plan(True, next_version(checkout.declared, ()), reason)
    newest = max(versions, key=lambda version: version.precedence)
    # The crates whose newest version is the newest of all, and its commit.
    heads: dict[str, tuple[str, str, Path]] = {}
    for crate, manifest, found in zip(
        checkout.crates, checkout.manifests, releases, strict=True
    ):
        for release in found:
            if release.version == newest:
                heads[manifest.name] = (
                    f"{manifest.name} {newest}",
                    *_packed_from(
                        repository, crate, manifest, release, registry, work_dir
                    ),
                )
    here = {name: path for name, (_, base, path) in heads.items() if base == commit}
    missing = [name for name in checkout.names if name not in heads]
    lacks = f", but crates.io lacks {', '.join(missing)} {newest}" if missing else ""
    first_label, first_base, _package = next(iter(heads.values()))
    if here:
        for label, base, _package in heads.values():
            if base != commit:
                raise ScriptError(f"{label} was packed from {base}, not this commit")
        reason = f"{first_label} was packed from this commit{lacks}"
        return Plan(bool(missing), newest, reason, here)
    inputs = ", ".join(checkout.inputs)
    for label, base, _package in heads.values():
        if repository.contains(commit, base):
            reason = f"{label} was packed from {base[:12]}, which contains this commit"
            return Plan(False, newest, reason)
    for label, base, _package in heads.values():
        if repository.changed(base, commit, checkout.inputs):
            return Plan(
                True,
                next_version(checkout.declared, versions),
                f"{inputs} changed since {base[:12]}, from which {label} was packed",
            )
    if missing:
        reason = f"{first_label} was packed from {first_base[:12]}{lacks}"
        return Plan(True, next_version(checkout.declared, versions), reason)
    return Plan(
        False,
        newest,
        f"{inputs} did not change since {first_base[:12]}, from which "
        f"{first_label} was packed",
    )


@dataclass(frozen=True)
class PackageState:
    """A package that the package job checked and built, for the publish job."""

    name: str
    declared: str
    version: str
    commit: str
    sha256: str


def write_states(states: Iterable[PackageState]) -> str:
    """Return the JSON of `states` on one line, which read_states() reads."""
    return json.dumps([asdict(state) for state in states], separators=(",", ":"))


def read_states(text: str, source: str) -> list[PackageState]:
    try:
        value: object = json.loads(text)
    except json.JSONDecodeError as error:
        raise ScriptError(f"{source} is not JSON: {error}") from error
    fields = [field.name for field in dataclass_fields(PackageState)]
    states: list[PackageState] = []
    for entry in cast(list[object], value) if isinstance(value, list) else [None]:
        document = table(entry)
        values = {name: document.get(name) for name in fields}
        if not all(isinstance(value, str) for value in values.values()):
            raise ScriptError(f"{source} is not the state of checked packages")
        states.append(PackageState(**cast(dict[str, str], values)))
    return states


def _write_state(work_dir: Path, states: Sequence[PackageState]) -> None:
    work_dir.mkdir(parents=True, exist_ok=True)
    (work_dir / STATE_FILE).write_text(write_states(states) + "\n", encoding="utf-8")


def _read_state(work_dir: Path) -> list[PackageState]:
    path = work_dir / STATE_FILE
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise ScriptError(
            f"cannot read {path}, which the prepare step writes: {error}"
        ) from error
    return read_states(text, str(path))


def _set_output(environment: Mapping[str, str], name: str, value: str) -> None:
    path = environment.get("GITHUB_OUTPUT")
    if path:
        with Path(path).open("a", encoding="utf-8", newline="\n") as stream:
            stream.write(f"{name}={value}\n")


def _summarize(environment: Mapping[str, str], text: str) -> None:
    print(f">> {text}")
    path = environment.get("GITHUB_STEP_SUMMARY")
    if path:
        with Path(path).open("a", encoding="utf-8", newline="\n") as stream:
            stream.write(f"### nvx crate\n\n{text}\n")


@dataclass(frozen=True)
class Checkout:
    """The checkout of the crates that CI publishes, whose files a step finds
    unchanged from its commit but for the version that it gives the crates."""

    repository: Repository
    commit: str
    crates: tuple[Crate, ...]
    manifests: tuple[Manifest, ...]
    # The manifest and the lockfile of the crates' workspace.
    manifest: str
    lockfile: str
    # The files whose versions a run rewrites, as the commit holds them: the
    # crates' manifests, the workspace's, and its lockfile.
    originals: Mapping[str, bytes]
    # The paths whose changes publish a new version: the crates' directories,
    # and the workspace's manifest and lockfile, which they share.
    inputs: tuple[str, ...]
    # The places of each crate's requirements on the others, sorted: the
    # platform that each is for, if any, its kind, its key, the crate, and
    # what it asks of the crate, as _options() puts it.
    requirements: Mapping[str, Sequence[tuple[str, ...]]]
    # The workspace's packages that its lockfile locks from a path at the
    # crates' version: the crates, and others, which take it from the
    # workspace or, as the workspace's own package, declare it.
    members: tuple[str, ...]

    @property
    def names(self) -> tuple[str, ...]:
        return tuple(manifest.name for manifest in self.manifests)

    @property
    def declared(self) -> Version:
        return self.manifests[0].version

    @classmethod
    def open(cls, root: Path, crates: Sequence[Crate]) -> Checkout:
        """Read the crates' names, their version, and their requirements on
        each other in the commit of the checkout at `root`."""
        repository = Repository(root)
        commit = repository.head()
        workspaces = {workspace_root(root / crate.directory, root) for crate in crates}
        if len(workspaces) != 1:
            raise ScriptError("the crates that CI publishes must share one workspace")
        directory = workspaces.pop().relative_to(root).as_posix()
        prefix = "" if directory == "." else f"{directory}/"
        manifest, lockfile = f"{prefix}Cargo.toml", f"{prefix}Cargo.lock"
        paths = [f"{crate.directory}/Cargo.toml" for crate in crates]
        originals = {
            path: repository.file(commit, path)
            for path in dict.fromkeys([*paths, manifest, lockfile])
        }
        documents = {
            path: read_toml(originals[path], f"{path} in {commit}")
            for path in dict.fromkeys([*paths, manifest])
        }
        workspace = table(documents[manifest].get("workspace"))
        manifests: list[Manifest] = []
        for path in paths:
            package = table(documents[path].get("package"))
            name, version = package.get("name"), package.get("version")
            if table(version).get("workspace") is True:
                version = table(workspace.get("package")).get("version")
            if not isinstance(name, str) or not isinstance(version, str):
                raise ScriptError(f"{path} in {commit} declares no name and version")
            manifests.append(Manifest(name, Version.parse(version)))
        names = [crate.name for crate in manifests]
        declared = manifests[0].version
        if any(crate.version != declared for crate in manifests):
            raise ScriptError(
                "the crates that CI publishes declare different versions: "
                + ", ".join(f"{crate.name} {crate.version}" for crate in manifests)
            )
        inherited = table(workspace.get("dependencies"))
        requirements: dict[str, list[tuple[str, ...]]] = {}
        for index, path in enumerate(paths):
            required = requirements.setdefault(names[index], [])
            found = _requirements(documents[path], names, inherited)
            for name, specification, place in found:
                required.append(place)
                if (
                    not isinstance(specification.get("path"), str)
                    or specification.get("version") != f"={declared}"
                    or names.index(name) >= index
                ):
                    raise ScriptError(
                        f"{path} must require {name} by its path and as "
                        f"={declared}, and CI must publish {name} before it"
                    )
        # The workspace's packages that a run gives the new version in the
        # lockfile: those that it locks from a path at the crates' version. The
        # lockfile must hold no other package of their names, which the
        # references to them would then have to tell apart.
        locked = read_toml(originals[lockfile], f"{lockfile} in {commit}")
        entries = [*map(table, cast(list[object], locked.get("package") or []))]
        members = tuple(
            str(entry.get("name"))
            for entry in entries
            if "source" not in entry and entry.get("version") == str(declared)
        )
        for entry in entries:
            own = "source" not in entry and entry.get("version") == str(declared)
            if entry.get("name") in members and not own:
                raise ScriptError(
                    f"{lockfile} in {commit} locks {entry.get('name')} "
                    f"{entry.get('version')} from {entry.get('source', 'a path')} "
                    "too: a package of the crates' version must be the only "
                    "package of its name in the workspace"
                )
        inputs = [f"{crate.directory}/" for crate in crates]
        inputs += [
            path
            for path in (manifest, lockfile)
            if not any(path.startswith(crate) for crate in inputs)
        ]
        return cls(
            repository,
            commit,
            tuple(crates),
            tuple(manifests),
            manifest,
            lockfile,
            originals,
            tuple(inputs),
            {name: sorted(required) for name, required in requirements.items()},
            members,
        )

    def versioned(self, version: Version) -> dict[str, bytes]:
        """Return the files that a run that publishes `version` rewrites, by
        path, as it rewrites them."""
        names = tuple(dict.fromkeys((*self.names, *self.members)))
        files = {
            path: versioned_file(path, data, names, self.declared, version)
            for path, data in self.originals.items()
        }
        return {
            path: data for path, data in files.items() if data != self.originals[path]
        }

    def lockfile_table(self, version: Version) -> Table:
        """Return the workspace's lockfile as a run that publishes `version`
        rewrites it."""
        data = self.versioned(version).get(self.lockfile, self.originals[self.lockfile])
        return read_toml(data, self.lockfile)

    def require(self, version: Version) -> None:
        """Fail unless the working tree holds the commit's files, but for the
        versions that a run that publishes `version` gives the crates."""
        files = self.versioned(version)
        require_checkout(self.repository, self.inputs, files)
        for path, data in files.items():
            if (self.repository.root / path).read_bytes() != data:
                raise ScriptError(
                    f"{path} differs from {self.commit} in more than {version}"
                )

    def check(
        self,
        files: Mapping[str, bytes],
        source: str,
        name: str,
        version: Version,
        checksums: Mapping[str, str],
    ) -> None:
        """Check a package of `version` against the checkout's commit."""
        check_package(
            files,
            source=source,
            checkout=self,
            name=name,
            version=version,
            checksums=checksums,
        )

    def check_published(self, plan: Plan) -> None:
        """Check the packages on crates.io that this commit packed."""
        checksums = {name: sha256_file(path) for name, path in plan.published.items()}
        for name, package in plan.published.items():
            files = read_crate(package, f"{name}-{plan.version}")
            self.check(files, package.name, name, plan.version, checksums)

    def pack(
        self, plan: Plan, work_dir: Path, environment: Mapping[str, str]
    ) -> dict[str, str]:
        """Give the crates the plan's version, pack them, and check the
        packages, of which those that crates.io holds of this commit must be
        its bytes. The checkout keeps the version, which `cargo publish` packs
        again. Return the packages' SHA-256 digests by crate."""
        version = plan.version
        files = self.versioned(version)
        for path, data in files.items():
            (self.repository.root / path).write_bytes(data)
        self.require(version)
        packages = cargo_package(
            self, work_dir / "target", self.names, version, environment=environment
        )
        checksums = {name: sha256_file(package) for name, package in packages.items()}
        for name, package in packages.items():
            files = read_crate(package, f"{name}-{version}")
            self.check(files, package.name, name, version, checksums)
            published = sha256_file(plan.published.get(name, package))
            if published != checksums[name]:
                raise ScriptError(
                    f"crates.io holds {name} {version} with SHA-256 {published}, "
                    f"but this commit packs {checksums[name]}"
                )
        return checksums


def package_release(
    root: Path,
    work_dir: Path,
    *,
    crates: Sequence[Crate] = CRATES,
    registry: Registry | None = None,
    environment: Mapping[str, str] | None = None,
    platform: str = sys.platform,
) -> Plan:
    """The package job: decide whether the checkout's commit needs a new
    version of the crates, and if so, pack them with that version, check the
    packages, and build them with every feature. Its outputs give the publish
    job each package's version and SHA-256. The job never publishes, so the
    build scripts and procedural macros that its build runs cannot either."""
    environment = dict(os.environ) if environment is None else environment
    registry = Registry() if registry is None else registry
    checkout = Checkout.open(root, crates)
    checkout.require(checkout.declared)
    plan = plan_release(checkout, registry, work_dir)
    url = crate_url(checkout.names[-1], plan.version)
    labels = ", ".join(f"{name} {plan.version}" for name in checkout.names)
    if not plan.publish:
        checkout.check_published(plan)
        for name, value in (("publish", "false"), ("version", str(plan.version))):
            _set_output(environment, name, value)
        _set_output(environment, "url", url)
        _summarize(
            environment,
            f"Publishes nothing: {plan.reason}. The newest version is "
            f"[{labels}]({url}).",
        )
        return plan
    print(f">> {plan.reason}: packing {labels} from {checkout.commit}")
    checksums = checkout.pack(plan, work_dir, environment)
    build_packages(
        checkout, checksums, plan.version, work_dir, environment, platform=platform
    )
    states = [
        PackageState(
            name, str(checkout.declared), str(plan.version), checkout.commit, sha256
        )
        for name, sha256 in checksums.items()
    ]
    for name, value in (
        ("publish", "true"),
        ("version", str(plan.version)),
        ("packages", write_states(states)),
        ("url", url),
    ):
        _set_output(environment, name, value)
    _summarize(
        environment,
        f"Packed, checked, and built {labels} from `{checkout.commit}`, with "
        f"SHA-256 {', '.join(f'`{sha256}`' for sha256 in checksums.values())}: "
        f"{plan.reason}.",
    )
    return plan


def prepare_release(
    root: Path,
    work_dir: Path,
    states: Sequence[PackageState],
    *,
    crates: Sequence[Crate] = CRATES,
    registry: Registry | None = None,
    environment: Mapping[str, str] | None = None,
) -> bool:
    """The publish job, before it authenticates with crates.io: check that
    crates.io still needs the version of the packages that the package job
    packed, checked, and built, the `states`, and that the checkout packs the
    same bytes, which it checks again. Return whether to upload any: the rerun
    of a job that uploaded them all finds them on crates.io."""
    environment = dict(os.environ) if environment is None else environment
    registry = Registry() if registry is None else registry
    checkout = Checkout.open(root, crates)
    label = ", ".join(f"{state.name} {state.version}" for state in states)
    version = Version.parse(states[0].version) if states else checkout.declared
    if [(s.name, s.declared, s.version, s.commit) for s in states] != [
        (name, str(checkout.declared), str(version), checkout.commit)
        for name in checkout.names
    ]:
        raise ScriptError(
            f"the package job packed {label or 'nothing'}, not one version of "
            f"{', '.join(checkout.names)} from {checkout.commit}"
        )
    sha256 = {state.name: state.sha256 for state in states}
    for digest in sha256.values():
        if _SHA256.fullmatch(digest) is None:
            raise ScriptError(f"{digest!r} is not a SHA-256 digest")
    checkout.require(checkout.declared)
    plan = plan_release(checkout, registry, work_dir)
    if not plan.publish and (
        plan.version != version or len(plan.published) != len(states)
    ):
        raise ScriptError(
            f"crates.io changed since the package job packed {label}: {plan.reason}"
        )
    if plan.version != version:
        raise ScriptError(
            f"the package job packed {label}, but crates.io needs "
            f"{plan.version} now: {plan.reason}"
        )
    for name, package in plan.published.items():
        published = sha256_file(package)
        if published != sha256[name]:
            raise ScriptError(
                f"crates.io holds {name} {version} with SHA-256 {published}, not "
                f"the {sha256[name]} that the package job built"
            )
    if not plan.publish:
        checkout.check_published(plan)
        _set_output(environment, "upload", "false")
        _summarize(
            environment,
            f"[{label}]({crate_url(checkout.names[-1], version)}) was published "
            f"from `{checkout.commit}` already.",
        )
        return False
    for name, actual in checkout.pack(plan, work_dir, environment).items():
        if actual != sha256[name]:
            raise ScriptError(
                f"the checkout packs {name}-{version}.crate with SHA-256 {actual}, "
                f"not the {sha256[name]} that the package job built"
            )
    _write_state(work_dir, states)
    _set_output(environment, "upload", "true")
    print(f">> the checkout packs the bytes that the package job built: {label}")
    return True


def publish_release(
    root: Path,
    work_dir: Path,
    *,
    crates: Sequence[Crate] = CRATES,
    registry: Registry | None = None,
    environment: Mapping[str, str] | None = None,
    index_timeout_seconds: float = INDEX_WAIT_SECONDS,
) -> None:
    """Upload each package that the prepare step checked and that crates.io
    lacks, after the packages of the crates that it depends on, and check
    that crates.io serves the checked bytes of each, those that it listed
    already included."""
    environment = dict(os.environ) if environment is None else environment
    registry = Registry() if registry is None else registry
    if not environment.get("CARGO_REGISTRY_TOKEN"):
        raise ScriptError(
            "CARGO_REGISTRY_TOKEN, the token of the crates.io authentication "
            "step, is required to publish"
        )
    states = _read_state(work_dir)
    checkout = Checkout.open(root, crates)
    commit = checkout.commit
    for state in states:
        if commit != state.commit:
            raise ScriptError(
                f"the checkout is at {commit}, but {state.commit} was packed"
            )
    if [state.name for state in states] != list(checkout.names):
        raise ScriptError(f"{STATE_FILE} names other crates than {checkout.names}")
    checkout.require(Version.parse(states[0].version))
    checksums = {state.name: state.sha256 for state in states}
    for state in states:
        version = Version.parse(state.version)
        releases = registry.releases(state.name)
        _publish_package(
            checkout,
            state,
            checksums,
            work_dir,
            registry,
            environment,
            index_timeout_seconds,
            upload=all(release.version != version for release in releases),
        )


def _publish_package(
    checkout: Checkout,
    state: PackageState,
    checksums: Mapping[str, str],
    work_dir: Path,
    registry: Registry,
    environment: Mapping[str, str],
    index_timeout_seconds: float,
    upload: bool,
) -> None:
    """Pack a package that the prepare step checked alone again, with the
    crates that it depends on from crates.io, upload it if `upload`, and check
    that crates.io serves the same bytes."""
    commit, declared = checkout.commit, checkout.declared
    version = Version.parse(state.version)
    dirty = version != declared
    manifest_path = checkout.repository.root / checkout.manifest
    # Cargo packs deterministically, so `cargo publish` uploads these bytes.
    package = cargo_package(
        checkout, work_dir / "target", (state.name,), version, environment=environment
    )[state.name]
    actual = sha256_file(package)
    if actual != state.sha256:
        raise ScriptError(
            f"cargo packed {package.name} with SHA-256 {actual}, not the checked "
            f"{state.sha256}"
        )
    # --no-verify: the publish job builds nothing, so that no build script or
    # procedural macro runs where a registry token can be obtained.
    command: list[str | os.PathLike[str]] = [
        "cargo",
        "publish",
        "--locked",
        "--no-verify",
        "--manifest-path",
        manifest_path,
        "--target-dir",
        work_dir / "target",
        "--package",
        state.name,
    ]
    if dirty:
        command.append("--allow-dirty")
    if upload:
        run_checked(command, env=environment)
    release = registry.wait_for(
        state.name, version, timeout_seconds=index_timeout_seconds
    )
    if release.checksum != state.sha256:
        raise ScriptError(
            f"crates.io lists {state.name} {version} with SHA-256 "
            f"{release.checksum}, not the checked {state.sha256}"
        )
    published = registry.download(
        state.name, release, work_dir / "published" / package.name
    )
    check_package(
        read_crate(published, f"{state.name}-{version}"),
        source=f"{published.name} from crates.io",
        checkout=checkout,
        name=state.name,
        version=version,
        checksums=checksums,
    )
    _summarize(
        environment,
        f"Published [{state.name} {version}]({crate_url(state.name, version)}) "
        f"from `{commit}`.",
    )
