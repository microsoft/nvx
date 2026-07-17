# syntax=docker/dockerfile:1
#
# Builds the microvm Linux artifacts (the PVH `vmlinux` and the Alpine RAM `initramfs.cpio.gz`)
# inside a Linux container, so they can be produced from any host — notably Windows, where the
# WHP backend runs but a Linux/GCC toolchain to compile the kernel is not available.
#
# The `artifacts` stage contains only the two build outputs, so:
#
#   docker build -f docker/Dockerfile --target artifacts --output type=local,dest=build .
#
# writes `build/vmlinux` and `build/initramfs.cpio.gz` onto the host. The kernel and initramfs
# stages build in parallel. See `python scripts/nvx.py build-linux-artifacts` for the
# cross-platform convenience wrapper.

ARG DEBIAN_IMAGE=debian:12-slim

# ---- Base toolchain: everything the two build scripts need. ----
FROM ${DEBIAN_IMAGE} AS base
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        flex \
        bison \
        libelf-dev \
        libssl-dev \
        bc \
        cpio \
        patch \
        xz-utils \
        gzip \
        tar \
        curl \
        ca-certificates \
        python3 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /repo
# Only the inputs the scripts consume (kept in sync with .dockerignore).
COPY scripts/ /repo/scripts/
COPY kernel/ /repo/kernel/
COPY alpine/ /repo/alpine/

# ---- Kernel: download + patch + build the uncompressed PVH vmlinux. ----
FROM base AS kernel
ARG KVER=6.18.38
ENV HOME=/root \
    WORK=/build/kernel \
    OUT=/out/vmlinux \
    KVER=${KVER}
RUN mkdir -p /out && python3 /repo/scripts/nvx.py build-kernel

# ---- Initramfs: unpack the Alpine mini rootfs + install /init, pack a cpio.gz. ----
FROM base AS initramfs
ARG AVER=3.24.1
ARG ABRANCH=v3.24
ENV HOME=/root \
    WORK=/build/initramfs \
    OUT=/out/initramfs.cpio.gz \
    AVER=${AVER} \
    ABRANCH=${ABRANCH}
RUN mkdir -p /out && python3 /repo/scripts/nvx.py build-initramfs

# ---- Python initramfs: Alpine rootfs + CPython + numpy/pandas + the snapshot apps. ----
FROM base AS python-initramfs
ARG AVER=3.24.1
ARG ABRANCH=v3.24
ENV HOME=/root \
    WORK=/build/initramfs-python \
    OUT=/out/initramfs-python.cpio.gz \
    AVER=${AVER} \
    ABRANCH=${ABRANCH}
RUN mkdir -p /out && python3 /repo/scripts/nvx.py build-python-initramfs --native

# ---- Artifacts: a minimal stage carrying just the outputs, for `--output`. ----
FROM scratch AS artifacts
COPY --from=kernel /out/vmlinux /vmlinux
COPY --from=initramfs /out/initramfs.cpio.gz /initramfs.cpio.gz

# ---- Python artifacts: just the Python initramfs (built on demand; needs network). ----
FROM scratch AS python-artifacts
COPY --from=python-initramfs /out/initramfs-python.cpio.gz /initramfs-python.cpio.gz
