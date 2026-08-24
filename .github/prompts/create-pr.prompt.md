---
name: "Create PR"
description: "Create PR with the current branch"
argument-hint: "Optional: issue number or context for the pull request"
agent: "agent"
---
Open a pull request for the current branch.

- Before pushing or creating the pull request, ask whether it should be **Regular** or **Draft**.
  Use the available question tool when possible and present **Regular** as the recommended default.
  Do not ask again if the input already makes the choice explicit; never infer **Draft** from silence.
- Discover the repository state, current branch, configured remotes, default remote, and default branch
  using the available source-control and hosting-provider tooling. Do not assume any repository layout,
  hosting provider, remote name, default-branch name, or globally installed command-line tool.
- Confirm that the current branch is suitable and inspect the commits that differ from the default
  branch. If the checkout is detached, the current branch is the default branch, no commits would be
  included, or the target remote and branch cannot be determined reliably, stop and report the issue.
- Do **not** stage, commit, amend, or modify files. If the working tree contains staged, unstaged, or
  untracked changes, report them and stop so the user can handle them first.
- Push the current branch to the selected remote and configure upstream tracking when needed. Preserve
  an existing upstream when it is valid; do not assume the remote is named `origin`.
- Derive the title from the commits that will be included, using a single commit's subject when
  appropriate. Follow commit, contribution, and pull-request conventions discovered in the repository.
- Write a concise body explaining **what** changed and **why**, based on the commits and the diff against
  the discovered default branch. Honor any repository pull-request template without dumping the full
  diff. Reference an issue supplied in the input only when the relationship and closing semantics are
  justified.
- Create the pull request against the discovered default branch with the repository host's available
  structured tools, API, or command-line tooling. Create a regular pull request by default; enable draft
  status only when the user chose **Draft**.
- Report the resulting pull-request URL and whether it was created as regular or draft. Do not change
  its review state afterward.
