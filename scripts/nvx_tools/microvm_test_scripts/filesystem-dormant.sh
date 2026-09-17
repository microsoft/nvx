set -eu
grep -q 'virtio_mmio.device=0x1000@0xd0001000:6' /proc/cmdline
! grep -q 'virtfs_tag=' /proc/cmdline
echo NVX-FILESYSTEM-DORMANT-BEFORE
nvx-snapshot
IFS= read -r action
sh -c "$action"
