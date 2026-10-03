#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

#define OUTER_HEADER_LEN 44U
#define OUTER_MAX_PAYLOAD 65536U
#define OUTER_VERSION 1U
#define OUTER_GUEST_ATTACH 1U
#define OUTER_RESET 3U
#define OUTER_ACK 4U
#define OUTER_DATA 5U
#define OUTER_CREDIT 9U
#define OUTER_RECEIVE_WINDOW (1024U * 1024U)

#define APP_HEADER_LEN 24U
#define APP_VERSION 1U
#define APP_PING 1U
#define APP_EXEC 2U
#define APP_STOP 3U
#define APP_CANCEL 4U
#define APP_FEATURES 5U
#define APP_READY 0x81U
#define APP_STDOUT 0x82U
#define APP_STDERR 0x83U
#define APP_EXIT 0x84U
#define APP_STOPPED 0x85U
#define APP_ERROR 0xffU

/*
 * Control features that a host can rely on, advertised in the answer to
 * FEATURES. The host refuses an image that lacks a feature it needs, so a
 * behavior that a host depends on needs a bit here and in the host's copy
 * (aci_edge_sandboxes, src/openvmm/protocol.rs). The init script of the image provides
 * FEATURE_WORKLOAD_ACCOUNT; the agent speaks for it because both ship in one
 * initramfs. An agent that predates FEATURES refuses the request as an
 * unsupported operation, which the host reads as no features.
 */
#define FEATURE_CANCEL (1U << 0)
#define FEATURE_HOST_MAPPINGS (1U << 1)
#define FEATURE_WORKLOAD_ACCOUNT (1U << 2)
#define FEATURE_EXEC_CGROUP (1U << 3)

#define MAX_ARGUMENTS 64U
#define MAX_ARGUMENT_LEN 4096U
#define MAX_ENVIRONMENT 256U
#define EXEC_EXTENDED 1U
#define EXEC_CWD_PRESENT 1U
#define EXEC_ENVIRONMENT_PRESENT 2U
#define MAX_OUTPUT_BYTES (1024U * 1024U)
#define OUTPUT_CHUNK_BYTES 32768U
#define CONTAINER_BARRIER_ATTEMPTS 500U
#define PORTB_CONSOLE 0xe9
#define AGENT_STOPPED 1
#ifndef CGROUP_ROOT
#define CGROUP_ROOT "/sys/fs/cgroup"
#endif
#define EXEC_CGROUP CGROUP_ROOT "/nvx-exec"
#define HOSTFS_DIR "/run/nvx/hostfs"
#define HOSTFS_ROOT HOSTFS_DIR "/root"
#define MAP_TOKEN "nvx_map="
#define MAX_COMMAND_LINE 4096U
#define MAX_GUEST_PATH 4096U

struct outer_record {
    uint8_t type;
    uint8_t instance_id[16];
    uint64_t epoch;
    uint64_t sequence;
    uint32_t payload_len;
    uint8_t *payload;
};

struct control_session {
    int fd;
    uint8_t instance_id[16];
    uint64_t epoch;
    uint64_t guest_sequence;
    uint64_t host_sequence;
};

struct app_request {
    uint8_t kind;
    uint64_t request_id;
    int32_t status;
    uint32_t payload_len;
    const uint8_t *payload;
};

struct agent_config {
    const char *rootfs;
    const char *hostname;
    const char *uid;
    const char *gid;
    const char *user;
    const char *home;
    int direct;
};

struct exec_config {
    char *cwd;
    char **environment;
    uint16_t environment_count;
    int environment_present;
};

static uint16_t read_u16(const uint8_t *bytes)
{
    return (uint16_t)bytes[0] | ((uint16_t)bytes[1] << 8);
}

static uint32_t read_u32(const uint8_t *bytes)
{
    return (uint32_t)bytes[0] | ((uint32_t)bytes[1] << 8) |
           ((uint32_t)bytes[2] << 16) | ((uint32_t)bytes[3] << 24);
}

static uint64_t read_u64(const uint8_t *bytes)
{
    uint64_t value = 0;
    unsigned int index;

    for (index = 0; index < 8; ++index) {
        value |= (uint64_t)bytes[index] << (index * 8);
    }
    return value;
}

static void write_u16(uint8_t *bytes, uint16_t value)
{
    bytes[0] = (uint8_t)value;
    bytes[1] = (uint8_t)(value >> 8);
}

static void write_u32(uint8_t *bytes, uint32_t value)
{
    bytes[0] = (uint8_t)value;
    bytes[1] = (uint8_t)(value >> 8);
    bytes[2] = (uint8_t)(value >> 16);
    bytes[3] = (uint8_t)(value >> 24);
}

static void write_u64(uint8_t *bytes, uint64_t value)
{
    unsigned int index;

    for (index = 0; index < 8; ++index) {
        bytes[index] = (uint8_t)(value >> (index * 8));
    }
}

static int read_exact(int fd, void *buffer, size_t length)
{
    uint8_t *bytes = buffer;
    size_t offset = 0;

    while (offset < length) {
        ssize_t count = read(fd, bytes + offset, length - offset);
        if (count > 0) {
            offset += (size_t)count;
        } else if (count == 0) {
            return -1;
        } else if (errno != EINTR) {
            return -1;
        }
    }
    return 0;
}

static int write_all(int fd, const void *buffer, size_t length)
{
    const uint8_t *bytes = buffer;
    size_t offset = 0;

    while (offset < length) {
        ssize_t count = write(fd, bytes + offset, length - offset);
        if (count > 0) {
            offset += (size_t)count;
        } else if (count < 0 && errno == EINTR) {
            continue;
        } else {
            return -1;
        }
    }
    return 0;
}

static void portb_log(const char *message)
{
    int fd = open("/dev/port", O_WRONLY | O_CLOEXEC);

    if (fd < 0) {
        return;
    }
    while (*message != '\0') {
        if (pwrite(fd, message, 1, PORTB_CONSOLE) != 1) {
            break;
        }
        ++message;
    }
    close(fd);
}

static void portb_error(const char *stage, int status)
{
    char message[128];
    int length = snprintf(
        message,
        sizeof(message),
        "NVX-MANAGED-ERROR: stage=%s status=%d\n",
        stage,
        status);

    if (length > 0 && (size_t)length < sizeof(message)) {
        portb_log(message);
    }
}

static void free_outer_record(struct outer_record *record)
{
    free(record->payload);
    record->payload = NULL;
    record->payload_len = 0;
}

static int read_outer_record(int fd, struct outer_record *record)
{
    uint8_t header[OUTER_HEADER_LEN];

    memset(record, 0, sizeof(*record));
    if (read_exact(fd, header, sizeof(header)) != 0) {
        return -1;
    }
    if (memcmp(header, "NVXS", 4) != 0 || read_u16(header + 4) != OUTER_VERSION ||
        header[7] != 0) {
        return -1;
    }
    record->type = header[6];
    memcpy(record->instance_id, header + 8, sizeof(record->instance_id));
    record->epoch = read_u64(header + 24);
    record->sequence = read_u64(header + 32);
    record->payload_len = read_u32(header + 40);
    if (record->payload_len > OUTER_MAX_PAYLOAD) {
        return -1;
    }
    if (record->payload_len != 0) {
        record->payload = malloc(record->payload_len);
        if (record->payload == NULL ||
            read_exact(fd, record->payload, record->payload_len) != 0) {
            free_outer_record(record);
            return -1;
        }
    }
    return 0;
}

static int write_outer_record(
    int fd,
    uint8_t type,
    const uint8_t instance_id[16],
    uint64_t epoch,
    uint64_t sequence,
    const void *payload,
    uint32_t payload_len)
{
    uint8_t header[OUTER_HEADER_LEN] = {0};

    if (payload_len > OUTER_MAX_PAYLOAD) {
        return -1;
    }
    memcpy(header, "NVXS", 4);
    write_u16(header + 4, OUTER_VERSION);
    header[6] = type;
    memcpy(header + 8, instance_id, 16);
    write_u64(header + 24, epoch);
    write_u64(header + 32, sequence);
    write_u32(header + 40, payload_len);
    if (write_all(fd, header, sizeof(header)) != 0) {
        return -1;
    }
    return payload_len == 0 || write_all(fd, payload, payload_len) == 0 ? 0 : -1;
}

static int hex_digit(char value)
{
    if (value >= '0' && value <= '9') {
        return value - '0';
    }
    if (value >= 'A' && value <= 'F') {
        return value - 'A' + 10;
    }
    if (value >= 'a' && value <= 'f') {
        return value - 'a' + 10;
    }
    return -1;
}

/* Decodes a percent-encoded field of length `length` into a NUL-terminated string. */
static int percent_decode(const char *input, size_t length, char *output, size_t capacity)
{
    size_t index;
    size_t used = 0;

    for (index = 0; index < length; ++index) {
        char value = input[index];

        if (value == '%') {
            int high;
            int low;

            if (index + 2 >= length) {
                return -1;
            }
            high = hex_digit(input[index + 1]);
            low = hex_digit(input[index + 2]);
            if (high < 0 || low < 0) {
                return -1;
            }
            value = (char)((high << 4) | low);
            index += 2;
        }
        if (value == '\0' || used + 1 >= capacity) {
            return -1;
        }
        output[used++] = value;
    }
    output[used] = '\0';
    return 0;
}

/* Accepts a /-separated path without empty, "." (unless alone), or ".." components. */
static int safe_path(const char *path, int absolute)
{
    const char *component = path;

    if (absolute) {
        if (path[0] != '/') {
            return 0;
        }
        component = path + 1;
        if (*component == '\0') {
            return 0;
        }
    } else if (strcmp(path, ".") == 0) {
        return 1;
    }
    while (*component != '\0') {
        const char *end = strchr(component, '/');
        size_t length = end == NULL ? strlen(component) : (size_t)(end - component);

        if (length == 0 || (length == 1 && component[0] == '.') ||
            (length == 2 && component[0] == '.' && component[1] == '.')) {
            return 0;
        }
        component += length;
        if (*component == '/') {
            ++component;
            if (*component == '\0') {
                return 0;
            }
        }
    }
    return 1;
}

/* Creates every missing directory of `path` up to, and optionally including, its last component. */
static int make_directories(const char *path, int include_last)
{
    char buffer[MAX_GUEST_PATH];
    size_t index;
    size_t length = strlen(path);

    if (length >= sizeof(buffer)) {
        return -1;
    }
    memcpy(buffer, path, length + 1);
    for (index = 1; index <= length; ++index) {
        if (buffer[index] == '/' || (buffer[index] == '\0' && include_last)) {
            char saved = buffer[index];

            buffer[index] = '\0';
            if (mkdir(buffer, 0755) != 0 && errno != EEXIST) {
                return -1;
            }
            buffer[index] = saved;
        }
    }
    return 0;
}

/*
 * Bind-mounts each mapped host path at its guest target. The host's export is
 * mounted at HOSTFS_ROOT, whose parent only root may enter, so workloads reach
 * mapped paths only through these bind mounts. Every token has the form
 * nvx_map=SOURCE,TARGET,ro|rw with percent-encoded paths; SOURCE is relative to
 * the export, and "." is the export itself.
 */
static int setup_host_mappings(void)
{
    char command_line[MAX_COMMAND_LINE];
    char *token;
    char *cursor;
    ssize_t count;
    int fd = open("/proc/cmdline", O_RDONLY | O_CLOEXEC);
    int prepared = 0;

    if (fd < 0) {
        return -1;
    }
    count = read(fd, command_line, sizeof(command_line) - 1);
    close(fd);
    if (count < 0) {
        return -1;
    }
    command_line[count] = '\0';

    for (token = strtok_r(command_line, " \n", &cursor); token != NULL;
         token = strtok_r(NULL, " \n", &cursor)) {
        char source[MAX_GUEST_PATH];
        char target[MAX_GUEST_PATH];
        char host[MAX_GUEST_PATH + sizeof(HOSTFS_ROOT) + 1];
        const char *fields = token + strlen(MAP_TOKEN);
        const char *first;
        const char *second;
        struct stat status;
        unsigned long flags = MS_BIND | MS_REMOUNT | MS_NOSUID | MS_NODEV;

        if (strncmp(token, MAP_TOKEN, strlen(MAP_TOKEN)) != 0) {
            continue;
        }
        first = strchr(fields, ',');
        second = first == NULL ? NULL : strchr(first + 1, ',');
        if (second == NULL || strchr(second + 1, ',') != NULL ||
            percent_decode(fields, (size_t)(first - fields), source, sizeof(source)) != 0 ||
            percent_decode(first + 1, (size_t)(second - first - 1), target, sizeof(target)) !=
                0 ||
            !safe_path(source, 0) || !safe_path(target, 1)) {
            errno = EINVAL;
            return -1;
        }
        if (strcmp(second + 1, "ro") == 0) {
            flags |= MS_RDONLY;
        } else if (strcmp(second + 1, "rw") != 0) {
            errno = EINVAL;
            return -1;
        }
        if (!prepared) {
            struct stat parent;
            struct stat export;

            /* The export must be a mount of its own, inside a root-only directory. */
            if (stat(HOSTFS_DIR, &parent) != 0 || stat(HOSTFS_ROOT, &export) != 0 ||
                parent.st_dev == export.st_dev || chown(HOSTFS_DIR, 0, 0) != 0 ||
                chmod(HOSTFS_DIR, 0700) != 0) {
                return -1;
            }
            prepared = 1;
        }
        if (strcmp(source, ".") == 0) {
            snprintf(host, sizeof(host), "%s", HOSTFS_ROOT);
        } else {
            snprintf(host, sizeof(host), "%s/%s", HOSTFS_ROOT, source);
        }
        if (lstat(host, &status) != 0) {
            return -1;
        }
        if (S_ISDIR(status.st_mode)) {
            if (make_directories(target, 1) != 0) {
                return -1;
            }
        } else if (S_ISREG(status.st_mode)) {
            struct stat existing;

            if (make_directories(target, 0) != 0) {
                return -1;
            }
            /*
             * An existing target may lie inside an earlier read-only bind, where
             * even O_CREAT with write access fails, so only a missing one is made.
             */
            if (lstat(target, &existing) != 0) {
                int file;

                if (errno != ENOENT) {
                    return -1;
                }
                file = open(target, O_RDONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0644);
                if (file < 0) {
                    return -1;
                }
                close(file);
            } else if (!S_ISREG(existing.st_mode)) {
                errno = EINVAL;
                return -1;
            }
        } else {
            errno = EINVAL;
            return -1;
        }
        if (mount(host, target, NULL, MS_BIND, NULL) != 0 ||
            mount(NULL, target, NULL, flags, NULL) != 0) {
            return -1;
        }
    }
    return 0;
}

static int send_guest_attach(int fd)
{
    const uint8_t zero_instance[16] = {0};

    return write_outer_record(
        fd, OUTER_GUEST_ATTACH, zero_instance, 0, 0, NULL, 0);
}

static int acknowledge_reset(
    struct control_session *session,
    const struct outer_record *record)
{
    uint8_t credit[4];

    if (record->type != OUTER_RESET || record->payload_len != 0 ||
        record->epoch == 0) {
        return -1;
    }
    memcpy(session->instance_id, record->instance_id, 16);
    session->epoch = record->epoch;
    session->guest_sequence = 0;
    session->host_sequence = record->sequence + 1;
    write_u32(credit, OUTER_RECEIVE_WINDOW);
    if (write_outer_record(
            session->fd,
            OUTER_ACK,
            session->instance_id,
            session->epoch,
            session->guest_sequence,
            credit,
            sizeof(credit)) != 0) {
        return -1;
    }
    session->guest_sequence = 1;
    return 0;
}

static int send_guest_record(
    struct control_session *session,
    uint8_t type,
    const void *payload,
    uint32_t payload_len)
{
    if (write_outer_record(
            session->fd,
            type,
            session->instance_id,
            session->epoch,
            session->guest_sequence,
            payload,
            payload_len) != 0) {
        return -1;
    }
    ++session->guest_sequence;
    return 0;
}

static int send_credit(struct control_session *session, uint32_t bytes)
{
    uint8_t payload[4];

    if (bytes == 0) {
        return 0;
    }
    write_u32(payload, bytes);
    return send_guest_record(session, OUTER_CREDIT, payload, sizeof(payload));
}

static int send_app_frame(
    struct control_session *session,
    uint8_t kind,
    uint64_t request_id,
    int32_t status,
    const void *payload,
    uint32_t payload_len)
{
    uint8_t *frame;
    uint32_t frame_len;
    int result;

    if (payload_len > OUTER_MAX_PAYLOAD - APP_HEADER_LEN) {
        return -1;
    }
    frame_len = APP_HEADER_LEN + payload_len;
    frame = malloc(frame_len);
    if (frame == NULL) {
        return -1;
    }
    memset(frame, 0, APP_HEADER_LEN);
    memcpy(frame, "NVXC", 4);
    frame[4] = APP_VERSION;
    frame[5] = kind;
    write_u64(frame + 8, request_id);
    write_u32(frame + 16, (uint32_t)status);
    write_u32(frame + 20, payload_len);
    if (payload_len != 0) {
        memcpy(frame + APP_HEADER_LEN, payload, payload_len);
    }
    result = send_guest_record(session, OUTER_DATA, frame, frame_len);
    free(frame);
    return result;
}

static int parse_app_request(
    const uint8_t *payload,
    uint32_t payload_len,
    struct app_request *request)
{
    uint32_t declared_len;

    if (payload_len < APP_HEADER_LEN || memcmp(payload, "NVXC", 4) != 0 ||
        payload[4] != APP_VERSION || read_u16(payload + 6) != 0) {
        return -1;
    }
    declared_len = read_u32(payload + 20);
    if (declared_len != payload_len - APP_HEADER_LEN) {
        return -1;
    }
    request->kind = payload[5];
    request->request_id = read_u64(payload + 8);
    request->status = (int32_t)read_u32(payload + 16);
    request->payload_len = declared_len;
    request->payload = payload + APP_HEADER_LEN;
    return request->request_id != 0 && request->status == 0 ? 0 : -1;
}

static int send_app_error(
    struct control_session *session,
    uint64_t request_id,
    int32_t status,
    const char *category)
{
    return send_app_frame(
        session,
        APP_ERROR,
        request_id,
        status,
        category,
        (uint32_t)strlen(category));
}

static uint64_t monotonic_milliseconds(void)
{
    struct timespec value;

    if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) {
        return 0;
    }
    return (uint64_t)value.tv_sec * 1000U + (uint64_t)value.tv_nsec / 1000000U;
}

static int open_control_tty(const char *path)
{
    const struct timespec delay = {
        .tv_sec = 0,
        .tv_nsec = 50U * 1000U * 1000U,
    };
    unsigned int attempt;

    for (attempt = 0; attempt < 600; ++attempt) {
        int fd = open(path, O_RDWR | O_CLOEXEC);
        if (fd >= 0) {
            return fd;
        }
        if (errno != ENOENT && errno != ENXIO && errno != ENODEV && errno != EIO) {
            return -1;
        }
        nanosleep(&delay, NULL);
    }
    errno = ETIMEDOUT;
    return -1;
}

static int configure_control_tty(int fd)
{
    struct termios settings;

    if (tcgetattr(fd, &settings) != 0) {
        return -1;
    }
    cfmakeraw(&settings);
    settings.c_cflag |= CLOCAL;
    return tcsetattr(fd, TCSANOW, &settings);
}

static int make_nonblocking(int fd)
{
    int flags = fcntl(fd, F_GETFL);

    return flags >= 0 && fcntl(fd, F_SETFL, flags | O_NONBLOCK) == 0 ? 0 : -1;
}

static int write_pid_to_cgroup(pid_t pid)
{
    char buffer[32];
    int fd;
    int length;

    fd = open("/sys/fs/cgroup/container/cgroup.procs", O_WRONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    length = snprintf(buffer, sizeof(buffer), "%ld\n", (long)pid);
    if (length <= 0 || (size_t)length >= sizeof(buffer) ||
        write_all(fd, buffer, (size_t)length) != 0) {
        close(fd);
        return -1;
    }
    return close(fd);
}

static int write_container_barrier(int fd)
{
    struct sigaction ignored = {0};
    struct sigaction previous;
    int result;
    int status;

    ignored.sa_handler = SIG_IGN;
    if (sigemptyset(&ignored.sa_mask) != 0 ||
        sigaction(SIGPIPE, &ignored, &previous) != 0) {
        return -1;
    }
    result = write_all(fd, "start\n", 6);
    status = errno;
    if (sigaction(SIGPIPE, &previous, NULL) != 0) {
        return -1;
    }
    if (result != 0) {
        errno = status;
    }
    return result;
}

/* Returns 0 after writing, 1 for an unreaped child exit, or -1 on failure. */
static int release_container_barrier(const char *path, pid_t child)
{
    unsigned int attempt;

    for (attempt = 0; attempt < CONTAINER_BARRIER_ATTEMPTS; ++attempt) {
        int fd = open(path, O_WRONLY | O_CLOEXEC | O_NONBLOCK);

        if (fd >= 0) {
            int result = write_container_barrier(fd);

            close(fd);
            return result;
        }
        if (errno != ENXIO && errno != EINTR) {
            return -1;
        }
        if (child > 0) {
            siginfo_t information = {0};

            if (waitid(
                    P_PID,
                    (id_t)child,
                    &information,
                    WEXITED | WNOHANG | WNOWAIT) != 0) {
                if (errno == EINTR) {
                    continue;
                }
                return -1;
            }
            if (information.si_pid == child) {
                return 1;
            }
        }
        {
            const struct timespec delay = {.tv_nsec = 10000000L};

            nanosleep(&delay, NULL);
        }
    }
    errno = ETIMEDOUT;
    return -1;
}

/*
 * Direct-mode workloads run in the guest's own root file system, contained by
 * a cgroup that every process of the workload inherits, whatever session or
 * process group it moves to. Killing the cgroup ends all of them.
 */
static int prepare_exec_cgroup(void)
{
    struct stat status;

    if (stat(CGROUP_ROOT "/cgroup.procs", &status) != 0) {
        if (mkdir(CGROUP_ROOT, 0755) != 0 && errno != EEXIST) {
            return -1;
        }
        if (mount("cgroup2", CGROUP_ROOT, "cgroup2", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL) !=
            0) {
            return -1;
        }
    }
    return mkdir(EXEC_CGROUP, 0755) == 0 || errno == EEXIST ? 0 : -1;
}

static int join_exec_cgroup(void)
{
    char buffer[32];
    int fd = open(EXEC_CGROUP "/cgroup.procs", O_WRONLY | O_CLOEXEC);
    int length;
    int result;

    if (fd < 0) {
        return -1;
    }
    length = snprintf(buffer, sizeof(buffer), "%ld\n", (long)getpid());
    result = length > 0 && (size_t)length < sizeof(buffer)
                 ? write_all(fd, buffer, (size_t)length)
                 : -1;
    close(fd);
    return result;
}

static int kill_exec_cgroup(void)
{
    int fd = open(EXEC_CGROUP "/cgroup.kill", O_WRONLY | O_CLOEXEC);
    int result;
    int status;

    if (fd < 0) {
        goto error;
    }
    result = write_all(fd, "1", 1);
    status = errno;
    if (close(fd) != 0 && result == 0) {
        goto error;
    }
    if (result == 0) {
        return 0;
    }
    errno = status;
error:
    status = errno;
    portb_error("exec-cgroup-kill", status);
    errno = status;
    return -1;
}

static int exec_cgroup_populated(void)
{
    char buffer[256];
    char *line;
    size_t length = 0;
    int populated = -1;
    int fd = open(EXEC_CGROUP "/cgroup.events", O_RDONLY | O_CLOEXEC);

    if (fd < 0) {
        return -1;
    }
    for (;;) {
        ssize_t count = read(fd, buffer + length, sizeof(buffer) - 1 - length);

        if (count > 0) {
            length += (size_t)count;
            if (length < sizeof(buffer) - 1) {
                continue;
            }
            errno = EOVERFLOW;
        } else if (count == 0) {
            break;
        } else if (errno == EINTR) {
            continue;
        }
        {
            int status = errno;

            close(fd);
            errno = status;
            return -1;
        }
    }
    if (close(fd) != 0) {
        return -1;
    }
    if (length == 0 || buffer[length - 1] != '\n' ||
        memchr(buffer, '\0', length) != NULL) {
        errno = EPROTO;
        return -1;
    }
    buffer[length] = '\0';
    line = buffer;
    while (*line != '\0') {
        char *end = strchr(line, '\n');

        *end = '\0';
        if (strncmp(line, "populated", 9) == 0) {
            if (populated >= 0 ||
                (strcmp(line, "populated 0") != 0 && strcmp(line, "populated 1") != 0)) {
                errno = EPROTO;
                return -1;
            }
            populated = line[10] == '1';
        }
        line = end + 1;
    }
    if (populated < 0) {
        errno = EPROTO;
    }
    return populated;
}

/*
 * Reaps exited children without blocking. The agent runs as PID 1, so besides
 * the workload's direct child it inherits every orphaned workload process.
 */
static void reap_children(pid_t child, int *child_exited, int *wait_status)
{
    for (;;) {
        int status;
        pid_t pid = waitpid(-1, &status, WNOHANG);

        if (pid < 0 && errno == EINTR) {
            continue;
        }
        if (pid <= 0) {
            return;
        }
        if (pid == child && !*child_exited) {
            *child_exited = 1;
            *wait_status = status;
        }
    }
}

/* Kills what is left of a direct-mode workload and waits until it is gone. */
static int settle_exec_cgroup(void)
{
    const struct timespec delay = {
        .tv_sec = 0,
        .tv_nsec = 10U * 1000U * 1000U,
    };
    int exited = 1;
    int status = 0;
    unsigned int attempt;

    if (kill_exec_cgroup() != 0) {
        return -1;
    }
    for (attempt = 0; attempt <= 200; ++attempt) {
        int populated;

        reap_children(0, &exited, &status);
        populated = exec_cgroup_populated();
        if (populated < 0) {
            return -1;
        }
        if (populated == 0) {
            return 0;
        }
        if (attempt == 200) {
            break;
        }
        nanosleep(&delay, NULL);
    }
    errno = ETIMEDOUT;
    return -1;
}

/* Kills the workload: its process group, and in direct mode its whole cgroup. */
static void terminate_workload(const struct agent_config *config, pid_t child)
{
    kill(-child, SIGKILL);
    if (config->direct) {
        (void)kill_exec_cgroup();
    }
}

static void exec_direct(
    const struct agent_config *config,
    const char *config_fd,
    char *const workload_argv[])
{
    char *arguments[MAX_ARGUMENTS + 20];
    size_t index = 0;
    size_t workload_index = 0;

    if (join_exec_cgroup() != 0) {
        _exit(125);
    }
    if (setenv("HOME", config->home, 1) != 0 ||
        setenv("USER", config->user, 1) != 0 ||
        setenv("LOGNAME", config->user, 1) != 0) {
        dprintf(
            STDERR_FILENO,
            "nvx-managed-agent: cannot configure workload environment: %s\n",
            strerror(errno));
        _exit(125);
    }
    arguments[index++] = "setpriv";
    arguments[index++] = "--reuid";
    arguments[index++] = (char *)config->uid;
    arguments[index++] = "--regid";
    arguments[index++] = (char *)config->gid;
    arguments[index++] = "--clear-groups";
    arguments[index++] = "--no-new-privs";
    arguments[index++] = "--bounding-set=-all";
    arguments[index++] = "--inh-caps=-all";
    arguments[index++] = "--ambient-caps=-all";
    arguments[index++] = "/sbin/nvx-managed-agent";
    arguments[index++] = "--exec-config-fd";
    arguments[index++] = (char *)config_fd;
    arguments[index++] = "--";
    while (workload_argv[workload_index] != NULL && index + 1 < MAX_ARGUMENTS + 20) {
        arguments[index++] = workload_argv[workload_index++];
    }
    arguments[index] = NULL;
    execvp("setpriv", arguments);
    _exit(125);
}

static void exec_sandbox(
    const struct agent_config *config,
    const char *barrier,
    const char *config_fd,
    char *const workload_argv[])
{
    char *arguments[MAX_ARGUMENTS + 10];
    size_t index = 0;
    size_t workload_index = 0;

    arguments[index++] = "/sbin/nvx-container-launch";
    arguments[index++] = (char *)barrier;
    arguments[index++] = (char *)config->rootfs;
    arguments[index++] = (char *)config->hostname;
    arguments[index++] = (char *)config->uid;
    arguments[index++] = (char *)config->gid;
    arguments[index++] = (char *)config->user;
    arguments[index++] = (char *)config->home;
    if (setenv("NVX_EXEC_CONFIG_FD", config_fd, 1) != 0) {
        dprintf(
            STDERR_FILENO,
            "nvx-managed-agent: cannot export execution configuration: %s\n",
            strerror(errno));
        _exit(125);
    }
    while (workload_argv[workload_index] != NULL && index + 1 < MAX_ARGUMENTS + 10) {
        arguments[index++] = workload_argv[workload_index++];
    }
    arguments[index] = NULL;
    execv(arguments[0], arguments);
    _exit(125);
}

static int decode_exec_payload(
    const uint8_t *payload,
    uint32_t payload_len,
    uint32_t *timeout_ms,
    char ***workload_argv,
    struct exec_config *config)
{
    uint16_t argc;
    uint16_t extension;
    uint16_t flags = 0;
    uint16_t environment_count = 0;
    uint32_t cwd_len = 0;
    uint32_t offset = 8;
    char **arguments;
    uint16_t index;

    memset(config, 0, sizeof(*config));
    if (payload_len < 8) {
        return -1;
    }
    *timeout_ms = read_u32(payload);
    argc = read_u16(payload + 4);
    extension = read_u16(payload + 6);
    if (argc == 0 || argc > MAX_ARGUMENTS) {
        return -1;
    }
    if (extension == EXEC_EXTENDED) {
        if (payload_len < 16) {
            return -1;
        }
        flags = read_u16(payload + 8);
        environment_count = read_u16(payload + 10);
        cwd_len = read_u32(payload + 12);
        offset = 16;
        if ((flags & ~(EXEC_CWD_PRESENT | EXEC_ENVIRONMENT_PRESENT)) != 0 ||
            ((flags & EXEC_CWD_PRESENT) == 0 && cwd_len != 0) ||
            ((flags & EXEC_CWD_PRESENT) != 0 &&
             (cwd_len == 0 || cwd_len > MAX_ARGUMENT_LEN)) ||
            ((flags & EXEC_ENVIRONMENT_PRESENT) == 0 &&
             environment_count != 0) ||
            environment_count > MAX_ENVIRONMENT) {
            return -1;
        }
    } else if (extension != 0) {
        return -1;
    }
    arguments = calloc((size_t)argc + 1, sizeof(*arguments));
    if (arguments == NULL) {
        return -1;
    }
    for (index = 0; index < argc; ++index) {
        uint32_t length;

        if (offset > payload_len || payload_len - offset < 4) {
            goto fail;
        }
        length = read_u32(payload + offset);
        offset += 4;
        if (length == 0 || length > MAX_ARGUMENT_LEN || length > payload_len - offset ||
            memchr(payload + offset, '\0', length) != NULL) {
            goto fail;
        }
        arguments[index] = malloc((size_t)length + 1);
        if (arguments[index] == NULL) {
            goto fail;
        }
        memcpy(arguments[index], payload + offset, length);
        arguments[index][length] = '\0';
        offset += length;
    }
    if (arguments[0][0] != '/' ||
        (extension == 0 && offset != payload_len)) {
        goto fail;
    }
    if (extension == EXEC_EXTENDED) {
        if (cwd_len != 0) {
            if (cwd_len > payload_len - offset ||
                memchr(payload + offset, '\0', cwd_len) != NULL ||
                payload[offset] != '/') {
                goto fail;
            }
            config->cwd = malloc((size_t)cwd_len + 1);
            if (config->cwd == NULL) {
                goto fail;
            }
            memcpy(config->cwd, payload + offset, cwd_len);
            config->cwd[cwd_len] = '\0';
            offset += cwd_len;
        }
        if ((flags & EXEC_ENVIRONMENT_PRESENT) != 0) {
            config->environment = calloc(
                (size_t)environment_count + 1, sizeof(*config->environment));
            if (config->environment == NULL) {
                goto fail;
            }
            config->environment_present = 1;
            config->environment_count = environment_count;
            for (index = 0; index < environment_count; ++index) {
                uint32_t length;
                const uint8_t *equals;

                if (offset > payload_len || payload_len - offset < 4) {
                    goto fail;
                }
                length = read_u32(payload + offset);
                offset += 4;
                if (length == 0 || length > MAX_ARGUMENT_LEN ||
                    length > payload_len - offset ||
                    memchr(payload + offset, '\0', length) != NULL) {
                    goto fail;
                }
                equals = memchr(payload + offset, '=', length);
                if (equals == NULL || equals == payload + offset) {
                    goto fail;
                }
                for (uint16_t previous = 0; previous < index; ++previous) {
                    size_t name_length = (size_t)(equals - (payload + offset));
                    const char *prior = config->environment[previous];
                    if (strcspn(prior, "=") == name_length &&
                        memcmp(prior, payload + offset, name_length) == 0) {
                        goto fail;
                    }
                }
                config->environment[index] = malloc((size_t)length + 1);
                if (config->environment[index] == NULL) {
                    goto fail;
                }
                memcpy(config->environment[index], payload + offset, length);
                config->environment[index][length] = '\0';
                offset += length;
            }
        }
        if (offset != payload_len) {
            goto fail;
        }
    }
    *workload_argv = arguments;
    return 0;

fail:
    for (index = 0; index < argc; ++index) {
        free(arguments[index]);
    }
    free(arguments);
    free(config->cwd);
    config->cwd = NULL;
    if (config->environment != NULL) {
        for (index = 0; index < config->environment_count; ++index) {
            free(config->environment[index]);
        }
        free(config->environment);
        config->environment = NULL;
    }
    return -1;
}

static void free_arguments(char **arguments)
{
    size_t index;

    if (arguments == NULL) {
        return;
    }
    for (index = 0; arguments[index] != NULL; ++index) {
        free(arguments[index]);
    }
    free(arguments);
}

static void free_exec_config(struct exec_config *config)
{
    uint16_t index;

    free(config->cwd);
    for (index = 0; index < config->environment_count; ++index) {
        free(config->environment[index]);
    }
    free(config->environment);
    memset(config, 0, sizeof(*config));
}

static int write_exec_config(int fd, const struct exec_config *config)
{
    uint8_t header[8];
    uint16_t flags = 0;
    uint16_t index;

    if (config->cwd != NULL) {
        flags |= EXEC_CWD_PRESENT;
    }
    if (config->environment_present) {
        flags |= EXEC_ENVIRONMENT_PRESENT;
    }
    write_u16(header, flags);
    write_u16(header + 2, config->environment_count);
    write_u32(header + 4, config->cwd == NULL ? 0 : (uint32_t)strlen(config->cwd));
    if (write_all(fd, header, sizeof(header)) != 0 ||
        (config->cwd != NULL &&
         write_all(fd, config->cwd, strlen(config->cwd)) != 0)) {
        return -1;
    }
    for (index = 0; index < config->environment_count; ++index) {
        uint32_t length = (uint32_t)strlen(config->environment[index]);
        uint8_t encoded_length[4];

        write_u32(encoded_length, length);
        if (write_all(fd, encoded_length, sizeof(encoded_length)) != 0 ||
            write_all(fd, config->environment[index], length) != 0) {
            return -1;
        }
    }
    return 0;
}

static int create_exec_config_fd(const struct exec_config *config)
{
    int fd = memfd_create("nvx-exec-config", MFD_CLOEXEC | MFD_ALLOW_SEALING);

    if (fd < 0) {
        return -1;
    }
    /* A sealed, bounded anonymous file avoids depending on pipe capacity. */
    if (write_exec_config(fd, config) != 0 ||
        lseek(fd, 0, SEEK_SET) < 0 ||
        fcntl(fd, F_ADD_SEALS,
              F_SEAL_WRITE | F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static int launch_workload(int argc, char **argv)
{
    char *end = NULL;
    long descriptor;
    uint8_t header[8];
    uint16_t flags;
    uint16_t environment_count;
    uint32_t cwd_len;
    char *cwd = NULL;
    char **environment = NULL;
    uint16_t index;

    if (argc < 5 || strcmp(argv[1], "--exec-config-fd") != 0 ||
        strcmp(argv[3], "--") != 0 || argv[4][0] != '/') {
        return 125;
    }
    errno = 0;
    descriptor = strtol(argv[2], &end, 10);
    if (errno != 0 || end == argv[2] || *end != '\0' ||
        descriptor < 0 || descriptor > INT32_MAX ||
        read_exact((int)descriptor, header, sizeof(header)) != 0) {
        return 125;
    }
    flags = read_u16(header);
    environment_count = read_u16(header + 2);
    cwd_len = read_u32(header + 4);
    if ((flags & ~(EXEC_CWD_PRESENT | EXEC_ENVIRONMENT_PRESENT)) != 0 ||
        environment_count > MAX_ENVIRONMENT ||
        ((flags & EXEC_CWD_PRESENT) == 0 && cwd_len != 0) ||
        ((flags & EXEC_CWD_PRESENT) != 0 &&
         (cwd_len == 0 || cwd_len > MAX_ARGUMENT_LEN)) ||
        ((flags & EXEC_ENVIRONMENT_PRESENT) == 0 &&
         environment_count != 0)) {
        return 125;
    }
    if (cwd_len != 0) {
        cwd = malloc((size_t)cwd_len + 1);
        if (cwd == NULL || read_exact((int)descriptor, cwd, cwd_len) != 0) {
            free(cwd);
            return 125;
        }
        cwd[cwd_len] = '\0';
    }
    environment = calloc(
        (size_t)environment_count + 1, sizeof(*environment));
    if (environment == NULL) {
        free(cwd);
        return 125;
    }
    for (index = 0; index < environment_count; ++index) {
        uint8_t encoded_length[4];
        uint32_t length;

        if (read_exact((int)descriptor, encoded_length, 4) != 0) {
            goto fail;
        }
        length = read_u32(encoded_length);
        if (length == 0 || length > MAX_ARGUMENT_LEN) {
            goto fail;
        }
        environment[index] = malloc((size_t)length + 1);
        if (environment[index] == NULL ||
            read_exact((int)descriptor, environment[index], length) != 0) {
            goto fail;
        }
        if (memchr(environment[index], '\0', length) != NULL ||
            environment[index][0] == '=' ||
            memchr(environment[index], '=', length) == NULL) {
            goto fail;
        }
        environment[index][length] = '\0';
    }
    close((int)descriptor);
    if (cwd != NULL &&
        (cwd[0] != '/' || memchr(cwd, '\0', cwd_len) != NULL)) {
        dprintf(STDERR_FILENO, "nvx-managed-agent: invalid working directory\n");
        goto fail;
    }
    if (unsetenv("NVX_EXEC_CONFIG_FD") != 0) {
        goto fail;
    }
    if ((flags & EXEC_ENVIRONMENT_PRESENT) != 0) {
        if (clearenv() != 0) {
            goto fail;
        }
        for (index = 0; index < environment_count; ++index) {
            if (putenv(environment[index]) != 0) {
                goto fail;
            }
            environment[index] = NULL;
        }
    }
    if (chdir(cwd == NULL ? "/" : cwd) != 0) {
        dprintf(
            STDERR_FILENO,
            "nvx-managed-agent: cannot use working directory %s: %s\n",
            cwd == NULL ? "/" : cwd,
            strerror(errno));
        goto fail;
    }
    execv(argv[4], &argv[4]);
    dprintf(
        STDERR_FILENO,
        "nvx-managed-agent: cannot execute workload: %s\n",
        strerror(errno));

fail:
    free(cwd);
    for (index = 0; index < environment_count; ++index) {
        free(environment[index]);
    }
    free(environment);
    return 125;
}

static int stream_output(
    struct control_session *session,
    uint64_t request_id,
    int fd,
    uint8_t kind,
    size_t *output_bytes)
{
    uint8_t buffer[OUTPUT_CHUNK_BYTES];
    ssize_t count;

    for (;;) {
        count = read(fd, buffer, sizeof(buffer));
        if (count > 0) {
            if (*output_bytes > MAX_OUTPUT_BYTES - (size_t)count) {
                return -2;
            }
            *output_bytes += (size_t)count;
            if (send_app_frame(
                    session,
                    kind,
                    request_id,
                    0,
                    buffer,
                    (uint32_t)count) != 0) {
                return -1;
            }
        } else if (count == 0) {
            return 1;
        } else if (errno == EINTR) {
            continue;
        } else if (errno == EAGAIN || errno == EWOULDBLOCK) {
            return 0;
        } else {
            return -1;
        }
    }
}

/* Outcome of control traffic observed while a workload runs. */
enum exec_control {
    EXEC_CONTROL_NONE,
    EXEC_CONTROL_CANCEL,
    EXEC_CONTROL_SESSION_LOST,
    EXEC_CONTROL_FAILED,
};

/*
 * Handles one control record that arrives while the workload with request ID
 * `exec_id` runs. CANCEL for that workload asks for its termination. A RESET
 * means the host that could observe the workload is gone: the broker has
 * started a new epoch and drops anything sent for the old one. Other requests
 * are refused, because executions are sequential.
 */
static enum exec_control handle_exec_control(
    struct control_session *session,
    uint64_t exec_id)
{
    struct outer_record record;
    struct app_request request;
    enum exec_control result = EXEC_CONTROL_NONE;

    if (read_outer_record(session->fd, &record) != 0) {
        portb_error("exec-outer-read", errno);
        return EXEC_CONTROL_FAILED;
    }
    if (record.type == OUTER_RESET) {
        result = acknowledge_reset(session, &record) == 0
                     ? EXEC_CONTROL_SESSION_LOST
                     : EXEC_CONTROL_FAILED;
        free_outer_record(&record);
        return result;
    }
    if (record.type != OUTER_DATA || record.sequence != session->host_sequence ||
        record.epoch != session->epoch ||
        memcmp(record.instance_id, session->instance_id, 16) != 0 ||
        parse_app_request(record.payload, record.payload_len, &request) != 0) {
        portb_error("exec-record", record.type);
        free_outer_record(&record);
        return EXEC_CONTROL_FAILED;
    }
    ++session->host_sequence;
    if (send_credit(session, record.payload_len) != 0) {
        free_outer_record(&record);
        return EXEC_CONTROL_FAILED;
    }
    if (request.kind == APP_CANCEL) {
        if (request.payload_len == 0 && request.request_id == exec_id) {
            result = EXEC_CONTROL_CANCEL;
        }
    } else if (send_app_error(session, request.request_id, 16, "busy") != 0) {
        result = EXEC_CONTROL_FAILED;
    }
    free_outer_record(&record);
    return result;
}

/* Discards output of a workload whose host is gone; returns 1 at end-of-file. */
static int drain_output(int fd)
{
    uint8_t buffer[OUTPUT_CHUNK_BYTES];
    ssize_t count;

    for (;;) {
        count = read(fd, buffer, sizeof(buffer));
        if (count > 0) {
            continue;
        }
        if (count == 0) {
            return 1;
        }
        if (errno == EINTR) {
            continue;
        }
        return errno == EAGAIN || errno == EWOULDBLOCK ? 0 : -1;
    }
}

static int run_exec(
    struct control_session *session,
    const struct agent_config *config,
    uint64_t request_id,
    uint32_t timeout_ms,
    char **workload_argv,
    const struct exec_config *exec_config)
{
    const char *barrier = "/run/nvx/managed-container-start";
    int stdout_pipe[2] = {-1, -1};
    int stderr_pipe[2] = {-1, -1};
    int exec_config_fd = -1;
    pid_t child;
    uint64_t started;
    size_t output_bytes = 0;
    int stdout_open = 1;
    int stderr_open = 1;
    int timed_out = 0;
    int output_limited = 0;
    int cancelled = 0;
    int session_lost = 0;
    int stragglers_killed = 0;
    int wait_status = 0;
    int child_exited = 0;
    int launch_failed = 0;

    if (config->direct && prepare_exec_cgroup() != 0) {
        portb_error("exec-cgroup", errno);
        return send_app_error(session, request_id, 125, "launch-failed");
    }
    if (config->direct) {
        int populated = exec_cgroup_populated();

        if (populated != 0) {
            portb_error("exec-cgroup-verify", populated < 0 ? errno : EBUSY);
            return send_app_error(session, request_id, 125, "containment-failed");
        }
    }
    if (!config->direct) {
        unlink(barrier);
        if (mkfifo(barrier, 0600) != 0) {
            return send_app_error(session, request_id, 125, "launch-failed");
        }
    }
    if (pipe2(stdout_pipe, O_CLOEXEC) != 0 ||
        pipe2(stderr_pipe, O_CLOEXEC) != 0 ||
        (exec_config_fd = create_exec_config_fd(exec_config)) < 0) {
        unlink(barrier);
        if (stdout_pipe[0] >= 0) {
            close(stdout_pipe[0]);
            close(stdout_pipe[1]);
        }
        if (stderr_pipe[0] >= 0) {
            close(stderr_pipe[0]);
            close(stderr_pipe[1]);
        }
        if (exec_config_fd >= 0) {
            close(exec_config_fd);
        }
        return send_app_error(session, request_id, 125, "launch-failed");
    }

    child = fork();
    if (child < 0) {
        close(stdout_pipe[0]);
        close(stdout_pipe[1]);
        close(stderr_pipe[0]);
        close(stderr_pipe[1]);
        close(exec_config_fd);
        unlink(barrier);
        return send_app_error(session, request_id, 125, "launch-failed");
    }
    if (child == 0) {
        int null_fd;
        char config_fd[32];

        setpgid(0, 0);
        if (fcntl(exec_config_fd, F_SETFD, 0) != 0 ||
            snprintf(
                config_fd, sizeof(config_fd), "%d", exec_config_fd) <= 0) {
            _exit(125);
        }
        close(stdout_pipe[0]);
        close(stderr_pipe[0]);
        null_fd = open("/dev/null", O_RDONLY);
        if (null_fd >= 0) {
            dup2(null_fd, STDIN_FILENO);
            close(null_fd);
        }
        dup2(stdout_pipe[1], STDOUT_FILENO);
        dup2(stderr_pipe[1], STDERR_FILENO);
        close(stdout_pipe[1]);
        close(stderr_pipe[1]);
        if (config->direct) {
            exec_direct(config, config_fd, workload_argv);
        }
        exec_sandbox(config, barrier, config_fd, workload_argv);
    }

    setpgid(child, child);
    close(exec_config_fd);
    close(stdout_pipe[1]);
    close(stderr_pipe[1]);
    launch_failed = make_nonblocking(stdout_pipe[0]) != 0 ||
                    make_nonblocking(stderr_pipe[0]) != 0;
    if (!launch_failed && !config->direct) {
        int barrier_result;

        if (write_pid_to_cgroup(child) != 0) {
            launch_failed = 1;
        } else {
            barrier_result = release_container_barrier(barrier, child);
            launch_failed = barrier_result < 0;
        }
    }
    if (launch_failed) {
        terminate_workload(config, child);
        waitpid(child, NULL, 0);
        close(stdout_pipe[0]);
        close(stderr_pipe[0]);
        unlink(barrier);
        return send_app_error(session, request_id, 125, "launch-failed");
    }
    unlink(barrier);
    started = monotonic_milliseconds();

    while (!child_exited || stdout_open || stderr_open) {
        struct pollfd descriptors[3];
        int poll_result;
        int stdout_result = 0;
        int stderr_result = 0;

        descriptors[0].fd = stdout_open ? stdout_pipe[0] : -1;
        descriptors[0].events = POLLIN | POLLHUP;
        descriptors[0].revents = 0;
        descriptors[1].fd = stderr_open ? stderr_pipe[0] : -1;
        descriptors[1].events = POLLIN | POLLHUP;
        descriptors[1].revents = 0;
        /* After a reset the old session is gone; later records belong to the main loop. */
        descriptors[2].fd = session_lost ? -1 : session->fd;
        descriptors[2].events = POLLIN;
        descriptors[2].revents = 0;
        poll_result = poll(descriptors, 3, 25);
        if (poll_result < 0 && errno != EINTR) {
            terminate_workload(config, child);
        }
        if (!session_lost && descriptors[2].revents != 0) {
            switch (handle_exec_control(session, request_id)) {
            case EXEC_CONTROL_NONE:
                break;
            case EXEC_CONTROL_CANCEL:
                if (!child_exited && !cancelled) {
                    cancelled = 1;
                    terminate_workload(config, child);
                }
                break;
            case EXEC_CONTROL_SESSION_LOST:
                session_lost = 1;
                terminate_workload(config, child);
                break;
            case EXEC_CONTROL_FAILED:
                terminate_workload(config, child);
                waitpid(child, NULL, 0);
                if (stdout_open) {
                    close(stdout_pipe[0]);
                }
                if (stderr_open) {
                    close(stderr_pipe[0]);
                }
                return -1;
            }
        }
        if (stdout_open && descriptors[0].revents != 0) {
            stdout_result = session_lost
                                ? drain_output(stdout_pipe[0])
                                : stream_output(
                                      session,
                                      request_id,
                                      stdout_pipe[0],
                                      APP_STDOUT,
                                      &output_bytes);
        }
        if (stderr_open && descriptors[1].revents != 0) {
            stderr_result = session_lost
                                ? drain_output(stderr_pipe[0])
                                : stream_output(
                                      session,
                                      request_id,
                                      stderr_pipe[0],
                                      APP_STDERR,
                                      &output_bytes);
        }
        if (stdout_result == 1) {
            close(stdout_pipe[0]);
            stdout_open = 0;
        }
        if (stderr_result == 1) {
            close(stderr_pipe[0]);
            stderr_open = 0;
        }
        if (stdout_result < 0 || stderr_result < 0) {
            output_limited = stdout_result == -2 || stderr_result == -2;
            terminate_workload(config, child);
        }
        reap_children(child, &child_exited, &wait_status);
        /* Processes the workload left behind must not hold its output open. */
        if (child_exited && config->direct && !stragglers_killed &&
            (stdout_open || stderr_open)) {
            stragglers_killed = 1;
            (void)kill_exec_cgroup();
        }
        if (!child_exited && timeout_ms != 0 &&
            monotonic_milliseconds() - started >= timeout_ms) {
            timed_out = 1;
            terminate_workload(config, child);
        }
        if ((timed_out || output_limited || cancelled || session_lost) && !child_exited) {
            if (waitpid(child, &wait_status, 0) == child) {
                child_exited = 1;
            }
        }
    }

    if (config->direct && settle_exec_cgroup() != 0) {
        portb_error("exec-cgroup-settle", errno);
        return session_lost
                   ? 0
                   : send_app_error(session, request_id, 125, "containment-failed");
    }
    if (session_lost) {
        /* Nothing may be sent for the abandoned exec once the new epoch is acknowledged. */
        return 0;
    }
    if (cancelled) {
        return send_app_frame(
            session, APP_EXIT, request_id, 128 + SIGKILL, "cancelled", 9);
    }
    if (timed_out) {
        return send_app_frame(
            session, APP_EXIT, request_id, 124, "timeout", 7);
    }
    if (output_limited) {
        return send_app_frame(
            session, APP_EXIT, request_id, 125, "output-limit", 12);
    }
    if (WIFEXITED(wait_status)) {
        return send_app_frame(
            session,
            APP_EXIT,
            request_id,
            WEXITSTATUS(wait_status),
            "exit",
            4);
    }
    if (WIFSIGNALED(wait_status)) {
        int status = 128 + WTERMSIG(wait_status);
        return send_app_frame(
            session, APP_EXIT, request_id, status, "signal", 6);
    }
    return send_app_frame(
        session, APP_EXIT, request_id, 125, "failed", 6);
}

/* Direct mode alone sets up host mappings and a cgroup for each workload. */
static uint32_t agent_features(const struct agent_config *config)
{
    uint32_t features = FEATURE_CANCEL | FEATURE_WORKLOAD_ACCOUNT;

    if (config->direct) {
        features |= FEATURE_HOST_MAPPINGS | FEATURE_EXEC_CGROUP;
    }
    return features;
}

static int handle_data_record(
    struct control_session *session,
    const struct agent_config *config,
    const struct outer_record *record)
{
    struct app_request request;
    char **workload_argv = NULL;
    uint8_t features[4];
    struct exec_config exec_config = {0};
    uint32_t timeout_ms = 0;
    int result;

    if (record->sequence != session->host_sequence) {
        portb_error("data-sequence", (int)record->sequence);
        return -1;
    }
    if (record->epoch != session->epoch ||
        memcmp(record->instance_id, session->instance_id, 16) != 0) {
        portb_error("data-identity", (int)record->epoch);
        return -1;
    }
    if (parse_app_request(record->payload, record->payload_len, &request) != 0) {
        portb_error("data-application", (int)record->payload_len);
        return -1;
    }
    ++session->host_sequence;
    if (send_credit(session, record->payload_len) != 0) {
        portb_error("data-credit", (int)record->payload_len);
        return -1;
    }

    switch (request.kind) {
    case APP_PING:
        if (request.payload_len != 0) {
            portb_error("ping-payload", (int)request.payload_len);
            return send_app_error(
                session, request.request_id, 22, "invalid-request");
        }
        result = send_app_frame(
            session, APP_READY, request.request_id, 0, NULL, 0);
        if (result != 0) {
            portb_error("ping-ready", errno);
        }
        return result;
    case APP_EXEC:
        if (decode_exec_payload(
                request.payload,
                request.payload_len,
                &timeout_ms,
                &workload_argv,
                &exec_config) != 0) {
            return send_app_error(
                session, request.request_id, 22, "invalid-request");
        }
        result = run_exec(
            session,
            config,
            request.request_id,
            timeout_ms,
            workload_argv,
            &exec_config);
        free_arguments(workload_argv);
        free_exec_config(&exec_config);
        return result;
    case APP_CANCEL:
        /* The targeted workload already finished and reported its outcome. */
        return 0;
    case APP_FEATURES:
        if (request.payload_len != 0) {
            return send_app_error(
                session, request.request_id, 22, "invalid-request");
        }
        write_u32(features, agent_features(config));
        return send_app_frame(
            session,
            APP_READY,
            request.request_id,
            0,
            features,
            sizeof(features));
    case APP_STOP:
        if (request.payload_len != 0 ||
            send_app_frame(
                session, APP_STOPPED, request.request_id, 0, NULL, 0) != 0) {
            return -1;
        }
        tcdrain(session->fd);
        return AGENT_STOPPED;
    default:
        return send_app_error(
            session, request.request_id, 95, "unsupported-operation");
    }
}

static int run_agent(
    struct control_session *session,
    const struct agent_config *config)
{
    struct outer_record record;

    if (send_guest_attach(session->fd) != 0) {
        return -1;
    }
    for (;;) {
        if (read_outer_record(session->fd, &record) != 0) {
            portb_error("outer-read", errno);
            return -1;
        }
        if (record.type == OUTER_RESET) {
            if (acknowledge_reset(session, &record) != 0) {
                portb_error("reset-ack", errno);
                free_outer_record(&record);
                return -1;
            }
        } else if (record.type == OUTER_DATA) {
            int result = handle_data_record(session, config, &record);

            if (result == AGENT_STOPPED) {
                free_outer_record(&record);
                return AGENT_STOPPED;
            }
            if (result != 0) {
                portb_error("data", errno);
                free_outer_record(&record);
                return -1;
            }
        } else {
            portb_error("outer-type", record.type);
            free_outer_record(&record);
            return -1;
        }
        free_outer_record(&record);
    }
}

/*
 * Direct mode runs as PID 1 and powers the VM off itself. In sandbox mode the
 * init agent supervises this process and unmounts the live share, overlay,
 * layers, and scratch before it powers the VM off.
 */
static int finish_agent(const struct agent_config *config, int status)
{
    char code[12];

    if (config->direct) {
        snprintf(code, sizeof(code), "%d", status);
        execl("/sbin/nvx-exit", "nvx-exit", code, (char *)NULL);
    }
    return status;
}

int main(int argc, char **argv)
{
    struct control_session session = {0};
    struct agent_config config;
    int result;

    if (argc >= 2 && strcmp(argv[1], "--exec-config-fd") == 0) {
        return launch_workload(argc, argv);
    }
    if (argc != 8) {
        return 125;
    }
    config.rootfs = argv[2];
    config.hostname = argv[3];
    config.uid = argv[4];
    config.gid = argv[5];
    config.user = argv[6];
    config.home = argv[7];
    config.direct = strcmp(config.rootfs, "-") == 0;
    session.fd = open_control_tty(argv[1]);
    if (session.fd < 0) {
        int status = errno;
        portb_error("control-open", status);
        dprintf(
            STDERR_FILENO,
            "NVX-MANAGED-ERROR: stage=control-open status=%d\n",
            status);
        return finish_agent(&config, 125);
    }
    if (config.direct && setup_host_mappings() != 0) {
        int status = errno;
        portb_error("host-mappings", status);
        dprintf(
            STDERR_FILENO,
            "NVX-MANAGED-ERROR: stage=host-mappings status=%d\n",
            status);
        close(session.fd);
        return finish_agent(&config, 125);
    }
    if (configure_control_tty(session.fd) != 0) {
        int status = errno;
        portb_error("control-tty", status);
        dprintf(
            STDERR_FILENO,
            "NVX-MANAGED-ERROR: stage=control-tty status=%d\n",
            status);
        close(session.fd);
        return finish_agent(&config, 125);
    }
    result = run_agent(&session, &config);
    if (result == AGENT_STOPPED) {
        close(session.fd);
        return finish_agent(&config, 0);
    }
    if (result != 0) {
        int status = errno;
        portb_error("control-session", status);
        dprintf(
            STDERR_FILENO,
            "NVX-MANAGED-ERROR: stage=control-session status=%d\n",
            status);
        if (session.fd >= 0) {
            close(session.fd);
        }
        return finish_agent(&config, 125);
    }
    return 0;
}
