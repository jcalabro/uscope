// Strings in the forms C programs keep them, for text summaries: pointers,
// arrays with and without a terminator, escapes, text longer than a summary
// holds, and text that runs into unreadable memory.
#define _DEFAULT_SOURCE

#include <stddef.h>
#include <string.h>
#include <sys/mman.h>

const char *global_greeting = "global text";

__attribute__((noinline)) int strings_target(const char *greeting, const char *escaped,
                                             const char *long_text, const char *edge) {
    char buffer[16] = "abc";
    char unterminated[4] = {'w', 'x', 'y', 'z'};
    const char *null_text = NULL;
    const char *invalid = (const char *)1;
    unsigned char bytes[3] = {0x41, 0xff, 0};
    __asm__ volatile("" : : "g"(greeting), "g"(escaped), "g"(long_text), "g"(edge),
                     "g"(buffer), "g"(unterminated), "g"(null_text), "g"(invalid),
                     "g"(bytes) : "memory");
    return (int)strlen(greeting) + buffer[0]; // strings stop here
}

int main(void) {
    static char long_text[600];
    memset(long_text, 'x', sizeof long_text - 1);
    // Text that fills the end of a readable page, with the next page gone.
    char *pages = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (pages == MAP_FAILED || munmap(pages + 4096, 4096) != 0) {
        return 100;
    }
    memset(pages, 'e', 4096);
    char *edge = pages + 4096 - 5;
    return strings_target("hello, world", "tab\there \"quoted\" \\ \xc3\xa9\x80", long_text,
                          edge) == 12 + 'a'
               ? 0
               : 1;
}
