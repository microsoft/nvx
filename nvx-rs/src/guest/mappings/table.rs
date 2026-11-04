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
/// root of its choice, from the host export: through a mapper beneath a guest's own `/` or a
/// container's root, for example, or through a layered root.
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
/// as a mapper's teardown, which reports each mapping that stays mounted, or a layered root's.
///
/// NVX's managed agent applies its table through one, beneath its own root, `/`, in direct mode,
/// and beneath the container's assembled root in sandbox mode, with a mapper that it opens at the
/// first part that the table hands it, so that a table of zero mappings opens nothing.
///
/// ```no_run
/// use nvx::guest::mappings::{Mapping, MappingTable, TableError};
///
/// # fn parts() -> Vec<Vec<u8>> { Vec::new() }
/// # fn mount(_: &Mapping) -> std::io::Result<()> { Ok(()) }
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let mut table = MappingTable::announced(&std::fs::read("/proc/cmdline")?)?;
/// // Each part of the table, in the order in which the host sends them.
/// for part in parts() {
///     // `mount` mounts one mapping beneath the agent's root, from the host export.
///     let applied = table.apply(&part, |mappings| {
///         mappings.iter().enumerate().try_for_each(|(index, mapping)| {
///             mount(mapping).map_err(|error| (index, error))
///         })
///     });
///     match applied {
///         Ok(()) => println!("{} of {} mappings applied", table.applied(), table.expected()),
///         // The host learns of the entry, and of why, as its protocol has it.
///         Err(TableError::Refused { entry, refusal }) => println!("refused at {entry}: {refusal}"),
///         Err(error) => println!("{error}"),
///     }
/// }
/// // Start workloads only once the table is complete; the root keeps the mappings until its
/// // teardown.
/// assert!(table.is_complete());
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
mod tests;
