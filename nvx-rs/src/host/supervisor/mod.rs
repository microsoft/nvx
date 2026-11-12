//! The supervisor of a sandbox's VMM, beneath the CLI, [`instance`](super::instance), and
//! external orchestrators: it launches OpenVMM through the [`store`](super::store)'s launch
//! journal, passing it only the descriptors or handles that the caller allows, waits for its exit
//! by event, stops it, and reattaches to it after the orchestrator restarts within the same host
//! boot.
//!
//! Callers keep the configuration, the launch planning, the authorization, and the application
//! protocol. The supervisor owns no control session: the caller checks that a launch is ready, and
//! asks a guest to shut down, through its own transport.
//!
//! # Launches
//!
//! [`launch`] runs the store's launch steps under the sandbox's lock, as a launch whose identity
//! the caller reserved beforehand, with [`LaunchId::generate`]:
//!
//! 1. [`LockedStore::begin_launch`] writes the journal's marker, which names the launch, and the
//!    caller's `command` builds the VMM's [`VmmCommand`] with the control endpoint that the
//!    launch chose.
//! 2. [`spawn`] starts the VMM with the launch's claimed log as its standard output and error, and
//!    [`Launch::record_process`] records its identity right after.
//! 3. The caller checks, through its own transport, that the VMM serves its control endpoint, and
//!    anything else that it requires of a launch, then [`Starting::commit`]s it, which writes the
//!    runtime record, or [`Starting::abandon`]s it.
//!
//! A launch that fails at any step ends as [`Launch::abandon`] ends it, and never leaves an
//! unrecorded VMM running: a VMM whose identity is not recorded is ended through its handle first.
//!
//! A caller launching an owned [`crate::host::vm::VmmCommand`] without a store uses [`spawn_vm`],
//! which refuses an untrusted Windows log-job name rather than taking [`spawn`]'s fallback
//! without a job.
//!
//! ```no_run
//! use std::time::{Duration, Instant};
//!
//! use nvx::host::launch::LaunchSpec;
//! use nvx::host::store::{LaunchId, RuntimeIdentity, SandboxStore};
//! use nvx::host::supervisor::{self, Reattached, StopOutcome, Vmm, VmmCommand};
//! use nvx::protocol::CAPABILITY_LEN;
//!
//! /// Finds the sandbox's VM again after a restart of the orchestrator, or launches it.
//! fn run(
//!     store: &SandboxStore,
//!     spec: &LaunchSpec,
//!     runtime: &RuntimeIdentity,
//!     capability: &[u8; CAPABILITY_LEN],
//! ) -> nvx::Result<Vmm> {
//!     let mut locked = store.lock(Some(Instant::now() + Duration::from_secs(10)))?;
//!     match supervisor::reattach(&mut locked, runtime)? {
//!         Reattached::Live(vmm) => return Ok(vmm),
//!         Reattached::Exited { .. } => {}
//!         // Other artifacts or contracts started the VM: end it through its verified handle,
//!         // without a graceful step that it might not understand.
//!         Reattached::Mismatched { vmm, .. } => {
//!             if vmm.terminate(&mut locked, Duration::from_secs(10))? == StopOutcome::TimedOut {
//!                 return Err(nvx::Error::timed_out("the VM of another runtime still runs"));
//!             }
//!         }
//!         Reattached::Indeterminate(reason) => return Err(nvx::Error::other(reason)),
//!     }
//!     let launch = LaunchId::generate()?;
//!     let starting = supervisor::launch(&mut locked, &launch, runtime, capability, |endpoint| {
//!         let arguments = spec.openvmm_arguments(endpoint, &store.outcome_path())?;
//!         Ok(VmmCommand::new(&spec.openvmm, arguments, store.dir()))
//!     })?;
//!     // Connect to `starting.endpoint()` through the caller's own transport, then commit once
//!     // the VMM serves it, or else abandon the launch.
//!     starting.commit()
//! }
//! ```
//!
//! # Inheritance
//!
//! The VMM receives its capability on its standard input, its claimed log as its standard output
//! and error, and the descriptors (Linux) or handles (Windows) that the caller names in
//! [`VmmCommand::inherit`], under the numbers that they have in the caller, and nothing else of
//! the caller's. On Linux, every other descriptor stays close-on-exec, and only the VMM's own
//! process clears the flag of the allowed ones, so a process that another thread spawns meanwhile
//! inherits none of them. On Windows, the allowed handles join the explicit list of handles that
//! the VMM inherits, and stay inheritable only while it is created; see [`Inherited`].
//!
//! On Windows, the VMM's standard handles are inheritable while it is created too, and a process
//! that another thread of the caller creates in that window without a list of handles, as
//! `std::process::Command` creates one, inherits them all, and holds them until it exits: it could
//! read the VMM's capability before the VMM does, and it holds the VMM's log, and the handles that
//! the VMM is allowed. It holds no claim of the VMM's launch, though: [`spawn`] starts the VMM in a
//! job object of its own, named after its log, whose processes decide the claim while such a
//! process holds the log, so a recovery still ends the launch once the VMM exits; see
//! [the store](crate::host::store#the-log-claim). Each spawn of a VMM holds a [`SpawnLock`]
//! throughout that window, so a caller that launches VMMs spawns its other processes with
//! [`spawn_command`], or under a lock of its own from [`lock_spawns`], and keeps them out of it. On
//! Linux, the lock holds nothing: a process that another thread spawns holds the VMM's log only
//! from its fork until its exec, and recovery checks a held claim again for long enough to see the
//! claim released.
//!
//! # Exit waits and stops
//!
//! A [`Vmm`] holds its VMM through a [`Process`]: a pidfd on Linux and a process handle on
//! Windows, whose exit event wakes waiters, whether or not this caller started the VMM. Linux
//! hosts without pidfds poll the VMM's identity instead.
//!
//! [`Vmm::stop`] runs one graceful step of the caller's, with a bounded deadline, and takes a
//! bounded forced step against the VMM's verified identity only when the caller requests it.
//! [`Vmm::terminate`] takes that forced step alone, with no graceful step: it ends a VM whose
//! guest the caller cannot ask to shut down, such as one that another runtime identity started.
//!
//! A stop decides by its own check whether the VMM still runs as it begins. [`Vmm::stop`] ends a
//! VM whose VMM had exited by then as one that it stopped, and clears the runtime record of its
//! launch. [`Vmm::stop_unless_exited`] stops nothing then, and keeps the record, so that
//! [`reattach`] finds that the VM exited without its holder, as a caller that tells how the VM
//! ended needs: a holder that checked whether the VMM had exited, then stopped the VM, would
//! otherwise attribute to its stop a VMM that exited in between.
//!
//! # Reattach
//!
//! After an orchestrator restarts within the same host boot, [`reattach`] ends an interrupted
//! launch with [`LockedStore::recover`], compares the runtime identity of the recorded VM with the
//! caller's own, with [`RuntimeRecord::check_identity`], and holds the VMM with
//! [`Process::open`]. A record from an earlier boot names a VMM that has exited. A VM of another
//! runtime identity is ended with [`Vmm::terminate`], never through the caller's own protocol,
//! which its guest might not speak.
//!
//! The supervisor signals only a process that it holds through a verified identity: the child
//! that it has not released, a [`Process`] that matched its recorded reference, or, during the
//! recovery of a launch that did not record its VMM, the process that recovery verified to be that
//! VMM. It never signals a process that reuses a recorded ID.
//!
//! [`launch`]: fn@crate::host::supervisor::launch
//! [`LaunchId::generate`]: crate::host::store::LaunchId::generate
//! [`LockedStore::begin_launch`]: crate::host::store::LockedStore::begin_launch
//! [`Launch::record_process`]: crate::host::store::Launch::record_process
//! [`Launch::abandon`]: crate::host::store::Launch::abandon
//! [`Process`]: crate::host::process::Process
//! [`reattach`]: fn@crate::host::supervisor::reattach
//! [`LockedStore::recover`]: crate::host::store::LockedStore::recover
//! [`RuntimeRecord::check_identity`]: crate::host::store::RuntimeRecord::check_identity
//! [`Process::open`]: crate::host::process::Process::open

mod launch;
mod reattach;
#[cfg(test)]
mod tests;
mod vmm;

pub use self::launch::{Starting, launch};
pub use self::reattach::{Reattached, reattach};
pub use self::vmm::{StopOutcome, Stopped, Vmm};
pub use super::platform::inherit::{Inherited, SpawnLock, lock_spawns, spawn_command};
pub use super::spawn::{Launched, VmmCommand, spawn, spawn_vm};
