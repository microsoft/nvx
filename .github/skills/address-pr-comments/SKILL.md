---
name: address-pr-comments
description: Address unresolved PR comments
disable-model-invocation: true
argument-hint: "PR number or URL (defaults to the current branch's PR)"
---
Address the unresolved review comments on the pull request identified by the input. If no pull
request is provided, find the pull request associated with the current branch.

- Record the initial working-tree state, including the `openvmm` submodule, and preserve all
  pre-existing changes.
- Use GitHub tooling to fetch review comments and identify unresolved review threads. Focus on
  unresolved threads; use resolved comments only when they provide necessary context. NVX review
  threads come almost entirely from Copilot code review, which re-reviews every push, so verify each
  claim against the code before acting on it.
- For each actionable unresolved comment, inspect the relevant code and nearby tests, then make the
  smallest change that addresses the underlying issue. Do not modify unrelated code.
- Test every fix locally. Start with the narrowest relevant test, lint, type-check, or build command
  inferred from repository documentation, nearby tests, and CI configuration. Expand validation when
  the affected behavior or blast radius requires it.
- If a comment is already addressed, obsolete, ambiguous, or cannot be validated locally, do not
  guess. Explain the status and any blocker in the final summary.
- The `dev` ruleset requires every review thread to be resolved before merge, so resolve a thread
  only when the pull-request head already reflects its outcome:
  - Do not reply to or resolve a thread whose fix exists only in the local working tree. Draft a
    template reply for the user to post after committing and pushing, for example
    `Fixed in <commit URL after push>. <what changed>`, and make clear that the placeholder must be
    replaced once the real commit URL exists.
  - When a commit on the pull-request head already addresses the comment, reply
    `Fixed in <commit URL>. <what changed>` citing that commit, then resolve the thread.
  - When no change is needed, reply `No change needed here. <evidence>`, then resolve the thread.
  - Leave ambiguous or blocked threads unresolved, and do not submit a new review.
- Post each reply from literal text or a file rather than an interpolated shell string, then read
  the posted reply back and confirm its body before resolving the thread.
- Do not stage or commit any changes. Leave all fixes unstaged in the working tree for review.

Finish by summarizing each unresolved thread and how it was handled, including the drafted reply for
each local-only fix, listing the validation commands and results, and noting any unresolved
blockers. Confirm that no changes were staged or committed.
