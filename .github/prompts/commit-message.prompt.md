---
name: "Commit Message"
description: "Generate a repository-conformant commit message for the currently staged changes"
agent: "agent"
---

# Suggest a Commit Message

Generate a commit message for the **currently staged** changes only. Do not stage,
commit, amend, or push anything — only produce the message text for the user to use.

Consider **only** what is in the staging area (the index). Ignore unstaged and
working-tree changes entirely. If nothing is staged, say so and stop without
producing a message.

## Steps

1. Inspect staged changes with `git diff --cached --stat` first to see scope, then
   `git diff --cached` for detail. Do **not** paste the full diff back into the chat —
   read it, then write the message.
   - Work from the diff hunks alone when they provide enough context. Do **not** read
     whole source files merely to restate what changed.
   - Only if a hunk is genuinely ambiguous, inspect **at most one or two** files and
     only the relevant ranges.
2. Discover the repository's commit-message conventions from explicit contributor
   guidance, templates, hooks, lint configuration, and other repository-local rules,
   then inspect recent commit history for established style. Do not assume any file
   location, language, package manager, hook path, lint tool, subject format, scope
   vocabulary, or length limit. Do not use unstaged file contents as evidence.
3. Prefer enforceable configuration over documentation and documentation over recent
   history. Resolve the staged change's intent, then write a message that follows the
   discovered format and style. Use the fallback rules below only when the repository
   provides no convention.
4. Output the final message inside one fenced ```text block so it can be copied as-is.

## Subject Line

- Follow the repository's required subject structure, capitalization, punctuation,
  scope or type vocabulary, and length limit exactly.
- Include a type, scope, issue key, or other prefix only when repository rules or
  established history support it. Never invent a scope or tag.
- If no convention exists, use a concise imperative summary in sentence case with no
  trailing period. Keep it to 72 characters when practical.
- Describe the dominant purpose and observable effect of the staged changes. Do not
  combine unrelated subjects or mention unstaged work.

## Body

- Include a body when repository rules require one or when it adds important context.
  Separate it from the subject with one blank line.
- Explain **what** changed and **why**, emphasizing behavior and intent rather than
  restating the diff. Match the repository's prose, wrapping, bullet, and footer style;
  if none exists, wrap prose at about 72 columns.
- Group details only when that improves clarity for a multi-part change.
- Reference issues, pull requests, breaking changes, co-authors, or other metadata only
  when justified by the staged diff or explicit user input, and use the repository's
  established syntax. Never fabricate identifiers or trailers.
- Mention validation commands only when evidenced and useful to future readers.

## Fallback Example

```text
Fix stale cache entries after updates

Invalidate affected entries when source records change so subsequent reads
return current data.
```

## Rules

- Read the diff; do not echo it back.
- Consider only staged changes; never describe unstaged or working-tree changes.
- If nothing is staged, report that and stop; do not produce a message.
- Use repository-specific conventions only when supported by repository evidence.
- Do not infer motivation, issue references, validation results, or compatibility
  effects that the staged diff and explicit user input do not establish.
- Do not run any mutating git command. Output the message and stop.
