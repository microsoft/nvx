/// The guest kernel tuning parameters that a platform snapshot's command line
/// may carry, each parameter at most once. Like the other tokens that the
/// command line may carry, each is fixed when the guest kernel boots, is the
/// same for every clone, and carries no tenant data.
const PLATFORM_TUNING_TOKENS: [&str; 13] = [
    "mitigations=off",
    "mitigations=auto",
    "mitigations=auto,nosmt",
    "no_timer_check",
    "quiet",
    "loglevel=0",
    "loglevel=1",
    "loglevel=2",
    "loglevel=3",
    "loglevel=4",
    "loglevel=5",
    "loglevel=6",
    "loglevel=7",
];

fn platform_command_line_token_is_invariant(token: &str) -> bool {
    matches!(
        token,
        "earlycon=xe9"
            | "console=hvc0"
            | "console=hvc1"
            | "reboot=t"
            | "panic=-1"
            | "nvx_sandbox=1"
            | "nvx_config=0xd0010000,65536"
            | "nvx_snapshot_tier=platform"
    ) || token.starts_with("nr_cpus=")
        || token.starts_with("maxcpus=")
        || token == openvmm_defs::microvm::MICROVM_CONTROL_TTY_COMMAND_LINE
        || PLATFORM_TUNING_TOKENS.contains(&token)
        || [
            "virtio_mmio.device=",
            "virtnet_ip=",
            "virtnet_mask=",
            "virtnet_gw=",
            "virtnet_dns=",
            "virtnet_ip6=",
            "virtnet_gw6=",
        ]
        .iter()
        .any(|prefix| token.starts_with(prefix))
}
