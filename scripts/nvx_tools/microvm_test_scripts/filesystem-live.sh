set -eu
fail() {
    code="$1"
    echo "NVX-FILESYSTEM-LIVE-FAIL code=$code"
    nvx-exit "$code"
    exit "$code"
}

# Swaps two names with renameat2(RENAME_EXCHANGE): util-linux's exch where the
# guest has it, and otherwise the x86-64 system call through Perl.
exchange() {
    if command -v exch >/dev/null 2>&1; then
        exch "$1" "$2"
    else
        perl -e 'syscall(316, -100, $ARGV[0], -100, $ARGV[1], 2) == 0 or die "renameat2: $!\n"' "$1" "$2"
    fi
}

grep -q ' /mnt/share virtiofs ' /proc/mounts || fail 30
grep -q 'virtfs_dir=/mnt/share' /proc/cmdline || fail 31
grep -q 'virtfs_mode=rw' /proc/cmdline || fail 32
exec 3<>/mnt/share/open-handle
printf NVX-HANDLE-BEFORE >&3
# A link inode in the captured inode table must survive save and restore.
ln -s open-handle /mnt/share/handle-link || fail 34
[ "$(readlink /mnt/share/handle-link)" = open-handle ] || fail 35
# Swap two files and two directories with RENAME_EXCHANGE right before the
# capture, so that it records the names that OpenVMM keeps for each object
# before any lookup refreshes them. The guest holds every object open: with
# zero entry lifetimes, it forgets an object that nothing holds, and OpenVMM
# then drops the object's node. A Windows host rejects the exchange without
# changing either name; the host checks the outcome for its platform.
mkdir /mnt/share/exchange-dir-a /mnt/share/exchange-dir-b
printf NVX-EXCHANGE-DIR-A >/mnt/share/exchange-dir-a/entry
printf NVX-EXCHANGE-DIR-B >/mnt/share/exchange-dir-b/entry
printf NVX-EXCHANGE-A >/mnt/share/exchange-a
printf NVX-EXCHANGE-B >/mnt/share/exchange-b
exec 4</mnt/share/exchange-a 5</mnt/share/exchange-b
exec 6</mnt/share/exchange-dir-a 7</mnt/share/exchange-dir-b
exec 8</mnt/share/exchange-dir-a/entry 9</mnt/share/exchange-dir-b/entry
if exchange /mnt/share/exchange-a /mnt/share/exchange-b; then
    exchange /mnt/share/exchange-dir-a /mnt/share/exchange-dir-b || fail 38
    at_a=B at_b=A
elif exchange /mnt/share/exchange-dir-a /mnt/share/exchange-dir-b; then
    fail 39
else
    at_a=A at_b=B
fi
# A process dirties a page of a host file through a shared mapping, which it
# holds until its input ends. Nothing else opens the file before the capture,
# which writes the page back, so the host reads the guest's write and no
# restore writes the page over the host's later changes. The sleep holds the
# mapping process's input open until it is killed.
mkfifo /tmp/nvx-mapping-input /tmp/nvx-mapping-output
sleep 3600 >/tmp/nvx-mapping-input &
mapping_input=$!
nvx-device-io map /mnt/share/mapped-page NVX-GUEST-MAPPED \
    </tmp/nvx-mapping-input >/tmp/nvx-mapping-output &
mapping=$!
IFS= read -r mapped </tmp/nvx-mapping-output || fail 48
case "$mapped" in
    *'"operation":"map"'*) ;;
    *) fail 49 ;;
esac
echo NVX-FILESYSTEM-LIVE-BEFORE
nvx-snapshot
printf NVX-HANDLE-AFTER >&3
exec 3>&-
[ "$(cat /mnt/share/open-handle)" = NVX-HANDLE-BEFORENVX-HANDLE-AFTER ] || fail 33
[ "$(readlink /mnt/share/handle-link)" = open-handle ] || fail 36
[ "$(cat /mnt/share/handle-link)" = NVX-HANDLE-BEFORENVX-HANDLE-AFTER ] || fail 37
# Each handle still reads the object that it opened, whatever its name now.
[ "$(cat <&4)" = NVX-EXCHANGE-A ] || fail 40
[ "$(cat <&5)" = NVX-EXCHANGE-B ] || fail 41
[ "$(cat <&8)" = NVX-EXCHANGE-DIR-A ] || fail 42
[ "$(cat <&9)" = NVX-EXCHANGE-DIR-B ] || fail 43
exec 4<&- 5<&- 6<&- 7<&- 8<&- 9<&-
[ "$(cat /mnt/share/exchange-a)" = "NVX-EXCHANGE-$at_a" ] || fail 44
[ "$(cat /mnt/share/exchange-b)" = "NVX-EXCHANGE-$at_b" ] || fail 45
[ "$(cat /mnt/share/exchange-dir-a/entry)" = "NVX-EXCHANGE-DIR-$at_a" ] || fail 46
[ "$(cat /mnt/share/exchange-dir-b/entry)" = "NVX-EXCHANGE-DIR-$at_b" ] || fail 47
# The restore holds the mapped page clean, so unmapping it writes nothing, and
# the file keeps what the host wrote after the capture.
kill "$mapping_input"
wait "$mapping" || fail 50
wait "$mapping_input" || :
[ "$(cat /mnt/share/mapped-page)" != NVX-GUEST-MAPPED ] || fail 51
echo NVX-FILESYSTEM-LIVE-AFTER
nvx-exit 0
