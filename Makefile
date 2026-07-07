# microvm — Makefile convenience targets.

CARGO ?= cargo

.PHONY: all world release build test kernel initramfs run boot selftest boot-test measure clean

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

run boot: release
	scripts/run.sh

selftest: release
	./target/release/microvm --selftest

boot-test: release
	scripts/test-boot.sh

measure: release
	scripts/measure-coldstart.sh

clean:
	$(CARGO) clean
