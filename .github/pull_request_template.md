<!--
Title: `area: summary`, or the commit subject of a single-commit pull request
(doc/contribute.md, "Commits and pull requests"). Open a draft until the gates
that cover the change pass (doc/contribute.md, "Draft or Ready").

First line: `Fixes #N` or `Closes #N` when this completes the issue, and
`Part of #N` or `Refs #N` otherwise.

Redact credentials, tokens, host names, and other sensitive content from the
commands and logs that you paste. Never disclose an unfixed vulnerability in a
pull request; report it privately, as SECURITY.md requires.
-->

## What does this pull request change?

<!--
The behavior before and after the change, the components and interfaces that
it touches, and where a reviewer should start. Name what it leaves out and the
limitations that remain.
-->

## Why is this change needed?

<!--
The problem that it solves and its practical impact. For a fix, the root cause
and, when you know it, the change that introduced it. The alternatives that you
rejected, such as a patch that the issue proposes, and why.
-->

## How was this change validated?

<!--
Each check that you ran, with its command and its result, as text rather than
screenshots; `python3 scripts/local_gates.py --base origin/dev --run` runs the
gates that the change selects. For a runtime change, the backend (KVM, MSHV,
or WHP), host OS, host type (bare metal or virtual machine), and guest of each
run. The CI runs that you link, and the gates that you leave to CI, such as
backends that you could not reach.

Checks and results:
Backend, host OS, host type, and guest:
Left to CI:
-->

## Checklist

<!-- Check each item that holds or does not apply. -->

- [ ] Tests cover the new or fixed behavior, in the layout that
      `python3 scripts/check_test_layout.py` checks.
- [ ] `python3 scripts/check_test_changes.py --base origin/dev` lists no change
      to an existing test, or a **Changes to existing tests** section below
      explains each one.
- [ ] The documentation that the change affects is updated, including
      `.github/copilot-instructions.md` and the skills when a convention or
      workflow changes.
- [ ] A change to the public API of `nvx` updates `nvx/public-api/` with
      `python3 scripts/public_api.py --update`.
- [ ] Snapshots, protocols, and on-disk formats stay compatible, or the
      description says how the change versions or migrates them.

<!--
Add the sections that apply here.

### Changes to existing tests

Each change that check_test_changes.py lists, and why the test still checks
what it did, or why the behavior changed on purpose. The test-changes workflow
adds the `tests-modified` label.

### OpenVMM pin

When the `openvmm` gitlink moves: the OpenVMM pull request and the exact
pinned SHA, the scope and prerequisites, the equivalence evidence, the roadmap
positions, and the landing order: the OpenVMM pull request lands first, by
fast-forwarding `feature/microvm/dev` to the pinned revision.

A first-time contributor signs the Microsoft Contributor License Agreement
when the `license/cla` check asks.
-->
