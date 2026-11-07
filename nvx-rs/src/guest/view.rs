//! The VM-wide hostname, machine ID, and resolver file of an assembled root.
//!
//! This is unstable internal reference code of the guest's space, hidden from documentation;
//! it becomes crate-private once nothing outside NVX uses it. The root owns the view's binds:
//! its teardown removes them before its switch, and nothing unmounts them afterwards.

use std::fs;
use std::io;
use std::net::IpAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[cfg(doc)]
use super::entry::Entry;
use super::root::Root;
use super::sys::{self, Mode, OpenFlags};
use crate::activation::{self, MachineIdMode, View};
use crate::protocol::cmdline::write_resolver;
use crate::{Error, Result};

/// The hostname used unless a caller names another.
pub const DEFAULT_HOSTNAME: &str = "nvx-sandbox";
/// The longest guest hostname, in bytes: Linux's `HOST_NAME_MAX`.
pub const MAX_HOSTNAME_BYTES: usize = 64;
/// The generated machine ID's file in the root's runtime directory.
pub const MACHINE_ID_FILE: &str = "workload-machine-id";
/// The resolver file in the root's runtime directory.
pub const RESOLVER_FILE: &str = "resolv.conf";
/// The mode of a generated machine ID's file.
const MACHINE_ID_MODE: u32 = 0o444;
/// The mode of the resolver file, whatever the umask.
const RESOLVER_MODE: u32 = 0o444;
/// The generated machine ID's target in the root.
pub(crate) const MACHINE_ID_TARGET: &str = "/etc/machine-id";
/// The resolver file's target in the root.
pub(crate) const RESOLVER_TARGET: &str = "/etc/resolv.conf";

/// The machine ID visible at `/etc/machine-id`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum MachineId {
    /// The image's own `/etc/machine-id`, left untouched.
    #[default]
    Image,
    /// The ID in this regular file, bound read-only over `/etc/machine-id`, as
    /// [`Entry::bind_runtime`] also does. Rewriting the file in place, such as after a restore,
    /// reaches every execution that sees the bind.
    Generated(PathBuf),
}

impl MachineId {
    /// Generates a random version 4 UUID and writes it to a new file at `path`, with mode 0444
    /// whatever the umask: 32 lowercase hexadecimal digits and a newline.
    ///
    /// An existing file or symbolic link fails with `AlreadyExists` and is left alone.
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
        file.set_mode(mode)?;
        file.write_all(&machine_id_text(id))?;
        file.close()?;
        Ok(Self::Generated(path))
    }
}

/// Sets the VM's hostname and applies its machine-ID mode and optional single DNS server.
///
/// Takes the actual hostname bytes of `View`, including those decoded by
/// [`protocol::view`](crate::protocol::view), not the legacy raw kernel token. Image mode leaves
/// the image's machine ID alone; generated mode writes a new ID in the runtime directory and
/// binds it read-only over the image's. DNS, when supplied, writes and binds the existing
/// single-server resolver format the same way.
///
/// The root owns every bind, including those made before a later failure. Its teardown removes
/// them before the switch; after the switch nothing unmounts them. The call needs the privilege
/// to set the hostname and mount, and refuses an invalid hostname, a switched or failed root,
/// or a root whose view has already been attempted. System failures have kind `Io`; refusals
/// have kind `Unsupported`. An error does not restore the previous hostname or remove files.
pub fn apply(root: &mut Root, view: &View, dns: Option<IpAddr>) -> Result<MachineId> {
    apply_staged(root, view, dns).map_err(ApplyFailure::into_error)
}

/// A view failure with the resolver's original cause distinguished for activation.
#[derive(Debug)]
pub(crate) enum ApplyFailure {
    /// A hostname, root, or machine-ID failure.
    View(Error),
    /// The resolver file could not be written.
    ResolverWrite(io::Error),
    /// The resolver file could not be bound.
    ResolverBind(Error),
}

impl From<Error> for ApplyFailure {
    fn from(error: Error) -> Self {
        Self::View(error)
    }
}

impl ApplyFailure {
    /// Returns the public view API's original error.
    fn into_error(self) -> Error {
        match self {
            Self::View(error) | Self::ResolverBind(error) => error,
            Self::ResolverWrite(error) => {
                Error::io(format!("failed to write the VM resolver file: {error}"))
                    .with_source(error)
            }
        }
    }
}

/// Applies the same view while retaining the resolver failure stage for activation.
pub(crate) fn apply_staged(
    root: &mut Root,
    view: &View,
    dns: Option<IpAddr>,
) -> std::result::Result<MachineId, ApplyFailure> {
    if !activation::is_hostname(&view.hostname) {
        return Err(Error::unsupported("the view's hostname is not valid").into());
    }
    root.begin_view()?;
    sys::set_hostname(&view.hostname).map_err(|error| {
        Error::io(format!("failed to set the VM hostname: {error}")).with_source(error)
    })?;
    apply_files(root, view, dns)
}

/// Applies files after the root's view has been reserved and its hostname set.
pub(crate) fn apply_files(
    root: &mut Root,
    view: &View,
    dns: Option<IpAddr>,
) -> std::result::Result<MachineId, ApplyFailure> {
    let machine_id = match view.machine_id {
        MachineIdMode::Image => MachineId::Image,
        MachineIdMode::Generated => MachineId::generate(root.runtime().join(MACHINE_ID_FILE))
            .map_err(|error| {
                Error::io(format!("failed to generate the VM machine ID: {error}"))
                    .with_source(error)
            })?,
    };
    let resolver = dns
        .map(|server| {
            let path = root.runtime().join(RESOLVER_FILE);
            write_resolver_file(&path, server).map(|()| path)
        })
        .transpose()
        .map_err(ApplyFailure::ResolverWrite)?;
    root.bind_view(&machine_id, None)?;
    root.bind_view(&MachineId::Image, resolver.as_deref())
        .map_err(ApplyFailure::ResolverBind)?;
    Ok(machine_id)
}

/// Returns a random ID's version 4 UUID text: 32 lowercase hexadecimal digits and a newline.
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

/// Writes one DNS server to a new resolver file, with mode 0444 whatever the umask.
pub(crate) fn write_resolver_file(path: &Path, server: IpAddr) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(RESOLVER_MODE)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(RESOLVER_MODE))?;
    write_resolver(&mut file, server)
}

#[cfg(test)]
mod tests;
