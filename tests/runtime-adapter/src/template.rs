//! Template mode: the guest agent of a template's VM captures the VM before it assembles any root,
//! so that every clone that a restore of the snapshot starts attaches a root of its own. This
//! module holds what the host program and the guest agent share of it: how the guest agent tells
//! a template's VM by its kernel command line, the platform tier's allowlist of that command line,
//! and the statuses with which the guest agent of a template's VM ends. The worker captures the
//! VM, in `worker::capture`.
//!
//! OpenVMM adds [`PLATFORM_TIER`], `nvx_snapshot_tier=platform`, to the kernel command line of a
//! VM that captures a platform snapshot. [`boot`] reads the command line as the kernel splits it,
//! with `nvx::protocol::cmdline::parameters`: a command line without that parameter boots cold,
//! as before, whatever else it holds. A platform snapshot holds the platform's invariant
//! configuration alone, which every clone shares, so with that parameter, `boot` refuses any
//! parameter that [`is_platform_token`] does not accept: the platform tier's allowlist, OpenVMM's
//! own, compiled from the excerpts of its sources that NVX's tests keep. The kernel drops double
//! quotes from a parameter, and hands what follows `--` to init rather than taking it as
//! parameters, though init may still read it from `/proc/cmdline`, so `boot` also refuses a
//! template's command line that holds either, as OpenVMM refuses them in the command line of a VM
//! with a control console. It then reads the IPv4 gateway of the template's NIC, if the template
//! has one, with `nvx::protocol::cmdline::gateway`, which refuses a gateway that OpenVMM would not
//! render: the announcer of each clone that publishes its listeners announces them to it.
//!
//! The worker of a template's VM, and with it the VM, ends with [`COMMAND_LINE_STATUS`] once
//! `boot` refused its command line, with the bootstrap's status, 13, once the bootstrap of the
//! guest for the template's root failed, with [`CAPTURE_STATUS_BASE`] plus the number of the stage
//! of the capture that failed, and with [`CONTINUED_STATUS`] once the capture ended without a
//! snapshot. In a clone, which it repairs, it ends with the status of the repair's failure, which
//! `nvx::protocol::status` documents, and once it repaired the clone, as the clone's session ends:
//! with the status of the class of the clone's activation's failure, which `nvx::protocol::status`
//! documents too, or, once the clone was ready, as the worker of a VM that boots cold does. A
//! committed capture ends the template's VM before the worker returns.

use std::net::Ipv4Addr;
use std::{fmt, str};

use nvx::protocol::ProtocolError;
use nvx::protocol::cmdline::{self, Parameter};

/// Returns whether `token`, a parameter of a kernel command line as the command line spells it,
/// such as `nr_cpus=2`, is on the platform tier's allowlist: whether OpenVMM's capture of a
/// platform snapshot admits it in the VM's kernel command line.
///
/// The allowlist holds `earlycon=xe9`, `console=hvc0`, `console=hvc1`, `reboot=t`, `panic=-1`,
/// `nvx_sandbox=1`, `nvx_config=0xd0010000,65536`, [`PLATFORM_TIER`], and the control tty's token,
/// `nvx_control_tty=hvc2`, whole, and the tokens that start with `nr_cpus=`, `maxcpus=`,
/// `virtio_mmio.device=`, `virtnet_ip=`, `virtnet_mask=`, `virtnet_gw=`, `virtnet_dns=`,
/// `virtnet_ip6=`, or `virtnet_gw6=`, whose values OpenVMM writes or checks.
///
/// It is OpenVMM's own check, `platform_command_line_token_is_invariant` of
/// `openvmm_helpers::snapshot::microvm`, compiled from the excerpts of OpenVMM's sources that
/// `nvx-rs/tests/vectors/openvmm/` keeps, of the OpenVMM revision that NVX pins. NVX's tests check
/// each token of a template's kernel command line with the same excerpts, and an ignored test of
/// `nvx-rs/tests/openvmm_vectors.rs` compares them with the OpenVMM submodule. OpenVMM checks
/// tokens as text, so a token that is not UTF-8 is not on the allowlist, and it compares names
/// exactly, though the kernel takes a dash in a parameter's name for an underscore.
pub fn is_platform_token(token: &[u8]) -> bool {
    str::from_utf8(token).is_ok_and(openvmm::admits)
}

/// OpenVMM's allowlist of the kernel command-line tokens of its platform snapshot tier, compiled
/// from the excerpts of its sources in `nvx-rs/tests/vectors/openvmm/`, unchanged.
#[allow(unreachable_pub)]
mod openvmm {
    /// OpenVMM's `openvmm_defs` crate, of which the allowlist reads one constant.
    mod openvmm_defs {
        /// OpenVMM's `openvmm_defs::microvm`.
        pub(super) mod microvm {
            include!("../../../nvx-rs/tests/vectors/openvmm/microvm_control_tty_command_line.rs");
        }
    }

    include!("../../../nvx-rs/tests/vectors/openvmm/platform_command_line_token_is_invariant.rs");

    /// Returns whether OpenVMM's platform allowlist admits `token`.
    pub(super) fn admits(token: &str) -> bool {
        platform_command_line_token_is_invariant(token)
    }
}

/// The parameter that names the platform snapshot tier, which OpenVMM adds to the kernel command
/// line of a template's VM.
pub const PLATFORM_TIER: &str = "nvx_snapshot_tier=platform";

/// The status with which the worker of a template's VM, and with it the VM, ends once [`boot`]
/// refused its kernel command line, or the worker could not read it.
pub const COMMAND_LINE_STATUS: u8 = 20;

/// The base of the statuses with which the worker of a template's VM ends once a stage of its
/// capture failed: this base plus the stage's number, in the order in which
/// `nvx::guest::clone::CaptureStage` declares the stages, from 21 for the fence to 28 for the
/// release, or 29 for a stage that the worker does not know.
pub const CAPTURE_STATUS_BASE: u8 = 20;

/// The status with which the worker of a template's VM ends once the capture ended without a
/// snapshot, and the template continued.
pub const CONTINUED_STATUS: u8 = 30;

/// How a VM boots, as its kernel command line tells the guest agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boot {
    /// A VM that serves the host program: its kernel command line does not name the platform
    /// tier.
    Cold,
    /// A template's VM, which captures before it assembles any root: its kernel command line
    /// names the platform tier, and holds that tier's tokens alone.
    Template {
        /// The IPv4 gateway of the template's NIC, which the command line names, if the template
        /// has a NIC.
        gateway: Option<Ipv4Addr>,
    },
}

/// Why [`boot`] refused the kernel command line of a template's VM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandLineError {
    /// A parameter that the platform tier's allowlist does not hold, as the command line spells
    /// it.
    Token(Vec<u8>),
    /// A double quote, which the kernel drops from a parameter.
    Quote,
    /// A `--`, after which the kernel hands the rest of the command line to init.
    Delimiter,
    /// A gateway that OpenVMM would not render, as `nvx::protocol::cmdline::gateway` tells: a
    /// `virtnet_gw=` token that repeats, or that does not name one IPv4 address in its canonical
    /// text.
    Gateway(ProtocolError),
}

impl fmt::Display for CommandLineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the kernel command line names the platform snapshot tier, but ")?;
        match self {
            Self::Token(token) => write!(
                formatter,
                "holds `{}`, which the tier's allowlist does not hold",
                token.escape_ascii()
            ),
            Self::Quote => formatter.write_str("holds a double quote, which the kernel drops"),
            Self::Delimiter => {
                formatter.write_str("holds `--`, after which the kernel hands the rest to init")
            }
            Self::Gateway(error) => formatter.write_str(error.message()),
        }
    }
}

impl std::error::Error for CommandLineError {}

/// Returns how a VM boots, as its kernel command line, `command_line`, such as `/proc/cmdline`,
/// tells, as the [module documentation](self) describes: a command line names the platform tier
/// if the kernel finds [`PLATFORM_TIER`] among its parameters, and a template's names the gateway
/// of its NIC, if it has one.
///
/// Fails with a [`CommandLineError`] if the command line names the platform tier, but holds a
/// double quote, or else a `--`, or else a parameter that [`is_platform_token`] does not accept,
/// the first in the command line's order, or else a gateway that OpenVMM would not render.
pub fn boot(command_line: &[u8]) -> Result<Boot, CommandLineError> {
    let template = cmdline::parameters(command_line)
        .any(|parameter| token(&parameter) == PLATFORM_TIER.as_bytes());
    if !template {
        return Ok(Boot::Cold);
    }
    if command_line.contains(&b'"') {
        return Err(CommandLineError::Quote);
    }
    // Without quotes, the kernel splits the command line at its whitespace alone.
    if command_line
        .split(|&byte| cmdline::is_space(byte))
        .any(|word| word == b"--")
    {
        return Err(CommandLineError::Delimiter);
    }
    if let Some(token) = cmdline::parameters(command_line)
        .map(|parameter| token(&parameter))
        .find(|token| !is_platform_token(token))
    {
        return Err(CommandLineError::Token(token));
    }
    let gateway = cmdline::gateway(command_line).map_err(CommandLineError::Gateway)?;
    Ok(Boot::Template { gateway })
}

/// Returns `parameter` as the command line spells it, without quotes: its name, then `=` and its
/// value, if it has one.
fn token(parameter: &Parameter<'_>) -> Vec<u8> {
    let mut token = parameter.name.to_vec();
    if let Some(value) = parameter.value {
        token.push(b'=');
        token.extend_from_slice(value);
    }
    token
}

#[cfg(test)]
mod tests {
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
}
