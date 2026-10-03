---
name: create-pr
description: Create PR with the current branch
disable-model-invocation: true
argument-hint: "Optional: issue number or context for the pull request"
---
Open a pull request for the current branch.

- Before pushing or creating the pull request, ask whether it should be **Ready** or **Draft**.
  Use the available question tool when possible. Ready pull requests start CI, while Draft pull
  requests skip its jobs. Recommend **Draft** when the branch changes non-documentation files and
  there is no evidence that the local deterministic gates passed for its head; otherwise recommend
  **Ready**. Do not ask again if the input already makes the choice explicit; never infer either state
  from silence.
- Discover the repository state, current branch, configured remotes, default remote, and default branch
  using the available source-control and hosting-provider tooling. Do not assume any repository layout,
  hosting provider, remote name, default-branch name, or globally installed command-line tool. Confirm
  that the target resolves to `microsoft/nvx`, never the archived `nanvix/nvx`.
- Confirm that the current branch is suitable and inspect the commits that differ from the default
  branch. If the checkout is detached, the current branch is the default branch, no commits would be
  included, the branch is behind or has diverged from its upstream, an open pull request already exists
  for the branch, or the target remote and branch cannot be determined reliably, stop and report the
  issue, including any existing pull-request URL.
- Do **not** stage, commit, amend, or modify files. If the working tree contains staged, unstaged, or
  untracked changes, report them and stop so the user can handle them first.
- Push the current branch to the selected remote and configure upstream tracking when needed. Preserve
  an existing upstream when it is valid; do not assume the remote is named `origin`. Never force-push.
- Derive the title from the commits that will be included, using a single commit's subject when
  appropriate. Follow commit, contribution, and pull-request conventions discovered in the repository.
  Ignore automation-generated pull requests and commits, such as gh-aw `[workflow-id]` title prefixes,
  `[bot]` authors, and merge commits, when inferring those conventions.
- Write a concise body explaining **what** changed and **why**, based on the commits and the diff against
  the discovered default branch. Honor any repository pull-request template without dumping the full
  diff. Reference an issue supplied in the input only when the relationship and closing semantics are
  justified: put `Closes #N` or `Fixes #N` first when the pull request completes the issue, and use
  `Part of #N` or `Refs #N` otherwise. Include a Validation section that lists the checks actually run
  and the gates left to CI. When the branch moves the `openvmm` gitlink, include the body content that
  the `nvx-openvmm-promote` skill requires.
- Create the pull request against the discovered default branch with the repository host's available
  structured tools, API, or command-line tooling. Create a Ready pull request unless the user chose
  **Draft**.
- Report the resulting pull-request URL and whether it was created as Ready or Draft. Do not change
  its review state afterward.
