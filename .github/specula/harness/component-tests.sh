#!/usr/bin/env bash
set -euo pipefail
HARNESS_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SOURCE="${1:?usage: component-tests.sh SOURCE TARGET OUTPUT}"
export CARGO_TARGET_DIR="${2:?target required}"
OUTPUT="${3:?unique output directory required}"
mkdir "$OUTPUT"
python3 "$HARNESS_DIR/resource_check.py" >"$OUTPUT/resources.json"
export TMPDIR="$OUTPUT/scratch"
mkdir "$TMPDIR"
export CARGO_BUILD_JOBS=4 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_DEV_INCREMENTAL=true
cd "$SOURCE"
git rev-parse HEAD >"$OUTPUT/source.sha"
if cargo nextest --version >"$OUTPUT/nextest.version" 2>&1; then
    timeout --kill-after=20s 900s cargo nextest run --profile agent --locked \
        -p chipset -E 'test(snapshot_request)' >"$OUTPUT/tests.log" 2>&1
else
    timeout --kill-after=20s 900s cargo test --locked -p chipset snapshot_request \
        >"$OUTPUT/tests.log" 2>&1
    python3 "$HARNESS_DIR/verify_cargo_tests.py" "$OUTPUT/tests.log" snapshot_request \
        >"$OUTPUT/test-count.json"
fi
cat "$OUTPUT/tests.log"
