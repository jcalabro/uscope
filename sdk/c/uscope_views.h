/* Views for uscope, carried in a program's debug information.
 *
 * USCOPE_VIEWS_FILE("views/app.views"); at file scope embeds a view file in
 * the .debug_uscope_views section of the object that expands it. uscope
 * presents the module's own types with those views, and no other module's.
 * USCOPE_KERNEL("tree", "kernels/tree.c", "build/tree.wasm"); embeds a
 * kernel those views may call by its name, with its source, so that it is
 * reviewed as that; uscope_kernel.h says how to write one.
 *
 * The section is not loaded when the program runs, and strip --strip-debug
 * removes it with the rest of the debug information. The paths are the
 * assembler's: relative to the directory the compiler runs in, or to a
 * directory given with -Wa,-I.
 *
 * The views language is described in uscope's docs/views.md, and how to
 * write views in docs/writing-views.md.
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */
#ifndef USCOPE_VIEWS_H
#define USCOPE_VIEWS_H

/* A record: kind 1 (view text), format 1, its length, then the file. */
#define USCOPE_VIEWS_FILE(path)                                        \
    __asm__(".pushsection .debug_uscope_views,\"\",@progbits\n"       \
            ".byte 1, 1\n"                                             \
            ".long 8f - 7f\n"                                          \
            "7:\n"                                                     \
            ".incbin \"" path "\"\n"                                   \
            "8:\n"                                                     \
            ".popsection\n")

/* A record: kind 2 (kernel), format 1, its length, then the name's length
 * and the name, the source's length and the source, and the module. */
#define USCOPE_KERNEL(name, source, module)                            \
    __asm__(".pushsection .debug_uscope_views,\"\",@progbits\n"       \
            ".byte 2, 1\n"                                             \
            ".long 8f - 7f\n"                                          \
            "7:\n"                                                     \
            ".short 4f - 3f\n"                                         \
            "3:\n"                                                     \
            ".ascii \"" name "\"\n"                                    \
            "4:\n"                                                     \
            ".long 6f - 5f\n"                                          \
            "5:\n"                                                     \
            ".incbin \"" source "\"\n"                                 \
            "6:\n"                                                     \
            ".incbin \"" module "\"\n"                                 \
            "8:\n"                                                     \
            ".popsection\n")

#endif
