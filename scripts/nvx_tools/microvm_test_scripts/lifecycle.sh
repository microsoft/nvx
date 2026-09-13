set -eu
fail() {
    code="$1"
    trap - EXIT
    echo "NVX-LIFECYCLE-FAIL code=$code"
    nvx-exit "$code"
    exit "$code"
}
trap 'status=$?; if [ "$status" -ne 0 ]; then fail "$status"; fi' EXIT

case "$(uname -m)" in
    x86_64)
        printf '\013' | dd of=/dev/port bs=1 seek=112 count=1 conv=notrunc 2>/dev/null
        rtc="$(dd if=/dev/port bs=1 skip=113 count=1 2>/dev/null | od -An -tu1 | tr -d '[:space:]')"
        [ "$rtc" = 2 ] || fail 40
        local_timer="$(awk '/^LOC:/ { print $2; exit }' /proc/interrupts)"
        ;;
    aarch64)
        wall_clock="$(nvx-port-io read-wall-clock 233 234)"
        [ "$wall_clock" -ge 1500000000 ] || fail 40
        local_timer="$(awk '$0 ~ /arch_timer/ { print $2; exit }' /proc/interrupts)"
        ;;
    *) fail 40 ;;
esac
[ "$(date +%s)" -ge 1500000000 ] || fail 41
[ "${local_timer:-0}" -gt 0 ] || fail 42
uptime_before="$(awk '{ print int($1); exit }' /proc/uptime)"
sleep 1
uptime_after="$(awk '{ print int($1); exit }' /proc/uptime)"
[ $((uptime_after - uptime_before)) -ge 1 ] || fail 43
for octal in 000 015 012 177 377; do
    nvx-port-io write-u8 233 "$((0$octal))"
done
echo NVX-LIFECYCLE-OK
nvx-exit 0
