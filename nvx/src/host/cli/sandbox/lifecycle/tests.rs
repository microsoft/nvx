use std::cell::{Cell, RefCell};

use super::stop::{StopOutcome, terminate_within};
use super::*;
use crate::host::cli::deadline::Deadline;
use crate::host::cli::snapshots::Name;
use crate::host::paths::resolve;
use crate::host::sandbox::distro::DistroBinding;
use crate::host::sandbox::layer::LayerRole;
use crate::host::sandbox::{DEFAULT_HOSTNAME, DEFAULT_WORKLOAD_IDENTITY};

/// An OpenVMM of another runtime identity whose termination a test scripts, without a process.
struct Scripted {
    /// Whether OpenVMM has exited.
    exited: Cell<bool>,
    /// Whether a termination ends OpenVMM within its bound, which one in uninterruptible I/O
    /// outlives.
    dies_when_terminated: bool,
    /// Whether the runtime record of its launch remains.
    recorded: Cell<bool>,
    /// The bounds of its terminations, in order.
    bounds: RefCell<Vec<Duration>>,
}

impl Scripted {
    /// Returns an OpenVMM that runs, which a termination ends if `dies_when_terminated`.
    fn running(dies_when_terminated: bool) -> Self {
        Self {
            exited: Cell::new(false),
            dies_when_terminated,
            recorded: Cell::new(true),
            bounds: RefCell::new(Vec::new()),
        }
    }

    /// Terminates OpenVMM within `bound`, as [`Vmm::terminate`] does: one that has exited is
    /// not signaled, and the runtime record goes once it has exited.
    fn terminate(&self, bound: Duration) -> Result<StopOutcome> {
        self.bounds.borrow_mut().push(bound);
        let outcome = if self.exited.get() {
            StopOutcome::Graceful
        } else if self.dies_when_terminated {
            self.exited.set(true);
            StopOutcome::Forced
        } else {
            StopOutcome::TimedOut
        };
        if self.exited.get() {
            self.recorded.set(false);
        }
        Ok(outcome)
    }

    /// Ends OpenVMM, as a stop with an hour's timeout does, and ends the stop's wait for its
    /// exit with `waited`: OpenVMM exits during the wait if that is `Ok(true)`.
    fn stop(&self, waited: Result<bool>) -> Result<StopOutcome> {
        terminate_within(
            Deadline::after(3600.0),
            |bound| self.terminate(bound),
            |_| {
                if matches!(waited, Ok(true)) {
                    self.exited.set(true);
                }
                waited
            },
        )
    }

    /// Returns the bounds of its terminations.
    fn bounds(&self) -> Vec<Duration> {
        self.bounds.borrow().clone()
    }
}

#[test]
fn ctrl_c_ends_the_wait_for_a_terminated_openvmm_that_has_not_exited() {
    // OpenVMM outlives its termination, so the termination waits one retry interval at most,
    // whatever the timeout, and the stop's own wait, which Ctrl-C ends, waits for the rest.
    let openvmm = Scripted::running(false);
    let stopped = openvmm.stop(Err(Exit::Interrupted));
    assert!(matches!(stopped, Err(Exit::Interrupted)), "{stopped:?}");
    let bounds = openvmm.bounds();
    assert_eq!(bounds.len(), 1, "{bounds:?}");
    assert!(bounds[0] <= RETRY_INTERVAL, "{bounds:?}");
    // Its exit was not seen, so its runtime record remains for the next stop.
    assert!(openvmm.recorded.get());
}

#[test]
fn a_terminated_openvmm_that_exits_during_the_wait_was_forced() {
    // Once it has exited, a second termination finds it exited, signals nothing, and clears
    // its runtime record.
    let openvmm = Scripted::running(false);
    let stopped = openvmm.stop(Ok(true));
    assert!(matches!(stopped, Ok(StopOutcome::Forced)), "{stopped:?}");
    assert_eq!(openvmm.bounds().len(), 2);
    assert!(!openvmm.recorded.get());

    // One that still runs at the deadline keeps its record.
    let survivor = Scripted::running(false);
    let stopped = survivor.stop(Ok(false));
    assert!(matches!(stopped, Ok(StopOutcome::TimedOut)), "{stopped:?}");
    assert_eq!(survivor.bounds().len(), 1);
    assert!(survivor.recorded.get());
}

#[test]
fn an_openvmm_that_its_termination_ends_or_that_exited_is_not_waited_for() {
    // The stop's wait, which would fail here, does not run.
    let openvmm = Scripted::running(true);
    let stopped = openvmm.stop(Err(Exit::Interrupted));
    assert!(matches!(stopped, Ok(StopOutcome::Forced)), "{stopped:?}");
    assert_eq!(openvmm.bounds().len(), 1);
    assert!(!openvmm.recorded.get());

    let exited = Scripted::running(true);
    exited.exited.set(true);
    let stopped = exited.stop(Err(Exit::Interrupted));
    assert!(matches!(stopped, Ok(StopOutcome::Graceful)), "{stopped:?}");
    assert_eq!(exited.bounds().len(), 1);
    assert!(!exited.recorded.get());
}

/// A provision refuses, before it creates the state directory, a distro layer that every start of
/// the sandbox would refuse, one whose extension makes OpenVMM open it with its VMDK backend
/// rather than raw: a cold start renders the root's block paths so, and so does the restore that
/// starts a sandbox of `snapshot restore`.
#[test]
fn provision_refuses_a_distro_layer_that_every_start_refuses() {
    let directory = tempfile::tempdir().unwrap();
    let root = resolve(directory.path()).unwrap();
    let image = root.join("distro.vmdk");
    std::fs::write(&image, "distro").unwrap();
    let layer = SandboxLayer::new(
        LayerRole::Distro,
        image.to_str().unwrap(),
        "11111111-1111-1111-1111-111111111111",
    );
    let distro = RecordedDistro::new(&layer, DistroBinding::verify(&layer).unwrap()).unwrap();
    let config = ProvisionConfig {
        hypervisor: "kvm",
        cpu_profile: None,
        memory_mib: 256,
        cmdline: "",
        ram_upper_percent: None,
        hostname: DEFAULT_HOSTNAME,
        workload_identity: DEFAULT_WORKLOAD_IDENTITY,
        export: None,
        network: None,
    };
    let restored = Source {
        snapshot: Name::try_from("source".to_owned()).unwrap(),
        store: root.join("store"),
        workload: vec!["/bin/true".to_owned()],
    };
    let state = root.join("state");
    for source in [None, Some(&restored)] {
        let error = provision(
            state.to_str().unwrap(),
            &layer,
            None,
            &distro,
            &config,
            source,
            5.0,
        )
        .unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::Unsupported), "{source:?}");
        let message = error.message().unwrap_or_default();
        assert!(
            message.contains("disk paths ending in .vmdk are unsupported"),
            "{source:?}: {message}"
        );
        assert!(!state.exists(), "{source:?}");
    }
}
