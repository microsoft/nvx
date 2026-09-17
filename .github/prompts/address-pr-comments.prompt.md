---
name: "Address PR Comments"
description: "Address unresolved PR comments"
argument-hint: "PR number or URL (defaults to the current branch's PR)"
agent: "agent"
---
Address the unresolved review comments on the pull request identified by the input. If no pull
request is provided, find the pull request associated with the current branch.

- Record the initial working-tree state and preserve all pre-existing changes.
- Use GitHub tooling to fetch review comments and identify unresolved review threads. Focus on
  unresolved threads; use resolved comments only when they provide necessary context.
- For each actionable unresolved comment, inspect the relevant code and nearby tests, then make the
  smallest change that addresses the underlying issue. Do not modify unrelated code.
- Test every fix locally. Start with the narrowest relevant test, lint, type-check, or build command
  inferred from repository documentation, nearby tests, and CI configuration. Expand validation when
  the affected behavior or blast radius requires it.
- If a comment is already addressed, obsolete, ambiguous, or cannot be validated locally, do not
  guess. Explain the status and any blocker in the final summary.
- After a fix passes local validation, reply briefly with what changed and resolve its review
  thread. Do not resolve ambiguous or blocked threads, and do not submit a new review.
- Do not stage or commit any changes. Leave all fixes unstaged in the working tree for review.

Finish by summarizing each unresolved thread and how it was handled, listing the validation commands
and results, and noting any unresolved blockers. Confirm that no changes were staged or committed.
