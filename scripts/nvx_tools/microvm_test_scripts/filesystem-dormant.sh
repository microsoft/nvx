set -eu
case "$(uname -m)" in
    x86_64) grep -q 'virtio_mmio.device=0x1000@0xd0001000:6' /proc/cmdline ;;
    aarch64) [ -d /sys/bus/platform/devices/d0001000.virtio_mmio ] ;;
    *) exit 1 ;;
esac
! grep -q 'virtfs_tag=' /proc/cmdline
echo NVX-FILESYSTEM-DORMANT-BEFORE
nvx-snapshot
IFS= read -r action
sh -c "$action"
