#!/usr/bin/env bash
set -euo pipefail
HARNESS_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SOURCE="${1:?usage: build.sh SOURCE TARGET BUILD_EVIDENCE_DIR}"
TARGET="${2:?target directory required}"
OUTPUT="${3:?unique build evidence directory required}"
mkdir "$OUTPUT"
python3 "$HARNESS_DIR/resource_check.py" >"$OUTPUT/resources.json"
export CARGO_TARGET_DIR="$TARGET"
export CARGO_BUILD_JOBS=4
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_DEV_INCREMENTAL=true
export TMPDIR="$OUTPUT/scratch"
mkdir -p "$TARGET" "$TMPDIR"
cd "$SOURCE"
git rev-parse HEAD >"$OUTPUT/source.sha"
git status --porcelain=v1 >"$OUTPUT/source.status"
git diff --binary HEAD >"$OUTPUT/source.diff"
rustc --version >"$OUTPUT/rustc.version"
cargo --version >"$OUTPUT/cargo.version"
if [[ ! ${PROTOC+x} ]]; then
    if [[ ! -x .packages/Google.Protobuf.Tools/tools/protoc ]]; then
        printf '%s\n' 'Missing packaged protoc; restoring native pinned packages (900s bound).'
        timeout --kill-after=20s 900s cargo xflowey restore-packages --no-compat-igvm \
            >"$OUTPUT/restore-packages.log" 2>&1
    fi
    PROTOC="$PWD/.packages/Google.Protobuf.Tools/tools/protoc"
fi
if [[ ! -f "$PROTOC" || ! -x "$PROTOC" ]]; then
    printf 'Selected PROTOC is not an executable file: %s\n' "$PROTOC" >&2
    exit 1
fi
PROTOC="$(realpath -- "$PROTOC")"
export PROTOC
printf '%s\n' "$PROTOC" >"$OUTPUT/protoc.path"
timeout --kill-after=5s 30s "$PROTOC" --version >"$OUTPUT/protoc.version" 2>&1
printf '%s\n' 'Building actual OpenVMM MSHV binary (1800s bound).'
status=0
timeout --kill-after=20s 1800s cargo build --locked -p openvmm \
    --no-default-features --features virt_mshv >"$OUTPUT/build.log" 2>&1 || status=$?
printf '%s\n' "$status" >"$OUTPUT/build.exit"
python3 "$HARNESS_DIR/resource_check.py" >"$OUTPUT/resources-after.json"
if ((status)); then
    tail -100 "$OUTPUT/build.log" >&2
    exit "$status"
fi
sha256sum "$TARGET/debug/openvmm" >"$OUTPUT/binary.sha256"
printf 'Build succeeded: %s\n' "$TARGET/debug/openvmm"
