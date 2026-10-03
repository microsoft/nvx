---
name: rebase-and-validate
description: Rebase the current branch onto a target branch, resolve conflicts safely, and validate the result
disable-model-invocation: true
argument-hint: "[target=origin/dev]"
---
Rebase the current Git branch onto the target branch supplied in the input, then ensure the
repository builds and its tests pass. If no target is supplied, use `dev` from the remote that
resolves to `microsoft/nvx`, normally `origin/dev`.

1. Inspect the current branch, `HEAD`, worktree status, and whether a rebase is already in progress.
Verify that the target ref exists and is not the current branch. Also record the `openvmm`
submodule status and whether it is dirty, any merge commits between the target and `HEAD`, and
which commits to be rebased are signed.
2. Resolve the effective target before rebasing. For a remote-tracking ref, fetch its branch from
that remote and use the refreshed remote-tracking ref. For a local branch with an upstream, fetch
and use its upstream ref without moving the local branch. For a local branch without an upstream,
use the supplied ref exactly. Do not perform a broad or destructive fetch.
3. Preserve all existing user changes, including uncommitted work in the `openvmm` submodule. Never
discard, overwrite, reset, or broadly replace them, and never update or switch a dirty submodule. If
local changes block the rebase, ask before stashing or using `--autostash`.
4. If no rebase is in progress, rebase the current branch onto the effective target ref. If one is
already in progress, inspect it and continue that rebase instead of starting another. If the branch
contains merge commits, such as earlier merges of `dev`, explain that a plain rebase drops them and
ask before flattening or preserving them. Keep commit signing enabled; never pass `--no-gpg-sign` or
override `commit.gpgsign`, and stop if signing fails.
5. Resolve conflicts by examining the base, both sides, nearby code, and relevant tests so the
result preserves the intent of both branches. Stage only resolved files and continue until the
rebase completes. Do not skip or drop commits unless explicitly authorized. Never resolve an
`openvmm` gitlink conflict by picking either pin: combining both changes usually needs a new
OpenVMM revision, so stop, report both revisions, and ask. Use `nvx-openvmm-promote` for a new pin.
6. Confirm that the effective target is an ancestor of the new `HEAD` and that no conflict markers
or unmerged paths remain. Confirm that every rebased commit that was signed is still signed.
Compare the `openvmm` gitlink at `HEAD` with the submodule checkout; `nvx.py verify` fails while
they differ. Ask before updating a clean submodule, and report dependent gates as blocked when it is
dirty.
7. Discover the repository's validation sources of truth from its current CI definitions,
contributor documentation, build manifests, task runners, and scripts. In NVX, the Validation
section of the repository instructions names the authoritative local gates. Do not assume any
repository layout, language, package manager, build system, command name, or globally installed
tool. Use the repository-prescribed commands and tool versions to identify the build, test, lint,
formatting, static-analysis, generation, packaging, and smoke gates applicable to the rebased
result.
8. Run the deterministic applicable gates with the working directories, environment, feature
flags, matrix values, dependency setup, and ordering prescribed by the repository. Start with the
narrowest checks for conflict-resolved or changed code, then run the complete applicable
CI-equivalent validation set. If a gate requires an unavailable platform, service, secret,
privilege, hardware feature, tool, dependency, or generated artifact, identify the prerequisite
precisely and report the gate as skipped or blocked instead of treating it as passed. Do not
substitute a materially different command without explaining why it is equivalent.
9. Diagnose and minimally fix failures caused by the rebased result. Rerun each failing check after
its fix, then rerun the complete applicable validation set. Do not change unrelated code merely to
make a check pass.
10. Do not push or force-push. Finish with a concise report containing the supplied and effective
target refs, resulting commit, conflicts and fixes, signature and submodule state, validation
commands and outcomes, final worktree status, and any checks that could not run. Note that
publishing the rewritten branch needs `git push --force-with-lease`, which restarts CI for a Ready
pull request and triggers a new Copilot review. Never claim success for a skipped or
environment-blocked check.
