# NVX Profiling and Flamegraphs

This document describes how to run the `microvm` profiler and prepare symbol-ready binaries so
sampled addresses resolve into names.

## What NVX profiling captures

- **Guest stack samples** (both backends): periodic host-side sampling interrupts the vCPU, reads
  `RIP/RBP/CR3`, walks the guest frame-pointer chain, and writes folded stacks. User-space samples
  are kept separate per process (keyed by the normalized `CR3`) while kernel samples are shared
  across processes; see [Notes](#notes).
- **Host tracing**: enabled by `--host-profile`.
  - Windows: WPR/ETW CPU trace (`.host.etl`).
  - Linux: `perf record` host trace (`.host.perf.data`).

Guest folded output contains raw guest stacks (root-first). The post-processing merge step applies
the `[GUEST]` root frame (and `[HOST]` for host stacks), so guest and host stacks are prefixed
uniformly in one place.

## CLI quick reference

```text
--guest-profile <file>         folded output path (enables guest sampling)
--profile-hz <hz>              sampling frequency (default: 997, max: 8190)
--kernel-symbols <elf[@base]>  guest kernel symbol file
--user-symbols <elf[@base][,..]>  guest user symbol file(s)
--host-profile                 enable host trace capture
--wpr-profile <NAME|FILE!NAME> Windows only: WPR recording profile (default: NvxCpuScheduling)
```

`--wpr-profile` selects the Windows host recording profile and requires `--host-profile`. It
accepts a bundled profile name (`NvxCpuScheduling` — the default — or the leaner `NvxCpu`), a
built-in WPR profile (e.g. `CPU`, `GeneralProfile`), or an explicit `path.wprp!ProfileName`. It
overrides the `NVX_WPR_PROFILE` environment variable. See
[Host recording profiles](#host-recording-profiles-windows) below.

Symbol files may carry an optional `@0x<base>` suffix giving the **absolute guest runtime load
address** of the image, e.g. `--user-symbols app.elf@0x555555554000`. Symbols are relocated by
`base - min(PT_LOAD.p_vaddr)`, so both a position-independent (PIE/ASLR) image and a relocated
fixed-address (`ET_EXEC`) image resolve correctly. A fixed-address image loaded at its link address
(e.g. a non-relocatable `vmlinux`) needs no base.

## Build requirements for symbol resolution

### 1. VMM (host binary)

Build the VMM:

```bash
cargo build --release
```

`cargo build` emits a symbol table for the VMM — a PDB next to `microvm.exe` on Windows, the ELF
symbol table on Linux — which is enough to resolve VMM frames to a function symbol. Because the
release profile is optimized with LTO, however, some frames resolve only to the **nearest** exported
symbol. For exact VMM function names (e.g. `microvm::whp::execute`,
`microvm::profiler::host::HostTraceSession::stop`), build the VMM with full debug info:

```bash
# Linux / macOS
CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release
```

```powershell
# Windows (PowerShell)
$env:CARGO_PROFILE_RELEASE_DEBUG = 'true'; cargo build --release
```

On **Linux**, host call-graph capture uses `perf record -g`, whose default user-space unwinder
follows frame pointers. The release profile enables LTO (see `Cargo.toml`), which otherwise omits
frame pointers and leaves host stacks shallow, so `.cargo/config.toml` forces them back on for
Linux builds:

```toml
[target.'cfg(target_os = "linux")']
rustflags = ["-C", "force-frame-pointers=yes"]
```

This is applied automatically by `cargo build` — no extra flags needed. On **Windows**, WHP host
stackwalking uses PDB unwind information rather than frame pointers, so the Windows binary is
intentionally left unchanged.

### 2. Guest kernel (`vmlinux`)

Use an unstripped `vmlinux` as the kernel symbol file (`--kernel-symbols`).

The guest profiler walks the **frame-pointer** chain, but the default kernel config
(`kernel/config-microvm`) uses the ORC unwinder and omits frame pointers
(`CONFIG_UNWINDER_ORC=y`, `# CONFIG_UNWINDER_FRAME_POINTER is not set`), so guest stacks would be
shallow. Build a profiling-ready kernel with the provided config overlay:

```bash
# Applies kernel/config-microvm-profiling on top of the base config via the kconfig merge helper.
python3 scripts/nvx.py build-kernel --profiling
```

`--profiling` writes the result to `$HOME/build/vmlinux-profiling` by default (so it never clobbers
the normal ORC `vmlinux`). The overlay sets:

- `CONFIG_UNWINDER_FRAME_POINTER=y`
- `# CONFIG_UNWINDER_ORC is not set`
- `CONFIG_FRAME_POINTER=y`
- `# CONFIG_SCHED_OMIT_FRAME_POINTER is not set`

The same profiling kernel is available through the supported Docker/Windows build wrappers, which
propagate a build argument and write `vmlinux-profiling` alongside the standard artifacts:

```powershell
# Windows (WHP), via Docker:
python scripts\nvx.py build-linux-artifacts --profiling --dest build
```

```bash
# Linux/macOS, via Docker:
python3 scripts/nvx.py build-linux-artifacts --profiling --dest build
# or the raw Docker build (the artifacts-profiling target forces PROFILE=1 internally and exports
# the profiling kernel as vmlinux-profiling, so no --build-arg is needed and it never overwrites an
# existing standard build/vmlinux):
docker build -f docker/Dockerfile --target artifacts-profiling --output type=local,dest=build .
```

Then profile against that kernel (`--kernel <vmlinux-profiling>` and
`--kernel-symbols <vmlinux-profiling>`).

### 3. Guest user workloads

Keep an unstripped ELF with symbol table (`.symtab`) for each profiled workload and pass it via
`--user-symbols`.

Compile guest user code with frame pointers (for C/C++ typically `-fno-omit-frame-pointer`).

## End-to-end (guest flamegraph)

Example (using a profiling-ready kernel built with `--profiling`, see above):

```bash
./target/release/microvm \
  --kernel "$HOME/build/vmlinux-profiling" \
  --initrd "$HOME/build/initramfs.cpio.gz" \
  --guest-profile "$HOME/build/nvx.guest.folded" \
  --kernel-symbols "$HOME/build/vmlinux-profiling" \
  --user-symbols "/path/to/app.elf.sym" \
  --profile-hz 997
```

Generate SVG (guest folded output is raw; add a `[GUEST]` root via the merge script, or render it
directly for a guest-only graph):

```bash
cat "$HOME/build/nvx.guest.folded" | rustfilt | inferno-flamegraph > "$HOME/build/nvx.guest.svg"
```

## Nanvix-style unified post-processing (guest + host)

Run with host tracing enabled:

```bash
./target/release/microvm ... --guest-profile "$HOME/build/nvx.folded" --host-profile
```

- Guest folded stacks are written to `--guest-profile`.
- Host traces are emitted as sibling files:
  - `<guest-profile-stem>.host.etl` on Windows
  - `<guest-profile-stem>.host.perf.data` on Linux

On Windows the trace is recorded in a dedicated nvx WPR *instance* (`nvxprofile`), so nvx never
disturbs an unrelated default WPR session. A machine-wide lock serializes host captures across
processes (see [Notes](#notes)), so the fixed instance name is safe and a session left behind by a
hard-killed prior run is cancelled and reclaimed by the next run.

### Host recording profiles (Windows)

By default nvx uses the bundled **`NvxCpuScheduling`** profile. nvx materializes
the bundled `src/profiler/nvx-cpu.wprp` next to the trace and selects the profile automatically, so the default
works regardless of the working directory.

The bundled profiles are scoped to what the flamegraph and scheduling analysis need: compared with
the built-in Windows `CPU` profile they drop its extra user-mode providers
(CLR/JScript/RPC/Win32k/networking) and system keywords (`CpuConfig`, `MemoryInfo`, `Power`) and use
a small fixed buffer pool. (For reference, a built-in `CPU` run of a sub-second nvx launch produced
a ~900 MiB ETL that took ~350 s to extract.)

The bundled `.wprp` defines two profiles:

- **`NvxCpu`** — CPU sampled-profile stacks only (no context-switch/ready-thread stacks). The
  leanest option and by far the smallest ETL; prefer it when trace size is the priority.
- **`NvxCpuScheduling`** (default) — everything in `NvxCpu` **plus** scheduling events and their
  stacks: context switch (`CSwitch`), wakeup (`ReadyThread`), thread-priority changes, and
  ideal-processor reassignment. Enables cold-start latency, preemption, wakeup, off-CPU, and
  CPU-affinity analysis. **Cost:** it captures the same high-volume `CSwitch`/`ReadyThread` stacks
  as the built-in `CPU` profile, so its ETL is *not* much smaller for scheduling-heavy runs. The CPU
  flamegraph still folds only `SampledProfile` events (scheduling events are read separately, e.g.
  WPA "CPU Usage (Precise)"). Prefer `NvxCpu` when ETL size matters most.

Select a profile with the CLI flag (preferred) or the environment variable:

```powershell
# Leanest sampled-only profile:
microvm ... --host-profile --wpr-profile NvxCpu
# A built-in WPR profile, or your own file:
microvm ... --host-profile --wpr-profile CPU
microvm ... --host-profile --wpr-profile "C:\path\my.wprp!MyProfileName"
# Equivalent via environment (CLI flag wins if both are set):
$env:NVX_WPR_PROFILE = "NvxCpu"
```

> **Ideal processor**: `NvxCpuScheduling` enables the `IdealProcessor` keyword and walks stacks on
> the `ThreadSetIdealProcessor`/`ThreadSetUserIdealProcessor` reassignment events, so
> affinity-reassignment analysis works directly. The switch-in target CPU is also carried by the
> `CSwitch`/`ReadyThread` records (the source of WPA's "Ideal CPU" columns).

Then run the post-processor:

```bash
# guest-only:
python scripts/flamegraph.py guest --guest-folded "$HOME/build/nvx.folded"

# guest + host (full E2E):
python scripts/flamegraph.py full --guest-folded "$HOME/build/nvx.folded"
```

By default this writes to `<guest-profile-dir>/<guest-profile-stem>-flamegraph/`:

- `guest.folded`, `guest.svg`
- `host.folded` (if host extraction succeeds), `host.svg`
- `merged.folded`, `flamegraph.svg` (combined `[GUEST]` + `[HOST]`)

Only artifacts that were actually written are reported. When an output directory is reused, stale
`guest.svg`, `host.svg`, `flamegraph.svg`, and `merged.folded` files from a previous run are
removed before rendering, so a run that produces no host data never leaves a misleading old graph
behind. In `full` mode, if host extraction fails the post-processor still emits the guest-only
outputs but exits non-zero so the failure is not mistaken for full success.

Windows host extraction streams `xperf -a dumper` output in a single pass and folds **only** CPU
sampled-profile (`SampledProfile`/`SampledProfileNmi`) events; scheduler events (context switches,
ready-thread, syscalls) are counted separately, never mixed into the CPU flamegraph. Each sample's
kernel and user fragments are combined into one root-first stack (via `-stacktimeshifting`), and
only one sample is held at a time, so memory is bounded by distinct stacks rather than total events.
The system-wide trace is narrowed to the recorder's PID via the run manifest (see [Notes](#notes));
with no PID it falls back to the `microvm(.exe)` image name (override with `--process <regex>`).
Dropped events are tolerated (`-tle`), but if xperf reports lost events `full` mode warns and exits
nonzero rather than present an incomplete capture as complete.

Windows host **symbol** resolution is automatic for the VMM's own frames: extraction prepends the
recorder's image directory (from the run manifest) to `_NT_SYMBOL_PATH`, so `microvm.exe` frames
resolve from the `microvm.pdb` that `cargo build` emits next to the binary, and any ambient
`_NT_SYMBOL_PATH` is still searched after it. **System** modules (`ntoskrnl.exe`,
`WinHvPlatform.dll`, `Vid.sys`, ...) resolve from cached symbols; without them, add the Microsoft
symbol server before post-processing:

```powershell
$env:_NT_SYMBOL_PATH = 'srv*C:\symbols*https://msdl.microsoft.com/download/symbols'
```

## Notes

- The profiler is opt-in and disabled unless `--guest-profile` is set.
- Sampling overhead scales with `--profile-hz`; start around 997 Hz.
- User stacks are aggregated per process (keyed by normalized `CR3`), kernel stacks across all
  processes (keyed `0`). With more than one user address space, user frames are rooted under an
  `[as 0x<cr3>]` frame so processes reusing the same virtual addresses never conflate; a
  single-process profile is untagged. Kernel and user symbols come from **separate** tables
  (`--kernel-symbols` for upper-half addresses, `--user-symbols` for lower-half), so supplying both
  is never mistaken for an image overlap. Since the user ELF can't be tied to a specific process,
  user frames resolve to names only when a single user address space is sampled; with several they
  stay raw addresses (kernel frames still resolve).
- Guest and host samples sit under separate `[GUEST]` and `[HOST]` roots. Widths are comparable
  **within** a root but **not across** them: the guest sampler snapshots only the boot vCPU each
  interval, while the host recorder samples every scheduled VMM thread, so `[HOST]` aggregates
  several threads' time per interval and `[GUEST]` one. Read each root independently. Because of
  this, nvx changes no machine-global sampling rate — on Windows it leaves the `xperf`/WPR interval
  untouched (it is system-wide and persists past exit, so a killed run could otherwise leak it), and
  on Linux it requests `perf -F <hz>` without `--strict-freq` so perf may clamp to a kernel-permitted
  rate rather than disable host tracing. `--profile-hz` is capped at 8190 Hz as a resource guard.
  The WPR session is machine-global, so a global named mutex serializes host captures: only one runs
  at a time; a second skips host tracing rather than race.
- Frames are classified by the canonical half of the sampled `RIP` (upper → kernel table, lower →
  user table). Two boundary cases aren't distinguished by half alone and are left unsymbolized or
  attributed to the kernel table: very early PVH boot (32-bit low-`RIP` code before paging) and the
  legacy vsyscall page. Both are transient/legacy and negligible in a normal profile.
- Memory and cost are bounded: samples are aggregated incrementally by distinct stack, the
  frame-pointer walk is capped at 64 frames, and per-page translations are cached within a sample.
  Retained state has a fixed 64 MiB budget; past it, new stacks are counted under a synthetic
  `[overflow]` frame rather than growing unbounded. Folded output is streamed to disk.
- A host trace is published to its final path only after the recorder finalizes cleanly; a failed or
  interrupted capture leaves no file, so post-processing reports a missing trace rather than
  consuming partial data. On Windows the recorder writes a unique `.partial.etl` and accepts it only
  once `wpr -stop` succeeds **and** the file exists; a failed stop cancels the WPR instance.
- Guest and host artifacts share a per-run **run id** (written to `<guest-profile>.run` and the host
  trace's `.pid` manifest). `full` mode merges host stacks only when both ids are present and
  identical, so a stale host trace from an earlier run is never merged as current.
- If stacks are shallow, verify frame-pointer settings in guest kernel/user binaries (build the
  kernel with `--profiling`, above).
- Symbol resolution keeps only executable-section symbols and prefers `.symtab` (falling back to
  `.dynsym` for stripped binaries). At a shared address a real function (`STT_FUNC`) wins over a
  zero-size label (`STT_NOTYPE`, e.g. `_stext`); each symbol's extent is bounded by its section end,
  so an address past `.text` is shown as raw hex, while one past a nested inner label still resolves
  to the enclosing function. Overlapping **user** images make the profiler fail fast — supply
  per-image load bases with `path@0x<base>` (honored for `ET_EXEC` too). Non-canonical return
  addresses from a broken frame-pointer chain are dropped, not rendered as fake frames.
- Tools needed for post-processing:
  - All platforms: `rustfilt`, `inferno-flamegraph` (`cargo install inferno rustfilt`)
  - Linux host stacks: `perf` + `inferno-collapse-perf`
  - Windows host stacks: `xperf` (Windows Performance Toolkit)
