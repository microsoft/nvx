---
name: nvx-promote-openvmm-pin
description: "Pin a reviewed OpenVMM revision in NVX: verify its provenance and signature, commit the gitlink and the OpenVMM roadmap mapping, validate the exact pin, and land the OpenVMM pull request by fast-forward. Use for OpenVMM pin updates, clean replacement pull requests, source-equivalence checks, and mapping new chain commits in the OpenVMM upstream roadmap."
argument-hint: "<OpenVMM PR URL or revision> [target=origin/dev] [Draft|Ready]"
---

# Promote An OpenVMM Pin

A promotion is two signed NVX commits directly on the target: one that moves
the `openvmm` gitlink, with only the coordinated changes that the revision
needs, then one that maps the pinned commits in the
[OpenVMM upstream roadmap](../../../doc/openvmm-upstream-roadmap.md). Never
merge or copy OpenVMM history into NVX. Moving the whole chain onto a newer
upstream `main` is [nvx-rebase-openvmm-chain](../nvx-rebase-openvmm-chain/SKILL.md)
instead. Commands use `python3`; use `python` on Windows.

## 1. State And Inputs

- Record both repositories before fetching or switching anything:
  `git status --short --branch`, `git submodule status -- openvmm`, and
  `git -C openvmm status --short --branch`. Never update, switch, reset, or
  clean a dirty repository; use an approved worktree or ask. Follow
  [the OpenVMM instructions](../../../openvmm/.github/copilot-instructions.md)
  for OpenVMM source work.
- Confirm the inputs: the OpenVMM pull request or exact revision; the target,
  `origin/dev` once `origin` resolves to `microsoft/nvx`; prerequisite
  revisions; whether it replaces an earlier pull request; and Draft or Ready,
  recommending Draft until the local gates pass.
- Fetch only the refs that you need. The tools stop on a shallow `openvmm/`;
  unshallow it from a remote that resolves to `microsoft/openvmm`.

## 2. Verify The Source

```console
python3 scripts/openvmm_pin.py source REVISION --pr NUMBER
```

It reports the full SHA, the signature, the tree, and the pull request's head,
base, state, review, and checks. Stop when the revision is missing, is not the
pull request's head, or lacks a prerequisite, and report an unverifiable
signature rather than treat it as verified. For a replacement of a reviewed
revision, `python3 scripts/openvmm_pin.py equivalence OLD NEW --old-base B
--new-base B` claims tree identity only for equal trees, and otherwise patch
equivalence from the range diff; never infer equivalence from titles or file
lists.

## 3. The Gitlink Commit

1. On a fresh branch from the refreshed target, with both repositories clean,
   run `git -C openvmm switch --detach REVISION`, and inspect
   `git diff --submodule=log -- openvmm`.
2. Compare what `nvx-rs` copies from OpenVMM, which only the NVX CLI jobs check:

   ```console
   cargo test --locked --manifest-path nvx-rs/Cargo.toml --no-default-features --test openvmm_vectors -- --ignored
   cargo test --locked --manifest-path nvx-rs/Cargo.toml --no-default-features --test openvmm_cpu_profiles -- --ignored
   ```

   Each stops at its first failure. Fix it and rerun until both pass:
   - a vector file differs: copy it unchanged into
     `nvx-rs/tests/vectors/openvmm/`, and record the revision in
     [the vectors' README](../../../nvx-rs/tests/vectors/README.md#openvmms-vectors);
   - an excerpt is no longer a run of whole lines of its file: copy the new
     lines unchanged into it, and record the revision the same way;
   - a contract version differs: change it in `nvx::contract`, with the code
     that reads what it versions;
   - a CPU profile was added, revised, or removed: update
     `nvx-rs/src/host/cli/cpu_generations.json` and the copy in
     `scripts/nvx_tools/time_abi.py`.
3. `python3 scripts/local_gates.py --run` runs the gates that these changes
   select, such as `nvx-rs`'s.
4. Stage only the gitlink and the coordinated changes, and commit, signed,
   when the user asked for commits or a pull request, with a subject such as
   `openvmm: pin <topic>`. Stop rather than commit unsigned, and never amend or
   rewrite a published commit without authorization.

## 4. The Roadmap Commit

1. `python3 scripts/openvmm_pin.py roadmap REVISION --target TARGET` checks
   that the previous pin is an ancestor and that the chain has no merges, and
   prints the pin's position and distance from the mapped tip, the new
   commits' positions with ready-made table rows, the earlier chain commits
   that change the same files, and whether the new commits apply to upstream
   `main`. When the previous pin is not an ancestor, it says whether the pull
   request needs a rebase onto the pin and new OpenVMM CI, or whether the
   chain itself was rebased, which nvx-rebase-openvmm-chain handles.
2. Edit the roadmap as [references/roadmap.md](references/roadmap.md)
   describes, then check it, and the links, before committing:

   ```console
   python3 scripts/openvmm_pin.py check-roadmap --worktree
   python3 scripts/check_markdown_links.py
   git diff --check
   ```

3. Commit it, signed, with a subject such as
   `doc: map the <topic> in the OpenVMM roadmap`, and a message that names the
   new positions and the pin, says what the commits change, and gives the
   pin's distance from the mapped tip.
4. `python3 scripts/openvmm_pin.py check-branch --target TARGET` requires
   exactly these two signed commits on the target, the first changing the
   gitlink and the coordinated paths that it lists, and the second only the
   roadmap, which names the gitlink as its pin.

## 5. Validate The Exact Pin

- `python3 scripts/nvx.py verify`.
- `python3 scripts/nvx.py check-openvmm-ci --revision REVISION` records
  OpenVMM's own CI of the exact pin, which NVX CI requires
  ([OpenVMM CI](../../../doc/ci.md#openvmm-ci)) and which an OpenVMM pull
  request gets only once it is ready for review, from a `microsoft/openvmm`
  branch. A pending job is still running; a failed one needs a rerun or a fix
  in OpenVMM before the promotion can pass.
- For OpenVMM source changes made in this task, run the nested repository's
  package-scoped gates; for a pin of reviewed source, cite its exact-head
  OpenVMM evidence, and never claim checks that did not run.
- Choose runtime checks from the changed behavior:
  [nvx-run-microvm](../nvx-run-microvm/SKILL.md) for a smoke test or a
  failure, and [nvx-collect-benchmarks](../nvx-collect-benchmarks/SKILL.md)
  only when the change needs performance evidence. Report backends that you
  could not reach as left to CI.

## 6. Pull Request And Landing

- Right before pushing, fetch the target again; when it moved, rebuild both
  commits on it, derive the roadmap's numbers again, and rerun the validation.
- Fill the [pull request template](../../pull_request_template.md) with the
  OpenVMM pull request and exact pinned SHA, the scope and exclusions, the
  prerequisites and their states, the equivalence evidence in its precise
  term, the roadmap positions, the validation, the exact head submitted to
  CI, and the landing order. Ask whether it is Draft or Ready.
- Follow only the run of the current head, and triage failures with
  [nvx-triage-ci-run](../nvx-triage-ci-run/SKILL.md). Its `openvmm-ci` job
  waits up to 300 minutes for OpenVMM's CI of the pin; when a required OpenVMM
  job failed, rerun it in OpenVMM, then rerun `openvmm-ci`.
- The pinned revision must be on `feature/microvm/dev` before the NVX pull
  request merges. `microsoft/openvmm` only squash-merges, so land the OpenVMM
  pull request by fast-forward, and only when the user asks:
  `python3 scripts/openvmm_pin.py land REVISION --remote REMOTE` prints the
  push, and `--push` runs it, never forced. When the branch moved past the
  pull request's base, the pull request needs a rebase, new OpenVMM CI, and a
  new promotion.

## Report

Report the source pull request and revision, the signature, the prerequisites,
the equivalence method and result, the target commit, both commits and their
files, the roadmap positions and the pin's distance from the mapped tip, the
comparisons of the vectors and CPU profiles, the local validation, the pull
request's URL and state, the exact-head workflow run, the OpenVMM pull
request's landing, and every blocked or pending gate.
