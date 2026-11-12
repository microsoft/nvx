"""Publication of the nvx crate to crates.io by CI's full runs of dev.

The crate's versions are pre-releases of the form ``X.Y.Z-LABEL.N``. A run
publishes a new version whenever the crate's directory changed since the
commit from which the newest version on crates.io was packed: the next number
of the series that the crate's ``Cargo.toml`` declares, and at least the
declared number. The new version exists only in the run's checkout, so the
package's ``.cargo_vcs_info.json`` records the run's commit, as dirty when the
version differs from the declared one.

Before the upload, the package must hold the commit's files byte for byte, but
for the version, and must build with every feature. The package job checks
and builds the package without the right to publish it, so that no build
script or procedural macro can publish either. The publish job builds
nothing: it packs the same bytes again and uploads them, and then crates.io
must serve those very bytes.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import tarfile
import time
import urllib.error
import urllib.request
from collections.abc import Callable, Collection, Iterable, Mapping, Sequence
from dataclasses import asdict, dataclass
from dataclasses import fields as dataclass_fields
from http.client import HTTPException
from pathlib import Path, PurePosixPath
from typing import cast

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
USER_AGENT = "nvx-crate-release (+https://github.com/microsoft/nvx)"

FETCH_ATTEMPTS = 3
FETCH_TIMEOUT_SECONDS = 60.0
FETCH_RETRY_SECONDS = 10.0
# crates.io lists a new version in its index within seconds, and its CDN
# caches index files for at most ten minutes.
INDEX_WAIT_SECONDS = 900.0
INDEX_POLL_SECONDS = 15.0

# Files that Cargo writes into every package instead of copying them.
GENERATED_FILES = frozenset({".cargo_vcs_info.json", "Cargo.toml"})
# Files of the package that Cargo copies from the checkout's files named
# here, whose version a run that publishes another version than the declared
# one rewrites.
VERSIONED_FILES = {"Cargo.toml.orig": "Cargo.toml", "Cargo.lock": "Cargo.lock"}
# Files without which the package does not build, document, or license itself.
REQUIRED_FILES = (
    ".cargo_vcs_info.json",
    "Cargo.toml",
    "Cargo.toml.orig",
    "Cargo.lock",
    "LICENSE",
    "README.md",
    "src/lib.rs",
)
# The state that the package step hands to the publish step.
STATE_FILE = "state.json"
# The most problems that a failed package check lists.
MAXIMUM_REPORTED_PROBLEMS = 20

_NUMERIC = r"(?:0|[1-9][0-9]*)"
_IDENTIFIER = rf"(?:{_NUMERIC}|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
_SEMVER = re.compile(
    rf"({_NUMERIC})\.({_NUMERIC})\.({_NUMERIC})"
    rf"(?:-({_IDENTIFIER}(?:\.{_IDENTIFIER})*))?"
    r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
)
_SHA256 = re.compile(r"[0-9a-f]{64}")
_COMMIT = re.compile(r"[0-9a-f]{40}|[0-9a-f]{64}")
_PACKAGE_TABLE = re.compile(r"\[\s*package\s*\]\s*(?:#.*)?")
_PACKAGE_FIELD = re.compile(
    r'(?P<key>name|version)\s*=\s*"(?P<value>[^"\\]*)"\s*(?:#.*)?'
)
_VERSION_VALUE = re.compile(r'(version\s*=\s*")[^"\\]*(")')


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
    """The fields of a Cargo manifest's [package] table that publication reads."""

    name: str
    version: Version


def _package_fields(lines: Sequence[str], source: str) -> dict[str, tuple[int, str]]:
    """Return the line and value of the [package] table's name and version."""
    fields: dict[str, tuple[int, str]] = {}
    in_package = False
    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith("["):
            in_package = _PACKAGE_TABLE.fullmatch(stripped) is not None
            continue
        match = _PACKAGE_FIELD.fullmatch(stripped) if in_package else None
        if match is None:
            continue
        key = match.group("key")
        if key in fields:
            raise ScriptError(f"{source} sets package.{key} twice")
        fields[key] = (index, match.group("value"))
    for key in ("name", "version"):
        if key not in fields:
            raise ScriptError(f"{source} sets no package.{key} string")
    return fields


def read_manifest(data: bytes, source: str) -> Manifest:
    fields = _package_fields(data.decode("utf-8").splitlines(), source)
    return Manifest(fields["name"][1], Version.parse(fields["version"][1]))


def rewrite_manifest(data: bytes, source: str, version: Version) -> bytes:
    """Return a manifest whose [package] table declares `version`, every
    other byte unchanged."""
    lines = data.decode("utf-8").splitlines(keepends=True)
    index = _package_fields(lines, source)["version"][0]
    lines[index] = _VERSION_VALUE.sub(
        lambda match: f"{match.group(1)}{version}{match.group(2)}",
        lines[index],
        count=1,
    )
    return "".join(lines).encode("utf-8")


def rewrite_lockfile(
    data: bytes, source: str, name: str, declared: Version, version: Version
) -> bytes:
    """Return a lockfile in which the package `name` has `version` instead of
    `declared`, every other byte unchanged."""
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
    path: str, data: bytes, name: str, declared: Version, version: Version
) -> bytes:
    """Return the checkout's file `path`, one of VERSIONED_FILES' sources,
    as a run that publishes `version` rewrites it."""
    if version == declared:
        return data
    if path == "Cargo.toml":
        return rewrite_manifest(data, path, version)
    return rewrite_lockfile(data, path, name, declared, version)


def git_blob_id(data: bytes, commit: str) -> str:
    """Return the ID of a Git blob that holds `data`, in the object format
    of `commit`."""
    digest = hashlib.new("sha1" if len(commit) == 40 else "sha256")
    digest.update(b"blob %d\0" % len(data))
    digest.update(data)
    return digest.hexdigest()


class Repository:
    """The Git checkout that holds the crate."""

    def __init__(self, crate_dir: Path) -> None:
        self.crate_dir = crate_dir
        # The crate's directory relative to the root of the checkout, with a
        # trailing slash, as `git status` names its files.
        self.prefix = self._text("locating the crate", "rev-parse", "--show-prefix")
        # The same without the slash, as `.cargo_vcs_info.json` names it.
        self.path_in_vcs = self.prefix.rstrip("/")

    def _git(self, *arguments: str) -> CommandResult:
        return run_capture(["git", "-C", self.crate_dir, *arguments])

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

    def changed(self, base: str, head: str) -> bool:
        """Return whether the crate's directory differs between two commits."""
        return not self._predicate(
            "comparing the crate's files",
            "diff",
            "--quiet",
            "--no-ext-diff",
            "--no-textconv",
            base,
            head,
            "--",
            ".",
        )

    def status(self) -> list[str]:
        """Return the changed and untracked files of the crate's directory,
        as `git status --porcelain` lists them."""
        result = self._git(
            "status", "--porcelain=v1", "-z", "--untracked-files=all", "--", "."
        )
        require_success(result, "git status")
        return [entry for entry in result.stdout.decode("utf-8").split("\0") if entry]

    def tree(self, commit: str) -> dict[str, str]:
        """Map each file of the crate's directory in `commit` to its blob."""
        tree = f"{commit}:{self.path_in_vcs}" if self.path_in_vcs else commit
        # Without --full-tree, ls-tree lists only the part of the tree beneath
        # the working directory's path, which the crate's subtree lacks.
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


def require_checkout(repository: Repository, modified: Iterable[str]) -> None:
    """Fail unless the crate's directory differs from its commit in exactly
    the `modified` files, which the working tree changed."""
    expected = sorted(f" M {repository.prefix}{path}" for path in modified)
    actual = sorted(repository.status())
    if actual != expected:
        changes = "\n".join(actual) or "(none)"
        wanted = "\n".join(expected) or "(none)"
        raise ScriptError(
            f"the checkout of {repository.prefix or './'} has these changes:\n"
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


def check_package(
    files: Mapping[str, bytes],
    *,
    source: str,
    repository: Repository,
    commit: str,
    name: str,
    declared: Version,
    version: Version,
) -> None:
    """Fail unless a package of `name` `version` was packed from `commit`, in
    which the crate declares `declared`: it records the commit, as dirty
    exactly when the versions differ, and it holds the commit's files byte for
    byte, but for the rewritten version and the files that Cargo generates."""
    problems: list[str] = []
    vcs = read_vcs_info(files, source)
    dirty = version != declared
    if vcs is None:
        problems.append(".cargo_vcs_info.json is missing")
    else:
        if vcs.commit != commit:
            problems.append(f".cargo_vcs_info.json records {vcs.commit}, not {commit}")
        if vcs.path_in_vcs != repository.path_in_vcs:
            problems.append(
                f".cargo_vcs_info.json records the path {vcs.path_in_vcs!r}, not "
                f"{repository.path_in_vcs!r}"
            )
        if vcs.dirty != dirty:
            problems.append(
                f".cargo_vcs_info.json records a {'dirty' if vcs.dirty else 'clean'} "
                f"checkout, but one that declares {declared} packs {version} from "
                f"a {'dirty' if dirty else 'clean'} one"
            )
    problems.extend(
        f"{path} is missing" for path in REQUIRED_FILES if path not in files
    )
    tree = repository.tree(commit)
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
                    original_path, original, name, declared, version
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
            manifest = read_manifest(manifest_data, f"{source}: Cargo.toml")
        except (ScriptError, UnicodeDecodeError) as error:
            problems.append(str(error))
        else:
            if manifest != Manifest(name, version):
                problems.append(
                    f"Cargo.toml declares {manifest.name} {manifest.version}, not "
                    f"{name} {version}"
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
    crate_dir: Path,
    target_dir: Path,
    manifest: Manifest,
    *,
    allow_dirty: bool,
    environment: Mapping[str, str],
) -> Path:
    """Pack the crate with Cargo, without building it, and return the package."""
    command: list[str | os.PathLike[str]] = [
        "cargo",
        "package",
        "--locked",
        "--no-verify",
        "--manifest-path",
        crate_dir / "Cargo.toml",
        "--target-dir",
        target_dir,
    ]
    if allow_dirty:
        command.append("--allow-dirty")
    run_checked(command, env=environment)
    package = target_dir / "package" / f"{manifest.name}-{manifest.version}.crate"
    if not package.is_file():
        raise ScriptError(f"cargo package wrote no {package}")
    return package


def build_package(
    files: Mapping[str, bytes],
    root: str,
    work_dir: Path,
    environment: Mapping[str, str],
) -> None:
    """Build the package's files with every feature, as a dependent would."""
    source = work_dir / "build" / root
    if source.exists():
        shutil.rmtree(source)
    for relative, data in files.items():
        destination = source.joinpath(*PurePosixPath(relative).parts)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)
    print(f">> building {root} with every feature")
    run_checked(
        [
            "cargo",
            "build",
            "--locked",
            "--all-features",
            "--manifest-path",
            source / "Cargo.toml",
            "--target-dir",
            work_dir / "build-target",
        ],
        env=environment,
    )


@dataclass(frozen=True)
class Plan:
    """Whether a run publishes, and the version that it publishes or, when it
    does not, the newest version on crates.io."""

    publish: bool
    version: Version
    reason: str
    # The newest package on crates.io, when this commit already published it,
    # so that a rerun checks it instead.
    published_package: Path | None = None


def plan_release(
    repository: Repository,
    manifest: Manifest,
    commit: str,
    registry: Registry,
    work_dir: Path,
) -> Plan:
    """Decide whether `commit` needs a new version: it does when the crate's
    directory changed since the commit from which the newest version on
    crates.io was packed, unless that commit already contains it."""
    releases = registry.releases(manifest.name)
    if not releases:
        return Plan(
            True,
            next_version(manifest.version, ()),
            f"crates.io has no version of {manifest.name}",
        )
    newest = max(releases, key=lambda release: release.version.precedence)
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
    if vcs.path_in_vcs != repository.path_in_vcs:
        raise ScriptError(
            f"{label} was packed from {vcs.path_in_vcs!r}, not from "
            f"{repository.path_in_vcs!r}"
        )
    if not repository.has_commit(vcs.commit):
        raise ScriptError(
            f"{label} was packed from {vcs.commit}, which this checkout lacks: "
            "check out the full history, or publish the next version by hand "
            "(doc/distribution.md, The nvx crate)"
        )
    directory = f"{repository.prefix or './'}"
    short = vcs.commit[:12]
    if vcs.commit == commit:
        return Plan(
            False, newest.version, f"{label} was packed from this commit", package
        )
    if repository.contains(commit, vcs.commit):
        return Plan(
            False,
            newest.version,
            f"{label} was packed from {short}, which contains this commit",
        )
    if not repository.changed(vcs.commit, commit):
        return Plan(
            False,
            newest.version,
            f"{directory} is unchanged since {short}, from which {label} was packed",
        )
    return Plan(
        True,
        next_version(manifest.version, [release.version for release in releases]),
        f"{directory} changed since {short}, from which {label} was packed",
    )


@dataclass(frozen=True)
class PackageState:
    """The package that the prepare step checked, for the publish step."""

    name: str
    declared: str
    version: str
    commit: str
    sha256: str


def _write_state(work_dir: Path, state: PackageState) -> None:
    work_dir.mkdir(parents=True, exist_ok=True)
    (work_dir / STATE_FILE).write_text(
        json.dumps(asdict(state), indent=2) + "\n", encoding="utf-8"
    )


def _read_state(work_dir: Path) -> PackageState:
    path = work_dir / STATE_FILE
    try:
        value: object = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ScriptError(
            f"cannot read {path}, which the prepare step writes: {error}"
        ) from error
    document = cast(dict[str, object], value) if isinstance(value, dict) else {}
    values = {
        field.name: document.get(field.name) for field in dataclass_fields(PackageState)
    }
    if not all(isinstance(value, str) for value in values.values()):
        raise ScriptError(f"{path} is not the state of a checked package")
    return PackageState(**cast(dict[str, str], values))


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
    """The crate's checkout, which a step finds unchanged from its commit."""

    crate_dir: Path
    repository: Repository
    commit: str
    manifest: Manifest

    @classmethod
    def open(cls, crate_dir: Path) -> Checkout:
        repository = Repository(crate_dir)
        commit = repository.head()
        require_checkout(repository, ())
        manifest_path = crate_dir / "Cargo.toml"
        manifest = read_manifest(manifest_path.read_bytes(), str(manifest_path))
        return cls(crate_dir, repository, commit, manifest)

    def check(self, files: Mapping[str, bytes], source: str, version: Version) -> None:
        """Check a package of `version` against the checkout's commit."""
        check_package(
            files,
            source=source,
            repository=self.repository,
            commit=self.commit,
            name=self.manifest.name,
            declared=self.manifest.version,
            version=version,
        )

    def check_published(self, plan: Plan) -> None:
        """Check the newest package on crates.io, which this commit packed."""
        package = plan.published_package
        if package is None:
            raise ScriptError(f"{plan.reason}, so no package of this commit exists")
        self.check(
            read_crate(package, f"{self.manifest.name}-{plan.version}"),
            package.name,
            plan.version,
        )

    def pack(
        self, version: Version, work_dir: Path, environment: Mapping[str, str]
    ) -> tuple[Path, dict[str, bytes]]:
        """Give the checkout `version`, pack the crate, and check the package.
        The checkout keeps the version, which `cargo publish` packs again."""
        name, declared = self.manifest.name, self.manifest.version
        dirty = version != declared
        if dirty:
            for path in VERSIONED_FILES.values():
                file = self.crate_dir / path
                file.write_bytes(
                    versioned_file(path, file.read_bytes(), name, declared, version)
                )
        require_checkout(self.repository, VERSIONED_FILES.values() if dirty else ())
        package = cargo_package(
            self.crate_dir,
            work_dir / "target",
            Manifest(name, version),
            allow_dirty=dirty,
            environment=environment,
        )
        files = read_crate(package, f"{name}-{version}")
        self.check(files, package.name, version)
        return package, files


def package_release(
    crate_dir: Path,
    work_dir: Path,
    *,
    registry: Registry | None = None,
    environment: Mapping[str, str] | None = None,
) -> Plan:
    """The package job: decide whether the checkout's commit needs a new
    version, and if so, pack the crate with that version, check the package,
    and build it with every feature. Its outputs give the publish job the
    version and the SHA-256 of the package. The job never publishes, so the
    build scripts and procedural macros that its build runs cannot either."""
    environment = dict(os.environ) if environment is None else environment
    registry = Registry() if registry is None else registry
    checkout = Checkout.open(crate_dir)
    manifest = checkout.manifest
    plan = plan_release(
        checkout.repository, manifest, checkout.commit, registry, work_dir
    )
    url = crate_url(manifest.name, plan.version)
    if not plan.publish:
        if plan.published_package is not None:
            checkout.check_published(plan)
        for name, value in (("publish", "false"), ("version", str(plan.version))):
            _set_output(environment, name, value)
        _set_output(environment, "url", url)
        _summarize(
            environment,
            f"Publishes nothing: {plan.reason}. The newest version is "
            f"[{manifest.name} {plan.version}]({url}).",
        )
        return plan
    print(
        f">> {plan.reason}: packing {manifest.name} {plan.version} from "
        f"{checkout.commit}"
    )
    package, files = checkout.pack(plan.version, work_dir, environment)
    build_package(files, f"{manifest.name}-{plan.version}", work_dir, environment)
    sha256 = sha256_file(package)
    for name, value in (
        ("publish", "true"),
        ("version", str(plan.version)),
        ("sha256", sha256),
        ("url", url),
    ):
        _set_output(environment, name, value)
    _summarize(
        environment,
        f"Packed, checked, and built {manifest.name} {plan.version} from "
        f"`{checkout.commit}`, with SHA-256 `{sha256}`: {plan.reason}.",
    )
    return plan


def prepare_release(
    crate_dir: Path,
    work_dir: Path,
    version: Version,
    sha256: str,
    *,
    registry: Registry | None = None,
    environment: Mapping[str, str] | None = None,
) -> bool:
    """The publish job, before it authenticates with crates.io: check that
    crates.io still needs `version`, which the package job packed, checked,
    and built with the SHA-256 `sha256`, and that the checkout packs the same
    bytes, which it checks again. Return whether to upload them: the rerun of
    a job that uploaded them finds them on crates.io."""
    environment = dict(os.environ) if environment is None else environment
    registry = Registry() if registry is None else registry
    if _SHA256.fullmatch(sha256) is None:
        raise ScriptError(f"{sha256!r} is not a SHA-256 digest")
    checkout = Checkout.open(crate_dir)
    manifest = checkout.manifest
    label = f"{manifest.name} {version}"
    plan = plan_release(
        checkout.repository, manifest, checkout.commit, registry, work_dir
    )
    if not plan.publish:
        if plan.published_package is None or plan.version != version:
            raise ScriptError(
                f"crates.io changed since the package job packed {label}: {plan.reason}"
            )
        published = sha256_file(plan.published_package)
        if published != sha256:
            raise ScriptError(
                f"crates.io holds {label} with SHA-256 {published}, not the "
                f"{sha256} that the package job built"
            )
        checkout.check_published(plan)
        _set_output(environment, "upload", "false")
        _summarize(
            environment,
            f"[{label}]({crate_url(manifest.name, version)}) was published "
            f"from `{checkout.commit}` already.",
        )
        return False
    if plan.version != version:
        raise ScriptError(
            f"the package job packed {label}, but crates.io needs "
            f"{plan.version} now: {plan.reason}"
        )
    package, _files = checkout.pack(version, work_dir, environment)
    actual = sha256_file(package)
    if actual != sha256:
        raise ScriptError(
            f"the checkout packs {package.name} with SHA-256 {actual}, not the "
            f"{sha256} that the package job built"
        )
    _write_state(
        work_dir,
        PackageState(
            manifest.name, str(manifest.version), str(version), checkout.commit, sha256
        ),
    )
    _set_output(environment, "upload", "true")
    print(f">> {package.name} holds the bytes that the package job built")
    return True


def publish_release(
    crate_dir: Path,
    work_dir: Path,
    *,
    registry: Registry | None = None,
    environment: Mapping[str, str] | None = None,
    index_timeout_seconds: float = INDEX_WAIT_SECONDS,
) -> None:
    """Upload the package that the prepare step checked, and check that
    crates.io serves the same bytes."""
    environment = dict(os.environ) if environment is None else environment
    registry = Registry() if registry is None else registry
    if not environment.get("CARGO_REGISTRY_TOKEN"):
        raise ScriptError(
            "CARGO_REGISTRY_TOKEN, the token of the crates.io authentication "
            "step, is required to publish"
        )
    state = _read_state(work_dir)
    repository = Repository(crate_dir)
    commit = repository.head()
    if commit != state.commit:
        raise ScriptError(f"the checkout is at {commit}, but {state.commit} was packed")
    manifest_path = crate_dir / "Cargo.toml"
    manifest = read_manifest(manifest_path.read_bytes(), str(manifest_path))
    declared = Version.parse(state.declared)
    version = Version.parse(state.version)
    if manifest != Manifest(state.name, version):
        raise ScriptError(
            f"the checkout declares {manifest.name} {manifest.version}, but "
            f"{state.name} {version} was packed"
        )
    dirty = version != declared
    require_checkout(repository, VERSIONED_FILES.values() if dirty else ())
    # Cargo packs deterministically, so `cargo publish` uploads these bytes.
    package = cargo_package(
        crate_dir,
        work_dir / "target",
        manifest,
        allow_dirty=dirty,
        environment=environment,
    )
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
    ]
    if dirty:
        command.append("--allow-dirty")
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
        repository=repository,
        commit=commit,
        name=state.name,
        declared=declared,
        version=version,
    )
    _summarize(
        environment,
        f"Published [{state.name} {version}]({crate_url(state.name, version)}) "
        f"from `{commit}`.",
    )
