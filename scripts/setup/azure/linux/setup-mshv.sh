#!/bin/bash
# Installs the MSHV host stack and makes the Dom0 boot entry the default.
#
# Azure Linux 3 ships the whole stack in the kata-packages-host
# meta-package: mshv, mshv-bootloader-lx, kernel-mshv, kernel-uvm,
# edk2-hvloader, cloud-hypervisor and kata-containers. This mirrors what
# AgentBaker does when it bakes the AKS V3katagen2 node image
# (parts/linux/cloud-init/artifacts/mariner/cse_install_mariner.sh).
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
echo "--- installing kata-packages-host ---"
installed=0
for attempt in 1 2 3; do
    if dnf install -y kata-packages-host; then
        installed=1
        break
    fi
    if [ "$attempt" = "3" ]; then
        break
    fi
    echo "dnf attempt $attempt failed, retrying in 30s"
    sleep 30
done
[ "$installed" = "1" ] || fail "could not install kata-packages-host after 3 attempts"

echo "--- installed MSHV stack ---"
rpm -qa | grep -Ei 'mshv|hvloader|cloud-hypervisor|kata' | sort || true

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
# Make sure Dom0 is actually selected. The RPM snippet normally emits
# 'set default="Dom0"'; grub2-set-default covers images that use the
# saved_entry mechanism instead. At least one must end up pointing at Dom0.
grub2-set-default "Dom0" 2>/dev/null || true

default_ok=0
grep -qE '^[[:space:]]*set[[:space:]]+default="?Dom0"?' "$GRUB_CFG" && default_ok=1
if [ -r /boot/grub2/grubenv ] && grep -q '^saved_entry=Dom0' /boot/grub2/grubenv; then
    default_ok=1
fi
[ "$default_ok" = "1" ] || fail "Dom0 exists but is not the default boot entry"
echo "Dom0 is the default boot entry"

echo "--- resulting menuentries ---"
grep -n '^menuentry ' "$GRUB_CFG" || true

# Marker meaning "configuration succeeded". It does NOT mean MSHV is
# running: that can only be confirmed after the reboot, by
# mshv-verify.service.
touch /var/log/enable-mshv.done
echo "=== enable-mshv finished, rebooting into Dom0 ==="
