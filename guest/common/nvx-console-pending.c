#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

int main(int argc, char **argv)
{
    int descriptor;
    int pending;

    if (argc != 2) {
        fprintf(stderr, "usage: nvx-console-pending DEVICE\n");
        return 2;
    }

    descriptor = open(argv[1], O_RDONLY | O_CLOEXEC | O_NOCTTY | O_NONBLOCK);
    if (descriptor < 0) {
        fprintf(stderr, "nvx-console-pending: open %s: %s\n", argv[1],
                strerror(errno));
        return 1;
    }
    if (ioctl(descriptor, FIONREAD, &pending) != 0) {
        fprintf(stderr, "nvx-console-pending: ioctl %s: %s\n", argv[1],
                strerror(errno));
        close(descriptor);
        return 1;
    }
    if (close(descriptor) != 0) {
        fprintf(stderr, "nvx-console-pending: close %s: %s\n", argv[1],
                strerror(errno));
        return 1;
    }
    if (printf("%d\n", pending) < 0) {
        fprintf(stderr, "nvx-console-pending: write: %s\n", strerror(errno));
        return 1;
    }
    return 0;
}