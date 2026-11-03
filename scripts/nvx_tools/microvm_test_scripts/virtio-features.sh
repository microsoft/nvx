set -eu
fail() {
    code="$1"
    echo "NVX-VIRTIO-FEATURES-FAIL code=$code"
    nvx-exit "$code"
    exit "$code"
}

# Counts the virtio devices that a driver has bound: a device's features are
# those that its driver negotiated.
bound_devices() {
    bound=0
    for device in /sys/bus/virtio/drivers/*/virtio*; do
        [ -e "$device" ] && bound=$((bound + 1))
    done
    echo "$bound"
}

# Prints the base of the MMIO window that /proc/iomem lists under the name of
# the virtio-mmio platform device $1.
window_base() {
    while IFS=' ' read -r range separator name _; do
        [ "$separator" = : ] && [ "$name" = "$1" ] || continue
        echo "${range%%-*}"
        return 0
    done </proc/iomem
    return 1
}

# The machine has six fixed virtio devices.
tries=0
while [ "$(bound_devices)" -lt 6 ] && [ "$tries" -lt 200 ]; do
    sleep 0.05
    tries=$((tries + 1))
done
[ "$(bound_devices)" -eq 6 ] || fail 20

for device in /sys/bus/virtio/devices/virtio*; do
    window=$(basename "$(dirname "$(readlink -f "$device")")")
    base=$(window_base "$window") || fail 21
    echo "NVX-VIRTIO-FEATURES base=0x$base device=$(cat "$device/device") features=$(cat "$device/features")"
done
echo NVX-VIRTIO-FEATURES-OK
nvx-exit 0
