#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 STAGE_DIR PHASE2_BENCHMARK" >&2
  exit 2
fi

stage_dir=$1
phase2_benchmark=$2

rm -rf -- "$stage_dir"
mkdir -p -- "$stage_dir"
cp -- "$phase2_benchmark" "$stage_dir/phase2_snapshot_bench"
strip --strip-debug "$stage_dir/phase2_snapshot_bench"
