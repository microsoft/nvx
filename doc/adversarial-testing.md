# Copilot-driven adversarial testing

NVX has a separate adversarial harness that uses GitHub Copilot CLI only as an
adaptive strategist. Copilot selects deterministic guest-side probes. A typed
broker, credential-free executor, independent watchdog, and existing microVM
test harness retain exclusive control over NVX and decide whether containment
held.

This suite complements deterministic pull-request tests. It is not a pull
request gate and must not run untrusted fork code.

## Trust boundary

The controller invokes an already installed and authenticated `copilot`
executable. It never installs Copilot CLI and never starts `copilot login`.
Preflight fails if `copilot --version` or a non-interactive authenticated smoke
prompt fails.

Every strategist invocation uses:

- no available tools and no built-in MCP servers;
- no repository custom instructions, remote control, or `ask_user`;
- no temporary-directory access;
- a single non-interactive prompt and bounded output;
- the Copilot CLI's minimum 30-credit session cap; and
- an additional controller-wide credit budget, action count, and
  wall-clock budget.

Copilot receives a trusted case catalog, categorical observations, coverage,
and the remaining budget. Guest stdout and stderr are truncated and
base64-encoded before they enter a prompt. The model must return exactly:

```json
{"schema_version":1,"case_id":"catalogued-case-id"}
```

The controller selects the tracked `nvx-adversary` custom agent explicitly and
disables autonomous continuation. This prevents the main agent from
dynamically delegating without forwarding the candidate catalog or adding
explanatory text around the action.

The broker rejects duplicate JSON properties, extra properties, unknown or
completed cases, invalid UTF-8, oversized responses, and unsupported schema
versions. A rejected response is never executed. While time and AI-credit
budgets permit, the controller may issue a bounded repair prompt containing
only the rejection and exact allowed identifiers. A model cannot provide a
command, path, environment variable, timeout, resource limit, or oracle
result.

## Components

### Controller and typed broker

The controller owns Copilot preflight, budgets, the adaptive loop, replay,
prefix minimization, target heartbeats, and controller artifacts. The typed
broker loads the tracked case catalogs, validates the one-action schema,
records each accepted action before execution, validates executor
observations, and keeps guest text encoded.

The initial catalogs expose deterministic primitives only:

- `workload-isolation` covers workload identity, capabilities,
  `no_new_privs`, namespaces, managed control, storage aliasing, and network
  policy;
- `guest-isolation` covers lifecycle, abnormal exit, output backpressure,
  virtio-net, interrupt progress, and structured teardown; and
- `snapshot-isolation` covers capture/restore during console, filesystem,
  scratch, network, endpoint-policy, and SMP activity.

Repeated managed-exec, console, SMP, filesystem, memory-growth, and
processor-prefix cases provide bounded process, signal, output, CPU, disk,
inode, and memory churn without allowing the model to synthesize host code.

Each primitive maps to an existing `test-microvm` scenario. Those scenarios
reuse the deterministic suite's OpenVMM process, console, and control-session
harnesses, the managed sandbox lifecycle, strict console markers, immutable
snapshot fingerprints, and structured outcome reports. Expected workload
termination or a deliberately tested guest failure passes only when the
deterministic scenario recognizes the expected outcome and OpenVMM tears down
correctly.

The initial workload catalog exercises the same managed control and isolation
mechanisms through `test-microvm`; it does not yet drive the public persistent
`sandbox provision -> start -> exec -> stop` command sequence. That lifecycle
remains required before the workload-adversary mode is considered complete.
The deterministic `sandbox-lifecycle` scenario already drives that sequence,
so it is the regression baseline for such a primitive.

Model-generated scripts are intentionally unsupported. A future generator
must place its output inside the disposable guest or a separate disposable
builder; it must never add host shell access to the strategist.

### Executor

A dedicated executor entry point starts the JSON-lines executor protocol.
The executor:

1. requires the exact controller NVX commit;
2. runs `scripts/nvx.py verify`;
3. validates and hashes the kernel, initramfs, initramfs package manifest,
   OpenVMM executable, the [nvx-rs](#nvx-rs-on-the-target) that the scenarios
   run, and their provenance documents before baseline boot;
4. starts filesystem and network canaries plus the watchdog;
5. refuses every scenario or canary dispatch after sticky oracle evidence
   records a filesystem or network violation;
6. boots and stops a baseline VM;
7. runs only the scenario mapped by the locally trusted case catalog;
8. reaps contained descendant process trees after timeouts and normal parent
   exits;
9. records every spawned OpenVMM PID in a flushed opt-in journal;
10. inventories structured outcome reports and preserves complete logs; and
11. boots and stops a fresh VM before final success.

Child test processes receive a small environment allowlist. In particular,
GitHub, Copilot, SSH-agent, token, and arbitrary controller environment
variables are not forwarded. The allowlist carries the paths that
[nvx-rs](#nvx-rs-on-the-target) needs: `NVX_RS`, `CARGO_HOME`, and
`RUSTUP_HOME`. It omits Cargo's other settings and credentials, such as
`CARGO_TARGET_DIR`, `RUSTFLAGS`, and registry tokens, and `RUSTUP_TOOLCHAIN`,
so that [`rust-toolchain.toml`](../rust-toolchain.toml) still selects the Rust
release.

The controller may start the executor locally, but production campaigns must
use `--executor-command`. This option names one trusted executable and accepts
no arguments. The executable is an administrator-owned wrapper that forwards
stdin/stdout to a forced executor command on a separately provisioned target.
It must not evaluate protocol data as a shell command.

The wrapper and target provisioner must:

- create a fresh disposable target for every executor process;
- stage the exact NVX commit, provenance-bound artifacts, and
  [nvx-rs](#nvx-rs-on-the-target) before exposure;
- provide KVM, MSHV, or WHP without production or GitHub credentials;
- deny production network access and bound target CPU, memory, disk, and time;
- run only the executor entry point for the restricted principal;
- let the controller classify connection loss or timeout as target
  unavailability;
- quarantine and reimage the target after every run; and
- move the target artifact directory to access-controlled security storage.

This controller/target split is required for CI. Local mode is useful for
development, but a controller on the target host cannot reliably distinguish
a host crash from its own failure.

Local executor artifacts use the short `build/adv/<run-id>` state root rather
than nesting below `--output-dir`. This keeps the deepest deterministic
microVM log paths below the legacy Windows `MAX_PATH` boundary. The controller
metadata and summary record the absolute target artifact root.

### nvx-rs on the target

Besides the guest and OpenVMM artifacts, a target needs nvx-rs, the Rust
command-line interface that `nvx.py` runs ([Usage](usage.md#nvx-rs)). Of the
catalogued scenarios, `managed-lifecycle` runs the nvx-rs test
`managed_lifecycle` with Cargo, and `l3-l4-egress-policy` compiles its egress
policy with nvx-rs and runs `nvx.py run`, which runs nvx-rs too. They find
nvx-rs where `nvx.py` does: at `NVX_RS`, then at
`nvx-rs/target/release/nvx-rs[.exe]`, and otherwise they build it there with
Cargo.

A target therefore needs the Rust toolchain: rustup, with Cargo on the
executor's `PATH` and the Rust release that `rust-toolchain.toml` pins
installed. A prebuilt nvx-rs at `NVX_RS` or in `nvx-rs/target/release` saves
its build but not the toolchain, which `managed-lifecycle` needs. Because the
executor passes `NVX_RS`, `CARGO_HOME`, and `RUSTUP_HOME`, either may live
outside the home directory.

A scenario's timeout includes any build, an existing nvx-rs is never rebuilt,
and a target may have no network access for Cargo. So, before exposure, build
both at the staged commit from the repository root, and record the provenance
of the nvx-rs build:

```bash
cargo build --release --locked --features cli --manifest-path nvx-rs/Cargo.toml --target-dir nvx-rs/target
cargo test --no-run --locked --manifest-path nvx-rs/Cargo.toml --test managed_lifecycle
python3 scripts/nvx.py record-nvx-rs-provenance
```

Because `nvx.py` never rebuilds an nvx-rs that exists, the executor checks it
with the other artifacts before the baseline boot: the provenance in
`build/nvx-rs.provenance.json` must describe the nvx-rs that the scenarios
run, the one at `NVX_RS` or else in `nvx-rs/target/release`, as built from the
staged commit's nvx-rs sources. A stale build, or a missing one, fails the
initialization instead of running unnoticed. Run `record-nvx-rs-provenance`
with `NVX_RS` set when it names the build. On a target with the release built
from the staged commit, [`download`](usage.md#download) installs both.

### Independent oracles

The executor-side oracle watchdog runs without consulting Copilot. It
continuously records monotonic heartbeats, child CPU time, disk use, load where
available, a synthetic filesystem-canary hash, and unexpected connections to
a loopback canary service.

The broker fails an action for:

- a deterministic scenario failure or timeout;
- incomplete process or structured teardown;
- OpenVMM or executor unavailability;
- filesystem-canary modification;
- any network-canary connection; or
- a failed clean post-campaign boot.

An executor heartbeat timeout is also a controller-side failure. External
infrastructure must use that signal to quarantine the target rather than
returning it to the pool.

A filesystem- or network-canary violation is a suspected escape. The executor
does not launch the post-campaign VM in that state; it closes its oracles,
returns `quarantined`, and exits so the wrapper can prevent target reuse.
Shorter-prefix minimization, when budget permits, uses newly provisioned
targets rather than the quarantined machine.

## Running a campaign

First prepare the normal NVX guest and OpenVMM release artifacts, and
[nvx-rs](#nvx-rs-on-the-target). For a local KVM virtual machine:

```bash
python3 scripts/nvx.py test-adversarial \
  --backend kvm \
  --campaign workload-isolation \
  --host-type virtual-machine \
  --budget-seconds 900 \
  --budget-actions 8 \
  --budget-ai-credits 300 \
  --seed 1234 \
  --output-dir build/test-results/adversarial-kvm
```

Linux supports `kvm` and `mshv`; Windows supports `whp`. `--host-type` records
`baremetal` or `virtual-machine`. If omitted, the harness records
`NVX_HOST_TYPE` or `unspecified`.

For the required two-node topology, configure a no-argument wrapper and run:

```bash
python3 scripts/nvx.py test-adversarial \
  --backend kvm \
  --campaign guest-isolation \
  --host-type virtual-machine \
  --executor-command nvx-adversarial-remote \
  --output-dir build/test-results/adversarial-kvm
```

The wrapper is trusted infrastructure configuration, not model input. A replay
requires the original `actions.jsonl` and sibling `replay-manifest.json`; the
controller verifies the action hash, case-catalog digest, campaign settings,
and local-versus-external executor topology before dispatching an action.

Copilot CLI's current `--max-ai-credits` minimum is 30. Every invocation has
that 30-credit cap. Adaptive campaigns require at least 60 credits so the
authentication preflight cannot consume the reservation for the first
adversarial action, and the controller starts later invocations only when the
campaign has at least 30 credits remaining. Afterward it charges the ceiling of
`totalNanoAiu` reported by Copilot; missing or malformed usage is
conservatively charged as 30 credits. The CLI cap is a documented soft cap
because usage is known after a model response. Per-invocation usage and
controller accounting JSON are preserved for auditing.

## Replay and minimization

Every accepted action is flushed to `actions.jsonl` before dispatch. Replay
validates the same schema and does not look for, authenticate, or invoke
Copilot:

```bash
python3 scripts/nvx.py test-adversarial \
  --backend kvm \
  --campaign workload-isolation \
  --host-type virtual-machine \
  --replay build/test-results/adversarial-kvm/run-.../actions.jsonl \
  --output-dir build/test-results/adversarial-kvm-replay
```

After a target anomaly, the controller uses new executor processes to replay
progressively shorter action prefixes while wall-clock budget remains. A
remote wrapper therefore provisions a fresh target for every minimization
attempt. Local mode does not minimize policy violations because another local
executor process would still run on the quarantined host. The shortest
reproduced prefix is written to
`minimized-actions.jsonl`; promote a confirmed minimized failure into the
deterministic `test-microvm` suite.

## Artifacts

The controller run directory contains:

- controller and target commit, platform, host type, dirty state, Copilot
  version/model, seed, and budgets;
- raw Copilot responses and per-invocation usage statistics;
- `actions.jsonl`, normalized `observations.jsonl`, and a replay manifest;
- target initialization and finalization reports;
- minimization attempts and the shortest reproduced prefix; and
- a full summary plus a guest-text-free `public-summary.json`.

The target directory contains watchdog samples, canary access logs, bounded
controller-process output, complete per-scenario OpenVMM logs, resource
samples, OpenVMM PID journals, outcome-report hashes and teardown fields,
baseline artifacts, and post-campaign canary-boot artifacts.

Potential escape payloads, target logs, Copilot transcripts, and full outcome
reports must go to access-controlled security storage. The GitHub Actions
workflow uploads only `public-summary.json`, case identifiers, and the replay
manifest. Its external provisioner owns collection and quarantine of the full
controller and target directories.

## CI policy

The adversarial GitHub Actions workflow runs only by trusted manual dispatch or
a schedule on `dev`. It does not handle pull requests or fork code and does not
install or authenticate Copilot CLI.

The workflow runs on a dedicated `nvx-adversarial-controller` runner and
requires the `NVX_ADVERSARIAL_EXECUTOR` repository variable to name its
administrator-owned remote wrapper. It must never point at the persistent
build, microVM-test, or performance runner fleet. Controller and executor
images, network isolation, artifact collection, quarantine, and reimage are
infrastructure prerequisites; a missing prerequisite fails preflight rather
than skipping a campaign.
