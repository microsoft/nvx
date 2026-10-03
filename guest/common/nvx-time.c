// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// nvx-time: the guest time component of NVX time ABI v1.
//
// doc/design/time-abi.md ("Guest obligations") is the contract this helper
// enforces. It runs the conformance checks, the violation watcher and the
// wall-clock discipline (one daemon), and the snapshot agent's time steps. It
// prints nothing on success: checks record their result in the state file,
// and only violation events reach the console.
//
//   boot                       initial clock step (C12), then the other boot
//                              checks asynchronously, which start the daemon
//   check --phase PHASE [--new-cpus LIST]
//                              capture or restore checks, for diagnosis
//   pre-capture                capture checks and watchdog suppression
//   capture REQUEST GENERATION_ID ENTROPY_FILE [--finish]
//                              capture request, then restore repair
//   cancel-capture             undo pre-capture after a rejected capture
//   restore-finish [--ack] [--new-cpus LIST]
//                              acknowledgement, then the daemon's restore
//                              checks and release
//   status                     print every recorded check and the runtime
//                              state, after waiting for pending checks
//   sample                     print one host time sample
//   generation-id              print the VM generation ID
//   exhaustive                 the CI-only exhaustive check
//   test NAME ARGUMENTS...     pure decoders and checks, for unit tests
//
// --report-only, or nvx_time_abi=report-only on the kernel command line,
// reports failures without powering the VM off and prints NVX-TIME-REPORT
// instead of NVX-TIME-ABI, so that its output never passes for a conformant
// run. Every portb transfer uses four-byte reads: a restore packet without
// memory ranges costs 24 data-port exits.

#define _GNU_SOURCE

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/io.h>
#include <sys/klog.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/signalfd.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <sys/timex.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif

#define ARRAY_SIZE(array) (sizeof(array) / sizeof((array)[0]))

#define NSEC_PER_SEC INT64_C(1000000000)
#define NSEC_PER_MSEC INT64_C(1000000)
#define NSEC_PER_USEC INT64_C(1000)
#define MAX_CPUS 64

#define STATUS_CONFORMANCE 193
#define STATUS_VIOLATION 194
#define STATUS_REPAIR 195

#define PORTB_DATA 0xe9
#define PORTB_STATUS 0xea
#define PORTB_WINDOW 0xeb
#define PORT_SHUTDOWN 0x604
#define PORT_SNAPSHOT 0x605
#define SELECT_PACKET 0xa5
#define SELECT_GENERATION_ID 0xa6
#define SELECT_TIME_SAMPLE 0xa7
#define PORTB_PACKET_AVAILABLE 0x02
#define PORTB_GENERATION_ID_AVAILABLE 0x20
#define PORTB_TIME_WINDOW 0x40
#define SNAPSHOT_ACKNOWLEDGE 2

#define PACKET_HEADER_SIZE 32
#define PACKET_RANGE_SIZE 16
#define PACKET_ENTROPY_SIZE 64
#define PACKET_MAX_RANGES 255
#define PACKET_MAX_SIZE                                                        \
    (PACKET_HEADER_SIZE + PACKET_MAX_RANGES * PACKET_RANGE_SIZE +              \
     PACKET_ENTROPY_SIZE)
#define PACKET_DOWNTIME_UTC 0x01
#define PACKET_MEMORY_TARGET 0x02
#define PACKET_ACK_REQUIRED 0x04
#define PACKET_TEST_HOOKS 0x08
#define PACKET_KNOWN_FLAGS 0x0f
#define SAMPLE_SIZE 16
#define SAMPLE_TEST_HOOKS 0x08
#define GENERATION_ID_SIZE 16

#define RATE_DEVIATION_LIMIT INT64_C(16384000)
#define FREQUENCY_LIMIT (INT64_C(500) << 16)
#define DOWNTIME_LIMIT_NS (INT64_C(2592000) * NSEC_PER_SEC)
#define REPAIR_BOUND_NS NSEC_PER_MSEC
#define BOOT_BOUND_NS NSEC_PER_MSEC
#define POLL_BOUND_NS (50 * NSEC_PER_USEC)
#define SAMPLE_ATTEMPTS 3
#define STEP_THRESHOLD_NS (128 * NSEC_PER_MSEC)
#define SYNCHRONIZED_AGE_NS (128 * NSEC_PER_SEC)
#define FAST_PERIOD_S 16
#define SLOW_PERIOD_S 64
#define FAST_POLLS 4
#define FAST_TIME_CONSTANT 4
#define SLOW_TIME_CONSTANT 6
#define TIMER_LIST_WAIT_NS (200 * NSEC_PER_MSEC)
#define DEFAULT_STALL_TIMEOUT_S 21
// The asynchronous boot checks start 150 ms after the time ABI boot step,
// which init runs shortly before shell-ready, and restore step 13 150 ms
// after the acknowledgement: earlier, they contend with the readiness path
// while the VMM still demand-faults guest memory, and with the workload's own
// first activity after readiness. Both start at SCHED_IDLE and continue at
// normal priority 100 ms after they start, which bounds the fail-fast window
// under a CPU-bound workload.
#define DEFERRED_START_NS (150 * NSEC_PER_MSEC)
#define IDLE_BOUND_NS (100 * NSEC_PER_MSEC)
// nvx-time status and pre-capture wait at most 30 s for pending checks.
#define STATUS_WAIT_NS (30 * NSEC_PER_SEC)
#define STATUS_POLL_NS (10 * NSEC_PER_MSEC)
#define EVENT_MAX 512
#define DETAIL_MAX 384
#define TEXT_MAX 65536

#define RUN_DIR "/run/nvx"
#define TIME_DIR RUN_DIR "/time"
#define STATE_PATH TIME_DIR "/state"
#define STATE_TEMP_PATH TIME_DIR "/.state"
#define STATE_LOCK_PATH TIME_DIR "/state.lock"
#define DAEMON_PID_PATH TIME_DIR "/daemon.pid"
#define SUPPRESSION_PATH TIME_DIR "/suppression"
#define SUPPRESSION_LOCK_PATH TIME_DIR "/suppression.lock"
#define RESTORE_PATH TIME_DIR "/restore"
#define PORTB_LOCK_PATH RUN_DIR "/portb.lock"
#define RCU_SYNC_PATH RUN_DIR "/rcu-sync"

#define RCU_STALL_SUPPRESS "/sys/module/rcupdate/parameters/rcu_cpu_stall_suppress"
#define RCU_STALL_TIMEOUT "/sys/module/rcupdate/parameters/rcu_cpu_stall_timeout"
#define RCU_NORMAL "/sys/kernel/rcu_normal"
#define RCU_STALL_COUNT "/sys/kernel/rcu_stall_count"
#define SOFT_WATCHDOG "/proc/sys/kernel/soft_watchdog"
#define HUNG_TASK_TIMEOUT "/proc/sys/kernel/hung_task_timeout_secs"
#define CLOCKSOURCE_DIR "/sys/devices/system/clocksource/clocksource0"

enum phase { PHASE_BOOT, PHASE_CAPTURE, PHASE_RESTORE, PHASE_RUNTIME };

static const char *const k_phase_names[] = {"boot", "capture", "restore",
                                            "runtime"};

static bool g_report_only;
static bool g_test;
static uint32_t g_generation;
static bool g_ports;
// Set in the daemon. Its lines can arrive while a shell or workload is in the
// middle of a console line, so each one starts with a newline.
static bool g_async_output;

// ---------------------------------------------------------------------------
// Small utilities
// ---------------------------------------------------------------------------

static int64_t clock_ns(clockid_t clock)
{
    struct timespec now;

    clock_gettime(clock, &now);
    return (int64_t)now.tv_sec * NSEC_PER_SEC + now.tv_nsec;
}

// The CPU time of this process, every thread's, exited threads' included:
// what the spec's cpu_us sums.
static int64_t cpu_time_ns(void)
{
    return clock_ns(CLOCK_PROCESS_CPUTIME_ID);
}

static int write_all(int fd, const void *data, size_t size)
{
    const char *bytes = data;

    while (size > 0) {
        ssize_t count = write(fd, bytes, size);

        if (count < 0 && errno == EINTR)
            continue;
        if (count <= 0)
            return -1;
        bytes += count;
        size -= (size_t)count;
    }
    return 0;
}

static ssize_t read_bytes(const char *path, void *buffer, size_t size)
{
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    size_t total = 0;

    if (fd < 0)
        return -1;
    while (total < size) {
        ssize_t count = read(fd, (char *)buffer + total, size - total);

        if (count < 0 && errno == EINTR)
            continue;
        if (count < 0) {
            int error = errno;

            close(fd);
            errno = error;
            return -1;
        }
        if (count == 0)
            break;
        total += (size_t)count;
    }
    close(fd);
    return (ssize_t)total;
}

static ssize_t read_text(const char *path, char *buffer, size_t size)
{
    ssize_t count = read_bytes(path, buffer, size - 1);

    if (count < 0)
        return -1;
    buffer[count] = '\0';
    return count;
}

typedef void (*line_consumer)(void *context, char *line);

// Passes each line of TEXT to CONSUME, splitting TEXT in place.
static void for_each_text_line(char *text, line_consumer consume,
                               void *context)
{
    for (char *line = text; line != NULL && *line != '\0';) {
        char *next = strchr(line, '\n');

        if (next != NULL)
            *next++ = '\0';
        consume(context, line);
        line = next;
    }
}

// Streams the file at PATH into CONSUME one line at a time, so files whose
// size depends on the workload (/proc/timer_list) or on the CPU count
// (/proc/cpuinfo) are never truncated. Longer lines than the line buffer are
// cut. Returns -1 with errno set on failure.
static int for_each_file_line(const char *path, line_consumer consume,
                              void *context)
{
    char chunk[16384];
    char line[8192];
    size_t length = 0;
    int fd = open(path, O_RDONLY | O_CLOEXEC);

    if (fd < 0)
        return -1;
    for (;;) {
        ssize_t count = read(fd, chunk, sizeof(chunk));

        if (count < 0 && errno == EINTR)
            continue;
        if (count < 0) {
            int error = errno;

            close(fd);
            errno = error;
            return -1;
        }
        if (count == 0)
            break;
        for (ssize_t index = 0; index < count; index++) {
            if (chunk[index] == '\n') {
                line[length] = '\0';
                consume(context, line);
                length = 0;
            } else if (length < sizeof(line) - 1) {
                line[length++] = chunk[index];
            }
        }
    }
    close(fd);
    if (length > 0) {
        line[length] = '\0';
        consume(context, line);
    }
    return 0;
}

static int parse_int64(const char *text, int64_t *value)
{
    char *end;

    errno = 0;
    *value = strtoll(text, &end, 10);
    if (errno != 0 || end == text)
        return -1;
    while (*end == '\n' || *end == ' ')
        end++;
    return *end == '\0' ? 0 : -1;
}

static int read_int64(const char *path, int64_t *value)
{
    char text[64];

    if (read_text(path, text, sizeof(text)) < 0)
        return -1;
    return parse_int64(text, value);
}

static int write_text(const char *path, const char *text)
{
    int fd = open(path, O_WRONLY | O_CLOEXEC);
    int result;

    if (fd < 0)
        return -1;
    result = write_all(fd, text, strlen(text));
    if (close(fd) != 0)
        result = -1;
    return result;
}

static int write_int64(const char *path, int64_t value)
{
    char text[32];

    snprintf(text, sizeof(text), "%" PRId64 "\n", value);
    return write_text(path, text);
}

// Atomically replaces PATH with TEXT through a temporary file in its directory.
static int replace_file(const char *path, const char *temporary,
                        const char *text)
{
    int fd = open(temporary, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
    int result;

    if (fd < 0)
        return -1;
    result = write_all(fd, text, strlen(text));
    if (close(fd) != 0)
        result = -1;
    if (result == 0 && rename(temporary, path) != 0)
        result = -1;
    if (result != 0)
        unlink(temporary);
    return result;
}

static void make_runtime_directories(void)
{
    (void)mkdir(RUN_DIR, 0755);
    (void)mkdir(TIME_DIR, 0755);
}

static bool ends_with(const char *text, const char *suffix)
{
    size_t length = strlen(text);
    size_t suffix_length = strlen(suffix);

    return length >= suffix_length &&
           strcmp(text + length - suffix_length, suffix) == 0;
}

static uint32_t read_le32(const uint8_t *bytes)
{
    return (uint32_t)bytes[0] | (uint32_t)bytes[1] << 8 |
           (uint32_t)bytes[2] << 16 | (uint32_t)bytes[3] << 24;
}

static uint64_t read_le64(const uint8_t *bytes)
{
    return (uint64_t)read_le32(bytes) | (uint64_t)read_le32(bytes + 4) << 32;
}

static void format_hex(const uint8_t *bytes, size_t size, char *text)
{
    static const char digits[] = "0123456789abcdef";

    for (size_t index = 0; index < size; index++) {
        text[index * 2] = digits[bytes[index] >> 4];
        text[index * 2 + 1] = digits[bytes[index] & 0x0f];
    }
    text[size * 2] = '\0';
}

static int parse_hex(const char *text, uint8_t *bytes, size_t size)
{
    if (strlen(text) != size * 2)
        return -1;
    for (size_t index = 0; index < size * 2; index++) {
        char c = text[index];
        int value;

        if (c >= '0' && c <= '9')
            value = c - '0';
        else if (c >= 'a' && c <= 'f')
            value = c - 'a' + 10;
        else if (c >= 'A' && c <= 'F')
            value = c - 'A' + 10;
        else
            return -1;
        if (index % 2 == 0)
            bytes[index / 2] = (uint8_t)(value << 4);
        else
            bytes[index / 2] |= (uint8_t)value;
    }
    return 0;
}

// Parses a CPU list such as "0-3,5". Returns the number of CPUs in it.
static int parse_cpu_list(const char *text, bool *cpus)
{
    const char *cursor = text;
    int count = 0;

    memset(cpus, 0, sizeof(bool) * MAX_CPUS);
    while (*cursor != '\0' && *cursor != '\n') {
        char *end;
        long first = strtol(cursor, &end, 10);
        long last = first;

        if (end == cursor || first < 0)
            return -1;
        cursor = end;
        if (*cursor == '-') {
            last = strtol(cursor + 1, &end, 10);
            if (end == cursor + 1 || last < first)
                return -1;
            cursor = end;
        }
        if (last >= MAX_CPUS)
            return -1;
        for (long cpu = first; cpu <= last; cpu++) {
            if (!cpus[cpu]) {
                cpus[cpu] = true;
                count++;
            }
        }
        if (*cursor == ',')
            cursor++;
        else if (*cursor != '\0' && *cursor != '\n')
            return -1;
    }
    return count;
}

// ---------------------------------------------------------------------------
// Console output, events, and power-off
// ---------------------------------------------------------------------------

static const char *abi_prefix(void)
{
    return g_report_only ? "NVX-TIME-REPORT" : "NVX-TIME-ABI";
}

// Escapes TEXT into OUT, which holds LIMIT characters plus a terminator. An
// escape sequence that would not fit is dropped whole, never split.
static void escape_text(const char *text, char *out, size_t limit)
{
    size_t length = 0;

    for (const unsigned char *c = (const unsigned char *)text; *c != '\0';
         c++) {
        char piece[5];
        size_t size;

        if (*c == '"' || *c == '\\') {
            piece[0] = '\\';
            piece[1] = (char)*c;
            size = 2;
        } else if (*c < 0x20 || *c > 0x7e) {
            snprintf(piece, sizeof(piece), "\\x%02x", *c);
            size = 4;
        } else {
            piece[0] = (char)*c;
            size = 1;
        }
        if (length + size > limit)
            break;
        memcpy(out + length, piece, size);
        length += size;
    }
    out[length] = '\0';
}

static void console_write(const char *line)
{
    char buffer[EVENT_MAX + 128];
    int fd = g_test ? -1 : open("/dev/console", O_WRONLY | O_NOCTTY | O_CLOEXEC);

    if (g_async_output) {
        snprintf(buffer, sizeof(buffer), "\n%s", line);
        line = buffer;
    }
    if (fd < 0) {
        (void)write_all(g_test ? STDOUT_FILENO : STDERR_FILENO, line,
                        strlen(line));
        return;
    }
    (void)write_all(fd, line, strlen(line));
    close(fd);
}

static int enable_ports(void)
{
    if (g_ports)
        return 0;
    if (g_test) {
        errno = EPERM;
        return -1;
    }
    // A four-byte read at port P needs permission for P to P + 3, so the
    // reads at 0xe9 and 0xeb cover 0xe9 to 0xee.
    if (ioperm(PORTB_DATA, PORTB_WINDOW + 4 - PORTB_DATA, 1) != 0 ||
        ioperm(PORT_SHUTDOWN, 2, 1) != 0)
        return -1;
    g_ports = true;
    return 0;
}

// Reads SIZE bytes from PORT four bytes per exit; the device zero-fills
// past the end of the selected record. A string read (rep insl) would take one
// exit, but OpenVMM's MSHV and WHP emulation raises #GP for it in user mode.
static void portb_read(uint16_t port, uint8_t *buffer, size_t size)
{
    for (size_t offset = 0; offset < size; offset += 4) {
        uint32_t value = inl(port);
        size_t count = size - offset < 4 ? size - offset : 4;

        memcpy(buffer + offset, &value, count);
    }
}

static int lock_file(const char *path)
{
    int fd;

    make_runtime_directories();
    fd = open(path, O_RDWR | O_CREAT | O_CLOEXEC, 0600);
    if (fd < 0)
        return -1;
    while (flock(fd, LOCK_EX) != 0) {
        if (errno != EINTR) {
            close(fd);
            return -1;
        }
    }
    return fd;
}

static void power_off(int status)
{
    fflush(NULL);
    if (enable_ports() == 0) {
        outb((unsigned char)status, PORT_SHUTDOWN);
    } else {
        int fd = open("/dev/port", O_WRONLY | O_CLOEXEC);
        unsigned char byte = (unsigned char)status;

        if (fd >= 0) {
            ssize_t written = pwrite(fd, &byte, 1, PORT_SHUTDOWN);

            (void)written;
            close(fd);
        }
    }
    for (;;)
        pause();
}

static void format_event(char *line, const char *code, const char *source,
                         enum phase phase, uint32_t generation,
                         int64_t boottime_ns, const char *detail)
{
    int prefix = snprintf(line, EVENT_MAX + 1,
                          "%s-VIOLATION: v=1 code=%s source=%s phase=%s "
                          "generation=%" PRIu32 " boottime_ns=%" PRId64
                          " detail=\"",
                          abi_prefix(), code, source, k_phase_names[phase],
                          generation, boottime_ns);

    escape_text(detail, line + prefix, EVENT_MAX - (size_t)prefix - 2);
    strcat(line, "\"\n");
}

static void count_violation(void);

// Writes a violation event to the kernel log, the portb data port, and the
// console. Consumers treat identical lines as one event.
static void emit_event(const char *code, const char *source, enum phase phase,
                       const char *detail)
{
    char line[EVENT_MAX + 1];
    char record[EVENT_MAX + 8];
    int fd;

    format_event(line, code, source, phase, g_generation,
                 clock_ns(CLOCK_BOOTTIME), detail);
    if (g_test) {
        console_write(line);
        return;
    }
    fd = open("/dev/kmsg", O_WRONLY | O_CLOEXEC);
    if (fd >= 0) {
        snprintf(record, sizeof(record), "<2>%s", line);
        (void)write_all(fd, record, strlen(record));
        close(fd);
    }
    if (enable_ports() == 0) {
        if (g_async_output)
            outb('\n', PORTB_DATA);
        for (const char *c = line; *c != '\0'; c++)
            outb((unsigned char)*c, PORTB_DATA);
    }
    console_write(line);
    count_violation();
}

// Reports a fatal time-ABI failure. In report-only mode it returns, and the
// caller abandons the failed step.
static void fail_fatal(int status, const char *code, const char *source,
                       enum phase phase, const char *format, ...)
    __attribute__((format(printf, 5, 6)));

static void fail_fatal(int status, const char *code, const char *source,
                       enum phase phase, const char *format, ...)
{
    char detail[DETAIL_MAX];
    va_list arguments;

    va_start(arguments, format);
    vsnprintf(detail, sizeof(detail), format, arguments);
    va_end(arguments);
    emit_event(code, source, phase, detail);
    if (!g_report_only)
        power_off(status);
}

// ---------------------------------------------------------------------------
// Published state (/run/nvx/time/state) and private records
// ---------------------------------------------------------------------------

// A conformance check's record: STATUS is empty until the phase first runs,
// then "pending" while its check runs, then "ok" (a failed check powers the
// guest off; report-only mode records "fail" instead). ELAPSED_US is the
// check's wall time and CPU_US its CPU time over every process that ran it.
// GENERATION is g when the check ran; the boot check's is 0. FAILURES is the
// check's failure count, kept in report-only mode only, and -1 otherwise.
struct check_state {
    char status[16];
    uint64_t cpus;
    int64_t elapsed_us;
    int64_t cpu_us;
    uint64_t generation;
    int64_t failures;
};

// The phases with a check record, indexed by enum phase.
#define CHECK_PHASES 3

struct time_state {
    uint64_t generation;
    struct check_state checks[CHECK_PHASES];
    uint64_t tsc_hz;
    uint64_t lapic_hz;
    uint64_t discontinuities;
    char last_discontinuity[16];
    int64_t last_step_ns;
    int64_t last_step_realtime_ns;
    uint64_t last_downtime_ns;
    char last_downtime_source[16];
    uint64_t synchronized;
    int64_t offset_ns;
    int64_t uncertainty_ns;
    int64_t frequency_ppb;
    uint64_t samples;
    uint64_t rejected_samples;
    char last_sample_error[24];
    uint64_t violations;
};

// A fresh state is published at the start of the boot checks, which are then
// pending; a state without a boot record also counts as pending.
static void state_init(struct time_state *state)
{
    memset(state, 0, sizeof(*state));
    for (int phase = 0; phase < CHECK_PHASES; phase++)
        state->checks[phase].failures = -1;
    snprintf(state->checks[PHASE_BOOT].status,
             sizeof(state->checks[PHASE_BOOT].status), "pending");
    snprintf(state->last_discontinuity, sizeof(state->last_discontinuity),
             "none");
    snprintf(state->last_downtime_source, sizeof(state->last_downtime_source),
             "none");
    snprintf(state->last_sample_error, sizeof(state->last_sample_error),
             "none");
}

static void copy_word(char *destination, size_t size, const char *value)
{
    snprintf(destination, size, "%.*s", (int)strcspn(value, "\n"), value);
}

// Parses key=value lines. Unknown keys are ignored; a malformed number in a
// known numeric key fails the parse.
static int state_parse(char *text, struct time_state *state)
{
    struct check_state *boot = &state->checks[PHASE_BOOT];
    struct check_state *capture = &state->checks[PHASE_CAPTURE];
    struct check_state *restore = &state->checks[PHASE_RESTORE];
    struct {
        const char *key;
        uint64_t *unsigned_value;
        int64_t *signed_value;
    } const numbers[] = {
        {"generation", &state->generation, NULL},
        {"boot_cpus", &boot->cpus, NULL},
        {"capture_cpus", &capture->cpus, NULL},
        {"restore_cpus", &restore->cpus, NULL},
        {"boot_elapsed_us", NULL, &boot->elapsed_us},
        {"capture_elapsed_us", NULL, &capture->elapsed_us},
        {"restore_elapsed_us", NULL, &restore->elapsed_us},
        {"boot_cpu_us", NULL, &boot->cpu_us},
        {"capture_cpu_us", NULL, &capture->cpu_us},
        {"restore_cpu_us", NULL, &restore->cpu_us},
        {"capture_generation", &capture->generation, NULL},
        {"restore_generation", &restore->generation, NULL},
        {"boot_failures", NULL, &boot->failures},
        {"capture_failures", NULL, &capture->failures},
        {"restore_failures", NULL, &restore->failures},
        {"tsc_hz", &state->tsc_hz, NULL},
        {"lapic_hz", &state->lapic_hz, NULL},
        {"discontinuities", &state->discontinuities, NULL},
        {"last_step_ns", NULL, &state->last_step_ns},
        {"last_step_realtime_ns", NULL, &state->last_step_realtime_ns},
        {"last_downtime_ns", &state->last_downtime_ns, NULL},
        {"synchronized", &state->synchronized, NULL},
        {"offset_ns", NULL, &state->offset_ns},
        {"uncertainty_ns", NULL, &state->uncertainty_ns},
        {"frequency_ppb", NULL, &state->frequency_ppb},
        {"samples", &state->samples, NULL},
        {"rejected_samples", &state->rejected_samples, NULL},
        {"violations", &state->violations, NULL},
    };
    struct {
        const char *key;
        char *value;
        size_t size;
    } const words[] = {
        {"boot_status", boot->status, sizeof(boot->status)},
        {"capture_status", capture->status, sizeof(capture->status)},
        {"restore_status", restore->status, sizeof(restore->status)},
        {"last_discontinuity", state->last_discontinuity,
         sizeof(state->last_discontinuity)},
        {"last_downtime_source", state->last_downtime_source,
         sizeof(state->last_downtime_source)},
        {"last_sample_error", state->last_sample_error,
         sizeof(state->last_sample_error)},
    };

    state_init(state);
    for (char *line = text; line != NULL && *line != '\0';) {
        char *next = strchr(line, '\n');
        char *value = strchr(line, '=');

        if (next != NULL)
            *next++ = '\0';
        if (value != NULL) {
            *value++ = '\0';
            for (size_t index = 0; index < ARRAY_SIZE(words); index++) {
                if (strcmp(line, words[index].key) == 0)
                    copy_word(words[index].value, words[index].size, value);
            }
            for (size_t index = 0; index < ARRAY_SIZE(numbers); index++) {
                int64_t number;

                if (strcmp(line, numbers[index].key) != 0)
                    continue;
                if (parse_int64(value, &number) != 0)
                    return -1;
                if (numbers[index].unsigned_value != NULL)
                    *numbers[index].unsigned_value = (uint64_t)number;
                else
                    *numbers[index].signed_value = number;
            }
        }
        line = next;
    }
    return 0;
}

// Appends a formatted line to TEXT at *LENGTH, if it fits.
static void append_text(char *text, size_t size, size_t *length,
                        const char *format, ...)
    __attribute__((format(printf, 4, 5)));

static void append_text(char *text, size_t size, size_t *length,
                        const char *format, ...)
{
    va_list arguments;
    int written;

    if (*length >= size)
        return;
    va_start(arguments, format);
    written = vsnprintf(text + *length, size - *length, format, arguments);
    va_end(arguments);
    if (written > 0)
        *length += (size_t)written;
}

// Formats the state in the order of the spec's state file table; the keys of
// a phase whose check never ran are absent.
static void state_format(const struct time_state *state, char *text,
                         size_t size)
{
    size_t length = 0;

    append_text(text, size, &length, "version=1\ngeneration=%" PRIu64 "\n",
                state->generation);
    for (int phase = 0; phase < CHECK_PHASES; phase++) {
        if (state->checks[phase].status[0] != '\0')
            append_text(text, size, &length, "%s_status=%s\n",
                        k_phase_names[phase], state->checks[phase].status);
    }
    for (int phase = 0; phase < CHECK_PHASES; phase++) {
        if (state->checks[phase].status[0] != '\0')
            append_text(text, size, &length, "%s_cpus=%" PRIu64 "\n",
                        k_phase_names[phase], state->checks[phase].cpus);
    }
    for (int phase = 0; phase < CHECK_PHASES; phase++) {
        if (state->checks[phase].status[0] != '\0')
            append_text(text, size, &length, "%s_elapsed_us=%" PRId64 "\n",
                        k_phase_names[phase], state->checks[phase].elapsed_us);
    }
    for (int phase = 0; phase < CHECK_PHASES; phase++) {
        if (state->checks[phase].status[0] != '\0')
            append_text(text, size, &length, "%s_cpu_us=%" PRId64 "\n",
                        k_phase_names[phase], state->checks[phase].cpu_us);
    }
    for (int phase = PHASE_CAPTURE; phase < CHECK_PHASES; phase++) {
        if (state->checks[phase].status[0] != '\0')
            append_text(text, size, &length, "%s_generation=%" PRIu64 "\n",
                        k_phase_names[phase], state->checks[phase].generation);
    }
    for (int phase = 0; phase < CHECK_PHASES; phase++) {
        if (state->checks[phase].status[0] != '\0' &&
            state->checks[phase].failures >= 0)
            append_text(text, size, &length, "%s_failures=%" PRId64 "\n",
                        k_phase_names[phase], state->checks[phase].failures);
    }
    append_text(text, size, &length,
                "tsc_hz=%" PRIu64 "\n"
                "lapic_hz=%" PRIu64 "\n"
                "discontinuities=%" PRIu64 "\n"
                "last_discontinuity=%s\n"
                "last_step_ns=%" PRId64 "\n"
                "last_step_realtime_ns=%" PRId64 "\n"
                "last_downtime_ns=%" PRIu64 "\n"
                "last_downtime_source=%s\n"
                "synchronized=%" PRIu64 "\n"
                "offset_ns=%" PRId64 "\n"
                "uncertainty_ns=%" PRId64 "\n"
                "frequency_ppb=%" PRId64 "\n"
                "samples=%" PRIu64 "\n"
                "rejected_samples=%" PRIu64 "\n"
                "last_sample_error=%s\n"
                "violations=%" PRIu64 "\n",
                state->tsc_hz, state->lapic_hz, state->discontinuities,
                state->last_discontinuity, state->last_step_ns,
                state->last_step_realtime_ns, state->last_downtime_ns,
                state->last_downtime_source, state->synchronized,
                state->offset_ns, state->uncertainty_ns, state->frequency_ppb,
                state->samples, state->rejected_samples,
                state->last_sample_error, state->violations);
}

static int state_load(struct time_state *state)
{
    char text[2048];

    if (read_text(STATE_PATH, text, sizeof(text)) < 0)
        return -1;
    return state_parse(text, state);
}

static int state_store(const struct time_state *state)
{
    char text[2048];

    state_format(state, text, sizeof(text));
    return replace_file(STATE_PATH, STATE_TEMP_PATH, text);
}

typedef void (*state_update)(struct time_state *state, const void *context);

// Applies UPDATE to the published state under its lock. The helpers and the
// daemon all write the state this way, so updates never interleave.
static int state_apply(state_update update, const void *context)
{
    struct time_state state;
    int lock = lock_file(STATE_LOCK_PATH);
    int result;

    if (lock < 0)
        return -1;
    if (state_load(&state) != 0)
        state_init(&state);
    update(&state, context);
    result = state_store(&state);
    close(lock);
    return result;
}

static void add_violation(struct time_state *state, const void *context)
{
    (void)context;
    state->violations++;
}

static void count_violation(void)
{
    if (!g_test && access(STATE_PATH, F_OK) == 0)
        (void)state_apply(add_violation, NULL);
}

static int64_t frequency_to_ppb(int64_t frequency)
{
    int64_t scaled = frequency * 1000;

    return (scaled + (scaled >= 0 ? 32768 : -32768)) / 65536;
}

// F and L are published in the state file by the boot check.
static int rates_load(uint64_t *tsc_hz, uint64_t *lapic_hz)
{
    struct time_state state;

    if (state_load(&state) != 0 || state.tsc_hz == 0 || state.lapic_hz == 0)
        return -1;
    *tsc_hz = state.tsc_hz;
    *lapic_hz = state.lapic_hz;
    return 0;
}

struct check_record {
    enum phase phase;
    const char *status;
    int cpus;
    int64_t elapsed_us;
    int64_t cpu_us;
    uint64_t tsc_hz;
    uint64_t lapic_hz;
    int failures;
};

static void apply_check(struct time_state *state, const void *context)
{
    const struct check_record *record = context;
    struct check_state *check = &state->checks[record->phase];

    snprintf(check->status, sizeof(check->status), "%s", record->status);
    check->cpus = (uint64_t)record->cpus;
    check->elapsed_us = record->elapsed_us;
    check->cpu_us = record->cpu_us;
    check->generation = record->phase == PHASE_BOOT ? 0 : state->generation;
    check->failures = -1;
    // Report-only mode keeps the failures in the state file, as the spec's
    // keys reserved for it.
    if (g_report_only) {
        check->failures = record->failures;
        if (record->failures > 0 && strcmp(record->status, "ok") == 0)
            snprintf(check->status, sizeof(check->status), "fail");
    }
    if (record->tsc_hz != 0)
        state->tsc_hz = record->tsc_hz;
    if (record->lapic_hz != 0)
        state->lapic_hz = record->lapic_hz;
}

// Records a phase's conformance check in the state file: "pending" while it
// runs, then "ok" (a failed check powers the guest off instead), with the
// generation it ran in and its wall and CPU times. Rates of 0 keep the
// published ones. Report-only mode records "fail" and the failure count of a
// check that failed.
static int record_check(enum phase phase, const char *status, int cpus,
                        int64_t elapsed_us, int64_t cpu_us, uint64_t tsc_hz,
                        uint64_t lapic_hz, int failures)
{
    struct check_record record = {phase,  status, cpus,     elapsed_us,
                                  cpu_us, tsc_hz, lapic_hz, failures};

    return state_apply(apply_check, &record);
}

// The watchdog settings saved before a capture, in the order they are
// written. Missing debug-kernel sysctls are skipped.
static const char *const k_suppressed[] = {RCU_STALL_SUPPRESS, SOFT_WATCHDOG,
                                           HUNG_TASK_TIMEOUT};
static const int64_t k_suppressed_values[] = {1, 0, 0};

static int suppression_save(void)
{
    char text[512] = "";
    size_t length = 0;
    int fd;

    for (size_t index = 0; index < ARRAY_SIZE(k_suppressed); index++) {
        int64_t value;

        if (access(k_suppressed[index], F_OK) != 0) {
            if (index == 0)
                return -1;
            continue;
        }
        if (read_int64(k_suppressed[index], &value) != 0)
            return -1;
        length += (size_t)snprintf(text + length, sizeof(text) - length,
                                   "%s=%" PRId64 "\n", k_suppressed[index],
                                   value);
    }
    fd = open(SUPPRESSION_PATH, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
    if (fd < 0)
        return -1;
    if (write_all(fd, text, length) != 0) {
        close(fd);
        unlink(SUPPRESSION_PATH);
        return -1;
    }
    close(fd);
    for (size_t index = 0; index < ARRAY_SIZE(k_suppressed); index++) {
        if (access(k_suppressed[index], F_OK) == 0 &&
            write_int64(k_suppressed[index], k_suppressed_values[index]) != 0)
            return -1;
    }
    return 0;
}

// Writes back every value saved by suppression_save, then forgets them. A
// lock serializes the daemon's release at the stall timeout with the restore
// worker's; with MISSING_OK, values already written back count as restored.
static int suppression_restore(char *detail, size_t size, bool missing_ok)
{
    char text[512];
    char *line = text;
    int lock = lock_file(SUPPRESSION_LOCK_PATH);
    int result = -1;

    if (lock < 0) {
        snprintf(detail, size, "lock %s: %s", SUPPRESSION_LOCK_PATH,
                 strerror(errno));
        return -1;
    }
    if (read_text(SUPPRESSION_PATH, text, sizeof(text)) < 0) {
        if (missing_ok && errno == ENOENT)
            result = 0;
        else
            snprintf(detail, size, "read %s: %s", SUPPRESSION_PATH,
                     strerror(errno));
        close(lock);
        return result;
    }
    while (*line != '\0') {
        char *next = strchr(line, '\n');
        char *value = strchr(line, '=');

        if (next != NULL)
            *next++ = '\0';
        if (value == NULL)
            break;
        *value++ = '\0';
        if (write_text(line, value) != 0) {
            snprintf(detail, size, "restore %.200s: %s", line,
                     strerror(errno));
            close(lock);
            return -1;
        }
        line = next != NULL ? next : line + strlen(line);
    }
    if (unlink(SUPPRESSION_PATH) != 0)
        snprintf(detail, size, "remove %s: %s", SUPPRESSION_PATH,
                 strerror(errno));
    else
        result = 0;
    close(lock);
    return result;
}

// ---------------------------------------------------------------------------
// Decoders and decisions (pure; exercised by `nvx-time test`)
// ---------------------------------------------------------------------------

struct restore_packet {
    uint8_t flags;
    uint8_t online_vp_count;
    uint8_t range_count;
    uint32_t generation;
    int32_t rate_deviation;
    uint64_t downtime_ns;
    uint64_t utc_ns;
    uint64_t ranges[PACKET_MAX_RANGES][2];
    uint8_t entropy[PACKET_ENTROPY_SIZE];
};

static int parse_packet_header(const uint8_t *bytes,
                               struct restore_packet *packet, char *detail,
                               size_t size)
{
    uint8_t online;

    memset(packet, 0, sizeof(*packet));
    if (memcmp(bytes, "OVR", 3) != 0) {
        snprintf(detail, size, "restore packet magic is %02x%02x%02x",
                 bytes[0], bytes[1], bytes[2]);
        return -1;
    }
    if (bytes[3] != 4) {
        snprintf(detail, size, "restore packet version is %u", bytes[3]);
        return -1;
    }
    packet->flags = bytes[4];
    online = bytes[5];
    packet->online_vp_count = online;
    packet->range_count = bytes[6];
    packet->generation = read_le32(bytes + 8);
    packet->rate_deviation = (int32_t)read_le32(bytes + 12);
    packet->downtime_ns = read_le64(bytes + 16);
    packet->utc_ns = read_le64(bytes + 24);
    if ((packet->flags & ~PACKET_KNOWN_FLAGS) != 0) {
        snprintf(detail, size, "restore packet flags 0x%02x set reserved bits",
                 packet->flags);
        return -1;
    }
    if (online != 0 && online != 1 && online != 2 && online != 4 &&
        online != 8) {
        snprintf(detail, size, "restore packet online VP count is %u", online);
        return -1;
    }
    if ((packet->flags & PACKET_MEMORY_TARGET) == 0 && packet->range_count != 0) {
        snprintf(detail, size,
                 "restore packet has %u memory ranges without a memory target",
                 packet->range_count);
        return -1;
    }
    if (bytes[7] != 0) {
        snprintf(detail, size, "restore packet reserved byte is %u", bytes[7]);
        return -1;
    }
    if (packet->generation == 0) {
        snprintf(detail, size, "restore packet generation is 0");
        return -1;
    }
    if (packet->rate_deviation > RATE_DEVIATION_LIMIT ||
        packet->rate_deviation < -RATE_DEVIATION_LIMIT) {
        snprintf(detail, size,
                 "restore packet rate deviation %" PRId32
                 " exceeds the 250 ppm tolerance",
                 packet->rate_deviation);
        return -1;
    }
    if (packet->downtime_ns > (uint64_t)DOWNTIME_LIMIT_NS) {
        snprintf(detail, size,
                 "restore packet downtime %" PRIu64 " ns exceeds 30 days",
                 packet->downtime_ns);
        return -1;
    }
    if (packet->utc_ns == 0 || packet->utc_ns > (uint64_t)INT64_MAX) {
        snprintf(detail, size, "restore packet UTC %" PRIu64 " is invalid",
                 packet->utc_ns);
        return -1;
    }
    return 0;
}

static size_t packet_body_size(const struct restore_packet *packet)
{
    return (size_t)packet->range_count * PACKET_RANGE_SIZE + PACKET_ENTROPY_SIZE;
}

static int parse_packet_body(const uint8_t *bytes,
                             struct restore_packet *packet, char *detail,
                             size_t size)
{
    for (unsigned index = 0; index < packet->range_count; index++) {
        uint64_t start = read_le64(bytes + index * PACKET_RANGE_SIZE);
        uint64_t length = read_le64(bytes + index * PACKET_RANGE_SIZE + 8);

        if (length == 0 || start + length < start) {
            snprintf(detail, size, "restore memory range %u is invalid", index);
            return -1;
        }
        packet->ranges[index][0] = start;
        packet->ranges[index][1] = length;
    }
    memcpy(packet->entropy, bytes + packet->range_count * PACKET_RANGE_SIZE,
           PACKET_ENTROPY_SIZE);
    return 0;
}

struct time_sample {
    uint8_t flags;
    uint32_t generation;
    uint64_t utc_ns;
    int64_t t0;
    int64_t t1;
};

static int parse_sample(const uint8_t *bytes, struct time_sample *sample,
                        char *detail, size_t size)
{
    uint16_t reserved = (uint16_t)(bytes[2] | bytes[3] << 8);

    sample->flags = bytes[1];
    sample->generation = read_le32(bytes + 4);
    sample->utc_ns = read_le64(bytes + 8);
    if (bytes[0] != 1) {
        snprintf(detail, size, "time sample version is %u", bytes[0]);
        return -1;
    }
    if ((sample->flags & ~SAMPLE_TEST_HOOKS) != 0 || reserved != 0) {
        snprintf(detail, size, "time sample flags 0x%02x or reserved 0x%04x",
                 sample->flags, reserved);
        return -1;
    }
    if (sample->utc_ns == 0 || sample->utc_ns > (uint64_t)INT64_MAX) {
        snprintf(detail, size, "time sample UTC %" PRIu64 " is invalid",
                 sample->utc_ns);
        return -1;
    }
    return 0;
}

// The host latched UTC between the guest's CLOCK_REALTIME reads T0 and T1.
static void pair_clocks(int64_t t0, int64_t t1, uint64_t utc_ns,
                        int64_t *theta, int64_t *epsilon)
{
    *epsilon = (t1 - t0) / 2;
    *theta = (int64_t)utc_ns - (t0 + (t1 - t0) / 2);
}

static void offset_to_timeval(int64_t offset_ns, struct timeval *time)
{
    time->tv_sec = (time_t)(offset_ns / NSEC_PER_SEC);
    time->tv_usec = (suseconds_t)(offset_ns % NSEC_PER_SEC);
    if (time->tv_usec < 0) {
        time->tv_sec -= 1;
        time->tv_usec += NSEC_PER_SEC;
    }
}

static int64_t restore_frequency(int32_t rate_deviation)
{
    int64_t frequency = -(int64_t)rate_deviation;

    if (frequency > FREQUENCY_LIMIT)
        return FREQUENCY_LIMIT;
    if (frequency < -FREQUENCY_LIMIT)
        return -FREQUENCY_LIMIT;
    return frequency;
}

static int64_t ceil_us(int64_t nanoseconds)
{
    return (nanoseconds + NSEC_PER_USEC - 1) / NSEC_PER_USEC;
}

// Builds the discipline's correction for an accepted sample: a step at or
// beyond 128 ms, otherwise a PLL slew. Returns true for a step.
static bool plan_correction(int64_t theta, int64_t epsilon, bool fast,
                            struct timex *tx)
{
    int64_t magnitude = theta < 0 ? -theta : theta;

    memset(tx, 0, sizeof(*tx));
    if (magnitude >= STEP_THRESHOLD_NS) {
        tx->modes = ADJ_SETOFFSET | ADJ_NANO;
        offset_to_timeval(theta, &tx->time);
        return true;
    }
    tx->modes = ADJ_OFFSET | ADJ_STATUS | ADJ_NANO | ADJ_TIMECONST |
                ADJ_MAXERROR | ADJ_ESTERROR;
    tx->offset = (long)theta;
    tx->status = STA_PLL | STA_NANO;
    tx->constant = fast ? FAST_TIME_CONSTANT : SLOW_TIME_CONSTANT;
    tx->maxerror = (long)ceil_us(magnitude + epsilon);
    tx->esterror = (long)ceil_us(epsilon);
    return false;
}

struct watch_rule {
    const char *code;
    const char *all[2];
    const char *any[4];
};

static const struct watch_rule k_watch_rules[] = {
    {"G_TSC_UNSTABLE", {"Marking TSC unstable due to ", NULL}, {NULL}},
    {"G_CLOCKSOURCE_UNSTABLE",
     {"timekeeping watchdog on CPU", "as unstable because the skew is too large"},
     {NULL}},
    {"G_CLOCKSOURCE_SWITCH", {"clocksource: Switched to clocksource ", NULL},
     {NULL}},
    {"G_CLOCKSOURCE_SKEW",
     {"clocksource: ", NULL},
     {" ahead of CPU ", " behind CPU ", NULL}},
    {"G_TSC_WARP",
     {NULL, NULL},
     {"TSC synchronization [CPU#", "cycles TSC warp between CPUs",
      "TSC warped randomly between CPUs", NULL}},
    {"G_TSC_ADJUST", {"TSC ADJUST", NULL}, {NULL}},
    {"G_RCU_STALL",
     {"rcu: INFO: ", NULL},
     {" detected stalls on CPUs/tasks", " self-detected stall on CPU",
      " detected expedited stalls", NULL}},
    {"G_RCU_STARVED",
     {"rcu: ", NULL},
     {" kthread starved for ", " kthread timer wakeup didn't happen for ",
      NULL}},
    {"G_SOFT_LOCKUP", {"watchdog: BUG: soft lockup - CPU#", NULL}, {NULL}},
    {"G_HARD_LOCKUP", {"Watchdog detected hard LOCKUP", NULL}, {NULL}},
    {"G_HUNG_TASK", {"INFO: task ", " blocked for more than "}, {NULL}},
    {"G_UNCHECKED_MSR", {"unchecked MSR access error", NULL}, {NULL}},
    {"G_APIC_ID_MISMATCH", {"APIC ID mismatch", NULL}, {NULL}},
};

// Returns the code of the first rule whose substrings all appear in MESSAGE.
// The guest's own NVX-TIME lines quote kernel text and are never matched.
static const char *match_watch_rule(const char *message)
{
    if (strncmp(message, "NVX-TIME-", 9) == 0)
        return NULL;
    for (size_t index = 0; index < ARRAY_SIZE(k_watch_rules); index++) {
        const struct watch_rule *rule = &k_watch_rules[index];
        bool all = true;
        bool any = rule->any[0] == NULL;

        for (size_t part = 0; part < ARRAY_SIZE(rule->all) && rule->all[part];
             part++)
            all = all && strstr(message, rule->all[part]) != NULL;
        for (size_t part = 0; part < ARRAY_SIZE(rule->any) && rule->any[part];
             part++)
            any = any || strstr(message, rule->any[part]) != NULL;
        if (all && any)
            return rule->code;
    }
    return NULL;
}

struct boot_log {
    bool hypervisor;
    bool privileges;
    bool lapic;
    char last_switch[48];
    char problem[DETAIL_MAX];
    char wx[DETAIL_MAX];
};

// CONFIG_DEBUG_WX audits the kernel page tables before init runs and logs
// this for a mapping that is both writable and executable.
static const char k_wx_record[] = "Found insecure W+X mapping";

static void boot_log_add(struct boot_log *log, const char *message)
{
    static const char *const forbidden[] = {
        "Fast TSC calibration", "Refined TSC clocksource calibration",
        "kvm-clock", "APIC timer: using supplied frequency"};
    static const char switched[] = "clocksource: Switched to clocksource ";
    const char *name = strstr(message, switched);
    const char *code;

    if (log->wx[0] == '\0' && strncmp(message, "NVX-TIME-", 9) != 0 &&
        strstr(message, k_wx_record) != NULL)
        snprintf(log->wx, sizeof(log->wx), "%.300s", message);
    if (strstr(message, "Hypervisor detected: Microsoft Hyper-V") != NULL)
        log->hypervisor = true;
    if (strstr(message, "Hyper-V: privilege flags low 0x8860,") != NULL)
        log->privileges = true;
    if (ends_with(message, "Hyper-V: LAPIC Timer Frequency: 0x989680") ||
        ends_with(message, "Hyper-V: LAPIC Timer Frequency: 0x1e8480"))
        log->lapic = true;
    if (name != NULL) {
        name += sizeof(switched) - 1;
        snprintf(log->last_switch, sizeof(log->last_switch), "%.*s",
                 (int)strcspn(name, " \n"), name);
    }
    if (log->problem[0] != '\0')
        return;
    code = match_watch_rule(message);
    if (code != NULL && strcmp(code, "G_CLOCKSOURCE_SWITCH") != 0) {
        snprintf(log->problem, sizeof(log->problem), "%s: %.300s", code,
                 message);
        return;
    }
    for (size_t index = 0; index < ARRAY_SIZE(forbidden); index++) {
        if (strncmp(message, "NVX-TIME-", 9) != 0 &&
            strstr(message, forbidden[index]) != NULL) {
            snprintf(log->problem, sizeof(log->problem),
                     "forbidden record: %.300s", message);
            return;
        }
    }
}

static bool boot_log_verdict(const struct boot_log *log, char *detail,
                             size_t size)
{
    if (log->problem[0] != '\0')
        snprintf(detail, size, "%s", log->problem);
    else if (!log->hypervisor)
        snprintf(detail, size, "no 'Hypervisor detected: Microsoft Hyper-V'");
    else if (!log->privileges)
        snprintf(detail, size, "no 'Hyper-V: privilege flags low 0x8860,'");
    else if (!log->lapic)
        snprintf(detail, size, "no 'Hyper-V: LAPIC Timer Frequency' of 1 GHz "
                               "or 200 MHz");
    else if (strcmp(log->last_switch, "tsc") != 0)
        snprintf(detail, size, "last clocksource switch is to '%s', not tsc",
                 log->last_switch);
    else
        return true;
    return false;
}

// ---------------------------------------------------------------------------
// Kernel log
// ---------------------------------------------------------------------------

typedef void (*kmsg_consumer)(void *context, const char *message);

// Reads every available /dev/kmsg record from the non-blocking FD and passes
// each message text to CONSUME. Returns 0 when drained, -EPIPE when records
// were overwritten before they were read, or another negative errno.
static int kmsg_drain(int fd, kmsg_consumer consume, void *context)
{
    char record[8192];

    for (;;) {
        ssize_t count = read(fd, record, sizeof(record) - 1);
        char *message;

        if (count < 0 && errno == EINTR)
            continue;
        if (count < 0)
            return errno == EAGAIN ? 0 : -errno;
        if (count == 0)
            return 0;
        record[count] = '\0';
        message = strchr(record, ';');
        if (message == NULL)
            continue;
        message++;
        message[strcspn(message, "\n")] = '\0';
        consume(context, message);
    }
}

#define SYSLOG_ACTION_READ_ALL 3
#define SYSLOG_ACTION_SIZE_BUFFER 10

// Returns the message text of one SYSLOG_ACTION_READ_ALL line, without its
// "<level>" prefix and its printk timestamp ("[    1.234567] ").
static const char *syslog_message(const char *line)
{
    const char *cursor = line;
    const char *end = strchr(line, '>');
    const char *scan;

    if (*cursor == '<' && end != NULL)
        cursor = end + 1;
    if (*cursor != '[')
        return cursor;
    scan = cursor + 1;
    while (*scan == ' ')
        scan++;
    while (*scan >= '0' && *scan <= '9')
        scan++;
    if (*scan++ != '.')
        return cursor;
    while (*scan >= '0' && *scan <= '9')
        scan++;
    if (*scan != ']')
        return cursor;
    return scan[1] == ' ' ? scan + 2 : scan + 1;
}

static void boot_log_line(void *context, char *line)
{
    boot_log_add(context, syslog_message(line));
}

struct line_starts {
    const char **starts;
    size_t count;
    size_t capacity;
};

// Records the start of every line of BUFFER that contains NEEDLE.
static bool mark_lines(const char *buffer, const char *needle,
                       struct line_starts *lines)
{
    for (const char *hit = strstr(buffer, needle); hit != NULL;
         hit = strstr(hit + 1, needle)) {
        const char *start = hit;

        while (start > buffer && start[-1] != '\n')
            start--;
        if (lines->count == lines->capacity) {
            size_t capacity = lines->capacity * 2;
            const char **grown =
                realloc(lines->starts, capacity * sizeof(*lines->starts));

            if (grown == NULL)
                return false;
            lines->starts = grown;
            lines->capacity = capacity;
        }
        lines->starts[lines->count++] = start;
    }
    return true;
}

static int compare_starts(const void *left, const void *right)
{
    const char *a = *(const char *const *)left;
    const char *b = *(const char *const *)right;

    return a < b ? -1 : a > b;
}

// C4 and K1 over the whole SYSLOG_ACTION_READ_ALL text. Only lines that
// contain a required record, a forbidden record, the W+X record, or the first
// substring of a watcher row can change the verdict, so a few whole-buffer
// searches find them, and boot_log_add() sees just those lines, in log order.
// Matching every rule against every line cost about a millisecond on a
// guest's boot log.
static void boot_log_scan(char *buffer, struct boot_log *log)
{
    static const char *const anchors[] = {
        "Hypervisor detected: Microsoft Hyper-V",
        "Hyper-V: privilege flags low 0x8860,",
        "Hyper-V: LAPIC Timer Frequency: ",
        "Fast TSC calibration",
        "Refined TSC clocksource calibration",
        "kvm-clock",
        "APIC timer: using supplied frequency",
        k_wx_record};
    struct line_starts lines = {malloc(256 * sizeof(const char *)), 0, 256};
    bool ok = lines.starts != NULL;

    for (size_t index = 0; ok && index < ARRAY_SIZE(anchors); index++)
        ok = mark_lines(buffer, anchors[index], &lines);
    for (size_t index = 0; ok && index < ARRAY_SIZE(k_watch_rules); index++) {
        const struct watch_rule *rule = &k_watch_rules[index];

        if (rule->all[0] != NULL) {
            ok = mark_lines(buffer, rule->all[0], &lines);
            continue;
        }
        for (size_t part = 0;
             ok && part < ARRAY_SIZE(rule->any) && rule->any[part]; part++)
            ok = mark_lines(buffer, rule->any[part], &lines);
    }
    if (!ok) {
        free(lines.starts);
        for_each_text_line(buffer, boot_log_line, log);
        return;
    }
    qsort(lines.starts, lines.count, sizeof(*lines.starts), compare_starts);
    for (size_t index = 0; index < lines.count; index++) {
        char *start = (char *)lines.starts[index];
        char *end = strchr(start, '\n');

        if (index > 0 && lines.starts[index - 1] == start)
            continue;
        if (end != NULL)
            *end = '\0';
        boot_log_add(log, syslog_message(start));
        if (end != NULL)
            *end = '\n';
    }
    free(lines.starts);
}

// Reads the whole kernel log for C4 with one SYSLOG_ACTION_READ_ALL. The
// caller has already positioned the daemon's /dev/kmsg descriptor at the end
// of the log, so records logged in between are seen twice, never missed.
// BUFFER holds SIZE + 1 bytes, the size SYSLOG_ACTION_SIZE_BUFFER reported.
static int read_boot_log_into(struct boot_log *log, char *buffer, int size)
{
    int count = klogctl(SYSLOG_ACTION_READ_ALL, buffer, size);
    int error = errno;

    if (count >= 0) {
        buffer[count] = '\0';
        boot_log_scan(buffer, log);
    }
    errno = error;
    return count < 0 ? -1 : 0;
}

// ---------------------------------------------------------------------------
// Conformance checks
// ---------------------------------------------------------------------------

struct checks {
    enum phase phase;
    int failures;
    int possible;
    int online_count;
    bool online[MAX_CPUS];
    uint64_t cpu_khz[MAX_CPUS];
    bool cpu_khz_known[MAX_CPUS];
    uint64_t tsc_hz;
    uint64_t lapic_hz;
};

static bool check_failed_code(struct checks *checks, const char *code,
                              const char *format, ...)
    __attribute__((format(printf, 3, 4)));

// Serializes failure reports from the boot check's per-CPU threads, which
// share the console and the violation count.
static pthread_mutex_t g_report_lock = PTHREAD_MUTEX_INITIALIZER;

// Emits CODE and powers off with status 193; a failed check prints no
// marker. In report-only mode it returns false and the run continues.
static bool check_failed_code(struct checks *checks, const char *code,
                              const char *format, ...)
{
    char detail[DETAIL_MAX];
    va_list arguments;

    va_start(arguments, format);
    vsnprintf(detail, sizeof(detail), format, arguments);
    va_end(arguments);
    pthread_mutex_lock(&g_report_lock);
    checks->failures++;
    emit_event(code, "conformance", checks->phase, detail);
    if (!g_report_only)
        power_off(STATUS_CONFORMANCE);
    pthread_mutex_unlock(&g_report_lock);
    return false;
}

static bool check_failed(struct checks *checks, const char *id,
                         const char *format, ...)
    __attribute__((format(printf, 3, 4)));

// check_failed_code() with the conformance code G_CONFORMANCE_<ID>.
static bool check_failed(struct checks *checks, const char *id,
                         const char *format, ...)
{
    char detail[DETAIL_MAX];
    char code[32];
    va_list arguments;

    va_start(arguments, format);
    vsnprintf(detail, sizeof(detail), format, arguments);
    va_end(arguments);
    snprintf(code, sizeof(code), "G_CONFORMANCE_%s", id);
    return check_failed_code(checks, code, "%s", detail);
}

static int checks_init(struct checks *checks, enum phase phase)
{
    char text[256];
    bool possible[MAX_CPUS];

    memset(checks, 0, sizeof(*checks));
    checks->phase = phase;
    if (read_text("/sys/devices/system/cpu/possible", text, sizeof(text)) < 0)
        return -1;
    checks->possible = parse_cpu_list(text, possible);
    if (read_text("/sys/devices/system/cpu/online", text, sizeof(text)) < 0)
        return -1;
    checks->online_count = parse_cpu_list(text, checks->online);
    return checks->possible > 0 && checks->online_count > 0 ? 0 : -1;
}

static void cpuid(uint32_t leaf, uint32_t subleaf, uint32_t registers[4])
{
    __asm__ __volatile__("cpuid"
                         : "=a"(registers[0]), "=b"(registers[1]),
                           "=c"(registers[2]), "=d"(registers[3])
                         : "a"(leaf), "c"(subleaf));
}

// Runs the calling thread on CPU only, so inline CPUID and the msr driver
// read that CPU without inter-processor calls.
static int pin_to_cpu(int cpu)
{
    cpu_set_t set;

    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    if (sched_setaffinity(0, sizeof(set), &set) != 0)
        return -1;
    return sched_getcpu() == cpu ? 0 : -1;
}

static bool all_zero(const uint32_t value[4])
{
    return (value[0] | value[1] | value[2] | value[3]) == 0;
}

// C1: the identity leaves and the explicit zero leaves. The absence of a
// hypervisor signature at other bases is a static backend property that the
// CI exhaustive check covers, so the boot check skips that 255-leaf scan.
static bool check_identity(struct checks *checks, int cpu)
{
    const uint32_t capacity = (uint32_t)checks->possible;
    const uint32_t expected[6][4] = {
        {0x40000005, 0x7263694d, 0x666f736f, 0x76482074},
        {0x31237648, 0, 0, 0},
        {0x0058564e, 0x00010000, 0, 0},
        {0x00008860, 0, 0, 0x00000100},
        {0, 0xffffffff, 0, 0},
        {capacity, capacity, 0, 0},
    };
    static const uint32_t zero_leaves[][2] = {{0x40000006, 0x4000000f},
                                              {0x40000080, 0x40000082}};
    uint32_t value[4];

    for (uint32_t index = 0; index < 6; index++) {
        cpuid(0x40000000 + index, 0, value);
        if (memcmp(value, expected[index], sizeof(value)) != 0)
            return check_failed(
                checks, "C1",
                "cpu %d leaf 0x%08" PRIx32 " is %08" PRIx32 ":%08" PRIx32
                ":%08" PRIx32 ":%08" PRIx32 ", expected %08" PRIx32
                ":%08" PRIx32 ":%08" PRIx32 ":%08" PRIx32,
                cpu, 0x40000000 + index, value[0], value[1], value[2],
                value[3], expected[index][0], expected[index][1],
                expected[index][2], expected[index][3]);
    }
    for (size_t range = 0; range < ARRAY_SIZE(zero_leaves); range++) {
        for (uint32_t leaf = zero_leaves[range][0]; leaf <= zero_leaves[range][1];
             leaf++) {
            cpuid(leaf, 0, value);
            if (!all_zero(value))
                return check_failed(
                    checks, "C1",
                    "cpu %d leaf 0x%08" PRIx32 " is %08" PRIx32 ":%08" PRIx32
                    ":%08" PRIx32 ":%08" PRIx32 ", expected zero",
                    cpu, leaf, value[0], value[1], value[2], value[3]);
        }
    }
    return true;
}

static bool check_bit(struct checks *checks, int cpu, const char *name,
                      uint32_t reg, unsigned bit, bool expected)
{
    if (((reg >> bit) & 1) == (expected ? 1u : 0u))
        return true;
    return check_failed(checks, "C2", "cpu %d %s is %s", cpu, name,
                        expected ? "clear" : "set");
}

static bool check_leaf(struct checks *checks, int cpu, uint32_t leaf,
                       const uint32_t expected[4])
{
    uint32_t value[4];

    cpuid(leaf, 0, value);
    if (memcmp(value, expected, sizeof(value)) == 0)
        return true;
    return check_failed(checks, "C2",
                        "cpu %d leaf 0x%08" PRIx32 " is %08" PRIx32
                        ":%08" PRIx32 ":%08" PRIx32 ":%08" PRIx32
                        ", expected %08" PRIx32 ":%08" PRIx32 ":%08" PRIx32
                        ":%08" PRIx32,
                        cpu, leaf, value[0], value[1], value[2], value[3],
                        expected[0], expected[1], expected[2], expected[3]);
}

// C2: the CPU time bits every profile fixes.
static bool check_time_bits(struct checks *checks, int cpu)
{
    static const uint32_t arat_only[4] = {4, 0, 0, 0};
    static const uint32_t zero[4] = {0, 0, 0, 0};
    static const uint32_t invariant_tsc[4] = {0, 0, 0, 0x100};
    uint32_t value[4];
    uint32_t max_basic;
    bool ok;

    cpuid(0, 0, value);
    max_basic = value[0];
    cpuid(1, 0, value);
    ok = check_bit(checks, cpu, "CPUID.1:ECX[31] (hypervisor)", value[2], 31,
                   true) &&
         check_bit(checks, cpu, "CPUID.1:ECX[24] (TSC-deadline)", value[2],
                   24, false) &&
         check_bit(checks, cpu, "CPUID.1:ECX[15] (PDCM)", value[2], 15,
                   false) &&
         check_bit(checks, cpu, "CPUID.1:EDX[4] (TSC)", value[3], 4, true);
    if (!ok)
        return false;
    if (max_basic < 6)
        return check_failed(checks, "C2", "cpu %d lacks leaf 0x6 (ARAT)", cpu);
    if (!check_leaf(checks, cpu, 6, arat_only))
        return false;
    if (max_basic >= 7) {
        cpuid(7, 0, value);
        if (!check_bit(checks, cpu, "CPUID.7.0:EBX[1] (TSC_ADJUST)", value[1],
                       1, false))
            return false;
    }
    if (max_basic >= 0xa && !check_leaf(checks, cpu, 0xa, zero))
        return false;
    if (max_basic >= 0x15 && !check_leaf(checks, cpu, 0x15, zero))
        return false;
    if (max_basic >= 0x16 && !check_leaf(checks, cpu, 0x16, zero))
        return false;
    cpuid(0x80000000, 0, value);
    if (value[0] < 0x80000007)
        return check_failed(checks, "C2", "cpu %d lacks leaf 0x80000007", cpu);
    cpuid(0x80000001, 0, value);
    if (!check_bit(checks, cpu, "CPUID.80000001:EDX[27] (RDTSCP)", value[3],
                   27, true))
        return false;
    return check_leaf(checks, cpu, 0x80000007, invariant_tsc);
}

static int msr_read(int fd, uint32_t msr, uint64_t *value)
{
    ssize_t count = pread(fd, value, sizeof(*value), (off_t)msr);

    if (count == (ssize_t)sizeof(*value))
        return 0;
    return count < 0 ? -errno : -EIO;
}

// C3: the synthetic MSRs. At boot (FULL), the VP index on every CPU and the
// partition-wide MSRs on CPU 0 only; at restore, CPU 0's declared TSC rate.
static bool check_msrs(struct checks *checks, int cpu, bool full)
{
    static const uint32_t faulting[] = {0x40000000, 0x40000001, 0x40000020};
    char path[64];
    uint64_t value = 0;
    bool ok = true;
    int error = 0;
    int fd;

    snprintf(path, sizeof(path), "/dev/cpu/%d/msr", cpu);
    fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0)
        return check_failed(checks, "C3", "cpu %d: open %s: %s", cpu, path,
                            strerror(errno));
    if (full && ((error = msr_read(fd, 0x40000002, &value)) != 0 ||
                 value != (uint64_t)cpu))
        ok = check_failed(checks, "C3",
                          "cpu %d: MSR 0x40000002 (VP index) is %" PRIu64
                          " (error %d)",
                          cpu, value, error);
    if (cpu != 0) {
        close(fd);
        return ok;
    }
    if (ok && (error = msr_read(fd, 0x40000022, &value)) != 0)
        ok = check_failed(checks, "C3", "cpu %d: MSR 0x40000022 read: %s",
                          cpu, strerror(-error));
    if (ok && checks->tsc_hz != 0 && value != checks->tsc_hz)
        ok = check_failed(checks, "C3",
                          "cpu %d: MSR 0x40000022 is %" PRIu64
                          " Hz, expected %" PRIu64 " Hz",
                          cpu, value, checks->tsc_hz);
    if (ok && checks->tsc_hz == 0)
        checks->tsc_hz = value;
    if (ok && checks->cpu_khz_known[cpu] &&
        value / 1000 != checks->cpu_khz[cpu])
        ok = check_failed(checks, "C3",
                          "cpu %d: floor(F / 1000) is %" PRIu64
                          " kHz but cpu MHz is %" PRIu64 " kHz",
                          cpu, value / 1000, checks->cpu_khz[cpu]);
    if (ok && full) {
        error = msr_read(fd, 0x40000023, &value);
        if (error != 0 || (value != 1000000000 && value != 200000000) ||
            (checks->lapic_hz != 0 && value != checks->lapic_hz))
            ok = check_failed(checks, "C3",
                              "cpu %d: MSR 0x40000023 (LAPIC rate) is %" PRIu64
                              " (error %d)",
                              cpu, value, error);
        else
            checks->lapic_hz = value;
    }
    if (ok && full &&
        ((error = msr_read(fd, 0x40000118, &value)) != 0 || value != 1))
        ok = check_failed(checks, "C3",
                          "cpu %d: MSR 0x40000118 is %" PRIu64 " (error %d)",
                          cpu, value, error);
    for (size_t index = 0; ok && full && index < ARRAY_SIZE(faulting);
         index++) {
        error = msr_read(fd, faulting[index], &value);
        if (error != -EIO)
            ok = check_failed(checks, "C3",
                              "cpu %d: MSR 0x%08" PRIx32
                              " read did not fail with EIO (error %d)",
                              cpu, faulting[index], error);
    }
    close(fd);
    return ok;
}

static bool has_word(const char *list, const char *word)
{
    size_t length = strlen(word);

    for (const char *cursor = list; (cursor = strstr(cursor, word)) != NULL;
         cursor += length) {
        bool starts = cursor == list || cursor[-1] == ' ' || cursor[-1] == '\t';
        char after = cursor[length];

        if (starts && (after == '\0' || after == ' ' || after == '\n'))
            return true;
    }
    return false;
}

// Parses /proc/cpuinfo line by line: records each CPU's `cpu MHz` in kHz and,
// when FLAGS is set, checks C5 on the CPUs in SELECTED. The first C5 failure
// ends the check.
struct cpuinfo_parse {
    struct checks *checks;
    const bool *selected;
    bool flags;
    bool failed;
    int cpu;
    bool seen[MAX_CPUS];
};

static void cpuinfo_init(struct cpuinfo_parse *parse, struct checks *checks,
                         const bool *selected, bool flags)
{
    memset(parse, 0, sizeof(*parse));
    parse->checks = checks;
    parse->selected = selected;
    parse->flags = flags;
    parse->cpu = -1;
}

static void cpuinfo_line(void *context, char *line)
{
    static const char *const required[] = {
        "tsc",          "constant_tsc", "nonstop_tsc", "tsc_known_freq",
        "tsc_reliable", "rdtscp",       "hypervisor",  "arat"};
    static const char *const forbidden[] = {"tsc_deadline_timer", "tsc_adjust"};
    struct cpuinfo_parse *parse = context;
    char *colon = strchr(line, ':');
    int cpu = parse->cpu;

    if (parse->failed || colon == NULL)
        return;
    if (strncmp(line, "processor", 9) == 0) {
        cpu = atoi(colon + 1);
        parse->cpu = cpu >= 0 && cpu < MAX_CPUS ? cpu : -1;
        if (parse->cpu >= 0)
            parse->seen[cpu] = true;
    } else if (cpu >= 0 && strncmp(line, "cpu MHz", 7) == 0) {
        char fraction[4] = "000";
        const char *dot = strchr(colon, '.');
        unsigned long long whole = strtoull(colon + 1, NULL, 10);

        for (int digit = 0; dot != NULL && digit < 3 &&
                            dot[1 + digit] >= '0' && dot[1 + digit] <= '9';
             digit++)
            fraction[digit] = dot[1 + digit];
        parse->checks->cpu_khz[cpu] = whole * 1000 + strtoull(fraction, NULL, 10);
        parse->checks->cpu_khz_known[cpu] = true;
    } else if (cpu >= 0 && parse->flags && parse->selected[cpu] &&
               strncmp(line, "flags", 5) == 0) {
        for (size_t index = 0; index < ARRAY_SIZE(required); index++) {
            if (!has_word(colon + 1, required[index])) {
                parse->failed = true;
                check_failed(parse->checks, "C5", "cpu %d lacks the %s flag",
                             cpu, required[index]);
                return;
            }
        }
        for (size_t index = 0; index < ARRAY_SIZE(forbidden); index++) {
            if (has_word(colon + 1, forbidden[index])) {
                parse->failed = true;
                check_failed(parse->checks, "C5", "cpu %d has the %s flag", cpu,
                             forbidden[index]);
                return;
            }
        }
    }
}

static bool cpuinfo_result(const struct cpuinfo_parse *parse)
{
    if (parse->failed)
        return false;
    for (int index = 0; parse->flags && index < MAX_CPUS; index++) {
        if (parse->selected[index] && !parse->seen[index])
            return check_failed(parse->checks, "C5",
                                "cpu %d is not in /proc/cpuinfo", index);
    }
    return true;
}

static bool check_cpuinfo_text(struct checks *checks, char *text,
                               const bool *selected, bool flags)
{
    struct cpuinfo_parse parse;

    cpuinfo_init(&parse, checks, selected, flags);
    for_each_text_line(text, cpuinfo_line, &parse);
    return cpuinfo_result(&parse);
}

static bool check_cpuinfo(struct checks *checks, const bool *selected,
                          bool flags)
{
    struct cpuinfo_parse parse;

    cpuinfo_init(&parse, checks, selected, flags);
    if (for_each_file_line("/proc/cpuinfo", cpuinfo_line, &parse) != 0)
        return check_failed(checks, "C5", "read /proc/cpuinfo: %s",
                            strerror(errno));
    return cpuinfo_result(&parse);
}

// ---------------------------------------------------------------------------
// Exhaustive check (CI only)
// ---------------------------------------------------------------------------

static int msr_write(int fd, uint32_t msr, uint64_t value)
{
    ssize_t count = pwrite(fd, &value, sizeof(value), (off_t)msr);

    if (count == (ssize_t)sizeof(value))
        return 0;
    return count < 0 ? -errno : -EIO;
}

// The leaves that OpenVMM programs as explicit zeros.
static bool explicit_zero_leaf(uint32_t leaf)
{
    return (leaf >= 0x40000006 && leaf <= 0x4000000f) ||
           (leaf >= 0x40000080 && leaf <= 0x40000082);
}

// X1 and X2 for one CPUID result: all zeros or, except for an explicit zero
// leaf, the Intel out-of-range result (the highest basic leaf's result for
// the same subleaf).
static bool exhaustive_leaf_ok(uint32_t leaf, const uint32_t value[4],
                               const uint32_t out_of_range[4], char *detail,
                               size_t size)
{
    char signature[13];

    if (all_zero(value) ||
        (!explicit_zero_leaf(leaf) &&
         memcmp(value, out_of_range, 4 * sizeof(uint32_t)) == 0))
        return true;
    memcpy(signature, &value[1], 4);
    memcpy(signature + 4, &value[2], 4);
    memcpy(signature + 8, &value[3], 4);
    signature[12] = '\0';
    for (int index = 0; index < 12; index++) {
        if (signature[index] < 0x20 || signature[index] > 0x7e)
            signature[index] = '.';
    }
    snprintf(detail, size,
             "leaf 0x%08" PRIx32 " is %08" PRIx32 " %08" PRIx32 " %08" PRIx32
             " %08" PRIx32 " (signature \"%s\"), expected zero%s",
             leaf, value[0], value[1], value[2], value[3], signature,
             explicit_zero_leaf(leaf) ? "" : " or the highest basic leaf");
    return false;
}

struct exhaustive {
    int failures;
    uint64_t tsc_hz;
    uint64_t lapic_hz;
    struct checks cpuinfo;
};

static void exhaustive_report(struct exhaustive *state, const char *id, int cpu,
                              bool passed, const char *detail)
{
    char escaped[DETAIL_MAX];

    escape_text(detail, escaped, sizeof(escaped) - 1);
    printf("NVX-TIME-ABI-EXHAUSTIVE: v=1 check=%s cpu=%d status=%s "
           "detail=\"%s\"\n",
           id, cpu, passed ? "pass" : "fail", escaped);
    if (!passed)
        state->failures++;
}

static int exhaustive_summary(const struct exhaustive *state, int online)
{
    printf("NVX-TIME-ABI-EXHAUSTIVE: v=1 status=%s cpus=%d failures=%d\n",
           state->failures == 0 ? "ok" : "fail", online, state->failures);
    return state->failures == 0 ? 0 : 1;
}

// X1 (every leaf 0x40000006..=0x400000ff) when STEP is 1, or X2 (every base
// 0x40000100..=0x4000ff00) when STEP is 0x100, on the current CPU.
static void exhaustive_leaves(struct exhaustive *state, const char *id, int cpu,
                              uint32_t first, uint32_t last, uint32_t step,
                              const uint32_t out_of_range[4])
{
    char detail[DETAIL_MAX] = "";
    int failing = 0;
    int count = 0;

    for (uint32_t leaf = first; leaf <= last; leaf += step, count++) {
        char reason[DETAIL_MAX];
        uint32_t value[4];

        cpuid(leaf, 0, value);
        if (!exhaustive_leaf_ok(leaf, value, out_of_range, reason,
                                sizeof(reason)) &&
            failing++ == 0)
            snprintf(detail, sizeof(detail), "%s", reason);
    }
    if (failing == 0) {
        snprintf(detail, sizeof(detail), "%d %s read zero or the highest basic leaf",
                 count, step == 1 ? "leaves" : "bases");
    } else if (failing > 1) {
        size_t length = strlen(detail);

        snprintf(detail + length, sizeof(detail) - length, "; %d of %d fail",
                 failing, count);
    }
    exhaustive_report(state, id, cpu, failing == 0, detail);
}

// X3: every C3 property on this CPU.
static void exhaustive_x3(struct exhaustive *state, int fd, int cpu)
{
    static const uint32_t faulting[] = {0x40000000, 0x40000001, 0x40000020};
    char detail[DETAIL_MAX];
    uint64_t index = 0;
    uint64_t tsc = 0;
    uint64_t lapic = 0;
    uint64_t control = 0;
    uint64_t value;
    bool ok = false;
    int error;

    if ((error = msr_read(fd, 0x40000002, &index)) != 0 ||
        index != (uint64_t)cpu)
        snprintf(detail, sizeof(detail),
                 "MSR 0x40000002 (VP index) is %" PRIu64 " (error %d)", index,
                 error);
    else if ((error = msr_read(fd, 0x40000022, &tsc)) != 0 || tsc == 0 ||
             (state->tsc_hz != 0 && tsc != state->tsc_hz))
        snprintf(detail, sizeof(detail),
                 "MSR 0x40000022 (TSC rate) is %" PRIu64
                 " (error %d; first CPU %" PRIu64 ")",
                 tsc, error, state->tsc_hz);
    else if (state->cpuinfo.cpu_khz_known[cpu] &&
             tsc / 1000 != state->cpuinfo.cpu_khz[cpu])
        snprintf(detail, sizeof(detail),
                 "floor(F / 1000) is %" PRIu64 " kHz but cpu MHz is %" PRIu64
                 " kHz",
                 tsc / 1000, state->cpuinfo.cpu_khz[cpu]);
    else if ((error = msr_read(fd, 0x40000023, &lapic)) != 0 ||
             (lapic != 1000000000 && lapic != 200000000) ||
             (state->lapic_hz != 0 && lapic != state->lapic_hz))
        snprintf(detail, sizeof(detail),
                 "MSR 0x40000023 (LAPIC rate) is %" PRIu64
                 " (error %d; first CPU %" PRIu64 ")",
                 lapic, error, state->lapic_hz);
    else if ((error = msr_read(fd, 0x40000118, &control)) != 0 || control != 1)
        snprintf(detail, sizeof(detail),
                 "MSR 0x40000118 is %" PRIu64 " (error %d)", control, error);
    else
        ok = true;
    for (size_t item = 0; ok && item < ARRAY_SIZE(faulting); item++) {
        error = msr_read(fd, faulting[item], &value);
        if (error != -EIO) {
            ok = false;
            snprintf(detail, sizeof(detail),
                     "MSR 0x%08" PRIx32 " read did not fail with EIO (error %d)",
                     faulting[item], error);
        }
    }
    if (ok) {
        snprintf(detail, sizeof(detail),
                 "vp_index=%d tsc_hz=%" PRIu64 " lapic_hz=%" PRIu64
                 " invariant_control=1",
                 cpu, tsc, lapic);
        if (state->tsc_hz == 0) {
            state->tsc_hz = tsc;
            state->lapic_hz = lapic;
        }
    }
    exhaustive_report(state, "X3", cpu, ok, detail);
}

// X4: MSR 0x40000118 accepts 0 and 1, each read back, and rejects 2. It is
// left at 1, the value Linux wrote at boot, whatever the outcome.
static void exhaustive_x4(struct exhaustive *state, int fd, int cpu)
{
    char detail[DETAIL_MAX] = "0 and 1 accepted, 2 rejected, left at 1";
    uint64_t value = 0;
    bool ok = true;
    int error = 0;

    for (uint64_t written = 0; ok && written <= 1; written++) {
        if ((error = msr_write(fd, 0x40000118, written)) != 0 ||
            (error = msr_read(fd, 0x40000118, &value)) != 0 ||
            value != written) {
            ok = false;
            snprintf(detail, sizeof(detail),
                     "after writing %" PRIu64 ", MSR 0x40000118 is %" PRIu64
                     " (error %d)",
                     written, value, error);
        }
    }
    if (ok && (error = msr_write(fd, 0x40000118, 2)) != -EIO) {
        ok = false;
        snprintf(detail, sizeof(detail),
                 "write of 2 to MSR 0x40000118 did not fail with EIO (error %d)",
                 error);
    }
    (void)msr_write(fd, 0x40000118, 1);
    if (ok && (msr_read(fd, 0x40000118, &value) != 0 || value != 1)) {
        ok = false;
        snprintf(detail, sizeof(detail),
                 "MSR 0x40000118 is %" PRIu64 " after the check", value);
    }
    exhaustive_report(state, "X4", cpu, ok, detail);
}

// X5: writes to the identity MSRs that are read-only or absent fail. X6:
// reads of IA32_TSC_ADJUST and IA32_TSC_DEADLINE fail.
static void exhaustive_faults(struct exhaustive *state, int fd, int cpu,
                              bool writes)
{
    static const uint32_t read_only[] = {0x40000000, 0x40000001, 0x40000002,
                                         0x40000020, 0x40000022, 0x40000023};
    static const uint32_t absent[] = {0x3b, 0x6e0};
    const uint32_t *list = writes ? read_only : absent;
    size_t length = writes ? ARRAY_SIZE(read_only) : ARRAY_SIZE(absent);
    char detail[DETAIL_MAX];
    bool ok = true;

    snprintf(detail, sizeof(detail), "%zu %s fail with EIO", length,
             writes ? "writes" : "reads");
    for (size_t item = 0; ok && item < length; item++) {
        uint64_t value = 0;
        int error = writes ? msr_write(fd, list[item], 0)
                           : msr_read(fd, list[item], &value);

        if (error != -EIO) {
            ok = false;
            snprintf(detail, sizeof(detail),
                     "%s MSR 0x%" PRIx32 " did not fail with EIO (error %d)",
                     writes ? "write of 0 to" : "read of", list[item], error);
        }
    }
    exhaustive_report(state, writes ? "X5" : "X6", cpu, ok, detail);
}

// The CI-only exhaustive check: X1 to X6 on every online CPU. It reports and
// exits, never powers off, and production boots never run it.
static int exhaustive_run(void)
{
    static const char *const ids[] = {"X1", "X2", "X3", "X4", "X5", "X6"};
    struct cpuinfo_parse parse;
    struct exhaustive state;
    bool cpus[MAX_CPUS];
    cpu_set_t original;
    char text[256];
    int online;

    memset(&state, 0, sizeof(state));
    if (read_text("/sys/devices/system/cpu/online", text, sizeof(text)) < 0 ||
        (online = parse_cpu_list(text, cpus)) <= 0) {
        state.failures = 1;
        return exhaustive_summary(&state, 0);
    }
    cpuinfo_init(&parse, &state.cpuinfo, cpus, false);
    (void)for_each_file_line("/proc/cpuinfo", cpuinfo_line, &parse);
    if (sched_getaffinity(0, sizeof(original), &original) != 0)
        CPU_ZERO(&original);
    for (int cpu = 0; cpu < MAX_CPUS; cpu++) {
        char detail[DETAIL_MAX];
        uint32_t basic[4];
        uint32_t out_of_range[4];
        char path[64];
        int fd;

        if (!cpus[cpu])
            continue;
        if (pin_to_cpu(cpu) != 0) {
            for (size_t item = 0; item < ARRAY_SIZE(ids); item++)
                exhaustive_report(&state, ids[item], cpu, false,
                                  "cannot run on this CPU");
            continue;
        }
        cpuid(0, 0, basic);
        cpuid(basic[0], 0, out_of_range);
        exhaustive_leaves(&state, "X1", cpu, 0x40000006, 0x400000ff, 1,
                          out_of_range);
        exhaustive_leaves(&state, "X2", cpu, 0x40000100, 0x4000ff00, 0x100,
                          out_of_range);
        snprintf(path, sizeof(path), "/dev/cpu/%d/msr", cpu);
        fd = open(path, O_RDWR | O_CLOEXEC);
        if (fd < 0) {
            snprintf(detail, sizeof(detail), "open %s: %s", path,
                     strerror(errno));
            for (size_t item = 2; item < ARRAY_SIZE(ids); item++)
                exhaustive_report(&state, ids[item], cpu, false, detail);
            continue;
        }
        exhaustive_x3(&state, fd, cpu);
        exhaustive_x4(&state, fd, cpu);
        exhaustive_faults(&state, fd, cpu, true);
        exhaustive_faults(&state, fd, cpu, false);
        close(fd);
    }
    if (CPU_COUNT(&original) > 0)
        (void)sched_setaffinity(0, sizeof(original), &original);
    return exhaustive_summary(&state, online);
}

// C6: `tsc` is current, and only tsc, refined-jiffies, or jiffies is
// available.
static bool check_clocksource_values(struct checks *checks, const char *current,
                                     char *available)
{
    bool tsc = false;

    if (strncmp(current, "tsc", 3) != 0 ||
        (current[3] != '\0' && current[3] != '\n'))
        return check_failed(checks, "C6", "current clocksource is %.*s",
                            (int)strcspn(current, "\n"), current);
    for (char *token = strtok(available, " \n"); token != NULL;
         token = strtok(NULL, " \n")) {
        if (strcmp(token, "tsc") == 0)
            tsc = true;
        else if (strcmp(token, "refined-jiffies") != 0 &&
                 strcmp(token, "jiffies") != 0)
            return check_failed(checks, "C6", "clocksource %.64s is available",
                                token);
    }
    if (!tsc)
        return check_failed(checks, "C6", "tsc is not an available clocksource");
    return true;
}

static bool check_clocksource(struct checks *checks)
{
    char current[64];
    char available[256];

    if (read_text(CLOCKSOURCE_DIR "/current_clocksource", current,
                  sizeof(current)) < 0 ||
        read_text(CLOCKSOURCE_DIR "/available_clocksource", available,
                  sizeof(available)) < 0)
        return check_failed(checks, "C6", "clocksource sysfs: %s",
                            strerror(errno));
    return check_clocksource_values(checks, current, available);
}

struct tick_section {
    bool seen;
    int mode;
    int state;
    char device[48];
    char handler[48];
};

// Line-by-line /proc/timer_list parser. The file lists every active hrtimer
// before the tick devices, so its size depends on the workload.
struct timer_list_parse {
    struct tick_section cpus[MAX_CPUS];
    struct tick_section broadcast;
    struct tick_section *current;
    int tick_mode;
};

static void timer_list_init(struct timer_list_parse *parse)
{
    static const struct tick_section empty = {false, -1, -1, "", ""};

    for (int index = 0; index < MAX_CPUS; index++)
        parse->cpus[index] = empty;
    parse->broadcast = empty;
    parse->current = NULL;
    parse->tick_mode = -1;
}

static void timer_list_line(void *context, char *line)
{
    struct timer_list_parse *parse = context;
    struct tick_section *current = parse->current;
    char name[48];
    int value;

    if (sscanf(line, "Tick Device: mode: %d", &value) == 1) {
        parse->tick_mode = value;
        parse->current = NULL;
    } else if (strncmp(line, "Broadcast device", 16) == 0) {
        parse->current = &parse->broadcast;
        parse->current->seen = true;
        parse->current->mode = parse->tick_mode;
    } else if (sscanf(line, "Per CPU device: %d", &value) == 1) {
        parse->current =
            value >= 0 && value < MAX_CPUS ? &parse->cpus[value] : NULL;
        if (parse->current != NULL) {
            parse->current->seen = true;
            parse->current->mode = parse->tick_mode;
        }
    } else if (current != NULL &&
               sscanf(line, "Clock Event Device: %47s", name) == 1) {
        snprintf(current->device, sizeof(current->device), "%s", name);
    } else if (current != NULL && sscanf(line, " mode: %d", &value) == 1) {
        current->state = value;
    } else if (current != NULL &&
               sscanf(line, " event_handler: %47s", name) == 1) {
        snprintf(current->handler, sizeof(current->handler), "%s", name);
    }
}

// C7 verdict: every CPU in SELECTED ticks with the LAPIC in one-shot mode
// through hrtimer_interrupt, and no broadcast device exists.
static bool timer_list_result(const struct timer_list_parse *parse,
                              const bool *selected, char *detail, size_t size)
{
    if (parse->broadcast.seen &&
        strcmp(parse->broadcast.device, "<NULL>") != 0) {
        snprintf(detail, size, "broadcast clock event device is %s",
                 parse->broadcast.device);
        return false;
    }
    for (int cpu = 0; cpu < MAX_CPUS; cpu++) {
        const struct tick_section *section = &parse->cpus[cpu];

        if (!selected[cpu])
            continue;
        if (!section->seen || strcmp(section->device, "lapic") != 0 ||
            strcmp(section->handler, "hrtimer_interrupt") != 0 ||
            section->mode != 1 || (section->state != 3 && section->state != 4)) {
            snprintf(detail, size,
                     "cpu %d tick device %s handler %s tick mode %d state %d",
                     cpu, section->seen ? section->device : "missing",
                     section->handler[0] != '\0' ? section->handler : "none",
                     section->mode, section->state);
            return false;
        }
    }
    return true;
}

static bool timer_list_verdict(char *text, const bool *selected, char *detail,
                               size_t size)
{
    static struct timer_list_parse parse;

    timer_list_init(&parse);
    for_each_text_line(text, timer_list_line, &parse);
    return timer_list_result(&parse, selected, detail, size);
}

// Polls C7 for at most WAIT_NS, because a CPU switches to one-shot mode at
// its first tick. A zero wait checks once.
static bool check_timer_list(struct checks *checks, int64_t wait_ns)
{
    static struct timer_list_parse parse;
    char detail[DETAIL_MAX] = "";
    int64_t deadline = clock_ns(CLOCK_MONOTONIC) + wait_ns;

    for (;;) {
        timer_list_init(&parse);
        if (for_each_file_line("/proc/timer_list", timer_list_line, &parse) !=
            0)
            return check_failed(checks, "C7", "read /proc/timer_list: %s",
                                strerror(errno));
        if (timer_list_result(&parse, checks->online, detail, sizeof(detail)))
            return true;
        if (clock_ns(CLOCK_MONOTONIC) >= deadline)
            return check_failed(checks, "C7", "%s", detail);
        nanosleep(&(struct timespec){0, 10 * NSEC_PER_MSEC}, NULL);
    }
}

// C8: no VMBus, no cpufreq, and the production RCU stall settings.
static bool check_platform(struct checks *checks)
{
    struct stat info;
    int64_t value = -1;

    if (stat("/sys/bus/vmbus", &info) == 0)
        return check_failed(checks, "C8", "/sys/bus/vmbus exists");
    if (stat("/sys/devices/system/cpu/cpufreq/policy0", &info) == 0)
        return check_failed(checks, "C8", "cpufreq policy0 exists");
    if (read_int64(RCU_STALL_SUPPRESS, &value) != 0 || value != 0)
        return check_failed(checks, "C8", "rcu_cpu_stall_suppress is %" PRId64,
                            value);
    if (read_int64(RCU_STALL_TIMEOUT, &value) != 0 || value != 21)
        return check_failed(checks, "C8", "rcu_cpu_stall_timeout is %" PRId64,
                            value);
    return true;
}

// C9: no clock workaround on the kernel command line.
static bool check_cmdline_text(struct checks *checks, char *cmdline)
{
    static const char *const prefixes[] = {"tsc_early_khz=", "lapic_timer_hz="};
    static const char *const tokens[] = {"notsc", "nolapic", "nolapic_timer",
                                         "tsc=unstable", "hpet=force"};

    for (char *token = strtok(cmdline, " \t\n"); token != NULL;
         token = strtok(NULL, " \t\n")) {
        bool bad = strncmp(token, "clocksource=", 12) == 0 &&
                   strcmp(token + 12, "tsc") != 0;

        for (size_t index = 0; index < ARRAY_SIZE(prefixes); index++)
            bad = bad || strncmp(token, prefixes[index],
                                 strlen(prefixes[index])) == 0;
        for (size_t index = 0; index < ARRAY_SIZE(tokens); index++)
            bad = bad || strcmp(token, tokens[index]) == 0;
        if (bad)
            return check_failed(checks, "C9", "command line contains %.120s",
                                token);
    }
    return true;
}

static bool check_cmdline(struct checks *checks)
{
    char text[4096];

    if (read_text("/proc/cmdline", text, sizeof(text)) < 0)
        return check_failed(checks, "C9", "read /proc/cmdline: %s",
                            strerror(errno));
    return check_cmdline_text(checks, text);
}

static pid_t daemon_pid(void)
{
    char path[64];
    char comm[32];
    int64_t pid;

    if (read_int64(DAEMON_PID_PATH, &pid) != 0 || pid <= 1 || pid > INT_MAX ||
        kill((pid_t)pid, 0) != 0)
        return -1;
    snprintf(path, sizeof(path), "/proc/%" PRId64 "/comm", pid);
    if (read_text(path, comm, sizeof(comm)) < 0 ||
        strcmp(comm, "nvx-time\n") != 0)
        return -1;
    return (pid_t)pid;
}

// The asynchronous boot checks and restore step 13 start at idle priority, so
// that on few vCPUs they never take a CPU from the workload, and continue at
// normal priority after IDLE_BOUND_NS; the watcher and the discipline keep
// normal priority. musl does not implement sched_setscheduler(), so this uses
// the per-thread system call: PID 0 is the calling thread.
static void set_idle_priority(pid_t pid, bool idle)
{
    struct sched_param param = {0};

    (void)syscall(SYS_sched_setscheduler, pid, idle ? SCHED_IDLE : SCHED_OTHER,
                  &param);
}

// Raises every thread of this process to normal priority, twice, so that a
// thread an idle thread created during the first pass is raised too.
static void promote_threads(void)
{
    for (int pass = 0; pass < 2; pass++) {
        DIR *tasks = opendir("/proc/self/task");
        struct dirent *entry;

        if (tasks == NULL)
            return;
        while ((entry = readdir(tasks)) != NULL) {
            long tid = strtol(entry->d_name, NULL, 10);

            if (tid > 0)
                set_idle_priority((pid_t)tid, false);
        }
        closedir(tasks);
    }
}

// A normal-priority thread that raises the boot checker's idle threads when
// the checks still run IDLE_BOUND_NS after they started.
struct promoter {
    pthread_t thread;
    pthread_mutex_t lock;
    pthread_cond_t stop;
    struct timespec deadline;
    bool stopped;
};

static void *promoter_run(void *argument)
{
    struct promoter *promoter = argument;
    bool stopped;
    int result = 0;

    pthread_mutex_lock(&promoter->lock);
    while (!promoter->stopped && result == 0)
        result = pthread_cond_timedwait(&promoter->stop, &promoter->lock,
                                        &promoter->deadline);
    stopped = promoter->stopped;
    pthread_mutex_unlock(&promoter->lock);
    if (!stopped)
        promote_threads();
    return NULL;
}

// Starts PROMOTER, at the caller's priority, for checks that started at
// START_NS. Returns true when it runs.
static bool promoter_start(struct promoter *promoter, int64_t start_ns)
{
    int64_t deadline = start_ns + IDLE_BOUND_NS;
    pthread_condattr_t attributes;
    bool ready;

    promoter->stopped = false;
    promoter->deadline.tv_sec = deadline / NSEC_PER_SEC;
    promoter->deadline.tv_nsec = deadline % NSEC_PER_SEC;
    if (pthread_condattr_init(&attributes) != 0)
        return false;
    ready = pthread_condattr_setclock(&attributes, CLOCK_MONOTONIC) == 0 &&
            pthread_cond_init(&promoter->stop, &attributes) == 0;
    pthread_condattr_destroy(&attributes);
    return ready && pthread_mutex_init(&promoter->lock, NULL) == 0 &&
           pthread_create(&promoter->thread, NULL, promoter_run, promoter) ==
               0;
}

static void promoter_stop(struct promoter *promoter)
{
    pthread_mutex_lock(&promoter->lock);
    promoter->stopped = true;
    pthread_cond_signal(&promoter->stop);
    pthread_mutex_unlock(&promoter->lock);
    pthread_join(promoter->thread, NULL);
}

// Restore steps 12 and 13 on the readiness path: the acknowledgement when the
// host gated its input, then hand step 13 to DAEMON. The sigqueue() value is
// the readiness path's wall time since START_NS (CLOCK_MONOTONIC), in
// microseconds clamped to 32 bits, read after the acknowledgement so that the
// read does not delay it. The readiness path's CPU time is not reported: the
// restore latency gate measures that path. Returns -1, without acknowledging,
// when no daemon runs; 1 when the daemon vanished after the acknowledgement.
// The caller has enabled the ports when ACK is set.
static int signal_restore(pid_t daemon, bool ack, int64_t start_ns)
{
    union sigval value;
    int64_t elapsed_us;

    if (daemon <= 0 || kill(daemon, 0) != 0)
        return -1;
    if (ack)
        outb(SNAPSHOT_ACKNOWLEDGE, PORT_SNAPSHOT);
    elapsed_us = (clock_ns(CLOCK_MONOTONIC) - start_ns) / NSEC_PER_USEC;
    if (elapsed_us > UINT32_MAX)
        elapsed_us = UINT32_MAX;
    value.sival_ptr = (void *)(uintptr_t)(elapsed_us < 0 ? 0 : elapsed_us);
    return sigqueue(daemon, SIGUSR1, value) == 0 ? 0 : 1;
}

// C10: the daemon runs, has recorded no violation, and no stall was counted.
static bool check_daemon(struct checks *checks)
{
    struct time_state state;
    int64_t stalls = -1;

    if (daemon_pid() < 0)
        return check_failed(checks, "C10", "the time daemon is not running");
    if (state_load(&state) != 0)
        return check_failed(checks, "C10", "time state is unreadable");
    if (state.violations != 0)
        return check_failed(checks, "C10",
                            "the time daemon recorded %" PRIu64 " violations",
                            state.violations);
    if (read_int64(RCU_STALL_COUNT, &stalls) != 0 || stalls != 0)
        return check_failed(checks, "C10", "rcu_stall_count is %" PRId64,
                            stalls);
    return true;
}

// C11: on the CI debug kernel, the soft-lockup and hung-task detectors run.
static bool check_debug_watchdogs(struct checks *checks)
{
    bool soft = access(SOFT_WATCHDOG, F_OK) == 0;
    bool hung = access(HUNG_TASK_TIMEOUT, F_OK) == 0;
    int64_t value = -1;

    if (!soft && !hung)
        return true;
    if (!soft || read_int64(SOFT_WATCHDOG, &value) != 0 || value != 1)
        return check_failed(checks, "C11", "soft_watchdog is %" PRId64, value);
    if (!hung || read_int64(HUNG_TASK_TIMEOUT, &value) != 0 || value == 0)
        return check_failed(checks, "C11",
                            "hung_task_timeout_secs is %" PRId64, value);
    return true;
}

// C1, C2, and C3 on every CPU in SELECTED, pinned to each in turn. The
// three checks are independent, so a report-only run shows each failure;
// the loop stops at the first CPU that fails.
static bool check_cpus(struct checks *checks, const bool *selected,
                       bool identity, bool msrs)
{
    cpu_set_t original;
    bool ok = true;

    if (sched_getaffinity(0, sizeof(original), &original) != 0)
        return check_failed(checks, "C1", "sched_getaffinity: %s",
                            strerror(errno));
    for (int cpu = 0; ok && cpu < MAX_CPUS; cpu++) {
        if (!selected[cpu])
            continue;
        if (pin_to_cpu(cpu) != 0) {
            ok = check_failed(checks, "C1", "cannot run on cpu %d: %s", cpu,
                              strerror(errno));
            break;
        }
        if (identity) {
            if (!check_identity(checks, cpu))
                ok = false;
            if (!check_time_bits(checks, cpu))
                ok = false;
        }
        if (msrs && !check_msrs(checks, cpu, true))
            ok = false;
    }
    (void)sched_setaffinity(0, sizeof(original), &original);
    return ok;
}

struct cpu_job {
    pthread_t thread;
    struct checks checks;
    bool identity;
    bool msrs;
    bool running;
    int cpu;
};

static void *cpu_job_run(void *argument)
{
    struct cpu_job *job = argument;

    if (pin_to_cpu(job->cpu) != 0) {
        check_failed(&job->checks, "C1", "cannot run on cpu %d: %s", job->cpu,
                     strerror(errno));
        return NULL;
    }
    if (job->identity) {
        check_identity(&job->checks, job->cpu);
        check_time_bits(&job->checks, job->cpu);
    }
    if (job->msrs)
        check_msrs(&job->checks, job->cpu, true);
    return NULL;
}

// A boot check that runs in its own thread beside the per-CPU checks, on a
// private copy of the checks.
struct side_job {
    pthread_t thread;
    struct checks checks;
    void (*run)(struct checks *checks, void *context);
    void *context;
    bool running;
};

static void *side_job_run(void *argument)
{
    struct side_job *job = argument;

    job->run(&job->checks, job->context);
    return NULL;
}

// check_cpus() with every CPU in its own thread pinned to it, beside the
// SIDES checks, each in a thread of its own, while the caller runs FOREGROUND
// (when set). Running them in parallel keeps the cost from growing with the
// CPU count and leaves the caller on its warm CPU. The threads share one
// stack mapping, so creating one maps nothing. Each job works on a copy of
// CHECKS whose failures are added back; a job whose thread cannot start runs
// in the caller afterwards.
static void check_cpus_parallel(struct checks *checks, const bool *selected,
                                bool identity, bool msrs,
                                struct side_job *sides, int side_count,
                                void (*foreground)(struct checks *, void *),
                                void *foreground_context)
{
    enum { STACK_SIZE = 128 * 1024 };
    static struct cpu_job jobs[MAX_CPUS];
    bool serial[MAX_CPUS] = {false};
    bool any_serial = false;
    pthread_attr_t attributes;
    pthread_attr_t *attr = NULL;
    size_t threads = (size_t)side_count;
    size_t next = 0;
    uint8_t *stacks;

    for (int cpu = 0; cpu < MAX_CPUS; cpu++)
        threads += selected[cpu] ? 1 : 0;
    // With one CPU, threads only add their own cost.
    if (threads <= (size_t)side_count + 1) {
        if (foreground != NULL)
            foreground(checks, foreground_context);
        for (int index = 0; index < side_count; index++) {
            struct side_job *side = &sides[index];

            side->checks = *checks;
            side->checks.failures = 0;
            side->run(&side->checks, side->context);
            checks->failures += side->checks.failures;
        }
        check_cpus(checks, selected, identity, msrs);
        return;
    }
    stacks = mmap(NULL, threads * STACK_SIZE, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (stacks != MAP_FAILED && pthread_attr_init(&attributes) == 0)
        attr = &attributes;
    for (int index = 0; index < side_count; index++) {
        struct side_job *side = &sides[index];

        side->checks = *checks;
        side->checks.failures = 0;
        if (attr != NULL)
            pthread_attr_setstack(attr, stacks + STACK_SIZE * next++,
                                  STACK_SIZE);
        side->running =
            pthread_create(&side->thread, attr, side_job_run, side) == 0;
    }
    for (int cpu = 0; cpu < MAX_CPUS; cpu++) {
        struct cpu_job *job = &jobs[cpu];

        job->running = false;
        if (!selected[cpu])
            continue;
        job->checks = *checks;
        job->checks.failures = 0;
        job->identity = identity;
        job->msrs = msrs;
        job->cpu = cpu;
        if (attr != NULL)
            pthread_attr_setstack(attr, stacks + STACK_SIZE * next++,
                                  STACK_SIZE);
        if (pthread_create(&job->thread, attr, cpu_job_run, job) == 0)
            job->running = true;
        else
            serial[cpu] = any_serial = true;
    }
    if (foreground != NULL)
        foreground(checks, foreground_context);
    for (int index = 0; index < side_count; index++) {
        struct side_job *side = &sides[index];

        if (side->running)
            pthread_join(side->thread, NULL);
        else
            side->run(&side->checks, side->context);
        checks->failures += side->checks.failures;
    }
    for (int cpu = 0; cpu < MAX_CPUS; cpu++) {
        struct cpu_job *job = &jobs[cpu];

        if (!job->running)
            continue;
        pthread_join(job->thread, NULL);
        checks->failures += job->checks.failures;
        // C3 reads the partition-wide rates on CPU 0 only.
        if (cpu == 0 && msrs) {
            checks->tsc_hz = job->checks.tsc_hz;
            checks->lapic_hz = job->checks.lapic_hz;
        }
    }
    if (attr != NULL)
        pthread_attr_destroy(attr);
    if (stacks != MAP_FAILED)
        munmap(stacks, threads * STACK_SIZE);
    if (any_serial)
        check_cpus(checks, serial, identity, msrs);
}
// ---------------------------------------------------------------------------
// Host time samples and clock adjustment
// ---------------------------------------------------------------------------

// Latches a sample with selector 0xa7 and reads it from 0xeb. CLOCK_REALTIME
// brackets the selector write, so the host instant lies in [t0, t1]. The
// caller holds the portb lock.
static int take_sample(struct time_sample *sample, char *detail, size_t size)
{
    uint8_t bytes[SAMPLE_SIZE];

    sample->t0 = clock_ns(CLOCK_REALTIME);
    outb(SELECT_TIME_SAMPLE, PORTB_STATUS);
    sample->t1 = clock_ns(CLOCK_REALTIME);
    portb_read(PORTB_WINDOW, bytes, sizeof(bytes));
    return parse_sample(bytes, sample, detail, size);
}

static int lock_portb(char *detail, size_t size)
{
    int lock = lock_file(PORTB_LOCK_PATH);

    if (lock < 0)
        snprintf(detail, size, "portb lock: %s", strerror(errno));
    return lock;
}

// Takes up to ATTEMPTS samples of generation G and keeps the first whose
// uncertainty is within BOUND_NS. Returns 0 when one is, 1 when only wider
// samples were seen (the narrowest is returned), and -1 when none was valid
// or the VMM has no time-sample window. A sample from another generation
// stops the search with -2. The caller holds the portb lock.
static int best_sample(int attempts, int64_t bound_ns, uint32_t generation,
                       struct time_sample *best, int64_t *theta,
                       int64_t *epsilon, char *detail, size_t size)
{
    unsigned status;
    int result = -1;

    *epsilon = INT64_MAX;
    if (enable_ports() != 0) {
        snprintf(detail, size, "ioperm: %s", strerror(errno));
        return -1;
    }
    status = inb(PORTB_STATUS);
    if ((status & PORTB_TIME_WINDOW) == 0) {
        snprintf(detail, size, "portb status 0x%02x has no time-sample window",
                 status);
        return -1;
    }
    for (int attempt = 0; attempt < attempts; attempt++) {
        struct time_sample sample;
        int64_t sample_theta;
        int64_t sample_epsilon;

        if (take_sample(&sample, detail, size) != 0)
            continue;
        if (sample.generation != generation) {
            snprintf(detail, size,
                     "time sample generation %" PRIu32 ", expected %" PRIu32,
                     sample.generation, generation);
            return -2;
        }
        pair_clocks(sample.t0, sample.t1, sample.utc_ns, &sample_theta,
                    &sample_epsilon);
        if (sample_epsilon < *epsilon) {
            *best = sample;
            *theta = sample_theta;
            *epsilon = sample_epsilon;
            result = 1;
        }
        if (sample_epsilon <= bound_ns)
            return 0;
        snprintf(detail, size,
                 "time sample uncertainty %" PRId64 " ns exceeds %" PRId64
                 " ns",
                 sample_epsilon, bound_ns);
    }
    return result;
}

static int step_clock(int64_t offset_ns)
{
    struct timex tx;

    memset(&tx, 0, sizeof(tx));
    tx.modes = ADJ_SETOFFSET | ADJ_NANO;
    offset_to_timeval(offset_ns, &tx.time);
    return adjtimex(&tx) < 0 ? -1 : 0;
}

static int set_frequency(int64_t frequency)
{
    struct timex tx;

    memset(&tx, 0, sizeof(tx));
    tx.modes = ADJ_FREQUENCY | ADJ_STATUS;
    tx.freq = (long)frequency;
    tx.status = STA_PLL | STA_NANO;
    return adjtimex(&tx) < 0 ? -1 : 0;
}

static int64_t current_frequency(void)
{
    struct timex tx;

    memset(&tx, 0, sizeof(tx));
    return adjtimex(&tx) < 0 ? 0 : (int64_t)tx.freq;
}

// C12: one sample within 1 ms (three attempts), then a clock step. The boot
// step is not a discontinuity.
static bool check_initial_sample(struct checks *checks, int64_t *theta,
                                 int64_t *epsilon)
{
    struct time_sample sample;
    char detail[DETAIL_MAX] = "no valid time sample";
    int result;
    int lock;

    lock = lock_portb(detail, sizeof(detail));
    if (lock < 0)
        return check_failed(checks, "C12", "%s", detail);
    result = best_sample(SAMPLE_ATTEMPTS, BOOT_BOUND_NS, g_generation, &sample,
                         theta, epsilon, detail, sizeof(detail));
    close(lock);
    if (result != 0)
        return check_failed(checks, "C12", "%s", detail);
    if (step_clock(*theta) != 0)
        return check_failed(checks, "C12", "adjtimex step: %s",
                            strerror(errno));
    return true;
}

// ---------------------------------------------------------------------------
// Daemon: violation watcher, wall-clock discipline, restore completion
// ---------------------------------------------------------------------------

struct daemon {
    int kmsg;
    int signals;
    int timer;
    int polls;
    int64_t last_accepted_ns;
    pid_t worker_pid;
    int64_t worker_promote_ns;
    int64_t worker_deadline_ns;
    bool suppression_released;
    bool restore_pending;
    int64_t restore_due_ns;
    int64_t readiness_us;
};

static void watcher_consume(void *context, const char *message)
{
    const char *code = match_watch_rule(message);

    (void)context;
    if (code != NULL)
        fail_fatal(STATUS_VIOLATION, code, "watcher", PHASE_RUNTIME, "%.400s",
                   message);
}

static void watch_kernel_log(struct daemon *daemon)
{
    if (kmsg_drain(daemon->kmsg, watcher_consume, daemon) == -EPIPE)
        fail_fatal(STATUS_VIOLATION, "G_KMSG_OVERRUN", "watcher",
                   PHASE_RUNTIME, "kernel log records were lost (EPIPE)");
}

static int arm_timer(struct daemon *daemon)
{
    struct itimerspec period;

    memset(&period, 0, sizeof(period));
    period.it_value.tv_sec =
        daemon->polls < FAST_POLLS ? FAST_PERIOD_S : SLOW_PERIOD_S;
    return timerfd_settime(daemon->timer, 0, &period, NULL);
}

struct poll_outcome {
    bool accepted;
    bool stepped;
    bool synchronized;
    int64_t theta;
    int64_t epsilon;
    int64_t step_realtime_ns;
    int64_t frequency;
};

static void apply_poll(struct time_state *state, const void *context)
{
    const struct poll_outcome *outcome = context;

    if (outcome->accepted) {
        state->samples++;
        state->offset_ns = outcome->theta;
        state->uncertainty_ns = outcome->epsilon;
        state->frequency_ppb = frequency_to_ppb(outcome->frequency);
        snprintf(state->last_sample_error, sizeof(state->last_sample_error),
                 "none");
        if (outcome->stepped) {
            state->discontinuities++;
            snprintf(state->last_discontinuity,
                     sizeof(state->last_discontinuity), "step");
            state->last_step_ns = outcome->theta;
            state->last_step_realtime_ns = outcome->step_realtime_ns;
        }
    } else {
        state->rejected_samples++;
        snprintf(state->last_sample_error, sizeof(state->last_sample_error),
                 "G_SAMPLE_UNCERTAIN");
    }
    state->synchronized = outcome->synchronized;
}

// One discipline poll: the RCU stall backstop, then up to three samples. A
// step reapplies the frequency and status that the step's NTP reset clears.
// The poll holds the portb lock from its first sample to the published state,
// as a capture does from its request to the restored clock, so a snapshot
// never contains a half-applied poll: a poll blocked behind a capture samples
// after the restore, sees the new generation, and is discarded.
static void discipline_poll(struct daemon *daemon)
{
    struct poll_outcome outcome;
    struct time_sample sample;
    char detail[DETAIL_MAX] = "";
    bool fast = daemon->polls < FAST_POLLS;
    int64_t stalls = 0;
    int result;
    int lock;

    memset(&outcome, 0, sizeof(outcome));
    if (read_int64(RCU_STALL_COUNT, &stalls) == 0 && stalls != 0)
        fail_fatal(STATUS_VIOLATION, "G_RCU_STALL", "watcher", PHASE_RUNTIME,
                   "rcu_stall_count is %" PRId64, stalls);
    daemon->polls++;
    lock = lock_portb(detail, sizeof(detail));
    result = lock < 0 ? -1
                      : best_sample(SAMPLE_ATTEMPTS, POLL_BOUND_NS,
                                    g_generation, &sample, &outcome.theta,
                                    &outcome.epsilon, detail, sizeof(detail));
    if (result == -2) {
        close(lock);
        return;
    }
    if (result == 0) {
        struct timex tx;
        int64_t frequency = current_frequency();

        outcome.stepped = plan_correction(outcome.theta, outcome.epsilon, fast,
                                          &tx);
        outcome.accepted = adjtimex(&tx) >= 0 &&
                           (!outcome.stepped || set_frequency(frequency) == 0);
        outcome.step_realtime_ns = clock_ns(CLOCK_REALTIME);
        outcome.frequency = current_frequency();
        if (outcome.accepted)
            daemon->last_accepted_ns = clock_ns(CLOCK_MONOTONIC);
    }
    outcome.synchronized =
        daemon->last_accepted_ns != 0 &&
        clock_ns(CLOCK_MONOTONIC) - daemon->last_accepted_ns <=
            SYNCHRONIZED_AGE_NS;
    (void)state_apply(apply_poll, &outcome);
    if (lock >= 0)
        close(lock);
}

// Waits for a full normal RCU grace period: with rcu_normal set, the
// synchronize_rcu_expedited() in every unmount waits for one that starts
// after the call, on any number of CPUs. The private mount namespace makes the
// caller a process of its own: the restore worker, or a child. Returns 0 when
// the release completed.
static int grace_period_release(void)
{
    int64_t normal = 0;
    bool saved = read_int64(RCU_NORMAL, &normal) == 0 &&
                 write_int64(RCU_NORMAL, 1) == 0;
    int status = 1;

    if (saved && unshare(CLONE_NEWNS) == 0 &&
        mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL) == 0) {
        (void)mkdir(RCU_SYNC_PATH, 0700);
        if (mount("nvx-rcu-sync", RCU_SYNC_PATH, "tmpfs",
                  MS_NOSUID | MS_NODEV | MS_NOEXEC, "size=4k") == 0 &&
            umount2(RCU_SYNC_PATH, 0) == 0)
            status = 0;
        (void)rmdir(RCU_SYNC_PATH);
    }
    if (saved && write_int64(RCU_NORMAL, normal) != 0)
        status = 1;
    return status;
}

// Runs the grace-period release in a child and waits at most TIMEOUT_S
// seconds for it, for callers without the daemon's event loop. A release
// still pending at the deadline counts as done: the grace period is stuck,
// and the kernel then reports the real stall.
static bool release_with_deadline(int64_t timeout_s)
{
    int64_t deadline = clock_ns(CLOCK_MONOTONIC) + timeout_s * NSEC_PER_SEC;
    sigset_t mask;
    sigset_t previous;
    bool released = false;
    pid_t pid;

    sigemptyset(&mask);
    sigaddset(&mask, SIGCHLD);
    sigprocmask(SIG_BLOCK, &mask, &previous);
    pid = fork();
    if (pid == 0) {
        sigprocmask(SIG_SETMASK, &previous, NULL);
        _exit(grace_period_release());
    }
    while (pid > 0) {
        int status;
        pid_t done = waitpid(pid, &status, WNOHANG);
        int64_t remaining = deadline - clock_ns(CLOCK_MONOTONIC);

        if (done == pid) {
            released = WIFEXITED(status) && WEXITSTATUS(status) == 0;
            break;
        }
        if (done < 0 && errno != EINTR)
            break;
        if (remaining <= 0) {
            released = true;
            break;
        }
        (void)sigtimedwait(&mask, NULL,
                           &(struct timespec){remaining / NSEC_PER_SEC,
                                              remaining % NSEC_PER_SEC});
    }
    sigprocmask(SIG_SETMASK, &previous, NULL);
    return released;
}

static void run_restore_checks(struct checks *checks, const bool *new_cpus,
                               int new_count);

static int parse_new_cpus(const char *text, bool *cpus)
{
    if (strcmp(text, "") == 0 || strcmp(text, "none") == 0) {
        memset(cpus, 0, sizeof(bool) * MAX_CPUS);
        return 0;
    }
    return parse_cpu_list(text, cpus);
}

// Parses the restore record that restore-finish writes for the daemon: the
// generation it finished and the CPUs that activation brought online.
// Returns the number of those CPUs, or -1 when the record is malformed.
static int parse_restore_record(const char *text, unsigned long long *generation,
                                bool *new_cpus)
{
    char cpus[128] = "none";

    if (sscanf(text, "generation=%llu new_cpus=%127s", generation, cpus) < 1)
        return -1;
    return parse_new_cpus(cpus, new_cpus);
}

// The times that restore_work records. The wall time is READINESS_US, the
// readiness path's, plus the checks' since START_NS, when they were due. The
// CPU time is step 13's alone: this process's since CPU_BASE_NS, plus the
// daemon's preparation, PRIOR_CPU_NS up to the fork and the fork's own share,
// which the daemon writes to FORK_FD (-1 when there is none) once the fork
// returns.
struct restore_timing {
    int64_t start_ns;
    int64_t readiness_us;
    int64_t prior_cpu_ns;
    int64_t cpu_base_ns;
    int fork_fd;
};

// Step 13's CPU time so far, in a process that TIMING describes. The read
// waits for the daemon's fork share if the daemon has not yet written it.
static int64_t step13_cpu_ns(const struct restore_timing *timing)
{
    int64_t cpu = timing->prior_cpu_ns + cpu_time_ns() - timing->cpu_base_ns;
    int64_t fork_share = 0;

    if (timing->fork_fd >= 0 &&
        read(timing->fork_fd, &fork_share, sizeof(fork_share)) ==
            sizeof(fork_share) &&
        fork_share > 0)
        cpu += fork_share;
    return cpu;
}

// Forks the step 13 worker at SCHED_IDLE for a daemon whose preparation
// started at CPU_START_NS, its CPU clock, and completes TIMING's CPU fields
// for the child. The parent returns to normal priority as soon as the fork
// does. Its fork costs CPU time after the child's copy of TIMING was taken, so
// it writes that share to a pipe that the child reads. Returns fork()'s
// result.
static pid_t fork_step13(int64_t cpu_start_ns, struct restore_timing *timing)
{
    int fds[2] = {-1, -1};
    int64_t fork_share;
    pid_t pid;

    if (pipe2(fds, O_CLOEXEC) != 0)
        fds[0] = fds[1] = -1;
    timing->fork_fd = fds[0];
    timing->cpu_base_ns = 0;
    set_idle_priority(0, true);
    timing->prior_cpu_ns = cpu_time_ns() - cpu_start_ns;
    pid = fork();
    if (pid == 0) {
        if (fds[1] >= 0)
            close(fds[1]);
        return 0;
    }
    set_idle_priority(0, false);
    if (fds[0] >= 0)
        close(fds[0]);
    timing->fork_fd = -1;
    if (fds[1] >= 0) {
        fork_share = cpu_time_ns() - cpu_start_ns - timing->prior_cpu_ns;
        if (pid > 0)
            (void)write_all(fds[1], &fork_share, sizeof(fork_share));
        close(fds[1]);
    }
    return pid;
}

// Restore step 13, fail-fast: the step 11 restore checks, the grace-period
// release, the saved watchdog settings, the deferred C7, and the restore
// check's record. A child of the daemon runs it at SCHED_IDLE from
// DEFERRED_START_NS after the acknowledgement; the daemon raises it to normal
// priority IDLE_BOUND_NS later and releases the suppression itself at the
// stall timeout. restore-finish runs it inline, with its own deadline
// (WITH_DEADLINE), in report-only mode when no daemon runs. Neither recorded
// time counts the grace period wait or C7. Returns 0, or 1 when a report-only
// release failed.
static int restore_work(const bool *new_cpus, int new_count, bool with_deadline,
                        const struct restore_timing *timing)
{
    int64_t timeout_s = DEFAULT_STALL_TIMEOUT_S;
    char detail[DETAIL_MAX];
    struct checks checks;
    int64_t elapsed;
    int64_t cpu;
    bool released;

    if (checks_init(&checks, PHASE_RESTORE) != 0)
        check_failed(&checks, "C6", "cannot read the CPU sets");
    else
        run_restore_checks(&checks, new_cpus, new_count);
    elapsed = clock_ns(CLOCK_MONOTONIC) - timing->start_ns;
    cpu = step13_cpu_ns(timing);
    if (with_deadline) {
        (void)read_int64(RCU_STALL_TIMEOUT, &timeout_s);
        released = release_with_deadline(timeout_s);
    } else {
        released = grace_period_release() == 0;
    }
    if (!released)
        fail_fatal(STATUS_REPAIR, "G_REPAIR_SUPPRESSION", "repair",
                   PHASE_RESTORE, "the RCU grace-period release failed");
    if (suppression_restore(detail, sizeof(detail), true) != 0)
        fail_fatal(STATUS_REPAIR, "G_REPAIR_SUPPRESSION", "repair",
                   PHASE_RESTORE, "%s", detail);
    check_timer_list(&checks, TIMER_LIST_WAIT_NS);
    if (record_check(PHASE_RESTORE, "ok", checks.online_count,
                     timing->readiness_us + elapsed / NSEC_PER_USEC,
                     cpu / NSEC_PER_USEC, 0, 0, checks.failures) != 0)
        check_failed(&checks, "C10", "cannot record the restore check: %s",
                     strerror(errno));
    return released ? 0 : 1;
}

// Starts restore step 13 when it is due, DEFERRED_START_NS after the
// acknowledgement, in a child at SCHED_IDLE, so the watcher and the
// discipline keep normal priority; the fork itself also runs at SCHED_IDLE.
// The readiness path's wall time came with the signal.
static void start_restore(struct daemon *daemon)
{
    int64_t cpu_start = cpu_time_ns();
    bool new_cpus[MAX_CPUS] = {false};
    struct restore_timing timing;
    struct time_state state;
    unsigned long long generation;
    int64_t timeout_s = DEFAULT_STALL_TIMEOUT_S;
    char text[256];
    int new_count = 0;
    pid_t pid;

    if (daemon->worker_pid > 0)
        return;
    if (state_load(&state) != 0) {
        fail_fatal(STATUS_REPAIR, "G_REPAIR_SUPPRESSION", "repair",
                   PHASE_RESTORE, "the time state is unreadable");
        return;
    }
    g_generation = (uint32_t)state.generation;
    if (read_text(RESTORE_PATH, text, sizeof(text)) >= 0) {
        new_count = parse_restore_record(text, &generation, new_cpus);
        if (new_count < 0) {
            fail_fatal(STATUS_REPAIR, "G_REPAIR_SUPPRESSION", "repair",
                       PHASE_RESTORE, "the restore record is malformed");
            return;
        }
        if (generation != state.generation) {
            memset(new_cpus, 0, sizeof(new_cpus));
            new_count = 0;
        }
    }
    (void)read_int64(RCU_STALL_TIMEOUT, &timeout_s);
    // The worker's wall time runs from when step 13 was due. Its CPU time is
    // step 13's: this preparation, the fork, and the worker's own.
    timing.start_ns = daemon->restore_due_ns;
    timing.readiness_us = daemon->readiness_us;
    pid = fork_step13(cpu_start, &timing);
    if (pid == 0)
        _exit(restore_work(new_cpus, new_count, false, &timing));
    if (pid < 0) {
        fail_fatal(STATUS_REPAIR, "G_REPAIR_SUPPRESSION", "repair",
                   PHASE_RESTORE, "cannot start the restore work: %s",
                   strerror(errno));
        return;
    }
    daemon->worker_pid = pid;
    daemon->worker_promote_ns = daemon->restore_due_ns + IDLE_BOUND_NS;
    daemon->worker_deadline_ns =
        clock_ns(CLOCK_MONOTONIC) + timeout_s * NSEC_PER_SEC;
    daemon->suppression_released = false;
}

// Releases the stall suppression for a worker whose grace period is still
// pending at the stall timeout: the grace period is stuck, and the kernel
// then reports the real stall.
static void release_suppression(struct daemon *daemon)
{
    char detail[DETAIL_MAX];

    daemon->suppression_released = true;
    if (suppression_restore(detail, sizeof(detail), true) != 0)
        fail_fatal(STATUS_REPAIR, "G_REPAIR_SUPPRESSION", "repair",
                   PHASE_RESTORE, "%s", detail);
}

static void reap_children(struct daemon *daemon)
{
    int status;
    pid_t pid;

    while ((pid = waitpid(-1, &status, WNOHANG)) > 0) {
        if (pid != daemon->worker_pid)
            continue;
        daemon->worker_pid = 0;
        daemon->worker_promote_ns = 0;
        // A failing worker powered the guest off itself, or reported in
        // report-only mode; a crash leaves the suppression to the daemon.
        if (!WIFEXITED(status)) {
            release_suppression(daemon);
            fail_fatal(STATUS_REPAIR, "G_REPAIR_SUPPRESSION", "repair",
                       PHASE_RESTORE, "the restore work ended with signal %d",
                       WTERMSIG(status));
        }
        // Step 13 ends by restarting the discipline at the fast cadence.
        daemon->polls = 0;
        (void)arm_timer(daemon);
    }
}

static void handle_signals(struct daemon *daemon)
{
    struct signalfd_siginfo info;

    while (read(daemon->signals, &info, sizeof(info)) == sizeof(info)) {
        if (info.ssi_signo == SIGTERM)
            _exit(0);
        // sigqueue() carries the readiness path's wall time. Step 13 is due
        // DEFERRED_START_NS later; the repair paired the clock with the
        // packet's UTC now.
        if (info.ssi_signo == SIGUSR1) {
            daemon->restore_pending = true;
            daemon->last_accepted_ns = clock_ns(CLOCK_MONOTONIC);
            daemon->restore_due_ns =
                daemon->last_accepted_ns + DEFERRED_START_NS;
            daemon->readiness_us = info.ssi_code == SI_QUEUE
                                       ? (int64_t)(info.ssi_ptr & UINT32_MAX)
                                       : 0;
        }
    }
    // A worker that ended, if any: the daemon loop starts a due restore only
    // once the previous worker is reaped.
    reap_children(daemon);
}

// Sets up the daemon's signal and timer descriptors and its OOM exemption.
// Returns 0 or an errno value.
static int daemon_setup(struct daemon *daemon)
{
    sigset_t mask;

    sigemptyset(&mask);
    sigaddset(&mask, SIGUSR1);
    sigaddset(&mask, SIGTERM);
    sigaddset(&mask, SIGCHLD);
    if (sigprocmask(SIG_BLOCK, &mask, NULL) != 0)
        return errno;
    signal(SIGHUP, SIG_IGN);
    signal(SIGPIPE, SIG_IGN);
    daemon->signals = signalfd(-1, &mask, SFD_CLOEXEC | SFD_NONBLOCK);
    if (daemon->signals < 0)
        return errno;
    daemon->timer = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC | TFD_NONBLOCK);
    if (daemon->timer < 0)
        return errno;
    if (write_text("/proc/self/oom_score_adj", "-1000") != 0 ||
        arm_timer(daemon) != 0)
        return errno;
    return 0;
}

// Returns the poll timeout in milliseconds until DEADLINE_NS, or -1 when
// DEADLINE_NS is 0, for none.
static int poll_timeout_until(int64_t deadline_ns)
{
    int64_t remaining;

    if (deadline_ns == 0)
        return -1;
    remaining = deadline_ns - clock_ns(CLOCK_MONOTONIC);
    return remaining <= 0
               ? 0
               : (int)((remaining + NSEC_PER_MSEC - 1) / NSEC_PER_MSEC);
}

// Returns the earlier of two poll timeouts, where -1 is none.
static int earlier_timeout(int first, int second)
{
    if (first < 0)
        return second;
    if (second < 0)
        return first;
    return first < second ? first : second;
}

static void daemon_loop(struct daemon *daemon)
{
    for (;;) {
        struct pollfd fds[3] = {{daemon->kmsg, POLLIN, 0},
                                {daemon->signals, POLLIN, 0},
                                {daemon->timer, POLLIN, 0}};
        bool waiting = daemon->worker_pid > 0 && !daemon->suppression_released;
        bool idle = daemon->worker_pid > 0 && daemon->worker_promote_ns != 0;
        bool due = daemon->restore_pending && daemon->worker_pid == 0;
        int timeout = earlier_timeout(
            earlier_timeout(
                poll_timeout_until(idle ? daemon->worker_promote_ns : 0),
                poll_timeout_until(waiting ? daemon->worker_deadline_ns : 0)),
            poll_timeout_until(due ? daemon->restore_due_ns : 0));
        int64_t now;

        if (poll(fds, ARRAY_SIZE(fds), timeout) < 0 && errno != EINTR) {
            sleep(1);
            continue;
        }
        if ((fds[0].revents & (POLLIN | POLLERR)) != 0)
            watch_kernel_log(daemon);
        if ((fds[1].revents & POLLIN) != 0)
            handle_signals(daemon);
        if ((fds[2].revents & POLLIN) != 0) {
            uint64_t expirations;
            ssize_t count = read(daemon->timer, &expirations,
                                 sizeof(expirations));

            (void)count;
            discipline_poll(daemon);
            arm_timer(daemon);
        }
        now = clock_ns(CLOCK_MONOTONIC);
        // Step 13 starts DEFERRED_START_NS after the ack, and continues at
        // normal priority IDLE_BOUND_NS after it starts.
        if (daemon->restore_pending && daemon->worker_pid == 0 &&
            now >= daemon->restore_due_ns) {
            daemon->restore_pending = false;
            start_restore(daemon);
            now = clock_ns(CLOCK_MONOTONIC);
        }
        if (daemon->worker_pid > 0 && daemon->worker_promote_ns != 0 &&
            now >= daemon->worker_promote_ns) {
            daemon->worker_promote_ns = 0;
            set_idle_priority(daemon->worker_pid, false);
        }
        if (daemon->worker_pid > 0 && !daemon->suppression_released &&
            now >= daemon->worker_deadline_ns)
            release_suppression(daemon);
    }
}

// Turns the boot checker into the time daemon, at normal priority. It keeps
// reading the kernel log from where the boot check positioned KMSG and
// publishes its PID once its descriptors are ready. Returns 0 or -1.
static int become_daemon(struct daemon *daemon, int kmsg,
                         int64_t last_accepted_ns)
{
    char text[32];
    int error;

    memset(daemon, 0, sizeof(*daemon));
    daemon->kmsg = kmsg;
    daemon->last_accepted_ns = last_accepted_ns;
    set_idle_priority(0, false);
    error = daemon_setup(daemon);
    if (error != 0) {
        errno = error;
        return -1;
    }
    snprintf(text, sizeof(text), "%d\n", (int)getpid());
    return replace_file(DAEMON_PID_PATH, DAEMON_PID_PATH ".tmp", text);
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

static void load_generation(void)
{
    struct time_state state;

    if (state_load(&state) == 0)
        g_generation = (uint32_t)state.generation;
}

struct boot_log_job {
    struct boot_log *log;
    char *buffer;
    int size;
    bool read_log;
};

// C4 and K1 over the kernel log, beside the per-CPU checks. The caller
// allocates the buffer before the threads start: freeing a large allocation
// unmaps it, and an unmap in a process with running threads waits for every
// CPU running one of them.
static void boot_kernel_log(struct checks *checks, void *context)
{
    struct boot_log_job *job = context;
    char detail[DETAIL_MAX];

    if (!job->read_log)
        return;
    if (job->buffer == NULL) {
        check_failed(checks, "C4", "no buffer for the %d-byte kernel log",
                     job->size);
        return;
    }
    if (read_boot_log_into(job->log, job->buffer, job->size) != 0) {
        check_failed(checks, "C4", "read the kernel log: %s", strerror(errno));
        return;
    }
    if (!boot_log_verdict(job->log, detail, sizeof(detail)))
        check_failed(checks, "C4", "%s", detail);
    // K1 is kernel hardening, not time: a writable and executable kernel
    // mapping, such as unprotected ITS thunk pages.
    if (job->log->wx[0] != '\0')
        check_failed_code(checks, "G_KERNEL_WX", "%s", job->log->wx);
}

// C7 beside the per-CPU checks.
static void boot_timer_list(struct checks *checks, void *context)
{
    (void)context;
    check_timer_list(checks, TIMER_LIST_WAIT_NS);
}

// C5 beside the per-CPU checks.
static void boot_cpuinfo(struct checks *checks, void *context)
{
    (void)context;
    check_cpuinfo(checks, checks->online, true);
}

struct boot_sample {
    int64_t theta;
    int64_t epsilon;
    int64_t accepted_ns;
};

// The boot checks that need no CPU of their own run in the caller while the
// parallel jobs run.
static void boot_foreground(struct checks *checks, void *context)
{
    (void)context;
    check_cmdline(checks);
    check_platform(checks);
    check_clocksource(checks);
    check_debug_watchdogs(checks);
}

// Sleeps until DEADLINE_NS on CLOCK_MONOTONIC.
static void sleep_until(int64_t deadline_ns)
{
    struct timespec when = {(time_t)(deadline_ns / NSEC_PER_SEC),
                            (long)(deadline_ns % NSEC_PER_SEC)};

    while (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &when, NULL) ==
           EINTR)
        ;
}

// The asynchronous boot checks: every boot check but C12, fail-fast, from
// DEFERRED_START_NS after the boot step at SCHED_IDLE, and at normal priority
// from IDLE_BOUND_NS after they start. When they pass, the checker becomes the
// daemon and records the boot check, whose wall and CPU times add C12's
// (C12_NS and C12_CPU_NS) and the daemon's start, but not the delay. It never
// returns.
static void boot_checker(const struct boot_sample *sample, int64_t c12_ns,
                         int64_t c12_cpu_ns, int c12_failures)
{
    // The delay counts from this fork in the time ABI boot step, the one
    // point that every guest mode shares: init runs it before the rest of its
    // setup, so the checks start somewhat less than DEFERRED_START_NS after
    // shell-ready.
    int64_t start = clock_ns(CLOCK_MONOTONIC) + DEFERRED_START_NS;
    int64_t cpu_start = cpu_time_ns();
    int null = open("/dev/null", O_RDWR | O_CLOEXEC);
    struct boot_log_job log_job;
    struct promoter promoter;
    struct side_job sides[3];
    struct daemon daemon;
    struct checks checks;
    struct boot_log log;
    bool started = false;
    bool promoting;
    int kmsg;

    g_async_output = true;
    (void)setsid();
    if (null >= 0) {
        (void)dup2(null, STDIN_FILENO);
        (void)dup2(null, STDOUT_FILENO);
        (void)dup2(null, STDERR_FILENO);
        close(null);
    }
    if (chdir("/") != 0)
        _exit(1);
    promoting = promoter_start(&promoter, start);
    set_idle_priority(0, true);
    sleep_until(start);
    if (checks_init(&checks, PHASE_BOOT) != 0) {
        check_failed(&checks, "C1", "cannot read the CPU sets");
        _exit(1);
    }
    checks.failures = c12_failures;
    memset(&log, 0, sizeof(log));
    log_job.log = &log;
    log_job.read_log = false;
    log_job.size = klogctl(SYSLOG_ACTION_SIZE_BUFFER, NULL, 0);
    log_job.buffer = log_job.size > 0 ? malloc((size_t)log_job.size + 1) : NULL;
    kmsg = open("/dev/kmsg", O_RDONLY | O_NONBLOCK | O_CLOEXEC);
    if (kmsg < 0 || lseek(kmsg, 0, SEEK_END) < 0) {
        check_failed(&checks, "C4", "open /dev/kmsg: %s", strerror(errno));
        if (kmsg >= 0)
            close(kmsg);
        kmsg = -1;
    } else {
        log_job.read_log = true;
    }
    // The kernel log, the tick mode, and cpuinfo run beside the per-CPU
    // checks, longest first; the other checks run in the caller meanwhile.
    sides[0] = (struct side_job){.run = boot_kernel_log, .context = &log_job};
    sides[1] = (struct side_job){.run = boot_timer_list};
    sides[2] = (struct side_job){.run = boot_cpuinfo};
    check_cpus_parallel(&checks, checks.online, true, true, sides,
                        ARRAY_SIZE(sides), boot_foreground, NULL);
    free(log_job.buffer);
    memcpy(checks.cpu_khz, sides[2].checks.cpu_khz, sizeof(checks.cpu_khz));
    memcpy(checks.cpu_khz_known, sides[2].checks.cpu_khz_known,
           sizeof(checks.cpu_khz_known));
    // C3 compares CPU 0's declared rate with cpuinfo once both have run.
    if (checks.tsc_hz != 0 && checks.cpu_khz_known[0] &&
        checks.tsc_hz / 1000 != checks.cpu_khz[0])
        check_failed(&checks, "C3",
                     "cpu 0: floor(F / 1000) is %" PRIu64
                     " kHz but cpu MHz is %" PRIu64 " kHz",
                     checks.tsc_hz / 1000, checks.cpu_khz[0]);
    // The daemon blocks its signals in its only thread.
    if (promoting)
        promoter_stop(&promoter);
    if (kmsg >= 0) {
        started = become_daemon(&daemon, kmsg, sample->accepted_ns) == 0;
        if (!started)
            check_failed(&checks, "C10", "cannot start the time daemon: %s",
                         strerror(errno));
    }
    if (record_check(PHASE_BOOT, "ok", checks.online_count,
                     (c12_ns + clock_ns(CLOCK_MONOTONIC) - start) /
                         NSEC_PER_USEC,
                     (c12_cpu_ns + cpu_time_ns() - cpu_start) / NSEC_PER_USEC,
                     checks.tsc_hz, checks.lapic_hz, checks.failures) != 0)
        check_failed(&checks, "C10", "cannot publish the time state: %s",
                     strerror(errno));
    if (!started)
        _exit(1);
    daemon_loop(&daemon);
    _exit(0);
}

// Before shell-ready, only C12: the initial time sample and clock step, then
// the published state. Every other boot check runs asynchronously in a child,
// which becomes the daemon.
static int cmd_boot(void)
{
    int64_t start = clock_ns(CLOCK_MONOTONIC);
    int64_t cpu_start = cpu_time_ns();
    struct boot_sample sample = {0, 0, 0};
    struct time_state state;
    struct checks checks;
    int64_t c12_cpu;
    pid_t pid;

    g_generation = 0;
    make_runtime_directories();
    memset(&checks, 0, sizeof(checks));
    checks.phase = PHASE_BOOT;
    if (check_initial_sample(&checks, &sample.theta, &sample.epsilon))
        sample.accepted_ns = clock_ns(CLOCK_MONOTONIC);
    state_init(&state);
    state.synchronized = sample.accepted_ns != 0;
    state.samples = sample.accepted_ns != 0;
    state.offset_ns = sample.accepted_ns != 0 ? sample.theta : 0;
    state.uncertainty_ns = sample.accepted_ns != 0 ? sample.epsilon : 0;
    state.frequency_ppb = frequency_to_ppb(current_frequency());
    if (state_store(&state) != 0) {
        check_failed(&checks, "C10", "cannot publish the time state: %s",
                     strerror(errno));
        return 1;
    }
    fflush(NULL);
    // The child cannot read this process's CPU clock.
    c12_cpu = cpu_time_ns() - cpu_start;
    pid = fork();
    if (pid == 0)
        boot_checker(&sample, clock_ns(CLOCK_MONOTONIC) - start, c12_cpu,
                     checks.failures);
    if (pid < 0) {
        check_failed(&checks, "C10", "cannot start the boot checks: %s",
                     strerror(errno));
        return 1;
    }
    return 0;
}

// Capture checks (C6, C7, and C10); they never wait.
static void run_capture_checks(struct checks *checks)
{
    (void)rates_load(&checks->tsc_hz, &checks->lapic_hz);
    check_clocksource(checks);
    check_timer_list(checks, 0);
    check_daemon(checks);
}

// Restore checks: C1, C2, and C5 on newly onlined CPUs, C3 for CPU 0, C6,
// and C10. They never wait.
static void run_restore_checks(struct checks *checks, const bool *new_cpus,
                               int new_count)
{
    uint64_t tsc_hz = 0;
    uint64_t lapic_hz = 0;

    if (rates_load(&tsc_hz, &lapic_hz) != 0) {
        check_failed(checks, "C3", "the boot rates are unavailable");
    } else {
        checks->tsc_hz = tsc_hz;
        checks->lapic_hz = lapic_hz;
    }
    if (new_count > 0) {
        check_cpus(checks, new_cpus, true, false);
        check_cpuinfo(checks, new_cpus, true);
    }
    check_msrs(checks, 0, false);
    check_clocksource(checks);
    check_daemon(checks);
}

static int cmd_check(int argc, char **argv)
{
    int64_t start = clock_ns(CLOCK_MONOTONIC);
    int64_t cpu_start = cpu_time_ns();
    bool new_cpus[MAX_CPUS] = {false};
    int new_count = 0;
    enum phase phase = PHASE_RUNTIME;
    struct checks checks;

    for (int index = 0; index < argc; index++) {
        if (strcmp(argv[index], "--phase") == 0 && index + 1 < argc) {
            const char *name = argv[++index];

            phase = strcmp(name, "capture") == 0   ? PHASE_CAPTURE
                    : strcmp(name, "restore") == 0 ? PHASE_RESTORE
                                                   : PHASE_RUNTIME;
        } else if (strcmp(argv[index], "--new-cpus") == 0 && index + 1 < argc) {
            new_count = parse_new_cpus(argv[++index], new_cpus);
        } else {
            new_count = -1;
        }
    }
    if (phase == PHASE_RUNTIME || new_count < 0) {
        fprintf(stderr, "usage: nvx-time check --phase capture|restore "
                        "[--new-cpus LIST]\n");
        return 2;
    }
    load_generation();
    if (checks_init(&checks, phase) != 0) {
        check_failed(&checks, "C6", "cannot read the CPU sets");
        return 1;
    }
    if (phase == PHASE_CAPTURE)
        run_capture_checks(&checks);
    else
        run_restore_checks(&checks, new_cpus, new_count);
    (void)record_check(phase, "ok", checks.online_count,
                       (clock_ns(CLOCK_MONOTONIC) - start) / NSEC_PER_USEC,
                       (cpu_time_ns() - cpu_start) / NSEC_PER_USEC, 0, 0,
                       checks.failures);
    return checks.failures == 0 ? 0 : 1;
}

// Whether a recorded check of STATE is still pending: the boot check only
// with BOOT_ONLY, or any. A state without a boot record parses as a pending
// boot check.
static bool checks_pending(const struct time_state *state, bool boot_only)
{
    for (int phase = 0; phase < (boot_only ? 1 : CHECK_PHASES); phase++) {
        if (strcmp(state->checks[phase].status, "pending") == 0)
            return true;
    }
    return false;
}

// Waits until no recorded check (with BOOT_ONLY, no boot check) is pending,
// for at most STATUS_WAIT_NS, and loads the state into STATE; a missing
// state file counts as a pending boot check. Returns 0, or 1 when a check is
// still pending.
static int wait_for_checks(struct time_state *state, bool boot_only)
{
    int64_t deadline = clock_ns(CLOCK_MONOTONIC) + STATUS_WAIT_NS;

    for (;;) {
        if (state_load(state) != 0)
            state_init(state);
        else if (!checks_pending(state, boot_only))
            return 0;
        if (clock_ns(CLOCK_MONOTONIC) >= deadline)
            return 1;
        (void)nanosleep(&(struct timespec){0, STATUS_POLL_NS}, NULL);
    }
}

static int cmd_pre_capture(void)
{
    struct time_state state;
    struct checks checks;
    int64_t cpu_start;
    int64_t elapsed;
    int64_t start;
    int64_t cpu;

    // Step 1: the asynchronous boot checks start the daemon that C10
    // requires.
    if (wait_for_checks(&state, true) != 0) {
        memset(&checks, 0, sizeof(checks));
        checks.phase = PHASE_CAPTURE;
        check_failed(&checks, "C10", "the boot checks are still pending");
        return 1;
    }
    start = clock_ns(CLOCK_MONOTONIC);
    cpu_start = cpu_time_ns();
    g_generation = (uint32_t)state.generation;
    if (checks_init(&checks, PHASE_CAPTURE) != 0) {
        check_failed(&checks, "C6", "cannot read the CPU sets");
        return 1;
    }
    run_capture_checks(&checks);
    elapsed = clock_ns(CLOCK_MONOTONIC) - start;
    cpu = cpu_time_ns() - cpu_start;
    if (suppression_save() != 0) {
        char detail[DETAIL_MAX];
        int error = errno;

        if (error != EEXIST)
            (void)suppression_restore(detail, sizeof(detail), false);
        fprintf(stderr, "nvx-time: cannot suppress the stall detectors: %s\n",
                error == EEXIST ? "a capture is already pending"
                                : strerror(error));
        return 1;
    }
    // Step 1's record travels in the snapshot.
    if (record_check(PHASE_CAPTURE, "ok", checks.online_count,
                     elapsed / NSEC_PER_USEC, cpu / NSEC_PER_USEC, 0, 0,
                     checks.failures) != 0)
        check_failed(&checks, "C10", "cannot record the capture check: %s",
                     strerror(errno));
    return 0;
}

static int cmd_cancel_capture(void)
{
    char detail[DETAIL_MAX];

    if (suppression_restore(detail, sizeof(detail), false) != 0) {
        fprintf(stderr, "nvx-time: %s\n", detail);
        return 1;
    }
    return 0;
}

struct repair_record {
    const struct restore_packet *packet;
    int64_t theta;
    int64_t epsilon;
    int64_t step_realtime_ns;
    int64_t frequency;
    int64_t elapsed_us;
};

static void apply_repair(struct time_state *state, const void *context)
{
    const struct repair_record *repair = context;

    state->generation = repair->packet->generation;
    state->discontinuities++;
    snprintf(state->last_discontinuity, sizeof(state->last_discontinuity),
             "restore");
    state->last_step_ns = repair->theta;
    state->last_step_realtime_ns = repair->step_realtime_ns;
    state->last_downtime_ns = repair->packet->downtime_ns;
    snprintf(state->last_downtime_source, sizeof(state->last_downtime_source),
             "%s",
             (repair->packet->flags & PACKET_DOWNTIME_UTC) != 0 ? "utc"
                                                                : "monotonic");
    state->synchronized = 1;
    state->offset_ns = repair->theta;
    state->uncertainty_ns = repair->epsilon;
    state->frequency_ppb = frequency_to_ppb(repair->frequency);
    state->samples++;
    snprintf(state->last_sample_error, sizeof(state->last_sample_error),
             "none");
    // Step 8 marks this restore's check pending, so nvx-time status waits for
    // the daemon's step 13. Until then, restore_elapsed_us is the readiness
    // path up to the clock step, at last_step_realtime_ns; restore_cpu_us
    // counts only step 13, which records it.
    snprintf(state->checks[PHASE_RESTORE].status,
             sizeof(state->checks[PHASE_RESTORE].status), "pending");
    state->checks[PHASE_RESTORE].cpus = 0;
    state->checks[PHASE_RESTORE].elapsed_us = repair->elapsed_us;
    state->checks[PHASE_RESTORE].cpu_us = 0;
    state->checks[PHASE_RESTORE].generation = state->generation;
    state->checks[PHASE_RESTORE].failures = -1;
}

static int repair_failed(const char *code, const char *detail)
{
    fail_fatal(STATUS_REPAIR, code, "repair", PHASE_RESTORE, "%s", detail);
    return 1;
}

// Restore steps 6 to 8: read the packet with four-byte reads, validate it,
// and set the wall clock from its bracketed UTC, then write the entropy to
// ENTROPY_FD, the caller's file at ENTROPY_PATH. START_NS is when the capture
// request returned. The caller holds the portb lock. With FINISH (untiered)
// and a DAEMON, a restore with no processors or memory to activate needs no
// shell work before the acknowledgement, so steps 12 and 13 start here too
// and no second helper process starts; the metadata then begins with 2
// instead of 1.
static int repair_restore(const uint8_t *previous_id, const char *entropy_path,
                          int entropy_fd, int64_t start_ns, bool finish,
                          pid_t daemon)
{
    static uint8_t body[PACKET_MAX_SIZE];
    static struct restore_packet packet;
    uint8_t header[PACKET_HEADER_SIZE];
    struct repair_record repair;
    struct time_sample sample;
    char detail[DETAIL_MAX];
    char id[GENERATION_ID_SIZE * 2 + 1];
    int64_t t0;
    int64_t t1;
    int64_t sample_theta;
    int64_t sample_epsilon;
    bool finishing;
    int finished = -1;

    t0 = clock_ns(CLOCK_REALTIME);
    outb(SELECT_PACKET, PORTB_STATUS);
    t1 = clock_ns(CLOCK_REALTIME);
    portb_read(PORTB_DATA, header, sizeof(header));
    if (parse_packet_header(header, &packet, detail, sizeof(detail)) != 0)
        return repair_failed("G_REPAIR_PACKET", detail);
    if (packet.generation == g_generation) {
        close(entropy_fd);
        unlink(entropy_path);
        puts("0");
        return 0;
    }
    if ((uint64_t)packet.generation != (uint64_t)g_generation + 1) {
        snprintf(detail, sizeof(detail),
                 "restore packet generation %" PRIu32
                 " does not follow recorded generation %" PRIu32,
                 packet.generation, g_generation);
        return repair_failed("G_REPAIR_GENERATION", detail);
    }
    portb_read(PORTB_DATA, body, packet_body_size(&packet));
    if (parse_packet_body(body, &packet, detail, sizeof(detail)) != 0)
        return repair_failed("G_REPAIR_PACKET", detail);
    if (memcmp(packet.entropy, previous_id, GENERATION_ID_SIZE) == 0)
        return repair_failed("G_REPAIR_GENERATION",
                             "the VM generation ID did not change");
    g_generation = packet.generation;
    pair_clocks(t0, t1, packet.utc_ns, &repair.theta, &repair.epsilon);
    if (repair.epsilon > REPAIR_BOUND_NS) {
        int result = best_sample(SAMPLE_ATTEMPTS - 1, REPAIR_BOUND_NS,
                                 packet.generation, &sample, &sample_theta,
                                 &sample_epsilon, detail, sizeof(detail));

        if (result == -2)
            return repair_failed("G_REPAIR_GENERATION", detail);
        if (result >= 0 && sample_epsilon < repair.epsilon) {
            repair.theta = sample_theta;
            repair.epsilon = sample_epsilon;
        }
        if (repair.epsilon > REPAIR_BOUND_NS) {
            snprintf(detail, sizeof(detail),
                     "UTC pairing uncertainty %" PRId64 " ns exceeds 1 ms",
                     repair.epsilon);
            return repair_failed("G_REPAIR_SAMPLE", detail);
        }
    }
    repair.frequency = restore_frequency(packet.rate_deviation);
    if (step_clock(repair.theta) != 0 || set_frequency(repair.frequency) != 0) {
        snprintf(detail, sizeof(detail), "adjtimex: %s", strerror(errno));
        return repair_failed("G_REPAIR_CLOCK", detail);
    }
    repair.step_realtime_ns = clock_ns(CLOCK_REALTIME);
    repair.elapsed_us = (clock_ns(CLOCK_MONOTONIC) - start_ns) / NSEC_PER_USEC;
    repair.packet = &packet;
    // A plain untiered restore needs no shell work: this helper also
    // acknowledges and hands step 13 to the daemon.
    finishing = finish && daemon > 0 && packet.online_vp_count == 0 &&
                packet.range_count == 0 &&
                (packet.flags & PACKET_MEMORY_TARGET) == 0;
    // The entropy goes to the caller in every case, an untiered restore's
    // too; the file already exists, empty, so only its bytes are written.
    if (pwrite(entropy_fd, packet.entropy, PACKET_ENTROPY_SIZE, 0) !=
            PACKET_ENTROPY_SIZE ||
        close(entropy_fd) != 0) {
        snprintf(detail, sizeof(detail), "write %.200s: %s", entropy_path,
                 strerror(errno));
        return repair_failed("G_REPAIR_PACKET", detail);
    }
    if (state_apply(apply_repair, &repair) != 0) {
        snprintf(detail, sizeof(detail), "record the restore: %s",
                 strerror(errno));
        return repair_failed("G_REPAIR_CLOCK", detail);
    }
    format_hex(packet.entropy, GENERATION_ID_SIZE, id);
    if (finishing) {
        finished = signal_restore(daemon,
                                  (packet.flags & PACKET_ACK_REQUIRED) != 0,
                                  start_ns);
        if (finished > 0)
            fail_fatal(STATUS_CONFORMANCE, "G_CONFORMANCE_C10", "conformance",
                       PHASE_RESTORE, "the time daemon is not running");
    }
    printf("%d %u %u %u %s", finished >= 0 ? 2 : 1, packet.flags,
           packet.online_vp_count, packet.range_count, id);
    for (unsigned index = 0; index < packet.range_count; index++)
        printf(" %" PRIu64 " %" PRIu64, packet.ranges[index][0],
               packet.ranges[index][1]);
    putchar('\n');
    return 0;
}

// Writes the capture request to 0x605. In a restored process the write
// returns with a restore packet available, and steps 6 to 8 run here before
// any other guest work. Prints 0 when no restore happened. The portb lock is
// held from the request to the restored clock, so the snapshot never
// contains a discipline poll that would apply a pre-capture sample after the
// restore.
static int cmd_capture(int argc, char **argv)
{
    uint8_t previous_id[GENERATION_ID_SIZE];
    char detail[DETAIL_MAX];
    char *end;
    long request;
    int64_t start;
    unsigned status;
    pid_t daemon = -1;
    int entropy_fd;
    int result;
    int lock;

    if ((argc != 3 && (argc != 4 || strcmp(argv[3], "--finish") != 0)) ||
        (request = strtol(argv[0], &end, 10), *end != '\0') ||
        (request != 0 && request != 1) ||
        parse_hex(argv[1], previous_id, sizeof(previous_id)) != 0) {
        fprintf(stderr, "usage: nvx-time capture 0|1 GENERATION_ID "
                        "ENTROPY_FILE [--finish]\n");
        return 2;
    }
    if (state_load(&(struct time_state){0}) != 0) {
        fprintf(stderr, "nvx-time: the time state is unavailable\n");
        return 1;
    }
    load_generation();
    if (enable_ports() != 0) {
        fprintf(stderr, "nvx-time: ioperm: %s\n", strerror(errno));
        return 1;
    }
    // The daemon keeps its PID in the restored guest; looking it up now keeps
    // the file reads off the restore path.
    if (argc == 4)
        daemon = daemon_pid();
    // The entropy file is created before the capture, empty, so that a
    // restored helper only writes its bytes: creating a file right after a
    // restore faults in the filesystem's metadata, 4 to 9 ms on WHP.
    entropy_fd = open(argv[2], O_WRONLY | O_CREAT | O_TRUNC | O_NOFOLLOW |
                                   O_CLOEXEC,
                      0600);
    if (entropy_fd < 0) {
        fprintf(stderr, "nvx-time: open %s: %s\n", argv[2], strerror(errno));
        return 1;
    }
    lock = lock_portb(detail, sizeof(detail));
    if (lock < 0) {
        fprintf(stderr, "nvx-time: %s\n", detail);
        close(entropy_fd);
        unlink(argv[2]);
        return 1;
    }
    outb((unsigned char)request, PORT_SNAPSHOT);
    start = clock_ns(CLOCK_MONOTONIC);
    status = inb(PORTB_STATUS);
    if ((status & PORTB_PACKET_AVAILABLE) == 0) {
        close(lock);
        close(entropy_fd);
        unlink(argv[2]);
        puts("0");
        return 0;
    }
    result = repair_restore(previous_id, argv[2], entropy_fd, start, argc == 4,
                            daemon);
    close(lock);
    return result;
}

// Restore steps 12 and 13 after the shell's activation and identity work: the
// acknowledgement when the packet asked for one, then the daemon runs step 13
// asynchronously. The record tells it which CPUs activation onlined. The
// readiness path started restore_elapsed_us before the helper's clock step,
// at last_step_realtime_ns.
static int cmd_restore_finish(int argc, char **argv)
{
    bool new_cpus[MAX_CPUS] = {false};
    const char *new_list = "none";
    struct restore_timing timing;
    struct time_state state;
    struct checks checks;
    char text[256];
    int64_t start;
    bool ack = false;
    int new_count = 0;
    int signaled;

    for (int index = 0; index < argc; index++) {
        if (strcmp(argv[index], "--ack") == 0) {
            ack = true;
        } else if (strcmp(argv[index], "--new-cpus") == 0 && index + 1 < argc) {
            new_list = argv[++index];
            new_count = parse_new_cpus(new_list, new_cpus);
        } else {
            new_count = -1;
        }
    }
    if (new_count < 0 || strlen(new_list) > 127) {
        fprintf(stderr, "usage: nvx-time restore-finish [--ack] "
                        "[--new-cpus LIST]\n");
        return 2;
    }
    if (state_load(&state) != 0)
        return repair_failed("G_REPAIR_CLOCK", "the time state is unreadable");
    g_generation = (uint32_t)state.generation;
    snprintf(text, sizeof(text), "generation=%" PRIu64 "\nnew_cpus=%s\n",
             state.generation, new_count > 0 ? new_list : "none");
    if (replace_file(RESTORE_PATH, RESTORE_PATH ".tmp", text) != 0) {
        snprintf(text, sizeof(text), "record the restore: %s",
                 strerror(errno));
        return repair_failed("G_REPAIR_CLOCK", text);
    }
    if (ack && enable_ports() != 0) {
        fprintf(stderr, "nvx-time: ioperm: %s\n", strerror(errno));
        return 1;
    }
    start = clock_ns(CLOCK_MONOTONIC) -
            (clock_ns(CLOCK_REALTIME) - state.last_step_realtime_ns) -
            state.checks[PHASE_RESTORE].elapsed_us * NSEC_PER_USEC;
    signaled = signal_restore(daemon_pid(), ack, start);
    if (signaled == 0)
        return 0;
    if (signaled > 0 || !g_report_only) {
        memset(&checks, 0, sizeof(checks));
        checks.phase = PHASE_RESTORE;
        check_failed(&checks, "C10", "the time daemon is not running");
        return 1;
    }
    // Report-only runs may lack the daemon; finish the restore here, where
    // step 13's CPU time is restore_work's own.
    if (ack)
        outb(SNAPSHOT_ACKNOWLEDGE, PORT_SNAPSHOT);
    timing.start_ns = clock_ns(CLOCK_MONOTONIC);
    timing.readiness_us = (timing.start_ns - start) / NSEC_PER_USEC;
    timing.prior_cpu_ns = 0;
    timing.cpu_base_ns = cpu_time_ns();
    timing.fork_fd = -1;
    (void)restore_work(new_cpus, new_count, true, &timing);
    return 0;
}

// Formats nvx-time status's lines from STATE: one per recorded phase, in the
// order boot, capture, and restore, with status=pending and neither
// elapsed_us nor cpu_us while its check runs, then the runtime line.
// Report-only mode appends each check's failure count, and a failed check's
// status is fail. Returns whether every phase line reports ok.
static bool format_status(const struct time_state *state, char *text,
                          size_t size)
{
    size_t length = 0;
    bool ok = true;

    text[0] = '\0';
    for (int phase = 0; phase < CHECK_PHASES; phase++) {
        const struct check_state *check = &state->checks[phase];
        const char *status = check->status;
        char times[64] = "";
        char suffix[40] = "";

        if (status[0] == '\0')
            continue;
        if (strcmp(status, "pending") != 0)
            snprintf(times, sizeof(times),
                     " elapsed_us=%" PRId64 " cpu_us=%" PRId64,
                     check->elapsed_us, check->cpu_us);
        if (g_report_only)
            snprintf(suffix, sizeof(suffix), " failures=%" PRId64,
                     check->failures > 0 ? check->failures : 0);
        ok = ok && strcmp(status, "ok") == 0;
        append_text(text, size, &length,
                    "%s: v=1 phase=%s status=%s cpus=%" PRIu64
                    " tsc_hz=%" PRIu64 " lapic_hz=%" PRIu64
                    " generation=%" PRIu64 "%s%s\n",
                    abi_prefix(), k_phase_names[phase], status, check->cpus,
                    state->tsc_hz, state->lapic_hz, check->generation, times,
                    suffix);
    }
    append_text(text, size, &length,
                "%s: v=1 phase=runtime status=%s generation=%" PRIu64
                " discontinuities=%" PRIu64 " offset_ns=%" PRId64
                " uncertainty_ns=%" PRId64 " rejected_samples=%" PRIu64
                " last_sample_error=%s\n",
                abi_prefix(),
                state->synchronized != 0 ? "synchronized" : "unsynchronized",
                state->generation, state->discontinuities, state->offset_ns,
                state->uncertainty_ns, state->rejected_samples,
                state->last_sample_error);
    return ok;
}

// Prints the time state on demand, after waiting for pending checks: CI and
// the matrix driver run it over the console; production paths never do.
// Exits with 0 only when every phase line reports ok.
static int cmd_status(void)
{
    struct time_state state;
    char text[1024];
    int waited = wait_for_checks(&state, false);
    bool ok = format_status(&state, text, sizeof(text));

    fputs(text, stdout);
    fflush(stdout);
    return waited == 0 && ok ? 0 : 1;
}

static int cmd_sample(void)
{
    struct time_sample sample;
    char detail[DETAIL_MAX] = "";
    int64_t theta;
    int64_t epsilon;
    unsigned status;
    int result;
    int lock;

    if (enable_ports() != 0) {
        fprintf(stderr, "nvx-time: ioperm: %s\n", strerror(errno));
        return 1;
    }
    status = inb(PORTB_STATUS);
    if ((status & PORTB_TIME_WINDOW) == 0) {
        fprintf(stderr, "nvx-time: portb status 0x%02x has no time window\n",
                status);
        return 1;
    }
    lock = lock_portb(detail, sizeof(detail));
    result = lock < 0 ? -1 : take_sample(&sample, detail, sizeof(detail));
    if (lock >= 0)
        close(lock);
    if (result != 0) {
        fprintf(stderr, "nvx-time: %s\n", detail);
        return 1;
    }
    pair_clocks(sample.t0, sample.t1, sample.utc_ns, &theta, &epsilon);
    printf("flags=0x%02x generation=%" PRIu32 " utc_ns=%" PRIu64
           " t0=%" PRId64 " t1=%" PRId64 " theta_ns=%" PRId64
           " epsilon_ns=%" PRId64 "\n",
           sample.flags, sample.generation, sample.utc_ns, sample.t0,
           sample.t1, theta, epsilon);
    return 0;
}

static int cmd_generation_id(void)
{
    uint8_t id[GENERATION_ID_SIZE];
    char text[GENERATION_ID_SIZE * 2 + 1];
    unsigned status;
    int lock;

    if (enable_ports() != 0) {
        fprintf(stderr, "nvx-time: ioperm: %s\n", strerror(errno));
        return 1;
    }
    status = inb(PORTB_STATUS);
    if ((status & PORTB_GENERATION_ID_AVAILABLE) == 0) {
        fprintf(stderr, "nvx-time: the VM generation ID is unavailable\n");
        return 1;
    }
    lock = lock_file(PORTB_LOCK_PATH);
    if (lock < 0) {
        fprintf(stderr, "nvx-time: portb lock: %s\n", strerror(errno));
        return 1;
    }
    outb(SELECT_GENERATION_ID, PORTB_STATUS);
    portb_read(PORTB_DATA, id, sizeof(id));
    close(lock);
    format_hex(id, sizeof(id), text);
    puts(text);
    return 0;
}

// ---------------------------------------------------------------------------
// Unit-test entry points: pure decoders and decisions over fixture files
// ---------------------------------------------------------------------------

static int test_bytes(const char *path, uint8_t *buffer, size_t size)
{
    memset(buffer, 0, size);
    return read_bytes(path, buffer, size) < 0 ? -1 : 0;
}

static int test_text(const char *path, char *buffer, size_t size)
{
    return read_text(path, buffer, size) < 0 ? -1 : 0;
}

static int test_packet(int argc, char **argv)
{
    static uint8_t bytes[PACKET_MAX_SIZE];
    static struct restore_packet packet;
    uint8_t previous[GENERATION_ID_SIZE];
    char detail[DETAIL_MAX];
    char id[GENERATION_ID_SIZE * 2 + 1];
    uint64_t recorded;

    if (argc != 3 || test_bytes(argv[0], bytes, sizeof(bytes)) != 0 ||
        parse_hex(argv[2], previous, sizeof(previous)) != 0)
        return 2;
    recorded = strtoull(argv[1], NULL, 10);
    if (parse_packet_header(bytes, &packet, detail, sizeof(detail)) != 0) {
        printf("error G_REPAIR_PACKET %s\n", detail);
        return 0;
    }
    if (packet.generation == recorded) {
        printf("stale\n");
        return 0;
    }
    if ((uint64_t)packet.generation != recorded + 1) {
        printf("error G_REPAIR_GENERATION generation %" PRIu32 "\n",
               packet.generation);
        return 0;
    }
    if (parse_packet_body(bytes + PACKET_HEADER_SIZE, &packet, detail,
                          sizeof(detail)) != 0) {
        printf("error G_REPAIR_PACKET %s\n", detail);
        return 0;
    }
    if (memcmp(packet.entropy, previous, GENERATION_ID_SIZE) == 0) {
        printf("error G_REPAIR_GENERATION generation ID unchanged\n");
        return 0;
    }
    format_hex(packet.entropy, GENERATION_ID_SIZE, id);
    printf("ok flags=%u online=%u ranges=%u generation=%" PRIu32
           " rate_deviation=%" PRId32 " frequency=%" PRId64
           " downtime_ns=%" PRIu64 " utc_ns=%" PRIu64 " generation_id=%s",
           packet.flags, packet.online_vp_count, packet.range_count,
           packet.generation, packet.rate_deviation,
           restore_frequency(packet.rate_deviation), packet.downtime_ns,
           packet.utc_ns, id);
    for (unsigned index = 0; index < packet.range_count; index++)
        printf(" range=%" PRIu64 ":%" PRIu64, packet.ranges[index][0],
               packet.ranges[index][1]);
    putchar('\n');
    return 0;
}

static int test_sample(int argc, char **argv)
{
    uint8_t bytes[SAMPLE_SIZE];
    struct time_sample sample;
    char detail[DETAIL_MAX];

    if (argc != 2 || test_bytes(argv[0], bytes, sizeof(bytes)) != 0)
        return 2;
    if (parse_sample(bytes, &sample, detail, sizeof(detail)) != 0)
        printf("error %s\n", detail);
    else if (sample.generation != strtoul(argv[1], NULL, 10))
        printf("error generation %" PRIu32 "\n", sample.generation);
    else
        printf("ok flags=%u generation=%" PRIu32 " utc_ns=%" PRIu64 "\n",
               sample.flags, sample.generation, sample.utc_ns);
    return 0;
}

// Watcher rules per line (KMSG), or the C4 verdict over message lines
// (BOOT_LOG) or SYSLOG_ACTION_READ_ALL text (SYSLOG).
enum line_test { LINES_KMSG, LINES_BOOT_LOG, LINES_SYSLOG };

static int test_lines(int argc, char **argv, enum line_test mode)
{
    static char text[TEXT_MAX];
    struct boot_log log;
    char detail[DETAIL_MAX];
    bool boot = mode != LINES_KMSG;

    if (argc != 1 || test_text(argv[0], text, sizeof(text)) != 0)
        return 2;
    memset(&log, 0, sizeof(log));
    if (mode == LINES_SYSLOG) {
        boot_log_scan(text, &log);
    } else {
        for (char *line = strtok(text, "\n"); line != NULL;
             line = strtok(NULL, "\n")) {
            const char *code = match_watch_rule(line);

            if (boot)
                boot_log_add(&log, line);
            else
                printf("%s\n", code != NULL ? code : "none");
        }
    }
    if (boot) {
        if (boot_log_verdict(&log, detail, sizeof(detail)))
            printf("ok\n");
        else
            printf("fail %s\n", detail);
        if (log.wx[0] != '\0')
            printf("wx %s\n", log.wx);
    }
    return 0;
}

static int test_timer_list(int argc, char **argv)
{
    static char text[TEXT_MAX];
    bool cpus[MAX_CPUS];
    char detail[DETAIL_MAX];

    if (argc != 2 || test_text(argv[0], text, sizeof(text)) != 0 ||
        parse_cpu_list(argv[1], cpus) <= 0)
        return 2;
    if (timer_list_verdict(text, cpus, detail, sizeof(detail)))
        printf("ok\n");
    else
        printf("fail %s\n", detail);
    return 0;
}

static int test_checks(const char *name, int argc, char **argv)
{
    static char text[TEXT_MAX];
    struct checks checks;
    bool cpus[MAX_CPUS];

    memset(&checks, 0, sizeof(checks));
    checks.phase = PHASE_BOOT;
    if (strcmp(name, "cpuinfo") == 0) {
        if (argc != 2 || test_text(argv[0], text, sizeof(text)) != 0 ||
            parse_cpu_list(argv[1], cpus) <= 0)
            return 2;
        if (check_cpuinfo_text(&checks, text, cpus, true)) {
            printf("ok");
            for (int cpu = 0; cpu < MAX_CPUS; cpu++) {
                if (checks.cpu_khz_known[cpu])
                    printf(" cpu%d_khz=%" PRIu64, cpu, checks.cpu_khz[cpu]);
            }
            putchar('\n');
        }
    } else if (strcmp(name, "cmdline") == 0) {
        if (argc != 1)
            return 2;
        snprintf(text, sizeof(text), "%s", argv[0]);
        if (check_cmdline_text(&checks, text))
            printf("ok\n");
    } else if (strcmp(name, "clocksource") == 0) {
        if (argc != 2)
            return 2;
        snprintf(text, sizeof(text), "%s", argv[1]);
        if (check_clocksource_values(&checks, argv[0], text))
            printf("ok\n");
    } else {
        return 2;
    }
    return 0;
}

static int test_correction(int argc, char **argv)
{
    struct timex tx;
    bool step;

    if (argc != 3)
        return 2;
    step = plan_correction(strtoll(argv[0], NULL, 10), strtoll(argv[1], NULL, 10),
                           strcmp(argv[2], "fast") == 0, &tx);
    if (step)
        printf("step modes=0x%x sec=%ld nsec=%ld\n", tx.modes,
               (long)tx.time.tv_sec, (long)tx.time.tv_usec);
    else
        printf("slew modes=0x%x offset=%ld status=0x%x constant=%ld "
               "maxerror=%ld esterror=%ld\n",
               tx.modes, tx.offset, (unsigned)tx.status, tx.constant,
               tx.maxerror, tx.esterror);
    return 0;
}

static int test_state(int argc, char **argv)
{
    static char text[4096];
    struct time_state state;

    if (argc != 1 || test_text(argv[0], text, sizeof(text)) != 0)
        return 2;
    if (state_parse(text, &state) != 0) {
        printf("error\n");
        return 0;
    }
    state_format(&state, text, sizeof(text));
    fputs(text, stdout);
    return 0;
}

static int test_restore_record(int argc, char **argv)
{
    static char text[4096];
    unsigned long long generation;
    bool cpus[MAX_CPUS];
    const char *separator = "";
    int count;

    if (argc != 1 || test_text(argv[0], text, sizeof(text)) != 0)
        return 2;
    count = parse_restore_record(text, &generation, cpus);
    if (count < 0) {
        printf("error\n");
        return 0;
    }
    printf("generation=%llu new_cpus=", generation);
    for (int cpu = 0; cpu < MAX_CPUS; cpu++) {
        if (cpus[cpu]) {
            printf("%s%d", separator, cpu);
            separator = ",";
        }
    }
    printf("%s count=%d\n", count == 0 ? "none" : "", count);
    return 0;
}

// A side job and a foreground that fail a known number of times, so the test
// sees whether check_cpus_parallel() merges every job's failures.
static void test_failing_job(struct checks *checks, void *context)
{
    const int *failures = context;

    for (int index = 0; index < *failures; index++)
        check_failed(checks, "C4", "side job failure %d", index + 1);
}

static void test_failing_foreground(struct checks *checks, void *context)
{
    (void)context;
    check_failed(checks, "C9", "foreground failure");
}

static int test_parallel(int argc, char **argv)
{
    static int failures[3] = {1, 2, 3};
    struct side_job sides[ARRAY_SIZE(failures)];
    struct checks checks;
    bool cpus[MAX_CPUS];

    if (argc != 1 || parse_cpu_list(argv[0], cpus) <= 0)
        return 2;
    memset(&checks, 0, sizeof(checks));
    checks.phase = PHASE_BOOT;
    for (size_t index = 0; index < ARRAY_SIZE(sides); index++)
        sides[index] = (struct side_job){.run = test_failing_job,
                                         .context = &failures[index]};
    check_cpus_parallel(&checks, cpus, false, false, sides, ARRAY_SIZE(sides),
                        test_failing_foreground, NULL);
    printf("failures=%d\n", checks.failures);
    return 0;
}

// A thread that an idle thread starts, so it is idle too; it waits until the
// test ends.
struct test_thread {
    pthread_mutex_t lock;
    pthread_cond_t changed;
    pid_t tid;
    bool done;
};

static void *test_thread_run(void *argument)
{
    struct test_thread *thread = argument;

    pthread_mutex_lock(&thread->lock);
    thread->tid = (pid_t)syscall(SYS_gettid);
    pthread_cond_broadcast(&thread->changed);
    while (!thread->done)
        pthread_cond_wait(&thread->changed, &thread->lock);
    pthread_mutex_unlock(&thread->lock);
    return NULL;
}

// Prints the policies of an idle caller and of the idle thread it started
// once a promoter fires 50 ms later, then the caller's policy after a
// promoter stopped before its 10 s bound, and how long stopping took.
static int test_promote(void)
{
    struct test_thread thread = {.lock = PTHREAD_MUTEX_INITIALIZER,
                                 .changed = PTHREAD_COND_INITIALIZER};
    struct promoter promoter;
    pthread_t handle;
    long caller;
    long started;
    int64_t start;

    if (!promoter_start(&promoter, clock_ns(CLOCK_MONOTONIC) - IDLE_BOUND_NS +
                                       50 * NSEC_PER_MSEC))
        return 1;
    set_idle_priority(0, true);
    if (pthread_create(&handle, NULL, test_thread_run, &thread) != 0)
        return 1;
    pthread_mutex_lock(&thread.lock);
    while (thread.tid == 0)
        pthread_cond_wait(&thread.changed, &thread.lock);
    pthread_mutex_unlock(&thread.lock);
    pthread_join(promoter.thread, NULL);
    caller = syscall(SYS_sched_getscheduler, 0);
    started = syscall(SYS_sched_getscheduler, thread.tid);
    pthread_mutex_lock(&thread.lock);
    thread.done = true;
    pthread_cond_broadcast(&thread.changed);
    pthread_mutex_unlock(&thread.lock);
    pthread_join(handle, NULL);
    printf("promoted caller=%ld thread=%ld\n", caller, started);

    start = clock_ns(CLOCK_MONOTONIC);
    if (!promoter_start(&promoter,
                        start - IDLE_BOUND_NS + 10 * NSEC_PER_SEC))
        return 1;
    set_idle_priority(0, true);
    promoter_stop(&promoter);
    printf("stopped caller=%ld stop_ms=%" PRId64 "\n",
           syscall(SYS_sched_getscheduler, 0),
           (clock_ns(CLOCK_MONOTONIC) - start) / NSEC_PER_MSEC);
    return 0;
}

static int64_t g_test_fork_burn_ms;

// Spends MS milliseconds of this process's CPU time.
static void burn_cpu(int64_t ms)
{
    int64_t until = cpu_time_ns() + ms * NSEC_PER_MSEC;

    while (cpu_time_ns() < until)
        ;
}

static void test_fork_burn(void)
{
    burn_cpu(g_test_fork_burn_ms);
}

// Forks a step 13 worker as start_restore does, after PRE_MS of preparation,
// with a fork that costs the parent FORK_MS; the worker spends CHILD_MS and
// prints its preparation share and step 13's CPU time.
static int test_step13_cpu(int argc, char **argv)
{
    struct restore_timing timing = {0};
    int64_t start = cpu_time_ns();
    int status;
    pid_t pid;

    if (argc != 3)
        return 2;
    g_test_fork_burn_ms = strtoll(argv[1], NULL, 10);
    if (pthread_atfork(NULL, test_fork_burn, NULL) != 0)
        return 1;
    burn_cpu(strtoll(argv[0], NULL, 10));
    pid = fork_step13(start, &timing);
    if (pid == 0) {
        burn_cpu(strtoll(argv[2], NULL, 10));
        printf("prior_ms=%" PRId64 " total_ms=%" PRId64 "\n",
               timing.prior_cpu_ns / NSEC_PER_MSEC,
               step13_cpu_ns(&timing) / NSEC_PER_MSEC);
        fflush(stdout);
        _exit(0);
    }
    if (pid < 0 || waitpid(pid, &status, 0) != pid)
        return 1;
    return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}

static int cmd_test(int argc, char **argv)
{
    const char *name = argc > 0 ? argv[0] : "";

    g_test = true;
    g_report_only = true;
    argc--;
    argv++;
    if (strcmp(name, "packet") == 0)
        return test_packet(argc, argv);
    if (strcmp(name, "sample") == 0)
        return test_sample(argc, argv);
    if (strcmp(name, "pair") == 0 && argc == 3) {
        int64_t theta;
        int64_t epsilon;

        pair_clocks(strtoll(argv[0], NULL, 10), strtoll(argv[1], NULL, 10),
                    strtoull(argv[2], NULL, 10), &theta, &epsilon);
        printf("theta_ns=%" PRId64 " epsilon_ns=%" PRId64 "\n", theta, epsilon);
        return 0;
    }
    if (strcmp(name, "kmsg") == 0)
        return test_lines(argc, argv, LINES_KMSG);
    if (strcmp(name, "console") == 0 && argc == 2) {
        g_async_output = strcmp(argv[1], "async") == 0;
        console_write(argv[0]);
        return 0;
    }
    if (strcmp(name, "exhaustive-leaf") == 0 && argc == 9) {
        uint32_t value[4];
        uint32_t out_of_range[4];
        char detail[DETAIL_MAX];

        for (int index = 0; index < 4; index++) {
            value[index] = (uint32_t)strtoul(argv[1 + index], NULL, 0);
            out_of_range[index] = (uint32_t)strtoul(argv[5 + index], NULL, 0);
        }
        if (exhaustive_leaf_ok((uint32_t)strtoul(argv[0], NULL, 0), value,
                               out_of_range, detail, sizeof(detail)))
            printf("pass\n");
        else
            printf("fail %s\n", detail);
        return 0;
    }
    if (strcmp(name, "exhaustive-report") == 0 && argc == 4) {
        struct exhaustive state;

        memset(&state, 0, sizeof(state));
        exhaustive_report(&state, argv[0], atoi(argv[1]),
                          strcmp(argv[2], "pass") == 0, argv[3]);
        printf("failures=%d\n", state.failures);
        return 0;
    }
    if (strcmp(name, "exhaustive-summary") == 0 && argc == 2) {
        struct exhaustive state;
        int status;

        memset(&state, 0, sizeof(state));
        state.failures = atoi(argv[1]);
        status = exhaustive_summary(&state, atoi(argv[0]));
        printf("exit=%d\n", status);
        return 0;
    }
    if (strcmp(name, "boot-log") == 0)
        return test_lines(argc, argv, LINES_BOOT_LOG);
    if (strcmp(name, "syslog") == 0)
        return test_lines(argc, argv, LINES_SYSLOG);
    if (strcmp(name, "timer-list") == 0)
        return test_timer_list(argc, argv);
    if (strcmp(name, "correction") == 0)
        return test_correction(argc, argv);
    if (strcmp(name, "frequency") == 0 && argc == 1) {
        int64_t frequency = restore_frequency((int32_t)strtol(argv[0], NULL, 10));

        printf("frequency=%" PRId64 " ppb=%" PRId64 "\n", frequency,
               frequency_to_ppb(frequency));
        return 0;
    }
    if (strcmp(name, "event") == 0 && argc == 6) {
        char line[EVENT_MAX + 1];
        enum phase phase = PHASE_RUNTIME;

        for (size_t index = 0; index < ARRAY_SIZE(k_phase_names); index++) {
            if (strcmp(argv[2], k_phase_names[index]) == 0)
                phase = (enum phase)index;
        }
        g_report_only = false;
        format_event(line, argv[0], argv[1], phase,
                     (uint32_t)strtoul(argv[3], NULL, 10),
                     strtoll(argv[4], NULL, 10), argv[5]);
        fputs(line, stdout);
        return 0;
    }
    if (strcmp(name, "state") == 0)
        return test_state(argc, argv);
    if (strcmp(name, "status") == 0 && argc == 2) {
        static char text[4096];
        struct time_state state;
        char out[1024];
        bool ok;

        if (test_text(argv[0], text, sizeof(text)) != 0 ||
            state_parse(text, &state) != 0)
            return 2;
        g_report_only = strcmp(argv[1], "report-only") == 0;
        ok = format_status(&state, out, sizeof(out));
        fputs(out, stdout);
        printf("ok=%d\n", ok ? 1 : 0);
        return 0;
    }
    if (strcmp(name, "restore-record") == 0)
        return test_restore_record(argc, argv);
    if (strcmp(name, "step13-cpu") == 0)
        return test_step13_cpu(argc, argv);
    if (strcmp(name, "deferral") == 0 && argc == 0) {
        printf("deferred_start_ms=%" PRId64 " idle_bound_ms=%" PRId64 "\n",
               (int64_t)(DEFERRED_START_NS / NSEC_PER_MSEC),
               (int64_t)(IDLE_BOUND_NS / NSEC_PER_MSEC));
        return 0;
    }
    if (strcmp(name, "parallel") == 0)
        return test_parallel(argc, argv);
    if (strcmp(name, "promote") == 0 && argc == 0)
        return test_promote();
    if (strcmp(name, "idle-priority") == 0 && argc == 0) {
        long idle;

        set_idle_priority(0, true);
        idle = syscall(SYS_sched_getscheduler, 0);
        set_idle_priority(0, false);
        printf("idle=%ld normal=%ld\n", idle,
               syscall(SYS_sched_getscheduler, 0));
        return 0;
    }
    return test_checks(name, argc, argv);
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

static bool cmdline_requests_report_only(void)
{
    char text[4096];

    if (read_text("/proc/cmdline", text, sizeof(text)) < 0)
        return false;
    for (char *token = strtok(text, " \t\n"); token != NULL;
         token = strtok(NULL, " \t\n")) {
        if (strcmp(token, "nvx_time_abi=report-only") == 0)
            return true;
    }
    return false;
}

static int usage(void)
{
    fprintf(stderr,
            "usage: nvx-time [--report-only] COMMAND\n"
            "  boot\n"
            "  check --phase capture|restore [--new-cpus LIST]\n"
            "  pre-capture\n"
            "  capture 0|1 GENERATION_ID ENTROPY_FILE [--finish]\n"
            "  cancel-capture\n"
            "  restore-finish [--ack] [--new-cpus LIST]\n"
            "  status\n"
            "  sample\n"
            "  generation-id\n"
            "  exhaustive\n"
            "  test NAME ARGUMENTS...\n");
    return 2;
}

int main(int argc, char **argv)
{
    char *arguments[32];
    int count = 0;
    const char *command;

    for (int index = 1; index < argc; index++) {
        if (strcmp(argv[index], "--report-only") == 0)
            g_report_only = true;
        else if (count < (int)ARRAY_SIZE(arguments))
            arguments[count++] = argv[index];
        else
            return usage();
    }
    if (count == 0)
        return usage();
    command = arguments[0];
    if (strcmp(command, "test") == 0)
        return cmd_test(count - 1, arguments + 1);
    // An inherited SIG_IGN would make the kernel reap the release child and
    // the daemon before waitpid() sees them.
    signal(SIGCHLD, SIG_DFL);
    if (cmdline_requests_report_only())
        g_report_only = true;
    if (strcmp(command, "boot") == 0 && count == 1)
        return cmd_boot();
    if (strcmp(command, "check") == 0)
        return cmd_check(count - 1, arguments + 1);
    if (strcmp(command, "pre-capture") == 0 && count == 1)
        return cmd_pre_capture();
    if (strcmp(command, "capture") == 0)
        return cmd_capture(count - 1, arguments + 1);
    if (strcmp(command, "cancel-capture") == 0 && count == 1)
        return cmd_cancel_capture();
    if (strcmp(command, "restore-finish") == 0)
        return cmd_restore_finish(count - 1, arguments + 1);
    if (strcmp(command, "status") == 0 && count == 1)
        return cmd_status();
    if (strcmp(command, "sample") == 0 && count == 1)
        return cmd_sample();
    if (strcmp(command, "generation-id") == 0 && count == 1)
        return cmd_generation_id();
    if (strcmp(command, "exhaustive") == 0 && count == 1)
        return exhaustive_run();
    return usage();
}
