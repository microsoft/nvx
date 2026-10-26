//! `MAPS` payloads: the host mapping table that [`APP_MAPS`](super::APP_MAPS) requests hand the
//! guest agent.
//!
//! A payload holds the index of its first entry (`u32`) and its number of entries (`u32`),
//! followed by the entries. Each entry holds its flags (`u16`), the lengths of its source and
//! target (`u16` each), and then both. A source names a path relative to the guest's mount of the
//! host's export, and an empty source names the export's root itself, which is how the table of a
//! directory export maps the whole directory. NVX's managed agent, whose export is an aggregate,
//! whose root lists its children, refuses an empty source.

use super::{ProtocolError, u16_at, u32_at, violation};

/// Largest number of host mappings that the guest agent accepts.
pub const MAX_HOST_MAPPINGS: usize = 4096;
/// Largest source or target of one host mapping: Linux's `PATH_MAX` without the terminating NUL.
pub const MAX_MAP_PATH_BYTES: usize = 4095;
/// Length of the header of a `MAPS` payload.
pub const MAPS_HEADER_LEN: usize = 8;
/// Length of the header of one entry of a `MAPS` payload.
pub const MAP_ENTRY_HEADER_LEN: usize = 6;
/// Flag of an entry that the guest agent mounts read-only.
pub const MAP_READ_ONLY: u16 = 1 << 0;

/// One bind mount of the host mapping table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMap {
    /// Path of the mapped object relative to the guest's mount of the export, `/`-separated, or
    /// empty for the export's root itself.
    pub source: String,
    /// Absolute guest path at which the guest agent mounts it.
    pub target: String,
    /// Whether the guest agent mounts it read-only.
    pub read_only: bool,
}

/// Encodes the host mapping table as the payloads of consecutive `APP_MAPS` requests, each at
/// most `limit` bytes long.
///
/// A payload holds the index of its first entry and its number of entries, so the guest agent
/// applies the entries in order and only up to the number that the kernel command line
/// announced. Each entry holds its flags, the lengths of its source and target, and then both.
/// The guest agent requires 1 to [`MAX_HOST_MAPPINGS`] entries, each with a source that is empty,
/// for the export's root, or relative, and an absolute target, each of at most
/// [`MAX_MAP_PATH_BYTES`] without NUL bytes, whose `/`-separated components are neither empty,
/// `.`, nor `..`.
pub fn encode_maps_payloads(maps: &[HostMap], limit: usize) -> Result<Vec<Vec<u8>>, ProtocolError> {
    if maps.is_empty() || maps.len() > MAX_HOST_MAPPINGS {
        return Err(violation(format!(
            "a host mapping table holds 1 to {MAX_HOST_MAPPINGS} entries"
        )));
    }
    let mut payloads = Vec::new();
    let mut payload = Vec::new();
    let mut count = 0u32;
    for (index, map) in maps.iter().enumerate() {
        if !acceptable(map.source.as_bytes(), map.target.as_bytes()) {
            return Err(violation(format!(
                "a host mapping needs a source that is empty, for the export's root, or relative, \
                 and an absolute target, each of at most {MAX_MAP_PATH_BYTES} bytes without NUL \
                 characters or empty, `.`, or `..` components"
            )));
        }
        let (source, target) = (map.source.as_bytes(), map.target.as_bytes());
        let entry_len = MAP_ENTRY_HEADER_LEN + source.len() + target.len();
        if MAPS_HEADER_LEN + entry_len > limit {
            return Err(violation(
                "a host mapping exceeds the control protocol limit",
            ));
        }
        if count > 0 && payload.len() + entry_len > limit {
            payload[4..8].copy_from_slice(&count.to_le_bytes());
            payloads.push(std::mem::take(&mut payload));
            count = 0;
        }
        if count == 0 {
            let first = u32::try_from(index).unwrap_or(u32::MAX);
            payload.extend_from_slice(&first.to_le_bytes());
            payload.extend_from_slice(&0u32.to_le_bytes());
        }
        let flags = if map.read_only { MAP_READ_ONLY } else { 0 };
        let length = |bytes: &[u8]| u16::try_from(bytes.len()).unwrap_or(u16::MAX);
        payload.extend_from_slice(&flags.to_le_bytes());
        payload.extend_from_slice(&length(source).to_le_bytes());
        payload.extend_from_slice(&length(target).to_le_bytes());
        payload.extend_from_slice(source);
        payload.extend_from_slice(target);
        count += 1;
    }
    payload[4..8].copy_from_slice(&count.to_le_bytes());
    payloads.push(payload);
    Ok(payloads)
}

/// A `MAPS` payload as the guest agent reads it: consecutive entries of the host mapping table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapsPayload {
    /// Index in the table of the payload's first entry.
    pub first: u32,
    /// The entries, in table order.
    pub maps: Vec<HostMap>,
}

/// One host mapping as the guest reads it, without imposing UTF-8 on Linux paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestHostMap {
    /// Path relative to the guest's mount of the export, or empty for the export's root itself.
    pub source: Vec<u8>,
    /// Absolute path at which the guest bind-mounts the source.
    pub target: Vec<u8>,
    /// Whether the bind is remounted read-only.
    pub read_only: bool,
}

/// A `MAPS` payload as the byte-oriented guest decoder reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestMapsPayload {
    /// Index in the table of the payload's first entry.
    pub first: u32,
    /// Entries in table order.
    pub maps: Vec<GuestHostMap>,
}

/// Decodes the payload of an `APP_MAPS` request.
///
/// It refuses what the guest agent refuses of a payload and its entries: a truncated payload,
/// flags other than [`MAP_READ_ONLY`], text that is not UTF-8, a source or target that
/// [`encode_maps_payloads`] would refuse, and trailing bytes. Whether the entries continue the
/// table and fit the number that the kernel command line announced is for the guest agent to
/// check.
pub fn decode_maps_payload(payload: &[u8]) -> Result<MapsPayload, ProtocolError> {
    let first = u32_at(payload, 0).ok_or_else(truncated)?;
    let count = u32_at(payload, 4).ok_or_else(truncated)?;
    let mut maps = Vec::new();
    decode_maps_entries(
        payload,
        count,
        &mut maps,
        |bytes| {
            String::from_utf8(bytes.to_vec())
                .map_err(|_| violation("a host mapping is not valid UTF-8"))
        },
        |source, target, read_only| HostMap {
            source,
            target,
            read_only,
        },
    )?;
    Ok(MapsPayload { first, maps })
}

/// Decodes a `MAPS` payload with the C guest agent's byte-oriented path rules.
///
/// [`decode_maps_payload`] remains UTF-8 for host and fake-OpenVMM callers. This decoder accepts
/// every non-NUL Linux path byte that the guest can mount.
pub fn decode_guest_maps_payload(payload: &[u8]) -> Result<GuestMapsPayload, ProtocolError> {
    let first = u32_at(payload, 0).ok_or_else(truncated)?;
    let count = u32_at(payload, 4).ok_or_else(truncated)?;
    let capacity = usize::try_from(count)
        .ok()
        .filter(|count| *count <= MAX_HOST_MAPPINGS)
        .ok_or_else(|| violation("the host mapping request has too many entries"))?;
    let mut maps = Vec::with_capacity(capacity);
    decode_maps_entries(
        payload,
        count,
        &mut maps,
        |bytes| Ok(bytes.to_vec()),
        |source, target, read_only| GuestHostMap {
            source,
            target,
            read_only,
        },
    )?;
    Ok(GuestMapsPayload { first, maps })
}

/// Decodes the `count` entries that follow the header of a `MAPS` payload into `maps`, as both
/// decoders read them: `path` converts each source and target as soon as it is read, and `entry`
/// builds an entry from its source, its target, and whether it is read-only.
///
/// It refuses a truncated entry, flags other than [`MAP_READ_ONLY`], a source or target that
/// [`acceptable`] refuses, and trailing bytes.
fn decode_maps_entries<P: AsRef<[u8]>, M>(
    payload: &[u8],
    count: u32,
    maps: &mut Vec<M>,
    path: impl Fn(&[u8]) -> Result<P, ProtocolError>,
    entry: impl Fn(P, P, bool) -> M,
) -> Result<(), ProtocolError> {
    let mut offset = MAPS_HEADER_LEN;
    for _ in 0..count {
        let (Some(flags), Some(source_len), Some(target_len)) = (
            u16_at(payload, offset),
            u16_at(payload, offset.saturating_add(2)),
            u16_at(payload, offset.saturating_add(4)),
        ) else {
            return Err(truncated());
        };
        offset = offset
            .checked_add(MAP_ENTRY_HEADER_LEN)
            .ok_or_else(truncated)?;
        let mut take = |length: u16| -> Result<P, ProtocolError> {
            let end = offset
                .checked_add(usize::from(length))
                .ok_or_else(truncated)?;
            let bytes = payload.get(offset..end).ok_or_else(truncated)?;
            offset = end;
            path(bytes)
        };
        let source = take(source_len)?;
        let target = take(target_len)?;
        if flags & !MAP_READ_ONLY != 0 || !acceptable(source.as_ref(), target.as_ref()) {
            return Err(violation(
                "a host mapping has unknown flags or a source or target that the guest agent \
                 refuses",
            ));
        }
        maps.push(entry(source, target, flags & MAP_READ_ONLY != 0));
    }
    if offset != payload.len() {
        return Err(violation("the host mapping request has trailing bytes"));
    }
    Ok(())
}

/// Returns the error of a `MAPS` payload that ends before its header or an entry does.
fn truncated() -> ProtocolError {
    violation("the host mapping request is truncated")
}

/// Whether the guest agent accepts the paths of a mapping: a `source` that is empty, for the
/// export's root itself, or relative, and an absolute `target`, each of at most
/// [`MAX_MAP_PATH_BYTES`] without NUL characters, whose `/`-separated components are neither
/// empty, `.`, nor `..`. UTF-8 text follows these rules as its bytes, in which `/` and NUL occur
/// only as themselves.
fn acceptable(source: &[u8], target: &[u8]) -> bool {
    let normal = |components: &[u8]| {
        !components.is_empty()
            && components
                .split(|&byte| byte == b'/')
                .all(|component| !matches!(component, b"" | b"." | b".."))
    };
    [source, target]
        .iter()
        .all(|path| path.len() <= MAX_MAP_PATH_BYTES && !path.contains(&0))
        && (source.is_empty() || normal(source))
        && target.strip_prefix(b"/").is_some_and(normal)
}
