"""Mirror pinned runner images into an Azure Container Registry before registration."""

import argparse
import http.client
import json
import os
import stat
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import cast

from . import common
from .common import BootstrapError

if __package__ and __package__.startswith("scripts."):
    from scripts.nvx_tools import container_images
else:
    from nvx_tools import container_images

MANIFEST_PATH = Path("/opt/nvx-bootstrap/container-images.json")
IMPORT_API_VERSION = "2023-07-01"
IMPORT_WAIT_SECONDS = 20 * 60
ACCEPT_MANIFESTS = ", ".join(
    (
        "application/vnd.oci.image.index.v1+json",
        "application/vnd.docker.distribution.manifest.list.v2+json",
        "application/vnd.oci.image.manifest.v1+json",
        "application/vnd.docker.distribution.manifest.v2+json",
    )
)


@dataclass(frozen=True)
class RegistrySpec:
    resource_id: str
    prefix: str


@dataclass(frozen=True)
class Registry(RegistrySpec):
    server: str

    @property
    def image_prefix(self) -> str:
        return f"{self.server}/{self.prefix}"


@dataclass(frozen=True)
class Image:
    source_registry: str
    source_repository: str
    digest: str
    target_repository: str

    def reference(self, registry: Registry) -> str:
        return f"{registry.server}/{self.target_repository}@{self.digest}"


def read_registry(value: object) -> RegistrySpec:
    config = common.json_object(value, "The containerRegistry configuration")
    if set(config) != {"resourceId", "repositoryPrefix"}:
        raise BootstrapError(
            "containerRegistry must contain resourceId and repositoryPrefix"
        )
    try:
        resource_id = container_images.registry_resource_id(
            common.text(config, "resourceId", "containerRegistry")
        )
        prefix = container_images.repository_prefix(
            common.text(config, "repositoryPrefix", "containerRegistry")
        )
    except ValueError as error:
        raise BootstrapError(str(error)) from error
    return RegistrySpec(resource_id, prefix)


def load_images(
    registry: RegistrySpec, path: Path = MANIFEST_PATH
) -> tuple[Image, ...]:
    manifest = common.load_json(path, "The container image manifest")
    values = manifest.get("images")
    if manifest.get("format") != 1 or not isinstance(values, list) or not values:
        raise BootstrapError("The container image manifest is invalid")
    images: list[Image] = []
    for reference in cast(list[object], values):
        if not isinstance(reference, str):
            raise BootstrapError("The container image manifest has an invalid image")
        try:
            image = container_images.image_reference(reference)
        except ValueError as error:
            raise BootstrapError(str(error)) from error
        source_registry = image.registry
        images.append(
            Image(
                source_registry=(
                    "registry.hub.docker.com"
                    if source_registry == "docker.io"
                    else source_registry
                ),
                source_repository=image.repository,
                digest=image.digest,
                target_repository=(
                    f"{registry.prefix}/{source_registry}/{image.repository}"
                ),
            )
        )
    if len({image.target_repository for image in images}) != len(images):
        raise BootstrapError("The container image manifest repeats a repository")
    return tuple(images)


def identity_token(resource: str) -> str:
    url = (
        common.IMDS_ENDPOINT
        + "/metadata/identity/oauth2/token?api-version=2019-08-01&resource="
        + urllib.parse.quote(resource, safe="")
    )
    response = common.request_json(url, {"Metadata": "true"})
    token = response.get("access_token")
    if not isinstance(token, str) or not token or any(c.isspace() for c in token):
        raise BootstrapError("Managed identity returned no valid registry access token")
    return token


class RejectRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(
        self,
        req: urllib.request.Request,
        fp: object,
        code: int,
        msg: str,
        headers: object,
        newurl: str,
    ) -> None:
        raise BootstrapError(
            "A registry endpoint redirected a credential-bearing request"
        )


def open_request(
    url: str,
    headers: Mapping[str, str],
    *,
    method: str = "GET",
    data: bytes | None = None,
) -> http.client.HTTPResponse:
    request = urllib.request.Request(
        url, data=data, headers=dict(headers), method=method
    )
    return cast(
        http.client.HTTPResponse,
        urllib.request.build_opener(RejectRedirect()).open(
            request, timeout=common.HTTP_TIMEOUT_SECONDS
        ),
    )


def resolve_registry(spec: RegistrySpec, token: str) -> Registry:
    url = f"https://management.azure.com{spec.resource_id}?api-version={IMPORT_API_VERSION}"
    try:
        with open_request(url, {"Authorization": f"Bearer {token}"}) as response:
            result = common.json_object(json.load(response), "The ACR resource")
    except urllib.error.HTTPError as error:
        raise BootstrapError(
            f"ACR resource lookup failed with HTTP {error.code}"
        ) from error
    resource_id = common.text(result, "id", "ACR resource")
    if resource_id.lower() != spec.resource_id.lower():
        raise BootstrapError(
            "ACR resource lookup did not identify the configured resource"
        )
    properties = common.json_object(result.get("properties"), "The ACR properties")
    try:
        server = container_images.registry_server(
            common.text(properties, "loginServer", "ACR properties")
        )
    except ValueError as error:
        raise BootstrapError(str(error)) from error
    return Registry(spec.resource_id, spec.prefix, server)


def form_token(url: str, values: Mapping[str, str], what: str) -> str:
    try:
        with open_request(
            url,
            {"Content-Type": "application/x-www-form-urlencoded"},
            method="POST",
            data=urllib.parse.urlencode(values).encode(),
        ) as response:
            result = common.json_object(json.load(response), "The ACR token response")
    except urllib.error.HTTPError as error:
        raise BootstrapError(f"{what} failed with HTTP {error.code}") from error
    token = result.get(what)
    if not isinstance(token, str) or not token or any(c.isspace() for c in token):
        raise BootstrapError(f"ACR returned no valid {what}")
    return token


def azure_cli_token(resource: str) -> str:
    command = common.which("az")
    if command is None:
        raise BootstrapError("Azure CLI is required with --azure-cli")
    result = common.run_command(
        [command, "account", "get-access-token", "--resource", resource, "-o", "json"]
    )
    response = common.json_object(json.loads(result.stdout), "The Azure CLI response")
    token = response.get("accessToken")
    if not isinstance(token, str) or not token or any(c.isspace() for c in token):
        raise BootstrapError("Azure CLI returned no valid registry access token")
    return token


def refresh_token(server: str, access_token: str) -> str:
    return form_token(
        f"https://{server}/oauth2/exchange",
        {
            "grant_type": "access_token",
            "service": server,
            "access_token": access_token,
        },
        "refresh_token",
    )


def pull_token(server: str, image: Image, refresh: str) -> str:
    return form_token(
        f"https://{server}/oauth2/token",
        {
            "grant_type": "refresh_token",
            "service": server,
            "scope": f"repository:{image.target_repository}:pull",
            "refresh_token": refresh,
        },
        "access_token",
    )


def docker_login(server: str, refresh: str, directory: Path | None = None) -> None:
    arguments = ["docker"]
    if directory is not None:
        arguments += ["--config", str(directory)]
    common.run_command(
        [
            *arguments,
            "login",
            server,
            "--username",
            "00000000-0000-0000-0000-000000000000",
            "--password-stdin",
        ],
        stdin=(refresh + "\n").encode(),
    )


def write_environment(path: Path, prefix: str) -> None:
    container_images.mirror_prefix(prefix)
    if path.parent.is_symlink() or path.is_symlink():
        raise BootstrapError("Registry environment paths must not be symlinks")
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        metadata = path.stat()
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
            raise BootstrapError(
                "Registry environment must be a regular file with one link"
            )
    descriptor, temporary = tempfile.mkstemp(prefix=".nvx-registry-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as stream:
            stream.write(f"export NVX_IMAGE_REGISTRY={prefix}\n")
        os.chmod(temporary, 0o644)
        os.replace(temporary, path)
    except BaseException:
        Path(temporary).unlink(missing_ok=True)
        raise


def provision(config: Path, manifest: Path, environment_file: Path) -> int:
    spec = read_registry(
        common.load_json(config, "The container registry configuration")
    )
    registry = sync_images(spec, manifest)
    write_environment(environment_file, registry.image_prefix)
    common.marker("NVX_CLOUD_IMAGES_READY")
    return 0


def manifest_exists(registry: Registry, image: Image, token: str) -> bool:
    url = (
        f"https://{registry.server}/v2/{image.target_repository}"
        f"/manifests/{image.digest}"
    )
    try:
        with open_request(
            url,
            {"Authorization": f"Bearer {token}", "Accept": ACCEPT_MANIFESTS},
            method="HEAD",
        ) as response:
            if response.status != 200:
                raise BootstrapError(
                    f"ACR manifest lookup returned HTTP {response.status}"
                )
            if response.headers.get("Docker-Content-Digest") != image.digest:
                raise BootstrapError("ACR manifest lookup returned a different digest")
            return True
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return False
        raise BootstrapError(
            f"ACR manifest lookup failed with HTTP {error.code}"
        ) from error


def pause(deadline: float) -> None:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise BootstrapError(
            "ACR import timed out before the pinned digest was published"
        )
    common.sleep(min(10, remaining))


def wait_for_manifest(
    registry: Registry, image: Image, refresh: str, deadline: float
) -> None:
    while True:
        token = pull_token(registry.server, image, refresh)
        if manifest_exists(registry, image, token):
            return
        pause(deadline)


def poll_import(location: str, registry: Registry, token: str, deadline: float) -> None:
    url = urllib.parse.urlsplit(location)
    subscription = registry.resource_id.split("/")[2]
    if (
        url.scheme != "https"
        or url.netloc != "management.azure.com"
        or not url.path.lower().startswith(f"/subscriptions/{subscription.lower()}/")
        or "/providers/microsoft.containerregistry/" not in url.path.lower()
        or url.fragment
    ):
        raise BootstrapError("ACR import returned an unexpected operation URL")
    while True:
        try:
            with open_request(
                location, {"Authorization": f"Bearer {token}"}
            ) as response:
                data = response.read()
                state: object = None
                if data:
                    result = common.json_object(
                        json.loads(data), "The ACR import operation"
                    )
                    state = result.get("status")
                if state == "Succeeded" or (not data and response.status == 200):
                    return
                if state in ("Failed", "Canceled", "Cancelled"):
                    raise BootstrapError(f"ACR import ended with status {state}")
                if response.status != 202 and state not in ("Running", "InProgress"):
                    raise BootstrapError("ACR import returned an unexpected status")
        except urllib.error.HTTPError as error:
            if error.code == 409:
                print("ACR import conflict; waiting for the pinned target digest")
                return
            raise BootstrapError(
                f"ACR import status failed with HTTP {error.code}"
            ) from error
        pause(deadline)


def import_image(registry: Registry, image: Image, token: str, deadline: float) -> None:
    url = (
        f"https://management.azure.com{registry.resource_id}"
        f"/importImage?api-version={IMPORT_API_VERSION}"
    )
    body = json.dumps(
        {
            "source": {
                "registryUri": image.source_registry,
                "sourceImage": f"{image.source_repository}@{image.digest}",
            },
            "untaggedTargetRepositories": [image.target_repository],
            "mode": "NoForce",
        }
    ).encode()
    try:
        with open_request(
            url,
            {"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
            method="POST",
            data=body,
        ) as response:
            if response.status == 200:
                return
            if response.status != 202:
                raise BootstrapError(f"ACR import returned HTTP {response.status}")
            location = response.headers.get("Location")
            if not location:
                raise BootstrapError("ACR import returned no operation URL")
    except urllib.error.HTTPError as error:
        if error.code == 409:
            print("ACR import conflict; waiting for the pinned target digest")
            return
        raise BootstrapError(f"ACR import failed with HTTP {error.code}") from error
    poll_import(location, registry, token, deadline)


def sync_images(
    spec: RegistrySpec,
    manifest: Path = MANIFEST_PATH,
    *,
    token_provider: Callable[[str], str] | None = None,
) -> Registry:
    images = load_images(spec, manifest)
    get_token = token_provider or identity_token
    registry = resolve_registry(spec, get_token("https://management.azure.com/"))
    refresh = refresh_token(
        registry.server, get_token("https://containerregistry.azure.net")
    )
    for image in images:
        token = pull_token(registry.server, image, refresh)
        if manifest_exists(registry, image, token):
            print(f"ACR image already present: {image.reference(registry)}")
            continue
        print(f"Importing pinned image: {image.reference(registry)}")
        deadline = time.monotonic() + IMPORT_WAIT_SECONDS
        import_image(
            registry, image, get_token("https://management.azure.com/"), deadline
        )
        wait_for_manifest(registry, image, refresh, deadline)
    return registry


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--registry-resource-id", required=True)
    parser.add_argument("--repository-prefix", default="nvx")
    parser.add_argument("--manifest", type=Path, default=MANIFEST_PATH)
    parser.add_argument(
        "--azure-cli",
        action="store_true",
        help="Use the workstation's Azure CLI login instead of VM managed identity",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="List image mappings without network access",
    )
    parser.add_argument(
        "--docker-login",
        action="store_true",
        help="Authenticate the caller's Docker client after syncing",
    )
    arguments = parser.parse_args(argv)
    try:
        spec = read_registry(
            {
                "resourceId": arguments.registry_resource_id,
                "repositoryPrefix": arguments.repository_prefix,
            }
        )
        if arguments.dry_run:
            if arguments.docker_login:
                parser.error("--docker-login cannot be combined with --dry-run")
            for image in load_images(spec, arguments.manifest):
                print(
                    f"{image.source_registry}/{image.source_repository}@{image.digest}"
                    f" -> {spec.resource_id}:{image.target_repository}@{image.digest}"
                )
        else:
            registry = sync_images(
                spec,
                arguments.manifest,
                token_provider=azure_cli_token if arguments.azure_cli else None,
            )
            if arguments.docker_login:
                get_token = azure_cli_token if arguments.azure_cli else identity_token
                refresh = refresh_token(
                    registry.server, get_token("https://containerregistry.azure.net")
                )
                docker_login(registry.server, refresh)
            print(f"NVX_IMAGE_REGISTRY={registry.image_prefix}")
    except (BootstrapError, OSError, ValueError) as error:
        return common.fail(str(error))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
