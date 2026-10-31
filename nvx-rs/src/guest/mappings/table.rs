//! The host mapping table: the mappings that a host hands a guest agent in `MAPS` parts once the
//! agent is ready, which a [`MappingTable`] checks and has the agent apply, in order, beneath the
//! root of the agent's choice.

use std::ffi::OsString;
use std::fmt;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use super::{Access, InvalidMapping, Mapping};
use crate::protocol::{self, GuestHostMap, MAX_HOST_MAPPINGS, ProtocolError};

/// The host mapping table of a guest agent: how many mappings the host announced, how many of
/// them the agent applied, in order, and whether one failed, after which the table refuses the
/// rest, so that the agent starts no workload on a partial table.
///
/// The host announces the table's size before it sends any of it: NVX's managed lifecycle with
/// the `nvx_maps=COUNT` token of the kernel command line, which [`announced`](Self::announced)
/// reads, and an adapter as its own protocol has it, such as in a configuration that it sends its
/// guest, which [`new`](Self::new) takes. Once the agent is ready, the host sends the table in
/// parts, each a `MAPS` payload of consecutive entries, in order, which
/// [`protocol::decode_guest_maps_payload`] reads. [`apply`](Self::apply) checks each part against
/// the table, and hands its entries, as [`Mapping`]s, to the caller, which maps them beneath the
/// root of its choice, from the host export: through a [`Mapper`](super::Mapper) beneath a
/// guest's own `/` or a container's root, for example, or through a layered
/// [`Root`](crate::guest::root::Root).
///
/// - The table refuses a part as a whole, and changes nothing, which [`TableError::Refused`]
///   reports with the entry that the table expects next and the [`Refusal`]: once a mapping
///   failed, and a part that does not decode, holds no entry, does not start at the entry that
///   the table expects next, or ends past the number that the host announced. The entries come
///   from the host export, an aggregate whose root lists its children, so it also refuses a part
///   with an entry whose source is empty, which names that root.
/// - A mapping that the root refuses or fails to apply latches the table, which
///   [`TableError::Failed`] reports with the entry that failed, the mapping's index in the table,
///   and the root's error. The mappings before it stay, and the table refuses every later part.
/// - The table is [complete](Self::is_complete) once it applied every mapping that the host
///   announced, and none failed: only then may the agent start workloads. A table of zero
///   mappings is complete at once, and its agent maps nothing.
///
/// The root keeps the mappings, and its own teardown tears the table down, in reverse order, such
/// as [`Mapper::teardown`](super::Mapper::teardown), which reports each mapping that stays mounted,
/// or [`Root::teardown`](crate::guest::root::Root::teardown).
///
/// NVX's managed agent applies its table through one, beneath its own root, `/`, in direct mode,
/// and beneath the container's assembled root in sandbox mode, with a mapper that it opens at the
/// first part that the table hands it, so that a table of zero mappings opens nothing.
///
/// ```no_run
/// use std::path::Path;
///
/// use nvx::guest::mappings::{
///     Mapper, MappingTable, Reserved, SourceRoot, TableError, TargetRoot,
/// };
///
/// # fn parts() -> Vec<Vec<u8>> { Vec::new() }
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let mut table = MappingTable::announced(&std::fs::read("/proc/cmdline")?)?;
/// let root = TargetRoot::open(Path::new("/newroot"))?;
/// let mut mapper = Mapper::new(root, Reserved::container_root());
/// let export = SourceRoot::open(Path::new("/run/nvx/hostfs/root"))?;
/// // Each part of the table, in the order in which the host sends them.
/// for part in parts() {
///     let applied = table.apply(&part, |mappings| {
///         mapper
///             .apply(&export, mappings)
///             .map_err(|error| (error.entry(), error))
///     });
///     match applied {
///         Ok(()) => println!("{} of {} mappings applied", table.applied(), table.expected()),
///         // The host learns of the entry, and of why, as its protocol has it.
///         Err(TableError::Refused { entry, refusal }) => println!("refused at {entry}: {refusal}"),
///         Err(error) => println!("{error}"),
///     }
/// }
/// // Start workloads only once the table is complete; then:
/// assert!(table.is_complete());
/// mapper.teardown()?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MappingTable {
    /// How many mappings the host announced.
    expected: u32,
    /// How many of them the table applied, in order.
    applied: u32,
    /// Whether a mapping failed, or the caller latched the table, after which it refuses every
    /// part.
    latched: bool,
}

impl MappingTable {
    /// Returns the table of `expected` mappings, which the host announced, none of them applied
    /// yet, or `None` for more than [`MAX_HOST_MAPPINGS`], which no table holds.
    pub fn new(expected: u32) -> Option<Self> {
        (expected as usize <= MAX_HOST_MAPPINGS).then_some(Self {
            expected,
            applied: 0,
            latched: false,
        })
    }

    /// Returns the table that the kernel command line `command_line`, such as the contents of
    /// `/proc/cmdline`, announces with its `nvx_maps=COUNT` token, as NVX's managed lifecycle
    /// announces it, none of its mappings applied yet: a table of zero mappings without one.
    ///
    /// Fails as [`protocol::parse_host_mapping_count`] does, for a malformed or repeated token,
    /// and for one that does not announce from 1 through [`MAX_HOST_MAPPINGS`] mappings.
    pub fn announced(command_line: &[u8]) -> Result<Self, ProtocolError> {
        Ok(Self {
            expected: protocol::parse_host_mapping_count(command_line)?,
            applied: 0,
            latched: false,
        })
    }

    /// Returns how many mappings the host announced.
    pub fn expected(&self) -> u32 {
        self.expected
    }

    /// Returns how many of the table's mappings it applied, in order: the entry that it expects
    /// next.
    pub fn applied(&self) -> u32 {
        self.applied
    }

    /// Returns whether a mapping failed, or the caller [latched](Self::latch) the table, after
    /// which it refuses every part.
    pub fn is_latched(&self) -> bool {
        self.latched
    }

    /// Returns whether the table applied every mapping that the host announced, and none failed,
    /// so that its agent may start workloads.
    pub fn is_complete(&self) -> bool {
        !self.latched && self.applied == self.expected
    }

    /// Latches the table, as a mapping that fails does: it refuses every later part, and is never
    /// complete. A caller whose protocol ends the table at a part that the table refused, which
    /// leaves the table as it was, latches it then.
    pub fn latch(&mut self) {
        self.latched = true;
    }

    /// Applies `part`, a `MAPS` payload of the table's next entries, through `map`, which maps
    /// them beneath the root of the caller's choice, as the [type](Self) describes.
    ///
    /// The table first checks the part. It refuses the part as a whole, without calling `map`,
    /// with [`TableError::Refused`] at the entry that it expects next, if the table is latched,
    /// and if the part does not decode, as [`protocol::decode_guest_maps_payload`] reads it,
    /// holds no entry, does not start at the entry that the table expects next, ends past the
    /// number of mappings that the host announced, or holds an entry whose source is empty.
    ///
    /// Otherwise it hands `map` the part's mappings, in order, each with the access of its entry.
    /// `map` maps them beneath its root, keeps those before the first that it refuses or fails to
    /// apply, and returns that one's index among them, which is how many of them it applied, with
    /// why; an index past the last counts as the number of mappings. The table counts the
    /// mappings that `map` applied, and once one failed it latches, and returns
    /// [`TableError::Failed`] with the entry that failed, its index in the table.
    pub fn apply<E>(
        &mut self,
        part: &[u8],
        map: impl FnOnce(&[Mapping]) -> Result<(), (usize, E)>,
    ) -> Result<(), TableError<E>> {
        let mappings = self.check(part).map_err(|refusal| TableError::Refused {
            entry: self.applied,
            refusal,
        })?;
        match map(&mappings) {
            Ok(()) => {
                // A part holds at most `MAX_HOST_MAPPINGS` mappings, as the table does.
                self.applied += mappings.len() as u32;
                Ok(())
            }
            Err((index, error)) => {
                self.applied += index.min(mappings.len()) as u32;
                self.latched = true;
                Err(TableError::Failed {
                    entry: self.applied,
                    error,
                })
            }
        }
    }

    /// Checks `part` as [`apply`](Self::apply) does, and returns its mappings, in order.
    fn check(&self, part: &[u8]) -> Result<Vec<Mapping>, Refusal> {
        if self.latched {
            return Err(Refusal::Latched);
        }
        let part = protocol::decode_guest_maps_payload(part).map_err(Refusal::Malformed)?;
        if part.maps.is_empty() {
            return Err(Refusal::Empty);
        }
        if part.first != self.applied {
            return Err(Refusal::Order {
                first: part.first,
                expected: self.applied,
            });
        }
        // The decoder refuses a part of more than `MAX_HOST_MAPPINGS` entries, and the table
        // holds at most as many, so the end fits a `u32`.
        let end = self.applied + part.maps.len() as u32;
        if end > self.expected {
            return Err(Refusal::Excess {
                end,
                announced: self.expected,
            });
        }
        (self.applied..)
            .zip(part.maps)
            .map(|(entry, map)| mapping(entry, map))
            .collect()
    }
}

/// Returns the mapping of `map`, the table's entry `entry`, whose paths the guest takes as bytes,
/// as Linux does: from a source relative to the root of the host export to an absolute target,
/// read-only if the entry says so. Refuses an entry whose source is empty, which names the
/// export's root.
fn mapping(entry: u32, map: GuestHostMap) -> Result<Mapping, Refusal> {
    if map.source.is_empty() {
        return Err(Refusal::ExportRoot { entry });
    }
    let path = |bytes: Vec<u8>| PathBuf::from(OsString::from_vec(bytes));
    let access = if map.read_only {
        Access::ReadOnly
    } else {
        Access::ReadWrite
    };
    Mapping::new(path(map.source), path(map.target), access)
        .map_err(|error| Refusal::Mapping { entry, error })
}

/// Why a [`MappingTable`] applied none of a part, or only its mappings before the one that
/// failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableError<E> {
    /// The table refused the part as a whole: it applied none of the part's mappings, and changed
    /// nothing.
    Refused {
        /// The entry that the table expects next: how many of its mappings it applied.
        entry: u32,
        /// Why it refused the part.
        refusal: Refusal,
    },
    /// The root refused or failed to apply one of the part's mappings, after the part's mappings
    /// before it, which stay, and the table is latched.
    Failed {
        /// The mapping's index in the table, from 0: how many of the table's mappings are applied.
        entry: u32,
        /// Why, as the root reported it.
        error: E,
    },
}

impl<E> TableError<E> {
    /// Returns how many of the table's mappings are applied: the entry that the table expects
    /// next, at which it refused the part, or the entry that failed.
    pub fn entry(&self) -> u32 {
        match self {
            Self::Refused { entry, .. } | Self::Failed { entry, .. } => *entry,
        }
    }
}

impl<E: fmt::Display> fmt::Display for TableError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused { refusal, .. } => {
                write!(formatter, "the mapping table refused a part: {refusal}")
            }
            Self::Failed { entry, error } => {
                write!(formatter, "mapping {entry} of the table failed: {error}")
            }
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for TableError<E> {}

/// Why a [`MappingTable`] refused a part as a whole.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// A mapping failed before, or the caller [latched](MappingTable::latch) the table, which
    /// refuses every part since.
    Latched,
    /// The part does not decode, as [`protocol::decode_guest_maps_payload`] reads it.
    Malformed(ProtocolError),
    /// The part holds no entry.
    Empty,
    /// The part does not start at the entry that the table expects next.
    Order {
        /// The entry at which the part starts.
        first: u32,
        /// The entry that the table expects next.
        expected: u32,
    },
    /// The part ends past the number of mappings that the host announced.
    Excess {
        /// How many mappings the table would hold with the part's.
        end: u32,
        /// How many mappings the host announced.
        announced: u32,
    },
    /// An entry of the part has an empty source, which names the root of the host export: an
    /// aggregate, whose root lists its children, and which the table never maps.
    ExportRoot {
        /// The entry's index in the table.
        entry: u32,
    },
    /// An entry of the part breaks the rules of a [`Mapping`]'s paths, which the decoder applies
    /// too.
    Mapping {
        /// The entry's index in the table.
        entry: u32,
        /// Which of its paths breaks them.
        error: InvalidMapping,
    },
}

impl fmt::Display for Refusal {
    /// Writes why the table refused the part, as a clause about it.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Latched => formatter.write_str("an earlier part of the table failed"),
            Self::Malformed(error) => write!(formatter, "it does not decode: {error}"),
            Self::Empty => formatter.write_str("it holds no mapping"),
            Self::Order { first, expected } => write!(
                formatter,
                "it starts at mapping {first}, but the table expects mapping {expected} next"
            ),
            Self::Excess { end, announced } => write!(
                formatter,
                "it ends the table at {end} mappings, past the {announced} that the host announced"
            ),
            Self::ExportRoot { entry } => write!(
                formatter,
                "its mapping {entry} maps the root of the host export, which lists the export's \
                 children"
            ),
            Self::Mapping { entry, error } => {
                write!(formatter, "its mapping {entry} is refused: {error}")
            }
        }
    }
}

impl std::error::Error for Refusal {}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::*;
    use crate::protocol::{MAP_READ_ONLY, MAPS_HEADER_LEN};

    /// Returns a `MAPS` payload whose first entry is the table's `first`, of `entries`, each a
    /// source, a target, and its flags, which says that it holds `count` entries, if given, and
    /// otherwise as many as it does.
    fn part(first: u32, entries: &[(&[u8], &[u8], u16)], count: Option<u32>) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&first.to_le_bytes());
        payload.extend_from_slice(&count.unwrap_or(entries.len() as u32).to_le_bytes());
        for (source, target, flags) in entries {
            payload.extend_from_slice(&flags.to_le_bytes());
            payload.extend_from_slice(&(source.len() as u16).to_le_bytes());
            payload.extend_from_slice(&(target.len() as u16).to_le_bytes());
            payload.extend_from_slice(source);
            payload.extend_from_slice(target);
        }
        payload
    }

    /// Returns the part of the table's entries from `first` up to `end`, each the read-write
    /// mapping of the export's `N` at `/srv/N`.
    fn numbered(first: u32, end: u32) -> Vec<u8> {
        let paths: Vec<(String, String)> = (first..end)
            .map(|entry| (entry.to_string(), format!("/srv/{entry}")))
            .collect();
        let entries: Vec<(&[u8], &[u8], u16)> = paths
            .iter()
            .map(|(source, target)| (source.as_bytes(), target.as_bytes(), 0))
            .collect();
        part(first, &entries, None)
    }

    /// Returns the targets of `mappings`, relative to the root, as text.
    fn targets(mappings: &[Mapping]) -> Vec<String> {
        mappings
            .iter()
            .map(|mapping| mapping.target().display().to_string())
            .collect()
    }

    /// Maps nothing and succeeds, as a root that applies every mapping does.
    fn applies(_: &[Mapping]) -> Result<(), (usize, &'static str)> {
        Ok(())
    }

    #[test]
    fn a_table_takes_the_count_that_the_host_announced() {
        let table = MappingTable::announced(b"quiet nvx_maps=3 console=hvc0\n").unwrap();
        assert_eq!((table.expected(), table.applied()), (3, 0));
        assert!(!table.is_latched() && !table.is_complete());
        // A command line without the token announces none, and such a table is complete.
        let table = MappingTable::announced(b"quiet nvx_map=0/src:/src").unwrap();
        assert_eq!(table, MappingTable::default());
        assert!(table.is_complete());
        for refused in [
            &b"nvx_maps=0"[..],
            b"nvx_maps=4097",
            b"nvx_maps=x",
            b"nvx_maps=",
            b"nvx_maps=1 nvx_maps=1",
        ] {
            assert!(
                MappingTable::announced(refused).is_err(),
                "{}",
                String::from_utf8_lossy(refused)
            );
        }
        // An adapter announces up to as many as the protocol allows.
        let largest = MAX_HOST_MAPPINGS as u32;
        assert_eq!(MappingTable::new(largest).unwrap().expected(), largest);
        assert_eq!(MappingTable::new(largest + 1), None);
        assert!(MappingTable::new(0).unwrap().is_complete());
    }

    #[test]
    fn parts_apply_in_order_until_the_table_is_complete() {
        let mut table = MappingTable::new(5).unwrap();
        let mut handed = Vec::new();
        for (first, end) in [(0, 2), (2, 3), (3, 5)] {
            table
                .apply(&numbered(first, end), |mappings| {
                    handed.push(targets(mappings));
                    applies(mappings)
                })
                .unwrap();
            assert_eq!(table.applied(), end);
            assert_eq!(table.is_complete(), end == 5);
        }
        let expected: [&[&str]; 3] = [&["srv/0", "srv/1"], &["srv/2"], &["srv/3", "srv/4"]];
        assert_eq!(handed, expected);
        // A complete table takes no further part.
        let refused = table.apply(&numbered(5, 6), applies).unwrap_err();
        assert_eq!(
            refused,
            TableError::Refused {
                entry: 5,
                refusal: Refusal::Excess {
                    end: 6,
                    announced: 5,
                },
            }
        );
        assert!(table.is_complete());
    }

    #[test]
    fn mappings_keep_the_paths_and_the_access_of_their_entries() {
        let mut table = MappingTable::new(2).unwrap();
        let payload = part(
            0,
            &[
                (b"0/src", b"/workspace", MAP_READ_ONLY),
                (b"1/\xff", b"/data/\xfe", 0),
            ],
            None,
        );
        table
            .apply(&payload, |mappings| {
                let parts: Vec<_> = mappings
                    .iter()
                    .map(|mapping| {
                        (
                            mapping.source().as_os_str().as_bytes(),
                            mapping.target().as_os_str().as_bytes(),
                            mapping.access(),
                            mapping.kind(),
                        )
                    })
                    .collect();
                assert_eq!(
                    parts,
                    [
                        (&b"0/src"[..], &b"workspace"[..], Access::ReadOnly, None),
                        (b"1/\xff", b"data/\xfe", Access::ReadWrite, None),
                    ]
                );
                applies(mappings)
            })
            .unwrap();
        assert!(table.is_complete());
    }

    #[test]
    fn refused_parts_map_nothing_and_change_nothing() {
        let good = (&b"0/src"[..], &b"/srv/src"[..], 0);
        let cases: [(&str, u32, Vec<u8>, Refusal); 8] = [
            (
                "a part that does not decode",
                3,
                vec![0; 3],
                Refusal::Malformed(protocol::decode_guest_maps_payload(&[0; 3]).unwrap_err()),
            ),
            (
                "a part with trailing bytes",
                3,
                [part(0, &[good], None), vec![0]].concat(),
                Refusal::Malformed(
                    protocol::decode_guest_maps_payload(
                        &[part(0, &[good], None), vec![0]].concat(),
                    )
                    .unwrap_err(),
                ),
            ),
            (
                "a part without an entry",
                3,
                vec![0; MAPS_HEADER_LEN],
                Refusal::Empty,
            ),
            (
                "a part that skips entries",
                3,
                numbered(1, 2),
                Refusal::Order {
                    first: 1,
                    expected: 0,
                },
            ),
            (
                "a part past the announcement",
                3,
                numbered(0, 4),
                Refusal::Excess {
                    end: 4,
                    announced: 3,
                },
            ),
            (
                "a part when the host announced none",
                0,
                numbered(0, 1),
                Refusal::Excess {
                    end: 1,
                    announced: 0,
                },
            ),
            (
                "a part that maps the root of the export",
                3,
                part(0, &[good, (b"", b"/srv/root", 0)], None),
                Refusal::ExportRoot { entry: 1 },
            ),
            (
                "a part that maps the root of the export first",
                3,
                part(0, &[(b"", b"/srv/root", MAP_READ_ONLY)], None),
                Refusal::ExportRoot { entry: 0 },
            ),
        ];
        for (name, expected, payload, refusal) in cases {
            let mut table = MappingTable::new(expected).unwrap();
            let before = table.clone();
            let called = Cell::new(false);
            let refused = table.apply(&payload, |mappings| {
                called.set(true);
                applies(mappings)
            });
            assert_eq!(
                refused,
                Err(TableError::Refused { entry: 0, refusal }),
                "{name}"
            );
            assert!(!called.get(), "{name}");
            assert_eq!(table, before, "{name}");
        }
    }

    #[test]
    fn a_refused_part_leaves_the_table_to_go_on() {
        // A part that repeats entries the table applied is refused at the entry that it expects
        // next, after which the right part applies.
        let mut table = MappingTable::new(4).unwrap();
        table.apply(&numbered(0, 2), applies).unwrap();
        let refused = table.apply(&numbered(0, 2), applies).unwrap_err();
        assert_eq!(
            refused,
            TableError::Refused {
                entry: 2,
                refusal: Refusal::Order {
                    first: 0,
                    expected: 2,
                },
            }
        );
        assert_eq!(refused.entry(), 2);
        assert!(!table.is_latched());
        table.apply(&numbered(2, 4), applies).unwrap();
        assert!(table.is_complete());
    }

    #[test]
    fn a_failed_mapping_keeps_those_before_it_and_latches_the_table() {
        // The root applies the part's first mapping, then fails at its second, the table's
        // fourth.
        let mut table = MappingTable::new(6).unwrap();
        table.apply(&numbered(0, 2), applies).unwrap();
        let failed = table
            .apply(&numbered(2, 5), |mappings| {
                assert_eq!(targets(mappings), ["srv/2", "srv/3", "srv/4"]);
                Err((1, "the target is reserved"))
            })
            .unwrap_err();
        assert_eq!(
            failed,
            TableError::Failed {
                entry: 3,
                error: "the target is reserved",
            }
        );
        assert_eq!(failed.entry(), 3);
        assert_eq!(table.applied(), 3);
        assert!(table.is_latched() && !table.is_complete());
        // The table refuses every later part, wherever it continues, without mapping it.
        for (first, end) in [(3, 4), (5, 6), (3, 6)] {
            let refused = table
                .apply(&numbered(first, end), |_| -> Result<(), (usize, &str)> {
                    panic!("a latched table maps nothing")
                })
                .unwrap_err();
            assert_eq!(
                refused,
                TableError::Refused {
                    entry: 3,
                    refusal: Refusal::Latched,
                }
            );
        }
        assert_eq!(table.applied(), 3);
    }

    #[test]
    fn a_failure_counts_at_most_the_mappings_of_its_part() {
        // A root that fails before its first mapping, as one that cannot open its root does,
        // applies none; one that reports an index past the part's last applies them all.
        for (index, applied) in [(0, 1), (2, 3), (usize::MAX, 3)] {
            let mut table = MappingTable::new(4).unwrap();
            table.apply(&numbered(0, 1), applies).unwrap();
            let failed = table
                .apply(&numbered(1, 3), |_| Err((index, "failed")))
                .unwrap_err();
            assert_eq!(failed.entry(), applied, "{index}");
            assert_eq!(table.applied(), applied, "{index}");
            assert!(table.is_latched() && !table.is_complete(), "{index}");
        }
    }

    #[test]
    fn a_latched_table_refuses_every_part() {
        // A caller latches the table once it refused a part, if its protocol ends the table then.
        let mut table = MappingTable::new(2).unwrap();
        let refused = table.apply(&numbered(1, 2), applies).unwrap_err();
        assert_eq!(refused.entry(), 0);
        table.latch();
        assert!(table.is_latched() && !table.is_complete());
        assert_eq!(
            table.apply(&numbered(0, 2), applies),
            Err(TableError::Refused {
                entry: 0,
                refusal: Refusal::Latched,
            })
        );
        // So does a complete table, which is complete no more.
        let mut table = MappingTable::new(0).unwrap();
        table.latch();
        assert!(!table.is_complete());
    }

    #[test]
    fn every_entry_that_the_decoder_accepts_but_the_export_s_root_becomes_a_mapping() {
        assert_eq!(super::super::MAX_PATH_BYTES, protocol::MAX_MAP_PATH_BYTES);
        let longest = vec![b'a'; protocol::MAX_MAP_PATH_BYTES];
        let target = [b"/".as_slice(), &longest[1..]].concat();
        let payload = part(
            0,
            &[
                (longest.as_slice(), target.as_slice(), MAP_READ_ONLY),
                (b"0/\xff", b"/\xfe", 0),
            ],
            None,
        );
        let mut table = MappingTable::new(2).unwrap();
        table
            .apply(&payload, |mappings| {
                assert_eq!(mappings[0].source(), Path::new(OsStr::from_bytes(&longest)));
                assert_eq!(
                    mappings[0].target(),
                    Path::new(OsStr::from_bytes(&target[1..]))
                );
                applies(mappings)
            })
            .unwrap();
        assert!(table.is_complete());
    }

    #[test]
    fn errors_say_which_entry_and_why() {
        let refused: TableError<&str> = TableError::Refused {
            entry: 2,
            refusal: Refusal::Order {
                first: 0,
                expected: 2,
            },
        };
        assert_eq!(
            refused.to_string(),
            "the mapping table refused a part: it starts at mapping 0, but the table expects \
             mapping 2 next"
        );
        let failed = TableError::Failed {
            entry: 3,
            error: "the target is reserved",
        };
        assert_eq!(
            failed.to_string(),
            "mapping 3 of the table failed: the target is reserved"
        );
    }
}
