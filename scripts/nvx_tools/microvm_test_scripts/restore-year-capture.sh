# The simulated one-year restore's preparation before the capture
# (doc/ci.md): a CPU-bound workload that the capture freezes, as the guest
# freezes its workload cgroup for a capture with paired scratch, a timer that
# the simulated downtime expires, the clocks, the frequency correction, the
# workload's CPU time, and the kernel log's length before the capture, and a
# slew of the clock that is still in flight when the capture starts.
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
cut -d' ' -f14,15 "/proc/$(cat "$year/busy.pid")/stat" >"$year/ticks"
sed -n 's/^usage_usec //p' "$workload/cpu.stat" >"$year/usage"
dmesg | wc -l >"$year/dmesg"
sed -n 's/^frequency_ppb=//p' /run/nvx/time/state >"$year/frequency"
/sbin/nvx-time-probe clock --measure-ms 0 >"$year/clock"
# A slew in flight at the capture, as the discipline's polls leave one: an
# adjtime of 5 ms, which the kernel slews at 500 ppm for 10 s, and whose rate
# it caches in its tick length from the next second on. The wait lets that
# second pass, and the capture, a fraction of a second later, must keep the slew
# out of the monotonic clocks' advance across the downtime.
adjtimex -o 5000 >/dev/null
sleep 1.2
