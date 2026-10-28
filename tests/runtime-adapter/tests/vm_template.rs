//! The host program captures templates on a real hypervisor, through public APIs only: VMs of the
//! guest agent's template mode, which NVX's template store captures through NVX's supervisor and
//! publishes as its generations, as `runtime-adapter-host create` asks.
//!
//! Each scenario stages the placeholder of its templates, a copy of the fixtures' Ubuntu EROFS
//! image with its manifest, as a platform stages its trusted artifacts, and creates a [`Template`]
//! of it, with a [`Network`], or without one, in a template store of its own. A template that the
//! store captured is the store's one generation, whose payload holds the record of its capture,
//! its key, and its snapshot, and whose key holds what the scenario asked for: the runtime of the
//! adapter's VMs, with the guest agent's build, the backend, the processors and those online at
//! boot, the RAM and its capacity, the geometries of the placeholder and of the dormant scratch, a
//! NIC exactly when the network lets something through, which the capture attaches under the
//! complete deny-all policy, whatever the network's own, and a kernel command line of the platform
//! tier's tokens alone, with `maxcpus=` for a template whose processors are not all online at
//! boot, and the NIC's addresses, but neither a DNS server nor a publication of the guest's
//! listeners, which names neither the placeholder's identity nor a root. The slot of the
//! template's class then holds no transaction, and records no launch, and a second creation serves
//! the generation without a capture, as does the creation of a network of any other policy at the
//! same guest's address, whose class is the same. The host program refuses a class that no capture
//! takes before it creates the store.
//!
//! Other scenarios run the host program's operations on the generations of a store: `refresh`,
//! which captures a generation of a class whose store holds an eligible one already, and retires
//! those that the new one supersedes, `list` and `inspect`, which report what the store records of
//! its generations, and `remove`, which retires one. A clone's lease, which a test takes through
//! NVX's store, as a clone's start does, but without launching anything, keeps a generation that a
//! refresh supersedes, or that a removal retires, tombstoned, with its payload, until the clone
//! releases it: a removal reports busy, with status 7, until then, and the next removal, or the
//! next eviction of the class, deletes it.
//!
//! The tests boot VMs, so they are ignored. They read what the `common` module lists, and take the
//! placeholder from its fixtures.
//!
//! ```text
//! cargo test --locked --features host --test vm_template -- --ignored
//! ```
#![cfg(feature = "host")]

mod common;

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nvx::activation::StoreGeneration;
use nvx::host::artifacts::Artifacts;
use nvx::host::store::{CallerFiles, LOG_NAME, SandboxStore};
use nvx::host::template::{
    CAPTURE_RECORD_NAME, CAPTURES_NAME, GENERATIONS_NAME, HostTime, KEY_NAME, Lease, MANIFEST_NAME,
    MEMORY_BLOCK_SIZE, SNAPSHOT_NAME, Store, TRANSACTION_NAME,
};
use nvx::protocol::cmdline::{self, Parameter};
use nvx::{ErrorCode, Hypervisor};
use runtime_adapter::GUEST_BUILD;
use runtime_adapter::capture::{DEFAULT_SCRATCH_LENGTH, LOGICAL_BLOCK_SIZE};
use runtime_adapter::probe::escape;
use runtime_adapter::runtime::Runtime;
use runtime_adapter::template::is_platform_token;
use serde_json::{Value, json};

use common::{Fixtures, Printed, Sandbox};

/// The exit status of the host program's command that failed, after a diagnostic.
const FAILED: i32 = 1;
/// The exit status of a removal that found leases that keep the generation.
const BUSY: i32 = 7;
/// How long a test waits for the template store's lock, and for the lock of a clone's sandbox.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
/// Bytes in a mebibyte.
const MIB: u64 = 1024 * 1024;
/// The RAM of a template, unless the scenario names another, and its capacity, which the host
/// program gives one by default: NVX's default memory, 256 MiB.
const DEFAULT_MEMORY: u64 = 256 * MIB;
/// The hexadecimal digits of the identity of a capture class that name its slot in a template
/// store.
const SLOT_NAME_LEN: usize = 16;
/// The guest's address in the scenarios' networks, in a range reserved for documentation.
const GUEST_NETWORK: &str = "192.0.2.2/24";
/// The guest's gateway in the scenarios' networks.
const GATEWAY: &str = "192.0.2.1";
/// The port of host loopback at which a proxy-only network names its proxy. The capture opens no
/// connection, so nothing listens there.
const PROXY_PORT: u16 = 3128;
/// The port of the gateway to which the allow rule lets TCP through.
const RULE_PORT: u16 = 8080;
/// The port of host loopback that a forward forwards to the guest's [`RULE_PORT`]. The capture
/// binds no forward, so nothing holds it.
const FORWARD_PORT: u16 = 18080;

/// The network of a scenario's template, which the creation reads from a file that the test
/// writes, in the serde form of a plan's network: the guest at [`GUEST_NETWORK`], with egress
/// denied and no host-loopback access unless the network says otherwise. The capture attaches the
/// NIC of a network that lets something through under the complete deny-all policy, whatever the
/// network's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Network {
    /// No network, which attaches no network device.
    None,
    /// Everything denied, without a proxy, which attaches no network device either.
    Deny,
    /// Egress allowed, without host-loopback access.
    Egress,
    /// Egress denied but for one rule, TCP to the gateway at [`RULE_PORT`], with outbound
    /// host-loopback access.
    AllowRule,
    /// Egress allowed, with outbound host-loopback access.
    Outbound,
    /// A proxy-only network, whose proxy is at [`PROXY_PORT`] of host loopback.
    Proxy,
    /// Ingress allowed, under which OpenVMM would publish the guest's listeners.
    Ingress,
    /// Host-loopback access in both directions, under which OpenVMM would publish the guest's
    /// listeners on host loopback.
    HostLoopback,
    /// Host-loopback access in both directions, with a forward from [`FORWARD_PORT`] of host
    /// loopback to the guest's [`RULE_PORT`].
    Forward,
}

impl Network {
    /// The networks that let something through, each at [`GUEST_NETWORK`].
    const WITH_NIC: [Self; 7] = [
        Self::Egress,
        Self::AllowRule,
        Self::Outbound,
        Self::Proxy,
        Self::Ingress,
        Self::HostLoopback,
        Self::Forward,
    ];
    /// Returns the network's serde form, or `None` without a network.
    fn form(self) -> Option<Value> {
        let mut form = json!({
            "guestNetwork": GUEST_NETWORK,
            "egress": "deny",
            "allow": {"name": "egress.allow", "rules": []},
            "deny": {"name": "egress.deny", "rules": []},
            "ingress": "deny",
            "hostLoopback": "deny",
            "hostLoopbackForwards": [],
            "proxy": null,
        });
        match self {
            Self::None => return None,
            Self::Deny => {}
            Self::Egress => form["egress"] = json!("allow"),
            Self::AllowRule => {
                form["allow"]["rules"] = json!([{
                    "to": [{"cidr": format!("{GATEWAY}/32"), "except": []}],
                    "ports": [{"protocol": "tcp", "port": RULE_PORT, "endPort": null}],
                }]);
                form["hostLoopback"] = json!("outbound");
            }
            Self::Outbound => {
                form["egress"] = json!("allow");
                form["hostLoopback"] = json!("outbound");
            }
            Self::Proxy => form["proxy"] = json!({"port": PROXY_PORT, "scheme": "http"}),
            Self::Ingress => form["ingress"] = json!("allow"),
            Self::HostLoopback => form["hostLoopback"] = json!("allow"),
            Self::Forward => {
                form["hostLoopback"] = json!("allow");
                form["hostLoopbackForwards"] = json!([{
                    "protocol": "tcp",
                    "hostPort": FORWARD_PORT,
                    "guestPort": RULE_PORT,
                }]);
            }
        }
        Some(form)
    }

    /// Returns whether the template has a NIC: whether the network lets anything through, for
    /// which OpenVMM attaches a network device.
    fn has_nic(self) -> bool {
        !matches!(self, Self::None | Self::Deny)
    }
}

/// A template that a scenario creates: its network, and the processors, those online at boot, the
/// RAM, the RAM capacity, and the length of the dormant scratch that the creation names, or else
/// the host program's defaults.
#[derive(Clone, Copy, Debug)]
struct Template {
    /// The network.
    network: Network,
    /// The processors, or 1 by default.
    processors: Option<u32>,
    /// The processors online at boot, or 1 by default.
    boot_processors: Option<u32>,
    /// The RAM, in bytes, or [`DEFAULT_MEMORY`] by default.
    memory: Option<u64>,
    /// The RAM capacity, in bytes, or the RAM by default.
    memory_capacity: Option<u64>,
    /// The length of the dormant scratch, in bytes, or [`DEFAULT_SCRATCH_LENGTH`] by default.
    scratch_length: Option<u64>,
}

impl Template {
    /// Returns the template of `network`, with the host program's defaults.
    fn new(network: Network) -> Self {
        Self {
            network,
            processors: None,
            boot_processors: None,
            memory: None,
            memory_capacity: None,
            scratch_length: None,
        }
    }

    /// Returns the processors of the template.
    fn processors(&self) -> u32 {
        self.processors.unwrap_or(1)
    }

    /// Returns the processors of the template that are online at boot.
    fn boot_processors(&self) -> u32 {
        self.boot_processors.unwrap_or(1)
    }

    /// Returns the RAM of the template, in bytes.
    fn memory(&self) -> u64 {
        self.memory.unwrap_or(DEFAULT_MEMORY)
    }

    /// Returns the RAM capacity of the template, in bytes.
    fn memory_capacity(&self) -> u64 {
        self.memory_capacity.unwrap_or(self.memory())
    }

    /// Returns the length of the template's dormant scratch, in bytes.
    fn scratch_length(&self) -> u64 {
        self.scratch_length.unwrap_or(DEFAULT_SCRATCH_LENGTH)
    }
}

/// The template store of a test, in a sandbox of its own, which is the store of the test's
/// commands, and the placeholder of its templates.
struct Templates {
    /// The test's sandbox.
    sandbox: Sandbox,
    /// The placeholder.
    placeholder: PathBuf,
    /// The identity that a guest would expect of the placeholder.
    identity: String,
}

impl Templates {
    /// Creates the sandbox of the test `name`, whose template store is `store` in it, and stages
    /// the placeholder.
    ///
    /// On Linux, the control socket of a capture's VM, `captures/C/transaction/control.sock` in
    /// the store, must fit the path of a Unix socket, so the tests' names and stores are short.
    fn new(name: &str) -> Self {
        let mut sandbox = Sandbox::new(name);
        sandbox.store = sandbox.root.join("store");
        let fixtures = Fixtures::find();
        let placeholder = stage(&sandbox.root.join("platform"), &fixtures);
        Self {
            sandbox,
            placeholder,
            identity: fixtures.identity,
        }
    }

    /// Returns the options of a creation or a refresh of `template`, which name its placeholder,
    /// and the network that it writes to a file of the test's, if any, and the processors, those
    /// online at boot, the RAM, its capacity, and the length of the scratch that the template
    /// names.
    fn options(&self, template: &Template) -> Vec<OsString> {
        let mut distro = OsString::from("erofs:");
        distro.push(&self.placeholder);
        let mut options = vec![
            OsString::from("--distro"),
            distro,
            OsString::from("--distro-identity"),
            OsString::from(&self.identity),
        ];
        let numbers = [
            ("--processors", template.processors.map(u64::from)),
            ("--boot-processors", template.boot_processors.map(u64::from)),
            ("--memory", template.memory.map(|memory| memory / MIB)),
            (
                "--memory-capacity",
                template.memory_capacity.map(|capacity| capacity / MIB),
            ),
            ("--scratch-length", template.scratch_length),
        ];
        for (option, value) in numbers {
            if let Some(value) = value {
                options.extend([OsString::from(option), OsString::from(value.to_string())]);
            }
        }
        if let Some(form) = template.network.form() {
            let file = self.sandbox.root.join("network.json");
            fs::write(&file, form.to_string()).unwrap();
            options.extend([OsString::from("--network"), file.into_os_string()]);
        }
        options
    }

    /// Creates `template` in the store, checks that the creation exits with `status`, and returns
    /// what it printed.
    fn create(&mut self, template: &Template, status: i32) -> Printed {
        let options = self.options(template);
        let options: Vec<&OsStr> = options.iter().map(OsString::as_os_str).collect();
        self.sandbox.create(&options, status)
    }

    /// Refreshes `template` in the store, checks that the refresh exits with `status`, and returns
    /// what it printed.
    fn refresh(&mut self, template: &Template, status: i32) -> Printed {
        let options = self.options(template);
        let options: Vec<&OsStr> = options.iter().map(OsString::as_os_str).collect();
        self.sandbox.refresh(&options, status)
    }

    /// Runs the host program's `command` on the store, with `arguments`, checks that it exits
    /// with `status`, and returns what it printed.
    fn run(&mut self, command: &str, arguments: &[&str], status: i32) -> Printed {
        let arguments: Vec<&OsStr> = arguments.iter().map(OsStr::new).collect();
        self.sandbox.run(command, &arguments, status)
    }

    /// Returns the template store, which a creation created.
    fn store(&self) -> Store {
        Store::open(&self.sandbox.store).unwrap()
    }

    /// Returns the slot of the class whose identity `class` printed.
    fn slot(&self, class: &str) -> PathBuf {
        self.sandbox
            .store
            .join(CAPTURES_NAME)
            .join(&class[..SLOT_NAME_LEN])
    }
}

/// Stages the placeholder of a test's templates in the directory `platform`, and returns it: a
/// copy of the fixtures' Ubuntu EROFS image, beside a copy of its manifest, which a creation
/// verifies.
///
/// A platform stages its trusted artifacts where no one else may change them, and NVX's capture
/// refuses a placeholder that anyone else may change. So the directory is private to the current
/// user, as NVX makes a sandbox's store, whose access control list on Windows passes on to the
/// files created in it, and each copy has the mode 0644 on Unix, whatever the umask.
fn stage(platform: &Path, fixtures: &Fixtures) -> PathBuf {
    SandboxStore::create(platform).unwrap();
    let [placeholder, _] = [&fixtures.distro, &fixtures.manifest].map(|source| {
        let copy = platform.join(source.file_name().unwrap());
        io::copy(
            &mut File::open(source).unwrap(),
            &mut File::create(&copy).unwrap(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&copy, fs::Permissions::from_mode(0o644)).unwrap();
        }
        copy
    });
    placeholder
}

/// Returns `parameter` as the command line spells it: its name, then `=` and its value, if it has
/// one.
fn token(parameter: &Parameter<'_>) -> Vec<u8> {
    let mut token = parameter.name.to_vec();
    if let Some(value) = parameter.value {
        token.push(b'=');
        token.extend_from_slice(value);
    }
    token
}

/// Returns the names of the entries of the directory `path`, sorted.
fn names(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Checks that the store of `templates` captured `template`, as the creation that printed
/// `created` says, and published it as its one generation, as the
/// [module documentation](self) describes.
fn assert_captured(templates: &Templates, created: &Printed, template: &Template) {
    assert_eq!(created["created"], "captured", "{created:?}");
    assert_eq!(created["generation"], "1", "{created:?}");
    let store = templates.store();
    let index = store.index().unwrap();
    let [generation] = index.generations() else {
        panic!("the store indexes {:?}", index.generations());
    };
    assert_eq!(generation.generation().to_string(), "1");
    assert_eq!(generation.key().to_string(), created["key"]);
    assert_eq!(generation.class().to_string(), created["class"]);
    let key = store.read_key(generation).unwrap();
    assert_eq!(key.identity(), generation.key());

    // The runtime of the adapter's VMs, with the guest agent's build, and the machine that the
    // scenario asked for.
    let artifacts = env::var_os(Artifacts::ENV_DIR).unwrap();
    let runtime = Runtime::from_dir(artifacts, GUEST_BUILD).unwrap();
    assert_eq!(key.runtime(), &runtime.identity);
    assert_eq!(
        key.guest_build().as_str(),
        format!("runtime-adapter-guest/{GUEST_BUILD}")
    );
    assert_eq!(key.hypervisor(), Hypervisor::from_env_or_default().unwrap());
    let processors = template.processors();
    assert_eq!(key.processors(), processors);
    // A template whose processors are all online at boot records none, and a restore brings
    // none online; one that boots with fewer records them, and a restore may bring the others
    // online.
    let boot = template.boot_processors();
    let limited = boot < processors;
    assert_eq!(key.boot_processors(), if limited { boot } else { 0 });
    assert_eq!(key.memory(), template.memory());
    assert_eq!(key.memory_capacity(), template.memory_capacity());
    let placeholder = fs::metadata(&templates.placeholder).unwrap().len();
    let distro = key.distro();
    assert_eq!(
        (distro.length, distro.logical_block_size),
        (placeholder, LOGICAL_BLOCK_SIZE)
    );
    let scratch = key.scratch();
    assert_eq!(
        (scratch.length, scratch.logical_block_size),
        (template.scratch_length(), LOGICAL_BLOCK_SIZE)
    );
    assert_eq!(key.has_network(), template.network.has_nic());

    // The kernel command line, which every clone restores: the platform tier's tokens alone,
    // which OpenVMM's capture admits, with the processors, those online at boot when they are
    // fewer, and the NIC's addresses exactly when the template has a NIC, but neither a DNS server
    // nor a publication of the guest's listeners, which OpenVMM names to no guest under the NIC's
    // complete deny-all policy, nor the placeholder's identity, nor a root's tokens.
    let command_line = key.command_line();
    let tokens: Vec<Vec<u8>> = cmdline::parameters(command_line.as_bytes())
        .map(|parameter| token(&parameter))
        .collect();
    for token in &tokens {
        assert!(
            is_platform_token(token),
            "{} in {command_line}",
            token.escape_ascii()
        );
    }
    for expected in [
        "nvx_sandbox=1".to_owned(),
        "nvx_snapshot_tier=platform".to_owned(),
        format!("nr_cpus={processors}"),
    ] {
        assert!(
            tokens.iter().any(|token| token == expected.as_bytes()),
            "{expected} in {command_line}"
        );
    }
    let limits: Vec<&Vec<u8>> = tokens
        .iter()
        .filter(|token| token.starts_with(b"maxcpus="))
        .collect();
    let expected = format!("maxcpus={boot}");
    match limits.as_slice() {
        [] => assert!(!limited, "{command_line}"),
        [limit] => assert!(limited && **limit == expected.as_bytes(), "{command_line}"),
        _ => panic!("{command_line} limits the processors online at boot more than once"),
    }
    for address in ["virtnet_ip=", "virtnet_gw="] {
        assert_eq!(
            tokens
                .iter()
                .any(|token| token.starts_with(address.as_bytes())),
            template.network.has_nic(),
            "{address} in {command_line}"
        );
    }
    for absent in ["virtnet_dns=", "virtnet_ingress="] {
        assert!(
            !tokens
                .iter()
                .any(|token| token.starts_with(absent.as_bytes())),
            "{absent} in {command_line}"
        );
    }
    for tenant in [templates.identity.as_str(), "nvx_layer", "nvx_scratch"] {
        assert!(!command_line.contains(tenant), "{tenant} in {command_line}");
    }

    // The payload: the record of the capture, the key, and the snapshot, as OpenVMM published it.
    let payload = templates
        .sandbox
        .store
        .join(GENERATIONS_NAME)
        .join(generation.generation().to_string());
    assert_eq!(
        names(&payload),
        [CAPTURE_RECORD_NAME, KEY_NAME, SNAPSHOT_NAME]
    );
    let snapshot = store.snapshot_dir(generation).unwrap();
    assert_eq!(snapshot, payload.join(SNAPSHOT_NAME));
    assert!(snapshot.join(MANIFEST_NAME).is_file());

    // The class's slot holds no transaction once the capture published it, and records no launch
    // once its VM ended.
    let slot = templates.slot(&created["class"]);
    assert!(!slot.join(TRANSACTION_NAME).exists());
    let slot = SandboxStore::open(&slot).unwrap();
    assert!(slot.journal().unwrap().is_none());
    assert!(slot.runtime().unwrap().is_none());
}

/// Creates `template` in a template store of its own, in the sandbox of the test `name`, checks
/// that the store captured it and published it, and that a second creation of its class serves
/// that generation, without a capture.
fn captures(name: &str, template: Template) {
    let mut templates = Templates::new(name);
    let created = templates.create(&template, 0);
    assert_captured(&templates, &created, &template);
    let log = templates.slot(&created["class"]).join(LOG_NAME);
    let captured = fs::read(&log).unwrap();

    let existing = templates.create(&template, 0);
    assert_eq!(existing["created"], "existing", "{existing:?}");
    for printed in ["generation", "key", "class"] {
        assert_eq!(existing[printed], created[printed], "{printed}");
    }
    assert_eq!(templates.store().index().unwrap().generations().len(), 1);
    // No VMM ran since the capture's, whose log the slot keeps.
    assert_eq!(fs::read(&log).unwrap(), captured);
}

#[test]
#[ignore = "boots a VM; see the module documentation for what it needs"]
fn a_template_without_a_network_has_no_nic() {
    // One of its processors is online at boot, the host program's default, of which a restore
    // may bring the other online, and a restore may expand the RAM to its capacity.
    let template = Template {
        processors: Some(2),
        memory: Some(2 * MEMORY_BLOCK_SIZE),
        memory_capacity: Some(4 * MEMORY_BLOCK_SIZE),
        scratch_length: Some(32 * MIB),
        ..Template::new(Network::None)
    };
    captures("tpl-none", template);
}

#[test]
#[ignore = "boots a VM; see the module documentation for what it needs"]
fn a_template_whose_processors_are_all_online_at_boot_limits_none() {
    let template = Template {
        processors: Some(2),
        boot_processors: Some(2),
        ..Template::new(Network::None)
    };
    captures("tpl-online", template);
}

#[test]
#[ignore = "boots a VM; see the module documentation for what it needs"]
fn a_network_that_lets_nothing_through_has_no_nic() {
    captures("tpl-deny", Template::new(Network::Deny));
}

#[test]
#[ignore = "boots a VM; see the module documentation for what it needs"]
fn the_networks_of_every_policy_at_one_guest_s_address_share_one_template() {
    // A creation of a network that would publish the guest's listeners, and forwards a port of
    // host loopback, captures a template whose NIC its capture attached under the complete
    // deny-all policy.
    let mut templates = Templates::new("tpl-nic");
    let template = Template::new(Network::Forward);
    let created = templates.create(&template, 0);
    assert_captured(&templates, &created, &template);
    let log = templates.slot(&created["class"]).join(LOG_NAME);
    let captured = fs::read(&log).unwrap();

    // A creation of each network that lets something through, at the same guest's address, finds
    // the same class, whatever its policy, and serves the generation without a capture.
    for network in Network::WITH_NIC {
        let existing = templates.create(&Template::new(network), 0);
        assert_eq!(existing["created"], "existing", "{network:?}: {existing:?}");
        for printed in ["generation", "key", "class"] {
            assert_eq!(
                existing[printed], created[printed],
                "{network:?}: {printed}"
            );
        }
    }
    assert_eq!(templates.store().index().unwrap().generations().len(), 1);
    // No VMM ran since the capture's, whose log the slot keeps.
    assert_eq!(fs::read(&log).unwrap(), captured);
}

#[test]
#[ignore = "needs NVX's artifacts; see the module documentation"]
fn a_class_that_no_capture_takes_is_refused_before_the_store() {
    // Processors that a microVM cannot have, and more online at boot than in all.
    for (processors, boot_processors) in [(3, None), (2, Some(4))] {
        let mut templates = Templates::new("tpl-class");
        let template = Template {
            processors: Some(processors),
            boot_processors,
            ..Template::new(Network::None)
        };
        templates.create(&template, FAILED);
        let stderr = templates.sandbox.stderr();
        assert!(stderr.contains("needs 1, 2, 4, or 8 of each"), "{stderr}");
        assert!(!templates.sandbox.store.exists());
    }
}

/// Returns the store generation `number`.
fn generation(number: u64) -> StoreGeneration {
    StoreGeneration::new(NonZeroU64::new(number).unwrap())
}

/// Leases the generation `number` of the store of `templates` to the clone of the sandbox `name`,
/// a directory of the test's own, which it provisions, as a clone's start leases one, but without
/// launching anything, and returns the lease.
fn lease(templates: &Templates, number: u64, name: &str) -> Lease {
    let sandbox = templates.sandbox.root.join(name);
    SandboxStore::create(&sandbox).unwrap();
    let now = HostTime::now().unwrap();
    let deadline = Some(Instant::now() + LOCK_TIMEOUT);
    // The acquisition returns the sandbox's lock too, which the lease outlasts.
    templates
        .store()
        .acquire(
            generation(number),
            &sandbox,
            &CallerFiles::none(),
            &now,
            deadline,
        )
        .unwrap()
        .lease
}

/// Releases `lease`, as the teardown of its clone does once the clone's VM has exited, under the
/// lock of its sandbox, which it takes through the lease's path.
fn release(templates: &Templates, lease: &Lease) {
    let sandbox = SandboxStore::open(lease.sandbox()).unwrap();
    let mut locked = sandbox.lock(Some(Instant::now() + LOCK_TIMEOUT)).unwrap();
    let released = templates.store().release(&mut locked).unwrap();
    assert_eq!(released.as_ref(), Some(lease));
}

/// Returns `leases` in the order of their identities, in which the store lists them.
fn ordered<'a>(leases: &[&'a Lease]) -> Vec<&'a Lease> {
    let mut leases = leases.to_vec();
    leases.sort_by_key(|lease| lease.id());
    leases
}

/// Returns the identities of `leases`, separated by commas, or `none` without one, as the host
/// program prints them.
fn lease_ids(leases: &[&Lease]) -> String {
    let ids: Vec<String> = ordered(leases)
        .iter()
        .map(|lease| lease.id().to_string())
        .collect();
    if ids.is_empty() {
        "none".to_owned()
    } else {
        ids.join(",")
    }
}

/// Checks that `printed` holds the sandbox and the launch of each of `leases`, as `inspect` and
/// `remove` print them.
fn assert_leases(printed: &Printed, leases: &[&Lease]) {
    for lease in leases {
        let id = lease.id();
        assert_eq!(
            printed[format!("lease.{id}.sandbox").as_str()],
            escape(lease.sandbox().to_str().unwrap()),
            "{printed:?}"
        );
        assert_eq!(
            printed[format!("lease.{id}.launch").as_str()],
            lease.launch().to_string(),
            "{printed:?}"
        );
    }
}

/// Checks that the store of `templates` indexes exactly the generations `numbers`, and holds their
/// payloads alone.
fn assert_held(templates: &Templates, numbers: &[u64]) {
    let index = templates.store().index().unwrap();
    let indexed: Vec<u64> = index
        .generations()
        .iter()
        .map(|generation| generation.generation().get().get())
        .collect();
    assert_eq!(indexed, numbers);
    let mut payloads: Vec<u64> = names(&templates.sandbox.store.join(GENERATIONS_NAME))
        .iter()
        .map(|name| name.parse().unwrap())
        .collect();
    payloads.sort_unstable();
    assert_eq!(payloads, numbers);
}

/// Checks that `printed`, what a creation or a refresh printed, says that its eviction retired
/// the generations `retired`, and left the generations `draining` to their leases.
fn assert_evicted(printed: &Printed, retired: &str, draining: &str) {
    assert_eq!(printed["retired"], retired, "{printed:?}");
    assert_eq!(printed["draining"], draining, "{printed:?}");
}

/// Checks that what `printed` says of the generation `number` of the store of `templates`, each
/// key after `prefix`, as `list` and `inspect` print it, is what the store records: its key, its
/// class, the state `state`, its eligibility at the host's time, and the leases `leases`.
fn assert_entry(
    templates: &Templates,
    printed: &Printed,
    prefix: &str,
    number: u64,
    state: &str,
    leases: &[&Lease],
) {
    let index = templates.store().index().unwrap();
    let indexed = index.get(generation(number)).unwrap();
    let value = |key: &str| printed[format!("{prefix}{key}").as_str()].clone();
    assert_eq!(value("key"), indexed.key().to_string(), "{printed:?}");
    assert_eq!(value("class"), indexed.class().to_string(), "{printed:?}");
    assert_eq!(value("state"), state, "{printed:?}");
    // The capture just happened, on this host.
    assert_eq!(value("eligibility"), "eligible", "{printed:?}");
    let reason = value("reason");
    assert!(
        reason.starts_with("eligible, after a downtime of ")
            && reason.ends_with(" ns by the host's monotonic clock"),
        "{reason}"
    );
    assert_eq!(value("leases"), lease_ids(leases), "{printed:?}");
}

/// Checks that `listed`, what `list` printed, reports the generations of the store of `templates`,
/// each the store generation, the state, and the leases of one of `expected`, and nothing else.
fn assert_listed(templates: &Templates, listed: &Printed, expected: &[(u64, &str, &[&Lease])]) {
    assert_eq!(
        listed["store"],
        templates.store().id().to_string(),
        "{listed:?}"
    );
    let numbers: Vec<String> = expected
        .iter()
        .map(|(number, ..)| number.to_string())
        .collect();
    let generations = if numbers.is_empty() {
        "none".to_owned()
    } else {
        numbers.join(",")
    };
    assert_eq!(listed["generations"], generations, "{listed:?}");
    for (number, state, leases) in expected {
        let prefix = format!("generation.{number}.");
        assert_entry(templates, listed, &prefix, *number, state, leases);
    }
    // The store and its generations, then six keys of each.
    assert_eq!(listed.len(), 2 + 6 * expected.len(), "{listed:?}");
}

/// Checks that `inspected`, what `inspect` printed of the generation `number` of the store of
/// `templates`, reports it as the store records it, with the state `state` and the leases
/// `leases`, and the metadata of its capture, and nothing else.
fn assert_inspected(
    templates: &Templates,
    inspected: &Printed,
    number: u64,
    state: &str,
    leases: &[&Lease],
) {
    let store = templates.store();
    assert_eq!(inspected["store"], store.id().to_string(), "{inspected:?}");
    assert_eq!(inspected["generation"], number.to_string(), "{inspected:?}");
    assert_entry(templates, inspected, "", number, state, leases);
    assert_leases(inspected, leases);
    let index = store.index().unwrap();
    let indexed = index.get(generation(number)).unwrap();
    let time = indexed.time();
    let host: String = time
        .host_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let now = HostTime::now().unwrap();
    // The capture's time contract names this host, its boot, and its clock.
    assert_eq!(time.host_id, now.host_id);
    assert_eq!(time.boot_id, now.boot_id);
    for (key, value) in [
        ("storage", indexed.storage_generation().to_string()),
        ("time.host", host),
        ("time.boot", time.boot_id.to_string()),
        ("time.clock", now.clock.as_str().to_owned()),
        ("time.utc", time.capture_utc_ns.to_string()),
        ("time.monotonic", time.capture_monotonic_ns.to_string()),
        ("time.lineage", time.lineage.to_string()),
        ("length.state", indexed.state_len().to_string()),
        ("length.memory", indexed.memory_len().to_string()),
    ] {
        assert_eq!(inspected[key], value, "{key}: {inspected:?}");
    }
    // The store and the generation, six keys of its entry, two of each lease, and nine of the
    // capture's metadata.
    assert_eq!(
        inspected.len(),
        2 + 6 + 2 * leases.len() + 9,
        "{inspected:?}"
    );
}

/// Checks that `removed`, what `remove` printed, says `outcome`, `retired` or `draining`, with
/// the leases `leases` that keep a draining generation, and nothing else.
fn assert_removed(removed: &Printed, outcome: &str, leases: &[&Lease]) {
    assert_eq!(removed["removed"], outcome, "{removed:?}");
    if outcome == "draining" {
        assert_eq!(removed["leases"], lease_ids(leases), "{removed:?}");
        assert_leases(removed, leases);
        assert_eq!(removed.len(), 2 + 2 * leases.len(), "{removed:?}");
    } else {
        assert!(leases.is_empty());
        assert_eq!(removed.len(), 1, "{removed:?}");
    }
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_refresh_captures_and_retires_the_generations_that_it_supersedes() {
    let mut templates = Templates::new("tpl-refresh");
    let template = Template::new(Network::None);
    let created = templates.create(&template, 0);
    assert_captured(&templates, &created, &template);
    assert_evicted(&created, "none", "none");

    // The store's one generation, which no clone leases, as the store records it.
    let listed = templates.run("list", &[], 0);
    assert_listed(&templates, &listed, &[(1, "available", &[])]);
    let inspected = templates.run("inspect", &["1"], 0);
    assert_inspected(&templates, &inspected, 1, "available", &[]);

    // A refresh captures a new generation of the class, although the store holds an eligible one,
    // and retires the one that it supersedes, which no lease keeps.
    let refreshed = templates.refresh(&template, 0);
    assert_eq!(refreshed["created"], "captured", "{refreshed:?}");
    assert_eq!(refreshed["generation"], "2", "{refreshed:?}");
    assert_eq!(refreshed["class"], created["class"], "{refreshed:?}");
    assert_evicted(&refreshed, "1", "none");
    assert_held(&templates, &[2]);
    templates.run("inspect", &["1"], FAILED);
    let stderr = templates.sandbox.stderr();
    assert!(
        stderr.contains("no longer holds a generation 1"),
        "{stderr}"
    );

    // A refresh that supersedes a generation that a clone leases leaves it, tombstoned, with its
    // payload, to the clone.
    let leased = lease(&templates, 2, "clone");
    let refreshed = templates.refresh(&template, 0);
    assert_eq!(refreshed["generation"], "3", "{refreshed:?}");
    assert_evicted(&refreshed, "none", "2");
    assert_held(&templates, &[2, 3]);
    let listed = templates.run("list", &[], 0);
    assert_listed(
        &templates,
        &listed,
        &[(2, "tombstoned", &[&leased]), (3, "available", &[])],
    );
    let inspected = templates.run("inspect", &["2"], 0);
    assert_inspected(&templates, &inspected, 2, "tombstoned", &[&leased]);

    // Once the clone released it, the next creation of the class serves the latest generation,
    // without a capture, and its eviction deletes the one that drained.
    release(&templates, &leased);
    let existing = templates.create(&template, 0);
    assert_eq!(existing["created"], "existing", "{existing:?}");
    assert_eq!(existing["generation"], "3", "{existing:?}");
    assert_evicted(&existing, "2", "none");
    assert_held(&templates, &[3]);
    assert!(templates.store().tombstones().unwrap().is_empty());
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_removal_reports_busy_while_leases_keep_the_generation() {
    let mut templates = Templates::new("tpl-remove");
    let template = Template::new(Network::None);
    let created = templates.create(&template, 0);
    assert_captured(&templates, &created, &template);
    let first = lease(&templates, 1, "first");
    let second = lease(&templates, 1, "second");
    let leases = ordered(&[&first, &second]);
    let listed = templates.run("list", &[], 0);
    assert_listed(&templates, &listed, &[(1, "available", &leases)]);
    let inspected = templates.run("inspect", &["1"], 0);
    assert_inspected(&templates, &inspected, 1, "available", &leases);

    // The removal tombstones the generation first, and reports busy, with the leases that keep
    // it, and its payload, in the store.
    let removed = templates.run("remove", &["1"], BUSY);
    assert_removed(&removed, "draining", &leases);
    let store = templates.store();
    assert_eq!(store.tombstones().unwrap(), [generation(1)]);
    assert_held(&templates, &[1]);
    let index = store.index().unwrap();
    let indexed = index.get(generation(1)).unwrap();
    let snapshot = store.snapshot_dir(indexed).unwrap();
    assert!(snapshot.join(MANIFEST_NAME).is_file());
    let listed = templates.run("list", &[], 0);
    assert_listed(&templates, &listed, &[(1, "tombstoned", &leases)]);

    // The generation takes no new lease, and no selection takes it, while its leases keep
    // working.
    let sandbox = templates.sandbox.root.join("third");
    SandboxStore::create(&sandbox).unwrap();
    let now = HostTime::now().unwrap();
    let deadline = Some(Instant::now() + LOCK_TIMEOUT);
    let refused = store
        .acquire(
            generation(1),
            &sandbox,
            &CallerFiles::none(),
            &now,
            deadline,
        )
        .unwrap_err();
    assert_eq!(refused.code(), ErrorCode::TemplateTombstoned, "{refused}");
    assert_eq!(store.select(indexed.key(), &now).unwrap(), None);

    // A creation of the class then captures a new generation, which the store holds no eligible
    // one of, and leaves the tombstoned one to its leases.
    let recreated = templates.create(&template, 0);
    assert_eq!(recreated["created"], "captured", "{recreated:?}");
    assert_eq!(recreated["generation"], "2", "{recreated:?}");
    assert_evicted(&recreated, "none", "1");
    assert_held(&templates, &[1, 2]);

    // The removal reports busy while any lease remains, and deletes the generation once the last
    // one is released.
    release(&templates, &first);
    let removed = templates.run("remove", &["1"], BUSY);
    assert_removed(&removed, "draining", &[&second]);
    assert_held(&templates, &[1, 2]);
    release(&templates, &second);
    let removed = templates.run("remove", &["1"], 0);
    assert_removed(&removed, "retired", &[]);
    assert_held(&templates, &[2]);
    assert!(templates.store().tombstones().unwrap().is_empty());

    // A removal of a generation that the store no longer holds finds it retired already, and one
    // of a generation that the store never published fails.
    let removed = templates.run("remove", &["1"], 0);
    assert_removed(&removed, "retired", &[]);
    templates.run("remove", &["3"], FAILED);
    let stderr = templates.sandbox.stderr();
    assert!(stderr.contains("holds no generation 3"), "{stderr}");

    // A generation that no lease keeps goes at once.
    let removed = templates.run("remove", &["2"], 0);
    assert_removed(&removed, "retired", &[]);
    assert_held(&templates, &[]);
    let listed = templates.run("list", &[], 0);
    assert_listed(&templates, &listed, &[]);
}

#[test]
#[ignore = "needs NVX's artifacts; see the module documentation"]
fn a_refresh_refuses_a_class_that_no_capture_takes_before_the_store() {
    let mut templates = Templates::new("tpl-rclass");
    let template = Template {
        processors: Some(3),
        ..Template::new(Network::None)
    };
    templates.refresh(&template, FAILED);
    let stderr = templates.sandbox.stderr();
    assert!(stderr.contains("needs 1, 2, 4, or 8 of each"), "{stderr}");
    assert!(!templates.sandbox.store.exists());
}
