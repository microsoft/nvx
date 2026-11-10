use super::*;
use crate::host::sandbox::layer::LayerRole;

#[test]
fn restored_sandbox_records_round_trip_only_their_own_snapshot_name() {
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
    };
    for source in [None, Some("My.snapshot-1"), Some("other")] {
        let name = source.map(|name| SnapshotName::try_from(name.to_owned()).unwrap());
        let text = serialize_launch_with_snapshot(&launch, &distro, None, &options, name.as_ref())
            .unwrap();
        let object: Object = serde_json::from_str(&text).unwrap();
        assert_eq!(object.contains_key("source_snapshot"), source.is_some());
        let record = Config::new(object.clone()).unwrap();
        assert_eq!(record.source_snapshot(), name.as_ref());
        let mut malformed = object;
        malformed.insert("source_snapshot".to_owned(), Value::from("../other"));
        assert!(Config::new(malformed).is_err());
    }
}
