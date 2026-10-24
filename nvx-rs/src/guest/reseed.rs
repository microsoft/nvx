//! Reseeding the kernel's random number generator with a restore's fresh entropy, for any guest
//! init or agent, and `nvx-reseed`, the program that does it from a seed file.
//!
//! A restore of a snapshot hands each clone 64 bytes of fresh entropy, which start with the
//! clone's new VM generation ID: a [`Seed`]. The time ABI's capture returns it in its in-memory
//! mode, or writes it to a seed file in its file mode, from which [`Seed::read`] reads it.
//! [`reseed`] adds a seed to the kernel's entropy pool with full credit and reseeds its CRNG, so
//! that the clones of one snapshot draw different random numbers. Neither call prints anything
//! or starts a process.
//!
//! A seed keeps its bytes in one allocation, which moving the seed leaves in place, and
//! overwrites them with zeros, with volatile writes, when it is dropped. It can't be cloned, and
//! formats none of its bytes: only its generation ID leaves it, and [`reseed`] hands the rest to
//! the kernel. A caller of the file mode reads and checks the seed, then drops it once it
//! reseeded:
//!
//! ```no_run
//! use nvx::guest::GenerationId;
//! use nvx::guest::reseed::{self, Seed};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let previous = GenerationId::from_bytes([0; GenerationId::LENGTH]);
//! let seed = Seed::read("/run/nvx/restore-entropy")?;
//! if seed.generation_id() == previous {
//!     return Err("the VM generation ID did not change".into());
//! }
//! reseed::reseed(&seed)?;
//! # Ok(())
//! # }
//! ```
//!
//! `nvx-reseed` runs these calls, such as for `nvx-snapshot` after a restore:
//!
//! - `nvx-reseed SEED_FILE [PREVIOUS_GENERATION_ID]` adds the 64-byte seed to the entropy pool
//!   with full credit and reseeds the CRNG. With a previous generation ID, it first checks that
//!   the seed's generation ID, its first 16 bytes, differs, and afterwards prints it in
//!   hexadecimal.
//! - `nvx-reseed --generation-only SEED_FILE PREVIOUS_GENERATION_ID` only checks and prints the
//!   generation ID.
//! - `nvx-reseed --sample` prints 32 bytes from `getrandom` in hexadecimal.

use std::ffi::{CStr, CString, OsStr};
use std::fmt;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::GenerationId;
use super::status::{CommandError, run_main};
use super::sys::{self, Errno, Mode, OpenFlags};

/// The kernel's random device, through which [`reseed`] adds a seed.
const RANDOM_DEVICE: &CStr = c"/dev/random";
/// Number of random bytes printed by `--sample`.
const RANDOM_SAMPLE_BYTES: usize = 32;
/// Exact public usage diagnostic.
const USAGE: &[u8] =
    b"usage: nvx-reseed --sample | [--generation-only] SEED_FILE [PREVIOUS_GENERATION_ID]\n";

/// A restore's fresh entropy: 64 bytes that start with the restored VM's new generation ID.
///
/// The seed keeps its bytes in one heap allocation for its whole life, so that no move of the
/// seed copies them, and overwrites them with zeros, with volatile writes, when it is dropped.
/// It is neither `Clone` nor `Copy`, and formats none of its bytes: its `Debug` form is
/// `Seed { .. }`. Only its generation ID leaves it, through
/// [`generation_id`](Self::generation_id), and [`reseed`] hands the rest to the kernel.
pub struct Seed(Box<[u8; Seed::LENGTH]>);

impl Seed {
    /// The seed's length in bytes.
    pub const LENGTH: usize = 64;

    /// Returns the seed whose bytes are `bytes`. The call zeroes its own copy of `bytes`; the
    /// caller zeroes any copy that it keeps.
    pub fn from_bytes(mut bytes: [u8; Self::LENGTH]) -> Self {
        let seed = Self::copy_from(&bytes);
        sys::zeroize(&mut bytes);
        seed
    }

    /// Returns the seed whose bytes are a copy of `bytes`, which go straight into its
    /// allocation.
    pub(crate) fn copy_from(bytes: &[u8; Self::LENGTH]) -> Self {
        let mut seed = Self::zeroed();
        seed.0.copy_from_slice(bytes);
        seed
    }

    /// Returns a seed of zeros, whose allocation a read then fills.
    fn zeroed() -> Self {
        Self(Box::new([0; Self::LENGTH]))
    }

    /// Reads the seed from the file at `path`, which must hold at least [`Seed::LENGTH`] bytes,
    /// of which it reads the first, straight into the seed's allocation. A read that a signal
    /// interrupts fails, with an error of kind [`io::ErrorKind::Interrupted`], and a file that
    /// is too short fails with an error of kind [`io::ErrorKind::UnexpectedEof`]; either way, the
    /// bytes that were read are zeroed.
    pub fn read(path: impl AsRef<Path>) -> Result<Self, ReseedError> {
        let path = path.as_ref();
        let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
            return Err(ReseedError::OpenSeed {
                path: path.to_owned(),
                source: io::ErrorKind::InvalidInput.into(),
            });
        };
        read_seed(&path)
    }

    /// Returns the VM generation ID, the seed's first 16 bytes.
    pub fn generation_id(&self) -> GenerationId {
        let mut id = [0; GenerationId::LENGTH];
        id.copy_from_slice(&self.0[..GenerationId::LENGTH]);
        GenerationId::from_bytes(id)
    }

    /// Returns the seed's bytes, which the crate reads only to hand them on: the time ABI's
    /// capture, in its file mode, writes a restore's to the entropy file.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn bytes(&self) -> &[u8; Self::LENGTH] {
        &self.0
    }
}

impl Drop for Seed {
    /// Overwrites the seed's bytes with zeros, with volatile writes, before its allocation is
    /// freed.
    fn drop(&mut self) {
        sys::zeroize(self.0.as_mut_slice());
        #[cfg(test)]
        tests::observe_drop(self);
    }
}

impl fmt::Debug for Seed {
    /// Writes `Seed { .. }`, and none of the seed's bytes.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Seed").finish_non_exhaustive()
    }
}

/// Adds `seed` to the kernel's entropy pool with full credit, and reseeds the kernel's CRNG,
/// which needs `CAP_SYS_ADMIN`.
pub fn reseed(seed: &Seed) -> Result<(), ReseedError> {
    reseed_through(RANDOM_DEVICE, seed)
}

/// Why a seed could not be read, or the kernel's random number generator not reseeded.
///
/// Its `Display` form is the diagnostic that `nvx-reseed` prints, such as
/// `read /run/nvx/restore-entropy: unexpected end of file`.
#[derive(Debug)]
#[non_exhaustive]
pub enum ReseedError {
    /// The seed file could not be opened.
    OpenSeed {
        /// The file's path.
        path: PathBuf,
        /// Why the file could not be opened.
        source: io::Error,
    },
    /// The seed file could not be read, or was too short.
    ReadSeed {
        /// The file's path.
        path: PathBuf,
        /// Why the file could not be read.
        source: io::Error,
    },
    /// `/dev/random` could not be opened.
    OpenRandom(io::Error),
    /// The kernel refused the seed or the reseed.
    Reseed(io::Error),
}

impl fmt::Display for ReseedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let describe = sys::describe;
        match self {
            Self::OpenSeed { path, source } => {
                write!(formatter, "open {}: {}", path.display(), describe(source))
            }
            Self::ReadSeed { path, source } => {
                write!(formatter, "read {}: {}", path.display(), describe(source))
            }
            Self::OpenRandom(source) => write!(
                formatter,
                "open {}: {}",
                RANDOM_DEVICE.to_string_lossy(),
                describe(source)
            ),
            Self::Reseed(source) => write!(formatter, "random ioctl: {}", describe(source)),
        }
    }
}

impl std::error::Error for ReseedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::OpenSeed { source, .. }
            | Self::ReadSeed { source, .. }
            | Self::OpenRandom(source)
            | Self::Reseed(source) => Some(source),
        }
    }
}

/// Reads the first [`Seed::LENGTH`] bytes of the file at `path` into a seed's allocation,
/// without retrying after `EINTR`. A failed read drops the seed, which zeroes what it read.
fn read_seed(path: &CStr) -> Result<Seed, ReseedError> {
    let owned = || PathBuf::from(OsStr::from_bytes(path.to_bytes()));
    let descriptor = sys::open(
        path,
        OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|errno| ReseedError::OpenSeed {
        path: owned(),
        source: errno.into(),
    })?;
    let mut seed = Seed::zeroed();
    let mut offset = 0;
    while offset < Seed::LENGTH {
        let source = match descriptor.read(&mut seed.0[offset..]) {
            Ok(0) => io::ErrorKind::UnexpectedEof.into(),
            Ok(count) => {
                offset += count;
                continue;
            }
            Err(errno) => errno.into(),
        };
        return Err(ReseedError::ReadSeed {
            path: owned(),
            source,
        });
    }
    Ok(seed)
}

/// Adds `seed` to the entropy pool through `random`, which stands for `/dev/random`, and reseeds
/// the CRNG.
///
/// The device closes when this returns, before `nvx-reseed` prints the generation ID, as in the
/// C program, which ignored the result of that close.
fn reseed_through(random: &CStr, seed: &Seed) -> Result<(), ReseedError> {
    let descriptor = sys::open(
        random,
        OpenFlags::O_WRONLY | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|errno| ReseedError::OpenRandom(errno.into()))?;
    sys::reseed_random(&descriptor, &seed.0).map_err(|errno| ReseedError::Reseed(errno.into()))
}

/// Runs `nvx-reseed` with the process's arguments. It exits with 0, 1 when an operation or a
/// check fails, or 2 for a usage error.
pub fn main() -> ExitCode {
    run_main(|args| {
        run(
            args,
            RANDOM_DEVICE,
            &mut sys::Stdout::new(),
            &mut io::stderr(),
        )
    })
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

    let seed = read_seed(seed_path).map_err(|error| reseed_failure(err, random, error))?;
    let generation_id = seed.generation_id().to_bytes();
    if let Some(previous) = previous {
        validate_generation_change(previous.as_bytes(), &generation_id, err)?;
    }
    if generation_only {
        return print_hex(&generation_id, out, err);
    }

    reseed_through(random, &seed).map_err(|error| reseed_failure(err, random, error))?;
    if previous.is_some() {
        print_hex(&generation_id, out, err)?;
    }
    Ok(())
}

/// Reports why the seed could not be read or added, with `random` standing for `/dev/random`,
/// such as `nvx-reseed: random ioctl: Operation not permitted`, and returns the operation
/// failure.
fn reseed_failure(err: &mut dyn Write, random: &CStr, error: ReseedError) -> CommandError {
    let (operation, path, source): (&[u8], &[u8], _) = match &error {
        ReseedError::OpenSeed { path, source } => (b"open", path.as_os_str().as_bytes(), source),
        ReseedError::ReadSeed { path, source } => (b"read", path.as_os_str().as_bytes(), source),
        ReseedError::OpenRandom(source) => (b"open", random.to_bytes(), source),
        ReseedError::Reseed(source) => {
            let reason = sys::describe(source);
            sys::report(
                err,
                &[b"nvx-reseed: random ioctl: ", reason.as_bytes(), b"\n"],
            );
            return CommandError::Operation;
        }
    };
    let reason = sys::describe(source);
    sys::report(
        err,
        &[
            b"nvx-reseed: ",
            operation,
            b" ",
            path,
            b": ",
            reason.as_bytes(),
            b"\n",
        ],
    );
    CommandError::Operation
}

/// Checks that `previous` is 32 hexadecimal digits that differ from `generation_id`.
fn validate_generation_change(
    previous: &[u8],
    generation_id: &[u8; GenerationId::LENGTH],
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    if previous.len() != GenerationId::LENGTH * 2 {
        sys::report(
            err,
            &[b"nvx-reseed: previous VM generation ID is not 32 hex digits\n"],
        );
        return Err(CommandError::Operation);
    }
    let mut previous_bytes = [0; GenerationId::LENGTH];
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
    if previous_bytes == *generation_id {
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
pub(crate) mod tests {
    use std::cell::RefCell;
    use std::fs;

    use super::*;
    use crate::guest::status::{ExitStatus, command_line};
    use crate::guest::sys::tests::Failing;

    /// A seed whose bytes count up from 0, so its generation ID is 000102...0f.
    const GENERATION_ID: &str = "000102030405060708090a0b0c0d0e0f";

    thread_local! {
        /// The bytes of each seed that the test's thread dropped, as its drop left them just
        /// before its allocation was freed.
        static DROPPED: RefCell<Vec<[u8; Seed::LENGTH]>> = const { RefCell::new(Vec::new()) };
    }

    /// Records the bytes of `seed`, which its drop has overwritten, while its allocation still
    /// holds them.
    pub(super) fn observe_drop(seed: &Seed) {
        DROPPED.with_borrow_mut(|dropped| dropped.push(*seed.0));
    }

    /// Returns the bytes of each seed that the test's thread dropped since the last call, in
    /// order, as their drops left them.
    pub(crate) fn take_dropped() -> Vec<[u8; Seed::LENGTH]> {
        DROPPED.take()
    }

    struct Fixture {
        directory: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let seed: Vec<u8> = (0..Seed::LENGTH as u8).collect();
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
            let args = command_line("nvx-reseed", arguments);
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
        fs::write(&short, [0; Seed::LENGTH - 1]).unwrap();
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

    #[test]
    fn a_seed_reads_the_first_bytes_of_its_file_and_formats_none_of_them() {
        let fixture = Fixture::new();
        let long = fixture.directory.path().join("long");
        let bytes: Vec<u8> = (0..=Seed::LENGTH as u8).map(|byte| byte ^ 0xa5).collect();
        fs::write(&long, &bytes).unwrap();
        let seed = Seed::read(&long).unwrap();
        assert_eq!(seed.0[..], bytes[..Seed::LENGTH]);
        let id = seed.generation_id();
        assert_eq!(id.to_bytes()[..], bytes[..GenerationId::LENGTH]);
        assert_eq!(format!("{seed:?}"), "Seed { .. }");
        assert_eq!(format!("{seed:#?}"), "Seed { .. }");
        let from_bytes = Seed::from_bytes(*seed.0);
        assert_eq!(
            Seed::read(fixture.path("seed")).unwrap().generation_id(),
            GenerationId::from_bytes(std::array::from_fn(|index| index as u8))
        );
        assert_eq!(from_bytes.0, seed.0);
        assert_eq!(from_bytes.generation_id(), id);
    }

    #[test]
    fn a_seed_moves_without_its_bytes_and_zeroes_them_when_dropped() {
        let fixture = Fixture::new();
        let seed = Seed::from_bytes([0xa5; Seed::LENGTH]);
        let bytes = seed.0.as_ptr();
        // A move, such as into a restore and out of it again, moves only the allocation's address.
        let seed = std::hint::black_box(seed);
        assert_eq!(seed.0.as_ptr(), bytes);
        let read = Seed::read(fixture.path("seed")).unwrap();
        DROPPED.take();
        drop((seed, read));
        assert_eq!(DROPPED.take(), [[0; Seed::LENGTH]; 2]);

        // A read that fails drops the seed that it was filling.
        let short = fixture.directory.path().join("short");
        fs::write(&short, [0xa5; Seed::LENGTH - 1]).unwrap();
        Seed::read(&short).unwrap_err();
        assert_eq!(DROPPED.take(), [[0; Seed::LENGTH]]);
    }

    #[test]
    fn seed_errors_keep_the_path_and_the_cause() {
        let fixture = Fixture::new();
        let short = fixture.directory.path().join("short");
        fs::write(&short, [0; Seed::LENGTH - 1]).unwrap();
        let error = Seed::read(&short).unwrap_err();
        let ReseedError::ReadSeed { path, source } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(
            (path, source.kind()),
            (&short, io::ErrorKind::UnexpectedEof)
        );
        assert_eq!(
            error.to_string(),
            format!("read {}: unexpected end of file", short.display())
        );

        let missing = fixture.directory.path().join("missing");
        let error = Seed::read(&missing).unwrap_err();
        let ReseedError::OpenSeed { path, source } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(
            (path, source.raw_os_error()),
            (&missing, Some(Errno::ENOENT as i32))
        );
        assert_eq!(
            error.to_string(),
            format!("open {}: {}", missing.display(), Errno::ENOENT.desc())
        );
        assert!(std::error::Error::source(&error).is_some());

        let error = Seed::read(OsStr::from_bytes(b"seed\0file")).unwrap_err();
        let ReseedError::OpenSeed { source, .. } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(source.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn reseed_errors_name_the_failing_step() {
        let fixture = Fixture::new();
        let seed = Seed::read(fixture.path("seed")).unwrap();
        let random = CString::new(fixture.path("random")).unwrap();
        let error = reseed_through(&random, &seed).unwrap_err();
        assert!(
            matches!(&error, ReseedError::Reseed(source) if source.raw_os_error() == Some(Errno::ENOTTY as i32)),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!("random ioctl: {}", Errno::ENOTTY.desc())
        );

        fs::remove_file(fixture.path("random")).unwrap();
        let error = reseed_through(&random, &seed).unwrap_err();
        assert!(
            matches!(&error, ReseedError::OpenRandom(source) if source.raw_os_error() == Some(Errno::ENOENT as i32)),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!("open /dev/random: {}", Errno::ENOENT.desc())
        );
    }
}
