set -eu
fail() {
    code="$1"
    echo "NVX-VIRTIO-NET-FAIL code=$code"
    nvx-exit "$code"
    exit "$code"
}

tries=0
device=
while [ -z "$device" ] && [ "$tries" -lt 200 ]; do
    for path in /sys/class/net/*; do
        [ -e "$path" ] || continue
        candidate=${path##*/}
        [ "$candidate" = lo ] && continue
        device=$candidate
        break
    done
    [ -n "$device" ] || sleep 0.05
    tries=$((tries + 1))
done
[ -n "$device" ] || fail 20
grep -q 'virtio_mmio.device=0x1000@0xd0000000:' /proc/cmdline || fail 21
grep -q 'virtnet_ip=10.0.0.2' /proc/cmdline || fail 22
grep -q 'virtnet_mask=255.255.255.0' /proc/cmdline || fail 23
grep -q 'virtnet_gw=10.0.0.1' /proc/cmdline || fail 24
[ "$(cat /sys/class/net/$device/address)" = '52:54:00:00:00:02' ] || fail 25
grep -qi 'd0000000-d0000fff.*virtio' /proc/iomem || fail 26
ifconfig "$device" 10.0.0.2 netmask 255.255.255.0 up || fail 27
route add default gw 10.0.0.1 dev "$device" 2>/dev/null || true
arp -d 10.0.0.9 2>/dev/null || true
arp -d 10.0.0.1 2>/dev/null || true
arp -d 10.0.0.10 2>/dev/null || true
ping -c 1 -W 1 10.0.0.9 >/dev/null 2>&1 || true
arp -n 10.0.0.9 | grep -Eqi '10\.0\.0\.9.*52:54:00:00:00:01' || fail 28
ping -c 1 -W 1 192.0.2.7 >/dev/null 2>&1 || true
arp -n 10.0.0.1 | grep -Eqi '10\.0\.0\.1.*52:54:00:00:00:01' || fail 29
ping -c 1 -W 1 10.0.0.10 >/dev/null 2>&1 || true
if arp -n 10.0.0.10 2>/dev/null | grep -qi '52:54:00:00:00:01'; then
    fail 30
fi
echo NVX-VIRTIO-NET-OK
nvx-exit 0
