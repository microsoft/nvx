# MicroVM snapshot/restore verification

## Source and scope

Model guest-requested microVM snapshot/restore in `nanvix/openvmm`, not upstream interactive REPL save/restore or the entire VMM. Use the exact revision in this run's CI inputs. Instrument only Specula's private source copy; supplied source and retained assets are read-only.

Prioritize one connected transaction:

1. `MicrovmSnapshotRequest` PIO request, duplicate coalescing, deferred write, release, write-completion acknowledgement, transaction completion, stop/reset cancellation and completed-request reuse.
2. Worker/controller boundary establishment, stop/save ordering, staged publication, capture-and-exit and no-destination/pre-commit recovery.
3. Manifest-driven new-process restore with private guest memory, including two independent restores of one supported reusable artifact without artifact mutation.
4. Consistency between boundary release, committed capture and ordinary source continuation. Releasing a deferred write is not completing the entire transaction.

Starting points:

- `vm/devices/chipset/src/microvm.rs`
- `openvmm/openvmm_core/src/worker/dispatch.rs`
- `openvmm/openvmm_entry/src/vm_controller.rs` and `lib.rs`
- `openvmm/openvmm_defs/src/rpc.rs`
- `openvmm/openvmm_helpers/src/snapshot.rs` and `shared_memory.rs`
- `Guide/src/user_guide/openvmm/snapshots.md`

Consult other files for these interfaces and concrete behavior. Keep finite bounds small: one vCPU, at most two requests/restores, one snapshot generation and abstract memory identity/private mutation. Do not model instruction execution, byte arrays, entire timers/filesystems or whole virtio protocols.

## Model reuse and incremental evidence

Reuse prior analysis, models and harnesses, but assess their applicability to the selected source and actual source diff. Source identities before and after a rebase are not interchangeable. Historical findings are not new discoveries, and historical traces are not fresh execution evidence.

When `/seed` is provided, use native BYOM initialization to adapt retained assets rather than repeating completed archaeology. Current-source execution, trace replay, model validation, confirmation and reporting still apply. Preserve unresolved findings and assess their current applicability; do not turn absent finalized reports into a claim of no bugs.

For `NO_MODEL_CHANGE`, still execute the current implementation and replay fresh traces, identifying exactly what was reused and why. For changed semantics, maintain a complete current reference model as well as update-focused artifacts; do not put replacement behavior only in `Update.tla`.

Record PIO invocation/return, whether polling originates in `poll_device` or `io_write`, and actual accepted/coalesced/notified outcomes. A completed-without-poll scenario must not insert an unobserved poll. Distinguish behavioral incompatibility from parser/schema rejection when comparing old models and new traces.

## Contracts and limitations

Derive contracts from the selected source and documentation. Untiered reusable microVM snapshots are not single-use instance checkpoints: do not generalize repeatability, erase `resume.claim` or bypass a restore gate.

Tiered sandbox resources, control broker/framing, networking, virtio-fs, hotplug, Windows/WHP, KVM, OpenHCL and host REPL operations are outside initial scope. Assess whether changes in those areas affect the in-scope interfaces before excluding them.

Clock/counter and entropy observations do not prove exact hardware post-state, downtime compensation or cryptographic freshness. Never populate observations with model-predicted values. No hostile guests, exploitation, adversarial filesystem manipulation, fuzzing or unrelated vulnerability search is required; confirm normal correctness counterexamples with component tests or the ordinary VM scenario. Report demonstrated impact and limits. Novelty is unknown without supporting source history or tracker evidence.

## Execution and traces

Read `/harness/README.md` and reuse the supplied normal MSHV scenario rather than rebuilding a VM framework:

```text
bash /harness/build.sh PRIVATE_SOURCE CARGO_TARGET_DIR UNIQUE_BUILD_OUTPUT
bash /harness/run.sh --source PRIVATE_SOURCE \
  --binary CARGO_TARGET_DIR/debug/openvmm --output UNIQUE_EVIDENCE_PARENT \
  --kernel /fixtures/vmlinux --initrd /fixtures/initramfs.cpio.gz --json
bash /harness/observe-reuse.sh PRIVATE_SOURCE CARGO_TARGET_DIR UNIQUE_TEST_OUTPUT
```

Bootstrap receipts are not model traces. Build the run's own reusable harness for instrumentation, fresh component/VM traces and replay inputs. Put any copied bootstrap under a distinct subdirectory to avoid `run.sh` recursion. Use the current private source/binary and shared Cargo caches, with unique evidence/build-log directories.

Required evidence is actual MSHV capture and source exit, two independent reusable restores, guest continuation/private-memory observations, unchanged artifact hashes, source/fixture/binary identities and process outcomes. Supplement with nonzero executed component tests for deterministic lifecycle interleavings, nonempty implementation traces, completed replay and bounded model checking.

Emit events at coherent implementation boundaries; wakeups must not allow dependent observations to precede prerequisites. Do not manufacture traces, substitute archived traces for current execution, or treat derived phase counters as unobserved state.

The removed `phase_2_snapshot_restore` selector must not produce a zero-test success. `phase2_snapshot_bench` is a storage benchmark, not a real VM snapshot/restore scenario. Preserve observation limitations instead of weakening expected behavior.

## Resources, recovery and compiler

The outer runtime provides 26 GiB memory, no extra swap, six CPUs and 1024 PIDs. Use at most four Cargo jobs. Writable state, caches, temporary output and TLC state belong under `/work` or `/cache`; use `/work/scratch`, not the 256 MiB `/run` tmpfs.

Every TLC invocation needs explicit small bounds, normally:

```text
-m 6G -M 2G -w 4 -t 10
```

The aggregate TLC budget is 12 GiB/four workers; it does not resize tool defaults. Run campaigns sequentially. Direct Java requires explicit heap, direct-memory, workers and timeout. Distinguish budget-limited search from exhaustive completion at stated finite bounds.

Use finite Cargo/VM/tool timeouts and preserve partial work on failure. Diagnose and repair in place; do not discard a store or restart analysis merely because a command failed. Native resume retains the saved conversation. Recovery uses the pinned runtime's finite budgets; do not add manual loops, change providers or disable filtering. Exhausted policy recovery is terminal and retains its evidence/hold.

Use the runtime-provided protoc 27.1 through `PROTOC` and `PROTOC_INCLUDE`. Do not restore unrelated packages just because `.packages` is absent: absolute compiler/sysroot symlinks violate native source isolation. Do not weaken source validation or substitute a different compiler.

## Report-only boundary

Do not fix product defects. Edits are limited to documented instrumentation, ordinary reproduction/tests, and models/harnesses that preserve product semantics. Retain source diffs and do not change implementation behavior, weaken invariants or hide counterexamples to obtain a pass.

Do not push, create issues/PRs, register runners, publish releases or modify remote systems. Do not read credentials or unrelated host paths. Authentication is supplied by the runtime.

Complete native artifacts and reporting honestly. OOM, missing scenarios, empty traces and incomplete replay are not passes even if a marker exists. A complete FAIL can retain a reusable model, but native publication is not human acceptance or proof that the product is bug-free. Product fixes remain a separate authorized task.
