//! `nvx-mmio-write ADDRESS VALUE`: stores a 32-bit value at a physical address through
//! `/dev/mem`, as `nvx-virtio-restore-probe` does to kick a virtio queue.

use std::ffi::{CStr, CString};
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::process::ExitCode;

use super::parse::parse_u64;
use super::status::{CommandError, run_main};
use super::sys::{self, Errno, Mode, OpenFlags};

/// Runs `nvx-mmio-write` with the process's arguments. It exits with 0, 1 when an operation
/// fails, or 2 for a usage error, and prints nothing on success.
pub fn main() -> ExitCode {
    run_main(|args| run(args, c"/dev/mem", &mut io::stderr()))
}

/// Validates the command, maps one physical page of `memory`, which stands for `/dev/mem`,
/// stores the value, and releases its resources.
fn run(args: &[CString], memory: &CStr, err: &mut dyn Write) -> Result<(), CommandError> {
    let parsed = match args {
        [_, address, value] => parse_u64(address.as_bytes()).zip(parse_u64(value.as_bytes())),
        _ => None,
    };
    let Some((address, value)) = parsed.filter(|&(_, value)| value <= u64::from(u32::MAX)) else {
        sys::report(err, &[b"usage: nvx-mmio-write ADDRESS VALUE\n"]);
        return Err(CommandError::Usage);
    };
    let value = value as u32;

    let Some(page_size) = sys::page_size() else {
        sys::report(err, &[b"invalid page size\n"]);
        return Err(CommandError::Operation);
    };
    let page_size_u64 = page_size as u64;
    let page_base = address & !(page_size_u64 - 1);
    let page_offset = address - page_base;
    if page_offset > page_size_u64.wrapping_sub(4) {
        sys::report(err, &[b"MMIO write crosses a page boundary\n"]);
        return Err(CommandError::Operation);
    }

    let descriptor = sys::open(memory, OpenFlags::O_RDWR | OpenFlags::O_SYNC, Mode::empty())
        .map_err(|error| perror(err, b"open /dev/mem", error))?;
    let length = NonZeroUsize::new(page_size).expect("a valid page size is nonzero");
    let mut mapping = sys::MemoryMapping::shared_read_write(&descriptor, length, page_base)
        .map_err(|error| perror(err, b"mmap /dev/mem", error))?;
    if !mapping.write_u32(page_offset as usize, value) {
        // The boundary validation above proves this unreachable, but preserve a typed failure if
        // the mapping abstraction and validation ever diverge.
        sys::report(err, &[b"MMIO write crosses a page boundary\n"]);
        return Err(CommandError::Operation);
    }
    mapping
        .unmap()
        .map_err(|error| perror(err, b"munmap /dev/mem", error))?;
    descriptor
        .close()
        .map_err(|error| perror(err, b"close /dev/mem", error))?;
    Ok(())
}

/// Reports `errno` as C's `perror(prefix)` does, with `nix`'s description, and returns the
/// operation failure.
fn perror(err: &mut dyn Write, prefix: &[u8], errno: Errno) -> CommandError {
    sys::report(err, &[prefix, b": ", errno.desc().as_bytes(), b"\n"]);
    CommandError::Operation
}

#[cfg(test)]
mod tests;
