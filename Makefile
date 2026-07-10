# microvm — Makefile convenience targets.

CARGO ?= cargo

# Locations of the artifacts the helper scripts produce. These mirror the defaults baked
# into scripts/build-kernel.sh and scripts/build-python-initramfs.sh so targets can depend
# on the artifacts themselves; override in tandem with the scripts' OUT if you relocate them.
BUILD_DIR ?= $(HOME)/build
KERNEL_IMG ?= $(BUILD_DIR)/vmlinux
PY_INITRD ?= $(BUILD_DIR)/initramfs-python.cpio.gz

.PHONY: all world release build test kernel initramfs python-initramfs artifacts run boot selftest boot-test measure bench-virtfs bench-net-snapshot bench-net-snapshot-py bench-snapshot-shell snapshot-demo snapshot-boot clean

all: release

# Build all three components: the VMM, the modified kernel, and the Alpine initramfs.
world: release kernel initramfs

release:
	$(CARGO) build --release

build:
	$(CARGO) build

test:
	$(CARGO) test --release

kernel:
	scripts/build-kernel.sh

initramfs:
	scripts/build-initramfs.sh

# Build the Linux artifacts (vmlinux + initramfs.cpio.gz) in Docker, into ./build. Useful when
# the host lacks a Linux/GCC toolchain (e.g. to produce artifacts for the Windows/WHP backend).
artifacts:
	scripts/build-linux-artifacts.sh

python-initramfs:
	scripts/build-python-initramfs.sh

# On-demand builds of the artifacts that snapshot-demo consumes. Unlike the phony targets
# above (which always re-run their script), these only fire when the artifact is missing, so
# `make snapshot-demo` works from a clean tree without rebuilding when it is already present.
$(KERNEL_IMG):
	scripts/build-kernel.sh

$(PY_INITRD): scripts/build-python-initramfs.sh alpine/init.python alpine/hello.py alpine/repl.py alpine/net-hello.py alpine/net-pandas.py
	scripts/build-python-initramfs.sh

run boot: release
	scripts/run.sh

selftest: release
	./target/release/microvm --selftest

boot-test: release
	scripts/test-boot.sh

measure: release
	scripts/measure-coldstart.sh

# Benchmark the virt-fs: guest read/write throughput and the persistent --mount-image round-trip.
bench-virtfs: release
	scripts/bench-virtfs.sh

# Benchmark networking across snapshot/restore: cold boot to a working-network shell versus
# restoring one from a snapshot. Needs privileges for the host TAP (root or passwordless `sudo ip`).
bench-net-snapshot: release
	scripts/bench-net-snapshot.sh

# Benchmark snapshot/restore of a networked Python guest: a bare interpreter and a warmed
# numpy/pandas app, each proving the link works before the timing marker. Needs the Python
# initramfs and privileges for the host TAP.
bench-net-snapshot-py: release
	scripts/bench-net-snapshot-py.sh

# Benchmark boot-to-interactive-shell for the plain Alpine initramfs: a cold PVH boot versus a
# restore from a snapshot captured at the shell-ready point, across 64/128/256/512 MiB of guest RAM.
bench-snapshot-shell: release
	scripts/bench-snapshot-shell.sh

snapshot-demo: release $(KERNEL_IMG) $(PY_INITRD)
	scripts/snapshot-demo.sh

# Resume an interactive Python interpreter straight from a snapshot, skipping the kernel boot
# and Python startup. The snapshot is captured once on first use (a one-off cold boot) and
# reused afterwards, so repeat runs drop you at the ">>>" prompt in milliseconds.
snapshot-boot: release $(KERNEL_IMG) $(PY_INITRD)
	scripts/snapshot-boot.sh

clean:
	$(CARGO) clean
