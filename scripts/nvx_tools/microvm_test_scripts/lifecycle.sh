set -eu
fail() {
    code="$1"
    trap - EXIT
    echo "NVX-LIFECYCLE-FAIL code=$code"
    nvx-exit "$code"
    exit "$code"
}
trap 'status=$?; if [ "$status" -ne 0 ]; then fail "$status"; fi' EXIT

printf '\013' | dd of=/dev/port bs=1 seek=112 count=1 conv=notrunc 2>/dev/null
rtc="$(dd if=/dev/port bs=1 skip=113 count=1 2>/dev/null | od -An -tu1 | tr -d '[:space:]')"
[ "$rtc" = 2 ] || fail 40
[ "$(date +%s)" -ge 1500000000 ] || fail 41
local_timer="$(awk '/^LOC:/ { print $2; exit }' /proc/interrupts)"
[ "${local_timer:-0}" -gt 0 ] || fail 42
uptime_before="$(awk '{ print int($1); exit }' /proc/uptime)"
sleep 1
uptime_after="$(awk '{ print int($1); exit }' /proc/uptime)"
[ $((uptime_after - uptime_before)) -ge 1 ] || fail 43
for octal in 000 015 012 177 377; do
    printf "\\$octal" | dd of=/dev/port bs=1 seek=233 count=1 conv=notrunc 2>/dev/null
done
echo NVX-LIFECYCLE-OK
nvx-exit 0
