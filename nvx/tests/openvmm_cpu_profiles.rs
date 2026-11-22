//! Compares `src/host/cli/cpu_generations.json`, the copy of the identities of OpenVMM's built-in
//! x86 CPU profiles from which `nvx run` guides a failed cold boot, with the profile documents of
//! the OpenVMM submodule's `vmm_core/cpu_profile/profiles/`.
//!
//! `nvx` reads the copy, because the `nvx` job cannot check out the submodule, whose deploy
//! key fork pull requests lack. This test is ignored by default: the NVX CLI jobs, which check out
//! the submodule, run it, and so does an OpenVMM pin promotion.

use std::collections::{BTreeMap, BTreeSet};
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

/// Classifies the catalog domain, not ARM policy validity, which OpenVMM's
/// `aarch64::load_catalog_entry` and its schema/digest tests check.
fn x86_identities(documents: impl IntoIterator<Item = Value>) -> Vec<Value> {
    let mut seen = BTreeSet::new();
    let mut identities = Vec::new();
    for document in documents {
        let arm = document.get("profile");
        let id = if let Some(profile) = arm {
            assert_eq!(
                profile["schema"], "nvx.arm64.cpu.v1",
                "unsupported ARM profile envelope"
            );
            assert!(
                document.get("id").is_none() && document.get("generation").is_none(),
                "ambiguous ARM and x86 profile identity"
            );
            &profile["id"]
        } else {
            assert_eq!(
                document["schema"], "openvmm-cpu-profile/v1",
                "unsupported x86 profile schema"
            );
            &document["id"]
        };
        let id = id
            .as_str()
            .filter(|id| !id.is_empty())
            .expect("malformed profile identity");
        if arm.is_some() {
            let vendor = document["vendor"]
                .as_str()
                .filter(|vendor| !vendor.is_empty())
                .expect("malformed ARM profile vendor");
            assert!(
                id.strip_prefix(vendor)
                    .is_some_and(|name| name.starts_with('.') && name.len() > 1),
                "ARM profile identity does not match its vendor"
            );
        }
        assert!(seen.insert(id.to_owned()), "{id} is listed twice");
        if arm.is_none() {
            identities.push(identity(&document));
        }
    }
    identities
}

#[test]
fn known_arm_profiles_are_not_cpuid_generation_identities() {
    let x86 = json!({
        "schema": "openvmm-cpu-profile/v1",
        "id": "intel.example.v1",
        "vendor": "GenuineIntel",
        "generation": {
            "name": "example",
            "cpus": [{"family": 6, "model": 85, "steppings": [0, 4]}]
        }
    });
    let arm = json!({
        "vendor": "qualcomm",
        "profile": {"schema": "nvx.arm64.cpu.v1", "id": "qualcomm.oryon-x1.v1"}
    });
    assert_eq!(x86_identities([arm, x86.clone()]), vec![identity(&x86)]);
}

#[test]
fn unknown_or_malformed_profiles_are_not_silently_omitted() {
    for document in [
        json!({"vendor": "qualcomm", "profile": {"schema": "unknown", "id": "qualcomm.x.v1"}}),
        json!({"vendor": "qualcomm", "profile": {"schema": "nvx.arm64.cpu.v1"}}),
        json!({"vendor": "qualcomm", "profile": {"schema": "nvx.arm64.cpu.v1", "id": "wrong.x.v1"}}),
        json!({"vendor": "qualcomm", "id": "ambiguous", "profile": {"schema": "nvx.arm64.cpu.v1", "id": "qualcomm.x.v1"}}),
        json!({"schema": "unknown", "id": "unknown.x.v1"}),
        json!({"schema": "openvmm-cpu-profile/v1", "id": "intel.x.v1", "vendor": "GenuineIntel"}),
    ] {
        assert!(std::panic::catch_unwind(|| x86_identities([document.clone()])).is_err());
    }
}

#[test]
fn duplicate_arm_profile_ids_are_not_hidden_by_classification() {
    let arm = json!({
        "vendor": "qualcomm",
        "profile": {"schema": "nvx.arm64.cpu.v1", "id": "qualcomm.oryon-x1.v1"}
    });
    assert!(std::panic::catch_unwind(|| x86_identities([arm.clone(), arm.clone()])).is_err());
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
            pinned.push(document);
        }
    }
    let pinned = x86_identities(pinned);
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
