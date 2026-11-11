use serde_json::json;

use super::*;
use crate::host::sandbox::layer::LayerRole;

#[test]
fn restored_sandbox_records_round_trip_only_their_own_source() {
    let directory = tempfile::tempdir().unwrap();
    let image = directory.path().join("distro.erofs");
    std::fs::write(&image, b"distro layer\n").unwrap();
    let layer = SandboxLayer::new(
        LayerRole::Distro,
        image.to_str().unwrap(),
        "11111111-1111-1111-1111-111111111111",
    );
    let binding = DistroBinding::verify(&layer).unwrap();
    let launch =
        SandboxLaunch::new(vec![layer], SandboxUpper::RAM, SandboxSettings::default()).unwrap();
    let distro = RecordedDistro::new(&launch, binding).unwrap();
    let options = ProvisionConfig {
        hypervisor: "whp",
        memory_mib: 128,
        cmdline: "",
        ram_upper_percent: None,
        network: None,
    };
    for name in [None, Some("My.snapshot-1"), Some("other")] {
        let source = name.map(|name| Source {
            snapshot: SnapshotName::try_from(name.to_owned()).unwrap(),
            store: directory.path().join("store"),
            workload: vec!["/bin/echo".to_owned(), name.to_owned()],
        });
        let text = serialize_launch(&launch, &distro, None, &options, source.as_ref()).unwrap();
        let object: Object = serde_json::from_str(&text).unwrap();
        for key in ["source_snapshot", "source_store", "workload"] {
            assert_eq!(object.contains_key(key), name.is_some(), "{key}");
        }
        assert_eq!(
            Config::new(object.clone()).unwrap().source().unwrap(),
            source
        );
        if source.is_none() {
            continue;
        }
        // A name that escapes its store, a part of a source, a relative store, and an empty
        // workload are refused.
        for (key, value) in [
            ("source_snapshot", Some(json!("../other"))),
            ("source_snapshot", None),
            ("source_store", None),
            ("source_store", Some(json!("relative"))),
            ("workload", None),
            ("workload", Some(json!([]))),
        ] {
            let mut malformed = object.clone();
            match value {
                Some(value) => malformed.insert(key.to_owned(), value),
                None => malformed.remove(key),
            };
            assert!(
                Config::new(malformed)
                    .and_then(|config| config.source())
                    .is_err(),
                "{key}"
            );
        }
    }
}
