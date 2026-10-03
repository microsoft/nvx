/*
 * nvx-exit [CODE]: stop the VM with an 8-bit exit code, by writing one byte
 * to OpenVMM's control port 0x604 through /dev/port. CODE defaults to 1, and
 * any CODE that isn't a decimal number from 0 to 255 becomes 1. It prints
 * nothing; if the write fails, it exits with status 1.
 *
 * This is a single static exec rather than a shell script, because a guest's
 * first command after a restore pays a cold first touch for every fork, exec
 * and exit, and the script ran sh, a command substitution and a printf | dd
 * pipeline.
 */
#include <fcntl.h>
#include <stddef.h>
#include <unistd.h>

/* Tests build against a regular file; the guest always writes /dev/port. */
#ifndef NVX_EXIT_DEVICE
#define NVX_EXIT_DEVICE "/dev/port"
#endif
#define NVX_EXIT_PORT 0x604

static unsigned char exit_code(const char *argument)
{
    unsigned int value = 0;

    if (argument == NULL || *argument == '\0')
        return 1;
    for (; *argument != '\0'; argument++) {
        if (*argument < '0' || *argument > '9')
            return 1;
        value = value * 10 + (unsigned int)(*argument - '0');
        if (value > 255)
            return 1;
    }
    return (unsigned char)value;
}

int main(int argc, char **argv)
{
    unsigned char code = exit_code(argc > 1 ? argv[1] : NULL);
    int port = open(NVX_EXIT_DEVICE, O_WRONLY);

    if (port < 0)
        return 1;
    return pwrite(port, &code, 1, NVX_EXIT_PORT) == 1 ? 0 : 1;
}
