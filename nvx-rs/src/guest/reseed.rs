//! `nvx-reseed`: reseeds the kernel's random number generator from a seed file, such as the one
//! that `nvx-snapshot` reads after a restore, and checks the VM generation ID in the seed.
//!
//! - `nvx-reseed SEED_FILE [PREVIOUS_GENERATION_ID]` adds the 64-byte seed to the entropy pool
//!   with full credit and reseeds the CRNG. With a previous generation ID, it first checks that
//!   the seed's generation ID, its first 16 bytes, differs, and afterwards prints it in
//!   hexadecimal.
//! - `nvx-reseed --generation-only SEED_FILE PREVIOUS_GENERATION_ID` only checks and prints the
//!   generation ID.
//! - `nvx-reseed --sample` prints 32 bytes from `getrandom` in hexadecimal.

use std::ffi::{CStr, CString};
use std::io::{self, Write};
use std::process::ExitCode;

use super::status::{CommandError, ExitStatus};
use super::sys::{self, Errno, Mode, OpenFlags};

/// Number of seed bytes credited to the kernel entropy pool.
const SEED_BYTES: usize = 64;
/// Number of leading seed bytes occupied by the VM generation ID.
const GENERATION_ID_BYTES: usize = 16;
/// Number of random bytes printed by `--sample`.
const RANDOM_SAMPLE_BYTES: usize = 32;
/// Exact public usage diagnostic.
const USAGE: &[u8] =
    b"usage: nvx-reseed --sample | [--generation-only] SEED_FILE [PREVIOUS_GENERATION_ID]\n";

/// Runs `nvx-reseed` with the process's arguments. It exits with 0, 1 when an operation or a
/// check fails, or 2 for a usage error.
pub fn main() -> ExitCode {
    sys::restore_sigpipe();
    ExitStatus::from(run(
        &sys::args(),
        c"/dev/random",
        &mut sys::Stdout::new(),
        &mut io::stderr(),
    ))
    .into()
}

/// Selects sampling, generation checking, or reseeding and performs that operation, with
/// `random` standing for `/dev/random`.
fn run(
    args: &[CString],
    random: &CStr,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    if let [_, flag] = args
        && flag.as_bytes() == b"--sample"
    {
        return print_random_sample(out, err);
    }
    // A first argument of `--generation-only` names the seed file unless three arguments follow
    // the program name.
    let (generation_only, seed_path, previous) = match args {
        [_, seed] => (false, seed, None),
        [_, seed, previous] => (false, seed, Some(previous)),
        [_, flag, seed, previous] if flag.as_bytes() == b"--generation-only" => {
            (true, seed, Some(previous))
        }
        _ => {
            sys::report(err, &[USAGE]);
            return Err(CommandError::Usage);
        }
    };

    let mut seed = [0; SEED_BYTES];
    read_seed(seed_path, &mut seed, err)?;
    let generation_id = &seed[..GENERATION_ID_BYTES];
    if let Some(previous) = previous {
        validate_generation_change(previous.as_bytes(), generation_id, err)?;
    }
    if generation_only {
        return print_hex(generation_id, out, err);
    }

    reseed(random, &seed, err)?;
    if previous.is_some() {
        print_hex(&seed[..GENERATION_ID_BYTES], out, err)?;
    }
    Ok(())
}

/// Adds `seed` to the entropy pool through `random`, which stands for `/dev/random`, and reseeds
/// the CRNG.
///
/// The device closes when this returns, before the caller prints the generation ID, as in the C
/// program, which ignored the result of that close.
fn reseed(random: &CStr, seed: &[u8; SEED_BYTES], err: &mut dyn Write) -> Result<(), CommandError> {
    let descriptor = sys::open(
        random,
        OpenFlags::O_WRONLY | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|errno| {
        let path = random.to_bytes();
        let text = errno.desc().as_bytes();
        sys::report(err, &[b"nvx-reseed: open ", path, b": ", text, b"\n"]);
        CommandError::Operation
    })?;
    sys::reseed_random(&descriptor, seed).map_err(|errno| {
        let text = errno.desc().as_bytes();
        sys::report(err, &[b"nvx-reseed: random ioctl: ", text, b"\n"]);
        CommandError::Operation
    })
}

/// Reads exactly `bytes.len()` bytes from the file at `path`, without retrying after `EINTR`.
fn read_seed(path: &CStr, bytes: &mut [u8], err: &mut dyn Write) -> Result<(), CommandError> {
    let fail = |err: &mut dyn Write, operation: &[u8], reason: &[u8]| {
        let path = path.to_bytes();
        sys::report(
            err,
            &[b"nvx-reseed: ", operation, b" ", path, b": ", reason, b"\n"],
        );
    };
    let descriptor = sys::open(
        path,
        OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|errno| {
        fail(err, b"open", errno.desc().as_bytes());
        CommandError::Operation
    })?;
    let mut offset = 0;
    while offset < bytes.len() {
        let rest = &mut bytes[offset..];
        match descriptor.read(rest) {
            Ok(0) => {
                fail(err, b"read", b"unexpected end of file");
                return Err(CommandError::Operation);
            }
            Ok(count) => offset += count,
            Err(errno) => {
                fail(err, b"read", errno.desc().as_bytes());
                return Err(CommandError::Operation);
            }
        }
    }
    Ok(())
}

/// Checks that `previous` is 32 hexadecimal digits that differ from `generation_id`.
fn validate_generation_change(
    previous: &[u8],
    generation_id: &[u8],
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    if previous.len() != GENERATION_ID_BYTES * 2 {
        sys::report(
            err,
            &[b"nvx-reseed: previous VM generation ID is not 32 hex digits\n"],
        );
        return Err(CommandError::Operation);
    }
    let mut previous_bytes = [0; GENERATION_ID_BYTES];
    for (byte, digits) in previous_bytes.iter_mut().zip(previous.chunks_exact(2)) {
        let (Some(high), Some(low)) = (hex_value(digits[0]), hex_value(digits[1])) else {
            sys::report(
                err,
                &[b"nvx-reseed: previous VM generation ID is not hexadecimal\n"],
            );
            return Err(CommandError::Operation);
        };
        *byte = (high << 4) | low;
    }
    if previous_bytes == generation_id {
        sys::report(err, &[b"nvx-reseed: VM generation ID did not change\n"]);
        return Err(CommandError::Operation);
    }
    Ok(())
}

/// Returns one hexadecimal nibble, accepting the upper- and lowercase C forms.
fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// Prints `bytes` in lowercase hexadecimal and a newline.
fn print_hex(bytes: &[u8], out: &mut dyn Write, err: &mut dyn Write) -> Result<(), CommandError> {
    let mut line: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    line.push('\n');
    if sys::print(out, line.as_bytes()).is_err() {
        sys::report(err, &[b"nvx-reseed: write output failed\n"]);
        return Err(CommandError::Operation);
    }
    Ok(())
}

/// Prints `RANDOM_SAMPLE_BYTES` bytes from the kernel's `getrandom` system call in hexadecimal.
///
/// The `getrandom` crate retries after `EINTR` and short reads, as the C program did. It reads
/// `/dev/urandom` instead only when the kernel lacks the system call or a seccomp policy blocks
/// it.
fn print_random_sample(out: &mut dyn Write, err: &mut dyn Write) -> Result<(), CommandError> {
    let mut sample = [0; RANDOM_SAMPLE_BYTES];
    if let Err(error) = getrandom::fill(&mut sample) {
        let reason = error.raw_os_error().map_or_else(
            || error.to_string(),
            |code| Errno::from_raw(code).desc().to_owned(),
        );
        sys::report(err, &[b"nvx-reseed: getrandom: ", reason.as_bytes(), b"\n"]);
        return Err(CommandError::Operation);
    }
    print_hex(&sample, out, err)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::*;
    use crate::guest::sys::tests::Failing;

    /// A seed whose bytes count up from 0, so its generation ID is 000102...0f.
    const GENERATION_ID: &str = "000102030405060708090a0b0c0d0e0f";

    struct Fixture {
        directory: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let seed: Vec<u8> = (0..SEED_BYTES as u8).collect();
            fs::write(directory.path().join("seed"), seed).unwrap();
            fs::write(directory.path().join("random"), b"").unwrap();
            Self { directory }
        }

        fn path(&self, name: &str) -> String {
            self.directory
                .path()
                .join(name)
                .to_str()
                .unwrap()
                .to_owned()
        }

        fn run_with(&self, arguments: &[&str], out: &mut dyn Write) -> (u8, Vec<u8>) {
            let mut args = vec![CString::new("nvx-reseed").unwrap()];
            args.extend(
                arguments
                    .iter()
                    .map(|argument| CString::new(*argument).unwrap()),
            );
            let random = CString::new(self.path("random")).unwrap();
            let mut err = Vec::new();
            let status = ExitStatus::from(run(&args, &random, out, &mut err));
            (status as u8, err)
        }

        fn run(&self, arguments: &[&str]) -> (u8, Vec<u8>, Vec<u8>) {
            let mut out = Vec::new();
            let (status, err) = self.run_with(arguments, &mut out);
            (status, out, err)
        }
    }

    fn message(operation: &str, path: &Path, reason: &[u8]) -> Vec<u8> {
        let mut message = format!("nvx-reseed: {operation} ").into_bytes();
        message.extend_from_slice(path.as_os_str().as_bytes());
        message.extend_from_slice(b": ");
        message.extend_from_slice(reason);
        message.push(b'\n');
        message
    }

    #[test]
    fn usage_takes_one_to_three_arguments() {
        let fixture = Fixture::new();
        for arguments in [
            &[][..],
            &["a", "b", "c"],
            &["--generation-only", "a", "b", "c"],
            &["--sample", "a", "b"],
        ] {
            assert_eq!(
                fixture.run(arguments),
                (2, Vec::new(), USAGE.to_vec()),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn options_name_the_seed_file_with_fewer_arguments() {
        let fixture = Fixture::new();
        for (arguments, path) in [
            (
                &["--generation-only", GENERATION_ID][..],
                "--generation-only",
            ),
            (&["--sample", GENERATION_ID], "--sample"),
        ] {
            let reason = Errno::ENOENT.desc().as_bytes();
            assert_eq!(
                fixture.run(arguments),
                (1, Vec::new(), message("open", Path::new(path), reason))
            );
        }
    }

    #[test]
    fn prints_a_random_sample() {
        let fixture = Fixture::new();
        let (status, first, err) = fixture.run(&["--sample"]);
        assert_eq!((status, err.as_slice()), (0, &b""[..]));
        assert_eq!(first.len(), RANDOM_SAMPLE_BYTES * 2 + 1);
        assert!(
            first[..first.len() - 1]
                .iter()
                .all(|digit| matches!(digit, b'0'..=b'9' | b'a'..=b'f'))
        );
        assert_eq!(first.last(), Some(&b'\n'));
        assert_ne!(fixture.run(&["--sample"]).1, first);
    }

    #[test]
    fn checks_and_prints_the_generation_id() {
        let fixture = Fixture::new();
        let seed = fixture.path("seed");
        let expected = format!("{GENERATION_ID}\n").into_bytes();
        for previous in [
            "ffffffffffffffffffffffffffffffff",
            "000102030405060708090A0B0C0D0E0E",
        ] {
            assert_eq!(
                fixture.run(&["--generation-only", &seed, previous]),
                (0, expected.clone(), Vec::new())
            );
        }
        let cases: &[(&str, &[u8])] = &[
            (
                "0001020304050607",
                b"nvx-reseed: previous VM generation ID is not 32 hex digits\n",
            ),
            (
                "000102030405060708090a0b0c0d0e0f0",
                b"nvx-reseed: previous VM generation ID is not 32 hex digits\n",
            ),
            (
                "000102030405060708090a0b0c0d0e0g",
                b"nvx-reseed: previous VM generation ID is not hexadecimal\n",
            ),
            (
                "000102030405060708090A0B0C0D0E0F",
                b"nvx-reseed: VM generation ID did not change\n",
            ),
        ];
        for &(previous, error) in cases {
            for arguments in [
                &["--generation-only", &seed, previous][..],
                &[&seed, previous],
            ] {
                assert_eq!(
                    fixture.run(arguments),
                    (1, Vec::new(), error.to_vec()),
                    "{arguments:?}"
                );
            }
        }
        let mut out = Failing(Errno::EIO);
        let arguments = [
            "--generation-only",
            &seed,
            "ffffffffffffffffffffffffffffffff",
        ];
        assert_eq!(
            fixture.run_with(&arguments, &mut out),
            (1, b"nvx-reseed: write output failed\n".to_vec())
        );
    }

    #[test]
    fn reports_seeds_that_cannot_be_read() {
        let fixture = Fixture::new();
        let missing = fixture.directory.path().join("missing");
        assert_eq!(
            fixture.run(&[missing.to_str().unwrap()]),
            (
                1,
                Vec::new(),
                message("open", &missing, Errno::ENOENT.desc().as_bytes())
            )
        );
        let short = fixture.directory.path().join("short");
        fs::write(&short, [0; SEED_BYTES - 1]).unwrap();
        assert_eq!(
            fixture.run(&[short.to_str().unwrap()]),
            (
                1,
                Vec::new(),
                message("read", &short, b"unexpected end of file")
            )
        );
        let directory = fixture.directory.path();
        assert_eq!(
            fixture.run(&[directory.to_str().unwrap()]),
            (
                1,
                Vec::new(),
                message("read", directory, Errno::EISDIR.desc().as_bytes())
            )
        );
    }

    #[test]
    fn reports_a_failed_reseed() {
        let fixture = Fixture::new();
        let seed = fixture.path("seed");
        // A regular file stands for /dev/random, and has no random ioctls.
        let mut expected = b"nvx-reseed: random ioctl: ".to_vec();
        expected.extend_from_slice(Errno::ENOTTY.desc().as_bytes());
        expected.push(b'\n');
        assert_eq!(fixture.run(&[&seed]), (1, Vec::new(), expected));

        fs::remove_file(fixture.path("random")).unwrap();
        let random = fixture.directory.path().join("random");
        assert_eq!(
            fixture.run(&[&seed, "ffffffffffffffffffffffffffffffff"]),
            (
                1,
                Vec::new(),
                message("open", &random, Errno::ENOENT.desc().as_bytes())
            )
        );
    }
}
