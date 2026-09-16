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
  `data/*.csv` unless the task explicitly concerns baseline maintenance.
