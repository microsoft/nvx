# Mapping A Pin In The OpenVMM Roadmap

[The roadmap](../../../../doc/openvmm-upstream-roadmap.md) numbers the commits
of the microVM chain by position, from 001 on the fork base, and records NVX's
pin. `python3 scripts/openvmm_pin.py roadmap REVISION` derives every number
from complete Git history; never count by hand. Follow each part's current
wording and table header.

## New Commits

Record the new commits as unassigned to roadmap PRs unless they fix one (see
[Fixups](#fixups)), and update:

- **Scope**: the pin's full SHA and position; the chain back from it, naming
  the OpenVMM pull request that proposes or merged each group of positions and
  the current head of `feature/microvm/dev`; the pin's distance from the
  mapped tip; and the unassigned positions. The previous pin's pull request
  has usually merged since; check it with
  `gh pr view NUMBER --repo microsoft/openvmm --json state,mergeCommit`.
- **Actual branch DAG**: a step for the new positions before the
  `(NVX pin)` line, or a longer last step when they continue its topic, and
  the pin line's 9-character SHA.
- **Commits after the mapped tip**: the count and ranges in its opening
  paragraph, and the rows that the tool prints, completed with the pull
  request that proposes them.
- **The remarks after that table**: for each new commit, which earlier chain
  commits change the same files, and, separately, whether it applies to
  upstream `main` as it is, so that it can go upstream on its own. Sharing a
  file decides neither: only a commit that changes what an earlier one added
  revisits that commit's PR. A clean `merge-tree` result shows only that the
  commits apply, not that upstream has the code that they use; before saying
  that they can go upstream on their own, type-check their packages on the
  merged tree in a temporary worktree outside both checkouts, as the tool's
  output describes.

## Fixups

A new commit that fixes a commit that a roadmap PR maps is a fixup of that PR,
to fold into the commit that it fixes. End its row with `fold into <position>`,
give it its own `PRnn fixup` step in the DAG, and update the fixup positions
that the Scope and the commit mapping's introduction list, the fold sentences
after the DAG, the PR's row in the PR summary, its node's count in the
dependency graph, and its list and count in the commit mapping.

## Checks

`python3 scripts/openvmm_pin.py check-roadmap --worktree` checks every
position and SHA that the Scope, the DAG, the table, and the commit mapping
name against Git, and that the Scope's pin is the gitlink.
