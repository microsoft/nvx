//! Tests of the typed plan: every combination of format and upper, the guest's reading of the
//! command lines that plans render, their budget, their checks, and their persisted form.

use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use super::*;
use crate::ErrorKind;
use crate::host::filesystem::{Bind, FileIdentity};
use crate::host::network::{
    Access, Destination, EgressRule, ForwardProtocol, HostLoopback, LoopbackForward, NetworkProxy,
    Protocol, ProxyScheme,
};
use crate::host::sandbox::distro::DistroBinding;
use crate::host::sandbox::mount::{boot_shares, export_command_line_fragment};
use crate::host::sandbox::tests::{deny, exported, host, host_text, native};
use crate::host::sandbox::{LayerRole, MountAccess, MountKind, SandboxLaunch, SandboxLayer};

const EROFS_UUID: &str = "11111111-1111-1111-1111-111111111111";
const GPT_GUID: &str = "fedcba98-7654-3210-fedc-ba9876543210";

fn uuid(text: &str) -> Uuid {
    Uuid::parse(text).unwrap()
}

fn message<T: std::fmt::Debug>(result: Result<T>) -> String {
    result.unwrap_err().to_string()
}

/// Returns the images of every format, with and without an identity.
fn images() -> [RootImage; 4] {
    [
        RootImage::erofs(Some(uuid(EROFS_UUID))),
        RootImage::erofs(None),
        RootImage::gpt(Some(uuid(GPT_GUID))),
        RootImage::gpt(None),
    ]
}

/// Returns the uppers: on `scratch.ext4`, and in RAM.
fn uppers() -> [SandboxUpper; 2] {
    [SandboxUpper::scratch("scratch.ext4"), SandboxUpper::RAM]
}

fn plan(image: RootImage, upper: SandboxUpper, settings: SandboxSettings) -> SandboxPlan {
    exporting(image, upper, settings, None)
}

fn exporting(
    image: RootImage,
    upper: SandboxUpper,
    settings: SandboxSettings,
    export: Option<ExportPlan>,
) -> SandboxPlan {
    SandboxPlan::new(
        SandboxDistro::new("images/distro.img", image),
        upper,
        settings,
    )
    .and_then(|plan| plan.with_export(export))
    .unwrap()
}

/// Returns the export plan of a read-write directory at `/workspace`, and of a read-only one at
/// `/srv/cache` unless `one`.
fn shares(one: bool) -> ExportPlan {
    let workspace = (
        "/workspace",
        "/host/work",
        MountAccess::ReadWrite,
        MountKind::Directory,
    );
    let cache = (
        "/srv/cache",
        "/host/cache",
        MountAccess::ReadOnly,
        MountKind::Directory,
    );
    if one {
        exported(&[workspace])
    } else {
        exported(&[workspace, cache])
    }
}

/// Returns a read-write bind of `/host/work/out`, inside the first child of [`shares`], at
/// `/workspace/out`, which pins `identity`.
fn nested(identity: Option<FileIdentity>) -> Bind {
    Bind {
        child: 0,
        source: "out".to_owned(),
        target: "/workspace/out".to_owned(),
        read_only: false,
        identity,
    }
}

/// Returns the bootstrap tokens that OpenVMM appends for the shares of `plan` in the managed
/// lifecycle, which exports them for the guest agent's mapping table.
fn fragment(plan: &SandboxPlan) -> String {
    plan.export()
        .map(export_command_line_fragment)
        .unwrap_or_default()
}

/// A rendering of a plan's kernel command line, and of the bootstrap tokens that OpenVMM appends
/// for its export plan.
type Rendering = (
    fn(&SandboxPlan, &str) -> Result<String>,
    fn(&ExportPlan) -> String,
);

/// Returns the renderings of the managed lifecycle, which announces a mapping table, and of the
/// one-shot lifecycle, which carries boot tokens, by name.
fn renderings() -> [(&'static str, Rendering); 2] {
    [
        (
            "managed",
            (
                SandboxPlan::kernel_command_line,
                export_command_line_fragment,
            ),
        ),
        (
            "one-shot",
            (SandboxPlan::one_shot_kernel_command_line, |export| {
                boot_shares(export).unwrap().fragment
            }),
        ),
    ]
}

#[test]
fn every_format_and_upper_plans_and_renders() {
    let settings = SandboxSettings {
        entrypoint: "/bin/workload".to_owned(),
        args: vec!["--serve".to_owned()],
        hostname: "example".to_owned(),
        workload_identity: (1000, 1001),
        memory_max: Some(268_435_456),
        pids_max: Some(64),
    };
    let distro = "nvx_layer=distro,0xd0003000";
    let cases = [
        (
            0,
            0,
            format!("{distro},erofs:{EROFS_UUID} nvx_scratch=0xd0006000,ext4"),
        ),
        (1, 0, format!("{distro},erofs nvx_scratch=0xd0006000,ext4")),
        (
            2,
            0,
            format!("{distro},gpt:{GPT_GUID} nvx_scratch=0xd0006000,ext4"),
        ),
        (3, 0, format!("{distro},gpt nvx_scratch=0xd0006000,ext4")),
        (
            0,
            1,
            format!("{distro},erofs:{EROFS_UUID} nvx_overlay_upper=ramfs"),
        ),
        (1, 1, format!("{distro},erofs nvx_overlay_upper=ramfs")),
        (
            2,
            1,
            format!("{distro},gpt:{GPT_GUID} nvx_overlay_upper=ramfs"),
        ),
        (3, 1, format!("{distro},gpt nvx_overlay_upper=ramfs")),
    ];
    for (image, upper, tokens) in cases {
        let (image, upper) = (images()[image], uppers()[upper].clone());
        let plan = plan(image, upper.clone(), settings.clone());
        assert_eq!(
            plan.layout(),
            RootLayout {
                image,
                upper: upper.kind()
            }
        );
        assert_eq!(
            plan.kernel_command_line("quiet").unwrap(),
            format!(
                "quiet nvx_sandbox=1 {tokens} nvx_entrypoint=/bin/workload nvx_hostname=example \
                 nvx_arg=--serve nvx_memory_max=268435456 nvx_pids_max=65"
            )
        );
        let distro_block = paths::normalized(Path::new("images/distro.img"));
        let mut expected = vec![
            "--machine".to_owned(),
            "microvm".to_owned(),
            "--microvm-sandbox-block".to_owned(),
            format!("distro:file:{},ro", distro_block.display()),
        ];
        let mut blocks = vec![(BlockRole::Distro, distro_block.as_path())];
        // A RAM upper has no scratch block device.
        if let Some(scratch) = upper.scratch_path() {
            expected.extend([
                "--microvm-sandbox-block".to_owned(),
                format!("scratch:file:{}", scratch.display()),
            ]);
            blocks.push((BlockRole::Scratch, scratch));
        }
        expected.extend([
            "--microvm-workload-identity".to_owned(),
            "1000:1001".to_owned(),
        ]);
        assert_eq!(plan.openvmm_arguments().unwrap(), expected);
        assert_eq!(plan.blocks(), blocks);
    }
}

#[test]
fn the_guest_reads_the_root_of_every_plan_from_its_command_line() {
    let settings = SandboxSettings {
        args: vec!["-c".to_owned(), "true".to_owned()],
        ..SandboxSettings::default()
    };
    for image in images() {
        for upper in uppers() {
            let plan = exporting(image, upper.clone(), settings.clone(), Some(shares(false)));
            let command_line = plan.kernel_command_line("console=hvc0 quiet").unwrap();
            assert_eq!(
                RootLayout::parse(command_line.as_bytes()),
                Ok(Some(plan.layout())),
                "{command_line}"
            );
            // OpenVMM appends its own tokens, the discovery of the plan's block devices among
            // them, which the guest reads alike.
            let mut effective =
                format!("{command_line} nr_cpus=1 virtio_mmio.device=0x1000@0xd0003000:4");
            if upper.scratch_path().is_some() {
                effective.push_str(" virtio_mmio.device=0x1000@0xd0006000:11");
            }
            effective.push_str(&fragment(&plan));
            assert_eq!(
                RootLayout::parse(effective.as_bytes()),
                Ok(Some(plan.layout())),
                "{effective}"
            );
        }
    }
}

#[test]
fn plans_refuse_the_command_lines_that_the_guest_refuses() {
    // The user's command line may add a virtio-mmio device, which the guest reads as the kernel
    // does: one at the scratch base conflicts with a RAM upper.
    let conflict = "sandbox kernel command line conflicts with the boot tokens of its root: a RAM \
                    upper takes no scratch block device, but the command line adds a virtio-mmio \
                    device at 0xd0006000";
    for image in images() {
        let plan = plan(image, SandboxUpper::RAM, SandboxSettings::default());
        for user in [
            "virtio_mmio.device=0x1000@0xd0006000:11",
            "quiet virtio-mmio.device=4K@0xd0006000:11:3",
            "\"virtio_mmio.device=0x1000@0xd0006000:11\"",
            "virtio_mmio.device=\"0x1000@ 0xd0006000:11\"",
            "virtio_mmio.device=0x1000@3489685504:11",
        ] {
            let error = plan.kernel_command_line(user).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{user}");
            assert_eq!(error.to_string(), conflict, "{user}");
        }
        for user in [
            // The driver refuses these values, so the kernel adds no device from them.
            "virtio_mmio.device=0x1000@0xd0006000:0",
            "virtio_mmio.device=0x1000@0xd0006000",
            // A device elsewhere, and a token that the kernel reads inside another value.
            "virtio_mmio.device=0x1000@0xd0007000:3",
            "x=\"y virtio_mmio.device=0x1000@0xd0006000:11\"",
        ] {
            let command_line = plan.kernel_command_line(user).unwrap();
            assert_eq!(
                RootLayout::parse(command_line.as_bytes()),
                Ok(Some(plan.layout())),
                "{user}"
            );
        }
    }
}

#[test]
fn the_longest_boot_tokens_fit_the_command_line_budget_to_the_byte() {
    // The tokens of an identified EROFS image over scratch are the longest: they take the most
    // of the budget, which every other plan keeps the rest of.
    let longest = plan(images()[0], uppers()[0].clone(), SandboxSettings::default());
    let longest_base = longest.kernel_command_line("").unwrap().len();
    for (lifecycle, (render, appended)) in renderings() {
        for export in [None, Some(shares(true)), Some(shares(false))] {
            for image in images() {
                for upper in uppers() {
                    let plan = exporting(image, upper, SandboxSettings::default(), export.clone());
                    let base = render(&plan, "").unwrap();
                    let fragment = plan.export().map(appended).unwrap_or_default();
                    // The user's command line, a space, the plan's tokens, OpenVMM's share
                    // tokens, and a NUL fill the budget exactly.
                    let fitting =
                        "x".repeat(SANDBOX_COMMAND_LINE_MAX_SIZE - base.len() - fragment.len() - 2);
                    let command_line = render(&plan, &fitting).unwrap();
                    assert_eq!(
                        command_line.len() + fragment.len() + 1,
                        SANDBOX_COMMAND_LINE_MAX_SIZE,
                        "{lifecycle}"
                    );
                    assert_eq!(
                        RootLayout::parse(command_line.as_bytes()),
                        Ok(Some(plan.layout()))
                    );
                    assert!(
                        message(render(&plan, &format!("{fitting}x"))).contains("1024-byte"),
                        "{lifecycle}"
                    );
                    if plan.export().is_none() {
                        assert!(base.len() <= longest_base, "{base}");
                    }
                }
            }
        }
    }
}

#[test]
fn the_managed_lifecycle_exports_the_shares_and_announces_their_mapping_table() {
    let export = shares(false);
    let bare = plan(images()[0], uppers()[0].clone(), SandboxSettings::default());
    let plan = exporting(
        images()[0],
        uppers()[0].clone(),
        SandboxSettings::default(),
        Some(export.clone()),
    );
    // OpenVMM exports the shares where the guest agent maps them from, after the block devices
    // and the workload identity.
    let blocks = bare.openvmm_arguments().unwrap();
    let arguments = plan.openvmm_arguments().unwrap();
    assert_eq!(arguments[..blocks.len()], blocks[..]);
    assert_eq!(arguments[blocks.len()..], export.openvmm_arguments()[..]);
    assert_eq!(
        arguments[blocks.len()..][..2],
        ["--mount-aggregate", crate::host::filesystem::GUEST_EXPORT]
    );
    // The kernel command line announces the number of mappings, not their targets, and OpenVMM
    // appends the export's tokens, read-write since a child is.
    let base = bare.kernel_command_line("quiet").unwrap();
    assert_eq!(
        plan.kernel_command_line("quiet").unwrap(),
        format!("{base} nvx_maps=2")
    );
    assert_eq!(
        fragment(&plan),
        " virtfs_dir=/run/nvx/hostfs/root virtfs_tag=microvm virtfs_mode=rw virtfs_aggregate=1"
    );
    let read_only = exporting(
        images()[0],
        uppers()[0].clone(),
        SandboxSettings::default(),
        Some(exported(&[(
            "/srv/cache",
            "/host/cache",
            MountAccess::ReadOnly,
            MountKind::Directory,
        )])),
    );
    assert!(fragment(&read_only).contains(" virtfs_mode=ro "));
    // The mapping table is the export plan's binds, in order: parents first.
    assert_eq!(plan.mapping_table(), export.guest_table());
    assert_eq!(
        plan.mapping_table()
            .iter()
            .map(|map| map.target.as_str())
            .collect::<Vec<_>>(),
        ["/workspace", "/srv/cache"]
    );
    // The one-shot lifecycle renders the same plan as boot tokens.
    let boot = boot_shares(&export).unwrap();
    assert_eq!(
        plan.one_shot_openvmm_arguments().unwrap()[blocks.len()..],
        boot.arguments[..]
    );
    assert_eq!(
        plan.one_shot_kernel_command_line("quiet").unwrap(),
        format!("{base} {}", boot.tokens.join(" "))
    );
    // Without shares, both render alike, and nothing is mapped.
    assert!(bare.mapping_table().is_empty());
    assert_eq!(bare.one_shot_openvmm_arguments().unwrap(), blocks);
    assert_eq!(bare.one_shot_kernel_command_line("quiet").unwrap(), base);
    assert!(!base.contains("nvx_maps="));
}

#[test]
fn plans_check_their_settings_as_launches_do() {
    let checked = |change: fn(&mut SandboxSettings)| {
        let mut settings = SandboxSettings::default();
        change(&mut settings);
        SandboxPlan::new(
            SandboxDistro::new("d.img", RootImage::gpt(None)),
            SandboxUpper::RAM,
            settings,
        )
    };
    for (change, expected) in [
        (
            (|settings: &mut SandboxSettings| settings.entrypoint = "bin/sh".to_owned())
                as fn(&mut SandboxSettings),
            "sandbox entrypoint must be an absolute path without whitespace, control characters, or quotes",
        ),
        (
            |settings| settings.args = vec![String::new()],
            "sandbox arguments must be nonempty and contain no whitespace, control characters, or quotes",
        ),
        (
            |settings| settings.entrypoint = "/opt/app\"".to_owned(),
            "sandbox entrypoint must be an absolute path without whitespace, control characters, or quotes",
        ),
        (
            |settings| settings.args = vec!["say\"hi".to_owned()],
            "sandbox arguments must be nonempty and contain no whitespace, control characters, or quotes",
        ),
        (
            |settings| settings.entrypoint = "/bin/\u{e0}--".to_owned(),
            "sandbox entrypoint must be an absolute path without whitespace, control characters, or quotes",
        ),
        (
            |settings| settings.args = vec!["voil\u{e0}--".to_owned()],
            "sandbox arguments must be nonempty and contain no whitespace, control characters, or quotes",
        ),
        (
            |settings| settings.hostname = "Invalid".to_owned(),
            "sandbox hostname must be a lowercase RFC 1123 label up to 63 characters",
        ),
        (
            |settings| settings.workload_identity = (0, 1),
            "sandbox workload identity must be a non-root UID:GID",
        ),
        (
            |settings| settings.memory_max = Some(0),
            "--memory-max must be positive",
        ),
    ] {
        let error = checked(change).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert_eq!(error.to_string(), expected);
    }
    checked(|_| {}).unwrap();
}

#[test]
fn plans_check_their_export_plan_for_the_container_root() {
    let checked = |change: fn(&mut ExportPlan)| {
        let mut export = shares(false);
        change(&mut export);
        SandboxPlan::new(
            SandboxDistro::new("d.img", RootImage::gpt(None)),
            SandboxUpper::RAM,
            SandboxSettings::default(),
        )
        .and_then(|plan| plan.with_export(Some(export)))
    };
    checked(|_| {}).unwrap();
    // A hidden root shows the paths that it shows again.
    checked(|export| {
        deny(export, 0, &["/host/work"]);
        export.children[0].allowed = vec![host("/host/work/public")];
    })
    .unwrap();
    // A child that the guest may write all of pins the denied and mapped objects inside it.
    checked(|export| {
        deny(export, 0, &["/host/work/secrets"]);
        export.binds.push(nested(Some(FileIdentity {
            device: 1,
            index: 9,
        })));
    })
    .unwrap();
    // Writes narrowed to `out` move only what lies strictly inside it, not `out` itself.
    checked(|export| {
        export.children[0].write = vec![host("/host/work/out")];
        deny(export, 0, &["/host/work/secrets", "/host/work/out/secret"]);
        export.binds.push(nested(None));
    })
    .unwrap();
    // A child whose root is mapped read-only writes the paths that it maps read-write.
    checked(|export| {
        export.binds[0].read_only = true;
        export.children[0].write = vec![host("/host/work/out")];
        export.binds.push(nested(None));
    })
    .unwrap();
    // A source holds the host's names joined with `/`: a Windows host reads a backslash in it as a
    // separator, which another host keeps in the name.
    let backslash = checked(|export| {
        export.binds.push(Bind {
            source: r"src\host".to_owned(),
            target: r"/workspace/src\host".to_owned(),
            ..nested(Some(FileIdentity {
                device: 1,
                index: 9,
            }))
        });
    });
    if cfg!(windows) {
        let error = backslash.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(
            error.to_string().contains(
                r#"it maps the path "src\\host" of child 0, which the host reads as other names"#
            ),
            "{error}"
        );
    } else {
        backslash.unwrap();
    }
    for (change, expected) in [
        (
            (|export: &mut ExportPlan| export.binds[0].target = "/etc".to_owned())
                as fn(&mut ExportPlan),
            "its guest target /etc would cover the guest's /etc",
        ),
        (
            |export| export.binds[0].target = "/run".to_owned(),
            "its guest target /run would cover the guest's /run",
        ),
        (
            |export| export.binds[1].target = "/proc/self".to_owned(),
            "its guest target /proc/self would cover the guest's /proc",
        ),
        (
            |export| export.binds[1].target = "/etc/resolv.conf".to_owned(),
            "its guest target /etc/resolv.conf would cover the guest's /etc/resolv.conf",
        ),
        (
            |export| export.binds[0].target = "workspace".to_owned(),
            "its guest target \"workspace\" is not absolute",
        ),
        (
            |export| export.binds[1].target = "/workspace/cache".to_owned(),
            "it maps /workspace/cache inside /workspace, where it would hide part of that path",
        ),
        (
            |export| export.binds.swap(0, 1),
            "it maps /workspace after /srv/cache, out of order or twice",
        ),
        (
            |export| export.binds[1].child = 7,
            "it maps no path at the root of child 1",
        ),
        (
            |export| export.binds[0].source = "sub".to_owned(),
            "it maps no path at the root of child 0",
        ),
        (
            |export| export.children[0].denied = vec![host("/host/work/secrets")],
            "child 0 has 1 denied paths but 0 identities",
        ),
        (
            |export| deny(export, 0, &["/host/cache/secrets"]),
            "child 0 has the policy path",
        ),
        (
            |export| export.children[1].write = vec![host("/host/cache/out")],
            "child 1 is read-only but has writable paths",
        ),
        (
            |export| export.binds[1].read_only = false,
            "child 1 is read-only but maps a path read-write",
        ),
        (
            // A read-only bind of a directory whose writes OpenVMM narrows to a path inside it.
            |export| {
                export.children[0].write = vec![host("/host/work/out")];
                export.binds[0].read_only = true;
            },
            "child 0 is writable but maps no path read-write",
        ),
        (
            |export| {
                export.children[0].denied = (0..129)
                    .map(|index| host(&format!("/host/work/secret-{index}")))
                    .collect();
                export.children[0].denied_identities = vec![None; 129];
            },
            "child 0: OpenVMM can hide at most 128 paths in one exported directory",
        ),
        (
            |export| {
                export.children[0].write = vec![host(&format!("/host/work/{}", "x".repeat(4097)))];
            },
            "child 0: OpenVMM cannot make writable",
        ),
        (
            |export| {
                export.children[0].write = (0..5)
                    .map(|index| host(&format!("/host/work/{index}{}", "x".repeat(3999))))
                    .collect();
            },
            "exceed its 16384-byte limit",
        ),
        (
            |export| {
                export.binds.insert(
                    0,
                    Bind {
                        child: 0,
                        source: "sub\0file".to_owned(),
                        target: "/data".to_owned(),
                        read_only: false,
                        identity: None,
                    },
                );
            },
            "which contains a NUL character",
        ),
        (
            |export| {
                export.binds.insert(
                    0,
                    Bind {
                        child: 0,
                        source: "x".repeat(4090),
                        target: "/data".to_owned(),
                        read_only: false,
                        identity: None,
                    },
                );
            },
            "which is too long for a guest path",
        ),
        (
            |export| export.children[1].root = host("/host/work/cache"),
            "overlaps child 0",
        ),
        (
            |export| deny(export, 0, &["/host/work/secrets", "/host/work/secrets"]),
            "lists the hidden path",
        ),
        (
            |export| {
                deny(
                    export,
                    0,
                    &["/host/work/secrets", "/host/work/secrets/keys"],
                )
            },
            "which it hides already",
        ),
        (
            |export| export.children[0].allowed = vec![host("/host/work/public")],
            "again, but hides no path that contains it",
        ),
        (
            |export| {
                deny(export, 0, &["/host/work/secrets"]);
                export.children[0].allowed = vec![host("/host/work/secrets")];
            },
            "it both hides and shows",
        ),
        (
            |export| {
                deny(export, 0, &["/host/work/secrets"]);
                export.children[0].allowed = vec![
                    host("/host/work/secrets/public"),
                    host("/host/work/secrets/public/docs"),
                ];
            },
            "which it shows already",
        ),
        (
            // OpenVMM makes no hidden path writable.
            |export| {
                deny(export, 0, &["/host/work/secrets"]);
                export.children[0].write = vec![host("/host/work/secrets/public")];
            },
            "writable, but",
        ),
        (
            |export| export.children[0].write = vec![host("/host/work")],
            "writable through a writable path",
        ),
        (
            |export| {
                export.children[0].write =
                    vec![host("/host/work/out"), host("/host/work/out/logs")];
            },
            "which it makes writable already",
        ),
        (
            |export| deny(export, 0, &["/host/work"]),
            "/workspace, but child 0 hides it",
        ),
        (
            // A path that a hidden directory holds is hidden too, unless it shows a path again.
            |export| {
                deny(export, 0, &["/host/work"]);
                export.children[0].allowed = vec![host("/host/work/public")];
                export.binds.push(Bind {
                    child: 0,
                    source: "private".to_owned(),
                    target: "/workspace/private".to_owned(),
                    read_only: false,
                    identity: None,
                });
            },
            "/workspace/private, but child 0 hides it",
        ),
        (
            |export| export.children.clear(),
            "it exports 0 directories and files",
        ),
        (
            // A read-only child whose denied path no process argument could carry.
            |export| deny(export, 1, &["/host/cache/secret\0suffix"]),
            r#"secret\0suffix", which contains a NUL character"#,
        ),
        (
            |export| export.children[1].root = host("/host/ca\0che"),
            "which contains a NUL character",
        ),
        (
            // A path that lies inside a child by its text, but not on the host.
            |export| deny(export, 0, &["/host/work/../cache/secret"]),
            "which has a `..` component",
        ),
        (
            |export| export.children[1].root = host("/host/work/../cache"),
            "which has a `..` component",
        ),
        (
            // The guest would write `other`, which no bind maps read-write, but not `out`.
            |export| {
                export.binds[0].read_only = true;
                export.children[0].write = vec![host("/host/work/other")];
                export.binds.push(nested(None));
            },
            "child 0 lets the guest write other paths than those that it maps read-write",
        ),
        (
            // Without write paths, the guest would write all of the child.
            |export| {
                export.binds[0].read_only = true;
                export.binds.push(nested(None));
            },
            "child 0 lets the guest write other paths than those that it maps read-write",
        ),
        (
            |export| {
                export.children[0].write = vec![host("/host/work/other")];
                export.binds.push(nested(None));
            },
            "which it maps read-write, stays read-only",
        ),
        (
            |export| {
                export.children[0].denied = vec![host("/host/work/secrets")];
                export.children[0].denied_identities = vec![None];
            },
            "child 0 does not pin the denied path",
        ),
        (
            |export| {
                export.children[0].write = vec![host("/host/work/out")];
                export.children[0].denied = vec![host("/host/work/out/secret")];
                export.children[0].denied_identities = vec![None];
            },
            "child 0 does not pin the denied path",
        ),
        (
            |export| export.binds.push(nested(None)),
            "it does not pin the object that it maps at /workspace/out, which a workload could \
             move",
        ),
    ] {
        let error = checked(change).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        let message = error.to_string();
        assert!(
            message.starts_with("the recorded export plan is invalid: ")
                && message.contains(expected),
            "{message} lacks {expected}"
        );
    }
    // On Windows, `plan` resolves paths on local drives, without the verbatim prefix.
    if cfg!(windows) {
        for change in [
            (|export: &mut ExportPlan| export.children[1].root = r"\\?\C:\host\cache".into())
                as fn(&mut ExportPlan),
            |export| export.children[1].root = r"\\server\share\cache".into(),
        ] {
            let message = checked(change).unwrap_err().to_string();
            assert!(
                message.ends_with("which is not a path on a local drive"),
                "{message}"
            );
        }
    }
}

#[test]
fn plans_require_properly_nested_disk_path_brackets() {
    let directory = tempfile::tempdir().unwrap();
    let path = |name: &str| native(directory.path().join(name).to_str().unwrap());
    let plain = path("plain.img");
    fs::write(&plain, b"image").unwrap();
    for (name, accepted) in [
        ("image[1.img", false),
        ("image]1.img", false),
        ("image][1.img", false),
        ("image[[1].img", false),
        ("image[1]].img", false),
        ("image.img", true),
        ("image[].img", true),
        ("image[1].img", true),
        ("image[[1][2]].img", true),
        ("image[1][2].img", true),
    ] {
        let image = path(name);
        fs::write(&image, b"image").unwrap();
        for plan in [
            SandboxPlan::adapter(
                SandboxDistro::new(&image, RootImage::gpt(None)),
                SandboxUpper::RAM,
            ),
            SandboxPlan::adapter(
                SandboxDistro::new(&plain, RootImage::gpt(None)),
                SandboxUpper::scratch(&image),
            ),
        ] {
            let result = plan.validated();
            if accepted {
                result.unwrap();
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), ErrorKind::Unsupported);
                assert_eq!(
                    error.to_string(),
                    format!(
                        "disk paths containing commas, semicolons, or unbalanced brackets are \
                         unsupported: {image}"
                    )
                );
            }
        }
    }
}

#[test]
fn plans_check_the_images_of_their_block_devices_against_the_host() {
    let directory = tempfile::tempdir().unwrap();
    let path = |name: &str| native(directory.path().join(name).to_str().unwrap());
    let plan = |name: &str, image: RootImage, upper: SandboxUpper| {
        SandboxPlan::new(
            SandboxDistro::new(&path(name), image),
            upper,
            SandboxSettings::default(),
        )
        .unwrap()
    };
    for (image, description) in [
        (RootImage::erofs(None), "distro EROFS layer"),
        (RootImage::gpt(Some(uuid(GPT_GUID))), "distro GPT image"),
    ] {
        let missing = plan("missing.img", image, SandboxUpper::RAM);
        let error = missing.validated().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unavailable);
        assert_eq!(
            error.to_string(),
            format!("{description} not found: {}", path("missing.img"))
        );
    }
    fs::write(path("distro.img"), b"image").unwrap();
    // A RAM upper has no scratch image to check.
    plan("distro.img", RootImage::gpt(None), SandboxUpper::RAM)
        .validated()
        .unwrap();
    let scratch = plan(
        "distro.img",
        RootImage::gpt(None),
        SandboxUpper::scratch(&path("scratch.ext4")),
    );
    assert_eq!(
        message(scratch.validated()),
        format!("ext4 scratch image not found: {}", path("scratch.ext4"))
    );
    fs::write(path("scratch.ext4"), b"scratch").unwrap();
    scratch.validated().unwrap();
    let delimited = path("distro,ro.img");
    fs::write(&delimited, b"image").unwrap();
    assert_eq!(
        message(plan("distro,ro.img", RootImage::gpt(None), SandboxUpper::RAM).validated()),
        format!(
            "disk paths containing commas, semicolons, or unbalanced brackets are unsupported: \
             {delimited}"
        )
    );
    // OpenVMM would read a container from a file with one of these extensions.
    for extension in ["vhd", "vhdx", "vmgs", "iso"] {
        let refusal = |name: &str| {
            format!(
                "disk paths ending in .{extension} are unsupported, since OpenVMM would not \
                 attach them as raw images: {}",
                path(name)
            )
        };
        let image = format!("distro.{extension}");
        fs::write(path(&image), b"image").unwrap();
        assert_eq!(
            message(plan(&image, RootImage::gpt(None), SandboxUpper::RAM).validated()),
            refusal(&image)
        );
        let scratch = format!("scratch.{extension}");
        fs::write(path(&scratch), b"scratch").unwrap();
        let upper = SandboxUpper::scratch(&path(&scratch));
        assert_eq!(
            message(plan("distro.img", RootImage::erofs(None), upper).validated()),
            refusal(&scratch)
        );
    }
    // OpenVMM reads the extension of the name alone, case-sensitively, and none of a name that
    // starts with its only dot.
    fs::create_dir(path("images.vhd")).unwrap();
    for name in ["raw.VHD", ".vhd", "distro.vhd.img", "images.vhd/distro.img"] {
        fs::write(path(name), b"image").unwrap();
        plan(name, RootImage::gpt(None), SandboxUpper::RAM)
            .validated()
            .unwrap();
    }
    let resolved = |name: &str| {
        paths::resolve(Path::new(&path(name)))
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    };
    // Windows resolves a name to the case of the file's own, which `provision` records.
    #[cfg(windows)]
    {
        assert_eq!(
            message(plan("distro.VHD", RootImage::gpt(None), SandboxUpper::RAM).validated()),
            format!(
                "disk paths ending in .vhd are unsupported, since OpenVMM would not attach them \
                 as raw images: {} resolves to {}",
                path("distro.VHD"),
                resolved("distro.VHD")
            )
        );
    }
    // A provisioned sandbox gives OpenVMM the paths to which its images' paths resolve, so a link
    // to a container's name is refused too, as the distro image or the scratch image.
    let linked = |link: &str, target: &str| {
        let (link, target) = (path(link), path(target));
        move |candidate: &Path| {
            if candidate.as_os_str() == link.as_str() {
                Ok(PathBuf::from(&target))
            } else {
                paths::resolve(candidate)
            }
        }
    };
    let refusal = |extension: &str, link: &str, target: &str| {
        format!(
            "disk paths ending in .{extension} are unsupported, since OpenVMM would not attach \
             them as raw images: {} resolves to {}",
            path(link),
            path(target)
        )
    };
    let resolve = linked("distro.img", "image.vhd");
    let host = mount::HostQueries {
        resolve: &resolve,
        ..mount::HostQueries::native()
    };
    let raw = plan("distro.img", RootImage::gpt(None), SandboxUpper::RAM);
    assert_eq!(
        message(raw.validated_with(&host)),
        refusal("vhd", "distro.img", "image.vhd")
    );
    let resolve = linked("scratch.ext4", "upper.vhdx");
    let host = mount::HostQueries {
        resolve: &resolve,
        ..mount::HostQueries::native()
    };
    let upper = SandboxUpper::scratch(&path("scratch.ext4"));
    let scratch = plan("distro.img", RootImage::erofs(None), upper);
    assert_eq!(
        message(scratch.validated_with(&host)),
        refusal("vhdx", "scratch.ext4", "upper.vhdx")
    );
    scratch.validated().unwrap();
    for (directory, accepted) in [
        ("images,raw", false),
        ("images;raw", false),
        ("images[1", false),
        ("images]1", false),
        ("images][1", false),
        ("images[[1][2]]", true),
    ] {
        for (link, candidate) in [("distro.img", &raw), ("scratch.ext4", &scratch)] {
            let target = format!("{directory}/{link}");
            let resolve = linked(link, &target);
            let host = mount::HostQueries {
                resolve: &resolve,
                ..mount::HostQueries::native()
            };
            let result = candidate.validated_with(&host);
            if accepted {
                result.unwrap();
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), ErrorKind::Unsupported);
                assert_eq!(
                    error.to_string(),
                    format!(
                        "disk paths containing commas, semicolons, or unbalanced brackets are \
                         unsupported: {} resolves to {}",
                        path(link),
                        path(&target)
                    )
                );
            }
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(path("distro.vhd"), path("link.img")).unwrap();
        assert_eq!(
            message(plan("link.img", RootImage::gpt(None), SandboxUpper::RAM).validated()),
            format!(
                "disk paths ending in .vhd are unsupported, since OpenVMM would not attach them \
                 as raw images: {} resolves to {}",
                path("link.img"),
                resolved("link.img")
            )
        );
    }
}

#[test]
fn launches_plan_their_distro_layer_over_their_scratch_image() {
    let launch = SandboxLaunch::new(
        vec![SandboxLayer::new(
            LayerRole::Distro,
            "distro.erofs",
            EROFS_UUID,
        )],
        SandboxUpper::scratch("scratch.ext4"),
        SandboxSettings::default(),
    )
    .unwrap();
    let plan = launch.plan();
    assert_eq!(plan.distro().path(), Path::new("distro.erofs"));
    assert_eq!(
        plan.distro().image(),
        RootImage::erofs(Some(uuid(EROFS_UUID)))
    );
    assert_eq!(plan.upper().kind(), RootUpper::Scratch);
    assert_eq!(plan.upper().scratch_path(), Some(Path::new("scratch.ext4")));
    assert_eq!(launch.scratch(), Some(Path::new("scratch.ext4")));
    assert_eq!(plan.settings(), Some(&SandboxSettings::default()));
    // The guest takes the UUID in its canonical text, which `--layer` writes, and nothing else.
    for invalid in ["", "not-a-uuid", "11111111-1111-1111-1111-11111111111A"] {
        let error = SandboxLaunch::new(
            vec![SandboxLayer::new(
                LayerRole::Distro,
                "distro.erofs",
                invalid,
            )],
            SandboxUpper::scratch("scratch.ext4"),
            SandboxSettings {
                entrypoint: "relative".to_owned(),
                ..SandboxSettings::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert_eq!(
            error.to_string(),
            format!("distro layer UUID is invalid: {invalid:?}")
        );
    }
}

#[test]
fn launches_plan_their_distro_layer_over_ram() {
    let launch = SandboxLaunch::new(
        vec![SandboxLayer::new(
            LayerRole::Distro,
            "distro.erofs",
            EROFS_UUID,
        )],
        SandboxUpper::RAM,
        SandboxSettings::default(),
    )
    .unwrap();
    assert_eq!(launch.upper(), &SandboxUpper::RAM);
    assert_eq!(launch.scratch(), None);
    let plan = launch.plan();
    assert_eq!(plan.upper().kind(), RootUpper::Ram);
    // The distro layer is the one block device, and the guest keeps the upper in RAM.
    assert_eq!(
        plan.blocks(),
        [(BlockRole::Distro, Path::new("distro.erofs"))]
    );
    let command_line = plan.kernel_command_line("").unwrap();
    let tokens: Vec<&str> = command_line.split(' ').collect();
    assert!(tokens.contains(&RAM_UPPER_TOKEN), "{command_line}");
    assert!(
        !tokens.iter().any(|token| token.starts_with(SCRATCH_TOKEN)),
        "{command_line}"
    );
}

#[test]
fn launches_plan_the_image_that_the_binding_of_their_distro_layer_records() {
    let directory = tempfile::tempdir().unwrap();
    let layer = directory.path().join("distro.erofs");
    fs::write(&layer, &b"layer"[..]).unwrap();
    let launch = SandboxLaunch::new(
        vec![SandboxLayer::new(
            LayerRole::Distro,
            layer.to_str().unwrap(),
            EROFS_UUID,
        )],
        SandboxUpper::scratch("scratch.ext4"),
        SandboxSettings::default(),
    )
    .unwrap();
    let binding = DistroBinding::verify(launch.distro()).unwrap();
    let bound = launch.bound_plan(&binding).unwrap();
    assert_eq!(
        bound.distro().image(),
        RootImage::erofs(Some(uuid(EROFS_UUID)))
    );
    assert_eq!(&bound, launch.plan());

    // The binding, not the `--layer` value, names the identity.
    let mut recorded = serde_json::to_value(&binding).unwrap();
    recorded["identity"] = json!(GPT_GUID);
    let other: DistroBinding = serde_json::from_value(recorded.clone()).unwrap();
    assert_eq!(
        launch.bound_plan(&other).unwrap().distro().image(),
        RootImage::erofs(Some(uuid(GPT_GUID)))
    );
    recorded["identity"] = json!("11111111-1111-1111-1111-11111111111A");
    let invalid: DistroBinding = serde_json::from_value(recorded).unwrap();
    assert_eq!(
        message(launch.bound_plan(&invalid)),
        "the distro binding names the identity \"11111111-1111-1111-1111-11111111111A\", which is \
         not a UUID in its canonical text"
    );
}

/// Returns a plan of every kind of setting, persisted as it records them.
fn persisted() -> (SandboxPlan, Value) {
    // The largest limits that the guest's cgroups apply, of which the memory limit exceeds the
    // integers that a JSON number holds exactly.
    let settings = SandboxSettings {
        entrypoint: "/bin/workload".to_owned(),
        args: vec!["--serve".to_owned()],
        hostname: "example".to_owned(),
        workload_identity: (1000, 1001),
        memory_max: Some(u64::MAX),
        pids_max: Some(4_194_303),
    };
    let mut export = exported(&[
        (
            "/workspace",
            "/host/work",
            MountAccess::ReadWrite,
            MountKind::Directory,
        ),
        (
            "/etc/app/settings.json",
            "/host/settings.json",
            MountAccess::ReadOnly,
            MountKind::File,
        ),
    ]);
    export.children[0].write = vec![host("/host/work/out")];
    deny(&mut export, 0, &["/host/work/secrets"]);
    export.children[0].allowed = vec![host("/host/work/secrets/public")];
    let plan = SandboxPlan::new(
        SandboxDistro::new("/images/distro.img", RootImage::gpt(Some(uuid(GPT_GUID)))),
        SandboxUpper::RAM,
        settings,
    )
    .and_then(|plan| plan.with_export(Some(export)))
    .unwrap()
    .with_network(Some(direct_network()));
    let path = native;
    let form = json!({
        "version": 3,
        "distro": {"path": path("/images/distro.img"), "format": "gpt", "identity": GPT_GUID},
        "upper": {"kind": "ram"},
        "settings": {
            "entrypoint": "/bin/workload",
            "args": ["--serve"],
            "hostname": "example",
            "workloadIdentity": {"uid": 1000, "gid": 1001},
            "memoryMax": "18446744073709551615",
            "pidsMax": "4194303",
        },
        "export": {
            "children": [
                {
                    "root": host_text("/host/work"),
                    "writable": true,
                    "denied": [host_text("/host/work/secrets")],
                    "deniedIdentities": [null],
                    "allowed": [host_text("/host/work/secrets/public")],
                    "write": [host_text("/host/work/out")],
                },
                {
                    "root": host_text("/host/settings.json"),
                    "file": true,
                    "writable": false,
                    "denied": [],
                    "deniedIdentities": [],
                    "allowed": [],
                    "write": [],
                },
            ],
            "binds": [
                {"child": 0, "source": "", "target": "/workspace", "readOnly": false},
                {"child": 1, "source": "", "target": "/etc/app/settings.json", "readOnly": true},
            ],
        },
        "network": direct_network_form(),
    });
    (plan, form)
}

/// Returns a network with every field of a policy that allows direct egress.
fn direct_network() -> SandboxNetwork {
    let mut policy = NetworkPolicy::new(Access::Allow)
        .with_allow(
            EgressRule {
                to: vec![Destination {
                    cidr: "192.0.2.0/24".to_owned(),
                    except: vec!["192.0.2.128/25".to_owned()],
                }],
                ports: Vec::new(),
            }
            .on_port_range(Protocol::Any, 8000, 8010),
        )
        .with_allow(EgressRule::default().on_port(Protocol::Tcp, 443))
        .with_deny(EgressRule::to("198.51.100.0/24").on_port(Protocol::Udp, 53))
        .with_deny(EgressRule::to("2001:db8::/32").on_protocol(Protocol::Icmp));
    policy.ingress = Access::Allow;
    policy.host_loopback = HostLoopback::Allow;
    policy.host_loopback_forwards = vec![LoopbackForward {
        protocol: ForwardProtocol::Tcp,
        host_port: 18080,
        guest_port: 8080,
    }];
    SandboxNetwork::new("10.0.0.2/24", policy).unwrap()
}

/// Returns the persisted form of [`direct_network`].
fn direct_network_form() -> Value {
    json!({
        "guestNetwork": "10.0.0.2/24",
        "egress": "allow",
        "allow": {
            "name": "egress.allow",
            "rules": [
                {
                    "to": [{"cidr": "192.0.2.0/24", "except": ["192.0.2.128/25"]}],
                    "ports": [{"protocol": "any", "port": 8000, "endPort": 8010}],
                },
                {"to": [], "ports": [{"protocol": "tcp", "port": 443, "endPort": null}]},
            ],
        },
        "deny": {
            "name": "egress.deny",
            "rules": [
                {
                    "to": [{"cidr": "198.51.100.0/24", "except": []}],
                    "ports": [{"protocol": "udp", "port": 53, "endPort": null}],
                },
                {
                    "to": [{"cidr": "2001:db8::/32", "except": []}],
                    "ports": [{"protocol": "icmp", "port": null, "endPort": null}],
                },
            ],
        },
        "ingress": "allow",
        "hostLoopback": "allow",
        "hostLoopbackForwards": [{"protocol": "tcp", "hostPort": 18080, "guestPort": 8080}],
        "proxy": null,
    })
}

/// Returns the network of a guest at `10.0.0.2/24` whose one way out is a proxy at port 3128 of
/// host loopback, with `scheme`.
fn proxy_network(scheme: Option<ProxyScheme>) -> SandboxNetwork {
    let proxy = NetworkProxy { port: 3128, scheme };
    SandboxNetwork::new("10.0.0.2/24", NetworkPolicy::proxy_only(proxy)).unwrap()
}

#[test]
fn plans_persist_in_their_versioned_form() {
    let (plan, form) = persisted();
    assert_eq!(serde_json::to_value(&plan).unwrap(), form);
    assert_eq!(serde_json::from_value::<SandboxPlan>(form).unwrap(), plan);
    // Every combination round-trips, and renders alike after it does.
    for image in images() {
        for upper in uppers() {
            for network in [None, Some(direct_network()), Some(proxy_network(None))] {
                let plan = plan_with(image, upper.clone()).with_network(network);
                let read: SandboxPlan =
                    serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
                assert_eq!(read, plan);
                assert_eq!(
                    read.kernel_command_line("").unwrap(),
                    plan.kernel_command_line("").unwrap()
                );
                assert_eq!(
                    read.openvmm_arguments().unwrap(),
                    plan.openvmm_arguments().unwrap()
                );
                assert_eq!(read.proxy_url(), plan.proxy_url());
            }
        }
    }
    let scratch = serde_json::to_value(plan_with(images()[1], uppers()[0].clone())).unwrap();
    assert_eq!(
        scratch["upper"],
        json!({"kind": "scratch", "path": native("scratch.ext4")})
    );
    assert_eq!(scratch["distro"]["identity"], Value::Null);
    assert_eq!(scratch["export"], Value::Null);
    assert_eq!(scratch["network"], Value::Null);
    let proxy = serde_json::to_value(proxy_network(Some(ProxyScheme::Https))).unwrap();
    assert_eq!(
        proxy,
        json!({
            "guestNetwork": "10.0.0.2/24",
            "egress": "deny",
            "allow": {"name": "egress.allow", "rules": []},
            "deny": {"name": "egress.deny", "rules": []},
            "ingress": "deny",
            "hostLoopback": "deny",
            "hostLoopbackForwards": [],
            "proxy": {"port": 3128, "scheme": "https"},
        })
    );
    assert_eq!(PLAN_VERSION, 3);
}

fn plan_with(image: RootImage, upper: SandboxUpper) -> SandboxPlan {
    plan(image, upper, SandboxSettings::default())
}

/// Returns the plan of an adapter's own guest of `image` over `upper`.
fn adapter(image: RootImage, upper: SandboxUpper) -> SandboxPlan {
    SandboxPlan::adapter(SandboxDistro::new("images/distro.img", image), upper)
}

#[test]
fn adapter_plans_render_their_root_and_devices_alone() {
    // The adapter's own tokens, `nvx_*` ones among them, come first, then the root's.
    let user = "console=hvc0 nvx_map=c291cmNl,dGFyZ2V0,ro nvx_control_tty=hvc2 nvx_sandbox=1";
    for image in images() {
        for upper in uppers() {
            let plan = adapter(image, upper.clone());
            assert_eq!(plan.settings(), None);
            assert_eq!(plan.export(), None);
            let [layer, upper_token] = plan.layout().tokens();
            assert_eq!(
                plan.kernel_command_line(user).unwrap(),
                format!("{user} {layer} {upper_token}")
            );
            assert_eq!(
                plan.kernel_command_line(" \t").unwrap(),
                format!("{layer} {upper_token}")
            );
            // No workload identity and no shares: the block devices alone.
            let mut expected = vec![
                "--machine".to_owned(),
                "microvm".to_owned(),
                "--microvm-sandbox-block".to_owned(),
                format!("distro:file:{},ro", native("images/distro.img")),
            ];
            if let Some(scratch) = upper.scratch_path() {
                expected.extend([
                    "--microvm-sandbox-block".to_owned(),
                    format!("scratch:file:{}", scratch.display()),
                ]);
            }
            assert_eq!(plan.openvmm_arguments().unwrap(), expected);
            // Then the network's, as for the sandbox profile.
            let network = proxy_network(None);
            expected.extend(network.openvmm_arguments().unwrap());
            let plan = plan.with_network(Some(network));
            assert_eq!(plan.openvmm_arguments().unwrap(), expected);
            assert_eq!(
                plan.proxy_url().as_deref(),
                Some("10.0.0.1:3128"),
                "{plan:?}"
            );
        }
    }
}

#[test]
fn adapter_plans_refuse_what_their_root_and_openvmm_own() {
    let plan = adapter(RootImage::gpt(Some(uuid(GPT_GUID))), SandboxUpper::RAM);
    for (user, name, owner) in [
        (
            "nvx_layer=distro,0xd0003000,gpt",
            "nvx_layer",
            "the plan's root",
        ),
        ("nvx_layer", "nvx_layer", "the plan's root"),
        (
            "quiet nvx_overlay_upper=ramfs",
            "nvx_overlay_upper",
            "the plan's root",
        ),
        (
            "nvx_scratch=0xd0006000,ext4",
            "nvx_scratch",
            "the plan's root",
        ),
        ("virtfs_tag=microvm", "virtfs_tag", "OpenVMM's shares"),
    ] {
        let error = plan.kernel_command_line(user).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{user}");
        assert_eq!(
            error.to_string(),
            format!("{name} is owned by {owner}"),
            "{user}"
        );
    }
    assert_eq!(
        message(plan.kernel_command_line("quiet\0")),
        "kernel command line contains an embedded NUL"
    );
    // The guest must read the plan's root from the command line.
    assert_eq!(
        message(plan.kernel_command_line("virtio_mmio.device=0x1000@0xd0006000:11")),
        "the kernel command line of an adapter's plan conflicts with the boot tokens of its root: \
         a RAM upper takes no scratch block device, but the command line adds a virtio-mmio \
         device at 0xd0006000"
    );
    // The adapter's command line, a space, the root's tokens, and a NUL fill the budget exactly.
    for image in images() {
        for upper in uppers() {
            let plan = adapter(image, upper);
            let base = plan.kernel_command_line("").unwrap();
            let fitting = "x".repeat(SANDBOX_COMMAND_LINE_MAX_SIZE - base.len() - 2);
            let command_line = plan.kernel_command_line(&fitting).unwrap();
            assert_eq!(command_line.len() + 1, SANDBOX_COMMAND_LINE_MAX_SIZE);
            assert_eq!(
                message(plan.kernel_command_line(&format!("{fitting}x"))),
                "the kernel command line of an adapter's plan exceeds its 1024-byte x86 budget"
            );
        }
    }
    // The adapter attaches its shares itself.
    assert_eq!(
        message(plan.clone().with_export(Some(shares(true)))),
        "an adapter's plan holds no export plan: the adapter attaches its shares itself"
    );
    assert_eq!(plan.clone().with_export(None).unwrap(), plan);
}

#[test]
fn plans_refuse_the_tokens_that_the_guest_init_reads_in_quoted_values() {
    // The kernel reads `x` with a quoted value, but the guest's init splits the command line at
    // spaces, tabs, and newlines without quoting, and reads the tokens in the value.
    let sandbox_plan = plan(images()[0], SandboxUpper::RAM, SandboxSettings::default());
    let adapter_plan = adapter(images()[2], SandboxUpper::RAM);
    for (plan, user, refusal) in [
        (
            &sandbox_plan,
            "x=\"y nvx_entrypoint=/bin/other\"",
            "nvx_entrypoint is owned by the sandbox profile",
        ),
        (
            &sandbox_plan,
            "x=\"y\tvirtfs_tag=other\"",
            "virtfs_tag is owned by the sandbox --mount option",
        ),
        (
            &adapter_plan,
            "x=\"y nvx_layer=distro,0xd0003000,gpt\"",
            "nvx_layer is owned by the plan's root",
        ),
        (
            &adapter_plan,
            "x=\"y\nvirtfs_tag=microvm\"",
            "virtfs_tag is owned by OpenVMM's shares",
        ),
    ] {
        let error = plan.kernel_command_line(user).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{user:?}");
        assert_eq!(error.to_string(), refusal, "{user:?}");
    }
}

#[test]
fn adapter_plans_persist_without_settings_or_an_export() {
    let plan = SandboxPlan::adapter(
        SandboxDistro::new("/images/distro.img", RootImage::gpt(Some(uuid(GPT_GUID)))),
        SandboxUpper::RAM,
    )
    .with_network(Some(proxy_network(Some(ProxyScheme::Http))));
    let form = serde_json::to_value(&plan).unwrap();
    assert_eq!(form["version"], json!(PLAN_VERSION));
    assert_eq!(form["settings"], Value::Null);
    assert_eq!(form["export"], Value::Null);
    let read: SandboxPlan = serde_json::from_value(form.clone()).unwrap();
    assert_eq!(read, plan);
    assert_eq!(
        read.kernel_command_line("quiet").unwrap(),
        plan.kernel_command_line("quiet").unwrap()
    );
    assert_eq!(
        read.openvmm_arguments().unwrap(),
        plan.openvmm_arguments().unwrap()
    );
    // An adapter's plan never holds an export plan, and its settings are required as `null`.
    let (_, sandbox) = persisted();
    let mut exporting = form.clone();
    exporting["export"] = sandbox["export"].clone();
    let error = serde_json::from_value::<SandboxPlan>(exporting)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("an adapter's plan holds no export plan"),
        "{error}"
    );
    let mut missing = form;
    missing.as_object_mut().unwrap().remove("settings");
    let error = serde_json::from_value::<SandboxPlan>(missing)
        .unwrap_err()
        .to_string();
    assert!(error.contains("missing field `settings`"), "{error}");
}

#[test]
fn persisted_plans_that_break_the_form_or_the_checks_are_refused() {
    let (_, form) = persisted();
    let refused = |change: &dyn Fn(&mut Value), expected: &str| {
        let mut value = form.clone();
        change(&mut value);
        let error = serde_json::from_value::<SandboxPlan>(value)
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{error} lacks {expected}");
    };
    // Every field is required, nulls included, and no other is accepted.
    for pointer in [
        "/version",
        "/distro",
        "/distro/path",
        "/distro/format",
        "/distro/identity",
        "/upper",
        "/upper/kind",
        "/settings",
        "/settings/entrypoint",
        "/settings/args",
        "/settings/hostname",
        "/settings/workloadIdentity",
        "/settings/workloadIdentity/uid",
        "/settings/memoryMax",
        "/settings/pidsMax",
        "/export",
        "/export/children",
        "/export/children/0/root",
        "/export/children/0/deniedIdentities",
        "/export/binds",
        "/export/binds/0/target",
        "/export/binds/1/readOnly",
        "/network",
        "/network/guestNetwork",
        "/network/egress",
        "/network/allow",
        "/network/allow/name",
        "/network/allow/rules",
        "/network/allow/rules/0/to",
        "/network/allow/rules/0/to/0/except",
        "/network/allow/rules/0/ports",
        "/network/allow/rules/0/ports/0/protocol",
        "/network/deny/rules/0/ports/0/endPort",
        "/network/deny",
        "/network/ingress",
        "/network/hostLoopback",
        "/network/hostLoopbackForwards",
        "/network/hostLoopbackForwards/0/guestPort",
        "/network/proxy",
    ] {
        let (parent, name) = pointer.rsplit_once('/').unwrap();
        refused(
            &|value| {
                value
                    .pointer_mut(parent)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .remove(name)
                    .unwrap();
            },
            "missing field",
        );
    }
    for pointer in [
        "",
        "/distro",
        "/upper",
        "/settings",
        "/settings/workloadIdentity",
        "/export",
        "/export/children/0",
        "/export/binds/0",
        "/network",
        "/network/allow",
        "/network/allow/rules/0",
        "/network/allow/rules/0/to/0",
        "/network/allow/rules/0/ports/0",
        "/network/hostLoopbackForwards/0",
    ] {
        refused(
            &|value| {
                value.pointer_mut(pointer).unwrap()["origin"] = json!("elsewhere");
            },
            "unknown field",
        );
    }
    // A RAM upper has no scratch image.
    refused(
        &|value| value["upper"]["path"] = json!("scratch.ext4"),
        "unknown field `path`",
    );
    refused(
        &|value| value["upper"] = json!({"kind": "scratch"}),
        "missing field `path`",
    );
    refused(
        &|value| value["upper"] = json!({"kind": "tmpfs"}),
        "unknown variant",
    );
    refused(&|value| value["upper"] = json!("ram"), "invalid type");
    refused(
        &|value| value["distro"]["format"] = json!("ext4"),
        "unknown variant",
    );
    for version in [1, 2, 4] {
        refused(
            &|value| value["version"] = json!(version),
            &format!("the sandbox plan has version {version}, which this build does not read"),
        );
    }
    // The forms of versions 1 and 2 held the shares as requested, and that of version 1 had no
    // network: neither is read.
    refused(
        &|value| {
            value["version"] = json!(1);
            value.as_object_mut().unwrap().remove("network");
        },
        "missing field `network`",
    );
    refused(
        &|value| {
            value["version"] = json!(2);
            value["settings"]["mounts"] = json!([]);
        },
        "unknown field `mounts`",
    );
    for identity in ["", "FEDCBA98-7654-3210-FEDC-BA9876543210", "{fedcba98}"] {
        refused(
            &|value| value["distro"]["identity"] = json!(identity),
            &format!(
                "the distro identity of the sandbox plan, {identity:?}, is not a UUID in its \
                 canonical text"
            ),
        );
    }
    for limit in ["", "0", "01", "+1", "1 ", "-1", "1_000", "\u{0661}", "0x10"] {
        refused(
            &|value| value["settings"]["memoryMax"] = json!(limit),
            "the memoryMax of the sandbox plan",
        );
        refused(
            &|value| value["settings"]["pidsMax"] = json!(limit),
            "is not a positive integer in decimal text",
        );
    }
    refused(
        &|value| value["settings"]["memoryMax"] = json!(268_435_456),
        "invalid type",
    );
    // What the plan and its shares check, reading checks too.
    refused(
        &|value| value["settings"]["memoryMax"] = json!("18446744073709551616"),
        "is not a positive integer in decimal text",
    );
    refused(
        &|value| value["settings"]["pidsMax"] = json!("4194304"),
        "--pids-max must be at most 4194303",
    );
    refused(
        &|value| value["settings"]["workloadIdentity"]["uid"] = json!(0),
        "sandbox workload identity must be a non-root UID:GID",
    );
    refused(
        &|value| value["settings"]["entrypoint"] = json!("bin/sh"),
        "sandbox entrypoint must be an absolute path without whitespace, control characters, or quotes",
    );
    refused(
        &|value| value["export"]["binds"][1]["target"] = json!("/workspace/settings.json"),
        "would hide part of that path",
    );
    refused(
        &|value| value["export"]["binds"][0]["target"] = json!("/etc"),
        "its guest target /etc would cover the guest's /etc",
    );
    refused(
        &|value| value["export"]["owner"] = json!("root"),
        "unknown variant `root`",
    );
    refused(
        &|value| value["export"]["children"] = json!([]),
        "the recorded export plan is invalid",
    );
    // The network's values, and what its policy checks.
    refused(
        &|value| value["network"]["egress"] = json!("block"),
        "unknown variant",
    );
    refused(
        &|value| value["network"]["hostLoopback"] = json!(true),
        "invalid type",
    );
    refused(
        &|value| value["network"]["allow"]["rules"][0]["ports"][0]["protocol"] = json!("sctp"),
        "unknown variant",
    );
    refused(
        &|value| value["network"]["allow"]["rules"][0]["ports"][0]["port"] = json!(65536),
        "invalid value",
    );
    refused(
        &|value| value["network"]["hostLoopbackForwards"][0]["protocol"] = json!("icmp"),
        "unknown variant",
    );
    refused(
        &|value| value["network"]["guestNetwork"] = json!("10.0.0.1/24"),
        "guest network \"10.0.0.1/24\" must be an IPv4 address with a /1 to /30 prefix that is not \
         its network's network, broadcast, or gateway (first) address",
    );
    refused(
        &|value| value["network"]["allow"]["rules"][0]["to"][0]["cidr"] = json!("example.com"),
        "egress.allow[0].to[0].cidr is not a valid IPv4 or IPv6 CIDR: \"example.com\"",
    );
    refused(
        &|value| value["network"]["hostLoopback"] = json!("outbound"),
        "host-loopback forwards need host-loopback access allowed",
    );
    // A proxy is the one way out of its network, and its scheme is http or https.
    refused(
        &|value| value["network"]["proxy"] = json!({"port": 3128, "scheme": null}),
        "a network proxy needs egress denied by default",
    );
    let proxy_only = serde_json::to_value(proxy_network(None)).unwrap();
    refused(
        &|value| {
            value["network"] = proxy_only.clone();
            value["network"]["proxy"]["scheme"] = json!("ftp");
        },
        "network proxy scheme \"ftp\" must be http or https",
    );
    refused(
        &|value| {
            value["network"] = proxy_only.clone();
            value["network"]["proxy"]["port"] = json!(0);
        },
        "a network proxy needs a port between 1 and 65535",
    );
    refused(
        &|value| {
            value["network"] = proxy_only.clone();
            value["network"]["proxy"]["port"] = json!(53);
        },
        "a network proxy needs a port other than 53",
    );
    refused(
        &|value| {
            value["network"] = proxy_only.clone();
            value["network"]["proxy"]
                .as_object_mut()
                .unwrap()
                .remove("scheme");
        },
        "missing field `scheme`",
    );
    refused(
        &|value| {
            value["network"] = proxy_only.clone();
            value["network"]["proxy"]["origin"] = json!("elsewhere");
        },
        "unknown field",
    );
}

#[test]
fn plans_render_and_report_their_network() {
    let without = exporting(
        images()[0],
        uppers()[0].clone(),
        SandboxSettings::default(),
        Some(shares(true)),
    );
    let shares = without.openvmm_arguments().unwrap();
    assert!(without.network().is_none());
    assert_eq!(without.proxy_url(), None);

    // The network's options follow the plan's other options.
    for network in [direct_network(), proxy_network(None)] {
        let plan = without.clone().with_network(Some(network.clone()));
        let mut expected = shares.clone();
        expected.extend(network.openvmm_arguments().unwrap());
        assert_eq!(plan.openvmm_arguments().unwrap(), expected);
        assert_eq!(plan.network(), Some(&network));
        // The network adds no boot token.
        assert_eq!(
            plan.kernel_command_line("quiet").unwrap(),
            without.kernel_command_line("quiet").unwrap()
        );
    }
    let direct = without.clone().with_network(Some(direct_network()));
    let arguments = direct.openvmm_arguments().unwrap();
    assert_eq!(
        arguments[shares.len()..],
        [
            "--net",
            "10.0.0.2/24",
            "--network-profile",
            "portable",
            "--network-egress",
            "allow",
            "--network-ingress",
            "allow",
            "--host-loopback",
            "allow",
            "--network-egress-allow",
            "0.0.0.0/0:tcp:443",
            "--network-egress-allow",
            "192.0.2.0/25:tcp:8000-8010",
            "--network-egress-allow",
            "192.0.2.0/25:udp:8000-8010",
            "--network-egress-allow",
            "::/0:tcp:443",
            "--network-egress-deny",
            "198.51.100.0/24:udp:53",
            "--network-egress-deny",
            "2001:db8::/32:icmp",
            "--host-loopback-forward",
            "tcp:18080:8080",
        ]
    );
    assert_eq!(direct.proxy_url(), None);

    // A proxy-only network reaches its proxy at the gateway, which its URL names.
    for (scheme, url) in [
        (None, "10.0.0.1:3128"),
        (Some(ProxyScheme::Http), "http://10.0.0.1:3128"),
        (Some(ProxyScheme::Https), "https://10.0.0.1:3128"),
    ] {
        let network = proxy_network(scheme);
        assert_eq!(network.gateway(), std::net::Ipv4Addr::new(10, 0, 0, 1));
        assert_eq!(network.proxy_url().as_deref(), Some(url));
        let plan = without.clone().with_network(Some(network));
        assert_eq!(plan.proxy_url().as_deref(), Some(url));
        assert_eq!(
            plan.openvmm_arguments().unwrap()[shares.len()..],
            [
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-ingress",
                "deny",
                "--host-loopback",
                "deny",
                "--network-proxy",
                "10.0.0.1:3128",
            ]
        );
    }
    let narrow = SandboxNetwork::new(
        "192.168.7.6/30",
        NetworkPolicy::proxy_only(NetworkProxy::new(8080).with_scheme(ProxyScheme::Http)),
    )
    .unwrap();
    assert_eq!(
        narrow.proxy_url().as_deref(),
        Some("http://192.168.7.5:8080")
    );

    // A policy that lets nothing through attaches no network device.
    let isolated = SandboxNetwork::new("10.0.0.2/24", NetworkPolicy::new(Access::Deny)).unwrap();
    assert!(isolated.openvmm_arguments().unwrap().is_empty());
    assert_eq!(
        without
            .clone()
            .with_network(Some(isolated))
            .openvmm_arguments()
            .unwrap(),
        shares
    );
}

#[test]
fn networks_check_their_guest_network_and_their_policy() {
    for guest_network in [
        "10.0.0.1/24",
        "10.0.0.0/24",
        "10.0.0.2/31",
        "10.0.0.2",
        "fd00::2/64",
    ] {
        let error =
            SandboxNetwork::new(guest_network, NetworkPolicy::new(Access::Allow)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert_eq!(
            error.to_string(),
            format!(
                "guest network {guest_network:?} must be an IPv4 address with a /1 to /30 prefix \
                 that is not its network's network, broadcast, or gateway (first) address"
            )
        );
    }
    // The policy must lower: each way out beside a proxy is refused before launch.
    let mut policy = NetworkPolicy::proxy_only(NetworkProxy::new(3128));
    policy.ingress = Access::Allow;
    let error = SandboxNetwork::new("10.0.0.2/24", policy).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert_eq!(error.to_string(), "a network proxy needs ingress denied");
    let mut policy = NetworkPolicy::new(Access::Allow);
    policy.host_loopback = HostLoopback::Allow;
    assert_eq!(
        message(SandboxNetwork::new("10.0.0.2/30", policy)),
        "host-loopback access needs a guest network prefix of /29 or shorter, which leaves an \
         address at which OpenVMM presents host peers to the guest"
    );
}

#[test]
fn launches_carry_their_network_into_their_plans() {
    let directory = tempfile::tempdir().unwrap();
    let layer = directory.path().join("distro.erofs");
    fs::write(&layer, b"layer").unwrap();
    let launch = SandboxLaunch::new(
        vec![SandboxLayer::new(
            LayerRole::Distro,
            layer.to_str().unwrap(),
            EROFS_UUID,
        )],
        SandboxUpper::scratch("scratch.ext4"),
        SandboxSettings::default(),
    )
    .unwrap();
    assert!(launch.network().is_none());
    let launch = launch.with_network(Some(proxy_network(Some(ProxyScheme::Http))));
    assert_eq!(
        launch.network(),
        Some(&proxy_network(Some(ProxyScheme::Http)))
    );
    assert_eq!(launch.plan().network(), launch.network());
    // The plan of the binding keeps the network.
    let binding = DistroBinding::verify(launch.distro()).unwrap();
    let bound = launch.bound_plan(&binding).unwrap();
    assert_eq!(bound.network(), launch.network());
    assert_eq!(bound.proxy_url().as_deref(), Some("http://10.0.0.1:3128"));
    assert!(launch.with_network(None).plan().network().is_none());
}
