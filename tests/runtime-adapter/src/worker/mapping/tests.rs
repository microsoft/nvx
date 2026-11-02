use super::*;

#[test]
fn refusals_name_their_kind_and_path() {
    assert_eq!(refusal(&MappingError::Reserved), "reserved");
    assert_eq!(refusal(&MappingError::TargetKind), "target-kind");
    let link = MappingError::Link {
        path: PathBuf::from("/srv/link"),
    };
    assert_eq!(refusal(&link), "link /srv/link");
    assert_eq!(at_link(refusal(&link), "/srv/link"), "link");
    assert_eq!(at_link(refusal(&link), "/srv"), "link /srv/link");
    let foreign = MappingError::Foreign {
        path: PathBuf::from("/m/new"),
    };
    assert_eq!(refusal(&foreign), "foreign /m/new");
    assert_eq!(
        in_root(Path::new("/run/a/rootfs"), Path::new("/run/a/rootfs/srv/x")),
        "/srv/x"
    );
}

#[test]
fn the_probes_take_named_targets_and_paths_in_the_export() {
    let arguments = |list: &[&str]| {
        list.iter()
            .map(|&argument| argument.to_owned())
            .collect::<Vec<_>>()
    };
    let named = arguments(&["dir=srv/maps/dir", "file-2=srv/maps/file.txt"]);
    let expected = [
        ("dir", Path::new("srv/maps/dir")),
        ("file-2", Path::new("srv/maps/file.txt")),
    ];
    assert_eq!(mappings(&named).unwrap(), expected);
    assert!(mappings(&[]).unwrap().is_empty());
    for refused in [
        "dir",
        "=srv/maps/dir",
        "Dir=srv/maps/dir",
        "dir.x=srv/maps/dir",
        "export=srv/maps/dir",
        "hostfs=srv/maps/dir",
        "dir=/srv/maps/dir",
        "dir=srv/../dir",
        "dir=",
    ] {
        assert!(mappings(&arguments(&[refused])).is_err(), "{refused}");
    }
    let twice = mappings(&arguments(&["dir=a", "dir=b"])).unwrap_err();
    assert_eq!(twice, "the map probe names dir twice");

    assert_eq!(export_path("2"), Ok("2"));
    assert_eq!(export_path("0/inner"), Ok("0/inner"));
    for refused in ["", "/2", "2/", "2//inner", "./2", "2/..", ".."] {
        assert!(export_path(refused).is_err(), "{refused}");
    }

    assert_eq!(Kind::File(4).to_string(), "file 4");
    assert_eq!(Kind::Directory.to_string(), "directory");
    assert_eq!(Kind::Denied.to_string(), "denied");
    assert_eq!(Kind::Absent.to_string(), "absent");
}
