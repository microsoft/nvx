# Resolve the Linux resource of a bound virtio device, on either discovery bus.
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

# Require the fixed window and its ISA-specific interrupt binding, not merely
# a working device at some address that the platform did not assign.
fixed_virtio_present() {
    case "$(uname -m)" in
        x86_64)
            grep -q "virtio_mmio.device=0x1000@0x$1:$2" /proc/cmdline
            ;;
        aarch64)
            nvx_fixed_count=0
            for nvx_fixed_device in /sys/bus/virtio/devices/virtio*; do
                [ -e "$nvx_fixed_device" ] || continue
                nvx_fixed_base="$(device_base "${nvx_fixed_device##*/}")" || return 1
                [ "$((0x$nvx_fixed_base))" -eq "$((0x$1))" ] || continue
                nvx_fixed_count=$((nvx_fixed_count + 1))
                nvx_fixed_platform="$(dirname "$(readlink -f "$nvx_fixed_device")")"
                nvx_fixed_range="$(window_range "$(basename "$nvx_fixed_platform")")" || return 1
                nvx_fixed_start=${nvx_fixed_range%%-*}
                nvx_fixed_end=${nvx_fixed_range#*-}
                [ "$((0x$nvx_fixed_start))" -eq "$((0x$1))" ] || return 1
                [ "$((0x$nvx_fixed_end - 0x$nvx_fixed_start + 1))" -eq 4096 ] || return 1
                tr '\000' '\n' <"$nvx_fixed_platform/of_node/compatible" | grep -Fxq 'virtio,mmio' || return 1
                nvx_fixed_spi=$(((0x$1 - 0xd0000000) / 0x1000))
                nvx_fixed_interrupts="$(od -An -tx1 -v "$nvx_fixed_platform/of_node/interrupts" | tr -d '[:space:]')" || return 1
                [ "$nvx_fixed_interrupts" = "$(printf '00000000%08x00000001' "$nvx_fixed_spi")" ] || return 1
            done
            [ "$nvx_fixed_count" -eq 1 ]
            ;;
        *) return 1 ;;
    esac
}

fixed_virtio_absent() {
    case "$(uname -m)" in
        x86_64) ! grep -q "0x$1" /proc/cmdline ;;
        aarch64)
            while read -r nvx_fixed_range nvx_fixed_separator _; do
                [ "$nvx_fixed_separator" = : ] || continue
                nvx_fixed_start=${nvx_fixed_range%%-*}
                [ "$((0x$nvx_fixed_start))" -ne "$((0x$1))" ] || return 1
            done </proc/iomem
            ;;
        *) return 1 ;;
    esac
}
