//! The host program launches VMs with a layered root, which the guest agent's worker assembles
//! through `nvx::guest::root` in a process of its own, never PID 1, on a real hypervisor, through
//! public APIs only.
//!
//! Each scenario of the table that [`scenarios!`] declares is an ignored test, which stages the
//! images of a [`Root`], launches a VM with it, and checks what it [`Expect`]s: a root that the
//! guest assembles, which the scenario probes, or runs executions in through the host program's
//! `exec`, and the stop tears down, the stage of the root that fails, or a launch that the host
//! program refuses before it spawns the VM, since the scenario changed what it staged. A root may
//! give the VM a [`Network`], which the launch reads from a file that the scenario writes, and
//! which names the ports of the [`Host`] side that the test binds first: a test proxy, and the
//! sockets of the destinations that an execution probes, as NVX's proxy-only scenario probes its
//! network. A scenario whose network stops traffic first boots a VM of an open network over the
//! same host's side, the positive control; see [`Expect::Controlled`]. A new scenario is a new
//! row, with its own checks, through the probes of the worker or executions.
//!
//! The tests boot VMs, so they are ignored. Besides what the `common` module lists, they read its
//! fixtures, those of NVX's own layered scenarios, from NVX's artifacts in `NVX_ARTIFACTS_DIR`.
//!
//! A scenario of a GPT disk writes the disk first, from the scratch template: its first
//! partition, opaque bytes, then a copy of the template, the base layer, and a small layer that the
//! test writes, the top one, which holds a file and a directory. The test writes it in the ext2
//! format, which the kernel's ext4 driver mounts, since `mkfs.ext4` is not available everywhere
//! that the tests run. One scenario corrupts that layer, whose superblock then asks the kernel to
//! panic on an error, as [`corrupt`] describes. Others give it a cyclic journal, with and without
//! the file system's `needs_recovery` bit, as [`add_cyclic_journal`] describes.
//!
//! A scenario of a mapping [`Table`] shares host directories and a file, which the test writes in
//! its directory, with the root, through the launch's `--share` options: the guest maps them
//! beneath the root from the host export before the sandbox starts, and the scenario then checks
//! the mappings in the guest and the files on the host. Others share what the guest, or the host
//! program before it spawns the VM, refuses.
//!
//! ```text
//! cargo test --locked --features host --test vm_layered -- --ignored
//! ```
#![cfg(feature = "host")]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use nvx::exec::{Key, Limits, Timers, Window};
use runtime_adapter::GUEST_BUILD;
use runtime_adapter::exec::Start;
use runtime_adapter::frame::MAX_BODY;
use runtime_adapter::layered::RECORD_NAME;
use runtime_adapter::root::{
    DEFAULT_RUNTIME, JournalPolicy, Listeners, OverlayMode, RootConfig, RootStage,
};
use runtime_adapter::staged::STAGED_NAME;
use serde_json::{Value, json};

use common::gpt::{
    self, Contents, LAYER_BLOCK, LAYER_BLOCKS, Layout, Partition, hexadecimal, small_layer,
};
use common::{Fixtures, Printed, Sandbox};

/// The exit status of the host program's command that failed, after a diagnostic.
const FAILED: i32 = 1;
/// The exit status of the host program's command whose guest reported that a stage of the root
/// failed.
const ROOT_FAILED: i32 = 4;
/// The exit status of the host program's launch whose guest failed a mapping of the table, and
/// stopped gracefully.
const TABLE_FAILED: i32 = 6;
/// The size of the guest's pages, in bytes.
const PAGE: u64 = 4096;
/// How many of the bytes available in an upper a file that fills it may leave, which the file's
/// own blocks of metadata take.
const FULL_SLACK: u64 = 1 << 20;
/// The size of a sector of a disk, its logical block, in bytes.
const SECTOR: u64 = 512;
/// The disk GUID of the scenarios' GPT disks.
const DISK_GUID: &str = "6c1f5e0a-2b7d-4c39-9e81-3a5d0f7b2c64";
/// The unique GUIDs of the partitions of the scenarios' GPT disks, the first partition's first.
const PARTITION_GUIDS: [&str; 3] = [
    "0b7e4a2c-5d16-4f83-a9c0-1e2d3f4a5b61",
    "0b7e4a2c-5d16-4f83-a9c0-1e2d3f4a5b62",
    "0b7e4a2c-5d16-4f83-a9c0-1e2d3f4a5b63",
];
/// The size of the first partition of the scenarios' GPT disks, in bytes.
const FIRST_PARTITION_SIZE: u64 = 4096;
/// The bytes of the first partition that the guest returns: bytes that are not UTF-8, with a NUL
/// and whitespace among them, but not at their end.
const FIRST_PARTITION: &[u8] = b"\xff\xferuntime-adapter\0\t\x80\xc3";
/// What the first partition holds after [`FIRST_PARTITION`], which the guest trims as the padding
/// that ends its bytes, as it trims the NUL bytes that fill the rest of the partition.
const FIRST_PARTITION_PADDING: &[u8] = b" \0\n\t\r\x0c\0 ";
/// The first sector of each partition of the scenarios' GPT disks is a multiple of this many: 1
/// MiB, as partitioning tools align them.
const ALIGNMENT: u64 = 2048;
/// The regular file that the top layer of the scenarios' GPT disks holds.
const LAYER_FILE: &str = "layer-file";
/// The size of [`LAYER_FILE`], in bytes, which spans three of its layer's blocks.
const LAYER_FILE_SIZE: usize = 2500;
/// The directory that the top layer of the scenarios' GPT disks holds.
const LAYER_DIRECTORY: &str = "layer-directory";
/// The directory of a scenario's sandbox that holds the host directories and the file that a
/// mapping table shares, as [`write_sources`] writes them.
const SOURCES: &str = "sources";
/// The files of the [`SOURCES`], each a path in them, with components separated by `/`, and its
/// contents.
const SOURCE_FILES: [(&str, &str); 4] = [
    ("dir/marker", "marker"),
    ("dir/secret", "secret"),
    ("file.txt", "file"),
    ("out/marker", "marker"),
];
/// The empty directory of a scenario's sandbox beside the [`SOURCES`], which the adapter does not
/// authorize: the sources are the one host directory that it authorizes.
const OUTSIDE: &str = "outside";
/// The shares of a mapping table: each host path, a path in the [`SOURCES`], and its target in the
/// root, with `:ro` for a read-only share. The launch hides `dir/secret`.
const SHARES: [(&str, &str); 4] = [
    ("dir", "/srv/maps/dir:ro"),
    ("dir/inner", "/srv/maps/dir/inner"),
    ("file.txt", "/srv/maps/file.txt:ro"),
    ("out", "/srv/maps/out"),
];
/// The guest tree that the adapter reserves in the scenarios of its reserved targets.
const ADAPTER_TREE: &str = "/srv/adapter";
/// The `map` probe of the [`SHARES`]: each target, and the hidden file in `dir`.
const MAP_TABLE: &str = "map dir=srv/maps/dir inner=srv/maps/dir/inner file=srv/maps/file.txt \
                         out=srv/maps/out secret=srv/maps/dir/secret";
/// The `map-checks` probe of the [`SHARES`], from the export's child `2`, `out`, and its child `1`,
/// `file.txt`: the plan exports the outermost shares as its children, in the order of their host
/// paths.
const MAP_CHECKS: &str = "map-checks 2 1";
/// The identity, besides root's, as which the scenarios' executions run: the image's `nobody`, of
/// the group `nogroup`, which NVX's Ubuntu image always holds.
const NOBODY: (u32, u32) = (65534, 65534);
/// An identity whose account the image lacks.
const ACCOUNTLESS: (u32, u32) = (4242, 4242);
/// Linux's error number of an operation that is not permitted.
const EPERM: i32 = 1;
/// Linux's error number of an invalid argument.
const EINVAL: i32 = 22;
/// The number of executions that meet only while all of them run.
const MEETING: usize = 4;
/// The guest's address and prefix in the scenarios' networks, in a range reserved for
/// documentation, which no host's own address shares, so that the guest reaches the host's address
/// through its gateway.
const GUEST_NETWORK: &str = "192.0.2.2/24";
/// The gateway of the scenarios' networks, the first address of the guest's, at which the guest
/// reaches host IPv4 loopback.
const GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
/// The gateway's IPv6 address, at which the guest reaches host IPv6 loopback: OpenVMM's portable
/// profile embeds the guest network's IPv4 addresses in `fd00::/96`.
const GATEWAY_IPV6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0xc000, 0x0201);
/// The first and the last of the ports that the host draws for its sockets: the dynamic range.
const PORTS: (u16, u16) = (49152, 65535);
/// The number of ports that the host draws for a socket before it gives up. Windows excludes
/// ports per protocol, in blocks, and gives consecutive ports to consecutive binds of port 0, so
/// the host draws each port independently.
const PORT_ATTEMPTS: usize = 16;
/// The ports that the host's sides of the scenarios hold, each from its draw until its scenario
/// ends, and for good if the scenario fails, since one of its VMs might then still run. Scenarios
/// run at the same time, and OpenVMM binds the ports of a listener of the guest and of a forward
/// only once it runs, and again for the scenario's VM once the positive control's ended, so no
/// other scenario may draw them in between.
static HELD_PORTS: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());
/// The URL that the probes request through a proxy. Its host's name is reserved, so that no
/// resolver knows it, and only a proxy answers the request.
const PROXY_TARGET: &str = "http://runtime-adapter.invalid/probe";
/// What the test proxy answers each request with.
const PROXY_RESPONSE: &str = "RUNTIME-ADAPTER-PROXY";
/// The longest request head that the test proxy reads, in bytes.
const REQUEST_LIMIT: usize = 4096;
/// How long the host waits at a socket for a probe's traffic, which the guest sent before its
/// execution ended, where the network lets the traffic through.
const ARRIVAL: Duration = Duration::from_secs(10);
/// How long the host waits at a socket for a probe's traffic where the network must not let it
/// through.
const NEGATIVE_OBSERVATION: Duration = Duration::from_millis(250);
/// The line with which a listener of the guest greets its client.
const GREETING: &str = "RUNTIME-ADAPTER-GUEST";
/// The line with which the host answers the greeting.
const ANSWER: &str = "RUNTIME-ADAPTER-HOST";
/// How long, in seconds, a listener of the guest waits for the host where the network publishes
/// it.
const LISTENING: u32 = 30;
/// How long, in seconds, a listener of the guest waits for the host where the network must not
/// publish it, all of which the host spends trying to reach it.
const UNPUBLISHED_LISTENING: u32 = 5;
/// A script that starts a descendant that leads a session of its own and holds the script's
/// output: the descendant prints its process ID, which is its session's ID, then moves its output
/// to the script's, which ends the command substitution that reads the ID. The script prints
/// `started` once `/proc` shows that the descendant leads its session.
const DESCENDANT_SCRIPT: &str = "exec 3>&1; \
    descendant=$(setsid sh -c 'echo $$; exec sleep 600 >&3 3>&-' &); \
    read -r pid comm state parent group session rest </proc/$descendant/stat; \
    [ -n \"$descendant\" ] && [ \"$session\" = \"$descendant\" ] && echo started";
/// A script that prints its process's user ID, group ID, and groups, and its permitted,
/// effective, and bounding capability sets, which the shell reads itself.
const IDENTITY_SCRIPT: &str = "id -u; id -g; id -G; \
    while read -r key value; do \
    case $key in CapPrm:|CapEff:|CapBnd:) echo \"$key $value\";; esac; \
    done </proc/self/status";
/// A script that prints the number of the guest's network devices besides loopback, as
/// `/sys/class/net` lists them, `devices N`, then how OpenVMM asked NVX's `/init` to publish the
/// guest's listeners on the kernel command line, `ingress MODE`, if it did.
const DEVICE_SCRIPT: &str = "echo \"devices $(ls /sys/class/net | grep -cvx lo)\"; \
    sed -n 's/.* virtnet_ingress=\\([^ ]*\\).*/ingress \\1/p' /proc/cmdline";
/// A script that requests its first argument, [`PROXY_TARGET`], through the proxy that its
/// environment names to `wget` in `http_proxy`, and prints `proxy STATUS RESPONSE`, then tries each
/// destination that its other arguments name, as `PROTOCOL NAME ADDRESS PORT`, with `nc`, which
/// sends a UDP probe's name as its datagram, and prints `NAME STATUS`. Over TCP, the status is 0
/// once the connection opened, and 1 once it failed; a UDP sender cannot see a lost datagram.
const PROBE_SCRIPT: &str = "response=$(wget -T 5 -qO- \"$1\" 2>&1); \
    echo \"proxy $? $response\"; shift; \
    while [ \"$#\" -ge 4 ]; do \
    case $1 in \
    tcp) nc -z -w 2 \"$3\" \"$4\" </dev/null >/dev/null 2>&1 ;; \
    udp) printf '%s\\n' \"$2\" | nc -u -w 1 \"$3\" \"$4\" >/dev/null 2>&1 ;; \
    *) exit 2 ;; \
    esac; \
    echo \"$2 $?\"; shift 4; \
    done";
/// A script that listens at the guest's TCP port `$1` for one client, for at most `$2` seconds,
/// greets it with the line `$3`, prints what it sends until it closes its side, then prints
/// `listener STATUS`: 0 once the client closed, and 124 once the time ran out without one.
const LISTENER_SCRIPT: &str =
    "printf '%s\\n' \"$3\" | timeout \"$2\" nc -l -p \"$1\"; echo \"listener $?\"";

/// The image of a scenario's root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Image {
    /// NVX's Ubuntu EROFS image.
    Erofs,
    /// A GPT disk that the scenario writes; see the [module documentation](self).
    Gpt,
}

/// The top layer of a scenario's GPT disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TopLayer {
    /// A [`small_layer`] that holds [`LAYER_FILE`] and [`LAYER_DIRECTORY`].
    Sound,
    /// That layer, corrupt: its top directory's inode has no mode, an error that the kernel finds
    /// when it mounts the layer, and its superblock asks the kernel to panic on an error.
    Corrupt,
    /// The layer with a cyclic journal, but no `needs_recovery` bit.
    CyclicJournal,
    /// The same journal, with `needs_recovery` set, which the assembly must refuse.
    NeedsRecovery,
}

/// The identity that a scenario's launch expects of the image.
#[derive(Clone, Copy, Debug)]
enum Identity {
    /// The image's own, from its manifest.
    Own,
    /// Another, which the image does not have.
    Other,
}

/// The upper of a scenario's root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Upper {
    /// A copy of the scratch template.
    Scratch,
    /// RAM, without a scratch.
    Ram,
}

/// The network of a scenario's launch, which the launch reads from a file that the test writes,
/// in the serde form of a plan's network: the guest at [`GUEST_NETWORK`], with egress denied and
/// no host-loopback access unless the network says otherwise. A network names the ports of the
/// [`Host`] side that the test binds before the launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Network {
    /// A proxy-only network, whose one way out is the test proxy on host loopback, with the scheme
    /// `http`, which the guest reaches at the same port of its gateway, at [`Host::proxy_url`].
    Proxy,
    /// Ingress allowed, so that OpenVMM publishes the guest's listeners to the host's private
    /// networks.
    Ingress,
    /// Egress allowed, and host-loopback access in both directions: the guest reaches the test
    /// proxy and every probe's destination, and OpenVMM publishes its listeners on host loopback.
    /// It is the positive control of the networks that stop traffic; see [`Expect::Controlled`].
    Open,
    /// Everything denied, without a proxy, which attaches no network device.
    Deny,
    /// Egress denied but for one rule, which allows TCP to the gateway at the port of
    /// [`Destination::GatewayPort`], with outbound host-loopback access, so that the guest
    /// reaches that service of host loopback alone.
    AllowRule,
    /// Egress allowed, with outbound host-loopback access: the guest reaches host loopback, and
    /// the host none of the guest's listeners.
    Outbound,
    /// Host-loopback access in both directions, with egress denied: the host reaches the guest's
    /// listeners on host loopback, at their ports, and the guest reaches nothing, since its own
    /// connections follow the egress policy.
    HostLoopback,
    /// Host-loopback access in both directions, with egress denied, and a forward from the port
    /// [`Host::forward`] of host loopback to the guest's port [`Host::listener`].
    Forward,
}

impl Network {
    /// Returns the network's serde form, which names the ports of the `host`.
    fn form(self, host: &Host) -> Value {
        let mut form = json!({
            "guestNetwork": GUEST_NETWORK,
            "egress": "deny",
            "allow": {"name": "egress.allow", "rules": []},
            "deny": {"name": "egress.deny", "rules": []},
            "ingress": "deny",
            "hostLoopback": "deny",
            "hostLoopbackForwards": [],
            "proxy": null,
        });
        match self {
            Self::Proxy => form["proxy"] = json!({"port": host.proxy_port(), "scheme": "http"}),
            Self::Ingress => form["ingress"] = json!("allow"),
            Self::Open => {
                form["egress"] = json!("allow");
                form["hostLoopback"] = json!("allow");
            }
            Self::Deny => {}
            Self::AllowRule => {
                let port = host.port(Destination::GatewayPort);
                form["allow"]["rules"] = json!([{
                    "to": [{"cidr": format!("{GATEWAY}/32"), "except": []}],
                    "ports": [{"protocol": "tcp", "port": port, "endPort": null}],
                }]);
                form["hostLoopback"] = json!("outbound");
            }
            Self::Outbound => {
                form["egress"] = json!("allow");
                form["hostLoopback"] = json!("outbound");
            }
            Self::HostLoopback => form["hostLoopback"] = json!("allow"),
            Self::Forward => {
                form["hostLoopback"] = json!("allow");
                form["hostLoopbackForwards"] = json!([{
                    "protocol": "tcp",
                    "hostPort": host.forward,
                    "guestPort": host.listener,
                }]);
            }
        }
        form
    }

    /// Returns whether OpenVMM publishes the guest's listeners through the network, as the root
    /// configuration says.
    fn listeners(self) -> Listeners {
        match self {
            Self::Ingress | Self::Open | Self::HostLoopback | Self::Forward => Listeners::Published,
            Self::Proxy | Self::Deny | Self::AllowRule | Self::Outbound => Listeners::Unpublished,
        }
    }

    /// Returns how OpenVMM asks NVX's `/init` to publish the guest's listeners on the kernel
    /// command line, `virtnet_ingress=MODE`, if it does: to the host's private networks, `allow`,
    /// or on host loopback alone, `host-loopback`.
    fn ingress(self) -> Option<&'static str> {
        match self {
            Self::Ingress => Some("allow"),
            Self::Open | Self::HostLoopback | Self::Forward => Some("host-loopback"),
            Self::Proxy | Self::Deny | Self::AllowRule | Self::Outbound => None,
        }
    }

    /// Returns whether the network lets the guest reach the test proxy, and the destinations of
    /// the probes that it lets the guest reach.
    fn reaches(self) -> (bool, &'static [Destination]) {
        match self {
            Self::Open | Self::Outbound => (true, &Destination::ALL),
            Self::Proxy => (true, &[]),
            Self::AllowRule => (false, &[Destination::GatewayPort]),
            Self::Ingress | Self::Deny | Self::HostLoopback | Self::Forward => (false, &[]),
        }
    }
}

/// The destination of a probe besides the proxy: a socket of the host, which the guest reaches
/// at its gateway, or at the host's own address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Destination {
    /// Another port of host IPv4 loopback than the proxy's, over TCP.
    GatewayPort,
    /// The proxy's port of host IPv4 loopback, over UDP.
    GatewayUdp,
    /// The proxy's port at the host's own address, over TCP, if the host has one.
    HostAddress,
    /// The proxy's port of host IPv6 loopback, at the gateway's IPv6 address, over TCP, if the
    /// host has IPv6 loopback.
    GatewayIpv6,
}

impl Destination {
    /// Every destination.
    const ALL: [Self; 4] = [
        Self::GatewayPort,
        Self::GatewayUdp,
        Self::HostAddress,
        Self::GatewayIpv6,
    ];

    /// Returns the destination's name, which the probe prints.
    fn name(self) -> &'static str {
        match self {
            Self::GatewayPort => "gateway-port",
            Self::GatewayUdp => "gateway-udp",
            Self::HostAddress => "host-address",
            Self::GatewayIpv6 => "gateway-ipv6",
        }
    }
}

/// The socket of the host at which a probe's traffic arrives if the network lets it through.
enum HostSocket {
    /// A TCP listener, at which a connection arrives.
    Tcp(TcpListener),
    /// A UDP socket, at which a datagram arrives.
    Udp(UdpSocket),
}

/// A probe of a scenario's network: a destination, and the host's socket there.
struct Probe {
    /// The destination.
    destination: Destination,
    /// The address at which the guest reaches the destination.
    address: IpAddr,
    /// The destination's port.
    port: u16,
    /// The host's socket.
    socket: HostSocket,
}

impl Probe {
    /// Returns the probe's protocol, as the probe script names it.
    fn protocol(&self) -> &'static str {
        match self.socket {
            HostSocket::Tcp(_) => "tcp",
            HostSocket::Udp(_) => "udp",
        }
    }
}

/// The host's side of a scenario's network, which the test binds before the launch, so that the
/// network can name its ports: the test proxy, as NVX's proxy-only scenario has it, the sockets
/// of the probes' destinations, and the ports at which the host reaches a listener of the guest.
/// It holds its ports in [`HELD_PORTS`] until it drops.
struct Host {
    /// The test proxy's listener on host IPv4 loopback, which the guest reaches at the same port of
    /// its gateway.
    proxy: TcpListener,
    /// The probes, in the order of [`Destination::ALL`], without those that the host lacks.
    probes: Vec<Probe>,
    /// A port that no socket holds for TCP or UDP, on every host address or loopback alone, at
    /// which a listener of the guest listens, and OpenVMM publishes it on host loopback.
    listener: u16,
    /// A free port of host IPv4 loopback, which a [`Network::Forward`] forwards to the guest's
    /// port [`Self::listener`].
    forward: u16,
}

impl Host {
    /// Binds the test proxy's listener on host IPv4 loopback, at a port at which it binds the
    /// probes' sockets too: a UDP socket on host IPv4 loopback, a listener at the host's own
    /// address, if it has one, and a listener on host IPv6 loopback, if it has that; a listener at
    /// another port of host IPv4 loopback; and draws the ports of a listener of the guest and of a
    /// forward. It draws no port that another host's side holds, and holds its own.
    fn bind() -> Self {
        // The host's sides draw their ports one at a time, past those that the others hold.
        let mut held = HELD_PORTS.lock().unwrap_or_else(PoisonError::into_inner);
        let address = host_address();
        let ipv6 = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_ok();
        let other = draw(&held, |port| TcpListener::bind((Ipv4Addr::LOCALHOST, port)));
        let other_port = other.local_addr().unwrap().port();
        let (proxy, probes) = draw(&held, |port| {
            let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
            let gateway = IpAddr::V4(GATEWAY);
            let mut probes = vec![Probe {
                destination: Destination::GatewayUdp,
                address: gateway,
                port,
                socket: HostSocket::Udp(UdpSocket::bind((Ipv4Addr::LOCALHOST, port))?),
            }];
            if let Some(address) = address {
                probes.push(Probe {
                    destination: Destination::HostAddress,
                    address,
                    port,
                    socket: HostSocket::Tcp(TcpListener::bind((address, port))?),
                });
            }
            if ipv6 {
                probes.push(Probe {
                    destination: Destination::GatewayIpv6,
                    address: IpAddr::V6(GATEWAY_IPV6),
                    port,
                    socket: HostSocket::Tcp(TcpListener::bind((Ipv6Addr::LOCALHOST, port))?),
                });
            }
            Ok((proxy, probes))
        });
        let proxy_port = proxy.local_addr().unwrap().port();
        let mut probes = probes;
        probes.insert(
            0,
            Probe {
                destination: Destination::GatewayPort,
                address: IpAddr::V4(GATEWAY),
                port: other_port,
                socket: HostSocket::Tcp(other),
            },
        );
        // OpenVMM publishes a listener of the guest at the same port of host IPv4 and IPv6
        // loopback, for TCP and UDP, after it checks, on Windows, every host address of each
        // family there, and binds a forward's port on host IPv4 loopback. Windows lets a socket
        // bind every host address beside one that binds loopback alone, and the reverse, so a port
        // is free only where both bind, for each transport and family, and none of the host's own
        // sockets takes it.
        let mut addresses = vec![
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        ];
        if ipv6 {
            addresses.extend([
                IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
            ]);
        }
        let free = |port: u16, taken: &[u16]| -> io::Result<u16> {
            if taken.contains(&port) {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "a socket of the host's side takes it",
                ));
            }
            for &address in &addresses {
                TcpListener::bind((address, port))?;
                UdpSocket::bind((address, port))?;
            }
            Ok(port)
        };
        let listener = draw(&held, |port| free(port, &[proxy_port, other_port]));
        let forward = draw(&held, |port| {
            free(port, &[proxy_port, other_port, listener])
        });
        let host = Self {
            proxy,
            probes,
            listener,
            forward,
        };
        held.extend(host.ports());
        host
    }

    /// Returns the ports that the host's side holds: the test proxy's, which the probes share, the
    /// other port of host loopback, and the ports of a listener of the guest and of a forward.
    fn ports(&self) -> [u16; 4] {
        [
            self.proxy_port(),
            self.port(Destination::GatewayPort),
            self.listener,
            self.forward,
        ]
    }

    /// Returns the test proxy's port.
    fn proxy_port(&self) -> u16 {
        self.proxy.local_addr().unwrap().port()
    }

    /// Returns the URL of the test proxy as the guest sees it, at its gateway.
    fn proxy_url(&self) -> String {
        format!("http://{GATEWAY}:{}", self.proxy_port())
    }

    /// Returns the port of the probe of `destination`, which the host always has.
    fn port(&self, destination: Destination) -> u16 {
        self.probes
            .iter()
            .find(|probe| probe.destination == destination)
            .map(|probe| probe.port)
            .unwrap()
    }

    /// Discards the connections and datagrams that are left at the host's sockets once a VM
    /// ended, so that the checks of the next VM see its own traffic alone.
    fn drain(&self) {
        while accepted(&self.proxy, Duration::ZERO) {}
        for probe in &self.probes {
            match &probe.socket {
                HostSocket::Tcp(listener) => while accepted(listener, Duration::ZERO) {},
                HostSocket::Udp(socket) => {
                    socket.set_nonblocking(true).unwrap();
                    while socket.recv(&mut [0; 4096]).is_ok() {}
                }
            }
        }
    }
}

impl Drop for Host {
    /// Releases the host's ports once its scenario ended, which confirmed that its VMs ended. A
    /// scenario that fails keeps them, since one of its VMs might still run, and bind them.
    fn drop(&mut self) {
        if thread::panicking() {
            return;
        }
        let mut held = HELD_PORTS.lock().unwrap_or_else(PoisonError::into_inner);
        for port in self.ports() {
            held.remove(&port);
        }
    }
}

/// Returns what `bind` makes of a port that the host draws from [`PORTS`], independently each
/// time, past the ports that `held` names, until it succeeds, at most [`PORT_ATTEMPTS`] times.
fn draw<T>(held: &BTreeSet<u16>, mut bind: impl FnMut(u16) -> io::Result<T>) -> T {
    let (first, last) = PORTS;
    let random = RandomState::new();
    let mut refused = Vec::new();
    for attempt in 0..PORT_ATTEMPTS {
        let offset = random.hash_one(attempt) % u64::from(last - first + 1);
        let port = first + u16::try_from(offset).unwrap();
        if held.contains(&port) {
            refused.push(format!("{port}: another scenario holds it"));
            continue;
        }
        match bind(port) {
            Ok(bound) => return bound,
            Err(error) => refused.push(format!("{port}: {error}")),
        }
    }
    panic!("no port drawn from {first} to {last} was free: {refused:?}")
}

/// Returns the host's own address on its default IPv4 route, if it has one.
fn host_address() -> Option<IpAddr> {
    let route = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // Connecting a UDP socket sends nothing: it selects the source address of the route to an
    // address reserved for documentation.
    route.connect((Ipv4Addr::new(198, 51, 100, 1), 9)).ok()?;
    let address = route.local_addr().ok()?.ip();
    (!address.is_loopback() && !address.is_unspecified()).then_some(address)
}

/// The layered root of a scenario's launch.
#[derive(Clone, Copy, Debug)]
struct Root {
    /// The image.
    image: Image,
    /// The top layer of a GPT disk.
    top_layer: TopLayer,
    /// The identity that the launch expects of the image.
    identity: Identity,
    /// The upper.
    upper: Upper,
    /// The guest's runtime directory, as the launch passes it, unless the default.
    runtime: Option<&'static str>,
    /// The overlay's mode, as the launch names it, unless the guest's default.
    overlay: Option<OverlayMode>,
    /// The scratch's journal policy, as the launch names it, unless the guest's default.
    journal: Option<JournalPolicy>,
    /// The bound of the first partition, as the launch names it, unless the guest's default.
    first_partition_bound: Option<u64>,
    /// The sandbox's workload identity, its user ID and its group ID, as the launch names it, if
    /// any.
    workload: Option<(u32, u32)>,
    /// The host paths that the launch shares with the root.
    table: Table,
    /// The VM's network, as the launch reads it, if any.
    network: Option<Network>,
}

/// The root of the scenarios unless they say otherwise: NVX's EROFS image, with its own identity,
/// over a copy of the scratch template, with the guest's defaults, and no share.
const ROOT: Root = Root {
    image: Image::Erofs,
    top_layer: TopLayer::Sound,
    identity: Identity::Own,
    upper: Upper::Scratch,
    runtime: None,
    overlay: None,
    journal: None,
    first_partition_bound: None,
    workload: None,
    table: Table::None,
    network: None,
};

/// The root of the scenarios of a GPT disk unless they say otherwise: the disk, with its own GUID,
/// over a copy of the scratch template, with the guest's defaults.
const GPT: Root = Root {
    image: Image::Gpt,
    ..ROOT
};

/// The image file of a scenario's launch.
struct Distro {
    /// The file.
    path: PathBuf,
    /// The image's format, as the launch names it.
    format: &'static str,
    /// The identity that the launch expects of the image.
    identity: String,
}

impl Root {
    /// Returns the image file of the launch, from the `fixtures`, which writes a GPT disk into
    /// `directory` first.
    fn distro(&self, fixtures: &Fixtures, directory: &Path) -> Distro {
        let (path, format, own) = match self.image {
            Image::Erofs => (fixtures.distro.clone(), "erofs", fixtures.identity.clone()),
            Image::Gpt => {
                let path = directory.join("distro.gpt");
                write_gpt_disk(&path, &fixtures.scratch, self.top_layer);
                (path, "gpt", DISK_GUID.to_owned())
            }
        };
        let identity = match self.identity {
            Identity::Own => own,
            Identity::Other => other_identity(&own),
        };
        Distro {
            path,
            format,
            identity,
        }
    }

    /// Returns the options that name the images of the launch, which staging takes as they are:
    /// the image `distro`, and the `scratch` image for a root over a scratch.
    fn images(&self, distro: &Distro, scratch: &Path) -> Vec<OsString> {
        let mut image = OsString::from(format!("{}:", distro.format));
        image.push(&distro.path);
        let mut options = [
            "--distro".into(),
            image,
            "--distro-identity".into(),
            distro.identity.clone().into(),
        ]
        .to_vec();
        if self.upper == Upper::Scratch {
            let mut upper = OsString::from("scratch:");
            upper.push(scratch);
            options.extend(["--upper".into(), upper]);
        }
        options
    }

    /// Returns the launch's options of the root, with the image `distro`, the `scratch` image for a
    /// root over a scratch, and the file `network` of the network of a root with one: its
    /// [images](Self::images), an upper in RAM for a root that is not over a scratch, the options
    /// of the root configuration that the root names, and the network's file.
    fn options(&self, distro: &Distro, scratch: &Path, network: &Path) -> Vec<OsString> {
        let mut options = self.images(distro, scratch);
        if self.upper == Upper::Ram {
            options.extend(["--upper".into(), "ram".into()]);
        }
        if let Some(runtime) = self.runtime {
            options.extend(["--runtime-dir".into(), runtime.into()]);
        }
        if let Some(mode) = self.overlay {
            options.extend(["--overlay-mode".into(), mode.as_str().into()]);
        }
        if let Some(policy) = self.journal {
            options.extend(["--scratch-journal".into(), policy.as_str().into()]);
        }
        if let Some(bound) = self.first_partition_bound {
            options.extend(["--first-partition-bound".into(), bound.to_string().into()]);
        }
        if let Some((uid, gid)) = self.workload {
            options.extend(["--workload-identity".into(), format!("{uid}:{gid}").into()]);
        }
        if self.network.is_some() {
            options.extend(["--network".into(), network.into()]);
        }
        options
    }

    /// Returns the overlay's mode, the guest's default unless the launch names another.
    fn overlay_mode(&self) -> OverlayMode {
        self.overlay.unwrap_or(OverlayMode::TrustedLowers)
    }

    /// Returns the serde form of the root's network, which names the ports of the `host`, or
    /// `null` without a network.
    fn network_form(&self, host: Option<&Host>) -> Value {
        match (self.network, host) {
            (Some(network), Some(host)) => network.form(host),
            _ => Value::Null,
        }
    }
}

/// Returns a UUID other than `identity`, which differs in its last digit.
fn other_identity(identity: &str) -> String {
    let (start, last) = identity.split_at(identity.len() - 1);
    format!("{start}{}", if last == "0" { "1" } else { "0" })
}

/// The host paths that a scenario's launch shares with the root, from the [`SOURCES`] that
/// [`write_sources`] writes: none, the [`SHARES`], or those and one more share, which the guest,
/// or the host program before it spawns the VM, refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Table {
    /// No share: the launch exports nothing, and the mapping table is empty.
    None,
    /// The [`SHARES`]: a read-only directory, with a read-write directory inside it, a read-only
    /// file, and a read-write directory, in a table of four mappings, which the plan orders by the
    /// depth of their targets, then by their targets.
    Maps,
    /// The [`SHARES`], then `out` again, onto `/usr/lib/os-release`, a file of NVX's Ubuntu image:
    /// the fourth mapping of the table, a directory onto a file, which the guest refuses.
    Failing,
    /// The [`SHARES`], then a read-only share inside `out`, which the guest may write.
    ReadOnlyInsideWritable,
    /// The [`SHARES`], then a share at a target inside the [`ADAPTER_TREE`], which the adapter
    /// reserves.
    InReservedTree,
    /// The [`SHARES`], then a share at a target that would cover the [`ADAPTER_TREE`].
    OverReservedTree,
    /// The [`SHARES`], then the [`OUTSIDE`] directory, which the adapter does not authorize.
    Unauthorized,
}

impl Table {
    /// Returns the launch's options of the table, with the sources in `sandbox`, the sandbox's
    /// directory: each share, with the [`ADAPTER_TREE`] reserved for the scenarios of reserved
    /// targets, `dir/secret` hidden, and the [`SOURCES`] as the one host directory that the
    /// adapter authorizes.
    fn options(self, sandbox: &Path) -> Vec<OsString> {
        let sources = sandbox.join(SOURCES);
        let extra = match self {
            Self::None => return Vec::new(),
            Self::Maps => None,
            Self::Failing => Some((source(&sources, "out"), "/usr/lib/os-release")),
            Self::ReadOnlyInsideWritable => {
                Some((source(&sources, "out/inner"), "/srv/maps/out/inner:ro"))
            }
            Self::InReservedTree => Some((source(&sources, "dir/inner"), "/srv/adapter/inner")),
            Self::OverReservedTree => Some((source(&sources, "out"), "/srv")),
            Self::Unauthorized => Some((sandbox.join(OUTSIDE), "/srv/outside")),
        };
        let shares = SHARES
            .iter()
            .map(|(path, target)| (source(&sources, path), *target))
            .chain(extra);
        let mut options = Vec::new();
        for (host, target) in shares {
            let mut share = host.into_os_string();
            share.push(":");
            share.push(target);
            options.extend(["--share".into(), share]);
        }
        if matches!(self, Self::InReservedTree | Self::OverReservedTree) {
            options.extend(["--reserved".into(), ADAPTER_TREE.into()]);
        }
        options.extend([
            "--deny".into(),
            source(&sources, "dir/secret").into_os_string(),
            "--share-root".into(),
            sources.into_os_string(),
        ]);
        options
    }

    /// Returns how many mappings the root configuration of a launch with the table announces.
    fn mapping_count(self) -> u32 {
        match self {
            Self::Maps => 4,
            Self::Failing => 5,
            Self::None
            | Self::ReadOnlyInsideWritable
            | Self::InReservedTree
            | Self::OverReservedTree
            | Self::Unauthorized => 0,
        }
    }
}

/// Returns the host path of `path`, a path in the `sources`, with components separated by `/`.
fn source(sources: &Path, path: &str) -> PathBuf {
    path.split('/')
        .fold(sources.to_path_buf(), |host, name| host.join(name))
}

/// What a scenario expects of its VM.
enum Expect {
    /// The guest assembles the root, the check probes the VM, and the stop tears the root down,
    /// which leaves a scratch unmounted.
    Ready(fn(&mut Vm<'_>)),
    /// As `Ready`, after the positive control of the scenario's network, as in NVX's proxy-only
    /// scenario: first a VM of an [`Network::Open`] network over the same host's side, in which
    /// every probe's traffic arrives at its socket, and the host reaches a listener of the guest at
    /// the guest's port of host loopback, which shows that the probes and the listener work, so
    /// that what does not arrive in the scenario's network shows its policy.
    Controlled(fn(&mut Vm<'_>)),
    /// The guest assembles the root, the check keeps it busy, and the stop reports the teardown
    /// that failed with this problem, which leaves a scratch mounted when the VM ends.
    TeardownFails(fn(&mut Vm<'_>), &'static str),
    /// The guest reports that the stage failed with this problem, and the VM ends with its
    /// status, without a scratch mounted: the guest fails each stage but the identity's before it
    /// mounts the scratch, and unmounts the root that it assembled once the identity's failed.
    Fails(RootStage, &'static str),
    /// The guest assembles the root, applies this many mappings of the table, and fails the next
    /// with a problem that holds this text: the launch sends no more of the table, nor `Start`, and
    /// stops the VM, whose guest tears the root down, and which leaves the host's sources as they
    /// were.
    TableFails(u32, &'static str),
    /// The host program refuses the launch's shares with this problem, before it spawns the VM.
    Unlaunched(&'static str),
    /// The scenario makes this change between its staging and its launch, which the host program
    /// then refuses with this problem, before it spawns the VM. Once the scenario stages the
    /// images again, the guest assembles the root, which [`check_root`] probes, and the stop tears
    /// it down.
    Refused(Change, &'static str),
}

/// What a scenario changes between its staging and its launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    /// It stages nothing.
    Unstaged,
    /// It changes the image in place: it writes its first sector again, as it was, then moves its
    /// modification time an hour on, so that the change shows whatever the granularity of the
    /// file system's times.
    Image,
    /// It replaces the scratch image with a copy of the template, a file of its own, which takes
    /// the image's path.
    Scratch,
}

impl Change {
    /// Makes the change to the image `distro` or to the `scratch` image, a copy of the template in
    /// the `fixtures`.
    fn make(self, distro: &Distro, scratch: &Path, fixtures: &Fixtures) {
        match self {
            Self::Unstaged => {}
            Self::Image => {
                let mut file = File::options()
                    .read(true)
                    .write(true)
                    .open(&distro.path)
                    .unwrap();
                let modified = file.metadata().unwrap().modified().unwrap();
                let mut sector = [0; SECTOR as usize];
                file.read_exact(&mut sector).unwrap();
                file.seek(SeekFrom::Start(0)).unwrap();
                file.write_all(&sector).unwrap();
                file.set_modified(modified + Duration::from_secs(3600))
                    .unwrap();
            }
            Self::Scratch => {
                let copy = scratch.with_extension("copy");
                fs::copy(&fixtures.scratch, &copy).unwrap();
                fs::rename(&copy, scratch).unwrap();
            }
        }
    }
}

/// A scenario: a VM with a layered root, and what the scenario expects of it.
struct Scenario {
    /// The root of the VM.
    root: Root,
    /// What the scenario expects.
    expect: Expect,
}

/// A VM of a scenario, whose guest assembled its root.
struct Vm<'a> {
    /// The scenario's sandbox.
    sandbox: &'a mut Sandbox,
    /// What the launch printed.
    launched: Printed,
    /// The guest's runtime directory, below which it assembled the root.
    runtime: &'static str,
    /// The scenario's root.
    root: Root,
    /// The root configuration that the launch sent.
    config: RootConfig,
    /// The host's side of the VM's network, if it has one.
    host: Option<&'a Host>,
}

impl Vm<'_> {
    /// Runs `probes` in the VM's root, and returns what they found, each fact as `PROBE.KEY`.
    fn probe(&mut self, probes: &[&str]) -> Printed {
        let probes = probes.iter().map(OsStr::new).collect::<Vec<_>>();
        let printed = self.sandbox.run("probe", &probes, 0);
        assert_eq!(printed["reattached"], "live");
        assert_eq!(printed["pid"], self.launched["pid"]);
        printed
    }

    /// Runs `executions` at once in the VM's root, each the options and the arguments of one
    /// execution of the host program's `exec`, and returns what they did, each fact as
    /// `exec.N.KEY`, for execution N, from 1.
    fn exec(&mut self, executions: &[Vec<String>]) -> Printed {
        let mut options = Vec::new();
        for (index, execution) in executions.iter().enumerate() {
            if index > 0 {
                options.push(OsStr::new(";"));
            }
            options.extend(execution.iter().map(OsStr::new));
        }
        let printed = self.sandbox.run("exec", &options, 0);
        assert_eq!(printed["reattached"], "live");
        assert_eq!(printed["pid"], self.launched["pid"]);
        printed
    }
}

/// Declares the table of scenarios: each row, `NAME: SCENARIO;`, is an ignored test, `NAME`, which
/// runs `SCENARIO`.
macro_rules! scenarios {
    ($($(#[doc = $doc:literal])* $name:ident: $scenario:expr;)*) => {
        $(
            $(#[doc = $doc])*
            #[test]
            #[ignore = "boots a VM; see the module documentation for what it needs"]
            fn $name() {
                run(stringify!($name), $scenario);
            }
        )*
    };
}

scenarios! {
    /// An EROFS image over an ext4 scratch, of the identity that the launch expects: the merged
    /// root's contents and mounts, a copy-up of metadata alone, the guest's disks, the agent's and
    /// the workload's cgroups, PID 1's unrelated child, and a clean teardown.
    erofs_over_scratch: Scenario {
        root: ROOT,
        expect: Expect::Ready(check_erofs_over_scratch),
    };
    /// The image over the scratch, in the untrusted-lowers mode, with a journal that the launch
    /// allows: a copy-up of the data too, and a rename of a lower directory that fails.
    erofs_over_scratch_with_untrusted_lowers: Scenario {
        root: Root {
            overlay: Some(OverlayMode::UntrustedLowers),
            journal: Some(JournalPolicy::Allowed),
            ..ROOT
        },
        expect: Expect::Ready(check_root),
    };
    /// The image over a tmpfs in RAM, of the kernel's default size, without a scratch device.
    erofs_over_ram: Scenario {
        root: Root { upper: Upper::Ram, ..ROOT },
        expect: Expect::Ready(check_root),
    };
    /// The image over a tmpfs in RAM, in the untrusted-lowers mode.
    erofs_over_ram_with_untrusted_lowers: Scenario {
        root: Root {
            upper: Upper::Ram,
            overlay: Some(OverlayMode::UntrustedLowers),
            ..ROOT
        },
        expect: Expect::Ready(check_root),
    };
    /// A GPT disk over an ext4 scratch, of the disk GUID that the launch expects: its first
    /// partition, intact; its layers, each mounted read-only from its partition, with partition
    /// 2's at the overlay's base and partition 3's at its top; a copy-up of metadata alone, the
    /// rename of a lower directory, and the guest's disks.
    gpt_over_scratch: Scenario {
        root: GPT,
        expect: Expect::Ready(check_root),
    };
    /// The disk over the scratch, in the untrusted-lowers mode: a copy-up of the data too, and a
    /// rename of a lower directory that fails.
    gpt_over_scratch_with_untrusted_lowers: Scenario {
        root: Root {
            overlay: Some(OverlayMode::UntrustedLowers),
            ..GPT
        },
        expect: Expect::Ready(check_root),
    };
    /// The disk over a tmpfs in RAM, without a scratch device, with a bound that the first
    /// partition meets exactly.
    gpt_over_ram: Scenario {
        root: Root {
            upper: Upper::Ram,
            first_partition_bound: Some(FIRST_PARTITION_SIZE),
            ..GPT
        },
        expect: Expect::Ready(check_root),
    };
    /// The disk over a tmpfs in RAM, in the untrusted-lowers mode.
    gpt_over_ram_with_untrusted_lowers: Scenario {
        root: Root {
            upper: Upper::Ram,
            overlay: Some(OverlayMode::UntrustedLowers),
            ..GPT
        },
        expect: Expect::Ready(check_root),
    };
    /// A scratch that a file fills, which then refuses writes until the file is gone.
    full_scratch_refuses_writes: Scenario {
        root: ROOT,
        expect: Expect::Ready(check_full_upper),
    };
    /// A tmpfs that a file fills, at half of the guest's memory, which then refuses writes until
    /// the file is gone.
    full_ram_upper_refuses_writes: Scenario {
        root: Root { upper: Upper::Ram, ..ROOT },
        expect: Expect::Ready(check_full_upper),
    };
    /// A scratch with a journal, which the launch forbids, and the assembly refuses.
    forbidden_journal_fails_the_assembly: Scenario {
        root: Root { journal: Some(JournalPolicy::Forbidden), ..ROOT },
        expect: Expect::Fails(
            RootStage::Assembly,
            "has an ext4 journal, which the assembly forbids",
        ),
    };
    /// An image of another identity than the launch expects, which the assembly refuses.
    other_identity_fails_the_assembly: Scenario {
        root: Root { identity: Identity::Other, ..ROOT },
        expect: Expect::Fails(RootStage::Assembly, "the distro layer's UUID is "),
    };
    /// A GPT disk of another GUID than the launch expects, which the assembly refuses.
    other_disk_guid_fails_the_assembly: Scenario {
        root: Root { identity: Identity::Other, ..GPT },
        expect: Expect::Fails(
            RootStage::Assembly,
            "invalid partition table: the disk GUID is 6c1f5e0a-2b7d-4c39-9e81-3a5d0f7b2c64, not \
             the expected ",
        ),
    };
    /// A first partition one byte larger than the launch's bound, which the assembly refuses.
    first_partition_over_its_bound_fails_the_assembly: Scenario {
        root: Root { first_partition_bound: Some(FIRST_PARTITION_SIZE - 1), ..GPT },
        expect: Expect::Fails(
            RootStage::Assembly,
            "the first partition has 4096 bytes, more than the bound of 4095",
        ),
    };
    /// A GPT disk whose top layer is corrupt and asks the kernel to panic on an error, which the
    /// kernel refuses to mount, without a panic, and the assembly with it.
    corrupt_layer_fails_the_assembly: Scenario {
        root: Root { top_layer: TopLayer::Corrupt, ..GPT },
        expect: Expect::Fails(
            RootStage::Assembly,
            "failed to mount the layer in partition 3: ",
        ),
    };
    /// A cyclic journal must not hold up assembly, reads, copy-up, or teardown.
    cyclic_journal_is_not_loaded: Scenario {
        root: Root {
            top_layer: TopLayer::CyclicJournal,
            upper: Upper::Ram,
            overlay: Some(OverlayMode::UntrustedLowers),
            ..GPT
        },
        expect: Expect::Ready(check_root),
    };
    /// Suppressing journal loading must not let a layer that needs recovery mount.
    recovery_required_layer_fails_the_assembly: Scenario {
        root: Root { top_layer: TopLayer::NeedsRecovery, ..GPT },
        expect: Expect::Fails(
            RootStage::Assembly,
            "needs a journal recovery, which its read-only device can't take",
        ),
    };
    /// A relative runtime directory, which the guest refuses with the configuration.
    invalid_configuration_fails_its_stage: Scenario {
        root: Root { runtime: Some("run/runtime-adapter"), ..ROOT },
        expect: Expect::Fails(RootStage::Configuration, "which is not absolute"),
    };
    /// A root that a probe holds open, whose teardown fails.
    busy_root_fails_the_teardown: Scenario {
        root: ROOT,
        expect: Expect::TeardownFails(hold_root, "failed to unmount the overlay: "),
    };
    /// The host's mapping table, which the guest maps beneath the root from the host export before
    /// the sandbox starts: the export, mounted out of the root; the mappings, mounted in the plan's
    /// order, each with its access, `nosuid`, and `nodev`, on targets created on the overlay; a
    /// file that the launch hides; and writes that reach the host's writable sources alone. Then,
    /// from the export, through mappers of the adapter's own: the refusal of a link and of a magic
    /// link at each component, of the wrong kinds, of a target that would be created inside
    /// another mapping, and of reserved targets, the runtime's, the container root's, and the
    /// adapter's own; and a read-only mapping that is never writable. The stop unmounts the
    /// mappings and the export before the overlay.
    erofs_over_scratch_maps_the_table: Scenario {
        root: Root { table: Table::Maps, ..ROOT },
        expect: Expect::Ready(check_table_and_mappers),
    };
    /// The table over a tmpfs in RAM.
    erofs_over_ram_maps_the_table: Scenario {
        root: Root { upper: Upper::Ram, table: Table::Maps, ..ROOT },
        expect: Expect::Ready(check_table),
    };
    /// The table on a GPT disk, whose layers hold no `/srv`, over the scratch.
    gpt_over_scratch_maps_the_table: Scenario {
        root: Root { table: Table::Maps, ..GPT },
        expect: Expect::Ready(check_table),
    };
    /// The table on the disk over a tmpfs in RAM.
    gpt_over_ram_maps_the_table: Scenario {
        root: Root { upper: Upper::Ram, table: Table::Maps, ..GPT },
        expect: Expect::Ready(check_table),
    };
    /// A mapping of the table that the guest refuses, which blocks the start: the launch stops the
    /// VM, which tears the root down.
    a_failed_mapping_tears_the_vm_down: Scenario {
        root: Root { table: Table::Failing, ..ROOT },
        expect: Expect::TableFails(
            3,
            "failed to mount the mapping at /usr/lib/os-release: the target exists as another kind \
             than its source",
        ),
    };
    /// A read-only share inside a read-write one, which the host could not keep read-only, and
    /// which the launch refuses before it spawns the VM.
    a_read_only_share_inside_a_writable_one_is_refused_before_launch: Scenario {
        root: Root { table: Table::ReadOnlyInsideWritable, ..ROOT },
        expect: Expect::Unlaunched("where the host cannot keep it read-only"),
    };
    /// A share inside a tree that the adapter reserves, which the launch refuses before it spawns
    /// the VM.
    a_share_inside_a_reserved_tree_is_refused_before_launch: Scenario {
        root: Root { table: Table::InReservedTree, ..ROOT },
        expect: Expect::Unlaunched(
            "has the guest target /srv/adapter/inner, which would cover the guest's /srv/adapter",
        ),
    };
    /// A share that would cover a tree that the adapter reserves, which the launch refuses before
    /// it spawns the VM.
    a_share_over_a_reserved_tree_is_refused_before_launch: Scenario {
        root: Root { table: Table::OverReservedTree, ..ROOT },
        expect: Expect::Unlaunched(
            "has the guest target /srv, which would cover the guest's /srv/adapter",
        ),
    };
    /// A share outside the host directories that the adapter authorizes, which the launch refuses
    /// before it spawns the VM.
    an_unauthorized_host_path_is_refused_before_launch: Scenario {
        root: Root { table: Table::Unauthorized, ..ROOT },
        expect: Expect::Unlaunched(
            "lies outside the host directories that --share-root authorizes",
        ),
    };
    /// Executions that enter the root as root profiles do, through `nvx::guest::entry`: user ID
    /// 0 with each allowed capability alone, with none, and with every one in each publication
    /// state, and a numeric identity, each PID 1 of its namespaces, with the root moved over `/`,
    /// without the descriptor that its caller leaked or a signal that it blocks, and contained by
    /// a nested `chroot`. No
    /// working directory reached through a magic link starts a program. No root
    /// workload lowers the unprivileged port start or binds a privileged port while listeners are
    /// published, and the entry refuses the capabilities outside its allowlist. The root's own
    /// machine ID, then a generated one, with the time status that the guest's time daemon
    /// publishes, in which a root workload finds no lock of the daemon's, both of which the
    /// runtime binds in a mount namespace of its own.
    root_profiles_enter_the_root: Scenario {
        root: ROOT,
        expect: Expect::Ready(check_root_profiles),
    };
    /// Images that the store does not stage, which the launch refuses before it spawns the VM.
    unstaged_images_are_refused_before_spawn: Scenario {
        root: ROOT,
        expect: Expect::Refused(Change::Unstaged, "the sandbox's store stages no image at "),
    };
    /// A GPT disk that changed after it was staged, which the launch refuses before it spawns the
    /// VM.
    a_changed_image_is_refused_before_spawn: Scenario {
        root: GPT,
        expect: Expect::Refused(
            Change::Image,
            "changed after its distro binding was made: its modification time differs",
        ),
    };
    /// A scratch image that another file replaced after it was staged, which the launch refuses
    /// before it spawns the VM.
    a_replaced_scratch_is_refused_before_spawn: Scenario {
        root: ROOT,
        expect: Expect::Refused(
            Change::Scratch,
            "changed after its scratch binding was made: its identity differs",
        ),
    };
    /// Executions that the host program runs at once over one session, in a sandbox without a
    /// workload identity, so that each start names its own: output beyond 1 MiB, whole; several
    /// executions that meet only while all of them run; a descendant in a session of its own,
    /// which holds its program's output, and which the end of its program ends; a user other than
    /// root, and root with an allowed capability alone; a working directory, an exact
    /// environment, and the default hostname, each as PID 1 of its namespaces. The guest
    /// refuses a start without an identity, and one with a capability outside the entry's
    /// allowlist, at stage 0.
    executions_run_in_the_root: Scenario {
        root: ROOT,
        expect: Expect::Ready(check_executions),
    };
    /// Executions in a sandbox whose workload identity the launch names, which every execution
    /// runs as: the guest refuses a start that names an identity of its own.
    executions_run_as_the_workload_identity: Scenario {
        root: Root { workload: Some(NOBODY), ..ROOT },
        expect: Expect::Ready(check_workload_executions),
    };
    /// A workload identity without an account in the image, which fails the identity stage once
    /// the guest assembled the root, before it answers the root configuration.
    accountless_workload_identity_fails_its_stage: Scenario {
        root: Root { workload: Some(ACCOUNTLESS), ..ROOT },
        expect: Expect::Fails(
            RootStage::Identity,
            "cannot enter the root as the workload identity 4242:4242: ",
        ),
    };
    /// Executions in a proxy-only network, which publishes no listener: each finds the proxy's URL
    /// in its environment, after its own entries, unless it names its own proxy settings, and root
    /// may hold the capabilities that bind privileged ports and open raw sockets. On Linux, `exec`
    /// refuses before it attaches a batch with a start that the proxy's URL takes past the framing.
    executions_take_the_settings_of_a_proxy_only_network: Scenario {
        root: Root { network: Some(Network::Proxy), ..ROOT },
        expect: Expect::Ready(check_proxy_settings),
    };
    /// Executions in a network that publishes the guest's listeners: the guest refuses root the
    /// capabilities that could forge a listener announcement at stage 0, but not the others, and
    /// an execution's environment, without a proxy, is exactly its start's.
    published_listeners_refuse_binding_and_raw_sockets: Scenario {
        root: Root { network: Some(Network::Ingress), ..ROOT },
        expect: Expect::Ready(check_published_listeners),
    };
    /// A network that denies everything, without a proxy, which attaches no network device: an
    /// execution reaches neither the test proxy nor any probe's destination.
    a_denied_network_lets_no_probe_through: Scenario {
        root: Root { network: Some(Network::Deny), ..ROOT },
        expect: Expect::Controlled(check_probes),
    };
    /// Egress denied but for an allow rule, TCP to a port of host loopback at the gateway, with
    /// outbound host-loopback access: an execution reaches that port alone.
    an_allow_rule_lets_only_its_destination_through: Scenario {
        root: Root { network: Some(Network::AllowRule), ..ROOT },
        expect: Expect::Controlled(check_probes),
    };
    /// A proxy-only network: an execution reaches the test proxy through the URL in its
    /// environment, and none of the probes' destinations, which never arrive.
    a_proxy_only_network_lets_only_its_proxy_through: Scenario {
        root: Root { network: Some(Network::Proxy), ..ROOT },
        expect: Expect::Controlled(check_probes),
    };
    /// Outbound host-loopback access, with egress allowed: an execution reaches host loopback,
    /// but the host does not reach the guest's listener, which OpenVMM does not publish.
    outbound_host_loopback_publishes_no_listener: Scenario {
        root: Root { network: Some(Network::Outbound), ..ROOT },
        expect: Expect::Controlled(check_host_loopback),
    };
    /// Host-loopback access allowed, with egress denied: the host reaches the guest's listener at
    /// the guest's port of host loopback, while an execution reaches nothing.
    allowed_host_loopback_publishes_the_listeners: Scenario {
        root: Root { network: Some(Network::HostLoopback), ..ROOT },
        expect: Expect::Controlled(check_host_loopback),
    };
    /// A forward of a port of host loopback to a guest port: the host reaches the guest's listener
    /// through it.
    a_forward_reaches_its_guest_port: Scenario {
        root: Root { network: Some(Network::Forward), ..ROOT },
        expect: Expect::Ready(check_forward),
    };
}

/// Runs the scenario `name`, `scenario`, after the positive control that it expects, if any, over
/// the same host's side of its network.
fn run(name: &str, scenario: Scenario) {
    let fixtures = Fixtures::find();
    let host = scenario.root.network.map(|_| Host::bind());
    if matches!(scenario.expect, Expect::Controlled(_)) {
        let host = host.as_ref().expect("a controlled scenario has a network");
        let control = Scenario {
            root: Root {
                network: Some(Network::Open),
                ..ROOT
            },
            expect: Expect::Ready(check_host_loopback),
        };
        // The control's sandbox has the scenario's name, so that the paths in it, such as a
        // Linux control socket's, are no longer than the scenario's own. It is gone before the
        // scenario's begins.
        run_vm(name, &fixtures, control, Some(host));
        host.drain();
    }
    run_vm(name, &fixtures, scenario, host.as_ref());
}

/// Runs the VM of the scenario `name`, `scenario`, from the `fixtures`, with the `host` side of its
/// network, if it has one.
fn run_vm(name: &str, fixtures: &Fixtures, scenario: Scenario, host: Option<&Host>) {
    let mut sandbox = Sandbox::new(name);
    let root = scenario.root;
    let scratch = sandbox.root.join("scratch.ext4");
    let over_scratch = root.upper == Upper::Scratch;
    if over_scratch {
        fs::copy(&fixtures.scratch, &scratch).unwrap();
    }
    let distro = root.distro(fixtures, &sandbox.root);
    let images = root.images(&distro, &scratch);
    let images = images.iter().map(OsString::as_os_str).collect::<Vec<_>>();
    let network = sandbox.root.join("network.json");
    if root.network.is_some() {
        let form = root.network_form(host);
        fs::write(&network, form.to_string()).unwrap();
    }
    let mut options = root.options(&distro, &scratch, &network);
    if root.table != Table::None {
        write_sources(&sandbox.root);
        options.extend(root.table.options(&sandbox.root));
    }
    let options = options.iter().map(OsString::as_os_str).collect::<Vec<_>>();
    if let Expect::Refused(change, problem) = scenario.expect {
        if change != Change::Unstaged {
            sandbox.run("stage", &images, 0);
        }
        change.make(&distro, &scratch, fixtures);
        let refused = sandbox.launch(&options, FAILED);
        assert!(refused.is_empty(), "{refused:?}");
        let stderr = sandbox.stderr();
        assert!(
            stderr.contains(problem) && stderr.contains("; stage the images first"),
            "{stderr}"
        );
        sandbox.assert_unlaunched();
    }
    // Each launch checks the images that the scenario staged.
    sandbox.run("stage", &images, 0);
    let (check, failure) = match scenario.expect {
        Expect::Fails(stage, problem) => {
            let launched = sandbox.launch(&options, ROOT_FAILED);
            assert_failed(&launched, stage, problem);
            assert_eq!(launched["stopped"], "graceful");
            // The launch recorded what it launched, with the root configuration that it sent, which
            // the guest refused, or accepted before a later stage failed.
            check_record(&sandbox, &root, &distro, &scratch, host);
            // The launch ended: no runtime record names the VM.
            assert_eq!(sandbox.record(), None);
            assert!(
                !(over_scratch && needs_recovery(&scratch)),
                "the guest left the scratch mounted"
            );
            return;
        }
        Expect::TableFails(mapped, problem) => {
            let launched = sandbox.launch(&options, TABLE_FAILED);
            assert_eq!(launched["root"], "ready", "{launched:?}");
            assert_eq!(launched["build"], GUEST_BUILD);
            // The guest applied the mappings before the one that it refused, and the launch sent
            // neither the rest of the table nor `Start`.
            assert_eq!(launched["table"], "failed", "{launched:?}");
            assert_eq!(launched["mapped"], mapped.to_string(), "{launched:?}");
            assert!(launched["problem"].contains(problem), "{launched:?}");
            assert!(!launched.contains_key("sandbox"), "{launched:?}");
            // The launch stopped the VM, whose guest tore the root down, mappings included.
            assert_eq!(launched["stopped"], "graceful", "{launched:?}");
            assert_eq!(launched["outcome"], "success 0", "{launched:?}");
            assert_eq!(launched["teardown"], "complete", "{launched:?}");
            check_record(&sandbox, &root, &distro, &scratch, host);
            assert_eq!(sandbox.record(), None);
            assert!(
                !(over_scratch && needs_recovery(&scratch)),
                "the stop left the scratch mounted"
            );
            check_sources(&sandbox.root, false);
            return;
        }
        Expect::Unlaunched(problem) => {
            let refused = sandbox.launch(&options, FAILED);
            assert!(refused.is_empty(), "{refused:?}");
            let stderr = sandbox.stderr();
            assert!(stderr.contains(problem), "{stderr}");
            sandbox.assert_unlaunched();
            check_sources(&sandbox.root, false);
            return;
        }
        Expect::Ready(check) | Expect::Controlled(check) => (check, None),
        Expect::TeardownFails(check, problem) => (check, Some(problem)),
        Expect::Refused(..) => (check_root as fn(&mut Vm<'_>), None),
    };
    let launched = sandbox.launch(&options, 0);
    assert_eq!(launched["root"], "ready");
    // The guest agent of the build that the host program ships assembled the root, applied the
    // host's mapping table, and started the sandbox.
    assert_eq!(launched["build"], GUEST_BUILD);
    assert_eq!(launched["table"], "applied");
    assert_eq!(launched["mapped"], root.table.mapping_count().to_string());
    assert_eq!(launched["sandbox"], "started");
    let config = check_record(&sandbox, &root, &distro, &scratch, host);
    let vmm = sandbox
        .vmm()
        .expect("the VM runs after its launcher exited");
    let mut vm = Vm {
        sandbox: &mut sandbox,
        launched: launched.clone(),
        runtime: root.runtime.unwrap_or(DEFAULT_RUNTIME),
        root,
        config,
        host,
    };
    if let Some(network) = root.network {
        check_device(&mut vm, network);
    }
    check(&mut vm);

    let status = if failure.is_some() { ROOT_FAILED } else { 0 };
    let stopped = sandbox.run("stop", &[], status);
    assert_eq!(stopped["reattached"], "live");
    assert_eq!(stopped["pid"], launched["pid"]);
    assert_eq!(stopped["stopped"], "graceful");
    assert!(vmm.has_exited().unwrap());
    assert_eq!(
        sandbox.record(),
        None,
        "the stop cleared the runtime record"
    );
    match failure {
        None => {
            assert!(!stopped.contains_key("root"), "{stopped:?}");
            assert_eq!(stopped["outcome"], "success 0");
            assert_eq!(stopped["teardown"], "complete");
            assert!(
                !(over_scratch && needs_recovery(&scratch)),
                "the stop left the scratch mounted"
            );
        }
        Some(problem) => {
            assert_failed(&stopped, RootStage::Teardown, problem);
            assert!(
                !over_scratch || needs_recovery(&scratch),
                "the failed teardown unmounted the scratch"
            );
        }
    }
}

/// Checks that a command printed that the guest reported that `stage` failed with `problem`, and
/// that the VM then ended with the stage's status and a complete teardown.
fn assert_failed(printed: &Printed, stage: RootStage, problem: &str) {
    assert_eq!(printed["root"], "failed", "{printed:?}");
    assert_eq!(printed["stage"], stage.as_str(), "{printed:?}");
    assert!(printed["problem"].contains(problem), "{printed:?}");
    let outcome = format!("guest-exit {}", stage.exit_status());
    assert_eq!(printed["outcome"], outcome, "{printed:?}");
    assert_eq!(printed["teardown"], "complete", "{printed:?}");
}

/// Checks what the launch recorded in the sandbox's store: a VM with the layered `root` of the
/// image `distro`, over `scratch` or RAM, the image of the identity that the launch expects, the
/// bindings that the store stages for the images, which the launch checked, the network, if any, as
/// the scenario's file gave it, with the ports of the `host`, the boot tokens of its plan alone,
/// after the launch's own, and the root configuration that the host sent, with listeners published
/// as OpenVMM publishes them through the network, and unpublished without one, whether or not the
/// guest accepted it, which it returns.
fn check_record(
    sandbox: &Sandbox,
    root: &Root,
    distro: &Distro,
    scratch: &Path,
    host: Option<&Host>,
) -> RootConfig {
    let read = |name: &str| -> Value {
        serde_json::from_str(&fs::read_to_string(sandbox.store.join(name)).unwrap()).unwrap()
    };
    let record = read(RECORD_NAME);
    assert_eq!(record["format"], 1);
    assert_eq!(record["mode"], "layered");
    let staged = read(STAGED_NAME);
    let scratch_binding = match root.upper {
        Upper::Scratch => staged["scratch"]["binding"].clone(),
        Upper::Ram => Value::Null,
    };
    assert_eq!(
        record["bindings"],
        json!({"distro": staged["distro"]["binding"], "scratch": scratch_binding})
    );
    assert_ne!(record["bindings"]["distro"], Value::Null);
    let plan = &record["plan"];
    assert_eq!(plan["distro"]["format"], distro.format);
    assert_eq!(plan["distro"]["identity"], distro.identity.as_str());
    assert_eq!(plan["settings"], Value::Null);
    assert_eq!(plan["network"], root.network_form(host));
    let path = |value: &Value| PathBuf::from(value.as_str().unwrap());
    assert_eq!(path(&plan["distro"]["path"]), distro.path);
    let upper = match root.upper {
        Upper::Scratch => {
            assert_eq!(plan["upper"]["kind"], "scratch");
            assert_eq!(path(&plan["upper"]["path"]), scratch);
            "nvx_scratch=0xd0006000,ext4"
        }
        Upper::Ram => {
            assert_eq!(plan["upper"], json!({"kind": "ram"}));
            "nvx_overlay_upper=ramfs"
        }
    };
    let tokens = record["kernelCommandLine"]
        .as_str()
        .unwrap()
        .split(' ')
        .collect::<BTreeSet<_>>();
    let layer = format!(
        "nvx_layer=distro,0xd0003000,{}:{}",
        distro.format, distro.identity
    );
    assert_eq!(
        tokens,
        BTreeSet::from(["nvx_sandbox=1", layer.as_str(), upper])
    );
    let sectors = |path: &Path| Some(fs::metadata(path).unwrap().len() / 512);
    let config = RootConfig {
        runtime: root.runtime.unwrap_or(DEFAULT_RUNTIME).to_owned(),
        distro_sectors: sectors(&distro.path),
        scratch_sectors: (root.upper == Upper::Scratch)
            .then(|| sectors(scratch))
            .flatten(),
        first_partition_bound: root.first_partition_bound,
        overlay_mode: root.overlay,
        scratch_journal: root.journal,
        mapping_count: root.table.mapping_count(),
        workload_identity: root.workload,
        listeners: Some(
            root.network
                .map_or(Listeners::Unpublished, Network::listeners),
        ),
        ..RootConfig::default()
    };
    let sent = String::from_utf8(config.encode()).unwrap();
    assert_eq!(record["rootConfiguration"], sent.as_str());
    config
}

/// Returns whether the ext4 file system at `path` needs its journal recovered: whether a mount
/// left its superblock's `needs_recovery` feature set, which a clean unmount clears.
fn needs_recovery(path: &Path) -> bool {
    let mut superblock = [0; 1024];
    let mut file = File::open(path).unwrap();
    file.seek(SeekFrom::Start(1024)).unwrap();
    file.read_exact(&mut superblock).unwrap();
    let magic = u16::from_le_bytes([superblock[0x38], superblock[0x39]]);
    assert_eq!(
        magic,
        0xef53,
        "{} holds an ext4 file system",
        path.display()
    );
    let incompatible = u32::from_le_bytes(superblock[0x60..0x64].try_into().unwrap());
    incompatible & 0x4 != 0
}

/// Returns the options of a list of mount options, separated by commas.
fn options(list: &str) -> BTreeSet<&str> {
    list.split(',').collect()
}

/// Checks the root of a scenario's VM, which the guest assembled, as its image, its upper, and its
/// overlay mode make it: its mounts, its contents, its first partition, a copy-up, a rename, and
/// the guest's disks.
fn check_root(vm: &mut Vm<'_>) {
    let image = vm.root.image;
    let (lowers, file, directory) = match image {
        Image::Erofs => (&["distro"][..], "usr/lib/os-release", "mnt"),
        Image::Gpt => (&["p2", "p3"][..], LAYER_FILE, LAYER_DIRECTORY),
    };
    let copy_up = format!("copy-up {file}");
    let rename = format!("rename {directory}");
    let facts = vm.probe(&[
        "mounts",
        "contents",
        &copy_up,
        &rename,
        "devices",
        "first-partition",
    ]);
    let runtime = vm.runtime;
    let (upper, kind, other) = match vm.root.upper {
        Upper::Scratch => ("scratch", "ext4", "tmpfs"),
        Upper::Ram => ("tmpfs", "tmpfs", "scratch"),
    };

    // The guest's disks, whatever their names: the image's, read-only, and the scratch's,
    // writable, unless the upper is in RAM.
    let disks = facts["devices.disks"].split(' ').collect::<Vec<_>>();
    let mut sizes = disks
        .iter()
        .map(|disk| disk.split_once(':').map_or(*disk, |(_, size)| size))
        .collect::<Vec<_>>();
    sizes.sort_unstable();
    let mut expected = vec![format!("{}:ro", vm.config.distro_sectors.unwrap())];
    expected.extend(
        vm.config
            .scratch_sectors
            .map(|sectors| format!("{sectors}:rw")),
    );
    expected.sort_unstable();
    assert_eq!(sizes, expected, "{facts:?}");

    // The image's layers, read-only, under the overlay, whose upper and work directories are on
    // the upper, all once, below the runtime directory, where no other upper is mounted. The
    // layers of a GPT disk are ext4 file systems on its partitions, whose journals are not loaded.
    let layer = match image {
        Image::Erofs => ("erofs", &["ro"][..]),
        Image::Gpt => ("ext4", &["ro", "nosuid", "nodev"][..]),
    };
    let mounts = lowers
        .iter()
        .map(|lower| (*lower, layer.0, layer.1))
        .chain([
            (upper, kind, &["rw", "nosuid", "nodev"][..]),
            ("root", "overlay", &["rw", "nosuid", "nodev"]),
        ]);
    for (mount, kind, flags) in mounts {
        assert_eq!(facts[&format!("mounts.{mount}.count")], "1", "{mount}");
        assert_eq!(facts[&format!("mounts.{mount}.type")], kind, "{mount}");
        let found = options(&facts[&format!("mounts.{mount}.options")]);
        assert!(
            found.is_superset(&flags.iter().copied().collect()),
            "{mount}: {found:?}"
        );
    }
    assert_eq!(facts[&format!("mounts.{other}.count")], "0", "{other}");
    if image == Image::Gpt {
        let disk = disks
            .iter()
            .find_map(|disk| disk.strip_suffix(":ro"))
            .and_then(|disk| disk.split_once(':'))
            .map(|(name, _)| name)
            .expect("the guest has the disk's device");
        for lower in lowers {
            let partition = lower.trim_start_matches('p');
            let source = &facts[&format!("mounts.{lower}.source")];
            assert_eq!(source, &format!("/dev/{disk}{partition}"), "{lower}");
            let found = options(&facts[&format!("mounts.{lower}.super")]);
            assert!(
                found.contains("noload") || found.contains("norecovery"),
                "{lower}: {found:?}"
            );
        }
    }
    if vm.root.upper == Upper::Ram {
        // A tmpfs of the kernel's default size, whose root directory has mode 0755.
        let tmpfs = options(&facts["mounts.tmpfs.super"]);
        assert!(tmpfs.contains("mode=755"), "{tmpfs:?}");
        let sized = tmpfs.iter().any(|option| option.starts_with("size="));
        assert!(!sized, "{tmpfs:?}");
    }
    // The overlay stacks the layers with the base last, after the top.
    let stacked = lowers
        .iter()
        .rev()
        .map(|lower| format!("{runtime}/layers/{lower}"))
        .collect::<Vec<_>>()
        .join(":");
    let layers = [
        format!("lowerdir={stacked}"),
        format!("upperdir={runtime}/{upper}/upper"),
        format!("workdir={runtime}/{upper}/work"),
    ];
    let overlay = options(&facts["mounts.root.super"]);
    for option in &layers {
        assert!(overlay.contains(option.as_str()), "{option} in {overlay:?}");
    }

    // The overlay's mode. Trusting its lowers, the overlay copies up a file's metadata alone,
    // which leaves the data in its layer, and renames a lower directory, with the options that
    // the mode and metadata copy-up set. Otherwise, it copies up the data too, and refuses the
    // rename, with these options off.
    let trusting = ["metacopy=on", "redirect_dir=on", "xino=on"];
    let trusted = vm.root.overlay_mode() == OverlayMode::TrustedLowers;
    for option in trusting {
        assert_eq!(overlay.contains(option), trusted, "{option} in {overlay:?}");
    }
    let (data, rename) = if trusted {
        ("none", "allowed")
    } else {
        ("copied", "refused")
    };
    assert_eq!(facts["copy-up.upper.size"], facts["copy-up.lower.size"]);
    assert_eq!(facts["copy-up.upper.data"], data, "{facts:?}");
    assert_eq!(facts["copy-up.root.data"], "intact");
    assert_eq!(facts["rename.outcome"], rename, "{facts:?}");

    // The merged root: the image's files, and a file that the worker wrote, which reaches the
    // upper and not the top layer, which refuses writes. A GPT disk's layers hold no
    // `os-release`.
    let identifier = match image {
        Image::Erofs => "ubuntu",
        Image::Gpt => "absent",
    };
    assert_eq!(facts["contents.os-release.id"], identifier);
    assert_eq!(facts["contents.root.mode"], "755");
    assert_eq!(facts["contents.write.root"], "runtime-adapter");
    assert_eq!(facts["contents.write.upper"], "runtime-adapter");
    assert_eq!(facts["contents.write.lower"], "absent");
    assert_eq!(facts["contents.lower.write"], "refused");

    // The first partition of a GPT disk, which the assembly returned as it was, without its
    // padding.
    let (length, bytes) = match image {
        Image::Erofs => ("none".to_owned(), "none".to_owned()),
        Image::Gpt => (
            FIRST_PARTITION.len().to_string(),
            hexadecimal(FIRST_PARTITION),
        ),
    };
    assert_eq!(facts["first-partition.length"], length, "{facts:?}");
    assert_eq!(facts["first-partition.bytes"], bytes, "{facts:?}");
}

/// Fills the upper of a scenario's root through the root, and checks that the upper refuses
/// writes once full, until the file that filled it is gone.
fn check_full_upper(vm: &mut Vm<'_>) {
    let facts = vm.probe(&["fill"]);
    let number = |fact: &str| facts[&format!("fill.{fact}")].parse::<u64>().unwrap();
    assert_eq!(facts["fill.error"], "ENOSPC", "{facts:?}");
    let (available, written) = (number("available"), number("written"));
    assert!(
        written <= available && available - written < FULL_SLACK,
        "{facts:?}"
    );
    assert_eq!(facts["fill.after"], "runtime-adapter");
    if vm.root.upper == Upper::Ram {
        // A tmpfs of the kernel's default size: half of the guest's memory, in whole pages.
        let pages = number("memory") / PAGE;
        assert_eq!(number("total"), pages / 2 * PAGE, "{facts:?}");
    }
}

/// Checks the VM of the `erofs_over_scratch` scenario: its root, the agent's and the workload's
/// cgroups, and the guest's processes.
fn check_erofs_over_scratch(vm: &mut Vm<'_>) {
    check_root(vm);
    let facts = vm.probe(&["cgroups", "processes"]);

    // The agent's and the workload's cgroups, siblings in the cgroup v2 hierarchy, with the
    // configuration's resource values, and no user namespaces.
    let config = &vm.config;
    assert_eq!(facts["cgroups.parent.type"], "cgroup2");
    assert_eq!(facts["cgroups.agent.exists"], "yes");
    assert_eq!(facts["cgroups.workload.exists"], "yes");
    let values = [
        ("cgroups.agent.memory.low", config.agent_memory_low),
        ("cgroups.workload.memory.max", config.workload_memory_max),
        ("cgroups.workload.pids.max", config.workload_pids_max),
    ];
    for (fact, value) in values {
        assert_eq!(facts[fact], value.unwrap().to_string(), "{fact}");
    }
    assert_eq!(facts["cgroups.workload.processes"], "0");
    let user_namespaces = facts["cgroups.user-namespaces"].as_str();
    assert!(
        ["0", "unavailable"].contains(&user_namespaces),
        "{user_namespaces}"
    );

    // The worker is not PID 1, but its child. PID 1's unrelated child runs on, untouched: in PID
    // 1's root directory and namespaces, like the worker, and in the agent's cgroup, into which
    // the bootstrap moved every process of the guest.
    let agent = Path::new(&config.agent_cgroup);
    let parent = agent.parent().unwrap();
    let agent = format!("/{}", agent.strip_prefix(parent).unwrap().display());
    assert_ne!(facts["processes.worker.pid"], "1");
    assert_eq!(facts["processes.worker.ppid"], "1");
    assert_eq!(facts["processes.sibling.ppid"], "1");
    let state = facts["processes.sibling.state"].as_str();
    assert!(["S", "R"].contains(&state), "{state}");
    let commands = [
        ("pid1", "init /dev/hvc2 layered"),
        ("worker", "worker /dev/hvc2"),
        ("sibling", "idle"),
    ];
    for (process, command) in commands {
        let found = &facts[&format!("processes.{process}.command")];
        let expected = format!("/sbin/runtime-adapter-guest {command}");
        assert!(found.starts_with(&expected), "{process}: {found}");
        assert_eq!(
            facts[&format!("processes.{process}.root")],
            "/",
            "{process}"
        );
        let namespaces = &facts[&format!("processes.{process}.namespaces")];
        assert_eq!(namespaces, "same", "{process}");
        assert_eq!(
            facts[&format!("processes.{process}.cgroup")],
            agent,
            "{process}"
        );
    }
    let sibling = &facts["processes.sibling.pid"];
    assert!(
        facts["processes.worker.command"].ends_with(&format!(" {sibling}")),
        "{facts:?}"
    );

    // Zero mappings do no work: the launch exported nothing, so the guest mounted no export, and
    // nothing is mounted below the root.
    let facts = vm.probe(&["map"]);
    assert_eq!(facts["map.export.count"], "0", "{facts:?}");
    assert!(!facts.contains_key("map.export.type"), "{facts:?}");
    assert_eq!(facts["map.hostfs.mode"], "absent", "{facts:?}");
    assert_eq!(facts["map.mounts"], "", "{facts:?}");
    assert_eq!(facts["map.latched"], "no", "{facts:?}");
}

/// Keeps the root of the `busy_root_fails_the_teardown` scenario busy.
fn hold_root(vm: &mut Vm<'_>) {
    let facts = vm.probe(&["hold"]);
    assert_eq!(facts["hold.held"], format!("{}/rootfs", vm.runtime));
}

/// Checks the mappings of the table of [`SHARES`], which the guest mapped beneath the root from
/// the host export before the sandbox started, and the host's sources, which they show.
fn check_table(vm: &mut Vm<'_>) {
    let facts = vm.probe(&[MAP_TABLE]);
    let fact = |key: &str| facts[&format!("map.{key}")].as_str();

    // The export, mounted once, read-write since some of its children are, without set-user-ID
    // programs or devices, below a directory of the runtime's that only root may enter.
    assert_eq!(fact("export.count"), "1");
    assert_eq!(fact("export.type"), "virtiofs");
    let export = options(fact("export.options"));
    assert!(
        export.is_superset(&BTreeSet::from(["rw", "nosuid", "nodev"])),
        "{export:?}"
    );
    assert_eq!((fact("hostfs.mode"), fact("hostfs.owner")), ("0700", "0:0"));

    // The table's mappings, and nothing else, in the plan's order: by the depth of their targets,
    // then by their targets.
    assert_eq!(
        fact("mounts"),
        "/srv/maps/dir,/srv/maps/file.txt,/srv/maps/out,/srv/maps/dir/inner"
    );
    // Each mapping has its access, `nosuid`, and `nodev`, and the guest may write the writable
    // ones alone, the one inside a read-only mapping included.
    for (name, access, write) in [
        ("dir", "ro", "refused"),
        ("inner", "rw", "allowed"),
        ("file", "ro", "refused"),
        ("out", "rw", "allowed"),
    ] {
        let found = options(fact(&format!("{name}.options")));
        for flag in [access, "nosuid", "nodev"] {
            assert!(found.contains(flag), "{name}: {found:?}");
        }
        assert_eq!(fact(&format!("{name}.write")), write, "{name}");
    }
    // What each shows, as the host holds it, but for the file that the launch hides: its directory
    // does not list it, and the host refuses its lookup.
    assert_eq!(
        (fact("dir.kind"), fact("dir.entries")),
        ("directory", "inner,marker")
    );
    assert_eq!(
        (fact("inner.kind"), fact("inner.entries")),
        ("directory", "")
    );
    assert_eq!((fact("file.kind"), fact("file.read")), ("file 4", "file"));
    assert_eq!(
        (fact("out.kind"), fact("out.entries")),
        ("directory", "inner,marker")
    );
    assert_eq!(
        (fact("secret.options"), fact("secret.kind")),
        ("none", "denied")
    );
    // The guest created the missing targets on the overlay, in its upper, empty: those of the
    // outermost mappings, but none inside another mapping.
    for (name, upper) in [
        ("dir", "directory"),
        ("inner", "absent"),
        ("file", "file 0"),
        ("out", "directory"),
        ("secret", "absent"),
    ] {
        assert_eq!(fact(&format!("{name}.upper")), upper, "{name}");
    }
    assert_eq!(fact("latched"), "no");
    check_sources(&vm.sandbox.root, true);
}

/// Checks the VM of the `erofs_over_scratch_maps_the_table` scenario: its [table](check_table),
/// then mappers of the adapter's own, from the export.
fn check_table_and_mappers(vm: &mut Vm<'_>) {
    check_table(vm);
    let facts = vm.probe(&[MAP_CHECKS]);
    let check = |key: &str| facts[&format!("map-checks.{key}")].as_str();
    // No link or magic link at any component leads a mapping anywhere.
    for key in [
        "link.a",
        "link.b",
        "link.new",
        "link.up",
        "magic.self",
        "magic.thread-self",
        "magic.root",
        "magic.cwd",
        "magic.fd",
    ] {
        assert_eq!(check(key), "link", "{key}");
    }
    assert_eq!(check("kind.directory"), "target-kind");
    assert_eq!(check("kind.file"), "target-kind");
    // A mapping may go onto what an earlier one shows, but creates nothing inside it.
    assert_eq!(check("nested.onto"), "applied");
    assert_eq!(check("nested.create"), "foreign /srv/checks/m/new");
    assert_eq!(check("nested.teardown"), "complete");
    // The runtime's, the container root's, and the adapter's own reserved targets, and the
    // ancestors of reserved trees, but not what is below a reserved path.
    for key in [
        "run",
        "run-nvx",
        "time",
        "time-below",
        "proc",
        "sys",
        "dev",
        "agent",
        "machine-id",
        "etc",
        "adapter-tree",
        "adapter-ancestor",
        "adapter-path",
    ] {
        assert_eq!(check(&format!("reserved.{key}")), "reserved", "{key}");
    }
    assert_eq!(check("reserved.adapter-below"), "applied");
    assert_eq!(check("reserved.created"), "absent");
    // A read-only mapping of a directory that the guest may write through the export is
    // restricted before it is attached, so never writable.
    assert_eq!(check("read-only.attaches"), "200");
    assert_eq!(check("read-only.flags"), "ro,nosuid,nodev");
    assert_eq!(check("read-only.writable"), "never");
    // Nothing went outside the root, the checks unmounted what they mapped, and they wrote
    // nothing to the host's sources.
    assert_eq!(check("outside.entries"), "0");
    assert_eq!(check("outside.mounts"), "0");
    assert_eq!(check("left"), "0");
    check_sources(&vm.sandbox.root, true);
}

/// Writes the [`SOURCES`] that a scenario's mapping table shares, in `sandbox`, the sandbox's
/// directory: `dir`, with the [`SOURCE_FILES`] `marker` and `secret`, which the launch hides, and
/// the empty directory `inner`; `file.txt`; and `out`, with the file `marker` and the empty
/// directory `inner`. Beside them, the empty directory [`OUTSIDE`].
fn write_sources(sandbox: &Path) {
    let sources = sandbox.join(SOURCES);
    for directory in ["dir/inner", "out/inner"] {
        fs::create_dir_all(source(&sources, directory)).unwrap();
    }
    for (file, contents) in SOURCE_FILES {
        fs::write(source(&sources, file), contents).unwrap();
    }
    fs::create_dir(sandbox.join(OUTSIDE)).unwrap();
}

/// Checks the files of the [`SOURCES`] in `sandbox`, the sandbox's directory: those that
/// [`write_sources`] wrote, unchanged, and, once `written`, the files that the `map` probe wrote
/// through the writable mappings of `dir/inner` and `out`, and nothing else; and the empty
/// [`OUTSIDE`].
fn check_sources(sandbox: &Path, written: bool) {
    let mut expected: BTreeMap<String, Option<String>> = ["dir", "dir/inner", "out", "out/inner"]
        .into_iter()
        .map(|directory| (directory.to_owned(), None))
        .collect();
    for (file, contents) in SOURCE_FILES {
        expected.insert(file.to_owned(), Some(contents.to_owned()));
    }
    if written {
        for file in ["dir/inner/written", "out/written"] {
            expected.insert(file.to_owned(), Some("runtime-adapter".to_owned()));
        }
    }
    assert_eq!(tree(&sandbox.join(SOURCES)), expected);
    assert_eq!(tree(&sandbox.join(OUTSIDE)), BTreeMap::new());
}

/// Returns what the directory `directory` holds, at any depth: each path in it, with components
/// separated by `/`, and a file's contents, or `None` for a directory.
fn tree(directory: &Path) -> BTreeMap<String, Option<String>> {
    let mut found = BTreeMap::new();
    let mut pending = vec![(directory.to_path_buf(), String::new())];
    while let Some((path, prefix)) = pending.pop() {
        for entry in fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap();
            let name = format!("{prefix}{}", entry.file_name().to_str().unwrap());
            if entry.file_type().unwrap().is_dir() {
                pending.push((entry.path(), format!("{name}/")));
                found.insert(name, None);
            } else {
                found.insert(name, Some(fs::read_to_string(entry.path()).unwrap()));
            }
        }
    }
    found
}

/// Writes the GPT disk of the scenarios at `path`, with [`gpt::write_disk`], of its partitions in
/// this order, each aligned to [`ALIGNMENT`]:
///
/// 1. the first partition, of [`FIRST_PARTITION_SIZE`] bytes, which holds [`FIRST_PARTITION`],
///    then [`FIRST_PARTITION_PADDING`], then NUL bytes;
/// 2. the base layer, a copy of the scratch template `template`, an ext4 file system that
///    `mkfs.ext4` made, with a journal;
/// 3. the top layer, a [`small_layer`] that holds [`LAYER_FILE`] and [`LAYER_DIRECTORY`], with
///    the corruption or journal that `top_layer` names.
///
/// Then checks that the first partition holds [`FIRST_PARTITION`] without its padding, as the
/// guest returns it.
fn write_gpt_disk(path: &Path, template: &Path, top_layer: TopLayer) {
    let template_size = fs::metadata(template).unwrap().len();
    let contents = b"runtime-adapter "
        .iter()
        .copied()
        .cycle()
        .take(LAYER_FILE_SIZE)
        .collect::<Vec<_>>();
    let mut top = small_layer(&[(LAYER_FILE, contents.as_slice())], &[LAYER_DIRECTORY]);
    match top_layer {
        TopLayer::Sound => {}
        TopLayer::Corrupt => corrupt(&mut top),
        TopLayer::CyclicJournal => add_cyclic_journal(&mut top, false),
        TopLayer::NeedsRecovery => add_cyclic_journal(&mut top, true),
    }
    let first = [FIRST_PARTITION, FIRST_PARTITION_PADDING].concat();
    let partitions = [
        Partition {
            guid: PARTITION_GUIDS[0],
            size: FIRST_PARTITION_SIZE,
            contents: Contents::Bytes(&first),
        },
        Partition {
            guid: PARTITION_GUIDS[1],
            size: template_size,
            contents: Contents::File(template),
        },
        Partition {
            guid: PARTITION_GUIDS[2],
            size: u64::try_from(top.len()).unwrap(),
            contents: Contents::Bytes(&top),
        },
    ];
    let layout = Layout {
        guid: DISK_GUID,
        partitions: &partitions,
        alignment: ALIGNMENT,
        length: None,
    };
    assert_eq!(gpt::write_disk(path, &layout), FIRST_PARTITION);
}

/// Corrupts `layer`, a [`small_layer`]: its top directory's inode, inode 2, loses its mode, while
/// it keeps its links, an error that the kernel finds when it mounts the layer, and the error
/// policy of its superblock, `s_errors`, becomes 3, which asks the kernel to panic on an error.
fn corrupt(layer: &mut [u8]) {
    let policy = LAYER_BLOCK + 0x3c;
    layer[policy..policy + 2].copy_from_slice(&3_u16.to_le_bytes());
    // The inode table starts at block 5, with inodes of 128 bytes, from inode 1.
    let mode = 5 * LAYER_BLOCK + 128;
    layer[mode..mode + 2].fill(0);
}

/// Adds an internal journal whose nonempty ring contains only empty revoke records of one
/// sequence, so a journal scan wraps forever without finding a commit or another sequence.
/// Even with `needs_recovery` clear, loading the journal scans it to discard its old contents.
fn add_cyclic_journal(layer: &mut Vec<u8>, needs_recovery: bool) {
    const JOURNAL_BLOCKS: usize = 1024;
    assert_eq!(layer.len(), LAYER_BLOCKS * LAYER_BLOCK);
    layer.resize((LAYER_BLOCKS + JOURNAL_BLOCKS) * LAYER_BLOCK, 0);

    // The appended blocks are allocated. The original bitmap already marks everything past
    // the original file system's end used, so its free counts do not change.
    let superblock = LAYER_BLOCK;
    for (offset, value) in [
        (0x04, u32::try_from(LAYER_BLOCKS + JOURNAL_BLOCKS).unwrap()),
        (0x5c, 0x04),                                         // has_journal
        (0x60, 0x42 | if needs_recovery { 0x04 } else { 0 }), // filetype, extents
        (0xe0, 8),                                            // journal inode
    ] {
        layer[superblock + offset..superblock + offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    // Reserved inode 8 maps the whole journal with one extent.
    let inode = 5 * LAYER_BLOCK + 7 * 128;
    for (offset, value) in [
        (0x00, 0o100_600_u16),
        (0x1a, 1),
        (0x28, 0xf30a), // extent header
        (0x2a, 1),
        (0x2c, 4),
        (0x38, u16::try_from(JOURNAL_BLOCKS).unwrap()),
    ] {
        layer[inode + offset..inode + offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    for (offset, value) in [
        (0x04, u32::try_from(JOURNAL_BLOCKS * LAYER_BLOCK).unwrap()),
        (
            0x1c,
            u32::try_from(JOURNAL_BLOCKS * LAYER_BLOCK / 512).unwrap(),
        ),
        (0x20, 0x80000), // EXT4_EXTENTS_FL
        (0x3c, u32::try_from(LAYER_BLOCKS).unwrap()),
    ] {
        layer[inode + offset..inode + offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    let journal = LAYER_BLOCKS * LAYER_BLOCK;
    for (offset, value) in [
        (0x00, 0xc03b_3998_u32),
        (0x04, 4), // JBD2_SUPERBLOCK_V2
        (0x0c, u32::try_from(LAYER_BLOCK).unwrap()),
        (0x10, u32::try_from(JOURNAL_BLOCKS).unwrap()),
        (0x14, 1), // first block of the ring
        (0x18, 1), // sequence
        (0x1c, 1), // nonempty ring's start
        (0x28, 1), // revoke feature; no journal checksums
        (0x40, 1), // one user
    ] {
        layer[journal + offset..journal + offset + 4].copy_from_slice(&value.to_be_bytes());
    }
    layer.copy_within(superblock + 0x68..superblock + 0x78, journal + 0x30);
    for block in layer[journal + LAYER_BLOCK..].chunks_exact_mut(LAYER_BLOCK) {
        for (offset, value) in [(0, 0xc03b_3998_u32), (4, 5), (8, 1), (12, 16)] {
            block[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
        }
    }
}

/// The capabilities that an entry allows, by their names in the cases of the `entries` probe, each
/// with its number and the attempt of the probe that it alone allows, if any.
const CAPABILITIES: [(&str, u8, Option<&str>); 13] = [
    ("chown", 0, Some("chown")),
    ("dac-override", 1, Some("dac-override")),
    ("fowner", 3, Some("fowner")),
    ("fsetid", 4, None),
    ("kill", 5, None),
    ("setgid", 6, Some("setgid")),
    ("setuid", 7, Some("setuid")),
    ("setpcap", 8, None),
    ("net-bind-service", 10, Some("bind")),
    ("net-raw", 13, Some("raw")),
    ("sys-chroot", 18, Some("escape")),
    ("audit-write", 29, None),
    ("setfcap", 31, None),
];

/// The attempts of the `entries` probe that a capability allows, each with its outcome without
/// the capability.
const ATTEMPTS: [(&str, &str); 8] = [
    ("chown", "refused EPERM"),
    ("dac-override", "refused EACCES"),
    ("fowner", "refused EPERM"),
    ("setuid", "refused EPERM"),
    ("setgid", "refused EPERM"),
    ("bind", "refused EACCES"),
    ("raw", "refused EPERM"),
    ("escape", "refused EPERM"),
];

/// The machine ID that the `entries` probe writes in the root, as the root's own.
const ROOT_MACHINE_ID: &str = "0123456789abcdef0123456789abcdef";

/// Returns the outcome of the `entries` probe's `attempt` with the capability that allows it.
fn allowed(attempt: &str) -> &'static str {
    if attempt == "escape" {
        "contained"
    } else {
        "allowed"
    }
}

/// Returns a capability set as `/proc/PID/status` writes it: 16 hexadecimal digits.
fn capability_set(mask: u64) -> String {
    format!("{mask:016x}")
}

/// Checks the VM of the `root_profiles_enter_the_root` scenario.
fn check_root_profiles(vm: &mut Vm<'_>) {
    let facts = vm.probe(&["entries"]);
    let fact = |key: &str| {
        facts
            .get(&format!("entries.{key}"))
            .unwrap_or_else(|| panic!("entries.{key} is missing: {facts:?}"))
            .as_str()
    };

    // The entry refuses every capability outside its allowlist, the binding and raw-socket ones
    // while listeners are published, any for a user ID other than 0, and an identity that has no
    // account in existing-account mode, before an execution starts.
    assert_eq!(fact("refused.outside"), "51");
    assert_eq!(fact("refused.accepted"), "none");
    assert_eq!(
        fact("refused.published.net-bind-service"),
        "published CAP_NET_BIND_SERVICE"
    );
    assert_eq!(fact("refused.published.net-raw"), "published CAP_NET_RAW");
    assert_eq!(fact("refused.non-root"), "non-root-capabilities");
    assert_eq!(fact("refused.existing"), "account-missing");

    let cases = CAPABILITIES
        .iter()
        .map(|(name, ..)| format!("only-{name}"))
        .chain(["none", "published", "unpublished", "numeric", "generated"].map(str::to_owned))
        .collect::<Vec<_>>();
    let none = capability_set(0);
    for case in &cases {
        let fact = |key: &str| fact(&format!("{case}.{key}"));
        // Each execution starts, as PID 1 of its namespaces, and keeps only its standard streams:
        // the descriptor that its caller leaked closed. It holds no inheritable or ambient
        // capability, and no new privileges.
        assert_eq!((fact("launch"), fact("status")), ("started", "exit 0"));
        assert_eq!(fact("pid"), "1", "{case}");
        assert_eq!(fact("fds"), "0,1,2", "{case}");
        assert_eq!(fact("leaked"), "closed", "{case}");
        // Without a working directory, it runs in its root's top.
        assert_eq!(fact("cwd"), "/", "{case}");
        assert_eq!(fact("no-new-privs"), "1", "{case}");
        assert_eq!(
            (fact("cap.inh"), fact("cap.amb")),
            (&*none, &*none),
            "{case}"
        );
        // It inherits no signal that its caller blocks, `SIGCHLD`.
        assert_eq!(fact("signals.blocked"), none, "{case}");
        // Its `/proc/sys` and `/proc/sysrq-trigger` refuse writes, read-only for user ID 0, so no
        // execution lowers the unprivileged port start, which the runtime owns.
        let refused = if case == "numeric" {
            "refused EACCES"
        } else {
            "refused EROFS"
        };
        for write in ["threshold.write", "hostname.write", "sysrq.write"] {
            assert_eq!(fact(write), refused, "{case}: {write}");
        }
        assert_eq!(fact("threshold"), "1024", "{case}");
        // The root, moved over `/`, holds the staged executable, and a nested `chroot` ends in
        // it, when the execution may `chroot` at all.
        let root = fact("root").split(',').collect::<BTreeSet<_>>();
        assert!(root.contains(".runtime-adapter-guest"), "{case}: {root:?}");
        assert!(
            root.contains("etc") && root.contains("usr"),
            "{case}: {root:?}"
        );
        let hostname = if case == "generated" {
            "runtime-adapter"
        } else {
            "nvx-sandbox"
        };
        assert_eq!(fact("hostname"), hostname, "{case}");
        // Before the runtime binds a generated machine ID, every execution sees the root's own.
        if case != "generated" {
            assert_eq!(fact("machine-id"), ROOT_MACHINE_ID, "{case}");
            assert_eq!(fact("mount.machine-id"), "none", "{case}");
            assert_eq!(fact("time.state"), "absent", "{case}");
            assert_eq!(fact("time.files"), "absent", "{case}");
            assert_eq!(fact("mount.time"), "none", "{case}");
        }
    }

    // User ID 0 holds each allowed capability alone in its permitted, effective, and bounding
    // sets, and only it: its own attempt succeeds, and every other one fails.
    for (name, number, allows) in CAPABILITIES {
        let case = format!("only-{name}");
        let fact = |key: &str| fact(&format!("{case}.{key}"));
        assert_eq!((fact("uid"), fact("gid"), fact("groups")), ("0", "0", ""));
        for set in ["cap.prm", "cap.eff", "cap.bnd"] {
            assert_eq!(fact(set), capability_set(1 << number), "{case}: {set}");
        }
        for (attempt, refused) in ATTEMPTS {
            let expected = if allows == Some(attempt) {
                allowed(attempt)
            } else {
                refused
            };
            assert_eq!(fact(attempt), expected, "{case}: {attempt}");
        }
    }
    // Without capabilities, user ID 0 may do none of it, nor may the numeric identity, which has
    // no account in the image.
    for case in ["none", "numeric"] {
        let fact = |key: &str| fact(&format!("{case}.{key}"));
        for set in ["cap.prm", "cap.eff", "cap.bnd"] {
            assert_eq!(fact(set), none, "{case}: {set}");
        }
        for (attempt, refused) in ATTEMPTS {
            assert_eq!(fact(attempt), refused, "{case}: {attempt}");
        }
    }
    let ids = |case: &str| {
        let fact = |key: &str| fact(&format!("{case}.{key}"));
        (fact("uid"), fact("gid"), fact("groups"))
    };
    assert_eq!(ids("none"), ("0", "0", ""));
    assert_eq!(ids("numeric"), ("4242", "4242", ""));

    // In both publication modes, private-network ingress and the host's loopback, which the entry
    // treats alike, a root workload with every other allowed capability can neither bind a
    // privileged source port, from which the gateway accepts listener announcements, nor open a
    // raw socket, which could forge one. Without published listeners, it may.
    let every = CAPABILITIES
        .iter()
        .fold(0u64, |mask, (_, number, _)| mask | 1 << number);
    let refused_while_published = 1 << 10 | 1 << 13;
    for (case, mask) in [
        ("published", every & !refused_while_published),
        ("unpublished", every),
    ] {
        let fact = |key: &str| fact(&format!("{case}.{key}"));
        for set in ["cap.prm", "cap.eff", "cap.bnd"] {
            assert_eq!(fact(set), capability_set(mask), "{case}: {set}");
        }
        for (attempt, refused) in ATTEMPTS {
            let expected = if case == "published" && ["bind", "raw"].contains(&attempt) {
                refused
            } else {
                allowed(attempt)
            };
            assert_eq!(fact(attempt), expected, "{case}: {attempt}");
        }
    }

    // An execution enters the working directory that it names in the root, but through no magic
    // link: neither the link of the descriptor that its caller leaked, which it closed, nor that
    // of its own or its first process's working directory or root, nor a symbolic link to one,
    // nor that of any descriptor, may place it outside the root, or run its program.
    assert_eq!(
        (fact("cwd-directory.launch"), fact("cwd-directory.status")),
        ("started", "exit 0")
    );
    assert_eq!(
        fact("cwd-directory.cwd"),
        "/tmp/runtime-adapter-entries/cwd-directory"
    );
    for (case, error) in [
        ("cwd-leaked", "ENOENT"),
        ("cwd-self", "ELOOP"),
        ("cwd-root", "ELOOP"),
        ("cwd-init", "ELOOP"),
        ("cwd-link", "ELOOP"),
    ] {
        let fact = |key: &str| fact(&format!("{case}.{key}"));
        assert_eq!(
            (fact("launch"), fact("status")),
            (&*format!("working directory {error}"), "exit 125"),
            "{case}"
        );
    }
    assert_eq!(
        fact("cwd-descriptors.launch"),
        "working directory ELOOP,working directory ENOENT"
    );
    assert_eq!(fact("cwd-descriptors.status"), "exit 125");

    // A generated machine ID and the time status that the guest's time daemon publishes, which the
    // runtime bound once, read-only, in the helper's mount namespace, reach the execution, and end
    // with the helper: the worker's own namespace keeps the root's machine ID, without mounts
    // below the root.
    let generated = fact("generated.source");
    assert_eq!(generated.len(), 32, "{generated}");
    assert!(
        generated
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{generated}"
    );
    assert_ne!(generated, ROOT_MACHINE_ID);
    assert_eq!(fact("generated.machine-id"), generated);
    assert_eq!(fact("generated.time.state"), "version=1");
    // The time status holds only what any process may read: no lock of the daemon's, which user
    // ID 0 could open and hold to block the daemon, and no private record.
    let files = fact("generated.time.files");
    let names = files.split(',').collect::<BTreeSet<_>>();
    assert!(names.contains("state"), "{files}");
    assert!(
        names.is_subset(&BTreeSet::from([
            "state",
            ".state",
            "daemon.pid",
            "daemon.pid.tmp"
        ])),
        "{files}"
    );
    for mount in ["generated.mount.machine-id", "generated.mount.time"] {
        let found = options(fact(mount));
        for flag in ["ro", "nosuid", "nodev"] {
            assert!(found.contains(flag), "{mount}: {found:?}");
        }
    }
    assert_eq!(fact("after.machine-id"), ROOT_MACHINE_ID);
    assert_eq!(fact("after.mounts"), "0");
}

/// Returns the options and the arguments of an execution of the host program's `exec`: `options`,
/// then `--`, then the program and its arguments, `command`.
fn execution(options: &[&str], command: &[&str]) -> Vec<String> {
    options
        .iter()
        .chain(&["--"])
        .chain(command)
        .map(|argument| (*argument).to_owned())
        .collect()
}

/// Returns `identity` as the value of the option `--identity` of `exec`: `UID:GID`.
fn ids((uid, gid): (u32, u32)) -> String {
    format!("{uid}:{gid}")
}

/// Returns `text` as the host program prints a value: with `\`, newlines, and carriage returns
/// escaped.
fn escaped(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Returns the output of `seq first last`.
fn lines(first: usize, last: usize) -> String {
    (first..=last).map(|number| format!("{number}\n")).collect()
}

/// Returns what the [`IDENTITY_SCRIPT`] prints for the user ID `uid`, the group ID `gid`, the
/// groups `groups`, and the capabilities `capabilities`, a set of each capability's bit.
fn identity_facts(uid: u32, gid: u32, groups: &str, capabilities: u64) -> String {
    let set = capability_set(capabilities);
    format!("{uid}\n{gid}\n{groups}\nCapPrm: {set}\nCapEff: {set}\nCapBnd: {set}\n")
}

/// Returns the fact KEY of execution `number` that `exec` printed in `printed`.
fn fact<'a>(printed: &'a Printed, number: usize, key: &str) -> &'a str {
    let key = format!("exec.{number}.{key}");
    printed
        .get(&key)
        .unwrap_or_else(|| panic!("{key} is missing: {printed:?}"))
}

/// Checks that execution `number` of what `exec` printed in `printed` started, ended on its own
/// with the exit code `code`, lost no byte, and settled, so that its place is reusable, and that
/// it wrote `stdout` and `stderr`.
fn assert_ran(printed: &Printed, number: usize, code: i32, stdout: &str, stderr: &str) {
    let status = format!("exit {code}");
    let facts =
        ["launch", "status", "cause", "truncated", "settled"].map(|key| fact(printed, number, key));
    assert_eq!(
        facts,
        ["started", &*status, "none", "none", "reusable"],
        "{number}: {printed:?}"
    );
    assert_eq!(fact(printed, number, "stdout"), escaped(stdout), "{number}");
    assert_eq!(fact(printed, number, "stderr"), escaped(stderr), "{number}");
}

/// Checks that the guest refused execution `number` of what `exec` printed in `printed` at stage
/// 0 with the error number `error`, before its program started: the execution is fenced, and has
/// no output.
fn assert_refused(printed: &Printed, number: usize, error: i32) {
    let launch = format!("failed 0 {error}");
    let facts =
        ["launch", "status", "settled", "stdout", "stderr"].map(|key| fact(printed, number, key));
    assert_eq!(
        facts,
        [&*launch, "none", "fenced", "", ""],
        "{number}: {printed:?}"
    );
}

/// Checks the VM of the `executions_run_in_the_root` scenario, whose starts name their identities.
fn check_executions(vm: &mut Vm<'_>) {
    let nobody = ids(NOBODY);

    // Several executions at once, which meet only while all of them run: each leaves its mark in
    // the root's `/tmp`, and waits for the others' marks. Beside them, one writes more than a MiB
    // to its standard output, and a little to its standard error, which arrive whole.
    let meet = format!(
        "mkdir -p /tmp/meeting && touch /tmp/meeting/$NUMBER && \
         until [ \"$(ls /tmp/meeting | wc -l)\" -ge {MEETING} ]; do sleep 0.01; done && \
         echo \"$NUMBER met\""
    );
    let mut executions = (1..=MEETING)
        .map(|number| {
            let environment = format!("NUMBER={number}");
            execution(
                &["--identity", &nobody, "--env", &environment],
                &["/bin/sh", "-c", &meet],
            )
        })
        .collect::<Vec<_>>();
    let writer = "seq 1 200000; seq 1 1000 >&2";
    executions.push(execution(
        &["--identity", "0:0"],
        &["/bin/sh", "-c", writer],
    ));
    let printed = vm.exec(&executions);
    for number in 1..=MEETING {
        assert_ran(&printed, number, 0, &format!("{number} met\n"), "");
    }
    let output = lines(1, 200_000);
    assert!(output.len() > 1 << 20, "{}", output.len());
    assert_ran(&printed, MEETING + 1, 0, &output, &lines(1, 1000));

    // In a later session, whose keys are new to the guest:
    // 1. a descendant that leads a session of its own, and holds its program's output, which the
    //    end of its program, PID 1 of its namespace, ends, so that the execution loses no byte;
    // 2. a user other than root, without capabilities, in existing-account mode;
    // 3. root with `CAP_CHOWN` alone, in its permitted, effective, and bounding sets;
    // 4. exactly the environment that the start names;
    // 5. the working directory that the start names;
    // 6. the entry's default hostname, `nvx-sandbox`, as PID 1 of the execution's namespaces;
    // 7. no identity, in a sandbox without a workload identity, which the guest refuses;
    // 8. a capability outside the entry's allowlist, `CAP_SYS_ADMIN`, which the entry refuses.
    let executions = [
        execution(
            &["--identity", "0:0"],
            &["/bin/sh", "-c", DESCENDANT_SCRIPT],
        ),
        execution(
            &["--identity", &nobody],
            &["/bin/sh", "-c", IDENTITY_SCRIPT],
        ),
        execution(
            &["--identity", "0:0", "--capabilities", "0"],
            &["/bin/sh", "-c", IDENTITY_SCRIPT],
        ),
        execution(
            &[
                "--identity",
                &nobody,
                "--env",
                "FIRST=1",
                "--env",
                "SECOND=two words",
            ],
            &["/usr/bin/env"],
        ),
        execution(
            &["--identity", &nobody, "--cwd", "/usr/share"],
            &["/bin/pwd"],
        ),
        execution(
            &["--identity", &nobody],
            &["/bin/sh", "-c", "cat /proc/sys/kernel/hostname; echo $$"],
        ),
        execution(&[], &["/bin/true"]),
        execution(
            &["--identity", "0:0", "--capabilities", "21"],
            &["/bin/true"],
        ),
    ];
    let printed = vm.exec(&executions);
    assert_ran(&printed, 1, 0, "started\n", "");
    let nobody_facts = identity_facts(NOBODY.0, NOBODY.1, "65534", 0);
    assert_ran(&printed, 2, 0, &nobody_facts, "");
    assert_ran(&printed, 3, 0, &identity_facts(0, 0, "0", 1), "");
    assert_ran(&printed, 4, 0, "FIRST=1\nSECOND=two words\n", "");
    assert_ran(&printed, 5, 0, "/usr/share\n", "");
    assert_ran(&printed, 6, 0, "nvx-sandbox\n1\n", "");
    assert_refused(&printed, 7, EINVAL);
    assert_refused(&printed, 8, EPERM);
}

/// Checks the VM of the `executions_run_as_the_workload_identity` scenario, whose sandbox's
/// workload identity is [`NOBODY`].
fn check_workload_executions(vm: &mut Vm<'_>) {
    assert_eq!(vm.config.workload_identity, Some(NOBODY));
    let executions = [
        execution(&[], &["/bin/sh", "-c", IDENTITY_SCRIPT]),
        execution(&["--env", "ONLY=this"], &["/usr/bin/env"]),
        execution(&["--identity", &ids(NOBODY)], &["/bin/true"]),
    ];
    let printed = vm.exec(&executions);
    let nobody_facts = identity_facts(NOBODY.0, NOBODY.1, "65534", 0);
    assert_ran(&printed, 1, 0, &nobody_facts, "");
    assert_ran(&printed, 2, 0, "ONLY=this\n", "");
    assert_refused(&printed, 3, EINVAL);
}

/// Checks the VM of the `executions_take_the_settings_of_a_proxy_only_network` scenario, whose
/// network's proxy the guest reaches at [`Host::proxy_url`].
fn check_proxy_settings(vm: &mut Vm<'_>) {
    assert_eq!(vm.config.listeners, Some(Listeners::Unpublished));
    let proxy_url = vm
        .host
        .expect("the network has the host's side")
        .proxy_url();
    let nobody = ids(NOBODY);
    let own = "HTTPS_PROXY=http://proxy.example:8080";
    let executions = [
        execution(
            &["--identity", &nobody, "--env", "FIRST=1"],
            &["/usr/bin/env"],
        ),
        execution(&["--identity", &nobody, "--env", own], &["/usr/bin/env"]),
        execution(
            &["--identity", "0:0", "--capabilities", "10,13"],
            &["/bin/sh", "-c", IDENTITY_SCRIPT],
        ),
    ];
    let printed = vm.exec(&executions);
    let proxies = ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"]
        .map(|variable| format!("{variable}={proxy_url}\n"))
        .concat();
    assert_ran(&printed, 1, 0, &format!("FIRST=1\n{proxies}"), "");
    assert_ran(&printed, 2, 0, &format!("{own}\n"), "");
    // `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW`, numbers 10 and 13, in each set.
    let facts = identity_facts(0, 0, "0", 1 << 10 | 1 << 13);
    assert_ran(&printed, 3, 0, &facts, "");
    // Only Linux's command lines carry a start as long as the framing allows.
    if cfg!(target_os = "linux") {
        check_unframed_batch(vm);
    }
}

/// Checks, in the VM of a proxy-only network, that `exec` refuses before it attaches a batch with
/// a start that the framing carries alone, but not with the proxy's variables: none of the batch's
/// executions runs, not even the first, which would leave a marker, and the VM still runs the next
/// command's.
fn check_unframed_batch(vm: &mut Vm<'_>) {
    let marker = "/tmp/unframed-batch";
    let first = execution(&["--identity", "0:0"], &["/bin/touch", marker]);
    // The second start's environment fills it up to the framing's limit, in entries that Linux
    // takes as arguments, of at most 128 KiB each. An entry takes its length, a `u32`, too.
    let names = ["PAD0=", "PAD1=", "PAD2="];
    let fill = MAX_BODY
        - unframed_start_length(&[])
        - names.map(|name| 4 + name.len()).iter().sum::<usize>();
    let padding = (0..names.len())
        .map(|index| {
            let share = fill / names.len() + usize::from(index < fill % names.len());
            format!("{}{}", names[index], "x".repeat(share))
        })
        .collect::<Vec<_>>();
    assert_eq!(unframed_start_length(&padding), MAX_BODY);
    let mut options = vec!["--identity".to_owned(), "0:0".to_owned()];
    for entry in &padding {
        options.extend(["--env".to_owned(), entry.clone()]);
    }
    options.extend(["--".to_owned(), "/bin/true".to_owned()]);
    let batch = [first, vec![";".to_owned()], options].concat();
    let batch = batch.iter().map(OsStr::new).collect::<Vec<_>>();
    let refused = vm.sandbox.run("exec", &batch, FAILED);
    assert_eq!(refused["reattached"], "live");
    assert!(
        !refused.keys().any(|key| key.starts_with("exec.")),
        "{refused:?}"
    );
    let stderr = vm.sandbox.stderr();
    let problem =
        "execution 2, with the proxy of the VM's network: the start of an execution is too long";
    assert!(stderr.contains(problem), "{stderr}");
    let printed = vm.exec(&[execution(
        &["--identity", "0:0"],
        &["/bin/sh", "-c", &format!("test ! -e {marker}")],
    )]);
    assert_ran(&printed, 1, 0, "", "");
}

/// Returns the length of the encoded start that `exec` makes, before the proxy's variables, of an
/// execution as root of `/bin/true` with the environment `environment`: a key, the limits, and the
/// timers each have the same size whatever their values.
fn unframed_start_length(environment: &[String]) -> usize {
    let limits = Limits::new(1, Window::frames(1), Window::frames(1)).unwrap();
    let timers = Timers::new(Duration::from_secs(1), Duration::from_secs(1));
    let mut start = Start::new(Key::new(0, 0), limits, timers, ["/bin/true"])
        .with_environment(environment.iter().map(String::as_str));
    start.identity = Some(runtime_adapter::exec::Identity::new(0, 0));
    start.encode().len()
}

/// Checks the VM of the `published_listeners_refuse_binding_and_raw_sockets` scenario, whose
/// network publishes the guest's listeners.
fn check_published_listeners(vm: &mut Vm<'_>) {
    assert_eq!(vm.config.listeners, Some(Listeners::Published));
    let executions = [
        execution(
            &["--identity", "0:0", "--capabilities", "10"],
            &["/bin/true"],
        ),
        execution(
            &["--identity", "0:0", "--capabilities", "13"],
            &["/bin/true"],
        ),
        execution(
            &["--identity", "0:0", "--capabilities", "0"],
            &["/bin/sh", "-c", IDENTITY_SCRIPT],
        ),
        execution(
            &["--identity", &ids(NOBODY), "--env", "ONLY=this"],
            &["/usr/bin/env"],
        ),
    ];
    let printed = vm.exec(&executions);
    assert_refused(&printed, 1, EPERM);
    assert_refused(&printed, 2, EPERM);
    assert_ran(&printed, 3, 0, &identity_facts(0, 0, "0", 1), "");
    assert_ran(&printed, 4, 0, "ONLY=this\n", "");
}

/// Checks the network device of a VM with `network`, and the publication of its listeners, before
/// the scenario's own checks: OpenVMM attaches no device for a network that lets nothing through,
/// and otherwise one, which NVX's `/init` configured, and asks `/init` on the kernel command line
/// to publish the guest's listeners exactly where the root configuration says that they are
/// published.
fn check_device(vm: &mut Vm<'_>, network: Network) {
    let printed = vm.exec(&[execution(
        &["--identity", &ids(NOBODY)],
        &["/bin/sh", "-c", DEVICE_SCRIPT],
    )]);
    let devices = usize::from(network != Network::Deny);
    let ingress = network
        .ingress()
        .map_or_else(String::new, |mode| format!("ingress {mode}\n"));
    assert_ran(&printed, 1, 0, &format!("devices {devices}\n{ingress}"), "");
    assert_eq!(
        vm.config.listeners == Some(Listeners::Published),
        network.ingress().is_some(),
        "{network:?}"
    );
}

/// Checks what an execution reaches through the VM's network, as NVX's proxy-only scenario probes
/// its network, and as [`Network::reaches`] expects: the execution requests [`PROXY_TARGET`]
/// through the test proxy, which the host serves meanwhile, then tries each probe's destination;
/// then the host checks what arrived at its sockets.
fn check_probes(vm: &mut Vm<'_>) {
    let network = vm.root.network.expect("the scenario has a network");
    let host = vm.host.expect("the network has the host's side");
    let (proxy_reached, reached) = network.reaches();
    let mut options = vec!["--identity".to_owned(), ids(NOBODY)];
    // An execution finds the URL of a proxy-only network's proxy in its environment; in the other
    // networks, it names the test proxy itself, at the same port of its gateway.
    if network != Network::Proxy {
        options.extend([
            "--env".to_owned(),
            format!("http_proxy={}", host.proxy_url()),
        ]);
    }
    let mut command = ["/bin/sh", "-c", PROBE_SCRIPT, "probe", PROXY_TARGET]
        .map(str::to_owned)
        .to_vec();
    for probe in &host.probes {
        command.extend([
            probe.protocol().to_owned(),
            probe.destination.name().to_owned(),
            probe.address.to_string(),
            probe.port.to_string(),
        ]);
    }
    let options = options.iter().map(String::as_str).collect::<Vec<_>>();
    let command = command.iter().map(String::as_str).collect::<Vec<_>>();
    let stop = AtomicBool::new(false);
    let (printed, requests) = thread::scope(|scope| {
        let proxy = scope.spawn(|| serve_proxy(&host.proxy, &stop));
        let printed = {
            let _stop = Stop(&stop);
            vm.exec(&[execution(&options, &command)])
        };
        (printed, proxy.join().unwrap())
    });
    let facts =
        ["launch", "status", "cause", "truncated", "stderr"].map(|key| fact(&printed, 1, key));
    assert_eq!(
        facts,
        ["started", "exit 0", "none", "none", ""],
        "{printed:?}"
    );
    let stdout = unescaped(fact(&printed, 1, "stdout"));
    let mut lines = stdout.lines();
    let proxy = lines.next().unwrap_or_default();
    if proxy_reached {
        assert_eq!(proxy, format!("proxy 0 {PROXY_RESPONSE}"), "{network:?}");
    } else {
        // wget exits with 1 when it cannot reach the proxy, and with another status when it cannot
        // run.
        assert!(proxy.starts_with("proxy 1 "), "{network:?}: {proxy:?}");
    }
    let reported = lines.collect::<Vec<_>>();
    assert_eq!(reported.len(), host.probes.len(), "{network:?}: {stdout:?}");
    for (probe, line) in host.probes.iter().zip(reported) {
        let name = probe.destination.name();
        // nc exits with 1 when it cannot connect, and with another status when it cannot run. A
        // UDP sender cannot see a lost datagram, and may see an unrelated error, so only the host
        // tells whether a datagram arrived.
        let statuses: &[&str] = match (&probe.socket, reached.contains(&probe.destination)) {
            (HostSocket::Udp(_), _) => &["0", "1"],
            (HostSocket::Tcp(_), true) => &["0"],
            (HostSocket::Tcp(_), false) => &["1"],
        };
        let status = line
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix(' '));
        assert!(
            status.is_some_and(|status| statuses.contains(&status)),
            "{network:?}: the {name} probe reported {line:?}"
        );
    }
    // The test proxy received the guest's one request, for its target, where the network lets the
    // guest reach it, and none otherwise.
    assert_eq!(
        requests.len(),
        usize::from(proxy_reached),
        "{network:?}: {requests:?}"
    );
    let request_line = format!("GET {PROXY_TARGET} HTTP/1.");
    assert!(
        requests
            .iter()
            .all(|request| request.starts_with(&request_line)),
        "{requests:?}"
    );
    // Each probe's traffic arrived at its host socket where the network lets it through, and none
    // did otherwise.
    for probe in &host.probes {
        let name = probe.destination.name();
        let reaches = reached.contains(&probe.destination);
        let wait = if reaches {
            ARRIVAL
        } else {
            NEGATIVE_OBSERVATION
        };
        match (arrival(&probe.socket, wait), &probe.socket) {
            (None, _) => assert!(
                !reaches,
                "{network:?}: the {name} probe did not arrive at port {}",
                probe.port
            ),
            (Some(datagram), HostSocket::Udp(_)) => {
                assert!(
                    reaches,
                    "{network:?}: the {name} probe arrived: {datagram:?}"
                );
                assert_eq!(datagram, format!("{name}\n").as_bytes());
            }
            (Some(_), HostSocket::Tcp(_)) => assert!(
                reaches,
                "{network:?}: the {name} probe arrived at port {}",
                probe.port
            ),
        }
    }
}

/// Checks what an execution reaches through the VM's network, as [`check_probes`] does, then
/// whether the host reaches a listener of the guest at the guest's port of host loopback: where
/// OpenVMM publishes the guest's listeners there, with host-loopback access in both directions, as
/// [`check_device`] found it, and nowhere else, such as with outbound host-loopback access alone.
fn check_host_loopback(vm: &mut Vm<'_>) {
    check_probes(vm);
    let network = vm.root.network.expect("the scenario has a network");
    let port = vm.host.expect("the network has the host's side").listener;
    let published = network.ingress() == Some("host-loopback");
    check_guest_listener(vm, SocketAddr::from((Ipv4Addr::LOCALHOST, port)), published);
}

/// Checks the VM of the `a_forward_reaches_its_guest_port` scenario: the host reaches a listener
/// of the guest through the forward's port of host loopback.
fn check_forward(vm: &mut Vm<'_>) {
    let port = vm.host.expect("the network has the host's side").forward;
    check_guest_listener(vm, SocketAddr::from((Ipv4Addr::LOCALHOST, port)), true);
}

/// Runs a listener of the guest at the guest's port [`Host::listener`] while the host tries to
/// reach it through `address`, until the listener's execution ends, and checks that the host
/// reached it, and exchanged a line each way with it, if `reached`, or else that the host never
/// did, while the listener waited all of its time.
fn check_guest_listener(vm: &mut Vm<'_>, address: SocketAddr, reached: bool) {
    let host = vm.host.expect("the network has the host's side");
    let port = host.listener.to_string();
    let seconds = if reached {
        LISTENING
    } else {
        UNPUBLISHED_LISTENING
    };
    let seconds = seconds.to_string();
    let listener = execution(
        &["--identity", &ids(NOBODY)],
        &[
            "/bin/sh",
            "-c",
            LISTENER_SCRIPT,
            "listener",
            &port,
            &seconds,
            GREETING,
        ],
    );
    let stop = AtomicBool::new(false);
    let (printed, exchanged) = thread::scope(|scope| {
        let client = scope.spawn(|| reach_guest(address, &stop));
        let printed = {
            let _stop = Stop(&stop);
            vm.exec(&[listener])
        };
        (printed, client.join().unwrap())
    });
    assert_eq!(exchanged, reached, "{address}: {printed:?}");
    let stdout = if reached {
        format!("{ANSWER}\nlistener 0\n")
    } else {
        "listener 124\n".to_owned()
    };
    assert_ran(&printed, 1, 0, &stdout, "");
}

/// Tries to exchange a line each way with a listener of the guest through `address`, until it did,
/// or until `stop`, and returns whether it did: it reads the listener's greeting, answers it,
/// closes its side, and reads to the end. A connection that ends without the greeting, as
/// OpenVMM's do while the guest does not listen yet, is retried. Each attempt to connect runs
/// until it opens or fails: Windows retries a loopback connection that nothing accepts every half
/// second for about two seconds, so an attempt that a timeout gave up on could still open once
/// OpenVMM publishes the listener, which would then take that connection, already closed, for its
/// one client.
fn reach_guest(address: SocketAddr, stop: &AtomicBool) -> bool {
    while !stop.load(Ordering::SeqCst) {
        if let Ok(mut stream) = TcpStream::connect(address) {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut greeting = Vec::new();
            let mut byte = [0];
            while !greeting.ends_with(b"\n") && matches!(stream.read(&mut byte), Ok(1)) {
                greeting.push(byte[0]);
            }
            if greeting == format!("{GREETING}\n").as_bytes() {
                stream.write_all(format!("{ANSWER}\n").as_bytes()).unwrap();
                stream.shutdown(Shutdown::Write).unwrap();
                let mut rest = Vec::new();
                stream.read_to_end(&mut rest).unwrap();
                assert!(rest.is_empty(), "the guest's listener then sent {rest:?}");
                return true;
            }
            assert!(
                greeting.is_empty(),
                "the guest's listener greeted the host with {greeting:?}"
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Serves the test proxy's `listener` until `stop`: answers each request with [`PROXY_RESPONSE`],
/// and returns the request line of each.
fn serve_proxy(listener: &TcpListener, stop: &AtomicBool) -> Vec<String> {
    listener.set_nonblocking(true).unwrap();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{PROXY_RESPONSE}",
        PROXY_RESPONSE.len()
    );
    let mut requests = Vec::new();
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut connection, _)) => {
                connection.set_nonblocking(false).unwrap();
                connection.set_read_timeout(Some(ARRIVAL)).unwrap();
                let head = read_head(&mut connection);
                let line = head
                    .split(|&byte| byte == b'\r' || byte == b'\n')
                    .next()
                    .unwrap_or_default();
                requests.push(String::from_utf8_lossy(line).into_owned());
                connection.write_all(response.as_bytes()).unwrap();
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("the test proxy failed: {error}"),
        }
    }
    requests
}

/// Reads the head of an HTTP request from `connection`, of at most [`REQUEST_LIMIT`] bytes.
fn read_head(connection: &mut TcpStream) -> Vec<u8> {
    let mut head = Vec::new();
    let mut chunk = [0; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        let length = connection.read(&mut chunk).unwrap();
        if length == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..length]);
        assert!(
            head.len() <= REQUEST_LIMIT,
            "a request's head exceeds {REQUEST_LIMIT} bytes"
        );
    }
    head
}

/// Returns what arrived at the host's `socket` within `wait`: a connection, which carries nothing
/// that a probe checks, or a datagram.
fn arrival(socket: &HostSocket, wait: Duration) -> Option<Vec<u8>> {
    match socket {
        HostSocket::Tcp(listener) => accepted(listener, wait).then(Vec::new),
        HostSocket::Udp(socket) => {
            socket.set_nonblocking(false).unwrap();
            socket.set_read_timeout(Some(wait)).unwrap();
            let mut datagram = [0; 4096];
            match socket.recv(&mut datagram) {
                Ok(length) => Some(datagram[..length].to_vec()),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    None
                }
                Err(error) => panic!("a probe's socket failed: {error}"),
            }
        }
    }
}

/// Returns whether a connection arrived at the host's `listener` within `wait`, or at once for no
/// wait, which it accepts and closes.
fn accepted(listener: &TcpListener, wait: Duration) -> bool {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + wait;
    loop {
        match listener.accept() {
            Ok(_) => return true,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("a listener of the host failed: {error}"),
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Sets its flag once it drops, so that a thread that waits for the flag ends even when the check
/// that the thread serves panics.
struct Stop<'a>(&'a AtomicBool);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Returns `text`, as the host program prints a value, with its escapes undone: `\\`, `\n`, and
/// `\r`.
fn unescaped(text: &str) -> String {
    let mut unescaped = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            unescaped.push(character);
            continue;
        }
        match characters.next() {
            Some('\\') => unescaped.push('\\'),
            Some('n') => unescaped.push('\n'),
            Some('r') => unescaped.push('\r'),
            other => panic!("{text:?} holds the escape {other:?}"),
        }
    }
    unescaped
}
