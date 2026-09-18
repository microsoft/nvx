# syntax=docker/dockerfile:1.7

ARG DEBIAN_IMAGE=debian:12-slim
ARG AZURELINUX_IMAGE=mcr.microsoft.com/azurelinux/base/core@sha256:c877612270d1ee2d6ab2bc1f64bfe38ab697ac50be325154ee5129fce89c17e4

FROM ${DEBIAN_IMAGE} AS base
RUN apt-get update && apt-get install -y --no-install-recommends \
        bc \
        binutils \
        bison \
        build-essential \
        ca-certificates \
        cpio \
        curl \
        flex \
        gzip \
        libelf-dev \
        libssl-dev \
        patch \
        python3 \
        rsync \
        tar \
        xz-utils \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /repo
COPY alpine/ /repo/alpine/
COPY docker/ /repo/docker/
COPY kernel/ /repo/kernel/
COPY scripts/ /repo/scripts/
COPY LICENSE README.md SOURCE-MANIFEST.json THIRD_PARTY_NOTICES.md /repo/

FROM base AS kernel
RUN python3 scripts/nvx.py build-kernel

FROM base AS initramfs
RUN python3 scripts/nvx.py build-initramfs

FROM base AS azurelinux-helpers
RUN set -eux; \
    mkdir /out; \
    for source in nvx-reseed nvx-mmio-write nvx-port-io nvx-console-pending nvx-managed-agent; do \
        cc -static -Os -s -Wall -Wextra -Werror -o "/out/$source" "/repo/alpine/$source.c"; \
    done; \
    cc -nostdlib -static -Os -fno-builtin -fno-pie -fno-stack-protector -no-pie \
        -Wl,--build-id=none -Wl,-z,noexecstack -s -o /out/nvx-device-io \
        /repo/alpine/nvx-device-io.c

FROM ${AZURELINUX_IMAGE} AS azurelinux-rootfs

FROM base AS azurelinux-initramfs
COPY --from=azurelinux-rootfs / /rootfs/
COPY --from=base /repo/alpine/ /tmp/nvx/
COPY --from=azurelinux-helpers /out/ /rootfs/sbin/
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
RUN set -eux; \
    chroot /rootfs tdnf install -y \
        e2fsprogs \
        iproute \
        net-tools \
        util-linux; \
    chroot /rootfs tdnf clean all; \
    install -d /rootfs/sbin; \
    install -m 0755 /tmp/nvx/init /rootfs/init; \
    sed -i 's/ || mdev -s/ || exit 1/' /rootfs/init; \
    install -m 0755 /tmp/nvx/nvx-exit /tmp/nvx/nvx-hostmount \
        /tmp/nvx/nvx-container-enter-azurelinux /rootfs/sbin/nvx-container-enter; \
    install -m 0755 /tmp/nvx/nvx-container-launch \
        /tmp/nvx/nvx-init-agent /tmp/nvx/nvx-identity-probe \
        /tmp/nvx/nvx-snapshot /tmp/nvx/nvx-virtio-restore-probe /rootfs/sbin/; \
    cd /rootfs; \
    find . -print0 | sort -z | cpio --null --quiet --reproducible \
        --owner=0:0 -o -H newc | gzip -n -9 > /tmp/initramfs.cpio.gz; \
    device_source_sha256="$(sha256sum /tmp/nvx/nvx-device-io.c | cut -d ' ' -f 1)"; \
    device_binary_sha256="$(sha256sum /rootfs/sbin/nvx-device-io | cut -d ' ' -f 1)"; \
    printf '%s\n' \
        '{' \
        '  "format": 1,' \
        '  "distribution": "azurelinux",' \
        '  "version": "3.0",' \
        '  "architecture": "x86_64",' \
        '  "packages": ["e2fsprogs", "iproute", "net-tools", "util-linux"],' \
        '  "helpers": {' \
        '    "nvx-device-io": {' \
        "      \"source_sha256\": \"$device_source_sha256\"," \
        "      \"binary_sha256\": \"$device_binary_sha256\"" \
        '    }' \
        '  }' \
        '}' > /tmp/initramfs.cpio.gz.packages.json

FROM scratch AS kernel-artifacts
COPY --from=kernel /repo/build/vmlinux /vmlinux
COPY --from=kernel /repo/build/vmlinux.config /vmlinux.config

FROM scratch AS initramfs-artifacts
COPY --from=initramfs /repo/build/initramfs.cpio.gz /initramfs.cpio.gz
COPY --from=initramfs /repo/build/initramfs.cpio.gz.packages.json /initramfs.cpio.gz.packages.json

FROM scratch AS azurelinux-artifacts
COPY --from=azurelinux-initramfs /tmp/initramfs.cpio.gz /initramfs.cpio.gz
COPY --from=azurelinux-initramfs /tmp/initramfs.cpio.gz.packages.json /initramfs.cpio.gz.packages.json

FROM scratch AS azurelinux-guest-artifacts
COPY --from=kernel /repo/build/vmlinux /vmlinux
COPY --from=kernel /repo/build/vmlinux.config /vmlinux.config
COPY --from=azurelinux-initramfs /tmp/initramfs.cpio.gz /initramfs.cpio.gz
COPY --from=azurelinux-initramfs /tmp/initramfs.cpio.gz.packages.json /initramfs.cpio.gz.packages.json

FROM kernel AS linux-source-archive
RUN mkdir -p /out \
    && python3 scripts/nvx.py create-linux-source-archive \
        --config /repo/build/linux/.config \
        --output /out/nvx-linux-source-6.18.38.tar.gz

FROM scratch AS artifacts
COPY --from=kernel /repo/build/vmlinux /vmlinux
COPY --from=kernel /repo/build/vmlinux.config /vmlinux.config
COPY --from=initramfs /repo/build/initramfs.cpio.gz /initramfs.cpio.gz
COPY --from=initramfs /repo/build/initramfs.cpio.gz.packages.json /initramfs.cpio.gz.packages.json

FROM scratch AS linux-source-artifacts
COPY --from=linux-source-archive /out/nvx-linux-source-6.18.38.tar.gz /nvx-linux-source-6.18.38.tar.gz
