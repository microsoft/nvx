// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// nvx-time-probe: guest-side time diagnostics for the NVX time ABI.
//
//   latency  ns/op of TSC reads and clock_gettime(), p50/p99.
//   warp     pairwise cross-CPU TSC warp and offset checks.
//   clock    clocks, clocksources, TSC flags, hypervisor identity, and
//            per-CPU clock event devices.
//
// Build (static, x86-64): cc -O2 -static -pthread -o nvx-time-probe
// nvx-time-probe.c

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/klog.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define PROBE_VERSION "1.1"
#define EXIT_FAIL 1
#define EXIT_USAGE 2
#define EXIT_ERROR 3
#define MAX_CPUS 1024
#define KMSG_RECORD_MAX 8192
#define SYSLOG_ACTION_READ_ALL 3
#define SYSLOG_ACTION_SIZE_BUFFER 10
#define CLOCKSOURCE_DIR "/sys/devices/system/clocksource/clocksource0"

struct tsc_rate {
    double hz;
    const char *source;
};

static struct tsc_rate g_tsc;
static volatile uint64_t g_sink;

// ---------------------------------------------------------------------------
// CPU primitives
// ---------------------------------------------------------------------------

static inline uint64_t rdtsc_plain(void)
{
    uint32_t lo, hi;

    __asm__ __volatile__("rdtsc" : "=a"(lo), "=d"(hi));
    return ((uint64_t)hi << 32) | lo;
}

static inline uint64_t rdtscp_plain(void)
{
    uint32_t lo, hi, aux;

    __asm__ __volatile__("rdtscp" : "=a"(lo), "=d"(hi), "=c"(aux));
    return ((uint64_t)hi << 32) | lo;
}

// Linux rdtsc_ordered(): the read cannot pass earlier loads.
static inline uint64_t rdtsc_lfence(void)
{
    uint32_t lo, hi;

    __asm__ __volatile__("lfence\n\trdtsc" : "=a"(lo), "=d"(hi)::"memory");
    return ((uint64_t)hi << 32) | lo;
}

// Fully fenced read for timing brackets: later work cannot start before it.
static inline uint64_t rdtsc_fenced(void)
{
    uint32_t lo, hi;

    __asm__ __volatile__("lfence\n\trdtsc\n\tlfence"
                         : "=a"(lo), "=d"(hi)::"memory");
    return ((uint64_t)hi << 32) | lo;
}

static inline void cpu_pause(void)
{
    __asm__ __volatile__("pause" ::: "memory");
}

static void cpuid(uint32_t leaf, uint32_t subleaf, uint32_t regs[4])
{
    __asm__ __volatile__("cpuid"
                         : "=a"(regs[0]), "=b"(regs[1]), "=c"(regs[2]),
                           "=d"(regs[3])
                         : "a"(leaf), "c"(subleaf));
}

static bool cpu_has_rdtscp(void)
{
    uint32_t regs[4];

    cpuid(0x80000000u, 0, regs);
    if (regs[0] < 0x80000001u)
        return false;
    cpuid(0x80000001u, 0, regs);
    return (regs[3] >> 27) & 1;
}

static int pin_self(int cpu)
{
    cpu_set_t set;

    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    if (sched_setaffinity(0, sizeof(set), &set) != 0)
        return -1;
    if (sched_getcpu() != cpu) {
        errno = EINVAL;
        return -1;
    }
    return 0;
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

static double cycles_to_ns(double cycles)
{
    return g_tsc.hz > 0 ? cycles * 1e9 / g_tsc.hz : 0;
}

static long long round_ll(double value)
{
    return (long long)(value >= 0 ? value + 0.5 : value - 0.5);
}

static int read_first_line(const char *path, char *buf, size_t size)
{
    FILE *file = fopen(path, "r");

    if (!file)
        return -1;
    if (!fgets(buf, (int)size, file)) {
        fclose(file);
        return -1;
    }
    fclose(file);
    buf[strcspn(buf, "\n")] = '\0';
    return 0;
}

static int cmp_double(const void *a, const void *b)
{
    double x = *(const double *)a;
    double y = *(const double *)b;

    return (x > y) - (x < y);
}

static int cmp_i64(const void *a, const void *b)
{
    int64_t x = *(const int64_t *)a;
    int64_t y = *(const int64_t *)b;

    return (x > y) - (x < y);
}

// Nearest-rank percentile over a sorted array, as the NVX harness reports.
static double nearest_rank(const double *sorted, size_t count, double pct)
{
    double exact = pct * (double)count;
    size_t rank = (size_t)exact;

    if ((double)rank < exact)
        rank++;
    if (rank < 1)
        rank = 1;
    if (rank > count)
        rank = count;
    return sorted[rank - 1];
}

static int parse_u64_arg(const char *text, uint64_t *value)
{
    char *end;

    errno = 0;
    *value = strtoull(text, &end, 0);
    return errno == 0 && end != text && *end == '\0' ? 0 : -1;
}

static int parse_cpu_list(const char *text, int *cpus, int max)
{
    int count = 0;
    const char *p = text;

    while (*p) {
        char *end;
        long first = strtol(p, &end, 10);
        long last = first;

        if (end == p || first < 0 || first >= MAX_CPUS)
            return -1;
        p = end;
        if (*p == '-') {
            last = strtol(p + 1, &end, 10);
            if (end == p + 1 || last < first || last >= MAX_CPUS)
                return -1;
            p = end;
        }
        for (long cpu = first; cpu <= last; cpu++) {
            for (int i = 0; i < count; i++)
                if (cpus[i] == cpu)
                    return -1;
            if (count >= max)
                return -1;
            cpus[count++] = (int)cpu;
        }
        if (*p == ',')
            p++;
        else if (*p)
            return -1;
    }
    return count;
}

static int affinity_cpus(int *cpus, int max)
{
    cpu_set_t set;
    int count = 0;

    if (sched_getaffinity(0, sizeof(set), &set) != 0)
        return -1;
    for (int cpu = 0; cpu < CPU_SETSIZE && count < max; cpu++)
        if (CPU_ISSET(cpu, &set))
            cpus[count++] = cpu;
    return count;
}

// Rejects CPUs outside the affinity mask (offline or excluded) before any
// measurement starts.
static int check_cpus_allowed(const char *command, const int *cpus, int count)
{
    cpu_set_t allowed;

    if (sched_getaffinity(0, sizeof(allowed), &allowed) != 0) {
        fprintf(stderr, "nvx-time-probe %s: sched_getaffinity: %s\n", command,
                strerror(errno));
        return -1;
    }
    for (int i = 0; i < count; i++) {
        if (cpus[i] >= CPU_SETSIZE || !CPU_ISSET(cpus[i], &allowed)) {
            fprintf(stderr,
                    "nvx-time-probe %s: cpu %d is not in the affinity mask"
                    " (offline or excluded)\n",
                    command, cpus[i]);
            return -1;
        }
    }
    return 0;
}

static void format_cpu_list(const int *cpus, int count, char *buf, size_t size)
{
    size_t used = 0;

    buf[0] = '\0';
    for (int i = 0; i < count && used < size; i++) {
        int first = cpus[i];
        int last = first;

        while (i + 1 < count && cpus[i + 1] == last + 1)
            last = cpus[++i];
        used += (size_t)snprintf(buf + used, size - used, "%s%d", used ? "," : "",
                                 first);
        if (last != first && used < size)
            used += (size_t)snprintf(buf + used, size - used, "-%d", last);
    }
}

// ---------------------------------------------------------------------------
// Kernel log access (/dev/kmsg, falling back to syslog(2))
// ---------------------------------------------------------------------------

typedef void (*kmsg_fn)(uint64_t usec, const char *text, void *ctx);

static int kmsg_from_devkmsg(kmsg_fn fn, void *ctx)
{
    char record[KMSG_RECORD_MAX];
    int count = 0;
    int fd = open("/dev/kmsg", O_RDONLY | O_NONBLOCK | O_CLOEXEC);

    if (fd < 0)
        return -1;
    for (;;) {
        ssize_t n = read(fd, record, sizeof(record) - 1);
        char *semi;
        char *p;
        int field = 0;

        if (n < 0 && (errno == EINTR || errno == EPIPE))
            continue;
        if (n <= 0)
            break;
        record[n] = '\0';
        // Record format: "prio,seq,usec,flags[,...];message\n[ KEY=VALUE\n]".
        semi = strchr(record, ';');
        if (!semi)
            continue;
        for (p = record; p < semi && field < 2; p++)
            if (*p == ',')
                field++;
        record[strcspn(record, "\n")] = '\0';
        fn(strtoull(p, NULL, 10), semi + 1, ctx);
        count++;
    }
    close(fd);
    return count > 0 ? 0 : -1;
}

static int kmsg_from_syslog(kmsg_fn fn, void *ctx)
{
    int size = klogctl(SYSLOG_ACTION_SIZE_BUFFER, NULL, 0);
    char *buf;
    char *line;
    int n;

    if (size <= 0)
        size = 1 << 20;
    buf = malloc((size_t)size + 1);
    if (!buf)
        return -1;
    n = klogctl(SYSLOG_ACTION_READ_ALL, buf, size);
    if (n < 0) {
        free(buf);
        return -1;
    }
    buf[n] = '\0';
    for (line = buf; line && *line;) {
        char *next = strchr(line, '\n');
        char *p = line;
        uint64_t usec = 0;

        if (next)
            *next++ = '\0';
        if (*p == '<') {
            char *close_angle = strchr(p, '>');

            if (close_angle)
                p = close_angle + 1;
        }
        if (*p == '[') {
            char *close_bracket = strchr(p, ']');

            if (close_bracket) {
                usec = (uint64_t)(strtod(p + 1, NULL) * 1e6 + 0.5);
                p = close_bracket + 1;
                if (*p == ' ')
                    p++;
            }
        }
        fn(usec, p, ctx);
        line = next;
    }
    free(buf);
    return 0;
}

static int kmsg_foreach(kmsg_fn fn, void *ctx)
{
    if (kmsg_from_devkmsg(fn, ctx) == 0)
        return 0;
    return kmsg_from_syslog(fn, ctx);
}

// ---------------------------------------------------------------------------
// /proc/cpuinfo and TSC rate discovery
// ---------------------------------------------------------------------------

static bool cpuinfo_has_flag(const char *flag)
{
    static char flags[8192];
    static bool loaded;
    size_t len = strlen(flag);
    const char *p;

    if (!loaded) {
        char line[8192];
        FILE *file = fopen("/proc/cpuinfo", "r");

        loaded = true;
        while (file && fgets(line, sizeof(line), file)) {
            if (strncmp(line, "flags", 5) == 0 && strchr(line, ':')) {
                snprintf(flags, sizeof(flags), " %s", strchr(line, ':') + 1);
                flags[strcspn(flags, "\n")] = '\0';
                strncat(flags, " ", sizeof(flags) - strlen(flags) - 1);
                break;
            }
        }
        if (file)
            fclose(file);
    }
    for (p = flags; (p = strstr(p, flag)) != NULL; p += len)
        if (p[-1] == ' ' && p[len] == ' ')
            return true;
    return false;
}

static double cpuinfo_mhz(void)
{
    char line[512];
    double mhz = 0;
    FILE *file = fopen("/proc/cpuinfo", "r");

    if (!file)
        return 0;
    while (fgets(line, sizeof(line), file)) {
        if (strncmp(line, "cpu MHz", 7) == 0 && strchr(line, ':')) {
            mhz = strtod(strchr(line, ':') + 1, NULL);
            break;
        }
    }
    fclose(file);
    return mhz;
}

struct kmsg_tsc {
    double refined_mhz;
    double detected_tsc_mhz;
    double detected_cpu_mhz;
};

static double mhz_after(const char *text, const char *prefix, const char *suffix)
{
    const char *p = strstr(text, prefix);
    char *end;
    double mhz;

    if (!p)
        return 0;
    p += strlen(prefix);
    mhz = strtod(p, &end);
    if (end == p || strncmp(end, suffix, strlen(suffix)) != 0)
        return 0;
    return mhz;
}

static void kmsg_tsc_cb(uint64_t usec, const char *text, void *ctx)
{
    struct kmsg_tsc *k = ctx;
    double mhz;

    (void)usec;
    if ((mhz = mhz_after(text, "Refined TSC clocksource calibration: ", " MHz")) > 0)
        k->refined_mhz = mhz;
    else if ((mhz = mhz_after(text, "tsc: Detected ", " MHz TSC")) > 0)
        k->detected_tsc_mhz = mhz;
    else if ((mhz = mhz_after(text, "tsc: Detected ", " MHz processor")) > 0)
        k->detected_cpu_mhz = mhz;
}

// The kernel prints kHz precision as "NNNN.NNN MHz".
static double mhz_to_hz(double mhz)
{
    return (double)(uint64_t)(mhz * 1000.0 + 0.5) * 1000.0;
}

static double measure_tsc_hz(unsigned ms)
{
    struct timespec a, b;
    struct timespec pause = {ms / 1000, (long)(ms % 1000) * 1000000L};
    uint64_t t0, t1;
    double ns;

    clock_gettime(CLOCK_MONOTONIC_RAW, &a);
    t0 = rdtsc_fenced();
    nanosleep(&pause, NULL);
    clock_gettime(CLOCK_MONOTONIC_RAW, &b);
    t1 = rdtsc_fenced();
    ns = (double)(b.tv_sec - a.tv_sec) * 1e9 + (double)(b.tv_nsec - a.tv_nsec);
    return ns > 0 ? (double)(t1 - t0) * 1e9 / ns : 0;
}

// Precedence: option, sysfs tsc_freq_khz, kernel log (refined, then detected
// TSC, then detected processor), "cpu MHz" without APERF/MPERF, measurement.
static void detect_tsc_rate(double override_hz)
{
    struct kmsg_tsc k = {0};
    char line[64];

    if (override_hz > 0) {
        g_tsc = (struct tsc_rate){override_hz, "option"};
        return;
    }
    if (read_first_line("/sys/devices/system/cpu/cpu0/tsc_freq_khz", line,
                        sizeof(line)) == 0 &&
        strtoull(line, NULL, 10) > 0) {
        g_tsc = (struct tsc_rate){(double)strtoull(line, NULL, 10) * 1000.0,
                                  "sysfs-tsc_freq_khz"};
        return;
    }
    kmsg_foreach(kmsg_tsc_cb, &k);
    if (k.refined_mhz > 0) {
        g_tsc = (struct tsc_rate){mhz_to_hz(k.refined_mhz), "kmsg-refined"};
        return;
    }
    if (k.detected_tsc_mhz > 0) {
        g_tsc = (struct tsc_rate){mhz_to_hz(k.detected_tsc_mhz),
                                  "kmsg-detected-tsc"};
        return;
    }
    if (k.detected_cpu_mhz > 0) {
        g_tsc = (struct tsc_rate){mhz_to_hz(k.detected_cpu_mhz),
                                  "kmsg-detected-processor"};
        return;
    }
    // Without APERF/MPERF, "cpu MHz" reports cpu_khz, which equals tsc_khz.
    if (!cpuinfo_has_flag("aperfmperf") && cpuinfo_mhz() > 0) {
        g_tsc = (struct tsc_rate){mhz_to_hz(cpuinfo_mhz()), "cpuinfo-cpu-mhz"};
        return;
    }
    g_tsc = (struct tsc_rate){measure_tsc_hz(200), "measured-monotonic-raw"};
}

static void current_clocksource(char *buf, size_t size)
{
    if (read_first_line(CLOCKSOURCE_DIR "/current_clocksource", buf, size) != 0)
        snprintf(buf, size, "unknown");
}

// ---------------------------------------------------------------------------
// Option parsing
// ---------------------------------------------------------------------------

// Matches "--name value" or "--name=value" at argv[*index].
static const char *option_value(int argc, char **argv, int *index, const char *name)
{
    const char *arg = argv[*index];
    size_t len = strlen(name);

    if (strncmp(arg, name, len) != 0)
        return NULL;
    if (arg[len] == '=')
        return arg + len + 1;
    if (arg[len] == '\0' && *index + 1 < argc)
        return argv[++*index];
    return NULL;
}

static int parse_hz_arg(const char *text, double *hz)
{
    char *end;

    errno = 0;
    *hz = strtod(text, &end);
    return errno == 0 && end != text && *end == '\0' && *hz > 0 ? 0 : -1;
}

static int bad_option(const char *command, const char *arg)
{
    fprintf(stderr, "nvx-time-probe %s: invalid option or value: %s\n", command, arg);
    return EXIT_USAGE;
}

// ---------------------------------------------------------------------------
// latency
// ---------------------------------------------------------------------------

enum method {
    M_RDTSC,
    M_RDTSCP,
    M_LFENCE_RDTSC,
    M_CLOCK_MONOTONIC,
    M_CLOCK_REALTIME,
    M_SYSCALL_MONOTONIC,
    M_SYSCALL_GETPID,
    M_SCHED_YIELD,
    M_COUNT,
};

static const char *const method_names[M_COUNT] = {
    "rdtsc",
    "rdtscp",
    "lfence_rdtsc",
    "clock_gettime_monotonic",
    "clock_gettime_realtime",
    "syscall_clock_gettime_monotonic",
    "syscall_getpid",
    "sched_yield",
};

struct latency_result {
    double p50;
    double p99;
    double min;
    double mean;
    size_t samples;
};

static uint64_t run_batch(enum method method, unsigned batch)
{
    struct timespec ts;
    uint64_t sink = 0;
    unsigned i;

    switch (method) {
    case M_RDTSC:
        for (i = 0; i < batch; i++)
            sink += rdtsc_plain();
        break;
    case M_RDTSCP:
        for (i = 0; i < batch; i++)
            sink += rdtscp_plain();
        break;
    case M_LFENCE_RDTSC:
        for (i = 0; i < batch; i++)
            sink += rdtsc_lfence();
        break;
    case M_CLOCK_MONOTONIC:
        for (i = 0; i < batch; i++) {
            clock_gettime(CLOCK_MONOTONIC, &ts);
            sink += (uint64_t)ts.tv_nsec;
        }
        break;
    case M_CLOCK_REALTIME:
        for (i = 0; i < batch; i++) {
            clock_gettime(CLOCK_REALTIME, &ts);
            sink += (uint64_t)ts.tv_nsec;
        }
        break;
    case M_SYSCALL_MONOTONIC:
        for (i = 0; i < batch; i++) {
            syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &ts);
            sink += (uint64_t)ts.tv_nsec;
        }
        break;
    case M_SYSCALL_GETPID:
        for (i = 0; i < batch; i++)
            sink += (uint64_t)syscall(SYS_getpid);
        break;
    case M_SCHED_YIELD:
        for (i = 0; i < batch; i++)
            sink += (uint64_t)sched_yield();
        break;
    default:
        break;
    }
    return sink;
}

// Median cost of an empty fenced timing bracket, subtracted from samples.
static double bracket_overhead_cycles(size_t iterations)
{
    double *samples = malloc(iterations * sizeof(*samples));
    double median;

    if (!samples)
        return 0;
    for (size_t i = 0; i < iterations; i++) {
        uint64_t t0 = rdtsc_fenced();
        uint64_t t1 = rdtsc_fenced();

        samples[i] = (double)(t1 - t0);
    }
    qsort(samples, iterations, sizeof(*samples), cmp_double);
    median = nearest_rank(samples, iterations, 0.50);
    free(samples);
    return median;
}

static int measure_method(enum method method, size_t iterations, unsigned batch,
                          double overhead_cycles, struct latency_result *out)
{
    double *per_op = malloc(iterations * sizeof(*per_op));
    double sum = 0;

    if (!per_op)
        return -1;
    g_sink += run_batch(method, batch);
    for (size_t i = 0; i < iterations; i++) {
        uint64_t t0 = rdtsc_fenced();
        uint64_t sink = run_batch(method, batch);
        uint64_t t1 = rdtsc_fenced();
        double cycles = (double)(t1 - t0) - overhead_cycles;

        g_sink += sink;
        if (cycles < 0)
            cycles = 0;
        per_op[i] = cycles_to_ns(cycles) / batch;
        sum += per_op[i];
    }
    qsort(per_op, iterations, sizeof(*per_op), cmp_double);
    out->p50 = nearest_rank(per_op, iterations, 0.50);
    out->p99 = nearest_rank(per_op, iterations, 0.99);
    out->min = per_op[0];
    out->mean = sum / (double)iterations;
    out->samples = iterations;
    free(per_op);
    return 0;
}

static int cmd_latency(int argc, char **argv)
{
    uint64_t iterations = 2000;
    uint64_t batch = 50;
    uint64_t cpu_arg = UINT64_MAX;
    double tsc_hz = 0;
    struct latency_result results[M_COUNT] = {0};
    char clocksource[64];
    bool has_rdtscp = cpu_has_rdtscp();
    bool has_vdso = getauxval(AT_SYSINFO_EHDR) != 0;
    double overhead;
    int cpu;

    for (int i = 2; i < argc; i++) {
        const char *value;

        if ((value = option_value(argc, argv, &i, "--iterations"))) {
            if (parse_u64_arg(value, &iterations) || iterations == 0 ||
                iterations > 10000000)
                return bad_option("latency", argv[i]);
        } else if ((value = option_value(argc, argv, &i, "--batch"))) {
            if (parse_u64_arg(value, &batch) || batch == 0 || batch > 1000000)
                return bad_option("latency", argv[i]);
        } else if ((value = option_value(argc, argv, &i, "--cpu"))) {
            if (parse_u64_arg(value, &cpu_arg) || cpu_arg >= MAX_CPUS)
                return bad_option("latency", argv[i]);
        } else if ((value = option_value(argc, argv, &i, "--tsc-hz"))) {
            if (parse_hz_arg(value, &tsc_hz))
                return bad_option("latency", argv[i]);
        } else {
            return bad_option("latency", argv[i]);
        }
    }

    cpu = cpu_arg == UINT64_MAX ? sched_getcpu() : (int)cpu_arg;
    if (cpu >= 0 && check_cpus_allowed("latency", &cpu, 1) != 0)
        return EXIT_USAGE;
    if (cpu < 0 || pin_self(cpu) != 0) {
        fprintf(stderr, "nvx-time-probe latency: cannot pin to cpu %d: %s\n", cpu,
                strerror(errno));
        return EXIT_ERROR;
    }
    detect_tsc_rate(tsc_hz);
    if (g_tsc.hz <= 0) {
        fprintf(stderr, "nvx-time-probe latency: unknown TSC rate; pass --tsc-hz\n");
        return EXIT_ERROR;
    }
    current_clocksource(clocksource, sizeof(clocksource));
    overhead = bracket_overhead_cycles((size_t)iterations);

    printf("nvx-time-probe %s latency: cpu=%d iterations=%" PRIu64 " batch=%" PRIu64
           " tsc_hz=%.0f tsc_hz_source=%s clocksource=%s vdso=%d"
           " bracket_overhead_ns=%.1f\n",
           PROBE_VERSION, cpu, iterations, batch, g_tsc.hz, g_tsc.source,
           clocksource, has_vdso, cycles_to_ns(overhead));
    printf("%-32s %10s %10s %10s %10s\n", "method", "p50_ns", "p99_ns", "min_ns",
           "mean_ns");
    for (int m = 0; m < M_COUNT; m++) {
        if (m == M_RDTSCP && !has_rdtscp) {
            printf("%-32s unsupported (CPUID.80000001H:EDX[27] clear)\n",
                   method_names[m]);
            continue;
        }
        if (measure_method((enum method)m, (size_t)iterations, (unsigned)batch,
                           overhead, &results[m]) != 0) {
            fprintf(stderr, "nvx-time-probe latency: out of memory\n");
            return EXIT_ERROR;
        }
        printf("%-32s %10.1f %10.1f %10.1f %10.1f\n", method_names[m],
               results[m].p50, results[m].p99, results[m].min, results[m].mean);
        fflush(stdout);
    }
    for (int m = 0; m < M_COUNT; m++) {
        if (!results[m].samples) {
            printf("NVX-TIME-PROBE latency method=%s unsupported=1\n", method_names[m]);
            continue;
        }
        printf("NVX-TIME-PROBE latency method=%s p50_ns=%.1f p99_ns=%.1f min_ns=%.1f"
               " mean_ns=%.1f samples=%zu batch=%" PRIu64 " cpu=%d clocksource=%s\n",
               method_names[m], results[m].p50, results[m].p99, results[m].min,
               results[m].mean, results[m].samples, batch, cpu, clocksource);
    }
    return 0;
}

// ---------------------------------------------------------------------------
// warp
// ---------------------------------------------------------------------------

#define PP_STOP UINT64_MAX
#define PP_MAX_SAMPLES (1u << 18)
#define STALL_SECONDS 2.0
#define SPIN_CHECK_MASK 0xfffffu

// (a) Linux check_tsc_warp(): both CPUs take one spinlock, read the TSC, and
// compare it with the last value either CPU stored. A backward step proves a
// skew larger than the lock handoff latency, so it is a lower bound.
static struct {
    _Alignas(64) atomic_int lock;
    uint64_t last_tsc;
    uint64_t max_warp;
    uint64_t warps;
    _Alignas(64) atomic_int ready;
    atomic_int abort;
    atomic_int stop;
} g_warp;

// (b) Ping-pong: cpu_a reads t1 and signals, cpu_b reads t2 and answers, and
// cpu_a reads t3. The offset of cpu_b relative to cpu_a lies in [t2-t3, t2-t1];
// the point estimate is t2-(t1+t3)/2 with uncertainty (t3-t1)/2.
static struct {
    _Alignas(64) _Atomic uint64_t seq;
    _Atomic uint64_t t2;
    _Alignas(64) atomic_int ready;
    atomic_int abort;
} g_pp;

struct pair_result {
    int cpu_a;
    int cpu_b;
    uint64_t warp_iterations;
    uint64_t warps;
    uint64_t max_warp_cycles;
    uint64_t rounds;
    uint64_t min_rtt_cycles;
    int64_t min_rtt_offset_x2;
    int64_t lo_cycles;
    int64_t hi_cycles;
    double median_offset_cycles;
    double offset_cycles;
    double uncertainty_cycles;
    bool consistent;
    bool stalled;
};

struct thread_arg {
    int cpu;
    bool leader;
    uint64_t budget_cycles;
    uint64_t stall_cycles;
    uint64_t iterations;
    int pin_errno;
    struct pair_result *result;
    int64_t *offsets_x2;
};

static bool pair_barrier(atomic_int *ready, atomic_int *abort_flag, bool failed)
{
    if (failed)
        atomic_store(abort_flag, 1);
    atomic_fetch_add(ready, 1);
    while (atomic_load(ready) < 2)
        cpu_pause();
    return !atomic_load(abort_flag);
}

static uint64_t coarse_ns(void)
{
    struct timespec ts;

    clock_gettime(CLOCK_MONOTONIC_COARSE, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static void *warp_thread(void *p)
{
    struct thread_arg *arg = p;
    bool pinned = pin_self(arg->cpu) == 0;
    uint64_t end;
    uint64_t deadline_ns;
    uint64_t i;

    if (!pinned)
        arg->pin_errno = errno;
    if (!pair_barrier(&g_warp.ready, &g_warp.abort, !pinned))
        return NULL;
    end = rdtsc_lfence() + arg->budget_cycles;
    // Guard against a TSC that never reaches the deadline.
    deadline_ns = coarse_ns() +
                  (uint64_t)(cycles_to_ns((double)arg->budget_cycles) + STALL_SECONDS * 1e9);
    for (i = 0;; i++) {
        uint64_t prev, now;

        while (atomic_exchange_explicit(&g_warp.lock, 1, memory_order_acquire))
            while (atomic_load_explicit(&g_warp.lock, memory_order_relaxed))
                cpu_pause();
        prev = g_warp.last_tsc;
        now = rdtsc_lfence();
        g_warp.last_tsc = now;
        if (prev > now) {
            if (prev - now > g_warp.max_warp)
                g_warp.max_warp = prev - now;
            g_warp.warps++;
        }
        atomic_store_explicit(&g_warp.lock, 0, memory_order_release);
        if (!(i & 7)) {
            if (arg->leader &&
                (rdtsc_plain() > end || (!(i & 4095) && coarse_ns() > deadline_ns)))
                atomic_store_explicit(&g_warp.stop, 1, memory_order_relaxed);
            if (atomic_load_explicit(&g_warp.stop, memory_order_relaxed))
                break;
        }
    }
    arg->iterations = i + 1;
    return NULL;
}

static void *pp_leader(void *p)
{
    struct thread_arg *arg = p;
    struct pair_result *r = arg->result;
    bool pinned = pin_self(arg->cpu) == 0;
    uint64_t stored = 0;
    uint64_t end;
    uint64_t k;

    if (!pinned)
        arg->pin_errno = errno;
    if (!pair_barrier(&g_pp.ready, &g_pp.abort, !pinned))
        return NULL;
    end = rdtsc_lfence() + arg->budget_cycles;
    for (k = 1;; k++) {
        uint64_t spins = 0;
        uint64_t t1, t2, t3, rtt;
        int64_t lo, hi;

        t1 = rdtsc_fenced();
        atomic_store_explicit(&g_pp.seq, 2 * k - 1, memory_order_release);
        while (atomic_load_explicit(&g_pp.seq, memory_order_acquire) != 2 * k) {
            if ((++spins & SPIN_CHECK_MASK) == 0 &&
                rdtsc_plain() - t1 > arg->stall_cycles) {
                r->stalled = true;
                goto done;
            }
        }
        t3 = rdtsc_lfence();
        t2 = atomic_load_explicit(&g_pp.t2, memory_order_relaxed);
        rtt = t3 - t1;
        lo = (int64_t)(t2 - t3);
        hi = (int64_t)(t2 - t1);
        if (rtt < r->min_rtt_cycles) {
            r->min_rtt_cycles = rtt;
            r->min_rtt_offset_x2 = lo + hi;
        }
        if (lo > r->lo_cycles)
            r->lo_cycles = lo;
        if (hi < r->hi_cycles)
            r->hi_cycles = hi;
        if (stored < PP_MAX_SAMPLES)
            arg->offsets_x2[stored++] = lo + hi;
        r->rounds = k;
        if ((k & 63) == 0 && rdtsc_plain() > end)
            break;
    }
done:
    atomic_store_explicit(&g_pp.seq, PP_STOP, memory_order_release);
    arg->iterations = stored;
    return NULL;
}

static void *pp_follower(void *p)
{
    struct thread_arg *arg = p;
    bool pinned = pin_self(arg->cpu) == 0;

    if (!pinned)
        arg->pin_errno = errno;
    if (!pair_barrier(&g_pp.ready, &g_pp.abort, !pinned))
        return NULL;
    for (uint64_t k = 1;; k++) {
        uint64_t spins = 0;
        uint64_t wait_start = 0;
        uint64_t value;

        while ((value = atomic_load_explicit(&g_pp.seq, memory_order_acquire)) !=
               2 * k - 1) {
            if (value == PP_STOP)
                return NULL;
            if ((++spins & SPIN_CHECK_MASK) == 0) {
                uint64_t now = rdtsc_plain();

                if (!wait_start)
                    wait_start = now;
                else if (now - wait_start > arg->stall_cycles)
                    return NULL;
            }
        }
        atomic_store_explicit(&g_pp.t2, rdtsc_fenced(), memory_order_relaxed);
        atomic_store_explicit(&g_pp.seq, 2 * k, memory_order_release);
    }
}

static int run_threads(void *(*fn_a)(void *), void *(*fn_b)(void *),
                       struct thread_arg *a, struct thread_arg *b)
{
    pthread_t ta, tb;
    int err;

    if ((err = pthread_create(&ta, NULL, fn_a, a)) != 0) {
        errno = err;
        return -1;
    }
    if ((err = pthread_create(&tb, NULL, fn_b, b)) != 0) {
        // Release the first thread from its barrier before joining it.
        atomic_store(&g_warp.abort, 1);
        atomic_store(&g_pp.abort, 1);
        atomic_fetch_add(&g_warp.ready, 1);
        atomic_fetch_add(&g_pp.ready, 1);
        pthread_join(ta, NULL);
        errno = err;
        return -1;
    }
    pthread_join(ta, NULL);
    pthread_join(tb, NULL);
    if (a->pin_errno || b->pin_errno) {
        errno = a->pin_errno ? a->pin_errno : b->pin_errno;
        return -1;
    }
    return 0;
}

static int run_pair(int cpu_a, int cpu_b, uint64_t budget_cycles,
                    uint64_t stall_cycles, int64_t *offsets_x2,
                    struct pair_result *r)
{
    struct thread_arg a = {cpu_a, true, budget_cycles, stall_cycles, 0, 0, r, offsets_x2};
    struct thread_arg b = {cpu_b, false, budget_cycles, stall_cycles, 0, 0, r, NULL};

    memset(r, 0, sizeof(*r));
    r->cpu_a = cpu_a;
    r->cpu_b = cpu_b;

    atomic_store(&g_warp.lock, 0);
    atomic_store(&g_warp.ready, 0);
    atomic_store(&g_warp.abort, 0);
    atomic_store(&g_warp.stop, 0);
    g_warp.last_tsc = 0;
    g_warp.max_warp = 0;
    g_warp.warps = 0;
    if (run_threads(warp_thread, warp_thread, &a, &b) != 0)
        return -1;
    r->warp_iterations = a.iterations + b.iterations;
    r->warps = g_warp.warps;
    r->max_warp_cycles = g_warp.max_warp;

    atomic_store(&g_pp.seq, 0);
    atomic_store(&g_pp.t2, 0);
    atomic_store(&g_pp.ready, 0);
    atomic_store(&g_pp.abort, 0);
    r->min_rtt_cycles = UINT64_MAX;
    r->lo_cycles = INT64_MIN;
    r->hi_cycles = INT64_MAX;
    a.iterations = 0;
    if (run_threads(pp_leader, pp_follower, &a, &b) != 0)
        return -1;
    if (a.iterations == 0) {
        r->stalled = true;
        return 0;
    }
    qsort(offsets_x2, (size_t)a.iterations, sizeof(*offsets_x2), cmp_i64);
    r->median_offset_cycles = (double)offsets_x2[(a.iterations - 1) / 2] / 2.0;
    r->consistent = r->lo_cycles <= r->hi_cycles;
    if (r->consistent) {
        r->offset_cycles = ((double)r->lo_cycles + (double)r->hi_cycles) / 2.0;
        r->uncertainty_cycles = ((double)r->hi_cycles - (double)r->lo_cycles) / 2.0;
    } else {
        r->offset_cycles = (double)r->min_rtt_offset_x2 / 2.0;
        r->uncertainty_cycles = (double)r->min_rtt_cycles / 2.0;
    }
    return 0;
}

static double abs_double(double value)
{
    return value < 0 ? -value : value;
}

static int cmd_warp(int argc, char **argv)
{
    static int cpus[MAX_CPUS];
    uint64_t duration_ms = 100;
    uint64_t bound_ns = 1000;
    double tsc_hz = 0;
    int ncpus = -1;
    int pairs = 0;
    int inconsistent = 0;
    int stalled = 0;
    uint64_t total_warps = 0;
    uint64_t max_backward = 0;
    double max_abs_offset = 0;
    double max_uncertainty = 0;
    double max_skew_bound = 0;
    uint64_t budget_cycles;
    uint64_t stall_cycles;
    int64_t *offsets_x2;
    char cpu_text[256];
    bool pass;
    bool conclusive;

    for (int i = 2; i < argc; i++) {
        const char *value;

        if ((value = option_value(argc, argv, &i, "--duration-ms"))) {
            if (parse_u64_arg(value, &duration_ms) || duration_ms == 0 ||
                duration_ms > 600000)
                return bad_option("warp", argv[i]);
        } else if ((value = option_value(argc, argv, &i, "--cpus"))) {
            if ((ncpus = parse_cpu_list(value, cpus, MAX_CPUS)) <= 0)
                return bad_option("warp", argv[i]);
        } else if ((value = option_value(argc, argv, &i, "--bound-ns"))) {
            if (parse_u64_arg(value, &bound_ns))
                return bad_option("warp", argv[i]);
        } else if ((value = option_value(argc, argv, &i, "--tsc-hz"))) {
            if (parse_hz_arg(value, &tsc_hz))
                return bad_option("warp", argv[i]);
        } else {
            return bad_option("warp", argv[i]);
        }
    }
    if (ncpus < 0 && (ncpus = affinity_cpus(cpus, MAX_CPUS)) <= 0) {
        fprintf(stderr, "nvx-time-probe warp: sched_getaffinity: %s\n", strerror(errno));
        return EXIT_ERROR;
    }
    if (check_cpus_allowed("warp", cpus, ncpus) != 0)
        return EXIT_USAGE;
    detect_tsc_rate(tsc_hz);
    if (g_tsc.hz <= 0) {
        fprintf(stderr, "nvx-time-probe warp: unknown TSC rate; pass --tsc-hz\n");
        return EXIT_ERROR;
    }
    budget_cycles = (uint64_t)((double)duration_ms * g_tsc.hz / 1000.0);
    stall_cycles = (uint64_t)(STALL_SECONDS * g_tsc.hz) + budget_cycles;
    offsets_x2 = malloc(PP_MAX_SAMPLES * sizeof(*offsets_x2));
    if (!offsets_x2) {
        fprintf(stderr, "nvx-time-probe warp: out of memory\n");
        return EXIT_ERROR;
    }
    format_cpu_list(cpus, ncpus, cpu_text, sizeof(cpu_text));
    printf("nvx-time-probe %s warp: cpus=%s pairs=%d duration_ms=%" PRIu64
           " (per test per pair) tsc_hz=%.0f tsc_hz_source=%s bound_ns=%" PRIu64 "\n",
           PROBE_VERSION, cpu_text, ncpus * (ncpus - 1) / 2, duration_ms, g_tsc.hz,
           g_tsc.source, bound_ns);
    fflush(stdout);

    for (int i = 0; i < ncpus; i++) {
        for (int j = i + 1; j < ncpus; j++) {
            struct pair_result r;
            double skew_bound;

            if (run_pair(cpus[i], cpus[j], budget_cycles, stall_cycles, offsets_x2,
                         &r) != 0) {
                fprintf(stderr, "nvx-time-probe warp: pair %d-%d: %s\n", cpus[i],
                        cpus[j], strerror(errno));
                free(offsets_x2);
                return EXIT_ERROR;
            }
            pairs++;
            total_warps += r.warps;
            if (r.max_warp_cycles > max_backward)
                max_backward = r.max_warp_cycles;
            if (r.stalled) {
                stalled++;
                printf("pair %d-%d: warp_iterations=%" PRIu64 " warps=%" PRIu64
                       " max_backward_cycles=%" PRIu64 " ping_pong=stalled\n",
                       r.cpu_a, r.cpu_b, r.warp_iterations, r.warps, r.max_warp_cycles);
                continue;
            }
            if (!r.consistent)
                inconsistent++;
            skew_bound = r.consistent
                             ? (abs_double((double)r.lo_cycles) > abs_double((double)r.hi_cycles)
                                    ? abs_double((double)r.lo_cycles)
                                    : abs_double((double)r.hi_cycles))
                             : abs_double(r.offset_cycles) + r.uncertainty_cycles;
            if (abs_double(r.offset_cycles) > max_abs_offset)
                max_abs_offset = abs_double(r.offset_cycles);
            if (r.uncertainty_cycles > max_uncertainty)
                max_uncertainty = r.uncertainty_cycles;
            if (skew_bound > max_skew_bound)
                max_skew_bound = skew_bound;
            printf("pair %d-%d: warp_iterations=%" PRIu64 " warps=%" PRIu64
                   " max_backward_cycles=%" PRIu64 " max_backward_ns=%lld rounds=%" PRIu64
                   " min_rtt_ns=%lld offset_cycles=%.1f offset_ns=%lld uncertainty_ns=%lld"
                   " bounds_ns=[%lld,%lld] median_offset_ns=%lld consistent=%d\n",
                   r.cpu_a, r.cpu_b, r.warp_iterations, r.warps, r.max_warp_cycles,
                   round_ll(cycles_to_ns((double)r.max_warp_cycles)), r.rounds,
                   round_ll(cycles_to_ns((double)r.min_rtt_cycles)), r.offset_cycles,
                   round_ll(cycles_to_ns(r.offset_cycles)),
                   round_ll(cycles_to_ns(r.uncertainty_cycles)),
                   round_ll(cycles_to_ns((double)r.lo_cycles)),
                   round_ll(cycles_to_ns((double)r.hi_cycles)),
                   round_ll(cycles_to_ns(r.median_offset_cycles)), r.consistent);
            fflush(stdout);
        }
    }
    free(offsets_x2);
    if (pairs == 0)
        printf("single CPU: no pairs to compare\n");

    pass = stalled == 0 && cycles_to_ns((double)max_backward) <= (double)bound_ns &&
           cycles_to_ns(max_abs_offset) <= (double)bound_ns;
    // The offset estimate decides nothing when its uncertainty reaches the bound
    // (for example, under trapped RDTSC, where every read exits to the VMM).
    conclusive = pairs == 0 || (stalled == 0 && cycles_to_ns(max_uncertainty) < (double)bound_ns);
    if (!conclusive)
        printf("warning: offset uncertainty (+/-%lld ns) is not below the bound (%" PRIu64
               " ns); the verdict is not conclusive (trapped RDTSC or a descheduled vCPU?)\n",
               round_ll(cycles_to_ns(max_uncertainty)), bound_ns);
    printf("NVX-TIME-PROBE warp max_backward_cycles=%" PRIu64 " max_backward_ns=%lld"
           " max_abs_offset_ns=%lld pairs=%d verdict=%s bound_ns=%" PRIu64 "\n",
           max_backward, round_ll(cycles_to_ns((double)max_backward)),
           round_ll(cycles_to_ns(max_abs_offset)), pairs, pass ? "PASS" : "FAIL",
           bound_ns);
    printf("NVX-TIME-PROBE warp-detail max_abs_offset_cycles=%.1f max_uncertainty_ns=%lld"
           " max_skew_bound_ns=%lld total_warps=%" PRIu64 " inconsistent_pairs=%d"
           " stalled_pairs=%d cpus=%s duration_ms=%" PRIu64 " tsc_hz=%.0f"
           " tsc_hz_source=%s conclusive=%d\n",
           max_abs_offset, round_ll(cycles_to_ns(max_uncertainty)),
           round_ll(cycles_to_ns(max_skew_bound)), total_warps, inconsistent, stalled,
           cpu_text, duration_ms, g_tsc.hz, g_tsc.source, conclusive);
    return pass ? 0 : EXIT_FAIL;
}

// ---------------------------------------------------------------------------
// clock
// ---------------------------------------------------------------------------

static const char *const tsc_flag_names[] = {
    "tsc",          "rdtscp",     "constant_tsc", "nonstop_tsc",        "nonstop_tsc_s3",
    "tsc_known_freq", "tsc_reliable", "tsc_deadline_timer", "tsc_adjust", "art",
    "hypervisor",   "arat",       "aperfmperf",   "hwp",                "x2apic",
    NULL,
};

static const char *const hypervisor_patterns[] = {
    "Hypervisor detected", "Hyper-V", "Booting paravirtualized kernel", "kvm-guest",
    NULL,
};

static const char *const time_patterns[] = {
    "tsc",           "TSC",         "clocksource", "clockevent", "kvm-clock",
    "APIC timer",    "LAPIC",       "lapic",       "calibrat",   "Calibrat",
    "unchecked MSR", "hpet",        "HPET",        "timekeeping", "sched_clock",
    "detected stall", "watchdog",   "lockup",      "hung_task",  "jiffies",
    "rtc",           "RTC",         NULL,
};

static const char *const param_files[] = {
    "/sys/module/rcupdate/parameters/rcu_cpu_stall_suppress",
    "/sys/module/rcupdate/parameters/rcu_cpu_stall_timeout",
    "/proc/sys/kernel/watchdog",
    "/proc/sys/kernel/soft_watchdog",
    "/proc/sys/kernel/nmi_watchdog",
    "/proc/sys/kernel/watchdog_thresh",
    "/proc/sys/kernel/hung_task_timeout_secs",
    NULL,
};

struct kmsg_filter {
    const char *const *patterns;
    int matches;
    int tsc_unstable;
    int unchecked_msr;
};

struct tick_info {
    bool seen;
    int tick_mode;
    int state;
    int hres_active;
    int nohz;
    char device[64];
    char handler[64];
};

static void add_unique(char *list, size_t size, const char *item)
{
    size_t len = strlen(item);

    for (const char *p = list; (p = strstr(p, item)) != NULL; p += len)
        if ((p == list || p[-1] == ',') && (p[len] == ',' || p[len] == '\0'))
            return;
    if (list[0])
        strncat(list, ",", size - strlen(list) - 1);
    strncat(list, item, size - strlen(list) - 1);
}

static void sanitize_token(char *text)
{
    for (char *p = text; *p; p++)
        if (!((*p >= 'a' && *p <= 'z') || (*p >= 'A' && *p <= 'Z') ||
              (*p >= '0' && *p <= '9') || *p == '#' || *p == '-' || *p == '.'))
            *p = '_';
}

static void print_clock(const char *name, clockid_t id, struct timespec *out)
{
    struct timespec ts;

    if (clock_gettime(id, &ts) != 0) {
        printf("%-20s unavailable (%s)\n", name, strerror(errno));
        return;
    }
    if (out)
        *out = ts;
    printf("%-20s %lld.%09ld", name, (long long)ts.tv_sec, ts.tv_nsec);
    if (id == CLOCK_REALTIME) {
        struct tm tm;
        char iso[32];
        time_t seconds = ts.tv_sec;

        gmtime_r(&seconds, &tm);
        strftime(iso, sizeof(iso), "%Y-%m-%dT%H:%M:%S", &tm);
        printf("  (%s.%09ldZ)", iso, ts.tv_nsec);
    }
    printf("\n");
}

static void kmsg_filter_cb(uint64_t usec, const char *text, void *ctx)
{
    struct kmsg_filter *filter = ctx;

    if (strstr(text, "Marking TSC unstable") || strstr(text, "TSC unstable"))
        filter->tsc_unstable++;
    if (strstr(text, "unchecked MSR access"))
        filter->unchecked_msr++;
    for (const char *const *pattern = filter->patterns; *pattern; pattern++) {
        if (strstr(text, *pattern)) {
            printf("[%5llu.%06llu] %s\n", (unsigned long long)(usec / 1000000),
                   (unsigned long long)(usec % 1000000), text);
            filter->matches++;
            return;
        }
    }
}

static const char *tick_mode_name(int mode)
{
    return mode == 0 ? "periodic" : mode == 1 ? "oneshot" : "unknown";
}

static const char *clockevent_state_name(int state)
{
    static const char *const names[] = {"detached", "shutdown", "periodic", "oneshot",
                                        "oneshot_stopped"};

    return state >= 0 && state <= 4 ? names[state] : "unknown";
}

static void print_timer_list(char *summary, size_t size)
{
    static struct tick_info cpus[MAX_CPUS];
    struct tick_info broadcast = {false, -1, -1, -1, -1, "", ""};
    struct tick_info *cur = NULL;
    char devices[256] = "";
    char modes[64] = "";
    char states[128] = "";
    char line[512];
    int hr_cpu = -1;
    int tick_mode = -1;
    int count = 0;
    FILE *file = fopen("/proc/timer_list", "r");

    if (!file) {
        printf("timer_list: unavailable (%s)\n", strerror(errno));
        snprintf(summary, size, "clockevents=unavailable");
        return;
    }
    for (int i = 0; i < MAX_CPUS; i++)
        cpus[i] = (struct tick_info){false, -1, -1, -1, -1, "", ""};
    while (fgets(line, sizeof(line), file)) {
        char name[64];
        int value;

        line[strcspn(line, "\n")] = '\0';
        if (strncmp(line, "cpu: ", 5) == 0) {
            hr_cpu = atoi(line + 5);
            cur = NULL;
        } else if (strncmp(line, "Tick Device: mode:", 18) == 0) {
            tick_mode = atoi(line + 18);
            hr_cpu = -1;
            cur = NULL;
        } else if (strncmp(line, "Broadcast device", 16) == 0) {
            cur = &broadcast;
            cur->seen = true;
            cur->tick_mode = tick_mode;
        } else if (sscanf(line, "Per CPU device: %d", &value) == 1) {
            cur = value >= 0 && value < MAX_CPUS ? &cpus[value] : NULL;
            if (cur) {
                cur->seen = true;
                cur->tick_mode = tick_mode;
            }
        } else if (cur && sscanf(line, "Clock Event Device: %63s", name) == 1) {
            snprintf(cur->device, sizeof(cur->device), "%s", name);
        } else if (cur && sscanf(line, " mode: %d", &value) == 1) {
            cur->state = value;
        } else if (cur && sscanf(line, " event_handler: %63s", name) == 1) {
            snprintf(cur->handler, sizeof(cur->handler), "%s", name);
        } else if (hr_cpu >= 0 && hr_cpu < MAX_CPUS) {
            if (sscanf(line, " .hres_active : %d", &value) == 1)
                cpus[hr_cpu].hres_active = value;
            else if (sscanf(line, " .nohz_mode : %d", &value) == 1 ||
                     sscanf(line, " .nohz : %d", &value) == 1)
                cpus[hr_cpu].nohz = value;
        }
    }
    fclose(file);

    if (broadcast.seen)
        printf("broadcast: device=%s state=%s tick_mode=%s handler=%s\n",
               broadcast.device[0] ? broadcast.device : "none",
               clockevent_state_name(broadcast.state), tick_mode_name(broadcast.tick_mode),
               broadcast.handler[0] ? broadcast.handler : "none");
    for (int i = 0; i < MAX_CPUS; i++) {
        if (!cpus[i].seen)
            continue;
        count++;
        printf("cpu%d: device=%s state=%s tick_mode=%s handler=%s hres_active=%d nohz=%d\n",
               i, cpus[i].device[0] ? cpus[i].device : "none",
               clockevent_state_name(cpus[i].state), tick_mode_name(cpus[i].tick_mode),
               cpus[i].handler[0] ? cpus[i].handler : "none", cpus[i].hres_active,
               cpus[i].nohz);
        add_unique(devices, sizeof(devices), cpus[i].device[0] ? cpus[i].device : "none");
        add_unique(modes, sizeof(modes), tick_mode_name(cpus[i].tick_mode));
        add_unique(states, sizeof(states), clockevent_state_name(cpus[i].state));
    }
    if (!broadcast.seen && count == 0)
        printf("timer_list: no tick devices found (file empty or masked)\n");
    snprintf(summary, size,
             "clockevent_cpus=%d clockevent_devices=%s tick_modes=%s clockevent_states=%s"
             " broadcast=%s:%s",
             count, devices[0] ? devices : "none", modes[0] ? modes : "none",
             states[0] ? states : "none",
             broadcast.seen && broadcast.device[0] ? broadcast.device : "none",
             broadcast.seen ? clockevent_state_name(broadcast.state) : "none");
}

struct cpuid_summary {
    char vendor[16];
    uint32_t max_leaf;
    uint32_t features_eax;
    uint32_t features_edx;
    bool hypervisor;
};

static void print_cpuid(struct cpuid_summary *s)
{
    uint32_t r[4];
    uint32_t max_basic, max_ext, family, model;

    memset(s, 0, sizeof(*s));
    cpuid(0, 0, r);
    max_basic = r[0];
    cpuid(0x80000000u, 0, r);
    max_ext = r[0];
    cpuid(1, 0, r);
    family = (r[0] >> 8) & 0xf;
    model = (r[0] >> 4) & 0xf;
    if (family == 0x6 || family == 0xf)
        model |= (r[0] >> 12) & 0xf0;
    if (family == 0xf)
        family += (r[0] >> 20) & 0xff;
    s->hypervisor = (r[2] >> 31) & 1;
    printf("cpuid.1: family=%u model=%u stepping=%u edx.tsc=%u ecx.tsc_deadline=%u"
           " ecx.hypervisor=%u\n",
           family, model, r[0] & 0xf, (r[3] >> 4) & 1, (r[2] >> 24) & 1, s->hypervisor);
    if (max_basic >= 6) {
        cpuid(6, 0, r);
        printf("cpuid.6: eax.arat=%u eax.hwp=%u ecx.aperfmperf=%u\n", (r[0] >> 2) & 1,
               (r[0] >> 7) & 1, r[2] & 1);
    }
    if (max_basic >= 7) {
        cpuid(7, 0, r);
        printf("cpuid.7.0: ebx.tsc_adjust=%u\n", (r[1] >> 1) & 1);
    }
    if (max_basic >= 0xa) {
        cpuid(0xa, 0, r);
        printf("cpuid.0xa: eax=%#010x (pmu_version=%u)\n", r[0], r[0] & 0xff);
    }
    if (max_basic >= 0x15) {
        cpuid(0x15, 0, r);
        printf("cpuid.0x15: eax.denominator=%u ebx.numerator=%u ecx.crystal_hz=%u", r[0],
               r[1], r[2]);
        if (r[0] && r[1] && r[2])
            printf(" (tsc_hz=%.0f)", (double)r[2] * r[1] / r[0]);
        printf("\n");
    }
    if (max_basic >= 0x16) {
        cpuid(0x16, 0, r);
        printf("cpuid.0x16: eax.base_mhz=%u ebx.max_mhz=%u ecx.bus_mhz=%u\n",
               r[0] & 0xffff, r[1] & 0xffff, r[2] & 0xffff);
    }
    if (max_ext >= 0x80000001u) {
        cpuid(0x80000001u, 0, r);
        printf("cpuid.0x80000001: edx.rdtscp=%u\n", (r[3] >> 27) & 1);
    }
    if (max_ext >= 0x80000007u) {
        cpuid(0x80000007u, 0, r);
        printf("cpuid.0x80000007: edx.invariant_tsc=%u\n", (r[3] >> 8) & 1);
    }
    if (!s->hypervisor) {
        printf("cpuid.0x40000000: not queried (hypervisor bit clear)\n");
        snprintf(s->vendor, sizeof(s->vendor), "none");
        return;
    }
    cpuid(0x40000000u, 0, r);
    s->max_leaf = r[0];
    memcpy(s->vendor, &r[1], 4);
    memcpy(s->vendor + 4, &r[2], 4);
    memcpy(s->vendor + 8, &r[3], 4);
    s->vendor[12] = '\0';
    for (int i = 0; i < 12 && s->vendor[i]; i++)
        if (s->vendor[i] < 0x20 || s->vendor[i] > 0x7e)
            s->vendor[i] = '.';
    printf("cpuid.0x40000000: eax.max_leaf=%#x vendor=\"%s\"\n", s->max_leaf, s->vendor);
    for (uint32_t leaf = 0x40000001u; leaf <= s->max_leaf && leaf <= 0x40000010u; leaf++) {
        cpuid(leaf, 0, r);
        printf("cpuid.%#x: eax=%#010x ebx=%#010x ecx=%#010x edx=%#010x\n", leaf, r[0],
               r[1], r[2], r[3]);
        if (leaf == 0x40000003u) {
            s->features_eax = r[0];
            s->features_edx = r[3];
        }
    }
    if (strcmp(s->vendor, "Microsoft Hv") == 0 && s->max_leaf >= 0x40000003u)
        printf("hyperv.features: hypercall=%u vp_index=%u access_frequency_msrs=%u"
               " access_tsc_invariant=%u frequency_msrs_available=%u\n",
               (s->features_eax >> 5) & 1, (s->features_eax >> 6) & 1,
               (s->features_eax >> 11) & 1, (s->features_eax >> 15) & 1,
               (s->features_edx >> 8) & 1);
}

// Opt-in: reads go through the msr driver's rdmsr_safe(), so a #GP returns
// EIO instead of faulting in the guest.
static void print_msrs(void)
{
    static const struct {
        uint32_t index;
        const char *name;
    } msrs[] = {
        {0x10, "IA32_TSC"},
        {0x3b, "IA32_TSC_ADJUST"},
        {0x40000002u, "HV_X64_MSR_VP_INDEX"},
        {0x40000022u, "HV_X64_MSR_TSC_FREQUENCY"},
        {0x40000023u, "HV_X64_MSR_APIC_FREQUENCY"},
        {0x40000118u, "HV_X64_MSR_TSC_INVARIANT_CONTROL"},
        {0x4b564d01u, "MSR_KVM_SYSTEM_TIME_NEW"},
    };
    int fd = open("/dev/cpu/0/msr", O_RDONLY | O_CLOEXEC);

    if (fd < 0) {
        printf("msr: /dev/cpu/0/msr unavailable (%s)\n", strerror(errno));
        return;
    }
    for (size_t i = 0; i < sizeof(msrs) / sizeof(msrs[0]); i++) {
        uint64_t value;

        if (pread(fd, &value, sizeof(value), (off_t)msrs[i].index) == sizeof(value))
            printf("msr %#010x %-34s = %#" PRIx64 " (%" PRIu64 ")\n", msrs[i].index,
                   msrs[i].name, value, value);
        else
            printf("msr %#010x %-34s : %s\n", msrs[i].index, msrs[i].name,
                   errno == EIO ? "read faulted (#GP)" : strerror(errno));
    }
    close(fd);
}

static int cmd_clock(int argc, char **argv)
{
    uint64_t measure_ms = 200;
    double tsc_hz = 0;
    bool read_msrs = false;
    struct timespec mono = {0};
    struct kmsg_filter hv_filter = {hypervisor_patterns, 0, 0, 0};
    struct kmsg_filter time_filter = {time_patterns, 0, 0, 0};
    struct cpuid_summary ids;
    char clocksource[64];
    char available[256] = "";
    char present[256] = "";
    char absent[256] = "";
    char timer_summary[512];
    char line[4096];
    uint64_t tsc;
    int cpu;

    for (int i = 2; i < argc; i++) {
        const char *value;

        if ((value = option_value(argc, argv, &i, "--measure-ms"))) {
            if (parse_u64_arg(value, &measure_ms) || measure_ms > 60000)
                return bad_option("clock", argv[i]);
        } else if ((value = option_value(argc, argv, &i, "--tsc-hz"))) {
            if (parse_hz_arg(value, &tsc_hz))
                return bad_option("clock", argv[i]);
        } else if (strcmp(argv[i], "--msr") == 0) {
            read_msrs = true;
        } else {
            return bad_option("clock", argv[i]);
        }
    }
    detect_tsc_rate(tsc_hz);

    printf("nvx-time-probe %s clock\n== clocks\n", PROBE_VERSION);
    cpu = sched_getcpu();
    tsc = rdtsc_fenced();
    print_clock("CLOCK_REALTIME", CLOCK_REALTIME, NULL);
    print_clock("CLOCK_MONOTONIC", CLOCK_MONOTONIC, &mono);
    print_clock("CLOCK_MONOTONIC_RAW", CLOCK_MONOTONIC_RAW, NULL);
    print_clock("CLOCK_BOOTTIME", CLOCK_BOOTTIME, NULL);
    printf("%-20s %" PRIu64 " on cpu %d", "TSC", tsc, cpu);
    if (g_tsc.hz > 0)
        printf(" (%.9f s; tsc_minus_monotonic_ns=%lld)", (double)tsc / g_tsc.hz,
               round_ll((double)tsc * 1e9 / g_tsc.hz -
                        ((double)mono.tv_sec * 1e9 + (double)mono.tv_nsec)));
    printf("\ntsc_hz=%.0f tsc_hz_source=%s\n", g_tsc.hz, g_tsc.source);
    if (measure_ms > 0)
        printf("tsc_hz_measured=%.0f (over %" PRIu64 " ms against CLOCK_MONOTONIC_RAW)\n",
               measure_tsc_hz((unsigned)measure_ms), measure_ms);

    printf("== clocksource\n");
    current_clocksource(clocksource, sizeof(clocksource));
    if (read_first_line(CLOCKSOURCE_DIR "/available_clocksource", line, sizeof(line)) == 0)
        for (char *token = strtok(line, " "); token; token = strtok(NULL, " "))
            add_unique(available, sizeof(available), token);
    printf("current: %s\navailable: %s\n", clocksource, available[0] ? available : "unknown");

    printf("== cpuinfo\n");
    {
        FILE *file = fopen("/proc/cpuinfo", "r");
        int processors = 0;
        bool model_printed = false;
        char mhz[512] = "";

        while (file && fgets(line, sizeof(line), file)) {
            char *colon = strchr(line, ':');

            line[strcspn(line, "\n")] = '\0';
            if (strncmp(line, "processor", 9) == 0)
                processors++;
            else if (!model_printed && strncmp(line, "model name", 10) == 0 && colon) {
                printf("model name:%s\n", colon + 1);
                model_printed = true;
            } else if (strncmp(line, "cpu MHz", 7) == 0 && colon)
                add_unique(mhz, sizeof(mhz), colon + 2);
        }
        if (file)
            fclose(file);
        printf("processors: %d\ncpu MHz: %s\n", processors, mhz[0] ? mhz : "unknown");
    }
    for (const char *const *flag = tsc_flag_names; *flag; flag++)
        add_unique(cpuinfo_has_flag(*flag) ? present : absent,
                   cpuinfo_has_flag(*flag) ? sizeof(present) : sizeof(absent), *flag);
    printf("time flags present: %s\ntime flags absent: %s\n", present[0] ? present : "none",
           absent[0] ? absent : "none");

    printf("== cpuid\n");
    print_cpuid(&ids);
    if (read_msrs) {
        printf("== msr (cpu 0)\n");
        print_msrs();
    }

    printf("== kernel\n");
    if (read_first_line("/proc/cmdline", line, sizeof(line)) == 0)
        printf("cmdline: %s\n", line);
    if (read_first_line("/sys/devices/system/cpu/online", line, sizeof(line)) == 0)
        printf("online cpus: %s\n", line);
    for (const char *const *path = param_files; *path; path++) {
        if (read_first_line(*path, line, sizeof(line)) == 0)
            printf("%s = %s\n", *path, line);
        else
            printf("%s: absent\n", *path);
    }

    printf("== kmsg: hypervisor\n");
    if (kmsg_foreach(kmsg_filter_cb, &hv_filter) != 0)
        printf("(kernel log unavailable: %s)\n", strerror(errno));
    else if (!hv_filter.matches)
        printf("(none)\n");
    printf("== kmsg: time\n");
    if (kmsg_foreach(kmsg_filter_cb, &time_filter) != 0)
        printf("(kernel log unavailable: %s)\n", strerror(errno));
    else if (!time_filter.matches)
        printf("(none)\n");

    printf("== timer_list\n");
    print_timer_list(timer_summary, sizeof(timer_summary));

    sanitize_token(ids.vendor);
    printf("NVX-TIME-PROBE clock clocksource=%s available=%s tsc_hz=%.0f tsc_hz_source=%s"
           " hv_vendor=%s hv_max_leaf=%#x hv_features_eax=%#x hv_features_edx=%#x"
           " flags=%s missing_flags=%s tsc_unstable_msgs=%d unchecked_msr_msgs=%d %s\n",
           clocksource, available[0] ? available : "unknown", g_tsc.hz, g_tsc.source,
           ids.vendor, ids.max_leaf, ids.features_eax, ids.features_edx,
           present[0] ? present : "none", absent[0] ? absent : "none",
           time_filter.tsc_unstable, time_filter.unchecked_msr, timer_summary);
    return 0;
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

static void usage(FILE *out)
{
    fprintf(out,
            "usage: nvx-time-probe COMMAND [OPTIONS]\n"
            "\n"
            "Commands:\n"
            "  latency [--iterations N] [--batch B] [--cpu C] [--tsc-hz HZ]\n"
            "      Per-operation cost (p50/p99/min/mean ns) of rdtsc, rdtscp,\n"
            "      lfence;rdtsc, clock_gettime(MONOTONIC/REALTIME) through the vDSO,\n"
            "      and syscall controls. Default: 2000 timed batches of 50 operations\n"
            "      on the current CPU.\n"
            "  warp [--duration-ms N] [--cpus LIST] [--bound-ns N] [--tsc-hz HZ]\n"
            "      Pairwise cross-CPU TSC checks: a check_tsc_warp-style spinlock test\n"
            "      and a ping-pong offset estimate, each running N ms per pair\n"
            "      (default 100). LIST looks like 0-3,6 (default: every CPU in the\n"
            "      affinity mask). The bound defaults to 1000 ns.\n"
            "  clock [--measure-ms N] [--tsc-hz HZ] [--msr]\n"
            "      Clocks, clocksources, TSC flags, CPUID time leaves, hypervisor and\n"
            "      time kernel messages, and the /proc/timer_list clock event summary.\n"
            "      --msr also reads the TSC, Hyper-V time, and kvmclock MSRs on CPU 0\n"
            "      through /dev/cpu/0/msr (a #GP is reported, not raised).\n"
            "  version\n"
            "\n"
            "The TSC rate comes from --tsc-hz, sysfs tsc_freq_khz, the kernel log, or\n"
            "/proc/cpuinfo, in that order, and is measured as a last resort.\n"
            "Exit status: 0 success, 1 warp FAIL, 2 usage error, 3 runtime error.\n");
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc < 2) {
        usage(stderr);
        return EXIT_USAGE;
    }
    if (strcmp(argv[1], "latency") == 0)
        return cmd_latency(argc, argv);
    if (strcmp(argv[1], "warp") == 0)
        return cmd_warp(argc, argv);
    if (strcmp(argv[1], "clock") == 0)
        return cmd_clock(argc, argv);
    if (strcmp(argv[1], "version") == 0 || strcmp(argv[1], "--version") == 0) {
        printf("nvx-time-probe %s\n", PROBE_VERSION);
        return 0;
    }
    if (strcmp(argv[1], "help") == 0 || strcmp(argv[1], "--help") == 0 ||
        strcmp(argv[1], "-h") == 0) {
        usage(stdout);
        return 0;
    }
    usage(stderr);
    return EXIT_USAGE;
}
