#!/usr/bin/env python3
# A minimal Python "hello world" that demonstrates snapshot/restore.
#
# By the time this code runs, the Python interpreter is fully started and warmed. It then
# asks the VMM to take a snapshot by writing one byte to I/O port 0x605 through /dev/port
# (a single `outb`, which the VMM intercepts as a snapshot request). On a snapshot run the
# VMM captures the VM at exactly this point and stops; on a later restore, execution resumes
# on the next line and prints the greeting immediately -- skipping kernel boot and the entire
# Python startup.
import os

try:
    fd = os.open("/dev/port", os.O_WRONLY)
    os.lseek(fd, 0x605, os.SEEK_SET)
    os.write(fd, b"\x01")
    os.close(fd)
except OSError:
    pass

print("HELLO-WORLD-FROM-PYTHON", flush=True)
