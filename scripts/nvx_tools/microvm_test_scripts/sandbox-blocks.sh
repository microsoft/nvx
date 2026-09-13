set -eu
fail() {
    code="$1"
    echo "NVX-SANDBOX-BLOCKS-FAIL code=$code"
    nvx-exit "$code"
    exit "$code"
}

for name in vda vdb vdc vdd; do
    tries=0
    while [ ! -b "/dev/$name" ] && [ "$tries" -lt 200 ]; do
        sleep 0.05
        tries=$((tries + 1))
    done
    [ -b "/dev/$name" ] || fail 20
done
case "$(uname -m)" in
    x86_64)
        grep -q 'virtio_mmio.device=0x1000@0xd0003000:4' /proc/cmdline || fail 21
        grep -q 'virtio_mmio.device=0x1000@0xd0004000:12' /proc/cmdline || fail 22
        grep -q 'virtio_mmio.device=0x1000@0xd0005000:9' /proc/cmdline || fail 23
        grep -q 'virtio_mmio.device=0x1000@0xd0006000:11' /proc/cmdline || fail 24
        ;;
    aarch64)
        [ -d /sys/bus/platform/devices/d0003000.virtio_mmio ] || fail 21
        [ -d /sys/bus/platform/devices/d0004000.virtio_mmio ] || fail 22
        [ -d /sys/bus/platform/devices/d0005000.virtio_mmio ] || fail 23
        [ -d /sys/bus/platform/devices/d0006000.virtio_mmio ] || fail 24
        ;;
    *) fail 21 ;;
esac
[ "$(cat /sys/block/vda/ro)" = 1 ] || fail 25
[ "$(cat /sys/block/vdb/ro)" = 1 ] || fail 26
[ "$(cat /sys/block/vdc/ro)" = 1 ] || fail 27
[ "$(cat /sys/block/vdd/ro)" = 0 ] || fail 28
printf MICROVM-SCRATCH-OK | dd of=/dev/vdd bs=512 count=1 conv=sync,notrunc 2>/dev/null
[ "$(dd if=/dev/vdd bs=512 count=1 2>/dev/null | head -c 18)" = MICROVM-SCRATCH-OK ] || fail 29
echo NVX-SANDBOX-BLOCKS-OK
nvx-exit 0
