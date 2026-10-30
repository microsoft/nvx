//! Tests of prepared launches: the checked files that they hold, the OpenVMM arguments that name
//! them, the descriptors that the VMM inherits, and the path from staging to launch, which reads
//! no content.

use std::fmt;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::time::Instant;

use sha2::{Digest, Sha256};

use super::*;
use crate::ErrorKind;
use crate::host::paths;
use crate::host::sandbox::distro::{self, DISTRO_POLICY_REVISION, DistroBinding, PolicyVerdict};
use crate::host::sandbox::scratch::{self, ScratchBinding};
use crate::host::sandbox::{
    LayerRole, SandboxDistro, SandboxLaunch, SandboxLayer, SandboxSettings, SandboxUpper,
};
use crate::host::seal::{FileSeal, open_metadata_only};
#[cfg(windows)]
use crate::host::test_support::junction;
use crate::protocol::root::RootImage;
use crate::uuid::Uuid;

/// The UUID of the staged layers.
const UUID: &str = "11111111-1111-1111-1111-111111111111";
/// Another UUID.
const OTHER_UUID: &str = "22222222-2222-2222-2222-222222222222";
/// The user's kernel command line of the launches.
const USER_COMMAND_LINE: &str = "console=ttyS0";

/// The access of a host that lets distro bindings open files and read their metadata, but fails
/// every read and mapping of their content.
const METADATA_ONLY: distro::Access<'static> = distro::Access {
    open: &open_metadata_only,
    seal: &FileSeal::of,
};

/// The access of a host that lets scratch bindings open the image for writing alone, so that no
/// read or mapping of its content can succeed through what they open: on Linux an open for writing
/// alone, and on Windows the access of binds and checks, which never ask to read.
#[cfg(target_os = "linux")]
const WRITE_ONLY: scratch::Access<'static> = scratch::Access {
    open: &open_write_only,
};
/// The access of a host that lets scratch bindings open the image for writing alone, so that no
/// read or mapping of its content can succeed through what they open: on Linux an open for writing
/// alone, and on Windows the access of binds and checks, which never ask to read.
#[cfg(windows)]
const WRITE_ONLY: scratch::Access<'static> = scratch::Access::WRITABLE;

/// Opens `path` for writing alone.
#[cfg(target_os = "linux")]
fn open_write_only(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new().write(true).open(path)
}

/// A distro layer with its manifest, and scratch images, staged in a temporary directory.
struct Staged {
    /// The temporary directory.
    directory: tempfile::TempDir,
}

impl Staged {
    /// Stages a distro layer with a manifest that binds it, another copy of the layer, and two
    /// scratch images.
    fn new() -> Self {
        let staged = Self {
            directory: tempfile::tempdir().unwrap(),
        };
        let layer = b"distro layer\n";
        fs::write(staged.path("distro.erofs"), layer).unwrap();
        fs::write(staged.path("other.erofs"), layer).unwrap();
        let digest: String = Sha256::digest(layer)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        fs::write(
            staged.path("distro.erofs.manifest.json"),
            format!(
                "{{\"format\": 1, \"artifact\": \"distro.erofs\", \"artifact_sha256\": \
                 \"{digest}\", \"packages\": [{{\"name\": \"bash\"}}]}}"
            ),
        )
        .unwrap();
        fs::write(staged.path("scratch.ext4"), b"scratch image\n").unwrap();
        fs::write(staged.path("other.ext4"), b"scratch image\n").unwrap();
        staged
    }

    /// Returns the path of the file `name` of the directory.
    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    /// Returns the text of the path of the file `name`, normalized.
    fn text(&self, name: &str) -> String {
        let path = paths::normalized(&self.path(name));
        path.to_str().unwrap().to_owned()
    }

    /// Returns the distro layer `name`.
    fn layer(&self, name: &str) -> SandboxLayer {
        SandboxLayer::new(LayerRole::Distro, &self.text(name), UUID)
    }

    /// Returns the launch of the layer `distro.erofs` over the image `scratch.ext4`.
    fn launch(&self) -> SandboxLaunch {
        SandboxLaunch::new(
            vec![self.layer("distro.erofs")],
            SandboxUpper::scratch(&self.text("scratch.ext4")),
            SandboxSettings::default(),
        )
        .unwrap()
    }

    /// Verifies the layer `name`, checks it, and returns its binding and the checked layer.
    fn checked_distro(&self, name: &str) -> (DistroBinding, CheckedDistro) {
        let layer = self.layer(name);
        let binding = DistroBinding::verify(&layer).unwrap();
        let checked = binding.check(&layer, DISTRO_POLICY_REVISION).unwrap();
        (binding, checked)
    }

    /// Binds the scratch image `name`, and returns it checked and claimed. Like a launch, it waits
    /// [`CLAIM_RELEASE_WAIT`](scratch::CLAIM_RELEASE_WAIT) for a claim that was released: on
    /// Linux, a process that another test spawns holds a copy of the claim until it executes its
    /// program.
    fn checked_scratch(&self, name: &str) -> CheckedScratch {
        let path = self.path(name);
        ScratchBinding::bind(&path)
            .unwrap()
            .check(&path, Instant::now() + scratch::CLAIM_RELEASE_WAIT)
            .unwrap()
    }
}

/// Returns the kind and the message of the error of `result`.
fn failure<T: fmt::Debug>(result: Result<T>) -> (ErrorKind, String) {
    let error = result.unwrap_err();
    (error.kind(), error.to_string())
}

/// Returns the value of each `option` of `arguments`, in order.
fn values<'a>(arguments: &'a [String], option: &str) -> Vec<&'a str> {
    arguments
        .windows(2)
        .filter(|pair| pair[0] == option)
        .map(|pair| pair[1].as_str())
        .collect()
}

/// Returns the number of the descriptor of `file`.
#[cfg(target_os = "linux")]
fn number(file: &fs::File) -> i32 {
    use std::os::fd::AsRawFd;

    file.as_raw_fd()
}

/// Returns the final path of the staged file `name`, by which a prepared launch names it on
/// Windows.
#[cfg(windows)]
fn final_path(staged: &Staged, name: &str) -> String {
    fs::canonicalize(staged.path(name))
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned()
}

/// Returns the value of the handle of `object`.
#[cfg(windows)]
fn raw(object: &impl std::os::windows::io::AsHandle) -> usize {
    use std::os::windows::io::AsRawHandle;

    object.as_handle().as_raw_handle() as usize
}

#[test]
fn prepared_launches_hand_the_vmm_the_checked_files() {
    let staged = Staged::new();
    let launch = staged.launch();
    let (binding, distro) = staged.checked_distro("distro.erofs");
    let scratch = staged.checked_scratch("scratch.ext4");
    let plan = launch.bound_plan(&binding).unwrap();
    let rendered = plan.openvmm_arguments().unwrap();
    #[cfg(target_os = "linux")]
    let (distro_number, scratch_number) = (number(distro.file()), number(scratch.file()));
    let prepared =
        PreparedLaunch::new(plan.clone(), distro, Some(scratch), USER_COMMAND_LINE).unwrap();
    assert_eq!(prepared.plan(), &plan);
    assert_eq!(
        prepared.kernel_command_line(),
        plan.kernel_command_line(USER_COMMAND_LINE).unwrap()
    );
    assert_eq!(
        prepared.distro().path().to_str().unwrap(),
        staged.text("distro.erofs")
    );
    assert_eq!(
        prepared.scratch().unwrap().path().to_str().unwrap(),
        staged.text("scratch.ext4")
    );

    // On Linux, the arguments name the checked files as descriptors that the VMM inherits, which
    // reach them; on Windows, by their final paths, which the checked files hold in place.
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::{AsFd, AsRawFd};

        let expected: Vec<String> = rendered
            .iter()
            .map(|argument| {
                argument
                    .replace(
                        &format!("distro:file:{},ro", staged.text("distro.erofs")),
                        &format!("distro:file:/proc/self/fd/{distro_number},ro"),
                    )
                    .replace(
                        &format!("scratch:file:{}", staged.text("scratch.ext4")),
                        &format!("scratch:file:/proc/self/fd/{scratch_number}"),
                    )
            })
            .collect();
        assert_ne!(expected, rendered);
        assert_eq!(prepared.openvmm_arguments(), expected);
        let inherited: Vec<i32> = prepared
            .inherited()
            .iter()
            .map(|inherited| inherited.as_fd().as_raw_fd())
            .collect();
        assert_eq!(inherited, [distro_number, scratch_number]);
        for (number, name) in [
            (distro_number, "distro.erofs"),
            (scratch_number, "scratch.ext4"),
        ] {
            assert_eq!(
                fs::read_link(format!("/proc/self/fd/{number}")).unwrap(),
                fs::canonicalize(staged.path(name)).unwrap()
            );
        }
    }
    #[cfg(windows)]
    {
        let expected: Vec<String> = rendered
            .iter()
            .map(|argument| {
                argument
                    .replace(
                        &format!("distro:file:{},ro", staged.text("distro.erofs")),
                        &format!("distro:file:{},ro", final_path(&staged, "distro.erofs")),
                    )
                    .replace(
                        &format!("scratch:file:{}", staged.text("scratch.ext4")),
                        &format!("scratch:file:{}", final_path(&staged, "scratch.ext4")),
                    )
            })
            .collect();
        assert_ne!(expected, rendered);
        assert_eq!(prepared.openvmm_arguments(), expected);
        // The VMM inherits the checked files, which hold their final paths for it, and the
        // claim's handle of the scratch image, which no argument names, and which is not its
        // guard.
        let inherited: Vec<usize> = prepared.inherited().iter().map(raw).collect();
        let scratch = prepared.scratch().unwrap();
        assert_eq!(
            inherited,
            [
                raw(prepared.distro().file()),
                raw(scratch.file()),
                raw(scratch.claim())
            ]
        );
        assert_ne!(raw(scratch.claim()), raw(scratch.file()));
    }

    // The plan, which callers persist, keeps the real paths.
    let persisted = serde_json::to_value(prepared.plan()).unwrap();
    assert_eq!(persisted["distro"]["path"], staged.text("distro.erofs"));
    assert_eq!(persisted["upper"]["path"], staged.text("scratch.ext4"));
}

#[test]
fn an_upper_in_ram_takes_no_scratch_image() {
    let staged = Staged::new();
    let (_, distro) = staged.checked_distro("distro.erofs");
    #[cfg(target_os = "linux")]
    let distro_number = number(distro.file());
    let plan = SandboxPlan::new(
        SandboxDistro::new(
            &staged.text("distro.erofs"),
            RootImage::erofs(Uuid::parse(UUID)),
        ),
        SandboxUpper::RAM,
        SandboxSettings::default(),
    )
    .unwrap();
    let prepared = PreparedLaunch::new(plan.clone(), distro, None, "").unwrap();
    assert!(prepared.scratch().is_none());
    // The VMM inherits the checked layer alone.
    assert_eq!(prepared.inherited().len(), 1);
    let blocks = values(prepared.openvmm_arguments(), "--microvm-sandbox-block");
    #[cfg(target_os = "linux")]
    assert_eq!(
        blocks,
        [format!("distro:file:/proc/self/fd/{distro_number},ro")]
    );
    #[cfg(windows)]
    {
        assert_eq!(
            blocks,
            [format!(
                "distro:file:{},ro",
                final_path(&staged, "distro.erofs")
            )]
        );
        let inherited: Vec<usize> = prepared.inherited().iter().map(raw).collect();
        assert_eq!(inherited, [raw(prepared.distro().file())]);
    }
    assert_eq!(
        prepared.kernel_command_line(),
        plan.kernel_command_line("").unwrap()
    );
}

#[test]
fn gpt_disks_are_prepared_from_their_bindings() {
    let staged = Staged::new();
    fs::write(staged.path("distro.img"), b"gpt disk\n").unwrap();
    let path = staged.text("distro.img");
    let layer = SandboxLayer::gpt(LayerRole::Distro, &path, UUID);
    let binding = DistroBinding::import(&layer, PolicyVerdict::ACCEPTED).unwrap();
    let distro = binding.check(&layer, DISTRO_POLICY_REVISION).unwrap();
    #[cfg(target_os = "linux")]
    let distro_number = number(distro.file());
    let plan = SandboxPlan::new(
        SandboxDistro::new(&path, RootImage::gpt(Uuid::parse(UUID))),
        SandboxUpper::RAM,
        SandboxSettings::default(),
    )
    .unwrap();
    let prepared = PreparedLaunch::new(plan.clone(), distro, None, "").unwrap();
    let token = format!("nvx_layer=distro,0xd0003000,gpt:{UUID}");
    assert!(
        prepared
            .kernel_command_line()
            .split(' ')
            .any(|t| t == token),
        "{}",
        prepared.kernel_command_line()
    );
    let blocks = values(prepared.openvmm_arguments(), "--microvm-sandbox-block");
    #[cfg(target_os = "linux")]
    assert_eq!(
        blocks,
        [format!("distro:file:/proc/self/fd/{distro_number},ro")]
    );
    #[cfg(windows)]
    assert_eq!(
        blocks,
        [format!(
            "distro:file:{},ro",
            final_path(&staged, "distro.img")
        )]
    );
    drop(prepared);

    // The disk's binding prepares no plan of an EROFS image at its path.
    let distro = binding.check(&layer, DISTRO_POLICY_REVISION).unwrap();
    let erofs = plan.with_image(RootImage::erofs(Uuid::parse(UUID)));
    assert_eq!(
        failure(PreparedLaunch::new(erofs, distro, None, "")),
        (
            ErrorKind::Unsupported,
            format!(
                "the plan's image is not the gpt layer with identity {UUID} that the distro \
                 binding of {path} names"
            )
        )
    );
}

#[test]
fn prepared_launches_take_only_the_checked_files_of_their_plan() {
    let staged = Staged::new();
    let launch = staged.launch();
    let distro = staged.text("distro.erofs");
    let scratch = staged.text("scratch.ext4");
    let refused = |message: String| (ErrorKind::Unsupported, message);
    let plan = |binding: &DistroBinding| launch.bound_plan(binding).unwrap();

    // Another layer than the plan's image, even with the same content and identity.
    let (binding, _) = staged.checked_distro("distro.erofs");
    let (_, other) = staged.checked_distro("other.erofs");
    assert_eq!(
        failure(PreparedLaunch::new(
            plan(&binding),
            other,
            Some(staged.checked_scratch("scratch.ext4")),
            ""
        )),
        refused(format!(
            "the checked distro layer {} is not the plan's image {distro}",
            staged.text("other.erofs")
        ))
    );

    // A layer whose binding names another identity than the plan's image.
    let (binding, checked) = staged.checked_distro("distro.erofs");
    let other_image = launch
        .plan()
        .with_image(RootImage::erofs(Uuid::parse(OTHER_UUID)));
    assert_eq!(
        failure(PreparedLaunch::new(
            other_image,
            checked,
            Some(staged.checked_scratch("scratch.ext4")),
            ""
        )),
        refused(format!(
            "the plan's image is not the erofs layer with identity {UUID} that the distro binding \
             of {distro} names"
        ))
    );

    // Another scratch image than the plan's, none for a plan on scratch, and one for an upper in
    // RAM.
    let (_, checked) = staged.checked_distro("distro.erofs");
    assert_eq!(
        failure(PreparedLaunch::new(
            plan(&binding),
            checked,
            Some(staged.checked_scratch("other.ext4")),
            ""
        )),
        refused(format!(
            "the checked scratch image {} is not the plan's scratch image {scratch}",
            staged.text("other.ext4")
        ))
    );
    let (_, checked) = staged.checked_distro("distro.erofs");
    assert_eq!(
        failure(PreparedLaunch::new(plan(&binding), checked, None, "")),
        refused(format!(
            "the plan's upper is on the scratch image {scratch}, which was not checked"
        ))
    );
    let (_, checked) = staged.checked_distro("distro.erofs");
    let ram = SandboxPlan::new(
        SandboxDistro::new(&distro, RootImage::erofs(Uuid::parse(UUID))),
        SandboxUpper::RAM,
        SandboxSettings::default(),
    )
    .unwrap();
    assert_eq!(
        failure(PreparedLaunch::new(
            ram,
            checked,
            Some(staged.checked_scratch("scratch.ext4")),
            ""
        )),
        refused(format!(
            "the plan's upper is in RAM, which takes no scratch image, not {scratch}"
        ))
    );

    // A user's command line that the plan refuses fails the preparation.
    let (_, checked) = staged.checked_distro("distro.erofs");
    assert_eq!(
        failure(PreparedLaunch::new(
            plan(&binding),
            checked,
            Some(staged.checked_scratch("scratch.ext4")),
            "nvx_sandbox=0"
        )),
        refused("nvx_sandbox is owned by the sandbox profile".to_owned())
    );
}

#[cfg(windows)]
#[test]
fn prepared_launches_refuse_final_paths_that_openvmm_cannot_take() {
    let staged = Staged::new();
    let refused = |message: String| (ErrorKind::Unsupported, message);
    let delimited = |planned: &str, name: &str| {
        refused(format!(
            "disk paths containing commas, semicolons, or unbalanced brackets are unsupported: \
             {} resolves to {}",
            staged.text(planned),
            final_path(&staged, name)
        ))
    };

    for (link, target) in [
        ("layers", "real;layers"),
        ("unclosed-layers", "real[layers"),
        ("unopened-layers", "real]layers"),
        ("reversed-layers", "real][layers"),
    ] {
        let layers = staged.path(target);
        fs::create_dir(&layers).unwrap();
        for name in ["distro.erofs", "distro.erofs.manifest.json"] {
            fs::copy(staged.path(name), layers.join(name)).unwrap();
        }
        junction(&staged.path(link), &layers);
        let planned = format!("{link}\\distro.erofs");
        let resolved = format!("{target}\\distro.erofs");
        let layer = staged.layer(&planned);
        let binding = DistroBinding::verify(&layer).unwrap();
        let checked = binding.check(&layer, DISTRO_POLICY_REVISION).unwrap();
        let launch = SandboxLaunch::new(
            vec![layer],
            SandboxUpper::scratch(&staged.text("scratch.ext4")),
            SandboxSettings::default(),
        )
        .unwrap();
        assert_eq!(
            failure(PreparedLaunch::new(
                launch.bound_plan(&binding).unwrap(),
                checked,
                Some(staged.checked_scratch("scratch.ext4")),
                ""
            )),
            delimited(&planned, &resolved)
        );
    }

    let plan_on = |scratch: &str| {
        let (binding, checked) = staged.checked_distro("distro.erofs");
        let launch = SandboxLaunch::new(
            vec![staged.layer("distro.erofs")],
            SandboxUpper::scratch(&staged.text(scratch)),
            SandboxSettings::default(),
        )
        .unwrap();
        (launch.bound_plan(&binding).unwrap(), checked)
    };
    for (link, target) in [
        ("images", "real,images"),
        ("unclosed-images", "real[images"),
        ("unopened-images", "real]images"),
        ("reversed-images", "real][images"),
    ] {
        let images = staged.path(target);
        fs::create_dir(&images).unwrap();
        fs::copy(staged.path("scratch.ext4"), images.join("scratch.ext4")).unwrap();
        junction(&staged.path(link), &images);
        let planned = format!("{link}\\scratch.ext4");
        let resolved = format!("{target}\\scratch.ext4");
        let (plan, checked) = plan_on(&planned);
        assert_eq!(
            failure(PreparedLaunch::new(
                plan,
                checked,
                Some(staged.checked_scratch(&planned)),
                ""
            )),
            delimited(&planned, &resolved)
        );
    }

    // A scratch image whose name on disk ends in `.vhd`, which the plan's path names in capitals:
    // OpenVMM, comparing extensions case-sensitively, would attach the plan's path as a raw
    // image, but would open the final path as a VHD.
    fs::write(staged.path("image.vhd"), b"scratch image\n").unwrap();
    let (plan, checked) = plan_on("IMAGE.VHD");
    assert_eq!(
        failure(PreparedLaunch::new(
            plan,
            checked,
            Some(staged.checked_scratch("IMAGE.VHD")),
            ""
        )),
        refused(format!(
            "disk paths ending in .vhd are unsupported, since OpenVMM would not attach them as \
             raw images: {} resolves to {}",
            staged.text("IMAGE.VHD"),
            final_path(&staged, "image.vhd")
        ))
    );
}

#[cfg(windows)]
#[test]
fn prepared_launches_accept_balanced_brackets_in_final_paths() {
    let staged = Staged::new();
    let images = staged.path("real[[images]]");
    fs::create_dir(&images).unwrap();
    for name in ["distro.erofs", "distro.erofs.manifest.json", "scratch.ext4"] {
        fs::copy(staged.path(name), images.join(name)).unwrap();
    }
    junction(&staged.path("images"), &images);
    let (binding, checked) = staged.checked_distro("images\\distro.erofs");
    let launch = SandboxLaunch::new(
        vec![staged.layer("images\\distro.erofs")],
        SandboxUpper::scratch(&staged.text("images\\scratch.ext4")),
        SandboxSettings::default(),
    )
    .unwrap();
    let prepared = PreparedLaunch::new(
        launch.bound_plan(&binding).unwrap(),
        checked,
        Some(staged.checked_scratch("images\\scratch.ext4")),
        "",
    )
    .unwrap();
    assert_eq!(
        values(prepared.openvmm_arguments(), "--microvm-sandbox-block"),
        [
            format!(
                "distro:file:{},ro",
                final_path(&staged, "real[[images]]\\distro.erofs")
            ),
            format!(
                "scratch:file:{}",
                final_path(&staged, "real[[images]]\\scratch.ext4")
            ),
        ]
    );
}

#[cfg(windows)]
#[test]
fn a_release_through_a_process_that_inherited_nothing_closes_none_of_its_handles() {
    use std::ffi::OsString;

    use crate::host::supervisor::{self, VmmCommand};
    use crate::protocol::CAPABILITY_LEN;

    let staged = Staged::new();
    let launch = staged.launch();
    let (binding, distro) = staged.checked_distro("distro.erofs");
    let prepared = PreparedLaunch::new(
        launch.bound_plan(&binding).unwrap(),
        distro,
        Some(staged.checked_scratch("scratch.ext4")),
        "",
    )
    .unwrap();
    // A process that runs until it is ended, and that inherited none of the prepared launch's
    // handles, whose own handles the release must leave alone.
    let system = std::env::var_os("SystemRoot").unwrap();
    let command = VmmCommand::new(
        Path::new(&system).join("System32").join("PING.EXE"),
        ["-n", "60", "127.0.0.1"].map(OsString::from).to_vec(),
        staged.directory.path(),
    );
    let log = fs::File::create(staged.path("other.log")).unwrap();
    let other = supervisor::spawn(&command, &[1; CAPABILITY_LEN], log).unwrap();
    let process = other.process().unwrap();
    let released = failure(prepared.release(&process));
    other.kill_and_wait();
    assert_eq!(
        released,
        (
            ErrorKind::Io,
            format!(
                "cannot release the guard of {}, which VMM process {} inherited",
                staged.text("distro.erofs"),
                process.reference().pid
            )
        )
    );
    // The release closed the checked files nonetheless, which no longer hold the images.
    for name in ["distro.erofs", "scratch.ext4"] {
        fs::rename(staged.path(name), staged.path("moved")).unwrap();
        fs::rename(staged.path("moved"), staged.path(name)).unwrap();
    }
}

#[test]
fn staging_checking_and_preparing_a_launch_read_no_content() {
    let staged = Staged::new();
    let launch = staged.launch();
    let scratch = PathBuf::from(staged.text("scratch.ext4"));

    // Staging: a caller that verified the layer imports its binding, with its manifest, and binds
    // the scratch image. Then the checks and the preparation of the launch. On this host, every
    // read and mapping of content through what they open fails.
    let binding =
        distro::import_with(launch.distro(), PolicyVerdict::ACCEPTED, METADATA_ONLY).unwrap();
    assert!(binding.manifest().is_some());
    let scratch_binding = scratch::bind_with(&scratch, WRITE_ONLY).unwrap();
    let checked = binding
        .check_with(launch.distro(), DISTRO_POLICY_REVISION, METADATA_ONLY)
        .unwrap();
    let checked_scratch = scratch_binding
        .check_with(&scratch, Instant::now(), WRITE_ONLY)
        .unwrap();
    let prepared = PreparedLaunch::new(
        launch.bound_plan(&binding).unwrap(),
        checked,
        Some(checked_scratch),
        USER_COMMAND_LINE,
    )
    .unwrap();
    let mut byte = [0];
    assert!(prepared.distro().file().read(&mut byte).is_err());
    assert!(prepared.scratch().unwrap().file().read(&mut byte).is_err());

    // Verification, the positive control, reads content: it fails on that host...
    let (kind, message) = failure(distro::verify_with(launch.distro(), METADATA_ONLY));
    assert_eq!(kind, ErrorKind::Unsupported);
    assert!(
        message.starts_with(&format!(
            "invalid sandbox distro manifest: {:?}: ",
            staged.path("distro.erofs.manifest.json")
        )),
        "{message}"
    );
    // ...and with reads allowed, binds the layer as the staging caller did.
    assert_eq!(DistroBinding::verify(launch.distro()).unwrap(), binding);
}

#[test]
fn no_plan_encodes_root_as_the_workload_identity() {
    // The VMM receives the plan's identity, which is never root's: a root profile is the
    // adapter's choice, which it makes otherwise.
    let staged = Staged::new();
    for identity in [(0, 0), (0, 1000), (1000, 0)] {
        let settings = SandboxSettings {
            workload_identity: identity,
            ..SandboxSettings::default()
        };
        let (kind, message) = failure(SandboxPlan::new(
            SandboxDistro::new(
                &staged.text("distro.erofs"),
                RootImage::erofs(Uuid::parse(UUID)),
            ),
            SandboxUpper::RAM,
            settings,
        ));
        assert_eq!(kind, ErrorKind::Unsupported, "{message}");
    }
    let (_, distro) = staged.checked_distro("distro.erofs");
    let prepared = PreparedLaunch::new(
        staged.launch().plan().clone(),
        distro,
        Some(staged.checked_scratch("scratch.ext4")),
        "",
    )
    .unwrap();
    assert_eq!(
        values(prepared.openvmm_arguments(), "--microvm-workload-identity"),
        ["65534:65534"]
    );

    // An adapter's plan names no workload identity: its own guest chooses one, root's too.
    fs::write(staged.path("distro.img"), b"gpt disk\n").unwrap();
    let path = staged.text("distro.img");
    let layer = SandboxLayer::gpt(LayerRole::Distro, &path, UUID);
    let binding = DistroBinding::import(&layer, PolicyVerdict::ACCEPTED).unwrap();
    let plan = SandboxPlan::adapter(
        SandboxDistro::new(&path, RootImage::gpt(Uuid::parse(UUID))),
        SandboxUpper::RAM,
    );
    let user = "console=hvc0 nvx_control_tty=hvc2";
    let prepared = PreparedLaunch::new(
        plan.clone(),
        binding.check(&layer, DISTRO_POLICY_REVISION).unwrap(),
        None,
        user,
    )
    .unwrap();
    assert!(values(prepared.openvmm_arguments(), "--microvm-workload-identity").is_empty());
    assert_eq!(
        prepared.kernel_command_line(),
        plan.kernel_command_line(user).unwrap()
    );
}
