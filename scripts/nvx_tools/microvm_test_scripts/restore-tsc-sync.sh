set -eu
cpuinfo="$(cat /proc/cpuinfo)"
case "$cpuinfo" in
    *tsc_adjust*)
        echo "NVX-RESTORE-PROCESSORS-FAIL tsc-adjust-still-enabled"
        exit 96
        ;;
esac
echo "NVX-RESTORE-TSC-SYNC-CHECK-ENABLED"
