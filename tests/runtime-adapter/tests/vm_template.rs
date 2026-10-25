//! The host program captures templates on a real hypervisor, through public APIs only: VMs of the
//! guest agent's template mode, which NVX's template store captures through NVX's supervisor and
//! publishes as its generations, as `runtime-adapter-host create` asks.
//!
//! Each scenario stages the placeholder of its templates, a copy of the fixtures' Ubuntu EROFS
//! image with its manifest, as a platform stages its trusted artifacts, and creates a [`Template`]
//! of it, with a [`Network`], or without one, in a template store of its own. A template that the
//! store captured is the store's one generation, whose payload holds the record of its capture,
//! its key, and its snapshot, and whose key holds what the scenario asked for: the runtime of the
//! adapter's VMs, with the guest agent's build, the backend, the processors, all online at boot,
//! the RAM and its capacity, the geometries of the placeholder and of the dormant scratch, a NIC
//! exactly when the network lets something through, and a kernel command line of the platform
//! tier's tokens alone, which names neither the placeholder's identity nor a root. The slot of the
//! template's class then holds no transaction, and records no launch, and a second creation serves
//! the generation without a capture. NVX refuses a network whose policy no template takes before
//! any VM starts, and the host program refuses a class that no capture takes before it creates the
//! store.
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
use std::path::{Path, PathBuf};

use nvx::Hypervisor;
use nvx::host::artifacts::Artifacts;
use nvx::host::store::{LOG_NAME, SandboxStore};
use nvx::host::template::{
    CAPTURE_RECORD_NAME, CAPTURES_NAME, GENERATIONS_NAME, KEY_NAME, MANIFEST_NAME,
    MEMORY_BLOCK_SIZE, SNAPSHOT_NAME, Store, TRANSACTION_NAME,
};
use nvx::protocol::cmdline::{self, Parameter};
use runtime_adapter::GUEST_BUILD;
use runtime_adapter::capture::{DEFAULT_SCRATCH_LENGTH, LOGICAL_BLOCK_SIZE};
use runtime_adapter::runtime::Runtime;
use runtime_adapter::template::is_platform_token;
use serde_json::{Value, json};

use common::{Fixtures, Printed, Sandbox};

/// The exit status of the host program's command that failed, after a diagnostic.
const FAILED: i32 = 1;
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

/// The network of a scenario's template, which the creation reads from a file that the test
/// writes, in the serde form of a plan's network: the guest at [`GUEST_NETWORK`], with egress
/// denied and no host-loopback access unless the network says otherwise.
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
    /// Ingress allowed, which no template takes: OpenVMM would publish the guest's listeners.
    Ingress,
    /// Host-loopback access in both directions, which no template takes either.
    HostLoopback,
}

impl Network {
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
        }
        Some(form)
    }

    /// Returns whether the template has a NIC: whether the network lets anything through, for
    /// which OpenVMM attaches a network device.
    fn has_nic(self) -> bool {
        !matches!(self, Self::None | Self::Deny)
    }
}

/// A template that a scenario creates: its network, and the processors, the RAM, the RAM
/// capacity, and the length of the dormant scratch that the creation names, or else the host
/// program's defaults.
#[derive(Clone, Copy, Debug)]
struct Template {
    /// The network.
    network: Network,
    /// The processors, all online at boot, or 1 by default.
    processors: Option<u32>,
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
            memory: None,
            memory_capacity: None,
            scratch_length: None,
        }
    }

    /// Returns the processors of the template.
    fn processors(&self) -> u32 {
        self.processors.unwrap_or(1)
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

    /// Creates `template` in the store, checks that the creation exits with `status`, and returns
    /// what it printed.
    fn create(&mut self, template: &Template, status: i32) -> Printed {
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
        let options: Vec<&OsStr> = options.iter().map(OsString::as_os_str).collect();
        self.sandbox.create(&options, status)
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
    // Every processor is online at boot, so a restore activates none.
    assert_eq!(key.boot_processors(), 0);
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
    // which OpenVMM's capture admits, with the processors, and the NIC's addresses exactly when
    // the template has a NIC, but neither the placeholder's identity nor a root's tokens.
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
    assert_eq!(
        tokens.iter().any(|token| token.starts_with(b"virtnet_ip=")),
        template.network.has_nic(),
        "{command_line}"
    );
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

/// Creates `template` in a template store of its own, in the sandbox of the test `name`, and
/// checks that NVX refuses its network with `problem` before any VM starts: the store indexes no
/// generation, and no VMM ran in any slot.
fn refuses(name: &str, template: Template, problem: &str) {
    let mut templates = Templates::new(name);
    templates.create(&template, FAILED);
    let stderr = templates.sandbox.stderr();
    assert!(stderr.contains(problem), "{stderr}");
    assert!(templates.store().index().unwrap().generations().is_empty());
    let slots = fs::read_dir(templates.sandbox.store.join(CAPTURES_NAME))
        .into_iter()
        .flatten();
    for slot in slots {
        let slot = slot.unwrap().path();
        for name in [LOG_NAME, TRANSACTION_NAME] {
            assert!(!slot.join(name).exists(), "{}", slot.display());
        }
    }
}

#[test]
#[ignore = "boots a VM; see the module documentation for what it needs"]
fn a_template_without_a_network_has_no_nic() {
    // Every processor is online at boot, and a restore may expand the RAM to its capacity.
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
fn a_network_that_lets_nothing_through_has_no_nic() {
    captures("tpl-deny", Template::new(Network::Deny));
}

#[test]
#[ignore = "boots a VM; see the module documentation for what it needs"]
fn an_egress_network_has_its_nic() {
    captures("tpl-egress", Template::new(Network::Egress));
}

#[test]
#[ignore = "boots a VM; see the module documentation for what it needs"]
fn an_allow_rule_has_its_nic() {
    captures("tpl-rule", Template::new(Network::AllowRule));
}

#[test]
#[ignore = "boots a VM; see the module documentation for what it needs"]
fn outbound_host_loopback_has_its_nic() {
    captures("tpl-outbound", Template::new(Network::Outbound));
}

#[test]
#[ignore = "boots a VM; see the module documentation for what it needs"]
fn a_proxy_only_network_has_its_nic() {
    captures("tpl-proxy", Template::new(Network::Proxy));
}

#[test]
#[ignore = "needs NVX's artifacts; see the module documentation"]
fn ingress_is_refused_before_any_vm_starts() {
    refuses(
        "tpl-ingress",
        Template::new(Network::Ingress),
        "a template's network policy cannot allow ingress",
    );
}

#[test]
#[ignore = "needs NVX's artifacts; see the module documentation"]
fn host_loopback_both_ways_is_refused_before_any_vm_starts() {
    refuses(
        "tpl-loopback",
        Template::new(Network::HostLoopback),
        "a template's network policy cannot allow host-loopback access both ways",
    );
}

#[test]
#[ignore = "needs NVX's artifacts; see the module documentation"]
fn a_class_that_no_capture_takes_is_refused_before_the_store() {
    let mut templates = Templates::new("tpl-class");
    let template = Template {
        processors: Some(3),
        ..Template::new(Network::None)
    };
    templates.create(&template, FAILED);
    let stderr = templates.sandbox.stderr();
    assert!(stderr.contains("needs 1, 2, 4, or 8 of each"), "{stderr}");
    assert!(!templates.sandbox.store.exists());
}
