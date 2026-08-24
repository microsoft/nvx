---
name: "Investigate Workflow Failure"
description: "Investigate a failed GitHub Actions workflow run"
argument-hint: "workflowRun=<URL or run ID>"
agent: "agent"
---

Investigate why GitHub Actions workflow run `${input:workflowRun:Workflow run URL or numeric ID}`
failed, reproduce the issue locally where feasible, and fix its root cause. Do not commit or push
any fixes.

1. Inspect the current repository, branch, `HEAD`, worktree status, and any in-progress Git
operation. Record all existing user changes and preserve them. Never reset, clean, discard,
overwrite, or stash them without explicit permission.
2. Resolve the supplied workflow run and verify that it belongs to this repository. Record its URL,
run ID and attempt, workflow, trigger, branch or ref, commit SHA, conclusion, failed jobs and steps,
runner OS or image, and relevant matrix values. If the selector is ambiguous or cannot identify
exactly one run, ask before proceeding.
3. Inspect annotations and complete logs for every failed job. Read enough context before and after
each error to identify the first causal failure, distinguishing it from downstream cancellations,
cleanup errors, secondary failures, and expected diagnostic output. Never expose secrets or dump
unrelated logs.
4. Inspect the workflow definition, referenced actions, scripts, configuration, and tests as they
existed at the run's commit SHA. Use the corresponding files in the current checkout only to
identify later changes or local validation conventions; do not assume they match the failed run.
5. State one falsifiable root-cause hypothesis grounded in the logs and owning code path. Classify
the failure as a product defect, test defect, workflow defect, dependency or toolchain change,
transient infrastructure issue, or missing environment prerequisite. Do not change code for a
transient or external failure unless the repository has a concrete robustness defect to fix.
6. Reproduce the earliest actionable failure with the narrowest equivalent local command. Match the
run's platform, tool versions, environment, feature flags, matrix inputs, and generated artifacts as
closely as practical. If `HEAD` differs from the run SHA, use the current checkout only: do not
create a temporary worktree, switch branches, reset, or otherwise alter `HEAD`. State when exact-SHA
reproduction is therefore unavailable and whether intervening changes could affect the result.
Report unavailable secrets, services, hardware, or privileges precisely.
7. Trace the reproduced failure to its root cause by inspecting nearby implementation code, call
sites, and relevant tests. Make the smallest coherent fix that addresses the cause. Do not mask the
failure by weakening assertions, disabling tests, broadening retries, swallowing errors, or changing
unrelated code.
8. Immediately rerun the narrow reproduction after the first substantive edit. If it passes, rerun
the original failed command and the complete applicable job gates from the workflow. If it fails,
use the new evidence to repair the same code path before widening scope. Add or update a focused
regression test when the fix changes behavior.
9. Confirm that no conflict markers, unmerged paths, or unintended generated files remain. Review
the final diff and worktree status. Do not create a commit, amend a commit, push, or force-push.
10. Finish with a concise report containing:
    - Workflow run URL, attempt, commit, failed job, and failed step.
    - Primary failure and root cause, with the decisive log evidence.
    - Files changed and why each change fixes the cause.
    - Reproduction and validation commands with outcomes.
    - Checks that were skipped or environment-blocked, without claiming they passed.
    - Final worktree status and explicit confirmation that no commit or push was performed.
