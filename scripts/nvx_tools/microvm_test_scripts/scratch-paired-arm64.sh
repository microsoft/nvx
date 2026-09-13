set -eu
fail() {
    code="$1"
    echo "NVX-SCRATCH-PAIRED-FAIL code=$code"
    nvx-exit "$code"
    exit "$code"
}

tries=0
while { [ ! -b /dev/vda ] || [ ! -b /dev/vdb ]; } && [ "$tries" -lt 600 ]; do
    sleep 0.05
    tries=$((tries + 1))
done
[ "$(cat /sys/block/vda/ro)" = 1 ] || fail 60
[ "$(cat /sys/block/vdb/ro)" = 0 ] || fail 61
printf NVX-PAIRED-SNAPSHOT | dd of=/dev/vdb bs=512 count=1 conv=sync,notrunc 2>/dev/null
sync
nvx-port-io write-u8 1541 1
echo NVX-SCRATCH-PAIRED-POST-OUT
first_bytes="$(dd if=/dev/vdb bs=1 count=19 2>/dev/null)"
[ "$first_bytes" = NVX-PAIRED-SNAPSHOT ] || fail 63
echo NVX-SCRATCH-PAIRED-RESTORED
printf NVX-PRIVATE-RESTORE-MUTATION | dd of=/dev/vdb bs=512 count=1 conv=sync,notrunc 2>/dev/null
sync
nvx-exit 0
