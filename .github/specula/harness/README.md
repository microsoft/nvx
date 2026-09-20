# MSHV snapshot/restore harness

This harness boots a real Linux microVM, captures an untiered/blockless snapshot through the guest's normal `nvx-snapshot` PIO operation, waits for source exit, then launches two independent restore processes from the same artifact. It does not call a model or modify the product implementation.

## Bounded execution

Run only inside the supplied container, with the host launcher holding `/mnt/data/openvmm-verification/.host.lock`. `resource_check.py` requires accessible `/dev/mshv`, UID:GID 1001:1003, device group 998, a read-only root filesystem, no Docker socket, 26 GiB memory, zero additional swap, six CPUs and 1024 PIDs. There is no KVM fallback.

Use current private source and its binary, not an old cached binary from another revision. Writable paths must resolve below host `/mnt/data` or container `/work` and `/cache`; the supplied source, harness and fixtures are read-only.

```bash
bash /harness/build.sh \
  "$SPECULA_SOURCE" "$CARGO_TARGET_DIR" "$UNIQUE_BUILD_OUTPUT"

bash /harness/run.sh \
  --source "$SPECULA_SOURCE" \
  --binary "$CARGO_TARGET_DIR/debug/openvmm" \
  --output "$SPECULA_OUTPUT/native-evidence" \
  --kernel /fixtures/vmlinux \
  --initrd /fixtures/initramfs.cpio.gz \
  --json
```

`build.sh` uses Rust 1.95, a locked MSHV-only build, four Cargo jobs and an 1800-second build timeout. Explicit `PROTOC` must name an executable; its path and version are retained. Only when `PROTOC` is unset and the packaged compiler is absent does the script attempt package restoration, with a 900-second timeout. The supplied image provides external protoc 27.1 to avoid source symlinks rejected by Specula isolation.

`run.sh` does not build or substitute binaries. Each invocation preserves a new timestamp/UUID evidence directory. Per-VM timeout defaults to 120 seconds, at most 900 via `--process-timeout`. SIGTERM/SIGINT stop and reap the owned active process group, retain interruption evidence and exit nonzero; already-reaped children are not signalled.

## Output contract

The directory can be copied beneath the current Specula output; helpers use paths relative to themselves. Keep a copied bootstrap under a distinct subdirectory if the model creates its own `harness/run.sh`, avoiding recursive invocation. Supply the current instrumented source and binary; instrumentation environment variables are inherited.

With `--json`, the final receipt contains `schema`, `status`, `run_id`, `backend` and an `evidence` path. Exit 0 means the harness checks passed; exit 1 retains failed evidence when a run directory exists. Invalid arguments or paths can fail before receipt creation. `result.schema.json` describes the receipt and `evidence.schema.json` describes the detailed record.

**Neither receipt is a Specula verdict or an implementation trace.** Specula must separately instrument real code, collect fresh traces, replay them and complete its own reporting.

Success requires:

- Source exits 0 after one boot/request, without post-snapshot continuation.
- Each restore exits 37 with exactly one continuation and no cold-boot marker.
- Each restored guest begins with a clean snapshotted private marker, overwrites it, and performs 16 MiB of private RAM writes.
- Restores report different guest-computed SHA-256 hashes of the actual 64-byte restore entropy; this does not prove Linux RNG reseeding.
- Snapshot files retain their original size and hash after each restore.

Evidence includes source identity/diff, binary and fixture hashes, guest output and separate process logs. The derivative initramfs replaces only `/init` without extracting other newc entries; both image identities and `guest-init.sh` are retained. Fixture hash defaults match `../config.json`; explicitly different fixtures require the corresponding hash arguments.

Guest wall/uptime lines are integer-second observations, and host durations are not model clocks. The harness does not establish downtime compensation, precise internal clock advancement or synthetic boundary events.

## Device-lifecycle observations

```bash
bash /harness/component-tests.sh \
  "$SPECULA_SOURCE" "$CARGO_TARGET_DIR" "$UNIQUE_TEST_OUTPUT"

bash /harness/observe-reuse.sh \
  "$SPECULA_SOURCE" "$CARGO_TARGET_DIR" "$UNIQUE_OBSERVATION_OUTPUT"
```

`component-tests.sh` selects existing `snapshot_request` tests with nextest's `agent` profile. The Cargo fallback requires complete successful summaries and at least one actual passing matching test; zero, ignored-only or unrelated-only runs fail.

`observe-reuse.sh` adds the supplied test-only `reuse-observation.rs` to an explicitly private clone, without changing implementation or dependencies or overwriting a different file. It completes one real device request and issues another PIO write before the next explicit poll. `summarize-reuse.py` records actual accepted/coalesced/notified outcomes rather than imposing version-dependent expectations. These are observations, not model traces or evidence of a newly discovered bug.

One snapshot per VM cannot deterministically establish device request-reuse ordering; component observations supplement rather than replace full VM evidence. Network, broker, sandbox block layers and tier gates are outside this harness's initial scope.

## Lightweight regressions

Inside the bounded container, with scratch under `/work`:

```bash
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s /harness/tests -v
```

These use harmless Python children, mocked fixtures and Cargo output; they do not build Rust, boot a VM or call a model.
