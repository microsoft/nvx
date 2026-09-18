#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define PORT_MAX UINT16_MAX
#define RESTORE_HEADER_SIZE 19
#define RESTORE_ENTROPY_SIZE 64
#define RESTORE_RANGE_SIZE 16
#define RESTORE_PACKET_SELECT 0xa5
#define GENERATION_ID_SELECT 0xa6
#define GENERATION_ID_SIZE 16
#define STATUS_GENERATION_ID_AVAILABLE 32
#define RESTORE_PACKET_MAX_SIZE                                             \
    (RESTORE_HEADER_SIZE + 2 + UINT8_MAX * RESTORE_RANGE_SIZE +            \
     RESTORE_ENTROPY_SIZE)

static const unsigned char RESTORE_HEADER_V1[RESTORE_HEADER_SIZE] =
    "OPENVMM_ENTROPY_V1";
static const unsigned char RESTORE_HEADER_V2[RESTORE_HEADER_SIZE] =
    "OPENVMM_ENTROPY_V2";
static const unsigned char RESTORE_HEADER_V3[RESTORE_HEADER_SIZE] =
    "OPENVMM_ENTROPY_V3";

static int parse_u64(const char *text, uint64_t *value)
{
    char *end;

    errno = 0;
    *value = strtoull(text, &end, 0);
    return errno == 0 && end != text && *end == '\0';
}

static int read_port_byte(int port, uint64_t offset, unsigned char *value)
{
    for (;;) {
        ssize_t count = pread(port, value, 1, (off_t)offset);
        if (count == 1) {
            return 0;
        }
        if (count < 0 && errno == EINTR) {
            continue;
        }
        fprintf(stderr, "nvx-port-io: read port 0x%llx: %s\n",
                (unsigned long long)offset,
                count == 0 ? "unexpected end of file" : strerror(errno));
        return -1;
    }
}

static int write_port_byte(int port, uint64_t offset, unsigned char value)
{
    for (;;) {
        ssize_t count = pwrite(port, &value, 1, (off_t)offset);
        if (count == 1) {
            return 0;
        }
        if (count < 0 && errno == EINTR) {
            continue;
        }
        fprintf(stderr, "nvx-port-io: write port 0x%llx: %s\n",
                (unsigned long long)offset,
                count == 0 ? "no byte written" : strerror(errno));
        return -1;
    }
}

static int write_all(int output, const unsigned char *buffer, size_t size)
{
    size_t offset = 0;
    while (offset < size) {
        ssize_t count = write(output, buffer + offset, size - offset);
        if (count > 0) {
            offset += (size_t)count;
            continue;
        }
        if (count < 0 && errno == EINTR) {
            continue;
        }
        fprintf(stderr, "nvx-port-io: write output: %s\n",
                count == 0 ? "no byte written" : strerror(errno));
        return -1;
    }
    return 0;
}

static int read_port_bytes(int port, uint64_t offset, unsigned char *buffer,
                           size_t size)
{
    for (size_t index = 0; index < size; ++index) {
        if (read_port_byte(port, offset, &buffer[index]) != 0) {
            return -1;
        }
    }
    return 0;
}

static int close_port(int port)
{
    if (close(port) == 0) {
        return 0;
    }
    fprintf(stderr, "nvx-port-io: close /dev/port: %s\n", strerror(errno));
    return -1;
}

static int print_hex(const unsigned char *bytes, size_t size)
{
    for (size_t index = 0; index < size; ++index) {
        if (printf("%02x", bytes[index]) < 0) {
            fprintf(stderr, "nvx-port-io: write output failed\n");
            return -1;
        }
    }
    if (putchar('\n') == EOF) {
        fprintf(stderr, "nvx-port-io: write output failed\n");
        return -1;
    }
    return 0;
}

static uint64_t read_le_u64(const unsigned char *bytes)
{
    uint64_t value = 0;
    for (size_t index = 0; index < sizeof(value); ++index) {
        value |= (uint64_t)bytes[index] << (index * 8);
    }
    return value;
}

static int read_restore_packet(uint64_t data_offset, uint64_t select_offset,
                               const char *path)
{
    unsigned char packet[RESTORE_PACKET_MAX_SIZE];
    unsigned int version;
    unsigned int online_count = 0;
    unsigned int range_count = 0;
    size_t packet_size = RESTORE_HEADER_SIZE;
    size_t payload_size;
    int port = open("/dev/port", O_RDWR | O_CLOEXEC);
    if (port < 0) {
        fprintf(stderr, "nvx-port-io: open /dev/port: %s\n", strerror(errno));
        return 1;
    }

    if (write_port_byte(port, select_offset, RESTORE_PACKET_SELECT) != 0 ||
        read_port_bytes(port, data_offset, packet, RESTORE_HEADER_SIZE) != 0) {
        close(port);
        return 1;
    }
    if (memcmp(packet, RESTORE_HEADER_V1, RESTORE_HEADER_SIZE) == 0) {
        version = 1;
        payload_size = RESTORE_ENTROPY_SIZE;
    } else if (memcmp(packet, RESTORE_HEADER_V2, RESTORE_HEADER_SIZE) == 0) {
        version = 2;
        payload_size = 1 + RESTORE_ENTROPY_SIZE;
    } else if (memcmp(packet, RESTORE_HEADER_V3, RESTORE_HEADER_SIZE) == 0) {
        version = 3;
        if (read_port_bytes(port, data_offset, packet + packet_size, 2) != 0) {
            close(port);
            return 1;
        }
        online_count = packet[packet_size];
        range_count = packet[packet_size + 1];
        packet_size += 2;
        payload_size =
            (size_t)range_count * RESTORE_RANGE_SIZE + RESTORE_ENTROPY_SIZE;
    } else {
        fprintf(stderr, "nvx-port-io: restore packet has an invalid header\n");
        close(port);
        return 1;
    }

    if (read_port_bytes(port, data_offset, packet + packet_size, payload_size) !=
        0) {
        close(port);
        return 1;
    }
    packet_size += payload_size;
    if (version == 2) {
        online_count = packet[RESTORE_HEADER_SIZE];
    }
    if (close_port(port) != 0) {
        return 1;
    }

    int output =
        open(path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, S_IRUSR | S_IWUSR);
    if (output < 0) {
        fprintf(stderr, "nvx-port-io: open %s: %s\n", path, strerror(errno));
        return 1;
    }
    if (write_all(output, packet, packet_size) != 0) {
        close(output);
        return 1;
    }
    if (close(output) != 0) {
        fprintf(stderr, "nvx-port-io: close %s: %s\n", path, strerror(errno));
        return 1;
    }
    printf("%u %u %u", version, online_count, range_count);
    if (version == 3) {
        size_t range_offset = RESTORE_HEADER_SIZE + 2;
        for (unsigned int index = 0; index < range_count; ++index) {
            uint64_t start = read_le_u64(packet + range_offset);
            uint64_t length =
                read_le_u64(packet + range_offset + sizeof(uint64_t));
            printf(" %" PRIu64 " %" PRIu64, start, length);
            range_offset += RESTORE_RANGE_SIZE;
        }
    }
    putchar('\n');
    return 0;
}

static int read_generation_id(uint64_t data_offset, uint64_t select_offset,
                              const char *path)
{
    unsigned char generation_id[GENERATION_ID_SIZE];
    unsigned char status;
    int port = open("/dev/port", O_RDWR | O_CLOEXEC);
    if (port < 0) {
        fprintf(stderr, "nvx-port-io: open /dev/port: %s\n", strerror(errno));
        return 1;
    }

    if (read_port_byte(port, select_offset, &status) != 0) {
        close(port);
        return 1;
    }
    if ((status & STATUS_GENERATION_ID_AVAILABLE) == 0) {
        fprintf(stderr,
                "nvx-port-io: VM generation ID is unavailable on port 0x%llx\n",
                (unsigned long long)select_offset);
        close(port);
        return 1;
    }
    if (write_port_byte(port, select_offset, GENERATION_ID_SELECT) != 0 ||
        read_port_bytes(port, data_offset, generation_id,
                        sizeof(generation_id)) != 0) {
        close(port);
        return 1;
    }
    if (close_port(port) != 0) {
        return 1;
    }

    if (path == NULL) {
        return print_hex(generation_id, sizeof(generation_id)) == 0 ? 0 : 1;
    }

    int output = open(path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC | O_NOFOLLOW,
                      S_IRUSR | S_IWUSR);
    if (output < 0) {
        fprintf(stderr, "nvx-port-io: open %s: %s\n", path, strerror(errno));
        return 1;
    }
    if (write_all(output, generation_id, sizeof(generation_id)) != 0) {
        close(output);
        return 1;
    }
    if (close(output) != 0) {
        fprintf(stderr, "nvx-port-io: close %s: %s\n", path, strerror(errno));
        return 1;
    }
    return 0;
}

static int read_u8(uint64_t offset)
{
    unsigned char value;
    int port = open("/dev/port", O_RDONLY | O_CLOEXEC);
    if (port < 0) {
        fprintf(stderr, "nvx-port-io: open /dev/port: %s\n", strerror(errno));
        return 1;
    }
    if (read_port_byte(port, offset, &value) != 0) {
        close(port);
        return 1;
    }
    if (close_port(port) != 0) {
        return 1;
    }
    printf("%u\n", value);
    return 0;
}

static int write_u8(uint64_t offset, uint64_t value)
{
    int port = open("/dev/port", O_WRONLY | O_CLOEXEC);
    if (port < 0) {
        fprintf(stderr, "nvx-port-io: open /dev/port: %s\n", strerror(errno));
        return 1;
    }
    if (write_port_byte(port, offset, (unsigned char)value) != 0) {
        close(port);
        return 1;
    }
    if (close_port(port) != 0) {
        return 1;
    }
    return 0;
}

static void usage(void)
{
    fprintf(stderr,
            "usage: nvx-port-io read-restore-packet DATA_PORT SELECT_PORT "
            "OUTPUT | read-generation-id DATA_PORT SELECT_PORT [OUTPUT] | "
            "read-u8 PORT | write-u8 PORT VALUE\n");
}

int main(int argc, char **argv)
{
    uint64_t offset;
    uint64_t select_offset;
    uint64_t value;

    if (argc >= 3 && parse_u64(argv[2], &offset) && offset <= PORT_MAX) {
        if (argc == 3 && strcmp(argv[1], "read-u8") == 0) {
            return read_u8(offset);
        }
        if (argc == 4 && strcmp(argv[1], "write-u8") == 0 &&
            parse_u64(argv[3], &value) && value <= UINT8_MAX) {
            return write_u8(offset, value);
        }
        if (argc == 5 && strcmp(argv[1], "read-restore-packet") == 0 &&
            parse_u64(argv[3], &select_offset) &&
            select_offset <= PORT_MAX) {
            return read_restore_packet(offset, select_offset, argv[4]);
        }
        if ((argc == 4 || argc == 5) &&
            strcmp(argv[1], "read-generation-id") == 0 &&
            parse_u64(argv[3], &select_offset) &&
            select_offset <= PORT_MAX) {
            return read_generation_id(offset, select_offset,
                                      argc == 5 ? argv[4] : NULL);
        }
    }

    usage();
    return 2;
}
