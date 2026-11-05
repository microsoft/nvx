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
# earlycon=xe9 starts the early console on the byte channel, and the kernel
# console, the boot console hvc1 here and hvc0 without one, replaces it. The
# kernel knows earlycon=, so it does not pass it to user space with the
# parameters that it does not know.
check_early_console() {
    log="$(dmesg)"
    case "$log" in *'bootconsole [earlyxe90] enabled'*) ;; *) fail 44 ;; esac
    case "$log" in *'bootconsole [earlyxe90] disabled'*) ;; *) fail 45 ;; esac
    if printf '%s\n' "$log" | grep -F 'Unknown kernel command line parameters' |
        grep -qF 'earlycon='; then
        fail 46
    fi
}
check_early_console
# OpenVMM relays the byte channel to its stderr beside the boot console, where
# the scenario requires these bytes unchanged and in order. The last one ends
# their line in the scenario's log, which interleaves stderr and the console.
for octal in 000 015 012 177 377 012; do
    printf "\\$octal" | dd of=/dev/port bs=1 seek=233 count=1 conv=notrunc 2>/dev/null
done
echo NVX-LIFECYCLE-OK
nvx-exit 0
