<!--
Title: `area: summary`, or the commit subject of a single-commit pull request
(doc/contribute.md, "Commits and pull requests"). Open a draft until the gates
that cover the change pass (doc/contribute.md, "Draft or Ready").

First line: `Fixes #N` or `Closes #N` when this completes the issue, and
`Part of #N` or `Refs #N` otherwise.
-->

## What and why

<!-- What changed and why; the root cause of a fix. -->

## Validation

<!--
The checks that you ran and their results (`python3 scripts/local_gates.py
--base origin/dev --run` selects and runs them), the backends and hosts that you
tested on, and the gates that you leave to CI.
-->

<!--
When `python3 scripts/check_test_changes.py --base origin/dev` lists changes to
existing tests, add the `tests-modified` label and explain each one here.

When the `openvmm` gitlink moves, name the OpenVMM pull request and the exact
pinned SHA, the scope and prerequisites, the equivalence evidence, the roadmap
positions, and the landing order: the OpenVMM pull request lands first, by
fast-forwarding `feature/microvm/dev` to the pinned revision.
-->
