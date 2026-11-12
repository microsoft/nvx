# Run

Build [`nvx-rs`](usage.md#nvx-rs), which runs the microVMs and sandboxes on
this page, with Cargo, and run it:

```bash
cargo build --release --locked --features cli \
  --manifest-path nvx-rs/Cargo.toml --target-dir nvx-rs/target
nvx-rs/target/release/nvx-rs run
```

The first run of a Git checkout that holds no artifacts downloads and
installs the host platform's package from the release for `HEAD`, or from its
nearest released first-parent ancestor when `HEAD` has no such package: the
packaged kernel and initramfs under `build/` and OpenVMM under
`openvmm/target/release/`, so neither needs a local build.
[`nvx-rs download`](usage.md#download) installs it explicitly, replacing the
artifacts of the checkout, as after you pull newer commits. Set `GH_TOKEN` to
a token with contents read access when downloading from a private repository.
On Linux, pass `--hypervisor mshv` to use the MSHV package. The examples on
this page use the POSIX spelling; on Windows, run
`nvx-rs\target\release\nvx-rs.exe` instead.

The CLI chooses WHP on Windows and KVM on Linux:

```bash
nvx-rs/target/release/nvx-rs run
```

A successful Alpine boot prints both `ALPINE-MICROVM-BOOT-OK` and
`NVX-GUEST-BOOT-OK: alpine`. The common init first requires procfs on `/proc`
and sysfs on `/sys`. If either mount fails and the expected filesystem is
not already there, it reports `NVX-INIT-ERROR` and exits the VM with status 1
before the time service and boot-completion banner.

Boot Ubuntu userland with the same NVX kernel:

```bash
nvx-rs/target/release/nvx-rs run --guest ubuntu
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
nvx-rs/target/release/nvx-rs run \
  --memory-mib 256 \
  --net 10.0.0.2/24 \
  --network-profile portable \
  --cmdline "quiet loglevel=0"
```

Networking requires the explicit `portable` capability profile. It uses the
same in-process data plane on KVM, MSHV, and WHP; a cold boot that omits
either `--net` or `--network-profile portable` is rejected before OpenVMM
starts. A restore takes its NIC, with its addresses, from the snapshot
instead, and installs a network policy of its own on it; see
[`snapshot restore`](usage.md#snapshot-restore).

Select an explicit processor count after building the matching specialized guest kernel:

```bash
nvx-rs/target/release/nvx-rs run --machine microvm --processors 8
```

The microVM uses fixed device topology, reserves a shared interrupt-status page, and uses
shared-status edge-triggered virtio interrupts with 1, 2, 4, or 8 vCPUs.

## Run OpenVMM directly

The platform release archives are self-contained: their `bin/nvx-rs` runs the
commands on this page on the archive's artifacts, and neither it nor
`scripts/nvx.py` is required to run OpenVMM. Extract the archive that matches
the host and run the following command from its `nvx-VERSION-PLATFORM`
directory. For Linux/KVM:

```bash
./bin/openvmm \
  --single-process \
  --machine microvm \
  --processors 1 \
  --hypervisor kvm \
  --memory 128M \
  --kernel guest/vmlinux \
  --initrd guest/initramfs.cpio.gz \
  --virtio-console console
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
  --initrd guest\initramfs.cpio.gz `
  --virtio-console console
```

`--virtio-console console` attaches the guest's console, the boot console
`hvc1`, to the terminal, as `nvx-rs run` does.
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
connections do not. With ingress `allow`, clients on the host's private
networks reach the guest's listeners, as described in
[Private-network ingress](#private-network-ingress).

The NIC is dual-stack. The guest's IPv6 address embeds its IPv4 address in the
unique local prefix `fd00::/96`, with a prefix of 96 plus the IPv4 prefix, and
its IPv6 gateway embeds the IPv4 gateway in the same way: `10.0.0.2/24` gives
the guest `fd00::a00:2/120` behind the gateway `fd00::a00:1`. The guest
configures both identities statically; it neither solicits router
advertisements nor autoconfigures addresses from them, so its only other IPv6
address is the kernel's link-local one in `fe80::/64`. Like the IPv4 gateway,
the IPv6 gateway answers ICMPv6 echo requests and maps TCP and UDP flows onto
host loopback, here `::1`, when host-loopback access is allowed. Because the
guest's only IPv6 source address beyond its link is a unique local one,
standard address selection prefers IPv4 for destinations that have both.

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

Rules match IPv4 or IPv6 addresses or CIDRs and may select one protocol: `tcp`,
`udp`, or `icmp`. A TCP or UDP rule may add one destination port, or an
inclusive range of them written `FIRST-LAST`; without a port, it matches every
port of that protocol. Ports are `1` through `65535`, and a range cannot end
below its first port. ICMP rules take no port, and an IPv6 ICMP rule matches
ICMPv6. For example, `192.0.2.0/24:udp` matches UDP on every port,
`192.0.2.1:tcp:8000-8010` matches TCP ports 8000 through 8010 but not UDP,
`192.0.2.1:icmp` matches ICMP but not TCP or UDP, and `2001:db8::/32:tcp:443`
matches TCP port 443 on that IPv6 network. Deny matches take precedence over
allow matches, so adding `--network-egress-deny 192.0.2.1:tcp:8005` keeps port
8005 blocked inside that range.

A rule matches only destinations of its own address family, so IPv4 rules never
admit IPv6 traffic, or the reverse, and a `deny` default blocks every IPv6
destination that no IPv6 rule allows. Use `0.0.0.0/0` and `::/0` to match
every destination of one family. Like ARP for the IPv4 gateway, Neighbor
Discovery for the IPv6 gateway is allowed under an `allow` default or with any
IPv6 allow rule. The gateway never forwards IPv6 packets that carry extension
headers or that target IPv4-mapped addresses. The guest names the IPv4 gateway
as its DNS server when the policy allows TCP or UDP port 53 to it, and
otherwise the IPv6 gateway when the policy allows DNS to that one.

NVX can lower protocol selectors, inclusive TCP/UDP port ranges, and rule-local
IPv4 or IPv6 exclusions to those native rules:

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
    },
    {
      "to": [
        {
          "cidr": "2001:db8::/48",
          "except": ["2001:db8::/56"]
        }
      ],
      "ports": [
        {
          "protocol": "tcp",
          "port": 443
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

Pass the file with `--network-egress-policy-file PATH` on `run`,
`sandbox run`, or `sandbox provision`. An explicit `--network-egress allow` or
`deny` is required. The file option cannot be mixed with
`--network-egress-allow` or `--network-egress-deny`.

The root accepts only `allow` and `deny` arrays. Each element uses the MXC
`NetworkRule` shape: optional `to` and `ports` arrays. Omitting `to` matches
every IPv4 and IPv6 destination. Each `to` entry requires one IPv4 or IPv6
`cidr`; optional `except` entries must be CIDRs of the same family contained by
that parent, and the entries of each family lower to native rules of that
family. Host bits are normalized like the native CIDR syntax: `10.0.0.5/24`
means `10.0.0.0/24`, not one host. Use `/32` to select one IPv4 address and
`/128` to select one IPv6 address. IPv6 scope IDs are rejected before host
bits are normalized, so `fe80::1%eth0/64` is an error, not `fe80::/64`.

Omitting `ports` matches every transport supported by the native rule. Each
port selector follows MXC: `protocol` defaults to `any`. Without `port`, `tcp`
and `udp` match every port of that protocol, `icmp` matches ICMP alone, or
ICMPv6 for IPv6 destinations, and `any` matches every protocol. A `port` in
`1..65535` applies to `tcp`, `udp`, or `any`, which expands to TCP and UDP on
that port but not ICMP. Optional inclusive `endPort` requires `port` and cannot
be below it.

Policy files are at most 1 MiB of UTF-8 JSON, as RFC 8259 defines it: `NaN`,
`Infinity`, numbers beyond the range of a double, duplicate properties, and
arrays and objects nested more than 32 deep are rejected, as are unknown
fields and explicit `null` protocol values. A rejection names the field path,
such as `allow[0].ports[0].endPort`, or the file, and the reason. The previous
flat `cidr`/`except`/`protocol`/`port` rule form remains accepted for
compatibility, with an IPv4 or IPv6 `cidr`, but new policy files should use the
MXC shape.

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
more than 256 allow rules or 256 deny rules, counting both families together;
protocol `any` with a port or a port range lowers to one TCP and one UDP rule.
NVX rejects oversized expansions before launch rather than truncating or
widening them. Managed provision stores the validated lowered rules in sandbox
state, so later starts do not reread a mutable source policy file.

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

Explicit `--host-loopback allow` opens access between the guest and host
loopback in both directions without a port list, and
`--host-loopback-forward` publishes further localhost ports; see
[Host-loopback access](#host-loopback-access). Omitting `--host-loopback`
preserves the existing guest-to-host mapping without publishing guest ports.
Guest-originated traffic remains subject to egress policy.

With the mapping, explicit or kept, a guest reaches every host-loopback TCP
listener that its egress policy permits, another VM's included. A microVM's
boot console authenticates no client, so OpenVMM refuses a TCP endpoint for
it, `listen=tcp:` or `connect=tcp:`, and a snapshot whose boot console has
one: give `--virtio-console` a Unix socket or, on Windows, a named pipe in the
`//./pipe/openvmm-microvm-NAME` namespace, as in `listen=PATH`; see
[Boot Console](design/3.3-consoles.md#331-boot-console).

Most options of a cold boot of `nvx-rs run` pass through unchanged:
`--machine`, `--processors`, `--cpu-profile`, `--net`, and
`--network-profile`. The `nvx-rs run` performs these translations and
additions:

| `nvx-rs run` | Direct OpenVMM option |
| --- | --- |
| `--guest alpine` | `--initrd .../initramfs.cpio.gz` |
| `--guest ubuntu` | `--initrd .../initramfs-ubuntu.cpio.gz` and a 512 MiB default |
| `--guest azurelinux` | `--initrd .../initramfs-azurelinux.cpio.gz` and a 512 MiB default |
| `--hypervisor auto` | `--hypervisor kvm` on Linux or `--hypervisor whp` on Windows |
| `--memory-mib N` | `--memory NM` |
| `--memory-capacity-mib N` | `--memory-capacity NM` |
| `--mount` | `--mount-aggregate /run/nvx/hostfs/root`, with one `--mount-child` for each outermost mapped directory and each mapped file outside them, and `--mount-write` for a read-write `--mount` inside a read-only one; see [virtio-fs host mapping](#virtio-fs-host-mapping) |
| `--cmdline TEXT` | `--cmdline` with `TEXT` among the tokens of the guest's view, mode, and shares, which NVX renders |
| Every cold boot | `--virtio-console listen=ENDPOINT`, whose client, `nvx-rs run`, connects the guest's console, the boot console `hvc1`, to the terminal, and the `--microvm-control-console`, `--microvm-control-auth-stdin`, `--microvm-report`, and `--guest-shutdown-action exit` that every launch of NVX passes |
| `--dry-run` | No equivalent; this only prints the generated command |

`nvx-rs run --restore-snapshot NAME --store-dir PATH --state-dir PATH` restores
only a named snapshot that `nvx-rs snapshot capture` published, once, as
`nvx-rs snapshot restore` does, and refuses a raw OpenVMM snapshot directory;
see [`run`](usage.md#run). OpenVMM itself restores a raw snapshot, as below.

Always include `--single-process`. Include `--virtio-console console` to type
into the guest's shell on a fresh boot: without a boot console, the guest's
console is the byte channel, `hvc0`, which takes no input, so OpenVMM discards
the terminal's keystrokes, though Ctrl-Q still opens its REPL. A restore keeps
the console of its snapshot. OpenVMM requires `--virtio-console console` again
to restore a snapshot of a boot that passed it, and refuses `--virtio-console`
for a snapshot without a boot console. When restoring, omit `--memory`,
`--kernel`, `--initrd`, and `--net`, whose addresses the snapshot keeps; see
[Networked snapshots](#networked-snapshots). Every restore gives the guest
fresh entropy through the time ABI's restore packet. For example, for a
snapshot of a boot with `--virtio-console console`:

```bash
./bin/openvmm \
  --single-process \
  --machine microvm \
  --processors 8 \
  --hypervisor kvm \
  --restore-snapshot /var/lib/nvx/snapshot \
  --restore-processors 4 \
  --restore-memory 1024M \
  --restore-ready-path /run/nvx/restore-ready.sock \
  --virtio-console console
```

### Private-network ingress

Ingress `allow` lets clients on the host's private networks reach every TCP
and UDP port on which the guest listens, without a port list:

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
  --network-ingress allow
```

OpenVMM appends `virtnet_ingress=allow` to the kernel command line, and the
guest's init then starts `nvx-ingress`, which reports the guest's listening
TCP sockets and its unconnected UDP sockets to the gateway, except those bound
to a loopback address or an IPv6 link-local one. It reports a change within
100 milliseconds, or about a second for a new UDP socket, and repeats the
report every 2 seconds. Only guest root can send the report, from a privileged
port, and the egress policy does not apply to it. For each reported port,
OpenVMM listens on the same port of every host address, of each family on which
the guest listens, and relays connections and datagrams to the guest.

OpenVMM admits a client only when its address is not globally reachable, such
as one in `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `100.64.0.0/10`,
`169.254.0.0/16`, or `fc00::/7`. It resets the TCP connections and drops the
UDP datagrams of other clients, so internet clients never reach the guest. It
also refuses clients that the guest could not answer: those inside the guest's
own networks, such as `10.0.0.0/24` and `fd00::a00:0/120` for `10.0.0.2/24`,
which the guest would take for its gateway or its neighbors, and IPv6
link-local clients, so choose a guest network that contains no client. The
host itself, at a loopback address or one of its own, counts as host loopback:
OpenVMM admits it only with explicit `--host-loopback allow`. Replies to
admitted clients pass whatever the egress policy, while the guest's own
connections remain subject to it.

Publication is best effort. OpenVMM cannot publish a port that a host process
holds, or on Linux a privileged port that it may not bind; it logs a warning
and retries with the next report. An explicit `--host-loopback-forward` to the
same guest port keeps working beside the publication. The guest reports at
most 64 ports. A TCP socket listening on `::` counts for IPv4 too unless it is
IPv6-only, and a UDP socket on `::` unless the guest sets
`net.ipv6.bindv6only`. Clients on other hosts also need the host's firewall to
admit them to OpenVMM, for example an inbound rule for `openvmm.exe` on
Windows. Snapshot capture rejects ingress `allow`, because the published
listeners are live host attachments, and so does a restore, unless it
installs a new policy on a [NIC template](#networked-snapshots).

### Host-loopback access

Explicit `--host-loopback allow` opens host loopback to the guest and the guest
to host loopback, without a port list:

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
  --host-loopback allow
```

The guest reaches host loopback services through its gateway, subject to the
egress policy: a host service on `127.0.0.1:8080` at `10.0.0.1:8080`, and one
on `[::1]:8080` at `[fd00::a00:1]:8080`. Their replies come from the gateway
address and port that the guest addressed, so connected UDP sockets receive
them too. The guest sees other host peers, such as clients of its published
ports, at addresses that OpenVMM assigns inside its own network, and the egress
policy applies to its traffic to them as it applies at the gateway, so a rule
that denies a gateway port denies it at those addresses too. In the other
direction, OpenVMM appends `virtnet_ingress=host-loopback` to the kernel
command line unless ingress is `allow`, and the guest's `nvx-ingress` reports
its listeners as it does for
[private-network ingress](#private-network-ingress). For each reported port,
OpenVMM listens on the same port of host `127.0.0.1` or `::1`, for each family
on which the guest listens, and admits only the host, so neither the host's
networks nor the internet reach the guest. Replies to the host pass whatever
the egress policy. A guest listener bound to `127.0.0.1` or `::1` alone is
never reported and stays private to the guest. A port that a host process
holds, on loopback or on every host address, stays with that process: OpenVMM
logs a warning and retries with the next report. A host process that later
binds every host address at a published port fails on Linux; on Windows,
unless it asks for exclusive use of the port, it succeeds but gets no loopback
traffic, which stays with the guest, as it would with any host process
listening on loopback alone. With ingress `allow` as well, the listeners that
OpenVMM publishes on every host address admit the host instead.

Explicit `--host-loopback allow` needs a guest network of `/29` or larger.
OpenVMM presents host peers at addresses inside the guest's network, and a
`/30` holds only the gateway and the guest, so OpenVMM rejects explicit
`allow` on a `/30` before it starts, as the NVX Rust API and `nvx-rs sandbox`
do, and also at a restore that installs a new policy on a
[NIC template](#networked-snapshots) captured on one.

Repeat `--host-loopback-forward tcp:HOST_PORT:GUEST_PORT` or its UDP form, at
most 64 times, to publish further localhost ports at guest ports of your
choice, such as host `127.0.0.1:18080` to guest port `8080`; forwards need
explicit `--host-loopback allow`. Each forward of a protocol needs its own host
port and its own guest port: OpenVMM rejects a repeated host port before it
starts and fails to start with a repeated guest port. The guest answers
forwarded traffic at addresses that OpenVMM assigns inside its own network.
OpenVMM lets the replies on a forwarded TCP connection through whatever the
egress policy, but the egress policy filters UDP replies, so forwarded UDP
ports need an egress policy that lets them out. Snapshot capture rejects
explicit `--host-loopback allow`, because its listeners are live host
attachments, and so does a restore, unless it installs a new policy on a
[NIC template](#networked-snapshots).

### Networked snapshots

A snapshot captured with `--net` keeps its NIC's addresses and the digest of
its network policy. Restore it with `--network-profile portable`, without
`--net`, and with the network policy options of the capture, which OpenVMM
checks against that digest before the guest runs:

```bash
./bin/openvmm \
  --single-process \
  --machine microvm \
  --processors 1 \
  --hypervisor kvm \
  --restore-snapshot /var/lib/nvx/snapshot \
  --network-profile portable \
  --network-egress deny \
  --network-egress-allow 10.0.0.1/32:tcp:443
```

The restore fails unless its policy matches the captured one: the default
egress, the same rules, whether `--network-egress-allow` and
`--network-egress-deny` give them or a policy file compiles to them,
`--host-loopback`, and `--network-proxy`. Such a restore therefore cannot
change the policy, and, like a capture, it rejects `--network-ingress allow`,
explicit `--host-loopback allow`, and `--host-loopback-forward`, whose
published ports are live host attachments. The restored guest keeps the
network configuration that its boot applied, such as its DNS server; flows
from before the capture do not survive the restore, and new traffic opens
fresh ones. A snapshot captured without `--net` takes no network option at
restore.

A snapshot whose NIC was captured with a complete deny-all policy,
`--network-egress deny` and `--host-loopback deny` without egress rules,
`--network-proxy`, or ingress, is a NIC template, whose restore may install a
policy of its own instead: pass `--restore-network-policy` with the network
options of the new policy, which may include `--network-ingress allow`,
explicit `--host-loopback allow`, and forwards, for example:

```bash
./bin/openvmm \
  --single-process \
  --machine microvm \
  --processors 1 \
  --hypervisor kvm \
  --restore-snapshot /var/lib/nvx/template \
  --restore-network-policy \
  --network-profile portable \
  --network-egress deny \
  --network-egress-allow 10.0.0.1/32:udp:53 \
  --network-egress-allow 0.0.0.0/0:tcp:443 \
  --host-loopback allow \
  --host-loopback-forward tcp:18080:8080
```

OpenVMM validates the new policy as it validates a cold boot's, binds it to the
NIC's captured addresses, which the restored guest keeps, and installs it, with
its forwards, before any vCPU runs; a forward whose host port is busy fails the
restore. A restore without connectivity passes the template's own deny-all. The
template's guest booted under the deny-all, which names no DNS server and
starts no `nvx-ingress`, so OpenVMM hands the guest the DNS server of the new
policy, and whether to announce its listeners, in the network settings of the
restore packet (see [Restore
Packet](design/5.5-restore-protocol.md#555-restore-packet)). Before the host
releases its input, the guest's repair names that server in `/etc/resolv.conf`
and starts `nvx-ingress`, as the guest's init does at a cold boot under that
policy; a failure of either fails the restore. Such a snapshot may hold a
workload that has already run, but under the deny-all that workload had no
connectivity, so nothing in the snapshot depends on the policy, and the restore
grants it connectivity that it never had. OpenVMM rejects the option for a cold
boot, for a snapshot without a NIC or whose NIC was captured with any other
policy, and for a `workload-start` or `instance-checkpoint` snapshot. A
[platform NIC template](#platform-nic-templates) takes the option too, and so
does each restore of a named snapshot, a platform snapshot whose NIC
`nvx-rs snapshot capture --net` captured under the complete deny-all policy,
through `nvx-rs run --restore-snapshot` or `nvx-rs snapshot restore`, which
install the policy of their network options, and whose guest learns its DNS
server and whether to announce its listeners from its activation, as
[`snapshot restore`](usage.md#snapshot-restore) describes.

### Platform NIC templates

A platform snapshot whose NIC was captured with a complete deny-all policy,
`--network-egress deny` and `--host-loopback deny` without egress rules,
`--network-proxy`, or ingress, is a NIC template: each restore may install a
policy of its own with `--restore-network-policy`, for example:

```bash
./bin/openvmm \
  --single-process \
  --machine microvm \
  --hypervisor kvm \
  --restore-snapshot /var/lib/nvx/template \
  --microvm-sandbox-block distro:file:/var/lib/nvx/image.erofs,ro \
  --restore-absent-scratch \
  --network-profile portable \
  --restore-network-policy \
  --network-egress deny \
  --network-egress-allow 0.0.0.0/0:tcp:443 \
  --host-loopback allow \
  --host-loopback-forward tcp:18080:8080
```

OpenVMM validates the restore's policy as it validates a cold boot's, binds it
to the NIC's captured addresses, which the clone keeps, and installs it, with
its forwards, before any vCPU runs; a forward whose host port is busy fails the
restore. A clone without connectivity passes the template's own deny-all. The
guest keeps its captured kernel command line, which under a deny-all policy
names no DNS server and starts no `nvx-ingress`, so a clone learns its DNS
server and whether to report its listeners from its activation after the
restore, and its restore packet carries no network settings. OpenVMM rejects
the option for a cold boot, for a `workload-start` or `instance-checkpoint`
snapshot or one without a NIC, and for a NIC captured with any other policy,
and every restore without it requires the captured policy, as [Networked
snapshots](#networked-snapshots) describes.

### Platform filesystem exports

A platform snapshot captured without `--mount` or `--mount-aggregate` leaves
the virtio-fs slot dormant. Each restore may bind one directory to it with
`--mount`, or an aggregate of directories and files with `--mount-aggregate`
and its `--mount-child` options, for example:

```bash
./bin/openvmm \
  --single-process \
  --machine microvm \
  --hypervisor kvm \
  --restore-snapshot /var/lib/nvx/template \
  --microvm-sandbox-block distro:file:/var/lib/nvx/image.erofs,ro \
  --restore-absent-scratch \
  --mount-aggregate /run/nvx/shares \
  --mount-child 0,/srv/checkout,rw \
  --mount-child 1,/opt/hostedtoolcache,ro \
  --mount-child 2,/srv/app/config/settings.json,ro,file \
  --mount-deny /srv/checkout/.git/credentials
```

OpenVMM validates the aggregate's children, their `--mount-deny`,
`--mount-allow`, and `--mount-write` paths, and the `--mount-owner` mode as it
validates a cold boot's, and attaches the aggregate before any vCPU runs. The
slot's device and the guest's kernel command line stay as captured, without a
`virtfs_*` or `nvx_share=` token, so the guest must learn of the export from
its host, mount tag `microvm` itself, and bind each child where it needs it.
The dormant slot of a snapshot of another tier, or without a tier, still
binds only a `--mount`, and a snapshot captured with an export requires that
export again.

## Snapshots of an earlier OpenVMM

OpenVMM's v4 microVM restarted the versions of its contracts: microVM ABI 1,
boot layout 1, time ABI 1, restore packet 1, and snapshot manifest 2, whose
version 1 is upstream OpenVMM's. It reads no snapshot of an earlier OpenVMM,
whose manifest it refuses with `E_SNAPSHOT_VERSION` by its version, or by its
missing format magic for the earliest manifests of version 2, before it
decodes the rest: capture such a snapshot again, as [Upgrades](#upgrades) says
of every snapshot of other artifacts.

The machine is the one that the earlier OpenVMM called microVM ABI 3: no
virtio device offers `VIRTIO_F_ACCESS_PLATFORM`, no console offers its size
(see
[Guest-Visible Capabilities](design/3.4-block-storage.md#342-guest-visible-capabilities)),
and each virtio device has an IOAPIC input of its own past the ISA IRQs, the
same on every backend (see
[Interrupt Routing](design/2.4-interrupt-and-io-architecture.md#242-interrupt-routing)).

## Snapshot restore readiness

Restore an existing microVM snapshot with OpenVMM and an optional
host-readiness endpoint:

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

`nvx-rs run --restore-snapshot`, `nvx-rs snapshot restore`, and each
`nvx-rs sandbox start` of a restored sandbox own the endpoint of their
restore, whose event they accept only from the VM's own OpenVMM process, and
restore the processors that their snapshot had online.

The endpoint must already be listening. OpenVMM connects to a Unix domain
socket on Linux or a `//./pipe/...` named pipe on Windows and writes exactly
`OPENVMM_RESTORE_READY_V1\n` once it has verified the snapshot, resolved its
attachments, and started the VM's workers, and before it releases a restored
vCPU, so the event precedes the guest's repair. A restore that requires the
guest's acknowledgement gates host input from before the event until the
guest's repair acknowledges the restore. With
`--restore-processors`, the snapshot keeps its immutable eight-vCPU capacity;
the source must have captured a canonical `maxcpus` boot-online prefix, and the
guest's repair onlines exactly the requested prefix while host input stays
gated, before it acknowledges the restore. Targets are limited to 1, 2, 4, or 8
and cannot be below the captured boot-online count or above capacity. Platform
snapshots opt in the same way, so each clone of one platform capture can bring
its own prefix online. Legacy snapshots reject the option. The
peer must accept and read while startup is in progress; Windows flush
completion waits for the named-pipe peer to consume the frame. Failure to
write the complete event aborts and tears down the restore.

For restore-time memory expansion, capture a fresh snapshot with
`--memory-mib 512 --memory-capacity-mib 2048`, then select a target with
`--restore-memory 512M`, `1024M`, or `2048M`. The snapshot's `memory.bin`
remains exactly 512 MiB; selected expansion ranges receive fresh per-launch
backing before any restored vCPU runs, and the guest's repair onlines them
while host input stays gated, before it acknowledges the restore.

## virtio-fs host mapping

The microVM has one virtio-fs slot, with the fixed tag `microvm`. On a cold
boot with `--mount`, the initramfs mounts the mapping automatically:

```bash
nvx-rs/target/release/nvx-rs run --mount "/mnt/host,/absolute/host/share,rw"
```

PowerShell example:

```powershell
nvx-rs\target\release\nvx-rs.exe run `
  --mount "/mnt/host,C:\Users\me\microvm-share,rw"
```

Use `ro` for read-only access. The guest target must be an absolute, canonical
Linux path other than `/`, without backslashes or `=`. The kernel command line
carries it, so it must also stay one kernel token: it cannot contain
whitespace, control characters, double quotes, or any UTF-8 byte that Linux
treats as whitespace, such as the `0xa0` in `à`. Linux would split the token at
such a byte, and a double quote would hide the virtio-mmio devices that OpenVMM
describes after the target from the guest's kernel. The target ends at the
first comma, and the mode follows the last comma, so a host path may contain
commas when the mode is given, which it then requires:

```powershell
nvx-rs\target\release\nvx-rs.exe run `
  --mount "/mnt/alpha,C:\projects\alpha,beta,rw" `
  --mount "/mnt/reports,C:\reports\2026, final,ro" `
  --mount-deny "C:\projects\alpha,beta\secrets" `
  --mount-deny "C:\reports\2026, final\secrets"
```

Repeat `--mount` to attach more directories, each with its own target and
access mode, for example a read-write workspace next to a read-only tool
cache:

```bash
nvx-rs/target/release/nvx-rs run \
  --mount "/workspace,/srv/checkout,rw" \
  --mount "/opt/hostedtoolcache,/opt/hostedtoolcache,ro"
```

A `--mount` may also share one regular host file. The guest then reaches that
file at its target, but neither its directory nor its siblings, for example a
read-only settings file next to a read-write results file:

```bash
nvx-rs/target/release/nvx-rs run \
  --mount "/config/settings.json,/srv/app/config/settings.json,ro" \
  --mount "/results/output.txt,/srv/app/results/output.txt,rw"
```

NVX attaches the slot as an aggregate, even for one mapping: OpenVMM's
`--mount-aggregate /run/nvx/hostfs/root` with one
`--mount-child INDEX,HOST_PATH,MODE[,file]` per child, numbered from 0. The
children are the outermost mapped directories and the mapped files outside
them, so a `--mount` of a directory again, or of a path inside a mapped
directory, maps a part of that directory's child rather than a child of its
own. OpenVMM's `file` flag marks a child that is a shared file, so OpenVMM
refuses a directory that replaced the file rather than export all of it. A
child is read-write only if it holds a read-write mapping, and OpenVMM then
limits its writes to its read-write mappings, which NVX passes as
`--mount-write` paths when they lie inside a read-only one. A read-only
mapping inside a writable part of its child, such as inside a read-write
mapping without `--mount-write` paths, is rejected before boot, since OpenVMM
cannot keep a path read-only there. The initramfs mounts the
aggregate at `/run/nvx/hostfs/root`, whose root lists one entry per child and
only root can enter, and binds each mapping at its target with its mode and
`nosuid,nodev`, as an `nvx_share=SOURCE,TARGET,MODE[,file]` token on the
kernel command line describes: `SOURCE` is the index of the mapping's child,
followed by the mapping's path inside it, if any, such as `0/single.txt`. The
`file` suffix marks a shared file, which the initramfs binds at a regular file
that it creates at the target, along with the target's missing directories; an
existing target must already be a regular file.
OpenVMM enforces each mapping's access on the host, so a read-only mapping
rejects writes with `EROFS`, except inside a read-write mapping that it
contains, even if guest root remounts its bind read-write or writes through
`/run/nvx/hostfs/root`. A rename between children fails with `EXDEV`, and so
does a hard link, unless its destination is read-only, which fails with
`EROFS` first. OpenVMM serves a file child alone: the guest cannot list,
inspect, or change the file's directory or reach its siblings, a lookup below
the file fails with `ENOTDIR`, and a read-write file can be rewritten in place
but not replaced, renamed, or removed. OpenVMM follows the file's name, so the
guest sees the file that host software, such as an editor, saves over it, and
requests fail with `EACCES` while anything but a regular file holds the name.
A mapped file inside a mapped directory is part of that directory's child
instead, whose mapping reaches the file's siblings. Guest targets that equal or
contain one another are rejected before boot.
The number of mappings is bounded only by the 1024-byte
budget that their tokens and `--cmdline` share on the kernel command line.
A repeatable `--mount-deny HOST_PATH` hides an existing file or directory
inside a mapped root. With several mappings, or a shared file, each
`--mount-deny`, `--mount-allow`, and `--mount-write` path must be absolute, and
it applies to the child whose root contains it, whichever of the child's
mappings reaches it. A file child takes none of them, because its mode applies
to the whole file. Denied
names are omitted from directory listings and remain
inaccessible through `..`, a symlink/junction, or another mount of the same
virtio-fs device. Names may contain spaces, but not begin or end with one.
`--mount-deny` may name a mapping's root itself when `--mount-allow` paths
inside it remain: the root then lists only the way to them. Unsafe, external,
duplicate, overlapping, and nested-mount
rules are rejected before boot. A repeatable `--mount-allow HOST_PATH`
exposes a path inside a denied path again, and a repeatable
`--mount-write HOST_PATH` makes a path one of the only writable parts of an
`rw` mapping; see [Access policy](#access-policy).
In an `rw` mapping, the guest can create symbolic links, and OpenVMM stores
each target exactly as given. The guest resolves links in its own namespace;
OpenVMM never follows a link while resolving a host path, so a link to an
absolute host path, outside the root, or into a denied path cannot reach host
data. An `ro` mapping rejects link creation with `EROFS`. On Windows, links
are WSL-style reparse points, which Windows path resolution never follows.
Treat links in a writable share as untrusted when host software later reads
the directory.
On a Linux host, the guest can also swap two names in an `rw` mapping
atomically with `renameat2(RENAME_EXCHANGE)`, as `exch` does. A Windows host
rejects rename flags with `EINVAL` and changes neither name.
By default, OpenVMM performs every guest operation on the mapping as its own
user. On a Linux host, `--mount-owner caller` instead performs each one as the
guest caller's UID and GID and squashes guest root to the owner of the shared
host directory or file, so files that guest root creates are owned by that
owner rather than by root or OpenVMM; see [File ownership](#file-ownership).

`nvx-rs` snapshots no VM with mappings: `snapshot capture` captures a VM
without them, and `run --restore-snapshot` refuses the share options. A
restored sandbox maps the shares of its own `--mount` options, which
[`snapshot restore`](usage.md#snapshot-restore) takes, once its guest is
ready, as [Live host shares](#live-host-shares) describes.

## Experimental single-workload sandbox

The `sandbox` command of [`nvx-rs`](usage.md#nvx-rs) launches the microVM
with one distro layer, a compressed EROFS image or a GPT disk, under a writable
upper: by default one preformatted ext4 scratch image, or a tmpfs in guest
memory with `--upper ram`, as [Overlay upper](#overlay-upper) describes. Build
the distro layer with `nvx.py`:

```bash
python3 scripts/nvx.py build-distro-layer \
  --guest ubuntu \
  --output build/ubuntu-distro.erofs
```

Read the deterministic UUID from
`build/ubuntu-distro.erofs.manifest.json`, create scratch, for example with
[`build-scratch-template`](usage.md#build-scratch-template), which formats the
sandbox scratch template at the path that `--output` gives, or with
`mkfs.ext4` itself, then run an Ubuntu workload in a sandbox that the Alpine
control initramfs boots and nvx-init manages:

```bash
python3 scripts/nvx.py build-scratch-template --output /var/lib/nvx/scratch.ext4
nvx-rs/target/release/nvx-rs sandbox \
  --layer distro,build/ubuntu-distro.erofs,11111111-1111-1111-1111-111111111111 \
  --scratch /var/lib/nvx/scratch.ext4 \
  --entrypoint /bin/sh --arg=-c --arg='cat /etc/os-release' \
  --workload-user 65534:65534 \
  --memory-mib 256
```

`sandbox run`, the default operation, keeps no state. It launches the VM that
a `provision` and a `start` of its options would launch, in a fresh temporary
VM directory, and prints its OpenVMM command as `>> COMMAND`. Once nvx-init is
ready and has mapped the shares, the run executes its workload once over the
control session, as [`sandbox exec`](#managed-lifecycle) does, with the same
`--cwd`, `--environment`, `--environment-file`,
`--inherit-default-environment`, and `--exec-timeout-ms` options, and streams
its standard input, output and error as `sandbox exec` does. It then asks
nvx-init to stop the VM, terminates OpenVMM if it does not exit within
`--timeout` seconds, warning that it did, and removes the VM directory, and
finally exits with the workload's status. A VM that exits before nvx-init is
ready, as when the guest
refuses the workload identity, fails the run as it fails a `start`, with the
category and status of the outcome report that OpenVMM published, such as
`guest-exit status 125`. `--outcome-report` forwards that report, of the VM's
stop, with the permissions that OpenVMM gave it. Ctrl-C ends the VM, and the
run then exits with status 130. `--dry-run` prints the command and launches
nothing.

CI runs `/sbin/nvx-sandbox-smoke`, with `--arg init`, as below, as the workload
of `sandbox run` and `sandbox exec`, to verify the security profile of the
executions that nvx-init runs. Before any other check, it requires:

- the fixed `65534:65534` identity as the real, effective, saved, and
  file-system IDs, no supplementary groups, and `HOME=/nonexistent`;
- empty inheritable, permitted, effective, bounding, and ambient capability
  sets and `NoNewPrivs: 1`;
- mount, PID, and UTS namespaces other than the guest's initial ones, whose
  inode numbers Linux 6.18 fixes in its UAPI, with the workload as PID 1 of its
  PID namespace and the `--hostname` value as its host name;
- exactly one mount at each of `/`, `/proc`, `/sys`, `/dev`, `/dev/pts`, and
  `/dev/shm`, none of which propagates to another namespace: an overlay root
  over the EROFS layers, with `metacopy=on` and `xino=on`, whose upper
  directory is on the upper that the kernel command line declares, scratch, or
  a tmpfs for `nvx_overlay_upper=ramfs`, `nosuid,nodev,noexec`
  procfs, read-only sysfs, and a private `/dev` tmpfs that holds only `fd`,
  `full`, `null`, `ptmx`, `pts`, `random`, `shm`, `stderr`, `stdin`, `stdout`,
  `tty`, `urandom`, and `zero`, with its own devpts instance;
- a refused `mknod` of a character device;
- a machine ID of 32 lowercase hexadecimal digits at `/etc/machine-id`, a
  read-only `nosuid,nodev` bind from tmpfs that the workload cannot replace;
- no port I/O: a port read without the permission ends the reader with
  `SIGSEGV`, and a request for the permission fails;
- the agent-owned `/container` cgroup; and
- Ubuntu image files that the workload cannot modify, and a `/tmp` write that
  lands in the overlay, on its upper.

A managed workload that mounted into the agent's namespace would stack its
runtime mounts on those of earlier requests, so the single-mount check also
covers repeated managed execs. With `--arg init` as its first argument, for an
execution of `sandbox run`, or in a sandbox that `sandbox start` started, which
nvx-init runs as a plain process in the VM's root, it checks the host name,
which nvx-init's kernel command line encodes in hexadecimal, instead of the
namespaces, and no cgroup:
such an execution shares nvx-init's mount, PID, and UTS namespaces and its
cgroup. With `--arg entry`, for a managed execution, it also requires every
mount other than `/dev` and its devpts instance to be `nodev`, as the managed
agent's execution entry and nvx-init's root make them. With
`--arg limits --arg MEMORY_MAX --arg PIDS_MAX`, for a workload whose cgroup has
those limits, it also requires the memory controller to kill an allocation of
twice `MEMORY_MAX` with `SIGKILL` while a 1 MiB allocation succeeds, and the
pids controller to stop the workload at `PIDS_MAX` processes, itself included;
keep twice `MEMORY_MAX` within the guest's free memory. `nvx-rs sandbox` sets no
such limit: the VM is the limit of its workload. With
`--arg TARGET --arg ro|rw`, it also checks a live share at
`TARGET` as described below, including symbolic links in an `rw` share; the
share needs a host-created, world-writable `nvx-links` directory for them.
With `--arg TARGET --arg file-ro|file-rw`, it checks a shared file at
`TARGET`: a `virtiofs` bind with the given mode, alone in its directory,
holding `host-to-guest`, which the workload can neither remove nor rename. It
requires a write to a `file-ro` file to fail with `EROFS`, so the host must let
every identity write the file, and it writes `guest-to-host` to a `file-rw`
file.
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

The layer UUID is the EROFS superblock UUID, not a content digest. Give it in
canonical lowercase hyphenated form; braces, a `urn:uuid:` prefix, uppercase,
and unhyphenated UUIDs are refused. The workload UID and GID use ASCII decimal
digits only, without signs, whitespace, underscores, or Unicode digits, and each
is in `1..4294967294`. The command
validates the files before launch, orders roles independently of option order,
attaches layers read-only, and reserves the writable slot for scratch, which a
RAM upper leaves empty.
Conversion and scratch formatting stay off the start path. The Ubuntu
converter verifies immutable inputs, applies the deny-by-default metadata
policy, creates `/nonexistent` for UID/GID 65534, and invokes `mkfs.erofs`.
Systemd entrypoints are explicitly unsupported and do not relax the non-root,
drop-all-capabilities sandbox policy.

The distro manifest beside the layer is strict UTF-8 JSON. Its `format` is the
integer `1` (not `true`, `1.0`, or an object); non-finite numbers, numbers
beyond the range of a double, and unpaired surrogate escapes are refused even in
unused fields, no object may repeat a field, and arrays and objects nest at most
32 deep, whatever features of `serde_json` other crates of a build enable. The
artifact name, SHA-256 digest, and package metadata still bind it to the layer.

A run is the cold-filesystem bootstrap described in
[the sandbox design](design/4.1-guest-filesystem-and-image-model.md#411-layered-sandbox-root), which nvx-init
manages. It does not expose snapshot capture or restore, which
[named snapshots](#named-snapshots) provide, the configuration region, or
runtime RPC. Guest share targets cannot contain
whitespace, control characters, double quotes, or any UTF-8 byte that Linux
treats as whitespace, including `0xa0`, while the workload's entrypoint and
arguments, which nvx-init receives over the control session, may.
Sandbox `--cmdline` uses Linux's quote and separator rules, and refuses reserved
names even when quoted or written with hyphens instead of underscores. Validation
reads the text that the kernel command line holds, after the generated settings,
and also checks the guest bootstrap's whitespace-only view, so reserved settings
inside quoted values cannot override the generated settings. It also refuses a
`--` or unclosed quote that hides the tokens that OpenVMM appends, and double
quotes, which the VM control console cannot represent.
nvx-init runs the workload as a plain process in the VM's root, with no
capabilities and `no_new_privs`, as it runs every
[managed](#managed-lifecycle) execution. The workload always runs as the fixed
non-root `UID:GID` selected at VM creation (`65534:65534` by default). The
guest verifies that exactly one matching user, its primary group, and its
absolute home directory exist in the assembled root; otherwise the workload is
never started.

### Overlay upper

The overlay keeps the workload's writes in its upper, which `--upper` selects
for `sandbox run` and `sandbox provision`:

- `scratch`, the default, keeps the upper on the ext4 image of `--scratch`,
  which OpenVMM attaches as the `scratch` block device. What the workload
  writes uses host storage, fails with `ENOSPC` when the image is full, and
  survives a managed sandbox's `stop` and next `start`.
- `ram` keeps the upper in guest memory, with no scratch image or block device.
  The kernel command line declares `nvx_overlay_upper=ramfs` and the cap of
  `--ram-upper-percent`, from 1 through 90 percent of the guest's RAM, 50 by
  default, and the guest mounts a tmpfs for the upper, `nosuid` and `nodev`, of
  that size, beyond which a write fails with `ENOSPC`. The
  kernel cannot reclaim its pages while the files exist, since the guest has no
  swap. Nothing that the workload writes outlives its VM: each `start` of a
  managed sandbox begins with an empty upper.

```bash
nvx-rs/target/release/nvx-rs sandbox \
  --layer distro,build/ubuntu-distro.erofs,11111111-1111-1111-1111-111111111111 \
  --upper ram \
  --entrypoint /bin/sh \
  --memory-mib 512
```

`--upper ram` refuses `--scratch`, and only `run` and `provision` take
`--upper`: a provisioned sandbox keeps the upper that its configuration
records. A restore, of [`snapshot restore`](usage.md#snapshot-restore) or
`run --restore-snapshot`, takes `--upper` and `--ram-upper-percent` as
`provision` does, and hands its guest the RAM upper's cap in its activation,
since the snapshot keeps the kernel command line of its capture. OpenVMM
admits the RAM upper of a cold boot, `nvx_overlay_upper=ramfs`, only with
exactly one read-only `distro` block device, and refuses it for a snapshot
capture or restore: a restore over a RAM upper attaches the snapshot's dormant
scratch without a file instead, with OpenVMM's `--restore-absent-scratch`. The
guest of a cold boot refuses a RAM upper beside a scratch block device. With
either upper, the overlay keeps `metacopy=on` and `xino=on`, which
`/sbin/nvx-sandbox-smoke` checks.

The `sandbox-ram-upper` scenario of `test-microvm --backend BACKEND` is the
acceptance for the RAM upper. Like the
[managed lifecycle](#managed-lifecycle) scenarios, it runs `nvx-rs sandbox` as
subprocesses with the Ubuntu layer that `nvx-rs download` installs, but it
attaches no scratch image. It checks that `provision`
refuses `--upper ram` beside `--scratch`, and `start` refuses `--upper`, before
either creates any state. A run of `/sbin/nvx-sandbox-smoke`, which keeps no
state, must print an OpenVMM command with one sandbox block device, the
read-only distro layer, `nvx_overlay_upper=ramfs` in place of the scratch
token, with the default cap, `nvx_ram_upper_percent=50`, and `nvx_sandbox=1`
without an OpenVMM lifecycle option, so that nvx-init manages its VM, and the
probe must pass its checks of an execution of nvx-init over a tmpfs upper. The
run's `--outcome-report`, under the scenario's temporary directory, must report
the success of the VM's stop and a complete teardown, and its `--cmdline` names
that directory, which names its OpenVMM to the scenario's process checks and
cleanup, which find VMs by that directory. A managed sandbox
must record `ram` without a scratch image, pass the probe's managed checks, show
its guest one virtio block device and the RAM upper's token, and read back a
file that an earlier request wrote. After a `stop` and a new `start`, the file
is gone. The scenario's evidence, with a `sandbox-ram-upper-` prefix, holds its
bounded command observations, the runtime record, the output of the run, of
the probe, and of the guest's view, and the outcome reports of the run and of
both stops.

The `sandbox-gpt-layer` scenario of `test-microvm --backend BACKEND` is the
acceptance for a GPT disk, over either upper. It writes a GPT disk in its
temporary directory, whose first partition holds a few bytes and whose second,
its one layer, a guest of the Alpine control initramfs formats as ext4 and
fills with a copy of the tree of the Ubuntu EROFS layer. Then, over a scratch
upper and then a RAM upper, a run of `/sbin/nvx-sandbox-smoke` with
`--layer-format gpt` must print an OpenVMM command with the disk as its
read-only distro block device, and a scratch block device for a scratch upper
alone, whose kernel command line names the disk's GUID,
`nvx_layer=distro,0xd0003000,gpt:GUID`, the upper's token alone,
`nvx_sandbox=1`, and `nvx_mode=managed`. The probe must pass its checks of an
execution of nvx-init, and the run's `--outcome-report` must report the
success of the VM's stop and a complete teardown. A managed sandbox must record
the disk's format and GUID and the upper, pass the probe's managed checks, and
show its guest the same tokens, one virtio block device more for a scratch
upper, and the disk's layer partition as the lower directory of its root,
`/run/nvx/layers/p2`, before it stops with a successful outcome report and a
complete teardown and is deprovisioned. The disk keeps its digest throughout.
The scenario's evidence, with a `sandbox-gpt-layer-` prefix, holds the console
of the guest that wrote the disk, the bounded command observations, the runtime
record, the output of each run, probe, and view of the guest, and the outcome
reports of each run and stop. With `sandbox-lifecycle` and
`managed-exec-config`, which boot the EROFS layer over a scratch upper, and
`sandbox-ram-upper`, CI runs it on KVM, MSHV, and WHP: the CLI's twelve cold
cells, which the [case matrix](acceptance-case-matrix.md#cold-boots) lists.

### Named snapshots

`nvx-rs snapshot capture` captures a VM that nvx-init runs, before any
workload, with a placeholder image, a dormant scratch, and a NIC only if
`--net` gives it one, into a named snapshot of a store, which
`nvx-rs snapshot restore` and `nvx-rs run --restore-snapshot` restore under the
snapshot's lock, with an image, an upper, and a network policy of their own;
see [`snapshot capture`](usage.md#snapshot-capture) and
[`snapshot restore`](usage.md#snapshot-restore). The public `snapshot-core` and
`snapshot-tiers` scenarios of `test-microvm --backend BACKEND` are their
acceptance, with the Ubuntu layer and the scratch template that
`nvx-rs download` installs, and `untiered-network-policy` that of a restore's
network policy. The workload of each restore of the first two is a probe, a
script that `/bin/sh -c` runs, which reports the guest's clock and uptime,
fresh random values, its machine ID, host name, and identity, the lower and
upper directories of its root's overlay, its block devices and network
interfaces, the distribution of its image, and the marker file of its host
share at `/data`, if it has one.

The `snapshot-core` scenario captures one snapshot, whose directory must hold
exactly `key.bin`, `snapshot.json`, and `openvmm/`, each file with one link,
while no directory or OpenVMM process of the capture's VM remains. After a
downtime longer than the capture, two `run --restore-snapshot` restores of the
Ubuntu EROFS layer, in RAM as `65534:65534` and over a fresh copy of the
scratch template as `1:1`, each with a host name of its own, must run as the
restore's identity with its host name, the layer as the root's lower directory
over the upper, the snapshot's two block devices, and no NIC, with a clock
that reads the host's time and an uptime that kept the time before the capture
and counts the downtime, and must report different random values, kernel
UUIDs, machine IDs, and temporary file names. A `snapshot restore` over a
scratch image of half the dormant scratch's length is refused before any VM
starts: its error names the lengths of the requested scratch and of the
dormant one, which attributes the failure to the request, and its sandbox,
which a failed start keeps provisioned, must hold `config.json` and
`sandbox.lock` alone, where a launch leaves OpenVMM's log and outcome report
and the view of the snapshot, until `sandbox deprovision` removes it. A
`run --restore-snapshot` over a zeroed scratch image of the dormant scratch's
length, which the guest cannot mount as its upper, ends its VM before its
workload runs and frees the snapshot's lock, which the restores that follow
take: nvx-init ends the VM with the status of a root that does not assemble,
54, and the run reports the guest's exit with the restore's evidence and what
it attributes the failure to, the request, such as
`OpenVMM exited during startup: guest-exit status 54; restore failed during
Activate: UnusableRoot, attributed to the request`.

Then three `snapshot restore` restores start at once, each with a host name
and a read-only host share of its own, the last over a scratch upper and the
others in RAM, and a workload that ends only once the scenario creates a
`release` file in its share, which the guest reads live. While the first holds
the snapshot's lock, a `run --restore-snapshot` restore that may wait one
second for the lock fails with `sandbox-busy` before its workload runs, and
neither of the other two may link its view of the snapshot, which a restore
does only once it holds the lock. Once the scenario releases the first, one of
the other two takes the lock, and the last may link its view only once the
scenario releases that one. The three sandboxes then run at once: each guest
must read the marker of its own share, and a file that a `sandbox exec` writes
in one sandbox's `/tmp`, in its upper, must appear in no other. Each must stop with an outcome report of a successful restored VM
without a network policy and a complete teardown, and be deprovisioned. No
restore may change the snapshot's files, leave a link to them, or leave an
OpenVMM process.

The `snapshot-tiers` scenario captures one snapshot of the platform tier, the
one tier that `snapshot capture` captures, and restores it with
`snapshot restore` into a managed sandbox of each image format over each
upper: the Ubuntu EROFS layer, and a GPT disk of its tree that a guest of the
Alpine control initramfs writes, as `sandbox-gpt-layer` does, each over a
fresh copy of the scratch template and in RAM. Each guest must have its image
as its root's lower directory, `/run/nvx/layers/distro` or
`/run/nvx/layers/p2`, over its upper, and a `sandbox exec` in its sandbox must
see the same root of the same guest. Each sandbox must stop cleanly and be
deprovisioned, and the restores change neither image nor the snapshot.

Both scenarios journal the OpenVMM processes of their managed restores, as
the other scenarios of the `snapshot-isolation`
[adversarial campaign](adversarial-testing.md) journal theirs. Their evidence,
with a `snapshot-core-` or `snapshot-tiers-` prefix, holds the bounded command
observations, the output of each command, and the outcome reports of the
managed sandboxes, and that of `snapshot-tiers` the console of the guest that
wrote its GPT disk. CI runs them on KVM, MSHV, and WHP with the public sandbox
scenarios.

The public `untiered-network-policy` scenario captures a snapshot with `--net`,
whose NIC has the complete deny-all policy, and restores it four times with
the Ubuntu layer over a RAM upper, each under a network policy of its own: the
capture's deny-all, and private-network ingress with allowed egress, through
`snapshot restore`, whose sandboxes it then stops and deprovisions; and egress
rules that admit DNS over IPv4 and one host service, and explicit
host-loopback access with a forward, under egress rules that admit DNS over
IPv6 alone, through `run --restore-snapshot`. Each restored guest must name
the DNS server of its policy in its resolver, and no gateway without one, run
the announcer of its listeners exactly when its policy publishes them, and
reach exactly the host services that its policy admits, and the host must
reach the guest's server through the forward, and through the port that the
policy publishes, on host loopback alone or to the host's private networks.
Before it provisions anything, the CLI must refuse a restore without
`--network-profile`, and one that names the NIC's address with `--net`. No
restore may change the snapshot or leave its sandbox or an OpenVMM process.
CI runs it with the other public sandbox scenarios.

### Live host shares

`sandbox run` and `sandbox provision` accept repeatable
`--mount GUEST_TARGET,HOST_PATH[,ro|rw]` (default `ro`) options, each with its
own guest target and access mode, plus repeatable `--mount-deny HOST_PATH`,
`--mount-allow HOST_PATH`, and `--mount-write HOST_PATH` rules (see
[Access policy](#access-policy)). As for `run`, the mode follows the last
comma, so a host path may contain commas when the mode is given. OpenVMM
exports one host directory through
the microVM's virtio-fs device, and several shares, or a shared file, as the
children of an aggregate on that device, and enforces each share's access mode
and policy on the host side, so edits are visible in both directions without
staging or copy-back:

```bash
nvx-rs/target/release/nvx-rs sandbox \
  --layer distro,build/ubuntu-distro.erofs,11111111-1111-1111-1111-111111111111 \
  --scratch /var/lib/nvx/scratch.ext4 \
  --mount /workspace,/srv/checkout,rw \
  --mount-deny .git/credentials \
  --mount /opt/hostedtoolcache,/opt/hostedtoolcache,ro \
  --entrypoint /bin/sh
```

A `--mount` whose host path is a regular file shares only that file, as
[virtio-fs host mapping](#virtio-fs-host-mapping) describes: the workload sees
it at its target, but neither its directory nor its siblings. A shared file
takes no `--mount-deny`, `--mount-allow`, or `--mount-write` path, because its
mode applies to the whole file.

A relative `--mount-deny`, `--mount-allow`, or `--mount-write` path is
resolved inside its share's host directory. With one share, every such path
applies to it. With several, each applies to the `--mount` before it. Either
way, it must name a path inside its share's directory. The guest targets and the host
paths of the shares must not equal or contain one another, because one
share could otherwise hide another or reach its files under a different access
mode, so a shared file may lie beside a shared directory, but not inside it.
NVX and OpenVMM compare the host paths by resolved path and by file
identity, so one directory or file reached through two paths, such as a bind
mount or a hard link, is rejected too. NVX also compares each share's identity
with those of the other share's ancestors, so a share inside a bind mount of
the other share's directory is rejected before OpenVMM starts. On Linux,
OpenVMM also
compares the filesystem sources that each directory reaches, through the
mount that contains it and every mount below it, so it also rejects a bind
mount or nested mount that exposes part of one share inside the other. That
check is Linux-only, so on Windows, don't share a directory that reaches the
other share's files through a mount.
Guest mount flags are not a security boundary: OpenVMM rejects every write to
an `ro` share with `EROFS`, whichever mount or link inside the guest reaches
it.
After it assembles the container overlay and verifies the workload identity,
the guest mounts the shares inside the container root with `nosuid,nodev`,
before any workload enters its private mount namespace. How it learns of them
depends on the lifecycle.

A sandbox, of `run` or of `start`, delivers them once its guest agent is
ready. OpenVMM exports every share as a child of one aggregate, which the guest
mounts at
`/run/nvx/hostfs/root`, where only root can enter, and the kernel command line
announces only the number of mappings, as `nvx_maps=COUNT`, or, for a restore,
whose snapshot keeps the command line of its capture, the guest's activation.
Once the guest
agent answers, `start` sends it the mapping table, in order, in as many
requests as it takes. The agent mounts each entry at its target with its mode,
and creates a missing target only on the container overlay, never inside
another mapping, as a directory or, for a shared file, a regular file. It
refuses every workload until it has mounted the whole table. A mapping that
the guest refuses or cannot mount fails `run` or `start`, after the guest
shuts down as `stop` has it do, so the sandbox never runs with part of its
shares; they terminate OpenVMM only if the guest does not shut down within
`--timeout`. The kernel command line does not bound the number of a sandbox's
shares; the 256 children of one aggregate do.

A managed `stop`, and any failure after a share is mounted, unmount the binds
or mappings in reverse order, then the aggregate or the export, before the
overlay is unmounted or the VM powers off, unless the guest stops answering, or
`start` is interrupted, and OpenVMM is terminated first. Each target
must be an absolute, canonical path; `/`, `/etc`, `/run`, and `/run/nvx`, and
the `/etc/machine-id`, `/etc/resolv.conf`, `/proc`, `/sys`, `/dev`,
`/.nvx-agent`, and `/run/nvx/time` trees, are reserved for the container
runtime, which replaces `/etc/machine-id` with the workload's machine ID, keeps
`/etc/resolv.conf` for the guest's resolver file, creates `/run` and
`/run/nvx`, and binds its time state over `/run/nvx/time`.
The guest refuses a target whose path crosses a symbolic link in a container
layer, a target that a container layer holds as another kind of file than the
share's, and overlapping targets. Any validation or mount failure fails a
`run` or a managed `start`, instead of starting the workload without its
shares.

An `rw` share supports the symbolic links that package managers and
language toolchains create; see
[virtio-fs host mapping](#virtio-fs-host-mapping) for their semantics. The
existing OpenVMM file-identity policy applies to each share.
`run` and `provision` plan the shares once, with the planner that every NVX
caller uses: it resolves each host path and access-policy path, which must
exist, refuses what OpenVMM or the guest would refuse, and records the file
identity of each shared or denied path that the workload could move, those
inside a writable part of a share. On Windows, it refuses a path that only a
verbatim `\\?\` path names, such as that of a directory whose name ends in a
period or a space, which the path without the prefix would not name. The
shares' children follow their resolved host paths. A managed sandbox stores
the plan, with those identities beside it, in its typed configuration, in
format 10. Each `start` checks it again, as `run` does once it has verified the
distro layer: it refuses a host path that no longer holds a directory or
regular file as recorded, a recorded path whose file identity changed, and
shares whose host paths came to overlap, such as through a hard link or a bind
mount. NVX no longer reads earlier configuration formats, including those that
recorded the shares as requested, nor a configuration that earlier builds wrote
without those identities beside the plan; deprovision such a sandbox and
provision it again.

A share must not reach the sandbox's own files, which the workload could
otherwise read or change: `provision` refuses a share whose resolved host path
equals, contains, or lies inside the sandbox's state directory, its image, its
scratch image, or the OpenVMM, kernel, or initramfs of the artifact root, or
the OpenVMM that `NVX_OPENVMM` names, before it records the sandbox, and
`start` refuses one again before it launches the VM. The `start` of a restored
sandbox also refuses a share that reaches its snapshot store, whose files the
view of each restore links, before it takes the snapshot's lock.
`start` resolves the recorded host paths again, so a share that came to reach
one of these files through a link since `provision` is refused too. A
`provision` that finds no artifact root leaves the artifacts to `start`. A
`run`, which has no state directory, refuses a share that reaches any of the
others, or the temporary VM directory in which it would launch its VM, before
it launches that VM. Only
resolved paths are compared, so don't share a directory that holds a link
through which the host reaches one of these files, nor a hard link or bind
mount of one.

#### Access policy

Each share can narrow what the workload can see and modify inside it. OpenVMM
enforces the policy on the host for every request, whichever guest mount, path,
or link reaches the share:

- `--mount-deny HOST_PATH` hides a path and everything below it. It may name
  the share's root itself when allowed paths inside it remain, so the share
  exposes only those paths.
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

Each path must exist inside the share's host directory, and is attributed to
a share like `--mount-deny`. `sandbox` and `run` resolve it once, when they
plan the shares, so it may cross a symbolic link or junction to a path inside
the same directory.
Its names may contain spaces, but not begin or end with one, nor contain
other whitespace, colons, or backslashes.
Allowed paths follow the share's write policy: without `--mount-write`, an
allowed path in an `rw` share is writable; with it, an allowed path is
writable only inside a writable path. For example,
the following share hides the MCP gateway's logs except the tool payloads
that it spills below them, which stay readable, and lets the workload write
only to `agent` and `cache`:

```bash
nvx-rs/target/release/nvx-rs sandbox \
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

On Linux, in either mode, the files and directories that the workload creates
get the modes that the guest asks for, narrowed by the umask of the process
that starts OpenVMM, which NVX starts with that umask without the owner's
bits, so it narrows only the group's and others' permissions. In a host
directory that has a default POSIX ACL, Linux narrows them by that ACL instead
of the umask, so the ACL decides every permission, the owner's included.

```bash
nvx-rs/target/release/nvx-rs sandbox \
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
  env HOME="$HOME" nvx-rs/target/release/nvx-rs sandbox ... --mount-owner caller
```

A service manager can grant the same set, such as systemd's
`AmbientCapabilities=CAP_SETUID CAP_SETGID`. Without these capabilities, an
operation succeeds only if its caller has OpenVMM's user and primary group and
OpenVMM's user belongs to no other group; every other operation fails with
`EPERM`. Hosts commonly grant access to `/dev/kvm` or `/dev/mshv` through such
a group, so `caller` usually needs both capabilities. A managed sandbox's guest
agent maps the shares before any workload runs, as guest root, which `caller`
squashes to the owner of the host directory, so a managed sandbox whose
OpenVMM cannot assume that identity fails to start. Grant OpenVMM no other
capabilities: `CAP_SETUID` lets it assume any host identity, which `caller`
mode confines to its export.

The guest kernel reports each caller's identity, and guest root may assume any
identity inside the guest, so guest root and a compromised guest kernel can act
as any nonzero host UID and GID inside the share, including leaving setuid
files that those identities own. Share only directories that such identities
may modify, and keep the share on a host filesystem mounted `nosuid` when host
users might execute files from it.

### Sandbox networking

`sandbox run` and `sandbox provision` read their network options into one
typed network policy, the policy that the NVX Rust API takes from external
orchestrators, and render OpenVMM's network options with the same lowering.
They check the policy before OpenVMM runs and before `provision` records
anything, and `provision` records the policy itself, which `start` checks
again. An invalid value or combination fails with status 1 and the planner's
message.

Without `--net`, the sandbox has no network device, and the other network
options are refused. With it, an omitted option takes OpenVMM's default:
egress allowed, ingress denied, and host-loopback access from the workload
alone, through its gateway. A policy attaches no network device when it has
`--network-egress deny` and `--host-loopback deny`, and no allow rules,
`--network-ingress allow`, or `--network-proxy`, whatever its deny rules. The
allow rules of a `--network-egress-policy-file` are the native rules that it
compiles to, so a file whose allow rules all except their whole destinations
compiles to none. Any other policy attaches a network device, even one that
lets nothing through, such as one whose `--network-egress-deny 192.0.2.0/24`
covers its `--network-egress-allow 192.0.2.1`. In the typed policy of the NVX
Rust API, every allow rule counts, even one that lowers to no OpenVMM rule.
Egress rules need an explicit `--network-egress`, as OpenVMM requires. Forwards
need `--host-loopback allow`; UDP forwards also need egress allowed by default
and no deny rule that covers the guest network, which carries their replies.
`--host-loopback allow` needs a guest network prefix of `/29` or shorter; see
[Host-loopback access](#host-loopback-access).

#### Proxy-only networking

A proxy-only sandbox has one way out: an HTTP or HTTPS proxy that listens on a
port of host IPv4 loopback, which the workload reaches at the same port of its
gateway. Every other destination is denied, the proxy's port over UDP and the
host's other loopback services included:

```bash
nvx-rs/target/release/nvx-rs sandbox \
  --layer distro,build/ubuntu-distro.erofs,11111111-1111-1111-1111-111111111111 \
  --scratch /var/lib/nvx/scratch.ext4 \
  --net 10.0.0.2/24 \
  --network-profile portable \
  --network-egress deny \
  --host-loopback deny \
  --network-proxy 10.0.0.1:3128
```

Here the proxy listens on `127.0.0.1:3128`, and the workload reaches it at
`10.0.0.1:3128`: the address of `--network-proxy` must be the gateway of
`--net`, the first address of its network. Direct egress and a proxy are
separate connectivity models, so a proxy requires `--network-egress deny` and
`--host-loopback deny`, and refuses egress rules, whether explicit or from a
policy file, `--network-ingress allow`, and `--host-loopback-forward`. OpenVMM
relays the workload's TCP connections to that port whatever they carry, so the
proxy decides what passes. The port can't be 53: OpenVMM answers DNS at the
gateway's port 53, over TCP too, before it relays a connection to a proxy, so
the proxy would receive nothing and the workload would reach the host's
resolver instead. NVX gives the workloads of a cold boot no proxy settings:
pass the proxy's URL as the workload expects it, such as
`--inherit-default-environment --environment HTTPS_PROXY=http://10.0.0.1:3128`
on a managed `exec`. The guest of a restore whose own network policy is
proxy-only gives its workloads the proxy in their default environment instead,
as [`snapshot restore`](usage.md#snapshot-restore) describes. The plan of the
NVX Rust API reports the URL as the guest sees it, `SCHEME://GATEWAY:PORT`, or
`GATEWAY:PORT` for a proxy without a scheme, as `--network-proxy` names it.

The `sandbox-proxy-only` scenario of `test-microvm --backend BACKEND` is the
acceptance for proxy-only networking. Like the
[managed lifecycle](#managed-lifecycle) scenarios, it runs `nvx-rs sandbox` as
subprocesses with the Ubuntu layer and the sandbox scratch template that
`nvx-rs download` installs. It checks that `provision` refuses
a proxy beside `--network-egress allow`, and one at an address other than the
gateway, before it creates any state. It then runs the same probes in two
sandboxes, whose proxy is a test proxy on host loopback: a managed `exec`
whose workload requests a URL through the proxy at its gateway with `wget`,
and tries with `nc` another port of the gateway, the proxy's port over UDP, at
the host's own address, and at the gateway's IPv6 address. In the first
sandbox, with `--network-egress allow` and `--host-loopback allow`, each try
must reach a host listener, which shows that the probes work. In the second, a
proxy-only sandbox, the test proxy must receive that one request and answer
it, and the host listeners must receive nothing. The last two destinations are
tried where the host has a default IPv4 route and IPv6 loopback. After each
`stop`, OpenVMM's report must show the policy that it applied: for the
proxy-only sandbox, egress denied without rules, and host-loopback access
denied. The scenario's evidence, with a `sandbox-proxy-only-` prefix, holds its
bounded command observations, the runtime record, and for each sandbox the
probe's output, the proxy's request line, the OpenVMM log, and the outcome
report.

### Managed lifecycle

For a state-aware sandbox, provision configuration without starting a VM,
start it once, run multiple workloads in the same warm guest, stop it while
retaining configuration, and finally deprovision it:

```bash
nvx-rs/target/release/nvx-rs sandbox provision \
  --state-dir /run/user/1000/nvx-example \
  --layer distro,/var/lib/nvx/distro.erofs,11111111-1111-1111-1111-111111111111 \
  --scratch /var/lib/nvx/scratch.ext4
nvx-rs/target/release/nvx-rs sandbox start \
  --state-dir /run/user/1000/nvx-example
nvx-rs/target/release/nvx-rs sandbox exec \
  --state-dir /run/user/1000/nvx-example \
  --entrypoint /usr/bin/python3 --arg=/work/agent.py --cwd /work \
  --environment-file /run/user/1000/nvx-example-environment.json \
  --outcome-report /run/user/1000/nvx-example-exec.json
nvx-rs/target/release/nvx-rs sandbox exec \
  --state-dir /run/user/1000/nvx-example \
  --entrypoint /bin/sh --arg=-c --arg='cat /tmp/previous-result'
nvx-rs/target/release/nvx-rs sandbox stop \
  --state-dir /run/user/1000/nvx-example
nvx-rs/target/release/nvx-rs sandbox deprovision \
  --state-dir /run/user/1000/nvx-example
```

`provision` verifies the distro layer as `run` does, hashing it against its
manifest when one sits beside it, and records the layer's binding in the
configuration: its identity and metadata, its manifest's, or the manifest's
absence, and the distro policy that accepted it. It records the layer at its
resolved path, so for a layer given through a link to another name, it also
verifies the manifest beside the link's target. `start` checks the binding
from metadata alone, without reading the layer again, and refuses a layer or
manifest that was replaced, modified, added, or removed since `provision`.
`provision` and `start` refuse a manifest path
that they cannot look up, as through a link loop, rather than take it for no
manifest. Keep provisioned layers and their manifests read-only, and never
rewrite them in place.

Disk image paths and the paths they resolve to must contain no commas or
semicolons, and square brackets must nest properly. Balanced brackets, as in
`layer[1].erofs` or `layer[[1][2]].erofs`, work; an unclosed `[`, an unmatched
`]`, or reversed `][` fails with `error[unsupported]` before `run` launches
OpenVMM or `provision` records the sandbox. `start` applies the same checks to
recorded paths.

`provision` also binds the scratch image: its identity, kind, and length,
which the VM's writes leave unchanged. `start` checks the binding from
metadata alone, refuses a scratch image that was replaced or resized since
`provision`, and claims the image for the sandbox until its OpenVMM exits: while
it runs, a second sandbox, or a `run`, on the same image fails with
`another sandbox claims the ext4 scratch image PATH`. On Linux, the identity is
the device and inode number, which a file that is created once the image is
removed can take again: such a replacement, of the same length, is not refused.
A sandbox whose upper is in RAM has no scratch image to bind or claim.
`run` binds the scratch image itself, and checks the binding and claims the
image as `start` does. `run` and `start` then hand OpenVMM the paths of the
images that they checked, which OpenVMM opens itself: keep the images in place
while the VM uses them. Images are attached raw: stage an image in a container
format, such as a VHD, as a raw file first.

`provision` records the guest memory of `--memory-mib`, which it takes from 1
to 17592186044415 MiB, as `run` does, and `start` refuses a configuration that
records memory outside that range before it launches OpenVMM.

Lifecycle transitions fail closed: `start` rejects a sandbox that is already
running; `exec` and `stop` require a live OpenVMM process; and `deprovision`
refuses to remove a running sandbox or unknown files. The state directory is
the sandbox's store, whose runtime record identifies the running OpenVMM by its
process ID, its start time, and the boot of the host, so these checks treat
OpenVMM as gone once it exits, even if no process reaps it or another process
reuses its ID. Every transition but `provision` first finds the sandbox's VM as
an orchestrator does after it restarts: it ends a `start` that was interrupted,
such as a killed one, and clears the runtime record of an OpenVMM that exited
without a `stop`. The first `exec` or `stop` to find such a record fails
because the runtime state is stale, while `start` launches OpenVMM again and
`deprovision` removes the sandbox.

`start` records the runtime identity of the artifacts with the VM: the SHA-256
digests of OpenVMM, the kernel, and the Alpine initramfs, as the install
metadata lists them, without hashing an artifact. A release lists them in its
`SHA256SUMS`, and a checkout in the build provenance that `nvx.py` writes to
`build/` with each artifact that it builds, and that `nvx-rs download`
installs. The transitions drive only a VM that artifacts of the same identity
started. One that other artifacts started, as before they were rebuilt or
replaced, may speak other contracts, so `start` and `exec` refuse it, naming
the parts that differ, `deprovision` refuses it as running, and `stop`
terminates its OpenVMM without a guest shutdown, waits up to `--timeout`
seconds for it to exit, and warns that it did. A `stop` that cannot determine
the OpenVMM process, whose terminated OpenVMM does not exit in time, or whose
wait Ctrl-C ends, keeps the sandbox's runtime record, and the next `stop`
retries. The store's records belong to the version of NVX that wrote them, so
stop and deprovision each sandbox with the version that started it before you
switch versions, as [Upgrades](#upgrades) says.

`provision`, `start`, `stop`, and `deprovision` hold the store's lock,
`sandbox.lock` in the state directory, until they return, so the transitions
of one sandbox take effect one at a time: each waits up to `--timeout` seconds
for any other in progress, and of two overlapping starts, exactly one launches
OpenVMM while the other then fails because the sandbox is already running.
`exec` holds the lock only while it finds the running OpenVMM, so a long
workload does not delay `stop`. One `exec` at a time runs in a sandbox, across
processes: an `exec` takes the sandbox's exec lock, `exec.lock` in the state
directory, before it finds the running OpenVMM, and holds it until the command
ends: once its workload has ended, or, after Ctrl-C or once `--timeout` has
passed, without waiting for the guest to end the workload. An `exec` that
finds it held tries again until `--wait` seconds, 60 by default, have passed,
and then fails with `sandbox-busy`. `stop` takes no exec
lock, so it proceeds while a workload runs. A provisioned state directory keeps
`sandbox.lock`, and `exec.lock` from its first `exec` on, and `deprovision`
removes them with NVX's other files. A
transition that leaves a state directory without `config.json`, such as one of
a sandbox that was never provisioned, removes `sandbox.lock` as it releases the
lock. These are the rules of the CLI's own store of a sandbox. On NFS, where a
removed file that is still open, such as a lock file, stays as `.nfs*` until
its last handle closes, and
on Windows file systems without POSIX deletion such as ReFS, where a removed
lock file stays under its name, such files keep the emptied directory, so
`deprovision` then releases the lock before it removes the directory. A
transition that waited while `deprovision` removed the sandbox then fails as
for a sandbox that is not provisioned, except `provision`, which provisions the
state directory again. A `provision` that takes the lock after `deprovision`
has removed the lock file, but before it has removed the directory, provisions
the directory first, and `deprovision` then fails because the sandbox was
provisioned again.
`--timeout` must be finite and greater than 0. If the guest exits while `start`
waits for it to become ready, for example because it refuses
the workload identity, OpenVMM closes the control endpoint and shuts down.
`start` then waits up to `--timeout` seconds for OpenVMM to publish
`outcome.json` and exit, and fails with the report's category and status, such
as `guest-exit status 125`. It terminates OpenVMM only if OpenVMM outlives that
wait. An OpenVMM that exits before it serves the control endpoint, as one that
refuses its configuration does, ends the start at once rather than after
`--timeout`: the start fails with the report's category and status if OpenVMM
published one, and otherwise with OpenVMM's exit status, as in
`OpenVMM exited during startup with status 2`.
OpenVMM writes the report to a file of its own in the state directory,
`.tmp` and six ASCII letters or digits, before it publishes it: NVX removes
such a file, which an OpenVMM that is terminated while it writes the report
leaves, once no OpenVMM of the sandbox runs, and `deprovision` removes it with
NVX's other files. Managed
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
and should not be used for secrets. These options apply to `sandbox exec`,
and to `sandbox run`, which executes its one workload as `exec` does. nvx-init,
the VM's init, runs each managed exec as a plain process in a process group of
its own, in the VM's root and in nvx-init's mount, PID, and UTS namespaces and
cgroup: the VM is the
boundary of its workloads. Every mount of the root but `/dev` and its devpts
instance is `nodev`. The workload sees one machine ID for the life of the VM.
On `stop`, nvx-init syncs the file systems and powers the VM off with them
mounted, and the next `start` replays the journal of the scratch file system.
The operation-less `sandbox` form is `sandbox run`, which keeps no state and
rejects `--state-dir`.

Environment files are limited to 1 MiB of UTF-8 JSON. Environments contain at
most 256 unique, nonempty names. Each `KEY=VALUE` entry and working-directory
path is limited to 4096 UTF-8 bytes; the combined execution request must also
fit the existing 64 KiB control-payload bound. A managed `exec` streams its
standard input to the workload, which sees it end when the command's own
input ends, and the workload's standard output and standard error back as
they arrive, without a limit on their size; the workload must end within
`--timeout`, which bounds the whole exchange, or `exec` fails with
`timed-out`. Ctrl-C asks the guest to end the workload, and `exec` exits with
status 130 without waiting for it, while the sandbox runs on.

The `managed-exec-config` scenario of `test-microvm --backend BACKEND` is the
authoritative acceptance for these public options. It invokes
`nvx-rs sandbox provision`, `start`, `exec`, `stop`, and `deprovision`
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
sandbox scratch template, `build/ubuntu-smoke-scratch.ext4`, which
`nvx-rs download` installs and the guest-artifact build makes, and
`test-microvm` fails before its first scenario when one of them is missing. A
default run includes it, and CI runs it on every backend in the guest and
sandbox job, which takes those Ubuntu artifacts.
Empty environments are measured with `/usr/bin/env`, not a shell that can
synthesize its own variables. The scenario retains bounded subprocess argument
and status observations, typed exec outcomes, and OpenVMM logs. Inline
environment values are redacted from the retained command observations.

The `sandbox-lifecycle` scenario of `test-microvm --backend BACKEND` is the
authoritative acceptance for the managed lifecycle itself. It
also runs the public commands as subprocesses against the real kernel, Alpine
control initramfs, Ubuntu EROFS layer, and fresh copies of the same scratch
template, so it has the same artifact requirements. It checks that:

- `provision` rejects a root `--workload-user` before it creates any state, and
  the guest refuses an identity that the Ubuntu image lacks before any
  workload starts, in a run and in a managed `start`, which both then fail with
  `guest-exit status 125` from the outcome report that OpenVMM must publish
  before it exits, and the start leaves only `config.json`, `sandbox.lock`,
  `openvmm.log`, and that `outcome.json`, without a runtime record,
  capability, control socket, report staging file, or OpenVMM process;
- every operation fails on a state directory that was never provisioned, a
  repeated `provision` leaves the configuration unchanged, and `exec` and
  `stop` fail before `start`, though the `exec` leaves the `exec.lock` that the
  state directory keeps from then on;
- a repeated `start` and a `deprovision` of a running sandbox fail without
  changing its runtime record, capability, or OpenVMM process, and the
  sandbox keeps serving requests;
- managed arguments keep leading, trailing, and embedded spaces, tabs, and
  newlines, a later request reads the file that an earlier one wrote, and the
  file survives `stop` and a new `start`, because the overlay's upper directory
  is on scratch;
- `/sbin/nvx-sandbox-smoke` passes its security profile checks in a sandbox
  provisioned with `--hostname`, after earlier requests;
- a request that writes 2 MiB streams all of it, with no limit on its output,
  and exits with status 0 and an `exit` outcome report, and a request whose
  `--outcome-report` path exists fails before its workload runs and leaves the
  file intact;
- `stop` ends OpenVMM and its control socket or pipe, leaves only
  `config.json`, `sandbox.lock`, `exec.lock`, `openvmm.log`, and a successful
  `outcome.json`, and leaves a cleanly unmounted scratch file system, after
  which `exec` and `stop` fail;
- two overlapping `start` calls then launch a single OpenVMM process: one
  succeeds, and the other fails with `sandbox is already running or has stale
  runtime state`;
- the `nvx-rs` CLI of an artifact root whose build provenance names another
  initramfs, as after an upgrade, drives no VM that the checkout's artifacts
  started: its `exec` and `start` fail, naming the initramfs and OpenVMM's
  process ID, and its `deprovision` fails because the sandbox runs, without
  changing the runtime record or OpenVMM process, while the checkout's CLI
  still serves requests. Its `stop` then terminates OpenVMM without a guest
  shutdown and warns that it did, and leaves only `config.json`,
  `sandbox.lock`, `exec.lock`, and `openvmm.log`, without a control socket or
  pipe, after which its `stop` finds the sandbox not running, and the sandbox
  starts again;
- after OpenVMM exits without a `stop`, `exec` reports the stale runtime state
  once and clears it, after which `stop` finds the sandbox not running, and the
  state directory holds only `config.json`, `sandbox.lock`, `exec.lock`, and
  `openvmm.log`, without a control socket or pipe; and
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
guest probe's transcript, and the output of the refused run, each limited to
its last 64 KiB. The command observations hold no workload output,
and no evidence holds the control capability.

Decoder, helper, and direct control-session tests remain useful supplemental
coverage for protocol boundaries and guest implementation details. They do not
replace or establish support through the public `nvx-rs sandbox` interface.

`run --outcome-report PATH` and `sandbox run --outcome-report PATH` forward
OpenVMM's bounded local JSON report, with the permissions that OpenVMM gave it:
that of the VM's stop, for `sandbox run`, whose exit status is its workload's.
Managed `sandbox exec` writes only the operation, bounded result category,
numeric status, and an opaque operation ID to its requested report; stdout,
stderr, arguments, environment values, and credentials remain excluded.
`sandbox stop` waits for OpenVMM teardown and retains the latest VM-level report
as `outcome.json` in the state directory.
On Linux, NVX starts OpenVMM with the caller's umask without the owner's bits,
so OpenVMM's reports keep their owner's read and write permissions whatever
that umask, unless their directory has a default POSIX ACL, which Linux
applies instead of the umask. Neither OpenVMM nor NVX uploads these files.

## Upgrades

NVX keeps no state from one version to the next, and migrates none: each
version starts with fresh state. A version keeps the state directories of its
sandboxes and its snapshot stores in formats of its own, and may refuse those
that another version wrote, as it refuses a sandbox's configuration of another
format with `format-unsupported`, or fail to read their records at all. A
snapshot restores only under the runtime identity that captured it, which
names the OpenVMM, kernel, and Alpine initramfs of its artifacts and the
contracts of NVX, so a restore refuses it once an upgrade has installed new
artifacts. To upgrade:

1. With the version that you leave, stop and deprovision each sandbox, which
   also ends the VM of a start that was interrupted.
2. Switch to the new version of `nvx-rs` and its artifacts.
3. Capture the snapshots again, into new stores, since a store never replaces
   a snapshot's name, and provision or restore the sandboxes again. No command
   removes a snapshot: remove an old store once no sandbox restores from it.
