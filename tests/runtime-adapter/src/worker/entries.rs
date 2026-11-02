//! The probe that enters the worker's assembled root as an adapter's root profiles do: through
//! `nvx::guest::entry` and `nvx::guest::spawn`, each execution in new mount, PID, and UTS
//! namespaces, with the root moved over `/`, a view of its own, and an identity with only the
//! capabilities that the entry allows.
//!
//! The worker copies its own executable to the top of the root, as [`STAGED`], and runs the
//! executions in a helper, `runtime-adapter-guest entries ROOT CGROUP`, which enters a mount
//! namespace of its own first, as NVX's managed agent does, so that the runtime's binds of a
//! generated machine ID and of the time status end with it, before the worker tears the root
//! down. The helper places each execution in the workload cgroup `CGROUP`, leaks it a descriptor
//! of the guest's own `/` without close-on-exec, and has it run [`STAGED`] as
//! `runtime-adapter-guest inside DIRECTORY DESCRIPTOR`, which reports, from inside the root, what
//! the execution sees and may do, in the case's directory below [`CASES`]. The helper blocks
//! `SIGCHLD`, as a caller that collects its children through a signal descriptor does, which no
//! execution inherits.
//!
//! | Probe | Facts |
//! | --- | --- |
//! | `entries` | The capabilities outside the allowlist that the entry refused, `refused.outside`, and those that it accepted, `refused.accepted`; its refusal of `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW` while listeners are published, `refused.published.net-bind-service` and `refused.published.net-raw`, of a capability for another user ID than 0, `refused.non-root`, and of an identity without an account in existing-account mode, `refused.existing`. For each case: `CASE.launch`, `CASE.status`, and the facts of `inside`, as `CASE.KEY`; for the working-directory descriptors, only their distinct `cwd-descriptors.launch` and `cwd-descriptors.status`. The generated machine ID, `generated.source`. Once the helper ended, the root's own machine ID, `after.machine-id`, and the mounts below the root, `after.mounts`, in the worker's mount namespace |
//!
//! The cases, as user ID 0 in numeric-account mode unless they say otherwise, are: each allowed
//! capability alone, unpublished, `only-NAME`, such as `only-net-bind-service`; no capability,
//! `none`; every allowed capability but those that published listeners refuse, published,
//! `published`, which stands for both publication modes, private-network ingress and the host's
//! loopback, since the entry treats them alike; every allowed capability, unpublished,
//! `unpublished`; the user and group ID 4242, of which the image has no account, `numeric`; with
//! every allowed capability, unpublished, the working-directory cases, which name a working
//! directory: the case's own, `cwd-directory`, and, through a magic link, which the entry refuses
//! before the program runs, the leaked descriptor's, `cwd-leaked`, the execution's own working
//! directory and root, `cwd-self` and `cwd-root`, that of its namespace's first process,
//! `cwd-init`, a symbolic link to its working directory, `cwd-link`, and each descriptor's link
//! from 0 to 63, `cwd-descriptors`; and, once the helper has bound the runtime's files, a
//! generated machine ID, the time status that the guest's time daemon publishes at
//! `/run/nvx/time`, with the daemon's own layout, and the hostname `runtime-adapter`,
//! `generated`. The cases before `generated` see the machine ID that the root holds as its own.
//!
//! `inside` reports, in this order: the descriptors open in the execution, `fds`, and whether the
//! leaked one is, `leaked`; `uid`, `gid`, and `groups`; the capability sets, `cap.inh`,
//! `cap.prm`, `cap.eff`, `cap.bnd`, and `cap.amb`, and `no-new-privs`; the signals that it
//! blocks, `signals.blocked`; its process ID, `pid`; its
//! working directory, `cwd`; the `hostname`; the `machine-id`, and the options of a mount at
//! `/etc/machine-id`, if any, `mount.machine-id`; the first line of the time status's state,
//! `time.state`, the names in `/run/nvx/time`, `time.files`, and the options of a mount there,
//! `mount.time`; the unprivileged port start, `threshold`; and the outcome of
//! these attempts: lowering it, `threshold.write`; writing the hostname through `/proc/sys`,
//! `hostname.write`; opening `/proc/sysrq-trigger` for writing, `sysrq.write`; binding UDP port
//! 1023, a privileged source port, from which the gateway accepts listener announcements,
//! `bind`; opening a raw socket, which could forge such a datagram, `raw`; changing the owner of
//! a new file, `chown`; reading a new file of mode 0, `dac-override`; changing the mode of a file
//! of user ID 1000, `fowner`; running a program as user ID 4321, `setuid`, and as group ID 4321,
//! `setgid`. Then it lists the root, `root`, and last tries a nested `chroot` escape, which
//! enters a directory of the root with `chroot`, climbs with `..` from outside it, and enters
//! where it ended with `chroot` again: `escape` is `contained` if it ends in the execution's own
//! root, `escaped: LISTING` otherwise, or the outcome of the first `chroot`.
//!
//! An attempt's outcome is `allowed`, `refused ERRNO`, such as `refused EPERM`, or `failed
//! PROBLEM`. A value that cannot be read is `absent` if its file does not exist, and otherwise
//! `unreadable: PROBLEM`.

use std::collections::BTreeSet;
use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use nix::errno::Errno;
use nix::mount::MsFlags;
use nix::sched::CloneFlags;
use nix::sys::signal::{SigSet, Signal};
use nix::sys::socket::{AddressFamily, SockFlag, SockProtocol, SockType};
use nix::sys::wait::{self, WaitStatus};
use nix::unistd::Pid;
use nvx::guest::entry::{
    AccountError, AccountMode, Capabilities, Capability, Entry, Error as EntryError, Identity,
    MachineId, Publication,
};
use nvx::guest::spawn::{Cgroup, Command};

use super::probes::{self, Context, MOUNTINFO};
use crate::probe::{Facts, Probe};

/// The names of the probes of this module, which the worker runs with its root.
pub const PROBES: [&str; 1] = ["entries"];

/// Where the worker copies its own executable in the root, which each execution runs.
pub const STAGED: &str = "/.runtime-adapter-guest";
/// The directory, in the root, of the cases' directories, one for each case, of its name.
pub const CASES: &str = "/tmp/runtime-adapter-entries";
/// The directory, outside the root, of the generated machine ID.
const SOURCES: &str = "/tmp/runtime-adapter-entry-sources";
/// The machine ID that the helper writes in the root, as the root's own.
pub const ROOT_MACHINE_ID: &str = "0123456789abcdef0123456789abcdef";
/// The guest's time status, which NVX's time daemon publishes from the boot, and which the
/// runtime binds into the root at the same path.
const TIME_STATUS: &str = "/run/nvx/time";
/// The hostname of the `generated` case.
pub const GENERATED_HOSTNAME: &str = "runtime-adapter";
/// The user and group ID of the `numeric` case, of which the image has no account.
pub const NUMERIC_ID: u32 = 4242;
/// The owner of each case's `owned` file, whose mode only `CAP_FOWNER` lets another user change.
const OWNER: u32 = 1000;
/// The user or group ID that the `setuid` and `setgid` attempts take.
const OTHER_ID: u32 = 4321;
/// The privileged UDP port that the `bind` attempt binds.
const PRIVILEGED_PORT: u16 = 1023;
/// The unprivileged port start, which the runtime owns.
const THRESHOLD: &str = "/proc/sys/net/ipv4/ip_unprivileged_port_start";
/// How many times the `escape` attempt climbs with `..`.
const CLIMBS: usize = 32;

/// Runs `probe`, one of [`PROBES`], with `context`, and returns its facts.
///
/// Fails with the problem for another probe, one with arguments, and one that fails.
pub fn run(probe: &Probe, context: &Context<'_>) -> Result<Facts, String> {
    let name = probe.name();
    if !PROBES.contains(&name) {
        return Err(format!("the worker runs no probe named {name}"));
    }
    if !probe.arguments().is_empty() {
        return Err(format!("the {name} probe takes no arguments"));
    }
    entries(context).map_err(|error| format!("the {name} probe failed: {error}"))
}

/// Runs the `entries` probe: stages the worker's executable, runs the helper, and reports what it
/// found, and then what the worker's own mount namespace holds.
fn entries(context: &Context<'_>) -> io::Result<Facts> {
    let program = std::env::current_exe()?;
    let staged = context.root.join(below_root(STAGED));
    fs::copy(&program, &staged)?;
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
    let output = std::process::Command::new(&program)
        .arg(crate::GUEST_ENTRIES_COMMAND)
        .arg(context.root)
        .arg(context.workload)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "the helper ended with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut facts = Facts::decode(&output.stdout)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.problem().to_owned()))?;
    facts.push(
        "after.machine-id",
        probes::read(&context.root.join("etc/machine-id")),
    );
    let table = probes::mount_table(&fs::read_to_string(MOUNTINFO)?)?;
    let below = table
        .iter()
        .filter(|mount| mount.point != context.root && mount.point.starts_with(context.root))
        .count();
    facts.push("after.mounts", below.to_string());
    Ok(facts)
}

/// Runs the executions of the `entries` probe in the root `root`, which each joins the cgroup
/// `workload`, as the helper `runtime-adapter-guest entries ROOT CGROUP` does, and returns their
/// facts, as the [module documentation](self) describes.
///
/// It first enters a mount namespace of its own, whose mounts stop propagating, so that the
/// runtime's binds end with the process, and blocks `SIGCHLD`. It needs the privileges of the
/// guest's root, and must be the only thread of its process.
pub fn helper(root: &Path, workload: &Path) -> io::Result<Facts> {
    nix::sched::unshare(CloneFlags::CLONE_NEWNS)?;
    nix::mount::mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )?;
    SigSet::from(Signal::SIGCHLD).thread_block()?;
    let mut facts = Facts::new();
    refusals(root, &mut facts);

    // A descriptor of the guest's own root, which a caller leaks to each execution: it is not
    // close-on-exec, unlike the descriptors that the standard library opens.
    let leaked = nix::unistd::dup(File::open("/")?)?;
    fs::write(root.join("etc/machine-id"), format!("{ROOT_MACHINE_ID}\n"))?;
    let profile = |capabilities| {
        Identity::new(0, 0)
            .capabilities(capabilities)
            .account(AccountMode::Numeric)
    };
    let unpublished = |identity| Entry::new(root, identity).publication(Publication::Unpublished);
    let mut cases = Capabilities::ALLOWED
        .iter()
        .map(|capability| {
            let name = format!("only-{}", capability_name(capability));
            (
                name,
                unpublished(profile(Capabilities::EMPTY.with(capability))),
            )
        })
        .collect::<Vec<_>>();
    let published = Capabilities::ALLOWED
        .iter()
        .filter(|capability| !Capabilities::PUBLISHED_REFUSED.contains(*capability))
        .collect();
    cases.extend([
        ("none".to_owned(), unpublished(profile(Capabilities::EMPTY))),
        (
            "published".to_owned(),
            Entry::new(root, profile(published)).publication(Publication::Published),
        ),
        (
            "unpublished".to_owned(),
            unpublished(profile(Capabilities::ALLOWED)),
        ),
        (
            "numeric".to_owned(),
            Entry::new(
                root,
                Identity::new(NUMERIC_ID, NUMERIC_ID).account(AccountMode::Numeric),
            ),
        ),
    ]);
    for (name, entry) in &cases {
        execute(root, workload, name, entry, &leaked, None, &mut facts)?;
    }
    working_directories(
        root,
        workload,
        &unpublished(profile(Capabilities::ALLOWED)),
        &leaked,
        &mut facts,
    )?;

    // The runtime's files, which the entry binds once, for every later execution: a machine ID
    // that it generates, and the time status that the guest's time daemon publishes, with the
    // daemon's own layout.
    let sources = Path::new(SOURCES);
    if sources.exists() {
        fs::remove_dir_all(sources)?;
    }
    fs::create_dir_all(sources)?;
    let machine_id = MachineId::generate(sources.join("machine-id"))?;
    facts.push(
        "generated.source",
        probes::read(&sources.join("machine-id")),
    );
    let generated = Entry::new(root, profile(Capabilities::EMPTY))
        .hostname(GENERATED_HOSTNAME)
        .machine_id(machine_id)
        .time_status(TIME_STATUS);
    generated.bind_runtime().map_err(io::Error::other)?;
    execute(
        root,
        workload,
        "generated",
        &generated,
        &leaked,
        None,
        &mut facts,
    )?;
    Ok(facts)
}

/// Runs the working-directory cases of `entry` in `root`, which each join the cgroup `workload`,
/// with `leaked` open in their caller, and adds their facts to `facts`: `cwd-directory`, in its
/// own directory; and, through a magic link, which the entry refuses, `cwd-leaked`, in the
/// directory of the leaked descriptor, `cwd-self` and `cwd-root`, in the execution's own working
/// directory and root, `cwd-init`, in that of its namespace's first process, and `cwd-link`,
/// through a symbolic link to the execution's working directory. Then `cwd-descriptors` tries each
/// descriptor's link from 0 to 63, and adds only the distinct outcomes, `cwd-descriptors.launch`
/// and `cwd-descriptors.status`, each sorted and separated by commas.
fn working_directories(
    root: &Path,
    workload: &Path,
    entry: &Entry,
    leaked: &OwnedFd,
    facts: &mut Facts,
) -> io::Result<()> {
    let own = format!("{CASES}/cwd-directory");
    let descriptor = format!("/proc/self/fd/{}", leaked.as_raw_fd());
    let link = format!("{CASES}/cwd-link/link");
    let target = root.join(below_root(&link));
    fs::create_dir_all(target.parent().unwrap_or(root))?;
    if fs::symlink_metadata(&target).is_ok() {
        fs::remove_file(&target)?;
    }
    std::os::unix::fs::symlink("/proc/self/cwd", &target)?;
    for (name, directory) in [
        ("cwd-directory", own.as_str()),
        ("cwd-leaked", descriptor.as_str()),
        ("cwd-self", "/proc/self/cwd"),
        ("cwd-root", "/proc/self/root"),
        ("cwd-init", "/proc/1/cwd"),
        ("cwd-link", link.as_str()),
    ] {
        execute(root, workload, name, entry, leaked, Some(directory), facts)?;
    }
    let mut launches = BTreeSet::new();
    let mut statuses = BTreeSet::new();
    for number in 0..64 {
        let directory = format!("/proc/self/fd/{number}");
        let (launched, status, _) = spawn_case(
            root,
            workload,
            "cwd-descriptors",
            entry,
            leaked,
            Some(&directory),
        )?;
        launches.insert(launched);
        statuses.insert(status);
    }
    let joined = |outcomes: BTreeSet<String>| outcomes.into_iter().collect::<Vec<_>>().join(",");
    facts.push("cwd-descriptors.launch", joined(launches));
    facts.push("cwd-descriptors.status", joined(statuses));
    Ok(())
}

/// Adds the entry's refusals of identities in `root` to `facts`.
fn refusals(root: &Path, facts: &mut Facts) {
    let check = |identity: Identity, publication| {
        refusal(
            Entry::new(root, identity.account(AccountMode::Numeric))
                .publication(publication)
                .check(),
        )
    };
    let outside = (0..64)
        .filter_map(Capability::new)
        .filter(|capability| !capability.is_allowed())
        .collect::<Vec<_>>();
    let accepted = outside
        .iter()
        .filter(|capability| {
            let identity = Identity::new(0, 0).capabilities(Capabilities::EMPTY.with(**capability));
            check(identity, Publication::Unpublished) != format!("capability {capability}")
        })
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    facts.push(
        "refused.outside",
        (outside.len() - accepted.len()).to_string(),
    );
    let accepted = if accepted.is_empty() {
        "none".to_owned()
    } else {
        accepted.join(",")
    };
    facts.push("refused.accepted", accepted);
    for capability in Capabilities::PUBLISHED_REFUSED.iter() {
        let identity = Identity::new(0, 0).capabilities(Capabilities::EMPTY.with(capability));
        facts.push(
            format!("refused.published.{}", capability_name(capability)),
            check(identity, Publication::Published),
        );
    }
    let identity = Identity::new(NUMERIC_ID, NUMERIC_ID)
        .capabilities(Capabilities::EMPTY.with(Capability::KILL));
    facts.push(
        "refused.non-root",
        check(identity, Publication::Unpublished),
    );
    let existing = Entry::new(root, Identity::new(NUMERIC_ID, NUMERIC_ID)).prepare();
    facts.push("refused.existing", refusal(existing.map(drop)));
}

/// Returns how the entry took an identity: `accepted`, or what refused it.
fn refusal(result: Result<(), EntryError>) -> String {
    match result {
        Ok(()) => "accepted".to_owned(),
        Err(EntryError::Capability(capability)) => format!("capability {capability}"),
        Err(EntryError::NonRootCapabilities) => "non-root-capabilities".to_owned(),
        Err(EntryError::Published(capability)) => format!("published {capability}"),
        Err(EntryError::Account(AccountError::Missing)) => "account-missing".to_owned(),
        Err(error) => format!("other: {error}"),
    }
}

/// Runs the execution of `entry` named `name`, in the cgroup `workload`, which runs [`STAGED`]
/// as `inside`, with `leaked` open in its caller, in the working `directory`, if any, and adds
/// its facts to `facts`.
fn execute(
    root: &Path,
    workload: &Path,
    name: &str,
    entry: &Entry,
    leaked: &OwnedFd,
    directory: Option<&str>,
    facts: &mut Facts,
) -> io::Result<()> {
    let (launched, status, output) = spawn_case(root, workload, name, entry, leaked, directory)?;
    facts.push(format!("{name}.launch"), launched);
    facts.push(format!("{name}.status"), status);
    let found = Facts::decode(&output)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.problem().to_owned()))?;
    for (key, value) in found.iter() {
        facts.push(format!("{name}.{key}"), value);
    }
    Ok(())
}

/// Runs the execution of `entry` named `name`, as [`execute`] does, and returns how it launched,
/// `started` or its failed stage and error, how it ended, and what it reported.
fn spawn_case(
    root: &Path,
    workload: &Path,
    name: &str,
    entry: &Entry,
    leaked: &OwnedFd,
    directory: Option<&str>,
) -> io::Result<(String, String, Vec<u8>)> {
    let inside = format!("{CASES}/{name}");
    let case = root.join(below_root(&inside));
    fs::create_dir_all(&case)?;
    fs::set_permissions(&case, fs::Permissions::from_mode(0o777))?;
    let owned = case.join("owned");
    fs::write(&owned, name)?;
    std::os::unix::fs::chown(&owned, Some(OWNER), Some(OWNER))?;

    let prepared = entry.prepare().map_err(io::Error::other)?;
    let argument = |text: &str| CString::new(text).map_err(io::Error::other);
    let arguments = [
        STAGED,
        crate::GUEST_INSIDE_COMMAND,
        &inside,
        &leaked.as_raw_fd().to_string(),
    ]
    .into_iter()
    .map(argument)
    .collect::<io::Result<Vec<_>>>()?;
    let (mut reader, writer) = io::pipe()?;
    let mut command = Command::new(argument(STAGED)?, arguments)
        .environment(Vec::new())
        .stdin(File::open("/dev/null")?)
        .stdout(writer)
        .stderr(io::stderr().as_fd().try_clone_to_owned()?)
        .cgroup(Cgroup::open(workload)?);
    if let Some(directory) = directory {
        command = command.working_directory(argument(directory)?);
    }
    let spawned = command.enter(prepared).spawn()?;
    let process = Pid::from_raw(i32::try_from(spawned.id()).map_err(io::Error::other)?);
    let launched = spawned.into_report().wait();
    let mut output = Vec::new();
    let read = reader.read_to_end(&mut output);
    let status = wait::waitpid(process, None)?;
    let launched = match launched? {
        None => "started".to_owned(),
        Some(failure) => format!(
            "{} {:?}",
            failure.stage(),
            Errno::from_raw(failure.raw_os_error())
        ),
    };
    read?;
    let status = match status {
        WaitStatus::Exited(_, code) => format!("exit {code}"),
        WaitStatus::Signaled(_, signal, _) => format!("signal {signal}"),
        status => format!("{status:?}"),
    };
    Ok((launched, status, output))
}

/// Reports, from inside an execution's root, what the execution sees and may do, as the
/// [module documentation](self) describes, as `runtime-adapter-guest inside DIRECTORY DESCRIPTOR`
/// does, with the case's `directory` and the number of the descriptor that its caller leaked,
/// if any.
///
/// The last attempt, the nested `chroot` escape, changes the process's root.
pub fn inside(directory: &Path, leaked: Option<u32>) -> Facts {
    let mut facts = Facts::new();
    facts.push("fds", open_descriptors());
    let leaked = leaked.map_or("none", |descriptor| {
        if fs::symlink_metadata(format!("/proc/self/fd/{descriptor}")).is_ok() {
            "open"
        } else {
            "closed"
        }
    });
    facts.push("leaked", leaked);
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| status_field(&status, name).unwrap_or("absent").to_owned();
    let effective = |ids: String| ids.split_whitespace().nth(1).unwrap_or("absent").to_owned();
    facts.push("uid", effective(field("Uid")));
    facts.push("gid", effective(field("Gid")));
    facts.push("groups", field("Groups"));
    for (key, name) in [
        ("cap.inh", "CapInh"),
        ("cap.prm", "CapPrm"),
        ("cap.eff", "CapEff"),
        ("cap.bnd", "CapBnd"),
        ("cap.amb", "CapAmb"),
        ("no-new-privs", "NoNewPrivs"),
        ("signals.blocked", "SigBlk"),
    ] {
        facts.push(key, field(name));
    }
    facts.push("pid", std::process::id().to_string());
    let working = std::env::current_dir().map_or_else(
        |error| format!("unreadable: {error}"),
        |path| path.display().to_string(),
    );
    facts.push("cwd", working);
    facts.push(
        "hostname",
        probes::read(Path::new("/proc/sys/kernel/hostname")),
    );
    facts.push("machine-id", probes::read(Path::new("/etc/machine-id")));
    let table = fs::read_to_string(MOUNTINFO)
        .and_then(|table| probes::mount_table(&table))
        .unwrap_or_default();
    let options = |point: &str| {
        table
            .iter()
            .rev()
            .find(|mount| mount.point == Path::new(point))
            .map_or("none".to_owned(), |mount| mount.options.clone())
    };
    facts.push("mount.machine-id", options("/etc/machine-id"));
    let status = Path::new(TIME_STATUS);
    let state = probes::read(&status.join("state"));
    facts.push("time.state", state.lines().next().unwrap_or_default());
    facts.push("time.files", listing(status));
    facts.push("mount.time", options(TIME_STATUS));
    facts.push("threshold", probes::read(Path::new(THRESHOLD)));
    facts.push("threshold.write", outcome(fs::write(THRESHOLD, "1")));
    facts.push(
        "hostname.write",
        outcome(fs::write("/proc/sys/kernel/hostname", "escaped")),
    );
    let sysrq = OpenOptions::new().write(true).open("/proc/sysrq-trigger");
    facts.push("sysrq.write", outcome(sysrq.map(drop)));
    let bind = std::net::UdpSocket::bind(("0.0.0.0", PRIVILEGED_PORT));
    facts.push("bind", outcome(bind.map(drop)));
    let raw = nix::sys::socket::socket(
        AddressFamily::Inet,
        SockType::Raw,
        SockFlag::SOCK_CLOEXEC,
        SockProtocol::Icmp,
    );
    facts.push("raw", outcome(raw.map(drop).map_err(io::Error::from)));
    facts.push("chown", outcome(change_owner(directory)));
    facts.push("dac-override", outcome(read_unreadable(directory)));
    let mode = fs::set_permissions(directory.join("owned"), fs::Permissions::from_mode(0o600));
    facts.push("fowner", outcome(mode));
    facts.push("setuid", outcome(run_as(Some(OTHER_ID), None)));
    facts.push("setgid", outcome(run_as(None, Some(OTHER_ID))));
    let listing = listing(Path::new("/"));
    facts.push("root", listing.clone());
    facts.push("escape", escape(directory, &listing));
    facts
}

/// Returns the descriptors open in the process, in increasing order, separated by commas.
fn open_descriptors() -> String {
    let numbers = match fs::read_dir("/proc/self/fd") {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
            .collect::<Vec<_>>(),
        Err(error) => return format!("unreadable: {error}"),
    };
    // The directory's own descriptor closed once it was read.
    let mut open = numbers
        .into_iter()
        .filter(|number| fs::symlink_metadata(format!("/proc/self/fd/{number}")).is_ok())
        .collect::<Vec<_>>();
    open.sort_unstable();
    open.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Creates a file in `directory`, and gives it to user and group ID 1000.
fn change_owner(directory: &Path) -> io::Result<()> {
    let path = directory.join("chown");
    File::create(&path)?;
    std::os::unix::fs::chown(&path, Some(OWNER), Some(OWNER))
}

/// Creates a file of mode 0 in `directory`, which only `CAP_DAC_OVERRIDE` lets even its owner
/// read, and opens it for reading.
fn read_unreadable(directory: &Path) -> io::Result<()> {
    let path = directory.join("dac-override");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o000)
        .open(&path)?;
    File::open(&path).map(drop)
}

/// Runs [`STAGED`], which reports its contract, as the user ID `uid` or the group ID `gid`.
fn run_as(uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
    let mut command = std::process::Command::new(STAGED);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(uid) = uid {
        command.uid(uid);
    }
    if let Some(gid) = gid {
        command.gid(gid);
    }
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("the program ended with {status}")))
    }
}

/// Tries a nested `chroot` escape from `directory`, and returns where it ended: `contained` if
/// the root then lists as `listing`, the execution's own root did.
fn escape(directory: &Path, listing: &str) -> String {
    let jail = directory.join("jail");
    if let Err(error) = fs::create_dir(&jail) {
        return format!("failed {error}");
    }
    if let Err(error) = std::os::unix::fs::chroot(&jail) {
        return outcome(Err(error));
    }
    // The working directory stays outside the new root, from where `..` climbs as far as it can.
    for _ in 0..CLIMBS {
        if let Err(error) = std::env::set_current_dir("..") {
            return format!("failed {error}");
        }
    }
    if let Err(error) = std::os::unix::fs::chroot(".") {
        return format!("failed {error}");
    }
    let ended = self::listing(Path::new("/"));
    if ended == listing {
        "contained".to_owned()
    } else {
        format!("escaped: {ended}")
    }
}

/// Returns the names in the directory `path`, sorted, and separated by commas, or `absent` if it
/// does not exist.
fn listing(path: &Path) -> String {
    match fs::read_dir(path) {
        Ok(entries) => {
            let mut names = entries
                .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
                .collect::<Vec<_>>();
            names.sort();
            names.join(",")
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => "absent".to_owned(),
        Err(error) => format!("unreadable: {error}"),
    }
}

/// Returns the outcome of an attempt: `allowed`, `refused ERRNO`, or `failed PROBLEM`.
fn outcome(result: io::Result<()>) -> String {
    match result {
        Ok(()) => "allowed".to_owned(),
        Err(error) => match error.raw_os_error() {
            Some(code) => format!("refused {:?}", Errno::from_raw(code)),
            None => format!("failed {error}"),
        },
    }
}

/// Returns the value of the field `name` of a process's `/proc/PID/status`, trimmed.
fn status_field<'a>(status: &'a str, name: &str) -> Option<&'a str> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(':').map(str::trim))
}

/// Returns the name of `capability` in the probe's facts: its kernel name without `CAP_`, in
/// lowercase, with `-` for `_`, such as `net-bind-service`.
pub fn capability_name(capability: Capability) -> String {
    let name = capability.to_string();
    name.strip_prefix("CAP_")
        .unwrap_or(&name)
        .to_ascii_lowercase()
        .replace('_', "-")
}

/// Returns `path`, an absolute path, relative to the root.
fn below_root(path: &str) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(
        path.as_bytes()
            .strip_prefix(b"/")
            .unwrap_or(path.as_bytes()),
    ))
}

/// Writes `facts` on standard output, as `inside` and the helper report them.
pub fn report(facts: &Facts) -> io::Result<()> {
    let mut output = io::stdout().lock();
    output.write_all(&facts.encode())?;
    output.flush()
}

#[cfg(test)]
mod tests;
