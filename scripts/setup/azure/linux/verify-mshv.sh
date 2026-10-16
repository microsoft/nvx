#!/bin/bash
# Reports whether this VM booted as the MSHV root partition.
# Exits non-zero if any check fails so it can be used as an automated gate.
rc=0

echo "== kernel =="
kernel=$(uname -r)
echo "$kernel"
case "$kernel" in
    *mshv*) echo "PASS: running an mshv kernel" ;;
    *)
        echo "FAIL: not an mshv kernel, the Dom0 boot entry was not used"
        rc=1
        ;;
esac

echo
echo "== mshv device nodes =="
# This is the decisive check. The Dom0 GRUB entry is fail-open, so the
# kernel check above can pass while the hypervisor never started, for
# example when Secure Boot blocks HvLoader.efi.
if [ -e /dev/mshv ]; then
    ls -l /dev/mshv /dev/mshv_diag 2>/dev/null || true
    echo "PASS: /dev/mshv present, this OS is the MSHV root partition"
else
    echo "FAIL: /dev/mshv missing, the hypervisor did not start"
    echo "      Most likely cause: Secure Boot is enabled. Check the serial"
    echo "      console for 'SHIM_LOCK verification failed'."
    rc=1
fi

echo
echo "== dmesg =="
if dmesg 2>/dev/null | grep -qi 'running as root partition'; then
    dmesg | grep -Ei 'mshv|root partition' | head -n 20
    echo "PASS: kernel reports running as root partition"
else
    echo "FAIL: no 'running as root partition' line in dmesg"
    rc=1
fi

echo
echo "== VMM and runtime =="
if command -v cloud-hypervisor >/dev/null 2>&1; then
    cloud-hypervisor --version
else
    echo "FAIL: cloud-hypervisor not found"
    rc=1
fi
if command -v kata-runtime >/dev/null 2>&1; then
    kata-runtime --version 2>/dev/null | head -n 3
else
    echo "WARN: kata-runtime not found"
fi

echo
echo "== packages =="
rpm -qa 2>/dev/null | grep -Ei 'mshv|hvloader|cloud-hypervisor|kata' | sort || true

echo
if [ "$rc" = "0" ]; then
    echo "RESULT: PASS - MSHV is running and /dev/mshv is available"
else
    echo "RESULT: FAIL - see the failures above"
fi
exit "$rc"
