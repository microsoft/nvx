//! Cold-run shares stay kernel tokens through the checked VM configuration; restore keeps its
//! existing policy-path lowering.

use proptest::prelude::*;

use super::*;
use crate::protocol::cmdline::{self, parameters, virtio_mmio_devices};

/// Models the kernel's discovery of the filesystem, control console, and boot console around the
/// command line rendered by the actual cold VM configuration.
fn effective_command_line(config: &VmConfig) -> String {
    format!(
        "earlycon=xe9 console=hvc1 reboot=t panic=-1 {} \
         virtio_mmio.device=0x1000@0xd0001000:17 virtio_mmio.device=0x1000@0xd0002000:18 \
         virtio_mmio.device=0x1000@0xd0003000:19",
        config.kernel_command_line().unwrap()
    )
}

/// Returns a strategy for the last component of a guest target: mostly characters that a target
/// may hold, and now and then a NUL, a quote, or a character with a byte that Linux reads as
/// whitespace. U+00A0, U+00E0, and U+0120 end in the byte 0xa0 in UTF-8.
fn component() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            12 => prop::char::range('a', 'z'),
            3 => prop::sample::select(vec!['0', '-', '_', '\u{e9}', '\u{4e2d}']),
            1 => prop::sample::select(vec![
                '\0', '\t', '\n', ' ', '"', '\u{a0}', '\u{e0}', '\u{120}',
            ]),
        ],
        1..12,
    )
    .prop_map(String::from_iter)
}

#[test]
fn single_share_targets_must_remain_one_kernel_token() {
    let directory = tempfile::tempdir().unwrap();
    for separator in ['\0', '\t', '\n', ' ', '"', '\u{a0}', '\u{120}'] {
        let options = Options {
            mounts: vec![format!(
                "/workspace/a{separator}b,{}",
                directory.path().display()
            )],
            ..Options::default()
        };
        let rules = options.network.egress_rules().unwrap();
        assert!(cold_config(&options, &rules).is_err());
    }
}

#[test]
fn cold_share_command_lines_fit_the_kernel_budget_to_the_byte() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let mut options = Options {
        mounts: vec![
            format!("/a,{},ro", first.display()),
            format!("/b,{},rw", second.display()),
        ],
        ..Options::default()
    };
    let rules = options.network.egress_rules().unwrap();
    let base = cold_config(&options, &rules).unwrap();
    let fragment = export_command_line_fragment(base.filesystem.as_ref().unwrap());
    let room = SANDBOX_COMMAND_LINE_MAX_SIZE
        - base.kernel_command_line().unwrap().len()
        - fragment.len()
        - 2;
    for unicode in [false, true] {
        options.cmdline = if unicode {
            "\u{e9}".repeat(room / 2) + &"x".repeat(room % 2)
        } else {
            "x".repeat(room)
        };
        let fitting = cold_config(&options, &rules).unwrap();
        assert_eq!(
            fitting.kernel_command_line().unwrap().len() + fragment.len() + 1,
            SANDBOX_COMMAND_LINE_MAX_SIZE
        );
        options.cmdline.push('x');
        assert!(cold_config(&options, &rules).is_err());
    }
}

#[test]
fn a_console_peer_exit_waits_for_the_owned_child_status() {
    let result = connect_before_exit::<()>(
        "unused",
        || {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "peer exited",
            ))
        },
        |timeout| Ok((!timeout.is_zero()).then_some(7)),
    );
    assert!(matches!(result, Ok(None)), "{result:?}");
}

#[test]
fn an_unverified_console_is_refused_while_the_owned_child_runs() {
    let result = connect_before_exit::<()>(
        "unused",
        || {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "wrong peer",
            ))
        },
        |_| Ok(None),
    );
    assert!(
        matches!(&result, Err(Exit::Error { code: ErrorCode::Io, message }) if message == "wrong peer"),
        "{result:?}"
    );
}

#[cfg(windows)]
#[test]
fn windows_console_output_handles_non_utf8_code_pages() {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NEW_CONSOLE;

    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "host::cli::run::tests::windows_console_output_helper",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("NVX_TEST_PRIVATE_CONSOLE", "cp437")
        .creation_flags(CREATE_NEW_CONSOLE)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

/// Runs only as the parent's isolated-console helper, never in the shared test process.
#[cfg(windows)]
#[test]
#[ignore]
fn windows_console_output_helper() {
    use std::io::Write;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{
        GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    use windows_sys::Win32::System::Console::{
        CONSOLE_TEXTMODE_BUFFER, COORD, CreateConsoleScreenBuffer, GetConsoleOutputCP,
        GetConsoleProcessList, GetStdHandle, ReadConsoleOutputCharacterW, STD_OUTPUT_HANDLE,
        SetConsoleOutputCP, SetStdHandle,
    };

    /// Restores only the helper's private console and its original redirected stdout.
    struct RestoreConsole {
        /// The original borrowed standard-output handle.
        stdout: HANDLE,
        /// The private console's original output code page.
        code_page: u32,
    }
    impl Drop for RestoreConsole {
        fn drop(&mut self) {
            // SAFETY: both settings belong to this isolated helper, and stdout stays open.
            unsafe {
                assert_ne!(SetStdHandle(STD_OUTPUT_HANDLE, self.stdout), 0);
                assert_ne!(SetConsoleOutputCP(self.code_page), 0);
            }
        }
    }

    assert_eq!(
        std::env::var("NVX_TEST_PRIVATE_CONSOLE").as_deref(),
        Ok("cp437")
    );
    let mut members = [0; 4];
    // SAFETY: the array is writable, and only this helper may belong to its new console.
    let count = unsafe { GetConsoleProcessList(members.as_mut_ptr(), members.len() as u32) };
    assert_eq!(count, 1);
    assert_eq!(members[0], std::process::id());
    // SAFETY: a private, inactive screen buffer has no inherited security attributes.
    let raw = unsafe {
        CreateConsoleScreenBuffer(
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            CONSOLE_TEXTMODE_BUFFER,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(raw, INVALID_HANDLE_VALUE);
    // SAFETY: CreateConsoleScreenBuffer returned this newly owned handle.
    let buffer = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
    io::stdout().flush().unwrap();
    // SAFETY: these getters return the helper's own current console settings.
    let _restore = RestoreConsole {
        stdout: unsafe { GetStdHandle(STD_OUTPUT_HANDLE) },
        code_page: unsafe { GetConsoleOutputCP() },
    };
    // SAFETY: only this helper is attached, and the owned buffer remains open until restoration.
    unsafe {
        assert_ne!(SetConsoleOutputCP(437), 0);
        assert_ne!(
            SetStdHandle(STD_OUTPUT_HANDLE, buffer.as_raw_handle().cast()),
            0
        );
    }
    for chunk in [
        b"caf\xc3".as_slice(),
        b"\xa9 \xe2".as_slice(),
        b"\x82\xac".as_slice(),
    ] {
        write_console(chunk).unwrap();
    }
    let expected: Vec<u16> = "caf\u{e9} \u{20ac}".encode_utf16().collect();
    let mut actual = vec![0; expected.len()];
    let mut read = 0;
    // SAFETY: the buffer is owned, and actual has room for the requested UTF-16 characters.
    assert_ne!(
        unsafe {
            ReadConsoleOutputCharacterW(
                buffer.as_raw_handle().cast(),
                actual.as_mut_ptr(),
                actual.len() as u32,
                COORD { X: 0, Y: 0 },
                &mut read,
            )
        },
        0
    );
    assert_eq!(read as usize, expected.len());
    assert_eq!(actual, expected);
}

proptest! {
    /// A target that `run` accepts stays inside its share's `nvx_share=` token, which the kernel
    /// reads as one parameter. Linux splits a parameter at a byte that it reads as whitespace,
    /// stops at a NUL, and reads everything after a lone quote as part of the parameter, so a
    /// target with one of them would split its token, or hide the virtio-mmio devices that OpenVMM
    /// describes after the shares from the guest's kernel (microsoft/nvx#798).
    #[test]
    fn share_tokens_hide_no_virtio_mmio_device(
        components in prop::collection::vec(component(), 2..5),
    ) {
        let directory = tempfile::tempdir().unwrap();
        let roots: Vec<_> = (0..components.len()).map(|index| directory.path().join(format!("host-{index}"))).collect();
        for root in &roots { std::fs::create_dir(root).unwrap(); }
        let options = Options {
            mounts: components
                .iter()
                .enumerate()
                .map(|(index, component)| format!("/share-{index}/{component},{},rw", roots[index].display()))
                .collect(),
            ..Options::default()
        };
        let rules = options.network.egress_rules().unwrap();
        let result = cold_config(&options, &rules);
        let separated = components.iter().any(|component| {
            component
                .bytes()
                .any(|byte| matches!(byte, b'\0' | b'"') || cmdline::is_space(byte))
        });
        prop_assert_eq!(result.is_err(), separated);
        if let Ok(config) = result {
            let line = effective_command_line(&config);
            let read: Vec<String> = parameters(line.as_bytes())
                .map(|parameter| {
                    let mut text = parameter.name.to_vec();
                    if let Some(value) = parameter.value {
                        text.push(b'=');
                        text.extend_from_slice(value);
                    }
                    String::from_utf8_lossy(&text).into_owned()
                })
                .collect();
            prop_assert_eq!(read, line.split(' ').collect::<Vec<_>>());
            prop_assert_eq!(virtio_mmio_devices(line.as_bytes()).count(), 3);
        }
    }

    #[test]
    fn restore_policy_paths_attach_to_their_shares(
        events in prop::collection::vec((any::<bool>(), 0_u8..3, any::<bool>()), 0..40),
    ) {
        let directory = tempfile::tempdir().unwrap();
        let roots = [directory.path().join("first"), directory.path().join("second")];
        for root in &roots {
            std::fs::create_dir(root).unwrap();
        }
        let mut arguments = Vec::new();
        let mut policies: [Vec<String>; 3] = Default::default();
        for (sequence, (second, kind, mount_first)) in events.into_iter().enumerate() {
            let root = &roots[usize::from(second)];
            let path = root.join(format!("path-{sequence}")).to_str().unwrap().to_owned();
            let option = ["--mount-deny", "--mount-allow", "--mount-write"][usize::from(kind)];
            let mount = format!("/share-{sequence},{},rw", root.display());
            let policy = [option.to_owned(), path.clone()];
            let share = ["--mount".to_owned(), mount];
            if mount_first {
                arguments.extend(share);
                arguments.extend(policy);
            } else {
                arguments.extend(policy);
                arguments.extend(share);
            }
            policies[usize::from(kind)].push(path);
        }
        // Two fixed shares exercise aggregate attribution even with an empty event sequence.
        // Restore avoids the unrelated cold-boot command-line budget.
        arguments.extend([
            "--mount".to_owned(), format!("/first,{},rw", roots[0].display()),
            "--mount".to_owned(), format!("/second,{},rw", roots[1].display()),
            "--restore-snapshot".to_owned(), "snapshot".to_owned(),
        ]);
        let (options, unrecognized) = parse(&arguments).unwrap();
        prop_assert!(unrecognized.is_empty());
        let rules = options.network.egress_rules().unwrap();
        let mut command = Vec::new();
        extend_shares_and_network(&mut command, &options, &rules).unwrap();
        for (kind, expected) in ["--mount-deny", "--mount-allow", "--mount-write"].into_iter().zip(policies) {
            let actual: Vec<_> = command.windows(2)
                .filter(|pair| pair[0] == kind).map(|pair| pair[1].clone()).collect();
            prop_assert_eq!(actual, expected);
        }
    }

    #[test]
    fn aggregate_policy_paths_must_be_absolute(relative in "[a-z]{1,20}") {
        for option in ["--mount-deny", "--mount-allow", "--mount-write"] {
            let arguments = [
                "--mount", "/first,first",
                "--mount", "/second,second",
                option, &relative,
            ].map(str::to_owned);
            let (options, _) = parse(&arguments).unwrap();
            let rules = options.network.egress_rules().unwrap();
            prop_assert!(cold_config(&options, &rules).is_err());
        }
    }
}
