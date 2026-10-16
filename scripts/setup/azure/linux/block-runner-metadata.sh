#!/bin/bash
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: the metadata firewall must be installed as root" >&2
    exit 1
fi
runner_uid=$(id -u nvx-runner)
if [ "$runner_uid" -eq 0 ] || ! command -v iptables >/dev/null 2>&1; then
    echo "ERROR: the runner account or IPv4 firewall is unavailable" >&2
    exit 1
fi

rule=(OUTPUT -d 169.254.169.254/32 -m owner --uid-owner "$runner_uid" -j REJECT)
while iptables -w 10 -C "${rule[@]}" >/dev/null 2>&1; do
    iptables -w 10 -D "${rule[@]}"
done
iptables -w 10 -I OUTPUT 1 "${rule[@]:1}"
iptables -w 10 -C "${rule[@]}" >/dev/null || {
    echo "ERROR: the runner account is not blocked from Azure instance metadata" >&2
    exit 1
}
