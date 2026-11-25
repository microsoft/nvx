#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import contextlib
import hashlib
import http.server
import io
import json
import os
import re
import shutil
import signal
import sys
import tempfile
import threading
import time
import unittest
import zipfile
from collections.abc import Callable, Generator, Mapping
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import nvx  # noqa: E402
from nvx_tools import (  # noqa: E402
    common,
    guest_artifact_reuse,
    merge_queue,
    release,
)
from nvx_tools.build_constants import (  # noqa: E402
    KernelBuildConstants,
    UbuntuBuildConstants,
)
from nvx_tools_testing import REUSE_JOBS as _REUSE_JOBS
from nvx_tools_testing import guest_uploads as _guest_uploads
from nvx_tools_testing import (
    write_release_fixture as _write_release_fixture,
)

_REUSE_REPOSITORY = "microsoft/nvx"
_REUSE_WORKFLOW = ".github/workflows/ci.yml"
_REUSE_WORKFLOW_REF = f"{_REUSE_REPOSITORY}/{_REUSE_WORKFLOW}@refs/heads/dev"
_REUSE_MERGE = "a" * 40
_REUSE_HEAD = "c" * 40
_REUSE_KEYS = {
    "kernel": "linux-kernel-v1-6.18.38-1111-2222",
    "kernel-debug": "linux-kernel-debug-v1-6.18.38-3333-2222",
    "alpine": "alpine-image-v2-3.24.1-4444-5555",
    "ubuntu": "ubuntu-initramfs-v2-26.04.1-6666-5555",
    "ubuntu-distro": "ubuntu-distro-v2-26.04.1-6666-5555",
    "azurelinux": "azurelinux-initramfs-v2-3.0-7777-5555",
}
# The artifact of each job's record.
_REUSE_RECORDS: dict[tuple[str, ...], str] = {
    ("kernel", "alpine"): "kernel-alpine-record",
    ("ubuntu", "azurelinux"): "ubuntu-azurelinux-record",
}
_REUSE_RUNS = (
    f"repos/{_REUSE_REPOSITORY}/actions/workflows/ci.yml/runs"
    f"?event=pull_request&head_sha={_REUSE_HEAD}&per_page=100"
)
# The ID of each artifact of run 102 that the fixtures serve.
_REUSE_ARTIFACT_IDS = {
    "kernel-alpine-record": 9,
    "guest-kernel": 10,
    "guest-alpine": 11,
    "ubuntu-azurelinux-record": 19,
    "guest-ubuntu": 20,
    "guest-ubuntu-distro": 21,
    "guest-azurelinux": 22,
}


def _reuse_list(name: str) -> str:
    """Return the endpoint that lists the artifacts named `name` of run 102."""
    return (
        f"repos/{_REUSE_REPOSITORY}/actions/runs/102/artifacts?name={name}&per_page=100"
    )


def _reuse_download(name: str) -> str:
    """Return the endpoint that downloads the artifact `name` of run 102."""
    return (
        f"repos/{_REUSE_REPOSITORY}/actions/artifacts/{_REUSE_ARTIFACT_IDS[name]}/zip"
    )


class _FakeGitHub:
    """Answers reuse's GitHub API requests from canned documents."""

    def __init__(
        self,
        documents: Mapping[str, object],
        downloads: Mapping[str, bytes] | None = None,
    ) -> None:
        self.documents = dict(documents)
        self.downloads = dict(downloads or {})
        self.requests: list[str] = []

    def get(self, endpoint: str) -> object:
        self.requests.append(endpoint)
        if endpoint not in self.documents:
            raise guest_artifact_reuse.ReuseRefused(
                f"GET {endpoint} failed with HTTP 404"
            )
        return self.documents[endpoint]

    def download(self, endpoint: str, destination: Path) -> None:
        self.requests.append(endpoint)
        if endpoint not in self.downloads:
            raise guest_artifact_reuse.ReuseRefused(
                f"GET {endpoint} failed with HTTP 404"
            )
        destination.write_bytes(self.downloads[endpoint])


def _reuse_run(
    run_id: int,
    run_number: int,
    *,
    head_repository: str | None = _REUSE_REPOSITORY,
    event: str = "pull_request",
    head_sha: str = _REUSE_HEAD,
    path: str = _REUSE_WORKFLOW,
) -> dict[str, object]:
    return {
        "id": run_id,
        "run_number": run_number,
        "event": event,
        "head_sha": head_sha,
        "path": path,
        "html_url": f"https://github.com/{_REUSE_REPOSITORY}/actions/runs/{run_id}",
        "head_repository": (
            None if head_repository is None else {"full_name": head_repository}
        ),
        "repository": {"full_name": _REUSE_REPOSITORY},
    }


def _reuse_artifact(
    artifact_id: int,
    run_id: int,
    *,
    name: str,
    expired: bool = False,
    created_at: str = "2026-10-21T12:00:00Z",
    digest: str | None = None,
) -> dict[str, object]:
    return {
        "id": artifact_id,
        "name": name,
        "expired": expired,
        "created_at": created_at,
        "digest": digest,
        "workflow_run": {"id": run_id},
    }


def _reuse_zip(members: Mapping[str, bytes]) -> bytes:
    """Return a ZIP archive of `members`, as an artifact download is."""
    archive = io.BytesIO()
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as bundle:
        for name, contents in members.items():
            bundle.writestr(name, contents)
    return archive.getvalue()


def _reuse_request(
    components: tuple[str, ...] = ("kernel", "alpine"),
    *,
    keys: Mapping[str, str] = _REUSE_KEYS,
    dry_run: bool = False,
) -> guest_artifact_reuse.ReuseRequest:
    return guest_artifact_reuse.ReuseRequest(
        repository=_REUSE_REPOSITORY,
        workflow_ref=_REUSE_WORKFLOW_REF,
        record=_REUSE_RECORDS[components],
        components=components,
        keys=guest_artifact_reuse.component_keys(components, keys),
        commit=None if dry_run else _REUSE_MERGE,
        head_sha=_REUSE_HEAD if dry_run else None,
        dry_run=dry_run,
    )


class GuestArtifactReuseTests(unittest.TestCase):
    template: Path
    kernel_inputs: dict[str, object]
    _template_directory: tempfile.TemporaryDirectory[str]

    @classmethod
    def setUpClass(cls) -> None:
        super().setUpClass()
        # Valid guest artifacts and their provenance.
        cls._template_directory = tempfile.TemporaryDirectory()
        paths, cls.kernel_inputs, _revision = _write_release_fixture(
            Path(cls._template_directory.name)
        )
        cls.template = paths["build"]

    @classmethod
    def tearDownClass(cls) -> None:
        cls._template_directory.cleanup()
        super().tearDownClass()

    @staticmethod
    def _serve(
        api: _FakeGitHub,
        name: str,
        contents: bytes,
        *,
        digest: str | None = None,
    ) -> None:
        """Serve `contents` as the artifact `name` of run 102, with the
        SHA-256 digest of `contents` unless `digest` replaces it."""
        if digest is None:
            digest = f"sha256:{hashlib.sha256(contents).hexdigest()}"
        api.documents[_reuse_list(name)] = {
            "artifacts": [
                _reuse_artifact(
                    _REUSE_ARTIFACT_IDS[name], 102, name=name, digest=digest
                )
            ]
        }
        api.downloads[_reuse_download(name)] = contents

    def _fixture(
        self,
        root: Path,
        components: tuple[str, ...] = ("kernel", "alpine"),
    ) -> tuple[_FakeGitHub, dict[str, object], Path]:
        """Return a fake GitHub API whose merge of _REUSE_HEAD and the latest
        run for it lead to the record of `components` and to their artifacts,
        as ci.yml uploads them, the kernel provenance inputs that the files
        match, and a copy of the files and their record under `root`."""
        files = root / "files"
        files.mkdir()
        for name in guest_artifact_reuse.component_files(components):
            shutil.copyfile(self.template / name, files / name)
        record = guest_artifact_reuse.record_guest_artifacts(
            files, components, _REUSE_KEYS
        )
        api = _FakeGitHub(
            {
                f"repos/{_REUSE_REPOSITORY}/git/commits/{_REUSE_MERGE}": {
                    "sha": _REUSE_MERGE,
                    "parents": [{"sha": "b" * 40}, {"sha": _REUSE_HEAD}],
                },
                _REUSE_RUNS: {
                    "workflow_runs": [
                        _reuse_run(103, 1851, head_repository="fork/nvx"),
                        _reuse_run(102, 1850),
                        _reuse_run(101, 1849),
                    ]
                },
            }
        )
        self._serve(
            api,
            _REUSE_RECORDS[components],
            _reuse_zip({record.name: record.read_bytes()}),
        )
        uploads = _guest_uploads()
        for component in components:
            for name in guest_artifact_reuse.COMPONENT_ARTIFACTS[component]:
                self._serve(
                    api,
                    name,
                    _reuse_zip(
                        {
                            member: (files / member).read_bytes()
                            for member in sorted(uploads[name][1])
                        }
                    ),
                )
        return api, self.kernel_inputs, files

    def _reuse(
        self,
        api: _FakeGitHub,
        kernel_inputs: dict[str, object],
        destination: Path,
        request: guest_artifact_reuse.ReuseRequest | None = None,
    ) -> guest_artifact_reuse.ReuseOutcome:
        with patch.object(
            release, "kernel_provenance_inputs", return_value=kernel_inputs
        ):
            outcome = guest_artifact_reuse.reuse_guest_artifacts(
                api, request or _reuse_request(), destination
            )
        # Reuse leaves no staging directory behind.
        if destination.exists():
            self.assertEqual(
                [path.name for path in destination.iterdir() if path.is_dir()], []
            )
        return outcome

    def _assert_built(
        self,
        outcome: guest_artifact_reuse.ReuseOutcome,
        destination: Path,
        reason: str,
        level: str,
    ) -> None:
        self.assertFalse(outcome.reused)
        self.assertEqual(outcome.status, "build")
        self.assertEqual(outcome.level, level)
        self.assertRegex(outcome.detail, re.compile(reason, re.IGNORECASE))
        self.assertTrue(
            outcome.detail.endswith(
                ", so the action builds the guest images that missed the cache."
            ),
            outcome.detail,
        )
        installed = (
            [path.name for path in destination.iterdir()]
            if destination.exists()
            else []
        )
        self.assertEqual(installed, [])

    def test_record_names_the_cache_keys_and_the_digest_of_every_file(self):
        for components in _REUSE_JOBS:
            with (
                self.subTest(components=components),
                tempfile.TemporaryDirectory() as temporary,
            ):
                directory = Path(temporary)
                for name in guest_artifact_reuse.component_files(components):
                    shutil.copyfile(self.template / name, directory / name)
                # No record covers the CI debug kernel.
                (directory / KernelBuildConstants.DEBUG_BINARY_NAME).write_bytes(
                    b"debug kernel"
                )
                stdout = io.StringIO()
                args = nvx.parse_args(
                    [
                        "record-guest-artifacts",
                        "--components",
                        " ".join(reversed(components)),
                        "--keys",
                        json.dumps(_REUSE_KEYS, indent=2),
                        "--directory",
                        os.fspath(directory),
                    ]
                )
                with contextlib.redirect_stdout(stdout):
                    args.handler(args)

                record = json.loads(
                    (directory / guest_artifact_reuse.RECORD_NAME).read_text(
                        encoding="utf-8"
                    )
                )
                # Only the keys of the components' own cache entries.
                self.assertEqual(
                    record,
                    {
                        "format": 1,
                        "keys": {
                            name: _REUSE_KEYS[name]
                            for name in sorted(
                                name
                                for component in components
                                for name in guest_artifact_reuse.COMPONENT_CACHE_KEYS[
                                    component
                                ]
                            )
                        },
                        "files": {
                            name: common.sha256_file(directory / name)
                            for component in components
                            for name in release.GUEST_ARTIFACT_COMPONENTS[component]
                        },
                    },
                )
                # The Ubuntu component holds the sandbox scratch template too.
                self.assertEqual(
                    len(record["files"]),
                    {("kernel", "alpine"): 6, ("ubuntu", "azurelinux"): 7}[components],
                )
                self.assertNotIn("kernel-debug", record["keys"])
                self.assertIn(guest_artifact_reuse.RECORD_NAME, stdout.getvalue())

                first = guest_artifact_reuse.component_files(components)[1]
                (directory / first).unlink()
                with self.assertRaisesRegex(
                    common.ScriptError, f"guest artifact {re.escape(first)} not found"
                ):
                    guest_artifact_reuse.record_guest_artifacts(
                        directory, components, _REUSE_KEYS
                    )

    def test_record_requires_the_key_of_each_cache_entry(self):
        keys = {
            name: key for name, key in _REUSE_KEYS.items() if name != "ubuntu-distro"
        }
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            components = ("ubuntu", "azurelinux")
            for name in guest_artifact_reuse.component_files(components):
                shutil.copyfile(self.template / name, directory / name)
            guest_artifact_reuse.record_guest_artifacts(
                directory, ("azurelinux",), keys
            )
            with self.assertRaisesRegex(
                common.ScriptError, "the cache keys give no ubuntu-distro key"
            ):
                guest_artifact_reuse.record_guest_artifacts(directory, components, keys)

    def test_record_components_are_those_that_a_package_ships(self):
        self.assertEqual(
            list(guest_artifact_reuse.COMPONENT_CACHE_KEYS),
            list(release.GUEST_ARTIFACT_COMPONENTS),
        )
        for text, expected in (
            ("kernel alpine", ("kernel", "alpine")),
            (" alpine\nkernel  kernel ", ("kernel", "alpine")),
            ("azurelinux ubuntu", ("ubuntu", "azurelinux")),
        ):
            with self.subTest(components=text):
                self.assertEqual(guest_artifact_reuse.parse_components(text), expected)
        for text in (
            "",
            "   ",
            "debug-kernel",
            "kernel debug-kernel",
            "Kernel",
            "kernel,alpine",
        ):
            with (
                self.subTest(components=text),
                self.assertRaises(common.ScriptError),
            ):
                guest_artifact_reuse.parse_components(text)

    def test_cache_keys_must_be_a_json_object_of_nonempty_strings(self):
        self.assertEqual(
            guest_artifact_reuse.parse_cache_keys(json.dumps(_REUSE_KEYS)),
            _REUSE_KEYS,
        )
        for text in (
            "",
            "kernel",
            "[]",
            "{}",
            '{"kernel": ""}',
            '{"kernel": 1}',
            '{"": "key"}',
            '{"kernel": "a", "kernel": "b"}',
        ):
            with (
                self.subTest(keys=text),
                self.assertRaises(common.ScriptError),
            ):
                guest_artifact_reuse.parse_cache_keys(text)

    def test_push_installs_every_file_of_its_components_when_one_missed(self):
        for components, cached in (
            # The cache restored the kernel but not the Alpine initramfs.
            (
                ("kernel", "alpine"),
                (
                    KernelBuildConstants.BINARY_NAME,
                    KernelBuildConstants.CONFIG_NAME,
                    KernelBuildConstants.PROVENANCE_NAME,
                ),
            ),
            # A restore that failed partway left a truncated distro layer.
            (("ubuntu", "azurelinux"), (UbuntuBuildConstants.DISTRO_NAME,)),
        ):
            with (
                self.subTest(components=components),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                api, kernel_inputs, files = self._fixture(root, components)
                destination = root / "destination"
                destination.mkdir()
                for name in cached:
                    (destination / name).write_bytes(b"cached " + name.encode())
                # The files of the other components stay as they are.
                (destination / "other").write_bytes(b"other")

                outcome = self._reuse(
                    api, kernel_inputs, destination, _reuse_request(components)
                )

                self.assertTrue(outcome.reused, outcome.detail)
                self.assertEqual(outcome.status, "reused")
                self.assertIsNone(outcome.level)
                self.assertEqual(
                    outcome.heading,
                    f"Guest artifact reuse ({', '.join(components)})",
                )
                self.assertIn(
                    "Installed every file of these components", outcome.detail
                )
                self.assertIn(
                    "[run 102](https://github.com/microsoft/nvx/actions/runs/102)",
                    outcome.detail,
                )
                artifacts = [
                    name
                    for component in components
                    for name in guest_artifact_reuse.COMPONENT_ARTIFACTS[component]
                ]
                record = _REUSE_RECORDS[components]
                self.assertEqual(
                    api.requests,
                    [
                        f"repos/{_REUSE_REPOSITORY}/git/commits/{_REUSE_MERGE}",
                        _REUSE_RUNS,
                        _reuse_list(record),
                        _reuse_download(record),
                        *(
                            endpoint
                            for name in artifacts
                            for endpoint in (_reuse_list(name), _reuse_download(name))
                        ),
                    ],
                )
                # Every file of the components comes from the checked
                # artifacts, and no other file.
                names = guest_artifact_reuse.component_files(components)
                self.assertEqual(
                    sorted(path.name for path in destination.iterdir()),
                    sorted((*names, "other")),
                )
                for name in names:
                    with self.subTest(file=name):
                        self.assertEqual(
                            (destination / name).read_bytes(),
                            (files / name).read_bytes(),
                        )
                self.assertEqual((destination / "other").read_bytes(), b"other")

    def test_rehearsal_checks_the_artifacts_without_installing_them(self):
        for components in _REUSE_JOBS:
            with (
                self.subTest(components=components),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                api, kernel_inputs, _files = self._fixture(root, components)
                destination = root / "destination"
                destination.mkdir()
                # A rehearsal checks the artifacts even when the cache
                # restored every file.
                for name in guest_artifact_reuse.component_files(components):
                    (destination / name).write_bytes(b"cached")

                outcome = self._reuse(
                    api,
                    kernel_inputs,
                    destination,
                    _reuse_request(components, dry_run=True),
                )

                self.assertFalse(outcome.reused)
                self.assertEqual(
                    outcome.heading,
                    f"Guest artifact reuse rehearsal ({', '.join(components)})",
                )
                self.assertEqual(outcome.status, "would reuse", outcome.detail)
                self.assertIsNone(outcome.level)
                # The rehearsal names the head, so it needs no merge commit.
                self.assertEqual(api.requests[0], _REUSE_RUNS)
                self.assertIn(_reuse_download(_REUSE_RECORDS[components]), api.requests)
                for name in guest_artifact_reuse.component_files(components):
                    self.assertEqual((destination / name).read_bytes(), b"cached")

                # A rehearsal that its own run can't serve warns, even for a
                # reason that a push would only note.
                record = _REUSE_RECORDS[components]
                api.documents[_reuse_list(record)] = {
                    "artifacts": [
                        _reuse_artifact(
                            _REUSE_ARTIFACT_IDS[record], 102, name=record, expired=True
                        )
                    ]
                }

                outcome = self._reuse(
                    api,
                    kernel_inputs,
                    destination,
                    _reuse_request(components, dry_run=True),
                )

                self.assertFalse(outcome.reused)
                self.assertEqual(outcome.status, "build")
                self.assertEqual(outcome.level, "warning")
                self.assertTrue(
                    outcome.detail.endswith(
                        f"has no unexpired `{record}` artifact, so a merge group "
                        "or a push that merged this head would build the guest "
                        "images that missed its cache."
                    ),
                    outcome.detail,
                )

    def test_reuse_is_not_needed_when_the_cache_restored_every_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary)
            # Those of the other components don't count.
            for name in guest_artifact_reuse.component_files(("kernel", "alpine")):
                (destination / name).write_bytes(b"cached")
            api = _FakeGitHub({})

            outcome = guest_artifact_reuse.reuse_guest_artifacts(
                api, _reuse_request(), destination
            )

            self.assertFalse(outcome.reused)
            self.assertEqual(outcome.status, "not needed")
            self.assertIsNone(outcome.level)
            self.assertEqual(api.requests, [])

    def test_only_same_repository_pull_request_runs_of_the_workflow_qualify(self):
        def latest(*runs: dict[str, object]) -> guest_artifact_reuse.PullRequestRun:
            api = _FakeGitHub({_REUSE_RUNS: {"workflow_runs": list(runs)}})
            return guest_artifact_reuse.latest_pull_request_run(
                api, _REUSE_REPOSITORY, _REUSE_WORKFLOW, _REUSE_HEAD
            )

        ineligible = (
            _reuse_run(201, 1901, head_repository="fork/nvx"),
            _reuse_run(202, 1902, head_repository=None),
            _reuse_run(203, 1903, event="push"),
            _reuse_run(204, 1904, event="workflow_dispatch"),
            _reuse_run(205, 1905, head_sha="d" * 40),
            _reuse_run(206, 1906, path=".github/workflows/adversarial.yml"),
        )
        self.assertEqual(
            latest(_reuse_run(101, 1849), *ineligible, _reuse_run(102, 1850)),
            guest_artifact_reuse.PullRequestRun(
                102, "https://github.com/microsoft/nvx/actions/runs/102"
            ),
        )
        with self.assertRaises(guest_artifact_reuse.ReuseRefused) as raised:
            latest(*ineligible)
        self.assertTrue(raised.exception.expected)
        self.assertIn(
            f"no pull request run of `{_REUSE_WORKFLOW}` from microsoft/nvx built "
            f"`{_REUSE_HEAD}`; 2 from other repositories never qualify",
            str(raised.exception),
        )
        with self.assertRaises(guest_artifact_reuse.ReuseRefused):
            latest({"event": "pull_request", "head_sha": _REUSE_HEAD})

    def test_fork_runs_never_supply_guest_artifacts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            api, kernel_inputs, _files = self._fixture(root)
            api.documents[_REUSE_RUNS] = {
                "workflow_runs": [
                    _reuse_run(102, 1850, head_repository="fork/nvx"),
                ]
            }
            destination = root / "destination"

            outcome = self._reuse(api, kernel_inputs, destination)

            self._assert_built(
                outcome, destination, "from other repositories never qualify", "notice"
            )
            self.assertEqual(len(api.requests), 2)

    def test_a_push_that_merged_no_pull_request_builds(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            api, kernel_inputs, _files = self._fixture(root)
            api.documents[f"repos/{_REUSE_REPOSITORY}/git/commits/{_REUSE_MERGE}"] = {
                "sha": _REUSE_MERGE,
                "parents": [{"sha": "b" * 40}],
            }
            destination = root / "destination"

            outcome = self._reuse(api, kernel_inputs, destination)

            self._assert_built(
                outcome,
                destination,
                f"^`{_REUSE_MERGE}` is not the merge commit of a pull request",
                "notice",
            )
            self.assertEqual(len(api.requests), 1)

    def test_only_an_unexpired_artifact_of_the_run_qualifies(self):
        run = guest_artifact_reuse.PullRequestRun(102, "https://example.invalid/102")
        name = "kernel-alpine-record"

        def retained(*artifacts: dict[str, object]) -> guest_artifact_reuse.RunArtifact:
            api = _FakeGitHub({_reuse_list(name): {"artifacts": list(artifacts)}})
            return guest_artifact_reuse.retained_artifact(
                api, _REUSE_REPOSITORY, run, name
            )

        ineligible = (
            _reuse_artifact(
                1, 102, name=name, expired=True, created_at="2026-10-21T13:00:00Z"
            ),
            _reuse_artifact(2, 102, name="guest-kernel"),
            _reuse_artifact(3, 101, name=name, created_at="2026-10-21T14:00:00Z"),
        )
        self.assertEqual(
            retained(
                _reuse_artifact(4, 102, name=name, created_at="2026-10-21T11:00:00Z"),
                *ineligible,
                _reuse_artifact(5, 102, name=name, digest="sha256:" + "e" * 64),
            ),
            guest_artifact_reuse.RunArtifact(5, name, "sha256:" + "e" * 64),
        )
        with self.assertRaises(guest_artifact_reuse.ReuseRefused) as raised:
            retained(*ineligible)
        self.assertTrue(raised.exception.expected)
        self.assertEqual(
            str(raised.exception),
            "[run 102](https://example.invalid/102) has no unexpired "
            "`kernel-alpine-record` artifact",
        )

    def test_reuse_needs_the_record_and_every_artifact_of_the_components(self):
        for name in (
            "ubuntu-azurelinux-record",
            "guest-ubuntu",
            "guest-ubuntu-distro",
            "guest-azurelinux",
        ):
            with (
                self.subTest(artifact=name),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                components = ("ubuntu", "azurelinux")
                api, kernel_inputs, _files = self._fixture(root, components)
                # Expired, as after a day, or never uploaded.
                api.documents[_reuse_list(name)] = {
                    "artifacts": [
                        _reuse_artifact(
                            _REUSE_ARTIFACT_IDS[name], 102, name=name, expired=True
                        )
                    ]
                }
                destination = root / "destination"

                outcome = self._reuse(
                    api, kernel_inputs, destination, _reuse_request(components)
                )

                self._assert_built(
                    outcome,
                    destination,
                    re.escape(f"has no unexpired `{name}` artifact"),
                    "notice",
                )
                self.assertNotIn(_reuse_download(name), api.requests)

    def test_reuse_requires_the_recorded_cache_keys_to_equal_its_own(self):
        def record(keys: Mapping[str, str]) -> bytes:
            document = {
                "format": 1,
                "keys": dict(keys),
                "files": {
                    name: "0" * 64
                    for name in guest_artifact_reuse.component_files(
                        ("kernel", "alpine")
                    )
                },
            }
            return _reuse_zip({"guest-artifacts.json": json.dumps(document).encode()})

        own = guest_artifact_reuse.component_keys(("kernel", "alpine"), _REUSE_KEYS)
        for case, keys, record_keys, names in (
            (
                "changed",
                dict(_REUSE_KEYS, alpine="alpine-image-v2-3.24.1-other-5555"),
                own,
                "alpine",
            ),
            ("missing", _REUSE_KEYS, {"alpine": own["alpine"]}, "kernel"),
            ("extra", _REUSE_KEYS, dict(own, ubuntu=_REUSE_KEYS["ubuntu"]), "ubuntu"),
            ("other", _REUSE_KEYS, {"kernel": "other"}, "alpine, kernel"),
        ):
            with (
                self.subTest(case=case),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                api, kernel_inputs, _files = self._fixture(root)
                if record_keys is not own:
                    self._serve(api, "kernel-alpine-record", record(record_keys))
                destination = root / "destination"

                outcome = self._reuse(
                    api, kernel_inputs, destination, _reuse_request(keys=keys)
                )

                self._assert_built(
                    outcome,
                    destination,
                    f"gives other cache keys for {re.escape(names)},",
                    "notice",
                )
                # Other keys refuse the components before any of their
                # artifacts downloads.
                self.assertNotIn(_reuse_download("guest-kernel"), api.requests)

        # A change in the key of another job's component does not matter.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            api, kernel_inputs, _files = self._fixture(root)
            destination = root / "destination"

            outcome = self._reuse(
                api,
                kernel_inputs,
                destination,
                _reuse_request(keys=dict(_REUSE_KEYS, ubuntu="other", azurelinux="")),
            )

            self.assertTrue(outcome.reused, outcome.detail)

    def test_reuse_refuses_files_that_do_not_match_the_record_or_artifact(self):
        record_name = "kernel-alpine-record"

        def record(files: Path, update: Mapping[str, object]) -> bytes:
            document = json.loads(
                (files / guest_artifact_reuse.RECORD_NAME).read_bytes()
            )
            document.update(update)
            return _reuse_zip({"guest-artifacts.json": json.dumps(document).encode()})

        def record_files(
            files: Path, change: Callable[[dict[str, str]], object]
        ) -> bytes:
            document = json.loads(
                (files / guest_artifact_reuse.RECORD_NAME).read_bytes()
            )
            change(document["files"])
            return _reuse_zip({"guest-artifacts.json": json.dumps(document).encode()})

        def members(files: Path, name: str) -> dict[str, bytes]:
            return {
                member: (files / member).read_bytes()
                for member in sorted(_guest_uploads()[name][1])
            }

        cases: tuple[tuple[str, str, Callable[[Path], bytes], str, str], ...] = (
            (
                "tampered file",
                "guest-alpine",
                lambda files: _reuse_zip(
                    {**members(files, "guest-alpine"), "initramfs.cpio.gz": b"x"}
                ),
                "initramfs.cpio.gz in artifact `guest-alpine` does not match the "
                "SHA-256 that the record gives",
                "warning",
            ),
            (
                "missing file",
                "guest-kernel",
                lambda files: _reuse_zip(
                    {
                        name: contents
                        for name, contents in members(files, "guest-kernel").items()
                        if name != "vmlinux.config"
                    }
                ),
                "holds vmlinux.config",
                "warning",
            ),
            (
                "file in two artifacts",
                "guest-alpine",
                lambda files: _reuse_zip(
                    {
                        **members(files, "guest-alpine"),
                        "vmlinux": (files / "vmlinux").read_bytes(),
                    }
                ),
                "the artifacts hold vmlinux more than once",
                "warning",
            ),
            (
                "record artifact without a record",
                record_name,
                lambda _files: _reuse_zip({"other.json": b"{}"}),
                "artifact `kernel-alpine-record` holds no single guest-artifacts.json",
                "warning",
            ),
            (
                "record without a file",
                record_name,
                lambda files: record_files(
                    files, lambda digests: digests.pop("vmlinux.config")
                ),
                "describes other files than those of kernel and alpine",
                "warning",
            ),
            (
                "record with a file of another component",
                record_name,
                lambda files: record_files(
                    files,
                    lambda digests: digests.update({"ubuntu-distro.erofs": "0" * 64}),
                ),
                "describes other files than those of kernel and alpine",
                "warning",
            ),
            (
                "record without a digest",
                record_name,
                lambda files: record_files(
                    files, lambda digests: digests.update({"vmlinux": "MD5"})
                ),
                "record is invalid: it gives no SHA-256 of vmlinux",
                "warning",
            ),
            (
                "unknown record format",
                record_name,
                lambda files: record(files, {"format": 2}),
                "record is invalid: its format is not 1",
                "warning",
            ),
            (
                "boolean record format",
                record_name,
                lambda files: record(files, {"format": True}),
                "record is invalid: its format is not 1",
                "warning",
            ),
            (
                "malformed record",
                record_name,
                lambda _files: _reuse_zip({"guest-artifacts.json": b"{"}),
                "record is invalid",
                "warning",
            ),
            (
                "record not an archive",
                record_name,
                lambda _files: b"not a ZIP archive",
                "artifact `kernel-alpine-record` is not a valid ZIP archive",
                "warning",
            ),
            (
                "files not an archive",
                "guest-kernel",
                lambda _files: b"not a ZIP archive",
                "artifact `guest-kernel` is not a valid ZIP archive",
                "warning",
            ),
        )
        for case, name, contents, reason, level in cases:
            with (
                self.subTest(case=case),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                api, kernel_inputs, files = self._fixture(root)
                self._serve(api, name, contents(files))
                destination = root / "destination"

                outcome = self._reuse(api, kernel_inputs, destination)

                self._assert_built(outcome, destination, re.escape(reason), level)

        for name in (record_name, "guest-kernel", "guest-alpine"):
            with (
                self.subTest(digest_of=name),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                api, kernel_inputs, _files = self._fixture(root)
                contents = api.downloads[_reuse_download(name)]
                self._serve(api, name, contents, digest="sha256:" + "0" * 64)
                destination = root / "destination"

                outcome = self._reuse(api, kernel_inputs, destination)

                self._assert_built(
                    outcome,
                    destination,
                    re.escape(
                        f"artifact {_REUSE_ARTIFACT_IDS[name]} does not match its "
                        "digest sha256:" + "0" * 64
                    ),
                    "warning",
                )

        # Without the digest that GitHub took at the upload, nothing but the
        # archive itself would vouch for it, so reuse doesn't download it.
        for digest in (None, "md5:" + "0" * 32, "sha256:00", "SHA256:" + "0" * 64):
            for name in (record_name, "guest-alpine"):
                with (
                    self.subTest(digest=digest, artifact=name),
                    tempfile.TemporaryDirectory() as temporary,
                ):
                    root = Path(temporary)
                    api, kernel_inputs, _files = self._fixture(root)
                    api.documents[_reuse_list(name)] = {
                        "artifacts": [
                            _reuse_artifact(
                                _REUSE_ARTIFACT_IDS[name], 102, name=name, digest=digest
                            )
                        ]
                    }
                    destination = root / "destination"

                    outcome = self._reuse(api, kernel_inputs, destination)

                    self._assert_built(
                        outcome,
                        destination,
                        re.escape(
                            f"artifact {_REUSE_ARTIFACT_IDS[name]} has no SHA-256 "
                            f"digest, but {digest!r}"
                        ),
                        "warning",
                    )
                    self.assertNotIn(_reuse_download(name), api.requests)

    def test_reuse_requires_provenance_that_matches_this_checkout(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            components = ("ubuntu", "azurelinux")
            api, kernel_inputs, files = self._fixture(root, components)
            # A distro layer built from other inputs, recorded as such.
            manifest = files / UbuntuBuildConstants.DISTRO_MANIFEST_NAME
            document = json.loads(manifest.read_text(encoding="utf-8"))
            document["input_sha256"] = "f" * 64
            manifest.write_text(json.dumps(document), encoding="utf-8")
            record = guest_artifact_reuse.record_guest_artifacts(
                files, components, _REUSE_KEYS
            )
            self._serve(
                api,
                "ubuntu-azurelinux-record",
                _reuse_zip({record.name: record.read_bytes()}),
            )
            self._serve(
                api,
                "guest-ubuntu-distro",
                _reuse_zip(
                    {
                        name: (files / name).read_bytes()
                        for name in sorted(_guest_uploads()["guest-ubuntu-distro"][1])
                    }
                ),
            )
            destination = root / "destination"

            outcome = self._reuse(
                api, kernel_inputs, destination, _reuse_request(components)
            )

            self._assert_built(
                outcome,
                destination,
                "do not match their provenance or this checkout: Ubuntu artifact "
                "manifest does not match ubuntu-distro.erofs",
                "warning",
            )

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            api, _kernel_inputs, _files = self._fixture(root)
            destination = root / "destination"

            # The fixture's kernel provenance names inputs that this checkout
            # lacks.
            outcome = guest_artifact_reuse.reuse_guest_artifacts(
                api, _reuse_request(), destination
            )

            self._assert_built(
                outcome,
                destination,
                "kernel build provenance does not match",
                "warning",
            )

    def test_reuse_installs_every_file_or_leaves_the_destination_as_it_was(self):
        # The installation moves a restored file aside (calls 1 and 3) before
        # it installs its replacement (2 and 4). Each rename fails before it
        # happens, or right after it, before the installation can note it.
        replace = os.replace
        failure = OSError("disk full")
        interruption = guest_artifact_reuse.ReuseRefused("interrupted")
        for failing_call, error, after_rename in (
            *((number, failure, False) for number in (1, 2, 3, 4)),
            *((number, interruption, False) for number in (1, 2, 3, 4)),
            *((number, interruption, True) for number in (1, 2, 3, 4)),
        ):
            with (
                self.subTest(
                    failing_call=failing_call,
                    error=str(error),
                    after_rename=after_rename,
                ),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                api, kernel_inputs, _files = self._fixture(root)
                destination = root / "destination"
                destination.mkdir()
                restored = {
                    KernelBuildConstants.BINARY_NAME: b"cached kernel",
                    KernelBuildConstants.CONFIG_NAME: b"cached kernel config",
                }
                for name, contents in restored.items():
                    (destination / name).write_bytes(contents)
                calls: list[str] = []

                def fail(
                    source: Path,
                    target: Path,
                    calls: list[str] = calls,
                    failing_call: int = failing_call,
                    error: BaseException = error,
                    after_rename: bool = after_rename,
                ) -> None:
                    calls.append(Path(target).name)
                    if len(calls) != failing_call:
                        replace(source, target)
                        return
                    if after_rename:
                        replace(source, target)
                    raise error

                with patch.object(guest_artifact_reuse.os, "replace", fail):
                    outcome = self._reuse(api, kernel_inputs, destination)

                self.assertFalse(outcome.reused)
                self.assertEqual(outcome.status, "build")
                self.assertIn(str(error).lower(), outcome.detail.lower())
                self.assertEqual(
                    {
                        path.name: path.read_bytes()
                        for path in destination.iterdir()
                        if path.is_file()
                    },
                    restored,
                )

    def test_time_limit_refuses_an_expired_deadline_at_once(self):
        with guest_artifact_reuse._time_limit(None):
            pass
        with (
            self.assertRaisesRegex(
                guest_artifact_reuse.ReuseRefused, "reuse ran out of time"
            ),
            guest_artifact_reuse._time_limit(time.monotonic() - 1),
        ):
            self.fail("an expired time limit ran its block")

    def test_time_limit_interrupts_work_that_outlasts_it(self):
        if sys.platform == "win32":
            self.skipTest("Windows has no interval timer")
        previous = signal.getsignal(signal.SIGALRM)
        start = time.monotonic()
        with (
            self.assertRaisesRegex(
                guest_artifact_reuse.ReuseRefused, "reuse ran out of time"
            ),
            guest_artifact_reuse._time_limit(time.monotonic() + 0.2),
        ):
            time.sleep(10)
        self.assertLess(time.monotonic() - start, 5)
        self.assertEqual(signal.getitimer(signal.ITIMER_REAL), (0.0, 0.0))
        self.assertEqual(signal.getsignal(signal.SIGALRM), previous)

    def test_time_limit_spares_the_installation(self):
        if sys.platform == "win32":
            self.skipTest("Windows has no interval timer")
        install = guest_artifact_reuse._install
        timers: dict[str, tuple[float, float]] = {}

        class TimedGitHub(_FakeGitHub):
            def get(self, endpoint: str) -> object:
                if sys.platform != "win32":
                    timers.setdefault("request", signal.getitimer(signal.ITIMER_REAL))
                return super().get(endpoint)

        def timed_install(
            staging: Path, destination: Path, names: tuple[str, ...]
        ) -> None:
            if sys.platform != "win32":
                timers["installation"] = signal.getitimer(signal.ITIMER_REAL)
            install(staging, destination, names)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture, kernel_inputs, _files = self._fixture(root)
            api = TimedGitHub(fixture.documents, fixture.downloads)
            with (
                patch.object(
                    release, "kernel_provenance_inputs", return_value=kernel_inputs
                ),
                patch.object(guest_artifact_reuse, "_install", timed_install),
            ):
                outcome = guest_artifact_reuse.reuse_guest_artifacts(
                    api,
                    _reuse_request(),
                    root / "destination",
                    deadline=time.monotonic() + 300,
                )

        self.assertTrue(outcome.reused, outcome.detail)
        # The timer runs while reuse fetches and checks the artifact, but not
        # while it installs the images, so it can't cut a rollback short.
        self.assertGreater(timers["request"][0], 0)
        self.assertEqual(timers["installation"], (0.0, 0.0))
        self.assertEqual(signal.getitimer(signal.ITIMER_REAL), (0.0, 0.0))

    def test_time_limit_ignores_its_timer_once_its_block_ends(self):
        if sys.platform == "win32":
            self.skipTest("Windows has no interval timer")
        previous = signal.getsignal(signal.SIGALRM)
        with guest_artifact_reuse._time_limit(time.monotonic() + 300):
            expire = signal.getsignal(signal.SIGALRM)
        self.assertEqual(signal.getsignal(signal.SIGALRM), previous)
        # A signal that arrived just before the timer stopped runs the handler
        # after the block, where it must not raise in place of the restoration.
        assert callable(expire)
        expire(signal.SIGALRM, None)

    def test_reuse_cleans_up_when_its_time_limit_expires_as_the_checks_end(self):
        @contextlib.contextmanager
        def expiring(_deadline: float | None) -> Generator[None]:
            yield
            raise guest_artifact_reuse.ReuseRefused("reuse ran out of time")

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            api, kernel_inputs, _files = self._fixture(root)
            destination = root / "destination"
            with patch.object(guest_artifact_reuse, "_time_limit", expiring):
                outcome = self._reuse(api, kernel_inputs, destination)

            self._assert_built(
                outcome, destination, "^Reuse ran out of time, so ", "warning"
            )

    def test_reuse_command_falls_back_when_reuse_outlasts_its_time_limit(self):
        if sys.platform == "win32":
            self.skipTest("Windows has no interval timer")

        class SlowGitHub(_FakeGitHub):
            def get(self, endpoint: str) -> object:
                time.sleep(10)
                return super().get(endpoint)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "output"
            stdout = io.StringIO()
            start = time.monotonic()
            with (
                patch.object(guest_artifact_reuse, "REUSE_TIMEOUT_SECONDS", 0.2),
                patch.object(
                    guest_artifact_reuse, "RestApi", return_value=SlowGitHub({})
                ),
                contextlib.redirect_stdout(stdout),
            ):
                outcome = guest_artifact_reuse.reuse_in_ci(
                    repository=_REUSE_REPOSITORY,
                    workflow_ref=_REUSE_WORKFLOW_REF,
                    record="kernel-alpine-record",
                    components="kernel alpine",
                    keys=json.dumps(_REUSE_KEYS),
                    commit=_REUSE_MERGE,
                    head_sha=None,
                    dry_run=False,
                    destination=root / "destination",
                    environment={"GH_TOKEN": "token", "GITHUB_OUTPUT": str(output)},
                )

            self.assertLess(time.monotonic() - start, 5)
            self.assertFalse(outcome.reused)
            self.assertEqual(outcome.level, "warning")
            self.assertTrue(
                outcome.detail.startswith("Reuse ran out of time, so "),
                outcome.detail,
            )
            self.assertEqual(output.read_text(encoding="utf-8"), "reused=false\n")

    def test_api_failures_fall_back_to_building(self):
        for endpoint in (
            f"repos/{_REUSE_REPOSITORY}/git/commits/{_REUSE_MERGE}",
            _REUSE_RUNS,
            _reuse_list("kernel-alpine-record"),
            _reuse_download("kernel-alpine-record"),
            _reuse_list("guest-kernel"),
            _reuse_download("guest-alpine"),
        ):
            with (
                self.subTest(endpoint=endpoint),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                api, kernel_inputs, _files = self._fixture(root)
                api.documents.pop(endpoint, None)
                api.downloads.pop(endpoint, None)
                destination = root / "destination"

                outcome = self._reuse(api, kernel_inputs, destination)

                self._assert_built(
                    outcome,
                    destination,
                    re.escape(f"GET {endpoint} failed with HTTP 404"),
                    "warning",
                )

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            api, kernel_inputs, _files = self._fixture(root)
            api.documents[_REUSE_RUNS] = {"workflow_runs": "invalid"}
            destination = root / "destination"

            outcome = self._reuse(api, kernel_inputs, destination)

            self._assert_built(
                outcome,
                destination,
                "GitHub returned an invalid workflow run list",
                "warning",
            )

    def test_workflow_path_comes_from_the_workflow_ref(self):
        for workflow_ref in (
            f"{_REUSE_REPOSITORY}/{_REUSE_WORKFLOW}@refs/heads/dev",
            f"{_REUSE_REPOSITORY}/{_REUSE_WORKFLOW}@refs/pull/702/merge",
        ):
            with self.subTest(workflow_ref=workflow_ref):
                self.assertEqual(
                    guest_artifact_reuse.workflow_path(workflow_ref, _REUSE_REPOSITORY),
                    _REUSE_WORKFLOW,
                )
        for workflow_ref in (
            f"{_REUSE_REPOSITORY}/{_REUSE_WORKFLOW}",
            f"fork/nvx/{_REUSE_WORKFLOW}@refs/heads/dev",
            f"{_REUSE_REPOSITORY}/ci.yml@refs/heads/dev",
            f"{_REUSE_REPOSITORY}/.github/workflows/nested/ci.yml@refs/heads/dev",
        ):
            with self.subTest(workflow_ref=workflow_ref):
                with self.assertRaises(guest_artifact_reuse.ReuseRefused):
                    guest_artifact_reuse.workflow_path(workflow_ref, _REUSE_REPOSITORY)
                # The parser that the reuse shares refuses it as a plain error.
                with self.assertRaises(common.ScriptError) as refused:
                    common.workflow_path(workflow_ref, _REUSE_REPOSITORY)
                self.assertNotIsInstance(
                    refused.exception, guest_artifact_reuse.ReuseRefused
                )
        # The cancellation of superseded merge groups shares it too.
        self.assertIs(merge_queue.workflow_path, common.workflow_path)

    def test_reuse_command_reports_its_outcome_and_never_fails(self):
        base = [
            "reuse-guest-artifacts",
            "--repository",
            _REUSE_REPOSITORY,
            "--workflow-ref",
            _REUSE_WORKFLOW_REF,
            "--record-artifact",
            "kernel-alpine-record",
            "--commit",
            _REUSE_MERGE,
        ]
        keys = json.dumps(_REUSE_KEYS)
        heading = "Guest artifact reuse (kernel, alpine)"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            api, kernel_inputs, _files = self._fixture(root)
            output = root / "output"
            summary = root / "summary"
            destination = root / "destination"
            for case, components, keys_text, token, reused, level, title in (
                ("reused", "alpine kernel", keys, "token", "true", None, heading),
                ("restored", "kernel alpine", keys, "token", "false", None, heading),
                ("no token", "kernel alpine", keys, "", "false", "warning", heading),
                (
                    "invalid keys",
                    "kernel alpine",
                    "{",
                    "token",
                    "false",
                    "warning",
                    heading,
                ),
                (
                    "missing key",
                    "kernel alpine",
                    json.dumps({"kernel": "key"}),
                    "token",
                    "false",
                    "warning",
                    heading,
                ),
                (
                    "unknown component",
                    "kernel debug-kernel",
                    keys,
                    "token",
                    "false",
                    "warning",
                    "Guest artifact reuse",
                ),
            ):
                with self.subTest(case=case):
                    output.write_text("", encoding="utf-8")
                    summary.write_text("", encoding="utf-8")
                    args = nvx.parse_args(
                        [
                            *base,
                            "--components",
                            components,
                            "--keys",
                            keys_text,
                            "--destination",
                            os.fspath(destination),
                        ]
                    )
                    stdout = io.StringIO()
                    with (
                        patch.dict(
                            os.environ,
                            {
                                "GH_TOKEN": token,
                                "GITHUB_TOKEN": "",
                                "GITHUB_OUTPUT": os.fspath(output),
                                "GITHUB_STEP_SUMMARY": os.fspath(summary),
                            },
                        ),
                        patch.object(
                            guest_artifact_reuse, "RestApi", return_value=api
                        ) as rest_api,
                        patch.object(
                            release,
                            "kernel_provenance_inputs",
                            return_value=kernel_inputs,
                        ),
                        contextlib.redirect_stdout(stdout),
                    ):
                        self.assertIsNone(args.handler(args))
                    self.assertEqual(
                        output.read_text(encoding="utf-8"), f"reused={reused}\n"
                    )
                    self.assertTrue(
                        summary.read_text(encoding="utf-8").startswith(f"### {title}: ")
                    )
                    annotations = [
                        line
                        for line in stdout.getvalue().splitlines()
                        if line.startswith("::")
                    ]
                    if level is None:
                        self.assertEqual(annotations, [])
                    else:
                        self.assertEqual(len(annotations), 1)
                        self.assertTrue(
                            annotations[0].startswith(
                                f"::{level} title={title.replace(',', '%2C')}::"
                            ),
                            annotations[0],
                        )
                        rest_api.assert_not_called()
        with (
            patch("sys.stderr", new_callable=io.StringIO),
            self.assertRaises(SystemExit) as exit_context,
        ):
            nvx.parse_args(base[:-2] + ["--components", "kernel", "--keys", "{}"])
        self.assertEqual(exit_context.exception.code, 2)
        # A rehearsal names the head instead of the merge commit.
        with (
            patch("sys.stderr", new_callable=io.StringIO),
            self.assertRaises(SystemExit) as exit_context,
        ):
            nvx.parse_args(
                [
                    *base,
                    "--components",
                    "kernel",
                    "--keys",
                    "{}",
                    "--head-sha",
                    _REUSE_HEAD,
                    "--dry-run",
                ]
            )
        self.assertEqual(exit_context.exception.code, 2)
        args = nvx.parse_args(
            [
                *base[:-2],
                "--components",
                "kernel",
                "--keys",
                "{}",
                "--head-sha",
                _REUSE_HEAD,
                "--dry-run",
            ]
        )
        self.assertEqual(
            (args.commit, args.head_sha, args.dry_run), (None, _REUSE_HEAD, True)
        )

    def test_reports_escape_annotations_and_append_outputs(self):
        outcome = guest_artifact_reuse.ReuseOutcome(
            False,
            "Guest artifact reuse",
            "build",
            "100% of 2\nlines",
            "warning",
        )
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "output"
            output.write_text("before=1\n", encoding="utf-8")
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                guest_artifact_reuse.report(
                    outcome, {"GITHUB_OUTPUT": os.fspath(output)}
                )
            self.assertEqual(
                output.read_text(encoding="utf-8"), "before=1\nreused=false\n"
            )
        self.assertIn(
            "::warning title=Guest artifact reuse::100%25 of 2%0Alines\n",
            stdout.getvalue(),
        )
        self.assertEqual(
            outcome.summary(),
            "### Guest artifact reuse: build\n100% of 2\nlines\n",
        )

    def test_rest_api_reads_json_and_downloads_without_forwarding_the_token(self):
        authorizations: dict[str, str | None] = {}

        class StorageHandler(http.server.BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                authorizations["storage"] = self.headers.get("Authorization")
                body = b"archive"
                self.send_response(http.HTTPStatus.OK)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, format: str, *args: object) -> None:
                pass

        storage = http.server.ThreadingHTTPServer(("127.0.0.1", 0), StorageHandler)

        class ApiHandler(http.server.BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                authorizations[self.path] = self.headers.get("Authorization")
                if self.path == "/repos/o/r/runs":
                    body = b'{"workflow_runs": []}'
                    self.send_response(http.HTTPStatus.OK)
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                elif self.path == "/repos/o/r/zip":
                    self.send_response(http.HTTPStatus.FOUND)
                    self.send_header(
                        "Location",
                        f"http://127.0.0.1:{storage.server_address[1]}/blob",
                    )
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                else:
                    self.send_error(http.HTTPStatus.NOT_FOUND)

            def log_message(self, format: str, *args: object) -> None:
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), ApiHandler)
        threads = [
            threading.Thread(target=instance.serve_forever, daemon=True)
            for instance in (server, storage)
        ]
        for thread in threads:
            thread.start()
        try:
            url = f"http://127.0.0.1:{server.server_address[1]}"
            api = guest_artifact_reuse.RestApi("secret", time.monotonic() + 60, url=url)
            self.assertEqual(api.get("repos/o/r/runs"), {"workflow_runs": []})
            with tempfile.TemporaryDirectory() as temporary:
                archive = Path(temporary) / "artifact.zip"
                api.download("repos/o/r/zip", archive)
                self.assertEqual(archive.read_bytes(), b"archive")
            self.assertEqual(authorizations["/repos/o/r/runs"], "Bearer secret")
            self.assertEqual(authorizations["/repos/o/r/zip"], "Bearer secret")
            self.assertIsNone(authorizations["storage"])
            with self.assertRaisesRegex(
                guest_artifact_reuse.ReuseRefused,
                "GET repos/o/r/missing failed with HTTP 404",
            ):
                api.get("repos/o/r/missing")
            expired = guest_artifact_reuse.RestApi(
                "secret", time.monotonic() - 1, url=url
            )
            with self.assertRaisesRegex(
                guest_artifact_reuse.ReuseRefused, "ran out of time"
            ):
                expired.get("repos/o/r/runs")
        finally:
            for instance in (server, storage):
                instance.shutdown()
                instance.server_close()
            for thread in threads:
                thread.join()

    def test_rest_api_refuses_responses_cut_short(self):
        # A response that ends before its Content-Length, or within a chunk,
        # raises http.client.IncompleteRead, which is no OSError, so it would
        # fail the step instead of falling back to building.
        class TruncatingHandler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_GET(self) -> None:
                self.send_response(http.HTTPStatus.OK)
                if self.path == "/repos/o/r/runs":
                    self.send_header("Content-Length", "100")
                    self.end_headers()
                    self.wfile.write(b'{"workflow_runs": ')
                else:
                    self.send_header("Transfer-Encoding", "chunked")
                    self.end_headers()
                    self.wfile.write(b"64\r\n" + b"x" * 10)
                self.wfile.flush()
                self.close_connection = True

            def log_message(self, format: str, *args: object) -> None:
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), TruncatingHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            api = guest_artifact_reuse.RestApi(
                "secret",
                time.monotonic() + 60,
                url=f"http://127.0.0.1:{server.server_address[1]}",
            )
            with self.assertRaisesRegex(
                guest_artifact_reuse.ReuseRefused,
                "^GET repos/o/r/runs failed: IncompleteRead",
            ):
                api.get("repos/o/r/runs")
            with (
                tempfile.TemporaryDirectory() as temporary,
                self.assertRaisesRegex(
                    guest_artifact_reuse.ReuseRefused,
                    "^GET repos/o/r/zip failed: IncompleteRead",
                ),
            ):
                api.download("repos/o/r/zip", Path(temporary) / "artifact.zip")
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    unittest.main()
