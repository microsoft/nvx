#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>

static int parse_u64(const char *text, uint64_t *value)
{
    char *end;

    errno = 0;
    *value = strtoull(text, &end, 0);
    return errno == 0 && end != text && *end == '\0';
}

int main(int argc, char **argv)
{
    uint64_t address;
    uint64_t value = 0;
    long page_size;
    uint64_t page_base;
    size_t page_offset;
    void *mapping;
    int memory;

    if ((argc != 2 && argc != 3) || !parse_u64(argv[1], &address) ||
        (argc == 3 && (!parse_u64(argv[2], &value) || value > UINT32_MAX))) {
        fprintf(stderr, "usage: nvx-mmio-write ADDRESS [VALUE]\n");
        return 2;
    }

    page_size = sysconf(_SC_PAGESIZE);
    if (page_size <= 0 || (page_size & (page_size - 1)) != 0) {
        fprintf(stderr, "invalid page size\n");
        return 1;
    }
    page_base = address & ~((uint64_t)page_size - 1);
    page_offset = (size_t)(address - page_base);
    if (page_offset > (size_t)page_size - sizeof(uint32_t)) {
        fprintf(stderr, "MMIO write crosses a page boundary\n");
        return 1;
    }

    memory = open("/dev/mem", O_RDWR | O_SYNC);
    if (memory < 0) {
        perror("open /dev/mem");
        return 1;
    }
    mapping = mmap(NULL, (size_t)page_size, PROT_READ | PROT_WRITE, MAP_SHARED,
                   memory, (off_t)page_base);
    if (mapping == MAP_FAILED) {
        perror("mmap /dev/mem");
        close(memory);
        return 1;
    }

    volatile uint32_t *target =
        (volatile uint32_t *)((unsigned char *)mapping + page_offset);
    if (argc == 2) {
        printf("%" PRIu32 "\n", *target);
    } else {
        *target = (uint32_t)value;
    }

    if (munmap(mapping, (size_t)page_size) != 0) {
        perror("munmap /dev/mem");
        close(memory);
        return 1;
    }
    if (close(memory) != 0) {
        perror("close /dev/mem");
        return 1;
    }
    return 0;
}
