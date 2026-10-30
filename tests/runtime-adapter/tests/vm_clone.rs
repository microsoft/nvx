//! The host program starts clones of templates on a real hypervisor, through public APIs only: VMs
//! that a restore of a generation of a template store starts, in place, which the store leases to
//! a sandbox, and which the guest agent's clone mode activates, as `runtime-adapter-host start`
//! and `run` ask.
//!
//! Each scenario captures a [`Template`] of its own, with 256 MiB of RAM, of a capacity of
//! 1 GiB, in a template store of its own, as `vm_template` does, of the platform's copy of the
//! fixtures' Ubuntu EROFS image, or of a GPT [`Disk`] longer than the image, without a NIC unless
//! the scenario gives it a [`Network`], with the guest at `192.0.2.2/24`, and starts its
//! clones, each in a sandbox store of its own, which stages the clone's images, as a launch's
//! does: that copy, or a GPT disk, and for a clone over a scratch image, a copy of the scratch
//! template, which, as NVX requires of a clone's images, no one else may change either. The
//! scenarios of exports, as the others but those of networks, capture templates without a NIC,
//! whose clones bind no resolver configuration, so the mount tables that they check hold no
//! `/etc/resolv.conf`.
//!
//! - One generation serves clones over a scratch image and in RAM, at once and in turn, at each
//!   memory target from the template's RAM to its capacity, which the guest has online. The store
//!   leases the generation to each, a stop releases it, and the clones change nothing of the
//!   generation's snapshot, and leave nothing in its payload.
//! - One generation of a template of 4 processors, one of them online at boot, serves clones of
//!   each processor count from that one to its capacity: a clone takes the template's processors
//!   online at boot by default, and the restore of each other target brings the rest of its prefix
//!   online, which the guest has online, and which the time ABI's record of the restore names.
//!   NVX's plan refuses a count that a microVM cannot have, and one beyond the capacity, before
//!   any VMM starts. The generation of a template that boots with 2 of its 4 processors online
//!   refuses a clone of 1, and serves those of 2, its default, and 4.
//! - One generation serves clones of either format over either upper, at once: the platform's
//!   EROFS image, and GPT disks of other layouts than the template's placeholder, a GPT disk too,
//!   and than each other's. The guest of a GPT clone rereads its disk's partition table before it
//!   assembles the root, since its kernel created the partitions of the placeholder, so each
//!   clone's root stacks its own disk's layers, from the partitions that its disk's table names.
//!   The placeholder is longer than the platform's image, so every clone's image is shorter than
//!   the capacity that its restore presents: the platform's image, a GPT disk of its length, and
//!   one as short as its layout allows. The guest reads each image's device to the capacity's end,
//!   past the image's. The restore of a clone in RAM declares the template's scratch absent, so no
//!   file backs the clone's scratch device, which the guest still finds, and reads to its end, at
//!   the template's scratch length. A disk whose first partition is larger than the clone's bound
//!   ends the clone before it is ready.
//! - The store and NVX's plan refuse a clone that the template cannot serve before any VMM starts,
//!   each a failure of the request, as NVX attributes it by its evidence, with no VM: an image of
//!   either format longer than the template's placeholder, a scratch image of another length than
//!   the template's scratch, an image of a geometry that OpenVMM refuses, a memory target beyond
//!   its capacity or of a part of a block. The start then releases the lease, as it does once the
//!   adapter refuses images that the sandbox does not stage. A generation whose key is gone fails
//!   the plan too, as the template's failure, and the start quarantines it through its tombstone,
//!   so that the store refuses a later start's selection of it, as ineligible.
//! - A clone whose activation fails ends before it is ready, with the status of the failure's
//!   class, which attributes the failure, and the VMM's exit: a scratch image that holds no ext4
//!   file system, a journal that the clone's record forbids, a workload identity whose account the
//!   image lacks, and a host that sends anything but the clone's Activate first, a ping, a start, or
//!   an execution. The sandbox records the failure until the release of its lease.
//! - A clone refuses a scratch image whose upper is not fresh, as an earlier clone left it, and
//!   ends before it is ready.
//! - Clones at once each keep a root of their own, and every execution of a clone sees its
//!   hostname and its machine ID: one that its guest generated, distinct in each clone, the same
//!   in each of the clone's executions, which a read-only bind holds at `/etc/machine-id`, or the
//!   image's own, which the guest leaves alone, so that what an execution writes there, the
//!   clone's later executions see.
//! - A clone whose generated machine ID the guest cannot bind, since its image holds a regular
//!   file `etc`, ends before it is ready, with the status of the workload's view, while a clone of
//!   the image that keeps the image's own machine ID, which needs no bind, starts.
//! - A clone maps its export beneath its root once it is ready, before its sandbox starts: an
//!   aggregate of host directories and files, read-only and read-write, which NVX attaches to the
//!   template's dormant filesystem slot as a cold launch attaches one, each directory and file a
//!   child of its own, and whose mappings, each from a source that names its child by its index,
//!   keep their access, show what the host holds, but for the paths that the start hides, and
//!   those inside them that it shows again, and take what the guest, and the clone's workload,
//!   write through the writable ones alone. The clone's record names the export, an aggregate, by
//!   the identity of its plan, with the number of its mappings.
//! - Clones of one generation at once see only their own exports: each its own directory, or two
//!   the same one, which their records name by the same identity, and a clone without an export
//!   none, whose restore leaves the slot dormant. A clone mounts its export read-write, even where
//!   every mapping is read-only, mount tables list the bind of a generated machine ID below the
//!   root beside the mappings, and the bind holds even against the guest's root.
//! - A start refuses an export whose mapped objects changed once NVX's plan checked the clone
//!   before any VMM starts, as a failure of the request, which the clone's launch finds as it
//!   checks the export again: a file that a directory replaced, and a directory, inside one that
//!   the guest may write, that another directory replaced.
//! - A clone whose guest fails a mapping of its table ends before its sandbox starts, nothing of
//!   it runs, and the host's directory stays as it was.
//! - A clone runs its executions as its record's workload identity, once it is ready: the repair
//!   acknowledged the restore before the clone took its reset, the host's first message came
//!   after that, and the clone's Ready, its start, and its first execution came in that order.
//! - Each command after the start reattaches to the clone from a new process: the clone answers
//!   the Activate of its record again, in a new session of the same broker, with its Ready, without
//!   activating again, and serves the command.
//! - A host program of another runtime ends the clone through its verified identity, without a
//!   word to its guest, and releases its lease: the template still serves its other clones.
//! - A clone whose VM ends without a word of its guest once a stop found it running fails, as NVX
//!   attributes the loss of its status, before the stop, which stops nothing, releases its lease:
//!   the template still serves clones.
//! - A tombstone quarantines the generation of a running clone: the store refuses the selection of
//!   the generation for a new clone, as ineligible, while the clone keeps running, reattaches,
//!   stops, and releases its lease, and the generation's snapshot stays as the capture left it.
//! - A selection that names no template leases nothing, with the record of the store's refusal: a
//!   template store that does not exist, a generation that the store never published, and one that
//!   `remove` retired.
//! - A run runs one execution in its clone, then tears the clone down, in one command: its
//!   preflight stages the clone's images, it starts the clone as a start does, runs the execution
//!   in the clone's root, with the environment and the working directory that its start names, as
//!   the clone's workload identity, whatever its program's status, and stops the clone, which
//!   releases the lease. An execution that the guest refuses leaves the run to tear the clone down
//!   as well.
//! - A run whose clone does not start runs nothing: once the preflight staged the clone's images,
//!   the store refuses a selection that names no template, or of a generation whose restore window
//!   passed, without a lease, and a clone whose activation fails ends the run with the record of
//!   its failure, once its VM exited, and the lease released.
//! - A start that ended once the store leased the generation, before it recorded the clone,
//!   leaves a lease whose template store no record names, or only the record of an earlier clone,
//!   of another template: a stop keeps it, and only its own template store, which `--template`
//!   names, releases it, after which the sandbox leases a generation again. A sandbox that holds a
//!   lease must not move: a command on one that moved refuses it, rather than take for it the
//!   sandbox at the lease's path, which holds another lease, or none.
//! - The store checks the generation's eligibility at the host's time: on the boot that captured
//!   the template, by the host's monotonic clock, whatever UTC says, and after a reboot by UTC,
//!   which clock fixtures stand for. It refuses the selection of a generation that is no longer
//!   eligible, as expired, or not yet, as ineligible, without a lease, with the refusal's record.
//! - A clone reads no more of its disks on its restore than its root needs, which a read of each
//!   disk to its end shows the guest's statistics count.
//! - A clone's time settles after it is ready, with no violation, after the downtime that the
//!   host's monotonic clock measured.
//! - A template without a NIC serves clones without a network, and of a network that lets nothing
//!   through, which attaches no NIC either: neither has a network device, nor a network in its
//!   record, and each keeps the image's own `/etc/resolv.conf`, which no bind holds. NVX's plan
//!   refuses a clone of a network with a NIC there before any VMM starts.
//! - One template with a NIC, which its capture attached under the complete deny-all policy,
//!   whatever the policy of the network that its creation named, serves the clones of every
//!   network at its guest's address: each clone's restore installs the clone's own policy on the
//!   template's NIC, with `--restore-network-policy`, and the clone's record names the policy, the
//!   DNS server that OpenVMM serves the guest under it, where it lets the guest reach the
//!   gateway's, the proxy, if any, and whether it publishes the workload's listeners, which a
//!   policy does with ingress, or with host-loopback access both ways. The NIC lets through what
//!   the clone's policy allows alone: of the listeners of host loopback that the tests bind, a
//!   clone reaches at its gateway the allow rule's alone, the proxy's alone, every one with egress
//!   and outbound host-loopback access allowed, and none otherwise. The host reaches a listener of
//!   the clone's guest at its port of host loopback exactly where the policy allows host-loopback
//!   access both ways, once the announcer that the clone's guest started announced it, and through
//!   a forward of the policy, while the guest refuses the executions that ask for
//!   `CAP_NET_BIND_SERVICE` or `CAP_NET_RAW` exactly where the policy publishes the listeners.
//!   NVX's plan refuses, before any VMM starts, a clone of a network at another address of the
//!   guest, of a network that lets nothing through, and one without a network. Each clone's
//!   record names a policy of its own.
//! - A clone of a network whose forward names a port of host loopback that another socket holds
//!   ends before its restore-ready event, since OpenVMM cannot bind the port as it restores the
//!   clone's NIC, which NVX attributes to the host, by the evidence of the forward's bind, and the
//!   start releases its lease. Once the port is free, the template serves a clone of the network.
//! - A clone of a network that lets the guest reach the gateway's DNS server finds a resolver
//!   file that names it, of mode 0444, which a read-only bind holds at `/etc/resolv.conf`, after
//!   the bind of a generated machine ID, as mount tables list them, and which no write changes,
//!   and reaches the server through the template's NIC, although the template's guest learned no
//!   DNS server: a TCP connection to its DNS port opens, which the gateway's DNS proxy accepts
//!   itself. A clone whose resolver configuration cannot be bound, since its image holds a regular
//!   file `etc`, ends before it is ready, with the status of its network settings.
//! - The executions of a clone of a proxy-only network find the URL of its proxy, which its record
//!   names, in their environment, as a launch's executions do, and the clone keeps the image's own
//!   `/etc/resolv.conf`.
//! - A clone of a template with a NIC maps its export once it is ready, below the binds of its
//!   resolver configuration and of a generated machine ID.
//! - A run of a clone of a proxy-only network gives its execution the URL of the network's proxy,
//!   as the executions of a started clone find it, and a run of a network that lets nothing
//!   through, which NVX's plan refuses before any VMM starts, once the preflight staged the
//!   clone's images, runs nothing.
//! - A run's execution finds the run's shares beneath the clone's root, since the restore
//!   sequence mapped them before the clone's sandbox started, and the host's directory keeps what
//!   it wrote through them once the run tore the clone down.
//! - A run whose clone's VM ends without a word of its guest during the execution ends with the
//!   status of the clone's failure, as NVX attributes the loss of its status, after the
//!   execution's diagnostic, once its stop, which stops nothing, released the lease: the template
//!   still serves clones.
//!
//! The tests boot VMs, so they are ignored. They read what the `common` module lists, and take
//! the template's image and the scratch template from its fixtures.
//!
//! ```text
//! cargo test --locked --features host --test vm_clone -- --ignored
//! ```
#![cfg(feature = "host")]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Read, Write};
use std::mem;
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket,
};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use nvx::activation::{
    ActivationRecord, Configuration, ExportKind, MachineIdMode, Publication, ReadyActivation,
    StoreGeneration,
};
use nvx::exec::{Key, Limits, Timers, Window};
use nvx::host::artifacts::Artifacts;
use nvx::host::filesystem::GUEST_EXPORT;
use nvx::host::platform::Transport;
use nvx::host::process::{BootId, Process};
use nvx::host::sandbox::SandboxNetwork;
use nvx::host::store::{CallerFiles, LOG_NAME, SandboxStore};
use nvx::host::template::{
    CAPTURE_RECORD_NAME, CloneControl, ClonePlan, CloneRecords, Evidence, GENERATIONS_NAME,
    Generation, HostTime, KEY_NAME, Lease, MAX_DOWNTIME_NS, MEMORY_BLOCK_SIZE, Outcome, Progress,
    RestoreDeadlines, SNAPSHOT_NAME, Stage, Store, Teardown,
};
use nvx::protocol::root::ImageFormat;
use nvx::protocol::status;
use nvx::protocol::{CAPABILITY_LEN, HostMap};
use nvx::uuid::Uuid;
use runtime_adapter::GUEST_BUILD;
use runtime_adapter::capture::DEFAULT_SCRATCH_LENGTH;
use runtime_adapter::clone::{CLONE_HOSTNAME, HOST_TIME_VARIABLE, Request, host_time_text};
use runtime_adapter::exec::{HostMessage, Start};
use runtime_adapter::frame::Message;
use runtime_adapter::hook::{Action, STEP_HOOK, Step, StepHook};
use runtime_adapter::host::Client;
use runtime_adapter::layered::PROXY_VARIABLES;
use runtime_adapter::root::DEFAULT_RUNTIME;
use runtime_adapter::runtime::Runtime;
use runtime_adapter::sharing::HostFsSharing;
use runtime_adapter::staged::Staged;
use serde_json::{Value, json};

use common::gpt::{self, Contents, Layout, Partition, hexadecimal, small_layer};
use common::{Fixtures, Printed, Sandbox, Started};

/// The exit status of the host program's command that failed, after a diagnostic.
const FAILED: i32 = 1;
/// The exit status of a command whose clone failed, once nothing of the clone runs, and the
/// command released its lease.
const CLONE_FAILED: i32 = 8;
/// The exit status of a start whose template store refused its selection, which leased and
/// launched nothing.
const SELECTION_REFUSED: i32 = 10;
/// The guest agent's build of a host program of another runtime.
const ANOTHER_BUILD: &str = "another-build";
/// Bytes in a mebibyte.
const MIB: u64 = 1024 * 1024;
/// The RAM of the scenarios' templates, in MiB.
const TEMPLATE_MEMORY: u64 = 256;
/// The RAM capacity of the scenarios' templates, in MiB.
const TEMPLATE_CAPACITY: u64 = 1024;
/// The generation of each scenario's template store that its template is.
const GENERATION: StoreGeneration = StoreGeneration::new(NonZeroU64::MIN);
/// The workload identity of the clones that run as one: nobody's, in the Ubuntu image.
const NOBODY: &str = "65534:65534";
/// A workload identity whose account the Ubuntu image lacks.
const STRANGER: &str = "4242:4242";
/// Bytes in a sector of the guest's disks, which their statistics count.
const SECTOR: u64 = 512;
/// How long a clone's time may take to settle once the clone is ready.
const TIME_SETTLEMENT: Duration = Duration::from_secs(30);
/// Nanoseconds in an hour.
const HOUR_NS: u64 = 3_600_000_000_000;
/// The error number with which the guest refuses an execution that names an identity in a
/// sandbox that has its own: `EINVAL`.
const EINVAL: i32 = 22;
/// The machine ID that each execution of [`VIEW`] writes to a clone's `/etc/machine-id`.
const WRITTEN_MACHINE_ID: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
/// The program of the executions that show a clone's view of its root, `/bin/sh -c VIEW sh
/// MARKER ID`, which prints, a line each: the hostname, `hostname NAME`; the machine ID at
/// `/etc/machine-id`, or `absent`, `machine-id ID`; the options of each mount there, `bind
/// OPTIONS`; whether a write of ID there succeeded, `write written`, or not, `write refused`; and
/// what the root's `/marker` holds, or `absent`, `marker TEXT`. Then it writes MARKER to `/marker`.
const VIEW: &str = r#"echo "hostname $(uname -n)"
echo "machine-id $(cat /etc/machine-id 2>/dev/null || echo absent)"
while read -r _ _ _ _ point options _; do
    if [ "$point" = /etc/machine-id ]; then echo "bind $options"; fi
done < /proc/self/mountinfo
if { printf '%s\n' "$2" > /etc/machine-id; } 2>/dev/null; then
    echo "write written"
else
    echo "write refused"
fi
echo "marker $(cat /marker 2>/dev/null || echo absent)"
printf '%s' "$1" > /marker
"#;
/// The directory of a template's sandbox that holds the host directories that the scenario's
/// clones export, the one host directory that their starts authorize.
const EXPORTS: &str = "exports";
/// The directories of the host directory whose directories and files a clone exports as an
/// aggregate, with the [`EXPORT_SHARES`]: `dir`, which holds the empty directory `inner`, and
/// `out`, which holds the directory `private`.
const EXPORT_DIRECTORIES: [&str; 4] = ["dir", "dir/inner", "out", "out/private"];
/// The files of that host directory, each a path in it, with components separated by `/`, and
/// its contents.
const EXPORT_FILES: [(&str, &str); 7] = [
    ("dir/marker", "marker"),
    ("dir/secret", "secret"),
    ("file.txt", "file"),
    ("notes.txt", "notes"),
    ("out/marker", "marker"),
    ("out/private/key", "key"),
    ("out/private/shown", "shown"),
];
/// The shares of a clone that exports an aggregate, each a path in the host directory, and its
/// target in the root, with `:ro` for a read-only share: the directory `dir`, read-only, with the
/// read-write directory `dir/inner` inside it, at the target that it has there, the read-only file
/// `file.txt`, the read-write file `notes.txt`, and the read-write directory `out`. The plan
/// exports the outermost shares, `dir`, `file.txt`, `notes.txt`, and `out`, as the aggregate's
/// children 0 to 3, and the start hides the [`EXPORT_DENIED`] paths, but for the
/// [`EXPORT_ALLOWED`] one.
const EXPORT_SHARES: [(&str, &str); 5] = [
    ("dir", "/srv/maps/dir:ro"),
    ("dir/inner", "/srv/maps/dir/inner"),
    ("file.txt", "/srv/maps/file.txt:ro"),
    ("notes.txt", "/srv/notes.txt"),
    ("out", "/srv/out"),
];
/// The paths of the host directory of the [`EXPORT_SHARES`] that the start hides: the file
/// `dir/secret`, and the directory `out/private`.
const EXPORT_DENIED: [&str; 2] = ["dir/secret", "out/private"];
/// The path inside a hidden one of the [`EXPORT_DENIED`] that the start shows again, which the
/// directory `out/private` lists alone.
const EXPORT_ALLOWED: [&str; 1] = ["out/private/shown"];
/// The `map` probe of the [`EXPORT_SHARES`]: each target, the hidden files, and the file that the
/// start shows again.
const MAP_EXPORT: &str = "map dir=srv/maps/dir inner=srv/maps/dir/inner file=srv/maps/file.txt \
                          notes=srv/notes.txt out=srv/out secret=srv/maps/dir/secret \
                          key=srv/out/private/key shown=srv/out/private/shown";
/// The mappings of the [`EXPORT_SHARES`], in the order in which the guest applied them, which the
/// root's mount table keeps: by the depth of their targets, then by their targets.
const EXPORT_MOUNTS: &str =
    "/srv/notes.txt,/srv/out,/srv/maps/dir,/srv/maps/file.txt,/srv/maps/dir/inner";
/// The program of the executions that show what a clone's workload finds of the
/// [`EXPORT_SHARES`], `/bin/sh -c EXPORTED sh`, which prints, a line each: the contents of each
/// file that a mapping shows, `read PATH TEXT`, or `read PATH refused` for one that cannot be
/// read; whether each hidden file could be read, `hidden PATH read`, or not, `hidden PATH
/// refused`; the names in `/srv/maps/dir`, `/srv/out`, and `/srv/out/private`, each followed by a
/// comma, `listing DIRECTORY NAMES`; for each of `/srv/maps/dir`, `/srv/maps/dir/inner`, and
/// `/srv/out`, whether a write of the new file `workload` in it succeeded, `write DIRECTORY
/// written`, or not, `write DIRECTORY refused`; and for each of `/srv/maps/file.txt` and
/// `/srv/notes.txt`, whether an append of ` workload` to it succeeded, `append FILE written`, or
/// not, `append FILE refused`.
const EXPORTED: &str = r#"for path in /srv/maps/dir/marker /srv/maps/file.txt /srv/notes.txt \
    /srv/out/private/shown; do
    if text=$(cat "$path" 2>/dev/null); then
        echo "read $path $text"
    else
        echo "read $path refused"
    fi
done
for path in /srv/maps/dir/secret /srv/out/private/key; do
    if cat "$path" >/dev/null 2>&1; then
        echo "hidden $path read"
    else
        echo "hidden $path refused"
    fi
done
for directory in /srv/maps/dir /srv/out /srv/out/private; do
    echo "listing $directory $(ls "$directory" | tr '\n' ',')"
done
for directory in /srv/maps/dir /srv/maps/dir/inner /srv/out; do
    if { printf '%s' workload > "$directory/workload"; } 2>/dev/null; then
        echo "write $directory written"
    else
        echo "write $directory refused"
    fi
done
for file in /srv/maps/file.txt /srv/notes.txt; do
    if { printf '%s' ' workload' >> "$file"; } 2>/dev/null; then
        echo "append $file written"
    else
        echo "append $file refused"
    fi
done
"#;
/// The `map` probe of the clones that share one host directory at `/srv/data`: the directory, and
/// its file `marker`.
const MAP_DATA: &str = "map data=srv/data marker=srv/data/marker";
/// The guest's address and prefix in the networks of the scenarios' templates and clones, in a
/// range reserved for documentation, which no host's own address shares, as `vm_template` gives
/// it.
const GUEST_NETWORK: &str = "192.0.2.2/24";
/// Another address of the guest in the same network, at which no template has its NIC.
const OTHER_GUEST_NETWORK: &str = "192.0.2.3/24";
/// The gateway of the scenarios' networks, the first address of the guest's, at which the guest
/// reaches the DNS server that OpenVMM serves, and host loopback.
const GATEWAY: &str = "192.0.2.1";
/// The host's side of the scenarios' networks, which the tests bind the first time that they name
/// one of its ports, and hold until they end.
static HOST_SIDE: LazyLock<HostSide> = LazyLock::new(HostSide::bind);
/// The first and the last of the ports that the host draws for a listener of a clone's guest and
/// for a forward: the dynamic range.
const PORTS: (u16, u16) = (49152, 65535);
/// The number of ports that the host draws for each before it gives up. Windows excludes ports per
/// protocol, in blocks, and gives consecutive ports to consecutive binds of port 0, so the host
/// draws each port independently.
const PORT_ATTEMPTS: usize = 16;
/// How long the host waits at a service of its side for a connection that a clone opened there,
/// where the clone's network lets it through.
const ARRIVAL: Duration = Duration::from_secs(10);
/// How long the host waits at a service of its side for a connection where the clone's network
/// must not let it through.
const NEGATIVE_OBSERVATION: Duration = Duration::from_millis(250);
/// The line with which a listener of a clone's guest greets its client.
const GREETING: &str = "RUNTIME-ADAPTER-GUEST";
/// The line with which the host answers the greeting.
const ANSWER: &str = "RUNTIME-ADAPTER-HOST";
/// How long, in seconds, a listener of a clone's guest waits for the host where the host must
/// reach it.
const LISTENING: u32 = 30;
/// How long, in seconds, a listener of a clone's guest waits for the host where the host must not
/// reach it, all of which the host spends trying to reach it.
const UNPUBLISHED_LISTENING: u32 = 5;
/// The error number with which the guest refuses an execution that asks for a capability that the
/// publication of the workload's listeners forbids: `EPERM`.
const EPERM: i32 = 1;
/// The proxy's settings of an execution that names its own, which it keeps.
const OWN_PROXY: &str = "HTTPS_PROXY=http://proxy.example:8080";
/// The `map` probe of a clone's resolver configuration: what the root's `/etc/resolv.conf` shows.
const MAP_RESOLVER: &str = "map resolver=etc/resolv.conf";
/// The program of the executions that show what a clone has of its network, `/bin/sh -c
/// NETWORK_VIEW sh`, which prints, a line each: the number of the guest's network devices besides
/// loopback, as `/sys/class/net` lists them, `devices N`, and the mode and the type of the root's
/// `/etc/resolv.conf`, as `stat` prints them, `resolver MODE TYPE`.
const NETWORK_VIEW: &str = "echo \"devices $(ls /sys/class/net | grep -cvx lo)\"; \
    echo \"resolver $(stat -c '%a %F' /etc/resolv.conf)\"";
/// The program of the executions that try the DNS server that a clone's resolver configuration
/// names, `/bin/sh -c DNS_REACH sh`, which prints, a line each: the server, `server ADDRESS`, and
/// `dns 0` once a TCP connection to the server's port 53, which it tries with `nc` for at most 2
/// seconds, opened, or `dns 1` once it failed. The gateway's DNS proxy accepts that connection
/// itself, before any query of the guest reaches the host's resolver, so whether it opens depends
/// on neither that resolver nor its upstream servers.
const DNS_REACH: &str = "server=$(sed -n 's/^nameserver //p' /etc/resolv.conf); \
    echo \"server $server\"; nc -z -w 2 \"$server\" 53 </dev/null >/dev/null 2>&1; \
    echo \"dns $?\"";
/// The program of the executions that try the services of the host's side of a clone's network,
/// `/bin/sh -c "set -- GATEWAY TARGET...; REACH"`, each target `NAME:PORT`, which tries a TCP
/// connection to the gateway at each PORT with `nc`, for at most 2 seconds, and prints, a line
/// each, `NAME 0` once it opened, or `NAME 1` once it failed.
const REACH: &str = "gateway=$1; shift; for target in \"$@\"; do \
    nc -z -w 2 \"$gateway\" \"${target#*:}\" </dev/null >/dev/null 2>&1; \
    echo \"${target%%:*} $?\"; done";
/// The program of the executions that listen at the guest's TCP port `$1` for one client, for at
/// most `$2` seconds, `/bin/sh -c LISTENER sh PORT SECONDS LINE`, which greets the client with the
/// line `$3`, prints what it sends until it closes its side, then prints `listener STATUS`: 0 once
/// the client closed, and 124 once the time ran out without one.
const LISTENER: &str =
    "printf '%s\\n' \"$3\" | timeout \"$2\" nc -l -p \"$1\"; echo \"listener $?\"";
/// The capabilities, by their numbers, of the executions that check what the publication of a
/// clone's listeners refuses, each with whether a publication refuses it: `CAP_NET_BIND_SERVICE`
/// and `CAP_NET_RAW`, with which an execution could take the announcer's privileged source port or
/// forge its datagrams, and `CAP_CHOWN`, which none refuses.
const PUBLISHED_CAPABILITIES: [(&str, bool); 3] = [("10", true), ("13", true), ("0", false)];
/// How much longer than the platform's image the GPT placeholder that [`Template::of_disk`]
/// captures is, so that the image of each of the template's clones, the platform's image or a GPT
/// disk of its length or shorter, is shorter than the capacity that the clone's restore presents.
const PLACEHOLDER_SLACK: u64 = 16 * MIB;
/// The placeholder of the template of the scenarios of GPT disks: a disk of two layers, whose
/// partitions start at multiples of 1 MiB.
const PLACEHOLDER: Disk = Disk {
    guid: "5c0f2a8e-3b71-4d96-8e04-7a1c9b2d6e30",
    partitions: &[
        "5c0f2a8e-3b71-4d96-8e04-7a1c9b2d6e31",
        "5c0f2a8e-3b71-4d96-8e04-7a1c9b2d6e32",
        "5c0f2a8e-3b71-4d96-8e04-7a1c9b2d6e33",
    ],
    alignment: 2048,
    first_size: 4096,
    first: b"\xffthe placeholder's first partition",
    top: &[],
};
/// A clone's GPT disk of one layer, whose partitions start at multiples of 2 MiB, with a larger
/// first partition than the placeholder's: its layer's partition starts where none of the
/// placeholder's does.
const ONE_LAYER: Disk = Disk {
    guid: "a17c3e90-6d24-4f5b-b8e1-02c9d4f6a8b0",
    partitions: &[
        "a17c3e90-6d24-4f5b-b8e1-02c9d4f6a8b1",
        "a17c3e90-6d24-4f5b-b8e1-02c9d4f6a8b2",
    ],
    alignment: 4096,
    first_size: 8192,
    first: b"\xfe\0one layer\x80",
    top: &[],
};
/// A clone's GPT disk of three layers, whose partitions start at multiples of 640 KiB, with the
/// largest first partition: its layers' partitions start where none of the placeholder's do, nor
/// [`ONE_LAYER`]'s.
const THREE_LAYERS: Disk = Disk {
    guid: "b2e4f6a8-0c1d-4e3f-9a5b-6c7d8e9f0a10",
    partitions: &[
        "b2e4f6a8-0c1d-4e3f-9a5b-6c7d8e9f0a11",
        "b2e4f6a8-0c1d-4e3f-9a5b-6c7d8e9f0a12",
        "b2e4f6a8-0c1d-4e3f-9a5b-6c7d8e9f0a13",
        "b2e4f6a8-0c1d-4e3f-9a5b-6c7d8e9f0a14",
    ],
    alignment: 1280,
    first_size: 12288,
    first: b"\xfd\tthree layers\xc3",
    top: &[],
};
/// A clone's GPT disk of one layer whose top directory holds a regular file `etc`, below which
/// nothing can be bound at `/etc/machine-id`.
const ETC_FILE: Disk = Disk {
    guid: "c3d5e7f9-1a2b-4c3d-8e4f-5a6b7c8d9e00",
    partitions: &[
        "c3d5e7f9-1a2b-4c3d-8e4f-5a6b7c8d9e01",
        "c3d5e7f9-1a2b-4c3d-8e4f-5a6b7c8d9e02",
    ],
    alignment: 2048,
    first_size: 4096,
    first: b"\xfcetc is a file",
    top: &[("etc", b"not a directory\n")],
};

/// A GPT disk of the scenarios, which [`Template::of_disk`] captures as the template's
/// placeholder, longer than the platform's image, or [`Template::disk`] writes for a clone, of the
/// image's length or shorter. Its partitions follow each other, each at a multiple of its
/// alignment: its first partition, then a layer, a [`small_layer`], in each of the others, empty
/// but the top one.
struct Disk {
    /// The disk's GUID, which the guest expects of it.
    guid: &'static str,
    /// The unique GUIDs of its partitions: the first partition's, then each layer's, from the base
    /// up.
    partitions: &'static [&'static str],
    /// The number of sectors of which the first sector of each partition is a multiple.
    alignment: u64,
    /// The size of its first partition, in bytes.
    first_size: u64,
    /// The bytes that its first partition holds, before NUL bytes: those that the guest returns,
    /// which end in neither a NUL byte nor whitespace.
    first: &'static [u8],
    /// The regular files in the top directory of its top layer, each a name and its bytes.
    top: &'static [(&'static str, &'static [u8])],
}

impl Disk {
    /// Writes the disk at `path`, of `length` bytes, or, for `None`, as short as its layout
    /// allows, its partitions and its backup array and header alone, a file that no one else may
    /// change.
    fn write(&self, path: &Path, length: Option<u64>) {
        let (layer, top) = (small_layer(&[], &[]), small_layer(self.top, &[]));
        let partitions = self
            .partitions
            .iter()
            .enumerate()
            .map(|(index, guid)| {
                let contents = match index {
                    0 => self.first,
                    _ if index + 1 == self.partitions.len() => top.as_slice(),
                    _ => layer.as_slice(),
                };
                let size = match index {
                    0 => self.first_size,
                    _ => u64::try_from(contents.len()).unwrap(),
                };
                Partition {
                    guid,
                    size,
                    contents: Contents::Bytes(contents),
                }
            })
            .collect::<Vec<_>>();
        let layout = Layout {
            guid: self.guid,
            partitions: &partitions,
            alignment: self.alignment,
            length,
        };
        assert_eq!(gpt::write_disk(path, &layout), self.first);
    }
}

/// The network of a scenario's template or clone, which the host program reads from a file that
/// the test writes, in the serde form of a plan's network: the guest at an address of its own, with
/// egress denied, and no host-loopback access, unless the network says otherwise. A network names
/// the ports of the [`HOST_SIDE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Network {
    /// Everything denied, without a proxy, which attaches no NIC.
    Deny,
    /// Egress allowed, without host-loopback access, which lets the guest reach the gateway's DNS
    /// server.
    Egress,
    /// Egress denied but for one rule, TCP to the gateway at the port of [`Service::Rule`], with
    /// outbound host-loopback access.
    AllowRule,
    /// Egress allowed, with outbound host-loopback access, which lets the guest reach the gateway's
    /// DNS server too.
    Outbound,
    /// A proxy-only network, whose proxy is [`Service::Proxy`], on host loopback.
    Proxy,
    /// Ingress allowed, so that OpenVMM publishes the guest's listeners to the host's private
    /// networks, which do not include the host itself without host-loopback access.
    Ingress,
    /// Host-loopback access in both directions, with egress denied: OpenVMM publishes the guest's
    /// listeners on host loopback, at their ports, and the guest reaches nothing, since its own
    /// connections follow the egress policy.
    HostLoopback,
    /// Host-loopback access in both directions, with egress denied, and a forward from the port
    /// [`HostSide::forward`] of host loopback to the guest's port [`HostSide::listener`].
    Forward,
}

impl Network {
    /// The networks with a NIC.
    const WITH_NIC: [Self; 7] = [
        Self::Egress,
        Self::AllowRule,
        Self::Outbound,
        Self::Proxy,
        Self::Ingress,
        Self::HostLoopback,
        Self::Forward,
    ];

    /// Returns the network's name, which names its files.
    fn name(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Egress => "egress",
            Self::AllowRule => "rule",
            Self::Outbound => "outbound",
            Self::Proxy => "proxy",
            Self::Ingress => "ingress",
            Self::HostLoopback => "loopback",
            Self::Forward => "forward",
        }
    }

    /// Returns the network's serde form, with the guest at `guest`.
    fn form(self, guest: &str) -> Value {
        let mut form = json!({
            "guestNetwork": guest,
            "egress": "deny",
            "allow": {"name": "egress.allow", "rules": []},
            "deny": {"name": "egress.deny", "rules": []},
            "ingress": "deny",
            "hostLoopback": "deny",
            "hostLoopbackForwards": [],
            "proxy": null,
        });
        match self {
            Self::Deny => {}
            Self::Egress => form["egress"] = json!("allow"),
            Self::AllowRule => {
                let port = HOST_SIDE.port(Service::Rule);
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
            Self::Proxy => {
                let port = HOST_SIDE.port(Service::Proxy);
                form["proxy"] = json!({"port": port, "scheme": "http"});
            }
            Self::Ingress => form["ingress"] = json!("allow"),
            Self::HostLoopback => form["hostLoopback"] = json!("allow"),
            Self::Forward => {
                form["hostLoopback"] = json!("allow");
                form["hostLoopbackForwards"] = json!([{
                    "protocol": "tcp",
                    "hostPort": HOST_SIDE.forward,
                    "guestPort": HOST_SIDE.listener,
                }]);
            }
        }
        form
    }

    /// Returns OpenVMM's arguments with which the restore of a clone of the network, with the
    /// guest at `guest`, installs the network's policy on the template's NIC, as NVX lowers the
    /// network: `--restore-network-policy`, in place of `--net` and the guest's address, which
    /// OpenVMM takes from the snapshot, then the policy's options, and none for a network that
    /// lets nothing through.
    fn restored(self, guest: &str) -> Vec<String> {
        let network = serde_json::from_value::<SandboxNetwork>(self.form(guest)).unwrap();
        let mut arguments = network.openvmm_arguments().unwrap();
        if !arguments.is_empty() {
            arguments.splice(..2, ["--restore-network-policy".to_owned()]);
        }
        arguments
    }

    /// Returns the DNS server that the network's clones' records name, which OpenVMM serves the
    /// guest under the network's policy: the gateway's, for a network that lets the guest reach
    /// it.
    fn dns(self) -> Option<IpAddr> {
        matches!(self, Self::Egress | Self::Outbound).then(|| GATEWAY.parse().unwrap())
    }

    /// Returns the URL of the network's proxy, as the guest sees it, which the network's clones'
    /// records name, if it has one.
    fn proxy(self) -> Option<String> {
        (self == Self::Proxy)
            .then(|| format!("http://{GATEWAY}:{}", HOST_SIDE.port(Service::Proxy)))
    }

    /// Returns whether the network publishes the guest's listeners, as its clones' records say:
    /// with ingress, or with host-loopback access both ways.
    fn publication(self) -> Publication {
        match self {
            Self::Ingress | Self::HostLoopback | Self::Forward => Publication::Published,
            Self::Deny | Self::Egress | Self::AllowRule | Self::Outbound | Self::Proxy => {
                Publication::Unpublished
            }
        }
    }

    /// Returns whether OpenVMM publishes the guest's listeners on host loopback, at their ports,
    /// where the host reaches them: with host-loopback access both ways.
    fn on_host_loopback(self) -> bool {
        matches!(self, Self::HostLoopback | Self::Forward)
    }

    /// Returns the services of the host's side that the network lets a clone reach at its
    /// gateway: the allow rule's alone, every one with egress and outbound host-loopback access
    /// allowed, the proxy alone for a proxy-only network, and none otherwise, since the guest's own
    /// connections follow the egress policy, and reach host loopback only with host-loopback
    /// access.
    fn reaches(self) -> &'static [Service] {
        match self {
            Self::AllowRule => &[Service::Rule],
            Self::Outbound => &Service::ALL,
            Self::Proxy => &[Service::Proxy],
            Self::Deny | Self::Egress | Self::Ingress | Self::HostLoopback | Self::Forward => &[],
        }
    }
}

/// A service of the host's side of the scenarios' networks: a listener on host IPv4 loopback,
/// which a clone reaches at the same port of its gateway where its network lets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Service {
    /// The proxy of a proxy-only network. No clone sends it a request; a connection that opens
    /// shows that the network lets the clone reach it.
    Proxy,
    /// The service to which the allow rule of a network lets TCP through.
    Rule,
    /// A service that no network names.
    Other,
}

impl Service {
    /// Every service.
    const ALL: [Self; 3] = [Self::Proxy, Self::Rule, Self::Other];

    /// Returns the service's name, which [`REACH`] prints.
    fn name(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::Rule => "rule",
            Self::Other => "other",
        }
    }
}

/// The host's side of the scenarios' networks: the listener of each [`Service`], on host IPv4
/// loopback, at a port that the host chose, which the networks that the tests write name, and the
/// ports of a listener of a clone's guest and of a forward, which no socket of the host held when
/// the host drew them. The listeners hold their ports until the tests end, so that no other socket
/// takes one.
struct HostSide {
    /// The listener of [`Service::Proxy`].
    proxy: TcpListener,
    /// The listener of [`Service::Rule`].
    rule: TcpListener,
    /// The listener of [`Service::Other`].
    other: TcpListener,
    /// The port at which a listener of a clone's guest listens, and OpenVMM publishes it on host
    /// loopback, under a network that publishes the guest's listeners there.
    listener: u16,
    /// The port of host IPv4 loopback that a [`Network::Forward`] forwards to the guest's port
    /// [`listener`](Self::listener).
    forward: u16,
}

impl HostSide {
    /// Binds the listener of each service, and draws the ports of a listener of a clone's guest
    /// and of a forward.
    fn bind() -> Self {
        let bind = || TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let (proxy, rule, other) = (bind(), bind(), bind());
        let mut taken = [&proxy, &rule, &other]
            .map(|listener| listener.local_addr().unwrap().port())
            .to_vec();
        let listener = free_port(&taken);
        taken.push(listener);
        let forward = free_port(&taken);
        Self {
            proxy,
            rule,
            other,
            listener,
            forward,
        }
    }

    /// Returns the listener of `service`.
    fn listener(&self, service: Service) -> &TcpListener {
        match service {
            Service::Proxy => &self.proxy,
            Service::Rule => &self.rule,
            Service::Other => &self.other,
        }
    }

    /// Returns the port of `service`.
    fn port(&self, service: Service) -> u16 {
        self.listener(service).local_addr().unwrap().port()
    }

    /// Returns whether a connection arrived at the listener of `service` within `wait`, or at once
    /// for no wait, which it accepts and closes.
    fn accepted(&self, service: Service, wait: Duration) -> bool {
        let listener = self.listener(service);
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + wait;
        loop {
            match listener.accept() {
                Ok(_) => return true,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("the listener of {service:?} failed: {error}"),
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Returns a port that the host draws from [`PORTS`], independently each time, at most
/// [`PORT_ATTEMPTS`] times, but none of `taken`, and none that a socket holds: OpenVMM publishes a
/// listener of the guest at the same port of host IPv4 and IPv6 loopback, for TCP and UDP, after
/// it checks, on Windows, every host address of each family there, and binds a forward's port on
/// host IPv4 loopback. Windows lets a socket bind every host address beside one that binds loopback
/// alone, and the reverse, so a port is free only where a TCP and a UDP socket bind both, for each
/// family that the host has.
fn free_port(taken: &[u16]) -> u16 {
    let mut addresses = vec![
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V4(Ipv4Addr::LOCALHOST),
    ];
    if TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_ok() {
        addresses.extend([
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ]);
    }
    let (first, last) = PORTS;
    let random = RandomState::new();
    let mut refused = Vec::new();
    for attempt in 0..PORT_ATTEMPTS {
        let offset = random.hash_one(attempt) % u64::from(last - first + 1);
        let port = first + u16::try_from(offset).unwrap();
        if taken.contains(&port) {
            refused.push(format!("{port}: the host's side holds it"));
            continue;
        }
        let bound = addresses.iter().try_for_each(|&address| {
            TcpListener::bind((address, port))?;
            UdpSocket::bind((address, port)).map(drop)
        });
        match bound {
            Ok(()) => return port,
            Err(error) => refused.push(format!("{port}: {error}")),
        }
    }
    panic!("no port drawn from {first} to {last} was free: {refused:?}")
}

/// Writes the network `network`, with the guest at `guest`, in a file of its own in `directory`,
/// and returns the file's path.
fn write_network(directory: &Path, network: Network, guest: &str) -> PathBuf {
    let address = guest.replace(['.', '/'], "-");
    let path = directory.join(format!("network-{}-{address}.json", network.name()));
    fs::write(&path, network.form(guest).to_string()).unwrap();
    path
}

/// The template of a scenario: the generation that a template store of its own captured, in the
/// scenario's sandbox, of the platform's copy of the fixtures' Ubuntu EROFS image, or of a GPT
/// [`Disk`] longer than it, with [`TEMPLATE_MEMORY`] MiB of RAM, of a capacity of
/// [`TEMPLATE_CAPACITY`] MiB, and a NIC exactly when its [`Network`] lets something through.
struct Template {
    /// The scenario's sandbox, whose store is the template store, until the template drops.
    sandbox: Option<Sandbox>,
    /// The platform's copy of the image, which the clones stage as their image unless they name
    /// another, and the template's placeholder, unless that is a GPT disk.
    image: PathBuf,
    /// The image's identity.
    identity: String,
    /// The placeholder's length, in bytes: the capacity of every clone's image, which a restore
    /// presents over it.
    capacity: u64,
    /// The fixtures.
    fixtures: Fixtures,
}

impl Template {
    /// Creates the sandbox of the scenario `name`, stages the platform's copy of the image, and
    /// captures the template of the image in the sandbox's store.
    ///
    /// On Linux, a clone's control socket, `generations/1/LAUNCH.sock` in the template store,
    /// after the 32 hexadecimal digits of the launch, must fit the path of a Unix socket, so the
    /// scenarios' names are short.
    fn create(name: &str) -> Self {
        Self::capture(name, None, None)
    }

    /// Creates the sandbox of the scenario `name`, stages the platform's copy of the image, and
    /// captures the template of the GPT disk `placeholder`, [`PLACEHOLDER_SLACK`] longer than the
    /// image, which the platform stages beside it, as [`create`](Self::create) does.
    fn of_disk(name: &str, placeholder: &Disk) -> Self {
        Self::capture(name, Some(placeholder), None)
    }

    /// Creates the sandbox of the scenario `name`, stages the platform's copy of the image, and
    /// captures the template of the image with the network `network`, with the guest at
    /// [`GUEST_NETWORK`], as [`create`](Self::create) does.
    fn in_network(name: &str, network: Network) -> Self {
        Self::capture(name, None, Some(network))
    }

    /// Creates the sandbox of the scenario `name`, stages the platform's copy of the image, and
    /// captures the template of the image with a capacity of `processors` processors, of which
    /// `boot` are online at boot, or the host program's default, as [`create`](Self::create)
    /// does.
    fn of_processors(name: &str, processors: u32, boot: Option<u32>) -> Self {
        let processors = processors.to_string();
        let boot = boot.map(|boot| boot.to_string());
        let mut options = vec!["--processors", processors.as_str()];
        options.extend(
            boot.iter()
                .flat_map(|boot| ["--boot-processors", boot.as_str()]),
        );
        Self::captured(name, None, None, &options)
    }

    /// Creates the sandbox of the scenario `name`, stages the platform's copy of the image, and
    /// captures the template of the GPT disk `placeholder`, if any, [`PLACEHOLDER_SLACK`] longer
    /// than the image, and of the image otherwise, with the network `network`, if any, with the
    /// guest at [`GUEST_NETWORK`].
    fn capture(name: &str, placeholder: Option<&Disk>, network: Option<Network>) -> Self {
        Self::captured(name, placeholder, network, &[])
    }

    /// Captures the template as [`capture`](Self::capture) does, with the options `extra` too.
    fn captured(
        name: &str,
        placeholder: Option<&Disk>,
        network: Option<Network>,
        extra: &[&str],
    ) -> Self {
        let mut sandbox = Sandbox::new(name);
        let fixtures = Fixtures::find();
        let image = stage_platform(&sandbox.root.join("platform"), &fixtures);
        let length = fs::metadata(&image).unwrap().len();
        let (distro, identity, capacity) = match placeholder {
            None => (erofs(&image), fixtures.identity.as_str(), length),
            Some(disk) => {
                let path = image.with_file_name("placeholder.gpt");
                let capacity = length + PLACEHOLDER_SLACK;
                disk.write(&path, Some(capacity));
                (prefixed("gpt:", &path), disk.guid, capacity)
            }
        };
        let memory = TEMPLATE_MEMORY.to_string();
        let memory_capacity = TEMPLATE_CAPACITY.to_string();
        let network = network.map(|network| write_network(&sandbox.root, network, GUEST_NETWORK));
        let mut options = vec![
            OsStr::new("--distro"),
            &distro,
            OsStr::new("--distro-identity"),
            OsStr::new(identity),
            OsStr::new("--memory"),
            OsStr::new(&memory),
            OsStr::new("--memory-capacity"),
            OsStr::new(&memory_capacity),
        ];
        if let Some(network) = &network {
            options.extend([OsStr::new("--network"), network.as_os_str()]);
        }
        options.extend(extra.iter().map(OsStr::new));
        let created = sandbox.create(&options, 0);
        assert_eq!(created["created"], "captured", "{created:?}");
        assert_eq!(created["generation"], GENERATION.to_string(), "{created:?}");
        Self {
            identity: fixtures.identity.clone(),
            sandbox: Some(sandbox),
            image,
            capacity,
            fixtures,
        }
    }

    /// Returns the template store's directory.
    fn dir(&self) -> &Path {
        let sandbox = self.sandbox.as_ref();
        &sandbox
            .expect("the sandbox lasts until the template drops")
            .store
    }

    /// Returns the directory of the template's sandbox, which holds the template store, the
    /// platform's directory, and the directory of the host directories that the scenario's clones
    /// export, [`EXPORTS`].
    fn root(&self) -> &Path {
        let sandbox = self.sandbox.as_ref();
        &sandbox
            .expect("the sandbox lasts until the template drops")
            .root
    }

    /// Returns the template store.
    fn store(&self) -> Store {
        Store::open(self.dir()).unwrap()
    }

    /// Returns the template's generation, as the store's index lists it.
    fn generation(&self) -> Generation {
        let index = self.store().index().unwrap();
        index.get(GENERATION).cloned().expect("the store holds it")
    }

    /// Returns the leases of the template's generation.
    fn leases(&self) -> Vec<Lease> {
        self.store().leases(GENERATION).unwrap()
    }

    /// Returns the payload of the template's generation.
    fn payload(&self) -> PathBuf {
        self.dir()
            .join(GENERATIONS_NAME)
            .join(GENERATION.to_string())
    }

    /// Returns a copy of the platform's image of `length` bytes, beside it, without a manifest,
    /// at `name`.
    fn variant(&self, name: &str, length: u64) -> PathBuf {
        let copy = self.image.with_file_name(name);
        fs::copy(&self.image, &copy).unwrap();
        File::options()
            .write(true)
            .open(&copy)
            .unwrap()
            .set_len(length)
            .unwrap();
        copy
    }

    /// Writes the GPT disk `disk`, of the length of the platform's image, which a clone restores
    /// in place of the template's placeholder, beside the image, at `name`, and returns its path.
    fn disk(&self, name: &str, disk: &Disk) -> PathBuf {
        self.disk_of(name, disk, Some(fs::metadata(&self.image).unwrap().len()))
    }

    /// Writes the GPT disk `disk`, of `length` bytes, or, for `None`, as short as its layout
    /// allows, as [`disk`](Self::disk) does, and returns its path.
    fn disk_of(&self, name: &str, disk: &Disk, length: Option<u64>) -> PathBuf {
        let path = self.image.with_file_name(name);
        disk.write(&path, length);
        path
    }

    /// Returns the options of a start whose clone has the network `network`, with the guest at
    /// `guest`, from a file that they name, which the template's directory holds.
    fn network_options(&self, network: Network, guest: &str) -> Vec<String> {
        let path = write_network(self.root(), network, guest);
        vec!["--network".to_owned(), path.display().to_string()]
    }

    /// Stages the template's image in the store of `sandbox`, which it creates, for a clone in
    /// RAM.
    fn stage(&self, sandbox: &mut Sandbox) {
        let distro = erofs(&self.image);
        let options = [
            OsStr::new("--distro"),
            &distro,
            OsStr::new("--distro-identity"),
            OsStr::new(&self.identity),
        ];
        sandbox.run("stage", &options, 0);
    }

    /// Starts a clone in a new sandbox of the scenario `name`, as `start` asks, checks that the
    /// start exits with `status`, and returns the clone.
    fn start(&self, name: &str, start: StartOf<'_>, status: i32) -> Cloned {
        self.clone_in(name, "start", &start, &[], status)
    }

    /// Runs one execution, of `execution`, its options and its program, in a clone in a new
    /// sandbox of the scenario `name`, as `run` asks, with the start's options of `start`, checks
    /// that the run exits with `status`, and returns the clone, as the run left it. The run's
    /// preflight stages the clone's images.
    fn run(&self, name: &str, start: StartOf<'_>, execution: &[&str], status: i32) -> Cloned {
        assert!(!start.unstaged, "a run stages its clone's images");
        assert!(start.at_checked.is_none(), "a run takes no step hook");
        self.clone_in(name, "run", &start, execution, status)
    }

    /// Runs the host program's `command`, `start` or `run`, of a clone in a new sandbox of the
    /// scenario `name`, as [`begin_in`](Self::begin_in) begins it, checks that it exits with
    /// `status`, and returns the clone.
    fn clone_in(
        &self,
        name: &str,
        command: &str,
        start: &StartOf<'_>,
        execution: &[&str],
        status: i32,
    ) -> Cloned {
        let (sandbox, started) = self.begin_in(name, command, start, execution);
        let started = sandbox.finish(started, status);
        Cloned {
            sandbox,
            started,
            ran: command == "run",
        }
    }

    /// Begins the host program's `command`, `start` or `run`, of a clone in a new sandbox of the
    /// scenario `name`, as `start` asks, with the arguments `execution` after the start's options,
    /// and returns the sandbox, with the command, which may still run. Before a start, the
    /// sandbox's store stages the clone's images, unless `start` says otherwise. A start that
    /// `start` asks to act at `checked` pauses there, through the step hook, until the scenario
    /// acted.
    fn begin_in(
        &self,
        name: &str,
        command: &str,
        start: &StartOf<'_>,
        execution: &[&str],
    ) -> (Sandbox, Started) {
        let mut sandbox = Sandbox::new(name);
        let (distro, identity) = match start.image {
            None => (erofs(&self.image), self.identity.as_str()),
            Some(Image::Erofs(path)) => (erofs(path), self.identity.as_str()),
            Some(Image::Gpt(disk, path)) => (prefixed("gpt:", path), disk.guid),
        };
        // NVX restores a clone only over images that no one else may change, so the scratch image
        // lies in a directory of the sandbox's own, as the platform's image lies in the platform's.
        let scratch = scratch_image(&sandbox);
        SandboxStore::create(scratch.parent().unwrap()).unwrap();
        match start.upper {
            Upper::Ram => {}
            Upper::Scratch => {
                private_copy(&self.fixtures.scratch, &scratch);
            }
            Upper::Resized(length) => {
                let file = private_copy(&self.fixtures.scratch, &scratch);
                file.set_len(length).unwrap();
            }
            Upper::Zeros => private_file(&scratch)
                .set_len(DEFAULT_SCRATCH_LENGTH)
                .unwrap(),
            Upper::Used(used) => {
                private_copy(used, &scratch);
            }
        }
        let upper = match start.upper {
            Upper::Ram => OsString::from("ram"),
            _ => prefixed("scratch:", &scratch),
        };
        let identity = OsStr::new(identity);
        let run = command == "run";
        if !run && !start.unstaged {
            let mut staging = vec![
                OsStr::new("--distro"),
                &distro,
                OsStr::new("--distro-identity"),
                identity,
            ];
            if start.upper != Upper::Ram {
                staging.extend([OsStr::new("--upper"), &upper]);
            }
            sandbox.run("stage", &staging, 0);
        }
        let generation = GENERATION.to_string();
        let mut options = vec![
            OsStr::new("--template"),
            start.templates.unwrap_or(self.dir()).as_os_str(),
            OsStr::new("--generation"),
            OsStr::new(&generation),
            OsStr::new("--distro"),
            &distro,
            OsStr::new("--distro-identity"),
            identity,
            OsStr::new("--upper"),
            &upper,
        ];
        options.extend(start.options.iter().map(OsStr::new));
        options.extend(execution.iter().map(OsStr::new));
        let time = start
            .time
            .as_ref()
            .map(|time| OsString::from(host_time_text(time)));
        let pause = sandbox.root.join("checked.pause");
        let hook = start.at_checked.map(|_| {
            let hook = StepHook {
                step: Step::Checked,
                action: Action::Pause(pause.clone()),
            };
            OsString::from(hook.to_string())
        });
        let environment: Vec<(&str, &OsStr)> = time
            .iter()
            .map(|time| (HOST_TIME_VARIABLE, time.as_os_str()))
            .chain(hook.iter().map(|hook| (STEP_HOOK, hook.as_os_str())))
            .collect();
        let mut started = sandbox.start(command, &options, &environment);
        if let Some(act) = start.at_checked {
            assert_eq!(sandbox.paused(&mut started, &pause), "checked");
            act();
            fs::remove_file(&pause).unwrap();
        }
        (sandbox, started)
    }
}

impl Drop for Template {
    /// Removes the template's sandbox, as a sandbox's drop does, unless a clone whose VM might
    /// still run leases the generation: one whose sandbox a test that failed kept, since it could
    /// not confirm that the clone's VM ended. Its VMM restores the generation's snapshot in place,
    /// and reads the platform's image, which the template's sandbox holds too, so the template
    /// stays with it. A clone's sandbox drops before the template, which the scenario creates
    /// first.
    fn drop(&mut self) {
        let Some(sandbox) = self.sandbox.take() else {
            return;
        };
        let kept = Store::open(&sandbox.store)
            .and_then(|store| store.leases(GENERATION))
            .map(|leases| {
                leases
                    .iter()
                    .map(Lease::sandbox)
                    .filter(|clone| clone.exists())
                    .map(|clone| clone.display().to_string())
                    .collect::<Vec<_>>()
            });
        let reason = match kept {
            Ok(kept) if kept.is_empty() => {
                drop(sandbox);
                return;
            }
            Ok(kept) => format!("the clones in {} might still run", kept.join(", ")),
            Err(error) => format!("its leases cannot be read: {}", common::describe(&error)),
        };
        eprintln!("kept {} because {reason}", sandbox.root.display());
        mem::forget(sandbox);
    }
}

/// The upper of a clone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Upper<'a> {
    /// RAM.
    #[default]
    Ram,
    /// A copy of the scratch template, an empty ext4 file system of the template's scratch
    /// length.
    Scratch,
    /// A copy of the scratch template of this many bytes.
    Resized(u64),
    /// Zeros of the template's scratch length, which hold no file system.
    Zeros,
    /// A copy of the scratch image at this path, which an earlier clone used, so that its upper is
    /// not fresh.
    Used(&'a Path),
}

/// A clone's image other than the platform's.
#[derive(Clone, Copy)]
enum Image<'a> {
    /// An EROFS image at this path, a variant of the platform's, with its identity.
    Erofs(&'a Path),
    /// A GPT disk of this layout at this path, which [`Template::disk`] wrote.
    Gpt(&'a Disk, &'a Path),
}

/// What a scenario asks of the start of a clone.
#[derive(Default)]
struct StartOf<'a> {
    /// The template store, or the template's by default.
    templates: Option<&'a Path>,
    /// The image, or the platform's image by default.
    image: Option<Image<'a>>,
    /// The upper.
    upper: Upper<'a>,
    /// The start's other options.
    options: &'a [&'a str],
    /// The host's time at which the store checks the generation's eligibility, as the clock
    /// fixture, or the time that the host program samples by default.
    time: Option<HostTime>,
    /// Whether the sandbox's store stages no image.
    unstaged: bool,
    /// What the scenario does while the step hook pauses the start at `checked`, once NVX's plan
    /// checked the clone, if anything.
    at_checked: Option<&'a dyn Fn()>,
}

/// A clone of a scenario's template, in a sandbox of its own, as its start, or its run, left it.
struct Cloned {
    /// The clone's sandbox.
    sandbox: Sandbox,
    /// What the start, or the run, printed.
    started: Printed,
    /// Whether a run, whose preflight staged the clone's images, rather than a start, started
    /// the clone.
    ran: bool,
}

impl Cloned {
    /// Checks that the start started the clone, as the [module documentation](self) describes,
    /// leasing the generation, and recorded its VM, which runs.
    fn assert_started(&self) {
        self.assert_mapped(0);
    }

    /// Checks that the start started the clone, as [`assert_started`](Self::assert_started) does,
    /// once its guest applied `mapped` mappings of its export, none for a clone without one.
    fn assert_mapped(&self, mapped: usize) {
        let started = &self.started;
        assert_clone_started(started, mapped);
        let record = self.sandbox.record().expect("the start recorded its VM");
        assert_eq!(started["pid"], record.process().pid.to_string());
        assert!(!started.contains_key("released"), "{started:?}");
    }

    /// Checks that the run ran the clone, as the [module documentation](self) describes: its
    /// preflight staged the clone's images, it started the clone as a start does, leasing the
    /// generation, and it stopped the clone as a stop does, which released the lease.
    fn assert_ran(&self) {
        self.assert_ran_mapped(0);
    }

    /// Checks that the run ran the clone, as [`assert_ran`](Self::assert_ran) does, once the
    /// clone's guest applied `mapped` mappings of its export, none for a clone without one.
    fn assert_ran_mapped(&self, mapped: usize) {
        let ran = &self.started;
        assert_eq!(ran["preflight"], "staged", "{ran:?}");
        assert_clone_started(ran, mapped);
        ran["pid"].parse::<u32>().unwrap();
        self.assert_stopped(ran);
    }

    /// Runs `probes` in the clone's root, and returns what they found, each fact as
    /// `PROBE.KEY`.
    fn probe(&mut self, probes: &[&str]) -> Printed {
        let probes = probes.iter().map(OsStr::new).collect::<Vec<_>>();
        let printed = self.sandbox.run("probe", &probes, 0);
        assert_eq!(printed["reattached"], "live");
        assert_eq!(printed["pid"], self.started["pid"]);
        printed
    }

    /// Runs one execution of `arguments`, its options and its program, in the clone's root, and
    /// returns what it did, each fact as `exec.1.KEY`.
    fn exec(&mut self, arguments: &[&str]) -> Printed {
        let arguments = arguments.iter().map(OsStr::new).collect::<Vec<_>>();
        let printed = self.sandbox.run("exec", &arguments, 0);
        assert_eq!(printed["reattached"], "live");
        printed
    }

    /// Stops the clone, and checks that it stopped gracefully, and that the stop released the
    /// clone's lease.
    fn stop(&mut self) {
        let stopped = self.sandbox.run("stop", &[], 0);
        self.assert_stopped(&stopped);
    }

    /// Checks that `stopped`, what a stop, or a run, printed, says that the clone stopped
    /// gracefully, with a success and a complete teardown, and released its lease, which the
    /// sandbox no longer holds, and that the sandbox records no VM.
    fn assert_stopped(&self, stopped: &Printed) {
        let facts = ["stopped", "outcome", "teardown"].map(|key| &stopped[key]);
        assert_eq!(facts, ["graceful", "success 0", "complete"], "{stopped:?}");
        assert_eq!(stopped["released"], self.started["lease"], "{stopped:?}");
        assert!(self.sandbox.record().is_none());
        let store = SandboxStore::open(&self.sandbox.store).unwrap();
        assert!(Lease::of(&store).unwrap().is_none());
    }

    /// Checks that the start refused the clone before any VMM started, and released the lease
    /// that it took, if it took one, with a diagnostic that says `problem`.
    fn assert_refused(&self, problem: &str) {
        let started = &self.started;
        assert!(!started.contains_key("pid"), "{started:?}");
        assert_eq!(started.get("released"), started.get("lease"), "{started:?}");
        let stderr = self.sandbox.stderr();
        assert!(stderr.contains(problem), "{problem:?} in {stderr}");
        self.sandbox.assert_unlaunched();
        let store = SandboxStore::open(&self.sandbox.store).unwrap();
        assert!(Lease::of(&store).unwrap().is_none());
    }

    /// Checks that NVX's plan refused the start's clone before any VMM started, as one that the
    /// template cannot serve, with a diagnostic that says `problem`: a failure of the request, by
    /// the evidence of invalid settings, which quarantines nothing, and that the start released the
    /// lease that the store granted.
    fn assert_unserved(&self, problem: &str) {
        let started = &self.started;
        assert!(started.contains_key("lease"), "{started:?}");
        assert_eq!(
            failure(started),
            [
                "request",
                "invalid-settings",
                "plan",
                "planned",
                "none",
                "no-vm"
            ],
            "{started:?}"
        );
        assert_eq!(started["quarantined"], "no", "{started:?}");
        self.assert_refused(problem);
    }

    /// Checks that the start started the clone's VM, which ended before the clone was ready, with
    /// `status`, the guest's status of its activation's failure, and that the start printed the
    /// failure's record, of the request, by `evidence`, at the activation, once the VM exited,
    /// quarantined nothing, and released the lease of `template`'s generation, which no other
    /// clone holds, with the records of the clone's launch.
    fn assert_unactivated(&self, template: &Template, evidence: &str, status: u8) {
        let printed = &self.started;
        assert!(printed.contains_key("pid"), "{printed:?}");
        assert!(!printed.contains_key("root"), "{printed:?}");
        let status = status.to_string();
        assert_eq!(
            failure(printed),
            [
                "request",
                evidence,
                "activation",
                "activating",
                status.as_str(),
                "exited"
            ],
            "{printed:?}"
        );
        assert_eq!(printed["quarantined"], "no");
        assert_eq!(printed["released"], printed["lease"], "{printed:?}");
        assert!(self.sandbox.record().is_none());
        // The release removed the records of the clone's launch.
        let store = SandboxStore::open(&self.sandbox.store).unwrap();
        assert!(CloneRecords::read(&store).unwrap().is_none());
        assert!(template.leases().is_empty());
    }

    /// Returns the clone's scratch image, for a clone over one.
    fn scratch(&self) -> PathBuf {
        scratch_image(&self.sandbox)
    }

    /// Runs [`VIEW`] in the clone's root, as user ID 0, which writes `marker` to the root's
    /// `/marker` last, and returns what it found.
    fn view(&mut self, marker: &str) -> View {
        let program = ["/bin/sh", "-c", VIEW, "sh", marker, WRITTEN_MACHINE_ID];
        let ran = self.exec(&[&["--identity", "0:0", "--"][..], &program].concat());
        let facts = ["launch", "status", "stderr"].map(|key| &ran[&format!("exec.1.{key}")]);
        assert_eq!(facts, ["started", "exit 0", ""], "{ran:?}");
        // The host program escapes each line feed, and each backslash, which none of the lines
        // holds.
        let lines = ran["exec.1.stdout"]
            .strip_suffix("\\n")
            .filter(|lines| !lines.contains("\\\\"))
            .unwrap_or_else(|| panic!("{ran:?}"))
            .split("\\n")
            .collect::<Vec<_>>();
        let value = |line: &str, key: &str| {
            let value = line
                .strip_prefix(key)
                .and_then(|rest| rest.strip_prefix(' '));
            value
                .unwrap_or_else(|| panic!("{key} in {ran:?}"))
                .to_owned()
        };
        let [hostname, machine_id, binds @ .., write, marker] = lines.as_slice() else {
            panic!("{ran:?}");
        };
        View {
            hostname: value(hostname, "hostname"),
            machine_id: value(machine_id, "machine-id"),
            binds: binds.iter().map(|line| value(line, "bind")).collect(),
            write: value(write, "write"),
            marker: value(marker, "marker"),
        }
    }

    /// Runs `script` with `/bin/sh -c` in the clone's root, as user ID 0, checks that it exited
    /// with status 0, and wrote nothing to its standard error, and returns the lines that it
    /// printed.
    fn script(&mut self, script: &str) -> Vec<String> {
        let program = ["/bin/sh", "-c", script, "sh"];
        let ran = self.exec(&[&["--identity", "0:0", "--"][..], &program].concat());
        let facts = ["launch", "status", "stderr"].map(|key| &ran[&format!("exec.1.{key}")]);
        assert_eq!(facts, ["started", "exit 0", ""], "{ran:?}");
        // The host program escapes each line feed, and each backslash, which none of the lines
        // holds.
        ran["exec.1.stdout"]
            .strip_suffix("\\n")
            .filter(|lines| !lines.contains("\\\\"))
            .unwrap_or_else(|| panic!("{ran:?}"))
            .split("\\n")
            .map(str::to_owned)
            .collect()
    }

    /// Checks that the clone reaches, at its gateway, the services of the host's side `reached`
    /// alone: a TCP connection that an execution opens to each of them, with [`REACH`], arrives at
    /// its listener, while one to each other service fails, and none arrives there.
    fn assert_reaches(&mut self, reached: &[Service]) {
        let host = &*HOST_SIDE;
        // Nothing else connects to the listeners, but a connection that a failed check left.
        for service in Service::ALL {
            while host.accepted(service, Duration::ZERO) {}
        }
        let targets =
            Service::ALL.map(|service| format!("{}:{}", service.name(), host.port(service)));
        let found = self.script(&format!("set -- {GATEWAY} {}; {REACH}", targets.join(" ")));
        let expected = Service::ALL.map(|service| {
            let status = u8::from(!reached.contains(&service));
            format!("{} {status}", service.name())
        });
        assert_eq!(found, expected, "{reached:?}");
        for service in Service::ALL {
            let reach = reached.contains(&service);
            let wait = if reach { ARRIVAL } else { NEGATIVE_OBSERVATION };
            assert_eq!(
                host.accepted(service, wait),
                reach,
                "{service:?} of {reached:?}"
            );
        }
    }

    /// Runs a listener of the clone's guest, as nobody, at the guest's port [`HostSide::listener`],
    /// with [`LISTENER`], while the host tries to reach it through `address`, until the listener's
    /// execution ends, and checks that the host reached it, and exchanged a line each way with it,
    /// if `reached`, or else that the host never did, while the listener waited all of its time.
    fn assert_listener(&mut self, address: SocketAddr, reached: bool) {
        let port = HOST_SIDE.listener.to_string();
        let seconds = if reached {
            LISTENING
        } else {
            UNPUBLISHED_LISTENING
        };
        let seconds = seconds.to_string();
        let execution = [
            "--identity",
            NOBODY,
            "--",
            "/bin/sh",
            "-c",
            LISTENER,
            "listener",
            &port,
            &seconds,
            GREETING,
        ];
        let stop = AtomicBool::new(false);
        let (ran, exchanged) = thread::scope(|scope| {
            let client = scope.spawn(|| reach_guest(address, &stop));
            let ran = {
                let _stop = Stop(&stop);
                self.exec(&execution)
            };
            (ran, client.join().unwrap())
        });
        assert_eq!(exchanged, reached, "{address}: {ran:?}");
        // The host program escapes each line feed.
        let stdout = if reached {
            format!("{ANSWER}\\nlistener 0\\n")
        } else {
            "listener 124\\n".to_owned()
        };
        let facts =
            ["launch", "status", "stdout", "stderr"].map(|key| &ran[&format!("exec.1.{key}")]);
        assert_eq!(
            facts,
            ["started", "exit 0", stdout.as_str(), ""],
            "{address}: {ran:?}"
        );
    }

    /// Checks that the clone's guest refuses an execution, as user ID 0, that asks for a
    /// capability with which it could take the announcer's privileged source port or forge its
    /// datagrams, at stage 0 with `EPERM`, before its program starts, exactly where `published`,
    /// as the [`PUBLISHED_CAPABILITIES`] say, and runs the others.
    fn assert_publication(&mut self, published: bool) {
        let mut executions = Vec::new();
        for (index, (capability, _)) in PUBLISHED_CAPABILITIES.into_iter().enumerate() {
            if index > 0 {
                executions.push(";");
            }
            executions.extend(["--identity", "0:0", "--capabilities", capability]);
            executions.extend(["--", "/bin/true"]);
        }
        let ran = self.exec(&executions);
        let refused = format!("failed 0 {EPERM}");
        for (number, (capability, refuses)) in (1..).zip(PUBLISHED_CAPABILITIES) {
            let facts = ["launch", "status"].map(|key| &ran[&format!("exec.{number}.{key}")]);
            let expected = if published && refuses {
                [refused.as_str(), "none"]
            } else {
                ["started", "exit 0"]
            };
            assert_eq!(facts, expected, "{capability} of {published}: {ran:?}");
        }
    }

    /// Returns the configuration of the clone's activation record, which NVX keeps in the clone's
    /// sandbox, in its canonical encoding, while the clone holds its lease.
    fn configuration(&self) -> Configuration {
        let store = SandboxStore::open(&self.sandbox.store).unwrap();
        let encoded = fs::read(store.clone_activation_path()).unwrap();
        let record = ActivationRecord::decode(&encoded).unwrap();
        record.configuration().clone()
    }

    /// Checks the arguments with which NVX started the clone's VMM, on Linux, where the test, as
    /// root, reads them: of OpenVMM's options of an export, `--mount` and those that start with
    /// `--mount-`, exactly `export`, each an option and its value, in order, the options that
    /// attach the clone's export to the template's dormant filesystem slot, as a cold launch
    /// attaches it, or none, for a clone without an export, whose restore leaves the slot dormant.
    /// Elsewhere, NVX's own tests of a clone's launch check the arguments, through a fake VMM.
    fn assert_vmm_export(&self, export: &[(&str, String)]) {
        #[cfg(target_os = "linux")]
        {
            let arguments = self.vmm_arguments();
            let options = arguments
                .windows(2)
                .filter(|pair| pair[0] == "--mount" || pair[0].starts_with("--mount-"))
                .map(|pair| (pair[0].as_str(), pair[1].clone()))
                .collect::<Vec<_>>();
            assert_eq!(options, export, "{arguments:?}");
        }
        #[cfg(not(target_os = "linux"))]
        let _ = export;
    }

    /// Checks the arguments with which NVX started the clone's VMM, on Linux, as
    /// [`assert_vmm_export`](Self::assert_vmm_export) does: `policy`, `--restore-network-policy`,
    /// then the options of the policy that the clone's restore installs on the template's NIC, in
    /// order, as NVX lowers the clone's network, and no other argument of a network, so no
    /// `--net`, since OpenVMM takes the NIC and the guest's address from the snapshot, nor any
    /// argument of a network for a template without a NIC.
    fn assert_vmm_network(&self, policy: &[String]) {
        #[cfg(target_os = "linux")]
        {
            let arguments = self.vmm_arguments();
            let of_network = |argument: &&String| {
                argument.starts_with("--net")
                    || argument.starts_with("--host-loopback")
                    || argument.as_str() == "--restore-network-policy"
            };
            assert_eq!(
                arguments.iter().filter(of_network).count(),
                policy.iter().filter(of_network).count(),
                "{policy:?} in {arguments:?}"
            );
            assert!(
                policy.is_empty()
                    || arguments
                        .windows(policy.len())
                        .any(|window| window == policy),
                "{policy:?} in {arguments:?}"
            );
        }
        #[cfg(not(target_os = "linux"))]
        let _ = policy;
    }

    /// Checks the arguments with which NVX started the clone's VMM, on Linux, as
    /// [`assert_vmm_export`](Self::assert_vmm_export) does, for a clone over `upper`: the image as
    /// the `distro` sandbox block device, then, over a scratch image, the image as the `scratch`
    /// one, or, in RAM, none, but `--restore-absent-scratch`, which declares the template's
    /// scratch absent, so that OpenVMM attaches it with no file.
    fn assert_vmm_scratch(&self, upper: Upper<'_>) {
        #[cfg(target_os = "linux")]
        {
            let arguments = self.vmm_arguments();
            let roles = arguments
                .windows(2)
                .filter(|pair| pair[0] == "--microvm-sandbox-block")
                .map(|pair| pair[1].split(':').next().unwrap_or_default())
                .collect::<Vec<_>>();
            let absent = arguments
                .iter()
                .any(|argument| argument == "--restore-absent-scratch");
            let expected = match upper {
                Upper::Ram => (vec!["distro"], true),
                _ => (vec!["distro", "scratch"], false),
            };
            assert_eq!((roles, absent), expected, "{arguments:?}");
        }
        #[cfg(not(target_os = "linux"))]
        let _ = upper;
    }

    /// Checks that no file of the clone's sandbox is as long as the template's scratch, so that
    /// none backs the scratch device of a clone in RAM, whose restore declares the template's
    /// scratch absent.
    fn assert_scratch_unbacked(&self) {
        let long = files_below(&self.sandbox.root)
            .into_iter()
            .filter(|(_, length, _)| *length >= DEFAULT_SCRATCH_LENGTH)
            .collect::<Vec<_>>();
        assert!(long.is_empty(), "{long:?}");
    }

    /// Returns the arguments with which NVX started the clone's VMM, which the test, as root,
    /// reads on Linux.
    #[cfg(target_os = "linux")]
    fn vmm_arguments(&self) -> Vec<String> {
        let pid = &self.started["pid"];
        let command_line = fs::read(format!("/proc/{pid}/cmdline")).unwrap();
        command_line
            .split(|byte| *byte == 0)
            .map(|argument| String::from_utf8_lossy(argument).into_owned())
            .collect()
    }

    /// Checks that the template store refused the start's selection before the start leased
    /// anything, as the record that the start printed says: the refusal's `outcome` and
    /// `evidence`, at the selection, without a VM, the code `code` of NVX's error, and no lease,
    /// generation, or quarantine, after the preflight of a run, and as
    /// [`assert_refused`](Self::assert_refused) checks it, with a diagnostic that says `problem`.
    fn assert_selection_refused(&self, outcome: &str, evidence: &str, code: &str, problem: &str) {
        let started = &self.started;
        assert_eq!(
            failure(started),
            [outcome, evidence, "selection", "none", "none", "no-vm"],
            "{started:?}"
        );
        assert_eq!(started["failure.code"], code, "{started:?}");
        let printed = if self.ran {
            assert_eq!(started["preflight"], "staged", "{started:?}");
            8
        } else {
            7
        };
        assert_eq!(started.len(), printed, "{started:?}");
        self.assert_refused(problem);
    }
}

/// What an execution of [`VIEW`] found in a clone's root.
#[derive(Clone, Debug, PartialEq, Eq)]
struct View {
    /// The hostname.
    hostname: String,
    /// The machine ID at `/etc/machine-id`, or `absent`.
    machine_id: String,
    /// The options of each mount at `/etc/machine-id`.
    binds: Vec<String>,
    /// Whether a write of [`WRITTEN_MACHINE_ID`] at `/etc/machine-id` was `written` or `refused`.
    write: String,
    /// What the root's `/marker` held, or `absent`.
    marker: String,
}

/// A host directory that a scenario's clones export, in the [`EXPORTS`] of the template's
/// sandbox, beside the template store, which neither holds the other.
struct Export {
    /// The directory.
    root: PathBuf,
}

impl Export {
    /// Writes the directory `name` in the [`EXPORTS`] of the sandbox of `template`, with `files`,
    /// each a path in it, with components separated by `/`, and its contents, and the empty
    /// `directories`, and lets anyone create entries in each of its directories, itself included,
    /// so that only a mapping's access refuses what a clone's execution writes there.
    ///
    /// A Linux host shows the guest each host object's owner and mode, which the guest's kernel
    /// checks: on an unprivileged host, the directories belong to the test's user, not to user
    /// ID 0, as which an execution runs without the capabilities that override the check, so they
    /// would refuse its writes. A Windows host shows the guest user ID 0 as each object's owner, so
    /// there the directories keep their access.
    fn write(
        template: &Template,
        name: &str,
        files: &[(&str, &str)],
        directories: &[&str],
    ) -> Self {
        let export = Self {
            root: template.root().join(EXPORTS).join(name),
        };
        fs::create_dir_all(&export.root).unwrap();
        for directory in directories {
            fs::create_dir_all(export.path(directory)).unwrap();
        }
        for (file, contents) in files {
            let path = export.path(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut pending = vec![export.root.clone()];
            while let Some(directory) = pending.pop() {
                fs::set_permissions(&directory, fs::Permissions::from_mode(0o777)).unwrap();
                for entry in fs::read_dir(&directory).unwrap() {
                    let entry = entry.unwrap();
                    if entry.file_type().unwrap().is_dir() {
                        pending.push(entry.path());
                    }
                }
            }
        }
        export
    }

    /// Returns the host path of `path`, a path in the directory, with components separated by
    /// `/`, or `.` for the directory itself.
    fn path(&self, path: &str) -> PathBuf {
        path.split('/')
            .filter(|name| *name != ".")
            .fold(self.root.clone(), |host, name| host.join(name))
    }

    /// Returns the host path of `path`, a path in the directory, as an export's plan resolves it.
    fn canonical(&self, path: &str) -> String {
        fs::canonicalize(self.path(path))
            .unwrap()
            .display()
            .to_string()
    }

    /// Returns OpenVMM's options, each with its value, that attach an aggregate of `children`, in
    /// order, each a path in the directory, `.` for the directory itself, and its access, `ro` or
    /// `rw`, with `,file` for a file, as a cold launch attaches it, but for the paths that it
    /// hides, shows again, or lets the guest write, which follow them.
    fn attached(&self, children: &[(&str, &str)]) -> Vec<(&'static str, String)> {
        let mut options = vec![("--mount-aggregate", GUEST_EXPORT.to_owned())];
        for (index, (path, access)) in children.iter().enumerate() {
            let child = format!("{index},{},{access}", self.canonical(path));
            options.push(("--mount-child", child));
        }
        options
    }

    /// Returns the options of a start that shares each of `shares` with the clone's root, a path
    /// in the directory, `.` for the directory itself, and its target, with `:ro` for a read-only
    /// share, hides each of `denied`, paths in the directory, and authorizes the [`EXPORTS`].
    fn options(&self, shares: &[(&str, &str)], denied: &[&str]) -> Vec<String> {
        let mut options = Vec::new();
        for (path, target) in shares {
            let share = format!("{}:{target}", self.path(path).display());
            options.extend(["--share".to_owned(), share]);
        }
        for path in denied {
            options.extend(["--deny".to_owned(), self.path(path).display().to_string()]);
        }
        let exports = self.root.parent().unwrap();
        options.extend(["--share-root".to_owned(), exports.display().to_string()]);
        options
    }

    /// Returns what the directory holds, at any depth: each path in it, with components separated
    /// by `/`, and a file's contents, or `None` for a directory.
    fn tree(&self) -> BTreeMap<String, Option<String>> {
        let mut found = BTreeMap::new();
        let mut pending = vec![(self.root.clone(), String::new())];
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
}

/// Returns the tree of a directory that holds the directories `directories`, and `files`, each a
/// path in it, with components separated by `/`, and its contents, as [`Export::tree`] lists it.
fn tree_of(directories: &[&str], files: &[(&str, &str)]) -> BTreeMap<String, Option<String>> {
    let mut tree = BTreeMap::new();
    for directory in directories {
        tree.insert((*directory).to_owned(), None);
    }
    for (file, contents) in files {
        tree.insert((*file).to_owned(), Some((*contents).to_owned()));
    }
    tree
}

/// Returns the options of a mount, as the `map` probe gives them.
fn mount_options(options: &str) -> BTreeSet<&str> {
    options.split(',').collect()
}

/// Returns the scratch image of a clone over one, in the clone's sandbox, `sandbox`, in a
/// directory of the sandbox's own.
fn scratch_image(sandbox: &Sandbox) -> PathBuf {
    sandbox.root.join("images").join("scratch.ext4")
}

/// Returns what `printed`, what an `exec` or a run printed, says of its first execution: how it
/// started, how it ended, and its standard output and error, as the host program escapes them.
fn execution_facts(printed: &Printed) -> [&str; 4] {
    ["launch", "status", "stdout", "stderr"].map(|key| printed[&format!("exec.1.{key}")].as_str())
}

/// Checks that `started`, what a start, or a run, printed, says that it started a clone of the
/// template's generation, with a lease of its own, at a downtime, whose guest agent, of the
/// host's build, assembled its root, applied `mapped` mappings of its export, none for a clone
/// without one, and started its sandbox.
fn assert_clone_started(started: &Printed, mapped: usize) {
    assert_eq!(started["generation"], GENERATION.to_string(), "{started:?}");
    let lease = &started["lease"];
    assert!(
        lease.len() == 32 && lease.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{started:?}"
    );
    started["downtime"].parse::<u64>().unwrap();
    let mapped = mapped.to_string();
    let facts = ["root", "build", "table", "mapped", "sandbox"].map(|key| &started[key]);
    assert_eq!(
        facts,
        ["ready", GUEST_BUILD, "applied", mapped.as_str(), "started"],
        "{started:?}"
    );
}

/// Stages the platform's copy of the fixtures' Ubuntu EROFS image in the directory `platform`,
/// beside a copy of its manifest, and returns it.
///
/// A platform stages its trusted artifacts where no one else may change them, and NVX refuses an
/// image that anyone else may change. So the directory is private to the current user, as NVX
/// makes a sandbox's store, whose access control list on Windows passes on to the files created in
/// it.
fn stage_platform(platform: &Path, fixtures: &Fixtures) -> PathBuf {
    SandboxStore::create(platform).unwrap();
    let [image, _] = [&fixtures.distro, &fixtures.manifest].map(|source| {
        let copy = platform.join(source.file_name().unwrap());
        private_copy(source, &copy);
        copy
    });
    image
}

/// Creates the file `path`, which no one else may change, in a directory private to the current
/// user, and returns it, open for writing: of the mode 0644 on Unix, whatever the umask, and on
/// Windows of the directory's access control list.
fn private_file(path: &Path) -> File {
    let file = File::create(path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o644))
            .unwrap();
    }
    file
}

/// Copies the file `source` to `copy`, which [`private_file`] creates, and returns the copy,
/// open for writing.
fn private_copy(source: &Path, copy: &Path) -> File {
    let mut file = private_file(copy);
    io::copy(&mut File::open(source).unwrap(), &mut file).unwrap();
    file
}

/// Returns `path` after `prefix`, as an option's value names a file.
fn prefixed(prefix: &str, path: &Path) -> OsString {
    let mut value = OsString::from(prefix);
    value.push(path);
    value
}

/// Returns the value of `--distro` for the EROFS image at `path`.
fn erofs(path: &Path) -> OsString {
    prefixed("erofs:", path)
}

/// Returns the names of the entries of the directory `path`, sorted.
fn names(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Returns each file below the directory `directory`, by its path in it, with its length and its
/// modification time.
fn files_below(directory: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let mut files = Vec::new();
    let mut directories = vec![directory.to_owned()];
    while let Some(below) = directories.pop() {
        for entry in fs::read_dir(&below).unwrap() {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            if metadata.is_dir() {
                directories.push(entry.path());
                continue;
            }
            let path = entry.path().strip_prefix(directory).unwrap().to_owned();
            files.push((path, metadata.len(), metadata.modified().unwrap()));
        }
    }
    files.sort();
    files
}

/// Checks that the payload of the template's generation holds what a capture published alone,
/// and, while clones run, their control sockets on Linux: no copy of a scratch image, say.
fn assert_payload(template: &Template, running: bool) {
    let published = [CAPTURE_RECORD_NAME, KEY_NAME, SNAPSHOT_NAME];
    let names = names(&template.payload());
    let (template_files, others): (Vec<_>, Vec<_>) = names
        .iter()
        .partition(|name| published.contains(&name.as_str()));
    assert_eq!(template_files, published, "{names:?}");
    let sockets = running && cfg!(target_os = "linux");
    assert!(
        others
            .iter()
            .all(|name| sockets && name.len() == 37 && name.ends_with(".sock")),
        "{names:?}"
    );
}

/// Checks that a clone's `memory` probe found the memory target `target`, in MiB, online.
fn assert_memory(facts: &Printed, target: u64) {
    let number = |key: &str| facts[&format!("memory.{key}")].parse::<u64>().unwrap();
    let block = number("block-size");
    assert!(
        block > 0 && MEMORY_BLOCK_SIZE.is_multiple_of(block),
        "{facts:?}"
    );
    assert_eq!(number("online"), target * MIB, "{facts:?}");
    assert_eq!(number("blocks") * block, target * MIB, "{facts:?}");
    assert!(number("total") <= target * MIB, "{facts:?}");
}

/// Checks the root of a clone of the platform's image over `upper`, as its `mounts` and `contents`
/// probes found it, in the overlay mode that trusts the lowers, or not.
fn assert_root(facts: &Printed, upper: Upper<'_>, trusted: bool) {
    assert_eq!(facts["mounts.distro.count"], "1", "{facts:?}");
    assert_eq!(facts["mounts.distro.type"], "erofs", "{facts:?}");
    assert_overlay(facts, upper, trusted, "ubuntu");
}

/// Checks the root of a clone of the GPT disk `disk` over `upper`, as its `devices`, `mounts`,
/// `contents`, and `first-partition` probes found it, in the overlay mode that trusts the lowers:
/// each of the disk's layers, an ext4 file system, mounted once, read-only, from the partition of
/// the clone's one read-only disk that the disk's table names, which the overlay stacks, the base
/// last; and the bytes of the disk's first partition.
fn assert_gpt_root(facts: &Printed, disk: &Disk, upper: Upper<'_>) {
    let read_only = facts["devices.disks"]
        .split(' ')
        .filter_map(|device| device.strip_suffix(":ro"))
        .filter_map(|device| device.split(':').next())
        .collect::<Vec<_>>();
    let &[name] = read_only.as_slice() else {
        panic!("the clone has one read-only disk: {facts:?}");
    };
    let numbers = 2..=disk.partitions.len();
    for number in numbers.clone() {
        let fact = |key: &str| facts[&format!("mounts.p{number}.{key}")].as_str();
        let source = format!("/dev/{name}{number}");
        assert_eq!(
            [fact("count"), fact("type"), fact("source")],
            ["1", "ext4", source.as_str()],
            "{facts:?}"
        );
        let options = fact("options").split(',').collect::<Vec<_>>();
        assert!(options.contains(&"ro"), "{facts:?}");
    }
    let lowers = numbers
        .rev()
        .map(|number| format!("{DEFAULT_RUNTIME}/layers/p{number}"))
        .collect::<Vec<_>>();
    let stacked = format!("lowerdir={}", lowers.join(":"));
    let overlay = facts["mounts.root.super"].split(',').collect::<Vec<_>>();
    assert!(
        overlay.contains(&stacked.as_str()),
        "{stacked} in {facts:?}"
    );
    assert_overlay(facts, upper, true, "absent");
    let (length, bytes) = (disk.first.len().to_string(), hexadecimal(disk.first));
    assert_eq!(facts["first-partition.length"], length, "{facts:?}");
    assert_eq!(facts["first-partition.bytes"], bytes, "{facts:?}");
}

/// Checks the upper, the overlay, and the contents of the root of a clone over `upper`, as its
/// `mounts` and `contents` probes found them, in the overlay mode that trusts the lowers, or not,
/// whose `os-release` names `identifier`, or `absent` for a root without one.
fn assert_overlay(facts: &Printed, upper: Upper<'_>, trusted: bool, identifier: &str) {
    let (used, unused, kind) = match upper {
        Upper::Ram => ("tmpfs", "scratch", "tmpfs"),
        _ => ("scratch", "tmpfs", "ext4"),
    };
    assert_eq!(facts[&format!("mounts.{used}.count")], "1", "{facts:?}");
    assert_eq!(facts[&format!("mounts.{used}.type")], kind, "{facts:?}");
    assert_eq!(facts[&format!("mounts.{unused}.count")], "0", "{facts:?}");
    assert_eq!(facts["mounts.root.type"], "overlay", "{facts:?}");
    let overlay = facts["mounts.root.super"].split(',').collect::<Vec<_>>();
    for option in ["metacopy=on", "redirect_dir=on", "xino=on"] {
        assert_eq!(
            overlay.contains(&option),
            trusted,
            "{option} in {overlay:?}"
        );
    }
    let contents = ["os-release.id", "write.root", "write.upper", "lower.write"]
        .map(|key| &facts[&format!("contents.{key}")]);
    assert_eq!(
        contents,
        [identifier, "runtime-adapter", "runtime-adapter", "refused"],
        "{facts:?}"
    );
}

/// Checks that an execution of [`VIEW`] found a machine ID that the clone's guest generated: 32
/// lowercase hexadecimal digits, which one bind holds at `/etc/machine-id`, read-only, without
/// set-user-ID programs or devices, so that the execution's write there was refused.
fn assert_generated(view: &View) {
    let id = &view.machine_id;
    let digit = |byte: u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte);
    assert!(id.len() == 32 && id.bytes().all(digit), "{view:?}");
    let [options] = view.binds.as_slice() else {
        panic!("one bind holds the machine ID: {view:?}");
    };
    let options = options.split(',').collect::<Vec<_>>();
    for flag in ["ro", "nosuid", "nodev"] {
        assert!(options.contains(&flag), "{flag} in {view:?}");
    }
    assert_eq!(view.write, "refused", "{view:?}");
}

/// Checks the events of a clone's activation that its `activation` probe found: the repair
/// acknowledged the restore, and the clone took its reset, the host's first message, its Ready,
/// and its start, in that order, after the acknowledgement. Returns when each came, in
/// microseconds after the acknowledgement.
fn assert_timeline(facts: &Printed) -> [u64; 4] {
    assert_eq!(facts["activation.acknowledged"], "yes", "{facts:?}");
    let events = ["reset", "message", "ready", "started"].map(|event| {
        let value = &facts[&format!("activation.{event}")];
        value
            .parse::<u64>()
            .unwrap_or_else(|_| panic!("the clone's {event} is {value}: {facts:?}"))
    });
    assert!(events.is_sorted(), "{facts:?}");
    events
}

/// Returns the record of a clone's failure that a command printed, as `failure.KEY` lines: its
/// outcome, its evidence, the stage at which the clone failed, how far the clone got, the guest's
/// status, and how the teardown ended, each `-` if the command did not print it.
fn failure(printed: &Printed) -> [&str; 6] {
    [
        "outcome", "evidence", "stage", "progress", "status", "teardown",
    ]
    .map(|key| {
        printed
            .get(&format!("failure.{key}"))
            .map_or("-", String::as_str)
    })
}

/// Returns the disks of a probe's `disks` fact, `NAME:NUMBER` or `NAME:NUMBER:ACCESS` each, with
/// their numbers.
fn disks(fact: &str) -> BTreeMap<String, u64> {
    fact.split(' ')
        .map(|disk| {
            let mut parts = disk.split(':');
            let name = parts.next().unwrap().to_owned();
            let number = parts.next().and_then(|number| number.parse().ok());
            (
                name,
                number.unwrap_or_else(|| panic!("{disk:?} in {fact:?}")),
            )
        })
        .collect()
}

/// Checks the disks of a clone, as its `devices` and `read-disks` probes found them: its image's,
/// read-only, at `capacity`, the length of the template's placeholder, and its scratch device,
/// writable, at the template's scratch length, each of which the guest read to its end, whatever
/// the length of the image, and whether a file backs the scratch device.
fn assert_capacity(facts: &Printed, capacity: u64) {
    let read = disks(&facts["read-disks.disks"]);
    let mut found = facts["devices.disks"]
        .split(' ')
        .map(|disk| {
            let parts = disk.split(':').collect::<Vec<_>>();
            let &[name, sectors, access] = parts.as_slice() else {
                panic!("{disk:?} in {facts:?}");
            };
            let length = sectors.parse::<u64>().unwrap() * SECTOR;
            (access, length, read.get(name).copied())
        })
        .collect::<Vec<_>>();
    found.sort_unstable();
    assert_eq!(
        found,
        [
            ("ro", capacity, Some(capacity)),
            ("rw", DEFAULT_SCRATCH_LENGTH, Some(DEFAULT_SCRATCH_LENGTH))
        ],
        "{facts:?}"
    );
}

/// Returns a boot of this host other than `boot`.
fn other_boot(boot: BootId) -> BootId {
    let text = boot.to_string();
    let first = if text.starts_with('0') { "1" } else { "0" };
    format!("{first}{}", &text[1..]).parse().unwrap()
}

/// The adapter's framing of a clone's control session that sends `early`, which the host must not
/// send before the clone is ready, in place of the clone's Activate, and, once the clone ended its
/// session, waits for the clone's VMM to exit, so that the restore sequence, which then ends the
/// launch, ends it once the VM ended on its own.
struct Early {
    /// What the host sends first.
    early: Message,
    /// The clone's VMM.
    vmm: Process,
    /// Whether the clone's VMM exited once the clone ended the session.
    exited: bool,
}

impl CloneControl for Early {
    type Session = Client;
    type Error = io::Error;

    fn attach(
        &mut self,
        transport: Box<dyn Transport>,
        capability: &[u8; CAPABILITY_LEN],
        deadline: Instant,
    ) -> io::Result<Client> {
        Client::attach(transport, capability, deadline).map_err(io::Error::other)
    }

    fn activate(
        &mut self,
        session: &mut Client,
        _record: &[u8],
        deadline: Instant,
    ) -> io::Result<Option<ReadyActivation>> {
        session
            .send(&self.early, Some(deadline))
            .map_err(io::Error::other)?;
        let answer = session.receive(Some(deadline));
        self.exited = self.vmm.wait(deadline)?;
        Err(match answer {
            Ok(message) => io::Error::other(format!("the clone answered {message:?}")),
            Err(error) => io::Error::other(error),
        })
    }

    fn map(
        &mut self,
        _session: &mut Client,
        _table: &[HostMap],
        _deadline: Instant,
    ) -> io::Result<()> {
        unreachable!("a clone that is never ready gets no mapping table")
    }

    fn start(&mut self, _session: &mut Client, _deadline: Instant) -> io::Result<()> {
        Err(io::Error::other("the clone is never ready"))
    }

    fn lost(&self, _error: &io::Error) -> bool {
        false
    }
}

/// Starts a clone of `template` in RAM, in a sandbox of its own, through NVX's public API, whose
/// host sends `early` before the clone's Activate, and checks that the clone ended before it was
/// ready, with the status of the host's session, which attributes the failure to no one, as the
/// sandbox records it, and that the VMM exited, then releases the clone's lease, which removes the
/// record.
fn refuses_early(template: &Template, early: Message) {
    let mut sandbox = Sandbox::new("cl-e");
    template.stage(&mut sandbox);
    let store = template.store();
    let artifacts = env::var_os(Artifacts::ENV_DIR).unwrap();
    let runtime = Runtime::from_dir(artifacts, GUEST_BUILD).unwrap();
    let request = Request {
        distro: template.image.clone(),
        format: ImageFormat::Erofs,
        identity: Uuid::parse(&template.identity).unwrap(),
        scratch: None,
        processors: None,
        memory: None,
        workload_identity: None,
        overlay_mode: None,
        scratch_journal: None,
        first_partition_bound: None,
        machine_id: MachineIdMode::Image,
        sharing: HostFsSharing::default(),
        network: None,
    };
    let plan = request.plan().unwrap();
    let staged = Staged::read(&sandbox.store).unwrap().unwrap();
    let key = store.read_key(&template.generation()).unwrap();
    let clone = request
        .clone_request(
            plan.clone(),
            None,
            &staged.bindings(&plan),
            key.processors(),
            key.memory(),
        )
        .unwrap();
    let now = HostTime::now().unwrap();
    let mut acquired = store
        .acquire(GENERATION, &sandbox.store, &CallerFiles::none(), &now, None)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let planned = ClonePlan::new(&store, &mut acquired, &clone, deadline).unwrap();
    let restoring = planned
        .launch(&mut acquired, &runtime.artifacts.openvmm, &runtime.identity)
        .unwrap();
    let vmm = Process::open(restoring.starting().process().reference())
        .unwrap()
        .expect("the clone's VMM runs");
    let mut control = Early {
        early: early.clone(),
        vmm,
        exited: false,
    };
    let failure = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap_err();
    let log = || fs::read_to_string(sandbox.store.join(LOG_NAME)).unwrap_or_default();
    let problem = common::describe(&failure);
    assert!(
        !problem.contains("the clone answered"),
        "{early:?}: {problem}"
    );
    assert!(control.exited, "{early:?}: {problem}\n{}", log());
    let record = failure.record();
    let ended = (
        record.outcome,
        record.evidence,
        record.stage,
        record.progress,
        record.status,
        &record.teardown,
    );
    let expected = (
        Outcome::Unknown,
        Evidence::Protocol,
        Stage::Activation,
        Progress::Activating,
        Some(status::ACTIVATION_PROTOCOL),
        &Teardown::Exited,
    );
    assert_eq!(ended, expected, "{early:?}: {problem}\n{}", log());
    assert_eq!(record.lease, acquired.lease.id());
    // The sandbox records the failure, which keeps the clone from launching again, until the
    // release of its lease.
    let records = CloneRecords::read(&acquired.locked).unwrap().unwrap();
    assert_eq!(records.failure.as_ref(), Some(record));
    assert!(records.journal.is_none() && records.runtime.is_none());
    let released = store.release(&mut acquired.locked).unwrap();
    assert_eq!(released.map(|lease| lease.id()), Some(acquired.lease.id()));
    assert!(CloneRecords::read(&acquired.locked).unwrap().is_none());
    assert!(template.leases().is_empty());
}

/// Returns the limits of the executions of the scenarios that the host starts itself.
fn limits() -> Limits {
    Limits::new(16 * 1024, Window::frames(4), Window::frames(8))
        .unwrap()
        .shared_output()
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn one_generation_serves_clones_of_each_upper_and_memory_target() {
    let template = Template::create("cl-reuse");
    let snapshot = template.payload().join(SNAPSHOT_NAME);
    let captured = files_below(&snapshot);

    // Two clones at once: one over a scratch image, at the template's RAM, which trusts its
    // lowers, as the guest does by default, and one in RAM, at twice the template's RAM, which
    // does not.
    let scratch = StartOf {
        upper: Upper::Scratch,
        ..StartOf::default()
    };
    let mut over_scratch = template.start("cl-a", scratch, 0);
    let options = ["--memory", "512", "--overlay-mode", "untrusted-lowers"];
    let in_ram = StartOf {
        options: &options,
        ..StartOf::default()
    };
    let mut in_ram = template.start("cl-b", in_ram, 0);
    for cloned in [&over_scratch, &in_ram] {
        cloned.assert_started();
        // On the boot that captured the template, the host's monotonic clock measures the
        // downtime.
        assert_eq!(
            cloned.started["source"], "monotonic",
            "{:?}",
            cloned.started
        );
    }
    assert_ne!(over_scratch.started["lease"], in_ram.started["lease"]);

    // The store leases the generation to both, by their sandboxes' paths.
    let mut leases = template
        .leases()
        .iter()
        .map(|lease| (lease.id().to_string(), lease.sandbox().to_owned()))
        .collect::<Vec<_>>();
    leases.sort();
    let mut expected = [&over_scratch, &in_ram].map(|cloned| {
        let root = cloned.sandbox.root.file_name().unwrap();
        (
            cloned.started["lease"].clone(),
            Path::new(root).join("store"),
        )
    });
    expected.sort();
    assert_eq!(leases.len(), expected.len(), "{leases:?}");
    for ((lease, sandbox), (expected, store)) in leases.iter().zip(&expected) {
        assert_eq!(lease, expected);
        assert!(sandbox.ends_with(store), "{}", sandbox.display());
    }
    assert_payload(&template, true);

    let probes = ["memory", "mounts", "contents", "activation"];
    let facts = over_scratch.probe(&probes);
    assert_memory(&facts, TEMPLATE_MEMORY);
    assert_root(&facts, Upper::Scratch, true);
    assert_timeline(&facts);
    let facts = in_ram.probe(&probes);
    assert_memory(&facts, 512);
    assert_root(&facts, Upper::Ram, false);
    assert_timeline(&facts);
    over_scratch.stop();
    assert_eq!(template.leases().len(), 1);
    in_ram.stop();
    assert!(template.leases().is_empty());

    // Then another over a scratch image, at the template's RAM capacity.
    let options = ["--memory", "1024"];
    let at_capacity = StartOf {
        upper: Upper::Scratch,
        options: &options,
        ..StartOf::default()
    };
    let mut at_capacity = template.start("cl-c", at_capacity, 0);
    at_capacity.assert_started();
    let facts = at_capacity.probe(&["memory", "mounts", "contents"]);
    assert_memory(&facts, TEMPLATE_CAPACITY);
    assert_root(&facts, Upper::Scratch, true);
    at_capacity.stop();
    assert!(template.leases().is_empty());

    // The clones changed nothing of the generation's snapshot, which they restored in place, and
    // left nothing in its payload.
    assert_eq!(files_below(&snapshot), captured);
    assert_payload(&template, false);
}

/// Checks that a clone's `processors` probe found the CPUs `online` online, of the possible CPUs
/// `possible`, and that the restore brought the CPUs `restored` online, `none` for none.
fn assert_processors(facts: &Printed, online: &str, possible: &str, restored: &str) {
    let found = ["online", "possible", "restored"].map(|key| &facts[&format!("processors.{key}")]);
    assert_eq!(found, [online, possible, restored], "{facts:?}");
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn one_generation_serves_clones_of_each_processor_count_from_its_boot_online_count_to_its_capacity()
{
    // A template of 4 processors, one of them online at boot, the host program's default.
    let template = Template::of_processors("cl-cpus", 4, None);
    let snapshot = template.payload().join(SNAPSHOT_NAME);
    let captured = files_below(&snapshot);
    // A clone takes the processors online at boot by default, which its restore brings none
    // online of, and the restore of each other target brings the rest of its prefix online.
    for (options, online, restored) in [
        (&[][..], "0", "none"),
        (&["--processors", "2"][..], "0-1", "1"),
        (&["--processors", "4"][..], "0-3", "1-3"),
    ] {
        let start = StartOf {
            options,
            ..StartOf::default()
        };
        let mut cloned = template.start("cl-n", start, 0);
        cloned.assert_started();
        let facts = cloned.probe(&["processors", "activation"]);
        assert_processors(&facts, online, "0-3", restored);
        assert_timeline(&facts);
        cloned.stop();
    }
    assert!(template.leases().is_empty());

    // A count that a microVM cannot have, and one beyond the template's capacity, are refused
    // before any VMM starts.
    for target in ["3", "8"] {
        let options = ["--processors", target];
        let start = StartOf {
            options: &options,
            ..StartOf::default()
        };
        let refused = template.start("cl-n", start, CLONE_FAILED);
        assert_eq!(
            failure(&refused.started),
            [
                "request",
                "invalid-targets",
                "plan",
                "planned",
                "none",
                "no-vm"
            ],
            "{target}: {:?}",
            refused.started
        );
        assert_eq!(refused.started["quarantined"], "no");
        refused.assert_refused(&format!(
            "cannot restore with a processor target of {target}: a target must be 1, 2, 4, or 8, \
             from the template's processors online at boot, 1, to its capacity, 4"
        ));
        assert!(template.leases().is_empty());
    }
    assert_eq!(files_below(&snapshot), captured);
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_generation_refuses_clones_of_fewer_processors_than_its_template_booted_with() {
    let template = Template::of_processors("cl-boot", 4, Some(2));
    let options = ["--processors", "1"];
    let start = StartOf {
        options: &options,
        ..StartOf::default()
    };
    let refused = template.start("cl-m", start, CLONE_FAILED);
    assert_eq!(
        failure(&refused.started),
        [
            "request",
            "invalid-targets",
            "plan",
            "planned",
            "none",
            "no-vm"
        ],
        "{:?}",
        refused.started
    );
    refused.assert_refused(
        "cannot restore with a processor target of 1: a target must be 1, 2, 4, or 8, from the \
         template's processors online at boot, 2, to its capacity, 4",
    );
    assert!(template.leases().is_empty());

    // Its clones take its 2 processors online at boot by default, and the restore of a larger
    // target brings online the CPUs beyond them.
    for (options, online, restored) in [
        (&[][..], "0-1", "none"),
        (&["--processors", "4"][..], "0-3", "2-3"),
    ] {
        let start = StartOf {
            options,
            ..StartOf::default()
        };
        let mut cloned = template.start("cl-m", start, 0);
        cloned.assert_started();
        let facts = cloned.probe(&["processors"]);
        assert_processors(&facts, online, "0-3", restored);
        cloned.stop();
    }
    assert!(template.leases().is_empty());
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn one_generation_serves_clones_of_either_format_over_either_upper() {
    // The placeholder is longer than the platform's image, so every clone's image is shorter than
    // the capacity that its restore presents: the platform's image, a GPT disk of its length, and
    // one as short as its layout allows.
    let template = Template::of_disk("cl-gpt", &PLACEHOLDER);
    let one = template.disk("one.gpt", &ONE_LAYER);
    let three = template.disk_of("three.gpt", &THREE_LAYERS, None);

    // At once: the platform's image over a scratch image and in RAM, and two GPT disks, of other
    // layouts than the placeholder's and than each other's, one in RAM, and one over a scratch
    // image, with a bound that its first partition meets exactly.
    let bound = THREE_LAYERS.first_size.to_string();
    let exact = ["--first-partition-bound", bound.as_str()];
    let starts = [
        ("cl-e", None, Upper::Scratch, &[][..]),
        ("cl-f", None, Upper::Ram, &[][..]),
        (
            "cl-g",
            Some(Image::Gpt(&ONE_LAYER, &one)),
            Upper::Ram,
            &[][..],
        ),
        (
            "cl-h",
            Some(Image::Gpt(&THREE_LAYERS, &three)),
            Upper::Scratch,
            &exact[..],
        ),
    ];
    let mut clones = starts.map(|(name, image, upper, options)| {
        let start = StartOf {
            image,
            upper,
            options,
            ..StartOf::default()
        };
        let cloned = template.start(name, start, 0);
        cloned.assert_started();
        (image, upper, cloned)
    });
    assert_eq!(template.leases().len(), clones.len());
    assert_payload(&template, true);
    for (image, upper, cloned) in &mut clones {
        let probes = [
            "devices",
            "mounts",
            "contents",
            "first-partition",
            "read-disks",
        ];
        let facts = cloned.probe(&probes);
        match image {
            Some(Image::Gpt(disk, _)) => assert_gpt_root(&facts, disk, *upper),
            _ => {
                assert_root(&facts, *upper, true);
                assert_eq!(facts["first-partition.length"], "none", "{facts:?}");
            }
        }
        // The restore presents the capacity over each image, and the template's scratch over the
        // scratch image, or, in RAM, with no file.
        assert_capacity(&facts, template.capacity);
        cloned.assert_vmm_scratch(*upper);
        if *upper == Upper::Ram {
            cloned.assert_scratch_unbacked();
        }
    }
    for (_, _, cloned) in &mut clones {
        cloned.stop();
    }
    assert!(template.leases().is_empty());

    // A disk whose first partition is larger than the clone's bound ends the clone before it is
    // ready.
    let under = (ONE_LAYER.first_size - 1).to_string();
    let options = ["--first-partition-bound", under.as_str()];
    let start = StartOf {
        image: Some(Image::Gpt(&ONE_LAYER, &one)),
        options: &options,
        ..StartOf::default()
    };
    let ended = template.start("cl-g", start, CLONE_FAILED);
    ended.assert_unactivated(&template, "unusable-root", status::ACTIVATION_ROOT);
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_start_refuses_what_the_template_cannot_serve_before_any_vmm_starts() {
    let template = Template::create("cl-refuse");
    let length = fs::metadata(&template.image).unwrap().len();
    let longer = template.variant("longer.erofs", length + 4096);
    let longer_disk = template.disk_of("longer.gpt", &ONE_LAYER, Some(length + 4096));
    // OpenVMM attaches block devices of whole 512-byte sectors alone.
    let ragged = template.variant("ragged.erofs", length + 100);
    let placeholder = |found: u64| {
        format!(
            "is {found} bytes long, longer than the template's placeholder, whose {length} bytes \
             are the capacity of every clone's image"
        )
    };
    let resized = 2 * DEFAULT_SCRATCH_LENGTH;
    let beyond = ["--memory", "2048"];
    let part = ["--memory", "320"];
    let cases = [
        (
            StartOf {
                image: Some(Image::Erofs(&longer)),
                ..StartOf::default()
            },
            "supplied-length",
            placeholder(length + 4096),
        ),
        (
            StartOf {
                image: Some(Image::Gpt(&ONE_LAYER, &longer_disk)),
                upper: Upper::Scratch,
                ..StartOf::default()
            },
            "supplied-length",
            placeholder(length + 4096),
        ),
        (
            StartOf {
                image: Some(Image::Erofs(&ragged)),
                ..StartOf::default()
            },
            "supplied-geometry",
            format!("of {} bytes cannot be attached", length + 100),
        ),
        (
            StartOf {
                upper: Upper::Resized(resized),
                ..StartOf::default()
            },
            "supplied-length",
            format!(
                "is {resized} bytes long, not the {DEFAULT_SCRATCH_LENGTH} bytes of the \
                 template's scratch"
            ),
        ),
        (
            StartOf {
                options: &beyond,
                ..StartOf::default()
            },
            "invalid-targets",
            format!("cannot restore with {} bytes of RAM", 2048 * MIB),
        ),
        (
            StartOf {
                options: &part,
                ..StartOf::default()
            },
            "invalid-targets",
            format!("cannot restore with {} bytes of RAM", 320 * MIB),
        ),
    ];
    for (start, evidence, problem) in cases {
        let refused = template.start("cl-r", start, CLONE_FAILED);
        // NVX's plan refused the request before anything of the clone's launch, so the start
        // released the lease that the store granted.
        assert!(refused.started.contains_key("lease"), "{problem}");
        assert_eq!(
            failure(&refused.started),
            ["request", evidence, "plan", "planned", "none", "no-vm"],
            "{problem}: {:?}",
            refused.started
        );
        assert_eq!(refused.started["quarantined"], "no");
        refused.assert_refused(&problem);
        assert!(template.leases().is_empty());
    }

    // The adapter refuses images that the sandbox does not stage, and releases the lease.
    let start = StartOf {
        unstaged: true,
        ..StartOf::default()
    };
    let refused = template.start("cl-r", start, FAILED);
    assert!(refused.started.contains_key("lease"));
    assert!(!refused.started.contains_key("failure.outcome"));
    refused.assert_refused("; stage the images first");
    assert!(template.leases().is_empty());

    // The store holds no other generation.
    let mut sandbox = Sandbox::new("cl-r");
    template.stage(&mut sandbox);
    let options = [
        OsStr::new("--template"),
        template.dir().as_os_str(),
        OsStr::new("--generation"),
        OsStr::new("2"),
        OsStr::new("--distro"),
        &erofs(&template.image),
        OsStr::new("--distro-identity"),
        OsStr::new(&template.identity),
        OsStr::new("--upper"),
        OsStr::new("ram"),
    ];
    // The store never published another generation: the selection names no template.
    let printed = sandbox.run("start", &options, SELECTION_REFUSED);
    let record = [
        "no-template",
        "unpublished",
        "selection",
        "none",
        "none",
        "no-vm",
    ];
    assert_eq!(failure(&printed), record, "{printed:?}");
    assert_eq!(printed["failure.code"], "not-found", "{printed:?}");
    assert_eq!(printed.len(), 7, "{printed:?}");
    assert!(sandbox.stderr().contains("holds no generation 2"));
    sandbox.assert_unlaunched();

    // A generation whose key is gone is malformed: NVX's plan, which reads the key once the store
    // leased the generation, attributes the failure to the template, and the start quarantines
    // the generation through its tombstone, which refuses a later start without a lease.
    fs::remove_file(template.payload().join(KEY_NAME)).unwrap();
    let refused = template.start("cl-r", StartOf::default(), CLONE_FAILED);
    assert!(
        refused.started.contains_key("lease"),
        "{:?}",
        refused.started
    );
    assert_eq!(
        failure(&refused.started),
        [
            "template",
            "malformed-generation",
            "plan",
            "planned",
            "none",
            "no-vm"
        ],
        "{:?}",
        refused.started
    );
    assert_eq!(refused.started["quarantined"], "yes");
    refused.assert_refused(" is missing");
    assert!(template.leases().is_empty());
    assert_eq!(template.store().tombstones().unwrap(), [GENERATION]);
    let quarantined = template.start("cl-r", StartOf::default(), SELECTION_REFUSED);
    quarantined.assert_selection_refused(
        "ineligible",
        "tombstone",
        "template-tombstoned",
        &format!("a tombstone quarantines the generation {GENERATION} of the template store"),
    );
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_whose_activation_fails_ends_before_it_is_ready() {
    let template = Template::create("cl-unready");
    let forbidden = ["--scratch-journal", "forbidden"];
    let stranger = ["--workload-identity", STRANGER];
    let cases = [
        (
            StartOf {
                upper: Upper::Zeros,
                ..StartOf::default()
            },
            ("unusable-root", status::ACTIVATION_ROOT),
        ),
        (
            StartOf {
                upper: Upper::Scratch,
                options: &forbidden,
                ..StartOf::default()
            },
            ("unusable-root", status::ACTIVATION_ROOT),
        ),
        (
            StartOf {
                options: &stranger,
                ..StartOf::default()
            },
            ("invalid-settings", status::ACTIVATION_WORKLOAD),
        ),
    ];
    for (start, (evidence, expected)) in cases {
        let ended = template.start("cl-u", start, CLONE_FAILED);
        // The VM started, and ended before the clone was ready, with the status of the failure's
        // class, which attributes the failure to the request, and the start released the lease.
        ended.assert_unactivated(&template, evidence, expected);
    }

    // Until the clone is ready, it takes nothing but its Activate: a ping, a start, and the start
    // of an execution, before it, each end the clone with the status of the host's session.
    let timers = Timers::new(Duration::from_secs(1), Duration::from_secs(5));
    let execution = Start::new(Key::new(1, 1), limits(), timers, [b"/bin/true".to_vec()]);
    for early in [
        Message::Ping,
        Message::Start,
        HostMessage::Start(execution).encode(),
    ] {
        refuses_early(&template, early);
    }
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_refuses_a_scratch_image_whose_upper_an_earlier_clone_used() {
    let template = Template::create("cl-fresh");
    // A clone over a scratch image, in whose root the `contents` probe writes a file, which
    // reaches the upper.
    let start = StartOf {
        upper: Upper::Scratch,
        ..StartOf::default()
    };
    let mut earlier = template.start("cl-f", start, 0);
    earlier.assert_started();
    let facts = earlier.probe(&["contents"]);
    assert_eq!(
        facts["contents.write.upper"], "runtime-adapter",
        "{facts:?}"
    );
    earlier.stop();

    // A clone over a copy of its scratch image, whose upper is not fresh, ends before it is
    // ready.
    let used = earlier.scratch();
    let start = StartOf {
        upper: Upper::Used(&used),
        ..StartOf::default()
    };
    let ended = template.start("cl-u", start, CLONE_FAILED);
    ended.assert_unactivated(&template, "unusable-root", status::ACTIVATION_ROOT);
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn clones_at_once_keep_their_own_uppers_and_machine_ids() {
    let template = Template::create("cl-view");
    // Three clones at once: two whose guests generate their machine IDs, over a scratch image and
    // in RAM, and one in RAM that keeps the image's own.
    let generated = ["--machine-id", "generated"];
    let starts = [
        ("cl-s", Upper::Scratch, &generated[..]),
        ("cl-r", Upper::Ram, &generated[..]),
        ("cl-i", Upper::Ram, &[][..]),
    ];
    let mut clones = starts.map(|(name, upper, options)| {
        let start = StartOf {
            upper,
            options,
            ..StartOf::default()
        };
        let cloned = template.start(name, start, 0);
        cloned.assert_started();
        (name, cloned)
    });
    assert_eq!(template.leases().len(), clones.len());

    // Each clone's first execution sees the clone's hostname, and a root of the clone's own,
    // without the marker that the executions before it left in the other clones' roots.
    let firsts = clones.each_mut().map(|(name, cloned)| {
        let view = cloned.view(name);
        let seen = [view.hostname.as_str(), view.marker.as_str()];
        assert_eq!(seen, [CLONE_HOSTNAME, "absent"], "{view:?}");
        view
    });
    let [scratch, ram, image] = &firsts;
    assert_generated(scratch);
    assert_generated(ram);
    assert_ne!(scratch.machine_id, ram.machine_id);
    // The guest leaves the image's own machine ID alone: the Ubuntu image has none, and an
    // execution may write one.
    let image_view = (
        image.machine_id.as_str(),
        image.binds.len(),
        image.write.as_str(),
    );
    assert_eq!(image_view, ("absent", 0, "written"), "{image:?}");

    // A later execution of each clone sees the machine ID that its first saw, or, with the image's
    // own, the one that its first wrote, and its first's marker.
    for ((name, cloned), first) in clones.iter_mut().zip(&firsts) {
        let machine_id = match first.binds.as_slice() {
            [] => WRITTEN_MACHINE_ID,
            _ => first.machine_id.as_str(),
        };
        let expected = View {
            machine_id: machine_id.to_owned(),
            marker: (*name).to_owned(),
            ..first.clone()
        };
        assert_eq!(cloned.view(name), expected);
    }
    for (_, cloned) in &mut clones {
        cloned.stop();
    }
    assert!(template.leases().is_empty());
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_whose_view_cannot_be_set_ends_before_it_is_ready() {
    // The template of the EROFS image serves clones of a GPT disk of its length too.
    let template = Template::create("cl-noview");
    let disk = template.disk("etc.gpt", &ETC_FILE);
    let image = Some(Image::Gpt(&ETC_FILE, &disk));

    // With the image's own machine ID, the guest binds nothing in the root, so a clone of a disk
    // whose top layer holds a regular file `etc` starts.
    let start = StartOf {
        image,
        ..StartOf::default()
    };
    let mut cloned = template.start("cl-n", start, 0);
    cloned.assert_started();
    let facts = cloned.probe(&["devices", "mounts", "contents", "first-partition"]);
    assert_gpt_root(&facts, &ETC_FILE, Upper::Ram);
    cloned.stop();

    // A generated machine ID, which the guest binds at `/etc/machine-id`, cannot be bound below a
    // file, so a clone of the disk that asks for one ends before it is ready, with the status of
    // the workload's view, which attributes the failure to the request's invalid settings.
    let options = ["--machine-id", "generated"];
    let start = StartOf {
        image,
        options: &options,
        ..StartOf::default()
    };
    let ended = template.start("cl-n", start, CLONE_FAILED);
    ended.assert_unactivated(&template, "invalid-settings", status::ACTIVATION_WORKLOAD);
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_runs_its_executions_as_its_workload_identity_once_it_is_ready() {
    let template = Template::create("cl-ident");
    let options = ["--workload-identity", NOBODY];
    let start = StartOf {
        upper: Upper::Scratch,
        options: &options,
        ..StartOf::default()
    };
    let mut cloned = template.start("cl-i", start, 0);
    cloned.assert_started();
    let facts = cloned.probe(&["activation"]);
    let events = assert_timeline(&facts);
    assert_eq!(facts["activation.execution"], "none", "{facts:?}");

    // Every execution runs as the record's workload identity, and one that names its own is
    // refused before its program starts.
    let ran = cloned.exec(&["--", "/bin/sh", "-c", "id -u; id -g"]);
    let facts = ["launch", "status", "stdout", "stderr"].map(|key| &ran[&format!("exec.1.{key}")]);
    assert_eq!(
        facts,
        ["started", "exit 0", "65534\\n65534\\n", ""],
        "{ran:?}"
    );
    let refused = cloned.exec(&["--identity", "1000:1000", "--", "/bin/true"]);
    let launch = format!("failed 0 {EINVAL}");
    assert_eq!(refused["exec.1.launch"], launch, "{refused:?}");

    // The first execution came once the clone's sandbox started.
    let facts = cloned.probe(&["activation"]);
    let execution = facts["activation.execution"].parse::<u64>().unwrap();
    assert!(execution >= events[3], "{facts:?}");
    assert_eq!(assert_timeline(&facts), events);
    cloned.stop();
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_generation_is_eligible_by_the_clock_of_its_capture() {
    let template = Template::create("cl-clock");
    let contract = template.generation().time().clone();
    let now = HostTime::now().unwrap();
    // The scenario runs on the host's boot that captured the template.
    assert_eq!(
        (now.host_id, now.boot_id, now.clock),
        (contract.host_id, contract.boot_id, contract.host_clock)
    );
    let started = |time: Option<HostTime>| {
        let start = StartOf {
            time,
            ..StartOf::default()
        };
        let mut cloned = template.start("cl-k", start, 0);
        cloned.assert_started();
        cloned.stop();
        cloned.started
    };

    // On that boot, the host's monotonic clock measures the downtime, whatever UTC says.
    let printed = started(None);
    assert_eq!(printed["source"], "monotonic", "{printed:?}");
    let behind = HostTime {
        utc_ns: contract.capture_utc_ns - 24 * HOUR_NS,
        ..now
    };
    let printed = started(Some(behind));
    assert_eq!(printed["source"], "monotonic", "{printed:?}");
    let downtime = behind.monotonic_ns - contract.capture_monotonic_ns;
    assert_eq!(printed["downtime"], downtime.to_string());

    // After a reboot, UTC does.
    let rebooted = HostTime {
        boot_id: other_boot(contract.boot_id),
        monotonic_ns: 1,
        utc_ns: contract.capture_utc_ns + HOUR_NS,
        ..now
    };
    let printed = started(Some(rebooted));
    assert_eq!(printed["source"], "utc", "{printed:?}");
    assert_eq!(printed["downtime"], HOUR_NS.to_string());

    // A generation that is no longer eligible, or not yet, takes no lease: the store refuses the
    // selection of an expired generation, and of one whose capture is after the host's time.
    let cases = [
        (
            HostTime {
                monotonic_ns: contract.capture_monotonic_ns + MAX_DOWNTIME_NS + 1,
                ..now
            },
            ["expired", "expired"],
            "is no longer eligible: after a downtime of",
            "by the host's monotonic clock",
        ),
        (
            HostTime {
                utc_ns: contract.capture_utc_ns + MAX_DOWNTIME_NS + 1,
                ..rebooted
            },
            ["expired", "expired"],
            "is no longer eligible: after a downtime of",
            "by UTC",
        ),
        (
            HostTime {
                utc_ns: contract.capture_utc_ns - 1,
                ..rebooted
            },
            ["ineligible", "early"],
            "is not eligible yet: by UTC",
            "1 ns before the anchor of the capture",
        ),
    ];
    for (time, [outcome, evidence], state, measured) in cases {
        let start = StartOf {
            time: Some(time),
            ..StartOf::default()
        };
        let refused = template.start("cl-k", start, SELECTION_REFUSED);
        let generation = format!("the generation {GENERATION} of the template store");
        refused.assert_selection_refused(outcome, evidence, "template-ineligible", &generation);
        let stderr = refused.sandbox.stderr();
        assert!(
            stderr.contains(state) && stderr.contains(measured),
            "{stderr}"
        );
        assert!(template.leases().is_empty());
    }
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_reads_no_more_of_its_disks_than_its_root_needs() {
    let template = Template::create("cl-reads");
    for upper in [Upper::Scratch, Upper::Ram] {
        let start = StartOf {
            upper,
            ..StartOf::default()
        };
        let mut cloned = template.start("cl-d", start, 0);
        cloned.assert_started();
        let facts = cloned.probe(&["devices", "reads"]);
        let sizes = disks(&facts["devices.disks"]);
        let restored = disks(&facts["reads.disks"]);
        assert_eq!(
            sizes.keys().collect::<Vec<_>>(),
            restored.keys().collect::<Vec<_>>(),
            "{facts:?}"
        );
        // The image and the scratch device, whose lengths are the template's.
        let mut lengths = sizes
            .values()
            .map(|sectors| sectors * SECTOR)
            .collect::<Vec<_>>();
        lengths.sort_unstable();
        let mut expected = [
            fs::metadata(&template.image).unwrap().len(),
            DEFAULT_SCRATCH_LENGTH,
        ];
        expected.sort_unstable();
        assert_eq!(lengths, expected, "{facts:?}");
        for (disk, sectors) in &sizes {
            assert!(
                restored[disk] * 8 < *sectors,
                "{upper:?}: the clone read {} of the {sectors} sectors of {disk}",
                restored[disk]
            );
        }

        // A read of each disk to its end, which the statistics count.
        let facts = cloned.probe(&["read-disks"]);
        let read = disks(&facts["read-disks.disks"]);
        let whole = sizes
            .iter()
            .map(|(disk, sectors)| (disk.clone(), sectors * SECTOR))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(read, whole, "{facts:?}");
        let facts = cloned.probe(&["reads"]);
        let after = disks(&facts["reads.disks"]);
        for (disk, sectors) in &sizes {
            assert!(
                after[disk] >= restored[disk] + sectors / 2,
                "{upper:?}: {disk} read {} sectors, then {}, of {sectors}",
                restored[disk],
                after[disk]
            );
        }
        cloned.stop();
    }
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_s_time_settles_after_it_is_ready() {
    let template = Template::create("cl-time");
    let mut cloned = template.start("cl-t", StartOf::default(), 0);
    cloned.assert_started();
    let deadline = Instant::now() + TIME_SETTLEMENT;
    let facts = loop {
        let facts = cloned.probe(&["time"]);
        let status = facts.get("time.restore-status").map(String::as_str);
        if !matches!(status, None | Some("pending")) {
            break facts;
        }
        assert!(Instant::now() < deadline, "{facts:?}");
        thread::sleep(Duration::from_millis(100));
    };
    let settled = [
        "restore-status",
        "violations",
        "last-downtime-source",
        "restore-generation",
    ]
    .map(|key| &facts[&format!("time.{key}")]);
    let generation = &facts["time.generation"];
    assert_eq!(
        settled,
        ["ok", "0", "monotonic", generation.as_str()],
        "{facts:?}"
    );
    let downtime = facts["time.last-downtime-ns"].parse::<u64>().unwrap();
    assert!(downtime > 0, "{facts:?}");

    // The clone still serves its order.
    let facts = cloned.probe(&["activation", "memory"]);
    assert_timeline(&facts);
    assert_memory(&facts, TEMPLATE_MEMORY);
    cloned.stop();
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn each_command_reattaches_to_the_clone_from_a_new_process() {
    let template = Template::create("cl-again");
    let start = StartOf {
        upper: Upper::Scratch,
        ..StartOf::default()
    };
    let mut cloned = template.start("cl-g", start, 0);
    cloned.assert_started();
    // The launch committed: the sandbox records the clone's VM, and how far the clone got, and no
    // failure.
    let store = SandboxStore::open(&cloned.sandbox.store).unwrap();
    let records = CloneRecords::read(&store).unwrap().unwrap();
    assert_eq!(records.lease.id().to_string(), cloned.started["lease"]);
    assert_eq!(records.progress, Progress::Committed);
    assert!(records.runtime.is_some(), "{records:?}");
    assert!(records.journal.is_none() && records.failure.is_none());

    // Each command, from a new process, reattaches to the clone in a new session of the same
    // broker, in which the clone answers the Activate of its record again with its Ready, without
    // activating again, so that the events of its activation stay as they were.
    let first = cloned.sandbox.run("reattach", &[], 0);
    assert_eq!(first["reattached"], "live", "{first:?}");
    assert_eq!(first["pid"], cloned.started["pid"]);
    let facts = cloned.probe(&["activation"]);
    let events = assert_timeline(&facts);
    let ran = cloned.exec(&["--identity", NOBODY, "--", "/bin/sh", "-c", "id -u"]);
    let facts_ran = ["launch", "status", "stdout"].map(|key| &ran[&format!("exec.1.{key}")]);
    assert_eq!(facts_ran, ["started", "exit 0", "65534\\n"], "{ran:?}");
    let last = cloned.sandbox.run("reattach", &[], 0);
    let epochs = [&first, &facts, &ran, &last].map(|printed| {
        assert_eq!(printed["instance"], first["instance"], "{printed:?}");
        printed["epoch"].parse::<u64>().unwrap()
    });
    assert!(
        epochs.windows(2).all(|pair| pair[0] < pair[1]),
        "{epochs:?}"
    );
    let facts = cloned.probe(&["activation"]);
    let again = assert_timeline(&facts);
    assert_eq!(again, events, "{facts:?}");
    let execution = facts["activation.execution"].parse::<u64>().unwrap();
    assert!(execution >= events[3], "{facts:?}");
    cloned.stop();
    let store = SandboxStore::open(&cloned.sandbox.store).unwrap();
    assert!(CloneRecords::read(&store).unwrap().is_none());
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_host_program_of_another_runtime_ends_the_clone_through_its_verified_identity() {
    let template = Template::create("cl-other");
    let cloned = template.start("cl-o", StartOf::default(), 0);
    cloned.assert_started();
    let vmm = cloned.sandbox.vmm().expect("the clone's VMM runs");
    let mut sandbox = cloned.sandbox;

    // The reattach finds the clone's VMM under another runtime identity, which might not speak
    // this host's protocol, so it ends it through its verified identity, without a session, and
    // the clone fails, by the host's evidence, which quarantines nothing.
    let options = [OsStr::new("--guest-build"), OsStr::new(ANOTHER_BUILD)];
    let ended = sandbox.run("reattach", &options, CLONE_FAILED);
    assert_eq!(ended["reattached"], "failed", "{ended:?}");
    assert_eq!(
        failure(&ended),
        [
            "host",
            "runtime-mismatch",
            "reattach",
            "committed",
            "none",
            "terminated"
        ],
        "{ended:?}"
    );
    assert_eq!(ended["failure.code"], "runtime-mismatch");
    assert_eq!(ended["quarantined"], "no");
    assert_eq!(ended["released"], cloned.started["lease"], "{ended:?}");
    assert!(!ended.contains_key("epoch"), "{ended:?}");
    assert!(sandbox.stderr().contains("differs in initramfs"));
    assert!(
        vmm.wait(Instant::now() + Duration::from_secs(10)).unwrap(),
        "the clone's VMM still runs"
    );
    assert!(sandbox.record().is_none());
    let store = SandboxStore::open(&sandbox.store).unwrap();
    assert!(CloneRecords::read(&store).unwrap().is_none());
    assert!(template.leases().is_empty());

    // The template still serves clones.
    let mut another = template.start("cl-p", StartOf::default(), 0);
    another.assert_started();
    another.stop();
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_whose_vm_exits_once_a_stop_found_it_fails_before_its_lease_is_released() {
    let template = Template::create("cl-gone");
    let cloned = template.start("cl-x", StartOf::default(), 0);
    cloned.assert_started();
    let vmm = cloned.sandbox.vmm().expect("the clone's VMM runs");
    let mut sandbox = cloned.sandbox;

    // The stop finds the clone running, in a new session, and pauses there, while the clone's VMM
    // ends without a word of its guest, as a crash of the VMM would end it.
    let pause = sandbox.root.join("found.pause");
    let hook = OsString::from(
        StepHook {
            step: Step::Found,
            action: Action::Pause(pause.clone()),
        }
        .to_string(),
    );
    let mut stopping = sandbox.start("stop", &[], &[(STEP_HOOK, &hook)]);
    assert_eq!(sandbox.paused(&mut stopping, &pause), "found");
    vmm.kill().unwrap();
    assert!(
        vmm.wait(Instant::now() + Duration::from_secs(10)).unwrap(),
        "the clone's VMM still runs"
    );
    fs::remove_file(&pause).unwrap();

    // The stop stops nothing, and the clone's VM ended without its holder, with no report of the
    // guest's own end: the clone fails, as NVX attributes the loss of its status, which
    // quarantines nothing, before the stop releases the lease.
    let ended = sandbox.finish(stopping, CLONE_FAILED);
    let found = ["reattached", "pid", "stopped"].map(|key| &ended[key]);
    assert_eq!(
        found,
        ["live", cloned.started["pid"].as_str(), "exited"],
        "{ended:?}"
    );
    assert_eq!(
        failure(&ended),
        [
            "unknown",
            "lost-status",
            "run",
            "committed",
            "none",
            "exited"
        ],
        "{ended:?}"
    );
    assert_eq!(ended["failure.code"], "exited");
    assert_eq!(ended["quarantined"], "no");
    assert_eq!(ended["released"], cloned.started["lease"], "{ended:?}");
    let stderr = sandbox.stderr();
    assert!(
        stderr.contains("without an outcome report of the guest's own end"),
        "{stderr}"
    );
    assert!(sandbox.record().is_none());
    let store = SandboxStore::open(&sandbox.store).unwrap();
    assert!(CloneRecords::read(&store).unwrap().is_none());
    assert!(template.leases().is_empty());
    assert!(template.store().tombstones().unwrap().is_empty());

    // The template still serves clones.
    let mut another = template.start("cl-y", StartOf::default(), 0);
    another.assert_started();
    another.stop();
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_tombstone_leaves_the_running_clone_of_its_generation_to_its_lease() {
    let template = Template::create("cl-tomb");
    let snapshot = template.payload().join(SNAPSHOT_NAME);
    let captured = files_below(&snapshot);
    let start = StartOf {
        upper: Upper::Scratch,
        ..StartOf::default()
    };
    let mut cloned = template.start("cl-q", start, 0);
    cloned.assert_started();
    let store = template.store();
    let deadline = Instant::now() + Duration::from_secs(10);
    store.tombstone(GENERATION, Some(deadline)).unwrap();
    assert_eq!(store.tombstones().unwrap(), [GENERATION]);

    // The generation takes no new lease: the store refuses the selection of a quarantined
    // generation as ineligible.
    let refused = template.start("cl-n", StartOf::default(), SELECTION_REFUSED);
    refused.assert_selection_refused(
        "ineligible",
        "tombstone",
        "template-tombstoned",
        &format!("a tombstone quarantines the generation {GENERATION} of the template store"),
    );

    // The clone keeps running on its lease: it reattaches, serves its order, and stops, which
    // releases the lease.
    let facts = cloned.probe(&["activation", "memory"]);
    assert_timeline(&facts);
    assert_memory(&facts, TEMPLATE_MEMORY);
    let ran = cloned.exec(&["--identity", NOBODY, "--", "/bin/true"]);
    assert_eq!(ran["exec.1.status"], "exit 0", "{ran:?}");
    assert_eq!(template.leases().len(), 1);
    cloned.stop();
    assert!(template.leases().is_empty());

    // The tombstone changed nothing of the generation's snapshot.
    assert_eq!(files_below(&snapshot), captured);
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn the_lease_of_a_start_that_ended_before_its_record_is_released_through_its_template_store() {
    let template = Template::create("cl-lost");
    // A start that ended once the store leased the generation, before it recorded the clone,
    // leaves the lease, and no record that names the template store.
    let mut sandbox = Sandbox::new("cl-l");
    template.stage(&mut sandbox);
    let lease = lease_to(&template, &sandbox);

    // A sandbox that holds a lease must not move. A command on one that moved refuses it, rather
    // than take for it the sandbox that the lease's path holds then: one with a lease of its own,
    // then one without a lease.
    let named = sandbox.store.clone();
    let moved = sandbox.root.join("moved");
    fs::rename(&named, &moved).unwrap();
    SandboxStore::create(&named).unwrap();
    let other = lease_to(&template, &sandbox);
    sandbox.store = moved.clone();
    let refused = sandbox.run("stop", &[], FAILED);
    assert!(refused.is_empty(), "{refused:?}");
    let stderr = sandbox.stderr();
    assert!(stderr.contains("which does not hold it"), "{stderr}");
    assert_eq!(template.leases().len(), 2);
    sandbox.store = named.clone();
    let options = [OsStr::new("--template"), template.dir().as_os_str()];
    let released = sandbox.run("stop", &options, FAILED);
    assert_eq!(released["released"], other, "{released:?}");
    sandbox.store = moved.clone();
    let refused = sandbox.run("reattach", &[], FAILED);
    assert!(refused.is_empty(), "{refused:?}");
    let stderr = sandbox.stderr();
    assert!(stderr.contains("which does not hold it"), "{stderr}");
    let leases = template.leases();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].id().to_string(), lease);
    // The sandbox moves back, as its lease names it.
    fs::remove_dir_all(&named).unwrap();
    fs::rename(&moved, &named).unwrap();
    sandbox.store = named;

    // The stop finds that the clone never launched, and keeps the lease without its template
    // store.
    let kept = sandbox.run("stop", &[], FAILED);
    assert_eq!(kept["reattached"], "ended", "{kept:?}");
    assert_eq!(kept["progress"], "planned", "{kept:?}");
    assert!(!kept.contains_key("released"), "{kept:?}");
    let stderr = sandbox.stderr();
    assert!(stderr.contains("records no clone"), "{stderr}");
    assert!(stderr.contains("--template TEMPLATES"), "{stderr}");
    assert_eq!(template.leases().len(), 1);

    // Another template store does not release it.
    let other = sandbox.root.join("other");
    Store::create(&other).unwrap();
    let options = [OsStr::new("--template"), other.as_os_str()];
    let refused = sandbox.run("stop", &options, FAILED);
    assert!(!refused.contains_key("released"), "{refused:?}");
    let stderr = sandbox.stderr();
    assert!(stderr.contains("not of this one"), "{stderr}");
    assert_eq!(template.leases().len(), 1);

    // Its own does, and the sandbox leases the generation again, to a clone whose record the
    // release then leaves.
    let options = [OsStr::new("--template"), template.dir().as_os_str()];
    let released = sandbox.run("reattach", &options, FAILED);
    assert_eq!(released["released"], lease, "{released:?}");
    assert!(template.leases().is_empty());
    let started = start_in(&mut sandbox, &template, template.dir());
    assert_ne!(started["lease"], lease, "{started:?}");
    let stopped = sandbox.run("stop", &[], 0);
    assert_eq!(stopped["released"], started["lease"], "{stopped:?}");
    assert!(template.leases().is_empty());

    // A start of another template's clone in the sandbox that ended as the first did leaves a
    // lease of that template's store, which the earlier clone's record does not name.
    let another = Template::create("cl-gone");
    let lease = lease_to(&another, &sandbox);
    let kept = sandbox.run("stop", &[], FAILED);
    assert!(!kept.contains_key("released"), "{kept:?}");
    let stderr = sandbox.stderr();
    assert!(stderr.contains("which did not grant its lease"), "{stderr}");
    assert!(stderr.contains("--template TEMPLATES"), "{stderr}");
    assert_eq!(another.leases().len(), 1);
    let options = [OsStr::new("--template"), another.dir().as_os_str()];
    let released = sandbox.run("stop", &options, FAILED);
    assert_eq!(released["released"], lease, "{released:?}");
    assert!(another.leases().is_empty());
    let started = start_in(&mut sandbox, &template, another.dir());
    let stopped = sandbox.run("stop", &[], 0);
    assert_eq!(stopped["released"], started["lease"], "{stopped:?}");
    assert!(another.leases().is_empty());
    assert_payload(&template, false);
    assert_payload(&another, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_selection_that_names_no_template_leases_nothing() {
    let mut template = Template::create("cl-none");

    // A template store that does not exist.
    let sandbox = template
        .sandbox
        .as_mut()
        .expect("the template keeps its sandbox");
    let missing = sandbox.root.join("missing");
    let start = StartOf {
        templates: Some(&missing),
        ..StartOf::default()
    };
    let refused = template.start("cl-m", start, SELECTION_REFUSED);
    refused.assert_selection_refused(
        "no-template",
        "no-store",
        "not-found",
        "cannot open the template store",
    );
    assert!(!missing.exists());

    // A generation that the store retired, once no lease kept it.
    let sandbox = template
        .sandbox
        .as_mut()
        .expect("the template keeps its sandbox");
    let removed = sandbox.run("remove", &[OsStr::new("1")], 0);
    assert_eq!(removed["removed"], "retired", "{removed:?}");
    let refused = template.start("cl-m", StartOf::default(), SELECTION_REFUSED);
    let problem = format!("holds no generation {GENERATION}");
    refused.assert_selection_refused("no-template", "retired", "not-found", &problem);
    let store = template.store();
    assert!(store.index().unwrap().generations().is_empty());
    assert!(store.leases(GENERATION).unwrap().is_empty());
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_run_runs_one_execution_in_its_clone_then_tears_the_clone_down() {
    let template = Template::create("cl-run");
    // The execution's program, its arguments, its environment, and its working directory reach
    // the guest in its start, and it runs in the clone's root, the Ubuntu image's, as the clone's
    // workload identity. The run ends with success whatever the program's status.
    let options = ["--workload-identity", NOBODY];
    let start = StartOf {
        upper: Upper::Scratch,
        options: &options,
        ..StartOf::default()
    };
    let script = ". /etc/os-release; echo \"$GREETING $ID $(id -u) $(pwd)\"; echo done >&2; exit 3";
    let execution = [
        "--env",
        "GREETING=hello",
        "--cwd",
        "/tmp",
        "--",
        "/bin/sh",
        "-c",
        script,
    ];
    let ran = template.run("cl-r", start, &execution, 0);
    ran.assert_ran();
    let printed = &ran.started;
    let facts = [
        "launch",
        "status",
        "cause",
        "truncated",
        "settled",
        "stdout",
        "stderr",
    ]
    .map(|key| &printed[&format!("exec.1.{key}")]);
    assert_eq!(
        facts,
        [
            "started",
            "exit 3",
            "none",
            "none",
            "reusable",
            "hello ubuntu 65534 /tmp\\n",
            "done\\n"
        ],
        "{printed:?}"
    );
    assert!(template.leases().is_empty());

    // An execution that the guest refuses before its program starts, one that names an identity
    // in a clone that runs as its own, leaves the run to tear the clone down as well.
    let start = StartOf {
        options: &options,
        ..StartOf::default()
    };
    let execution = ["--identity", "1000:1000", "--", "/bin/true"];
    let ran = template.run("cl-r", start, &execution, 0);
    ran.assert_ran();
    let printed = &ran.started;
    let launch = format!("failed 0 {EINVAL}");
    let facts = ["launch", "status", "settled"].map(|key| &printed[&format!("exec.1.{key}")]);
    assert_eq!(facts, [launch.as_str(), "none", "fenced"], "{printed:?}");
    assert!(template.leases().is_empty());
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_run_whose_clone_does_not_start_runs_nothing() {
    let template = Template::create("cl-norun");
    let execution = ["--identity", NOBODY, "--", "/bin/true"];

    // The store refuses a selection that names no template, and one of a generation whose restore
    // window passed, once the run's preflight staged the clone's images.
    let sandbox = template
        .sandbox
        .as_ref()
        .expect("the template keeps its sandbox");
    let missing = sandbox.root.join("missing");
    let start = StartOf {
        templates: Some(&missing),
        ..StartOf::default()
    };
    let refused = template.run("cl-n", start, &execution, SELECTION_REFUSED);
    refused.assert_selection_refused(
        "no-template",
        "no-store",
        "not-found",
        "cannot open the template store",
    );
    assert!(!missing.exists());
    let contract = template.generation().time().clone();
    let expired = HostTime {
        monotonic_ns: contract.capture_monotonic_ns + MAX_DOWNTIME_NS + 1,
        ..HostTime::now().unwrap()
    };
    let start = StartOf {
        time: Some(expired),
        ..StartOf::default()
    };
    let refused = template.run("cl-n", start, &execution, SELECTION_REFUSED);
    let generation = format!("the generation {GENERATION} of the template store");
    refused.assert_selection_refused("expired", "expired", "template-ineligible", &generation);

    // A clone whose activation fails ends the run before its execution, with the record of the
    // failure, once the VM exited, and the lease released.
    let start = StartOf {
        upper: Upper::Zeros,
        ..StartOf::default()
    };
    let failed = template.run("cl-n", start, &execution, CLONE_FAILED);
    let printed = &failed.started;
    assert_eq!(printed["preflight"], "staged", "{printed:?}");
    assert!(printed.contains_key("pid"), "{printed:?}");
    let status = status::ACTIVATION_ROOT.to_string();
    assert_eq!(
        failure(printed),
        [
            "request",
            "unusable-root",
            "activation",
            "activating",
            status.as_str(),
            "exited"
        ],
        "{printed:?}"
    );
    assert_eq!(printed["quarantined"], "no");
    assert_eq!(printed["released"], printed["lease"], "{printed:?}");
    assert!(
        !printed
            .keys()
            .any(|key| key.starts_with("exec.") || key == "stopped"),
        "{printed:?}"
    );
    assert!(failed.sandbox.record().is_none());
    assert!(template.leases().is_empty());
    assert_payload(&template, false);
}

/// Leases the generation of `template` to the store of `sandbox`, through NVX's store, as a start
/// does before it records its clone, and returns the lease's identity.
fn lease_to(template: &Template, sandbox: &Sandbox) -> String {
    let now = HostTime::now().unwrap();
    let acquired = template
        .store()
        .acquire(GENERATION, &sandbox.store, &CallerFiles::none(), &now, None)
        .unwrap();
    acquired.lease.id().to_string()
}

/// Starts a clone in RAM, in the store of `sandbox`, which staged the image of `template`, of the
/// generation of the template store `templates`, checks that it started, and returns what the
/// start printed.
fn start_in(sandbox: &mut Sandbox, template: &Template, templates: &Path) -> Printed {
    let generation = GENERATION.to_string();
    let distro = erofs(&template.image);
    let options = [
        OsStr::new("--template"),
        templates.as_os_str(),
        OsStr::new("--generation"),
        OsStr::new(&generation),
        OsStr::new("--distro"),
        &distro,
        OsStr::new("--distro-identity"),
        OsStr::new(&template.identity),
        OsStr::new("--upper"),
        OsStr::new("ram"),
    ];
    sandbox.run("start", &options, 0)
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_maps_an_aggregate_of_directories_and_files_beneath_its_root_before_its_sandbox_starts() {
    let template = Template::create("cl-export");
    let export = Export::write(&template, "full", &EXPORT_FILES, &EXPORT_DIRECTORIES);
    // A Linux host shows the guest each host object's owner and mode, which the guest's kernel
    // checks, so that anyone may write the read-write file.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let permissions = fs::Permissions::from_mode(0o666);
        fs::set_permissions(export.path("notes.txt"), permissions).unwrap();
    }
    let mut shares = export.options(&EXPORT_SHARES, &EXPORT_DENIED);
    for path in EXPORT_ALLOWED {
        shares.extend([
            "--allow".to_owned(),
            export.path(path).display().to_string(),
        ]);
    }
    let options = shares.iter().map(String::as_str).collect::<Vec<_>>();
    let start = StartOf {
        upper: Upper::Scratch,
        options: &options,
        ..StartOf::default()
    };
    let mut clone = template.start("cl-export-1", start, 0);
    clone.assert_mapped(EXPORT_SHARES.len());

    // The clone's record names the export, an aggregate, by the identity of its plan, and the
    // number of its mappings, which the restore sequence sent once the clone was ready. NVX
    // attached the aggregate to the template's dormant filesystem slot as a cold launch attaches
    // one: its children, the outermost shares, in order, `dir` read-write, since the guest may
    // write `dir/inner` in it, and nothing else of it, each file with its access, and `out`, then
    // the paths that the start hides, the one that it shows again, and the one that the guest may
    // write in `dir`.
    let configuration = clone.configuration();
    let recorded = configuration.export.expect("the record names the export");
    assert_eq!(recorded.kind, ExportKind::Aggregate);
    assert_eq!(configuration.mappings, EXPORT_SHARES.len());
    let mut attached = export.attached(&[
        ("dir", "rw"),
        ("file.txt", "ro,file"),
        ("notes.txt", "rw,file"),
        ("out", "rw"),
    ]);
    for (option, path) in [
        ("--mount-deny", "dir/secret"),
        ("--mount-deny", "out/private"),
        ("--mount-allow", "out/private/shown"),
        ("--mount-write", "dir/inner"),
    ] {
        attached.push((option, export.canonical(path)));
    }
    clone.assert_vmm_export(&attached);

    // The export, mounted once, read-write, without set-user-ID programs or devices, below a
    // directory of the runtime's that only root may enter, and the table's mappings below the
    // root, and nothing else, in their order, as a launch's: each with its access, `nosuid`, and
    // `nodev`, the guest writing the writable ones alone, the one inside a read-only mapping
    // included, each showing what the host holds, but the files that the start hides, and the
    // directory that holds the one that it shows again, which lists it alone. The guest created
    // the missing targets in the upper, empty, but none inside another mapping.
    let facts = clone.probe(&[MAP_EXPORT]);
    let fact = |key: &str| facts[&format!("map.{key}")].as_str();
    assert_eq!(
        (fact("export.count"), fact("export.type")),
        ("1", "virtiofs")
    );
    let options = mount_options(fact("export.options"));
    assert!(
        options.is_superset(&BTreeSet::from(["rw", "nosuid", "nodev"])),
        "{options:?}"
    );
    assert_eq!((fact("hostfs.mode"), fact("hostfs.owner")), ("0700", "0:0"));
    assert_eq!(fact("mounts"), EXPORT_MOUNTS);
    for (name, access, kind, write, upper) in [
        ("dir", "ro", "directory", "refused", "directory"),
        ("inner", "rw", "directory", "allowed", "absent"),
        ("file", "ro", "file 4", "refused", "file 0"),
        ("notes", "rw", "file 5", "allowed", "file 0"),
        ("out", "rw", "directory", "allowed", "directory"),
    ] {
        let options = mount_options(fact(&format!("{name}.options")));
        for flag in [access, "nosuid", "nodev"] {
            assert!(options.contains(flag), "{name}: {options:?}");
        }
        let found = ["kind", "write", "upper"].map(|key| fact(&format!("{name}.{key}")));
        assert_eq!(found, [kind, write, upper], "{name}");
    }
    assert_eq!(fact("dir.entries"), "inner,marker");
    assert_eq!(fact("inner.entries"), "");
    assert_eq!(fact("file.read"), "file");
    assert_eq!(fact("notes.read"), "notes");
    assert_eq!(fact("out.entries"), "marker,private");
    for name in ["secret", "key"] {
        let hidden = ["options", "kind", "upper"].map(|key| fact(&format!("{name}.{key}")));
        assert_eq!(hidden, ["none", "denied", "absent"], "{name}");
    }
    let shown =
        ["options", "kind", "read", "write", "upper"].map(|key| fact(&format!("shown.{key}")));
    assert_eq!(shown, ["none", "file 5", "shown", "allowed", "absent"]);
    assert_eq!(fact("latched"), "no");

    // The clone's workload finds the same, as user ID 0: the files that the mappings show, but
    // not the hidden ones, and the writable mappings alone take what it writes.
    assert_eq!(
        clone.script(EXPORTED),
        [
            "read /srv/maps/dir/marker marker",
            "read /srv/maps/file.txt file",
            "read /srv/notes.txt notes",
            "read /srv/out/private/shown shown",
            "hidden /srv/maps/dir/secret refused",
            "hidden /srv/out/private/key refused",
            "listing /srv/maps/dir inner,marker,",
            "listing /srv/out marker,private,written,",
            "listing /srv/out/private shown,",
            "write /srv/maps/dir refused",
            "write /srv/maps/dir/inner written",
            "write /srv/out written",
            "append /srv/maps/file.txt refused",
            "append /srv/notes.txt written",
        ]
    );

    // The host's directory holds what the guest wrote through the writable mappings, and nothing
    // else changed, before the stop, which tears the root down, and after it.
    let mut expected = tree_of(&EXPORT_DIRECTORIES, &EXPORT_FILES);
    for (file, contents) in [
        ("dir/inner/written", "runtime-adapter"),
        ("dir/inner/workload", "workload"),
        ("notes.txt", "notes workload"),
        ("out/written", "runtime-adapter"),
        ("out/workload", "workload"),
    ] {
        expected.insert(file.to_owned(), Some(contents.to_owned()));
    }
    assert_eq!(export.tree(), expected);
    clone.stop();
    assert_eq!(export.tree(), expected);
    assert!(template.leases().is_empty());
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn clones_at_once_see_only_their_own_exports() {
    let template = Template::create("cl-exports");
    let own = Export::write(&template, "own", &[("marker", "own")], &[]);
    let common = Export::write(&template, "common", &[("marker", "common")], &[]);
    let writable = own.options(&[(".", "/srv/data")], &[]);
    let mut generated = writable.iter().map(String::as_str).collect::<Vec<_>>();
    generated.extend(["--machine-id", "generated"]);
    let read_only = common.options(&[(".", "/srv/data:ro")], &[]);
    let read_only = read_only.iter().map(String::as_str).collect::<Vec<_>>();
    // Four clones of one generation at once: one with a read-write export of its own and a
    // generated machine ID, two over the same read-only export, over a scratch image and in RAM,
    // and one without an export.
    let starts = [
        (
            "cl-own",
            StartOf {
                options: &generated,
                ..StartOf::default()
            },
        ),
        (
            "cl-common",
            StartOf {
                upper: Upper::Scratch,
                options: &read_only,
                ..StartOf::default()
            },
        ),
        (
            "cl-common-2",
            StartOf {
                options: &read_only,
                ..StartOf::default()
            },
        ),
        ("cl-none", StartOf::default()),
    ];
    let mut clones = starts.map(|(name, start)| template.start(name, start, 0));
    let [with_own, with_common, with_same, without] = &mut clones;
    for clone in [&*with_own, &*with_common, &*with_same] {
        clone.assert_mapped(1);
    }
    without.assert_started();

    // Each record names its clone's export, an aggregate of one directory, by the identity of its
    // plan, which the clones of the same export share, and one mapping; the clone without an
    // export has none, and no mapping, and NVX restored it with none of OpenVMM's options of an
    // export, which leaves the template's filesystem slot dormant. NVX attached each directory to
    // the slot as its aggregate's one child, with the access that its mapping needs.
    let exported = |clone: &Cloned| {
        let configuration = clone.configuration();
        let export = configuration
            .export
            .map(|export| (export.kind, export.plan));
        (export, configuration.mappings)
    };
    let (own_export, own_mappings) = exported(with_own);
    let (common_export, common_mappings) = exported(with_common);
    let (own_export, common_export) = (own_export.unwrap(), common_export.unwrap());
    assert_eq!((own_export.0, own_mappings), (ExportKind::Aggregate, 1));
    assert_eq!(
        (common_export.0, common_mappings),
        (ExportKind::Aggregate, 1)
    );
    assert_ne!(own_export.1, common_export.1);
    assert_eq!(exported(with_same), (Some(common_export), 1));
    assert_eq!(exported(without), (None, 0));
    with_own.assert_vmm_export(&own.attached(&[(".", "rw")]));
    with_common.assert_vmm_export(&common.attached(&[(".", "ro")]));
    with_same.assert_vmm_export(&common.attached(&[(".", "ro")]));
    without.assert_vmm_export(&[]);

    // Each clone with an export mounted it read-write, even where every mapping of its table is
    // read-only, as a cold launch's would not be, since OpenVMM keeps the export's access, and each
    // mapping carries its own. Below the root, beside its mapping, the mount table shows the bind
    // of a generated machine ID. Each shows its own directory, and takes what its access allows.
    for (clone, marker, access, write, mounts) in [
        (
            &mut *with_own,
            "own",
            "rw",
            "allowed",
            "/etc/machine-id,/srv/data",
        ),
        (&mut *with_common, "common", "ro", "refused", "/srv/data"),
        (&mut *with_same, "common", "ro", "refused", "/srv/data"),
    ] {
        let facts = clone.probe(&[MAP_DATA]);
        let fact = |key: &str| facts[&format!("map.{key}")].as_str();
        assert_eq!(fact("export.count"), "1", "{facts:?}");
        let options = mount_options(fact("export.options"));
        assert!(
            options.is_superset(&BTreeSet::from(["rw", "nosuid", "nodev"])),
            "{facts:?}"
        );
        assert_eq!(fact("mounts"), mounts, "{facts:?}");
        assert!(
            mount_options(fact("data.options")).contains(access),
            "{facts:?}"
        );
        let data = ["kind", "entries", "write"].map(|key| fact(&format!("data.{key}")));
        assert_eq!(data, ["directory", "marker", write], "{facts:?}");
        let size = format!("file {}", marker.len());
        let found = ["kind", "read"].map(|key| fact(&format!("marker.{key}")));
        assert_eq!(found, [size.as_str(), marker], "{facts:?}");
    }
    // The clone without an export mounted none, and maps nothing.
    let facts = without.probe(&[MAP_DATA]);
    let fact = |key: &str| facts[&format!("map.{key}")].as_str();
    assert_eq!(fact("export.count"), "0", "{facts:?}");
    assert!(!facts.contains_key("map.export.type"), "{facts:?}");
    assert_eq!(fact("hostfs.mode"), "absent", "{facts:?}");
    assert_eq!(fact("mounts"), "", "{facts:?}");
    let found = ["data.options", "data.kind", "marker.kind"].map(fact);
    assert_eq!(found, ["none", "absent", "absent"], "{facts:?}");

    // The read-only bind of the generated machine ID holds even against the guest's root: the
    // helper of the `entries` probe, which writes the root's own machine ID first, finds its file
    // system read-only.
    with_own
        .sandbox
        .run("probe", &[OsStr::new("entries")], FAILED);
    let stderr = with_own.sandbox.stderr();
    assert!(stderr.contains("Read-only file system"), "{stderr}");

    // The host's directories hold what the clone of the writable export wrote, and nothing else.
    let written = [("marker", "own"), ("written", "runtime-adapter")];
    assert_eq!(own.tree(), tree_of(&[], &written));
    assert_eq!(common.tree(), tree_of(&[], &[("marker", "common")]));
    for clone in &mut clones {
        clone.stop();
    }
    assert!(template.leases().is_empty());
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_start_refuses_an_export_whose_mapped_objects_changed_before_any_vmm_starts() {
    let template = Template::create("cl-changed");
    let files = [("data/sub/marker", "sub"), ("notes.txt", "notes")];
    let export = Export::write(&template, "changed", &files, &["data/sub"]);
    // A read-write directory, a directory inside it, at a target of its own, which the guest
    // could move, so that the export's plan pins it, and a read-only file.
    let shares = export.options(
        &[
            ("data", "/srv/data"),
            ("data/sub", "/srv/sub"),
            ("notes.txt", "/srv/notes.txt:ro"),
        ],
        &[],
    );
    let options = shares.iter().map(String::as_str).collect::<Vec<_>>();
    let (notes, sub) = (export.path("notes.txt"), export.path("data/sub"));
    let moved = |path: &Path| path.with_extension("moved");
    // Once NVX's plan checked the clone, a directory replaces the file, which would grant all of
    // itself, or another directory the one inside the read-write directory, which would map in
    // its place.
    let replace_file = || {
        fs::rename(&notes, moved(&notes)).unwrap();
        fs::create_dir(&notes).unwrap();
    };
    let replace_directory = || {
        fs::rename(&sub, moved(&sub)).unwrap();
        fs::create_dir(&sub).unwrap();
    };
    for (name, replace, replaced) in [
        ("cl-file", &replace_file as &dyn Fn(), &notes),
        ("cl-directory", &replace_directory, &sub),
    ] {
        let start = StartOf {
            options: &options,
            at_checked: Some(replace),
            ..StartOf::default()
        };
        let refused = template.start(name, start, CLONE_FAILED);

        // The clone's launch checked the export again, before it wrote anything, and refused it:
        // a failure of the request, by the evidence of its mappings, which quarantines nothing,
        // with no VM. The start released the lease, and nothing of the clone's launch remains.
        let printed = &refused.started;
        assert_eq!(
            failure(printed),
            [
                "request",
                "invalid-mappings",
                "launch",
                "planned",
                "none",
                "no-vm"
            ],
            "{printed:?}"
        );
        assert_eq!(printed["quarantined"], "no", "{printed:?}");
        assert!(!printed.contains_key("pid"), "{printed:?}");
        assert_eq!(printed["released"], printed["lease"], "{printed:?}");
        let stderr = refused.sandbox.stderr();
        assert!(
            stderr.contains("no longer names the host object"),
            "{stderr}"
        );
        let store = SandboxStore::open(&refused.sandbox.store).unwrap();
        assert_eq!(store.runtime().unwrap(), None);
        assert!(store.journal().unwrap().is_none());
        assert!(!refused.sandbox.store.join(LOG_NAME).exists());
        assert!(Lease::of(&store).unwrap().is_none());
        assert!(CloneRecords::read(&store).unwrap().is_none());

        // The scenario puts the original back for the next start.
        fs::remove_dir(replaced).unwrap();
        fs::rename(moved(replaced), replaced).unwrap();
    }
    assert!(template.leases().is_empty());
    let directories = ["data", "data/sub"];
    assert_eq!(export.tree(), tree_of(&directories, &files));
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_whose_mapping_fails_ends_before_its_sandbox_starts() {
    let template = Template::create("cl-mapfail");
    let export = Export::write(&template, "data", &[("out/marker", "marker")], &["out"]);
    // The table's second mapping, after that of the export's directory, puts the directory `out`
    // onto `/usr/lib/os-release`, a file of NVX's Ubuntu image, which the guest refuses once it
    // applied the first.
    let shares = export.options(
        &[(".", "/srv/maps:ro"), ("out", "/usr/lib/os-release")],
        &[],
    );
    let options = shares.iter().map(String::as_str).collect::<Vec<_>>();
    let start = StartOf {
        upper: Upper::Scratch,
        options: &options,
        ..StartOf::default()
    };
    let failed = template.start("cl-mapfail-1", start, CLONE_FAILED);
    let printed = &failed.started;

    // The clone was ready, but the restore sequence sent no start once its guest failed the
    // table: the failure is the start's, at the clone's Ready, by the evidence of the protocol,
    // since the guest's refusal does not tell the request's mappings from a failure of its own.
    // The clone order asked the guest to stop, which tears the root down, and NVX then ended the
    // launch at once, which found the VM exited, with the guest's success, or terminated it,
    // whether or not OpenVMM had reported the guest's success yet.
    assert!(printed.contains_key("pid"), "{printed:?}");
    assert_eq!(
        (printed["root"].as_str(), printed["build"].as_str()),
        ("ready", GUEST_BUILD)
    );
    for key in ["table", "mapped", "sandbox"] {
        assert!(!printed.contains_key(key), "{printed:?}");
    }
    let [outcome, evidence, stage, progress, status, teardown] = failure(printed);
    assert_eq!(
        [outcome, evidence, stage, progress],
        ["unknown", "protocol", "start", "ready"],
        "{printed:?}"
    );
    assert!(
        matches!(
            (status, teardown),
            ("0", "exited" | "terminated") | ("none", "terminated")
        ),
        "{printed:?}"
    );
    let stderr = failed.sandbox.stderr();
    // The guest applied the mapping of the export's directory, the table's first, and refused the
    // second: the guest's own report, which no other failure of the table prints.
    assert!(
        stderr.contains(
            "the clone did not apply its mapping table: the mapping table failed after 1 \
             mapping: "
        ),
        "{stderr}"
    );
    // Nothing of the clone runs: the failure quarantined nothing, and the start released the
    // lease, with the records of the clone's launch.
    assert_eq!(printed["quarantined"], "no", "{printed:?}");
    assert_eq!(printed["released"], printed["lease"], "{printed:?}");
    assert!(failed.sandbox.record().is_none());
    let store = SandboxStore::open(&failed.sandbox.store).unwrap();
    assert!(CloneRecords::read(&store).unwrap().is_none());
    assert!(template.leases().is_empty());
    // The guest wrote nothing to the host's directory.
    assert_eq!(
        export.tree(),
        tree_of(&["out"], &[("out/marker", "marker")])
    );
    assert_payload(&template, false);
}

/// Tries to exchange a line each way with a listener of a clone's guest through `address`, until
/// it did, or until `stop`, and returns whether it did: it reads the listener's greeting, answers
/// it, closes its side, and reads to the end. A connection that ends without the greeting, as
/// OpenVMM's do while the guest does not listen yet, or where it admits no connection of the host,
/// is retried. Each attempt to connect runs until it opens or fails: Windows retries a loopback
/// connection that nothing accepts every half second for about two seconds, so an attempt that a
/// timeout gave up on could still open once OpenVMM publishes the listener, which would then take
/// that connection, already closed, for its one client.
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

/// Sets its flag once it drops, so that a thread that waits for the flag ends even when the check
/// that the thread serves panics.
struct Stop<'a>(&'a AtomicBool);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Returns the facts that the [`MAP_RESOLVER`] probe found of a root's `/etc/resolv.conf`: the
/// mounts below the root, and the options of the topmost mount at the file, what it shows, and
/// whether a write of it is allowed.
fn resolver_facts(facts: &Printed) -> [&str; 4] {
    [
        "mounts",
        "resolver.options",
        "resolver.kind",
        "resolver.write",
    ]
    .map(|key| facts[&format!("map.{key}")].as_str())
}

/// Starts a clone in RAM of `template` in a new sandbox of the scenario `name`, with the start's
/// `options`, checks that the start exits with `status`, and returns the clone.
fn start_with(template: &Template, name: &str, options: &[String], status: i32) -> Cloned {
    let options = options.iter().map(String::as_str).collect::<Vec<_>>();
    let start = StartOf {
        options: &options,
        ..StartOf::default()
    };
    template.start(name, start, status)
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_template_without_a_nic_serves_clones_without_a_network() {
    let template = Template::create("cl-nonic");
    // A clone without a network, and one of a network that lets nothing through, which attaches
    // no NIC either, as the template's capture attached none: the template serves both.
    let deny = template.network_options(Network::Deny, GUEST_NETWORK);
    let mut clones = [("cl-none", Vec::new()), ("cl-deny", deny)].map(|(name, options)| {
        let cloned = start_with(&template, name, &options, 0);
        cloned.assert_started();
        cloned
    });

    // Neither has a network: no network in its record, and on Linux, no argument of a network of
    // its VMM; no network device, and the image's own `/etc/resolv.conf`, an empty file, which no
    // bind holds, and which root may write.
    for cloned in &mut clones {
        assert_eq!(cloned.configuration().network, None);
        cloned.assert_vmm_network(&[]);
        assert_eq!(
            cloned.script(NETWORK_VIEW),
            ["devices 0", "resolver 644 regular empty file"]
        );
        let facts = cloned.probe(&[MAP_RESOLVER]);
        assert_eq!(
            resolver_facts(&facts),
            ["", "none", "file 0", "allowed"],
            "{facts:?}"
        );
    }
    for cloned in &mut clones {
        cloned.stop();
    }

    // A clone of a network with a NIC finds no template there: NVX's plan refuses each before any
    // VMM starts.
    for network in Network::WITH_NIC {
        let options = template.network_options(network, GUEST_NETWORK);
        let refused = start_with(&template, "cl-r", &options, CLONE_FAILED);
        refused.assert_unserved(
            "the adapter's plan of a clone attaches a network, but the template's generation 1 \
             has no NIC",
        );
    }
    assert!(template.leases().is_empty());
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn one_template_serves_the_clones_of_every_network_policy_at_its_guest_s_address() {
    // The template's capture attached its NIC under the complete deny-all policy, whatever the
    // policy of the network that its creation named, which publishes the guest's listeners, and
    // forwards a port of host loopback.
    let template = Template::in_network("cl-pol", Network::Forward);
    let mut policies = Vec::new();
    for network in Network::WITH_NIC {
        // The restore of a clone of each network installs the network's policy on the template's
        // NIC, and the clone's record names the policy, the DNS server that OpenVMM serves the
        // guest under it, where it lets the guest reach the gateway's, the proxy, if any, and
        // whether it publishes the workload's listeners.
        let own = template.network_options(network, GUEST_NETWORK);
        let name = format!("cl-{}", network.name());
        let mut cloned = start_with(&template, &name, &own, 0);
        cloned.assert_started();
        let recorded = cloned
            .configuration()
            .network
            .expect("the record names the network");
        assert_eq!(
            (&recorded.dns, &recorded.proxy, recorded.publication),
            (&network.dns(), &network.proxy(), network.publication()),
            "{network:?}"
        );
        policies.push((network, recorded.policy));
        cloned.assert_vmm_network(&network.restored(GUEST_NETWORK));
        assert_eq!(cloned.script(NETWORK_VIEW)[0], "devices 1", "{network:?}");

        // The NIC lets through what the clone's policy allows alone: the clone reaches, at its
        // gateway, the services of the host's side that the policy lets it reach, and no other.
        cloned.assert_reaches(network.reaches());

        // The host reaches a listener of the clone's guest at its port of host loopback exactly
        // where the policy allows host-loopback access both ways, once the announcer that the
        // clone's guest started for the policy announced it, since the template's guest started
        // none, and through the policy's forward.
        let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, HOST_SIDE.listener));
        cloned.assert_listener(loopback, network.on_host_loopback());
        if network == Network::Forward {
            let forward = SocketAddr::from((Ipv4Addr::LOCALHOST, HOST_SIDE.forward));
            cloned.assert_listener(forward, true);
        }

        // While the policy publishes the guest's listeners, the guest refuses the capabilities
        // with which an execution could take the announcer's port or forge its datagrams.
        cloned.assert_publication(network.publication() == Publication::Published);
        cloned.stop();
    }

    // NVX's plan refuses, before any VMM starts, a clone of a network at another address of the
    // guest than the template's NIC's, of a network that lets nothing through, which attaches no
    // NIC, and one without a network.
    let elsewhere = format!(
        "the adapter's plan of a clone attaches a network at {OTHER_GUEST_NETWORK}, but the NIC \
         of the template's generation 1 is at {GUEST_NETWORK}: a template serves the clones of \
         its own guest's address alone"
    );
    let none = "the template's generation 1 has a NIC, which its clones restore with the network \
                of the adapter's plan, but the plan has none";
    let cases = [
        (
            Some((Network::Egress, OTHER_GUEST_NETWORK)),
            elsewhere.as_str(),
        ),
        (Some((Network::Deny, GUEST_NETWORK)), none),
        (None, none),
    ];
    for (requested, problem) in cases {
        let options = requested.map_or_else(Vec::new, |(requested, guest)| {
            template.network_options(requested, guest)
        });
        let refused = start_with(&template, "cl-r", &options, CLONE_FAILED);
        refused.assert_unserved(problem);
    }

    // Each clone's record names a policy of its own.
    for (index, (network, policy)) in policies.iter().enumerate() {
        for (other, other_policy) in &policies[index + 1..] {
            assert_ne!(policy, other_policy, "{network:?} and {other:?}");
        }
    }
    assert!(template.leases().is_empty());
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_forward_whose_port_another_socket_holds_fails_the_clone_as_the_host_s() {
    let template = Template::in_network("cl-busy", Network::Egress);

    // A socket of the host holds the port of host loopback that the forward of the clone's network
    // names, so that OpenVMM cannot bind it as it restores the clone's NIC.
    let busy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = busy.local_addr().unwrap().port();
    let mut form = Network::HostLoopback.form(GUEST_NETWORK);
    form["hostLoopbackForwards"] = json!([{
        "protocol": "tcp",
        "hostPort": port,
        "guestPort": HOST_SIDE.listener,
    }]);
    let network = template.root().join("network-busy.json");
    fs::write(&network, form.to_string()).unwrap();
    let options = ["--network".to_owned(), network.display().to_string()];

    // The clone's VMM exits before its restore-ready event, and NVX attributes the failure to the
    // host, by the evidence of the forward's bind, at the restore, which quarantines nothing. The
    // start's diagnostic names the forward, and the start released the lease, with the records of
    // the clone's launch.
    let failed = start_with(&template, "cl-b", &options, CLONE_FAILED);
    let printed = &failed.started;
    assert!(printed.contains_key("pid"), "{printed:?}");
    assert_eq!(
        failure(printed),
        [
            "host",
            "forward-bind",
            "restore",
            "spawned",
            "none",
            "exited"
        ],
        "{printed:?}"
    );
    assert_eq!(printed["quarantined"], "no", "{printed:?}");
    assert_eq!(printed["released"], printed["lease"], "{printed:?}");
    let stderr = failed.sandbox.stderr();
    let cause = format!("failed to create socket for port forward (Tcp 127.0.0.1:{port})");
    assert!(stderr.contains(&cause), "{cause:?} in {stderr}");
    assert!(failed.sandbox.record().is_none());
    let store = SandboxStore::open(&failed.sandbox.store).unwrap();
    assert!(CloneRecords::read(&store).unwrap().is_none());
    assert!(template.leases().is_empty());

    // Once the port is free, the template serves a clone of the same network.
    drop(busy);
    let mut cloned = start_with(&template, "cl-f", &options, 0);
    cloned.assert_started();
    cloned.stop();
    assert!(template.leases().is_empty());
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_binds_the_resolver_of_its_policy_s_dns_server_read_only() {
    // A template of a proxy-only network serves the clones of a network with egress allowed: its
    // capture attached its NIC under the complete deny-all policy, under which its guest learned
    // no DNS server.
    let template = Template::in_network("cl-dns", Network::Proxy);
    let own = template.network_options(Network::Egress, GUEST_NETWORK);
    let mut generated = own.clone();
    generated.extend(["--machine-id".to_owned(), "generated".to_owned()]);
    // Two clones at once: one with a generated machine ID, and one with the image's own.
    let mut clones = [
        ("cl-gen", generated, "/etc/machine-id,/etc/resolv.conf"),
        ("cl-img", own.clone(), "/etc/resolv.conf"),
    ]
    .map(|(name, options, mounts)| {
        let cloned = start_with(&template, name, &options, 0);
        cloned.assert_started();
        (cloned, mounts)
    });
    let resolver = format!("nameserver {GATEWAY}");
    for (cloned, mounts) in &mut clones {
        // The record names the gateway's DNS server, which OpenVMM serves the guest under the
        // clone's policy.
        let recorded = cloned
            .configuration()
            .network
            .expect("the record names the network");
        assert_eq!(recorded.dns, Network::Egress.dns());

        // The guest wrote a resolver file that names it, of mode 0444, which a read-only bind,
        // without set-user-ID programs or devices, holds at the root's `/etc/resolv.conf`, below
        // the root, after the bind of a generated machine ID: a write there, even as root, finds
        // its file system read-only, and the overlay's upper holds nothing there.
        let facts = cloned.probe(&[MAP_RESOLVER]);
        let fact = |key: &str| facts[&format!("map.{key}")].as_str();
        assert_eq!(fact("mounts"), *mounts, "{facts:?}");
        let options = mount_options(fact("resolver.options"));
        assert!(
            options.is_superset(&BTreeSet::from(["ro", "nosuid", "nodev"])),
            "{facts:?}"
        );
        let size = format!("file {}", resolver.len() + 1);
        let found = ["kind", "read", "write", "upper"].map(|key| fact(&format!("resolver.{key}")));
        assert_eq!(
            found,
            [size.as_str(), resolver.as_str(), "refused", "absent"],
            "{facts:?}"
        );
        assert_eq!(
            cloned.script(NETWORK_VIEW),
            ["devices 1", "resolver 444 regular file"]
        );

        // The guest reaches that server through the template's NIC: a TCP connection to its DNS
        // port opens, which the gateway's DNS proxy accepts itself, whatever the host's resolver
        // and its upstream servers do.
        assert_eq!(
            cloned.script(DNS_REACH),
            [format!("server {GATEWAY}"), "dns 0".to_owned()]
        );
    }
    for (cloned, _) in &mut clones {
        cloned.stop();
    }

    // A clone of a disk whose top layer holds a regular file `etc`, below which nothing can be
    // bound at `/etc/resolv.conf`, ends before it is ready, with the status of its network
    // settings, which attributes the failure to the request's invalid settings. It keeps the
    // image's own machine ID, which needs no bind.
    let disk = template.disk("etc.gpt", &ETC_FILE);
    let own = own.iter().map(String::as_str).collect::<Vec<_>>();
    let start = StartOf {
        image: Some(Image::Gpt(&ETC_FILE, &disk)),
        options: &own,
        ..StartOf::default()
    };
    let ended = template.start("cl-etc", start, CLONE_FAILED);
    ended.assert_unactivated(&template, "invalid-settings", status::ACTIVATION_NETWORK);
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_s_executions_find_the_proxy_of_its_policy() {
    // A template of a network with egress allowed serves the clones of a proxy-only network.
    let template = Template::in_network("cl-proxy", Network::Egress);
    let own = template.network_options(Network::Proxy, GUEST_NETWORK);
    let mut cloned = start_with(&template, "cl-p", &own, 0);
    cloned.assert_started();
    let url = Network::Proxy.proxy().expect("the network has a proxy");

    // The record names the proxy's URL, as the guest sees it, and no DNS server, since the policy
    // lets the guest reach nothing but the proxy.
    let recorded = cloned
        .configuration()
        .network
        .expect("the record names the network");
    assert_eq!(
        (recorded.dns, recorded.proxy.as_deref()),
        (None, Some(url.as_str()))
    );

    // An execution finds that URL in each of the proxy's variables, after its own entries, as
    // the host gives it from the clone's record, as it gives a launch's executions; one that names
    // its own proxy's settings keeps exactly its own environment.
    let proxies = PROXY_VARIABLES
        .map(|variable| format!("{variable}={url}\\n"))
        .concat();
    for (entry, expected) in [
        ("FIRST=1", format!("FIRST=1\\n{proxies}")),
        (OWN_PROXY, format!("{OWN_PROXY}\\n")),
    ] {
        let ran = cloned.exec(&["--identity", "0:0", "--env", entry, "--", "/usr/bin/env"]);
        let facts =
            ["launch", "status", "stdout", "stderr"].map(|key| &ran[&format!("exec.1.{key}")]);
        assert_eq!(
            facts,
            ["started", "exit 0", expected.as_str(), ""],
            "{ran:?}"
        );
    }

    // Without a DNS server, the guest binds no resolver: the clone, which has the template's NIC,
    // keeps the image's own `/etc/resolv.conf`.
    assert_eq!(
        cloned.script(NETWORK_VIEW),
        ["devices 1", "resolver 644 regular empty file"]
    );
    let facts = cloned.probe(&[MAP_RESOLVER]);
    assert_eq!(
        resolver_facts(&facts),
        ["", "none", "file 0", "allowed"],
        "{facts:?}"
    );
    cloned.stop();
    assert!(template.leases().is_empty());
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_of_a_template_with_a_nic_maps_its_export_beside_its_resolver() {
    let template = Template::in_network("cl-netexp", Network::Egress);
    let export = Export::write(&template, "data", &[("marker", "data")], &[]);
    let mut options = export.options(&[(".", "/srv/data")], &[]);
    options.extend(template.network_options(Network::Egress, GUEST_NETWORK));
    options.extend(["--machine-id".to_owned(), "generated".to_owned()]);
    let mut cloned = start_with(&template, "cl-ne", &options, 0);
    cloned.assert_mapped(1);

    // The record names the export, with its one mapping, and the network's DNS server. On Linux,
    // the restore attached the export's directory to the template's dormant filesystem slot, as
    // its aggregate's one child, and installed the clone's policy on the template's NIC.
    let configuration = cloned.configuration();
    let recorded = configuration.export.map(|export| export.kind);
    assert_eq!(
        (recorded, configuration.mappings),
        (Some(ExportKind::Aggregate), 1)
    );
    let dns = configuration.network.and_then(|network| network.dns);
    assert_eq!(dns, Network::Egress.dns());
    cloned.assert_vmm_export(&export.attached(&[(".", "rw")]));
    cloned.assert_vmm_network(&Network::Egress.restored(GUEST_NETWORK));

    // Below the root, the binds of the generated machine ID and of the resolver, which the
    // activation made, then the export's mapping, which came once the clone was ready.
    let probe = format!("{MAP_DATA} resolver=etc/resolv.conf");
    let facts = cloned.probe(&[&probe]);
    let fact = |key: &str| facts[&format!("map.{key}")].as_str();
    assert_eq!(
        fact("mounts"),
        "/etc/machine-id,/etc/resolv.conf,/srv/data",
        "{facts:?}"
    );
    assert_eq!(fact("resolver.read"), format!("nameserver {GATEWAY}"));
    let data = ["kind", "entries", "write"].map(|key| fact(&format!("data.{key}")));
    assert_eq!(data, ["directory", "marker", "allowed"], "{facts:?}");
    assert_eq!(fact("marker.read"), "data", "{facts:?}");
    cloned.stop();
    let written = [("marker", "data"), ("written", "runtime-adapter")];
    assert_eq!(export.tree(), tree_of(&[], &written));
    assert!(template.leases().is_empty());
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_run_in_a_template_of_a_network_finds_its_policy_s_proxy() {
    let template = Template::in_network("cl-runnet", Network::Proxy);
    let own = template.network_options(Network::Proxy, GUEST_NETWORK);
    let own = own.iter().map(String::as_str).collect::<Vec<_>>();
    let url = Network::Proxy.proxy().expect("the network has a proxy");
    let execution = [
        "--identity",
        "0:0",
        "--env",
        "FIRST=1",
        "--",
        "/usr/bin/env",
    ];

    // The run's execution finds the URL of the proxy of its clone's network in each of the proxy's
    // variables, after its own entries, as the executions of a started clone find it, from the
    // plan of the run's network, then the run tears the clone down.
    let start = StartOf {
        options: &own,
        ..StartOf::default()
    };
    let ran = template.run("cl-rn", start, &execution, 0);
    ran.assert_ran();
    let proxies = PROXY_VARIABLES
        .map(|variable| format!("{variable}={url}\\n"))
        .concat();
    let environment = format!("FIRST=1\\n{proxies}");
    assert_eq!(
        execution_facts(&ran.started),
        ["started", "exit 0", environment.as_str(), ""],
        "{:?}",
        ran.started
    );
    assert!(template.leases().is_empty());

    // A run of a network that lets nothing through, which attaches no NIC, finds no template
    // there: NVX's plan refuses its clone once the preflight staged the images, and the store
    // leased the generation, before any VMM starts, and the run runs nothing.
    let deny = template.network_options(Network::Deny, GUEST_NETWORK);
    let deny = deny.iter().map(String::as_str).collect::<Vec<_>>();
    let start = StartOf {
        options: &deny,
        ..StartOf::default()
    };
    let refused = template.run("cl-rn", start, &execution, CLONE_FAILED);
    let printed = &refused.started;
    assert_eq!(printed["preflight"], "staged", "{printed:?}");
    refused.assert_unserved(
        "the template's generation 1 has a NIC, which its clones restore with the network of the \
         adapter's plan, but the plan has none",
    );
    assert!(
        !printed
            .keys()
            .any(|key| key.starts_with("exec.") || key == "stopped"),
        "{printed:?}"
    );
    assert!(template.leases().is_empty());
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_run_s_execution_finds_its_shares_beneath_the_root() {
    let template = Template::create("cl-runexp");
    let export = Export::write(&template, "data", &[("marker", "data")], &[]);
    let shares = export.options(&[(".", "/srv/data")], &[]);
    let options = shares.iter().map(String::as_str).collect::<Vec<_>>();
    let start = StartOf {
        upper: Upper::Scratch,
        options: &options,
        ..StartOf::default()
    };

    // The execution, as user ID 0, reads the host's file through the share, which the restore
    // sequence mapped beneath the clone's root before the clone's sandbox started, and writes
    // another one through it.
    let script = "cat /srv/data/marker; printf run > /srv/data/written";
    let execution = ["--identity", "0:0", "--", "/bin/sh", "-c", script];
    let ran = template.run("cl-re", start, &execution, 0);
    ran.assert_ran_mapped(1);
    assert_eq!(
        execution_facts(&ran.started),
        ["started", "exit 0", "data", ""],
        "{:?}",
        ran.started
    );

    // Once the run tore the clone down, the host's directory holds what the execution wrote.
    let written = [("marker", "data"), ("written", "run")];
    assert_eq!(export.tree(), tree_of(&[], &written));
    assert!(template.leases().is_empty());
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_run_whose_clone_s_vm_exits_during_its_execution_ends_with_the_clone_s_failure() {
    let template = Template::create("cl-runlost");
    let export = Export::write(&template, "data", &[], &[]);
    let shares = export.options(&[(".", "/srv/data")], &[]);
    let options = shares.iter().map(String::as_str).collect::<Vec<_>>();
    let start = StartOf {
        upper: Upper::Scratch,
        options: &options,
        ..StartOf::default()
    };

    // The execution, as user ID 0, tells the host through the share that it runs, then waits,
    // while the clone's VMM ends without a word of its guest, as a crash of the VMM would end it.
    let script = "printf running > /srv/data/running; exec sleep 600";
    let execution = ["--identity", "0:0", "--", "/bin/sh", "-c", script];
    let (sandbox, mut running) = template.begin_in("cl-rl", "run", &start, &execution);
    let marker = export.path("running");
    assert_eq!(sandbox.paused(&mut running, &marker), "running");
    let vmm = sandbox.vmm().expect("the clone's VMM runs");
    vmm.kill().unwrap();
    assert!(
        vmm.wait(Instant::now() + Duration::from_secs(10)).unwrap(),
        "the clone's VMM still runs"
    );

    // The execution fails to run, and the run's stop stops nothing: the clone's VM ended without
    // its holder, with no report of the guest's own end, so the clone fails, as NVX attributes the
    // loss of its status, which quarantines nothing, and the run ends with the status of the
    // clone's failure, after the execution's diagnostic, once it released the lease.
    let ended = sandbox.finish(running, CLONE_FAILED);
    assert_eq!(ended["preflight"], "staged", "{ended:?}");
    assert_clone_started(&ended, 1);
    assert_eq!(ended["stopped"], "exited", "{ended:?}");
    assert_eq!(
        failure(&ended),
        [
            "unknown",
            "lost-status",
            "run",
            "committed",
            "none",
            "exited"
        ],
        "{ended:?}"
    );
    assert_eq!(ended["failure.code"], "exited");
    assert_eq!(ended["quarantined"], "no");
    assert_eq!(ended["released"], ended["lease"], "{ended:?}");
    assert!(
        !ended.keys().any(|key| key.starts_with("exec.")),
        "{ended:?}"
    );
    let stderr = sandbox.stderr();
    for diagnostic in [
        "the executions failed",
        "without an outcome report of the guest's own end",
    ] {
        assert!(stderr.contains(diagnostic), "{stderr}");
    }
    assert!(sandbox.record().is_none());
    let store = SandboxStore::open(&sandbox.store).unwrap();
    assert!(CloneRecords::read(&store).unwrap().is_none());
    assert!(template.leases().is_empty());
    assert!(template.store().tombstones().unwrap().is_empty());

    // The template still serves clones.
    let mut another = template.start("cl-rm", StartOf::default(), 0);
    another.assert_started();
    another.stop();
    assert_payload(&template, false);
}
