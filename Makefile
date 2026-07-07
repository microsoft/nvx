# microvm — Makefile convenience targets.

CARGO ?= cargo

.PHONY: all release build test kernel initramfs run selftest boot-test clean

all: release

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

run: release
	scripts/run.sh

selftest: release
	./target/release/microvm --selftest

boot-test: release
	scripts/test-boot.sh

clean:
	$(CARGO) clean
