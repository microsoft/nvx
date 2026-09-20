# Separately authorized OpenVMM finding follow-up

This is a template, not authorization to start a fix or publish anything.
Fill the evidence fields and obtain explicit approval before using it.

- Repository: https://github.com/nanvix/openvmm
- Target source commit: <exact source_commit>
- Native run: <run_id>
- Native publication: <immutable ci-published token>
- Finding: <id and exact disposition>
- Confirmation evidence: <retained local report and actual reproduction>
- Current applicability/reuse receipt: <path, or not reused>
- Approved local working directory: <directory below /mnt/data>

Inspect the evidence before modifying code. A TLA counterexample alone, an
ENV_LIMITED/MASKED disposition, a timeout, or a missing trace is not proof of an
implementation defect. If the finding is not confirmed and applicable to the
chosen source, report the missing evidence instead of inventing a fix.

For an approved, confirmed defect, create a dedicated local branch
`fix/specula-<finding-id>` in a separate clean checkout based on the approved
source commit. Never modify Specula's frozen source, published model, original
evidence, or the independent NVX runner checkout.

Reproduce the defect using the actual implementation. Make the smallest
source-level correction and add a regression covering the causal ordering.
Do not weaken a model invariant, fabricate expected traces, remove failing
scenarios, or reinterpret a known historical fix as a new discovery.
Use the existing bounded Docker launcher for builds/VMs: exactly26GiB RAM,
no additional swap, and retained local evidence below /mnt/data.

Leave the fix on its local branch. Write a local PR-description document with
the finding, affected revision, root cause, change, reproduction, regression
evidence, limitations, and links to the preserved native artifacts. Do not
commit, push, create a GitHub issue/PR, enable a workflow, or publish a release
unless the user separately authorizes that action.
