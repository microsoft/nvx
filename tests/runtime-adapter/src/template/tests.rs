use nvx::protocol::status;

use super::*;
use crate::root::RootStage;

/// The kernel command line of a template's VM, as OpenVMM writes it for a platform capture
/// without a network, with the newline that ends `/proc/cmdline`.
const TEMPLATE: &str = "earlycon=xe9 console=hvc0 reboot=t panic=-1 nvx_sandbox=1 \
        nvx_config=0xd0010000,65536 nvx_snapshot_tier=platform nr_cpus=4 \
        virtio_mmio.device=0x1000@0xd0001000:6 virtio_mmio.device=0x1000@0xd0003000:7 \
        virtio_mmio.device=0x1000@0xd0006000:10 virtio_mmio.device=0x1000@0xd0007000:11 \
        nvx_control_tty=hvc2\n";

/// The tokens of a template's VM with a network, which OpenVMM adds to [`TEMPLATE`].
const NETWORK: &str = "virtio_mmio.device=0x1000@0xd0000000:5 virtnet_ip=10.0.2.15 \
        virtnet_mask=255.255.255.0 virtnet_gw=10.0.2.2 virtnet_dns=10.0.2.3,10.0.2.4 \
        virtnet_ip6=fd00::15/64 virtnet_gw6=fd00::2";

/// Returns [`TEMPLATE`] with `token` at the start, in the middle, and at the end.
fn with(token: &str) -> [String; 3] {
    let template = TEMPLATE.trim_end();
    let middle = template.find(" nvx_snapshot_tier").unwrap();
    [
        format!("{token} {template}\n"),
        format!("{} {token}{}\n", &template[..middle], &template[middle..]),
        format!("{template} {token}\n"),
    ]
}

/// The tokens that the platform tier's allowlist holds whole: the consoles, the reboot and
/// panic settings, the sandbox's guest mode, NVX's configuration window, the tier itself, and
/// the control tty.
const WHOLE: [&str; 9] = [
    "earlycon=xe9",
    "console=hvc0",
    "console=hvc1",
    "reboot=t",
    "panic=-1",
    "nvx_sandbox=1",
    "nvx_config=0xd0010000,65536",
    PLATFORM_TIER,
    "nvx_control_tty=hvc2",
];

#[test]
fn the_platform_tier_s_allowlist_is_openvmm_s() {
    for token in WHOLE {
        assert!(is_platform_token(token.as_bytes()), "{token}");
    }
    for token in [
        "nr_cpus=1",
        "nr_cpus=8",
        "nr_cpus=",
        "maxcpus=1",
        "maxcpus=4",
        "maxcpus=",
        "virtio_mmio.device=0x1000@0xd0003000:7",
        "virtio_mmio.device=4K@0xd0003000:7:2",
        "virtnet_ip=10.0.2.15",
        "virtnet_mask=255.255.255.0",
        "virtnet_gw=10.0.2.2",
        "virtnet_dns=10.0.2.3,10.0.2.4",
        "virtnet_dns=",
        "virtnet_ip6=fd00::15/64",
        "virtnet_gw6=fd00::2",
    ] {
        assert!(is_platform_token(token.as_bytes()), "{token}");
    }
    for token in [
        "",
        "earlycon=xe9,keep",
        "earlycon",
        "console=hvc2",
        "console=ttyS0",
        "reboot=k",
        "panic=0",
        "nvx_sandbox=0",
        "nvx_config=0xd0010000,65537",
        "nvx_config=0xd0010000",
        "nvx_snapshot_tier=workload-start",
        "nvx_snapshot_tier=instance-checkpoint",
        "nvx-snapshot-tier=platform",
        "nvx_snapshot_tier=platform2",
        "nvx_control_tty=hvc1",
        "nvx-control-tty=hvc2",
        "nr_cpus",
        "NR_CPUS=4",
        "nr-cpus=4",
        "maxcpus",
        "MAXCPUS=1",
        "max_cpus=1",
        "virtio_mmio.device",
        "virtio-mmio.device=0x1000@0xd0003000:7",
        "virtnet_ingress=allow",
        "virtnet_probe=10.0.2.2",
        "virtfs_dir=/run/nvx/hostfs/root",
        "virtfs_tag=microvm",
        "virtfs_mode=ro",
        "virtfs_aggregate=1",
        "nvx_workload_uid=1000",
        "nvx_workload_gid=1000",
        "nvx_lifecycle=managed",
        "nvx_exec=/bin/sh",
        "nvx_share=0,/data",
        "driver_async_probe=virtio_mmio",
        "init=/bin/sh",
        "rdinit=/bin/sh",
        "quiet",
        "--",
        "--=1",
    ] {
        assert!(!is_platform_token(token.as_bytes()), "{token}");
    }
    // OpenVMM checks text: a token that is not UTF-8 is not on the allowlist, whatever it
    // starts with.
    for token in [
        &b"virtnet_dns=10.0.2.3\xc2"[..],
        b"nr_cpus=\xff",
        b"\xff=\x01",
    ] {
        assert!(!is_platform_token(token), "{}", token.escape_ascii());
    }
}

#[test]
fn the_platform_tier_s_allowlist_takes_tokens_as_openvmm_splits_them() {
    // OpenVMM splits at ASCII whitespace alone, so it admits these tokens by their prefixes,
    // but the kernel splits each in two at the vertical tab or at the no-break space's 0xa0,
    // and its second parameter is not on the allowlist, so a template's VM refuses it.
    for token in [
        "virtnet_dns=10.0.2.3\u{0b}quiet",
        "virtnet_dns=10.0.2.3\u{a0}quiet",
    ] {
        assert!(is_platform_token(token.as_bytes()), "{token:?}");
        let names: Vec<&[u8]> = cmdline::parameters(token.as_bytes())
            .map(|parameter| parameter.name)
            .collect();
        assert_eq!(names, [&b"virtnet_dns"[..], b"quiet"], "{token:?}");
    }
    assert!(!is_platform_token(b"quiet"));
}

#[test]
fn a_command_line_without_the_platform_tier_boots_cold() {
    for command_line in [
        "",
        "\n",
        "earlycon=xe9 console=hvc0 reboot=t panic=-1 nvx_sandbox=1 nvx_control_tty=hvc2\n",
        // A layered root's tokens, which a template's command line never holds.
        "nvx_sandbox=1 virtfs_tag=microvm quiet init=/bin/sh\n",
        "nvx_snapshot_tier=workload-start nvx_sandbox=1 quiet\n",
        "nvx-snapshot-tier=platform quiet\n",
        "nvx_snapshot_tier=platform2 quiet\n",
        "nvx_snapshot_tier quiet\n",
        // A cold boot takes quotes and the delimiter as before.
        "nvx_sandbox=1 note=\"left right\" -- extra\n",
        // The kernel hands the tier after the delimiter to init, as an argument.
        "nvx_sandbox=1 -- nvx_snapshot_tier=platform\n",
        // The kernel stops at a NUL.
        "nvx_sandbox=1\0nvx_snapshot_tier=platform\n",
    ] {
        assert_eq!(
            boot(command_line.as_bytes()),
            Ok(Boot::Cold),
            "{command_line:?}"
        );
    }
}

#[test]
fn a_template_s_command_line_holds_the_platform_tier_s_tokens_alone() {
    let network = TEMPLATE.replacen(
        " nvx_control_tty",
        &format!(" {NETWORK} nvx_control_tty"),
        1,
    );
    // A template with a NIC names its gateway, which the announcers of its clones take.
    assert_eq!(
        boot(network.as_bytes()),
        Ok(Boot::Template {
            gateway: Some(Ipv4Addr::new(10, 0, 2, 2))
        })
    );
    for command_line in [
        TEMPLATE.to_owned(),
        TEMPLATE.trim_end().to_owned(),
        // A template that brings fewer processors online at boot than its capacity.
        TEMPLATE.replacen(" nvx_control_tty", " maxcpus=1 nvx_control_tty", 1),
        // The tier alone, and each token whole once more, which OpenVMM leaves to its own
        // checks.
        format!("{PLATFORM_TIER}\n"),
        format!("{} {}\n", TEMPLATE.trim_end(), WHOLE.join(" ")),
        // The kernel's whitespace: tabs, newlines, carriage returns, and no-break spaces.
        TEMPLATE.replace(' ', " \t\r\n\u{0b}\u{0c}"),
    ] {
        assert_eq!(
            boot(command_line.as_bytes()),
            Ok(Boot::Template { gateway: None }),
            "{command_line:?}"
        );
    }
    let mut latin1 = TEMPLATE.as_bytes().to_vec();
    for byte in &mut latin1 {
        if *byte == b' ' {
            *byte = 0xa0;
        }
    }
    assert_eq!(boot(&latin1), Ok(Boot::Template { gateway: None }));
}

#[test]
fn template_mode_refuses_a_gateway_that_openvmm_would_not_render() {
    for (gateway, problem) in [
        (
            "virtnet_gw=10.0.2.2 virtnet_gw=10.0.2.2",
            "the virtnet_gw kernel parameter is repeated",
        ),
        (
            "virtnet_gw=010.0.2.2",
            "the virtnet_gw kernel parameter must name one IPv4 address as OpenVMM renders \
                 it, not '010.0.2.2'",
        ),
        (
            "virtnet_gw=fd00::2",
            "the virtnet_gw kernel parameter must name one IPv4 address as OpenVMM renders \
                 it, not 'fd00::2'",
        ),
    ] {
        let command_line = format!("{} {gateway}\n", TEMPLATE.trim_end());
        let refused = boot(command_line.as_bytes()).unwrap_err();
        assert!(
            matches!(&refused, CommandLineError::Gateway(error) if error.message() == problem),
            "{refused:?}"
        );
        assert_eq!(
            refused.to_string(),
            format!("the kernel command line names the platform snapshot tier, but {problem}")
        );
    }
}

#[test]
fn template_mode_refuses_a_token_outside_the_allowlist() {
    for token in [
        "quiet",
        "init=/bin/sh",
        "console=ttyS0",
        "nvx_config=0xd0010000,65537",
        "nvx_snapshot_tier=workload-start",
        "nvx-sandbox=1",
        "nvx_control_tty=hvc1",
        "nvx_exec=/bin/sh",
        "nvx_lifecycle=managed",
        "nvx_workload_uid=1000",
        "nvx_share=0,/data",
        "virtfs_tag=microvm",
        "virtfs_dir=/run/nvx/hostfs/root",
        "virtnet_ingress=allow",
        "driver_async_probe=virtio_mmio",
        "nr_cpus",
        "--=1",
        "caf\u{e9}=1",
    ] {
        for command_line in with(token) {
            let refused = CommandLineError::Token(token.as_bytes().to_vec());
            assert_eq!(
                boot(command_line.as_bytes()),
                Err(refused.clone()),
                "{command_line:?}"
            );
            assert_eq!(
                refused.to_string(),
                format!(
                    "the kernel command line names the platform snapshot tier, but holds \
                         `{}`, which the tier's allowlist does not hold",
                    token.as_bytes().escape_ascii()
                )
            );
        }
    }
    // The first of the parameters that the allowlist does not hold, in the command line's
    // order.
    assert_eq!(
        boot(format!("a {} b\n", TEMPLATE.trim_end()).as_bytes()),
        Err(CommandLineError::Token(b"a".to_vec()))
    );
    assert_eq!(
        boot(b"nvx_snapshot_tier=platform \xff=\x01\n"),
        Err(CommandLineError::Token(b"\xff=\x01".to_vec()))
    );
    // The kernel's whitespace includes the vertical tab and the byte 0xa0, with which the
    // no-break space ends in UTF-8. A split at ASCII whitespace, as OpenVMM's checks of a
    // platform snapshot's command line make, keeps either within a token, but to the kernel,
    // what follows either in a platform token's value is a parameter of its own. Before the
    // no-break space's 0xa0, its first byte, 0xc2, stays in the platform token, which is then
    // not text, and so not on the allowlist, which takes tokens as OpenVMM's text.
    for (separator, refused) in [
        ("\u{0b}", &b"quiet"[..]),
        ("\u{a0}", b"virtnet_dns=10.0.2.3\xc2"),
    ] {
        let command_line = format!(
            "{} virtnet_dns=10.0.2.3{separator}quiet\n",
            TEMPLATE.trim_end()
        );
        assert_eq!(
            boot(command_line.as_bytes()),
            Err(CommandLineError::Token(refused.to_vec())),
            "{command_line:?}"
        );
    }
    let mut latin1 = format!("{} virtnet_dns=10.0.2.3", TEMPLATE.trim_end()).into_bytes();
    latin1.extend_from_slice(b"\xa0quiet\n");
    assert_eq!(
        boot(&latin1),
        Err(CommandLineError::Token(b"quiet".to_vec()))
    );
    assert_eq!(
        CommandLineError::Token(b"\xff=\x01".to_vec()).to_string(),
        "the kernel command line names the platform snapshot tier, but holds `\\xff=\\x01`, \
             which the tier's allowlist does not hold"
    );
}

#[test]
fn template_mode_refuses_quotes_and_the_delimiter() {
    for quoted in [
        "\"console=hvc0\"",
        "nvx_config=\"0xd0010000,65536\"",
        "virtnet_dns=\"10.0.2.3 10.0.2.4\"",
        "note=\"left right\"",
    ] {
        for command_line in with(quoted) {
            assert_eq!(
                boot(command_line.as_bytes()),
                Err(CommandLineError::Quote),
                "{command_line:?}"
            );
        }
    }
    // The tier, quoted, still names a template's VM, whose command line holds a quote.
    assert_eq!(
        boot(
            TEMPLATE
                .replace(PLATFORM_TIER, "\"nvx_snapshot_tier=platform\"")
                .as_bytes()
        ),
        Err(CommandLineError::Quote)
    );
    assert_eq!(
        boot(format!("{} \"\n", TEMPLATE.trim_end()).as_bytes()),
        Err(CommandLineError::Quote)
    );
    // A quote that the kernel never closes holds the rest of the command line in one
    // parameter, as the delimiter hands the rest to init: a tier there names no template.
    assert_eq!(boot(format!("\" {TEMPLATE}").as_bytes()), Ok(Boot::Cold));
    let template = TEMPLATE.trim_end();
    for command_line in [
        format!("{template} --\n"),
        format!("{template} -- quiet\n"),
        format!("{template} -- nvx_sandbox=1\n"),
        format!("{template}\t--\t\n"),
        format!("{template}\u{0b}--\u{0b}quiet\n"),
        format!("{PLATFORM_TIER} -- {template}\n"),
    ] {
        assert_eq!(
            boot(command_line.as_bytes()),
            Err(CommandLineError::Delimiter),
            "{command_line:?}"
        );
    }
    assert_eq!(
        CommandLineError::Quote.to_string(),
        "the kernel command line names the platform snapshot tier, but holds a double quote, \
             which the kernel drops"
    );
    assert_eq!(
        CommandLineError::Delimiter.to_string(),
        "the kernel command line names the platform snapshot tier, but holds `--`, after \
             which the kernel hands the rest to init"
    );
}

#[test]
fn a_template_s_statuses_are_its_own() {
    let mut statuses = vec![COMMAND_LINE_STATUS, CONTINUED_STATUS];
    statuses.extend((1..=9).map(|stage| CAPTURE_STATUS_BASE + stage));
    assert_eq!(statuses, [20, 30, 21, 22, 23, 24, 25, 26, 27, 28, 29]);
    // None is a root stage's, the worker's after its session failed, or the usage error's.
    for stage in RootStage::ALL {
        assert!(!statuses.contains(&stage.exit_status()), "{stage}");
    }
    assert!(!statuses.contains(&0) && !statuses.contains(&1) && !statuses.contains(&2));
    // Nor is one the time ABI's, or one of a clone's repair or activation.
    for reserved in [
        status::TIME_STATUSES,
        status::REPAIR_STATUSES,
        status::ACTIVATION_STATUSES,
    ] {
        assert!(
            !statuses.iter().any(|own| reserved.contains(own)),
            "{reserved:?}"
        );
    }
}

#[test]
fn a_template_s_repair_delay_is_positive_milliseconds_of_at_most_half_a_minute() {
    for (delay, contents) in [
        (Duration::from_millis(1), "1\n"),
        (Duration::from_millis(8_000), "8000\n"),
        (MAX_REPAIR_DELAY, "30000\n"),
    ] {
        assert_eq!(repair_delay_contents(delay), contents);
        assert_eq!(parse_repair_delay(contents.as_bytes()), Some(delay));
        let line = contents.trim_end();
        assert_eq!(parse_repair_delay(line.as_bytes()), Some(delay));
    }
    // The milliseconds are whole.
    assert_eq!(repair_delay_contents(Duration::from_micros(2_500)), "2\n");
    for contents in [
        "", "\n", "0\n", "30001\n", "+5\n", "-5\n", " 5\n", "5 \n", "5\n\n", "5\r\n", "5s\n",
        "0x10\n", "1.5\n",
    ] {
        assert_eq!(
            parse_repair_delay(contents.as_bytes()),
            None,
            "{contents:?}"
        );
    }
    assert_eq!(parse_repair_delay(&[b'9'; 40]), None);
    assert_eq!(parse_repair_delay(b"5\xff"), None);
    // A file of the initramfs's `/etc`, which NVX's initramfs holds.
    assert!(REPAIR_DELAY_FILE.starts_with("/etc/"));
    assert_eq!(REPAIR_DELAY_FILE.matches('/').count(), 2);
}
