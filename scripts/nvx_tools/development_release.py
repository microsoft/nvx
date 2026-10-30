"""Resumable GitHub development release publication.

Publication finds the release that carries its tag, or creates it as a draft,
once, and then reads and changes it only by its ID, through the REST API with
`gh api`. `gh release` would resolve a draft's tag again on every call,
through a lookup that reports any failure as `release not found`.
"""

from __future__ import annotations

import json
import re
import subprocess
import time
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import cast
from urllib.parse import urlencode

from .common import (
    ScriptError,
    diagnostic_tail,
    require_file,
    sha256_file,
)

DEVELOPMENT_RELEASE_UPLOAD_ATTEMPTS = 3
DEVELOPMENT_RELEASE_UPLOAD_TIMEOUT_SECONDS = 180.0
DEVELOPMENT_RELEASE_RETRY_BACKOFF_SECONDS = 10.0

# GitHub ends a REST request that it takes more than 10 seconds to process.
_GITHUB_COMMAND_TIMEOUT_SECONDS = 20.0
# Listing reads a page per 100 releases, and every development release adds one.
_GITHUB_LIST_TIMEOUT_SECONDS = 120.0
# The bounded attempts and the backoff of a request that repeating cannot
# harm: a read, or a deletion or publication that names its asset or release by
# ID. Creating the release is not one, and uploads have attempts of their own.
# The release job's timeout covers every request's attempts at once.
_GITHUB_REQUEST_ATTEMPTS = 3
_GITHUB_REQUEST_RETRY_BACKOFF_SECONDS = 5.0
# How much of a failed response's body a diagnostic quotes: a failed listing
# leaves on stdout every page that it read.
_DIAGNOSTIC_BODY_CHARACTERS = 1000
_REPOSITORY_PATTERN = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
_TARGET_SHA_PATTERN = re.compile(r"^[0-9A-Fa-f]{40}$")
# `gh api` reports a response with an error status as `gh: <message> (HTTP
# <status>)`, or as `gh: HTTP <status>` when the response has no message.
_HTTP_STATUS = re.compile(r"^gh: (?:HTTP (\d{3})|.* \(HTTP (\d{3})\))$", re.MULTILINE)
# GitHub answers these statuses, and 5xx, to requests that can succeed later.
_TRANSIENT_HTTP_STATUSES = frozenset({408, 425, 429})
# How a request that got no response fails transiently: the errors with which
# Go reports a connection that broke, was refused or closed, or timed out, an
# HTTP/2 connection or stream that failed, and gh's for a host that it could
# not reach. `gh release upload` repeated an upload that failed so by itself,
# and `gh api` does not.
_TRANSIENT_CONNECTION_ERROR = re.compile(
    r"(?:"
    r"timed?\s*out|timeout|context deadline exceeded|"
    r"connection (?:aborted|refused|reset)|"
    r"temporary failure|temporarily unavailable|"
    r"\bEOF\b|TLS handshake timeout|broken pipe|"
    r"use of closed network connection|network is unreachable|"
    r"http2: |stream error|"
    r"error connecting to "
    r")",
    re.IGNORECASE,
)
# How GitHub rejects an upload that a later attempt can complete, such as one
# whose name an earlier attempt's asset still holds.
_TRANSIENT_UPLOAD_ERROR = re.compile(
    r"error saving asset|already[_ ]exists",
    re.IGNORECASE,
)


@dataclass(frozen=True)
class _GitHubCommandResult:
    returncode: int
    stdout: str
    stderr: str
    elapsed_seconds: float
    timed_out: bool = False

    @property
    def output(self) -> str:
        return "\n".join(
            part for part in (self.stderr.strip(), self.stdout.strip()) if part
        )

    @property
    def diagnostic(self) -> str:
        """The end of the body that gh printed, then the error that it reported."""
        body = self.stdout.strip()
        if len(body) > _DIAGNOSTIC_BODY_CHARACTERS:
            body = "..." + body[-_DIAGNOSTIC_BODY_CHARACTERS:]
        return "\n".join(part for part in (body, self.stderr.strip()) if part)


@dataclass(frozen=True)
class _DevelopmentReleaseAsset:
    id: int
    name: str
    size: int
    digest: str | None
    state: str


@dataclass(frozen=True)
class _DevelopmentRelease:
    id: int
    tag: str
    target_commitish: str
    draft: bool
    prerelease: bool
    upload_url: str
    assets: tuple[_DevelopmentReleaseAsset, ...]


@dataclass(frozen=True)
class _LocalDevelopmentReleaseAsset:
    path: Path
    name: str
    size: int
    sha256: str


def _captured_text(value: str | bytes | None) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    return value


def _run_gh(
    args: Sequence[str],
    *,
    timeout_seconds: float,
) -> _GitHubCommandResult:
    command = ("gh", *args)
    started = time.monotonic()
    try:
        completed = subprocess.run(
            command,
            capture_output=True,
            text=True,
            timeout=timeout_seconds,
        )
    except subprocess.TimeoutExpired as error:
        return _GitHubCommandResult(
            124,
            _captured_text(error.stdout),
            _captured_text(error.stderr),
            time.monotonic() - started,
            timed_out=True,
        )
    except OSError as error:
        raise ScriptError(f"failed to run gh: {error}") from error
    return _GitHubCommandResult(
        completed.returncode,
        completed.stdout,
        completed.stderr,
        time.monotonic() - started,
    )


def _command_error(label: str, result: _GitHubCommandResult) -> ScriptError:
    if result.timed_out:
        status = "timed out"
    else:
        status = f"exited {result.returncode}"
    diagnostic = diagnostic_tail(result.diagnostic)
    suffix = f"\n{diagnostic}" if diagnostic else ""
    return ScriptError(f"{label} {status}{suffix}")


def _http_status(result: _GitHubCommandResult) -> int | None:
    """Return the status of the error response that `gh api` reported, if any."""
    match = _HTTP_STATUS.search(result.stderr)
    if match is None:
        return None
    return int(match.group(1) or match.group(2))


def _is_transient_failure(result: _GitHubCommandResult) -> bool:
    """Whether a request that failed can succeed if it is repeated."""
    if result.timed_out:
        return True
    status = _http_status(result)
    if status is None:
        return _TRANSIENT_CONNECTION_ERROR.search(result.stderr) is not None
    return status in _TRANSIENT_HTTP_STATUSES or 500 <= status <= 599


def _failure_reason(result: _GitHubCommandResult) -> str:
    if result.timed_out:
        return "command timed out"
    diagnostic = diagnostic_tail(result.diagnostic, lines=5)
    return diagnostic or f"command exited {result.returncode}"


def _run_gh_with_retries(
    args: Sequence[str],
    *,
    label: str,
    timeout_seconds: float = _GITHUB_COMMAND_TIMEOUT_SECONDS,
) -> _GitHubCommandResult:
    """Run a request that repeating cannot harm, again while it fails
    transiently, up to its bounded attempts."""
    attempt = 1
    while True:
        result = _run_gh(args, timeout_seconds=timeout_seconds)
        if (
            result.returncode == 0
            or attempt >= _GITHUB_REQUEST_ATTEMPTS
            or not _is_transient_failure(result)
        ):
            return result
        delay = _GITHUB_REQUEST_RETRY_BACKOFF_SECONDS * (2 ** (attempt - 1))
        print(f">> {label} failed; retrying in {delay:g}s: {_failure_reason(result)}")
        time.sleep(delay)
        attempt += 1


def _release_from_document(value: object) -> _DevelopmentRelease:
    """Read a release from its REST document."""
    if not isinstance(value, dict):
        raise ScriptError("gh returned an invalid release document")
    document = cast(dict[str, object], value)
    release_id = document.get("id")
    tag = document.get("tag_name")
    target = document.get("target_commitish")
    draft = document.get("draft")
    prerelease = document.get("prerelease")
    upload_url = document.get("upload_url")
    assets_value = document.get("assets")
    if (
        not isinstance(release_id, int)
        or isinstance(release_id, bool)
        or not isinstance(tag, str)
        or not isinstance(target, str)
        or not isinstance(draft, bool)
        or not isinstance(prerelease, bool)
        or not isinstance(upload_url, str)
        or not isinstance(assets_value, list)
    ):
        raise ScriptError("gh returned incomplete release metadata")

    assets: list[_DevelopmentReleaseAsset] = []
    for asset_value in cast(list[object], assets_value):
        if not isinstance(asset_value, dict):
            raise ScriptError("gh returned an invalid release asset")
        asset = cast(dict[str, object], asset_value)
        asset_id = asset.get("id")
        name = asset.get("name")
        size = asset.get("size")
        digest_value = asset.get("digest")
        state = asset.get("state")
        if (
            not isinstance(asset_id, int)
            or isinstance(asset_id, bool)
            or not isinstance(name, str)
            or not isinstance(size, int)
            or isinstance(size, bool)
            or not isinstance(state, str)
            or not (digest_value is None or isinstance(digest_value, str))
        ):
            raise ScriptError("gh returned incomplete release asset metadata")
        assets.append(
            _DevelopmentReleaseAsset(
                asset_id,
                name,
                size,
                digest_value,
                state,
            )
        )
    return _DevelopmentRelease(
        release_id,
        tag,
        target,
        draft,
        prerelease,
        upload_url,
        tuple(assets),
    )


def _parse_release(payload: str) -> _DevelopmentRelease:
    try:
        value: object = json.loads(payload)
    except json.JSONDecodeError as error:
        raise ScriptError("gh returned invalid release JSON") from error
    return _release_from_document(value)


def _releases_tagged(payload: str, tag: str) -> list[_DevelopmentRelease]:
    """Return the releases that carry `tag` among the pages of a listing."""
    try:
        value: object = json.loads(payload)
    except json.JSONDecodeError as error:
        raise ScriptError("gh returned invalid release list JSON") from error
    if not isinstance(value, list):
        raise ScriptError("gh returned an invalid release list")
    releases: list[_DevelopmentRelease] = []
    for page in cast(list[object], value):
        if not isinstance(page, list):
            raise ScriptError("gh returned an invalid release list page")
        for document_value in cast(list[object], page):
            if not isinstance(document_value, dict):
                raise ScriptError("gh returned an invalid release document")
            document = cast(dict[str, object], document_value)
            if document.get("tag_name") == tag:
                releases.append(_release_from_document(document))
    return releases


def _find_development_release(
    repository: str,
    tag: str,
) -> _DevelopmentRelease | None:
    """Find the release, draft or published, that carries `tag`.

    The listing includes drafts for tokens with push access. GitHub keeps tags
    unique only among published releases, so drafts can share one, and a tag
    that several releases carry no longer identifies the release to publish.
    """
    result = _run_gh_with_retries(
        (
            "api",
            "--paginate",
            "--slurp",
            f"repos/{repository}/releases?per_page=100",
        ),
        label=f"listing releases of {repository}",
        timeout_seconds=_GITHUB_LIST_TIMEOUT_SECONDS,
    )
    if result.returncode != 0:
        raise _command_error(f"listing releases of {repository}", result)
    # A release created during the listing shifts its later pages, which then
    # show again the release that ended the page before.
    releases = {release.id: release for release in _releases_tagged(result.stdout, tag)}
    if len(releases) > 1:
        ids = ", ".join(str(release_id) for release_id in sorted(releases))
        raise ScriptError(
            f"releases {ids} all carry development release tag {tag}; "
            "refusing to change any of them"
        )
    return next(iter(releases.values()), None)


def _require_development_release(
    repository: str,
    release: _DevelopmentRelease,
) -> _DevelopmentRelease:
    """Read `release` again by its ID."""
    result = _run_gh_with_retries(
        ("api", f"repos/{repository}/releases/{release.id}"),
        label=f"reading development release {release.tag}",
    )
    if result.returncode == 0:
        return _parse_release(result.stdout)
    if _http_status(result) == 404:
        raise ScriptError(f"development release {release.tag} disappeared")
    raise _command_error(f"reading development release {release.tag}", result)


def _create_development_release(
    repository: str,
    tag: str,
    target_sha: str,
    title: str,
    notes: str,
) -> _DevelopmentRelease:
    print(f">> creating draft development release {tag}")
    result = _run_gh(
        (
            "api",
            "--method",
            "POST",
            f"repos/{repository}/releases",
            "--raw-field",
            f"tag_name={tag}",
            "--raw-field",
            f"target_commitish={target_sha}",
            "--raw-field",
            f"name={title}",
            "--raw-field",
            f"body={notes}",
            "--field",
            "draft=true",
            "--field",
            "prerelease=true",
            "--raw-field",
            "make_latest=false",
        ),
        timeout_seconds=_GITHUB_COMMAND_TIMEOUT_SECONDS,
    )
    if result.returncode == 0:
        return _parse_release(result.stdout)
    # GitHub can create the draft although the request failed on the client.
    release = _find_development_release(repository, tag)
    if release is None:
        raise _command_error(f"creating development release {tag}", result)
    print(f">> release {tag} exists despite the create command failure; resuming it")
    return release


def _asset_named(
    release: _DevelopmentRelease,
    name: str,
) -> _DevelopmentReleaseAsset | None:
    matches = [asset for asset in release.assets if asset.name == name]
    if len(matches) > 1:
        raise ScriptError(
            f"development release {release.tag} contains duplicate asset {name}"
        )
    return matches[0] if matches else None


def _local_asset(path: Path) -> _LocalDevelopmentReleaseAsset:
    require_file(path, f"development release package {path.name}")
    return _LocalDevelopmentReleaseAsset(
        path,
        path.name,
        path.stat().st_size,
        sha256_file(path),
    )


def _asset_mismatch(
    remote: _DevelopmentReleaseAsset,
    local: _LocalDevelopmentReleaseAsset,
) -> str | None:
    if remote.state != "uploaded":
        return f"remote state is {remote.state!r}"
    if remote.size != local.size:
        return f"remote size is {remote.size} bytes, expected {local.size}"
    expected_digest = f"sha256:{local.sha256}"
    if remote.digest != expected_digest:
        return f"remote digest is {remote.digest!r}, expected {expected_digest}"
    return None


def _verification_description(
    remote: _DevelopmentReleaseAsset,
    local: _LocalDevelopmentReleaseAsset,
) -> str:
    if remote.digest is not None and remote.digest.startswith("sha256:"):
        return remote.digest
    return f"{local.size} bytes"


def _delete_development_release_asset(
    repository: str,
    release: _DevelopmentRelease,
    asset: _DevelopmentReleaseAsset,
) -> None:
    result = _run_gh_with_retries(
        (
            "api",
            "--method",
            "DELETE",
            f"repos/{repository}/releases/assets/{asset.id}",
        ),
        label=f"deleting release asset {asset.name}",
    )
    observed = _require_development_release(repository, release)
    if _asset_named(observed, asset.name) is None:
        if result.returncode != 0:
            print(
                f">> asset {asset.name} is absent despite the delete command failure; "
                "continuing"
            )
        return
    if result.returncode != 0:
        raise _command_error(f"deleting release asset {asset.name}", result)
    raise ScriptError(f"release asset {asset.name} still exists after deletion")


def _is_transient_upload_failure(result: _GitHubCommandResult) -> bool:
    return (
        _is_transient_failure(result)
        or _TRANSIENT_UPLOAD_ERROR.search(result.output) is not None
    )


def _upload_failure_reason(result: _GitHubCommandResult) -> str:
    if result.returncode == 0 and not result.timed_out:
        return "upload command completed but the asset was not verified"
    return _failure_reason(result)


def _asset_upload_url(release: _DevelopmentRelease, name: str) -> str:
    """Return the URL to which the asset `name` of `release` uploads.

    GitHub gives a release's upload URL as a URI template that ends in
    `{?name,label}`, which gh drops before it adds the query.
    """
    url = release.upload_url.split("{", 1)[0]
    if not url.startswith("https://"):
        raise ScriptError(
            f"development release {release.tag} has an invalid upload URL "
            f"{release.upload_url!r}"
        )
    return f"{url}?{urlencode({'name': name})}"


def _asset_content_type(name: str) -> str:
    """Return the content type with which `gh release upload` sends an asset."""
    lowered = name.lower()
    if lowered.endswith((".tar.gz", ".tgz")):
        return "application/x-gtar"
    if lowered.endswith(".zip"):
        return "application/zip"
    return "application/octet-stream"


def _upload_expected_asset(
    repository: str,
    release: _DevelopmentRelease,
    local: _LocalDevelopmentReleaseAsset,
    *,
    attempts: int,
    timeout_seconds: float,
    retry_backoff_seconds: float,
) -> None:
    release = _require_development_release(repository, release)
    existing = _asset_named(release, local.name)
    if existing is not None:
        mismatch = _asset_mismatch(existing, local)
        if mismatch is None:
            print(
                f">> {local.name}: already verified "
                f"({_verification_description(existing, local)}); skipping upload"
            )
            return
        raise ScriptError(
            f"development release asset {local.name} already exists but is invalid: "
            f"{mismatch}; refusing to replace it"
        )
    elif not release.draft:
        raise ScriptError(f"published release is missing required asset {local.name}")

    for attempt in range(1, attempts + 1):
        print(
            f">> {local.name}: upload attempt {attempt}/{attempts} "
            f"with {timeout_seconds:g}s timeout"
        )
        result = _run_gh(
            (
                "api",
                "--method",
                "POST",
                _asset_upload_url(release, local.name),
                "--input",
                str(local.path),
                "--header",
                f"Content-Type: {_asset_content_type(local.name)}",
            ),
            timeout_seconds=timeout_seconds,
        )

        observed_release = _require_development_release(repository, release)
        observed = _asset_named(observed_release, local.name)
        mismatch = (
            _asset_mismatch(observed, local)
            if observed is not None
            else "asset is absent"
        )
        if observed is not None and mismatch is None:
            outcome = (
                "verified after the client command failed"
                if result.returncode != 0
                else "verified"
            )
            print(
                f">> {local.name}: {outcome} after {result.elapsed_seconds:.1f}s "
                f"({_verification_description(observed, local)})"
            )
            return

        if not observed_release.draft:
            raise ScriptError(
                f"release {release.tag} was published before {local.name} was verified"
            )
        if observed is not None:
            if result.returncode != 0:
                raise ScriptError(
                    f"upload attempt for {local.name} encountered an invalid asset: "
                    f"{mismatch}; refusing to delete it because upload ownership "
                    "is uncertain"
                )
            print(f">> {local.name}: removing failed upload ({mismatch})")
            _delete_development_release_asset(repository, release, observed)

        reason = _upload_failure_reason(result)
        retryable = result.returncode == 0 or _is_transient_upload_failure(result)
        if not retryable:
            raise _command_error(f"uploading release asset {local.name}", result)
        if attempt == attempts:
            raise ScriptError(
                f"failed to verify release asset {local.name} after "
                f"{attempts} attempts: {reason}"
            )

        delay = retry_backoff_seconds * (2 ** (attempt - 1))
        print(
            f">> {local.name}: attempt {attempt} failed after "
            f"{result.elapsed_seconds:.1f}s; retrying in {delay:g}s: {reason}"
        )
        if delay > 0:
            time.sleep(delay)


def _validate_release_identity(
    release: _DevelopmentRelease,
    tag: str,
    target_sha: str,
) -> None:
    if release.tag != tag:
        raise ScriptError(f"queried release {release.tag}, expected {tag}")
    if release.target_commitish != target_sha:
        raise ScriptError(
            f"release {tag} targets {release.target_commitish}, expected {target_sha}"
        )


def _validate_no_unexpected_assets(
    release: _DevelopmentRelease,
    expected: Sequence[_LocalDevelopmentReleaseAsset],
) -> None:
    expected_names = {asset.name for asset in expected}
    unexpected = sorted(
        asset.name for asset in release.assets if asset.name not in expected_names
    )
    if unexpected:
        raise ScriptError(
            f"development release {release.tag} contains unexpected assets: "
            + ", ".join(unexpected)
        )


def _verify_exact_assets(
    release: _DevelopmentRelease,
    expected: Sequence[_LocalDevelopmentReleaseAsset],
) -> None:
    expected_names = sorted(asset.name for asset in expected)
    actual_names = sorted(asset.name for asset in release.assets)
    if actual_names != expected_names:
        raise ScriptError(
            f"development release {release.tag} asset set is {actual_names}, "
            f"expected {expected_names}"
        )
    for local in expected:
        remote = _asset_named(release, local.name)
        assert remote is not None
        mismatch = _asset_mismatch(remote, local)
        if mismatch is not None:
            raise ScriptError(f"release asset {local.name} is invalid: {mismatch}")
        print(
            f">> {local.name}: final verification passed "
            f"({_verification_description(remote, local)})"
        )


def _publish_development_release_draft(
    repository: str,
    release: _DevelopmentRelease,
) -> _DevelopmentRelease:
    tag = release.tag
    print(f">> publishing verified development release {tag}")
    result = _run_gh_with_retries(
        (
            "api",
            "--method",
            "PATCH",
            f"repos/{repository}/releases/{release.id}",
            "--field",
            "draft=false",
            "--field",
            "prerelease=true",
            "--raw-field",
            "make_latest=false",
        ),
        label=f"publishing development release {tag}",
    )
    published = _require_development_release(repository, release)
    if not published.draft and published.prerelease:
        if result.returncode != 0:
            print(f">> release {tag} was published despite the publish command failure")
        return published
    if result.returncode != 0:
        raise _command_error(f"publishing development release {tag}", result)
    raise ScriptError(f"development release {tag} remained a draft after publication")


def development_release_tag(version: str, target_sha: str) -> str:
    """Names the development release that CI publishes for a commit."""

    return f"v{version}-dev.{target_sha[:12]}"


def publish_development_release(
    repository: str,
    target_sha: str,
    version: str,
    packages: Sequence[Path],
    *,
    upload_attempts: int = DEVELOPMENT_RELEASE_UPLOAD_ATTEMPTS,
    upload_timeout_seconds: float = DEVELOPMENT_RELEASE_UPLOAD_TIMEOUT_SECONDS,
    retry_backoff_seconds: float = DEVELOPMENT_RELEASE_RETRY_BACKOFF_SECONDS,
) -> None:
    if _REPOSITORY_PATTERN.fullmatch(repository) is None:
        raise ScriptError("GitHub repository must be OWNER/REPOSITORY")
    if _TARGET_SHA_PATTERN.fullmatch(target_sha) is None:
        raise ScriptError("development release target must be a full commit SHA")
    if not version:
        raise ScriptError("development release version must not be empty")
    if len(packages) != 3:
        raise ScriptError("development releases require exactly three packages")
    if upload_attempts < 1:
        raise ScriptError("upload attempts must be positive")
    if upload_timeout_seconds <= 0:
        raise ScriptError("upload timeout must be positive")
    if retry_backoff_seconds < 0:
        raise ScriptError("upload retry backoff must not be negative")

    local_assets = tuple(_local_asset(path) for path in packages)
    asset_names = [asset.name for asset in local_assets]
    if len(set(asset_names)) != len(asset_names):
        raise ScriptError("development release package names must be unique")

    short_sha = target_sha[:12]
    tag = development_release_tag(version, target_sha)
    release = _find_development_release(repository, tag)
    if release is None:
        release = _create_development_release(
            repository,
            tag,
            target_sha,
            f"NVX {version} dev {short_sha}",
            f"Automated development release for {target_sha}.",
        )
    print(f">> development release {tag} is release {release.id}")
    _validate_release_identity(release, tag, target_sha)

    if not release.draft:
        _verify_exact_assets(release, local_assets)
        if not release.prerelease:
            raise ScriptError(f"development release {tag} is not a prerelease")
        print(f">> development release {tag} is already published and verified")
        return

    _validate_no_unexpected_assets(release, local_assets)
    for local in local_assets:
        _upload_expected_asset(
            repository,
            release,
            local,
            attempts=upload_attempts,
            timeout_seconds=upload_timeout_seconds,
            retry_backoff_seconds=retry_backoff_seconds,
        )

    release = _require_development_release(repository, release)
    _validate_release_identity(release, tag, target_sha)
    _verify_exact_assets(release, local_assets)
    if not release.draft:
        if not release.prerelease:
            raise ScriptError(f"development release {tag} is not a prerelease")
        print(f">> development release {tag} was already published after verification")
        return

    published = _publish_development_release_draft(repository, release)
    _validate_release_identity(published, tag, target_sha)
    _verify_exact_assets(published, local_assets)
    if published.draft or not published.prerelease:
        raise ScriptError(
            f"development release {tag} was not published as a prerelease"
        )
    print(f">> published development prerelease {tag}")
