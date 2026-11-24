---
name: nvx-prepare-host
description: "Prepare a local host, its WSL, or a configured SSH host for NVX work: select a reachable host or a local fallback, probe its KVM, MSHV, or WHP backend and checkout, check out the requested revision in a temporary worktree, and stage nvx and the artifacts. Use before running, debugging, or benchmarking NVX on a host, when remote test hosts are unreachable, or to diagnose a host's prerequisites."
argument-hint: "[local|wsl|host-profile] [revision]"
---

# Prepare An NVX Host

Establish a verified execution context, report it, and stop; then continue
with [nvx-run-microvm](../nvx-run-microvm/SKILL.md) or
[nvx-collect-benchmarks](../nvx-collect-benchmarks/SKILL.md). Run the commands
from the NVX checkout. They use `python3`; use `python` on Windows.

## 1. Choose The Host

Work locally when the user asks for it or the local host provides the backend
natively. Use the profile that the user or the calling skill named; otherwise
let the inventory, the ignored `.nvx-hosts.json` that
[Setup](../../../doc/setup.md#remote-agent-hosts) describes, which the tool
reads from the main worktree when you work in a linked one, choose one for each
backend that the task needs:

```console
python3 scripts/nvx_hosts.py select BACKEND [--host-type HOST_TYPE]
```

It checks the backend's profiles over SSH concurrently, giving each a minute
to accept and end a session, and picks the first reachable one in the order
that the inventory declares them. When none is
reachable or configured, it falls back to this workstation: natively for its
own backend, WHP on Windows when the Windows Hypervisor Platform API reports a
hypervisor, or KVM or MSHV on Linux with access to the device, and for KVM on
Windows to the default WSL 2 distribution, when it exposes a usable `/dev/kvm`
and `python3`. MSHV never runs in WSL. `--host-type` rules out both fallbacks,
whose host type no one recorded, so pass it only when the task depends on the
host type, such as benchmarks and performance or hardware reproductions. Its
JSON names the `execution` (`ssh`, `local`, `wsl`, or `null`, with exit status
1), the `host`, the `checkout`, the `unreachable` profiles, and the `reason`.
Report the unreachable profiles and the fallback; when the result is `null`,
leave that backend to CI and say so. Never repair a host, the network, WSL, or
its configuration to make a target reachable without the user's approval.

- `python3 scripts/nvx_hosts.py list` shows every profile; when its
  `remote_enabled` is `false`, no remote host is configured.
- `python3 scripts/nvx_hosts.py show PROFILE` returns a profile's
  `ssh_target`, `backend`, `platform`, `host_type`, and `remote_repo`, which
  are authoritative: never connect to a target outside the inventory, override
  its backend, or infer bare metal or a virtual machine from probes. When
  `remote_repo` is `null`, ask for the checkout's absolute path, and pass it
  as `--cwd`.
- A local or WSL fallback has no host type: report it as `local` or `wsl`, not
  as `virtual-machine` or `baremetal`, and never use it for benchmarks or
  evidence that depends on the host type.

### WSL

The `wsl` execution runs Linux commands for KVM in WSL, as the Windows
benchmark coordinator does, in a checkout that WSL sees, `WSL_CHECKOUT`:

```console
wsl.exe --cd WSL_CHECKOUT --exec bash -lc "python3 scripts/nvx.py verify"
```

The `checkout` that `select` prints, this checkout's WSL path, serves when its
`.git` is a directory. Linux Git cannot open a linked worktree that Git for
Windows created, such as an agent session's, since its `.git` file names a
Windows path, and in WSL it can fail to create a repository on a Windows
drive, so a temporary worktree cannot replace it; yet the probe and the NVX
commands that read the revision need Git. Then commit the work that you test,
asking before you commit the user's, and test that commit in a clone in the
WSL file system that borrows the objects of the main checkout, the first entry
of `git worktree list`, whose WSL path `wsl.exe --exec wslpath -a -u PATH`
prints as `MAIN_WSL`:

```console
wsl.exe --exec bash -lc 'git clone -q --shared --no-checkout MAIN_WSL ~/nvx-wsl-REV && cd ~/nvx-wsl-REV && git remote set-url origin https://github.com/microsoft/nvx.git && git checkout -q --detach REV && git submodule update -q --init --checkout --reference MAIN_WSL/.git/modules/openvmm openvmm'
```

Drop `--reference` when the main checkout has no `.git/modules/openvmm`. The
clone's `origin` would name `MAIN_WSL`, so the command points it at NVX, as
[step 4](#4-revision)'s `host_worktree.py` requires. The clone is
`WSL_CHECKOUT`, whose Git commands run in WSL; it registers nothing in the
Windows repository, and `rm -rf` removes it at the end.

The login shell puts `cargo` on `PATH`. Run every Linux command of the steps
below, the probe and the Linux builds of `nvx` and OpenVMM included, this way,
with `python3`, and keep the Git commands of a Windows checkout in Windows.
Never change `.wslconfig`, the distribution's packages, or its device
permissions without the user's approval.

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
WHP), `nvx` and the artifacts, and running OpenVMM processes. On a host,
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

Build `nvx`, which runs microVMs and sandboxes and downloads releases, from
the checkout's root, and again after `nvx` changes; no package contains it:

```console
cargo build --release --locked --features cli --manifest-path nvx/Cargo.toml --target-dir nvx/target
```

Then stage the artifacts that the task's provenance calls for:

- Packaged: `nvx/target/release/nvx download --hypervisor BACKEND`
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
nvx/target/release/nvx run --hypervisor BACKEND --dry-run
```

## Handoff

Report the host, local, or WSL execution, the unreachable profiles that a
fallback replaced, the checkout and any temporary worktree or WSL clone,
the OS, backend, platform, and host type, the commit and dirty state, the
artifact source and the commands that staged it, the dry run's result, any
running OpenVMM process, and every privilege or platform limitation. For
interactive work on a remote host, open `ssh -tt <ssh_target>` and change to
that directory.
