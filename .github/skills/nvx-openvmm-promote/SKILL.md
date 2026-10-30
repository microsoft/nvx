---
name: nvx-openvmm-promote
description: "Promote an OpenVMM pull request or revision into NVX as a clean gitlink update and map it in the OpenVMM upstream roadmap. Use for OpenVMM pin updates, clean replacement PRs, source-equivalence checks, signed integration commits, dependency validation, OpenVMM roadmap updates, and exact-head NVX CI validation."
argument-hint: "<OpenVMM PR URL or revision> [target=origin/dev] [Draft|Ready]"
---

# Promote An OpenVMM Revision

Move one reviewed OpenVMM revision into NVX without mixing the source repository's
history into the superproject, and map its commits in the OpenVMM upstream roadmap.
Preserve exact provenance and validate only claims supported by the selected source and
current NVX head.

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
two NVX commits directly on the current target: one that changes only the `openvmm`
gitlink, then one that maps the pinned commits in the OpenVMM upstream roadmap, as
[the next section](#4-map-the-pin-in-the-openvmm-roadmap) describes. Do not merge
OpenVMM commits or copy their history into NVX.

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

`nvx-rs` reads copies of what it needs from OpenVMM, because the `nvx-rs` CI job cannot
check out the submodule: the control-session protocol vectors, excerpts of OpenVMM's
sources, and, in `nvx::contract`, the versions of OpenVMM's snapshot format, saved-state
schema, time ABI, and restore packet. Compare them with the revision's, as the NVX CLI
jobs do:

```console
cargo test --locked --manifest-path nvx-rs/Cargo.toml --no-default-features --test openvmm_vectors -- --ignored
```

Each test stops at its first failure, which names a copy that the revision changes:

- A vector file differs: the revision changes the `NVXS` vectors that NVX's host and
  guest must follow. Copy the file unchanged into `nvx-rs/tests/vectors/openvmm/`.
- An excerpt is no longer a run of whole lines of its OpenVMM file: copy the new lines
  unchanged into it.
- A contract version differs: change the version in `nvx::contract`, and the code that
  reads what it versions.

After copying a vector file or an excerpt, record the revision in
[the vectors' README](../../../nvx-rs/tests/vectors/README.md#openvmms-vectors). Rerun the
comparison after each fix until it passes, since only the NVX CLI jobs run these ignored
tests. Then run the checks of [check-nvx-rs](../../actions/check-nvx-rs/action.yml), which
also build the runtime adapter that compiles the excerpts; they may need coordinated
changes too.

`nvx-rs run` also reads a copy of the identities of OpenVMM's built-in CPU profiles, from
which it guides a failed cold boot on a CPU that none serves. Compare it with the revision's
profiles too:

```console
cargo test --locked --manifest-path nvx-rs/Cargo.toml --no-default-features --test openvmm_cpu_profiles -- --ignored
```

A failure means that the revision adds, revises, or removes a built-in profile. Update
`nvx-rs/src/host/cli/cpu_generations.json` and the copy of `scripts/nvx_tools/time_abi.py`,
which `scripts/test_time_abi.py` compares with the same profiles, as coordinated changes,
and rerun the comparison until it passes.

Before committing the gitlink, require `git diff --name-only` to contain exactly
`openvmm` unless coordinated NVX changes were explicitly requested, or the revision needs
them, as a change to what `nvx-rs` copies from OpenVMM or to its CPU profiles does. Stage
only the gitlink and those changes, and name the coordinated changes in the pull request.
Commit only when the user requested it. Follow the repository's subject convention,
sign each commit with the user's existing configuration, and stop rather than falling
back to an unsigned commit if signing fails. Never amend, force-push, or rewrite an
already published integration commit without explicit authorization.

After a requested commit, verify that it is the only commit on the target so far and
that the target is its sole parent, then verify its signature:

```console
git diff --name-only <target-commit>..HEAD
git rev-list --count <target-commit>..HEAD
git rev-list --parents --max-count=1 HEAD
git log --show-signature --format=fuller -1
```

Require the count to be exactly `1` and the parent line to contain exactly the `HEAD`
and `<target-commit>` object IDs. Reject a different parent or any additional parent.

## 4. Map The Pin In The OpenVMM Roadmap

The [OpenVMM upstream roadmap](../../../doc/openvmm-upstream-roadmap.md) numbers the
commits of the microVM chain by position, from 001 on the fork base, and records NVX's
pin. Record the promotion there in a second commit, directly on the gitlink commit, that
changes only the roadmap, with a subject such as
`doc: map the <topic> in the OpenVMM roadmap`. Its message names the new positions and
the new pin, says what the commits change, and gives the pin's new distance from the
mapped tip.

Take the fork base and the mapped tip from the roadmap's Scope, and the previous pin from
`git rev-parse <target-commit>:openvmm`, which must match the Scope's pin. Derive every
number from Git rather than by counting, and from complete history: a shallow boundary,
such as the `--depth 1` initialization of the
[Copilot setup](../../workflows/copilot-setup-steps.yml) leaves, cuts counts and logs
short without an error. When `git -C openvmm rev-parse --is-shallow-repository` prints
`true`, run `git -C openvmm fetch --unshallow <remote>`, from a remote that resolves to
`microsoft/openvmm`, and require the check to print `false` before going on:

```console
git -C openvmm merge-base --is-ancestor <previous-pin> <revision>
git -C openvmm rev-list --count --merges <fork-base>..<revision>
git -C openvmm rev-list --count <fork-base>..<revision>
git -C openvmm rev-list --count <mapped-tip>..<revision>
git -C openvmm log --reverse --abbrev=9 --format='%h %s' <previous-pin>..<revision>
```

Require the first command to succeed and the second to print `0`; otherwise stop and
ask. When the first fails, list the pinned commits that the revision lacks:

```console
git -C openvmm log --oneline <revision>..<previous-pin>
```

If they are only the latest commits of the chain, the revision's pull request branched
from an older head of `feature/microvm/dev`: the roadmap stays valid, and the pull request
needs a rebase onto the pin and new OpenVMM CI before its promotion. If they are every
commit of the chain, the chain itself was rebased, as onto a new upstream base: positions
shift, and the whole roadmap needs remapping, as the `feature/microvm/release/v2.3`
rebase did.

The third command prints the pin's position, the fourth its distance from the mapped tip,
and the fifth lists the new positions in order. Any commit's position is
`git -C openvmm rev-list --count <fork-base>..<commit>`.

Record the new commits as unassigned. An unassigned range grows only when the new
positions directly follow it; after a fixup step, start a new range. Update these parts,
following their wording:

- **Scope**: the pin's full SHA and position; the chain back from it, naming the OpenVMM
  pull request that proposes or merged each group of positions and the current head of
  `feature/microvm/dev`; the pin's distance from the mapped tip; and the unassigned
  ranges. The previous pin's pull request has usually merged since, so check each state
  with `gh pr view <number> --repo microsoft/openvmm --json state,mergeCommit`.
- **Actual branch DAG**: the final step, extended when it is unassigned and otherwise
  followed by a new `unassigned` step, and the 9-character SHA of the `(NVX pin)` line.
- **Commits after the mapped tip**: the count and the ranges of each kind in its opening
  paragraph, and one row per topic, in chain order. A row gives inclusive positions, the
  9-character SHAs of the first and last commit, the count, and what the commits change,
  ending with the OpenVMM pull request.
- **The remarks after that table**: which earlier chain commits change the same files,
  and, separately, whether the new commits apply to upstream main as they are, so they
  can go upstream on their own. Sharing a file decides neither: only a commit that changes
  what an earlier one added revisits that commit's PR.

For the remarks, list the files that the new commits change and the earlier chain commits
that change them too, then check whether the new commits apply to upstream main, which the
Scope records:

```console
git -C openvmm diff --name-only <previous-pin> <revision>
git -C openvmm log --reverse --abbrev=9 --format='%h %s' <fork-base>..<previous-pin> -- <file>...
git -C openvmm merge-tree --write-tree --name-only --merge-base <previous-pin> <upstream-main> <revision>
```

The last command replays the new commits on upstream main without touching a working tree.
It exits with `1` when they conflict, listing the conflicting files, as for positions 366
and 367, which change fork-only CPU checks, and with `0` when they apply cleanly, as
positions 368 and 369 do although positions 043 and 301 change other parts of their file.
A clean merge shows only that the commits apply, not that upstream has the code that they
use. Before the remarks say that they can go upstream on their own, type-check their
packages on the merged tree, whose ID is the first line of that output, in a temporary
worktree. Give it an absolute `<directory>` outside both checkouts, since `git -C`
resolves a relative one from another directory, and return to the NVX root before
removing it:

```console
git -C openvmm worktree add --detach <directory> <upstream-main>
git -C <directory> read-tree -u --reset <tree>
cd <directory>
cargo check -p <package>
cd <nvx-root>
git -C openvmm worktree remove --force <directory>
```

Without that check, say only that the commits apply to upstream main without conflicts.

A new commit that fixes a commit that a roadmap PR maps is instead a fixup of that PR, to
fold into the commit that it fixes. End its row with `fold into <position>`, give it its
own `PRnn fixup` step in the DAG, and also update the fixup positions that the Scope and
the commit mapping's introduction list, the fold sentences after the DAG, the PR's row in
the PR summary, its node's count in the dependency graph, and its list and count in the
commit mapping.

Before committing, confirm that the Scope's pin is `git rev-parse HEAD:openvmm`, then run
`git diff --check` and the link check with the platform launcher,
`python3 scripts/check_markdown_links.py` on Linux or
`python scripts\check_markdown_links.py` on Windows. After a requested commit,
verify that the branch holds exactly the two commits, that the gitlink commit is the
roadmap commit's sole parent, that the roadmap commit changes only the roadmap, and its
signature:

```console
git rev-list --count <target-commit>..HEAD
git rev-list --parents --max-count=1 HEAD
git diff --name-only HEAD~1 HEAD
git log --show-signature --format=fuller -1
```

Require the count to be exactly `2`, the parent line to contain exactly the `HEAD` and
gitlink-commit object IDs, and the file list to be exactly
`doc/openvmm-upstream-roadmap.md`.

## 5. Validate The Exact Pin

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

## 6. Create And Validate The Pull Request

Immediately before pushing, refresh the target and confirm the gitlink commit is still
directly based on it. If the target moved, rebuild both commits on the new target,
deriving the roadmap's numbers again, and rerun validation; do not present stale
evidence as current.

The pull-request body should identify:

- OpenVMM pull request and exact pinned revision.
- Scope and explicit exclusions.
- Required dependencies and their states.
- Tree, patch, or behavioral equivalence evidence using the precise verified term.
- The roadmap positions of the pinned commits.
- Local validation and unavailable platform gates.
- The exact NVX head SHA submitted to CI.
- The landing order: the OpenVMM pull request lands, as described below, before the NVX
  pull request merges.

Ask for Draft or Ready state before creation. Ready pull requests start the complete
[NVX matrix](../../workflows/ci.yml); changing an already active pull request to Draft
does not cancel its current run. After each push, follow only the workflow run whose
head SHA matches the current pull-request head. Treat older runs as superseded and use
`nvx-ci-investigation` for a failed or unexpectedly cancelled current-head run.

`.gitmodules` tracks `feature/microvm/dev`, so the pinned revision must be on that branch
before the NVX pull request merges. `microsoft/openvmm` allows only squash merges, which
would land a different commit, so land the OpenVMM pull request by fast-forwarding the
branch to the pinned revision instead, from a remote that resolves to `microsoft/openvmm`,
and only when the user asks:

```console
git -C openvmm push <remote> <revision>:refs/heads/feature/microvm/dev
```

Never force it. If the branch has moved past the pull request's base, the pull request
needs a rebase, new OpenVMM CI, and a new promotion.

## Report

Report the source PR and revision, signature result, dependency state, equivalence
method and result, NVX target commit, both commits and their changed-file sets, the
roadmap positions and the pin's distance from the mapped tip, local validation, the
comparisons of what `nvx-rs` copies from OpenVMM and of its CPU profiles, pull-request
state and URL, exact-head workflow URL, the OpenVMM pull request's landing state, and
every blocked or pending gate.
