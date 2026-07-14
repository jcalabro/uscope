#define _POSIX_C_SOURCE 200809L

#include <dlfcn.h>
#include <linux/limits.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

typedef int32_t (*touch_fn)(void);

volatile int32_t shared_sink;
static volatile int32_t module_collision;

__attribute__((noinline)) static void after_load(void) {
    shared_sink += 1 + module_collision;
}

__attribute__((noinline)) static void after_unload(void) {
    shared_sink += 2;
}

__attribute__((noinline)) static void after_reload(void) {
    shared_sink += 3;
}

static int library_path(char *path, size_t size) {
    ssize_t length = readlink("/proc/self/exe", path, size - 1);
    if (length <= 0 || (size_t)length >= size - 1) {
        return -1;
    }
    path[length] = '\0';
    char *slash = strrchr(path, '/');
    if (slash == NULL) {
        return -1;
    }
    slash[1] = '\0';
    return strncat(path, "libglobals.so", size - strlen(path) - 1) == NULL ? -1 : 0;
}

static int use_library(const char *path, int reload) {
    void *library = dlopen(path, RTLD_NOW | RTLD_LOCAL);
    if (library == NULL) {
        return -1;
    }
    touch_fn touch = (touch_fn)dlsym(library, "dso_touch");
    if (touch == NULL || touch() != 636) {
        return -1;
    }
    if (reload != 0) {
        after_reload();
    } else {
        after_load();
    }
    return dlclose(library);
}

int main(void) {
    char path[PATH_MAX];
    if (library_path(path, sizeof(path)) != 0 || use_library(path, 0) != 0) {
        return 1;
    }
    after_unload();
    if (use_library(path, 1) != 0) {
        return 1;
    }
    return shared_sink == 6 ? 0 : 1;
}
