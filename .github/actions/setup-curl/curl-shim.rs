use std::env;
use std::process::{self, Command};

const RETRY_ARGUMENTS: &[&str] = &[
    "--retry",
    "5",
    "--retry-all-errors",
    "--retry-delay",
    "2",
    "--retry-max-time",
    "90",
];

fn main() {
    let system_curl = env::var_os("NVX_SYSTEM_CURL").unwrap_or_else(|| {
        eprintln!("NVX_SYSTEM_CURL is not set");
        process::exit(1);
    });
    let status = Command::new(system_curl)
        .args(RETRY_ARGUMENTS)
        .args(env::args_os().skip(1))
        .status()
        .unwrap_or_else(|error| {
            eprintln!("failed to start system curl: {error}");
            process::exit(1);
        });
    process::exit(status.code().unwrap_or(1));
}
