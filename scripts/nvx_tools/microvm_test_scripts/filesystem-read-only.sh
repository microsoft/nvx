set -eu
fail() {
    code="$1"
    echo "NVX-FILESYSTEM-READ-ONLY-FAIL code=$code"
    nvx-exit "$code"
    exit "$code"
}

grep -q ' /mnt/share virtiofs ' /proc/mounts || fail 20
[ "$(cat /mnt/share/seed)" = NVX-FILESYSTEM-READ-ONLY ] || fail 21
if touch /mnt/share/mutation 2>/dev/null; then
    fail 22
fi
grep -q 'virtio_mmio.device=0x1000@0xd0001000:6' /proc/cmdline || fail 23
grep -q 'virtfs_tag=microvm' /proc/cmdline || fail 24
grep -q 'virtfs_mode=ro' /proc/cmdline || fail 25
echo NVX-FILESYSTEM-READ-ONLY-OK
nvx-exit 0
