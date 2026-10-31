mod endpoint;
mod identity;
mod log_claim;
mod private;
mod process;
mod spawn;

use std::fs::OpenOptions;

use nix::sys::stat::Mode;

use crate::hypervisor::Hypervisor;

#[cfg(feature = "cli")]
pub(crate) use crate::host::sys::{interrupted, watch_interrupts, write_interruptibly};

pub(crate) use self::endpoint::{
    ReadyListener, connect_endpoint, control_endpoint, endpoint_server_pid, listen_ready,
};
pub use self::identity::file_identity;
pub(crate) use self::identity::{
    Directory, file_id, is_deleted, link_count, opened_file_id, opened_file_identity,
};
pub(crate) use self::log_claim::{claim_log, log_claim_held, serves_launch};
pub use self::private::create_private_dir;
pub(crate) use self::private::{
    check_owner_only_dir, check_snapshot_dir, check_store_dir, create_owner_only_dir,
    create_store_dir, foreign_writer_problem, make_sparse, private_file_problem, rename_no_replace,
    restore_private_file_mode,
};
#[cfg(test)]
pub(crate) use self::private::{owner_only_problem, store_dir_problem};
pub use self::process::process_start_time;
pub(crate) use self::process::{
    ProcessHandle, boot_id, host_id, monotonic_ns, open_process, own_bytes_read, pin_child,
};
#[cfg(test)]
pub(crate) use self::process::{check_identity, start_time_from_stat};
pub(crate) use self::spawn::detach;
#[cfg(feature = "cli")]
pub(crate) use self::spawn::inherit_only;

/// Whether this host can run NVX microVMs.
pub const SUPPORTED: bool = true;

/// File that holds the identity of the current boot, a UUID that the kernel chooses at random.
const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

/// Mode of a directory that only the current user may access: read, write, and search for its
/// owner, and nothing for anyone else.
const PRIVATE_DIR_MODE: Mode = Mode::S_IRWXU;

/// Mode of the directory of a sandbox store, the exact mode that OpenVMM requires of the
/// directory of its control socket.
const STORE_DIR_MODE: Mode = Mode::S_IRWXU;

/// Permission bits of a mode that grant access to the group and to others.
const GROUP_AND_OTHER_BITS: Mode = Mode::S_IRWXG.union(Mode::S_IRWXO);

/// Checks that the hypervisor device is present and accessible.
pub(crate) fn probe_hypervisor(hypervisor: Hypervisor) -> Result<(), String> {
    let device = match hypervisor {
        Hypervisor::Kvm => "/dev/kvm",
        Hypervisor::Mshv => "/dev/mshv",
        Hypervisor::Whp => return Err("WHP is only available on Windows".to_owned()),
    };
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .map(drop)
        .map_err(|error| format!("{device} is not accessible: {error}"))
}

#[cfg(test)]
mod tests;
