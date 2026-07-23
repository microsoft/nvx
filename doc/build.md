# Building NVX

An NVX installation has three build products:

| Product | Source | Typical output |
| --- | --- | --- |
| Host VMM | Rust in `src/` | `target/release/microvm` or `microvm.exe` |
| Guest kernel | Linux LTS plus `kernel/` | `vmlinux` |
| Guest root filesystem | Alpine plus `alpine/` | `initramfs.cpio.gz` |

The kernel and initramfs are Linux artifacts even when the VMM runs on Windows. Build them
natively on Linux or use the repository's Docker build from either host.

## Build on Linux

With the native dependencies from [Host Setup](setup.md) installed, build all three products:

```console
make world
```

By default this writes the VMM to `target/release/microvm` and the guest artifacts to:

```text
$HOME/build/vmlinux
$HOME/build/initramfs.cpio.gz
```

Then validate and boot them:

```console
make selftest
make boot-test
make run
```

The equivalent artifact commands are:

```console
python3 scripts/nvx.py build-kernel
python3 scripts/nvx.py build-initramfs
cargo build --release
```

`build-kernel` downloads Linux, applies the PVH/portb changes from `kernel/`, installs
`kernel/config-microvm`, runs `make olddefconfig`, and builds the uncompressed `vmlinux`. It also
checks the result for the Xen PVH entry note. `build-initramfs` downloads the Alpine mini root
filesystem, adds the NVX PID 1 and snapshot helper, and packs a gzipped `newc` cpio archive.

## Build guest artifacts with Docker

The portable build uses `docker/Dockerfile` and exports only the finished artifacts:

```console
python scripts/nvx.py build-linux-artifacts --dest build
```

It produces:

```text
build/vmlinux
build/initramfs.cpio.gz
```

The Docker kernel and initramfs stages can build in parallel and reuse normal Docker layer caches.
On Windows this is the standard guest-artifact build. On Linux, pass these non-default paths to the
workflow wrapper if you choose `build/` instead of `$HOME/build`:

```console
python3 scripts/nvx.py run \
  --kernel build/vmlinux \
  --initrd build/initramfs.cpio.gz
```

The underlying Docker command is:

```console
docker build -f docker/Dockerfile \
  --target artifacts \
  --output type=local,dest=build .
```

## Build on Windows

Build the Linux artifacts with Docker, then compile the native WHP VMM:

```powershell
python scripts\nvx.py build-linux-artifacts --dest build
cargo build --release
.\target\release\microvm.exe --selftest
python scripts\nvx.py test-boot
```

The workflow wrapper uses `build\vmlinux` and `build\initramfs.cpio.gz` by default on Windows.

## Python initramfs

Snapshot demonstrations that use CPython, NumPy, and pandas require a larger Python initramfs.
Build it natively on Linux:

```console
make python-initramfs
```

or through Docker on either host:

```console
python scripts/nvx.py build-python-initramfs --docker --dest build
```

The output is `initramfs-python.cpio.gz`. The native Linux default is
`$HOME/build/initramfs-python.cpio.gz`; the portable Windows default is
`build/initramfs-python.cpio.gz`.

The image contains the applications in `alpine/hello.py`, `alpine/repl.py`,
`alpine/net-hello.py`, and `alpine/net-pandas.py`. PID 1 selects one with a `pyapp=<file>` kernel
command-line token.

## Rust build profiles

```console
cargo build             # target/debug/microvm[.exe]
cargo build --release   # target/release/microvm[.exe]
cargo test --release
```

Release builds use optimization level 3, link-time optimization, and the locked dependency graph
in `Cargo.lock`. The Python wrapper's `run --debug` option selects the debug VMM; other workflows
expect the release executable.

## Make targets

The Makefile is primarily a Linux convenience interface.

| Target | Action |
| --- | --- |
| `all`, `release` | Build the release VMM. |
| `world` | Build the release VMM, kernel, and base initramfs. |
| `build` | Build the debug VMM. |
| `test` | Run `cargo test --release`. |
| `kernel` | Build the native Linux `vmlinux`. |
| `initramfs` | Build the base Alpine initramfs. |
| `artifacts` | Build the kernel and base initramfs through Docker into `build/`. |
| `python-initramfs` | Build the Python initramfs. |
| `run`, `boot` | Build the release VMM and boot the configured guest artifacts. |
| `selftest` | Run the backend protected-mode self-test. |
| `boot-test` | Require the guest userspace marker. |
| `measure` | Run cold-start measurements. |
| `bench-*` | Run the named benchmark workflow. |
| `snapshot-demo` | Compare cold and restored warmed Python. |
| `snapshot-boot` | Capture or resume the interactive Python snapshot. |
| `clean` | Run `cargo clean`. |

`kernel`, `initramfs`, and `python-initramfs` are phony and rebuild when invoked. The artifact rules
used by `snapshot-demo` and `snapshot-boot` only build missing inputs, so repeat runs can reuse an
existing kernel and Python initramfs.

## Versions and output overrides

Current defaults are Linux `6.18.38`, Alpine `3.24.1`, and Alpine branch `v3.24`.

| Variable or option | Purpose |
| --- | --- |
| `KVER`, `--kver` | Linux kernel version. |
| `AVER`, `--aver` | Alpine release. |
| `ABRANCH`, `--abranch` | Alpine repository branch. |
| `WORK`, `--work` | Download and intermediate build directory. |
| `OUT`, `--output` | Output file for a native artifact build. |
| `DEST`, `--dest` | Docker export directory. |
| `BUILD_DIR` | Makefile artifact directory, defaulting to `$HOME/build`. |
| `KERNEL_IMG`, `INITRD_IMG`, `PY_INITRD` | Individual Makefile artifact paths. |
| `CARGO`, `PYTHON` | Executables used by the Makefile. |
| `NVX_ARTIFACT_DIR` | Linux workflow artifact directory, defaulting to `$HOME/build`. |

Run `python scripts/nvx.py <command> --help` for command-specific options. Continue with
[Running NVX](usage.md) for launch options and guest behavior.
