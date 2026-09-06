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
dd if=/dev/zero of=/dev/vdb bs=512 count=1 conv=notrunc 2>/dev/null & writer=$!
tries=0
while [ "$tries" -lt 2000 ]; do
    set -- $(cat /sys/block/vdb/inflight)
    [ $(( $1 + $2 )) -gt 0 ] && break
    kill -0 "$writer" 2>/dev/null || break
    tries=$((tries + 1))
done
[ "$tries" -lt 2000 ] && kill -0 "$writer" 2>/dev/null || fail 62
printf '\001' | dd of=/dev/port bs=1 seek=1541 count=1 conv=notrunc 2>/dev/null
printf '\002' | dd of=/dev/port bs=1 seek=1541 count=1 conv=notrunc 2>/dev/null
echo NVX-SCRATCH-PAIRED-POST-OUT
wait "$writer"
first_byte="$(dd if=/dev/vdb bs=1 count=1 2>/dev/null | od -An -tu1 | tr -d '[:space:]')"
[ "$first_byte" = 0 ] || fail 63
echo NVX-SCRATCH-PAIRED-RESTORED
printf NVX-PRIVATE-RESTORE-MUTATION | dd of=/dev/vdb bs=512 count=1 conv=sync,notrunc 2>/dev/null
sync
nvx-exit 0
