set -eu
fail() {
    code="$1"
    echo "NVX-FILESYSTEM-LIVE-FAIL code=$code"
    nvx-exit "$code"
    exit "$code"
}

grep -q ' /mnt/share virtiofs ' /proc/mounts || fail 30
grep -q 'virtfs_dir=/mnt/share' /proc/cmdline || fail 31
grep -q 'virtfs_mode=rw' /proc/cmdline || fail 32
exec 3<>/mnt/share/open-handle
printf NVX-HANDLE-BEFORE >&3
echo NVX-FILESYSTEM-LIVE-BEFORE
nvx-snapshot
printf NVX-HANDLE-AFTER >&3
exec 3>&-
[ "$(cat /mnt/share/open-handle)" = NVX-HANDLE-BEFORENVX-HANDLE-AFTER ] || fail 33
echo NVX-FILESYSTEM-LIVE-AFTER
nvx-exit 0
