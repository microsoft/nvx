# NVX Repository Instructions

## Repository Boundaries

- The integration repository is `microsoft/nvx`, and its default branch is `dev`.
  The `nanvix/nvx` repository is archived; do not create new branches or pull
  requests there. Verify configured remotes before pushing.
- `openvmm/` is a Git submodule that tracks the `feature/microvm/dev` branch of
  `microsoft/openvmm`. For OpenVMM source changes, work in the nested repository
  and follow [its instructions](../openvmm/.github/copilot-instructions.md). For
  an NVX pin promotion, use the `nvx-openvmm-promote` skill: keep the gitlink
  commit gitlink-only unless the task explicitly requires coordinated NVX
  changes, and map the pinned commits in
  [the OpenVMM upstream roadmap](../doc/openvmm-upstream-roadmap.md) in a
  separate commit.
- Preserve dirty state in both repositories. Do not reset, clean, switch, update,
  or synchronize the submodule over uncommitted work.

## Validation

- Run `python3 scripts/nvx.py verify` on Linux or `python scripts\nvx.py verify`
  on Windows before build, run, debug, or benchmark work.
- Treat [check-quality](./actions/check-quality/action.yml) and
  [validate-nvx](./actions/validate-nvx/action.yml) as the authoritative local
  quality and CLI test definitions. Start with the narrowest affected test, then
  run the applicable commands from those actions.
- For changes under `nvx-rs/`, run the commands in
  [check-nvx-rs](./actions/check-nvx-rs/action.yml). It also checks the
  runtime adapter in `tests/runtime-adapter/`, which builds on `nvx`; run its
  steps for changes to either.
  They need no hypervisor.
- OpenVMM source changes also require the package-scoped checks prescribed by the
  nested repository. Do not claim unavailable hardware or cross-platform gates
  passed; report the missing prerequisite and rely on CI when appropriate.

## Documentation

- In [`nvx-rs/README.md`](../nvx-rs/README.md#layout), each row of the module
  table says in one line, of at most 200 characters, what a module is for, and
  links to the module's section after the table, which holds the module's
  details. Put a module's details in its section, wrapped like the rest of the
  README, and change its row only when the module's purpose changes.

## Platforms And Hosts

- Linux supports `kvm` or `mshv`; Windows supports only `whp`. Select one backend
  explicitly for runtime operations.
- A performance platform is `<os>-<backend>-<host-type>`. The Azure CI runners are
  `virtual-machine`; configured SSH profiles use the `host_type` returned by
  `.github/skills/nvx-host-connect/scripts/hosts.py`. For local measurements, ask
  for the host type when it is not explicit. Never default it to `baremetal`.
- Use `nvx-host-connect` before remote work, then exactly one of `nvx-run`,
  `nvx-debug`, or `nvx-benchmark`. Never expose `.nvx-hosts.json` or authentication
  material.

## Copilot Cloud Agent Sessions

- Sessions started from GitHub, such as from an issue, a pull request, or the
  agents panel, run on a GitHub-hosted Ubuntu runner that
  [copilot-setup-steps](./workflows/copilot-setup-steps.yml) prepares. It
  initializes `openvmm/`; installs `requirements-dev.txt` into a Python virtual
  environment on `PATH`, the Rust release that `rust-toolchain.toml` pins with
  the OpenVMM test targets and the `x86_64-unknown-linux-musl` target used by
  the Rust guest binaries, the minimum supported Rust version of the `nvx`
  crate, cargo-nextest, and the remaining
  native guest build prerequisites; grants `/dev/kvm` access; pulls the pinned shell linter images;
  and stages the pinned Linux and Ubuntu Base archives in `.cache/downloads`.
  When CI has cached them for the checked-out inputs, it also restores the guest
  artifacts into `build/` and the KVM OpenVMM binary into
  `openvmm/target/release/openvmm`. `NVX_COPILOT_SETUP` is `complete` when every
  setup step succeeded; otherwise it is `incomplete` and `NVX_COPILOT_SETUP_FAILED`
  lists the failed step IDs from that workflow.
- The session ends 59 minutes after it starts, setup included, on 4 vCPUs. Run the
  narrowest affected check first. Rebuild OpenVMM, which takes about 5 minutes and
  4 GB, only when the `openvmm` gitlink changed or the cached binary is missing,
  and guest artifacts only when their inputs changed. Leave `test-openvmm-unit`,
  `test-openvmm`, `build-guest --guest all`, `verify-guest-determinism`, and
  `benchmark` to CI.
- `nvx-rs/target/release/nvx-rs run` and `nvx-rs/target/release/nvx-rs sandbox`
  run microVMs and sandboxes. Build `nvx-rs` there, in about a minute, with
  `cargo build --release --locked --features cli --manifest-path nvx-rs/Cargo.toml`,
  and again after changing `nvx-rs/`. The microVM scenarios run it too, and build
  it there the first time that they need it, unless `NVX_RS` names another build
  (see [Usage](../doc/usage.md#nvx-rs)). The `managed-lifecycle` scenario runs an
  `nvx-rs` test with Cargo. In a checkout that holds no artifacts, `run` and
  `sandbox` first download those of the release of `HEAD` or of its nearest
  released ancestor, as `nvx-rs download` does; build the artifacts that a change
  affects instead of testing a release's.
- Build trees are large. Check `df -h .` before a build, keep one OpenVMM build
  profile at a time, and delete trees you no longer need with `cargo clean` or
  `docker builder prune -af`. The setup disables incremental compilation and
  limits Cargo debug info to line tables to save space.
- The agent firewall allows GitHub, crates.io, rustup, PyPI, npm, Docker Hub, MCR,
  and the Debian, Ubuntu, and Alpine package archives. It blocks other hosts, such
  as `cdn.kernel.org` and `cdimage.ubuntu.com`, even inside containers, so
  Docker-based `build-guest` cannot download Linux. Rebuild guest artifacts
  natively from the staged archives with
  `python3 scripts/nvx.py build-guest --native`, which takes about 5 minutes,
  adding `--guest ubuntu` for the Ubuntu guest. Do not retry blocked downloads;
  rely on CI for inputs that remain blocked.
- With KVM access, run affected microVM scenarios with
  `python3 scripts/nvx.py test-microvm --backend kvm --scenario NAME`, passing only
  the `--processors` counts the change needs.
  MSHV and WHP are never available there. The setup summary names the runner's
  CPU. Built-in CPU profiles serve the runner CPUs met so far, the AMD EPYC
  7763, 9V74, and 9V45 and the Intel Xeon 8573C, among the generations that
  `doc/usage.md` lists. On a CPU that no built-in profile serves, such as the
  Xeon 6973P-C (Granite Rapids), `--cpu-profile auto` falls back to a host CPU
  profile and OpenVMM warns with `NVX-CPU-PROFILE-FALLBACK:` at every cold
  boot, so the microVM scenarios run there on that profile. `doctor` H2 and H3
  still fail there, and `benchmark` refuses that CPU, so leave qualification
  and benchmarks to CI.

## Pull Requests And CI

- Ask whether a pull request should be Draft or Ready before creating it. Draft
  pull requests skip CI jobs; opening or marking a pull request ready starts the
  matrix. Converting an active pull request to draft does not cancel its existing
  run. Prefer Draft for workload-affecting changes until local deterministic gates
  pass.
- Pull-request concurrency cancels superseded runs, while `dev` pushes queue in
  order and never cancel one another. Before debugging a cancelled pull-request
  run, compare its head SHA with the current pull-request head and classify an
  expected supersession separately from a failure. Use `nvx-ci-investigation` for
  failed or unexpectedly cancelled runs.
- `dev` requires GitHub's merge queue. Merge a pull request by queueing it with
  `gh pr merge NUMBER`, which queues it once it can merge.
  `gh pr merge NUMBER --merge --admin` skips the queue and rebuilds every queued
  group, so use it only when the queue itself is broken, and remove a queued
  pull request from the queue first, since `gh pr merge` does nothing for one.
  A merge group's run that `Cancel superseded merge group runs` cancelled
  belongs to a group that the queue replaced or that its pull request left, so
  check the pull request's queue outcome before treating it as a failure; see
  [Merge queue](../doc/ci.md#merge-queue).
- Pull-request runs must not publish development releases or persist performance
  history. Successful `dev` pushes own those operations. Do not manually edit
  `data/*.csv` unless the task explicitly concerns baseline maintenance, and change
  an expected benchmark host, `data/*-host.json`, only to accept a new host
  together with a reset of its series' history.
- Full runs of `dev`, scheduled or dispatched, publish the `nvx` crate to
  crates.io when `nvx-rs/` changed; see
  [crates.io releases](../doc/ci.md#cratesio-releases). Do not publish it by
  hand, and change the version that `nvx-rs/Cargo.toml` declares only to start
  another series, as [The nvx crate](../doc/distribution.md#the-nvx-crate)
  describes.
