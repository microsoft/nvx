//! Captures of templates through the supervisor, with the fake OpenVMM as their VMM: the block
//! devices that the VMM opens, which no handle of the host may write as it starts, the
//! generations that captures publish and that creations then serve, VMs that end without a
//! capture or never end, captures of one class that race, and an adapter that crashes while its
//! capture's VM runs, whose VMM a recovery ends.
//!
//! The fake OpenVMM publishes the snapshot whose manifest OpenVMM wrote for the fixture
//! `tests/fixtures/template/platform-kvm`, or fails, as the scenario next to the kernel asks; see
//! `nvx::testing::fake_openvmm`. The crash is real: a child process of this test binary, the
//! adapter, captures in the store, and the test kills it while its capture's VM runs.
#![cfg(all(
    feature = "host",
    feature = "testing",
    any(target_os = "linux", windows)
))]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use nvx::host::platform;
use nvx::host::process::{BootId, Process, ProcessRef};
use nvx::host::sandbox::distro::{DISTRO_POLICY_REVISION, DistroBinding, PolicyVerdict};
use nvx::host::sandbox::{LayerRole, SandboxDistro, SandboxLayer, SandboxPlan, SandboxUpper};
#[cfg(target_os = "linux")]
use nvx::host::store::SOCKET_NAME;
use nvx::host::store::{ArtifactIdentity, OUTCOME_NAME, RuntimeIdentity, SandboxStore};
use nvx::host::template::{
    BlockGeometry, CAPTURES_NAME, Capture, CaptureClass, Created, GENERATIONS_NAME, Generation,
    HostTime, Invariants, SCRATCH_NAME, SNAPSHOT_NAME, Store, TRANSACTION_NAME, TemplateKey,
    TemplateManifest,
};
use nvx::protocol::root::RootImage;
use nvx::uuid::Uuid;
use nvx::{Artifacts, ErrorCode, ErrorKind, Hypervisor};
use serde_json::{Value, json};

/// Bytes in a MiB.
const MIB: u64 = 1024 * 1024;
/// Nanoseconds in a second.
const SECOND_NS: u64 = 1_000_000_000;
/// The identity of the placeholder.
const IDENTITY: &str = "11111111-1111-1111-1111-111111111111";
/// Name of the scenario of the fake OpenVMM's captures, next to the kernel.
const SCENARIO_NAME: &str = "fake-openvmm-capture.json";
/// Name of the marker that the fake OpenVMM holds while a capture's VM runs, in the platform's
/// directory.
const ACTIVE_NAME: &str = "active";
/// The longest wait of these tests for a capture, a recovery, or a process.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Environment variable that marks the adapter that [`adapter_role`] runs, and names the
/// directory of its bench.
const ADAPTER_BENCH: &str = "NVX_TEMPLATE_CAPTURE_TEST_BENCH";

/// Serializes the tests that spawn processes: a process that one test spawns while another test's
/// log is inheritable for its own spawn could hold that log's claim for a while.
static SPAWNS: Mutex<()> = Mutex::new(());

/// Takes [`SPAWNS`].
fn serialized() -> MutexGuard<'static, ()> {
    SPAWNS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Returns the directory of the fixture whose manifest OpenVMM wrote for a capture of [`class`].
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/template/platform-kvm")
}

/// Returns the manifest of [`fixture`].
fn manifest() -> TemplateManifest {
    TemplateManifest::read(&fixture()).unwrap()
}

/// Returns the identity `sha256:` and 64 copies of `digit`.
fn artifact(digit: char) -> ArtifactIdentity {
    ArtifactIdentity::sha256(&digit.to_string().repeat(64)).unwrap()
}

/// Returns the class of the capture of [`fixture`].
fn class() -> CaptureClass {
    CaptureClass {
        runtime: RuntimeIdentity::new(artifact('a'), artifact('b'), artifact('c')),
        guest_build: ArtifactIdentity::new("guest-build-1").unwrap(),
        hypervisor: Hypervisor::Kvm,
        cpu_profile: "auto".to_owned(),
        processors: 2,
        boot_processors: 2,
        memory: 512 * MIB,
        memory_capacity: 2048 * MIB,
        distro: BlockGeometry {
            length: 1024 * MIB,
            logical_block_size: 512,
            physical_block_size: 4096,
        },
        scratch: BlockGeometry {
            length: 64 * MIB,
            logical_block_size: 512,
            physical_block_size: 512,
        },
        network: None,
        invariants: Invariants::new(),
    }
}

/// Returns the host's time `seconds` after the capture of [`fixture`], on the host and in the
/// boot that captured it.
fn after(seconds: u64) -> HostTime {
    let manifest = manifest();
    let time = manifest.time();
    HostTime {
        host_id: time.host_id,
        boot_id: time.boot_id,
        clock: time.host_clock,
        monotonic_ns: time.capture_monotonic_ns + seconds * SECOND_NS,
        utc_ns: time.capture_utc_ns + seconds * SECOND_NS,
    }
}

/// Returns a deadline that no capture of these tests reaches.
fn deadline() -> Instant {
    Instant::now() + TIMEOUT
}

/// Creates the directory `path`, private to the current user: on Windows, with a protected list
/// that allows the owner and SYSTEM alone, which the files created in it inherit.
fn private_dir(path: &Path) {
    fs::create_dir(path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[cfg(windows)]
    windows::set_access(path, "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)");
}

/// Creates the sparse file `path` of `length` bytes, which no one else may write, whatever the
/// umask.
fn sparse(path: &Path, length: u64) {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(path).unwrap();
    #[cfg(windows)]
    windows::set_sparse(&file);
    file.set_len(length).unwrap();
}

/// Returns the file at `path` as JSON.
fn json_of(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

/// Returns the names of the entries of `dir`, sorted.
fn sorted(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Returns the store generation of `generation`, as a number.
fn number(generation: &Generation) -> u64 {
    generation.generation().get().get()
}

/// A store, and the inputs of captures of [`class`] into it, in a temporary directory: the
/// platform's files in `platform`, a placeholder, a kernel, an initramfs, and the fake OpenVMM's
/// scenario, which a capture's VM follows, and the store in `store`.
struct Bench {
    /// The temporary directory.
    directory: tempfile::TempDir,
    /// The store.
    store: Store,
    /// The inputs of a capture.
    capture: Capture,
}

impl Bench {
    /// Returns a bench whose VMs publish the fixture's snapshot, as `scenario` modifies the
    /// default scenario.
    fn new(scenario: &Value) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let platform = directory.path().join("platform");
        private_dir(&platform);
        sparse(&platform.join("placeholder.erofs"), class().distro.length);
        fs::write(platform.join("vmlinux"), b"kernel").unwrap();
        fs::write(platform.join("initramfs.cpio.gz"), b"initrd").unwrap();
        let store = Store::create(directory.path().join("store")).unwrap();
        let bench = Self {
            store,
            capture: capture_in(&platform),
            directory,
        };
        bench.follow(scenario);
        bench
    }

    /// Returns the bench in `dir`, as another process made it.
    fn open(dir: &Path) -> (Store, Capture) {
        (
            Store::open(dir.join("store")).unwrap(),
            capture_in(&dir.join("platform")),
        )
    }

    /// Returns the platform's directory.
    fn platform(&self) -> PathBuf {
        self.directory.path().join("platform")
    }

    /// Lets the next VMs follow the default scenario, as `fields` modify it: the fake OpenVMM
    /// publishes the fixture's snapshot.
    fn follow(&self, fields: &Value) {
        let mut scenario = json!({
            "manifest": fixture().join("manifest.bin"),
            "stateLen": manifest().state_len(),
        });
        for (name, value) in fields.as_object().unwrap() {
            scenario[name] = value.clone();
        }
        fs::write(self.platform().join(SCENARIO_NAME), scenario.to_string()).unwrap();
    }

    /// Returns the path of the marker of an active VM.
    fn active(&self) -> PathBuf {
        self.platform().join(ACTIVE_NAME)
    }

    /// Returns the slot of the class, the only one.
    fn slot(&self) -> PathBuf {
        let captures = self.store.dir().join(CAPTURES_NAME);
        let slots = sorted(&captures);
        assert_eq!(slots.len(), 1, "{slots:?}");
        captures.join(&slots[0])
    }

    /// Returns the path of the file `suffix` that the fake OpenVMM wrote of the slot's last VM,
    /// which it names after the directory of the VM's outcome report: the capture's transaction,
    /// from whose sandbox store the VM launched.
    fn dump(&self, suffix: &str) -> PathBuf {
        self.platform()
            .join(format!("fake-openvmm-{TRANSACTION_NAME}{suffix}.json"))
    }

    /// Creates a template at a second after the fixture's capture.
    fn create(&self) -> nvx::Result<Created> {
        self.store
            .create_template(&self.capture, || Ok(after(1)), deadline())
    }

    /// Refreshes the template.
    fn refresh(&self) -> nvx::Result<Generation> {
        self.store.refresh_template(&self.capture, deadline())
    }

    /// Returns the store generations that the index lists.
    fn indexed(&self) -> Vec<u64> {
        self.store
            .index()
            .unwrap()
            .generations()
            .iter()
            .map(number)
            .collect()
    }

    /// Asserts that no capture of the class runs or remains: its slot holds no transaction and no
    /// launch.
    fn assert_idle(&self) {
        let slot = self.slot();
        assert!(!slot.join(TRANSACTION_NAME).exists());
        assert!(
            SandboxStore::open(&slot)
                .unwrap()
                .journal()
                .unwrap()
                .is_none()
        );
    }
}

/// Returns the inputs of a capture of [`class`] whose placeholder, kernel, initramfs, and
/// scenario are in `platform`, and whose VMM is the fake OpenVMM.
fn capture_in(platform: &Path) -> Capture {
    let placeholder = platform.join("placeholder.erofs");
    let path = placeholder.to_str().unwrap();
    let layer = SandboxLayer::new(LayerRole::Distro, path, IDENTITY);
    let binding = DistroBinding::import(&layer, PolicyVerdict::ACCEPTED).unwrap();
    let image = RootImage::erofs(Some(Uuid::parse(IDENTITY).unwrap()));
    let upper = SandboxUpper::scratch(platform.join("clone.ext4").to_str().unwrap());
    Capture {
        class: class(),
        adapter: SandboxPlan::adapter(SandboxDistro::new(path, image), upper),
        binding,
        policy_revision: DISTRO_POLICY_REVISION,
        artifacts: Artifacts::new(
            env!("CARGO_BIN_EXE_nvx-fake-openvmm"),
            platform.join("vmlinux"),
            platform.join("initramfs.cpio.gz"),
        ),
    }
}

/// Returns the value of `option` in the command line `arguments`.
fn value_of<'a>(arguments: &'a [String], option: &str) -> &'a str {
    let at = arguments
        .iter()
        .position(|argument| argument == option)
        .unwrap_or_else(|| panic!("{option} is missing from {arguments:?}"));
    &arguments[at + 1]
}

#[test]
fn captures_publish_generations_that_creations_then_serve() {
    let _spawns = serialized();
    let bench = Bench::new(&json!({}));
    let Created::Captured(generation) = bench.create().unwrap() else {
        panic!("the first creation captures");
    };
    assert_eq!(bench.indexed(), [1]);
    assert_eq!(number(&generation), 1);
    // The VMM received the template's command line, with the generation's storage generation as
    // its block devices' identity.
    let arguments: Vec<String> = serde_json::from_value(json_of(&bench.dump(""))).unwrap();
    assert_eq!(value_of(&arguments, "--cmdline"), "nvx_sandbox=1");
    assert_eq!(
        value_of(&arguments, "--snapshot-generation-id"),
        generation.storage_generation().to_string()
    );
    let transaction = bench.slot().join(TRANSACTION_NAME);
    assert_eq!(
        Path::new(value_of(&arguments, "--snapshot-destination")),
        transaction.join(SNAPSHOT_NAME)
    );
    // The VM launched from the transaction's sandbox store, so that its control socket, on
    // Linux, is beside the snapshot's destination, as OpenVMM requires, and its outcome report
    // too, which the slot then keeps.
    assert_eq!(
        Path::new(value_of(&arguments, "--microvm-report")),
        transaction.join(OUTCOME_NAME)
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        value_of(&arguments, "--microvm-control-console"),
        format!("listen={}", transaction.join(SOCKET_NAME).display())
    );
    assert!(bench.slot().join(OUTCOME_NAME).is_file());
    // The VMM opened the placeholder and the scratch at their paths, and found no handle that
    // could write them: on Windows, its open would have failed, and on Linux, its read lease
    // finds one, unless the file system takes no lease.
    let blocks = json_of(&bench.dump("-blocks"));
    let opened: Vec<(&str, PathBuf, u64)> = blocks
        .as_array()
        .unwrap()
        .iter()
        .map(|block| {
            assert_ne!(block["writers"], true, "{block}");
            (
                block["role"].as_str().unwrap(),
                PathBuf::from(block["path"].as_str().unwrap()),
                block["length"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        opened,
        [
            (
                "distro",
                bench.platform().join("placeholder.erofs"),
                1024 * MIB
            ),
            ("scratch", transaction.join(SCRATCH_NAME), 64 * MIB),
        ]
    );
    // The generation's payload holds the snapshot that the VMM published, with its key, and
    // the slot keeps nothing of the capture.
    let snapshot = bench.store.snapshot_dir(&generation).unwrap();
    assert_eq!(
        sorted(&snapshot),
        ["manifest.bin", "memory.bin", "state.bin"]
    );
    let key = TemplateKey::new(&bench.capture.class, &manifest()).unwrap();
    assert_eq!(bench.store.read_key(&generation).unwrap(), key);
    assert_eq!(generation.key(), key.identity());
    bench.assert_idle();
    // A second creation serves the generation without a VM.
    fs::remove_file(bench.dump("")).unwrap();
    assert_eq!(bench.create().unwrap(), Created::Existing(generation));
    assert!(!bench.dump("").exists());
}

#[test]
fn refreshes_publish_new_generations_and_leave_the_older_ones() {
    let _spawns = serialized();
    let bench = Bench::new(&json!({}));
    let first = bench.create().unwrap().into_generation();
    // A VMM that ends abruptly once it has published leaves the file that backed the RAM, which
    // the publication does not take.
    bench.follow(&json!({ "keepMemory": true }));
    let second = bench.refresh().unwrap();
    assert_eq!(number(&second), 2);
    assert_ne!(second.storage_generation(), first.storage_generation());
    assert_eq!(bench.indexed(), [1, 2]);
    for generation in [&first, &second] {
        let snapshot = bench.store.snapshot_dir(generation).unwrap();
        assert_eq!(
            sorted(&snapshot),
            ["manifest.bin", "memory.bin", "state.bin"]
        );
    }
    assert_eq!(
        sorted(&bench.store.dir().join(GENERATIONS_NAME)),
        ["1", "2"]
    );
    bench.assert_idle();
    assert_eq!(bench.create().unwrap(), Created::Existing(second));
}

#[test]
fn vms_that_end_without_a_capture_publish_nothing() {
    let _spawns = serialized();
    let bench = Bench::new(&json!({}));
    for scenario in [
        json!({ "outcome": "fail", "status": 3 }),
        // The snapshot that the VMM published before it failed is not the capture's.
        json!({ "outcome": "publish-then-fail" }),
        json!({ "outcome": "crash" }),
    ] {
        bench.follow(&scenario);
        let error = bench.refresh().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Exited, "{scenario}: {error}");
        assert!(bench.indexed().is_empty(), "{scenario}");
        bench.assert_idle();
    }
    // The slot serves the next capture.
    bench.follow(&json!({}));
    bench.refresh().unwrap();
    assert_eq!(bench.indexed(), [1]);
}

/// Returns the process ID that the marker of an active VM at `path` holds.
fn active_pid(path: &Path) -> u32 {
    fs::read_to_string(path).unwrap().parse().unwrap()
}

#[test]
fn vms_that_never_end_are_torn_down_at_the_deadline() {
    let _spawns = serialized();
    let bench = Bench::new(&json!({}));
    for outcome in ["hang", "publish-then-hang"] {
        bench.follow(&json!({ "outcome": outcome, "active": bench.active() }));
        let deadline = Instant::now() + Duration::from_secs(2);
        let error = bench
            .store
            .refresh_template(&bench.capture, deadline)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut, "{outcome}: {error}");
        // The VMM, which was killed, left its marker.
        let pid = active_pid(&bench.active());
        assert_eq!(
            platform::process_start_time(pid).unwrap(),
            None,
            "{outcome}"
        );
        fs::remove_file(bench.active()).unwrap();
        assert!(bench.indexed().is_empty(), "{outcome}");
        bench.assert_idle();
    }
}

#[test]
fn captures_of_one_class_are_serialized() {
    let _spawns = serialized();
    // A VM that started while another ran would fail to mark itself active.
    let bench = Bench::new(&json!({}));
    bench.follow(&json!({ "active": bench.active(), "delayMs": 300 }));
    let (store, capture) = (&bench.store, &bench.capture);
    let generations: Vec<Generation> = thread::scope(|scope| {
        let refreshes: Vec<_> = (0..2)
            .map(|_| scope.spawn(|| store.refresh_template(capture, deadline())))
            .collect();
        refreshes
            .into_iter()
            .map(|refresh| refresh.join().unwrap().unwrap())
            .collect()
    });
    let mut published: Vec<u64> = generations.iter().map(number).collect();
    published.sort_unstable();
    assert_eq!(published, [1, 2]);
    assert_eq!(bench.indexed(), [1, 2]);
    assert!(!bench.active().exists());
    bench.assert_idle();
}

/// The adapter that [`a_vm_that_outlives_its_crashed_adapter_is_ended_by_recovery`] runs in a
/// process of its own: it refreshes the template of the bench that [`ADAPTER_BENCH`] names, until
/// the test kills it.
#[test]
#[ignore = "an adapter that the capture tests run in a process of its own"]
fn adapter_role() {
    let Some(dir) = env::var_os(ADAPTER_BENCH) else {
        return;
    };
    let (store, capture) = Bench::open(Path::new(&dir));
    let result = store.refresh_template(&capture, deadline());
    panic!("the capture ended before the test killed its adapter: {result:?}");
}

/// A VMM that a crashed adapter left behind, which is killed when the test ends if it still runs.
struct Orphan(ProcessRef);

impl Orphan {
    /// Returns whether the VMM still runs.
    fn runs(&self) -> bool {
        platform::process_start_time(self.0.pid).unwrap() == Some(self.0.start_time)
    }
}

impl Drop for Orphan {
    fn drop(&mut self) {
        if let Ok(Some(process)) = Process::open(&self.0) {
            let _ = process.kill();
            let _ = process.wait(Instant::now() + TIMEOUT);
        }
    }
}

/// Runs [`adapter_role`] on `bench` until its capture's VM is active, the journal of the
/// transaction's sandbox store records the VMM, and, if `published`, the VM has published its
/// snapshot, then kills the adapter, as a crash would end it, and returns the VMM, which still
/// runs.
fn crash_adapter(bench: &Bench, published: bool) -> Orphan {
    let mut adapter = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "adapter_role",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ADAPTER_BENCH, bench.directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let recorded = loop {
        assert!(
            adapter.try_wait().unwrap().is_none(),
            "the adapter ended before the test killed it"
        );
        // The VM marks itself active before it publishes its snapshot.
        let recorded = bench.active().exists().then(|| {
            let transaction = bench.slot().join(TRANSACTION_NAME);
            if published && !transaction.join(SNAPSHOT_NAME).is_dir() {
                return None;
            }
            SandboxStore::open(transaction)
                .ok()?
                .journal()
                .ok()??
                .process()
                .cloned()
        });
        if let Some(Some(process)) = recorded {
            break process;
        }
        assert!(
            started.elapsed() < TIMEOUT,
            "the capture's VM did not start"
        );
        thread::sleep(Duration::from_millis(20));
    };
    adapter.kill().unwrap();
    adapter.wait().unwrap();
    assert_eq!(recorded.pid, active_pid(&bench.active()));
    assert_eq!(recorded.boot, BootId::current().unwrap());
    let orphan = Orphan(recorded);
    assert!(orphan.runs());
    orphan
}

#[test]
fn a_vm_that_outlives_its_crashed_adapter_is_ended_by_recovery() {
    let _spawns = serialized();
    let bench = Bench::new(&json!({}));
    // A VM that published its snapshot, but whose exit no capture verified, is not published
    // either.
    for outcome in ["hang", "publish-then-hang"] {
        bench.follow(&json!({ "outcome": outcome, "active": bench.active() }));
        let orphan = crash_adapter(&bench, outcome == "publish-then-hang");
        let deadline = deadline();
        // A scanner that briefly opens the launch's log can keep its claim held for a moment.
        let recovered = loop {
            match bench.store.recover_captures(deadline) {
                Err(error)
                    if error.code() == ErrorCode::VmmIndeterminate && Instant::now() < deadline =>
                {
                    eprintln!("the recovery is indeterminate: {error}");
                    thread::sleep(Duration::from_millis(100));
                }
                recovered => break recovered.unwrap(),
            }
        };
        assert!(recovered.is_empty(), "{outcome}");
        assert!(!orphan.runs(), "{outcome}");
        assert!(bench.indexed().is_empty(), "{outcome}");
        bench.assert_idle();
        fs::remove_file(bench.active()).unwrap();
    }
    // The slot serves the next capture.
    bench.follow(&json!({}));
    bench.refresh().unwrap();
    assert_eq!(bench.indexed(), [1]);
}

/// What the tests need of Windows that the crate does not export.
#[cfg(windows)]
mod windows {
    use std::fs::File;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::{io, ptr};

    use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SetFileSecurityW,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    /// Returns `text` as a NUL-terminated UTF-16 string.
    fn wide(text: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
        text.as_ref().encode_wide().chain([0]).collect()
    }

    /// Replaces the access control list of the file or directory `path` with the protected list
    /// of the descriptor `sddl`, in the security descriptor definition language.
    pub(super) fn set_access(path: &Path, sddl: &str) {
        let sddl = wide(sddl);
        let name = wide(path);
        let mut descriptor = ptr::null_mut();
        // SAFETY: the string is NUL-terminated, and the output pointer is valid.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        assert_ne!(converted, 0, "{}", io::Error::last_os_error());
        let information = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
        // SAFETY: the name is NUL-terminated, and the descriptor is valid.
        let set = unsafe { SetFileSecurityW(name.as_ptr(), information, descriptor) };
        let error = io::Error::last_os_error();
        // SAFETY: the conversion allocated the descriptor, which nothing uses any more.
        unsafe { LocalFree(descriptor) };
        assert_ne!(set, 0, "{error}");
    }

    /// Marks the file that `file` holds open for writing sparse, which NTFS otherwise allocates
    /// whole as the file is extended.
    pub(super) fn set_sparse(file: &File) {
        /// `FSCTL_SET_SPARSE`, in `winioctl.h`, which marks a file sparse without a buffer.
        const FSCTL_SET_SPARSE: u32 = 0x0009_00c4;
        let mut returned = 0;
        // SAFETY: the handle is open for as long as `file` is borrowed, the control takes no
        // input or output buffer, and the count's pointer is valid.
        let set = unsafe {
            DeviceIoControl(
                file.as_raw_handle() as HANDLE,
                FSCTL_SET_SPARSE,
                ptr::null(),
                0,
                ptr::null_mut(),
                0,
                &mut returned,
                ptr::null_mut(),
            )
        };
        assert_ne!(set, 0, "{}", io::Error::last_os_error());
    }
}
