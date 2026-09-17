set -eu
tries=0
while [ ! -b /dev/vdb ] && [ "$tries" -lt 600 ]; do
    sleep 0.05
    tries=$((tries + 1))
done
[ -b /dev/vdb ] || { nvx-exit 70; exit 70; }
/sbin/nvx-snapshot --tier platform
echo NVX-SCRATCH-FRESH-POST-OUT
blockdev --flushbufs /dev/vdb
value="$(dd if=/dev/vdb bs=1 count=1 2>/dev/null | od -An -tu1 | tr -d '[:space:]')"
echo "NVX-SCRATCH-FRESH-VALUE-$value"
nvx-exit 0
