//! Stages the OpenVMM executable, guest kernel, and Alpine initramfs when the `bundled` feature
//! is enabled.
//!
//! The staged directory has the release layout (`bin/`, `guest/`, `SOURCE-MANIFEST.json`, and
//! `SHA256SUMS`). The library reads it through `ACI_EDGE_SANDBOXES_BUNDLED_ARTIFACTS_DIR`, and dependent build
//! scripts read it as `DEP_ACI_EDGE_SANDBOXES_ARTIFACTS_DIR` to ship it next to their executables.
//!
//! Environment variables:
//!
//! - `ACI_EDGE_SANDBOXES_BUNDLE_DIR`: an extracted NVX release to stage instead of downloading the release
//!   pinned in `artifacts.json`. Use it for offline builds or to bundle locally built artifacts.
//! - `ACI_EDGE_SANDBOXES_BUNDLE_HYPERVISOR`: `kvm` (default) or `mshv`; selects the Linux release package.
//! - `ACI_EDGE_SANDBOXES_BUNDLE=skip`: stages nothing, as when the feature is disabled.
//!
//! Every staged file must match the source's `SHA256SUMS`, and a downloaded archive must match
//! the digest pinned in `artifacts.json`.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    #[cfg(feature = "bundled")]
    bundle::run();
}

#[cfg(feature = "bundled")]
#[allow(dead_code, unreachable_pub)]
#[path = "src/openvmm/contract.rs"]
mod contract;

#[cfg(feature = "bundled")]
mod bundle {
    use std::collections::HashMap;
    use std::env;
    use std::fs::{self, File};
    use std::io::{BufReader, Read};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use serde_json::Value;
    use sha2::{Digest, Sha256};

    const SKIP: &str = "ACI_EDGE_SANDBOXES_BUNDLE";
    const SOURCE_DIR: &str = "ACI_EDGE_SANDBOXES_BUNDLE_DIR";
    const HYPERVISOR: &str = "ACI_EDGE_SANDBOXES_BUNDLE_HYPERVISOR";
    const MANIFEST: &str = "SOURCE-MANIFEST.json";
    const CHECKSUMS: &str = "SHA256SUMS";
    const GUEST: [&str; 2] = ["guest/vmlinux", "guest/initramfs.cpio.gz"];

    pub(crate) fn run() {
        println!("cargo::rerun-if-changed=artifacts.json");
        for name in [SKIP, SOURCE_DIR, HYPERVISOR] {
            println!("cargo::rerun-if-env-changed={name}");
        }
        if env::var_os(SKIP).is_some_and(|value| value == "skip") {
            return;
        }
        let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        let executable = match target_os.as_str() {
            "windows" => "bin/openvmm.exe",
            "linux" => "bin/openvmm",
            // The OpenVMM backend supports Linux and Windows hosts only.
            _ => return,
        };
        let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set"));
        let source = match env::var_os(SOURCE_DIR) {
            Some(directory) => {
                let directory = PathBuf::from(directory);
                println!(
                    "cargo::rerun-if-changed={}",
                    directory.join(CHECKSUMS).display()
                );
                directory
            }
            None => download(&out_dir, &target_os),
        };
        let staged = out_dir.join("nvx");
        stage(&source, &staged, executable);
        println!(
            "cargo::rustc-env=ACI_EDGE_SANDBOXES_BUNDLED_ARTIFACTS_DIR={}",
            staged.display()
        );
        println!("cargo::metadata=artifacts_dir={}", staged.display());
    }

    /// Verifies the release at `source` and copies the files the backend needs to `staged`.
    fn stage(source: &Path, staged: &Path, executable: &str) {
        let checksums = read_checksums(&source.join(CHECKSUMS));
        let manifest: Value = serde_json::from_str(
            &fs::read_to_string(source.join(MANIFEST))
                .unwrap_or_else(|error| fail(format!("cannot read {MANIFEST}: {error}"))),
        )
        .unwrap_or_else(|error| fail(format!("{MANIFEST} is not valid JSON: {error}")));
        if !crate::contract::manifest_is_compatible(&manifest) {
            fail(format!(
                "{} does not declare the {} control contract",
                source.join(MANIFEST).display(),
                crate::contract::CONTROL_CONTRACT_REVISION
            ));
        }
        let mut files = vec![executable, MANIFEST];
        files.extend(GUEST);
        if staged.exists() {
            fs::remove_dir_all(staged).unwrap_or_else(|error| {
                fail(format!("cannot clear {}: {error}", staged.display()))
            });
        }
        let mut sums = String::new();
        for name in files {
            let expected = checksums.get(name).unwrap_or_else(|| {
                fail(format!(
                    "{} does not list {name}",
                    source.join(CHECKSUMS).display()
                ))
            });
            let from = source.join(name);
            let actual = sha256(&from);
            if &actual != expected {
                fail(format!(
                    "SHA-256 mismatch for {}: expected {expected}, found {actual}",
                    from.display()
                ));
            }
            let to = staged.join(name);
            fs::create_dir_all(to.parent().expect("staged files have a parent")).unwrap_or_else(
                |error| fail(format!("cannot create {}: {error}", staged.display())),
            );
            fs::copy(&from, &to).unwrap_or_else(|error| {
                fail(format!(
                    "cannot copy {} to {}: {error}",
                    from.display(),
                    to.display()
                ))
            });
            sums.push_str(&format!("{actual}  {name}\n"));
        }
        fs::write(staged.join(CHECKSUMS), sums)
            .unwrap_or_else(|error| fail(format!("cannot write {CHECKSUMS}: {error}")));
    }

    /// Downloads and extracts the release package pinned in `artifacts.json`.
    fn download(out_dir: &Path, target_os: &str) -> PathBuf {
        let pins_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("artifacts.json");
        let pins: Value = serde_json::from_str(
            &fs::read_to_string(&pins_path)
                .unwrap_or_else(|error| fail(format!("cannot read artifacts.json: {error}"))),
        )
        .unwrap_or_else(|error| fail(format!("artifacts.json is not valid JSON: {error}")));
        let platform = if target_os == "windows" {
            "windows-whp".to_owned()
        } else {
            let hypervisor = env::var(HYPERVISOR).unwrap_or_else(|_| "kvm".to_owned());
            if hypervisor != "kvm" && hypervisor != "mshv" {
                fail(format!(
                    "{HYPERVISOR} must be kvm or mshv, not {hypervisor:?}"
                ));
            }
            format!("linux-{hypervisor}")
        };
        let text = |value: &Value, field: &str| -> String {
            value
                .get(field)
                .and_then(Value::as_str)
                .unwrap_or_else(|| fail(format!("artifacts.json lacks {field}")))
                .to_owned()
        };
        let asset = pins
            .get("assets")
            .and_then(|assets| assets.get(&platform))
            .unwrap_or_else(|| fail(format!("artifacts.json pins no {platform} package")));
        let (repository, tag) = (text(&pins, "repository"), text(&pins, "tag"));
        let (name, digest) = (text(asset, "name"), text(asset, "sha256"));
        let root = name
            .strip_suffix(".zip")
            .or_else(|| name.strip_suffix(".tar.gz"))
            .unwrap_or_else(|| fail(format!("unsupported release package {name}")));

        let downloads = out_dir.join("download");
        fs::create_dir_all(&downloads).unwrap_or_else(|error| {
            fail(format!("cannot create {}: {error}", downloads.display()))
        });
        let archive = downloads.join(&name);
        if !archive.is_file() || sha256(&archive) != digest {
            let partial = downloads.join(format!("{name}.part"));
            let url = format!("https://github.com/{repository}/releases/download/{tag}/{name}");
            println!("cargo::warning=nvx: downloading {url}");
            execute(
                Command::new("curl")
                    .args(["--fail", "--silent", "--show-error", "--location"])
                    .args(["--retry", "5", "--retry-delay", "5", "--retry-all-errors"])
                    .arg("--output")
                    .arg(&partial)
                    .arg(&url),
            );
            let actual = sha256(&partial);
            if actual != digest {
                let _ = fs::remove_file(&partial);
                fail(format!(
                    "SHA-256 mismatch for {url}: expected {digest}, found {actual}"
                ));
            }
            fs::rename(&partial, &archive)
                .unwrap_or_else(|error| fail(format!("cannot store {name}: {error}")));
        }

        let extracted = downloads.join("extract");
        if extracted.exists() {
            fs::remove_dir_all(&extracted).unwrap_or_else(|error| {
                fail(format!("cannot clear {}: {error}", extracted.display()))
            });
        }
        fs::create_dir_all(&extracted).unwrap_or_else(|error| {
            fail(format!("cannot create {}: {error}", extracted.display()))
        });
        let executable = if target_os == "windows" {
            "bin/openvmm.exe"
        } else {
            "bin/openvmm"
        };
        let mut members = vec![CHECKSUMS, MANIFEST, executable];
        members.extend(GUEST);
        let members: Vec<String> = members
            .iter()
            .map(|member| format!("{root}/{member}"))
            .collect();
        execute(&mut extraction(
            name.ends_with(".zip"),
            &archive,
            &extracted,
            &members,
        ));
        extracted.join(root)
    }

    /// Returns a command that extracts `members` of a release package into `directory` with a
    /// host tool that reads the package's format.
    fn extraction(zip: bool, archive: &Path, directory: &Path, members: &[String]) -> Command {
        let tar_command = if zip {
            // GNU tar, the usual `tar` on Linux, cannot read ZIP archives, but bsdtar can.
            [tar(), PathBuf::from("bsdtar")]
                .into_iter()
                .find(|candidate| is_bsdtar(candidate))
        } else {
            Some(tar())
        };
        let mut command;
        match tar_command {
            Some(tar) => {
                command = Command::new(tar);
                command.arg("-xf").arg(archive).arg("-C").arg(directory);
                command.args(members);
            }
            None if succeeds(Command::new("unzip").arg("-v")) => {
                command = Command::new("unzip");
                command.arg("-q").arg(archive).args(members);
                command.arg("-d").arg(directory);
            }
            None => fail(format!(
                "cannot extract {}: reading ZIP archives requires bsdtar or unzip on the build \
                 host; install one, or set {SOURCE_DIR} to an extracted release",
                archive.display()
            )),
        }
        command
    }

    /// Returns the host's `tar`.
    fn tar() -> PathBuf {
        // Windows ships bsdtar, which reads ZIP archives; GNU tar from Git for Windows does not.
        if let Some(system_root) = env::var_os("SystemRoot") {
            let system_tar = Path::new(&system_root).join("System32").join("tar.exe");
            if system_tar.is_file() {
                return system_tar;
            }
        }
        PathBuf::from("tar")
    }

    /// Returns whether `tar` is libarchive's bsdtar, which also reads ZIP archives.
    fn is_bsdtar(tar: &Path) -> bool {
        Command::new(tar)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success() && output.stdout.starts_with(b"bsdtar"))
    }

    fn succeeds(command: &mut Command) -> bool {
        command.output().is_ok_and(|output| output.status.success())
    }

    fn execute(command: &mut Command) {
        let output = command
            .output()
            .unwrap_or_else(|error| fail(format!("cannot run {command:?}: {error}")));
        if !output.status.success() {
            fail(format!(
                "{command:?} failed with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }

    fn read_checksums(path: &Path) -> HashMap<String, String> {
        let text = fs::read_to_string(path)
            .unwrap_or_else(|error| fail(format!("cannot read {}: {error}", path.display())));
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let (digest, name) = line
                    .split_once(char::is_whitespace)
                    .unwrap_or_else(|| fail(format!("malformed line in {}", path.display())));
                let name = name.trim_start().trim_start_matches('*');
                (name.to_owned(), digest.to_ascii_lowercase())
            })
            .collect()
    }

    fn sha256(path: &Path) -> String {
        let file = File::open(path)
            .unwrap_or_else(|error| fail(format!("cannot open {}: {error}", path.display())));
        let mut reader = BufReader::with_capacity(1 << 20, file);
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 1 << 20];
        loop {
            let read = reader
                .read(&mut buffer)
                .unwrap_or_else(|error| fail(format!("cannot read {}: {error}", path.display())));
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn fail(message: String) -> ! {
        panic!("nvx bundled artifacts: {message}");
    }
}
