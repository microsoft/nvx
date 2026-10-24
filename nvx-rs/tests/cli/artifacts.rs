//! `nvx-rs artifacts`, which prints the paths of the artifacts of the root as JSON, as
//! `json.dumps(value, indent=2, sort_keys=True)` writes it: the layout and the root, OpenVMM, the
//! guest kernel, and every guest in the order of `nvx.py`, with its initramfs, package manifest,
//! and distro layer and manifest, or `null`.

use nvx::host::artifacts::Guest;
use serde_json::{Value, json};

use crate::harness::{LINE_END, Root};

/// A root whose artifacts `artifacts` lists.
struct Listing {
    /// The name of the case.
    name: &'static str,
    /// The variables that the case sets, with placeholders.
    environment: &'static [(&'static str, &'static str)],
    /// The OpenVMM that `artifacts` prints, with placeholders.
    openvmm: &'static str,
}

/// The roots whose artifacts `artifacts` lists: a checkout, and one whose OpenVMM `NVX_OPENVMM`
/// overrides, which it does not check, since the variable may name an OpenVMM that does not exist.
const LISTINGS: [Listing; 2] = [
    Listing {
        name: "checkout",
        environment: &[],
        openvmm: "{root/openvmm/target/release/openvmm{exe}}",
    },
    Listing {
        name: "OpenVMM override",
        environment: &[("NVX_OPENVMM", "{root/alt/openvmm{exe}}")],
        openvmm: "{root/alt/openvmm{exe}}",
    },
];

/// Returns the paths that `artifacts` prints for a checkout, with placeholders, with `openvmm` as
/// its OpenVMM.
fn paths(openvmm: &str) -> Value {
    json!({
        "guests": [
            {
                "distribution": "Alpine Linux",
                "distro_layer": null,
                "distro_manifest": null,
                "initramfs": "{root/build/initramfs.cpio.gz}",
                "name": "alpine",
                "package_manifest": "{root/build/initramfs.cpio.gz.packages.json}",
            },
            {
                "distribution": "Ubuntu Base",
                "distro_layer": "{root/build/ubuntu-distro.erofs}",
                "distro_manifest": "{root/build/ubuntu-distro.erofs.manifest.json}",
                "initramfs": "{root/build/initramfs-ubuntu.cpio.gz}",
                "name": "ubuntu",
                "package_manifest": "{root/build/initramfs-ubuntu.cpio.gz.packages.json}",
            },
            {
                "distribution": "Azure Linux",
                "distro_layer": null,
                "distro_manifest": null,
                "initramfs": "{root/build/initramfs-azurelinux.cpio.gz}",
                "name": "azurelinux",
                "package_manifest": "{root/build/initramfs-azurelinux.cpio.gz.packages.json}",
            },
        ],
        "kernel": "{root/build/vmlinux}",
        "layout": "repository",
        "openvmm": openvmm,
        "root": "{root}",
    })
}

#[test]
fn artifacts_list_every_guest_in_order() {
    let root = Root::new();
    for case in &LISTINGS {
        let name = case.name;
        let outcome = root.nvx_rs_with(&["artifacts"], case.environment);
        outcome.assert_success();
        let expected = root.expand_json(&paths(case.openvmm));
        let printed: Value = serde_json::from_slice(&outcome.stdout).unwrap();
        assert_eq!(printed, expected, "{name}");
        let names: Vec<&str> = printed["guests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|guest| guest["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, Guest::ALL.map(Guest::name), "{name}");
        // Two spaces indent each level, and the members are sorted, as Python writes them.
        let layout = ensure_ascii(&serde_json::to_string_pretty(&expected).unwrap()) + "\n";
        assert_eq!(outcome.stdout(), layout.replace('\n', LINE_END), "{name}");
    }
}

/// Returns `json`, which `serde_json` wrote, as `json.dumps` writes it with `ensure_ascii`: every
/// character outside printable ASCII but a newline between lines, such as one of a root below a
/// non-ASCII temporary directory, becomes `\uXXXX`, by the UTF-16 code units of the character.
fn ensure_ascii(json: &str) -> String {
    let mut escaped = String::with_capacity(json.len());
    for character in json.chars() {
        if character == '\n' || (' '..='~').contains(&character) {
            escaped.push(character);
        } else {
            for unit in character.encode_utf16(&mut [0; 2]) {
                escaped.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    escaped
}
