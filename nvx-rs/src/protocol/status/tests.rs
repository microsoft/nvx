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
    GUEST_RESTORE_DEADLINE,
    GUEST_RESTORE_PROTOCOL,
    GUEST_RESTORE_RECORD,
    GUEST_RESTORE_RESOURCES,
    GUEST_RESTORE_ROOT,
    GUEST_RESTORE_WORKLOAD,
    GUEST_RESTORE_EXPORT,
    GUEST_RESTORE_NETWORK,
    GUEST_RESTORE_ORDER,
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
    assert_eq!(GUEST_RESTORE_STATUSES, 50..=58);
}

#[test]
fn each_status_has_a_class_and_a_meaning_of_its_own() {
    let mut meanings = Vec::new();
    for status in ALL {
        let classes = [&TIME_STATUSES, &REPAIR_STATUSES, &GUEST_RESTORE_STATUSES]
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
