#!/bin/sh

set -eu

RUST_TOOLCHAIN=stable
RUST_MINIMUM_VERSION=1.95.0
RUSTUP_VERSION=1.29.1
RUSTUP_SHA256=dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71
PYTHON_MINIMUM_VERSION=3.10.0
CARGO_NEXTEST_VERSION=0.9.133

script_directory=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
workspace=$(CDPATH='' cd -- "${script_directory}/../.." && pwd)
guest_bundle=
bundle_only=false
check_only=false
skip_build=false
reconnect_required=false

usage() {
    cat <<EOF
Usage: $0 [options]

Bootstrap an existing NVX checkout on a Linux/MSHV host.

Options:
    --workspace PATH     Existing NVX checkout (default: repository containing this script)
    --guest-bundle PATH  Write or validate guest artifacts for a Windows host
    --bundle-only        Write the bundle without provisioning or building
    --check-only         Validate the environment without changing it
    --skip-build         Configure and validate dependencies without building NVX
    -h, --help           Show this help
EOF
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

version_at_least() {
    [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n 1)" = "$2" ]
}

run_as_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo "$@"
    fi
}

install_packages() {
    if command -v apt-get >/dev/null 2>&1; then
        run_as_root apt-get update
        run_as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y \
            bc binutils bison build-essential ca-certificates cmake cpio curl \
            docker-buildx docker.io flex git gzip iproute2 iptables \
            libarchive-tools libelf-dev libssl-dev make ninja-build patch perl \
            pkg-config protobuf-compiler python3 rsync tar util-linux xz-utils \
            zstd
    elif command -v tdnf >/dev/null 2>&1; then
        run_as_root tdnf install -y \
            bc binutils bison ca-certificates cmake cpio curl diffutils \
            docker-buildx docker-cli elfutils-libelf-devel findutils flex gcc \
            gcc-c++ git glibc-devel glibc-iconv gzip iproute iptables \
            kernel-headers libarchive libarchive-devel make moby-engine \
            ninja-build openssl openssl-devel patch perl perl-FindBin \
            perl-IPC-Cmd perl-Time-Piece perl-lib pkgconf pkgconf-pkg-config \
            protobuf python3 rsync shadow-utils tar \
            util-linux which xz zstd
        run_as_root tdnf install -y docker-cli
    elif command -v dnf >/dev/null 2>&1; then
        run_as_root dnf install -y \
            bc binutils bison ca-certificates cmake cpio curl elfutils-libelf-devel \
            findutils flex gcc gcc-c++ git glibc-devel gzip iproute iptables \
            kernel-headers libarchive libarchive-devel make moby-engine \
            ninja-build openssl openssl-devel patch perl pkgconf \
            pkgconf-pkg-config protobuf-compiler python3 rsync shadow-utils tar \
            util-linux which xz zstd docker-buildx
    else
        die "supported package manager not found (apt-get, dnf, or tdnf)"
    fi
}

install_rust_tools() {
    if ! command -v rustup >/dev/null 2>&1; then
        require_command curl
        installer=$(mktemp "${TMPDIR:-/tmp}/rustup-init-${RUSTUP_VERSION}.XXXXXX")
        trap 'rm -f "$installer"' 0 HUP INT TERM
        installer_url=https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/x86_64-unknown-linux-gnu/rustup-init
        curl --fail --proto '=https' --tlsv1.2 --silent --show-error \
            --output "$installer" "$installer_url"
        printf '%s  %s\n' "$RUSTUP_SHA256" "$installer" | sha256sum --check -
        chmod 0755 "$installer"
        "$installer" -y --no-modify-path --profile minimal \
            --default-toolchain "$RUST_TOOLCHAIN"
        rm -f "$installer"
        trap - 0 HUP INT TERM
        # shellcheck disable=SC1091
        . "${HOME}/.cargo/env"
    fi

    rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal
    rust_version=$(RUSTUP_TOOLCHAIN=$RUST_TOOLCHAIN rustc --version | awk '{print $2}')
    version_at_least "$rust_version" "$RUST_MINIMUM_VERSION" ||
        die "Rust ${RUST_MINIMUM_VERSION} or newer is required"
    if ! command -v cargo-nextest >/dev/null 2>&1 ||
        ! cargo nextest --version | grep -Fq "cargo-nextest ${CARGO_NEXTEST_VERSION}"; then
        cargo +"$RUST_TOOLCHAIN" install --locked cargo-nextest \
            --version "$CARGO_NEXTEST_VERSION"
    fi
}

configure_docker_access() {
    require_command systemctl
    run_as_root systemctl enable --now docker
    getent group docker >/dev/null 2>&1 ||
        die "the Docker package did not create its access group"

    user_name=${SUDO_USER:-$(id -un)}
    if [ "$user_name" != root ] &&
        ! id -nG "$user_name" | tr ' ' '\n' | grep -Fxq docker; then
        run_as_root usermod -aG docker "$user_name"
        reconnect_required=true
    fi
}

configure_mshv_access() {
    if [ ! -e /dev/mshv ]; then
        run_as_root modprobe mshv_root 2>/dev/null ||
            die "/dev/mshv is unavailable and mshv_root could not be loaded"
    fi
    [ -c /dev/mshv ] || die "/dev/mshv is not a character device"

    if ! getent group mshv >/dev/null 2>&1; then
        run_as_root groupadd --system mshv
    fi
    run_as_root sh -c \
        'printf "%s\n" '\''KERNEL=="mshv", GROUP="mshv", MODE="0660"'\'' > /etc/udev/rules.d/70-mshv.rules'
    run_as_root udevadm control --reload-rules
    run_as_root udevadm trigger --name-match=mshv

    user_name=${SUDO_USER:-$(id -un)}
    if [ "$user_name" != root ] &&
        ! id -nG "$user_name" | tr ' ' '\n' | grep -Fxq mshv; then
        run_as_root usermod -aG mshv "$user_name"
        reconnect_required=true
    fi
    run_as_root chgrp mshv /dev/mshv
    run_as_root chmod 0660 /dev/mshv
}

check_environment() {
    [ "$(uname -s)" = Linux ] || die "this script requires Linux"
    [ "$(uname -m)" = x86_64 ] || die "this script requires x86_64"
    [ -f "${workspace}/scripts/nvx.py" ] ||
        die "NVX checkout not found at ${workspace}"
    [ -f "${workspace}/openvmm/Cargo.toml" ] ||
        die "OpenVMM submodule is not initialized"
    [ -c /dev/mshv ] || die "/dev/mshv is not a character device"
    [ -r /dev/mshv ] && [ -w /dev/mshv ] ||
        die "current session cannot access /dev/mshv"

    for command_name in python3 git curl rustup cargo cargo-nextest gcc make ld \
        bison flex cpio gzip sha256sum tar xz zstd; do
        require_command "$command_name"
    done
    python_version=$(python3 -c \
        'import sys; print(".".join(map(str, sys.version_info[:3])))')
    version_at_least "$python_version" "$PYTHON_MINIMUM_VERSION" ||
        die "Python ${PYTHON_MINIMUM_VERSION} or newer is required"
    rust_version=$(RUSTUP_TOOLCHAIN=$RUST_TOOLCHAIN rustc --version | awk '{print $2}')
    version_at_least "$rust_version" "$RUST_MINIMUM_VERSION" ||
        die "Rust ${RUST_MINIMUM_VERSION} or newer is required"
    cargo nextest --version | grep -Fq "cargo-nextest ${CARGO_NEXTEST_VERSION}" ||
        die "cargo-nextest ${CARGO_NEXTEST_VERSION} is not installed"
    require_command docker
    docker version >/dev/null 2>&1 ||
        die "Docker is not available to the current session"
    docker buildx version >/dev/null 2>&1 || die "Docker Buildx is not installed"

    (
        cd "$workspace"
        python3 scripts/nvx.py verify
    )
}

check_build() {
    (
        cd "$workspace"
        python3 scripts/nvx.py run --hypervisor mshv --dry-run
    )
}

build_nvx() {
    require_command docker
    docker version >/dev/null 2>&1 || die "Docker is not available to the current session"
    docker buildx version >/dev/null 2>&1 || die "Docker Buildx is not installed"
    (
        cd "$workspace"
        RUSTUP_TOOLCHAIN=$RUST_TOOLCHAIN python3 scripts/nvx.py build
    )
}

check_guest_bundle() {
    bundle_path=$1
    [ -d "$bundle_path" ] || die "guest bundle not found: $bundle_path"
    for name in vmlinux vmlinux.config initramfs.cpio.gz \
        initramfs.cpio.gz.packages.json REVISION SHA256SUMS; do
        [ -f "${bundle_path}/${name}" ] ||
            die "missing guest bundle file: ${bundle_path}/${name}"
    done
    (
        cd "$bundle_path"
        sha256sum --check SHA256SUMS
    ) || die "guest bundle checksum validation failed"
    bundle_revision=$(cat "${bundle_path}/REVISION")
    workspace_revision=$(git -C "$workspace" rev-parse HEAD)
    [ "$bundle_revision" = "$workspace_revision" ] ||
        die "guest bundle revision does not match the NVX checkout"
}

create_guest_bundle() {
    bundle_path=$1
    if [ -e "$bundle_path" ] &&
        find "$bundle_path" -mindepth 1 -print -quit | grep -q .; then
        die "guest bundle directory is not empty: $bundle_path"
    fi
    mkdir -p "$bundle_path"
    for name in vmlinux vmlinux.config initramfs.cpio.gz \
        initramfs.cpio.gz.packages.json; do
        source_path="${workspace}/build/${name}"
        [ -f "$source_path" ] || die "missing guest artifact: $source_path"
        cp "$source_path" "$bundle_path"
    done
    git -C "$workspace" rev-parse HEAD >"${bundle_path}/REVISION"
    (
        cd "$bundle_path"
        sha256sum vmlinux vmlinux.config initramfs.cpio.gz \
            initramfs.cpio.gz.packages.json REVISION >SHA256SUMS
    )
    check_guest_bundle "$bundle_path"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --workspace)
            [ "$#" -ge 2 ] || die "--workspace requires a path"
            workspace=$2
            shift 2
            ;;
        --guest-bundle)
            [ "$#" -ge 2 ] || die "--guest-bundle requires a path"
            guest_bundle=$2
            shift 2
            ;;
        --bundle-only)
            bundle_only=true
            shift
            ;;
        --check-only)
            check_only=true
            shift
            ;;
        --skip-build)
            skip_build=true
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            die "unknown option: $1"
            ;;
    esac
done

workspace=$(CDPATH='' cd -- "$workspace" 2>/dev/null && pwd) ||
    die "workspace directory not found: $workspace"

if [ "$bundle_only" = true ]; then
    [ -n "$guest_bundle" ] || die "--bundle-only requires --guest-bundle"
    [ "$check_only" = false ] ||
        die "--bundle-only and --check-only cannot be combined"
    create_guest_bundle "$guest_bundle"
    printf 'NVX_GUEST_BUNDLE=%s\n' "$guest_bundle"
    exit 0
fi

if [ "$check_only" = true ]; then
    check_environment
    if [ "$skip_build" = false ]; then
        check_build
    fi
    if [ -n "$guest_bundle" ]; then
        check_guest_bundle "$guest_bundle"
    fi
    printf 'NVX_SETUP_CHECK=ok\n'
    exit 0
fi

if [ "$(id -u)" -ne 0 ]; then
    require_command sudo
fi
install_packages
install_rust_tools
configure_docker_access
configure_mshv_access
if [ "$reconnect_required" = true ]; then
    printf '%s\n' \
        'NVX_SETUP_RECONNECT_REQUIRED=1' \
        'Reconnect so Docker/MSHV group membership applies, then rerun this script.'
    exit 20
fi
if [ "$skip_build" = false ]; then
    build_nvx
    check_build
fi
if [ -n "$guest_bundle" ]; then
    create_guest_bundle "$guest_bundle"
fi
check_environment
printf 'NVX_SETUP_COMPLETE=1\n'
