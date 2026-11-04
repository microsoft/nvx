---
name: nvx-openvmm-rebase
description: "Rebase or reorganize the OpenVMM microVM chain onto the latest upstream main as a new feature/microvm/release branch, fast-forward the previous release branch to feature/microvm/dev before moving dev to the new release, and pin the release in NVX with a remapped roadmap. Use for microVM release rebases and reorganizations, commits that upstream landed, rebase conflicts and fixups, and the release landing order."
argument-hint: "[rebase|reorganization] [upstream=main] [Draft|Ready]"
---

# Rebase The OpenVMM MicroVM Chain

`feature/microvm/dev` of `microsoft/openvmm`, which `.gitmodules` tracks, carries the microVM
chain on a fork base taken from upstream `main`. Moving the whole chain onto a newer upstream
`main` publishes it as a new release branch, `feature/microvm/release/vX.Y`:

- A **rebase** bumps the minor version, as v2.3 and v2.4 did. It replays every commit, drops
  those whose changes upstream landed, resolves conflicts in place, and adds fixups at its tip for
  upstream changes that break the chain without a textual conflict.
- A **reorganization** bumps the major version, as v3.0 and v4.0 did. It also folds fixups into
  the commits that they fix and regroups commits, so that every commit builds on the new base, and
  it may change behavior, as v4.0's restart of each ABI at a single version did.

Every commit of the chain changes, so this is not the promotion of a revision on the current
chain, which [nvx-openvmm-promote](../nvx-openvmm-promote/SKILL.md) describes; this skill reuses
that skill's NVX commits and pull request. It rewrites shared branches of `microsoft/openvmm`, so
push only when the user asks.

## 1. Establish The State

Record both repositories before fetching or switching anything, as nvx-openvmm-promote does.
Preserve dirty state in both, and work on the chain in a separate worktree of `openvmm/`, never in
its checkout. Use a remote of `openvmm/` that resolves to `microsoft/openvmm`, and add one with
`git -C openvmm remote add <remote> https://github.com/microsoft/openvmm.git` when none does: some
checkouts name `nanvix/openvmm` `origin`. History must be complete: when
`git -C openvmm rev-parse --is-shallow-repository` prints `true`, run
`git -C openvmm fetch --unshallow <remote>`, and require the check to print `false` before going
on. Then fetch upstream `main`, `feature/microvm/dev`, and the release branches:

```console
git -C openvmm fetch <remote> main feature/microvm/dev "+refs/heads/feature/microvm/release/*:refs/remotes/<remote>/feature/microvm/release/*"
```

Record:

- `<old-head>`, the head of `feature/microvm/dev`.
- `<old-base>`, which `git -C openvmm merge-base <old-head> <remote>/main` prints and which must be
  the fork base in the Scope of the
  [OpenVMM upstream roadmap](../../../doc/openvmm-upstream-roadmap.md).
- `<prev-release>`, the newest release branch, such as `feature/microvm/release/v4.0`, whose
  head, `<prev-release-head>`, is an ancestor of `<old-head>`, as
  `git -C openvmm merge-base --is-ancestor <prev-release-head> <old-head>` shows.
- `<nvx-pin>`, `git rev-parse <target-commit>:openvmm` on NVX's target branch, which must be an
  ancestor of `<old-head>`.
- `<new-base>`, the head of upstream `main`, and its date.

Then settle the prerequisites:

- List the open pull requests into `feature/microvm/dev` with
  `gh pr list --repo microsoft/openvmm --base feature/microvm/dev --state open`. Also list open
  pull requests based on each listed pull request's head branch, recursively, by repeating the
  command with `--base <head-branch>`. Land each one in dependency order, as nvx-openvmm-promote
  describes, or agree with the user to carry its commits into the release. Record every layer
  left open for rebasing onto the new release once `feature/microvm/dev` moves.
- Merge or close NVX's open pin promotions. Once `feature/microvm/dev` moves, a revision of the old
  chain is no longer on the branch that `.gitmodules` tracks.
- Ask whether this is a rebase or a reorganization unless the user said so. Name the release after
  `<prev-release>`: its next minor version for a rebase, its next major version for a
  reorganization. Require
  `git -C openvmm ls-remote --exit-code <remote> refs/heads/feature/microvm/release/<version>` to
  find no such branch.

## 2. Choose What To Drop

Upstream lands chain commits through the pull requests that the roadmap's
[Open upstream pull requests](../../../doc/openvmm-upstream-roadmap.md#open-upstream-pull-requests)
and commit mapping name. Query the state of each, list upstream's commits since the fork base,
and list the chain commits whose patches upstream already has, which `git cherry` marks with `-`:

```console
gh pr view <number> --repo microsoft/openvmm --json state,mergeCommit
git -C openvmm log --oneline <old-base>..<new-base>
git -C openvmm cherry <new-base> <old-head> <old-base>
```

Drop each chain commit whose change upstream merged, even in a revised form, and adapt the later
commits to upstream's version, as the v2.4 rebase did for ten commits. Drop a commit that an
upstream change supersedes only once you have shown that upstream covers what it does: v4.0
dropped v3.0 position 016 because the tests of microsoft/openvmm#4835 cover its stop
cancellation. Record each drop's former position, fork commit, roadmap PR, upstream pull request,
and upstream commit for the roadmap's
[Landed upstream](../../../doc/openvmm-upstream-roadmap.md#landed-upstream) table.

## 3. Replay The Chain

Add a worktree outside both checkouts, with an absolute `<directory>`, on a local branch at
`<old-head>`, and replay the chain onto `<new-base>`. In the rebase's todo list, change the `pick`
of each commit that section 2 drops to `drop`:

```console
git -C openvmm worktree add -b microvm-release-<version> <directory> <old-head>
git -C <directory> rebase -i --onto <new-base> <old-base>
```

Keep commit signing on: never pass `--no-gpg-sign` or override `commit.gpgsign`, and stop if
signing fails. The rebase keeps each commit's author, author date, subject, and trailers. Drop no
other commit without the user's approval. When the rebase drops a commit because it became empty,
record that drop too: v2.3 dropped position 074 that way, since microsoft/openvmm#4661 had landed
its hunk.

Resolve each conflict where it occurs, so that the result keeps the intent of both upstream's
change and the commit. Record its position, its commit, the upstream pull request that it
conflicts with, and the resolution, for the roadmap's conflict table:

- Regenerate generated pipelines instead of merging their text: at a commit that conflicts in
  `.github/workflows/` or `ci-flowey/`, run `cargo xflowey regen`. A textual merge of them can also
  succeed wrongly: at v2.4 position 139, one kept upstream's old runner settings. Require
  `cargo xtask fmt --pass verify-flowey` at every commit that changes `flowey/` or the generated
  files.
- In `Cargo.lock`, keep the entries of both sides, and require `cargo check --locked` at that
  commit.

Upstream changes can also break the chain without a conflict: an API that a later commit uses
changes, or behavior that NVX relies on does. Builds and tests find them, as sections 4 and 6
describe. Fix each by the kind of release:

- In a rebase, add a fixup commit at the tip for each upstream change. Its message names the
  upstream pull request and the commits that it folds into at the next reorganization, as v2.4's
  positions 371-386 did.
- In a reorganization, fold each fix into the commits that it fixes, re-expressed for the code
  there, so that every commit builds. A commit keeps its anchor's author and author date and
  credits the authors of the commits that it folds with `Co-authored-by`. Commits after the
  roadmap's mapped tip may combine by concept, but those that can go upstream on their own stay
  apart, as in v3.0. Record where each fix went.

Then require no merges and a good signature, `G`, on every commit, and compare the chains. The
worktree's `HEAD` is now `<new-head>`:

```console
git -C <directory> rev-list --count --merges <new-base>..HEAD
git -C <directory> log --format='%h %G? %s' <new-base>..HEAD
git -C <directory> range-diff <old-base>..<old-head> <new-base>..HEAD
```

For a rebase, the range diff gives the counts of identical, changed, and dropped commits. A
reorganization that changes history only keeps the tree of the plain rebase without its drops, as
v3.0's `652f88f95` did: claim tree equivalence only when the two `^{tree}` IDs are equal. One that
changes behavior claims neither tree nor patch equivalence, as v4.0 did, and rests on per-commit
builds and tests.

## 4. Validate The Chain

Check every commit of a reorganization, and at least each commit of a rebase that a conflict or a
fixup changed. Check out each commit of `git rev-list --reverse <new-base>..HEAD` in a scratch
worktree, on x86_64 Linux and x86_64 Windows, run these, and run `rustfmt --check --edition 2024`
on each `.rs` file that the commit adds or changes:

```console
cargo check --locked --workspace --all-targets
cargo xtask fmt --pass lints
```

At the head, run the gates of the
[OpenVMM instructions](../../../openvmm/.github/copilot-instructions.md) for each package that a
conflict, fixup, or fold changed: `cargo xtask fmt` with all its passes,
`cargo clippy --workspace --all-targets -- -D warnings` on Windows and Linux, the unit tests of
those packages, and the microVM VMM tests through
`cargo xflowey vmm-tests-run --filter "test(microvm)"`, as the nested repository's `vmm-tests`
skill describes. Report each platform that you could not use as left to OpenVMM CI.

## 5. Publish The Release Branch

When the user asks, push the head as the release branch, without force. Its push runs the OpenVMM
CI workflow, whose push trigger covers `feature/microvm/**`, so the release needs no OpenVMM pull
request, as none of v2.2-v4.0 had. After the push below, wait until
`gh run list --repo microsoft/openvmm --commit <new-head>` lists the push's OpenVMM CI run before
running the check with the full SHA of `<new-head>`. The check returns `missing` immediately
while no run exists, even with `--wait-minutes`:

```console
git -C <directory> push <remote> HEAD:refs/heads/feature/microvm/release/<version>
python3 scripts/nvx.py check-openvmm-ci --revision <new-head> --wait-minutes 300
```

The six jobs that NVX requires must pass. Before calling another failed job unrelated, find its
root cause and show that it fails on upstream `main` or on the previous release too, as the v2.3
rebase did for its CVM jobs. A failure that the chain causes needs a fix. A fixup at the tip of a
rebase extends the release branch without force. A fold in a reorganization rewrites it, which
needs the user's approval, a lease on the published head, and a `feature/microvm/dev` that does
not point to it yet, as when v4.0's first run caught two tests that still expected the earlier ABI
versions:

```console
git -C <directory> push --force-with-lease=refs/heads/feature/microvm/release/<version>:<published-head> <remote> HEAD:refs/heads/feature/microvm/release/<version>
```

## 6. Pin The Release In NVX

Build the NVX branch, its two commits, and its pull request as sections 3, 5, and 6 of
nvx-openvmm-promote describe, with these differences:

- Its section 4 does not apply, since no commit of the old chain is an ancestor of the new head:
  remap the whole roadmap instead, as below.
- Name the gitlink commit `openvmm: pin microVM <version>, rebased onto upstream main`, or
  `reorganized onto upstream main` for a reorganization, and the roadmap commit
  `doc: map the OpenVMM roadmap onto the microVM <version> rebase`, or `reorganization`.
- The comparisons of what `nvx-rs` copies from OpenVMM decide the gitlink commit's coordinated
  changes. A release that changes an ABI changes more: v4.0's restarted versions changed
  `nvx::contract`, `SOURCE-MANIFEST.json`, the guest's decoders, and the tests that encode those
  versions, which the pull request explains under the `tests-modified` label.
- With the submodule at the new head, build OpenVMM with `nvx.py build-openvmm` and run
  `nvx.py test-microvm --backend <backend>` on each backend that you can reach, locally or through
  nvx-host-connect and then nvx-run, and leave the others to CI.

Remap the roadmap from Git, deriving each number as nvx-openvmm-promote's section 4 does, and
check every position and SHA that it names against
`git -C openvmm rev-list --reverse <new-base>..<new-head>`:

- **Scope**: the new fork base, its date, and the base before it; the SHAs and positions of the
  mapped tip and the pin; the pin's distance from the mapped tip; and the unassigned ranges.
- **Upstream status**: a first paragraph that says which chain moved from which base onto which,
  how many upstream commits later, and what the release dropped, resolved, and added.
- **Landed upstream**: a `<version>` row for each drop, and the branch that the release started
  from in the sentence after the table.
- **Open upstream pull requests**: each pull request's state, queried again, as of the date.
- **Conflicts**: a `Conflicts of the <version> rebase` table with each conflict's position, its
  commit, the previous release's commit, the upstream pull request, and the resolution, and for a
  reorganization a `The <version> reorganization` section that says where each fix went.
- **Upstream main past the fork base**: upstream's commits after the new base, and those that
  conflict with the chain.
- The dependency graph, the actual branch DAG, the commits after the mapped tip with each one's
  position in the previous release, the PR summary, and the exact commit-to-PR mapping.

Give the pull request the sections of microsoft/nvx#1238: the OpenVMM source, with the pinned
revision, the previous pin, the new base, upstream `main` since it, the absence of an OpenVMM pull
request, and the signatures; what changed in OpenVMM and in NVX; the equivalence evidence; the
validation; and the landing order of section 7. Ask whether it should be Draft or Ready.

## 7. Land The Release

Land the release in this order, pushing only when the user asks:

1. OpenVMM CI passed NVX's required jobs on `<new-head>`.
2. Fetch `feature/microvm/dev` again, and require it to still be `<old-head>`. If commits landed on
   it meanwhile, carry them onto the release first, then validate it and wait for its OpenVMM CI
   again.
3. Fast-forward `<prev-release>` to `<old-head>`, so that it holds the whole old line, including
   every NVX pin after its cut, and require `ls-remote` to print `<old-head>`. Never force it: when
   `<prev-release-head>` is not an ancestor of `<old-head>`, stop and ask.

   ```console
   git -C openvmm merge-base --is-ancestor <prev-release-head> <old-head>
   git -C openvmm push <remote> <old-head>:refs/heads/<prev-release>
   git -C openvmm ls-remote <remote> refs/heads/<prev-release>
   ```

4. Move `feature/microvm/dev` to `<new-head>` with a lease on `<old-head>`, and confirm it. The
   push starts another OpenVMM CI run of `<new-head>`, and each OpenVMM pull request still open
   against `feature/microvm/dev` then needs a rebase onto it.

   ```console
   git -C openvmm push --force-with-lease=refs/heads/feature/microvm/dev:<old-head> <remote> <new-head>:refs/heads/feature/microvm/dev
   git -C openvmm ls-remote <remote> refs/heads/feature/microvm/dev
   ```

5. The NVX pull request merges through the merge queue once a maintainer approves it.

Step 3 keeps the old line on a branch. Once `feature/microvm/dev` moves, the commits that followed
the previous release's cut are on no branch of the chain, yet NVX's history pins some of them, and
`git submodule update` of an older NVX commit must still fetch them. By the v2.4 rebase,
`feature/microvm/release/v2.3` had moved from its cut, `79a501b63`, 43 commits forward to the v2.3
line's last pin, `757a15fa3`. The v4.0 reorganization skipped the step:
`feature/microvm/release/v3.0` stayed at its cut, `0ac254109`, five commits short of the v3.0
line's last pin, `35e76265f`, which microsoft/nvx#1238 left on a pull request's branch that was
deleted later.

## Learnings

- OpenVMM's CI does not run NVX's workloads. microsoft/openvmm#4606 made OpenVMM's REPL quit at
  the end of a standard input that is not a terminal, which stopped a sandbox's microVM before its
  guest ran. No OpenVMM job caught it, but NVX CI's sandbox jobs failed on KVM, MSHV, and WHP, and
  v2.4's fixup 385 kept the microVM running. Run NVX's scenarios on the new head, not only
  OpenVMM's tests.
- Tests encode ABI versions. v4.0's first OpenVMM CI run failed `virt_mshv`'s CPUID test and the
  TTRPC microVM test, which still expected the earlier versions, so position 198 updates both, and
  the release branch was rewritten before `feature/microvm/dev` moved.
- Compare a failure with upstream `main` before blaming the rebase. v2.3's SNP and TDX jobs failed
  with the same out-of-space OpenHCL boots on upstream `main` and on the previous release, so they
  were not regressions of the rebase.

## Report

Report the kind and version of the release; the old and new bases and heads; the dropped commits
with their upstream pull requests; the conflicts and their resolutions; the fixups or folds; the
merge and signature checks; the equivalence evidence; the per-commit and head validation; the
OpenVMM CI runs; the previous release branch's fast-forward and the move of `feature/microvm/dev`;
the NVX commits, pull request, and exact-head workflow run; and every blocked or pending gate.
