# syntax=docker/dockerfile:1.7

ARG DEBIAN_IMAGE=debian:12-slim

FROM ${DEBIAN_IMAGE} AS base
RUN apt-get update && apt-get install -y --no-install-recommends \
        bc \
        binutils \
        bison \
        build-essential \
        ca-certificates \
        cpio \
        curl \
        erofs-utils \
        flex \
        gzip \
        libelf-dev \
        libssl-dev \
        patch \
        python3 \
        rsync \
        tar \
        xz-utils \
        zstd \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /repo
COPY guest/ /repo/guest/
COPY ubuntu/ /repo/ubuntu/
COPY docker/ /repo/docker/
COPY kernel/ /repo/kernel/
COPY scripts/ /repo/scripts/
COPY LICENSE README.md SOURCE-MANIFEST.json THIRD_PARTY_NOTICES.md /repo/

FROM base AS kernel
RUN python3 scripts/nvx.py build-kernel

FROM base AS initramfs
RUN python3 scripts/nvx.py build-initramfs --guest alpine

FROM base AS ubuntu-initramfs
RUN python3 scripts/nvx.py build-initramfs --guest ubuntu

FROM ubuntu-initramfs AS ubuntu-distro-layer
RUN python3 scripts/nvx.py build-distro-layer \
        --guest ubuntu \
        --output build/ubuntu-distro.erofs

FROM base AS ubuntu-determinism
RUN python3 scripts/nvx.py verify-guest-determinism \
        --guest ubuntu \
        --work-dir build/guest-determinism

FROM scratch AS kernel-artifacts
COPY --from=kernel /repo/build/vmlinux /vmlinux
COPY --from=kernel /repo/build/vmlinux.config /vmlinux.config
COPY --from=kernel /repo/build/vmlinux.provenance.json /vmlinux.provenance.json

FROM scratch AS initramfs-artifacts
COPY --from=initramfs /repo/build/initramfs.cpio.gz /initramfs.cpio.gz
COPY --from=initramfs /repo/build/initramfs.cpio.gz.packages.json /initramfs.cpio.gz.packages.json
COPY --from=initramfs /repo/build/initramfs.provenance.json /initramfs.provenance.json

FROM scratch AS ubuntu-initramfs-artifacts
COPY --from=ubuntu-initramfs /repo/build/initramfs-ubuntu.cpio.gz /initramfs-ubuntu.cpio.gz
COPY --from=ubuntu-initramfs /repo/build/initramfs-ubuntu.cpio.gz.packages.json /initramfs-ubuntu.cpio.gz.packages.json

FROM scratch AS ubuntu-distro-layer-artifacts
COPY --from=ubuntu-distro-layer /repo/build/ubuntu-distro.erofs /ubuntu-distro.erofs
COPY --from=ubuntu-distro-layer /repo/build/ubuntu-distro.erofs.manifest.json /ubuntu-distro.erofs.manifest.json

FROM kernel AS linux-source-archive
RUN mkdir -p /out \
    && python3 scripts/nvx.py create-linux-source-archive \
        --config /repo/build/linux/.config \
        --output /out/nvx-linux-source-6.18.38.tar.gz

FROM scratch AS artifacts
COPY --from=kernel /repo/build/vmlinux /vmlinux
COPY --from=kernel /repo/build/vmlinux.config /vmlinux.config
COPY --from=kernel /repo/build/vmlinux.provenance.json /vmlinux.provenance.json
COPY --from=initramfs /repo/build/initramfs.cpio.gz /initramfs.cpio.gz
COPY --from=initramfs /repo/build/initramfs.cpio.gz.packages.json /initramfs.cpio.gz.packages.json
COPY --from=initramfs /repo/build/initramfs.provenance.json /initramfs.provenance.json

FROM scratch AS ubuntu-guest-artifacts
COPY --from=kernel /repo/build/vmlinux /vmlinux
COPY --from=kernel /repo/build/vmlinux.config /vmlinux.config
COPY --from=kernel /repo/build/vmlinux.provenance.json /vmlinux.provenance.json
COPY --from=ubuntu-initramfs /repo/build/initramfs-ubuntu.cpio.gz /initramfs-ubuntu.cpio.gz
COPY --from=ubuntu-initramfs /repo/build/initramfs-ubuntu.cpio.gz.packages.json /initramfs-ubuntu.cpio.gz.packages.json

FROM scratch AS all-guest-artifacts
COPY --from=kernel /repo/build/vmlinux /vmlinux
COPY --from=kernel /repo/build/vmlinux.config /vmlinux.config
COPY --from=kernel /repo/build/vmlinux.provenance.json /vmlinux.provenance.json
COPY --from=initramfs /repo/build/initramfs.cpio.gz /initramfs.cpio.gz
COPY --from=initramfs /repo/build/initramfs.cpio.gz.packages.json /initramfs.cpio.gz.packages.json
COPY --from=initramfs /repo/build/initramfs.provenance.json /initramfs.provenance.json
COPY --from=ubuntu-initramfs /repo/build/initramfs-ubuntu.cpio.gz /initramfs-ubuntu.cpio.gz
COPY --from=ubuntu-initramfs /repo/build/initramfs-ubuntu.cpio.gz.packages.json /initramfs-ubuntu.cpio.gz.packages.json
COPY --from=ubuntu-distro-layer /repo/build/ubuntu-distro.erofs /ubuntu-distro.erofs
COPY --from=ubuntu-distro-layer /repo/build/ubuntu-distro.erofs.manifest.json /ubuntu-distro.erofs.manifest.json

FROM scratch AS linux-source-artifacts
COPY --from=linux-source-archive /out/nvx-linux-source-6.18.38.tar.gz /nvx-linux-source-6.18.38.tar.gz
