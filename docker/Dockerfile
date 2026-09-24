# syntax=docker/dockerfile:1.7

ARG DEBIAN_IMAGE=debian:12-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251
ARG EROFS_UTILS_VERSION=1.5-1
ARG AZURELINUX_VERSION=3.0
ARG AZURELINUX_IMAGE=mcr.microsoft.com/azurelinux/base/core@sha256:c877612270d1ee2d6ab2bc1f64bfe38ab697ac50be325154ee5129fce89c17e4

FROM ${DEBIAN_IMAGE} AS base
ARG EROFS_UTILS_VERSION
RUN apt-get update && apt-get install -y --no-install-recommends \
        bc \
        binutils \
        bison \
        build-essential \
        busybox-static \
        ca-certificates \
        cpio \
        curl \
        erofs-utils=${EROFS_UTILS_VERSION} \
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

FROM base AS azurelinux-helpers
RUN set -eux; \
    mkdir -p /out/helpers; \
    for helper in nvx-reseed nvx-mmio-write nvx-port-io nvx-console-pending nvx-managed-agent; do \
        cc -static -Os -s -Wall -Wextra -Werror -o "/out/helpers/$helper" "/repo/guest/common/$helper.c"; \
    done; \
    cc -nostdlib -static -Os -fno-builtin -fno-pie -fno-stack-protector -no-pie \
        -Wl,--build-id=none -Wl,-z,noexecstack -s -o /out/helpers/nvx-device-io \
        /repo/guest/common/nvx-device-io.c; \
    ! readelf -l /bin/busybox | grep -q INTERP; \
    cp /bin/busybox /out/busybox

FROM ${AZURELINUX_IMAGE} AS azurelinux-rootfs
RUN tdnf install -y util-linux \
    && test -x /usr/bin/ldd \
    && test -x /usr/bin/setpriv \
    && tdnf clean all

FROM base AS azurelinux-initramfs
ARG AZURELINUX_IMAGE
ARG AZURELINUX_VERSION
COPY --from=azurelinux-rootfs / /rootfs/
COPY --from=azurelinux-helpers /out/helpers/ /rootfs/sbin/
COPY --from=azurelinux-helpers /out/busybox /rootfs/bin/busybox
RUN set -eux; \
    install -m 0755 /repo/guest/common/init /rootfs/init; \
    install -m 0755 /repo/guest/common/nvx-exit \
        /repo/guest/common/nvx-hostmount \
        /repo/guest/common/nvx-identity-probe \
        /repo/guest/common/nvx-snapshot \
        /repo/guest/common/nvx-virtio-restore-probe \
        /repo/guest/common/nvx-container-enter-azurelinux /rootfs/sbin/; \
    install -m 0755 /repo/guest/common/nvx-container-launch-azurelinux \
        /rootfs/sbin/nvx-container-launch; \
    for utility in awk basename cat chmod chroot cp grep head hostname ifconfig ip mdev mkdir \
        mknod mount ping route rm sed sleep tr blkid losetup mkfs.ext4 mountpoint flock unshare; do \
        ln -sf busybox "/rootfs/bin/$utility"; \
    done; \
    cd /rootfs; \
    find . -print0 | sort -z | cpio --null --quiet --reproducible \
        --owner=0:0 -o -H newc | gzip -n -9 > /tmp/initramfs-azurelinux.cpio.gz; \
    artifact_sha256="$(sha256sum /tmp/initramfs-azurelinux.cpio.gz | cut -d ' ' -f 1)"; \
    source_sha256="$(sha256sum /repo/guest/common/nvx-device-io.c | cut -d ' ' -f 1)"; \
    binary_sha256="$(sha256sum /rootfs/sbin/nvx-device-io | cut -d ' ' -f 1)"; \
    printf '%s\n' \
        '{' \
        '  "format": 1,' \
        '  "guest": "azurelinux",' \
        '  "artifact": "initramfs-azurelinux.cpio.gz",' \
        "  \"artifact_sha256\": \"$artifact_sha256\"," \
        '  "distribution": "Azure Linux",' \
        "  \"version\": \"$AZURELINUX_VERSION\"," \
        '  "architecture": "x86_64",' \
        "  \"image\": \"$AZURELINUX_IMAGE\"," \
        '  "package_manifest_format": "azurelinux-v1",' \
        '  "packages": [],' \
        '  "helpers": {' \
        '    "nvx-device-io": {' \
        "      \"source_sha256\": \"$source_sha256\"," \
        "      \"binary_sha256\": \"$binary_sha256\"" \
        '    }' \
        '  }' \
        '}' > /tmp/initramfs-azurelinux.cpio.gz.packages.json

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

FROM scratch AS azurelinux-initramfs-artifacts
COPY --from=azurelinux-initramfs /tmp/initramfs-azurelinux.cpio.gz /initramfs-azurelinux.cpio.gz
COPY --from=azurelinux-initramfs /tmp/initramfs-azurelinux.cpio.gz.packages.json /initramfs-azurelinux.cpio.gz.packages.json

FROM scratch AS ubuntu-distro-layer-artifacts
COPY --from=ubuntu-distro-layer /repo/build/ubuntu-distro.erofs /ubuntu-distro.erofs
COPY --from=ubuntu-distro-layer /repo/build/ubuntu-distro.erofs.manifest.json /ubuntu-distro.erofs.manifest.json

FROM scratch AS azurelinux-guest-artifacts
COPY --from=kernel /repo/build/vmlinux /vmlinux
COPY --from=kernel /repo/build/vmlinux.config /vmlinux.config
COPY --from=kernel /repo/build/vmlinux.provenance.json /vmlinux.provenance.json
COPY --from=azurelinux-initramfs /tmp/initramfs-azurelinux.cpio.gz /initramfs-azurelinux.cpio.gz
COPY --from=azurelinux-initramfs /tmp/initramfs-azurelinux.cpio.gz.packages.json /initramfs-azurelinux.cpio.gz.packages.json

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
COPY --from=azurelinux-initramfs /tmp/initramfs-azurelinux.cpio.gz /initramfs-azurelinux.cpio.gz
COPY --from=azurelinux-initramfs /tmp/initramfs-azurelinux.cpio.gz.packages.json /initramfs-azurelinux.cpio.gz.packages.json

FROM scratch AS linux-source-artifacts
COPY --from=linux-source-archive /out/nvx-linux-source-6.18.38.tar.gz /nvx-linux-source-6.18.38.tar.gz
