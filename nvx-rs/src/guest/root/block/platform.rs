//! ARM fixed virtio-MMIO discovery through real OF properties and platform-driver identity.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::Paths;

/// First fixed virtio-MMIO window in the development machine binding.
const BASE: u64 = 0xd000_0000;
/// Size and stride of each fixed transport.
const WINDOW_SIZE: u64 = 0x1000;
/// Slots that have an assigned device role; 4, 5, and 8 remain reserved.
const SLOTS: [u64; 6] = [0, 1, 2, 3, 6, 7];

/// Reports a malformed or ambiguous kernel-published fixed-device identity.
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Reads the explicitly declared cell width of the fixed machine's root bus.
fn cell_count(root: &Path, property: &str) -> io::Result<usize> {
    let bytes: [u8; 4] = fs::read(root.join(property))?
        .try_into()
        .map_err(|_| invalid("OF cell count must be exactly one big-endian word"))?;
    match u32::from_be_bytes(bytes) {
        count @ (1 | 2) => Ok(count as usize),
        _ => Err(invalid(
            "fixed OF resources need one or two address/size cells",
        )),
    }
}

/// Decodes the one root-relative register window, using the parent's big-endian cell widths.
fn resource(bytes: &[u8], address_cells: usize, size_cells: usize) -> io::Result<(u64, u64)> {
    if !matches!(address_cells, 1 | 2)
        || !matches!(size_cells, 1 | 2)
        || bytes.len() != (address_cells + size_cells) * 4
    {
        return Err(invalid(
            "fixed OF reg must contain exactly one complete address/size tuple",
        ));
    }
    let number = |cells: &[u8]| {
        cells
            .iter()
            .fold(0_u64, |value, &byte| (value << 8) | u64::from(byte))
    };
    let split = address_cells * 4;
    let (start, size) = (number(&bytes[..split]), number(&bytes[split..]));
    let end = start
        .checked_add(size)
        .filter(|_| size != 0)
        .ok_or_else(|| invalid("OF resource has zero size or overflowing bounds"))?;
    Ok((start, end))
}

/// Identifies the OF transport even before its platform driver has bound.
fn compatible(device: &Path) -> io::Result<bool> {
    let bytes = match fs::read(device.join("of_node/compatible")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let found = bytes
        .split(|&byte| byte == 0)
        .any(|item| item == b"virtio,mmio");
    if found && !bytes.ends_with(&[0]) {
        return Err(invalid("virtio,mmio OF compatible is not NUL terminated"));
    }
    Ok(found)
}

/// Verifies the driver and distinguishes an unbound transport that is still probing.
fn virtio_mmio(device: &Path) -> io::Result<bool> {
    match fs::read_link(device.join("driver")) {
        Ok(driver) if driver.file_name().is_some_and(|name| name == "virtio-mmio") => Ok(true),
        Ok(_) => Err(invalid(
            "fixed virtio-MMIO resource is bound to another driver",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Finds exactly one fixed transport, without node-name or enumeration-order assumptions.
pub(super) fn window_directories(paths: &Paths, mmio: u64) -> io::Result<Vec<PathBuf>> {
    if !SLOTS.iter().any(|&slot| BASE + slot * WINDOW_SIZE == mmio) {
        return Err(invalid("MMIO address has no assigned fixed-device slot"));
    }
    let end = mmio
        .checked_add(WINDOW_SIZE)
        .ok_or_else(|| invalid("fixed MMIO window overflows"))?;
    let mut found = Vec::new();
    let root = fs::canonicalize(paths.sys.join("firmware/devicetree/base"))?;
    let address_cells = cell_count(&root, "#address-cells")?;
    let size_cells = cell_count(&root, "#size-cells")?;
    for entry in fs::read_dir(paths.sys.join("bus/platform/devices"))? {
        let device = entry?.path();
        let node = match fs::canonicalize(device.join("of_node")) {
            Ok(node) => node,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::read(device.join("modalias")) {
                    Ok(alias)
                        if alias.strip_suffix(b"\n").unwrap_or(&alias)
                            == b"platform:virtio-mmio" =>
                    {
                        return Err(invalid("ARM virtio-MMIO transport has no OF identity"));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        let identity = compatible(&device)?;
        let registers = match fs::read(node.join("reg")) {
            Ok(registers) => registers,
            Err(error) if error.kind() == io::ErrorKind::NotFound && !identity => continue,
            Err(error) => return Err(error),
        };
        // The fixed binding emits root children, so no bus-range translation is implicit.
        if node.parent() != Some(root.as_path()) {
            return Err(invalid(
                "fixed MMIO discovery does not support non-root OF register translation",
            ));
        }
        let stride = (address_cells + size_cells) * 4;
        if registers.is_empty() || !registers.len().is_multiple_of(stride) {
            return Err(invalid("OF reg has incomplete address/size tuples"));
        }
        let mut overlaps = Vec::new();
        for tuple in registers.chunks_exact(stride) {
            let (start, last) = resource(tuple, address_cells, size_cells)?;
            if start < end && last > mmio {
                overlaps.push((start, last));
            }
        }
        if overlaps.is_empty() {
            continue;
        }
        if registers.len() != stride || overlaps != [(mmio, end)] {
            return Err(invalid(
                "fixed virtio-MMIO OF resource has incorrect bounds",
            ));
        }
        if !identity {
            return Err(invalid(
                "fixed MMIO role is occupied by a non-virtio OF device",
            ));
        }
        virtio_mmio(&device)?;
        found.push(device);
    }
    if found.len() > 1 {
        return Err(invalid(
            "multiple platform devices claim the fixed MMIO resource",
        ));
    }
    Ok(found)
}

/// Resolves a unique virtio/block child, or waits for its ordinary driver probe.
pub(super) fn block_device(window: &Path) -> io::Result<Option<String>> {
    if !virtio_mmio(window)? {
        return Ok(None);
    }
    let virtio: Vec<_> = super::sorted_entries(window)?
        .into_iter()
        .filter(|name| name.starts_with("virtio"))
        .collect();
    if virtio.len() > 1 {
        return Err(invalid("fixed transport has multiple virtio children"));
    }
    let Some(child) = virtio.first() else {
        return Ok(None);
    };
    let blocks = super::sorted_entries(&window.join(child).join("block"))?;
    if blocks.len() > 1 {
        return Err(invalid("fixed transport has multiple block children"));
    }
    Ok(blocks.into_iter().next())
}

#[cfg(test)]
mod tests;
