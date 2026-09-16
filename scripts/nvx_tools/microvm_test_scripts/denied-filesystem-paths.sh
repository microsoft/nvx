set -eu
fail() {
    code="$1"
    echo "NVX-DENIED-PATHS-FAIL code=$code"
    nvx-exit "$code"
    exit "$code"
}

[ "$(cat /mnt/share/allowed/seed)" = NVX-ALLOWED ] || fail 100
if ls -A /mnt/share | grep -qx secrets; then
    fail 101
fi
if cat /mnt/share/secrets/token >/dev/null 2>&1; then
    fail 102
fi
if sh -c 'cd /mnt/share/allowed && cat ../secrets/token' >/dev/null 2>&1; then
    fail 103
fi
if cat /mnt/share/alias/token >/dev/null 2>&1; then
    fail 105
fi
mkdir -p /mnt/second
mount -t virtiofs microvm /mnt/second || fail 106
if cat /mnt/second/secrets/token >/dev/null 2>&1; then
    fail 107
fi
printf 'NVX-GUEST-WRITE\n' >/mnt/share/allowed/from-guest || fail 108

echo NVX-DENIED-PATHS-OK
nvx-exit 0
