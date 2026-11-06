use super::*;

/// Seeds CAP_CHOWN in the parent's inheritable and ambient sets for real credential controls.
pub(crate) fn seed_inheritable_and_ambient() -> Result<(), Errno> {
    let header = CapabilityHeader {
        version: CAPABILITY_VERSION,
        pid: 0,
    };
    let mut data = capability_data(0);
    // SAFETY: the valid header selects the two writable data words.
    unit(unsafe { libc::syscall(libc::SYS_capget, &header, data.as_mut_ptr()) })?;
    data[0].inheritable |= 1;
    // SAFETY: the initialized header and both data words outlive the syscall.
    unit(unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) })?;
    prctl(
        libc::PR_CAP_AMBIENT,
        libc::PR_CAP_AMBIENT_RAISE as c_ulong,
        0,
    )
}

/// How a test without privileges simulates the stages of an entry: the child starts without
/// new namespaces and skips the system calls of every entry stage.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Simulation {
    /// Every entry stage succeeds.
    Skip,
    /// The stage fails with the error.
    Fail(LaunchStage, Errno),
}

#[test]
fn capability_sets_split_into_two_data_words() {
    assert_eq!(
        capability_data(0),
        [CapabilityData {
            effective: 0,
            permitted: 0,
            inheritable: 0
        }; 2]
    );
    // CAP_CHOWN, CAP_NET_RAW, CAP_SETFCAP (31), and CAP_MAC_OVERRIDE (32) straddle the words.
    let set = 1 | 1 << 13 | 1 << 31 | 1 << 32;
    let [low, high] = capability_data(set);
    assert_eq!(
        (low.effective, low.permitted, low.inheritable),
        (0x8000_2001, 0x8000_2001, 0)
    );
    assert_eq!(
        (high.effective, high.permitted, high.inheritable),
        (1, 1, 0)
    );
    let [low, high] = capability_data(u64::MAX);
    assert_eq!((low.permitted, high.permitted), (u32::MAX, u32::MAX));
    assert_eq!((low.inheritable, high.inheritable), (0, 0));
}

#[test]
fn the_kernel_structures_have_their_uapi_sizes() {
    assert_eq!(std::mem::size_of::<CloneArgs>(), 64);
    assert_eq!(std::mem::size_of::<OpenHow>(), 24);
    assert_eq!(std::mem::size_of::<MountAttr>(), 32);
    assert_eq!(std::mem::size_of::<CapabilityHeader>(), 8);
    assert_eq!(std::mem::size_of::<CapabilityData>(), 12);
}
