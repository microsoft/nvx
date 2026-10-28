//! Tests of the kernel command line as Linux and the guest's init read it, and of the DNS server
//! that OpenVMM names in it. The expectations follow Linux 6.18: `lib/cmdline.c`,
//! `kernel/params.c`, `lib/vsprintf.c`, `lib/kstrtox.c`, and `drivers/virtio/virtio_mmio.c`; for
//! the init's tokens, the field splitting of the shell with which NVX's init, `guest/common/init`,
//! reads the command line, `for token in $cmdline` with the default `IFS`; and, for the DNS
//! server, OpenVMM's rendering and reading of its `virtnet_dns=` token, `openvmm_defs::microvm`'s
//! `command_line_fragment_with` and `microvm_command_line_dns_server`, whose vectors they take.
//! The [`model`] of `next_arg` and `parse_args` transcribes the kernel's code, and `parameters` must
//! read every command line as it does.

use proptest::prelude::*;

use super::*;

/// The MMIO base of the scratch block device of a layered root.
const SCRATCH: u64 = 0xd000_6000;

/// The network tokens that OpenVMM renders for the guest `10.0.0.2/24`, with the IPv6 identity
/// that it derives from it, before and after the place of a `virtnet_dns=` token.
const NETWORK: (&str, &str) = (
    "virtnet_ip=10.0.0.2 virtnet_mask=255.255.255.0 virtnet_gw=10.0.0.1",
    "virtnet_ip6=fd00::a00:2/120 virtnet_gw6=fd00::a00:1",
);

/// Checks that the kernel splits `command_line` into the parameters `expected`: that the model
/// reads them, and that [`parameters`] does.
fn assert_parameters(command_line: &[u8], expected: &[(&str, Option<&str>)]) {
    let expected: Vec<_> = expected
        .iter()
        .map(|&(name, value)| Parameter {
            name: name.as_bytes(),
            value: value.map(str::as_bytes),
        })
        .collect();
    assert_eq!(
        parameters(command_line).collect::<Vec<_>>(),
        expected,
        "{}",
        command_line.escape_ascii()
    );
    let expected: Vec<_> = expected
        .iter()
        .map(|parameter| (parameter.name.to_vec(), parameter.value.map(<[u8]>::to_vec)))
        .collect();
    assert_eq!(
        model::read(command_line).parameters,
        expected,
        "the model reads {}",
        command_line.escape_ascii()
    );
}

/// Returns the parameters that [`parameters`] reads from `command_line`, as the model reads them:
/// the name of each, and its value, if it has one.
fn owned(command_line: &[u8]) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    parameters(command_line)
        .map(|parameter| (parameter.name.to_vec(), parameter.value.map(<[u8]>::to_vec)))
        .collect()
}

/// Checks that the guest's init splits `command_line` into the tokens `expected`: that
/// [`init_tokens`] reads the name of each, and its value, if it has one.
fn assert_init_tokens(command_line: &[u8], expected: &[(&[u8], Option<&[u8]>)]) {
    let expected: Vec<_> = expected
        .iter()
        .map(|&(name, value)| Token { name, value })
        .collect();
    assert_eq!(
        init_tokens(command_line).collect::<Vec<_>>(),
        expected,
        "{}",
        command_line.escape_ascii()
    );
}

/// Returns a strategy for command lines: mostly the bytes that `next_arg` treats apart, letters,
/// and parameters that NVX reserves, and now and then any byte.
fn command_line() -> impl Strategy<Value = Vec<u8>> {
    /// The pieces of the command lines: a quote, an equals sign, dashes, an underscore, the
    /// whitespace of `isspace`, a UTF-8 character that ends with its no-break space, a NUL, and
    /// letters and parameter names.
    const PIECES: [&[u8]; 19] = [
        b"\"",
        b"=",
        b"-",
        b"--",
        b"_",
        b" ",
        b"\t",
        b"\n",
        b"\x0b",
        b"\x0c",
        b"\r",
        b"\xa0",
        b"\xc3\xa0",
        b"\0",
        b"a",
        b"b",
        b"nvx_x=",
        b"tsc=",
        b"hostname",
    ];
    prop::collection::vec(
        prop_oneof![
            8 => prop::sample::select(PIECES.to_vec()).prop_map(<[u8]>::to_vec),
            1 => any::<u8>().prop_map(|byte| vec![byte]),
        ],
        0..32,
    )
    .prop_map(|pieces| pieces.concat())
}

/// Returns the device of `size` bytes at `base` with `irq`, and with `id` if any.
fn device(size: u64, base: u64, irq: u32, id: Option<i32>) -> Option<VirtioMmioDevice> {
    Some(VirtioMmioDevice {
        size,
        base,
        irq,
        id,
    })
}

#[test]
fn whitespace_is_what_the_kernel_takes_for_it() {
    for byte in 0..=u8::MAX {
        let expected = matches!(byte, 0x09..=0x0d | 0x20 | 0xa0);
        assert_eq!(is_space(byte), expected, "{byte:#04x}");
    }
}

#[test]
fn command_lines_split_into_parameters_as_the_kernel_splits_them() {
    assert_parameters(b"a b=c", &[("a", None), ("b", Some("c"))]);
    assert_parameters(
        b" a\x0bb\x0cc\rd\xa0e\tf\ng=h ",
        &[
            ("a", None),
            ("b", None),
            ("c", None),
            ("d", None),
            ("e", None),
            ("f", None),
            ("g", Some("h")),
        ],
    );
    assert_parameters(b"", &[]);
    assert_parameters(b" \t\n", &[]);
    assert_parameters(b"\"a b\" c", &[("a b", None), ("c", None)]);
    assert_parameters(b"\"a=b c\" d", &[("a", Some("b c")), ("d", None)]);
    assert_parameters(b"a=\"b c\" d", &[("a", Some("b c")), ("d", None)]);
    assert_parameters(b"a=\"b c", &[("a", Some("b c"))]);
    assert_parameters(b"a=\"\" b=c\"", &[("a", Some("")), ("b", Some("c\""))]);
    assert_parameters(b"a=\"", &[("a", Some(""))]);
    assert_parameters(b"x=\"a b=c\"", &[("x", Some("a b=c"))]);
    assert_parameters(b"a\"b c\"d e", &[("a\"b c\"d", None), ("e", None)]);
    assert_parameters(b"=a=b", &[("=a", Some("b"))]);
    assert_parameters(b"a=", &[("a", Some(""))]);
    assert_parameters(b"\"", &[("", None)]);
    // The kernel stops at a NUL, and at a `--` without a value.
    assert_parameters(b"a\0b", &[("a", None)]);
    assert_parameters(b"a -- b", &[("a", None)]);
    assert_parameters(b"a \"--\" b", &[("a", None)]);
    assert_parameters(b"a --=1 b", &[("a", None), ("--", Some("1")), ("b", None)]);
    assert_parameters(b"a ---- b", &[("a", None), ("----", None), ("b", None)]);
    // Once it stopped, it stays stopped.
    let mut split = parameters(b"a -- b");
    assert_eq!(
        split.next().map(|parameter| parameter.name),
        Some(&b"a"[..])
    );
    assert_eq!(split.next(), None);
    assert_eq!(split.next(), None);
}

#[test]
fn parameter_names_compare_as_the_kernel_compares_them() {
    let named = |name: &str, expected: &str| {
        Parameter {
            name: name.as_bytes(),
            value: None,
        }
        .is_named(expected)
    };
    for name in ["virtio_mmio.device", "virtio-mmio.device"] {
        assert!(named(name, VIRTIO_MMIO_DEVICE), "{name}");
    }
    assert!(named("a_b", "a-b"));
    assert!(named("a-b", "a_b"));
    for name in [
        "virtio-mmio-device",
        "virtio_mmio_device",
        "virtio_mmio.devices",
        "virtio.mmio.device",
        "Virtio_mmio.device",
        "",
    ] {
        assert!(!named(name, VIRTIO_MMIO_DEVICE), "{name}");
    }
}

#[test]
fn the_kernel_leaves_what_follows_a_delimiter_to_init() {
    let cases: [(&[u8], Option<&[u8]>); 8] = [
        (b"a -- b  c", Some(b"b  c")),
        (b"a \"--\"\tb", Some(b"b")),
        (b"a\xa0--\xa0b", Some(b"b")),
        (b"--", Some(b"")),
        (b"a --=1 b", None),
        (b"a ---- b", None),
        (b"a=\"-- b\"", None),
        (b"a", None),
    ];
    for (command_line, rest) in cases {
        assert_eq!(
            model::read(command_line).rest.as_deref(),
            rest,
            "{}",
            command_line.escape_ascii()
        );
    }
}

#[test]
fn the_model_compares_names_as_the_kernel_does() {
    // `parameq` compares whole names, and `parameqn` their first bytes, with dashes and
    // underscores alike; past its end, a string reads as its NUL.
    assert!(model::parameq(b"virtio-mmio.device", b"virtio_mmio.device"));
    assert!(model::parameq(b"a_b", b"a-b"));
    assert!(!model::parameq(b"tsc", b"tsc_early_khz"));
    assert!(!model::parameq(b"tsc_early_khz", b"tsc"));
    assert!(model::parameqn(b"notsc=1", b"notsc", 5));
    assert!(model::parameqn(b"notscx", b"notsc", 5));
    assert!(!model::parameqn(b"nots", b"notsc", 5));
    assert!(model::parameqn(b"tsc=unstable", b"tsc=", 4));
    assert!(!model::parameqn(b"tsc", b"tsc=", 4));
    // `repair_env_string` restores the equals sign of a parameter with a value.
    assert_eq!(model::line(b"tsc", Some(b"unstable")), b"tsc=unstable");
    assert_eq!(model::line(b"a", Some(b"")), b"a=");
    assert_eq!(model::line(b"quiet", None), b"quiet");
    assert_eq!(
        model::read(b"\"tsc=unstable\" a=\"b c\"").lines(),
        [&b"tsc=unstable"[..], b"a=b c"]
    );
}

#[test]
fn parameters_follow_the_model_on_every_short_command_line() {
    /// The bytes that `next_arg` treats apart, and a letter.
    const ALPHABET: [u8; 7] = [b'"', b'=', b'-', b' ', 0xa0, 0, b'a'];
    for length in 0..=6 {
        for mut index in 0..ALPHABET.len().pow(length) {
            let command_line: Vec<u8> = (0..length)
                .map(|_| {
                    let byte = ALPHABET[index % ALPHABET.len()];
                    index /= ALPHABET.len();
                    byte
                })
                .collect();
            assert_eq!(
                owned(&command_line),
                model::read(&command_line).parameters,
                "{}",
                command_line.escape_ascii()
            );
        }
    }
}

proptest! {
    /// `parameters` reads every command line as the kernel's `parse_args` and `next_arg` read it.
    #[test]
    fn parameters_follow_the_model(command_line in command_line()) {
        prop_assert_eq!(owned(&command_line), model::read(&command_line).parameters);
    }

    /// `Parameter::is_named` compares names as the kernel's `parameq` does.
    #[test]
    fn names_compare_as_the_model_compares_them(
        name in "[-_.ab]{0,5}",
        other in "[-_.ab]{0,5}",
    ) {
        let parameter = Parameter { name: name.as_bytes(), value: None };
        prop_assert_eq!(
            parameter.is_named(&other),
            model::parameq(name.as_bytes(), other.as_bytes())
        );
    }
}

#[test]
fn device_values_read_as_the_driver_reads_them() {
    for (value, expected) in [
        ("0x1000@0xd0006000:11", device(0x1000, SCRATCH, 11, None)),
        ("4K@0XD0006000:11:3", device(4096, SCRATCH, 11, Some(3))),
        ("4k@0xd0006000:11", device(4096, SCRATCH, 11, None)),
        ("1G@0xd0006000:11", device(1 << 30, SCRATCH, 11, None)),
        ("2e@0xd0006000:11", device(2 << 60, SCRATCH, 11, None)),
        ("0x1E@0xd0006000:11", device(0x1e, SCRATCH, 11, None)),
        ("4096@3489685504:11", device(4096, SCRATCH, 11, None)),
        ("4096@032000060000:11", device(4096, SCRATCH, 11, None)),
        ("010@0xd0006000:11", device(8, SCRATCH, 11, None)),
        ("0x1000@ \t0xd0006000:11", device(0x1000, SCRATCH, 11, None)),
        ("0x1000@0xd0003000:4", device(0x1000, 0xd000_3000, 4, None)),
        // `memparse` takes a suffix even without digits, and reads no size at all as 0. The
        // kernel then fails to claim the empty region, which the driver's reading leaves to it.
        ("K@0xd0006000:11", device(0, SCRATCH, 11, None)),
        ("@0xd0006000:11", device(0, SCRATCH, 11, None)),
        // A size wraps at 64 bits, and so does a base, which a minus sign negates.
        ("16E@0xd0006000:11", device(0, SCRATCH, 11, None)),
        (
            "0x1000@-0xffffffff2fffa000:11",
            device(0x1000, SCRATCH, 11, None),
        ),
        (
            "0x1000@0x100000000d0006000:11",
            device(0x1000, SCRATCH, 11, None),
        ),
        ("0x1000@0x10000000000000000:11", device(0x1000, 0, 11, None)),
        ("0x1000@-1:11", device(0x1000, u64::MAX, 11, None)),
        // IRQ and ID are decimal, after any whitespace, and keep their low 32 bits.
        ("0x1000@0xd0006000: 11", device(0x1000, SCRATCH, 11, None)),
        ("0x1000@0xd0006000:011", device(0x1000, SCRATCH, 11, None)),
        (
            "0x1000@0xd0006000:4294967307",
            device(0x1000, SCRATCH, 11, None),
        ),
        (
            "0x1000@0xd0006000:11:-2",
            device(0x1000, SCRATCH, 11, Some(-2)),
        ),
        (
            "0x1000@0xd0006000:11:\t7",
            device(0x1000, SCRATCH, 11, Some(7)),
        ),
        (
            "0x1000@0xd0006000:11:010",
            device(0x1000, SCRATCH, 11, Some(10)),
        ),
        (
            "0x1000@0xd0006000:11:4294967303",
            device(0x1000, SCRATCH, 11, Some(7)),
        ),
        (
            "0x1000@0xd0006000:11:2147483648",
            device(0x1000, SCRATCH, 11, Some(i32::MIN)),
        ),
        // The driver refuses an IRQ of 0, a value without an IRQ, and text after the last integer.
        ("0x1000@0xd0006000:0", None),
        ("0x1000@0xd0006000:4294967296", None),
        ("0x1000@0xd0006000", None),
        ("0x1000@0xd0006000:", None),
        ("0x1000@0xd0006000:-11", None),
        ("0x1000@0xd0006000:+11", None),
        ("0x1000@0xd0006000:0x11", None),
        ("0x1000@0xd0006000:11x", None),
        ("0x1000@0xd0006000:11 ", None),
        ("0x1000@0xd0006000:11:", None),
        ("0x1000@0xd0006000:11:x", None),
        ("0x1000@0xd0006000:11:-", None),
        ("0x1000@0xd0006000:11:7x", None),
        ("0x1000@0xd0006000:11:2:3", None),
        // It needs `@` right after the size, and a base that starts with a digit.
        ("0xd0006000:11", None),
        ("0x1000@:11", None),
        ("0x1000@0x:11", None),
        ("0x1000@-:11", None),
        ("0x1000@--1:11", None),
        ("0x1000@+3489685504:11", None),
        ("0x1000@0xd0006000x:11", None),
        ("0x1000@0xd0006000 :11", None),
        ("0x1000@09:11", None),
        ("0x1000 @0xd0006000:11", None),
        (" 0x1000@0xd0006000:11", None),
        ("-0x1000@0xd0006000:11", None),
        ("4KB@0xd0006000:11", None),
        ("x@0xd0006000:11", None),
        ("", None),
    ] {
        assert_eq!(
            VirtioMmioDevice::parse(value.as_bytes()),
            expected,
            "{value}"
        );
    }
}

#[test]
fn devices_come_from_the_parameters_that_the_kernel_reads() {
    let command_line = b"quiet virtio_mmio.device=0x1000@0xd0003000:4 \
        virtio-mmio.device=4K@0xd0006000:11:3 \"virtio_mmio.device=0x1000@0xd0007000:3\" \
        virtio_mmio.device virtio_mmio.device=0x1000@0xd0008000:0 \
        virtio_mmio_device=0x1000@0xd0009000:5 x=\"virtio_mmio.device=0x1000@0xd000a000:6\" \
        virtio_mmio.device=\"0x1000@ 0xd000b000:7\" -- virtio_mmio.device=0x1000@0xd000c000:8";
    let devices: Vec<_> = virtio_mmio_devices(command_line).collect();
    assert_eq!(
        devices,
        [
            device(0x1000, 0xd000_3000, 4, None).unwrap(),
            device(4096, SCRATCH, 11, Some(3)).unwrap(),
            device(0x1000, 0xd000_7000, 3, None).unwrap(),
            device(0x1000, 0xd000_b000, 7, None).unwrap(),
        ]
    );
    assert_eq!(virtio_mmio_devices(b"").count(), 0);
}

#[test]
fn the_init_splits_only_at_spaces_tabs_and_newlines() {
    // Every other byte stays in its token: those that the kernel takes for whitespace too, the
    // vertical tab, the form feed, the carriage return, and 0xa0, and a quote and a NUL.
    for byte in 0..=u8::MAX {
        let command_line = [b'a', byte, b'b'];
        match byte {
            b' ' | b'\t' | b'\n' => {
                assert_init_tokens(&command_line, &[(b"a", None), (b"b", None)]);
            }
            b'=' => assert_init_tokens(&command_line, &[(b"a", Some(b"b"))]),
            _ => assert_init_tokens(&command_line, &[(&command_line, None)]),
        }
    }
}

#[test]
fn command_lines_split_into_tokens_as_the_init_splits_them() {
    assert_init_tokens(b"a b=c", &[(b"a", None), (b"b", Some(b"c"))]);
    // The init finds no empty token before, between, or after the bytes at which it splits.
    assert_init_tokens(
        b" \ta \n\tb\t\t c\n",
        &[(b"a", None), (b"b", None), (b"c", None)],
    );
    assert_init_tokens(b"", &[]);
    assert_init_tokens(b" \t\n", &[]);
    // A name ends at the token's first `=`, even one that starts the token, at which the kernel's
    // would not end, and the value holds the rest.
    assert_init_tokens(b"=", &[(b"", Some(b""))]);
    assert_init_tokens(b"=a=b", &[(b"", Some(b"a=b"))]);
    assert_init_tokens(b"a=", &[(b"a", Some(b""))]);
    assert_init_tokens(b"a=b=c", &[(b"a", Some(b"b=c"))]);
    // The init honors no quotes, and reads past a NUL and a `--`, at which the kernel stops.
    assert_init_tokens(
        b"\"a b\" c=\"d e\"",
        &[
            (b"\"a", None),
            (b"b\"", None),
            (b"c", Some(b"\"d")),
            (b"e\"", None),
        ],
    );
    assert_init_tokens(b"a\0b c", &[(b"a\0b", None), (b"c", None)]);
    assert_init_tokens(
        b"a -- b=c",
        &[(b"a", None), (b"--", None), (b"b", Some(b"c"))],
    );
}

#[test]
fn the_dns_server_is_read_as_openvmm_renders_it() {
    let ipv4: IpAddr = "10.0.0.1".parse().unwrap();
    let ipv6: IpAddr = "fd00::a00:1".parse().unwrap();
    // OpenVMM names either gateway, in its canonical text, or none, among its network tokens, and
    // the kernel appends a newline to `/proc/cmdline`.
    let (before, after) = NETWORK;
    for (dns, expected) in [
        (" virtnet_dns=10.0.0.1", Some(ipv4)),
        (" virtnet_dns=fd00::a00:1", Some(ipv6)),
        ("", None),
    ] {
        let command_line = format!("console=hvc0 {before}{dns} {after}\n");
        assert_eq!(
            dns_server(command_line.as_bytes()),
            Ok(expected),
            "{command_line}"
        );
    }
    // A token is one that starts with `virtnet_dns=` between ASCII whitespace, without quoting,
    // as OpenVMM splits the command line, and past a `--`, where the kernel stops.
    for command_line in [
        &b"virtnet_dns=10.0.0.1"[..],
        b"a\tvirtnet_dns=10.0.0.1\nb",
        b"a\x0cvirtnet_dns=10.0.0.1\rb",
        b"quiet -- virtnet_dns=10.0.0.1",
        b"\"a virtnet_dns=10.0.0.1",
    ] {
        assert_eq!(
            dns_server(command_line),
            Ok(Some(ipv4)),
            "{}",
            command_line.escape_ascii()
        );
    }
    for command_line in [
        &b""[..],
        b"virtnet_dns",
        b"virtnet_dns =10.0.0.1",
        b"virtnet-dns=10.0.0.1",
        b"VIRTNET_DNS=10.0.0.1",
        b"xvirtnet_dns=10.0.0.1",
        b"\"virtnet_dns=10.0.0.1\"",
        b"a=virtnet_dns=10.0.0.1",
    ] {
        assert_eq!(
            dns_server(command_line),
            Ok(None),
            "{}",
            command_line.escape_ascii()
        );
    }
}

#[test]
fn the_dns_server_is_one_address_in_one_token() {
    // OpenVMM's refusals of the token itself, and the spellings that it never renders: its reader
    // refuses each of them too. A vertical tab or a no-break space ends a parameter for the kernel,
    // but not a token for OpenVMM.
    for command_line in [
        &b"virtnet_dns=fd00:0::a00:1"[..],
        b"virtnet_dns=010.0.0.1",
        b"virtnet_dns=10.0.0.1,fd00::a00:1",
        b"virtnet_dns=10.0.0.1 virtnet_dns=fd00::a00:1",
        b"virtnet_dns=10.0.0.1 virtnet_dns=10.0.0.1",
        b"virtnet_dns=10.0.0.1 virtnet_dns=",
        b"virtnet_dns=bad virtnet_dns=10.0.0.1",
        b"virtnet_dns=",
        b"virtnet_dns= 10.0.0.1",
        b"virtnet_dns=FD00::A00:1",
        b"virtnet_dns=fd00::0a00:1",
        b"virtnet_dns=10.0.0.1:53",
        b"virtnet_dns=[fd00::a00:1]",
        b"virtnet_dns=fd00::a00:1%eth0",
        b"virtnet_dns=10.0.0.1/24",
        b"virtnet_dns=10.0.0",
        b"virtnet_dns=\"10.0.0.1\"",
        b"virtnet_dns=10.0.0.1\x0b",
        b"virtnet_dns=10.0.0.1\xa0",
        b"virtnet_dns=gateway",
    ] {
        assert!(
            dns_server(command_line).is_err(),
            "{}",
            command_line.escape_ascii()
        );
    }
    assert_eq!(
        dns_server(b"virtnet_dns=10.0.0.1 virtnet_dns=fd00::a00:1")
            .unwrap_err()
            .message(),
        "the virtnet_dns kernel parameter is repeated"
    );
    assert_eq!(
        dns_server(b"quiet virtnet_dns=010.0.0.1\xa0")
            .unwrap_err()
            .message(),
        "the virtnet_dns kernel parameter must name one IPv4 or IPv6 address as OpenVMM renders \
         it, not '010.0.0.1\\xa0'"
    );
}

#[test]
fn the_gateway_is_one_ipv4_address_in_one_token_as_openvmm_renders_it() {
    // OpenVMM names the IPv4 gateway of a guest with a NIC among its network tokens, and its IPv6
    // gateway with another token, and the kernel appends a newline to `/proc/cmdline`.
    let (before, after) = NETWORK;
    let command_line = format!("console=hvc0 {before} virtnet_dns=10.0.0.1 {after}\n");
    assert_eq!(
        gateway(command_line.as_bytes()),
        Ok(Some(Ipv4Addr::new(10, 0, 0, 1)))
    );
    for command_line in [
        &b""[..],
        b"console=hvc0 virtnet_gw6=fd00::a00:1",
        b"virtnet_gw",
        b"xvirtnet_gw=10.0.0.1",
    ] {
        assert_eq!(
            gateway(command_line),
            Ok(None),
            "{}",
            command_line.escape_ascii()
        );
    }
    for command_line in [
        &b"virtnet_gw="[..],
        b"virtnet_gw=010.0.0.1",
        b"virtnet_gw=10.0.0.1/24",
        b"virtnet_gw=10.0.0.1,10.0.0.2",
        b"virtnet_gw=10.0.0.1 virtnet_gw=10.0.0.1",
        b"virtnet_gw=10.0.0.1\xa0",
    ] {
        assert!(
            gateway(command_line).is_err(),
            "{}",
            command_line.escape_ascii()
        );
    }
    assert_eq!(
        gateway(b"virtnet_gw=10.0.0.1 virtnet_gw=10.0.0.2")
            .unwrap_err()
            .message(),
        "the virtnet_gw kernel parameter is repeated"
    );
    assert_eq!(
        gateway(b"virtnet_gw=fd00::a00:1").unwrap_err().message(),
        "the virtnet_gw kernel parameter must name one IPv4 address as OpenVMM renders it, not \
         'fd00::a00:1'"
    );
}

#[test]
fn a_resolver_file_names_the_server_as_the_init_writes_it() {
    for (server, file) in [
        ("10.0.0.1", "nameserver 10.0.0.1\n"),
        ("fd00::a00:1", "nameserver fd00::a00:1\n"),
        // An address in another spelling is written in its canonical text.
        ("fd00:0:0::0a00:1", "nameserver fd00::a00:1\n"),
    ] {
        let server: IpAddr = server.parse().unwrap();
        let mut written = Vec::new();
        write_resolver(&mut written, server).unwrap();
        assert_eq!(String::from_utf8(written).unwrap(), file);
        // The text is the one that the reader takes from OpenVMM's token.
        let command_line = format!("{DNS_TOKEN}{}", file["nameserver ".len()..].trim_end());
        assert_eq!(dns_server(command_line.as_bytes()), Ok(Some(server)));
    }
    // The writer's failure is the call's.
    struct Full;
    impl io::Write for Full {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("the disk is full"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let error = write_resolver(Full, "10.0.0.1".parse().unwrap()).unwrap_err();
    assert_eq!(error.to_string(), "the disk is full");
}
