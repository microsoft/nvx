#!/bin/sh
# Normal Linux workload derived from the legacy phase_2_snapshot_restore scenario.
export PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/root TERM=linux
mount -t proc none /proc
mount -t sysfs none /sys
mount -t devtmpfs none /dev 2>/dev/null || mdev -s
mkdir -p /run
mount -t tmpfs none /run
exec </dev/hvc0 >/dev/hvc0 2>&1

fail() {
    echo "NATIVE-FAIL-$1"
    nvx-exit 91
    while :; do sleep 1; done
}

echo NATIVE-BOOT
echo captured-clean > /run/private-marker
sleep 5 &
timer_pid=$!
sleep 1
wall_before=$(date +%s)
uptime_before=$(cut -d. -f1 /proc/uptime)
cpu_before=$(awk '{print $14+$15}' /proc/$$/stat)
echo "NATIVE-BEFORE-$wall_before-$uptime_before-$cpu_before"
echo NATIVE-CAPTURE-REQUEST
nvx-snapshot || fail snapshot
echo NATIVE-CONTINUED
wall_after=$(date +%s)
uptime_after=$(cut -d. -f1 /proc/uptime)
cpu_after=$(awk '{print $14+$15}' /proc/$$/stat)
echo "NATIVE-AFTER-$wall_after-$uptime_after-$cpu_after"
echo "NATIVE-DELTAS-$((wall_after-wall_before))-$((uptime_after-uptime_before))-$((cpu_after-cpu_before))"
[ "$(cat /run/private-marker)" = captured-clean ] || fail private-initial
echo NATIVE-PRIVATE-INITIAL-CLEAN
wait "$timer_pid" || fail timer
echo NATIVE-TIMER-DONE

# Request only the existing blockless restore entropy packet, not a tiered gate.
printf '\245' | dd of=/dev/port bs=1 seek=234 count=1 conv=notrunc 2>/dev/null || fail select
: > /run/entropy-packet
i=0
while [ "$i" -lt 83 ]; do
    dd if=/dev/port bs=1 skip=233 count=1 2>/dev/null >> /run/entropy-packet || fail entropy-read
    i=$((i+1))
done
head -c 18 /run/entropy-packet | grep -q OPENVMM_ENTROPY_V1 || fail entropy-header
entropy_digest=$(tail -c 64 /run/entropy-packet | sha256sum | cut -d' ' -f1)
echo "NATIVE-ENTROPY-$entropy_digest"
echo "$entropy_digest" > /run/private-marker
dd if=/dev/zero of=/run/private-dirty bs=1M count=16 2>/dev/null || fail private-dirty
[ "$(cat /run/private-marker)" = "$entropy_digest" ] || fail private-write
echo "NATIVE-PRIVATE-WRITTEN-$entropy_digest"
echo NATIVE-DONE
nvx-exit 37
while :; do sleep 1; done
