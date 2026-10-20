<!--
Say what changed and why. Start with "Closes #N" or "Fixes #N" when the pull
request completes an issue, and "Part of #N" or "Refs #N" otherwise; see
doc/contribute.md for the conventions.
-->

## Validation

<!--
List the checks that you ran, and the gates that you leave to CI, such as
backends that you could not test.
-->

## Adapter contract

Does this change the adapter contract (public API, behavior, persisted formats or wire)? If so, add a migration note and conformance cases.

<!--
Answer "No", or describe the change and how adapters migrate, and name the
conformance cases that cover it. CI checks the public API of nvx that
adapters build on against tests/public-api: when it changes, run
`python3 scripts/check_public_api.py --update` and commit the snapshots with
the change.
-->
