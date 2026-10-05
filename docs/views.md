# Views

A view presents a value as the thing it stands for: a `std::string` as its
text, a `Vec` as its elements, a hand-rolled C vector as the integers it
holds. The value as it is stored is never lost. It is one step away, as the
`[raw]` child, `print/r`, or `set views off`.

uscope has views for the C++, Rust, and Zig standard libraries' strings and
contiguous containers built in, and anyone can write views for their own
types in the same language. Views run inside the debugger, on the data of a
stopped program, and can only read: they cannot write memory, call
functions, or perform I/O, and every read they make is charged to the
inspection's budget, so a view can never hang the debugger or show a
convincing wrong value.

Every example on this page runs as one of uscope's tests, against a small
world of C types:

- `intvec`, a vector `{int *data; unsigned long n; unsigned long cap}`, as
  `v` holding 10, 20, and 30 with room for 4, `bad` claiming 9 elements
  in that room, `none` with no storage, `big` holding 0 to 299, and
  `dangling` pointing at unmapped memory;
- `str_t`, text `{char *p; unsigned long len}`, as `s` holding `hello`;
- `tagged`, a tagged union `{int kind; int value}`, as `nothing` of kind 0
  and `something` of kind 1 holding 7;
- the Rust `alloc::boxed::Box<i32>`, a wrapper around a pointer, as `b`
  pointing at 42;
- the C++ `app::detail::Pair<int, 3>`, `{int first; int items[3]}`, as `p`
  holding 1, then 2, 3, and 4.

An example block holds a view file, then `---`, then rows that read
`value => outcome`. An outcome is the summary the value is presented as;
`children:` and the children it expands to; `problem:` and why a view that
binds refuses the value; `unbound:` and why no view binds; or `error:` and
why the file itself is refused.

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
pattern names what a type is, not how its compiler spelled it, so it matches
whatever the compiler called the type: `pair<int const, …>` and
`pair<const int, …>` are one type, and inline namespaces such as libc++'s
`std::__1` may be spelled or left out.

- `**` in a path matches any run of segments, so `alloc::**::Box<T>` keeps
  matching when the standard library moves a type between modules.
- `_` matches any argument, and trailing arguments may be left out.
- A capitalized name captures an argument: a type, which the view's
  expressions may name, or a value, which they may use as a number.
- An integer matches an argument of that value, and a pattern matches a
  type argument.

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

The expressions in a view are uscope's own expressions
(`docs/expressions.md`), evaluated with `self` as the value presented. A
member of `self` is named by its own name, and a view's `let`s and the
arguments its pattern captured shadow them. Nothing the program's frame
names is visible, so a view means the same thing at every stop.

Views may also call `inner(x)`, which steps through wrapper records: while
`x` is a record with exactly one member of non-zero size, and no base, it is
that member. Zero-sized markers such as Rust's `PhantomData` do not count.
`inner` absorbs the wrapper layers a library adds and removes between
versions, such as the `RawVec`, `Unique`, and `NonNull` around a Rust
`Vec`'s pointer.

```uscope-view-example
uscope-views 1
view rust alloc::**::Box<T> {
    show value(*inner(self))
}
---
b => 42
```

A member named `or`, `for`, `if`, or `else`, which end an expression in a
view, is written in backticks.

## Statements

A view's body holds statements, one to a line. A statement continues onto
the next line unless that line begins another statement or ends the view.

- `let NAME = EXPR or EXPR …` names a value, computed at most once each time
  the view presents a value. The first alternative that binds is used, so
  one view can describe a layout that changed between versions.
- `type NAME = TYPE or TYPE …` names a type: a type the program defines, an
  argument the pattern captured, `typeof(EXPR)`, or `arg(TYPE, N)`, the
  type's `N`th argument, counted from 0.
- `check EXPR` states an invariant. A value that breaks one is not what the
  view describes, so it shows as stored, with the check that failed. A
  check of several conditions joined by `&&` is several checks.
- `field NAME = EXPR` adds a named child.
- `summary "TEXT {EXPR} TEXT"` overrides the summary; `{EXPR}` is replaced
  by its value's summary, and `\{` and `\}` are braces.
- `show SHAPE` says what the value is. A view shows exactly once.

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

## Shapes

- `text(PTR)` and `text(PTR, LEN)` are text: the characters a pointer to
  one-byte characters points to, up to a NUL or `LEN` of them. `PTR` may
  also be an array or slice of one-byte elements, whose length is `LEN`'s
  default.
- `value(EXPR)` presents the value as another value, as a box presents what
  it holds.
- `empty("TEXT")` is a value that holds nothing, summarized as `TEXT`.
- `sequence(COUNT) for I in range(N) => ELEMENT` is a sequence of `N`
  elements, element `I` being `ELEMENT`. `COUNT` must equal `N`, or be `_`
  to leave the count to the range. Each element is computed on its own, so
  reading one costs the same wherever it is.
- `if COND { SHAPE } else { SHAPE }` chooses a shape, and may begin a
  statement of its own.

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
The value then shows as stored, with the reason, never as a plausible
container. An element the program cannot provide is said to be unavailable
and ends the summary's preview.

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

## Summaries

A presented value's summary is one line in one style for every language:
text in quotes, as `"hello, world"`; a sequence's length and its first
elements, as `len=3 [1, 2, 3]`, up to 16 elements or about 96 characters;
and other values as they print.

## Where views come from

The views built into uscope cover:

- C++: `std::string` and its other characters in libstdc++, in the C++11
  and the earlier copy-on-write ABI, and in libc++ short and long;
  `std::string_view`, `std::vector` (not `std::vector<bool>`),
  `std::array`, and `std::span`.
- Rust: `String`, `PathBuf`, `OsString`, `CString`, `Vec`, and `VecDeque`.
  `&str`, `Box<str>`, and slices are text and elements without a view.
- Zig: `std.ArrayList` and the managed list.

Their files are in `views/`, one per library.

## Using views

- `print EXPR` shows a value as its view presents it, with its elements up
  to the inspection's budget, and `print/r EXPR` shows it as stored.
- `set views off` shows every value as stored, and `set views on` restores
  views.
- `info view EXPR` says which view presents a value, from which file and
  line, and why each view tried before it did not bind.
- An element of a value presented as a sequence is `v[i]`, and its count is
  `len(v)`, in any expression: `break f if len(queue) > 100`. An element
  in memory can be assigned and its address taken.
- A debug adapter client sees a presented value's elements as indexed
  variables, in pages, and its fields and `[raw]` as named ones.

## Limits

A view file may be at most 256 KiB and hold at most 1024 views, and each of
its expressions is subject to the expression limits. A presentation's
summary may use a quarter of what its inspection has left, and running out
ends the summary early without failing the inspection. Text is read up to
256 bytes. Views present values inside the values they present at most four
deep.
