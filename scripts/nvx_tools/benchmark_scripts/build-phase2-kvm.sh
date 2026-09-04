#!/bin/sh
set -eu

export CARGO_TARGET_DIR=target/bench-linux
export PROTOC=/usr/bin/protoc
exec cargo build --release -p openvmm_helpers --example phase2_snapshot_bench
