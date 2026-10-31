//! The initramfs of a VM that runs the adapter's guest agent in place of NVX's agents.
//!
//! NVX's initramfs boots through its `/init`, which hands a VM of the managed lifecycle to NVX's
//! managed agent, as PID 1, on the VM's control tty:
//!
//! ```text
//! exec /sbin/nvx-managed-agent /dev/hvc2 - nvx-managed UID GID NAME HOME
//! ```
//!
//! and a VM with a layered root, whose kernel command line has `nvx_sandbox=1`, to NVX's init
//! agent, which assembles the root, without arguments:
//!
//! ```text
//! exec /sbin/nvx-init-agent
//! ```
//!
//! [`write`](fn@write) copies that initramfs, and appends to the copy a second archive, in the
//! `newc` format, which Linux unpacks over the first:
//!
//! - the guest agent, at [`GUEST_AGENT`];
//! - a script, at [`LAUNCHER`], that runs the guest agent as `runtime-adapter-guest init TTY`,
//!   with the control tty, the first of the arguments that `/init` passes the managed agent;
//! - a symbolic link from [`MANAGED_AGENT`] to that script;
//! - a script, at [`LAYERED_LAUNCHER`], that runs the guest agent as
//!   `runtime-adapter-guest init /dev/hvc2 layered`, PID 1 of a VM with a layered root, which
//!   names the control tty, [`CONTROL_TTY`], itself, since `/init` passes NVX's init agent none;
//! - and a symbolic link from [`INIT_AGENT`] to that script, so that NVX's init agent never
//!   assembles the root in the adapter's VMs;
//! - for the capture of a template whose clones delay their repair, for tests, the delay's file,
//!   [`REPAIR_DELAY_FILE`], in the `/etc` directory of NVX's initramfs.
//!
//! Linux removes a file whose type an archive changes before it creates the new one, so each link
//! replaces NVX's agent without writing to its file, which could have other names.
//!
//! The second archive starts at a multiple of 4 bytes, as Linux requires of an uncompressed
//! archive, after the zeros that pad the copy, which Linux skips. NVX's initramfs itself stays as
//! it is.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use crate::template::{REPAIR_DELAY_FILE, repair_delay_contents};

/// Path of the guest agent in the VM.
pub const GUEST_AGENT: &str = "/sbin/runtime-adapter-guest";
/// Path of the script that runs the guest agent in place of NVX's managed agent.
pub const LAUNCHER: &str = "/sbin/runtime-adapter-init";
/// Path of NVX's managed agent, which `/init` runs.
pub const MANAGED_AGENT: &str = "/sbin/nvx-managed-agent";
/// Path of the script that runs the guest agent's PID 1 of a VM with a layered root, in place of
/// NVX's init agent.
pub const LAYERED_LAUNCHER: &str = "/sbin/runtime-adapter-layered";
/// Path of NVX's init agent, which `/init` runs in a VM with a layered root.
pub const INIT_AGENT: &str = "/sbin/nvx-init-agent";
/// The VM's control tty, which `/init` requires of both lifecycles that NVX's agents serve.
pub const CONTROL_TTY: &str = "/dev/hvc2";

/// Returns the script at [`LAUNCHER`], which runs the guest agent's
/// [`GUEST_INIT_COMMAND`](crate::GUEST_INIT_COMMAND) on the control tty, the first of the
/// arguments that `/init` passes the managed agent.
pub fn launcher_script() -> String {
    format!(
        "#!/bin/sh\nexec {GUEST_AGENT} {} \"$1\"\n",
        crate::GUEST_INIT_COMMAND
    )
}

/// Returns the script at [`LAYERED_LAUNCHER`], which runs the guest agent's
/// [`GUEST_INIT_COMMAND`](crate::GUEST_INIT_COMMAND) on [`CONTROL_TTY`], in its
/// [`GUEST_LAYERED_MODE`](crate::GUEST_LAYERED_MODE).
pub fn layered_launcher_script() -> String {
    format!(
        "#!/bin/sh\nexec {GUEST_AGENT} {} {CONTROL_TTY} {}\n",
        crate::GUEST_INIT_COMMAND,
        crate::GUEST_LAYERED_MODE,
    )
}

/// The magic number of a `newc` header.
const MAGIC: &[u8] = b"070701";
/// The name of the entry that ends an archive.
const TRAILER: &str = "TRAILER!!!";
/// The type bits of a regular file.
const REGULAR: u32 = 0o100_000;
/// The type bits of a symbolic link.
const SYMLINK: u32 = 0o120_000;

/// Writes to `output` a copy of NVX's initramfs `base` with the guest agent `guest_agent`, a
/// static `x86_64-unknown-linux-musl` build of `runtime-adapter-guest`, in place of NVX's managed
/// agent and init agent, and the file that delays the repair of each clone of a template by
/// `repair_delay`, if any, as the [module documentation](self) describes.
///
/// Fails if a file cannot be read or written, or if the guest agent does not fit an archive
/// entry, whose size has 32 bits. The error names the file.
pub fn write(
    base: &Path,
    guest_agent: &Path,
    repair_delay: Option<Duration>,
    output: &Path,
) -> io::Result<()> {
    let agent = fs::read(guest_agent).map_err(naming(guest_agent))?;
    let archive = archive(&agent, repair_delay)?;
    let mut file = File::create(output).map_err(naming(output))?;
    let mut source = File::open(base).map_err(naming(base))?;
    let length = io::copy(&mut source, &mut file).map_err(naming(output))?;
    let padding = length.next_multiple_of(4) - length;
    // `padding` is less than 4.
    file.write_all(&[0; 3][..padding as usize])
        .and_then(|()| file.write_all(&archive))
        .map_err(naming(output))
}

/// Returns a function that names `path` in the message of an error.
fn naming(path: &Path) -> impl Fn(io::Error) -> io::Error + '_ {
    move |error| io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

/// Returns the archive that [`write`](fn@write) appends, with the guest agent `guest_agent`, and
/// the file that delays the repair of each clone of a template by `repair_delay`, if any: every
/// entry that the [module documentation](self) lists, owned by root, then the trailer.
///
/// Fails if the guest agent does not fit an archive entry, whose size has 32 bits.
pub fn archive(guest_agent: &[u8], repair_delay: Option<Duration>) -> io::Result<Vec<u8>> {
    let script = launcher_script();
    let layered_script = layered_launcher_script();
    let delay = repair_delay.map(repair_delay_contents);
    let mut entries: Vec<(&str, u32, &[u8])> = vec![
        (GUEST_AGENT, REGULAR | 0o755, guest_agent),
        (LAUNCHER, REGULAR | 0o755, script.as_bytes()),
        // A link's data is its target.
        (MANAGED_AGENT, SYMLINK | 0o777, LAUNCHER.as_bytes()),
        (LAYERED_LAUNCHER, REGULAR | 0o755, layered_script.as_bytes()),
        (INIT_AGENT, SYMLINK | 0o777, LAYERED_LAUNCHER.as_bytes()),
    ];
    if let Some(delay) = &delay {
        entries.push((REPAIR_DELAY_FILE, REGULAR | 0o644, delay.as_bytes()));
    }
    let mut archive = Vec::new();
    for (inode, (path, mode, data)) in (1..).zip(entries) {
        // Archives name their entries relative to the root.
        push(&mut archive, inode, &path[1..], mode, data)?;
    }
    push(&mut archive, 0, TRAILER, 0, &[])?;
    Ok(archive)
}

/// Appends to `archive` the entry `name`, with the inode number `inode`, the mode `mode`, and the
/// data `data`, each padded to a multiple of 4 bytes.
fn push(archive: &mut Vec<u8>, inode: u32, name: &str, mode: u32, data: &[u8]) -> io::Result<()> {
    let size = u32::try_from(data.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} does not fit an archive entry, whose size has 32 bits"),
        )
    })?;
    // The name's size counts its terminating NUL.
    let name_size = u32::try_from(name.len() + 1).expect("a constant name is short");
    // The inode, mode, owner and group, link count, modification time, size, device numbers of
    // the file and of a device that it is, name size, and checksum, which `newc` leaves zero.
    let fields = [inode, mode, 0, 0, 1, 0, size, 0, 0, 0, 0, name_size, 0];
    archive.extend_from_slice(MAGIC);
    for field in fields {
        archive.extend_from_slice(format!("{field:08X}").as_bytes());
    }
    archive.extend_from_slice(name.as_bytes());
    archive.push(0);
    pad(archive);
    archive.extend_from_slice(data);
    pad(archive);
    Ok(())
}

/// Pads `archive` with zeros to a multiple of 4 bytes.
fn pad(archive: &mut Vec<u8>) {
    archive.resize(archive.len().next_multiple_of(4), 0);
}
