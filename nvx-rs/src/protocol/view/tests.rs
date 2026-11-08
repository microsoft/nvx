use super::*;

#[test]
fn view_tokens_preserve_hostname_bytes_and_both_machine_id_modes() {
    for hostname in [
        b"x".to_vec(),
        b"box".to_vec(),
        b" \t\n\"=\\\x80\xff".to_vec(),
        vec![0xff; MAX_HOSTNAME_BYTES],
    ] {
        for machine_id in [MachineIdMode::Image, MachineIdMode::Generated] {
            let view = View {
                hostname: hostname.clone(),
                machine_id,
            };
            let rendered = tokens(&view).unwrap();
            assert_eq!(parse(rendered.join(" ").as_bytes()).unwrap(), Some(view));
        }
    }
    assert_eq!(
        tokens(&View {
            hostname: b"box".to_vec(),
            machine_id: MachineIdMode::Generated,
        })
        .unwrap(),
        ["nvx_hostname=626f78", "nvx_machine_id=generated"]
    );
}

#[test]
fn a_view_is_absent_only_when_neither_exact_init_token_is_present() {
    for unrelated in [
        "",
        "quiet nvx-hostname=box nvx-machine-id=unknown",
        "\"nvx_hostname=626f78\" other=nvx_machine_id=image",
    ] {
        assert_eq!(parse(unrelated.as_bytes()).unwrap(), None);
    }
    assert_eq!(
        parse(b"quiet\t-- nvx_machine_id=image\nnvx_hostname=626f78").unwrap(),
        Some(View {
            hostname: b"box".to_vec(),
            machine_id: MachineIdMode::Image,
        })
    );
}

#[test]
fn hostname_values_must_be_bounded_canonical_hexadecimal_without_a_nul() {
    let mut malformed: Vec<Vec<u8>> = [
        b"".as_slice(),
        b"0",
        b"gg",
        b"6A",
        b"0x62",
        b"box",
        b"62006f",
        b"62=6f",
        b"62\r",
        b"62\0",
        b"\"62\"",
        b"\xff\xff",
    ]
    .into_iter()
    .map(<[u8]>::to_vec)
    .collect();
    malformed.push(b"62".repeat(MAX_HOSTNAME_BYTES + 1));
    for value in malformed {
        let line = [
            b"nvx_hostname=".as_slice(),
            &value,
            b" nvx_machine_id=image",
        ]
        .concat();
        let error = parse(&line).unwrap_err();
        assert!(error.to_string().contains("nvx_hostname"), "{error}");
    }
    for hostname in [
        Vec::new(),
        vec![b'a'; MAX_HOSTNAME_BYTES + 1],
        b"a\0b".to_vec(),
    ] {
        assert!(
            tokens(&View {
                hostname,
                machine_id: MachineIdMode::Image,
            })
            .is_err()
        );
    }
}

#[test]
fn machine_id_values_are_exact_and_unquoted() {
    for value in [
        b"".as_slice(),
        b"Image",
        b"generate",
        b"image=generated",
        b"image\r",
        b"\"image\"",
        b"\xff",
    ] {
        let line = [b"nvx_hostname=626f78 nvx_machine_id=".as_slice(), value].concat();
        let error = parse(&line).unwrap_err();
        assert!(error.to_string().contains("nvx_machine_id"), "{error}");
    }
}

#[test]
fn partial_missing_and_duplicate_owned_tokens_are_refused() {
    for line in [
        "nvx_hostname=626f78",
        "nvx_machine_id=image",
        "nvx_hostname nvx_machine_id=image",
        "nvx_hostname=626f78 nvx_machine_id",
        "nvx_hostname=626f78 nvx_hostname=626f78 nvx_machine_id=image",
        "nvx_hostname=626f78 nvx_hostname=61 nvx_machine_id=image",
        "nvx_hostname=626f78 nvx_machine_id=image nvx_machine_id=image",
        "nvx_hostname=626f78 nvx_machine_id=image nvx_machine_id=generated",
    ] {
        assert!(parse(line.as_bytes()).is_err(), "{line}");
    }
}
