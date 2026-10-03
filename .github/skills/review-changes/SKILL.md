---
name: review-changes
description: Review current changes, address findings, and verify tests pass without committing
disable-model-invocation: true
argument-hint: "Optional: issue number this change should fix (e.g. 296)"
---
Review the current changes (staged, unstaged, and untracked) in this repository.

- Start with `git --no-pager status --short`, then inspect the diff with `git --no-pager diff` and
  `git --no-pager diff --staged`. Prefer `--stat` first, then drill into specific files — do not
  dump full diffs into context. `git diff` omits untracked files, so review those directly.
- If the `openvmm` submodule is dirty or its gitlink changed, review the nested changes with
  `git -C openvmm` and apply the nested repository's instructions and checks.
- Report findings, issues, and observations. Address them with code edits.
- If an issue number was provided in the input, confirm the change actually fixes it.
- Discover the repository's validation commands from its current CI definitions, contributor
  documentation, build manifests, task runners, and scripts. In NVX, the Validation section of the
  repository instructions names the authoritative local gates. Do not assume any repository layout,
  language, package manager, build system, command name, or globally installed tool.
- Run the narrowest checks for the changed behavior, then the complete applicable repository-defined
  build, test, lint, formatting, static-analysis, generation, packaging, and smoke gates. Use the
  prescribed tool versions, working directories, environment, feature flags, and matrix values.  Keep
  output focused on actionable results. Identify unavailable prerequisites precisely and report
  affected checks as skipped or blocked rather than passed.
- Do not commit nor stage any fixes. Leave changes in the working tree for review.
