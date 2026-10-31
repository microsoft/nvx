//! The initramfs that boots the guest agent is a copy of NVX's initramfs, with an archive after it
//! that Linux unpacks over NVX's: the guest agent, the scripts that run it, and the links that
//! replace NVX's managed agent and init agent with the scripts, and for the capture of a template
//! whose clones delay their repair, for tests, the delay's file.
#![cfg(feature = "host")]

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use runtime_adapter::initramfs::{
    self, CONTROL_TTY, GUEST_AGENT, INIT_AGENT, LAUNCHER, LAYERED_LAUNCHER, MANAGED_AGENT,
};
use runtime_adapter::template::{REPAIR_DELAY_FILE, parse_repair_delay};

/// An entry of a `newc` archive.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    /// The entry's name, relative to the root.
    name: String,
    /// The inode number.
    inode: u32,
    /// The type and permission bits.
    mode: u32,
    /// The owner, the group, and the link count.
    owner: (u32, u32, u32),
    /// The data: a file's contents, or a link's target.
    data: Vec<u8>,
}

/// The length of a `newc` header.
const HEADER_LEN: usize = 110;

/// Parses the `newc` archive `bytes`, which must end with its trailer and the trailer's padding,
/// and checks that its headers and data start at multiples of 4 bytes, and that its padding is
/// zeros. Returns its entries before the trailer.
fn parse(bytes: &[u8]) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut offset = 0;
    loop {
        assert_eq!(offset % 4, 0, "a header at {offset}");
        let header = &bytes[offset..offset + HEADER_LEN];
        assert_eq!(&header[..6], b"070701", "the magic number at {offset}");
        let field = |index: usize| {
            let digits = &header[6 + 8 * index..14 + 8 * index];
            assert!(
                digits
                    .iter()
                    .all(|digit| digit.is_ascii_digit() || (b'A'..=b'F').contains(digit)),
                "{digits:?}"
            );
            u32::from_str_radix(std::str::from_utf8(digits).unwrap(), 16).unwrap()
        };
        // The device numbers and the checksum are zero.
        for index in [7, 8, 9, 10, 12] {
            assert_eq!(field(index), 0, "field {index} at {offset}");
        }
        let name_start = offset + HEADER_LEN;
        let name_end = name_start + field(11) as usize;
        assert_eq!(bytes[name_end - 1], 0, "the name's NUL at {offset}");
        let name = String::from_utf8(bytes[name_start..name_end - 1].to_vec()).unwrap();
        let data_start = name_end.next_multiple_of(4);
        let data_end = data_start + field(6) as usize;
        let end = data_end.next_multiple_of(4);
        assert!(bytes[name_end..data_start].iter().all(|&byte| byte == 0));
        assert!(bytes[data_end..end].iter().all(|&byte| byte == 0));
        if name == "TRAILER!!!" {
            assert_eq!(end, bytes.len(), "the trailer ends the archive");
            return entries;
        }
        entries.push(Entry {
            name,
            inode: field(0),
            mode: field(1),
            owner: (field(2), field(3), field(4)),
            data: bytes[data_start..data_end].to_vec(),
        });
        offset = end;
    }
}

/// Returns the entries that the archive with the guest agent `agent` holds.
fn expected(agent: &[u8]) -> Vec<Entry> {
    let entry = |name: &str, inode, mode, data: &[u8]| Entry {
        name: name.trim_start_matches('/').to_owned(),
        inode,
        mode,
        owner: (0, 0, 1),
        data: data.to_vec(),
    };
    vec![
        entry(GUEST_AGENT, 1, 0o100_755, agent),
        entry(
            LAUNCHER,
            2,
            0o100_755,
            initramfs::launcher_script().as_bytes(),
        ),
        entry(MANAGED_AGENT, 3, 0o120_777, LAUNCHER.as_bytes()),
        entry(
            LAYERED_LAUNCHER,
            4,
            0o100_755,
            initramfs::layered_launcher_script().as_bytes(),
        ),
        entry(INIT_AGENT, 5, 0o120_777, LAYERED_LAUNCHER.as_bytes()),
    ]
}

#[test]
fn the_archive_replaces_nvx_s_agents_with_links_to_the_guest_agent_launchers() {
    // Guest agents of every length modulo 4 pad their data differently.
    for length in 0..8 {
        let agent = (1..=length).collect::<Vec<u8>>();
        let archive = initramfs::archive(&agent, None).unwrap();
        assert_eq!(
            parse(&archive),
            expected(&agent),
            "a guest agent of {length} bytes"
        );
    }
}

#[test]
fn a_template_s_archive_holds_the_delay_of_its_clones_repair_after_the_launchers() {
    // Delays of every length modulo 4 pad their data differently.
    for milliseconds in [1, 10, 100, 1_000, 30_000] {
        let delay = Duration::from_millis(milliseconds);
        let archive = initramfs::archive(b"guest agent", Some(delay)).unwrap();
        let mut entries = expected(b"guest agent");
        let contents = format!("{milliseconds}\n");
        entries.push(Entry {
            name: REPAIR_DELAY_FILE.trim_start_matches('/').to_owned(),
            inode: 6,
            mode: 0o100_644,
            owner: (0, 0, 1),
            data: contents.clone().into_bytes(),
        });
        assert_eq!(parse(&archive), entries, "{delay:?}");
        assert_eq!(parse_repair_delay(contents.as_bytes()), Some(delay));
    }
}

#[test]
fn the_launchers_run_the_guest_agent_on_the_control_tty() {
    // `/init` passes the managed agent the control tty first.
    let init = runtime_adapter::GUEST_INIT_COMMAND;
    assert_eq!(
        initramfs::launcher_script(),
        format!("#!/bin/sh\nexec {GUEST_AGENT} {init} \"$1\"\n")
    );
    assert_eq!(MANAGED_AGENT, "/sbin/nvx-managed-agent");

    // It passes NVX's init agent nothing, so the layered launcher names the control tty, which
    // `/init` requires of both lifecycles.
    let layered = runtime_adapter::GUEST_LAYERED_MODE;
    assert_eq!(
        initramfs::layered_launcher_script(),
        format!("#!/bin/sh\nexec {GUEST_AGENT} {init} /dev/hvc2 {layered}\n")
    );
    assert_eq!(CONTROL_TTY, "/dev/hvc2");
    assert_eq!(INIT_AGENT, "/sbin/nvx-init-agent");
}

#[test]
fn the_initramfs_is_a_copy_of_nvx_s_with_the_archive_at_a_multiple_of_4_bytes() {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("initramfs");
    fs::create_dir_all(&directory).unwrap();
    let (base, agent, output) = (
        directory.join("base.cpio.gz"),
        directory.join("runtime-adapter-guest"),
        directory.join("output.cpio.gz"),
    );
    fs::write(&agent, b"guest agent").unwrap();
    let archive = initramfs::archive(b"guest agent", None).unwrap();
    // An earlier, longer output is replaced.
    fs::write(&output, vec![0xee; 4096]).unwrap();
    for length in 0..8 {
        let contents = vec![0xab; length];
        fs::write(&base, &contents).unwrap();
        initramfs::write(&base, &agent, None, &output).unwrap();
        let written = fs::read(&output).unwrap();
        let start = length.next_multiple_of(4);
        assert_eq!(&written[..length], contents, "a base of {length} bytes");
        assert!(written[length..start].iter().all(|&byte| byte == 0));
        assert_eq!(&written[start..], archive, "a base of {length} bytes");
    }
    // NVX's initramfs stays as it is.
    assert_eq!(fs::read(&base).unwrap(), vec![0xab; 7]);
    // A template's initramfs appends the archive with the delay of its clones' repair.
    let delay = Some(Duration::from_secs(8));
    initramfs::write(&base, &agent, delay, &output).unwrap();
    let written = fs::read(&output).unwrap();
    let archive = initramfs::archive(b"guest agent", delay).unwrap();
    assert_eq!(&written[8..], archive);

    // A failure names the file.
    for (base, agent) in [
        (&directory.join("missing"), &agent),
        (&base, &directory.join("missing")),
    ] {
        let error = initramfs::write(base, agent, None, &output).unwrap_err();
        let missing = directory.join("missing").display().to_string();
        assert!(error.to_string().contains(&missing), "{error}");
    }
}
