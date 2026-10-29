//! The host program's template store: where it is, what `runtime-adapter-host list` and
//! `inspect` report of its generations, and which generations leave it, as `create`, `refresh`,
//! and `remove` evict them. NVX's template store, `nvx::host::template::Store`, provides the
//! mechanisms: the adapter decides where its store is, when to capture, and which generations
//! to retire.
//!
//! # Location
//!
//! The template store is the directory that each template command of the host program names, as
//! its sandbox commands name a sandbox's store: a directory of its own, apart from every sandbox's
//! store, private to the current user, whose parent exists. `create` and `refresh` create it if
//! needed, as `Store::create` does, while `list`, `inspect`, and `remove` open it, as `Store::open`
//! does, and fail if it holds no store. On Linux, the control socket of a capture's VMM,
//! `captures/C/transaction/control.sock` in the store, must fit the path of a Unix socket, so NVX
//! refuses a capture in a store whose path is too long for it.
//!
//! # Listings
//!
//! [`entries`] and [`entry`] report what the store records of its generations, from its index, its
//! tombstones, and the records of its leases alone: each generation's store generation, the
//! identities of its compatibility key and of its capture class, the metadata of its capture,
//! whether a tombstone quarantines it, whether a clone may restore it at the host's time, and why,
//! as `Generation::eligibility` decides it, and its leases. They read no generation's payload:
//! neither its snapshot nor its key, whose decoding checks the digest of the key's kernel command
//! line, so a listing loads no VM state, and hashes nothing. They take no lock of the store, so
//! each record is as a listing read it, and a transaction of the store may change the next one
//! meanwhile.
//!
//! # Eviction
//!
//! The host program keeps, of each capture class, the generation that it served last, and none
//! that it supersedes. Once `create` or `refresh` has served a generation of a class, which the
//! store held already or has just published, [`evict`] retires every other generation of the
//! class that the store published before it, or that a tombstone quarantines. `remove` retires the
//! generation that it names. A retirement, `Store::retire`, tombstones the generation first, so
//! that it takes no new lease, and deletes it, with its payload, once no lease of it remains: it
//! never moves or deletes a generation that a clone leases. A generation that leases keep, which
//! drains, stays tombstoned, and the next retirement of it, by the next eviction of its class or a
//! `remove` that names it, deletes it once its clones have released them.
//!
//! A retirement that a crash interrupts, even one of the host, leaves its generation tombstoned.
//! Once it has removed the generation from the store's index, the tombstone alone remains: the
//! generation is gone, with its payload, its leases, and the class that its entry in the index
//! recorded. [`evict`] also retires each such tombstone, of a store generation below the index's
//! next one that the index no longer lists, whatever the class that it serves, since no class is
//! left to compare: retiring it again removes the tombstone alone, and touches no generation that
//! the index lists. A `remove` that names its generation retires it too.
//!
//! Only `create`, for a class that the store holds no eligible generation of, and `refresh`
//! capture templates: no other command of the host program captures one, and none retires a
//! generation otherwise.

use std::time::Instant;

use nvx::Result;
use nvx::activation::StoreGeneration;
use nvx::host::template::{
    ClassIdentity, Eligibility, Generation, HostTime, Lease, Retirement, Store,
};

/// What a template store records of one of its generations, as `runtime-adapter-host list` and
/// `inspect` report it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The generation, as the store's index lists it: its store generation, the identities of its
    /// key and of its capture class, and the metadata of its capture.
    pub generation: Generation,
    /// Whether a tombstone quarantines the generation.
    pub state: State,
    /// Whether a clone may restore the generation at the host's time of the listing, by the
    /// restore window of its capture's time contract, and why.
    pub eligibility: Eligibility,
    /// The leases of the generation that the store's records list, in the order of their
    /// identities, including those that an interrupted release left, which the generation's
    /// retirement removes.
    pub leases: Vec<Lease>,
}

/// The state of a generation in its template store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// No tombstone quarantines the generation: a clone may lease it while it is eligible.
    Available,
    /// A tombstone quarantines the generation: it takes no new lease, and its retirement deletes
    /// it once its leases have drained.
    Tombstoned,
}

impl State {
    /// Returns the state's name, as the host program prints it: `available` or `tombstoned`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Tombstoned => "tombstoned",
        }
    }
}

/// Returns what the template store `store` records of each of its generations, in the order of
/// their publication, with their eligibility at the host's time `now`, as the
/// [module documentation](self#listings) describes.
///
/// Fails as `Store::index`, `Store::tombstones`, and `Store::leases` fail.
pub fn entries(store: &Store, now: &HostTime) -> Result<Vec<Entry>> {
    let index = store.index()?;
    let tombstoned = store.tombstones()?;
    index
        .generations()
        .iter()
        .map(|generation| describe(store, generation, &tombstoned, now))
        .collect()
}

/// Returns what the template store `store` records of its generation `generation`, with its
/// eligibility at the host's time `now`, as [`entries`] does.
///
/// Fails with [`ErrorKind::NotFound`](nvx::ErrorKind::NotFound) if the store does not index the
/// generation, which it no longer holds, or never published, and otherwise as [`entries`] does.
pub fn entry(store: &Store, generation: StoreGeneration, now: &HostTime) -> Result<Entry> {
    let index = store.index()?;
    let Some(indexed) = index.get(generation) else {
        let why = if generation < index.next_generation() {
            "no longer holds"
        } else {
            "never published"
        };
        return Err(nvx::Error::not_found(format!(
            "the template store {} {why} a generation {generation}",
            store.dir().display()
        )));
    };
    let tombstoned = store.tombstones()?;
    describe(store, indexed, &tombstoned, now)
}

/// Returns what the template store `store` records of `generation`, which the store's tombstones
/// `tombstoned` quarantine, or not, with its eligibility at `now`.
fn describe(
    store: &Store,
    generation: &Generation,
    tombstoned: &[StoreGeneration],
    now: &HostTime,
) -> Result<Entry> {
    let state = if tombstoned.contains(&generation.generation()) {
        State::Tombstoned
    } else {
        State::Available
    };
    Ok(Entry {
        generation: generation.clone(),
        state,
        eligibility: generation.eligibility(now),
        leases: store.leases(generation.generation())?,
    })
}

/// What [`evict`] retired.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Eviction {
    /// The generations that the eviction deleted, or found gone already, with those whose
    /// interrupted retirement it completed, in order.
    pub retired: Vec<StoreGeneration>,
    /// The generations that leases keep, which the eviction tombstoned, or found tombstoned, and
    /// which the next retirement of each deletes once its clones have released them, in order.
    pub draining: Vec<StoreGeneration>,
}

/// Retires, from the template store `store`, the generations that its generation `served`
/// supersedes, once the host program has served it: every other generation of the class of
/// `served` that the store published before it, or that a tombstone quarantines, with
/// `Store::retire`, each waiting for the store's lock until `deadline`, as the
/// [module documentation](self#eviction) describes. It also retires the tombstone that each
/// retirement that a crash interrupted left alone, once it had removed its generation from the
/// index, whatever the generation's class.
///
/// Fails as `Store::index`, `Store::tombstones`, and `Store::retire` fail, once it has retired the
/// generations before the one whose retirement failed. Each retirement is complete, or leaves a
/// tombstoned generation, which the next retirement of it deletes.
pub fn evict(store: &Store, served: &Generation, deadline: Instant) -> Result<Eviction> {
    let index = store.index()?;
    let tombstoned = store.tombstones()?;
    let indexed: Vec<_> = index
        .generations()
        .iter()
        .map(|generation| (generation.generation(), generation.class()))
        .collect();
    let mut eviction = Eviction::default();
    for number in superseded(
        &indexed,
        index.next_generation(),
        &tombstoned,
        served.generation(),
        served.class(),
    ) {
        match store.retire(number, Some(deadline))? {
            Retirement::Retired => eviction.retired.push(number),
            Retirement::Draining(_) => eviction.draining.push(number),
        }
    }
    Ok(eviction)
}

/// Returns the store generations that an eviction retires, in order, once the host program has
/// served the generation `served`, of the capture class `class`, from a read of the store's index,
/// which lists the generations `indexed`, each with the identity of its class, and whose next
/// publication takes `next`, and from the store's tombstones `tombstoned`: every other generation
/// of the class that the index lists before `served`, or that a tombstone quarantines, and every
/// other tombstoned generation below `next` that the index no longer lists, whatever its class. A
/// tombstone of `next` or later is of a generation that the store published after the read.
fn superseded(
    indexed: &[(StoreGeneration, ClassIdentity)],
    next: StoreGeneration,
    tombstoned: &[StoreGeneration],
    served: StoreGeneration,
    class: ClassIdentity,
) -> Vec<StoreGeneration> {
    let of_class = indexed
        .iter()
        .filter(|(number, of)| *of == class && (*number < served || tombstoned.contains(number)))
        .map(|(number, _)| *number);
    let unindexed = tombstoned
        .iter()
        .copied()
        .filter(|number| *number < next && !indexed.iter().any(|(listed, _)| listed == number));
    let mut superseded: Vec<_> = of_class
        .chain(unindexed)
        .filter(|number| *number != served)
        .collect();
    superseded.sort_unstable();
    superseded
}

#[cfg(test)]
mod tests {
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
    fn evicted(
        indexed: &[(u64, u8)],
        next: u64,
        tombstoned: &[u64],
        served: (u64, u8),
    ) -> Vec<u64> {
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
}
