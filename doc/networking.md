# Networking

NVX presents one virtio-net device on a virtio-mmio transport when networking is enabled. The
guest-visible device and Alpine bootstrap are shared, while the host data plane depends on the
backend:

| Backend and option | Host data plane | Privilege |
| --- | --- | --- |
| Linux/KVM `--net` | Managed TAP | Root or passwordless `sudo ip` for setup |
| Linux/KVM `--net --net-tap` | Preconfigured TAP | No per-run privileged command |
| Windows/WHP `--net` | Built-in user-mode NAT | No administrator rights |
| Windows/WHP `--net-config` | External L2Bridge through AF_XDP | Externally provisioned, privileged resources |

The guest uses a static IPv4 address. IPv6 is not part of the current guest network contract.

## Egress policy

All three host data planes support the same repeatable destination policy:

```text
--allow-host 10.20.0.0/16 --allow-host 192.0.2.10
--block-host 169.254.169.254
```

`--allow-host` permits only the listed IPv4 addresses or CIDRs. `--block-host` permits all IPv4
destinations except the listed addresses or CIDRs. The modes are mutually exclusive and require
`--net`, `--net-config`, or a networked `--restore`. DNS has no implicit exception, so an
allow-list must include every DNS resolver the guest needs.

The policy is checked before SLIRP opens a host socket, before KVM writes a frame to TAP, and before
AF_XDP queues a frame for transmission. TAP and AF_XDP allow ARP so the guest can resolve its
gateway, and they inspect IPv4 inside stacked VLAN headers. In allow-list mode they reject IPv6
and other non-ARP protocols because those destinations cannot be represented by an IPv4 CIDR. In
block-list mode non-IPv4 protocols remain outside the IPv4 policy. Malformed IPv4 or VLAN frames
fail closed whenever either policy mode is active.

The policy is run-scoped and is not serialized into snapshots. The options therefore conflict
with `--snapshot`; supply the desired policy again whenever restoring a networked snapshot.

## Standalone addressing

`--net <IP/PREFIX>` identifies the guest. Prefixes from `/1` through `/30` are accepted. NVX
derives the gateway as the first usable address in the subnet, so the guest cannot use that same
address:

```text
--net 10.0.0.2/24
guest:   10.0.0.2
gateway: 10.0.0.1
mask:    255.255.255.0
```

PID 1 waits for the virtio-net interface, assigns the address, and installs a default route through
the derived gateway. The VMM appends the virtio-mmio and `virtnet_*` tokens needed for that setup.

```console
# Linux
./target/release/microvm \
  --kernel "$HOME/build/vmlinux" \
  --initrd "$HOME/build/initramfs.cpio.gz" \
  --net 10.0.0.2/24
```

```powershell
# Windows
.\target\release\microvm.exe `
  --kernel build\vmlinux `
  --initrd build\initramfs.cpio.gz `
  --net 10.0.0.2/24
```

The workflow wrapper exposes the same option:

```console
python scripts/nvx.py run --net 10.0.0.2/24
```

## Linux TAP networking

On KVM, NVX creates a TAP, assigns the derived gateway address and deterministic gateway MAC, and
brings it up. Guest frames pass directly between virtio-net and the TAP. The interface is removed
when the VMM exits.

The default link provides host-to-guest and guest-to-host connectivity only:

```console
# In the guest
ping -c1 10.0.0.1
wget -qO- http://10.0.0.1:8000/
```

Forwarding to another network is a host policy decision. Enable IPv4 forwarding and add an
appropriate nftables or iptables NAT rule if internet access is required.

### Reuse a TAP

Managed TAP setup requires several privileged `ip` commands on each launch. A persistent,
user-owned TAP avoids that cost, which is especially useful for restore benchmarks:

```console
sudo ip tuntap add dev llxnet0 mode tap user "$USER"
sudo ip link set dev llxnet0 address 52:54:00:00:00:01
sudo ip addr add 10.0.0.1/24 dev llxnet0
sudo ip link set dev llxnet0 up

./target/release/microvm \
  --kernel "$HOME/build/vmlinux" \
  --initrd "$HOME/build/initramfs.cpio.gz" \
  --net 10.0.0.2/24 \
  --net-tap llxnet0
```

The TAP must already have the gateway address, be up, and use the gateway MAC expected by the
guest. NVX attaches with `TUNSETIFF` and leaves the interface in place on exit. `--net-tap` is
Linux-only and requires `--net` on a cold boot or `--restore` for a networked snapshot.

## Windows user-mode NAT

WHP has no unprivileged layer-2 TAP, so `--net` starts an in-process network stack. It:

- answers ARP for the derived gateway;
- answers ICMP echo sent to the gateway;
- proxies guest TCP through host `TcpStream` connections;
- proxies UDP, including DNS, through host `UdpSocket` connections;
- maps connections to the gateway address to host loopback.

For example, a service listening on host loopback port 8080 is reachable from a
`--net 10.0.0.2/24` guest as `http://10.0.0.1:8080/`. This mode needs no driver, pre-created
interface, or administrator rights.

## Networking across snapshot and restore

Snapshots include the guest endpoint, MAC, virtio transport state, queue addresses, and queue
consumer indices. Guest RAM holds the rings themselves. On restore, NVX recreates the host data
plane before resuming the device:

- KVM recreates the managed TAP, or attaches the `--net-tap` supplied on restore.
- WHP recreates the user-mode NAT automatically.
- External AF_XDP snapshots require a fresh, compatible `--net-config`.

A standalone restore does not need `--net` because addressing comes from the snapshot:

```console
./target/release/microvm --restore ./snap --mem 256
```

The `bench-net-snapshot` and `bench-net-snapshot-py` workflows verify connectivity after restore;
see [Benchmark Reference](benchmark.md).

## External HCN and AF_XDP networking

WHP can replace user-mode NAT with an externally owned layer-2 interface. NVX binds AF_XDP queues,
but it does not create or destroy the interface, HCN network, endpoint, policy, or control pipe.
There is no fallback to standalone NAT if this path fails.

### Requirements

- Windows Hyper-V, Windows Hypervisor Platform, and Host Network Service.
- An administrator or service account able to manage HCN and XDP resources.
- Signed XDP-for-Windows Runtime x64 v1.3.0. NVX loads `xdpapi.dll` from System32 and requires the
  version 2 API table through `XdpOpenApi(2)`.
- An Agent-hosted local named pipe to coordinate data-plane readiness.

Install the required XDP runtime from an elevated PowerShell session:

```powershell
$package = Join-Path $env:TEMP 'xdp-runtime-1.3.0.zip'
$runtime = Join-Path $env:TEMP 'xdp-runtime-1.3.0'
Invoke-WebRequest `
  https://www.nuget.org/api/v2/package/Microsoft.XDP-for-Windows.Runtime.x64/1.3.0 `
  -OutFile $package
Expand-Archive $package $runtime -Force
& "$runtime\runtime\native\xdp-setup.ps1" -Install xdp
```

The older `aka.ms/xdp-v1.msi` redirect may install an incompatible runtime.

### Manifest contract

Pass a strict version 2 JSON manifest with `--net-config`. Unknown fields, duplicate keys, invalid
addresses, and unsupported queue selections are rejected.

```json
{
  "version": 2,
  "attachment": {
    "backend": "hcn-afxdp-l2bridge",
    "interfaceIndex": 42,
    "interfaceLuid": 1689399632855040,
    "gatewayMac": "00-15-5D-52-CF-2B",
    "queueSelection": { "mode": "auto" }
  },
  "device": {
    "macAddress": "00-15-5D-01-02-03",
    "mtu": 1500
  },
  "guestBootstrap": {
    "ipv4": {
      "address": "192.168.240.2",
      "prefixLength": 24,
      "gateway": "192.168.240.1"
    },
    "routes": [
      { "destination": "0.0.0.0/0", "nextHop": "192.168.240.1" }
    ],
    "dns": {
      "servers": ["1.1.1.1"],
      "search": []
    }
  },
  "runtime": {
    "controlPipe": "\\\\.\\pipe\\nvx-network-agent"
  }
}
```

Two attachment backends are accepted:

| Backend | Contract |
| --- | --- |
| `afxdp-l2bridge` | Bind an existing interface. Nonzero index and LUID must identify the same interface. |
| `hcn-afxdp-l2bridge` | Bind an externally provisioned HCN host vNIC and use `gatewayMac` for the local ARP proxy. |

Use `queueSelection.mode = "auto"` for HCN host vNICs because RSS may place return traffic on any
receive queue. `explicit` requires 1 through 64 strictly increasing queue IDs and is intended for
an Agent that controls steering. The AF_XDP frame layout accepts MTUs up to 4082 bytes.

HCN addressing must use a `/1` through `/30`, a usable guest address, and the first usable subnet
address as the gateway. The manifest may carry up to 32 explicit non-default routes, 3 IPv4 DNS
servers, and 6 search domains.

### Readiness protocol

NVX connects to the Agent's named pipe as a client, initializes every selected AF_XDP queue, and
writes one newline-terminated message:

```json
{"type":"DataPlaneReady","queues":[0,1],"interfaceLuid":1689399632855040}
```

The Agent replies only after the external network is ready:

```json
{"type":"StartVm"}
```

NVX does not enter the vCPU loop before receiving `StartVm`. Initialization failures are reported
as `DataPlaneError` on the same pipe.

On external snapshot restore, provide a fresh `--net-config`. The host interface index, LUID, and
queue resources may be reprovisioned, but the guest-visible MAC, MTU, IPv4 bootstrap, routes, and
DNS identity must match the snapshot. NVX rejects a standalone NAT snapshot with `--net-config`
and refuses to restore an external snapshot without it.

The repository's `setup-hcn-endpoint.ps1`, `cleanup-hcn-endpoint.ps1`,
`test-hcn-afxdp.ps1`, and `benchmark-hcn-afxdp-snapshot.ps1` scripts implement the CI provisioning,
cleanup, smoke-test, and benchmark flows.

### CI hardware lane

The `windows-hcn-afxdp` workflow job targets a dedicated self-hosted runner with the labels
`windows`, `x64`, and `hcn-afxdp`. Its service account must be an Administrator or Local System,
and Hyper-V, WHP, HNS, and XDP-for-Windows 1.3.0 must already be installed.

The current workflow schedules this job for pull requests. Main-branch pushes opt in with the
repository variable `NVX_HCN_AFXDP_CI=true`, and manual runs opt in with the `run_hcn_afxdp`
workflow input. Optional repository variables select a non-overlapping subnet:

| Variable | Default |
| --- | --- |
| `NVX_HCN_AFXDP_GUEST_ADDRESS` | `192.168.240.2` |
| `NVX_HCN_AFXDP_GATEWAY` | `192.168.240.1` |

The job provisions an HCN host vNIC before testing and runs cleanup in an `always()` step. When
benchmarks are enabled, every accepted restore must rebind the selected AF_XDP queues and complete
a gateway probe before contributing a sample.
