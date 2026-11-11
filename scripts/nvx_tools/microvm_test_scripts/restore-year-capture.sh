# The simulated one-year restore's preparation before the capture
# (doc/ci.md): a CPU-bound workload that the capture freezes, as the guest
# freezes its workload cgroup for a capture with paired scratch, a timer that
# the simulated downtime expires, and, once the time daemon's next poll has
# passed, the clocks, the frequency correction, the workload's CPU time, and
# the kernel log's length before the capture.
set -eu
year=/run/nvx-year
workload=/sys/fs/cgroup/nvx-year-workload
trap 'status=$?; [ "$status" -eq 0 ] || { echo "NVX-RESTORE-YEAR-FAIL setup status=$status"; nvx-exit 88; }' EXIT
mkdir -p "$year"
mountpoint -q /sys/fs/cgroup || mount -t cgroup2 cgroup2 /sys/fs/cgroup
mkdir "$workload"
sh -c 'while :; do :; done' &
echo "$!" >"$year/busy.pid"
echo "$!" >"$workload/cgroup.procs"
(sleep 3600 && cut -d' ' -f1 /proc/uptime >"$year/timer") &
sleep 1
echo 1 >"$workload/cgroup.freeze"
attempts=0
until grep -qx 'frozen 1' "$workload/cgroup.events"; do
    attempts=$((attempts + 1))
    [ "$attempts" -le 500 ]
    sleep 0.01
done
# The time daemon changes the frequency correction only at its discipline
# polls, which hold the port lock that the capture takes too. It polls 16 s
# after its last poll at first, and 64 s after it once it has settled. Right
# after its next poll, the correction that this records is the one that the
# capture holds, since no other poll can come between them. A poll counts one
# accepted or one rejected time sample. The wait gives up after 70 s, within
# what the capture allows this preparation.
polls() {
    grep -E '^(samples|rejected_samples)=' /run/nvx/time/state
}
polled=$(polls)
attempts=0
until [ "$(polls)" != "$polled" ]; do
    attempts=$((attempts + 1))
    [ "$attempts" -le 700 ]
    sleep 0.1
done
cut -d' ' -f14,15 "/proc/$(cat "$year/busy.pid")/stat" >"$year/ticks"
sed -n 's/^usage_usec //p' "$workload/cpu.stat" >"$year/usage"
dmesg | wc -l >"$year/dmesg"
sed -n 's/^frequency_ppb=//p' /run/nvx/time/state >"$year/frequency"
/sbin/nvx-time-probe clock --measure-ms 0 >"$year/clock"
