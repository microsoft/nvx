//! Tests of the layered root's command line, account lookup, and console lines, which need no
//! privileges.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use super::account::{self, Account};
use super::boot::Boot;
use super::{RUNTIME, cgroup_line, machine_id, share_line};
use crate::guest::root::{AGGREGATE_DIRECTORY, Resources};
use crate::protocol::share::{AGGREGATE_MOUNT_TARGET, Child, Shares};
use crate::uuid::Uuid;

/// The UUID of the base command line's image.
const IMAGE_UUID: &str = "0123abcd-4567-89ef-0123-456789abcdef";

/// The command line of a managed sandbox without live shares, as the host program and the VMM
/// write it.
const BASE: &str = "console=hvc0 quiet nvx_sandbox=1 \
                    nvx_layer=distro,0xd0003000,erofs:0123abcd-4567-89ef-0123-456789abcdef \
                    nvx_scratch=0xd0006000,ext4 nvx_entrypoint=/bin/sh nvx_hostname=box \
                    nvx_memory_max=268435456 nvx_pids_max=9 nvx_workload_uid=1000 \
                    nvx_workload_gid=1001 nvx_lifecycle=managed nvx_control_tty=hvc2";

/// Returns the base command line with `changes`: each `NAME=VALUE` replaces the base's tokens
/// named `NAME`, and each `NAME` alone removes them.
fn with(changes: &[&str]) -> String {
    let name = |token: &str| token.split('=').next().unwrap_or_default().to_owned();
    let mut tokens: Vec<&str> = BASE
        .split_whitespace()
        .filter(|token| !changes.iter().any(|change| name(change) == name(token)))
        .collect();
    tokens.extend(changes.iter().filter(|change| change.contains('=')));
    tokens.join(" ")
}

/// Reads `command_line`.
fn parse(command_line: &str) -> Result<Boot<'_>, String> {
    Boot::parse(command_line.as_bytes())
}

#[test]
fn a_managed_command_line_is_read_whole() {
    let command_line = format!("{BASE}\n");
    let boot = parse(&command_line).unwrap();
    assert_eq!(
        boot,
        Boot {
            resources: Resources {
                agent_memory_low: Some(16 << 20),
                workload_memory_max: Some(268_435_456),
                workload_pids_max: Some(9),
            },
            image_uuid: Uuid::parse(IMAGE_UUID),
            layer_sectors: None,
            hostname: c"box".to_owned(),
            uid: b"1000".to_vec(),
            gid: b"1001".to_vec(),
            shares: Shares::None,
        }
    );
}

#[test]
fn missing_and_empty_values_take_their_defaults_and_the_first_token_counts() {
    let command_line = with(&[
        "nvx_hostname=",
        "nvx_memory_max",
        "nvx_pids_max=",
        "nvx_layer=distro,0xd0003000,erofs",
        "nvx_agent_memory_low=4096",
        "nvx_layer_capacity_sectors=2048",
    ]);
    let boot = parse(&command_line).unwrap();
    assert_eq!(
        boot.resources,
        Resources {
            agent_memory_low: Some(4096),
            workload_memory_max: None,
            workload_pids_max: None,
        }
    );
    assert_eq!(boot.image_uuid, None);
    assert_eq!(boot.layer_sectors, Some(2048));
    assert_eq!(boot.hostname, c"nvx-sandbox".to_owned());

    // The first of a repeated token counts, as the shell init's lookup found it.
    let command_line = format!(
        "nvx_hostname=first nvx_workload_uid=2000 nvx_agent_memory_low=4096 {BASE} \
         nvx_agent_memory_low=1"
    );
    let boot = parse(&command_line).unwrap();
    assert_eq!(boot.hostname, c"first".to_owned());
    assert_eq!(boot.uid, b"2000");
    assert_eq!(boot.resources.agent_memory_low, Some(4096));

    // Tabs and newlines separate tokens too, and the managed lifecycle needs no entrypoint.
    let command_line = with(&["nvx_entrypoint"]).replace(' ', "\t\n");
    let boot = parse(&command_line).unwrap();
    assert_eq!(boot.uid, b"1000");
}

#[test]
fn invalid_values_are_refused_with_the_shell_init_s_messages() {
    for (changes, message) in [
        (
            &["nvx_agent_memory_low=16M"][..],
            "invalid agent memory reservation",
        ),
        (&["nvx_memory_max=max"], "invalid container memory limit"),
        (&["nvx_pids_max=-1"], "invalid container process limit"),
        (
            &["nvx_layer_capacity_sectors=0x10"],
            "the nvx_layer_capacity_sectors kernel parameter must be a decimal number of \
             sectors, not '0x10'",
        ),
        (
            &["nvx_entrypoint=bin/sh"],
            "entrypoint must be an absolute path",
        ),
        (&["nvx_workload_uid"], "workload UID is unavailable"),
        (&["nvx_workload_uid="], "workload UID is unavailable"),
        (&["nvx_workload_uid=0"], "workload UID is unavailable"),
        (&["nvx_workload_uid=+1"], "workload UID is unavailable"),
        (&["nvx_workload_gid=0"], "workload GID is unavailable"),
        (&["nvx_workload_gid=1x"], "workload GID is unavailable"),
        (
            &["nvx_control_tty"],
            "managed lifecycle requires the dedicated control tty",
        ),
        (
            &["nvx_control_tty=hvc0"],
            "managed lifecycle requires the dedicated control tty",
        ),
    ] {
        assert_eq!(parse(&with(changes)).unwrap_err(), message, "{changes:?}");
    }
}

#[test]
fn only_an_erofs_image_over_the_scratch_is_assembled() {
    for (changes, message) in [
        (
            &["nvx_layer", "nvx_scratch"][..],
            "the command line names no distro layer: this init needs \
             nvx_layer=distro,0xd0003000,erofs[:UUID]"
                .to_owned(),
        ),
        (
            &["nvx_layer=distro,0xd0003000,gpt"],
            "nvx_layer=distro,0xd0003000,gpt names a GPT image, which this init cannot \
             assemble: it assembles only an EROFS image over a scratch disk"
                .to_owned(),
        ),
        (
            &["nvx_scratch", "nvx_overlay_upper=ramfs"],
            "nvx_overlay_upper=ramfs asks for a RAM upper, which this init cannot assemble: it \
             assembles only an EROFS image over a scratch disk"
                .to_owned(),
        ),
        // The boot tokens' own rules come from their contract, which refuses the form without a
        // format.
        (
            &["nvx_layer=distro,0xd0003000,0123abcd-4567-89ef-0123-456789abcdef"],
            format!(
                "the nvx_layer kernel parameter names no image format, erofs or gpt: \
                 'distro,0xd0003000,{IMAGE_UUID}'"
            ),
        ),
        (
            &["nvx_scratch"],
            "the nvx_layer kernel parameter needs an upper, nvx_scratch or nvx_overlay_upper"
                .to_owned(),
        ),
    ] {
        assert_eq!(parse(&with(changes)).unwrap_err(), message, "{changes:?}");
    }
}

#[test]
fn the_live_shares_are_read_last_with_the_share_protocol() {
    let command_line = with(&[
        "virtfs_dir=/run/nvx/shares",
        "virtfs_tag=microvm",
        "virtfs_mode=rw",
        "virtfs_aggregate=1",
        "nvx_share=0-a,/workspace,rw",
        "nvx_share=1-b,/etc/app/settings.json,ro,file",
    ]);
    assert_eq!(
        parse(&command_line).unwrap().shares,
        Shares::Aggregate {
            tag: "microvm",
            read_only: false,
            children: vec![
                Child {
                    name: "0-a",
                    target: b"/workspace",
                    read_only: false,
                    file: false,
                },
                Child {
                    name: "1-b",
                    target: b"/etc/app/settings.json",
                    read_only: true,
                    file: true,
                },
            ],
        }
    );

    // The shares' refusal keeps its message, and comes after every other check.
    assert_eq!(
        parse(&with(&["virtfs_dir=/workspace"])).unwrap_err(),
        "live share bootstrap is incomplete"
    );
    assert_eq!(
        parse(&with(&["virtfs_dir=/workspace", "nvx_control_tty"])).unwrap_err(),
        "managed lifecycle requires the dedicated control tty"
    );
}

#[test]
fn the_root_mounts_the_aggregate_where_the_share_protocol_requires() {
    assert_eq!(
        Path::new(RUNTIME).join(AGGREGATE_DIRECTORY),
        Path::new(AGGREGATE_MOUNT_TARGET)
    );
}

/// A root with user and group databases, and the home directory of its workload account.
struct Root {
    /// The temporary directory of the root.
    directory: tempfile::TempDir,
}

impl Root {
    /// Returns a root whose user database holds `passwd` and whose group database `group`,
    /// with the directory `/home/user`.
    fn new(passwd: &str, group: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("etc")).unwrap();
        fs::create_dir_all(directory.path().join("home/user")).unwrap();
        fs::write(directory.path().join("etc/passwd"), passwd).unwrap();
        fs::write(directory.path().join("etc/group"), group).unwrap();
        Self { directory }
    }

    /// Returns `relative`'s path in the root.
    fn path(&self, relative: &str) -> PathBuf {
        self.directory.path().join(relative)
    }

    /// Looks up the account of `uid` and `gid` in the root.
    fn lookup(&self, uid: &str, gid: &str) -> Result<Account, &'static str> {
        account::lookup(self.directory.path(), uid.as_bytes(), gid.as_bytes())
    }
}

/// A user database with the root's account and the workload's.
const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\nuser:x:1000:1001::/home/user:/bin/sh\n";
/// A group database with the root's group and the workload's.
const GROUP: &str = "root:x:0:\nusers:x:1001:\n";

#[test]
fn the_workload_account_is_its_user_id_s_one_record() {
    let root = Root::new(PASSWD, GROUP);
    assert_eq!(
        root.lookup("1000", "1001").unwrap(),
        Account {
            name: b"user".to_vec(),
            home: b"/home/user".to_vec(),
        }
    );
    // The last record needs no newline, and other records may be malformed.
    let root = Root::new(
        "broken\n::\nuser:x:1000:1001::/home/user:/bin/sh",
        "users:x:1001:",
    );
    assert_eq!(root.lookup("1000", "1001").unwrap().name, b"user");
}

#[test]
fn accounts_are_refused_with_the_shell_init_s_messages() {
    for (passwd, group, uid, gid, message) in [
        // Decimal IDs compare as numbers, so a record with leading zeros matches, but its text
        // differs from the configured ID's.
        (
            "user:x:01000:1001::/home/user:\n",
            GROUP,
            "1000",
            "1001",
            "configured workload identity does not match the user database",
        ),
        (
            PASSWD,
            GROUP,
            "00",
            "0",
            "configured workload identity does not match the user database",
        ),
        (
            PASSWD,
            GROUP,
            "1000",
            "1002",
            "configured workload identity does not match the user database",
        ),
        (
            PASSWD,
            GROUP,
            "2000",
            "1001",
            "configured workload UID is unavailable",
        ),
        (
            "a:x:1000:1001::/home/user:\nb:x:1000:1001::/home/user:\n",
            GROUP,
            "1000",
            "1001",
            "configured workload UID is unavailable",
        ),
        (
            ":x:1000:1001::/home/user:\n",
            GROUP,
            "1000",
            "1001",
            "configured workload user record is invalid",
        ),
        (
            PASSWD,
            "root:x:0:\nusers:x:01002:\n",
            "1000",
            "1001",
            "configured workload GID is unavailable",
        ),
        (
            "user:x:1000:1001::home/user:\n",
            GROUP,
            "1000",
            "1001",
            "configured workload home is invalid",
        ),
        (
            "user:x:1000:1001::/home/missing:\n",
            GROUP,
            "1000",
            "1001",
            "configured workload home is unavailable",
        ),
        (
            "user:x:1000:1001:::\n",
            GROUP,
            "1000",
            "1001",
            "configured workload home is invalid",
        ),
    ] {
        let root = Root::new(passwd, group);
        assert_eq!(
            root.lookup(uid, gid).unwrap_err(),
            message,
            "{passwd:?} {group:?} {uid} {gid}"
        );
    }
}

#[test]
fn databases_and_homes_must_not_be_links() {
    let root = Root::new(PASSWD, GROUP);
    fs::remove_file(root.path("etc/passwd")).unwrap();
    assert_eq!(
        root.lookup("1000", "1001").unwrap_err(),
        "workload user database is unavailable"
    );
    fs::write(root.path("passwd"), PASSWD).unwrap();
    symlink("../passwd", root.path("etc/passwd")).unwrap();
    assert_eq!(
        root.lookup("1000", "1001").unwrap_err(),
        "workload user database is unavailable"
    );

    let root = Root::new(PASSWD, GROUP);
    fs::remove_file(root.path("etc/group")).unwrap();
    fs::create_dir(root.path("etc/group")).unwrap();
    assert_eq!(
        root.lookup("1000", "1001").unwrap_err(),
        "workload group database is unavailable"
    );

    let root = Root::new(PASSWD, GROUP);
    fs::remove_dir(root.path("home/user")).unwrap();
    fs::create_dir(root.path("elsewhere")).unwrap();
    symlink("../elsewhere", root.path("home/user")).unwrap();
    assert_eq!(
        root.lookup("1000", "1001").unwrap_err(),
        "configured workload home is unavailable"
    );
    fs::remove_file(root.path("home/user")).unwrap();
    fs::write(root.path("home/user"), "").unwrap();
    assert_eq!(
        root.lookup("1000", "1001").unwrap_err(),
        "configured workload home is unavailable"
    );
}

#[test]
fn console_lines_keep_the_shell_init_s_markers() {
    assert_eq!(
        cgroup_line(&[1, 57, 58]),
        b"NVX-SANDBOX-CGROUP: agent=1,57,58\n"
    );
    assert_eq!(
        share_line(b"/workspace", false, "tag", "microvm"),
        b"NVX-SANDBOX-SHARE: target=/workspace mode=rw tag=microvm\n"
    );
    assert_eq!(
        share_line(b"/etc/app.json", true, "child", "0-a"),
        b"NVX-SANDBOX-SHARE: target=/etc/app.json mode=ro child=0-a\n"
    );
    // The machine ID is the kernel's random UUID without its hyphens.
    assert_eq!(
        machine_id(b"0123abcd-4567-89ef-0123-456789abcdef\n"),
        b"0123abcd456789ef0123456789abcdef\n"
    );
}
