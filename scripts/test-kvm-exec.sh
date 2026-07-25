#!/bin/sh
set -eu

microvm=${MICROVM:-./target/release/microvm}
kernel=${KERNEL:-build/vmlinux}
initrd=${INITRD:-build/initramfs.cpio.gz}
timeout_seconds=${NVX_KVM_EXEC_TIMEOUT_SECONDS:-60}
memory_mib=${MEM:-128}

for file in "$microvm" "$kernel" "$initrd"; do
    if [ ! -f "$file" ]; then
        echo "missing KVM exec smoke-test input: $file" >&2
        exit 1
    fi
done
if ! command -v timeout >/dev/null 2>&1; then
    echo "KVM exec smoke testing requires timeout(1)" >&2
    exit 1
fi
if ! command -v cc >/dev/null 2>&1; then
    echo "KVM exec smoke testing requires cc(1)" >&2
    exit 1
fi

work_root=$(mktemp -d)
trap 'rm -rf "$work_root"' EXIT HUP INT TERM
script_path=$work_root/workload.sh
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cc -nostdlib -static -Os -s -o "$work_root/kvm-pin-ap" "$script_dir/kvm-pin-ap.c"

run_exec() {
    expected_status=$1
    guest_path=$2
    required_output=$3

    set +e
    output=$(timeout "$timeout_seconds" "$microvm" \
        --kernel "$kernel" \
        --initrd "$initrd" \
        --mem "$memory_mib" \
        --vcpus 2 \
        --mount "$work_root" \
        --exec "$guest_path" \
        --log-level off 2>&1)
    actual_status=$?
    set -e

    if [ "$actual_status" -ne "$expected_status" ]; then
        printf '%s\n' "$output" >&2
        echo "KVM exec exited $actual_status, expected $expected_status" >&2
        exit 1
    fi
    if ! printf '%s\n' "$output" | grep -F "$required_output" >/dev/null; then
        printf '%s\n' "$output" >&2
        echo "KVM exec output did not contain: $required_output" >&2
        exit 1
    fi
    echo "KVM guest exec status $actual_status propagated"
}

for exit_code in 0 37; do
    marker=NVX-KVM-EXEC-SMOKE-$exit_code
    if [ "$exit_code" -eq 37 ]; then
        cat > "$script_path" <<EOF
echo $marker
if ! /mnt/host/kvm-pin-ap; then
    echo NVX-KVM-EXEC-AP-PIN-FAIL
    exit 98
fi
/sbin/nvx-exit 37
exit 99
EOF
    else
        printf 'echo %s\nexit %s\n' "$marker" "$exit_code" > "$script_path"
    fi
    run_exec "$exit_code" /mnt/host/workload.sh "$marker"
done

legacy_marker=NVX-KVM-LEGACY-SHUTDOWN
printf 'echo %s\nexit 37\n' "$legacy_marker" > "$script_path"
set +e
legacy_output=$(timeout "$timeout_seconds" "$microvm" \
    --kernel "$kernel" \
    --initrd "$initrd" \
    --mem "$memory_mib" \
    --vcpus 2 \
    --mount "$work_root" \
    --cmdline "earlycon=xe9 console=hvc0 reboot=t panic=-1 nvx_exec=/mnt/host/workload.sh" \
    --log-level off 2>&1)
legacy_status=$?
set -e
if [ "$legacy_status" -ne 0 ] || ! printf '%s\n' "$legacy_output" | grep -F "$legacy_marker" >/dev/null; then
    printf '%s\n' "$legacy_output" >&2
    echo "legacy KVM shutdown returned $legacy_status, expected 0" >&2
    exit 1
fi
echo 'legacy KVM shutdown payload ignored'

rm -f "$script_path"
run_exec 127 /mnt/host/missing.sh 'executable script not found'

echo 'KVM exec smoke test passed'