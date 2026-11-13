//! The restore-only announcer step and its existing helper supervision.

use std::cell::RefCell;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::*;

thread_local! {
    static PROGRAM: RefCell<Option<CString>> = const { RefCell::new(None) };
}

pub(crate) fn announcer_program(program: CString) -> CString {
    PROGRAM.with_borrow(|replacement| replacement.clone().unwrap_or(program))
}

fn with_program<T>(path: &Path, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<CString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            PROGRAM.replace(self.0.take());
        }
    }
    let _restore = Restore(PROGRAM.replace(Some(sys::path_c_string(path).unwrap())));
    run()
}

fn runtime() -> Runtime {
    Runtime {
        system: RealSystem { signals: None },
        helpers: Helpers {
            time: pid(i32::MAX),
            announcer: None,
        },
        terminating: false,
    }
}

struct Child {
    process: ProcessId,
    descriptor: FileDescriptor,
    collected: bool,
}

impl Child {
    fn new(process: ProcessId) -> Self {
        Self {
            process,
            descriptor: sys::open_pidfd(process).unwrap(),
            collected: false,
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if !self.collected {
            let _ = sys::kill_through_pidfd(self.descriptor.borrowed(), false);
            let _ = sys::wait(self.process);
        }
    }
}

fn script(directory: &Path, body: &str) -> std::path::PathBuf {
    let path = directory.join("announcer");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "the announcer did not advance");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn collect_failure(runtime: &mut Runtime, child: &mut Child) -> String {
    assert!(sys::readable_or_hung_up(&[child.descriptor.borrowed()], 5000).unwrap()[0]);
    let error = runtime
        .service(|_, _| panic!("the restored announcer was treated as a workload"))
        .unwrap_err();
    child.collected = true;
    error
}

fn blocked(process: ProcessId) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{}/status", process.raw())).unwrap();
    let mask = status
        .lines()
        .find_map(|line| line.strip_prefix("SigBlk:"))
        .unwrap();
    u64::from_str_radix(mask.trim(), 16).unwrap()
}

#[test]
fn unpublished_restores_start_nothing_even_without_a_gateway() {
    let mut runtime = runtime();
    for gateway in [None, Some(Ipv4Addr::new(10, 88, 0, 1))] {
        runtime
            .restore_announcer(Publication::Unpublished, gateway)
            .unwrap();
        assert_eq!(runtime.helpers.announcer, None);
    }
}

#[test]
fn a_missing_gateway_or_an_existing_announcer_starts_no_replacement() {
    let mut runtime = runtime();
    assert_eq!(
        runtime
            .restore_announcer(Publication::Published, None)
            .unwrap_err()
            .kind(),
        crate::ErrorKind::Unsupported
    );
    assert_eq!(runtime.helpers.announcer, None);
    runtime.helpers.announcer = Some(pid(7));
    assert_eq!(
        runtime
            .restore_announcer(Publication::Published, Some(Ipv4Addr::new(10, 88, 0, 1)))
            .unwrap_err()
            .kind(),
        crate::ErrorKind::Unsupported
    );
    assert_eq!(runtime.helpers.announcer, Some(pid(7)));
}

#[test]
fn the_restored_child_gets_only_the_gateway_and_joins_fatal_helper_supervision() {
    let name = "guest::init::tests::restore::the_restored_child_gets_only_the_gateway_and_joins_fatal_helper_supervision";
    if isolate_process_test("NVX_INIT_RESTORED_CHILD_TEST", name) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    in_child(|| {
        let record = directory.path().join("arguments");
        let program = script(
            directory.path(),
            &format!(
                "printf '%s\\n' \"$#\" \"$1\" \"$PPID\" > '{}'\nexec sleep 30",
                record.display()
            ),
        );
        let mut runtime = runtime();
        runtime.system.listen().unwrap();
        with_program(&program, || {
            runtime
                .restore_announcer(Publication::Published, Some(Ipv4Addr::new(10, 88, 0, 1)))
                .unwrap();
        });
        let mut child = Child::new(runtime.helpers.announcer.unwrap());
        let expected = format!("1\n10.88.0.1\n{}\n", sys::current_process().raw());
        until(|| std::fs::read_to_string(&record).is_ok_and(|record| record == expected));
        assert_eq!(nice_of(child.process), nice_of(sys::current_process()));
        assert_eq!(
            std::fs::read(format!("/proc/{}/oom_score_adj", child.process.raw())).unwrap(),
            std::fs::read("/proc/self/oom_score_adj").unwrap()
        );
        sys::kill_through_pidfd(child.descriptor.borrowed(), false).unwrap();
        let error = collect_failure(&mut runtime, &mut child);
        assert_eq!(
            error,
            format!(
                "the announcer, process {}, was killed by signal 9",
                child.process.raw()
            )
        );
        let mut system = Fake::default();
        finish(&mut system, Err(error.clone()));
        assert_eq!(system.calls, [failure(&error), Call::ShutDown(FAILURE)]);
    });
}

#[test]
fn a_failed_exec_remains_a_registered_fatal_helper() {
    let name = "guest::init::tests::restore::a_failed_exec_remains_a_registered_fatal_helper";
    if isolate_process_test("NVX_INIT_RESTORED_EXEC_FAILURE_TEST", name) {
        return;
    }
    in_child(|| {
        let mut runtime = runtime();
        runtime.system.listen().unwrap();
        with_program(Path::new("/nonexistent/nvx-ingress"), || {
            runtime
                .restore_announcer(Publication::Published, Some(Ipv4Addr::new(10, 88, 0, 1)))
                .unwrap();
        });
        let mut child = Child::new(runtime.helpers.announcer.unwrap());
        assert_eq!(
            collect_failure(&mut runtime, &mut child),
            format!(
                "the announcer, process {}, exited with status 125",
                child.process.raw()
            )
        );
    });
}

#[test]
fn the_restored_announcer_unblocks_pid_1_signals_before_exec() {
    let name =
        "guest::init::tests::restore::the_restored_announcer_unblocks_pid_1_signals_before_exec";
    if isolate_process_test("NVX_INIT_RESTORED_SIGNALS_TEST", name) {
        return;
    }
    in_child(|| {
        let mut runtime = runtime();
        runtime.system.listen().unwrap();
        assert_ne!(blocked(sys::current_process()), 0);
        with_program(Path::new("/bin/false"), || {
            runtime
                .restore_announcer(Publication::Published, Some(Ipv4Addr::new(10, 88, 0, 1)))
                .unwrap();
        });
        let mut child = Child::new(runtime.helpers.announcer.unwrap());
        assert!(sys::readable_or_hung_up(&[child.descriptor.borrowed()], 5000).unwrap()[0]);
        assert_eq!(blocked(child.process), 0);
        assert_eq!(
            collect_failure(&mut runtime, &mut child),
            format!(
                "the announcer, process {}, exited with status 1",
                child.process.raw()
            )
        );
    });
}

#[test]
fn a_refused_fork_keeps_the_helper_registry_unchanged() {
    let mut runtime = runtime();
    let error = runtime
        .restore_announcer(Publication::Published, Some(Ipv4Addr::new(10, 88, 0, 1)))
        .unwrap_err();
    assert_eq!(error.kind(), crate::ErrorKind::Io);
    assert_eq!(runtime.helpers.announcer, None);
}

#[test]
#[ignore = "requires a guest's root, CAP_SYS_NICE, CAP_SYS_RESOURCE, and CAP_SYS_CHROOT"]
fn the_restored_helper_keeps_the_pre_switch_root_and_pid_1_priority() {
    let name = "guest::init::tests::restore::the_restored_helper_keeps_the_pre_switch_root_and_pid_1_priority";
    if std::env::var_os("NVX_INIT_RESTORED_PRIORITY_TEST").is_none() {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--ignored", "--nocapture"])
            .env("NVX_INIT_RESTORED_PRIORITY_TEST", "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    assert!(holds_capability(CAP_SYS_NICE).unwrap());
    assert!(holds_capability(CAP_SYS_RESOURCE).unwrap());
    let directory = tempfile::tempdir().unwrap();
    in_child(|| {
        let root = directory.path().join("new-root");
        std::fs::create_dir(&root).unwrap();
        let marker = directory.path().join("old-root-marker");
        std::fs::write(&marker, b"the helper kept the initramfs root\n").unwrap();
        let ready = directory.path().join("ready");
        let gate = directory.path().join("gate");
        let output = directory.path().join("observed");
        let mut gate_file = std::fs::File::create(&gate).unwrap();
        let mut output_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&output)
            .unwrap();
        let program = script(
            directory.path(),
            &format!(
                "printf ready > '{}'\nwhile test ! -s '{}'; do sleep 0.01; done\ncat '{}' > '{}'\nexec sleep 30",
                ready.display(),
                gate.display(),
                marker.display(),
                output.display()
            ),
        );
        let mut runtime = runtime();
        runtime.system.listen().unwrap();
        runtime.system.raise_priority(None).unwrap();
        with_program(&program, || {
            runtime
                .restore_announcer(Publication::Published, Some(Ipv4Addr::new(10, 88, 0, 1)))
                .unwrap();
        });
        let mut child = Child::new(runtime.helpers.announcer.unwrap());
        until(|| ready.is_file());
        assert_eq!(nice_of(child.process), -10);
        assert_eq!(
            std::fs::read_to_string(format!("/proc/{}/oom_score_adj", child.process.raw()))
                .unwrap()
                .trim(),
            "-1000"
        );
        nix::unistd::chroot(&root).unwrap();
        std::env::set_current_dir("/").unwrap();
        gate_file.write_all(b"continue\n").unwrap();
        let mut observed = String::new();
        until(|| {
            output_file.seek(SeekFrom::Start(0)).unwrap();
            observed.clear();
            output_file.read_to_string(&mut observed).unwrap();
            observed == "the helper kept the initramfs root\n"
        });
        sys::kill_through_pidfd(child.descriptor.borrowed(), false).unwrap();
        assert!(collect_failure(&mut runtime, &mut child).contains("the announcer"));
    });
}
