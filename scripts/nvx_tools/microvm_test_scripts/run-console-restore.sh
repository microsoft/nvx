set -eu
# The restored shell read this script from the terminal, through the boot
# console that the restore of `nvx-rs run` attached to it again.
echo NVX-RUN-CONSOLE-RESTORE-OK
nvx-exit 0
