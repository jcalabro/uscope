/* Built without debug information, so that breakpoints find its functions,
 * and the C library's, by their symbols: `strlen` among them, an indirect
 * function whose implementation the loader chooses for the machine. */
#include <stdio.h>
#include <string.h>

__attribute__((noinline)) size_t measure(const char *text) {
    return strlen(text);
}

int main(int argc, char **argv) {
    printf("%zu\n", measure(argc > 1 ? argv[1] : "symbols"));
    return 0;
}
