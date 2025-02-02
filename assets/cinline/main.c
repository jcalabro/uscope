#include <stdio.h>

static inline void InlineFunc(int inline_param) {
    printf("Inline 1: %d\n", inline_param);
    printf("Inline 2: %d\n", inline_param);
}

void NotInlineFunc(char not_inline_param) {
    printf("Not Inline 1: %d\n", not_inline_param);
    printf("Not Inline 2: %d\n", not_inline_param);
}

int main() {
    NotInlineFunc(1);
    InlineFunc(2);
    NotInlineFunc(3);

    return 0;
}
