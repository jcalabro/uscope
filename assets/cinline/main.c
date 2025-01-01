#include <stdio.h>

static inline void InlineFunc() {
    printf("Inline 1\n");
    printf("Inline 2\n");
}

void NotInlineFunc() {
    printf("Not Inline 1\n");
    printf("Not Inline 2\n");
}

int main() {
    NotInlineFunc();
    InlineFunc();
    NotInlineFunc();

    return 0;
}
