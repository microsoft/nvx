#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <linux/random.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/random.h>
#include <time.h>
#include <unistd.h>

#define SEED_BYTES 64
#define GENERATION_ID_BYTES 16
#define RANDOM_SAMPLE_BYTES 32
#define CMOS_INDEX_PORT 0x70
#define CMOS_DATA_PORT 0x71
#define CMOS_STATUS_A 0x0a
#define CMOS_STATUS_B 0x0b
#define CMOS_UPDATE_IN_PROGRESS 0x80
#define CMOS_24_HOUR 0x02
#define CMOS_BINARY 0x04
#define CMOS_CENTURY 0x32

struct seed_payload {
    int entropy_count;
    int buf_size;
    unsigned char bytes[SEED_BYTES];
};

static int read_seed(const char *path, unsigned char *bytes)
{
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        fprintf(stderr, "nvx-reseed: open %s: %s\n", path, strerror(errno));
        return -1;
    }

    size_t offset = 0;
    while (offset < SEED_BYTES) {
        ssize_t count = read(fd, bytes + offset, SEED_BYTES - offset);
        if (count <= 0) {
            fprintf(stderr, "nvx-reseed: read %s: %s\n", path,
                    count == 0 ? "unexpected end of file" : strerror(errno));
            close(fd);
            return -1;
        }
        offset += (size_t)count;
    }
    close(fd);
    return 0;
}

static int print_hex(const unsigned char *bytes, size_t size)
{
    for (size_t index = 0; index < size; ++index) {
        if (printf("%02x", bytes[index]) < 0) {
            fprintf(stderr, "nvx-reseed: write output failed\n");
            return -1;
        }
    }
    if (putchar('\n') == EOF) {
        fprintf(stderr, "nvx-reseed: write output failed\n");
        return -1;
    }
    return 0;
}

static int hex_value(char digit)
{
    if (digit >= '0' && digit <= '9') {
        return digit - '0';
    }
    if (digit >= 'a' && digit <= 'f') {
        return digit - 'a' + 10;
    }
    if (digit >= 'A' && digit <= 'F') {
        return digit - 'A' + 10;
    }
    return -1;
}

static int validate_generation_change(const char *previous,
                                      const unsigned char *generation_id)
{
    if (strlen(previous) != GENERATION_ID_BYTES * 2) {
        fprintf(stderr,
                "nvx-reseed: previous VM generation ID is not 32 hex digits\n");
        return -1;
    }
    unsigned char previous_bytes[GENERATION_ID_BYTES];
    for (size_t index = 0; index < sizeof(previous_bytes); ++index) {
        int high = hex_value(previous[index * 2]);
        int low = hex_value(previous[index * 2 + 1]);
        if (high < 0 || low < 0) {
            fprintf(stderr,
                    "nvx-reseed: previous VM generation ID is not hexadecimal\n");
            return -1;
        }
        previous_bytes[index] = (unsigned char)((high << 4) | low);
    }
    if (memcmp(previous_bytes, generation_id, sizeof(previous_bytes)) == 0) {
        fprintf(stderr, "nvx-reseed: VM generation ID did not change\n");
        return -1;
    }
    return 0;
}

static int print_random_sample(void)
{
    unsigned char sample[RANDOM_SAMPLE_BYTES];
    size_t offset = 0;
    while (offset < sizeof(sample)) {
        ssize_t count =
            getrandom(sample + offset, sizeof(sample) - offset, 0);
        if (count > 0) {
            offset += (size_t)count;
            continue;
        }
        if (count < 0 && errno == EINTR) {
            continue;
        }
        fprintf(stderr, "nvx-reseed: getrandom: %s\n",
                count == 0 ? "no bytes returned" : strerror(errno));
        return -1;
    }
    return print_hex(sample, sizeof(sample));
}

#ifndef __aarch64__
static int read_cmos_register(int fd, uint8_t index, uint8_t *value)
{
    if (pwrite(fd, &index, 1, CMOS_INDEX_PORT) != 1 ||
        pread(fd, value, 1, CMOS_DATA_PORT) != 1) {
        fprintf(stderr, "nvx-reseed: CMOS register 0x%02x: %s\n", index,
                strerror(errno));
        return -1;
    }
    return 0;
}

static uint8_t from_bcd(uint8_t value)
{
    return (uint8_t)((value & 0x0f) + ((value >> 4) * 10));
}

struct rtc_sample {
    uint8_t second;
    uint8_t minute;
    uint8_t hour;
    uint8_t day;
    uint8_t month;
    uint8_t year;
    uint8_t century;
    uint8_t status_b;
};

static int read_rtc_sample(int fd, struct rtc_sample *sample)
{
    uint8_t status_a;
    for (int attempt = 0; attempt < 1000; ++attempt) {
        if (read_cmos_register(fd, CMOS_STATUS_A, &status_a) != 0) {
            return -1;
        }
        if ((status_a & CMOS_UPDATE_IN_PROGRESS) == 0) {
            break;
        }
        if (attempt == 999) {
            fprintf(stderr, "nvx-reseed: CMOS update did not complete\n");
            return -1;
        }
    }

    return read_cmos_register(fd, 0x00, &sample->second) ||
           read_cmos_register(fd, 0x02, &sample->minute) ||
           read_cmos_register(fd, 0x04, &sample->hour) ||
           read_cmos_register(fd, 0x07, &sample->day) ||
           read_cmos_register(fd, 0x08, &sample->month) ||
           read_cmos_register(fd, 0x09, &sample->year) ||
           read_cmos_register(fd, CMOS_CENTURY, &sample->century) ||
           read_cmos_register(fd, CMOS_STATUS_B, &sample->status_b);
}

static int refresh_wall_clock(void)
{
    int port = open("/dev/port", O_RDWR | O_CLOEXEC);
    if (port < 0) {
        fprintf(stderr, "nvx-reseed: open /dev/port: %s\n", strerror(errno));
        return -1;
    }

    struct rtc_sample first;
    struct rtc_sample second;
    int matched = 0;
    for (int attempt = 0; attempt < 100; ++attempt) {
        if (read_rtc_sample(port, &first) != 0 ||
            read_rtc_sample(port, &second) != 0) {
            close(port);
            return -1;
        }
        if (memcmp(&first, &second, sizeof(first)) == 0) {
            matched = 1;
            break;
        }
    }
    close(port);
    if (!matched) {
        fprintf(stderr, "nvx-reseed: failed to obtain a stable CMOS sample\n");
        return -1;
    }

    uint8_t hour = first.hour;
    int is_pm = hour & 0x80;
    hour &= 0x7f;
    if ((first.status_b & CMOS_BINARY) == 0) {
        first.second = from_bcd(first.second);
        first.minute = from_bcd(first.minute);
        hour = from_bcd(hour);
        first.day = from_bcd(first.day);
        first.month = from_bcd(first.month);
        first.year = from_bcd(first.year);
        first.century = from_bcd(first.century);
    }
    if ((first.status_b & CMOS_24_HOUR) == 0) {
        hour = (uint8_t)((hour % 12) + (is_pm ? 12 : 0));
    }

    struct tm utc = {
        .tm_sec = first.second,
        .tm_min = first.minute,
        .tm_hour = hour,
        .tm_mday = first.day,
        .tm_mon = first.month - 1,
        .tm_year = first.century * 100 + first.year - 1900,
        .tm_isdst = 0,
    };
    time_t seconds = timegm(&utc);
    if (seconds < 0) {
        fprintf(stderr, "nvx-reseed: CMOS date is outside the supported range\n");
        return -1;
    }
    struct timespec time = {.tv_sec = seconds, .tv_nsec = 0};
    if (clock_settime(CLOCK_REALTIME, &time) != 0) {
        fprintf(stderr, "nvx-reseed: clock_settime: %s\n", strerror(errno));
        return -1;
    }
    return 0;
}
#else
static int refresh_wall_clock(void)
{
    return 0;
}
#endif

int main(int argc, char **argv)
{
    if (argc == 2 && strcmp(argv[1], "--sample") == 0) {
        return print_random_sample() == 0 ? 0 : 1;
    }

    int generation_only = 0;
    const char *seed_path;
    const char *previous_generation_id = NULL;
    if (argc == 2 || argc == 3) {
        seed_path = argv[1];
        if (argc == 3) {
            previous_generation_id = argv[2];
        }
    } else if (argc == 4 && strcmp(argv[1], "--generation-only") == 0) {
        generation_only = 1;
        seed_path = argv[2];
        previous_generation_id = argv[3];
    } else {
        fprintf(stderr,
                "usage: nvx-reseed --sample | [--generation-only] SEED_FILE "
                "[PREVIOUS_GENERATION_ID]\n");
        return 2;
    }

    struct seed_payload seed = {
        .entropy_count = SEED_BYTES * 8,
        .buf_size = SEED_BYTES,
    };
    if (read_seed(seed_path, seed.bytes) != 0) {
        return 1;
    }
    if (previous_generation_id != NULL &&
        validate_generation_change(previous_generation_id, seed.bytes) != 0) {
        return 1;
    }
    if (generation_only) {
        return print_hex(seed.bytes, GENERATION_ID_BYTES) == 0 ? 0 : 1;
    }

    int random = open("/dev/random", O_WRONLY | O_CLOEXEC);
    if (random < 0) {
        fprintf(stderr, "nvx-reseed: open /dev/random: %s\n", strerror(errno));
        return 1;
    }
    if (ioctl(random, RNDADDENTROPY, &seed) != 0 ||
        ioctl(random, RNDRESEEDCRNG, 0) != 0) {
        fprintf(stderr, "nvx-reseed: random ioctl: %s\n", strerror(errno));
        close(random);
        return 1;
    }
    close(random);
    if (refresh_wall_clock() != 0) {
        return 1;
    }
    if (previous_generation_id != NULL &&
        print_hex(seed.bytes, GENERATION_ID_BYTES) != 0) {
        return 1;
    }
    return 0;
}
