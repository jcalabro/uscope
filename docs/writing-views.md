# Writing views

A view tells uscope what one of your types stands for: a vector as its
elements, a tagged union as the value it holds, a queue as the tasks on it.
This tutorial writes views for a small C program and a Rust one, both in
uscope's own tests, so everything here runs. `docs/views.md` is the
reference for the language.

## Where views go

A view file is any file that begins with `uscope-views 1`. uscope reads:

- files you name, with `uscope --views FILE`, `views load FILE` in a
  session, or the debug adapter's `viewFiles`;
- every `*.views` file in your project's `.uscope/views`, and in
  `~/.config/uscope/views`;
- views a program carries for its own types, which this tutorial does.

Whichever you choose, `uscope views check PROGRAM` tells you, without
running the program, which view presents each type a view names, and why a
view does not bind.

## A vector

The program keeps integers in a vector of its own:

```c tests/fixtures/c/tutorial/tutorial.c
// A vector of integers: `n` of them at `data`, with room for `cap`.
typedef struct {
    int *data;
    size_t n;
    size_t cap;
} intvec;
```

Printed as stored, a vector shows a pointer and two numbers. Its view says
what they mean:

```uscope-views tests/fixtures/c/tutorial/tutorial.views
# A vector of integers: `n` of them at `data`, with room for `cap`.
view c intvec {
    check n <= cap
    show sequence(n) for i in range(n) => data[i]
    field capacity = cap
}
```

- `view c intvec` names the type: its language, then a pattern of its
  name.
- `check n <= cap` states what must hold of a vector that is whole. One
  that breaks it shows as stored, with the check that failed, never as a
  plausible vector of nine elements.
- `show sequence(n) for i in range(n) => data[i]` says the vector is `n`
  elements, the `i`th at `data[i]`. A debug adapter pages through them,
  each reached directly.
- `field capacity = cap` adds a child beside the elements.

```text
(intvec) numbers = len=3 [10, 20, 30]
(intvec) broken = {data = 0x00007fffffff74b0, n = 9, cap = 4} <view tutorial.views[0]:4 `c intvec`: check `n <= cap` failed: `n` is 9, `cap` is 4>
```

`numbers[1]` and `len(numbers)` now work in any expression, such as a
breakpoint's condition: `break push if len(numbers) > 100`.

## A tagged union

A value holds one of three kinds of thing, which its `kind` says:

```c tests/fixtures/c/tutorial/tutorial.c
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
```

A `match` chooses what to show by the kind:

```uscope-views tests/fixtures/c/tutorial/tutorial.views
# A tagged union: what it holds depends on its kind.
view c value {
    show match kind {
        VAL_INT => value(u.i)
        VAL_STR => text(u.s.ptr, u.s.len)
        VAL_NIL => empty("nil")
    }
}
```

`value(u.i)` shows the integer as the value, `text` shows the characters
the string points to, and `empty` shows a value that holds nothing. A kind
no arm names is not guessed at: the value shows as stored, saying what its
kind was.

```text
(value) count = 42
(value) name = "uscope"
(value) nothing = nil
```

## An intrusive list

Tasks wait on a run queue through a node each task embeds, as the Linux
kernel's lists do:

```c tests/fixtures/c/tutorial/tutorial.c
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
```

The queue's view walks the nodes and finds each task from its node:

```uscope-views tests/fixtures/c/tutorial/tutorial.views
# A run queue: its tasks, each found from the node that links it.
view c run_queue {
    show sequence(nr) for n in list(&tasks, p => p->next) if n != &tasks
        => container_of(n, task, run_node)->pid
}
```

- `list(&tasks, p => p->next)` is the nodes from the queue's own, each
  node's `next` after it, until it comes back around.
- `if n != &tasks` leaves out the queue's own node, which is no task's.
- `container_of(n, task, run_node)` is the task whose `run_node` `n` is.
  uscope checks that `n` points to a `list_head`, the type of `run_node`.

```text
(run_queue) queue = len=2 [7, 8]
```

A list that leads back to a node it has already shown is a cycle, which
shows as a problem rather than a list that never ends, and a list that
holds fewer nodes than `nr` says is one too.

## Carrying views in the program

This program carries its views, so whoever debugs it gets them:

```c tests/fixtures/c/tutorial/tutorial.c
#include "uscope_views.h"

USCOPE_VIEWS_FILE("tests/fixtures/c/tutorial/tutorial.views");
```

`uscope_views.h` is in uscope's `sdk/c`. The path is read when the program
is built, relative to where the compiler runs. The views go in a section
of the debug information that is not loaded when the program runs, and
that `strip --strip-debug` removes. A library's views present only that
library's types, so one library cannot change how another's look.

## A kernel

The program keeps a tree whose nodes keep their children in a list:

```c tests/fixtures/c/tutorial/tutorial.c
// A tree whose nodes keep their children in a list.
typedef struct node {
    int value;
    struct node *child;
    struct node *sibling;
} node;

struct tree {
    node *root;
    size_t count;
};
```

No generator walks it, each node before its children: `list` follows one
link and `inorder` two, and this walk needs a stack of the siblings still
to come. A kernel walks it instead. A kernel is a small function, compiled
to WebAssembly, that reads the program's memory and yields items, here each
node's address:

```c tests/fixtures/c/tutorial/tree.c
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
```

- `uscope_kernel.h`, in uscope's `sdk/c`, declares what a kernel may call:
  `uscope_read` and the loads built on it, which read the program's
  memory, and `uscope_yield`, which yields an item of words and says
  whether uscope wants another. A kernel can do nothing else: it cannot
  write memory, call the program, or perform I/O, and its reads and work
  are charged to the inspection's budget, as a view's are.
- `run` takes the view's arguments, here the root and where a node keeps
  its links, and returns 0 when it is done. Anything else is a failure,
  which shows the value as stored, with the number `run` returned.
- `zig cc --target=wasm32-freestanding -Os -nostdlib -Wl,--no-entry
  -Isdk/c tree.c -o tree.wasm` builds it, as does `clang --target=wasm32`
  with the same options.

The view calls the kernel by name, and presents each node it yields:

```uscope-views tests/fixtures/c/tutorial/tutorial.views
# A tree whose nodes keep their children in a list. No generator walks it,
# since that takes a stack, so the tree kernel the program carries does,
# yielding each node before its children.
view c tree {
    show sequence(count) for at in kernel("tree", root, offsetof(node, child), offsetof(node, sibling))
        => ((node *)at)->value
}
```

```text
(tree) family = len=5 [1, 2, 3, 4, 5]
```

The program carries the kernel with its views, and its source with it, so
that `uscope views check` shows the kernel as the source it is built from:

```c tests/fixtures/c/tutorial/tutorial.c
USCOPE_KERNEL("tree", "tests/fixtures/c/tutorial/tree.c", "tutorial-tree.wasm");
```

A view file's kernel can instead be beside it, as `tree.wasm`. Zig kernels
use `sdk/zig/uscope_kernel.zig`, and Rust ones the `uscope-views` crate's
`kernel` module, as uscope's test program in
`tests/fixtures/rust/embedded-views` does for the same tree.

A run of a kernel depends only on its arguments and the memory it reads,
so it can be recorded and run again without the program. `views record
runs.txt family` records the runs that present `family`, and `uscope views
replay runs.txt --kernel tree.wasm` runs a new build of the kernel on them,
saying where it first does something else.

## A Rust newtype

In Rust, the `uscope-views` crate in uscope's `sdk/rust` does the same:

```rust tests/fixtures/rust/embedded-views/main.rs
uscope_views::uscope_views_file!("tests/fixtures/rust/embedded-views/main.views");

/// Tags, which their view presents as the names they hold.
pub struct Tags {
    names: Vec<&'static str>,
}

/// A temperature in degrees Celsius.
pub struct Celsius(f64);
```

```uscope-views tests/fixtures/rust/embedded-views/main.views
# Tags, as the names they hold.
view rust embedded_views::Tags {
    show value(names)
}

# A temperature, a newtype around its degrees.
view rust embedded_views::Celsius {
    show value(self.0)
    summary "{self.0}°C"
}
```

`value(names)` shows the tags as the `Vec` they hold, which the built-in
view of `Vec` presents, so `Tags` has elements of its own. `summary`
writes the one line a value shows as.

```text
(Tags) tags = len=2 ["red", "green"]
(Celsius) temperature = 21.5°C
```

A view names a Rust type by its path from its crate, here
`embedded_views`, and `**` stands for any modules between, as in
`alloc::**::Rc<T, _>`.

## When a view does not bind

Before a view runs, uscope binds it against the type: every member it
names must exist, and every expression must make sense for the type's
members. A view that does not bind is skipped, and the next one whose
pattern names the type is tried. `info view EXPR` in a session, and
`uscope views explain PROGRAM TYPE` without one, say which view presents a
value and why each one before it did not bind:

```text
$ uscope views explain build/test-programs/tutorial intvec
intvec in /…/build/test-programs/tutorial
  presented by tutorial.views[0]:4 `c intvec`
  views tried, in order:
    tutorial.views[0]:4 `c intvec`: binds
```

`uscope views check PROGRAM` does so for every type a view names, and fails
when a view you gave it binds nothing it names, or presents no type at all,
so it can guard a project's views in its own tests.

## Adding to a view

`extend` adds fields, `hide`s, and `format`s to whatever view presents a
type, without copying it, such as a field of your own on the built-in view
of `std::vector`, or hexadecimal for a member of a type with no view:

```text
extend c intvec {
    field room = cap - n
}
extend c packet {
    format flags as hex
}
```

## Contributing a built-in view

uscope's own views are the files in `views/`, one per library. Contributing
one is two changes: the view, and a line in that language's `containers`
fixture, under `tests/fixtures`, that declares a value of the type and says
what it must show:

```cpp
std::vector<int> ints = {1, 2, 3};            // VIEW: ints => len=3 [1, 2, 3]
```

The tests check every marker in every build of the fixture, and that every
built-in view presents some marked value.
