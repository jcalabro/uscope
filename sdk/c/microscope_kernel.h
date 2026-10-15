/* Kernels for microscope, in C.
 *
 * A kernel is a WebAssembly function a view calls to walk a structure that
 * is an algorithm more than a layout, as microscope's docs/views.md describes.
 * It exports run, which takes the view's arguments as 64-bit words, and
 * yields items of words, such as addresses, which the view presents. It
 * reads the program's memory only through microscope_read:
 *
 *     #include "microscope_kernel.h"
 *
 *     MICROSCOPE_KERNEL_EXPORT int32_t run(const uint64_t *arguments,
 *                                          int32_t count) {
 *         if (count != 1)
 *             return 1;
 *         for (uint64_t node = arguments[0]; node != 0;
 *              node = microscope_load_u64(node))
 *             if (!microscope_yield(&node, 1))
 *                 return 0;
 *         return 0;
 *     }
 *
 * Build it with zig cc --target=wasm32-freestanding -Os -nostdlib
 * -Wl,--no-entry, or clang --target=wasm32 with the same flags. run returns
 * 0 when it is done, and anything else says it failed.
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */
#ifndef MICROSCOPE_KERNEL_H
#define MICROSCOPE_KERNEL_H

#include <stdint.h>

#define MICROSCOPE_KERNEL_IMPORT(name) \
    __attribute__((import_module("microscope_kernel_v1"), import_name(name)))

/* Exports a kernel's entry point as run. */
#define MICROSCOPE_KERNEL_EXPORT __attribute__((export_name("run")))

/* Fills buffer with length bytes of the program's memory at address, and
 * returns length. A read microscope cannot make ends the run, so a kernel never
 * sees one fail; negative results are reserved. */
MICROSCOPE_KERNEL_IMPORT("read")
int32_t microscope_read(uint64_t address, void *buffer, int32_t length);

/* Yields an item of count words, and returns whether microscope wants another.
 * microscope may also stop running the kernel at any call. */
MICROSCOPE_KERNEL_IMPORT("yield")
int32_t microscope_yield(const uint64_t *words, int32_t count);

static inline uint64_t microscope_load_u64(uint64_t address) {
    uint64_t value;
    microscope_read(address, &value, sizeof value);
    return value;
}

static inline uint32_t microscope_load_u32(uint64_t address) {
    uint32_t value;
    microscope_read(address, &value, sizeof value);
    return value;
}

static inline uint16_t microscope_load_u16(uint64_t address) {
    uint16_t value;
    microscope_read(address, &value, sizeof value);
    return value;
}

static inline uint8_t microscope_load_u8(uint64_t address) {
    uint8_t value;
    microscope_read(address, &value, sizeof value);
    return value;
}

#endif
