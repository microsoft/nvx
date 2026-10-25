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
//! parameter that [`is_platform_token`] does not accept: the platform tier's allowlist, the same
//! as OpenVMM's capture of a platform snapshot allows. The kernel drops double quotes from a
//! parameter, and hands what follows `--` to init rather than taking it as parameters, though init
//! may still read it from `/proc/cmdline`, so `boot` also refuses a template's command line that
//! holds either, as OpenVMM refuses them in the command line of a VM with a control console.
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

use std::fmt;

use nvx::protocol::cmdline::{self, Parameter};

/// The parameter that names the platform snapshot tier, which OpenVMM adds to the kernel command
/// line of a template's VM.
pub const PLATFORM_TIER: &str = "nvx_snapshot_tier=platform";

/// The tokens of the platform tier's allowlist that it names whole: the consoles, the reboot
/// and panic settings, the sandbox's guest mode, NVX's configuration window, the tier itself, and
/// the control tty.
const PLATFORM_TOKENS: [&str; 9] = [
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

/// The prefixes of the platform tier's tokens whose values the host chooses: the processor
/// capacity, the virtio-mmio devices, and the addresses of the VM's network, if it has one.
const PLATFORM_PREFIXES: [&str; 8] = [
    "nr_cpus=",
    "virtio_mmio.device=",
    "virtnet_ip=",
    "virtnet_mask=",
    "virtnet_gw=",
    "virtnet_dns=",
    "virtnet_ip6=",
    "virtnet_gw6=",
];

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
    Template,
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
}

impl fmt::Display for CommandLineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .write_str("the kernel command line names the platform snapshot tier, but holds ")?;
        match self {
            Self::Token(token) => write!(
                formatter,
                "`{}`, which the tier's allowlist does not hold",
                token.escape_ascii()
            ),
            Self::Quote => formatter.write_str("a double quote, which the kernel drops"),
            Self::Delimiter => {
                formatter.write_str("`--`, after which the kernel hands the rest to init")
            }
        }
    }
}

impl std::error::Error for CommandLineError {}

/// Returns how a VM boots, as its kernel command line, `command_line`, such as `/proc/cmdline`,
/// tells, as the [module documentation](self) describes: a command line names the platform tier
/// if the kernel finds [`PLATFORM_TIER`] among its parameters.
///
/// Fails with a [`CommandLineError`] if the command line names the platform tier, but holds a
/// double quote, or else a `--`, or else a parameter that [`is_platform_token`] does not accept,
/// the first in the command line's order.
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
    match cmdline::parameters(command_line)
        .map(|parameter| token(&parameter))
        .find(|token| !is_platform_token(token))
    {
        Some(token) => Err(CommandLineError::Token(token)),
        None => Ok(Boot::Template),
    }
}

/// Returns whether `token`, a parameter of a kernel command line as the command line spells it,
/// is one of the platform tier's: `earlycon=xe9`, `console=hvc0`, `console=hvc1`, `reboot=t`,
/// `panic=-1`, `nvx_sandbox=1`, `nvx_config=0xd0010000,65536`, [`PLATFORM_TIER`], or
/// `nvx_control_tty=hvc2`, or a token that starts with `nr_cpus=`, `virtio_mmio.device=`,
/// `virtnet_ip=`, `virtnet_mask=`, `virtnet_gw=`, `virtnet_dns=`, `virtnet_ip6=`, or
/// `virtnet_gw6=`.
///
/// These are the tokens that OpenVMM's capture of a platform snapshot allows, byte for byte, so it
/// compares names exactly, though the kernel takes a dash in a parameter's name for an
/// underscore.
pub fn is_platform_token(token: &[u8]) -> bool {
    PLATFORM_TOKENS
        .iter()
        .any(|whole| token == whole.as_bytes())
        || PLATFORM_PREFIXES
            .iter()
            .any(|prefix| token.starts_with(prefix.as_bytes()))
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

    #[test]
    fn the_platform_tier_s_allowlist_is_openvmm_s() {
        for token in PLATFORM_TOKENS {
            assert!(is_platform_token(token.as_bytes()), "{token}");
        }
        for token in [
            "nr_cpus=1",
            "nr_cpus=8",
            "nr_cpus=",
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
        for command_line in [
            TEMPLATE.to_owned(),
            TEMPLATE.trim_end().to_owned(),
            network,
            // The tier alone, and each token whole once more, which OpenVMM leaves to its own
            // checks.
            format!("{PLATFORM_TIER}\n"),
            format!("{} {}\n", TEMPLATE.trim_end(), PLATFORM_TOKENS.join(" ")),
            // The kernel's whitespace: tabs, newlines, carriage returns, and no-break spaces.
            TEMPLATE.replace(' ', " \t\r\n\u{0b}\u{0c}"),
        ] {
            assert_eq!(
                boot(command_line.as_bytes()),
                Ok(Boot::Template),
                "{command_line:?}"
            );
        }
        let mut latin1 = TEMPLATE.as_bytes().to_vec();
        for byte in &mut latin1 {
            if *byte == b' ' {
                *byte = 0xa0;
            }
        }
        assert_eq!(boot(&latin1), Ok(Boot::Template));
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
        // what follows either in a platform token's value is a parameter of its own.
        for separator in ["\u{0b}", "\u{a0}"] {
            let command_line = format!(
                "{} virtnet_dns=10.0.2.3{separator}quiet\n",
                TEMPLATE.trim_end()
            );
            assert_eq!(
                boot(command_line.as_bytes()),
                Err(CommandLineError::Token(b"quiet".to_vec())),
                "{command_line:?}"
            );
        }
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
