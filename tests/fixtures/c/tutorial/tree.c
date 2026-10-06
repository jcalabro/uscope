// The values of a tree whose nodes keep their children in a list, each
// node before its children, for the view of `struct tree` in tutorial.views.
// Its arguments are the root and where a node keeps its first child and its
// next sibling; it yields each node's address.

#include "uscope_kernel.h"

// The deepest tree it walks: a node waits here for each ancestor whose next
// sibling is still to come.
#define MAX_DEPTH 64

USCOPE_KERNEL_EXPORT int32_t run(const uint64_t *arguments, int32_t count) {
    if (count != 3)
        return 1;
    uint64_t child = arguments[1];
    uint64_t sibling = arguments[2];
    uint64_t waiting[MAX_DEPTH];
    int depth = 0;
    uint64_t node = arguments[0];
    while (node != 0) {
        if (!uscope_yield(&node, 1))
            return 0;
        uint64_t first = uscope_load_u64(node + child);
        uint64_t next = uscope_load_u64(node + sibling);
        if (first == 0) {
            node = next;
        } else {
            if (next != 0) {
                if (depth == MAX_DEPTH)
                    return 2;
                waiting[depth++] = next;
            }
            node = first;
        }
        if (node == 0 && depth > 0)
            node = waiting[--depth];
    }
    return 0;
}
