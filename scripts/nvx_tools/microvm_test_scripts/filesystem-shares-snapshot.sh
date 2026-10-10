set -eu
fail() {
    code="$1"
    echo "NVX-FILESYSTEM-SHARES-SNAPSHOT-FAIL code=$code"
    nvx-exit "$code"
    exit "$code"
}

grep -q '^microvm /run/nvx/shares virtiofs rw' /proc/mounts || fail 80
grep -q '^microvm /workspace virtiofs rw' /proc/mounts || fail 81
grep -q '^microvm /opt/hostedtoolcache virtiofs ro' /proc/mounts || fail 82
grep -q '^microvm /results/output.txt virtiofs rw' /proc/mounts || fail 88
exec 3<>/workspace/journal
printf NVX-BEFORE >&3
# A handle on a file child survives the snapshot like one in a directory.
: >/results/output.txt
exec 4<>/results/output.txt
printf NVX-FILE-BEFORE >&4
echo NVX-FILESYSTEM-SHARES-BEFORE
nvx-snapshot
printf NVX-AFTER >&3
exec 3>&-
printf NVX-FILE-AFTER >&4
exec 4>&-
[ "$(cat /workspace/journal)" = NVX-BEFORENVX-AFTER ] || fail 83
[ "$(cat /opt/hostedtoolcache/seed)" = NVX-TOOLCACHE ] || fail 84
if touch /opt/hostedtoolcache/mutation 2>/dev/null; then
    fail 85
fi
if touch /run/nvx/shares/1/mutation 2>/dev/null; then
    fail 86
fi
[ "$(ls /srv/data | tr '\n' '|')" = "notes one.txt|public|" ] || fail 87
[ "$(cat /results/output.txt)" = NVX-FILE-BEFORENVX-FILE-AFTER ] || fail 89
[ "$(cat /config/settings.json)" = NVX-SETTINGS ] || fail 90
if (printf 'append\n' >>/config/settings.json) 2>/dev/null; then
    fail 91
fi
echo NVX-FILESYSTEM-SHARES-AFTER
nvx-exit 0
