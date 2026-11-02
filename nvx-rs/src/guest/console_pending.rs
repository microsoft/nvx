//! `nvx-console-pending DEVICE`: prints the number of bytes waiting to be read from a device, such
//! as a console, which the console-snapshot test reads from `/dev/hvc1`.

use std::ffi::CString;
use std::io::{self, Write};
use std::process::ExitCode;

use super::status::{CommandError, run_main};
use super::sys::{self, Errno, Mode, OpenFlags};

/// Runs `nvx-console-pending` with the process's arguments. It exits with 0, 1 when an operation
/// fails, or 2 for a usage error.
pub fn main() -> ExitCode {
    run_main(|args| run(args, &mut sys::Stdout::new(), &mut io::stderr()))
}

/// Opens the requested device, queries and closes it, and prints its pending byte count.
fn run(args: &[CString], out: &mut dyn Write, err: &mut dyn Write) -> Result<(), CommandError> {
    let [_, device] = args else {
        sys::report(err, &[b"usage: nvx-console-pending DEVICE\n"]);
        return Err(CommandError::Usage);
    };
    let fail = |err: &mut dyn Write, operation: &[u8], errno: Errno| {
        let device = device.as_bytes();
        sys::report(
            err,
            &[
                b"nvx-console-pending: ",
                operation,
                b" ",
                device,
                b": ",
                errno.desc().as_bytes(),
                b"\n",
            ],
        );
        CommandError::Operation
    };

    let flags =
        OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC | OpenFlags::O_NOCTTY | OpenFlags::O_NONBLOCK;
    let descriptor =
        sys::open(device, flags, Mode::empty()).map_err(|error| fail(err, b"open", error))?;
    let pending = sys::console_pending(&descriptor).map_err(|error| fail(err, b"ioctl", error))?;
    descriptor
        .close()
        .map_err(|error| fail(err, b"close", error))?;
    if let Err(errno) = sys::print(out, format!("{pending}\n").as_bytes()) {
        let text = errno.desc().as_bytes();
        sys::report(err, &[b"nvx-console-pending: write: ", text, b"\n"]);
        return Err(CommandError::Operation);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
