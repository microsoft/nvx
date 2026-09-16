---
name: nvx-ci-investigation
description: "Investigate and fix NVX GitHub Actions failures, cancelled or hanging checks, runner and matrix failures, artifact handoff errors, and performance-gate failures. Use for workflow run URLs or IDs, red PR checks, superseded-run triage, exact-head log analysis, and platform-specific reproduction."
argument-hint: "<workflow run URL or ID>"
---

# Investigate NVX CI

Identify the first causal failure for one exact workflow head, reproduce it at the
narrowest practical scope, and fix repository defects without committing or pushing.

## 1. Preserve The Local State

Record the current repository, branch, `HEAD`, worktree status, submodule status, and
any in-progress Git operation. Preserve all existing changes in both NVX and
`openvmm/`. Never reset, clean, discard, overwrite, switch, synchronize submodules,
or stash without explicit permission.

Resolve the supplied run URL or ID and verify that it belongs to the current GitHub
repository. If the input instead identifies a pull request or check, resolve exactly
one workflow run before continuing. Record:

- Run URL, database ID, attempt, workflow, event, conclusion, and timestamps.
- Branch or ref, head SHA, pull-request number and current head SHA when applicable.
- Draft state at run creation, current draft state, and relevant workflow trigger.
- Failed, cancelled, skipped, and pending jobs; runner OS, labels, and matrix values.

Ask before proceeding only when the selector remains ambiguous.

## 2. Classify Cancellation And Currency First

Read the `concurrency` and trigger configuration from the workflow at the run's head
SHA before treating a non-success result as a defect. NVX pull-request concurrency
cancels an older run when a newer head is synchronized.

- If a cancelled run's head differs from the current pull-request head and a newer run
  exists, classify it as expected supersession. Report the replacement run and stop
  unless cancellation cleanup itself failed.
- A run started while a pull request was Ready remains active after conversion to
  Draft because `converted_to_draft` is not a workflow trigger. Do not diagnose this
  as the draft guard being ignored.
- Investigate a cancelled current-head run when there is no replacement, a job timed
  out, the runner disappeared, or required cleanup or artifact publication failed.
- Bind every later conclusion to the run attempt and head SHA. Never use an older
  green run as evidence for the current head.

## 3. Find The First Causal Failure

Inspect annotations and complete logs for every failed job. Read enough context around
each error to distinguish the first cause from matrix cancellations, cleanup failures,
secondary errors, deprecation warnings, and expected diagnostics. Never expose secrets
or dump unrelated logs.

Inspect the workflow, local actions, scripts, configuration, and tests as they existed
at the run SHA. Use current-checkout files only to identify later fixes or current
validation conventions. Pay particular attention to:

- The backend (`kvm`, `mshv`, or `whp`), host type, runner instance, and platform ID.
- Producer and consumer job dependencies and whether a rerun reused successful jobs
  from an earlier attempt.
- Artifact names, retention, overwrite behavior, and run ID versus run-attempt scope.
  Same-run required handoffs use workflow artifacts; caches are not reliable handoffs.
- The exact OpenVMM gitlink, guest artifacts, benchmark metadata, and performance
  history dimensions used by the failing job.

State one falsifiable root-cause hypothesis grounded in a decisive log line and the
owning code path. Classify it as a product defect, test defect, workflow defect,
dependency or toolchain change, transient infrastructure issue, missing prerequisite,
or expected supersession. Do not modify code for external or transient failures unless
the repository has a concrete robustness defect.

## 4. Reproduce On The Matching Platform

Reproduce the earliest actionable failure with the narrowest equivalent command.
Match the run's revision, platform, tools, environment, feature flags, matrix inputs,
and generated artifacts as closely as practical.

- For quality or CLI failures, run the focused command from
  [check-quality](../../actions/check-quality/action.yml) or
  [validate-nvx](../../actions/validate-nvx/action.yml).
- For an NVX boot, microVM, snapshot, guest-console, or benchmark acceptance failure,
  load [nvx-debug](../nvx-debug/SKILL.md). When the matching backend is remote, first
  load [nvx-host-connect](../nvx-host-connect/SKILL.md) and use a configured profile
  with the same backend and host type.
- For a performance regression, inspect the failed summary, platform, ABI, processor
  count, metric direction, current sample, and exact baseline window before running
  [nvx-benchmark](../nvx-benchmark/SKILL.md). Never compare different hosts or series
  as a reproduction.

Do not alter the current checkout merely to reach the run SHA. Use immutable `git show`
content for inspection and report when exact-SHA execution is unavailable. Do not
install packages, change runner configuration, elevate privileges, or repair remote
hosts without explicit approval.

## 5. Fix And Validate

Trace a reproduced failure through nearby implementation code, call sites, and tests.
Make the smallest coherent fix to the controlling code path. Do not weaken assertions,
disable tests, broaden retries, swallow errors, or hide missing output.

Immediately rerun the narrow reproduction after the first substantive edit. If it
passes, rerun the original failed command and the complete applicable job gates. If it
fails, use the new evidence to repair the same path before widening scope. Add or
update focused regression coverage when behavior changes.

Confirm that no conflict markers, unmerged paths, unintended generated files, or
unrelated submodule changes remain. Review the final diff and both repository statuses.
Do not create or amend a commit, push, force-push, rerun a workflow, cancel a run, or
change pull-request state unless the user explicitly requests that separate mutation.

## 6. Follow Current-Head CI Without Thrashing

When the user separately asks to push and validate a fix, identify the workflow run by
the pushed head SHA. Keep at most one watcher for that run. Stop following a run as soon
as a newer head supersedes it, and rebind once to the replacement run. Do not start
overlapping `gh run watch` processes or repeatedly poll completed, draft-skipped, or
superseded runs.

## Report

Report the run URL, attempt, exact head, failed job and step, cancellation/currency
classification, primary failure, decisive evidence, root cause, files changed,
reproduction and validation commands with outcomes, replacement run when superseded,
blocked checks, and final NVX and OpenVMM worktree states. Confirm explicitly that no
commit or push was performed.
