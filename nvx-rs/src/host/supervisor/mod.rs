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
//! the caller reserved beforehand, with [`LaunchId::generate`], or through a clone's lease, as
//! [Clones](#clones) describes:
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
//! ended, such as a clone's, needs: a holder that checked whether the VMM had exited, then stopped
//! the VM, would otherwise attribute to its stop a VMM that exited in between.
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
//! # Clones
//!
//! A clone restores a generation of a template that it leases from a
//! [template store](crate::host::template::Store), and its lease reserves its launch:
//!
//! 1. [`Store::acquire`](crate::host::template::Store::acquire) leases the generation, and writes
//!    the lease's record in the store and the clone binding in the sandbox, durably, with the
//!    identity of the launch that the lease reserves,
//!    [`Lease::launch`](crate::host::template::Lease::launch), before any VMM of the clone
//!    exists. It returns the sandbox's lock.
//! 2. Under that lock, the caller plans the clone with
//!    [`ClonePlan::new`](crate::host::template::ClonePlan::new), which checks the clone's image,
//!    upper, and memory target against the generation, and claims the clone's scratch image, if
//!    it has one, as
//!    [`ScratchBinding::check`](crate::host::sandbox::scratch::ScratchBinding::check) does. It
//!    then launches the clone with
//!    [`ClonePlan::launch`](crate::host::template::ClonePlan::launch), which checks that the
//!    binding records the lease, writes the clone's files in the sandbox, and runs [`launch`] as
//!    the reserved launch, which [`LockedStore::begin_launch`] requires of a sandbox that holds a
//!    lease, and begins once, under that lock alone, whatever its outcome. The clone's VMM
//!    restores the generation's snapshot in place, and on Linux listens beside it, at the control
//!    socket that the lease reserves in the generation's payload, named after the launch, which
//!    [`Starting::endpoint`] and the runtime record name, as every launch's endpoint, rather than
//!    at the sandbox's own socket.
//! 3. Still under that lock, the caller runs the clone's restore sequence:
//!    [`Restoring::activate`](crate::host::template::Restoring::activate) waits for the VMM's
//!    restore-ready event, activates the clone through the adapter's framing of its control
//!    session, and checks its Ready, by a deadline that runs from the launch;
//!    [`Activated::start`](crate::host::template::Activated::start) starts its sandbox; and
//!    [`Started::commit`](crate::host::template::Started::commit) commits the launch, as
//!    [`Starting::commit`] does. A failure ends the launch, as [`Starting::abandon`] does, through
//!    the VMM's verified identity, and the sandbox records it until the release, with the evidence
//!    that attributes it, as a [`CloneFailure`](crate::host::template::CloneFailure) describes: a
//!    launch that cannot be ended keeps its journal, and the lease, and so does one whose failure
//!    cannot be recorded before its end, whose VMM is ended through its handle.
//! 4. Once the clone's VM has exited, as a stop, a termination, or a reattach verifies,
//!    [`Store::release`](crate::host::template::Store::release) ends the lease, under the
//!    sandbox's lock, taken through the lease's path,
//!    [`Lease::sandbox`](crate::host::template::Lease::sandbox), as the acquisition took it. The
//!    VMM removes its control socket as it exits, the stop or the reattach removes the one that an
//!    abrupt exit leaves, and the release makes the socket's absence durable before the lease
//!    ends.
//!
//! ## The lock order
//!
//! These steps, and every other transition of a template store or a sandbox, take their locks in
//! one order: the lifecycle lock of a capture class's slot, which serializes the class's captures,
//! as [`Store::refresh_template`](crate::host::template::Store::refresh_template) describes, then
//! the template store's lock, then the [lifecycle lock](super::store#lifecycle-locks) of a sandbox,
//! which guards its clone binding and its launch journal, or of a capture's transaction, the
//! sandbox store of the capture's VM, then the scratch image's claim. None waits for a lock while
//! it holds a later one: under its slot's lock, a capture, or its recovery, takes its
//! transaction's lock and then, once it has released it, the store's lock, to publish, and no
//! other; an acquisition releases the store's lock before it returns; a release, a launch, a stop,
//! a reattach, and a removal of the sandbox take no lock of the store; and a retirement takes no
//! lock of a sandbox. No lock but those of a capture lasts for a VM's lifetime: a capture holds
//! its slot's lock and its transaction's until its VM has ended, while the caller of a clone's
//! launch releases the sandbox's lock once the launch commits or ends, and the scratch image's
//! claim belongs to the VMM, which holds it until it exits.
//!
//! ## Crashes
//!
//! A crash at any step leaves records from which a later transition, under the same locks, decides
//! what becomes of the lease, which a release ends only once the sandbox records no VM and no
//! launch:
//!
//! - After the acquisition, the binding reserves a launch that never began, and that no later lock
//!   begins: a release ends the lease at once, and another acquisition, which checks the
//!   generation's eligibility again, reserves another launch.
//! - After the launch's journal, before or after the spawn, or in the clone's restore sequence,
//!   before its commit, [`reattach`] ends the launch, as [`LockedStore::recover`] does, and the
//!   lease lasts until it has: a launch that cannot be ended yet is
//!   [`Reattached::Indeterminate`], and keeps it.
//! - After the commit, the runtime record names the VM, which [`reattach`] holds again: the lease
//!   lasts as long as the VM runs, until a stop or a termination verifies its exit.
//! - During a release, the binding goes first, durably, and leaves the record of the lease, which
//!   a retirement removes.
//!
//! The caller of a clone's launch that restarts takes the clone over with
//! [`reattach_clone`](crate::host::template::reattach_clone), which reattaches as [`reattach`]
//! does, records the failure of a launch that it ended, checks the activation of a clone whose
//! launch committed in a new session, and ends a clone that another runtime identity launched, or
//! that fails the check, through its verified identity.
//!
//! The evidence of the VM's exit decides each outcome. A record of an earlier boot, or of a process
//! ID that another process now has, names a VM that has exited: reattach clears it, and the lease
//! may be released. A VM that cannot be examined is indeterminate, and keeps its records, and so
//! the lease. A forced teardown that fails, or that does not end the VM, keeps its runtime record,
//! and the lease.
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

pub(crate) use self::launch::launch_reporting;
pub use self::launch::{Starting, launch};
pub(crate) use self::reattach::reattach_reporting;
pub use self::reattach::{Reattached, reattach};
pub use self::vmm::{StopOutcome, Stopped, Vmm};
pub use super::platform::inherit::{Inherited, SpawnLock, lock_spawns, spawn_command};
pub use super::spawn::{Launched, VmmCommand, spawn};
