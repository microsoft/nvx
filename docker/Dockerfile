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
        ca-certificates \
        cpio \
        curl \
        erofs-utils=${EROFS_UTILS_VERSION} \
        flex \
        gzip \
        libelf-dev \
        libssl-dev \
        musl-tools \
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
COPY azurelinux/ /repo/azurelinux/
COPY docker/ /repo/docker/
COPY kernel/ /repo/kernel/
COPY scripts/ /repo/scripts/
COPY LICENSE README.md SOURCE-MANIFEST.json THIRD_PARTY_NOTICES.md /repo/

FROM base AS kernel
RUN python3 scripts/nvx.py build-kernel

FROM base AS kernel-debug
RUN python3 scripts/nvx.py build-kernel --debug

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
    for helper in nvx-time nvx-exit; do \
        musl-gcc -static -Os -s -Wall -Wextra -Werror -o "/out/helpers/$helper" "/repo/guest/common/$helper.c"; \
    done; \
    musl-gcc -std=gnu11 -static -O2 -pthread -s -Wall -Wextra -Werror -o /out/helpers/nvx-time-probe \
        /repo/guest/common/nvx-time-probe.c; \
    cc -nostdlib -static -Os -fno-builtin -fno-pie -fno-stack-protector -no-pie \
        -Wl,--build-id=none -Wl,-z,noexecstack -s -o /out/helpers/nvx-device-io \
        /repo/guest/common/nvx-device-io.c

FROM base AS azurelinux-packages
RUN python3 - <<'PY'
import sys
from pathlib import Path

sys.path.insert(0, '/repo/scripts')
from nvx_tools.azurelinux import download_packages

download_packages(Path('/out/rpms'))
PY

FROM ${AZURELINUX_IMAGE} AS azurelinux-rootfs
COPY --from=azurelinux-packages /out/rpms/ /tmp/azurelinux-rpms/
RUN rpm --import /etc/pki/rpm-gpg/MICROSOFT-RPM-GPG-KEY \
    && rpm --checksig /tmp/azurelinux-rpms/*.rpm \
    && rpm --upgrade --verbose --hash /tmp/azurelinux-rpms/*.rpm \
    && rm -rf /tmp/azurelinux-rpms \
    && test -x /usr/bin/ldd \
    && test -x /usr/bin/setpriv \
    && test -x /usr/sbin/busybox \
    && rpm -qa --queryformat '%{NAME}\t%{VERSION}\t%{RELEASE}\t%{ARCH}\t%{LICENSE}\t%{SOURCERPM}\n' \
        | sort > /tmp/azurelinux-rpm-packages.tsv

FROM base AS azurelinux-initramfs
ARG AZURELINUX_IMAGE
ARG AZURELINUX_VERSION
COPY --from=azurelinux-rootfs / /rootfs/
COPY --from=azurelinux-rootfs /tmp/azurelinux-rpm-packages.tsv /tmp/azurelinux-rpm-packages.tsv
COPY --from=azurelinux-helpers /out/helpers/ /rootfs/sbin/
RUN <<'EOF'
set -eux
install -m 0755 /repo/guest/common/init /rootfs/init
install -m 0755 /repo/guest/common/nvx-hostmount \
    /repo/guest/common/nvx-identity-probe \
    /repo/guest/common/nvx-snapshot \
    /repo/guest/common/nvx-virtio-restore-probe \
    /repo/guest/common/nvx-container-enter-azurelinux /rootfs/sbin/
install -m 0755 /repo/guest/common/nvx-container-launch-azurelinux \
    /rootfs/sbin/nvx-container-launch
# Link the BusyBox applets that the shared guest scripts expect; every other
# command resolves through PATH to the Azure Linux packages.
ln -sf ../sbin/busybox /rootfs/usr/bin/busybox
rm -f /rootfs/bin/sh;
for utility in arp awk basename cat chmod chroot cp grep head hostname ifconfig ip mdev mkdir nc \
    sh mknod mount ping route rm sed sleep tr wget blkid losetup mountpoint flock; do
    ln -sf busybox "/rootfs/bin/$utility"
done
# Workload identities require an existing home directory. Azure Linux points
# nobody at /dev/null, so give it the empty home that Ubuntu Base uses.
sed -i 's#^\(nobody:[^:]*:65534:65534:[^:]*:\)/dev/null:#\1/nonexistent:#' \
    /rootfs/etc/passwd
grep -q '^nobody:[^:]*:65534:65534:[^:]*:/nonexistent:' /rootfs/etc/passwd
install -d -m 0755 /rootfs/nonexistent
rm -f /rootfs/tmp/azurelinux-rpm-packages.tsv
# The package manifest records the inventory; these databases and caches
# record install-time state that differs between otherwise identical builds.
rm -rf /rootfs/usr/lib/sysimage/tdnf /rootfs/var/cache/tdnf \
    /rootfs/var/cache/ldconfig /rootfs/var/lib/rpm
python3 <<'PY'
import json
from pathlib import Path

packages = []
for line in Path('/tmp/azurelinux-rpm-packages.tsv').read_text(encoding='utf-8').splitlines():
    fields = line.split('\t', 5)
    if len(fields) != 6:
        raise ValueError(f'malformed RPM package metadata: {line!r}')
    name, version, release, architecture, license_text, source_rpm = fields
    if name == 'gpg-pubkey':
        continue
    packages.append({
        'type': 'rpm',
        'name': name,
        'version': version,
        'release': release,
        'architecture': architecture,
        'license': license_text,
        'source_rpm': source_rpm,
    })
Path('/tmp/azurelinux-packages.json').write_text(
    json.dumps(packages, indent=2, sort_keys=True) + '\n',
    encoding='utf-8',
)
PY
cd /rootfs
find . -exec touch --no-dereference --date=@0 {} +
find . -print0 | LC_ALL=C sort -z | cpio --null --quiet --reproducible \
    --owner=0:0 -o -H newc | gzip -n -9 > /tmp/initramfs-azurelinux.cpio.gz
python3 <<'PY'
import hashlib
import json
import os
import sys
from pathlib import Path

sys.path.insert(0, '/repo/scripts')
from nvx_tools.azurelinux import input_sha256

def sha256(path: str) -> str:
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()

manifest = {
    'format': 1,
    'guest': 'azurelinux',
    'artifact': 'initramfs-azurelinux.cpio.gz',
    'artifact_sha256': sha256('/tmp/initramfs-azurelinux.cpio.gz'),
    'distribution': 'Azure Linux',
    'version': os.environ['AZURELINUX_VERSION'],
    'architecture': 'x86_64',
    'image': os.environ['AZURELINUX_IMAGE'],
    'input_sha256': input_sha256(
        image=os.environ['AZURELINUX_IMAGE'],
        version=os.environ['AZURELINUX_VERSION'],
    ),
    'package_manifest_format': 'azurelinux-v1',
    'packages': json.loads(Path('/tmp/azurelinux-packages.json').read_text(encoding='utf-8')),
    'helpers': {
        'nvx-device-io': {
            'source_sha256': sha256('/repo/guest/common/nvx-device-io.c'),
            'binary_sha256': sha256('/rootfs/sbin/nvx-device-io'),
        },
    },
}
Path('/tmp/initramfs-azurelinux.cpio.gz.packages.json').write_text(
    json.dumps(manifest, indent=2, sort_keys=True) + '\n',
    encoding='utf-8',
)
PY
EOF

FROM scratch AS kernel-artifacts
COPY --from=kernel /repo/build/vmlinux /vmlinux
COPY --from=kernel /repo/build/vmlinux.config /vmlinux.config
COPY --from=kernel /repo/build/vmlinux.provenance.json /vmlinux.provenance.json

FROM scratch AS kernel-debug-artifacts
COPY --from=kernel-debug /repo/build/vmlinux-debug /vmlinux-debug
COPY --from=kernel-debug /repo/build/vmlinux-debug.config /vmlinux-debug.config
COPY --from=kernel-debug /repo/build/vmlinux-debug.provenance.json /vmlinux-debug.provenance.json

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
