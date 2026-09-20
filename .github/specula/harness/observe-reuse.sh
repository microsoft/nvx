#!/usr/bin/env bash
set -euo pipefail
HARNESS_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SOURCE="${1:?usage: observe-reuse.sh PRIVATE_SOURCE TARGET UNIQUE_OUTPUT}"
export CARGO_TARGET_DIR="${2:?target directory required}"
OUTPUT="${3:?unique output directory required}"
mkdir "$OUTPUT"
python3 "$HARNESS_DIR/resource_check.py" >"$OUTPUT/resources.json"
export TMPDIR="$OUTPUT/scratch"
mkdir "$TMPDIR"
export CARGO_BUILD_JOBS=4 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_DEV_INCREMENTAL=true
cd "$SOURCE"
git rev-parse HEAD >"$OUTPUT/source.sha"
git status --porcelain=v1 >"$OUTPUT/source-before.status"
TEST="vm/devices/chipset/tests/native_snapshot_request_reuse.rs"
if [[ -e "$TEST" ]]; then
    cmp "$HARNESS_DIR/reuse-observation.rs" "$TEST"
else
    mkdir -p "$(dirname "$TEST")"
    cp "$HARNESS_DIR/reuse-observation.rs" "$TEST"
fi
sha256sum "$TEST" >"$OUTPUT/test.sha256"
cp "$TEST" "$OUTPUT/test-source.rs"
git status --porcelain=v1 >"$OUTPUT/source-after.status"
status=0
if cargo nextest --version >"$OUTPUT/nextest.version" 2>&1; then
    timeout --kill-after=20s 900s cargo nextest run --profile agent --locked \
        -p chipset --test native_snapshot_request_reuse --success-output immediate \
        >"$OUTPUT/tests.log" 2>&1 || status=$?
else
    timeout --kill-after=20s 900s cargo test --locked -p chipset \
        --test native_snapshot_request_reuse -- --nocapture \
        >"$OUTPUT/tests.log" 2>&1 || status=$?
fi
printf '%s\n' "$status" >"$OUTPUT/tests.exit"
python3 "$HARNESS_DIR/resource_check.py" >"$OUTPUT/resources-after.json"
cat "$OUTPUT/tests.log"
if ((status == 0)); then
    python3 "$HARNESS_DIR/summarize-reuse.py" "$OUTPUT"
fi
exit "$status"
