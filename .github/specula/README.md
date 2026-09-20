# Specula bug-finding CI

`ci.py` is the shared local and GitHub entrypoint in `microsoft/nvx`. It uses `release.py` for source selection, native execution, recovery and publication. The target remains microVM snapshot/restore in `nanvix/openvmm`, selected by NVX's `openvmm` submodule.

## Trigger and execution

The workflow runs on NVX published releases or manual dispatch, never on push or pull requests. Manual dispatch is restricted to `refs/heads/dev`; release events still use their release tags. It checks out control code from trusted NVX `dev`, resolves the selected NVX release to an immutable commit on `dev` ancestry, and reads its `openvmm` gitlink. The gitlink must use the allowed `nanvix/openvmm` URL and point to a commit on OpenVMM `main` ancestry. Only that OpenVMM commit is cloned into the bounded verification runtime; NVX itself is not the modeling target and the submodule pin is not changed.

NVX's automated development releases are published with `GITHUB_TOKEN`, which does not trigger another release workflow. Use manual dispatch with the published development tag for those releases. Releases published with a credential that emits workflow events can trigger this workflow normally; no push/PR or `workflow_run` fallback is added.

With an existing compatible baseline, the default operation is native incremental verification. On the first invocation, the configured `bootstrap_revision` and pinned `initialization_seed` prepare a compatible baseline through native `--ci-init --byom`; the same invocation then incrementally verifies the requested release. Retained analysis, models and harnesses are reused, not discarded. Bootstrap completion alone is not a verification result for the requested release.

The bootstrap request has a stable identity. An interrupted bootstrap resumes its recorded native run rather than starting another initialization. Provider holds, unresolved-runtime checks and saved retry budgets still apply. Incomplete target incrementals require explicit `resume` with the same release and native run ID. This gate applies across request IDs and later releases for the same target, including timeout, OOM and completed-but-unpublished runs; creating another Actions dispatch does not discard pending work.

The bootstrap revision must be an ancestor of the selected release. Existing incompatible baselines are not overwritten, and source identities are never relabeled. After a future history rewrite, an operator must deliberately update the bootstrap configuration and select a separate store.

## Dedicated runner prerequisites

| Requirement | Configuration |
| --- | --- |
| Workflow repository and runner | `microsoft/nvx`, label `[specula]` |
| Verification source | NVX release's `openvmm` gitlink in `nanvix/openvmm` |
| Runner workspace | `/mnt/data/openvmm-verification/native-ci/runner/_work` |
| Host | Linux x64, accessible `/dev/mshv`, Docker, Python 3, Git, approximately 30 GiB RAM |
| Runtime identity | UID:GID `1001:1003`, device group `998` |
| Runtime image | Immutable digest in `config.json`, Specula pinned to `088049c5b3474340213cded2664cdb674bff1e1a` |
| Guest fixtures | Kernel and initramfs paths and SHA-256 pins in `config.json` |
| Retained seed | Exact directory, native manifest and manifest hash in `config.json` |
| Model credential | Owner-only `/mnt/data/openvmm-verification/private/copilot-auth.json` |
| Execution consent | Owner-only `/mnt/data/openvmm-verification/private/specula-execution-authorization.json` |

An administrator must provision a runner accessible to `microsoft/nvx` with the `specula` label and the host/runtime requirements above. This PR does not register, relabel or modify any runner, and the label alone does not provision those requirements. Use a runner version compatible with the pinned checkout and artifact actions.

The manual-dispatch job condition prevents selecting another ref in this workflow; it is not a boundary against someone who can modify the workflow itself. Runner administrators must restrict access to trusted repositories and, where available, trusted workflows/refs through runner-group policy. Runner labels and checking out `dev` alone do not provide that restriction.

The runtime, fixtures, seed and filesystem permissions must be provisioned before use; the workflow does not install packages, build images, register runners or use `sudo`. Authorized host provisioning can use:

```bash
bash .github/specula/controller/build-image.sh
```

The image build requires the immutable toolchain base in `controller/Dockerfile`, a Specula checkout at the pinned revision under `/mnt/data/openvmm-verification/repos/specula-latest-20260916`, and the retained `native-ci/cache/protoc-27.1` package. Specula's own source is archived into the image at build time, not vendored in this repository. Review any rebuilt image digest and deliberately update `config.json`.

The model credential stays on the host, mounted read-only only for model operations. Never substitute personal/admin credentials, host HOME or an SSH agent. No model secret is committed or uploaded.

`authorization.example.json` documents version 2 operator consent: workflow repository (`microsoft/nvx`), OpenVMM target, explicit authorization, the latest acknowledged interruption and the user's approval reference. Existing version 2 consent for `nanvix/openvmm` must be deliberately updated by the operator before execution from NVX; this PR does not rewrite host authorization. It is not a provider policy exception. Service-side filtering remains active; a new terminal policy stop creates a new hold that old consent does not clear. The committed example is disabled. Legacy version 1 records remain readable.

## Resource and retry limits

Each container has 26 GiB memory and 26 GiB total memory-plus-swap, six CPUs, 1024 PIDs, dropped capabilities, no-new-privileges and a read-only root filesystem. There is no Docker socket in the workload. Scratch, caches, source clones, model state and evidence remain under `/mnt/data`; engine layers remain Docker-managed.

New native conversations permit two policy continuations and three temporary-provider/transport resumptions. Saved runs retain their original budgets. Exhaustion fails; no provider switch or unlimited workflow retry is introduced.

Preparation and verification share a six-hour execution budget. The Actions job has a 420-minute timeout for overhead. GitHub concurrency and host/release locks prevent concurrent Specula workloads. The independent NVX runner does not share these locks; schedule other heavyweight work separately.

These are operational guardrails, not an adversarial secret-isolation boundary. The model process necessarily has its dedicated credential and network access.

## Local and GitHub use

```bash
# Readiness only; no model call, Rust build, VM or TLC run.
python3 .github/specula/ci.py --mode preflight --tag NVX_RELEASE_TAG

# Prepare the configured baseline if needed, then run incremental verification.
python3 .github/specula/ci.py --tag NVX_RELEASE_TAG --request-id local-release

# Resume an incomplete target verification using the same source identity.
python3 .github/specula/ci.py \
  --mode resume --tag NVX_RELEASE_TAG --run-id NATIVE_RUN_ID \
  --request-id local-resume
```

Preflight may fetch public Git objects and inspect native state in a credential-free container. A cold store reports `needs_initialization`; the configured bootstrap is performed only by the normal incremental invocation, not by preflight. Other readiness blockers prevent bootstrap.

Local commands also accept `--revision OPENVMM_FULL_SHA` instead of an NVX tag; this deliberately selects OpenVMM directly and requires OpenVMM `main` ancestry. Automatic bootstrap uses the same direct-source path for `bootstrap_revision`. `--mode initialize` remains available for explicit initialization, and `--preflight-only` never starts verification.

After merge and runner provisioning in `microsoft/nvx`, open **Actions → Specula release verification → Run workflow**, select `dev`, choose `incremental`, and provide an existing NVX release tag. For an incomplete target run, choose `resume`, the original NVX tag and its exact native run ID. A release event automatically uses incremental mode. Old tags predating the workflow require manual dispatch; do not move tags.

Completed requests reuse validated receipts rather than rerunning models. Native source, image, environment, seed and retained control-bundle identities are preserved on resume. A malformed request can fail before a report directory exists.

## Results and publication

GitHub displays `summary.md` and uploads only `summary.md` plus `result.json`, retained for 14 days in NVX Actions. Reports identify both the NVX release commit and the target OpenVMM source commit (`revision`); the two are not interchangeable. Detailed findings, models, prompts, traces and raw logs stay on the host. No issues, PRs, product fixes or pushes are created by CI.

For the default configuration, public request reports are below:

```text
/mnt/data/openvmm-verification/native-ci/work/release-ci/
  releases/requests/<request-id>/public/
    summary.md
    result.json
  releases/rejections/<rejection-id>/public/
  releases/active-work.json
  initializations/<bootstrap-request-id>/
```

Use the emitted `artifact_dir` and the result's `native_work`/`native_run` to locate evidence. Errors can use a rejection directory instead of overwriting an earlier request.

| Outcome | Behavior |
| --- | --- |
| Complete PASS / WARNING | Publish the valid native result; warnings remain visible |
| Complete FAIL | Fail the job; a complete reusable model may still become the next baseline |
| Incomplete / policy stop / timeout / OOM | Fail without promoting incomplete verification output |
| Bootstrap failure | Report preparation failure; do not claim the requested release was verified |

Native completion and baseline publication are not guarantees that the implementation is bug-free. Review finding evidence before using `controller/templates/copilot-fix.prompt.md` for a separately authorized fix branch and PR description.

## Runtime and retained assets

The container mounts its selected state directory at `/work` and shared build caches at `/cache`. Source clones, harness, guest fixtures, control scripts and optional initialization seed are read-only mounts at `/sources`, `/harness`, `/fixtures`, `/control` and `/seed`. Mount roots must be disjoint. Resume preserves those paths and the saved seed identity.

The runtime fixes Copilot/gpt-5.6-sol-fast/xhigh, single-agent initialization, and aggregate TLC limits of 12 GiB/four workers. Guidance also requires explicit per-job TLC bounds. The image supplies protoc 27.1 through `PROTOC` and `PROTOC_INCLUDE`; avoid broad package restoration that introduces source symlinks rejected by native isolation. Source inputs must be complete ordinary clones, not linked worktrees or partial clones.

Each runtime attempt retains its command, cgroup observations, console log and status under `<native_work>/runtime/<attempt>/`. Interruption stops the owned workload and preserves progress. Native resume requires a saved conversation; a failure between phases can require a separately prepared BYOM initialization instead. Do not infer sessions or fabricate completion from an exit code alone.

Historical experiments and their one-time import tools stay outside this PR. The default CI keeps the existing OpenVMM `work/release-ci` store and retained assets through native BYOM bootstrap. The NVX migration does not relabel OpenVMM source identities or change the model seed, existing experiment evidence or saved control bundles. New Actions request IDs use the `nvx-gh-` prefix.

## Lightweight development checks

```bash
cd .github/specula
PYTHONDONTWRITEBYTECODE=1 PYTHONPATH=.:tests:controller/tests \
  python3 -m unittest \
    test_release test_runtime \
    test_seeded_initialization test_ci test_nvx_release -q
```

These use disposable histories and synthetic execution receipts, not real model verification. Native storage/runtime contracts are in `controller/tests_native/` and run only in the bounded runtime. Mount a retained controller bundle with `--control`, an existing complete checkout with `--source`, and invoke `python3 -m unittest test_store test_runtime_contract` with `PYTHONPATH=/control/tests_native:/control:/opt/specula-native` in `controller/run.py`'s credential-free `exec` mode. Harness contracts are documented in `harness/README.md`.
