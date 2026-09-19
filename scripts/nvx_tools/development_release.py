"""Resumable GitHub development release publication."""

from __future__ import annotations

import json
import re
import subprocess
import time
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import cast

from .common import (
    ScriptError,
    diagnostic_tail,
    require_file,
    sha256_file,
)

DEVELOPMENT_RELEASE_UPLOAD_ATTEMPTS = 3
DEVELOPMENT_RELEASE_UPLOAD_TIMEOUT_SECONDS = 180.0
DEVELOPMENT_RELEASE_RETRY_BACKOFF_SECONDS = 10.0

_GITHUB_COMMAND_TIMEOUT_SECONDS = 60.0
_REPOSITORY_PATTERN = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
_TARGET_SHA_PATTERN = re.compile(r"^[0-9A-Fa-f]{40}$")
_TRANSIENT_UPLOAD_ERROR = re.compile(
    r"(?:"
    r"HTTP\s+(?:408|425|429|5\d\d)\b|"
    r"timed?\s*out|timeout|context deadline exceeded|"
    r"connection (?:aborted|refused|reset)|"
    r"temporary failure|temporarily unavailable|"
    r"unexpected EOF|TLS handshake timeout|broken pipe|"
    r"error saving asset|already[_ ]exists"
    r")",
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


@dataclass(frozen=True)
class _DevelopmentReleaseAsset:
    name: str
    size: int
    digest: str | None
    state: str


@dataclass(frozen=True)
class _DevelopmentRelease:
    tag: str
    target_commitish: str
    draft: bool
    prerelease: bool
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
    diagnostic = diagnostic_tail(result.output)
    suffix = f"\n{diagnostic}" if diagnostic else ""
    return ScriptError(f"{label} {status}{suffix}")


def _parse_release(payload: str) -> _DevelopmentRelease:
    try:
        value: object = json.loads(payload)
    except json.JSONDecodeError as error:
        raise ScriptError("gh returned invalid release JSON") from error
    if not isinstance(value, dict):
        raise ScriptError("gh returned an invalid release document")
    document = cast(dict[str, object], value)
    tag = document.get("tagName")
    target = document.get("targetCommitish")
    draft = document.get("isDraft")
    prerelease = document.get("isPrerelease")
    assets_value = document.get("assets")
    if (
        not isinstance(tag, str)
        or not isinstance(target, str)
        or not isinstance(draft, bool)
        or not isinstance(prerelease, bool)
        or not isinstance(assets_value, list)
    ):
        raise ScriptError("gh returned incomplete release metadata")

    assets: list[_DevelopmentReleaseAsset] = []
    for asset_value in cast(list[object], assets_value):
        if not isinstance(asset_value, dict):
            raise ScriptError("gh returned an invalid release asset")
        asset = cast(dict[str, object], asset_value)
        name = asset.get("name")
        size = asset.get("size")
        digest_value = asset.get("digest")
        state = asset.get("state")
        if (
            not isinstance(name, str)
            or type(size) is not int
            or isinstance(size, bool)
            or not isinstance(state, str)
            or not (digest_value is None or isinstance(digest_value, str))
        ):
            raise ScriptError("gh returned incomplete release asset metadata")
        assets.append(
            _DevelopmentReleaseAsset(
                name,
                size,
                digest_value,
                state,
            )
        )
    return _DevelopmentRelease(tag, target, draft, prerelease, tuple(assets))


def _query_development_release(
    repository: str,
    tag: str,
) -> _DevelopmentRelease | None:
    result = _run_gh(
        (
            "release",
            "view",
            tag,
            "--repo",
            repository,
            "--json",
            "tagName,isDraft,isPrerelease,targetCommitish,assets",
        ),
        timeout_seconds=_GITHUB_COMMAND_TIMEOUT_SECONDS,
    )
    if result.returncode == 0:
        return _parse_release(result.stdout)
    if not result.timed_out and result.output.strip().lower() == "release not found":
        return None
    raise _command_error(f"querying development release {tag}", result)


def _require_development_release(
    repository: str,
    tag: str,
) -> _DevelopmentRelease:
    release = _query_development_release(repository, tag)
    if release is None:
        raise ScriptError(f"development release {tag} disappeared")
    return release


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
            "release",
            "create",
            tag,
            "--repo",
            repository,
            "--target",
            target_sha,
            "--title",
            title,
            "--notes",
            notes,
            "--draft",
            "--prerelease",
            "--latest=false",
        ),
        timeout_seconds=_GITHUB_COMMAND_TIMEOUT_SECONDS,
    )
    release = _query_development_release(repository, tag)
    if release is not None:
        if result.returncode != 0:
            print(
                f">> release {tag} exists despite the create command failure; "
                "resuming it"
            )
        return release
    if result.returncode != 0:
        raise _command_error(f"creating development release {tag}", result)
    raise ScriptError(f"created development release {tag} was not found")


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
    tag: str,
    asset_name: str,
) -> None:
    result = _run_gh(
        (
            "release",
            "delete-asset",
            tag,
            asset_name,
            "--repo",
            repository,
            "--yes",
        ),
        timeout_seconds=_GITHUB_COMMAND_TIMEOUT_SECONDS,
    )
    release = _require_development_release(repository, tag)
    if _asset_named(release, asset_name) is None:
        if result.returncode != 0:
            print(
                f">> asset {asset_name} is absent despite the delete command failure; "
                "continuing"
            )
        return
    if result.returncode != 0:
        raise _command_error(f"deleting release asset {asset_name}", result)
    raise ScriptError(f"release asset {asset_name} still exists after deletion")


def _is_transient_upload_failure(result: _GitHubCommandResult) -> bool:
    return result.timed_out or _TRANSIENT_UPLOAD_ERROR.search(result.output) is not None


def _upload_failure_reason(result: _GitHubCommandResult) -> str:
    if result.timed_out:
        return "command timed out"
    diagnostic = diagnostic_tail(result.output, lines=5)
    if diagnostic:
        return diagnostic
    if result.returncode == 0:
        return "upload command completed but the asset was not verified"
    return f"upload command exited {result.returncode}"


def _upload_expected_asset(
    repository: str,
    tag: str,
    local: _LocalDevelopmentReleaseAsset,
    *,
    attempts: int,
    timeout_seconds: float,
    retry_backoff_seconds: float,
) -> None:
    release = _require_development_release(repository, tag)
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
                "release",
                "upload",
                tag,
                str(local.path),
                "--repo",
                repository,
            ),
            timeout_seconds=timeout_seconds,
        )

        observed_release = _require_development_release(repository, tag)
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
                f"release {tag} was published before {local.name} was verified"
            )
        if observed is not None:
            if result.returncode != 0:
                raise ScriptError(
                    f"upload attempt for {local.name} encountered an invalid asset: "
                    f"{mismatch}; refusing to delete it because upload ownership "
                    "is uncertain"
                )
            print(f">> {local.name}: removing failed upload ({mismatch})")
            _delete_development_release_asset(repository, tag, local.name)

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
    tag: str,
) -> _DevelopmentRelease:
    print(f">> publishing verified development release {tag}")
    result = _run_gh(
        (
            "release",
            "edit",
            tag,
            "--repo",
            repository,
            "--draft=false",
            "--prerelease",
        ),
        timeout_seconds=_GITHUB_COMMAND_TIMEOUT_SECONDS,
    )
    release = _require_development_release(repository, tag)
    if not release.draft and release.prerelease:
        if result.returncode != 0:
            print(f">> release {tag} was published despite the edit command failure")
        return release
    if result.returncode != 0:
        raise _command_error(f"publishing development release {tag}", result)
    raise ScriptError(f"development release {tag} remained a draft after publication")


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
    tag = f"v{version}-dev.{short_sha}"
    release = _query_development_release(repository, tag)
    if release is None:
        release = _create_development_release(
            repository,
            tag,
            target_sha,
            f"NVX {version} dev {short_sha}",
            f"Automated development release for {target_sha}.",
        )
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
            tag,
            local,
            attempts=upload_attempts,
            timeout_seconds=upload_timeout_seconds,
            retry_backoff_seconds=retry_backoff_seconds,
        )

    release = _require_development_release(repository, tag)
    _validate_release_identity(release, tag, target_sha)
    _verify_exact_assets(release, local_assets)
    if not release.draft:
        if not release.prerelease:
            raise ScriptError(f"development release {tag} is not a prerelease")
        print(f">> development release {tag} was already published after verification")
        return

    published = _publish_development_release_draft(repository, tag)
    _validate_release_identity(published, tag, target_sha)
    _verify_exact_assets(published, local_assets)
    if published.draft or not published.prerelease:
        raise ScriptError(
            f"development release {tag} was not published as a prerelease"
        )
    print(f">> published development prerelease {tag}")
