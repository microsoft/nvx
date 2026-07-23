# Contributing

NVX changes often span a host backend, shared boot or device code, guest initialization, and the
Python workflow that proves the behavior. Keep changes focused and validate the smallest affected
path before broadening to both hosts.

## Prepare a development environment

Follow [Host Setup](setup.md), then build the VMM and guest artifacts as described in
[Building NVX](build.md). A full end-to-end checkout needs KVM on Linux or WHP on Windows, but unit
tests and most Python tooling tests do not require a hypervisor.

The repository pins stable Rust through `rust-toolchain.toml` and commits `Cargo.lock`. Python code
supports Python 3.10 and newer and uses only the standard library for the development workflows.

## Sources of truth

When behavior and prose differ, update them together. These files own the public contracts:

| Contract | Source |
| --- | --- |
| Direct VMM options and platform rejection | `src/main.rs` |
| Python workflow commands and defaults | `scripts/nvx_tools/cli.py` |
| Make targets and artifact paths | `Makefile` |
| Guest boot and feature setup | `alpine/init`, `alpine/init.python` |
| External network JSON schema | `src/l2bridge.rs` |
| Required CI checks and benchmark policy | `.github/workflows/ci.yml` |
| Canonical benchmark metrics | `scripts/performance.py`, `doc/benchmark.md` |

Generated `target/`, `build/`, and local snapshot directories are not source. A stale executable
can advertise behavior from another revision, so rebuild before using `microvm --help` as a check.

## Development checks

Run these platform-independent checks for Rust or Python changes:

```console
cargo fmt --all --check
cargo build --release
cargo test --release
python -m compileall -q scripts alpine
python -m unittest scripts/test_performance.py scripts/tests/test_nvx_tools.py -v
```

Use `python3` on Linux where appropriate. CI currently compiles and tests release Rust on both
Linux and Windows, compiles the Python and Alpine sources, and runs the Python unit suites on both
hosts. `cargo fmt --check` is a contributor-side check and should remain clean even though the
workflow does not currently run it explicitly.

PowerShell changes should parse under Windows PowerShell 5.1 or newer. The Windows CI job parses
all HCN/AF_XDP scripts before running them.

## Backend validation

After unit tests, use the narrowest relevant executable check:

| Change area | Minimum focused validation |
| --- | --- |
| PVH entry, registers, memory layout | `microvm --selftest`, then `scripts/nvx.py test-boot` |
| Kernel config, patch, or base initramfs | Rebuild Linux artifacts, then `test-boot` |
| Snapshot state | Relevant snapshot workflow with at least one cold capture and two restores |
| Virt-fs | Read-only smoke test or `bench-virtfs --runs 1`, matching the changed mode |
| Standalone networking | Guest gateway probe and `bench-net-snapshot --runs 1` |
| Python guest | Rebuild the Python initramfs and run the relevant snapshot smoke test |
| HCN/AF_XDP | `test-hcn-afxdp.ps1` on a provisioned Windows host |
| Performance parser or metric | Python unit tests and `performance.py collect` on representative logs |

Run both KVM and WHP when changing shared boot structures, PMIO, console behavior, kernel command
line generation, guest PID 1, virtio transport state, snapshots, or Python workflows. If one
backend is unavailable, state that limitation in the pull request.

## Code changes

- Preserve the minimal machine model unless the change deliberately extends it. Avoid introducing
  PCI, ACPI, or a general chipset to solve a feature that fits an existing PMIO, virtio-mmio, or
  mapped-memory mechanism.
- Keep backend-specific host APIs behind `cfg(target_os = ...)`; put guest-visible contracts in
  shared modules when both backends must agree.
- Validate serialized and external input before allocating host resources. Snapshot magic and
  version changes need explicit compatibility decisions and tests.
- Keep guest command-line fragments bounded. The external network bootstrap shares the kernel
  command-line limit with all other boot options.
- Add unit tests for parsers, layout calculations, serialization framing, and device state. Add an
  end-to-end smoke test when correctness depends on a real hypervisor or guest driver.
- Do not commit generated kernels, initramfs images, VMM binaries, snapshots, benchmark logs, or
  local interface descriptors.

Follow existing Rust and Python style. Comments should explain device contracts, ordering, or
safety conditions rather than restating code.

## Documentation changes

Update the page that owns the behavior:

| Change | Documentation |
| --- | --- |
| Host prerequisite | `doc/setup.md` |
| Build command, version, or artifact | `doc/build.md` |
| CLI or normal launch behavior | `doc/usage.md` |
| Module ownership or machine model | `doc/project-organization.md` |
| Snapshot, virt-fs, or network contract | Matching feature page in `doc/` |
| Metric, sample method, or CI gate | `doc/benchmark.md` |

Keep the root README as an entry point rather than duplicating detailed instructions. Check every
relative link after moving or renaming a page.

## Performance changes

CI collects the 23 metrics in [Benchmark Reference](benchmark.md) on KVM and WHP. The optional
HCN/AF_XDP lane emits the same metric set with its external-network measurement.

For pull requests, the current workflow compares each p50 with the latest 10 p50 values from the
base branch. A regression must exceed 50%. For lower-is-better millisecond metrics it must also be
more than 10 ms slower; this prevents tiny timing changes from failing on percentage alone.
Higher-is-better throughput metrics use the percentage threshold. Metrics without enough history
are reported as warmups.

Run only the benchmark affected by a narrow change during iteration. Run the shared suite when
changing timing, memory, console, device transport, snapshot, or workflow behavior broadly.
Successful main builds append new p50 values to `data/performance/`; do not hand-edit those CSVs for
a code change.

## Pull request checklist

- Explain the user-visible or machine-visible contract being changed.
- Keep generated files and unrelated formatting out of the diff.
- Add or update focused tests.
- Run the platform-independent checks.
- Run the relevant backend smoke test and feature workflow.
- Update documentation and CLI help together.
- Report any backend or hardware validation that could not be run.
- Call out snapshot format, guest ABI, or performance implications explicitly.

Contributions are licensed under the repository's MIT license. Preserve existing provenance when
modifying code derived from the Nanvix Micro-VM.
