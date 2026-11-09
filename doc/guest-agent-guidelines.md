# Guest-agent guidelines

`nvx` parses the contracts, captures the guest and drives a restore's activation.
It builds no space, runs no executions and enforces none of the init-agent
obligations below. Each init agent owns its space, executions and lifecycle.
The one exception is `TimeAbi::start_daemon`, which forks the time daemon.
`nvx-init` is the single worked example here, not a public space API.

The architecture and its boundaries are in
[Guest Agents and Workload Model](design/4.2-guest-agents-and-workload-model.md)
and [Isolation and Resource Model](design/4.3-isolation-and-resource-model.md).
The public mechanisms are documented in
[`guest`](../nvx-rs/src/guest/mod.rs),
[`snapshot`](../nvx-rs/src/guest/snapshot.rs) and
[`activation`](../nvx-rs/src/activation/mod.rs).

## Ownership

An init parses the public boot contracts, or receives the restore's activation
record, and builds the VM-wide space from them. It owns the root, its runtime
mounts and view binds, the source and target descriptors of mappings, the
mapper, the helpers, the control session and every execution. Keep these owners
alive for as long as their resources are used; a reset of the transport is not
a new root or a new activation.

The library's capture call does not freeze executions or drive the transport.
The activation state machine holds no workspace fence, makes no system call
and starts no process. The internal application of a record builds the space
without starting helpers; its caller starts and owns the announcer. Starting
the time daemon through `TimeAbi::start_daemon` is the explicit process-starting
exception, not permission for the library to run workloads.

## Construct the space

NVX's space is **internal reference code implemented by `nvx-init`**, not stable
public library API. Hidden compatibility exports do not make it a supported
space interface. Another init may implement its own space or adapt the
reference implementation; it must not depend on a new public space module.

The inputs are the public root and view tokens (`protocol::root` and
`protocol::view`), the export tokens (`protocol::export`), the reserved share
targets (`protocol::share`), mapping payloads (`protocol::maps`) and the
activation record (`activation`). At cold boot they come from the checked
command line; after restore the record supplies the launch-specific settings.
The captured template supplies fixed assembly inputs, such as the runtime
directory, device windows, filesystem tag and device-wait timeout. Do not
substitute a captured launch's view or network for the restore's own record.

The reference space keeps these rules:

- Assemble the checked EROFS image or GPT disk on the `distro` device, checking
  its expected identity, with its upper through overlayfs at a fixed runtime
  directory. The upper is scratch or a tmpfs capped at 1 to 90 percent of VM
  memory, with 50 percent the default. A restored upper is Fresh, not an old
  tenant's writable tree.
- Make the tree `nosuid,nodev` first. Mount `/proc`, read-only `/sys`, `/dev`,
  `/dev/pts`, `/dev/shm` and `/run`; bind the time status read-only at
  `/run/nvx/time`. If a step fails, undo the mounts already made and report any
  cleanup that failed. Do not hide a partial space behind success.
- Apply the VM-wide hostname, machine ID and resolver file. `MachineIdMode::Image`
  keeps the image's `/etc/machine-id`; `Generated` replaces it. The DNS server
  is the one named by the boot contract or this restore's record. The root owns
  the machine-ID and resolver binds, including binds made before a later failure.
- Open the `TargetRoot` and the export's `SourceRoot` before switching, and
  keep them for later mapping requests. Bind beneath the target root, never
  over a reserved target. Account for the root's runtime mounts, such as `/run`,
  before constructing its mapper.
- Start and retain all required helpers, and open every descriptor that later
  work needs, before the one switch. The time daemon and any announcer are
  PID 1's children to supervise, not workload children.
- Switch once: move the root onto `/`, `chroot` and `chdir`. Executions need no
  root entry of their own. Before the switch, teardown closes dependent handles
  and undoes mounts in reverse. After it, nothing unmounts the root; the VM ends
  by syncing and powering off.

### Checked private-space excerpts

These excerpts are quoted from
[`the_reference_space_steps_compile`](../nvx-rs/src/guest/init/boot/tests.rs).
That private unit test type-checks the calls without mounting or switching the
developer host's root. The names below are crate-internal, **not public-API
examples**.

First prepare the root and retain the mapping handles:

```rust
let _space = |assembly: &root::Assembly,
              configuration: &crate::activation::View,
              dns: Option<std::net::IpAddr>,
              export: &crate::protocol::export::HostExport,
              time_status: Option<&std::path::Path>|
 -> Result<
    (root::Root, mappings::Mapper, mappings::SourceRoot),
    Box<dyn std::error::Error>,
> {
    let mut root = root::assemble(assembly)?;
    root.mount_runtime(time_status)?;
    view::apply(&mut root, configuration, dns)?;
    let source = root.mount_export(&export.tag, export.mode.into())?;
    let mut target = mappings::TargetRoot::open(root.path())?;
    target.add_mount(&root.path().join("run"))?;
    let mapper = mappings::Mapper::new(target, mappings::Reserved::container_root());
    Ok((root, mapper, source))
};
```

Start and own the helpers between preparation and the final switch. The caller
keeps the returned root, mapper and source afterwards:

```rust
let _switch = |root: &mut root::Root| root.switch();
```

For a restore, the internal application takes the accepted record and the
captured assembly inputs. It applies the VM-wide view and exposes the
publication and gateway to the init, but starts no announcer:

```rust
let _restored = |record: &crate::activation::ActivationRecord,
                 template: &crate::guest::activation::RootTemplate,
                 deadline: Option<std::time::Instant>| {
    crate::guest::activation::apply(record, template, deadline)
};
```

### Account for the host mapping table

The public `MappingTable` checks and accounts for payloads; the internal mapper
performs mounts. They are different owners and neither replaces the other.
The host announces the total count, then sends consecutive parts in order,
after Ready. `MappingTable::apply` hands a checked part's mappings to the init's
mapper in that order.

A malformed, empty, out-of-order or excess part is refused as a whole without
changing the table. A mapping that fails after earlier mappings succeeded
leaves those mappings in place and **latches** the table: all later parts are
refused and the table cannot become complete. Report both the failure and its
entry. If the init's protocol treats a refused part as terminal, it explicitly
latches the table too. A workload may start only when `is_complete()` is true;
a table of zero mappings is complete immediately.

This public-API excerpt is also a
[`guest` rustdoc test](../nvx-rs/src/guest/mod.rs). The mount callback belongs to
the init; the returned `false` means admission must remain closed, not success
for a workload:

```rust
use nvx::guest::mappings::{Mapping, MappingTable, TableError};

fn apply_part<E>(
    table: &mut MappingTable,
    part: &[u8],
    mount: impl FnOnce(&[Mapping]) -> Result<(), (usize, E)>,
) -> Result<bool, TableError<E>> {
    table.apply(part, mount)?;
    Ok(table.is_complete())
}
```

## Own the executions

`nvx-init` runs executions as children in the single VM-wide space, each in its
own process group. Before fork, the init validates the request's account mode;
when that mode requires an existing account, it checks the root's user and
group databases. The child resets `oom_score_adj` and nice to their normal
values, then sets its credentials: the selected user and primary group IDs,
no supplementary groups, empty capability sets and `no_new_privs`. It then
changes to the requested working directory and execs. Account mode is a
parent-side check, not credential state that the child sets. A failed identity
or working-directory step fails the execution; it must not run with another
identity or in another directory.

End, cancellation, timeout and session reset kill the execution's process
group. The init collects child statuses and cleans up descendants and output
owners; a reset abandons the old epoch's output rather than reporting it in
the replacement session. A process group is a cleanup scope, not a namespace
or cgroup containment boundary. The capture sweep below also deals with
processes that escaped their groups.

This is priority, not per-execution resource policy. PID 1 and its helpers use
`oom_score_adj = -1000` and nice `-10`. Each exec child resets them before
dropping credentials, so the workload cannot reclaim those privileges. There
are no cgroups, namespaces, per-execution roots or per-execution limits in this
model: the VM is the limit. The one additional limit is the RAM upper's cap.

## Keep executions still for capture

An init that supports concurrent executions owns a workspace contract; this
is an init obligation, **not a trait provided or enforced by `nvx`**:

- `freeze(deadline)` first refuses new executions, then stops those running.
  While frozen, a start is refused, not blocked until a later thaw.
- A failed freeze undoes its own work and reports whether it did so (`undone`).
  If it could not undo, the VM must end; work must not resume in an uncertain
  partially frozen state.
- Freeze several workspaces in order, all or nothing. If one fails with
  `undone`, thaw the already frozen workspaces in reverse order. Without
  `undone`, end the VM and thaw nothing.
- `thaw()` reverses a successful freeze, in reverse workspace order.
  `after_restore()` renews what executions need after restore, before Ready.

`nvx-init` has no workspace. It refuses capture while an execution is running,
unless the host's capture request asks it to kill executions. In that case it
kills their process groups, then sweeps every process except PID 1 and its
registered helpers, such as the time daemon, until none remains. Only then
does it call capture.

Capture occurs before mounting the replaceable image, scratch and host export.
Keeping executions still is not sufficient if something from those devices
remains mounted. The public Fresh capture call checks this, but the init owns
the decision to capture and the execution cleanup before it.

## Keep the transport silent

Answer the host's capture request, then send nothing on the transport until
`capture` returns. Keep the session and its owners, but do not send progress,
workload output or another response while capture is pending.

- After `Continued`, thaw workspaces in reverse where they exist, and report
  on the **same session**.
- After a failure with `Resumed`, do the same: thaw and report the failure on
  the **same session**.
- After `Restored(repaired)`, take the session's reset and attach again before
  accepting the activation record. The broker has a fresh instance and epoch,
  with no old queued records. The NVXS guest driver acknowledges RESET with
  `GuestConnection::acknowledge(window)` as part of this attachment.

The following public calls are also compiled as
[`snapshot` rustdoc tests](../nvx-rs/src/guest/snapshot.rs); they deliberately do
not implement the init's freeze, transport, shutdown or space operations:

```rust
use std::time::{Duration, Instant};
use nvx::guest::activation::Activation;
use nvx::guest::snapshot::{self, CaptureFailure, Captured, Devices, Disks, Repaired};

fn fresh_capture(devices: &Devices, deadline: Instant) -> Result<Captured, CaptureFailure> {
    snapshot::capture(devices, Disks::Fresh, deadline)
}

fn begin_activation(repaired: &Repaired, timeout: Duration, now: Instant) -> Activation {
    Activation::new(repaired, timeout, now)
}
```

## Complete restore before Ready

Capture repairs a restored guest before returning `Restored(repaired)`: its
processor and memory targets, kernel randomness and time acknowledgement.
Keep that `Repaired` and one `Activation` for the restore. The activation's
deadline counts from the repair acknowledgement, not from each attachment or
retry.

For `nvx-init`, restore proceeds in this order:

1. `Activation::new`, then the reset/reattachment and the accepted activation
   record. Validate the record and repaired resources before space side effects.
2. Apply the record to a Fresh space, including the VM-wide view and **this
   restore's** network policy. Mount the runtime filesystems and retain mapping
   descriptors. Apply its DNS/listener state; start and own its announcer before
   Ready when listeners are published. Do not reuse the captured launch's DNS
   or listener policy.
3. Start all required helpers, then switch into the root once.
4. Run each workspace's `after_restore()` where workspaces exist, renewing the
   resources its executions need.
5. Call `Activation::complete` with the result.
6. Thaw workspaces in reverse where they exist, then report Ready.

`nvx-init` omits workspace calls, not the ordering around them. Keep the applied
space, mapper, export source, helpers and activation alive. The host sends the
mapping table after Ready; no workload starts until the table is complete.

An identical activation retry joins the existing activation or receives its
cached result; it must not apply or switch the root twice. `nvx-init` preserves
the activation and its original deadline across replacement control sessions,
checking the deadline rather than creating another bootstrap. Changed bytes
or another transaction are not a new permission to activate.

The Fresh repair packet itself carries no replacement network settings. The
post-repair activation record supplies the restore's DNS and publication policy
that the init applies and owns.

## Failures and replay

After `Resumed`, thaw and report the failure on the same session. After
`MustEnd { status }`, start nothing, thaw nothing and power off with that exact
status; the library returns the disposition rather than powering off for the
init. A failed or partially applied activation likewise reports its failure
and ends the VM, not Ready.

Never capture live external connections or other state that cannot be reused
safely by a restored guest. Close or reconstruct it under the init's capture
contract. Never replay an irreversible external effect just because execution
resumed from captured memory. Kernel reseeding does not repair a runtime's
private random state: the runtime must repair its own randomness before it
accepts work after restore, during its `after_restore()` where a workspace
exists. No workspace abstraction in the library enforces these replay rules.
