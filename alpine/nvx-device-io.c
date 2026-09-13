typedef unsigned long u64;

#ifdef __x86_64__
#define SYS_READ 0
#define SYS_WRITE 1
#define SYS_CLOSE 3
#define SYS_POLL 7
#define SYS_PREAD64 17
#define SYS_PWRITE64 18
#define SYS_SOCKET 41
#define SYS_CONNECT 42
#define SYS_FSYNC 74
#define SYS_FTRUNCATE 77
#define SYS_CLOCK_GETTIME 228
#define SYS_OPENAT 257
#define SYS_EXIT_GROUP 231
#define O_DIRECT 16384
#define O_LARGEFILE 0
#elif defined(__aarch64__)
#define SYS_FTRUNCATE 46
#define SYS_OPENAT 56
#define SYS_CLOSE 57
#define SYS_READ 63
#define SYS_WRITE 64
#define SYS_PREAD64 67
#define SYS_PWRITE64 68
#define SYS_PPOLL 73
#define SYS_FSYNC 82
#define SYS_EXIT_GROUP 94
#define SYS_CLOCK_GETTIME 113
#define SYS_SOCKET 198
#define SYS_CONNECT 203
#define O_DIRECT 65536
#define O_LARGEFILE 131072
#else
#error unsupported architecture
#endif

#define AT_FDCWD -100
#define O_RDONLY 0
#define O_RDWR 2
#define O_CREAT 64
#define O_CLOEXEC 524288
#define AF_INET 2
#define SOCK_DGRAM 2
#define SOCK_CLOEXEC 524288
#define POLLIN 1
#define CLOCK_MONOTONIC 1

struct timespec {
    long seconds;
    long nanoseconds;
};

struct sockaddr_in {
    unsigned short family;
    unsigned short port;
    unsigned int address;
    unsigned char padding[8];
};

struct pollfd {
    int fd;
    short events;
    short returned_events;
};

static unsigned char io_buffer[4096] __attribute__((aligned(4096)));
static char output[384];
static u64 output_length;

static long syscall6(
    long number,
    long argument1,
    long argument2,
    long argument3,
    long argument4,
    long argument5,
    long argument6
) {
#ifdef __x86_64__
    register long register10 __asm__("r10") = argument4;
    register long register8 __asm__("r8") = argument5;
    register long register9 __asm__("r9") = argument6;
    long result;
    __asm__ volatile(
        "syscall"
        : "=a"(result)
        : "a"(number), "D"(argument1), "S"(argument2), "d"(argument3),
          "r"(register10), "r"(register8), "r"(register9)
        : "rcx", "r11", "memory"
    );
    return result;
#else
    register long register0 __asm__("x0") = argument1;
    register long register1 __asm__("x1") = argument2;
    register long register2 __asm__("x2") = argument3;
    register long register3 __asm__("x3") = argument4;
    register long register4 __asm__("x4") = argument5;
    register long register5 __asm__("x5") = argument6;
    register long register8 __asm__("x8") = number;
    __asm__ volatile(
        "svc 0"
        : "+r"(register0)
        : "r"(register1), "r"(register2), "r"(register3), "r"(register4),
          "r"(register5), "r"(register8)
        : "memory"
    );
    return register0;
#endif
}

static long syscall4(long number, long a1, long a2, long a3, long a4) {
    return syscall6(number, a1, a2, a3, a4, 0, 0);
}

static long syscall3(long number, long a1, long a2, long a3) {
    return syscall6(number, a1, a2, a3, 0, 0, 0);
}

static long syscall2(long number, long a1, long a2) {
    return syscall6(number, a1, a2, 0, 0, 0, 0);
}

static long syscall1(long number, long a1) {
    return syscall6(number, a1, 0, 0, 0, 0, 0);
}

static long poll_one(struct pollfd *descriptor, long timeout_ms) {
#ifdef __x86_64__
    return syscall3(SYS_POLL, (long)descriptor, 1, timeout_ms);
#else
    struct timespec timeout = {
        .seconds = timeout_ms / 1000,
        .nanoseconds = (timeout_ms % 1000) * 1000000,
    };
    return syscall6(SYS_PPOLL, (long)descriptor, 1, (long)&timeout, 0, 0, 0);
#endif
}

static int strings_equal(const char *left, const char *right) {
    while (*left && *left == *right) {
        left++;
        right++;
    }
    return *left == *right;
}

static int parse_u64(const char *text, u64 *value) {
    u64 parsed = 0;
    if (!*text) {
        return -1;
    }
    while (*text) {
        if (*text < '0' || *text > '9') {
            return -1;
        }
        parsed = parsed * 10 + (u64)(*text - '0');
        text++;
    }
    *value = parsed;
    return 0;
}

static int parse_ipv4(const char *text, unsigned int *address) {
    unsigned int octets[4] = {0, 0, 0, 0};
    int index = 0;
    int digits = 0;
    while (*text) {
        if (*text == '.') {
            if (!digits || index == 3) {
                return -1;
            }
            index++;
            digits = 0;
        } else if (*text >= '0' && *text <= '9') {
            octets[index] = octets[index] * 10 + (unsigned int)(*text - '0');
            if (octets[index] > 255) {
                return -1;
            }
            digits++;
        } else {
            return -1;
        }
        text++;
    }
    if (index != 3 || !digits) {
        return -1;
    }
    *address = octets[0] | (octets[1] << 8) | (octets[2] << 16) |
        (octets[3] << 24);
    return 0;
}

static unsigned short network_u16(unsigned short value) {
    return (unsigned short)((value << 8) | (value >> 8));
}

static u64 monotonic_ns(void) {
    struct timespec value;
    if (syscall2(SYS_CLOCK_GETTIME, CLOCK_MONOTONIC, (long)&value) < 0) {
        return 0;
    }
    return (u64)value.seconds * 1000000000UL + (u64)value.nanoseconds;
}

static void append_text(const char *text) {
    while (*text && output_length < sizeof(output)) {
        output[output_length++] = *text++;
    }
}

static void append_u64(u64 value) {
    char digits[24];
    int length = 0;
    do {
        digits[length++] = (char)('0' + value % 10);
        value /= 10;
    } while (value);
    while (length && output_length < sizeof(output)) {
        output[output_length++] = digits[--length];
    }
}

static int write_all(int descriptor, const void *data, u64 length) {
    const char *current = data;
    while (length) {
        long written = syscall3(SYS_WRITE, descriptor, (long)current, (long)length);
        if (written == -4) {
            continue;
        }
        if (written <= 0) {
            return -1;
        }
        current += written;
        length -= (u64)written;
    }
    return 0;
}

static int emit_result(
    const char *device,
    const char *operation,
    u64 operations,
    u64 bytes_per_operation,
    u64 elapsed_ns
) {
    output_length = 0;
    append_text("{\"device\":\"");
    append_text(device);
    append_text("\",\"operation\":\"");
    append_text(operation);
    append_text("\",\"operations\":");
    append_u64(operations);
    append_text(",\"bytes_per_operation\":");
    append_u64(bytes_per_operation);
    append_text(",\"elapsed_ns\":");
    append_u64(elapsed_ns);
    append_text("}\n");
    return write_all(1, output, output_length);
}

static int run_file(int argc, char **argv) {
    u64 duration_ms;
    u64 size_bytes;
    int writing;
    int direct;
    int create;
    int flags;
    long descriptor;
    u64 started;
    u64 ended;
    u64 operations = 0;
    u64 random = 0x9e3779b97f4a7c15UL;
    u64 block_count;
    int index;

    if (argc != 8 || parse_u64(argv[4], &duration_ms) ||
        parse_u64(argv[5], &size_bytes) || duration_ms == 0 ||
        size_bytes < sizeof(io_buffer)) {
        return 2;
    }
    writing = strings_equal(argv[3], "write");
    if (!writing && !strings_equal(argv[3], "read")) {
        return 2;
    }
    direct = strings_equal(argv[6], "direct");
    create = strings_equal(argv[7], "create");
    if ((!direct && !strings_equal(argv[6], "buffered")) ||
        (!create && !strings_equal(argv[7], "existing"))) {
        return 2;
    }
    flags = (writing ? O_RDWR : O_RDONLY) | O_CLOEXEC | O_LARGEFILE;
    if (direct) {
        flags |= O_DIRECT;
    }
    if (create) {
        flags |= O_CREAT;
    }
    descriptor = syscall4(SYS_OPENAT, AT_FDCWD, (long)argv[2], flags, 0600);
    if (descriptor < 0) {
        return 3;
    }
    if (create && syscall2(SYS_FTRUNCATE, descriptor, (long)size_bytes) < 0) {
        syscall1(SYS_CLOSE, descriptor);
        return 3;
    }
    for (index = 0; index < (int)sizeof(io_buffer); index++) {
        io_buffer[index] = (unsigned char)(index * 31 + 17);
    }
    block_count = size_bytes / sizeof(io_buffer);
    started = monotonic_ns();
    if (!started) {
        syscall1(SYS_CLOSE, descriptor);
        return 3;
    }
    ended = started;
    while (ended - started < duration_ms * 1000000UL) {
        u64 offset;
        long transferred;
        random = random * 6364136223846793005UL + 1442695040888963407UL;
        offset = (random % block_count) * sizeof(io_buffer);
        transferred = writing
            ? syscall4(SYS_PWRITE64, descriptor, (long)io_buffer, sizeof(io_buffer), offset)
            : syscall4(SYS_PREAD64, descriptor, (long)io_buffer, sizeof(io_buffer), offset);
        if (transferred != (long)sizeof(io_buffer)) {
            syscall1(SYS_CLOSE, descriptor);
            return 4;
        }
        operations++;
        if ((operations & 63) == 0) {
            ended = monotonic_ns();
        }
    }
    ended = monotonic_ns();
    if (writing) {
        syscall1(SYS_FSYNC, descriptor);
    }
    syscall1(SYS_CLOSE, descriptor);
    return emit_result("file", argv[3], operations, sizeof(io_buffer), ended - started);
}

static int run_network(int argc, char **argv) {
    u64 port;
    u64 duration_ms;
    unsigned int address;
    struct sockaddr_in peer = {0};
    struct pollfd poll_descriptor;
    long descriptor;
    u64 started;
    u64 ended;
    u64 operations = 0;
    int index;

    if (argc != 5 || parse_ipv4(argv[2], &address) ||
        parse_u64(argv[3], &port) || port == 0 || port > 65535 ||
        parse_u64(argv[4], &duration_ms) || duration_ms == 0) {
        return 2;
    }
    descriptor = syscall3(SYS_SOCKET, AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (descriptor < 0) {
        return 3;
    }
    peer.family = AF_INET;
    peer.port = network_u16((unsigned short)port);
    peer.address = address;
    if (syscall3(SYS_CONNECT, descriptor, (long)&peer, sizeof(peer)) < 0) {
        syscall1(SYS_CLOSE, descriptor);
        return 3;
    }
    for (index = 0; index < 64; index++) {
        io_buffer[index] = (unsigned char)(index + 1);
    }
    poll_descriptor.fd = (int)descriptor;
    poll_descriptor.events = POLLIN;
    started = monotonic_ns();
    if (!started) {
        syscall1(SYS_CLOSE, descriptor);
        return 3;
    }
    ended = started;
    while (ended - started < duration_ms * 1000000UL) {
        long received;
        if (syscall3(SYS_WRITE, descriptor, (long)io_buffer, 64) != 64) {
            syscall1(SYS_CLOSE, descriptor);
            return 4;
        }
        poll_descriptor.returned_events = 0;
        if (poll_one(&poll_descriptor, 100) > 0 &&
            (poll_descriptor.returned_events & POLLIN)) {
            received = syscall3(SYS_READ, descriptor, (long)io_buffer, 64);
            if (received == 64) {
                operations++;
            }
        }
        ended = monotonic_ns();
    }
    syscall1(SYS_CLOSE, descriptor);
    return emit_result("network", "roundtrip", operations, 64, ended - started);
}

static int benchmark_main(int argc, char **argv) {
    if (argc > 1 && strings_equal(argv[1], "file")) {
        return run_file(argc, argv);
    }
    if (argc > 1 && strings_equal(argv[1], "network")) {
        return run_network(argc, argv);
    }
    return 2;
}

void nvx_device_io_start(long *stack) {
    int status = benchmark_main((int)stack[0], (char **)&stack[1]);
    syscall1(SYS_EXIT_GROUP, status);
    for (;;) {
    }
}

#ifdef __x86_64__
__asm__(
    ".global _start\n"
    ".type _start,@function\n"
    "_start:\n"
    "mov %rsp,%rdi\n"
    "andq $-16,%rsp\n"
    "call nvx_device_io_start\n"
    "ud2\n"
);
#else
__asm__(
    ".global _start\n"
    ".type _start,%function\n"
    "_start:\n"
    "mov x0, sp\n"
    "bl nvx_device_io_start\n"
    "brk #0\n"
);
#endif