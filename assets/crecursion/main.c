#include <stdio.h>

#define MAX_DEPTH 5

void Recursive(int *depth) {
    if (*depth >= MAX_DEPTH) {
        return;
    }

    printf("recursion with depth: %d\n", *depth);
    fflush(stdout);

    *depth += 1;
    Recursive(depth);
}

int main() {
    printf("first call:\n");
    int depth1 = 0;
    Recursive(&depth1);

    printf("\nsecond call:\n");
    int depth2 = 0;
    Recursive(&depth2);
}
