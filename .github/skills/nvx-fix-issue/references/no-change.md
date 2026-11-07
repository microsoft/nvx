# When An Issue Needs No Change

When the issue is already fixed on `dev`, is obsolete, or does not reproduce:

1. Gather the evidence: the commit or pull request that fixed it or made it
   obsolete, or the attempts to reproduce it at the current `dev` on matching
   hosts, with their revisions, profiles, backends, commands, and results.
2. Post it with `gh issue comment N --repo microsoft/nvx --body-file FILE`.
   Close a fixed issue with `--reason completed` and an obsolete one with
   `--reason "not planned"`. Leave open an issue that only does not reproduce,
   since failed attempts do not show that it needs no change, least of all when
   it is intermittent, and ask in the comment for what would reproduce it, such
   as the host, revision, command, and logs. When the issue is already closed,
   only comment on it, leaving its state as it is.
3. Open no pull request. Remove what the session created on the hosts, and run
   `python3 scripts/issue_workspace.py cleanup N`, which undoes only what
   `create` did, where nothing changed since. Report what it kept, and the
   comment's URL.
