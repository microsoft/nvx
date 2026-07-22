# microvm — Makefile convenience targets.

CARGO ?= cargo
PYTHON ?= python3

# Locations of the artifacts the helper scripts produce. These mirror the defaults baked
# into the Python tooling so targets can depend on the artifacts themselves.
BUILD_DIR ?= $(HOME)/build
KERNEL_IMG ?= $(BUILD_DIR)/vmlinux
INITRD_IMG ?= $(BUILD_DIR)/initramfs.cpio.gz
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
	$(PYTHON) scripts/nvx.py build-kernel --output "$(KERNEL_IMG)"

initramfs:
	$(PYTHON) scripts/nvx.py build-initramfs --output "$(INITRD_IMG)"

# Build the Linux artifacts (vmlinux + initramfs.cpio.gz) in Docker, into ./build. Useful when
# the host lacks a Linux/GCC toolchain (e.g. to produce artifacts for the Windows/WHP backend).
artifacts:
	$(PYTHON) scripts/nvx.py build-linux-artifacts --dest build

python-initramfs:
	$(PYTHON) scripts/nvx.py build-python-initramfs --output "$(PY_INITRD)"

# On-demand builds of the artifacts that snapshot-demo consumes. Unlike the phony targets
# above (which always re-run their script), these only fire when the artifact is missing, so
# `make snapshot-demo` works from a clean tree without rebuilding when it is already present.
$(KERNEL_IMG):
	$(PYTHON) scripts/nvx.py build-kernel --output "$@"

$(PY_INITRD): scripts/nvx.py scripts/nvx_tools/build.py alpine/init.python alpine/nvx-snapshot alpine/hello.py alpine/repl.py alpine/net-hello.py alpine/net-pandas.py
	$(PYTHON) scripts/nvx.py build-python-initramfs --output "$@"

run boot: release
	$(PYTHON) scripts/nvx.py run --kernel "$(KERNEL_IMG)" --initrd "$(INITRD_IMG)"

selftest: release
	./target/release/microvm --selftest

boot-test: release
	$(PYTHON) scripts/nvx.py test-boot --kernel "$(KERNEL_IMG)" --initrd "$(INITRD_IMG)"

measure: release
	$(PYTHON) scripts/nvx.py measure-coldstart --kernel "$(KERNEL_IMG)" --initrd "$(INITRD_IMG)"

# Benchmark the virt-fs: guest read/write throughput and the persistent --mount-image round-trip.
bench-virtfs: release
	$(PYTHON) scripts/nvx.py bench-virtfs --kernel "$(KERNEL_IMG)" --initrd "$(INITRD_IMG)"

# Benchmark networking across snapshot/restore: cold boot to a working-network shell versus
# restoring one from a snapshot. Needs privileges for the host TAP (root or passwordless `sudo ip`).
bench-net-snapshot: release
	$(PYTHON) scripts/nvx.py bench-net-snapshot --kernel "$(KERNEL_IMG)" --initrd "$(INITRD_IMG)"

# Benchmark snapshot/restore of a networked Python guest: a bare interpreter and a warmed
# numpy/pandas app, each proving the link works before the timing marker. Needs the Python
# initramfs and privileges for the host TAP.
bench-net-snapshot-py: release
	$(PYTHON) scripts/nvx.py bench-net-snapshot-py --kernel "$(KERNEL_IMG)" --initrd "$(PY_INITRD)"

# Benchmark boot-to-interactive-shell for the plain Alpine initramfs: a cold PVH boot versus a
# restore from a snapshot captured at the shell-ready point, across 64/128/256/512 MiB of guest RAM.
bench-snapshot-shell: release
	$(PYTHON) scripts/nvx.py bench-snapshot-shell --kernel "$(KERNEL_IMG)" --initrd "$(INITRD_IMG)"

snapshot-demo: release $(KERNEL_IMG) $(PY_INITRD)
	$(PYTHON) scripts/nvx.py snapshot-demo --kernel "$(KERNEL_IMG)" --initrd "$(PY_INITRD)"

# Resume an interactive Python interpreter straight from a snapshot, skipping the kernel boot
# and Python startup. The snapshot is captured once on first use (a one-off cold boot) and
# reused afterwards, so repeat runs drop you at the ">>>" prompt in milliseconds.
snapshot-boot: release $(KERNEL_IMG) $(PY_INITRD)
	$(PYTHON) scripts/nvx.py snapshot-boot --kernel "$(KERNEL_IMG)" --initrd "$(PY_INITRD)"

clean:
	$(CARGO) clean
