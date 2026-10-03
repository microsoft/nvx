---
name: code-reusability
description: Reuse one existing NVX tooling or aci_edge_sandboxes helper at a second call site
intent: Reduce NVX maintenance cost by reusing one existing internal helper in the tooling or the aci_edge_sandboxes crate at a second proven call site without changing public behavior or repeating rejected work.
on:
  schedule: hourly
  workflow_dispatch:
  skip-if-match:
    query: 'is:pr is:open "gh-aw-workflow-id: code-reusability" in:body'
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
concurrency: code-reusability
sandbox:
  agent:
    id: awf
    model-fallback: false
    token-steering: false
imports:
  - uses: shared/code-improvement.md
    with:
      workflow-id: code-reusability
evals:
  questions:
    - id: operational_value
      question: If the agent requested a pull request, does its output demonstrate that one draft pull request makes one existing internal helper serve a second call site? Answer UNKNOWN if the agent called noop.
    - id: single_scope
      question: Does the agent output limit the run to at most one code-reusability candidate?
    - id: second_call_site
      question: If the agent requested a pull request, does its output name both the existing helper and the second call site that now uses it? Answer UNKNOWN if the agent called noop.
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

# Code Reusability

Make one existing internal helper reusable at a second proven call site without
changing public behavior.

A qualifying candidate pairs an existing helper that already has at least one
caller with a second call site that reimplements the same behavior inline: a
Python helper in `scripts/nvx.py` or `scripts/nvx_tools/`, a function in one
allowlisted shell script whose second call site is in the same script, or a
function in `aci_edge_sandboxes/src/`. Prove that the helper's results, errors,
and messages match the inline code for every input the second site can pass,
citing both definitions.

Replace the inline logic with a call to the helper. When a Python helper needs
a change to serve the second site, keep it minimal and backward compatible: a
keyword parameter whose default preserves every existing caller, or a move to
an existing shared module such as `scripts/nvx_tools/common.py` when the second
site is in another module. A crate helper may change only by widening a private
`fn` to `pub(crate)` for a second module; a new parameter or generic would
touch every caller, so reject that candidate. Do not add a new helper, module,
class, trait, macro, public entry point, or cross-file shell sourcing, and do
not change existing callers' behavior.

Preserve the second site's user-visible behavior, including CLI output, error
codes and messages, and exit codes. For Python, cover it with an existing or
new focused test and keep the helper's existing tests passing; for the crate,
do the same with a unit or integration test that runs without a hypervisor.

Reject cosmetic call-site rewrites, reuse that needs behavior-changing
adaptation, and speculative reuse at a call site that does not exist yet.
Consolidating two or more inline copies into a new helper belongs to
`code-deduplication`; do not select it here.

In the pull request body, name the helper, its existing callers, the second
call site, and the evidence that behavior is unchanged.
