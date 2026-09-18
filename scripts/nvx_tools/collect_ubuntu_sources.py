"""Collect exact Ubuntu source packages for generated guest artifacts."""

from __future__ import annotations

import argparse
import json
import lzma
import re
import shutil
import urllib.parse
from collections.abc import Sequence
from pathlib import Path, PurePosixPath
from typing import TypedDict, cast

from .common import (
    REPO_ROOT,
    ScriptError,
    download,
    download_verified,
    sha256_file,
    write_sha256_sums,
)
from .ubuntu import (
    DEFAULT_UBUNTU_ARCHITECTURE,
    DEFAULT_UBUNTU_CODENAME,
    DEFAULT_UBUNTU_VERSION,
    parse_deb822,
)

UBUNTU_SOURCE_INDEXES = (
    (
        "https://archive.ubuntu.com/ubuntu/dists/"
        f"{DEFAULT_UBUNTU_CODENAME}-updates/main/source/Sources.xz",
        "https://archive.ubuntu.com/ubuntu",
    ),
    (
        "https://archive.ubuntu.com/ubuntu/dists/"
        f"{DEFAULT_UBUNTU_CODENAME}-updates/universe/source/Sources.xz",
        "https://archive.ubuntu.com/ubuntu",
    ),
    (
        "https://security.ubuntu.com/ubuntu/dists/"
        f"{DEFAULT_UBUNTU_CODENAME}-security/main/source/Sources.xz",
        "https://security.ubuntu.com/ubuntu",
    ),
    (
        "https://security.ubuntu.com/ubuntu/dists/"
        f"{DEFAULT_UBUNTU_CODENAME}-security/universe/source/Sources.xz",
        "https://security.ubuntu.com/ubuntu",
    ),
    (
        "https://archive.ubuntu.com/ubuntu/dists/"
        f"{DEFAULT_UBUNTU_CODENAME}/main/source/Sources.xz",
        "https://archive.ubuntu.com/ubuntu",
    ),
    (
        "https://archive.ubuntu.com/ubuntu/dists/"
        f"{DEFAULT_UBUNTU_CODENAME}/universe/source/Sources.xz",
        "https://archive.ubuntu.com/ubuntu",
    ),
)
_SHA256 = re.compile(r"[0-9a-f]{64}")
_LAUNCHPAD_ARCHIVE_API = "https://api.launchpad.net/1.0/ubuntu/+archive/primary"
_LAUNCHPAD_SERIES_API = (
    f"https://api.launchpad.net/1.0/ubuntu/{DEFAULT_UBUNTU_CODENAME}"
)


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
    files: list[SourceFile]


def _source_requirements(manifests: list[Path]) -> tuple[SourceRequirement, ...]:
    requirements: dict[tuple[str, str], SourceRequirement] = {}
    for path in manifests:
        raw_document = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(raw_document, dict):
            raise ScriptError(f"{path} must contain a JSON object")
        document = cast(dict[str, object], raw_document)
        if (
            document.get("guest") != "ubuntu"
            or document.get("release") != DEFAULT_UBUNTU_VERSION
            or document.get("architecture") != DEFAULT_UBUNTU_ARCHITECTURE
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


def _load_source_records(
    cache: Path,
    requirements: Sequence[SourceRequirement] = (),
) -> tuple[dict[tuple[str, str], SourceRecord], list[dict[str, str]]]:
    cache.mkdir(parents=True, exist_ok=True)
    records: dict[tuple[str, str], SourceRecord] = {}
    indexes: list[dict[str, str]] = []
    for index_url, archive_url in UBUNTU_SOURCE_INDEXES:
        index_path = cache / _index_cache_name(index_url)
        print(f">> downloading Ubuntu source index {index_url}")
        download(index_url, index_path)
        index_sha256 = sha256_file(index_path)
        indexes.append({"url": index_url, "sha256": index_sha256})
        try:
            contents = lzma.decompress(index_path.read_bytes()).decode("utf-8")
        except (lzma.LZMAError, UnicodeDecodeError) as error:
            raise ScriptError(
                f"invalid Ubuntu source index {index_url}: {error}"
            ) from error
        for package in parse_deb822(contents):
            name = package.get("Package")
            version = package.get("Version")
            directory = package.get("Directory")
            checksums = package.get("Checksums-Sha256")
            if not name or not version or not directory or not checksums:
                continue
            if directory.startswith("/") or ".." in Path(directory).parts:
                raise ScriptError(
                    f"Ubuntu source index has invalid directory {directory!r}"
                )
            key = (name, version)
            source_files = [
                cast(
                    SourceFile,
                    {
                        **source_file,
                        "url": (f"{archive_url}/{directory}/{source_file['name']}"),
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
                "index_sha256": index_sha256,
                "files": source_files,
            }
            previous = records.get(key)
            if previous is not None:
                comparable = {
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
                previous_comparable = {
                    "directory": previous["directory"],
                    "files": [
                        {
                            "name": source_file["name"],
                            "size": source_file["size"],
                            "sha256": source_file["sha256"],
                        }
                        for source_file in previous["files"]
                    ],
                }
                if comparable != previous_comparable:
                    raise ScriptError(
                        f"Ubuntu source indexes disagree about {name}-{version}"
                    )
                continue
            records[key] = record
    for requirement in requirements:
        key = (
            requirement["source_name"],
            requirement["source_version"],
        )
        if key in records:
            continue
        record, metadata = _launchpad_source_record(cache, *key)
        records[key] = record
        indexes.append(metadata)
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


def _launchpad_source_record(
    cache: Path,
    name: str,
    version: str,
) -> tuple[SourceRecord, dict[str, str]]:
    key = _encoded_package_directory(name, version)
    query_url = f"{_LAUNCHPAD_ARCHIVE_API}?" + urllib.parse.urlencode(
        {
            "ws.op": "getPublishedSources",
            "source_name": name,
            "version": version,
            "exact_match": "true",
            "distro_series": _LAUNCHPAD_SERIES_API,
        }
    )
    query_path = cache / f"launchpad-{key}-query.json"
    raw_query = _download_json(query_url, query_path)
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
    self_link = entries[0].get("self_link")
    if not isinstance(self_link, str) or not self_link.startswith(
        f"{_LAUNCHPAD_ARCHIVE_API}/+sourcepub/"
    ):
        raise ScriptError(
            f"Launchpad returned an invalid source publication for {name}"
        )

    urls_url = f"{self_link}?ws.op=sourceFileUrls"
    urls_path = cache / f"launchpad-{key}-urls.json"
    raw_urls = _download_json(urls_url, urls_path)
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
    dsc_names = sorted(filename for filename in urls if filename.endswith(".dsc"))
    if len(dsc_names) != 1:
        raise ScriptError(f"Launchpad returned {len(dsc_names)} .dsc files for {name}")
    dsc_name = dsc_names[0]
    dsc_path = cache / "launchpad-dsc" / key / dsc_name
    download(urls[dsc_name], dsc_path)
    dsc_text = dsc_path.read_text(encoding="utf-8")
    if not dsc_text.startswith("-----BEGIN PGP SIGNED MESSAGE-----\n"):
        raise ScriptError(f"Launchpad source metadata is not clear-signed: {dsc_name}")
    document = _dsc_document(dsc_path)
    if document.get("Source") != name or document.get("Version") != version:
        raise ScriptError(f"Launchpad returned the wrong .dsc for {name}={version}")
    checksums = document.get("Checksums-Sha256")
    if checksums is None:
        raise ScriptError(f"Launchpad .dsc has no SHA-256 metadata: {dsc_name}")
    files: list[SourceFile] = [
        {
            "name": dsc_name,
            "size": dsc_path.stat().st_size,
            "sha256": sha256_file(dsc_path),
            "url": urls[dsc_name],
        }
    ]
    for checksum in _checksum_records(checksums, dsc_name):
        filename = checksum["name"]
        url = urls.get(filename)
        if url is None:
            raise ScriptError(f"Launchpad omitted {filename} referenced by {dsc_name}")
        files.append(cast(SourceFile, {**checksum, "url": url}))
    extra_urls = sorted(set(urls) - {source_file["name"] for source_file in files})
    if extra_urls:
        raise ScriptError(
            f"Launchpad returned unreferenced source files for {name}: "
            + ", ".join(extra_urls)
        )

    metadata_path = cache / f"launchpad-{key}-metadata.json"
    metadata_path.write_text(
        json.dumps(
            {
                "query_url": query_url,
                "query": query,
                "source_urls_url": urls_url,
                "source_urls": raw_urls,
            },
            sort_keys=True,
        )
        + "\n",
        encoding="utf-8",
    )
    metadata_sha256 = sha256_file(metadata_path)
    return (
        {
            "source_name": name,
            "source_version": version,
            "directory": self_link,
            "index_url": urls_url,
            "index_sha256": metadata_sha256,
            "files": sorted(files, key=lambda item: item["name"]),
        },
        {"url": urls_url, "sha256": metadata_sha256},
    )


def _encoded_package_directory(name: str, version: str) -> str:
    return f"{urllib.parse.quote(name, safe='')}_{urllib.parse.quote(version, safe='')}"


def collect_ubuntu_sources(
    manifests: list[Path],
    output: Path,
    cache: Path,
) -> None:
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

    output = output.resolve()
    packages_root = output / "packages"
    if packages_root.exists():
        shutil.rmtree(packages_root)
    for generated in (output / "manifest.json", output / "SHA256SUMS"):
        generated.unlink(missing_ok=True)
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
                "dsc": dsc_path.relative_to(output).as_posix(),
                "files": downloaded,
            }
        )

    output.mkdir(parents=True, exist_ok=True)
    (output / "manifest.json").write_text(
        json.dumps(
            {
                "format": 1,
                "release": DEFAULT_UBUNTU_VERSION,
                "codename": DEFAULT_UBUNTU_CODENAME,
                "architecture": DEFAULT_UBUNTU_ARCHITECTURE,
                "source_indexes": indexes,
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
        default=REPO_ROOT / "build" / "sources" / "ubuntu",
    )
    parser.add_argument(
        "--cache",
        type=Path,
        default=REPO_ROOT / ".cache" / "ubuntu-source-indexes",
    )
    parser.set_defaults(handler=command_collect_ubuntu_sources)


def command_collect_ubuntu_sources(args: argparse.Namespace) -> None:
    collect_ubuntu_sources(args.manifests, args.output, args.cache)
