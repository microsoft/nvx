//! `nvx egress compile PATH`, which prints, as a JSON object, the native OpenVMM rules into
//! which the library lowers an egress policy file, each list in the order in which OpenVMM receives
//! them, or fails with the library's refusal of the file.

use serde_json::{Value, json};

use crate::harness::Root;

/// The allow and deny rules, in OpenVMM's form, into which an egress policy file compiles.
type Rules = (&'static [&'static str], &'static [&'static str]);

/// The code of the error with which the library refuses an egress policy file, and what the error
/// names.
type Refusal = (&'static str, &'static [&'static str]);

/// An egress policy file, and what `egress compile` prints for it.
struct Compilation {
    /// The name of the case.
    name: &'static str,
    /// The content of `policy.json`, which the case writes below the root, if any.
    policy: Option<&'static str>,
    /// The path that `egress compile` compiles, with placeholders.
    path: &'static str,
    /// The rules into which the file compiles, or the library's refusal of it.
    expected: Result<Rules, Refusal>,
}

/// The policy files that `egress compile` compiles, or refuses.
const COMPILATIONS: [Compilation; 9] = [
    Compilation {
        name: "empty",
        policy: Some("{}"),
        path: "{root/policy.json}",
        expected: Ok((&[], &[])),
    },
    Compilation {
        name: "allow",
        policy: Some(r#"{"allow": [{"cidr": "192.0.2.0/24"}]}"#),
        path: "{root/policy.json}",
        expected: Ok((&["192.0.2.0/24"], &[])),
    },
    // An exclusion carves a range out of its rule, and a rule may give its destinations and
    // ports in lists.
    Compilation {
        name: "both",
        policy: Some(
            r#"{"allow": [{"cidr": "192.0.2.0/24", "except": ["192.0.2.128/25"]}, {"to": [{"cidr": "2001:db8::/32"}], "ports": [{"protocol": "tcp", "port": 80, "endPort": 81}]}], "deny": [{"cidr": "198.51.100.0/24", "protocol": "icmp"}]}"#,
        ),
        path: "{root/policy.json}",
        expected: Ok((
            &["192.0.2.0/25", "2001:db8::/32:tcp:80-81"],
            &["198.51.100.0/24:icmp"],
        )),
    },
    Compilation {
        name: "invalid JSON",
        policy: Some("{"),
        path: "{root/policy.json}",
        expected: Err(("unsupported", &["egress policy file", "{root/policy.json}"])),
    },
    Compilation {
        name: "invalid rule",
        policy: Some(r#"{"allow": [{"cidr": "192.0.2.0/24", "port": 70000}]}"#),
        path: "{root/policy.json}",
        expected: Err(("unsupported", &["allow[0].port"])),
    },
    Compilation {
        name: "unknown field",
        policy: Some(r#"{"allow": [], "other": []}"#),
        path: "{root/policy.json}",
        expected: Err(("unsupported", &["unknown field", "\"other\""])),
    },
    Compilation {
        name: "missing file",
        policy: None,
        path: "{root/missing.json}",
        expected: Err(("io", &["egress policy file", "{root/missing.json}"])),
    },
    Compilation {
        name: "directory",
        policy: None,
        path: "{root/share}",
        expected: Err(("io", &["egress policy file", "{root/share}"])),
    },
    // A scoped IPv6 range is refused, also one whose address has host bits that normalization
    // would drop with the scope (microsoft/nvx#462).
    Compilation {
        name: "scoped IPv6 range",
        policy: Some(r#"{"allow": [{"cidr": "fe80::1%eth0/64"}]}"#),
        path: "{root/policy.json}",
        expected: Err(("unsupported", &["allow[0].cidr", "scope"])),
    },
];

#[test]
fn egress_compile_prints_the_library_lowering() {
    let root = Root::new();
    for case in &COMPILATIONS {
        if let Some(policy) = case.policy {
            root.write("policy.json", policy);
        }
        let outcome = root.nvx(&["egress", "compile", case.path]);
        eprintln!("case: {}", case.name);
        match case.expected {
            Ok((allow, deny)) => {
                outcome.assert_success();
                let printed: Value = serde_json::from_slice(&outcome.stdout).unwrap();
                assert_eq!(
                    printed,
                    json!({"allow": allow, "deny": deny}),
                    "{}",
                    case.name
                );
            }
            Err((code, fragments)) => {
                let fragments: Vec<String> = fragments
                    .iter()
                    .map(|fragment| root.expand(fragment))
                    .collect();
                let fragments: Vec<&str> = fragments.iter().map(String::as_str).collect();
                outcome.assert_error(code, &fragments);
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn egress_compile_reads_a_fifo_once_its_writer_comes() {
    // A policy file may be a FIFO, such as `/dev/stdin` fed by a pipe, whose writer may open it
    // only after the compilation does: the compilation waits for the writer, and reads what it
    // writes up to its end.
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::{Duration, Instant};

    let root = Root::new();
    let fifo = root.below("policy.json");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
    let writer = std::thread::spawn(move || {
        // Late, so that the compilation waits for it.
        std::thread::sleep(Duration::from_millis(500));
        // An open that does not wait fails until a reader has the FIFO open.
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut file = loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
                .open(&fifo)
            {
                Ok(file) => break file,
                Err(error) if Instant::now() < deadline => {
                    assert_eq!(error.raw_os_error(), Some(nix::libc::ENXIO), "{error}");
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("no reader opened the FIFO: {error}"),
            }
        };
        std::io::Write::write_all(&mut file, br#"{"allow": [{"cidr": "192.0.2.0/24"}]}"#).unwrap();
    });
    let outcome = root.nvx(&["egress", "compile", "{root/policy.json}"]);
    writer.join().unwrap();
    outcome.assert_success();
    let printed: Value = serde_json::from_slice(&outcome.stdout).unwrap();
    assert_eq!(printed, json!({"allow": ["192.0.2.0/24"], "deny": []}));
}

#[cfg(target_os = "linux")]
#[test]
#[allow(unsafe_code)]
fn egress_compile_reads_a_leased_policy_file_once_its_lease_ends() {
    // A policy file may be a regular file that another process holds a lease of, as Samba does of
    // the files that it shares: opening it breaks the lease, and waits until the holder gives it
    // up, and the compilation then reads the file.
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    use nix::libc::{EAGAIN, F_GETLEASE, F_SETLEASE, F_SETOWN, F_WRLCK, fcntl};

    let root = Root::new();
    let path = root.below("policy.json");
    std::fs::write(&path, br#"{"allow": [{"cidr": "192.0.2.0/24"}]}"#).unwrap();
    let holder = std::fs::File::open(&path).unwrap();
    let descriptor = holder.as_raw_fd();
    let deadline = Instant::now() + Duration::from_secs(10);
    let leased = loop {
        // SAFETY: `holder` keeps the descriptor open, and these commands take integers.
        if unsafe { fcntl(descriptor, F_SETLEASE, F_WRLCK) } == 0 {
            break true;
        }
        // A process that another test started holds copies of this one's files until it runs its
        // program, such as the file that wrote the policy, which no write lease admits.
        if std::io::Error::last_os_error().raw_os_error() != Some(EAGAIN)
            || Instant::now() >= deadline
        {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // SAFETY: as above. Taking the lease made this process the owner of the file, which the break
    // of the lease would signal with SIGIO, ending it; without an owner, the break signals none.
    if !leased || unsafe { fcntl(descriptor, F_SETOWN, 0) } != 0 {
        // The file system grants no lease, or the file stays open elsewhere.
        return;
    }
    let releaser = std::thread::spawn(move || {
        // Once the compilation's open breaks the lease, F_GETLEASE reports the read lease to which
        // the holder must give it up, no longer the write lease.
        let deadline = Instant::now() + Duration::from_secs(30);
        // SAFETY: as above.
        while unsafe { fcntl(holder.as_raw_fd(), F_GETLEASE) } == F_WRLCK {
            assert!(
                Instant::now() < deadline,
                "the compilation never opened the policy file"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // Late, so that the compilation waits for the lease to end, as closing the file ends it.
        std::thread::sleep(Duration::from_millis(500));
        drop(holder);
    });
    let outcome = root.nvx(&["egress", "compile", "{root/policy.json}"]);
    releaser.join().unwrap();
    outcome.assert_success();
    let printed: Value = serde_json::from_slice(&outcome.stdout).unwrap();
    assert_eq!(printed, json!({"allow": ["192.0.2.0/24"], "deny": []}));
}
