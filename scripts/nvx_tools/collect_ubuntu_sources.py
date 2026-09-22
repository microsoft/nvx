"""Collect exact Ubuntu source packages for generated guest artifacts."""

from __future__ import annotations

import argparse
import json
import lzma
import re
import shutil
import urllib.parse
from collections.abc import Sequence
from datetime import datetime, timedelta, timezone
from pathlib import Path, PurePosixPath
from tempfile import TemporaryDirectory
from typing import TypedDict, cast

from .build_constants import (
    BuildConstants,
    UbuntuBuildConstants,
)
from .common import (
    ScriptError,
    download,
    download_verified,
    require_tool,
    run_checked,
    sha256_file,
    write_sha256_sums,
)
from .ubuntu import parse_deb822

_SHA256 = re.compile(r"[0-9a-f]{64}")


class SourceRequirement(TypedDict):
    source_name: str
    source_version: str


class SourceChecksum(TypedDict):
    name: str
    size: int
    sha256: str


class SourceFile(SourceChecksum):
    url: str


class SourceRecord(TypedDict):
    source_name: str
    source_version: str
    directory: str
    index_url: str
    index_sha256: str
    index_release_url: str
    index_release_sha256: str
    files: list[SourceFile]


class SourceMetadata(TypedDict):
    role: str
    url: str
    sha256: str
    cache_path: Path | None
    output_name: str | None
    authenticated_by: str | None


def _source_requirements(manifests: list[Path]) -> tuple[SourceRequirement, ...]:
    requirements: dict[tuple[str, str], SourceRequirement] = {}
    for path in manifests:
        raw_document = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(raw_document, dict):
            raise ScriptError(f"{path} must contain a JSON object")
        document = cast(dict[str, object], raw_document)
        if (
            document.get("guest") != "ubuntu"
            or document.get("release") != UbuntuBuildConstants.VERSION
            or document.get("architecture") != UbuntuBuildConstants.ARCHITECTURE
        ):
            raise ScriptError(f"{path} is not a pinned Ubuntu package manifest")
        raw_packages = document.get("packages")
        if not isinstance(raw_packages, list):
            raise ScriptError(f"{path} has no packages array")
        for raw_package in cast(list[object], raw_packages):
            if not isinstance(raw_package, dict):
                raise ScriptError(f"{path} contains a non-object package record")
            package = cast(dict[str, object], raw_package)
            source_name = package.get("source_name")
            source_version = package.get("source_version")
            if (
                not isinstance(source_name, str)
                or not source_name
                or not isinstance(source_version, str)
                or not source_version
            ):
                raise ScriptError(
                    f"{path} package record has no source name and version"
                )
            requirements[(source_name, source_version)] = {
                "source_name": source_name,
                "source_version": source_version,
            }
    if not requirements:
        raise ScriptError("Ubuntu package manifests contain no source requirements")
    return tuple(requirements[key] for key in sorted(requirements))


def _checksum_records(value: str, label: str) -> list[SourceChecksum]:
    records: list[SourceChecksum] = []
    names: set[str] = set()
    for line in value.splitlines():
        if not line.strip():
            continue
        fields = line.split()
        if len(fields) != 3:
            raise ScriptError(f"{label} has malformed Checksums-Sha256 metadata")
        digest, raw_size, name = fields
        if (
            _SHA256.fullmatch(digest) is None
            or "/" in name
            or "\\" in name
            or name in names
        ):
            raise ScriptError(f"{label} has invalid source member {name!r}")
        try:
            size = int(raw_size, 10)
        except ValueError as error:
            raise ScriptError(f"{label} has invalid size for {name}") from error
        if size < 0:
            raise ScriptError(f"{label} has negative size for {name}")
        names.add(name)
        records.append({"name": name, "size": size, "sha256": digest})
    if not records:
        raise ScriptError(f"{label} has no SHA-256 source members")
    return records


def _index_cache_name(url: str) -> str:
    parsed = urllib.parse.urlsplit(url)
    stem = "-".join(part for part in parsed.path.split("/") if part)[-120:]
    return f"{parsed.netloc}-{stem}"


def _release_checksum(
    document: dict[str, str],
    relative: str,
    label: str,
) -> SourceChecksum:
    value = document.get("SHA256")
    if value is None:
        raise ScriptError(f"{label} has no SHA256 metadata")
    matches: list[SourceChecksum] = []
    for line in value.splitlines():
        if not line.strip():
            continue
        fields = line.split()
        if len(fields) != 3:
            raise ScriptError(f"{label} has malformed SHA256 metadata")
        digest, raw_size, name = fields
        path = PurePosixPath(name)
        if (
            _SHA256.fullmatch(digest) is None
            or path.is_absolute()
            or "\\" in name
            or "." in path.parts
            or ".." in path.parts
            or path.as_posix() != name
        ):
            raise ScriptError(f"{label} has invalid release member {name!r}")
        try:
            size = int(raw_size, 10)
        except ValueError as error:
            raise ScriptError(f"{label} has invalid size for {name}") from error
        if size < 0:
            raise ScriptError(f"{label} has negative size for {name}")
        if name == relative:
            matches.append({"name": name, "size": size, "sha256": digest})
    if len(matches) != 1:
        raise ScriptError(
            f"{label} contains {len(matches)} SHA256 records for {relative}"
        )
    return matches[0]


def _verify_inrelease(
    path: Path,
    keyring: Path,
    suite: str,
) -> dict[str, str]:
    gpgv = require_tool(
        "gpgv",
        "gpgv was not found on PATH; install it to authenticate Ubuntu sources",
    )
    with TemporaryDirectory(prefix="nvx-gpgv-") as homedir:
        run_checked([gpgv, "--homedir", homedir, "--keyring", keyring, path])
    payload = _clearsigned_payload(path.read_text(encoding="utf-8"), path.name)
    documents = parse_deb822(payload)
    if len(documents) != 1:
        raise ScriptError(f"{path} has invalid Ubuntu release metadata")
    document = documents[0]
    if (
        document.get("Origin") != "Ubuntu"
        or document.get("Codename") != UbuntuBuildConstants.CODENAME
        or document.get("Suite") != suite
    ):
        raise ScriptError(f"{path} identifies the wrong Ubuntu suite")
    return document


def _source_record_payload(record: SourceRecord) -> dict[str, object]:
    return {
        "directory": record["directory"],
        "files": [
            {
                "name": source_file["name"],
                "size": source_file["size"],
                "sha256": source_file["sha256"],
            }
            for source_file in record["files"]
        ],
    }


def _merge_source_records(
    destination: dict[tuple[str, str], SourceRecord],
    incoming: dict[tuple[str, str], SourceRecord],
) -> None:
    for key, record in incoming.items():
        previous = destination.get(key)
        if previous is not None:
            if _source_record_payload(previous) != _source_record_payload(record):
                raise ScriptError(
                    "Ubuntu source indexes disagree about "
                    f"{record['source_name']}-{record['source_version']}"
                )
            continue
        destination[key] = record


def _load_authenticated_source_index(
    cache: Path,
    keyring: Path,
    archive_url: str,
    suite: str,
    component: str,
) -> tuple[dict[tuple[str, str], SourceRecord], list[SourceMetadata]]:
    if component not in UbuntuBuildConstants.COMPONENTS:
        raise ScriptError(f"unsupported Ubuntu archive component: {component}")
    archive_url = archive_url.rstrip("/")
    release_url = f"{archive_url}/dists/{suite}/InRelease"
    release_path = cache / _index_cache_name(release_url)
    print(f">> downloading Ubuntu signed release {release_url}")
    download(release_url, release_path)
    release_document = _verify_inrelease(release_path, keyring, suite)
    release_sha256 = sha256_file(release_path)

    relative = f"{component}/source/Sources.xz"
    expected = _release_checksum(release_document, relative, release_url)
    index_url = f"{archive_url}/dists/{suite}/{relative}"
    index_path = cache / _index_cache_name(index_url)
    print(f">> downloading Ubuntu source index {index_url}")
    download_verified(index_url, index_path, expected["sha256"])
    actual_size = index_path.stat().st_size
    if actual_size != expected["size"]:
        raise ScriptError(
            f"{index_path.name} is {actual_size} bytes, expected {expected['size']}"
        )
    try:
        contents = lzma.decompress(index_path.read_bytes()).decode("utf-8")
    except (lzma.LZMAError, UnicodeDecodeError) as error:
        raise ScriptError(
            f"invalid Ubuntu source index {index_url}: {error}"
        ) from error

    records: dict[tuple[str, str], SourceRecord] = {}
    for package in parse_deb822(contents):
        name = package.get("Package")
        version = package.get("Version")
        directory = package.get("Directory")
        checksums = package.get("Checksums-Sha256")
        if not name or not version or not directory or not checksums:
            continue
        directory_path = PurePosixPath(directory)
        if (
            directory_path.is_absolute()
            or "." in directory_path.parts
            or ".." in directory_path.parts
            or directory_path.as_posix() != directory
        ):
            raise ScriptError(
                f"Ubuntu source index has invalid directory {directory!r}"
            )
        source_files = [
            cast(
                SourceFile,
                {
                    **source_file,
                    "url": f"{archive_url}/{directory}/{source_file['name']}",
                },
            )
            for source_file in _checksum_records(
                checksums,
                f"{name}-{version} in {index_url}",
            )
        ]
        record: SourceRecord = {
            "source_name": name,
            "source_version": version,
            "directory": directory,
            "index_url": index_url,
            "index_sha256": expected["sha256"],
            "index_release_url": release_url,
            "index_release_sha256": release_sha256,
            "files": source_files,
        }
        _merge_source_records(records, {(name, version): record})

    return (
        records,
        [
            {
                "role": "ubuntu-inrelease",
                "url": release_url,
                "sha256": release_sha256,
                "cache_path": release_path,
                "output_name": _index_cache_name(release_url),
                "authenticated_by": UbuntuBuildConstants.ARCHIVE_KEYRING_URL,
            },
            {
                "role": "ubuntu-source-index",
                "url": index_url,
                "sha256": expected["sha256"],
                "cache_path": None,
                "output_name": None,
                "authenticated_by": release_url,
            },
        ],
    )


def _load_source_records(
    cache: Path,
    requirements: Sequence[SourceRequirement] = (),
) -> tuple[dict[tuple[str, str], SourceRecord], list[SourceMetadata]]:
    cache.mkdir(parents=True, exist_ok=True)
    keyring = cache / "ubuntu-archive-keyring.gpg"
    download_verified(
        UbuntuBuildConstants.ARCHIVE_KEYRING_URL,
        keyring,
        UbuntuBuildConstants.ARCHIVE_KEYRING_SHA256,
    )
    records: dict[tuple[str, str], SourceRecord] = {}
    indexes: list[SourceMetadata] = [
        {
            "role": "ubuntu-archive-keyring",
            "url": UbuntuBuildConstants.ARCHIVE_KEYRING_URL,
            "sha256": UbuntuBuildConstants.ARCHIVE_KEYRING_SHA256,
            "cache_path": keyring,
            "output_name": keyring.name,
            "authenticated_by": None,
        }
    ]
    for archive_url, suite, component in UbuntuBuildConstants.SOURCE_INDEXES:
        loaded, metadata = _load_authenticated_source_index(
            cache,
            keyring,
            archive_url,
            suite,
            component,
        )
        _merge_source_records(records, loaded)
        indexes.extend(metadata)
    for requirement in requirements:
        key = (
            requirement["source_name"],
            requirement["source_version"],
        )
        if key in records:
            continue
        record, metadata = _launchpad_source_record(cache, keyring, *key)
        records[key] = record
        indexes.extend(metadata)
    return records, indexes


def _clearsigned_payload(text: str, label: str) -> str:
    if not text.startswith("-----BEGIN PGP SIGNED MESSAGE-----\n"):
        return text
    separator = "\n\n"
    header_end = text.find(separator)
    signature = text.find("\n-----BEGIN PGP SIGNATURE-----")
    if header_end < 0 or signature < 0 or signature <= header_end:
        raise ScriptError(f"{label} has malformed clear-signed metadata")
    payload = text[header_end + len(separator) : signature]
    return "\n".join(
        line[2:] if line.startswith("- ") else line for line in payload.splitlines()
    )


def _dsc_document(path: Path) -> dict[str, str]:
    # The signed Release -> Sources.xz -> .dsc digest chain authenticates this file.
    payload = _clearsigned_payload(
        path.read_text(encoding="utf-8"),
        path.name,
    )
    documents = parse_deb822(payload)
    if len(documents) != 1:
        raise ScriptError(f"{path} has invalid Debian source metadata")
    return documents[0]


def _validate_dsc(
    path: Path,
    record: SourceRecord,
) -> None:
    document = _dsc_document(path)
    if (
        document.get("Source") != record["source_name"]
        or document.get("Version") != record["source_version"]
    ):
        raise ScriptError(f"{path} identifies the wrong Ubuntu source package")
    checksums = document.get("Checksums-Sha256")
    if checksums is None:
        raise ScriptError(f"{path} has no Checksums-Sha256")
    dsc_members = {
        item["name"]: (item["size"], item["sha256"])
        for item in _checksum_records(checksums, path.name)
    }
    index_members = {
        item["name"]: (item["size"], item["sha256"]) for item in record["files"]
    }
    expected = {
        name: metadata
        for name, metadata in index_members.items()
        if not name.endswith(".dsc")
    }
    if dsc_members != expected:
        raise ScriptError(f"{path} source members do not match the Ubuntu source index")


def _download_json(url: str, path: Path) -> object:
    download(url, path)
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise ScriptError(f"invalid Launchpad response from {url}: {error}") from error


def _launchpad_timestamp(entry: dict[str, object], field: str) -> datetime | None:
    raw_value = entry.get(field)
    if raw_value is None:
        return None
    if not isinstance(raw_value, str):
        raise ScriptError(f"Launchpad returned an invalid {field}")
    try:
        value = datetime.fromisoformat(raw_value.replace("Z", "+00:00"))
    except ValueError as error:
        raise ScriptError(f"Launchpad returned an invalid {field}") from error
    if value.tzinfo is None:
        raise ScriptError(f"Launchpad returned a timezone-free {field}")
    return value.astimezone(timezone.utc)


def _snapshot_source_index(
    entry: dict[str, object],
) -> tuple[str, str, str]:
    pocket = entry.get("pocket")
    component = entry.get("component_name")
    if not isinstance(pocket, str) or pocket not in UbuntuBuildConstants.POCKET_SUITES:
        raise ScriptError(f"Launchpad returned an unsupported pocket: {pocket!r}")
    if (
        not isinstance(component, str)
        or component not in UbuntuBuildConstants.COMPONENTS
    ):
        raise ScriptError(f"Launchpad returned an unsupported component: {component!r}")
    published = _launchpad_timestamp(entry, "date_published")
    if published is None:
        raise ScriptError("Launchpad source publication has no publication date")
    end = next(
        (
            value
            for field in (
                "date_superseded",
                "date_removed",
                "scheduled_deletion_date",
            )
            if (value := _launchpad_timestamp(entry, field)) is not None
        ),
        None,
    )
    if end is not None:
        if end <= published:
            raise ScriptError("Launchpad source publication has an invalid lifetime")
        snapshot_time = published + (end - published) / 2
    else:
        snapshot_time = published + timedelta(days=1)
    snapshot_id = snapshot_time.strftime("%Y%m%dT%H%M%SZ")
    return (
        f"{UbuntuBuildConstants.SNAPSHOT_ARCHIVE_URL}/{snapshot_id}",
        UbuntuBuildConstants.POCKET_SUITES[pocket],
        component,
    )


def _launchpad_source_record(
    cache: Path,
    keyring: Path,
    name: str,
    version: str,
) -> tuple[SourceRecord, tuple[SourceMetadata, ...]]:
    key = _encoded_package_directory(name, version)
    query_url = (
        f"{UbuntuBuildConstants.LAUNCHPAD_ARCHIVE_API}?"
        + urllib.parse.urlencode(
            {
                "ws.op": "getPublishedSources",
                "source_name": name,
                "version": version,
                "exact_match": "true",
                "distro_series": UbuntuBuildConstants.LAUNCHPAD_SERIES_API,
            }
        )
    )
    query_path = cache / f"launchpad-{key}-query.json"
    raw_query = _download_json(query_url, query_path)
    query_sha256 = sha256_file(query_path)
    if not isinstance(raw_query, dict):
        raise ScriptError(f"Launchpad returned invalid publishing data for {name}")
    query = cast(dict[str, object], raw_query)
    raw_entries = query.get("entries")
    if not isinstance(raw_entries, list):
        raise ScriptError(f"Launchpad returned no publishing entries for {name}")
    entries: list[dict[str, object]] = []
    for raw_entry in cast(list[object], raw_entries):
        if not isinstance(raw_entry, dict):
            continue
        entry = cast(dict[str, object], raw_entry)
        if (
            entry.get("source_package_name") == name
            and entry.get("source_package_version") == version
            and entry.get("status") in ("Published", "Superseded", "Deleted")
        ):
            entries.append(entry)
    if not entries:
        raise ScriptError(
            f"Launchpad has no exact publishing history for {name}={version}"
        )
    entries.sort(
        key=lambda entry: (
            entry.get("status") != "Published",
            str(entry.get("date_published", "")),
        )
    )
    selected = entries[0]
    self_link = selected.get("self_link")
    if not isinstance(self_link, str) or not self_link.startswith(
        f"{UbuntuBuildConstants.LAUNCHPAD_ARCHIVE_API}/+sourcepub/"
    ):
        raise ScriptError(
            f"Launchpad returned an invalid source publication for {name}"
        )

    snapshot_archive, snapshot_suite, snapshot_component = _snapshot_source_index(
        selected
    )
    snapshot_records, snapshot_metadata = _load_authenticated_source_index(
        cache,
        keyring,
        snapshot_archive,
        snapshot_suite,
        snapshot_component,
    )
    record = snapshot_records.get((name, version))
    if record is None:
        raise ScriptError(f"signed Ubuntu snapshot does not contain {name}={version}")

    urls_url = f"{self_link}?ws.op=sourceFileUrls"
    urls_path = cache / f"launchpad-{key}-urls.json"
    raw_urls = _download_json(urls_url, urls_path)
    urls_sha256 = sha256_file(urls_path)
    if not isinstance(raw_urls, list) or not raw_urls:
        raise ScriptError(f"Launchpad returned no source files for {name}={version}")
    urls: dict[str, str] = {}
    for raw_url in cast(list[object], raw_urls):
        if not isinstance(raw_url, str):
            raise ScriptError(f"Launchpad returned an invalid source URL for {name}")
        parsed = urllib.parse.urlsplit(raw_url)
        filename = urllib.parse.unquote(PurePosixPath(parsed.path).name)
        if (
            parsed.scheme != "https"
            or parsed.netloc != "launchpad.net"
            or not filename
            or "/" in filename
            or filename in urls
        ):
            raise ScriptError(f"Launchpad returned unsafe source URL {raw_url!r}")
        urls[filename] = raw_url
    expected_names = {source_file["name"] for source_file in record["files"]}
    if set(urls) != expected_names:
        raise ScriptError(
            f"Launchpad source files disagree with the signed snapshot for "
            f"{name}={version}"
        )
    dsc_names = sorted(
        source_file["name"]
        for source_file in record["files"]
        if source_file["name"].endswith(".dsc")
    )
    if len(dsc_names) != 1:
        raise ScriptError(
            f"signed Ubuntu snapshot contains {len(dsc_names)} .dsc files for {name}"
        )

    return (
        record,
        (
            *snapshot_metadata,
            {
                "role": "launchpad-publishing-history",
                "url": query_url,
                "sha256": query_sha256,
                "cache_path": query_path,
                "output_name": f"launchpad-{key}-publishing.json",
                "authenticated_by": None,
            },
            {
                "role": "launchpad-source-file-urls",
                "url": urls_url,
                "sha256": urls_sha256,
                "cache_path": urls_path,
                "output_name": f"launchpad-{key}-source-urls.json",
                "authenticated_by": None,
            },
        ),
    )


def _encoded_package_directory(name: str, version: str) -> str:
    return f"{urllib.parse.quote(name, safe='')}_{urllib.parse.quote(version, safe='')}"


def _validate_source_output(output: Path) -> None:
    if output.is_symlink() or (output.exists() and not output.is_dir()):
        raise ScriptError(f"Ubuntu source output is not a directory: {output}")
    if not output.exists():
        return
    unexpected = sorted(
        path.name
        for path in output.iterdir()
        if path.name not in UbuntuBuildConstants.SOURCE_OUTPUT_ENTRIES
    )
    if unexpected:
        raise ScriptError(
            "Ubuntu source output contains unexpected entries: " + ", ".join(unexpected)
        )


def _reset_source_output(output: Path) -> None:
    for generated in (output / "packages", output / "metadata"):
        if generated.is_symlink() or (generated.exists() and not generated.is_dir()):
            raise ScriptError(
                f"Ubuntu source generated path is not a directory: {generated}"
            )
        if generated.exists():
            shutil.rmtree(generated)
    for generated in (output / "manifest.json", output / "SHA256SUMS"):
        if generated.is_symlink() or (generated.exists() and not generated.is_file()):
            raise ScriptError(
                f"Ubuntu source generated path is not a file: {generated}"
            )
        generated.unlink(missing_ok=True)


def _materialize_source_metadata(
    output: Path,
    indexes: Sequence[SourceMetadata],
) -> list[dict[str, str]]:
    manifest_indexes: list[dict[str, str]] = []
    metadata_root = output / "metadata"
    seen: dict[tuple[str, str], tuple[str, str | None, str | None]] = {}
    for index in indexes:
        identity = (index["role"], index["url"])
        value = (
            index["sha256"],
            index["authenticated_by"],
            index["output_name"],
        )
        previous = seen.get(identity)
        if previous is not None:
            if previous != value:
                raise ScriptError(
                    f"Ubuntu source metadata disagrees for {index['url']}"
                )
            continue
        seen[identity] = value
        manifest_index = {
            "role": index["role"],
            "url": index["url"],
            "sha256": index["sha256"],
        }
        if index["authenticated_by"] is not None:
            manifest_index["authenticated_by"] = index["authenticated_by"]
        cache_path = index["cache_path"]
        output_name = index["output_name"]
        if (cache_path is None) != (output_name is None):
            raise ScriptError("Ubuntu source metadata path is incomplete")
        if cache_path is not None and output_name is not None:
            metadata_root.mkdir(parents=True, exist_ok=True)
            destination = metadata_root / output_name
            if destination.exists():
                raise ScriptError(
                    f"duplicate Ubuntu source metadata output: {destination}"
                )
            shutil.copyfile(cache_path, destination)
            actual_sha256 = sha256_file(destination)
            if actual_sha256 != index["sha256"]:
                raise ScriptError(
                    f"Ubuntu source metadata changed while collecting: {cache_path}"
                )
            manifest_index["path"] = destination.relative_to(output).as_posix()
        manifest_indexes.append(manifest_index)
    return manifest_indexes


def collect_ubuntu_sources(
    manifests: list[Path],
    output: Path,
    cache: Path,
) -> None:
    _validate_source_output(output)
    output = output.resolve()
    _validate_source_output(output)
    requirements = _source_requirements(manifests)
    records, indexes = _load_source_records(cache, requirements)
    missing = [
        f"{requirement['source_name']}={requirement['source_version']}"
        for requirement in requirements
        if (
            requirement["source_name"],
            requirement["source_version"],
        )
        not in records
    ]
    if missing:
        raise ScriptError(
            "Ubuntu source indexes and Launchpad do not contain required packages: "
            + ", ".join(missing)
        )

    _reset_source_output(output)
    packages_root = output / "packages"
    packages_root.mkdir(parents=True, exist_ok=True)

    collected: list[dict[str, object]] = []
    for requirement in requirements:
        key = (
            requirement["source_name"],
            requirement["source_version"],
        )
        record = records[key]
        package_directory = packages_root / _encoded_package_directory(*key)
        package_directory.mkdir()
        downloaded: list[dict[str, object]] = []
        dsc_path: Path | None = None
        for source_file in record["files"]:
            path = package_directory / source_file["name"]
            url = source_file["url"]
            download_verified(url, path, source_file["sha256"])
            actual_size = path.stat().st_size
            if actual_size != source_file["size"]:
                raise ScriptError(
                    f"{path.name} is {actual_size} bytes, "
                    f"expected {source_file['size']}"
                )
            if path.suffix == ".dsc":
                if dsc_path is not None:
                    raise ScriptError(f"{key[0]}={key[1]} has multiple .dsc files")
                dsc_path = path
            downloaded.append(
                {
                    **source_file,
                    "path": path.relative_to(output).as_posix(),
                    "url": url,
                }
            )
        if dsc_path is None:
            raise ScriptError(f"{key[0]}={key[1]} has no .dsc file")
        _validate_dsc(dsc_path, record)
        collected.append(
            {
                "source_name": key[0],
                "source_version": key[1],
                "directory": record["directory"],
                "index_url": record["index_url"],
                "index_sha256": record["index_sha256"],
                "index_release_url": record["index_release_url"],
                "index_release_sha256": record["index_release_sha256"],
                "dsc": dsc_path.relative_to(output).as_posix(),
                "files": downloaded,
            }
        )

    source_indexes = _materialize_source_metadata(output, indexes)
    (output / "manifest.json").write_text(
        json.dumps(
            {
                "format": UbuntuBuildConstants.SOURCE_MANIFEST_FORMAT,
                "release": UbuntuBuildConstants.VERSION,
                "codename": UbuntuBuildConstants.CODENAME,
                "architecture": UbuntuBuildConstants.ARCHITECTURE,
                "source_indexes": source_indexes,
                "packages": collected,
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    write_sha256_sums(output)
    print(f">> collected Ubuntu sources in {output}")


def configure_parser(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("manifests", nargs="+", type=Path)
    parser.add_argument(
        "--output",
        type=Path,
        default=BuildConstants.SOURCE_DIR / UbuntuBuildConstants.GUEST_NAME,
    )
    parser.add_argument(
        "--cache",
        type=Path,
        default=(
            BuildConstants.REPO_ROOT
            / BuildConstants.CACHE_DIRECTORY_NAME
            / UbuntuBuildConstants.SOURCE_CACHE_DIRECTORY_NAME
        ),
    )
    parser.set_defaults(handler=command_collect_ubuntu_sources)


def command_collect_ubuntu_sources(args: argparse.Namespace) -> None:
    collect_ubuntu_sources(args.manifests, args.output, args.cache)
