# Contributing

NVX takes contributions as pull requests to the `dev` branch of
[microsoft/nvx](https://github.com/microsoft/nvx). This guide covers the
workflow, the conventions that reviewers expect, the quality gates, test
ownership, and how OpenVMM changes reach NVX. Report security
vulnerabilities as [SECURITY.md](../SECURITY.md) describes, never in a public
issue.

## Workflow

1. Clone the repository with its OpenVMM submodule and run
   `scripts/nvx.py verify`, as [Setup](setup.md#initialize) describes.
2. Create a topic branch from `dev`.
3. Make the change with its tests and documentation, then run the
   [quality gates](#quality-gates) and [tests](#test-ownership) that cover it.
4. Open a pull request against `dev` as a [draft or ready for
   review](#draft-or-ready).
5. Address the review until the pull request meets the merge rules below.

Branch rules on `dev` require one approving review, every review thread
resolved, and a passing `Required status check`, the CI job that confirms
that every job the change schedules succeeded. Copilot reviews every push,
drafts included. Pull requests merge with a merge commit, so every commit on
the branch lands in `dev`; give each one a subject that follows the
[conventions](#commits-and-pull-requests).

CI runs the jobs that need the OpenVMM deploy key or a hypervisor runner,
including the NVX CLI tests, only for pushes and same-repository pull
requests. A pull request from a fork runs the remaining GitHub-hosted checks,
and a maintainer stages it on a `microsoft/nvx` branch to run the rest; see
[Job graph and runner capacity](ci.md#job-graph-and-runner-capacity).

## Commits and pull requests

| Item | Convention |
| --- | --- |
| Commit subject | `area: summary`, where `area` is the component, command, or directory that changes, such as `doc`, `ci`, `guest`, `sandbox`, or `openvmm`. Start the summary with a lowercase imperative verb and omit the trailing period: `ci: run each OpenVMM suite only when its inputs change`. |
| Commit body | What changed and why, when the subject does not say enough. |
| Pull request title | The commit subject of a single-commit pull request; otherwise a summary in the same form. |
| Pull request body | What changed and why, then a `Validation` section that lists the checks you ran and the gates you leave to CI, such as backends you could not test. |
| Issue references | `Closes #N` or `Fixes #N` first when the pull request completes the issue; `Part of #N` or `Refs #N` otherwise. |
| Review threads | Reply with the commit that addresses the comment, or with the evidence that no change is needed, then resolve the thread. |

Leave the performance histories in `data/*.csv` to CI: successful `dev`
pushes append to them.

## Draft or Ready

CI runs for a pull request to `dev` when it is opened, reopened, pushed to,
or marked ready for review, and skips its lint, build, and test jobs while the
pull request is a draft. A new push cancels the pull request's run in
progress; converting it back to a draft does not. A ready pull request that
changes more than documentation occupies the shared self-hosted KVM, MSHV,
and WHP runner pools, so:

- Open a **Draft** when the change touches more than documentation and has
  not yet passed the [quality gates](#quality-gates) and
  [tests](#test-ownership) that cover it. Mark it ready once it has.
- Open it **Ready** otherwise. A documentation-only change, in which every
  path is under `doc/` or ends in `.md`, skips the guest artifacts, the
  OpenVMM builds, and every hypervisor job; see
  [Change classification](ci.md#change-classification).

## Quality gates

CI runs these checks on GitHub-hosted runners. Run the ones that cover your
change before you push; [Setup](setup.md#development) lists the commands for
the Python and shell checks.

| Code | Checks | Definition |
| --- | --- | --- |
| Python in `scripts/` and `.github/specula/` | Ruff lint and format with 88-column lines, Pyright in strict mode over `scripts/` against Linux and Windows APIs, and the Specula wiring tests. The code targets Python 3.10. | [check-quality](../.github/actions/check-quality/action.yml), [pyproject.toml](../pyproject.toml) |
| Guest and setup shell scripts | ShellCheck, and shfmt with four-space indentation and indented `case` branches. Scripts are POSIX `sh` unless check-quality checks them as Bash. | [check-quality](../.github/actions/check-quality/action.yml) |
| `scripts/setup/setup-windows-whp.ps1` | PowerShell syntax | [check-quality](../.github/actions/check-quality/action.yml) |
| `aci_edge_sandboxes/` | rustfmt, Clippy and rustdoc with warnings denied, tests, and the minimum supported Rust version; see [Rust crate](ci.md#rust-crate) | [check-aci-edge-sandboxes](../.github/actions/check-aci-edge-sandboxes/action.yml) |
| NVX CLI | Compiles `scripts/`, runs every `scripts/test_*.py` module, and checks command help, on Linux and Windows | [validate-nvx](../.github/actions/validate-nvx/action.yml) |

check-quality names each shell script that it checks, and a CLI test fails
unless validate-nvx runs every `scripts/test_*.py` module on both operating
systems, so add a new script or test module to those lists.

## Test ownership

OpenVMM owns control-plane coverage. Run its unit and documentation tests with
`scripts/nvx.py test-openvmm-unit` and its VMM tests with
`scripts/nvx.py test-openvmm --backend BACKEND`. The VMM tests boot
OpenVMM's own test guests, except the management-RPC (TTRPC) lifecycle, SMP,
and snapshot test, which boots NVX's kernel and Alpine initramfs when
[`test-openvmm`](usage.md#test-openvmm) runs it.

NVX owns behavior that depends on its patched Linux kernel, its Alpine,
Ubuntu, and Azure Linux userspaces, guest helpers, SMP behavior, or virtio
devices. Add those scenarios to
`scripts/nvx_tools/microvm_tests.py` and run them with
`scripts/nvx.py test-microvm --backend BACKEND`. Keep guest workloads in
`scripts/nvx_tools/microvm_test_scripts` and retain complete failure logs.
The scenarios that drive the public `nvx.py sandbox` commands live in
`scripts/nvx_tools/managed_exec_tests.py` and
`scripts/nvx_tools/sandbox_lifecycle_tests.py`, and `microvm_tests.py`
registers them.

Unit tests for the host tooling live in `scripts/test_*.py`. The
`aci_edge_sandboxes` crate keeps unit tests alongside its Rust source and
integration tests in `aci_edge_sandboxes/tests`. The
`scripts/nvx.py test-aci-edge-sandboxes --backend BACKEND` command runs all
ignored `openvmm_e2e` tests on a real hypervisor. [Validation](design/validation.md)
describes what the OpenVMM VMM tests and the NVX microVM suite cover.

## OpenVMM

### Access

`openvmm/` is a Git submodule of the public
[nanvix/openvmm](https://github.com/nanvix/openvmm) repository.
`.gitmodules` records its URL and tracked `main` branch, and each NVX commit
pins one exact revision. Clone with `--recurse-submodules`, or run
`scripts/nvx.py init` in an existing checkout, as [Setup](setup.md#initialize)
describes. `scripts/nvx.py verify` fails until the submodule is checked out at
the pinned revision.

### Source changes

Change OpenVMM through a pull request to `nanvix/openvmm` that follows
[its contributor instructions](https://github.com/nanvix/openvmm/blob/main/.github/copilot-instructions.md),
including their Clippy, rustdoc, test, and formatting checks, then promote
the reviewed revision into NVX.

`nanvix/openvmm` is a fork of
[microsoft/openvmm](https://github.com/microsoft/openvmm). The
[OpenVMM upstream roadmap](openvmm-upstream-roadmap.md) groups the fork's
microVM commits into planned upstream pull requests and marks those that stay
fork-only.

### Pin promotion

A pin promotion moves the `openvmm` gitlink to a reviewed OpenVMM revision:

- Make the promotion a one-commit topic branch based directly on the current
  `dev`; that commit should change only the gitlink unless the revision needs
  coordinated NVX changes, with a subject such as `openvmm: pin auto's fallback
  to host CPU profiles`. Never merge OpenVMM history into NVX.
- After committing, run `scripts/nvx.py verify`, which checks that the
  submodule checkout matches the recorded pin.
- In the pull request, name the OpenVMM pull request and the exact pinned
  SHA, the scope and any prerequisite revisions, and the validation you ran.
- A gitlink change is an input of every CI suite, so its pull request runs
  the full matrix, including both OpenVMM suites.
- When a rebase conflicts on the gitlink, do not pick either side: combining
  both changes usually needs a new OpenVMM revision.

The [nvx-openvmm-promote](../.github/skills/nvx-openvmm-promote/SKILL.md)
skill gives the complete procedure, including how to show that a replacement
revision matches the reviewed one.
