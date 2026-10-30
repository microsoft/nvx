#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import io
import json
import os
import re
import sys
import tarfile
import tempfile
import unittest
import zipfile
from collections.abc import Callable, Mapping, Sequence
from dataclasses import asdict, dataclass, field, replace
from pathlib import Path
from unittest.mock import call, patch
from urllib.parse import parse_qs, urlsplit

import yaml

sys.path.insert(0, str(Path(__file__).parent))
import publish_development_release  # noqa: E402
from nvx_tools import development_release  # noqa: E402
from nvx_tools.build_constants import BuildConstants  # noqa: E402
from nvx_tools.common import ScriptError, sha256_file  # noqa: E402

_Result = development_release._GitHubCommandResult
_LocalAsset = development_release._LocalDevelopmentReleaseAsset

REPOSITORY = "example/nvx"
RELEASES = f"repos/{REPOSITORY}/releases"
TARGET = "a" * 40
TAG = "v1.2.3-dev.aaaaaaaaaaaa"


def _http_error(status: int, message: str) -> _Result:
    """Return how `gh api` reports a response with an error status."""
    body = json.dumps({"message": message, "status": str(status)})
    return _Result(1, body, f"gh: {message} (HTTP {status})\n", 0.1)


def _reason(result: _Result) -> str:
    """Return how publication quotes a failure: the body that gh printed, then
    its error."""
    return f"{result.stdout}\n{result.stderr.strip()}"


_UPLOAD = f'Post "https://uploads.github.com/repos/{REPOSITORY}/releases/1/assets'
_READ = f'Get "https://api.github.com/{RELEASES}/1"'
_TCP = "tcp 10.1.0.4:5432->140.82.112.6:443"
# How Go and gh report requests that got no response.
LOST_CONNECTIONS = (
    f'{_UPLOAD}?name=a.zip": read {_TCP}: read: connection reset by peer',
    f'{_UPLOAD}?name=a.zip": EOF',
    f'{_UPLOAD}?name=a.zip": unexpected EOF',
    f'{_UPLOAD}?name=a.zip": write {_TCP}: write: broken pipe',
    f'{_UPLOAD}?name=a.zip": write {_TCP}: use of closed network connection',
    f'{_UPLOAD}?name=a.zip": http2: client connection lost',
    f'{_UPLOAD}?name=a.zip": http2: server sent GOAWAY and closed the connection; '
    'LastStreamID=1, ErrCode=NO_ERROR, debug=""',
    f'{_UPLOAD}?name=a.zip": stream error: stream ID 1; INTERNAL_ERROR; '
    "received from peer",
    f"{_READ}: net/http: TLS handshake timeout",
    f"{_READ}: dial tcp 140.82.112.6:443: connect: network is unreachable",
    "error connecting to api.github.com\n"
    "check your internet connection or https://githubstatus.com",
)


@dataclass
class _FakeAsset:
    id: int
    name: str
    size: int
    digest: str | None
    state: str


@dataclass
class _FakeRelease:
    id: int
    tag: str
    target: str
    draft: bool
    prerelease: bool
    assets: list[_FakeAsset] = field(default_factory=lambda: [])

    def document(self) -> dict[str, object]:
        return {
            "id": self.id,
            "tag_name": self.tag,
            "target_commitish": self.target,
            "draft": self.draft,
            "prerelease": self.prerelease,
            "upload_url": (
                f"https://uploads.github.com/repos/{REPOSITORY}/releases/"
                f"{self.id}/assets{{?name,label}}"
            ),
            "assets": [asdict(asset) for asset in self.assets],
        }

    def snapshot(self) -> development_release._DevelopmentRelease:
        return development_release._release_from_document(self.document())


@dataclass(frozen=True)
class _Fault:
    result: _Result
    applied: bool
    effect: Callable[[], object] | None


@dataclass(frozen=True)
class _ApiRequest:
    method: str
    endpoint: str
    flags: frozenset[str]
    fields: dict[str, object]
    input: Path | None
    headers: dict[str, str]


def _field_value(value: str) -> object:
    """Convert the value of a `gh api --field` as gh does."""
    if value in ("true", "false"):
        return value == "true"
    if value == "null":
        return None
    if value.isdigit():
        return int(value)
    return value


def _parse_api_command(command: tuple[str, ...]) -> _ApiRequest:
    if command[:1] != ("api",):
        raise AssertionError(f"unexpected gh command {command!r}")
    method = "GET"
    endpoint: str | None = None
    flags: set[str] = set()
    fields: dict[str, object] = {}
    input_path: Path | None = None
    headers: dict[str, str] = {}
    arguments = iter(command[1:])
    for argument in arguments:
        if argument == "--method":
            method = next(arguments)
        elif argument in ("--paginate", "--slurp"):
            flags.add(argument)
        elif argument in ("--field", "--raw-field"):
            key, value = next(arguments).split("=", 1)
            fields[key] = _field_value(value) if argument == "--field" else value
        elif argument == "--input":
            input_path = Path(next(arguments))
        elif argument == "--header":
            name, value = next(arguments).split(":", 1)
            headers[name] = value.strip()
        elif argument.startswith("-") or endpoint is not None:
            raise AssertionError(f"unexpected gh api argument {argument!r}")
        else:
            endpoint = argument
    if endpoint is None:
        raise AssertionError(f"gh api command without an endpoint: {command!r}")
    return _ApiRequest(
        method,
        endpoint,
        frozenset(flags),
        fields,
        input_path,
        headers,
    )


class _FakeGitHub:
    """Model the releases of a repository behind the `gh api` requests of
    publication.

    Each request is one operation: `list`, `create`, `read`, `upload`,
    `delete`, or `publish`. A fault makes the next request of its operation
    return a given result instead, after `effect` changed the releases and, if
    `applied` is set, after GitHub carried the request out anyway.
    """

    def __init__(self) -> None:
        self.releases: list[_FakeRelease] = []
        self.operations: list[str] = []
        self.commands: list[tuple[str, ...]] = []
        self.timeouts: list[float] = []
        self.uploads: list[str] = []
        self.created: list[dict[str, object]] = []
        self.published: list[dict[str, object]] = []
        self.deleted: list[int] = []
        # The states of the next uploaded assets, which are otherwise uploaded.
        self.upload_states: list[str] = []
        self.page_size = 100
        self._faults: dict[str, list[_Fault]] = {}
        self._next_id = 1000

    def add_release(
        self,
        tag: str,
        *,
        draft: bool = True,
        prerelease: bool = True,
        target: str = TARGET,
    ) -> _FakeRelease:
        self._next_id += 1
        release = _FakeRelease(self._next_id, tag, target, draft, prerelease)
        # GitHub lists the newest release first.
        self.releases.insert(0, release)
        return release

    def add_asset(
        self,
        release: _FakeRelease,
        path: Path,
        *,
        state: str = "uploaded",
    ) -> _FakeAsset:
        self._next_id += 1
        asset = _FakeAsset(
            self._next_id,
            path.name,
            path.stat().st_size,
            f"sha256:{sha256_file(path)}",
            state,
        )
        release.assets.append(asset)
        return asset

    def fail(
        self,
        operation: str,
        result: _Result,
        *,
        applied: bool = False,
        effect: Callable[[], object] | None = None,
    ) -> None:
        self._faults.setdefault(operation, []).append(_Fault(result, applied, effect))

    def __call__(self, args: Sequence[str], *, timeout_seconds: float) -> _Result:
        command = tuple(args)
        self.commands.append(command)
        self.timeouts.append(timeout_seconds)
        operation, respond = self._route(command)
        self.operations.append(operation)
        faults = self._faults.get(operation)
        if not faults:
            return respond()
        fault = faults.pop(0)
        if fault.effect is not None:
            fault.effect()
        if fault.applied:
            respond()
        return fault.result

    def _release(self, release_id: int) -> _FakeRelease | None:
        return next(
            (release for release in self.releases if release.id == release_id),
            None,
        )

    def _route(self, command: tuple[str, ...]) -> tuple[str, Callable[[], _Result]]:
        request = _parse_api_command(command)
        if request.method == "GET" and request.endpoint == f"{RELEASES}?per_page=100":
            if request.flags != frozenset({"--paginate", "--slurp"}):
                raise AssertionError(f"unexpected listing {command!r}")
            return "list", self._list
        if request.method == "POST" and request.endpoint == RELEASES:
            return "create", lambda: self._create(request.fields)
        match = re.fullmatch(rf"{re.escape(RELEASES)}/(\d+)", request.endpoint)
        if request.method == "GET" and match is not None:
            release_id = int(match.group(1))
            return "read", lambda: self._read(release_id)
        if request.method == "PATCH" and match is not None:
            release_id = int(match.group(1))
            return "publish", lambda: self._publish(release_id, request.fields)
        match = re.fullmatch(rf"{re.escape(RELEASES)}/assets/(\d+)", request.endpoint)
        if request.method == "DELETE" and match is not None:
            asset_id = int(match.group(1))
            return "delete", lambda: self._delete(asset_id)
        url = urlsplit(request.endpoint)
        match = re.fullmatch(
            rf"/repos/{re.escape(REPOSITORY)}/releases/(\d+)/assets",
            url.path,
        )
        if (
            request.method == "POST"
            and url.scheme == "https"
            and url.netloc == "uploads.github.com"
            and match is not None
            and request.input is not None
        ):
            query = parse_qs(url.query, strict_parsing=True)
            path = request.input
            if query != {"name": [path.name]} or "Content-Type" not in request.headers:
                raise AssertionError(f"unexpected upload {command!r}")
            self.uploads.append(path.name)
            release_id = int(match.group(1))
            return "upload", lambda: self._upload(release_id, path)
        raise AssertionError(f"unexpected gh api request {command!r}")

    def _list(self) -> _Result:
        size = self.page_size
        pages = [
            [release.document() for release in self.releases[start : start + size]]
            for start in range(0, len(self.releases), size)
        ]
        return _Result(0, json.dumps(pages or [[]]), "", 0.1)

    def _create(self, fields: dict[str, object]) -> _Result:
        self.created.append(fields)
        release = self.add_release(
            str(fields["tag_name"]),
            draft=fields["draft"] is True,
            prerelease=fields["prerelease"] is True,
            target=str(fields["target_commitish"]),
        )
        return _Result(0, json.dumps(release.document()), "", 0.1)

    def _read(self, release_id: int) -> _Result:
        release = self._release(release_id)
        if release is None:
            return _http_error(404, "Not Found")
        return _Result(0, json.dumps(release.document()), "", 0.1)

    def _upload(self, release_id: int, path: Path) -> _Result:
        release = self._release(release_id)
        if release is None:
            return _http_error(404, "Not Found")
        if any(asset.name == path.name for asset in release.assets):
            body = {
                "message": "Validation Failed",
                "errors": [
                    {
                        "resource": "ReleaseAsset",
                        "code": "already_exists",
                        "field": "name",
                    }
                ],
            }
            return _Result(
                1, json.dumps(body), "gh: Validation Failed (HTTP 422)\n", 0.1
            )
        state = self.upload_states.pop(0) if self.upload_states else "uploaded"
        asset = self.add_asset(release, path, state=state)
        return _Result(0, json.dumps(asdict(asset)), "", 0.1)

    def _delete(self, asset_id: int) -> _Result:
        for release in self.releases:
            for asset in release.assets:
                if asset.id == asset_id:
                    release.assets.remove(asset)
                    self.deleted.append(asset_id)
                    return _Result(0, "", "", 0.1)
        return _http_error(404, "Not Found")

    def _publish(self, release_id: int, fields: dict[str, object]) -> _Result:
        release = self._release(release_id)
        if release is None:
            return _http_error(404, "Not Found")
        self.published.append(fields)
        release.draft = fields["draft"] is True
        release.prerelease = fields["prerelease"] is True
        return _Result(0, json.dumps(release.document()), "", 0.1)


class DevelopmentReleaseTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.github = _FakeGitHub()
        self.stdout = io.StringIO()
        run = patch.object(development_release, "_run_gh", self.github)
        run.start()
        self.addCleanup(run.stop)
        sleep = patch.object(development_release.time, "sleep")
        self.sleep = sleep.start()
        self.addCleanup(sleep.stop)
        stdout = patch("sys.stdout", self.stdout)
        stdout.start()
        self.addCleanup(stdout.stop)

    def _package(self, name: str, contents: bytes) -> _LocalAsset:
        path = self.root / name
        path.write_bytes(contents)
        return development_release._local_asset(path)

    def _packages(self) -> list[_LocalAsset]:
        return [
            self._package("nvx-linux-kvm.tar.gz", b"kvm"),
            self._package("nvx-linux-mshv.tar.gz", b"mshv"),
            self._package("nvx-windows-whp.zip", b"whp"),
        ]

    def _result(
        self,
        returncode: int = 0,
        *,
        stderr: str = "",
        elapsed: float = 1.0,
        timed_out: bool = False,
    ) -> _Result:
        return _Result(returncode, "", stderr, elapsed, timed_out)

    def _upload(self, release: _FakeRelease, local: _LocalAsset) -> None:
        development_release._upload_expected_asset(
            REPOSITORY,
            release.snapshot(),
            local,
            attempts=3,
            timeout_seconds=180,
            retry_backoff_seconds=10,
        )

    def _publish(self, packages: Sequence[_LocalAsset]) -> None:
        development_release.publish_development_release(
            REPOSITORY,
            TARGET,
            "1.2.3",
            [package.path for package in packages],
        )

    def _commands(self, operation: str) -> list[tuple[str, ...]]:
        return [
            command
            for name, command in zip(
                self.github.operations,
                self.github.commands,
                strict=True,
            )
            if name == operation
        ]

    def _timeouts(self, operation: str) -> list[float]:
        return [
            timeout
            for name, timeout in zip(
                self.github.operations,
                self.github.timeouts,
                strict=True,
            )
            if name == operation
        ]

    def test_transient_upload_failure_retries_and_succeeds(self):
        local = self._package("nvx-linux.tar.gz", b"package")
        release = self.github.add_release("v1")
        failure = _http_error(502, "Bad Gateway")
        self.github.fail("upload", failure)

        self._upload(release, local)

        self.assertEqual(
            self.github.operations,
            ["read", "upload", "read", "upload", "read"],
        )
        first, second = self._commands("upload")
        self.assertEqual(first, second)
        self.assertEqual(self._timeouts("upload"), [180, 180])
        self.sleep.assert_called_once_with(10)
        self.assertIn(
            f"retrying in 10s: {_reason(failure)}\n",
            self.stdout.getvalue(),
        )

    def test_upload_that_lost_its_connection_is_retried(self):
        local = self._package("nvx-linux.tar.gz", b"package")
        for error in LOST_CONNECTIONS:
            with self.subTest(error=error):
                release = self.github.add_release("v1")
                self.github.fail("upload", self._result(1, stderr=f"{error}\n"))
                start = len(self.github.operations)
                self.sleep.reset_mock()

                self._upload(release, local)

                self.assertEqual(
                    self.github.operations[start:],
                    ["read", "upload", "read", "upload", "read"],
                )
                self.sleep.assert_called_once_with(10)
                self.assertEqual(
                    [asset.state for asset in release.assets],
                    ["uploaded"],
                )

    def test_upload_retry_exhaustion_fails_with_bounded_attempts(self):
        local = self._package("nvx-linux.tar.gz", b"package")
        release = self.github.add_release("v1")
        for _ in range(3):
            self.github.fail(
                "upload",
                self._result(124, elapsed=180, timed_out=True),
            )

        with self.assertRaisesRegex(ScriptError, "after 3 attempts"):
            self._upload(release, local)

        self.assertEqual(self.github.operations.count("upload"), 3)
        self.assertEqual(self.sleep.call_args_list, [call(10), call(20)])

    def test_server_error_upload_failure_exhausts_bounded_attempts(self):
        local = self._package("nvx-linux.tar.gz", b"package")
        release = self.github.add_release("v1")
        failure = _http_error(502, "Bad Gateway")
        for _ in range(3):
            self.github.fail("upload", failure)

        with self.assertRaisesRegex(
            ScriptError,
            f"after 3 attempts: {re.escape(_reason(failure))}$",
        ):
            self._upload(release, local)

        self.assertEqual(self.github.operations.count("upload"), 3)
        self.assertEqual(self.sleep.call_args_list, [call(10), call(20)])

    def test_upload_failure_stops_when_release_disappears(self):
        local = self._package("nvx-linux.tar.gz", b"package")
        release = self.github.add_release("v1")
        self.github.fail(
            "upload",
            _http_error(404, "Not Found"),
            effect=self.github.releases.clear,
        )

        with self.assertRaisesRegex(
            ScriptError,
            "^development release v1 disappeared$",
        ):
            self._upload(release, local)

        self.assertEqual(self.github.operations, ["read", "upload", "read"])
        self.sleep.assert_not_called()

    def test_upload_failure_stops_when_release_was_published(self):
        local = self._package("nvx-linux.tar.gz", b"package")
        release = self.github.add_release("v1")

        def publish() -> None:
            release.draft = False

        self.github.fail("upload", _http_error(502, "Bad Gateway"), effect=publish)

        with self.assertRaisesRegex(ScriptError, "release v1 was published before"):
            self._upload(release, local)

        self.assertEqual(self.github.operations, ["read", "upload", "read"])
        self.sleep.assert_not_called()

    def test_permanent_upload_failure_is_not_retried(self):
        local = self._package("nvx-linux.tar.gz", b"package")
        failures = (
            _http_error(403, "Resource not accessible by integration"),
            # The release exists, so the status of the upload is its own.
            _http_error(404, "Not Found"),
        )
        for failure in failures:
            with self.subTest(failure=failure.stderr):
                release = self.github.add_release("v1")
                self.github.fail("upload", failure)
                start = len(self.github.operations)

                with self.assertRaisesRegex(
                    ScriptError,
                    "^uploading release asset nvx-linux.tar.gz exited 1\n.*\n"
                    f"{re.escape(failure.stderr.strip())}$",
                ):
                    self._upload(release, local)

                self.assertEqual(
                    self.github.operations[start:],
                    ["read", "upload", "read"],
                )
                self.assertEqual(release.assets, [])
        self.sleep.assert_not_called()

    def test_upload_rejection_that_a_later_attempt_can_overcome_is_retried(self):
        local = self._package("nvx-linux.tar.gz", b"package")
        taken = {
            "message": "Validation Failed",
            "errors": [
                {"resource": "ReleaseAsset", "code": "already_exists", "field": "name"}
            ],
        }
        rejections = (
            _Result(1, json.dumps(taken), "gh: Validation Failed (HTTP 422)\n", 0.1),
            _http_error(422, "Error saving asset"),
        )
        for rejection in rejections:
            with self.subTest(rejection=rejection.stdout):
                release = self.github.add_release("v1")
                self.github.fail("upload", rejection)
                start = len(self.github.operations)
                self.sleep.reset_mock()

                self._upload(release, local)

                self.assertEqual(
                    self.github.operations[start:],
                    ["read", "upload", "read", "upload", "read"],
                )
                self.sleep.assert_called_once_with(10)
                # The retry says why GitHub rejected the upload.
                self.assertIn(
                    f"retrying in 10s: {_reason(rejection)}\n",
                    self.stdout.getvalue(),
                )

    def test_transient_failures_are_timeouts_lost_connections_and_some_statuses(
        self,
    ):
        transient = (
            self._result(124, timed_out=True),
            _http_error(408, "Request Timeout"),
            _http_error(425, "Too Early"),
            _http_error(429, "Too Many Requests"),
            _http_error(500, "Server Error"),
            self._result(1, stderr="gh: HTTP 502\n"),
            _http_error(503, "Service Unavailable"),
            *(self._result(1, stderr=f"{error}\n") for error in LOST_CONNECTIONS),
        )
        permanent = (
            _http_error(400, "Bad Request"),
            _http_error(401, "Bad credentials"),
            _http_error(403, "Resource not accessible by integration"),
            _http_error(404, "Not Found"),
            _http_error(422, "Validation Failed"),
            # The status that gh reported is the response's, not one that the
            # body mentions.
            _Result(1, "HTTP 502: timed out", "gh: Not Found (HTTP 404)\n", 0.1),
            self._result(1, stderr="unexpected end of JSON input\n"),
            self._result(1, stderr="open dist/nvx.zip: no such file or directory\n"),
            self._result(
                4,
                stderr="To get started with GitHub CLI, please run:  gh auth login\n",
            ),
        )
        for result in transient:
            with self.subTest(result=result):
                self.assertTrue(development_release._is_transient_failure(result))
                self.assertTrue(
                    development_release._is_transient_upload_failure(result)
                )
        for result in permanent:
            with self.subTest(result=result):
                self.assertFalse(development_release._is_transient_failure(result))
                self.assertFalse(
                    development_release._is_transient_upload_failure(result)
                )

    def test_publication_recovers_from_a_failed_upload(self):
        packages = publish_development_release.release_packages(self.root, "1.2.3")
        for platform, package in packages.items():
            package.write_bytes(platform.encode("ascii"))
        failure = _http_error(502, "Bad Gateway")
        self.github.fail("upload", failure)

        development_release.publish_development_release(
            REPOSITORY,
            TARGET,
            "1.2.3",
            tuple(packages.values()),
        )

        self.assertEqual(
            self.github.operations,
            [
                "list",
                "create",
                "read",
                "upload",
                "read",
                "upload",
                "read",
                "read",
                "upload",
                "read",
                "read",
                "upload",
                "read",
                "read",
                "publish",
                "read",
            ],
        )
        (release,) = self.github.releases
        self.assertFalse(release.draft)
        self.assertTrue(release.prerelease)
        self.assertEqual(
            sorted(asset.name for asset in release.assets),
            sorted(package.name for package in packages.values()),
        )
        self.sleep.assert_called_once_with(10)
        output = self.stdout.getvalue()
        self.assertIn(f"retrying in 10s: {_reason(failure)}\n", output)
        self.assertIn(f"development release {TAG} is release {release.id}", output)
        self.assertIn(f"published development prerelease {TAG}", output)

    def test_publication_addresses_its_release_by_id(self):
        packages = self._packages()

        self._publish(packages)

        (release,) = self.github.releases
        read = ("api", f"{RELEASES}/{release.id}")
        uploads = [
            (
                "api",
                "--method",
                "POST",
                f"https://uploads.github.com/repos/{REPOSITORY}/releases/"
                f"{release.id}/assets?name={package.name}",
                "--input",
                str(package.path),
                "--header",
                f"Content-Type: {content_type}",
            )
            for package, content_type in zip(
                packages,
                ("application/x-gtar", "application/x-gtar", "application/zip"),
                strict=True,
            )
        ]
        publish = (
            "api",
            "--method",
            "PATCH",
            f"{RELEASES}/{release.id}",
            "--field",
            "draft=false",
            "--field",
            "prerelease=true",
            "--raw-field",
            "make_latest=false",
        )
        self.assertEqual(self.github.operations[:2], ["list", "create"])
        self.assertEqual(
            self.github.commands[2:],
            [
                read,
                uploads[0],
                read,
                read,
                uploads[1],
                read,
                read,
                uploads[2],
                read,
                read,
                publish,
                read,
            ],
        )
        self.assertEqual(
            self.github.published,
            [{"draft": False, "prerelease": True, "make_latest": "false"}],
        )
        self.assertEqual(self._timeouts("list"), [120])
        self.assertEqual(set(self._timeouts("read")), {20})
        self.assertEqual(self._timeouts("upload"), [180, 180, 180])
        self.assertEqual(self._timeouts("publish"), [20])

    def test_upload_url_drops_its_template_and_encodes_the_name(self):
        release = self.github.add_release("v1").snapshot()

        self.assertEqual(
            development_release._asset_upload_url(release, "nvx 1+2.tar.gz"),
            f"https://uploads.github.com/repos/{REPOSITORY}/releases/{release.id}"
            "/assets?name=nvx+1%2B2.tar.gz",
        )
        for upload_url in ("http://uploads.github.com/assets{?name,label}", ""):
            with (
                self.subTest(upload_url=upload_url),
                self.assertRaisesRegex(ScriptError, "invalid upload URL"),
            ):
                development_release._asset_upload_url(
                    replace(release, upload_url=upload_url),
                    "nvx.zip",
                )

    def test_assets_upload_with_the_content_types_of_gh_release_upload(self):
        for name, content_type in (
            ("nvx-0.1.0-linux-kvm.tar.gz", "application/x-gtar"),
            ("NVX.TGZ", "application/x-gtar"),
            ("nvx-0.1.0-windows-whp.zip", "application/zip"),
            ("SHA256SUMS", "application/octet-stream"),
        ):
            with self.subTest(name=name):
                self.assertEqual(
                    development_release._asset_content_type(name),
                    content_type,
                )

    def test_partial_draft_resumes_without_reuploading_valid_asset(self):
        packages = self._packages()
        release = self.github.add_release(TAG)
        self.github.add_asset(release, packages[0].path)

        self._publish(packages)

        self.assertEqual(
            self.github.uploads,
            ["nvx-linux-mshv.tar.gz", "nvx-windows-whp.zip"],
        )
        self.assertNotIn("create", self.github.operations)
        self.assertNotIn("delete", self.github.operations)
        self.assertEqual(self.github.operations[-2:], ["publish", "read"])
        self.assertFalse(release.draft)

    def test_mismatched_existing_asset_is_rejected_without_mutation(self):
        local = self._package("nvx-linux.tar.gz", b"package")
        cases: tuple[tuple[str, dict[str, object]], ...] = (
            ("digest", {"digest": f"sha256:{'0' * 64}"}),
            ("missing-digest", {"digest": None}),
            ("size", {"size": local.size + 1}),
            ("state", {"state": "new"}),
        )
        for name, changes in cases:
            with self.subTest(name=name):
                release = self.github.add_release("v1")
                asset = self.github.add_asset(release, local.path)
                release.assets = [replace(asset, **changes)]
                start = len(self.github.operations)

                with self.assertRaisesRegex(
                    ScriptError,
                    "already exists but is invalid.*refusing to replace",
                ):
                    self._upload(release, local)

                self.assertEqual(self.github.operations[start:], ["read"])

    def test_failed_asset_created_by_successful_attempt_is_cleaned_up(self):
        local = self._package("nvx-linux.tar.gz", b"package")
        release = self.github.add_release("v1")
        self.github.upload_states.append("new")

        self._upload(release, local)

        self.assertEqual(
            self.github.operations,
            ["read", "upload", "read", "delete", "read", "upload", "read"],
        )
        (deleted,) = self.github.deleted
        self.assertEqual(
            self._commands("delete"),
            [("api", "--method", "DELETE", f"{RELEASES}/assets/{deleted}")],
        )
        (asset,) = release.assets
        self.assertNotEqual(asset.id, deleted)
        self.assertEqual(asset.state, "uploaded")
        self.sleep.assert_called_once_with(10)

    def test_failed_upload_does_not_delete_asset_with_uncertain_ownership(self):
        local = self._package("nvx-linux.tar.gz", b"package")
        release = self.github.add_release("v1")
        self.github.upload_states.append("new")
        self.github.fail(
            "upload",
            self._result(124, timed_out=True),
            applied=True,
        )

        with self.assertRaisesRegex(ScriptError, "ownership is uncertain"):
            self._upload(release, local)

        self.assertEqual(self.github.operations, ["read", "upload", "read"])
        self.assertEqual([asset.state for asset in release.assets], ["new"])
        self.sleep.assert_not_called()

    def test_timed_out_client_detects_server_side_upload(self):
        local = self._package("nvx-linux.tar.gz", b"package")
        release = self.github.add_release("v1")
        self.github.fail(
            "upload",
            self._result(124, elapsed=180, timed_out=True),
            applied=True,
        )

        self._upload(release, local)

        self.assertEqual(self.github.operations, ["read", "upload", "read"])
        self.sleep.assert_not_called()

    def test_final_asset_verification_gates_publication(self):
        packages = self._packages()
        release = self.github.add_release(TAG)
        for package in packages[:2]:
            self.github.add_asset(release, package.path)

        with (
            patch.object(development_release, "_upload_expected_asset") as upload,
            self.assertRaisesRegex(ScriptError, "asset set"),
        ):
            self._publish(packages)

        self.assertEqual(upload.call_count, 3)
        self.assertEqual(self.github.operations, ["list", "read"])
        self.assertTrue(release.draft)

    def test_new_release_is_created_as_draft_without_assets(self):
        release = development_release._create_development_release(
            REPOSITORY,
            "v1",
            TARGET,
            "title",
            "notes",
        )

        self.assertEqual(
            self.github.commands,
            [
                (
                    "api",
                    "--method",
                    "POST",
                    RELEASES,
                    "--raw-field",
                    "tag_name=v1",
                    "--raw-field",
                    f"target_commitish={TARGET}",
                    "--raw-field",
                    "name=title",
                    "--raw-field",
                    "body=notes",
                    "--field",
                    "draft=true",
                    "--field",
                    "prerelease=true",
                    "--raw-field",
                    "make_latest=false",
                )
            ],
        )
        self.assertEqual(
            self.github.created,
            [
                {
                    "tag_name": "v1",
                    "target_commitish": TARGET,
                    "name": "title",
                    "body": "notes",
                    "draft": True,
                    "prerelease": True,
                    "make_latest": "false",
                }
            ],
        )
        (created,) = self.github.releases
        self.assertEqual(release.id, created.id)
        self.assertTrue(release.draft)
        self.assertTrue(release.prerelease)
        self.assertEqual(release.assets, ())

    def test_publication_finds_its_release_among_pages_of_others(self):
        packages = self._packages()
        release = self.github.add_release(TAG)
        for index in range(3):
            self.github.add_release(f"v1.2.3-dev.{index:012d}", draft=False)
        self.github.page_size = 2

        self._publish(packages)

        self.assertEqual(
            self.github.commands[0],
            ("api", "--paginate", "--slurp", f"{RELEASES}?per_page=100"),
        )
        self.assertNotIn("create", self.github.operations)
        self.assertEqual(self.github.operations.count("list"), 1)
        self.assertFalse(release.draft)
        self.assertIn(
            f"development release {TAG} is release {release.id}",
            self.stdout.getvalue(),
        )

    def test_releases_that_share_the_tag_fail_publication_without_changes(self):
        first = self.github.add_release(TAG)
        second = self.github.add_release(TAG, draft=False)
        self.github.add_release("v1.2.3-dev.bbbbbbbbbbbb")

        with self.assertRaisesRegex(
            ScriptError,
            f"^releases {first.id}, {second.id} all carry development release tag "
            f"{re.escape(TAG)}; refusing to change any of them$",
        ):
            self._publish(self._packages())

        self.assertEqual(self.github.operations, ["list"])

    def test_release_that_a_shifted_page_lists_twice_is_no_duplicate(self):
        release = self.github.add_release(TAG)
        other = self.github.add_release("v1.2.3-dev.bbbbbbbbbbbb")
        listing = json.dumps(
            [[release.document()], [release.document(), other.document()]]
        )

        with patch.object(
            development_release,
            "_run_gh",
            return_value=_Result(0, listing, "", 0.1),
        ):
            found = development_release._find_development_release(REPOSITORY, TAG)

        self.assertEqual(found, release.snapshot())

    def test_create_failure_resumes_the_draft_that_github_created(self):
        packages = self._packages()
        self.github.fail(
            "create",
            self._result(124, elapsed=60, timed_out=True),
            applied=True,
        )

        self._publish(packages)

        self.assertEqual(self.github.operations[:4], ["list", "create", "list", "read"])
        self.assertEqual(self.github.operations.count("create"), 1)
        (release,) = self.github.releases
        self.assertFalse(release.draft)
        self.assertIn(
            f"release {TAG} exists despite the create command failure; resuming it",
            self.stdout.getvalue(),
        )

    def test_create_failure_without_a_draft_fails_with_its_status(self):
        self.github.fail("create", _http_error(500, "Server Error"))

        with self.assertRaisesRegex(
            ScriptError,
            f"^creating development release {re.escape(TAG)} exited 1\n"
            r".*\ngh: Server Error \(HTTP 500\)$",
        ):
            self._publish(self._packages())

        # Creating is never repeated: a second request could duplicate a draft
        # that the listing did not show yet.
        self.assertEqual(self.github.operations, ["list", "create", "list"])
        self.assertEqual(self.github.releases, [])
        self.sleep.assert_not_called()

    def test_create_failure_that_left_drafts_sharing_the_tag_fails(self):
        def create_twice() -> None:
            self.github.add_release(TAG)
            self.github.add_release(TAG)

        self.github.fail(
            "create",
            self._result(124, elapsed=60, timed_out=True),
            effect=create_twice,
        )

        with self.assertRaises(ScriptError) as raised:
            self._publish(self._packages())

        ids = ", ".join(str(release.id) for release in reversed(self.github.releases))
        self.assertEqual(
            str(raised.exception),
            f"releases {ids} all carry development release tag {TAG}; "
            "refusing to change any of them",
        )
        self.assertEqual(self.github.operations, ["list", "create", "list"])

    def test_release_that_is_gone_fails_as_disappeared(self):
        release = self.github.add_release("v1")
        self.github.releases.clear()

        with self.assertRaisesRegex(
            ScriptError, "^development release v1 disappeared$"
        ):
            development_release._require_development_release(
                REPOSITORY,
                release.snapshot(),
            )

        self.assertEqual(self.github.commands, [("api", f"{RELEASES}/{release.id}")])

    def test_failed_read_fails_with_its_status(self):
        release = self.github.add_release("v1")
        self.github.fail(
            "read",
            _http_error(403, "Resource not accessible by integration"),
        )

        with self.assertRaisesRegex(
            ScriptError,
            r"^reading development release v1 exited 1\n.*\n"
            r"gh: Resource not accessible by integration \(HTTP 403\)$",
        ):
            development_release._require_development_release(
                REPOSITORY,
                release.snapshot(),
            )

        self.assertEqual(self.github.operations, ["read"])
        self.sleep.assert_not_called()

    def test_transient_read_failure_is_retried_within_bounds(self):
        release = self.github.add_release("v1")
        failure = _http_error(502, "Bad Gateway")
        self.github.fail("read", failure)
        self.github.fail("read", self._result(124, elapsed=60, timed_out=True))

        observed = development_release._require_development_release(
            REPOSITORY,
            release.snapshot(),
        )

        self.assertEqual(observed, release.snapshot())
        self.assertEqual(self.github.operations, ["read", "read", "read"])
        self.assertEqual(self.sleep.call_args_list, [call(5), call(10)])
        output = self.stdout.getvalue()
        self.assertIn(
            "reading development release v1 failed; retrying in 5s: "
            f"{_reason(failure)}\n",
            output,
        )
        self.assertIn(
            "reading development release v1 failed; retrying in 10s: "
            "command timed out\n",
            output,
        )

    def test_transient_read_failure_exhausts_bounded_attempts(self):
        release = self.github.add_release("v1")
        for _ in range(3):
            self.github.fail("read", _http_error(503, "Service Unavailable"))

        with self.assertRaisesRegex(
            ScriptError,
            r"^reading development release v1 exited 1\n.*\n"
            r"gh: Service Unavailable \(HTTP 503\)$",
        ):
            development_release._require_development_release(
                REPOSITORY,
                release.snapshot(),
            )

        self.assertEqual(self.github.operations, ["read", "read", "read"])
        self.assertEqual(self.sleep.call_args_list, [call(5), call(10)])

    def test_listing_that_timed_out_is_retried(self):
        release = self.github.add_release(TAG)
        self.github.fail("list", self._result(124, elapsed=300, timed_out=True))

        found = development_release._find_development_release(REPOSITORY, TAG)

        self.assertEqual(found, release.snapshot())
        self.assertEqual(self._timeouts("list"), [120, 120])
        self.sleep.assert_called_once_with(5)

    def test_deletion_that_failed_transiently_is_retried(self):
        local = self._package("nvx-linux.tar.gz", b"package")
        release = self.github.add_release("v1")
        self.github.upload_states.append("new")
        self.github.fail("delete", _http_error(502, "Bad Gateway"))

        self._upload(release, local)

        self.assertEqual(
            self.github.operations,
            ["read", "upload", "read", "delete", "delete", "read", "upload", "read"],
        )
        self.assertEqual(self.sleep.call_args_list, [call(5), call(10)])
        self.assertEqual([asset.state for asset in release.assets], ["uploaded"])

    def test_deletion_that_github_carried_out_despite_its_failure_continues(self):
        local = self._package("nvx-linux.tar.gz", b"package")
        release = self.github.add_release("v1")
        self.github.upload_states.append("new")
        self.github.fail("delete", self._result(124, timed_out=True), applied=True)

        self._upload(release, local)

        # The repeated request finds the asset gone, which the read confirms.
        self.assertEqual(
            self.github.operations,
            ["read", "upload", "read", "delete", "delete", "read", "upload", "read"],
        )
        self.assertIn(
            "asset nvx-linux.tar.gz is absent despite the delete command failure",
            self.stdout.getvalue(),
        )
        self.assertEqual([asset.state for asset in release.assets], ["uploaded"])

    def test_publication_that_failed_transiently_is_retried(self):
        packages = self._packages()
        release = self.github.add_release(TAG)
        for package in packages:
            self.github.add_asset(release, package.path)
        self.github.fail("publish", _http_error(502, "Bad Gateway"))

        self._publish(packages)

        self.assertEqual(self.github.operations[-3:], ["publish", "publish", "read"])
        self.assertFalse(release.draft)
        self.sleep.assert_called_once_with(5)

    def test_release_published_despite_failed_publish_requests_is_accepted(self):
        packages = self._packages()
        release = self.github.add_release(TAG)
        for package in packages:
            self.github.add_asset(release, package.path)
        timeout = self._result(124, elapsed=60, timed_out=True)
        self.github.fail("publish", timeout, applied=True)
        self.github.fail("publish", timeout)
        self.github.fail("publish", timeout)

        self._publish(packages)

        self.assertEqual(
            self.github.operations[-4:],
            ["publish", "publish", "publish", "read"],
        )
        self.assertFalse(release.draft)
        self.assertIn(
            f"release {TAG} was published despite the publish command failure",
            self.stdout.getvalue(),
        )

    def test_draft_that_failed_publish_requests_left_fails_with_their_status(self):
        packages = self._packages()
        release = self.github.add_release(TAG)
        for package in packages:
            self.github.add_asset(release, package.path)
        for _ in range(3):
            self.github.fail("publish", _http_error(502, "Bad Gateway"))

        with self.assertRaisesRegex(
            ScriptError,
            f"^publishing development release {re.escape(TAG)} exited 1\n.*\n"
            r"gh: Bad Gateway \(HTTP 502\)$",
        ):
            self._publish(packages)

        self.assertEqual(self.github.operations.count("publish"), 3)
        self.assertTrue(release.draft)

    def test_release_job_outlasts_every_attempt_of_publication(self):
        # The job must not cancel publication while its bounded attempts are
        # not spent: when every request takes all of its attempts, each to its
        # timeout, and every upload leaves an invalid asset to delete. The
        # job's checkout and artifact downloads take a minute or two.
        d = development_release

        def attempts(count: int, timeout: float, backoff: float) -> float:
            return count * timeout + backoff * (2 ** (count - 1) - 1)

        request = attempts(
            d._GITHUB_REQUEST_ATTEMPTS,
            d._GITHUB_COMMAND_TIMEOUT_SECONDS,
            d._GITHUB_REQUEST_RETRY_BACKOFF_SECONDS,
        )
        listing = attempts(
            d._GITHUB_REQUEST_ATTEMPTS,
            d._GITHUB_LIST_TIMEOUT_SECONDS,
            d._GITHUB_REQUEST_RETRY_BACKOFF_SECONDS,
        )
        # A package is read for, then uploaded in attempts that each read the
        # release, delete the asset that they left, and read the release again.
        package = request + attempts(
            d.DEVELOPMENT_RELEASE_UPLOAD_ATTEMPTS,
            d.DEVELOPMENT_RELEASE_UPLOAD_TIMEOUT_SECONDS + 3 * request,
            d.DEVELOPMENT_RELEASE_RETRY_BACKOFF_SECONDS,
        )
        # Listing, a create request that fails, listing again, the packages,
        # and a read before the publication, which a read follows.
        publication = (
            2 * listing
            + d._GITHUB_COMMAND_TIMEOUT_SECONDS
            + len(publish_development_release.PLATFORMS) * package
            + 3 * request
        )
        workflow = (
            BuildConstants.REPO_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        job = yaml.safe_load(workflow)["jobs"]["release"]
        self.assertEqual(
            job["steps"][-1]["uses"],
            "./.github/actions/publish-development-release",
        )
        self.assertLessEqual(publication + 5 * 60, job["timeout-minutes"] * 60)

    def test_http_status_comes_from_the_error_that_gh_api_reported(self):
        cases: tuple[tuple[str, int | None], ...] = (
            ("gh: Not Found (HTTP 404)\n", 404),
            ("gh: HTTP 502\n", 502),
            ("gh: Bad Gateway (HTTP 502)", 502),
            (
                "gh: Validation Failed (HTTP 422)\n"
                'gh: This API operation needs the "repo" scope.\n',
                422,
            ),
            ("release not found\n", None),
            ('Post "https://uploads.github.com": connection reset by peer\n', None),
            ("", None),
        )
        for stderr, status in cases:
            with self.subTest(stderr=stderr):
                self.assertEqual(
                    development_release._http_status(self._result(1, stderr=stderr)),
                    status,
                )
        # A body that gh printed is not the response's status.
        self.assertIsNone(
            development_release._http_status(_Result(1, "gh: HTTP 500", "", 0.1))
        )

    def test_release_documents_need_their_rest_fields(self):
        release = self.github.add_release("v1")
        local = self._package("nvx-linux.tar.gz", b"package")
        self.github.add_asset(release, local.path)
        valid = release.document()
        self.assertEqual(
            development_release._parse_release(json.dumps(valid)),
            release.snapshot(),
        )
        asset = asdict(release.assets[0])
        cases: tuple[tuple[str, dict[str, object], str], ...] = (
            ("no-id", {"id": None}, "incomplete release metadata"),
            ("boolean-id", {"id": True}, "incomplete release metadata"),
            ("no-upload-url", {"upload_url": None}, "incomplete release metadata"),
            (
                "gh-json",
                {"tag_name": None, "tagName": "v1"},
                "incomplete release metadata",
            ),
            (
                "asset-without-id",
                {"assets": [{**asset, "id": None}]},
                "incomplete release asset metadata",
            ),
        )
        for name, changes, error in cases:
            with self.subTest(name=name), self.assertRaisesRegex(ScriptError, error):
                development_release._parse_release(json.dumps({**valid, **changes}))

    def test_failed_listing_quotes_only_the_end_of_its_output(self):
        documents = [
            self.github.add_release(f"v{index}").document() for index in range(100)
        ]
        failure = _Result(
            1,
            json.dumps([documents]) + '{"message":"Server Error"}',
            "gh: Server Error (HTTP 502)\n",
            0.1,
        )

        with (
            patch.object(
                development_release,
                "_run_gh",
                return_value=failure,
            ) as run,
            self.assertRaises(ScriptError) as raised,
        ):
            development_release._find_development_release(REPOSITORY, TAG)

        self.assertEqual(run.call_count, 3)
        self.assertEqual(self.sleep.call_args_list, [call(5), call(10)])
        message = str(raised.exception)
        self.assertTrue(
            message.startswith(f"listing releases of {REPOSITORY} exited 1\n..."),
            message[:200],
        )
        self.assertTrue(
            message.endswith('{"message":"Server Error"}\ngh: Server Error (HTTP 502)')
        )
        self.assertLess(len(message), 1200)


def _write_package(path: Path, members: Mapping[str, bytes | None]) -> None:
    """Write a release archive whose package root holds `members`, files with
    their contents, or directories where the contents are None."""
    path.parent.mkdir(parents=True, exist_ok=True)
    root = path.name.removesuffix(".tar.gz").removesuffix(".zip")
    if path.name.endswith(".tar.gz"):
        with tarfile.open(path, "w:gz") as archive:
            for name, data in members.items():
                info = tarfile.TarInfo(f"{root}/{name}")
                if data is None:
                    info.type = tarfile.DIRTYPE
                    archive.addfile(info)
                    continue
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
        return
    with zipfile.ZipFile(path, "w") as archive:
        for name, data in members.items():
            archive.writestr(
                f"{root}/{name}" + ("/" if data is None else ""), data or b""
            )


class DevelopmentReleasePackageTests(unittest.TestCase):
    """The packages that publication checks: each ships OpenVMM and nvx-rs."""

    def _packages(self, directory: Path) -> dict[str, Path]:
        packages = publish_development_release.release_packages(directory, "1.2.3")
        for platform, package in packages.items():
            suffix = ".exe" if platform.startswith("windows-") else ""
            _write_package(
                package,
                {
                    "SHA256SUMS": b"sums",
                    f"bin/openvmm{suffix}": b"openvmm",
                    f"bin/nvx-rs{suffix}": b"nvx-rs",
                },
            )
        return packages

    def test_packages_are_named_after_their_platforms(self):
        self.assertEqual(
            publish_development_release.release_packages(Path("dist"), "1.2.3"),
            {
                "linux-kvm": Path("dist") / "nvx-1.2.3-linux-kvm.tar.gz",
                "linux-mshv": Path("dist") / "nvx-1.2.3-linux-mshv.tar.gz",
                "windows-whp": Path("dist") / "nvx-1.2.3-windows-whp.zip",
            },
        )

    def test_every_package_must_ship_openvmm_and_nvx_rs(self):
        with tempfile.TemporaryDirectory() as temporary:
            packages = self._packages(Path(temporary))
            publish_development_release.check_release_packages(packages)
            for platform, members, missing in (
                ("linux-kvm", {"bin/openvmm": b"openvmm"}, "bin/nvx-rs"),
                ("linux-mshv", {"bin/nvx-rs": b"nvx-rs"}, "bin/openvmm"),
                (
                    "linux-kvm",
                    {"bin/openvmm": b"openvmm", "bin/nvx-rs": None},
                    "bin/nvx-rs",
                ),
                # Windows packages ship Windows executables.
                (
                    "windows-whp",
                    {"bin/openvmm.exe": b"openvmm", "bin/nvx-rs": b"nvx-rs"},
                    "bin/nvx-rs.exe",
                ),
            ):
                with self.subTest(platform=platform, members=sorted(members)):
                    package = packages[platform]
                    package.unlink()
                    _write_package(package, members)
                    with self.assertRaisesRegex(
                        ScriptError,
                        f"^{re.escape(package.name)} does not ship {missing}$",
                    ):
                        publish_development_release.check_release_packages(packages)
                    self._packages(Path(temporary))
            packages["linux-mshv"].unlink()
            with self.assertRaisesRegex(
                ScriptError, "development release package nvx-1.2.3-linux-mshv.tar.gz"
            ):
                publish_development_release.check_release_packages(packages)

    def test_publication_checks_the_packages_before_it_publishes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "VERSION").write_text("1.2.3\n", encoding="ascii")
            packages = self._packages(root / "dist")
            environment = {"GH_REPO": "example/nvx", "GITHUB_SHA": "a" * 40}
            previous = os.getcwd()
            os.chdir(root)
            try:
                with (
                    patch.dict(os.environ, environment),
                    patch.object(BuildConstants, "REPO_ROOT", root),
                    patch.object(
                        publish_development_release, "publish_development_release"
                    ) as publish,
                ):
                    publish_development_release.main()
                    publish.assert_called_once_with(
                        "example/nvx",
                        "a" * 40,
                        "1.2.3",
                        tuple(
                            Path("dist") / package.name for package in packages.values()
                        ),
                    )
                    publish.reset_mock()
                    _write_package(packages["windows-whp"], {"bin/openvmm.exe": b""})
                    with self.assertRaisesRegex(ScriptError, "bin/nvx-rs.exe"):
                        publish_development_release.main()
                    publish.assert_not_called()
            finally:
                os.chdir(previous)


if __name__ == "__main__":
    unittest.main()
