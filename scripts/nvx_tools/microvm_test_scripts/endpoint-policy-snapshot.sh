set -eu
fail() {
    code="$1"
    echo "NVX-ENDPOINT-POLICY-FAIL code=$code"
    nvx-exit "$code"
    exit "$code"
}

check_neighbors() {
    arp -d 10.0.0.9 2>/dev/null || true
    arp -d 10.0.0.1 2>/dev/null || true
    arp -d 10.0.0.10 2>/dev/null || true
    ping -c 1 -W 1 10.0.0.9 >/dev/null 2>&1 || true
    arp -n 10.0.0.9 | grep -Eqi '10\.0\.0\.9.*52:54:00:00:00:01' || fail 20
    ping -c 1 -W 1 192.0.2.7 >/dev/null 2>&1 || true
    arp -n 10.0.0.1 | grep -Eqi '10\.0\.0\.1.*52:54:00:00:00:01' || fail 21
    ping -c 1 -W 1 10.0.0.10 >/dev/null 2>&1 || true
    if arp -n 10.0.0.10 2>/dev/null | grep -qi '52:54:00:00:00:01'; then
        fail 22
    fi
}

check_neighbors
echo NVX-ENDPOINT-POLICY-BEFORE
nvx-snapshot
check_neighbors
echo NVX-ENDPOINT-POLICY-AFTER
nvx-exit 0
