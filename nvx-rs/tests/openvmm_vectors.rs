//! Compares what `nvx-rs` copies from OpenVMM with the OpenVMM submodule: the control-session
//! protocol vectors and the excerpts of OpenVMM's sources in `tests/vectors/openvmm/`, and the
//! versions of OpenVMM's contracts in `nvx::contract`.
//!
//! The protocol's tests read the copies, and the unit tests of `nvx::host::template` compile or
//! read the excerpts, because the `nvx-rs` job cannot check out the submodule, whose deploy key
//! fork pull requests lack. This test is ignored by default: the NVX CLI jobs, which check out the
//! submodule, run it, and so does an OpenVMM pin promotion.

use std::fs;
use std::path::Path;

use nvx::contract;

/// The files of OpenVMM's `vm/devices/virtio/virtio_console/test_data/` that
/// `tests/vectors/openvmm/` copies.
const FILES: [&str; 2] = [
    "control_session_protocol_cases.txt",
    "control_session_protocol_vectors.txt",
];

/// The excerpts of OpenVMM's sources that `tests/vectors/openvmm/` copies: for each, its file
/// there, and the file of the OpenVMM submodule whose lines it copies, unchanged.
const EXCERPTS: [(&str, &str); 3] = [
    (
        "microvm_control_tty_command_line.rs",
        "openvmm/openvmm_defs/src/microvm.rs",
    ),
    (
        "platform_command_line_token_is_invariant.rs",
        "openvmm/openvmm_helpers/src/snapshot/microvm.rs",
    ),
    (
        "port_forward_socket_creation.rs",
        "vm/devices/net/net_consomme/src/resolver.rs",
    ),
];

/// The versions of `nvx::contract` that are OpenVMM's: for each, the file of the OpenVMM
/// submodule that defines OpenVMM's, the start of the line that defines it, up to its value, and
/// the version of `nvx::contract`.
const VERSIONS: [(&str, &str, u64); 4] = [
    (
        "openvmm/openvmm_helpers/src/snapshot.rs",
        "pub const MANIFEST_VERSION: u32 = ",
        contract::SNAPSHOT_FORMAT_VERSION,
    ),
    (
        "openvmm/openvmm_helpers/src/snapshot/format.rs",
        "pub const SAVED_STATE_SCHEMA_VERSION: u32 = ",
        contract::DEVICE_STATE_SCHEMA_VERSION,
    ),
    (
        "vmm_core/virt/src/time_abi/mod.rs",
        "pub const TIME_ABI_VERSION: u32 = ",
        contract::TIME_ABI_VERSION,
    ),
    (
        "vm/devices/chipset_resources/src/microvm_time.rs",
        "pub const RESTORE_PACKET_VERSION: u8 = ",
        contract::RESTORE_PACKET_VERSION,
    ),
];

#[test]
#[ignore = "needs the OpenVMM submodule, which the NVX CLI jobs check out"]
fn the_copies_match_the_submodules_files() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let originals = crate_root.join("../openvmm/vm/devices/virtio/virtio_console/test_data");
    let copies = crate_root.join("tests/vectors/openvmm");
    let mut copied: Vec<_> = fs::read_dir(&copies)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    copied.sort();
    let mut expected: Vec<&str> = FILES
        .into_iter()
        .chain(EXCERPTS.map(|(excerpt, _)| excerpt))
        .collect();
    expected.sort_unstable();
    assert_eq!(copied, expected, "tests/vectors/openvmm/ holds other files");
    for file in FILES {
        let original = fs::read(originals.join(file)).unwrap_or_else(|error| {
            panic!(
                "cannot read {file} of the OpenVMM submodule, which `scripts/nvx.py init` checks \
                 out: {error}"
            )
        });
        let copy = fs::read(copies.join(file)).unwrap();
        assert!(
            copy == original,
            "tests/vectors/openvmm/{file} differs from the OpenVMM submodule's: copy the \
             submodule's unchanged, and record its commit in tests/vectors/README.md"
        );
    }
}

#[test]
#[ignore = "needs the OpenVMM submodule, which the NVX CLI jobs check out"]
fn the_excerpts_are_lines_of_the_submodules_files() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (excerpt, file) in EXCERPTS {
        let source =
            fs::read_to_string(crate_root.join("../openvmm").join(file)).unwrap_or_else(|error| {
                panic!(
                    "cannot read {file} of the OpenVMM submodule, which `scripts/nvx.py init` \
                     checks out: {error}"
                )
            });
        let copy =
            fs::read_to_string(crate_root.join("tests/vectors/openvmm").join(excerpt)).unwrap();
        // Whole lines: the excerpt starts at the start of a line, and ends with its newline.
        assert!(
            copy.ends_with('\n') && format!("\n{source}").contains(&format!("\n{copy}")),
            "tests/vectors/openvmm/{excerpt} is not a run of whole lines of {file} of the OpenVMM \
             submodule: copy the submodule's lines unchanged, record its commit in \
             tests/vectors/README.md, and update the tests that they fail"
        );
    }
}

#[test]
#[ignore = "needs the OpenVMM submodule, which the NVX CLI jobs check out"]
fn the_contract_versions_are_the_submodules() {
    let openvmm = Path::new(env!("CARGO_MANIFEST_DIR")).join("../openvmm");
    for (file, definition, version) in VERSIONS {
        let source = fs::read_to_string(openvmm.join(file)).unwrap_or_else(|error| {
            panic!(
                "cannot read {file} of the OpenVMM submodule, which `scripts/nvx.py init` checks \
                 out: {error}"
            )
        });
        let values: Vec<&str> = source
            .lines()
            .filter_map(|line| line.trim().strip_prefix(definition)?.strip_suffix(';'))
            .collect();
        assert_eq!(
            values,
            [version.to_string()],
            "{file} of the OpenVMM submodule does not define `{definition}{version};` once: \
             change the version of nvx::contract that names {file} with OpenVMM's, and the code \
             that reads what it versions"
        );
    }
}
