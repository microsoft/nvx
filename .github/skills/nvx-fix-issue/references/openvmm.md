# Fixing An Issue In OpenVMM

When the fix needs OpenVMM changes, the invocation's authority covers a branch
of `microsoft/openvmm`, new commits on it, and a pull request against
`feature/microvm/dev`; rewriting its pushed commits still needs the user's
approval. The OpenVMM pull request lands first, by fast-forward, when the user
decides, and the NVX pull request must not merge before the pinned revision is
on `feature/microvm/dev`.

## Branch And Pull Request

1. Work in `openvmm/` under
   [its instructions](../../../../openvmm/.github/copilot-instructions.md), on a
   branch from the latest `feature/microvm/dev`, fetched by URL, since the
   remotes of `openvmm/` can point elsewhere:
   `git -C openvmm fetch https://github.com/microsoft/openvmm.git feature/microvm/dev`.
   When NVX's pin is not on that branch, as
   `git -C openvmm merge-base --is-ancestor PIN FETCH_HEAD` shows, ask the user
   which base to use.
2. Push the branch by URL, without force:
   `git -C openvmm push https://github.com/microsoft/openvmm.git HEAD:refs/heads/BRANCH`.
   Open a Ready pull request against `feature/microvm/dev` titled
   `[DO NOT REVIEW] <subject>`; OpenVMM's CI skips drafts and takes hours.
3. Pin its head on the topic branch as
   [nvx-promote-openvmm-pin](../../nvx-promote-openvmm-pin/SKILL.md) describes,
   with its source, vector, and CPU-profile checks and its validation: a commit
   that changes only the gitlink and the coordinated changes that the revision
   needs, and a `doc:` commit that maps the pin in the roadmap, among the fix's
   other commits rather than on a two-commit branch. Check them with
   `python3 scripts/openvmm_pin.py check-branch --topic`.

## Until It Lands

- Follow the OpenVMM pull request's CI run the way the NVX one is followed, with
  `gh pr checks M --repo microsoft/openvmm --watch --fail-fast --interval 60`,
  and fix the failures that the change causes under the OpenVMM instructions,
  with new commits pushed by URL without force.
- Whenever a fix changes that pull request's head, move the pin and the roadmap
  to the new head, folding them into their commits, and rerun the validation
  that the pin affects.
- Before finishing, `python3 scripts/openvmm_pin.py land PIN --remote REMOTE`,
  a dry run with a remote that resolves to `microsoft/openvmm`, must still find
  a fast-forward. When another change landed there first, ask the user before
  rebasing the OpenVMM pull request, then force-push it only with a lease on the
  head that `gh pr view M --repo microsoft/openvmm --json headRefOid` reports,
  move the pin, and repeat the validation, CI, and review.
- In the NVX pull request, tell the maintainer to land the OpenVMM pull request
  first by fast-forwarding `feature/microvm/dev` to the exact pinned revision,
  since OpenVMM's merge button squashes and would leave the pin off the branch.
