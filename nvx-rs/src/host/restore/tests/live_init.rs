use std::error::Error;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::Hypervisor;
use crate::activation::{
    ActivationRecord, Configuration, ExpectedResources, Export, ExportKind, Image, MachineIdMode,
    Network, OverlayMode, Publication, ReadyActivation, RestoreSection, TransactionId, Upper, View,
};
use crate::host::nvxs::HostConnection;
use crate::host::session;
use crate::host::snapshot::SnapshotManifest;
use crate::host::spawn::{Launched, VmmCommand, spawn_vm_with_mode};
use crate::protocol::v2::{self, Frame, Outcome};
use crate::protocol::{
    CAPABILITY_LEN, HostMap, Workload, WorkloadEnvironment, encode_exec_payload,
    encode_maps_payloads,
};

type AnyError = Box<dyn Error>;

struct OwnedVm(Option<Launched>);
impl Drop for OwnedVm {
    fn drop(&mut self) {
        if let Some(vm) = self.0.take() {
            if vm
                .process()
                .and_then(|process| process.has_exited())
                .is_ok_and(|done| done)
            {
                vm.detach();
            } else if vm.kill_and_confirm_exit().is_err() {
                eprintln!("OWNED_RESTORE_TEARDOWN_FAILED");
            }
        }
    }
}

fn require(ok: bool, message: impl Into<String>) -> Result<(), AnyError> {
    if ok {
        Ok(())
    } else {
        Err(message.into().into())
    }
}

fn backend() -> Hypervisor {
    if cfg!(windows) {
        Hypervisor::Whp
    } else {
        Hypervisor::Mshv
    }
}

fn private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(windows)]
    {
        fs::create_dir(path)
    }
}

fn create_export(root: &Path) -> io::Result<PathBuf> {
    let export = root.join("export");
    fs::create_dir(&export)?;
    let proof = export.join("proof");
    fs::write(&proof, b"CURRENT_573F_HOST_MAP\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&export, fs::Permissions::from_mode(0o755))?;
        fs::set_permissions(&proof, fs::Permissions::from_mode(0o644))?;
    }
    Ok(export)
}

fn ready(
    connection: &mut HostConnection,
    id: u64,
    expected: &[u8],
    by: Instant,
) -> Result<(), AnyError> {
    let bytes = connection.receive(Some(by))?;
    let frame = Frame::decode(&bytes)?;
    let ready = Frame::Ready {
        version: v2::VERSION,
        activation: expected,
    };
    require(frame == (id, ready), format!("unexpected Ready: {frame:?}"))
}

fn workload(script: &str, environment: WorkloadEnvironment<'_>) -> Result<Vec<u8>, AnyError> {
    Ok(encode_exec_payload(&Workload {
        argv: vec!["/bin/sh".into(), "-eu".into(), "-c".into(), script.into()],
        timeout_ms: 10_000,
        cwd: Some("/"),
        environment,
    })?)
}

fn execute(
    connection: &mut HostConnection,
    id: u64,
    script: &str,
    environment: WorkloadEnvironment<'_>,
    by: Instant,
) -> Result<String, AnyError> {
    connection.send(
        &Frame::Exec(&workload(script, environment)?).encode(id)?,
        Some(by),
    )?;
    // The workload reads no standard input.
    connection.send(&Frame::StdinEnd.encode(id)?, Some(by))?;
    let mut output = Vec::new();
    loop {
        let bytes = connection.receive(Some(by))?;
        let (request_id, frame) = Frame::decode(&bytes)?;
        require(
            request_id == id,
            format!("wrong execution response: {frame:?}"),
        )?;
        match frame {
            Frame::Stdout(chunk) => output.extend_from_slice(chunk),
            Frame::Stderr(chunk) => eprint!("{}", String::from_utf8_lossy(chunk)),
            Frame::Exit { status, outcome } => {
                require(
                    status == 0 && outcome == Outcome::Exit,
                    format!("execution failed: {frame:?}"),
                )?;
                return Ok(String::from_utf8(output)?);
            }
            _ => return Err(format!("unexpected execution frame: {frame:?}").into()),
        }
    }
}

fn check_restored_console(logs: &str, case: &str) -> Result<usize, AnyError> {
    let cold_switches = logs.matches("NVX-INIT-ROOT: switched").count();
    require(
        cold_switches == 0,
        "restored VM unexpectedly entered the cold-boot root path",
    )?;
    if matches!(case, "published" | "unpublished") {
        require(
            !logs.contains("NVX-INIT-ACTIVATION:"),
            "successful restored activation emitted a failure diagnostic",
        )?;
    }
    Ok(cold_switches)
}

fn run() -> Result<(), AnyError> {
    let grant = std::env::var("NVX_SL77B_LIVE_GRANT")?;
    require(
        !grant.is_empty(),
        "live fixture requires explicit named host admission",
    )?;
    let mut args = vec![OsString::from("nvx-live-restored-init")];
    for key in [
        "NVX_SL77B_LIVE_NATIVE",
        "NVX_SL77B_LIVE_SNAPSHOT",
        "NVX_SL77B_LIVE_DISTRO",
        "NVX_SL77B_LIVE_CONSOLE",
        "NVX_SL77B_LIVE_ROOT",
        "NVX_SL77B_LIVE_RESULT",
        "NVX_SL77B_LIVE_CASE",
    ] {
        args.push(
            std::env::var_os(key).ok_or_else(|| format!("missing live fixture input {key}"))?,
        );
    }
    require(
        args.len() == 8,
        "usage: restore VMM SNAPSHOT DISTRO ORIGINAL_CONSOLE ROOT RESULT CASE",
    )?;
    let vmm = PathBuf::from(&args[1]);
    let snapshot = PathBuf::from(&args[2]);
    let distro = PathBuf::from(&args[3]);
    let original_console = PathBuf::from(&args[4]);
    let root = PathBuf::from(&args[5]);
    let result = PathBuf::from(&args[6]);
    let case = args[7].to_str().ok_or("non-Unicode case")?;
    require(
        matches!(case, "published" | "unpublished" | "changed" | "deadline"),
        "invalid fixture case",
    )?;
    for path in [&vmm, &snapshot, &distro, &original_console, &root, &result] {
        require(path.is_absolute(), "all fixture paths must be absolute")?;
    }
    let manifest = SnapshotManifest::read(&snapshot)?;
    require(
        manifest.hypervisor() == backend() && manifest.memory_len() == 128 << 20,
        "wrong backend/memory snapshot",
    )?;
    require(
        !root.exists() && !result.exists(),
        "preserve existing fixture effects",
    )?;
    private_directory(&root)?;
    let export = create_export(&root)?;
    let endpoint = if cfg!(windows) {
        format!(
            "//./pipe/openvmm-microvm-sl77b-live-{}-{case}",
            std::process::id()
        )
    } else {
        snapshot
            .parent()
            .ok_or("snapshot has no private namespace parent")?
            .join(format!("nvxs-{case}"))
            .to_str()
            .ok_or("non-Unicode endpoint")?
            .to_owned()
    };
    let address: SocketAddr = fs::read_to_string(&original_console)?.trim().parse()?;
    require(address.ip().is_loopback(), "console must remain loopback")?;
    let listener = TcpListener::bind(address)?;
    listener.set_nonblocking(true)?;
    let console = root.join("restored-console.log");
    let console_output = console.clone();
    let reader = thread::spawn(move || -> io::Result<()> {
        let by = Instant::now() + Duration::from_secs(30);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < by => {
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error),
            }
        };
        stream.set_nonblocking(false)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(console_output)?;
        io::copy(&mut stream, &mut output)?;
        Ok(())
    });
    let report = root.join("outcome.json");
    let (_temporary, directory) = super::vm_directory();
    let mut listener = super::ReadyListener::bind(&directory)?;
    let acknowledgement = listener.endpoint().to_owned();
    let log_file = root.join("openvmm.log");
    let log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&log_file)?;
    let options: Vec<OsString> = vec![
        "--machine".into(),
        "microvm".into(),
        "--microvm-sandbox-block".into(),
        format!("distro:file:{},ro", distro.display()).into(),
        "--restore-absent-scratch".into(),
        "--single-process".into(),
        "--hypervisor".into(),
        if cfg!(windows) { "whp" } else { "mshv" }.into(),
        "--cpu-profile".into(),
        "auto".into(),
        "--processors".into(),
        "1".into(),
        "--restore-snapshot".into(),
        snapshot.as_os_str().to_owned(),
        "--restore-memory".into(),
        "128M".into(),
        "--restore-ready-path".into(),
        acknowledgement.into(),
        "--restore-network-policy".into(),
        "--network-profile".into(),
        "portable".into(),
        "--network-egress".into(),
        "deny".into(),
        "--network-ingress".into(),
        "deny".into(),
        "--host-loopback".into(),
        "deny".into(),
        "--mount-aggregate".into(),
        "/run/nvx/hostfs/root".into(),
        "--mount-child".into(),
        format!("0,{},ro", export.display()).into(),
        "--virtio-console".into(),
        format!("connect=tcp:{address}").into(),
        "--microvm-control-console".into(),
        format!("listen={endpoint}").into(),
        "--microvm-control-auth-stdin".into(),
        "--microvm-report".into(),
        report.as_os_str().to_owned(),
    ];
    let mut capability = [0; CAPABILITY_LEN];
    getrandom::fill(&mut capability)?;
    let command = VmmCommand {
        program: vmm,
        arguments: options,
        working_dir: root.clone(),
        breakaway_from_job: false,
        inherit: Vec::new(),
    };
    let mut owned = OwnedVm(Some(spawn_vm_with_mode(&command, &capability, log, false)?));
    let running = owned.0.as_ref().ok_or("missing owned VM")?;
    let process = running.process()?;
    let pid = running.id();
    fs::write(
        root.join("process.txt"),
        format!(
            "pid={pid}\nreference={:?}\ncase={case}\n",
            process.reference()
        ),
    )?;
    println!(
        "OWNED_RESTORE_PID={pid} REFERENCE={:?} CASE={case}",
        process.reference()
    );
    let by = Instant::now() + Duration::from_secs(90);
    let expected_event = b"OPENVMM_RESTORE_READY_V1\n";
    let mut event = Vec::new();
    while event.len() < expected_event.len() {
        require(Instant::now() < by, "native readiness deadline expired")?;
        let open = listener.receive(&process, &mut event, 64, Duration::from_millis(50))?;
        require(
            open || event.len() == expected_event.len(),
            "native readiness closed before its event",
        )?;
    }
    require(
        event == expected_event,
        "native restore-ready event differs",
    )?;
    super::parse_ready_event(&event)?;
    fs::write(root.join("native-ready.receipt"), &event)?;
    println!("NATIVE_READY_AUTHENTICATED_PID={pid}");
    let native_ready = Instant::now();
    if case == "deadline" {
        thread::sleep(Duration::from_secs(8));
    }
    let transport = session::connect(&endpoint, pid, process.reference().start_time, by)?;
    let mut connection = HostConnection::attach(transport, &capability, by)?;
    println!(
        "RESTORED_HOST_SESSION_EPOCH={}",
        connection.identity().epoch
    );
    let attached = Instant::now();
    let expected_status;
    if case == "deadline" {
        require(
            process
                .wait(Instant::now() + Duration::from_secs(40))?
                .is_some(),
            "activation deadline did not end VM",
        )?;
        require(
            native_ready.elapsed() >= Duration::from_secs(27)
                && native_ready.elapsed() < Duration::from_secs(38)
                && attached.elapsed() < Duration::from_secs(27),
            "activation deadline renewed at the deliberately delayed host attachment",
        )?;
        println!(
            "ORIGINAL_ACK_DEADLINE_ELAPSED_MS={}",
            native_ready.elapsed().as_millis()
        );
        expected_status = i64::from(crate::protocol::status::ACTIVATION_DEADLINE);
    } else {
        let published = case != "unpublished";
        let transaction = TransactionId::from_bytes([0x77; 16]);
        let record = ActivationRecord::new(
            Some(RestoreSection { transaction }),
            Configuration {
                image: Image::Erofs { uuid: None },
                upper: Upper::Tmpfs {
                    max_memory_percent: 50,
                },
                overlay: OverlayMode::UntrustedLowers,
                export: Some(Export {
                    kind: ExportKind::Aggregate,
                }),
                mappings: 1,
                network: Some(Network {
                    dns: Some("10.88.0.53".parse()?),
                    publication: if published {
                        Publication::Published
                    } else {
                        Publication::Unpublished
                    },
                    proxy: published.then(|| "http://10.88.0.1:3128".to_owned()),
                }),
                view: View {
                    hostname: b"restored-573f".to_vec(),
                    machine_id: MachineIdMode::Generated,
                },
                resources: ExpectedResources {
                    cpus: 1,
                    memory: 128 << 20,
                },
            },
        )?;
        let mut activation = Vec::new();
        activation.extend_from_slice(&1000u32.to_le_bytes());
        activation.extend_from_slice(&1001u32.to_le_bytes());
        activation.push(1);
        activation.extend(record.encode());
        fs::write(root.join("activate.bin"), &activation)?;
        let expected_ready = ReadyActivation {
            transaction: Some(transaction),
        }
        .encode();
        connection.send(&Frame::Activate(&activation).encode(100)?, Some(by))?;
        ready(&mut connection, 100, &expected_ready, by)?;
        println!("ACTIVATION_READY_PUBLICATION={published} OWN_DNS=10.88.0.53");
        if case == "changed" {
            activation[0] ^= 1;
            connection.send(&Frame::Activate(&activation).encode(101)?, Some(by))?;
            require(
                process
                    .wait(Instant::now() + Duration::from_secs(15))?
                    .is_some(),
                "changed identity replay did not end VM",
            )?;
            expected_status = i64::from(crate::protocol::status::ACTIVATION_PROTOCOL);
            println!("CHANGED_IDENTITY_REPLAY_FATAL");
        } else {
            connection.send(
                &Frame::Exec(&workload("true", WorkloadEnvironment::Default)?).encode(102)?,
                Some(by),
            )?;
            let bytes = connection.receive(Some(by))?;
            let frame = Frame::decode(&bytes)?;
            let refusal = Frame::Error {
                status: 11,
                category: b"mappings-incomplete",
            };
            require(
                frame == (102, refusal),
                format!("EXEC bypassed MAPS gate: {frame:?}"),
            )?;
            let maps = [HostMap {
                source: "0".into(),
                target: "/mnt/restore-map".into(),
                read_only: true,
            }];
            for payload in encode_maps_payloads(&maps, 32768)? {
                connection.send(&Frame::Maps(&payload).encode(103)?, Some(by))?;
                ready(&mut connection, 103, &[], by)?;
            }
            let script = format!(
                r#"
test "$(id -u)" = 1000
test "$(id -g)" = 1001
test "$(hostname)" = restored-573f
grep -q 'nameserver 10.88.0.53' /etc/resolv.conf
test "${{http_proxy-}}" = '{1}'
test "${{https_proxy-}}" = '{1}'
test "${{HTTP_PROXY-}}" = '{1}'
test "${{HTTPS_PROXY-}}" = '{1}'
if test '{1}' = ''; then
    test "${{http_proxy+x}}${{https_proxy+x}}${{HTTP_PROXY+x}}${{HTTPS_PROXY+x}}" = ''
fi
printf 'RESTORED_PROXY_DEFAULTS_OK\n'
test "$(cat /mnt/restore-map/proof)" = CURRENT_573F_HOST_MAP
test ! -e /sbin/nvx-ingress
count=0
for p in /proc/[0-9]*; do
    test "$(cat "$p/comm" 2>/dev/null || true)" = nvx-ingress || continue
    count=$((count + 1))
    grep -q '^PPid:[[:space:]]*1$' "$p/status"
    test "$(cat "$p/oom_score_adj")" = -1000
    test "$(tr '\000' ' ' <"$p/cmdline")" = 'nvx-ingress 10.88.0.1 '
    test "$(awk '{{print $19}}' "$p/stat")" = -10
    printf 'REAL_RESTORED_ANNOUNCER pid=%s argv=' "${{p##*/}}"
    tr '\000' ' ' <"$p/cmdline"
    printf '\n'
done
test "$count" = {}
printf 'PRESERVED_573F_UPPER\n' >/tmp/restore-sentinel
printf 'MACHINE_ID='
cat /etc/machine-id
printf '\n'
printf 'REAL_READY_MAPS_EXEC_OK\n'
"#,
                u8::from(published),
                if published {
                    "http://10.88.0.1:3128"
                } else {
                    ""
                },
            );
            let first = execute(
                &mut connection,
                104,
                &script,
                WorkloadEnvironment::Default,
                by,
            )?;
            print!("{first}");
            let proxy_script = format!(
                "test \"$HTTP_PROXY\" = http://exec.example:2; \
                 test \"${{http_proxy-}}\" = '{}'; printf 'EXEC_PROXY_OVERRIDE_OK\\n'",
                if published {
                    "http://10.88.0.1:3128"
                } else {
                    ""
                },
            );
            let proxy = execute(
                &mut connection,
                105,
                &proxy_script,
                WorkloadEnvironment::Layered(&["HTTP_PROXY=http://exec.example:2".to_owned()]),
                by,
            )?;
            print!("{proxy}");
            let machine = first
                .lines()
                .find(|line| line.starts_with("MACHINE_ID="))
                .ok_or("missing generated machine ID")?
                .to_owned();
            for kill in [0, 1] {
                connection.send(
                    &Frame::Capture(&[kill]).encode(106 + u64::from(kill))?,
                    Some(by),
                )?;
                let bytes = connection.receive(Some(by))?;
                let (_, frame) = Frame::decode(&bytes)?;
                require(
                    matches!(frame, Frame::Error { status: 95, .. }),
                    "activated restore accepted capture",
                )?;
            }
            drop(connection);
            let transport = session::connect(&endpoint, pid, process.reference().start_time, by)?;
            connection = HostConnection::attach(transport, &capability, by)?;
            connection.send(&Frame::Activate(&activation).encode(108)?, Some(by))?;
            ready(&mut connection, 108, &expected_ready, by)?;
            let replay = execute(
                &mut connection,
                109,
                r#"
test "$(cat /tmp/restore-sentinel)" = PRESERVED_573F_UPPER
test "$(cat /mnt/restore-map/proof)" = CURRENT_573F_HOST_MAP
printf 'MACHINE_ID='
cat /etc/machine-id
printf '\n'
printf 'RECONNECT_REPLAY_PRESERVED_UPPER_AND_MAPS\n'
"#,
                WorkloadEnvironment::Default,
                by,
            )?;
            require(
                replay.lines().any(|line| line == machine),
                "retry regenerated the machine ID",
            )?;
            print!("{replay}");
            connection.send(&Frame::Stop.encode(110)?, Some(by))?;
            let bytes = connection.receive(Some(by))?;
            let frame = Frame::decode(&bytes)?;
            require(frame == (110, Frame::Stopped), "STOP not acknowledged")?;
            require(
                process
                    .wait(Instant::now() + Duration::from_secs(30))?
                    .is_some(),
                "STOP did not tear down restore",
            )?;
            expected_status = 0;
        }
    }
    drop(connection);
    reader.join().map_err(|_| "console reader panicked")??;
    let outcome = crate::host::report::read(&report)?.ok_or("missing native outcome")?;
    require(
        outcome.status_code == Some(expected_status) && outcome.teardown_complete == Some(true),
        format!("wrong restore outcome: {outcome:?}"),
    )?;
    let logs = fs::read_to_string(&console)?;
    let cold_switches = check_restored_console(&logs, case)?;
    let text = format!(
        "RESTORE_OK case={case} pid={pid} status={expected_status} cold_switches={cold_switches} teardown=true\n"
    );
    fs::write(result, &text)?;
    print!("{text}");
    owned.0.take().ok_or("missing owned restore")?.detach();
    Ok(())
}

#[cfg(unix)]
#[test]
fn restored_export_is_readable_under_restrictive_umask() {
    use std::os::unix::fs::PermissionsExt;

    const CHILD: &str = "NVX_RESTORED_EXPORT_UMASK_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host::restore::tests::live_init::restored_export_is_readable_under_restrictive_umask",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("vm");
    private_directory(&root).unwrap();
    let export = create_export(&root).unwrap();
    for (path, mode) in [
        (root, 0o700),
        (export.clone(), 0o755),
        (export.join("proof"), 0o644),
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o7777,
            mode
        );
    }
}

#[test]
fn restored_console_accepts_success_without_a_cold_switch() {
    for case in ["published", "unpublished"] {
        assert_eq!(check_restored_console("", case).unwrap(), 0);
    }
}

#[test]
fn restored_console_rejects_a_cold_switch_for_every_case() {
    for case in ["published", "unpublished", "changed", "deadline"] {
        let error =
            check_restored_console("NVX-INIT-ROOT: switched mappings=1\n", case).unwrap_err();
        assert_eq!(
            error.to_string(),
            "restored VM unexpectedly entered the cold-boot root path"
        );
    }
}

#[test]
fn restored_console_rejects_activation_failure_on_success() {
    for case in ["published", "unpublished"] {
        let error = check_restored_console("NVX-INIT-ACTIVATION: failed\n", case).unwrap_err();
        assert_eq!(
            error.to_string(),
            "successful restored activation emitted a failure diagnostic"
        );
    }
}

#[test]
fn restored_console_allows_expected_activation_failure() {
    for case in ["changed", "deadline"] {
        assert_eq!(
            check_restored_console("NVX-INIT-ACTIVATION: failed\n", case).unwrap(),
            0
        );
    }
}

#[test]
#[ignore = "requires explicit named host admission and current-source live restore fixture inputs"]
fn live_restored_init_activation() -> Result<(), AnyError> {
    run()
}
