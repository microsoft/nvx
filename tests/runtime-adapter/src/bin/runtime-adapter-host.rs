//! The runtime adapter's host program.
//!
//! - `runtime-adapter-host` reports the contract that it was built against and whether `nvx`
//!   supports this host.
//! - `runtime-adapter-host stage STORE --distro FORMAT:PATH --distro-identity UUID
//!   [--upper scratch:PATH]` stages an image and a scratch image for the launches of a VM with a
//!   layered root in the sandbox store STORE, which it creates if needed. The image is the raw
//!   file PATH, an EROFS filesystem, `erofs`, or a GPT disk, `gpt`, of which the guest expects the
//!   identity UUID, and the scratch image the ext4 image PATH, if given. Staging verifies the
//!   image explicitly, which hashes the image only when a manifest is beside it, binds the scratch
//!   image without reading it, and records both bindings in STORE, in place of those staged
//!   earlier; see `runtime_adapter::staged::Staged`. It needs no artifacts, and prints nothing.
//! - `runtime-adapter-host launch STORE --guest-agent PATH [--hypervisor NAME] [LAYERED]` launches
//!   a VM in the sandbox store STORE, which it creates if needed, through NVX's supervisor, with
//!   the guest agent PATH, a static `x86_64-unknown-linux-musl` build of `runtime-adapter-guest`,
//!   in place of NVX's agents; see `runtime_adapter::initramfs`. It records what it launches in
//!   STORE; see `runtime_adapter::layered::LaunchRecord`. It attaches to the VM's control
//!   session, exchanges a `Ping` and the longest `Echo` with the guest agent, commits the launch,
//!   and exits, leaving the VM running. The VM runs on the hypervisor NAME, `kvm`, `mshv`, or
//!   `whp`, or else on the one that `nvx::Hypervisor::from_env_or_default` selects.
//!
//!   LAYERED, `--distro FORMAT:PATH --distro-identity UUID --upper (scratch:PATH | ram)
//!   [--runtime-dir DIR] [--overlay-mode MODE] [--scratch-journal POLICY]
//!   [--first-partition-bound BYTES] [--workload-identity UID:GID] [--network FILE] [SHARES]`,
//!   gives the VM a layered root, which the guest agent's worker assembles; see
//!   `runtime_adapter::layered` and `runtime_adapter::worker`. Its image is the raw file PATH, an
//!   EROFS filesystem, `erofs`, or a GPT disk, `gpt`, of which the guest expects the identity UUID:
//!   the file system's UUID, or the disk's GUID. Its upper is the ext4 scratch image PATH, or RAM.
//!   The guest assembles the root below its runtime directory DIR, `/run/runtime-adapter` by
//!   default, in the overlay mode MODE, `trusted-lowers` or `untrusted-lowers`, checks a scratch's
//!   journal against the policy POLICY, `allowed` or `forbidden`, and refuses a first partition of
//!   a GPT disk larger than BYTES, with the guest's defaults, `trusted-lowers`, `allowed`, and 64
//!   KiB, unless given. A workload identity, the user ID UID and the group ID GID, two numbers from
//!   1 to 4294967294, is the identity as which every execution of the sandbox runs: the guest
//!   checks its account in the assembled root, without which the root's `identity` stage fails. The
//!   VM has a network if FILE names one: an `nvx::host::sandbox::SandboxNetwork`, the guest's
//!   address and the policy that OpenVMM enforces there, in the serde form of a plan's network,
//!   which the launch reads before anything else, so that it refuses a network that the plan
//!   refuses before it needs NVX's artifacts or the store, and puts on the adapter's plan, which
//!   renders it; see `runtime_adapter::layered::read_network`. The root configuration says whether
//!   the guest's listeners are published through the network, as OpenVMM publishes them, and
//!   unpublished without one; see `runtime_adapter::layered::LayeredLaunch::with_network`. Before
//!   it spawns the VM, the launch checks the bindings that STORE stages for the image and the
//!   scratch image, from their metadata alone, and fails if STORE stages none for either, or one no
//!   longer holds, as for an image that changed after it was staged. It hands OpenVMM the files
//!   that the checks opened, which it holds until the guest has assembled the root, or until it has
//!   torn down a launch that failed, and records the plan, with its network, and the bindings.
//!
//!   SHARES, `--share HOST:TARGET[:ro]... [--deny PATH]... [--allow PATH]...
//!   [--reserved TARGET]... [--share-root DIR]...`, shares each host directory or regular file
//!   HOST with the root, at the absolute guest path TARGET, read-only with `:ro`, and read-write
//!   otherwise. The guest path starts at the last `:/`. Each HOST must lie inside a host directory
//!   DIR, which the adapter authorizes, once links are resolved: the launch refuses one outside
//!   them before it plans the export, with NVX's `filesystem::plan`. The export hides each host
//!   path PATH of `--deny` inside the shares, but shows again each host path PATH of `--allow`
//!   inside a hidden one, with the directories on the way to it listing only it, and no TARGET may
//!   take or cover a guest tree that the container root keeps, or a tree TARGET that `--reserved`
//!   keeps for the adapter. The launch refuses a request that `filesystem::plan` refuses, such as
//!   one with a read-only share inside a read-write one, before it spawns the VM, and checks the
//!   export with `ExportPlan::verify` before it spawns it. OpenVMM exports the shares; see
//!   `runtime_adapter::sharing::HostFsSharing`.
//!
//!   Once attached, the launch follows the host's order; see `runtime_adapter::host`. It sends the
//!   root configuration, which announces how many mappings the export's table holds, and waits for
//!   the guest agent's `Ready`, then checks that the guest agent is of the build that the host
//!   ships, sends the mapping table, in order, then `Start`, and exchanges the `Ping` and the
//!   `Echo` once the guest started the sandbox. A guest agent of another build gets nothing more,
//!   and the launch fails, which terminates the VM. If the guest reports that a stage of the root
//!   failed instead, the launch waits for the VM to end, abandons it, and checks that the VM ended
//!   with the stage's status. If the guest fails a mapping, the launch sends no more of the table,
//!   nor `Start`, asks the guest agent to stop, which tears down the root, waits for the VM to
//!   end, abandons it, and checks that the VM ended with success and a complete teardown.
//! - `runtime-adapter-host create STORE --guest-agent PATH --distro FORMAT:PATH --distro-identity
//!   UUID [--hypervisor NAME] [--cpu-profile PROFILE] [--processors N] [--boot-processors B]
//!   [--memory MIB] [--memory-capacity MIB] [--scratch-length BYTES] [--network FILE]` serves a
//!   template of the guest agent PATH from the template store STORE, an
//!   `nvx::host::template::Store`, which it creates if needed: a generation of the template's
//!   class that the store holds, and that is eligible at the host's time, or else a new one,
//!   which the store captures and publishes, with `Store::create_template`. The options choose
//!   the class; see `runtime_adapter::capture`. The placeholder is the raw file PATH, an EROFS
//!   filesystem, `erofs`, or a GPT disk, `gpt`, with the identity UUID, a trusted artifact of the
//!   platform, whose length is the capacity of every clone's image. The template's VM runs on the
//!   hypervisor NAME, as a launch's does, with the CPU profile PROFILE, `auto` by default, N
//!   processors, 1 by default, of which B are online at boot, 1 by default, the fewest that its
//!   clones take, since each clone's restore brings its own processor target online, up to N, MIB
//!   mebibytes of RAM, 256 by default, and a RAM capacity of MIB mebibytes, its RAM by default, to
//!   which a restore may expand it, and a dormant scratch of BYTES bytes, 64 MiB by default. It has
//!   one NIC at the guest's address of the network FILE, which the creation reads as a launch reads
//!   its network, under the complete deny-all policy, whatever the network's own, which each
//!   clone's restore replaces with the clone's, unless the network's policy lets nothing through,
//!   and none without a network: the networks of every policy at one guest's address share one
//!   class, and so one template. The creation
//!   reads the network before anything else, then stages the placeholder as `stage` stages an
//!   image, verifying it explicitly, which hashes it only when a manifest is beside it, and checks
//!   the class before it creates the store. A capture boots the template's VM through NVX's
//!   supervisor, with the guest agent in place of NVX's agents, which captures the VM before it
//!   assembles any root, in template mode; see `runtime_adapter::template`. The store checks what
//!   the VM left, and publishes the snapshot as its next generation. The command writes the
//!   initramfs of the guest agent into a directory of its own in the system's temporary
//!   directory, private to the current user, which it removes once the creation returns, and
//!   prints the generation only once the store has committed it. Once it served the generation,
//!   the creation evicts the generations of the template's class that it supersedes: it retires
//!   every other generation of the class that the store published before it, or that a tombstone
//!   quarantines, and the tombstones that interrupted retirements left alone, whatever their
//!   class; see `runtime_adapter::generations`. An eviction that fails leaves the rest for
//!   the next one, and fails nothing that the creation served.
//! - `runtime-adapter-host refresh STORE` takes the options of `create`, and captures a new
//!   generation of the template, as `create` captures one, with `Store::refresh_template`, whether
//!   or not the store holds an eligible generation of its class, then evicts the generations of
//!   the class that the new one supersedes, as `create` does. No other command captures, and no
//!   restore triggers a capture.
//! - `runtime-adapter-host list STORE` reports each generation of the template store STORE, which
//!   it opens, and does not create, in the order of their publication, from the store's index,
//!   its tombstones, and the records of its leases alone: it reads no generation's payload, so it
//!   loads no VM state, and hashes nothing, and it takes no lock. Its eligibility is at the host's
//!   time; see `runtime_adapter::generations`.
//! - `runtime-adapter-host inspect STORE GENERATION` reports the generation of STORE whose store
//!   generation is GENERATION, a positive number, as `list` reports it, with its leases and the
//!   metadata of its capture. It fails for a generation that the store does not hold.
//! - `runtime-adapter-host remove STORE GENERATION` retires the generation GENERATION of STORE,
//!   with `Store::retire`: it tombstones the generation first, so that it takes no new lease, and
//!   deletes it once no lease of it remains, but never moves or deletes a generation that a clone
//!   leases. While leases remain, it reports them, and leaves the generation tombstoned, which a
//!   later `remove`, or the next eviction of its class, deletes once they drained. A generation
//!   that the store no longer holds is retired already.
//! - `runtime-adapter-host reattach STORE` reattaches to the VM of STORE, which an earlier process
//!   launched and which it holds although it is not its child, attaches a new session to it, in
//!   the session's next epoch, and exchanges a `Ping`, leaving the VM running.
//! - `runtime-adapter-host start STORE --template TEMPLATES --generation G --distro FORMAT:PATH
//!   --distro-identity UUID --upper (scratch:PATH | ram) [--processors N] [--memory MIB]
//!   [--workload-identity UID:GID] [--overlay-mode MODE] [--scratch-journal POLICY]
//!   [--first-partition-bound BYTES] [--machine-id (image | generated)] [--network FILE] [SHARES]`
//!   starts a clone of the
//!   generation G of the template store TEMPLATES, an `nvx::host::template::Store`, in the sandbox
//!   store STORE, which it creates if needed; see `runtime_adapter::clone`. The clone's VM has the
//!   network of the file FILE, if given, which the command reads first, as `launch` reads its
//!   network: it must be at the guest's address of the template's NIC, under a policy of its own,
//!   which the clone's restore installs on the NIC in place of the template's complete deny-all
//!   policy, or for a template without a NIC none, or one that lets nothing through. SHARES, as
//!   `launch` takes them, share
//!   host paths with the clone's root: the command plans their export as a launch does, with
//!   NVX's `filesystem::plan`, and refuses shares that it cannot export before anything but its
//!   network. It then reads the generation from the store's index, and, unless N and MIB
//!   mebibytes are both given, its compatibility key, before it needs NVX's artifacts: the key's
//!   processors online at boot, or all of its processors for a template that records none, are
//!   then the clone's processor target unless N is given, and its RAM the clone's memory target
//!   unless MIB mebibytes are given. A key that cannot be read is left to NVX's plan, which reads
//!   it again, and attributes the failure, to the generation or to the host, as it does for every
//!   clone. The store then leases the generation to the sandbox, at the host's time, with
//!   `Store::acquire`, which refuses a generation that a tombstone quarantines, or that is not
//!   eligible then, and NVX's `ClonePlan` checks the clone against the generation, from the
//!   metadata of its files
//!   alone: its network, which must attach a NIC exactly when the template's key records one, at
//!   the guest's address of the template's NIC, under a policy of its own, its image, the
//!   raw file PATH, an EROFS filesystem, `erofs`, or a GPT disk, `gpt`, of
//!   which the guest expects the identity UUID, no longer than the template's placeholder,
//!   whatever the placeholder's format, and of its block sizes, its upper, the ext4 scratch image
//!   PATH, of the geometry of the template's scratch, or RAM, which needs no file, with the
//!   bindings that STORE stages for the images, as
//!   `stage` stages them, its processor target, 1, 2, 4, or 8, from the template's processors
//!   online at boot to its capacity, which the restore brings online, its memory target, in whole
//!   blocks, from the template's RAM to its
//!   capacity, and its export, if it has one, which must be one that a cold launch attaches, of
//!   directories and files that neither lie inside the template store's directory nor hold it:
//!   the clone's restore attaches it to the template's dormant filesystem slot as an aggregate.
//!   A request that the template cannot serve fails there, before any VMM starts. The command
//!   records the clone in STORE, with the template store and the generation; see
//!   `runtime_adapter::clone::CloneRecord`. Once NVX checked the export again, with
//!   `ExportPlan::verify`, OpenVMM restores the generation's snapshot in place, with the clone's
//!   devices, through NVX's supervisor, as the launch that the lease reserves, and the restore
//!   sequence of `nvx::host::template` activates the clone through the adapter's clone order,
//!   `runtime_adapter::host::CloneOrder`, with the clone's activation record: the workload
//!   identity UID:GID, if given, as which every execution runs, whose account the guest checks in
//!   the clone's root before it is ready, the overlay mode MODE and the scratch's journal policy
//!   POLICY, as a launch takes them, for a GPT disk, the bound BYTES of its first partition, 64
//!   KiB unless given, as a launch's guest takes it by default, the export, by the identity of
//!   its plan, with the number of its mappings, and for a template with a NIC the clone's network:
//!   the identity of its policy, the DNS server that OpenVMM serves the guest under the policy, if
//!   any, which NVX finds as OpenVMM does, since the template's guest learned none, and whose
//!   address the clone's guest writes to a resolver file that it binds read-only over the root's
//!   `/etc/resolv.conf`, whether the policy publishes the workload's listeners, as it does with
//!   ingress, or with host-loopback access both ways, for which the clone's guest starts their
//!   announcer and refuses `CAP_NET_BIND_SERVICE` and `CAP_NET_RAW` to its executions, and the
//!   URL of the policy's proxy, if any, which every execution then finds in its environment, as a
//!   launch's executions do. Every
//!   execution of the clone sees the hostname
//!   `runtime-adapter`, and the machine ID that `--machine-id` chooses: the image's own, `image`,
//!   by default, or one that the guest generates once for the clone, `generated`, which a
//!   read-only bind holds at `/etc/machine-id`. The guest refuses a first partition larger than
//!   BYTES, and an upper that is not fresh, whose `upper` or `work` directory on the scratch image
//!   exists and is not empty, as an earlier clone's would be, before the clone is ready. Once the
//!   clone is ready, the sequence sends the export's mapping table, in order, as a launch sends
//!   its own; a clone without shares has no export: its restore leaves the slot dormant, and the
//!   sequence sends no table. If the guest fails a mapping, the sequence sends no more of the
//!   table, nor `Start`, and ends the clone, whose guest the clone order asks to stop, which tears
//!   down the root, and which then fails as below. Once every mapping applied, the sequence
//!   starts the clone's sandbox, and the command exchanges a `Ping` and the longest `Echo` with
//!   the guest agent, commits the launch, and exits, leaving the VM running, and the sandbox
//!   holding the lease, which a stop releases.
//!   A clone that fails, from its plan to its commit, fails with NVX's
//!   `nvx::host::template::CloneFailure`, which attributes the failure by its evidence, and records
//!   it in STORE from the clone's launch on: the command prints the failure's record, releases the
//!   lease once the failure's teardown is resolved, and nothing of the clone runs, or keeps it, and
//!   the records, for a later reattach, while it is not, and quarantines the generation through
//!   its tombstone, once it released the sandbox's lock, for a failure of the template. A guest
//!   agent that does not answer the `Ping` or the `Echo` fails the clone so, through
//!   `Started::fail`, as the restore sequence attributes the failures of its sessions. A start
//!   that fails otherwise, at a step of the adapter's own before the clone's launch, releases the
//!   lease. A selection that the template store refuses ends the start before it leases anything,
//!   with the refusal's record, in the form of a failure's: a template store that does not exist,
//!   and a generation that the store never published, or no longer holds, name no template; a
//!   generation whose capture the restore window of its time contract no longer admits at the
//!   host's time is expired; and one that a tombstone quarantines, or whose capture is after the
//!   host's time, is ineligible.
//! - `runtime-adapter-host stop STORE` reattaches and attaches as `reattach` does, then stops the
//!   VM: it asks the guest agent to stop, which first tears down a layered root, waits for the VM
//!   to exit, and checks the VM's outcome report. A VM whose guest agent does not confirm the stop
//!   is terminated. A stop of a clone releases the lease once the VM exited, and the clone's
//!   records told how it ended, which the release removes, with the template store that the
//!   clone's record names: a clone whose VM had exited when the stop began ended without its
//!   holder, and one whose VM the guest ended, once the stop began, with a status that is evidence
//!   of a failure, such as its repair's, failed, which ends the stop as it ends `start`.
//! - `runtime-adapter-host probe STORE PROBE...` reattaches and attaches as `reattach` does, then
//!   runs each PROBE in the VM's layered root, and prints the facts that it found. A PROBE is the
//!   text of a probe: its name, then its arguments, each after one space; see
//!   `runtime_adapter::probe` and `runtime_adapter::worker::probes`.
//! - `runtime-adapter-host exec STORE EXECUTION [; EXECUTION]...` reattaches and attaches as
//!   `reattach` does, then runs each EXECUTION in the VM's layered root, all at once, over that one
//!   session, through `runtime_adapter::host::Executions`, and prints their outputs and outcomes
//!   once every one finished. It runs at most 8 executions, as many as the guest runs at once; see
//!   `runtime_adapter::exec::MAX_EXECUTIONS`. An EXECUTION, `[--identity UID:GID [--groups GID,...]
//!   [--capabilities N,...] [--numeric]] [--cwd DIR] [--env NAME=VALUE]... -- PROGRAM [ARG]...`,
//!   runs PROGRAM with its ARGs, up to a lone `;`, as `find -exec` takes them, without standard
//!   input. Without `--identity`, it names no identity, and runs as the sandbox's workload
//!   identity, which the guest otherwise refuses; with it, it runs as the user ID UID and the group
//!   ID GID, with the supplementary groups GIDs and the capabilities numbered N, such as 0 for
//!   `CAP_CHOWN`, if given, and the guest checks its account, unless `--numeric` says that the IDs
//!   have none. It starts in the root's directory DIR, or `/`, with exactly the environment of its
//!   `--env` entries, then, if the VM's network has a proxy, the proxy's URL as the guest sees it
//!   in `http_proxy`, `https_proxy`, `HTTP_PROXY`, and `HTTPS_PROXY`, unless the entries name any
//!   of them; see `runtime_adapter::layered::with_proxy`. Before it starts any execution, the
//!   command checks that the framing still carries each start with the proxy's URL, and runs none
//!   of the executions if one no longer fits. It runs for at most a minute, with 16 KiB chunks
//!   and the credit of 4 of them for its input and 8 for its output, which its standard output and
//!   error share. The command keeps at most 16 MiB of each stream, and cancels an execution whose
//!   stream would exceed them; see `runtime_adapter::exec::MAX_CAPTURE`. Execution N, from 1, has
//!   the key `(B + N, E)`: E is the epoch of the command's attachment, and B the first 8 bytes of
//!   the broker instance's ID, as a little-endian number. The guest refuses a key that it heard of
//!   before, and the epochs of a broker instance only advance, while a broker restored from a
//!   snapshot has a new instance ID, so no command repeats the key of another's execution.
//! - `runtime-adapter-host run STORE --template TEMPLATES --generation G --distro FORMAT:PATH
//!   --distro-identity UUID --upper (scratch:PATH | ram) [--processors N] [--memory MIB]
//!   [--workload-identity UID:GID] [--overlay-mode MODE] [--scratch-journal POLICY]
//!   [--first-partition-bound BYTES] [--machine-id (image | generated)] [--network FILE] [SHARES]
//!   EXECUTION` runs one execution in
//!   a clone of the generation G of the template store TEMPLATES, in the sandbox store STORE,
//!   which it creates if needed, then tears the clone down. The EXECUTION is one of `exec`'s, whose
//!   options may come among the start's, in any order, and whose PROGRAM and ARGs are every
//!   argument after `--`. The command first reads the clone's network, and plans the clone and the
//!   export of its shares, as `start` does, then checks that the guest takes the execution's
//!   start, with the URL of the proxy of the clone's network, if any, in its environment, as
//!   `exec` gives it to an execution of a clone, so that none of them fails the command once it
//!   staged anything. Its preflight then stages the clone's images in STORE, as `stage` stages
//!   them, which verifies the image, and hashes it only when a manifest is beside it, and prints
//!   that it did, before the clone's start, which hashes nothing. It starts the clone as `start`
//!   does, and ends as a start ends once the template store refuses the selection, or the clone
//!   fails. It runs the execution in the clone's root, as `exec` runs one, through the guest
//!   agent's execution control, in the session in which the clone's sandbox started, so that the
//!   program, its arguments, and its environment reach the guest in the execution's start, never
//!   in the template's boot tokens, and prints its output and outcome once it finished. The
//!   execution finds the clone's shares beneath the root, since the restore sequence maps them
//!   before the clone's sandbox starts. Then it stops the clone, as `stop` does, whatever the
//!   execution did, which releases the lease once the VM exited. The stop takes the same session,
//!   in which it skips the guest's messages about the execution that may still arrive once the
//!   command finished it without them, as when it gave up waiting for its outcome. It holds the
//!   sandbox's lock, which the lease took, until then, so no other command reattaches to the clone
//!   meanwhile.
//!
//! Each command but `stage`, `list`, `inspect`, and `remove`, which need no artifacts, takes NVX's
//! artifacts from the directory that `NVX_ARTIFACTS_DIR` names, a repository checkout or an
//! extracted release, and the VM's runtime identity from NVX's identity of them, with the guest
//! agent's build that `--guest-build BUILD` names, this crate's version by default; see
//! `runtime_adapter::runtime`. Reattach finds a VM that booted cold under another runtime identity
//! mismatched, and then signals nothing: the VM keeps running.
//!
//! The sandbox of a clone holds the clone's lease, through whose path the commands that reattach
//! take the sandbox's lock. They reattach to a clone with `nvx::host::template::reattach_clone`,
//! in a new session of the adapter's clone order, in which the clone answers the Activate of its
//! record again, with the Ready that it answered once, and which they then use as `reattach`
//! attaches one. The reattach ends a clone that runs under another runtime identity, without a
//! word to its guest, or that fails the check of its Ready, through its verified identity, and
//! finishes the record of an earlier failure of the clone, whose teardown it resolves if it can.
//! The command then releases the lease of a clone whose VM ended without a failure, and fails;
//! ends as `start` does for a clone that failed; and keeps the lease and the records, and fails,
//! for a clone whose state cannot be determined. It releases a lease through the template store
//! that the clone's record names, once the lease is of it. A sandbox whose start ended before it
//! recorded the clone holds a lease of a template store that no record names: it records no
//! clone, or an earlier clone's, which a release leaves, of another template store perhaps.
//! `reattach STORE --template TEMPLATES` and `stop STORE --template TEMPLATES` release such a
//! lease through the template store TEMPLATES, in place of the record's, which the release refuses
//! unless the lease is of it.
//!
//! The commands print what they find as `KEY=VALUE` lines:
//!
//! - `reattached`: what reattach found: `live`, `mismatched`, `exited`, or `indeterminate`, and of
//!   a clone, `live`, `ended`, `failed`, or `indeterminate`;
//! - `differences`: the parts of the runtime identity in which a mismatched VM differs, separated
//!   by commas;
//! - `progress`: the step that a clone, which ended, reached: `planned`, `launching`, `spawned`,
//!   `event`, `acknowledged`, `activating`, `ready`, `starting`, `started`, or `committed`;
//! - `pid`: the VMM's process ID;
//! - `instance` and `epoch`: the broker instance, in hexadecimal, and the epoch of the session
//!   that the command attached to;
//! - `root`: how the guest answered the root configuration: `ready` or `failed`, or `failed`
//!   once a stop finds that the root's teardown failed;
//! - `build`: the build of the guest agent that assembled the root;
//! - `stage` and `problem`: the stage of the root that failed, such as `assembly`, and the
//!   problem, or the problem of a mapping that failed;
//! - `table`: how the guest took the mapping table: `applied` or `failed`;
//! - `mapped`: how many of the table's mappings the guest applied;
//! - `sandbox`: `started` once the guest started the sandbox;
//! - `preflight`: `staged` once `run` staged the images of its clone;
//! - `PROBE.KEY`: the fact KEY that the probe named PROBE found;
//! - `exec.N.KEY`: what execution N did, with these KEYs:
//!   - `launch`: `started`, `failed STAGE ERRNO`, with the number of the guest's stage that failed
//!     and the error number, or `none` once the guest reported neither;
//!   - `status`: how the program ended: `exit CODE`, `signal NUMBER`, or `none`;
//!   - `cause`: why a side ended the execution, `cancelled`, `dropped`, `timed-out`, `reset`, or
//!     `disconnected`, `other` for a cause that `nvx` adds later, or `none` for an execution that
//!     ended on its own;
//!   - `truncated`: the streams that lost bytes, `stdout`, `stderr`, or both, separated by a
//!     comma, or `none`;
//!   - `settled`: `reusable` once the execution settled, or `fenced`;
//!   - `stdout` and `stderr`: the program's standard output and error;
//! - `stopped`: how the stop ended: `graceful`, `forced`, or `timed-out`, or `exited` once a
//!   clone's VM, which the command found running, had exited when the stop began, which stopped
//!   nothing;
//! - `outcome`: the category and status code of the VM's outcome report, such as `success 0`;
//! - `teardown`: whether the VM's teardown completed: `complete` or `incomplete`;
//! - `created`: how `create` or `refresh` served the template: `captured`, with a generation that
//!   a capture made and the store published, which a refresh always prints, or `existing`, with
//!   one that the store held;
//! - `generation`, `key`, and `class`: the store generation of the template that `create` or
//!   `refresh` served, or that `inspect` reports, and the identities of its compatibility key and
//!   of its capture class, each 64 hexadecimal digits, or the store generation that `start` or
//!   `run` leased;
//! - `retired` and `draining`: the generations that the eviction of `create` or `refresh` retired,
//!   each list separated by commas, or `none`: those of the class that it deleted, or found gone
//!   already, with those whose interrupted retirement it completed, and those that leases keep,
//!   tombstoned;
//! - `lease`: the identity of the lease that `start` or `run` took, 32 hexadecimal digits;
//! - `downtime` and `source`: the downtime of the leased generation's capture at the host's time,
//!   in nanoseconds, and the clock that measured it: `monotonic`, the host's monotonic clock, on
//!   the host's boot that captured the template, or `utc` otherwise;
//! - `released`: the identity of the lease that a stop, a start that failed, or the teardown of a
//!   run released;
//! - `failure.KEY`: the record of a clone's failure, or of the selection of a start or a run that
//!   the template store refused, with these KEYs:
//!   - `outcome`: what the failure is attributed to: `request`, `staging`, `host`, `unknown`,
//!     `template`, or `repair`, or, for a refused selection, `no-template`, `expired`, or
//!     `ineligible`;
//!   - `evidence`: what the host saw of it, which attributes it, such as `supplied-length`, or,
//!     for a refused selection, `no-store`, `unpublished`, or `retired`, of no template,
//!     `expired`, or `tombstone` or `early`, of an ineligible generation;
//!   - `stage`: where the clone failed: `plan`, `launch`, `restore`, `activation`, `start`,
//!     `commit`, `run`, or `reattach`, or `selection` for a refused selection;
//!   - `progress`: the step that the clone reached, as `progress` spells it, or `none` for a
//!     refused selection;
//!   - `status`: the status with which the guest ended the VM, as OpenVMM's outcome report gives
//!     it, or `none`;
//!   - `teardown`: how the teardown that followed ended: `no-vm`, `exited`, `terminated`, or
//!     `unresolved`;
//!   - `code`: the code of the failure's error, such as `runtime-mismatch`, or `not-found`,
//!     `template-tombstoned`, or `template-ineligible` for a refused selection;
//! - `quarantined`: whether the command quarantined the generation of a clone that failed:
//!   `yes`, for a failure of the template, or `no`;
//! - `store`: the identity of the template store that `list` or `inspect` read, 32 hexadecimal
//!   digits;
//! - `generations`: the store generations that `list` found, separated by commas, or `none`;
//! - `generation.G.KEY`: what `list` found of the generation G, with the KEYs `key`, `class`,
//!   `state`, `eligibility`, `reason`, and `leases`, which `inspect` prints of its generation;
//! - `state`: `available`, or `tombstoned` once a tombstone quarantines the generation;
//! - `eligibility` and `reason`: whether a clone may restore the generation at the host's time,
//!   `eligible`, `early`, or `expired`, and NVX's reason, with the downtime of its capture;
//! - `leases`: the identities of the generation's leases that the store's records list, each 32
//!   hexadecimal digits, separated by commas, or `none`;
//! - `lease.ID.sandbox` and `lease.ID.launch`: the directory of the sandbox that holds the lease ID,
//!   with its links resolved, and the launch of its clone that the lease reserves;
//! - `storage`: the storage generation of the capture, 32 hexadecimal digits;
//! - `time.host`, `time.boot`, `time.clock`, `time.utc`, `time.monotonic`, and `time.lineage`: the
//!   time contract of the capture: the identities of the host and of its boot that captured it,
//!   each 32 hexadecimal digits, the host's monotonic clock, `linux-boottime` or
//!   `windows-interrupt-time`, the anchor of the capture in UTC, in nanoseconds since the Unix
//!   epoch, and by that clock, in nanoseconds, and its time lineage counter;
//! - `length.state` and `length.memory`: the lengths of the snapshot's saved state and of its RAM,
//!   in bytes;
//! - `removed`: what `remove` did: `retired`, once the generation is gone, or `draining`, once it
//!   tombstoned the generation, which the leases that it reports keep.
//!
//! Values escape `\`, newlines, and carriage returns as `\\`, `\n`, and `\r`, and each byte of a
//! program's output that is not part of UTF-8 text as `\xHH`, with two hexadecimal digits.
//!
//! For tests, `launch`, `reattach`, and `stop` take a step hook from the variable
//! `RUNTIME_ADAPTER_STEP_HOOK`, which pauses the command at a named step, for a time that a launch
//! leaves out of its deadline, or exits it there without unwinding, with status 5; see
//! `runtime_adapter::hook`. `start` takes one at `checked` alone: once NVX's `ClonePlan` checked
//! the clone against the leased generation, and holds the files that its checks opened, before the
//! start records the clone, and before the clone's launch checks its export again. `start` and
//! `run` take the host's time, at which the store checks the generation's eligibility, from the
//! variable `RUNTIME_ADAPTER_HOST_TIME`, if it is set, as a clock fixture, such as another boot's;
//! see
//! `runtime_adapter::clone::HOST_TIME_VARIABLE`.
//!
//! They exit with 0 once they did what they were asked, `exec` once every execution finished,
//! whatever its outcome, and `run` once its execution finished so, and the clone stopped, as
//! `stop` stops one, with 3 when reattach finds a VM that booted cold mismatched, with 4 when
//! the guest reported that a stage of the root failed, and the VM ended with the stage's status
//! and a complete teardown, with 5 when the step hook ends a command, with 6 when the guest failed
//! a mapping of the table, and the VM stopped gracefully, with success and a complete teardown,
//! with 7 when `remove` finds leases that keep the generation, which it tombstoned,
//! with 8 when a clone failed, once nothing of it runs and the command released its lease, with 9
//! when a clone failed, and its teardown is unresolved, so that the sandbox keeps the lease, with
//! 10 when the template store refused the selection of a start or a run, so that it leased and
//! launched nothing, and with 1, after a diagnostic on standard error, when they fail, as `run`
//! does once its execution failed to run, after the teardown, unless the teardown ended with 4, 8,
//! or 9, as when the clone's VM exited during the execution, which `run` then exits with, after
//! the diagnostic. Anything else is a usage error, with status 2, as is a value of
//! `RUNTIME_ADAPTER_STEP_HOOK` that sets no step hook, or a step hook of a start at another step
//! than `checked`, or a value of `RUNTIME_ADAPTER_HOST_TIME` that is not a host's time.

use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use nvx::activation::{MachineIdMode, StoreGeneration};
use nvx::exec::{Cause, Key, Limits, Outcome, Status, Stream, Timers, Window};
use nvx::host::artifacts::{Artifacts, absolute};
use nvx::host::filesystem::ExportPlan;
use nvx::host::instance::generate_capability;
use nvx::host::launch::{DEFAULT_MEMORY_MIB, LaunchSpec};
use nvx::host::nvxs::HostConnectionError;
use nvx::host::report::{self, Report};
use nvx::host::sandbox::SandboxPlan;
use nvx::host::session;
use nvx::host::store::{CallerFiles, IdentityPart, LOG_NAME, LaunchId, LockedStore, SandboxStore};
use nvx::host::supervisor::{self, Reattached, Starting, StopOutcome, Vmm, VmmCommand};
use nvx::host::template::{
    Acquired, CAPTURES_NAME, Capture, CloneControl, CloneFailure, ClonePlan, Created,
    DowntimeSource, Eligibility, Evidence, FailureRecord, Generation, HostTime, Lease,
    ReattachedClone, RestoreDeadlines, Retirement, Store, TRANSACTION_NAME, reattach_clone,
};
use nvx::protocol::CAPABILITY_LEN;
use nvx::protocol::root::ImageFormat;
use nvx::uuid::Uuid;
use nvx::{ErrorCode, ErrorKind, Hypervisor};
use runtime_adapter::GUEST_BUILD;
use runtime_adapter::capture::{
    DEFAULT_BOOT_PROCESSORS, DEFAULT_CPU_PROFILE, DEFAULT_PROCESSORS, DEFAULT_SCRATCH_LENGTH,
    Request,
};
use runtime_adapter::clone::{CloneRecord, host_time_from_env};
use runtime_adapter::exec::{self, AccountMode, HostExecution, Start};
use runtime_adapter::frame::{MAX_BODY, Message};
use runtime_adapter::generations::{self, Entry};
use runtime_adapter::hook::{STEP_HOOK, Step, StepHook};
use runtime_adapter::host::{self, Client, ClientError, CloneOrder, Executions, OrderError};
use runtime_adapter::initramfs;
use runtime_adapter::layered::{Bindings, LaunchRecord, LayeredLaunch, read_network, with_proxy};
use runtime_adapter::probe::{Probe, ProbeReply, escape};
use runtime_adapter::root::{JournalPolicy, OverlayMode, RootFailure};
use runtime_adapter::runtime::Runtime;
use runtime_adapter::sharing::{HostFsSharing, Share};
use runtime_adapter::staged::Staged;

/// How long a command waits for the sandbox's lock, and `remove` and an eviction for the template
/// store's lock to retire a generation.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a launch waits for the VM to boot, its guest agent to assemble a layered root, and to
/// answer.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// How long reattach, stop, and probe wait to attach a session and for the guest agent's answers.
const SESSION_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a stop waits for the guest agent's confirmation and for the VM to exit, and how long
/// a launch whose root failed waits for the VM to exit.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a stop waits for a VM that it terminated to exit.
const FORCED_TIMEOUT: Duration = Duration::from_secs(10);
/// The name of the initramfs that boots the guest agent, which a launch writes into the sandbox's
/// store, and a creation into a private directory of its own.
const INITRAMFS_NAME: &str = "runtime-adapter.cpio.gz";
/// How long a creation waits for the slot of the template's class and for the template store's
/// lock, and for a capture, which boots the template's VM and writes its RAM.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(120);
/// The prefix of the name of the directory, private to the current user, into which a creation
/// writes the initramfs of a capture's VM, in the system's temporary directory.
const CAPTURE_DIRECTORY_PREFIX: &str = "runtime-adapter-template-";
/// The hexadecimal digits of the identity of a capture class that name its slot in a template
/// store, as the store documents its layout.
const SLOT_NAME_LEN: usize = 16;
/// Bytes in a mebibyte, the unit of `create`'s `--memory` and `--memory-capacity`.
const MIB: u64 = 1024 * 1024;
/// The exit status of a command that finds the VM mismatched.
const MISMATCHED: u8 = 3;
/// The exit status of a command whose guest reported that a stage of the root failed, once the VM
/// ended with the stage's status.
const ROOT_FAILED: u8 = 4;
/// The exit status of a launch whose guest failed a mapping of the table, once the VM stopped
/// gracefully, with success and a complete teardown.
const TABLE_FAILED: u8 = 6;
/// The exit status of a removal that found leases of the generation, which keep it, tombstoned,
/// until their clones release them.
const BUSY: u8 = 7;
/// The exit status of a command whose clone failed, as NVX attributes and records the failure,
/// once nothing of the clone runs: the failure's teardown is resolved, and the command released
/// the clone's lease.
const CLONE_FAILED: u8 = 8;
/// The exit status of a command whose clone failed, once the failure's teardown is unresolved: a
/// VM of the clone may still run, or NVX could not record the failure before it ended the VM, and
/// the sandbox keeps the clone's lease, and NVX's records of its launch, for a later reattach.
const CLONE_UNRESOLVED: u8 = 9;
/// The exit status of a start whose template store refused its selection: the store does not
/// exist, or does not hold the generation, or the generation is expired or ineligible at the
/// host's time, so that the start leased and launched nothing.
const SELECTION_REFUSED: u8 = 10;
// The timers and limits of `exec` below are values chosen for tests that make a valid
// configuration, within the limits that the guest and the host allow: chunks of at most
// `exec::MAX_CHUNK` bytes, at most `worker::MAX_EXECUTION_MEMORY` bytes held in the guest for
// each execution, and output windows of at most `host::MAX_OUTPUT_FRAMES` frames.
/// How long an execution of `exec` may run before its sides end it.
const EXEC_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the pending output of an execution of `exec` still flows once a side ended it.
const EXEC_GRACE: Duration = Duration::from_secs(1);
/// How long an execution of `exec` may take to settle, once its program or a side ended it.
const EXEC_SETTLEMENT: Duration = Duration::from_secs(5);
/// The most bytes of a chunk of an execution of `exec`.
const EXEC_CHUNK: u32 = 16 * 1024;
/// The chunks of an execution's input that the guest may hold for `exec`.
const EXEC_INPUT_FRAMES: u32 = 4;
/// The chunks of an execution's output, of both streams, that `exec` may hold.
const EXEC_OUTPUT_FRAMES: u32 = 8;
/// The argument that ends an execution's arguments, after which another execution may follow.
const EXEC_END: &str = ";";
/// The options of an execution, which `run` takes among those of a start.
const EXECUTION_OPTIONS: [&str; 6] = [
    "--identity",
    "--groups",
    "--capabilities",
    "--numeric",
    "--cwd",
    "--env",
];
/// The command lines that the program accepts.
const USAGE: &str = concat!(
    "usage: runtime-adapter-host\n",
    "       runtime-adapter-host stage STORE --distro FORMAT:PATH --distro-identity UUID",
    " [--upper scratch:PATH]\n",
    "       runtime-adapter-host launch STORE --guest-agent PATH [--hypervisor NAME]",
    " [--guest-build BUILD]\n",
    "           [--distro FORMAT:PATH --distro-identity UUID",
    " --upper (scratch:PATH | ram) [--runtime-dir DIR]\n",
    "            [--overlay-mode MODE] [--scratch-journal POLICY]",
    " [--first-partition-bound BYTES]\n",
    "            [--workload-identity UID:GID] [--network FILE]\n",
    "            [--share HOST:TARGET[:ro]... [--deny PATH]... [--allow PATH]...\n",
    "             [--reserved TARGET]... [--share-root DIR]...]]\n",
    "       runtime-adapter-host (create | refresh) STORE --guest-agent PATH --distro FORMAT:PATH",
    " --distro-identity UUID\n",
    "           [--hypervisor NAME] [--guest-build BUILD] [--cpu-profile PROFILE]",
    " [--processors N]\n",
    "           [--boot-processors N] [--memory MIB] [--memory-capacity MIB]",
    " [--scratch-length BYTES]\n",
    "           [--network FILE]\n",
    "       runtime-adapter-host list STORE\n",
    "       runtime-adapter-host (inspect | remove) STORE GENERATION\n",
    "       runtime-adapter-host (reattach | stop) STORE [--guest-build BUILD]",
    " [--template TEMPLATES]\n",
    "       runtime-adapter-host start STORE --template TEMPLATES --generation G",
    " --distro FORMAT:PATH\n",
    "           --distro-identity UUID --upper (scratch:PATH | ram) [--guest-build BUILD]",
    " [--processors N]\n",
    "           [--memory MIB] [--workload-identity UID:GID] [--overlay-mode MODE]",
    " [--scratch-journal POLICY]\n",
    "           [--first-partition-bound BYTES] [--machine-id (image | generated)]",
    " [--network FILE]\n",
    "           [--share HOST:TARGET[:ro]... [--deny PATH]... [--allow PATH]...\n",
    "            [--reserved TARGET]... [--share-root DIR]...]\n",
    "       runtime-adapter-host run STORE --template TEMPLATES --generation G",
    " --distro FORMAT:PATH\n",
    "           --distro-identity UUID --upper (scratch:PATH | ram) [--guest-build BUILD]",
    " [--processors N]\n",
    "           [--memory MIB] [--workload-identity UID:GID] [--overlay-mode MODE]",
    " [--scratch-journal POLICY]\n",
    "           [--first-partition-bound BYTES] [--machine-id (image | generated)]",
    " [--network FILE]\n",
    "           [--share HOST:TARGET[:ro]... [--deny PATH]... [--allow PATH]...\n",
    "            [--reserved TARGET]... [--share-root DIR]...] EXECUTION\n",
    "       runtime-adapter-host probe STORE PROBE... [--guest-build BUILD]\n",
    "       runtime-adapter-host exec STORE [--guest-build BUILD] EXECUTION [';' EXECUTION]...\n",
    "           EXECUTION: [--identity UID:GID [--groups GID,...] [--capabilities N,...]",
    " [--numeric]]\n",
    "                      [--cwd DIR] [--env NAME=VALUE]... -- PROGRAM [ARG]...",
);

/// What the program was asked to do.
enum Command {
    /// Report the contract that the program was built against.
    Report,
    /// Stage the images of a layered root.
    Stage {
        /// The sandbox's store.
        store: PathBuf,
        /// The image.
        distro: PathBuf,
        /// The image's format.
        format: ImageFormat,
        /// The identity that the guest expects of the image.
        identity: Uuid,
        /// The ext4 scratch image, if any.
        scratch: Option<PathBuf>,
    },
    /// Launch a VM of the guest agent.
    Launch {
        /// The sandbox's store.
        store: PathBuf,
        /// The guest agent's executable.
        guest_agent: PathBuf,
        /// The hypervisor, unless `nvx::Hypervisor::from_env_or_default` selects it.
        hypervisor: Option<Hypervisor>,
        /// The guest agent's build.
        guest_build: String,
        /// The VM's layered root, if it has one.
        layered: Option<Box<Layered>>,
        /// The step hook, for tests, if `RUNTIME_ADAPTER_STEP_HOOK` sets one.
        hook: Option<StepHook>,
    },
    /// Serve a template from a template store, which captures one if it holds none, or which a
    /// refresh captures anyway.
    Create {
        /// The template store.
        store: PathBuf,
        /// The guest agent's executable.
        guest_agent: PathBuf,
        /// The hypervisor, unless `nvx::Hypervisor::from_env_or_default` selects it.
        hypervisor: Option<Hypervisor>,
        /// The guest agent's build.
        guest_build: String,
        /// The template.
        template: Box<Template>,
        /// Whether a generation that the store holds may serve the template, or a capture must.
        serving: Serving,
    },
    /// Report the generations of a template store.
    List {
        /// The template store.
        store: PathBuf,
    },
    /// Report a generation of a template store.
    Inspect {
        /// The template store.
        store: PathBuf,
        /// The generation's store generation.
        generation: StoreGeneration,
    },
    /// Retire a generation of a template store.
    Remove {
        /// The template store.
        store: PathBuf,
        /// The generation's store generation.
        generation: StoreGeneration,
    },
    /// Reattach to the VM, and stop it if asked.
    Reattach {
        /// The sandbox's store.
        store: PathBuf,
        /// The guest agent's build.
        guest_build: String,
        /// Whether to stop the VM.
        stop: bool,
        /// The template store through which to release a clone's lease, in place of the one that
        /// the clone's record names.
        template: Option<PathBuf>,
        /// The step hook, for tests, if `RUNTIME_ADAPTER_STEP_HOOK` sets one.
        hook: Option<StepHook>,
    },
    /// Start a clone of a generation of a template.
    Start {
        /// The sandbox's store.
        store: PathBuf,
        /// The guest agent's build.
        guest_build: String,
        /// The clone.
        request: Box<CloneOptions>,
        /// The host's time, for tests, if `RUNTIME_ADAPTER_HOST_TIME` sets one.
        time: Option<HostTime>,
        /// The step hook, for tests, if `RUNTIME_ADAPTER_STEP_HOOK` sets one, at `checked`.
        hook: Option<StepHook>,
    },
    /// Start a clone of a generation of a template, run one execution in its root, and tear it
    /// down.
    Run {
        /// The sandbox's store.
        store: PathBuf,
        /// The guest agent's build.
        guest_build: String,
        /// The clone.
        request: Box<CloneOptions>,
        /// The execution.
        execution: Box<Execution>,
        /// The host's time, for tests, if `RUNTIME_ADAPTER_HOST_TIME` sets one.
        time: Option<HostTime>,
    },
    /// Reattach to the VM, and run probes in its layered root.
    Probe {
        /// The sandbox's store.
        store: PathBuf,
        /// The guest agent's build.
        guest_build: String,
        /// The probes, in order.
        probes: Vec<Probe>,
    },
    /// Reattach to the VM, and run executions in its layered root.
    Exec {
        /// The sandbox's store.
        store: PathBuf,
        /// The guest agent's build.
        guest_build: String,
        /// The executions, in order.
        executions: Vec<Execution>,
    },
}

/// An execution of `exec` or `run`, as its options and arguments give it.
#[derive(Default)]
struct Execution {
    /// `--identity`: the user ID and the group ID.
    identity: Option<(u32, u32)>,
    /// `--groups`.
    groups: Option<Vec<u32>>,
    /// `--capabilities`: the set, a bit for each number.
    capabilities: Option<u64>,
    /// `--numeric`.
    numeric: bool,
    /// `--cwd`.
    working_directory: Option<String>,
    /// `--env`, in order.
    environment: Vec<String>,
    /// The program's path, then its arguments.
    arguments: Vec<String>,
}

impl Execution {
    /// Returns the start of the execution, as the execution `key`.
    fn start(&self, key: Key) -> Start {
        let arguments = self.arguments.iter().map(String::as_str);
        let mut start = Start::new(key, exec_limits(), exec_timers(), arguments)
            .with_environment(self.environment.iter().map(String::as_str));
        start.identity = self.identity.map(|(uid, gid)| exec::Identity {
            groups: self.groups.clone().unwrap_or_default(),
            capabilities: self.capabilities.unwrap_or(0),
            account: if self.numeric {
                AccountMode::Numeric
            } else {
                AccountMode::Existing
            },
            ..exec::Identity::new(uid, gid)
        });
        start.working_directory = self.working_directory.clone().map(String::into_bytes);
        start
    }
}

/// Checks that the guest takes `start`: that it decodes it, as it decodes an execution's start,
/// and that the framing carries it. The guest ends its session on a start that it cannot decode.
fn check_start(start: &Start) -> Result<(), String> {
    let encoded = start.encode();
    Start::decode(&encoded).map_err(|error| error.problem().to_owned())?;
    Message::ExecStart(encoded)
        .encode()
        .map_err(|error| format!("the start of an execution is too long: {error}"))?;
    Ok(())
}

/// Returns the limits of an execution of `exec`: chunks of [`EXEC_CHUNK`] bytes, and frame-counted
/// credit, of [`EXEC_INPUT_FRAMES`] for the input, and of [`EXEC_OUTPUT_FRAMES`] in one pool that
/// standard output and error share.
fn exec_limits() -> Limits {
    Limits::new(
        EXEC_CHUNK,
        Window::frames(EXEC_INPUT_FRAMES),
        Window::frames(EXEC_OUTPUT_FRAMES),
    )
    .expect("the limits of `exec` hold no empty window, and a chunk that the framing carries")
    .shared_output()
}

/// Returns the timers of both sides of an execution of `exec`.
fn exec_timers() -> Timers {
    Timers::new(EXEC_GRACE, EXEC_SETTLEMENT).timeout(EXEC_TIMEOUT)
}

/// The layered root of a launch, as its options give it.
struct Layered {
    /// The image.
    distro: PathBuf,
    /// The image's format.
    format: ImageFormat,
    /// The identity that the guest expects of the image.
    identity: Uuid,
    /// The ext4 scratch image of the upper, or `None` for an upper in RAM.
    scratch: Option<PathBuf>,
    /// The guest's runtime directory, unless the default.
    runtime: Option<String>,
    /// The overlay's mode, unless the guest's default.
    overlay_mode: Option<OverlayMode>,
    /// Whether the scratch may have a journal, unless the guest's default.
    scratch_journal: Option<JournalPolicy>,
    /// The most bytes that the first partition of a GPT disk may hold, unless the guest's default.
    first_partition_bound: Option<u64>,
    /// The workload identity of the whole sandbox, if any: its user ID and its group ID.
    workload_identity: Option<(u32, u32)>,
    /// The host paths that the launch shares with the root.
    sharing: HostFsSharing,
    /// The file of the VM's network, if it has one.
    network: Option<PathBuf>,
}

/// The template of `create`, as its options give it.
struct Template {
    /// The placeholder.
    distro: PathBuf,
    /// The placeholder's format.
    format: ImageFormat,
    /// The identity that the guest would expect of the placeholder.
    identity: Uuid,
    /// The CPU profile.
    cpu_profile: String,
    /// The processors: the processor capacity.
    processors: u32,
    /// The processors online at boot.
    boot_processors: u32,
    /// The RAM, in bytes.
    memory: u64,
    /// The RAM capacity, in bytes.
    memory_capacity: u64,
    /// The length of the dormant scratch, in bytes.
    scratch_length: u64,
    /// The file of the template's network, if it has one.
    network: Option<PathBuf>,
}

/// How `create` and `refresh` serve a template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Serving {
    /// `create`: with a generation of the template's class that the store holds, and that is
    /// eligible at the host's time, or else with one that a capture makes.
    Create,
    /// `refresh`: with a generation that a capture makes, always.
    Refresh,
}

impl Serving {
    /// Serves the template that `capture` describes from `store`, as `self` says, waiting for the
    /// capture's slot and the store's lock until `deadline`, which bounds a capture too.
    fn serve(self, store: &Store, capture: &Capture, deadline: Instant) -> nvx::Result<Created> {
        match self {
            Self::Create => store.create_template(capture, HostTime::now, deadline),
            Self::Refresh => store
                .refresh_template(capture, deadline)
                .map(Created::Captured),
        }
    }
}

/// The clone of `start` or `run`, as its options give it.
struct CloneOptions {
    /// The template store.
    templates: PathBuf,
    /// The generation of the template store that the clone leases.
    generation: StoreGeneration,
    /// The clone's image, upper, memory target, and settings, without its network.
    request: runtime_adapter::clone::Request,
    /// The file of the clone's network, if it has one, which the start reads first, as a launch
    /// reads its network.
    network: Option<PathBuf>,
}

impl CloneOptions {
    /// Reads the clone's network, if it has one, as a launch reads its network, then plans the
    /// clone's layered root, with that network, and the export of the clone's shares, if it has
    /// any, as a launch plans its own, or says why it cannot: a network that the plan refuses,
    /// then an image, or shares, that it refuses. It needs nothing but the options, and the files
    /// that they name, and creates nothing. NVX's plan of the clone then requires the template's
    /// network of it.
    fn plan(&self) -> Result<PlannedClone, String> {
        let network = self
            .network
            .as_deref()
            .map(read_network)
            .transpose()
            .map_err(|error| describe(&error))?;
        let request = runtime_adapter::clone::Request {
            network,
            ..self.request.clone()
        };
        let plan = request.plan().map_err(|error| describe(&error))?;
        let export = request.export().map_err(|error| describe(&error))?;
        Ok(PlannedClone {
            request,
            plan,
            export,
        })
    }
}

/// The clone of `start` or `run`, as [`CloneOptions::plan`] planned it.
struct PlannedClone {
    /// The clone's image, upper, memory target, settings, and network.
    request: runtime_adapter::clone::Request,
    /// The adapter's plan of the clone's image over its upper, with its network.
    plan: SandboxPlan,
    /// The export of the host paths that the clone shares with its root, if any.
    export: Option<ExportPlan>,
}

/// The options of a command.
#[derive(Default)]
struct Options {
    /// `--guest-agent`.
    guest_agent: Option<PathBuf>,
    /// `--hypervisor`.
    hypervisor: Option<Hypervisor>,
    /// `--guest-build`.
    guest_build: Option<String>,
    /// `--template`.
    template: Option<PathBuf>,
    /// `--generation`.
    generation: Option<StoreGeneration>,
    /// `--distro`: the image's format and its file.
    distro: Option<(ImageFormat, PathBuf)>,
    /// `--distro-identity`.
    identity: Option<Uuid>,
    /// `--upper`: the scratch image, or `None` for RAM.
    upper: Option<Option<PathBuf>>,
    /// `--runtime-dir`.
    runtime: Option<String>,
    /// `--overlay-mode`.
    overlay_mode: Option<OverlayMode>,
    /// `--scratch-journal`.
    scratch_journal: Option<JournalPolicy>,
    /// `--first-partition-bound`.
    first_partition_bound: Option<u64>,
    /// `--machine-id`.
    machine_id: Option<MachineIdMode>,
    /// `--workload-identity`.
    workload_identity: Option<(u32, u32)>,
    /// `--share`, `--deny`, `--allow`, `--reserved`, and `--share-root`, in order.
    sharing: HostFsSharing,
    /// `--network`.
    network: Option<PathBuf>,
    /// `--cpu-profile`.
    cpu_profile: Option<String>,
    /// `--processors`.
    processors: Option<u32>,
    /// `--boot-processors`.
    boot_processors: Option<u32>,
    /// `--memory`, in bytes.
    memory: Option<u64>,
    /// `--memory-capacity`, in bytes.
    memory_capacity: Option<u64>,
    /// `--scratch-length`.
    scratch_length: Option<u64>,
}

fn main() -> ExitCode {
    let command = match parse(env::args_os().skip(1).collect()) {
        Ok(command) => command,
        Err(problem) => {
            eprintln!("runtime-adapter-host: {problem}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match command {
        Command::Report => {
            println!(
                "runtime-adapter-host: {}; supported host: {}",
                runtime_adapter::contract(),
                nvx::host::platform::SUPPORTED,
            );
            return ExitCode::SUCCESS;
        }
        Command::Stage {
            store,
            distro,
            format,
            identity,
            scratch,
        } => {
            stage(&store, &distro, format, identity, scratch.as_deref()).map(|()| ExitCode::SUCCESS)
        }
        Command::Launch {
            store,
            guest_agent,
            hypervisor,
            guest_build,
            layered,
            hook,
        } => launch(
            &store,
            &guest_agent,
            hypervisor,
            &guest_build,
            layered.as_deref(),
            hook.as_ref(),
        ),
        Command::Create {
            store,
            guest_agent,
            hypervisor,
            guest_build,
            template,
            serving,
        } => create(
            &store,
            &guest_agent,
            hypervisor,
            &guest_build,
            &template,
            serving,
        ),
        Command::List { store } => list(&store),
        Command::Inspect { store, generation } => inspect(&store, generation),
        Command::Remove { store, generation } => remove(&store, generation),
        Command::Reattach {
            store,
            guest_build,
            stop,
            template,
            hook,
        } => reattach(
            &store,
            &guest_build,
            stop,
            template.as_deref(),
            hook.as_ref(),
        ),
        Command::Start {
            store,
            guest_build,
            request,
            time,
            hook,
        } => start(&store, &guest_build, &request, time, hook.as_ref()),
        Command::Run {
            store,
            guest_build,
            request,
            execution,
            time,
        } => run(&store, &guest_build, &request, &execution, time),
        Command::Probe {
            store,
            guest_build,
            probes,
        } => probe(&store, &guest_build, &probes),
        Command::Exec {
            store,
            guest_build,
            executions,
        } => exec(&store, &guest_build, &executions),
    };
    result.unwrap_or_else(|problem| {
        eprintln!("runtime-adapter-host: {problem}");
        ExitCode::FAILURE
    })
}

/// Parses the program's arguments, with the step hook that `RUNTIME_ADAPTER_STEP_HOOK` sets for a
/// launch, a start, a reattach, or a stop, or says why they are not a command.
fn parse(arguments: Vec<OsString>) -> Result<Command, String> {
    let mut arguments = arguments.into_iter();
    let Some(name) = arguments.next() else {
        return Ok(Command::Report);
    };
    let name = match name.to_str() {
        Some(
            name @ ("stage" | "launch" | "create" | "refresh" | "list" | "inspect" | "remove"
            | "reattach" | "stop" | "start" | "run" | "probe" | "exec"),
        ) => name.to_owned(),
        _ => return Err(format!("unknown command {name:?}")),
    };
    let store = PathBuf::from(arguments.next().ok_or_else(|| match name.as_str() {
        "create" | "refresh" | "list" | "inspect" | "remove" => {
            format!("{name} needs a template store")
        }
        _ => format!("{name} needs a sandbox store"),
    })?);
    if name == "exec" {
        return parse_exec(store, arguments);
    }
    if name == "run" {
        return parse_run(store, arguments);
    }
    if matches!(name.as_str(), "list" | "inspect" | "remove") {
        return parse_generations(&name, store, arguments);
    }
    let mut options = Options::default();
    let mut probes = Vec::new();
    while let Some(argument) = arguments.next() {
        let Some(option) = argument.to_str().filter(|option| option.starts_with("--")) else {
            if name != "probe" {
                return Err(format!("unexpected argument {argument:?} of {name}"));
            }
            let text = argument
                .to_str()
                .ok_or_else(|| format!("the probe {argument:?} is not Unicode"))?;
            probes.push(Probe::parse(text).map_err(|error| error.problem().to_owned())?);
            continue;
        };
        let value = arguments
            .next()
            .ok_or_else(|| format!("{option:?} needs a value"))?;
        parse_option(&mut options, &name, option, value)?;
    }
    let guest_build = options
        .guest_build
        .take()
        .unwrap_or_else(|| GUEST_BUILD.to_owned());
    Ok(match name.as_str() {
        "stage" => {
            let (format, distro) = options.distro.ok_or("stage needs --distro")?;
            Command::Stage {
                store,
                distro,
                format,
                identity: options.identity.ok_or("stage needs --distro-identity")?,
                scratch: options.upper.flatten(),
            }
        }
        "launch" => {
            check_shares(&options.sharing)?;
            let layered = match (options.distro, options.identity, options.upper) {
                (Some(_), None, _) => return Err("--distro needs --distro-identity".to_owned()),
                (Some(_), Some(_), None) => return Err("--distro needs --upper".to_owned()),
                (Some(_), Some(_), Some(None)) if options.scratch_journal.is_some() => {
                    return Err("--scratch-journal needs --upper scratch:PATH".to_owned());
                }
                (Some((ImageFormat::Erofs, _)), Some(_), Some(_))
                    if options.first_partition_bound.is_some() =>
                {
                    return Err("--first-partition-bound needs --distro gpt:PATH".to_owned());
                }
                (Some((format, distro)), Some(identity), Some(scratch)) => {
                    Some(Box::new(Layered {
                        distro,
                        format,
                        identity,
                        scratch,
                        runtime: options.runtime,
                        overlay_mode: options.overlay_mode,
                        scratch_journal: options.scratch_journal,
                        first_partition_bound: options.first_partition_bound,
                        workload_identity: options.workload_identity,
                        sharing: options.sharing,
                        network: options.network,
                    }))
                }
                (None, Some(_), _) => {
                    return Err("--distro-identity needs --distro".to_owned());
                }
                (None, None, Some(_)) => return Err("--upper needs --distro".to_owned()),
                (None, None, None) => {
                    let layered = [
                        ("--runtime-dir", options.runtime.is_some()),
                        ("--overlay-mode", options.overlay_mode.is_some()),
                        ("--scratch-journal", options.scratch_journal.is_some()),
                        (
                            "--first-partition-bound",
                            options.first_partition_bound.is_some(),
                        ),
                        ("--workload-identity", options.workload_identity.is_some()),
                        ("--share", !options.sharing.shares().is_empty()),
                        ("--network", options.network.is_some()),
                    ];
                    if let Some((option, _)) = layered.iter().find(|(_, given)| *given) {
                        return Err(format!("{option} needs --distro"));
                    }
                    None
                }
            };
            Command::Launch {
                store,
                guest_agent: options.guest_agent.ok_or("launch needs --guest-agent")?,
                hypervisor: options.hypervisor,
                guest_build,
                layered,
                hook: StepHook::from_env()?,
            }
        }
        "create" | "refresh" => {
            let guest_agent = options
                .guest_agent
                .ok_or_else(|| format!("{name} needs --guest-agent"))?;
            let (format, distro) = options
                .distro
                .ok_or_else(|| format!("{name} needs --distro"))?;
            let identity = options
                .identity
                .ok_or_else(|| format!("{name} needs --distro-identity"))?;
            let memory = options
                .memory
                .unwrap_or(u64::from(DEFAULT_MEMORY_MIB) * MIB);
            Command::Create {
                store,
                guest_agent,
                hypervisor: options.hypervisor,
                guest_build,
                template: Box::new(Template {
                    distro,
                    format,
                    identity,
                    cpu_profile: options
                        .cpu_profile
                        .unwrap_or_else(|| DEFAULT_CPU_PROFILE.to_owned()),
                    processors: options.processors.unwrap_or(DEFAULT_PROCESSORS),
                    boot_processors: options.boot_processors.unwrap_or(DEFAULT_BOOT_PROCESSORS),
                    memory,
                    memory_capacity: options.memory_capacity.unwrap_or(memory),
                    scratch_length: options.scratch_length.unwrap_or(DEFAULT_SCRATCH_LENGTH),
                    network: options.network,
                }),
                serving: if name == "refresh" {
                    Serving::Refresh
                } else {
                    Serving::Create
                },
            }
        }
        "start" => Command::Start {
            store,
            guest_build,
            request: Box::new(clone_options(&name, options)?),
            time: host_time_from_env()?,
            hook: start_hook()?,
        },
        "probe" if probes.is_empty() => return Err("probe needs a probe".to_owned()),
        "probe" => Command::Probe {
            store,
            guest_build,
            probes,
        },
        stop => Command::Reattach {
            store,
            guest_build,
            stop: stop == "stop",
            template: options.template,
            hook: StepHook::from_env()?,
        },
    })
}

/// Parses `option` of the command `name`, with `value`, into `options`, or says why it is not one
/// of the command's options, or why its value is invalid.
fn parse_option(
    options: &mut Options,
    name: &str,
    option: &str,
    value: OsString,
) -> Result<(), String> {
    let text = |value: OsString| {
        value
            .into_string()
            .map_err(|value| format!("the value {value:?} of {option} is not Unicode"))
    };
    // A refresh takes the options of a creation, and a run those of a start.
    let command = match name {
        "refresh" => "create",
        "run" => "start",
        name => name,
    };
    match (command, option) {
        ("launch" | "create", "--guest-agent") if options.guest_agent.is_none() => {
            options.guest_agent = Some(PathBuf::from(value));
        }
        ("launch" | "create", "--hypervisor") if options.hypervisor.is_none() => {
            let hypervisor = text(value)?.parse().map_err(|error| describe(&error))?;
            options.hypervisor = Some(hypervisor);
        }
        ("launch" | "stage" | "create" | "start", "--distro") if options.distro.is_none() => {
            let value = text(value)?;
            let distro = value.split_once(':').and_then(|(format, path)| {
                let format = ImageFormat::from_name(format)?;
                (!path.is_empty()).then(|| (format, PathBuf::from(path)))
            });
            options.distro = Some(distro.ok_or_else(|| {
                format!("--distro takes FORMAT:PATH, with the format erofs or gpt, not {value:?}")
            })?);
        }
        ("launch" | "stage" | "create" | "start", "--distro-identity")
            if options.identity.is_none() =>
        {
            let value = text(value)?;
            let identity = Uuid::parse(&value)
                .ok_or_else(|| format!("--distro-identity takes a UUID, not {value:?}"))?;
            options.identity = Some(identity);
        }
        ("launch" | "stage" | "start", "--upper") if options.upper.is_none() => {
            let value = text(value)?;
            // Staging binds a scratch image; only a launch or a clone puts the upper in RAM.
            let upper = match value.split_once(':') {
                _ if value == "ram" && name != "stage" => Some(None),
                Some(("scratch", path)) if !path.is_empty() => Some(Some(PathBuf::from(path))),
                _ => None,
            };
            options.upper = Some(upper.ok_or_else(|| match name {
                "stage" => format!("--upper of stage takes scratch:PATH, not {value:?}"),
                _ => format!("--upper takes scratch:PATH or ram, not {value:?}"),
            })?);
        }
        ("launch", "--runtime-dir") if options.runtime.is_none() => {
            options.runtime = Some(text(value)?);
        }
        ("launch" | "start", "--overlay-mode") if options.overlay_mode.is_none() => {
            let value = text(value)?;
            let mode = OverlayMode::from_name(&value).ok_or_else(|| {
                format!("--overlay-mode takes trusted-lowers or untrusted-lowers, not {value:?}")
            })?;
            options.overlay_mode = Some(mode);
        }
        ("launch" | "start", "--scratch-journal") if options.scratch_journal.is_none() => {
            let value = text(value)?;
            let policy = JournalPolicy::from_name(&value).ok_or_else(|| {
                format!("--scratch-journal takes allowed or forbidden, not {value:?}")
            })?;
            options.scratch_journal = Some(policy);
        }
        ("launch" | "start", "--first-partition-bound")
            if options.first_partition_bound.is_none() =>
        {
            let value = text(value)?;
            let bound = value.parse::<u64>().ok().filter(|bound| *bound > 0);
            options.first_partition_bound = Some(bound.ok_or_else(|| {
                format!("--first-partition-bound takes a positive number of bytes, not {value:?}")
            })?);
        }
        ("start", "--machine-id") if options.machine_id.is_none() => {
            let value = text(value)?;
            let mode = match value.as_str() {
                "image" => MachineIdMode::Image,
                "generated" => MachineIdMode::Generated,
                _ => {
                    return Err(format!(
                        "--machine-id takes image or generated, not {value:?}"
                    ));
                }
            };
            options.machine_id = Some(mode);
        }
        ("launch" | "start", "--workload-identity") if options.workload_identity.is_none() => {
            let value = text(value)?;
            let identity = ids(&value)
                .filter(|(uid, gid)| [uid, gid].into_iter().all(|id| (1..u32::MAX).contains(id)));
            options.workload_identity = Some(identity.ok_or_else(|| {
                format!(
                    "--workload-identity takes UID:GID, two numbers from 1 to {}, not {value:?}",
                    u32::MAX - 1
                )
            })?);
        }
        ("launch" | "start", "--share") => {
            let value = text(value)?;
            let share = Share::parse(&value).ok_or_else(|| {
                format!(
                    "--share takes HOST:TARGET or HOST:TARGET:ro, with an absolute guest path \
                     TARGET, not {value:?}"
                )
            })?;
            options.sharing.add_share(share);
        }
        ("launch" | "start", "--deny") => options.sharing.add_denied(value),
        ("launch" | "start", "--allow") => options.sharing.add_allowed(value),
        ("launch" | "start", "--reserved") => options.sharing.add_reserved(text(value)?),
        ("launch" | "start", "--share-root") => options.sharing.add_root(value),
        ("launch" | "create" | "start", "--network") if options.network.is_none() => {
            options.network = Some(PathBuf::from(value));
        }
        ("create", "--cpu-profile") if options.cpu_profile.is_none() => {
            options.cpu_profile = Some(text(value)?);
        }
        ("create" | "start", "--processors") if options.processors.is_none() => {
            options.processors = Some(count(option, &text(value)?)?);
        }
        ("create", "--boot-processors") if options.boot_processors.is_none() => {
            options.boot_processors = Some(count(option, &text(value)?)?);
        }
        ("create" | "start", "--memory") if options.memory.is_none() => {
            options.memory = Some(mebibytes(option, &text(value)?)?);
        }
        ("start" | "reattach" | "stop", "--template") if options.template.is_none() => {
            options.template = Some(PathBuf::from(value));
        }
        ("start", "--generation") if options.generation.is_none() => {
            let value = text(value)?;
            let generation = value
                .parse::<NonZeroU64>()
                .ok()
                .filter(|_| value.bytes().all(|byte| byte.is_ascii_digit()));
            options.generation =
                Some(StoreGeneration::new(generation.ok_or_else(|| {
                    format!("--generation takes a positive number, not {value:?}")
                })?));
        }
        ("create", "--memory-capacity") if options.memory_capacity.is_none() => {
            options.memory_capacity = Some(mebibytes(option, &text(value)?)?);
        }
        ("create", "--scratch-length") if options.scratch_length.is_none() => {
            let value = text(value)?;
            let length = value.parse::<u64>().ok().filter(|length| *length > 0);
            options.scratch_length = Some(length.ok_or_else(|| {
                format!("--scratch-length takes a positive number of bytes, not {value:?}")
            })?);
        }
        (_, "--guest-build") if name != "stage" && options.guest_build.is_none() => {
            options.guest_build = Some(text(value)?);
        }
        _ => return Err(format!("unexpected option {option:?} of {name}")),
    }
    Ok(())
}

/// Says why the options of `sharing`, of a launch, a start, or a run, are not a command's:
/// `--deny`, `--allow`, `--reserved`, or `--share-root` without a `--share`.
fn check_shares(sharing: &HostFsSharing) -> Result<(), String> {
    let options = [
        ("--deny", !sharing.denied().is_empty()),
        ("--allow", !sharing.allowed().is_empty()),
        ("--reserved", !sharing.reserved().is_empty()),
        ("--share-root", !sharing.roots().is_empty()),
    ];
    if sharing.shares().is_empty()
        && let Some((option, _)) = options.iter().find(|(_, given)| *given)
    {
        return Err(format!("{option} needs --share"));
    }
    Ok(())
}

/// Returns the step hook that `RUNTIME_ADAPTER_STEP_HOOK` sets for a start, if any, or says why it
/// sets none: a start takes it at `checked` alone, once NVX's `ClonePlan` checked the clone.
fn start_hook() -> Result<Option<StepHook>, String> {
    match StepHook::from_env()? {
        Some(hook) if hook.step != Step::Checked => Err(format!(
            "{STEP_HOOK} names the step {}, but a start takes the hook at {} alone",
            hook.step,
            Step::Checked
        )),
        hook => Ok(hook),
    }
}

/// Parses the arguments of `exec` after its sandbox store `store`: its executions, each ended by
/// a lone `;` or the end of the arguments, with the options before them, or says why they are not
/// the command's.
fn parse_exec(
    store: PathBuf,
    arguments: impl Iterator<Item = OsString>,
) -> Result<Command, String> {
    let mut arguments = arguments.map(|argument| {
        argument
            .into_string()
            .map_err(|argument| format!("the argument {argument:?} of exec is not Unicode"))
    });
    let mut guest_build = None;
    let mut executions = Vec::new();
    loop {
        let mut execution = Execution::default();
        loop {
            let option = arguments
                .next()
                .ok_or("exec needs `-- PROGRAM` after the options of each execution")??;
            match option.as_str() {
                "--" => break,
                "--numeric" if !execution.numeric => {
                    execution.numeric = true;
                    continue;
                }
                _ => {}
            }
            let value = arguments
                .next()
                .ok_or_else(|| format!("{option:?} needs a value"))??;
            match option.as_str() {
                "--guest-build" if guest_build.is_none() => guest_build = Some(value),
                _ => execution_option(&mut execution, "exec", &option, value)?,
            }
        }
        let mut more = false;
        for argument in arguments.by_ref() {
            let argument = argument?;
            if argument == EXEC_END {
                more = true;
                break;
            }
            execution.arguments.push(argument);
        }
        check_execution(&execution, "exec")?;
        executions.push(execution);
        if !more {
            break;
        }
    }
    // The driver runs no more executions at once than the guest does.
    if executions.len() > exec::MAX_EXECUTIONS {
        return Err(format!(
            "exec runs at most {} executions, not {}",
            exec::MAX_EXECUTIONS,
            executions.len()
        ));
    }
    Ok(Command::Exec {
        store,
        guest_build: guest_build.unwrap_or_else(|| GUEST_BUILD.to_owned()),
        executions,
    })
}

/// Parses the option `option` of an execution of the command `name`, `exec` or `run`, with
/// `value`, into `execution`, or says why it is not one of an execution's options that take a
/// value, or why its value is invalid.
fn execution_option(
    execution: &mut Execution,
    name: &str,
    option: &str,
    value: String,
) -> Result<(), String> {
    match option {
        "--identity" if execution.identity.is_none() => {
            let identity =
                ids(&value).ok_or_else(|| format!("--identity takes UID:GID, not {value:?}"))?;
            execution.identity = Some(identity);
        }
        "--groups" if execution.groups.is_none() => {
            let groups = numbers(&value, |group| group.parse::<u32>().ok()).ok_or_else(|| {
                format!("--groups takes GIDs, separated by commas, not {value:?}")
            })?;
            execution.groups = Some(groups);
        }
        "--capabilities" if execution.capabilities.is_none() => {
            let capabilities = numbers(&value, |number| {
                number.parse::<u8>().ok().filter(|number| *number < 64)
            })
            .ok_or_else(|| {
                format!(
                    "--capabilities takes capability numbers from 0 to 63, separated by commas, \
                     not {value:?}"
                )
            })?;
            let set = capabilities
                .into_iter()
                .fold(0, |set, number| set | 1 << number);
            execution.capabilities = Some(set);
        }
        "--cwd" if execution.working_directory.is_none() => {
            execution.working_directory = Some(value);
        }
        "--env" => execution.environment.push(value),
        _ => return Err(format!("unexpected option {option:?} of {name}")),
    }
    Ok(())
}

/// Checks the execution `execution` of the command `name`, once its arguments are parsed: that it
/// names a program, that it names an identity if it qualifies one, and that the guest takes its
/// start, or says why not.
fn check_execution(execution: &Execution, name: &str) -> Result<(), String> {
    if execution.arguments.is_empty() {
        return Err(format!("{name} needs a program after `--`"));
    }
    if execution.identity.is_none() {
        let qualifiers = [
            ("--groups", execution.groups.is_some()),
            ("--capabilities", execution.capabilities.is_some()),
            ("--numeric", execution.numeric),
        ];
        if let Some((option, _)) = qualifiers.iter().find(|(_, given)| *given) {
            return Err(format!("{option} needs --identity"));
        }
    }
    // The guest ends its session on a start that it cannot decode, so the program refuses one
    // that breaks the rules of the encoding, as a relative working directory or an environment
    // entry without a name does, or that the framing does not carry, before it attaches.
    check_start(&execution.start(Key::new(0, 0)))
}

/// Parses the arguments of `run` after its sandbox store `store`: the options of `start`, and
/// those of one execution, as `exec` takes them, in any order, then `--`, and the execution's
/// program and its arguments, every one that follows, or says why they are not the command's.
fn parse_run(
    store: PathBuf,
    mut arguments: impl Iterator<Item = OsString>,
) -> Result<Command, String> {
    let mut options = Options::default();
    let mut execution = Execution::default();
    loop {
        let argument = arguments
            .next()
            .ok_or("run needs `-- PROGRAM` after its options")?;
        let Some(option) = argument.to_str().filter(|option| option.starts_with("--")) else {
            return Err(format!("unexpected argument {argument:?} of run"));
        };
        match option {
            "--" => break,
            "--numeric" if !execution.numeric => {
                execution.numeric = true;
                continue;
            }
            _ => {}
        }
        let option = option.to_owned();
        let value = arguments
            .next()
            .ok_or_else(|| format!("{option:?} needs a value"))?;
        if EXECUTION_OPTIONS.contains(&option.as_str()) {
            let value = value
                .into_string()
                .map_err(|value| format!("the value {value:?} of {option} is not Unicode"))?;
            execution_option(&mut execution, "run", &option, value)?;
        } else {
            parse_option(&mut options, "run", &option, value)?;
        }
    }
    for argument in arguments {
        let argument = argument
            .into_string()
            .map_err(|argument| format!("the argument {argument:?} of run is not Unicode"))?;
        execution.arguments.push(argument);
    }
    check_execution(&execution, "run")?;
    let guest_build = options
        .guest_build
        .take()
        .unwrap_or_else(|| GUEST_BUILD.to_owned());
    Ok(Command::Run {
        store,
        guest_build,
        request: Box::new(clone_options("run", options)?),
        execution: Box::new(execution),
        time: host_time_from_env()?,
    })
}

/// Returns the clone that the options `options` of `start` or `run`, the command `name`, select
/// and describe, or says why they do not.
fn clone_options(name: &str, options: Options) -> Result<CloneOptions, String> {
    let templates = options
        .template
        .ok_or_else(|| format!("{name} needs --template"))?;
    let generation = options
        .generation
        .ok_or_else(|| format!("{name} needs --generation"))?;
    let (format, distro) = options
        .distro
        .ok_or_else(|| format!("{name} needs --distro"))?;
    let identity = options
        .identity
        .ok_or_else(|| format!("{name} needs --distro-identity"))?;
    let scratch = options
        .upper
        .ok_or_else(|| format!("{name} needs --upper"))?;
    if scratch.is_none() && options.scratch_journal.is_some() {
        return Err("--scratch-journal needs --upper scratch:PATH".to_owned());
    }
    if format == ImageFormat::Erofs && options.first_partition_bound.is_some() {
        return Err("--first-partition-bound needs --distro gpt:PATH".to_owned());
    }
    check_shares(&options.sharing)?;
    Ok(CloneOptions {
        templates,
        generation,
        request: runtime_adapter::clone::Request {
            distro,
            format,
            identity,
            scratch,
            processors: options.processors,
            memory: options.memory,
            workload_identity: options.workload_identity,
            overlay_mode: options.overlay_mode,
            scratch_journal: options.scratch_journal,
            first_partition_bound: options.first_partition_bound,
            machine_id: options.machine_id.unwrap_or(MachineIdMode::Image),
            sharing: options.sharing,
            network: None,
        },
        network: options.network,
    })
}

/// Parses the arguments of `list`, `inspect`, or `remove`, the command `name`, after its template
/// store `store`: none for `list`, and the store generation of a generation, a positive number,
/// for `inspect` and `remove`, or says why they are not the command's.
fn parse_generations(
    name: &str,
    store: PathBuf,
    mut arguments: impl Iterator<Item = OsString>,
) -> Result<Command, String> {
    let unexpected = |argument: &OsString| match argument.to_str() {
        Some(option) if option.starts_with("--") => {
            format!("unexpected option {option:?} of {name}")
        }
        _ => format!("unexpected argument {argument:?} of {name}"),
    };
    let generation = match (name, arguments.next()) {
        ("list", None) => None,
        ("list", Some(argument)) => return Err(unexpected(&argument)),
        (_, None) => return Err(format!("{name} needs a generation")),
        (_, Some(argument)) if argument.to_str().is_some_and(|text| text.starts_with("--")) => {
            return Err(unexpected(&argument));
        }
        (_, Some(argument)) => {
            let generation = argument
                .to_str()
                .and_then(|text| text.parse::<NonZeroU64>().ok())
                .ok_or_else(|| {
                    format!("{name} takes a generation, a positive number, not {argument:?}")
                })?;
            Some(StoreGeneration::new(generation))
        }
    };
    if let Some(argument) = arguments.next() {
        return Err(unexpected(&argument));
    }
    Ok(match (name, generation) {
        ("inspect", Some(generation)) => Command::Inspect { store, generation },
        ("remove", Some(generation)) => Command::Remove { store, generation },
        _ => Command::List { store },
    })
}

/// Returns the user ID and the group ID of `value`, `UID:GID`, or `None` if it is not that.
fn ids(value: &str) -> Option<(u32, u32)> {
    let (uid, gid) = value.split_once(':')?;
    Some((uid.parse().ok()?, gid.parse().ok()?))
}

/// Returns the numbers of `value`, separated by commas, which `parse` reads, or `None` if one is
/// not a number that it accepts, or `value` is empty.
fn numbers<T>(value: &str, parse: impl Fn(&str) -> Option<T>) -> Option<Vec<T>> {
    value.split(',').map(parse).collect()
}

/// Returns the bytes of `value`, the value of `option`, a positive number of mebibytes, or says why
/// it is not one.
fn mebibytes(option: &str, value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|mebibytes| *mebibytes > 0)
        .and_then(|mebibytes| mebibytes.checked_mul(MIB))
        .ok_or_else(|| format!("{option} takes a positive number of MiB, not {value:?}"))
}

/// Returns `value`, the value of `option`, a positive number of processors, or says why it is not
/// one.
fn count(option: &str, value: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .ok()
        .filter(|count| *count > 0)
        .ok_or_else(|| format!("{option} takes a positive number, not {value:?}"))
}

/// Stages the image `distro`, in `format`, of which the guest expects `identity`, and the ext4
/// scratch image `scratch`, if any, in the sandbox store `store`, which it creates if needed.
fn stage(
    store: &Path,
    distro: &Path,
    format: ImageFormat,
    identity: Uuid,
    scratch: Option<&Path>,
) -> Result<(), String> {
    // Verification may hash the image, so it runs before the sandbox is locked.
    let staged =
        Staged::stage(distro, format, identity, scratch).map_err(|error| describe(&error))?;
    let store = SandboxStore::create(store).map_err(|error| describe(&error))?;
    let _locked = store
        .lock(Some(Instant::now() + LOCK_TIMEOUT))
        .map_err(|error| describe(&error))?;
    staged.write(store.dir()).map_err(|error| describe(&error))
}

/// Launches a VM of the guest agent `guest_agent` in the sandbox store `store`, on `hypervisor`
/// or else the one that `nvx::Hypervisor::from_env_or_default` selects, with the guest agent's
/// build `guest_build`, and with the layered root `layered`, if any. The step hook `hook`, if any,
/// acts at its step of the launch.
fn launch(
    store: &Path,
    guest_agent: &Path,
    hypervisor: Option<Hypervisor>,
    guest_build: &str,
    layered: Option<&Layered>,
    hook: Option<&StepHook>,
) -> Result<ExitCode, String> {
    // A network that the plan refuses fails the launch before it needs NVX's artifacts or the
    // store.
    let network = layered
        .and_then(|layered| layered.network.as_deref())
        .map(read_network)
        .transpose()
        .map_err(|error| describe(&error))?;
    let hypervisor = match hypervisor {
        Some(hypervisor) => hypervisor,
        None => Hypervisor::from_env_or_default().map_err(|error| describe(&error))?,
    };
    let runtime = runtime(guest_build)?;
    runtime
        .artifacts
        .probe()
        .map_err(|error| describe(&error))?;
    let mut layered = layered
        .map(|layered| -> nvx::Result<LayeredLaunch> {
            let mut launch = LayeredLaunch::new(
                &layered.distro,
                layered.format,
                layered.identity,
                layered.scratch.as_deref(),
                layered.runtime.as_deref(),
            )?;
            launch.config.overlay_mode = layered.overlay_mode;
            launch.config.scratch_journal = layered.scratch_journal;
            launch.config.first_partition_bound = layered.first_partition_bound;
            launch.config.workload_identity = layered.workload_identity;
            // The launch refuses shares that it cannot export before it touches the store.
            if let Some(export) = layered.sharing.plan()? {
                launch.share(export)?;
            }
            Ok(launch.with_network(network))
        })
        .transpose()
        .map_err(|error| describe(&error))?;
    let store = SandboxStore::create(store).map_err(|error| describe(&error))?;
    let mut locked = store
        .lock(Some(Instant::now() + LOCK_TIMEOUT))
        .map_err(|error| describe(&error))?;
    // The launch checks the bindings that the store stages for its images before it spawns
    // anything. The prepared launch holds the files that the checks opened until the guest has
    // assembled the root, or until a launch that failed is torn down, which ends its VM first:
    // the launch, declared after it, drops first.
    let mut prepared = None;
    if let Some(layered) = &mut layered {
        if let Some(staged) = Staged::read(store.dir()).map_err(|error| describe(&error))? {
            layered.bindings = staged.bindings(&layered.plan);
        }
        prepared = Some(layered.prepare().map_err(|error| binding_error(&error))?);
    }
    step(hook, Step::Checked)?;
    let capability = generate_capability().map_err(|error| describe(&error))?;
    let initramfs = store.dir().join(INITRAMFS_NAME);
    let spec = LaunchSpec::new(
        Artifacts::new(
            &runtime.artifacts.openvmm,
            &runtime.artifacts.kernel,
            &initramfs,
        ),
        hypervisor,
    );
    let mut deadline = Instant::now() + START_TIMEOUT;
    let launch_id = LaunchId::generate().map_err(|error| describe(&error))?;
    let command = |endpoint: &str| -> nvx::Result<VmmCommand<'_>> {
        // The launch has checked that no VM of the sandbox runs, which could still read the
        // initramfs of an earlier launch.
        initramfs::write(&runtime.artifacts.initrd, guest_agent, &initramfs).map_err(|error| {
            nvx::Error::io("cannot write the initramfs of the guest agent").with_source(error)
        })?;
        let outcome = store.outcome_path();
        let (arguments, kernel_command_line) = match layered.as_ref().zip(prepared.as_ref()) {
            Some((layered, prepared)) => (
                layered.openvmm_arguments(
                    prepared,
                    hypervisor,
                    &spec.kernel,
                    &spec.initrd,
                    endpoint,
                    &outcome,
                ),
                prepared.kernel_command_line().to_owned(),
            ),
            None => (
                spec.openvmm_arguments(endpoint, &outcome)?,
                spec.kernel_command_line()?,
            ),
        };
        let record = LaunchRecord {
            kernel_command_line: &kernel_command_line,
            layered: layered.as_ref(),
        };
        record.write(store.dir())?;
        let mut command = VmmCommand::new(&spec.openvmm, arguments, store.dir());
        // OpenVMM inherits the checked files that the prepared launch passes, and nothing else.
        if let Some(prepared) = &prepared {
            command.inherit = prepared.inherited();
        }
        Ok(command)
    };
    let starting = supervisor::launch(
        &mut locked,
        &launch_id,
        &runtime.identity,
        &capability,
        command,
    )
    .map_err(|error| describe(&error))?;
    let process = starting.process().reference().clone();
    println!("pid={}", process.pid);
    // A launch that fails from here on ends its VM, once `starting` drops. Its deadline leaves out
    // the time that the step hook pauses it.
    deadline += step(hook, Step::Spawned)?;
    let failed = |problem: String| {
        format!(
            "{problem}; see the VMM's log {}",
            store.log_path().display()
        )
    };
    let transport = session::connect(
        starting.endpoint(),
        process.pid,
        process.start_time,
        deadline,
    )
    .map_err(|error| failed(format!("cannot connect to the VM: {}", describe(&error))))?;
    let mut client = attach(transport, &capability, deadline).map_err(failed)?;
    if let Some(layered) = &layered {
        match host::ready(&mut client, &layered.config, guest_build, deadline) {
            Ok(ready) => {
                println!("root=ready");
                println!("build={}", escape(ready.build()));
                // The guest has assembled the root, so OpenVMM holds the images: the checked files
                // can go, with OpenVMM's copies of them on Windows, which releases their guards,
                // while OpenVMM keeps the scratch image's claim.
                if let Some(prepared) = prepared.take() {
                    prepared.release(starting.process()).map_err(|error| {
                        failed(format!(
                            "cannot release the checked files: {}",
                            describe(&error)
                        ))
                    })?;
                }
                deadline += step(hook, Step::Ready)?;
            }
            Err(OrderError::Root(failure)) => {
                print_failure(&failure);
                drop(client);
                return launch_failed(starting, &store, &failure).map_err(failed);
            }
            // The host sends a guest agent of another build nothing more, and the VM ends once
            // `starting` drops.
            Err(OrderError::Build { reported, expected }) => {
                println!("root=ready");
                println!("build={}", escape(&reported));
                return Err(failed(format!(
                    "the guest agent is of build {reported}, not of the host's build {expected}"
                )));
            }
            Err(error) => {
                return Err(failed(format!(
                    "the guest agent did not answer the root configuration: {}",
                    describe(&error)
                )));
            }
        }
        let table = layered.table();
        match host::start_sandbox(&mut client, &table, deadline) {
            Ok(()) => {
                println!("table=applied");
                println!("mapped={}", table.len());
                println!("sandbox=started");
            }
            Err(OrderError::Mapping { failure, stop }) => {
                println!("table=failed");
                println!("mapped={}", failure.applied());
                println!("problem={}", escape(failure.problem()));
                drop(client);
                return table_failed(starting, &store, stop).map_err(failed);
            }
            Err(error) => {
                return Err(failed(format!(
                    "the guest agent did not start the sandbox: {}",
                    describe(&error)
                )));
            }
        }
    }
    client
        .ping(deadline)
        .and_then(|()| client.echo(&echo_body(), deadline))
        .map_err(|error| {
            failed(format!(
                "the guest agent did not answer: {}",
                describe(&error)
            ))
        })?;
    step(hook, Step::Commit)?;
    starting.commit().map_err(|error| describe(&error))?;
    Ok(ExitCode::SUCCESS)
}

/// Runs the step hook `hook`, if any, at the command's `step`, and returns how long it paused the
/// command there, which a launch leaves out of its deadline.
fn step(hook: Option<&StepHook>, step: Step) -> Result<Duration, String> {
    match hook {
        Some(hook) => hook
            .at(step)
            .map_err(|error| format!("the step hook cannot pause the command at {step}: {error}")),
        None => Ok(Duration::ZERO),
    }
}

/// Ends the launch `starting` of the sandbox `store`, whose guest reported `failure` and ends: it
/// waits for the VM to exit, abandons the launch, which terminates a VM that still runs, and
/// checks that the VM ended with the failed stage's status and a complete teardown.
fn launch_failed(
    starting: Starting<'_>,
    store: &SandboxStore,
    failure: &RootFailure,
) -> Result<ExitCode, String> {
    let exited = starting
        .process()
        .wait(Instant::now() + STOP_TIMEOUT)
        .map_err(|error| format!("cannot wait for the VM to exit: {error}"))?;
    starting.abandon().map_err(|error| describe(&error))?;
    println!("stopped={}", if exited { "graceful" } else { "forced" });
    if !exited {
        return Err("the VM did not exit once its guest reported the failure".to_owned());
    }
    check_outcome(store, "guest-exit", failure.stage().exit_status().into())?;
    Ok(ExitCode::from(ROOT_FAILED))
}

/// Ends the launch `starting` of the sandbox `store`, whose guest failed a mapping of the table,
/// and which the host asked to stop, as `stop` tells how the stop ended: once the guest agent
/// confirmed the stop, it waits for the VM to exit, then abandons the launch, which terminates a
/// VM that still runs, and checks that the VM stopped gracefully, with success and a complete
/// teardown. A guest agent that did not confirm the stop, or reported that the root's teardown
/// failed, fails the launch.
fn table_failed(
    starting: Starting<'_>,
    store: &SandboxStore,
    stop: Result<Option<RootFailure>, ClientError>,
) -> Result<ExitCode, String> {
    let exited = match &stop {
        Ok(_) => starting
            .process()
            .wait(Instant::now() + STOP_TIMEOUT)
            .map_err(|error| format!("cannot wait for the VM to exit: {error}"))?,
        Err(_) => false,
    };
    starting.abandon().map_err(|error| describe(&error))?;
    println!("stopped={}", if exited { "graceful" } else { "forced" });
    let failure = stop.map_err(|error| {
        format!(
            "the guest agent did not confirm the stop: {}",
            describe(&error)
        )
    })?;
    if !exited {
        return Err("the VM did not exit once the guest agent confirmed the stop".to_owned());
    }
    if let Some(failure) = failure {
        return Err(format!("once the mapping table failed, {failure}"));
    }
    check_outcome(store, "success", 0)?;
    Ok(ExitCode::from(TABLE_FAILED))
}

/// Prints the root's `failure`.
fn print_failure(failure: &RootFailure) {
    println!("root=failed");
    println!("stage={}", failure.stage());
    println!("problem={}", escape(failure.problem()));
}

/// Describes `error`, of a launch that could not check the bindings of its images or prepare their
/// handoff: a binding that the store does not stage, or that no longer holds, needs the images
/// staged again, while a scratch image that another sandbox claims, and a failure of the host, do
/// not.
fn binding_error(error: &nvx::Error) -> String {
    match error.kind() {
        ErrorKind::Unavailable => format!("{}; stage the images first", describe(error)),
        _ => describe(error),
    }
}

/// Returns the body of the launch's `Echo`: the longest that the framing allows, of every byte
/// value, which spans several of the session's records, each way.
fn echo_body() -> Vec<u8> {
    (0..=u8::MAX).cycle().take(MAX_BODY).collect()
}

/// Serves a template of the guest agent `guest_agent`, of the build `guest_build`, which
/// `template` describes, on `hypervisor`, or else the one that
/// `nvx::Hypervisor::from_env_or_default` selects, from the template store `store`, which it
/// creates if needed: for `create`, a generation of the template's class that the store holds,
/// and that is eligible at the host's time, or one that a capture makes, which the store
/// publishes, and for `refresh`, one that a capture makes, whatever the store holds, as `serving`
/// says. It then evicts the generations of the class that the one it served supersedes.
fn create(
    store: &Path,
    guest_agent: &Path,
    hypervisor: Option<Hypervisor>,
    guest_build: &str,
    template: &Template,
    serving: Serving,
) -> Result<ExitCode, String> {
    // A network that the plan refuses fails the creation before it needs NVX's artifacts or the
    // store.
    let network = template
        .network
        .as_deref()
        .map(read_network)
        .transpose()
        .map_err(|error| describe(&error))?;
    let hypervisor = match hypervisor {
        Some(hypervisor) => hypervisor,
        None => Hypervisor::from_env_or_default().map_err(|error| describe(&error))?,
    };
    let runtime = runtime(guest_build)?;
    runtime
        .artifacts
        .probe()
        .map_err(|error| describe(&error))?;
    let request = Request {
        distro: template.distro.clone(),
        format: template.format,
        identity: template.identity,
        hypervisor,
        cpu_profile: template.cpu_profile.clone(),
        processors: template.processors,
        boot_processors: template.boot_processors,
        memory: template.memory,
        memory_capacity: template.memory_capacity,
        scratch_length: template.scratch_length,
        network,
    };
    // The placeholder is staged as `stage` stages an image: its explicit verification may hash it,
    // which no capture does.
    let staged = Staged::stage(&request.distro, request.format, request.identity, None)
        .map_err(|error| describe(&error))?;
    // The VM boots the initramfs from a directory that only the current user may access, as a
    // launch's VM boots it from the sandbox's store, so that no one else may change it first.
    let directory = TemporaryDirectory::new(CAPTURE_DIRECTORY_PREFIX)?;
    let initramfs = directory.path().join(INITRAMFS_NAME);
    initramfs::write(&runtime.artifacts.initrd, guest_agent, &initramfs)
        .map_err(|error| format!("cannot write the initramfs of the guest agent: {error}"))?;
    // The capture creates the dormant scratch in the store, on the file system of its directory,
    // or, before the store exists, of its parent's.
    let store = absolute(store).map_err(|error| describe(&error))?;
    let scratch = match store.parent() {
        Some(parent) if !store.is_dir() => parent.to_owned(),
        _ => store.clone(),
    };
    let capture = request
        .capture(
            staged.distro.binding,
            &runtime,
            guest_build,
            &initramfs,
            &scratch,
        )
        .map_err(|error| describe(&error))?;
    // A class that no capture takes fails before the store is created.
    let class = capture.class.identity().map_err(|error| describe(&error))?;
    let store = Store::create(&store).map_err(|error| describe(&error))?;
    let created = serving
        .serve(&store, &capture, Instant::now() + CAPTURE_TIMEOUT)
        .map_err(|error| {
            // The store names the slot of a class by the first digits of its identity, and keeps
            // the log of the VMM of the class's latest capture there once the capture's launch
            // has ended, and in the capture's transaction until then.
            let slot = store
                .dir()
                .join(CAPTURES_NAME)
                .join(&class.to_string()[..SLOT_NAME_LEN]);
            let logs = [
                slot.join(TRANSACTION_NAME).join(LOG_NAME),
                slot.join(LOG_NAME),
            ];
            match logs.into_iter().find(|log| log.is_file()) {
                Some(log) => format!(
                    "{}; see the VMM's log of the class's latest capture {}",
                    describe(&error),
                    log.display()
                ),
                None => describe(&error),
            }
        })?;
    let how = match &created {
        Created::Existing(_) => "existing",
        Created::Captured(_) => "captured",
    };
    let generation = created.generation();
    println!("created={how}");
    println!("generation={}", generation.generation());
    println!("key={}", generation.key());
    println!("class={}", generation.class());
    // The store has committed the generation, which the eviction never undoes: one that fails
    // leaves the generations that it did not retire to the next eviction of the class.
    match generations::evict(&store, generation, Instant::now() + LOCK_TIMEOUT) {
        Ok(eviction) => {
            println!("retired={}", store_generations(&eviction.retired));
            println!("draining={}", store_generations(&eviction.draining));
        }
        Err(error) => eprintln!(
            "runtime-adapter-host: the store serves the generation {}, but cannot retire the \
             generations of its class that it supersedes: {}",
            generation.generation(),
            describe(&error)
        ),
    }
    Ok(ExitCode::SUCCESS)
}

/// Returns the store generations `generations`, separated by commas, or `none` without one.
fn store_generations(generations: &[StoreGeneration]) -> String {
    joined(generations.iter().map(StoreGeneration::to_string))
}

/// Returns `items`, separated by commas, or `none` without one.
fn joined(items: impl Iterator<Item = String>) -> String {
    let joined = items.collect::<Vec<_>>().join(",");
    if joined.is_empty() {
        "none".to_owned()
    } else {
        joined
    }
}

/// Reports the generations of the template store `store`, which it opens, in the order of their
/// publication, with their eligibility at the host's time, from the store's records alone: it
/// reads no generation's payload, and takes no lock.
fn list(store: &Path) -> Result<ExitCode, String> {
    let store = Store::open(store).map_err(|error| describe(&error))?;
    let now = HostTime::now().map_err(|error| describe(&error))?;
    let entries = generations::entries(&store, &now).map_err(|error| describe(&error))?;
    println!("store={}", store.id());
    let listed: Vec<_> = entries
        .iter()
        .map(|entry| entry.generation.generation())
        .collect();
    println!("generations={}", store_generations(&listed));
    for entry in &entries {
        print_entry(
            &format!("generation.{}.", entry.generation.generation()),
            entry,
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Reports the generation `generation` of the template store `store`, which it opens, as `list`
/// reports it, with its leases and the metadata of its capture.
fn inspect(store: &Path, generation: StoreGeneration) -> Result<ExitCode, String> {
    let store = Store::open(store).map_err(|error| describe(&error))?;
    let now = HostTime::now().map_err(|error| describe(&error))?;
    let entry = generations::entry(&store, generation, &now).map_err(|error| describe(&error))?;
    println!("store={}", store.id());
    println!("generation={generation}");
    print_entry("", &entry);
    print_leases(&entry.leases);
    let indexed = &entry.generation;
    println!("storage={}", indexed.storage_generation());
    let time = indexed.time();
    let host: String = time
        .host_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    println!("time.host={host}");
    println!("time.boot={}", time.boot_id);
    println!("time.clock={}", time.host_clock.as_str());
    println!("time.utc={}", time.capture_utc_ns);
    println!("time.monotonic={}", time.capture_monotonic_ns);
    println!("time.lineage={}", time.lineage);
    println!("length.state={}", indexed.state_len());
    println!("length.memory={}", indexed.memory_len());
    Ok(ExitCode::SUCCESS)
}

/// Prints what `list` and `inspect` report of the generation of `entry`, each key after `prefix`.
fn print_entry(prefix: &str, entry: &Entry) {
    let generation = &entry.generation;
    println!("{prefix}key={}", generation.key());
    println!("{prefix}class={}", generation.class());
    println!("{prefix}state={}", entry.state.name());
    let eligibility = match entry.eligibility {
        Eligibility::Eligible(_) => "eligible",
        Eligibility::Early { .. } => "early",
        Eligibility::Expired(_) => "expired",
    };
    println!("{prefix}eligibility={eligibility}");
    println!("{prefix}reason={}", escape(&entry.eligibility.to_string()));
    let leases = entry.leases.iter().map(|lease| lease.id().to_string());
    println!("{prefix}leases={}", joined(leases));
}

/// Prints the sandbox and the launch of each of `leases`.
fn print_leases(leases: &[Lease]) {
    for lease in leases {
        let id = lease.id();
        println!(
            "lease.{id}.sandbox={}",
            escape(&lease.sandbox().to_string_lossy())
        );
        println!("lease.{id}.launch={}", lease.launch());
    }
}

/// Retires the generation `generation` of the template store `store`, which it opens: it
/// tombstones the generation, and deletes it once no lease of it remains. Ends with [`BUSY`] once
/// it reported the leases that keep the generation.
fn remove(store: &Path, generation: StoreGeneration) -> Result<ExitCode, String> {
    let store = Store::open(store).map_err(|error| describe(&error))?;
    let retirement = store
        .retire(generation, Some(Instant::now() + LOCK_TIMEOUT))
        .map_err(|error| describe(&error))?;
    match retirement {
        Retirement::Retired => {
            println!("removed=retired");
            Ok(ExitCode::SUCCESS)
        }
        Retirement::Draining(leases) => {
            println!("removed=draining");
            let ids = leases.iter().map(|lease| lease.id().to_string());
            println!("leases={}", joined(ids));
            print_leases(&leases);
            Ok(ExitCode::from(BUSY))
        }
    }
}

/// A directory private to the current user, which a command creates in the system's temporary
/// directory, with a name of its own, and removes with everything in it once it drops it, as
/// `create` does with the initramfs of a capture's VM.
struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    /// Creates a directory in the system's temporary directory, whose name starts with `prefix`,
    /// then the process ID, and which nothing had, private to the current user as NVX makes the
    /// directory of a sandbox store, with `SandboxStore::create`: no one else may create, change,
    /// or replace a file in it.
    fn new(prefix: &str) -> Result<Self, String> {
        let parent = env::temp_dir();
        let pid = std::process::id();
        for attempt in 0..u32::MAX {
            let path = parent.join(format!("{prefix}{pid}-{attempt}"));
            match fs::symlink_metadata(&path) {
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("cannot open {}: {error}", path.display())),
            }
            // NVX refuses a directory that someone else created at the path meanwhile, unless it
            // is private to the current user, as it refuses such a sandbox store.
            SandboxStore::create(&path).map_err(|error| describe(&error))?;
            return Ok(Self(path));
        }
        Err(format!(
            "every name of a directory of {prefix}{pid}- is taken in {}",
            parent.display()
        ))
    }

    /// Returns the directory's path.
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Starts a clone of the generation of the template store that `clone` names, as it asks, in the
/// sandbox store `store`, which it creates if needed, with the guest agent's build `guest_build`,
/// at the host's time `time`, or at the time that `HostTime::now` samples without one, as
/// [`start_clone`] does, with the step hook `hook`, if any, and leaves it running.
fn start(
    store: &Path,
    guest_build: &str,
    clone: &CloneOptions,
    time: Option<HostTime>,
    hook: Option<&StepHook>,
) -> Result<ExitCode, String> {
    // A network that the plan refuses fails the start before anything else, as it fails a launch,
    // then an image, or shares, that it refuses.
    let planned = clone.plan()?;
    // The clone keeps running once the command lets go of its VM, its session, and its sandbox's
    // lock, as once a launch committed.
    Ok(
        match start_clone(store, guest_build, clone, planned, time, hook)? {
            Ok(_running) => ExitCode::SUCCESS,
            Err(status) => status,
        },
    )
}

/// A clone that [`start_clone`] started, whose launch it committed, and which runs.
struct Running {
    /// The runtime of NVX's artifacts, under which the clone runs.
    runtime: Runtime,
    /// The clone's sandbox store.
    sandbox: SandboxStore,
    /// The sandbox's lock, which the clone's lease holds.
    locked: LockedStore,
    /// The clone's VM.
    vmm: Vmm,
    /// The session of the adapter's clone order in which the clone's sandbox started.
    client: Client,
}

/// Starts a clone of the generation of the template store that `clone` names, as `planned`, its
/// plan, asks, in the sandbox store `store`, which it creates if needed, with the guest agent's
/// build `guest_build`, at the host's time `time`, or at the time that `HostTime::now` samples
/// without one, and prints what it did. The step hook `hook`, if any, acts at `checked`, once
/// NVX's `ClonePlan` checked the clone. Returns the clone, which runs, once it committed the
/// clone's launch, or the status with which the command ends otherwise: once the template store
/// refused the selection, or the clone failed, as [`clone_failed`] ends it.
fn start_clone(
    store: &Path,
    guest_build: &str,
    clone: &CloneOptions,
    planned: PlannedClone,
    time: Option<HostTime>,
    hook: Option<&StepHook>,
) -> Result<Result<Running, ExitCode>, String> {
    let PlannedClone {
        request,
        plan,
        export,
    } = planned;
    // A selection that the template store refuses as it names no template fails the start before
    // it needs NVX's artifacts or the sandbox's store.
    let templates = match Store::open(&clone.templates) {
        Ok(templates) => templates,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(Err(selection_refused(
                Refusal::NoTemplate("no-store"),
                &error,
            )));
        }
        Err(error) => return Err(describe(&error)),
    };
    let index = templates.index().map_err(|error| describe(&error))?;
    let Some(generation) = index.get(clone.generation).cloned() else {
        // The store never assigns a store generation twice, so one before its next publication's
        // was published, and retired since.
        let evidence = if clone.generation < index.next_generation() {
            "retired"
        } else {
            "unpublished"
        };
        let error = nvx::Error::not_found(format!(
            "the template store {} holds no generation {}",
            templates.dir().display(),
            clone.generation
        ));
        return Ok(Err(selection_refused(
            Refusal::NoTemplate(evidence),
            &error,
        )));
    };
    // The clone's processor and memory targets are the request's, or else the template's, which
    // the generation's key gives: its processors online at boot, all of them for a template that
    // records none, and its RAM. A key that cannot be read gives none: NVX's plan reads the key
    // again, once the store leased the generation, and attributes the failure, to the generation
    // or to the host, before it checks a target.
    let targets = &clone.request;
    let key = if targets.processors.is_none() || targets.memory.is_none() {
        templates.read_key(&generation).ok()
    } else {
        None
    };
    let processors = targets.processors.or_else(|| {
        key.as_ref().map(|key| match key.boot_processors() {
            0 => key.processors(),
            boot => boot,
        })
    });
    let memory = targets
        .memory
        .or_else(|| key.as_ref().map(|key| key.memory()));
    let runtime = runtime(guest_build)?;
    runtime
        .artifacts
        .probe()
        .map_err(|error| describe(&error))?;
    let now = match time {
        Some(time) => time,
        None => HostTime::now().map_err(|error| describe(&error))?,
    };
    let sandbox = SandboxStore::create(store).map_err(|error| describe(&error))?;
    let acquired = templates.acquire(
        generation.generation(),
        sandbox.dir(),
        &CallerFiles::none(),
        &now,
        Some(Instant::now() + LOCK_TIMEOUT),
    );
    let mut acquired = match acquired {
        Ok(acquired) => acquired,
        Err(error) => {
            return match Refusal::of(&error, &generation, &now) {
                Some(refusal) => Ok(Err(selection_refused(refusal, &error))),
                None => Err(describe(&error)),
            };
        }
    };
    println!("generation={}", generation.generation());
    println!("lease={}", acquired.lease.id());
    if let Eligibility::Eligible(downtime) = generation.eligibility(&now) {
        println!("downtime={}", downtime.nanos);
        println!("source={}", source_name(downtime.source));
    }
    let clone = CloneLaunch {
        templates: &templates,
        request: &request,
        plan,
        export,
        processors,
        memory,
        runtime: &runtime,
        guest_build,
        hook,
    };
    match clone.run(&mut acquired) {
        Ok((vmm, client)) => Ok(Ok(Running {
            runtime,
            sandbox,
            locked: acquired.locked,
            vmm,
            client,
        })),
        Err(Failed::Clone(failure)) => {
            clone_failed(Ok(&templates), acquired.locked, failure).map(Err)
        }
        Err(Failed::Command(problem)) => {
            // The command failed before the clone's launch, so nothing of it runs.
            match release_lease(&templates, &mut acquired.locked) {
                Ok(()) => Err(problem),
                Err(release) => Err(format!("{problem}; {release}")),
            }
        }
    }
}

/// Why a template store refused the selection of a start, as the start reports it: its outcome,
/// with the evidence that attributes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    /// The selection names no template: the store does not exist, `no-store`, or never published
    /// the generation, `unpublished`, or retired it, `retired`.
    NoTemplate(&'static str),
    /// The restore window of the time contract of the generation's capture no longer admits it at
    /// the host's time.
    Expired,
    /// The generation takes no new clone: a tombstone quarantines it, `tombstone`, or its capture
    /// is after the host's time, `early`.
    Ineligible(&'static str),
}

impl Refusal {
    /// Returns why the store refused, with `error`, to lease `generation` at the host's time `now`,
    /// or `None` if the error is not a refusal of the selection: the store no longer holds the
    /// generation, which it retired since the start read its index, a tombstone quarantines it, or
    /// it is not eligible at `now`, as `Generation::eligibility` decides it. A sandbox that is not
    /// provisioned, whose error is of the kind `NotFound` too, is not a refusal of the selection.
    fn of(error: &nvx::Error, generation: &Generation, now: &HostTime) -> Option<Self> {
        match error.code() {
            ErrorCode::TemplateTombstoned => Some(Self::Ineligible("tombstone")),
            ErrorCode::TemplateIneligible => Some(match generation.eligibility(now) {
                Eligibility::Expired(_) => Self::Expired,
                _ => Self::Ineligible("early"),
            }),
            ErrorCode::NotFound => Some(Self::NoTemplate("retired")),
            _ => None,
        }
    }

    /// Returns the outcome and the evidence of the refusal, as its record spells them.
    fn names(self) -> (&'static str, &'static str) {
        match self {
            Self::NoTemplate(evidence) => ("no-template", evidence),
            Self::Expired => ("expired", "expired"),
            Self::Ineligible(evidence) => ("ineligible", evidence),
        }
    }
}

/// Ends a start whose template store refused its selection, as `refusal` says, with `error`, which
/// leased and launched nothing: prints the refusal's record, as [`print_clone_failure`] prints a
/// failure's, at the stage `selection`, and returns the exit status [`SELECTION_REFUSED`].
fn selection_refused(refusal: Refusal, error: &nvx::Error) -> ExitCode {
    let (outcome, evidence) = refusal.names();
    println!("failure.outcome={outcome}");
    println!("failure.evidence={evidence}");
    println!("failure.stage=selection");
    println!("failure.progress=none");
    println!("failure.status=none");
    println!("failure.teardown=no-vm");
    println!("failure.code={}", error.code());
    eprintln!("runtime-adapter-host: {}", describe(error));
    ExitCode::from(SELECTION_REFUSED)
}

/// How the launch of a clone failed, once the store leased its generation.
enum Failed {
    /// The clone failed, as NVX attributes the failure, and records it from the clone's launch on.
    Clone(CloneFailure),
    /// The command failed otherwise, with this problem, as a step of the adapter's own did before
    /// the clone's launch.
    Command(String),
}

/// The clone that `start` launches, once the store leased the generation.
struct CloneLaunch<'a> {
    /// The template store.
    templates: &'a Store,
    /// The clone, as the command's options give it.
    request: &'a runtime_adapter::clone::Request,
    /// The adapter's plan of the clone's image over its upper, with its network.
    plan: nvx::host::sandbox::SandboxPlan,
    /// The export of the host paths that the clone shares with its root, if any.
    export: Option<ExportPlan>,
    /// The clone's processor target, or none if the request names none, and the generation's
    /// key, whose processors online at boot it would be, cannot be read.
    processors: Option<u32>,
    /// The clone's memory target, in bytes, or none if the request names none, and the
    /// generation's key, whose RAM it would be, cannot be read.
    memory: Option<u64>,
    /// The runtime of NVX's artifacts.
    runtime: &'a Runtime,
    /// The guest agent's build.
    guest_build: &'a str,
    /// The step hook, if any, which acts at `checked` alone.
    hook: Option<&'a StepHook>,
}

impl CloneLaunch<'_> {
    /// Plans, records, and launches the clone in the sandbox of `acquired`, which leased the
    /// generation, activates it, maps its export's table, if it has one, starts its sandbox,
    /// exchanges a `Ping` and the longest `Echo` with its guest agent, and commits the launch,
    /// leaving the clone running. Returns the clone's VM, and the session of the adapter's order
    /// in which its sandbox started. The step hook, if any, acts once the plan checked the clone,
    /// before the clone is recorded.
    ///
    /// Fails with the clone's failure, as NVX's plan, launch, restore sequence, and commit return
    /// it, or as `Started::fail` records it for a guest agent that does not answer the command
    /// once the clone's sandbox started, which ended the launch, unless its teardown is
    /// unresolved, and with the problem of a step of the adapter's own before the launch
    /// otherwise.
    fn run(self, acquired: &mut Acquired) -> Result<(Vmm, Client), Failed> {
        let Self {
            templates,
            request,
            plan,
            export,
            processors,
            memory,
            runtime,
            guest_build,
            hook,
        } = self;
        let staged = Staged::read(acquired.locked.dir())
            .map_err(|error| Failed::Command(describe(&error)))?;
        let bindings = staged.map_or_else(Bindings::default, |staged| staged.bindings(&plan));
        // No template serves a processor or memory target of 0, which stands for the one of a key
        // that could not be read: the plan fails as it reads the key again, before it checks the
        // targets.
        let clone_request = request
            .clone_request(
                plan.clone(),
                export,
                &bindings,
                processors.unwrap_or(0),
                memory.unwrap_or(0),
            )
            .map_err(|error| Failed::Command(binding_error(&error)))?;
        let clone_plan = ClonePlan::new(
            templates,
            acquired,
            &clone_request,
            Instant::now() + LOCK_TIMEOUT,
        )
        .map_err(Failed::Clone)?;
        // The plan checked the clone against the generation, and holds the files that the checks
        // opened, but nothing records the clone yet, and nothing runs: a test may change what the
        // clone's launch checks again.
        step(hook, Step::Checked).map_err(Failed::Command)?;
        // The mappings that the clone's record announces, which the restore sequence sends.
        let mappings = clone_plan.activation().configuration().mappings;
        let record = CloneRecord {
            template: templates.dir(),
            generation: acquired.lease.generation(),
            kernel_command_line: clone_plan.key().command_line(),
            plan: &plan,
            bindings: &bindings,
        };
        record
            .write(acquired.locked.dir())
            .map_err(|error| Failed::Command(describe(&error)))?;
        let restoring = clone_plan
            .launch(acquired, &runtime.artifacts.openvmm, &runtime.identity)
            .map_err(Failed::Clone)?;
        println!("pid={}", restoring.starting().process().reference().pid);
        let mut order = clone_order(guest_build);
        // The restore sequence, and the commit, end the launch that they fail.
        let activated = restoring
            .activate(&mut order, RestoreDeadlines::default())
            .map_err(Failed::Clone)?;
        println!("root=ready");
        println!("build={}", escape(guest_build));
        let mut started = activated.start(&mut order).map_err(Failed::Clone)?;
        println!("table=applied");
        println!("mapped={mappings}");
        println!("sandbox=started");
        let deadline = Instant::now() + SESSION_TIMEOUT;
        let client = started.session();
        let answered = client
            .ping(deadline)
            .and_then(|()| client.echo(&echo_body(), deadline));
        if let Err(error) = answered {
            let (evidence, error) = unanswered(&order, error, deadline);
            return Err(Failed::Clone(started.fail(evidence, error)));
        }
        started.commit().map_err(Failed::Clone)
    }
}

/// Returns the evidence and the error of a failure of the session of a clone whose sandbox
/// started, in which the client failed, with `error`, a request of the command's own by
/// `deadline`, as NVX's restore sequence attributes the failures of its own sessions: the passing
/// of the deadline, the loss of a session that the clone's `order` takes for lost, or a failure of
/// the protocol otherwise.
fn unanswered(
    order: &CloneOrder<'_>,
    error: ClientError,
    deadline: Instant,
) -> (Evidence, nvx::Error) {
    const PROBLEM: &str = "the guest agent did not answer once the clone's sandbox started";
    let timed_out = matches!(error, ClientError::Session(HostConnectionError::TimedOut));
    if timed_out || Instant::now() >= deadline {
        return (
            Evidence::Timeout,
            nvx::Error::timed_out(PROBLEM).with_source(error),
        );
    }
    let error = OrderError::Client(error);
    let evidence = if order.lost(&error) {
        Evidence::Reset
    } else {
        Evidence::Protocol
    };
    (evidence, nvx::Error::protocol(PROBLEM).with_source(error))
}

/// Returns the adapter's order of a clone, with the guest agent's build `build`, which the restore
/// sequence of a clone's start, and a reattach to it, take, the one place that makes it. The
/// sequence sends through it the clone's mapping table, which the clone's record announces if the
/// clone has an export, and a reattach sends none.
fn clone_order(build: &str) -> CloneOrder<'_> {
    CloneOrder { build }
}

/// Ends a command whose clone, in the sandbox of `locked`, failed with `failure`, as NVX
/// attributes and records the failure, and returns its exit status.
///
/// It prints the failure's record. Once the failure's teardown is resolved, nothing of the clone
/// runs, and it releases the clone's lease through the template store `templates`, which removes
/// the records of the clone's launch, its failure's included, so that the sandbox may lease a
/// generation again. While the teardown is unresolved, the sandbox keeps the lease, and the
/// records, for a later reattach. Then, once it released the sandbox's lock, which comes after the
/// template store's in NVX's lock order, it quarantines the leased generation through its
/// tombstone if the failure is the template's, as `CloneFailure::quarantine` does, and prints
/// whether it did.
fn clone_failed(
    templates: Result<&Store, String>,
    mut locked: LockedStore,
    failure: CloneFailure,
) -> Result<ExitCode, String> {
    print_clone_failure(failure.record());
    let mut problem = describe(failure.error());
    let log = locked.log_path();
    if log.is_file() {
        problem = format!("{problem}; see the VMM's log {}", log.display());
    }
    let resolved = failure.teardown().is_resolved();
    let released = match &templates {
        Ok(templates) if resolved => release_lease(templates, &mut locked),
        Ok(_) => Ok(()),
        Err(problem) => Err(format!("the sandbox keeps its lease: {problem}")),
    };
    drop(locked);
    let quarantined = templates.and_then(|templates| {
        failure
            .quarantine(templates, Some(Instant::now() + LOCK_TIMEOUT))
            .map_err(|error| {
                format!(
                    "the failure's generation was not quarantined: {}",
                    describe(&error)
                )
            })
    });
    if let Ok(quarantined) = quarantined {
        println!("quarantined={}", if quarantined { "yes" } else { "no" });
    }
    match (released, quarantined) {
        (Ok(()), Ok(_)) => {
            eprintln!("runtime-adapter-host: {problem}");
            Ok(ExitCode::from(if resolved {
                CLONE_FAILED
            } else {
                CLONE_UNRESOLVED
            }))
        }
        (Err(release), _) => Err(format!("{problem}; {release}")),
        (Ok(()), Err(quarantine)) => Err(format!("{problem}; {quarantine}")),
    }
}

/// Prints the `record` of a clone's failure: its outcome, the evidence that attributes it, the
/// stage at which the clone failed, how far the clone got, the guest's status, if OpenVMM's
/// outcome report gives one, how the teardown that followed ended, and the error's code.
fn print_clone_failure(record: &FailureRecord) {
    println!("failure.outcome={}", record.outcome);
    println!("failure.evidence={}", record.evidence);
    println!("failure.stage={}", record.stage);
    println!("failure.progress={}", record.progress);
    println!(
        "failure.status={}",
        record
            .status
            .map_or("none".to_owned(), |status| status.to_string())
    );
    println!("failure.teardown={}", record.teardown.as_str());
    println!("failure.code={}", record.code);
}

/// Releases the lease of the template store `templates` that the sandbox of `locked` holds, once
/// nothing of its clone's launch runs, and prints it.
fn release_lease(templates: &Store, locked: &mut LockedStore) -> Result<(), String> {
    let released = templates
        .release(locked)
        .map_err(|error| format!("the sandbox keeps its lease: {}", describe(&error)))?;
    if let Some(lease) = released {
        println!("released={}", lease.id());
    }
    Ok(())
}

/// Returns the name of `source`, as `start` prints it.
fn source_name(source: DowntimeSource) -> &'static str {
    match source {
        DowntimeSource::HostMonotonic => "monotonic",
        DowntimeSource::Utc => "utc",
    }
}

/// Opens the template store whose lease the sandbox of `locked` holds: `named`, which the release
/// checks against the lease, or else the one that the record of the sandbox's clone names, once
/// the lease is of it. A sandbox whose start ended before it recorded its clone records none, or
/// an earlier clone, of another template store perhaps, which a release leaves: only `named` then
/// names the lease's.
fn clone_templates(locked: &LockedStore, named: Option<&Path>) -> Result<Store, String> {
    if let Some(named) = named {
        return Store::open(named).map_err(|error| describe(&error));
    }
    let unnamed = |problem: String| {
        format!(
            "{problem}, so the template store of the lease of the sandbox in {} is unknown: name \
             it with --template TEMPLATES",
            locked.dir().display()
        )
    };
    let (templates, _) = CloneRecord::read_template(locked.dir())
        .map_err(|error| unnamed(describe(&error)))?
        .ok_or_else(|| unnamed("the sandbox records no clone".to_owned()))?;
    let store = Store::open(&templates).map_err(|error| unnamed(describe(&error)))?;
    match Lease::of(locked).map_err(|error| describe(&error))? {
        Some(lease) if lease.store() == store.id() => Ok(store),
        _ => Err(unnamed(format!(
            "the sandbox's record of its clone names the template store {}, which did not grant \
             its lease",
            templates.display()
        ))),
    }
}

/// Opens the sandbox store `path`, and takes its lock, through the path of the lease that the
/// sandbox holds, if it holds one, as a release requires, and returns whether the sandbox holds
/// one, as it reads it under the lock: an acquisition binds the lease under the sandbox's lock
/// too.
///
/// A lease names the sandbox by its path with its links resolved, so the lock that a second
/// attempt takes through it names the sandbox as the lease does. The sandbox there must hold the
/// same lease: a sandbox that holds a lease must not move, and the lease of one that moved names
/// a path at which another sandbox, or none, may be, which the command must not take for it.
fn lock_sandbox(path: &Path) -> Result<(SandboxStore, LockedStore, bool), String> {
    let deadline = Instant::now() + LOCK_TIMEOUT;
    let store = SandboxStore::open(path).map_err(|error| describe(&error))?;
    let locked = store
        .lock(Some(deadline))
        .map_err(|error| describe(&error))?;
    let lease = match Lease::of(&store).map_err(|error| describe(&error))? {
        None => return Ok((store, locked, false)),
        Some(lease) if lease.sandbox() == locked.dir() => return Ok((store, locked, true)),
        Some(lease) => lease,
    };
    // The lock through the lease's path is the same lock when both paths name the sandbox.
    drop(locked);
    drop(store);
    let named = lease.sandbox();
    let moved = |problem: &str| {
        format!(
            "the lease of the sandbox in {} names the sandbox in {}, {problem}: a sandbox that \
             holds a lease must not move",
            path.display(),
            named.display()
        )
    };
    let store = SandboxStore::open(named).map_err(|error| moved(&describe(&error)))?;
    let locked = store
        .lock(Some(deadline))
        .map_err(|error| describe(&error))?;
    match Lease::of(&store).map_err(|error| describe(&error))? {
        Some(held) if held == lease && held.sandbox() == locked.dir() => Ok((store, locked, true)),
        Some(held) if held == lease => Err(format!(
            "the lease of the sandbox in {} does not name it by the path through which its lock is \
             taken",
            named.display()
        )),
        _ => Err(moved("which does not hold it")),
    }
}

/// The VM of a sandbox that a command found, as [`find`] returns it, under the sandbox's lock,
/// which the command holds.
struct Found {
    /// The sandbox's store.
    store: SandboxStore,
    /// The sandbox's lock, which names a clone's sandbox by its lease's path.
    locked: LockedStore,
    /// The VM.
    vmm: Vmm,
    /// The session of the adapter's framing that the command holds of the VM.
    session: Session,
    /// Whether the VM is a clone, whose lease the sandbox holds.
    leased: bool,
}

/// The session that a command holds of the VM that it found.
enum Session {
    /// None yet, of a VM that booted cold, to which the command attaches one once it needs it, as
    /// [`Found::attach`] does.
    Cold,
    /// The one in which a clone answered the Activate of its record again, with the Ready that it
    /// answered once, as `reattach_clone` checks it.
    Clone(Box<Client>),
}

impl Found {
    /// Returns the session that the command holds of the VM, attaching one, with the sandbox's
    /// capability, by `deadline`, to a VM that booted cold.
    fn attach(&mut self, deadline: Instant) -> Result<Client, String> {
        match std::mem::replace(&mut self.session, Session::Cold) {
            Session::Clone(client) => Ok(*client),
            Session::Cold => {
                let capability = self
                    .store
                    .read_capability()
                    .map_err(|error| describe(&error))?;
                connect(&self.vmm, &capability, deadline)
            }
        }
    }
}

/// Finds the VM of the sandbox store `path`, under the runtime identity of `runtime`, with the
/// guest agent's build `guest_build`, and prints what it found. Returns the VM, which runs, or the
/// status with which the command ends otherwise.
///
/// It takes the sandbox's lock through the path of the lease that the sandbox holds, if it holds
/// one, which it reads under the lock, and refuses a sandbox whose lease names a path at which no
/// sandbox holds the same lease, as [`lock_sandbox`] does. The VM of a sandbox without a lease
/// booted cold: it holds the VM again, as `supervisor::reattach` does, and returns a mismatched
/// VM's status, which it leaves running. A sandbox with a lease holds a clone, which it reattaches
/// to with `reattach_clone`, in a new session of the adapter's clone order, in which the clone
/// answers the Activate of its record again, with its Ready, and which ends a clone that it may
/// not keep: one that runs under another runtime identity, without a word to its guest, or that
/// fails the check of its Ready. A clone that ended releases its lease, and one that failed, or
/// whose earlier failure the reattach finishes, ends the command as [`clone_failed`] does, through
/// the template store that [`clone_templates`] opens with `templates`. A clone whose state cannot
/// be determined keeps its lease and its records, and fails the command; see [`conclude`].
fn find(
    path: &Path,
    runtime: &Runtime,
    guest_build: &str,
    templates: Option<&Path>,
) -> Result<Result<Found, ExitCode>, String> {
    let (store, mut locked, leased) = lock_sandbox(path)?;
    if !leased {
        let vmm = match hold(&mut locked, runtime)? {
            Ok(vmm) => vmm,
            Err(status) => return Ok(Err(status)),
        };
        return Ok(Ok(Found {
            store,
            locked,
            vmm,
            session: Session::Cold,
            leased: false,
        }));
    }
    let reattached = reattach_leased(&mut locked, runtime, guest_build)?;
    let found = match &reattached {
        ReattachedClone::Live { .. } => "live",
        ReattachedClone::Ended { .. } => "ended",
        ReattachedClone::Failed(_) => "failed",
        ReattachedClone::Indeterminate(_) => "indeterminate",
        _ => return Err(UNKNOWN_STATE.to_owned()),
    };
    println!("reattached={found}");
    let ReattachedClone::Live { vmm, session } = reattached else {
        return conclude(reattached, locked, templates).map(Err);
    };
    println!("pid={}", vmm.record().process().pid);
    print_identity(&session);
    Ok(Ok(Found {
        store,
        locked,
        vmm: *vmm,
        session: Session::Clone(Box::new(session)),
        leased: true,
    }))
}

/// What a command says of a clone that `reattach_clone` found in a state that this build does not
/// know.
const UNKNOWN_STATE: &str = "the reattach found the clone in a state that this build does not know";

/// Reattaches to the clone of the sandbox of `locked`, under the runtime identity of `runtime`,
/// with `reattach_clone`, in a new session of the adapter's clone order, with the guest agent's
/// build `guest_build`.
fn reattach_leased(
    locked: &mut LockedStore,
    runtime: &Runtime,
    guest_build: &str,
) -> Result<ReattachedClone<Client>, String> {
    let mut order = clone_order(guest_build);
    let deadline = Instant::now() + SESSION_TIMEOUT;
    reattach_clone(
        locked,
        &runtime.identity,
        &mut order,
        deadline,
        FORCED_TIMEOUT,
    )
    .map_err(|error| describe(&error))
}

/// Ends a command on the clone of the sandbox of `locked` that `reattached`, what
/// `reattach_clone` found of it, tells does not run, and returns the command's exit status, or
/// fails. It releases the lease of a clone that ended, through the template store `templates`, or
/// the one that the clone's record names, as [`clone_templates`] opens it, and fails; ends as
/// [`clone_failed`] does for a clone that failed; and keeps the lease and the records, and fails,
/// for a clone whose state cannot be determined, or that runs.
fn conclude(
    reattached: ReattachedClone<Client>,
    mut locked: LockedStore,
    templates: Option<&Path>,
) -> Result<ExitCode, String> {
    match reattached {
        ReattachedClone::Ended { progress, report } => {
            println!("progress={progress}");
            if let Some(report) = &report {
                print_outcome(report);
            }
            let templates = clone_templates(&locked, templates)?;
            release_lease(&templates, &mut locked)?;
            Err(format!(
                "no VM of the clone runs: it ended at its step {progress}, and the sandbox's lease \
                 is released"
            ))
        }
        ReattachedClone::Failed(failure) => {
            let templates = clone_templates(&locked, templates);
            clone_failed(templates.as_ref().map_err(Clone::clone), locked, failure)
        }
        ReattachedClone::Indeterminate(reason) => Err(format!(
            "whether the clone's VM runs cannot be determined, so the sandbox keeps its lease: \
             {reason}"
        )),
        ReattachedClone::Live { .. } => Err(
            "the clone's VM runs again, which the command did not expect, so the sandbox keeps \
             its lease"
                .to_owned(),
        ),
        _ => Err(UNKNOWN_STATE.to_owned()),
    }
}

/// Reattaches to the VM of the sandbox store `store`, with the guest agent's build `guest_build`,
/// runs the step hook `hook`, if any, once it found the VM, at [`Step::Found`], then attaches a
/// session to the VM, and exchanges a `Ping`, then stops the VM if `stop` asks.
///
/// The stop of a clone leaves the clone's records, which the release of its lease removes, to
/// tell how the clone ended first, as `reattach_clone` reads them. A clone's VM that had exited
/// when the stop began ended without its holder, as it may at any time: the stop,
/// `Vmm::stop_unless_exited`, stops nothing then, and keeps the runtime record of its launch,
/// which tells so, and the command prints `stopped=exited`, and ends as [`find`] does for a clone
/// that it finds so. A VM that exited once the stop began ended through the stop, unless the
/// guest's status that ended it is evidence of a failure, which ends the command as
/// [`clone_failed`] does. The lease of a clone that ended otherwise is released through the
/// template store `templates`, or the one that the clone's record names, as [`clone_templates`]
/// opens it. A VM that booted cold, whose end the command attributes to nothing, stops with
/// `Vmm::stop`, which ends one that had exited by then as one that it stopped: its outcome report
/// then tells how its guest ended.
fn reattach(
    store: &Path,
    guest_build: &str,
    stop: bool,
    templates: Option<&Path>,
    hook: Option<&StepHook>,
) -> Result<ExitCode, String> {
    let runtime = runtime(guest_build)?;
    let mut found = match find(store, &runtime, guest_build, templates)? {
        Ok(found) => found,
        Err(status) => return Ok(status),
    };
    step(hook, Step::Found)?;
    let deadline = Instant::now() + SESSION_TIMEOUT;
    let attached = found.attach(deadline).and_then(|mut client| {
        client
            .ping(deadline)
            .map_err(|error| format!("the guest agent did not answer: {}", describe(&error)))?;
        Ok(client)
    });
    if !stop {
        return attached.map(|_| ExitCode::SUCCESS);
    }
    let Found {
        store,
        locked,
        vmm,
        leased,
        ..
    } = found;
    let clone = leased.then_some(CloneStop {
        runtime: &runtime,
        guest_build,
        templates,
    });
    stop_vm(&store, locked, vmm, clone, |deadline| {
        attached?
            .stop(deadline)
            .map_err(|error| unconfirmed(&error))
    })
}

/// Returns the problem of a stop whose guest agent did not confirm it, as the client failed with
/// `error`.
fn unconfirmed(error: &ClientError) -> String {
    format!(
        "the guest agent did not confirm the stop: {}",
        describe(error)
    )
}

/// What a command that stops the VM of a clone, as [`stop_vm`] stops it, needs to end the clone:
/// the runtime and the guest agent's build under which it reattaches to the clone once its VM
/// exited, as [`reattach_leased`] does, and the template store that the command names, if any, in
/// place of the one that the clone's record names, through which it releases the clone's lease.
#[derive(Clone, Copy)]
struct CloneStop<'a> {
    /// The runtime of NVX's artifacts.
    runtime: &'a Runtime,
    /// The guest agent's build.
    guest_build: &'a str,
    /// The template store that the command names, if any.
    templates: Option<&'a Path>,
}

/// Stops the VM `vmm` of the sandbox store `store`, whose lock `locked` the command holds, and
/// prints how it stopped. Its graceful step, `confirm`, asks the guest agent to stop by the
/// deadline that it takes, and returns, once the guest agent confirmed the stop, the failure of
/// the root's teardown that the guest agent reported first, if any. A VM whose guest agent cannot
/// confirm the stop is terminated.
///
/// The VM of a clone, which `clone` ends, stops as [`reattach`] describes: with
/// `Vmm::stop_unless_exited`, after which the command reads how the clone ended from its records,
/// and releases its lease once the clone ended through the stop, or ends as [`conclude`] does
/// otherwise. A VM that booted cold stops with `Vmm::stop`.
///
/// Returns success once the VM exited, as its guest agent confirmed, with the outcome `success 0`
/// and a complete teardown, and [`ROOT_FAILED`] once the guest agent reported the failure of its
/// root, which it prints, and fails otherwise.
fn stop_vm(
    store: &SandboxStore,
    mut locked: LockedStore,
    vmm: Vmm,
    clone: Option<CloneStop<'_>>,
    confirm: impl FnOnce(Instant) -> Result<Option<RootFailure>, String>,
) -> Result<ExitCode, String> {
    // A VM whose guest agent cannot confirm the stop is terminated.
    let mut failure = None;
    let graceful = |_: &Vmm, deadline: Instant| -> Result<(), String> {
        failure = confirm(deadline)?;
        Ok(())
    };
    let stopped = if clone.is_some() {
        // A clone's VM that had exited when the stop began ended without its holder: the stop
        // stops nothing then, and keeps the runtime record of its launch, which tells so.
        vmm.stop_unless_exited(
            &mut locked,
            Instant::now() + STOP_TIMEOUT,
            Some(FORCED_TIMEOUT),
            graceful,
        )
    } else {
        vmm.stop(
            &mut locked,
            Instant::now() + STOP_TIMEOUT,
            Some(FORCED_TIMEOUT),
            graceful,
        )
        .map(Some)
    }
    .map_err(|error| describe(&error))?;
    let Some(stopped) = stopped else {
        println!("stopped=exited");
        let clone = clone.expect("only the stop of a clone's VM stops nothing");
        let reattached = reattach_leased(&mut locked, clone.runtime, clone.guest_build)?;
        return conclude(reattached, locked, clone.templates);
    };
    println!(
        "stopped={}",
        match stopped.outcome {
            StopOutcome::Graceful => "graceful",
            StopOutcome::Forced => "forced",
            StopOutcome::TimedOut => "timed-out",
        }
    );
    // The store verified that the VM exited, unless the stop timed out.
    if let Some(clone) = clone
        && stopped.outcome != StopOutcome::TimedOut
    {
        match reattach_leased(&mut locked, clone.runtime, clone.guest_build)? {
            ReattachedClone::Ended { .. } => {
                let templates = clone_templates(&locked, clone.templates)?;
                release_lease(&templates, &mut locked)?;
            }
            reattached => return conclude(reattached, locked, clone.templates),
        }
    }
    if let Some(problem) = stopped.graceful_error {
        return Err(problem);
    }
    if stopped.outcome != StopOutcome::Graceful {
        return Err("the VM did not exit once the guest agent confirmed the stop".to_owned());
    }
    match failure {
        None => {
            check_outcome(store, "success", 0)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(failure) => {
            print_failure(&failure);
            check_outcome(store, "guest-exit", failure.stage().exit_status().into())?;
            Ok(ExitCode::from(ROOT_FAILED))
        }
    }
}

/// Reattaches to the VM of the sandbox store `store`, with the guest agent's build `guest_build`,
/// attaches a session to it, and runs `probes` in the VM's layered root, printing the facts that
/// each found.
fn probe(store: &Path, guest_build: &str, probes: &[Probe]) -> Result<ExitCode, String> {
    let runtime = runtime(guest_build)?;
    let mut found = match find(store, &runtime, guest_build, None)? {
        Ok(found) => found,
        Err(status) => return Ok(status),
    };
    let deadline = Instant::now() + SESSION_TIMEOUT;
    let mut client = found.attach(deadline)?;
    for probe in probes {
        let reply = client
            .probe(probe, deadline)
            .map_err(|error| format!("the guest agent did not answer: {}", describe(&error)))?;
        match reply {
            ProbeReply::Report(facts) => {
                for (key, value) in facts.iter() {
                    println!("{}.{key}={}", probe.name(), escape(value));
                }
            }
            ProbeReply::Failed(problem) => return Err(problem),
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Reattaches to the VM of the sandbox store `store`, with the guest agent's build `guest_build`,
/// attaches a session to it, runs `executions` at once in the VM's layered root, and prints what
/// each did once every one finished.
fn exec(store: &Path, guest_build: &str, executions: &[Execution]) -> Result<ExitCode, String> {
    let runtime = runtime(guest_build)?;
    let mut found = match find(store, &runtime, guest_build, None)? {
        Ok(found) => found,
        Err(status) => return Ok(status),
    };
    // Each execution finds the URL of the proxy of the VM's network, if any, in its environment,
    // from the plan that the launch recorded. The proxy's variables lengthen a start, so the
    // command checks every start again before it starts any, rather than fail once it sent the
    // starts before it, and, for a VM that booted cold, before it attaches. A start takes its key
    // once the command attached, and a key's encoding has the same size whatever its value.
    let plan = LaunchRecord::read_plan(found.store.dir()).map_err(|error| describe(&error))?;
    let starts = starts_of(executions, plan.as_ref())?;
    let deadline = Instant::now() + SESSION_TIMEOUT;
    let mut client = found.attach(deadline)?;
    run_executions(&mut client, &mut Executions::new(), starts, deadline)?;
    Ok(ExitCode::SUCCESS)
}

/// Returns the starts of `executions`, numbered from 1, each with the URL of the proxy of the
/// network of the VM's plan `plan`, if any, in its environment, once it checked that the guest
/// takes every one, or says why it would not take one.
fn starts_of(executions: &[Execution], plan: Option<&SandboxPlan>) -> Result<Vec<Start>, String> {
    let mut starts = Vec::with_capacity(executions.len());
    for (number, execution) in (1..).zip(executions) {
        let start = execution.start(Key::new(0, 0));
        let start = match plan {
            Some(plan) => with_proxy(start, plan),
            None => start,
        };
        check_start(&start).map_err(|problem| {
            format!("execution {number}, with the proxy of the VM's network: {problem}")
        })?;
        starts.push(start);
    }
    Ok(starts)
}

/// Runs the executions of `starts`, numbered from 1, at once, with the driver `running`, which
/// started none before, in the session `client`, starting each by `deadline`, and prints what each
/// did once every one finished.
fn run_executions(
    client: &mut Client,
    running: &mut Executions,
    starts: Vec<Start>,
    deadline: Instant,
) -> Result<(), String> {
    // The keys of the attachment's executions are new to the guest: see the module documentation.
    let identity = client.identity();
    let (base, _) = identity.instance_id.split_at(8);
    let base = u64::from_le_bytes(base.try_into().expect("an instance ID has 16 bytes"));
    let mut ids = Vec::new();
    for (number, mut start) in (1..).zip(starts) {
        let key = Key::new(base.wrapping_add(number), identity.epoch);
        start.key = key;
        running
            .start(client, start, exec_timers(), deadline)
            .map_err(|error| format!("cannot start execution {number}: {}", describe(&error)))?;
        ids.push(key.id());
    }
    // Every execution finishes once it ended, which its timeout bounds, and its output's grace and
    // its settlement passed, or once the guest's last messages about it arrived.
    let deadline = Instant::now() + EXEC_TIMEOUT + EXEC_GRACE + EXEC_SETTLEMENT + SESSION_TIMEOUT;
    running
        .run(client, deadline)
        .map_err(|error| format!("the executions failed: {}", describe(&error)))?;
    for (number, id) in (1..).zip(ids) {
        let execution = running.get(id).expect("the driver started every execution");
        print_execution(number, execution);
    }
    Ok(())
}

/// Runs one execution, `execution`, in a clone of the generation of the template store that
/// `clone` names, as it asks, in the sandbox store `store`, which it creates if needed, with the
/// guest agent's build `guest_build`, at the host's time `time`, or at the time that
/// `HostTime::now` samples without one, then tears the clone down, and prints what it did.
///
/// It first plans the clone as `start` does, with its network and the export of its shares, and
/// checks that the guest takes the execution's start, with the URL of the proxy of the clone's
/// network, if any, in its environment, as `exec` gives it to an execution of a clone. Its
/// preflight then stages the clone's images in the sandbox, as `stage` does, which verifies the
/// image, and prints `preflight=staged`. It then starts the clone, as `start` does, and ends as a
/// start ends once the template store refuses the selection, or the clone fails. It runs the
/// execution in the clone's root, as `exec` runs one, through the guest agent's execution control,
/// in the session in which the clone's sandbox started, and prints what the execution did. Then it
/// stops the clone, whatever the execution did, as `stop` does, which releases the clone's lease
/// once its VM exited. The stop takes that session too, in which it skips the guest's messages
/// about the execution that may still arrive once the command finished it without them.
///
/// An execution that failed to run, as one whose session ended, fails the command after the stop.
/// Since the session may have ended with the VM, as when its VMM crashed, the command first waits
/// for the VMM to exit, as long as a stop waits for one that it terminated, [`FORCED_TIMEOUT`]: the
/// stop then finds that the VM exited without the command, and ends the clone as a reattach that
/// finds it so does, rather than take the exit for its own. Once the stop returned another status
/// than success, [`ROOT_FAILED`], [`CLONE_FAILED`], or [`CLONE_UNRESOLVED`], the command ends with
/// that status, after the execution's diagnostic.
fn run(
    store: &Path,
    guest_build: &str,
    clone: &CloneOptions,
    execution: &Execution,
    time: Option<HostTime>,
) -> Result<ExitCode, String> {
    // A network, an image, or shares that the plan refuses, and a start that the guest would
    // refuse, fail the command before it stages anything.
    let planned = clone.plan()?;
    let starts = starts_of(std::slice::from_ref(execution), Some(&planned.plan))?;
    let request = &planned.request;
    stage(
        store,
        &request.distro,
        request.format,
        request.identity,
        request.scratch.as_deref(),
    )?;
    println!("preflight=staged");
    let Running {
        runtime,
        sandbox,
        locked,
        vmm,
        mut client,
    } = match start_clone(store, guest_build, clone, planned, time, None)? {
        Ok(running) => running,
        Err(status) => return Ok(status),
    };
    let mut executions = Executions::new();
    let ran = run_executions(
        &mut client,
        &mut executions,
        starts,
        Instant::now() + SESSION_TIMEOUT,
    );
    if ran.is_err() {
        // The session may have ended with the VM, as when its VMM crashed. Once the VMM's exit is
        // done, the stop finds that the VM exited without the command, and ends the clone as a
        // reattach that finds it so does, rather than take the exit for its own. The stop checks
        // the VMM itself, so a wait that fails changes nothing.
        let _ = vmm.wait(Instant::now() + FORCED_TIMEOUT);
    }
    // The clone's record names the template store that releases its lease, as for `stop`.
    let stop = CloneStop {
        runtime: &runtime,
        guest_build,
        templates: None,
    };
    let stopped = stop_vm(&sandbox, locked, vmm, Some(stop), |deadline| {
        client
            .stop_skipping(deadline, |message| executions.is_about(message))
            .map_err(|error| unconfirmed(&error))
    });
    match (ran, stopped) {
        (Ok(()), stopped) => stopped,
        // A stop that ended otherwise than in success, as one that found that the clone's VM
        // exited during the execution, tells how the clone ended, so the command ends with its
        // status, after the execution's diagnostic.
        (Err(problem), Ok(status)) if status != ExitCode::SUCCESS => {
            eprintln!("runtime-adapter-host: {problem}");
            Ok(status)
        }
        (Err(problem), Ok(_)) => Err(problem),
        (Err(problem), Err(teardown)) => Err(format!("{problem}; {teardown}")),
    }
}

/// Prints what the execution numbered `number`, `execution`, which finished, did.
fn print_execution(number: u64, execution: &HostExecution) {
    let Some(outcome) = execution.outcome() else {
        unreachable!("every execution finished");
    };
    let launch = match outcome.launch_failure() {
        Some(failure) => format!("failed {} {}", failure.stage(), failure.error()),
        None if execution.peer().has_started() => "started".to_owned(),
        None => "none".to_owned(),
    };
    let status = match outcome.status() {
        Some(Status::Exited(code)) => format!("exit {code}"),
        Some(Status::Signaled(signal)) => format!("signal {signal}"),
        None => "none".to_owned(),
    };
    let truncated = Stream::ALL
        .into_iter()
        .filter(|stream| outcome.is_truncated(*stream))
        .map(stream_name)
        .collect::<Vec<_>>();
    let settled = if outcome.is_reusable() {
        "reusable"
    } else {
        "fenced"
    };
    println!("exec.{number}.launch={launch}");
    println!("exec.{number}.status={status}");
    println!("exec.{number}.cause={}", cause_name(&outcome));
    println!(
        "exec.{number}.truncated={}",
        if truncated.is_empty() {
            "none".to_owned()
        } else {
            truncated.join(",")
        }
    );
    println!("exec.{number}.settled={settled}");
    for stream in Stream::ALL {
        let output = escape_bytes(execution.output(stream));
        println!("exec.{number}.{}={output}", stream_name(stream));
    }
}

/// Returns the name of why a side ended the execution whose outcome is `outcome`, or `none`.
fn cause_name(outcome: &Outcome) -> &'static str {
    match outcome.cause() {
        None => "none",
        Some(Cause::Cancelled) => "cancelled",
        Some(Cause::Dropped) => "dropped",
        Some(Cause::TimedOut) => "timed-out",
        Some(Cause::Reset) => "reset",
        Some(Cause::Disconnected) => "disconnected",
        Some(_) => "other",
    }
}

/// Returns the name of `stream`: `stdout` or `stderr`.
fn stream_name(stream: Stream) -> &'static str {
    match stream {
        Stream::Stdout => "stdout",
        Stream::Stderr => "stderr",
    }
}

/// Returns `bytes` as a value: the UTF-8 text that they hold, escaped as [`escape`] escapes it, and
/// each other byte as `\xHH`, with two hexadecimal digits.
fn escape_bytes(bytes: &[u8]) -> String {
    let mut escaped = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        escaped.push_str(&escape(chunk.valid()));
        for byte in chunk.invalid() {
            escaped.push_str(&format!("\\x{byte:02x}"));
        }
    }
    escaped
}

/// Reattaches to the VM of `locked`, with the runtime identity of `runtime`, and prints what it
/// found. Returns the VM, or the status with which the command ends once it finds the VM
/// mismatched, which it leaves running.
fn hold(locked: &mut LockedStore, runtime: &Runtime) -> Result<Result<Vmm, ExitCode>, String> {
    let reattached =
        supervisor::reattach(locked, &runtime.identity).map_err(|error| describe(&error))?;
    let vmm = match reattached {
        Reattached::Live(vmm) => {
            println!("reattached=live");
            vmm
        }
        Reattached::Mismatched { vmm, differences } => {
            println!("reattached=mismatched");
            println!("differences={}", names(&differences));
            println!("pid={}", vmm.record().process().pid);
            return Ok(Err(ExitCode::from(MISMATCHED)));
        }
        Reattached::Exited { .. } => {
            println!("reattached=exited");
            return Err("no VM of the sandbox runs".to_owned());
        }
        Reattached::Indeterminate(reason) => {
            println!("reattached=indeterminate");
            return Err(format!(
                "whether the VM of the sandbox runs cannot be determined: {reason}"
            ));
        }
    };
    println!("pid={}", vmm.record().process().pid);
    Ok(Ok(vmm))
}

/// Reads the outcome report of the VM of `store`, prints it, and checks that the VM ended with
/// the category `category`, the status code `status`, and a complete teardown.
fn check_outcome(store: &SandboxStore, category: &str, status: i64) -> Result<(), String> {
    let outcome = report::read(&store.outcome_path()).ok_or_else(|| {
        format!(
            "the VM left no outcome report {}",
            store.outcome_path().display()
        )
    })?;
    print_outcome(&outcome);
    if (
        outcome.category.as_deref(),
        outcome.status_code,
        outcome.teardown_complete,
    ) != (Some(category), Some(status), Some(true))
    {
        return Err(format!(
            "the VM's outcome is not {category} {status} with a complete teardown"
        ));
    }
    Ok(())
}

/// Prints the VM's `outcome` report: its category and status code, and whether its teardown is
/// complete.
fn print_outcome(outcome: &Report) {
    let found = outcome.category.as_deref().unwrap_or("none");
    let code = outcome
        .status_code
        .map_or("none".to_owned(), |code| code.to_string());
    println!("outcome={found} {code}");
    println!(
        "teardown={}",
        match outcome.teardown_complete {
            Some(true) => "complete",
            Some(false) | None => "incomplete",
        }
    );
}

/// Returns the runtime of NVX's artifacts in the directory that `NVX_ARTIFACTS_DIR` names, with
/// the guest agent's build `guest_build`.
fn runtime(guest_build: &str) -> Result<Runtime, String> {
    let directory = env::var_os(Artifacts::ENV_DIR)
        .filter(|directory| !directory.is_empty())
        .ok_or_else(|| {
            format!(
                "set {} to NVX's artifacts: a repository checkout or an extracted release",
                Artifacts::ENV_DIR
            )
        })?;
    Runtime::from_dir(directory, guest_build).map_err(|error| describe(&error))
}

/// Connects to the control endpoint of the VM `vmm`, and attaches a session with `capability`,
/// by `deadline`.
fn connect(
    vmm: &Vmm,
    capability: &[u8; CAPABILITY_LEN],
    deadline: Instant,
) -> Result<Client, String> {
    let process = vmm.record().process();
    let transport = session::connect(vmm.endpoint(), process.pid, process.start_time, deadline)
        .map_err(|error| format!("cannot connect to the VM: {}", describe(&error)))?;
    attach(transport, capability, deadline)
}

/// Attaches a session over `transport` with `capability` by `deadline`, and prints its identity.
fn attach(
    transport: Box<dyn nvx::host::platform::Transport>,
    capability: &[u8; CAPABILITY_LEN],
    deadline: Instant,
) -> Result<Client, String> {
    let client = Client::attach(transport, capability, deadline)
        .map_err(|error| format!("cannot attach to the VM: {}", describe(&error)))?;
    print_identity(&client);
    Ok(client)
}

/// Prints the identity of the session that `client` attached: its broker instance, in
/// hexadecimal, and its epoch.
fn print_identity(client: &Client) {
    let identity = client.identity();
    let instance: String = identity
        .instance_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    println!("instance={instance}");
    println!("epoch={}", identity.epoch);
}

/// Returns the names of `parts`, separated by commas.
fn names(parts: &[IdentityPart]) -> String {
    parts
        .iter()
        .map(|part| part.name())
        .collect::<Vec<_>>()
        .join(",")
}

/// Describes `error` with the causes that its description does not end with already.
fn describe(error: &dyn Error) -> String {
    let mut description = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_description = cause.to_string();
        if !description.ends_with(&cause_description) {
            description = format!("{description}: {cause_description}");
        }
        source = cause.source();
    }
    description
}
