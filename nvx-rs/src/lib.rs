//! NVX mechanics: the OpenVMM control protocol, and the launch, policy lowering, and supervision
//! of NVX microVMs.
//!
//! An NVX sandbox is a microVM that OpenVMM runs with the managed lifecycle: the guest's init
//! starts a managed agent, which runs workloads that the host sends it over an authenticated
//! control console. This crate holds everything it takes to drive one:
//!
//! - [`protocol`]: the codec of the control console, the `NVXS` session that OpenVMM brokers and
//!   the `NVXC` frames of the guest agent, the guest targets that a sandbox's live shares may not
//!   take, and the exit statuses with which a clone's guest fails fast. It has no dependencies,
//!   so the host, the guest, and test doubles share it.
//! - [`activation`]: the activation record of a layered root, the bounded, versioned, non-secret
//!   record of the settings that its activation applies, which a clone restored from a template
//!   reads in place of its captured configuration, and what Ready reports about the activation.
//!   It needs no features, so the host, the guest, and every adapter's framing share it.
//! - [`contract`]: the control-contract versions that this crate speaks and the compatibility of
//!   NVX releases with them, and the versions of the contracts through which OpenVMM restores a
//!   snapshot.
//! - [`exec`]: the credit and stdio engine of an execution, which the side that runs a workload
//!   and the side that requested it share with any adapter, whatever its framing: the state of
//!   each stream, the credit that bounds the bytes in flight, the order of each stream's data and
//!   end, and the execution's outcome and settlement. It needs only the standard library, so
//!   every configuration has it.
//! - [`net`]: IP networks in CIDR notation and their subtraction, and the forms of a VM's
//!   hostname and guest network address.
//! - [`gpt`]: GUID Partition Table disks whose first partition configures an adapter and whose
//!   others hold the read-only layers of a root: a strict reader of their protective MBR,
//!   primary header, and partition entries, which reads only through a source that its caller
//!   gives, so that the guest and the host share it.
//! - [`uuid`]: UUIDs, such as the GUIDs of a GPT disk, in their canonical text, the one form that
//!   NVX writes and reads, and in the layout in which GPT stores them. It has no dependencies, so
//!   the host and the guest share it.
//! - [`Hypervisor`]: the hypervisors that run the VMs.
//! - [`host`] (feature `host`): artifact discovery, the OpenVMM command line, the lowering of
//!   network and filesystem policies, the host's NVXS connection, control sessions, process and
//!   platform primitives, the supervision of VM instances: starting, reattaching, executing
//!   workloads, and stopping, the store of a sandbox's runtime records and launch journal, and the
//!   manifests, compatibility keys, store, and leases of templates.
//! - `testing` (feature `testing`): a fake OpenVMM that serves the control console without
//!   running a VM.
//! - `guest` (feature `guest`, Linux): the programs that run in NVX guests, such as
//!   `nvx-console-pending`, the guest's NVXS connection, the libraries through which any guest
//!   init or agent opens the terminal on which it serves its session (`guest::terminal`), starts
//!   a program in a new process (`guest::spawn`), has the program enter an execution's
//!   namespaces, view, root, and identity (`guest::entry`), collects the status of each child
//!   that it names (`guest::process`), places, kills, and settles the processes of its
//!   executions in cgroups (`guest::cgroup`), mounts host mappings beneath the descriptors of the
//!   roots that it names (`guest::mappings`), drives each program as one execution, with its
//!   launch, bounded output, input, ending, and outcome (`guest::exec`), alone or under the
//!   execution engine (`guest::exec::binding`), bootstraps the guest for a layered root,
//!   assembles the root, mounts live shares and mappings in it, and tears it down
//!   (`guest::root`), and reads the partition table of a GPT disk's block device (`guest::gpt`),
//!   the typed calls through which it runs the time ABI's steps (`guest::time`) and reseeds after
//!   a restore (`guest::reseed`) in its own process, and the preparation of a template's guest
//!   for a capture before its root's assembly, and the repair of its clones (`guest::clone`). The
//!   crate's `nvx-*` guest binaries run the programs.
//!
//! # Example
//!
//! ```no_run
//! # #[cfg(feature = "host")]
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use std::sync::atomic::AtomicBool;
//! use std::time::Duration;
//!
//! use nvx::host::instance::{Found, Instance};
//! use nvx::host::launch::LaunchSpec;
//! use nvx::host::store::CallerFiles;
//! use nvx::protocol::{Workload, WorkloadEnvironment};
//! use nvx::{Artifacts, Hypervisor};
//!
//! // Locates OpenVMM, the guest kernel, and the Alpine initramfs, which the install metadata of
//! // their release or checkout identifies.
//! let artifacts = Artifacts::discover()?;
//! let runtime = artifacts.runtime_identity()?;
//! let spec = LaunchSpec::new(artifacts, Hypervisor::from_env_or_default()?);
//! // The example keeps no files of its own beside the instance's store.
//! let directory = std::env::temp_dir().join("nvx-example");
//! let instance = Instance::create(directory, "example", CallerFiles::none())?;
//! // Lifecycle transitions hold the instance's lock; executions need none.
//! let mut locked = instance.lock(None)?;
//! let Found::Stopped = locked.find(|| Ok(runtime.clone()))? else {
//!     return Err("the example's VM runs already".into());
//! };
//! locked.start(&spec, &runtime, Duration::from_secs(60))?;
//! let Found::Running(running) = locked.find(|| Ok(runtime.clone()))? else {
//!     return Err("the example's VM stopped".into());
//! };
//!
//! let workload = Workload {
//!     argv: vec!["/bin/echo".to_owned(), "hello".to_owned()],
//!     timeout_ms: 0,
//!     cwd: None,
//!     environment: WorkloadEnvironment::Default,
//! };
//! let timeout = Duration::from_secs(60);
//! let execution = instance.exec(&running, &workload, timeout, timeout)?;
//! let mut stdout = Vec::new();
//! let outcome = execution.pump(
//!     &mut |chunk: &[u8]| {
//!         stdout.extend_from_slice(chunk);
//!         Ok(())
//!     },
//!     &mut |_: &[u8]| Ok(()),
//!     &AtomicBool::new(false),
//! )?;
//! println!("{outcome:?}: {}", String::from_utf8_lossy(&stdout));
//!
//! locked.stop(running, Duration::from_secs(30))?;
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "host"))]
//! # fn main() {}
//! ```
//!
//! # Features
//!
//! - `host` (default): the [`host`] module. It enables `serde`.
//! - `bundled`: stages the OpenVMM executable, guest kernel, and control initramfs at build time;
//!   see `Artifacts::bundled`. The build script downloads the release that `artifacts.json` pins,
//!   or copies the release directory that `NVX_BUNDLE_DIR` names.
//! - `testing`: the fake OpenVMM of `testing::fake_openvmm` and its `nvx-fake-openvmm` binary.
//! - `serde`: the serde form of [`Hypervisor`].
//! - `guest`: the `guest` module and the guest binaries, which the `guest` profile builds as
//!   static `x86_64-unknown-linux-musl` executables.

pub mod activation;
pub mod contract;
mod error;
pub mod exec;
pub mod gpt;
mod hypervisor;
pub mod net;
pub mod protocol;
pub mod uuid;

#[cfg(all(feature = "guest", target_os = "linux"))]
pub mod guest;
#[cfg(feature = "host")]
pub mod host;
#[cfg(feature = "testing")]
pub mod testing;
#[cfg(test)]
mod tests;

pub use error::{Error, ErrorCode, ErrorKind, Result};
#[cfg(feature = "host")]
pub use host::artifacts::Artifacts;
#[cfg(feature = "host")]
pub use host::filesystem::{guest_path, resolve_guest_path};
pub use hypervisor::Hypervisor;
