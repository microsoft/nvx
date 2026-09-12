#!/bin/sh

set -eu

RUST_TOOLCHAIN=stable
RUST_MINIMUM_VERSION=1.95.0
RUSTUP_VERSION=1.29.1
RUSTUP_SHA256=dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71
CARGO_NEXTEST_VERSION=0.9.133
RUNNER_VERSION=2.337.0
RUNNER_SHA256=70920811a4f8ad4328818682bca5c6469c1c942fab52448868071d0063816613

backend=
check_only=false
configure_runner=false
repository_url=https://github.com/microsoft/nvx
legacy_runner_directory=${HOME}/actions-runner
runner_directory=/opt/nvx-runner
runner_directory_is_default=true
runner_name=
runner_service_account=nvx-runner
runner_token=
runner_package_directory=
removed_service_name=
trusted_tool_root=/opt/nvx
trusted_cargo_home=${trusted_tool_root}/cargo
trusted_rustup_home=${trusted_tool_root}/rustup
runner_cargo_home=
runner_service_path=${trusted_cargo_home}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

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

validate_runner_name() {
    case "$1" in
        '' | .* | *..* | *[!A-Za-z0-9_.-]*)
            die "invalid runner name: $1"
            ;;
    esac
}

run_as_root() {
    sudo -n "$@"
}

run_as_runner() {
    run_as_root -u "$runner_service_account" env \
        PATH="$runner_service_path" \
        CARGO_HOME="$runner_cargo_home" \
        RUSTUP_HOME="$trusted_rustup_home" \
        "$@"
}

run_runner_service() {
    (
        cd "$runner_directory"
        run_as_root ./svc.sh "$@"
    )
}

runner_service_name() {
    target_directory=${1:-$runner_directory}
    service_file=${target_directory}/.service
    run_as_root test -f "$service_file" ||
        die "GitHub Actions runner service is not installed"
    service_name=$(run_as_root cat "$service_file")
    [ -n "$service_name" ] || die "GitHub Actions runner service name is empty"
    case "$service_name" in
        actions.runner.*.service)
            service_stem=${service_name#actions.runner.}
            service_stem=${service_stem%.service}
            case "$service_stem" in
                '' | .* | *..* | *[!A-Za-z0-9_.-]*)
                    die "invalid GitHub Actions runner service name: ${service_name}"
                    ;;
            esac
            ;;
        *)
            die "invalid GitHub Actions runner service name: ${service_name}"
            ;;
    esac
    load_state=$(run_as_root systemctl show "$service_name" \
        --property=LoadState --value 2>/dev/null || true)
    if [ "$load_state" = loaded ]; then
        service_command=$(run_as_root systemctl show "$service_name" \
            --property=ExecStart --value)
        printf '%s\n' "$service_command" |
            grep -Fq "path=${target_directory}/runsvc.sh ;" ||
            die "GitHub Actions service does not belong to ${target_directory}"
    elif [ -n "$load_state" ] && [ "$load_state" != not-found ]; then
        die "GitHub Actions runner service has unexpected state: ${load_state}"
    fi
    printf '%s\n' "$service_name"
}

remove_runner_service() {
    target_directory=${1:-$runner_directory}
    service_name=$(runner_service_name "$target_directory")
    load_state=$(run_as_root systemctl show "$service_name" \
        --property=LoadState --value 2>/dev/null || true)
    if [ "$load_state" != loaded ]; then
        run_as_root rm -f "${target_directory}/.service"
        return 0
    fi
    removed_service_name=$service_name
    run_as_root systemctl stop "$service_name" || true
    run_as_root systemctl disable "$service_name" || true
    run_as_root rm -f "/etc/systemd/system/${service_name}"
    run_as_root rm -rf "/etc/systemd/system/${service_name}.d"
    run_as_root rm -f "${target_directory}/.service"
    run_as_root systemctl daemon-reload
}

prepare_runner_package() {
    runner_package_directory=$(run_as_root mktemp -d \
        "${trusted_tool_root}/runner-package.XXXXXX")
    runner_package_archive=${runner_package_directory}/runner.tar.gz
    runner_package_root=${runner_package_directory}/root
    runner_package_url=https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz
    trap 'if [ -n "$runner_package_directory" ]; then run_as_root rm -rf "$runner_package_directory"; fi' \
        0 HUP INT TERM
    run_as_root curl --fail --location --proto '=https' --tlsv1.2 \
        --silent --show-error \
        --output "$runner_package_archive" "$runner_package_url"
    printf '%s  %s\n' "$RUNNER_SHA256" "$runner_package_archive" |
        run_as_root sha256sum --check -
    run_as_root mkdir -p "$runner_package_root"
    run_as_root tar --extract --gzip \
        --file "$runner_package_archive" \
        --directory "$runner_package_root"
}

install_or_verify_runner_package() {
    if ! run_as_root test -x "${runner_directory}/bin/Runner.Listener"; then
        if [ -n "$(run_as_root find "$runner_directory" \
            -mindepth 1 -maxdepth 1 -print -quit)" ]; then
            die "refusing to install into partial runner directory: ${runner_directory}"
        fi
        run_as_root cp -a "${runner_package_root}/." "$runner_directory"
    fi
    for entry in $(run_as_root find "$runner_package_root" \
        -mindepth 1 -maxdepth 1 -printf '%f\n'); do
        run_as_root diff --brief --recursive --no-dereference \
            "${runner_package_root}/${entry}" \
            "${runner_directory}/${entry}" >/dev/null ||
            die "installed runner package does not match ${RUNNER_VERSION}: ${entry}"
    done
    for entry in $(run_as_root find "$runner_directory" \
        -mindepth 1 -maxdepth 1 -printf '%f\n'); do
        case "$entry" in
            .credentials | .credentials_rsaparams | .env | .nvx-labels | \
                .path | .runner | .service | _diag | _work | runsvc.sh | svc.sh) ;;
            *)
                run_as_root test -e "${runner_package_root}/${entry}" ||
                    die "installed runner package has unexpected entry: ${entry}"
                ;;
        esac
    done
    run_as_root rm -rf "$runner_package_directory"
    runner_package_directory=
    trap - 0 HUP INT TERM
}

refresh_runner_service_script() {
    [ -n "$removed_service_name" ] || return 0
    run_as_root python3 - "$runner_directory" "$removed_service_name" "$runner_name" <<'PY'
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
service_name = sys.argv[2]
runner_name = sys.argv[3]
template = (root / "bin" / "systemd.svc.sh.template").read_text(encoding="utf-8")
script = template.replace("{{SvcNameVar}}", service_name).replace(
    "{{SvcDescription}}", f"GitHub Actions Runner ({runner_name})"
)
(root / "svc.sh").write_text(script, encoding="utf-8")
PY
    run_as_root chown "root:${runner_service_account}" "${runner_directory}/svc.sh"
    run_as_root chmod 0750 "${runner_directory}/svc.sh"
}

configure_runner_account() {
    if ! getent passwd "$runner_service_account" >/dev/null 2>&1; then
        run_as_root useradd \
            --system \
            --user-group \
            --home-dir /nonexistent \
            --no-create-home \
            --shell /usr/sbin/nologin \
            "$runner_service_account"
    fi
    [ "$(id -u "$runner_service_account")" -ne 0 ] ||
        die "runner service account must not be root"
    primary_group=$(id -gn "$runner_service_account")
    for group_name in $(id -nG "$runner_service_account"); do
        case "$group_name" in
            "$primary_group" | "$backend") ;;
            *) run_as_root gpasswd -d "$runner_service_account" "$group_name" ;;
        esac
    done
    if run_as_runner sudo -n true >/dev/null 2>&1; then
        die "runner service account has passwordless sudo access"
    fi
}

set_runner_disable_update() {
    run_as_root python3 -c '
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
configuration = json.loads(path.read_text(encoding="utf-8-sig"))
configuration["disableUpdate"] = True
path.write_text(json.dumps(configuration, indent=2), encoding="utf-8-sig")
' "${runner_directory}/.runner"
}

protect_runner_installation() {
    work_directory=${runner_directory}/_work
    diagnostics_directory=${runner_directory}/_diag
    diagnostics_target=${work_directory}/_diag
    if run_as_root test -L "$work_directory"; then
        die "runner work directory must not be a symlink: ${work_directory}"
    fi
    if run_as_root test -e "$work_directory" &&
        ! run_as_root test -d "$work_directory"; then
        die "runner work path is not a directory: ${work_directory}"
    fi
    for component in _temp _diag; do
        component_path=${work_directory}/${component}
        if run_as_root test -L "$component_path"; then
            die "runner work component must not be a symlink: ${component_path}"
        fi
        if run_as_root test -e "$component_path" &&
            ! run_as_root test -d "$component_path"; then
            die "runner work component is not a directory: ${component_path}"
        fi
    done
    if run_as_root test -L "$runner_cargo_home"; then
        die "runner Cargo home must not be a symlink: ${runner_cargo_home}"
    fi
    if run_as_root test -e "$runner_cargo_home" &&
        ! run_as_root test -d "$runner_cargo_home"; then
        die "runner Cargo home is not a directory: ${runner_cargo_home}"
    fi
    run_as_root mkdir -p \
        "$work_directory" \
        "$diagnostics_target" \
        "${runner_cargo_home}/bin"

    if [ -L "$diagnostics_directory" ] &&
        [ "$(readlink "$diagnostics_directory")" != _work/_diag ]; then
        run_as_root rm "$diagnostics_directory"
    fi
    if [ -d "$diagnostics_directory" ] && [ ! -L "$diagnostics_directory" ]; then
        run_as_root cp -a "${diagnostics_directory}/." "$diagnostics_target"
        run_as_root rm -rf "$diagnostics_directory"
    fi
    if [ ! -L "$diagnostics_directory" ]; then
        run_as_root ln -s _work/_diag "$diagnostics_directory"
    fi

    run_as_root chown "root:${runner_service_account}" "$runner_directory"
    run_as_root chmod u=rwx,g=rx,o=x "$runner_directory"
    run_as_root find "$runner_directory" \
        -mindepth 1 -maxdepth 1 \
        ! -name _work ! -name _diag \
        -exec chown -R "root:${runner_service_account}" {} +
    run_as_root find "$runner_directory" \
        -mindepth 1 -maxdepth 1 \
        ! -name _work ! -name _diag \
        -exec chmod -R u=rwX,g=rX,o= {} +
    run_as_root chown -h "root:${runner_service_account}" "$diagnostics_directory"
    if [ "$(run_as_root stat -c %U "$work_directory")" != \
        "$runner_service_account" ]; then
        run_as_root chown -R \
            "${runner_service_account}:${runner_service_account}" \
            "$work_directory"
    else
        run_as_root chown "${runner_service_account}:${runner_service_account}" \
            "$work_directory"
    fi
    run_as_root chmod u=rwx,go= "$work_directory"
    run_as_root chown -R "${runner_service_account}:${runner_service_account}" \
        "$diagnostics_target"
    run_as_root chmod -R u=rwX,go= "$diagnostics_target"
    run_as_root chown -R "${runner_service_account}:${runner_service_account}" \
        "$runner_cargo_home"
    run_as_root chmod -R u=rwX,go= "$runner_cargo_home"
}

migrate_legacy_runner() {
    [ "$runner_directory_is_default" = true ] || return 0
    [ -f "${legacy_runner_directory}/.runner" ] || return 0
    [ -n "$runner_token" ] ||
        die "runner registration token is required to migrate and refresh labels"

    if [ -f "${runner_directory}/.runner" ]; then
        die "both legacy and protected runner installations are configured"
    fi
    if [ -e "$runner_directory" ] || [ -L "$runner_directory" ]; then
        run_as_root rm -rf "$runner_directory"
    fi

    if [ -f "${legacy_runner_directory}/.service" ]; then
        remove_runner_service "$legacy_runner_directory"
    fi
    run_as_root mkdir -p "$(dirname "$runner_directory")"
    run_as_root mv "$legacy_runner_directory" "$runner_directory"
}

configure_runner_service() {
    service_name=$(runner_service_name)
    drop_in=/etc/systemd/system/${service_name}.d
    run_as_root mkdir -p "$drop_in"
    printf '[Service]\nEnvironment="PATH=%s"\nEnvironment="CARGO_HOME=%s"\nEnvironment="RUSTUP_HOME=%s"\nLimitCORE=infinity\n' \
        "$runner_service_path" "$runner_cargo_home" "$trusted_rustup_home" |
        run_as_root tee "${drop_in}/nvx.conf" >/dev/null
    run_as_root systemctl daemon-reload
    run_as_root systemctl stop "$service_name"
    protect_runner_installation
    set_runner_disable_update
    printf '%s\n' "$runner_service_path" |
        run_as_root tee "${runner_directory}/.path" >/dev/null
    run_as_root systemctl start "$service_name"
}

install_packages() {
    if command -v apt-get >/dev/null 2>&1; then
        run_as_root apt-get update
        run_as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y \
            bc binutils bison build-essential ca-certificates cmake cpio curl diffutils \
            flex git gzip iproute2 iptables \
            libarchive-tools libelf-dev libssl-dev make ninja-build patch perl \
            pkg-config protobuf-compiler python3 rsync tar util-linux xz-utils \
            zstd
    elif command -v tdnf >/dev/null 2>&1; then
        run_as_root tdnf install -y \
            bc binutils bison ca-certificates cmake cpio curl diffutils \
            elfutils-libelf-devel findutils flex gcc \
            gcc-c++ git glibc-devel glibc-iconv gzip icu iproute iptables \
            kernel-headers libarchive libarchive-devel lttng-ust make \
            ninja-build openssl openssl-devel patch perl pkgconf \
            pkgconf-pkg-config protobuf python3 rsync shadow-utils tar \
            util-linux which xz zstd
    elif command -v dnf >/dev/null 2>&1; then
        run_as_root dnf install -y \
            bc binutils bison ca-certificates cmake cpio curl diffutils \
            elfutils-libelf-devel findutils flex gcc gcc-c++ git glibc-devel \
            gzip iproute iptables kernel-headers libarchive libarchive-devel \
            make ninja-build openssl openssl-devel patch perl \
            pkgconf pkgconf-pkg-config protobuf-compiler python3 rsync \
            shadow-utils tar util-linux which xz zstd
    else
        die "supported package manager not found (apt-get, dnf, or tdnf)"
    fi
}

install_rust_tools() {
    rustup=${trusted_cargo_home}/bin/rustup
    cargo=${trusted_cargo_home}/bin/cargo
    rustc=${trusted_cargo_home}/bin/rustc
    nextest=${trusted_cargo_home}/bin/cargo-nextest
    run_as_root mkdir -p "$trusted_cargo_home" "$trusted_rustup_home"

    if [ ! -x "$rustup" ]; then
        installer=${trusted_tool_root}/rustup-init-${RUSTUP_VERSION}
        installer_url=https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/x86_64-unknown-linux-gnu/rustup-init
        run_as_root curl --fail --proto '=https' --tlsv1.2 \
            --silent --show-error --output "$installer" "$installer_url"
        printf '%s  %s\n' "$RUSTUP_SHA256" "$installer" |
            run_as_root sha256sum --check -
        run_as_root chmod 0755 "$installer"
        run_as_root env \
            CARGO_HOME="$trusted_cargo_home" \
            RUSTUP_HOME="$trusted_rustup_home" \
            "$installer" -y --no-modify-path --profile minimal \
            --default-toolchain "$RUST_TOOLCHAIN"
        run_as_root rm -f "$installer"
    fi

    run_as_root env \
        CARGO_HOME="$trusted_cargo_home" \
        RUSTUP_HOME="$trusted_rustup_home" \
        "$rustup" toolchain install "$RUST_TOOLCHAIN" --profile minimal
    run_as_root env \
        CARGO_HOME="$trusted_cargo_home" \
        RUSTUP_HOME="$trusted_rustup_home" \
        RUSTUP_TOOLCHAIN="$RUST_TOOLCHAIN" \
        "$rustup" target add x86_64-unknown-none
    if [ "$backend" = mshv ]; then
        run_as_root env \
            CARGO_HOME="$trusted_cargo_home" \
            RUSTUP_HOME="$trusted_rustup_home" \
            RUSTUP_TOOLCHAIN="$RUST_TOOLCHAIN" \
            "$rustup" target add x86_64-unknown-linux-musl
    fi

    rust_version=$(run_as_root env \
        CARGO_HOME="$trusted_cargo_home" \
        RUSTUP_HOME="$trusted_rustup_home" \
        RUSTUP_TOOLCHAIN="$RUST_TOOLCHAIN" \
        "$rustc" --version | awk '{print $2}')
    version_at_least "$rust_version" "$RUST_MINIMUM_VERSION" ||
        die "Rust ${RUST_MINIMUM_VERSION} or newer is required"
    if [ ! -x "$nextest" ] ||
        ! "$nextest" --version | grep -Fq "cargo-nextest ${CARGO_NEXTEST_VERSION}"; then
        run_as_root env \
            CARGO_HOME="$trusted_cargo_home" \
            RUSTUP_HOME="$trusted_rustup_home" \
            "$cargo" +"$RUST_TOOLCHAIN" install --locked --force cargo-nextest \
            --version "$CARGO_NEXTEST_VERSION"
    fi
    run_as_root chown -R root:root "$trusted_tool_root"
    run_as_root chmod -R go-w "$trusted_tool_root"
}

restrict_docker_access() {
    if getent group docker >/dev/null 2>&1 &&
        id -nG "$runner_service_account" | tr ' ' '\n' | grep -Fxq docker; then
        run_as_root gpasswd -d "$runner_service_account" docker
    fi
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
    run_as_root usermod -G "$backend" "$runner_service_account"
}

install_runner() {
    migrate_legacy_runner
    run_as_root mkdir -p "$runner_directory"
    prepare_runner_package
    install_or_verify_runner_package
    if ! grep -Eq '^ID=(azurelinux|mariner)$' /etc/os-release; then
        run_as_root "${runner_directory}/bin/installdependencies.sh"
    fi

    registration_required=false
    if ! run_as_root test -f "${runner_directory}/.runner" ||
        ! run_as_root test -f "$runner_labels_file" ||
        [ "$(run_as_root cat "$runner_labels_file" 2>/dev/null || true)" != \
            "$expected_runner_labels" ]; then
        registration_required=true
    fi
    if [ "$registration_required" = true ]; then
        [ -n "$runner_token" ] || die "runner registration token is required"
        if run_as_root test -f "${runner_directory}/.service"; then
            remove_runner_service
        fi
        run_as_root rm -f \
            "${runner_directory}/.runner" \
            "${runner_directory}/.credentials" \
            "${runner_directory}/.credentials_rsaparams" \
            "$runner_labels_file"
        (
            cd "$runner_directory"
            run_as_root env RUNNER_ALLOW_RUNASROOT=1 \
                ./config.sh --unattended --replace \
                --url "$repository_url" \
                --token "$runner_token" \
                --name "$runner_name" \
                --labels "$expected_runner_labels" \
                --disableupdate \
                --work _work
        )
        printf '%s\n' "$expected_runner_labels" |
            run_as_root tee "$runner_labels_file" >/dev/null
    fi
    runner_token=

    if run_as_root test -f "${runner_directory}/.service"; then
        service_name=$(runner_service_name)
        service_user=$(run_as_root systemctl show "$service_name" \
            --property=User --value)
        if [ "$service_user" != "$runner_service_account" ]; then
            remove_runner_service
        fi
    fi
    if ! run_as_root test -f "${runner_directory}/.service"; then
        refresh_runner_service_script
        run_runner_service install "$runner_service_account"
    fi
    configure_runner_service
}

check_environment() {
    [ "$(uname -s)" = Linux ] || die "this script requires Linux"
    [ "$(uname -m)" = x86_64 ] || die "this script requires x86_64"
    for command_name in python3 git curl diff rustup cargo cargo-nextest gcc make ld \
        bison flex cpio gzip sha256sum tar xz zstd systemctl; do
        require_command "$command_name"
    done

    python_version=$(python3 -c \
        'import sys; print(".".join(map(str, sys.version_info[:3])))')
    version_at_least "$python_version" 3.10.0 ||
        die "Python 3.10.0 or newer is required"
    rust_version=$(run_as_runner env RUSTUP_TOOLCHAIN=$RUST_TOOLCHAIN \
        rustc --version | awk '{print $2}')
    version_at_least "$rust_version" "$RUST_MINIMUM_VERSION" ||
        die "Rust ${RUST_MINIMUM_VERSION} or newer is required"
    run_as_runner cargo nextest --version |
        grep -Fq "cargo-nextest ${CARGO_NEXTEST_VERSION}" ||
        die "cargo-nextest ${CARGO_NEXTEST_VERSION} is not installed"
    [ "$(stat -c %U "$trusted_tool_root")" = root ] ||
        die "trusted Rust toolchain is not root-owned"
    run_as_runner test ! -w "${trusted_cargo_home}/bin/cargo" ||
        die "runner service account can modify trusted Cargo"
    run_as_runner test ! -w "$trusted_rustup_home" ||
        die "runner service account can modify trusted Rustup state"

    run_as_runner sh -c \
        "test -r /dev/${backend} && test -w /dev/${backend}" ||
        die "${runner_service_account} cannot access /dev/${backend}"
    if run_as_runner sudo -n true >/dev/null 2>&1; then
        die "runner service account has passwordless sudo access"
    fi
    runner_primary_group=$(id -gn "$runner_service_account")
    for group_name in $(id -nG "$runner_service_account"); do
        case "$group_name" in
            "$runner_primary_group" | "$backend") ;;
            *) die "runner service account has unexpected group: ${group_name}" ;;
        esac
    done

    if run_as_root test -f "${runner_directory}/.runner"; then
        if [ "$configure_runner" = true ]; then
            configured_runner_name=$(run_as_root python3 -c \
                'import json, sys; print(json.load(open(sys.argv[1], encoding="utf-8-sig"))["agentName"])' \
                "${runner_directory}/.runner")
            [ "$configured_runner_name" = "$runner_name" ] ||
                die "configured runner is ${configured_runner_name}, expected ${runner_name}"
        fi
        runner_disable_update=$(run_as_root python3 -c \
            'import json, sys; print(json.load(open(sys.argv[1], encoding="utf-8-sig")).get("disableUpdate"))' \
            "${runner_directory}/.runner")
        [ "$runner_disable_update" = True ] ||
            die "GitHub Actions runner automatic updates are not disabled"
        if [ "$configure_runner" = true ]; then
            run_as_root test -f "$runner_labels_file" &&
                [ "$(run_as_root cat "$runner_labels_file")" = \
                    "$expected_runner_labels" ] ||
                die "GitHub Actions runner labels are not validated"
        fi
        [ "$(stat -c %U "$runner_directory")" = root ] ||
            die "GitHub Actions runner installation is not root-owned"
        run_as_runner test ! -w "${runner_directory}/bin/Runner.Listener" ||
            die "runner service account can modify Runner.Listener"
        run_as_runner test ! -w "$(dirname "$runner_directory")" ||
            die "runner service account can replace the runner installation"
        run_as_runner test -w "${runner_directory}/_work" ||
            die "runner service account cannot modify its work directory"
        run_as_root test -L "${runner_directory}/_diag" &&
            [ "$(run_as_root readlink "${runner_directory}/_diag")" = _work/_diag ] ||
            die "GitHub Actions runner diagnostics are not stored under _work"
        [ "$(run_as_root cat "${runner_directory}/.path")" = "$runner_service_path" ] ||
            die "GitHub Actions runner persisted PATH is not configured"
        run_as_runner "${runner_directory}/bin/Runner.Listener" --version
        run_runner_service status >/dev/null
        service_name=$(runner_service_name)
        service_user=$(run_as_root systemctl show "$service_name" \
            --property=User --value)
        [ "$service_user" = "$runner_service_account" ] ||
            die "GitHub Actions runner service uses ${service_user}, expected ${runner_service_account}"
        service_pid=$(run_as_root systemctl show "$service_name" \
            --property=MainPID --value)
        case "$service_pid" in
            '' | 0 | *[!0-9]*) die "GitHub Actions runner service has no main process" ;;
        esac
        service_environment=$(run_as_root cat "/proc/${service_pid}/environ" |
            tr '\0' '\n')
        for expected_environment in \
            "PATH=${runner_service_path}" \
            "CARGO_HOME=${runner_cargo_home}" \
            "RUSTUP_HOME=${trusted_rustup_home}"; do
            printf '%s\n' "$service_environment" |
                grep -Fxq "$expected_environment" ||
                die "GitHub Actions runner service environment is not configured: ${expected_environment}"
        done
        listener_pid=$(run_as_root pgrep -f \
            "^${runner_directory}/bin/Runner.Listener run --startuptype service$")
        [ -n "$listener_pid" ] ||
            die "GitHub Actions runner listener process was not found"
        listener_uid=$(run_as_root sed -n \
            's/^Uid:[[:space:]]*\([0-9]*\).*/\1/p' \
            "/proc/${listener_pid}/status")
        [ "$listener_uid" = "$(id -u "$runner_service_account")" ] ||
            die "GitHub Actions runner listener uses unexpected uid ${listener_uid}"
        run_as_root grep -Eq '^Max core file size[[:space:]]+unlimited[[:space:]]+unlimited' \
            "/proc/${listener_pid}/limits" ||
            die "GitHub Actions runner listener core limit is not unlimited"
        listener_environment=$(run_as_root cat "/proc/${listener_pid}/environ" |
            tr '\0' '\n')
        for expected_environment in \
            "PATH=${runner_service_path}" \
            "CARGO_HOME=${runner_cargo_home}" \
            "RUSTUP_HOME=${trusted_rustup_home}"; do
            printf '%s\n' "$listener_environment" |
                grep -Fxq "$expected_environment" ||
                die "GitHub Actions runner listener environment is not configured: ${expected_environment}"
        done
        if getent group docker >/dev/null 2>&1; then
            docker_gid=$(getent group docker | cut -d: -f3)
            listener_groups=$(run_as_root sed -n \
                's/^Groups:[[:space:]]*//p' "/proc/${listener_pid}/status")
            case " $listener_groups " in
                *" ${docker_gid} "*)
                    die "GitHub Actions runner listener has root-equivalent Docker access"
                    ;;
            esac
        fi
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
            runner_directory_is_default=false
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
[ "$(id -u)" -ne 0 ] || die "run this script as the SSH administrator, not root"
require_command sudo
sudo -n true || die "passwordless sudo is required"
if [ "$configure_runner" = true ]; then
    [ -n "$runner_name" ] || die "--runner-name is required to configure a runner"
    validate_runner_name "$runner_name"
fi
runner_cargo_home=${runner_directory}/_work/_temp/cargo-home
runner_labels_file=${runner_directory}/.nvx-labels
expected_runner_labels=linux,${backend},virtual-machine,${runner_name}

if [ "$check_only" = false ]; then
    install_packages
    configure_runner_account
    install_rust_tools
    restrict_docker_access
    configure_backend_access
    if [ "$configure_runner" = true ]; then
        install_runner
    fi
elif ! getent passwd "$runner_service_account" >/dev/null 2>&1; then
    die "runner service account is not configured: ${runner_service_account}"
fi

export PATH="$runner_service_path"
export CARGO_HOME="$runner_cargo_home"
export RUSTUP_HOME="$trusted_rustup_home"
check_environment
printf 'NVX_RUNNER_SETUP_COMPLETE=1\n'
