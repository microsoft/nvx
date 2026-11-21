use super::*;
use crate::host::filesystem::{ExportRequest, Mapping, Owner, PathList, ReservedTargets, Target};

#[test]
fn blockless_modes_and_workloads_use_cli_owned_names() {
    for mode in ["one-shot", "managed"] {
        let line = render(
            &format!(
                "nvx_lifecycle={mode} nvx_workload_uid=1000 nvx_workload_gid=1001 \
                      nvx_entrypoint=/bin/echo nvx_arg=a nvx_arg=--help nvx_workload_account=create"
            ),
            None,
        )
        .unwrap();
        assert!(line.contains(&format!("nvx_mode={mode}")));
        assert!(line.contains("nvx_identity=1000:1001"));
        assert!(line.contains("nvx_entrypoint=/bin/echo nvx_arg=a nvx_arg=--help"));
        assert!(line.contains("nvx_workload_account=create"));
        assert!(!line.contains("nvx_lifecycle="));
        assert!(!line.contains("nvx_workload_uid="));
        assert!(!line.contains("nvx_workload_gid="));
    }
    assert_eq!(
        render("quiet nvx_exec=/sbin/nvx-exit", None).unwrap(),
        "quiet nvx_exec=/sbin/nvx-exit nvx_mode=one-shot"
    );
    let caller = "  quiet\tloglevel=3\nnvx_arg='x' ";
    assert_eq!(
        render(caller, None).unwrap(),
        format!("{caller} nvx_mode=one-shot")
    );
}

#[test]
fn partial_or_root_workload_identities_and_unknown_modes_are_refused() {
    for line in [
        "nvx_workload_uid=1000",
        "nvx_identity=0:1000",
        "nvx_identity=1000:4294967295",
        "nvx_mode=other",
    ] {
        assert!(render(line, None).is_err(), "{line}");
    }
}

#[test]
fn invalid_workload_identities_are_refused_by_their_token_names() {
    let range = "must be between 1 and 4294967294";
    for (line, message) in [
        (
            "nvx_identity=1000",
            "nvx_identity must be UID:GID".to_owned(),
        ),
        (
            "nvx_identity=1:2:3",
            "nvx_identity must be UID:GID".to_owned(),
        ),
        (
            "nvx_identity=a:1000",
            "nvx_identity must contain numeric UID and GID".to_owned(),
        ),
        ("nvx_identity=0:1000", format!("nvx_identity UID {range}")),
        (
            "nvx_identity=1000:4294967295",
            format!("nvx_identity GID {range}"),
        ),
        (
            "nvx_workload_uid=0 nvx_workload_gid=1000",
            format!("nvx_workload_uid {range}"),
        ),
        (
            "nvx_workload_uid=+1000 nvx_workload_gid=1000",
            format!("nvx_workload_uid {range}"),
        ),
        (
            "nvx_workload_uid=1000 nvx_workload_gid=4294967295",
            format!("nvx_workload_gid {range}"),
        ),
        (
            "nvx_workload_uid=1000 nvx_workload_gid=1000:1000",
            format!("nvx_workload_gid {range}"),
        ),
    ] {
        let error = render(line, None).unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::Unsupported, "{line}");
        assert_eq!(error.to_string(), message, "{line}");
    }
}

#[test]
fn repeated_workload_identities_and_modes_are_refused() {
    let identity = "a workload identity may be specified only once";
    let mode = "the blockless mode may be specified only once";
    for (line, message) in [
        ("nvx_identity=1000:1000 nvx_identity=1001:1001", identity),
        ("nvx_identity=1000:1000 nvx_identity=1000:1000", identity),
        (
            "nvx_workload_uid=1000 nvx_workload_uid=1001 nvx_workload_gid=1001",
            identity,
        ),
        (
            "nvx_workload_uid=1000 nvx_workload_gid=1000 nvx_workload_gid=1001",
            identity,
        ),
        (
            "nvx_workload_uid=1000 nvx_workload_gid=1000 nvx_identity=1000:1000",
            identity,
        ),
        ("nvx_mode=one-shot nvx_lifecycle=managed", mode),
        ("nvx_mode=managed nvx_mode=managed", mode),
        ("nvx_lifecycle=managed\tnvx_lifecycle=one-shot", mode),
    ] {
        let error = render(line, None).unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::Unsupported, "{line}");
        assert_eq!(error.to_string(), message, "{line}");
    }
}

#[test]
fn planned_boot_shares_keep_table_sources_targets_access_and_file_kind() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("share");
    std::fs::create_dir(&directory).unwrap();
    let file = directory.join("single.txt");
    std::fs::write(&file, "data").unwrap();
    let mappings = [
        Mapping {
            name: "--mount",
            path: &directory,
            target: Target::Guest("/workspace"),
            read_only: true,
        },
        Mapping {
            name: "--mount",
            path: &file,
            target: Target::Guest("/config/single.txt"),
            read_only: true,
        },
    ];
    let none = |name| PathList { name, paths: &[] };
    let plan = crate::host::filesystem::plan(&ExportRequest {
        mappings: &mappings,
        denied: none("--mount-deny"),
        allowed: none("--mount-allow"),
        write: none("--mount-write"),
        owner: Owner::Vmm,
        reserved: ReservedTargets::BLOCKLESS_ROOT,
    })
    .unwrap()
    .unwrap()
    .description;
    let table = plan.guest_table();
    assert_eq!(table[1].source, "0/single.txt");
    let line = render("", Some(&plan)).unwrap();
    assert!(line.contains("nvx_share=0,/workspace,ro"));
    assert!(line.contains("nvx_share=0/single.txt,/config/single.txt,ro,file"));
}
