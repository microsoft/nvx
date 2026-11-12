use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::guest::mappings::{Mapper, MappingTable, Reserved, SourceRoot, TableError, TargetRoot};
use crate::guest::sys::{self, Errno, Mode};

use super::seam::{System, path_c_string};

/// The direct root's own mounts besides `/`'s, on which the table's mapper may create what a
/// target lacks, and whose roots a mapping may cover: the tmpfs that the guest's init mounts at
/// `/tmp`, below which the mappings of a host's temporary directories land, and which a mapping
/// of the host's `/tmp` itself covers.
const OWN_MOUNTS: [&str; 1] = ["/tmp"];

/// The boot-announced host mapping table of the agent, the root beneath which it maps, and its
/// mapper.
#[derive(Default)]
pub(crate) struct HostMappings {
    /// The table: the number of mappings that the kernel command line announced, the number
    /// mounted in order, and whether a mount failure latched it, which refuses the rest.
    pub(crate) table: MappingTable,
    /// The root beneath which the table maps.
    root: TableRoot,
    /// The table's mapper and the export's root, once the first part has opened them.
    mapper: Option<TableMapper>,
}

/// The root beneath which an agent maps its host mapping table.
#[derive(Debug, Default, PartialEq, Eq)]
enum TableRoot {
    /// The direct agent's own root, `/`.
    #[default]
    Direct,
    /// A layered root's retained mapper and export, never reopened by path.
    Layered,
}

/// The mapper of the host mapping table beneath the table's root, and the root of the host's
/// export, below which the table's sources are.
struct TableMapper {
    /// The mapper, which tracks the table's mappings, and latches at the first that fails.
    mapper: Mapper,
    /// The host export's mount, `hostfs_dir/root`.
    export: Option<SourceRoot>,
}

impl HostMappings {
    /// Adopts a layered root's table and prepared mapping handles without resetting either.
    pub(crate) fn layered(
        table: MappingTable,
        mapper: Mapper,
        export: Option<SourceRoot>,
    ) -> Result<Self, Errno> {
        if table.expected() != 0 && export.is_none() {
            return Err(Errno::ENOENT);
        }
        Ok(Self {
            table,
            root: TableRoot::Layered,
            mapper: Some(TableMapper { mapper, export }),
        })
    }
}

/// Failure returned while validating or applying one MAPS request.
#[derive(Debug)]
pub(crate) enum MappingError {
    /// The payload or sequence does not match the announced table.
    Invalid,
    /// A specific mapping failed while being mounted.
    Mount {
        /// Errno returned by mount preparation or the mount itself.
        status: i32,
        /// Zero-based mapping-table entry that failed.
        entry: u32,
    },
}

/// Reads the announced mapping count and validates the host export mount, which the guest's init
/// mounts for the direct agent.
pub(crate) fn prepare(system: &mut impl System, mappings: &mut HostMappings) -> Result<(), Errno> {
    let mut command_line = [0u8; 4096];
    let count = std::fs::File::open(&system.paths().proc_cmdline)
        .and_then(|mut file| file.read(&mut command_line[..4095]))
        .map_err(|error| sys::errno_of(&error))?;
    mappings.table = MappingTable::announced(&command_line[..count]).map_err(|_| Errno::EINVAL)?;
    let hostfs = system.paths().hostfs_dir.clone();
    let root = hostfs.join("root");
    let mounted = match (std::fs::metadata(&hostfs), std::fs::metadata(&root)) {
        (Ok(parent), Ok(export)) => parent.dev() != export.dev(),
        _ => false,
    };
    if mounted {
        let hostfs = path_c_string(&hostfs)?;
        system.chown(&hostfs, 0, 0)?;
        system.chmod(&hostfs, Mode::from_bits_retain(0o700))?;
    }
    if mappings.table.expected() != 0 && !mounted {
        return Err(Errno::ENOENT);
    }
    Ok(())
}

/// Validates and applies one ordered part of the host mapping table.
///
/// The agent's [`MappingTable`] keeps the table's account: the count that the kernel command
/// line announced, and the entries applied in order. It refuses a part that does not continue the
/// table, and one that maps the export's root, which an empty source names: the agent's export is
/// an aggregate, whose root lists its children, which the agent never maps. A [`Mapper`] beneath
/// the table's root applies each entry, from the host export's mount, `hostfs_dir/root`, and
/// latches, with the table, at the first entry that fails, as the
/// [mappings](crate::guest::mappings) module describes. The direct agent's table root is its own,
/// `/`, on whose mount and the tmpfs at `/tmp`, which a mapping may also cover, the mapper creates
/// what a target lacks, and nothing inside another mapping. The first part that the table hands
/// it opens them, so that a table of zero mappings does no work.
pub(crate) fn apply_request(
    system: &mut impl System,
    mappings: &mut HostMappings,
    payload: &[u8],
) -> Result<(), MappingError> {
    let HostMappings {
        table,
        root,
        mapper,
    } = mappings;
    table
        .apply(payload, |batch| {
            let opened = match mapper.take() {
                Some(opened) => opened,
                None => open_table(system, root).map_err(|error| (0, error as i32))?,
            };
            let opened = mapper.insert(opened);
            let export = opened.export.as_ref().ok_or((0, Errno::ENOENT as i32))?;
            opened
                .mapper
                .apply_with(system.kernel(), export, batch)
                .map_err(|failure| (failure.entry(), failure.error().raw_os_error()))
        })
        .map_err(|error| match error {
            TableError::Refused { .. } => MappingError::Invalid,
            TableError::Failed {
                entry,
                error: status,
            } => MappingError::Mount { status, entry },
        })
}

/// Opens the table's mapper beneath `root`, as [`apply_request`] describes, and the root of the
/// host's export.
fn open_table(system: &mut impl System, root: &TableRoot) -> Result<TableMapper, Errno> {
    let errno = |error: io::Error| sys::errno_of(&error);
    let (root, reserved) = match root {
        TableRoot::Direct => {
            let mut root = TargetRoot::open_with(system.kernel(), Path::new("/")).map_err(errno)?;
            for own in OWN_MOUNTS {
                match root.add_mount_with(system.kernel(), Path::new(own)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(errno(error)),
                }
            }
            (root, Reserved::default())
        }
        TableRoot::Layered => return Err(Errno::EINVAL),
    };
    let export = SourceRoot::open(&system.paths().hostfs_dir.join("root")).map_err(errno)?;
    Ok(TableMapper {
        mapper: Mapper::new(root, reserved),
        export: Some(export),
    })
}
#[cfg(test)]
mod tests;
