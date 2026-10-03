---
name: code-documentation
description: Correct one verifiable statement in NVX documentation
intent: Keep NVX documentation trustworthy by correcting one novel inaccurate, ambiguous, or missing statement that current NVX code, including the aci_edge_sandboxes crate, or commands verify, without repeating rejected work.
on:
  schedule: hourly
  workflow_dispatch:
  skip-if-match:
    query: 'is:pr is:open "gh-aw-workflow-id: code-documentation" in:body'
    max: 1
if: github.event_name != 'workflow_dispatch' || github.ref_name == 'dev'
permissions:
  contents: read
  issues: read
  pull-requests: read
  copilot-requests: write
strict: true
engine:
  id: copilot
  version: "1.0.86"
model: gpt-5.6-sol-fast
max-turns: 100
timeout-minutes: 60
concurrency: code-documentation
sandbox:
  agent:
    id: awf
    model-fallback: false
    token-steering: false
imports:
  - uses: shared/code-improvement.md
    with:
      workflow-id: code-documentation
evals:
  questions:
    - id: operational_value
      question: If the agent requested a pull request, does its output demonstrate that one draft pull request corrects one inaccurate, ambiguous, or missing documentation statement? Answer UNKNOWN if the agent called noop.
    - id: single_scope
      question: Does the agent output limit the run to at most one code-documentation candidate?
    - id: verified_statement
      question: If the agent requested a pull request, does its output name the current NVX source line or command output that verifies the corrected statement? Answer UNKNOWN if the agent called noop.
    - id: bounded_patch
      question: If the agent requested a pull request, does its output report that the final patch changes fewer than 100 total lines? Answer UNKNOWN if the agent called noop.
    - id: validation_passed
      question: If the agent requested a pull request, does its output report that every applicable post-edit validation command passed? Answer UNKNOWN if the agent called noop.
    - id: rejection_aware
      question: If the agent requested a pull request, does its output report that the change does not repeat an open, merged, or rejected pull request? Answer UNKNOWN if the agent called noop.
    - id: justified_noop
      question: If the agent called noop, does its output state the checked scope and a concrete reason that no candidate qualified? Answer UNKNOWN if the agent requested a pull request.
  model: small
---

# Code Documentation

Correct one specific inaccurate, ambiguous, or missing statement that can be
verified against current NVX code or commands.

Target Markdown documentation in `doc/`, a comment or docstring in allowlisted
tooling that misstates the code it describes, or a Rust documentation comment
(`//!` or `///`) in `aci_edge_sandboxes/` that misstates the crate code it
describes. A qualifying candidate is one statement, table row, or short passage
that contradicts, is ambiguous about, or omits a fact established by current
NVX sources, such as a CLI option, subcommand, default, path, or prerequisite
defined in `scripts/nvx.py` or `scripts/nvx_tools/`, a CI step defined under
`.github/`, or, for the crate, a public item, default, error code, or
environment variable defined in `aci_edge_sandboxes/src/` or a Cargo feature
defined in its `Cargo.toml`. Leave every `README.md` alone, including the
crate's: gh-aw protects that file name, so the pull request allowlist excludes
it.

Verify the current behavior before editing: cite the defining source line, or
run a read-only command such as `python scripts/nvx.py <command> --help` and
compare its output. Then change the documentation to match the code. Never
change executable code, CLI help text, or behavior to match the documentation;
in code files, edit only comments and docstrings, which in Rust means `//`,
`///`, and `//!` comments.

Skip `doc/design.md` and `doc/design/**` unless NVX files outside `openvmm/`
verify the statement entirely; the `documentation-updater` workflow owns design
content derived from the pinned OpenVMM implementation. Reject stylistic
rewording, typo or grammar sweeps, statements about future plans, and
hardware-dependent claims that cannot be verified here.

In the pull request body, quote the previous statement or, for an omission,
name the section that now documents the missing fact; name the source of truth
that verifies the correction; and list every command, path, and link you
checked.
