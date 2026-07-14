#define _GNU_SOURCE

#include <stdint.h>
#include <sys/mman.h>
#include <unistd.h>

volatile int32_t pointer_memory_sink;

__attribute__((noinline)) static void inspect_boundaries(int32_t *valid_pointer,
                                                         int32_t *boundary_pointer) {
    __asm__ volatile("" : : "g"(valid_pointer), "g"(boundary_pointer) : "memory");
    pointer_memory_sink = *valid_pointer;
}

int main(void) {
    long page_size = sysconf(_SC_PAGESIZE);
    if (page_size <= 0) {
        return 1;
    }
    uint8_t *mapping = mmap(NULL, (size_t)page_size * 2, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mapping == MAP_FAILED || munmap(mapping + page_size, (size_t)page_size) != 0) {
        return 1;
    }

    int32_t valid = 42;
    int32_t *boundary = (int32_t *)(mapping + page_size - 2);
    mapping[page_size - 2] = 0x2a;
    mapping[page_size - 1] = 0;
    inspect_boundaries(&valid, boundary);

    if (munmap(mapping, (size_t)page_size) != 0) {
        return 1;
    }
    return pointer_memory_sink == 42 ? 0 : 1;
}
