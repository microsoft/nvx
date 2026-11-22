//! Bind a root's selected MMIO window to the kernel's verified DT platform device.

use std::fs;
use std::io;
use std::path::PathBuf;

use super::super::kernel::Paths;
use super::sorted_entries;

/// Rejects malformed or ambiguous device-tree bindings without an x86 fallback.
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Returns only the platform device whose `of_node` is the matching enabled virtio DT node.
pub(super) fn windows(paths: &Paths, mmio: u64) -> io::Result<Vec<PathBuf>> {
    let tree = paths.sys.join("firmware/devicetree/base");
    for property in ["#address-cells", "#size-cells"] {
        if fs::read(tree.join(property))? != 2u32.to_be_bytes() {
            return Err(invalid(
                "the ARM root DT must use two address and size cells",
            ));
        }
    }
    let mut selected = None;
    for name in sorted_entries(&tree)? {
        let node = tree.join(name);
        if !node.is_dir() {
            continue;
        }
        let reg = match fs::read(node.join("reg")) {
            Ok(reg) => reg,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if reg.len() < 8 || u64::from_be_bytes(reg[..8].try_into().unwrap()) != mmio {
            continue;
        }
        if reg.len() != 16 {
            return Err(invalid(
                "the ARM root DT MMIO region must have one address and size",
            ));
        }
        let size = u64::from_be_bytes(reg[8..].try_into().unwrap());
        if size == 0 || size > 4096 || mmio.checked_add(size).is_none() {
            return Err(invalid("the ARM root DT MMIO window has an invalid size"));
        }
        let compatible = fs::read(node.join("compatible"))?;
        if !compatible.ends_with(&[0])
            || !compatible
                .split(|&byte| byte == 0)
                .any(|value| value == b"virtio,mmio")
        {
            return Err(invalid("the ARM root DT MMIO window is not virtio"));
        }
        match fs::read(node.join("status")) {
            Ok(status) if status == b"okay\0" || status == b"ok\0" => {}
            Ok(_) => return Err(invalid("the ARM root DT MMIO window is disabled")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if selected.replace(node.canonicalize()?).is_some() {
            return Err(invalid("the ARM root DT duplicates an MMIO window"));
        }
    }
    let Some(node) = selected else {
        return Ok(Vec::new());
    };
    let devices = paths.sys.join("bus/platform/devices");
    let mut found = Vec::new();
    for name in sorted_entries(&devices)? {
        let device = devices.join(name);
        let target = match device.join("of_node").canonicalize() {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if target == node {
            found.push(device);
        }
    }
    if found.len() > 1 {
        return Err(invalid(
            "the ARM root DT node has duplicate platform devices",
        ));
    }
    Ok(found)
}

#[cfg(test)]
mod tests;
