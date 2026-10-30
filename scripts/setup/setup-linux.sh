#!/bin/sh

set -eu

PYTHON_MINIMUM_VERSION=3.10.0

script_directory=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# The versions, artifacts, and checksums of the tools that this script
# installs; stage the manifest next to the script.
tool_manifest=${script_directory}/tool-versions.conf
tool_platform=linux-x86_64

backend=
check_only=false
runner_only=false
# The last option given that only development-host or only runner mode takes.
development_option=
runner_option=

workspace=${script_directory}/../..
guest_bundle=
bundle_only=false
skip_build=false
reconnect_required=false

benchmark_runner=false
configure_runner=false
repository_url=https://github.com/microsoft/nvx
runner_directory=/opt/nvx-runner
runner_directory_is_default=true
runner_name=
extra_runner_labels=
runner_service_account=nvx-runner
runner_token=
runner_token_stdin=false
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
       $0 --runner-only --backend kvm|mshv [runner options]

Bootstrap a Linux host for NVX.

Development-host mode, the default, prepares an existing NVX checkout for the
current user: it installs the build dependencies, the pinned Rust tools, and,
unless the host has Docker, Docker with Buildx, grants the user Docker and
backend device access, and builds and validates NVX. Runner mode prepares a
host for NVX GitHub Actions jobs and needs no checkout: it installs the build
dependencies and a read-only Rust toolchain, and, with --runner-name,
registers a runner whose jobs run as the dedicated ${runner_service_account}
account, without sudo or Docker access.

Options:
    --backend BACKEND       Virtualization backend: kvm or mshv
    --check-only            Validate the host without changing it
    -h, --help              Show this help

Development-host options:
    --workspace PATH        Existing NVX checkout (default: repository containing this script)
    --guest-bundle PATH     Write or validate guest artifacts for a Windows host
    --bundle-only           Write the bundle without provisioning or building
    --skip-build            Configure and validate dependencies without building NVX

Runner options:
    --runner-only           Prepare an Actions runner host instead of a development host
    --runner-name NAME      Register a runner with this unique name
    --benchmark-runner      Use the backend's dedicated benchmark role instead
                            of the general role; requires --runner-name
    --extra-runner-labels LABELS  Add labels to the required platform labels
    --repository-url URL    Runner repository URL (default: ${repository_url})
    --runner-directory PATH Runner installation path (default: ${runner_directory})
    --runner-token-stdin    Read a short-lived registration token from stdin
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

tool_manifest_error() {
    die "invalid tool manifest ${tool_manifest}:${tool_manifest_line}: $*"
}

# Reads the tool manifest as data, without sourcing or evaluating it, so a
# fresh host needs nothing beyond the shell. Every line must be well formed.
load_tool_manifest() {
    [ -f "$tool_manifest" ] || die "tool manifest not found: ${tool_manifest}"
    carriage_return=$(printf '\r')
    blank=$(printf ' \t')
    tool_manifest_entries=
    tool_manifest_line=0
    while IFS= read -r line || [ -n "$line" ]; do
        tool_manifest_line=$((tool_manifest_line + 1))
        line=${line%"$carriage_return"}
        case "$line" in
            '' | '#'*) continue ;;
            *["$blank"]*) tool_manifest_error "expected KEY=VALUE without spaces" ;;
            *=*) ;;
            *) tool_manifest_error "expected KEY=VALUE without spaces" ;;
        esac
        key=${line%%=*}
        value=${line#*=}
        tool=${key%%.*}
        case "$tool" in
            '' | [!a-z]* | *[!a-z0-9_]*) tool_manifest_error "unsupported key: ${key}" ;;
        esac
        case "${key#"$tool"}" in
            .version | .toolchain) field=version ;;
            .artifacts.*.name | .artifacts.*.sha256)
                field=${key##*.}
                platform=${key#"$tool".artifacts.}
                platform=${platform%.*}
                case "$platform" in
                    '' | *[!a-z0-9_-]*) tool_manifest_error "unsupported key: ${key}" ;;
                    linux-x86_64 | windows-x86_64) ;;
                    *) tool_manifest_error "unsupported platform: ${platform}" ;;
                esac
                ;;
            *) tool_manifest_error "unsupported key: ${key}" ;;
        esac
        case "${tool_manifest_entries} " in
            *" ${key}="*) tool_manifest_error "duplicate key: ${key}" ;;
        esac
        case "$field" in
            version)
                case "$value" in
                    *[!0-9.]* | .* | *. | *..* | *.*.*.*) value= ;;
                    *.*.*) ;;
                    *) value= ;;
                esac
                ;;
            sha256)
                case "$value" in
                    *[!0-9a-f]*) value= ;;
                esac
                [ "${#value}" -eq 64 ] || value=
                ;;
            name)
                name=$value
                while :; do
                    case "$name" in
                        *'{version}'*) name=${name%%'{version}'*}v${name#*'{version}'} ;;
                        *) break ;;
                    esac
                done
                case "$name" in
                    '' | /* | */ | *//* | .* | */.* | *[!A-Za-z0-9._/-]*) value= ;;
                esac
                ;;
        esac
        [ -n "$value" ] ||
            tool_manifest_error "invalid value for ${key}: ${line#*=}"
        tool_manifest_entries="${tool_manifest_entries} ${key}=${value}"
    done <"$tool_manifest"
}

# Sets tool_value to the value of a key in the loaded tool manifest.
tool_manifest_value() {
    case "${tool_manifest_entries} " in
        *" ${1}="*) ;;
        *) die "tool manifest ${tool_manifest} does not define ${1}" ;;
    esac
    tool_value=${tool_manifest_entries#*" ${1}="}
    tool_value=${tool_value%%" "*}
}

# Sets tool_version, tool_artifact, and tool_sha256 to a tool's release and
# the name and checksum of its artifact for tool_platform.
tool_manifest_artifact() {
    tool_manifest_value "${1}.version"
    tool_version=$tool_value
    tool_manifest_value "${1}.artifacts.${tool_platform}.name"
    tool_artifact=
    while :; do
        case "$tool_value" in
            *'{version}'*)
                tool_artifact=${tool_artifact}${tool_value%%'{version}'*}${tool_version}
                tool_value=${tool_value#*'{version}'}
                ;;
            *) break ;;
        esac
    done
    tool_artifact=${tool_artifact}${tool_value}
    tool_manifest_value "${1}.artifacts.${tool_platform}.sha256"
    tool_sha256=$tool_value
}

# Pins every tool that this script installs before it inspects or changes
# the host. Only runner mode installs sccache and the Actions runner, so only
# it needs their pins.
read_tool_versions() {
    load_tool_manifest
    tool_manifest_value rust.toolchain
    RUST_TOOLCHAIN=$tool_value
    tool_manifest_value cargo_nextest.version
    CARGO_NEXTEST_VERSION=$tool_value
    tool_manifest_artifact rustup
    RUSTUP_VERSION=$tool_version
    RUSTUP_ARTIFACT=$tool_artifact
    RUSTUP_SHA256=$tool_sha256
    [ "$runner_only" = true ] || return 0
    tool_manifest_artifact sccache
    SCCACHE_VERSION=$tool_version
    SCCACHE_ARTIFACT=$tool_artifact
    SCCACHE_SHA256=$tool_sha256
    tool_manifest_artifact actions_runner
    RUNNER_VERSION=$tool_version
    RUNNER_ARTIFACT=$tool_artifact
    RUNNER_SHA256=$tool_sha256
}

report_invariant_tsc() {
    # The invariant-TSC flag is evidence only: the doctor's guest warp probe
    # measures the cross-vCPU skew that a host without one can cause (#265).
    grep -Eq '^flags[[:space:]]*:.*[[:space:]]nonstop_tsc([[:space:]]|$)' "$1" ||
        printf '%s\n' "warning: host does not expose an invariant TSC (nonstop_tsc); qualify it with nvx.py doctor, whose guest warp probe measures the cross-vCPU skew that this can cause" >&2
}

validate_runner_name() {
    case "$1" in
        '' | .* | *..* | *[!A-Za-z0-9_.-]*)
            die "invalid runner name: $1"
            ;;
    esac
}

append_runner_labels() {
    case "$1" in
        '' | ,* | *, | *,,* | *[!A-Za-z0-9_,.-]*)
            die "invalid additional runner labels: $1"
            ;;
    esac
    remaining=$1
    seen_labels=$expected_runner_labels
    while :; do
        label=${remaining%%,*}
        lowercase_label=$(printf '%s' "$label" | tr '[:upper:]' '[:lower:]')
        case "$lowercase_label" in
            general | benchmark)
                die "runner role label must be selected with --benchmark-runner: $label"
                ;;
        esac
        case ",${seen_labels}," in
            *,"${lowercase_label}",*) die "duplicate runner label: $label" ;;
        esac
        expected_runner_labels=${expected_runner_labels},${label}
        seen_labels=${seen_labels},${lowercase_label}
        [ "$remaining" != "$label" ] || break
        remaining=${remaining#*,}
    done
}

# Sets expected_runner_labels to the platform labels, exactly one runner role,
# and the additional labels.
set_expected_runner_labels() {
    expected_runner_labels=linux,${backend},virtual-machine
    # CI's platform jobs use a dedicated benchmark runner. Every other runner uses
    # the complementary general role so unrelated jobs cannot occupy that runner.
    if [ "$benchmark_runner" = true ]; then
        expected_runner_labels=${expected_runner_labels},benchmark
    else
        expected_runner_labels=${expected_runner_labels},general
    fi
    if [ -n "$extra_runner_labels" ]; then
        append_runner_labels "$extra_runner_labels"
    fi
}

# Rejects options that the selected mode doesn't take, and invalid values,
# before anything reads the manifest or inspects the host. In runner mode, it
# also sets the runner's labels.
validate_options() {
    case "$backend" in
        kvm | mshv) ;;
        *) die "--backend must be kvm or mshv" ;;
    esac
    if [ "$runner_only" = false ]; then
        [ -z "$runner_option" ] || die "${runner_option} requires --runner-only"
        if [ "$bundle_only" = true ]; then
            [ -n "$guest_bundle" ] || die "--bundle-only requires --guest-bundle"
            [ "$check_only" = false ] ||
                die "--bundle-only and --check-only cannot be combined"
        fi
        return 0
    fi
    [ -z "$development_option" ] ||
        die "${development_option} cannot be combined with --runner-only"
    if [ "$configure_runner" = true ]; then
        [ -n "$runner_name" ] || die "--runner-name is required to configure a runner"
        validate_runner_name "$runner_name"
    fi
    [ "$benchmark_runner" = false ] || [ -n "$runner_name" ] ||
        die "--benchmark-runner requires --runner-name"
    set_expected_runner_labels
}

# Runner mode runs unattended as the SSH administrator, so sudo must never
# prompt there. Development-host mode may run as root or prompt for sudo.
run_as_root() {
    if [ "$runner_only" = true ]; then
        sudo -n "$@"
    elif [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo "$@"
    fi
}

run_as_runner() {
    run_as_root -u "$runner_service_account" env \
        PATH="$runner_service_path" \
        CARGO_INCREMENTAL=0 \
        CARGO_HOME="$runner_cargo_home" \
        RUSTC_WRAPPER=sccache \
        RUSTUP_HOME="$trusted_rustup_home" \
        SCCACHE_CACHE_SIZE=10G \
        SCCACHE_DIR="$runner_sccache_dir" \
        "$@"
}

run_runner_service() {
    (
        cd "$runner_directory"
        run_as_root ./svc.sh "$@"
    )
}

# Runs a command as the owner of the tools that this script installs: root,
# for a runner's trusted toolchain, or the current user on a development host.
run_as_tool_owner() {
    if [ "$runner_only" = true ]; then
        run_as_root "$@"
    else
        "$@"
    fi
}

# Runs a command of the Rust toolchain that this script installs: the runner's
# trusted toolchain as root, or the current user's toolchain.
run_rust_tool() {
    if [ "$runner_only" = true ]; then
        run_as_root env \
            CARGO_HOME="$trusted_cargo_home" \
            RUSTUP_HOME="$trusted_rustup_home" \
            "$@"
    else
        "$@"
    fi
}

# Runs a command as whoever builds NVX on this host: the runner service
# account, with the runner service's environment, or the current session.
run_as_builder() {
    if [ "$runner_only" = true ]; then
        run_as_runner "$@"
    else
        "$@"
    fi
}

# Runs a command with the Rust targets that this mode's builds use appended:
# the musl target of the guest binaries and, for runner jobs, the bare-metal
# and UEFI targets of OpenVMM's tests.
with_rust_targets() {
    if [ "$runner_only" = true ]; then
        "$@" x86_64-unknown-none x86_64-unknown-linux-musl x86_64-unknown-uefi
    else
        "$@" x86_64-unknown-linux-musl
    fi
}

# Downloads a release artifact from a URL into a file and checks the pinned
# SHA-256 checksum before anything runs or unpacks it.
download_verified() {
    require_command curl
    run_as_tool_owner curl --fail --location --proto '=https' --tlsv1.2 \
        --silent --show-error --output "$3" "$1"
    printf '%s  %s\n' "$2" "$3" | run_as_tool_owner sha256sum --check - ||
        die "SHA-256 checksum mismatch: $1"
}

# Adds the user who runs this script to a group. The membership applies only
# to the user's new sessions, so setup asks them to reconnect.
grant_group() {
    user_name=${SUDO_USER:-$(id -un)}
    if [ "$user_name" != root ] &&
        ! id -nG "$user_name" | tr ' ' '\n' | grep -Fxq "$1"; then
        run_as_root usermod -aG "$1" "$user_name"
        reconnect_required=true
    fi
}

validate_runner_state_paths() {
    target_directory=${1:-$runner_directory}
    if run_as_root test -L "$target_directory"; then
        die "runner directory must not be a symlink: ${target_directory}"
    fi
    if run_as_root test -e "$target_directory" &&
        ! run_as_root test -d "$target_directory"; then
        die "runner path is not a directory: ${target_directory}"
    fi
    for entry in \
        .credentials .credentials_rsaparams .env .nvx-labels .path \
        .runner .service runsvc.sh svc.sh; do
        state_path=${target_directory}/${entry}
        if run_as_root test -L "$state_path"; then
            die "runner state path must not be a symlink: ${state_path}"
        fi
        if run_as_root test -e "$state_path" &&
            ! run_as_root test -f "$state_path"; then
            die "runner state path is not a regular file: ${state_path}"
        fi
        if run_as_root test -f "$state_path" &&
            [ "$(run_as_root stat -c %h "$state_path")" -ne 1 ]; then
            die "runner state file must not have multiple links: ${state_path}"
        fi
    done
}

validate_runner_work_paths() {
    target_directory=${1:-$runner_directory}
    target_work_directory=${target_directory}/_work
    for path in \
        "$target_work_directory" \
        "${target_work_directory}/_temp" \
        "${target_work_directory}/_diag" \
        "${target_work_directory}/_sccache" \
        "${target_work_directory}/_temp/cargo-home"; do
        if run_as_root test -L "$path"; then
            die "runner writable path must not be a symlink: ${path}"
        fi
        if run_as_root test -e "$path" && ! run_as_root test -d "$path"; then
            die "runner writable path is not a directory: ${path}"
        fi
    done
}

runner_service_name() {
    target_directory=${1:-$runner_directory}
    validate_runner_state_paths "$target_directory"
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
    trap 'if [ -n "$runner_package_directory" ]; then run_as_root rm -rf "$runner_package_directory"; fi' \
        0 HUP INT TERM
    download_verified \
        "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/${RUNNER_ARTIFACT}" \
        "$RUNNER_SHA256" "$runner_package_archive"
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
        package_hardlink=$(run_as_root find "${runner_directory}/${entry}" \
            -type f -links +1 -print -quit)
        [ -z "$package_hardlink" ] ||
            die "installed runner package contains a hard link: ${package_hardlink}"
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
    validate_runner_state_paths
    run_as_root rm -rf "$runner_package_directory"
    runner_package_directory=
    trap - 0 HUP INT TERM
}

refresh_runner_service_script() {
    [ -n "$removed_service_name" ] || return 0
    run_as_root python3 -I - "$runner_directory" "$removed_service_name" "$runner_name" <<'PY'
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
    validate_runner_state_paths
    run_as_root python3 -I -c '
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
    temporary_directory=${work_directory}/_temp
    diagnostics_directory=${runner_directory}/_diag
    diagnostics_target=${work_directory}/_diag
    validate_runner_state_paths
    validate_runner_work_paths
    if run_as_root test -d "$work_directory" &&
        [ "$(run_as_root stat -c %U "$work_directory")" != \
            "$runner_service_account" ]; then
        run_as_root rm -rf "$work_directory"
    fi
    run_as_root mkdir -p \
        "$work_directory" \
        "$diagnostics_target" \
        "${runner_cargo_home}/bin" \
        "$runner_sccache_dir"

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
    for path in \
        "$work_directory" \
        "$temporary_directory" \
        "$diagnostics_target" \
        "$runner_cargo_home" \
        "$runner_sccache_dir"; do
        run_as_root chown \
            "${runner_service_account}:${runner_service_account}" "$path"
        run_as_root chmod u=rwx,go= "$path"
    done
}

migrate_legacy_runner() {
    [ "$runner_directory_is_default" = true ] || return 0
    validate_runner_state_paths "$legacy_runner_directory"
    validate_runner_work_paths "$legacy_runner_directory"
    [ -f "${legacy_runner_directory}/.runner" ] || return 0
    [ -n "$runner_token" ] ||
        die "runner registration token is required to migrate and refresh labels"

    validate_runner_state_paths
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
    printf '[Service]\nEnvironment="PATH=%s"\nEnvironment="CARGO_HOME=%s"\nEnvironment="RUSTUP_HOME=%s"\nEnvironment="RUSTC_WRAPPER=sccache"\nEnvironment="CARGO_INCREMENTAL=0"\nEnvironment="SCCACHE_DIR=%s"\nEnvironment="SCCACHE_CACHE_SIZE=10G"\nLimitCORE=infinity\n' \
        "$runner_service_path" \
        "$runner_cargo_home" \
        "$trusted_rustup_home" \
        "$runner_sccache_dir" |
        run_as_root tee "${drop_in}/nvx.conf" >/dev/null
    run_as_root systemctl daemon-reload
    run_as_root systemctl stop "$service_name"
    protect_runner_installation
    set_runner_disable_update
    printf '%s\n' "$runner_service_path" |
        run_as_root tee "${runner_directory}/.path" >/dev/null
    run_as_root systemctl start "$service_name"
}

# Development hosts build guest artifacts with Docker and Buildx. The
# distribution's Docker packages can replace another Docker installation, such
# as Docker's own docker-ce, so a host that has a Docker CLI keeps its Docker.
needs_docker_packages() {
    [ "$runner_only" = false ] && ! command -v docker >/dev/null 2>&1
}

# Installs the build dependencies of both modes and each mode's own: Docker
# with Buildx on a development host that lacks Docker, or, with tdnf, the
# Actions runner's dependencies, which installdependencies.sh does not install
# on Azure Linux.
install_packages() {
    if command -v apt-get >/dev/null 2>&1; then
        set -- bc binutils bison build-essential ca-certificates cmake cpio curl \
            diffutils flex git gzip iproute2 iptables libarchive-tools libelf-dev \
            libssl-dev make ninja-build patch perl pkg-config protobuf-compiler \
            python3 rsync tar util-linux xz-utils zstd
        if needs_docker_packages; then
            set -- "$@" docker-buildx docker.io
        fi
        run_as_root apt-get update
        run_as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y "$@"
    elif command -v tdnf >/dev/null 2>&1; then
        set -- bc binutils bison ca-certificates cmake cpio curl diffutils \
            elfutils-libelf-devel findutils flex gcc gcc-c++ git glibc-devel \
            glibc-iconv gzip iproute iptables kernel-headers libarchive \
            libarchive-devel make ninja-build openssl openssl-devel patch perl \
            perl-FindBin perl-IPC-Cmd perl-Time-Piece perl-lib pkgconf \
            pkgconf-pkg-config protobuf python3 rsync shadow-utils tar util-linux \
            which xz zstd
        if [ "$runner_only" = true ]; then
            set -- "$@" icu lttng-ust
        elif needs_docker_packages; then
            set -- "$@" docker-buildx docker-cli moby-engine
        fi
        run_as_root tdnf install -y "$@"
    elif command -v dnf >/dev/null 2>&1; then
        set -- bc binutils bison ca-certificates cmake cpio curl diffutils \
            elfutils-libelf-devel findutils flex gcc gcc-c++ git glibc-devel gzip \
            iproute iptables kernel-headers libarchive libarchive-devel make \
            ninja-build openssl openssl-devel patch perl pkgconf \
            pkgconf-pkg-config protobuf-compiler python3 rsync shadow-utils tar \
            util-linux which xz zstd
        if needs_docker_packages; then
            set -- "$@" docker-buildx moby-engine
        fi
        run_as_root dnf install -y "$@"
    else
        die "supported package manager not found (apt-get, dnf, or tdnf)"
    fi
}

# Installs the pinned rustup with the pinned Rust release as its default
# toolchain.
install_rustup() {
    if [ "$runner_only" = true ]; then
        installer_directory=$trusted_tool_root
    else
        installer_directory=${TMPDIR:-/tmp}
    fi
    installer=$(run_as_tool_owner mktemp \
        "${installer_directory}/rustup-init-${RUSTUP_VERSION}.XXXXXX")
    trap 'run_as_tool_owner rm -f "$installer"' 0 HUP INT TERM
    download_verified \
        "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/${RUSTUP_ARTIFACT}" \
        "$RUSTUP_SHA256" "$installer"
    run_as_tool_owner chmod 0755 "$installer"
    run_rust_tool "$installer" -y --no-modify-path --profile minimal \
        --default-toolchain "$RUST_TOOLCHAIN"
    run_as_tool_owner rm -f "$installer"
    trap - 0 HUP INT TERM
}

# Installs the pinned sccache release into the trusted toolchain.
install_sccache() {
    sccache=${trusted_cargo_home}/bin/sccache
    if [ ! -x "$sccache" ] ||
        ! "$sccache" --version | grep -Fq "sccache ${SCCACHE_VERSION}"; then
        (
            package_directory=$(run_as_root mktemp -d \
                "${trusted_tool_root}/sccache-package.XXXXXX")
            archive=${package_directory}/sccache.tar.gz
            root=${package_directory}/root
            trap 'run_as_root rm -rf "$package_directory"' 0 HUP INT TERM
            download_verified \
                "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/${SCCACHE_ARTIFACT}" \
                "$SCCACHE_SHA256" "$archive"
            run_as_root mkdir -p "$root"
            run_as_root tar --extract --gzip \
                --file "$archive" \
                --directory "$root"
            extracted=${root}/${SCCACHE_ARTIFACT%.tar.gz}/sccache
            run_as_root test -x "$extracted" ||
                die "sccache executable was not found after extraction"
            run_as_root install -m 0755 "$extracted" "$sccache"
        )
    fi
}

# Installs the pinned Rust release with this mode's targets and the pinned
# cargo-nextest: for the current user on a development host, or, for runner
# jobs, with the pinned sccache as a toolchain that only root can modify.
install_rust_tools() {
    rust_bin=
    if [ "$runner_only" = true ]; then
        rust_bin=${trusted_cargo_home}/bin/
        run_as_root mkdir -p "$trusted_cargo_home" "$trusted_rustup_home"
    fi
    if ! command -v "${rust_bin}rustup" >/dev/null 2>&1; then
        install_rustup
        if [ "$runner_only" = false ]; then
            # shellcheck disable=SC1091
            . "${HOME}/.cargo/env"
        fi
    fi

    run_rust_tool "${rust_bin}rustup" toolchain install "$RUST_TOOLCHAIN" \
        --profile minimal --no-self-update
    with_rust_targets run_rust_tool "${rust_bin}rustup" target add \
        --toolchain "$RUST_TOOLCHAIN"
    rust_version=$(run_rust_tool env RUSTUP_TOOLCHAIN="$RUST_TOOLCHAIN" \
        "${rust_bin}rustc" --version | awk '{print $2}')
    [ "$rust_version" = "$RUST_TOOLCHAIN" ] ||
        die "Rust ${RUST_TOOLCHAIN} is required, found ${rust_version}"
    # Run cargo-nextest itself, as the invoking user: cargo reads the
    # configuration of the current directory, such as its aliases, which
    # another user, such as a runner's job, may control.
    if ! "${rust_bin}cargo-nextest" --version 2>/dev/null |
        grep -Fq "cargo-nextest ${CARGO_NEXTEST_VERSION}"; then
        run_rust_tool "${rust_bin}cargo" +"$RUST_TOOLCHAIN" install --locked \
            --force cargo-nextest --version "$CARGO_NEXTEST_VERSION"
    fi
    if [ "$runner_only" = true ]; then
        install_sccache
        run_as_root chown -R root:root "$trusted_tool_root"
        run_as_root chmod -R go-w "$trusted_tool_root"
    fi
}

# Starts Docker and lets the user who runs this script build guest artifacts
# with it.
configure_docker_access() {
    require_command systemctl
    run_as_root systemctl enable --now docker
    getent group docker >/dev/null 2>&1 ||
        die "the Docker package did not create its access group"
    grant_group docker
}

restrict_docker_access() {
    if getent group docker >/dev/null 2>&1 &&
        id -nG "$runner_service_account" | tr ' ' '\n' | grep -Fxq docker; then
        run_as_root gpasswd -d "$runner_service_account" docker
    fi
}

# Lets only the backend's group open the backend's device, now and after every
# boot, and makes that group the runner service account's only supplementary
# group, or adds the user who runs this script to it.
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
    if [ "$runner_only" = true ]; then
        run_as_root usermod -G "$backend" "$runner_service_account"
    else
        grant_group "$backend"
    fi
}

install_runner() {
    validate_runner_state_paths
    validate_runner_work_paths
    migrate_legacy_runner
    run_as_root mkdir -p "$runner_directory"
    prepare_runner_package
    install_or_verify_runner_package
    if ! grep -Eq '^ID=(azurelinux|mariner)$' /etc/os-release; then
        run_as_root "${runner_directory}/bin/installdependencies.sh"
    fi

    configured_runner_name=
    if run_as_root test -f "${runner_directory}/.runner"; then
        configured_runner_name=$(run_as_root python3 -I -c \
            'import json, sys; print(json.load(open(sys.argv[1], encoding="utf-8-sig"))["agentName"])' \
            "${runner_directory}/.runner")
    fi
    registration_required=false
    if [ "$configured_runner_name" != "$runner_name" ] ||
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

check_platform() {
    [ "$(uname -s)" = Linux ] || die "this script requires Linux"
    [ "$(uname -m)" = x86_64 ] || die "this script requires x86_64"
}

# Checks the Python release and the commands that both modes need, and those
# that the caller names.
check_host_tools() {
    for command_name in python3 git curl rustup cargo cargo-nextest gcc make ld \
        bison flex cpio gzip sha256sum tar xz zstd "$@"; do
        require_command "$command_name"
    done
    python_version=$(python3 -c \
        'import sys; print(".".join(map(str, sys.version_info[:3])))')
    version_at_least "$python_version" "$PYTHON_MINIMUM_VERSION" ||
        die "Python ${PYTHON_MINIMUM_VERSION} or newer is required"
}

# Checks that installed_targets lists every target given.
check_rust_targets() {
    for target in "$@"; do
        printf '%s\n' "$installed_targets" | grep -Fxq "$target" ||
            die "Rust target ${target} is not installed"
    done
}

# Checks that whoever builds NVX on this host runs the pinned Rust release,
# with this mode's targets, and the pinned cargo-nextest.
check_rust_tools() {
    rust_version=$(run_as_builder env RUSTUP_TOOLCHAIN="$RUST_TOOLCHAIN" \
        rustc --version | awk '{print $2}')
    [ "$rust_version" = "$RUST_TOOLCHAIN" ] ||
        die "Rust ${RUST_TOOLCHAIN} is required, found ${rust_version}"
    run_as_builder cargo nextest --version |
        grep -Fq "cargo-nextest ${CARGO_NEXTEST_VERSION}" ||
        die "cargo-nextest ${CARGO_NEXTEST_VERSION} is not installed"
    installed_targets=$(run_as_builder env RUSTUP_TOOLCHAIN="$RUST_TOOLCHAIN" \
        rustup target list --installed)
    with_rust_targets check_rust_targets
}

# Checks that whoever builds NVX on this host can open the backend's device.
check_backend_access() {
    device=/dev/${backend}
    [ -c "$device" ] || die "${device} is not a character device"
    if [ "$runner_only" = true ]; then
        run_as_runner sh -c "test -r ${device} && test -w ${device}" ||
            die "${runner_service_account} cannot access ${device}"
    else
        [ -r "$device" ] && [ -w "$device" ] ||
            die "current session cannot access ${device}"
    fi
}

check_runner_host() {
    check_platform
    check_host_tools diff sccache systemctl
    check_rust_tools
    run_as_runner sccache --version |
        grep -Fq "sccache ${SCCACHE_VERSION}" ||
        die "sccache ${SCCACHE_VERSION} is not installed"
    [ "$(stat -c %U "$trusted_tool_root")" = root ] ||
        die "trusted Rust toolchain is not root-owned"
    run_as_runner test ! -w "${trusted_cargo_home}/bin/cargo" ||
        die "runner service account can modify trusted Cargo"
    run_as_runner test ! -w "$trusted_rustup_home" ||
        die "runner service account can modify trusted Rustup state"
    check_backend_access
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

    validate_runner_state_paths
    validate_runner_work_paths
    if run_as_root test -f "${runner_directory}/.runner"; then
        # A dependency-only pass must not require state updated by a named pass.
        if [ "$configure_runner" = false ] && [ "$check_only" = false ]; then
            return 0
        fi
        if [ "$configure_runner" = true ]; then
            configured_runner_name=$(run_as_root python3 -I -c \
                'import json, sys; print(json.load(open(sys.argv[1], encoding="utf-8-sig"))["agentName"])' \
                "${runner_directory}/.runner")
            [ "$configured_runner_name" = "$runner_name" ] ||
                die "configured runner is ${configured_runner_name}, expected ${runner_name}"
        fi
        runner_disable_update=$(run_as_root python3 -I -c \
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
        run_as_runner test -w "$runner_sccache_dir" ||
            die "runner service account cannot modify its sccache directory"
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
            "RUSTUP_HOME=${trusted_rustup_home}" \
            "RUSTC_WRAPPER=sccache" \
            "CARGO_INCREMENTAL=0" \
            "SCCACHE_DIR=${runner_sccache_dir}" \
            "SCCACHE_CACHE_SIZE=10G"; do
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
            "RUSTUP_HOME=${trusted_rustup_home}" \
            "RUSTC_WRAPPER=sccache" \
            "CARGO_INCREMENTAL=0" \
            "SCCACHE_DIR=${runner_sccache_dir}" \
            "SCCACHE_CACHE_SIZE=10G"; do
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

# Checks that the current session can build guest artifacts with Docker and
# Buildx.
check_docker() {
    require_command docker
    docker version >/dev/null 2>&1 ||
        die "Docker is not available to the current session"
    docker buildx version >/dev/null 2>&1 || die "Docker Buildx is not installed"
}

check_development_host() {
    check_platform
    [ -f "${workspace}/scripts/nvx.py" ] ||
        die "NVX checkout not found at ${workspace}"
    [ -f "${workspace}/openvmm/Cargo.toml" ] ||
        die "OpenVMM submodule is not initialized"
    check_backend_access
    check_host_tools
    check_rust_tools
    check_docker

    (
        cd "$workspace"
        python3 scripts/nvx.py verify
    )
}

check_build() {
    (
        cd "$workspace"
        RUSTUP_TOOLCHAIN=$RUST_TOOLCHAIN cargo build --release --locked --features cli \
            --manifest-path nvx-rs/Cargo.toml --target-dir nvx-rs/target
        # Packaging and the adversarial executor take only an nvx-rs whose
        # provenance binds it to the checkout's sources.
        RUSTUP_TOOLCHAIN=$RUST_TOOLCHAIN NVX_RS=nvx-rs/target/release/nvx-rs \
            python3 scripts/nvx.py record-nvx-rs-provenance
        nvx-rs/target/release/nvx-rs run --hypervisor "$backend" --dry-run
    )
}

build_nvx() {
    check_docker
    (
        cd "$workspace"
        RUSTUP_TOOLCHAIN=$RUST_TOOLCHAIN python3 scripts/nvx.py build --guest all
    )
}

check_guest_bundle() {
    bundle_path=$1
    [ -d "$bundle_path" ] || die "guest bundle not found: $bundle_path"
    for name in vmlinux vmlinux.config vmlinux.provenance.json initramfs.cpio.gz \
        initramfs.cpio.gz.packages.json initramfs.provenance.json initramfs-ubuntu.cpio.gz \
        initramfs-ubuntu.cpio.gz.packages.json ubuntu-distro.erofs \
        ubuntu-distro.erofs.manifest.json ubuntu-smoke-scratch.ext4 \
        initramfs-azurelinux.cpio.gz initramfs-azurelinux.cpio.gz.packages.json \
        REVISION SHA256SUMS; do
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
    for name in vmlinux vmlinux.config vmlinux.provenance.json initramfs.cpio.gz \
        initramfs.cpio.gz.packages.json initramfs.provenance.json initramfs-ubuntu.cpio.gz \
        initramfs-ubuntu.cpio.gz.packages.json ubuntu-distro.erofs \
        ubuntu-distro.erofs.manifest.json ubuntu-smoke-scratch.ext4 \
        initramfs-azurelinux.cpio.gz initramfs-azurelinux.cpio.gz.packages.json; do
        source_path="${workspace}/build/${name}"
        [ -f "$source_path" ] || die "missing guest artifact: $source_path"
        cp "$source_path" "$bundle_path"
    done
    git -C "$workspace" rev-parse HEAD >"${bundle_path}/REVISION"
    (
        cd "$bundle_path"
        sha256sum vmlinux vmlinux.config vmlinux.provenance.json initramfs.cpio.gz \
            initramfs.cpio.gz.packages.json initramfs.provenance.json initramfs-ubuntu.cpio.gz \
            initramfs-ubuntu.cpio.gz.packages.json ubuntu-distro.erofs \
            ubuntu-distro.erofs.manifest.json ubuntu-smoke-scratch.ext4 \
            initramfs-azurelinux.cpio.gz initramfs-azurelinux.cpio.gz.packages.json \
            REVISION >SHA256SUMS
    )
    check_guest_bundle "$bundle_path"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --backend)
            [ "$#" -ge 2 ] || die "--backend requires a value"
            backend=$2
            shift 2
            ;;
        --check-only)
            check_only=true
            shift
            ;;
        --workspace)
            [ "$#" -ge 2 ] || die "--workspace requires a path"
            development_option=$1
            workspace=$2
            shift 2
            ;;
        --guest-bundle)
            [ "$#" -ge 2 ] || die "--guest-bundle requires a path"
            development_option=$1
            guest_bundle=$2
            shift 2
            ;;
        --bundle-only)
            development_option=$1
            bundle_only=true
            shift
            ;;
        --skip-build)
            development_option=$1
            skip_build=true
            shift
            ;;
        --runner-only)
            runner_only=true
            shift
            ;;
        --runner-name)
            [ "$#" -ge 2 ] || die "--runner-name requires a value"
            runner_option=$1
            runner_name=$2
            configure_runner=true
            shift 2
            ;;
        --benchmark-runner)
            runner_option=$1
            benchmark_runner=true
            shift
            ;;
        --extra-runner-labels)
            [ "$#" -ge 2 ] || die "--extra-runner-labels requires a value"
            runner_option=$1
            extra_runner_labels=$2
            [ -n "$extra_runner_labels" ] ||
                die "--extra-runner-labels requires a nonempty value"
            shift 2
            ;;
        --repository-url)
            [ "$#" -ge 2 ] || die "--repository-url requires a value"
            runner_option=$1
            repository_url=$2
            shift 2
            ;;
        --runner-directory)
            [ "$#" -ge 2 ] || die "--runner-directory requires a value"
            runner_option=$1
            runner_directory=$2
            runner_directory_is_default=false
            shift 2
            ;;
        --runner-token-stdin)
            runner_option=$1
            runner_token_stdin=true
            configure_runner=true
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

validate_options
# Only a valid runner invocation consumes the token, so a misplaced option
# never waits for one.
if [ "$runner_token_stdin" = true ]; then
    IFS= read -r runner_token || die "could not read runner token from stdin"
    runner_token=$(printf '%s' "$runner_token" | tr -d '\r\n')
fi
read_tool_versions

if [ "$runner_only" = true ]; then
    report_invariant_tsc /proc/cpuinfo
    [ "$(id -u)" -ne 0 ] || die "run this script as the SSH administrator, not root"
    require_command sudo
    sudo -n true || die "passwordless sudo is required"
    legacy_runner_directory=${HOME}/actions-runner
    runner_cargo_home=${runner_directory}/_work/_temp/cargo-home
    runner_sccache_dir=${runner_directory}/_work/_sccache
    runner_labels_file=${runner_directory}/.nvx-labels

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
    check_runner_host
    printf 'NVX_RUNNER_SETUP_COMPLETE=1\n'
    exit 0
fi

resolved_workspace=$(CDPATH='' cd -- "$workspace" 2>/dev/null && pwd) ||
    die "workspace directory not found: $workspace"
workspace=$resolved_workspace

if [ "$bundle_only" = true ]; then
    create_guest_bundle "$guest_bundle"
    printf 'NVX_GUEST_BUNDLE=%s\n' "$guest_bundle"
    exit 0
fi

if [ "$check_only" = true ]; then
    check_development_host
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
configure_backend_access
if [ "$reconnect_required" = true ]; then
    printf '%s\n' \
        'NVX_SETUP_RECONNECT_REQUIRED=1' \
        "Reconnect so the docker and ${backend} group memberships apply, then rerun this script."
    exit 20
fi
if [ "$skip_build" = false ]; then
    build_nvx
    check_build
fi
if [ -n "$guest_bundle" ]; then
    create_guest_bundle "$guest_bundle"
fi
check_development_host
printf 'NVX_SETUP_COMPLETE=1\n'
