#!/bin/bash
# Installs the MSHV host stack and makes the Dom0 boot entry the default.
#
# Install only the hypervisor, boot loaders, and host kernel needed for Dom0.
#
# Booting the mshv kernel on its own is NOT enough: GRUB has to chainload
# HvLoader.efi first so the hypervisor is running before the Linux root
# partition starts. kernel-mshv ships /etc/grub.d/50_mariner_mshv_menuentry
# which generates that entry, and /etc/default/grub.d/50_mariner_mshv.cfg
# which makes it the default.
#
# This script deliberately FAILS CLOSED. The generated Dom0 entry is
# fail-open by design (if HvLoader cannot start the hypervisor, GRUB falls
# through and boots Linux bare, giving an mshv kernel with no /dev/mshv),
# so any ambiguity here must be surfaced as a non-zero exit rather than an
# optimistic reboot.
set -euo pipefail

GRUB_CFG=/boot/grub2/grub.cfg
LOG=/var/log/enable-mshv.log
# The MSHV host stack that CI measures, as name=version-release: the
# hypervisor and its loader, the root partition's kernel, and the EFI loader
# that starts the hypervisor. The expected Linux/MSHV benchmark host,
# data/linux-mshv-virtual-machine-host.json, requires this hypervisor build
# and kernel series, so move a pin only in the pull request that accepts the
# new host and resets the series (doc/benchmarks.md, "Expected benchmark
# hosts"). Unpinned, every reimage installed the newest releases (#980).
MSHV_PINS=(
    mshv=26100.9444.2609032029.1-1.azl3
    mshv-bootloader-lx=26100.9444.2609032029.1-1.azl3
    kernel-mshv=6.18.34.mshv3-1.azl3
    edk2-hvloader=20240524git3e722403cd16-21.azl3
)

exec > >(tee -a "$LOG") 2>&1
echo "=== enable-mshv started at $(date --iso-8601=seconds) ==="

fail() {
    echo "ERROR: $*"
    echo "=== enable-mshv FAILED, not rebooting ==="
    exit 1
}

# ---------------------------------------------------------------- step 1
# Preflight. These invariants are cheap to check and each one causes a
# confusing downstream failure if violated.
echo "--- preflight ---"
arch=$(uname -m)
[ "$arch" = "x86_64" ] || fail "MSHV requires x86_64, found $arch"

[ -d /sys/firmware/efi ] || fail "not booted in UEFI mode, MSHV needs the ESP"

# /proc/mounts rather than mountpoint(1), which is not guaranteed present.
grep -q ' /boot/efi ' /proc/mounts || fail "/boot/efi is not mounted, cannot stage HvLoader"

if ! grep -qE '^flags[[:space:]]*:.*[[:space:]](vmx|svm)([[:space:]]|$)' /proc/cpuinfo; then
    fail "no vmx/svm flag: this VM size does not support nested virtualization"
fi
echo "preflight OK (arch=$arch, UEFI, ESP mounted, nested virt available)"

# ---------------------------------------------------------------- step 2
echo "--- installing MSHV boot components ---"
mshv_names=()
mshv_packages=()
for pin in "${MSHV_PINS[@]}"; do
    mshv_names+=("${pin%%=*}")
    mshv_packages+=("${pin%%=*}-${pin#*=}.x86_64")
done
echo "pinned MSHV stack:"
printf '  %s\n' "${mshv_packages[@]}"
installed=0
for attempt in 1 2 3; do
    if dnf install -y "${mshv_packages[@]}"; then
        installed=1
        break
    fi
    if [ "$attempt" = "3" ]; then
        break
    fi
    echo "dnf attempt $attempt failed, retrying in 30s"
    sleep 30
done
[ "$installed" = "1" ] || fail "could not install MSHV boot components after 3 attempts"

echo "--- installed MSHV stack ---"
rpm -qa | grep -Ei 'mshv|hvloader' | sort || true

# Fail closed unless exactly the pinned releases are installed. kernel-mshv is
# an install-only package, so dnf keeps another release beside the pinned one,
# and GRUB could boot it.
installed_stack=$(
    rpm -q --queryformat '%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}\n' "${mshv_names[@]}" |
        sort
) || fail "a pinned MSHV package is not installed: ${installed_stack//$'\n'/, }"
pinned_stack=$(printf '%s\n' "${mshv_packages[@]}" | sort)
[ "$installed_stack" = "$pinned_stack" ] ||
    fail "the installed MSHV stack is not the pinned one: ${installed_stack//$'\n'/, }"
echo "installed the pinned MSHV stack"

# Confirm the pieces the Dom0 boot path actually depends on. A partial
# install would otherwise only show up as a silent fail-open boot.
[ -f /boot/efi/HvLoader.efi ] || fail "HvLoader.efi missing from the ESP after install"
[ -f /boot/efi/lxhvloader.dll ] || fail "lxhvloader.dll missing from the ESP after install"
ls /boot/vmlinuz-*mshv* >/dev/null 2>&1 || fail "no mshv kernel found in /boot after install"

# ---------------------------------------------------------------- step 3
[ -f "$GRUB_CFG" ] || fail "$GRUB_CFG not found, cannot configure the Dom0 boot entry"

# Keep the FIRST backup. On a re-run the current config may already be
# modified, so overwriting would destroy the known-good copy.
if [ ! -f "${GRUB_CFG}.pre-mshv" ]; then
    cp -a "$GRUB_CFG" "${GRUB_CFG}.pre-mshv"
    echo "saved pristine grub.cfg to ${GRUB_CFG}.pre-mshv"
else
    echo "${GRUB_CFG}.pre-mshv already exists, keeping the original backup"
fi

# The kernel-mshv scriptlet runs grub2-mkconfig during the dnf
# transaction, which happens BEFORE edk2-hvloader places HvLoader.efi on
# the ESP (observed: kernel-mshv at step 8/11, edk2-hvloader at 9/11).
# That race makes 50_mariner_mshv_menuentry log "MSHV component
# HvLoader.efi does not exist in /boot/efi. Not enabling kernel-mshv
# menuentry" and produce no Dom0 entry. Regenerate now that every package
# is on disk so the result is deterministic.
echo "--- regenerating grub config with HvLoader.efi present ---"
grub2-mkconfig -o "$GRUB_CFG" || fail "grub2-mkconfig failed"

# ---------------------------------------------------------------- step 4
# Validate the real thing, not just that the string appears somewhere.
# Extract the Dom0 menuentry body and assert it contains every element the
# boot path needs.
echo "--- validating the Dom0 boot entry ---"
dom0_body=$(awk '/^menuentry "Dom0"/{f=1} f{print} f && /^}/{exit}' "$GRUB_CFG")

[ -n "$dom0_body" ] || fail "no 'Dom0' menuentry in $GRUB_CFG (kernel-mshv grub snippet did not run; check $LOG for the HvLoader.efi warning)"

echo "$dom0_body" | grep -q 'chainloader .*HvLoader\.efi' ||
    fail "Dom0 entry does not chainload HvLoader.efi"
echo "$dom0_body" | grep -q 'MSHV_SCHEDULER_TYPE=ROOT' ||
    fail "Dom0 entry does not request the root scheduler"
echo "$dom0_body" | grep -qE '^[[:space:]]*linux(16|efi)?[[:space:]].*mshv' ||
    fail "Dom0 entry does not load an mshv kernel"

echo "Dom0 entry validated:"
echo "$dom0_body"

# Catch a syntactically broken config before we ever reboot into it.
if command -v grub2-script-check >/dev/null 2>&1; then
    grub2-script-check "$GRUB_CFG" || fail "$GRUB_CFG failed grub2-script-check"
    echo "grub2-script-check passed"
fi

# ---------------------------------------------------------------- step 5
# A saved entry matters only when the generated configuration uses it.
verify_dom0_default() {
    local defaults value default_count=0 saved_default=0 grub_environment next_entry
    defaults=$(awk '
        /^[[:space:]]*set[[:space:]]+default=/ {
            sub(/^[^=]*=/, "")
            sub(/[[:space:]]*#.*/, "")
            gsub(/["\047]/, "")
            sub(/^[[:space:]]*/, "")
            sub(/[[:space:]]*$/, "")
            print
        }
    ' "$GRUB_CFG") || fail "could not read the active GRUB default"
    while IFS= read -r value; do
        case "$value" in
            Dom0) default_count=$((default_count + 1)) ;;
            "\$saved_entry" | "\${saved_entry}")
                default_count=$((default_count + 1))
                saved_default=1
                ;;
            "\$next_entry" | "\${next_entry}")
                [ "$default_count" -eq 0 ] || fail "GRUB overrides its default after selecting Dom0"
                ;;
            *) fail "GRUB default '$value' does not select Dom0" ;;
        esac
    done <<<"$defaults"
    [ "$default_count" -eq 1 ] || fail "GRUB has no unique Dom0 default"
    if [ "$saved_default" -eq 1 ]; then
        grep -qE '^[[:space:]]*load_env[[:space:]]*(#.*)?$' "$GRUB_CFG" ||
            fail "GRUB uses saved_entry without loading the default grubenv"
        grub2-set-default Dom0 || fail "could not set the saved Dom0 default"
    fi
    grub_environment=$(grub2-editenv - list) || fail "could not read grubenv"
    if [ "$saved_default" -eq 1 ]; then
        printf '%s\n' "$grub_environment" | grep -qx 'saved_entry=Dom0' ||
            fail "the saved GRUB default is not Dom0"
    fi
    next_entry=$(printf '%s\n' "$grub_environment" | sed -n 's/^next_entry=//p')
    [ -z "$next_entry" ] || [ "$next_entry" = Dom0 ] ||
        fail "the one-time GRUB entry '$next_entry' overrides Dom0"
}

verify_dom0_default
echo "Dom0 is the default boot entry"

echo "--- resulting menuentries ---"
grep -n '^menuentry ' "$GRUB_CFG" || true

# Marker meaning "configuration succeeded". It does NOT mean MSHV is
# running: that can only be confirmed after the reboot by verify-mshv.sh.
touch /var/log/enable-mshv.done
echo "=== enable-mshv finished, rebooting into Dom0 ==="
