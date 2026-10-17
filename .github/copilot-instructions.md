# NVX Repository Instructions

## Repository Boundaries

- The integration repository is `microsoft/nvx`, and its default branch is `dev`.
  The `nanvix/nvx` repository is archived; do not create new branches or pull
  requests there. Verify configured remotes before pushing.
- `openvmm/` is a Git submodule sourced from `nanvix/openvmm`. For OpenVMM source
  changes, work in the nested repository and follow
  [its instructions](../openvmm/.github/copilot-instructions.md). For an NVX pin
  promotion, use the `nvx-openvmm-promote` skill and keep the superproject change
  gitlink-only unless the task explicitly requires coordinated NVX changes.
- Preserve dirty state in both repositories. Do not reset, clean, switch, update,
  or synchronize the submodule over uncommitted work.

## Validation

- Run `python3 scripts/nvx.py verify` on Linux or `python scripts\nvx.py verify`
  on Windows before build, run, debug, or benchmark work.
- Treat [check-quality](./actions/check-quality/action.yml) and
  [validate-nvx](./actions/validate-nvx/action.yml) as the authoritative local
  quality and CLI test definitions. Start with the narrowest affected test, then
  run the applicable commands from those actions.
- For changes under `aci_edge_sandboxes/`, run the commands in
  [check-aci-edge-sandboxes](./actions/check-aci-edge-sandboxes/action.yml), and for
  changes under `nvx-rs/`, those in [check-nvx-rs](./actions/check-nvx-rs/action.yml)
  and, because `aci_edge_sandboxes` builds on `nvx-rs`, check-aci-edge-sandboxes too.
  They need no hypervisor. `scripts/nvx.py test-aci-edge-sandboxes` exercises both
  crates on a real hypervisor.
- OpenVMM source changes also require the package-scoped checks prescribed by the
  nested repository. Do not claim unavailable hardware or cross-platform gates
  passed; report the missing prerequisite and rely on CI when appropriate.

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
  the Rust guest binaries, the minimum supported Rust versions of the
  `aci_edge_sandboxes` and `nvx-rs` crates, cargo-nextest, and the remaining
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
  the `--processors` counts the change needs, and `aci_edge_sandboxes` backend
  or `nvx-rs` changes with `python3 scripts/nvx.py test-aci-edge-sandboxes --backend kvm`.
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
- Pull-request concurrency cancels superseded runs. Before debugging a cancelled
  run, compare its head SHA with the current pull-request head and classify an
  expected supersession separately from a failure. Use `nvx-ci-investigation` for
  failed or unexpectedly cancelled runs.
- Pull-request runs must not publish development releases or persist performance
  history. Successful `dev` pushes own those operations. Do not manually edit
  `data/*.csv` unless the task explicitly concerns baseline maintenance, and change
  an expected benchmark host, `data/*-host.json`, only to accept a new host
  together with a reset of its series' history.
