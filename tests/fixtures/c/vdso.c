// Runs code in the vDSO, the shared object the kernel maps into every process.
// `vdso clock` faults inside the unexported helper behind clock_gettime, which
// libc calls; `vdso time` faults inside the vDSO's own time, to which libc's
// indirect function binds the program directly; `vdso calls` makes both calls
// with valid arguments and exits; `vdso move` moves the vDSO, as a checkpoint
// restore does, and then unmaps it.
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>

// Nothing is mapped at low addresses, so the vDSO's first store faults.
#define UNMAPPED ((void *)16)

// Callees may change a global, so each caller reads it before its call and
// keeps the value for after it in a register every callee, the vDSO
// included, must preserve.
long vdso_depth = 42;
struct timespec vdso_clock_value;
time_t vdso_time_value;

__attribute__((noinline)) long vdso_clock(struct timespec *destination) {
    long depth = vdso_depth;
    int status = clock_gettime(CLOCK_MONOTONIC_COARSE, destination);
    return status + depth;
}

__attribute__((noinline)) long vdso_time(time_t *destination) {
    long depth = vdso_depth;
    long now = (long)time(destination);
    return now + depth;
}

// Where the vDSO is mapped, as the process's own memory map says.
static int vdso_mapping(unsigned long *start, unsigned long *end) {
    FILE *maps = fopen("/proc/self/maps", "r");
    if (maps == NULL) {
        return 0;
    }
    char line[512];
    int found = 0;
    while (!found && fgets(line, sizeof line, maps) != NULL) {
        found = strstr(line, "[vdso]") != NULL && sscanf(line, "%lx-%lx", start, end) == 2;
    }
    fclose(maps);
    return found;
}

__attribute__((noinline)) void vdso_moved(void *address) {
    __asm__ volatile("" : : "r"(address) : "memory");
}

__attribute__((noinline)) void vdso_unmapped(void) {
    __asm__ volatile("" : : : "memory");
}

// libc keeps the vDSO's first address, so nothing calls into it once moved.
static int move_vdso(void) {
    unsigned long start = 0;
    unsigned long end = 0;
    if (!vdso_mapping(&start, &end)) {
        return 2;
    }
    size_t size = end - start;
    void *target = mmap(NULL, size, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (target == MAP_FAILED ||
        mremap((void *)start, size, size, MREMAP_MAYMOVE | MREMAP_FIXED, target) != target) {
        return 3;
    }
    vdso_moved(target);
    if (munmap(target, size) != 0) {
        return 4;
    }
    vdso_unmapped();
    return 0;
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "clock";
    if (strcmp(mode, "time") == 0) {
        return vdso_time(UNMAPPED) == vdso_depth ? 0 : 1;
    }
    if (strcmp(mode, "calls") == 0) {
        long now = vdso_time(&vdso_time_value);
        long clock = vdso_clock(&vdso_clock_value);
        return now > vdso_depth && clock == vdso_depth ? 0 : 1;
    }
    if (strcmp(mode, "move") == 0) {
        return move_vdso();
    }
    return vdso_clock(UNMAPPED) == vdso_depth ? 0 : 1;
}
