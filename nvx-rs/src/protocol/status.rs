//! The exit statuses with which a clone's guest ends its VM once it fails fast, and from which
//! the host classifies a clone's end. The guest library's errors select them: the time ABI's
//! violations, `nvx::guest::time::Violation::status`, and a clone's repair errors,
//! `nvx::guest::clone::RepairError::status`.
//!
//! - [`TIME_CONFORMANCE`], 193, [`TIME_VIOLATION`], 194, and [`TIME_REPAIR`], 195: the time ABI's
//!   own statuses, which keep their meanings. In enforcing mode, the time ABI stops the VM with
//!   them itself. In strict mode, which a guest init or agent selects when it runs the time ABI's
//!   calls in its own process, such as a clone's capture and repair, a call fails with the
//!   violation instead, whose status is one of these, and the caller ends the VM with it.
//! - [`REPAIR_RESTORE`] to [`REPAIR_FINISH`], 40 to 45: the other failures of a clone's repair, one
//!   status for each class.
//! - [`ACTIVATION_DEADLINE`] to [`ACTIVATION_ORDER`], 50 to 58: the failures of a clone's
//!   activation, which follows its repair, one status reserved for each class: the deadline, the
//!   host's session, the Activate's record and the resources that it expects, the activation of
//!   the root, of the workload, of the host export, and of the network settings, and the guest's
//!   own order.
//!
//! The guest ends the VM with a status after a diagnostic that names the failure, such as a
//! violation event of the time ABI, or an error line. A status alone does not tell the cause, nor
//! where it came from, since a guest whose workload ends it may exit with any status, so the host
//! classifies a clone's end by its status together with that diagnostic.

use std::ops::RangeInclusive;

/// A conformance check of the time ABI failed.
pub const TIME_CONFORMANCE: u8 = 193;
/// The time ABI's violation watcher observed a violation at run time, such as a clocksource
/// switch, a TSC warp, or an RCU stall.
pub const TIME_VIOLATION: u8 = 194;
/// The time ABI's part of a restore's repair failed: the restore packet's validation, the wall
/// clock's step, the record of the restore, or the release of the stall detectors.
pub const TIME_REPAIR: u8 = 195;

/// The restore cannot be repaired: it holds no fresh entropy, as when its capture wrote the
/// entropy to a file, or its capture finished it already.
pub const REPAIR_RESTORE: u8 = 40;
/// The online CPUs cannot be read or are not a prefix of the CPUs, the restore's processor target
/// is below them, or the repair could not bring the target's CPUs online: a write of a CPU's
/// state failed, or the online CPUs are not the target's prefix once it brought them online.
pub const REPAIR_PROCESSORS: u8 = 41;
/// The restore's memory target is refused, before any memory block was added: the kernel's
/// memory block size is not the snapshot ABI's, the restore lists ranges without a target, or
/// they do not continue the RAM that the clone captured.
pub const REPAIR_MEMORY_TARGET: u8 = 42;
/// The activation of the restore's memory target failed: a memory block could not be added or
/// onlined, or did not come online, or the kernel's memory interface could not be read.
pub const REPAIR_MEMORY: u8 = 43;
/// The reseed of the kernel's random number generator with the restore's fresh entropy failed.
pub const REPAIR_RESEED: u8 = 44;
/// The restore's finish failed other than by a violation of the time ABI, which ends the clone
/// with the violation's status, or an earlier attempt at it never returned. It may have
/// acknowledged the restore.
pub const REPAIR_FINISH: u8 = 45;

/// The clone was not activated before its deadline, which starts at its repair's
/// acknowledgement.
pub const ACTIVATION_DEADLINE: u8 = 50;
/// The host broke the session of a repaired clone's activation: before the clone was ready, it
/// reset the session again, or sent a request before the activation, and, before or after, it
/// sent an Activate of another transaction, or one that changes the accepted record.
pub const ACTIVATION_PROTOCOL: u8 = 51;
/// The Activate's record is refused: it does not decode, breaks a limit, or is a cold boot's,
/// without the restore section of a clone's.
pub const ACTIVATION_RECORD: u8 = 52;
/// The Activate's record expects other resources than the repaired clone has: another number of
/// online CPUs, or another memory size.
pub const ACTIVATION_RESOURCES: u8 = 53;
/// The clone's root cannot be activated with the record's settings: its image does not assemble
/// over its upper, a GPT image's partition table is refused, or its scratch is not fresh.
pub const ACTIVATION_ROOT: u8 = 54;
/// The clone's workload cannot be activated with the record's settings: its account is missing
/// or invalid in the image, or its hostname or its machine ID cannot be set.
pub const ACTIVATION_WORKLOAD: u8 = 55;
/// The host export cannot be mounted in the clone.
pub const ACTIVATION_EXPORT: u8 = 56;
/// The record's network settings cannot be applied in the clone: its resolver or its proxy
/// endpoint, or the announcer of the workload's listeners, which its network publishes, cannot
/// start, or ended.
pub const ACTIVATION_NETWORK: u8 = 57;
/// The guest's own order misused the activation: it completed an activation that did not run,
/// or started the sandbox before the clone was ready.
pub const ACTIVATION_ORDER: u8 = 58;

/// The time ABI's statuses, from [`TIME_CONFORMANCE`] to [`TIME_REPAIR`].
pub const TIME_STATUSES: RangeInclusive<u8> = TIME_CONFORMANCE..=TIME_REPAIR;
/// The statuses of a clone's repair besides the time ABI's, from [`REPAIR_RESTORE`] to
/// [`REPAIR_FINISH`].
pub const REPAIR_STATUSES: RangeInclusive<u8> = REPAIR_RESTORE..=REPAIR_FINISH;
/// The statuses of a clone's activation, from [`ACTIVATION_DEADLINE`] to [`ACTIVATION_ORDER`].
pub const ACTIVATION_STATUSES: RangeInclusive<u8> = ACTIVATION_DEADLINE..=ACTIVATION_ORDER;

/// Returns what `status` means, as the module tells, such as `a clone's memory activation failed`,
/// or `None` for a status that the module does not name.
pub fn describe(status: u8) -> Option<&'static str> {
    Some(match status {
        TIME_CONFORMANCE => "a time ABI conformance check failed",
        TIME_VIOLATION => "the time ABI's violation watcher observed a violation",
        TIME_REPAIR => "the time ABI's part of a restore's repair failed",
        REPAIR_RESTORE => "a clone's restore cannot be repaired",
        REPAIR_PROCESSORS => "a clone's online CPUs do not meet its restore's processor target",
        REPAIR_MEMORY_TARGET => "a clone's memory target is refused",
        REPAIR_MEMORY => "a clone's memory activation failed",
        REPAIR_RESEED => "a clone's reseed failed",
        REPAIR_FINISH => "a clone's restore finish failed",
        ACTIVATION_DEADLINE => "a clone was not activated before its deadline",
        ACTIVATION_PROTOCOL => "the host broke a repaired clone's restricted session",
        ACTIVATION_RECORD => "a clone's Activate record is refused",
        ACTIVATION_RESOURCES => "a clone's Activate record expects other resources",
        ACTIVATION_ROOT => "a clone's root activation failed",
        ACTIVATION_WORKLOAD => "a clone's workload activation failed",
        ACTIVATION_EXPORT => "a clone's host export cannot be mounted",
        ACTIVATION_NETWORK => "a clone's network settings cannot be applied",
        ACTIVATION_ORDER => "a guest's order misused a clone's activation",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every status that the module names, in its order.
    const ALL: [u8; 18] = [
        TIME_CONFORMANCE,
        TIME_VIOLATION,
        TIME_REPAIR,
        REPAIR_RESTORE,
        REPAIR_PROCESSORS,
        REPAIR_MEMORY_TARGET,
        REPAIR_MEMORY,
        REPAIR_RESEED,
        REPAIR_FINISH,
        ACTIVATION_DEADLINE,
        ACTIVATION_PROTOCOL,
        ACTIVATION_RECORD,
        ACTIVATION_RESOURCES,
        ACTIVATION_ROOT,
        ACTIVATION_WORKLOAD,
        ACTIVATION_EXPORT,
        ACTIVATION_NETWORK,
        ACTIVATION_ORDER,
    ];

    #[test]
    fn the_statuses_are_pinned() {
        assert_eq!(
            ALL,
            [
                193, 194, 195, 40, 41, 42, 43, 44, 45, 50, 51, 52, 53, 54, 55, 56, 57, 58
            ]
        );
        assert_eq!(TIME_STATUSES, 193..=195);
        assert_eq!(REPAIR_STATUSES, 40..=45);
        assert_eq!(ACTIVATION_STATUSES, 50..=58);
    }

    #[test]
    fn each_status_has_a_class_and_a_meaning_of_its_own() {
        let mut meanings = Vec::new();
        for status in ALL {
            let classes = [&TIME_STATUSES, &REPAIR_STATUSES, &ACTIVATION_STATUSES]
                .into_iter()
                .filter(|statuses| statuses.contains(&status))
                .count();
            assert_eq!(classes, 1, "{status}");
            meanings.push(describe(status).unwrap());
        }
        meanings.sort_unstable();
        meanings.dedup();
        assert_eq!(meanings.len(), ALL.len());
        // The ranges hold no other status, and the module names no other.
        for status in 0..=u8::MAX {
            assert_eq!(
                describe(status).is_some(),
                ALL.contains(&status),
                "{status}"
            );
        }
        // None is a success, the generic failure, a usage error, a sandbox setup failure, or a
        // status that a shell gives a command that it cannot run or that a signal ended.
        for reserved in [0, 1, 2, 125, 126, 127, 128, 129, 137, 143, 255] {
            assert!(!ALL.contains(&reserved), "{reserved}");
        }
    }
}
