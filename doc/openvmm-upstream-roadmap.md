# OpenVMM Micro-VM PR Strategy

## Scope

- OpenVMM fork base: `b2676222b383bf50c7022f6a5983920421469a47`, upstream main on 2026-10-31.
  The `feature/microvm/release/v3.0` reorganization, which NVX adopted in `6e7dcec4d`, moved
  the chain there from `27fe0c64f02e08e0495b64fe5267ecc7d2f94c70`; see
  [Upstream status](#upstream-status).
- Mapped fork tip: `82686c983907738d44a6eb9e3322589b573c0074`, position 144. The graph, PR
  summary, and commit mapping below cover positions 001-144 and the PR20 fixup at
  position 161.
- NVX's pinned OpenVMM revision: `d891dfd9d9c9243f045069f5f077dbf5e13f9a4c`, position 201. Position
  201, which microsoft/openvmm#4839 proposes for `feature/microvm/dev`, is one commit on position
  200, `0ac254109`, the head of the `feature/microvm/release/v3.0` branch of microsoft/openvmm, to
  which `feature/microvm/dev` also points. The v3.0 reorganization regrouped the 385 commits of
  `feature/microvm/release/v2.4` that upstream had not landed into 200 commits: it folded
  fixups into the commits that they fix and combined the commits after the mapped tip into commits
  by concept, without changing what the chain adds to upstream; see
  [The v3.0 reorganization](#the-v30-reorganization). The pin is 57 commits past the mapped tip
  on the same first-parent chain. Positions 145-201, except the PR20 fixup at position
  161, are not yet assigned to PRs; see
  [Commits after the mapped tip](#commits-after-the-mapped-tip).
- Upstream main: `b2676222b383bf50c7022f6a5983920421469a47` (2026-10-31), the fork base.

## Upstream status

The `feature/microvm/release/v3.0` reorganization moved the chain of
`feature/microvm/release/v2.4` from `27fe0c64f` onto `b2676222b`, two upstream commits later.
Upstream had landed v2.4 position 016 in a revised form as microsoft/openvmm#4583, so the
reorganization dropped it. microsoft/openvmm#4829 makes cancelled `pal_async` event waits
reusable and changes no file of the chain except `Cargo.lock`, where it adds a line that the chain
does not touch. Replaying the remaining 385 commits had no conflicts. The reorganization then
regrouped them into 200 commits without changing the tree: the pin's tree, `652f88f95`, is
the tree of that replay, the rebase of `feature/microvm/release/v2.4` onto `b2676222b` without
position 016; see [The v3.0 reorganization](#the-v30-reorganization).

The `feature/microvm/release/v2.4` rebase moved the 380-commit chain of
`feature/microvm/release/v2.3` from `22c0485be` onto `27fe0c64f`, 76 upstream commits later. It
dropped the ten commits whose changes upstream had landed, v2.3 positions 003, 005, 009, 012,
015, 017, 023, 030, 031, and 036, and resolved conflicts at 45 positions, which
[Conflicts of the v2.4 rebase](#conflicts-of-the-v24-rebase) lists at their v3.0 positions. It
added sixteen fixups at its tip, v2.4 positions 371-386, which adapted the chain to upstream
changes that no textual conflict exposed; the v3.0 reorganization folded them into the commits
that they fix.

The `feature/microvm/release/v2.3` rebase moved the 334-commit chain of
`feature/microvm/release/v2.2` from `790132851` onto `22c0485be`, 23 upstream commits later. It
dropped position 074, whose change microsoft/openvmm#4661 had landed upstream, resolved seven
commits that conflicted, and added four fixups at its tip, v2.4 positions 324-327, which the
v3.0 reorganization folded too.

The `microvm/v2.2` rebase moved the 320-commit chain of `microvm/v2.1` from `e1cdbd916` onto
`790132851`, 75 upstream commits later. It dropped the eight fork commits that had landed
upstream in between and adapted the remaining 312 to upstream API changes where they needed it,
which changed the diffs of 50 of them.

### Landed upstream

| Dropped by | Former position | Fork commit | Roadmap PR | Upstream PR | Upstream commit |
| --- | ---: | --- | --- | --- | --- |
| v3.0 | 016 | `f5231b2d2` - chipset: preserve IOAPIC line levels across restore | PR06 | microsoft/openvmm#4583 | `db4331e53` |
| v2.4 | 003 | `0213cf65d` - flowey: skip reinstalling an identical, possibly running cargo-nextest binary | PR01 | microsoft/openvmm#4608 | `03a2fc597` |
| v2.4 | 005 | `a91dc86fb` - lxutil: tolerate inherited extended attributes in tests | PR01 | microsoft/openvmm#4587 | `a1d71b899` |
| v2.4 | 009 | `15a895dd8` - sparse_mmap: add private copy-on-write file views | PR02 | microsoft/openvmm#4557 | `d8acf2a7c` |
| v2.4 | 012 | `4a7ab405c` - pal: add Unix extent cloning and sparse-extent seeking | PR03 | microsoft/openvmm#4590 | `c62489737` |
| v2.4 | 015 | `074de3406` - pal: add sparse-file range queries, zeroing, and block cloning | PR04 | microsoft/openvmm#4596 | `8f1c36ba5` |
| v2.4 | 017 | `50460c27a` - pal: query named-pipe peers and create protected pipes | PR04 | microsoft/openvmm#4582 | `65db52693` |
| v2.4 | 023 | `938b14343` - chipset: fast-forward periodic PIT counting over long intervals | PR06 | microsoft/openvmm#4586 | `023329997` |
| v2.4 | 030 | `135c503ba` - consomme: cover the pending DNS request limit with a test | PR07 | microsoft/openvmm#4601 | `cd93f761f` |
| v2.4 | 031 | `90734a2ff` - consomme: cover IPv4 fragment rejection with a test | PR07 | microsoft/openvmm#4602 | `3d9cb006a` |
| v2.4 | 036 | `6794f12d0` - openvmm_entry: skip raw console mode when stdin is not a terminal | PR08 | microsoft/openvmm#4606 | `769f2f61f` |
| v2.3 | 074 | `f6e6ff738` - loader_defs: name the boot_params ACPI RSDP field | PR14 | microsoft/openvmm#4661 | `8beae2961` |
| v2.2 | 002 | `607e6c6a2` - nextest: scope a longer budget to the exhaustive ring-buffer loom test | PR01 | microsoft/openvmm#4584 | `9501bb481` |
| v2.2 | 017 | `c61d77644` - pal: create files and directories with explicit security descriptors | PR04 | microsoft/openvmm#4599 | `9fbf965c5` |
| v2.2 | 021 | `88865bf90` - virt_mshv: describe SNP guest workarounds without ACI-specific names | PR05 | microsoft/openvmm#4580 | `40cff1268` |
| v2.2 | 022 | `9b3ce6760` - kvm: preserve pending external interrupts across VP-state capture | PR05 | microsoft/openvmm#4522 | `392890680` |
| v2.2 | 029 | `947ae858b` - virtio: make interrupt tests wait for queue completion | PR06 | microsoft/openvmm#4592 | `5b4656a86` |
| v2.2 | 030 | `21c2206c9` - fuse: rate-limit guest-triggerable diagnostics | PR06 | microsoft/openvmm#4581 | `926827446` |
| v2.2 | 035 | `54c5c34cd` - virtio_console: rate-limit worker errors | PR06 | microsoft/openvmm#4614 | `77ce2aeac` |
| v2.2 | 039 | `dd349021e` - openvmm_entry: accept `delay:<ms>:<disk>` disk wrappers | PR08 | microsoft/openvmm#4610 | `6e0e4fdab` |

A former position counts in the branch that the rebase started from: `microvm/v2.1` for the v2.2
rebase, `feature/microvm/release/v2.2` for the v2.3 rebase, `feature/microvm/release/v2.3` for
the v2.4 rebase, and `feature/microvm/release/v2.4` for the v3.0 reorganization.
microsoft/openvmm#4661 names the `acpi_rsdp_addr` field of `boot_params` exactly as position 074
did, so the v2.3 rebase dropped 074 as empty. microsoft/openvmm#4554 still carries it, ahead of
positions 064-066.

microsoft/openvmm#4583 saves each IOAPIC input's last observed level as edge-detector history, as
v2.4 position 016 did, but restores the redirection entries, routes, and input history without
evaluating the inputs, which 016 did. A restored asserted level-triggered input therefore raises
its interrupt when its producer republishes it, when the guest's EOI clears the remote IRR, or
when the guest writes its redirection entry, not during the restore itself.

Upstream merged most of the v2.4 drops after review changes, so the v2.4 rebase took upstream's
versions and adapted the later commits to them. microsoft/openvmm#4557 creates a copy-on-write
mappable without the chain's `executable` flag, and on Windows it refuses to unmap or map over
part of a written copy-on-write view. microsoft/openvmm#4582 answers a pipe's peer process
through `PipeExt::get_pipe_peer_process_id` instead of the chain's `pal::windows::pipe::peer`
module, which also had `is_pipe`. microsoft/openvmm#4586 fast-forwards more counting modes, and
microsoft/openvmm#4606 forwards console input in `repl.rs` instead of the `headless` module, which
position 130 creates for the chain's own use, and ends the run when a stdin that is not a
terminal ends, which position 130 confines to machines other than the microVM.
microsoft/openvmm#4587 is identical to v2.3 position 005.

microsoft/openvmm#4522 fixes the lost-PIC-interrupt bug of v2.1 position 022 in a different way.
Instead of tracking the queued vector in `kvm::VpRunner`, it injects PIC interrupts through the
vCPU's injected-interrupt slot (`KVM_SET_VCPU_EVENTS`), which the existing activity save/restore
already covers. Its saved state has no slot for v2.1 position 022's queued vector: KVM snapshots
from builds with position 022 persist a queued PIC vector as a pending `ExtInt` event. Current
builds reject restoring one with `KvmError::InvalidState`, so recapture KVM snapshots that those
builds captured.

Upstream also landed parts of commits that remain in the chain:

- microsoft/openvmm#4556 (`96647c331`) derives `Clone` for `ProtobufMessage` and `ProtobufAny`.
  Position 006 keeps its subject but now only adds `ProtobufAny::value_len` and checks a
  clone in the round-trip test.
- microsoft/openvmm#4524 (`9cf109547`) adds the generic `PartitionTimeControl` contract, MSHV
  and WHP implementations, and full-VM freeze/thaw lifecycle calls. Position 041 reuses the
  MSHV implementation, through fallible freeze and thaw helpers, instead of private plumbing.
  Positions 039-043 keep the exact TSC-frequency exposure, snapshot downtime handling,
  and restored-vCPU synchronization that upstream lacks, and position 110 synchronizes only
  the VPs that a restore creates.
- microsoft/openvmm#4704 (`1c093df84`) adds the architectural `TscDeadline` VP state element,
  the `tsc_deadline` partition capability, and the KVM, MSHV, WHP, and OpenHCL register
  accessors that v2.4 position 038 also added. Position 038 now uses upstream's, so it keeps
  the `tsc_deadline` field of the CPU compatibility contract, the finalized MSHV partition's
  report of no TSC-deadline support, and the LAPIC timer helpers that position 160 deletes
  in favor of those that position 152 adds for the time ABI. Upstream compares a restored
  deadline on partitions that can freeze time, as WHP can, whereas 038 never compared it, so the
  WHP state access of position 042, which reads the deadline before the APIC state,
  discards that read when it restores the deadline.
- microsoft/openvmm#4764 (`29f0adceb`) makes every lxutil volume resolve paths beneath its root
  without following a host symbolic link, as position 143 did for a confined volume, and
  microsoft/openvmm#4783 (`7c92b147e`) rejects unsafe opens. Position 143 now only adds
  `LxVolumeOptions::confine_paths`, which makes a Windows volume create LX symlinks, and runs its
  confinement tests against upstream's implementation. A share therefore needs `openat2`,
  Linux 5.6 or later, where the chain fell back to opening each component with `O_NOFOLLOW`.

### Open upstream pull requests

As of 2026-10-31, every commit of PR01-PR09 and PR14 except positions 001, 006, and
032 is in an open upstream pull request; the eleven commits that upstream has since merged
are under [Landed upstream](#landed-upstream). Each pull request carries one commit, except
microsoft/openvmm#4554, which carries positions 064-066 after the `boot_params` field
that microsoft/openvmm#4661 has since landed; its ELF and initrd validation was split out during
review and landed as microsoft/openvmm#4667 (`a87971184`). Six of the 31 pull requests are
drafts, microsoft/openvmm#4607, microsoft/openvmm#4619, microsoft/openvmm#4622,
microsoft/openvmm#4623, microsoft/openvmm#4625, and microsoft/openvmm#4626, and none has an
approving review yet. microsoft/openvmm#4451, which proposed position 001, was closed without
merging, and neither the remainder of position 006 nor position 032, which the v3.0
reorganization moved into PR09, has been proposed. PR10-PR13 and PR15-PR27 have no upstream pull
requests yet.

| Roadmap PR | Positions | Upstream pull requests, in position order |
| --- | ---: | --- |
| PR01 | 002-005 | microsoft/openvmm#4595, microsoft/openvmm#4618, microsoft/openvmm#4598, microsoft/openvmm#4615 |
| PR02 | 007 | microsoft/openvmm#4558 |
| PR03 | 008-009 | microsoft/openvmm#4589, microsoft/openvmm#4591 |
| PR04 | 010-012 | microsoft/openvmm#4593, microsoft/openvmm#4604, microsoft/openvmm#4585 |
| PR05 | 013-015 | microsoft/openvmm#4536, microsoft/openvmm#4607, microsoft/openvmm#4613 |
| PR06 | 016-020 | microsoft/openvmm#4588, microsoft/openvmm#4594, microsoft/openvmm#4597, microsoft/openvmm#4605, microsoft/openvmm#4609 |
| PR07 | 021 | microsoft/openvmm#4600 |
| PR08 | 022-025 | microsoft/openvmm#4616, microsoft/openvmm#4617, microsoft/openvmm#4620, microsoft/openvmm#4621 |
| PR09 | 026-031 | microsoft/openvmm#4619, microsoft/openvmm#4622, microsoft/openvmm#4623, microsoft/openvmm#4624, microsoft/openvmm#4625, microsoft/openvmm#4626 |
| PR14 | 064-067 | microsoft/openvmm#4554 (064-066), microsoft/openvmm#4611 |

Stacked pull requests carry their prerequisites' commits: microsoft/openvmm#4585 built on
microsoft/openvmm#4582, which has merged, and microsoft/openvmm#4609 builds on
microsoft/openvmm#4585; microsoft/openvmm#4613 builds on microsoft/openvmm#4536; and
microsoft/openvmm#4622, microsoft/openvmm#4623, and microsoft/openvmm#4625 build on
microsoft/openvmm#4619, as microsoft/openvmm#4626 builds on microsoft/openvmm#4625. Most of these
pull requests have gained review commits that the fork chain does not carry. When one merges, the
next rebase should drop its fork commit, as the `microvm/v2.2`, `feature/microvm/release/v2.3`,
and `feature/microvm/release/v2.4` rebases and the v3.0 reorganization dropped the twenty above,
and adapt later commits to the merged form.

The v3.0 reorganization folded fixups into positions 007, 014, 015,
016, 026, 030, and 067, whose pull requests lack them; see
[The v3.0 reorganization](#the-v30-reorganization).

### The v3.0 reorganization

The reorganization regrouped the chain without changing what it adds to upstream. Fixups fold into
the commits that they fix, apart from the few that the list after the table names. A fixup that
fixes several commits splits by file, or by hunk or line within a file, so that each commit carries
its own part: partition scrub's resets name each field in the commit that adds it, and each microVM
option test's x86_64 gate goes with the commit that adds the test. Where the code that a fixup
changes differs in the commit that it fixes, the fold re-expresses the fixup for that code and
carries it through the later commits: v2.4 position 309's transmit loop replaces position
058's one-packet transmit path before position 117 adds the egress check to it, position
038 uses upstream's `TscDeadline` and its WHP accessors from the start, and the MSHV tests of
positions 041, 147, 157, and 164 call upstream's `partition_create_args` with
its creation options from the commits that add them. The table lists where each fix went.

| v2.4 position | Fix | v3.0 positions |
| ---: | --- | --- |
| 141 | `77394aa60` - virt_mshv: synchronize restored TSCs only on created VPs | 110 |
| 142 | `f8f122375` - openvmm_core: instantiate only the requested VP prefix on MSHV restore | 110, 137 |
| 144 | `03dbd8e8a` - openvmm_entry: compile connect_serial without the RPC features | 094 |
| 145 | `33bc51c01` - openvmm_entry: gate RPC-only microVM helpers behind the RPC features | 137 |
| 148 | `e41a95b1b` - openvmm_entry: flush portb output before a guest-requested snapshot | 132, 137, 138 |
| 213 | `f81f3c54d` - state_unit: advance time only in the units that keep their own | 033, 034 |
| 309 | `a239e34fc` - net_tap: write every packet of a transmit batch | 058, 117 |
| 310 | `edc0f89a2` - vmm_tests: poll portb when the ttrpc microVM guest has no hvc0 | 137 |
| 311 | `ec2c26920` - virt_mshv: initialize the ExtINT flag only for x86_64 guests | 015 |
| 312 | `9d332bb8c` - virt_whp: query the TSC frequency for the aarch64 hv1 emulator | 042 |
| 313 | `c8eb19272` - virt_whp: build the deferred-registration fault path for x86_64 only | 052 |
| 314 | `8d3bcb9df` - openvmm_core: gate the microVM's x86_64-only items for aarch64 guests | 075, 076, 080, 152, 172 |
| 315 | `1ca441da2` - openvmm_core: reject MP-table boot for aarch64 guests | 075 |
| 316 | `96658eb2c` - openvmm_entry: gate the microVM checks of x86_64-only options | 077, 096 |
| 317 | `9a0c10c11` - microVM: fix the macOS clippy build | 047, 077, 105, 130, 145, 180 |
| 318 | `9704505d8` - virt_hvf: name the ID register type in the policy test | 162 |
| 319 | `bd0bf48f8` - openvmm_entry: make the fatal error message test backtrace-independent | 152 |
| 320 | `97e43dc27` - openvmm_entry: run the microVM option tests only for x86_64 guests | 077, 104, 106, 112, 119, 121, 130, 133, 177, 179, 180, 181, 182, 183, 184, 187, 188 |
| 321 | `257290a47` - vmm_tests: run the ttrpc microVM test only on x86_64 hosts | 137 |
| 324 | `442f69804` - virt: adopt upstream's TscDeadline VP state element | 038, 042, 160 |
| 325 | `7d19d47f6` - virt_mshv: adapt the microVM partition to partition scrub | 014, 015, 110, 147, 156 |
| 326 | `1f856cc8c` - vmm_core: build the ACPI builder tests with level-triggered IRQs | 067 |
| 327 | `eb3b80b3a` - underhill_core: make the hardware reseal unit's start fallible | 026 |
| 328 | `1887a8370` - sparse_mmap: flush writable file views whole, in one ascending pass | 007, 089 |
| 330 | `5ebf53bdb` - virtio_console: time host authentication only while host input is read | 126, 130 |
| 334 | `6057b9e3c` - virtiofs: factor the integration test harness for input-gate tests | 186 |
| 335 | `6af3a4d49` - virtiofs: wait for request admission instead of ending a queue worker | 101, 103, 108, 186 |
| 371 | `2b10020f8` - state_unit: pass upstream's dependency sets to the chain's transactions | 030, 033 |
| 372 | `0ae3fc7c5` - virtio: await the PCI transport's save and restore in saved-state tests | 053 |
| 373 | `6a5a6f40d` - virt_mshv: adapt the chain to upstream's nested virtualization | 014, 041, 147, 157, 160, 164 |
| 374 | `0fff37663` - serial_socket, openvmm_entry: use upstream's named-pipe peer API | 123, 130 |
| 375 | `f0432967b` - openvmm: create copy-on-write mappables with upstream's signature | 048, 050, 139 |
| 376 | `f04b01d5f` - openvmm_entry: keep RX GSO off on the microVM NIC | 096 |
| 377 | `d37530633` - openvmm_entry: accept one microVM console in virtio_consoles | 137, 175 |
| 378 | `db5cbc830` - virtio_blk: import what the blocking-write test disk needs | 016 |
| 379 | `6206bb816` - net: enable no receive offloads in the chain's endpoint tests | 058, 116, 117, 188 |
| 380 | `43c45af53` - petri: adapt upstream's Incubator changes to the chain's petri | 055, 078 |
| 381 | `f0b5b7f84` - openvmm_entry: name Path through the RPC module's import in a test | 137 |
| 382 | `b2bab5bbc` - vmm_tests: give the backend contract tests the chain's partition options | 052, 145 |
| 383 | `b06fbd19f` - ci: regenerate the workflows from upstream's pipeline changes | 140 |
| 384 | `647171110` - virtio_console: attach the broker's replacement host on its own connection | 126 |
| 385 | `967da81f2` - openvmm_entry: keep a microVM running past the end of its stdin | 130 |
| 386 | `998b9f240` - vmm_tests: finalize the backend contract tests' partition memory | 014 |

Some changes stay apart from the commits that they fix, or moved:

- v2.4 position 329 fixes PR20's position 110, but it changes the platform tier check that
  position 160 rewrites for manifest version 6, so it follows that commit as position
  161. Folding it needs the fix re-expressed for 110's version of the check. Its test helper
  also clears the scratch restore mode that position 180 adds, so that statement moved into
  position 180.
- v2.4 position 143 restores listener endpoint rebinding for both the boot listener of PR18 and
  the control listener of PR23, and it builds on PR23's position 130, so it stays a commit of
  its own, position 131, at the end of PR23.
- v2.4 position 335's integration tests drive a microVM queue through the microVM share API that
  positions 183 and 184 complete, so they and the test harness of v2.4 position 334
  follow position 185 as position 186, while 335's admission wait folds into
  position 101.
- v2.4 position 137 gives upstream's power-exit test of the management RPC server the field that
  v2.4 position 136 adds, so that line moved into position 137.
- v2.4 position 301, which stops the devices before the VM worker tears down the partition and
  guest memory, changes only upstream code, so it moved into PR09 as position 032; v2.4
  position 308, which runs the check-in gates on the `feature/microvm` branches, joined the fork
  tail as position 141.

The v2.4 commits after the mapped tip, apart from the fixes that folded into earlier commits,
became the 56 commits that [Commits after the mapped tip](#commits-after-the-mapped-tip) lists, by
concept: the time ABI v1 series and its CPU profiles, the later CPU profiles, VM time, the time ABI
checks, time ABI 2, and the host filesystem, storage, network, device DMA, and command-line
features. Each combines a series of commits that revised one another. Combining a series kept
apart the commits that can go upstream on their own: positions 171, 191, and
196.

Upstream's backend contract test fixture, which microsoft/openvmm#4705 added after position
090 was written, builds a partition configuration. Position 090 adds
`versioned_cpu_contract` to that configuration and position 160 removes it, so the fixture
sets the field in the commits between them. It is the only change that the reorganization made for
its own sake, and it leaves the pin's tree unchanged.

Every commit of the v3.0 chain builds: `cargo check --locked --workspace --all-targets` passes
at each one on x86_64 Linux and x86_64 Windows. Before the reorganization, most of the chain's
intermediate commits did not build on the current upstream base, because the tip's fixups
adapted them only at the end: from v2.4 position 038 on, for example, `virt` defined
`TscDeadline` twice.

#### Conflicts of the v2.4 rebase

The `feature/microvm/release/v2.4` rebase replayed the chain onto `86427897a`, the upstream main
of 2026-10-31, with conflicts at 45 positions, which it resolved in place. It merged `Cargo.lock`
by keeping both sides' entries, checked with `cargo metadata --locked`. When upstream main gained
microsoft/openvmm#4587, a second pass moved the chain onto `a1d71b899` without its position 005.
When upstream main then gained microsoft/openvmm#4833, which fixes the build of its `net_dio` test
exactly as a fixup at the chain's tip had, a third pass moved the chain onto `27fe0c64f` without
that fixup. Neither pass had conflicts, and neither changed the tree. The table lists each
resolution at the v3.0 position that now holds it.

| Position | v3.0 commit | v2.4 commit | Upstream PR | Resolution |
| ---: | --- | --- | --- | --- |
| 008 | `d0bb3db2a` | 008 `4ef8c34b4` - pal: bound Linux descriptor-table pre-expansion | microsoft/openvmm#4590 | Declares the commit's `fd_table` module next to upstream's `fs` module |
| 010 | `1a8bc4363` | 010 `167b61f99` - pal: add handle-relative file access that does not follow reparse points | microsoft/openvmm#4596 | Declares the commit's `relative` module next to upstream's `sparse` module |
| 014 | `8a7dea926` | 014 `6557e809a` - virt: add Partition::finalize_memory; create the MSHV BSP after finalization | microsoft/openvmm#4753 | Keeps upstream's nested-virtualization check, and the capabilities that finalization builds take `nested_virt` from a new `CreationConfig` field |
| 016 | `47102941e` | 017 `c88f61b52` - virtio_blk: let stop cancellation win over a ready queue and drain accepted I/O | microsoft/openvmm#4751 | Upstream's tests take `disk_hook` instead of `disklayer_ram`, and the commit's blocking-write test keeps `parking_lot` and adds `async-channel`; fixup 378 restores the imports that its test disk needs |
| 017 | `41460f3f7` | 018 `784f589c5` - virtiofs: make handle and inode ID allocation fallible | microsoft/openvmm#4764, microsoft/openvmm#4783 | `open` takes upstream's validated open options with the fallible handle insert, and `InodeMap::insert` stays fallible after upstream's eviction of a dedup key whose file type changed |
| 018 | `3f6941671` | 019 `ddb756a73` - virtiofs: bound guest payload allocations and rate-limit diagnostics | microsoft/openvmm#4783 | `virtiofs` depends on both upstream's `thiserror` and the commit's `tracelimit` |
| 021 | `6425cf88a` | 022 `4388e26cd` - consomme: bound active TCP, UDP, and ICMP flows | microsoft/openvmm#4602 | Declares the commit's `limits` test module next to upstream's `fragments` module |
| 025 | `c1bd1e0a5` | 026 `c9f5240b5` - virtio_console: move the direct receive loop into its own module | microsoft/openvmm#4778 | The moved receive loop takes upstream's disconnect detection, which polls for a disconnect without a receive queue and while it waits for one |
| 026 | `fa02eed4b` | 027 `7fcfadd68` - state_unit: make unit start fallible and roll back partial starts | microsoft/openvmm#3846 | The rollback of a partial start waits on upstream's ordered dependency sets, and upstream's edge-pruning test unwraps the fallible start |
| 028 | `7496e02e8` | 029 `b96679a2e` - openvmm: make VM resume fallible | microsoft/openvmm#4468 | The fallible resume keeps upstream's `resume_vm` OpenTelemetry span |
| 038 | `876e1a7ca` | 038 `5c2580b5d` - virt: add portable TSC-deadline VP state and APIC-timer advance | microsoft/openvmm#4753 | The finalized capabilities set both upstream's `nested_virt` and the commit's `tsc_deadline` |
| 046 | `fd5eb056e` | 046 `223416953` - openvmm_helpers: publish snapshots atomically through durable staging | microsoft/openvmm#4732 | `Cargo.lock` lists both upstream's `pal_async` and the commit's `pal` for `openvmm_helpers` |
| 051 | `e7aaeab26` | 051 `49630526a` - openvmm: signal restore readiness to a local socket or pipe | microsoft/openvmm#4606 | Declares the commit's `launch` and `restore_ready` REPL modules without the `headless` module of v2.3 position 036, which upstream's version of that change does not have |
| 053 | `3d11ed01e` | 053 `d426451b2` - virtio: add device-private saved state with deferred activation and serialized kicks | microsoft/openvmm#4799 | The PCI transport's saved state adds the commit's `device_state` to upstream's explicit config-space save, and `save_restore.rs` keeps upstream's async adapter tests with the commit's test |
| 055 | `569c31998` | 055 `125e5a5a6` - virtio_console: add attachment identities and reconnect and disconnect policies | microsoft/openvmm#4778 | Keeps the imports of upstream's disconnect detection with the commit's task control, and the crate documentation describes both |
| 056 | `a29e7173e` | 056 `f9e0903e5` - virtio_console: persist buffered direct-console I/O | microsoft/openvmm#4778, microsoft/openvmm#4532 | The staged receive loop polls upstream's disconnect detection without a receive queue and while staged input waits for a guest buffer; the snapshot guides list upstream's virtio-rtc and the commit's virtio-console |
| 058 | `4931d8ab0` | 058 `3059fd408` - net_tap: quiesce queues and retain pending transmit ownership | microsoft/openvmm#4545 | `TapQueue` keeps upstream's receive-offload state and read buffer with the commit's transmit state and input quiescing, and both sides' tests |
| 061 | `0afebc9dd` | 061 `ebfd28f5b` - virtio_net: persist drained queue state for static-identity NICs | microsoft/openvmm#4545, microsoft/openvmm#4792 | virtio-net carries both upstream's `enable_rx_gso` and the commit's `save_restore`; petri adds NICs with RX GSO off next to upstream's private virtio devices, and the test harness gives upstream's mock endpoint its new channels |
| 063 | `be2ec6c40` | 063 `63ccb7158` - virtiofs: track inode aliases through link, rename, and unlink | microsoft/openvmm#4764, microsoft/openvmm#4783 | `InodeMap` keeps upstream's dedup-key eviction next to the commit's alias methods, and the commit's dedup-key rebuild keeps the newest inode per key so as not to undo it |
| 077 | `d386b6bfe` | 077 `a92e6d272` - openvmm_entry: expose `--machine microvm` | microsoft/openvmm#4766 | Upstream's `open_vmgs` replaces the VMGS destructuring, after the commit's microVM VMGS check |
| 078 | `310048ce1` | 078 `d83592d5d` - petri, vmm_tests: add a microVM builder, portb harness, and cold-boot test | microsoft/openvmm#4665, microsoft/openvmm#4792 | Pipette runs as init when both upstream's conditions and the machine profile allow it; the builder keeps upstream's new methods with the commit's, and the microVM command line takes upstream's `rdinit` |
| 087 | `2fccba420` | 087 `5fc3f7c3f` - openvmm_helpers: define the authoritative microVM machine contract | microsoft/openvmm#4732 | `Cargo.lock` lists both upstream's `pal_async` and the commit's `sha2` for `openvmm_helpers` |
| 097 | `619ce0e45` | 097 `adea4e3ed` - openvmm: persist the microVM NIC and restore a fresh endpoint generation | microsoft/openvmm#4532 | The snapshot format guide keeps upstream's virtio-rtc with the commit's microVM NIC example |
| 099 | `d0b79eaad` | 099 `259117fbb` - virtiofs: add the fixed microVM filesystem profile | microsoft/openvmm#4764, microsoft/openvmm#4783 | The strict-path volume builds on upstream's constructor, and inode `open` keeps the commit's confinement check but no longer adds `O_NOFOLLOW`, which upstream's `LxVolume::open` rejects and applies itself |
| 101 | `f11af4bb1` | 101 `67b3d7887` - virtiofs: persist microVM FUSE namespaces, handles, and directory cookies | microsoft/openvmm#4783 | A restored handle reopens with upstream's validated host options, without the guest-only flags that upstream's `LxVolume::open` applies itself or rejects |
| 117 | `65e32fee9` | 117 `e9e0fdeb6` - net_tap: enforce the egress policy on TAP transmit | microsoft/openvmm#4545 | TAP endpoints and queues carry upstream's receive-offload state with the commit's egress policy, and the tests run both sides' trials |
| 118 | `906bd0de9` | 118 `be3595b94` - virtio_net: install and prefilter the egress policy | microsoft/openvmm#4545, microsoft/openvmm#4746 | The adapter carries upstream's `enable_rx_gso` with the commit's egress policy, and `TxPacketError` adds the commit's variants without the one that upstream removed |
| 120 | `d6ee19e65` | 120 `8b19f06ea` - virtiofs: enforce denied subtrees by object identity | microsoft/openvmm#4764 | The strict-path constructor builds on upstream's `VirtioFsVolume::new` and sets the commit's denied-subtree fields |
| 130 | `5ec16d5ee` | 130 `d0c81d202` - openvmm_entry: add authenticated microVM control-console endpoints | microsoft/openvmm#4606 | Creates the `headless` REPL module of v2.3 position 036, which upstream's version of that change does not have, without its raw-mode helpers; the documentation sync heuristics take upstream's list with the commit's entries |
| 137 | `ddd66b91c` | 136 `1b85b7165` - vmservice: create, capture, and restore microVMs over the management RPC | microsoft/openvmm#4752, microsoft/openvmm#4753, microsoft/openvmm#4766 | `VMConfig` keeps upstream's `vmgs` and moves upstream's `nested_virt` to field 23, since the chain uses field 20; `CreateVm` prepares a microVM and rejects its VMGS before it builds upstream's VMGS, PCIe topology, and MMIO overrides |
| 138 | `8bf88a785` | 137 `1f1edd232` - ttrpc: propagate nonzero guest exit status | microsoft/openvmm#4785 | Upstream's server returns the guest's exit status at once; the commit's handler still marks the VM halted and answers a pending `WaitVM`, successfully only for status 0, and a failed output drain still ends the server with its error |
| 139 | `83f0740aa` | 138 `dbcfce404` - openvmm_helpers: add a publication and private-restore benchmark | microsoft/openvmm#4732 | Dev-dependencies keep upstream's `pal_async` with the commit's `sparse_mmap` |
| 142 | `1be1c24d3` | 140 `ba4e62fe0` - vmservice: keep NVX wire-tag compatibility | microsoft/openvmm#4696, microsoft/openvmm#4766 | `DevicesConfig` keeps upstream's repeated `virtio_consoles` at field 6, which decodes the former single field, and `VMConfig` moves upstream's `vmgs` to field 24, as `disable_hv` takes field 21 |
| 143 | `b4e47fc38` | 146 `a91864166` - lxutil: add path confinement that never follows symbolic links | microsoft/openvmm#4764, microsoft/openvmm#4783 | Upstream's lxutil never follows a host symbolic link, so the commit drops its own Unix resolver and per-component fallback, keeps `confine_paths` for Windows LX links, and runs its tests against upstream's implementation, which reports `ENOTDIR` or `ELOOP` |
| 144 | `82686c983` | 147 `f94922de6` - virtiofs: allow symlink creation on read-write microVM shares | microsoft/openvmm#4764, microsoft/openvmm#4783 | Inode `open` keeps upstream's options without `O_NOFOLLOW` and adds the commit's checks; the symlink tests accept upstream's `ENOTDIR`, and the security model says that a share needs `openat2` on Linux |
| 147 | `7047f020c` | 153 `43c78328b` - virt_mshv: implement the NVX time ABI backend | microsoft/openvmm#4753 | Partition creation runs upstream's nested-virtualization check and then the time ABI's, which never both apply, as nested virtualization requires `hv_config` and the time ABI rejects it |
| 164 | `99f06acae` | 269 `6abfbb272` - virt: serve AMD's HWCR and DE_CFG and per-VP 0x8000001E under AMD profiles | microsoft/openvmm#4753 | Keeps upstream's unimplemented-MSR action for nested virtualization before the time ABI's identity-MSR routing, which now takes the profile's vendor |
| 179 | `be760ed24` | 273 `adf000e6d` - virtiofs: add a per-subtree access policy to microVM shares | microsoft/openvmm#4764 | The strict-path volume still builds on upstream's constructor, with the policy and pinned identities that replace the denied paths, and the commit's `exchanged_path` helper precedes upstream's inode tests |
| 183 | `15f62a20d` | 283 `9b88cabd9` - microVM: share several host directories through one virtio-fs slot | microsoft/openvmm#4764 | `add_child` keeps upstream's `CreateError` and returns the commit's `EINVAL` for a microVM aggregate through it |
| 184 | `7797b12e2` | 287 `da5f32344` - microVM: export single host files as aggregate virtio-fs children | microsoft/openvmm#4764 | `VirtioFsVolume` keeps upstream's `supports_stable_file_id` next to the commit's `root_entry` |
| 184 | `7797b12e2` | 288 `c4c6e01f0` - microVM: model aggregate child kinds and exposed files explicitly | microsoft/openvmm#4764 | `VirtioFsVolume` keeps upstream's `supports_stable_file_id` next to the commit's `exposed_file` |
| 175 | `675ba0818` | 322 `7dfc57a46` - vmservice: select the microVM CPU profile over the management RPC | microsoft/openvmm#4753, microsoft/openvmm#4766 | The gRPC reference lists upstream's `nested_virt` and `vmgs` at fields 23 and 24 with the commit's `cpu_profile` at field 22 |
| 189 | `805f5d721` | 331 `b8ada6d09` - disk_zerofill: present a fixed geometry over a shorter or absent disk | microsoft/openvmm#4732 | `openvmm_resources` depends on both upstream's `disk_vmdk` and the commit's `disk_zerofill` |
| 191 | `55f9e50dd` | 340 `7601b67cf` - disk_blob: bound the time that an HTTP blob request may take | microsoft/openvmm#4732 | The storage backend table keeps upstream's `FlatVmdkDisk` row with the commit's `BlobDisk` row |
| 191 | `55f9e50dd` | 341 `fa32ac888` - disk_blob: retry HTTP blob requests that fail transiently | microsoft/openvmm#4732 | The storage backend table keeps upstream's `FlatVmdkDisk` row with the commit's `BlobDisk` row |

Replaying v2.4 position 139 also changed more than its context without a conflict: its generated
workflows merged back to the runner image and bootstrap packages that upstream had replaced,
while the pipeline sources kept upstream's. v2.4 position 383 regenerated them, and the v3.0
reorganization folded it into position 140.

microsoft/openvmm#4703 (`284127421`) resets MSHV partitions through the partition scrub ioctl
and moves partition reset into `virt_mshv`'s `lib.rs`. Positions 014, 015,
110, 147, and 156 name the fields that they add in upstream's reset, position
014 reads capabilities from the finalized partition state, and a VP reset also clears a
pending PIC ExtINT, since the PIC is reset as well.
microsoft/openvmm#4415 (`2ed16e8fc`) adds an OpenHCL hardware reseal state unit whose start
returns nothing, which position 026 makes fallible with `StateUnit::start`.

### Upstream main past the fork base

Upstream main is at the fork base, `b2676222b`, so the chain applies to it without conflicts.

## Legend

| Class | Meaning |
| --- | --- |
| U | Independently upstreamable |
| P | Upstreamable as part of an accepted microVM profile |
| N | NVX product policy; fork-only unless generalized |
| F | Fork-only |

Solid arrows in the diagram are the recommended landing/stacking order. Dashed arrows show work
that can start before the product stack reaches that PR. Purple nodes mark upstream changes that
supersede or partially overlap fork work. "In review" counts a node's commits that are in open
upstream pull requests.

## PR Dependency and Submission Graph

```mermaid
flowchart TB
    LB["Landed upstream before the base<br/>20 fork commits, dropped by<br/>the v2.2, v2.3, and v2.4 rebases and v3.0"]
    LPART["Partly superseded upstream<br/>microsoft/openvmm#4703, #4704, and #4764<br/>adopted by the commits that they overlap"]
    B["OpenVMM fork base and<br/>current upstream main<br/>b2676222b"]
    F["Recommended foundation fan-in<br/>before the microVM profile stack"]

    subgraph Independent["Parallel foundation PRs in upstream review"]
        direction LR
        P01["PR01<br/>Build/test infrastructure<br/>U - 5 commits, 4 in review"]
        P02["PR02<br/>Mesh + sparse_mmap<br/>U - 2 commits, 1 in review"]
        P03["PR03<br/>PAL on Unix<br/>U - 2 commits, 2 in review"]
        P04["PR04<br/>PAL/PAL async on Windows<br/>U - 3 commits, 3 in review"]
        P05["PR05<br/>Hypervisor fixes<br/>U - 3 commits, 3 in review"]
        P06["PR06<br/>Device hardening<br/>U - 5 commits, 5 in review"]
        P07["PR07<br/>Consomme resource bounding<br/>U - 1 commit, 1 in review"]
        P08["PR08<br/>Preparatory refactors<br/>U - 4 commits, 4 in review"]
        P14["PR14<br/>Direct-boot loader<br/>U - 4 commits, 4 in review"]
    end

    subgraph Generic["Generic snapshot and persistence chain"]
        direction LR
        P09["PR09<br/>Fallible VM lifecycle<br/>U - 7 commits, 6 in review"]
        P10["PR10<br/>CPU/clock state<br/>U - 11 commits"]
        P11["PR11<br/>Snapshot publication<br/>U - 4 commits"]
        P12["PR12<br/>Private-CoW restore<br/>U - 5 commits"]
        P13["PR13<br/>Virtio persistence<br/>U - 11 commits"]
    end

    subgraph Product["Ordered microVM product stack"]
        direction LR
        P15["PR15<br/>Machine profile<br/>P - 11 commits"]
        P16["PR16<br/>Fixed virtio-MMIO ABI<br/>P - 4 commits"]
        P17["PR17<br/>Capture + authoritative restore<br/>P - 11 commits"]
        P18["PR18<br/>Host attachments<br/>P - 10 commits"]
        P19["PR19<br/>Block roles + tiers<br/>N/P - 3 commits"]
        P20["PR20<br/>Restore activation<br/>P - 7 commits + fixup"]
        P21["PR21<br/>Network egress policy<br/>P/N - 6 commits"]
        P22["PR22<br/>Filesystem deny policy<br/>P - 2 commits"]
        P23["PR23<br/>Authenticated control console<br/>N - 10 commits"]
        P24["PR24<br/>Managed workload/reporting<br/>N - 5 commits"]
        P25["PR25<br/>Management plane<br/>P - 2 commits"]
        P26["PR26<br/>Profiling benchmark<br/>U - 1 commit"]
        P27["PR27<br/>Safe writable-share symlinks<br/>U/P - 2 commits"]
        Z["Fork-only tail<br/>F - 3 commits"]
    end

    LB --> B
    LPART --> B
    B --> P01
    B --> P02
    B --> P03
    B --> P04
    B --> P05
    B --> P06
    B --> P07
    B --> P08
    B --> P09
    B --> P14

    P09 --> P10
    P10 --> P11
    P10 --> P12
    P11 --> P13
    P12 --> P13

    P01 --> F
    P02 --> F
    P03 --> F
    P04 --> F
    P05 --> F
    P06 --> F
    P07 --> F
    P08 --> F
    P13 --> F
    P14 --> F

    F --> P15
    P15 --> P16
    P16 --> P17
    P17 --> P18
    P18 --> P19
    P19 --> P20
    P20 --> P21
    P21 --> P22
    P22 --> P23
    P22 --> P27
    P23 --> P24
    P24 --> P25
    P25 --> P26
    P26 --> Z

    P07 -. "PR21 core can start early" .-> P21
    P08 -. "PR21 core can start early" .-> P21
    P13 -. "PR21 core can start early" .-> P21
    P03 -. "PR23 protocol/broker can start early" .-> P23
    P04 -. "PR23 protocol/broker can start early" .-> P23
    P13 -. "PR23 protocol/broker can start early" .-> P23

    classDef upstream fill:#dbeafe,stroke:#2563eb,color:#111827;
    classDef profile fill:#dcfce7,stroke:#16a34a,color:#111827;
    classDef product fill:#fef3c7,stroke:#d97706,color:#111827;
    classDef fork fill:#fee2e2,stroke:#dc2626,color:#111827;
    classDef gate fill:#f3f4f6,stroke:#4b5563,color:#111827;
    classDef landed fill:#ede9fe,stroke:#7c3aed,color:#111827;

    class B,F gate;
    class LB,LPART landed;
    class P01,P02,P03,P04,P05,P06,P07,P08,P09,P10,P11,P12,P13,P14,P26 upstream;
    class P15,P16,P17,P18,P20,P22,P25,P27 profile;
    class P19,P21,P23,P24 product;
    class Z fork;
```

## Actual branch DAG

The physical branch is one first-parent chain:

```text
b2676222b
  -> PR01 [001-005] -> PR02 [006-007] -> PR03 [008-009]
  -> PR04 [010-012] -> PR05 [013-015] -> PR06 [016-020]
  -> PR07 [021] -> PR08 [022-025] -> PR09 [026-032]
  -> PR10 [033-043] -> PR11 [044-047] -> PR12 [048-052]
  -> PR13 [053-063] -> PR14 [064-067] -> PR15 [068-078]
  -> PR16 [079-082] -> PR17 [083-093] -> PR18 [094-103]
  -> PR19 [104-106] -> PR20 [107-113] -> PR21 [114-119]
  -> PR22 [120-121] -> PR23 [122-131] -> PR24 [132-136]
  -> PR25 [137-138] -> PR26 [139] -> fork tail [140-142]
  -> PR27 [143-144]
  -> 82686c983 (mapped tip)
  -> time ABI v1 and CPU profiles [145-160] -> PR20 fixup [161]
  -> time ABI v1 and CPU profiles [162] -> CPU profiles [163-168]
  -> VM time [169-171] -> time ABI checks [172-173]
  -> time ABI 2 [174] -> management and test profiles [175-176]
  -> host filesystem [177-179] -> storage [180-181]
  -> network [182] -> host filesystem [183-187]
  -> network [188] -> storage [189-191]
  -> network [192-193] -> host filesystem [194]
  -> network [195] -> device DMA [196-197]
  -> command lines [198] -> KVP test [199-200]
  -> sandbox block roles [201]
  -> d891dfd9d (NVX pin)
```

For upstream review, PR01-PR08 and PR14 can be rebased independently onto current upstream main,
and nearly all of their commits are already in upstream review; see
[Open upstream pull requests](#open-upstream-pull-requests). PR09-PR13 form the generic dependency
chain shown above. The recommended product queue then fans those foundations into PR15 and keeps
PR15-PR26 ordered. PR21's generic enforcement commits can be prepared after PR07, PR08, and PR13;
PR23's protocol and broker commits can be prepared after PR03, PR04, and PR13. The fork tail,
positions 140-142, remains outside the upstream queue. PR27 can be prepared once PR22
lands; its generic confinement commit can be submitted independently if preferred. Fold position
161 into PR20's position 110 before submission, re-expressed for 110's tier check;
see [The v3.0 reorganization](#the-v30-reorganization).

## Commits after the mapped tip

The NVX pin extends the chain past position 144 without merges. Of the 57 later
commits, position 161 is a PR20 fixup, and the others are not yet classified or assigned to
PRs. Each of positions 145-200 combines the v2.4 commits that the table lists, which developed one
concept; "(part)" marks a v2.4 fixup that split across commits. Position 201 follows the
reorganization, so it combines none.

| Position | Commit | v2.4 positions | Topic |
| ---: | --- | --- | --- |
| 145 | `45ccb2046` | 149, 150, 382 (part), 317 (part) | virt: add the NVX time ABI v1 scaffolding |
| 146 | `cefb681ab` | 151, 152 | virt_kvm: implement the NVX time ABI backend |
| 147 | `7047f020c` | 153, 325 (part), 373 (part) | virt_mshv: implement the NVX time ABI backend |
| 148 | `ef56c5658` | 154, 157, 159, 160 | openvmm_core: add the time ABI platform behind --x-time-abi-v1 |
| 149 | `d06c7e3ed` | 155, 158 | openvmm_helpers: add snapshot manifest version 6 for the time ABI |
| 150 | `a53e8ae98` | 156 | chipset: serve restore packet v4 and time samples on portb |
| 151 | `0dbff3dd2` | 161, 162, 163, 164, 166, 167, 179 | cpu_profile: fingerprint backend CPU surfaces and pin CPU profiles |
| 152 | `fdb8621d6` | 165, 314 (part), 168, 169, 170, 171, 173, 174, 175, 176, 180, 319 | openvmm: preflight, capture, and restore the time ABI clocks |
| 153 | `ebfa46bb9` | 172 | virt_whp: implement the NVX time ABI backend |
| 154 | `cc98d2d5d` | 177 | openvmm_entry: add the --cpu-fingerprint tool |
| 155 | `14bf7c2a5` | 178, 181, 182, 183, 184, 185 | openvmm: carry the selected CPU profile in the time ABI configuration |
| 156 | `9ddd6ea41` | 186, 187, 325 (part), 196 | virt_mshv, virt_whp: deliver LAPIC vectors written to a halted VP's IRR |
| 157 | `6c6e289f1` | 188, 189, 190, 191, 192, 193, 373 (part), 194, 195, 197, 198, 199 | openvmm: program the complete effective CPUID of time ABI CPU profiles |
| 158 | `6587a8c5e` | 200, 201, 202, 203, 204, 205, 206, 207, 208, 209, 210, 211, 212 | virt: make the CPUID entries that a CPU profile omits read zero |
| 159 | `0b75cbf78` | 214, 215, 216, 217, 218, 219, 220, 221, 222, 223, 224, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 235 | virt: complete the time ABI's topology leaves and VP 0's CPUID view |
| 160 | `f5116f0c9` | 236, 237, 238, 373 (part), 239, 240, 241, 242, 243, 244, 324 (part) | openvmm: make the NVX time ABI the only microVM time path |
| 161 | `ae8adf97d` | 329 (part) | microVM: let platform snapshots opt into restore-time vCPU activation |
| 162 | `8300b44b3` | 245, 246, 247, 248, 249, 250, 318, 251, 252, 253, 254, 255, 256, 257, 258, 259 | virt: build the time ABI effective CPUID in one place |
| 163 | `62c5274c0` | 262, 263, 264 | openvmm: pin an Alder Lake CPU profile and add opt-in host profiles |
| 164 | `99f06acae` | 267, 268, 269, 270, 271, 373 (part), 272 | cpu_profile: pin AMD Milan, Genoa, and Turin CPU profiles |
| 165 | `fb9267e26` | 274 | virt_whp: let WHP answer CPUID leaves 0 and 1 without exits |
| 166 | `f378efef6` | 278 | openvmm_core: fall back from --cpu-profile auto to a host profile |
| 167 | `1c456d676` | 338, 339 | cpu_profile: pin amd.genoa.v2 for Genoa hosts without the TSA immunities |
| 168 | `2387dfa93` | 350, 351 | cpu_profile: verify pinned profiles, including their CPU time bits |
| 169 | `6bdffe36b` | 290 | vmm_core: preserve halt state across snapshots |
| 170 | `8d7d8d348` | 291, 292 | openvmm: preserve snapshot VM clock cut across save/restore |
| 171 | `a34a36144` | 358, 359 | vmcore: map VM times beyond the host clock to its latest time |
| 172 | `18dd2adb4` | 299, 314 (part) | openvmm: make the time ABI documentation standalone |
| 173 | `ba6ccc332` | 352, 353, 354, 355, 356, 357 | openvmm_core: name every CPUID difference in E_CPU_SURFACE errors |
| 174 | `1b94655ae` | 363, 364, 365, 366, 367, 368 | virt: publish host UTC and its clock correction in time ABI 2 |
| 175 | `675ba0818` | 322, 377 (part) | vmservice: select the microVM CPU profile over the management RPC |
| 176 | `cfbd76dec` | 323 | petri, vmm_tests: fall back to the host CPU profile where auto's is unsupported |
| 177 | `ba2e34fa9` | 260, 261, 320 (part) | virtiofs: perform microVM share requests as the guest caller |
| 178 | `ff04f6718` | 265, 266 | microVM: attach a second virtio-fs share with its own slot |
| 179 | `be760ed24` | 273, 320 (part) | virtiofs: add a per-subtree access policy to microVM shares |
| 180 | `5d61dbddb` | 275, 276, 277, 317 (part), 320 (part), 329 (part) | openvmm_entry: bind microVM snapshots to their generation and scratch |
| 181 | `11d1d95bc` | 279, 320 (part) | openvmm_entry: permit RAM-backed microVM overlays without scratch |
| 182 | `3444159b4` | 280, 320 (part), 281, 282 | microVM: select egress rules by protocol and port range, and add IPv6 |
| 183 | `15f62a20d` | 283, 320 (part), 284, 285, 286 | microVM: share several host directories through one virtio-fs slot |
| 184 | `7797b12e2` | 287, 320 (part), 288, 289 | microVM: export single host files as aggregate virtio-fs children |
| 185 | `963963a35` | 293 | virtiofs: test RENAME_EXCHANGE node names and snapshots |
| 186 | `9608c775e` | 334, 335 (part) | virtiofs: test microVM queues that start or stop with host input gated |
| 187 | `cc2748a35` | 294, 320 (part), 295 | microVM: accept host paths with commas in --mount |
| 188 | `ccf3a2849` | 296, 320 (part), 297, 379 (part), 298, 300 | microVM: publish guest listeners to private networks and host loopback |
| 189 | `805f5d721` | 331 | disk_zerofill: present a fixed geometry over a shorter or absent disk |
| 190 | `571301549` | 332, 333 | microVM: let a platform restore present a shorter distro and no scratch |
| 191 | `55f9e50dd` | 340, 341, 342 | disk_blob: bound, retry, and inspect HTTP blob requests |
| 192 | `caa450448` | 336, 337 | microVM: let a platform restore install a new policy on a NIC template |
| 193 | `1dd3bc0e3` | 360 | microVM: reject explicit host-loopback access on a /30 IPv4 network |
| 194 | `56e6b0e4b` | 343, 344 | microVM: let a platform restore attach an aggregate to a dormant slot |
| 195 | `6450005b9` | 369, 370 | microVM: install a new network policy at an untiered restore |
| 196 | `1f56fbbef` | 345, 346 | guestmem, virtio: admit only a set of ranges to a device's memory |
| 197 | `92fe29c05` | 347, 348, 349 | microVM: restrict virtio device DMA to the RAM ranges of the e820 map |
| 198 | `9fda4ba25` | 302, 303, 304, 305, 306, 307 | microVM: refuse command lines that the guest kernel would truncate |
| 199 | `3528a85c1` | 361 | vmm_tests: Fix kvp test flakiness by updating windows version |
| 200 | `0ac254109` | 362 | ci: run the Windows Server 2025 KVP test on Linux KVM |
| 201 | `d891dfd9d` | - | microVM: remove the runtime and custom sandbox block roles (microsoft/openvmm#4839) |

Some of these commits change mapped work rather than add to it. Position 160 removes the
legacy microVM clock paths, changing files that PR10 and PR17 also change, and makes manifest
version 6 the only snapshot format, changing files that PR11 and PR17 also change. Position
190 changes the sandbox block attachment of PR19's position 105 and the restore of
position 180. Position 192 changes the snapshot network policy and the forward refusal
of PR21's position 119 and the snapshot refusals of position 188. Position 194
changes the dormant-slot restore of PR18's position 103, as the aggregates of position
183 restricted it. Position 201 removes the `runtime` and `custom` sandbox block roles that PR19's
position 104 adds, with the slots and IRQs that PR16's position 080 places for them and the
shared-status words that PR16's position 082 gives them, which stay unassigned, and the role
names and layer counts of PR19's position 105's snapshot capture and restore. Assigning the later
commits therefore means revisiting those PRs, not only appending new ones.

Position 191, by contrast, changes only `disk_blob`, which upstream main and the fork base
have alike and no other commit of the chain changes, so it can go upstream on its own. Position
196 likewise changes only `guestmem` and the virtio queue's check, and can go upstream on its
own; position 197 builds on the fork's virtio-mmio shared interrupt-status transport,
MP-table loader, and microVM worker. Position 168 changes only `cpu_profile`, the fork's CPU
profile crate that the time ABI series adds, one of its test helpers in `virt_mshv`, and the
snapshot guide, so it builds on that fork-only code. Position 173 changes only the CPU checks
of the VM worker's time ABI and the snapshot guide's description of them. Position 171
changes only the VM time keeper of `vmcore`, whose `vmtime.rs` upstream main and the fork base have
alike, and applies to upstream main as it is, so it can go upstream on its own. Position 193
changes only how the microVM options bind their network policy to the NIC's identity and the CLI
and Consomme references, so it builds on the fork-only commits of positions 188 and
192. Position 199 makes the change of microsoft/openvmm#4803 unchanged, and position
200 stages the Server 2025 image for the x64 Linux KVM job, which runs the test, and
excludes the test and its image from the x64 Linux MSHV job, where Server 2025's required initial
reset fails. A rebase onto an upstream main that includes microsoft/openvmm#4803 drops position
199, as the rebases dropped the commits under [Landed upstream](#landed-upstream), and
reconciles position 200 with whatever upstream changes for those jobs. Position 174
moves the time ABI to version 2: it changes the time ABI backends of KVM, MSHV, and WHP, the
portb device's time sample, the restore packet, and the microVM worker's time-abi unit, all of
which the time ABI series added, so it follows that series upstream. Position 195 changes
the tier check of position 192's restore that installs a new policy and extends restore
packet 5 of position 174, so it builds on those fork-only commits. Position 201 changes only the
microVM profile, CLI, and snapshot contract that PR16 and PR19 add, and their Guide pages, so it
does not apply to upstream main and follows those PRs upstream.

## PR summary

Each count is of v3.0 commits, which include the fixups that the reorganization folded into them;
see [The v3.0 reorganization](#the-v30-reorganization).

| PR | Class | Topic | Actual positions | Actual commit range(s) | Count | Recommended direct dependency |
| --- | --- | --- | ---: | --- | ---: | --- |
| PR01 | U | Build and test infrastructure | 001-005 | `af6e67d40..ece4ba8ff` | 5 | Current upstream |
| PR02 | U | Host primitives: mesh and sparse_mmap | 006-007 | `35d6935c6..d1fb02755` | 2 | Current upstream |
| PR03 | U | Host primitives: PAL on Unix | 008-009 | `d0bb3db2a..cbcecab34` | 2 | Current upstream |
| PR04 | U | Host primitives: PAL and PAL async on Windows | 010-012 | `1a8bc4363..d25e0e13f` | 3 | Current upstream |
| PR05 | U | Hypervisor backend fixes | 013-015 | `1f6048d4c..c451b5925` | 3 | Current upstream |
| PR06 | U | Device fixes and hardening | 016-020 | `47102941e..716b0c800` | 5 | Current upstream |
| PR07 | U | Consomme resource bounding | 021 | `6425cf88a` | 1 | Current upstream |
| PR08 | U | Preparatory refactors | 022-025 | `38998a580..c1bd1e0a5` | 4 | Current upstream |
| PR09 | U | Fallible and bounded VM lifecycle | 026-032 | `fa02eed4b..6e8ba8efb` | 7 | Current upstream |
| PR10 | U | Portable CPU and clock state | 033-043 | `3dcf87849..adcce24c4` | 11 | PR09 |
| PR11 | U | Snapshot publication protocol | 044-047 | `2bc04fd7d..9a41756cc` | 4 | PR10 |
| PR12 | U | Immutable private-CoW restore | 048-052 | `7c898206e..a603c7857` | 5 | PR10 |
| PR13 | U | Generic virtio persistence | 053-063 | `3d11ed01e..be2ec6c40` | 11 | PR11 + PR12 |
| PR14 | U | Firmware-less direct boot, loader half | 064-067 | `3865d595d..9bed01b63` | 4 | Current upstream |
| PR15 | P | microVM machine profile and minimal chipset | 068-078 | `e6a222c36..310048ce1` | 11 | Foundation fan-in |
| PR16 | P | Fixed virtio-MMIO ABI and shared interrupt status | 079-082 | `bf7e37fef..2c7bbdb9b` | 4 | PR15 |
| PR17 | P | microVM snapshot capture and authoritative restore | 083-093 | `b1bce8932..d693f0f68` | 11 | PR16 |
| PR18 | P | microVM host attachments and device persistence | 094-103 | `3824af263..07f7f0bf1` | 10 | PR17 |
| PR19 | N/P | Sandbox block roles and snapshot tiers | 104-106 | `5d4bbd028..68a8aec5f` | 3 | PR18 |
| PR20 | P | Restore-time processor and memory activation | 107-113; 161 (fixup) | `fdca04ef6..6d70e6c20`; `ae8adf97d` | 8 | PR19 |
| PR21 | P/N | Network egress policy | 114-119 | `25a237a18..8a77294e6` | 6 | PR20; core can start after PR07 + PR08 + PR13 |
| PR22 | P | Filesystem deny policy | 120-121 | `d6ee19e65..9382eb822` | 2 | PR21 |
| PR23 | N | Authenticated control console | 122-131 | `fb49e4c62..f947cc7a3` | 10 | PR22; protocol/broker can start after PR03 + PR04 + PR13 |
| PR24 | N | Managed workload and outcome reporting | 132-136 | `6bd20010e..89ff3f110` | 5 | PR23 |
| PR25 | P | Management plane | 137-138 | `ddd66b91c..8bf88a785` | 2 | PR24 |
| PR26 | U | Profiling benchmark | 139 | `83f0740aa` | 1 | PR25 in the current stack |
| PR27 | U/P | Safe symlinks for writable microVM shares | 143-144 | `b4e47fc38..82686c983` | 2 | PR22; position 143 can land independently |
| Fork tail | F | CI guards and NVX wire compatibility | 140-142 | `d8d2bf34a..1be1c24d3` | 3 | PR26; do not submit upstream |

## Exact commit-to-PR mapping

Each entry names the v2.4 position of the commit and the v2.4 fixups that it folds; "(part)" marks
a fixup that split across commits. Landed entries name the commits and positions of the branch
that the rebase started from, `microvm/v2.1`, `feature/microvm/release/v2.2`,
`feature/microvm/release/v2.3`, or `feature/microvm/release/v2.4`, and the counts exclude them.
Partial upstream overlap is called out without striking the remaining fork work, and
**open upstream** names the upstream pull request that carries a commit.

<details>
<summary><strong>PR01 - Build and test infrastructure (U)</strong> - 5 commits + 3 landed upstream</summary>

- 001 `af6e67d40` - build: keep Winsock eagerly imported on Windows (v2.4 001; **not upstream**: microsoft/openvmm#4451 was closed without merging)
- 002 `030cabda8` - flowey: distinguish native Windows and WSL nextest paths (v2.4 002; **open upstream**: microsoft/openvmm#4595)
- 003 `7f3c4a929` - flowey: keep caller temp directories for native local VMM-test runs (v2.4 003; **open upstream**: microsoft/openvmm#4618)
- 004 `7b7d7c3b7` - consomme: make TCP window-reopen validation deterministic (v2.4 004; **open upstream**: microsoft/openvmm#4598)
- 005 `ece4ba8ff` - virt_whp, petri: enable native PCIe NVMe boot on WHP (v2.4 005; **open upstream**: microsoft/openvmm#4615)

Landed upstream and dropped from the chain:

- ~~`607e6c6a2` - nextest: scope a longer budget to the exhaustive ring-buffer loom test~~
  (**landed upstream**: microsoft/openvmm#4584, merged as `9501bb481`; v2.1 position 002)
- ~~`0213cf65d` - flowey: skip reinstalling an identical, possibly running cargo-nextest binary~~
  (**landed upstream**: microsoft/openvmm#4608, merged as `03a2fc597`; v2.3 position 003)
- ~~`a91dc86fb` - lxutil: tolerate inherited extended attributes in tests~~
  (**landed upstream**: microsoft/openvmm#4587, merged as `a1d71b899`; v2.3 position 005)

</details>

<details>
<summary><strong>PR02 - Host primitives: mesh and sparse_mmap (U)</strong> - 2 commits + 1 landed upstream</summary>

- 006 `35d6935c6` - mesh_protobuf: clone type-erased messages and expose encoded length (v2.4 006; **partly landed upstream**: microsoft/openvmm#4556, merged as `96647c331`, derives `Clone`; this commit now adds only `ProtobufAny::value_len` and a clone check)
- 007 `d1fb02755` - sparse_mmap: flush file-backed ranges with bounded concurrency (v2.4 007; **open upstream**: microsoft/openvmm#4558; folds v2.4 328 (part))

Landed upstream and dropped from the chain:

- ~~`15a895dd8` - sparse_mmap: add private copy-on-write file views~~
  (**landed upstream**: microsoft/openvmm#4557, merged as `d8acf2a7c`; v2.3 position 009)

</details>

<details>
<summary><strong>PR03 - Host primitives: PAL on Unix (U)</strong> - 2 commits + 1 landed upstream</summary>

- 008 `d0bb3db2a` - pal: bound Linux descriptor-table pre-expansion (v2.4 008; **open upstream**: microsoft/openvmm#4589)
- 009 `cbcecab34` - pal: query Unix socket peer credentials (v2.4 009; **open upstream**: microsoft/openvmm#4591)

Landed upstream and dropped from the chain:

- ~~`4a7ab405c` - pal: add Unix extent cloning and sparse-extent seeking~~
  (**landed upstream**: microsoft/openvmm#4590, merged as `c62489737`; v2.3 position 012)

</details>

<details>
<summary><strong>PR04 - Host primitives: PAL and PAL async on Windows (U)</strong> - 3 commits + 3 landed upstream</summary>

- 010 `1a8bc4363` - pal: add handle-relative file access that does not follow reparse points (v2.4 010; **open upstream**: microsoft/openvmm#4593)
- 011 `43a6358ca` - pal: resolve the process token user SID (v2.4 011; **open upstream**: microsoft/openvmm#4604)
- 012 `d25e0e13f` - pal_async: fix named-pipe reconnect and already-connected listen (v2.4 012; **open upstream**: microsoft/openvmm#4585, stacked on microsoft/openvmm#4582)

Landed upstream and dropped from the chain:

- ~~`074de3406` - pal: add sparse-file range queries, zeroing, and block cloning~~
  (**landed upstream**: microsoft/openvmm#4596, merged as `8f1c36ba5`; v2.3 position 015)
- ~~`c61d77644` - pal: create files and directories with explicit security descriptors~~
  (**landed upstream**: microsoft/openvmm#4599, merged as `9fbf965c5`; v2.1 position 017)
- ~~`50460c27a` - pal: query named-pipe peers and create protected pipes~~
  (**landed upstream**: microsoft/openvmm#4582, merged as `65db52693`; v2.3 position 017)

</details>

<details>
<summary><strong>PR05 - Hypervisor backend fixes (U)</strong> - 3 commits + 2 landed upstream</summary>

- 013 `1f6048d4c` - virt_mshv: make RUN_VP cancellation race-free (v2.4 013; **open upstream**: microsoft/openvmm#4536)
- 014 `8a7dea926` - virt: add Partition::finalize_memory; create the MSHV BSP after finalization (v2.4 014; **open upstream**: microsoft/openvmm#4607; folds v2.4 325 (part), 373 (part), and 386)
- 015 `c451b5925` - virt_mshv: persist and redeliver pending external interrupts (v2.4 015; **open upstream**: microsoft/openvmm#4613, stacked on microsoft/openvmm#4536; folds v2.4 325 (part) and 311)

Landed upstream and dropped from the chain:

- ~~`88865bf90` - virt_mshv: describe SNP guest workarounds without ACI-specific names~~
  (**landed upstream**: microsoft/openvmm#4580, merged as `40cff1268`; v2.1 position 021)
- ~~`9b3ce6760` - kvm: preserve pending external interrupts across VP-state capture~~
  (**landed upstream**: superseded by microsoft/openvmm#4522, merged as `392890680`; v2.1 position 022)

</details>

<details>
<summary><strong>PR06 - Device fixes and hardening (U)</strong> - 5 commits + 5 landed upstream</summary>

- 016 `47102941e` - virtio_blk: let stop cancellation win over a ready queue and drain accepted I/O (v2.4 017; **open upstream**: microsoft/openvmm#4588; folds v2.4 378)
- 017 `41460f3f7` - virtiofs: make handle and inode ID allocation fallible (v2.4 018; **open upstream**: microsoft/openvmm#4594)
- 018 `3f6941671` - virtiofs: bound guest payload allocations and rate-limit diagnostics (v2.4 019; **open upstream**: microsoft/openvmm#4597)
- 019 `b61aca567` - serial_socket: drop the connection on any write failure (v2.4 020; **open upstream**: microsoft/openvmm#4605)
- 020 `716b0c800` - serial_socket: keep Windows named-pipe listeners reconnectable (v2.4 021; **open upstream**: microsoft/openvmm#4609, stacked on microsoft/openvmm#4582 and microsoft/openvmm#4585)

Landed upstream and dropped from the chain:

- ~~`f5231b2d2` - chipset: preserve IOAPIC line levels across restore~~
  (**landed upstream**: microsoft/openvmm#4583, merged as `db4331e53`; v2.4 position 016)
- ~~`938b14343` - chipset: fast-forward periodic PIT counting over long intervals~~
  (**landed upstream**: microsoft/openvmm#4586, merged as `023329997`; v2.3 position 023)
- ~~`947ae858b` - virtio: make interrupt tests wait for queue completion~~
  (**landed upstream**: microsoft/openvmm#4592, merged as `5b4656a86`; v2.1 position 029)
- ~~`21c2206c9` - fuse: rate-limit guest-triggerable diagnostics~~
  (**landed upstream**: microsoft/openvmm#4581, merged as `926827446`; v2.1 position 030)
- ~~`54c5c34cd` - virtio_console: rate-limit worker errors~~
  (**landed upstream**: microsoft/openvmm#4614, merged as `77ce2aeac`; v2.1 position 035)

</details>

<details>
<summary><strong>PR07 - Consomme resource bounding (U)</strong> - 1 commit + 2 landed upstream</summary>

- 021 `6425cf88a` - consomme: bound active TCP, UDP, and ICMP flows (v2.4 022; **open upstream**: microsoft/openvmm#4600)

Landed upstream and dropped from the chain:

- ~~`135c503ba` - consomme: cover the pending DNS request limit with a test~~
  (**landed upstream**: microsoft/openvmm#4601, merged as `cd93f761f`; v2.3 position 030)
- ~~`90734a2ff` - consomme: cover IPv4 fragment rejection with a test~~
  (**landed upstream**: microsoft/openvmm#4602, merged as `3d9cb006a`; v2.3 position 031)

</details>

<details>
<summary><strong>PR08 - Preparatory refactors (U)</strong> - 4 commits + 2 landed upstream</summary>

- 022 `38998a580` - openvmm_helpers: split the snapshot module without behavior changes (v2.4 023; **open upstream**: microsoft/openvmm#4616)
- 023 `313b5fe0b` - net_tap: move the transmit path into its own module (v2.4 024; **open upstream**: microsoft/openvmm#4617)
- 024 `238c6cc96` - net_consomme: move queue transmit processing into a module (v2.4 025; **open upstream**: microsoft/openvmm#4620)
- 025 `c1bd1e0a5` - virtio_console: move the direct receive loop into its own module (v2.4 026; **open upstream**: microsoft/openvmm#4621)

Landed upstream and dropped from the chain:

- ~~`dd349021e` - openvmm_entry: accept `delay:<ms>:<disk>` disk wrappers~~
  (**landed upstream**: microsoft/openvmm#4610, merged as `6e0e4fdab`; v2.1 position 039)
- ~~`6794f12d0` - openvmm_entry: skip raw console mode when stdin is not a terminal~~
  (**landed upstream**: microsoft/openvmm#4606, merged as `769f2f61f`; v2.3 position 036)

</details>

<details>
<summary><strong>PR09 - Fallible and bounded VM lifecycle (U)</strong> - 7 commits</summary>

- 026 `fa02eed4b` - state_unit: make unit start fallible and roll back partial starts (v2.4 027; **open upstream**: microsoft/openvmm#4619; folds v2.4 327)
- 027 `d94e7b9b3` - vmcore: add fallible device start through chipset device workers (v2.4 028; **open upstream**: microsoft/openvmm#4622, stacked on microsoft/openvmm#4619)
- 028 `7496e02e8` - openvmm: make VM resume fallible (v2.4 029; **open upstream**: microsoft/openvmm#4623, stacked on microsoft/openvmm#4619)
- 029 `f2d0d92b2` - state_unit: record the exact unit inventory in saved state (v2.4 030; **open upstream**: microsoft/openvmm#4624)
- 030 `7f428a982` - state_unit: add bounded quiesce transactions with rollback classification (v2.4 031; **open upstream**: microsoft/openvmm#4625, stacked on microsoft/openvmm#4619; folds v2.4 371 (part))
- 031 `671df22a6` - vmcore: add device input quiesce and resume (v2.4 032; **open upstream**: microsoft/openvmm#4626, stacked on microsoft/openvmm#4625)
- 032 `6e8ba8efb` - openvmm_core: stop the devices before tearing down the VM (v2.4 301)

</details>

<details>
<summary><strong>PR10 - Portable CPU and clock state (U)</strong> - 11 commits</summary>

- 033 `3dcf87849` - vmcore, state_unit: advance guest time across stopped downtime (v2.4 033; folds v2.4 213 (part) and 371 (part))
- 034 `2791971cf` - chipset: persist and advance the RTC clock (v2.4 034; folds v2.4 213 (part))
- 035 `436814969` - chipset: reject inconsistent RTC interrupt state on restore (v2.4 035)
- 036 `30d8a9b9a` - chipset: catch up the PIT after downtime (v2.4 036)
- 037 `52fd82a74` - virt: define a canonical x86 CPU compatibility contract (v2.4 037)
- 038 `876e1a7ca` - virt: record TSC-deadline support and advance APIC timers (v2.4 038; **partly superseded upstream**: microsoft/openvmm#4704, merged as `1c093df84` before the fork base, adds the same `TscDeadline` state element and backend accessors, whose chain copies this commit no longer adds; folds v2.4 324 (part))
- 039 `6fcf430a8` - virt: add exact TSC-frequency helpers and snapshot clock hooks (v2.4 039)
- 040 `a669f91b5` - virt_kvm: save and restore exact TSC and kvm-clock state (v2.4 040)
- 041 `2e2709c2a` - virt_mshv: synchronize restored vCPU clocks and expose the exact TSC frequency (v2.4 041; folds v2.4 373 (part))
- 042 `40389eec8` - virt_whp: synchronize restored vCPU clocks and keep SMP TSC reads coherent (v2.4 042; folds v2.4 312 and 324 (part))
- 043 `adcce24c4` - vmm_core: advance VP TSC and APIC timers across downtime (v2.4 043)

</details>

<details>
<summary><strong>PR11 - Snapshot publication protocol (U)</strong> - 4 commits</summary>

- 044 `2bc04fd7d` - openvmm_defs: add opt-in snapshot lifecycle profiling (v2.4 044)
- 045 `65f4d884f` - openvmm_helpers: version the snapshot manifest format and bound its decoding (v2.4 045)
- 046 `fd5eb056e` - openvmm_helpers: publish snapshots atomically through durable staging (v2.4 046)
- 047 `9a41756cc` - openvmm: publish the exact file-backed RAM handle (v2.4 047; folds v2.4 317 (part))

</details>

<details>
<summary><strong>PR12 - Immutable private-CoW restore (U)</strong> - 5 commits</summary>

- 048 `7c898206e` - membacking: map guest RAM privately copy-on-write with fault accounting (v2.4 048; folds v2.4 375 (part))
- 049 `6b2c5e486` - openvmm_helpers: open snapshots once and read artifacts relative to the directory (v2.4 049)
- 050 `85d7a751e` - openvmm: restore RAM through a private CoW mapping and retain generation handles (v2.4 050; folds v2.4 375 (part))
- 051 `e7aaeab26` - openvmm: signal restore readiness to a local socket or pipe (v2.4 051)
- 052 `a603c7857` - virt_whp: resolve unmapped-GPA faults and register restored RAM lazily (v2.4 052; folds v2.4 313 and 382 (part))

</details>

<details>
<summary><strong>PR13 - Generic virtio persistence (U)</strong> - 11 commits</summary>

- 053 `3d11ed01e` - virtio: add device-private saved state with deferred activation and serialized kicks (v2.4 053; folds v2.4 372)
- 054 `f7abf8215` - virtio: forward input quiesce and resume through transports (v2.4 054)
- 055 `569c31998` - virtio_console: add attachment identities and reconnect and disconnect policies (v2.4 055; folds v2.4 380 (part))
- 056 `a29e7173e` - virtio_console: persist buffered direct-console I/O (v2.4 056)
- 057 `2bc137713` - net_backend: add an endpoint queue quiesce contract (v2.4 057)
- 058 `4931d8ab0` - net_tap: quiesce queues and retain pending transmit ownership (v2.4 058; folds v2.4 309 (part) and 379 (part))
- 059 `0f7048709` - net_consomme: quiesce endpoint queues (v2.4 059)
- 060 `7d2664819` - consomme: add an exact static IPv4 and gateway identity (v2.4 060)
- 061 `0afebc9dd` - virtio_net: persist drained queue state for static-identity NICs (v2.4 061)
- 062 `01e69989a` - fuse: save and restore negotiated session state (v2.4 062)
- 063 `be2ec6c40` - virtiofs: track inode aliases through link, rename, and unlink (v2.4 063)

</details>

<details>
<summary><strong>PR14 - Firmware-less direct boot, loader half (U)</strong> - 4 commits + 1 landed upstream</summary>

- 064 `3865d595d` - loader: let static ELF loading reuse a caller buffer (v2.4 064; **open upstream**: microsoft/openvmm#4554)
- 065 `2bea08576` - loader: add an Intel MP 1.4 table builder (v2.4 065; **open upstream**: microsoft/openvmm#4554)
- 066 `476320533` - loader: add firmware-less Linux direct boot with MP tables (v2.4 066; **open upstream**: microsoft/openvmm#4554)
- 067 `9bed01b63` - vmm_core: make level-triggered legacy IRQs explicit (v2.4 067; **open upstream**: microsoft/openvmm#4611; folds v2.4 326)

Landed upstream and dropped from the chain:

- ~~`f6e6ff738` - loader_defs: name the boot_params ACPI RSDP field~~
  (**landed upstream**: microsoft/openvmm#4661, merged as `8beae2961`; v2.2 position 074)

</details>

<details>
<summary><strong>PR15 - microVM machine profile and minimal chipset (P)</strong> - 11 commits</summary>

- 068 `e6a222c36` - openvmm_defs: add machine-profile selection (v2.4 068)
- 069 `15744a7ed` - virt: describe the deterministic microVM topology (v2.4 069)
- 070 `5c3628470` - chipset: add an RTC mode seam (v2.4 070)
- 071 `cf9d8446f` - vm_manifest_builder: add the minimal microVM base chipset without ACPI (v2.4 071)
- 072 `492a38441` - vmm_core: add status-bearing guest power-off (v2.4 072)
- 073 `7397dca52` - chipset: add the microVM portb console (v2.4 073)
- 074 `a69d4752f` - chipset: add the microVM shutdown and exit-status port (v2.4 074)
- 075 `ec777df58` - openvmm_core: boot microVMs through MP-table direct load (v2.4 075; folds v2.4 315 and 314 (part))
- 076 `bf08aca4b` - openvmm_core: reject host save, pulse save/restore, and worker restart for microVMs (v2.4 076; folds v2.4 314 (part))
- 077 `d386b6bfe` - openvmm_entry: expose `--machine microvm` (v2.4 077; folds v2.4 316 (part), 317 (part), and 320 (part))
- 078 `310048ce1` - petri, vmm_tests: add a microVM builder, portb harness, and cold-boot test (v2.4 078; folds v2.4 380 (part))

</details>

<details>
<summary><strong>PR16 - Fixed virtio-MMIO ABI and shared interrupt status (P)</strong> - 4 commits</summary>

- 079 `bf7e37fef` - virtio: allow feature masking for virtio-MMIO devices (v2.4 079)
- 080 `27845ec4a` - openvmm: place virtio devices at fixed microVM MMIO slots (v2.4 080; folds v2.4 314 (part))
- 081 `c87678155` - virtio: add shared MMIO interrupt status and ACK doorbells (v2.4 081)
- 082 `2c7bbdb9b` - openvmm: use shared interrupt status for fixed microVM slots (v2.4 082)

</details>

<details>
<summary><strong>PR17 - microVM snapshot capture and authoritative restore (P)</strong> - 11 commits</summary>

- 083 `b1bce8932` - vmm_core: stop VPs at a deferred-I/O boundary (v2.4 083)
- 084 `8cb59d778` - chipset: add the microVM snapshot-request port (v2.4 084)
- 085 `2c371ac94` - openvmm_core: establish the post-OUT snapshot boundary in the worker (v2.4 085)
- 086 `afad25da0` - openvmm_core: fence management mutations during snapshot boundaries (v2.4 086)
- 087 `2fccba420` - openvmm_helpers: define the authoritative microVM machine contract (v2.4 087)
- 088 `67e31121d` - openvmm_core: propagate the guest TSC frequency for snapshot-capable cold boots (v2.4 088)
- 089 `28eb6663f` - openvmm_entry: capture guest-requested microVM snapshots (v2.4 089; folds v2.4 328 (part))
- 090 `e5f17e960` - virt: build a reproducible CPU and clock contract for microVM partitions (v2.4 090)
- 091 `013d32d49` - openvmm_core: validate restored CPU and clock contracts and apply host downtime (v2.4 091)
- 092 `0f522e942` - openvmm_entry: restore microVMs authoritatively (v2.4 092)
- 093 `d693f0f68` - vmm_tests: check that an unconfigured microVM snapshot request continues (v2.4 093)

</details>

<details>
<summary><strong>PR18 - microVM host attachments and device persistence (P)</strong> - 10 commits</summary>

- 094 `3824af263` - openvmm_entry: add connect= serial backends with bounded timeouts (v2.4 094; folds v2.4 144)
- 095 `359c96990` - openvmm: persist the microVM boot console and rebuild its attachment on restore (v2.4 095)
- 096 `b69bca7b2` - openvmm: attach a portable Consomme NIC to microVMs (v2.4 096; folds v2.4 376 and 316 (part))
- 097 `619ce0e45` - openvmm: persist the microVM NIC and restore a fresh endpoint generation (v2.4 097)
- 098 `1b70e91f9` - net_tap: support persistent TAP attachments (v2.4 098)
- 099 `d0b79eaad` - virtiofs: add the fixed microVM filesystem profile (v2.4 099)
- 100 `7f0bb01a2` - openvmm_entry: expose `--mount` for the microVM HostFs slot (v2.4 100)
- 101 `f11af4bb1` - virtiofs: persist microVM FUSE namespaces, handles, and directory cookies (v2.4 101; folds v2.4 335 (part))
- 102 `e8e0a2486` - openvmm: snapshot the microVM filesystem and revalidate it on restore (v2.4 102)
- 103 `07f7f0bf1` - openvmm: reserve a dormant microVM filesystem slot (v2.4 103; folds v2.4 335 (part))

</details>

<details>
<summary><strong>PR19 - Sandbox block roles and snapshot tiers (N/P)</strong> - 3 commits</summary>

- 104 `5d4bbd028` - openvmm: add fixed-role microVM sandbox block slots (v2.4 104; folds v2.4 320 (part))
- 105 `50c57baab` - openvmm: snapshot sandbox blocks with paired or fresh scratch (v2.4 105; folds v2.4 317 (part))
- 106 `68a8aec5f` - openvmm: add clone and resume tiers with single-use resume claims (v2.4 106; folds v2.4 320 (part))

</details>

<details>
<summary><strong>PR20 - Restore-time processor and memory activation (P)</strong> - 8 commits</summary>

- 107 `fdca04ef6` - chipset: expose a stable generation ID and restore entropy on portb (v2.4 107)
- 108 `795a348af` - openvmm: gate external input until post-restore repair completes (v2.4 108; folds v2.4 335 (part))
- 109 `ee40eab94` - vmm_core: instantiate an active VP prefix (v2.4 109)
- 110 `101eae736` - openvmm: bring a restore-time processor prefix online (v2.4 110; folds v2.4 141, 142 (part), and 325 (part))
- 111 `ca2f7a40a` - openvmm_core: back expandable restore RAM with fresh private memory (v2.4 111)
- 112 `7369f0c87` - openvmm: add bounded restore-time memory expansion (v2.4 112; folds v2.4 320 (part))
- 113 `6d70e6c20` - openvmm_entry: pre-expand the Linux descriptor table at startup (v2.4 113)
- 161 `ae8adf97d` - microVM: let platform snapshots opt into restore-time vCPU activation (v2.4 329; **fold into 110**, re-expressed for its tier check)

</details>

<details>
<summary><strong>PR21 - Network egress policy (P/N)</strong> - 6 commits</summary>

- 114 `25a237a18` - net_backend_resources: define a canonical bound IPv4 egress policy (v2.4 114)
- 115 `0c9f4faed` - net_backend: let endpoints accept a run-scoped egress policy (v2.4 115)
- 116 `e0228e079` - net_consomme: enforce the egress policy on final transmitted bytes (v2.4 116; folds v2.4 379 (part))
- 117 `65e32fee9` - net_tap: enforce the egress policy on TAP transmit (v2.4 117; folds v2.4 309 (part) and 379 (part))
- 118 `906bd0de9` - virtio_net: install and prefilter the egress policy (v2.4 118)
- 119 `8a77294e6` - openvmm: add microVM network egress policy and bind it into snapshots (v2.4 119; folds v2.4 320 (part))

</details>

<details>
<summary><strong>PR22 - Filesystem deny policy (P)</strong> - 2 commits</summary>

- 120 `d6ee19e65` - virtiofs: enforce denied subtrees by object identity (v2.4 120)
- 121 `9382eb822` - openvmm: add --mount-deny and record denied paths in snapshots (v2.4 121; folds v2.4 320 (part))

</details>

<details>
<summary><strong>PR23 - Authenticated control console (N)</strong> - 10 commits</summary>

- 122 `fb49e4c62` - serial_core: expose local peer identity and active-peer eviction (v2.4 122)
- 123 `7d5d89331` - serial_socket: report Unix and named-pipe peer identities (v2.4 123; folds v2.4 374 (part))
- 124 `4948fedb2` - virtio_console: define the control-session record protocol (v2.4 124)
- 125 `ed5f777eb` - virtio_console: add the control-session broker state machine (v2.4 125)
- 126 `a699e9b52` - virtio_console: attach the broker to serial I/O with receive credits (v2.4 126; folds v2.4 330 (part) and 384)
- 127 `7b0b483a6` - virtio_console: save and restore broker state (v2.4 127)
- 128 `8ab202d90` - openvmm: reserve the dedicated microVM control-console slot (v2.4 128)
- 129 `d09d266a2` - openvmm_helpers: record the control console in the microVM machine contract (v2.4 129)
- 130 `5ec16d5ee` - openvmm_entry: add authenticated microVM control-console endpoints (v2.4 130; folds v2.4 330 (part), 374 (part), 385, 317 (part), and 320 (part))
- 131 `f947cc7a3` - openvmm_entry: restore listener endpoint rebinding (v2.4 143)

</details>

<details>
<summary><strong>PR24 - Managed workload and outcome reporting (N)</strong> - 5 commits</summary>

- 132 `6bd20010e` - openvmm_entry: drain portb output before guest-requested exit (v2.4 131; folds v2.4 148 (part))
- 133 `3fe27c6cb` - openvmm: add a fixed non-root workload identity (v2.4 132; folds v2.4 320 (part))
- 134 `46c85905f` - openvmm: support a managed workload lifecycle (v2.4 133)
- 135 `62b766d34` - openvmm_entry: write bounded microVM outcome reports (v2.4 134)
- 136 `89ff3f110` - Guide: describe teardown after a failed snapshot boundary (v2.4 135)

</details>

<details>
<summary><strong>PR25 - Management plane (P)</strong> - 2 commits</summary>

- 137 `ddd66b91c` - vmservice: create, capture, and restore microVMs over the management RPC (v2.4 136; folds v2.4 137 (part), 142 (part), 145, 148 (part), 310, 321, 377 (part), and 381)
- 138 `8bf88a785` - ttrpc: propagate nonzero guest exit status (v2.4 137; folds v2.4 148 (part))

</details>

<details>
<summary><strong>PR26 - Profiling benchmark (U)</strong> - 1 commit</summary>

- 139 `83f0740aa` - openvmm_helpers: add a publication and private-restore benchmark (v2.4 138; folds v2.4 375 (part))

</details>

<details>
<summary><strong>PR27 - Safe symlinks for writable microVM shares (U/P)</strong> - 2 commits</summary>

- 143 `b4e47fc38` - lxutil: add path confinement that never follows symbolic links (v2.4 146)
- 144 `82686c983` - virtiofs: allow symlink creation on read-write microVM shares (v2.4 147)

</details>

<details>
<summary><strong>Fork-only tail (F)</strong> - 3 commits</summary>

- 140 `d8d2bf34a` - ci: restrict privileged, self-hosted, and publishing jobs to microsoft/openvmm (v2.4 139; folds v2.4 383)
- 141 `6c478c865` - ci: run the checkin gates on the feature/microvm branches (v2.4 308)
- 142 `1be1c24d3` - vmservice: keep NVX wire-tag compatibility (v2.4 140)

</details>
