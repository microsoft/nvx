---
name: nvx-openvmm-promote
description: "Promote an OpenVMM pull request or revision into NVX as a clean gitlink update. Use for OpenVMM pin updates, clean replacement PRs, source-equivalence checks, signed integration commits, dependency validation, and exact-head NVX CI validation."
argument-hint: "<OpenVMM PR URL or revision> [target=origin/dev] [Draft|Ready]"
---

# Promote An OpenVMM Revision

Move one reviewed OpenVMM revision into NVX without mixing the source repository's
history into the superproject. Preserve exact provenance and validate only claims
supported by the selected source and current NVX head.

## 1. Establish Both Repository States

From the NVX root, record both repositories before fetching or switching anything:

```console
git status --short --branch
git remote -v
git submodule status -- openvmm
git -C openvmm status --short --branch
git -C openvmm remote -v
git -C openvmm rev-parse HEAD
```

Treat NVX and `openvmm/` as independent Git repositories. For OpenVMM source work,
follow the [nested repository instructions](../../../openvmm/.github/copilot-instructions.md).
Do not update, switch, reset, clean, or synchronize a dirty repository. If either
repository is dirty, preserve it and use an approved isolated worktree or stop and
ask how to proceed.

Confirm these inputs:

- OpenVMM pull request URL or exact revision to pin.
- NVX target remote and branch; use `origin/dev` only after verifying it resolves
  to `microsoft/nvx` and remains the integration target.
- Any prerequisite OpenVMM pull requests or revisions.
- Whether this replaces an earlier NVX or OpenVMM pull request.
- Draft or Ready state for the NVX pull request. Recommend Draft until local
  deterministic checks pass.

## 2. Resolve And Verify The Source

Use GitHub tooling to inspect the OpenVMM pull request when one was supplied. Record
its repository, number, base, current head SHA, state, review status, checks, stated
dependencies, and scope. Fetch only the required remote refs, then verify locally:

```console
git -C openvmm cat-file -e <revision>^{commit}
git -C openvmm show --no-patch --format=fuller <revision>
git -C openvmm verify-commit <revision>
git -C openvmm rev-parse <revision>^{tree}
```

Do not substitute a branch name for the recorded SHA after resolution. Stop if the
revision is unavailable, belongs to another repository, does not match the current
pull-request head, or omits a required dependency. Report an unverifiable signature
instead of silently treating it as verified.

When the promotion is a clean replacement, verify the stated relationship to the
reviewed revision:

- Claim byte-for-byte tree equivalence only when the two `^{tree}` object IDs are
  equal.
- For rebased or regrouped commits, use `git range-diff` and compare both complete
  diffs against their respective bases. Describe this as patch or behavioral
  equivalence, not tree identity.
- Inspect added, removed, and reordered commits. Never infer equivalence from equal
  titles, summaries, or changed-file lists.

## 3. Prepare A Clean NVX Integration Branch

Fetch the target branch narrowly and record its exact commit. A normal promotion is
one NVX commit directly on the current target and changes only the `openvmm` gitlink.
Do not merge OpenVMM commits or copy their history into NVX.

If replacing a stale integration branch, create a fresh branch or isolated worktree
from the refreshed target rather than importing merge or rebase history. With both
repositories clean, detach the submodule at the verified revision and inspect the
superproject change:

```console
git -C openvmm switch --detach <revision>
git diff --submodule=log -- openvmm
git diff --name-only
git diff --check
```

Before committing, require `git diff --name-only` to contain exactly `openvmm`
unless coordinated NVX changes were explicitly requested. Stage only the gitlink.
Commit only when the user requested it. Follow the repository's subject convention,
sign the commit with the user's existing configuration, and stop rather than falling
back to an unsigned commit if signing fails. Never amend, force-push, or rewrite an
already published integration commit without explicit authorization.

After a requested commit, verify its shape and signature:

```console
git diff --name-only <target-commit>..HEAD
git log --show-signature --format=fuller -1
git merge-base --is-ancestor <target-commit> HEAD
```

## 4. Validate The Exact Pin

Run NVX verification with the platform launcher:

```bash
python3 scripts/nvx.py verify
```

```powershell
python scripts\nvx.py verify
```

Confirm that upstream OpenVMM validation belongs to the exact pinned SHA. For source
changes made in the current task, run the package-scoped check, clippy, documentation,
unit-test, and formatting gates required by the nested instructions before relying on
NVX CI. For a pin-only promotion of an already reviewed source, record existing exact-
head OpenVMM evidence and do not claim unexecuted local checks.

Choose additional NVX runtime checks from the changed behavior and its dependencies.
Use `nvx-run` for a smoke test, `nvx-debug` for a failure, and `nvx-benchmark` only
when performance evidence is required. Report unavailable KVM, MSHV, or WHP coverage
as pending CI rather than substituting another backend.

## 5. Create And Validate The Pull Request

Immediately before pushing, refresh the target and confirm the integration commit is
still directly based on it. If the target moved, rebuild the clean integration commit
on the new target and rerun validation; do not present stale evidence as current.

The pull-request body should identify:

- OpenVMM pull request and exact pinned revision.
- Scope and explicit exclusions.
- Required dependencies and their states.
- Tree, patch, or behavioral equivalence evidence using the precise verified term.
- Local validation and unavailable platform gates.
- The exact NVX head SHA submitted to CI.

Ask for Draft or Ready state before creation. Ready pull requests start the complete
[NVX matrix](../../workflows/ci.yml); changing an already active pull request to Draft
does not cancel its current run. After each push, follow only the workflow run whose
head SHA matches the current pull-request head. Treat older runs as superseded and use
`nvx-ci-investigation` for a failed or unexpectedly cancelled current-head run.

## Report

Report the source PR and revision, signature result, dependency state, equivalence
method and result, NVX target commit, final changed-file set, local validation, pull-
request state and URL, exact-head workflow URL, and every blocked or pending gate.
