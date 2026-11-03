//! The adapter builds on `nvx` independently, as an adapter outside the repository does.
//!
//! - It is a workspace of its own, with a lockfile of its own, and declares the minimum supported
//!   Rust version of `nvx`.
//! - Each configuration selects one side of `nvx`. The protocol-only build depends on `nvx`
//!   alone, without features, and `nvx` on its OS layer, `nvx-sys`, alone, which depends on
//!   `nix` on Unix and on nothing elsewhere. The host-only build enables the host side of `nvx`
//!   and not its guest side. The guest-only build for `x86_64-unknown-linux-musl` enables only
//!   the guest side, and pulls in neither Tokio nor a dependency that only the host side of
//!   `nvx` declares.
//! - Each binary runs and reports the contract that it was built against, and the guest binary
//!   is a static executable.
//!
//! The dependency checks run `cargo tree` on the lockfile for every configuration, whichever one
//! the tests run in.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

/// The target of the guest-only configuration.
const GUEST_TARGET: &str = "x86_64-unknown-linux-musl";

/// A package in a dependency tree that `cargo tree` prints.
#[derive(Debug)]
struct Package {
    /// The distance from the adapter, which is at depth 0.
    depth: usize,
    /// The package name.
    name: String,
    /// The features that the configuration enables in the package.
    features: BTreeSet<String>,
}

/// Returns the adapter's directory.
fn directory() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Returns a command that runs the Cargo that builds these tests in the adapter's directory.
fn cargo() -> Command {
    let mut command = Command::new(env!("CARGO"));
    command.current_dir(directory());
    command
}

/// Runs `command` and returns its standard output, failing the test unless it succeeds.
fn output(command: &mut Command) -> String {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("cannot run {command:?}: {error}"));
    assert!(
        output.status.success(),
        "{command:?} failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("Cargo prints UTF-8")
}

/// Returns the dependency tree of the configuration with `features`, for `target` or else the
/// host, without dev-dependencies, in the depth-first order of `cargo tree`.
fn tree(features: &[&str], target: Option<&str>) -> Vec<Package> {
    let mut command = cargo();
    command.args(["tree", "--locked", "--edges", "no-dev", "--prefix", "depth"]);
    command.args(["--format", "{p}|{f}"]);
    if !features.is_empty() {
        command.args(["--features", &features.join(",")]);
    }
    if let Some(target) = target {
        command.args(["--target", target]);
    }
    output(&mut command).lines().map(parse).collect()
}

/// Parses a line of `cargo tree --prefix depth --format "{p}|{f}"`, such as
/// `2serde v1.0.0|default,std (*)`.
fn parse(line: &str) -> Package {
    let digits = line
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or_else(|| panic!("no package in {line:?}"));
    let (package, features) = line[digits..]
        .rsplit_once('|')
        .unwrap_or_else(|| panic!("no features in {line:?}"));
    let features = features.strip_suffix(" (*)").unwrap_or(features);
    Package {
        depth: line[..digits]
            .parse()
            .unwrap_or_else(|_| panic!("no depth in {line:?}")),
        name: package
            .split_whitespace()
            .next()
            .unwrap_or_else(|| panic!("no name in {line:?}"))
            .to_owned(),
        features: features
            .split(',')
            .filter(|feature| !feature.is_empty())
            .map(str::to_owned)
            .collect(),
    }
}

/// Returns the names of the packages in `tree`.
fn names(tree: &[Package]) -> BTreeSet<&str> {
    tree.iter().map(|package| package.name.as_str()).collect()
}

/// Returns the features that `tree` enables in `nvx`.
fn nvx_features(tree: &[Package]) -> BTreeSet<&str> {
    let package = tree
        .iter()
        .find(|package| package.name == "nvx")
        .expect("the adapter depends on nvx");
    package.features.iter().map(String::as_str).collect()
}

/// Returns the names of the direct dependencies of the package `name` in `tree`, including its
/// build dependencies.
fn dependencies<'a>(tree: &'a [Package], name: &str) -> BTreeSet<&'a str> {
    let start = tree
        .iter()
        .position(|package| package.name == name)
        .unwrap_or_else(|| panic!("the tree holds no {name}"));
    let depth = tree[start].depth;
    tree[start + 1..]
        .iter()
        .take_while(|package| package.depth > depth)
        .filter(|package| package.depth == depth + 1)
        .map(|package| package.name.as_str())
        .collect()
}

/// Returns the names of the direct dependencies of `nvx` in `tree`, including its build
/// dependencies.
fn nvx_dependencies(tree: &[Package]) -> BTreeSet<&str> {
    dependencies(tree, "nvx")
}

#[test]
fn the_adapter_is_a_workspace_of_its_own_with_its_own_lockfile() {
    let workspace =
        output(cargo().args(["locate-project", "--workspace", "--message-format", "plain"]));
    assert_eq!(
        std::fs::canonicalize(workspace.trim()).expect("the workspace manifest exists"),
        std::fs::canonicalize(directory().join("Cargo.toml")).expect("the manifest exists"),
    );
    assert!(directory().join("Cargo.lock").is_file());
}

#[test]
fn the_adapter_declares_the_minimum_rust_version_of_nvx() {
    let manifest = std::fs::read_to_string(directory().join("../../nvx-rs/Cargo.toml"))
        .expect("nvx has a manifest");
    let version = manifest
        .lines()
        .find_map(|line| line.strip_prefix("rust-version = "))
        .map(|version| version.trim_matches('"'));
    assert_eq!(version, Some(env!("CARGO_PKG_RUST_VERSION")));
}

#[test]
fn the_protocol_only_configuration_depends_on_nvx_alone_without_features() {
    let tree = tree(&[], None);
    assert_eq!(
        dependencies(&tree, "runtime-adapter"),
        BTreeSet::from(["nvx"])
    );
    assert_eq!(nvx_features(&tree), BTreeSet::new());
    // The OS layer, which nvx requires whatever its features, and the system calls of its Unix
    // side.
    assert_eq!(nvx_dependencies(&tree), BTreeSet::from(["nvx-sys"]));
    let system = if cfg!(unix) {
        BTreeSet::from(["nix"])
    } else {
        BTreeSet::new()
    };
    assert_eq!(dependencies(&tree, "nvx-sys"), system);
}

#[test]
fn the_host_only_configuration_enables_the_host_side_of_nvx_alone() {
    let tree = tree(&["host"], None);
    let features = nvx_features(&tree);
    assert!(features.contains("host"), "nvx features: {features:?}");
    assert!(!features.contains("guest"), "nvx features: {features:?}");
}

#[test]
fn the_guest_only_configuration_pulls_in_no_host_or_tokio_dependency() {
    let guest = tree(&["guest"], Some(GUEST_TARGET));
    assert_eq!(nvx_features(&guest), BTreeSet::from(["guest"]));
    let tokio = names(&guest)
        .into_iter()
        .filter(|name| *name == "tokio" || name.starts_with("tokio-"))
        .collect::<Vec<_>>();
    assert!(tokio.is_empty(), "the guest build pulls in {tokio:?}");

    // The dependencies that the host side of nvx declares and its guest side doesn't.
    let host = tree(&["host"], Some(GUEST_TARGET));
    let host_side = nvx_dependencies(&host);
    let guest_side = nvx_dependencies(&guest);
    let host_only = host_side
        .difference(&guest_side)
        .copied()
        .collect::<BTreeSet<_>>();
    assert!(
        !host_only.is_empty(),
        "the host side of nvx adds no dependency"
    );
    let pulled = names(&guest)
        .intersection(&host_only)
        .copied()
        .collect::<Vec<_>>();
    assert!(
        pulled.is_empty(),
        "the guest build pulls in host dependencies {pulled:?}"
    );
}

/// Runs the binary at `path`, and checks that it prints `report`, and that it refuses arguments
/// with the usage status, 2.
#[cfg(any(feature = "host", all(feature = "guest", target_os = "linux")))]
fn check_binary(path: &str, report: &str) {
    assert_eq!(output(&mut Command::new(path)), report, "{path}");
    let refused = Command::new(path)
        .arg("--unexpected")
        .output()
        .unwrap_or_else(|error| panic!("cannot run {path}: {error}"));
    assert_eq!(refused.status.code(), Some(2), "{path}");
}

#[cfg(feature = "host")]
#[test]
fn the_host_binary_reports_the_contract_that_it_was_built_against() {
    check_binary(
        env!("CARGO_BIN_EXE_runtime-adapter-host"),
        &format!(
            "runtime-adapter-host: {}; supported host: {}\n",
            runtime_adapter::contract(),
            nvx::host::platform::SUPPORTED,
        ),
    );
}

#[cfg(all(feature = "guest", target_os = "linux"))]
#[test]
fn the_guest_binary_reports_the_contract_that_it_was_built_against() {
    check_binary(
        env!("CARGO_BIN_EXE_runtime-adapter-guest"),
        &format!("runtime-adapter-guest: {}\n", runtime_adapter::contract()),
    );
}

/// The ELF program header type of a program interpreter.
const PT_INTERP: u32 = 3;
/// The ELF program header type of the dynamic section.
const PT_DYNAMIC: u32 = 2;
/// The ELF program header type that sets the permissions of the stack.
const PT_GNU_STACK: u32 = 0x6474_e551;
/// The ELF program header flag of an executable segment.
const PF_X: u32 = 1;
/// The dynamic section tag that ends it.
const DT_NULL: u64 = 0;
/// The dynamic section tag of a shared library that the executable needs.
const DT_NEEDED: u64 = 1;

/// What an ELF executable needs from a dynamic loader, and whether its stack is executable.
#[derive(Debug, PartialEq, Eq)]
struct Linkage {
    /// Whether it names a program interpreter.
    interpreter: bool,
    /// The number of shared libraries that it needs.
    needed: usize,
    /// Whether its stack is executable.
    executable_stack: bool,
}

impl Linkage {
    /// The linkage of a static executable, including a static position-independent one, which
    /// has a dynamic section for its own relocations.
    const STATIC: Self = Self {
        interpreter: false,
        needed: 0,
        executable_stack: false,
    };
}

/// Reads the linkage of a 64-bit little-endian ELF executable.
fn linkage(elf: &[u8]) -> Linkage {
    let bytes = |offset: usize, length: usize| &elf[offset..offset + length];
    let u16_at = |offset| u16::from_le_bytes(bytes(offset, 2).try_into().expect("2 bytes"));
    let u32_at = |offset| u32::from_le_bytes(bytes(offset, 4).try_into().expect("4 bytes"));
    let usize_at = |offset| {
        let value = u64::from_le_bytes(bytes(offset, 8).try_into().expect("8 bytes"));
        usize::try_from(value).expect("an offset in the file")
    };
    assert_eq!(
        bytes(0, 6),
        b"\x7fELF\x02\x01",
        "a 64-bit little-endian ELF file"
    );
    let headers = usize_at(0x20);
    let header_size = usize::from(u16_at(0x36));
    let mut linkage = Linkage::STATIC;
    for index in 0..usize::from(u16_at(0x38)) {
        let header = headers + index * header_size;
        match u32_at(header) {
            PT_INTERP => linkage.interpreter = true,
            PT_DYNAMIC => {
                linkage.needed += bytes(usize_at(header + 8), usize_at(header + 32))
                    .chunks_exact(16)
                    .map(|entry| u64::from_le_bytes(entry[..8].try_into().expect("8 bytes")))
                    .take_while(|tag| *tag != DT_NULL)
                    .filter(|tag| *tag == DT_NEEDED)
                    .count();
            }
            PT_GNU_STACK => linkage.executable_stack = u32_at(header + 4) & PF_X != 0,
            _ => {}
        }
    }
    linkage
}

/// Builds a 64-bit little-endian ELF file whose program headers describe `segments`, each a
/// type, flags, and contents.
fn synthetic_elf(segments: &[(u32, u32, &[u8])]) -> Vec<u8> {
    const HEADER_SIZE: usize = 64;
    const PROGRAM_HEADER_SIZE: usize = 56;
    let mut elf = vec![0; HEADER_SIZE + PROGRAM_HEADER_SIZE * segments.len()];
    elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
    elf[0x20..0x28].copy_from_slice(&(HEADER_SIZE as u64).to_le_bytes());
    elf[0x36..0x38].copy_from_slice(&(PROGRAM_HEADER_SIZE as u16).to_le_bytes());
    let count = u16::try_from(segments.len()).expect("few segments");
    elf[0x38..0x3a].copy_from_slice(&count.to_le_bytes());
    for (index, (kind, flags, contents)) in segments.iter().enumerate() {
        let header = HEADER_SIZE + index * PROGRAM_HEADER_SIZE;
        let offset = elf.len() as u64;
        elf.extend_from_slice(contents);
        elf[header..header + 4].copy_from_slice(&kind.to_le_bytes());
        elf[header + 4..header + 8].copy_from_slice(&flags.to_le_bytes());
        elf[header + 8..header + 16].copy_from_slice(&offset.to_le_bytes());
        elf[header + 32..header + 40].copy_from_slice(&(contents.len() as u64).to_le_bytes());
    }
    elf
}

/// Encodes a dynamic section with the entries `tags`, each with the value 0.
fn dynamic_section(tags: &[u64]) -> Vec<u8> {
    tags.iter()
        .flat_map(|tag| [tag.to_le_bytes(), 0u64.to_le_bytes()].concat())
        .collect()
}

#[test]
fn the_linkage_check_finds_what_needs_a_dynamic_loader() {
    let static_pie = synthetic_elf(&[
        (PT_DYNAMIC, 6, &dynamic_section(&[DT_NULL])),
        (PT_GNU_STACK, 6, &[]),
    ]);
    assert_eq!(linkage(&static_pie), Linkage::STATIC);
    let dynamic = synthetic_elf(&[
        (PT_INTERP, 4, b"/lib/ld-musl-x86_64.so.1\0"),
        (
            PT_DYNAMIC,
            6,
            &dynamic_section(&[DT_NEEDED, DT_NEEDED, DT_NULL, DT_NEEDED]),
        ),
        (PT_GNU_STACK, 7, &[]),
    ]);
    assert_eq!(
        linkage(&dynamic),
        Linkage {
            interpreter: true,
            needed: 2,
            executable_stack: true,
        }
    );
}

#[cfg(all(feature = "guest", target_os = "linux"))]
#[test]
fn the_guest_binary_is_a_static_executable() {
    let path = env!("CARGO_BIN_EXE_runtime-adapter-guest");
    let elf = std::fs::read(path).unwrap_or_else(|error| panic!("cannot read {path}: {error}"));
    assert_eq!(
        linkage(&elf),
        Linkage::STATIC,
        "{path} is not static: test the guest-only configuration for {GUEST_TARGET}"
    );
}
