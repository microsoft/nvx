# Run

Download and install the host platform's package from the release for `HEAD`,
or from its nearest released first-parent ancestor when `HEAD` has no such
package, before the first run:

```bash
python3 scripts/nvx.py download
python3 scripts/nvx.py run
```

This installs the packaged kernel and initramfs under `build/` and OpenVMM
under `openvmm/target/release/`, so no local build is required. Set `GH_TOKEN`
to a token with contents read access when downloading from a private repository.
On Linux, pass `--hypervisor mshv` to both commands to use the MSHV package.

The CLI chooses WHP on Windows and KVM on Linux:

```bash
python3 scripts/nvx.py run
```

A successful Alpine boot prints both `ALPINE-MICROVM-BOOT-OK` and
`NVX-GUEST-BOOT-OK: alpine`. Boot Ubuntu userland with the same NVX kernel:

```bash
python3 scripts/nvx.py run --guest ubuntu
```

Ubuntu defaults to 512 MiB and prints `NVX-GUEST-BOOT-OK: ubuntu`. It is
Ubuntu userland with the NVX kernel, not a stock Ubuntu kernel or systemd VM.
Boot Azure Linux 3.0 userland the same way with `--guest azurelinux`. The
kernel unpacks its rootfs into a RAM filesystem capped at half of guest memory,
so it defaults to 512 MiB, and it prints `NVX-GUEST-BOOT-OK: azurelinux`.
Exit cleanly from the guest with:

```sh
/sbin/nvx-exit 0
```

Pass extra guest options without changing the generated device ABI:

```bash
python3 scripts/nvx.py run \
  --memory-mib 256 \
  --net 10.0.0.2/24 \
  --network-profile portable \
  --cmdline "quiet loglevel=0"
```

Networking requires the explicit `portable` capability profile. It uses the
same in-process data plane on KVM, MSHV, and WHP; omitting either `--net` or
`--network-profile portable` is rejected before OpenVMM starts.

Select an explicit processor count after building the matching specialized guest kernel:

```bash
python3 scripts/nvx.py run --machine microvm --processors 8
```

The microVM uses fixed device topology, reserves a shared interrupt-status page, and uses
shared-status edge-triggered virtio interrupts with 1, 2, 4, or 8 vCPUs.

## Run OpenVMM directly

The platform release archives are self-contained; `scripts/nvx.py` is a
convenience wrapper and is not required at runtime. Extract the archive that
matches the host and run the following command from its
`nvx-VERSION-PLATFORM` directory. For Linux/KVM:

```bash
./bin/openvmm \
  --single-process \
  --machine microvm \
  --processors 1 \
  --hypervisor kvm \
  --memory 128M \
  --kernel guest/vmlinux \
  --initrd guest/initramfs.cpio.gz
```

For Windows/WHP in PowerShell:

```powershell
.\bin\openvmm.exe `
  --single-process `
  --machine microvm `
  --processors 1 `
  --hypervisor whp `
  --memory 128M `
  --kernel guest\vmlinux `
  --initrd guest\initramfs.cpio.gz
```

For Linux/MSHV, use the `linux-mshv` archive and replace `kvm` with `mshv`.
If the artifacts are already installed in the repository layout, use
`openvmm/target/release/openvmm[.exe]`, `build/vmlinux`, and
`build/initramfs.cpio.gz` instead of the paths above.

For Ubuntu userland, use 512 MiB and select
`guest/initramfs-ubuntu.cpio.gz` or
`build/initramfs-ubuntu.cpio.gz` as the initrd. Below 320 MiB, the kernel
cannot unpack the whole Ubuntu initramfs and boots a truncated root. The
kernel path remains unchanged.

Direct OpenVMM launches accept generic directional network defaults:

```bash
./bin/openvmm \
  --single-process \
  --machine microvm \
  --hypervisor kvm \
  --memory 128M \
  --kernel guest/vmlinux \
  --initrd guest/initramfs.cpio.gz \
  --net 10.0.0.2/24 \
  --network-profile portable \
  --network-egress allow \
  --network-ingress deny
```

Egress defaults to `allow` and ingress defaults to `deny`. With ingress denied,
responses to guest-initiated connections remain available, while new inbound
connections do not. The portable profile supports egress `allow` or `deny` but
rejects ingress `allow` before the workload starts.

For destination and port rules, select an explicit default and repeat generic
allow/deny options:

```bash
./bin/openvmm \
  --single-process \
  --machine microvm \
  --hypervisor kvm \
  --memory 128M \
  --kernel guest/vmlinux \
  --initrd guest/initramfs.cpio.gz \
  --net 10.0.0.2/24 \
  --network-profile portable \
  --network-egress deny \
  --network-egress-allow 140.82.112.0/20:tcp:443 \
  --network-egress-deny 140.82.114.0/24:tcp:443
```

Rules match IPv4 addresses or CIDRs and may select one protocol: `tcp`, `udp`,
or `icmp`. A TCP or UDP rule may add one destination port, or an inclusive
range of them written `FIRST-LAST`; without a port, it matches every port of
that protocol. Ports are `1` through `65535`, and a range cannot end below its
first port. ICMP rules take no port. For example, `192.0.2.0/24:udp` matches
UDP on every port, `192.0.2.1:tcp:8000-8010` matches TCP ports 8000 through
8010 but not UDP, and `192.0.2.1:icmp` matches ICMP but not TCP or UDP. Deny
matches take precedence over allow matches, so adding
`--network-egress-deny 192.0.2.1:tcp:8005` keeps port 8005 blocked inside that
range.

NVX can lower protocol selectors, inclusive TCP/UDP port ranges, and rule-local
IPv4 exclusions to those native rules:

```json
{
  "allow": [
    {
      "to": [
        {
          "cidr": "192.0.2.0/24",
          "except": ["192.0.2.128/25"]
        }
      ],
      "ports": [
        {
          "protocol": "tcp",
          "port": 8000,
          "endPort": 8010
        }
      ]
    },
    {
      "to": [
        {
          "cidr": "192.0.2.200/32"
        }
      ]
    },
    {
      "to": [
        {
          "cidr": "198.51.100.0/24"
        }
      ],
      "ports": [
        {
          "protocol": "any",
          "port": 443
        }
      ]
    },
    {
      "to": [
        {
          "cidr": "198.51.100.7/32"
        }
      ],
      "ports": [
        {
          "protocol": "icmp"
        }
      ]
    }
  ],
  "deny": [
    {
      "to": [
        {
          "cidr": "192.0.2.0/24"
        }
      ],
      "ports": [
        {
          "protocol": "tcp",
          "port": 8005
        }
      ]
    }
  ]
}
```

Pass the file with `--network-egress-policy-file PATH` on `run`, one-shot
`sandbox run`, or `sandbox provision`. An explicit `--network-egress allow` or
`deny` is required. The file option cannot be mixed with
`--network-egress-allow` or `--network-egress-deny`.

The root accepts only `allow` and `deny` arrays. Each element uses the MXC
`NetworkRule` shape: optional `to` and `ports` arrays. Omitting `to` matches all
IPv4 destinations. Each `to` entry requires one IPv4 `cidr`; optional `except`
entries must be IPv4 CIDRs contained by that parent. Host bits are normalized
like the native CIDR syntax: `10.0.0.5/24` means `10.0.0.0/24`, not one host.
Use `/32` to select one IPv4 address.

Omitting `ports` matches every IPv4 transport supported by the native rule.
Each port selector follows MXC: `protocol` defaults to `any`. Without `port`,
`tcp` and `udp` match every port of that protocol, `icmp` matches ICMP alone,
and `any` matches every IPv4 protocol. A `port` in `1..65535` applies to
`tcp`, `udp`, or `any`, which expands to TCP and UDP on that port but not ICMP.
Optional inclusive `endPort` requires `port` and cannot be below it. IPv6 is
not supported.

Duplicate JSON properties, unknown fields, and explicit `null` protocol values
are rejected. Policy files are limited to 1 MiB of UTF-8 input. The previous
flat `cidr`/`except`/`protocol`/`port` rule form remains accepted for
compatibility, but new policy files should use the MXC shape.

A port range lowers to one native `FIRST-LAST` rule for each destination
network. The policy above therefore allows `192.0.2.0/25:tcp:8000-8010`, which
matches TCP ports 8000 through 8010 of that network but not UDP or ports 7999
and 8011, and its deny rule `192.0.2.0/24:tcp:8005` keeps port 8005 blocked
inside the range.

Exclusions affect only their containing rule: they never become global deny
rules. A later allow rule may therefore match an address excluded from an
earlier allow rule, while an address excluded from a deny rule falls through to
other rules and the explicit default. Explicit deny matches still take
precedence over allow matches.

NVX canonicalizes safely equivalent prefixes, merges the adjacent and
overlapping port ranges of each network, and rejects policies that lower to
more than 256 allow rules or 256 deny rules; protocol `any` with a port or a
port range lowers to one TCP and one UDP rule. NVX rejects oversized expansions
before launch rather than truncating or widening them. Managed provision stores
the validated lowered rules in sandbox state, so later starts do not reread a
mutable source policy file.

Host-loopback denial and deliberate localhost port publishing are separately
controlled from ordinary egress:

```bash
./bin/openvmm \
  --single-process \
  --machine microvm \
  --hypervisor kvm \
  --memory 128M \
  --kernel guest/vmlinux \
  --initrd guest/initramfs.cpio.gz \
  --net 10.0.0.2/24 \
  --network-profile portable \
  --host-loopback deny \
  --network-proxy 10.0.0.1:3128
```

With `deny`, general guest-to-host loopback and every host-to-guest forward are
blocked, while the exact TCP proxy endpoint remains available. UDP on that
same port and other host service ports remain blocked even with ordinary
egress allowed.

The portable profile does **not** support generic bidirectional host-loopback
allow. Explicit `--host-loopback allow` without any forward is rejected before
VM resources are opened. For deliberate port publishing, repeat
`--host-loopback-forward tcp:HOST_PORT:GUEST_PORT` or its UDP form to expose
only selected localhost ports toward the guest, with explicit
`--host-loopback allow`. These forwards do not satisfy a generic allow policy
that provides no port list. Omitting `--host-loopback` preserves existing
guest-to-host mapping without publishing guest ports. Guest-originated traffic
remains subject to egress policy.

Most `nvx.py run` options pass through unchanged: `--machine`, `--processors`,
`--cpu-profile`, `--mount`, `--net`, `--network-profile`, `--cmdline`,
`--restore-snapshot`, `--restore-processors`, and `--restore-ready-path`. The
wrapper performs these translations and additions:

| `nvx.py run` | Direct OpenVMM option |
| --- | --- |
| `--guest alpine` | `--initrd .../initramfs.cpio.gz` on a fresh boot |
| `--guest ubuntu` | `--initrd .../initramfs-ubuntu.cpio.gz` and a 512 MiB default on a fresh boot |
| `--guest azurelinux` | `--initrd .../initramfs-azurelinux.cpio.gz` and a 512 MiB default on a fresh boot |
| `--hypervisor auto` | `--hypervisor kvm` on Linux or `--hypervisor whp` on Windows |
| `--memory-mib N` | `--memory NM` |
| `--memory-capacity-mib N` | `--memory-capacity NM` on a fresh boot |
| `--restore-memory-mib N` | `--restore-memory NM` |
| `--dry-run` | No equivalent; this only prints the generated command |

Always include `--single-process`. When restoring, omit `--memory`, `--kernel`,
and `--initrd`. Every restore gives the guest fresh entropy through the time
ABI's restore packet, so `--restore-entropy` is no longer needed; OpenVMM still
accepts it without effect. Do not pass `--guest ubuntu` during restore; the
captured RAM and machine contract already identify the restored guest. For
example:

```bash
./bin/openvmm \
  --single-process \
  --machine microvm \
  --processors 8 \
  --hypervisor kvm \
  --restore-snapshot /var/lib/nvx/snapshot \
  --restore-processors 4 \
  --restore-memory 1024M \
  --restore-ready-path /run/nvx/restore-ready.sock
```

## Migration from the retired profile

`microvm` is now the only selector and launches the contract previously named
`microvm-v2`. The `microvm-v2` spelling, the former one-vCPU ABI-1 behavior,
ABI-1 device-I/O control, TTRPC numeric value 1, ABI-1 snapshot restore, and
boot-layout-1 snapshot restore are removed. Snapshot metadata and performance
series continue to use numeric ABI value 2 and boot-layout value 2.
Use NVX commit `cb52bcd454b454cb241096c33ed42a1dcdc65347` with OpenVMM commit
`1b70365613517a10718e00284a62bdffbd80e41c`, or an earlier compatible pair, to
run retired ABI-1 guests or snapshots.

## Snapshot restore readiness

Restore an existing microVM snapshot with an optional host-readiness endpoint:

```bash
python3 scripts/nvx.py run \
  --machine microvm \
  --processors 8 \
  --restore-snapshot /var/lib/nvx/snapshot \
  --restore-processors 4 \
  --restore-memory-mib 1024 \
  --restore-ready-path /run/nvx/restore-ready.sock
```

The endpoint must already be listening. OpenVMM connects to a Unix domain
socket on Linux or a `//./pipe/...` named pipe on Windows and writes exactly
`OPENVMM_RESTORE_READY_V1\n` after snapshot verification, attachment
resolution, worker startup, gated guest repair, and host-input re-enable
complete while the restored vCPU remains stopped. With
`--restore-processors`, the snapshot keeps its immutable eight-vCPU capacity;
the source must have captured a canonical `maxcpus` boot-online prefix, and the
guest onlines exactly the requested prefix before readiness and host input
release. Targets are limited to 1, 2, 4, or 8 and cannot be below the captured
boot-online count or above capacity. Legacy snapshots reject the option. The
peer must accept and read while startup is in progress; Windows flush
completion waits for the named-pipe peer to consume the frame. Failure to
write the complete event aborts and tears down the restore.

For restore-time memory expansion, capture a fresh snapshot with
`--memory-mib 512 --memory-capacity-mib 2048`, then select a target with
`--restore-memory-mib 512`, `1024`, or `2048`. The snapshot's `memory.bin`
remains exactly 512 MiB; selected expansion ranges receive fresh per-launch
backing and are onlined before restore readiness.

For a snapshot with image slots, select the process-local active prefix with:

```console
python3 scripts/nvx.py run \
  --restore-snapshot build/snapshot \
  --restore-image-slots 4
```

The target must be between the snapshot's cold-boot prefix and four. Activation
finishes before restore readiness; slots cannot be activated later.

## virtio-fs host mapping

The microVM has two mapping slots with the fixed tags `microvm` and
`microvm1`. On a cold boot with `--mount`, the initramfs mounts each mapping
automatically:

```bash
python3 scripts/nvx.py run --mount "/mnt/host,/absolute/host/share,rw"
```

PowerShell example:

```powershell
python scripts\nvx.py run `
  --mount "/mnt/host,C:\Users\me\microvm-share,rw"
```

Use `ro` for read-only access. The guest target must be an absolute Linux path.
Host paths containing commas are unsupported. Repeat `--mount` once to attach
a second directory with its own target and access mode, for example a
read-write workspace next to a read-only tool cache:

```bash
python3 scripts/nvx.py run \
  --mount "/workspace,/srv/checkout,rw" \
  --mount "/opt/hostedtoolcache,/opt/hostedtoolcache,ro"
```

The first mapping uses tag `microvm` and the second tag `microvm1`. OpenVMM
enforces each mapping's access mode on the host, so a read-only mapping
rejects writes with `EROFS` even if guest root remounts its tag read-write.
Guest targets that equal or contain one another, host directories that equal
or contain one another, and more than two mappings are rejected before boot.
A snapshot captured with mappings requires the same mappings, in the same
order, with the same canonical host paths, targets, modes, and filesystem
identities.
A repeatable `--mount-deny HOST_PATH` hides an existing file or directory
inside a mapped root. With two mappings, each `--mount-deny` must be an
absolute path, and it applies to the mapping whose root contains it. Denied
names are omitted from directory listings and remain
inaccessible through `..`, a symlink/junction, or another mount of the same
virtio-fs device. Unsafe, external, duplicate, overlapping, and nested-mount
rules are rejected before boot. A repeatable `--mount-allow HOST_PATH`
exposes a path inside a denied path again, and a repeatable
`--mount-write HOST_PATH` makes a path one of the only writable parts of an
`rw` mapping; see [Access policy](#access-policy). A snapshot captured with
these paths requires the same denied, allowed, and writable paths.
In an `rw` mapping, the guest can create symbolic links, and OpenVMM stores
each target exactly as given. The guest resolves links in its own namespace;
OpenVMM never follows a link while resolving a host path, so a link to an
absolute host path, outside the root, or into a denied path cannot reach host
data. An `ro` mapping rejects link creation with `EROFS`. On Windows, links
are WSL-style reparse points, which Windows path resolution never follows.
Treat links in a writable share as untrusted when host software later reads
the directory.
By default, OpenVMM performs every guest operation on the mapping as its own
user. On a Linux host, `--mount-owner caller` instead performs each one as the
guest caller's UID and GID and squashes guest root to the owner of the host
directory, so files that guest root creates are owned by that owner rather
than by root or OpenVMM; see [File ownership](#file-ownership). A snapshot
captured with a mapping also requires the same `--mount-owner` mode. Capture
and restore inspect and reopen the guest's open files as OpenVMM's user, so
unless OpenVMM runs as root, they fail while the guest holds a file that only
its caller can reach or reopen, such as one open for writing.
A snapshot captured without a mapping may restore with one new `--mount`; after
resume, mount it explicitly inside the guest because the initramfs hook has
already completed:

```sh
mkdir -p /mnt/host
mount -t virtiofs microvm /mnt/host
```

## Experimental single-workload sandbox

The `sandbox` command launches the microVM with one to three compressed
EROFS lower layers and one preformatted ext4 scratch image:

```bash
python3 scripts/nvx.py build-distro-layer \
  --guest ubuntu \
  --output build/ubuntu-distro.erofs
```

Read the deterministic UUID from
`build/ubuntu-distro.erofs.manifest.json`, create scratch independently with
`mkfs.ext4`, then launch the Ubuntu workload through the existing Alpine
control initramfs:

```bash
python3 scripts/nvx.py sandbox \
  --layer distro,build/ubuntu-distro.erofs,11111111-1111-1111-1111-111111111111 \
  --scratch /var/lib/nvx/scratch.ext4 \
  --entrypoint /bin/sh \
  --workload-user 65534:65534 \
  --memory-mib 256
```

CI uses `/sbin/nvx-sandbox-smoke` as the entrypoint to verify the security
profile that this section describes before clean guest exit. Before any other
check, it requires:

- the fixed `65534:65534` identity as the real, effective, saved, and
  file-system IDs, no supplementary groups, and `HOME=/nonexistent`;
- empty inheritable, permitted, effective, bounding, and ambient capability
  sets and `NoNewPrivs: 1`;
- mount, PID, and UTS namespaces other than the guest's initial ones, whose
  inode numbers Linux 6.18 fixes in its UAPI, with the workload as PID 1 of its
  PID namespace and the `--hostname` value as its host name;
- exactly one mount at each of `/`, `/proc`, `/sys`, `/dev`, `/dev/pts`, and
  `/dev/shm`, none of which propagates to another namespace: an overlay root
  over the EROFS layers whose upper directory is on scratch, `nosuid,nodev,noexec`
  procfs, read-only sysfs, and a private `/dev` tmpfs that holds only `fd`,
  `full`, `null`, `ptmx`, `pts`, `random`, `shm`, `stderr`, `stdin`, `stdout`,
  `tty`, `urandom`, and `zero`, with its own devpts instance;
- the agent-owned `/container` cgroup; and
- Ubuntu image files that the workload cannot modify, and a scratch-backed
  `/tmp` write that lands in the overlay.

A managed workload that mounted into the agent's namespace would stack its
runtime mounts on those of earlier requests, so the single-mount check also
covers repeated managed execs. With `--arg limits --arg MEMORY_MAX --arg
PIDS_MAX`, for a sandbox started with the same `--memory-max` and
`--pids-max`, it also requires the memory controller to kill an allocation of
twice `MEMORY_MAX` with `SIGKILL` while a 1 MiB allocation succeeds, and the
pids controller to stop the workload at `PIDS_MAX` processes, itself included;
keep twice `MEMORY_MAX` within the guest's free memory. With
`--arg TARGET --arg ro|rw`, it also checks a live share at
`TARGET` as described below, including symbolic links in an `rw` share; the
share needs a host-created, world-writable `nvx-links` directory for them.
Repeat the pair to check several shares; with an `rw` and an `ro` share, it
also verifies that a link in the `rw` share cannot write into the `ro` share.
With `--mount-owner caller`, `--arg caller` performs the `rw` checks and also
verifies that the workload owns what it creates, populates a directory it
created, and creates links in its own `nvx-caller-links` directory, while
`--arg eperm` requires reading and listing the share to fail with `EPERM` and
writes to fail, which is the result when OpenVMM cannot assume the workload
identity. `--arg policy` checks an `rw` share whose
[access policy](#access-policy) denies `nvx-denied`, allows
`nvx-denied/nvx-allowed`, and makes only `nvx-writable` writable: the workload
writes only inside `nvx-writable`, gets `EROFS` everywhere else, cannot
hard-link a read-only file into `nvx-writable`, and sees only `nvx-allowed`,
which stays readable, in `nvx-denied`. The share's other directories and files
must be writable by every identity, so that only OpenVMM can refuse a write.

The layer UUID is the EROFS superblock UUID, not a content digest. The command
validates the files before launch, orders roles independently of option order,
attaches layers read-only, and reserves the writable slot for scratch.
Conversion and scratch formatting stay off the start path. The Ubuntu
converter verifies immutable inputs, applies the deny-by-default metadata
policy, creates `/nonexistent` for UID/GID 65534, and invokes `mkfs.erofs`.
Systemd entrypoints are explicitly unsupported and do not relax the non-root,
drop-all-capabilities sandbox policy.

Managed sandboxes can opt into image slots at provision time:

```console
python3 scripts/nvx.py sandbox provision \
  --state-dir build/sandbox \
  --layer distro,build/distro.erofs,EROFS_UUID \
  --scratch build/scratch.ext4 \
  --image-slot-boot-count 4
python3 scripts/nvx.py sandbox start --state-dir build/sandbox
python3 scripts/nvx.py sandbox bind-image-slot \
  --state-dir build/sandbox \
  --image-slot 0 \
  --image-path build/image.raw \
  --image-identity sha256:IMAGE_DIGEST
python3 scripts/nvx.py sandbox query-image-slots --state-dir build/sandbox
```

Capacity is always four; `--image-slot-boot-count` selects `B`. Workloads that
need every slot immediately use `B = 4`, while other slot-declaring launches
use `B = 1`. A slot-declaring managed launch may omit a `distro` layer and boot
with scratch plus empty image slots. Image slot 0 uses the second virtio-fs
slot's transport, so a slot-declaring launch accepts at most one `--mount`.
A slot-declaring configuration uses format 6, which earlier NVX releases reject
rather than start without its image slots. Launches without image slots retain
the existing layer requirement, behavior, and configuration formats.

This is the cold-filesystem bootstrap described in
[the sandbox design](design/sandbox-filesystem-and-agent-architecture.md#implemented-filesystem-bootstrap), not the final
production agent. It accepts no environment variables or secrets and does not
expose sandbox snapshot capture or restore, the configuration region, or runtime RPC.
Arguments are individual kernel-command-line tokens and therefore cannot
contain whitespace. The workload enters private mount/PID/UTS namespaces with
a private `/dev`, an agent-owned cgroup, no capabilities, and `no_new_privs`.
It always runs as the fixed non-root `UID:GID` selected at VM creation
(`65534:65534` by default). The guest verifies that exactly one matching user,
its primary group, and its absolute home directory exist in the assembled
root; otherwise the workload is never started.
The outer agent retains the initramfs root; the capability-stripped child
enters only the assembled root with `chroot`, because Linux cannot
`pivot_root` away from an initramfs `rootfs`.

### Live host-directory shares

`sandbox run` and `sandbox provision` accept up to two
`--mount GUEST_TARGET,HOST_PATH[,ro|rw]` (default `ro`) options, each with its
own guest target and access mode, plus repeatable `--mount-deny HOST_PATH`,
`--mount-allow HOST_PATH`, and `--mount-write HOST_PATH` rules (see
[Access policy](#access-policy)). OpenVMM exports each host directory through
its own microVM virtio-fs device and enforces its access mode and policy on
the host side, so edits are visible in both directions without staging or
copy-back:

```bash
python3 scripts/nvx.py sandbox \
  --layer distro,build/ubuntu-distro.erofs,11111111-1111-1111-1111-111111111111 \
  --scratch /var/lib/nvx/scratch.ext4 \
  --mount /workspace,/srv/checkout,rw \
  --mount-deny .git/credentials \
  --mount /opt/hostedtoolcache,/opt/hostedtoolcache,ro \
  --entrypoint /bin/sh
```

A relative `--mount-deny`, `--mount-allow`, or `--mount-write` path is
resolved inside its share's host directory. With one share, every such path
applies to it. With two, each applies to the `--mount` before it and must name
a path inside that share's directory. The guest targets and the host directories of the two
shares must not equal or contain one another, because one share could
otherwise hide the other or reach its files under a different access mode.
NVX and OpenVMM compare the host directories by resolved path and by file
identity, so one directory reached through two paths, such as a bind mount,
is rejected too. NVX also compares each directory's identity with those of
the other directory's ancestors, so a share inside a bind mount of the other
share's directory is rejected before OpenVMM starts. On Linux, OpenVMM also
compares the filesystem sources that each directory reaches, through the
mount that contains it and every mount below it, so it also rejects a bind
mount or nested mount that exposes part of one share inside the other. That
check is Linux-only, so on Windows, don't share a directory that reaches the
other share's files through a mount.
Guest mount flags are not a security boundary: OpenVMM rejects every write to
an `ro` share with `EROFS`, whichever mount or link inside the guest reaches
it.
After it assembles the container overlay and verifies the workload identity,
the guest agent creates each target inside the container root and mounts the
shares there in order with `nosuid,nodev` before the workload enters its
private mount namespace. A one-shot workload exit, a managed `stop`, and any
failure after a share is mounted unmount the mounted shares in reverse order
before the overlay is unmounted or the VM powers off. Each target must be an
absolute, canonical path; `/`, `/etc`, and
the `/proc`, `/sys`, `/dev`, and `/.nvx-agent` trees are reserved for the
container runtime.
The guest refuses a target whose path crosses a symbolic link in a container
layer, a repeated tag, and overlapping targets, and any validation or mount
failure aborts the sandbox with status 125 instead of starting the workload
without its shares.

An `rw` share supports the symbolic links that package managers and
language toolchains create; see
[virtio-fs host mapping](#virtio-fs-host-mapping) for their semantics. The
existing OpenVMM file-identity policy applies to each share. A managed
sandbox stores each share's absolute host path, mode, denied, allowed, and
writable paths and the ownership mode in its configuration, and reattaches
every share on each `start`. A configuration with two shares uses format 4,
which earlier NVX releases reject rather than start without the second share.
A configuration with allowed or writable paths uses format 5, which earlier
NVX releases reject rather than start a share without its access policy.

#### Access policy

Each share can narrow what the workload can see and modify inside it. OpenVMM
enforces the policy on the host for every request, whichever guest mount, path,
or link reaches the share:

- `--mount-deny HOST_PATH` hides a path and everything below it.
- `--mount-allow HOST_PATH` exposes a path inside a denied path, and
  everything below it, again. The hidden directories on the way to it are
  traverse-only: the workload can enter and list them, but the listing shows
  only the entries that lead to allowed paths, any other name fails with
  `EACCES` (`Permission denied`), and the workload cannot modify them. A
  denied path may in turn lie inside an allowed path.
- `--mount-write HOST_PATH`, on an `rw` share, makes a file or directory, and
  everything below it, one of the only writable parts of the share. Every
  other write fails with `EROFS` (`Read-only file system`), including
  creating, removing, or renaming an entry in a read-only directory, which
  also stops a rename into or out of a writable path. A hard link from a
  writable path to a read-only file fails with `EXDEV` (`Invalid cross-device
  link`), so the link cannot make the file writable.

Each path must exist inside the share's host directory, must not cross a
symbolic link or junction, and is attributed to a share like `--mount-deny`.
Allowed paths follow the share's write policy: without `--mount-write`, an
allowed path in an `rw` share is writable; with it, an allowed path is
writable only inside a writable path. For example,
the following share hides the MCP gateway's logs except the tool payloads
that it spills below them, which stay readable, and lets the workload write
only to `agent` and `cache`:

```bash
python3 scripts/nvx.py sandbox \
  --layer distro,build/ubuntu-distro.erofs,11111111-1111-1111-1111-111111111111 \
  --scratch /var/lib/nvx/scratch.ext4 \
  --mount /tmp/mcp-runtime,/tmp/mcp-runtime,rw \
  --mount-deny mcp-logs \
  --mount-allow mcp-logs/mcp-payloads \
  --mount-write agent \
  --mount-write cache \
  --entrypoint /bin/sh
```

OpenVMM rejects an allowed path outside a denied path, a writable path inside
a hidden part of the share or inside another writable path, and a writable
path on an `ro` share before boot. The policy applies to names inside the
share: a host hard link or bind mount that makes a read-only file reachable
inside a writable path makes it writable, and one that makes part of a denied
path reachable elsewhere exposes that part, so do not create such aliases
inside a shared directory.

#### File ownership

`--mount-owner` selects the host identity that performs the share's file
operations:

- `vmm` (the default): OpenVMM performs every operation as its own user, so
  the files and directories that the workload creates are owned by the
  OpenVMM user. Guest file permissions use the ownership and mode bits that
  OpenVMM reports, so grant the workload identity access to the host
  directory. The workload cannot create entries inside a directory it created
  unless the host grants write access to others.
- `caller`: OpenVMM performs each operation as the guest caller's numeric UID
  and GID, without supplementary groups or capabilities. Files and
  directories that the workload creates are owned by its `--workload-user`
  identity on the host, and the host also enforces permissions for that
  identity, so the workload can populate the directories it creates. Guest
  UID 0 and GID 0 are squashed to the owner and group of the host directory,
  which therefore must not be owned by UID 0 or GID 0; the guest can create
  neither root-owned nor setuid-root host files. If OpenVMM cannot assume a
  caller's identity, that operation fails with `EPERM` (`Operation not
  permitted`) instead of running as OpenVMM.

```bash
python3 scripts/nvx.py sandbox \
  --layer distro,/var/lib/nvx/distro.erofs,11111111-1111-1111-1111-111111111111 \
  --scratch /var/lib/nvx/scratch.ext4 \
  --mount /workspace,/srv/checkout,rw \
  --mount-owner caller \
  --workload-user 1001:1001 \
  --entrypoint /bin/sh
```

As for any sandbox, the layers must define the workload user. Choosing the
owner of the host directory as the workload identity keeps every file in it
owned by that user.

`caller` requires a Linux host. Windows has no per-request POSIX identity for
OpenVMM to switch to, so NVX and OpenVMM reject `--mount-owner caller` on
Windows/WHP. On Linux, assuming an identity other than OpenVMM's own, or
dropping OpenVMM's supplementary groups, needs the `CAP_SETUID` and
`CAP_SETGID` capabilities, for example as ambient capabilities of an
unprivileged NVX process. `sudo` replaces `HOME` with root's home directory,
so the example passes the user's own back:

```bash
sudo setpriv --reuid="$(id -u)" --regid="$(id -g)" --init-groups \
  --inh-caps=+setuid,+setgid --ambient-caps=+setuid,+setgid -- \
  env HOME="$HOME" python3 scripts/nvx.py sandbox ... --mount-owner caller
```

A service manager can grant the same set, such as systemd's
`AmbientCapabilities=CAP_SETUID CAP_SETGID`. Without these capabilities, an
operation succeeds only if its caller has OpenVMM's user and primary group and
OpenVMM's user belongs to no other group; every other operation fails with
`EPERM`. Hosts commonly grant access to `/dev/kvm` or `/dev/mshv` through such
a group, so `caller` usually needs both capabilities. Grant OpenVMM no other
capabilities: `CAP_SETUID` lets it assume any host identity, which `caller`
mode confines to its export.

The guest kernel reports each caller's identity, and guest root may assume any
identity inside the guest, so guest root and a compromised guest kernel can act
as any nonzero host UID and GID inside the share, including leaving setuid
files that those identities own. Share only directories that such identities
may modify, and keep the share on a host filesystem mounted `nosuid` when host
users might execute files from it.

### Managed lifecycle

For a state-aware sandbox, provision configuration without starting a VM,
start it once, run multiple workloads in the same warm guest, stop it while
retaining configuration, and finally deprovision it:

```bash
python3 scripts/nvx.py sandbox provision \
  --state-dir /run/user/1000/nvx-example \
  --layer distro,/var/lib/nvx/distro.erofs,11111111-1111-1111-1111-111111111111 \
  --scratch /var/lib/nvx/scratch.ext4
python3 scripts/nvx.py sandbox start \
  --state-dir /run/user/1000/nvx-example
python3 scripts/nvx.py sandbox exec \
  --state-dir /run/user/1000/nvx-example \
  --entrypoint /usr/bin/python3 --arg=/work/agent.py --cwd /work \
  --environment-file /run/user/1000/nvx-example-environment.json \
  --outcome-report /run/user/1000/nvx-example-exec.json
python3 scripts/nvx.py sandbox exec \
  --state-dir /run/user/1000/nvx-example \
  --entrypoint /bin/sh --arg=-c --arg='cat /tmp/previous-result'
python3 scripts/nvx.py sandbox stop \
  --state-dir /run/user/1000/nvx-example
python3 scripts/nvx.py sandbox deprovision \
  --state-dir /run/user/1000/nvx-example
```

Lifecycle transitions fail closed: `start` rejects an already-running or stale
runtime record, `exec` and `stop` require a live OpenVMM process, and
`deprovision` refuses to remove a running sandbox or unknown files. If the guest
exits while `start` waits for it to become ready, for example because it refuses
the workload identity, OpenVMM closes the control endpoint and shuts down.
`start` then waits up to `--timeout` seconds for OpenVMM to publish
`outcome.json` and exit, and fails with the report's category and status, such
as `guest-exit status 125`. It terminates OpenVMM only if OpenVMM outlives that
wait. The runtime record identifies OpenVMM by its process ID and start time, so
these checks treat OpenVMM as gone once it exits, even if no process reaps it or
another process reuses its ID. A record that an earlier NVX version wrote lacks
the start time and identifies OpenVMM by its process ID alone. Managed
workload arguments use the bounded control protocol rather than the kernel
command line and may contain whitespace. Managed execution can select an
absolute working directory and either repeated inline `KEY=VALUE` entries or a
UTF-8 JSON-array environment file. The two environment forms are mutually
exclusive. Omission inherits the guest bootstrap environment, not the host
environment: `PATH=/usr/sbin:/usr/bin:/sbin:/bin`, `TERM=linux`, and `HOME`,
`USER`, and `LOGNAME` resolved from the fixed workload identity. An empty file
array requests an empty environment. `--inherit-default-environment` layers
the supplied entries over that default environment instead of replacing it,
each entry replacing the default variable of the same name; without supplied
entries it has no effect. Inline values are visible in host process arguments
and should not be used for secrets. These options apply only to managed
`sandbox exec`; one-shot execution rejects them. The workload sees one machine
ID for the life of the VM. On `stop`, the guest agent unmounts the live share,
overlay, layers, and scratch in dependency order before the VM powers off, as it
does when a one-shot workload exits. The legacy operation-less `sandbox`
form is `sandbox run`; it remains one-shot and rejects `--state-dir` or any
request to retain VM state.

Environment files are limited to 1 MiB of UTF-8 JSON. Environments contain at
most 256 unique, nonempty names. Each `KEY=VALUE` entry and working-directory
path is limited to 4096 UTF-8 bytes; the combined execution request must also
fit the existing 64 KiB control-payload bound. A managed `exec` forwards at
most 1 MiB of combined standard output and standard error; the guest agent
kills a workload that writes more, and `exec` exits with status 125 and reports
the `output-limit` category.

The explicit `test-microvm --scenario managed-exec-config --backend BACKEND`
scenario is the authoritative acceptance for these public options. It invokes
`scripts/nvx.py sandbox provision`, `start`, `exec`, `stop`, and `deprovision`
as subprocesses with an Alpine control guest and an Ubuntu workload layer. It
checks sequential distinct CWD and exact-environment requests, an environment
layered over the defaults, and then omitted defaults. It resolves the selected
UID 65534 account from the Ubuntu workload's own passwd database through a
public managed `getent` execution, then requires exactly the documented `PATH`,
`TERM`, `HOME`, `USER`, and `LOGNAME` values, which a layered entry replaces
only in its own execution. Unrelated guest bootstrap and shell-provided entries are
permitted because omission inherits the guest bootstrap environment; prior
request entries and the internal execution-config descriptor must not leak.
It also checks public
rejection of relative CWD and out-of-range `uint32`
timeouts, timeout recovery, stdout/stderr forwarding, and typed exec outcome
reports. It requires `build/ubuntu-distro.erofs`, its manifest, and the
`build/ubuntu-smoke-scratch.ext4` template produced by the guest-artifact build.
CI invokes it separately on every backend; it is not included in the default
scenario set because downloaded packages do not include the scratch template.
Empty environments are measured with `/usr/bin/env`, not a shell that can
synthesize its own variables. The scenario retains bounded subprocess argument
and status observations, typed exec outcomes, and OpenVMM logs. Inline
environment values are redacted from the retained command observations.

The explicit `test-microvm --scenario sandbox-lifecycle --backend BACKEND`
scenario is the authoritative acceptance for the managed lifecycle itself. It
also runs the public commands as subprocesses against the real kernel, Alpine
control initramfs, Ubuntu EROFS layer, and fresh copies of the same scratch
template, so it has the same artifact requirements and also runs only when
named. It checks that:

- `provision` rejects a root `--workload-user` before it creates any state, and
  the guest refuses an identity that the Ubuntu image lacks before a one-shot
  workload starts, and in a managed `start`, which then fails with
  `guest-exit status 125` from OpenVMM's outcome report and leaves only
  `config.json`, `openvmm.log`, and that `outcome.json`, without a runtime
  record, capability, control socket, report staging file, or OpenVMM process;
- every operation fails on a state directory that was never provisioned, a
  repeated `provision` leaves the configuration unchanged, and `exec` and
  `stop` fail before `start`;
- a repeated `start` and a `deprovision` of a running sandbox fail without
  changing its runtime record, capability, or OpenVMM process, and the
  sandbox keeps serving requests;
- managed arguments keep leading, trailing, and embedded spaces, tabs, and
  newlines, a later request reads the file that an earlier one wrote, and the
  file survives `stop` and a new `start`, because the overlay's upper directory
  is on scratch;
- `/sbin/nvx-sandbox-smoke` passes its security profile and resource-limit
  checks in a sandbox provisioned with `--hostname`, `--memory-max`, and
  `--pids-max`, after earlier requests;
- a request that writes more than the 1 MiB output bound exits with status
  125 and an `output-limit` outcome report after the host has received more
  than 1 MiB less one 32 KiB read chunk and at most 1 MiB, and a request whose
  `--outcome-report` path exists fails before its workload runs and leaves the
  file intact;
- `stop` ends OpenVMM and its control socket or pipe, leaves only
  `config.json`, `openvmm.log`, and a successful `outcome.json`, and leaves a
  cleanly unmounted scratch file system, after which `exec` and `stop` fail;
- after OpenVMM exits without a `stop`, `exec` and `stop` report the stale
  runtime state and `start` refuses it; and
- `deprovision` removes only NVX's files: it fails and keeps a foreign file in
  the state directory, and once that file is gone, it removes the directory,
  which a later `exec` does not recreate.

Throughout, no OpenVMM process whose arguments name the scenario's fixture
outlives its sandbox, and the EROFS layer keeps its digest and the scratch
image its file. On failure, the scenario stops and deprovisions what it
started, ends every OpenVMM process that names its fixture, and preserves the
fixture for recovery only if that cleanup fails. Its evidence, written to the
output directory with a `sandbox-lifecycle-` prefix, holds bounded command
observations (arguments, statuses, output sizes, and state-directory entries),
the runtime record, the OpenVMM logs, the VM-level and exec outcome reports, the
guest probe's transcript, and the console of the refused one-shot run, each
limited to its last 64 KiB. The command observations hold no workload output,
and no evidence holds the control capability.

Decoder, helper, and direct control-session tests remain useful supplemental
coverage for protocol boundaries and guest implementation details. They do not
replace or establish support through the public `nvx.py sandbox` interface.

`run --outcome-report PATH` and one-shot `sandbox run --outcome-report PATH`
forward OpenVMM's bounded local JSON report. Managed `sandbox exec` writes only
the operation, bounded result category, numeric status, and an opaque operation
ID to its requested report; stdout, stderr, arguments, environment values, and
credentials remain excluded. `sandbox stop` waits for OpenVMM teardown and
retains the latest VM-level report as `outcome.json` in the state directory.
Neither OpenVMM nor NVX uploads these files.
