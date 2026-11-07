# Remapping The OpenVMM Roadmap Onto A Release

A release moves every commit of the chain, so the NVX pull request that pins it
remaps the whole [roadmap](../../../../doc/openvmm-upstream-roadmap.md). Derive
each number from Git, as `python3 scripts/openvmm_pin.py roadmap` does, and
check every position and SHA against
`git -C openvmm rev-list --reverse NEW_BASE..NEW_HEAD`. Follow each part's
current wording and table headers, and update:

- **Scope**: the new fork base, its date, and the base before it; the SHAs and
  positions of the mapped tip and the pin; the pin's distance from the mapped
  tip; and the positions not yet assigned to PRs.
- **Upstream status**: a first paragraph that says which chain moved from which
  base onto which, how many upstream commits later, and what the release
  dropped, resolved, and added.
- **Landed upstream**: a row for each drop, with the release's version, and the
  branch that the release started from in the sentence after the table.
- **Open upstream pull requests**: each pull request's state, queried again, as
  of the date.
- **Conflicts**: a `Conflicts of the VERSION rebase` table with each conflict's
  position, its commit, the previous release's commit, the upstream pull
  request, and the resolution; for a reorganization, also a
  `The VERSION reorganization` section that says where each fix went.
- **Upstream main past the fork base**: upstream's commits after the new base,
  and those that conflict with the chain.
- The dependency graph, the actual branch DAG, the commits after the mapped tip
  with each one's position in the previous release, the PR summary, and the
  exact commit-to-PR mapping.

Then run `python3 scripts/openvmm_pin.py check-roadmap --worktree`, which
checks the Scope, the DAG, the table, and the commit mapping against Git.
