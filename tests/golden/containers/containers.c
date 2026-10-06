// Hand-rolled containers, which containers.views presents: a vector, a
// singly linked list, and an open-addressed table. Every odd round leaves
// the list cyclic, its last node leading back to its middle, and claiming
// two more nodes than it links, until the next round links it again. Runs
// as many rounds as its argument says, or four.

#include "../rt/rt.h"

typedef int i32;

struct intvec {
    i32 *data;
    u64 n;
    u64 cap;
};

struct node {
    i32 value;
    struct node *next;
};

struct list {
    struct node *head;
    u64 count;
};

struct slot {
    i32 used;
    i32 key;
    i32 value;
};

struct table {
    struct slot *slots;
    u64 cap;
    u64 n;
};

enum {
    CAPACITY = 6,
    NODES = 5,
    SLOTS = 8,
};

i32 storage[CAPACITY];
struct intvec vec = {storage, 0, CAPACITY};
struct node nodes[NODES];
struct list list;
struct slot slots[SLOTS];
struct table table = {slots, SLOTS, 0};

__attribute__((noinline)) u64 visit(u64 round) {
    u64 sum = vec.n + list.count + table.n; // MARK: round < 100 // EXPECT: len(vec) == vec.n && len(table) == table.n && len(list) == list.count
    return sum + round;
}

static void push(i32 value) {
    if (vec.n < vec.cap) {
        vec.data[vec.n] = value;
        vec.n++;
    }
}

static void insert(i32 key, i32 value) {
    u64 index = (u64)key % SLOTS;
    while (slots[index].used && slots[index].key != key) {
        index = (index + 1) % SLOTS;
    }
    if (!slots[index].used) {
        table.n++;
    }
    slots[index].used = 1;
    slots[index].key = key;
    slots[index].value = value;
}

// Links the first `count` nodes in order.
static void link(u64 count) {
    for (u64 index = 0; index < count; index++) {
        nodes[index].value = (i32)(index * 10 + count);
        nodes[index].next = index + 1 < count ? &nodes[index + 1] : 0;
    }
    list.head = count ? &nodes[0] : 0;
    list.count = count;
}

int main(int argc, char **argv) {
    u64 rounds = argc > 1 ? rt_parse_u64(argv[1]) : 4;
    u64 total = 0;
    for (u64 round = 0; round < rounds; round++) {
        push((i32)(round * 3));
        insert((i32)(round * 5), (i32)round);
        u64 count = round % NODES + 1;
        link(count);
        if (round % 2 == 1 && count > 1) {
            nodes[count - 1].next = &nodes[count / 2];
            list.count = count + 2;
        }
        total += visit(round);
    }
    rt_print("total ");
    rt_print_u64(total);
    rt_print("\n");
    return (int)(total % 100);
}
