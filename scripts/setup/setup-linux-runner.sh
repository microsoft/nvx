#!/bin/sh

set -eu

RUST_TOOLCHAIN=stable
RUST_MINIMUM_VERSION=1.95.0
CARGO_NEXTEST_VERSION=0.9.133
RUNNER_VERSION=2.337.0
RUNNER_SHA256=70920811a4f8ad4328818682bca5c6469c1c942fab52448868071d0063816613

backend=
check_only=false
configure_runner=false
repository_url=https://github.com/microsoft/nvx
runner_directory=${HOME}/actions-runner
runner_name=
runner_token=

usage() {
    cat <<EOF
Usage: $0 --backend kvm|mshv [options]

Bootstrap a Linux Azure VM for NVX GitHub Actions jobs.

Options:
    --backend BACKEND       Virtualization backend: kvm or mshv
    --runner-name NAME      Register a runner with this unique name
    --repository-url URL    Runner repository URL (default: ${repository_url})
    --runner-directory PATH Runner installation path (default: ${runner_directory})
    --runner-token-stdin    Read a short-lived registration token from stdin
    --check-only            Validate the host and configured runner without changes
    -h, --help              Show this help
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
    sudo -n "$@"
}

run_runner_service() {
    (
        cd "$runner_directory"
        run_as_root ./svc.sh "$@"
    )
}

install_packages() {
    if command -v apt-get >/dev/null 2>&1; then
        run_as_root apt-get update
        run_as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y \
            bc binutils bison build-essential ca-certificates cmake cpio curl \
            docker-buildx docker.io flex git gzip iproute2 iptables \
            libarchive-tools libelf-dev libssl-dev make ninja-build patch perl \
            pkg-config protobuf-compiler python3 rsync tar util-linux xz-utils
    elif command -v tdnf >/dev/null 2>&1; then
        run_as_root tdnf install -y \
            bc binutils bison ca-certificates cmake cpio curl diffutils \
            docker-buildx elfutils-libelf-devel findutils flex gcc \
            gcc-c++ git glibc-devel glibc-iconv gzip icu iproute iptables \
            kernel-headers libarchive libarchive-devel lttng-ust make \
            moby-engine ninja-build openssl openssl-devel patch perl pkgconf \
            pkgconf-pkg-config protobuf python3 rsync shadow-utils tar \
            util-linux which xz
        run_as_root tdnf install -y docker-cli
    elif command -v dnf >/dev/null 2>&1; then
        run_as_root dnf install -y \
            bc binutils bison ca-certificates cmake cpio curl \
            elfutils-libelf-devel findutils flex gcc gcc-c++ git glibc-devel \
            gzip iproute iptables kernel-headers libarchive libarchive-devel \
            make moby-engine ninja-build openssl openssl-devel patch perl \
            pkgconf pkgconf-pkg-config protobuf-compiler python3 rsync \
            shadow-utils tar util-linux which xz docker-buildx
    else
        die "supported package manager not found (apt-get, dnf, or tdnf)"
    fi
}

install_rust_tools() {
    if ! command -v rustup >/dev/null 2>&1; then
        curl --fail --proto '=https' --tlsv1.2 --silent --show-error \
            https://sh.rustup.rs |
            sh -s -- -y --profile minimal --default-toolchain "$RUST_TOOLCHAIN"
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
    run_as_root systemctl enable --now docker
    getent group docker >/dev/null 2>&1 ||
        die "the Docker package did not create its access group"
    run_as_root usermod -aG docker "$(id -un)"
}

configure_backend_access() {
    device=/dev/${backend}
    module=${backend}
    [ "$backend" = mshv ] && module=mshv_root

    if [ ! -e "$device" ]; then
        run_as_root modprobe "$module" 2>/dev/null ||
            die "${device} is unavailable and ${module} could not be loaded"
    fi
    [ -c "$device" ] || die "${device} is not a character device"

    if ! getent group "$backend" >/dev/null 2>&1; then
        run_as_root groupadd --system "$backend"
    fi
    run_as_root sh -c \
        "printf '%s\\n' 'KERNEL==\"${backend}\", GROUP=\"${backend}\", MODE=\"0660\"' > /etc/udev/rules.d/70-${backend}.rules"
    run_as_root udevadm control --reload-rules
    run_as_root udevadm trigger --name-match="$backend"
    run_as_root chgrp "$backend" "$device"
    run_as_root chmod 0660 "$device"
    run_as_root usermod -aG "$backend" "$(id -un)"
}

install_runner() {
    archive="${RUNNER_TEMP:-/tmp}/actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz"
    download_url="https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/$(basename "$archive")"

    mkdir -p "$runner_directory"
    if [ ! -x "${runner_directory}/bin/Runner.Listener" ]; then
        curl --fail --location --proto '=https' --tlsv1.2 \
            --output "$archive" "$download_url"
        printf '%s  %s\n' "$RUNNER_SHA256" "$archive" | sha256sum --check -
        tar --extract --gzip --file "$archive" --directory "$runner_directory"
        rm -f "$archive"
    fi
    if ! grep -Eq '^ID=(azurelinux|mariner)$' /etc/os-release; then
        run_as_root "${runner_directory}/bin/installdependencies.sh"
    fi

    if [ ! -f "${runner_directory}/.runner" ]; then
        [ -n "$runner_token" ] || die "runner registration token is required"
        runner_labels="linux,${backend},virtual-machine,${runner_name}"
        (
            cd "$runner_directory"
            ./config.sh --unattended --replace \
                --url "$repository_url" \
                --token "$runner_token" \
                --name "$runner_name" \
                --labels "$runner_labels" \
                --work _work
        )
    fi
    runner_token=

    if ! run_runner_service status >/dev/null 2>&1; then
        run_runner_service install "$(id -un)"
    fi
    run_runner_service start
}

check_environment() {
    [ "$(uname -s)" = Linux ] || die "this script requires Linux"
    [ "$(uname -m)" = x86_64 ] || die "this script requires x86_64"
    for command_name in python3 git curl rustup cargo cargo-nextest gcc make ld \
        bison flex cpio gzip tar xz docker systemctl; do
        require_command "$command_name"
    done

    python_version=$(python3 -c \
        'import sys; print(".".join(map(str, sys.version_info[:3])))')
    version_at_least "$python_version" 3.10.0 ||
        die "Python 3.10.0 or newer is required"
    rust_version=$(RUSTUP_TOOLCHAIN=$RUST_TOOLCHAIN rustc --version | awk '{print $2}')
    version_at_least "$rust_version" "$RUST_MINIMUM_VERSION" ||
        die "Rust ${RUST_MINIMUM_VERSION} or newer is required"
    cargo nextest --version | grep -Fq "cargo-nextest ${CARGO_NEXTEST_VERSION}" ||
        die "cargo-nextest ${CARGO_NEXTEST_VERSION} is not installed"

    user_name=$(id -un)
    run_as_root -u "$user_name" docker version >/dev/null 2>&1 ||
        die "Docker is not available to ${user_name}"
    run_as_root -u "$user_name" docker buildx version >/dev/null 2>&1 ||
        die "Docker Buildx is not available to ${user_name}"
    run_as_root -u "$user_name" sh -c \
        "test -r /dev/${backend} && test -w /dev/${backend}" ||
        die "${user_name} cannot access /dev/${backend}"

    if [ -f "${runner_directory}/.runner" ]; then
        "${runner_directory}/bin/Runner.Listener" --version
        run_runner_service status >/dev/null
    elif [ "$configure_runner" = true ]; then
        die "GitHub Actions runner is not configured"
    fi
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --backend)
            [ "$#" -ge 2 ] || die "--backend requires a value"
            backend=$2
            shift 2
            ;;
        --runner-name)
            [ "$#" -ge 2 ] || die "--runner-name requires a value"
            runner_name=$2
            configure_runner=true
            shift 2
            ;;
        --repository-url)
            [ "$#" -ge 2 ] || die "--repository-url requires a value"
            repository_url=$2
            shift 2
            ;;
        --runner-directory)
            [ "$#" -ge 2 ] || die "--runner-directory requires a value"
            runner_directory=$2
            shift 2
            ;;
        --runner-token-stdin)
            configure_runner=true
            IFS= read -r runner_token || die "could not read runner token from stdin"
            runner_token=$(printf '%s' "$runner_token" | tr -d '\r\n')
            shift
            ;;
        --check-only)
            check_only=true
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

case "$backend" in
    kvm | mshv) ;;
    *) die "--backend must be kvm or mshv" ;;
esac
[ "$(id -u)" -ne 0 ] || die "run this script as the runner service account, not root"
require_command sudo
sudo -n true || die "passwordless sudo is required"
if [ "$configure_runner" = true ]; then
    [ -n "$runner_name" ] || die "--runner-name is required to configure a runner"
fi

if [ "$check_only" = false ]; then
    install_packages
    export PATH="${HOME}/.cargo/bin:${PATH}"
    install_rust_tools
    configure_docker_access
    configure_backend_access
    if [ "$configure_runner" = true ]; then
        install_runner
    fi
fi

export PATH="${HOME}/.cargo/bin:${PATH}"
check_environment
printf 'NVX_RUNNER_SETUP_COMPLETE=1\n'
