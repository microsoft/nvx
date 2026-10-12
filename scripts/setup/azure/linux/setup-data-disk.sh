#!/bin/bash
set -euo pipefail

LOG=/var/log/setup-datadisk.log
MOUNTPOINT=${NVX_DATA_DISK_MOUNT_POINT:-/mnt/data}
LUN=${NVX_DATA_DISK_LUN:-0}
[[ "$LUN" =~ ^[0-9]+$ ]] && [ "$LUN" -le 63 ] || {
    echo "ERROR: data disk LUN must be an integer from 0 to 63" >&2
    exit 1
}
case "$MOUNTPOINT" in
    /mnt/* | /srv/*) ;;
    *)
        echo "ERROR: data disk mount point must be below /mnt or /srv" >&2
        exit 1
        ;;
esac
[[ "$MOUNTPOINT" =~ ^/[A-Za-z0-9_./-]+$ ]] &&
    [[ "/$MOUNTPOINT/" != *"/../"* ]] && [[ "/$MOUNTPOINT/" != *"/./"* ]] &&
    [[ "$MOUNTPOINT" != */ ]] && [[ "$MOUNTPOINT" != *"//"* ]] || {
    echo "ERROR: invalid data disk mount point" >&2
    exit 1
}
INIT_PART_TYPE=8f8f0f40-5c84-4a2a-8c22-8fbff2e8bc91
DATA_PART_TYPE=0fc63daf-8483-4772-8e79-3d69d8477de4
INIT_TABLE_ID=6f746dc1-31d4-4b7c-b2ee-9cf6075517a1
exec > >(tee -a "$LOG") 2>&1
echo "=== setup-datadisk.sh run at $(date --iso-8601=seconds) ==="

# Azure exposes managed data disks differently on SCSI and NVMe/Boost
# controllers. Both paths identify the managed disk by Azure LUN.
DISK=""
for _ in $(seq 1 30); do
    for candidate in "/dev/disk/azure/data/by-lun/$LUN" "/dev/disk/azure/scsi1/lun$LUN"; do
        if [ -e "$candidate" ]; then
            DISK="$candidate"
            break 2
        fi
    done
    sleep 1
done
if [ -z "$DISK" ]; then
    echo "ERROR: managed data disk LUN $LUN was not found after 30 seconds" >&2
    exit 1
fi

DISKDEV=$(readlink -f "$DISK")
if [ -z "$DISKDEV" ] || [ ! -b "$DISKDEV" ]; then
    echo "ERROR: $DISK did not resolve to a block device" >&2
    exit 1
fi
echo "Using managed data disk LUN $LUN: $DISK -> $DISKDEV"

list_partitions() {
    lsblk -lnpo NAME,TYPE "$DISKDEV" 2>/dev/null |
        awk '$2 == "part" { print $1 }'
}

filesystem_type() {
    blkid -p -s TYPE -o value "$1" 2>/dev/null || true
}

partition_type() {
    lsblk -dno PARTTYPE "$1" 2>/dev/null | tr '[:upper:]' '[:lower:]' | tr -d '[:space:]'
}

set_partition_type() {
    local part_number part_name
    part_name=$(basename "$1")
    part_number=$(cat "/sys/class/block/$part_name/partition" 2>/dev/null | tr -d '[:space:]') || true
    if [ -z "$part_number" ]; then
        echo "ERROR: could not determine the partition number for $1" >&2
        exit 1
    fi
    sfdisk --part-type "$DISKDEV" "$part_number" "$2"
    udevadm settle
}

has_signatures() {
    local output rc=0
    output=$(wipefs -n "$1" 2>/dev/null) || rc=$?
    if [ "$rc" -ne 0 ]; then
        echo "ERROR: signature probe of $1 failed (rc=$rc); refusing to modify it" >&2
        exit 1
    fi
    [ -n "${output//[[:space:]]/}" ]
}

verify_new_filesystem() {
    local rc=0
    sync
    e2fsck -fn "$1" >/var/log/setup-datadisk-e2fsck.log 2>&1 || rc=$?
    if [ "$rc" -ne 0 ]; then
        echo "ERROR: validation of newly created filesystem $1 failed (rc=$rc); see /var/log/setup-datadisk-e2fsck.log" >&2
        exit 1
    fi
}

WHOLE_FSTYPE=$(filesystem_type "$DISKDEV")
mapfile -t PARTITIONS < <(list_partitions)

if [ -n "$WHOLE_FSTYPE" ] && [ "${#PARTITIONS[@]}" -gt 0 ]; then
    echo "ERROR: $DISKDEV reports both a whole-disk filesystem and partitions; refusing to guess" >&2
    exit 1
fi

TARGETDEV=""
if [ -n "$WHOLE_FSTYPE" ]; then
    TARGETDEV="$DISKDEV"
elif [ "${#PARTITIONS[@]}" -eq 0 ]; then
    # Only a truly empty disk reaches this path. Refuse to overwrite any
    # unknown signatures even if lsblk did not recognize a filesystem.
    if has_signatures "$DISKDEV"; then
        dump=$(sfdisk --dump "$DISKDEV" 2>/dev/null || true)
        table_id=$(printf '%s\n' "$dump" | awk -F: '$1 == "label-id" { gsub(/[[:space:]]/, "", $2); print tolower($2) }')
        entries=$(printf '%s\n' "$dump" | awk '/^[^#].* : / { count++ } END { print count+0 }')
        if [ "$table_id" != "$INIT_TABLE_ID" ] || [ "$entries" -ne 0 ]; then
            echo "ERROR: $DISKDEV contains signatures not belonging to an interrupted AzTemplates initialization; refusing to repartition it" >&2
            exit 1
        fi
        echo "Recovering an interrupted empty GPT-table creation on $DISKDEV"
    fi

    echo "Atomically creating a GPT partition on blank disk $DISKDEV"
    printf 'label: gpt\nlabel-id: %s\n,,%s\n' "$INIT_TABLE_ID" "$INIT_PART_TYPE" | sfdisk "$DISKDEV"
    udevadm settle

    for _ in $(seq 1 30); do
        mapfile -t PARTITIONS < <(list_partitions)
        if [ "${#PARTITIONS[@]}" -eq 1 ] && [ -b "${PARTITIONS[0]}" ]; then
            break
        fi
        sleep 1
    done
    if [ "${#PARTITIONS[@]}" -ne 1 ] || [ ! -b "${PARTITIONS[0]}" ]; then
        echo "ERROR: exactly one partition did not appear on $DISKDEV after 30 seconds" >&2
        exit 1
    fi

    TARGETDEV="${PARTITIONS[0]}"
    if has_signatures "$TARGETDEV"; then
        echo "ERROR: new partition $TARGETDEV unexpectedly contains a signature; refusing to format it" >&2
        exit 1
    fi
    mkfs.ext4 "$TARGETDEV"
    verify_new_filesystem "$TARGETDEV"
    set_partition_type "$TARGETDEV" "$DATA_PART_TYPE"
elif [ "${#PARTITIONS[@]}" -eq 1 ]; then
    TARGETDEV="${PARTITIONS[0]}"
    PART_FSTYPE=$(filesystem_type "$TARGETDEV")
    PART_TYPE=$(partition_type "$TARGETDEV")
    if [ -z "$PART_FSTYPE" ]; then
        # This is the recoverable state left by a crash after mkpart and
        # before mkfs. Never format if any unknown signature is present.
        if [ "$PART_TYPE" != "$INIT_PART_TYPE" ]; then
            echo "ERROR: blank partition $TARGETDEV lacks the AzTemplates initialization marker; refusing to format it" >&2
            exit 1
        fi
        if has_signatures "$TARGETDEV"; then
            echo "ERROR: unformatted partition $TARGETDEV contains a signature; refusing to overwrite it" >&2
            exit 1
        fi
        echo "Completing interrupted initialization by formatting the sole blank partition $TARGETDEV"
        mkfs.ext4 "$TARGETDEV"
        verify_new_filesystem "$TARGETDEV"
        set_partition_type "$TARGETDEV" "$DATA_PART_TYPE"
    elif [ "$PART_TYPE" = "$INIT_PART_TYPE" ]; then
        # A crash during or immediately after mkfs can leave an ext4
        # signature while the persistent in-progress marker remains. The
        # partition has never been exposed at /mnt/data in this state, so
        # finishing initialization cannot overwrite user data.
        echo "Recreating the incomplete marked filesystem on $TARGETDEV"
        mkfs.ext4 -F "$TARGETDEV"
        verify_new_filesystem "$TARGETDEV"
        set_partition_type "$TARGETDEV" "$DATA_PART_TYPE"
    fi
else
    echo "ERROR: $DISKDEV has ${#PARTITIONS[@]} partitions; refusing to select or modify one" >&2
    exit 1
fi

FSTYPE=$(filesystem_type "$TARGETDEV")
if [ "$FSTYPE" != "ext4" ]; then
    echo "ERROR: $TARGETDEV contains '$FSTYPE', not ext4; preserving it but refusing to mount it as ext4" >&2
    exit 1
fi

if ! mountpoint -q "$MOUNTPOINT"; then
    if ! findmnt -rn -S "$TARGETDEV" >/dev/null 2>&1; then
        rc=0
        e2fsck -n "$TARGETDEV" >/var/log/setup-datadisk-e2fsck.log 2>&1 || rc=$?
        if [ "$rc" -ne 0 ]; then
            # e2fsck status 1 means errors were found and would be corrected;
            # any nonzero status is operator-visible, but bootstrap never
            # repairs or reformats an existing filesystem automatically.
            echo "WARNING: e2fsck -n returned $rc for $TARGETDEV; see /var/log/setup-datadisk-e2fsck.log" >&2
        fi
    fi
fi

PARTUUID=$(blkid -s PARTUUID -o value "$TARGETDEV" || true)
UUID=$(blkid -s UUID -o value "$TARGETDEV" || true)
if [ -n "$PARTUUID" ]; then
    FS_SPEC="PARTUUID=$PARTUUID"
elif [ -n "$UUID" ]; then
    FS_SPEC="UUID=$UUID"
else
    echo "ERROR: could not read PARTUUID or UUID for $TARGETDEV" >&2
    exit 1
fi

mkdir -p "$MOUNTPOINT"
FSTAB_LINE="$FS_SPEC $MOUNTPOINT ext4 defaults,nofail,x-systemd.device-timeout=30s 1 2"
touch /etc/fstab

if ! awk -v mp="$MOUNTPOINT" -v expected="$FSTAB_LINE" '
  $1 !~ /^#/ && $2 == mp { count++; if ($0 == expected) exact++ }
  END { exit !(count == 1 && exact == 1) }
' /etc/fstab; then
    tmp=$(mktemp /etc/fstab.setup-datadisk.XXXXXX)
    awk -v mp="$MOUNTPOINT" '$1 ~ /^#/ || $2 != mp' /etc/fstab >"$tmp"
    printf '%s\n' "$FSTAB_LINE" >>"$tmp"
    chown root:root "$tmp"
    chmod 0644 "$tmp"
    mv -f "$tmp" /etc/fstab
    command -v restorecon >/dev/null 2>&1 && restorecon /etc/fstab || true
    echo "Set fstab entry: $FSTAB_LINE"
else
    echo "fstab already contains exactly the expected $MOUNTPOINT entry"
fi

systemctl daemon-reload
if mountpoint -q "$MOUNTPOINT"; then
    MOUNTED_DEVICE=$(findmnt -rn -o MAJ:MIN --target "$MOUNTPOINT" || true)
    TARGET_DEVICE=$(lsblk -dno MAJ:MIN "$TARGETDEV" 2>/dev/null | tr -d '[:space:]')
    if [ -z "$MOUNTED_DEVICE" ] || [ "$MOUNTED_DEVICE" != "$TARGET_DEVICE" ]; then
        echo "ERROR: $MOUNTPOINT is not mounted from $TARGETDEV" >&2
        exit 1
    fi
else
    mount "$MOUNTPOINT" || mountpoint -q "$MOUNTPOINT"
fi

if ! mountpoint -q "$MOUNTPOINT"; then
    echo "ERROR: $MOUNTPOINT is not mounted after setup" >&2
    exit 1
fi
echo "=== setup-datadisk.sh finished OK ==="
