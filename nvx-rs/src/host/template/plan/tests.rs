//! Tests of the plans of templates' captures: the lowering of adapters' plans, the rendered
//! arguments, the kernel command line against OpenVMM's platform allowlist, and the refusals.

use std::path::Path;

use serde_json::json;

use super::*;
use crate::ErrorKind;
use crate::host::network::{
    Access, EgressRule, ForwardProtocol, HostLoopback, LoopbackForward, NetworkPolicy,
    NetworkProxy, Protocol,
};
use crate::host::sandbox::distro::{BINDING_VERSION, DISTRO_POLICY_REVISION};
use crate::host::sandbox::{SandboxDistro, SandboxNetwork, SandboxSettings};
use crate::host::seal::SEAL_VERSION;
use crate::protocol::cmdline;
use crate::protocol::root::RootImage;
use crate::uuid::Uuid;

/// OpenVMM's platform allowlist of kernel command-line tokens, compiled from the copies of its
/// source in `tests/vectors/openvmm/`, which an ignored test of `tests/openvmm_vectors.rs`
/// compares with the OpenVMM submodule.
#[allow(unreachable_pub, clippy::missing_docs_in_private_items)]
mod openvmm {
    /// OpenVMM's `openvmm_defs` crate, of which the allowlist reads one constant.
    mod openvmm_defs {
        /// OpenVMM's `openvmm_defs::microvm`.
        pub(super) mod microvm {
            include!("../../../../tests/vectors/openvmm/microvm_control_tty_command_line.rs");
        }
    }

    include!("../../../../tests/vectors/openvmm/platform_command_line_token_is_invariant.rs");

    /// Returns whether OpenVMM's platform allowlist admits `token`.
    pub(super) fn admits(token: &str) -> bool {
        platform_command_line_token_is_invariant(token)
    }
}

/// Bytes in a GiB.
const GIB: u64 = 1024 * MIB;

/// The identity of the placeholder.
const IDENTITY: &str = "11111111-1111-1111-1111-111111111111";

/// The placeholder.
const PLACEHOLDER: &str = if cfg!(windows) {
    r"C:\platform\placeholder.img"
} else {
    "/platform/placeholder.img"
};

/// A clone's scratch, which an adapter's plan names as its upper.
const CLONE_SCRATCH: &str = if cfg!(windows) {
    r"C:\sandboxes\tenant-a\scratch.ext4"
} else {
    "/sandboxes/tenant-a/scratch.ext4"
};

/// The capture's transaction directory.
const TRANSACTION: &str = if cfg!(windows) {
    r"C:\templates\tx-1"
} else {
    "/templates/tx-1"
};

/// The dormant scratch device's file in [`TRANSACTION`].
const SCRATCH: &str = if cfg!(windows) {
    r"C:\templates\tx-1\scratch.raw"
} else {
    "/templates/tx-1/scratch.raw"
};

/// The snapshot's destination in [`TRANSACTION`].
const DESTINATION: &str = if cfg!(windows) {
    r"C:\templates\tx-1\snapshot"
} else {
    "/templates/tx-1/snapshot"
};

/// The text of [`generation`].
const GENERATION: &str = "000102030405060708090a0b0c0d0e0f";

/// The options of templates' arguments, each of which takes one value.
const OPTIONS: [&str; 14] = [
    "--machine",
    "--microvm-sandbox-block",
    "--net",
    "--network-profile",
    "--network-egress",
    "--network-ingress",
    "--host-loopback",
    "--processors",
    "--memory",
    "--memory-capacity",
    "--snapshot-destination",
    "--snapshot-tier",
    "--snapshot-block-identity",
    "--snapshot-generation-id",
];

/// Returns the EROFS image of the placeholder.
fn erofs() -> RootImage {
    RootImage::erofs(Some(Uuid::parse(IDENTITY).unwrap()))
}

/// Returns the GPT image of the placeholder.
fn gpt() -> RootImage {
    RootImage::gpt(Some(Uuid::parse(IDENTITY).unwrap()))
}

/// Returns the binding of a placeholder of `format` with `identity`, of `length` bytes.
fn binding_of(format: &str, identity: &str, length: u64) -> DistroBinding {
    serde_json::from_value(json!({
        "version": BINDING_VERSION,
        "format": format,
        "identity": identity,
        "image": {
            "version": SEAL_VERSION,
            "scheme": "linux",
            "device": 2049,
            "fileId": "0000000000000000000000000004a2f1",
            "kind": "regular",
            "length": length,
            "modified": {"seconds": 1_792_000_000, "nanoseconds": 5},
            "changed": {"seconds": 1_792_000_000, "nanoseconds": 5},
        },
        "manifest": null,
        "policy": {"revision": DISTRO_POLICY_REVISION, "result": "accepted"},
    }))
    .unwrap()
}

/// Returns the binding of the EROFS placeholder of 1 GiB.
fn binding() -> DistroBinding {
    binding_of("erofs", IDENTITY, GIB)
}

/// Returns an adapter's plan of the placeholder as `image` over `upper`, with `network`.
fn adapter_with(
    image: RootImage,
    upper: SandboxUpper,
    network: Option<SandboxNetwork>,
) -> SandboxPlan {
    adapter_at(PLACEHOLDER, image, upper, network)
}

/// Returns an adapter's plan of the placeholder at `path` as `image` over `upper`, with
/// `network`.
fn adapter_at(
    path: &str,
    image: RootImage,
    upper: SandboxUpper,
    network: Option<SandboxNetwork>,
) -> SandboxPlan {
    SandboxPlan::adapter(SandboxDistro::new(path, image), upper).with_network(network)
}

/// Returns an adapter's plan of the EROFS placeholder over a clone's scratch, with `network`.
fn adapter(network: Option<SandboxNetwork>) -> SandboxPlan {
    adapter_with(erofs(), SandboxUpper::scratch(CLONE_SCRATCH), network)
}

/// Returns the network of a guest at `10.0.0.2/24` under `policy`.
fn network(policy: NetworkPolicy) -> SandboxNetwork {
    SandboxNetwork::new("10.0.0.2/24", policy).unwrap()
}

/// Returns a policy that allows egress, and host-loopback access from the guest alone.
fn outbound() -> NetworkPolicy {
    let mut policy = NetworkPolicy::new(Access::Allow);
    policy.host_loopback = HostLoopback::Outbound;
    policy
}

/// Returns a policy that allows ingress.
fn listening() -> NetworkPolicy {
    let mut policy = NetworkPolicy::new(Access::Deny);
    policy.ingress = Access::Allow;
    policy
}

/// Returns a policy that allows egress, and host-loopback access both ways, with a forward of a
/// TCP port of host loopback to the guest.
fn forwarded() -> NetworkPolicy {
    let mut policy = NetworkPolicy::new(Access::Allow);
    policy.host_loopback = HostLoopback::Allow;
    policy.host_loopback_forwards.push(LoopbackForward {
        protocol: ForwardProtocol::Tcp,
        host_port: 18080,
        guest_port: 8080,
    });
    policy
}

/// Returns the policies of networks that attach a NIC: they allow egress, egress to some
/// destinations, egress and host-loopback access from the guest, a proxy alone, ingress, and
/// host-loopback access both ways, with a forward.
fn attaching() -> Vec<NetworkPolicy> {
    vec![
        NetworkPolicy::new(Access::Allow),
        NetworkPolicy::new(Access::Deny)
            .with_allow(EgressRule::to("192.0.2.0/24").on_port(Protocol::Tcp, 443))
            .with_deny(EgressRule::to("192.0.2.128/25")),
        outbound(),
        NetworkPolicy::proxy_only(NetworkProxy::new(3128)),
        listening(),
        forwarded(),
    ]
}

/// Returns the networks of templates: none; one under a policy that lets nothing through, which
/// attaches no NIC; and those under each policy of [`attaching`].
fn networks() -> Vec<Option<SandboxNetwork>> {
    let mut networks = vec![None, Some(network(NetworkPolicy::new(Access::Deny)))];
    networks.extend(attaching().into_iter().map(|policy| Some(network(policy))));
    networks
}

/// Returns the resources of `processors`, all online at boot, with 512 MiB of RAM and a capacity
/// of 2 GiB.
fn resources(processors: u32) -> TemplateResources {
    TemplateResources {
        processors,
        boot_processors: processors,
        memory: 512 * MIB,
        memory_capacity: 2 * GIB,
    }
}

/// Returns the storage generation whose bytes are 0 to 15.
fn generation() -> StorageGeneration {
    StorageGeneration::from_bytes([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]).unwrap()
}

/// The inputs of a lowering, which tests change from their defaults: an adapter's plan of the
/// EROFS placeholder over a clone's scratch, without a network, the placeholder's binding, 64 MiB
/// of scratch, the resources of 2 processors, and [`TRANSACTION`].
#[derive(Clone)]
struct Inputs {
    /// The adapter's plan.
    adapter: SandboxPlan,
    /// The binding of the placeholder.
    binding: DistroBinding,
    /// The length of the dormant scratch device.
    scratch_length: u64,
    /// The processors and RAM.
    resources: TemplateResources,
    /// The transaction directory.
    transaction: String,
}

impl Default for Inputs {
    fn default() -> Self {
        Self {
            adapter: adapter(None),
            binding: binding(),
            scratch_length: 64 * MIB,
            resources: resources(2),
            transaction: TRANSACTION.to_owned(),
        }
    }
}

impl Inputs {
    /// Returns the default inputs with `adapter`.
    fn of(adapter: SandboxPlan) -> Self {
        Self {
            adapter,
            ..Self::default()
        }
    }

    /// Lowers the adapter's plan with the other inputs and [`generation`].
    fn lower(&self) -> Result<TemplatePlan> {
        TemplatePlan::new(
            &self.adapter,
            &self.binding,
            self.scratch_length,
            self.resources,
            Path::new(&self.transaction),
            generation(),
        )
    }
}

/// Returns the template of `adapter`, with the other inputs' defaults.
fn template(adapter: SandboxPlan) -> TemplatePlan {
    Inputs::of(adapter).lower().unwrap()
}

/// Returns the templates of every format of placeholder, of adapters' plans over a clone's scratch
/// and over RAM, of every network of [`networks`], and of every processor count, with every count
/// up to it online at boot.
fn every_template() -> Vec<TemplatePlan> {
    let mut templates = Vec::new();
    for (format, image) in [("erofs", erofs()), ("gpt", gpt())] {
        for upper in [SandboxUpper::scratch(CLONE_SCRATCH), SandboxUpper::RAM] {
            for network in networks() {
                for processors in PROCESSOR_COUNTS {
                    for boot_processors in PROCESSOR_COUNTS
                        .into_iter()
                        .filter(|boot| *boot <= processors)
                    {
                        let inputs = Inputs {
                            adapter: adapter_with(image, upper.clone(), network.clone()),
                            binding: binding_of(format, IDENTITY, GIB),
                            resources: TemplateResources {
                                boot_processors,
                                ..resources(processors)
                            },
                            ..Inputs::default()
                        };
                        templates.push(inputs.lower().unwrap());
                    }
                }
            }
        }
    }
    templates
}

/// Returns `texts` as owned strings.
fn strings(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|text| (*text).to_owned()).collect()
}

/// Returns the value of the one `option` of `arguments`, whose options each take one value.
fn value<'a>(arguments: &'a [String], option: &str) -> &'a str {
    let values: Vec<&str> = arguments
        .chunks(2)
        .filter(|pair| pair[0] == option)
        .map(|pair| pair[1].as_str())
        .collect();
    assert_eq!(values.len(), 1, "{option} in {arguments:?}");
    values[0]
}

/// Returns the message of the error of `result`, which must be
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
fn refusal<T: std::fmt::Debug>(result: Result<T>) -> String {
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    error.to_string()
}

#[test]
fn templates_render_their_devices_resources_and_capture_options() {
    let devices = strings(&[
        "--machine",
        "microvm",
        "--microvm-sandbox-block",
        &format!("distro:file:{PLACEHOLDER},ro"),
        "--microvm-sandbox-block",
        &format!("scratch:file:{SCRATCH}"),
    ]);
    let capture = strings(&[
        "--processors",
        "2",
        "--memory",
        "512M",
        "--memory-capacity",
        "2048M",
        "--snapshot-destination",
        DESTINATION,
        "--snapshot-tier",
        "platform",
        "--snapshot-block-identity",
        "generation",
        "--snapshot-generation-id",
        GENERATION,
    ]);
    // No NIC, without a network or with one that lets nothing through.
    for network in [None, Some(network(NetworkPolicy::new(Access::Deny)))] {
        let template = template(adapter(network));
        assert!(template.nic_arguments().is_empty());
        assert_eq!(template.capture_arguments(), capture);
        assert_eq!(
            template.openvmm_arguments().unwrap(),
            [devices.clone(), capture.clone()].concat()
        );
    }
    // One fixed NIC, at the network's guest address, under the complete deny-all policy, whatever
    // the network's own, which each clone's restore replaces.
    let nic = strings(&[
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
    ]);
    for policy in attaching() {
        let template = template(adapter(Some(network(policy))));
        assert_eq!(template.nic_arguments(), nic);
        assert_eq!(template.capture_arguments(), capture);
        assert_eq!(
            template.openvmm_arguments().unwrap(),
            [devices.clone(), nic.clone(), capture.clone()].concat()
        );
    }
    // RAM and its capacity in MiB.
    let inputs = Inputs {
        resources: TemplateResources {
            processors: 8,
            boot_processors: 8,
            memory: MEMORY_BLOCK_SIZE,
            memory_capacity: MEMORY_BLOCK_SIZE,
        },
        ..Inputs::default()
    };
    assert_eq!(
        inputs.lower().unwrap().capture_arguments()[..6],
        strings(&[
            "--processors",
            "8",
            "--memory",
            "128M",
            "--memory-capacity",
            "128M",
        ])
    );
}

#[test]
fn templates_keep_the_placeholder_and_the_files_of_their_capture_in_its_transaction() {
    let network = Some(network(NetworkPolicy::new(Access::Allow)));
    let template = template(adapter(network.clone()));
    let plan = template.plan();
    assert_eq!(plan.distro().path(), Path::new(PLACEHOLDER));
    assert_eq!(plan.distro().image(), erofs());
    assert_eq!(plan.upper().scratch_path(), Some(Path::new(SCRATCH)));
    assert_eq!(plan.settings(), None);
    assert_eq!(plan.export(), None);
    assert_eq!(plan.network(), None);
    assert_eq!(template.binding(), &binding());
    assert_eq!(template.image_capacity(), GIB);
    assert_eq!(template.scratch_path(), Path::new(SCRATCH));
    assert_eq!(template.scratch_length(), 64 * MIB);
    assert_eq!(template.resources(), resources(2));
    assert_eq!(template.transaction(), Path::new(TRANSACTION));
    assert_eq!(template.destination(), Path::new(DESTINATION));
    assert_eq!(template.generation(), generation());
    assert_eq!(template.kernel_command_line(), "nvx_sandbox=1");
    assert_eq!(
        (SCRATCH_NAME, SNAPSHOT_NAME, MEMORY_BLOCK_SIZE),
        ("scratch.raw", "snapshot", 128 * MIB)
    );
    // The capacity of every clone's image is the placeholder's length, as its binding records it.
    let inputs = Inputs {
        binding: binding_of("erofs", IDENTITY, 8 * GIB + 512),
        scratch_length: 512,
        ..Inputs::default()
    };
    let template = inputs.lower().unwrap();
    assert_eq!(
        (template.image_capacity(), template.scratch_length()),
        (8 * GIB + 512, 512)
    );
}

#[test]
fn templates_limit_their_processors_online_at_boot_on_their_command_line() {
    // A template whose processors are all online at boot sets no limit, and OpenVMM records none.
    for processors in PROCESSOR_COUNTS {
        let template = Inputs {
            resources: resources(processors),
            ..Inputs::default()
        }
        .lower()
        .unwrap();
        assert_eq!(template.kernel_command_line(), "nvx_sandbox=1");
    }
    // One that boots with fewer online limits them, and keeps its capacity.
    let inputs = Inputs {
        resources: TemplateResources {
            boot_processors: 2,
            ..resources(8)
        },
        ..Inputs::default()
    };
    let template = inputs.lower().unwrap();
    assert_eq!(template.kernel_command_line(), "nvx_sandbox=1 maxcpus=2");
    assert_eq!(value(&template.capture_arguments(), "--processors"), "8");
    assert_eq!(template.resources(), inputs.resources);
}

#[test]
fn the_upper_of_an_adapters_plan_is_a_clones_and_no_part_of_its_template() {
    let over_scratch = template(adapter(None));
    let over_ram = template(adapter_with(erofs(), SandboxUpper::RAM, None));
    assert_eq!(over_ram, over_scratch);
    assert_eq!(
        over_ram.openvmm_arguments().unwrap(),
        over_scratch.openvmm_arguments().unwrap()
    );
    assert_eq!(over_ram.scratch_path(), Path::new(SCRATCH));
}

#[test]
fn transaction_directories_are_spelled_with_their_components() {
    let given = if cfg!(windows) {
        r"C:/templates//tx-1/."
    } else {
        "/templates//tx-1/./"
    };
    let inputs = Inputs {
        transaction: given.to_owned(),
        ..Inputs::default()
    };
    let template = inputs.lower().unwrap();
    assert_eq!(template.transaction().to_str(), Some(TRANSACTION));
    assert_eq!(template.scratch_path().to_str(), Some(SCRATCH));
    assert_eq!(template.destination().to_str(), Some(DESTINATION));
    assert_eq!(template, Inputs::default().lower().unwrap());
}

#[test]
fn openvmms_platform_allowlist_admits_its_tokens_and_refuses_tenant_data() {
    for token in [
        "earlycon=xe9",
        "console=hvc0",
        "console=hvc1",
        "reboot=t",
        "panic=-1",
        "nvx_sandbox=1",
        "nvx_config=0xd0010000,65536",
        "nvx_snapshot_tier=platform",
        "nr_cpus=8",
        "maxcpus=1",
        "nvx_control_tty=hvc2",
        "virtio_mmio.device=0x1000@0xd0001000:6",
        "virtnet_ip=10.0.0.2",
        "virtnet_mask=255.255.255.0",
        "virtnet_gw=10.0.0.1",
        "virtnet_dns=10.0.0.1",
        "virtnet_ip6=fd00::2/64",
        "virtnet_gw6=fd00::1",
    ] {
        assert!(openvmm::admits(token), "{token}");
    }
    // The tokens of a root, a workload, a share, a listener, and anything else that a tenant's
    // sandbox or another tier sets.
    let [layer, upper] = adapter(None).layout().tokens();
    let [_, ram] = adapter_with(gpt(), SandboxUpper::RAM, None)
        .layout()
        .tokens();
    for token in [
        layer.as_str(),
        upper.as_str(),
        ram.as_str(),
        "nvx_hostname=tenant-a",
        "hostname=tenant-a",
        "nvx_entrypoint=/bin/sh",
        "nvx_arg=-c",
        "nvx_memory_max=1073741824",
        "nvx_maps=1",
        "nvx_workload_account=create",
        "nvx_workload_uid=1000",
        "nvx_workload_gid=1000",
        "nvx_lifecycle=managed",
        "virtfs_dir=0",
        "virtnet_ingress=1",
        "nvx_proxy=http://10.0.0.1:3128",
        "nvx_config=tenant-data",
        "nvx_sandbox=0",
        "nvx_snapshot_tier=workload-start",
        "console=ttyS0",
        "quiet",
        "",
    ] {
        assert!(!openvmm::admits(token), "{token}");
    }
}

#[test]
fn the_command_line_holds_only_tokens_of_openvmms_platform_allowlist() {
    for template in every_template() {
        let arguments = template.openvmm_arguments().unwrap();
        let command_line = template.kernel_command_line();
        let mut tokens: Vec<String> = command_line.split(' ').map(str::to_owned).collect();
        // The tokens that OpenVMM writes for the template's arguments.
        tokens.push(format!("nr_cpus={}", value(&arguments, "--processors")));
        tokens.push(format!(
            "nvx_snapshot_tier={}",
            value(&arguments, "--snapshot-tier")
        ));
        // Under the NIC's complete deny-all policy, OpenVMM writes no DNS server and no
        // published listeners.
        if arguments.iter().any(|argument| argument == "--net") {
            let guest_network = value(&arguments, "--net");
            let (address, _) = guest_network.split_once('/').unwrap();
            tokens.push(format!("virtnet_ip={address}"));
            tokens.push(format!(
                "virtnet_gw={}",
                crate::net::guest_gateway(guest_network).unwrap()
            ));
        }
        for token in &tokens {
            assert!(openvmm::admits(token), "{token} of {template:?}");
            // OpenVMM splits the command line at ASCII whitespace, and the kernel at its own,
            // which also holds the vertical tab and 0xa0: no token holds either, so both split
            // the command line into the same tokens.
            assert!(
                token.is_ascii() && !token.bytes().any(cmdline::is_space),
                "{token:?} of {template:?}"
            );
        }
        // The command line of the template's devices names the root of its plan, which a clone
        // binds, and which the allowlist refuses: the template's replaces it.
        let root = template.plan().kernel_command_line("").unwrap();
        for token in root.split(' ') {
            assert!(!openvmm::admits(token), "{token} of {root}");
            assert!(!command_line.contains(token), "{token} in {command_line}");
        }
    }
}

#[test]
fn templates_hold_no_tenant_data() {
    for template in every_template() {
        let arguments = template.openvmm_arguments().unwrap();
        let command_line = template.kernel_command_line();
        // Neither the identity of the image, nor the clone's upper, nor the proxy.
        for text in [IDENTITY, "tenant", "3128", "/sandboxes", r"\sandboxes"] {
            assert!(!command_line.contains(text), "{text} in {command_line}");
        }
        for text in [IDENTITY, "tenant", "3128", "/sandboxes", r"\sandboxes"] {
            assert!(
                arguments.iter().all(|argument| !argument.contains(text)),
                "{text} in {arguments:?}"
            );
        }
        // No kernel command line, workload identity, lifecycle, share, published listener, or
        // other option besides those of the devices, the resources, and the capture, each with one
        // value.
        for pair in arguments.chunks(2) {
            let [option, value] = pair else {
                panic!("{arguments:?} ends in an option without a value");
            };
            let option = option.as_str();
            assert!(OPTIONS.contains(&option), "{option} in {arguments:?}");
            assert!(!value.starts_with("--"), "{option} {value}");
            if ["--network-egress", "--network-ingress", "--host-loopback"].contains(&option) {
                assert_eq!(value, "deny", "{option} in {arguments:?}");
            }
        }
        let devices = template.plan().openvmm_arguments().unwrap();
        assert_eq!(
            [
                devices,
                template.nic_arguments().to_vec(),
                template.capture_arguments()
            ]
            .concat(),
            arguments
        );
    }
}

#[test]
fn templates_refuse_what_no_capture_takes() {
    // A plan of the sandbox profile.
    let profile = SandboxPlan::new(
        SandboxDistro::new(PLACEHOLDER, erofs()),
        SandboxUpper::scratch(CLONE_SCRATCH),
        SandboxSettings::default(),
    )
    .unwrap();
    assert_eq!(
        refusal(Inputs::of(profile).lower()),
        "a template lowers an adapter's plan, not a plan of the sandbox profile, whose workload \
         settings no template captures"
    );

    // A placeholder that is not the image that its binding names.
    let other = "22222222-2222-2222-2222-222222222222";
    for (image, binding) in [
        (gpt(), binding()),
        (RootImage::erofs(None), binding()),
        (erofs(), binding_of("erofs", other, GIB)),
        (erofs(), binding_of("gpt", IDENTITY, GIB)),
    ] {
        let expected = format!(
            "the placeholder {PLACEHOLDER} is not the {} layer with identity {} that its distro \
             binding names",
            binding.format(),
            binding.identity()
        );
        let inputs = Inputs {
            adapter: adapter_with(image, SandboxUpper::RAM, None),
            binding,
            ..Inputs::default()
        };
        assert_eq!(refusal(inputs.lower()), expected, "{image:?}");
    }
    let inputs = Inputs {
        binding: binding_of("erofs", "11111111-1111-1111-1111-11111111111A", GIB),
        ..Inputs::default()
    };
    assert_eq!(
        refusal(inputs.lower()),
        "the distro binding names the identity \"11111111-1111-1111-1111-11111111111A\", which \
         is not a UUID in its canonical text"
    );

    // Placeholders that OpenVMM would resolve against its own directory.
    let mut relative = vec!["platform/placeholder.img"];
    if cfg!(windows) {
        // Rooted on the current drive, and relative to a drive's current directory.
        relative.extend([r"\platform\placeholder.img", r"C:platform\placeholder.img"]);
    }
    for path in relative {
        let inputs = Inputs::of(adapter_at(path, erofs(), SandboxUpper::RAM, None));
        assert_eq!(
            refusal(inputs.lower()),
            format!(
                "the placeholder of a template's capture must be absolute, since OpenVMM would \
                 resolve it against the directory in which it runs: {}",
                paths::normalized(Path::new(path)).display()
            )
        );
    }
    // Placeholders that OpenVMM's block device options would split, and that it would not attach
    // as raw images.
    for (name, problem) in [
        (
            "placeholder,1.img",
            "containing commas, semicolons, or unbalanced brackets are unsupported",
        ),
        (
            "placeholder;1.img",
            "containing commas, semicolons, or unbalanced brackets are unsupported",
        ),
        (
            "placeholder[1.img",
            "containing commas, semicolons, or unbalanced brackets are unsupported",
        ),
        (
            "placeholder]1.img",
            "containing commas, semicolons, or unbalanced brackets are unsupported",
        ),
        (
            "placeholder][1.img",
            "containing commas, semicolons, or unbalanced brackets are unsupported",
        ),
        (
            "placeholder.vhd",
            "ending in .vhd are unsupported, since OpenVMM would not attach them as raw images",
        ),
        (
            "placeholder.vhdx",
            "ending in .vhdx are unsupported, since OpenVMM would not attach them as raw images",
        ),
        (
            "placeholder.vmgs",
            "ending in .vmgs are unsupported, since OpenVMM would not attach them as raw images",
        ),
        (
            "placeholder.iso",
            "ending in .iso are unsupported, since OpenVMM would not attach them as raw images",
        ),
    ] {
        let path = Path::new(PLACEHOLDER).with_file_name(name);
        let path = path.to_str().unwrap();
        let inputs = Inputs::of(adapter_at(path, erofs(), SandboxUpper::RAM, None));
        assert_eq!(
            refusal(inputs.lower()),
            format!("disk paths {problem}: {path}")
        );
    }

    // Block devices that OpenVMM does not attach.
    for length in [0, 511, 1000, GIB + 1] {
        let placeholder = Inputs {
            binding: binding_of("erofs", IDENTITY, length),
            ..Inputs::default()
        };
        assert_eq!(
            refusal(placeholder.lower()),
            format!(
                "the placeholder {PLACEHOLDER} of {length} bytes cannot be attached: OpenVMM \
                 attaches block devices of nonzero multiples of 512 bytes"
            )
        );
        let scratch = Inputs {
            scratch_length: length,
            ..Inputs::default()
        };
        assert_eq!(
            refusal(scratch.lower()),
            format!(
                "a template's scratch of {length} bytes cannot be attached: OpenVMM attaches \
                 block devices of nonzero multiples of 512 bytes"
            )
        );
    }

    // Processor counts other than 1, 2, 4, and 8.
    for processors in [0, 3, 6, 16, u32::MAX] {
        let inputs = Inputs {
            resources: resources(processors),
            ..Inputs::default()
        };
        assert_eq!(
            refusal(inputs.lower()),
            format!("a template's VM has 1, 2, 4, or 8 processors, not {processors}")
        );
    }
    // Processors online at boot other than 1, 2, 4, and 8, or more than the processors.
    for (processors, boot_processors) in [(2, 0), (8, 3), (2, 4), (8, 16), (8, u32::MAX)] {
        let inputs = Inputs {
            resources: TemplateResources {
                boot_processors,
                ..resources(processors)
            },
            ..Inputs::default()
        };
        assert_eq!(
            refusal(inputs.lower()),
            format!(
                "a template's VM boots with 1, 2, 4, or 8 of its {processors} processors online, \
                 not {boot_processors}"
            )
        );
    }
    // RAM and capacities that are not whole blocks, and capacities below the RAM.
    for (memory, memory_capacity) in [
        (0, 0),
        (0, 2 * GIB),
        (100 * MIB, 2 * GIB),
        (512 * MIB, 2 * GIB + MIB),
        (512 * MIB, 384 * MIB),
        (512 * MIB, 0),
    ] {
        let inputs = Inputs {
            resources: TemplateResources {
                processors: 2,
                boot_processors: 2,
                memory,
                memory_capacity,
            },
            ..Inputs::default()
        };
        assert_eq!(
            refusal(inputs.lower()),
            format!(
                "a template's VM needs nonzero RAM, and a RAM capacity of at least it, each a \
                 multiple of 128 MiB, not {memory} bytes of RAM and a capacity of \
                 {memory_capacity} bytes"
            )
        );
    }

    // Transaction directories that OpenVMM would resolve against its own directory.
    let mut relative = vec!["templates/tx-1", "."];
    if cfg!(windows) {
        // Rooted on the current drive, and relative to a drive's current directory.
        relative.extend([r"\templates\tx-1", r"C:templates\tx-1"]);
    }
    for transaction in relative {
        let inputs = Inputs {
            transaction: transaction.to_owned(),
            ..Inputs::default()
        };
        assert_eq!(
            refusal(inputs.lower()),
            format!(
                "the transaction directory of a template's capture must be absolute, since \
                 OpenVMM would resolve it against the directory in which it runs: {}",
                paths::normalized(Path::new(transaction)).display()
            )
        );
    }
    // Transaction directories whose scratch device OpenVMM's options would split.
    for name in ["a,b", "a;b", "a[b", "a]b", "a][b"] {
        let inputs = Inputs {
            transaction: format!("{TRANSACTION}-{name}"),
            ..Inputs::default()
        };
        assert_eq!(
            refusal(inputs.lower()),
            format!(
                "disk paths containing commas, semicolons, or unbalanced brackets are unsupported: \
                 {}",
                Path::new(&inputs.transaction).join(SCRATCH_NAME).display()
            )
        );
    }
}

/// Lowers the default inputs with the transaction directory `transaction`.
fn lower_in(transaction: &Path) -> Result<TemplatePlan> {
    let inputs = Inputs::default();
    TemplatePlan::new(
        &inputs.adapter,
        &inputs.binding,
        inputs.scratch_length,
        inputs.resources,
        transaction,
        generation(),
    )
}

#[cfg(unix)]
#[test]
fn transaction_directories_must_be_valid_unicode() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let transaction = Path::new(OsStr::from_bytes(b"/templates/tx-\xff"));
    assert_eq!(
        refusal(lower_in(transaction)),
        "the transaction directory of a template's capture is not valid Unicode: \
         /templates/tx-\u{fffd}"
    );
}

#[cfg(windows)]
#[test]
fn transaction_directories_must_be_valid_unicode() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    let mut units: Vec<u16> = r"C:\templates\tx-".encode_utf16().collect();
    units.push(0xd800);
    let transaction = PathBuf::from(OsString::from_wide(&units));
    assert_eq!(
        refusal(lower_in(&transaction)),
        "the transaction directory of a template's capture is not valid Unicode: \
         C:\\templates\\tx-\u{fffd}"
    );
}

/// A change to the inputs of a lowering.
type Fix = fn(&mut Inputs);

#[test]
fn templates_check_their_inputs_in_order() {
    // With every input refused, each fix in turn reveals the next refusal.
    let mut inputs = Inputs {
        adapter: SandboxPlan::new(
            SandboxDistro::new(PLACEHOLDER, gpt()),
            SandboxUpper::RAM,
            SandboxSettings::default(),
        )
        .unwrap()
        .with_network(Some(network(listening()))),
        binding: binding_of("erofs", IDENTITY, 1000),
        scratch_length: 0,
        resources: TemplateResources {
            processors: 3,
            boot_processors: 3,
            memory: 0,
            memory_capacity: 0,
        },
        transaction: "templates".to_owned(),
    };
    let steps: [(&str, Fix); 11] = [
        ("a template lowers an adapter's plan", |inputs| {
            inputs.adapter = adapter_at(
                "placeholder.vhd",
                gpt(),
                SandboxUpper::RAM,
                Some(network(listening())),
            );
        }),
        ("is not the erofs layer", |inputs| {
            inputs.adapter = adapter_at(
                "placeholder.vhd",
                erofs(),
                SandboxUpper::RAM,
                Some(network(listening())),
            );
        }),
        (
            "the placeholder of a template's capture must be absolute",
            |inputs| {
                inputs.adapter = adapter_at(
                    &PLACEHOLDER.replace(".img", ".vhd"),
                    erofs(),
                    SandboxUpper::RAM,
                    Some(network(listening())),
                );
            },
        ),
        ("disk paths ending in .vhd", |inputs| {
            inputs.adapter = adapter(Some(network(listening())));
        }),
        ("of 1000 bytes cannot be attached", |inputs| {
            inputs.binding = binding();
        }),
        ("a template's scratch of 0 bytes", |inputs| {
            inputs.scratch_length = 512;
        }),
        ("processors, not 3", |inputs| {
            inputs.resources.processors = 2
        }),
        ("of its 2 processors online, not 3", |inputs| {
            inputs.resources.boot_processors = 1
        }),
        ("needs nonzero RAM", |inputs| {
            inputs.resources = resources(2)
        }),
        ("must be absolute", |inputs| {
            inputs.transaction = format!("{TRANSACTION},1");
        }),
        ("commas, semicolons, or unbalanced brackets", |inputs| {
            inputs.transaction = TRANSACTION.to_owned()
        }),
    ];
    for (refused, fix) in steps {
        let message = refusal(inputs.lower());
        assert!(message.contains(refused), "{refused}: {message}");
        fix(&mut inputs);
    }
    assert_eq!(inputs.lower().unwrap().scratch_length(), 512);
}
