set -eu
fail() {
    code="$1"
    echo "NVX-FILESYSTEM-SHARES-FAIL code=$code"
    nvx-exit "$code"
    exit "$code"
}

expect_read_only() {
    case "$1" in
        *'Read-only file system'*) ;;
        *)
            echo "NVX-FILESYSTEM-SHARES-ERROR $1"
            fail "$2"
            ;;
    esac
}

# One virtio-fs slot carries every share as a child of one aggregate, which
# the guest mounts where only root can enter and binds at each target.
grep -q 'virtio_mmio.device=0x1000@0xd0001000:6' /proc/cmdline || fail 40
if grep -q '0xd0008000' /proc/cmdline; then
    fail 41
fi
grep -q 'virtfs_dir=/run/nvx/shares virtfs_tag=microvm virtfs_mode=rw virtfs_aggregate=1' /proc/cmdline || fail 42
grep -q '^microvm /run/nvx/shares virtiofs rw' /proc/mounts || fail 43
grep -q '^microvm /workspace virtiofs rw' /proc/mounts || fail 44
grep -q '^microvm /opt/hostedtoolcache virtiofs ro' /proc/mounts || fail 45
grep -q '^microvm /srv/data virtiofs rw' /proc/mounts || fail 46
[ "$(ls /run/nvx/shares | tr '\n' ' ')" = "0 1 2 " ] || fail 47
[ "$(stat -c %a /run/nvx/shares)" = 500 ] || fail 48
[ "$(cat /workspace/seed)" = NVX-WORKSPACE ] || fail 49
[ "$(cat /opt/hostedtoolcache/seed)" = NVX-TOOLCACHE ] || fail 50
[ "$(cat /opt/hostedtoolcache/tools/node)" = NVX-TOOL ] || fail 51

# Each share hides only its own denied paths, and the data share's root
# exposes only its allowed paths, one of them with a space in its name.
[ ! -e /workspace/secrets ] || fail 52
[ ! -e /opt/hostedtoolcache/credentials ] || fail 53
[ "$(ls /srv/data | tr '\n' '|')" = "notes one.txt|public|" ] || fail 54
[ ! -e /srv/data/private.txt ] || fail 55
[ "$(cat /srv/data/public/readme)" = NVX-PUBLIC ] || fail 56
[ "$(cat '/srv/data/notes one.txt')" = NVX-NOTES ] || fail 57

# The read-write share accepts writes, and the read-only share rejects them,
# also through a link in the read-write share.
printf 'NVX-GUEST-WRITE\n' >/workspace/from-guest || fail 58
mkdir /workspace/guest-directory || fail 59
if touch /opt/hostedtoolcache/mutation 2>/dev/null; then
    fail 60
fi
ln -s /opt/hostedtoolcache/seed /workspace/toolcache-seed || fail 61
if (printf 'overwrite\n' >/workspace/toolcache-seed) 2>/dev/null; then
    fail 62
fi

# Within the read-write data share, OpenVMM permits writes only to the notes.
printf 'NVX-GUEST-NOTE\n' >>'/srv/data/notes one.txt' || fail 63
if error=$(touch /srv/data/public/mutation 2>&1); then
    fail 64
fi
expect_read_only "$error" 65
# The data share's root refuses to look up any name that it does not expose,
# so a new name cannot be created there either.
if error=$(touch /srv/data/added 2>&1); then
    fail 66
fi
case "$error" in
    *'Permission denied'*) ;;
    *)
        echo "NVX-FILESYSTEM-SHARES-ERROR $error"
        fail 67
        ;;
esac

# OpenVMM, not the guest mount flags, enforces each share's mode: the
# aggregate is mounted read-write, and through it the guest's root still
# cannot change the read-only share or reach hidden paths.
[ "$(cat /run/nvx/shares/1/seed)" = NVX-TOOLCACHE ] || fail 68
if error=$(touch /run/nvx/shares/1/mutation 2>&1); then
    fail 69
fi
expect_read_only "$error" 70
if error=$(mkdir /run/nvx/shares/1/directory 2>&1); then
    fail 71
fi
expect_read_only "$error" 72
if error=$( (printf 'append\n' >>/run/nvx/shares/1/seed) 2>&1); then
    fail 73
fi
expect_read_only "$error" 74
if error=$(rm -f /run/nvx/shares/1/seed 2>&1); then
    fail 75
fi
expect_read_only "$error" 76
if error=$(chmod 0777 /run/nvx/shares/1/seed 2>&1); then
    fail 77
fi
expect_read_only "$error" 78
[ ! -e /run/nvx/shares/0/secrets ] || fail 79
[ ! -e /run/nvx/shares/2/private.txt ] || fail 80
# Hard links cannot join children: like Linux's linkat, OpenVMM refuses a link
# into a read-only directory first, and one into a writable directory of
# another child crosses devices. The aggregate's root cannot change either.
if error=$(ln /run/nvx/shares/0/seed /run/nvx/shares/2/public/linked 2>&1); then
    fail 81
fi
expect_read_only "$error" 82
if error=$(ln /run/nvx/shares/1/seed /run/nvx/shares/0/linked 2>&1); then
    fail 83
fi
case "$error" in
    *'ross-device link'*) ;;
    *)
        echo "NVX-FILESYSTEM-SHARES-ERROR $error"
        fail 84
        ;;
esac
[ ! -e /workspace/linked ] || fail 85
[ ! -e /srv/data/public/linked ] || fail 86
if mkdir /run/nvx/shares/3 2>/dev/null; then
    fail 87
fi
[ "$(cat /opt/hostedtoolcache/seed)" = NVX-TOOLCACHE ] || fail 88

echo NVX-FILESYSTEM-SHARES-OK
nvx-exit 0
