use std::num::NonZeroU64;

use super::*;

/// Returns the store generation `number`.
fn generation(number: u64) -> StoreGeneration {
    StoreGeneration::new(NonZeroU64::new(number).unwrap())
}

/// Returns the identity of a capture class, whose bytes are all `byte`.
fn class(byte: u8) -> ClassIdentity {
    ClassIdentity::from_bytes([byte; 32])
}

/// Returns the numbers of the store generations that an eviction retires, as [`superseded`]
/// returns them, once the host program has served the generation `served` of its class, from
/// an index that lists the generations `indexed`, each with its class, and whose next
/// publication takes `next`, and from the tombstones of the generations `tombstoned`. Each
/// class is a byte, which [`class`] makes an identity of.
fn evicted(indexed: &[(u64, u8)], next: u64, tombstoned: &[u64], served: (u64, u8)) -> Vec<u64> {
    let indexed: Vec<_> = indexed
        .iter()
        .map(|&(number, of)| (generation(number), class(of)))
        .collect();
    let tombstoned: Vec<_> = tombstoned.iter().copied().map(generation).collect();
    let (served, of) = served;
    superseded(
        &indexed,
        generation(next),
        &tombstoned,
        generation(served),
        class(of),
    )
    .into_iter()
    .map(|number| number.get().get())
    .collect()
}

#[test]
fn an_eviction_retires_the_generations_of_the_served_class_that_it_supersedes() {
    // Generations 1, 3, 5, and 6 are of one class, 2 and 4 of another, and tombstones
    // quarantine 4 and 6.
    let indexed = [(1, 1), (2, 2), (3, 1), (4, 2), (5, 1), (6, 1)];
    assert_eq!(evicted(&indexed, 7, &[4, 6], (3, 1)), [1, 6]);
    assert_eq!(evicted(&indexed, 7, &[4, 6], (2, 2)), [4]);
    assert_eq!(evicted(&indexed, 7, &[4, 6], (6, 1)), [1, 3, 5]);
    assert!(evicted(&indexed, 7, &[], (1, 1)).is_empty());
}

#[test]
fn an_eviction_retires_the_tombstones_that_interrupted_retirements_left_alone() {
    // The store published generations 1 to 6, and its index lists 2, 4, and 5, of two
    // classes: the retirements of 1 and 3 ended once they had removed them from the index,
    // and left their tombstones, while that of 6 removed its tombstone too.
    let indexed = [(2, 1), (4, 2), (5, 1)];
    assert_eq!(evicted(&indexed, 7, &[1, 3], (4, 2)), [1, 3]);
    assert_eq!(evicted(&indexed, 7, &[1, 3], (5, 1)), [1, 2, 3]);
    assert_eq!(evicted(&indexed, 7, &[1, 3], (2, 1)), [1, 3]);
    // A tombstone of the index's next generation, or of a later one, is of a generation that
    // the store published, and tombstoned, after the eviction read its index, which it leaves.
    assert_eq!(evicted(&indexed, 7, &[1, 7, 8], (4, 2)), [1]);
}
