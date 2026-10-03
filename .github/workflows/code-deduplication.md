---
name: code-deduplication
description: Remove one small instance of repeated NVX tooling or aci_edge_sandboxes logic
intent: Reduce NVX maintenance cost by removing one novel, validated instance of repeated internal logic in the tooling or the aci_edge_sandboxes crate, without new public abstractions or repeated rejected work.
on:
  schedule: hourly
  workflow_dispatch:
  skip-if-match:
    query: 'is:pr is:open "gh-aw-workflow-id: code-deduplication" in:body'
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
concurrency: code-deduplication
sandbox:
  agent:
    id: awf
    model-fallback: false
    token-steering: false
imports:
  - uses: shared/code-improvement.md
    with:
      workflow-id: code-deduplication
evals:
  questions:
    - id: operational_value
      question: If the agent requested a pull request, does its output demonstrate that one draft pull request removes one instance of repeated internal logic? Answer UNKNOWN if the agent called noop.
    - id: single_scope
      question: Does the agent output limit the run to at most one code-deduplication candidate?
    - id: consolidated_copies
      question: If the agent requested a pull request, does its output identify each copy of the repeated logic that the change consolidates? Answer UNKNOWN if the agent called noop.
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

# Code Deduplication

Remove one small instance of repeated internal logic without creating a new
public abstraction.

A qualifying candidate has at least two inline copies of the same non-trivial
logic in `scripts/nvx.py`, `scripts/nvx_tools/`, one allowlisted shell
script, or the sources of `aci_edge_sandboxes/src/`: the same computation,
validation, parsing, or command sequence with the same inputs, results, and
errors, not merely code with a similar shape. Consolidate every copy into one
helper:

- keep the helper private to the module that owns the concern or, when the
  copies span modules, place it beside related helpers in an existing shared
  module such as `scripts/nvx_tools/common.py`; in the crate, use a private
  `fn` in the module that owns the concern or, when the copies span modules, a
  `pub(crate)` `fn` in an existing module that every copy's module already
  uses;
- do not add modules, classes, traits, macros, generics, protocols, registries,
  configuration, CLI surface, public items, Cargo features, or parameters that
  the existing copies do not need;
- in shell, use a function within the same script, and never add shared shell
  files or cross-file sourcing.

Preserve every call site's behavior exactly, including results, exception
types or error codes, and messages. Prove equivalence with the existing tests
that cover each call site, add a focused test only where a copy lacks coverage,
and update mock targets that move with the logic. Excluding tests, the patch
should delete more lines than it adds.

Reject near-duplicates whose differences matter, test-only deduplication, and
consolidation that needs flags or branches to preserve divergent behavior. When
one copy is already a helper and another site only needs to call it, the
candidate belongs to `code-reusability`; do not select it here.

In the pull request body, list each consolidated copy, the resulting helper,
and the evidence that behavior is unchanged.
