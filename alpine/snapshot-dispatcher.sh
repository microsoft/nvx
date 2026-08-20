#!/bin/sh

if [ "$1" = "--pause" ]; then
    shift
    kill -STOP "$$"
fi

if [ "$#" -ne 1 ]; then
    echo "usage: snapshot-dispatcher.sh [--pause] GUEST_SCRIPT" >&2
    exit 2
fi

if ! /sbin/nvx-hostmount; then
    exit 126
fi
printf 'NVX-EXEC-START'
if [ ! -f "$1" ]; then
    echo "nvx: executable script not found: $1" >&2
    exit 127
fi
exec /bin/sh "$1"
