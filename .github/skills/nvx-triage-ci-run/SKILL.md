---
name: nvx-triage-ci-run
description: "Triage a failed, cancelled, or hanging microsoft/nvx GitHub Actions run: classify supersession and queueing, find the first causal failure for the exact head, reproduce it on the matching platform, and fix it locally without committing. Use for workflow run URLs or IDs, red pull-request checks, runner, matrix, and artifact-handoff failures, and performance-gate or benchmark-host failures."
argument-hint: "<workflow run URL or ID, or #pull-request>"
---

# Triage An NVX CI Run

Find the first causal failure of one exact head, reproduce it at the narrowest
scope, and fix repository defects without committing or pushing. Commands use
`python3`; use `python` on Windows.

1. Record the branch, `HEAD`, the status of NVX and `openvmm/`, and any Git
   operation in progress. Never reset, clean, discard, switch, stash, or
   update the submodule without permission.
2. Classify the run before treating it as a defect:

   ```console
   python3 scripts/ci_triage.py <run URL, run ID, or #PR>
   ```

   It resolves exactly one run and reports its attempt, event, and head; the
   pull request's current head and draft state; a classification; the failed
   jobs with their first failed step, runner, and matrix values; the first
   error in each failed log; earlier failures of the same jobs on `dev`; and
   open issues that name them. Act on the classification:
   - `superseded`: report the replacement run and stop, unless the cancelled
     run's cleanup failed.
   - `merge-group-cancelled`: check the pull request's queue outcome first.
   - `dev-push-cancelled`: `dev` pushes queue rather than replace each other,
     so investigate it.
   - `benchmark-host-changed`: the runner is no longer the host of its series'
     history, an infrastructure change; accept the host and reset the history
     only on request, as
     [Expected benchmark hosts](../../../doc/benchmarks.md#expected-benchmark-hosts)
     describes.

   A run that started while its pull request was Ready keeps running after the
   pull request becomes a draft; that is not the draft guard failing. Bind every
   conclusion to that run's attempt and head, and never take an older green run
   as evidence for the current head.
3. Read the complete logs of the failed jobs where the excerpt is not enough
   (`gh run view RUN --repo microsoft/nvx --job JOB --log-failed`), and the
   workflow, actions, scripts, and tests as they were at the run's commit
   (`git show SHA:PATH`). Separate the first cause from matrix cancellations,
   cleanup failures, and secondary errors. State one falsifiable hypothesis that
   a decisive log line and the owning code path support, and classify it as a
   product, test, workflow, dependency or toolchain, transient infrastructure,
   or missing-prerequisite failure. Change no code for an external or transient
   failure unless the repository has a concrete robustness defect.
4. Reproduce the earliest actionable failure with the narrowest equivalent
   command, on the run's platform and backend:
   - quality and CLI failures: `python3 scripts/local_gates.py --run --only
     GATE`, or the failed step's command;
   - boot, microVM, snapshot, guest-console, and benchmark acceptance failures:
     [nvx-run-microvm](../nvx-run-microvm/SKILL.md), after
     [nvx-prepare-host](../nvx-prepare-host/SKILL.md) selects a profile with
     the run's backend and host type;
   - performance regressions: compare the platform, ABI, processor count,
     metric direction, sample, and baseline window first, and reproduce only on
     the same host and series, with
     [nvx-collect-benchmarks](../nvx-collect-benchmarks/SKILL.md).

   Do not switch the checkout to the run's commit, install packages, change
   runners, elevate privileges, or repair hosts without approval, and report
   when the exact commit cannot run.
5. Fix the controlling code path with the smallest coherent change, never by
   weakening assertions, disabling tests, broadening retries, swallowing errors,
   or hiding output. Rerun the reproduction, then the original failed command,
   then `python3 scripts/local_gates.py --run`, and add regression coverage
   when behavior changes.
6. Do not commit, push, rerun or cancel runs, or change the pull request
   unless asked. When asked to push and follow CI, follow only the run of the
   pushed head, with one watcher, and rebind once when a newer head supersedes
   it.

## Report

Report the run's URL, attempt, and head; the failed job and step; the
classification; the decisive evidence and the root cause; the files changed;
the reproduction and validation commands with their results; any replacement
run; blocked checks; and the final status of NVX and `openvmm/`. Confirm that
nothing was committed or pushed.
