---
name: nvx-rebase-openvmm-chain
description: "Rebase or reorganize the OpenVMM microVM chain onto the latest upstream main as a new feature/microvm/release branch, validate and publish it, pin it in NVX with a remapped roadmap, and land it. Use for microVM release rebases and reorganizations, chain commits that upstream landed, rebase conflicts and fixups, and the release landing order."
argument-hint: "[rebase|reorganization] [upstream=main] [Draft|Ready]"
---

# Rebase The OpenVMM MicroVM Chain

`feature/microvm/dev` of `microsoft/openvmm` carries the microVM chain on a
fork base taken from upstream `main`. A release moves the whole chain onto a
newer upstream `main` as `feature/microvm/release/vX.Y`:

- A **rebase**, the next minor version, replays every commit, drops those whose
  changes upstream landed, resolves conflicts in place, and adds fixups at its
  tip for upstream changes that break the chain without a conflict.
- A **reorganization**, the next major version, also folds those fixups into
  the commits that they fix and regroups commits so that every commit builds
  on the new base; it may change behavior.

It rewrites shared branches of `microsoft/openvmm`, so push only when the user
asks. Its NVX commits and pull request follow
[nvx-promote-openvmm-pin](../nvx-promote-openvmm-pin/SKILL.md), except as
section 6 says. Commands use `python3`; use `python` on Windows.

## 1. State

- Preserve dirty state in both repositories, and work on the chain in a worktree
  of `openvmm/` outside both checkouts, never in `openvmm/` itself.
- Use a remote of `openvmm/` that resolves to `microsoft/openvmm`; add one with
  `git -C openvmm remote add REMOTE https://github.com/microsoft/openvmm.git`
  when none does, since older checkouts name `nanvix/openvmm` `origin`.
- Record the state:

  ```console
  python3 scripts/openvmm_release.py state --remote REMOTE --kind rebase
  ```

  It fetches upstream `main`, `feature/microvm/dev`, and the release branches;
  stops on a shallow history; and reports `old_head`, `old_base` (which must be
  the roadmap's fork base), the previous release and its head, NVX's pin,
  `new_base` and its date, the next version (refusing one that exists), the
  open OpenVMM pull requests into `feature/microvm/dev` with their stacked
  layers, and NVX's open pin promotions. Pass `--kind reorganization` for a
  major version, and ask which kind it is unless the user said so.
- Settle the prerequisites: land each open OpenVMM pull request in dependency
  order, as nvx-promote-openvmm-pin describes, or agree with the user to carry
  its commits into the release. Retarget a stacked pull request to
  `feature/microvm/dev` with `gh pr edit` only once its parent landed, after
  `gh stack unstack` when a native stack locks its base. Merge or close NVX's
  open pin promotions, since a revision of the old chain leaves the branch that
  `.gitmodules` tracks once `feature/microvm/dev` moves.

## 2. Choose What To Drop

```console
python3 scripts/openvmm_release.py drops --old-base OLD_BASE --old-head OLD_HEAD --new-base NEW_BASE
```

It lists upstream's commits since the old base, the chain commits whose patches
upstream already has, and the roadmap's open upstream pull requests with their
states. Drop each commit whose change upstream merged, even in a revised form,
and adapt the later commits to upstream's version. Drop a commit that an
upstream change supersedes only once you have shown that upstream covers what
it does. Record each drop's former position, fork commit, roadmap PR, upstream
pull request, and upstream commit for the roadmap.

## 3. Replay The Chain

1. Add the worktree with an absolute directory outside both checkouts, and
   replay the chain onto the new base, changing each dropped commit's `pick` to
   `drop`:

   ```console
   git -C openvmm worktree add -b microvm-release-VERSION DIRECTORY OLD_HEAD
   git -C DIRECTORY rebase -i --onto NEW_BASE OLD_BASE
   ```

2. Keep commit signing on; never pass `--no-gpg-sign` or override
   `commit.gpgsign`, and stop if signing fails. Drop no other commit without
   the user's approval, and record each commit that the rebase drops as empty.
3. Resolve each conflict where it occurs, keeping the intent of both upstream's
   change and the commit, and record its position, commit, the upstream pull
   request that it conflicts with, and the resolution:
   - Regenerate generated pipelines with `cargo xflowey regen` instead of
     merging their text, and require `cargo xtask fmt --pass verify-flowey` at
     every commit that changes `flowey/` or the generated files.
   - In `Cargo.lock`, keep both sides' entries, and require
     `cargo check --locked` at that commit.
4. Fix upstream changes that break the chain without a conflict, which builds
   and tests find. In a rebase, add a fixup commit at the tip for each, naming
   the upstream pull request and the commits that it folds into at the next
   reorganization. In a reorganization, fold each fix into the commits that it
   fixes, keeping the anchor's author and date and crediting the folded
   commits' authors with `Co-authored-by`; keep commits that can go upstream on
   their own apart. Record where each fix went.
5. Check the chain:

   ```console
   python3 scripts/openvmm_release.py check-chain --dir DIRECTORY --new-base NEW_BASE --old-base OLD_BASE --old-head OLD_HEAD
   ```

   It requires no merges and a signature on every commit, good wherever this
   host can verify it; it lists the signatures that this host cannot verify,
   with how to let git verify them, and gives the range diff's counts. Claim
   tree equivalence only for equal trees; a reorganization that changes
   behavior claims neither tree nor patch equivalence and rests on per-commit
   builds and tests.

## 4. Validate The Chain

- `python3 scripts/openvmm_release.py check-commits --dir DIRECTORY --new-base NEW_BASE`
  runs `rustfmt` on the changed Rust files, `cargo check --locked --workspace
  --all-targets`, and `cargo xtask fmt --pass lints` at each selected commit,
  on x86_64 Linux and on x86_64 Windows: every commit of a reorganization, and
  at least the commits of a rebase that a conflict or a fixup changed
  (`--commits SHA...`). In a rebase, an intermediate failure that an upstream
  change causes is acceptable with the tip fixup that resolves it; the head
  must pass.
- At the head, run [the OpenVMM instructions'](../../../openvmm/.github/copilot-instructions.md)
  gates for each package that a conflict, fixup, or fold changed: `cargo xtask
  fmt`, `cargo clippy --workspace --all-targets -- -D warnings` on Linux and
  Windows, the packages' unit tests, and the microVM VMM tests through
  `cargo xflowey vmm-tests-run --filter "test(microvm)"`. Report each platform
  that you could not use as left to OpenVMM CI.
- OpenVMM's CI does not run NVX's workloads, so section 6 runs NVX's scenarios
  on the new head too.

## 5. Publish The Release Branch

When the user asks:

```console
python3 scripts/openvmm_release.py publish --dir DIRECTORY --version VERSION --remote REMOTE --push
```

It pushes the release branch without force, which runs OpenVMM CI, so the
release needs no OpenVMM pull request; waits for that run to appear; and checks
it with `nvx.py check-openvmm-ci`. The six jobs that NVX requires must pass.
Before calling another failed job unrelated, find its root cause and show that
it fails on upstream `main` or on the previous release too. Fix a failure that
the chain causes: a rebase gets a tip fixup, pushed without force; a
reorganization rewrites the branch, which needs the user's approval, a lease
on the published head, and a `feature/microvm/dev` that does not point to it
yet. Tests that encode ABI versions change with a reorganization that restarts
them.

## 6. Pin The Release In NVX

Build the NVX branch, its two commits, and its pull request as sections 3, 5,
and 6 of nvx-promote-openvmm-pin describe, with these differences:

- No commit of the old chain is an ancestor of the new head, so its roadmap
  section does not apply: remap the whole roadmap as
  [references/remap.md](references/remap.md) describes, then run
  `python3 scripts/openvmm_pin.py check-roadmap --worktree`.
- Name the commits `openvmm: pin microVM VERSION, rebased onto upstream main`
  (or `reorganized onto upstream main`) and
  `doc: map the OpenVMM roadmap onto the microVM VERSION rebase` (or
  `reorganization`).
- The comparisons of what `nvx-rs` copies from OpenVMM decide the gitlink
  commit's coordinated changes. A release that changes an ABI changes more,
  such as the CLI's compatibility policy, the snapshot manifest's versions,
  `SOURCE-MANIFEST.json`, the guest's decoders, and the tests that encode the
  versions, which the pull request explains under the `tests-modified` label.
- With the submodule at the new head, build OpenVMM with
  `python3 scripts/nvx.py build-openvmm`, and run
  `python3 scripts/nvx.py test-microvm --backend BACKEND` on each backend that
  you can reach, locally or through
  [nvx-prepare-host](../nvx-prepare-host/SKILL.md); leave the others to CI.

Its body gives the OpenVMM source (the pinned revision, the previous pin, the
new base, upstream `main` since it, the absence of an OpenVMM pull request, and
the signatures), what changed in OpenVMM and in NVX, the equivalence evidence,
the validation, and the landing order of section 7. Ask whether it is Draft or
Ready.

## 7. Land The Release

Once OpenVMM CI passed NVX's required jobs on the new head, and only when the
user asks:

```console
python3 scripts/openvmm_release.py land --remote REMOTE --old-head OLD_HEAD --new-head NEW_HEAD --prev-release PREV_RELEASE --push
```

Without `--push`, it prints the plan. It fetches `feature/microvm/dev` again
and stops when the branch moved: carry the new commits onto the release, repeat
sections 4 to 6, including publication, OpenVMM CI, and the NVX pin, and land
again with the new heads. It then fast-forwards the previous release branch to
the old head, never by force, so that every revision of the old line that NVX
pinned stays on a branch that `git submodule update` of an older NVX commit can
fetch; and it moves `feature/microvm/dev` to the new head with a lease on the
old one. Each OpenVMM pull request still open against `feature/microvm/dev`
then needs a rebase. The NVX pull request merges through the merge queue once
a maintainer approves it.

## Report

Report the kind and version of the release; the old and new bases and heads;
the drops with their upstream pull requests; the conflicts and their
resolutions; the fixups or folds; the merge and signature checks; the
equivalence evidence; the per-commit and head validation; the OpenVMM CI runs;
the previous release branch's fast-forward and the move of
`feature/microvm/dev`; the NVX commits, pull request, and exact-head workflow
run; and every blocked or pending gate.
