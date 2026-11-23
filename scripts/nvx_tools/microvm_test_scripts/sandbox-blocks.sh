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
[ ! -b /dev/vdc ] || fail 30
fixed_virtio_present d0003000 19 || fail 21
fixed_virtio_present d0006000 22 || fail 24
# The windows and interrupts of the removed runtime and custom layers stay unassigned.
fixed_virtio_absent d0004000 || fail 31
fixed_virtio_absent d0005000 || fail 32
[ "$(cat /sys/block/vda/ro)" = 1 ] || fail 25
[ "$(cat /sys/block/vdb/ro)" = 0 ] || fail 28
printf MICROVM-SCRATCH-OK | dd of=/dev/vdb bs=512 count=1 conv=sync,notrunc 2>/dev/null
[ "$(dd if=/dev/vdb bs=512 count=1 2>/dev/null | head -c 18)" = MICROVM-SCRATCH-OK ] || fail 29
echo NVX-SANDBOX-BLOCKS-OK
nvx-exit 0
