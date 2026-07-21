#define _GNU_SOURCE

#include <errno.h>
#include <linux/vm_sockets.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static void retry_delay(void)
{
    const struct timespec delay = { .tv_sec = 0, .tv_nsec = 50 * 1000 * 1000 };
    nanosleep(&delay, NULL);
}

static void mount_timeout(int signal_number)
{
    (void)signal_number;
}

int main(int argc, char **argv)
{
    if (argc != 4 || (strcmp(argv[2], "ro") != 0 && strcmp(argv[2], "rw") != 0)) {
        fprintf(stderr, "usage: hcs-plan9 TARGET ro|rw ANAME\n");
        return 2;
    }

    int socket_fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (socket_fd < 0) {
        perror("hcs-plan9: socket(AF_VSOCK)");
        return 1;
    }

    const struct sockaddr_vm address = {
        .svm_family = AF_VSOCK,
        .svm_port = 564,
        .svm_cid = VMADDR_CID_HOST,
    };
    int connected = 0;
    for (int attempt = 0; attempt < 100; attempt++) {
        if (connect(socket_fd, (const struct sockaddr *)&address, sizeof(address)) == 0) {
            connected = 1;
            break;
        }
        if (errno != ENODEV && errno != ECONNREFUSED && errno != ETIMEDOUT && errno != EAGAIN) {
            break;
        }
        retry_delay();
    }
    if (!connected) {
        perror("hcs-plan9: connect host port 564");
        close(socket_fd);
        return 1;
    }

    char options[256];
    int length = snprintf(
        options,
        sizeof(options),
        "trans=fd,rfdno=%d,wfdno=%d,msize=65536,aname=%s",
        socket_fd,
        socket_fd,
        argv[3]
    );
    if (length < 0 || (size_t)length >= sizeof(options)) {
        fprintf(stderr, "hcs-plan9: mount options are too long\n");
        close(socket_fd);
        return 1;
    }

    unsigned long flags = MS_NODEV;
    if (strcmp(argv[2], "ro") == 0) {
        flags |= MS_RDONLY;
    }
    struct sigaction action = { 0 };
    action.sa_handler = mount_timeout;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGALRM, &action, NULL) != 0) {
        perror("hcs-plan9: sigaction");
        close(socket_fd);
        return 1;
    }
    alarm(15);
    int mount_result = mount(argv[3], argv[1], "9p", flags, options);
    int mount_errno = errno;
    alarm(0);
    if (mount_result != 0) {
        errno = mount_errno;
        perror("hcs-plan9: mount");
        close(socket_fd);
        return 1;
    }
    close(socket_fd);
    return 0;
}