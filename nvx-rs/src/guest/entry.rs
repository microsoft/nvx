//! Entering an execution, for any guest init or agent: the namespaces, the view, the root, and
//! the identity in which a program runs, from the adapter's typed inputs.
//!
//! An [`Entry`] describes how an adapter's executions enter its root:
//!
//! - the root: the path of a mount, such as a [`Root`](super::root::Root)'s;
//! - the [`Identity`]: the user and group IDs, the supplementary groups, the [`Capabilities`],
//!   from an allowlist of 13, and the [`AccountMode`];
//! - the hostname, `nvx-sandbox` unless the adapter names another;
//! - the [`MachineId`] mode, the [`Publication`] state, the source of the time status, if any,
//!   the resolver file, if any, and the [`DeviceNode`]s of `/dev`.
//!
//! [`Entry::bind_runtime`] binds the runtime's files into the root once, for every execution
//! that enters it later, and returns the [`RuntimeBinds`], which a caller removes before it tears
//! the root down in the same mount namespace. [`Entry::prepare`] checks the inputs for one
//! execution, and reads and checks the account in existing-account mode; [`Command::enter`] then
//! has the child enter that execution, in launch stages of [`spawn`](super::spawn), in the order
//! of [`LaunchStage`]. Nothing here reads the kernel command line or a boot token, and nothing
//! creates an account.
//!
//! # The runtime's binds
//!
//! [`Entry::bind_runtime`] binds the file of a [generated](MachineId::Generated) machine ID over
//! the root's `/etc/machine-id`, the time status's directory at the root's `/run/nvx/time`, and
//! a [resolver](Entry::resolver) file, which names the guest's DNS servers, over the root's
//! `/etc/resolv.conf`, through the walk and attach of [`mappings::bind`]: each is read-only,
//! `nosuid`, and `nodev` before anything sees it, and the walk creates what the root lacks only
//! on the root's own mount, through no symbolic link, and refuses a target of another kind than
//! its source. A symbolic link at `/etc/machine-id` or `/etc/resolv.conf` itself, which no bind
//! can cover, is replaced with an empty file first. In [image](MachineId::Image) mode, the
//! image's own `/etc/machine-id` is left alone, and without a resolver file, so is the image's
//! own `/etc/resolv.conf`. The time status's directory must hold only what any execution may
//! use, since one of user ID 0 can open every file in it that user ID 0 owns, and hold its lock:
//! the guest's time tools keep their locks and private records outside it.
//!
//! The binds are made in the caller's mount namespace, of which each execution's starts as a
//! copy, so every execution that enters the root later sees them, and sees their sources as they
//! change, such as a machine ID that the guest rewrites in place after a restore. They last as
//! long as that namespace, unless [`RuntimeBinds::remove`] removes them, in reverse order. A
//! root's teardown detaches no busy mount, so a caller that tears the root down in the namespace
//! where it bound them removes them first. A caller whose root is torn down after it ends, such
//! as an agent that an init outlives, may instead bind them in a mount namespace of its own,
//! which ends with it, and drop them. A bind that fails has the call remove the binds that it
//! made before it, and an [`Error::Rollback`] names any of them that stays mounted. A bind that a
//! rename moved while the walk ran, and that the attach failed to detach, fails the call, and
//! stays mounted wherever it went, as [`mappings::bind`] describes.
//!
//! # The execution
//!
//! The child starts with `clone3` in new mount, PID, and UTS namespaces: it is PID 1 of its own
//! PID namespace, with no process between it and its caller, so that its end ends every process
//! of the namespace. It then:
//!
//! 1. leads a new session, without a controlling terminal;
//! 2. waits at its [barrier](Command::barrier), if its command has one, until its caller has
//!    placed it, then places its descriptors, and closes every other descriptor of its caller's
//!    before it waits or places them, so that no descriptor of the old root, of the caller's
//!    control session, or of another namespace or process reaches the program;
//! 3. joins its [cgroup](Command::cgroup), if its command names one;
//! 4. makes every mount of its mount namespace private, recursively, and sets its hostname;
//! 5. moves the root over `/`, after it makes every mount of the root's tree `nosuid` and
//!    `nodev`. The root hides the former root, which no path reaches from it, `..` from the top
//!    included, and the child never enters the root by `chroot` alone;
//! 6. mounts a procfs of its own PID namespace at `/proc`, `nosuid`, `nodev`, and `noexec`;
//! 7. mounts a read-only sysfs at `/sys`, `nosuid`, `nodev`, and `noexec`;
//! 8. mounts a tmpfs at `/dev`, `nosuid`, which holds only the allowlisted device nodes, the
//!    links `fd`, `stdin`, `stdout`, `stderr`, and `ptmx`, a devpts instance of its own at
//!    `/dev/pts`, `nosuid` and `noexec`, and a tmpfs at `/dev/shm`, `nosuid` and `nodev`. Every
//!    other mount of the execution is `nodev`;
//! 9. takes its [identity](Identity), and sets `no_new_privs`;
//! 10. closes every descriptor that its stages used or opened but its program's, and enters its
//!     working directory, `/` unless its command names another, with that identity, through no
//!     magic link, so that neither a `/proc` link to a descriptor, a working directory, or a root,
//!     nor a symbolic link to one, places it outside the root;
//! 11. gives every signal its default disposition, and blocks none, so that its program inherits
//!     no signal that its caller ignores or blocks, and runs its program, which a command that
//!     [searches a `PATH`](Command::search_path) looks for in the root.
//!
//! For user ID 0, `/proc/sys`, `/proc/sysrq-trigger`, `/proc/bus`, `/proc/fs`, and `/proc/irq`
//! are read-only, and `/proc/kcore`, `/proc/keys`, `/proc/timer_list`, `/proc/timer_stats`,
//! `/proc/latency_stats`, `/proc/sched_debug`, `/proc/acpi`, `/proc/asound`, `/proc/scsi`, and
//! `/sys/firmware` are masked by an empty, read-only file or directory; an entry that the kernel
//! lacks needs neither.
//!
//! The view creates what the root lacks of `/proc`, `/sys`, and `/dev` on the root's own mount,
//! through no symbolic link and inside no other mount, and attaches its mounts there; a symbolic
//! link or another mount on the way fails the stage. The guest's kernel has no user namespaces,
//! so the entry creates none.
//!
//! ```no_run
//! use std::os::unix::ffi::OsStrExt;
//!
//! use nvx::guest::entry::{Entry, Identity, MachineId};
//! use nvx::guest::spawn::Command;
//! use nvx::protocol::cmdline;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let machine_id = MachineId::generate("/run/nvx/workload-machine-id")?;
//! let mut entry = Entry::new("/run/nvx/rootfs", Identity::new(65534, 65534))
//!     .hostname("sandbox")
//!     .machine_id(machine_id)
//!     .time_status("/run/nvx/time");
//! // The guest's init wrote its resolver file from the DNS server that the command line names.
//! if cmdline::dns_server(&std::fs::read("/proc/cmdline")?)?.is_some() {
//!     entry = entry.resolver("/etc/resolv.conf");
//! }
//! // Once, before the first execution.
//! let binds = entry.bind_runtime()?;
//! let prepared = entry.prepare()?;
//! let home = prepared.account().map(|account| account.home().to_vec());
//! let mut spawned = Command::new(c"/bin/true".to_owned(), vec![c"true".to_owned()])
//!     .barrier()
//!     .enter(prepared)
//!     .spawn()?;
//! let barrier = spawned.take_barrier().expect("the command has a barrier");
//! // Place the child, by `spawned.id()`, in its cgroup; then:
//! barrier.release()?;
//! // Once the executions have ended, before the root's teardown in this mount namespace:
//! binds.remove()?;
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::mappings::{
    self, Access, Kernel, Kind, Mapped, Mapping, MappingError, SourceRoot, Step, SystemKernel,
    TargetRoot, TeardownError,
};
use super::parse::parse_decimal;
#[cfg(doc)]
use super::spawn::{Command, LaunchStage};
use super::sys::{self, EntryChild, Errno, FileDescriptor, Mode, OpenFlags};
#[cfg(doc)]
use crate::protocol::cmdline::write_resolver;

/// The hostname that an entry gives its execution unless the adapter names another.
pub const DEFAULT_HOSTNAME: &str = "nvx-sandbox";
/// The longest hostname, in bytes: Linux's `HOST_NAME_MAX`.
pub const MAX_HOSTNAME_BYTES: usize = 64;
/// The most supplementary groups that an identity holds: Linux's `NGROUPS_MAX`.
pub const MAX_GROUPS: usize = 65536;
/// The largest account database, `/etc/passwd` or `/etc/group`, that existing-account mode reads.
pub const MAX_ACCOUNT_DATABASE_BYTES: u64 = 1 << 20;
/// The path of the passwd database below the root.
const PASSWD: &std::ffi::CStr = c"etc/passwd";
/// The path of the group database below the root.
const GROUP: &std::ffi::CStr = c"etc/group";
/// The ID that `setresuid` and `setresgid` read as "unchanged", which no identity may hold.
const UNCHANGED_ID: u32 = u32::MAX;
/// The mode of a generated machine ID's file.
const MACHINE_ID_MODE: u32 = 0o444;
/// Where [`Entry::bind_runtime`] binds a generated machine ID in the root.
const MACHINE_ID_TARGET: &str = "/etc/machine-id";
/// Where [`Entry::bind_runtime`] binds the time status in the root.
const TIME_STATUS_TARGET: &str = "/run/nvx/time";
/// Where [`Entry::bind_runtime`] binds the resolver file in the root.
const RESOLVER_TARGET: &str = "/etc/resolv.conf";

/// The names of the Linux capabilities, by number.
const CAPABILITY_NAMES: [&str; 41] = [
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_KILL",
    "CAP_SETGID",
    "CAP_SETUID",
    "CAP_SETPCAP",
    "CAP_LINUX_IMMUTABLE",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_BROADCAST",
    "CAP_NET_ADMIN",
    "CAP_NET_RAW",
    "CAP_IPC_LOCK",
    "CAP_IPC_OWNER",
    "CAP_SYS_MODULE",
    "CAP_SYS_RAWIO",
    "CAP_SYS_CHROOT",
    "CAP_SYS_PTRACE",
    "CAP_SYS_PACCT",
    "CAP_SYS_ADMIN",
    "CAP_SYS_BOOT",
    "CAP_SYS_NICE",
    "CAP_SYS_RESOURCE",
    "CAP_SYS_TIME",
    "CAP_SYS_TTY_CONFIG",
    "CAP_MKNOD",
    "CAP_LEASE",
    "CAP_AUDIT_WRITE",
    "CAP_AUDIT_CONTROL",
    "CAP_SETFCAP",
    "CAP_MAC_OVERRIDE",
    "CAP_MAC_ADMIN",
    "CAP_SYSLOG",
    "CAP_WAKE_ALARM",
    "CAP_BLOCK_SUSPEND",
    "CAP_AUDIT_READ",
    "CAP_PERFMON",
    "CAP_BPF",
    "CAP_CHECKPOINT_RESTORE",
];

/// A Linux capability, by its number.
///
/// Any of the 64 that a capability set holds can be named, but an entry allows only those of
/// [`Capabilities::ALLOWED`], which have constants here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Capability(u8);

impl Capability {
    /// `CAP_CHOWN`: changes the owners and groups of files.
    pub const CHOWN: Self = Self(0);
    /// `CAP_DAC_OVERRIDE`: bypasses file read, write, and execute permission checks.
    pub const DAC_OVERRIDE: Self = Self(1);
    /// `CAP_FOWNER`: bypasses the checks that require the caller to own a file.
    pub const FOWNER: Self = Self(3);
    /// `CAP_FSETID`: keeps the set-user-ID and set-group-ID bits of a file that changes.
    pub const FSETID: Self = Self(4);
    /// `CAP_KILL`: signals processes of other users.
    pub const KILL: Self = Self(5);
    /// `CAP_SETGID`: changes group IDs and supplementary groups.
    pub const SETGID: Self = Self(6);
    /// `CAP_SETUID`: changes user IDs.
    pub const SETUID: Self = Self(7);
    /// `CAP_SETPCAP`: drops capabilities from the bounding set, and changes securebits.
    pub const SETPCAP: Self = Self(8);
    /// `CAP_NET_BIND_SERVICE`: binds ports below the unprivileged port start.
    pub const NET_BIND_SERVICE: Self = Self(10);
    /// `CAP_NET_RAW`: uses raw and packet sockets.
    pub const NET_RAW: Self = Self(13);
    /// `CAP_SYS_CHROOT`: changes the root directory.
    pub const SYS_CHROOT: Self = Self(18);
    /// `CAP_AUDIT_WRITE`: writes records to the kernel's audit log.
    pub const AUDIT_WRITE: Self = Self(29);
    /// `CAP_SETFCAP`: sets file capabilities.
    pub const SETFCAP: Self = Self(31);

    /// Returns the capability numbered `number`, or `None` above 63.
    pub const fn new(number: u8) -> Option<Self> {
        if number < 64 {
            Some(Self(number))
        } else {
            None
        }
    }

    /// Returns the capability's number.
    pub const fn number(self) -> u8 {
        self.0
    }

    /// Returns whether an entry allows the capability: whether
    /// [`Capabilities::ALLOWED`] holds it.
    pub const fn is_allowed(self) -> bool {
        Capabilities::ALLOWED.contains(self)
    }
}

impl fmt::Display for Capability {
    /// Writes the capability's name, such as `CAP_CHOWN`, or `capability N` for a number that
    /// Linux does not name.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match CAPABILITY_NAMES.get(usize::from(self.0)) {
            Some(name) => formatter.write_str(name),
            None => write!(formatter, "capability {}", self.0),
        }
    }
}

/// A set of capabilities.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Capabilities(u64);

impl Capabilities {
    /// No capability.
    pub const EMPTY: Self = Self(0);
    /// The 13 capabilities that an entry allows: `CHOWN`, `DAC_OVERRIDE`, `FOWNER`, `FSETID`,
    /// `KILL`, `SETGID`, `SETUID`, `SETPCAP`, `SETFCAP`, `NET_BIND_SERVICE`, `NET_RAW`,
    /// `SYS_CHROOT`, and `AUDIT_WRITE`.
    pub const ALLOWED: Self = Self::EMPTY
        .with(Capability::CHOWN)
        .with(Capability::DAC_OVERRIDE)
        .with(Capability::FOWNER)
        .with(Capability::FSETID)
        .with(Capability::KILL)
        .with(Capability::SETGID)
        .with(Capability::SETUID)
        .with(Capability::SETPCAP)
        .with(Capability::SETFCAP)
        .with(Capability::NET_BIND_SERVICE)
        .with(Capability::NET_RAW)
        .with(Capability::SYS_CHROOT)
        .with(Capability::AUDIT_WRITE);
    /// The capabilities that an entry refuses while listeners are
    /// [published](Publication::Published): `NET_BIND_SERVICE`, which binds the ports from which
    /// the guest accepts announcements, and `NET_RAW`, which forges packets from them.
    pub const PUBLISHED_REFUSED: Self = Self::EMPTY
        .with(Capability::NET_BIND_SERVICE)
        .with(Capability::NET_RAW);

    /// Returns this set and `capability`.
    pub const fn with(self, capability: Capability) -> Self {
        Self(self.0 | 1 << capability.0)
    }

    /// Returns this set without `capability`.
    pub const fn without(self, capability: Capability) -> Self {
        Self(self.0 & !(1 << capability.0))
    }

    /// Returns whether the set holds `capability`.
    pub const fn contains(self, capability: Capability) -> bool {
        self.0 & 1 << capability.0 != 0
    }

    /// Returns whether the set is empty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns the capabilities of the set, in the order of their numbers.
    pub fn iter(self) -> impl Iterator<Item = Capability> {
        (0..64)
            .map(Capability)
            .filter(move |capability| self.contains(*capability))
    }

    /// Returns the set's bits: bit `n` for capability `n`.
    pub const fn bits(self) -> u64 {
        self.0
    }
}

impl FromIterator<Capability> for Capabilities {
    fn from_iter<I: IntoIterator<Item = Capability>>(capabilities: I) -> Self {
        capabilities
            .into_iter()
            .fold(Self::EMPTY, |set, capability| set.with(capability))
    }
}

/// How an entry checks its identity's account.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AccountMode {
    /// The root's `/etc/passwd` holds exactly one entry for the user ID, whose primary group is
    /// the identity's group ID, which `/etc/group` holds, and whose home is absolute. Each
    /// database is read below the root, through no symbolic link, and refused unless it is a
    /// regular file of at most [`MAX_ACCOUNT_DATABASE_BYTES`].
    #[default]
    Existing,
    /// The IDs are used as given, without an account.
    Numeric,
}

/// The identity of an execution: its IDs, its capabilities, and how its account is checked.
///
/// An identity with user ID 0 keeps exactly its [`Capabilities`], from the allowlist, in its
/// bounding, permitted, and effective sets, so that its program keeps them too, and nothing
/// else. Any other identity gets none: its bounding, permitted, and effective sets are empty.
/// Every identity has empty inheritable and ambient sets and `no_new_privs`, so no program that
/// it runs gains a privilege, set-user-ID ones included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The user ID.
    uid: u32,
    /// The group ID.
    gid: u32,
    /// The supplementary groups.
    groups: Vec<u32>,
    /// The capabilities, for user ID 0.
    capabilities: Capabilities,
    /// How the account is checked.
    account: AccountMode,
}

impl Identity {
    /// Returns the identity of user ID `uid` and group ID `gid`, without supplementary groups or
    /// capabilities, whose account an entry checks in [existing](AccountMode::Existing) mode.
    pub fn new(uid: u32, gid: u32) -> Self {
        Self {
            uid,
            gid,
            groups: Vec::new(),
            capabilities: Capabilities::EMPTY,
            account: AccountMode::Existing,
        }
    }

    /// Gives the identity the supplementary groups `groups`.
    pub fn groups(mut self, groups: impl Into<Vec<u32>>) -> Self {
        self.groups = groups.into();
        self
    }

    /// Gives the identity the capabilities `capabilities`, which only user ID 0 may hold.
    pub fn capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Checks the identity's account in `mode`.
    pub fn account(mut self, mode: AccountMode) -> Self {
        self.account = mode;
        self
    }

    /// Returns the user ID.
    pub fn uid(&self) -> u32 {
        self.uid
    }

    /// Returns the group ID.
    pub fn gid(&self) -> u32 {
        self.gid
    }

    /// Returns the supplementary groups.
    pub fn supplementary_groups(&self) -> &[u32] {
        &self.groups
    }

    /// Returns the capabilities.
    pub fn capability_set(&self) -> Capabilities {
        self.capabilities
    }

    /// Returns how the account is checked.
    pub fn account_mode(&self) -> AccountMode {
        self.account
    }
}

/// The machine ID that an execution sees at `/etc/machine-id`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum MachineId {
    /// The image's own `/etc/machine-id`, which the entry leaves untouched.
    #[default]
    Image,
    /// The ID that the regular file at this path holds, such as one that [`MachineId::generate`]
    /// wrote once for the boot, which [`Entry::bind_runtime`] binds read-only over
    /// `/etc/machine-id`. The bind shows the file as it is, so an ID that the file's owner
    /// rewrites in place, as after a restore, reaches every execution.
    Generated(PathBuf),
}

impl MachineId {
    /// Generates a machine ID from the kernel's random number generator, writes it to a new file
    /// at `path`, with mode 0444 whatever the process's umask, and returns the mode that binds
    /// it.
    ///
    /// The ID is a random version 4 UUID, written as `/etc/machine-id` holds one: 32 lowercase
    /// hexadecimal digits and a newline. A file, or a symbolic link, at `path` already fails the
    /// call with `AlreadyExists`, so each boot generates its ID once.
    pub fn generate(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let mut id = [0u8; 16];
        getrandom::fill(&mut id).map_err(|error| {
            error
                .raw_os_error()
                .map_or_else(|| io::Error::other(error), io::Error::from_raw_os_error)
        })?;
        let mode = Mode::from_bits_retain(MACHINE_ID_MODE);
        let file = sys::open(
            &sys::path_c_string(&path)?,
            OpenFlags::O_WRONLY
                | OpenFlags::O_CREAT
                | OpenFlags::O_EXCL
                | OpenFlags::O_NOFOLLOW
                | OpenFlags::O_CLOEXEC,
            mode,
        )?;
        // The umask narrows the mode that creates the file, which could keep executions of
        // other users from reading the ID.
        file.set_mode(mode)?;
        file.write_all(&machine_id_text(id))?;
        file.close()?;
        Ok(Self::Generated(path))
    }
}

/// Returns the text of the machine ID made of `id`'s random bits, as a version 4 UUID: 32
/// lowercase hexadecimal digits and a newline.
fn machine_id_text(mut id: [u8; 16]) -> [u8; 33] {
    /// The lowercase hexadecimal digits.
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    id[6] = (id[6] & 0x0f) | 0x40;
    id[8] = (id[8] & 0x3f) | 0x80;
    let mut text = [b'\n'; 33];
    for (index, byte) in id.iter().enumerate() {
        text[2 * index] = DIGITS[usize::from(byte >> 4)];
        text[2 * index + 1] = DIGITS[usize::from(byte & 0x0f)];
    }
    text
}

/// Whether an execution's listeners are published, through private-network ingress or the
/// host's loopback, which decides whether the entry allows the capabilities of
/// [`Capabilities::PUBLISHED_REFUSED`].
///
/// The runtime owns `net.ipv4.ip_unprivileged_port_start`, which no execution can lower: its
/// `/proc/sys` is read-only for user ID 0, and no other user may write it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Publication {
    /// Listeners are published, so the entry refuses `NET_BIND_SERVICE` and `NET_RAW`.
    #[default]
    Published,
    /// No listener is published.
    Unpublished,
}

/// A device node that an execution's `/dev` may hold, which the character device of the same
/// name backs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DeviceNode {
    /// `/dev/null`.
    Null,
    /// `/dev/zero`.
    Zero,
    /// `/dev/full`.
    Full,
    /// `/dev/random`.
    Random,
    /// `/dev/urandom`.
    Urandom,
    /// `/dev/tty`, the controlling terminal, of which the execution has none.
    Tty,
}

impl DeviceNode {
    /// Every device node, which an execution's `/dev` holds by default.
    pub const ALL: [Self; 6] = [
        Self::Null,
        Self::Zero,
        Self::Full,
        Self::Random,
        Self::Urandom,
        Self::Tty,
    ];

    /// Returns the node's bit in the child's device set, in the order of the entry's device
    /// table.
    fn bit(self) -> u8 {
        1 << match self {
            Self::Null => 0,
            Self::Zero => 1,
            Self::Full => 2,
            Self::Random => 3,
            Self::Urandom => 4,
            Self::Tty => 5,
        }
    }
}

/// How an adapter's executions enter its root: see the [module](self) documentation.
///
/// An entry holds only the adapter's inputs, so that the adapter builds it once, binds its
/// runtime's files once, and [prepares](Self::prepare) it for each execution.
#[derive(Clone, Debug)]
pub struct Entry {
    /// The root: the path of a mount.
    root: PathBuf,
    /// The identity.
    identity: Identity,
    /// The hostname.
    hostname: Vec<u8>,
    /// The machine ID mode.
    machine_id: MachineId,
    /// Whether listeners are published.
    publication: Publication,
    /// The directory of the time status, if the executions see one.
    time_status: Option<PathBuf>,
    /// The resolver file, if the executions see one.
    resolver: Option<PathBuf>,
    /// The device nodes of `/dev`, as [`DeviceNode::bit`]s.
    devices: u8,
}

impl Entry {
    /// Describes executions that enter the mount at `root`, an absolute path, as `identity`,
    /// with the hostname `nvx-sandbox`, the image's machine ID, published listeners, no time
    /// status, the image's resolver file, and every [`DeviceNode`].
    pub fn new(root: impl Into<PathBuf>, identity: Identity) -> Self {
        Self {
            root: root.into(),
            identity,
            hostname: DEFAULT_HOSTNAME.as_bytes().to_vec(),
            machine_id: MachineId::Image,
            publication: Publication::Published,
            time_status: None,
            resolver: None,
            devices: DeviceNode::ALL
                .iter()
                .fold(0, |devices, node| devices | node.bit()),
        }
    }

    /// Gives the executions the hostname `hostname`: 1 to 64 bytes, without a NUL.
    pub fn hostname(mut self, hostname: impl Into<Vec<u8>>) -> Self {
        self.hostname = hostname.into();
        self
    }

    /// Gives the executions the machine ID of `mode`: a generated one once
    /// [`bind_runtime`](Self::bind_runtime) has bound it.
    pub fn machine_id(mut self, mode: MachineId) -> Self {
        self.machine_id = mode;
        self
    }

    /// Tells whether the executions' listeners are published.
    pub fn publication(mut self, publication: Publication) -> Self {
        self.publication = publication;
        self
    }

    /// Has the executions see the directory `source`, which holds the guest's time status,
    /// read-only at `/run/nvx/time`, once [`bind_runtime`](Self::bind_runtime) has bound it.
    ///
    /// The directory must hold only what any execution may use: one of user ID 0 can open, and
    /// lock, every file in it that user ID 0 owns. The guest's time tools publish only their
    /// state and their daemon's process ID in `/run/nvx/time`, and keep their locks and private
    /// records outside it.
    pub fn time_status(mut self, source: impl Into<PathBuf>) -> Self {
        self.time_status = Some(source.into());
        self
    }

    /// Has the executions see the regular file `source`, a resolver file in the format of
    /// `/etc/resolv.conf` that names the guest's DNS servers, read-only at `/etc/resolv.conf`,
    /// once [`bind_runtime`](Self::bind_runtime) has bound it. Without one, they see the image's
    /// own `/etc/resolv.conf`.
    ///
    /// The file may be the one that the guest's init wrote from the `virtnet_dns=` token, or one
    /// that [`write_resolver`] wrote. Every execution must be able to read it. The bind shows the
    /// file as it is, so what its owner rewrites in place reaches every execution.
    pub fn resolver(mut self, source: impl Into<PathBuf>) -> Self {
        self.resolver = Some(source.into());
        self
    }

    /// Gives the executions' `/dev` exactly the device nodes `devices`.
    pub fn devices(mut self, devices: impl IntoIterator<Item = DeviceNode>) -> Self {
        self.devices = devices
            .into_iter()
            .fold(0, |devices, node| devices | node.bit());
        self
    }

    /// Returns the identity.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Checks the entry's inputs, without reading anything: the root's path, the IDs, the
    /// groups, the capabilities, and the hostname.
    ///
    /// The root's path must be absolute and hold no NUL, and no ID may be `u32::MAX`, which the
    /// kernel reads as "unchanged". An identity holds at most [`MAX_GROUPS`] supplementary
    /// groups. A capability outside [`Capabilities::ALLOWED`] is refused, as is any capability
    /// of an identity whose user ID is not 0, and, while listeners are
    /// [published](Publication::Published), `NET_BIND_SERVICE` and `NET_RAW`.
    pub fn check(&self) -> Result<(), Error> {
        let root = self.root.as_os_str().as_bytes();
        if root.first() != Some(&b'/') || root.contains(&0) {
            return Err(Error::RootPath);
        }
        let identity = &self.identity;
        if identity.uid == UNCHANGED_ID
            || identity.gid == UNCHANGED_ID
            || identity.groups.contains(&UNCHANGED_ID)
        {
            return Err(Error::InvalidId);
        }
        if identity.groups.len() > MAX_GROUPS {
            return Err(Error::Groups);
        }
        if let Some(capability) = identity
            .capabilities
            .iter()
            .find(|capability| !capability.is_allowed())
        {
            return Err(Error::Capability(capability));
        }
        if identity.uid != 0 && !identity.capabilities.is_empty() {
            return Err(Error::NonRootCapabilities);
        }
        if self.publication == Publication::Published
            && let Some(capability) = identity
                .capabilities
                .iter()
                .find(|capability| Capabilities::PUBLISHED_REFUSED.contains(*capability))
        {
            return Err(Error::Published(capability));
        }
        if self.hostname.is_empty()
            || self.hostname.len() > MAX_HOSTNAME_BYTES
            || self.hostname.contains(&0)
        {
            return Err(Error::Hostname);
        }
        Ok(())
    }

    /// Prepares one execution: [checks](Self::check) the inputs, opens the root, and reads and
    /// checks the account in [existing](AccountMode::Existing) mode, for [`Command::enter`].
    ///
    /// The execution sees the runtime's binds that [`bind_runtime`](Self::bind_runtime) made in
    /// the caller's mount namespace before it starts.
    pub fn prepare(&self) -> Result<Prepared, Error> {
        self.check()?;
        let root = sys::path_c_string(&self.root).map_err(|_| Error::RootPath)?;
        let directory = sys::open(
            &root,
            OpenFlags::O_PATH | OpenFlags::O_DIRECTORY | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| Error::Root(error.into()))?;
        let account = match self.identity.account {
            AccountMode::Existing => {
                Some(existing_account(&directory, &self.identity).map_err(Error::Account)?)
            }
            AccountMode::Numeric => None,
        };
        Ok(Prepared {
            child: EntryChild {
                root,
                hostname: self.hostname.clone().into_boxed_slice(),
                devices: self.devices,
                uid: self.identity.uid,
                gid: self.identity.gid,
                groups: self.identity.groups.clone().into_boxed_slice(),
                capabilities: self.identity.capabilities.bits(),
                #[cfg(test)]
                simulated: None,
            },
            account,
        })
    }

    /// Binds the runtime's files into the root, in the caller's mount namespace, for every
    /// execution that enters the root later, as the [module](self) describes, and returns the
    /// binds that it made: the file of a [generated](MachineId::Generated) machine ID over
    /// `/etc/machine-id`, then the [time status](Self::time_status) at `/run/nvx/time`, then the
    /// [resolver](Self::resolver) file over `/etc/resolv.conf`. An entry with none of them binds
    /// nothing.
    ///
    /// The binds last as long as the caller's mount namespace, unless [`RuntimeBinds::remove`]
    /// removes them, as a caller that tears the root down in that namespace does first. If a bind
    /// fails, the call removes the binds that it made before it, in reverse order, and returns
    /// the failure, or, if any of them stays mounted, an [`Error::Rollback`] that names them too.
    ///
    /// Each call binds again, over the earlier binds, so an adapter calls it once, before its
    /// first execution. It needs the privilege to mount, and Linux 6.8 or later.
    pub fn bind_runtime(&self) -> Result<RuntimeBinds, Error> {
        self.bind_runtime_with(&mut SystemKernel)
    }

    /// Implements [`bind_runtime`](Self::bind_runtime) with `kernel`.
    pub(crate) fn bind_runtime_with(&self, kernel: &mut dyn Kernel) -> Result<RuntimeBinds, Error> {
        self.check()?;
        if self.machine_id == MachineId::Image
            && self.time_status.is_none()
            && self.resolver.is_none()
        {
            return Ok(RuntimeBinds {
                root: None,
                binds: Vec::new(),
            });
        }
        let root = TargetRoot::open_with(kernel, &self.root).map_err(Error::Root)?;
        let mut binds = Vec::new();
        if let Err(error) = self.bind_each(kernel, &root, &mut binds) {
            // The caller cannot remove the binds before the failure, which a teardown in this
            // namespace would trip over, so it learns of any that stays.
            return Err(match mappings::unbind_with(kernel, &root, &binds) {
                Ok(()) => error,
                Err(teardown) => Error::Rollback {
                    error: Box::new(error),
                    teardown,
                },
            });
        }
        Ok(RuntimeBinds {
            root: Some(root),
            binds,
        })
    }

    /// Makes the binds of [`bind_runtime`](Self::bind_runtime) beneath `root`, the root's mount,
    /// in order, with `kernel`, and adds each to `binds`, until one fails.
    fn bind_each(
        &self,
        kernel: &mut dyn Kernel,
        root: &TargetRoot,
        binds: &mut Vec<Mapped>,
    ) -> Result<(), Error> {
        if let MachineId::Generated(source) = &self.machine_id {
            let bound = bind_file(kernel, root, &self.root, source, MACHINE_ID_TARGET);
            binds.push(bound.map_err(Error::MachineId)?);
        }
        if let Some(source) = &self.time_status {
            let bound =
                bind_runtime_file(kernel, root, source, TIME_STATUS_TARGET, Kind::Directory);
            binds.push(bound.map_err(Error::TimeStatus)?);
        }
        if let Some(source) = &self.resolver {
            let bound = bind_file(kernel, root, &self.root, source, RESOLVER_TARGET);
            binds.push(bound.map_err(Error::Resolver)?);
        }
        Ok(())
    }
}

/// The runtime's binds that [`Entry::bind_runtime`] made in the caller's mount namespace, in the
/// order in which it made them.
///
/// A caller that tears the root down in that namespace [removes](Self::remove) them first: a
/// teardown detaches no busy mount, and the root's mount is busy while they are on it. A caller
/// whose namespace ends before the teardown, such as an agent that an init outlives, may drop
/// them instead, which leaves them mounted. Until they are removed or dropped, they hold a
/// descriptor of the root, which keeps its mount busy too.
#[derive(Debug)]
pub struct RuntimeBinds {
    /// The root beneath which the binds were made, unless there are none.
    root: Option<TargetRoot>,
    /// The binds, in the order in which they were made.
    binds: Vec<Mapped>,
}

impl RuntimeBinds {
    /// Removes the binds, in reverse order: unmounts each that is still the topmost mount at its
    /// target, by its name in its parent directory, without detaching a busy one, which needs
    /// procfs at `/proc`. It attempts each, whatever failed before, and reports every one that it
    /// failed to unmount, which stays mounted.
    pub fn remove(self) -> Result<(), TeardownError> {
        self.remove_with(&mut SystemKernel)
    }

    /// Implements [`remove`](Self::remove) with `kernel`.
    pub(crate) fn remove_with(self, kernel: &mut dyn Kernel) -> Result<(), TeardownError> {
        match &self.root {
            Some(root) => mappings::unbind_with(kernel, root, &self.binds),
            None => Ok(()),
        }
    }
}

/// Binds the regular file at `source` read-only over `target`, a file of the root's `/etc`,
/// beneath `root`, the mount at `path`, with `kernel`, and returns the bind. A symbolic link at
/// `target` itself, which no bind can cover, is first replaced with an empty file, which the bind
/// creates.
fn bind_file(
    kernel: &mut dyn Kernel,
    root: &TargetRoot,
    path: &Path,
    source: &Path,
    target: &str,
) -> Result<Mapped, MappingError> {
    match bind_runtime_file(kernel, root, source, target, Kind::File) {
        Err(MappingError::Link { path: link }) if link == Path::new(target) => {
            remove_link(path, target).map_err(|_| MappingError::Link { path: link })?;
            bind_runtime_file(kernel, root, source, target, Kind::File)
        }
        bound => bound,
    }
}

/// Binds the file or directory at `path`, which must be of `kind`, read-only at `target` beneath
/// `root`, with `kernel`, through the walk and attach of [`mappings::bind`] from a source root at
/// `path`'s parent directory, and returns the bind.
fn bind_runtime_file(
    kernel: &mut dyn Kernel,
    root: &TargetRoot,
    path: &Path,
    target: &str,
    kind: Kind,
) -> Result<Mapped, MappingError> {
    let source_error = |error: i32| MappingError::System {
        step: Step::Source,
        error,
    };
    let Some((parent, name)) = runtime_source(path) else {
        return Err(source_error(Errno::EINVAL as i32));
    };
    let source = SourceRoot::open(parent)
        .map_err(|error| source_error(error.raw_os_error().unwrap_or(Errno::EIO as i32)))?;
    let mapping = Mapping::new(name, target, Access::ReadOnly)
        .map_err(|_| source_error(Errno::EINVAL as i32))?
        .with_kind(kind);
    mappings::bind_with(kernel, root, &source, &mapping)
}

/// Returns the directory that holds the runtime file at `path`, the current directory for a
/// relative path of one component, and the file's name in it, unless `path` names no file.
fn runtime_source(path: &Path) -> Option<(&Path, &std::ffi::OsStr)> {
    let name = path.file_name()?;
    let parent = path.parent()?;
    if parent.as_os_str().is_empty() {
        Some((Path::new("."), name))
    } else {
        Some((parent, name))
    }
}

/// Removes the symbolic link at `target`, a file of `/etc` such as `/etc/machine-id`, beneath
/// `root`, reached through no other link and on the root's own mount.
fn remove_link(root: &Path, target: &str) -> Result<(), Errno> {
    let name = target.strip_prefix("/etc/").ok_or(Errno::EINVAL)?;
    let name = sys::c_string(name.as_bytes())?;
    let root = sys::path_c_string(root)?;
    let directory = sys::open(
        &root,
        OpenFlags::O_PATH | OpenFlags::O_DIRECTORY | OpenFlags::O_NOFOLLOW | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )?;
    let etc = sys::open_beneath(
        &directory,
        c"etc",
        OpenFlags::O_PATH | OpenFlags::O_DIRECTORY,
        Mode::empty(),
        false,
    )?;
    let status = nix::sys::stat::fstatat(
        etc.borrowed(),
        name.as_c_str(),
        nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
    )?;
    if status.st_mode & nix::libc::S_IFMT != nix::libc::S_IFLNK {
        return Err(Errno::EEXIST);
    }
    nix::unistd::unlinkat(
        etc.borrowed(),
        name.as_c_str(),
        nix::unistd::UnlinkatFlags::NoRemoveDir,
    )
}

/// One execution that an [`Entry`] prepared, which [`Command::enter`] takes.
#[derive(Debug)]
pub struct Prepared {
    /// What the child needs.
    child: EntryChild,
    /// The account that existing-account mode found.
    account: Option<Account>,
}

impl Prepared {
    /// Returns the identity's account, in [existing](AccountMode::Existing) mode.
    pub fn account(&self) -> Option<&Account> {
        self.account.as_ref()
    }

    /// Returns what the child needs.
    pub(crate) fn into_child(self) -> EntryChild {
        self.child
    }

    /// Returns an execution for tests without privileges, whose child starts without new
    /// namespaces and skips the system calls of every entry stage, so that it keeps the test's
    /// own root and identity, except that the stage of `failure`, if any, fails with its error.
    #[cfg(test)]
    pub(crate) fn simulated(failure: Option<(super::spawn::LaunchStage, Errno)>) -> Self {
        Self {
            child: EntryChild {
                root: c"/".to_owned(),
                hostname: DEFAULT_HOSTNAME.as_bytes().into(),
                simulated: Some(match failure {
                    Some((stage, error)) => sys::Simulation::Fail(stage, error),
                    None => sys::Simulation::Skip,
                }),
                ..EntryChild::default()
            },
            account: None,
        }
    }
}

/// The account of an identity, as its passwd entry names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    /// The user name.
    name: Vec<u8>,
    /// The home directory.
    home: Vec<u8>,
}

impl Account {
    /// Returns the user name, as bytes, since passwd entries need not be UTF-8.
    pub fn name(&self) -> &[u8] {
        &self.name
    }

    /// Returns the home directory, an absolute path.
    pub fn home(&self) -> &[u8] {
        &self.home
    }
}

/// An account database of existing-account mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Database {
    /// `/etc/passwd`.
    Passwd,
    /// `/etc/group`.
    Group,
}

impl fmt::Display for Database {
    /// Writes the database's path in the root.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Passwd => "/etc/passwd",
            Self::Group => "/etc/group",
        })
    }
}

/// Why existing-account mode refused an identity.
#[derive(Debug)]
#[non_exhaustive]
pub enum AccountError {
    /// The database cannot be opened or read below the root, which a symbolic link on its path
    /// fails too.
    Read(Database, io::Error),
    /// The database is not a regular file.
    NotFile(Database),
    /// The database is larger than [`MAX_ACCOUNT_DATABASE_BYTES`].
    TooLarge(Database),
    /// No passwd entry has the user ID.
    Missing,
    /// Several passwd entries have the user ID.
    Duplicate,
    /// The passwd entry of the user ID lacks a field, or a name.
    Malformed,
    /// The passwd entry's primary group, this group ID, is not the identity's.
    PrimaryGroup(u32),
    /// No group entry has the group ID.
    Group,
    /// The passwd entry's home is not absolute.
    Home,
}

impl fmt::Display for AccountError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(database, error) => write!(formatter, "cannot read {database}: {error}"),
            Self::NotFile(database) => write!(formatter, "{database} is not a regular file"),
            Self::TooLarge(database) => write!(
                formatter,
                "{database} is larger than {MAX_ACCOUNT_DATABASE_BYTES} bytes"
            ),
            Self::Missing => formatter.write_str("no passwd entry has the user ID"),
            Self::Duplicate => formatter.write_str("several passwd entries have the user ID"),
            Self::Malformed => formatter.write_str("the passwd entry of the user ID is malformed"),
            Self::PrimaryGroup(gid) => write!(
                formatter,
                "the passwd entry's primary group is {gid}, not the identity's"
            ),
            Self::Group => formatter.write_str("no group entry has the group ID"),
            Self::Home => formatter.write_str("the passwd entry's home is not absolute"),
        }
    }
}

impl std::error::Error for AccountError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(_, error) => Some(error),
            _ => None,
        }
    }
}

/// Why an entry refused to prepare an execution.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The root's path is not absolute, or holds a NUL.
    RootPath,
    /// The root cannot be opened as a directory, or, for [`Entry::bind_runtime`], is not the root
    /// of a mount.
    Root(io::Error),
    /// The user ID, the group ID, or a supplementary group is `u32::MAX`, which the kernel reads
    /// as "unchanged".
    InvalidId,
    /// The identity has more than [`MAX_GROUPS`] supplementary groups.
    Groups,
    /// The capability is outside [`Capabilities::ALLOWED`].
    Capability(Capability),
    /// The identity's user ID is not 0, and it has capabilities.
    NonRootCapabilities,
    /// The capability is refused while listeners are published.
    Published(Capability),
    /// The hostname is empty, longer than [`MAX_HOSTNAME_BYTES`], or holds a NUL.
    Hostname,
    /// Existing-account mode refused the identity.
    Account(AccountError),
    /// [`Entry::bind_runtime`] cannot bind the machine ID's file over the root's
    /// `/etc/machine-id`: it is missing or not a regular file, the target cannot hold it, or the
    /// caller cannot mount.
    MachineId(MappingError),
    /// [`Entry::bind_runtime`] cannot bind the time status's directory at the root's
    /// `/run/nvx/time`.
    TimeStatus(MappingError),
    /// [`Entry::bind_runtime`] cannot bind the resolver file over the root's `/etc/resolv.conf`:
    /// it is missing or not a regular file, the target cannot hold it, or the caller cannot
    /// mount.
    Resolver(MappingError),
    /// [`Entry::bind_runtime`] failed to make a bind, and to remove some of the binds that it made
    /// before it, which stay mounted, so that a teardown of the root in the caller's mount
    /// namespace fails.
    Rollback {
        /// Why the bind failed: an [`Error::MachineId`], [`Error::TimeStatus`], or
        /// [`Error::Resolver`].
        error: Box<Error>,
        /// The binds made before it that stay mounted.
        teardown: TeardownError,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootPath => formatter.write_str("the root's path is not absolute"),
            Self::Root(error) => write!(formatter, "cannot open the root: {error}"),
            Self::InvalidId => formatter.write_str("an ID of the identity is invalid"),
            Self::Groups => write!(
                formatter,
                "the identity has more than {MAX_GROUPS} supplementary groups"
            ),
            Self::Capability(capability) => write!(formatter, "{capability} is not allowed"),
            Self::NonRootCapabilities => {
                formatter.write_str("an identity other than user ID 0 has capabilities")
            }
            Self::Published(capability) => write!(
                formatter,
                "{capability} is not allowed while listeners are published"
            ),
            Self::Hostname => formatter.write_str("the hostname is invalid"),
            Self::Account(error) => write!(formatter, "the account is unavailable: {error}"),
            Self::MachineId(error) => write!(formatter, "cannot bind the machine ID: {error}"),
            Self::TimeStatus(error) => write!(formatter, "cannot bind the time status: {error}"),
            Self::Resolver(error) => write!(formatter, "cannot bind the resolver file: {error}"),
            Self::Rollback { error, teardown } => write!(
                formatter,
                "{error}; the binds made before it stay mounted: {teardown}"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Root(error) => Some(error),
            Self::MachineId(error) | Self::TimeStatus(error) | Self::Resolver(error) => Some(error),
            Self::Account(error) => Some(error),
            Self::Rollback { error, .. } => Some(error.as_ref()),
            _ => None,
        }
    }
}

/// Reads the account databases below `root` and checks `identity`'s account, as
/// [`AccountMode::Existing`] describes.
fn existing_account(root: &FileDescriptor, identity: &Identity) -> Result<Account, AccountError> {
    let passwd = read_database(root, Database::Passwd)?;
    let account = passwd_account(&passwd, identity.uid, identity.gid)?;
    let group = read_database(root, Database::Group)?;
    if !has_group(&group, identity.gid) {
        return Err(AccountError::Group);
    }
    Ok(account)
}

/// Reads `database` below `root`, through no symbolic link, if it is a regular file of at most
/// [`MAX_ACCOUNT_DATABASE_BYTES`].
fn read_database(root: &FileDescriptor, database: Database) -> Result<Vec<u8>, AccountError> {
    let path = match database {
        Database::Passwd => PASSWD,
        Database::Group => GROUP,
    };
    let read = |error: Errno| AccountError::Read(database, error.into());
    // A FIFO must not block the open, which the file's kind then refuses.
    let file = sys::open_beneath(
        root,
        path,
        OpenFlags::O_RDONLY | OpenFlags::O_NONBLOCK | OpenFlags::O_NOCTTY,
        Mode::empty(),
        true,
    )
    .map_err(read)?;
    let (regular, size) = sys::file_size(&file).map_err(read)?;
    if !regular {
        return Err(AccountError::NotFile(database));
    }
    if size > MAX_ACCOUNT_DATABASE_BYTES {
        return Err(AccountError::TooLarge(database));
    }
    let mut bytes = Vec::new();
    file.take(MAX_ACCOUNT_DATABASE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| AccountError::Read(database, error))?;
    if bytes.len() as u64 > MAX_ACCOUNT_DATABASE_BYTES {
        return Err(AccountError::TooLarge(database));
    }
    Ok(bytes)
}

/// Returns the fields of each entry of an account database, with the ID in its third field: each
/// line that names one. As the C library's files database does, it skips blank lines and
/// comments, whose first character after blanks is `#`.
fn entries(database: &[u8]) -> impl Iterator<Item = (u64, Vec<&[u8]>)> {
    database
        .split(|&byte| byte == b'\n')
        .filter(|line| !matches!(line.trim_ascii_start().first(), None | Some(b'#')))
        .filter_map(|line| {
            let fields: Vec<&[u8]> = line.split(|&byte| byte == b':').collect();
            let id = parse_decimal(fields.get(2)?)?;
            Some((id, fields))
        })
}

/// Finds the one passwd entry of `uid` in `passwd`, and checks that its primary group is `gid`
/// and its home absolute.
fn passwd_account(passwd: &[u8], uid: u32, gid: u32) -> Result<Account, AccountError> {
    let mut matching = entries(passwd).filter(|(id, _)| *id == u64::from(uid));
    let (_, fields) = matching.next().ok_or(AccountError::Missing)?;
    if matching.next().is_some() {
        return Err(AccountError::Duplicate);
    }
    let [name, _, _, primary, _, home, _] = fields[..] else {
        return Err(AccountError::Malformed);
    };
    if name.is_empty() {
        return Err(AccountError::Malformed);
    }
    let primary = parse_decimal(primary)
        .and_then(|primary| u32::try_from(primary).ok())
        .ok_or(AccountError::Malformed)?;
    if primary != gid {
        return Err(AccountError::PrimaryGroup(primary));
    }
    if home.first() != Some(&b'/') {
        return Err(AccountError::Home);
    }
    Ok(Account {
        name: name.to_vec(),
        home: home.to_vec(),
    })
}

/// Returns whether `group` holds an entry of `gid`.
fn has_group(group: &[u8], gid: u32) -> bool {
    entries(group).any(|(id, _)| id == u64::from(gid))
}

#[cfg(test)]
mod tests;
