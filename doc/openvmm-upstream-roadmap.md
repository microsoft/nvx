# OpenVMM Micro-VM PR Strategy

## Scope

- OpenVMM fork base: `bf353460ff70e4a65627bb7003bed2cad1ce121f`, upstream main on 2026-11-21.
  The `feature/microvm/release/v8.0` reorganization kept the chain there, where the
  `feature/microvm/release/v7.2` rebase had moved it from
  `9a875b563d62eaca040137c0227224c9a17e944d`, upstream main on 2026-11-18; see
  [Upstream status](#upstream-status).
- Mapped fork tip: `c6e62f286b2879ca87da42e46de2fee887231d08`, position 126. The graph, PR
  summary, and commit mapping below cover positions 001-126, the PR20 fixups at positions 375 and
  376, the PR25 fixup at position 377, and the PR18 fixups at positions 389 and 393.
- NVX's pinned OpenVMM revision: `9822f732ae72c1e0dce91590842c0c395624a745`, position 394, the head
  of microsoft/openvmm#5286, whose commit follows position 393, `5af753b17`, the head of
  microsoft/openvmm#5285, whose commit follows position 392, `e61fa5326`, the head of
  microsoft/openvmm#5284, whose commit follows position 391, `c7f6a04fd`, the head of
  microsoft/openvmm#5283, whose commit follows position 390, `770bc61e7`, the head of
  microsoft/openvmm#5282, whose commit follows position 389, `350018ce4`, the head of
  microsoft/openvmm#5281, whose commit follows position 388, `1633b3797`, the head of
  `feature/microvm/dev` since microsoft/openvmm#5280 landed positions 386-388 on position 385,
  `043083f40`, where microsoft/openvmm#5279 had landed position 385 on position 384, `44747bf1c`,
  where microsoft/openvmm#5278 had landed position 384 on position 383, `fa239c823`, where
  microsoft/openvmm#5277 had landed position 383 on position 382, `acfaa5199`, where
  microsoft/openvmm#5275 and microsoft/openvmm#5276 had landed positions 381 and 382 on position
  380, `32e824728`, where microsoft/openvmm#5271, microsoft/openvmm#5272, and
  microsoft/openvmm#5273 had landed positions 378-380 on position 377, `3f5506f9d`, where
  microsoft/openvmm#5270 had landed positions 376 and 377 on position 375,
  `d44a113be`, where microsoft/openvmm#5262, microsoft/openvmm#5263, microsoft/openvmm#5264, and
  microsoft/openvmm#5265 had landed positions 371-375 on position 370,
  `aa145aff9`, where microsoft/openvmm#5260 and microsoft/openvmm#5261 had landed positions
  369 and 370 on position 368, `9805f8984`, where microsoft/openvmm#5254, microsoft/openvmm#5255,
  and microsoft/openvmm#5257 had landed positions 362-368 on position 361, `09966aa63`, where
  microsoft/openvmm#5248, microsoft/openvmm#5249, microsoft/openvmm#5251, microsoft/openvmm#5252,
  and microsoft/openvmm#5253 had landed positions 356-361 on position 355, `899dcd63f`, the head of
  the `feature/microvm/release/v8.0` branch of microsoft/openvmm. The
  v8.0 reorganization replayed the 370 commits of `feature/microvm/dev` at `09524b79c`, the pin
  before it, on their own fork base and folded the 15 fixups that the roadmap assigned to earlier
  commits, v7.2 positions 302, 316, 333, 335-340, 342, 345, 347, 349, 351, and 365, into the
  commits that they fix, so each later commit's position is its v7.2 position less the number of
  those fixups before it; see [The v8.0 reorganization](#the-v80-reorganization). The tree of
  `899dcd63f` differs from the tree of `09524b79c` only in rustfmt's layout of two `virt_kvm`
  tests. The pin is 268 commits past the mapped tip on the same first-parent chain. Positions
  127-374, 378-388, 390-392, and 394 are not yet assigned to PRs, positions 375 and 376 are PR20
  fixups, position 377 is a PR25 fixup, and positions 389 and 393 are PR18 fixups; see
  [Commits after the mapped tip](#commits-after-the-mapped-tip).
- Upstream main: `bf353460ff70e4a65627bb7003bed2cad1ce121f` (2026-11-21), the fork base; see
  [Upstream main past the fork base](#upstream-main-past-the-fork-base).

## Upstream status

The `feature/microvm/release/v8.0` reorganization kept the 370-commit chain of
`feature/microvm/dev`, through NVX's previous pin `09524b79c`, on `bf353460f`, which upstream main
had not passed, so it dropped no commit and nothing conflicted with upstream. It folded the 15
fixups of the v7.2 line into the commits that they fix, re-expressed for the code that those
commits have: the PR13 and PR18 fixups at v7.2 positions 302, 333, 340, 342, 345, 347, 349, 351,
and 365, which pull requests added after v7.0, the v7.1 rebase's fixup at v7.2 position 316, and
the v7.2 rebase's fixups at v7.2 positions 335-339. The tree of its head, `899dcd63f`, differs
from the tree of `09524b79c` only in rustfmt's layout of two `virt_kvm` tests; see
[The v8.0 reorganization](#the-v80-reorganization).

The `feature/microvm/release/v7.2` rebase moved the 335-commit chain of `feature/microvm/dev`,
through NVX's previous pin `30fe00786`, from `9a875b563` onto `bf353460f`, 27 upstream commits
later. It dropped v7.1 position 317, `2a5e77698`, which backported upstream's `0474df224`
(microsoft/openvmm#5160) before upstream landed it; [Landed upstream](#landed-upstream) lists it.
Replaying the remaining 334 commits had conflicts at 47 positions: 38 with microsoft/openvmm#5082
and one with microsoft/openvmm#5164, which merge guest-architecture and platform cfgs into
`cfg_select!` blocks; seven with microsoft/openvmm#4876, which impersonates Windows users in
virtio-fs, or microsoft/openvmm#5002, which extends the management RPC's virtio-fs options, one of
them, position 223, with microsoft/openvmm#5082 too; one with microsoft/openvmm#5040, which adds
configurable virtio-blk request queues; and one with microsoft/openvmm#5159, which moves the mirror
refresh to Azure Pipelines; [Conflicts of the v7.2 rebase](#conflicts-of-the-v72-rebase) lists them.
The patches of 274 replayed commits are unchanged, and those of 13 more differ only in context. Four
fixups at its tip, v7.2 positions 335-338, adapt the chain to microsoft/openvmm#4876,
microsoft/openvmm#5080, microsoft/openvmm#5002, and microsoft/openvmm#5040, which broke it without a
textual conflict, and a fifth, v7.2 position 339, completes the resolution of position 254 with
microsoft/openvmm#5082.

The `feature/microvm/release/v7.1` rebase moved the 315-commit chain of `feature/microvm/dev`,
through NVX's previous pin `6e5303f94`, from `e6b458cc0` onto `9a875b563`, 14 upstream commits
later. Upstream had landed none of its commits, so it dropped none, and every commit kept its
position. Replaying the chain conflicted at position 060 with microsoft/openvmm#4941, which boots an
x64 Linux child through a native MSHV root in Petri, and at position 277 with
microsoft/openvmm#4942, which handles MSHV's invalid VP register exits; [Conflicts of the v7.1
rebase](#conflicts-of-the-v71-rebase) lists them. Position 007 replayed with new context beside
microsoft/openvmm#4942's error variants, and the other 312 commits replayed with unchanged patches.
A fixup at its tip, v7.1 position 316, adapts the lock file entries that positions 069 and 133 add
to microsoft/openvmm#4941, which no textual conflict exposed.

The `feature/microvm/release/v7.0` reorganization moved the 315-commit chain of
`feature/microvm/dev`, through NVX's previous pin `fe61f9cbf`, from `7d90c4104` onto `e6b458cc0`,
eight upstream commits later. Upstream had landed none of its commits, so it dropped none.
Replaying the chain had conflicts at three positions, two with microsoft/openvmm#4862, upstream's
USB command line, and one with microsoft/openvmm#5070, which gates the Patina nightly jobs on a
label, and the generated workflows of one more needed regenerating;
[Conflicts of the v7.0 rebase](#conflicts-of-the-v70-rebase) lists them. No upstream change broke
the chain without a textual conflict. The reorganization then folded the 19 fixups of the v6.1
line into the commits that they fix, moved two `vmcore` timer commits that PR10's RTC fixups need
into PR10, and formatted the `virt_kvm` x86-64 files of six commits, which `cargo fmt` does not
reach; see [The v7.0 reorganization](#the-v70-reorganization).

The `feature/microvm/release/v6.1` rebase moved the 268-commit chain of `feature/microvm/dev`,
through NVX's previous pin `5ca20ee08`, from `ba105dd7f` onto `7d90c4104`, 44 upstream commits
later. It dropped v6.0 positions 002, 008, 009, 014, and 051-053, whose changes upstream had
landed, all but 002 after review changes; [Landed upstream](#landed-upstream) lists them.
Replaying the remaining 261 commits had conflicts at 16 positions, 11 of them with
microsoft/openvmm#4553, upstream's SaveVM and RestoreVM, which changes the snapshot manifest and
publication of PR11, and the generated workflows of one more needed regenerating;
[Conflicts of the v6.1 rebase](#conflicts-of-the-v61-rebase) lists them. Eight fixups at its tip,
v6.1 positions 262-269, adapted the chain to upstream changes that no textual conflict exposed. The
patches of 220 replayed commits are unchanged, and those of 25 more differ only in context.

The `feature/microvm/release/v6.0` reorganization kept the 223-commit chain of
`feature/microvm/dev`, through NVX's previous pin `3f2ab4c2f`, on `ba105dd7f`, which upstream main
had not passed, so it dropped no commit and nothing conflicted with upstream. It folded the PR20
fixup at v5.1 position 148 into v6.0 position 097 and the PR12 fixup at v5.1 position 221 into
v6.0 positions 035 and 036, combined v5.1 positions 222 and 223 into v6.0 position 220, and
checked that every commit builds; the pin's tree is the tree of `3f2ab4c2f`. See
[The v6.0 reorganization](#the-v60-reorganization).

The `feature/microvm/release/v5.1` rebase moved the 219-commit chain of `feature/microvm/dev`,
through NVX's previous pin `db42fde04`, from `61994293a` onto `ba105dd7f`, nine upstream commits
later. Upstream had landed none of its commits, so it dropped none, and every commit kept its
position. Replaying the chain conflicted only at v5.1 position 183, now position 177, which
[Conflicts of the v5.1 rebase](#conflicts-of-the-v51-rebase) lists; the other 218 commits replayed
with unchanged patches, and no upstream change broke the chain without a textual conflict, so the
rebase added no fixups.

The `feature/microvm/release/v5.0` reorganization moved the 220-commit chain of
`feature/microvm/dev`, through NVX's previous pin `e1ca353eb`, from `c6d3da0ff` onto `61994293a`,
61 upstream commits later. It dropped v4.0 positions 004, 017, and 217, whose changes upstream had
landed, and v4.0 position 023, whose stop before teardown microsoft/openvmm#4863 supersedes;
[Landed upstream](#landed-upstream) lists them. Replaying the remaining commits had conflicts at 31
positions, which [Conflicts of the v5.0 rebase](#conflicts-of-the-v50-rebase) lists, and 15
fixups adapted commits to upstream changes that no textual conflict exposed. The reorganization
then folded those fixups into the commits that they fix, retired PR09's bounded quiesce
transactions of v4.0 position 021, folded the snapshot quiesce without a deadline of v4.0 position
216 and PR13's fixups at v4.0 positions 218 and 219 into the commits that they change, and fixed
what checking each commit found; see [The v5.0 reorganization](#the-v50-reorganization).

The `feature/microvm/release/v4.0` reorganization moved the 205-commit chain of
`feature/microvm/release/v3.0`, through NVX's previous pin `35e76265f`, from `b2676222b` onto
`c6d3da0ff`, 30 upstream commits later. It dropped v3.0 positions 010, 012, 017, and 022-025,
whose changes upstream had landed, and position 016, whose stop cancellation
microsoft/openvmm#4835 supersedes; [Landed upstream](#landed-upstream) lists them. Replaying the
remaining commits had conflicts at 16 positions, which
[Conflicts of the v4.0 rebase](#conflicts-of-the-v40-rebase) lists, and two fixups adapted
v4.0 positions 017 and 128 to upstream changes that no textual conflict exposed. The
reorganization then retired the fallible device start of v3.0 position 027, folded
microsoft/nvx#1232's awaited device restoration into the chain, and added the ABI restart at v4.0
positions 197-210; see [The v4.0 reorganization](#the-v40-reorganization).

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
| v7.2 | 317 | `2a5e77698` - pal_async: treat disconnected pipe event query as EOF | - | microsoft/openvmm#5160 | `0474df224` |
| v6.1 | 002 | `707747c16` - flowey: distinguish native Windows and WSL nextest paths | PR01 | microsoft/openvmm#4595 | `5d3ab02f0` |
| v6.1 | 008 | `299a04d4f` - pal: query Unix socket peer credentials | PR03 | microsoft/openvmm#4591 | `42d4fe619` |
| v6.1 | 009 | `a0e70d99e` - pal: resolve the process token user SID | PR04 | microsoft/openvmm#4604 | `6937acefb` |
| v6.1 | 014 | `5ed5f7a22` - serial_socket: keep Windows named-pipe listeners reconnectable | PR06 | microsoft/openvmm#4609 | `f5ef11b8a` |
| v6.1 | 051 | `6cea1ae18` - loader: let static ELF loading reuse a caller buffer | PR14 | microsoft/openvmm#4554, which supersedes it | `e4728b7f4` |
| v6.1 | 052 | `4ccef3c27` - loader: add an Intel MP 1.4 table builder | PR14 | microsoft/openvmm#4554 | `e4728b7f4` |
| v6.1 | 053 | `b585c7300` - loader: add firmware-less Linux direct boot with MP tables | PR14 | microsoft/openvmm#4554 | `e4728b7f4` |
| v5.0 | 004 | `6476a457f` - consomme: make TCP window-reopen validation deterministic | PR01 | microsoft/openvmm#4598 | `19485f88e` |
| v5.0 | 017 | `ea1153ef4` - consomme: bound active TCP, UDP, and ICMP flows | PR07 | microsoft/openvmm#4600 | `e89a7e22d` |
| v5.0 | 023 | `0692a461c` - openvmm_core: stop the devices before tearing down the VM | PR09 | microsoft/openvmm#4863, which supersedes it | `0c9697277` |
| v5.0 | 217 | `62b427a38` - serial_socket: report read errors as EOF and drop the connection | PR06 | microsoft/openvmm#4605 | `48e0681e9` |
| v4.0 | 010 | `1a8bc4363` - pal: add handle-relative file access that does not follow reparse points | PR04 | microsoft/openvmm#4593 | `789d51653` |
| v4.0 | 012 | `d25e0e13f` - pal_async: fix named-pipe reconnect and already-connected listen | PR04 | microsoft/openvmm#4585 | `03e8ae741` |
| v4.0 | 016 | `47102941e` - virtio_blk: let stop cancellation win over a ready queue and drain accepted I/O | PR06 | microsoft/openvmm#4835, which supersedes it | `79e67ae36` |
| v4.0 | 017 | `41460f3f7` - virtiofs: make handle and inode ID allocation fallible | PR06 | microsoft/openvmm#4594 | `389e4e57a` |
| v4.0 | 022 | `38998a580` - openvmm_helpers: split the snapshot module without behavior changes | PR08 | microsoft/openvmm#4616 | `0812555fe` |
| v4.0 | 023 | `313b5fe0b` - net_tap: move the transmit path into its own module | PR08 | microsoft/openvmm#4617 | `c696625c6` |
| v4.0 | 024 | `238c6cc96` - net_consomme: move queue transmit processing into a module | PR08 | microsoft/openvmm#4620 | `3bb37a191` |
| v4.0 | 025 | `c1bd1e0a5` - virtio_console: move the direct receive loop into its own module | PR08 | microsoft/openvmm#4621 | `2a17dfdd5` |
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
the v2.4 rebase, `feature/microvm/release/v2.4` for the v3.0 reorganization,
`feature/microvm/release/v3.0` for the v4.0 reorganization, `feature/microvm/dev` at
`e1ca353eb`, whose first 210 commits are those of `feature/microvm/release/v4.0`, for the v5.0
reorganization, `feature/microvm/dev` at `5ca20ee08`, whose first 220 commits are those of
`feature/microvm/release/v6.0`, for the v6.1 rebase, and `feature/microvm/dev` at `30fe00786`,
whose first 316 commits are those of `feature/microvm/release/v7.1`, for the v7.2 rebase.
microsoft/openvmm#4661 names the `acpi_rsdp_addr` field of `boot_params` exactly as position 074
did, so the v2.3 rebase dropped 074 as empty. microsoft/openvmm#4554 carried it ahead of v6.0
positions 051-053 until it merged.

v7.1 position 317, which no roadmap PR held, was upstream's `0474df224`, cherry-picked unchanged
so that `pal_async`'s `client_read_completes_when_server_disconnects` passed OpenVMM's
`unit tests [x64-windows]` job, which NVX requires, before microsoft/openvmm#5160 reached the fork
base. The v7.2 base has it, and microsoft/openvmm#5165 adds tests of the same handling there, so
the v7.2 rebase dropped it. No other commit of the chain changes `pal_async`'s pipe code.

The v6.1 rebase took upstream's versions of its drops and adapted the later commits to them.
microsoft/openvmm#4595 is identical to v6.0 position 002, and microsoft/openvmm#4591 adds tests to
v6.0 position 008's `credentials` module of `pal`, whose API the chain uses unchanged.
microsoft/openvmm#4604 resolves the token user SID in `pal::windows::security` itself rather than in
the `user_sid` module of v6.0 position 009, so the fixup at v6.1 position 262, which the v7.0
reorganization folded into positions 105 and 112, calls it there. microsoft/openvmm#4609 returns a
failed write, flush, or close of a Windows named pipe and returns its backend to listening, as v6.0
position 014 did, with reconnection tests of its own in place of 014's, so position 105's call of
`disconnect_current` in 014's test moved into upstream's. microsoft/openvmm#4554 merged the MP table
builder and the firmware-less Linux direct boot of v6.0 positions 052 and 053 after review moved the
boot into `loader::linux::mptable_boot`, rebuilt the table with `zerocopy` from the MP
specification's structures, numbered the I/O APIC after the processors, and loaded the kernel and
initrd through upstream's `load_kernel_and_initrd_x64`, without the buffer reuse that v6.0 position
051 added for it. Position 185's routing of every IOAPIC input moved onto that builder, and the
fixup at v6.1 position 263, which the v7.0 reorganization folded into positions 057 and 178, loads
the microVM through `mptable_boot`; see [Conflicts of the v6.1
rebase](#conflicts-of-the-v61-rebase).

microsoft/openvmm#4863 gives devices a consuming teardown. The VM worker's run drains pending
device removals, and its pause stops the state units before the worker tears down the chipset
devices and the partition, which is what v4.0 position 023, which had no upstream pull request,
made it do, so that commit was empty on the new base and the v5.0 reorganization dropped it.

microsoft/openvmm#4835 makes devices answer task-control requests promptly. Its virtio-blk
tests `stop_drains_one_accepted_write_exactly_once` and `stop_preempts_continuously_ready_queue`
cover what v3.0 position 016 made virtio-blk do, so the v4.0 rebase dropped 016, and its pull
request, microsoft/openvmm#4588, was closed. No later commit of the chain changes `virtio_blk`.

microsoft/openvmm#4583 saves each IOAPIC input's last observed level as edge-detector history, as
v2.4 position 016 did, but restores the redirection entries, routes, and input history without
evaluating the inputs, which 016 did. A restored asserted level-triggered input therefore raises
its interrupt when its producer republishes it, when the guest's EOI clears the remote IRR, or
when the guest writes its redirection entry, not during the restore itself.

Upstream merged most of the v2.4 drops after review changes, so the v2.4 rebase took upstream's
versions and adapted the later commits to them. microsoft/openvmm#4557 creates a copy-on-write
mappable without the chain's `executable` flag, and on Windows it refuses to unmap or map over part
of a written copy-on-write view. microsoft/openvmm#4582 answers a pipe's peer process through
`PipeExt::get_pipe_peer_process_id` instead of the chain's `pal::windows::pipe::peer` module, which
also had `is_pipe`. microsoft/openvmm#4586 fast-forwards more counting modes, and
microsoft/openvmm#4606 forwards console input in `repl.rs` instead of the `headless` module, which
position 112 creates for the chain's own use, and ends the run when a stdin that is not a terminal
ends, which position 112 confines to machines other than the microVM. microsoft/openvmm#4587 is
identical to v2.3 position 005.

microsoft/openvmm#4522 fixes the lost-PIC-interrupt bug of v2.1 position 022 in a different way.
Instead of tracking the queued vector in `kvm::VpRunner`, it injects PIC interrupts through the
vCPU's injected-interrupt slot (`KVM_SET_VCPU_EVENTS`), which the existing activity save/restore
already covers. Its saved state has no slot for v2.1 position 022's queued vector: KVM snapshots
from builds with position 022 persist a queued PIC vector as a pending `ExtInt` event. Current
builds reject restoring one with `KvmError::InvalidState`, so recapture KVM snapshots that those
builds captured.

Upstream also landed parts of commits that remain in the chain:

- microsoft/openvmm#4556 (`96647c331`) derives `Clone` for `ProtobufMessage` and `ProtobufAny`.
  Position 004 keeps its subject but now only adds `ProtobufAny::value_len` and checks a clone in
  the round-trip test.
- microsoft/openvmm#4524 (`9cf109547`) adds the generic `PartitionTimeControl` contract, MSHV and
  WHP implementations, and full-VM freeze/thaw lifecycle calls. Position 025 reuses the MSHV
  implementation, through fallible freeze and thaw helpers, instead of private plumbing. Positions
  023-027 keep the exact TSC-frequency exposure, snapshot downtime handling, and restored-vCPU
  synchronization that upstream lacks, and position 092 synchronizes only the VPs that a restore
  creates.
- microsoft/openvmm#4704 (`1c093df84`) adds the architectural `TscDeadline` VP state element, the
  `tsc_deadline` partition capability, and the KVM, MSHV, WHP, and OpenHCL register accessors that
  v2.4 position 038 also added. Position 022 now uses upstream's, so it keeps the `tsc_deadline`
  field of the CPU compatibility contract, the finalized MSHV partition's report of no TSC-deadline
  support, and the LAPIC timer helpers that position 142 deletes in favor of those that position 134
  adds for the time ABI. Upstream compares a restored deadline on partitions that can freeze time,
  as WHP can, whereas 020 never compared it, so the WHP state access of position 026, which reads
  the deadline before the APIC state, discards that read when it restores the deadline.
- microsoft/openvmm#4764 (`29f0adceb`) makes every lxutil volume resolve paths beneath its root
  without following a host symbolic link, as position 125 did for a confined volume, and
  microsoft/openvmm#4783 (`7c92b147e`) rejects unsafe opens. Position 125 now only adds
  `LxVolumeOptions::confine_paths`, which makes a Windows volume create LX symlinks, and runs its
  confinement tests against upstream's implementation. A share therefore needs `openat2`, Linux 5.6
  or later, where the chain fell back to opening each component with `O_NOFOLLOW`.

### Open upstream pull requests

As of 2026-11-23, every commit of PR01-PR03, PR05, PR06, and PR09 except positions 001, 003, 004,
and 012 is in an open upstream pull request; the commits that upstream has merged are under [Landed
upstream](#landed-upstream), and PR04, PR07, and PR08 have landed whole. Each pull request carries
one commit. Of those 10 pull requests, only microsoft/openvmm#4597, microsoft/openvmm#4613, and
microsoft/openvmm#4618 are ready for review: microsoft/openvmm#4607 has been a draft again since
2026-11-06, and the other six since 2026-11-10. None has an approving review.
microsoft/openvmm#4451, which proposed position 001, was closed without merging, and the remainder
of position 004 has not been proposed. microsoft/openvmm#4615, which proposed position 003, was
closed on 2026-11-10, since a Windows servicing release fixes the WHP behavior that it works around;
the chain keeps it. PR14's remaining commit, position 049, is in no pull request either:
microsoft/openvmm#4611, which proposed it, was closed on 2026-11-10, after review suggested
combining it with the commit that needs it. microsoft/openvmm#4619, microsoft/openvmm#4622, and
microsoft/openvmm#4623, which proposed the fallible unit start, device start, and VM resume of v3.0
positions 026-028, were closed in favor of microsoft/nvx#1232: position 011 now only rolls back a
partial start, v3.0 position 027 is retired, and position 012's resume stays fallible; see [The v4.0
reorganization](#the-v40-reorganization) and [The device restore
contract](#the-device-restore-contract). PR10-PR13 and PR15-PR27 have no upstream pull requests
yet.

On 2026-11-04, microsoft/nvx#1260 withdrew v4.0 position 021 from PR09, since the deadline of the
snapshot quiesce added no safety: microsoft/openvmm#4625, which proposed its bounded quiesce
transactions, was closed, and the v5.0 reorganization retired the commit and folded the quiesce
without a deadline into the commits that it changes; see [The v5.0
reorganization](#the-v50-reorganization). microsoft/openvmm#4872, opened on 2026-11-03, proposes
position 011's rollback of a partial start, with review changes that the chain lacks, apart from its
handling of a unit in a terminal state, its teardown after a failed restart, and its rollback's stop
in reverse dependency order, which position 011 carries, since the v7.0 reorganization folded the
PR09 fixups at v6.1 positions 234-236 into it. microsoft/openvmm#4626, which proposes position 014's
input gate without a deadline, as the chain now has it, builds on it. So does
microsoft/openvmm#4937, a draft since 2026-11-10, which makes `StateUnits::stop` fail instead of
panicking when a unit's task has ended, as position 236 does in the chain. Position 236 also adapts
the chain's own callers of `StateUnits::stop`, such as the microVM worker's snapshot quiesce, and
makes `VmRpc::Pause` fail where #4937's VM worker panics on a failed stop.

On 2026-11-05, microsoft/openvmm#4605 merged as `48e0681e9`, narrowed in review to the read path: a
read error is traced, drops the socket, and is reported as EOF, while writes keep upstream's
handling, which reports a broken pipe as a successful write and keeps the socket. Review settled
disconnect detection in virtio-console instead, with microsoft/openvmm#4778, which every fork base
since v2.4 includes. Position 039, which carried the pull request's first revision and drops the
socket and returns the error on any failed write, therefore has no upstream pull request. The chain
keeps it for output rather than disconnects: a virtio-console with the `Retain` policy of PR13's
position 040 and the portb console of PR15's position 055 keep the guest output written after their
peer leaves only when that write fails, and on Linux a write to an AF_UNIX peer that has closed
fails with `EPIPE`, which upstream's backend reports as successful. Dropping it as
microsoft/openvmm#4605's commit would make `Retain` consoles and portb lose that output on Linux.
The v5.0 reorganization therefore dropped v4.0 position 217, which cherry-picked `48e0681e9` for
PR06, and moved v4.0 position 015 into PR13 as position 039, ahead of the policy that needs it, with
v4.0 position 218, which explains and tests that write handling, folded into it. PR13's position 041
took the test of v4.0 position 219 that a `Retain` console keeps that output for the next peer.

microsoft/openvmm#4609, which v6.0 position 014 carried, had the same split, but its review kept
the write handling: since it merged as `f5ef11b8a` on 2026-11-11, a Windows named pipe returns a
failed write, flush, or close and drops its client, so a `Retain` console on a named pipe keeps
that output too. The v6.1 rebase therefore dropped 014 as landed.

On 2026-11-10, microsoft/openvmm#4553, upstream's SaveVM and RestoreVM for the management RPC, which
proposes no chain commit, merged as `64828d2b5`. It records a snapshot ID in the manifest's field
18, to which it moved the ID from field 8, the chain's `state_size_bytes`, after microsoft/nvx#1210
raised the clash, and it reserves fields 19 and 20 for `compatibility_bundle_digest` and
`memory_artifacts`, all three of which position 226 had kept free. It also changes `write_snapshot`
in the snapshot publication of `openvmm_helpers`, which PR11's positions 029-031 and later chain
commits change. The v6.1 rebase therefore gives the chain's `SnapshotManifest` upstream's
`snapshot_id`, which a microVM capture leaves zero, publishes upstream's snapshots through the
chain's staged publication, and keeps microVMs out of RestoreVM; see [Conflicts of the v6.1
rebase](#conflicts-of-the-v61-rebase). NVX's pin of the rebase has `nvx`'s reader of a template's
manifest, which refuses fields that it does not know, accept field 18. Positions 313 and 314 record
the field map that the two manifests share, and refuse a field 18 that is not a GUID of 16 bytes by
its field; see [Commits after the mapped tip](#commits-after-the-mapped-tip).

| Roadmap PR | Positions | Upstream pull requests, in position order |
| --- | ---: | --- |
| PR01 | 002 | microsoft/openvmm#4618 |
| PR02 | 005 | microsoft/openvmm#4558 |
| PR03 | 006 | microsoft/openvmm#4589 |
| PR05 | 007-009 | microsoft/openvmm#4536, microsoft/openvmm#4607, microsoft/openvmm#4613 |
| PR06 | 010 | microsoft/openvmm#4597 |
| PR09 | 011, 013, 014 | microsoft/openvmm#4872, microsoft/openvmm#4624, microsoft/openvmm#4626 |

Stacked pull requests carry their prerequisites' commits: microsoft/openvmm#4613 builds on
microsoft/openvmm#4536, and microsoft/openvmm#4626 builds on microsoft/openvmm#4872.
Most of these pull requests have gained review commits that the fork chain does not carry. When
one merges, the next rebase should drop its fork commit, as the `microvm/v2.2`,
`feature/microvm/release/v2.3`, `feature/microvm/release/v2.4`, `feature/microvm/release/v6.1`,
and `feature/microvm/release/v7.2` rebases and the v3.0, v4.0, and v5.0 reorganizations dropped the
40 above, and adapt later commits to the merged form.

The v3.0 reorganization folded fixups into positions 005, 008, 009, and 049, and the v4.0
reorganization changed positions 011 and 014, whose pull requests lack those changes; see [The v4.0
reorganization](#the-v40-reorganization) and [The v3.0 reorganization](#the-v30-reorganization). The
v5.0 reorganization adapted positions 002, 005, 010, 011, and 014 to upstream main, and folded into
position 014 the removal of its input gate's deadline, which microsoft/openvmm#4626 already lacks;
see [The v5.0 reorganization](#the-v50-reorganization). The v6.1 rebase adapted position 006 to
microsoft/openvmm#4591; see [Conflicts of the v6.1 rebase](#conflicts-of-the-v61-rebase).

### The device restore contract

microsoft/nvx#1232 asked to agree the contract between a device's restore, its preparation, and its
activation with the authors of microsoft/openvmm#4713 and microsoft/openvmm#4729, drafts since
October 2026, and to record the final architecture here. NVX settled the contract itself, reusing
their designs where they apply. The chain implements it as of position 311, without a new element,
field, or version of any saved state:

- **Restore** runs while the VM is stopped, and is awaited, through upstream's chipset and
  device-worker restore, microsoft/openvmm#4799, after PR09's position 013 checks the exact unit
  inventory. Each device validates its saved state, applies it, and prepares the host resources that
  the state refers to, as virtio-fs reopens the files that the guest held open, so the contract has
  no preparation step apart from the restore. So invalid state, a missing host resource, or a failed
  device worker fails the restore itself, before it reports readiness, even for a VM that stays
  paused. A virtio device's restore has no effect that the guest sees: it starts no queue, so it
  admits no host input and sends no queue notification, and since position 308 it drives no
  interrupt line. Position 307 programs each restored MSI-X vector without delivering, so that a
  vector delivers to its message after the start without the guest programming it again.
- **Start** runs what a restore prepared: each device starts its queues from their restored
  progress, after a legacy virtio-mmio or PCI INTx line goes to its restored level, and the VM
  confirms every device's start, through position 213, before any vCPU runs. The steps that can
  still fail are those that need a live host backend or a running queue, and each fails the start
  instead of being logged and dropped: virtio-net's endpoint creates its queues (position 207), each
  queue builds its wait and its ring, virtio-console checks its restored partial transmission
  against the queue that it starts, and a remote device worker answers the confirmation, which fails
  if the worker has ended (position 310). A failed start stops the devices that started again and
  keeps the state that the stop records: the progress of every queue (position 212), which includes
  the requests that a queue that did start completed before another failed, since the confirmation
  keeps the vCPUs from running but does not undo a device's I/O, and each device's saved private
  state in step with it (positions 306 and 311). It publishes no restore readiness and runs no vCPU,
  so that a later resume retries the start from that state, and a save, where the VM allows one,
  saves it.
- **Stop** releases each queue with its exact progress, which the transport keeps, so that an
  ordinary pause and resume keeps queue progress without a restore, as position 208 tests on both
  transports. A device keeps its host resources across a stop, and releases them as it is removed or
  the VM ends, as after a failed restore; position 208 also tests the removal of a restored device
  before its first start.

Each activation path follows this contract. The first run of a restored VM, paused or not, an
ordinary resume, the resume after a capture, and the start of the units again after a failed save
all go through the confirmed start; a unit that a hot-add adds to a running VM starts through
`start_stopped_units`, which returns the failure; a cold boot starts a device's queues when the
guest sets `DRIVER_OK`, and a queue that cannot start fails `DRIVER_OK`, which resets the device's
status, since the guest already runs; and a microVM refuses a worker restart (PR15's position 058).
Upstream's remote device proxy still panics when its worker ends during a stop, a reset, a poll, or
a teardown; only the TPM uses a remote device worker, and no microVM has one, so the chain leaves
that to upstream.

The chain departs from two upstream drafts, where they would not meet microsoft/nvx#1232 or would
change saved state, and adopts each at the rebase after it lands:

- microsoft/openvmm#4713 reconstructs virtio-net's queues at restore, in a device task parked under
  `TaskControl`, and keeps them across an ordinary pause with a retained pause, separate from the
  stop that releases them. It still creates the host backend's queues at activation, so the chain
  instead validates queue progress at restore and builds the queues at start, where a failure fails
  the confirmed start. The chain has no retained pause: microVM NICs, which have a static identity,
  keep their queue progress through the stop (PR13's position 046). The retained pause of other
  NICs, and the independent ring enable of vhost-user of microsoft/openvmm#4729, stay upstream's.
- microsoft/openvmm#4855 replays the level of every line set after its targets start. The chain
  holds only the virtio transports' level lines until their device starts (position 308).

RestoreVM follows the contract too: microsoft/openvmm#4553's RestoreVM, which the v6.1 rebase took,
restores a standard VM, whose resume callers handle the fallible resume of PR09's position 012, and
refuses a microVM, which restores through CreateVM. microsoft/openvmm#4676, an open upstream pull
request that adds an eager-copy RestoreVM, changes no device contract, so the chain takes it at the
rebase after it lands. No restore-at-start plumbing remains to remove: the v4.0 reorganization
stopped staging device state for activation; see [The v4.0 reorganization](#the-v40-reorganization).

### The v8.0 reorganization

A v7.2 position in this section counts in `feature/microvm/dev` at `09524b79c`, whose first 339
commits are those of `feature/microvm/release/v7.2`, and a position without a release is a v8.0
position. The reorganization folded 15 commits, so v7.2 positions 001-301 keep their numbers; v7.2
positions 303-315 are positions 302-314, 317-332 are 315-330, 334 is 331, 341 is 332, 343 and 344
are 333 and 334, 346 is 335, 348 is 336, 350 is 337, 352-364 are 338-350, and 366-370 are 351-355.
A position from 302 on in a section on an earlier release counts in that release.

The reorganization replayed the 370 commits of `feature/microvm/dev` at `09524b79c` on their own
fork base, `bf353460f`, which upstream main has not passed: upstream had landed none of their
changes, and none conflicted with upstream. It folded the 15 fixups that the roadmap assigned to
earlier commits into the commits that they fix, re-expressed for the code that those commits have.
Each commit that takes a fold keeps its author and date, and its message lists the subjects that
it folds. Positions 301 and 304 also take rustfmt's layout of a test that each adds, in
`vmm_core/virt_kvm/src/arch/x86_64/smm.rs` and `msr_state.rs`: `virt_kvm` declares their module
inside `cfg_select!`, where the `mod` walk of `cargo xtask fmt` does not reach, but a check of the
Rust files that a commit changes formats them. The pin's tree differs from the tree of `09524b79c`
only in that layout. The table lists where each change went.

| v7.2 position | Change | v8.0 positions |
| ---: | --- | --- |
| 302 | `8d71cff3a` - microVM: refuse a decoded network identity other than the derived one | 078, 093, 193 |
| 316 | `335ebf312` - cpu_profile, openvmm_helpers: name sha2 as upstream's lock file does | 069, 133 |
| 333 | `2dbfb495a` - virtiofs: preflight a microVM object of another file type as a new node | 081, 083 |
| 335 | `8d989e670` - microVM: adapt virtio-fs to upstream's Windows impersonation | 081, 082, 085, 119, 120, 159, 198, 223 |
| 336 | `5dbe25966` - virt_support_x86emu: convert exception codes with u16::from | 143 |
| 337 | `d22ea2c5a` - vmservice: refuse virtio-fs mount options on a microVM | 119, 198, 223 |
| 338 | `942eeeda1` - openvmm_entry: keep the microVM sandbox blocks' fixed queue shape | 086, 087, 171, 223 |
| 339 | `b51f9156c` - openvmm_entry: build with_big_stack only where it runs | 254 |
| 340 | `32888344a` - virtiofs: keep node names when renaming a name onto itself | 048, 166 |
| 342 | `5c82edac4` - virtiofs: bound microVM directory snapshots in total when listing | 083, 164 |
| 345 | `8986cc537` - virtiofs: stop failed microVM READDIRPLUS from leaking lookup references | 083 |
| 347 | `a533d6a55` - virtiofs: refuse device nodes and whiteouts on a microVM share | 081, 085, 164, 166 |
| 349 | `2a483a06f` - virtiofs: keep node names when renaming one hard link onto another | 048, 166 |
| 351 | `8bea555db` - virtiofs: skip gone or hidden entries in microVM READDIRPLUS | 083, 160 |
| 365 | `78ddb8044` - virtiofs: restart a restored microVM's handles at 1 on a reset | 083, 350 |

- v7.2 position 302 folds into PR18's position 078, which adds the network identity, its parser,
  and the check of a decoded identity's prefix: `validate_machine_config` refuses an identity other
  than the one that `FromStr` derives from its IPv4 address and prefix. There, its test checks the
  six IPv4 cases and that the identity check accepts each identity that the parser builds, since
  the configuration of position 078's test helper has no RAM. PR20's position 093, which adds
  `valid_microvm_config`, gives the test that configuration, which `validate_machine_config`
  accepts with each identity that the parser builds. Position 163, which adds an optional IPv6
  identity, has the check compare an identity without one with the derived identity without its
  IPv6 identity, as a snapshot whose network predates IPv6 restores it, and the test check that
  such an identity passes; position 186 drops the hypervisor argument of the command-line check
  from the test, and position 193, which gives every NIC an IPv6 identity, takes the check's final
  form and the test's eleven IPv6 cases.
- v7.2 position 316 folds into PR17's position 069 and position 133, which add `sha2` to the lock
  file entries of `openvmm_helpers` and `cpu_profile`; `cargo metadata` rewrites neither lock file.
- v7.2 position 333 folds into PR18's position 081, whose microVM preflight of an insert it
  corrects, and its tests into position 083, which adds the `VirtioFsInode::volume` that they call.
- v7.2 position 335 folds into each commit that builds what it adapts to microsoft/openvmm#4876:
  position 081's microVM device and resolver, with the resolver's test of Windows options,
  position 082's `HostFs` value, position 085's dormant device, position 119's management RPC
  attachment, which refuses `windows_options`, its tests, which pass upstream's
  `ResourceRegistry`, and upstream's handle-passing VMM test and `virtio_fs_windows_options` test,
  which leave position 119's fields at their defaults, position 120's exit status test, the
  resolver tests' `host_fs` helper, which position 159 adds, and the wire test and gRPC guide of
  positions 198 and 223.
- v7.2 position 336 folds into position 143, which makes `virt` depend on `cpu_profile`.
- v7.2 position 337 folds into positions 119, 198, and 223: the RPC's microVM attachment refuses a
  non-empty `mount_options`, the access mode test passes `ro` through them, and the wire test and
  the gRPC guide encode `mount_options` as field 3 from position 198 on. Position 124, which keeps
  the legacy tags 3 and 4 for the microVM mount fields, had moved upstream's `read_only` to field
  5; since upstream replaced it, its tests check that `mount_options` takes the field 5 that the
  v7.2 rebase gave it there, and its guide and message name `mount_options` and `windows_options`
  at fields 5 and 6.
- v7.2 position 338 folds into PR19's positions 086 and 087 and into positions 171 and 223, which
  build the sandbox blocks and their `DiskCli` values; position 239 keeps `num_queues` in the
  `DiskCli` that it moves.
- v7.2 position 339 folds into position 254.
- v7.2 positions 340 and 349 fold into PR13's position 048, which tracks the names that they keep
  through a rename onto the same name or onto another link of the same object: 340's early return
  precedes the bookkeeping of a move, and 349's check follows the host rename, with
  `names_one_object` beside `supports_stable_file_id`. Their tests fold into position 166, which
  adds the exchange tests that they extend.
- v7.2 position 342 folds into PR18's position 083, which adds the directory listings that it
  bounds in total, and its budget in `new_microvm_aggregate` and its tests into position 164,
  which adds that constructor and now imports the policy tests' `request` helper for them.
- v7.2 position 345 folds into PR18's position 083, which adds the `READDIRPLUS` of a listing
  whose failure it fixes. There, an entry takes its lookup reference before the check that its
  name still reaches the listed object, so through position 329 the fold also releases the
  reference of an entry that fails that check; position 330, v7.2 position 332, takes the
  reference only after the check.
- v7.2 position 347 folds into PR18's position 081, which adds the microVM paths of `mknod` and
  `rename`, with the `RENAME_EXCHANGE` constant that its rename check names and that position 160
  would add. Its guide's device-node row and security paragraph fold into position 085, which adds
  the virtio-fs page, its rename-flags row into position 166, which adds the row, and its tests
  into position 164.
- v7.2 position 351 folds into PR18's position 083, which adds the `READDIRPLUS` that it lets go
  on past a name that is gone or hidden, and its test of a hard link to a hidden file into position
  160, which adds the access policy that hides the file; until then, the comment on the skip names
  the host's `EACCES` rather than the policy.
- v7.2 position 365 folds into PR18's position 083, which adds the restore of the handle table
  whose first handle it keeps, and its test into position 350, v7.2 position 364, whose full table
  at `u64::MAX` the test restores and then resets.
- Positions 124, 163, 186, 239, and 330 adapt to a folded change, as the list says, and position
  331, v7.2 position 334, no longer adds the two imports of its tests that the folds into positions
  083 and 164 add. The patches of positions 103, 126, 156, 158, 165, and 182 differ only in their
  context.

The chain has no merges, and every commit has a good signature. Against `09524b79c`, a range diff
finds the patches of 317 commits unchanged, the 15 fixups folded, and those of 38 commits changed:
the 24 that take folds, the six adaptations, the six changes of context, and positions 301 and 304.

Every commit of the v8.0 chain builds. At each of the 355 commits, on x86_64 Linux and on x86_64
Windows, `vtpm_util` included, `cargo check --locked --workspace --all-targets` passes, as do
`cargo xtask fmt --pass lints`, rustfmt on the Rust files that the commit changes, and, at each
commit that changes the pipelines, positions 002, 112, 122, 123, and 181,
`cargo xtask fmt --pass verify-flowey`. At the pin, `cargo xtask fmt` passes on both hosts, as
does `cargo clippy --workspace --all-targets -- -D warnings`, on Windows without
`guest_test_uefi`, `opentmk_executor`, and `vtpm_util`; the unit tests of the packages that the
folds, the adaptations, and the layout change pass on each host that builds them, and the microVM
VMM tests pass on KVM and WHP.

### Conflicts of the v7.2 rebase

A v7.1 position in this section counts in `feature/microvm/dev` at `30fe00786`, whose first 316
commits are those of `feature/microvm/release/v7.1`, and a position without a release is a v7.2
position. The v7.2 rebase dropped v7.1 position 317, so v7.1 positions 001-316 keep their numbers
and v7.1 positions 318-335 are positions 317-334. A position from 317 on in a section on an earlier
release counts in that release.

The `feature/microvm/release/v7.2` rebase replayed the 334 remaining commits onto `bf353460f` with
conflicts at 47 positions, which it resolved in place. Each is before position 317, so the table
lists each resolution at the position that the commit had in v7.1 too. Each resolution keeps
upstream's structure, such as its `cfg_select!` blocks, its `ParsedVirtioFs` and
`VirtioFsDeviceOptions`, and its virtio-blk options, and applies the commit's change to it.

| Position | v7.2 commit | v7.1 commit | Upstream PR | Resolution |
| ---: | --- | --- | --- | --- |
| 003 | `56a94131a` | 003 `75e56fa86` - virt_whp, petri: enable native PCIe NVMe boot on WHP | microsoft/openvmm#5082 | `cpu_contract` is declared in the x86-64 arm of upstream's module `cfg_select!`, beside `apic` and `emu`, instead of under a top-level `#[cfg(guest_arch = "x86_64")]` |
| 007 | `af721003c` | 007 `01a59c75c` - virt_mshv: make RUN_VP cancellation race-free | microsoft/openvmm#5082 | The architecture-independent `run_vp` module is declared before upstream's `cfg_select!` of the aarch64 and x86-64 modules |
| 008 | `a2490e1ed` | 008 `e50265e1a` - virt: add Partition::finalize_memory; create the MSHV BSP after finalization | microsoft/openvmm#5082 | `virt_mshv` declares the architecture-independent `timing` module beside `run_vp`, before upstream's `cfg_select!`; `virt_backend_tests` keeps upstream's `cfg_select!` of `fixture`, `native`, and `time` and drops `#![forbid(unsafe_code)]`, as the commit does, since its fixture now maps RAM under `#![expect(unsafe_code)]` |
| 032 | `75f1282c7` | 032 `002f67201` - membacking: map guest RAM privately copy-on-write with fault accounting | microsoft/openvmm#5082 | Keeps upstream's unbraced Windows arm of the `cfg_select!` that maps a file view, and the commit's choice between `map_file_copy_on_write` and `map_file` in the other arm |
| 040 | `001b572b6` | 040 `ec13c36b2` - virtio_console: add attachment identities and reconnect and disconnect policies | microsoft/openvmm#5164 | Builds the console handle with the commit's `Discard` disconnect policy and no attachment, and keeps upstream's `cfg!(any(windows, target_os = "macos"))` test after it |
| 049 | `9aa49a98b` | 049 `631aea0ff` - vmm_core: make level-triggered legacy IRQs explicit | microsoft/openvmm#5082 | Keeps upstream's `cfg_select!` of each ACPI architecture configuration and adds the commit's empty `level_triggered_irqs` to each x86-64 configuration in it |
| 051 | `0ce005036` | 051 `16d19ecd3` - virt: describe the deterministic microVM topology | microsoft/openvmm#5082 | Keeps upstream's `cfg_select!` of the aarch64 and x86-64 processor topologies and passes the commit's machine profile to `build_x86_topology` in the x86-64 arm |
| 055 | `b0f9dced1` | 055 `974bf5f96` - chipset: add the microVM portb console | microsoft/openvmm#5082 | Registers `MicrovmPortbResolver` after `PitResolver` in upstream's x86-64-only `register_static_resolvers!` block instead of with a cfg of its own in the shared list |
| 056 | `afb93af63` | 056 `30099c8ab` - chipset: add the microVM shutdown and exit-status port | microsoft/openvmm#5082 | Registers `MicrovmShutdownResolver` after `MicrovmPortbResolver` in upstream's x86-64-only `register_static_resolvers!` block |
| 057 | `6dcc237a5` | 057 `9e3dcbf79` - openvmm_core: boot microVMs through MP-table direct load | microsoft/openvmm#5082 | Declares the `microvm` module before upstream's `cfg_select!` of the architecture imports; keeps upstream's `cfg_select!` of the IOMMU devices and of the ACPI architecture configuration, treats MP-table boot like device-tree boot in its aarch64 SMMU arm, and takes the x86-64 level-triggered IRQs from the machine profile |
| 059 | `92aba67ea` | 059 `936fb32ef` - openvmm_entry: expose `--machine microvm` | microsoft/openvmm#5082 | Keeps upstream's `cfg_select!` of the four serial device names, sets up the microVM portb console after it, and passes the commit's microVM default COM1 backend with upstream's `serial0_name` |
| 066 | `42b523b81` | 066 `8055aa4a3` - chipset: add the microVM snapshot-request port | microsoft/openvmm#5082 | Registers `MicrovmSnapshotRequestResolver` after `MicrovmShutdownResolver` in upstream's x86-64-only `register_static_resolvers!` block |
| 072 | `7a292f606` | 072 `144d19bf0` - virt: build a reproducible CPU and clock contract for microVM partitions | microsoft/openvmm#5082 | `virt_kvm` declares `cpu_contract` without the inner `#![cfg]` that upstream moved to the module's declaration; `virt_whp` configures the versioned contract in an x86-64 statement of its own, before the x86-64 block into which upstream folded the nested-virtualization property, which keeps the commit's order |
| 078 | `5e75bbdf7` | 078 `e96f8f754` - openvmm: attach a portable Consomme NIC to microVMs | microsoft/openvmm#5082 | Keeps upstream's `cfg_select!` of the TAP endpoint and adds the commit's refusal of a bare IPv4/prefix endpoint outside the microVM profile after it |
| 081 | `4354bcc9e` | 081 `41686521c` - virtiofs: add the fixed microVM filesystem profile | microsoft/openvmm#4876, microsoft/openvmm#5002 | Keeps upstream's `ParsedVirtioFs` parsing and `VirtioFsDeviceOptions` construction, and resolves the commit's microVM profile before upstream parses the handle; the parser, which only standard-profile handles reach, ignores the profile, and upstream's resolver tests build standard-profile handles |
| 085 | `45704afb4` | 085 `ad4d90921` - openvmm: reserve a dormant microVM filesystem slot | microsoft/openvmm#4876 | The commit's refusal of a `Dormant` backend for the standard device moves into upstream's `ParsedVirtioFs::parse`, as the first arm of its backend match |
| 086 | `ef8854204` | 086 `8bac7ff2d` - openvmm: add fixed-role microVM sandbox block slots | microsoft/openvmm#5040 | `VirtioBlkDisk` keeps upstream's request-queue options beside the commit's microVM sandbox role, and an ordinary virtio-blk disk takes its options and no role |
| 119 | `c1865e5da` | 119 `8b3480db6` - vmservice: create, capture, and restore microVMs over the management RPC | microsoft/openvmm#5002 | `VirtioFs` keeps upstream's `mount_options` and `windows_options`, fields 3 and 4, which replace `read_only`, and the commit's `guest_mount_target` and `read_write` follow them as fields 5 and 6; the standard machine's conversion refuses the microVM fields before it resolves upstream's Windows options; the server and the VMM tests keep upstream's handle-passing modules beside the commit's `microvm` modules; the interface tests take upstream's mount options and default the microVM fields; the gRPC reference keeps upstream's resource-passing link beside the commit's |
| 122 | `c5a107f2e` | 122 `81bb943b1` - ci: restrict privileged, self-hosted, and publishing jobs to microsoft/openvmm | microsoft/openvmm#5159 | Upstream moved the mirror refresh to Azure Pipelines and deleted `refresh-mirror.yml`, so the commit's repository gate on that workflow goes with it |
| 124 | `b0121fd5d` | 124 `91dc2f416` - vmservice: keep NVX wire-tag compatibility | microsoft/openvmm#5002 | `VirtioFs` gives the commit's legacy tags 3 and 4 to `guest_mount_target` and `read_write`, and moves upstream's `mount_options` and `windows_options`, which replace the `read_only` that the commit moved to field 5, to fields 5 and 6 |
| 133 | `8ae27d658` | 133 `93ace61db` - cpu_profile: fingerprint backend CPU surfaces and pin CPU profiles | microsoft/openvmm#5082 | `mod fingerprint` and `pub use fingerprint::cpu_fingerprint` join the x86-64 arm of upstream's module `cfg_select!` |
| 135 | `4c6f2cb89` | 135 `4c55ccdd5` - virt_whp: implement the NVX time ABI backend | microsoft/openvmm#5082 | The x86-64 arm of upstream's capabilities `cfg_select!` takes the commit's time ABI identity lookup and its hv1 and KVM-clock refusal; upstream's vendor `cfg_select!` stays |
| 142 | `7fc976c52` | 142 `9f5a2f22b` - openvmm: make the NVX time ABI the only microVM time path | microsoft/openvmm#5082 | The x86-64 arm of upstream's capabilities `cfg_select!` refuses hv1 alone, as the commit does; the file's change matches the commit's, 9 insertions and 63 deletions |
| 144 | `f85810622` | 144 `29b065e8f` - openvmm: pin an Alder Lake CPU profile and add opt-in host profiles | microsoft/openvmm#5082 | `pub use fingerprint::profile_unlisted_cpuid` joins the x86-64 arm of upstream's module `cfg_select!` |
| 150 | `e0df778af` | 150 `6b45b9251` - vmm_core: preserve halt state across snapshots | microsoft/openvmm#5082 | The halt path calls the commit's `record_halt`, which reports the halt to the debugger through upstream's `cfg_select!` on `feature = "gdb"` |
| 167 | `c4e774d9f` | 167 `1a1630600` - virtiofs: test microVM queues that start or stop with host input gated | microsoft/openvmm#4876 | Upstream already split the harness into `new` and `with_device`, now with `VirtioFsDeviceOptions`; the commit's `new_microvm` harness and the doc comment of `with_device` follow upstream's `new` |
| 198 | `e1710ad0b` | 198 `1837ec180` - vmservice: give the microVM fields upstream-compatible numbers | microsoft/openvmm#5002 | `VirtioFs` returns to the layout of position 119: upstream's `mount_options` and `windows_options` keep fields 3 and 4, and `guest_mount_target` and `read_write` follow at 5 and 6; the `read_only` that the commit restored to field 3 is gone upstream |
| 223 | `671b9592d` | 223 `62d7a8311` - vmservice: carry microVM blocks, network, RAM file, and share policy | microsoft/openvmm#5082, microsoft/openvmm#5002 | `on_big_stack`, which the commit moves to the module, takes upstream's `cfg_select!` on `windows`; `VirtioFs`'s new fork fields follow upstream's `windows_options` and the fork's `read_write`: `denied_paths` to `writable_paths` take 7 to 9, `owner` 10, and `aggregate_children` 11, one above the commit's numbers |
| 254 | `2f1d5e036` | 254 `cbbd386b7` - openvmm_entry: parse the microVM options on a big stack on every host | microsoft/openvmm#5082 | The Windows arm of `on_big_stack`'s `cfg_select!` calls the commit's `with_big_stack`, which position 339 builds only where a caller is |
| 260 | `a5ed8b8da` | 260 `2dd72ba30` - virt_kvm: refuse a restored EFER.SVME for a guest whose CPUID has no SVM | microsoft/openvmm#5082 | `mod registers`, under `#[cfg(target_os = "linux")]`, joins the native x86-64 arm of upstream's module `cfg_select!` |
| 263 | `957844e85` | 263 `360009b17` - virt_kvm: reject a restored APIC timer whose delivery mode is not Fixed | microsoft/openvmm#5082 | `mod apic`, under `#[cfg(target_os = "linux")]`, joins the native x86-64 arm of upstream's module `cfg_select!` |
| 266 | `9a9076f2f` | 266 `1a7fa3923` - vmm_tests: check that a new KVM VP can be saved | microsoft/openvmm#5082 | `mod save`, under `#[cfg(target_os = "linux")]`, joins the native x86-64 arm of upstream's module `cfg_select!` |
| 267 | `c1a26f57b` | 267 `9cf73f25b` - virt_kvm: refuse to restore an injected #BP or #OF | microsoft/openvmm#5082 | `mod activity`, under `#[cfg(target_os = "linux")]`, joins the native x86-64 arm of upstream's module `cfg_select!` |
| 268 | `8d5b5eab5` | 268 `2b84aa555` - virt_kvm: report an INIT or a SIPI that a stopped VP has not accepted | microsoft/openvmm#5082 | `mod init_sipi`, under `#[cfg(target_os = "linux")]`, joins the native x86-64 arm of upstream's module `cfg_select!` |
| 270 | `3dfc3133f` | 270 `08200bb48` - consomme: relay ICMP echo replies on Linux hosts | microsoft/openvmm#5082 | The socket type follows the commit's `match Delivery::HOST`, which replaces upstream's `cfg_select!` on `windows` |
| 273 | `95c6d55bc` | 273 `8ee750138` - vmm_tests: check that a restore keeps a guest's SPEC_CTRL controls | microsoft/openvmm#5082 | `mod speculation` joins the native x86-64 arm of upstream's module `cfg_select!` |
| 277 | `f38539a5b` | 277 `eaf039eed` - virt: save a guest's user-wait, debug, and feature MSRs | microsoft/openvmm#5082 | The x86-64 arm of upstream's capabilities `cfg_select!` clears `umwait_control`, as the commit does; `mod msr_state` joins the native x86-64 arm of upstream's module `cfg_select!` |
| 283 | `7fdcddeb6` | 283 `62d8ba613` - virt_kvm: refuse to restore a short or malformed XSAVE area | microsoft/openvmm#5082 | `mod xsave`, under `#[cfg(target_os = "linux")]`, joins the native x86-64 arm of upstream's module `cfg_select!` |
| 284 | `6af2c5139` | 284 `ddb174c79` - virt_kvm: drop interrupts in the reserved remote-read delivery mode | microsoft/openvmm#5082 | `mod msi` joins the native x86-64 arm of upstream's module `cfg_select!` |
| 288 | `d6d6b7720` | 288 `445469dfe` - virt: refuse a pending NMI count that a backend cannot hold | microsoft/openvmm#5082 | `mod nmi` joins the native x86-64 arm of upstream's module `cfg_select!` |
| 290 | `772bba5e5` | 290 `39c220726` - virt: let a restore's check accept what a hypervisor documents it keeps | microsoft/openvmm#5082 | `mod restore`, under `#[cfg(target_os = "linux")]`, joins the native x86-64 arm of upstream's module `cfg_select!` |
| 293 | `9d0904f41` | 293 `76f382c05` - virt: refuse an interrupt shadow kind that a backend cannot hold | microsoft/openvmm#5082 | `mod shadow` joins the native x86-64 arm of upstream's module `cfg_select!` |
| 296 | `af16fdfc9` | 296 `6065e6616` - vmm_tests: check that each backend refuses unsupported restore state | microsoft/openvmm#5082 | `mod unsupported_state` joins the native x86-64 arm of upstream's module `cfg_select!` |
| 297 | `58f4eb0c4` | 297 `7f2d3f771` - virt_whp: keep a restored halted VP halted | microsoft/openvmm#5082 | `mod suspend`, under `#[cfg(windows)]`, joins the native x86-64 arm of upstream's module `cfg_select!` |
| 301 | `42432d77e` | 301 `ec3dc1b2a` - virt_kvm: map RAM into KVM's SMM address space | microsoft/openvmm#5082 | `mod smm`, under `#[cfg(target_os = "linux")]`, joins the native x86-64 arm of upstream's module `cfg_select!` |
| 303 | `9023d0e3e` | 303 `064f61c76` - virt_whp, virt_mshv: save and restore a guest's IA32_UMWAIT_CONTROL | microsoft/openvmm#5082 | The x86-64 arm of upstream's capabilities `cfg_select!` no longer clears `umwait_control`, as the commit does; the file's change matches the commit's |
| 305 | `b6e165d96` | 305 `b61856239` - virt, virt_kvm: save and restore AMD's VM_CR, VM_HSAVE_PA, and OSVW | microsoft/openvmm#5082 | The x86-64 arm of upstream's capabilities `cfg_select!` clears `osvw`, as the commit does; the file's change matches the commit's |

Upstream changes also broke the chain without a textual conflict. Four fixups at the tip adapt the
chain to them, and a fifth completes a resolution that OpenVMM CI found incomplete; fold each into
the commits that it names at the next reorganization, or before submission.

| Position | Fixup | Upstream PR | Fold into |
| ---: | --- | --- | --- |
| 335 | `8d989e670` - microVM: adapt virtio-fs to upstream's Windows impersonation | microsoft/openvmm#4876 | 081, 082, 085, 119, 120, 164, 198, and 223 |
| 336 | `5dbe25966` - virt_support_x86emu: convert exception codes with u16::from | microsoft/openvmm#5080 | 143 |
| 337 | `d22ea2c5a` - vmservice: refuse virtio-fs mount options on a microVM | microsoft/openvmm#5002 | 119, 198, and 223 |
| 338 | `942eeeda1` - openvmm_entry: keep the microVM sandbox blocks' fixed queue shape | microsoft/openvmm#5040 | 086, 087, 171, and 223 |
| 339 | `b51f9156c` - openvmm_entry: build with_big_stack only where it runs | microsoft/openvmm#5082 | 254 |

Position 335 builds the fixed no-DAX shape of the microVM virtio-fs devices of positions 081 and
085 through the `VirtioFsDeviceOptions` of upstream's `VirtioFsDevice::new`, which replaced
`with_num_request_queues`. The microVM resolver of position 081 and the management RPC's microVM
attachment of positions 119 and 223 refuse upstream's Windows options, as they refuse mount
options, and the `HostFs` values of positions 082 and 164 leave them unset; the tests of positions
119 and 120 pass upstream's `ResourceRegistry`, which replaced `FdRegistry`. Position 336 converts
the exception codes that upstream's translation code and its tests compare with `u16::from`:
position 143 makes `virt` depend on `cpu_profile`, which position 133 makes depend on
`serde_json`, whose `PartialEq<serde_json::Value>` implementation for `u16` leaves the target of
upstream's `.into()` ambiguous. Position 337 has the RPC's microVM attachment refuse non-empty
`mount_options` before it opens device resources, as the microVM's `HostFs` resolver does, since
the `read_only` that it refused when set is gone. Position 338 gives the sandbox block devices of
positions 086, 087, and 171 upstream's default virtio-blk options, one queue of depth 64, which is
the shape that they had. A sandbox block refuses upstream's per-disk `num_queues` as it refuses the
other per-disk options of position 086, and a microVM refuses the global `--virtio-blk-queues` and
`--virtio-blk-io-depth` options unless they keep their defaults, since its blocks have a fixed
device shape. Position 339 completes the resolution of position 254: upstream's `cfg_select!`
builds only the Windows arm of `on_big_stack`, where the commit's `cfg!(windows)` test built both,
so on other hosts only the management RPC's microVM options, under the `ttrpc` or `grpc` feature,
call `with_big_stack`. Without either feature, as `cargo hack` checks `openvmm_entry`, it was dead
code that `-D warnings` refuses, so it is built only on Windows or with one of those features.

microsoft/openvmm#4876 and microsoft/openvmm#5002 replace the management RPC's `VirtioFs.read_only`,
field 3, with `mount_options` and `windows_options`, fields 3 and 4, so each fork field of
`VirtioFs` is one above its v7.1 number: `guest_mount_target` and `read_write` are fields 5 and 6
from position 119 on, apart from positions 124-197, where they keep the legacy tags 3 and 4, and
`denied_paths`, `allowed_paths`, `writable_paths`, `owner`, and `aggregate_children` are fields 7
to 11 from position 223 on. The messages of positions 119, 198, and 223 still give the v7.1
numbers, and that of position 124 the `read_only` field that upstream replaced. NVX does not use
the management RPC.

The chain has no merges, and every commit has a good signature. Against v7.1, a range diff finds
the patches of 274 commits unchanged, those of the 47 resolved commits changed, and those of 13
more changed only in their context.

### Conflicts of the v7.1 rebase

The `feature/microvm/release/v7.1` rebase replayed the 315 commits of `feature/microvm/dev` at
`6e5303f94`, whose first 296 commits are those of `feature/microvm/release/v7.0`, onto `9a875b563`
with conflicts at two positions, which it resolved in place. Every commit kept its position, so the
table lists each resolution at the position that the commit had in v7.0 too.

| Position | v7.1 commit | v7.0 commit | Upstream PR | Resolution |
| ---: | --- | --- | --- | --- |
| 060 | `65258e052` | 060 `22a67d85b` - petri, vmm_tests: add a microVM builder, portb harness, and cold-boot test | microsoft/openvmm#4941 | `uses_pipette_as_init` keeps upstream's condition, non-OpenHCL Linux direct boot or an mshv base initrd, and applies the commit's `microvm::uses_pipette_as_init` gate, which admits only the standard machine profile, to both before it requires a Pipette binary; its comment keeps upstream's wording and names the standard machine profile |
| 277 | `eaf039eed` | 277 `f636c7f1d` - virt: save a guest's user-wait, debug, and feature MSRs | microsoft/openvmm#4942 | `virt_mshv`'s `ErrorInner` keeps upstream's `InvalidVpRegisterValue` and `InvalidVpRegisterMessage` variants and the commit's x86-64 `UnsupportedState` variant after them |

Upstream changes also broke the chain without a textual conflict. A fixup at the tip adapts the
chain to them; fold it into the commits that it names at the next reorganization, or before
submission.

| Position | Fixup | Upstream PR | Fold into |
| ---: | --- | --- | --- |
| 316 | `85c3698f9` - cpu_profile, openvmm_helpers: name sha2 as upstream's lock file does | microsoft/openvmm#4941 | 069 and 133 |

microsoft/openvmm#4941 removed `sha2` 0.10 from `Cargo.lock`, which leaves 0.11.0 its only version,
so upstream's lock file names `sha2` without a version. PR17's position 069 and position 133, which
add `sha2` to the dependencies of `openvmm_helpers` and `cpu_profile`, still name it
`sha2 0.11.0`. `cargo check --locked` accepts that, since it compares the resolves, but any cargo
command that may update the lock file rewrites those two lines and leaves the checkout dirty.
Position 316 names `sha2` there as upstream does.

### The v7.0 reorganization

A v6.1 position in this section and in [Conflicts of the v7.0 rebase](#conflicts-of-the-v70-rebase)
counts in `feature/microvm/dev` at `fe61f9cbf`, whose first 269 commits are those of
`feature/microvm/release/v6.1`, and a position without a release is a v7.0 position. The
reorganization folded 19 commits and moved two, so v6.1 positions 001-015 keep their numbers; v6.1
positions 016-233 are positions 018-235, 237-239 are 236-238, 244-253 are 239-248, 255-258 are
249-252, 260 is 253, 272 is 254, and 274-315 are 255-296; and v6.1 positions 261 and 270 are
positions 016 and 017. A position from 016 on in a section on an earlier release counts in that
release.

The reorganization replayed the 315 commits onto `e6b458cc0`, resolving the conflicts at three
positions in place, and folded the 19 fixups of the v6.1 line into the commits that they fix,
re-expressed for the code that those commits have: v6.1 positions 234-236, 240-243, 254, and 259,
which pull requests added to `feature/microvm/dev` after v6.0; the v6.1 rebase's fixups at 262-269;
and v6.1 positions 271 and 273, which pull requests added after v6.1. It moved v6.1 positions 261
and 270, which make a periodic timer skip its missed periods at once and start at a given deadline,
into PR10 ahead of the RTC commit, whose fold restarts the RTC's timers at their saved deadlines
with them; each can still go upstream on its own, in that order. Each anchor keeps its author and
date, and its message lists the subjects that it folds. The pin's tree, `52504d232`, is the tree of
the replay without the folds, `07e36f824`, apart from five `virt_kvm` x86-64 files that the
reorganization formats, below, and the `feature/microvm/**` branch that position 123's
regeneration adds to the pull-request trigger that microsoft/openvmm#5070 gave the Patina nightly
workflow. The table lists where each change went.

| v6.1 position | Change | v7.0 positions |
| ---: | --- | --- |
| 234 | `dcc3065ad` - state_unit: handle a unit in a terminal state instead of panicking | 011, 015, 213 |
| 235 | `b62b21163` - openvmm_core: tear the VM down when a restart cannot resume it | 011 |
| 236 | `e1433b346` - state_unit: roll back a partial start without panicking on a lost unit | 011, 067, 213 |
| 240 | `459c28110` - virtio: keep a held input gate across a device reset | 014, 038, 041, 046, 083, 090, 108, 167 |
| 241 | `066b40f14` - microVM: refuse a decoded network prefix outside /1 to /30 | 078, 197 |
| 242 | `1ec79d60f` - microVM: refuse an unaligned or reversed decoded snapshot RAM range | 093, 142 |
| 243 | `67000c096` - openvmm_helpers: check paired scratch when opening a snapshot | 088, 232 |
| 254 | `93c4f651e` - virt_kvm: build the LAPIC timer KVM tests for musl | 248 |
| 259 | `3cad2668f` - chipset: keep the PIT restore tests within Linux's open file limit | 252 |
| 261 | `f2389fd16` - vmcore: skip a periodic timer's missed periods at once | 016, moved |
| 262 | `55adb5c93` - serial_socket, openvmm_entry: resolve user SIDs at upstream's path | 105, 112 |
| 263 | `b9a8f7f6e` - openvmm_core: load microVMs through upstream's MP-table boot module | 057, 178 |
| 264 | `7b98adc6d` - openvmm_entry, openvmm_helpers: read upstream's snapshots through OpenedSnapshot | 033 |
| 265 | `5460c8e05` - openvmm_entry, openvmm_helpers: record no snapshot ID in microVM captures | 071, 121 |
| 266 | `649aeacf0` - ttrpc: give the guest exit test's service upstream's SaveVM state | 120 |
| 267 | `4a1d9b07a` - Guide, openvmm_entry: describe upstream's snapshots with the chain's publication | 030, 034, 071 |
| 268 | `63688dd58` - vmm_tests: save the ttrpc interface test's VM with a VMBus NIC | 046 |
| 269 | `73cf40357` - openvmm_helpers: drop an independent snapshot copy's page cache | 030 |
| 270 | `fad381b7a` - vmcore: start a periodic timer at a given deadline | 017, moved |
| 271 | `67ac3e283` - chipset: keep the RTC's time and timer phases across a snapshot | 018, 019, 052, 195, 243 |
| 273 | `09cd771c0` - chipset: bound the RTC's restored and advanced time | 018, 019 |

- v6.1 positions 234-236 fold into PR09's position 011, which rolls back a partial start. With them,
  a reset, save, restore, or time advance fails, naming each unit in a terminal state, before it
  sends any unit a request, and a stop also stops the units whose start is uncertain but leaves them
  in that state (234); a worker restart that cannot start the VM again after a failed save tears the
  VM down (235); and the rollback stops the started units in reverse dependency order and leaves a
  unit whose stop is blocked or unanswered in a terminal state instead of panicking (236). PR10's
  position 015, which adds the time advance, takes 234's refusal of a time advance, and the comments
  of PR17's position 067 and of position 213, which confirms device starts, describe the rollback as
  234 and 236 leave it.
- v6.1 position 240 folds into each input gate that it keeps across a device reset: PR09's position
  014 documents it, PR13's positions 038, 041, and 046 keep the gate in the virtio transports, the
  console, and virtio-net, PR18's position 083 keeps virtio-fs's request admission closed, PR20's
  position 090 documents it for the post-restore gate, PR23's position 108 keeps the control
  console's host attachment gated, and the virtio-fs gate tests of position 167 reset the device's
  ring for another start.
- v6.1 positions 241-243 fold into the commits that add the checks that they complete: PR18's
  position 078 refuses a decoded network prefix outside /1 to /30, PR20's position 093 an unaligned
  or reversed decoded snapshot RAM range, and PR19's position 088 a paired scratch that is not a
  regular file of its recorded length when it opens a snapshot. Position 197, which always reserves
  the virtio-fs slot, gives 241's test a virtio-fs device; position 142, which makes the time ABI
  the only microVM time path, gives 242's test configuration the time ABI's parameters; and position
  232 adds 243's FIFO case to its test of a FIFO artifact.
- v6.1 position 254 folds into position 248, whose KVM tests use Linux's `SIGEV_THREAD_ID`, which
  the `libc` crate does not define for musl, and v6.1 position 259 into position 252, whose PIT
  restore tests create five PITs in all rather than two or three for each case, so that they stay
  within Linux's default limit of open files.
- The v6.1 rebase's fixups at v6.1 positions 262-269 fold into the positions that their own table
  names, renumbered: see [Conflicts of the v6.1 rebase](#conflicts-of-the-v61-rebase).
- v6.1 positions 271 and 273 fold into PR10's position 018, which persists and advances the RTC
  clock, so that the RTC keeps its time and the phases of its alarm, periodic, and update interrupts
  across a snapshot, restarting its periodic timer at its saved deadline through position 017, and
  holds a restored or advanced time within the range of the host clock. The fold conflicted with
  three later commits, which the reorganization resolved: position 019 adds its check of the
  interrupt state to the saved state's validation, which the fold has take the current time and
  check the saved time's range; position 052's RTC mode seam rebases the held clock on reset and
  builds the clock without one; and position 195, which drops the retired RTC transaction state,
  numbers the timer deadlines' fields 5-8. Position 243, which runs restored VM time from the
  instant that the TSCs resume, takes 271's comment on the RTC's held clock and its sentence of the
  timekeeping guide.
- Upstream's `decode_manifest_without_snapshot_id`, from microsoft/openvmm#4553, checks that an
  unsupported manifest version fails, with version 6, which position 131 makes supported for the
  time ABI, so it failed from v6.1 position 129 to v6.1 position 187. Position 131 has it decode
  version 7, and position 190, which makes the format's single version 2 and refuses version 6
  again, restores version 6, as the head has it.
- rustfmt rejected the layout of six commits' tests in the `virt_kvm` x86-64 files, which the v6.1
  chain already had: positions 260, 276, 277, 278, 282, and 289 take rustfmt's layout. `cargo xtask
  fmt`, like `cargo fmt`, formats the modules that rustfmt finds from each crate root, and does not
  reach those files, which `vmm_core/virt_kvm/src/arch.rs` declares in a `cfg_select!` since
  microsoft/openvmm#4883.
- Later commits that build on a folded change adapt to it, in their arguments, imports, tests, or
  Guide text: positions 082, 085-087, 102, 110, 158-160, 163, 169, 186, 190, and 225.

The chain has no merges, and every commit has a good signature. Against v6.1, a range diff finds the
patches of 225 commits unchanged, the 19 fixups folded, the patches of 58 commits changed by the
changes above, and those of 13 more changed only in their context. Every commit of the v7.0 chain
builds: `cargo check --locked --workspace --all-targets` passes at each one on x86_64 Linux and,
without `vtpm_util`, whose TPM needs OpenSSL and which upstream's Windows jobs exclude too, on
x86_64 Windows, as do `cargo xtask fmt --pass lints`, rustfmt on the files that the commit changes,
and, at positions 002, 122, 123, and 181, which change the pipelines, `cargo xtask fmt --pass
verify-flowey`. At position 296, `cargo xtask fmt` passes, Clippy with `-D warnings` passes on
both hosts, leaving out on Windows `guest_test_uefi` and `opentmk_executor`, which upstream's
Clippy job checks for their UEFI targets alone, and `vtpm_util`, the unit tests of the 26 packages
that the conflicts, the folds, and the formatting change or build on pass on both hosts, each
hypervisor's package on the host that has that hypervisor, and the microVM VMM tests pass on KVM
and WHP.

#### Conflicts of the v7.0 rebase

The reorganization's replay of the 315 commits onto `e6b458cc0` had conflicts at three positions,
which it resolved in place. The table lists each resolution at its v7.0 position.

| Position | v7.0 commit | v6.1 commit | Upstream PR | Resolution |
| ---: | --- | --- | --- | --- |
| 046 | `a1726ac71` | 044 `309dd7989` - virtio_net: persist drained queue state for static-identity NICs | microsoft/openvmm#4862 | Petri's check for PCIe devices that cannot be saved takes upstream's `pcie_device_supports_save_restore`, which refuses NVMe, GDMA, and the USB xHCI controllers, in place of the commit's match of NVMe and GDMA, and adds the commit's ports whose virtio-net NICs have no save/restore contract |
| 059 | `d7ce3ef13` | 057 `cd065f0c0` - openvmm_entry: expose `--machine microvm` | microsoft/openvmm#4862 | The command line's `pub mod microvm;` precedes upstream's import of `crate::usb::UsbBackend` |
| 122 | `49e738e3b` | 120 `4527dbc00` - ci: restrict privileged, self-hosted, and publishing jobs to microsoft/openvmm | microsoft/openvmm#5070 | The per-job hook no longer sets upstream's condition that admits a Patina nightly job's pull-request run only with the `needs-patina` label on a non-draft pull request, since it would replace the fork gate; the fork-gate helper gives every Patina nightly job that condition, and its self-hosted jobs the repository gate too, and a new flowey test, `patina_nightly_jobs_keep_the_patina_label_gate`, checks the condition on every job, as `release_pr_jobs_keep_the_release_label_gate` checks the release pull-request pipeline's; `cargo xflowey regen` reproduces the merged workflows, and `cargo xtask fmt --pass verify-flowey` passes |

Position 123, v6.1 position 121 `d9c7e801a` - ci: run the checkin gates on the feature/microvm
branches, replayed without a conflict, but its generated workflows did not trigger the Patina
nightly workflow on `feature/microvm/**`, whose pull-request trigger microsoft/openvmm#5070 adds.
The reorganization regenerated them with `cargo xflowey regen` from the commit's flowey sources,
which add `feature/microvm/**` to the branches of every checkin-gate pipeline's triggers, and its
message names the Patina nightly workflow with its label gate.

No other upstream change broke the chain without a textual conflict.

### Conflicts of the v6.1 rebase

A v6.0 position in this section counts in `feature/microvm/dev` at `5ca20ee08`, whose first 220
commits are those of `feature/microvm/release/v6.0`, and a position without a release is a v6.1
position. The v6.1 rebase dropped seven commits, so a commit's position is its v6.0 position less
the number of dropped commits before it: v6.0 positions 003-007 are positions 002-006, 010-013 are
007-010, 015-050 are 011-046, and 054-268 are 047-261. A position from 003 on in a section on an
earlier release counts in that release.

The `feature/microvm/release/v6.1` rebase replayed the 261 remaining commits onto `7d90c4104`
with conflicts at 16 positions, which it resolved in place. The table lists each resolution at its
v6.1 position.

| Position | v6.1 commit | v6.0 commit | Upstream PR | Resolution |
| ---: | --- | --- | --- | --- |
| 006 | `50018e52e` | 007 `432f59f81` - pal: bound Linux descriptor-table pre-expansion | microsoft/openvmm#4591 | Keeps upstream's `credentials` module beside the commit's Linux-only `fd_table` module, in name order |
| 027 | `9a8d99b0b` | 031 `f1d03f428` - openvmm_helpers: version the snapshot manifest format and bound its decoding | microsoft/openvmm#4553 | The manifest keeps upstream's `snapshot_id`, field 18, after the commit's fields, and the commit's `Default` gives it the zero GUID; the CLI save and the save-to-disk VMM test set their snapshot IDs and take the rest from `Default`; `write_snapshot` takes upstream's refusal of an existing destination, its cleanup, and its manifest-last order, checks the saved-state size limit before it claims the destination, and records `state_size_bytes` in the manifest that it writes last; the round-trip test checks both the ID and the recorded length |
| 028 | `119345ead` | 032 `f6cdb703a` - openvmm_helpers: publish snapshots atomically through durable staging | microsoft/openvmm#4553 | The commit's staged publication replaces upstream's in-place write, cleanup, and hard link, and keeps upstream's public `check_snapshot_destination`, which SaveVM calls before it pauses the VM; upstream's tests stay, apart from `write_snapshot_creates_dir`, which the commit removes since the parent must exist: the existing-snapshot test backs the manifest's 1024 bytes of RAM, and the cleanup test fails while it copies `memory.bin`, after `state.bin` is staged, and checks that no staging directory remains; the guides describe the copy and the staged rename, with upstream's advice to keep the backing file outside the snapshot directory |
| 032 | `fff152a23` | 036 `49a1a98b9` - openvmm: restore RAM through a private CoW mapping and retain generation handles | microsoft/openvmm#4553 | The management RPC's launch keeps upstream's saved state and shared memory, which RestoreVM prepares from the snapshot, and passes the commit's defaults for other management launches, a shared mapping and no restore guards, so RestoreVM still runs guest RAM in place from `memory.bin` |
| 057 | `cd065f0c0` | 064 `87811096f` - openvmm_entry: expose `--machine microvm` | microsoft/openvmm#5018, microsoft/openvmm#4147, microsoft/openvmm#4554 | Upstream's `supports_acpi_power_button`, which decides from the load mode and gives the microVM's MP-table boot no power button, replaces the commit's check of the machine profile in the helper that microsoft/openvmm#5018 removed; the microVM's load mode precedes upstream's IGVM branch, without the restore branch that microsoft/openvmm#4147 removed; the Linux direct boot guide keeps upstream's description of `load_x86_mptable`, which `--machine microvm` now uses, before the commit's layout |
| 069 | `f8a051525` | 076 `652724750` - openvmm_entry: capture guest-requested microVM snapshots | microsoft/openvmm#4553 | The CLI's save keeps upstream's return of the snapshot ID and refuses a microVM first, as the commit has it; the snapshots guide takes the commit's description of the RAM file's promotion and publication, with upstream's advice to keep the backing file outside the snapshot directory and its statement that existing data is never overwritten |
| 086 | `7868666b8` | 093 `65f8b0d31` - openvmm: add clone and resume tiers with single-use resume claims | microsoft/openvmm#4553 | The manifest's tier, restore policy, and consumed sections, fields 15-17 until position 198 numbers them 13-15, precede upstream's `snapshot_id`, field 18, in the manifest and in its `Default` |
| 103 | `56b65cb4f` | 110 `1414c1a56` - serial_socket: report Unix and named-pipe peer identities | microsoft/openvmm#4609 | Upstream's named-pipe tests replace the reconnect test of v6.0 position 014, which the rebase dropped; the commit's call of `disconnect_current`, a no-op once the client has gone, moves into upstream's `reconnects_after_client_close`, after the failed write leaves the backend listening, and upstream's `reconnects_after_close_failure` stays as it is |
| 117 | `669e4aecd` | 124 `5fde97978` - vmservice: create, capture, and restore microVMs over the management RPC | microsoft/openvmm#4553 | CreateVM parses its microVM settings with the commit's `CreateVm` and creates the VM through upstream's `create_vm_inner`, which takes them beside upstream's restore parameters; RestoreVM restores only a standard VM and refuses a microVM, which restores from its authoritative manifest through CreateVM; a microVM refuses upstream's memory backing file; the worker takes upstream's restored state and shared memory, or else the microVM's, and the controller upstream's backing file, or else the microVM's RAM file; the service keeps upstream's SaveVM state beside the commit's pending readiness; the ttrpc test's upstream-built configuration fills the new virtio-fs fields with defaults |
| 118 | `9ef38e9ec` | 125 `502f72235` - ttrpc: propagate nonzero guest exit status | microsoft/openvmm#4553 | Upstream's `save_vm` stays ahead of the controller-event handler, which takes the commit's fallible signature |
| 120 | `4527dbc00` | 127 `75d359f4c` - ci: restrict privileged, self-hosted, and publishing jobs to microsoft/openvmm | microsoft/openvmm#5057 | The commit's fork gates take the runner, `.0`, of upstream's `(GhRunner, NextestProfile)` pools wherever microsoft/openvmm#5057 made the pool functions return one; `cargo xflowey regen` reproduces the merged workflows, and `cargo xtask fmt --pass verify-flowey` passes |
| 183 | `3a891414e` | 190 `282c72377` - microVM: route every IOAPIC input in the MP table | microsoft/openvmm#4554 | The commit's routing of IOAPIC inputs 16 and up moves onto upstream's `zerocopy` table builder: `MpTableConfig::ioapic_inputs` and `Error::TooFewIoApicInputs` join upstream's, the entry count adds the inputs past the ISA IRQs, and each of those inputs gets an `IoInterruptEntry` of its own number, active high and edge triggered through a new `spec::INTERRUPT_FLAG_EDGE_TRIGGERED`, aimed at upstream's I/O APIC, whose ID follows the processors' instead of being 0; upstream's tests pass 24 inputs, or 16 where they pin the ISA-only bytes, and the commit's tests expect upstream's I/O APIC ID |
| 221 | `68cb32387` | 228 `d6addf8c1` - vmservice: carry microVM blocks, network, RAM file, and share policy | microsoft/openvmm#4553 | The commit's `MemoryConfig.backing_file_path`, field 10, is upstream's, which is `optional` and which a standard VM takes for SaveVM, so the microVM conversion reads the optional value and a standard request no longer refuses it; CreateVM converts a microVM request as the commit does, and builds a standard VM through the commit's `create_standard_vm`, which takes upstream's restore parameters and returns the restored snapshot's ID beside the launch; RestoreVM builds a standard request from its configuration, refuses a microVM, and launches through the commit's `launch_vm`, which records the backing file that SaveVM needs for a standard VM only, beside upstream's saved-snapshot state |
| 224 | `03c6c0cef` | 231 `98e5b3ca5` - openvmm_helpers: reserve the manifest fields that upstream assigns | microsoft/openvmm#4553 | The manifest carries upstream's `snapshot_id` in field 18, which the commit only reserved, and the commit's reservation of fields 19 and 20 joins upstream's comment; the Guide's rule for extending the format still keeps 18-20 from new fields |
| 227 | `32b73e83b` | 234 `f41a1ecfc` - openvmm_helpers: don't publish a snapshot over an empty directory | microsoft/openvmm#4553 | The snapshots guide keeps upstream's statement that existing data is never overwritten beside the commit's refusal of a destination that appears during the capture |
| 232 | `9cb3c8206` | 239 `b5d20bf84` - petri: let a microVM test be its snapshot controller | microsoft/openvmm#4553 | Petri's worker launch takes upstream's saved state and then the commit's microVM snapshot channels |

Position 121, v6.0 position 128 `88c55fa8f` - ci: run the checkin gates on the feature/microvm
branches, replayed its generated workflows without a conflict, but the merge displaced the SIVM
IGVM build and publish steps that microsoft/openvmm#4940 adds, so the rebase regenerated the three
workflows from the commit's flowey sources with `cargo xflowey regen`, which leaves the commit's
patch as it was.

Upstream changes also broke the chain without a textual conflict. Eight fixups at the tip adapt
the chain to them; fold each into the commits that it names at the next reorganization, or before
submission.

| Position | Fixup | Upstream PR | Fold into |
| ---: | --- | --- | --- |
| 262 | `55adb5c93` - serial_socket, openvmm_entry: resolve user SIDs at upstream's path | microsoft/openvmm#4604 | 103 and 110 |
| 263 | `b9a8f7f6e` - openvmm_core: load microVMs through upstream's MP-table boot module | microsoft/openvmm#4554 | 055 and 176 |
| 264 | `7b98adc6d` - openvmm_entry, openvmm_helpers: read upstream's snapshots through OpenedSnapshot | microsoft/openvmm#4553 | 031 |
| 265 | `5460c8e05` - openvmm_entry, openvmm_helpers: record no snapshot ID in microVM captures | microsoft/openvmm#4553 | 069 and 119 |
| 266 | `649aeacf0` - ttrpc: give the guest exit test's service upstream's SaveVM state | microsoft/openvmm#4553 | 118 |
| 267 | `4a1d9b07a` - Guide, openvmm_entry: describe upstream's snapshots with the chain's publication | microsoft/openvmm#4553 | 028, 032, and 069 |
| 268 | `63688dd58` - vmm_tests: save the ttrpc interface test's VM with a VMBus NIC | microsoft/openvmm#4553 | 044 |
| 269 | `73cf40357` - openvmm_helpers: drop an independent snapshot copy's page cache | microsoft/openvmm#4553 | 028 |

Position 262 calls the token user SID queries of microsoft/openvmm#4604 in
`pal::windows::security`, where upstream merged them, from the peer identities and the control
console of PR23's positions 103 and 110. Position 263 loads the microVM's kernel and computes
its e820 RAM ranges through upstream's `loader::linux::mptable_boot` instead of the chain's
`loader::linux::microvm`, for PR15's position 055 and the device DMA ranges of position 176.
Position 264 has RestoreVM open its snapshot once with PR12's `OpenedSnapshot`, which checks that
`memory.bin` has the manifest's length, instead of upstream's path-based `read_snapshot`, which
position 031 replaced; it checks the expected snapshot ID against that manifest and closes the
snapshot's artifacts before the restore maps `memory.bin`. Position 265 gives the manifests that
the microVM capture of PR17's position 069 and the benchmark of PR26's position 119 build field by
field the zero snapshot ID, which the manifest defines as none, since only RestoreVM, which
refuses a microVM, checks an expected ID. Position 266 initializes upstream's SaveVM state in the
service of the guest exit test of PR25's position 118, and position 267 describes upstream's SaveVM
and the command line's save with the staged copy of PR11's position 028, which never hard-links
the memory backing file into the snapshot, and the command line's restore with the private
copy-on-write mapping of PR12's position 032, which leaves the snapshot unchanged. RestoreVM still
runs `memory.bin` in place, and so restores a snapshot once, as upstream's guide says. Position 268
gives the save and restore iteration of upstream's ttrpc interface test a VMBus NIC and moves the
test's virtio-frontend NIC to its third iteration, which does not save: PR13's position 044 lets
virtio-net save only a NIC bound to a static identity and feature contract, which the management
RPC does not configure, so SaveVM refused the test's VM on KVM and MSHV in OpenVMM CI. Position
269 has `copy_exact`, from PR11's position 028, drop the page cache of each independent copy that
it makes, of `memory.bin` or of a paired scratch image, once the copy is flushed. On MSHV, with the
snapshot on ext4, the VM that the ttrpc interface test restores through RestoreVM stopped as soon
as it resumed, on a memory intercept that an interrupt's delivery caused and that the emulator
refuses, unless the snapshot hard-linked the backing file, as upstream's does, or the copy's pages
had left the page cache; on tmpfs, it passed either way. NVX's templates publish `memory.bin` by
hard link wherever the filesystem allows one and pair no scratch, so their captures and clones
make no such copy.

The chain has no merges, and every commit has a good signature. Against v6.0, a range diff finds
the patches of 220 commits unchanged, those of the 16 resolved commits changed, and those of 25
more changed only in their context. At the 16 resolved commits, the eight fixups, and positions
002, 121, and 179, which change the pipelines, rustfmt on the files that the commit changes and
`cargo xtask fmt --pass lints` pass on x86_64 Linux and x86_64 Windows, and
`cargo xtask fmt --pass verify-flowey` passes at positions 002, 120, 121, and 179. There,
`cargo check --locked --workspace --all-targets` passes at positions 002, 006, 027, 028, and
266-269 on both hosts, and from position 032 to position 265 it fails only with the breaks that the
fixups at positions 262-266 repair; on Windows, it and Clippy leave out `guest_test_uefi` and
`opentmk_executor`, which upstream's Clippy job checks for their UEFI targets alone, and
`vtpm_util`, whose TPM needs OpenSSL, as upstream's Windows jobs do. At position 267,
`cargo xtask fmt` passes, Clippy with `-D warnings` passes on both hosts, the unit tests of the 20
packages that the conflicts and fixups change or build on pass on Linux and, apart from a
`sparse_mmap` test that fails on upstream main on that host too, on Windows, and the microVM VMM
tests pass on WHP; position 268 changes only a VMM test, whose package's Clippy passes there. At
position 269, Clippy and the unit tests of `openvmm_helpers` pass on both hosts, and the ttrpc
interface test, which failed on MSHV at position 268 with its snapshot on ext4, passes there twice.

### The v6.0 reorganization

A v5.1 position in this section counts in `feature/microvm/dev` at `3f2ab4c2f`, the head of
`feature/microvm/release/v5.1`, and a position without a release is a v6.0 position. The v6.0
reorganization renumbered the positions after 147: v5.1 position N is position N-1 for N from 149
to 220, so a position from 149 on in a section on an earlier release counts in that release.

The reorganization replayed the 223 commits of `feature/microvm/dev` at `3f2ab4c2f` on their own
fork base, `ba105dd7f`, which upstream main has not passed: upstream had landed none of their
changes, and none conflicted with upstream. It folded the two fixups that the roadmap assigned to
mapped PRs into the commits that they fix, re-expressed for the code that those commits have, and
combined the two commits of microsoft/openvmm#4984, which landed together and the second of which
revises the first. The pin's tree, `7575cbccb`, is the tree of `3f2ab4c2f`. Position 219, which is
v5.1 position 220 renumbered, has the tree of v5.1 position 221, `0e46a01ed`, rather than that of
v5.1 position 220, since the fold moved v5.1 position 221's change ahead of it, into positions 035
and 036. The table lists where each change went.

| v5.1 position | Change | v6.0 positions |
| ---: | --- | --- |
| 148 | `8c0a0251f` - microVM: let platform snapshots opt into restore-time vCPU activation | 097, 099, 116, 147 |
| 221 | `0e46a01ed` - openvmm_helpers: leave the change time out of snapshot file generations | 035, 036, 076, 092, 093 |
| 222 | `5799877f0` - virt: read on while the guest TSC's offset is wide | 220 |
| 223 | `3f2ab4c2f` - virt: read the guest TSC offset until it is within its bound | 220 |

- v5.1 position 221 folds into PR12's position 035, which adds the file generation that it changes:
  the Linux generation of an opened snapshot file keeps the device, inode, length, and modification
  time, but not the change time, which removing the other link of an automatic `memory.bin` moves.
  Its test publishes the snapshot through `write_snapshot_from_owned_memory_file`, the owned-RAM
  publication that PR11's position 033 adds, and 035's snapshot format guide no longer says that
  Linux records the change time. Position 036 takes 221's sentence of that guide in place of the
  one on observable metadata changes that it adds, which PR19's position 093 rewraps. PR17's
  position 076, which names the RAM file of a microVM capture `.openvmm-microvm-memory-`, gives the
  test's RAM file that name, and PR19's position 092, which replaces
  `write_snapshot_from_owned_memory_file` with `write_snapshot_from_owned_memory_and_scratch_files`,
  has the test publish through the new function, as 221's test does.
- v5.1 position 148 folds into PR20's position 097. 097's platform tier check still checks the TSC
  and LAPIC tokens of the platform command line, so its `maxcpus=` check follows the processor
  capacity's among them, the allowlist accepts `maxcpus=` between `nr_cpus=` and
  `lapic_timer_hz=`, the test helper that builds the worker's platform command line takes the TSC
  frequency and builds it with `build_microvm_command_line`, and the tests validate with
  `validate_manifest_version`. At 097, the snapshots guide's sentence on platform snapshots ends
  with each clone's own prefix, and PR20's position 099, which adds `--restore-memory`, adds its
  comparison with expanding RAM. PR23's position 116 keeps `maxcpus=` ahead of the control
  console's token and builds the helper's command line with `build_microvm_control_command_line`,
  and position 147, which removes the clock tokens, takes the check, the allowlist, the helper, the
  tests, and the CLI and snapshots guides as v5.1 position 148 left them.
- v5.1 positions 222 and 223 combine into position 220, with 223's subject: 223 replaced 222's
  limit of 64 reads with a search that reads until a read is within the bound and fails the start
  after 10 seconds of the host TSC. Position 220 keeps 222's author and date, and its message gives
  the measurements of both.

Replaying the chain with the folds conflicted at four positions, all in files that the folds
change, which the reorganization resolved as the list says: position 036 in the snapshot format
guide, position 093 in that guide and in the test imports of `restore.rs`, position 116 in the
platform allowlist and the test helper, and position 147 in the tier check, the test helper, and
the CLI and snapshots guides. Only the patches of positions 035, 036, 076, 092, 093, 097, 099, 116,
147, and 220 changed; positions 115 and 184 differ only in the context of their hunks.

Every commit of the v6.0 chain builds: `cargo check --locked --workspace --all-targets` passes at
each one on x86_64 Linux and, without `vtpm_util`, whose TPM needs OpenSSL and which upstream's
Windows jobs exclude too, on x86_64 Windows, as do `cargo xtask fmt --pass lints`, rustfmt on the
files that the commit changes, and, at each commit that changes the pipelines, positions 002, 003,
127, 128, and 186, `cargo xtask fmt --pass verify-flowey`. `openvmm_helpers`' unit tests pass at
each position whose patch the folds changed, on Linux and Windows, and the folded tests fail at
positions 035 and 097 without the fixes that they test.

### Conflicts of the v5.1 rebase

The `feature/microvm/release/v5.1` rebase replayed the 219 commits of `feature/microvm/dev` at
`db42fde04`, whose first 212 commits are those of `feature/microvm/release/v5.0`, onto `ba105dd7f`
with a conflict at one position, which it resolved in place. Every commit kept its position, so the
table lists the resolution at the position that the commit had in v5.0 too. It is position 182 in
v6.0, position 175 in v6.1, and position 177 in v7.0.

| Position | v5.1 commit | v5.0 commit | Upstream PR | Resolution |
| ---: | --- | --- | --- | --- |
| 183 | `7bc9f7fd3` | 183 `0efa13a45` - guestmem, virtio: admit only a set of ranges to a device's memory | microsoft/openvmm#4944 | `GuestMemoryInner` keeps upstream's separate kernel and user lock capabilities next to the commit's restriction, and a restricted view copies both from the memory that it restricts; the view's backing forwards upstream's `lock_kernel_gpns` and `lock_user_gpns` instead of `lock_gpns`, and the restriction test locks through `lock_user_gpns`, as its `lock_gpns` calls without kernel access did, from the commit's place in `mod tests` rather than at the end of upstream's new `lock_mode_tests`, where the merge appended it |

### The v5.0 reorganization

A v4.0 position in this section and in
[Conflicts of the v5.0 rebase](#conflicts-of-the-v50-rebase) counts in `feature/microvm/dev` at
`e1ca353eb`, whose first 210 commits are those of `feature/microvm/release/v4.0`;
[Exact commit-to-PR mapping](#exact-commit-to-pr-mapping) and
[Commits after the mapped tip](#commits-after-the-mapped-tip) name the v4.0 position of each
commit. A position without a release counts in v5.0, as in v5.1; see
[The v6.0 reorganization](#the-v60-reorganization) for the positions from 149 on.

Besides the rebase, the v5.0 reorganization retired PR09's bounded quiesce transactions and folded
the snapshot quiesce without a deadline into the commits that it changes, moved PR13's write
handling ahead of the console policy that needs it, and folded every fixup into the commit that it
fixes. The pin's tree is the tree of the replay with its fixups, apart from the pipeline workflows,
which the reorganization regenerated where the replay merged them wrongly, and the fixes of
positions 079 and 112, below. Every commit of the v5.0 chain builds:
`cargo check --locked --workspace --all-targets` passes at each one on x86_64 Linux and, without
upstream's `vtpm_util`, whose TPM needs OpenSSL and which upstream's Windows jobs exclude too, on
x86_64 Windows, as do `cargo xtask fmt --pass lints`, rustfmt on the files that the commit changes,
and, at each commit that changes the pipelines, `cargo xtask fmt --pass verify-flowey`.

microsoft/nvx#1260 withdrew v4.0 position 021, PR09's bounded quiesce transactions, since the
deadline of the snapshot quiesce added no safety, and v4.0 position 216, which
microsoft/openvmm#4921 landed on `feature/microvm/dev`, removed them with the capture's other
bounds. The reorganization retired v4.0 position 021 and folded v4.0 position 216 into the commits
that it changes, so that no commit adds a bound that a later one removes:

- PR09's position 018 adds the input gate without a deadline, as microsoft/openvmm#4626 proposes
  it, in a private module.
- PR17's position 071 adds the snapshot-request port, and position 076 its handles in OpenVMM and
  petri, without the input gate's timeout. The worker of position 072 quiesces a capture by
  stopping the state units and rolls back a capture that fails by starting them again, with the
  start rollback of PR09's position 015, so the controller of position 076 has no uncertain
  quiesce to handle. Position 076 adds no `--snapshot-quiesce-timeout-ms` option, and tests that
  the capture options take no timeout; the dispatcher test of position 073 builds the RPCs and the
  port without durations.
- The post-restore input gate of PR20's position 095 quiesces input without a timeout, and the
  console flush of PR24's position 119 has none either.
- The management RPC of PR25's position 124 never adds the timeout field. Position 204, which
  gives the microVM fields upstream-compatible numbers, reserves the field's number, 3, and the
  names that v4.0 gave it.
- The boot console flush test of position 207 calls the flush without a duration, and position
  211, which logs the units that a state change waits for, also adds the test of a slow input
  quiesce that reads that log, and the CLI guide's sentence about it.

The fold changed the patches of 30 commits: positions 018, 019, 071-073, 076, 079, 082-084, 087,
089, 091-095, 099, 106, 119, 124, 125, 139, 147, 163, 167, 177, 204, 207, and 211. The others
that it changed adapt to those changes, in their imports, arguments, tests, or Guide text, or only
in their context.

Position 041, v4.0 position 015, moved from PR06's place into PR13, ahead of the `Retain` policy
of position 042, and v4.0 position 218, which explains and tests why a failed write drops the
connection, folded into it, so that its message gives the output-retention reason. The test runs
on Linux only, since on Windows the first write after an AF_UNIX peer closes is accepted and the
next fails with `ConnectionReset`. v4.0 position 219, which tests that a `Retain` console keeps
that output for the next peer, folded into position 043, re-expressed for the direct-mode test
harness there, to which PR23's position 113 later adds a `Default` implementation and the write
interception that the test extends; see [Open upstream pull requests](#open-upstream-pull-requests).

Checking each commit found four more problems, which the reorganization fixed in the commits that
introduced them:

- Upstream's `TestVp` of microsoft/openvmm#4871 does not implement the `advance_tsc` that PR10's
  position 029 adds to `ControlVp`, so `vmm_core`'s tests did not build from position 029 to
  position 146. Position 029 gives `TestVp` the method, which position 147 removes with the rest of
  `advance_tsc`.
- rustfmt rejected the layout of the one-element list that the fold left in position 082's microVM
  option test, and an import order in position 097's MSHV TSC tests, which the v4.0 chain already
  had.
- Clippy of Rust 1.97, which upstream main now requires, rejects a needless borrow of a format
  argument in a test of position 112's broker.
- Position 079 made `--restore-snapshot` conflict with `--kernel` and `--initrd` for every machine,
  so upstream's new test that a restore builds the ACPI power button of its boot command line,
  from microsoft/openvmm#4924, failed. Clap's conflicts are upstream's again, and a microVM restore
  refuses, in the microVM's own validation, a kernel or initrd that differs from its default, with
  a test. Positions 177 and 179, whose tests and validation comments had used `--kernel` as an
  option that conflicts with `--restore-snapshot`, use `--snapshot-destination` instead.

#### Conflicts of the v5.0 rebase

The `feature/microvm/release/v5.0` rebase replayed the 216 remaining commits onto `61994293a`
with conflicts at 31 positions, which it resolved in place. The table lists each resolution at its
v5.0 position.

| Position | v5.0 commit | v4.0 commit | Upstream PR | Resolution |
| ---: | --- | --- | --- | --- |
| 003 | `824eead65` | 003 `301082446` - flowey: keep caller temp directories for native local VMM-test runs | microsoft/openvmm#4817 | Takes upstream's manifest, which already makes `tempfile` a regular dependency, with the commit's `test_with_tracing` development dependency |
| 013 | `96ffa21cb` | 014 `d58f66764` - virtiofs: bound guest payload allocations and rate-limit diagnostics | microsoft/openvmm#4836, microsoft/openvmm#4885 | `read_dir_with`, to which upstream's `read_dir` now delegates, checks the reply size before it locks the directory and reserves fallibly; `forget` keeps upstream's atomic update with the commit's rate-limited warning |
| 015 | `bb55a304c` | 018 `a1a72fa96` - state_unit: roll back a partial start instead of panicking | microsoft/openvmm#4538 | The PCI resource assignment's start keeps upstream's comment that it freezes the SMMU capabilities, with the commit's start |
| 018 | `0efc9061b` | 022 `cc2108a97` - vmcore: add device input quiesce and resume | microsoft/openvmm#4863 | `quiesce_input` and `resume_input` precede upstream's rewritten stop documentation, and the device worker handles their requests next to upstream's running flag and teardown request |
| 019 | `c2b9feb0e` | 024 `a6bd7a70f` - vmcore, state_unit: advance guest time across stopped downtime | microsoft/openvmm#4863 | `ChangeDeviceState` and the erased chipset device take the commit's `advance_time` and `advances_time` after upstream's teardown, and a dynamic device's unit advances time before upstream takes its driver for the removal handle |
| 040 | `77d460b89` | 045 `b11f00541` - virtio: forward input quiesce and resume through transports | microsoft/openvmm#4945 | The device task's commands and their handling take the commit's input quiesce and resume next to upstream's teardown |
| 047 | `d82897c56` | 051 `a3d5e9d22` - consomme: add an exact static IPv4 and gateway identity | microsoft/openvmm#4669 | `ConsommeParams` and `ConsommeHandle` carry upstream's parameters and DNS switch with the commit's identity and overrides; the resolver applies the identity and the overrides after upstream's CIDR and parameters, so that they cannot make the identity inexact and the host-local access override wins over upstream's parameter; the RPC's handles come from upstream's `build_consomme_handle`, which sets none of the commit's fields |
| 063 | `1bf092dff` | 067 `4bb8bd81c` - openvmm_core: reject host save, pulse save/restore, and worker restart for microVMs | microsoft/openvmm#4863 | The event loop rejects a microVM's unsupported RPCs before upstream defers RPCs that wait for a pending device removal, so that a rejection never waits for one |
| 064 | `346d4cc40` | 068 `474b41571` - openvmm_entry: expose `--machine microvm` | microsoft/openvmm#4856 | The portb and shutdown devices join upstream's chipset build, which now follows the load mode, and the microVM rejects VMGS after it; `acpi_power_button_enabled` is false for `--machine microvm`, whose machine contract admits no chipset device beyond its own |
| 072 | `6fc32aff9` | 076 `43ca4535e` - openvmm_core: establish the post-OUT snapshot boundary in the worker | microsoft/openvmm#4863 | The worker's event loop races the commit's snapshot boundary with upstream's device removal |
| 073 | `8550ae2b1` | 077 `401676126` - openvmm_core: fence management mutations during snapshot boundaries | microsoft/openvmm#4883 | `openvmm_core`'s lock entry takes the commit's `chipset` dependency without the `cfg-if` dependency that upstream removed |
| 078 | `7ce646403` | 082 `8bf1c4069` - openvmm_core: validate restored CPU and clock contracts and apply host downtime | microsoft/openvmm#4863 | Declares the commit's `clock` module next to upstream's `device_removal` module |
| 081 | `df9ce2bf0` | 085 `0a3b8d226` - openvmm_entry: add connect= serial backends with bounded timeouts | microsoft/openvmm#4889 | The serial parser's rejection test keeps upstream's `listen=tcp:` cases and adds the commit's bare `connect` |
| 088 | `82776b0cf` | 092 `81e72c373` - virtiofs: persist microVM FUSE namespaces, handles, and directory cookies | microsoft/openvmm#4836 | A microVM's directory read takes its persisted-cookie path in upstream's `read_dir` wrapper, before upstream's `read_dir_with` |
| 095 | `f57d6a19e` | 099 `cc78e4691` - openvmm: gate external input until post-restore repair completes | microsoft/openvmm#4863 | The worker's event loop races the commit's restore gate expiry with upstream's device removal and the snapshot boundary |
| 110 | `af611dcb5` | 114 `f2dbfa315` - serial_socket: report Unix and named-pipe peer identities | microsoft/openvmm#4605 | The commit's Linux-only peer-identity test joins upstream's unconditional test module, gated on Linux, as v4.0 position 217 had it |
| 117 | `f3130df35` | 121 `8254c78a0` - openvmm_entry: add authenticated microVM control-console endpoints | microsoft/openvmm#4856 | `end_of_stdin` follows the console input forwarding, and the REPL's tests stay in the module that upstream moved to the end of the file |
| 124 | `4f169ab2d` | 128 `064573568` - vmservice: create, capture, and restore microVMs over the management RPC | microsoft/openvmm#4856, microsoft/openvmm#4873, microsoft/openvmm#4669 | `VMConfig` keeps upstream's `guest_watchdog` field 22 with the commit's `machine_profile`; the standard profile's base chipset follows upstream's choice by `disable_hv`; the chipset takes the commit's portb split of the serial ports, then upstream's ACPI power button, which an MP-table boot does not support, and guest watchdog; the per-NIC Consomme RPC map stays upstream's |
| 127 | `d2cb7dae8` | 131 `9e71e43ce` - ci: restrict privileged, self-hosted, and publishing jobs to microsoft/openvmm | microsoft/openvmm#4902 | The shared VMM-test artifact and OpenHCL build jobs gate upstream's AMD v6 1ES pool on the repository, and `cargo xflowey regen` regenerates the four workflows |
| 129 | `e641bf33d` | 133 `965a34e60` - vmservice: keep NVX wire-tag compatibility | microsoft/openvmm#4873 | `VMConfig` keeps upstream's `guest_watchdog` field 22 next to the commit's `vmgs` field 24, so no number repeats |
| 138 | `ccff33cac` | 142 `ffd43376f` - cpu_profile: fingerprint backend CPU surfaces and pin CPU profiles | microsoft/openvmm#4883, microsoft/openvmm#4817 | `virt_kvm`'s lock entry takes the commit's `cpu_profile` dependency without the `cfg-if` dependency that upstream removed, and the new `cpu_profile` crate depends on `sha2` 0.11.0 by its version, since upstream's lock holds `sha2` 0.10.9 too |
| 139 | `b4f3afaae` | 143 `63335024f` - openvmm: preflight, capture, and restore the time ABI clocks | microsoft/openvmm#4863, microsoft/openvmm#4817 | The worker's event loop races the commit's restore events with upstream's device removal, and `openvmm_core` depends on `sha2` 0.11.0 by its version |
| 143 | `b362d940a` | 147 `9bdad46fd` - virt_mshv, virt_whp: deliver LAPIC vectors written to a halted VP's IRR | microsoft/openvmm#4893 | The commit's pending-vector tests join upstream's page-aligned buffer tests in one test module |
| 146 | `f4f5d9724` | 150 `058421168` - virt: complete the time ABI's topology leaves and VP 0's CPUID view | microsoft/openvmm#4817 | `openvmm_core`'s lock entry drops its `sha2` 0.11.0 dependency, as the commit drops `sha2` |
| 147 | `470bfc2ba` | 151 `4273d4891` - openvmm: make the NVX time ABI the only microVM time path | microsoft/openvmm#4863 | Removes the commit's retired `clock` module and keeps upstream's `device_removal` module |
| 157 | `94566f5dd` | 161 `6391930dd` - openvmm: preserve snapshot VM clock cut across save/restore | microsoft/openvmm#4863 | Adds the commit's `VMTIME_UNIT_NAME` after upstream's constants, without the VPCI eject helpers that upstream moved into its `device_removal` module |
| 161 | `acd94d986` | 165 `e7c38705d` - virt: publish host UTC and its clock correction in time ABI 2 | microsoft/openvmm#4538 | The PCI resource assignment starts the units through the commit's `start_units`, which measures the TSC offset first, under upstream's comment that the start freezes the SMMU capabilities |
| 169 | `935149d32` | 173 `b1a5a0e82` - microVM: select egress rules by protocol and port range, and add IPv6 | microsoft/openvmm#4669, microsoft/openvmm#4600 | `ConsommeParams` adds the commit's `gateway_ipv6` before upstream's UDP and ICMP idle timeouts; upstream's `build_consomme_handle` sets no static IPv6 identity, and the commit's addressing test handle sets upstream's default parameters and internal DNS |
| 175 | `e23b4676a` | 179 `7d5b21c09` - microVM: publish guest listeners to private networks and host loopback | microsoft/openvmm#4600, microsoft/openvmm#4669 | The UDP flows keep upstream's flow limit check next to the commit's listener peer lookup, and upstream's `build_consomme_handle` publishes no guest listener |
| 187 | `5cb534d56` | 191 `560a9ffa1` - ci: run the Windows Server 2025 KVP test on Linux KVM | microsoft/openvmm#4938 | On upstream's regrouped test artifacts, the Linux KVM job downloads the emulation host's artifacts and the Windows Server 2025 image, and the Linux MSHV job keeps upstream's filter and excludes the Server 2025 KVP test |
| 204 | `9f67da154` | 208 `bec51b3ac` - vmservice: give the microVM fields upstream-compatible numbers | microsoft/openvmm#4873 | `machine_profile` and `cpu_profile` follow upstream's `guest_watchdog` field 22, so they take 23 and 24 |

Other upstream changes broke commits without a textual conflict. The reorganization folded each
fix into the commit that it fixes:

| Position | v5.0 commit | v4.0 commit | Upstream PR | Fix |
| ---: | --- | --- | --- | --- |
| 006 | `078fdf174` | 007 `596f9c28e` - sparse_mmap: flush file-backed ranges with bounded concurrency | microsoft/openvmm#4866 | Upstream already adds `test_with_tracing` as a development dependency, so the commit no longer adds it a second time, as the merge had |
| 020 | `8928f7136` | 025 `10d06284f` - chipset: persist and advance the RTC clock | microsoft/openvmm#4864 | Advancing the clock updates the interrupt line through upstream's renamed `sync_interrupt_line_level` |
| 048 | `6b9194eee` | 052 `3f7c036d2` - virtio_net: persist drained queue state for static-identity NICs | microsoft/openvmm#4669 | The management RPC's virtio NIC frontend, which upstream added, saves no queue state and has no static identity or effective features |
| 062 | `dc5ee4ad5` | 066 `4afdb2716` - openvmm_core: boot microVMs through MP-table direct load | microsoft/openvmm#4856 | `supports_acpi_power_button`, which upstream matches on every direct-boot mode, gives MP-table boot no ACPI power button |
| 073 | `8550ae2b1` | 077 `401676126` - openvmm_core: fence management mutations during snapshot boundaries | microsoft/openvmm#4666 | A snapshot boundary also refuses upstream's `AddHvsockListener` and `RemoveHvsockListener` |
| 074 | `27f608991` | 078 `6ce9a1691` - openvmm_helpers: define the authoritative microVM machine contract | microsoft/openvmm#4817 | `openvmm_helpers` depends on `sha2` 0.11.0 by its version, since upstream's lock now holds `sha2` 0.10.9 too |
| 079 | `ea3d738bb` | 083 `b09ea1b9d` - openvmm_entry: restore microVMs authoritatively | microsoft/openvmm#4924 | Upstream's test that a restore builds the ACPI power button of its boot command line passes the commit's new `MicrovmRestore` argument, as its default |
| 083 | `fd56833b1` | 087 `0fd958133` - openvmm: attach a portable Consomme NIC to microVMs | microsoft/openvmm#4669 | The microVM's Consomme handle sets upstream's default parameters and internal DNS, which keep Consomme's behavior |
| 105 | `ab288b6a0` | 109 `c75757e59` - virtio_net: install and prefilter the egress policy | microsoft/openvmm#4669 | The management RPC's virtio NIC frontend installs no egress policy |
| 117 | `f3130df35` | 121 `8254c78a0` - openvmm_entry: add authenticated microVM control-console endpoints | microsoft/openvmm#4856 | The headless REPL test's controller has no ACPI power button |
| 128 | `db0da392b` | 132 `caf65a036` - ci: run the checkin gates on the feature/microvm branches | microsoft/openvmm#4852, microsoft/openvmm#4902 | The textual merge of the regenerated workflows restored the old base's Rust 1.95.0 and runner pools, so `cargo xflowey regen` regenerates them |
| 139 | `b4f3afaae` | 143 `63335024f` - openvmm: preflight, capture, and restore the time ABI clocks | microsoft/openvmm#4871 | Upstream's `TestVp` gets the LAPIC timer check and advance that the commit adds to `ControlVp`, as unreachable stubs |
| 156 | `7b46154ae` | 160 `3f1221d1e` - vmm_core: preserve halt state across snapshots | microsoft/openvmm#4871 | The halt latch test joins upstream's test module in `vp_set.rs` |
| 162 | `abab4db6d` | 166 `3e1ea0376` - vmservice: select the microVM CPU profile over the management RPC | microsoft/openvmm#4873 | `cpu_profile` takes field 25, since upstream's `guest_watchdog` has field 22, until position 204 numbers it 24 |
| 175 | `e23b4676a` | 179 `7d5b21c09` - microVM: publish guest listeners to private networks and host loopback | microsoft/openvmm#4600 | Upstream's UDP flow-limit tests build their flows with the commit's `translated_peers` |
| 183 | `0efa13a45` | 187 `135d5f7f6` - guestmem, virtio: admit only a set of ranges to a device's memory | microsoft/openvmm#4948 | The restriction test names `AccessType` through the import that upstream's test module now has |
| 212 | `f1e1d8545` | 220 `e1ca353eb` - net_consomme: give a port forward that cannot bind a stable code | microsoft/openvmm#4669 | The resolver test's Consomme handle sets upstream's default parameters and internal DNS |

### The v4.0 reorganization

This section and [Conflicts of the v4.0 rebase](#conflicts-of-the-v40-rebase) number positions,
and name commits, as `feature/microvm/release/v4.0` did;
[Exact commit-to-PR mapping](#exact-commit-to-pr-mapping) and
[Commits after the mapped tip](#commits-after-the-mapped-tip) name the current position of each
v4.0 position.

The v4.0 reorganization made three changes besides the rebase: it moved microsoft/nvx#1232's
awaited device restoration into the chain in place of the fallible-start design, it added 14
commits that give each ABI that the chain introduces a single version, and it folded three smaller
fixes into the commits that they fix. Every commit of the v4.0 chain builds:
`cargo check --locked --workspace --all-targets` passes at each one, without warnings, on x86_64
Linux and x86_64 Windows.

microsoft/nvx#1232 restores a device's private state while the VM is stopped, so that a device that
cannot restore fails the restore itself, before any VP runs, rather than a later start. Upstream's
awaited chipset restore, microsoft/openvmm#4799, which the fork base includes, carries it through
the chipset and the device workers. The chain therefore no longer needs a fallible start to report
restore errors:

- v3.0 position 026, now position 018, keeps `StateUnits::start`'s rollback of a partial start
  and its refusal to start a unit in a terminal state, but a unit's own `start` cannot fail again,
  as upstream's cannot. A unit task that does not answer leaves its unit in the terminal state
  instead of panicking.
- v3.0 position 027, which made chipset device starts fallible through the device workers, is
  retired: device workers start their devices without an acknowledgement, as upstream's do.
- v3.0 position 053, now position 044, restores device-private state into the stopped device
  task and waits for it, on the virtio-mmio transport, whose save and restore become asynchronous,
  as on PCI. It no longer stages the state for activation at start, at `DRIVER_OK`, at a device
  configuration access, or at a queue kick, and queue kicks signal the queue event directly again,
  as upstream's do. A queue that fails to start at resume stops the queues that did and marks the
  device `DEVICE_NEEDS_RESET`, notifying a driver that has set `DRIVER_OK`. Since the serialized
  kicks are gone, the virtio-blk over virtio-mmio boot test keeps upstream's unstable marking.
- Position 165, time ABI 2, measures the guest TSC offset in the VM's start path, before the state
  units start, instead of in the `time-abi` unit's start, so that a failed measurement fails the
  start; the partition unit no longer depends on the `time-abi` unit. Position 139 likewise starts
  the `time-abi` unit infallibly.
- v3.0 position 028, now position 019, keeps the VM resume fallible: the post-restore input gate,
  the restore readiness event, the TSC offset measurement, and a unit task that does not answer
  fail it. The hardware reseal unit of microsoft/openvmm#4415 keeps its infallible start.

After the v4.0 reorganization, a queue or host backend that failed to start after a successful
restore still let the resume succeed: the device marked itself `DEVICE_NEEDS_RESET`, which a
driver that has set `DRIVER_OK` sees, and the host logged the failure. Until v5.0 position 213,
virtio-net's host backend did not even do that: its coordinator created the endpoint queues after
the start and only logged a failure, leaving workers that wait for a queue forever. v5.0 position
213 creates them in the queue start, and v5.0 position 215 documented in the Guide that neither
the resume nor the restore readiness event waited for the queues to start. microsoft/nvx#1232
requires that a partially activated VM never report success. Preparing the queues at restore, as
microsoft/openvmm#4713 proposes, would not meet it alone, since microsoft/openvmm#4713 still creates
their host backends at activation. v5.0 position 219 instead confirms each device's start before
the VM runs: `StateUnits::start` awaits a confirmation of each unit's start, which reports success
by default, before it starts the unit's dependents, and the virtio transports confirm with their
device task's answer. Since the partition depends on the chipset devices, a device that fails to
start fails the resume before any vCPU runs, stops again with the units that started, and keeps the
progress of its queues, which v5.0 position 218 preserves, and a restore publishes no readiness
event. The starts themselves stay notifications, as upstream's are, so v3.0 position 027's fallible
device start stays retired.

The fold changed the patches of 27 commits, positions 018, 021, 022, 024, 044, 045, 047, 051,
052, 072, 093, 094, 110, 111, 118, 128, 139, 143, 151, 165, 168-170, 174, 179, 185, and 188,
which define, implement, or call state-unit and device starts, save or restore virtio-mmio
devices, or measure the TSC offset. It left the pin's tree as microsoft/nvx#1232's implementation
on the rebased chain has it, apart from the boot test's unstable marking and the three smaller
fixes. Position 051 drops an unnecessary qualification of `UdpSocket` in its static IPv4 test,
which upstream's test module now imports, instead of position 179 dropping it. Positions 052 and
094 no longer add `chipset_device` as a development dependency of `virtio_net` and `virtiofs`,
which only the virtio-mmio register tests that the fold rewrote used. And position 165's comment
says that the VM, not the `time-abi` unit, measures the offset that time samples use.

Positions 197-210 then give each ABI that the chain introduces a single version, without decoders
of earlier versions: microVM ABI 1 and boot layout 1; time ABI 1, with time sample 1 and restore
packet 1, whose single layout carries the network settings that packet 6 had and, as before, every
restore's fresh entropy, which made the `--restore-entropy` switch a no-op that position 199
removes; snapshot manifest version 2, since upstream's manifest is version 1,
with the magic `OPENVMM_SNAPSHOT_V2`; one saved-state schema each for virtio-console, virtio-fs,
and the egress policy digest; an IPv6 identity for every NIC; a required VM-time cut; a
filesystem slot that every microVM reserves; the management RPC's microVM fields after upstream's
field numbers; and snapshot fields numbered without gaps. The chain's earlier commits still
introduce and revise those ABIs in steps, so these 14 commits, rather than the commits that
introduced each version, retire the earlier versions.

#### Conflicts of the v4.0 rebase

The `feature/microvm/release/v4.0` rebase replayed the 197 remaining commits onto `c6d3da0ff`
with conflicts at 16 positions, which it resolved in place. The table lists each resolution at its
v4.0 position.

| Position | v4.0 commit | v3.0 commit | Upstream PR | Resolution |
| ---: | --- | --- | --- | --- |
| 004 | `6476a457f` | 004 `7b7d7c3b7` - consomme: make TCP window-reopen validation deterministic | microsoft/openvmm#4847 | Keeps the commit's deterministic window-reopen test in place of upstream's bounded-send-buffer flood test |
| 014 | `d58f66764` | 018 `3f6941671` - virtiofs: bound guest payload allocations and rate-limit diagnostics | microsoft/openvmm#4594, microsoft/openvmm#3440 | `read_dir` checks the reply size first, then takes upstream's lock and sandbox revalidation, then reserves fallibly; `lib.rs` takes upstream's reservation types with the commit's `guest_buffer` |
| 017 | `ea1153ef4` | 021 `6425cf88a` - consomme: bound active TCP, UDP, and ICMP flows | microsoft/openvmm#4620 | Takes upstream's `process_tx` call, which upstream moved into `egress.rs`, and adds the commit's TCP, UDP, and ICMP connection limits to its dropped-packet accounting |
| 018 | `a1a72fa96` | 026 `fa02eed4b` - state_unit: make unit start fallible and roll back partial starts | microsoft/openvmm#4845 | A start first clears upstream's `restore_allowed`, as upstream's start does, then rolls back a partial start; upstream's edge-pruning and failing-unit tests unwrap the start |
| 024 | `a6bd7a70f` | 033 `3dcf87849` - vmcore, state_unit: advance guest time across stopped downtime | microsoft/openvmm#4845 | Keeps upstream's failing-unit tests and adds the commit's time unit before `test_state_change` |
| 046 | `c72a9d0d6` | 055 `569c31998` - virtio_console: add attachment identities and reconnect and disconnect policies | microsoft/openvmm#4835 | The receive loop keeps upstream's `StopCheck` parameter before the commit's direct-mode destructuring |
| 047 | `10b776837` | 056 `a29e7173e` - virtio_console: persist buffered direct-console I/O | microsoft/openvmm#4835 | The direct receive loop checks for a stop before it delivers staged input, and the test's mock keeps upstream's write gate with the commit's controls |
| 050 | `25140a413` | 059 `0f7048709` - net_consomme: quiesce endpoint queues | microsoft/openvmm#4847 | `ConsommeQueue` keeps upstream's enabled receive offloads with the commit's input quiescing |
| 052 | `3f7c036d2` | 061 `0afebc9dd` - virtio_net: persist drained queue state for static-identity NICs | microsoft/openvmm#4847 | Petri's `with_virtio_nic` delegates to upstream's `with_virtio_nic_rx_gso`, which builds the commit's NIC with RX GSO off |
| 054 | `c10e1b7c2` | 063 `be2ec6c40` - virtiofs: track inode aliases through link, rename, and unlink | microsoft/openvmm#4594 | Upstream's dedup-key eviction of type-replaced keys makes the commit's `evict_dedup_key` unused, so it is gone; `inode.rs` keeps upstream's tests |
| 090 | `d2274f94f` | 099 `d0b79eaad` - virtiofs: add the fixed microVM filesystem profile | microsoft/openvmm#4594 | The microVM preflights run first, then upstream's ID reservations, whose inode and file reservations gain the microVM's handle limit |
| 092 | `81e72c373` | 101 `f11af4bb1` - virtiofs: persist microVM FUSE namespaces, handles, and directory cookies | microsoft/openvmm#4594 | `create` reserves IDs as upstream does, and a test-only `insert_file` reserves and inserts for the commit's tests |
| 107 | `eecd4ad25` | 116 `e0228e079` - net_consomme: enforce the egress policy on final transmitted bytes | microsoft/openvmm#4620, microsoft/openvmm#4854 | The egress check builds on upstream's moved transmit path without partial checksums, and the commit's egress tests move into `tests/egress.rs` next to upstream's test module |
| 117 | `e45d33646` | 126 `a699e9b52` - virtio_console: attach the broker to serial I/O with receive credits | microsoft/openvmm#4835 | The receive loop keeps upstream's `StopCheck` with the commit's broker dispatch; a reset test waits on a bounded timer for the host to detach, since upstream checks for a stop before work |
| 135 | `3cdf4fe38` | 144 `82686c983` - virtiofs: allow symlink creation on read-write microVM shares | microsoft/openvmm#4594 | Symlink creation validates its target and path, then reserves the inode as upstream does |
| 179 | `7d5b21c09` | 188 `ccf3a2849` - microVM: publish guest listeners to private networks and host loopback | microsoft/openvmm#4846, microsoft/openvmm#4854 | TCP listeners keep upstream's optional socket with the commit's admission check, and the commit's UDP publication tests build packets without partial checksums |

Two fixups, which the rebase folded, adapted commits that no textual conflict exposed: position
017's TCP connection-limit tests take the `test_with_tracing` test attribute that upstream's TCP
test module now uses, and upstream's graceful-shutdown tests of microsoft/openvmm#4688 build the
management RPC server with the fields that position 128 adds.

### The v3.0 reorganization

This section and
[Conflicts of the v2.4 rebase](#conflicts-of-the-v24-rebase) number positions, and name
commits, as `feature/microvm/release/v3.0` did; [Exact commit-to-PR mapping](#exact-commit-to-pr-mapping)
and [Commits after the mapped tip](#commits-after-the-mapped-tip) name the current position of
each v3.0 position.

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

As of 2026-11-23, upstream main is the fork base, `bf353460f`, so the v8.0 reorganization met
no upstream change.

Of the 27 commits that upstream main had past the v7.1 base, `9a875b563`, the v7.2 rebase resolved
`715927303` (microsoft/openvmm#5082), `8538ead65` (microsoft/openvmm#4876), `e5c06b0aa`
(microsoft/openvmm#5002), `e1bf98ef7` (microsoft/openvmm#5159), `78af54a51`
(microsoft/openvmm#5040), and `e692d56c8` (microsoft/openvmm#5164) where they conflict with the
chain, and dropped the chain's backport of `0474df224` (microsoft/openvmm#5160); fixups at its tip
adapted the chain to microsoft/openvmm#4876, `8ee0e98d8` (microsoft/openvmm#5080),
microsoft/openvmm#5002, and microsoft/openvmm#5040, and completed its resolution of
microsoft/openvmm#5082 at position 254, and the v8.0 reorganization folded them into the commits
that they fix; see [Conflicts of the v7.2 rebase](#conflicts-of-the-v72-rebase) and [The v8.0
reorganization](#the-v80-reorganization). The other 19 merged with the chain without conflicts.

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
    LB["Landed upstream before the base<br/>40 fork commits, dropped by<br/>the v2.2, v2.3, v2.4, v6.1, and v7.2 rebases,<br/>v3.0, v4.0, and v5.0"]
    LPART["Partly superseded upstream<br/>microsoft/openvmm#4703, #4704, and #4764<br/>adopted by the commits that they overlap"]
    B["OpenVMM fork base<br/>bf353460f"]
    F["Recommended foundation fan-in<br/>before the microVM profile stack"]

    subgraph Independent["Parallel foundation PRs in upstream review"]
        direction LR
        P01["PR01<br/>Build/test infrastructure<br/>U - 3 commits, 1 in review"]
        P02["PR02<br/>Mesh + sparse_mmap<br/>U - 2 commits, 1 in review"]
        P03["PR03<br/>PAL on Unix<br/>U - 1 commit, 1 in review"]
        P05["PR05<br/>Hypervisor fixes<br/>U - 3 commits, 3 in review"]
        P06["PR06<br/>Device hardening<br/>U - 1 commit, 1 in review"]
        P14["PR14<br/>Direct-boot loader<br/>U - 1 commit"]
    end

    subgraph Generic["Generic snapshot and persistence chain"]
        direction LR
        P09["PR09<br/>Fallible VM lifecycle<br/>U - 4 commits, 3 in review"]
        P10["PR10<br/>CPU/clock state<br/>U - 13 commits"]
        P11["PR11<br/>Snapshot publication<br/>U - 4 commits"]
        P12["PR12<br/>Private-CoW restore<br/>U - 5 commits"]
        P13["PR13<br/>Virtio persistence<br/>U - 12 commits"]
    end

    subgraph Product["Ordered microVM product stack"]
        direction LR
        P15["PR15<br/>Machine profile<br/>P - 11 commits"]
        P16["PR16<br/>Fixed virtio-MMIO ABI<br/>P - 4 commits"]
        P17["PR17<br/>Capture + authoritative restore<br/>P - 11 commits"]
        P18["PR18<br/>Host attachments<br/>P - 10 commits + 2 fixups"]
        P19["PR19<br/>Block roles + tiers<br/>N/P - 3 commits"]
        P20["PR20<br/>Restore activation<br/>P - 7 commits + 2 fixups"]
        P21["PR21<br/>Network egress policy<br/>P/N - 6 commits"]
        P22["PR22<br/>Filesystem deny policy<br/>P - 2 commits"]
        P23["PR23<br/>Authenticated control console<br/>N - 10 commits"]
        P24["PR24<br/>Managed workload/reporting<br/>N - 5 commits"]
        P25["PR25<br/>Management plane<br/>P - 2 commits + 1 fixup"]
        P26["PR26<br/>Profiling benchmark<br/>U - 1 commit"]
        P27["PR27<br/>Safe writable-share symlinks<br/>U/P - 2 commits"]
        Z["Fork-only tail<br/>F - 3 commits"]
    end

    LB --> B
    LPART --> B
    B --> P01
    B --> P02
    B --> P03
    B --> P05
    B --> P06
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
    P05 --> F
    P06 --> F
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

    P13 -. "PR21 core can start early" .-> P21
    P13 -. "PR23 protocol/broker can start early" .-> P23

    classDef upstream fill:#dbeafe,stroke:#2563eb,color:#111827;
    classDef profile fill:#dcfce7,stroke:#16a34a,color:#111827;
    classDef product fill:#fef3c7,stroke:#d97706,color:#111827;
    classDef fork fill:#fee2e2,stroke:#dc2626,color:#111827;
    classDef gate fill:#f3f4f6,stroke:#4b5563,color:#111827;
    classDef landed fill:#ede9fe,stroke:#7c3aed,color:#111827;

    class B,F gate;
    class LB,LPART landed;
    class P01,P02,P03,P05,P06,P09,P10,P11,P12,P13,P14,P26 upstream;
    class P15,P16,P17,P18,P20,P22,P25,P27 profile;
    class P19,P21,P23,P24 product;
    class Z fork;
```

## Actual branch DAG

The physical branch is one first-parent chain:

```text
bf353460f
  -> PR01 [001-003] -> PR02 [004-005] -> PR03 [006]
  -> PR05 [007-009] -> PR06 [010]
  -> PR09 [011-014] -> PR10 [015-027] -> PR11 [028-031]
  -> PR12 [032-036] -> PR13 [037-048] -> PR14 [049]
  -> PR15 [050-060] -> PR16 [061-064] -> PR17 [065-075]
  -> PR18 [076-085] -> PR19 [086-088] -> PR20 [089-095]
  -> PR21 [096-101] -> PR22 [102-103] -> PR23 [104-113]
  -> PR24 [114-118] -> PR25 [119-120] -> PR26 [121]
  -> fork tail [122-124] -> PR27 [125-126]
  -> c6e62f286 (mapped tip)
  -> time ABI v1 and CPU profiles [127-143] -> CPU profiles [144-149]
  -> VM time [150-152] -> time ABI checks [153-154]
  -> time ABI 2 [155] -> management and test profiles [156-157]
  -> host filesystem [158-160] -> storage [161-162]
  -> network [163] -> host filesystem [164-168]
  -> network [169] -> storage [170-172]
  -> network [173-174] -> host filesystem [175]
  -> network [176] -> device DMA [177-178]
  -> command lines [179] -> KVP test [180-181]
  -> sandbox block roles [182] -> virtio feature masks [183-184]
  -> IOAPIC interrupt lines [185-186] -> ABI restart [187-200]
  -> boot console exit drain [201] -> portb without host input [202-203]
  -> long Windows paths [204] -> state change wait logging [205]
  -> port forward bind code [206] -> virtio queue starts after restore [207-209]
  -> storage generation block identity [210-211]
  -> device start confirmation [212-213] -> long Windows report paths [214]
  -> guest TSC offset within its bound [215]
  -> Granite Rapids CPU profile [216-218]
  -> second Turin CPU profile [219]
  -> microVM consoles without TCP [220]
  -> undelivered portb output kept [221]
  -> restore listener of a disconnected boot console [222]
  -> management RPC with the command line's microVM settings [223]
  -> staged manifest removed before a failed capture resumes [224]
  -> earlier snapshot formats refused before decoding [225]
  -> manifest fields reserved for upstream [226]
  -> unknown paired-scratch restore mode refused [227]
  -> clone's activation record carried in the restore packet [228]
  -> no-replace snapshot publication on every target [229]
  -> kernel tuning admitted on platform command lines [230]
  -> memory-expansion ranges placed without overflow [231]
  -> FIFO artifacts refused without waiting for a writer [232]
  -> device input gated without closing devices to the vCPUs [233]
  -> quiesced microVM run again only through its rollback [234-235]
  -> state changes failed rather than panicking on a lost unit [236-237]
  -> PCI restore tests before the first start [238]
  -> direct-claimed scratch attached in place [239]
  -> clock tokens split at Linux's whitespace [240]
  -> 365-day maximum snapshot downtime [241]
  -> restored VM time run from the TSCs' restart [242-243]
  -> LAPIC timer restores without drift or a downtime refire [244-246]
  -> LAPIC timer captures that keep a taken timer from refiring [247-248]
  -> restore phase logs through a host clock step back [249]
  -> PIT low/high counts without a control word [250]
  -> PIT time below 100 ns counted once [251]
  -> PIT restore rejecting a control word with RW = 0 [252]
  -> RTC port reads wider than a byte zero-extended [253]
  -> microVM RPC options parsed on a big stack [254]
  -> IOAPIC MMIO reads wider than 4 bytes zero-extended [255]
  -> restored IOAPIC id checked against the ID register's 4 bits [256]
  -> restored host CPU profile checked against the derivation policy [257]
  -> XSAVE lengths that overflow saturated [258]
  -> KVM's XSAVE buffer checked against a restored profile's layout [259]
  -> restored EFER's SVME checked against the guest's CPUID [260]
  -> SYSENTER MSRs held in the fields that name them [261]
  -> restored XSAVE component checked against the caps [262]
  -> restored APIC timer's delivery mode checked to be Fixed [263]
  -> save refused for a state that the elements cannot hold [264]
  -> KVM save refused for a vCPU whose GIF is clear [265]
  -> contract test that a new KVM VP can be saved [266]
  -> injected #BP or #OF refused on restore [267]
  -> INIT or SIPI that a stopped VP has not accepted reported [268]
  -> capture refused while a VP holds an INIT or a SIPI [269]
  -> ICMP echo replies relayed to the guest on Linux hosts [270]
  -> queued connections to a published port accepted without another wakeup [271]
  -> guest's IA32_SPEC_CTRL saved and restored [272]
  -> contract test that a restore keeps the guest's SPEC_CTRL controls [273]
  -> WHP hardware test of the SPEC_CTRL state under a host profile [274]
  -> SPEC_CTRL profile test built only for x86-64 guests [275]
  -> AMD's legacy PMU registers hidden from a time ABI guest [276]
  -> guest's user-wait, debug, and feature MSRs saved and restored [277]
  -> guest's machine-check state saved and restored [278]
  -> Guide's description of the VP state elements that hold a guest's MSRs [279]
  -> AMD's HWCR refused under an Intel CPU profile [280]
  -> VP thread's PKRU of 0 raised so that saves keep the guest's [281]
  -> XSAVE area's init components restored as init [282]
  -> short or malformed XSAVE area refused on restore [283]
  -> reserved remote-read MSI delivery mode dropped before KVM [284]
  -> KVM save refused for a vCPU in SMM or with an SMI pending [285]
  -> vCPU taken out of SMM when its activity is set [286]
  -> two pending NMIs saved and restored on KVM [287]
  -> backends that hold one pending NMI refuse a count of them [288]
  -> queued exception saved once and a pending one that KVM would drop refused on KVM [289]
  -> restore check that accepts what KVM documents it keeps [290]
  -> saved activity that holds the kind of its interrupt shadow [291]
  -> kind of an interrupt shadow saved and restored on KVM [292]
  -> backends that cannot hold the kind of an interrupt shadow refuse it [293]
  -> comments and Guide that say what an STI shadow restored as a MOV SS shadow loses [294]
  -> restore that refuses an element that the partition lacks before it sets any state [295]
  -> contract tests that each backend refuses an element that its partition lacks [296]
  -> restored halted or idle VP kept suspended on WHP, with contract tests [297]
  -> fixture accessor of those contract tests built on Windows only [298]
  -> restored VP's suspending instruction checked through the page tables in guest memory [299]
  -> restored VP with a pending NMI left to take it [300]
  -> RAM of a KVM VM mapped into its SMM address space [301]
  -> IA32_UMWAIT_CONTROL saved and restored on WHP and MSHV [302]
  -> IA32_BNDCFGS saved and restored where the guest's CPUID has MPX [303]
  -> AMD's VM_CR, VM_HSAVE_PA, and OSVW MSRs saved and restored on KVM [304]
  -> MSHV and WHP save refused for a VP that delivers a software interrupt [305]
  -> restored NIC's queue lifecycle kept through a failed start [306]
  -> restored MSI-X vectors programmed without delivering [307]
  -> restored virtio interrupt lines held until the device starts [308]
  -> restore and start failure tests of virtio-fs and remote devices [309-310]
  -> the queue progress that a failed start keeps documented [311]
  -> microVM snapshot rollback refused outside a snapshot boundary [312]
  -> snapshot manifest's field map with upstream recorded in the Guide [313]
  -> malformed snapshot ID refused by its field before the rest of the manifest decodes [314]
  -> offsets found in I/O regions that end at the top of their address space [315]
  -> chipset devices wired only once their state unit spawns [316]
  -> empty buffers skipped in virtio-fs descriptor chains [317]
  -> FUSE data sizes checked before their data is allocated [318]
  -> FUSE request lengths too short for their header refused [319]
  -> virtio-fs SETATTR times with out-of-range nanoseconds refused [320]
  -> FUSE ioctl replies that carry the file system's output [321]
  -> FUSE replies whose length counts their data [322]
  -> EIO replies for out-of-range FUSE error codes [323]
  -> FUSE replies longer than their header can describe refused [324]
  -> one reply checked for each FUSE session test request [325]
  -> EIO replies for FUSE data longer than its request allows [326]
  -> EIO replies for FUSE write and copy counts beyond u32 [327]
  -> FUSE_INIT failed for file systems that want flags that the guest did not offer [328]
  -> virtio-fs inodes keyed by device and inode number [329]
  -> virtio-fs guest inode numbers keyed by host device [330]
  -> virtio-fs lookup counts saturated rather than wrapped to zero [331]
  -> virtio-fs aggregate root reads bounded at 1 MiB [332]
  -> microVM file children whose names hold `\` or `:` refused [333]
  -> microVM creates of existing names held to the write rule of opens [334]
  -> microVM handles served only for their held nodes [335]
  -> unmapped host groups no longer taken for a caller's overflow GID [336]
  -> virtio-fs aggregate root reads too small for their next entry refused [337]
  -> virtio-fs links refused for nodes whose names are all gone [338]
  -> microVM file children attached through relative paths and symbolic links [339]
  -> virtio-fs aggregate root link counts that leave out file children [340]
  -> caller requests refused while host groups may be unmapped [341]
  -> virtio-fs aggregate root syncs acknowledged through the root's handle [342]
  -> virtio-fs renames refused when they would move a denied path [343]
  -> Windows virtio-fs names reach entries only by their stored names [344-349]
  -> virtio-fs handle and node IDs kept below u64::MAX [350]
  -> writes through virtio-fs handles not open for writing refused [351-352]
  -> virtio-fs nodes whose names are all gone reached only through their handles [353]
  -> microVM READDIRPLUS listings of btrfs placeholders and same-device mount points [354]
  -> microVM READDIRPLUS names of the listed node listed without a node [355]
  -> FUSE_INIT negotiations adopted only once their reply is sent [356]
  -> PCI slots of chipset devices claimed only once their state unit spawns [357]
  -> virtio-fs inodes documented to deduplicate only within a volume [358]
  -> offsets found in test and fuzzing chipset regions that end at the top [359-360]
  -> virtio-fs nodes of LINK replies kept until the guest forgets them [361]
  -> microVM saves refused while the guest holds a replaced virtio-fs object [362-364]
  -> FUSE_COPY_FILE_RANGE_64 requests failed with ENOSYS rather than EINVAL [365]
  -> FUSE_INIT negotiations that the session discards destroyed before the next one [366-368]
  -> reserved DFR models of the software APIC decoded by bit 31 [369]
  -> missing-device claims of full-width and empty ranges registered without overflow [370]
  -> microVM directory listings read to their end on Windows hosts [371]
  -> debugger breaks into a paused VM kept out of its saved state [372]
  -> KVM paravirtual kicks cleared when a VP's activity is set, or its halt refused [373]
  -> post-restore input gates armed by petri [374]
  -> PR20 fixup [375]
  -> PR20 fixup [376]
  -> PR25 fixup [377]
  -> host VPs per socket outside the topology builder's range refused [378]
  -> idle VPs refused by restores with WHP's user-mode APIC [379]
  -> KVM partitions refused more enlightened VPs than they have SynIC routes for [380]
  -> saves of KVM vCPUs in an SEV-ES AP reset hold refused rather than panicking [381]
  -> Intel L3 sharing counts saturated above 4096 VPs per socket rather than panicking [382]
  -> WHP user-mode APIC activity written without the internal activity state [383]
  -> failed io-uring set-ups reported with their error rather than as no support [384]
  -> microVM listing bounds that count only the entries that the policy shows [385]
  -> gdb stub sessions that detach the debugger from the VM however they end [386-388]
  -> PR18 fixup [389]
  -> microVM restores of handles opened with access mode 3 [390]
  -> Windows virtio-fs xattr names longer than an NTFS EA name holds refused [391]
  -> Windows symbolic links to an empty target refused with ENOENT [392]
  -> PR18 fixup [393]
  -> Windows hard links to names of one UTF-16 unit [394]
  -> 9822f732a (NVX pin)
```

For upstream review, PR01-PR03, PR05, PR06, and PR14 can be rebased independently onto current
upstream main, and most of their commits are already in upstream review; see
[Open upstream pull requests](#open-upstream-pull-requests). PR04, PR07, and PR08 have landed.
PR09-PR13 form the generic dependency chain shown above. The recommended product queue then fans
those foundations into PR15 and keeps PR15-PR26 ordered. PR21's generic enforcement commits can be
prepared after PR13, and PR23's protocol and broker commits too, since the peer credentials and
user SIDs that they take from PR03 and PR04 have landed upstream.
The fork tail, positions 122-124, remains outside the upstream queue. PR27 can be prepared once
PR22 lands; its generic confinement commit can be submitted independently if preferred. The v6.0
reorganization folded the PR20 and PR12 fixups into the commits that they fix, the v7.0
reorganization folded the fixups of PR09, PR10, PR13, and PR18-PR20, of two later commits, and of
the v6.1 rebase, and moved the two periodic timer commits that PR10's RTC commit builds on into
PR10, as positions 016 and 017, which can go upstream on their own in that order, and the v8.0
reorganization folded the fixups of PR13 and PR18 and of the v7.1 and v7.2 rebases; see
[The v6.0 reorganization](#the-v60-reorganization),
[The v7.0 reorganization](#the-v70-reorganization), and
[The v8.0 reorganization](#the-v80-reorganization). Fold position 375 into PR20's position 090
before submission, which arms the post-restore input gate's deadline and clears it only at the
release of the snapshot boundary that holds the guest's acknowledgement, with its changes of
`SnapshotRestore::restore_acknowledged` and of its call, and its unit tests, into position 134,
which adds that method and `SnapshotRestore::next_event`, and its VMM test into position 374, whose
petri option arms the gate. Fold position 376 into PR20's position 090, which adds the passages on
the restore readiness event of a gated restore that it corrects and the documentation of
`MicrovmRestore::gate_required`, with its changes of the help and the CLI reference's entry of
`--restore-processors` into position 092, which adds them, and its change of the snapshot guide's
sentence that calls that option no hotplug API into position 142, which rewraps the sentence that
position 092 adds. Fold position 377 into PR25's position 119, which adds the management RPC
reference's passage on the readiness event and the comment of `restore_processor_count` that it
corrects. Fold position 389 into PR18's position 081, whose `InodeReservation::insert` check of a
reserved node's insert required another free node ID, and its test into position 350, whose node
table never hands out `u64::MAX`, after that position's test of the largest next node ID. Fold
position 393 into PR18's position 083, whose `reopen_flags` refused a saved handle's `O_DIRECT`, and
its test into position 335, whose `get_file` takes a handle's node, before position 083's test of
`reopen_flags`.

## Commits after the mapped tip

The NVX pin extends the chain past position 126 without merges. Of its 268 later commits, positions
375 and 376 are PR20 fixups, position 377 is a PR25 fixup, positions 389 and 393 are PR18 fixups,
and the others are not yet classified or assigned to PRs.
Each of positions 127-181 combines the v2.4 commits that the table lists, as v3.0 positions 145-160
and 162-200 did, which developed one concept; "(part)" marks a v2.4 fixup that split across
commits. Positions 182-186 followed the v3.0 reorganization, positions 187-200 are the ABI restart
of the v4.0 reorganization, positions 201-206 followed it, positions 207-213 followed the v5.0
reorganization, positions 214 and 215 followed the v5.1 rebase, positions 216-253 followed the
v6.0 reorganization, positions 254-296 followed the v6.1 rebase, positions 297-314 followed the
v7.0 reorganization, positions 315-331 followed the v7.1 rebase, positions 332-355 followed the
v7.2 rebase, and positions 356-394 followed the v8.0 reorganization, so they combine none, apart
from position 215, which combines v5.1 positions 222 and 223, and the commits that the v7.0 and
v8.0 reorganizations folded into them. The table gives each commit's v7.2 position, which through
position 301 is its own, since the v8.0 reorganization folded v7.2 positions 302, 316, 333,
335-340, 342, 345, 347, 349, 351, and 365, and which positions 356-394 lack.
Each commit through v7.2 position 316 has the position that it had in v7.0 and v7.1, and each of
v7.2 positions 317-334 is its v7.1 position less one, since the v7.2 rebase dropped v7.1 position
317, upstream's microsoft/openvmm#5160, which v7.1 had backported. Each commit's v6.1 position is
its v6.0 position less seven, the number of commits that the v6.1 rebase dropped before it, and
its v6.0 position is its v5.1 position through 147 and one less from v5.1 position 149 on, and
positions 297-394 have no position before v7.0; see
[The v8.0 reorganization](#the-v80-reorganization),
[Conflicts of the v7.2 rebase](#conflicts-of-the-v72-rebase),
[The v7.0 reorganization](#the-v70-reorganization),
[Conflicts of the v6.1 rebase](#conflicts-of-the-v61-rebase), and
[The v6.0 reorganization](#the-v60-reorganization). A v7.2 position counts in
`feature/microvm/dev` at `09524b79c`, whose first 339 commits are those of
`feature/microvm/release/v7.2`, a v6.1 position counts in `feature/microvm/dev` at `fe61f9cbf`,
whose first 269 commits are those of `feature/microvm/release/v6.1`, a v6.0 position counts in
`feature/microvm/dev` at `5ca20ee08`, whose first 220 commits are those of
`feature/microvm/release/v6.0`, and a v4.0 position counts in `feature/microvm/dev` at
`e1ca353eb`, whose first 210 commits are those of `feature/microvm/release/v4.0`.

| Position | Commit | v7.2 position | v6.1 position | v6.0 position | v5.1 position | v4.0 position | v3.0 position | v2.4 positions | Topic |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |
| 127 | `9a0ef9279` | 127 | 125 | 132 | 132 | 136 | 145 | 149, 150, 382 (part), 317 (part) | virt: add the NVX time ABI v1 scaffolding |
| 128 | `18a39d7be` | 128 | 126 | 133 | 133 | 137 | 146 | 151, 152 | virt_kvm: implement the NVX time ABI backend |
| 129 | `d82912793` | 129 | 127 | 134 | 134 | 138 | 147 | 153, 325 (part), 373 (part) | virt_mshv: implement the NVX time ABI backend |
| 130 | `30ff2fe66` | 130 | 128 | 135 | 135 | 139 | 148 | 154, 157, 159, 160 | openvmm_core: add the time ABI platform behind --x-time-abi-v1 |
| 131 | `edf9f9821` | 131 | 129 | 136 | 136 | 140 | 149 | 155, 158 | openvmm_helpers: add snapshot manifest version 6 for the time ABI |
| 132 | `c43942abf` | 132 | 130 | 137 | 137 | 141 | 150 | 156 | chipset: serve restore packet v4 and time samples on portb |
| 133 | `6abc2a06c` | 133 | 131 | 138 | 138 | 142 | 151 | 161, 162, 163, 164, 166, 167, 179 | cpu_profile: fingerprint backend CPU surfaces and pin CPU profiles |
| 134 | `1ae2ee08b` | 134 | 132 | 139 | 139 | 143 | 152 | 165, 314 (part), 168, 169, 170, 171, 173, 174, 175, 176, 180, 319 | openvmm: preflight, capture, and restore the time ABI clocks |
| 135 | `b07cee477` | 135 | 133 | 140 | 140 | 144 | 153 | 172 | virt_whp: implement the NVX time ABI backend |
| 136 | `607d075b0` | 136 | 134 | 141 | 141 | 145 | 154 | 177 | openvmm_entry: add the --cpu-fingerprint tool |
| 137 | `925c093cd` | 137 | 135 | 142 | 142 | 146 | 155 | 178, 181, 182, 183, 184, 185 | openvmm: carry the selected CPU profile in the time ABI configuration |
| 138 | `8b49e42fb` | 138 | 136 | 143 | 143 | 147 | 156 | 186, 187, 325 (part), 196 | virt_mshv, virt_whp: deliver LAPIC vectors written to a halted VP's IRR |
| 139 | `1bff445ba` | 139 | 137 | 144 | 144 | 148 | 157 | 188, 189, 190, 191, 192, 193, 373 (part), 194, 195, 197, 198, 199 | openvmm: program the complete effective CPUID of time ABI CPU profiles |
| 140 | `6aa365975` | 140 | 138 | 145 | 145 | 149 | 158 | 200, 201, 202, 203, 204, 205, 206, 207, 208, 209, 210, 211, 212 | virt: make the CPUID entries that a CPU profile omits read zero |
| 141 | `6428e1edb` | 141 | 139 | 146 | 146 | 150 | 159 | 214, 215, 216, 217, 218, 219, 220, 221, 222, 223, 224, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 235 | virt: complete the time ABI's topology leaves and VP 0's CPUID view |
| 142 | `6001a0612` | 142 | 140 | 147 | 147 | 151 | 160 | 236, 237, 238, 373 (part), 239, 240, 241, 242, 243, 244, 324 (part) | openvmm: make the NVX time ABI the only microVM time path |
| 143 | `ff6d9c37d` | 143 | 141 | 148 | 149 | 153 | 162 | 245, 246, 247, 248, 249, 250, 318, 251, 252, 253, 254, 255, 256, 257, 258, 259 | virt: build the time ABI effective CPUID in one place |
| 144 | `caf50a3aa` | 144 | 142 | 149 | 150 | 154 | 163 | 262, 263, 264 | openvmm: pin an Alder Lake CPU profile and add opt-in host profiles |
| 145 | `e0af54d9a` | 145 | 143 | 150 | 151 | 155 | 164 | 267, 268, 269, 270, 271, 373 (part), 272 | cpu_profile: pin AMD Milan, Genoa, and Turin CPU profiles |
| 146 | `af2a05fd8` | 146 | 144 | 151 | 152 | 156 | 165 | 274 | virt_whp: let WHP answer CPUID leaves 0 and 1 without exits |
| 147 | `30a6da85d` | 147 | 145 | 152 | 153 | 157 | 166 | 278 | openvmm_core: fall back from --cpu-profile auto to a host profile |
| 148 | `1908b00d5` | 148 | 146 | 153 | 154 | 158 | 167 | 338, 339 | cpu_profile: pin amd.genoa.v2 for Genoa hosts without the TSA immunities |
| 149 | `298ed9db2` | 149 | 147 | 154 | 155 | 159 | 168 | 350, 351 | cpu_profile: verify pinned profiles, including their CPU time bits |
| 150 | `9f8d947ff` | 150 | 148 | 155 | 156 | 160 | 169 | 290 | vmm_core: preserve halt state across snapshots |
| 151 | `93a2d3ac5` | 151 | 149 | 156 | 157 | 161 | 170 | 291, 292 | openvmm: preserve snapshot VM clock cut across save/restore |
| 152 | `0032658dc` | 152 | 150 | 157 | 158 | 162 | 171 | 358, 359 | vmcore: map VM times beyond the host clock to its latest time |
| 153 | `e977d9743` | 153 | 151 | 158 | 159 | 163 | 172 | 299, 314 (part) | openvmm: make the time ABI documentation standalone |
| 154 | `f9b228108` | 154 | 152 | 159 | 160 | 164 | 173 | 352, 353, 354, 355, 356, 357 | openvmm_core: name every CPUID difference in E_CPU_SURFACE errors |
| 155 | `74c46c49c` | 155 | 153 | 160 | 161 | 165 | 174 | 363, 364, 365, 366, 367, 368 | virt: publish host UTC and its clock correction in time ABI 2 |
| 156 | `4672da0a7` | 156 | 154 | 161 | 162 | 166 | 175 | 322, 377 (part) | vmservice: select the microVM CPU profile over the management RPC |
| 157 | `1456b264d` | 157 | 155 | 162 | 163 | 167 | 176 | 323 | petri, vmm_tests: fall back to the host CPU profile where auto's is unsupported |
| 158 | `44471064a` | 158 | 156 | 163 | 164 | 168 | 177 | 260, 261, 320 (part) | virtiofs: perform microVM share requests as the guest caller |
| 159 | `ac7c76476` | 159 | 157 | 164 | 165 | 169 | 178 | 265, 266 | microVM: attach a second virtio-fs share with its own slot |
| 160 | `2ea6f1e0b` | 160 | 158 | 165 | 166 | 170 | 179 | 273, 320 (part) | virtiofs: add a per-subtree access policy to microVM shares |
| 161 | `de9142877` | 161 | 159 | 166 | 167 | 171 | 180 | 275, 276, 277, 317 (part), 320 (part), 329 (part) | openvmm_entry: bind microVM snapshots to their generation and scratch |
| 162 | `7bf61982f` | 162 | 160 | 167 | 168 | 172 | 181 | 279, 320 (part) | openvmm_entry: permit RAM-backed microVM overlays without scratch |
| 163 | `48ef718c2` | 163 | 161 | 168 | 169 | 173 | 182 | 280, 320 (part), 281, 282 | microVM: select egress rules by protocol and port range, and add IPv6 |
| 164 | `d109f3e89` | 164 | 162 | 169 | 170 | 174 | 183 | 283, 320 (part), 284, 285, 286 | microVM: share several host directories through one virtio-fs slot |
| 165 | `25737f2af` | 165 | 163 | 170 | 171 | 175 | 184 | 287, 320 (part), 288, 289 | microVM: export single host files as aggregate virtio-fs children |
| 166 | `740b2d42e` | 166 | 164 | 171 | 172 | 176 | 185 | 293 | virtiofs: test RENAME_EXCHANGE node names and snapshots |
| 167 | `3786b019a` | 167 | 165 | 172 | 173 | 177 | 186 | 334, 335 (part) | virtiofs: test microVM queues that start or stop with host input gated |
| 168 | `6632437c5` | 168 | 166 | 173 | 174 | 178 | 187 | 294, 320 (part), 295 | microVM: accept host paths with commas in --mount |
| 169 | `cac0bb445` | 169 | 167 | 174 | 175 | 179 | 188 | 296, 320 (part), 297, 379 (part), 298, 300 | microVM: publish guest listeners to private networks and host loopback |
| 170 | `cad1c4e40` | 170 | 168 | 175 | 176 | 180 | 189 | 331 | disk_zerofill: present a fixed geometry over a shorter or absent disk |
| 171 | `96011a757` | 171 | 169 | 176 | 177 | 181 | 190 | 332, 333 | microVM: let a platform restore present a shorter distro and no scratch |
| 172 | `b5848e302` | 172 | 170 | 177 | 178 | 182 | 191 | 340, 341, 342 | disk_blob: bound, retry, and inspect HTTP blob requests |
| 173 | `39f623402` | 173 | 171 | 178 | 179 | 183 | 192 | 336, 337 | microVM: let a platform restore install a new policy on a NIC template |
| 174 | `a2746a104` | 174 | 172 | 179 | 180 | 184 | 193 | 360 | microVM: reject explicit host-loopback access on a /30 IPv4 network |
| 175 | `8e299a5dd` | 175 | 173 | 180 | 181 | 185 | 194 | 343, 344 | microVM: let a platform restore attach an aggregate to a dormant slot |
| 176 | `25e3464bd` | 176 | 174 | 181 | 182 | 186 | 195 | 369, 370 | microVM: install a new network policy at an untiered restore |
| 177 | `4c908421c` | 177 | 175 | 182 | 183 | 187 | 196 | 345, 346 | guestmem, virtio: admit only a set of ranges to a device's memory |
| 178 | `ce6e973ef` | 178 | 176 | 183 | 184 | 188 | 197 | 347, 348, 349 | microVM: restrict virtio device DMA to the RAM ranges of the e820 map |
| 179 | `ac2ff6c5a` | 179 | 177 | 184 | 185 | 189 | 198 | 302, 303, 304, 305, 306, 307 | microVM: refuse command lines that the guest kernel would truncate |
| 180 | `6cab7513a` | 180 | 178 | 185 | 186 | 190 | 199 | 361 | vmm_tests: Fix kvp test flakiness by updating windows version |
| 181 | `63f08e03c` | 181 | 179 | 186 | 187 | 191 | 200 | 362 | ci: run the Windows Server 2025 KVP test on Linux KVM |
| 182 | `db041abbe` | 182 | 180 | 187 | 188 | 192 | 201 | - | microVM: remove the runtime and custom sandbox block roles (microsoft/openvmm#4839) |
| 183 | `2aa08dad9` | 183 | 181 | 188 | 189 | 193 | 202 | - | microVM: give every virtio device class an exact feature mask (microsoft/openvmm#4840) |
| 184 | `6553e8e9d` | 184 | 182 | 189 | 190 | 194 | 203 | - | microVM: drop access-platform and the console size in microVM ABI 3 (microsoft/openvmm#4840) |
| 185 | `872e9ab51` | 185 | 183 | 190 | 191 | 195 | 204 | - | microVM: route every IOAPIC input in the MP table (microsoft/openvmm#4842) |
| 186 | `177450acd` | 186 | 184 | 191 | 192 | 196 | 205 | - | microVM: give each virtio slot its own IOAPIC input in ABI 3 (microsoft/openvmm#4842) |
| 187 | `614272368` | 187 | 185 | 192 | 193 | 197 | - | - | microVM: restart the microVM ABI and boot layout at version 1 |
| 188 | `3ad15e728` | 188 | 186 | 193 | 194 | 198 | - | - | microVM: restart the time ABI, time sample, and restore packet at version 1 |
| 189 | `a7bce21b0` | 189 | 187 | 194 | 195 | 199 | - | - | openvmm_entry: remove the no-op --restore-entropy switch |
| 190 | `2ee3d9051` | 190 | 188 | 195 | 196 | 200 | - | - | openvmm_helpers: make the snapshot manifest version 2 |
| 191 | `8d8b4e4b2` | 191 | 189 | 196 | 197 | 201 | - | - | virtio_console: give the saved state a single schema version |
| 192 | `beb802d77` | 192 | 190 | 197 | 198 | 202 | - | - | net_backend_resources: give the egress policy digest a single encoding |
| 193 | `08990b2b1` | 193 | 191 | 198 | 199 | 203 | - | - | microVM: give every NIC an IPv6 identity |
| 194 | `2b7893305` | 194 | 192 | 199 | 200 | 204 | - | - | virtiofs: restart the microVM saved-state schema at 1 |
| 195 | `458c28d59` | 195 | 193 | 200 | 201 | 205 | - | - | chipset: drop the retired RTC transaction state |
| 196 | `71c16e456` | 196 | 194 | 201 | 202 | 206 | - | - | openvmm: require the snapshot's VM-time cut |
| 197 | `44f8362c4` | 197 | 195 | 202 | 203 | 207 | - | - | microVM: always reserve the virtio-fs slot |
| 198 | `c56322552` | 198 | 196 | 203 | 204 | 208 | - | - | vmservice: give the microVM fields upstream-compatible numbers |
| 199 | `6e36756fe` | 199 | 197 | 204 | 205 | 209 | - | - | microVM: drop the history from the layout and snapshot helpers |
| 200 | `202ac475b` | 200 | 198 | 205 | 206 | 210 | - | - | openvmm_helpers: number the snapshot's fields without gaps |
| 201 | `c77dd57a6` | 201 | 199 | 206 | 207 | 211 | - | - | microVM: drain the boot console's terminal output at guest exit (microsoft/openvmm#4894) |
| 202 | `ad26b8c6e` | 202 | 200 | 207 | 208 | 212 | - | - | vmm_tests: drive the microVM guests without portb input (microsoft/openvmm#4886) |
| 203 | `ba19d45fc` | 203 | 201 | 208 | 209 | 213 | - | - | microVM: remove portb's host input (microsoft/openvmm#4887) |
| 204 | `b68d9ac7a` | 204 | 202 | 209 | 210 | 214 | - | - | pal: support long paths in secure file and directory creation (microsoft/openvmm#4916) |
| 205 | `fc398b750` | 205 | 203 | 210 | 211 | 215 | - | - | state_unit: log the units that a state change waits for (microsoft/openvmm#4921) |
| 206 | `16f2f992a` | 206 | 204 | 211 | 212 | 220 | - | - | net_consomme: give a port forward that cannot bind a stable code (microsoft/openvmm#4951) |
| 207 | `d69cf845a` | 207 | 205 | 212 | 213 | - | - | - | virtio_net: fail the queue start when the endpoint cannot create queues (microsoft/openvmm#4953) |
| 208 | `d70139608` | 208 | 206 | 213 | 214 | - | - | - | virtio: test removal before start and stop/start queue progress (microsoft/openvmm#4954) |
| 209 | `a34636e95` | 209 | 207 | 214 | 215 | - | - | - | virtio: document how a queue that fails to start after restore is reported (microsoft/openvmm#4955) |
| 210 | `1236ed71a` | 210 | 208 | 215 | 216 | - | - | - | microVM: identify snapshot blocks only by storage generation (microsoft/openvmm#4970) |
| 211 | `c5b4ef49d` | 211 | 209 | 216 | 217 | - | - | - | microVM: check the storage generation of a restore's sandbox blocks (microsoft/openvmm#4970) |
| 212 | `b732258f8` | 212 | 210 | 217 | 218 | - | - | - | virtio: keep the progress of queues that a failed start stops (microsoft/openvmm#4968) |
| 213 | `7ee945347` | 213 | 211 | 218 | 219 | - | - | - | vmcore: confirm device starts before the VM runs (microsoft/openvmm#4969) |
| 214 | `8c3a7e61a` | 214 | 212 | 219 | 220 | - | - | - | microVM: publish the outcome report below long Windows paths (microsoft/openvmm#4981) |
| 215 | `6f80f4862` | 215 | 213 | 220 | 222, 223 | - | - | - | virt: read the guest TSC offset until it is within its bound (microsoft/openvmm#4984) |
| 216 | `9b742e593` | 216 | 214 | 221 | - | - | - | - | cpu_profile: clear AMX's features without its XSAVE state (microsoft/openvmm#4986) |
| 217 | `94b5a537f` | 217 | 215 | 222 | - | - | - | - | cpu_profile: clear AVX10, whose leaf 0x24 keeps no data (microsoft/openvmm#4986) |
| 218 | `ee95b0e25` | 218 | 216 | 223 | - | - | - | - | cpu_profile: pin intel.graniterapids.v1 for Granite Rapids (microsoft/openvmm#4986) |
| 219 | `28799495d` | 219 | 217 | 224 | - | - | - | - | cpu_profile: pin amd.turin.v2 for Turin hosts without the TSA immunities (microsoft/openvmm#4987) |
| 220 | `875ee79e2` | 220 | 218 | 225 | - | - | - | - | openvmm_entry: refuse TCP endpoints for microVM consoles (microsoft/openvmm#4988) |
| 221 | `9e86ab16c` | 221 | 219 | 226 | - | - | - | - | chipset: keep undelivered microVM portb output (microsoft/openvmm#4989) |
| 222 | `ae41d3906` | 222 | 220 | 227 | - | - | - | - | microvm: let a restore attach a listener to a disconnected boot console (microsoft/openvmm#4991) |
| 223 | `563802d1a` | 223 | 221 | 228 | - | - | - | - | vmservice: carry microVM blocks, network, RAM file, and share policy (microsoft/openvmm#4992) |
| 224 | `ba15f3b01` | 224 | 222 | 229 | - | - | - | - | openvmm_helpers: don't resume beside a restorable staged snapshot (microsoft/openvmm#4993) |
| 225 | `23f8c6dfc` | 225 | 223 | 230 | - | - | - | - | openvmm_helpers: refuse earlier snapshot formats before decoding them (microsoft/openvmm#4994) |
| 226 | `232fe5a16` | 226 | 224 | 231 | - | - | - | - | openvmm_helpers: reserve the manifest fields that upstream assigns (microsoft/openvmm#4994) |
| 227 | `3fba616e6` | 227 | 225 | 232 | - | - | - | - | openvmm_helpers: refuse an unknown paired-scratch restore mode (microsoft/openvmm#4995) |
| 228 | `e690bfd1a` | 228 | 226 | 233 | - | - | - | - | microvm: carry a clone's activation record in the restore packet (microsoft/openvmm#4996) |
| 229 | `8add71117` | 229 | 227 | 234 | - | - | - | - | openvmm_helpers: don't publish a snapshot over an empty directory (microsoft/openvmm#4997) |
| 230 | `9d658512b` | 230 | 228 | 235 | - | - | - | - | openvmm_helpers: admit kernel tuning on platform command lines (microsoft/openvmm#5001) |
| 231 | `389b493aa` | 231 | 229 | 236 | - | - | - | - | openvmm_helpers: compute memory-expansion ranges without overflow (microsoft/openvmm#5003) |
| 232 | `cb7e0fd50` | 232 | 230 | 237 | - | - | - | - | openvmm_helpers: refuse a FIFO artifact without waiting for a writer (microsoft/openvmm#5004) |
| 233 | `28e182ff4` | 233 | 231 | 238 | - | - | - | - | vmotherboard: gate a device's input without closing it to the vCPUs (microsoft/openvmm#5005) |
| 234 | `5f336d11e` | 234 | 232 | 239 | - | - | - | - | petri: let a microVM test be its snapshot controller (microsoft/openvmm#5010) |
| 235 | `7dd57f6cd` | 235 | 233 | 240 | - | - | - | - | openvmm_core: run a quiesced microVM again only through its rollback (microsoft/openvmm#5010) |
| 236 | `350641ab9` | 236 | 237 | 244 | - | - | - | - | state_unit: fail a stop instead of panicking when a unit is lost (microsoft/openvmm#5013) |
| 237 | `bb739d0a2` | 237 | 238 | 245 | - | - | - | - | state_unit: fail a reset, save, restore, or time advance on a lost unit (microsoft/openvmm#5013) |
| 238 | `232c1d957` | 238 | 239 | 246 | - | - | - | - | virtio: test PCI re-save before start and missing queue progress (microsoft/openvmm#5016) |
| 239 | `ea57c9598` | 239 | 244 | 251 | - | - | - | - | microVM: attach direct-claimed scratch in place instead of linking it (microsoft/openvmm#5042) |
| 240 | `561770a44` | 240 | 245 | 252 | - | - | - | - | virt: split clock-token command lines at every Linux whitespace byte (microsoft/openvmm#5044) |
| 241 | `6474016a4` | 241 | 246 | 253 | - | - | - | - | virt: accept a snapshot downtime of up to 365 days (microsoft/openvmm#5046) |
| 242 | `b72c8a58c` | 242 | 247 | 254 | - | - | - | - | virt: report when the synchronized TSC set lets the TSCs run (microsoft/openvmm#5052) |
| 243 | `593d85fba` | 243 | 248 | 255 | - | - | - | - | openvmm: run restored VM time from the instant the TSCs resume (microsoft/openvmm#5052) |
| 244 | `a1a8aee8a` | 244 | 249 | 256 | - | - | - | - | virt_kvm: share the guest RAM of the real-mode KVM tests (microsoft/openvmm#5053) |
| 245 | `ccd424577` | 245 | 250 | 257 | - | - | - | - | virt_kvm: do not fire an expired LAPIC timer whose interrupt is pending (microsoft/openvmm#5053) |
| 246 | `5d2104e13` | 246 | 251 | 258 | - | - | - | - | vmm_core, virt: run a restored LAPIC timer from the time ABI anchors (microsoft/openvmm#5053) |
| 247 | `5392b506f` | 247 | 252 | 259 | - | - | - | - | kvm: deliver the interrupts that KVM holds without running the VP (microsoft/openvmm#5055) |
| 248 | `a229d8bd0` | 248 | 253 | 260 | - | - | - | - | virt_kvm: keep a restore from firing a taken one-shot LAPIC timer again (microsoft/openvmm#5055) |
| 249 | `5cbc8cc7e` | 249 | 255 | 262 | - | - | - | - | microVM: saturate the time ABI restore's logged phase durations (microsoft/openvmm#5056) |
| 250 | `237a7ebe8` | 250 | 256 | 263 | - | - | - | - | chipset: start each PIT low/high count with its low byte (microsoft/openvmm#5061) |
| 251 | `a164a9297` | 251 | 257 | 264 | - | - | - | - | chipset: keep the sub-100 ns part of the PIT's last tick time (microsoft/openvmm#5065) |
| 252 | `c560d2560` | 252 | 258 | 265 | - | - | - | - | chipset: reject a saved PIT control word with RW = 0 (microsoft/openvmm#5067) |
| 253 | `d0e7a90e6` | 253 | 260 | 267 | - | - | - | - | chipset: zero-extend wide reads of the RTC's ports (microsoft/openvmm#5072) |
| 254 | `438093f12` | 254 | 272 | - | - | - | - | - | openvmm_entry: parse the microVM options on a big stack on every host (microsoft/openvmm#5077) |
| 255 | `f392eea7f` | 255 | 274 | - | - | - | - | - | chipset: zero-extend wide IOAPIC MMIO reads (microsoft/openvmm#5084) |
| 256 | `a742a0d81` | 256 | 275 | - | - | - | - | - | chipset: reject a restored IOAPIC id that the ID register cannot hold (microsoft/openvmm#5087) |
| 257 | `e2e40c755` | 257 | 276 | - | - | - | - | - | cpu_profile: refuse a restored host profile outside the derivation policy (microsoft/openvmm#5094) |
| 258 | `c43752f56` | 258 | 277 | - | - | - | - | - | virt: saturate the XSAVE lengths that overflow (microsoft/openvmm#5095) |
| 259 | `be38ffc8e` | 259 | 278 | - | - | - | - | - | virt_kvm: refuse an XSAVE layout that KVM cannot hold (microsoft/openvmm#5095) |
| 260 | `802a8fc1e` | 260 | 279 | - | - | - | - | - | virt_kvm: refuse a restored EFER.SVME for a guest whose CPUID has no SVM (microsoft/openvmm#5097) |
| 261 | `4cb857a97` | 261 | 280 | - | - | - | - | - | virt: hold each SYSENTER MSR in the field that names it (microsoft/openvmm#5100) |
| 262 | `1555420f9` | 262 | 281 | - | - | - | - | - | virt_kvm: refuse a restored XSAVE component outside the caps (microsoft/openvmm#5101) |
| 263 | `0bf85512b` | 263 | 282 | - | - | - | - | - | virt_kvm: reject a restored APIC timer whose delivery mode is not Fixed (microsoft/openvmm#5102) |
| 264 | `a04fae1b1` | 264 | 283 | - | - | - | - | - | virt: let a backend refuse to save a state that it cannot hold (microsoft/openvmm#5105) |
| 265 | `faad02e04` | 265 | 284 | - | - | - | - | - | virt_kvm: refuse to save a vCPU whose GIF is clear (microsoft/openvmm#5105) |
| 266 | `f05db63d8` | 266 | 285 | - | - | - | - | - | vmm_tests: check that a new KVM VP can be saved (microsoft/openvmm#5105) |
| 267 | `d9b88fafb` | 267 | 286 | - | - | - | - | - | virt_kvm: refuse to restore an injected #BP or #OF (microsoft/openvmm#5106) |
| 268 | `56330e7c9` | 268 | 287 | - | - | - | - | - | virt_kvm: report an INIT or a SIPI that a stopped VP has not accepted (microsoft/openvmm#5107) |
| 269 | `01661368a` | 269 | 288 | - | - | - | - | - | vmm_core: refuse a snapshot capture while a VP holds an INIT or a SIPI (microsoft/openvmm#5107) |
| 270 | `fe866c963` | 270 | 289 | - | - | - | - | - | consomme: relay ICMP echo replies on Linux hosts (microsoft/openvmm#5108) |
| 271 | `982469696` | 271 | 290 | - | - | - | - | - | consomme: poll a TCP listener again after it accepts a connection (microsoft/openvmm#5109) |
| 272 | `f13e9c2de` | 272 | 291 | - | - | - | - | - | virt: save and restore a guest's IA32_SPEC_CTRL (microsoft/openvmm#5110) |
| 273 | `19d1a3a6a` | 273 | 292 | - | - | - | - | - | vmm_tests: check that a restore keeps a guest's SPEC_CTRL controls (microsoft/openvmm#5110) |
| 274 | `e61703653` | 274 | 293 | - | - | - | - | - | virt_whp: check that WHP takes SPEC_CTRL under a host profile (microsoft/openvmm#5110) |
| 275 | `0ef9a39bc` | 275 | 294 | - | - | - | - | - | virt: build the SPEC_CTRL profile test only for x86-64 guests (microsoft/openvmm#5110) |
| 276 | `6f839de82` | 276 | 295 | - | - | - | - | - | virt_kvm: hide AMD's legacy PMU from a time ABI guest (microsoft/openvmm#5114) |
| 277 | `13d611a34` | 277 | 296 | - | - | - | - | - | virt: save a guest's user-wait, debug, and feature MSRs (microsoft/openvmm#5114) |
| 278 | `81e8f814d` | 278 | 297 | - | - | - | - | - | virt_kvm: save the machine-check state of a VP (microsoft/openvmm#5114) |
| 279 | `9001fc90c` | 279 | 298 | - | - | - | - | - | Guide: describe the VP state elements that hold a guest's MSRs (microsoft/openvmm#5114) |
| 280 | `de9676230` | 280 | 299 | - | - | - | - | - | virt_kvm: raise #GP for AMD's HWCR under an Intel CPU profile (microsoft/openvmm#5115) |
| 281 | `926d21760` | 281 | 300 | - | - | - | - | - | virt_kvm: raise a VP thread's PKRU of 0 so that saves keep the guest's (microsoft/openvmm#5117) |
| 282 | `a078e0b38` | 282 | 301 | - | - | - | - | - | virt_kvm: restore the init components of an XSAVE area as init (microsoft/openvmm#5118) |
| 283 | `6e449b8b7` | 283 | 302 | - | - | - | - | - | virt_kvm: refuse to restore a short or malformed XSAVE area (microsoft/openvmm#5119) |
| 284 | `92aad2d6f` | 284 | 303 | - | - | - | - | - | virt_kvm: drop interrupts in the reserved remote-read delivery mode (microsoft/openvmm#5120) |
| 285 | `906a14dcd` | 285 | 304 | - | - | - | - | - | virt_kvm: refuse to save a vCPU in SMM or with an SMI pending (microsoft/openvmm#5121) |
| 286 | `a6bff237a` | 286 | 305 | - | - | - | - | - | virt_kvm: take a vCPU out of SMM when its activity is set (microsoft/openvmm#5121) |
| 287 | `aa6b49013` | 287 | 306 | - | - | - | - | - | virt_kvm: save and restore two pending NMIs (microsoft/openvmm#5122) |
| 288 | `4fae1a029` | 288 | 307 | - | - | - | - | - | virt: refuse a pending NMI count that a backend cannot hold (microsoft/openvmm#5122) |
| 289 | `731256ce2` | 289 | 308 | - | - | - | - | - | virt_kvm: save a queued exception once and refuse one KVM would drop (microsoft/openvmm#5123) |
| 290 | `3dc53a71a` | 290 | 309 | - | - | - | - | - | virt: let a restore's check accept what a hypervisor documents it keeps (microsoft/openvmm#5123) |
| 291 | `48d97c31a` | 291 | 310 | - | - | - | - | - | virt: let a saved activity hold the kind of its interrupt shadow (microsoft/openvmm#5125) |
| 292 | `ebab3ee69` | 292 | 311 | - | - | - | - | - | virt_kvm: keep the kind of an interrupt shadow across a save and restore (microsoft/openvmm#5125) |
| 293 | `70afadf04` | 293 | 312 | - | - | - | - | - | virt: refuse an interrupt shadow kind that a backend cannot hold (microsoft/openvmm#5125) |
| 294 | `e3e0367e4` | 294 | 313 | - | - | - | - | - | virt: say what an STI shadow restored as a MOV SS shadow loses (microsoft/openvmm#5125) |
| 295 | `33acd604c` | 295 | 314 | - | - | - | - | - | virt: refuse to restore an element that the partition lacks (microsoft/openvmm#5126) |
| 296 | `8845b4b20` | 296 | 315 | - | - | - | - | - | vmm_tests: check that each backend refuses unsupported restore state (microsoft/openvmm#5126) |
| 297 | `f647b20ba` | 297 | - | - | - | - | - | - | virt_whp: keep a restored halted VP halted (microsoft/openvmm#5129) |
| 298 | `cc92efe0e` | 298 | - | - | - | - | - | - | vmm_tests: build the fixture's memory accessor on Windows only (microsoft/openvmm#5129) |
| 299 | `2f5babf96` | 299 | - | - | - | - | - | - | virt_whp: resuspend a restored VP without the hypervisor's translation (microsoft/openvmm#5129) |
| 300 | `9d5b03c5e` | 300 | - | - | - | - | - | - | virt_whp: let a restored VP with a pending NMI take it (microsoft/openvmm#5129) |
| 301 | `de2e45c73` | 301 | - | - | - | - | - | - | virt_kvm: map RAM into KVM's SMM address space (microsoft/openvmm#5131) |
| 302 | `f0c11b120` | 303 | - | - | - | - | - | - | virt_whp, virt_mshv: save and restore a guest's IA32_UMWAIT_CONTROL (microsoft/openvmm#5134) |
| 303 | `6e4e28d97` | 304 | - | - | - | - | - | - | virt: save and restore a guest's IA32_BNDCFGS (microsoft/openvmm#5134) |
| 304 | `eb1682676` | 305 | - | - | - | - | - | - | virt, virt_kvm: save and restore AMD's VM_CR, VM_HSAVE_PA, and OSVW (microsoft/openvmm#5134) |
| 305 | `6a723a820` | 306 | - | - | - | - | - | - | virt_mshv, virt_whp: refuse to save a pending software interrupt (microsoft/openvmm#5134) |
| 306 | `c2c14209c` | 307 | - | - | - | - | - | - | virtio_net: keep the saved queue lifecycle through a failed start (microsoft/openvmm#5136) |
| 307 | `341688f0c` | 308 | - | - | - | - | - | - | pci_core: program the delivery of restored MSI-X vectors (microsoft/openvmm#5136) |
| 308 | `3d9093030` | 309 | - | - | - | - | - | - | virtio: hold restored interrupt lines until the device starts (microsoft/openvmm#5136) |
| 309 | `1b683c9db` | 310 | - | - | - | - | - | - | virtiofs: test a transport restore of a file gone from the host (microsoft/openvmm#5136) |
| 310 | `21fc05044` | 311 | - | - | - | - | - | - | chipset_device_worker: test requests to an ended device worker (microsoft/openvmm#5136) |
| 311 | `c4766920f` | 312 | - | - | - | - | - | - | vmcore, virtio: say that a failed start keeps its queues' progress (microsoft/openvmm#5136) |
| 312 | `f6499c433` | 313 | - | - | - | - | - | - | openvmm_core: roll back a microVM snapshot only at its boundary (microsoft/openvmm#5140) |
| 313 | `600de1fca` | 314 | - | - | - | - | - | - | Guide: record the snapshot manifest's field map with upstream (microsoft/openvmm#5141) |
| 314 | `9a75872e6` | 315 | - | - | - | - | - | - | openvmm_helpers: refuse a malformed snapshot ID by its field (microsoft/openvmm#5141) |
| 315 | `69e69e23e` | 317 | - | - | - | - | - | - | vmotherboard: find offsets in an I/O region that ends at the top (microsoft/openvmm#5168) |
| 316 | `804097333` | 318 | - | - | - | - | - | - | vmotherboard: wire a chipset device only once its unit spawns (microsoft/openvmm#5170) |
| 317 | `46fd43808` | 319 | - | - | - | - | - | - | virtiofs: skip empty buffers in a request's descriptor chain (microsoft/openvmm#5171) |
| 318 | `d11965f2a` | 320 | - | - | - | - | - | - | fuse: check a request's data size before allocating its data (microsoft/openvmm#5172) |
| 319 | `370aa0903` | 321 | - | - | - | - | - | - | fuse: refuse a request whose length cannot hold its header (microsoft/openvmm#5173) |
| 320 | `110227610` | 322 | - | - | - | - | - | - | virtiofs: refuse SETATTR times with out-of-range nanoseconds (microsoft/openvmm#5175) |
| 321 | `0fc1a22b6` | 323 | - | - | - | - | - | - | fuse: reply to FUSE_IOCTL with the file system's output (microsoft/openvmm#5176) |
| 322 | `58db1f8bc` | 324 | - | - | - | - | - | - | fuse: count the data in the length of an argument-and-data reply (microsoft/openvmm#5177) |
| 323 | `614d7731d` | 325 | - | - | - | - | - | - | fuse: reply EIO for an error code that a reply cannot carry (microsoft/openvmm#5180) |
| 324 | `2c9c427c5` | 326 | - | - | - | - | - | - | fuse: refuse a reply longer than its header can describe (microsoft/openvmm#5194) |
| 325 | `ea8f4a9a6` | 327 | - | - | - | - | - | - | fuse: check that a session sends one reply to each test request (microsoft/openvmm#5195) |
| 326 | `b536e541f` | 328 | - | - | - | - | - | - | fuse: fail a reply longer than the request allows with EIO (microsoft/openvmm#5195) |
| 327 | `521868176` | 329 | - | - | - | - | - | - | fuse: reply EIO when a write or copy count overflows u32 (microsoft/openvmm#5198) |
| 328 | `795b49ee7` | 330 | - | - | - | - | - | - | fuse: fail FUSE_INIT when init wants flags the client did not offer (microsoft/openvmm#5200) |
| 329 | `910a23e26` | 331 | - | - | - | - | - | - | virtiofs: deduplicate inodes by device and inode number (microsoft/openvmm#5201) |
| 330 | `663c29454` | 332 | - | - | - | - | - | - | virtiofs: key guest inode numbers by host device (microsoft/openvmm#5202) |
| 331 | `1aa8e487c` | 334 | - | - | - | - | - | - | virtiofs: saturate lookup counts rather than wrap them to zero (microsoft/openvmm#5205) |
| 332 | `88aa0ea0b` | 341 | - | - | - | - | - | - | virtiofs: bound the buffer of an aggregate root's directory read (microsoft/openvmm#5207) |
| 333 | `8351f403b` | 343 | - | - | - | - | - | - | virtiofs: refuse a microVM file child that its volume cannot name (microsoft/openvmm#5209) |
| 334 | `9d3817d1f` | 344 | - | - | - | - | - | - | virtiofs: apply open's write rule to a create of an existing name (microsoft/openvmm#5210) |
| 335 | `719978043` | 346 | - | - | - | - | - | - | virtiofs: serve a handle only for its node while the guest holds it (microsoft/openvmm#5212) |
| 336 | `b4fd7938b` | 348 | - | - | - | - | - | - | lxutil: do not mistake unmapped groups for the overflow GID (microsoft/openvmm#5214) |
| 337 | `ab8cc5394` | 350 | - | - | - | - | - | - | virtiofs: fail a synthetic root read too small for its next entry (microsoft/openvmm#5216) |
| 338 | `417ff1bb7` | 352 | - | - | - | - | - | - | virtiofs: refuse to link a node whose last name is gone (microsoft/openvmm#5218) |
| 339 | `8046a1cee` | 353 | - | - | - | - | - | - | virtiofs: resolve a microVM file child's path as its identity does (microsoft/openvmm#5219) |
| 340 | `32004291d` | 354 | - | - | - | - | - | - | virtiofs: count only child directories in the synthetic root's nlink (microsoft/openvmm#5220) |
| 341 | `c885cf4f6` | 355 | - | - | - | - | - | - | lxutil: fail closed before dropping groups it cannot restore (microsoft/openvmm#5221) |
| 342 | `e4cc222fd` | 356 | - | - | - | - | - | - | virtiofs: accept FSYNCDIR through the synthetic root's handle (microsoft/openvmm#5222) |
| 343 | `a038ebeaf` | 357 | - | - | - | - | - | - | virtiofs: refuse renames of directories that hold denied paths (microsoft/openvmm#5223) |
| 344 | `80413e8e7` | 358 | - | - | - | - | - | - | lxutil: report the names under which Windows stores an entry (microsoft/openvmm#5224) |
| 345 | `86bbed5f4` | 359 | - | - | - | - | - | - | virtiofs: hide a microVM policy path in another case on Windows (microsoft/openvmm#5224) |
| 346 | `c3f2d343a` | 360 | - | - | - | - | - | - | virtiofs: reach a Windows entry only by its own name in a microVM (microsoft/openvmm#5224) |
| 347 | `0d84f7522` | 361 | - | - | - | - | - | - | virtiofs: let a Windows file child name its file by any of its names (microsoft/openvmm#5224) |
| 348 | `d907600df` | 362 | - | - | - | - | - | - | virtiofs: fail a Windows rename without a source before the host (microsoft/openvmm#5224) |
| 349 | `303154b27` | 363 | - | - | - | - | - | - | virtiofs: skip a Windows READDIRPLUS name whose case has changed (microsoft/openvmm#5224) |
| 350 | `301937559` | 364 | - | - | - | - | - | - | virtiofs: never hand out u64::MAX, the synthetic root's handle (microsoft/openvmm#5225) |
| 351 | `6007b6460` | 366 | - | - | - | - | - | - | lxutil: keep O_TRUNC from making a Windows handle writable (microsoft/openvmm#5226) |
| 352 | `8ce4155d8` | 367 | - | - | - | - | - | - | virtiofs: refuse a write through a handle not open for writing (microsoft/openvmm#5226) |
| 353 | `766a74a9d` | 368 | - | - | - | - | - | - | virtiofs: reach a node whose last name is gone only through its handles (microsoft/openvmm#5227) |
| 354 | `c0efd6c92` | 369 | - | - | - | - | - | - | virtiofs: list btrfs placeholders and bind mounts in microVM READDIRPLUS (microsoft/openvmm#5228) |
| 355 | `899dcd63f` | 370 | - | - | - | - | - | - | virtiofs: stop microVM READDIRPLUS from aliasing the listed node (microsoft/openvmm#5229) |
| 356 | `b73db6676` | - | - | - | - | - | - | - | fuse: adopt a FUSE_INIT negotiation only once its reply is sent (microsoft/openvmm#5248) |
| 357 | `e8324ea31` | - | - | - | - | - | - | - | vmotherboard: claim a device's PCI slot only once its unit spawns (microsoft/openvmm#5249) |
| 358 | `7e94a090a` | - | - | - | - | - | - | - | virtiofs: say that inodes deduplicate only within a volume (microsoft/openvmm#5251) |
| 359 | `06e69049e` | - | - | - | - | - | - | - | chipset_arc_mutex_device: find offsets in a region that ends at the top (microsoft/openvmm#5252) |
| 360 | `3f7112dea` | - | - | - | - | - | - | - | chipset_device_fuzz: find offsets in an I/O region that ends at the top (microsoft/openvmm#5252) |
| 361 | `09966aa63` | - | - | - | - | - | - | - | virtiofs: keep the node that a LINK returns until the guest forgets it (microsoft/openvmm#5253) |
| 362 | `b5914cf2d` | - | - | - | - | - | - | - | virtiofs: fail a microVM save while the guest holds a replaced object (microsoft/openvmm#5254) |
| 363 | `b2ac1073d` | - | - | - | - | - | - | - | virtiofs: fail a microVM save of nodes or handles that a restore rejects (microsoft/openvmm#5254) |
| 364 | `5e0e72d72` | - | - | - | - | - | - | - | virtiofs: say which nodes a microVM save keeps apart (microsoft/openvmm#5254) |
| 365 | `be75ab532` | - | - | - | - | - | - | - | fuse: fail FUSE_COPY_FILE_RANGE_64 with ENOSYS, not EINVAL (microsoft/openvmm#5255) |
| 366 | `9737fc609` | - | - | - | - | - | - | - | fuse: let FUSE_INIT negotiate only once a session has ended (microsoft/openvmm#5257) |
| 367 | `1a0f6be2d` | - | - | - | - | - | - | - | fuse: call destroy after an init that the session does not adopt (microsoft/openvmm#5257) |
| 368 | `9805f8984` | - | - | - | - | - | - | - | fuse: say which FUSE_INIT waits for the end of a session (microsoft/openvmm#5257) |
| 369 | `8c1204c68` | - | - | - | - | - | - | - | virt_support_apic: select a reserved DFR model by bit 31 (microsoft/openvmm#5260) |
| 370 | `aa145aff9` | - | - | - | - | - | - | - | missing_dev: claim a full-width or empty range without overflow (microsoft/openvmm#5261) |
| 371 | `2bb009b01` | - | - | - | - | - | - | - | virtiofs: read microVM directory listings to the end on Windows (microsoft/openvmm#5262) |
| 372 | `6ace73360` | - | - | - | - | - | - | - | vmm_core: keep a debugger break into a paused VM out of its saved state (microsoft/openvmm#5263) |
| 373 | `82ff6d2f7` | - | - | - | - | - | - | - | virt_kvm: clear a vCPU's paravirtual kick when its activity is set (microsoft/openvmm#5264) |
| 374 | `4760f77f0` | - | - | - | - | - | - | - | petri: let a microVM test arm the post-restore input gate (microsoft/openvmm#5265) |
| 375 | `d44a113be` | - | - | - | - | - | - | - | openvmm_core: stop a gated restore's deadline at its acknowledgement (microsoft/openvmm#5265); fold into 090, its `restore_acknowledged` changes and unit tests into 134, its VMM test into 374 |
| 376 | `c440ce3cc` | - | - | - | - | - | - | - | openvmm: say that the restore readiness event precedes guest repair (microsoft/openvmm#5270); fold into 090, its `--restore-processors` changes into 092, its hotplug sentence into 142 |
| 377 | `3f5506f9d` | - | - | - | - | - | - | - | vmservice: say that the restore readiness event precedes guest repair (microsoft/openvmm#5270); fold into 119 |
| 378 | `2d222dcdc` | - | - | - | - | - | - | - | vm_topology: refuse host VPs per socket outside the builder's range (microsoft/openvmm#5271) |
| 379 | `a8aeeb65e` | - | - | - | - | - | - | - | virt_whp: refuse to restore an idle VP with the user-mode APIC (microsoft/openvmm#5272) |
| 380 | `32e824728` | - | - | - | - | - | - | - | virt_kvm: refuse more enlightened VPs than the GSI routing table holds (microsoft/openvmm#5273) |
| 381 | `5d3f172c3` | - | - | - | - | - | - | - | virt_kvm: refuse to save a vCPU in an SEV-ES AP reset hold (microsoft/openvmm#5275) |
| 382 | `acfaa5199` | - | - | - | - | - | - | - | virt: saturate Intel's L3 sharing count above 4096 VPs per socket (microsoft/openvmm#5276) |
| 383 | `fa239c823` | - | - | - | - | - | - | - | virt_whp: write no internal activity state with the user-mode APIC (microsoft/openvmm#5277) |
| 384 | `44747bf1c` | - | - | - | - | - | - | - | pal_async: report why a driver cannot set up its io-uring (microsoft/openvmm#5278) |
| 385 | `043083f40` | - | - | - | - | - | - | - | virtiofs: count only shown entries toward a microVM listing's bound (microsoft/openvmm#5279) |
| 386 | `f06060dfe` | - | - | - | - | - | - | - | vmm_core: forget a debugger's halt reports when it detaches (microsoft/openvmm#5280) |
| 387 | `320e09cf5` | - | - | - | - | - | - | - | vmm_core: clear VP debug state on detach only if the debugger set it (microsoft/openvmm#5280) |
| 388 | `1633b3797` | - | - | - | - | - | - | - | debug_worker: detach the debugger when its session ends (microsoft/openvmm#5280) |
| 389 | `350018ce4` | - | - | - | - | - | - | - | virtiofs: let a new microVM object take the last node ID (microsoft/openvmm#5281); fold into 081, its test into 350 |
| 390 | `770bc61e7` | - | - | - | - | - | - | - | virtiofs: restore microVM handles opened with access mode 3 (microsoft/openvmm#5282) |
| 391 | `c7f6a04fd` | - | - | - | - | - | - | - | lxutil: reject xattr names longer than an NTFS EA name allows (microsoft/openvmm#5283) |
| 392 | `e61fa5326` | - | - | - | - | - | - | - | lxutil: fail an empty symlink target with ENOENT on Windows (microsoft/openvmm#5284) |
| 393 | `5af753b17` | - | - | - | - | - | - | - | virtiofs: reopen a microVM handle with O_DIRECT on restore (microsoft/openvmm#5285); fold into 083, its test into 335 |
| 394 | `9822f732a` | - | - | - | - | - | - | - | lxutil: hard-link to one-character names on Windows (microsoft/openvmm#5286) |

The remarks that say whether commits apply to upstream main record what was found on upstream main
when each commit was mapped: the v3.0 reorganization's base, `b2676222b`, for the commits that it
regrouped, and later fork bases, up to `7d90c4104` for v6.1 positions 270-315, for later commits,
and `c0694c4da`, upstream main on 2026-11-16, for positions 297-305, `c4419be15`, upstream main on
2026-11-17, for positions 306-311, `2640ad182`, upstream main later that day, for positions 312-314,
`715927303`, upstream main on 2026-11-18, for positions 315-318, `5cf28492b`, upstream main on
2026-11-19, for positions 319-322, `78af54a51`, upstream main on 2026-11-20, for position 323,
`d83f87209`, upstream main later that day, for position 324, `265f0a73f`, upstream main later still,
for positions 325 and 326, `bf353460f`, upstream main on 2026-11-21 and the fork base of v7.2 and
v8.0, for positions 327-355, `b50d0c979`, upstream main later on 2026-11-23, for position 356,
`5d6ab1cfa`, upstream main later that day, for positions 357-368, `efe0b08df`, upstream main later
still, for positions 369-375, and `8d4e03132`, upstream main on 2026-11-24, for positions
376-394. They name the positions that the commits have now, but neither
the v8.0 reorganization and the v7.2 rebase, both on `bf353460f`, the v7.1 rebase on `9a875b563`,
nor the v7.0 reorganization on `e6b458cc0` checked them again, nor did the v6.1 rebase check the
earlier ones on `7d90c4104`.

Some of these commits change mapped work rather than add to it. Position 142 removes the legacy
microVM clock paths, changing files that PR10 and PR17 also change, and makes manifest version 6 the
only snapshot format, changing files that PR11 and PR17 also change. Position 171 changes the
sandbox block attachment of PR19's position 087 and the restore of position 161. Position 173
changes the snapshot network policy and the forward refusal of PR21's position 101 and the snapshot
refusals of position 169. Position 175 changes the dormant-slot restore of PR18's position 085, as
the aggregates of position 164 restricted it. Position 182 removes the `runtime` and `custom`
sandbox block roles that PR19's position 086 adds, with the slots and IRQs that PR16's position 062
places for them and the shared-status words that PR16's position 064 gives them, which stay
unassigned, and the role names and layer counts of PR19's position 087's snapshot capture and
restore. Positions 183 and 184 change the feature masks of the devices that PR16's position 062
places, the console's fall-through among them, and that PR18's positions 077, 078, and 082, PR19's
position 086, and PR23's position 111 give the NIC, the filesystem, the sandbox blocks, and the
consoles and record in the snapshot contract. Position 184 also moves the microVM ABI version of
PR17's position 069 to 3, which PR20's position 090 and PR25's position 119 check. Position 185
extends the MP table builder that microsoft/openvmm#4554 merged in place of v6.0 position 052, its
tests, those of upstream's firmware-less direct boot, and those of PR14's position 049, to route
every IOAPIC input, and passes the microVM's 24 inputs from the loader of PR15's position 057.
Position 186 moves every virtio slot from the ISA IRQ that PR16's position 062 gives it, and the NIC
from the backend-specific IRQ that PR18's position 078 selects, to the IOAPIC input of its window,
which changes the interrupts and discovery tokens that PR17's position 069 records in the snapshot
contract and that the slots of PR18, PR19, and PR23 use. Position 201 extends the portb exit drain
of PR24's position 114 to the relay that copies the boot console of PR16's position 062 to the
terminal, and gives the management RPC's drain of PR25's position 119 no such relay. Position 202
makes the guests of the microVM cold-boot test and portb harness of PR15's position 060, of the
check of PR17's position 075 that an unconfigured snapshot request continues, and of the management
RPC's microVM test of PR25's position 119 run without portb's host input, and removes the harness's
input. Position 203 then removes that input: the receive path of the portb console of PR15's
position 055, which PR20's position 089 shares with the restore records and PR20's position 090
gates, and the console input that PR15's position 059 gives the device on the CLI. Since those reads
also noticed a departed peer of a listening endpoint, position 203 has the device poll the backend
for it instead. Position 205 documents and tests that the input quiesce of PR09's position 014 logs
each unit that has not answered, as the CLI guide now says of a capture. Position 208 adds tests to
the virtio saved-state tests of PR13's position 037, and position 209 extends the restore
descriptions that PR18's position 084 and PR25's position 119 add to the CLI and management RPC
guides, and the restore section of the snapshot guide after the readiness event of PR12's position
035, which PR20's position 090 gates. Position 210 removes the SHA-256 identity of sandbox blocks,
with the whole-file hashing and the verified copy that PR19's position 087 adds, and the
`--snapshot-block-identity` option and default that position 161 adds, whose storage generation
becomes a bound block's only identity. It also has the block contract of position 161 record a
platform capture's layers unbound, which PR19's position 088 did in the capture, and changes
position 171's test of an absent scratch. Position 211 adds a restore-time check of that generation
to position 161's restore. Position 212 changes the failed start that PR13's position 037 gives the
virtio device task, which now keeps the progress of the queues that it stops. Position 213 changes
PR09's position 011, whose start now confirms each unit's start before it starts the unit's
dependents, and PR13's position 037, whose transports now leave a start's answer to that
confirmation instead of taking it in `poll_device`, and replaces one of position 037's tests; it
adds its confirmation next to the input quiesce of PR09's position 014 in the files that position
014 changes, and rewrites position 209's documentation. Position 214 changes the outcome report
publication of PR24's position 117, which on Windows now publishes the report through the
extended-length form of its path, and moves the extended-length encoding that position 204 gives
`pal`'s secure creation into `pal`'s file system module, which both now use. Position 220 removes
the TCP endpoints of microVM consoles, which the CLI guide described: the TCP listener that PR16's
position 062 gives the microVM's virtio console; the TCP client, the checks of loopback addresses
and stable ports, and the `tcp` identities of the boot console's attachment, its restore, and the
snapshot contract of PR17's position 069, which PR18's position 077 adds; the `Tcp` backend kind of
the console attachments of PR13's position 040; and the TCP backends that the management RPC's
restore of PR25's position 119 rebuilds. Position 221 changes the output of the portb console of
PR15's position 055, which dropped the output that it held when a write to its endpoint failed with
an error other than a broken pipe, and whose stop wrote that output to an endpoint without a peer,
which discards it; the sentence of the CLI guide on that output, which PR25's position 120 adds; and
the test endpoint and tests of position 203. Position 222 lets a restore attach a listener to a
disconnected boot console, which the restore of PR18's position 077 kept disconnected: it extends
the restore of the boot console's attachment that position 077 adds and PR23's position 112
generalizes, beside the listener rebinding of PR23's position 113, whose alignment of the machine
contract in the restore check of PR17's position 074 it extends; the disconnect policy that the
virtio console's handle of PR16's position 062 takes from the attachment; the validation of the
console attachments of PR13's position 040, which now accepts a listener that discards output while
disconnected; and the management RPC's restore of PR25's position 119, which now takes the
disconnect policy from the attachment, as the CLI's does. Position 223 replaces the management RPC's
microVM module of position 119, which the fork tail's position 124 and positions 156, 198, 220, and
222 change, with a conversion of a request into the options of the microVM CLI of PR15's position
059, whose launch, capture, and restore it then runs: it adds to the protocol that position 119
extends and position 198 renumbers the fields of the sandbox blocks and tiers of PR19's positions
086-088, of the NIC and policy of PR18's position 078 and PR21's position 101, of a RAM backing
file, which microsoft/openvmm#4553 has since added to the same field for SaveVM, of the share policy
of PR22's position 103 and positions 158, 160, 164, and 165, and of a boot console with no endpoint;
it changes the CLI's configuration builder, which takes portb's endpoint from the RPC, and its
storage builder and network endpoint, which take the microVM's resources; and it removes the exports
and accessors that only position 119's module used, the console restore that positions 220 and 222
changed among them. Position 224 changes the rollback of a failed publication that PR11's position
030 adds and its position 031 extends with the live RAM alias: the cleanup of the staging directory
now removes its manifest first, and a cleanup that cannot prove the manifest gone returns position
031's uncertain outcome, on which the controller of PR17's position 071 terminates the source, where
position 030 let it resume beside a snapshot that restore could open. It also rewords that outcome's
message, the controller's log, and the sentences on a failed publication that position 071 adds to
the snapshot and snapshot format guides. Position 225 changes the manifest's decoding, which PR11's
position 029 bounds and checks and PR12's position 033 moves to the snapshot directory that it
opens: positions 190 and 200 restarted the manifest at version 2 and numbered its fields without
gaps, which gives some fields of earlier formats other types, so restore now decodes the version and
then the magic, each alone, and refuses any other with `E_SNAPSHOT_VERSION` before it decodes the
rest, and the version and magic checks of position 029's header check become helpers that both
share. It also says why in the manifest section of the snapshot format guide, which position 190
rewrote. Position 226 adds a comment to the manifest's fields, which position 200 numbers, and a
sentence to the snapshot format guide's rule for extending the manifest, that keep fields 18-20 for
upstream's SaveVM and RestoreVM, microsoft/openvmm#4553; since the v6.1 rebase, field 18 holds
upstream's `snapshot_id`, and upstream reserves 19 and 20. Position 227 changes the restore-mode
match that position 161 adds to the capture-tier check of PR19's position 088: restore runs that
check before the machine contract's shape check, so the match now refuses a mode that it does not
know with the shape check's error, where it panicked on the assumption that the shape check had
refused the mode. Position 228 lets a platform restore end the restore packet with its clone's
activation record: it defines bit 5 of the packet's flags, which position 188 reserved when it
restarted the packet at version 1, beside the network settings of position 176; adds
`--restore-activation-record`, which the restore preparation of PR17's position 074 reads before any
vCPU runs, only for the platform tier of PR19's position 088, and with the post-restore gate of
PR20's position 090; adds the field that the management RPC's conversion of position 223 turns into
that option, in the protocol that PR25's position 119 extends; and adds a test of the restore packet
that the portb device of PR15's position 055 serves since position 132. Position 229 changes the
no-replace rename of the snapshot publication that PR11's position 030 adds, which renamed without
replacing only in Linux glibc builds and replaced an empty destination directory elsewhere: every
target now renames through `pal`, which the commit gives a no-replace rename on Unix and on Windows,
the latter on the extended-length encoding that position 214 moves into `pal`'s file system module.
It also adds a test beside position 030's test of a destination created after staging, and extends
the snapshot guide's sentence that PR17's position 071 adds. Position 230 changes the platform
tier's check of a snapshot's command line, which PR19's position 088 adds and PR20's position 092,
PR23's position 111, and positions 142 and 163 change: the allowlist of the command line's tokens
also admits a closed set of kernel tuning parameters, and the check refuses a command line that
repeats one of them, after position 092's check of the boot-online processor count. It also extends
the `--snapshot-tier` entry that position 088 adds to the CLI reference. Position 231 changes how
the canonical memory-expansion ranges that PR20's position 094 adds to the machine contract of
PR17's position 069 compute their start, which overflowed for a base RAM of at least 2^64 - 4 GiB,
and adds a test beside position 094's tests of the memory capacity. Position 232 changes the
artifact openers of the snapshot file system module that PR11's position 030 adds, with the
path-based opener and the type check, and its position 031 extends with the directory-relative
opener, through which PR12's position 033 restores: on Unix both now open with `O_NONBLOCK`, so that
position 030's type check refuses a FIFO without waiting for a writer, and then make a regular
file's descriptor blocking again. It also adds tests beside position 030's tests of the module and
position 033's tests of a restore's artifacts, and extends the restore paragraph that PR12's
position 034 adds to the snapshot format guide. Position 233 changes the input gate that PR09's
position 014 adds: the chipset device unit, which changed a device's gate with the device's mutex
closed, as it changes the device's other states, now makes the gate's synchronous part under the
device's ordinary lock and awaits the rest without the device, since the vCPUs run through a gate
change. It changes the gate's methods of `ChangeDeviceState`, the erased chipset devices, and the
remote device proxy, which position 014 adds, and the virtio transports' forwarding of the gate,
which PR13's position 038 adds, and adds tests of the unit, both virtio transports, and the proxy.
Positions 234 and 235 change the snapshot boundary of PR17. Position 234 lets a VMM test be the
snapshot controller: it gives the snapshot-request port's handle in petri, which PR17's position 071
adds without a channel, and petri's launch of the VM worker, which position 067 gives the boundary's
parameters, a boundary's channels, which the petri microVM support of PR15's position 060 keeps for
the test, and adds such a test beside position 060's cold-boot test. Position 235 has the snapshot
quiesce and the boundary's release, which position 067 adds to the VM worker, refuse a VM outside a
boundary and the boundary of a quiesced VM, so that only position 067's rollback runs a quiesced VM
again. It extends the snapshot guide's paragraph on the RPCs that a boundary refuses, which position
071 adds, and adds tests beside position 234's. Position 236 has `StateUnits::stop` fail, rather
than panic, when a unit does not answer, leaving that unit in a new terminal state, and moves the
ordered stop of PR09's position 011's rollback into a module that the stop and the rollback share.
It changes the callers that the chain adds or changes: the restart and the teardown of the VM
worker, which position 011 changes; the snapshot quiesce and boundary of PR17's position 067, whose
quiesce error it extends, the boundary's request filter of position 068, and the VM controller's
capture of position 071; the restore readiness failure of PR12's position 035 and the gate timeout
of PR20's position 090; and, in OpenHCL, the stops of a pause, a save, a restart, and servicing,
beside the start that position 011 makes fallible. It makes `VmRpc::Pause` failable, as position 012
makes `VmRpc::Resume`, and has position 011's tests of a terminal unit check the stop's result.
Position 237 has a reset, a save, a restore, and the time advance of PR10's position 015 fail,
rather than panic, when a unit does not answer, leaving that unit in a terminal state of the
operation, which position 011's check of terminal units then refuses, and adds tests in a new file.
Position 238 adds the PCI counterparts of two MMIO tests to the virtio saved-state tests of PR13's
position 037, which positions 208 and 213 also extend, and corrects the module documentation of the
virtio-net saved-state tests that PR13's position 046 adds, which still described the deferred
restore of device-private state that position 037's rewrite for microsoft/nvx#1232 retired. Position
239 changes the direct-claimed scratch restore that position 161 adds to the paired scratch restore
of PR19's position 087, which hard-linked a checkpoint's scratch into a private directory before the
claim of position 088: the restore now opens the opened generation's scratch for writing through the
snapshot directory that position 033 opens, with a writable variant of that position's
directory-relative openers, which position 232 makes refuse a FIFO without waiting, and carries the
handle in the restore inputs of PR17's position 074 to the sandbox blocks of PR19's position 086,
whose storage builder attaches it through the part of position 087's block attachment that takes an
opened file. It removes position 161's link helper and the path-based opener of a given length that
position 087 adds and only that helper used, replaces position 161's test of the link, extends
position 232's test of the openers, adds tests beside those that position 142 adds to the microVM
restore module, and changes the CLI reference's sentence on direct-claimed scratch, which position
161 adds. Position 242 has the synchronized TSC set of the time ABI backends of positions 128, 129,
and 135 report the host instant from which the TSCs run, which the time ABI restore of position 134
passes, with position 243, to the time advance that PR10's position 015 adds:
`StateUnits::advance_time`, `StateUnit::advance_time`, and `VmTimeKeeper::advance` take that
instant, from which the keeper's next start runs VM time, and the chipset's state unit, whose time
advance position 015 adds, takes it too. Position 243 also has the tests of positions 011, 236, and
237 pass an instant, and extends the snapshot guide's paragraph on the VM-time cut that positions
151 and 196 write. Assigning the later commits therefore means revisiting those PRs, not only
appending new ones.

Position 172, by contrast, changes only `disk_blob`, which upstream main and the fork base have
alike and no other commit of the chain changes, so it can go upstream on its own. Position 177
likewise changes only `guestmem` and the virtio queue's check, and can go upstream on its own;
position 178 builds on the fork's virtio-mmio shared interrupt-status transport, MP-table loader,
and microVM worker. Position 149 changes only `cpu_profile`, the fork's CPU profile crate that the
time ABI series adds, one of its test helpers in `virt_mshv`, and the snapshot guide, so it builds
on that fork-only code. Position 154 changes only the CPU checks of the VM worker's time ABI and the
snapshot guide's description of them. Position 152 changes only the VM time keeper of `vmcore`,
whose `vmtime.rs` upstream main and the fork base have alike, and applies to upstream main as it is,
so it can go upstream on its own. Position 174 changes only how the microVM options bind their
network policy to the NIC's identity and the CLI and Consomme references, so it builds on the
fork-only commits of positions 169 and 173. Position 180 makes the change of microsoft/openvmm#4803
unchanged, and position 181 stages the Server 2025 image for the x64 Linux KVM job, which runs the
test, and excludes the test and its image from the x64 Linux MSHV job, where Server 2025's required
initial reset fails. microsoft/openvmm#4803 was closed without merging, so the chain keeps both
until upstream fixes the test's flakiness. Position 155 moves the time ABI to version 2: it changes
the time ABI backends of KVM, MSHV, and WHP, the portb device's time sample, the restore packet, and
the microVM worker's time-abi unit, all of which the time ABI series added, so it follows that
series upstream. Position 176 changes the tier check of position 173's restore that installs a new
policy and extends restore packet 5 of position 155, so it builds on those fork-only commits.
Position 182 changes only the microVM profile, CLI, and snapshot contract that PR16 and PR19 add,
and their Guide pages, so it does not apply to upstream main and follows those PRs upstream.
Positions 183 and 184 likewise change only the microVM profile, its worker's device configuration,
its snapshot contract, and its management RPC profile, which PR16-PR20, PR23, and PR25 add, and
their Guide pages: replaying them on `b2676222b` conflicts in all nine of their files, so they
follow those PRs upstream. Positions 185 and 186 change the MP-table loader of PR14 and the microVM
profile, worker, CLI, and snapshot contract of PR15-PR19 and PR23, and their Guide pages: replaying
them on `b2676222b` conflicts in 16 of their 17 files, so they follow those PRs upstream. The 17th,
the IOAPIC's `chipset` tests, which position 185 extends with a save and restore of the inputs past
the ISA IRQs, applies to upstream main without conflicts.

Positions 187-200 change only ABIs that the chain introduces: the microVM ABI and boot layout, the
time ABI, its time samples and restore packet, the snapshot manifest and machine contract, the saved
states of virtio-console, virtio-fs, and the RTC, the egress policy digest, the NIC's identity, the
virtio-fs slot, and the management RPC's microVM fields, so they follow the PRs that introduce those
ABIs upstream.

Position 201 changes only the microVM CLI's output drain and host consoles, the anonymous serial
pair that they use, and the CLI guide: replaying it on upstream main, `ba105dd7f`, conflicts in five
of its seven files, four of which upstream main lacks, so it follows those PRs upstream. Positions
202 and 203 change only the microVM portb device, its CLI and test harness, the VMM tests of PR15,
PR17, and PR25, and their Guide pages: replaying them on upstream main conflicts in all nine of
their files, six of which upstream main lacks, so they follow those PRs upstream.

Position 204 makes the secure file and directory creation of `pal`, which upstream main has, take
long Windows paths, and adds tests of it to `pal` and to the snapshot publication of
`openvmm_helpers`. microsoft/openvmm#4604, which landed PR04, also changes that `pal` file, and
PR11's positions 029-031, PR17's position 071, PR19's position 087, and positions 134, 142, and 161
that publication file, but position 204 changes nothing that they add. Replaying it on upstream main
conflicts only in the publication file, whose new tests use the paired scratch of PR19's position
087 and the exact-file RAM of PR11's position 031. Its `pal` change alone applies to upstream main
without conflicts, and `pal` builds there and passes its tests on Windows, so that change can go
upstream on its own.

Position 205 changes `state_unit`'s state change requests and their tests and, since the v5.0
reorganization folded into it the test that a slow input quiesce names its unit, the input quiesce
of PR09's position 014, its tests, and the CLI guide. Replaying it on upstream main conflicts in
four of its seven files: `state_unit`'s `lib.rs`, which PR09's positions 011, 013, and 014 and
PR10's position 015 also change, the input quiesce module and its tests, which position 014 adds and
upstream main lacks, and the CLI guide. So it follows PR09 upstream.

Position 206 leads the error with which the Consomme resolver, which upstream main has, reports a
port forward that it cannot bind with the stable code `E_PORT_FORWARD_BIND`, renames the helper with
which position 134's fatal error finds the first code of an error chain, and gives the code in the
Guide's sentences on the forwards of positions 169, 173, and 176 and on the time ABI codes of
position 142. It changes nothing that a mapped PR adds, but its resolver test builds a Consomme
handle with the fields that PR13's position 045 and positions 163 and 169 add. Replaying it on
upstream main conflicts in all four of its files, one of which, the verification module of position
134, upstream main lacks, so it follows those commits upstream.

Position 207 changes virtio-net's queue start and coordinator, which upstream main has, and changes
nothing that PR13's position 046 or PR21's position 100 add to the same files. Replaying it on
upstream main, `ba105dd7f`, conflicts only where its coordinator field joins the input-gate field of
position 046 and where its test module joins those of positions 046 and 100, and its test starts the
receive queue with position 046's harness, so it follows PR13 upstream unless those lines are
adapted. Position 208 changes only the virtio saved-state tests of PR13's position 037, which
upstream main lacks, so it follows PR13 upstream. Position 209's documentation of
`VirtioDevice::start_queue` applies to upstream main without conflicts but refers to the
`restore_device` that PR13's position 037 adds, and its Guide paragraphs conflict there, since they
extend those of PR12, PR18, PR20, and PR25, so it follows those PRs upstream.

Positions 210 and 211 change only the microVM snapshot capture, publication, and restore, their CLI,
block contracts, and manifest validation, and the Guide pages that describe them: replaying them on
upstream main conflicts in all 13 of their files, eight of which upstream main lacks, those that
PR11's position 030, PR15's position 059, PR17's positions 069, 071, and 074, PR19's position 086,
and PR25's position 119 add, so they follow those PRs upstream.

Positions 212 and 213 build on the chain too. Replaying position 212 on upstream main conflicts in
two of its three files: the device task, whose failed start PR13's position 037 adds, and the list
of the virtio test modules that the chain extends. Replaying position 213 on upstream main after it
conflicts in 15 of its 19 files, four of which upstream main lacks: `state_unit`'s start module and
its tests, which PR09's position 011 adds, the virtio saved-state tests of PR13's position 037, and
position 212's tests. Only `state_unit`'s requests, the virtio test transports, the chipset device
unit, and the device worker's request loop apply there. So both follow PR09 and PR13 upstream.

Position 214 builds on the chain too. Replaying it on upstream main conflicts in two of its three
files: the outcome report module, which PR24's position 117 adds and upstream main lacks, and
`pal`'s security module, whose encoding position 204 adds. Only its addition to `pal`'s file system
module applies there. So its report change follows PR24 upstream, and its `pal` change follows
position 204's.

Position 215 changes only the time ABI's measurement of the guest TSC offset, which reads on while
the offset's uncertainty, half its tightest bracket, is over 50 µs, until a read is within that
bound, and fails the start with `E_TSC_ANCHOR` if none is within it after 10 seconds. It changes
`virt`'s TSC module, which position 155 adds, the backend trait of position 127, and the MSHV and
WHP backends of positions 129 and 135, files that no other commits than positions 127-155 and 188
change, and the paragraph on time samples that position 155 adds to the Guide's snapshots page, in
the section that position 142 adds. It changes nothing that a mapped PR adds. Replaying it on
upstream main conflicts in all five of its files: upstream main lacks the four of the time ABI and
that section of the snapshots page, so it follows the time ABI series upstream.

Positions 216-218 change only the CPU profiles. Positions 216 and 217 clear AMX's features without
AMX's XSAVE states, and AVX10, in the derivation policy of position 133, which positions 145 and 148
extended. Position 218 pins `intel.graniterapids.v1` as positions 144, 145, and 148 pinned theirs,
in the generations, static data, fixtures, and tests of the `cpu_profile` crate, which position 133
adds and positions 141, 144, 145, 148, and 149 change. It also extends the test of the topology
leaves of the effective CPUID that position 141 adds, moves the tests of the fallback of positions
147 and 157 in `openvmm_core` and petri to another unserved CPU, adds the profile to the WHP
partition tests that position 148 last extended, and lists it on the Guide's snapshots page, in the
section that position 142 adds and position 215 last changed. They change nothing that a mapped PR
adds. Replaying them on upstream main conflicts in all nine of their files that existed before them:
upstream main lacks the `cpu_profile` crate, the time ABI, and that section of the snapshots page,
so they follow the time ABI series upstream.

Position 219 changes only the CPU profiles. It pins `amd.turin.v2`, a second revision of the Turin
profile of position 145, as position 148 pinned `amd.genoa.v2`, in the static data, fixtures, and
tests of the `cpu_profile` crate, which position 133 adds and positions 141, 144, 145, 148, 149, and
216-218 change. It also adds the revision to the WHP partition tests that position 218 last
extended, and lists it on the Guide's snapshots page, in the section that position 142 adds and
position 218 last changed. It changes nothing that a mapped PR adds. Replaying it on upstream main
conflicts in all seven of its files that existed before it, for the same reasons, so it follows the
time ABI series upstream.

Position 220 changes only the endpoints of the microVM consoles: their attachments, which PR13's
position 040 adds and no other commit changes; the microVM CLI's options, configuration, and module,
which PR15's position 059 adds, and its console module, which PR18's position 077 adds and PR23's
positions 112 and 113 and PR25's position 119 change; the management RPC's microVM module, which
position 119 adds; the snapshot contract, which PR17's position 069 adds; and the CLI guide's
`--virtio-console`, serial backend, and `--host-loopback` entries. Many later commits also change
the CLI's files, the management RPC's module, and the snapshot contract. Replaying it on upstream
main conflicts in all eight of its files, seven of which upstream main lacks, those that positions
040, 059, 069, 077, and 119 add, so it follows PR13, PR15, PR17, PR18, and PR25 upstream.

Position 221 changes only `chipset`'s microVM module, which PR15's positions 055 and 056, PR17's
position 066, PR19's position 087, PR20's positions 089, 090, 092, and 094, PR24's position 114, and
positions 132, 134, 142, 155, 176, 188, 190, and 203 also change, and the CLI guide. In the module
it changes only the write and the stop of the portb console, which position 055 adds and position
203 last changed, and position 203's test endpoint; in the guide, only the sentence on portb's
output in the `--snapshot-destination` entry, which PR25's position 120 adds. Replaying it on
upstream main conflicts in both of its files: upstream main lacks the module and that entry, so it
follows PR15 and PR25 upstream.

Position 222 changes only the restore of the microVM boot console's attachment: the console module,
which PR18's position 077 adds and PR23's positions 112 and 113, PR25's position 119, and position
220 change; the restore check, which PR17's position 074 adds; the microVM CLI's configuration and
module, which PR15's position 059 adds; the management RPC's microVM module, which position 119
adds; the resolver of the console attachments, which PR13's position 040 adds and no other commit
changes; and the `--virtio-console none` entry of the CLI guide and the restore section of the
snapshot guide. Replaying it on upstream main conflicts in all eight of its files: upstream main
lacks the six that positions 040, 059, 074, 077, and 119 add, and those sections of the guides, so
it follows PR13, PR15, PR16, PR17, PR18, PR23, and PR25 upstream.

Position 223 changes the management RPC's microVM module and protocol, which PR25's position 119
adds and the fork tail's position 124 and positions 156, 198, 220, and 222 change, and the RPC's
server module, which upstream main has and many chain commits, position 119 among them, change; the
microVM CLI's configuration, console, network, restore, and storage modules, which PR15's position
059, PR17's position 074, PR18's positions 077 and 078, and PR19's position 086 add, and many later
commits change; the CLI's options and entry point, the snapshot restore module that PR12's position
034 adds, the TTRPC VMM tests, whose microVM module position 119 adds, and the gRPC reference.
Replaying it on upstream main conflicts in 16 of its 17 files: upstream main lacks the microVM
modules, options, fields, and tests that those positions add, so it follows PR12, PR15, PR17, PR18,
PR19, PR21, PR22, and PR25 upstream.

Position 224 changes only the snapshot publication of `openvmm_helpers`, which PR11's positions
029-031, PR17's position 071, PR19's position 087, and positions 134, 142, 161, 204, and 210 also
change, the microVM controller's capture, which position 071 adds, and the sentences on a failed
publication that position 071 adds to the snapshot and snapshot format guides. In the publication
file it changes only the cleanup and rollback of the staging directory that positions 030 and 031
add, and their tests. Replaying it on upstream main conflicts in all four of its files: upstream
main lacks the controller, the staging of positions 030 and 031, and those sentences, so it follows
PR11 and PR17 upstream.

Positions 225 and 226 change only the snapshot manifest of `openvmm_helpers`, its format checks, and
its restore, which PR11's positions 029 and 030, PR12's position 033, PR17's positions 069 and 071,
PR19's positions 087 and 088, and positions 131, 142, 161, 179, 190, 199, 200, and 210 also change,
and the snapshot format guide's sections on the manifest. In those files they change only the
manifest's decoding and header check, which positions 029 and 033 add, and comment on the fields
that position 200 numbers, and they add tests that use the machine contract of PR17's position 069.
Replaying them on upstream main conflicts in all four of their files: upstream main lacks the format
magic and header check of position 029, the restore of position 033, the machine contract of
position 069, and the fields that position 200 numbers, so they follow PR11, PR12, and PR17
upstream.

Position 227 changes only the capture-tier check of the microVM snapshot module of
`openvmm_helpers`, which PR17's position 069 adds and many later commits change, and adds a test to
the snapshot restore of `openvmm_helpers`, which PR11's position 029, PR12's position 033, PR17's
position 071, PR19's positions 087 and 088, and positions 142, 161, 179, 190, 199, 210, and 225 also
change; the test uses the paired-scratch manifest of PR19's position 087 and the resume claim of
position 088. Replaying it on upstream main conflicts in both of its files: upstream main lacks the
microVM snapshot module of position 069 and the tiers and paired scratch that positions 087, 088,
and 161 add to the restore, so it follows PR17 and PR19 upstream.

Position 228 changes the restore packet of the time ABI series in `chipset_resources`, which
position 127 adds and positions 155, 176, and 188 change, and adds a test to the portb device that
serves it, which PR15's positions 055 and 056, PR17's position 066, PR19's position 087, PR20's
positions 089, 090, 092, and 094, PR24's position 114, and positions 132, 134, 142, 155, 176, 188,
190, 203, and 221 also change. It changes the microVM CLI of PR15's position 059, the restore
preparation of PR17's position 074, the management RPC's microVM conversion of position 223 and the
protocol of PR25's position 119, and their Guide pages. Replaying it on upstream main conflicts in
all nine of its files: upstream main lacks the time ABI series, the microVM CLI and restore of PR15
and PR17, and the conversion of position 223, so it follows that series, PR15, PR17, PR19, PR20, and
PR25 upstream.

Position 229 changes the snapshot publication of `openvmm_helpers`, which PR11's positions 029-031,
PR17's position 071, PR19's position 087, and positions 134, 142, 161, 204, 210, and 224 also
change, and the file system module that PR11's position 030 adds to it, which PR11's position 031,
PR12's position 033, PR19's positions 087 and 088, and positions 161 and 210 also change. It adds
renames to `pal`'s Unix file system module, which no commit of the chain changes, and to its Windows
one, which position 214 changes, and extends a sentence that position 071 adds to the snapshot
guide. Replaying it on upstream main conflicts in four of its six files: upstream main writes
snapshots without the staging and the file system module of position 030, and lacks the
extended-length encoding of position 214 and that sentence; only the Unix rename and `pal`'s
manifest apply there. So it follows PR11 and PR17 upstream, and its Windows rename follows position
214's `pal` change.

Position 230 changes only the platform tier's check of the microVM snapshot module of
`openvmm_helpers`, which PR17's position 069 adds, PR19's position 088 gives the platform tier, and
PR20's position 092, PR23's position 111, and positions 142 and 163 change, and adds tests to that
module, which use the paired-scratch manifest of PR19's position 087, the platform tier of position
088, and the worker's platform command line of PR20's position 092. It also extends the
`--snapshot-tier` entry that position 088 adds to the CLI reference of PR15's position 059.
Replaying it on upstream main conflicts in both of its files: upstream main lacks the microVM
snapshot module of position 069, the tiers of position 088, and the microVM CLI of position 059, so
it follows PR15, PR17, and PR19 upstream.

Position 231 changes only the memory-expansion ranges of the microVM snapshot module of
`openvmm_helpers`, which PR17's position 069 adds and PR20's position 094 extends with those ranges,
and adds a test to that module, which uses the expandable-memory manifest of position 094. Replaying
it on upstream main conflicts in its only file: upstream main lacks the microVM snapshot module of
position 069 and the memory expansion of position 094, so it follows PR17 and PR20 upstream.

Position 232 changes only the artifact openers of the snapshot file system module of
`openvmm_helpers`, which PR11's positions 030 and 031 add and PR12's position 033, PR19's positions
087 and 088, and positions 161, 210, and 229 also change, and adds tests to that module and to the
snapshot restore, the latter on the paired-scratch manifest of PR19's position 087 and the resume
claim of position 088. It also extends the restore paragraph that PR12's position 034 adds to the
snapshot format guide. Replaying it on upstream main conflicts in all three of its files: upstream
main lacks the file system module of position 030, the directory-relative restore of position 033,
and that paragraph, so it follows PR11, PR12, and PR19 upstream.

Position 233 changes only the input gate that PR09's position 014 adds to `vmcore`'s device state,
the chipset device unit, the erased chipset devices, and the remote device proxy, whose files PR10's
position 015 and position 213 also change, and the gate's forwarding through the virtio transports,
which PR13's positions 037 and 038 add and PR16's positions 061 and 063 and positions 178 and 213
also change. It adds tests of the unit, the transports, and the proxy, those of the transports in a
new file. Replaying it on upstream main conflicts in eight of its nine files, all but that new one,
which applies cleanly: upstream main lacks the input gate of position 014 and its forwarding by
position 038, so it follows PR09 and PR13 upstream.

Position 234 changes only petri's microVM support, which PR15's position 060 adds and PR17's
position 071 extends with the snapshot-request port's handle, and which PR16's position 062, PR20's
position 089, PR24's position 114, and positions 132, 142, 155, 157, 202, and 218 also change;
petri's runtime functions, which PR15's position 060 extends with the portb waits after its
positions 054 and 058, and position 202 changes; petri's launch of the VM worker, whose boundary
parameters PR17's position 067 adds, and which PR09's position 012, PR12's positions 034 and 035,
PR15's position 060, PR17's positions 070 and 073, PR20's positions 090 and 092, and positions 134
and 142 also change; petri's start of the VM, which only PR13's position 046 also changes; and the
microVM VMM tests of position 060, which PR17's position 075 and position 202 also change. Replaying
it on upstream main conflicts in four of its five files, all but petri's start of the VM, which
applies cleanly: upstream main lacks the petri microVM support of position 060, the boundary
parameters of position 067, and the port's handle of position 071, so it follows PR15 and PR17
upstream.

Position 235 changes only the snapshot quiesce and the boundary's release of the VM worker's microVM
module, which PR15's position 051 adds, PR17's position 067 gives the snapshot boundary, and 27
other positions also change; extends the snapshot guide's paragraph on the RPCs that a boundary
refuses, which PR17's position 071 adds; and adds VMM tests beside position 234's. Replaying it on
upstream main conflicts in all three of its files: upstream main lacks the microVM module of
position 051, the boundary of position 067, that paragraph, and the tests of positions 060 and 234,
so it follows PR15, PR17, and position 234 upstream.

The v7.0 reorganization folded v6.1 positions 234-236, which changed the start rollback and the
terminal state of PR09's position 011 and the time advance of PR10's position 015, into those
positions, and v6.1 positions 240-243 into the PR13, PR18, PR19, and PR20 positions that they
changed, as [The v7.0 reorganization](#the-v70-reorganization) records.

Positions 236 and 237 build on the chain too. Replaying position 236 on upstream main conflicts in
12 of its 20 files, since upstream main lacks, among others, the rollback and terminal states of
position 011, the fallible resume of position 012, and the microVM worker, snapshot boundary, and
controller of PR15 and PR17; its new module and tests and its changes to OpenHCL, the VMRS dump, the
ttrpc server, the VM controller's disk snapshot, and petri apply cleanly. microsoft/openvmm#4937
proposes its change of `StateUnits::stop` for upstream's code, on microsoft/openvmm#4872, so it
follows PR09 upstream. Replaying position 237 conflicts in two of its three files, all but its new
test file: upstream main lacks the terminal state of position 011, which `state_unit`'s `lib.rs`
extends, and the time advance of PR10's position 015, so it follows PR09 and PR10 upstream.

Position 238 changes only tests: the virtio saved-state tests that PR13's position 037 adds and
positions 208 and 213 also change, and the module documentation of the virtio-net saved-state tests
that PR13's position 046 adds. Replaying it on upstream main conflicts in both of its files:
upstream main lacks both test modules, so it follows PR13 upstream.

Position 239 changes the restore-side snapshot access of `openvmm_helpers`, which PR12's position
033 adds, and the microVM restore and storage builder of `openvmm_entry`, which PR17's position 074
and PR19's positions 086 and 087 add, and adds a read-write open to `pal`'s handle-relative file
access, which upstream main has. Replaying it on upstream main conflicts in 6 of its 7 files, all
but that `pal` module: upstream main lacks the snapshot modules of PR11 and PR12, the microVM
restore of PR17, the sandbox blocks of PR19, and the direct-claimed scratch of position 161, so it
follows position 161 upstream, and its `pal` open, which applies to upstream main as it is, has no
other user.

Position 240 changes only the time ABI's check of the clock tokens of a microVM's kernel command
line, which position 130 adds to the time ABI module of `virt` that position 127 adds and positions
134, 137, 139, 140, 143, 144, 147, 153, 155, and 188 also change: it splits the command line at
every byte that Linux's `isspace` takes for whitespace, the vertical tab and 0xa0 among them, rather
than at ASCII whitespace, and adds tests beside position 130's. It changes nothing that a mapped PR
adds. Replaying it on upstream main conflicts in its only file: upstream main lacks the time ABI, so
it follows the time ABI series upstream.

Position 241 raises `MAX_DOWNTIME_NS`, the time ABI's longest accepted downtime, which position 127
adds to the time ABI module of `virt`, from 30 days to 365 days, the fixed duration of 31,536,000
seconds, and the downtime bound in the snapshot Guide's time compatibility list, which position 142
adds. The checks that compare the anchor downtime of position 134 and the VM-time downtime of
positions 151 and 196 with it are unchanged. Its tests extend those of positions 127, 134, and 151,
in `virt` and in the snapshot time module of `openvmm_helpers`, which positions 131, 134, 137,
139-145, 151, 153, 155, 188, 190, and 196 also change. It changes nothing that a mapped PR adds.
Replaying it on upstream main conflicts in all five of its files: upstream main lacks the time ABI,
so it follows the time ABI series upstream.

Positions 242 and 243 change the synchronized TSC set of the time ABI backends of positions 128,
129, and 135, the time ABI restore of position 134, and the time advance of PR10's position 015,
with its callers and tests. Replaying them on upstream main conflicts in 15 of their 16 files, all
but the Guide's timekeeping page, whose description of `VmTimeKeeper` upstream main has alike:
upstream main lacks the time ABI of positions 127-135, the time advance of position 015 and its
tests, the tests of positions 236 and 237, and the snapshot guide's paragraph on the VM-time cut
that positions 142, 151, and 196 write, so they follow PR10 and the time ABI series upstream.

Positions 244-246 change how a restore runs a one-shot LAPIC timer, and change nothing that a mapped
PR adds. Position 244 moves the guest RAM of the real-mode KVM tests of `virt_kvm`'s ExtINT module,
which upstream main has and position 133 also changes, into a test module that position 245's tests
share. Replaying it on upstream main conflicts only in the module list of `virt_kvm`'s x86-64
module, which PR10's positions 021 and 024, PR17's position 072, and positions 128, 133, 139, 142,
144, and 145 also change, where its test module joins the modules of positions 128 and 133; its new
module and its change of the ExtINT tests apply there.

Position 245 changes the KVM backend's `set_apic`, in a file that upstream main has and PR10's
position 024 and position 142 also change, and the Guide's timekeeping reference, which position 243
also changes. Replaying it on upstream main applies without conflicts, but its KVM tests use
position 244's guest RAM, position 134's LAPIC timer advance, position 127's TSC target, position
155's host TSC bracket, and the TSC accessors that PR10's position 024 adds to the `kvm` crate,
which upstream main lacks, so it follows them upstream.

Position 246 changes the time ABI restore of the LAPIC timers that position 134 adds to `virt`'s VP
snapshot module, the VP set, the partition unit, and the VM worker, which positions 141, 142, 150,
153, and 243 also change, and the snapshot guide's sentence on it that position 142 adds. It sets
each timer again after the synchronized TSC set, so that the timer runs as the TSCs do, as position
243's VM time does. It adds a LAPIC tick conversion to the rate module of position 127 and extends
position 245's KVM tests. Replaying it on upstream main conflicts in all 12 of its files, seven of
which upstream main lacks, those that PR10's positions 022 and 027, PR12's position 034, and
positions 127, 130, 134, and 244 add, so it follows the time ABI series upstream.

Positions 247 and 248 have the KVM backend tell a one-shot LAPIC timer whose interrupt the guest
took apart from one whose expiry KVM holds when it saves a VP, and change nothing that a mapped PR
adds. Position 247 adds a call to the `kvm` crate's VP runner that has KVM deliver the interrupts
that it holds without entering the guest, in a file that upstream main has and that PR10's position
024 and positions 128, 133, 142, and 143 also change, none of them in the runner's `KVM_RUN` and
exit decoding, which it changes and upstream main has. Replaying it on upstream main applies without
conflicts, and the `kvm` crate type-checks there, so it can go upstream on its own.

Position 248 adds a LAPIC timer module to `virt_kvm`'s x86-64 backend, which the VP run loop, the
partition, and the VP state's `apic` and `set_apic` use, in files that upstream main has and that
PR10's positions 021 and 024, PR17's position 072, and positions 128, 133, 139, 142, 144, 145, 243,
244, 245, and 246 also change, and rewrites the paragraph of the Guide's timekeeping reference that
position 245 adds. Replaying it on upstream main conflicts in that paragraph, in the module list of
the x86-64 module, and in the VP state module, whose KVM LAPIC page from position 245 its KVM tests
use, with position 244's guest RAM and position 247's delivery, so it follows positions 244, 245,
and 247 upstream. The v7.0 reorganization folded v6.1 position 254, which built its KVM tests for
musl, for which the `libc` crate defines no `SIGEV_THREAD_ID`, into position 248.

Position 249 changes only the time ABI's logs of how long the phases of a restore take: the restore
clock event of the VM worker's time ABI, which position 139 adds and position 141 gives its
concurrent LAPIC and VM-time phases, and the synchronized TSC sets of the MSHV and WHP backends of
positions 129 and 135. It computes each of their phases that is the difference of two readings with
a helper that it adds to `virt`'s time ABI module of position 127, which gives 0 where the host's
monotonic clock stepped back past the phase's start and the subtraction panicked, in files that
positions 130, 133, 134, 137, 140, 142-145, 147, 148, 151, 153-155, 188, 196, 215, 218, 219,
240-243, and 246 also change. It changes nothing that a mapped PR adds. Replaying it on upstream
main conflicts in all four of its files: upstream main lacks the time ABI, so it follows the time
ABI series upstream.

Position 250 makes the PIT's count writes in the low-then-high access mode take each count's low
byte first, in `Timer::write`, code that upstream main has, and adds three tests, in a file that
PR10's position 020 and position 134 also change, neither of them in the code that it changes. It
changes nothing that a mapped PR adds. Replaying it on upstream main applies without conflicts, and
`chipset` type-checks there with its tests, which pass, so it can go upstream on its own.

Position 251 keeps the part of the PIT's last tick time below VM time's 100 ns resolution, which
`PitDevice::evaluate` dropped and counted again, in the PIT's evaluation, wakeup, reset, and saved
state, code that upstream main has, and adds six tests that extend the test helper of position 250,
in a file that PR10's position 020 and positions 134 and 250 also change. It changes none of the
code of positions 020 and 134, and nothing that a mapped PR adds. Replaying it on upstream main
conflicts in that file, which lacks position 250's helper; after position 250, it conflicts only
where its tests follow position 134's test, so it follows position 250 upstream.

Position 252 makes `PitDevice::restore` reject a saved control word with RW = 0, the counter latch
command, before it changes any state, in code that upstream main has, and adds two tests on position
250's test helper, in a file that PR10's position 020 and positions 134, 250, and 251 also change.
It changes none of the code of positions 020 and 134, and nothing that a mapped PR adds. Replaying
it on upstream main conflicts in that file where it adds its error, its tests' import, and its tests
next to code of positions 134, 250, and 251, which upstream main lacks; its check applies there. It
follows position 251 upstream. The v7.0 reorganization folded v6.1 position 259, which kept its
tests from creating PITs for each case and so exhausting Linux's default limit of open files, into
position 252.

Position 253 makes the RTC's `io_read` zero-extend a read of port `0x70` or `0x71` that is wider
than a byte, whose upper bytes kept what the backend's buffer held, in code that upstream main has,
and adds a test, in a file that PR10's position 018, PR15's position 052, and position 195 also
change, none of them in the code that it changes. It changes nothing that a mapped PR adds.
Replaying it on upstream main applies without conflicts, and `chipset` type-checks there with its
tests, which pass, so it can go upstream on its own.

Position 254 changes only `microvm_options`, which position 223 adds to the CLI's options module, so
that it builds a management RPC request's options on a thread with a 4 MiB stack on every host
rather than only on Windows: the RPC calls it on its worker thread, whose 2 MiB stack clap's
construction of the options overflows in debug builds. It moves that thread into a helper, which
`on_big_stack`, which upstream main has, also uses on Windows. It changes nothing that a mapped PR
adds. Replaying it on upstream main conflicts in its only file, which lacks position 223's
`microvm_options`, so it follows position 223 upstream.

Position 255 makes the IOAPIC's `mmio_read` zero-extend a read of its index or data register that is
wider than 4 bytes, which panicked the vCPU thread, and so OpenVMM, because it copied the register's
low bytes into the whole buffer, in code that upstream main has, and adds a test to the device's
tests, in a file whose saved-state tests position 185 also changes, none of the code that it
changes. It changes nothing that a mapped PR adds. Replaying it on upstream main applies without
conflicts, and `chipset` type-checks there with its tests, which pass, so it can go upstream on its
own.

Position 256 makes the IOAPIC's `restore` reject a saved id that the ID register's 4-bit id field
cannot hold, which made the next read of that register panic the vCPU thread, and so OpenVMM,
because the field's setter panics on a larger value, in code that upstream main has: it adds
`IoApicId::MAX_ID` and a check that runs before `restore` changes the device. It adds tests to the
device's saved-state tests, which position 185 also extends, without changing any of its code, and
changes nothing that a mapped PR adds. Replaying it on upstream main conflicts only in those tests,
at their imports and at their end, where position 185 adds an import and a test, and its first test
reads the routes that position 185 makes the tests' routing record, so its tests follow position 185
upstream, and its change to the device itself applies without conflicts.

Position 257 makes the restore of a host CPU profile refuse a profile that the derivation policy
would change, which let a snapshot's own copy of its profile give the guest the VMX or SVM that
every derived profile withholds. In the `cpu_profile` crate, which position 133 adds and positions
140, 141, 143-145, 147-149, 153, and 216-219 change, it adds `policy_violations`, which applies to
the profile the derivation policy, the stateless-feature clears, and the XSAVE allowances that
position 133 adds and positions 145, 148, 216, and 217 extend, and has `host_profile_for_restore`,
which position 144 adds, refuse a profile that differs. It extends the host profile tests of the
restore preflight and of the worker's selection of the CPU profile, which position 144 adds to the
snapshot time module of `openvmm_helpers` and the time ABI module of `openvmm_core`, which positions
131 and 130 add, without changing their code. It also describes the refusal on the Guide's snapshots
page, in the paragraph on a restore's CPU profile requirements, which positions 143, 144, 147, and
149 write in the section that position 142 adds, and in the limit on CPU profiles that positions
142, 143, 147, and 190 write. It changes nothing that a mapped PR adds. Replaying it on upstream
main conflicts in all seven of its files: upstream main lacks the `cpu_profile` crate, the time
ABI's modules of `openvmm_helpers` and `openvmm_core`, and that section of the snapshots page, so it
follows the time ABI series upstream.

Position 258 makes the lengths that `virt`'s `XsaveCapabilities` computes for the XSAVE areas that a
CPUID describes, `standard_len_for` and `compact_len_for`, saturate at `u32::MAX`, since a leaf 0xd
whose components end past 2^32 bytes overflowed them and panicked a debug build, in code that
upstream main has and no commit of the chain changes. It adds two tests, in a file that PR10's
positions 021 and 023 and position 142 also change, none of them in the code that it changes. It
changes nothing that a mapped PR adds. Replaying it on upstream main applies without conflicts, and
`virt` type-checks there with its tests, which pass, so it can go upstream on its own.

Position 259 has the KVM backend refuse, as it builds a partition, an XSAVE layout that its VP state
code cannot hold, which a restored CPU profile can describe and which only the profile support check
refuses, after the build: a standard length past the 4096 bytes of the buffer of `KVM_GET_XSAVE`, or
a compact length that is short of its components or not in whole words. In `virt_kvm`'s x86-64
module it adds `partition_capabilities`, which replaces the derivation of the capabilities that
position 128 writes in `build`, from the time ABI's CPUID through position 127's identity leaves,
and `check_xsave_layout`, which refuses a layout with `XSaveLengthTooLarge`, a new error of the
capabilities of `virt`'s x86 module, or with the existing `XSaveLengthTooSmall`. It also gives
`KvmError::Capabilities` its cause as its source, and adds tests of the capabilities of every
profile that position 133 pins, from its effective CPUID and position 143's backend CPUID, and of
layouts that no CPU describes, which rely on position 258's saturated lengths. It changes files that
PR10's positions 021, 023, and 024, PR17's position 072, and positions 128, 133, 139, 142, 144, 145,
244, 248, and 258 also change, only position 128's derivation in the code that it changes, and
nothing that a mapped PR adds. Replaying it on upstream main conflicts only where it replaces
position 128's derivation in `build`, and its other changes apply there, but its tests use what
positions 127, 133, and 143 add, which upstream main lacks, so it follows positions 127, 128, 133,
143, and 258 upstream.

Position 260 makes the KVM backend's `set_registers` refuse a restored EFER that has SVME set when
the partition's CPUID has no SVM, before it changes any register, since `KVM_SET_SREGS` took any
EFER before Linux 7.3, 6.18.52, and 6.12.112 and so gave such a guest SVM, in code that upstream
main has and no commit of the chain changes. Nothing else sets EFER from a restored state. It adds
`check_restored_efer` and a unit test to `virt_kvm`'s VP state module, a KVM-only form of
`backend_test!` and a `registers` module of contract tests to `virt_backend_tests`, which upstream
main has, and a sentence to the Guide's page on VMM tests. It changes files that PR01's position
002, PR05's positions 007 and 008, PR10's position 024, and positions 142, 245, 246, and 248 also
change, none of them in `set_registers`, the macro, or the Guide's section on the backend contract
tests, and adds only to the imports that positions 024 and 142 change and to the `tests` module that
position 245 adds. It changes nothing that a mapped PR adds. Replaying it on upstream main conflicts
only at the end of the VP state module, where its unit test extends the `tests` module that position
245 adds, so that test follows position 245 upstream, and the rest of the commit applies there
without conflicts.

Position 261 makes `VirtualMsrs`, the virtual MSRs of a VP, hold IA32_SYSENTER_ESP in `sysenter_esp`
and IA32_SYSENTER_EIP in `sysenter_eip`. Its `get_values` and `set_values` paired the registers with
the fields in the opposite order of `names`, so that on KVM, MSHV, and WHP, which save the type
through them, each field held the other's register, while OpenHCL's TDX and SNP processors fill the
fields by name. It pairs them in the order of `names` and swaps the two fields and their numbers, 3
and 4, so that the type encodes the same bytes as before, and it documents on `HvRegisterState` that
`names`, `get_values`, and `set_values` list the same registers in the same order, in code that
upstream main has and no commit of the chain changes. It adds three tests at the end of the VP state
module's `tests` module, which position 134 adds and position 246 extends. It changes the VP state
module, which PR10's positions 022 and 024 and positions 134, 142, and 246 also change, none of them
in `VirtualMsrs`, and nothing that a mapped PR adds. Replaying it on upstream main conflicts only
where its tests extend that `tests` module, which upstream main lacks, so they follow positions 134
and 246 upstream, and the rest of the commit applies there without conflicts.

Position 262 makes the KVM backend's `set_xsave` refuse a restored XSAVE area whose XSTATE_BV
selects a component that the partition's caps lack. `KVM_SET_XSAVE` took such a component when the
host's KVM supports it, and the next save dropped it, so that the restored state was not the
snapshot's, in code that upstream main has and no commit of the chain changes. It restores through
`xsave::restore`, in a new module of `virt_kvm`'s x86-64 module, which checks XSTATE_BV against the
caps' features, with x87 and SSE, before it writes anything, and adds five unit tests and an ignored
test that restores each component into a real VP. It changes files that PR10's positions 021 and
024, PR17's position 072, and positions 128, 133, 139, 142, 144, 145, 244-246, 248, 259, and 260
also change, none of them in `set_xsave`, and nothing that a mapped PR adds. Replaying it on
upstream main applies without conflicts, and `virt_kvm` type-checks there with its tests, which
pass, the ignored one included, so it can go upstream on its own.

Position 263 makes the KVM backend's `set_apic` reject a LAPIC page whose unmasked timer entry has a
delivery mode other than Fixed, which KVM would fire as an INIT, a startup IPI, an NMI, or an SMI,
or as a wake of a halted VP, none of which the saved state holds, in code that upstream main has. It
adds `check_restorable_apic_timer`, which `set_apic` calls first, a unit test, a KVM-only contract
test of `virt_backend_tests` in a module of its own, and a paragraph and a sentence of the Guide. It
changes files that PR01's position 002, PR05's positions 007 and 008, PR10's positions 018 and 024,
and positions 142, 243, 245, 246, 248, 260, and 262 also change, and adds to no line that a mapped
PR adds. Replaying it on upstream main conflicts in `vp_state.rs`, at the start of `set_apic`, whose
timer reset position 248 adds, and from the end of the `AccessVpState` implementation on, which
positions 245, 246, 248, and 260 add; in the Guide's KVM timekeeping section, which positions 245
and 248 extend; in the Guide's page of the contract tests, whose `kvm_only` sentence is position
260's; and in the contract tests' `main.rs`, beside the attribute that position 008 drops. So it
follows positions 008, 245, 246, 248, and 260 upstream.

Position 264 lets a backend refuse to save a state that it cannot hold. The state traits that
`virt`'s `state_trait!` generates gain `check_saveable`, whose default accepts every state and which
`save_all` calls before it reads any element, so that a refusal reads nothing, in code that upstream
main has and no commit of the chain changes. It adds two tests, in a `tests` module of its own at
the end of the file. It changes `state.rs`, which only position 261 also changes, in the
documentation of `HvRegisterState`, and nothing that a mapped PR adds. Replaying it on upstream main
applies without conflicts, and `virt` type-checks there with its two tests, which pass, so it can go
upstream on its own.

Position 265 has the KVM backend refuse to save a vCPU of an AMD or Hygon CPU whose global interrupt
flag (GIF) is clear, or whose GIF KVM cannot report, since the saved state has no GIF and a restore
would run the vCPU with it set, in code that upstream main has: a KVM that uses virtual GIF and
predates Linux 7.1 lets a guest without SVM clear it with `CLGI`. `KvmVpStateAccess` overrides the
check that position 264 adds. The `kvm` crate gains the `KVM_GET_NESTED_STATE` ioctl,
`Processor::nested_state_header`, and `NestedStateHeader`, which reads GIF from the nested state's
flags; `virt_kvm`'s x86-64 module gains a `gif` module with the check, its unit tests, and two
ignored tests that need `/dev/kvm`; `KvmError` gains `GifClear`; and the Guide's snapshots page
gains a paragraph on the refusal. It changes files that PR10's positions 021 and 024, PR11's
position 030, PR12's positions 033-035, PR13's positions 041 and 046, PR17's positions 071, 072, and
074, PR18's positions 077, 079, 084, and 085, PR19's positions 087 and 088, PR20's positions 089,
090, 092, and 094, PR23's positions 108 and 113, PR24's position 118, and 58 positions that no PR
has yet also change, and it adds to no line that a mapped PR adds. Replaying it on upstream main
conflicts in four of its files, where it adds beside what upstream main lacks: in the Guide's
snapshots page, its paragraph sits in the saving section that position 071 writes; in
`vm/kvm/src/lib.rs`, its test follows the MSR filter tests that position 128 adds; in `virt_kvm`'s
x86-64 module, `mod gif;` sits between `mod fingerprint;` of position 133 and `mod lapic_timer;` of
position 248; and in its `lib.rs`, `GifClear` follows the time ABI's variants of position 128. Its
ignored test that runs `CLGI` also uses the guest RAM that position 244 shares among the real-mode
tests. So it follows positions 071, 128, 133, 244, and 248 upstream, after position 264.

Position 266 adds a KVM-only contract test, `save::kvm::saves_a_new_vp`, in a module of its own of
`virt_backend_tests`. It requires that a new VP's `check_saveable` and `save_all` succeed, which on
an AMD host shows that position 265's read of the nested state accepts a new vCPU. It registers with
the `kvm_only` form of `backend_test!` that position 260 adds and calls the check that position 264
adds. It changes the contract tests' `main.rs`, which PR05's position 008 and positions 260 and 263
also change, by the `mod` line of its module, and nothing that a mapped PR adds. Replaying it on
upstream main conflicts there, beside the `registers` module that position 260 adds, and its new
file needs that form of `backend_test!`, so it follows positions 260, 264, and 265 upstream.

Position 267 makes the KVM backend's `set_activity` refuse an injected soft exception, #BP or #OF,
which `KVM_SET_VCPU_EVENTS` accepts although delivering one takes the length of the instruction that
raised it, which the ioctl does not carry, so that `KVM_GET_VCPU_EVENTS` never reports one as
injected and a restore lost it or, in a debug build, panicked in the read-back check of
`restore_all`, in code that upstream main has and no commit of the chain changes. It adds
`check_restorable_interruption`, which `set_activity` calls beside its refusal of a pending ExtINT,
a unit test, and a KVM-only contract test, `activity::kvm::refuses_an_injected_soft_exception`, in a
module of its own of `virt_backend_tests`, which registers with the `kvm_only` form of
`backend_test!` that position 260 adds. It changes files that PR05's position 008, PR10's position
024, and positions 142, 245, 246, 248, 260, 262, 263, 265, and 266 also change, and adds to no line
that a mapped PR adds. Replaying it on upstream main conflicts in `vp_state.rs`, from the end of the
`AccessVpState` implementation on, which positions 245, 246, 248, 260, and 263 add, where its
function and its unit test sit, and in the contract tests' `main.rs`, beside the `apic` module that
position 263 adds and the attribute that position 008 drops; its call in `set_activity` applies
there without conflicts. So it follows positions 008, 245, 246, 248, 260, and 263 upstream.

Position 268 lets a backend report that a stopped VP holds an INIT or a SIPI that it has not
accepted yet, so that a snapshot capture can refuse it. KVM holds such an event until the VP runs,
and accepts it when it reads the VP's MP state, which the activity element of a save does after the
registers, so a save of a VP that another VP sent an event after it stopped held registers from
before the event and the rest of its state from after it, in code that upstream main has and no
commit of the chain changes. `Processor` gains `has_pending_init_sipi`, whose default reports none
and which the `openvmm_core` VP wrapper forwards; the KVM backend reports a latched INIT, which
`KVM_GET_VCPU_EVENTS` returns without accepting it, and a pending SIPI, which `KVM_GET_MP_STATE`
accepts, as `KVM_RUN` would, so that the move of the VP's registers shows it. It adds a `virt_kvm`
module with the detection and five ignored tests that need `/dev/kvm`, and two KVM-only contract
tests, `init_sipi::kvm::reports_an_init_that_it_has_not_accepted` and
`reports_a_sipi_that_a_waiting_vp_has_not_accepted`, in a module of its own of `virt_backend_tests`,
which register with the `kvm_only` form of `backend_test!` that position 260 adds. It changes files
that PR05's position 008, PR10's positions 021, 023, and 024, PR12's position 036, PR15's position
057, PR17's positions 072 and 073, and 16 positions that no PR has yet also change, and adds to no
line that a mapped PR adds. Replaying it on upstream main conflicts in `virt_kvm`'s x86-64 module,
where `mod init_sipi;` sits in the list between `mod gif;` of position 265 and `mod lapic_timer;` of
position 248, which upstream main lacks along with `mod fingerprint;` of position 133; its ignored
tests also use the guest RAM that position 244 shares among the real-mode tests, and its contract
tests need that form of `backend_test!`. So it follows positions 133, 244, 248, 260, and 265
upstream.

Position 269 has a guest-requested microVM capture refuse to save while a VP holds an INIT or a SIPI
that it has not accepted. After the capture has stopped every state unit, and before the time ABI's
LAPIC timer checks that position 134 adds read any VP, `quiesce_for_snapshot` asks every VP, through
the VP set, whether it holds one, and a VP that does fails the capture with a rollback-safe error
that names it, so that the guest continues and can request the capture again; a backend that reports
none, which is every backend but KVM, passes. It uses position 268's method and follows the shape of
position 134's LAPIC timer checks: `PartitionUnit` gains `check_no_pending_init_sipi`, the VP set a
fan-out in a module of its own, and `SnapshotRequest`, `StateEvent`, and `ControlVp` a variant or a
method each. It adds three unit tests of the fan-out and a paragraph to the Guide's snapshots page,
after the one that position 265 adds. It changes files that PR10's position 027, PR11's position
030, PR12's positions 033-035, PR13's positions 041 and 046, PR15's positions 051, 057, and 058,
PR16's positions 062 and 064, PR17's positions 065, 067, 068, 070-072, and 074, PR18's positions
077-079, 082, 084, and 085, PR19's positions 086-088, PR20's positions 089-094, PR23's positions
108, 110, and 113, PR24's position 118, and 56 positions that no PR has yet also change, and adds to
no line that a mapped PR adds; the two lines of `snapshot.rs`'s module documentation that it
rewrites are position 142's. Replaying it on upstream main conflicts in all four of those files.
`microvm.rs` and `snapshot.rs` do not exist there, since positions 051 and 027 add them: its call
sits in `quiesce_for_snapshot` of position 067, after the stop that position 236 rewrites and before
the time ABI's checks of position 142, and its variant, method, and handler in `snapshot.rs` sit
before the LAPIC timer checks of position 134. In `vp_set.rs`, its module sits between the modules
of positions 065 and 091, its method, variant, and handler beside those checks, and its tests after
the halt-latch test of position 150. In the Guide's snapshots page, its paragraph sits after the GIF
paragraph of position 265, in the block that positions 071 and 235 write. So it follows positions
027, 051, 065, 067, 071, 091, 134, 142, 150, 235, 236, 265, and 268 upstream.

Position 270 makes the IPv4 echo relay of Consomme return to the guest the echo replies that answer
its requests, on Linux hosts, where it returned none, and only those, on every host. A Linux host's
unprivileged datagram ICMP socket delivers the ICMP message without its IPv4 header, and with the
identifier of its own binding, but `poll_conn` read every delivery as a raw IPv4 packet, and a
Windows host's raw socket delivers every ICMP message that reaches the host, all of which the relay
forwarded, in code that upstream main has and no commit of the chain changes. A new `icmp/echo.rs`
holds `PendingEchoes`, the table of the echo requests of a flow that await replies: it gives each
request a sequence number of its own on the host, and each request that leaves through a raw socket
the flow's own identifier, both random to start with, and turns what a socket delivers into the
reply that the guest receives. `poll_conn` and `handle_icmp` use it, and `handle_icmp` drops the
guest's other ICMP messages with the new `DropReason::UnsupportedIcmpv4`, which `net_consomme`'s
`process_tx` counts as an unknown transmit. It adds 18 unit tests in a module of their own, one of
which sends an echo through a real host socket, adapts the flow-limit and driver-refresh tests of
`icmp.rs`, and rewrites the Guide's ICMP section. It changes files that PR13's position 045, PR21's
positions 098 and 101, and positions 163, 169, and 174 also change, none of them in the ICMP relay
or the Guide's ICMP section, and adds to no line that a mapped PR adds. Replaying it on upstream
main applies without conflicts, and `consomme` and `net_consomme` type-check there with their tests,
which pass, so it can go upstream on its own.

Position 271 makes the listener of a published port in Consomme accept every connection that waits
for it. `poll_tcp` accepts one connection per poll, and a successful accept leaves the listener's
readiness set and registers no wakeup for the next one, so a connection that waited behind the
accepted one, or arrived after it, stayed in the host's accept queue until something unrelated woke
the endpoint, such as the guest's announcement of its listeners, which repeats every two seconds. A
client that the listener refuses, whose connect the host had completed, hung for up to that long
before its reset. `poll_tcp` now wakes the endpoint after each accept, and the wake that the
flow-limit path of upstream's microsoft/openvmm#4600 issued for the same reason becomes redundant
and goes. It adds one unit test, which uses `InboundAdmission`, `bind_tcp_port_with_admission`, and
the `connections_refused` counter that position 169 adds. It changes files that PR13's position 045
and positions 163 and 169 also change, and changes no line that a mapped PR adds. Replaying it on
upstream main conflicts in `tcp.rs` and `tcp/tests.rs`: its wake sits beside the admission check
that position 169 adds, and its test follows the tests of position 169. So it follows position 169
upstream, and cannot go on its own.

Position 272 makes a snapshot keep the guest's `IA32_SPEC_CTRL`, which no element of a VP's saved
state held, so that a restore into new vCPUs left it at 0 on KVM, MSHV, and WHP alike, and a guest
that had enabled IBRS, STIBP, or SSBD ran with the controls clear until it next wrote the MSR, in
code that upstream main has and no commit of the chain changes. `SpecCtrl`, field 16 of
`VpSavedState`, is a new optional element, and `X86PartitionCapabilities::spec_ctrl`, which
`from_cpuid` derives from the guest's CPUID with `enumerates_spec_ctrl`, gates it: Intel's IBRS,
STIBP, or SSBD in `CPUID.(7,0):EDX`, or AMD's in `CPUID.0x80000008:EBX`, which of the pinned CPU
profiles only the Skylake-SP and Alder Lake ones enumerate. KVM, MSHV, and WHP read and write the
MSR as a register. KVM clears the capability on a host whose `KVM_GET_MSR_INDEX_LIST` lacks the MSR
and for an SNP guest, MSHV for an isolated one, and OpenHCL does not save the MSR. The element needs
no version, as upstream's microsoft/openvmm#4704 needed none for the TSC-deadline element: a saved
state without it restores the reset value, as before, and an earlier build ignores it. The commit
adds six unit tests, a comment above `state_trait!` that lists what the next optional element needs,
a section of the Guide's snapshot format, and a paragraph of its snapshots page. It changes files
that PR05's positions 007-009, PR10's positions 021-026, PR11's positions 029 and 030, PR12's
positions 033-035, PR13's positions 041 and 046, PR17's positions 071, 072, and 074, PR18's
positions 077, 079, and 083-085, PR19's positions 087 and 088, PR20's positions 089, 090, 092, and
094, PR21's position 101, PR23's positions 108 and 113, PR24's position 118, and 74 positions that
no PR has yet also change, and it changes no line that a mapped PR adds: the one block of
`from_cpuid` that it re-indents is upstream's. Replaying it on upstream main conflicts in six of its
files, where it adds beside what upstream main lacks: in the Guide's snapshots page, its paragraph
sits in the time and CPU compatibility section that position 142 writes, between lines of positions
154 and 188; in `state.rs` and `x86/vp.rs`, its tests sit at the end of the test modules that
positions 264 and 134 add, after the tests of position 261 in the latter; in `virt_kvm`'s x86-64
module, `msr_index_list` sits beside `cpu_surface` of position 139, and its gate in `build` sits in
the capabilities code that positions 024, 128, and 139 write; in `virt_mshv`'s x86-64 module,
`spec_ctrl: false` sits beside the TSC-deadline comment of position 022; and `finalize.rs` does not
exist there, since position 008 adds it, and positions 022 and 025 extend it where the gate sits. So
it follows positions 008, 022, 024, 025, 128, 134, 139, 142, 154, 188, 261, and 264 upstream.

Position 273 adds a contract test, `speculation::spec_ctrl_survives_a_save_and_restore`, in a module
of its own of `virt_backend_tests`, with the capability probe `Capabilities::spec_ctrl` that its
`requires` clause reads. The test enables the controls that the guest-free fixture's VP takes, saves
the VP through the mesh encoding, clears the MSR, restores the saved state, and requires the
controls back, so it fails for a saved state that leaves the element out. It reports ignored where
the fixture's default CPUID enumerates no control, as on the Azure virtual machines of NVX's hosts,
whose hypervisors hide the MSR, and the Guide's description of the suite says so. It changes files
that PR01's position 002, PR05's positions 007 and 008, and 5 positions that no PR has yet also
change, and adds to no line that a mapped PR adds. Replaying it on upstream main applies without
conflicts, but it uses the capability and the accessors that position 272 adds, so it follows
position 272 upstream, and cannot go on its own.

Position 274 adds a WHP hardware test, ignored without WHP like its neighbors, to the `whp_tests`
module of the time ABI backend:
`host_profile_partition_takes_spec_ctrl_where_it_enumerates_a_control`. It builds the time ABI
partition of the host's CPU profile, requires WHP's CPUID to enumerate the SPEC_CTRL controls as the
profile does, and, where it does, reads 0 from the VP's `WHvX64RegisterSpecCtrl` and sets and reads
back IBRS, STIBP, SSBD, all three, and 0; elsewhere it skips. WHP's default partition does not
present the controls, so only a partition with a profile's processor features holds the MSR. It
changes `time_abi.rs`, which 17 positions that no PR has yet also change, and adds to no line that a
mapped PR adds. Replaying it on upstream main conflicts in `time_abi.rs`, which does not exist
there, since position 135 adds it, with the `Partition` fixture and `partition` helper of positions
137, 139, 143, and 144 that its test uses; the test also calls `enumerates_spec_ctrl` of position
272. So it follows positions 135, 137, 139, 143, 144, and 272 upstream.

Position 275 builds position 272's unit test of the SPEC_CTRL capability of every pinned profile,
`spec_ctrl_state_is_present_for_the_profiles_that_enumerate_a_control`, only for x86-64 guests. The
test calls `time_abi::cpuid` and `time_abi::identity`, which exist only for x86-64 guests, but
`virt`'s `x86` module and its tests build for every guest architecture, so the unit tests of `virt`
did not compile, and clippy failed, for the aarch64 guests, such as `aarch64-apple-darwin` and
`aarch64-unknown-linux-gnu`, which the CI jobs and the development hosts of x86-64 do not build. It
adds a `cfg` attribute and a comment to that test, in `vmm_core/virt/src/x86/mod.rs`, which PR10's
positions 021 and 023 and positions 142, 258, 259, and 272 also change, and it adds to no line that
a mapped PR adds. It fixes position 272, which no PR maps yet, so it is not a fixup of a PR yet, and
folds into position 272 when that position goes into one. Replaying it on upstream main conflicts in
that file, since the test that it changes follows the other tests of position 272. So it follows
position 272, and the positions that position 272 follows, upstream.

Position 276 hides AMD's legacy performance counters, `PerfEvtSel0-3` and `PerfCtr0-3` (`0xc0010000`
through `0xc0010007`), from a time ABI guest of an AMD CPU profile on KVM. The guest has no PMU, but
a KVM whose virtual PMU is on serves the counters to every AMD guest and keeps their state in the
vCPU, where no element of the saved state reaches it, so a restore dropped what a guest had
programmed, and what the guest saw depended on the host. The time ABI's MSR filter denies the eight
registers in that case, and `KvmTimeAbi::hides_msr` answers them with #GP, as for the MSRs that the
time ABI hides already; `time_abi_msr_filter` now matches on the vendor, so that a vendor's range is
a change of one arm. Without the time ABI no filter is installed, and nothing changes. The commit
adds a test, changes two, and adds a sentence to the Guide's snapshots page. It changes files that
PR11's position 030, PR12's positions 033-035, PR13's positions 041 and 046, PR17's positions 071
and 074, PR18's positions 077, 079, 084, and 085, PR19's positions 087 and 088, PR20's positions
089, 090, 092, and 094, PR23's positions 108 and 113, PR24's position 118, and 57 positions that no
PR has yet also change, and it changes only lines that positions 137 and 145, which no PR has yet,
added, so it changes no line that a mapped PR adds. Replaying it on upstream main conflicts in both
of its files: `time_abi.rs` does not exist there, since position 128 adds it, and the sentence of
the Guide sits in the time and CPU compatibility section that position 142 writes, beside lines of
positions 145 and 155. So it follows positions 128, 137, 142, 145, and 155 upstream.

Position 277 makes a snapshot keep the guest's `IA32_UMWAIT_CONTROL`, `IA32_DEBUGCTL`,
`IA32_MISC_ENABLE`, `MSR_MISC_FEATURES_ENABLES`, and `IA32_POWER_CTL`, which KVM keeps in the vCPU
and no element of a VP's saved state held, so that a restore into a new vCPU gave the guest the
values that the VMM gives a new one, and a VM reset did not return them to their reset values. It
adds three optional elements of `VpSavedState`, `umwait_control`, `debug_ctl`, and `misc_msrs`, in
fields 17 through 19, each gated by a capability of `X86PartitionCapabilities`. `from_cpuid` derives
`umwait_control` from the guest's WAITPKG; the KVM backend sets it, and the other two, for the MSRs
that the host's `KVM_GET_MSR_INDEX_LIST` lists, except `IA32_DEBUGCTL`, which every KVM has and only
kernels since commit `3700f0788da6` list, and never for an SNP guest; MSHV, WHP, and OpenHCL hold
none of them. `MISC_ENABLE`'s reset value moves from `hv1_emulator` to `x86defs`. The elements need
no version, as position 272's needed none. The commit adds their accessors to the `AccessVpState`
implementations of KVM, MSHV, WHP, and OpenHCL that position 272 extended, five unit tests of `virt`
and six of `virt_kvm`, and the backend contract tests of a module of its own of
`virt_backend_tests`. It changes files that PR01's position 003, PR05's positions 007-009, PR10's
positions 021-026, PR12's position 036, PR17's position 072, PR20's positions 092 and 093, and 32
positions that no PR has yet also change, and it changes only lines that position 272, which no PR
maps yet, adds, so it changes no line that a mapped PR adds. Replaying it on upstream main conflicts
in 14 of its files, where it adds beside what upstream main lacks: the accessors of the OpenHCL,
MSHV, WHP, and KVM backends and `virt_kvm`'s `regs.rs` sit beside the SPEC_CTRL lines of position
272, and in `virt_kvm`'s `vp_state.rs` and `virt_mshv`'s x86-64 module also beside lines of
positions 142 and 022, respectively; `virt`'s `x86/mod.rs` sits beside lines of positions 272 and
275, and its `x86/vp.rs` beside lines of positions 134 and 272; `virt_kvm`'s x86-64 module sits
beside lines of positions 024, 244, 248, 265, 268, and 272; the capability probe of `native.rs` sits
beside position 273's; and `finalize.rs` does not exist there, since position 008 adds it. So it
follows positions 008, 022, 024, 134, 142, 244, 248, 265, 268, 272, 273, and 275 upstream.

Position 278 makes a snapshot keep the guest's machine-check state, `IA32_MCG_STATUS`,
`IA32_MCG_CTL`, and the control, status, address, miscellaneous, and CMCI control registers of each
bank, which KVM keeps in the vCPU and no element held, so that restoring the CMCI entry of the
LAPIC's local vector table did not restore the bank controls behind it, and recorded errors were
lost. It adds an optional element, `machine_check`, in field 20, with the KVM side in the new
`machine_check.rs`. KVM lists `IA32_MCG_STATUS` and `IA32_MCG_CTL` but not the registers of the
banks, so the capability follows `IA32_MCG_STATUS` and the number of banks is the one that the
vCPU's `IA32_MCG_CAP` reports. A restore sets the banks that the saved state lists, puts the others
back as `bind` left them, and refuses a state that lists more banks than the vCPU has before it
writes anything; the state at reset lists none, so it resets every bank, and a VM reset now does.
The element needs no version. The commit adds three unit tests of `virt_kvm`, five against
`/dev/kvm`, and two backend contract tests. It changes files that PR05's positions 007-009, PR10's
positions 021-026, PR17's position 072, PR20's position 092, and 29 positions that no PR has yet
also change, and it changes only lines that position 277 adds, so it changes no line that a mapped
PR adds. Replaying it on upstream main conflicts in 13 of its files, where it adds beside lines of
position 277, which upstream main lacks, in the `AccessVpState` implementations, the capability
list, and the test modules, and, in some, beside lines of other positions: those of position 272 in
`x86/vp.rs`, of positions 248, 265, and 268 in `virt_kvm`'s x86-64 module, of position 142 in its
`vp_state.rs`, and of position 022 in `virt_mshv`'s x86-64 module. The `msr_state.rs` of `virt_kvm`
and the one of `virt_backend_tests` do not exist there, since position 277 adds them. So it follows
position 277, and the positions that position 277 follows, upstream.

Position 279 describes the VP state elements that hold a guest's MSRs in the Guide. The snapshot
format page lists elements 16 through 20 with the MSRs that each holds, says how a partition decides
which it has, the guest's CPUID for two and the backend for the others, which on KVM follows the
host's MSR list except for `IA32_DEBUGCTL`, with the kernel commit that lists it, and for the
machine-check banks, says that a saved state restores only into a partition that has every element
that it holds, what a snapshot or a build from before the elements does, and which guest-writable
state has no element and why (`IA32_BNDCFGS`, `MSR_VM_HSAVE_PA`, and AMD's legacy performance
counters). The snapshots page adds a bullet to its limitations, and the guide to the VMM tests names
the contract cases. It changes files that PR01's position 002, PR05's position 007, PR11's positions
029 and 030, PR12's positions 033-035, PR13's positions 041 and 046, PR17's positions 071 and 074,
PR18's positions 077, 079, and 083-085, PR19's positions 087 and 088, PR20's positions 089, 090,
092, and 094, PR21's position 101, PR23's positions 108 and 113, PR24's position 118, and 59
positions that no PR has yet also change, and it adds to no line that a mapped PR adds. Replaying it
on upstream main conflicts in all three of its files, where it adds beside what upstream main lacks:
the table follows the paragraph of position 272 in the snapshot format, the note in the guide to the
VMM tests follows that of position 273, and the bullet of the snapshots page sits beside a line of
position 257. So it follows positions 257, 272, and 273, and the positions that they follow,
upstream.

Position 280 makes a time ABI guest of an Intel CPU profile on KVM take a #GP for AMD's `HWCR`
(`0xc0010015`), as an Intel CPU does. KVM took the guest's write for any guest CPU, kept the
`McStatusWrEn` and `TscFreqSel` bits in the vCPU, where no element of the saved state reaches them,
and read them back, so a restore dropped what the guest had written. The time ABI's MSR filter now
denies `HWCR` under an Intel profile, in the `Intel` arm that position 276 left empty, and
`KvmTimeAbi::hides_msr` answers it with #GP, as for the MSRs that the time ABI hides already. An AMD
profile, every other register, and every host-initiated access are unchanged, and no element, field,
or version of the saved state changes. The commit adds a unit test and three tests against
`/dev/kvm`, changes two tests, and adds a sentence to the Guide's snapshots page. It changes files
that PR11's position 030, PR12's positions 033-035, PR13's positions 041 and 046, PR17's positions
071 and 074, PR18's positions 077, 079, 084, and 085, PR19's positions 087 and 088, PR20's positions
089, 090, 092, and 094, PR23's positions 108 and 113, PR24's position 118, and 59 positions that no
PR has yet also change, and it changes only lines that positions 145 and 276, which no PR has yet,
added, so it changes no line that a mapped PR adds. Replaying it on upstream main conflicts in both
of its files: `time_abi.rs` does not exist there, since position 128 adds it, and the sentence of
the Guide sits in the time and CPU compatibility section that position 142 writes, beside lines of
positions 145, 155, and 276. So it follows positions 128, 142, 145, 155, and 276, and the positions
that position 276 follows, upstream.

Position 281 keeps the guest's PKRU in a save of a KVM VP whose thread runs with a PKRU of 0. KVM
keeps a guest's PKRU in the vCPU, and `KVM_GET_XSAVE` reports it only if bit 9 of `XSTATE_BV` is set
in the guest FPU that the last `KVM_RUN` saved, which XSAVE writes from `XINUSE[9]`, and the
processor may clear that for a PKRU of 0, so a save after a `KVM_RUN` on such a thread could drop
the guest's PKRU. `KvmProcessorBinder::bind` now gives its thread Linux's default PKRU,
`0x5555_5554`, which leaves key 0 open, where the thread's PKRU is 0 and CPUID reports PKU and
OSPKE, with the new `pkru` module, and leaves any other PKRU as it is. No element, field, or version
of the saved state changes. The commit adds four unit tests and one test against `/dev/kvm` that is
ignored without a host with PKU. It changes `mod.rs`, which PR10's positions 021 and 024, PR17's
position 072, and 15 positions that no PR has yet also change, and it only adds lines, so it changes
no line that a mapped PR adds. Replaying it on upstream main conflicts in `mod.rs`: the `mod` list
there lacks the modules that the chain adds, and the line of `pkru` sits beside lines of positions
244, 248, 277, and 278. So it follows positions 244, 248, 277, and 278, and the positions that they
follow, upstream.

Position 282 makes a restore of an XSAVE area write the init state that the area names, on every KVM
host. `KVM_SET_XSAVE` keeps a VP's PKRU on a host with PKU, and its MXCSR on a kernel that uses
neither XSAVES nor XSAVEC, when the area's `XSTATE_BV` leaves the component out, and a kernel
without XSAVE loads the legacy bytes that `XSTATE_BV` marks init, so a VM reset, a restore onto a VP
that has run, or a crafted snapshot left state that a save did not show, in the `xsave::restore`
that position 262 adds. It now writes the buffers that the new `Buffers` builds: the bytes of a
legacy component that `XSTATE_BV` leaves out as init bytes, a seed first where KVM would keep PKRU
or MXCSR, which makes KVM write their init values, and the area itself last, so that the VP's XSAVE
state is the area's own. No element, field, or version of the saved state changes. The commit adds
five unit tests, two of the conversions' bytes and three against a model of the hosts' kernels, and
one test against `/dev/kvm` whose PKRU part runs only on a host with PKU. It changes `xsave.rs`,
which only position 262 changes, and it changes only lines that position 262, which no PR has yet,
added, so it changes no line that a mapped PR adds. Replaying it on upstream main conflicts in
`xsave.rs`, which does not exist there, since position 262 adds it. So it follows position 262,
which applies there on its own.

Position 283 makes a restore of a short or malformed XSAVE area fail with an error, where it
panicked the VM worker, in debug and release builds. `virt`'s `Xsave::write_standard`, which the
`Buffers::new` that position 282 adds calls on the area of the saved state, unwrapped its reads of
the legacy area and of the header, and sliced the area at each component that its `XCOMP_BV` and
`XSTATE_BV` both select, without checking either bound, in code that upstream main has and no commit
of the chain changes; only a crafted or damaged saved state holds such an area. It now returns
`Result<(), XsaveError>`, a new error of `virt`'s x86 VP module, and checks the area's length, each
selected component's place in the area, and the buffer's length before it writes a byte, with one
iterator that places the components for the check and the copy. `Buffers::new` returns the error,
and `KvmError` gains `Xsave`, which wraps it, so `restore_all` fails with the state error of the
xsave element before any `KVM_SET_XSAVE`. No element, field, or version of the saved state changes.
The commit adds five unit tests of `write_standard`, one of `Buffers::new`, and a KVM-only contract
test of `virt_backend_tests` in a module of its own, which registers with the `kvm_only` form of
`backend_test!` that position 260 adds. It changes files that PR05's position 008, PR10's positions
022 and 024, and 19 positions that no PR has yet also change, and it changes no line that a mapped
PR adds: it rewrites `write_standard`, which upstream main has and no commit of the chain changes,
changes lines that positions 262 and 282, which no PR has yet, added, and adds the rest. Replaying
it on upstream main conflicts in three files, none of them in `write_standard`, which its patch
rewrites there without conflicts: the `xsave.rs` of `virt_kvm` does not exist there, since position
262 adds it; in `lib.rs`, the new variant follows the `GifClear` of position 265; and in `vp.rs`,
its tests follow those that positions 134 and 277 add. Its contract test also needs the `kvm_only`
form of `backend_test!`, and upstream main's other caller of `write_standard`, in `set_xsave` of
`vp_state.rs`, which position 262 replaces with `xsave::restore`, would have to handle the result.
So it follows positions 134, 260, 262, 265, 277, and 282, and the positions that they follow,
upstream.

Position 284 makes the KVM backend drop an interrupt in the reserved delivery mode 011, remote read,
that comes by a direct MSI, an irqfd route, or an IO-APIC route, where it handed the mode to KVM,
which takes it as its paravirtual kick: KVM sets the target VP's `pv_unhalted`, so
`KVM_GET_MP_STATE` reports a halted VP as runnable, which the saved state holds, and a running VP's
next HLT returns at once, an early return that the saved state does not hold, so a restored VP halts
at an HLT where the saved one returned. `KvmMsi::new`, which `request_msi`, `KvmX86MsiRouteBuilder`,
and `set_irq_route` call, in code that upstream main has and no commit of the chain changes, now
returns `None` for the mode, so each of them drops the request with the rate-limited log that it
already has, as it does for an address outside the MSI window, and the messages of `request_msi` and
`set_irq_route` now say that the delivery mode can be the reason. The other seven modes reach KVM
unchanged, including 110, which KVM takes as a startup IPI and which the INIT and SIPI contract
tests of position 268 send as an MSI, and MSHV and WHP already reject the mode for an MSI and for an
IO-APIC input. No element, field, or version of the saved state changes. The commit adds two unit
tests of `KvmMsi::new` and one test against `/dev/kvm`, which is ignored, to `mod.rs`, and the `msi`
module of `virt_backend_tests`, whose contract test runs on KVM, MSHV, and WHP, and whose irqfd twin
is KVM-only. It changes files that PR05's position 008, PR10's positions 021 and 024, PR17's
position 072, and 22 positions that no PR has yet also change, and it changes no line that a mapped
PR adds: it rewrites two log strings, which upstream main has and no commit of the chain changes,
and adds the rest. Replaying it on upstream main conflicts in two files, neither in `KvmMsi::new`
nor in the three callers, which its patch changes there without conflicts: in `mod.rs`, the imports
of its tests sit among those that position 259 adds, and in `main.rs`, the `mod msi;` sits between
the `mod init_sipi;` and the `mod msr_state;` that positions 268 and 277 add. Its test against
`/dev/kvm` also uses the `test_guest` module that position 244 adds, which moves the `GuestRam` that
upstream main keeps in the tests of `extint.rs`, and its irqfd twin needs the `kvm_only` form of
`backend_test!` that position 260 adds. So it follows positions 244, 259, 260, 268, and 277, and the
positions that they follow, upstream.

Position 285 has the KVM backend refuse to save a vCPU that is in system management mode (SMM) or
has a system management interrupt (SMI) pending, where a save succeeded. A guest can send an SMI as
an interrupt in the SMI delivery mode, and KVM holds one that reaches a stopped vCPU as a pending
SMI until the vCPU runs; it reports both states in the `smi` field of `KVM_GET_VCPU_EVENTS`, which
no element of the saved state reads, so a restore lost the SMI, or ran the registers of the SMM
handler outside SMM. The `check_saveable` that position 265 overrides, and that `save_all` calls
before it reads any element, now also reads the events, after the GIF check, and refuses such a vCPU
with a new `KvmError::SmmState`, which names it. The read leaves the SMI with the vCPU, so a capture
that rolls back keeps it, and a latched INIT stays with the capture's check of positions 268 and
269. The commit adds a `virt_kvm` module, `smm`, with the check, two unit tests, and two ignored
tests that need `/dev/kvm`; a KVM-only contract test, `save::kvm::refuses_a_vp_with_an_smi_pending`;
and two paragraphs to the Guide's snapshots page. It rewrites a sentence of `init_sipi.rs`'s module
documentation that said that nothing else notices an INIT that SMM blocks. The Guide and the
module's documentation also say that, with RAM mapped only in KVM's normal address space as OpenVMM
maps it, an SMI that a vCPU takes ends its VM, as observed on Linux 6.18.57, and that a change that
maps memory for SMM must revisit the refusal; the observation was of a minimal VM with that layout,
not of the OpenVMM process. No element, field, or version of the saved state changes. It changes
files that PR10's positions 021 and 024, PR11's position 030, PR12's positions 033-035, PR13's
positions 041 and 046, PR17's positions 071, 072, and 074, PR18's positions 077, 079, 084, and 085,
PR19's positions 087 and 088, PR20's positions 089, 090, 092, and 094, PR23's positions 108 and 113,
PR24's position 118, and 71 positions that no PR has yet also change, and it changes no line that a
mapped PR adds: it rewrites lines of `check_saveable` that position 265 adds and the sentence that
position 268 adds, and adds the rest. Replaying it on upstream main conflicts in five of its files,
where it adds beside what upstream main lacks: `init_sipi.rs` and `save.rs` do not exist there,
since positions 268 and 266 add them; in `vp_state.rs`, `check_saveable` does not exist there, since
position 265 adds it; in `lib.rs`, `SmmState` follows the `Xsave` of position 283 and the `GifClear`
of position 265, which follow the time ABI's variants of position 128; and in the Guide's snapshots
page, its paragraphs sit after the INIT and SIPI paragraph of position 269, in the block that
positions 071 and 235 write. Its `mod smm;` applies in `mod.rs` without conflicts. Its contract test
also needs the `kvm_only` form of `backend_test!` that position 260 adds, and the `check_saveable`
hook that position 264 adds. So it follows positions 071, 128, 235, 260, 264, 265, 266, 268, 269,
and 283, and the positions that they follow, upstream.

Position 286 makes setting the activity of a KVM vCPU take the vCPU out of SMM, where it left a vCPU
that had run as it was. A restore and a reset set the state of a vCPU outside SMM, with no SMI
pending and no INIT latched, as a saved state describes it, but KVM changes a vCPU's SMM state, its
pending SMI, and its latched INIT only when the events that are set mark them valid
(`KVM_VCPUEVENT_VALID_SMM`), and `set_activity` never did. So the reset of a guest that reboots, or
a restore onto a vCPU that had run, left it in SMM, with its SMI pending, or with its INIT latched,
and the next read of its MP state accepted the INIT, which reset the registers that the restore had
just set. `set_activity` now calls the new `smm::leave` before it sets the MP state. It gets the
vCPU's events first, because KVM replaces an SMI that is pending but not one that it has been asked
to inject, which getting the events makes pending, and sets them with `KVM_VCPUEVENT_VALID_SMM` and
no SMM state, so the other events are as they were. It runs before the MP state is set, because a
vCPU in SMM cannot wait for a SIPI: `KVM_SET_MP_STATE` refuses it, and a vCPU with an SMI pending,
until Linux 6.16, and `KVM_RUN` since Linux 6.17, by the kernel's source and not by a run on a host
before 6.17. A restore into a new vCPU, as every microVM restore is, changes nothing. No element,
field, or version of the saved state changes. The commit adds two ignored tests that need `/dev/kvm`
to the `smm` module of position 285, and four KVM-only contract tests to `activity.rs`:
`activity::kvm::setting_the_activity_drops_an_smi`, `setting_the_activity_drops_a_latched_init`,
`refusing_an_activity_keeps_an_smi`, and `reset_drops_an_smi`. It changes files that PR10's position
024 and 13 positions that no PR has yet also change, and it changes no line that a mapped PR adds:
it rewrites a line of `smm.rs`'s module documentation, which position 285 adds, and adds the rest.
Replaying it on upstream main conflicts in `smm.rs` and `activity.rs`, which do not exist there,
since positions 285 and 267 add them; its call in `set_activity` applies in `vp_state.rs` without
conflicts, but calls the `smm` module of position 285. Its contract tests need the `kvm_only` form
of `backend_test!` that position 260 adds and the `check_saveable` hook of position 264, which
position 265 overrides. So it follows positions 260, 264, 265, 267, and 285 upstream.

Position 287 makes the KVM backend save and restore two pending NMIs, where a restore gave a vCPU
one. KVM holds a second NMI that reaches a vCPU before it has run as a second pending NMI: it
injects one at once and the other once the guest's handler for the first returns, so
`KVM_GET_VCPU_EVENTS` reports two pending NMIs while no NMI is blocked and none is being injected,
and one otherwise, from Linux 6.4 on; by the sources of Linux 5.16, 6.1, and 6.3, an earlier kernel
reports only whether an NMI is pending. `Activity` held that Boolean alone and `set_activity` wrote
one pending NMI, so a restored guest took one NMI where the captured guest would have taken two,
which a guest-free test on azure-intel-kvm-7 (Linux 6.18.57) observed as two pending NMIs at the source
and one at the destination. `Activity` gains `nmi_pending_count`, an optional field under mesh
number 7 that is `Some(2)` for two pending NMIs and `None` for every other state, so a state with at
most one pending NMI encodes as it always did and no version changes: a saved state of an earlier
build has no count and restores one NMI as before, and an earlier build ignores the count in a later
state, so it restores two pending NMIs as one and the guest loses an NMI, silently, as the Guide
says. KVM's `activity` reports the count, and its `set_activity` writes it after refusing a count
that KVM would cut down to what it holds, with the other refusals and so before the `smm::leave`
that position 286 adds, which a refused activity must not run. The commit adds a wire test of the
field, with a mirror of the earlier `Activity` that pins the bytes; a unit test of the refusal; two
KVM-only contract tests in `activity.rs`, one of which restores into the VP of a second partition
and so registers with `petri::multitest!`; a subsection of the Guide's snapshot format and a
paragraph of its snapshots page; and the field's initializers and bindings in WHP's `apic.rs` and in
the SNP and TDX backends of OpenHCL. It changes files that PR10's positions 022 and 024, PR11's
positions 029 and 030, PR12's positions 033-035, PR13's positions 041 and 046, PR17's positions 071
and 074, PR18's positions 077, 079, and 083-085, PR19's positions 087 and 088, PR20's positions 089,
090, 092, and 094, PR21's position 101, PR23's positions 108 and 113, PR24's position 118, and 72
positions that no PR has yet also change, and it changes no line that a mapped PR adds: it rewrites
one line of `set_activity`, `pending: nmi_pending.into()`, which upstream main has and no commit of
the chain changes, and adds the rest. Replaying it on upstream main conflicts in five files, where
it adds beside what upstream main lacks: in the Guide's snapshot format, its subsection follows the
section that position 272 adds and position 279 extends; in its snapshots page, its paragraph sits
among text that positions 176, 209, and 213 write; in `vp.rs`, its tests follow the test module that
position 134 adds and the tests that positions 272, 277, and 283 add, while the field applies
without conflicts; in `vp_state.rs`, its refusal and its function follow the
`check_restorable_interruption` that position 267 adds, while its change to `activity` and its write
of the count apply without conflicts; and `activity.rs` does not exist there, since position 267
adds it. The initializers in `apic.rs`, `snp/mod.rs`, and `tdx/mod.rs` apply without conflicts. Its
contract tests also need the `kvm_only` form of `backend_test!` that position 260 adds, and the
`send` helper and the imports that position 286 adds to `activity.rs`. So it follows positions 134,
260, 267, 272, 279, and 286, and the positions that they follow, upstream.

Position 288 makes the backends that hold at most one pending NMI refuse a restore of a count of
them, where a restore that ignored the count would drop the second NMI without a word. MSHV, and WHP
with its offloaded APIC, keep the pending NMI in a bit of the APIC page, WHP's emulated APIC and
OpenHCL's SNP and TDX backends in a Boolean, and OpenHCL's mshv backend neither saves nor restores
it yet, as its TODOs say, so none has a field for position 287's count. `set_activity` of MSHV and
WHP now returns their `UnsupportedState` error, which position 277 adds for the elements that they
cannot hold, naming `nmi_pending_count`, and OpenHCL's three backends return their `Unimplemented`
error, before they change any state. Only a damaged or crafted saved state reaches it, since a
restore requires the backend that captured the snapshot and only KVM saves a count. No element,
field, or version of the saved state changes. The commit adds the `nmi` module of
`virt_backend_tests`, which builds on Windows too and whose contract test runs on KVM, MSHV, and
WHP: a restore of two pending NMIs and a halt must either read back as given or fail and leave the
activity as it was. It also adds a clause to the field's documentation and rewrites the sentences of
the Guide's two pages that position 287 adds. It changes files that PR05's position 008, PR10's
positions 022 and 024, PR11's positions 029 and 030, PR12's positions 033-035, PR13's positions 041
and 046, PR17's positions 071 and 074, PR18's positions 077, 079, and 083-085, PR19's positions 087
and 088, PR20's positions 089, 090, 092, and 094, PR21's position 101, PR23's positions 108 and 113,
PR24's position 118, and 73 positions that no PR has yet also change, and it changes no line that a
mapped PR adds: it rewrites lines that position 287 adds, and adds the rest. Replaying it on
upstream main conflicts in six files, where it edits or adds beside what upstream main lacks: in the
Guide's two pages, the text that position 287 adds; in `snp/mod.rs` and `tdx/mod.rs`, the binding of
`nmi_pending_count` that position 287 adds; in `vp.rs`, the field's documentation, which position
287 adds; and in `main.rs`, `mod nmi;` sits between upstream's `mod native;` and the `mod
registers;` that position 260 adds, among the `mod` lines that positions 260, 263, 266-268, 273,
277, 283, and 284 add. Its changes to `x64.rs`, to the `vp_state.rs` of `virt_mshv`, and to
`apic.rs` apply without conflicts, and `nmi.rs` is a new file. So it follows positions 277 and 287
upstream.

Position 289 makes the KVM backend save an exception that it has queued but not injected yet once,
and refuse a pending exception that KVM would drop, where a save recorded it twice and a restore
lost an exception that it was given alone. OpenVMM does not enable `KVM_CAP_EXCEPTION_PAYLOAD`, so
`KVM_GET_VCPU_EVENTS` reports such an exception as pending and injected both, and
`KVM_SET_VCPU_EVENTS` restores an exception only as injected and drops one that it is given as
pending, as two ignored tests that need `/dev/kvm` observe on Linux 6.18.57, and as the sources of
Linux 6.18 say. `activity` now records an exception, queued or injected, as the pending interruption
alone, with no pending event, and `set_activity` restores as injected the pending exception that an
earlier save recorded beside the same injected one, with no payload, and refuses any other pending
exception, with the other refusals, before it changes any activity state and so before the
`smm::leave` that position 286 adds, which a refused activity must not run. No element, field, or
version of the saved state changes: a saved state of an earlier build restores as before, and an
earlier build restores a later state's pending interruption as the injected exception, as it
restored the pair. The commit adds unit tests of the new functions, the two ignored tests, two
KVM-only contract tests in `activity.rs`, a paragraph of the Guide's snapshots page, and a passage
of its snapshot format's "Fields of an element". It changes files that PR10's position 024, PR11's
positions 029 and 030, PR12's positions 033-035, PR13's positions 041 and 046, PR17's positions 071
and 074, PR18's positions 077, 079, and 083-085, PR19's positions 087 and 088, PR20's positions 089,
090, 092, and 094, PR21's position 101, PR23's positions 108 and 113, PR24's position 118, and 70
positions that no PR has yet also change, and it changes no line that a mapped PR adds: it rewrites
two lines of a comment in `set_activity` that position 267 adds and lines that upstream main has and
no commit of the chain changes, and adds the rest. Replaying it on upstream main conflicts in four
files, where it edits or adds beside what upstream main lacks: in the Guide's snapshot format, its
paragraph follows the subsection that position 287 adds, and the text that position 288 adds to it,
in the section that position 272 adds and position 279 extends; in its snapshots page, its paragraph
follows the paragraph that position 287 adds and position 288 extends, and precedes text that
position 176 writes, in a block that positions 209 and 213 also write; in `vp_state.rs`, its refusal
sits beside the calls that positions 267, 287, and 286 add, its functions precede the
`check_restorable_interruption` that position 267 adds, and its change to `activity` sits beside the
`nmi_pending_count` that position 287 adds; and `activity.rs` does not exist there, since position
267 adds it. Its contract tests also need the `kvm_only` form of `backend_test!` that position 260
adds. So it follows positions 176, 209, 213, 260, 267, 272, 279, 286, 287, and 288, and the
positions that they follow, upstream.

Position 290 makes the debug check of `restore_all` accept what a hypervisor documents that it keeps
differently from how it is given, where it asserted equality and so panicked after a restore that
had succeeded. KVM drops the reserved bits 8-11 of a segment's attributes, sets the accessed bit of
the segment registers but the LDTR as it writes them on a CPU with VMX that runs the guest
unrestricted, and reads it set of the data segments that are present on an SVM CPU, while the
backend clears it of a segment that is not present, so a state that an earlier save, another host,
or a crafted snapshot holds reads back differently; it makes SYSENTER_EIP and SYSENTER_ESP canonical
at the width of the host's virtual addresses, or keeps their low 32 bits on SVM for a guest CPU that
is not Intel-compatible, keeps SYSENTER_CS in 32 bits on VMX, and keeps the low 32 bits of TSC_AUX
for a guest CPU that is not Intel-compatible, all by the sources of Linux 6.18; and it restores the
exception that position 289 describes as the injected one alone. `StateElement` gains `restore_eq`,
whose default is equality, `restore_all` asserts it in a debug build and prints both states, and
`Registers`, `Activity`, `VirtualMsrs`, and `TscAux` override it, each only when the new
`X86PartitionCapabilities::normalizes_restored_state`, an in-memory field that only the KVM backend
sets, says that the hypervisor normalizes, so MSHV, WHP, and OpenHCL compare as before; everything
else of those elements, the count of pending NMIs of position 287 included, is compared exactly. No
production VP saved-state element, serialized field, saved-state type, or version changes. The
commit adds unit tests of each override and of the macro, whose test backend keeps less than it is
given, and two KVM-only contract tests, in the new `restore.rs`, that restore a crafted VP state
with `restore_all`. It changes files that PR05's positions 007-009, PR10's positions 021-025, PR17's
position 072, PR20's position 092, and 36 positions that no PR has yet also change, and it changes
no line that a mapped PR adds: it rewrites two lines of `state.rs` that upstream main has and no
commit of the chain changes, the declaration of `StateElement` and the assertion of `restore_all`,
and adds the rest. Replaying it on upstream main conflicts in four files, where it adds beside what
upstream main lacks: its tests of `state.rs` and `vp.rs` follow the test modules that positions 264
and 134 add; in `virt_kvm`'s `mod.rs`, its line follows the `caps.nested_virt` that position 024
adds, in the `build` that position 259 also changes; and in `main.rs`, `mod restore;` sits between
the `mod registers;` that position 260 adds and the `mod save;` that position 266 adds, among the
`mod` lines that positions 260, 263, 266-268, 273, 277, 283, 284, and 288 add. The rest of its
changes to `state.rs` and `vp.rs`, and its changes to `x86/mod.rs` and to the `mod.rs` of
`virt_mshv`, apply without conflicts, and `restore.rs` is a new file. Its contract tests also need
the `kvm_only` form of `backend_test!` that position 260 adds, and its `Activity` override names the
`nmi_pending_count` that position 287 adds. So it follows positions 024, 134, 259, 260, 264, 266,
and 287, and the positions that they follow, upstream.

Position 291 lets a VP's saved activity hold the kind of its interrupt shadow, where it held only
whether the VP had one, so that no backend could save the kind or restore it and KVM restored every
shadow as a MOV SS shadow. The shadow that an `STI` leaves when it sets `IF` and the one that a `MOV
SS` or `POP SS` leaves differ at the next instruction boundary: the MOV SS shadow also blocks NMIs
and suppresses certain debug exceptions, and the STI shadow does not. `Activity` gains the optional
field `interrupt_shadow_kind`, number 8, of the new `InterruptShadowKind`, whose one variant, `Sti`,
names the only kind that restoring `None` does not set, since that sets the MOV SS shadow, the safe
one. A backend reports `Some(Sti)` for an STI shadow and `None` for every other shadow, a MOV SS
shadow, AMD's one kind of shadow, and the one bit that Hyper-V's interrupt state register has for
both kinds included, as position 287's `nmi_pending_count` is reported only when it is 2, so every
state that an earlier build can save encodes as it did and reads back from its restore as it was
saved. The field needs no version: a saved state of an earlier build has none and restores as
before, and an earlier build ignores it in a later state. The commit changes no backend behavior:
`Activity`'s conversion from and to Hyper-V's registers writes no kind and reads `None`, WHP's
literals say `None`, and KVM and OpenHCL's SNP and TDX backends report `None` and ignore the field
for now. It adds a wire test that mirrors the activity of the build before the field, shows that
each build reads the other's states, and pins the field's bytes, a test that the registers hold no
kind, and the kind in the test of the `restore_eq` that position 290 adds, which compares it
exactly. It changes files that PR10's positions 022 and 024, and 23 positions that no PR has yet,
also change, and it changes no line that a mapped PR adds: it rewrites three lines of the comment of
`restore_eq` that position 290 adds, and adds the rest. Replaying it on upstream main conflicts in
five files, where it adds or edits beside what upstream main lacks: in `vp.rs`, its field follows
the `nmi_pending_count` that position 287 adds, with its documentation, its lines of `set_values`
and `at_reset` follow the `nmi_pending_count` lines that position 287 adds to them, its edits of
`restore_eq` sit in the code and the comment that position 290 adds, and its tests follow the test
module that position 134 adds and the tests that position 290 adds; in `virt_kvm`'s `vp_state.rs`,
its line in `activity` follows the `nmi_pending_count` line that position 287 adds, in the activity
that position 289 changes, and its binding in `set_activity` follows the one that position 287 adds;
in `virt_whp`'s `apic.rs`, its two lines follow the `nmi_pending_count` lines that position 287
adds; and in OpenHCL's SNP and TDX backends, its lines follow the `nmi_pending_count` lines that
positions 287 and 288 add. So it follows positions 134, 287, 288, 289, and 290, and the positions
that they follow, upstream.

Position 292 makes the KVM backend save the kind of an interrupt shadow and restore it, where
`activity` turned KVM's `interrupt.shadow` into a Boolean and `set_activity` wrote it back as 1, a
MOV SS shadow, while VMX reports the shadow that an `STI` leaves as 2, so that a VP that KVM had
stopped in an STI shadow restored in a MOV SS shadow, as two ignored tests that need `/dev/kvm`
observe on Linux 6.18.57, and as the sources of Linux 6.18 say. A new `shadow` module converts
between KVM's value and the activity's `interrupt_shadow` and `interrupt_shadow_kind`: the shadow
that VMX reports as the STI bit alone is saved as `Some(Sti)` and restores as an STI shadow, and
every other shadow, a MOV SS shadow and SVM's one shadow, which KVM reports as both bits and sets
for any value but 0, is saved without a kind and restores as a MOV SS shadow, as every restore did.
So every state that an earlier build can save is saved as before and reads back as saved, and the
check of the `restore_eq` that position 290 adds needs no allowance for it. `set_activity` also
refuses a kind without a shadow, with its other refusals, in a function of its own, before it
changes any activity state and so before the `smm::leave` that position 286 adds, and it accepts the
kind on SVM, where it reads back with none, since an AMD capture holds no kind and a snapshot
restore refuses the snapshot of another vendor's CPU generation. No element, field, or version of
the saved state changes beyond the optional field of position 291. The commit adds unit tests of the
module and of the refusal, two ignored tests that need `/dev/kvm`, which run a real-mode guest that
stops with each kind of shadow, two KVM-only contract tests, one in `activity.rs` and one in
`restore.rs` that restores with `restore_all`, a case of the `refusing_an_activity_keeps_an_smi`
that position 286 adds, a paragraph of the Guide's snapshots page, and a passage of its snapshot
format's "Fields of an element", whose introduction it extends. It changes files that PR10's
positions 021 and 024, PR11's positions 029 and 030, PR12's positions 033-035, PR13's positions 041
and 046, PR17's positions 071, 072, and 074, PR18's positions 077, 079, and 083-085, PR19's
positions 087 and 088, PR20's positions 089, 090, 092, and 094, PR21's position 101, PR23's
positions 108 and 113, PR24's position 118, and 81 positions that no PR has yet also change, and it
changes no line that a mapped PR adds: it rewrites three lines of the Guide's introduction that
position 287 adds, three lines of `activity` and `set_activity` that position 291 adds, and two
lines of them that upstream main has and no commit of the chain changes, and adds the rest.
Replaying it on upstream main conflicts in six files, where it edits or adds beside what upstream
main lacks, and two of them, `activity.rs` and `restore.rs`, do not exist there, since positions 267
and 290 add them: in the Guide's snapshot format, its paragraph follows the paragraph that position
289 adds, and it rewrites lines of the subsection that position 287 adds, in the section that
position 272 adds and position 279 extends; in its snapshots page, its paragraph follows the
paragraph that position 289 adds and precedes text that position 176 writes, in a block that
positions 033, 034, 071, 090, 113, 209, 213, 222, 224, 229, 235, 265, 269, 285, 287, and 288 also
write; in `virt_kvm`'s `mod.rs`, its `mod shadow;` precedes the `mod smm;` that position 285 adds;
and in `vp_state.rs`, its import precedes the `use super::xsave;` that position 262 adds, its
changes to `activity` and `set_activity` replace lines that position 291 adds and sit beside the
`nmi_pending_count` lines that position 287 adds, in the activity that position 289 changes, and its
function follows the functions that positions 267, 287, and 289 add. Its contract tests also need
the `kvm_only` form of `backend_test!` that position 260 adds, and its case of
`refusing_an_activity_keeps_an_smi` needs the test that position 286 adds. So it follows positions
176, 209, 213, 260, 262, 267, 272, 279, 285, 286, 287, 289, 290, and 291, and the positions that
they follow, upstream.

Position 293 makes the backends that cannot hold the kind of an interrupt shadow refuse it, where a
restore that ignored it would give the VP a shadow that differs from the saved one without a word.
MSHV, WHP, and OpenHCL's mshv backend keep the shadow in the one bit that Hyper-V's interrupt state
register has for both kinds, OpenHCL's TDX backend reads both kinds of its VMCS as one Boolean and
restores a MOV SS shadow, and its SNP backend does not hold the shadow yet. `set_activity` of MSHV
and WHP now returns the `UnsupportedState` error that they have, naming `interrupt_shadow_kind`,
beside the refusal of `nmi_pending_count` that position 288 adds, and OpenHCL's mshv, SNP, and TDX
backends return `Unimplemented("interrupt_shadow_kind")`, before any state changes, and the SNP and
TDX backends bind the kind, which they ignored. Only a damaged or crafted saved state reaches the
refusal, since a restore requires the backend that captured the snapshot and only KVM on an Intel
CPU saves a kind. KVM on an AMD CPU sets its one shadow for the kind, which reads back with none, as
position 292's comment says. That the VMCS of TDX holds the kind, so that its backend could save and
restore it, is not in this change. No element, field, or version of the saved state changes. The
commit adds one contract test, which runs on KVM, MSHV, and WHP, in the new `shadow.rs`, which
builds on Windows too, and adds a sentence to the Guide's snapshots page and extends a paragraph of
its snapshot format. It changes files that PR05's position 008, PR11's positions 029 and 030, PR12's
positions 033-035, PR13's positions 041 and 046, PR17's positions 071 and 074, PR18's positions 077,
079, and 083-085, PR19's positions 087 and 088, PR20's positions 089, 090, 092, and 094, PR21's
position 101, PR23's positions 108 and 113, PR24's position 118, and 76 positions that no PR has yet
also change, and it changes no line that a mapped PR adds: it rewrites two lines of the snapshot
format and one of the snapshots page that position 292 adds, and two lines each of the SNP and TDX
backends that position 291 adds, and adds the rest. Replaying it on upstream main conflicts in eight
files, where it adds beside what upstream main lacks: in the Guide's snapshot format and its
snapshots page, its text follows the paragraphs that position 292 adds, in the section and the block
that position 292's paragraphs sit in; in `virt_mshv`'s `vp_state.rs`, `virt_whp`'s `apic.rs`, and
OpenHCL's `mshv/x64.rs`, `snp/mod.rs`, and `tdx/mod.rs`, its refusal follows the refusal of
`nmi_pending_count` that position 288 adds; and in `main.rs`, `mod shadow;` sits between the `mod
save;` that position 266 adds and the `mod speculation;` that position 273 adds, among the `mod`
lines that positions 260, 263, 266-268, 273, 277, 283, 284, 288, and 290 add. `shadow.rs` is a new
file. So it follows positions 266, 273, 288, and 292, and the positions that they follow, upstream.

Position 294 corrects what the comments and the Guide of positions 291 and 292 say about the shadow
that a restore sets, and changes no code. An `STI` shadow that restores as a MOV SS shadow, as a
build that predates the kind restores a later snapshot's, holds an NMI off for one more instruction
and also loses an instruction breakpoint there, because an Intel CPU suppresses that breakpoint
under a MOV SS shadow, as KVM models in `kvm_is_code_breakpoint_inhibited`, where those said that it
only delays an NMI or a debug exception by an instruction; and a restore sets a MOV SS shadow only
for a state that has a shadow, where those said that a restore that is given no kind sets one. It
changes the doc comments of `InterruptShadowKind`, `Activity::interrupt_shadow_kind`, and
`shadow::to_kvm`, a comment of each of two KVM-only contract tests, and a paragraph of each of the
Guide's snapshot format and snapshots pages. It changes files that PR10's positions 022 and 024,
PR11's positions 029 and 030, PR12's positions 033-035, PR13's positions 041 and 046, PR17's
positions 071 and 074, PR18's positions 077, 079, and 083-085, PR19's positions 087 and 088, PR20's
positions 089, 090, 092, and 094, PR21's position 101, PR23's positions 108 and 113, PR24's position
118, and 73 positions that no PR has yet also change, and it changes no line that a mapped PR adds:
it rewrites 20 lines of `vp.rs` that position 291 adds, and 26 lines of the other five files that
position 292 adds. Replaying it on upstream main conflicts in six files, where it edits beside what
upstream main lacks, and `shadow.rs`, `activity.rs`, and `restore.rs`, which positions 292, 267, and
290 add, do not exist there: in `vp.rs`, its edits sit in the field and the enum that position 291
adds, after the `nmi_pending_count` that position 287 adds; in the Guide's snapshot format, they sit
in the paragraph that position 292 adds and position 293 extends, in the section that position 272
adds and position 279 extends; and in its snapshots page, in the paragraph that position 292 adds
and position 293 extends, in a block that positions 033, 034, 071, 090, 113, 176, 209, 213, 222,
224, 229, 235, 265, 269, 285, 287, 288, and 289 also write. So it follows positions 176, 209, 213,
267, 272, 279, 287, 290, 291, 292, and 293, and the positions that they follow, upstream.

Position 295 makes the generated `restore_all` refuse a saved state that holds an element that the
partition lacks, where it asserted that each element was present as it set them, so that a decoded
snapshot that held one, such as SynIC state for a microVM, whose partition has no Hyper-V
enlightenments, or an element for an MSR that the destination's KVM does not list, panicked after
`restore_all` had set the elements before it and, since OpenVMM builds with `panic = "abort"`,
aborted the process with no restore error. `restore_all` now checks every element of the saved state
against the partition before it sets or commits any of them, and fails with the new
`StateError::Unsupported`, which names the element in the phase `restore check` and has no source.
`StateError` becomes an enum, whose `Access` variant keeps the phase and the backend's error as its
source, so a backend failure reports as before. The check is per call, so a refusal for a later VP
leaves the VM state and the earlier VPs restored, as any other failed restore does. No element,
field, or version of the saved state changes, and `StateError` is an in-memory error. The commit
adds three unit tests of the macro, one of which fails without the check, adapts the test of
`check_saveable` that position 264 adds to the enum, and says in the Guide's snapshot format and
snapshots page, and in the rules comment of the VP state's `state_trait!`, that a restore refuses
such a state, where position 279 said that it stops on an assertion. It changes files that PR10's
positions 022 and 024, PR11's positions 029 and 030, PR12's positions 033-035, PR13's positions 041
and 046, PR17's positions 071 and 074, PR18's positions 077, 079, and 083-085, PR19's positions 087
and 088, PR20's positions 089, 090, 092, and 094, PR21's position 101, PR23's positions 108 and 113,
PR24's position 118, and 73 positions that no PR has yet also change, and it changes no line that a
mapped PR adds: it rewrites 11 lines of `state.rs` that upstream main has and no commit of the chain
changes, two lines of `state.rs` that position 264 adds, a line of the rules comment that position
277 adds, and a line each of the Guide's snapshot format and snapshots page that position 279 adds,
and adds the rest. Replaying it on upstream main conflicts in four files, where it edits or adds
beside what upstream main lacks: in `state.rs`, its paragraph of the macro's documentation follows
the one that position 264 adds for `check_saveable`, its edit of the line of `save_all` that calls
`check_saveable` rewrites a line that position 264 adds, its paragraph of `restore_all`'s
documentation precedes the one that position 290 adds, and its tests follow the test module that
positions 264, 272, and 290 add; in `vp.rs`, its bullet extends the comment on adding an element
that position 272 adds and position 277 extends; in the Guide's snapshot format, its text sits in
the section that position 272 adds and position 279 extends; and in its snapshots page, its
paragraph follows the one that position 292 adds, position 293 extends, and position 294 corrects,
and precedes text that position 176 writes, and its edit of the Limitations rewrites a line of the
bullet that position 279 adds. So it follows positions 176, 264, 272, 277, 279, 290, and 292-294,
and the positions that they follow, upstream.

Position 296 adds the contract tests of position 295's refusal, in the new `unsupported_state.rs`,
which builds on Linux and Windows. `refuses_unsupported_cpu_state` restores a saved state that holds
SynIC state, which the fixture's partition always lacks, and, where the backend, the host, or the
fixture's CPUID does not give the partition them, `IA32_SPEC_CTRL`, `IA32_UMWAIT_CONTROL`,
`IA32_DEBUGCTL`, the feature and power MSRs, and the machine-check state, one at a time and with
registers that differ from the VP's, and requires that the restore fails with
`StateError::Unsupported`, naming the element, and that the registers stay as they were.
`refuses_unsupported_vm_state` does the same for each element of the VM state, the hypercall MSRs,
the reference time, and the reference TSC page. Both run on KVM, MSHV, and WHP, and the commit adds
a paragraph to the Guide's page of the contract tests. It changes files that PR01's position 002,
PR05's positions 007 and 008, and 13 positions that no PR has yet also change, and it changes no
line that a mapped PR adds: it removes no line. Replaying it on upstream main conflicts in two
files, where it adds beside what upstream main lacks, and `unsupported_state.rs` is a new file: in
`main.rs`, `mod unsupported_state;` sits between the `mod time;` that upstream main has and the `mod
xsave;` that position 283 adds, among the `mod` lines that positions 260, 263, 266-268, 273, 277,
283, 284, 288, 290, and 293 add; and in the Guide's page of the contract tests, its paragraph
follows the ones that positions 273 and 279 add. Its tests also need the `StateError::Unsupported`
that position 295 adds, and `SpecCtrl`, `UmwaitControl`, `DebugCtl`, `MiscMsrs`, and `MachineCheck`
and the capabilities that name them, which positions 272, 277, and 278 add. So it follows positions
272, 273, 277, 278, 279, 283, and 295, and the positions that they follow, upstream.

Position 297 makes WHP keep a restored VP that was saved halted, or idle, suspended, where a VP that
was saved in `cli; hlt` ran on past the `hlt` when it first ran, although its activity read back as
halted, and a guest that halts with its interrupts disabled, as Linux does for an offlined
processor, expects no wake-up. With the hypervisor's APIC, WHP keeps a suspend state that the VMM
writes only until the VMM writes more of the VP's state, and `restore_all` writes the registers, the
activity, and then the APIC page, so a VP ran on after a restore into the VP that was saved and into
a new partition's alike. Only a VP that runs its own `hlt`, or its `rdmsr` of
`HV_X64_MSR_GUEST_IDLE` for an idle one, after the VMM's last write stays suspended, and a VP that
waits for a SIPI is not affected. `restore_activity` now records a halted or idle state in the VP's
run state, which `save_activity` reports until the VP runs, and `run_vp` calls the new
`resuspend_restored_vp` before it first runs the VP: where guest memory holds the `hlt`, or the
`rdmsr` of an idle VP whose RCX holds `HV_X64_MSR_GUEST_IDLE`, before RIP, it puts RIP back onto the
instruction, clears the activity, and sets the interrupt shadow, so that the VP suspends itself
again in the state that it was saved in. The shadow is needed: an NMI or an interrupt that the
restore queued, as that of an LAPIC timer that expired during the downtime, would be taken before
the instruction and return to it, and the VP would suspend again with nothing left to wake it. A VP
that has an interruption pending, or whose RIP does not follow the instruction, is left as the
restore wrote it, with a rate-limited warning, and the VPs of the user-mode APIC, whose halted state
the VMM keeps, are unchanged. No element, field, or version of the saved state changes. The commit
adds the contract tests of the restore in the new `suspend.rs`, which builds on Windows and runs on
WHP: eight cases run a real-mode guest of a few bytes, which the test writes into the fixture's RAM,
that halts, or idles, with its interrupts enabled or disabled, save the VP, restore it into the same
VP or into a new partition's, and require that the VP stays suspended until an NMI or a fixed
interrupt wakes it and then resumes after the instruction. Three of them also deliver the wake-up
before the VP first runs. Four of them fail without the change, and those three fail without the
interrupt shadow. The commit also gives the fixture a `memory` accessor, which position 298 builds
on Windows only, and says in the Guide's page of the contract tests, and in a comment of `msi.rs`,
that a test can run the VP and that WHP reports the halted state that it restored. It changes files
that PR01's positions 002 and 003, PR05's positions 007 and 008, PR10's positions 021 and 026,
PR12's position 036, PR17's position 072, PR20's position 093, and 26 positions that no PR has yet
also change, and it changes one line that a mapped PR adds: it rewrites the first line of the header
of `fixture.rs`, which PR05's position 008 adds, into two. It also rewrites four lines of the
comment in `msi.rs` that position 284 adds and a line of the Guide's page of the contract tests that
upstream main has and no commit of the chain changes, and adds the rest. Replaying it on upstream
main conflicts in four files, where it edits or adds beside what upstream main lacks: in
`fixture.rs`, its header rewrites the line that position 008 rewrites; in `main.rs`, `mod suspend;`
sits between the `mod speculation;` that position 273 adds and the `mod time;` that upstream main
has, among the `mod` lines that positions 260, 263, 266-268, 273, 277, 283, 284, 288, 290, 293, and
296 add; in `msi.rs`, which does not exist there, since position 284 adds it; and in the Guide's
page of the contract tests, its paragraph follows the ones that positions 273, 279, and 296 add. Its
changes to `virt_whp` apply without conflicts, and `cargo check -p virt_whp --all-targets` passes
with them on upstream main, but `suspend.rs` is a new file whose cases write their guest into the
RAM that only position 008 maps into the fixture's partition. So it follows positions 008, 273, 279,
284, and 296, and the positions that they follow, upstream.

Position 298 builds the `memory` accessor that position 297 adds to the fixture of the contract
tests on Windows only, as `suspend.rs`, the one module that calls it, is. The Linux builds of
`virt_backend_tests` found the accessor unused, and Clippy's `dead_code` check, which the quick
check of OpenVMM's pull request runs with `-D warnings` on `x86_64-unknown-linux-gnu`, failed on
it. The commit adds one `cfg` attribute to `fixture.rs`, so no code that a build for Windows
compiles changes. It fixes position 297, which no PR maps yet, so it is not a fixup of a PR yet, and
folds into position 297 when that position goes into one. It changes `fixture.rs`, which PR05's
position 008, PR12's position 036, PR17's position 072, and positions 127, 142, and 297 also change,
and adds to no line that a mapped PR adds. Replaying it on upstream main conflicts in `fixture.rs`,
since the accessor that it gates does not exist there. So it follows position 297, and the
positions that position 297 follows, upstream.

Position 299 makes position 297's check of the instruction before RIP read the page tables from
guest memory, where it asked the hypervisor to translate the address with `WHvTranslateGva`, which
fails with an unmapped GPA for a page, or a page table, that a lazy restore, as that of a
copy-on-write snapshot is, has not registered yet, so that the check failed and the VP ran past its
`hlt` as before. It walks the tables from the VP's registers and guest memory, as the emulator does
without nested virtualization, and leaves the translation to the hypervisor with nested
virtualization, as the emulator does. It also drops position 297's record of the suspend state
where the VMM sets a VP's activity itself, in the injection of an extint through the synic, in a
VTL2 write of the activity register, and in the start of VTL0 by VTL2, and says in the doc comment
of `resuspend_restored_vp` what a save reads when a stop cancels the run before the VP runs the
instruction. It adds a case to position 297's `suspend.rs`, which runs the guest in 64-bit mode
with paging, through page tables that are the fixture's page alone, and says in the Guide that one
case runs in 64-bit mode. It fixes position 297, which no PR maps yet, so it is not a fixup of a PR
yet, and folds into position 297 when that position goes into one. It changes files that PR01's
positions 002 and 003, PR05's position 007, PR10's positions 021 and 026, PR12's position 036,
PR17's position 072, PR20's position 093, position 297, and 19 positions that no PR has yet also
change, and it changes no line that a mapped PR adds: it rewrites 28 lines of `suspend.rs`, six of
`apic.rs`, two of `lib.rs`, and two of the Guide that position 297 adds, and adds the rest.
Replaying it on upstream main conflicts in four files, since it rewrites lines that position 297
adds, in `apic.rs`, `lib.rs`, and the Guide's page of the contract tests, and `suspend.rs` is the
new file of position 297; its additions to `vp.rs` and `hypercalls.rs` apply without conflicts, and
`cargo check -p virt_whp --all-targets` passes on upstream main with the `virt_whp` changes of
positions 297 and 299. So it follows position 297, and the positions that position 297 follows,
upstream.

Position 300 has position 297's `resuspend_restored_vp` leave a restored VP that holds an NMI
pending, and not masked, alone: the NMI wakes it, as it would the VP that was saved suspended, so
the VP takes the NMI as it first runs and resumes after its suspending instruction. Putting RIP
back onto the instruction, in the interrupt shadow, left such a VP suspended with its NMI still
pending on the AMD host of OpenVMM's CI, whose hypervisor does not wake it, and so failed the case
of position 297 that queues an NMI before the VP first runs. An interrupt still waits for the
instruction in the shadow, which that host passes, and an NMI that the guest has masked does not
wake the VP, so it is suspended again as before. It makes the cases of `suspend.rs` that queue an
NMI before the VP first runs require that the VP holds it as pending, and rewords the comment on
them. It fixes position 297, which no PR maps yet, so it is not a fixup of a PR yet, and folds into
position 297 when that position goes into one. It changes files that six positions that no PR has
yet, and positions 297 and 299, also change, and it changes no line that a mapped PR adds: it
rewrites seven lines of `apic.rs` and three of `suspend.rs` that position 297 adds, and one of
`suspend.rs` that position 299 adds, and adds the rest. Replaying it on upstream main conflicts in
`apic.rs`, beside the check that position 297 adds, and in `suspend.rs`, which is position 297's
new file. So it follows positions 297 and 299 upstream.

Position 301 has the KVM backend register each RAM slot of a VM without private memory in KVM's
address space 1, which a vCPU uses while in system management mode (SMM), as well as in address
space 0, so that a system management interrupt (SMI) that a guest sends itself enters SMM instead of
ending the VM. A vCPU that takes an SMI saves the state that it interrupts to SMRAM at SMBASE, which
is in RAM, through that address space, and KVM takes the address space from the upper 16 bits of the
slot ID, which `map_region` gave it as a dense slot index, so with no slot in address space 1 KVM
marked the whole VM dead: `KVM_RUN` failed with `EIO`, and so did every later ioctl, as position 285
recorded. `map_region` now sets each userspace-backed slot in both address spaces, `clear_slot`
clears both, and a slot that is unmapped, reused, or moved takes its mirror along, when
`KVM_CAP_MULTI_ADDRESS_SPACE` reports a second address space for the VM, which x86 does where KVM
has SMM built in and the VM has no private memory; a VM with guest_memfd-backed RAM, such as an SNP
VM, is unchanged. It mirrors all of RAM, not the default SMBASE alone, since a guest's handler can
relocate SMBASE. A vCPU can now be in SMM when a save is requested, as well as hold an SMI, and
position 285's refusal holds for both: no element, field, or version of the saved state changes. The
commit adds two ignored tests that need `/dev/kvm` to the `smm` module of position 285, one of
which relocates SMBASE and unmaps, reuses, and moves ranges of RAM, and a KVM-only contract test,
`smm::kvm::a_guest_smi_enters_smm_instead_of_killing_the_vm`, which runs a VP through `run_vp` and
checks that a save refuses it in the guest's handler. It rewrites the module documentation of
`smm.rs` and a paragraph of the Guide's snapshots page that position 285 adds, which said that
mapping memory for SMM would change them, lists the contract test in the Guide's section on the
backend contract tests, where positions 297 and 299 say that only `suspend.rs` runs a VP, and
documents the address space of a slot ID in the `kvm` crate. It also builds the fixture accessor
that positions 297 and 298 build on Windows only on Linux too, where its contract test needs it, and
lets a test choose the size of the fixture's RAM. It changes files that PR01's position 002, PR05's
positions 007 and 008, PR06's position 010, PR09's positions 013 and 014, PR10's positions 018, 021,
024, and 026, PR11's position 030, PR12's positions 032-036, PR13's positions 037, 041, and 046,
PR14's position 049, PR15's position 055, PR17's positions 067-069, 071, 072, and 074, PR18's
positions 077, 079, and 083-085, PR19's positions 087 and 088, PR20's positions 089, 090, 092, and
094, PR21's positions 096 and 101, PR23's positions 105, 108, and 113, PR24's positions 117 and 118,
and 97 positions that no PR has yet also change, and it changes no line that a mapped PR adds. It
rewrites 13 lines of `smm.rs` and 7 of the Guide's snapshots page that position 285 adds, one line
of the Guide's page on the VMM tests that position 297 adds and two that position 299 adds, two
lines of `fixture.rs` that position 297 adds and one that position 298 adds, and two lines each of
`memory.rs` and `fixture.rs` that upstream main has and no commit of the chain changes, and adds the
rest. Replaying it on upstream main conflicts in seven files, where it edits or adds beside what
upstream main lacks: `smm.rs` does not exist there, since position 285 adds it; in `mod.rs` and
`lib.rs`, the `smm_address_space` that it adds follows the `lapic_preemption_timer` that position
248 adds; in the Guide's snapshots page, its paragraph replaces one that position 285 adds, in the
block that positions 071, 235, 265, 269, and 285 write; in the Guide's page on the VMM tests, its
edits sit beside what positions 273, 279, 296, 297, and 299 write; in `fixture.rs`, its module
documentation rewrites what positions 008 and 297 write, and its accessor gate what positions 297
and 298 add; and in `main.rs`, its `mod smm;` sits among the `mod` lines that positions 260, 266,
273, 288, 290, 293, and 297 add. Its contract test also needs the `check_saveable` that position 264
adds, and the rest of its files apply without conflicts. So it follows positions 008, 071, 235, 248,
260, 264, 265, 266, 269, 273, 279, 285, 288, 290, 293, and 296-299, and the positions that they
follow, upstream.

Position 302 has the WHP and MSHV backends save and restore a guest's `IA32_UMWAIT_CONTROL`, which
Linux writes on every CPU at boot where the CPU profile enumerates WAITPKG, as the Alder Lake and
Granite Rapids profiles do. Position 277 gave KVM the `umwait_control` element and had both of the
Microsoft hypervisor's backends clear its capability and refuse its accessors, so a WHP or MSHV
snapshot restored the MSR as 0. The hypervisor keeps the MSR as a register of the VP, as it keeps
the `IA32_SPEC_CTRL` of position 272, so the commit maps `UmwaitControl` to WHP's register, reads
and writes it through the register state on both, and keeps the capability that the guest's CPUID
gives, which MSHV clears for an SNP guest. No element, field, or version of the saved state changes.
It says in the Guide which elements MSHV and WHP hold and why they hold no others: the hypervisor
drops a guest's writes of `IA32_DEBUGCTL` and `IA32_MISC_ENABLE`, raises #GP for the other feature
and power MSRs, and gives the guest no machine-check banks, as a guest showed on WHP and MSHV hosts.
It completes position 277 for WHP and MSHV, which no PR maps yet, so it is not a fixup of a PR yet,
and can fold into position 277 when that position goes into one. It changes files that PR01's
positions 002 and 003, PR05's positions 007 and 008, PR10's positions 021, 022, 025, and 026, PR11's
positions 029 and 030, PR12's positions 033-036, PR13's positions 041 and 046, PR17's positions 071,
072, and 074, PR18's positions 077, 079, and 083-085, PR19's positions 087 and 088, PR20's positions
089, 090, and 092-094, PR21's position 101, PR23's positions 108 and 113, PR24's position 118, and
82 positions that no PR has yet also change, and it changes no line that a mapped PR adds. It
rewrites two lines of the Guide's page on the snapshot format, one of its page on the VMM tests, and
two of its snapshots page that position 279 adds, two of the snapshots page that position 295 adds,
one line of MSHV's `finalize.rs` that position 272 adds, and two lines of `finalize.rs`, three of
MSHV's `vp_state.rs`, two of WHP's `lib.rs`, and three of WHP's `vp_state.rs` that position 277
adds, and adds the rest. Replaying it on upstream main conflicts in seven files, where it edits or
adds beside what upstream main lacks: in the three Guide pages, its edits sit in what positions 279,
295, and 296 write; in MSHV's `finalize.rs` and `vp_state.rs` and WHP's `vp_state.rs`, it rewrites
lines that positions 272 and 277 add; and in WHP's `regs.rs`, its mapping follows the one that
position 272 adds. Its change to WHP's `lib.rs` applies, since upstream main lacks the two lines of
position 277 that it removes. So it follows positions 272, 277, 279, 295, and 296, and the positions
that they follow, upstream.

Position 303 adds an optional VP state element, `bndcfgs`, in field 21 of the saved state, which
holds `IA32_BNDCFGS`, MPX's supervisor configuration, where the guest's CPUID enumerates MPX, as the
Skylake-SP profile does, with a capability that `from_cpuid` derives from `CPUID.(7,0):EBX[14]`.
Position 279 had said that no guest can write the MSR, since KVM had not exposed MPX since Linux
5.6, which is not so: Linux 5.6 dropped MPX for the kernel's own processes but still enables MPX's
state components for KVM's guests, and `vmx_set_msr` takes a guest's write where its CPUID has MPX.
KVM reads and writes the MSR as `0xd90`, which it lists exactly where it exposes MPX, so the
capability follows the list, as `umwait_control`'s does; MSHV and WHP read and write it as a
register; and OpenHCL leaves the capability clear and its accessors unimplemented. A saved state of
an earlier build lacks the field, so its restore leaves the MSR at 0, as before, and an earlier
build ignores it, so no version changes. The commit adds the field's wire bytes and both directions
of a build without it to the tests of `vp.rs`, the bit, its leaf gating, and the pinned profiles
that have it, KVM's gating, a KVM test of the MSR where the host lists it, a contract test,
`bndcfgs_survives_a_save_and_restore`, which runs on a host with MPX, and the element to
`refuses_unsupported_cpu_state`. It changes files that PR01's position 002, PR05's positions
007-009, PR10's positions 021-026, PR11's positions 029 and 030, PR12's positions 033-035, PR13's
positions 041 and 046, PR17's positions 071, 072, and 074, PR18's positions 077, 079, and 083-085,
PR19's positions 087 and 088, PR20's positions 089, 090, 092, and 094, PR21's position 101, PR23's
positions 108 and 113, PR24's position 118, and 95 positions that no PR has yet also change, and it
changes no line that a mapped PR adds. It rewrites 10 lines of the Guide's page on the snapshot
format that position 279 adds and eight that position 302 adds, two lines of the Guide's page on the
VMM tests and five of its snapshots page that position 302 adds, two lines of OpenHCL's `lib.rs` and
six of KVM's `msr_state.rs` that position 277 adds, two lines of MSHV's `finalize.rs` that position
302 adds, one line of the contract tests' `msr_state.rs` that position 278 adds, and one of
`unsupported_state.rs` that position 296 adds, and adds the rest. Replaying it on upstream main
conflicts in every file that it changes but `x86defs`'s `lib.rs`, whose constant applies, where it
edits or adds beside what upstream main lacks: what positions 272, 277, 278, 283, 296, and 302 add,
and in the Guide what position 279 writes. So it follows positions 272, 277-279, 283, 296, and 302,
and the positions that they follow, upstream.

Position 304 adds two optional VP state elements for the MSRs that KVM on an AMD host keeps for
every guest: `svm_msrs`, in field 22, which holds `VM_CR` and `VM_HSAVE_PA`, whatever the guest's
CPUID says, though no CPU profile enumerates SVM, and `osvw`, in field 23, which holds
`OSVW_ID_LENGTH` and `OSVW_STATUS` where the guest's CPUID enumerates OSVW, as every AMD profile
does. KVM lists `VM_HSAVE_PA` and not `VM_CR`, but lists `VM_HSAVE_PA` wherever the host can read
it, as an Intel host under Hyper-V can, so `svm_msrs` also needs KVM to run on SVM, as the vendor of
its supported CPUID shows, and `osvw` needs that CPUID to enumerate OSVW, since a recent kernel
keeps a vCPU's features within KVM's own. A restore refuses a `VM_HSAVE_PA` that no guest can write,
or a `VM_CR` that sets `SVMDIS` while the restored `EFER.SVME` is set, which KVM would refuse only
after it set `SVMDIS` and `LOCK`, before it writes either SVM MSR, and a VM reset puts back the OSVW
values that KVM gave the vCPU at bind, which the backend now records, rather than their reset value.
MSHV, WHP, and OpenHCL clear both capabilities, and the elements need no version, as position 303's
needs none. The commit adds the elements, the capabilities and their tests, KVM's gating and its
tests, KVM tests that its host keeps the SVM MSRs exactly where the capability says, and of both
pairs where KVM runs on SVM or supports OSVW, three contract tests, which run on an AMD host, and
the elements to `refuses_unsupported_cpu_state`. It rewrites position 279's remark in the Guide that
the SVM MSRs need no element. It changes files that PR01's positions 002 and 003, PR05's positions
007-009, PR10's positions 021-026, PR11's positions 029 and 030, PR12's positions 033-036, PR13's
positions 041 and 046, PR17's positions 071, 072, and 074, PR18's positions 077, 079, and 083-085,
PR19's positions 087 and 088, PR20's positions 089, 090, and 092-094, PR21's position 101, PR23's
positions 108 and 113, PR24's position 118, and 104 positions that no PR has yet also change, and it
changes no line that a mapped PR adds. It rewrites 15 lines of the Guide's page on the snapshot
format and two of its page on the VMM tests that position 279 adds, six lines of the page on the
snapshot format that position 303 adds, one line of the snapshots page that position 302 adds, one
line each of OpenHCL's `lib.rs`, the contract tests' `msr_state.rs`, and `unsupported_state.rs`, and
four of KVM's `msr_state.rs`, that position 303 adds, seven lines of `virt`'s `x86/mod.rs` that
position 272 adds, one line of KVM's `mod.rs` and 10 of its `msr_state.rs` that position 277 adds,
and one line of `x86/mod.rs` and two of KVM's `mod.rs` that upstream main has and no commit of the
chain changes, and adds the rest. Replaying it on upstream main conflicts in every file that it
changes but `x86defs`'s `lib.rs` and WHP's `lib.rs`, where it edits or adds beside what upstream
main lacks: what positions 128, 248, 272, 277, 278, 283, 296, 302, and 303 add, and in the Guide
what position 279 writes. So it follows positions 128, 248, 272, 277-279, 283, 296, 302, and 303,
and the positions that they follow, upstream.

Position 305 has the MSHV and WHP backends refuse to save a VP whose Hyper-V registers hold a
pending interruption or event that the saved activity cannot hold, instead of panicking: a software
interrupt (`INT n`), a privileged software exception (`INT1`), or a software exception (`INT3` or
`INTO`), which the hypervisor holds pending, with the length of the instruction that raised it,
while an intercept interrupts its delivery, an interruption of a type that Hyper-V does not define,
an event other than an exception or an extint, and a vector beyond 8 bits. The activity's
`HvRegisterState::set_values`, which upstream main has, panicked on each, so a capture at such a
moment panicked instead of failing. The commit adds `Activity::from_hv_values`, which converts the
four registers or names what it cannot hold, and the activity getters of MSHV and WHP use it, so a
save fails with `UnsupportedState`, and a guest-requested capture rolls back. No element, field, or
version of the saved state changes. It changes files that PR05's position 008, PR10's positions 022,
024, and 026, PR11's positions 029 and 030, PR12's positions 033-035, PR13's positions 041 and 046,
PR17's positions 071 and 074, PR18's positions 077, 079, and 083-085, PR19's positions 087 and 088,
PR20's positions 089, 090, 092, and 094, PR21's position 101, PR23's positions 108 and 113, PR24's
position 118, and 81 positions that no PR has yet also change, and it changes no line that a mapped
PR adds: it rewrites 52 lines of `vp.rs`, four of MSHV's `vp_state.rs`, two of WHP's `apic.rs`, and
three of WHP's `vp_state.rs` that upstream main has and no commit of the chain changes, and adds the
rest. Replaying it on upstream main conflicts in three files, where it edits or adds beside what
upstream main lacks: in the Guide's page on the snapshot format and its snapshots page, its
paragraphs sit beside what positions 071, 285, 289, 292, and 301 write, and in `vp.rs`, the
conversion that it rewrites builds the `nmi_pending_count` and `interrupt_shadow_kind` that
positions 287 and 291 add, and its tests follow one that position 291 adds. Its changes to
`virt_mshv` and `virt_whp` apply without conflicts, but call the conversion that it adds to `vp.rs`.
So it follows positions 071, 285, 287, 289, 291, 292, and 301, and the positions that they follow,
upstream.

Position 306 keeps the queue-pair lifecycle that a virtio-net NIC built for save and restore, as
each microVM NIC is, saves through a failed start of its queues. Since PR13's position 046, such a
NIC saves how many of its queues its last stop or restore stopped, and a restore checks that count
against the queue progress that the transport saves; but `prepare_queue_start` cleared the count as
each pair started, and a pair whose second queue failed to build stayed half open, so that the stop
that undid the start recorded a single queue. After a restored or paused NIC's start failed, as when
its endpoint could not create its queues, which position 207 makes fail the start, a save therefore
wrote a snapshot whose restore failed with "virtio-net queue lifecycle does not match transport
state", although a device whose start fails is to keep the private state that it saves in step with
the queue progress that the transport keeps. No microVM flow saves between a failed start and the
next start today, since a microVM refuses a host save and its guest cannot request a capture while
the VM is stopped, so the commit fixes the contract rather than a failure that NVX met.
`prepare_queue_start` now only checks the saved state, the next stop of a started pair records the
count again, and the start drops a pair whose second queue does not build before it fails. No
element, field, or version of the saved state changes. The commit adds a test that fails a restored
NIC's start, once in the endpoint's queue creation and once in its TX queue's, and requires that the
transport's stop of the started queue gets no progress, that the restore's validation accepts a save
of the stopped NIC with the restored queue progress, which it refused, and that the next start
resumes the queues where the restore left them; it fails without either half of the change. It also
says in the doc comment of `VirtioDevice::start_queue`, which positions 209 and 213 wrote, that a
device whose start fails keeps the private state that it saves, and in the Guide's snapshots page
that a save of the paused VM saves the restored state. It changes files that PR11's position 030,
PR12's positions 033-035, PR13's positions 037, 038, 041, and 046, PR16's position 063, PR17's
positions 071 and 074, PR18's positions 077, 079, 084, and 085, PR19's positions 087 and 088, PR20's
positions 089, 090, 092, and 094, PR21's position 100, PR23's positions 108 and 113, PR24's position
118, and 67 positions that no PR has yet also change, and it changes eight lines that a mapped PR
adds: it rewrites seven lines of `prepare_queue_start` in `saved_state.rs`, and the line of the test
harness that builds its NIC for save and restore, which PR13's position 046 adds. It also rewrites
12 lines of the activation tests that position 207 adds, one line of the Guide's snapshots page that
position 213 adds, and 11 lines of `lib.rs` that upstream main has and no commit of the chain
changes, and adds the rest. Replaying it on upstream main conflicts in five of its six files:
upstream main lacks `saved_state.rs` and the test harness, which position 046 adds, and the
activation tests of position 207, and its sentences extend the doc comment that positions 209 and
213 write and the Guide's paragraph that position 213 writes; its change to `lib.rs` applies without
conflicts. So it follows PR13's position 046 and positions 207, 209, and 213 upstream. The failed
start that it fixes is the one that positions 212 and 213 define, which keeps a restored device's
queue progress and state for a later start or save, and which position 207 makes an endpoint's
failure cause. No PR maps those positions yet, so it is not a fixup of a PR yet, and folds into
positions 212 and 213 when they go into one.

Position 307 has `pci_core`'s MSI-X capability program each vector's delivery as it restores it,
where it restored the vector table without enabling the vectors' MSI interrupts, so that a restored
PCI device delivered none of its MSI-X interrupts until the guest wrote its vector table again. A
microVM has no PCI device, so only OpenVMM's other VMs met this. A restore now enables the interrupt
of each enabled and unmasked vector at the address and data that it restores, and disables the
others, without delivering anything, and a save records the pending bit that the guest's read of the
pending bits would return, counting the interrupt that a masked vector holds on its route's event or
in its MSI interrupt, without taking that interrupt, so that the saved state keeps it for the
vector's unmask and the save changes nothing that the guest sees. No element, field, or version of
the saved state changes. The commit adds two tests of the capability, one that saves a vector that
is unmasked and one that is masked with an interrupt pending, restores them into another capability,
and requires that the restore delivers nothing, that the unmasked vector delivers to its message,
and that the masked one delivers as the guest unmasks it, and one that requires that a save leaves
the interrupts that masked vectors hold where they are, so that the saved capability delivers a
route's interrupt once, on the route, although another arrived after the save, and a virtio test, in
the new `restore_interrupt.rs`, that a restored PCI device delivers its queue's interrupt to the
vector that the guest programmed before the save; they fail without the change. It changes
`msix.rs`, which no commit of the chain changes, and the virtio tests' `tests.rs`, which declares
the new module and which PR13's position 037, PR16's position 063, and positions 177, 212, 213, and
233 also change, and it changes no line that a mapped PR adds: it rewrites one line of `msix.rs`
that upstream main has, and adds the rest. Replaying it on upstream main conflicts only in
`tests.rs`, among the modules that the chain declares there; its change to `pci_core` applies
without conflicts, and `cargo nextest run -p pci_core` passes with it on upstream main, but its
virtio test confirms the start that position 213 adds. So its `pci_core` change can go upstream on
its own, and its virtio test follows position 213 upstream.

Position 308 has the virtio transports hold their interrupt line from a restore until the device
starts, where the restore drove it while the interrupt controller that it reaches could still be
restoring: the virtio-mmio transport drove a legacy line to the restored status and a shared-status
line low, and the PCI transport drove its INTx line to the restored status. A line set drops a level
that reaches an interrupt controller while that controller restores, and the IOAPIC restores the
line levels that it saved, so a level that a transport drove during the restore could be lost, or
could change the level that a controller restored. A restore now restores the status without driving
the line, a change of the status during the hold, such as a configuration change, drives none
either, and the device's start drives a legacy virtio-mmio line or an INTx line to the restored
status before it starts the queues; a reset ends the hold. A shared-status line, which every microVM
slot uses, only pulses as the device raises the status, and the pulse for the status that the
restored word reports went out before the save, so neither the restore nor the start drives it. A
restore sends no MSI-X message either, since the queues' messages begin with the queues. No element,
field, or version of the saved state changes. The commit adds unit tests of the virtio-mmio
interrupt state, which require that a restore drives no legacy line until the start releases it,
once, and drives no shared-status line, and tests in position 307's `restore_interrupt.rs` that a
restored virtio-mmio device and a restored PCI device with INTx drive their line only as they start;
they fail without the change. It also says in the Guide's snapshots page that no virtio device
drives its interrupt line during a restore. It changes files that PR11's position 030, PR12's
positions 033-035, PR13's positions 037, 038, 041, and 046, PR16's positions 061 and 063, PR17's
positions 071 and 074, PR18's positions 077, 079, 084, and 085, PR19's positions 087 and 088, PR20's
positions 089, 090, 092, and 094, PR23's positions 108 and 113, PR24's position 118, positions 306
and 307, and 68 other positions that no PR has yet also change, and it changes four lines that a
mapped PR adds: it rewrites one line of `mmio.rs` and three lines of the virtio-mmio interrupt
module that PR16's position 063 adds, into which position 063 moved upstream's drive of the line at
restore. It fixes no defect of position 063, whose restore drove the line as upstream main's
transports do, so it is not a fixup of PR16. It also rewrites one line of the Guide's snapshots page
that position 209 adds, five that position 213 adds, and one that position 306 adds, five lines of
`restore_interrupt.rs` that position 307 adds, and two lines of `mmio.rs` and six of `pci.rs` that
upstream main has, and adds the rest. Replaying it on upstream main conflicts in three of its seven
files: upstream main lacks the virtio-mmio interrupt module, which position 063 adds, and position
307's test module, and its sentences of the Guide sit in the paragraph that positions 213 and 306
write. Its changes to `mmio.rs` and to the PCI transport, with the PCI transport's new interrupt
module, apply without conflicts, and with its PCI change alone, `cargo check -p virtio
--all-targets` and `cargo nextest run -p virtio` pass on upstream main. So its virtio-mmio change
follows PR16's position 063 upstream, and its PCI change can go upstream on its own, with a test
outside position 307's module.

Position 309 tests that the awaited restore of a microVM virtio-fs device's transport reopens the
host files that the guest held open: a virtio-mmio transport over a device that holds a file open
saves, the file is removed from the host, and the restore of another transport over the share fails
because the file cannot be reopened, before any queue starts. The filesystem's own tests restore
names that are gone, but none went through the transport's restore. The commit adds only the test,
to the microVM virtio-fs device module, which PR18's positions 081, 083, and 085, PR22's position
102, and positions 158-160, 164, and 175 also change, and changes no line. Replaying it on upstream
main conflicts in that module, which upstream main lacks, since PR18's position 081 adds it. So it
follows PR18's position 081, and the positions that it follows, upstream.

Position 310 tests that a remote chipset device whose worker has ended does not start: the
confirmation of its start, and the closing and the opening of its input gate, fail instead of
succeeding without the device. The commit adds only the test, to the tests of the device proxy,
which PR09's position 014 and positions 213 and 233 also change, and changes no line. Replaying it
on upstream main conflicts in the proxy, whose tests position 213 extends; the test also needs the
start confirmation of position 213 and the input gate of position 014. So it follows PR09's position
014 and positions 213 and 233, and the positions that they follow, upstream.

Position 311 corrects what positions 213 and 306 say that a failed start keeps.
`ChangeDeviceState::confirm_start`, which position 213 adds, said that a device whose start fails
keeps the state that it had before the start, so that stopping it leaves it as it was; position 306
said in the doc comment of `VirtioDevice::start_queue` that such a device keeps the private state
that it saves as it was before the start, and in the Guide's snapshots page that a save of the
paused VM saves the restored state that, as position 213 wrote there, the devices keep. A virtio
device whose second queue fails to start can have started its first one, which completes the
requests that were pending in it before the VM stops the device again, and the stop keeps that
queue's progress, as position 213's tests of a failed confirmation require. So a failed start leaves
the state that its stop records, which a save saves: the confirmation keeps the vCPUs from running,
but does not undo a device's I/O. The commit says so in the three places and changes no code. It
changes files that PR09's position 014, PR10's position 015, PR11's position 030, PR12's positions
033-035, PR13's positions 037, 038, 041, and 046, PR16's position 063, PR17's positions 071 and 074,
PR18's positions 077, 079, 084, and 085, PR19's positions 087 and 088, PR20's positions 089, 090,
092, and 094, PR23's positions 108 and 113, PR24's position 118, positions 306 and 308, and 67 other
positions that no PR has yet also change, and it changes no line that a mapped PR adds: it rewrites
two lines of `device_state.rs` that position 213 adds, three of `device.rs` that position 306 adds,
and three of the Guide's snapshots page that position 308 adds. Replaying it on upstream main
conflicts in all three of its files, since upstream main lacks the start confirmation of position
213 and the sentences of positions 306 and 308 that it rewrites. It corrects positions 213, 306, and
308, which no PR maps yet, so it is not a fixup of a PR yet, and folds into them when they go into
one.

Position 312 has the rollback of a failed capture, which PR17's position 067 adds to the VM worker,
refuse a VM that holds no snapshot boundary before it starts the VM's state units, so that a paused
VM runs again only through a resume, which first measures the time ABI's guest TSC offset and, for
a restored VM, gates its host input, publishes its readiness, and releases its vCPUs. The rollback
had started the state units of a paused VM and only then failed, for want of a boundary to release,
so a cold-booted VM ran without the offset, and a restored VM whose start had failed started its
devices without its input gate while its vCPUs stayed stopped. The VM controller of PR17's position
071 sends the rollback only after a quiesce at a boundary, so only another caller, such as a VMM
test, could send it there, as position 235 found of the quiesce and the boundary's release. The
commit extends the snapshot guide's paragraph on the order of the snapshot RPCs, which position 235
adds, and adds a VMM test beside position 235's tests, with the helpers of positions 060, 234, and
235. It changes files that PR11's position 030, PR12's positions 033-035, PR13's positions 041 and
046, PR15's positions 051, 057, 058, and 060, PR16's positions 062 and 064, PR17's positions 067,
068, 070-072, 074, and 075, PR18's positions 077-079, 082, 084, and 085, PR19's positions 086-088,
PR20's positions 089, 090, and 092-094, PR23's positions 108, 110, and 113, PR24's position 118, and
78 positions that no PR has yet also change, and it changes no line that a mapped PR adds: it
rewrites one line of the guide that position 235 adds, and adds the rest, in the rollback of
position 067 and beside the tests of position 235. Replaying it on upstream main conflicts in all
three of its files, where it edits or adds beside what upstream main lacks: the VM worker's microVM
module does not exist there, since PR15's position 051 adds it, nor do the microVM VMM tests, which
position 060 adds, and in the guide its sentence extends the paragraph of position 235. So it
follows positions 067, 234, and 235, and the positions that they follow, upstream.

Position 313 records the manifest's field map, which microsoft/nvx#1210 settles with upstream, in
the rule of the Guide's snapshot format page for extending the manifest: a table says that fields
1-7 of `SnapshotManifest` are those of upstream's manifest, version 1, 8-15 this format's, 16 and
17 free, 18 the `snapshot_id` of upstream's SaveVM, which microsoft/openvmm#4553 merged, 19 and 20
reserved upstream for `compatibility_bundle_digest` and `memory_artifacts`, and 21 and later free,
and the rule asks for the next free field number, never 19 or 20. It also says that the messages of
the device state in `state.bin` number their fields on their own, such as `virt.x86.VpSavedState`,
whose fields 21-23 positions 303 and 304 add. It changes no code. It changes a file that PR11's
positions 029 and 030, PR12's positions 033 and 034, PR13's positions 041 and 046, PR17's positions
071 and 074, PR18's positions 079 and 083, PR19's positions 087 and 088, PR21's position 101, and
27 positions that no PR has yet also change, and it changes no line that a mapped PR adds: it
rewrites one line of the rule that upstream main has and no commit of the chain changes, and three
that position 226 adds, and adds the rest. Replaying it on upstream main conflicts in the Guide's
page, whose rule lacks the reservation that position 226 adds. So it follows position 226, and the
positions that it follows, upstream.

Position 314 has restore decode a manifest's snapshot ID, field 18, alone, after the version and
the magic, which position 225 decodes alone, and refuse a field 18 of another wire type or length
than a GUID of 16 bytes as malformed, naming the ID and its field, before it decodes the rest, where
the decode of the whole manifest refused it only as a manifest that failed to decode. A manifest
without the field still reads with the zero ID, which stands for none, and a manifest of another
version still gets `E_SNAPSHOT_VERSION` whatever its field 18 holds. It adds a test of a malformed
ID and one of a missing ID to the restore tests, and says in the Guide's snapshot format page how
restore reads them, and that the ID, which the v6.1 rebase gave the manifest from
microsoft/openvmm#4553, needed no new version. It changes files that PR11's positions 029 and 030,
PR12's positions 033 and 034, PR13's positions 041 and 046, PR17's positions 069, 071, and 074,
PR18's positions 079 and 083, PR19's positions 087 and 088, PR21's position 101, and 33 positions
that no PR has yet, position 313 among them, also change, and it changes no line that a mapped PR
adds: it rewrites one line of the Guide's snapshot format page that position 225 adds, and adds the
rest. Replaying it on upstream main conflicts in its three files: `format.rs` does not exist there,
since PR11's position 029 adds it; its tests in `restore.rs` follow those of position 225 and use
its `read_errors_with_manifest`; and its sentences in the Guide's page follow one that position 225
writes. So it follows position 225, and the positions that it follows, upstream.

Position 315 makes `offset_of`, which `vmotherboard` implements for the port I/O and MMIO regions
of chipset devices, find the offsets in a region whose last byte is the top of its address space,
such as 2 ports at `0xfffe` or an MMIO region that ends at `u64::MAX` (microsoft/nvx#1384). It
compared the address with the region's exclusive end, which does not fit the address type there, so
the first lookup in such a region panicked where overflow checks are on and otherwise found no
offset. It now compares the address's offset from the base with the region's length, in code that
upstream main has, and adds two tests beside the tests of the region's mapping, in a file that
PR10's position 015 also changes, none of the code that it changes. It changes nothing that a mapped
PR adds. No device of the microVM looks up an offset in its regions, so the microVM behaves as it
did. Replaying it on upstream main applies without conflicts, and `vmotherboard` type-checks there
with its tests, which pass, so it can go upstream on its own.

Position 316 has `ArcMutexChipsetServices::finalize`, which ends the add of a chipset device in
`vmotherboard`, make the device the chipset's EOI handler and PIC and a target of the line sets that
it asked for only once its state unit has spawned, the last step of the add that can fail
(microsoft/nvx#1387). It made them before the spawn, so an add that failed there, with `NameInUse`
when another unit had the device's name, left the device wired: the line sets went on driving it,
though no state unit started, stopped, saved, or restored it, and an interrupt controller added in
its place failed with `EoiHandlerAlreadySet`. It now checks first that the EOI handler and the PIC
are free and that the device can be a line target, then spawns the unit, and then wires the device
in the same order as before, so a failed add leaves the device none of them, and a successful one
leaves the chipset as before. It changes code that upstream main has, and adds two tests in a new
`tests` module, in a file that PR10's position 015 and position 315 also change, none of the code
that either changes. It changes nothing that a mapped PR adds. Every caller of the add, the
microVM's device loader among them, returns its error, so the microVM behaves as it did. Upstream
main has the defect: the tests fail there without the fix. Replaying it on upstream main applies
without conflicts, and `vmotherboard` type-checks there with its tests, which pass, so it can go
upstream on its own.

Position 317 has the payload reader and writer of `virtiofs`, which read a FUSE request from the
device-readable buffers of a descriptor chain and write its reply into the device-writable ones,
skip empty buffers (microsoft/nvx#1621), which a driver may place anywhere in a chain, since the
virtio specification forbids a device to assume any arrangement of descriptors. They moved to the
next buffer of their direction whether it was empty or not, and once per call, so a call that
reached an empty buffer in the middle of a request, or of the space for its reply, read or wrote
nothing, which ended the request or the reply there: a request whose header such a buffer split, or
that two empty buffers preceded, failed to parse, so the device signalled corruption and completed
the chain with no reply, arguments, a name, or data after such a buffer failed with `EINVAL` or
`EIO`, and a reply that reached such a buffer completed the chain with no bytes written. It changes
code that upstream main has, in a file that PR06's position 010 and PR18's position 081 also change,
none of the code that either changes, and adds three tests, and the harness helpers that they use,
in a file that position 167 also changes, none of the code that it changes. It changes nothing that
a mapped PR adds. NVX's Linux guest posts no empty buffer, since its virtio-fs driver adds a buffer
for arguments or data only when they hold some, so the microVM behaves as it did. Upstream main has
the defect: the tests fail there without the fix. Replaying it on upstream main applies without
conflicts, and `virtiofs` type-checks there with its tests, which pass, so it can go upstream on its
own.

Position 318 has `RequestReader::read_count`, with which the `fuse` crate's request decoder reads
the data of a `FUSE_WRITE`, `FUSE_SETXATTR`, or `FUSE_IOCTL` request, check the size that the
request declares against the data that it carries before it allocates the data (microsoft/nvx#1596).
It allocated and zeroed the declared size first, so an 85-byte `FUSE_WRITE` whose size is
`0xFFFFFFFF` made the decoder ask for 4 GiB, which aborts the VMM under a memory or address-space
limit, before the short read failed with `EINVAL`. It now fails with the same `EINVAL` before it
allocates, so the decoder allocates at most the data that the request carries, and a request that
carries its data decodes as before. It changes code that upstream main has, and adds two tests, with
`test_with_tracing` as a dev-dependency on every platform, in files that no earlier chain commit
changes. It changes nothing that a mapped PR adds. NVX's Linux guest sends the data that it
declares, so the microVM behaves as it did. Upstream main has the defect: without the fix,
`read_count_past_remaining_data_is_einval` fails there. Replaying it on upstream main applies
without conflicts, and `fuse` type-checks there with its tests, which pass, so it can go upstream on
its own.

Position 319 has `Request::read_operation`, with which the `fuse` crate's request decoder decodes a
request's operation, refuse a request whose header declares a length shorter than the header
(microsoft/nvx#1607), as it refuses one whose header declares more than the request carries. It
checked only that upper bound, so a `FUSE_INIT` whose length is below 40 underflowed the size of its
payload: where overflow checks are on, that panicked and aborted the VMM, and otherwise the length
wrapped, and the request decoded as `FUSE_INIT` with a payload read past its declared length, while
any other request with such a length decoded as if its length were valid. It now decodes such a
request as `Invalid`, which the session fails with `EIO`, and a request whose length holds its
header decodes as before. It changes code that upstream main has, whose subtraction of the header's
size came with the extended `FUSE_INIT` payload in microsoft/openvmm#3385, and adds two tests, in a
file that position 318 also changes, none of the code that it changes. It changes nothing that a
mapped PR adds. NVX's Linux guest sets each length to the header plus the arguments, so the microVM
behaves as it did. Upstream main has the defect: the tests fail there without the fix. Replaying it
on upstream main applies without conflicts, and `fuse` type-checks there with its tests, which pass,
so it can go upstream on its own.

Position 320 has `fuse_set_attr_to_lxutil`, with which `virtiofs` converts the attributes that a
`SETATTR` request sets, refuse with `EINVAL`, as `utimensat` does, a time whose nanoseconds are a
second or more (microsoft/nvx#1597). It passed the request's seconds and nanoseconds to
`Duration::new`, which carries whole seconds of nanoseconds into the seconds and panics when that
overflows, so a time of `u64::MAX` seconds and a second or more of nanoseconds aborted the VMM,
while with fewer seconds such nanoseconds carried silently into the seconds. Both `SETATTR`
handlers, on the node and on an open handle, now fail before they change any attribute, and a time
set to the current time or left unchanged ignores its fields as before. It changes code that
upstream main has, from its initial import: the conversion, in a file that no earlier chain commit
changes, and the handlers' calls of it, in `file.rs`, which PR06's position 010 and PR18's position
083 also change, and in `inode.rs`, which those, PR13's position 048, PR18's position 081, PR22's
position 102, PR27's position 126, and positions 160 and 165 also change, none of the code that
they change. It adds three tests, one in `lib.rs`, which PR06's position 010, PR13's position 048,
PR18's positions 081 and 083, PR27's position 126, and positions 158, 160, and 164 also change,
none of the code that they change. It changes nothing that a mapped PR adds. NVX's Linux guest
never sends such nanoseconds, since its kernel refuses them as `utimensat` does, so the microVM
behaves as it did. Upstream main has the defect, in the same calls of `Duration::new`. Replaying it
on upstream main conflicts in `inode.rs`, where the call that it changes follows the
`validate_confined_object()` check that PR27's position 126 adds, so it cannot go upstream on its
own as it is.

Position 321 has the `fuse` crate's session reply to a successful `FUSE_IOCTL` with the output that
the file system's `Fuse::ioctl` returns (microsoft/nvx#1615). It sent the request's own input after
`fuse_ioctl_out` in its place, so the reply never carried that output, and carried as many bytes as
the request's input, which can exceed the room for output that the request allows. The output, which
the session already bounds by that room, now follows `fuse_ioctl_out`, and the documentation of
`Fuse::ioctl` says what it receives and returns. It changes code that upstream main has, in
`session.rs` and `lib.rs`, which PR13's position 047 also changes, none of the code that it changes,
and adds two tests, and the session's test harness that they use, in new files that it declares in
the session's test module next to the `saved_state` tests of PR13's position 047. It changes nothing
that a mapped PR adds. NVX's virtio-fs servers do not implement `ioctl`, and fail `FUSE_IOCTL` with
`ENOSYS` before any reply data, so the microVM behaves as it did. Upstream main has the defect: the
tests fail there without the fix. Replaying it on upstream main conflicts in `session.rs`, whose
test module lacks the `saved_state` tests of PR13's position 047, and on Windows its tests need
position 318's development dependency on `test_with_tracing`, which upstream main has only on Unix,
so it cannot go upstream on its own as it is. With the conflict resolved and that dependency, `fuse`
type-checks there with its tests, which pass.

Position 322 has `ReplySender::send_arg_data`, with which the `fuse` crate sends a reply of an
argument and data, count the data in the length that the reply's header gives (microsoft/nvx#1616),
as `send_data` and `send_string` count theirs. It left the data out, so a reply with data claimed
fewer bytes than it carried: the reply of a `fuse_ioctl_out` and 4 bytes of ioctl output said 32
bytes instead of 36, which Linux refuses on `/dev/fuse`, and from which its virtio-fs driver takes
the length of the ioctl's output, so it dropped the output. The session's `FUSE_IOCTL` reply, which
carries the file system's output since position 321, is its only caller. It changes code that
upstream main has, in `reply.rs`, which no earlier chain commit changes, adds a test in a new file,
and has the session's test harness that position 321 adds check the length of every reply that it
returns, so position 321's tests check the length of the `FUSE_IOCTL` reply too. It changes nothing
that a mapped PR adds. NVX's virtio-fs servers do not implement `ioctl`, and fail `FUSE_IOCTL` with
`ENOSYS` before any reply data, so the microVM behaves as it did. Upstream main has the defect: the
new test fails there without the fix. Replaying it on upstream main conflicts in the session's test
harness, which upstream main lacks, and on Windows its test needs position 318's development
dependency on `test_with_tracing`, which upstream main has only on Unix, so it cannot go upstream on
its own as it is. Without the harness's change and with that dependency, `fuse` type-checks there
with its tests, which pass.

Position 323 has `ReplySender::send_error`, with which the `fuse` crate sends an error reply, send a
code only when a reply can carry it, 0 for an empty reply or 1 through 511, and otherwise log a
rate-limited error and fail the request with `EIO` (microsoft/nvx#1625). It negated the code
unchecked, though the session passes it the code of any `lx::Error` that a file system fails with:
negating `i32::MIN` panicked where overflow checks are on, which aborts the VMM, and a release build
replied with the error `i32::MIN`. Linux refuses a reply whose error is positive or `-512`
(`-ERESTARTSYS`) or less, which every code outside that range gave: on `/dev/fuse` its request
stays pending, and its virtio-fs driver fails the request with `EIO`. It changes code that upstream
main has, in `reply.rs`, which position 322 also changes, and documents the range on the `Fuse`
trait in `lib.rs`, which PR13's position 047 and position 321 also change, none of the code that
they change. It adds two tests to the file that position 322 adds, and one that uses the session's
test harness of position 321, in a new file that it declares in the session's test module. It
changes nothing that a mapped PR adds. NVX's virtio-fs servers fail with host errno values and `lx`
constants below 512, so the microVM behaves as it did. Upstream main has the defect: without the
fix, the test that sends `i32::MIN` panics there. Replaying it on upstream main conflicts in
`reply/tests.rs`, which upstream main lacks, and in `session.rs`, whose test module lacks the tests
of PR13's position 047 and position 321, and on Windows its tests need position 318's development
dependency on `test_with_tracing`, which upstream main has only on Unix, so it cannot go upstream on
its own as it is. With its change of `reply.rs`, its tests of it, and that dependency, `fuse`
type-checks there with its tests, which pass.

Position 324 has `make_header`, which builds the header of every reply that the `fuse` crate's
`ReplySender` helpers send, fail with `InvalidInput` for a reply longer than `u32::MAX` bytes, which
the `u32` `len` of its header cannot describe, and the helpers return that error before they send
anything (microsoft/nvx#1628). It converted the length with `try_into().unwrap()`, so such a reply
panicked, which aborts the VMM: a file system's reply to a read, directory, or extended-attribute
request of `u32::MAX` bytes that it honors in full, or with nearly `u32::MAX` bytes of ioctl output
or of a symbolic link's target. The session logs a reply that it cannot send with a rate-limited
error, so such a request now gets no reply. It changes code that upstream main has, in `reply.rs`,
which positions 322 and 323 also change, among it the line of `send_arg_data` whose length position
322 corrects, and adds a test to the file that position 322 adds, and one that uses the session's
test harness of position 321, in a new file that it declares in the session's test module. It
changes nothing that a mapped PR adds. NVX's virtio-fs servers send no reply that long: they refuse
buffers of more than 1 MiB, return the targets of the host's links, list only their shares in an
aggregate's root, and do not implement `ioctl`, so the microVM behaves as it did. Upstream main has
the defect, in the same `make_header`. Replaying it on upstream main conflicts in `reply.rs`, whose
`send_arg_data` and `send_error` lack the changes of positions 322 and 323, in `reply/tests.rs`,
which upstream main lacks, and in `session.rs`, whose test module lacks the tests of PR13's
position 047 and positions 321 and 323, so it cannot go upstream on its own as it is.

Position 325 has the session's test harness, which position 321 adds, keep every reply that the
session sends to a request, and fail the request unless it gets exactly one. Each request that the
tests send through it should get one reply: the harness returns the reply, so the tests never send
it `FUSE_FORGET` or `FUSE_BATCH_FORGET`, which get none. Its `dispatch_request` read the reply
through `CapturingSender`, which keeps only the last buffer sent, so a session that sent a reply's
data and then an error reply to the same request passed. Each of those requests gets one reply, so
the tests pass as before. It changes only that harness, which position 322 also changes, and
nothing that a mapped PR adds or that the microVM runs. Upstream main lacks the harness, so it
cannot go upstream on its own.

Position 326 has `Session::send_max_size`, which sends the replies to `FUSE_READ`, `FUSE_READDIR`,
`FUSE_READDIRPLUS`, `FUSE_GETXATTR`, and `FUSE_LISTXATTR`, fail a file system's reply that is longer
than its request allows with `EIO`, as the Linux FUSE driver does, and log it with a rate-limited
error (microsoft/nvx#1629). It asserted the bound, so a file system that returned even one byte
more panicked, which aborts the VMM. It changes code that upstream main has, `send_max_size` in
`session.rs`, which no earlier chain commit changes, and documents the bound on the five `Fuse`
methods in `lib.rs`, which PR13's position 047 and positions 321 and 323 also change, none of the
lines that they change. Its tests, which use the session's test harness of positions 321 and 325,
are in a new file that it declares in the session's test module. It changes nothing that a mapped
PR adds. NVX's virtio-fs servers send no such reply: `VirtioFs` sizes its read, directory, and
extended-attribute buffers from the request, and `SectionFs` and the dormant microVM file system
implement none of these requests, so the microVM behaves as it did. Upstream main has the defect,
in the same `send_max_size`. Replaying it on upstream main conflicts in `session.rs`, whose test
module lacks the tests of PR13's position 047 and positions 321, 323, and 324, and its tests need
the harness, which upstream main lacks, so it cannot go upstream on its own as it is.

Position 327 has `Session::write_out` build the replies to `FUSE_WRITE` and `FUSE_COPY_FILE_RANGE`,
and fail with `EIO` a byte count from the file system that the `u32` `size` of their
`fuse_write_out` cannot hold, which it logs with a rate-limited error (microsoft/nvx#1631). The
session converted the count with `try_into().unwrap()`, so a file system that reported more than
`u32::MAX` bytes panicked, which aborts the VMM, and a guest can drive a copy's count that far,
since `fuse_copy_file_range_in` carries a 64-bit length, which Linux passed on unclamped until 6.17.
It changes code that upstream main has, the two replies in `dispatch_helper` in `session.rs`, which
no earlier chain commit changes, adds `write_out` after `send_max_size`, which position 326
changes, and documents the bound on `Fuse::write` and `Fuse::copy_file_range` in `lib.rs`, which
PR13's position 047 and positions 321, 323, and 326 also change, none of the lines that they
change. Its tests, which use the session's test harness of positions 321 and 325, are in a new file
that it declares in the session's test module. It changes nothing that a mapped PR adds. NVX's
virtio-fs servers never report such a count: `VirtioFs` refuses a write of more than 1 MiB and
implements no `copy_file_range`, and `SectionFs` and the dormant microVM file system implement
neither request, so `write_out` replies to the microVM's writes with the counts that it did.
Upstream main has the defect, in the same two replies. Replaying it on upstream main conflicts
only in the module declarations of `session.rs`'s test module, which lacks the tests of PR13's
position 047 and positions 321, 323, 324, and 326, and its tests need the harness, which upstream
main lacks, so it cannot go upstream on its own as it is.

Position 328 has `Session::dispatch_init` fail `FUSE_INIT` with `EIO`, and leave the session
uninitialized with no negotiation to save, when the file system's `init` wants a flag that the guest
did not offer, or, while it keeps `FUSE_INIT_EXT`, an extended flag that the guest did not offer,
which it logs with a rate-limited error (microsoft/nvx#1630). It asserted that `init` wants only
offered flags, so a file system that wanted a flag that the guest left out panicked, which aborts
the VMM. It also keeps the version and the guest's capabilities across `init`, rather than reading
them back from the `SessionInfo` that `init` may replace: PR13's position 047 made `SessionInfo`
convertible from the public `SessionInfoState`, so an `init` that claimed every capability passed
the check, replied flags that the guest never offered, and saved a version and capabilities that the
guest never sent. It changes code that upstream main has, `dispatch_init` in `session.rs`, whose
lines that it changes no earlier chain commit changes, and documents the bounds of the negotiation
on `Fuse::init` in `lib.rs`, which PR13's position 047 and positions 321, 323, 326, and 327 also
change, none of the lines that they change. Its tests, which use the session's test harness of
positions 321 and 325 and the session state of PR13's position 047, are in a new file that it
declares in the session's test module. It changes nothing that a mapped PR adds, but it closes the
replacement that position 047 opened, so PR13 should go upstream with it. NVX's virtio-fs servers
never want an unoffered flag: `VirtioFs` wants only flags that the guest offers, and `SectionFs` and
the dormant microVM file system keep the default `init`, so the microVM negotiates as it did.
Upstream main has the abort, in the same two assertions, but not the replacement, since its
`SessionInfo` converts from no `SessionInfoState`. Replaying it on upstream main conflicts only in
the module declarations of `session.rs`'s test module, which lacks the tests of PR13's position 047
and positions 321, 323, 324, 326, and 327; without its tests, its change applies there, and `fuse`
type-checks. Its tests need the harness and PR13's session state, which upstream main lacks, so it
cannot go upstream on its own as it is.

Position 329 has `VirtioFsInode` record the device number of the stat that gives its inode number,
and `InodeMap` key the inodes of a volume with stable file IDs by volume, device, and inode number
(microsoft/nvx#1604). It keyed them by volume and inode number alone, but a share that spans
devices, through a nested mount or a btrfs subvolume, whose roots all have inode number 256, can
hold two objects with one inode number, and a lookup of the second returned the first's node and
repointed its path to the second. A share root whose nested mount's root has its inode number, as a
btrfs subvolume's root does inside another subvolume, became a directory cycle whose lookups
resolved inside the mount, requests on two such files reached whichever the guest had looked up
last, and saving a microVM failed the check of PR18's position 083 that a node's aliases identify
one object, since they identified objects on two devices. Windows reports every device number as
0, so its keys are unchanged in effect. It changes code that upstream main has: `DedupKey`,
`with_attr`, and `dedup_key` in `inode.rs`, which PR06's position 010, PR13's position 048, PR18's
positions 081 and 083, PR22's position 102, PR27's position 126, and positions 160, 165, and 320
also change, none of the code that they change, though it adds its field next to the alias field
of position 048 and the fields that position 083 makes visible to the crate; and the description
of `insert_inode` in `lib.rs`, which PR06's position 010, PR13's position 048, PR18's positions 081
and 083, PR27's position 126, and positions 158, 160, 164, and 320 also change, none of the code
that they change. It adds a test to each of those files and one to the microVM's persistence tests,
which PR18's positions 081 and 083 and PR22's position 102 also change, and the helper that its
Linux tests share to a file that no earlier chain commit changes. It changes nothing that a mapped
PR adds. The key is runtime state: position 083's saved state records each inode's device and inode
numbers, and a restore rebuilds each key from a fresh stat that must match them, so the saved state
keeps its format, and a microVM share within one device behaves as it did. Upstream main has the
defect, in the same `dedup_key`, which keyed inodes by inode number alone from its initial import
and by volume and inode number since `a5f055dd7` (microsoft/openvmm#3821). Replaying it on upstream
main conflicts in `inode.rs`, only in the fields of `VirtioFsInode` next to the new one, which
positions 048 and 083 change, and in `microvm/fs_tests.rs`, which upstream main lacks, so it cannot
go upstream on its own as it is; its other changes and tests apply there.

Position 330 has a virtio-fs volume record the host device of its root object, its directory or the
file that a microVM aggregate child exposes, and XOR a key of each other host device into the
namespaced guest inode numbers of that device's objects (microsoft/nvx#1619). The guest sees a share
under the one device of its FUSE superblock, but LOOKUP, GETATTR, STATX, and READDIR numbered each
object by its volume and host inode number alone, so distinct objects of a share that spans devices,
through a nested mount or btrfs subvolumes, which number their roots 256 and their files from 257,
could share a `(st_dev, st_ino)` in the guest: through a share of `/`, `/proc`, `/sys`, `/dev`, and
`/run` all had inode number 1, and tools that identify files by it, such as `find`, `du`, `tar`, and
`rsync -H`, reported false directory loops, skipped subtrees, or recorded unrelated files as hard
links. The key is MurmurHash3's 64-bit finalizer of the XOR of the two device numbers, which is 0
for the root's device, so a share on one device keeps its numbers, as does every share on Windows,
which reports every device number as 0. A plain READDIR numbers each entry as an object of its
directory's device, as a lookup of the entry reports it unless the entry is a mount point or a btrfs
subvolume, whose listed number names another object on the host too. The commit also rewrites lines
that PR18 adds to the microVM file system. The READDIRPLUS check that position 083 adds, that a
listed name still reaches the listed object, now covers only entries on the directory's device and
runs before the entry takes its lookup reference: it failed READDIRPLUS with EIO at almost every
mount point, whose listed number names the directory that the mount covers, and leaked the lookup
reference of an entry that failed it. Position 083's restore check of each node's saved guest inode
number, which position 164 changes, also accepts the number without the device's key that a snapshot
of an earlier pin recorded; when a node shows that numbering, the attachment keeps numbering every
object without keys, and it refuses a snapshot whose nodes number objects both ways. A snapshot that
holds no node of another device than its volume's root shows no numbering and holds no number that
keys change, so it restores with keys, and the saved state keeps its format. The constructor of
strict-path volumes that position 081 adds and position 165 changes now fails, as the volume's other
constructors do, when the volume's root object cannot be inspected. It changes `inode.rs`, which
PR06's position 010, PR13's position 048, PR18's positions 081 and 083, PR22's position 102, PR27's
position 126, and positions 160, 165, 320, and 329 also change; `lib.rs`, which PR06's position 010,
PR13's position 048, PR18's positions 081 and 083, PR27's position 126, and positions 158, 160, 164,
320, and 329 also change; `aggregate.rs`, which PR18's position 081 and positions 164 and 165 also
change, the last in the lines of a child's READDIR entry that it changes; `aggregate_tests.rs`,
which no earlier chain commit changes; the microVM's `file.rs`, `fs.rs`, `fs_tests.rs`, and
`inode.rs`, which PR18's positions 081 and 083 add and its position 085, PR22's position 102, PR27's
position 126, and positions 158, 160, 164, 165, 194, and 329 also change; and the Guide's virtio-fs
page, which PR18's position 085 adds and PR22's position 103, PR25's position 119, PR27's position
126, and positions 158-160, 164-166, 168, 175, 184, 186, and 197 also change, position 164 in the
sentence on an aggregate's inode numbers that it changes. It adds tests to `inode.rs`, `lib.rs`, and
the microVM's persistence tests, and adapts calls in existing tests, among them position 329's, to
the fallible constructor and to `map_inode`'s device argument. Its READDIRPLUS change fixes two
defects of position 083, but it tells entries apart by the device numbers that position 329 records,
which position 083 lacks, and its restore and constructor changes fix no defect of PR18 but follow
the new numbering, so it is not a fixup of PR18, and its microVM changes follow PR18 upstream.
Upstream main has the defect, in the same `map_inode`, which `a5f055dd7` (microsoft/openvmm#3821)
adds and which numbers objects by volume and inode number alone. Replaying it on upstream main
conflicts in eight of its nine files, all but `aggregate_tests.rs`: upstream main lacks the
microVM's files and the Guide's virtio-fs page, and `inode.rs`, `lib.rs`, and `aggregate.rs`
conflict next to code that PR13's position 048, PR18's positions 081 and 083, and positions 160,
165, and 329 add, so it cannot go upstream on its own as it is.

Position 331 has `VirtioFsInode::inc_lookup` saturate a node's lookup count at `u64::MAX`, as
`forget` already saturates its decrement, and `VirtioFsInode::lookup` call it (microsoft/nvx#1633).
Both added to the count with a wrapping `fetch_add`, so a reference that the guest took to a node at
the largest count, through a LOOKUP, a READDIRPLUS entry, or another reply that returns a node that
`InodeMap` already tracks, which reach `lookup`, or through a LINK, which calls `inc_lookup`,
wrapped the count to zero, and the next `FORGET` dropped the node while the guest still referenced
it: the guest's later requests for the node failed with `EINVAL`, and saving a microVM with a handle
open on it failed the check of PR18's position 083 that each handle's node is tracked. Live requests
would take some 2^64 references to reach that count, but the restore that position 083 adds accepts
any nonzero saved count, `u64::MAX` included, and still does, since a saturated node saves it, so
the saved state keeps its format. Forgets still decrement a saturated count, as the guest, which
counts its lookups in a `u64` too, never holds more references than a saturated count records. It
changes `lookup` and `inc_lookup` in `inode.rs`, upstream's code, whose increments no earlier chain
commit changes, though PR13's position 048 adds the alias that `lookup` records after its increment,
and PR06's position 010, PR13's position 048, PR18's positions 081 and 083, PR22's position 102,
PR27's position 126, and positions 160, 165, 320, 329, and 330 also change the file. It adds a test
to `inode.rs`, which calls the fallible `VirtioFsVolume::new` of position 330, and one to the
microVM's persistence tests, which PR18's positions 081 and 083, PR22's position 102, and positions
329 and 330 also change. It changes nothing that a mapped PR adds, and it fixes no defect of
position 083, whose restore only makes upstream's defect reachable, so it is not a PR18 fixup.
Upstream main has the defect, in the same two increments, which `lookup` has had since its initial
import and `inc_lookup` since `021d02d00` (microsoft/openvmm#2661), but no restore that reaches the
largest count. Replaying it on upstream main conflicts in `inode.rs`, only in `lookup`, next to the
alias of position 048, and in `microvm/fs_tests.rs`, which upstream main lacks; its change to
`inc_lookup` and its unit test apply there, but the test calls the fallible constructor of position
330, which upstream main lacks, so it cannot go upstream on its own as it is.

Position 332 has `VirtioFs::read_synthetic_root_dir_with`, which answers `FUSE_READDIR` and
`FUSE_READDIRPLUS` on an aggregate's synthetic root, fail a read that asks for more than
`MAX_GUEST_BUFFER_SIZE`, 1 MiB, with `E2BIG` before it reserves its reply buffer, and reserve the
buffer with `try_reserve_exact`, failing with `ENOMEM`, as the reads of every other directory do
(microsoft/nvx#1598). It reserved whatever size the guest asked for with an infallible
`Vec::with_capacity`, so a read of the root with a size of `u32::MAX` made the device reserve 4 GiB
for one request, which aborts the VMM where the host cannot provide it. A read of 1 MiB, the most
that the session's 256 pages per request let a Linux guest ask for, still lists the whole root. It
changes upstream's code, which no earlier chain commit changes, though PR18's position 081 and
positions 164, 165, and 330 change `aggregate.rs`, and it uses the `MAX_GUEST_BUFFER_SIZE` with
which PR06's position 010 bounds `guest_buffer` and the reads of the other directories. It adds a
test to the microVM's aggregate tests, which position 164 adds and positions 165 and 194 also
change. It changes nothing that a mapped PR adds, and it fixes no defect of position 010, whose
bound never covered the synthetic root, so it is not a PR06 fixup. NVX's Linux guest asks for at
most 1 MiB, so the microVM behaves as it did. Upstream main has the defect, in the same
`read_synthetic_root_dir_with`, which `a5f055dd7` (microsoft/openvmm#3821) adds, and in the reads
that position 010 bounds. Replaying the commit on upstream main conflicts only in the microVM's
`aggregate_fs_tests.rs`, which upstream main lacks; its change to `aggregate.rs` applies there, but
uses the `MAX_GUEST_BUFFER_SIZE` of position 010, which upstream main lacks, so it cannot go
upstream on its own as it is, and could go upstream with position 010, in microsoft/openvmm#4597.

Position 333 has `open_microvm_volume` refuse the file of an aggregate's file child whose name holds
`\` or `:`, so that attaching the aggregate fails, at a cold boot or a restore, with an error that
names the file (microsoft/nvx#1614). Position 165 opens the file's host directory as a strict volume
whose root object is the file, at its name, and PR18's position 081 has a strict volume refuse every
path with `\` or `:`, in `validate_relative_path`, which the preflight of an insert that position
081 adds, the confinement check that PR27's position 126 adds, and the save and restore of PR18's
position 083 apply to every name of a node. Nothing checked the file's name when the child attached,
since the profile's path rules cover only the policy paths that it parses, so a file child whose
name had one, common on Linux, as in timestamps or digests, and on Windows the name of an alternate
data stream of a file, attached, but every guest `LOOKUP` of it failed with `EINVAL`, and so did a
`READDIRPLUS` of the aggregate's root from the child on, while a plain `READDIR` listed it as a
regular file. The commit applies the same `validate_relative_path` to the file's name; a directory's
own name is never a path of its volume, so directory shares and children are unchanged, and the
saved state keeps its format. It changes the file branch of `open_microvm_volume` in the microVM's
`fs.rs`, which position 164 adds and position 165 extends to files, and the rustdoc of
`new_microvm_aggregate`, which position 164 adds and position 165 changes; PR18's positions 081,
083, and 085, PR22's position 102, PR27's position 126, and positions 158, 160, 194, and 330 also
change the file. It adds tests to the microVM's aggregate tests, which position 164 adds and
positions 165, 194, and 332 also change, and a bullet to the "Single files" part of the Guide's
virtio-fs page, which position 165 adds. It fixes a defect of position 165, which no PR maps, so it
is not yet assigned to a PR either and follows position 165. Upstream main lacks the defect, since
it lacks the microVM profile and its file children. Replaying the commit there conflicts in all
three files, which upstream main lacks: the microVM's `fs.rs` and `aggregate_fs_tests.rs` and the
Guide's virtio-fs page, so it cannot go upstream on its own.

Position 334 has a microVM `FUSE_CREATE` without `O_EXCL` of a name that already links to an object
fail with `EROFS` when it would write or truncate the object and the node that the guest holds for
the object is read-only, as `FUSE_OPEN` of that node fails (microsoft/nvx#1602). Position 160's
access policy lets the guest modify an object with several names only if every name that the guest
has used for it is writable: it makes `VirtioFsInode::readonly` hold for a node with a name outside
the writable paths, which upstream's `check_open_readonly` applies to an open. But it gave a create
only the checks of a namespace mutation, of its directory and its entry, while the host open of a
create without `O_EXCL` opens an existing object: a create of `out/twin`, a host hard link of a
read-only `marker`, truncated `marker` with `O_TRUNC`, and the create's insert deduplicated the file
onto the read-only node, which got a writable handle, so the restore of a snapshot taken then failed
the check of writable handles that position 160 adds to the validation of PR18's position 083. The
commit has `preflight_create_inode`, the microVM create preflight that PR18's position 081 adds,
which already resolves the object that the name links to before the host opens anything, call a new
`VirtioFs::check_create_existing`. That finds the node that the create's insert would reuse, with a
new `InodeMap::reused` on the `InodeMap::reuses` of PR18's position 081, and refuses the create when
its flags write or truncate, by a new `opens_for_writing` that `check_open_readonly` now shares, and
that node is read-only; `preflight_microvm_insert` finds the reused node with `reused` too. It
changes `create`, `check_open_readonly`, and `InodeMap` in `lib.rs`, upstream's code, next to the
checks of position 160 and the `reuses` of PR18's position 081; PR06's position 010, PR13's position
048, PR18's positions 081 and 083, PR27's position 126, and positions 158, 164, 320, 329, and 330
also change the file. It changes `preflight_create_inode` in the microVM's `fs.rs`, which position
081 adds; PR18's positions 083 and 085, PR22's position 102, PR27's position 126, and positions 158,
160, 164, 165, 194, 330, and 333 also change the file. It changes `preflight_microvm_insert` in the
microVM's `limits.rs`, which position 081 adds; PR27's position 126 and position 160 also change the
file. It adds a test to the microVM's policy tests, which position 160 adds and positions 164 and
194 also change. It fixes a defect of position 160, which no PR maps, so it is not yet assigned to a
PR either and follows position 160. Upstream main lacks the defect, since it lacks the microVM
access policy. Replaying the commit there conflicts in `lib.rs`, next to the code of PR06's position
010, PR18's position 081, and position 160, and in the microVM's `fs.rs`, `limits.rs`, and
`policy_tests.rs`, which upstream main lacks, so it cannot go upstream on its own.

Position 335 has `VirtioFs::get_file` serve a handle only for the node that the handle was opened
on, and only while the guest holds that node (microsoft/nvx#1599). The nine requests through a
handle pass it the node that they name: `GETATTR` and `STATX` with `FUSE_GETATTR_FH`, `SETATTR` with
`FATTR_FH`, `READ`, `WRITE`, `READDIR`, `READDIRPLUS`, `FSYNC`, and `FSYNCDIR`. A request whose node
the inode table does not hold, as once the guest has forgotten it, or as an aggregate's synthetic
root, fails with `EINVAL`, as other requests about an unknown node do, and one that names another
node that the table holds fails with `EBADF`, while `RELEASE` and `RELEASEDIR` still close the
handle of a forgotten node, since closing uses no name and refusing it would leak the handle.
`get_file` looked the handle up alone, so a guest could use a handle with any node's ID, and after
`FORGET` dropped the handle's node. The names that PR13's position 048 keeps current through renames
and removals, for the nodes that the table holds only, decide what a request through a handle may
do: whether it may write, which the access policy of position 160 checks against every name that the
guest has used for the object, and where `READDIRPLUS` looks up the entries that it lists. A
forgotten node's names go stale, so a guest that opened a file through a writable host hard link,
forgot the node, and looked the file up again at its read-only name still wrote and truncated the
read-only file through the handle. `get_file` does not walk the node's names again, as `get_inode`
does on a microVM share since PR27's position 126, so a handle still serves its file once the file
is unlinked. The saved state keeps its format. It changes upstream's `get_file` and the nine
handlers' calls of it in `lib.rs`, which no earlier chain commit changes but for the rate-limited
warning that PR06's position 010 gives `get_file` and the buffer bounds that it adds next to the
calls in `read` and `write`, though PR13's position 048, PR18's positions 081 and 083, PR27's
position 126, and positions 158, 160, 164, 320, 329, 330, and 334 also change the file. It adds two
tests to the microVM's policy tests, which positions 160, 164, 194, and 334 also change, and passes
the handle's node in the tests that read a directory through `get_file`: in the microVM's
persistence tests, which PR18's positions 081 and 083, PR22's position 102, and positions 164, 329,
330, and 331 also change, and in the microVM's symlink tests, which PR27's position 126 adds. It
changes nothing that a mapped PR adds, and it fixes no defect of position 048, which keeps the names
of the nodes that the guest holds as it should: the defect is upstream's handle lookup, which the
names of position 048 and the policy of position 160 only let reach the write policy, so it is not a
PR13 fixup. Upstream main has the defect, in the same `get_file`, which serves a handle with any
node's ID and after `FORGET`, though without the names of position 048 or a write policy that they
decide. Replaying the commit there conflicts in `lib.rs`, next to the bounds and the warning of
position 010 in `read`, `write`, and `get_file`, and in the microVM's `fs_tests.rs`,
`policy_tests.rs`, and `symlink_tests.rs`, which upstream main lacks, so it cannot go upstream on
its own.

Position 336 has `SwitchedCredentials::switch`, through which `lxutil::with_fs_identity` performs
each request of a share with `--mount-owner caller` as its guest caller, count a supplementary group
that reads as the kernel's overflow GID as another group unless the thread's user namespace maps
every group ID (microsoft/nvx#1600). In a user namespace, `getgroups` reports each group that the
namespace's `gid_map` does not map as the overflow GID, `/proc/sys/kernel/overflowgid`, 65534 by
default, while the kernel still checks access against the group itself. Position 158 adds
`with_fs_identity`, whose `switch` dropped the thread's supplementary groups only when one of them
differed from the caller's GID, so a request whose caller's GID was itself the overflow GID, as one
of NVX's default workload identity `65534:65534`, kept OpenVMM's unmapped host groups and could
read, write, or keep `S_ISGID` on host files of those groups. When `getgroups` reports at least one
group and each equals the caller's GID, `switch` now reads `/proc/self/gid_map` and, unless the map
covers every ID, which a namespace can do only when its parent does, so that the initial user
namespace keeps the exact comparison, reads the overflow GID too and, when the caller's GID is that
GID, drops the groups, or fails with `EPERM` before the request runs where it cannot, as where the
namespace denies `setgroups`; a map that cannot be read counts as partial, and an overflow GID that
cannot be read as any GID. The saved state keeps its format. It
changes `switch` and the rustdoc of `with_fs_identity`, and adds `may_report_other_groups_as`,
`overflow_gid`, and `maps_every_id`, in `lxutil`'s `credentials.rs`, which position 158 adds and no
other chain commit changes, and adds two tests there, one of which runs itself again in a new user
namespace that maps only the overflow GID. In the Guide's virtio-fs page, which PR18's position 085
adds, it adds a sentence to the caller-mode paragraph that position 158 adds. It fixes a defect of
position 158, which no PR maps, so it is not yet assigned to a PR either and follows position 158.
Upstream main lacks the defect, since it lacks caller ownership and `credentials.rs`. Replaying the
commit there conflicts in `credentials.rs` and the Guide's virtio-fs page, which upstream main
lacks, so it cannot go upstream on its own.

Position 337 has `VirtioFs::read_synthetic_root_dir_with`, which answers `FUSE_READDIR` and
`FUSE_READDIRPLUS` on an aggregate's synthetic root, fail a read whose buffer cannot hold the next
entry with `EINVAL`, as the reads of every other directory do, rather than return an empty reply,
which the guest takes for the end of the directory (microsoft/nvx#1617). It stopped at the first
entry that did not fit and returned what it had written, even nothing, so a read from the start with
a buffer too small for `.` listed an empty root, and a listing whose buffer held the entries before
a child with a longer name ended before that child without an error. A read that fits some entries
still returns them, a read at the end of the root still returns an empty reply, even with a buffer
too small for an entry, and `READDIRPLUS` checks that an entry fits before it looks the child up, so
the failed read takes no lookup reference. It changes only the `!fit` branch of the loop that
upstream's `a5f055dd7` (microsoft/openvmm#3821) adds, which no chain commit changes, though PR18's
position 081, positions 164, 165, and 330, and position 332, which bounds the reply buffer of the
same function, also change `aggregate.rs`. It adds a test after the last of upstream's
`aggregate_tests.rs`, on an aggregate of upstream's `VirtioFs::new_aggregate` and `add_child`, with
only upstream's `read_synthetic_root_dir` and `decode_dir_entries`; position 330 also changes the
file, none of the code that the test uses. It changes nothing that a mapped PR adds. NVX's Linux
guest reads directories with buffers of at least a page, which holds any entry, so the microVM
behaves as it did. Upstream main has the defect, in the same loop. Replaying the commit there
applies without conflicts, and `virtiofs` type-checks there with its tests on Windows, where the new
test fails without the change and all of them pass with it, so it can go upstream on its own.

Position 338 has `FUSE_LINK` to a node whose names the guest has all removed fail with `ENOENT`, as
Linux's `linkat` fails for an unlinked file, rather than link whatever object has since appeared at
the node's last name (microsoft/nvx#1608). PR13's position 048 records the names through which the
guest reaches each node and drops them when the guest removes them or renames another entry over
them, and a node whose last name it drops keeps that name, which upstream's `VirtioFsInode::link`
linked: a guest that unlinked a file, or renamed another entry over it, and then put another object
at its name linked that object, recorded the new name for the stale node, and got the stale node's
ID with the other object's attributes. Since position 160's access policy, a link through a
read-only file's own node fails with `EXDEV`, so that the file gets no name in a writable path, but
through a stale node whose last name the file had taken, the guest could give it one. A new
`VirtioFsInode::object_path` returns a node's path only while the guest still knows a name for its
object, and `link` takes the target's path from it. The saved state keeps its format. It changes the
path that upstream's `VirtioFsInode::link` links in `inode.rs`, a line that no chain commit changes,
after the confinement checks that PR18's position 081 adds and PR27's position 126 changes, and adds
`object_path` after upstream's `get_path`, which PR18's position 083 makes `pub(crate)`, reading the
names that position 048 adds; PR06's position 010, PR22's position 102, and positions 160, 165, 320,
329, 330, and 331 also change the file. It adds a test to the microVM's policy tests, which position
160 adds and positions 164, 194, 334, and 335 also change. It changes nothing that a mapped PR adds,
and it fixes no defect of position 048, whose names it reads: the defect is upstream's `link`, which
links the object at the target's last name; the names of position 048 only make it record the new
name for the target too, and the policy of position 160 only makes it a way to give a read-only file
a writable name, so it is not a PR13 fixup. Upstream main has the defect, in the same `link`, since
its nodes keep the name of their last lookup through removals, though without the names of position
048, which tell that a node's names are gone, or a write policy. Replaying the commit there
conflicts in the microVM's `policy_tests.rs`, which upstream main lacks, and its `object_path` reads
the names that position 048 adds, which upstream main lacks too, so it cannot go upstream on its
own.

Position 339 has `open_microvm_volume` open the file that the host path of an aggregate's file child
resolves to, as the child's identity does, so that a bare file name or a symbolic link to the file
attaches (microsoft/nvx#1618). Position 165's `microvm_file_identity` and
`MicroVmAggregateChild::validate_root_path` canonicalize the path, from the current directory and
through symbolic links to their final target, but its `open_microvm_volume` split the path as given:
it opened the path's parent, which is empty for a bare name, so that opening the volume failed with
`ENOENT` on Linux or `EINVAL` on Windows, and exposed the path's last name, which for a link is the
link's, whose `lstat` the volume of a file child refuses with `EACCES`. The commit resolves a
symbolic link at the path's end to its final target, as the identity does, and makes any other path
absolute, which keeps the name that the path gives: on Windows, canonicalization would turn a path
to the default stream of a file, `file::$DATA`, into the file's, which position 333's check would
then accept. That check thus applies to the name that the volume uses, the target's for a link. The
command line, which passes canonical paths and refuses links, directory shares and children, and the
saved state are unchanged. It changes the file branch of `open_microvm_volume` in the microVM's
`fs.rs`, which position 164 adds, position 165 extends to files, and position 333 changes, and the
rustdoc of `new_microvm_aggregate`, which position 164 adds and positions 165 and 333 change; PR18's
positions 081, 083, and 085, PR22's position 102, PR27's position 126, and positions 158, 160, 194,
330, and 334 also change the file. It adds tests to the microVM's aggregate tests, which position
164 adds and positions 165, 194, 332, and 333 also change. It fixes a defect of position 165, which
no PR maps, so it is not yet assigned to a PR either and follows position 165, as position 333 does.
Upstream main lacks the defect, since it lacks the microVM profile and its file children. Replaying
the commit there conflicts in both files, which upstream main lacks, so it cannot go upstream on its
own.

Position 340 has `VirtioFs::synthetic_root_nlink` count only the children of an aggregate's
synthetic root that a read of the root lists as directories, so that `FUSE_GETATTR` and `FUSE_STATX`
of the root report two plus its subdirectories, as for any directory (microsoft/nvx#1632).
Upstream's aggregate support, microsoft/openvmm#3821, counted every child, when every child was a
directory. Position 165 adds file children, which `write_child_entry` lists as `DT_REG`, but left
them in the count, so an aggregate with file children reported more links than the subdirectories
that its root holds: two file children and a directory child gave 5 rather than 3. The commit counts
the children whose volume exposes no file, which is how `write_child_entry` tells a directory child
from a file child; aggregates without file children, among them every aggregate outside a microVM,
and the saved state are unchanged. It changes `synthetic_root_nlink` in `aggregate.rs`, which
microsoft/openvmm#3821 adds and no chain commit changes; PR18's position 081 and positions 164, 165,
330, 332, and 337 also change the file. It adds a test to the microVM's aggregate tests, which
position 164 adds and positions 165, 194, 332, 333, and 339 also change. It fixes a defect of
position 165, which no PR maps, so it is not yet assigned to a PR either and follows position 165,
as positions 333 and 339 do. Upstream main lacks the defect, since it lacks the microVM profile and
its file children. Its change to `aggregate.rs` applies to upstream main, but replaying the commit
there conflicts in the microVM's `aggregate_fs_tests.rs`, which upstream main lacks, so it cannot go
upstream on its own.

Position 341 has `SwitchedCredentials::switch`, through which `lxutil::with_fs_identity` performs
each request of a share with `--mount-owner caller` that may access host files as its guest caller,
fail with `EPERM` before it changes anything while a supplementary group of the thread may be one
that its user namespace does not map (microsoft/nvx#1606). In a user namespace, `getgroups` reports
each group that the namespace's `gid_map` does not map as the overflow GID, and `setgroups` refuses
a GID that the namespace does not map, so a thread that drops such a group can never regain it.
Where the namespace's `setgroups` file says `allow`, as where `newgidmap` maps a subordinate GID
range, a thread with `CAP_SETGID` there could drop one: `switch`, which position 158 adds and which
position 336 makes drop the groups also when the caller's GID is the overflow GID, saved the list
that `getgroups` reported and dropped the groups, and the restore in position 158's
`SwitchedCredentials::drop` then failed with `EINVAL`, whose panic aborted OpenVMM, where the
overflow GID was unmapped, and otherwise gave the thread the group that the overflow GID maps to
instead of its own. Such a group can be neither kept, since it may be any group, nor dropped, so
`switch` now takes the groups from a new `mapped_groups`, which returns them as `getgroups` reports
them when there are none or when `/proc/self/gid_map` maps every ID, as the initial user namespace
does, and otherwise reads them again between two reads of the overflow GID, which the administrator
may change at any time, and fails with `EPERM` unless both reads agree and no group has that GID.
That refusal covers the case for which position 336 drops the groups, so it replaces that drop and
its `may_report_other_groups_as`. The saved state keeps its format. It changes `switch` and the
rustdoc of `with_fs_identity` in `lxutil`'s `credentials.rs`, which position 158 adds and position
336 changes, replaces position 336's `may_report_other_groups_as` with `mapped_groups`, which uses
position 336's `overflow_gid` and `maps_every_id`, and adds a test there that requires `CAP_SETGID`
and runs itself again as the root of a new user namespace that permits `setgroups`. In the Guide's
virtio-fs page, which PR18's position 085 adds, it rewrites the sentence that position 336 adds to
the caller-mode paragraph that position 158 adds. It fixes defects of positions 158 and 336, which
no PR maps, so it is not yet assigned to a PR either and follows position 336. Upstream main lacks
the defect, since it lacks caller ownership and `credentials.rs`. Replaying the commit there
conflicts in `credentials.rs` and the Guide's virtio-fs page, which upstream main lacks, so it
cannot go upstream on its own.

Position 342 has `VirtioFs::fsync_dir`, which answers `FUSE_FSYNCDIR`, succeed for the handle that
`FUSE_OPENDIR` returns for an aggregate's synthetic root, with the root's node, as `release_dir`
does, since the root keeps its entries only in memory and has nothing to write back
(microsoft/nvx#1624). It looked that handle, the sentinel `SYNTHETIC_ROOT_FH`, up in the handle map,
which never holds it, so `fsync` and `fdatasync` of a descriptor of the root, as `sync DIR` calls,
failed with `EBADF`, which POSIX reserves for a descriptor that is not open, although `READDIR`,
`READDIRPLUS`, `RELEASEDIR`, and `GETATTR` and `STATX` through a handle recognize the sentinel. It
does not answer `ENOSYS`, which the Linux client takes for the whole mount, after which it would
stop sending `FSYNCDIR` for the children's directories too, and every other handle and node reaches
`get_file` as before. It changes upstream's `fsync_dir` in `lib.rs`, which no chain commit changes
but position 335, which passes it the request's node for `get_file`, and the comments on the
sentinel in `aggregate.rs` and in `open_dir`, which no chain commit changes, though PR06's position
010, PR13's position 048, PR18's positions 081 and 083, PR27's position 126, and positions 158, 160,
164, 320, 329, 330, and 334 also change `lib.rs`, and PR18's position 081 and positions 164, 165,
330, 332, 337, and 340 also change `aggregate.rs`. It adds a test and a request helper to upstream's
`aggregate_tests.rs`, on an aggregate of upstream's `VirtioFs::new_aggregate` and `add_child`, with
imports next to those that position 337 adds; position 330 also changes the file, none of the code
that the test uses. It changes nothing that a mapped PR adds, and it fixes no defect of position
335, which only passes the node, so it is not yet assigned to a PR. In NVX's guest, `sync` and `sync
-d` of an aggregate's root failed with `Bad file descriptor`. Upstream main has the defect, in the
same `fsync_dir`, which `a5f055dd7` (microsoft/openvmm#3821) adds with the sentinel. Replaying the
commit there conflicts in `lib.rs`, where position 335 passes `fsync_dir` the request's node, and in
`aggregate_tests.rs`, among the imports of position 337. The same change made on upstream's
`fsync_dir` and imports applies there without conflicts, and `virtiofs` type-checks there with its
tests on Windows, where the new test fails without the change and all of them pass with it, so it
can go upstream on its own in that form.

Position 343 has `VirtioFs::rename` fail with `EROFS`, before the host changes anything, when a
denied path lies below either of its names, as a new `SubtreePolicy::is_subtree_writable` decides
(microsoft/nvx#1601). A rename moves everything below its source to its destination, or with
`RENAME_EXCHANGE` swaps everything below both, but `rename` checked only that the guest may modify
its two names, with the `check_writable_entry` that position 160 adds, and a writable directory can
hold a denied path. So a guest that renamed such a directory, such as `logs` under `--mount-deny
logs/secret`, or exchanged it with another on a Linux host, moved the hidden entries to a name that
no denied path covers, where `READDIR` listed them and `UNLINK` removed them, and where `LOOKUP` and
`OPEN` reached an object that the host had created at the denied path after attaching the share,
which no pin covers; and a guest could rename a directory of its own onto the name of one that the
host had removed, and so put its entries at the denied path, where it cannot create them. Below a
writable path, only a denied path hides or protects anything, since an allowed path lies inside a
denied one and a writable path that contains a path contains everything below it, so the check looks
only for denied paths below each name. A rename of such a name onto itself, which PR13's position
048 lets reach the host, fails as well, as any rename does on a read-only share. It changes the
checks of `rename` in `lib.rs` that position 160 adds, before the flag check that PR18's position
081 adds, and adds `check_writable_subtree` between position 160's `check_writable_entry` and
position 334's `check_create_existing`; PR06's position 010, PR13's position 048, PR18's positions
081 and 083, PR27's position 126, and positions 158, 164, 320, 329, 330, 335, and 342 also change
`lib.rs`. It adds `is_subtree_writable` and its test to `policy.rs`, which position 160 adds and
position 164 changes, `VirtioFsVolume::subtree_writable` to the microVM `inode.rs`, which PR18's
position 081 adds and PR18's position 083, PR22's position 102, PR27's position 126, and positions
160, 165, and 330 change, and two tests to `policy_tests.rs`, which position 160 adds and positions
164, 194, 334, 335, and 338 change, after position 160's
`readdirplus_skips_entries_that_reach_hidden_objects`. In the Guide's virtio-fs page, which PR18's
position 085 adds, it adds a sentence to the access-policy paragraph that position 160 adds. The gap
dates from the denied paths of PR22's position 102, whose checks of a name never looked below it,
but the commit changes only what position 160 adds, whose policy replaced those checks and whose
`SubtreePolicy` it needs, and nothing that a mapped PR adds, so it is not yet assigned to a PR, as
position 160 is not. Upstream main lacks the defect, since it lacks the access policy. Replaying the
commit there conflicts in `lib.rs` and in the four other files that it changes, which upstream main
lacks, so it cannot go upstream on its own.

Position 344 adds `LxVolume::stored_path` on Windows, which opens the entry that a path reaches
without access, resolving the path as every other operation does, and returns the path under whose
names the file system stores the entry, from its `FileNormalizedNameInformation`, or `None` when the
path reaches no entry (microsoft/nvx#1603). Windows also reaches an entry by its name in another
case, unless the entry's directory is case sensitive, and by the 8.3 short name of a long name, so
the stored path can differ from the path. It adds the method to lxutil's `lib.rs`, after upstream's
`statx`, its Windows implementation to `windows/mod.rs`, after upstream's `lstat`, a
`query_normalized_name` to `windows/util.rs`, which no chain commit changes, and a test to `lib.rs`,
after the tests that PR27's position 125 adds; position 125 also changes `windows/mod.rs`, and
position 158 changes `lib.rs`. It changes nothing that an earlier chain commit adds and needs only
upstream's code, but only position 346 uses it, so it is not yet assigned to a PR, as position 346
is not. Replaying the commit on upstream main conflicts in the tests of `lib.rs`, after those of
position 125, so it cannot go upstream on its own as it is.

Position 345 has a microVM share's access policy also match names ignoring case on Windows, wherever
that restricts the guest more (microsoft/nvx#1603). Windows resolves a name in another case to the
entry of that name, unless its directory is case sensitive, but `SubtreePolicy` matched the guest's
names against its denied, allowed, and writable paths byte for byte, so a name that differed from a
protected path only in case passed every check and reached the protected entry: `FUSE_UNLINK` of
`MARKER` deleted a denied `marker`, `FUSE_RENAME` of `LOGS` moved a traverse-only `logs`, or one
that holds a denied path, to a visible name, and `FUSE_CREATE` of `MARKER` put a file at a denied
path that the host had removed. On Windows, `SubtreePolicy::visibility`, and with it `is_writable`,
`is_subtree_writable`, and every check of `VirtioFsVolume`, now keeps the more restrictive of the
exact and the case-insensitive match, which compares names by Unicode's full upper-case mappings.
Grants never follow another case, so such a name exposes no allowed path and makes no writable path
writable; the validation of a policy and the pinned identities still compare paths exactly, and
Linux hosts keep matching them exactly. It changes `visibility` and `is_writable` in `policy.rs`,
which position 160 adds and position 164 changes, and the `is_subtree_writable` of position 343,
with a unit test; it adds four Windows tests to `policy_tests.rs`, after those of position 343, and
a paragraph to the Guide's access-policy section, which position 160 adds to the virtio-fs page of
PR18's position 085. The gap dates from the denied paths of PR22's position 102, whose checks of a
name compared it exactly, but the commit changes only what positions 160 and 343 add, whose policy
replaced those checks, and nothing that a mapped PR adds, so it is not yet assigned to a PR, as
position 160 is not. Upstream main lacks the defect, since it lacks the access policy. Replaying the
commit there conflicts in the three files that it changes, which upstream main lacks, so it cannot
go upstream on its own.

Position 346 has the guest reach an existing entry of a strict volume on Windows only by the names
under which Windows stores it, as on Linux (microsoft/nvx#1603). Windows also resolves the 8.3 short
name of a long name, such as `CREDEN~1.JSO` for `credentials.json`, which no matching of policy
paths can recognize, and a name in another case still became a name of the object that the policy
checks writes through: `FUSE_UNLINK` and `FUSE_RENAME` of `CREDEN~1.JSO` deleted or moved a denied
`credentials.json`, `FUSE_LOOKUP` and `FUSE_CREATE` with `O_TRUNC` of that name exposed and
truncated one that the host had recreated after attaching the share, which no pin covers, a node
kept reaching whatever its name came to reach, and `FUSE_LOOKUP` of `OUT` in a share whose writable
path is `out` left `out` read-only. With position 344's `stored_path`, a lookup, an unlink or rmdir,
and the source of a rename of a name that reaches an entry by other names fail with `ENOENT`, and a
create without `O_EXCL` and the target of a rename, which would open or replace the entry, with
`EACCES`, while a rename of an entry to its own name in another case changes that name's case; every
operation on a node also checks the node's names. It adds the checks to `lookup_child`, `create`,
`unlink`, and `rename` in `inode.rs`, upstream's code, after the confinement checks that PR18's
position 081 adds there, inlining `VirtioFsInode::new` in `lookup_child` to check a name between its
`lstat` and its check of the object, and to `validate_confined_paths` in the microVM's `inode.rs`,
which position 081 adds. It has `write_microvm_entry_plus` in the microVM's `file.rs`, which PR18's
position 083 adds and position 330 changes, look up the names that the host lists, which are the
entries' own, with a new `lookup_listed_child` that skips the check. It adds two Windows tests to
`policy_tests.rs`, after those of position 345, and extends position 345's paragraph in the Guide.
Assigning it to a PR therefore revisits PR18. It needs position 344, and position 345, whose
case-insensitive matching covers a name that reaches no entry, which no stored name can check, so it
is not yet assigned to a PR either and follows position 345. Upstream main lacks the defect, since
it lacks the microVM profile. Replaying the commit there merges in `inode.rs`, whose checks then
call the microVM module that upstream main lacks, and conflicts in the four other files that it
changes, which upstream main lacks, so it cannot go upstream on its own.

Position 347 exempts from position 346's checks a volume that exposes a single file, an aggregate's
file child (microsoft/nvx#1603). Such a child reaches its file by the last name of its host path as
given, as positions 165 and 339 have it, which the child's identity accepts, since it canonicalizes
the path, and on Windows that name may be the file's name in another case or its short name;
position 346 then failed every operation on the child with `ENOENT`, although the guest names
nothing in such a volume, which reaches nothing but the file. OpenVMM's command line and NVX
canonicalize the path, so they were unaffected. It changes the `stored_path` helper that position
346 adds to the microVM's `inode.rs`, with the `exposed_file` of position 165, and adds a Windows
test to the microVM's aggregate tests, after those of position 339; positions 164, 165, 194, 332,
333, and 340 also change that file. It fixes a defect of position 346, which no PR maps, so it is
not yet assigned to a PR either and follows position 346. Replaying the commit on upstream main
conflicts in both files that it changes, which upstream main lacks, so it cannot go upstream on its
own.

Position 348 has position 346's check of a rename fail a rename whose source is missing with
`ENOENT`, as the host would, before the host renames anything (microsoft/nvx#1603). The check let
such a rename reach the host without checking its target, but the guest's requests run concurrently,
so the guest could create the source in between, and the host's rename then replaced whatever
another name of the target reached: a replacing rename of `staging` onto `CREDEN~1.JSO` leaves a
denied `credentials.json` with the content of `staging`. Whether a volume checks names at all is now
asked of a new `checks_stored_names`, rather than inferred from a missing stored path. It changes
the helpers that positions 346 and 347 add to the microVM's `inode.rs` and adds a Windows test to
`policy_tests.rs`, after those of position 346. It fixes a defect of position 346, which no PR maps,
so it is not yet assigned to a PR either and follows position 347. Replaying the commit on upstream
main conflicts in both files that it changes, which upstream main lacks, so it cannot go upstream on
its own.

Position 349 has the READDIRPLUS of the microVM's `file.rs` look its listed names up with the
checked `lookup_child` again, rather than with the `lookup_listed_child` that position 346 adds,
which skips position 346's check (microsoft/nvx#1603). READDIRPLUS reads a directory's entries from
the snapshot that its first read takes, so a later read can list a name whose case has changed
since, which Windows still resolves to the entry: after a rename of `new` to `NEW`, the read handed
the guest a node of `new`, whose every request failed with `ENOENT`, and once the host had replaced
`old` with a file named `OLD`, the read failed at `old` with `EIO`, as did every later read, which
starts there. With the check, such a name fails with `ENOENT`, which the READDIRPLUS of PR18's
position 083 skips, as it skips a name that is gone. It changes `write_microvm_entry_plus`, which
PR18's position 083 adds and position 330 changes, back to the lookup that it had before position
346, removes `lookup_listed_child` from the microVM's `inode.rs`, and adds a Windows test to
`policy_tests.rs`, after those of position 348. It fixes a defect of position 346, which no PR maps,
so it is not yet assigned to a PR either and follows position 348. Replaying the commit on upstream
main conflicts in the three files that it changes, which upstream main lacks, so it cannot go
upstream on its own.

Position 350 has `HandleMap::can_insert` refuse a next handle of `u64::MAX`, as it refuses one of 0,
so that a handle or node table never hands out `u64::MAX` and is full once its next handle reaches
it (microsoft/nvx#1627). An aggregate's synthetic root answers `OPENDIR` with `SYNTHETIC_ROOT_FH`,
`u64::MAX`, which the root's `READDIR`, `READDIRPLUS`, `FSYNCDIR`, `RELEASEDIR`, `GETATTR`, and
`STATX` recognize, on the documented assumption that no real handle takes it, but a table handed out
`u64::MAX` as its last handle and then wrapped its next handle to 0. A child's file or directory
that got it shared the root's handle, so that a `RELEASE` of the root's node and handle, which does
not check the node, closed the child's, and the next save of a microVM recorded a next ID of 0,
which the restore that PR18's position 083 adds refuses, so the VM could no longer be restored, for
node IDs as for handles. Live requests would take some 2^64 allocations to get there, but that
restore accepts any next ID above the saved ones, `u64::MAX` included, and still does, since a full
table saves it, so the saved state keeps its format; a request that needs another handle or node
fails with `ENOSPC`, as at any other exhaustion. It changes upstream's `can_insert` in `lib.rs`,
which `389e4e57a` (microsoft/openvmm#4594) adds and no chain commit changes, though PR06's position
010, PR13's position 048, PR18's positions 081 and 083, PR27's position 126, and positions 158, 160,
164, 320, 329, 330, 334, 335, 342, and 343 change the file, and it moves the tests of a table's last
handle that `389e4e57a` adds there from `u64::MAX` to `u64::MAX - 1`. It also changes the comment on
upstream's `SYNTHETIC_ROOT_FH` in `aggregate.rs`, which `a5f055dd7` (microsoft/openvmm#3821) adds
and whose first lines position 342 changes; PR18's position 081 and positions 164, 165, 330, 332,
337, and 340 also change that file. It adds a test to the microVM's aggregate tests, which positions
164, 165, 194, 332, 333, 339, 340, and 347 also change, and one to its persistence tests, which
PR18's positions 081 and 083, PR22's position 102, and positions 164, 329, 330, 331, and 335 also
change. It changes nothing that a mapped PR adds, and it fixes no defect of position 083, whose
restore only makes upstream's defect reachable, so it is not a PR18 fixup. Upstream main has the
defect, in the same `can_insert` and under the same `SYNTHETIC_ROOT_FH`, but no restore that reaches
the largest ID. Replaying the commit on upstream main conflicts in `aggregate.rs`, in the comment
whose first lines position 342 changes, and in the microVM's `aggregate_fs_tests.rs` and
`fs_tests.rs`, which upstream main lacks; its change to `lib.rs` applies there, with its unit tests,
but the commit cannot go upstream on its own as it is.

Position 351 has lxutil's Windows `LxVolume::open` keep the access of the file that it opens to its
access mode and `O_APPEND`, and truncate an existing file for `O_TRUNC` through a handle reopened
with `FILE_WRITE_DATA` for the truncation alone when that access lacks it (microsoft/nvx#1609).
Windows truncates only through a handle with write access, so upstream's
`open_flags_to_access_mask` added `FILE_WRITE_DATA` to every `O_TRUNC` open, whose file kept that
handle and recorded that access: an `O_RDONLY | O_TRUNC` or `O_NOACCESS | O_TRUNC` file could be
written with `pwrite` and truncated with `set_attr`, where a Linux descriptor fails with `EBADF`
and `EINVAL`, and `O_APPEND | O_TRUNC` got back the `FILE_WRITE_DATA` that `O_APPEND` strips, so
its writes went to their offsets rather than to the end of the file. A virtio-fs or virtio-9p guest
that opened a file `O_RDONLY | O_TRUNC` could thus keep writing it, and a microVM restore, which
reopens the handle without `O_TRUNC`, took that access away. The open still needs write access to
truncate, as on Linux, and Linux hosts are unchanged. It changes upstream's
`open_flags_to_access_mask` in `windows/util.rs` and the truncation of upstream's `LxVolume::open`
in `windows/mod.rs`, which upstream has had since `304657e9a` and `f0c2e7814`
(microsoft/openvmm#2181) and no chain commit changes, though position 344 changes both files and
PR27's position 125 changes `windows/mod.rs`, and it adds two tests to lxutil's `lib.rs`, after
upstream's `readonly_truncate`; PR27's position 125 and positions 158 and 344 also change that
file. It changes nothing that a chain commit adds, so it is not yet assigned to a PR. Upstream main
has the defect, in the same two functions. Replaying the commit there applies, and
`cargo check -p lxutil` and the tests of `lxutil` pass on the result on Windows, so it can go
upstream on its own.

Position 352 has `VirtioFs::write` fail with `EBADF`, before it checks whether the guest may write
the file, unless the access mode of the open flags that the handle records, and a microVM snapshot
saves, is `O_WRONLY` or `O_RDWR` (microsoft/nvx#1609). `write` checked the file first, which fails
with `EROFS` on a read-only share or child or outside a microVM's writable paths, and left the
handle's access mode to the host, so a `FUSE_WRITE` through an `O_RDONLY` handle of a file that the
guest may not write failed with `EROFS` on every host, where Linux's `write(2)` fails with `EBADF`
first, and whether one through such a handle of a writable file failed depended on the host's
handle, which position 351 makes read-only on Windows. A guest kernel writes only through handles
that it opened for writing, so only a guest that drives the queue itself sees the change. It changes
upstream's `write` in `lib.rs`, between the `get_file` that position 335 changes and the
`check_writable` call that `a5f055dd7` (microsoft/openvmm#3821) adds; PR06's position 010 also
changes `write`, and PR13's position 048, PR18's positions 081 and 083, PR27's position 126, and
positions 158, 160, 164, 320, 329, 330, 334, 342, 343, and 350 change the file. It adds a test to
the microVM's `policy_tests.rs`, before the first, which position 160 adds and positions 164, 194,
334, 335, 338, 343, 345, 346, 348, and 349 change. It changes nothing that a chain commit adds, and
it fixes no defect of position 160, whose policy only adds another `EROFS` that upstream's order put
first, so it is not yet assigned to a PR. Upstream main has the defect, in the same `write`.
Replaying the commit there conflicts in `lib.rs`, next to the `get_file` of position 335, and in the
microVM's `policy_tests.rs`, which upstream main lacks, so it cannot go upstream on its own as it
is.

Position 353 has every request about a node whose names the guest has all removed, other than those
through the node's open handles, fail with `ENOENT`, as one through an unlinked file's former name
does and as Linux refuses to look up or create an entry of a removed directory, rather than reach
whatever object has since appeared at the node's last name (microsoft/nvx#1623). Position 338 made
`FUSE_LINK` take its target's path from `VirtioFsInode::object_path`, but every other operation of
`VirtioFsInode` that reaches its object by its path still used upstream's `get_path`, which yields
the name that position 048 keeps when it drops a node's last name: `GETATTR`, `STATX`, `SETATTR`,
`OPEN`, `READLINK`, `STATFS`, and the extended-attribute requests acted on that object, and a
removed directory's node listed, looked up, created, moved, and removed the entries of the directory
created in its place. Through a file's node whose last name a host hard link of a read-only file had
taken, writable through that name, the guest could truncate the read-only file, which a request
through the file's own node refuses since position 160's policy. Those operations, upstream's
`check_directory_handle` and `child_path`, through which the requests about a directory's entries
reach them, and position 083's `object_stat` now take the path from `object_path`, which now checks
that the path is one of the node's names rather than that the node has one. Position 048's
`add_alias` makes a link's new name the path when the path is no longer one of the names, as when an
unlink of the node's other name on another request queue drops that name first, and the microVM's
`validate_confined_paths` no longer checks the former name, which PR27's position 126 added to its
checks and no operation now uses. The requests through a handle, which position 335 serves without a
path, still reach the node's object. The saved state keeps its format. In `inode.rs`, it changes
upstream's operations, in lines that no chain commit changes, and upstream's `child_path`, which
position 048 makes `pub(crate)`, as well as position 083's `object_stat`, position 048's
`add_alias`, and position 338's `object_path`; PR06's position 010, PR18's position 081, PR22's
position 102, PR27's position 126, and positions 160, 165, 320, 329, 330, 331, and 346 also change
the file. In the microVM's `inode.rs`, it changes the names that PR27's position 126 has
`validate_confined_paths` check, which PR18's position 081 adds and PR18's position 083, PR22's
position 102, positions 160, 165, 330, 343, and 346-349 also change. It adds a test to `inode.rs`
and two to the microVM's `policy_tests.rs`, after position 338's, which position 160 adds and
positions 164, 194, 334, 335, 338, 343, 345, 346, 348, 349, and 352 also change. Assigning it to a
PR therefore revisits PR13, PR18, and PR27, but it fixes no defect of positions 048, 083, or 126:
the defect is upstream's, whose nodes keep the name of their last lookup through removals and whose
operations use it, and the names of position 048 only tell that a node's names are gone. It needs
position 338's `object_path`, so it is not yet assigned to a PR either and follows position 338.
Upstream main has the defect, in the same operations. Replaying the commit there conflicts in
`inode.rs`, in the microVM's `inode.rs`, and in its `policy_tests.rs`, which upstream main lacks,
and it reads the names that position 048 adds, which upstream main lacks too, so it cannot go
upstream on its own.

Position 354 has a microVM `FUSE_READDIRPLUS` list an entry that the host lists by another inode
number than a lookup of the entry reports on the directory's own host device, rather than fail with
`EIO` at it as at a name that a rename replaced since the listing (microsoft/nvx#1605). A btrfs
snapshot of a subvolume that holds a nested subvolume holds an empty placeholder for it, which the
snapshot lists by the nested subvolume's ID while a lookup reaches a directory numbered 2 on the
snapshot's device, and a directory lists a mount point of its own file system, such as a bind mount
of one of its directories, by the number of what the mount covers, so a guest could not list a
directory that held either with `READDIRPLUS`: the check that PR18's position 083 adds failed at
them, and position 330, which limits it to entries on the directory's device, exempts only the root
of a btrfs subvolume and a mount of another device. When an entry's lookup reports another number
than the listing, the new `check_still_listed` fails with `EIO` if the listing numbers the entry 0,
as a Windows file system that reports no file IDs numbers every entry, or if the node's path no
longer reaches the handle's directory, and otherwise lists the directory again with position 083's
`build_microvm_directory_snapshot`, at most once per read: it lists the entry if the host still
lists its name by the listed number, still fails with `EIO` if the host lists the name by another
number, as after a rename over it, and fails with `ENOENT`, which the READDIRPLUS of PR18's position
083 skips, if the host no longer lists the name. A lookup of a mount point reaches whatever is
mounted on it then, as one of another device does. The saved state keeps its format. In the
microVM's `file.rs`, it changes the body of the check as position 330 limits it, in the
`write_microvm_entry_plus` that PR18's position 083 adds and positions 330, 346, and 349 change,
passes the new listing to it from the loop of `read_dir_microvm`, which position 083 adds, and adds
a type after the directory snapshot budget that position 083 adds; position 160, whose access policy
the new listing applies, also changes the file. It adds three tests to the microVM's persistence
tests, after position 083's `readdirplus_ends_the_listing_only_after_skipping_every_entry_left`,
which PR18's positions 081 and 083, PR22's position 102, and positions 164, 329, 330, 331, 335, and
350 also change; they call position 083's `microvm_with_an_open_root` and position 335's `get_file`,
and one runs on Linux in user and mount namespaces of its own, around a bind mount. It fixes a
defect of position 083, but it changes the check as position 330 limits it, by the device numbers
that position 329 records, and a test needs position 335's `get_file`, so it is not a PR18 fixup,
and it follows position 353. Upstream main lacks the defect, since it lacks the microVM's directory
listings, and its `read_dir_with` checks no listed number. Replaying the commit there conflicts in
the microVM's `file.rs` and `fs_tests.rs`, which upstream main lacks, so it cannot go upstream on
its own.

Position 355 has a microVM `FUSE_READDIRPLUS` list a name whose lookup reaches the object of the
handle's own node without a node, as it lists `.` and `..`, rather than hand the guest that node
below itself and move the node to the name's path (microsoft/nvx#1634). A bind mount of a directory
on one of its own subdirectories reaches the directory, and position 354's check lets it through,
since the host lists the mount point by the number of the directory that it covers; the node's
directory does too once the host moves it into a new directory at the node's path, which a handle
opened on the node then lists. Upstream's `InodeMap::insert` reuses the node of an object that it
tracks, by the dedup key to which position 329 adds the device, and upstream's
`VirtioFsInode::lookup` makes the name's path the node's path, so the guest received a directory
below itself, which Linux refuses with `ELOOP`, and the rest of the read looked the listing's names
up below the node: in the moved directory, which lacks them, so that the READDIRPLUS of PR18's
position 083 skipped them, or through the bind mount, so that the node, the share's root included,
kept a path that names another directory once the host unmounts the mount. After position 354's
check, `write_microvm_entry_plus` now compares the dedup key and the file type of the looked-up
object, by the `InodeMap::reuses` of PR18's position 081, with those of the handle's node, and
writes the entry of such an object with upstream's `fuse_entry_out::new_dot`, which Linux takes for
an entry without attributes or a handle, by the number and type of the listing, as `READDIR` lists
it; the entry takes no lookup reference, and the read goes on. The saved state keeps its format. In
the microVM's `file.rs`, it changes the `write_microvm_entry_plus` that PR18's position 083 adds and
positions 330, 346, 349, and 354 change; position 160 also changes the file. It adds two tests to
the microVM's persistence tests, after those of position 354, which PR18's positions 081 and 083,
PR22's position 102, and positions 164, 329, 330, 331, 335, 350, and 354 also change; they call
position 083's `microvm_with_an_open_root`, position 335's `get_file`, and the directory snapshot
budget that position 083 adds, and one runs on Linux in user and mount namespaces of its own, around
a bind mount of the share's root. It fixes a defect of position 083's `READDIRPLUS`, but it changes
`write_microvm_entry_plus` after position 354's check, and its tests need position 335, so it is not
a PR18 fixup, and it follows position 354. Upstream main has the defect too, in `read_dir_with`, its
own `READDIRPLUS`, which the commit leaves as it is. Replaying the commit there conflicts in the
microVM's `file.rs` and `fs_tests.rs`, which upstream main lacks, so it cannot go upstream on its
own.

Position 356 has `Session::dispatch_init` adopt a `FUSE_INIT` negotiation, and mark the session
initialized, only once it has sent the negotiation's reply (microsoft/nvx#1898). It negotiated in
the session's own `SessionInfo`, under the write guard that it holds for the whole request, so a
reply that could not be sent, as when a guest's reply buffers cannot hold it, left the values of a
negotiation that the guest never received in an uninitialized session: `Session::save_state` saved
them in a state that the `SessionState::validate` of PR13's position 047 refuses, which a dormant
microVM slot refused to save, and the next `FUSE_INIT`, which sets only some fields before
`Fuse::init`, inherited the failed attempt's `max_background` and `congestion_threshold`. It now
negotiates in a `SessionInfo` of the request's own, from the defaults and the guest's offer alone,
and stores it in the session only once it has sent the negotiation's reply, so a reply that fails
leaves the session uninitialized without a negotiation. Position 328's refusal of unoffered flags,
which replies with an error instead, already left it so, but no longer resets the session's
`SessionInfo`, which it no longer touches. A `FUSE_INIT` whose negotiation's reply is sent
negotiates as before. When the reply fails, `Fuse::init` has already run, and nothing pairs it with
`Fuse::destroy`, which microsoft/nvx#1900 tracks; `VirtioFs::init` records its negotiation there, so
a microVM save after such a reply still finds that record and the session's state disagree, as
before. The saved state keeps its format. In `session.rs`, it changes `dispatch_init`, in lines that
PR13's position 047, which caps the guest's minor version, and position 328, which keeps the version
and the guest's capabilities across `init` and adds the refusal, also change, and in `lib.rs` it
documents the adoption on `Fuse::init` after position 328's paragraph; PR13's position 047 and
positions 321, 323, 326, and 327 also change both files, and position 324 changes `session.rs`. It
adds two tests to position 328's tests of `FUSE_INIT`, which use the session's test harness of
positions 321 and 325 and the session state of PR13's position 047. It is not a PR13 fixup, although
the failed attempt's values reached position 047's saved state: the negotiation in place is
upstream's, and the commit changes lines that position 328 adds, so it follows position 328.
Upstream main negotiates in the session's `SessionInfo` too, so its next `FUSE_INIT` after a reply
that fails inherits the same two fields, but it saves no session state. Replaying the commit there
conflicts in `lib.rs` and `session.rs`, whose lines of positions 047 and 328 it changes, and in
position 328's `init.rs`, which upstream main lacks, so it cannot go upstream on its own.

Position 357 has `ArcMutexChipsetServices::finalize`, which ends the add of a chipset device in
`vmotherboard`, place the device on its PCI bus, on its PCIe downstream port, or at its devfn as an
RCiEP only once its state unit has spawned, the last step of the add that can fail
(microsoft/nvx#1904). The add registered those places with the builder's bus resolver before
`finalize`, so an add that failed there, with `NameInUse` when another unit had the device's name,
left the dropped device's weak reference in them: the built chipset attached it to its bus, where
configuration accesses found no device, and a device added in its place failed the build with a
`PciConflict` or a `PcieConflict` that named the dropped device. `register_static_pci`,
`register_static_pcie`, and `register_static_pcie_rciep` now only record the places that the add
asks for, and `finalize` registers them with the resolver after the spawn, under the builder's
lock, once position 316 has made the device the chipset's EOI handler, PIC, and line target, so a
failed add leaves no place behind, and a successful one registers the same places as before. The
resolver's three registration methods move from `ChipsetBuilder`, whose methods take the builder's
lock, to `BusResolver`, which `finalize` reaches under the lock that it holds, in `builder/mod.rs`,
which PR10's position 015 also changes, none of the code that the commit changes. In `services.rs`,
it changes code that upstream main has, and the comment and the order of `finalize` that position
316 adds, and adds two tests after position 316's, in the `tests` module that position 316 adds;
PR10's position 015 and position 315 also change the file, none of the code that the commit
changes. It changes nothing that a mapped PR adds. Every caller of the add, the microVM's device
loader among them, returns its error, and the microVM has no PCI or PCIe device, so it behaves as
it did. Upstream main has the defect: its add registers the places before `finalize` the same way.
Replaying the commit there conflicts in `services.rs`, whose `finalize` and `tests` module of
position 316 it changes, so it cannot go upstream on its own, and it follows position 316.

Position 358 has two comments in the `lib.rs` of `virtiofs` say that inodes deduplicate only within
a volume (microsoft/nvx#1905). The comment of `VirtioFs::insert_inode` said that, on a file system
with stable inode numbers, an inode that already existed with the inserted inode's device and inode
number was returned instead, which reads as a key over the whole file system and leaves out the
file type that a match needs and the volumes without stable file IDs, and the summary of `InodeMap`
said that the map tracked in-use inodes by their actual inode number. The map indexes the inodes
that insertions may reuse by `DedupKey`, which holds the volume ID with the host device and inode
numbers on a volume with stable file IDs and with the path on any other, so a host object that two
children of an aggregate reach gets a node in each, and it reuses only a candidate of the inserted
inode's file type. The comment of `insert_inode` now says so, and the summary of `InodeMap` that the
map indexes by `DedupKey` the inodes that insertions may reuse, as section 3.6.4 of the host
filesystem design has said since microsoft/nvx#1875; the commit changes no code. It changes the
comment of `insert_inode`, to which position 329 adds the device, and upstream's summary of
`InodeMap`; PR06's position 010, PR13's position 048, PR18's positions 081 and 083, PR27's
position 126, and positions 158, 160, 164, 320, 330, 334, 335, 342, 343, 350, and 352 also change
`lib.rs`, none of the lines that the commit changes. It changes nothing that a mapped PR adds, so
it is no fixup, and it follows position 329. Upstream main keys a stable-ID volume's inodes by
volume ID and inode number, also reuses only a candidate of the same file type, and has both
comments, but for the device in that of `insert_inode`. Replaying the commit there conflicts in
that comment, so it cannot go upstream on its own.

Positions 359 and 360 make `offset_of`, which the two test-only chipsets implement for the regions
of their devices, find the offsets in a region whose last byte is the top of its address space, as
position 315 does for `vmotherboard` (microsoft/nvx#1902): position 359 for the MMIO regions of
`TestChipset` in `chipset_arc_mutex_device`, on which device unit tests build their devices, and
position 360 for the port I/O and MMIO regions of the fuzzing chipset of `chipset_device_fuzz`, on
which the chipset and IDE fuzzers run theirs. They compared the address with the region's exclusive
end, which does not fit the address type there, so the first lookup in such a region panicked where
overflow checks are on and otherwise found no offset. They now compare the address's offset from the
base with the region's length, in code that upstream main has, and each adds position 315's boundary
tests for its chipset, with `test_with_tracing` as a new dev-dependency of its crate. No earlier
chain commit changes the files of either crate, and the 38 that change `Cargo.lock`, mapped PRs'
among them, change neither crate's entry, to which each adds one line. They change nothing that a
mapped PR adds. No device of the microVM runs on either chipset, so the microVM behaves as it did.
Replaying them on upstream main applies without conflicts, and both packages type-check there with
their tests, which pass, so they can go upstream on their own.

Position 361 has `FUSE_LINK` take the lookup reference that its reply hands the guest when it
resolves its target node, under the inode map's lock, so that a `FORGET` cannot remove the node
before the reply returns it (microsoft/nvx#1897). Upstream's `VirtioFs::link` counted the reference
with `VirtioFsInode::inc_lookup` only after its host link, and outside the lock under which
`Fuse::forget` removes a node whose count reaches zero. The guest sends `FORGET` on the
high-priority queue, whose worker runs independently of the request queue's, so a `FORGET` of the
guest's other references to the target while the `LINK` ran removed the node: the `LINK` then
replied with an ID that every later request failed with `EINVAL`, and recorded the new name, as an
alias of PR13's position 048, on an inode that nothing tracked. A new `VirtioFs::reference_inode`
increments the node's count under the map's read lock, which keeps out the write lock that
`FORGET` takes, and checks the node's confinement as `get_inode` does since PR27's position 126.
The `LookupReference` that it returns releases the reference, through a new `InodeMap::forget` that
`forget` now uses too, unless the `LINK` keeps it for its reply, so a `LINK` that fails before or at
its host link keeps no reference, and the release removes the node if a `FORGET` meanwhile left it
none. `inc_lookup`, which position 331 saturates at `u64::MAX`, now reports whether it changed the
count, so that releasing a reference that a saturated count did not record changes nothing. The
saved state keeps its format. In `lib.rs`, it changes upstream's `link` and `forget`, and in `link`
the argument of the alias preflight that PR18's position 081 adds, and it adds `reference_inode`,
whose comment cites that of `insert_inode`, which position 358 rewrites, after upstream's
`reserve_inode`, `LookupReference` after upstream's `InodeReservation`, and `InodeMap::forget` after
position 334's `reused`; PR06's position 010, PR13's position 048, PR18's position 083, PR27's
position 126, and positions 158, 160, 164, 320, 329, 330, 335, 342, 343, 350, and 352 also change
the file, none of the lines that the commit changes. In `inode.rs`, it changes `inc_lookup`, which
upstream's microsoft/openvmm#2661 adds and position 331 changes; PR06's position 010, PR13's
position 048, PR18's positions 081 and 083, PR22's position 102, PR27's position 126, and positions
160, 165, 320, 329, 330, 338, 346, and 353 also change the file, none of those lines. It adds three
tests to the tests of `lib.rs`, which use the microVM profile of PR18's position 081 and the alias
lock of PR13's position 048. It changes nothing that a mapped PR adds but that argument, and it
fixes no defect of a mapped PR: the race is upstream's, so it is no fixup. Upstream main has the
race, in the same `link`, since microsoft/openvmm#2661. Replaying the commit there conflicts in
`inode.rs`, in `inc_lookup`, which upstream main does not saturate, and in `lib.rs`: in `link`,
whose checks of PR13's position 048, PR18's position 081, and positions 160 and 164 it keeps, and
where it adds `reference_inode` and `InodeMap::forget`, next to position 335's `get_file` and
position 334's `reused`, which upstream main lacks, so it cannot go upstream on its own.

Positions 362-364 have the microVM save, `VirtioFs::save_microvm_state`, fail rather than save a
snapshot that cannot restore while the guest holds an object that the host has replaced
(microsoft/nvx#1901). The save of PR18's position 083 took the identity that it records for a node
from the object that the node's first name reached at the save, and checked only that the node's
other names reached the same one. Once host software had renamed a new file over a file that the
guest held open, it thus recorded the held node with the new file's identity under the held file's
guest inode number, and the restore failed with "saved guest inode ID does not match the
attachment", or, once the guest had looked the name up again, which gives the new file a node of its
own, with "saved aliases are duplicated or ambiguous", since both nodes recorded the name. Position
362 has the save record each node's own identity, the device, inode number, and file type of the
object that the node got, through a new `VirtioFsInode::object_identity`, and check every name of
the node against it with position 083's `validate_identity`, as the restore does, and removes
position 083's `validate_saved_identity`, which only compared the names with each other. Position
363 also fails the save when two nodes of one volume share a name or a dedup key, as when the host
gives the inode number of an object that the guest holds to an object of another type and then to
one of the held object's type, each of which PR18's position 081 gives a node of its own, or when a
handle reaches another object than its node, as when the guest opened the node while the host had
replaced its object; the restore rejects both. Each error names the node or handle and says what the
guest must release. Position 364 corrects two comments of position 363's checks, which said that no
two nodes may share a name or an object, and that two nodes of one object arise when the host gives
a held object's inode number to another object of its type: the checks keep apart only the nodes of
one volume, as the restore does, and such a host gives the guest the held node again, since a lookup
reuses it. The saved state keeps its format, and the restore is unchanged. In the microVM's `fs.rs`,
they change the node and handle loops of `save_microvm_state`, in lines that position 083 adds and
no later position changes, and position 363 adds `nodes_of_one_object` after the
`open_microvm_volume` of position 164; PR18's positions 081 and 085, PR22's position 102, PR27's
position 126, and positions 158, 160, 164, 165, 194, 330, 333, 334, and 339 also change the file. In
the microVM's `inode.rs`, which PR18's position 081, PR22's position 102, PR27's position 126, and
positions 160, 165, 330, 343, 346-349, and 353 also change, position 362 adds `object_identity`
after position 083's `from_saved`, and in its `state.rs`, which PR18's positions 081 and 085, PR22's
position 102, PR27's position 126, and positions 158, 160, 164, 165, and 194 also change, it removes
`validate_saved_identity`; none of those positions change the lines that the commits change.
Positions 362 and 363 add three tests to the microVM's persistence tests, after position 083's
`hard_link_aliases_restore_and_reject_missing_or_replaced_names`, which PR18's position 081, PR22's
position 102, and positions 164, 329-331, 335, 350, 354, and 355 also change; the tests call the
policy tests' `lookup`, `open`, `release`, and `read_node`, which position 160 adds and position 164
exports to the other tests, and position 083's `microvm_with_a_tracked_file` and
`directory_with_the_inode_number_of`. Position 362 also adds a paragraph to the "Snapshot
attachments" section of the Guide's virtio-fs page, after the sentence about writable handles that
position 160 adds; PR18's position 085 adds the page, and PR22's position 103, PR25's position 119,
PR27's position 126, and positions 158-160, 164-166, 168, 175, 184, 186, 197, 330, 333, 336, 341,
343, 345, and 346 change it. They fix defects of position 083's save, but position 362 compares by
the device that position 329 records, through position 330's `device_nr`, and their tests need
position 164's export of the policy tests' helpers, so they are not PR18 fixups, and they follow
position 330. Upstream main lacks the defect, since it lacks the microVM's snapshots. Replaying the
commits there conflicts in the microVM's `fs.rs`, `fs_tests.rs`, `inode.rs`, and `state.rs` and in
the Guide's virtio-fs page, which upstream main lacks, so they cannot go upstream on their own.

Position 365 has the FUSE session fail `FUSE_COPY_FILE_RANGE_64` with `ENOSYS` rather than `EINVAL`
(microsoft/nvx#1899). Since 6.18, Linux sends each `copy_file_range` first as
`FUSE_COPY_FILE_RANGE_64`, opcode 53 of protocol 7.45, whatever minor version the session
negotiated, and only `ENOSYS` makes it resend the copy as `FUSE_COPY_FILE_RANGE` for the rest of the
connection and, after `ENOSYS` to that one too, copy the data itself. The session did not know
opcode 53, so the catch-all of upstream's `FuseOperation::read` failed it with `EINVAL`, and the
system call failed in NVX's 6.18.38 guests. The commit declares the opcode in `protocol.rs`, which
no earlier position changes, decodes it as `FUSE_COPY_FILE_RANGE` is decoded, in the operation table
of `request.rs`, and fails it with `ENOSYS` in `Session::dispatch_helper`, without calling the file
system, so `Fuse::copy_file_range` keeps answering `FUSE_COPY_FILE_RANGE` alone, through position
327's `Session::write_out`. In `lib.rs`, it says so in a paragraph of the documentation of
`Fuse::copy_file_range`, after the one that position 327 adds. A short request still fails with
`EINVAL`, and the session still negotiates protocol 7.39; no format or version changes. It adds two
tests, in a `copy_file_range_64.rs` that it registers among the session's test modules before
position 323's `error_reply`, which use the session's test harness of positions 321, 322, and 325
and position 327's reply to `FUSE_COPY_FILE_RANGE`. Positions 318 and 319 also change `request.rs`,
PR13's position 047 and positions 321, 323, 326-328, and 356 change `lib.rs` and `session.rs`, and
position 324 changes `session.rs`; apart from the paragraph and the test module, the commit adds its
lines between upstream's, which none of those positions change. It is not a fixup: no earlier
position handles the opcode, and the catch-all is upstream's. Upstream main lacks the opcode too and
fails it the same way, but replaying the commit there conflicts in `lib.rs`, which lacks position
327's paragraph, and in `session.rs`, which lacks the session's test modules, so it cannot go
upstream on its own.

Position 366 has `Session::destroy` lock the session's negotiation before it marks the session
uninitialized and calls `Fuse::destroy`, and keep it locked until it has reset the negotiation, as
`dispatch_init` keeps it locked while it negotiates (microsoft/nvx#1900). It locked the negotiation
only to reset it, after `Fuse::destroy`, so a `FUSE_INIT` that the session dispatched meanwhile, as
from another queue while it handled `FUSE_DESTROY`, ran `Fuse::init` while `Fuse::destroy` was still
undoing the previous negotiation, and the reset then cleared the negotiation that the `FUSE_INIT`
had just adopted, leaving an initialized session with version 0 and no flags, whose saved state the
`SessionState::validate` of PR13's position 047 refuses. A `FUSE_INIT` that the session dispatches
once `Session::destroy` has marked it uninitialized now negotiates only once the session has ended,
and the session keeps that negotiation, so it calls `Fuse::init` and `Fuse::destroy` one at a time,
which position 367's pairing needs. OpenVMM's virtio-fs device runs all its queues on the VM's
device thread and stops them before it resets, so its dispatches do not overlap, and the microVM
behaves as it did. In `session.rs`, it changes `Session::destroy`, in the line of PR13's position
047 that resets the negotiation, and documents the wait there; PR13's position 047 and positions
321, 323, 324, 326-328, 356, and 365 also change the file, none of the other lines that it changes.
Its test, which it adds to position 328's tests of `FUSE_INIT` after position 356's, uses the
session's test harness of positions 321 and 325 and the session state of PR13's position 047. It is
not a PR13 fixup: the unlocked `Fuse::destroy` is upstream's, and position 047's reset only adds the
cleared negotiation to the race. Upstream main calls `Fuse::destroy` outside the lock too, so a
racing `FUSE_INIT` can run `Fuse::init` while it runs, but it resets no negotiation. Replaying the
commit there conflicts in `session.rs`, whose line of position 047 it changes, and in position 328's
`init.rs`, which upstream main lacks, so it cannot go upstream on its own.

Position 367 has `Session::dispatch_init` call `Fuse::destroy` right after a `Fuse::init` whose
negotiation it does not adopt, while it still holds the negotiation's lock (microsoft/nvx#1900):
before it fails with `EIO` a `FUSE_INIT` whose `init` wants a flag that the guest did not offer,
which position 328 refuses, and when it cannot send the reply, after which position 356 adopts
nothing. The guest completes no such `FUSE_INIT`, and Linux sends no `FUSE_DESTROY` for a connection
that it did not initialize, so the next `FUSE_INIT` ran `Fuse::init` again over what the first one
had set up, and `Session::destroy`, which calls `Fuse::destroy` only for an initialized session,
never undid it either. `VirtioFs::init` records the negotiation that it agrees to, so after an
`INIT` reply that the device could not send, that record and the session's state disagreed, and a
microVM save failed until the guest negotiated again, which position 356 left to microsoft/nvx#1900;
`VirtioFs::destroy` resets that record, so such a save now stores no session. The session stays
uninitialized, so neither `FUSE_DESTROY` nor `Session::destroy` calls `Fuse::destroy` again, and an
adopted negotiation is still destroyed once, when the session ends. In `lib.rs`, it documents this
pairing on `Fuse::init`, after position 356's paragraph, and on `Fuse::destroy`, and in
`saved_state.rs` it says on PR13's `Session::restore_state` that a restore replaces a negotiation
without `Fuse::destroy`, so it restores only into a session without one. NVX's virtio-fs servers
never want a flag that the guest did not offer, so in the microVM only a reply that the device
cannot send reaches the new `Fuse::destroy`; the saved state keeps its format. In `session.rs`, it
changes `dispatch_init`, in lines that positions 328 and 356 add; PR13's position 047 and positions
321, 323, 324, 326, 327, and 365 also change the file, and all of them but position 324 also change
`lib.rs`, none of the lines that it changes. It adds four tests to position 328's tests of
`FUSE_INIT`, after position 366's, which use the session's test harness of positions 321 and 325 and
position 356's sender that fails each reply, and one to the microVM's file-system tests, after the
`initialized_microvm_negotiation_restores` of PR18's position 083, which uses the microVM profile of
PR18's position 081 and the state checks of position 083; PR22's position 102 and positions 164,
329-331, 335, 350, 354, 355, 362, and 363 also change that file. It is not a fixup of position 328
or 356: neither pairs `Fuse::init` with `Fuse::destroy`, which upstream's `dispatch_init` never did.
Upstream main also returns without `Fuse::destroy` when it cannot send the reply, and asserts that
`init` wants only offered flags rather than refuse them. Replaying the commit there conflicts in
`lib.rs` and `session.rs`, whose lines of positions 328 and 356 it changes, and in `saved_state.rs`,
position 328's `init.rs`, and the microVM's `fs_tests.rs`, which upstream main lacks, so it cannot
go upstream on its own.

Position 368 corrects comments of position 366: `Session::dispatch` routes a request by the
session's initialized flag, which `Session::destroy` clears only once it holds the lock, so a
`FUSE_INIT` that the session dispatches before then fails with `EIO` as a duplicate, as on any
initialized session, and only one that it dispatches once the flag is clear waits for the end of the
session, as `Session::destroy` and position 366's test now say. The pairing of position 367 holds
either way, since the duplicate calls no `Fuse::init`. It changes only comments, in lines that
position 366 adds, so, as position 366, it cannot go upstream on its own.

Position 369 has `cluster_mode`, which decodes the model of the xAPIC DFR for the local APIC
emulator of `virt_support_apic`, select the flat model for a reserved model whose bit 31 is set and
the cluster model for one whose bit 31 is clear, with a rate-limited warning, rather than reach
`unreachable!` (microsoft/nvx#1473). The DFR's bits 31:28 select the flat model with `0b1111` and
the cluster model with `0b0000`, and the SDM reserves their other 14 values, so a guest's xAPIC
write of the DFR, whole or of its high byte, or a restore of a saved state whose DFR held one
aborted the VMM of WHP's user-mode APIC and of OpenHCL's SNP and TDX backends. Hyper-V's APIC, which
WHP uses otherwise, selects the model the same way: as microsoft/nvx#1473 measured, and a guest of a
few bytes on WHP's in-hypervisor xAPIC confirmed on an Intel host, a write of `0x7fffffff` switches
the flat model to the cluster model, and one of `0xefffffff` switches it back, after which the DFR
reads back `0x0fffffff` or `0xffffffff`. The DFR reads back, and a save stores, the selected model,
so no saved state holds a reserved one, and the saved state keeps its format. NVX's microVMs use the
APICs of KVM, MSHV, and WHP itself, which never run the emulator. The commit adds a test to the
crate's tests in `lib.rs`, where PR10's position 026 makes `TIMER_FREQUENCY` public, none of the
lines that the commit changes. It is not a fixup: the `unreachable!` is upstream's, and upstream
main panics the same way. It applies to upstream main as it is, and `virt_support_apic` type-checks
and passes its tests on the merged tree, so it can go upstream on its own. microsoft/openvmm#5247,
open upstream, proposes another fix of the same panic, which ignores such a write, so that the APIC
keeps its model, and restores such a DFR as the flat model; replaying the commit on it conflicts in
`lib.rs`, so once it lands, the next rebase must keep one of the two behaviors.

Position 370 has `MissingDev::from_manifest`, which builds a missing device from a
`MissingDevManifest` or, through its resolver, from a `MissingDevHandle`, claim a port I/O or MMIO
range that spans its whole address space, or an empty one, without overflowing the length of its
region (microsoft/nvx#1409). It registered each range as one region of length `end - start + 1`,
which does not fit the address type for `0..=0xffff` or `0..=u64::MAX` and underflows for a range
whose start lies past its end, so building such a device panicked where overflow checks are on.
Otherwise it claimed nothing for a full-width range or an empty one whose start lies just past its
end, whose lengths wrapped to 0, and panicked in `vmotherboard`'s `DeviceRange::map` for any other
empty range. A new helper, `regions`, now claims an empty range with no region and a full-width
range with two halves under its name, and every other range as before, and three tests check the
regions that `from_manifest` maps, with `test_with_tracing` as a new dev-dependency of
`missing_dev`. No earlier chain commit changes `missing_dev`, and the 40 that change `Cargo.lock`,
mapped PRs' among them, change none of its entry, to which it adds one line. It changes nothing that
a mapped PR adds. The microVM attaches no missing device, and the chipset's fallback device for
unclaimed addresses claims nothing, so the microVM behaves as it did. Replaying the commit on
upstream main applies without conflicts, and `missing_dev` type-checks there with its tests, which
pass, so it can go upstream on its own.

Position 371 has a microVM directory listing read its directory to the end on a Windows host
(microsoft/nvx#1923). The `build_microvm_directory_snapshot` of PR18's position 083 listed the
directory with one `LxFile::read_dir` call, which on Linux reads the whole directory, but on Windows
returns the entries of one cached enumeration window of up to 64 entries at most, so that a
`READDIR` reply never spans two windows and the offsets that it hands out stay valid while the guest
removes the entries that it read (upstream's microsoft/openvmm#2661). The guest of a non-microVM
`READDIR` reads on from the offset of the last entry that it received, but the listing did not, so
on a Windows host every microVM listing ended after its first 64 entries besides `.` and `..`, and
a save kept the truncated listing of each open handle. The listing now reads on from the offset of
the last entry that a call returned until a call returns none, counting the entries that position
160's access policy hides, so that a window that the policy hides whole does not end it; position
083's bounds of 4,096 entries and 64 KiB of names, and the listing again that position 354's
`check_still_listed` takes, now cover the whole directory on Windows too. On Linux, the listing
makes one more call, which returns nothing. The saved state keeps its format. In the microVM's
`file.rs`, it turns the closure of position 083's call, which position 160 filters by the access
policy, into one that a loop calls; positions 330, 346, 349, 354, and 355 also change the file. In
lxutil's `lib.rs`, it adds to upstream's documentation of `LxFile::read_dir` that a call can end
before the end of the directory, as on Windows, and how a caller reads on; positions 125, 158, 344,
and 351 also change the file, but not that documentation. It adds two tests to the microVM's
persistence tests, after position 164's `restored_directory_listings_count_toward_the_saved_bound`,
which PR18's positions 081 and 083, PR22's position 102, and positions 164, 329-331, 335, 350,
354, 355, 362, 363, and 367 also change; they hide entries by PR22's position 102's denied paths,
which position 160's policy leaves out of listings, and call position 164's `open_root` and
`read_root`, position 335's `get_file`, and upstream's `read_directory`. It fixes a defect of PR18's
position 083, but its tests need positions 160, 164, and 335, so it is not a PR18 fixup, and it
follows position 370, whose commit and position 369's change none of its files. Upstream main lacks
the defect, since it lacks the microVM's directory listings, and its `READDIR` leaves reading on to
the guest. Replaying the commit there conflicts in the microVM's `file.rs` and `fs_tests.rs`, which
upstream main lacks, so it cannot go upstream on its own, although its documentation of
`LxFile::read_dir` applies there.

Position 372 keeps a debugger's break into a paused VM out of the VM's saved state
(microsoft/nvx#1374). With the gdb stub attached, a `DebugRequest::Break` that reached a stopped
partition unit, as when gdb attaches to a VM that runs with `--paused`, halted VPs that were already
stopped, and the run loop recorded `HaltReason::DebugBreak` as the partition's halt reason, which
the saved halt state of position 150 carries, so a save taken after the break differed from one
taken before it although the VM never ran, and its restore started the VM halted on the break. The
gdb stub attaches with a `Resume` and a `Break` and serves the debugger only once a stop report
answers them, so a break into a stopped unit without a halt now reports the stop at once, and holds
the VPs stopped for the debugger, without a halt reason, until the debugger resumes them or
detaches; starting the unit does not start held VPs, and clearing the halt, as the REPL's
`clear-halt` and OpenHCL's `resume` do, ends the hold, as it clears a recorded debugger break. A
break into a running unit halts it and records the halt as before. The debug worker sends no
`Detach` when GDB disconnects (microsoft/nvx#2094), so a hold, as a recorded debugger break before
it, lasts until a debugger attaches and resumes the VM, or the halt is cleared. In
`partition_unit.rs`, it changes upstream's `try_start`, `clear_halt`, and inspection of the power
state, none of the lines that positions 027, 065, 091, 134, 150, and 246 add, and adds four tests
to position 150's save and restore tests, after `restore_refuses_a_halted_destination`, which use
its test runner; in `partition_unit/debug.rs`, it changes upstream's handling of `Break`, `Resume`,
and `Detach`, not the halt reasons that position 054 maps to stop reasons. It also documents the
break on `DebugRequest::Break` in `vmm_core_defs` and on the Guide's gdbstub page, which no chain
commit changes, and it follows position 371, whose commit and those of positions 369 and 370 change
none of its files. It is not a fixup: the break that halts stopped VPs is upstream's, and position
150 is not yet assigned to a PR. Upstream main saves no halt state, so its saves do not change, and
replaying the commit there conflicts in `partition_unit.rs`, whose save and restore tests of
position 150 it lacks, so it cannot go upstream on its own.

Position 373 has the KVM backend's `set_activity` clear a VP's paravirtual kick, or refuse to set
the VP halted over it (microsoft/nvx#1386). A guest kicks a VP with KVM's `KVM_HC_KICK_CPU`, which
KVM serves for a partition without a time ABI whatever the guest's CPUID says, or with an IPI in
the remote-read delivery mode, which KVM's LAPIC delivers without the VMM, and KVM holds the kick
until it wakes the VP from a halt: it reads a halted VP that holds one as runnable. `set_activity`
set the MP state with one `KVM_SET_MP_STATE`, which clears the kick only for the runnable state, so
a restore that set a VP halted, onto a VP that had run and held a kick, read back as running: a
debug build's `restore_all` panicked on its read-back check, and a release build's VP ran on at
once. It now sets a VP that it does not set runnable runnable first, which clears the kick from
Linux 6.15 on, and refuses with a new `KvmError::PvKick` a VP that it sets halted and that KVM then
still reads as runnable, as before Linux 6.15, where no ioctl clears the kick. Before Linux 6.15, a
VP set runnable, or to wait for a SIPI, as a reset sets it, keeps its kick, as it did. A microVM
restores into the VPs of a new partition, which hold no kick, so its restores are unchanged. In
`vp_state.rs`, it adds its lines between position 286's `smm::leave` and upstream's
`KVM_SET_MP_STATE`, and in `lib.rs` its variant after position 285's `SmmState`; positions 024,
142, 245, 246, 248, 260, 262, 263, 265, 267, 272, 277, 278, 285-287, 289, 291, 292, 303, and 304
also change `vp_state.rs`, and positions 024, 128, 142, 248, 259, 265, 283, 285, and 301 change
`lib.rs`, none of the lines that it changes. Its KVM-only contract test, in a new `pv_kick.rs` that
it declares among the modules of `virt_backend_tests` after the `nmi` of position 288, runs a VP on
real-mode code, as position 301's test of an SMI does, and it names the module in the sentence of
the Guide's VMM-test page that position 301 changed, and describes the test after position 301's
paragraph. It follows position 372, whose commit and those of positions 369-371 change none of its
files. It is not a fixup: the single `KVM_SET_MP_STATE` is upstream's, which position 286 only
precedes with `smm::leave`. Upstream main sets the MP state the same way, but replaying the commit
there conflicts in `vp_state.rs` and `lib.rs`, whose lines of positions 285 and 286 it follows, in
the module list of `virt_backend_tests`, which lacks the `nmi` and `registers` of positions 288 and
260, and in the Guide's VMM-test page, which lacks the sentence and the paragraph of position 301,
so it cannot go upstream on its own.

Position 374 lets a VMM test arm the post-restore input gate of PR20's position 090, which petri
launched the VM worker without, so that no VMM test could check how the worker handles the gate
(microsoft/nvx#1739). `PetriVmConfigOpenVmm::with_microvm_restore_gate` keeps the gate's timeout
with the microVM's runtime resources until the worker launches, and `Worker::launch` hands it to
the worker, which gates the first start of a cold boot as it gates a restore's, so a test arms the
gate without a snapshot to restore. The worker holds the guest's acknowledgement at a snapshot
boundary, so such a test is also the VM's snapshot controller, as position 234 makes it. The commit
adds the option and its resource after position 234's in `petri/src/vm/openvmm/microvm.rs`, which
PR15's position 060, PR16's position 062, PR17's position 071, PR20's position 089, PR24's position
114, and positions 132, 142, 155, 157, 202, 218, and 234 also change, and it passes the timeout
beside position 234's channels in `start.rs`, which PR13's position 046 and position 234 also
change, and in `worker.rs`, which PR09's position 012, PR12's positions 034 and 035, PR15's position
060, PR17's positions 067, 070, and 073, PR20's positions 090 and 092, and positions 134, 142, 234,
and 236 also change, where it replaces the empty timeout in the line of `Worker::launch` that
position 090 adds. Its VMM test follows position 312's in the microVM lifecycle tests, which PR15's
position 060, PR17's position 075, and positions 202, 234, 235, and 312 also change, and uses the
helpers of positions 060 and 234. It follows position 373, whose commit and those of positions
369-372 change none of its files. It fixes no defect of position 090, whose petri launch arms no
gate, so it is not a PR20 fixup. Replaying the commit on upstream main conflicts in all four of its
files: upstream main lacks petri's microVM module and the microVM VMM tests, which position 060
adds, the VM worker's gate timeout, which position 090 adds, and the snapshot boundary channels
that position 234 passes in `start.rs` and `worker.rs`, so it follows PR20 and position 234
upstream.

Position 375 stops the deadline of the post-restore input gate when the guest acknowledges the
restore, rather than when the snapshot controller releases the snapshot boundary that holds the
acknowledgement (microsoft/nvx#1739). PR20's position 090 arms the deadline when the VM worker
starts a gated VM and clears it only in `release_snapshot_boundary`, although its message bounds
only the acknowledgement, so when a controller released the boundary after the deadline, the
worker had already stopped the VM and torn it down, although the guest had acknowledged in time.
`SnapshotRestore::restore_acknowledged`, which position 134 adds and which
`establish_snapshot_boundary` calls for a boundary under the armed gate, now clears the deadline
and keeps host input gated, so the boundary holds the acknowledged VM until the controller releases
it or rolls it back; a boundary that fails to establish still tears the VM down, and a guest that
does not acknowledge in time is still stopped. An acknowledgement that the run loop handles only
after the deadline is too late: `restore_acknowledged` then keeps the deadline and reports it, and
`establish_snapshot_boundary` stops the VM as the deadline does, so the order in which the run loop
handles the two cannot stop an expired deadline. In `restore.rs`, which PR12's positions 034 and
035, PR17's position 073, PR20's positions 090 and 092, and positions 134, 139, 142, 155, 236, and
246 also change, it changes the documentation of the `gate_deadline` that position 090 adds and
`restore_acknowledged`, which position 134 adds. In the VM worker's microVM module, which PR15's
positions 051, 057, and 058, PR16's positions 062 and 064, PR17's positions 067, 068, 070, and 072,
PR18's positions 078 and 082, PR19's positions 086 and 087, PR20's positions 090 and 093, PR23's
position 110, and 17 positions that no PR has yet also change, it changes position 134's call and
its comment. In the snapshot guide, which PR11's position 030, PR12's positions 033-035, PR13's
positions 041 and 046, PR17's positions 071 and 074, PR18's positions 077, 079, 084, and 085,
PR19's positions 087 and 088, PR20's positions 089, 090, 092, and 094, PR23's positions 108 and 113,
PR24's position 118, and 70 positions that no PR has yet also change, it extends the sentence of
position 090 that bounds the acknowledgement, in the line that PR23's position 108 extends. Its unit
tests follow position 134's test of the gate's expiry and call position 134's `next_event`, and its
VMM test follows position 374's and uses position 374's option and the helpers of PR15's position
060 and position 234. Positions 369-373 change none of its files. It fixes a defect of position 090
and needs nothing that a later position adds but, for its changes of the call and its tests, the
`restore_acknowledged` and `next_event` of position 134 and the option of position 374, so it is a
PR20 fixup: fold it into position 090, its changes of `restore_acknowledged` and of its call, and
its unit tests, into position 134, and its VMM test into position 374. Upstream main has no
post-restore input gate. Replaying the commit there conflicts in all four of its files: upstream
main lacks the VM worker's restore module, which PR12's position 034 adds, its microVM module, which
PR15's position 051 adds, the microVM VMM tests of position 060, and the guide's paragraph on the
gate, which position 090 adds, so it follows PR20 upstream.

Position 376 corrects the order in which the Guide and the CLI's documentation put a gated restore's
readiness event and the guest's repair (microsoft/nvx#2100). PR20's position 090 has the snapshot
guide and the CLI reference's entry of `--restore-ready-path` say that a gated microVM restore
publishes its restore readiness event after the guest acknowledges its repair and host input is
re-enabled, while the restored vCPU remains stopped, although `LoadedVm::start_state_units` gates
host input, starts the state units, publishes the event, arms the gate's deadline, and only then
releases the restored vCPUs, gated or not, as position 090's own message says, so the guest's repair
and its acknowledgement follow the event. The commit says so in those two passages and in the
documentation of `MicrovmRestore::gate_required`, which position 090 also adds. PR20's position 092
relied on the same order: its help and CLI reference entry of `--restore-processors` say that the
option brings the VP prefix online before restore readiness, which the guest's post-restore repair
does after the event, and its sentence of the snapshot guide, which position 142 rewraps, calls the
option no post-readiness hotplug API; the commit says that the guest's repair brings the prefix
online and that the option is no runtime hotplug API. The snapshot guide is also changed by PR11's
position 030, PR12's positions 033-035, PR13's positions 041 and 046, PR17's positions 071 and 074,
PR18's positions 077, 079, 084, and 085, PR19's positions 087 and 088, PR20's positions 089, 090,
092, 094, and 375, PR23's positions 108 and 113, PR24's position 118, and 70 positions that no PR
has yet, the CLI reference by PR15's position 059, PR16's position 062, PR17's positions 071 and
074, PR18's positions 076-079, 082, 084, and 085, PR19's positions 086-088, PR20's positions 089,
090, 092, and 094, PR21's position 101, PR22's position 103, PR23's positions 110, 112, and 113,
PR24's positions 114-117, PR25's position 120, the fork tail's position 124, PR27's position 126,
and 48 positions that no PR has yet, the CLI's microVM options in `cli_args/microvm.rs`, which
PR15's position 059 adds, by PR16's positions 062 and 064, PR17's positions 071 and 074, PR18's
positions 077-079, 082, 084, and 085, PR19's positions 086-088, PR20's positions 089, 090, 092, and
094, PR21's position 101, PR22's position 103, PR23's positions 110 and 112, PR24's positions
115-117, and 34 positions that no PR has yet, and the microVM restore module of `openvmm_entry`,
which PR17's position 074 adds, by PR18's positions 077, 079, 084, and 085, PR19's position 087,
PR20's positions 089, 090, 092, and 094, PR21's position 101, PR23's positions 111-113, and 26
positions that no PR has yet, but none of them changes the lines that it changes, other than
positions 090, 092, and 142. Of positions 369-375, only position 375 changes one of its files, in
another paragraph of the snapshot guide. It changes documentation and comments only, fixes text that
positions 090 and 092 add, and needs nothing that a later position adds but position 142's wrapping
of one sentence, so it is a PR20 fixup: fold it into position 090, its changes of
`--restore-processors` into position 092, and its change of the hotplug sentence into position 142.
Replaying the commit on upstream main conflicts in all four of its files: upstream main lacks the
CLI's microVM options and the microVM restore module, which positions 059 and 074 add, and the
passages of the guide and the CLI reference that it corrects, so it follows PR20 upstream.

Position 377 makes the same correction in the management RPC's reference (microsoft/nvx#2100).
PR25's position 119 has its entry of `restore_ready_path` say that `ResumeVM` publishes the
readiness event after all fatal restore startup work and, for a gated restore, after guest repair
succeeds and host input is re-enabled, while the restored vCPU remains stopped; the commit says that
`ResumeVM` publishes it once the restored VM's devices have started and before it releases a
restored vCPU, for a gated restore with host input still gated, before the guest's repair and its
acknowledgement, and it wraps the entry's overlong line, which position 119 left. Position 119 also
has `vmservice.proto` say that `restore_processor_count` brings the VP prefix online before restore
readiness, which the commit corrects as position 376 corrects the CLI's help. The management RPC's
reference is also changed by PR11's position 030, the fork tail's position 124, and 16 positions
that no PR has yet, and `vmservice.proto` by PR11's position 030, the fork tail's position 124, and
positions 156, 198, 223, and 228, but none of them changes the lines that it changes; position 198
numbers the fields beside the comment that it corrects. It follows position 376, which changes none
of its files. It changes documentation and comments only, fixes text that position 119 adds, and
needs nothing that a later position adds, so it is a PR25 fixup: fold it into position 119.
Replaying the commit on upstream main conflicts in both of its files, which lack the microVM
snapshot settings that position 119 adds and documents, so it follows PR25 upstream.

Position 378 has `TopologyBuilder::from_cpuid`, which `from_host_topology` calls to start a topology
from the host's, refuse the VPs per socket that it derives from the host's CPUID outside the range
of 1 to 32768 that its `vps_per_socket` setter clamps to (microsoft/nvx#1378). It stored them
unchecked, so a host whose leaf 0Bh core level has an x2APIC shift of 16 or more, or an AMD host
without leaf 0Bh whose leaf 80000008h counts fewer threads than its leaf 8000001Eh gives a core,
made a VM that starts from the host's topology, as OpenVMM's standard machine and OpenHCL do, panic
later: in `topology_cpuid`, which encodes 65536 or more VPs per socket into the 4-bit
`ApicIdCoreIdSize` of an AMD guest's leaf 80000008h, or in `TopologyBuilder::build`, which divides
by 0 VPs per socket. `from_cpuid` now fails with a new `HostTopologyError::UnsupportedVpsPerSocket`
instead, a private `MAX_VPS_PER_SOCKET` names the range's bound for the setter and the check, and
two tests check the refusal of 65536 and 0 and the acceptance of 32768 and 1, with
`test_with_tracing` as a new dev-dependency of `vm_topology`. No earlier chain commit changes
`vm_topology`, and the 41 that change `Cargo.lock`, mapped PRs' among them, change none of its
entry, to which it adds one line. It changes nothing that a mapped PR adds. The microVM starts from
`TopologyBuilder::new_x86()`, which reads no host topology, so the microVM behaves as it did. It
follows position 377; of positions 369-377, only position 370 changes one of its files, another
package's entry of `Cargo.lock`. It is not a fixup: the unchecked count is upstream's, and upstream
main panics the same way. Replaying the commit on upstream main applies without conflicts, and
`vm_topology` type-checks there with its tests, which pass, so it can go upstream on its own.

Position 379 has the WHP backend's `restore_activity` refuse the Hyper-V idle MP state for a
partition with the user-mode APIC, rather than abort the VMM (microsoft/nvx#1475). Only the
hypervisor's APIC suspends a VP that reads `HV_X64_MSR_GUEST_IDLE`, and the user-mode APIC's
`save_activity` saves a VP only as running, halted, or waiting for a SIPI, but a snapshot does not
record which APIC captured it, so a restore with `whp:user_mode_apic`, or with `no_enlightenments`
and `--hv`, can be given the idle state, and `restore_activity` reached upstream's
`vp::MpState::Idle => unimplemented!()`. The user-mode APIC cannot hold the state, since an
interrupt wakes an idle VP even with its interrupts disabled, so the commit refuses it before it
changes any state, with the `UnsupportedState` error that positions 288 and 293 return at the start
of the function for a pending NMI count and an interrupt shadow kind. In `apic.rs`, which positions
138, 143, 287, 288, 291, 293, 297, 299, 300, and 305 also change, it replaces only upstream's arm.
Its WHP contract test follows the cases of position 297 at the end of `suspend.rs`, which position
297 adds and positions 299 and 300 change, and it adds a sentence to the module's comment, after
the last, which position 299 rewrote. The Guide's VMM-test page names the test after position 297's
paragraph on `suspend.rs`, before position 301's on SMM; PR01's position 002, PR05's position 007,
and positions 260, 263, 273, 279, 296, 299, 301-304, and 373 also change the page, none of the lines
that it changes. It follows position 378; of positions 369-378, only position 373 changes one of its
files, other lines of the Guide's VMM-test page. It is not a fixup: the `unimplemented!()` is
upstream's, it changes no line that a mapped PR adds, and positions 297 and 299, whose test module
and comment it extends, belong to no PR yet. Upstream main has the same arm, and the commit's change
of `apic.rs` merges there, but replaying the commit conflicts in `suspend.rs`, which upstream main
lacks, and in the Guide's VMM-test page, which lacks position 297's paragraph, so it cannot go
upstream on its own.

Position 380 has the KVM backend refuse a partition with Hyper-V enlightenments of more VPs than
its GSI routing table has SynIC routes for, rather than panic (microsoft/nvx#1377). Such a partition
routes the SynIC of each VP through a GSI of its own, upstream's `VMBUS_BASE_GSI`, after the 24
lines of the I/O APIC, plus the VP's index, which upstream's `KvmProtoPartition::build` claims in
the table of `NUM_GSIS`, 2048, GSIs without checking the number of VPs, so on a host whose KVM
allows that many vCPUs, `build` created every vCPU of a partition of more than 2024 enlightened VPs
and then panicked in `GsiRouting::claim` with an index out of bounds. `new_partition` now refuses
such a configuration before it creates the VM, with a new `KvmError::TooManyVps`; a partition
without enlightenments, as the microVM's, routes nothing for its VPs and is not limited. In
`x86_64/mod.rs`, which PR10's positions 021 and 024, PR17's position 072, and 22 positions that no
PR has yet also change, it adds the check to upstream's block of `new_partition` that configures
the enlightenments, after its refusal of VTL2, a constant `MAX_SYNIC_VPS` after upstream's
`VMBUS_BASE_GSI`, and a sentence to upstream's comment on the SynIC routes in `build`; in `gsi.rs`,
which no chain commit changes, it makes upstream's `NUM_GSIS` visible to the crate; and in
`lib.rs`, which PR10's position 024 and positions 128, 142, 248, 259, 265, 283, 285, 301, and 373
also change, it adds its variant after position 128's `TimeAbiUnsupported`. Its KVM-only contract
test, in a new `vp_count.rs` that it declares among the modules of `virt_backend_tests` between
position 296's `unsupported_state` and position 283's `xsave`, creates partitions without building
them, and registers by hand with upstream's helpers, as position 301's test of an SMI does, and the
Guide's VMM-test page describes it after position 373's paragraph; PR05's position 008 and 16
positions that no PR has yet also change the module list, and PR01's position 002, PR05's position
007, and 13 positions that no PR has yet the Guide's page, none of the lines that it changes. It
follows position 379; of positions 369-379, position 373 changes other lines of three of its files,
`lib.rs`, the module list, and the Guide's VMM-test page, and position 379 another paragraph of
that page, and the others change none. It is not a fixup: the SynIC routes and their unchecked
claim are upstream's, and it changes no line that a mapped PR adds. Upstream main claims the routes
the same way, but replaying the commit there conflicts in `lib.rs`, which lacks position 128's
variant, in the module list of `virt_backend_tests`, which lacks the modules of positions 283 and
296, and in the Guide's VMM-test page, which lacks position 373's paragraph, and its test sets the
`time_abi` of `ProtoPartitionConfig`, which position 127 adds, so it cannot go upstream on its own.

Position 381 has the KVM backend's `activity` refuse an MP state that it does not describe, with a
new `KvmError::UnsupportedMpState` that names the VP and the state, rather than panic
(microsoft/nvx#1383). An AP of an SEV-ES or SEV-SNP guest waits in its AP reset hold once the guest
asks for it through the GHCB, and KVM then reports its MP state as `KVM_MP_STATE_AP_RESET_HOLD`,
which upstream's `activity` did not map: it panicked with `unrecognized mp state 9`, so a save of
such a VP, and an inspection of its state, which both read the activity, aborted the VM worker. The
mapping, now `mp_state_from_kvm`, keeps upstream's four states and refuses every other, the reset
hold and `KVM_MP_STATE_SIPI_RECEIVED`, which KVM never reports and for which upstream had a
`todo!()`, included, so a save of such a VP fails, a capture rolls back, and an inspection shows no
activity for the VP. The microVM runs without isolation, so its VPs never hold the reset hold. In
`vp_state.rs`, which PR10's position 024 and 21 positions that no PR has yet also change, it
replaces upstream's match in `activity`, adds its function after upstream's `AccessVpState` impl,
before position 289's `pending_interruption`, adds its unit test after position 292's at the end of
the test module that position 245 adds, and adds its `/dev/kvm` test, which builds a partition and
binds its second VP, as position 301's tests of an SMI build one and bind its VP, in a module of its
own after position 289's `exception_kvm_tests`; in `lib.rs`, which PR10's position 024 and
positions 128, 142, 248, 259, 265, 283, 285, 301, 373, and 380 also change, it adds its variant
between position 285's `SmmState` and position 373's `PvKick`. It follows position 380, which
changes other lines of `lib.rs`; of positions 369-379, only position 373 changes its files, other
lines of both. It is not a fixup: the panic and the `todo!()` are upstream's, and it changes no line
that a mapped PR adds. Upstream main maps the MP state the same way, and the commit's change of
`activity` merges there, but replaying the commit conflicts in `vp_state.rs`, which lacks the
function of position 289 and the test modules of positions 245 and 289 that its function and tests
follow, and in `lib.rs`, which lacks the variants of positions 285 and 373 that its variant sits
between, and its `/dev/kvm` test sets the fields of `ProtoPartitionConfig` that positions 036 and
127 add, so it cannot go upstream on its own.

Position 382 has `cache_parameters_cpuid` saturate at 4096, the field's maximum, the number of
logical processors sharing the L3 cache that it reports in an Intel guest's leaf 4, as position
145's `amd_cache_topology_cpuid` saturates it in an AMD guest's leaf 8000001Dh, rather than panic
(microsoft/nvx#1379). It encoded the socket's reserved VPs less one in that 12-bit field unchecked,
so a topology of more than 4096 VPs per socket, which `--vps-per-socket` or the host's topology can
give OpenVMM's standard machine, made `topology_cpuid` panic with `value out of bounds [0, 4095]` on
an Intel host with KVM, without a time ABI, or with WHP, whatever the VM's VP count. Counts of up to
4096 VPs per socket do not change, so neither does any CPUID that OpenVMM produced before, the time
ABI's effective CPUID included; the microVM has at most 8 VPs in its one socket. In `topology.rs`,
which PR15's position 051 and positions 139, 141, 143, and 145, which no PR has yet, also change, it
saturates upstream's assignment, which upstream main never reaches, since it reads the cache level
from the topology fields that it builds, which hold none: position 143 made the condition read the
cache's own descriptor, to share the L3 cache across the socket, which made the overflow reachable.
Its test follows position 145's test of leaf 8000001Dh in the test module that position 139 adds,
with the Intel and AMD CPUIDs of positions 139 and 145, and the module's tests now take
`test_with_tracing`'s test attribute. It follows position 381; of positions 369-381, none changes
its file. It is not a fixup: it changes no line that a mapped PR adds, position 051's BSP identity
among them, and positions 139, 143, and 145, whose test module, L3 condition, and tests it builds
on, belong to no PR yet. Replaying the commit on upstream main conflicts in its one file, which
lacks position 143's condition and the tests of positions 139 and 145, so it cannot go upstream on
its own.

Position 383 has the WHP backend's `restore_activity` write the activity of a VP whose APIC the VMM
emulates without the hypervisor's internal activity state, which WHP then denies, so that a restore
of such a VP's activity, a snapshot's included, no longer fails, and an INIT to an AP no longer
aborts the VMM (microsoft/nvx#1472). With `whp:user_mode_apic`, or with `no_enlightenments` and
`--hv`, WHP's partition has `LocalApicEmulationMode` `None`, and WHP denies a write of
`InternalActivityState` with `HV_STATUS_ACCESS_DENIED`, even of the running state that it holds, but
upstream's branch of `restore_activity` for the user-mode APIC writes all four registers of
`vp::Activity`, so `restore_all` failed at the activity for every saved state, and `x86_init`, which
makes an AP wait for a SIPI through `set_activity`, failed in `inject_init`, which unwraps the
error. The VMM keeps the halt and the wait for a SIPI of such a VP itself, so the commit writes only
the other three registers there. In `apic.rs`, which positions 138, 143, 287, 288, 291, 293, 297,
299, 300, 305, and 379 also change, it replaces upstream's `vp::Activity` of that branch, to which
positions 287 and 291 add `nmi_pending_count` and `interrupt_shadow_kind`, and its write, and adds
three imports. Its WHP contract tests, in a new `user_mode_apic.rs` that it declares among the
modules of `virt_backend_tests` between position 296's `unsupported_state` and position 380's
`vp_count`, register with a new backend, `whp_user_mode_apic`, that it adds after upstream's `whp`
in `native.rs`, which PR05's position 008 and positions 260, 273, 277, 278, 303, and 304 also
change, none of the lines that it changes. They build two partitions, or one of two VPs, with a
`build_all` and a `with_vps` that it adds to the fixture, which PR05's position 008, PR12's position
036, PR17's position 072, and positions 127, 142, 297, 298, and 301 also change: `build_all` takes
over upstream's `build` with position 008's mapping of the RAM, `build` keeps upstream's check of
one VP, position 301's `with_ram` now calls a function that also takes the number of VPs, and it
rewrites the fixture's comment, which position 301 last rewrote. The Guide's VMM-test page describes
the tests after position 380's paragraph; PR01's position 002, PR05's position 007, and positions
260, 263, 273, 279, 296, 297, 299, 301-304, 373, and 379 also change the page, none of the lines
that it changes. It follows position 382; of positions 369-382, position 373 changes other lines of
the module list and of the Guide's page, position 379 other lines of `apic.rs` and another paragraph
of the Guide's page, position 380 other lines of the module list and the paragraph that the commit's
follows, and the others none of its files. It is not a fixup: the write of the activity is
upstream's, and it changes no line that a mapped PR adds. Upstream main writes the activity the same
way, but replaying the commit there conflicts in `apic.rs`, whose `vp::Activity` lacks the fields of
positions 287 and 291, in the fixture, which lacks position 008's mapped RAM and positions 297's and
301's `memory` and `with_ram`, in the module list of `virt_backend_tests`, which lacks the modules
of positions 296 and 380, and in the Guide's VMM-test page, which lacks position 380's paragraph, so
it cannot go upstream on its own.

Position 384 has the epoll driver of `pal_async` report the error with which the set-up of its
io-uring failed, rather than drop it, and `disk_blockdevice` name it, so that a block device whose
driver cannot set up its io-uring, as a microVM sandbox block on Linux, fails with `failed to set up
io-uring` and the error of `io_uring_setup`, such as `ENOMEM`, rather than with `driver does not
support io-uring` (microsoft/nvx#1909). Upstream's epoll driver, since microsoft/openvmm#3271, sets
its io-uring up lazily, at the first probe or submission, and dropped the error of that set-up with
`.ok()`, so a set-up that failed, as one does under heavy host load, read as a driver without
io-uring, and its errno was lost; the microVM's sandbox blocks are block-device disks on Linux, so
their microVM failed to start that way. `IoUringDriver::io_uring_submitter` and
`Driver::io_uring_probe` now return the error that keeps a driver from providing an io-uring,
which `vmcore`'s task drivers forward and `disk_blockdevice`'s new `NewDeviceError::IoUringSetup`
carries; a failed set-up still leaves the epoll driver without a ring, which its next probe tries
again, and `disk_vmdk` still falls back to its portable reads when the probe fails. Two tests refuse
`io_uring_setup` with `ENOMEM` through a seccomp filter, with `seccompiler`, which `pal` already
uses, as a new Linux dev-dependency of `pal_async` and `disk_blockdevice`. No earlier chain commit
changes its other files, and the 42 that change `Cargo.lock`, mapped PRs' among them, change neither
the entry of `pal_async` nor that of `disk_blockdevice`, to each of which it adds one line. It
follows position 383; of positions 369-383, only positions 370 and 378 change one of its files,
other packages' entries of `Cargo.lock`. It is not a fixup: the dropped error is upstream's, and it
changes no line that a mapped PR adds. Replaying the commit on upstream main applies without
conflicts, and its packages, `pal_async`, `pal_uring`, `underhill_threadpool`, `vmcore`,
`disk_vmdk`, and `disk_blockdevice`, type-check there for `x86_64-unknown-linux-gnu` with their
tests, so it can go upstream on its own.

Position 385 counts only the entries that the access policy shows toward the bounds of a microVM
directory listing (microsoft/nvx#2093). The `build_microvm_directory_snapshot` of PR18's position
083 checked its bound of 4,096 entries per handle at the top of the callback that `LxFile::read_dir`
calls for each entry, before the filter that skips the entries that the policy hides: those whose
`child_path` fails with `EACCES`, as PR22's position 102 made denied paths do, and those that
position 160's `entry_listable` leaves out of a traverse-only directory. A hidden entry thus counted
toward the bound, so a directory whose shown entries, `.` and `..` included, filled it failed the
listing with `E2BIG` when the host listed a hidden entry after them, and listed them all when the
host listed it first, while position 083's bound of 64 KiB of names, which it checks after the
filter, never counted hidden names. The check of the entries now runs after the filter too, beside
that of the names, so only the entries that the guest can list count toward either bound, in the
listing that the first read of a handle takes and in the listing again that position 354's
`check_still_listed` takes. The saved state keeps its format and bounds. The builder now lists
through `build_microvm_directory_snapshot_within`, whose bound a test can lower, since a directory
of more than 4,096 entries takes seconds to create on Windows. In the microVM's `file.rs`, which
PR18's position 083 adds and positions 160, 330, 346, 349, 354, 355, and 371 also change, it changes
the closure that position 371's loop calls. It adds a test to the microVM's persistence tests, after
position 371's `microvm_listings_of_more_names_than_a_handle_holds_fail_with_e2big`, which PR18's
positions 081 and 083, PR22's position 102, and positions 164, 329-331, 335, 350, 354, 355, 362,
363, 367, and 371 also change; it hides files by PR22's position 102's denied paths, and calls
position 164's `open_root` and position 335's `get_file`. It fixes a defect of PR18's position 083,
but its code follows position 371's loop and its test needs PR22's position 102 and positions 164
and 335, so it is not a PR18 fixup, and it follows position 384; none of positions 372-384 changes
its files. Upstream main, `8d4e03132`, lacks the defect, since it lacks the microVM's directory
listings, and replaying the commit there conflicts in the microVM's `file.rs` and `fs_tests.rs`,
which upstream main lacks, so it cannot go upstream on its own.

Positions 386-388 have the gdb stub detach the debugger from the VM when its session ends
(microsoft/nvx#2094). Upstream's debug worker sent `DebugRequest::Attach` as a session started, but
its `run_state_machine` returned without a `DebugRequest::Detach` however the session ended: by
GDB's `D` or `k` packet or a target exit, by an error, such as the one with which a dropped
connection ends it, or by the worker dropping the session as it stops or restarts. The partition
unit thus went on treating a gone debugger as attached: the session's hardware breakpoints and
single-stepping stayed set on the VPs, the halts that followed, a power-off, reset, or triple fault
included, went to the debugger's stop channel instead of the client, and position 372's hold of a
paused VM lasted until a new session or a `clear-halt`. Upstream's handling of `Detach`, which
therefore never ran, had defects that sending it would expose, which positions 386 and 387 fix
first. Position 386 has a detach clear `halt_reported`, which the next session's first resume took
for the debugger continuing from a halt recorded after the detach, such as a power-off, and so ran
the VM again, and report the recorded halt to the client only when the halt went to the debugger
instead, as `record_halt` now records through `DebuggerState::route_halt`, rather than a second time
when the client heard of it before the debugger attached. Position 387 has a detach clear the VPs'
debug state only when the debugger set it, since the ARM64 backends of KVM and OpenHCL panic on any
request to set it, which aborts the VMM. Position 388 has `run_state_machine` attach through a
`Session` guard of the debug worker's VM proxy, which sends `Detach` when it is dropped, however the
session ends, and forgets the session's hardware breakpoints, which the next session's first resume
set again; it documents the detach on `DebugRequest::Detach`, after position 386's documentation,
and in a paragraph of its own on the Guide's gdbstub page, in another section than position 372's,
and adds four tests of the debug worker, which run the stub on a TCP connection. In
`partition_unit.rs`, position 386 changes the line of position 150's `record_halt` that reports a
halt to the debugger, and positions 386 and 387 add three tests to position 372's debugger tests,
after `hold_ends_when_the_debugger_detaches_or_the_halt_is_cleared`, which use position 150's test
runner, `halt`, and `test_topology`; positions 027, 065, 091, 134, and 246 change none of those
lines. In `partition_unit/debug.rs`, they change upstream's handling of `Detach` and
`SetDebugState`, beside the hold that position 372 adds, and upstream's `report_halt_to_debugger`,
not the halt reasons that position 054 maps to stop reasons. No chain commit changes the debug
worker, and none of the 43 that change `Cargo.lock` changes the entry of `debug_worker`, to which
position 388 adds `test_with_tracing`. They are not fixups: the missing detach and the defects of
its handling are upstream's, and positions 150 and 372, whose lines they change or extend, are not
assigned to PRs. They follow position 385; none of positions 373-385 changes their files, apart from
other packages' entries of `Cargo.lock`. Replaying them on upstream main, `8d4e03132`, conflicts in
`partition_unit.rs`, which lacks position 150's `record_halt` and tests and position 372's debugger
tests, in `partition_unit/debug.rs`, which lacks position 372's hold, and in `debug_rpc.rs`, which
lacks position 372's documentation of `Break`; position 388's changes of the debug worker and the
Guide apply there, but its documentation of `Detach` extends position 386's, so they cannot go
upstream on their own.

Position 389 lets the object that a microVM `FUSE_MKDIR`, `FUSE_MKNOD`, `FUSE_SYMLINK`, or
`FUSE_CREATE` makes take the last node ID that the node table hands out, `u64::MAX - 1`
(microsoft/nvx#2135). Such a request reserves its object's node ID before it makes the object, so
that a full table refuses it before the host changes, and PR18's position 081 has upstream's
`InodeReservation::insert` check the insert of the object's node against the microVM's limits with
`preflight_microvm_insert`, which, for a node of its own, also requires that the table can hand out
its next node ID, although the reservation has already taken that ID. After a reservation of the
last node ID, the insert thus failed with `ENOSPC` once the object was on the host, which left the
object there without a node, and each retry under another name left another. Position 083's
restore, which accepts any next node ID above the saved ones, reaches that state, and position 350,
whose `can_insert` never hands out `u64::MAX`, moved it from a reservation of `u64::MAX` to one of
`u64::MAX - 1`. A reserved insert now uses the new `preflight_microvm_reserved_insert`, which skips
only that check and keeps the inode and alias limits; the checks that run before the host changes,
and the inserts without a reservation, still make it. In `lib.rs`, it changes the call that
position 081 adds to `InodeReservation::insert`, which `389e4e57a` (microsoft/openvmm#4594) adds;
PR06's position 010, PR13's position 048, PR18's positions 081 and 083, PR27's position 126, and
positions 158, 160, 164, 320, 329, 330, 334, 335, 342, 343, 350, 352, 358, and 361 also change the
file. In the microVM's `limits.rs`, which PR18's position 081 adds and PR27's position 126 and
positions 160 and 334 also change, it splits position 081's `preflight_microvm_insert` and
`preflight_microvm_new_inode` around that check, keeping position 334's use of `InodeMap::reused`.
It adds a test to the microVM's persistence tests, after position 350's
`largest_restored_next_node_id_is_never_handed_out`, which PR18's positions 081 and 083, PR22's
position 102, and positions 164, 329-331, 335, 350, 354, 355, 362, 363, 367, 371, and 385 also
change; it restores a state with position 083's restore and makes its objects with the helpers that
position 164 shares from the policy tests. It follows position 388; of positions 372-388, only 385
changes one of its files, `fs_tests.rs`, where it adds another test. It fixes a defect of PR18's
position 081 and needs nothing that a later position adds but, for its test, position 350's table,
whose last node ID is `u64::MAX - 1`, and position 164's helpers, so it is a PR18 fixup: fold it
into position 081, where its changes of `lib.rs` and `limits.rs` apply as they are, since position
334 changes only a line of their context, and its test into position 350, after that position's
test of the largest next node ID, with the imports of `lookup` and `release` that position 362 then
no longer adds. At position 350 with the fix and the test so placed, the package's tests pass on
Windows, and without the fix the test fails as it does at position 388. Upstream main, `8d4e03132`,
lacks the defect, since it lacks the microVM's limits, and replaying the commit there conflicts in
`lib.rs`, whose `InodeReservation::insert` lacks position 081's check, and in the microVM's
`limits.rs` and `fs_tests.rs`, which upstream main lacks, so it follows PR18 upstream.

Position 390 has a microVM restore reopen the virtio-fs handles that the guest opened with access
mode 3, which neither reads nor writes (microsoft/nvx#2125). Upstream's `FUSE_OPEN` accepts that
access mode, and upstream's `check_open_readonly` of `c06ecdde0` (microsoft/openvmm#2827) refuses on
a read-only node only the opens for `O_WRONLY` or `O_RDWR` or with `O_TRUNC`, the rule that position
334 names `opens_for_writing`, so a guest gets such a handle on a read-only share or child and,
under position 160's access policy, outside the writable paths too. PR18's position 083 saved the
handle's open flags, but its `reopen_flags` rejected access mode 3, and its restore refused on a
read-only profile every handle whose access mode is not `O_RDONLY`, a rule that position 160's
`writable_handle` extended to the writable paths, so a snapshot of a guest that held such a handle
saved but failed to restore. `reopen_flags` now keeps every access mode, and
`validate_microvm_state` takes a handle for a writable one as position 334's `opens_for_writing`
takes the open that reopens it, which drops `O_TRUNC`, so only `O_WRONLY` and `O_RDWR` need a
writable node. The saved state keeps its format. In the microVM's `state.rs`, which PR18's position
081 adds and PR18's positions 083 and 085, PR22's position 102, PR27's position 126, and positions
158, 160, 164, 165, 194, and 362 also change, it removes position 083's check from `reopen_flags`
and changes the line of position 160's `writable_handle`. It adds a test to the microVM's
persistence tests, after position 083's `readonly_profile_rejects_a_writable_saved_handle`, which
PR18's positions 081 and 083, PR22's position 102, and positions 164, 329-331, 335, 350, 354, 355,
362, 363, 367, 371, 385, and 389 also change; it builds its writable paths with position 160's
`from_attachment_with_policy`, uses the helpers that position 164 shares from the policy tests, and
expects the `EBADF` with which position 352's `write` refuses a handle not open for writing. It
fixes a defect of PR18's position 083, but its code changes position 160's line and calls position
334's `opens_for_writing`, and its test needs positions 160, 164, and 352, so it is not a PR18
fixup, unlike position 389, which it follows; of positions 372-389, only 385 and 389 change one of
its files, `fs_tests.rs`, where they add other tests, and position 389's imports are only context of
its own, so folding position 389 leaves it as it is. Upstream main, `8d4e03132`, lacks the defect,
since it lacks the microVM's saved state, and replaying the commit there conflicts in the microVM's
`state.rs` and `fs_tests.rs`, which upstream main lacks, so it cannot go upstream on its own.

Position 391 has lxutil refuse on Windows the xattr names that an NTFS extended attribute's name
cannot hold (microsoft/nvx#2137). lxutil stores a Linux xattr as an NTFS extended attribute named
`LX.` and the xattr's name, whose length `FILE_GET_EA_INFORMATION` and `FILE_FULL_EA_INFORMATION`
hold in one byte, but upstream's `xattr.rs` of `f0c2e7814` (microsoft/openvmm#2181) limited xattr
names to `u16::MAX - 3` bytes and wrote the length with `as u8`, so a `FUSE_GETXATTR` of a 262-byte
name that starts with `user.a` read the `user.a` attribute, and names of 252 to 261 bytes failed
with `EINVAL` or `ENODATA` rather than `ERANGE`. NTFS accepts names of at most 254 bytes, so lxutil
now limits xattr names to 251 bytes, fails every longer name with `ERANGE` before it reaches NTFS,
and no longer truncates a name's length. No chain commit changes `xattr.rs`. In lxutil's `lib.rs`,
which positions 125, 158, 344, 351, and 371 also change, none of them in the xattr methods'
documentation or tests, it states the limit in that documentation and adds a test after the `xattr`
test. It fixes a defect that upstream has rather than one of a chain commit, so it is no fixup, and
none of positions 372-390 changes its files. It applies to upstream main, `8d4e03132`, as it is,
and on the merged tree `cargo check -p lxutil` and the lxutil tests pass on Windows, so it can go
upstream on its own.

Position 392 has lxutil's Windows volume fail a symbolic link to an empty target with `ENOENT`, as
Linux's `symlink(2)` does, before it creates anything (microsoft/nvx#2126). Upstream's
`symlink_helper` accepted the empty target. On a volume that does not confine paths, the target
names the link's own directory, so `symlink_helper` chose an NT directory link and called upstream's
`create_nt_link_reparse_buffer`, which sizes its buffer as the `REPARSE_DATA_BUFFER` up to the
`PathBuffer` of its symbolic link part, 20 bytes, plus the target twice, but writes the whole
24-byte struct at its start, 4 bytes past the end of the heap allocation for an empty target. Where
the process may not create NT links, the volume then fell back to a WSL-style link to the empty
target, which a volume that PR27's position 125's `confine_paths` confines makes directly. A guest
that builds its own `FUSE_SYMLINK` on a direct virtio-fs share, or its own `Tsymlink` on a virtio-9p
share, could thus overflow the host's heap and get a link to nothing, while PR27's position 126's
`validate_symlink_target` already refused an empty target on a microVM share. `symlink_helper` now
fails an empty target with `ENOENT` before it checks the link's path, and
`create_nt_link_reparse_buffer` refuses one too, so that its write stays within its buffer. In
lxutil's Windows `mod.rs`, which PR27's position 125 and positions 344 and 351 also change, it adds
the check at the top of upstream's `symlink_helper`, before the choice of link type that position
125 changes, and a test after upstream's `no_follow_symlink_stat_without_read_ea`; in `util.rs`,
which positions 344 and 351 also change, it changes upstream's `create_nt_link_reparse_buffer`,
which no chain commit changes. In lxutil's `lib.rs`, which PR27's position 125 and positions 158,
344, 351, 371, and 391 also change, it adds a test after upstream's `symlink` test, which also
builds a confined volume with position 125's `confine_paths`. In virtiofs's `lib.rs`, which PR06's
position 010, PR13's position 048, PR18's positions 081 and 083 and its fixup at position 389,
PR27's position 126, and positions 158, 160, 164, 320, 329, 330, 334, 335, 342, 343, 350, 352, 358,
and 361 also change, it adds a test of a direct share after upstream's
`direct_root_ignores_forget_across_destroy`, before position 361's `fs_with_a_looked_up_file`. It
fixes a defect that upstream has rather than one of a chain commit, since position 125 left the
empty target to the WSL-style link as upstream did, so it is no fixup, and it follows position 391;
of positions 372-391, only 389 and 391 change its files, position 389 a line of
`InodeReservation::insert` in virtiofs's `lib.rs`, far from its test, so folding position 389
leaves it as it is, and position 391 the documentation and tests of the xattr methods in lxutil's
`lib.rs`. Upstream main, `8d4e03132`, has the defect, and replaying the commit there merges its
lxutil changes but conflicts in virtiofs's `lib.rs`, around position 361's
`fs_with_a_looked_up_file`, and its lxutil test needs position 125's `confine_paths`, which upstream
main lacks, so it cannot go upstream on its own as it is.

Position 393 has a microVM restore reopen with `O_DIRECT` the virtio-fs handles that the guest
opened with it (microsoft/nvx#2128). Upstream's `FUSE_OPEN` and `FUSE_CREATE` accept `O_DIRECT` and
pass it to the host open, which Linux performs with it and Windows ignores, through the
`util::open_flags`, `util::create_flags`, and `LxVolume::open` of `7c92b147e`
(microsoft/openvmm#4783). PR18's position 083 records each handle's open flags, saves them, and has
the restore reopen the handle with them through its `reopen_flags`, which also checks them when the
saved state is validated, but which refused any flag outside its list, which lacked `O_DIRECT`, so
a snapshot of a guest that held such a handle saved but failed to restore with `saved handle has
unsupported open flags`. `reopen_flags` now keeps `O_DIRECT`, so the restore reopens the handle as
the guest opened it, with the host's `O_DIRECT` semantics. The saved state keeps its format. In the
microVM's `state.rs`, which PR18's position 081 adds and PR18's positions 083 and 085, PR22's
position 102, PR27's position 126, and positions 158, 160, 164, 165, 194, 362, and 390 also change,
it adds `O_DIRECT` to position 083's list in `reopen_flags`, whose check of the access mode, after
the list, position 390 removed. It adds a test to the microVM's persistence tests in `fs_tests.rs`,
which PR18's position 081 adds and PR18's position 083 and its fixup at position 389, PR22's
position 102, and positions 164, 329-331, 335, 350, 354, 355, 362, 363, 367, 371, 385, and 390 also
change, before position 083's `reopen_flags_preserve_safe_status_bits_and_reject_effects`, and
imports one more helper beside those that positions 389 and 390 import. The test uses the helpers
that position 164 shares from the policy tests, the `get_file` that position 335 has take a handle's
node, and position 334's `create_with_flags`, which it shares from `policy_tests.rs`, which position
160 adds and positions 164, 194, 334, 335, 338, 343, 345, 346, 348, 349, 352, and 353 also change.
It fixes a defect of PR18's position 083 and needs nothing that a later position adds but, for its
test, position 164's helpers, position 334's `create_with_flags`, and position 335's `get_file`, so
it is a PR18 fixup: fold it into position 083, where its line of `reopen_flags` applies as it is,
since position 390 removes only the check of the access mode after the list, and its test into
position 335, before position 083's test of `reopen_flags`, with `create_with_flags` shared there.
It follows position 392; of positions 372-392, only 385, 389, and 390 change its files: they add
other tests to `fs_tests.rs`, where positions 389 and 390 also import other helpers next to the one
that it imports, and position 390 also removes position 083's check of the access mode from
`reopen_flags`. Upstream main, `8d4e03132`,
lacks the defect, since it lacks the microVM's saved state, and replaying the commit there conflicts
in the microVM's `state.rs`, `fs_tests.rs`, and `policy_tests.rs`, which upstream main lacks, so it
cannot go upstream on its own.

Position 394 has lxutil's Windows volume create a hard link whose name is one UTF-16 unit long, such
as `x`, which failed with `EOVERFLOW` (microsoft/nvx#2136). Upstream's `create_link`, through which
`LxVolume::link` and `LxVolume::link_stat` link on Windows, last changed by `9ae5d6821`
(microsoft/openvmm#2269), passed `NtSetInformationFile` a `FILE_LINK_INFORMATION` built from a local
header that mirrors the structure up to `FileName`, 20 bytes with an alignment of 1, followed by the
name and nothing more. Windows refuses a buffer shorter than the structure, 24 bytes on 64-bit
Windows, with `STATUS_INFO_LENGTH_MISMATCH`, which lxutil maps to `EOVERFLOW`, so the 22-byte buffer
of a one-unit name failed, while longer names reached 24 bytes. A guest's `ln file x` on a virtio-fs
share of a Windows directory thus failed, as would a link on a virtio-9p share, which links through
the same `LxVolume::link`. `create_link` now pads the name with NULs that `FileNameLength` leaves
out until the buffer is as long as the structure, as upstream's `hard_link_relative` in `pal`
already sizes its own. In lxutil's Windows `util.rs`, which positions 344, 351, and 392 also change,
it changes upstream's `create_link`, which no chain commit changes. In lxutil's `lib.rs`, which
PR27's position 125 and positions 158, 344, 351, 371, 391, and 392 also change, it adds a test after
upstream's `link` test, which no chain commit changes either. It fixes a defect that upstream has
rather than one of a chain commit, so it is no fixup, and it follows position 393; of positions
372-393, only 391 and 392 change its files, position 391 the documentation and tests of the xattr
methods in lxutil's `lib.rs`, and position 392 a test after upstream's `symlink` test there and
upstream's `create_nt_link_reparse_buffer` in `util.rs`. Upstream main, `8d4e03132`, has the defect:
there, the commit's test fails without its fix with `EOVERFLOW` on Windows. The commit applies there
as it is, and on the merged tree `cargo check --all-targets -p lxutil` and lxutil's link tests pass
on Windows, so it can go upstream on its own.

## PR summary

Each count is of v8.0 commits, which include the fixups that the v3.0, v4.0, v5.0, v6.0, v7.0, and
v8.0 reorganizations folded into them; see [The v8.0 reorganization](#the-v80-reorganization),
[The v7.0 reorganization](#the-v70-reorganization),
[The v6.0 reorganization](#the-v60-reorganization),
[The v5.0 reorganization](#the-v50-reorganization),
[The v4.0 reorganization](#the-v40-reorganization), and
[The v3.0 reorganization](#the-v30-reorganization).

| PR | Class | Topic | Actual positions | Actual commit range(s) | Count | Recommended direct dependency |
| --- | --- | --- | ---: | --- | ---: | --- |
| PR01 | U | Build and test infrastructure | 001-003 | `3971db76b..56a94131a` | 3 | Current upstream |
| PR02 | U | Host primitives: mesh and sparse_mmap | 004-005 | `8c4beaa25..a9154aa5b` | 2 | Current upstream |
| PR03 | U | Host primitives: PAL on Unix | 006 | `b5df7b773` | 1 | Current upstream |
| PR04 | U | Host primitives: PAL and PAL async on Windows | - | all landed upstream | 0 | Current upstream |
| PR05 | U | Hypervisor backend fixes | 007-009 | `af721003c..7b19b1618` | 3 | Current upstream |
| PR06 | U | Device fixes and hardening | 010 | `8b956a687` | 1 | Current upstream |
| PR07 | U | Consomme resource bounding | - | all landed upstream | 0 | Current upstream |
| PR08 | U | Preparatory refactors | - | all landed upstream | 0 | Current upstream |
| PR09 | U | Fallible VM lifecycle | 011-014 | `8aa86bc26..4d5f410ea` | 4 | Current upstream |
| PR10 | U | Portable CPU and clock state | 015-027 | `9f254770b..fea603ce9` | 13 | PR09 |
| PR11 | U | Snapshot publication protocol | 028-031 | `99707a0bf..eee71081d` | 4 | PR10 |
| PR12 | U | Immutable private-CoW restore | 032-036 | `75f1282c7..d7e43e399` | 5 | PR10 |
| PR13 | U | Generic virtio persistence | 037-048 | `9d5bde6b6..9bb18f33f` | 12 | PR11 + PR12 |
| PR14 | U | Firmware-less direct boot, loader half | 049 | `d72e90d60` | 1 | Current upstream |
| PR15 | P | microVM machine profile and minimal chipset | 050-060 | `abeabf34d..2dd8e1e92` | 11 | Foundation fan-in |
| PR16 | P | Fixed virtio-MMIO ABI and shared interrupt status | 061-064 | `f5753d818..e8369480d` | 4 | PR15 |
| PR17 | P | microVM snapshot capture and authoritative restore | 065-075 | `548bf2ec2..464aa9d27` | 11 | PR16 |
| PR18 | P | microVM host attachments and device persistence | 076-085; 389 and 393 (fixups) | `d3d2a088f..77eebd10f`; `350018ce4`; `5af753b17` | 12 | PR17 |
| PR19 | N/P | Sandbox block roles and snapshot tiers | 086-088 | `6830760b7..ce4ead69a` | 3 | PR18 |
| PR20 | P | Restore-time processor and memory activation | 089-095; 375-376 (fixups) | `16b103720..e960c809b`; `d44a113be..c440ce3cc` | 9 | PR19 |
| PR21 | P/N | Network egress policy | 096-101 | `8db534880..de90ec83b` | 6 | PR20; core can start after PR13 |
| PR22 | P | Filesystem deny policy | 102-103 | `9f1580ceb..3811c4580` | 2 | PR21 |
| PR23 | N | Authenticated control console | 104-113 | `a28d4337a..6c4c9bcdf` | 10 | PR22; protocol/broker can start after PR13 |
| PR24 | N | Managed workload and outcome reporting | 114-118 | `030f308bc..ccc6545c1` | 5 | PR23 |
| PR25 | P | Management plane | 119-120; 377 (fixup) | `ac6f0d9ec..2c2e3c9ee`; `3f5506f9d` | 3 | PR24 |
| PR26 | U | Profiling benchmark | 121 | `e063cfba1` | 1 | PR25 in the current stack |
| PR27 | U/P | Safe symlinks for writable microVM shares | 125-126 | `067908221..c6e62f286` | 2 | PR22; position 125 can land independently |
| Fork tail | F | CI guards and NVX wire compatibility | 122-124 | `a54969a0b..689b87818` | 3 | PR26; do not submit upstream |

## Exact commit-to-PR mapping

Each entry, at the position that the commit had in v7.0, v7.1, and v7.2 too, names the v6.1
position of the commit, its v6.0 position, which it had in v5.0 and v5.1 too, the v4.0 position,
the v3.0 and v2.4 positions that v4.0 recorded, and the v2.4, v5.1, v6.1, and v7.2 fixups that it
folds; "(part)" marks a fixup that split across commits. Positions 016 and 017, which followed
the v6.0 reorganization and the v6.1 rebase, and positions 375-377, 389, and 393, which followed the
v8.0 reorganization, name their OpenVMM pull request instead of earlier positions. A v7.2 position
counts in `feature/microvm/dev` at `09524b79c`, whose first 339
commits are those of `feature/microvm/release/v7.2`. A v6.1
position counts in `feature/microvm/dev` at `fe61f9cbf`, whose first
269 commits are those of `feature/microvm/release/v6.1`, a v6.0 position in `feature/microvm/dev`
at `5ca20ee08`, whose first 220 commits are those of `feature/microvm/release/v6.0`, and a v4.0
position in `feature/microvm/dev` at `e1ca353eb`, whose first 210 commits are those of
`feature/microvm/release/v4.0`. Landed entries name the commits and
positions of the branch that the rebase started from, `microvm/v2.1`,
`feature/microvm/release/v2.2`, `feature/microvm/release/v2.3`, `feature/microvm/release/v2.4`,
`feature/microvm/release/v3.0`, `feature/microvm/dev` at `e1ca353eb`, or `feature/microvm/dev` at
`5ca20ee08`, and the counts exclude them. Partial upstream overlap is called out without striking
the remaining fork work, and **open upstream** names the upstream pull request that carries a
commit.

<details>
<summary><strong>PR01 - Build and test infrastructure (U)</strong> - 3 commits + 5 landed upstream</summary>

- 001 `3971db76b` - build: keep Winsock eagerly imported on Windows (v6.1 001, v6.0 001, v4.0 001, v3.0 001, v2.4 001; **not upstream**: microsoft/openvmm#4451 was closed without merging)
- 002 `c2e0222d7` - flowey: keep caller temp directories for native local VMM-test runs (v6.1 002, v6.0 003, v4.0 003, v3.0 003, v2.4 003; **open upstream**: microsoft/openvmm#4618)
- 003 `56a94131a` - virt_whp, petri: enable native PCIe NVMe boot on WHP (v6.1 003, v6.0 004, v4.0 005, v3.0 005, v2.4 005; **not upstream**: microsoft/openvmm#4615 was closed, since a Windows servicing release fixes the WHP behavior that it works around)

Landed upstream and dropped from the chain:

- ~~`707747c16` - flowey: distinguish native Windows and WSL nextest paths~~
  (**landed upstream**: microsoft/openvmm#4595, merged as `5d3ab02f0`; v6.0 position 002)
- ~~`6476a457f` - consomme: make TCP window-reopen validation deterministic~~
  (**landed upstream**: microsoft/openvmm#4598, merged as `19485f88e`; v4.0 position 004)
- ~~`607e6c6a2` - nextest: scope a longer budget to the exhaustive ring-buffer loom test~~
  (**landed upstream**: microsoft/openvmm#4584, merged as `9501bb481`; v2.1 position 002)
- ~~`0213cf65d` - flowey: skip reinstalling an identical, possibly running cargo-nextest binary~~
  (**landed upstream**: microsoft/openvmm#4608, merged as `03a2fc597`; v2.3 position 003)
- ~~`a91dc86fb` - lxutil: tolerate inherited extended attributes in tests~~
  (**landed upstream**: microsoft/openvmm#4587, merged as `a1d71b899`; v2.3 position 005)

</details>

<details>
<summary><strong>PR02 - Host primitives: mesh and sparse_mmap (U)</strong> - 2 commits + 1 landed upstream</summary>

- 004 `8c4beaa25` - mesh_protobuf: clone type-erased messages and expose encoded length (v6.1 004, v6.0 005, v4.0 006, v3.0 006, v2.4 006; **partly landed upstream**: microsoft/openvmm#4556, merged as `96647c331`, derives `Clone`; this commit now adds only `ProtobufAny::value_len` and a clone check)
- 005 `a9154aa5b` - sparse_mmap: flush file-backed ranges with bounded concurrency (v6.1 005, v6.0 006, v4.0 007, v3.0 007, v2.4 007; **open upstream**: microsoft/openvmm#4558; folds v2.4 328 (part))

Landed upstream and dropped from the chain:

- ~~`15a895dd8` - sparse_mmap: add private copy-on-write file views~~
  (**landed upstream**: microsoft/openvmm#4557, merged as `d8acf2a7c`; v2.3 position 009)

</details>

<details>
<summary><strong>PR03 - Host primitives: PAL on Unix (U)</strong> - 1 commit + 2 landed upstream</summary>

- 006 `b5df7b773` - pal: bound Linux descriptor-table pre-expansion (v6.1 006, v6.0 007, v4.0 008, v3.0 008, v2.4 008; **open upstream**: microsoft/openvmm#4589)

Landed upstream and dropped from the chain:

- ~~`299a04d4f` - pal: query Unix socket peer credentials~~
  (**landed upstream**: microsoft/openvmm#4591, merged as `42d4fe619` with tests; v6.0 position 008)
- ~~`4a7ab405c` - pal: add Unix extent cloning and sparse-extent seeking~~
  (**landed upstream**: microsoft/openvmm#4590, merged as `c62489737`; v2.3 position 012)

</details>

<details>
<summary><strong>PR04 - Host primitives: PAL and PAL async on Windows (U)</strong> - 0 commits + 6 landed upstream</summary>

No commit of PR04 remains in the chain.

Landed upstream and dropped from the chain:

- ~~`a0e70d99e` - pal: resolve the process token user SID~~
  (**landed upstream**: microsoft/openvmm#4604, merged as `6937acefb` in `pal::windows::security` itself rather than in a `user_sid` module of it; v6.0 position 009)
- ~~`1a8bc4363` - pal: add handle-relative file access that does not follow reparse points~~
  (**landed upstream**: microsoft/openvmm#4593, merged as `789d51653`; v3.0 position 010)
- ~~`d25e0e13f` - pal_async: fix named-pipe reconnect and already-connected listen~~
  (**landed upstream**: microsoft/openvmm#4585, merged as `03e8ae741`; v3.0 position 012)
- ~~`074de3406` - pal: add sparse-file range queries, zeroing, and block cloning~~
  (**landed upstream**: microsoft/openvmm#4596, merged as `8f1c36ba5`; v2.3 position 015)
- ~~`c61d77644` - pal: create files and directories with explicit security descriptors~~
  (**landed upstream**: microsoft/openvmm#4599, merged as `9fbf965c5`; v2.1 position 017)
- ~~`50460c27a` - pal: query named-pipe peers and create protected pipes~~
  (**landed upstream**: microsoft/openvmm#4582, merged as `65db52693`; v2.3 position 017)

</details>

<details>
<summary><strong>PR05 - Hypervisor backend fixes (U)</strong> - 3 commits + 2 landed upstream</summary>

- 007 `af721003c` - virt_mshv: make RUN_VP cancellation race-free (v6.1 007, v6.0 010, v4.0 011, v3.0 013, v2.4 013; **open upstream**: microsoft/openvmm#4536)
- 008 `a2490e1ed` - virt: add Partition::finalize_memory; create the MSHV BSP after finalization (v6.1 008, v6.0 011, v4.0 012, v3.0 014, v2.4 014; **open upstream**: microsoft/openvmm#4607; folds v2.4 325 (part), 373 (part), and 386)
- 009 `7b19b1618` - virt_mshv: persist and redeliver pending external interrupts (v6.1 009, v6.0 012, v4.0 013, v3.0 015, v2.4 015; **open upstream**: microsoft/openvmm#4613, stacked on microsoft/openvmm#4536; folds v2.4 325 (part) and 311)

Landed upstream and dropped from the chain:

- ~~`88865bf90` - virt_mshv: describe SNP guest workarounds without ACI-specific names~~
  (**landed upstream**: microsoft/openvmm#4580, merged as `40cff1268`; v2.1 position 021)
- ~~`9b3ce6760` - kvm: preserve pending external interrupts across VP-state capture~~
  (**landed upstream**: superseded by microsoft/openvmm#4522, merged as `392890680`; v2.1 position 022)

</details>

<details>
<summary><strong>PR06 - Device fixes and hardening (U)</strong> - 1 commit + 9 landed upstream</summary>

- 010 `8b956a687` - virtiofs: bound guest payload allocations and rate-limit diagnostics (v6.1 010, v6.0 013, v4.0 014, v3.0 018, v2.4 019; **open upstream**: microsoft/openvmm#4597)

Position 039, which carried the first revision of microsoft/openvmm#4605, moved to PR13.

Landed upstream and dropped from the chain:

- ~~`5ed5f7a22` - serial_socket: keep Windows named-pipe listeners reconnectable~~
  (**landed upstream**: microsoft/openvmm#4609, merged as `f5ef11b8a` with the write handling that a `Retain` console on a named pipe needs; v6.0 position 014)
- ~~`62b427a38` - serial_socket: report read errors as EOF and drop the connection~~
  (**landed upstream**: microsoft/openvmm#4605, merged as `48e0681e9`, narrowed in review to the read path; v4.0 position 217, which cherry-picked it)
- ~~`47102941e` - virtio_blk: let stop cancellation win over a ready queue and drain accepted I/O~~
  (**superseded upstream**: microsoft/openvmm#4835, merged as `79e67ae36`, answers task-control requests promptly, which covers this commit's stop cancellation and drain; microsoft/openvmm#4588 was closed; v3.0 position 016)
- ~~`41460f3f7` - virtiofs: make handle and inode ID allocation fallible~~
  (**landed upstream**: microsoft/openvmm#4594, merged as `389e4e57a`; v3.0 position 017)
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
<summary><strong>PR07 - Consomme resource bounding (U)</strong> - 0 commits + 3 landed upstream</summary>

No commit of PR07 remains in the chain.

Landed upstream and dropped from the chain:

- ~~`ea1153ef4` - consomme: bound active TCP, UDP, and ICMP flows~~
  (**landed upstream**: microsoft/openvmm#4600, merged as `e89a7e22d`; v4.0 position 017)
- ~~`135c503ba` - consomme: cover the pending DNS request limit with a test~~
  (**landed upstream**: microsoft/openvmm#4601, merged as `cd93f761f`; v2.3 position 030)
- ~~`90734a2ff` - consomme: cover IPv4 fragment rejection with a test~~
  (**landed upstream**: microsoft/openvmm#4602, merged as `3d9cb006a`; v2.3 position 031)

</details>

<details>
<summary><strong>PR08 - Preparatory refactors (U)</strong> - 0 commits + 6 landed upstream</summary>

No commit of PR08 remains in the chain.

Landed upstream and dropped from the chain:

- ~~`38998a580` - openvmm_helpers: split the snapshot module without behavior changes~~
  (**landed upstream**: microsoft/openvmm#4616, merged as `0812555fe`; v3.0 position 022)
- ~~`313b5fe0b` - net_tap: move the transmit path into its own module~~
  (**landed upstream**: microsoft/openvmm#4617, merged as `c696625c6`; v3.0 position 023)
- ~~`238c6cc96` - net_consomme: move queue transmit processing into a module~~
  (**landed upstream**: microsoft/openvmm#4620, merged as `3bb37a191`; v3.0 position 024)
- ~~`c1bd1e0a5` - virtio_console: move the direct receive loop into its own module~~
  (**landed upstream**: microsoft/openvmm#4621, merged as `2a17dfdd5`; v3.0 position 025)
- ~~`dd349021e` - openvmm_entry: accept `delay:<ms>:<disk>` disk wrappers~~
  (**landed upstream**: microsoft/openvmm#4610, merged as `6e0e4fdab`; v2.1 position 039)
- ~~`6794f12d0` - openvmm_entry: skip raw console mode when stdin is not a terminal~~
  (**landed upstream**: microsoft/openvmm#4606, merged as `769f2f61f`; v2.3 position 036)

</details>

<details>
<summary><strong>PR09 - Fallible VM lifecycle (U)</strong> - 4 commits + 1 landed upstream + 2 retired</summary>

- 011 `8aa86bc26` - state_unit: roll back a partial start instead of panicking (v6.1 011, v6.0 015, v4.0 018, v3.0 026, v2.4 027; **open upstream**: microsoft/openvmm#4872, with review changes that the chain lacks, apart from those of the v6.1 commits that it folds; microsoft/openvmm#4619 proposed the fallible unit start that the v4.0 reorganization retired, in favor of microsoft/nvx#1232; see [The v4.0 reorganization](#the-v40-reorganization); folds v6.1 234 (part), 235, and 236 (part))
- 012 `b385ba898` - openvmm: make VM resume fallible (v6.1 012, v6.0 016, v4.0 019, v3.0 028, v2.4 029; **closed upstream**: microsoft/openvmm#4623 was closed in favor of microsoft/nvx#1232, but the resume stays fallible: the post-restore input gate, the restore readiness event, and the time ABI's TSC offset measurement fail it)
- 013 `a861462ec` - state_unit: record the exact unit inventory in saved state (v6.1 013, v6.0 017, v4.0 020, v3.0 029, v2.4 030; **open upstream**: microsoft/openvmm#4624)
- 014 `4d5f410ea` - vmcore: add device input quiesce and resume (v6.1 014, v6.0 018, v4.0 022, v3.0 031, v2.4 032; **open upstream**: microsoft/openvmm#4626, stacked on microsoft/openvmm#4872, without the input gate's deadline, which the v5.0 reorganization removed here too, by folding v4.0 position 216; folds v6.1 240 (part))

Landed upstream and dropped from the chain:

- ~~`0692a461c` - openvmm_core: stop the devices before tearing down the VM~~
  (**superseded upstream**: microsoft/openvmm#4863, merged as `0c9697277`, drains device removals and stops the state units before the VM worker tears down the chipset devices and the partition, so this commit, which had no upstream pull request, became empty; v4.0 position 023)

Retired by the v4.0 and v5.0 reorganizations:

- ~~`03291e1ea` - state_unit: add bounded quiesce transactions with rollback classification~~
  (**retired** by the v5.0 reorganization: microsoft/openvmm#4625 was closed in favor of microsoft/nvx#1260, and a snapshot quiesce stops the state units and a failed capture starts them again instead, with no deadline; v4.0 position 021)
- ~~`d94e7b9b3` - vmcore: add fallible device start through chipset device workers~~
  (**retired** by the v4.0 reorganization: microsoft/openvmm#4622 was closed in favor of microsoft/nvx#1232, and chipset device workers start their devices without an acknowledgement, as upstream's do; position 213 has them carry a separate confirmation of the start instead; v3.0 position 027)

</details>

<details>
<summary><strong>PR10 - Portable CPU and clock state (U)</strong> - 13 commits</summary>

- 015 `9f254770b` - vmcore, state_unit: advance guest time across stopped downtime (v6.1 015, v6.0 019, v4.0 024, v3.0 033, v2.4 033; folds v2.4 213 (part) and 371 (part), and v6.1 234 (part))
- 016 `4ef77bd1f` - vmcore: skip a periodic timer's missed periods at once (v6.1 261, microsoft/openvmm#5073; moved here by the v7.0 reorganization, ahead of position 018, whose RTC restarts its periodic timer at its saved deadline; can go upstream on its own)
- 017 `569f78f59` - vmcore: start a periodic timer at a given deadline (v6.1 270, microsoft/openvmm#5077; moved here by the v7.0 reorganization, ahead of position 018, whose RTC restarts its periodic timer at its saved deadline through it; can go upstream on its own after position 016)
- 018 `1c6059f0f` - chipset: persist and advance the RTC clock (v6.1 016, v6.0 020, v4.0 025, v3.0 034, v2.4 034; folds v2.4 213 (part), and v6.1 271 (part) and 273 (part))
- 019 `bedaaeb17` - chipset: reject inconsistent RTC interrupt state on restore (v6.1 017, v6.0 021, v4.0 026, v3.0 035, v2.4 035; folds v6.1 271 (part) and 273 (part))
- 020 `aec5fb643` - chipset: catch up the PIT after downtime (v6.1 018, v6.0 022, v4.0 027, v3.0 036, v2.4 036)
- 021 `ce62cbf86` - virt: define a canonical x86 CPU compatibility contract (v6.1 019, v6.0 023, v4.0 028, v3.0 037, v2.4 037)
- 022 `26374ae5a` - virt: record TSC-deadline support and advance APIC timers (v6.1 020, v6.0 024, v4.0 029, v3.0 038, v2.4 038; **partly superseded upstream**: microsoft/openvmm#4704, merged as `1c093df84` before the fork base, adds the same `TscDeadline` state element and backend accessors, whose chain copies this commit no longer adds; folds v2.4 324 (part))
- 023 `e345f79c8` - virt: add exact TSC-frequency helpers and snapshot clock hooks (v6.1 021, v6.0 025, v4.0 030, v3.0 039, v2.4 039)
- 024 `fe1f3f144` - virt_kvm: save and restore exact TSC and kvm-clock state (v6.1 022, v6.0 026, v4.0 031, v3.0 040, v2.4 040)
- 025 `87e5a1499` - virt_mshv: synchronize restored vCPU clocks and expose the exact TSC frequency (v6.1 023, v6.0 027, v4.0 032, v3.0 041, v2.4 041; folds v2.4 373 (part))
- 026 `fead6b254` - virt_whp: synchronize restored vCPU clocks and keep SMP TSC reads coherent (v6.1 024, v6.0 028, v4.0 033, v3.0 042, v2.4 042; folds v2.4 312 and 324 (part))
- 027 `fea603ce9` - vmm_core: advance VP TSC and APIC timers across downtime (v6.1 025, v6.0 029, v4.0 034, v3.0 043, v2.4 043)

</details>

<details>
<summary><strong>PR11 - Snapshot publication protocol (U)</strong> - 4 commits</summary>

- 028 `99707a0bf` - openvmm_defs: add opt-in snapshot lifecycle profiling (v6.1 026, v6.0 030, v4.0 035, v3.0 044, v2.4 044)
- 029 `d66488008` - openvmm_helpers: version the snapshot manifest format and bound its decoding (v6.1 027, v6.0 031, v4.0 036, v3.0 045, v2.4 045)
- 030 `4a44532d0` - openvmm_helpers: publish snapshots atomically through durable staging (v6.1 028, v6.0 032, v4.0 037, v3.0 046, v2.4 046; folds v6.1 267 (part) and 269)
- 031 `eee71081d` - openvmm: publish the exact file-backed RAM handle (v6.1 029, v6.0 033, v4.0 038, v3.0 047, v2.4 047; folds v2.4 317 (part))

</details>

<details>
<summary><strong>PR12 - Immutable private-CoW restore (U)</strong> - 5 commits</summary>

- 032 `75f1282c7` - membacking: map guest RAM privately copy-on-write with fault accounting (v6.1 030, v6.0 034, v4.0 039, v3.0 048, v2.4 048; folds v2.4 375 (part))
- 033 `a5a9e5aa6` - openvmm_helpers: open snapshots once and read artifacts relative to the directory (v6.1 031, v6.0 035, v4.0 040, v3.0 049, v2.4 049; folds v5.1 221 (part), and v6.1 264)
- 034 `d5ef7dbb3` - openvmm: restore RAM through a private CoW mapping and retain generation handles (v6.1 032, v6.0 036, v4.0 041, v3.0 050, v2.4 050; folds v2.4 375 (part) and v5.1 221 (part), and v6.1 267 (part))
- 035 `d43a30a90` - openvmm: signal restore readiness to a local socket or pipe (v6.1 033, v6.0 037, v4.0 042, v3.0 051, v2.4 051)
- 036 `d7e43e399` - virt_whp: resolve unmapped-GPA faults and register restored RAM lazily (v6.1 034, v6.0 038, v4.0 043, v3.0 052, v2.4 052; folds v2.4 313 and 382 (part))

</details>

<details>
<summary><strong>PR13 - Generic virtio persistence (U)</strong> - 12 commits</summary>

- 037 `9d5bde6b6` - virtio: add device-private saved state and restore it while the device is stopped (v6.1 035, v6.0 039, v4.0 044, v3.0 053, v2.4 053; folds v2.4 372; rewritten by the v4.0 reorganization to restore device-private state into the stopped device, for microsoft/nvx#1232)
- 038 `a15ebdc8d` - virtio: forward input quiesce and resume through transports (v6.1 036, v6.0 040, v4.0 045, v3.0 054, v2.4 054; folds v6.1 240 (part))
- 039 `c032b542b` - serial_socket: drop the connection on any write failure (v6.1 037, v6.0 041, v4.0 015, v3.0 019, v2.4 020; folds v4.0 218, which explains and tests the write handling; moved from PR06 by the v5.0 reorganization: it carried the first revision of microsoft/openvmm#4605, which merged narrowed to the read path as `48e0681e9`, and stays for the guest output that position 040's `Retain` policy and PR15's portb console keep for the next peer, so it goes upstream with this PR)
- 040 `001b572b6` - virtio_console: add attachment identities and reconnect and disconnect policies (v6.1 038, v6.0 042, v4.0 046, v3.0 055, v2.4 055; folds v2.4 380 (part))
- 041 `55c415e56` - virtio_console: persist buffered direct-console I/O (v6.1 039, v6.0 043, v4.0 047, v3.0 056, v2.4 056; folds v4.0 219, which tests that a `Retain` console keeps output for the next peer, re-expressed for this commit's harness, and v6.1 240 (part))
- 042 `0f02e7336` - net_backend: add an endpoint queue quiesce contract (v6.1 040, v6.0 044, v4.0 048, v3.0 057, v2.4 057)
- 043 `21b2194a4` - net_tap: quiesce queues and retain pending transmit ownership (v6.1 041, v6.0 045, v4.0 049, v3.0 058, v2.4 058; folds v2.4 309 (part) and 379 (part))
- 044 `cf5d31578` - net_consomme: quiesce endpoint queues (v6.1 042, v6.0 046, v4.0 050, v3.0 059, v2.4 059)
- 045 `058ad5710` - consomme: add an exact static IPv4 and gateway identity (v6.1 043, v6.0 047, v4.0 051, v3.0 060, v2.4 060)
- 046 `eb9e0e00b` - virtio_net: persist drained queue state for static-identity NICs (v6.1 044, v6.0 048, v4.0 052, v3.0 061, v2.4 061; folds v6.1 240 (part) and 268)
- 047 `3daff84aa` - fuse: save and restore negotiated session state (v6.1 045, v6.0 049, v4.0 053, v3.0 062, v2.4 062)
- 048 `9bb18f33f` - virtiofs: track inode aliases through link, rename, and unlink (v6.1 046, v6.0 050, v4.0 054, v3.0 063, v2.4 063; folds v7.2 340 (part) and 349 (part))

</details>

<details>
<summary><strong>PR14 - Firmware-less direct boot, loader half (U)</strong> - 1 commit + 4 landed upstream</summary>

- 049 `d72e90d60` - vmm_core: make level-triggered legacy IRQs explicit (v6.1 047, v6.0 054, v4.0 058, v3.0 067, v2.4 067; **not upstream**: microsoft/openvmm#4611 was closed after review suggested combining it with the commit that needs it; folds v2.4 326)

Landed upstream and dropped from the chain:

- ~~`6cea1ae18` - loader: let static ELF loading reuse a caller buffer~~
  (**superseded upstream**: microsoft/openvmm#4554, merged as `e4728b7f4`, loads the kernel and initrd through `load_kernel_and_initrd_x64`, which needs no shared buffer; v6.0 position 051)
- ~~`4ccef3c27` - loader: add an Intel MP 1.4 table builder~~
  (**landed upstream**: microsoft/openvmm#4554, merged as `e4728b7f4` with a table that `zerocopy` builds from the specification's structures; v6.0 position 052)
- ~~`b585c7300` - loader: add firmware-less Linux direct boot with MP tables~~
  (**landed upstream**: microsoft/openvmm#4554, merged as `e4728b7f4` as `loader::linux::mptable_boot`; v6.0 position 053)
- ~~`f6e6ff738` - loader_defs: name the boot_params ACPI RSDP field~~
  (**landed upstream**: microsoft/openvmm#4661, merged as `8beae2961`; v2.2 position 074)

</details>

<details>
<summary><strong>PR15 - microVM machine profile and minimal chipset (P)</strong> - 11 commits</summary>

- 050 `abeabf34d` - openvmm_defs: add machine-profile selection (v6.1 048, v6.0 055, v4.0 059, v3.0 068, v2.4 068)
- 051 `d1e9bbfd8` - virt: describe the deterministic microVM topology (v6.1 049, v6.0 056, v4.0 060, v3.0 069, v2.4 069)
- 052 `e37836367` - chipset: add an RTC mode seam (v6.1 050, v6.0 057, v4.0 061, v3.0 070, v2.4 070; folds v6.1 271 (part))
- 053 `823206bff` - vm_manifest_builder: add the minimal microVM base chipset without ACPI (v6.1 051, v6.0 058, v4.0 062, v3.0 071, v2.4 071)
- 054 `52aa71dc5` - vmm_core: add status-bearing guest power-off (v6.1 052, v6.0 059, v4.0 063, v3.0 072, v2.4 072)
- 055 `107908042` - chipset: add the microVM portb console (v6.1 053, v6.0 060, v4.0 064, v3.0 073, v2.4 073)
- 056 `906efc23a` - chipset: add the microVM shutdown and exit-status port (v6.1 054, v6.0 061, v4.0 065, v3.0 074, v2.4 074)
- 057 `b8b2c3f46` - openvmm_core: boot microVMs through MP-table direct load (v6.1 055, v6.0 062, v4.0 066, v3.0 075, v2.4 075; folds v2.4 315 and 314 (part), and v6.1 263 (part))
- 058 `653edfa7f` - openvmm_core: reject host save, pulse save/restore, and worker restart for microVMs (v6.1 056, v6.0 063, v4.0 067, v3.0 076, v2.4 076; folds v2.4 314 (part))
- 059 `7b3eacc10` - openvmm_entry: expose `--machine microvm` (v6.1 057, v6.0 064, v4.0 068, v3.0 077, v2.4 077; folds v2.4 316 (part), 317 (part), and 320 (part))
- 060 `2dd8e1e92` - petri, vmm_tests: add a microVM builder, portb harness, and cold-boot test (v6.1 058, v6.0 065, v4.0 069, v3.0 078, v2.4 078; folds v2.4 380 (part))

</details>

<details>
<summary><strong>PR16 - Fixed virtio-MMIO ABI and shared interrupt status (P)</strong> - 4 commits</summary>

- 061 `f5753d818` - virtio: allow feature masking for virtio-MMIO devices (v6.1 059, v6.0 066, v4.0 070, v3.0 079, v2.4 079)
- 062 `b02e4feb1` - openvmm: place virtio devices at fixed microVM MMIO slots (v6.1 060, v6.0 067, v4.0 071, v3.0 080, v2.4 080; folds v2.4 314 (part))
- 063 `33e962718` - virtio: add shared MMIO interrupt status and ACK doorbells (v6.1 061, v6.0 068, v4.0 072, v3.0 081, v2.4 081)
- 064 `e8369480d` - openvmm: use shared interrupt status for fixed microVM slots (v6.1 062, v6.0 069, v4.0 073, v3.0 082, v2.4 082)

</details>

<details>
<summary><strong>PR17 - microVM snapshot capture and authoritative restore (P)</strong> - 11 commits</summary>

- 065 `548bf2ec2` - vmm_core: stop VPs at a deferred-I/O boundary (v6.1 063, v6.0 070, v4.0 074, v3.0 083, v2.4 083)
- 066 `a97c5403d` - chipset: add the microVM snapshot-request port (v6.1 064, v6.0 071, v4.0 075, v3.0 084, v2.4 084)
- 067 `cb1ed23f7` - openvmm_core: establish the post-OUT snapshot boundary in the worker (v6.1 065, v6.0 072, v4.0 076, v3.0 085, v2.4 085; folds v6.1 236 (part))
- 068 `f7b10bf2c` - openvmm_core: fence management mutations during snapshot boundaries (v6.1 066, v6.0 073, v4.0 077, v3.0 086, v2.4 086)
- 069 `6003b612a` - openvmm_helpers: define the authoritative microVM machine contract (v6.1 067, v6.0 074, v4.0 078, v3.0 087, v2.4 087; folds v7.2 316 (part))
- 070 `273c049ff` - openvmm_core: propagate the guest TSC frequency for snapshot-capable cold boots (v6.1 068, v6.0 075, v4.0 079, v3.0 088, v2.4 088)
- 071 `1f5dab496` - openvmm_entry: capture guest-requested microVM snapshots (v6.1 069, v6.0 076, v4.0 080, v3.0 089, v2.4 089; folds v2.4 328 (part), and v6.1 265 (part) and 267 (part))
- 072 `1163122f9` - virt: build a reproducible CPU and clock contract for microVM partitions (v6.1 070, v6.0 077, v4.0 081, v3.0 090, v2.4 090)
- 073 `7cc2dc799` - openvmm_core: validate restored CPU and clock contracts and apply host downtime (v6.1 071, v6.0 078, v4.0 082, v3.0 091, v2.4 091)
- 074 `a11b40aac` - openvmm_entry: restore microVMs authoritatively (v6.1 072, v6.0 079, v4.0 083, v3.0 092, v2.4 092)
- 075 `464aa9d27` - vmm_tests: check that an unconfigured microVM snapshot request continues (v6.1 073, v6.0 080, v4.0 084, v3.0 093, v2.4 093)

</details>

<details>
<summary><strong>PR18 - microVM host attachments and device persistence (P)</strong> - 12 commits</summary>

- 076 `d3d2a088f` - openvmm_entry: add connect= serial backends with bounded timeouts (v6.1 074, v6.0 081, v4.0 085, v3.0 094, v2.4 094; folds v2.4 144)
- 077 `429d01c0f` - openvmm: persist the microVM boot console and rebuild its attachment on restore (v6.1 075, v6.0 082, v4.0 086, v3.0 095, v2.4 095)
- 078 `977bd0f2f` - openvmm: attach a portable Consomme NIC to microVMs (v6.1 076, v6.0 083, v4.0 087, v3.0 096, v2.4 096; folds v2.4 376 and 316 (part), v6.1 241 (part), and v7.2 302 (part))
- 079 `53bd06b6e` - openvmm: persist the microVM NIC and restore a fresh endpoint generation (v6.1 077, v6.0 084, v4.0 088, v3.0 097, v2.4 097)
- 080 `36e98b3ba` - net_tap: support persistent TAP attachments (v6.1 078, v6.0 085, v4.0 089, v3.0 098, v2.4 098)
- 081 `dbf0fa400` - virtiofs: add the fixed microVM filesystem profile (v6.1 079, v6.0 086, v4.0 090, v3.0 099, v2.4 099; folds v7.2 333 (part), 335 (part), and 347 (part))
- 082 `aac0b0967` - openvmm_entry: expose `--mount` for the microVM HostFs slot (v6.1 080, v6.0 087, v4.0 091, v3.0 100, v2.4 100; folds v7.2 335 (part))
- 083 `6315c16c5` - virtiofs: persist microVM FUSE namespaces, handles, and directory cookies (v6.1 081, v6.0 088, v4.0 092, v3.0 101, v2.4 101; folds v2.4 335 (part), v6.1 240 (part), and v7.2 333 (part), 342 (part), 345, 351 (part), and 365 (part))
- 084 `e6ce5a654` - openvmm: snapshot the microVM filesystem and revalidate it on restore (v6.1 082, v6.0 089, v4.0 093, v3.0 102, v2.4 102)
- 085 `77eebd10f` - openvmm: reserve a dormant microVM filesystem slot (v6.1 083, v6.0 090, v4.0 094, v3.0 103, v2.4 103; folds v2.4 335 (part), and v7.2 335 (part) and 347 (part))
- 389 `350018ce4` - virtiofs: let a new microVM object take the last node ID (microsoft/openvmm#5281; **fold into 081**, its test into 350)
- 393 `5af753b17` - virtiofs: reopen a microVM handle with O_DIRECT on restore (microsoft/openvmm#5285; **fold into 083**, its test into 335)

</details>

<details>
<summary><strong>PR19 - Sandbox block roles and snapshot tiers (N/P)</strong> - 3 commits</summary>

- 086 `6830760b7` - openvmm: add fixed-role microVM sandbox block slots (v6.1 084, v6.0 091, v4.0 095, v3.0 104, v2.4 104; folds v2.4 320 (part), and v7.2 338 (part))
- 087 `00ff83ce5` - openvmm: snapshot sandbox blocks with paired or fresh scratch (v6.1 085, v6.0 092, v4.0 096, v3.0 105, v2.4 105; folds v2.4 317 (part), and v7.2 338 (part))
- 088 `ce4ead69a` - openvmm: add clone and resume tiers with single-use resume claims (v6.1 086, v6.0 093, v4.0 097, v3.0 106, v2.4 106; folds v2.4 320 (part), and v6.1 243 (part))

</details>

<details>
<summary><strong>PR20 - Restore-time processor and memory activation (P)</strong> - 9 commits</summary>

- 089 `16b103720` - chipset: expose a stable generation ID and restore entropy on portb (v6.1 087, v6.0 094, v4.0 098, v3.0 107, v2.4 107)
- 090 `f8ad9b978` - openvmm: gate external input until post-restore repair completes (v6.1 088, v6.0 095, v4.0 099, v3.0 108, v2.4 108; folds v2.4 335 (part), and v6.1 240 (part))
- 091 `3ab16c754` - vmm_core: instantiate an active VP prefix (v6.1 089, v6.0 096, v4.0 100, v3.0 109, v2.4 109)
- 092 `35fefa13f` - openvmm: bring a restore-time processor prefix online (v6.1 090, v6.0 097, v4.0 101, v3.0 110, v2.4 110; folds v2.4 141, 142 (part), and 325 (part), and v5.1 148)
- 093 `7b26a119f` - openvmm_core: back expandable restore RAM with fresh private memory (v6.1 091, v6.0 098, v4.0 102, v3.0 111, v2.4 111; folds v6.1 242 (part), and v7.2 302 (part))
- 094 `85d5d0828` - openvmm: add bounded restore-time memory expansion (v6.1 092, v6.0 099, v4.0 103, v3.0 112, v2.4 112; folds v2.4 320 (part))
- 095 `e960c809b` - openvmm_entry: pre-expand the Linux descriptor table at startup (v6.1 093, v6.0 100, v4.0 104, v3.0 113, v2.4 113)
- 375 `d44a113be` - openvmm_core: stop a gated restore's deadline at its acknowledgement (microsoft/openvmm#5265; **fold into 090**, its `restore_acknowledged` changes and unit tests into 134, its VMM test into 374)
- 376 `c440ce3cc` - openvmm: say that the restore readiness event precedes guest repair (microsoft/openvmm#5270; **fold into 090**, its `--restore-processors` changes into 092, its hotplug sentence into 142)

</details>

<details>
<summary><strong>PR21 - Network egress policy (P/N)</strong> - 6 commits</summary>

- 096 `8db534880` - net_backend_resources: define a canonical bound IPv4 egress policy (v6.1 094, v6.0 101, v4.0 105, v3.0 114, v2.4 114)
- 097 `fd9fbd943` - net_backend: let endpoints accept a run-scoped egress policy (v6.1 095, v6.0 102, v4.0 106, v3.0 115, v2.4 115)
- 098 `ddbcaa699` - net_consomme: enforce the egress policy on final transmitted bytes (v6.1 096, v6.0 103, v4.0 107, v3.0 116, v2.4 116; folds v2.4 379 (part))
- 099 `3d08dcbb5` - net_tap: enforce the egress policy on TAP transmit (v6.1 097, v6.0 104, v4.0 108, v3.0 117, v2.4 117; folds v2.4 309 (part) and 379 (part))
- 100 `4026cead9` - virtio_net: install and prefilter the egress policy (v6.1 098, v6.0 105, v4.0 109, v3.0 118, v2.4 118)
- 101 `de90ec83b` - openvmm: add microVM network egress policy and bind it into snapshots (v6.1 099, v6.0 106, v4.0 110, v3.0 119, v2.4 119; folds v2.4 320 (part))

</details>

<details>
<summary><strong>PR22 - Filesystem deny policy (P)</strong> - 2 commits</summary>

- 102 `9f1580ceb` - virtiofs: enforce denied subtrees by object identity (v6.1 100, v6.0 107, v4.0 111, v3.0 120, v2.4 120)
- 103 `3811c4580` - openvmm: add --mount-deny and record denied paths in snapshots (v6.1 101, v6.0 108, v4.0 112, v3.0 121, v2.4 121; folds v2.4 320 (part))

</details>

<details>
<summary><strong>PR23 - Authenticated control console (N)</strong> - 10 commits</summary>

- 104 `a28d4337a` - serial_core: expose local peer identity and active-peer eviction (v6.1 102, v6.0 109, v4.0 113, v3.0 122, v2.4 122)
- 105 `90d4f3ef3` - serial_socket: report Unix and named-pipe peer identities (v6.1 103, v6.0 110, v4.0 114, v3.0 123, v2.4 123; folds v2.4 374 (part), and v6.1 262 (part))
- 106 `e3e4c6f08` - virtio_console: define the control-session record protocol (v6.1 104, v6.0 111, v4.0 115, v3.0 124, v2.4 124)
- 107 `8feaa2253` - virtio_console: add the control-session broker state machine (v6.1 105, v6.0 112, v4.0 116, v3.0 125, v2.4 125)
- 108 `a7a0401cc` - virtio_console: attach the broker to serial I/O with receive credits (v6.1 106, v6.0 113, v4.0 117, v3.0 126, v2.4 126; folds v2.4 330 (part) and 384, and v6.1 240 (part))
- 109 `4630fd15e` - virtio_console: save and restore broker state (v6.1 107, v6.0 114, v4.0 118, v3.0 127, v2.4 127)
- 110 `a5b08a916` - openvmm: reserve the dedicated microVM control-console slot (v6.1 108, v6.0 115, v4.0 119, v3.0 128, v2.4 128)
- 111 `3a3ebabf9` - openvmm_helpers: record the control console in the microVM machine contract (v6.1 109, v6.0 116, v4.0 120, v3.0 129, v2.4 129)
- 112 `221fecc21` - openvmm_entry: add authenticated microVM control-console endpoints (v6.1 110, v6.0 117, v4.0 121, v3.0 130, v2.4 130; folds v2.4 330 (part), 374 (part), 385, 317 (part), and 320 (part), and v6.1 262 (part))
- 113 `6c4c9bcdf` - openvmm_entry: restore listener endpoint rebinding (v6.1 111, v6.0 118, v4.0 122, v3.0 131, v2.4 143)

</details>

<details>
<summary><strong>PR24 - Managed workload and outcome reporting (N)</strong> - 5 commits</summary>

- 114 `030f308bc` - openvmm_entry: drain portb output before guest-requested exit (v6.1 112, v6.0 119, v4.0 123, v3.0 132, v2.4 131; folds v2.4 148 (part))
- 115 `3a57e9c79` - openvmm: add a fixed non-root workload identity (v6.1 113, v6.0 120, v4.0 124, v3.0 133, v2.4 132; folds v2.4 320 (part))
- 116 `ee27877aa` - openvmm: support a managed workload lifecycle (v6.1 114, v6.0 121, v4.0 125, v3.0 134, v2.4 133)
- 117 `4d9777800` - openvmm_entry: write bounded microVM outcome reports (v6.1 115, v6.0 122, v4.0 126, v3.0 135, v2.4 134)
- 118 `ccc6545c1` - Guide: describe teardown after a failed snapshot boundary (v6.1 116, v6.0 123, v4.0 127, v3.0 136, v2.4 135)

</details>

<details>
<summary><strong>PR25 - Management plane (P)</strong> - 3 commits</summary>

- 119 `ac6f0d9ec` - vmservice: create, capture, and restore microVMs over the management RPC (v6.1 117, v6.0 124, v4.0 128, v3.0 137, v2.4 136; folds v2.4 137 (part), 142 (part), 145, 148 (part), 310, 321, 377 (part), and 381, and v7.2 335 (part) and 337 (part))
- 120 `2c2e3c9ee` - ttrpc: propagate nonzero guest exit status (v6.1 118, v6.0 125, v4.0 129, v3.0 138, v2.4 137; folds v2.4 148 (part), v6.1 266, and v7.2 335 (part))
- 377 `3f5506f9d` - vmservice: say that the restore readiness event precedes guest repair (microsoft/openvmm#5270; **fold into 119**)

</details>

<details>
<summary><strong>PR26 - Profiling benchmark (U)</strong> - 1 commit</summary>

- 121 `e063cfba1` - openvmm_helpers: add a publication and private-restore benchmark (v6.1 119, v6.0 126, v4.0 130, v3.0 139, v2.4 138; folds v2.4 375 (part), and v6.1 265 (part))

</details>

<details>
<summary><strong>PR27 - Safe symlinks for writable microVM shares (U/P)</strong> - 2 commits</summary>

- 125 `067908221` - lxutil: add path confinement that never follows symbolic links (v6.1 123, v6.0 130, v4.0 134, v3.0 143, v2.4 146)
- 126 `c6e62f286` - virtiofs: allow symlink creation on read-write microVM shares (v6.1 124, v6.0 131, v4.0 135, v3.0 144, v2.4 147)

</details>

<details>
<summary><strong>Fork-only tail (F)</strong> - 3 commits</summary>

- 122 `a54969a0b` - ci: restrict privileged, self-hosted, and publishing jobs to microsoft/openvmm (v6.1 120, v6.0 127, v4.0 131, v3.0 140, v2.4 139; folds v2.4 383)
- 123 `7d7c76c6a` - ci: run the checkin gates on the feature/microvm branches (v6.1 121, v6.0 128, v4.0 132, v3.0 141, v2.4 308)
- 124 `689b87818` - vmservice: keep NVX wire-tag compatibility (v6.1 122, v6.0 129, v4.0 133, v3.0 142, v2.4 140)

</details>
