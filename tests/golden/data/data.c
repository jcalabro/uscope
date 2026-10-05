// Records, arrays, and pointers in one thread: a table of records the
// program fills, then visits through pointers into it. Runs as many rounds
// as its argument says, or two.

#include "../rt/rt.h"

typedef unsigned char u8;
typedef unsigned short u16;
typedef int i32;

struct point {
    i32 x;
    i32 y;
};

struct item {
    u8 tag;
    u16 count;
    struct point at;
    u64 weight;
};

enum {
    ITEMS = 4,
};

struct item items[ITEMS];

__attribute__((noinline)) u64 visit(struct item *item, u64 index) {
    u64 weight = item->weight + item->count; // MARK: index < 4 // EXPECT: item == &items[index] && item->tag == index + 1 && (*item).count == index * 300 && (u8)item->count == index * 300 % 256 && items[index].at.x == -(i32)index && (*(items + index)).at.y == index * index
    item->weight = weight;
    return weight * index + (u64)(item->at.x + item->at.y);
}

int main(int argc, char **argv) {
    u64 rounds = argc > 1 ? rt_parse_u64(argv[1]) : 2;
    for (u64 index = 0; index < ITEMS; index++) {
        items[index].tag = (u8)(index + 1);
        items[index].count = (u16)(index * 300);
        items[index].at.x = -(i32)index;
        items[index].at.y = (i32)(index * index);
        items[index].weight = index;
    }
    u64 total = 0;
    for (u64 round = 0; round < rounds; round++) {
        for (u64 index = 0; index < ITEMS; index++) {
            total += visit(&items[index], index);
        }
    }
    rt_print("total ");
    rt_print_u64(total);
    rt_print("\n");
    return (int)(total % 100);
}
