//! The `nvx` command-line interface; see `nvx::host::cli`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let status = nvx::host::cli::main();
    // Returning the status from `main`, rather than exiting with it, lets the C runtime run its exit
    // handlers, which `std::process::exit` skips on Windows: among them is the one with which a
    // coverage build writes its profile. `ExitCode` holds the statuses from 0 to 255; the others,
    // such as a Windows status of OpenVMM's or a negated signal on Unix, still exit.
    match u8::try_from(status) {
        Ok(status) => ExitCode::from(status),
        Err(_) => std::process::exit(status),
    }
}
