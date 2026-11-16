//! Compares `src/host/cli/cpu_generations.json`, the copy of the identities of OpenVMM's built-in
//! CPU profiles from which `nvx run` guides a failed cold boot, with the profile documents of
//! the OpenVMM submodule's `vmm_core/cpu_profile/profiles/`.
//!
//! `nvx` reads the copy, because the `nvx` job cannot check out the submodule, whose deploy
//! key fork pull requests lack. This test is ignored by default: the NVX CLI jobs, which check out
//! the submodule, run it, and so does an OpenVMM pin promotion.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::{Value, json};

/// Returns the identity of the profile `document`, with the fields that the copy holds: its ID,
/// its vendor, and its generation's name and CPU models.
fn identity(document: &Value) -> Value {
    let generation = &document["generation"];
    let cpus: Vec<Value> = generation["cpus"]
        .as_array()
        .unwrap_or_else(|| panic!("{document} names no CPU models"))
        .iter()
        .map(|cpu| {
            json!({
                "family": cpu["family"],
                "model": cpu["model"],
                "steppings": cpu["steppings"],
            })
        })
        .collect();
    json!({
        "id": document["id"],
        "vendor": document["vendor"],
        "generation": {"name": generation["name"], "cpus": cpus},
    })
}

/// Returns `profiles` by their IDs, failing on an ID that two of them share.
fn by_id(profiles: impl IntoIterator<Item = Value>) -> BTreeMap<String, Value> {
    let mut identities = BTreeMap::new();
    for profile in profiles {
        let id = profile["id"].as_str().unwrap().to_owned();
        assert!(
            identities.insert(id.clone(), profile).is_none(),
            "{id} is listed twice"
        );
    }
    identities
}

#[test]
#[ignore = "needs the OpenVMM submodule, which the NVX CLI jobs check out"]
fn the_copy_matches_the_submodules_profiles() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let directory = crate_root.join("../openvmm/vmm_core/cpu_profile/profiles");
    let entries = fs::read_dir(&directory).unwrap_or_else(|error| {
        panic!(
            "cannot read the CPU profiles of the OpenVMM submodule, which `scripts/nvx.py init` \
             checks out: {error}"
        )
    });
    let mut pinned = Vec::new();
    for entry in entries {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            pinned.push(identity(&document));
        }
    }
    let copy: Vec<Value> = serde_json::from_slice(
        &fs::read(crate_root.join("src/host/cli/cpu_generations.json")).unwrap(),
    )
    .unwrap();
    // The copy holds nothing but the identities, which nvx reads with these fields alone.
    assert_eq!(
        copy,
        copy.iter().map(identity).collect::<Vec<_>>(),
        "src/host/cli/cpu_generations.json holds other fields"
    );
    assert!(
        !pinned.is_empty(),
        "the OpenVMM submodule pins no CPU profile"
    );
    assert!(
        by_id(copy) == by_id(pinned),
        "src/host/cli/cpu_generations.json differs from the CPU profiles that the OpenVMM \
         submodule pins: copy the ID, the vendor, and the generation of each profile, and keep \
         the order in which nvx run lists the generations"
    );
}
