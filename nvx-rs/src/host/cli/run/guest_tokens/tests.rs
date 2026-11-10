use super::*;
use crate::host::filesystem::{ExportRequest, Mapping, ReservedTargets, Target};

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
    let plan = crate::host::filesystem::plan(&ExportRequest::new(
        &mappings,
        ReservedTargets::BLOCKLESS_ROOT,
    ))
    .unwrap()
    .unwrap();
    let table = plan.guest_table();
    assert_eq!(table[1].source, "0/single.txt");
    let line = render("", Some(&plan)).unwrap();
    assert!(line.contains("nvx_share=0,/workspace,ro"));
    assert!(line.contains("nvx_share=0/single.txt,/config/single.txt,ro,file"));
}
