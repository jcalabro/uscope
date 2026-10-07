# Views

A view presents a value as the thing it stands for: a `std::string` as its
text, a `Vec` as its elements, a hand-rolled C vector as the integers it
holds. The value as stored stays one step away, as the `[raw]` child,
`print/r`, or `set views off`. uscope builds in views for the C++, Rust, Go,
and Zig standard libraries, and anyone can write views for their own types
in the same language; `docs/writing-views.md` is a tutorial.

A view can only read the stopped program: it cannot write memory, call
functions, or perform I/O. Every read it makes is charged to the
inspection's budget, so a view can never hang the debugger, and a value a
view cannot make sense of shows as stored, with the reason, never as a
plausible wrong value.

Every example on this page runs as one of uscope's tests, against a small
world of C types:

- `intvec`, a vector `{int *data; unsigned long n; unsigned long cap}`, as
  `v` holding 10, 20, and 30 with room for 4, `bad` claiming 9 elements
  in that room, `none` with no storage, `big` holding 0 to 299, and
  `dangling` pointing at unmapped memory;
- `str_t`, text `{char *p; unsigned long len}`, as `s` holding `hello`;
- `tagged`, a tagged union `{int kind; int value}`, as `nothing` of kind 0,
  `something` of kind 1 holding 7, and `strange` of kind 5;
- the Rust `alloc::boxed::Box<i32>`, a wrapper around a pointer, as `b`
  pointing at 42;
- the C++ `app::detail::Pair<int, 3>`, `{int first; int items[3]}`, as `p`
  holding 1, then 2, 3, and 4;
- the C++ `app::Handle<int>`, `{void *cell}`, whose `cell` points at an
  `app::Cell<int>`, `{int value}`, as `handle` holding 5;
- the C++ `app::Either<int, char*>`, `{char *storage; int index}`, which
  holds the argument `index` chooses in `storage`, as `number` holding the
  int 7, `pointer` holding the pointer 0x90000, and `neither` whose index
  is 5;
- `entry`, `{int mode; int color; long elapsed; unsigned short label[4];
  int letter}`, as `item`, with the enumerations `Access` (`NONE`, `READ`,
  `WRITE`, `EXEC`) and `Color` (`RED`, `GREEN`, `BLUE`);
- `run_queue`, `{list_head tasks; unsigned long nr}`, whose `task`s,
  `{int pid; list_head node}`, link through their `node`s in a ring, as
  `queue` holding the tasks 10 and 20;
- `handle_t`, `{int index}` into the global array `arena` of 5, 6, 7, and
  8, as `slot` with index 2;
- `list`, a linked list `{node *head; unsigned long count}` of `node {int
  value; node *next}`, as `three` holding 1, 2, and 3, `circle` holding 4,
  5, and 6 in a ring, `looped` whose third node leads back to its second,
  and `short` claiming 5 elements of its 3;
- `tree`, a binary tree `{tnode *root; unsigned long count}` of `tnode {int
  key; int value; tnode *left; tnode *right}`, as `balanced` holding the
  keys 1, 2, and 3 with the values 10, 20, and 30, `tangled`, whose
  rightmost node leads back to its root, and `deep`, a chain of 200 left
  children;
- `table`, an open-addressed table `{slot *slots; unsigned long cap;
  unsigned long n}` of `slot {int used; int key; int value}`, as `sparse`
  using two of its four slots, for the keys 5 and 6;
- `chained`, buckets of lists `{node **buckets; unsigned long nbuckets;
  unsigned long n}`, as `buckets` holding 1 and 2 in its first bucket and 3
  in its third.

An example block holds a view file, then `---`, then rows that read
`value => outcome`. An outcome is the summary the value is presented as;
`children:` and the children it expands to, a map's entries as `key:
value`; `problem:` and why a view that binds refuses the value; `unbound:`
and why no view binds; or `error:` and why the file itself is refused.

## Files

A view file begins with the language's version, `uscope-views 1`, and holds
views. `#` begins a comment. A view names the language of the types it
applies to and a pattern of those types, and its body says how to present
them:

```uscope-view-example
uscope-views 1

# A vector of integers, as its elements.
view c intvec {
    check n <= cap
    show sequence(n) for i in range(n) => data[i]
    field capacity = cap
}
---
v => len=3 [10, 20, 30]
none => len=0 []
big => len=300 [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, …]
v => children: [0] = 10, [1] = 20, [2] = 30, capacity = 4, [raw]
```

A value expands to its elements, then the view's fields, then `[raw]`, the
value as stored, whose children are its members.

The language is `c`, `c++`, `rust`, `go`, `zig`, or `any`. A file that does
not begin with its version is refused, and a view with an error is skipped
while the rest of its file is read:

```uscope-view-example
uscope-views 1
view kotlin intvec {
    show empty("")
}
view c intvec {
    show empty("an intvec")
}
---
file => error: 2:6: expected a language
v => an intvec
```

## Patterns

A pattern is a type's path from its root, its name, and its leading
arguments: `std::vector<T, _>`, `alloc::vec::Vec<T, _>`,
`array_list.Aligned(T, _)`. Paths are separated by `::` or `.`, and
arguments are in `<>`, `()`, or `[]`, whichever the language writes. A
pattern names what a type is, not how its compiler spelled it:
`pair<int const, …>` and `pair<const int, …>` are one type, and inline
namespaces such as libc++'s `std::__1` may be spelled or left out.

- `**` in a path matches any run of segments, so `alloc::**::Box<T>` keeps
  matching when the standard library moves a type between modules.
- `_` matches any argument, and trailing arguments may be left out.
- A capitalized name captures an argument: a type, which the view's
  expressions may name, or a value, which they may use as a number.
- An integer matches an argument of that value, and a pattern matches a
  type argument.
- A pattern that reaches a C++ parameter pack spells all of it:
  `std::tuple<A, B>` names only tuples of two, and `std::tuple`, with no
  arguments, every tuple.
- In Go, `map<K, V>`, `chan<T>`, and `interface` name every map, channel,
  and interface by the kind Go's debug information gives the type, whatever
  its name: `go map<K, V>` presents `map[string]int` and a `type Counts
  map[string]int` alike.

```uscope-view-example
uscope-views 1
view c++ app::**::Pair<T, N> {
    show sequence(N) for i in range(N) => *((T*)&items[0] + i)
}
---
p => len=3 [2, 3, 4]
```

```uscope-view-example
uscope-views 1
view c++ detail::Pair<T, N> {
    show empty("")
}
view c++ app::detail::Pair<T, 4> {
    show empty("")
}
---
p => unbound: no view's pattern names the type
```

## Expressions

A view's expressions are uscope's own (`docs/expressions.md`), evaluated
with `self` as the value presented. A member of `self` is named by its own
name, and a view's `let`s and the arguments its pattern captured shadow
members. Nothing the program's frame names is visible, so a view means the
same thing at every stop.

Views may also call:

- `inner(x)`, which steps through wrapper records: while `x` is a record
  with exactly one member of non-zero size, and no base, it is that member.
  Zero-sized markers such as Rust's `PhantomData` do not count. `inner`
  absorbs the wrapper layers a library adds and removes between versions,
  such as the `RawVec`, `Unique`, and `NonNull` around a Rust `Vec`'s
  pointer.
- `offsetof(TYPE, member)`, where one of a record's own members is, in
  bytes.
- `container_of(PTR, TYPE, member)`, a pointer to the `TYPE` whose own
  `member` `PTR` points to, as intrusive lists find their nodes. `PTR` must
  point to the member's type, or be a `void *`.
- `global(NAME)`, a global of the module whose value the view presents.

```uscope-view-example
uscope-views 1
view rust alloc::**::Box<T> {
    show value(*inner(self))
}
---
b => 42
```

```uscope-view-example
uscope-views 1
view c run_queue {
    show sequence(nr) for n in list(&tasks, p => p->next) if n != &tasks
        => container_of(n, task, node)->pid
}
view c handle_t {
    show value(global(arena)[index])
}
---
queue => len=2 [10, 20]
slot => 7
```

```uscope-view-example
uscope-views 1
view c run_queue {
    show value(container_of(&nr, task, node)->pid)
}
---
queue => unbound: line 3: `container_of(&nr, task, node)->pid`: `&nr`: points to `unsigned long`, not to the type of `node`
```

A member named `or`, `for`, `if`, `else`, or `let`, which end an expression
in a view, is written in backticks.

## Statements

A view's body holds statements, one to a line. A statement continues onto
the next line unless that line begins another statement or ends the view.

- `let NAME = EXPR or EXPR …` names a value, computed at most once each time
  the view presents a value. The first alternative that binds is used, so
  one view can describe a layout that changed between versions.
- `type NAME = TYPE or TYPE …` names a type: a type the program defines, an
  argument the pattern captured, `typeof(EXPR)`, `arg(TYPE, N)`, the type's
  `N`th argument, counted from 0, or `TYPE.Name`, a type declared inside
  another, as Zig's `typeof(self).Header`. A type with arguments, such as
  `app::Cell<T>`, is the one type whose arguments are those, found by what
  they are rather than how a compiler spelled them, so its arguments may be
  types the pattern captured or the view names.
- `check EXPR` states an invariant. A value that breaks one shows as
  stored, with the check that failed. A check of conditions joined by `&&`
  is one check for each.
- `field NAME = EXPR` adds a named child.
- `summary "TEXT {EXPR} TEXT"` overrides the summary; `{EXPR}` is replaced
  by its value's summary, and `\{` and `\}` are braces.
- `show SHAPE` says what the value is. A view shows at most once; one
  that does not presents a record's members, with its bases as members
  named by their types, or any other value as itself.
- `hide NAME, …` leaves members and fields out of the children and the
  summary; `[raw]` still has them.
- `format NAME, … as FORMAT` writes members and fields another way:
  `hex`; `char`; `bytes`, a value's bytes in memory; `utf16`, an array of
  16-bit units as text; `flags(ENUM)`, the enumerators whose bits an
  integer sets; `enum(ENUM)`, the enumerator it equals; or `duration(UNIT)`,
  a count of `ns`, `us`, `ms`, or `s`. `self` is the value a `value` shape
  presents. A format that does not suit what it names keeps the view from
  binding.

```uscope-view-example
uscope-views 1
view c intvec {
    let size = length or n
    check size <= cap && cap < 1000
    show sequence(size) for i in range(size) => data[i]
    field room = cap - size
}
---
v => children: [0] = 10, [1] = 20, [2] = 30, room = 1, [raw]
bad => problem: check `size <= cap` failed: `size` is 9, `cap` is 4
```

```uscope-view-example
uscope-views 1
view c++ app::Handle<T> {
    type Cell = app::Cell<T>
    show value(((Cell*)cell)->value)
    field offset = offsetof(Cell, value)
}
---
handle => 5
handle => children: offset = 0, [raw]
```

```uscope-view-example
uscope-views 1
view c tagged {
    hide value
    format kind as hex
}
view c entry {
    format mode as flags(Access)
    format color as enum(Color)
    format elapsed as duration(ms)
    format label as utf16
    format letter as char
}
---
something => {kind: 0x1}
something => children: kind = 0x1, [raw]
item => {mode: READ | WRITE, color: BLUE, elapsed: 1.5s, label: "hi", letter: 'A'}
```

```uscope-view-example
uscope-views 1
view c entry {
    hide mode, color, elapsed, letter
    format label as bytes
}
view c tagged {
    format kind as utf16
}
---
item => {label: 68 00 69 00 00 00 00 00}
something => unbound: line 7: `format kind`: the format writes an array of 16-bit units
```

## Shapes

- `text(PTR)` and `text(PTR, LEN)` are text: the characters a pointer to
  one-byte characters points to, up to a NUL or `LEN` of them. `PTR` may
  also be an array or slice of one-byte elements, whose length is `LEN`'s
  default.
- `value(EXPR)` presents the value as another value, as a box presents what
  it holds.
- `empty("TEXT")` is a value that holds nothing, summarized as `TEXT`.
- `sequence(COUNT) GENERATORS => ELEMENT` is a sequence of `COUNT`
  elements, one for each value the generators make. `COUNT` may be `_` to
  leave the count to the generators.
- `map(COUNT) GENERATORS => KEY : VALUE` is a map of `COUNT` entries, each
  a key and a value.
- `record { NAME = EXPR, … }` is a record of the members it names, which
  are its children before the view's fields. Members named by their
  positions, from 0, make it a tuple.
- `dynamic(PTR, TYPE)` is what `PTR` points to, as a value of `TYPE`,
  presented as any value of that type is. `TYPE` may be `arg(TYPE, EXPR)`,
  the type's argument at a position the program's data holds, as a
  `std::variant`'s index does; a position that names no type is a problem.
- `if COND { SHAPE } else { SHAPE }` chooses a shape.
- `match EXPR { VALUE => SHAPE, … _ => SHAPE }` chooses the shape of the
  first arm whose value `EXPR` equals, or of `_`; a value no arm names is a
  problem.

`if` and `match` may also begin a statement of their own.

```uscope-view-example
uscope-views 1
view c str_t {
    show text(p, len)
}
view c tagged {
    show if kind == 0 { empty("None") } else { value(value) }
}
---
s => "hello"
nothing => None
something => 7
```

```uscope-view-example
uscope-views 1
view c tagged {
    show if kind == 0 {
        empty("None")
    } else {
        value(value)
    }
    summary "tagged {kind}: {value}"
}
---
nothing => tagged 0: 0
something => tagged 1: 7
```

```uscope-view-example
uscope-views 1
view c tagged {
    show match kind {
        0 => empty("None")
        1 => value(value)
    }
}
---
nothing => None
something => 7
strange => problem: `kind` is 5, which no arm of the match names
```

```uscope-view-example
uscope-views 1
view c++ app::detail::Pair<T, N> {
    show record { first = first, rest = items[0] }
}
view c tagged {
    show record { 0 = kind, 1 = value }
}
---
p => {first: 1, rest: 2}
p => children: first = 1, rest = 2, [raw]
something => (1, 7)
```

```uscope-view-example
uscope-views 1
view c++ app::Either<_, _> {
    show dynamic(&storage, arg(typeof(self), index))
}
---
number => 7
pointer => 0x90000
neither => problem: the type has no type argument 5
```

## Generators

A sequence's or map's elements come from generators, each `for NAME in
GENERATOR`, which may nest: an inner generator runs once for each value of
the one around it.

- `range(N)` is 0, 1, …, `N` - 1. A sequence of one `range` reaches each
  element directly, so reading one costs the same wherever it is.
- `list(HEAD, P => NEXT)` is a linked list's nodes: `HEAD`, a pointer, then
  each node's `NEXT`, written with the node as `P`. It ends at a null
  pointer, or at `HEAD` again, as a ring ends.
- `inorder(ROOT, P => LEFT, P => RIGHT)` is a binary tree's nodes, each
  after its left subtree and before its right, a null pointer being an
  empty tree.
- `kernel("NAME", ARG, …)` is the items a kernel yields (see Kernels).

After a generator, `if COND` keeps only the values for which `COND` holds,
and `let NAME = EXPR` names a value computed once for each value, which the
rest of the view's generators, filters, and element may use.

```uscope-view-example
uscope-views 1
view c list {
    show sequence(count) for x in list(head, n => n->next) => x->value
}
view c tree {
    show map(count) for x in inorder(root, n => n->left, n => n->right) => x->key : x->value
}
---
three => len=3 [1, 2, 3]
circle => len=3 [4, 5, 6]
balanced => len=3 {1: 10, 2: 20, 3: 30}
balanced => children: 1: 10, 2: 20, 3: 30, [raw]
```

```uscope-view-example
uscope-views 1
view c table {
    show map(n) for i in range(cap) let slot = slots[i] if slot.used != 0 => slot.key : slot.value
}
view c chained {
    show sequence(n) for b in range(nbuckets) for x in list(buckets[b], p => p->next) => x->value
}
---
sparse => len=2 {5: 50, 6: 60}
buckets => len=3 [1, 2, 3]
```

A sequence with a count generates exactly that many elements, and
generators that end before it are a problem. Without one, the generators are
counted as far as the inspection's budget allows; a count the budget cut
short is shown as at least that many, as `len>=40`.

```uscope-view-example
uscope-views 1
view c list {
    show sequence(_) for x in list(head, n => n->next) => x->value
}
---
three => len=3 [1, 2, 3]
short => len=3 [1, 2, 3]
```

A linked structure never shows a node twice as if it were two elements. A
node that leads back to one already visited is a cycle, and a tree deeper
than any a library builds is refused; either is a problem, as is a sequence
without a count that passes its limit.

```uscope-view-example
uscope-views 1
view c list {
    show sequence(count) for x in list(head, n => n->next) => x->value
}
view c tree {
    show map(count) for x in inorder(root, n => n->left, n => n->right) => x->key : x->value
}
---
looped => problem: cycle at element 3: it leads back to a node already visited
short => problem: the view declares 5 elements and generates 3
tangled => problem: cycle at element 3: it leads back to a node already visited
deep => problem: the tree is deeper than 128 levels
```

## Kernels

Some structures are algorithms more than layouts. A Rust `BTreeMap` keeps
up to eleven entries in each node, and its entries lie in order within and
between its nodes, which no generator walks. For those, a generator calls a
**kernel**: a small WebAssembly function that reads memory and yields
items. Types, layout, and presentation stay in the view, as in the
built-in view of a `BTreeMap`:

```text
view rust alloc::collections::btree::map::BTreeMap<K, V, _> {
    type Leaf = alloc::collections::btree::node::LeafNode<K, V>
    type Internal = alloc::collections::btree::node::InternalNode<K, V>
    let tree = root.Some.0
    let node = length == 0 ? 0 : inner(tree.node) as usize
    let height = length == 0 ? 0 : tree.height
    show map(length) for key, value in kernel("rust-btree", node, height, offsetof(Leaf, len), offsetof(Leaf, keys), offsetof(Leaf, vals), sizeof(K), sizeof(V), offsetof(Internal, edges), 11)
        => *(key as *K) : *(value as *V)
}
```

- `kernel("NAME", ARG, …)` runs the kernel `NAME`, each argument a 64-bit
  word: an integer in two's complement, a pointer's address, or a truth
  value as 0 or 1.
- Each item the kernel yields is a word for each variable its clause
  names, and each variable is that word, an integer, which the view casts
  to the pointer it is.
- A kernel can only compute. It may import nothing but uscope's `read` and
  `yield`, so a module that imports anything else, starts itself, or uses
  floats or SIMD is refused. Its reads and its work are charged to the
  inspection's budget like the view's own.
- A kernel that traps, returns a failure, or yields the wrong number of
  words makes the value's presentation a problem.

```uscope-view-example
uscope-views 1
view c list {
    show sequence(count) for at in kernel("rust-btree", head) => at
}
view c tree {
    show sequence(count) for at in kernel("rust-btree", *root) => at
}
view c table {
    show sequence(n) for at in kernel("nowhere", slots) => at
}
---
three => problem: kernel `rust-btree` failed: it returned 1
balanced => unbound: `*root`: a kernel's arguments are integers, pointers, and truth values
sparse => unbound: `kernel("nowhere")`: no kernel has that name
```

A view calls the kernels of its own source before the built-in ones:

- uscope builds in `rust-btree`, from `views/kernels/rust-btree.zig`;
- a view file's kernels are `NAME.wasm` files beside it, loaded with it;
- a module's own views call the kernels the module carries (see Where views
  come from).

`views check` lists each kernel loaded for the session or carried by a
module with its source, so a kernel is reviewed as its source rather than
trusted as a module.

A kernel is a core WebAssembly module. It imports at most `read(address:
i64, buffer: i32, length: i32) -> i32`, which fills its buffer from the
program's memory, and `yield(words: i32, count: i32) -> i32`, which yields
an item and returns whether uscope wants another, both from the module
`uscope_kernel_v1`. It exports its `memory` and `run(arguments: i32, count:
i32) -> i32`, which uscope calls with the arguments in memory it adds after
the kernel's own, and which returns 0 once it has yielded every item. A
read that uscope cannot make ends the run, as any read a view makes does.
`sdk/c/uscope_kernel.h`, `sdk/zig/uscope_kernel.zig`, and the
`uscope-views` crate's `kernel` module write the rest.

A run is a pure function of its arguments and the bytes its reads return,
so a recording of them replays it without the program. `views record FILE
EXPR` presents a value and its first page of children and writes each
kernel run that took to `FILE`, and `uscope views replay FILE` runs the
kernels again, the built-in one each names or the module given with
`--kernel FILE.wasm`, and fails unless each does what it did.

## Choosing a view

Each view whose pattern names a value's type is bound against the type, in
order, before anything runs: every member it names must exist, every type
must resolve, and every expression must type-check. The first view that
binds presents the type's values; one that does not bind is skipped, and
`info view EXPR` says why. Supporting two layouts of a library is two
views, or one view with `or` alternatives, and neither needs to know a
library's version, because the program's debug information says which
layout it has.

```uscope-view-example
uscope-views 1
view c intvec {
    show sequence(count) for i in range(count) => data[i]
}
---
v => unbound: `count` is neither a member of `intvec` nor a name the view declares
```

A view that binds can still refuse a value: when a check fails, when the
memory it needs cannot be read, or when its count disagrees with its range.
The value then shows as stored, with the reason. An element the program
cannot provide is said to be unavailable and ends the summary's preview.

```uscope-view-example
uscope-views 1
view c intvec {
    show sequence(n) for i in range(n) => data[i]
}
view c str_t {
    show sequence(len + 1) for i in range(len) => p[i]
}
---
dangling => len=2 [<unavailable>, …]
s => problem: the view declares 6 elements and generates 5
```

## Extending views

`extend LANGUAGE PATTERN { … }` adds to whichever view presents a type,
from any file, without copying it: its fields come after that view's, and
its `hide`s and `format`s apply to that view's members and fields as to its
own. An `extend` holds `let`s, `type`s, `field`s, `hide`s, and `format`s,
never a `show`, and every `extend` whose pattern names a type adds to its
view. One with no view to add to adds to the value's members.

```uscope-view-example
uscope-views 1
view c intvec {
    show sequence(n) for i in range(n) => data[i]
    field capacity = cap
}
extend c intvec {
    field room = cap - n
    hide capacity
}
extend c tagged {
    format kind as hex
}
---
v => children: [0] = 10, [1] = 20, [2] = 30, room = 1, [raw]
something => {kind: 0x1, value: 7}
```

## Summaries

A presented value's summary is one line in one style for every language:
text in quotes, as `"hello, world"`; a sequence's length and its first
elements, as `len=3 [1, 2, 3]`, and a map's and its first entries, as
`len=2 {"one": 1, "two": 2}`; and other values as they print.

## Where views come from

A type's view is the first that binds, from these sources in order:

1. **The session's files**: those given with `--views FILE` or the debug
   adapter's `viewFiles`, and those `views load FILE` loads, the latest
   first. `views clear` forgets those loaded, and `views` lists them all.
2. **The project's and the user's files**: every `*.views` file in
   `.uscope/views` at the project root, then in
   `$XDG_CONFIG_HOME/uscope/views` (or `~/.config/uscope/views`), each
   directory's in name order. The project root is the nearest directory,
   from the one uscope runs in upwards (for the debug adapter, the launch
   request's `cwd`), that holds `.uscope/`, then the nearest that holds
   `.git`, so a session started anywhere in a project finds its views.
3. **The program's own**: views a module carries in its
   `.debug_uscope_views` section, which present only that module's own
   types, so that one library never restyles another's.
4. **The built-in views**, in `views/`, one file per library.

Loading views never asks first, since a view can only read. A file, or a
view in one, that cannot be used is reported once, with where and why, and
the session goes on without it.

A C or C++ program carries a view file by including `uscope_views.h`, from
uscope's `sdk/c`, and writing `USCOPE_VIEWS_FILE("views/app.views");` at
file scope, and a kernel by writing `USCOPE_KERNEL("tree",
"kernels/tree.c", "build/tree.wasm");`, the kernel's name, its source or a
link to it, and its module. A Rust program, with the `uscope-views` crate
in `sdk/rust`, writes
`uscope_views::uscope_views_file!(concat!(env!("CARGO_MANIFEST_DIR"), "/app.views"));`
and `uscope_views::uscope_kernel!("tree", SOURCE, MODULE);`. Both read the
files when the program is built, into a section that is not loaded when it
runs and that `strip --strip-debug` removes with the rest of the debug
information.

The section holds records, each a kind, a format (1), a 32-bit
little-endian length, and that many bytes; zero bytes between records are
padding. A view file is kind 1. A kernel is kind 2: a 16-bit length and
the kernel's name, a 32-bit length and its source, and its module.

The built-in views cover:

- C++, in libstdc++ and libc++: `std::string` and its other characters,
  in libstdc++'s C++11 and copy-on-write ABIs and in libc++ short and long;
  `std::string_view`, `std::vector` (not `std::vector<bool>`),
  `std::array`, `std::span`, `std::deque`, `std::list`, `std::forward_list`,
  `std::map`, `std::multimap`, `std::set`, `std::multiset`, and the
  `unordered_` maps and sets; `std::unique_ptr` (not of an array),
  `std::shared_ptr` and `std::weak_ptr` with their counts, `std::optional`,
  `std::variant`, and tuples of up to six elements. libc++ describes a
  `shared_ptr`'s counts only when built with `-fstandalone-debug`; without
  them a `shared_ptr` shows no counts, and a `weak_ptr`, which cannot say
  whether its object still exists, shows as stored.
- Rust: `String`, `PathBuf`, `OsString`, `CString`, `Vec`, `VecDeque`,
  `HashMap`, `HashSet`, `BTreeMap`, `BTreeSet`, `Box`, `Rc`, `Arc`, both
  `Weak`s, `Cell`, `RefCell`, and `Mutex`. `&str`, `Box<str>`, and slices
  are text and elements without a view.
- Go: maps and channels, including nil ones, which show as `nil`.
- Zig: `std.ArrayList` and the managed list, `std.HashMap`, its unmanaged
  map, and `std.ArrayHashMapUnmanaged`.

Some values need no view, because their debug information says what they
are:

- A Rust enum, and a Zig optional, error union, or tagged union, shows as
  the variant it holds: `Some(4)`, `Err("no")`, `Square {side: 4}`, or a
  Zig optional's or error union's payload itself, `null`, or `error.Bad`.
- A C++ object of a class with virtual functions shows as the object it
  is part of, `Square {id: 7, side: 3}`, when its vtable pointer is one
  the program's symbols name. A Rust trait object shows as the value its
  vtable is for, and a Go interface as its dynamic type and value, `int
  42`, `main.Point {X: 1, Y: 2}`, or `*errors.errorString *{s: "bad"}`.
  A value whose table is not one the program names shows as stored.

## Using views

- `print EXPR` shows a value as its view presents it, with its elements up
  to the inspection's budget, and `print/r EXPR` shows it as stored.
- `set views off` shows every value as stored, and `set views on` restores
  views.
- `info view EXPR` says which view presents a value, from which file and
  line, and why each view tried before it did not bind. `views explain
  TYPE` says the same of a type, and `views check` of every type a view's
  pattern names. Without a session, `uscope views explain PROGRAM TYPE`
  and `uscope views check PROGRAM` do so from the program's debug
  information alone; `views check` fails when a view loaded for the
  session or carried by the program presents no type, or binds no type it
  names.
- A pointer to a value presented as text shows the text after its address,
  as a pointer to characters does, or why the view could not read it. A
  null pointer shows only its address.
- An element of a value presented as a sequence is `v[i]`, and the count
  of a sequence or map is `len(v)`, in any expression: `break f if
  len(queue) > 100`. A Go channel is indexed this way too, though it is
  stored as a pointer, because Go never indexes one as a pointer. An
  element in memory can be assigned and its address taken. A map is
  indexed neither by position nor by key.
- A debug adapter client sees a presented value's elements or entries as
  indexed variables, in pages, and its fields and `[raw]` as named ones. An
  entry is named by its key, and evaluates as the place its value is in,
  `*(T*)ADDRESS`.
- Reading a later page of a list, tree, or table resumes from a checkpoint
  at most 256 elements back rather than from the start. A kernel's run
  cannot be resumed, so a later page of one runs the kernel again.

## Limits

A view file may be at most 256 KiB and hold at most 1024 views, and each of
its expressions is subject to the expression limits. A presentation's
summary may use a quarter of what its inspection has left, which the values
it presents share, and running out ends the summary early without failing
the inspection. A summary previews at most 16 elements or about 96
characters. Text is read up to 256 bytes. Views present values inside the
values they present at most four deep. Generators nest at most four deep,
trees are walked at most 128 levels deep, and a sequence without a count
generates at most 16,777,216 elements. A kernel is at most 256 KiB, its
memory at most 4 MiB, its calls nest at most 1024 deep, and each read it
makes is at most 64 KiB; it takes at most 32 arguments, and its items are
one to eight words.
