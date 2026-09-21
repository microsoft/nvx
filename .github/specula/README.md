# Specula incremental CI

This integration intentionally delegates modeling, retained state, resume, publication, and incremental updates to Specula itself. NVX contains only a thin adapter that resolves an NVX release tag to its pinned `nanvix/openvmm` submodule commit, maintains one clean persistent OpenVMM checkout, and invokes the pinned Specula version.

The first `incremental` request automatically runs `--ci-init` when no current model exists. Later requests run `--incremental` against the same CI directory. Interrupted runs are resumed explicitly with their Specula run ID. No historical model, local Docker image, kernel fixture, or machine-specific path other than the configurable state root is required.

## Runner provisioning

The runner must be a dedicated Linux x86_64 account with at least 32 GiB RAM, sufficient persistent disk, readable/writable `/dev/kvm`, and the `specula` label. Run:

```bash
bash .github/specula/setup-runner.sh
```

The setup script installs the pinned latest Specula commit, Copilot CLI, Java, Maven, Rust, cargo-nextest, Python environments, skills, and MCP configuration. Authenticate Copilot CLI either through the repository secret `SPECULA_COPILOT_TOKEN` or by running `copilot login` as the dedicated runner account. Use a dedicated token rather than a personal administrator token.

Agent routing is declared in `agents.json`: Copilot GPT-6 runs analysis, specification, harness, validation, repair, classification, reviews, and the main incremental conversation; Copilot GPT-5.5 is reserved for bug confirmation and reproduction.

For an LXD container, enable nesting and pass KVM from the host:

```bash
lxc config set INSTANCE security.nesting true
lxc config device add INSTANCE kvm unix-char source=/dev/kvm path=/dev/kvm mode=0660 gid=RUNNER_GID
```

## Local use

```bash
python3 .github/specula/ci.py --mode preflight --tag NVX_RELEASE_TAG
python3 .github/specula/ci.py --mode incremental --tag NVX_RELEASE_TAG --request-id local
python3 .github/specula/ci.py --mode resume --tag NVX_RELEASE_TAG --run-id SPECULA_RUN_ID
```

State is retained under `/mnt/data/openvmm-verification/state/openvmm-snapshot-restore`. The source checkout is stable across runs, which is required for Specula to compute cumulative diffs from the current model baseline.
