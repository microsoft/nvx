set -eu
fail() {
    code="$1"
    echo "NVX-SANDBOX-BLOCKS-FAIL code=$code"
    nvx-exit "$code"
    exit "$code"
}

for name in vda vdb; do
    tries=0
    while [ ! -b "/dev/$name" ] && [ "$tries" -lt 200 ]; do
        sleep 0.05
        tries=$((tries + 1))
    done
    [ -b "/dev/$name" ] || fail 20
done
grep -q 'virtio_mmio.device=0x1000@0xd0003000:4' /proc/cmdline || fail 21
grep -q 'virtio_mmio.device=0x1000@0xd0006000:11' /proc/cmdline || fail 22
if grep -q 'virtio_mmio.device=0x1000@0xd0004000:' /proc/cmdline; then fail 23; fi
if grep -q 'virtio_mmio.device=0x1000@0xd0005000:' /proc/cmdline; then fail 24; fi
[ "$(cat /sys/block/vda/ro)" = 1 ] || fail 25
[ "$(cat /sys/block/vdb/ro)" = 0 ] || fail 26
printf MICROVM-SCRATCH-OK | dd of=/dev/vdb bs=512 count=1 conv=sync,notrunc 2>/dev/null
[ "$(dd if=/dev/vdb bs=512 count=1 2>/dev/null | head -c 18)" = MICROVM-SCRATCH-OK ] || fail 29
echo NVX-SANDBOX-BLOCKS-OK
nvx-exit 0
