use std::cell::RefCell;
use std::fs;

use super::*;
use crate::guest::status::ExitStatus;
use crate::guest::status::tests::command_line;
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
