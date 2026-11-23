# Resolve a bound device's actual resource on either discovery bus.
window_range() {
    while IFS=' ' read -r range separator name _; do
        [ "$separator" = : ] && [ "$name" = "$1" ] || continue
        echo "$range"
        return 0
    done </proc/iomem
    return 1
}

window_base() {
    nvx_window_range="$(window_range "$1")" || return 1
    echo "${nvx_window_range%%-*}"
}

device_base() {
    window_base "$(basename "$(dirname "$(readlink -f "/sys/bus/virtio/devices/$1")")")"
}
