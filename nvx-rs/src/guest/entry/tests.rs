//! Tests of the entry's checks, accounts, machine IDs, runtime binds, and launch stages.
//!
//! The launch tests run each entry without privileges: the child starts without new namespaces
//! and skips the system calls of every entry stage, but fails the one that a test names, so that
//! each stage reports its own failure. The runtime binds' tests run on the root operations' fake
//! kernel, which simulates mounts by their unique IDs. The tests whose names start with
//! `privileged_` run the entry, and the runtime's binds, for real, and only where the test
//! process may mount, as root does.

use std::ffi::CString;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::guest::root::{Call, FakeKernel, Operation, attach_calls};
use crate::guest::spawn::{Command, LaunchFailure, LaunchStage};
use crate::guest::sys::{ChildStatus, Simulation};
use crate::protocol::cmdline::write_resolver;

/// The passwd database of the tests' roots.
const PASSWD_DATABASE: &[u8] = b"root:x:0:0:root:/root:/bin/sh\n\
# A comment names no account.\n\
\n\
nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n\
workload:x:1000:1001::/home/workload:/bin/sh\n";
/// The group database of the tests' roots.
const GROUP_DATABASE: &[u8] = b"root:x:0:\nworkload:x:1001:\nnogroup:x:65534:\n";

/// Returns an entry of a root at `/run/nvx/rootfs` as `identity`, whose account is numeric, so
/// that checking it reads nothing.
fn entry(identity: Identity) -> Entry {
    Entry::new("/run/nvx/rootfs", identity.account(AccountMode::Numeric))
}

/// Returns a temporary root whose `etc` holds `passwd` and `group`, when given.
fn root_with(passwd: Option<&[u8]>, group: Option<&[u8]>) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let etc = root.path().join("etc");
    std::fs::create_dir(&etc).unwrap();
    if let Some(passwd) = passwd {
        std::fs::write(etc.join("passwd"), passwd).unwrap();
    }
    if let Some(group) = group {
        std::fs::write(etc.join("group"), group).unwrap();
    }
    root
}

/// Returns the account error with which an entry of `root` refuses `identity`.
fn account_error(root: &Path, identity: Identity) -> AccountError {
    match Entry::new(root, identity).prepare() {
        Err(Error::Account(error)) => error,
        other => panic!("expected an account error, not {other:?}"),
    }
}

#[test]
fn the_allowlist_holds_the_thirteen_capabilities() {
    let numbers: Vec<u8> = Capabilities::ALLOWED
        .iter()
        .map(Capability::number)
        .collect();
    assert_eq!(numbers, [0, 1, 3, 4, 5, 6, 7, 8, 10, 13, 18, 29, 31]);
    let names: Vec<String> = Capabilities::ALLOWED
        .iter()
        .map(|capability| capability.to_string())
        .collect();
    assert_eq!(
        names,
        [
            "CAP_CHOWN",
            "CAP_DAC_OVERRIDE",
            "CAP_FOWNER",
            "CAP_FSETID",
            "CAP_KILL",
            "CAP_SETGID",
            "CAP_SETUID",
            "CAP_SETPCAP",
            "CAP_NET_BIND_SERVICE",
            "CAP_NET_RAW",
            "CAP_SYS_CHROOT",
            "CAP_AUDIT_WRITE",
            "CAP_SETFCAP",
        ]
    );
    assert_eq!(Capability::new(21).unwrap().to_string(), "CAP_SYS_ADMIN");
    assert_eq!(
        Capability::new(40).unwrap().to_string(),
        "CAP_CHECKPOINT_RESTORE"
    );
    assert_eq!(Capability::new(41).unwrap().to_string(), "capability 41");
    assert_eq!(Capability::new(63).map(Capability::number), Some(63));
    assert_eq!(Capability::new(64), None);
    assert!(Capability::SYS_CHROOT.is_allowed());
    assert!(!Capability::new(27).unwrap().is_allowed(), "CAP_MKNOD");
}

#[test]
fn capability_sets_add_remove_and_list_their_members() {
    let set = Capabilities::EMPTY
        .with(Capability::SETFCAP)
        .with(Capability::CHOWN)
        .with(Capability::CHOWN);
    assert_eq!(set.bits(), 1 | 1 << 31);
    assert!(set.contains(Capability::CHOWN) && set.contains(Capability::SETFCAP));
    assert!(!set.contains(Capability::KILL));
    assert_eq!(
        set.iter().collect::<Vec<_>>(),
        [Capability::CHOWN, Capability::SETFCAP]
    );
    assert_eq!(set.without(Capability::CHOWN).bits(), 1 << 31);
    assert_eq!(set.without(Capability::KILL), set);
    assert!(
        set.without(Capability::CHOWN)
            .without(Capability::SETFCAP)
            .is_empty()
    );
    let highest = Capability::new(63).unwrap();
    assert_eq!(Capabilities::EMPTY.with(highest).bits(), 1 << 63);
    assert_eq!(
        [Capability::KILL, highest]
            .into_iter()
            .collect::<Capabilities>(),
        Capabilities::EMPTY.with(Capability::KILL).with(highest)
    );
    assert_eq!(Capabilities::default(), Capabilities::EMPTY);
    assert_eq!(
        Capabilities::PUBLISHED_REFUSED.iter().collect::<Vec<_>>(),
        [Capability::NET_BIND_SERVICE, Capability::NET_RAW]
    );
}

#[test]
fn every_capability_outside_the_allowlist_is_refused() {
    for number in 0..64 {
        let capability = Capability::new(number).unwrap();
        let identity = Identity::new(0, 0).capabilities(Capabilities::EMPTY.with(capability));
        let result = entry(identity)
            .publication(Publication::Unpublished)
            .check();
        if Capabilities::ALLOWED.contains(capability) {
            assert!(result.is_ok(), "{capability}: {result:?}");
        } else {
            assert!(
                matches!(result, Err(Error::Capability(refused)) if refused == capability),
                "{capability}: {result:?}"
            );
        }
    }
    // The first refused capability is reported, whatever else the set holds.
    let identity = Identity::new(0, 0).capabilities(
        Capabilities::ALLOWED
            .with(Capability::new(21).unwrap())
            .with(Capability::new(27).unwrap()),
    );
    let error = entry(identity).check().unwrap_err();
    assert!(matches!(error, Error::Capability(capability) if capability.number() == 21));
    assert_eq!(error.to_string(), "CAP_SYS_ADMIN is not allowed");
}

#[test]
fn only_user_id_zero_may_hold_capabilities() {
    for uid in [1, 1000, 65534] {
        let identity =
            Identity::new(uid, 0).capabilities(Capabilities::EMPTY.with(Capability::KILL));
        assert!(matches!(
            entry(identity).check(),
            Err(Error::NonRootCapabilities)
        ));
        assert!(entry(Identity::new(uid, 0)).check().is_ok());
    }
    // Root with no capability is a valid identity, and so is root with all thirteen while no
    // listener is published.
    assert!(entry(Identity::new(0, 0)).check().is_ok());
    let identity = Identity::new(0, 0).capabilities(Capabilities::ALLOWED);
    assert!(
        entry(identity)
            .publication(Publication::Unpublished)
            .check()
            .is_ok()
    );
    // A refused capability outranks the non-root rule.
    let identity =
        Identity::new(1000, 0).capabilities(Capabilities::EMPTY.with(Capability::new(21).unwrap()));
    assert!(matches!(entry(identity).check(), Err(Error::Capability(_))));
}

#[test]
fn published_listeners_refuse_binding_and_raw_sockets() {
    for capability in [Capability::NET_BIND_SERVICE, Capability::NET_RAW] {
        let identity = Identity::new(0, 0)
            .capabilities(Capabilities::EMPTY.with(capability).with(Capability::CHOWN));
        let published = entry(identity.clone());
        assert_eq!(published.publication, Publication::Published);
        let error = published.check().unwrap_err();
        assert!(
            matches!(error, Error::Published(refused) if refused == capability),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!("{capability} is not allowed while listeners are published")
        );
        assert!(
            entry(identity)
                .publication(Publication::Unpublished)
                .check()
                .is_ok()
        );
    }
    let identity = Identity::new(0, 0).capabilities(
        Capabilities::ALLOWED
            .without(Capability::NET_BIND_SERVICE)
            .without(Capability::NET_RAW),
    );
    assert!(entry(identity).check().is_ok());
}

#[test]
fn invalid_roots_ids_groups_and_hostnames_are_refused() {
    let check = |entry: Entry| entry.check();
    assert!(matches!(
        check(Entry::new("relative", Identity::new(1, 1))),
        Err(Error::RootPath)
    ));
    assert!(matches!(
        check(Entry::new(
            std::ffi::OsStr::from_bytes(b"/root\0"),
            Identity::new(1, 1)
        )),
        Err(Error::RootPath)
    ));
    for identity in [
        Identity::new(u32::MAX, 1),
        Identity::new(1, u32::MAX),
        Identity::new(1, 1).groups([2, u32::MAX]),
    ] {
        assert!(matches!(check(entry(identity)), Err(Error::InvalidId)));
    }
    let groups: Vec<u32> = (0..u32::try_from(MAX_GROUPS).unwrap()).collect();
    assert!(check(entry(Identity::new(1, 1).groups(groups.clone()))).is_ok());
    let mut too_many = groups;
    too_many.push(1 << 20);
    assert!(matches!(
        check(entry(Identity::new(1, 1).groups(too_many))),
        Err(Error::Groups)
    ));
    for hostname in [b"".to_vec(), vec![b'h'; 65], b"host\0name".to_vec()] {
        assert!(matches!(
            check(entry(Identity::new(1, 1)).hostname(hostname)),
            Err(Error::Hostname)
        ));
    }
    assert!(check(entry(Identity::new(1, 1)).hostname(vec![b'h'; 64])).is_ok());
    assert_eq!(
        entry(Identity::new(1, 1)).hostname,
        DEFAULT_HOSTNAME.as_bytes()
    );
}

#[test]
fn passwd_entries_are_parsed_and_checked() {
    let account = passwd_account(PASSWD_DATABASE, 65534, 65534).unwrap();
    assert_eq!(
        (account.name(), account.home()),
        (b"nobody".as_slice(), b"/nonexistent".as_slice())
    );
    let account = passwd_account(PASSWD_DATABASE, 1000, 1001).unwrap();
    assert_eq!(account.home(), b"/home/workload");
    assert!(matches!(
        passwd_account(PASSWD_DATABASE, 1001, 1001),
        Err(AccountError::Missing)
    ));
    assert!(matches!(
        passwd_account(b"", 0, 0),
        Err(AccountError::Missing)
    ));
    let duplicate = [PASSWD_DATABASE, b"other:x:1000:1001::/home/other:/bin/sh\n"].concat();
    assert!(matches!(
        passwd_account(&duplicate, 1000, 1001),
        Err(AccountError::Duplicate)
    ));
    // Leading zeros are decimal, as the shell init's comparison read them.
    let padded = b"padded:x:0065534:65534::/home/padded:/bin/sh";
    assert!(passwd_account(padded, 65534, 65534).is_ok());
    assert!(matches!(
        passwd_account(PASSWD_DATABASE, 1000, 1000),
        Err(AccountError::PrimaryGroup(1001))
    ));
    for malformed in [
        b"short:x:1000:1001::/home/short".as_slice(),
        b"long:x:1000:1001::/home/long:/bin/sh:extra",
        b":x:1000:1001::/home/nameless:/bin/sh",
        b"group:x:1000:not-a-number::/home/group:/bin/sh",
        b"group:x:1000:-1::/home/group:/bin/sh",
        b"group:x:1000:4294967296::/home/group:/bin/sh",
    ] {
        assert!(
            matches!(
                passwd_account(malformed, 1000, 1001),
                Err(AccountError::Malformed)
            ),
            "{}",
            String::from_utf8_lossy(malformed)
        );
    }
    for home in [b"".as_slice(), b"relative/home", b"~"] {
        let line = [b"workload:x:1000:1001::".as_slice(), home, b":/bin/sh"].concat();
        assert!(matches!(
            passwd_account(&line, 1000, 1001),
            Err(AccountError::Home)
        ));
    }
    // Blank lines, comments, and lines whose third field is no ID name no account.
    assert!(matches!(
        passwd_account(
            b"# 1000:1000:1000\n  #x:x:1000:1000::/home/x:/bin/sh\n\n+::::::\n",
            1000,
            1000
        ),
        Err(AccountError::Missing)
    ));
}

#[test]
fn group_entries_are_found_by_id() {
    assert!(has_group(GROUP_DATABASE, 1001));
    assert!(has_group(GROUP_DATABASE, 65534));
    assert!(!has_group(GROUP_DATABASE, 1000));
    assert!(!has_group(b"# x:x:1000:\n\n", 1000));
}

#[test]
fn existing_accounts_are_read_below_the_root() {
    let root = root_with(Some(PASSWD_DATABASE), Some(GROUP_DATABASE));
    let prepared = Entry::new(root.path(), Identity::new(1000, 1001))
        .prepare()
        .unwrap();
    let account = prepared.account().unwrap();
    assert_eq!(
        (account.name(), account.home()),
        (b"workload".as_slice(), b"/home/workload".as_slice())
    );
    let child = prepared.into_child();
    assert_eq!(child.root.as_bytes(), root.path().as_os_str().as_bytes());
    assert_eq!((child.uid, child.gid, child.capabilities), (1000, 1001, 0));
    assert_eq!(child.devices, 0b11_1111);
    assert_eq!(&*child.hostname, DEFAULT_HOSTNAME.as_bytes());

    assert!(matches!(
        account_error(root.path(), Identity::new(1000, 1000)),
        AccountError::PrimaryGroup(1001)
    ));
    assert!(matches!(
        account_error(root.path(), Identity::new(2000, 2000)),
        AccountError::Missing
    ));
    let missing_group = root_with(Some(PASSWD_DATABASE), Some(b"root:x:0:\n"));
    assert!(matches!(
        account_error(missing_group.path(), Identity::new(1000, 1001)),
        AccountError::Group
    ));
    // Numeric mode reads no database.
    let empty = root_with(None, None);
    let prepared = Entry::new(
        empty.path(),
        Identity::new(1000, 1001).account(AccountMode::Numeric),
    )
    .prepare()
    .unwrap();
    assert!(prepared.account().is_none());
    assert!(matches!(
        account_error(empty.path(), Identity::new(1000, 1001)),
        AccountError::Read(Database::Passwd, error)
            if error.raw_os_error() == Some(Errno::ENOENT as i32)
    ));
    let no_group = root_with(Some(PASSWD_DATABASE), None);
    assert!(matches!(
        account_error(no_group.path(), Identity::new(1000, 1001)),
        AccountError::Read(Database::Group, _)
    ));
}

#[test]
fn account_databases_are_bounded_regular_files_reached_through_no_link() {
    let identity = || Identity::new(1000, 1001);
    // A symbolic link, as the database or on its path, fails the read, even one that leads to a
    // valid database inside the root.
    let root = root_with(None, Some(GROUP_DATABASE));
    std::fs::write(root.path().join("passwd"), PASSWD_DATABASE).unwrap();
    std::os::unix::fs::symlink("../passwd", root.path().join("etc/passwd")).unwrap();
    assert!(matches!(
        account_error(root.path(), identity()),
        AccountError::Read(Database::Passwd, error)
            if error.raw_os_error() == Some(Errno::ELOOP as i32)
    ));
    let root = tempfile::tempdir().unwrap();
    let real = root_with(Some(PASSWD_DATABASE), Some(GROUP_DATABASE));
    std::os::unix::fs::symlink(real.path().join("etc"), root.path().join("etc")).unwrap();
    assert!(matches!(
        account_error(root.path(), identity()),
        AccountError::Read(Database::Passwd, error)
            if error.raw_os_error() == Some(Errno::ELOOP as i32)
    ));
    // A directory or a FIFO is no database, and the FIFO does not block the read.
    let root = root_with(None, Some(GROUP_DATABASE));
    std::fs::create_dir(root.path().join("etc/passwd")).unwrap();
    assert!(matches!(
        account_error(root.path(), identity()),
        AccountError::NotFile(Database::Passwd)
    ));
    let root = root_with(Some(PASSWD_DATABASE), None);
    let fifo = sys::path_c_string(&root.path().join("etc/group")).unwrap();
    sys::create_fifo(&fifo, Mode::from_bits_retain(0o644)).unwrap();
    assert!(matches!(
        account_error(root.path(), identity()),
        AccountError::NotFile(Database::Group)
    ));
    // A database of exactly the bound is read, and one byte more is refused.
    let mut database = PASSWD_DATABASE.to_vec();
    database.resize(usize::try_from(MAX_ACCOUNT_DATABASE_BYTES).unwrap(), b'\n');
    let root = root_with(Some(&database), Some(GROUP_DATABASE));
    assert!(Entry::new(root.path(), identity()).prepare().is_ok());
    database.push(b'\n');
    std::fs::write(root.path().join("etc/passwd"), &database).unwrap();
    assert!(matches!(
        account_error(root.path(), identity()),
        AccountError::TooLarge(Database::Passwd)
    ));
}

#[test]
fn an_entry_needs_a_root_directory() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let result = Entry::new(
        "/nonexistent/root",
        Identity::new(1, 1).account(AccountMode::Numeric),
    )
    .prepare();
    assert!(matches!(result, Err(Error::Root(_))), "{result:?}");
    let result = Entry::new(
        file.path(),
        Identity::new(1, 1).account(AccountMode::Numeric),
    )
    .prepare();
    assert!(
        matches!(&result, Err(Error::Root(error))
            if error.raw_os_error() == Some(Errno::ENOTDIR as i32)),
        "{result:?}"
    );
}

#[test]
fn machine_ids_are_version_4_uuids_in_lowercase_hexadecimal() {
    assert_eq!(
        &machine_id_text([0; 16]),
        b"00000000000040008000000000000000\n"
    );
    assert_eq!(
        &machine_id_text([0xff; 16]),
        b"ffffffffffff4fffbfffffffffffffff\n"
    );
    assert_eq!(
        &machine_id_text([
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef
        ]),
        b"0123456789ab4def8123456789abcdef\n"
    );
}

#[test]
fn a_machine_id_is_generated_once_per_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("machine-id");
    let mode = MachineId::generate(&path).unwrap();
    assert_eq!(mode, MachineId::Generated(path.clone()));
    let text = std::fs::read(&path).unwrap();
    assert_eq!(text.len(), 33);
    assert_eq!(text[32], b'\n');
    assert!(
        text[..32]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    );
    assert_eq!(text[12], b'4');
    assert!(matches!(text[16], b'8' | b'9' | b'a' | b'b'));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o444
    );
    let error = MachineId::generate(&path).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&path).unwrap(), text);
    // A link at the path is never followed.
    let target = directory.path().join("target");
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert_eq!(
        MachineId::generate(&link).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert!(!target.exists());
    let other = directory.path().join("other");
    MachineId::generate(&other).unwrap();
    assert_ne!(std::fs::read(&other).unwrap(), text);
}

#[test]
fn a_generated_machine_id_has_mode_0444_whatever_the_umask() {
    // The umask is the process's, so the test runs alone in a process of its own.
    if crate::guest::managed_agent::isolate_process_test(
        "NVX_ENTRY_UMASK_TEST_CHILD",
        "guest::entry::tests::a_generated_machine_id_has_mode_0444_whatever_the_umask",
    ) {
        return;
    }
    nix::sys::stat::umask(sys::Mode::from_bits_truncate(0o077));
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("machine-id");
    MachineId::generate(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o444
    );
}

#[test]
fn runtime_binds_need_a_mount_root_unless_there_are_none() {
    let root = root_with(None, None);
    let identity = || Identity::new(1, 1).account(AccountMode::Numeric);
    // The image's machine ID and resolver file, and no time status, bind nothing, so any root
    // does, and there is nothing to remove.
    let binds = Entry::new(root.path(), identity()).bind_runtime().unwrap();
    assert!(binds.root.is_none() && binds.binds.is_empty());
    binds.remove().unwrap();
    let machine_id = MachineId::generate(root.path().join("machine-id")).unwrap();
    let result = Entry::new(root.path(), identity())
        .machine_id(machine_id)
        .bind_runtime();
    assert!(
        matches!(&result, Err(Error::Root(error)) if error.kind() == io::ErrorKind::InvalidInput),
        "{result:?}"
    );
    let result = Entry::new(root.path(), identity())
        .time_status(root.path().join("etc"))
        .bind_runtime();
    assert!(matches!(result, Err(Error::Root(_))), "{result:?}");
    let result = Entry::new(root.path(), identity())
        .resolver(root.path().join("machine-id"))
        .bind_runtime();
    assert!(matches!(result, Err(Error::Root(_))), "{result:?}");
    // The inputs are checked first.
    let result = Entry::new(root.path(), identity())
        .hostname("")
        .time_status(root.path().join("etc"))
        .bind_runtime();
    assert!(matches!(result, Err(Error::Hostname)), "{result:?}");
}

/// A root, `root`, on a mount of the fake kernel's, with an empty `etc`, and the sources of the
/// runtime's files, in `sources`: a generated machine ID, `machine-id`, the time status's
/// directory, `time`, and a resolver file that names `10.0.0.1`, `resolv.conf`, below a
/// temporary directory.
struct RuntimeFixture {
    /// The temporary directory, which the fixture holds only so that dropping it removes it.
    _directory: tempfile::TempDir,
    /// The root.
    root: PathBuf,
    /// The directory of the sources.
    sources: PathBuf,
    /// The fake kernel.
    kernel: FakeKernel,
}

impl RuntimeFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        let root = base.join("root");
        let sources = base.join("sources");
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::create_dir_all(sources.join("time")).unwrap();
        MachineId::generate(sources.join("machine-id")).unwrap();
        let resolver = std::fs::File::create(sources.join("resolv.conf")).unwrap();
        write_resolver(resolver, "10.0.0.1".parse().unwrap()).unwrap();
        let kernel = FakeKernel::default();
        kernel.add_mount(&root);
        Self {
            _directory: directory,
            root,
            sources,
            kernel,
        }
    }

    /// Returns the entry of the root, of user ID 0 in numeric-account mode, with every runtime
    /// file of the sources.
    fn entry(&self) -> Entry {
        Entry::new(
            &self.root,
            Identity::new(0, 0).account(AccountMode::Numeric),
        )
        .machine_id(MachineId::Generated(self.sources.join("machine-id")))
        .time_status(self.sources.join("time"))
        .resolver(self.sources.join("resolv.conf"))
    }

    /// Returns the mount points of the runtime's targets in the root, in the order of the binds.
    fn points(&self) -> [PathBuf; 3] {
        ["etc/machine-id", "run/nvx/time", "etc/resolv.conf"].map(|target| self.root.join(target))
    }

    /// Returns the mount points as the fake kernel records them, in reverse order.
    fn removals(&self, points: &[PathBuf]) -> Vec<String> {
        points
            .iter()
            .rev()
            .map(|point| point.display().to_string())
            .collect()
    }
}

#[test]
fn runtime_binds_are_made_in_order_and_removed_in_reverse() {
    let fixture = RuntimeFixture::new();
    // The image's resolver file is a link, as a resolver daemon's image has, which no bind
    // covers; it gives way to an empty file, and its target is left alone.
    let outside = fixture.sources.join("stub-resolv.conf");
    std::fs::write(&outside, b"nameserver 127.0.0.53\n").unwrap();
    std::os::unix::fs::symlink(&outside, fixture.root.join("etc/resolv.conf")).unwrap();
    let binds = fixture
        .entry()
        .bind_runtime_with(&mut fixture.kernel.clone())
        .unwrap();
    let points = fixture.points();
    let expected: Vec<Call> = ["machine-id", "time", "resolv.conf"]
        .iter()
        .zip(&points)
        .flat_map(|(source, point)| {
            attach_calls(&fixture.sources.join(source), point, Access::ReadOnly)
        })
        .collect();
    assert_eq!(fixture.kernel.calls(), expected);
    for (bind, point) in binds.binds.iter().zip(&points) {
        assert_eq!(fixture.root.join(bind.target()), *point);
        assert!(fixture.kernel.mount_at(point).is_some(), "{point:?}");
    }
    let replaced = std::fs::symlink_metadata(fixture.root.join("etc/resolv.conf")).unwrap();
    assert!(replaced.is_file() && replaced.len() == 0);
    assert_eq!(replaced.permissions().mode() & 0o7777, 0o644);
    assert_eq!(std::fs::read(&outside).unwrap(), b"nameserver 127.0.0.53\n");
    // Their removal takes them off, last first.
    binds.remove_with(&mut fixture.kernel.clone()).unwrap();
    assert_eq!(fixture.kernel.unmounts(), fixture.removals(&points));
    assert!(
        points
            .iter()
            .all(|point| fixture.kernel.mount_at(point).is_none())
    );
}

#[test]
fn a_failed_runtime_bind_removes_the_binds_before_it() {
    // A missing resolver file, after the machine ID and the time status.
    let fixture = RuntimeFixture::new();
    let points = fixture.points();
    let result = fixture
        .entry()
        .resolver(fixture.sources.join("missing"))
        .bind_runtime_with(&mut fixture.kernel.clone());
    assert!(
        matches!(
            &result,
            Err(Error::Resolver(MappingError::System { step: Step::Source, error }))
                if *error == Errno::ENOENT as i32
        ),
        "{result:?}"
    );
    assert_eq!(fixture.kernel.unmounts(), fixture.removals(&points[..2]));
    assert!(
        points
            .iter()
            .all(|point| fixture.kernel.mount_at(point).is_none())
    );
    // A target of another kind for the time status, after the machine ID.
    let fixture = RuntimeFixture::new();
    let points = fixture.points();
    std::fs::create_dir_all(fixture.root.join("run/nvx")).unwrap();
    std::fs::write(fixture.root.join("run/nvx/time"), b"").unwrap();
    let result = fixture
        .entry()
        .bind_runtime_with(&mut fixture.kernel.clone());
    assert!(
        matches!(result, Err(Error::TimeStatus(MappingError::TargetKind))),
        "{result:?}"
    );
    assert_eq!(fixture.kernel.unmounts(), fixture.removals(&points[..1]));
    assert_eq!(fixture.kernel.mount_at(&points[0]), None);
    // A failure of the first bind leaves nothing to remove.
    let fixture = RuntimeFixture::new();
    std::fs::create_dir(fixture.root.join("etc/machine-id")).unwrap();
    let result = fixture
        .entry()
        .bind_runtime_with(&mut fixture.kernel.clone());
    assert!(
        matches!(result, Err(Error::MachineId(MappingError::TargetKind))),
        "{result:?}"
    );
    assert!(fixture.kernel.calls().is_empty());
}

#[test]
fn a_failed_runtime_bind_names_the_binds_before_it_that_stay() {
    // A missing resolver file, after the machine ID and the time status, whose bind is busy.
    let fixture = RuntimeFixture::new();
    let points = fixture.points();
    fixture
        .kernel
        .fail(Operation::Unmount, &points[1], Errno::EBUSY);
    let error = fixture
        .entry()
        .resolver(fixture.sources.join("missing"))
        .bind_runtime_with(&mut fixture.kernel.clone())
        .unwrap_err();
    let Error::Rollback {
        error: failure,
        teardown,
    } = &error
    else {
        panic!("{error:?}");
    };
    assert!(
        matches!(
            failure.as_ref(),
            Error::Resolver(MappingError::System { step: Step::Source, error: code })
                if *code == Errno::ENOENT as i32
        ),
        "{failure:?}"
    );
    let stayed: Vec<&Path> = teardown
        .failures()
        .iter()
        .map(|stayed| stayed.target())
        .collect();
    assert_eq!(stayed, [Path::new("/run/nvx/time")]);
    assert_eq!(
        error.to_string(),
        format!("{failure}; the binds made before it stay mounted: {teardown}")
    );
    assert!(std::error::Error::source(&error).is_some());
    // The rollback attempts each bind, so the machine ID's is removed all the same.
    assert_eq!(fixture.kernel.unmounts(), fixture.removals(&points[..2]));
    assert!(fixture.kernel.mount_at(&points[1]).is_some());
    assert!(fixture.kernel.mount_at(&points[0]).is_none());
}

#[test]
fn a_runtime_bind_that_stays_is_reported() {
    let fixture = RuntimeFixture::new();
    let binds = fixture
        .entry()
        .bind_runtime_with(&mut fixture.kernel.clone())
        .unwrap();
    let points = fixture.points();
    fixture
        .kernel
        .fail(Operation::Unmount, &points[2], Errno::EBUSY);
    let error = binds.remove_with(&mut fixture.kernel.clone()).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "failed to unmount the mapping at /etc/resolv.conf: failed to unmount the mapping: {}",
            io::Error::from_raw_os_error(Errno::EBUSY as i32)
        )
    );
    // The others are removed all the same.
    assert_eq!(fixture.kernel.unmounts(), fixture.removals(&points));
    assert!(fixture.kernel.mount_at(&points[2]).is_some());
    assert!(fixture.kernel.mount_at(&points[0]).is_none());
}

#[test]
fn runtime_sources_are_the_directories_beside_their_files() {
    use std::ffi::OsStr;

    // A relative path of one component names a file of the current directory.
    assert_eq!(
        runtime_source(Path::new("machine-id")),
        Some((Path::new("."), OsStr::new("machine-id")))
    );
    assert_eq!(
        runtime_source(Path::new("/run/nvx/workload-machine-id")),
        Some((Path::new("/run/nvx"), OsStr::new("workload-machine-id")))
    );
    assert_eq!(
        runtime_source(Path::new("state/time")),
        Some((Path::new("state"), OsStr::new("time")))
    );
    for path in ["", "/", "..", "time/.."] {
        assert_eq!(runtime_source(Path::new(path)), None, "{path}");
    }
}

#[test]
fn only_a_link_at_a_runtime_file_of_etc_is_removed() {
    for target in [MACHINE_ID_TARGET, RESOLVER_TARGET] {
        let root = root_with(None, None);
        let link = root.path().join(target.trim_start_matches('/'));
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        remove_link(root.path(), target).unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err());
        assert!(outside.path().exists());
        assert_eq!(remove_link(root.path(), target), Err(Errno::ENOENT));
        std::fs::write(&link, b"image\n").unwrap();
        assert_eq!(remove_link(root.path(), target), Err(Errno::EEXIST));
        assert_eq!(std::fs::read(&link).unwrap(), b"image\n");
        // A link on the way to it is not followed.
        let linked = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(root.path().join("etc"), linked.path().join("etc")).unwrap();
        assert_eq!(remove_link(linked.path(), target), Err(Errno::ELOOP));
        assert_eq!(std::fs::read(&link).unwrap(), b"image\n");
    }
    // Only a file of `/etc` is a runtime file there.
    let root = root_with(None, None);
    assert_eq!(
        remove_link(root.path(), TIME_STATUS_TARGET),
        Err(Errno::EINVAL)
    );
}

#[test]
fn device_sets_select_their_nodes() {
    let entry = entry(Identity::new(1, 1));
    assert_eq!(entry.devices, 0b11_1111);
    assert_eq!(entry.clone().devices([]).devices, 0);
    assert_eq!(
        entry
            .devices([DeviceNode::Null, DeviceNode::Tty, DeviceNode::Null])
            .devices,
        0b10_0001
    );
}

/// Returns an execution, prepared without privileges, whose child skips every entry stage's
/// system calls and fails `stage` with `error`.
fn simulated(stage: LaunchStage, error: Errno) -> Prepared {
    Prepared {
        child: EntryChild {
            root: c"/".to_owned(),
            hostname: b"simulated".to_vec().into_boxed_slice(),
            devices: 0,
            uid: 0,
            gid: 0,
            groups: Box::new([]),
            capabilities: 0,
            simulated: Some(Simulation::Fail(stage, error)),
        },
        account: None,
    }
}

/// Returns a command that runs the shell script `script`.
fn shell(script: &str) -> Command {
    Command::new(
        c"/bin/sh".to_owned(),
        vec![
            c"sh".to_owned(),
            c"-c".to_owned(),
            CString::new(script).unwrap(),
        ],
    )
}

/// Spawns `command`, which waits at a barrier, releases it, and returns the child's launch
/// failure, status, and standard output.
fn run_released(command: Command) -> (Option<LaunchFailure>, Option<ChildStatus>, Vec<u8>) {
    let (mut output, writer) = std::io::pipe().unwrap();
    let mut spawned = command.barrier().stdout(writer).spawn().unwrap();
    let process = spawned.process();
    spawned.take_barrier().unwrap().release().unwrap();
    let failure = spawned.into_report().wait().unwrap();
    let mut bytes = Vec::new();
    output.read_to_end(&mut bytes).unwrap();
    (failure, sys::wait(process), bytes)
}

#[test]
fn each_entry_stage_reports_its_own_failure_and_runs_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("ran");
    let script = format!("touch '{}'", marker.display());
    let stages = [
        (LaunchStage::Propagation, Errno::EINVAL),
        (LaunchStage::Hostname, Errno::EPERM),
        (LaunchStage::MoveRoot, Errno::EBUSY),
        (LaunchStage::Proc, Errno::ENODEV),
        (LaunchStage::Sys, Errno::EACCES),
        (LaunchStage::Devices, Errno::ENOSPC),
        (LaunchStage::Bounding, Errno::EPERM),
        (LaunchStage::Identity, Errno::EINVAL),
        (LaunchStage::Capabilities, Errno::EPERM),
        (LaunchStage::NoNewPrivileges, Errno::EINVAL),
    ];
    for (stage, error) in stages {
        let (failure, status, _) = run_released(shell(&script).enter(simulated(stage, error)));
        let failure = failure.unwrap_or_else(|| panic!("{stage} did not fail"));
        assert_eq!(
            (failure.stage(), failure.raw_os_error()),
            (stage, error as i32)
        );
        assert_eq!(status, Some(ChildStatus::Exited(125)), "{stage}");
        assert!(!marker.exists(), "{stage} ran the program");
        assert_eq!(
            failure.to_string(),
            format!(
                "the {stage} launch stage failed: {}",
                io::Error::from_raw_os_error(error as i32)
            )
        );
    }
}

#[test]
fn a_denied_umask_fails_the_proc_stage_and_runs_nothing() {
    // A seccomp filter lasts as long as the thread that installs it, and passes to the processes
    // that the thread starts, so the test runs in a process of its own.
    if crate::guest::managed_agent::isolate_process_test(
        "NVX_ENTRY_DENIED_UMASK_TEST_CHILD",
        "guest::entry::tests::a_denied_umask_fails_the_proc_stage_and_runs_nothing",
    ) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("ran");
    // The simulated stage is no entry stage, so every entry stage succeeds, and the child's own
    // call, which clears its umask before it mounts the view, is the one that fails.
    sys::deny_system_call(nix::libc::SYS_umask);
    let (failure, status, _) = run_released(
        shell(&format!("touch '{}'", marker.display()))
            .enter(simulated(LaunchStage::Credentials, Errno::EPERM)),
    );
    let failure = failure.expect("the denied umask fails the launch");
    assert_eq!(
        (failure.stage(), failure.raw_os_error()),
        (LaunchStage::Proc, Errno::EPERM as i32)
    );
    assert_eq!(status, Some(ChildStatus::Exited(125)));
    assert!(!marker.exists());
}

#[test]
fn an_entered_program_leads_a_session_without_a_terminal_or_the_callers_descriptors() {
    // A descriptor that the caller opened without close-on-exec, as another thread of a caller
    // might, and one that the command passes on.
    let leaked = nix::fcntl::open(c"/dev/null", OpenFlags::O_RDONLY, Mode::empty()).unwrap();
    let passed = tempfile::tempfile().unwrap();
    let leaked_number = std::os::fd::AsRawFd::as_raw_fd(&leaked);
    let passed_number = std::os::fd::AsRawFd::as_raw_fd(&passed);
    // The simulated stage is no entry stage, so every entry stage succeeds.
    let script = format!(
        "read -r pid comm state ppid pgrp session tty rest </proc/self/stat; \
         printf '%s %s %s %s|' \"$pid\" \"$pgrp\" \"$session\" \"$tty\"; \
         test -e /proc/self/fd/{passed_number} && printf passed; \
         test -e /proc/self/fd/{leaked_number} && printf leaked; :"
    );
    let (failure, status, output) = run_released(
        shell(&script)
            .inherit(passed)
            .enter(simulated(LaunchStage::Credentials, Errno::EPERM)),
    );
    drop(leaked);
    assert_eq!((failure, status), (None, Some(ChildStatus::Exited(0))));
    let output = String::from_utf8(output).unwrap();
    let (ids, descriptors) = output.split_once('|').unwrap();
    let ids: Vec<&str> = ids.split(' ').collect();
    // `sh -c` reads its own status file: the entry's child leads its session and process group,
    // without a terminal.
    assert_eq!(ids[1], ids[0], "{output}");
    assert_eq!(ids[2], ids[0], "{output}");
    assert_eq!(ids[3], "0", "{output}");
    assert_eq!(descriptors, "passed");
}

/// Runs `cat /proc/self/status` as the execution that `prepared` enters, while the caller ignores
/// `SIGHUP` and `SIGPIPE`, and its thread blocks `SIGCHLD`, as one that collects its children
/// through a signal descriptor does, and checks that the program ignores and blocks no signal.
fn check_signals(prepared: Prepared) {
    sys::ignore_signals(&[sys::Signal::SIGHUP, sys::Signal::SIGPIPE]);
    let blocked = nix::sys::signal::SigSet::from(sys::Signal::SIGCHLD);
    blocked.thread_block().unwrap();
    let (failure, status, output) = run_released(
        Command::new(
            c"/bin/cat".to_owned(),
            vec![c"cat".to_owned(), c"/proc/self/status".to_owned()],
        )
        .enter(prepared),
    );
    blocked.thread_unblock().unwrap();
    assert_eq!((failure, status), (None, Some(ChildStatus::Exited(0))));
    let output = String::from_utf8(output).unwrap();
    for set in ["SigIgn", "SigBlk"] {
        let value = output
            .lines()
            .find_map(|line| line.strip_prefix(set)?.strip_prefix(':'))
            .unwrap_or_else(|| panic!("{set} is missing from {output}"));
        assert_eq!(value.trim(), "0000000000000000", "{set}");
    }
}

#[test]
fn an_entered_program_ignores_and_blocks_no_signal_of_its_caller_s() {
    // Ignoring a signal changes the whole process, so the test runs in a process of its own.
    if crate::guest::managed_agent::isolate_process_test(
        "NVX_ENTRY_SIGNALS_TEST_CHILD",
        "guest::entry::tests::an_entered_program_ignores_and_blocks_no_signal_of_its_caller_s",
    ) {
        return;
    }
    // The simulated stage is no entry stage, so every entry stage succeeds.
    check_signals(simulated(LaunchStage::Credentials, Errno::EPERM));
}

#[test]
fn an_entry_needs_a_placement_and_neither_a_root_nor_a_check() {
    let invalid = |command: Command| {
        let error = command.spawn().unwrap_err();
        assert_eq!(error.raw_os_error(), Some(Errno::EINVAL as i32));
    };
    let prepared = || simulated(LaunchStage::Credentials, Errno::EPERM);
    invalid(shell("exit 0").enter(prepared()));
    let root = std::fs::File::open("/").unwrap();
    invalid(shell("exit 0").barrier().root(root).enter(prepared()));
    invalid(
        shell("exit 0")
            .barrier()
            .working_directory(c"/".to_owned())
            .check_working_directory_as(1, 1)
            .enter(prepared()),
    );
    // A cgroup places the child as well as a barrier.
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("cgroup.procs"), b"").unwrap();
    let cgroup = crate::guest::spawn::Cgroup::open(directory.path()).unwrap();
    let spawned = shell("exit 0")
        .cgroup(cgroup)
        .enter(prepared())
        .spawn()
        .unwrap();
    let process = spawned.process();
    assert_eq!(spawned.into_report().wait().unwrap(), None);
    assert_eq!(sys::wait(process), Some(ChildStatus::Exited(0)));
    let written = std::fs::read_to_string(directory.path().join("cgroup.procs")).unwrap();
    assert_eq!(written, format!("{}\n", process.raw()));
}

/// A probe that prints what an entered program sees of its identity and view, one `name=value`
/// line each.
const PROBE: &str = r#"
field() {
    while IFS= read -r line; do
        case "$line" in "$1:"*) set -- ${line#*:}; printf '%s' "$*"; return ;; esac
    done </proc/self/status
}
echo "ids=$(id -u) $(id -g) $(id -G)"
for set in CapInh CapPrm CapEff CapBnd CapAmb NoNewPrivs; do echo "$set=$(field $set)"; done
echo "pid=$$"
echo "hostname=$(cat /proc/sys/kernel/hostname)"
echo "machine-id=$(cat /etc/machine-id)"
echo "resolver=$(cat /etc/resolv.conf)"
echo "time=$(cat /run/nvx/time/state)"
echo "dev=$(cd /dev && echo *)"
echo "pts=$(ls -A /dev/pts)"
echo "first-process=$(cd /proc && set -- [0-9]* && echo "$1")"
while read -r _ _ _ _ point options _; do echo "mount=$point $options"; done </proc/self/mountinfo
if : >/dev/null; then echo "null=writable"; fi
if mknod /tmp/node c 1 3 2>/dev/null; then echo "mknod=allowed"; else echo "mknod=refused"; fi
if (echo 1023 >/proc/sys/net/ipv4/ip_unprivileged_port_start) 2>/dev/null; then
    echo "port-start=written"
else
    echo "port-start=refused"
fi
echo "kcore=$(wc -c 2>/dev/null </proc/kcore || echo unreadable)"
echo "firmware=$(ls -A /sys/firmware 2>/dev/null | tr '\n' ' ')"
echo "leaked=$(test -e /proc/self/fd/$LEAKED && echo open || echo closed)"
echo "root=$(ls -A / | tr '\n' ' ')"
if [ "$(id -u)" = 0 ] && command -v python3 >/dev/null; then
    python3 -c '
import os
os.makedirs("/tmp/jail", exist_ok=True)
try:
    os.chroot("/tmp/jail")
except PermissionError:
    print("escape=refused")
    raise SystemExit
for _ in range(16):
    os.chdir("..")
os.chroot(".")
print("escape=" + " ".join(sorted(os.listdir("/"))) + " ")
'
fi
"#;

/// Returns the values of the probe's lines named `name`, in their order.
fn values<'a>(output: &'a str, name: &str) -> Vec<&'a str> {
    output
        .lines()
        .filter_map(|line| line.strip_prefix(name)?.strip_prefix('='))
        .collect()
}

/// Returns the probe's one value named `name`.
fn value<'a>(output: &'a str, name: &str) -> &'a str {
    match values(output, name)[..] {
        [value] => value,
        ref found => panic!("{name}: {found:?} in\n{output}"),
    }
}

/// Returns the options of the one mount at `point` that the probe lists.
fn mount_options<'a>(output: &'a str, point: &str) -> Vec<&'a str> {
    let mounts: Vec<&str> = values(output, "mount")
        .into_iter()
        .filter_map(|mount| mount.strip_prefix(point)?.strip_prefix(' '))
        .collect();
    match mounts[..] {
        [options] => options.split(',').collect(),
        ref found => panic!("mounts at {point}: {found:?} in\n{output}"),
    }
}

/// Runs the probe as an execution of `entry`, with a descriptor of the caller's root leaked to
/// it, and returns its output.
fn probe(entry: &Entry) -> String {
    let leaked = nix::fcntl::open(
        c"/",
        OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY,
        Mode::empty(),
    )
    .unwrap();
    let number = std::os::fd::AsRawFd::as_raw_fd(&leaked);
    let (mut output, writer) = std::io::pipe().unwrap();
    let mut spawned = shell(PROBE)
        .environment(vec![
            c"PATH=/usr/sbin:/usr/bin:/sbin:/bin".to_owned(),
            c"LC_ALL=C".to_owned(),
            CString::new(format!("LEAKED={number}")).unwrap(),
        ])
        .stdout(writer)
        .barrier()
        .enter(entry.prepare().unwrap())
        .spawn()
        .unwrap();
    let process = spawned.process();
    spawned.take_barrier().unwrap().release().unwrap();
    let failure = spawned.into_report().wait().unwrap();
    let mut bytes = Vec::new();
    output.read_to_end(&mut bytes).unwrap();
    drop(leaked);
    let output = String::from_utf8(bytes).unwrap();
    assert_eq!(failure, None, "{output}");
    assert_eq!(sys::wait(process), Some(ChildStatus::Exited(0)), "{output}");
    output
}

/// Mounts a root for the privileged entries in `base`, a tmpfs that holds the account databases
/// and reaches the caller's programs through a read-only bind of `/usr`, and returns its path.
fn mount_test_root(base: &Path) -> PathBuf {
    let root = base.join("root");
    std::fs::create_dir(&root).unwrap();
    let path = |path: &Path| sys::path_c_string(path).unwrap();
    sys::mount(
        Some(c"tmpfs"),
        &path(&root),
        Some(c"tmpfs"),
        sys::MountFlags::empty(),
        Some(c"mode=0755"),
    )
    .unwrap();
    for directory in ["usr", "etc", "tmp"] {
        std::fs::create_dir(root.join(directory)).unwrap();
    }
    std::fs::set_permissions(root.join("tmp"), std::fs::Permissions::from_mode(0o1777)).unwrap();
    for (link, target) in [
        ("bin", "usr/bin"),
        ("sbin", "usr/sbin"),
        ("lib", "usr/lib"),
        ("lib64", "usr/lib64"),
    ] {
        std::os::unix::fs::symlink(target, root.join(link)).unwrap();
    }
    sys::mount(
        Some(c"/usr"),
        &path(&root.join("usr")),
        None,
        sys::MountFlags::MS_BIND | sys::MountFlags::MS_REC,
        None,
    )
    .unwrap();
    sys::mount(
        None,
        &path(&root.join("usr")),
        None,
        sys::MountFlags::MS_BIND | sys::MountFlags::MS_REMOUNT | sys::MountFlags::MS_RDONLY,
        None,
    )
    .unwrap();
    std::fs::write(root.join("etc/passwd"), PASSWD_DATABASE).unwrap();
    std::fs::write(root.join("etc/group"), GROUP_DATABASE).unwrap();
    root
}

/// Checks what a workload of `entry`, which `output` probed, sees of the view that every
/// identity shares.
fn check_view(output: &str, machine_id: &str) {
    assert_eq!(value(output, "pid"), "1", "{output}");
    assert_eq!(value(output, "first-process"), "1", "{output}");
    assert_eq!(value(output, "NoNewPrivs"), "1", "{output}");
    assert_eq!(value(output, "CapInh"), "0000000000000000", "{output}");
    assert_eq!(value(output, "CapAmb"), "0000000000000000", "{output}");
    assert_eq!(value(output, "hostname"), "entry-test", "{output}");
    assert_eq!(value(output, "machine-id"), machine_id, "{output}");
    assert_eq!(value(output, "resolver"), "nameserver 10.0.0.1", "{output}");
    assert_eq!(value(output, "time"), "time-state", "{output}");
    assert_eq!(
        value(output, "dev"),
        "fd full null ptmx pts random shm stderr stdin stdout tty urandom zero"
    );
    assert_eq!(value(output, "pts"), "ptmx");
    assert_eq!(value(output, "null"), "writable");
    assert_eq!(value(output, "mknod"), "refused");
    assert_eq!(value(output, "port-start"), "refused");
    assert_eq!(value(output, "leaked"), "closed");
    assert_eq!(
        value(output, "root"),
        "bin dev etc lib lib64 proc run sbin sys tmp usr "
    );
    for (point, required) in [
        ("/", &["nosuid", "nodev"][..]),
        ("/usr", &["ro", "nosuid", "nodev"]),
        ("/proc", &["nosuid", "nodev", "noexec"]),
        ("/sys", &["ro", "nosuid", "nodev", "noexec"]),
        ("/dev", &["nosuid"]),
        ("/dev/pts", &["nosuid", "noexec"]),
        ("/dev/shm", &["nosuid", "nodev"]),
        ("/etc/machine-id", &["ro", "nosuid", "nodev"]),
        ("/etc/resolv.conf", &["ro", "nosuid", "nodev"]),
        ("/run/nvx/time", &["ro", "nosuid", "nodev"]),
    ] {
        let options = mount_options(output, point);
        for option in required {
            assert!(
                options.contains(option),
                "{point} lacks {option}: {options:?}"
            );
        }
    }
    // Only `/dev` and its devpts may hold devices.
    for mount in values(output, "mount") {
        let (point, options) = mount.split_once(' ').unwrap();
        if !matches!(point, "/dev" | "/dev/pts") {
            assert!(
                options.split(',').any(|option| option == "nodev"),
                "{mount}"
            );
        }
    }
}

/// Returns the mount points below `path`, not `path` itself, in the caller's mount namespace.
fn mounts_below(path: &Path) -> Vec<PathBuf> {
    let table = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    table
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(PathBuf::from)
        .filter(|point| point.starts_with(path) && point != path)
        .collect()
}

/// Checks that [`Entry::bind_runtime`] refuses what it cannot bind into `root`, the privileged
/// entries' mounted root, from the sources in `base`, and leaves the root as it was.
fn check_runtime_binds(base: &Path, root: &Path) {
    let mounts = mounts_below(root);
    let entry = || Entry::new(root, Identity::new(0, 0).account(AccountMode::Numeric));
    let generated = |path: PathBuf| {
        entry()
            .machine_id(MachineId::Generated(path))
            .bind_runtime()
    };
    let result = generated(base.join("missing"));
    assert!(
        matches!(
            &result,
            Err(Error::MachineId(MappingError::System { step: Step::Source, error }))
                if *error == Errno::ENOENT as i32
        ),
        "{result:?}"
    );
    let result = generated(base.join("time"));
    assert!(
        matches!(result, Err(Error::MachineId(MappingError::SourceKind))),
        "{result:?}"
    );
    let result = entry().time_status(base.join("machine-id")).bind_runtime();
    assert!(
        matches!(result, Err(Error::TimeStatus(MappingError::SourceKind))),
        "{result:?}"
    );
    let result = entry().resolver(base.join("time")).bind_runtime();
    assert!(
        matches!(result, Err(Error::Resolver(MappingError::SourceKind))),
        "{result:?}"
    );
    // A target that the image holds as another kind, or below a link, is refused.
    std::fs::create_dir(root.join("etc/machine-id")).unwrap();
    let result = generated(base.join("machine-id"));
    assert!(
        matches!(result, Err(Error::MachineId(MappingError::TargetKind))),
        "{result:?}"
    );
    std::fs::remove_dir(root.join("etc/machine-id")).unwrap();
    std::os::unix::fs::symlink(base, root.join("run")).unwrap();
    let result = entry().time_status(base.join("time")).bind_runtime();
    assert!(
        matches!(&result, Err(Error::TimeStatus(MappingError::Link { path }))
            if path == Path::new("/run")),
        "{result:?}"
    );
    std::fs::remove_file(root.join("run")).unwrap();
    // A failure after the first binds removes them.
    std::fs::create_dir(root.join("etc/resolv.conf")).unwrap();
    let result = entry()
        .machine_id(MachineId::Generated(base.join("machine-id")))
        .time_status(base.join("time"))
        .resolver(base.join("resolv.conf"))
        .bind_runtime();
    assert!(
        matches!(result, Err(Error::Resolver(MappingError::TargetKind))),
        "{result:?}"
    );
    std::fs::remove_dir(root.join("etc/resolv.conf")).unwrap();
    assert_eq!(mounts_below(root), mounts);
    // What the walk created stays, but for the machine ID's file, which the caller's test puts a
    // link in place of.
    std::fs::remove_file(root.join("etc/machine-id")).unwrap();
}

/// Checks that an execution of `entry`, whose root is the privileged entries' mounted `root`,
/// enters its working directory, by default `/`, through no magic link: neither a `/proc` link to
/// a descriptor, such as one of the caller's root that leaks to the child or one that the view's
/// stages opened, a working directory, or a root, nor a symbolic link to one, which could name a
/// directory outside the root.
fn check_working_directories(entry: &Entry, root: &Path) {
    let run = |directory: Option<&str>| {
        let command = shell("pwd -P");
        let command = match directory {
            Some(directory) => command.working_directory(CString::new(directory).unwrap()),
            None => command,
        };
        run_released(command.enter(entry.prepare().unwrap()))
    };
    for (directory, expected) in [(None, "/\n"), (Some("/tmp"), "/tmp\n")] {
        let (failure, status, output) = run(directory);
        assert_eq!(
            (failure, status),
            (None, Some(ChildStatus::Exited(0))),
            "{directory:?}"
        );
        assert_eq!(String::from_utf8(output).unwrap(), expected);
    }
    let leaked = nix::fcntl::open(
        c"/",
        OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY,
        Mode::empty(),
    )
    .unwrap();
    let number = std::os::fd::AsRawFd::as_raw_fd(&leaked);
    std::os::unix::fs::symlink("/proc/self/cwd", root.join("tmp/cwd-link")).unwrap();
    let mut refused: Vec<(String, &[Errno])> = vec![
        (format!("/proc/self/fd/{number}"), &[Errno::ENOENT][..]),
        ("/proc/self/cwd".to_owned(), &[Errno::ELOOP][..]),
        ("/proc/self/root".to_owned(), &[Errno::ELOOP][..]),
        ("/proc/1/cwd".to_owned(), &[Errno::ELOOP][..]),
        ("/tmp/cwd-link".to_owned(), &[Errno::ELOOP][..]),
    ];
    // The child closes every other descriptor first, so a link names a standard stream or the
    // launch report's writer, or nothing.
    refused.extend((0..64).map(|number| {
        (
            format!("/proc/self/fd/{number}"),
            &[Errno::ELOOP, Errno::ENOENT][..],
        )
    }));
    for (directory, errors) in refused {
        let (failure, status, output) = run(Some(&directory));
        let failure = failure.unwrap_or_else(|| panic!("{directory} was entered: {output:?}"));
        assert_eq!(
            failure.stage(),
            LaunchStage::WorkingDirectory,
            "{directory}"
        );
        assert!(
            errors
                .iter()
                .any(|error| failure.raw_os_error() == *error as i32),
            "{directory}: {failure}"
        );
        assert_eq!(status, Some(ChildStatus::Exited(125)), "{directory}");
        assert!(output.is_empty(), "{directory}");
    }
    std::fs::remove_file(root.join("tmp/cwd-link")).unwrap();
    drop(leaked);
}

/// Checks that an execution of `entry`, whose root is the privileged entries' mounted `root`,
/// looks for its program in the directories of its command's search path in that root, and for
/// an empty one in the working directory that it entered.
fn check_search(entry: &Entry, root: &Path) {
    std::fs::create_dir(root.join("tmp/only-inside")).unwrap();
    std::os::unix::fs::symlink("/bin/sh", root.join("tmp/only-inside/inside-shell")).unwrap();
    for (path, directory) in [
        (c"/missing:/tmp/only-inside", None),
        (c"/missing:", Some(c"/tmp/only-inside")),
    ] {
        let command = Command::new(
            c"inside-shell".to_owned(),
            vec![
                c"inside-shell".to_owned(),
                c"-c".to_owned(),
                c"printf found".to_owned(),
            ],
        )
        .search_path(path.to_owned());
        let command = match directory {
            Some(directory) => command.working_directory(directory.to_owned()),
            None => command,
        };
        let (failure, status, output) = run_released(command.enter(entry.prepare().unwrap()));
        assert_eq!(
            (failure, status),
            (None, Some(ChildStatus::Exited(0))),
            "{path:?}"
        );
        assert_eq!(output, b"found", "{path:?}");
    }
    std::fs::remove_dir_all(root.join("tmp/only-inside")).unwrap();
}

#[test]
fn privileged_entries_see_their_own_view_and_identity() {
    if crate::guest::managed_agent::isolate_process_test(
        "NVX_ENTRY_PRIVILEGED_TEST_CHILD",
        "guest::entry::tests::privileged_entries_see_their_own_view_and_identity",
    ) {
        return;
    }
    if !sys::holds_capability(sys::CAP_SYS_ADMIN).unwrap() {
        return;
    }
    // The test mounts its root in a mount namespace of its own, which only a process with one
    // thread may enter, so a child of the harness's process makes the calls.
    match sys::fork_isolated_test().unwrap() {
        sys::ForkResult::Child => {
            let outcome = std::panic::catch_unwind(|| {
                sys::enter_private_mount_namespace().unwrap();
                let base = tempfile::tempdir().unwrap();
                let root = mount_test_root(base.path());
                let machine_id = MachineId::generate(base.path().join("machine-id")).unwrap();
                let id = std::fs::read_to_string(base.path().join("machine-id")).unwrap();
                std::fs::create_dir(base.path().join("time")).unwrap();
                std::fs::write(base.path().join("time/state"), "time-state\n").unwrap();
                let resolver = std::fs::File::create(base.path().join("resolv.conf")).unwrap();
                write_resolver(resolver, "10.0.0.1".parse().unwrap()).unwrap();
                let entry = |identity: Identity| {
                    Entry::new(&root, identity)
                        .hostname("entry-test")
                        .machine_id(machine_id.clone())
                        .time_status(base.path().join("time"))
                        .resolver(base.path().join("resolv.conf"))
                };
                check_runtime_binds(base.path(), &root);
                // The image's links at the machine ID and at the resolver file, which no bind
                // covers, give way to empty files, and their targets are left alone.
                std::fs::write(base.path().join("outside"), "outside\n").unwrap();
                std::os::unix::fs::symlink(
                    base.path().join("outside"),
                    root.join("etc/machine-id"),
                )
                .unwrap();
                std::os::unix::fs::symlink(
                    base.path().join("outside"),
                    root.join("etc/resolv.conf"),
                )
                .unwrap();
                entry(Identity::new(65534, 65534)).bind_runtime().unwrap();
                assert_eq!(
                    std::fs::read_to_string(root.join("etc/machine-id")).unwrap(),
                    id
                );
                assert_eq!(
                    std::fs::read_to_string(root.join("etc/resolv.conf")).unwrap(),
                    "nameserver 10.0.0.1\n"
                );
                assert_eq!(
                    std::fs::read_to_string(base.path().join("outside")).unwrap(),
                    "outside\n"
                );

                // The non-root profile, with its existing account.
                let output = probe(&entry(Identity::new(65534, 65534)));
                check_view(&output, id.trim_end());
                assert_eq!(value(&output, "ids"), "65534 65534 65534");
                for set in ["CapPrm", "CapEff", "CapBnd"] {
                    assert_eq!(value(&output, set), "0000000000000000", "{set}");
                }
                assert_eq!(value(&output, "kcore"), "unreadable");

                // A machine ID that the guest rewrites in place, as after a restore, is what the
                // later executions see.
                let id = "0123456789abcdef0123456789abcdef\n";
                std::fs::write(base.path().join("machine-id"), id).unwrap();

                // Root, with three allowlisted capabilities and its masked and read-only proc.
                let capabilities = Capabilities::EMPTY
                    .with(Capability::CHOWN)
                    .with(Capability::KILL)
                    .with(Capability::SYS_CHROOT);
                let privileged = entry(
                    Identity::new(0, 0)
                        .capabilities(capabilities)
                        .account(AccountMode::Numeric),
                )
                .publication(Publication::Unpublished);
                let output = probe(&privileged);
                check_view(&output, id.trim_end());
                assert_eq!(value(&output, "ids"), "0 0 0");
                let mask = format!("{:016x}", capabilities.bits());
                for set in ["CapPrm", "CapEff", "CapBnd"] {
                    assert_eq!(value(&output, set), mask, "{set}");
                }
                assert_eq!(value(&output, "kcore"), "0");
                assert_eq!(value(&output, "firmware"), "");
                for point in [
                    "/proc/sys",
                    "/proc/sysrq-trigger",
                    "/proc/kcore",
                    "/sys/firmware",
                ] {
                    assert!(mount_options(&output, point).contains(&"ro"), "{point}");
                }
                // A nested `chroot` and `..` from it land at the top of the root, never above.
                for escape in values(&output, "escape") {
                    assert_eq!(escape, value(&output, "root"));
                }
                check_working_directories(&privileged, &root);
                check_search(&privileged, &root);
                check_signals(privileged.prepare().unwrap());

                // Root with no capability.
                let output = probe(&entry(Identity::new(0, 0).account(AccountMode::Numeric)));
                for set in ["CapPrm", "CapEff", "CapBnd"] {
                    assert_eq!(value(&output, set), "0000000000000000", "{set}");
                }
                assert_eq!(value(&output, "port-start"), "refused");
                assert_eq!(value(&output, "mknod"), "refused");
                for escape in values(&output, "escape") {
                    assert_eq!(escape, "refused");
                }
            });
            sys::exit_immediately(u8::from(outcome.is_err()));
        }
        sys::ForkResult::Parent(child) => {
            assert_eq!(sys::wait(child), Some(ChildStatus::Exited(0)));
        }
    }
}

#[test]
fn privileged_runtime_binds_are_removed_before_a_teardown() {
    if crate::guest::managed_agent::isolate_process_test(
        "NVX_ENTRY_TEARDOWN_TEST_CHILD",
        "guest::entry::tests::privileged_runtime_binds_are_removed_before_a_teardown",
    ) {
        return;
    }
    if !sys::holds_capability(sys::CAP_SYS_ADMIN).unwrap() {
        return;
    }
    // As for the entries' own test, a child of the harness's process makes the calls.
    match sys::fork_isolated_test().unwrap() {
        sys::ForkResult::Child => {
            let outcome = std::panic::catch_unwind(|| {
                use nix::sys::statvfs::{FsFlags, statvfs};

                sys::enter_private_mount_namespace().unwrap();
                let directory = tempfile::tempdir().unwrap();
                let base = directory.path().canonicalize().unwrap();
                // A root that is a mount of its own, as a layered root's overlay is, whose
                // resolver file is a link, as a resolver daemon's image has.
                let root = base.join("root");
                std::fs::create_dir(&root).unwrap();
                let point = sys::path_c_string(&root).unwrap();
                sys::mount(
                    Some(c"tmpfs"),
                    &point,
                    Some(c"tmpfs"),
                    sys::MountFlags::empty(),
                    Some(c"mode=0755"),
                )
                .unwrap();
                std::fs::create_dir(root.join("etc")).unwrap();
                std::os::unix::fs::symlink(
                    "../run/resolver/resolv.conf",
                    root.join("etc/resolv.conf"),
                )
                .unwrap();
                let machine_id = MachineId::generate(base.join("machine-id")).unwrap();
                std::fs::create_dir(base.join("time")).unwrap();
                let resolver = std::fs::File::create(base.join("resolv.conf")).unwrap();
                write_resolver(resolver, "fd00::a00:1".parse().unwrap()).unwrap();
                let binds = Entry::new(&root, Identity::new(0, 0).account(AccountMode::Numeric))
                    .machine_id(machine_id)
                    .time_status(base.join("time"))
                    .resolver(base.join("resolv.conf"))
                    .bind_runtime()
                    .unwrap();
                // Each bind is read-only, `nosuid`, and `nodev`.
                let targets = ["etc/machine-id", "run/nvx/time", "etc/resolv.conf"]
                    .map(|target| root.join(target));
                assert_eq!(mounts_below(&root), targets);
                for target in &targets {
                    let flags = statvfs(target).unwrap().flags();
                    assert!(
                        flags.contains(FsFlags::ST_RDONLY | FsFlags::ST_NOSUID | FsFlags::ST_NODEV),
                        "{}: {flags:?}",
                        target.display()
                    );
                }
                assert_eq!(
                    std::fs::read_to_string(root.join("etc/resolv.conf")).unwrap(),
                    "nameserver fd00::a00:1\n"
                );
                // A teardown that finds them fails, since it detaches no busy mount.
                assert_eq!(sys::unmount(&point), Err(Errno::EBUSY));
                binds.remove().unwrap();
                assert!(mounts_below(&root).is_empty());
                // The image's link gave way to an empty file, which stays.
                let replaced = std::fs::symlink_metadata(root.join("etc/resolv.conf")).unwrap();
                assert!(replaced.is_file() && replaced.len() == 0);
                // Once they are removed, the teardown succeeds.
                sys::unmount(&point).unwrap();
            });
            sys::exit_immediately(u8::from(outcome.is_err()));
        }
        sys::ForkResult::Parent(child) => {
            assert_eq!(sys::wait(child), Some(ChildStatus::Exited(0)));
        }
    }
}
