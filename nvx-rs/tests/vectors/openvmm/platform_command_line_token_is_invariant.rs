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
        || token == openvmm_defs::microvm::MICROVM_CONTROL_TTY_COMMAND_LINE
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
