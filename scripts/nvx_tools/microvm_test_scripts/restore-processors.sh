set -eu
online_count="$(getconf _NPROCESSORS_ONLN)"
case "$online_count" in
    1 | 2 | 4 | 8) ;;
    *) echo "NVX-RESTORE-PROCESSORS-FAIL count=$online_count"; exit 92 ;;
esac
if [ "$online_count" -eq 1 ]; then
    expected_online=0
else
    expected_online="0-$((online_count - 1))"
fi
actual_online="$(cat /sys/devices/system/cpu/online)"
[ "$actual_online" = "$expected_online" ] || {
    echo "NVX-RESTORE-PROCESSORS-FAIL expected=$expected_online actual=$actual_online"
    exit 93
}
cpu=0
while [ "$cpu" -lt "$online_count" ]; do
    actual="$(taskset -c "$cpu" awk '{ print $39 }' /proc/self/stat)"
    [ "$actual" -eq "$cpu" ] || {
        echo "NVX-RESTORE-PROCESSORS-FAIL requested=$cpu actual=$actual"
        exit 94
    }
    echo "NVX-RESTORE-PROCESSOR-OK count=$online_count cpu=$cpu"
    cpu=$((cpu + 1))
done
echo "NVX-RESTORE-PROCESSORS-OK count=$online_count"
