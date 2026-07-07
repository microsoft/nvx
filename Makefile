# microvm — Makefile convenience targets.

CARGO ?= cargo

# Locations of the artifacts the helper scripts produce. These mirror the defaults baked
# into scripts/build-kernel.sh and scripts/build-python-initramfs.sh so targets can depend
# on the artifacts themselves; override in tandem with the scripts' OUT if you relocate them.
BUILD_DIR ?= $(HOME)/build
KERNEL_IMG ?= $(BUILD_DIR)/vmlinux
PY_INITRD ?= $(BUILD_DIR)/initramfs-python.cpio.gz

.PHONY: all world release build test kernel initramfs python-initramfs run boot selftest boot-test measure snapshot-demo clean

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

python-initramfs:
	scripts/build-python-initramfs.sh

# On-demand builds of the artifacts that snapshot-demo consumes. Unlike the phony targets
# above (which always re-run their script), these only fire when the artifact is missing, so
# `make snapshot-demo` works from a clean tree without rebuilding when it is already present.
$(KERNEL_IMG):
	scripts/build-kernel.sh

$(PY_INITRD):
	scripts/build-python-initramfs.sh

run boot: release
	scripts/run.sh

selftest: release
	./target/release/microvm --selftest

boot-test: release
	scripts/test-boot.sh

measure: release
	scripts/measure-coldstart.sh

snapshot-demo: release $(KERNEL_IMG) $(PY_INITRD)
	scripts/snapshot-demo.sh

clean:
	$(CARGO) clean
