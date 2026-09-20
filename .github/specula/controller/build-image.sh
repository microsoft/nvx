#!/usr/bin/env bash
set -euo pipefail
controller="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
root=/mnt/data/openvmm-verification
sha=088049c5b3474340213cded2664cdb674bff1e1a
source="$root/repos/specula-latest-20260916"
image=local/openvmm-specula-native:088049c-policy2
protoc="$root/native-ci/cache/protoc-27.1"
exec 9>"$root/.host.lock"
flock -n 9 || { echo "Another verification build/run owns .host.lock." >&2; exit 1; }
test "$(git -C "$source" rev-parse HEAD)" = "$sha"
if [[ ! -x "$protoc/bin/protoc" ]]; then
    echo "Missing retained protoc 27.1 package at $protoc; provision the pinned package first." >&2
    exit 1
fi
context="$root/native-ci/cache/build-$(date -u +%Y%m%dT%H%M%SZ)-$$"
mkdir -p "$context/specula" "$context/runtime"
git -C "$source" archive "$sha" | tar -x -C "$context/specula"
cp "$controller/Dockerfile" "$context/Dockerfile"
cp "$controller/runtime/"*.py "$context/runtime/"
cp -a "$protoc" "$context/protoc"
printf '%s\n' "$sha" > "$context/runtime/specula-commit"
# The legacy builder can use the immutable daemon-local image directly.
# Each RUN has bounded memory/CPU; no persistent BuildKit daemon is started.
DOCKER_BUILDKIT=0 docker build --rm --force-rm \
    --memory=26g --memory-swap=26g --cpu-period=100000 --cpu-quota=600000 \
    --tag "$image" "$context" 2>&1 | tee "$context/build.log"
docker image inspect "$image" > "$controller/image.json"
printf 'Built %s\nContext/log: %s\n' "$image" "$context"
