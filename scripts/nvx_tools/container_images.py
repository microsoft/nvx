"""Resolve the repository's pinned images and apply an optional ACR mirror."""

import json
import os
import re
import subprocess
import sys
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

_REPOSITORY = r"[a-z0-9]+(?:[._-][a-z0-9]+)*(?:/[a-z0-9]+(?:[._-][a-z0-9]+)*)*"
_SERVER = r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.azurecr\.io"
_RESOURCE_ID = re.compile(
    r"/subscriptions/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"
    r"/resourceGroups/[A-Za-z0-9_().-]{1,90}"
    r"/providers/Microsoft\.ContainerRegistry/registries/[a-z0-9]{5,50}",
    re.IGNORECASE,
)


@dataclass(frozen=True)
class ImageReference:
    registry: str
    repository: str
    digest: str

    @property
    def canonical(self) -> str:
        return f"{self.registry}/{self.repository}@{self.digest}"


def image_reference(value: str) -> ImageReference:
    match = re.fullmatch(
        r"(?P<name>[a-z0-9][a-z0-9._/-]*)(?::[A-Za-z0-9_.-]+)?@"
        r"(?P<digest>sha256:[0-9a-f]{64})",
        value,
    )
    if match is None:
        raise ValueError(f"Container image must have a pinned SHA-256 digest: {value}")
    name = match["name"]
    first, separator, repository = name.partition("/")
    if "." in first:
        if first not in ("docker.io", "mcr.microsoft.com") or not separator:
            raise ValueError(f"Unsupported source registry: {value}")
        registry = first
    else:
        registry = "docker.io"
        repository = name if separator else "library/" + name
    if re.fullmatch(_REPOSITORY, repository) is None:
        raise ValueError(f"Container image has an invalid repository: {value}")
    return ImageReference(registry, repository, match["digest"])


def registry_server(value: str) -> str:
    if re.fullmatch(_SERVER, value) is None:
        raise ValueError("Container registry loginServer must be an ACR hostname")
    return value


def repository_prefix(value: str) -> str:
    if re.fullmatch(_REPOSITORY, value) is None:
        raise ValueError("containerRegistry.repositoryPrefix is invalid")
    return value


def registry_resource_id(value: str) -> str:
    if _RESOURCE_ID.fullmatch(value) is None:
        raise ValueError(
            "containerRegistry.resourceId is not a canonical ACR resource ID"
        )
    if value.split("/")[4].endswith("."):
        raise ValueError("containerRegistry.resourceId has an invalid resource group")
    return value


def mirror_prefix(value: str) -> tuple[str, str]:
    server, separator, prefix = value.partition("/")
    if not separator or re.fullmatch(_SERVER, server) is None:
        raise ValueError(
            "NVX_IMAGE_REGISTRY must be an ACR server and repository prefix"
        )
    try:
        repository_prefix(prefix)
    except ValueError as error:
        raise ValueError(
            "NVX_IMAGE_REGISTRY must be an ACR server and repository prefix"
        ) from error
    return server, prefix


def mirrored_image(source: str, prefix: str) -> str:
    mirror_prefix(prefix)
    return f"{prefix}/{image_reference(source).canonical}"


def docker_pins(root: Path) -> dict[str, str]:
    source = (root / "docker" / "Dockerfile").read_text(encoding="utf-8")
    pins: dict[str, str] = {}
    for argument in ("DEBIAN_IMAGE", "RUST_BUILDER_IMAGE", "AZURELINUX_IMAGE"):
        matches = re.findall(rf"(?m)^ARG {argument}=(\S+)$", source)
        if len(matches) != 1:
            raise ValueError(f"Expected one pinned Dockerfile {argument}")
        image_reference(matches[0])
        pins[argument] = matches[0]
    return pins


def pinned_images(root: Path) -> tuple[str, ...]:
    pins = docker_pins(root)
    references = list(pins.values())
    quality = (root / ".github" / "actions" / "check-quality" / "action.yml").read_text(
        encoding="utf-8"
    )
    for repository in ("koalaman/shellcheck-alpine", "mvdan/shfmt"):
        matches = re.findall(rf"\b({repository}@sha256:[0-9a-f]{{64}})\b", quality)
        if len(matches) != 1:
            raise ValueError(f"Expected one pinned {repository} linter image")
        references.extend(matches)
    manifest = json.loads((root / "SOURCE-MANIFEST.json").read_text(encoding="utf-8"))
    lock = json.loads(
        (root / "azurelinux" / "packages.lock.json").read_text(encoding="utf-8")
    )
    azurelinux = image_reference(pins["AZURELINUX_IMAGE"]).canonical
    if manifest["azurelinux"]["image"] != azurelinux or lock["image"] != azurelinux:
        raise ValueError(
            "Azure Linux provenance and package lock must match its Dockerfile pin"
        )
    return tuple(image_reference(reference).canonical for reference in references)


def docker_arguments(root: Path, prefix: str | None) -> list[str]:
    if prefix is None:
        return []
    mirror_prefix(prefix)
    arguments: list[str] = []
    for name, image in docker_pins(root).items():
        arguments.extend(("--build-arg", f"{name}={mirrored_image(image, prefix)}"))
    return arguments


def main(argv: Sequence[str] | None = None) -> int:
    arguments = list(sys.argv[1:] if argv is None else argv)
    if not arguments or arguments[0] in ("-h", "--help"):
        print("Usage: container_images.py build [docker build arguments]")
        return 0
    if arguments[0] != "build":
        print("error: expected the build command", file=sys.stderr)
        return 2
    try:
        overrides = docker_arguments(
            Path(__file__).resolve().parents[2], os.environ.get("NVX_IMAGE_REGISTRY")
        )
        return subprocess.run(
            ["docker", "build", *overrides, *arguments[1:]], check=False
        ).returncode
    except (OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
