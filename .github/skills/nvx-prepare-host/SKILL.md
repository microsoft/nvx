---
name: nvx-prepare-host
description: "Prepare a local host or a configured SSH host for NVX work: resolve its profile, probe its KVM, MSHV, or WHP backend and checkout, check out the requested revision in a temporary worktree, and stage nvx-rs and the artifacts. Use before running, debugging, or benchmarking NVX on a host, or to diagnose a host's prerequisites."
argument-hint: "[local|host-profile] [revision]"
---

# Prepare An NVX Host

Establish a verified execution context, report it, and stop; then continue
with [nvx-run-microvm](../nvx-run-microvm/SKILL.md) or
[nvx-collect-benchmarks](../nvx-collect-benchmarks/SKILL.md). Run the commands
from the NVX checkout. They use `python3`; use `python` on Windows.

## 1. Choose The Host

Work locally when the user asks for it or the local host provides the backend.
Otherwise, list the profiles of the developer's inventory, the ignored
`.nvx-hosts.json` that [Setup](../../../doc/setup.md#remote-agent-hosts) describes,
which the tool reads from the main worktree when you work in a linked one:

```console
python3 scripts/nvx_hosts.py list
```

- When `remote_enabled` is `false`, no remote host is configured: say so and
  work locally.
- Use the profile that the user or the calling skill named, or ask the user to
  choose one. `python3 scripts/nvx_hosts.py show PROFILE` returns its
  `ssh_target`, `backend`, `platform`, `host_type`, and `remote_repo`, which
  are authoritative: never connect to a target outside the inventory, override
  its backend, or infer bare metal or a virtual machine from probes. When
  `remote_repo` is `null`, ask for the checkout's absolute path, and pass it
  as `--cwd`.

## 2. Safety

- Never ask for or handle passwords, keys, tokens, or passphrases; when SSH
  asks for one, the user types it in the terminal. Never disable host-key
  checking, and stop on a changed host key.
- Never reset, clean, switch, pull, overwrite, or update submodules in a dirty
  checkout. Test another revision in a temporary worktree, and ask before
  copying uncommitted files to a host.
- Never install packages, enable Windows features, change device permissions,
  edit `sudoers`, load kernel modules, reboot, or use `sudo` to get past an
  access check without the user's approval.
- Select one backend explicitly, never `both`.
- Never terminate an OpenVMM process that this session did not start.

## 3. Probe

Probe the local host or the profile's checkout:

```console
python3 scripts/host_probe.py --backend BACKEND
python3 scripts/nvx_hosts.py probe PROFILE
```

Both report the OS, Python, `HEAD`, the dirty state, the OpenVMM pin and
checkout, `nvx.py verify`, the backend's access (`/dev/kvm`, `/dev/mshv`, or
WHP), `nvx-rs` and the artifacts, and running OpenVMM processes. On a host,
`nvx_hosts.py` runs this checkout's copy of the probe, so the host's checkout
may predate it. Report a failed access check as the probe states it. When KVM
and MSHV are both usable and no backend was named, ask which one to use. A
smoke run is the definitive WHP check.

## 4. Revision

Use the checkout as it is only when it is clean and at the requested revision,
or when the user approves. To test another revision, such as a branch that you
pushed, without touching the checkout, add a temporary worktree, which checks
the remotes, fetches the branch, and checks out the worktree's OpenVMM pin:

```console
python3 scripts/nvx_hosts.py worktree PROFILE -- add FETCH_HEAD --fetch BRANCH
```

On a host, this runs this checkout's `host_worktree.py`, as `probe` does;
locally, run `python3 scripts/host_worktree.py add ...` directly. Work in the
worktree that it prints, passing it as `--cwd` on a remote host, and remove it
at the end with the same command, `remove PATH` taking the place of `add ...`.

## 5. Artifacts

Build `nvx-rs`, which runs microVMs and sandboxes and downloads releases, from
the checkout's root, and again after `nvx-rs` changes; no package contains it:

```console
cargo build --release --locked --features cli --manifest-path nvx-rs/Cargo.toml --target-dir nvx-rs/target
```

Then stage the artifacts that the task's provenance calls for:

- Packaged: `nvx-rs/target/release/nvx-rs download --hypervisor BACKEND`
  installs the release built from `HEAD`, or from its nearest released
  first-parent ancestor; report that release. It replaces every artifact, so
  never run it over artifacts that the user wants to keep.
- Changed inputs: the matching `nvx.py build-guest`, `build-openvmm`, or
  `build` command of [Build](../../../doc/build.md).
- Changes to the coordinator only: the matching release artifacts in place,
  with their provenance reported.
- The public sandbox scenarios of `test-microvm` also need
  `build/ubuntu-smoke-scratch.ext4`, which `download` installs from the
  releases that ship it. For an older release, format it on a Linux host with
  `python3 scripts/nvx.py build-scratch-template` and copy it to a Windows
  host.

A run downloads a release only into a checkout that holds no artifacts;
`NVX_AUTO_DOWNLOAD=0` turns that off. Check the generated command and its files
without starting a VM:

```console
nvx-rs/target/release/nvx-rs run --hypervisor BACKEND --dry-run
```

## Handoff

Report the host or local execution, the checkout and any temporary worktree,
the OS, backend, platform, and host type, the commit and dirty state, the
artifact source and the commands that staged it, the dry run's result, any
running OpenVMM process, and every privilege or platform limitation. For
interactive work on a remote host, open `ssh -tt <ssh_target>` and change to
that directory.
