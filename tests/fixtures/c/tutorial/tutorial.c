// The program docs/writing-views.md writes views for, which it carries in
// tutorial.views. Each `VIEW:` marker says what its expression shows.

#include <stddef.h>

#include "uscope_views.h"

USCOPE_VIEWS_FILE("tests/fixtures/c/tutorial/tutorial.views");

// A vector of integers: `n` of them at `data`, with room for `cap`.
typedef struct {
    int *data;
    size_t n;
    size_t cap;
} intvec;

// A value of one of three kinds.
enum kind { VAL_INT, VAL_STR, VAL_NIL };

typedef struct {
    enum kind kind;
    union {
        long i;
        struct {
            const char *ptr;
            size_t len;
        } s;
    } u;
} value;

// Tasks on a run queue, linked through the node each embeds.
struct list_head {
    struct list_head *next, *prev;
};

struct task {
    int pid;
    struct list_head run_node;
};

struct run_queue {
    struct list_head tasks;
    size_t nr;
};

__attribute__((noinline)) void barrier(void *fixture) {
    __asm__ volatile("" : : "r"(fixture) : "memory");
}

static void enqueue(struct run_queue *queue, struct task *task) {
    struct list_head *last = queue->tasks.prev;
    task->run_node.prev = last;
    task->run_node.next = &queue->tasks;
    last->next = &task->run_node;
    queue->tasks.prev = &task->run_node;
    queue->nr++;
}

int main(void) {
    int storage[] = {10, 20, 30, 0};
    intvec numbers = {storage, 3, 4}; // VIEW: numbers => len=3 [10, 20, 30]
    intvec broken = {storage, 9, 4};  // VIEW: broken => problem: check `n <= cap` failed
    value count = {VAL_INT, {.i = 42}};                // VIEW: count => 42
    value name = {VAL_STR, {.s = {"uscope", 6}}};      // VIEW: name => "uscope"
    value nothing = {VAL_NIL, {.i = 0}};               // VIEW: nothing => nil
    struct run_queue queue = {{&queue.tasks, &queue.tasks}, 0}; // VIEW: queue => len=2 [7, 8]
    struct task first = {7, {0, 0}};
    struct task second = {8, {0, 0}};
    enqueue(&queue, &first);
    enqueue(&queue, &second);
    barrier(&numbers);
    barrier(&broken);
    barrier(&count);
    barrier(&name);
    barrier(&nothing);
    barrier(&queue);
    return 0;
}
