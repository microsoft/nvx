//! The host program stages the images of a layered root in a sandbox's store: it verifies the
//! image, binds the scratch image, and records both bindings, which a launch takes for its own
//! images. The tests need no hypervisor.
#![cfg(feature = "host")]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use nvx::ErrorKind;
use nvx::host::sandbox::distro::DistroFormat;
use nvx::protocol::root::ImageFormat;
use nvx::uuid::Uuid;
use runtime_adapter::layered::{Bindings, LayeredLaunch};
use runtime_adapter::staged::{STAGED_NAME, Staged};
use serde_json::{Value, json};

/// The SHA-256 digest of 1 KiB of zeros, the content of the tests' images with a manifest.
const ZERO_KIB_SHA256: &str = "5f70bf18a086007016e948b04aed3b82103a36bea41755b6cddfaf10ace3c6ef";

/// Returns a directory of the test `name`, empty, below Cargo's temporary directory.
fn directory(name: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    directory
}

/// Writes a file of `length` bytes at `path`, and returns the path.
fn image(path: PathBuf, length: usize) -> PathBuf {
    fs::write(&path, vec![0; length]).unwrap();
    path
}

/// Writes the manifest of the image at `path` beside it, with the format 1, the artifact's name
/// `artifact`, its digest `digest`, and a package.
fn manifest(path: &Path, artifact: &str, digest: &str) {
    let mut name = path.file_name().unwrap().to_owned();
    name.push(".manifest.json");
    let manifest = json!({
        "format": 1,
        "artifact": artifact,
        "artifact_sha256": digest,
        "packages": [{"name": "busybox"}],
    });
    fs::write(path.with_file_name(name), manifest.to_string()).unwrap();
}

/// Returns the identity that the tests expect of their images.
fn identity() -> Uuid {
    Uuid::parse("0a490905-41be-ed5f-984e-63eb7e5e20b1").unwrap()
}

/// Returns the absolute path of `path`.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap()
}

#[test]
fn staging_verifies_the_image_and_binds_the_scratch() {
    let directory = directory("layered-stage");
    let distro = image(directory.join("distro.erofs"), 1024);
    let scratch = image(directory.join("scratch.ext4"), 4096);

    // An image without a manifest passes without being read, and the scratch image is bound
    // without being read either. Each is staged at its absolute path, as a plan names it.
    let staged = Staged::stage(&distro, ImageFormat::Erofs, identity(), Some(&scratch)).unwrap();
    assert_eq!(staged.distro.path, absolute(&distro));
    let binding = &staged.distro.binding;
    assert_eq!(binding.format(), DistroFormat::Erofs);
    assert_eq!(binding.identity(), identity().to_string());
    assert_eq!(binding.manifest(), None);
    let staged_scratch = staged.scratch.as_ref().unwrap();
    assert_eq!(staged_scratch.path, absolute(&scratch));
    assert_eq!(staged_scratch.binding.length(), 4096);

    // A manifest beside the image must name it and its digest, which verification compares with a
    // hash of the image.
    manifest(&distro, "distro.erofs", ZERO_KIB_SHA256);
    let staged = Staged::stage(&distro, ImageFormat::Erofs, identity(), None).unwrap();
    assert!(staged.distro.binding.manifest().is_some());
    assert_eq!(staged.scratch, None);
    for (artifact, digest) in [
        ("distro.erofs", &"0".repeat(64)[..]),
        ("other.erofs", ZERO_KIB_SHA256),
    ] {
        manifest(&distro, artifact, digest);
        let error = Staged::stage(&distro, ImageFormat::Erofs, identity(), None).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert_eq!(
            error.message(),
            "sandbox distro manifest does not match its artifact"
        );
    }

    // A GPT disk over RAM.
    let disk = image(directory.join("distro.img"), 1024);
    let staged = Staged::stage(&disk, ImageFormat::Gpt, identity(), None).unwrap();
    assert_eq!(staged.distro.binding.format(), DistroFormat::Gpt);
    assert_eq!(staged.scratch, None);

    // Staging refuses what no launch could attach: an image that does not exist, and one that
    // OpenVMM would not attach as a raw image.
    let missing = directory.join("missing.erofs");
    let error = Staged::stage(&missing, ImageFormat::Erofs, identity(), None).unwrap_err();
    assert!(
        error.message().contains("distro EROFS layer not found"),
        "{error}"
    );
    let container = image(directory.join("distro.vhd"), 1024);
    let error = Staged::stage(&container, ImageFormat::Gpt, identity(), None).unwrap_err();
    assert!(
        error.message().contains("disk paths ending in .vhd"),
        "{error}"
    );
    let error = Staged::stage(&disk, ImageFormat::Gpt, identity(), Some(&missing)).unwrap_err();
    assert!(
        error.message().contains("ext4 scratch image not found"),
        "{error}"
    );
}

#[test]
fn a_launch_takes_the_bindings_that_the_store_stages_for_its_images() {
    let directory = directory("layered-bindings");
    let distro = image(directory.join("distro.erofs"), 1024);
    let scratch = image(directory.join("scratch.ext4"), 1024);
    let other = image(directory.join("other.ext4"), 1024);
    let staged = Staged::stage(&distro, ImageFormat::Erofs, identity(), Some(&scratch)).unwrap();
    let scratch_binding = staged.scratch.as_ref().unwrap().binding.clone();
    let bindings = |scratch: Option<&Path>| {
        let launch =
            LayeredLaunch::new(&distro, ImageFormat::Erofs, identity(), scratch, None).unwrap();
        staged.bindings(&launch.plan)
    };
    assert_eq!(
        bindings(Some(&scratch)),
        Bindings {
            distro: Some(staged.distro.binding.clone()),
            scratch: Some(scratch_binding),
        }
    );
    // An upper in RAM takes no scratch image, and another scratch image is not staged.
    for scratch in [None, Some(other.as_path())] {
        assert_eq!(
            bindings(scratch),
            Bindings {
                distro: Some(staged.distro.binding.clone()),
                scratch: None,
            }
        );
    }
    // Another image is not staged either.
    let launch = LayeredLaunch::new(&other, ImageFormat::Erofs, identity(), None, None).unwrap();
    assert_eq!(staged.bindings(&launch.plan), Bindings::default());
}

#[test]
fn the_store_records_the_staged_images() {
    let directory = directory("layered-staged");
    let distro = image(directory.join("distro.erofs"), 1024);
    let scratch = image(directory.join("scratch.ext4"), 1024);
    assert_eq!(Staged::read(&directory).unwrap(), None);
    let staged = Staged::stage(&distro, ImageFormat::Erofs, identity(), Some(&scratch)).unwrap();
    let json = staged.to_json().unwrap();
    let expected = json!({
        "format": 1,
        "distro": {
            "path": absolute(&distro).to_str().unwrap(),
            "binding": serde_json::to_value(&staged.distro.binding).unwrap(),
        },
        "scratch": {
            "path": absolute(&scratch).to_str().unwrap(),
            "binding": serde_json::to_value(&staged.scratch.as_ref().unwrap().binding).unwrap(),
        },
    });
    assert_eq!(json, expected);

    // The record replaces an earlier one in the store, and reads back as it was written.
    fs::write(directory.join(STAGED_NAME), "earlier").unwrap();
    staged.write(&directory).unwrap();
    let written = fs::read_to_string(directory.join(STAGED_NAME)).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&written).unwrap(), json);
    assert_eq!(Staged::read(&directory).unwrap(), Some(staged.clone()));
    let ram = Staged::stage(&distro, ImageFormat::Erofs, identity(), None).unwrap();
    let mut without_scratch = json.clone();
    without_scratch["scratch"] = Value::Null;
    assert_eq!(ram.to_json().unwrap(), without_scratch);
    assert_eq!(Staged::from_json(&without_scratch).unwrap(), ram);

    // Other forms are refused.
    let mut cases = Vec::new();
    let mut change = |problem: &'static str, edit: &dyn Fn(&mut Value)| {
        let mut record = json.clone();
        edit(&mut record);
        cases.push((record, problem));
    };
    change(
        "has the format 2, which this build does not read",
        &|record| {
            record["format"] = json!(2);
        },
    );
    change("is not an object of a format", &|record| {
        record["extra"] = json!(null);
    });
    change("is not an object of a format", &|record| {
        record.as_object_mut().unwrap().remove("scratch");
    });
    change(
        "does not name its image by a path and a binding",
        &|record| {
            record["distro"]["extra"] = json!(null);
        },
    );
    change(
        "names its scratch image by a path that is not a string",
        &|record| {
            record["scratch"]["path"] = json!(1);
        },
    );
    change(
        "holds a binding of the image that does not decode",
        &|record| {
            record["distro"]["binding"]["extra"] = json!(null);
        },
    );
    change(
        "holds a binding of the scratch image that does not decode",
        &|record| record["scratch"]["binding"]["version"] = json!("1"),
    );
    cases.push((json!([1]), "is not an object of a format"));
    for (record, problem) in cases {
        let error = Staged::from_json(&record).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
        assert!(error.message().contains(problem), "{problem}: {error}");
    }
    fs::write(directory.join(STAGED_NAME), "{").unwrap();
    let error = Staged::read(&directory).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(
        error
            .message()
            .starts_with("cannot read the record of the staged images"),
        "{error}"
    );
}

#[test]
fn the_host_program_stages_images_in_the_store() {
    let directory = directory("layered-stage-command");
    let distro = image(directory.join("distro.erofs"), 1024);
    let scratch = image(directory.join("scratch.ext4"), 1024);
    let store = directory.join("store");
    let stage = |options: &[&Path]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_runtime-adapter-host"));
        let mut image = OsString::from("erofs:");
        image.push(&distro);
        command.arg("stage").arg(&store).arg("--distro").arg(image);
        command.arg("--distro-identity").arg(identity().to_string());
        for scratch in options {
            let mut upper = OsString::from("scratch:");
            upper.push(scratch);
            command.arg("--upper").arg(upper);
        }
        command.output().unwrap()
    };

    // Staging creates the store, records both bindings in it, and prints the bytes that it read.
    let read_bytes = |output: &std::process::Output| -> u64 {
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .strip_prefix("read-bytes=")
            .and_then(|line| line.strip_suffix('\n'))
            .and_then(|bytes| bytes.parse().ok())
            .unwrap_or_else(|| panic!("{output:?}"))
    };
    let output = stage(&[scratch.as_path()]);
    assert!(output.status.success(), "{output:?}");
    read_bytes(&output);
    let expected = Staged::stage(&distro, ImageFormat::Erofs, identity(), Some(&scratch)).unwrap();
    assert_eq!(Staged::read(&store).unwrap(), Some(expected.clone()));

    // A manifest that does not match the image fails the staging, which leaves the earlier one.
    manifest(&distro, "distro.erofs", &"0".repeat(64));
    let output = stage(&[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("sandbox distro manifest does not match its artifact"),
        "{stderr}"
    );
    assert_eq!(Staged::read(&store).unwrap(), Some(expected));

    // Staging again replaces the staged images: an image whose manifest matches, without a scratch
    // image. It hashed the image, whose bytes it read.
    manifest(&distro, "distro.erofs", ZERO_KIB_SHA256);
    let output = stage(&[]);
    assert!(output.status.success(), "{output:?}");
    assert!(read_bytes(&output) >= 1024, "{output:?}");
    let staged = Staged::read(&store).unwrap().unwrap();
    assert!(staged.distro.binding.manifest().is_some());
    assert_eq!(staged.scratch, None);
}
