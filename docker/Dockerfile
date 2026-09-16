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

FROM base AS agent-initramfs
ARG NVX_AGENT_SHA256
RUN --mount=type=secret,id=nvx-agent,required=true \
    test -n "${NVX_AGENT_SHA256}" \
    && python3 scripts/nvx.py stage-agent \
        --input /run/secrets/nvx-agent \
        --sha256 "${NVX_AGENT_SHA256}" \
    && python3 scripts/nvx.py build-agent-initramfs --native

FROM scratch AS kernel-artifacts
COPY --from=kernel /repo/build/vmlinux /vmlinux
COPY --from=kernel /repo/build/vmlinux.config /vmlinux.config
COPY --from=kernel /repo/build/vmlinux.provenance.json /vmlinux.provenance.json

FROM scratch AS initramfs-artifacts
COPY --from=initramfs /repo/build/initramfs.cpio.gz /initramfs.cpio.gz
COPY --from=initramfs /repo/build/initramfs.cpio.gz.packages.json /initramfs.cpio.gz.packages.json

FROM scratch AS agent-initramfs-artifacts
COPY --from=agent-initramfs /repo/build/initramfs-agent.cpio.gz /initramfs-agent.cpio.gz
COPY --from=agent-initramfs /repo/build/initramfs-agent.cpio.gz.packages.json /initramfs-agent.cpio.gz.packages.json

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

FROM scratch AS linux-source-artifacts
COPY --from=linux-source-archive /out/nvx-linux-source-6.18.38.tar.gz /nvx-linux-source-6.18.38.tar.gz
