use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::guest::mappings::{
    Access, InvalidMapping, Mapper, Mapping, Reserved, SourceRoot, TargetRoot,
};
use crate::guest::sys::{self, Errno, Mode};
use crate::protocol::{self, GuestHostMap};

use super::AgentConfig;
use super::diagnostics;
use super::seam::{System, path_c_string};

/// The direct root's own mounts besides `/`'s, on which the table's mapper may create what a
/// target lacks, and whose roots a mapping may cover: the tmpfs that the guest's init mounts at
/// `/tmp`, below which the mappings of a host's temporary directories land, and which a mapping
/// of the host's `/tmp` itself covers.
const OWN_MOUNTS: [&str; 1] = ["/tmp"];

/// Progress and latched failure state of the boot-announced host mapping table.
#[derive(Default)]
pub(crate) struct HostMappings {
    /// Number of mappings announced on the kernel command line.
    pub(crate) expected: u32,
    /// Number of mappings mounted in order.
    pub(crate) applied: u32,
    /// Whether a prior mount failure permanently refused the remaining table.
    pub(crate) failed: bool,
    /// The root beneath which the table maps.
    root: TableRoot,
    /// The table's mapper and the export's root, once the first batch has opened them.
    table: Option<Table>,
}

/// The root beneath which an agent maps its host mapping table.
#[derive(Debug, Default, PartialEq, Eq)]
enum TableRoot {
    /// The direct agent's own root, `/`.
    #[default]
    Direct,
    /// The sandbox agent's `ROOTFS`, the container's assembled root.
    Sandbox(PathBuf),
}

/// The mapper of the host mapping table beneath the table's root, and the root of the host's
/// export, below which the table's sources are.
struct Table {
    /// The mapper, which tracks and latches the table's mappings.
    mapper: Mapper,
    /// The host export's mount, `hostfs_dir/root`.
    export: SourceRoot,
}

impl HostMappings {
    /// Returns the state of the table of the agent that `config` configures, before
    /// [`prepare`] reads its announcement: the direct agent maps beneath `/`, and the sandbox
    /// agent beneath `ROOTFS`.
    pub(crate) fn new(config: &AgentConfig) -> Self {
        let root = if config.direct {
            TableRoot::Direct
        } else {
            let rootfs = OsStr::from_bytes(config.rootfs.as_bytes());
            TableRoot::Sandbox(PathBuf::from(rootfs))
        };
        Self {
            root,
            ..Self::default()
        }
    }
}

#[cfg(test)]
impl HostMappings {
    /// Returns the state of a direct agent's table of `expected` announced mappings, none
    /// applied yet.
    pub(crate) fn announced(expected: u32) -> Self {
        Self {
            expected,
            ..Self::default()
        }
    }

    /// Returns the state of the table of `expected` announced mappings of the agent that
    /// `config` configures, none applied yet.
    pub(crate) fn announced_for(config: &AgentConfig, expected: u32) -> Self {
        Self {
            expected,
            ..Self::new(config)
        }
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

/// Reads the announced mapping count and validates the host export mount, in either mode: the
/// guest's init mounts the export for the direct agent, and the init agent for the sandbox
/// agent, at the same place.
pub(crate) fn prepare(system: &mut impl System, mappings: &mut HostMappings) -> Result<(), Errno> {
    let mut command_line = [0u8; 4096];
    let count = std::fs::File::open(&system.paths().proc_cmdline)
        .and_then(|mut file| file.read(&mut command_line[..4095]))
        .map_err(|error| sys::errno_of(&error))?;
    mappings.expected =
        protocol::parse_host_mapping_count(&command_line[..count]).map_err(|_| Errno::EINVAL)?;
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
    if mappings.expected != 0 && !mounted {
        return Err(Errno::ENOENT);
    }
    Ok(())
}

/// Validates and applies one ordered batch of host mappings.
///
/// The agent keeps the table's bookkeeping: the count that the kernel command line announced,
/// and the order of the batches. A [`Mapper`] beneath the table's root applies each entry, from
/// the host export's mount, `hostfs_dir/root`, and latches the table at the first entry that
/// fails, as the [mappings](crate::guest::mappings) module describes. The direct agent's table
/// root is its own, `/`, on whose mount and the tmpfs at `/tmp`, which a mapping may also cover,
/// the mapper creates what a target lacks. The sandbox agent's is `ROOTFS`, the container's
/// assembled root, on whose mount alone it creates what a target lacks, and whose runtime's
/// targets it refuses: [`Reserved::container_root`]. Either creates nothing inside another
/// mapping. The first batch opens them, so that a table of zero mappings does no work.
pub(crate) fn apply_request(
    system: &mut impl System,
    mappings: &mut HostMappings,
    payload: &[u8],
) -> Result<(), MappingError> {
    if mappings.failed {
        return Err(MappingError::Invalid);
    }
    let request =
        protocol::decode_guest_maps_payload(payload).map_err(|_| MappingError::Invalid)?;
    let count = u32::try_from(request.maps.len()).map_err(|_| MappingError::Invalid)?;
    if request.first != mappings.applied
        || count == 0
        || mappings.applied > mappings.expected
        || count > mappings.expected - mappings.applied
    {
        return Err(MappingError::Invalid);
    }
    let first = request.first;
    let batch = request
        .maps
        .into_iter()
        .map(mapping_of)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| MappingError::Invalid)?;
    let table = match mappings.table.take() {
        Some(table) => table,
        None => match open_table(system, &mappings.root) {
            Ok(table) => table,
            Err(error) => {
                mappings.failed = true;
                return Err(MappingError::Mount {
                    status: error as i32,
                    entry: first,
                });
            }
        },
    };
    let table = mappings.table.insert(table);
    match table
        .mapper
        .apply_with(system.kernel(), &table.export, &batch)
    {
        Ok(()) => {
            mappings.applied += count;
            Ok(())
        }
        Err(failure) => {
            // The batch holds at most `count` entries, which fit in a `u32`.
            let entry = failure.entry() as u32;
            mappings.applied += entry;
            mappings.failed = true;
            Err(MappingError::Mount {
                status: failure.error().raw_os_error(),
                entry: first + entry,
            })
        }
    }
}

/// Opens the table's mapper beneath `root`, as [`apply_request`] describes, and the root of the
/// host's export.
fn open_table(system: &mut impl System, root: &TableRoot) -> Result<Table, Errno> {
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
        TableRoot::Sandbox(rootfs) => (
            TargetRoot::open_with(system.kernel(), rootfs).map_err(errno)?,
            Reserved::container_root(),
        ),
    };
    let export = SourceRoot::open(&system.paths().hostfs_dir.join("root")).map_err(errno)?;
    Ok(Table {
        mapper: Mapper::new(root, reserved),
        export,
    })
}

/// Unmounts the table's mappings in reverse order, as the sandbox agent does before it returns
/// to the init agent, which then unmounts the root that holds them. It attempts each, whatever
/// failed before, reports each failure with its zero-based entry, and returns the error of the
/// first failure, after which those mappings stay mounted.
pub(crate) fn teardown(system: &mut impl System, mappings: &mut HostMappings) -> Result<(), Errno> {
    let Some(mut table) = mappings.table.take() else {
        return Ok(());
    };
    let failures = table.mapper.unmount_with(system.kernel());
    let mut first = None;
    for (entry, failure) in failures {
        let status = failure.error().raw_os_error();
        // The table holds at most `MAX_HOST_MAPPINGS` entries, whose indices fit in a `u32`.
        diagnostics::unmapping_error(system, status, entry as u32);
        first.get_or_insert(status);
    }
    match first {
        Some(status) => Err(Errno::from_raw(status)),
        None => Ok(()),
    }
}

/// Returns the library's mapping of one decoded entry, which the decoder's path rules let
/// through unchanged, and refuses an entry of the export's root, which an empty source names: the
/// decoder and the library take it for the root of a directory export, but the agent's export is
/// an aggregate, whose root lists its children, which the agent never maps.
///
/// The paths are the decoder's buffers, which it bounds: it refuses a payload of more than
/// `MAX_HOST_MAPPINGS` entries before it allocates them, and a source or target of more than
/// `MAX_MAP_PATH_BYTES` bytes.
fn mapping_of(entry: GuestHostMap) -> Result<Mapping, InvalidMapping> {
    if entry.source.is_empty() {
        return Err(InvalidMapping::Source);
    }
    let path = |bytes: Vec<u8>| PathBuf::from(OsString::from_vec(bytes));
    let access = if entry.read_only {
        Access::ReadOnly
    } else {
        Access::ReadWrite
    };
    Mapping::new(path(entry.source), path(entry.target), access)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    use super::super::seam::{TestSystem, attach_calls};
    use super::*;
    use crate::guest::root::Operation;

    /// Returns a system with a host export, a table of `expected` mappings, and the canonical
    /// path of the directory where the tests' targets go, which no symbolic link leads through.
    fn setup(expected: u32) -> (tempfile::TempDir, TestSystem, HostMappings, PathBuf) {
        let (temporary, system) = TestSystem::with_host_export();
        let guest = temporary.path().canonicalize().unwrap().join("guest");
        (temporary, system, HostMappings::announced(expected), guest)
    }

    /// Returns the canonical path of `relative` in the system's host export.
    fn exported(system: &TestSystem, relative: &str) -> PathBuf {
        system
            .paths
            .hostfs_dir
            .join("root")
            .canonicalize()
            .unwrap()
            .join(relative)
    }

    /// Returns a system with a host export, the state of a sandbox agent's table of `expected`
    /// mappings, and the canonical path of the agent's assembled root, which is the root of a
    /// mount.
    fn sandbox(expected: u32) -> (tempfile::TempDir, TestSystem, HostMappings, PathBuf) {
        let (temporary, system) = TestSystem::with_host_export();
        let rootfs = temporary.path().canonicalize().unwrap().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        system.kernel.add_mount(&rootfs);
        let config = AgentConfig::for_test(Some(&rootfs));
        let mappings = HostMappings::announced_for(&config, expected);
        (temporary, system, mappings, rootfs)
    }

    /// Applies three mappings of a sandbox's table: a read-only directory at `/workspace`, a file
    /// at `/data/notes.txt`, and a directory at `/out`, whose paths below `rootfs` it returns.
    fn map_three(
        system: &mut TestSystem,
        mappings: &mut HostMappings,
        rootfs: &Path,
    ) -> [PathBuf; 3] {
        let request = payload(
            0,
            &[
                (b"0/src", b"/workspace", protocol::MAP_READ_ONLY),
                (b"0/notes.txt", b"/data/notes.txt", 0),
                (b"1", b"/out", 0),
            ],
            None,
        );
        apply_request(system, mappings, &request).unwrap();
        ["workspace", "data/notes.txt", "out"].map(|target| rootfs.join(target))
    }

    #[test]
    fn a_sandbox_table_unmounts_in_reverse_order() {
        let (_temporary, mut system, mut mappings, rootfs) = sandbox(3);
        let [workspace, notes, out] = map_three(&mut system, &mut mappings, &rootfs);
        assert_eq!(mappings.applied, 3);
        teardown(&mut system, &mut mappings).unwrap();
        let shown = |path: &PathBuf| path.display().to_string();
        assert_eq!(
            system.kernel.unmounts(),
            [&out, &notes, &workspace].map(shown)
        );
        for target in [&workspace, &notes, &out] {
            assert_eq!(system.kernel.mount_at(target), None);
        }
        // The teardown forgot the table, so another has nothing to do.
        teardown(&mut system, &mut mappings).unwrap();
        assert_eq!(system.kernel.unmounts().len(), 3);
        assert!(system.diagnostics.is_empty());
    }

    #[test]
    fn a_sandbox_table_reports_each_mapping_that_stays_mounted() {
        let (_temporary, mut system, mut mappings, rootfs) = sandbox(3);
        let [workspace, notes, out] = map_three(&mut system, &mut mappings, &rootfs);
        system.kernel.fail(Operation::Unmount, &notes, Errno::EBUSY);
        assert_eq!(teardown(&mut system, &mut mappings), Err(Errno::EBUSY));
        // Every mapping is attempted, in reverse order, and only the busy one stays mounted.
        let shown = |path: &PathBuf| path.display().to_string();
        assert_eq!(
            system.kernel.unmounts(),
            [&out, &notes, &workspace].map(shown)
        );
        assert!(system.kernel.mount_at(&notes).is_some());
        assert_eq!(system.kernel.mount_at(&workspace), None);
        assert_eq!(system.kernel.mount_at(&out), None);
        assert_eq!(
            String::from_utf8_lossy(&system.diagnostics),
            format!(
                "NVX-MANAGED-ERROR: stage=host-unmapping status={}\n",
                Errno::EBUSY as i32
            )
        );
    }

    #[test]
    fn a_table_without_mappings_has_nothing_to_tear_down() {
        let (_temporary, mut system, mut mappings, _rootfs) = sandbox(0);
        teardown(&mut system, &mut mappings).unwrap();
        assert!(system.kernel.calls().is_empty());
    }

    fn payload(first: u32, entries: &[(&[u8], &[u8], u16)], count: Option<u32>) -> Vec<u8> {
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

    #[test]
    fn mappings_mount_in_order_and_unblock_workloads() {
        // Under a restrictive umask, which the modes of what the table creates ignore.
        sys::with_thread_umask(0o077, || {
            let (_temporary, mut system, mut mappings, guest) = setup(3);
            let source = guest.join("src");
            let notes = guest.join("notes.txt");
            let out = guest.join("out");
            let first = payload(
                0,
                &[
                    (
                        b"0/src",
                        source.as_os_str().as_bytes(),
                        protocol::MAP_READ_ONLY,
                    ),
                    (b"0/notes.txt", notes.as_os_str().as_bytes(), 0),
                ],
                None,
            );
            apply_request(&mut system, &mut mappings, &first).unwrap();
            assert_eq!((mappings.applied, mappings.expected), (2, 3));
            let second = payload(2, &[(b"1", out.as_os_str().as_bytes(), 0)], None);
            apply_request(&mut system, &mut mappings, &second).unwrap();
            assert_eq!((mappings.applied, mappings.expected), (3, 3));
            // Each mapping is a copy of its source's mount, restricted before it is attached.
            let expected: Vec<_> = [
                (exported(&system, "0/src"), &source, Access::ReadOnly),
                (exported(&system, "0/notes.txt"), &notes, Access::ReadWrite),
                (exported(&system, "1"), &out, Access::ReadWrite),
            ]
            .into_iter()
            .flat_map(|(from, to, access)| attach_calls(&from, to, access))
            .collect();
            assert_eq!(system.kernel.calls(), expected);
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
            assert!(source.is_dir() && notes.is_file() && out.is_dir());
            assert_eq!(
                [&guest, &source, &notes, &out].map(|path| mode(path)),
                [0o755, 0o755, 0o644, 0o755]
            );
        });
    }

    #[test]
    fn targets_are_created_only_on_the_agent_s_own_mounts() {
        // Not inside an earlier mapping, whose source does not hold the target.
        let (_temporary, mut system, mut mappings, guest) = setup(2);
        let source = guest.join("src");
        let nested = guest.join("src/notes.txt");
        let request = payload(
            0,
            &[
                (b"0/src", source.as_os_str().as_bytes(), 0),
                (b"0/notes.txt", nested.as_os_str().as_bytes(), 0),
            ],
            None,
        );
        assert!(matches!(
            apply_request(&mut system, &mut mappings, &request),
            Err(MappingError::Mount { status, entry: 1 }) if status == Errno::EXDEV as i32
        ));
        assert_eq!((mappings.applied, mappings.failed), (1, true));
        assert!(!nested.exists());
        // Nor on another mount, such as a file system that the guest mounted there.
        let (_temporary, mut system, mut mappings, guest) = setup(1);
        let foreign = guest.join("foreign");
        fs::create_dir_all(&foreign).unwrap();
        system.kernel.add_mount(&foreign);
        let target = foreign.join("target");
        let request = payload(0, &[(b"0/src", target.as_os_str().as_bytes(), 0)], None);
        assert!(matches!(
            apply_request(&mut system, &mut mappings, &request),
            Err(MappingError::Mount { status, entry: 0 }) if status == Errno::EXDEV as i32
        ));
        assert!(!target.exists());
        assert!(system.kernel.moves().is_empty());
    }

    #[test]
    fn a_mapping_of_the_host_s_tmp_covers_the_guest_s_tmpfs() {
        // The tmpfs that the guest's init mounts at `/tmp` is one of the table's own mounts.
        let (_temporary, mut system, mut mappings, _guest) = setup(1);
        let tmp = Path::new("/tmp");
        let tmpfs = system.kernel.add_mount(tmp);
        let request = payload(0, &[(b"1".as_slice(), b"/tmp".as_slice(), 0)], None);
        apply_request(&mut system, &mut mappings, &request).unwrap();
        assert_eq!((mappings.applied, mappings.failed), (1, false));
        assert_eq!(
            system.kernel.calls(),
            attach_calls(&exported(&system, "1"), tmp, Access::ReadWrite)
        );
        let covering = system.kernel.mount_at(tmp).unwrap();
        assert_ne!(covering, tmpfs);
    }

    #[test]
    fn the_runtime_s_time_tree_and_its_ancestors_are_refused() {
        for target in [
            &b"/run/nvx/time"[..],
            b"/run/nvx/time/x",
            b"/run/nvx",
            b"/run",
        ] {
            let (_temporary, mut system, mut mappings, _guest) = setup(1);
            let request = payload(0, &[(b"0/src", target, 0)], None);
            assert!(matches!(
                apply_request(&mut system, &mut mappings, &request),
                Err(MappingError::Mount { status, entry: 0 }) if status == Errno::EPERM as i32
            ));
            assert!(mappings.failed);
            assert!(system.kernel.calls().is_empty());
        }
    }

    #[test]
    fn entries_beyond_the_announcement_or_out_of_order_are_refused() {
        let (_temporary, mut system, mut mappings, guest) = setup(1);
        let target = guest.join("src");
        let entry = [(b"0/src".as_slice(), target.as_os_str().as_bytes(), 0)];
        for invalid in [payload(1, &entry, None), payload(0, &entry, Some(2))] {
            assert!(matches!(
                apply_request(&mut system, &mut mappings, &invalid),
                Err(MappingError::Invalid)
            ));
        }
        apply_request(&mut system, &mut mappings, &payload(0, &entry, None)).unwrap();
        assert!(matches!(
            apply_request(&mut system, &mut mappings, &payload(1, &entry, None)),
            Err(MappingError::Invalid)
        ));
        assert_eq!(system.kernel.moves(), [target.display().to_string()]);
    }

    #[test]
    fn malformed_tables_mount_nothing() {
        let (_temporary, mut system, mut mappings, guest) = setup(2);
        let target = guest.join("x");
        let good = (b"0/src".as_slice(), target.as_os_str().as_bytes(), 0);
        let malformed = [
            payload(
                0,
                &[good, (b"0/../1", target.as_os_str().as_bytes(), 0)],
                None,
            ),
            payload(
                0,
                &[good, (b"/0/src", target.as_os_str().as_bytes(), 0)],
                None,
            ),
            payload(0, &[good, (b"0/src", b"relative", 0)], None),
            payload(0, &[good, (b".", target.as_os_str().as_bytes(), 0)], None),
            payload(
                0,
                &[good, (b"0//src", target.as_os_str().as_bytes(), 0)],
                None,
            ),
            payload(
                0,
                &[good, (b"0/s\0rc", target.as_os_str().as_bytes(), 0)],
                None,
            ),
            payload(
                0,
                &[good, (b"0/src", target.as_os_str().as_bytes(), 2)],
                None,
            ),
            [payload(0, &[good], None), vec![0]].concat(),
            payload(0, &[good], Some(2)),
            payload(0, &[], Some(0)),
            vec![0, 0, 0],
        ];
        for payload in malformed {
            assert!(matches!(
                apply_request(&mut system, &mut mappings, &payload),
                Err(MappingError::Invalid)
            ));
        }
        assert!(system.kernel.calls().is_empty());
        assert_eq!(mappings.applied, 0);
        assert!(!target.exists());
    }

    #[test]
    fn the_root_of_the_export_is_refused_though_it_decodes() {
        // An empty source names the export's root, which a directory export maps, but the
        // agent's export is an aggregate, whose root lists its children: the agent refuses the
        // table that holds one, as it refuses a malformed one, and mounts none of it.
        let (_temporary, mut system, mut mappings, guest) = setup(2);
        let (first, root) = (guest.join("first"), guest.join("root"));
        let request = payload(
            0,
            &[
                (b"0/src", first.as_os_str().as_bytes(), 0),
                (b"", root.as_os_str().as_bytes(), protocol::MAP_READ_ONLY),
            ],
            None,
        );
        let decoded = protocol::decode_guest_maps_payload(&request).unwrap();
        assert!(decoded.maps[1].source.is_empty());
        assert!(matches!(
            apply_request(&mut system, &mut mappings, &request),
            Err(MappingError::Invalid)
        ));
        assert!(system.kernel.calls().is_empty());
        assert_eq!((mappings.applied, mappings.failed), (0, false));
        assert!(!first.exists() && !root.exists());
        // So does a sandbox agent, whose export is an aggregate too.
        let (_temporary, mut system, mut mappings, rootfs) = sandbox(1);
        let request = payload(0, &[(b"", b"/workspace", 0)], None);
        assert!(matches!(
            apply_request(&mut system, &mut mappings, &request),
            Err(MappingError::Invalid)
        ));
        assert!(system.kernel.calls().is_empty());
        assert!(!rootfs.join("workspace").exists());
    }

    #[test]
    fn without_an_announcement_tables_are_refused() {
        let (_temporary, mut system, mut mappings, guest) = setup(0);
        let target = guest.join("src");
        let request = payload(0, &[(b"0/src", target.as_os_str().as_bytes(), 0)], None);
        assert!(matches!(
            apply_request(&mut system, &mut mappings, &request),
            Err(MappingError::Invalid)
        ));
        assert!(system.kernel.calls().is_empty());
    }

    #[test]
    fn a_failed_mount_latches_and_keeps_refusing_tables() {
        let (_temporary, mut system, mut mappings, guest) = setup(2);
        let target = guest.join("src");
        system
            .kernel
            .fail(Operation::MoveMount, &target, Errno::ENOENT);
        let request = payload(0, &[(b"0/src", target.as_os_str().as_bytes(), 0)], None);
        assert!(matches!(
            apply_request(&mut system, &mut mappings, &request),
            Err(MappingError::Mount { status, entry: 0 }) if status == Errno::ENOENT as i32
        ));
        assert!(mappings.failed);
        assert!(matches!(
            apply_request(&mut system, &mut mappings, &request),
            Err(MappingError::Invalid)
        ));
        assert_eq!(mappings.applied, 0);
        assert_eq!(system.kernel.mount_at(&target), None);
    }

    #[test]
    fn a_failure_in_the_middle_of_a_batch_keeps_earlier_mappings_and_latches() {
        // The second entry fails at its attach, or at its missing source.
        for (fail_attach, source) in [(true, b"0/notes.txt".as_slice()), (false, b"0/missing")] {
            let (_temporary, mut system, mut mappings, guest) = setup(3);
            let [first, second, third] = ["first", "second", "third"].map(|name| guest.join(name));
            if fail_attach {
                system
                    .kernel
                    .fail(Operation::MoveMount, &second, Errno::ENOENT);
            }
            let request = payload(
                0,
                &[
                    (
                        b"0/src",
                        first.as_os_str().as_bytes(),
                        protocol::MAP_READ_ONLY,
                    ),
                    (source, second.as_os_str().as_bytes(), 0),
                    (b"1", third.as_os_str().as_bytes(), 0),
                ],
                None,
            );
            assert!(matches!(
                apply_request(&mut system, &mut mappings, &request),
                Err(MappingError::Mount { status, entry: 1 }) if status == Errno::ENOENT as i32
            ));
            assert_eq!((mappings.applied, mappings.failed), (1, true));
            // The first mapping stays mounted, and the third is never attempted.
            assert!(system.kernel.mount_at(&first).is_some());
            assert_eq!(system.kernel.mount_at(&second), None);
            assert!(first.is_dir());
            // The latch refuses the rest of the table, wherever it continues.
            let calls = system.kernel.calls();
            for continued in [1, 2] {
                let rest = payload(continued, &[(b"1", third.as_os_str().as_bytes(), 0)], None);
                assert!(matches!(
                    apply_request(&mut system, &mut mappings, &rest),
                    Err(MappingError::Invalid)
                ));
            }
            assert_eq!(system.kernel.calls(), calls);
            assert_eq!(mappings.applied, 1);
            assert!(!third.exists());
        }
    }

    #[test]
    fn every_entry_that_the_decoder_accepts_but_the_export_s_root_becomes_a_library_mapping() {
        assert_eq!(
            crate::guest::mappings::MAX_PATH_BYTES,
            protocol::MAX_MAP_PATH_BYTES
        );
        let longest = vec![b'a'; protocol::MAX_MAP_PATH_BYTES];
        let target = [b"/".as_slice(), &longest[1..]].concat();
        let request = protocol::decode_guest_maps_payload(&payload(
            0,
            &[
                (
                    longest.as_slice(),
                    target.as_slice(),
                    protocol::MAP_READ_ONLY,
                ),
                (b"0/\xff", b"/\xfe", 0),
                (b"", b"/root", 0),
            ],
            None,
        ))
        .unwrap();
        let mut entries = request.maps;
        let root = entries.pop().unwrap();
        assert_eq!(mapping_of(root), Err(InvalidMapping::Source));
        let converted = entries
            .into_iter()
            .map(mapping_of)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let parts = |mapping: &Mapping| {
            (
                mapping.source().as_os_str().as_bytes().to_vec(),
                mapping.target().as_os_str().as_bytes().to_vec(),
                mapping.access(),
            )
        };
        assert_eq!(
            converted.iter().map(parts).collect::<Vec<_>>(),
            [
                (longest, target[1..].to_vec(), Access::ReadOnly),
                (b"0/\xff".to_vec(), b"\xfe".to_vec(), Access::ReadWrite),
            ]
        );
    }
}
