//! `nvx-exit [CODE]`: stops the VM with an 8-bit exit code through OpenVMM's platform control
//! interface: control port 0x604 through `/dev/port` on x86-64, or the shutdown register of the
//! restricted platform device on AArch64.
//!
//! CODE defaults to 1, and so does anything other than a decimal number from 0 through 255.
//! Further arguments are ignored, and the program prints nothing. It exits with 1 when it cannot
//! write the byte.

use std::process::ExitCode;

use super::status::{CommandError, run_main};
use super::sys;
#[cfg(target_arch = "x86_64")]
use super::sys::{Mode, OpenFlags, PORT_DEVICE, Port};

/// Runs `nvx-exit` with the process's arguments. It exits with 0 after it writes the byte,
/// unless the VM stops first, or with 1 when it cannot write the byte.
pub fn main() -> ExitCode {
    #[cfg(target_arch = "aarch64")]
    return run_main(|args| {
        let code = exit_code(args.get(1).map(|argument| argument.as_bytes()));
        let access = sys::platform::Access::request().map_err(platform_error)?;
        access
            .write_u8(sys::platform::Register::Shutdown, code)
            .map_err(platform_error)?;
        access.release().map_err(platform_error)
    });
    #[cfg(target_arch = "x86_64")]
    run_main(|args| run(args, PORT_DEVICE))
}

/// Writes the selected exit code through `port`, which stands for `/dev/port`.
///
/// Any failure to open the device or write the byte is an operation failure, which the program
/// does not report. The descriptor closes when the write returns, whereas the C program left it
/// open until the process exited; neither the output nor the status differs.
#[cfg(target_arch = "x86_64")]
fn run(args: &[std::ffi::CString], port: &std::ffi::CStr) -> Result<(), CommandError> {
    let code = exit_code(args.get(1).map(|argument| argument.as_bytes()));
    let written = sys::open(port, OpenFlags::O_WRONLY, Mode::empty())
        .and_then(|descriptor| descriptor.pwrite(&[code], Port::Control.offset()));
    if written == Ok(1) {
        Ok(())
    } else {
        Err(CommandError::Operation)
    }
}

/// Reports an ARM platform operation failure without treating an absent device as shutdown.
#[cfg(target_arch = "aarch64")]
fn platform_error(error: sys::Errno) -> CommandError {
    sys::write_standard_error(format!("nvx-exit: platform access: {error}\n").as_bytes());
    CommandError::Operation
}

/// Returns the VM exit code that `argument` selects.
///
/// A nonempty decimal number from 0 through 255 is accepted, including leading zeros. A missing
/// or empty argument, a sign, whitespace, any other character, or a larger value selects 1, as in
/// the C program. Rust's `u8::from_str` also accepts a leading `+`, so the program keeps its own
/// parser.
fn exit_code(argument: Option<&[u8]>) -> u8 {
    let Some(argument) = argument else {
        return 1;
    };
    let mut value = 0_u32;
    for &byte in argument {
        if !byte.is_ascii_digit() {
            return 1;
        }
        // The function returns as soon as the value exceeds 255, so this cannot overflow.
        value = value * 10 + u32::from(byte - b'0');
        if value > 255 {
            return 1;
        }
    }
    if argument.is_empty() { 1 } else { value as u8 }
}

#[cfg(test)]
mod tests;
