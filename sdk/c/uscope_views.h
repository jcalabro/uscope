/* Views for uscope, carried in a program's debug information.
 *
 * USCOPE_VIEWS_FILE("views/app.views"); at file scope embeds a view file in
 * the .debug_uscope_views section of the object that expands it. uscope
 * presents the module's own types with those views, and no other module's.
 *
 * The section is not loaded when the program runs, and strip --strip-debug
 * removes it with the rest of the debug information. The path is the
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

#endif
