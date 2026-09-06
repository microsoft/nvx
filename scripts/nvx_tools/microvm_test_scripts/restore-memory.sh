set -eu

memtotal_kib=$(awk '/^MemTotal:/ { print $2 }' /proc/meminfo)
case "$memtotal_kib" in
    '' | *[!0-9]*) echo "NVX-RESTORE-MEMORY-FAIL invalid MemTotal"; exit 95 ;;
esac

if [ "$memtotal_kib" -gt 700000 ]; then
    mkdir -p /tmp/nvx-memory-test
    mount -t tmpfs -o size=700m tmpfs /tmp/nvx-memory-test
    dd if=/dev/zero of=/tmp/nvx-memory-test/payload bs=1M count=550 2>/dev/null
    printf NVX-MEMORY-EXPANSION > /tmp/nvx-memory-test/signature
    dd if=/tmp/nvx-memory-test/signature of=/tmp/nvx-memory-test/payload \
        bs=1 seek=576716800 conv=notrunc 2>/dev/null
    actual=$(dd if=/tmp/nvx-memory-test/payload bs=1 skip=576716800 count=20 2>/dev/null)
    [ "$actual" = NVX-MEMORY-EXPANSION ] || {
        echo "NVX-RESTORE-MEMORY-FAIL expanded allocation mismatch"
        exit 96
    }
    rm -f /tmp/nvx-memory-test/payload /tmp/nvx-memory-test/signature
    umount /tmp/nvx-memory-test
fi

echo "NVX-RESTORE-MEMORY-WORKLOAD-OK memtotal_kib=$memtotal_kib"
