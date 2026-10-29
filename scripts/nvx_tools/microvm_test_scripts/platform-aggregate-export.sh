set -eu
on_exit() {
    status=$?
    if [ "$status" -ne 0 ]; then
        nvx-exit "$status"
    fi
}
trap on_exit EXIT
fail() {
    echo "NVX-PLATFORM-AGGREGATE-FAIL $1"
    exit "$2"
}
expect_error() {
    case "$1" in
        *"$2"*) ;;
        *) fail "$3: $1" "$4" ;;
    esac
}
# The restore attached an aggregate to the slot that the platform capture left
# dormant. The guest keeps its captured command line, which discovers the slot
# but names no filesystem, so the guest mounts the aggregate itself.
grep -q 'virtio_mmio.device=0x1000@0xd0001000:6' /proc/cmdline || fail "discovery" 80
if grep -Eq '(^| )(virtfs_|nvx_share=)' /proc/cmdline; then
    fail "command line" 81
fi
shares=/run/nvx/shares
mkdir -p "$shares"
mount -t virtiofs microvm "$shares" || fail "mount" 82
[ "$(ls "$shares" | tr '\n' ' ')" = "0 1 2 3 4 " ] || fail "children" 83
[ "$(stat -c %a "$shares")" = 500 ] || fail "root mode" 84

# The read-write workspace hides its denied path and accepts writes.
[ "$(cat "$shares/0/seed")" = NVX-WORKSPACE ] || fail "workspace read" 85
[ ! -e "$shares/0/secrets" ] || fail "workspace denied path" 86
printf 'NVX-GUEST-WRITE\n' >"$shares/0/from-guest" || fail "workspace write" 87
mkdir "$shares/0/guest-directory" || fail "workspace directory" 88

# The read-only tool cache hides its denied path and rejects writes.
[ "$(cat "$shares/1/tools/node")" = NVX-TOOL ] || fail "tool cache read" 89
[ ! -e "$shares/1/credentials" ] || fail "tool cache denied path" 90
if error=$(touch "$shares/1/mutation" 2>&1); then
    fail "tool cache write" 91
fi
expect_error "$error" 'Read-only file system' "tool cache write" 92

# The data directory's root exposes only its allowed paths, and only the notes
# are writable.
[ "$(ls "$shares/2" | tr '\n' '|')" = "notes one.txt|public|" ] || fail "data listing" 93
[ ! -e "$shares/2/private.txt" ] || fail "data hidden path" 94
[ "$(cat "$shares/2/public/readme")" = NVX-PUBLIC ] || fail "data allowed path" 95
printf 'NVX-GUEST-NOTE\n' >>"$shares/2/notes one.txt" || fail "data writable path" 96
if error=$(touch "$shares/2/public/mutation" 2>&1); then
    fail "data read-only path" 97
fi
expect_error "$error" 'Read-only file system' "data read-only path" 98
if error=$(touch "$shares/2/added" 2>&1); then
    fail "data hidden root" 99
fi
expect_error "$error" 'Permission denied' "data hidden root" 100

# Each file child is the file alone, with its own mode.
[ -f "$shares/3" ] && [ -f "$shares/4" ] || fail "file children" 101
[ "$(cat "$shares/3")" = NVX-SETTINGS ] || fail "settings read" 102
if error=$(ls "$shares/3/secret.json" 2>&1); then
    fail "settings directory" 103
fi
expect_error "$error" 'Not a directory' "settings directory" 104
if error=$( (printf 'append\n' >>"$shares/3") 2>&1); then
    fail "settings write" 105
fi
expect_error "$error" 'Read-only file system' "settings write" 106
printf 'NVX-GUEST-OUTPUT\n' >"$shares/4" || fail "output write" 107
if error=$(rm -f "$shares/4" 2>&1); then
    fail "output removal" 108
fi
expect_error "$error" 'Read-only file system' "output removal" 109

# The guest binds the children where it needs them, a file child at a file.
mkdir -p /workspace /config
mount --bind "$shares/0" /workspace || fail "workspace bind" 110
[ "$(cat /workspace/from-guest)" = NVX-GUEST-WRITE ] || fail "workspace bind read" 111
: >/config/settings.json
mount --bind "$shares/3" /config/settings.json || fail "settings bind" 112
[ "$(cat /config/settings.json)" = NVX-SETTINGS ] || fail "settings bind read" 113
echo NVX-PLATFORM-AGGREGATE-OK
