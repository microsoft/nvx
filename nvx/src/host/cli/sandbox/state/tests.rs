use serde_json::json;

use super::*;
use crate::host::sandbox::layer::LayerRole;
use crate::host::sandbox::mount::{MountKind, MountOwner, MountPolicy, SandboxMount, plan_mounts};
use crate::host::sandbox::{DEFAULT_HOSTNAME, DEFAULT_WORKLOAD_IDENTITY};

/// A configuration records the export's description, its paths alone, under `export`, and its
/// pins beside it under `export_pins`, and reads both back. Each start's check, which comes before
/// the VM's configuration takes the description, refuses a pin that names another object now, as
/// when a workload moved the denied object and put a decoy at its path, or one that the host can
/// no longer inspect.
#[test]
fn records_keep_the_export_with_its_pins_and_starts_refuse_changed_pins() {
    let directory = tempfile::tempdir().unwrap();
    let root = paths::resolve(directory.path()).unwrap();
    let image = root.join("distro.erofs");
    std::fs::write(&image, b"distro layer\n").unwrap();
    // The guest may write all of the share, so a workload could move the denied `secret`.
    let work = root.join("work");
    std::fs::create_dir_all(work.join("secret")).unwrap();
    let share = SandboxMount::parse(
        &format!("/workspace,{},rw", work.display()),
        MountPolicy {
            denied: vec!["secret".to_owned()],
            ..MountPolicy::default()
        },
        MountOwner::Vmm,
        MountKind::Directory,
    )
    .unwrap();
    let export = plan_mounts(&[share]).unwrap().unwrap();
    assert!(export.pins.denied[0][0].is_some());
    let layer = SandboxLayer::new(
        LayerRole::Distro,
        image.to_str().unwrap(),
        "11111111-1111-1111-1111-111111111111",
    );
    let distro = RecordedDistro::new(&layer, DistroBinding::verify(&layer).unwrap()).unwrap();
    let options = ProvisionConfig {
        hypervisor: "whp",
        cpu_profile: None,
        memory_mib: 128,
        cmdline: "",
        ram_upper_percent: None,
        hostname: DEFAULT_HOSTNAME,
        workload_identity: DEFAULT_WORKLOAD_IDENTITY,
        export: Some(&export),
        network: None,
    };
    let text = serialize_launch(&distro, None, &options, None).unwrap();
    let object: Object = serde_json::from_str(&text).unwrap();
    assert_eq!(
        object["export"],
        serde_json::to_value(&export.description).unwrap()
    );
    assert_eq!(
        object["export_pins"],
        serde_json::to_value(&export.pins).unwrap()
    );
    let config = Config::new(object.clone()).unwrap();
    assert_eq!(config.planned_export().as_ref(), Some(&export));
    config.check().unwrap();
    assert_eq!(
        config.vm_config().unwrap().filesystem.as_ref(),
        Some(&export.description)
    );

    let refused = |config: &Config, code: ErrorCode, message: &str| {
        let error = config.check().unwrap_err();
        assert_eq!(error.code(), Some(code), "{error:?}");
        assert!(
            error.message().unwrap_or_default().contains(message),
            "{error:?}"
        );
    };
    let changed = "no longer names the host object it named at provision";
    std::fs::rename(work.join("secret"), work.join("moved")).unwrap();
    std::fs::create_dir(work.join("secret")).unwrap();
    refused(&config, ErrorCode::Other, changed);
    std::fs::remove_dir(work.join("secret")).unwrap();
    refused(&config, ErrorCode::Other, changed);
    std::fs::rename(work.join("moved"), work.join("secret")).unwrap();
    config.check().unwrap();
    // A record whose pinned path the host cannot look up, here by a name too long for its file
    // system, shows nothing of the object, and fails as the host's own failure.
    let mut unnamed = object;
    let long = work.join("n".repeat(300));
    unnamed["export"]["children"][0]["denied"] = json!([long]);
    let unnamed = Config::new(unnamed).unwrap();
    refused(
        &unnamed,
        ErrorCode::Io,
        &format!("cannot inspect {}", long.display()),
    );
}

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
    let distro = RecordedDistro::new(&layer, binding).unwrap();
    let options = ProvisionConfig {
        hypervisor: "whp",
        cpu_profile: None,
        memory_mib: 128,
        cmdline: "",
        ram_upper_percent: None,
        hostname: DEFAULT_HOSTNAME,
        workload_identity: DEFAULT_WORKLOAD_IDENTITY,
        export: None,
        network: None,
    };
    for name in [None, Some("My.snapshot-1"), Some("other")] {
        let source = name.map(|name| Source {
            snapshot: SnapshotName::try_from(name.to_owned()).unwrap(),
            store: directory.path().join("store"),
            workload: vec!["/bin/echo".to_owned(), name.to_owned()],
        });
        let text = serialize_launch(&distro, None, &options, source.as_ref()).unwrap();
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

/// A configuration records the CPU profile of `--cpu-profile` under `cpu_profile`, which each
/// start requests of OpenVMM, and none without it, so that a start then takes OpenVMM's default,
/// `auto`. Each start refuses a recorded profile that `--cpu-profile` refuses as malformed.
#[test]
fn records_keep_the_cpu_profile_that_their_starts_request() {
    let directory = tempfile::tempdir().unwrap();
    let image = directory.path().join("distro.erofs");
    std::fs::write(&image, b"distro layer\n").unwrap();
    let layer = SandboxLayer::new(
        LayerRole::Distro,
        image.to_str().unwrap(),
        "11111111-1111-1111-1111-111111111111",
    );
    let distro = RecordedDistro::new(&layer, DistroBinding::verify(&layer).unwrap()).unwrap();
    let options = |cpu_profile| ProvisionConfig {
        hypervisor: "whp",
        cpu_profile,
        memory_mib: 128,
        cmdline: "",
        ram_upper_percent: None,
        hostname: DEFAULT_HOSTNAME,
        workload_identity: DEFAULT_WORKLOAD_IDENTITY,
        export: None,
        network: None,
    };
    for profile in [None, Some("auto"), Some("host"), Some("intel.alderlake.v1")] {
        let text = serialize_launch(&distro, None, &options(profile), None).unwrap();
        let object: Object = serde_json::from_str(&text).unwrap();
        assert_eq!(
            object.get("cpu_profile").and_then(Value::as_str),
            profile,
            "{text}"
        );
        assert_eq!(object.contains_key("cpu_profile"), profile.is_some());
        let config = Config::new(object).unwrap();
        config.check().unwrap();
        assert_eq!(config.cpu_profile(), profile);
    }
    let text = serialize_launch(&distro, None, &options(Some("host")), None).unwrap();
    let object: Object = serde_json::from_str(&text).unwrap();
    for value in [json!(""), json!("Host"), json!("intel.alderlake")] {
        let mut recorded = object.clone();
        recorded.insert("cpu_profile".to_owned(), value.clone());
        let error = Config::new(recorded).unwrap().check().unwrap_err();
        assert_eq!(
            error.message(),
            Some("sandbox configuration is malformed"),
            "{value}"
        );
    }
    let mut recorded = object;
    recorded.insert("cpu_profile".to_owned(), json!(1));
    assert_eq!(
        Config::new(recorded)
            .err()
            .and_then(|error| error.message().map(str::to_owned)),
        Some("sandbox configuration is malformed".to_owned())
    );
}

/// A sandbox's scratch image is a file of its own: the check of the images, which `run` and
/// `provision` make of their options and each start of its record, refuses a scratch image that
/// is the distro layer, by its path or through a hard or symbolic link, which OpenVMM would attach
/// as both the read-only layer and the writable scratch, and accepts a copy of the layer.
#[test]
fn images_refuse_a_scratch_image_that_is_the_distro_layer() {
    let directory = tempfile::tempdir().unwrap();
    let root = paths::resolve(directory.path()).unwrap();
    let image = root.join("distro.erofs");
    std::fs::write(&image, b"distro layer\n").unwrap();
    let copy = root.join("copy.ext4");
    std::fs::copy(&image, &copy).unwrap();
    let hard = root.join("hard.ext4");
    std::fs::hard_link(&image, &hard).unwrap();
    let mut scratches = vec![image.clone(), hard];
    let symbolic = root.join("symbolic.ext4");
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&image, &symbolic);
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(&image, &symbolic);
    match linked {
        Ok(()) => scratches.push(symbolic),
        // Windows lets only Developer Mode or a privilege create a symbolic link.
        Err(error) => eprintln!("skipped the symbolic link: {error}"),
    }
    let refusal = |scratch: &Path, description: &str| {
        format!(
            "ext4 scratch image {} is the same file as the {description} {}; a sandbox needs a \
             scratch image of its own",
            scratch.display(),
            image.display()
        )
    };
    let path = image.to_str().unwrap();
    let uuid = "11111111-1111-1111-1111-111111111111";
    let erofs = SandboxLayer::new(LayerRole::Distro, path, uuid);
    let gpt = SandboxLayer::gpt(LayerRole::Distro, path, uuid);
    for (layer, description) in [(&erofs, "distro EROFS layer"), (&gpt, "distro GPT image")] {
        check_images(layer, Some(copy.as_path()), None).unwrap();
        for scratch in scratches.iter().map(PathBuf::as_path) {
            let error = check_images(layer, Some(scratch), None).unwrap_err();
            assert_eq!(error.code(), Some(ErrorCode::Unsupported), "{error:?}");
            assert_eq!(
                error.message(),
                Some(refusal(scratch, description).as_str())
            );
        }
    }
    // A provision before the check recorded such a sandbox, with the layer's binding as its scratch
    // image's, which each start now refuses before it claims or launches anything.
    let distro = RecordedDistro::new(&erofs, DistroBinding::verify(&erofs).unwrap()).unwrap();
    let scratch = RecordedScratch::new(Some(copy.as_path())).unwrap();
    let options = ProvisionConfig {
        hypervisor: "whp",
        cpu_profile: None,
        memory_mib: 128,
        cmdline: "",
        ram_upper_percent: None,
        hostname: DEFAULT_HOSTNAME,
        workload_identity: DEFAULT_WORKLOAD_IDENTITY,
        export: None,
        network: None,
    };
    let text = serialize_launch(&distro, scratch.as_ref(), &options, None).unwrap();
    let mut object: Object = serde_json::from_str(&text).unwrap();
    Config::new(object.clone()).unwrap().check().unwrap();
    object["upper"]["path"] = json!(path);
    object["scratch_binding"] =
        serde_json::to_value(ScratchBinding::bind(&image).unwrap()).unwrap();
    let config = Config::new(object).unwrap();
    assert_eq!(config.scratch().as_deref(), Some(image.as_path()));
    let error = config.check().unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Unsupported), "{error:?}");
    assert_eq!(
        error.message(),
        Some(refusal(image.as_path(), "distro EROFS layer").as_str())
    );
}
