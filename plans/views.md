# Data views plan

TODO item (new): *Render standard-library and user data structures as what
they mean: a `std::string` as text, a `Vec` as its elements, a Go map as its
entries.*

A debugger exists to show data. Today uscope shows structure: a Rust `Vec` is
`{buf: {inner: {ptr: …, cap: …}}, len: 3}`, a Go map is a pointer to
`internal/runtime/maps.Map`, and a libc++ `std::string` is a union of bit
fields. This plan adds **views**, which present a value as the thing it
stands for while keeping the raw structure one step away. They cover the C++,
Rust, Go, and Zig standard libraries and any type a user describes. Nothing
in the design depends on one compiler, compiler version, linker, or loader.

Research ran on 2026-10-05 against the pinned toolchains (gcc 15.2, clang
21.1.8 with libstdc++ and libc++ 21, rustc nightly 2026-07-10, Go 1.26.5,
Zig 0.16.0) and the Windows and Linux visualizer ecosystems. §1 and the
appendices keep what matters. Decisions D1–D10 were made the same day (§6).

## Goals

1. **Flexible for maintainers and users alike.** The built-in views track
   the newest versions of every supported toolchain. Users override and
   extend them trivially, and write views for their own complex types as
   easily as uscope writes them for standard libraries. Contributions to
   the built-in views are welcome, so contributing is easy.
2. **Fast and robust.** It should be nigh impossible for a view to harm the
   debugger. That means:
   - no crash, no hang, no unbounded memory;
   - no noticeable delay to run control;
   - no convincing wrong answer.
3. **Sane defaults, easy to extend and maintain**, inside a clean and
   simple architecture: views run in the existing inspection paths, not
   beside them.

## 0. Summary

- **What the ecosystem offers.** Nothing on Linux describes container
  semantics in a debugger-neutral form that uscope can consume (§1).
- **What is reliable.** DWARF's *structure*:
  - template parameters by position;
  - inline namespaces marked `DW_AT_export_symbols`;
  - `DW_TAG_variant_part` discriminants;
  - fat-pointer shapes;
  - Go's `DW_AT_go_kind`, `go_key`, `go_elem`, and `go_runtime_type`;
  - Zig's `DW_AT_ZIG_sentinel`;
  - vtable symbols naming concrete types.

  uscope throws most of this away today (§2). The first phase makes
  uscope's DWARF handling compliant and complete, and uses it fully.
- **Design.** There are three layers, joined by one small **view contract**
  (§3.0).
  1. The provider normalizes structure and type identity (§3.1–3.2).
  2. **Declarative views** map type patterns to presentations (§3.3–3.6).
     - A view *binds* against the concrete type before it runs, and an
       alternative that does not bind falls through to the next.
     - Algorithms the language cannot say well can later call sandboxed
       WebAssembly **kernels** (§3.13).
  3. A neutral presentation model, which every client renders (§3.7).
- **Where views come from.** Built-in view files ship in the repository.
  Session, user, and project files override and extend them. Binaries can
  carry views in a `.debug_uscope_views` section. All of them load without a
  prompt, because views cannot write, call, or perform I/O (§3.8).
- **Where views run.** On the controller thread, inside the existing
  inspection paths (§3.11):
  - every step is charged to the inspection budget, which serves as a
    deterministic timeout;
  - run control and process events are scheduled ahead of inspection;
  - a resume cancels a running presentation.
- **Explicit results.** A view never guesses:
  - a value no view binds shows raw, and `info view` says why;
  - a failed invariant shows raw with the reason;
  - a cycle or an exhausted budget is a typed partial result.

  Raw is always one step away: a `[raw]` child, `print/r`, or `set views
  off`.
- **Clean room.** Views are written from the DWARF of uscope's own compiled
  fixtures. uscope does not run, read, port, or test against any Python
  printer, or any GPL code (D2).

## 1. What exists, and what it teaches

### 1.1 The ecosystem

| Approach | Where | Model | Lesson |
|---|---|---|---|
| natvis | Visual Studio, WinDbg, VS Code's MIEngine, CLion/Rider 2026.2 (also on Linux and macOS) | declarative XML; expressions in each host's C++ evaluator; matched by name wildcards with `$T` arguments; embedded per module in PDBs (`/NATVIS`) | The richest declarative vocabulary. Not portable in practice: the hosts' evaluators disagree, and implementers support subsets. |
| LLDB C++ formatters | compiled into LLDB | C++ code keyed by type-name regex | Owned by LLDB, not by libc++, so they chase libc++ changes months later. LLDB's tests simulate 60 layouts of `std::string` alone. |
| LLDB formatter bytecode | `.lldbformatters` section in a binary | small sandboxed stack VM with a fixed selector table | The only sandboxed in-binary format. Emitted only by Swift; libc++ intends to use it eventually. |
| RAD Debugger | config, or a `.raddbg` section | `type_view: {type, expr}` patterns with captures; views are expressions with lenses | The closest design to this plan; indexing goes through views. |
| Delve | Go | hard-coded, but decides kinds from `DW_AT_go_kind`, never names, and chooses layouts by which fields exist; CI checks its runtime assumptions | The best model for surviving runtime churn. |

### 1.2 Who maintains visualizers

- **Windows is cheap mostly because the MSVC STL's ABI has been frozen
  since 2015.**
  - `STL.natvis` changed in 36 commits over 5.5 years, about 6 a year,
    mostly for new types.
  - It has no tests, and ships with the IDE through a hand-mirrored copy
    that has lagged by a year.
  - ABI-compatible member renames still broke it.
  - One of its bugs showed the wrong year for February dates.
- **Only one arrangement reliably keeps a visualizer correct.** The owner
  changes it in the same commit as the layout, and a test fails if they
  forget.
  - Rust does this for its natvis: all 13 layout-driven natvis changes in
    the last three years landed in the std PR that caused them, caught by
    Windows CI.
  - Library-owned natvis files without tests rot for 10–18 months (LLVM's
    `SmallPtrSet`, nlohmann/json, Qt 6).
- **Rust's natvis is portable but not usable as a source.** 92% of its
  entries carry over to Linux DWARF, as-is or after a mechanical rename. But
  Linux toolchains don't ship the files, each copy matches only one rustc
  commit, and they omit `BTreeMap`, `PathBuf`, `Box<str>`, and `Mutex`.
- **How fast layouts change, by library:**

  | Library | Change rate |
  |---|---|
  | libstdc++ | effectively frozen since GCC 5 |
  | libc++ | stable bytes, but renamed DWARF members, about 1–2 a year |
  | Go | rare but large (the 1.24 map rewrite, `abi.Type` changes) |
  | Rust | about 4 a year |
  | Zig | pre-1.0, and its two backends differ |

Nothing removes uscope's share of the work. §3.12 keeps it small, visible,
and harmless.

### 1.3 What every successful design shares

1. **Matching by type, with captured arguments.** Natvis `$T1`, RAD
   `?{T}`, LLDB template-argument selectors.
2. **Children are lazy, and random-access where possible.** LLDB's
   synthetic `get_child_at_index`, and DAP's `start`/`count`. Linked
   structures scan with checkpoints; LLDB caches iterators by index.
3. **Fallback across layouts.** Natvis `Priority` and `Optional`; Delve
   chooses map layouts by which fields exist.
4. **A raw view everywhere.** `[Raw View]`, `frame variable --raw`.
5. **Views compose with expressions.** Natvis `[]` on `ArrayItems`; RAD
   lenses as types.

### 1.4 Failures this design forbids

- **A convincing wrong answer.**
  - lldb truncates 64-bit Rust discriminants to 32 bits, so `Ok(7)` shows
    as `Err("")`.
  - A visualizer that swallows its own error can show a populated container
    as empty.
  - `STL.natvis` once showed the wrong year.
- **Unbounded work.** Debuggers have hung or exhausted memory on
  uninitialized containers, garbage sizes, and cyclic lists.
- **Name matching that misses real spellings.**
  - At `-O`, rustc names slices `*const [T]`. uscope's own `&[` prefix test
    misses them (§2).
  - GCC writes `pair<int const, …>` and `array<int, 4>`; clang writes
    `pair<const int, …>` and `array<int, 4UL>`.
  - `-gsimple-template-names` emits bare `vector`.
- **Version churn handled by path lists.** Delve contains it best: kinds
  from structural attributes, layouts chosen by which fields exist, and
  assumptions checked automatically.

## 2. Where uscope is today

From a survey of `next` at 89d634cf:

- **Types** are normalized per image (`TypeInfo { name, byte_size, kind }`,
  `src/model.rs:625`). Several things are dropped:
  - the unit's language and producer (read for Zig and Go special cases,
    then discarded);
  - namespaces (`alloc::string::String` is named `String`);
  - **template parameters** (`is_scope_only_child`,
    `variables/types.rs:108–128`);
  - every Go attribute except `go_embedded_field`.

  The only thing a matcher could key on is the bare `name` string.
- **Containers** are recognized by name:
  - slices by the prefixes `&[` or `[]` (`is_slice_type_name`), which misses
    Rust's optimized `*const [T]`;
  - text by `string_parts` in `variables/text.rs` (`&str`, Go `string`,
    Rust `String` by "first pointer under `buf`", libstdc++
    `basic_string<char,`).

  Zig `[]const u8` gets no text, and nor do libc++ strings or `Box<str>`.
  Text reads are not charged to the inspection budget.
- **Values** are one level deep, with children fetched through a
  stop-scoped, paged capability (`ValueChildrenReference`, `total` fixed).
  This is the right shape for lazy views.
- **Limits:**
  - The default per-operation budget (1024 memory bytes, 64 reads) is too
    tight for a container that follows a heap pointer.
  - The DAP adapter parses the `variables` request's `filter` and ignores it.
- **Expressions** reach programs through `Scope` and `Machine`
  (`src/eval/target.rs`), and the provider owns layout. Views reuse both.
- **The simulator** never asks for children or text, so nothing here is
  under its oracles yet.

## 3. Design

### 3.0 The view contract

Everything below implements one small contract between *a view* and *a
debugger*. LLDB's bytecode RFC called this the hard part: "all the
interesting/difficult work here is about how to interface with
ValueObject". Keeping it small and explicit is also what would make it
proposable later (§7).

**What a view may ask the debugger.** Every request is a pure read,
answered at one stop, and charged to the budget:

- **Type queries.** Identity (language, path, base, arguments), size,
  members (name, type, offset or bit range), variants and discriminants,
  pointer target, array bounds, enumerators, and lookup of an instance by
  identity in the value's own module.
- **Places.**
  - Member, index, dereference, and reinterpretation at an address as a
    type.
  - `inner()` and `container_of()`.
  - The address of a place.
- **Reads.** A scalar at a place, or bytes at an address, bounded.
- **Dynamic types.** The concrete type behind a vtable address or a Go
  runtime type address (§3.6).
- **Module globals.** A named global of the value's module, read-only.
  Some custom types are meaningless without one: an arena, a string table,
  an ECS world. Locals and registers are never visible, so a view means
  the same at every stop.

**What a view produces.**

- A **shape**: text, value (transparent), empty, sequence, map, variant, or
  record.
- A **count**: exact, at least, or unknown.
- A one-line **summary**.
- **Children**, each one of:
  - a **place** (address and type), so expressions, watchpoints,
    `setVariable`, and memory views keep working on it;
  - a computed value, which is read-only;
  - a key and value pair.
- Named **fields**.
- **Problems**, which are typed.

There are three ways to implement the contract, and all of them bind,
budget, and report problems identically:

| Front-end | Written as | For | Status |
|---|---|---|---|
| Declarative view (§3.3) | text, in the view language | almost everything; all built-in views if possible | the core |
| Kernel (§3.13) | WebAssembly, called from a declarative view | iteration algorithms the language cannot say | after the first real need |
| Native view | Rust inside uscope | nothing, unless a stated reason exists | the exception |

### 3.1 Layer 1: structure the provider normalizes (no views)

These are DWARF semantics, not library knowledge. The provider presents them
for every language with no view involved:

- **Sum types.** `DW_TAG_variant_part` (Rust enums including niches;
  Zig self-hosted `?T`, `E!T`, tagged unions) is already normalized to
  `TypeKind::Variant`. The LLVM backend's Zig shapes are added:
  - an optional `{payload, some}` record;
  - an error union `{error, payload}` record;
  - `?*T` as a nullable pointer;
  - a tagged union as `{payload, tag}`.

  They are recognized by the producer and shape, as the self-hosted ones
  already are.
- **Fat pointers by shape, not name.** A two-word record whose members are a
  data pointer and a length is a slice wherever the language says so:
  - Rust `data_ptr`/`length`, under any name, including `*const [T]`,
    `Box<[T]>`, `*const Path`;
  - Zig `ptr`/`len`;
  - Go `go_kind = Slice` with `array`/`len`/`cap`.

  Rust trait objects are `pointer`/`vtable`. Unsized tails (`RcInner<str>`,
  `Path`) take their length from the enclosing fat pointer.
- **Text by encoding.** The provider presents these as text:
  - a slice or pointer of 1-byte character type;
  - Rust `str`;
  - Go `go_kind = String`;
  - Zig `[]const u8` and sentinel pointers (`DW_AT_ZIG_sentinel`).

  The standard-library string *classes* (C++ `basic_string`, Rust
  `String`) are views (§3.3), because their layouts are private.
- **Dynamic types** (§3.6) are provider primitives, because they come from
  symbols and runtime tables rather than from a field path.

### 3.2 Type identity

Views match on identity, which the provider keeps alongside `name`:

```rust
pub struct TypeIdentity {
    pub language: SourceLanguage,       // DW_AT_language of the defining unit
    pub path: Arc<[Arc<str>]>,          // namespaces, inline ones removed
    pub inline_namespaces: Arc<[Arc<str>]>, // the removed ones, which names may spell
    pub base: Arc<str>,                 // "vector", "Vec", "Aligned"
    pub arguments: Arc<[TypeArgument]>, // by position, packs flattened
    pub origin: ArgumentOrigin,         // Dwarf | ParsedName | None
    pub go: Option<GoTypeAttributes>,   // go_kind and go_runtime_type
}
pub enum TypeArgument { Type(TypeReference), Value(IntegerValue), Unknown(Arc<str>) }
```

- **Inline namespaces collapse structurally.** gcc 15 and clang 21 both mark
  `std::__cxx11` and libc++'s `std::__1` with `DW_AT_export_symbols` (checked
  on this machine). A fallback list (`__1`, `__Cr`, `__ndk1`, `__cxx11`,
  `__8`, `__debug`) applies only to C++ units older than DWARF 5, and only
  when no unit in the image marks any namespace, since an unmarked namespace
  from a producer that marks is not inline. libstdc++'s `__cxx1998` is never
  inline, and `__debug` is inline only in debug mode.
- **Template arguments come from `DW_TAG_template_*_parameter` and
  `GNU_template_parameter_pack` DIEs**, by position. The DIE's names differ
  by library (`_Tp, _Nm` against `_Tp, _Size`). Building them makes their
  types reachable, so element types are built even when no variable
  mentions them.
- **Where DWARF has no parameters, the name is parsed.**
  - GCC omits parameters on 39 of 309 templates in the experiment, including
    `std::allocator<T>`.
  - Zig and Go never emit them.

  One neutral parser handles `a::b<…>`, Zig's `mod.Fn(…)`, and Go's
  `pkg.T[…]`. An argument resolves through the type index; one that does not
  stays `Unknown(text)` and can match only a wildcard.
- **Go** keeps `go_kind`, `go_key`, `go_elem`, and `go_runtime_type`. A Go
  map, channel, slice, string, or interface is identified by kind. Its key
  and element are its arguments, whatever it is named.
- **The type index.** Each image gets an index, built at load, from
  `(language, path, base)` to instances. It answers "the
  `std::_Rb_tree_node` whose argument is `std::pair<const K, V>`" by
  comparing argument *identities*, never by spelling a name. This replaces
  the linear scan in `FrameScope::lookup_type` that expressions P3 deferred.

### 3.3 Layer 2: the view language

A view file holds views. Each view matches type identities and says how to
present a matching value. Expressions inside are ordinary uscope
expressions (`docs/expressions.md`). They are evaluated with `self` as the
value and its members as bare names. A view's own names shadow members.
The value's module's globals are reachable only through `global(NAME)`.
Locals and registers are not visible, so a view means the same thing at
every stop.

The syntax below is a sketch; P2 settles it in `docs/views.md`.

```text
view <language> <pattern> {
    let  NAME = EXPR [or EXPR]…        # first alternative that binds
    type NAME = TYPE [or TYPE]…        # captured argument, typeof(EXPR), arg(TYPE, n), or an instance
    check EXPR                         # run-time invariant; failure is reported, raw is shown
    summary "TEXT {EXPR} TEXT"         # optional one-line override
    show SHAPE
    field NAME = EXPR                  # named synthetic children, e.g. capacity
}

SHAPE := text(PTR [, LEN])             # encoding from the element type; NUL-terminated without LEN
       | value(EXPR)                   # transparent: present as another value (Box, unique_ptr)
       | empty("TEXT")                 # None, nullopt, empty Weak
       | sequence(COUNT) for I in GEN [if COND]… => ELEMENT    # COUNT may be _ (unknown)
       | map(COUNT)      for I in GEN [if COND]… => KEY : VALUE
       | dynamic(PTR, TYPE)            # present PTR's target as TYPE (§3.6)
       | match EXPR { CONST => SHAPE, … _ => SHAPE }   # C tagged unions, state machines
       | record { NAME = EXPR, … }     # a synthetic record: choose, rename, compute
       | if COND { SHAPE } else { SHAPE }

# Statements that shape what users see, mostly for their own types:
    hide NAME, …                       # members omitted from children ([raw] keeps them)
    format NAME as hex | char | flags(ENUM) | bytes | utf16 | duration(UNIT) | enum(ENUM)

# Built-ins available in view expressions (not in the console language):
    inner(E)  container_of(PTR, TYPE, MEMBER)  offsetof(TYPE, MEMBER)  typeof(E)  arg(TYPE, N)  global(NAME)
    vtable_type(PTR)  go_type(ADDR)  kernel(NAME, ARGS…)   # §3.13

# A file may also extend a view another source defined, without copying it:
extend <language> <pattern> { field … / hide … / format … }

GEN    := range(N) | list(HEAD, N => NEXT) | inorder(ROOT, N => LEFT, N => RIGHT) | GEN for J in GEN
# after a generator, in order: `if COND` filters, and `let NAME = EXPR` names a
# value computed once for each of the generator's values (P3)
```

**Wrappers.** `inner(EXPR)` steps through wrapper records. While the value
is a record with exactly one member of non-zero size, it descends into that
member, ignoring zero-sized markers such as `PhantomData` and `Global`. It
stops at anything else.

- This is structural, not a guess. The record's own DWARF says the wrapper
  holds nothing else, and the binder still type-checks what comes out.
- It absorbs the most common kind of library churn, which adds or removes
  a newtype layer (§3.12).
- Checked on rustc 2026-07-10:
  - `inner(buf)` of a `Vec` descends `RawVec { inner, _marker }` into
    `RawVecInner { ptr, cap, alloc }` and stops, because two members have
    size.
  - `inner(inner(buf).ptr)` descends `Unique { pointer, _marker }` and then
    `NonNull { pointer }` to a `*u8`.
  - The pre-1.84 `RawVec<T> { ptr, cap, alloc }` gives the same answers.

**Patterns.** A pattern is `<path>::<base><ARG, …>`:

- `_` is a wildcard argument, and a capital name captures an argument as a
  type usable in the body.
- `<language>` is `c`, `c++`, `rust`, `go`, `zig`, or `any`.
- For Go, the pattern can be a kind (`go map<K, V>`, `go interface`).
- `**` in a path matches any run of modules, so `alloc::**::Rc<T>` keeps
  matching when std moves `Rc` between modules (`alloc::rc` became
  `alloc::rcs` in 2026). It anchors on the crate root and the base name,
  which are what users write.

**Binding.** When a value of a new type is first presented, each matching
view is bound against the concrete type in source order (§3.8). Binding
means:

- every member path resolves;
- every cast and type construction resolves through the type index;
- every expression type-checks.

An `or` alternative that fails to bind is skipped. A view with any
unbindable part is rejected, *with its reasons recorded*, and the next
candidate is tried. The first view that binds is cached per
`(TypeReference, view-set generation)`. If none binds, the value is raw.

Supporting libc++ and libstdc++, or an old and a new Rust `RawVec`, is
therefore two views or one `or` chain. Neither needs version detection,
because the DWARF itself says which layout this binary has. A `when` guard
over the unit's producer version is the escape hatch, kept for facts
DWARF cannot express, such as the deque block size.

**Running.** A bound view runs through the `Machine` trait, charging the
inspection budget for every read and every generator step. `check`s run
first. If one is false, the value presents raw with the failure
(`len 9 > capacity 4`), never as a plausible container.

**Children and access.** The engine derives how children can be reached
from the shape:

- `for i in range(N)` with no filter is **random access**: child `k` costs
  one evaluation, so a DAP page costs only that page.
- Anything else is a **scan**:
  - the controller caches checkpoints (the generator state every 256
    children) for the current `StopId`, so later pages resume rather than
    restart;
  - `list` ends at a null pointer or at its head again (a ring), and finds
    a node seen before exactly among the nodes one request visits, and by
    Brent's algorithm across the requests that resume from checkpoints;
  - `inorder` bounds its explicit stack at 128 levels.

  A scan stops at its declared `COUNT`. A cycle, an overlong stack, or
  generators that end before the count are typed problems (`cycle at
  element 3`): found while presenting the summary, the value shows raw with
  the problem; found while paging, that page fails with it. (P3 settled
  this; reading past the count to find more elements was dropped, since a
  sparse hash table would scan every empty slot to prove there are none.)

### 3.4 Examples

These are sketches written against the layouts observed in the research.
The final spellings are checked by the fixtures (§5).

```text
# Rust Vec<T>. Since 1.84 RawVec holds an erased Unique<u8>, so T comes from
# the pattern. inner() absorbs the wrapper layers std adds and removes:
# RawVecInner, Unique, NonNull, the Cap/UsizeNoHighBit newtypes.
view rust alloc::vec::Vec<T, _> {
    let data = inner(inner(buf).ptr) as *T
    let cap  = inner(inner(buf).cap)
    check len <= cap
    show sequence(len) for i in range(len) => data[i]
    field capacity = cap
}

# std::vector: libstdc++ and libc++ differ in every member name. Exactly one
# of these binds against a given binary.
view c++ std::vector<T, _> {
    let begin = _M_impl._M_start
    let end   = _M_impl._M_finish
    check begin <= end && end <= _M_impl._M_end_of_storage
    show sequence(end - begin) for i in range(end - begin) => begin[i]
    field capacity = _M_impl._M_end_of_storage - begin
}
view c++ std::vector<T, _> {
    let cap = __cap_ or __end_cap_.__value_      # the older __compressed_pair layout
    check __begin_ <= __end_ && __end_ <= cap
    show sequence(__end_ - __begin_) for i in range(__end_ - __begin_) => __begin_[i]
    field capacity = cap - __begin_
}

# libstdc++ std::map: an in-order walk of the red-black tree, with the node
# type found through the type index rather than by spelling its name.
view c++ std::map<K, V, _, _> {
    type Node = std::_Rb_tree_node<std::pair<const K, V>>
    let  n    = _M_t._M_impl._M_node_count
    show map(n) for x in inorder(_M_t._M_impl._M_header._M_parent, x => x->_M_left, x => x->_M_right)
        => (*(std::pair<const K, V>*)&((Node*)x)->_M_storage).first
         : (*(std::pair<const K, V>*)&((Node*)x)->_M_storage).second
}

# hashbrown, under std::collections::HashMap. A bucket is full when its
# control byte's top bit is clear; entries sit just below ctrl, in reverse.
view rust std::collections::hash::map::HashMap<K, V, _> {
    let  t       = base.table.table
    type Pair    = arg(typeof(base.table), 0)          # RawTable<(K, V)>
    let  buckets = t.bucket_mask + 1
    let  ctrl    = t.ctrl.pointer as *u8
    check t.items <= buckets
    show map(t.items) for i in range(buckets) if ctrl[i] & 0x80 == 0
        => ((ctrl as *Pair) - (i + 1)).0 : ((ctrl as *Pair) - (i + 1)).1
}

# Go maps (swiss tables, Go 1.24+), matched by DW_AT_go_kind. A small map's
# dirPtr is really one group; the directory repeats a table across 2^(G-L)
# aligned slots, so only each table's first slot is visited.
view go map<K, V> {
    let  m     = *self
    type Group = typeof(*(**m.dirPtr).groups.data)
    if m.dirLen == 0 {
        show map(m.used) for s in range(8) if ((Group*)m.dirPtr)->ctrl >> (8 * s) & 0x80 == 0
            => ((Group*)m.dirPtr)->slots[s].key : ((Group*)m.dirPtr)->slots[s].elem
    } else {
        show map(m.used)
            for d in range(m.dirLen) if d % (1 << (m.globalDepth - m.dirPtr[d]->localDepth)) == 0
            for g in range(m.dirPtr[d]->groups.lengthMask + 1)
            for s in range(8) if m.dirPtr[d]->groups.data[g].ctrl >> (8 * s) & 0x80 == 0
            => m.dirPtr[d]->groups.data[g].slots[s].key : m.dirPtr[d]->groups.data[g].slots[s].elem
    }
}

# A user's own C types, from a project view file.
view c intvec {
    check n <= cap
    show sequence(n) for i in range(n) => data[i]
}
view c node {
    show sequence(_) for x in list(self.next, x => x->next) => x->value   # _: count unknown
}

# A C tagged union, the most common hand-rolled sum type.
view c value {
    show match kind {
        VAL_INT => value(as.i)
        VAL_STR => text(as.s.ptr, as.s.len)
        VAL_NIL => empty("nil")
    }
}

# An intrusive list in the Linux kernel's style: nodes embed a list_head.
view c run_queue {
    show sequence(nr) for n in list(tasks.next, n => n->next) if n != &tasks
        => *container_of(n, struct task, run_node)
}

# An arena handle is an index into a module global.
view rust game::Handle<T> {
    show value(global(`game::ARENA`).slots[index].value as T)
}

# Adding to a built-in view instead of replacing it.
extend rust alloc::vec::Vec<T, _> { format len as hex }
```

The language cannot express everything well. Go maps caught mid-growth
(classic `hmap` before 1.24) and Rust's `BTreeMap` are examples. For these,
a **native view** implements the same trait in Rust: it binds, runs through
`Machine`, charges the budget, and appears in diagnostics like any other.
Native views are the exception, and each one needs a stated reason.

### 3.5 Expressions through views

Views extend expressions only where the raw type gives no meaning, so there
is never an ambiguity:

- **Indexing.** `v[i]` on a value whose type has no indexing of its own (a
  record) indexes a sequence view, by random access or by scanning within
  the budget.
- **Length.** `len(v)` is the view's count.
- **Members.** `v.len` still reads the raw member.

Children carry an `evaluateName` that parses back:

- `v[3]` for sequences;
- a canonical place expression for map entries (`*(Pair*)0x…`), until map
  lookup by key (`m["one"]`) lands in a later phase.

Conditions such as `break … if len(queue) > 100` follow from this.

### 3.6 Dynamic types

These are provider primitives, exact or unavailable, never guessed:

- **C++ polymorphic classes:** the vptr's target address must equal a
  `vtable for X` symbol plus the ABI's offset. Then `X` is looked up through
  the type index.
- **Rust `dyn`:** the vtable address equals a
  `<C as Trait>::{vtable}` variable, whose `{vtable_type}` has
  `DW_AT_containing_type` C.
- **Go interfaces:**
  1. read `_type` (or `tab.Type`);
  2. find the module whose `[types, etypes)` contains it;
  3. subtract `runtime.types` and look the result up in the
     `go_runtime_type` table.

  Whether the value is stored directly sits in a flag that moved from
  `Kind_` to `TFlag` in Go 1.26. Both are checked, as Delve does.

Views use these through `dynamic(PTR, TYPE)`, with `TYPE` from
`vtable_type(PTR)` or `go_type(ADDR)`. The provider owns the lookups.

### 3.7 Layer 3: presentation

`VariableState::Available` gains an optional presentation; the raw value is
untouched:

```rust
pub struct Presentation {
    pub view: ViewName,                 // source and name, for diagnostics
    pub shape: PresentedShape,          // Text | Value | Empty | Sequence | Map | Dynamic
    pub count: PresentedCount,          // Exact(n) | AtLeast(n) | Unknown
    pub summary: Arc<str>,              // bounded one-line rendering
    pub problem: Option<ViewProblem>,   // check failed, cycle, budget, unreadable
}
```

`ValueChildrenReference` gains a view form: the bound view, the `self`
place, and the access mode. Paging, `StopId` validation, and image checks
are unchanged. Children are:

- the elements or entries;
- the view's `field`s;
- a `[raw]` child whose children are today's members.

A map entry is a child whose relationship carries its key.

**Summaries** follow one style across languages and clients. They are fixed
by tests in `docs/views.md`, at a cost of one bounded preview:

```text
"hello, world"                       len=5 [1, 2, 3, 4, 5]
len=2 {"one": 1, "two": 2}           len=300 [0, 1, 2, 3, 4, 5, 6, 7, …]
Some(42)   None   Ok(7)              Rc(strong=2, weak=1) Point {x: 3, y: 4}
nullopt    unique_ptr 0x5555… → 7    error(*errors.errorString) "boom"
```

**Clients:**

- **CLI.**
  - `print v` shows the summary, then the elements up to the output limit.
  - `print/r v` and a `set views off` setting show raw.
  - `info view v` explains which view applied, from which source and line,
    and why each other candidate failed to bind. Without it, nobody can
    tell why a type is not pretty.
- **DAP.**
  - `indexedVariables` is the element count and `namedVariables` counts the
    fields plus `[raw]`.
  - The `filter` argument is honored, so VS Code can chunk large sequences
    in hundreds.
  - `presentationHint.attributes` gets `rawString` for text and `readOnly`
    for synthetic children.
  - Elements that are places keep `memoryReference` and `setVariable`.

### 3.8 Where views come from

The sources, highest precedence first. Within a source, a file's own order
applies.

1. **Session:** `views load FILE`, and a DAP launch argument `viewFiles`.
2. **User and project:** `$XDG_CONFIG_HOME/uscope/views/*.views` and
   `.uscope/views/*.views` in the working directory.
3. **The binary:** a `.debug_uscope_views` section, which follows the
   `.debug_gdb_scripts` precedent.
   - Records are length-prefixed: a kind (view text, or a kernel module),
     a format version, a length, then the payload.
   - Zero bytes between records are skipped, because compilers pad
     section contributions and wasm payloads contain NULs.
   - The section must be non-`ALLOC`, so it costs nothing at run time and
     `strip --strip-debug` moves it with the rest of the debug information.
     That rules out the obvious spellings. A C
     `__attribute__((section(…)))` array and a Rust
     `#[link_section] #[used] static` both produce an `ALLOC` section that
     is loaded into memory and survives stripping (checked with gcc, clang,
     and rustc). The C header and the Rust macro emit
     `.pushsection .debug_uscope_views,"",@progbits` through inline or
     global asm instead, as gdb documents for `.debug_gdb_scripts`.
   - A view embedded in a module applies only to types defined in that
     module, so one library cannot restyle another's types.
4. **Built-in:** `views/{libstdcxx,libcxx,rust,go,zig}.views`, compiled
   into uscope with `include_str!`.

Every source is safe to load without a prompt:

- views cannot call functions or write memory;
- every step is charged to the budget;
- a broken view costs only its own value, which falls back to raw with the
  reason.

A parse or bind error in a file is reported once per load (in the
CLI, and as DAP output), and never stops the session.

### 3.9 Limits

- Views reuse `InspectionBudget`:
  - text reads are now charged;
  - the default per-presentation budget rises so that a container plus
    one page fits (proposed: 64 KiB and 256 reads per top-level value);
  - the hard maxima are unchanged.
- **Summary preview:** at most 16 elements or 96 characters, and a quarter
  of the budget.
- **Text:** 256 bytes in a summary; 4096 when printed; the full length
  through `[raw]` and memory views.
- **Generators:**
  - nesting is at most 4 deep;
  - a scan may not pass its declared count, or a fixed ceiling (2^24)
    when the count is unknown;
  - every loop consumes input on each pass (AGENTS.md).
- **View files:** 256 KiB per file and 1024 views per source. Expression
  limits apply to every expression inside a view.

### 3.10 What is out of scope

- **Python and GPL code** (D2). uscope does not run, read, port, or test
  against any Python printer or GPL source. Views are written from the DWARF
  of uscope's own fixtures.
- **Structural guessing.** RAD's `slice` guesses "the first pointer and the
  first integer". Every view here is explicit about which members it reads.
- **Inferior calls.** Views never run code in the debuggee.
- **Whole views in WebAssembly**, as opposed to kernels (§3.13). A full wasm
  view would need all of §3.0 as a permanent binary ABI. Kernels need only
  `read` and `yield`.
- **For later, if users ask:**
  - a natvis importer for third-party libraries (imgui, EASTL, Godot,
    Unreal), translating into §3.3 rather than adding an engine;
  - reading LLDB formatter bytecode, if libc++ ships it.

### 3.11 Where views run (D9)

Views run on the controller thread, inside the existing inspection paths:
`variables`, `evaluate`, `value_children`, and conditions and log messages
at internal stops. There is no worker thread and no second execution site.

- **`src/view` is pure.** It holds the parser, binder, generators, kernel
  host, and summary formatter. It sits under the same boundary test as
  `src/eval`, and reaches programs only through the contract's traits,
  which `StopMachine` and the provider implement. The simulator therefore
  runs views exactly as the debugger does.
- **Budgets are the timeouts.** Every read, generator step, kernel
  instruction (fuel), and output node is charged to the request's
  `InspectionBudget`. A request costs milliseconds at most, and the same
  inputs always stop at the same point. No wall clock is involved, so runs
  stay deterministic.
- **Scheduling.** The controller's single FIFO loop
  (`backend/linux.rs:1078`) takes run-control requests (continue, step,
  pause, kill, detach) and waiter events before queued inspection requests.
  So a burst of hovers, watches, and locals never delays a step or the
  classification of a process event.
- **Cancellation.** A presentation checks between reads whether a
  run-control request is waiting. If one is, it stops with a stale result,
  as a request for an old `StopId` does today, and the run-control request
  proceeds.
- **Caches** live as long as their inputs:
  - bound views, per type and view-set generation, until the view set
    changes;
  - scan checkpoints, until the `StopId` changes.

  A per-stop page cache is added only if measurements show views re-reading
  the same memory.
- **View sources** are parsed and validated when loaded, outside the
  controller. The controller receives an immutable `Arc<ViewSet>`.

### 3.12 Keeping views working

The goal is that a library change never shows a user a wrong value, rarely
shows them raw, and is caught by uscope's gate rather than by users.

1. **Most presentation needs no library knowledge at all.** Enums and
   optionals; slices, `str`, and Go strings and slices; Zig sums and
   sentinels; and trait objects and Go interfaces (§3.1, §3.6) are DWARF
   semantics. They change only when DWARF does.
2. **Views name meaning, not paths, wherever DWARF allows it:**
   - `inner()` steps through wrappers;
   - element and key types come from template arguments, `go_key` and
     `go_elem`, or `typeof`;
   - patterns anchor on the crate or namespace root and base name, with
     `**` for intermediate modules.

   Of the 13 Rust std changes that broke natvis between 2023 and 2026, by
   our reading of each PR, 11 add, remove, or rename a wrapper or a module,
   which views written this way survive.
3. **The built-in views target the pinned toolchains** (D4, D7): the newest
   version of each supported compiler and library. An alternative for an
   older layout stays while it is cheap, but older toolchains are not
   promised. A user on one can override a view.
4. **The gate catches drift.** "Every built-in view binds" (§5.2) fails when
   a toolchain bump moves a field, with `info view`'s explanation, so the
   fix lands with the bump.
5. **Failure is visible, never wrong.** When nothing binds, the value is
   raw and `info view` says why, for example "alloc::vec::Vec:
   `inner(buf).ptr`: no member `ptr` in `RawVecInner`". A failed `check` is
   shown as the problem it is.
6. **Fixing does not wait for a release.** A user or project file
   overrides or extends a built-in view on the spot. A fix to a built-in
   view is a one-file change with a fixture marker.

### 3.13 Kernels: WebAssembly for algorithms

Some structures are algorithms more than layouts:

- Rust's `BTreeMap`, which walks nodes by height;
- Go's classic maps caught mid-growth, with evacuated and unevacuated
  buckets;
- open-addressed tables with tombstones;
- ECS archetype storage;
- a user's custom allocator.

The declarative language could grow loops, mutable variables, and recursion
to express them, as natvis grew `CustomListItems`. But that turns a
readable data format into a poor programming language. Instead, a view can
call a **kernel**: a WebAssembly function that reads memory and yields
items. Types, layout, and presentation stay in the declarative view:

```text
view rust alloc::collections::btree::map::BTreeMap<K, V, _> {
    type Leaf = alloc::collections::btree::node::LeafNode<K, V>
    show map(length) for e in kernel("btree", root.node, root.height, length,
                                     sizeof(K), sizeof(V), offsetof(Leaf, keys), …)
        => *(K*)e.0 : *(V*)e.1
}
```

**The ABI is core WebAssembly, not the Component Model.** The kernel module
imports exactly one versioned module, `uscope_kernel_v1`, which provides:

- `read(addr: u64, buf: i32, len: i32) -> i32`, which fills a guest buffer
  from debuggee memory and returns bytes read or a negative error;
- `yield(ptr: i32, n: i32) -> i32`, which emits one item of `n` u64 words
  and returns whether the host wants more.

It exports `run(args_ptr, nargs) -> i32`. Any other import is rejected at
link time, so WASI, clocks, randomness, and I/O are impossible by
construction. Start functions, threads, and SIMD are rejected, and floats
are refused by default.

**Why a kernel rather than a typed API:**

- Every run is a pure function of its arguments and the bytes `read`
  returned. A recorded run therefore replays offline as a unit test, in
  the simulator, or in a bug report.
- Two imports are trivial to implement and to keep stable, and any
  debugger could host them (§7).
- The Component Model is still Phase 1. Only wasmtime implements it.
  Zed's extension API carries ten versioned WIT directories.

**The runtime is wasmi 2.0** (research, 2026-10-05):

- 8 crates, +1.7 MB, a 10 s clean build;
- eager compilation, with fuel tied to wasm instructions, so budgets are
  deterministic;
- frame-counted recursion limits on heap stacks;
- NaN canonicalization;
- no signal handlers.

wasmtime is 6× faster, but it brings 74 crates and 11 MB, takes 10–15 ms to
compile each module, installs process-wide signal handlers, and can abort
on native stack overflow. Inside a ptrace debugger those are all worse than
the speed matters. View work is dominated by memory reads, and 10M fuel
costs about 3 ms on wasmi.

**Limits:**

- module 256 KiB;
- linear memory 4 MiB;
- recursion 1024 frames;
- fuel per presentation;
- `read` charged to `InspectionBudget`;
- `yield` bounded by the view's declared count.

Compile and run are wrapped in `catch_unwind`, and the store is discarded
on any trap or caught panic: wasmi 1.0.9 once panicked on a valid module
and took Typst down.

**Authoring:**

- An SDK crate and a C/Zig header make a kernel 15–30 lines.
- Measured sizes for a vector-like kernel: Rust 589 B, C 581 B, Zig 921 B.
- Go cannot target a pure ABI (it imports WASI).
- Kernels ship as `.wasm` beside a view file, or as kernel records in
  `.debug_uscope_views`.
- A kernel record carries its source or a source link, so reviewers are
  never asked to trust an opaque blob.

**Sequencing.** Kernels come after the declarative phases, when the first
built-in view genuinely needs one. The candidates are `BTreeMap` and
classic Go maps (§4, P7). The contract is designed now so they fit without
change (D8).

### 3.14 Robustness model

A view can make its own value wrong only by being visibly wrong, and can
never affect anything else.

| Property | Mechanism | Test |
|---|---|---|
| No side effects | The contract has no writes, calls, or I/O; kernels can import only `read` and `yield` | import validation; the boundary test on `src/view` |
| Bounded work | One per-request budget charges reads, generator steps, kernel fuel, and output; scans may not pass their count | fake-world budget tests; hostile fuzzing |
| Bounded memory | Budget caps on output and text; kernel memory 4 MiB; view files 256 KiB | the heap cap in every test process |
| Run control comes first | Priority scheduling and cancellation (§3.11) | scenario: `continue` during a large presentation is acknowledged first, and the presentation ends stale; `just stress` |
| Contained failure | A bind error, failed check, cycle, trap, or exhausted budget makes *that value* raw with a typed problem. Engine panics are caught at the presentation boundary, recorded by the flight recorder, and reported as an internal problem | sabotage tests; simulator fault injection |
| Deterministic | Budgets and fuel, never clocks | `a_seed_always_names_the_same_run` |
| Never convincingly wrong | Binding is static and total; `check`s guard invariants; an incomplete type is "unavailable", never zero | the failures of §1.4 as regression tests |

**Hostile fuzzing** is the "nigh impossible" assurance. A contained fuzz
target runs every built-in view, and random view files, over random
memory:

- garbage pointers;
- cycles;
- huge counts;
- unmapped pages.

Every presentation must end in a value or a typed problem within its
budget, never panic, and never allocate past its cap. Like the existing
fuzzers, it runs only through `scripts/contained.sh`.

### 3.15 Authoring, contributing, and the built-in library

**The built-in library is plain view files in the repository**, under
`views/`:

- one per library: `libstdc++.views`, `libc++.views`, `rust-std.views`,
  `go-runtime.views`, `zig-std.views`;
- each opens with a header naming the library, the toolchain versions it
  is verified against, and its fixtures;
- uscope compiles them in, and a user file with the same pattern shadows a
  view, or `extend`s it.

**Tests live in the fixtures, as markers.** A fixture line states what the
view must show:

```cpp
std::vector<int> ints = {1, 2, 3};   // VIEW: ints => len=3 [1, 2, 3]
```

Contributing a view is therefore two edits: the view, and one marked line
in a fixture.

**`uscope views check` binds views statically against a binary**, with no
process. Binding needs only debug information. For every matching type in
every module, it reports which view binds, which alternatives fail, and
why. It serves:

- contributors before they open a PR;
- users writing views for their own types;
- the "every built-in view binds" gate check (§5.2).

`uscope views explain TYPE` and `info view EXPR` give the same answer
interactively.

**The view language is versioned.** A file starts with `uscope-views 1`.
Once released, a version's meaning never changes, so a file written today
keeps working.

**Documentation.** `docs/views.md` is the reference, and every example in
it runs as a test. `docs/writing-views.md` is a tutorial built from real
fixtures: a C vector, a C tagged union, an intrusive list, a Rust newtype
collection, and a kernel.

## 4. Phases

Each phase is test-first, lands on `next` on its own, and is reviewed
(`/roast`) before landing.

- **While iterating:** targeted tests only.
- **At the end of each phase:** `just` and `just sim 60`.
- **Once, at the end of the project:** `just all`, `just stress`, and
  `just sim 600`.

**P1 Compliant DWARF and type identity.** This phase is in the provider and
changes little that users see. *Done 2026-10-05.*

- [x] §3.1 fat pointers by shape, which fixes `*const [T]` at `-O`.
- [x] Zig `[]const u8` text, and text charged to the budget.
- [x] §3.2 identity:
  - [x] the language;
  - [x] the path, with `DW_AT_export_symbols` collapsing;
  - [x] template arguments, including packs;
  - [x] name parsing where DWARF has no parameters;
  - [x] Go's attributes.
- [x] The per-image identity index.
- [x] libc++ in the dev shell (D4).
- [x] `ptype` shows template arguments.

What P1 built, and what it learned:

- **Where it lives.** `TypeInfo.identity` holds a `TypeIdentity`
  (`src/model.rs`). `src/type_identity.rs` is the neutral part: the name
  parser for the three syntaxes (`a::b<T>`, Go's `pkg.T[A]`, Zig's
  `mod.T(A)`), argument matching, and the per-image `TypeIndex`. The DWARF
  side is `variables/identity.rs`. `ModuleImage::type_instances`,
  `types_named`, and `same_type` are the index's API.
- **Fixtures.** `cpp/templates.cpp` (gcc, gcc DWARF 4 type units, clang,
  clang with libc++), `rust/generics.rs` (-O0 and -O2), `go/generics` (both),
  `zig/generics.zig`. Tests: `tests/debugger/identities.rs`, and
  `ptype_shows_qualified_names_and_template_arguments` in `tests/cli.rs`.
- **libc++** is `clang++-libc++` in the dev shell, a wrapper around the
  shell's clang with nixpkgs' libc++ 21.1.8.
- **Rust fat pointers.** rustc emits every pointer to a slice or `str` as a
  structure `{data_ptr, length}` outside every module, whatever it names it.
  A pointer to a type with an unsized tail (`&Path`, `&CStr`,
  `*const RcInner<str>`, a user's DST) has the same shape, but its data
  pointer targets the whole type and its length counts the tail, so it stays
  a record; P4's unsized tails take it from there. `TypeKind::Slice` gained
  `text` for Rust's `str` and Zig's `[]const u8`, `[:0]const u8`, `[:0]u8`.
  Zig's LLVM backend has no `DW_AT_ZIG_sentinel`, so `[*:0]const u8` is
  recognized by Zig's own spelling.
- **Arguments.** rustc describes type parameters but omits const ones, so
  the name's integers fill the positions the parameter DIEs skip. GCC omits
  `std::allocator<T>`'s parameters; its name is parsed and each argument
  resolves only when the types it could name share one identity. Go maps,
  channels, slices, arrays, and pointers take their arguments from `go_key`
  and `go_elem`; Go generics and Zig instances are parsed.
- **Inline namespaces.** The identity's path omits them, and
  `TypeIdentity.inline_namespaces` keeps them so a name may spell them
  (`outer::v1::Thing`). GCC copies namespaces into type units, DWARF 4 and
  5 alike, without their `DW_AT_export_symbols`, so a namespace is inline
  when any unit marks the namespace with the same full path inline. The
  name list (§3.2) is only for producers that mark nothing:
  `std::__debug::vector`, libstdc++'s checked vector, is a different type
  from `std::vector` outside debug mode, and `templates.cpp` holds one.
- **Anonymous namespaces** are each unit's own, so a type in one, and an
  instance over it, has an identity no other unit's type shares.
- **Types nested in records.** GCC declares the qualified types a class's
  methods use inside the class, which made libstdc++'s classes opaque; any
  type entry nested in a record now only scopes it.
- **Type names in expressions** resolve through the index: outer path
  segments and trailing (defaulted) arguments may be omitted, as in
  `` std::`vector<int>` ``. Go emits same-named typedefs over its named
  types, and a synonym of another candidate with the same name is that
  candidate. A dotted Go name in `sizeof` still binds as a value path, so it
  needs backticks; the binder is unchanged.
- **Budgets.** Text reads are charged. Text the budget cannot afford ends
  in `TextCompletion::Limited` without exhausting the inspection, so the
  values after it are still read. The default per-operation limits rose to
  64 KiB and 256 reads (§3.9); the per-top-level-value budget remains P2.
- **The index is built at load**, not lazily: resolving parsed arguments
  needs it, and building it measured no load-time or memory difference on
  Rust-with-std, Go, Zig, and C++ binaries.
- **For P3.** The type graph holds only types reachable from data and, now,
  from template arguments and Go's attributes. A type reachable only through
  member functions, such as libstdc++'s `_Rb_tree_node<V>`, is not built,
  so the index cannot find it yet. P3 must make such types reachable before
  map views can name their nodes.
- `ptype` qualifies C, C++, and Rust names with their path, says `class` for
  classes, and ends with an `arguments:` line.

**P2 The engine and contiguous shapes.** *Done 2026-10-05.*

- [x] Prerequisite found in research: a member name is found through C++
  base classes and C/C++ anonymous members (`v._M_impl`, libc++'s
  `__cap_`), virtual bases included. The expression fixtures check it
  against the compilers.
- [x] Prerequisite: the evaluator's view dialect: `inner()`, values a scope
  binds once (`let`), and constants (captured values).
- [x] The `src/view` parser and binder (`or`, types, checks).
- [x] Random-access sequences, `text`, `value`, `empty`, `if`, and fields.
- [x] The `Presentation` model, the view children reference, and `[raw]`.
- [x] Scheduling and cancellation (§3.11).
- [x] The hostile fuzz target (`just fuzz views`), with a proptest of the
  same harness in the suite.
- [x] CLI `print`, `print/r`, `info view`, `set views on|off`; DAP counts,
  `filter`, and hints.
- [x] `v[i]` and `len(v)` through views, including assignment to an
  element and `&v[i]`.
- [x] Built-in views that replace `string_parts`:
  - [x] C++ strings: libstdc++, the old ABI, and libc++ in short and long
    forms;
  - [x] Rust `String`, `Box<str>` (text at layer 1 since P1), `PathBuf`,
    `OsString`, `CString`.
- [x] Further built-in views:
  - [x] C++ `std::vector` (not `<bool>`), `std::array`, `std::span`,
    `std::string_view`;
  - [x] Rust `Vec`, `VecDeque`;
  - [x] Zig `ArrayList`, `ArrayListUnmanaged` (0.16's `array_list.Aligned`
    and `array_list.AlignedManaged`).
- [x] `containers` fixtures per language with `VIEW:` markers, corrupted
  instances, and "every built-in view binds" (`tests/debugger/views.rs`).
- [x] `docs/views.md`, with executable examples.
- [x] End of phase: `/roast` (two findings, both declined: the provider's
  own reads are bounded and were never interruptible, which §3.11 does not
  ask for; `print` shows a sequence's elements, not its fields, by
  design), `just`, `just sim 60`, and 15 minutes of `just fuzz views`
  (one overflow found and fixed).
- [x] The long run, since scheduling changes run control: `just all` and
  `just sim 600`. `just all` passed (886 tests, stress 10 of 10), and the
  sweep ran 3,375,451 sessions in 600 seconds without a failure.

What P2 built, and what it learned:

- **Where it lives.** `src/view` is pure, under the same boundary test as
  `src/eval`: `syntax` (view files), `pattern` (matching identities),
  `bind` (a view scope over a module's types, and binding), `run` (a view
  machine over the evaluator's `Machine`), `summary` (the neutral style,
  which the CLI and DAP now render values with too), and `fuzz`.
  `src/backend/linux/presentation.rs` is the glue: the view each type has,
  cached per view set, presentations for every inspection path, and view
  children. The built-in library is `views/{libstdc++,libc++,rust-std,
  zig-std}.views`.
- **The evaluator** gained the view dialect (`Expression::parse_view`,
  `inner()`), values a scope binds once (`Lookup::Bound`, `Machine::bound`,
  `interp::value`, `bind_value`), constants (`Lookup::Constant`), and
  `Machine::presented_length`. A record is indexed through
  `Scope::plan(Index)`, which a frame answers with its view, so `v[i]` is a
  place: it can be assigned and its address taken.
- **Member lookup** now finds names through base classes and anonymous
  members, as C and C++ do, with a virtual base reached along two paths
  being one object. libstdc++'s `_M_impl` and libc++'s `__cap_` need it.
- **The model.** `VariableState::Available` gained `presentation`, and its
  `children` stay the stored value's. A presentation's own `children` are
  its elements, fields, and `[raw]` (`ValueChildRelationship::{Element,
  Field, Raw}`, `ValueChildrenReference::elements()`); a `value` view
  lends the children of the value it presents. A view that fails is
  `PresentedShape::Raw` with its `ViewProblem`. A text view also sets the
  state's `text`, so the string tests stayed unchanged, and a pointer or
  reference to such a value carries the text, as the provider did before.
- **Binding.** `let`s and `type`s bind in order; checks, fields, the
  summary, and the shape see them all. A check of `a && b` is two checks,
  so a failure names the comparison that failed and its sides (`check
  \`end <= storage_end\` failed: \`end\` is 0x…24, \`storage_end\` is
  0x…10`). A capitalized pattern argument captures a type or a value;
  `std::span`'s extent and `std::array`'s size are captured values.
  Patterns anchor at the root, and `**` spans any run of segments.
- **Budgets.** A presentation runs on `InspectionBudget::share()`, a
  quarter of what remains, and its usage is then absorbed, so running out
  ends a summary early without failing the inspection. A page of children
  ends at the first child its budget cannot afford.
- **Scheduling (§3.11).** `next_message` serves messages in arrival order,
  except that one that reads one stop (`reads_one_stop`) waits behind run
  control or a wait event queued after it (`preempts_inspection`), and so
  fails as a request for an old stop does. Expression evaluation and
  presentation in such a request check every 64 units of a frame's work,
  counted across the machines of views nested in one another, whether run
  control is waiting; if it is, the request is served again after it
  (`serve_later`). The provider's own reads of a frame's variables, bounded
  by the budget as before P2, are not interrupted. Conditions and log
  messages, which run control evaluates itself, and assignments, including
  reading the target again after the write, are never interrupted. The live, post-mortem, and simulated controllers share
  `next_message`. The DAP session handles requests one at a time and
  already cancels queued inspection on a resume, so the scheduling serves
  pipelining clients.
- **Library facts the views rely on.** The old ABI's string keeps its
  length and capacity in a three-word header before its characters, which
  its DWARF does not describe. libc++ stores a long string's capacity
  halved on little-endian targets. libc++'s empty `std::array` is presented
  by libstdc++.views's view, which reads nothing. Zig 0.16 names
  `std.ArrayList` `array_list.Aligned` and the managed list
  `array_list.AlignedManaged`. Rust's `ManuallyDrop` now wraps
  `MaybeDangling`; wrappers are P4's.
- **Found on the way.** Clang at `-O2` emits a nameless subprogram with no
  code only to scope a function's local types (libstdc++'s `_Guard`); the
  loader refused every such binary, and now ignores these.
- **Tests.** `tests/debugger/views.rs` runs the `containers` fixtures'
  `VIEW:` markers over gcc and clang with libstdc++, libc++, the old ABI,
  Rust, and Zig, at `-O0` and `-O2`, checking summaries, children, pages of
  any size, element names that evaluate back, `[raw]`, corrupted instances,
  and that every built-in view binds; session views; and inspection sent
  beside run control. `tests/cli.rs`, `tests/dap/variables.rs`, the
  scheduling unit test, the views' fake-world tests, and the reference's
  examples cover the rest.
- **After the end-of-phase review** (a second `/roast` of cb735700): an
  assignment's read of its target after the write could be interrupted
  and answered with `Interrupted`; the interrupt interval reset in every
  nested view's machine; a pointer to a value whose text view failed
  dropped the reason; `p - q` truncated pointers not a whole number of
  elements apart, so a corrupt `std::vector` showed a plausible length; and
  `Vec<()>` and `VecDeque<()>` did not bind. That last one led to three
  older bugs: the loader rejected Rust's `()`, a zero-byte base type, as
  malformed, so any value holding it, such as `Ok(())`, failed to print;
  the binder refused to index a pointer to a zero-sized type; and the
  console's one-line summary called every Rust variant `<no matching
  variant>`, because Rust names a variant by its one member. `()` is now an
  empty structure, summarized `{}`.
- **Deferred.** Text is read up to 256 bytes everywhere, as for C strings;
  §3.9's 4096 bytes when printed needs a per-request text limit. Views do
  not index values through other views inside a view. A page of 256
  children slightly exceeds the default 256 reads, so a client asking for
  more than about 250 elements at once gets a truncated page (VS Code asks
  for 100). Session view files from the CLI and DAP are P5;
  `DebuggerHandle::load_views` exists for them.

**P3 Scans and maps.** *Done 2026-10-05.*

- [x] `list`, `inorder`, filters, nested generators, checkpoints, and cycle
  detection.
- [x] Map presentation.
- [x] Built-in views:
  - [x] C++ `std::map`, `set`, and the multi- forms; `unordered_*`;
    `std::list`, `forward_list`, `deque`;
  - [x] Rust `HashMap`, `HashSet`;
  - [x] Go maps;
  - [x] Zig `HashMap`, `ArrayHashMap`.
- [x] §5.4: the `containers` golden program, its views, and the views
  oracle, with a sabotage and coverage marks.
- [x] End of phase: `/roast` (two findings, both fixed: the fake world ran
  out of work as `EvaluationLimit`, which the real controller never reports
  for a budget, so the runner had treated a provider's evaluation limit as
  the budget; and the simulated client paged by the size it asked for
  rather than the children it got), `just` (908 tests), and `just sim 60`
  (368,884 sessions).

What P3 built, and what it learned:

- **The language.** Generators nest up to four deep; after each come, in
  order, `if` filters and `let`s computed once per value, which views of
  nested tables need (the first Go map view re-read
  `dirPtr[d]->groups.data[g]` for every slot, five reads each, and ran out
  of 1024 reads by its 118th entry). `map(COUNT) … => KEY : VALUE`.
  `offsetof(TYPE, member)` reaches a record's layout (Zig's
  `MultiArrayList` keeps each field's array in the order of the entry
  type's DWARF members). `type T = typeof(EXPR).Name` is a type declared
  inside another (Zig's `Header`). A `type` with arguments, such as
  `std::_Rb_tree_node<Value>`, is constructed through the type index by
  matching the pattern with the view's captures and types already bound,
  so it compares identities, never spellings. Go patterns name kinds:
  `go map<K, V>` presents every map, named or not. `let` now ends an
  expression, like `or`, `for`, `if`, and `else`.
- **The engine.** `src/view/scan.rs` holds the scan: a plain-data
  `Cursor` per value (each clause's generator state and variable, Brent's
  state, the elements generated), kept every 256 elements in
  `Checkpoints`, which the controller caches per stop and forgets on a new
  stop, a write, or a new view set (`presentation::Views`). A random-access
  sequence, one `range` with no filter, never scans.
- **The model.** `PresentedShape::Map`, `PresentedCount::AtLeast` (a
  sequence without a count the budget cut short: `len>=40`),
  `ValueChildRelationship::Entry { index, key }` with a `MapKey`, and
  `ViewProblem::{Cycle, TooDeep, TooMany}`. The console prints `len=2
  {"one": 1, "two": 2}` and expands entries; the DAP names an entry by its
  key, counts entries as indexed, and gives its value the evaluate name
  `*(T*)ADDRESS`, which `setVariable` writes through.
- **Presentation glue.** A value whose own type a view presents need not
  be an aggregate: a Go map is a typedef of a pointer, and choosing a view
  now tries each type along a typedef chain, outermost first, and presents
  pointers too (`len(m)` binds through `Scope::has_view`). A value view of
  a sequence or map (Zig's managed `HashMap`) takes on that shape and
  count, so clients page its entries. Only a top-level presentation takes a
  quarter of the budget; the values inside it share that share.
- **Library facts the views rely on.** libstdc++'s trees, lists, and hash
  tables are reached through node types their allocators' arguments name;
  the old ABI's `std::list` keeps no size. libc++'s deque keeps no block
  size in the object or its DWARF (the static member has no value): the
  view states libc++'s rule, 4096 bytes of elements or 16 elements of 256
  bytes or more, and checks it against the map of blocks. Rust's hashbrown
  table keeps buckets below its control bytes, a full one's top bit clear.
  Go 1.26's swiss maps keep a small map's single group in `dirPtr`, and a
  table's first directory slot in its `index`. Zig 0.16's array hash maps
  are unmanaged only; ReleaseSafe emits no entry type for them, so they show
  raw with the reason there.
- **Found on the way.** `false` and `true` template arguments never matched
  the bool arguments DWARF gives, and GCC's `int const` never matched a
  `const int` type, so `allocator<_Hash_node<pair<int const, int>,
  false>>`'s argument stayed unresolved; both now match. Types only inlined
  code uses (an abstract instance's return and variable types, such as
  Zig's `Header` at ReleaseSafe) were never built; they are now, which on
  an eight-unit `-O2` C++ program added 26% more types and 3% more load
  time. A page whose budget ran out while a filter looked for its next
  element threw away the elements it had found, so a sparse hash table's
  page came back empty; it now keeps them, and a summary shows
  `<unavailable>` where it stopped.
- **The simulator.** `tests/golden/containers` holds a vector, a list that
  every odd round leaves cyclic and overcounted, and an open-addressed
  table, all globals, with `containers.views` beside them, which the
  corpus loads and the client loads into its session. At half its
  inspections the client evaluates each container and reads its elements
  in one page and in pages of a drawn size. The views oracle
  (`src/sim/views.rs`) walks memory by the program's own C layouts, never
  uscope's DWARF, and requires each element's address and bytes, the
  count, both pagings, and the typed cycle or count problem to match. Its
  sabotage, `SkipLinkedNodes`, makes ptrace skip a node, and the marks
  `ViewPresented`, `ViewPaged`, and `ViewCycleRefused` are reached by the
  gate's seeds.
- **Tests.** The `containers` fixtures in all four languages, with Go new
  (`go/containers`), carry `count:` and `(any order)` markers and corrupted
  lists; the harness pages through children with checkpoints and evaluates
  each entry's place back. Unit tests cover lists, rings, cycles behind
  checkpoints, trees too deep, filters, nesting, budgets that end anywhere,
  construction, `offsetof`, nested types, and the parser; the hostile
  harness gained a libstdc++ list and map, a Rust `HashMap`, a Go map, and
  a Zig hash map; `tests/cli.rs` and `tests/dap/variables.rs` cover printing
  and paging maps.
- **Left for later.** A cycle that leads back past the checkpoint a page
  resumed from is found by Brent's algorithm within a few times its length,
  so pages between may repeat nodes before the page that reports it. Each
  node link is its own read, so a tree costs about four reads an entry; a
  read cache waits for a measurement that the budget, not the reads, is
  what users hit. uscope cannot load a 30-unit `-O2` C++ program at all: it
  passes the 262,144 data objects the DWARF loader allows, with or without
  views.

**P4 Pointers, sums, and dynamic types.** *Done 2026-10-06.*

- [x] Rust `Box`, `Rc`, `Arc`, `Weak`, `Cell`, `RefCell`, `Mutex`.
- [x] C++ `unique_ptr`, `shared_ptr`, `weak_ptr`, `optional`, `variant`,
  `tuple`.
- [x] §3.6 vtables (C++ and Rust `dyn`); Go interfaces and `error`.
- [x] Go channels.
- [x] Zig's LLVM-backend optionals, error unions, and tagged unions.
- [x] End of phase: two `/roast` passes, one over all 41 files and an
  interactive one over the engine, found nothing (each refuted one
  candidate); a review of my own found that resolving a spelled pointer
  scanned every type once per argument, which now uses an index built once.
  `just` (909 tests) and `just sim 60` (362,739 sessions) passed.

What P4 built, and what it learned:

- **Layer 1 without a view.** When no view binds, the controller presents
  what debug information and the program's own tables say exactly
  (`PresentedShape::Dynamic`, and sums as `Value` or `Empty`, named by the
  view `uscope`):
  - a sum as its active variant: `Some(4)`, `Err("no")`, `Square {side:
    4}`; a Zig optional's or error union's payload is itself, and its
    other states `null` and `error.Bad`;
  - a C++ object of a polymorphic class as the complete object its vptr
    belongs to: the vptr must point into a `vtable for X` symbol, and the
    offset-to-top before it must lead to an object whose own vptr is that
    group's primary address point, 16 bytes in, so `Shape *` into a `Tile`
    shows `Tile {tag: 2, id: 7, side: 3, row: 9}`. Both demanglers'
    spellings, `vtable for X` and `{vtable(X)}`, name it;
  - a Rust trait object through the `<C as Trait>::{vtable}` variables,
    whose type's `DW_AT_containing_type` is `C`;
  - a Go interface through `runtime.types` and `DW_AT_go_runtime_type`,
    stored directly when Go 1.26's `TFlag` bit 5 (or `Kind_` bit 5 before)
    says so: `int 42`, `main.Point {X: 1, Y: 2}`, and, as Delve shows an
    `error`, `*errors.errorString *{s: "bad"}`.

  A base-class subobject is never presented as its complete object, which
  would recurse. Clang names some thunks only by their linkage names; the
  loader refused those binaries and now names them by the demangled name.
- **The language.** `record { NAME = EXPR, … }` presents chosen members as
  children before the view's fields, and positions make a tuple, `(1, 99
  'c', 2.5)`; `dynamic(PTR, arg(TYPE, EXPR))` presents what a pointer
  points to as the type argument at a position the program's data holds,
  which `std::variant` needs. Expressions gained the C++ upcast: `(Base)x`
  is `x`'s `Base` subobject, as `static_cast` makes it, and ambiguous when
  there are several; libstdc++'s tuple elements need it, because every
  element is a `_M_head_impl` of its own base.
- **Packs.** A type identity records where its C++ parameter pack begins,
  and a pattern that reaches the pack spells all of it, so `std::tuple<A,
  B>` names only pairs and `std::variant`, with no arguments, every
  variant. GCC emits an empty pack for some instances, such as
  `unique_ptr`'s `tuple<int*, default_delete<int>>`, so their arguments
  come from the name; a pointer spelled there, `int*`, now resolves to the
  pointer type to what its target spells.
- **Views.** libstdc++ and libc++ `unique_ptr` (not of an array: `sizeof(T)`
  of `T[]` does not bind), `shared_ptr` and `weak_ptr` with their counts
  (an expired `weak_ptr` is `expired`, never its destroyed object),
  `optional`, `variant` (and `valueless`), and tuples of up to six; Rust
  `Box`, `Rc`, `Arc`, both `Weak`s (`dangling`, `dropped`), `Cell`,
  `RefCell` with its borrow count, and `Mutex` with its lock and poison
  flags; Go channels, whose ring starts at `recvx`. A Go channel is a
  pointer only in representation, so `ch[i]` indexes its view.
- **Lending.** A value view lends the value's children but no longer its
  `[raw]`: a presented value has one `[raw]`, its own and last.
- **Tests.** The `containers` fixtures gained every new type, empty and
  populated, an expired `weak_ptr`, a valueless and a corrupt `variant`
  (index 5), every tuple arity, polymorphic objects, Rust sums and trait
  objects, Go interfaces and channels, and Zig sums, with a new `children:`
  marker that checks every child. A libc++ `-fstandalone-debug` build joins
  the matrix (from P6): only there does clang describe a `shared_ptr`'s
  control block. The fake world gained base classes and packs, the
  references' examples cover upcasts, `record`, and `dynamic`, and the
  hostile harness covers `optional`, `variant`, a tuple, and a channel.
- **Left for later.** Unsized tails (`Rc<str>`, `&Path`) show as stored:
  rustc describes `str` and `[u8]` alike, as an array of `u8` with no
  count, so nothing says which one is text. Tuples of more than six, an
  `Rc<dyn Trait>`, and a libc++ `weak_ptr` without `-fstandalone-debug`
  show as stored. A Go interface's pointer to anything but a struct shows
  its address.

**P5 User and embedded views, and the authoring tools.** *Done 2026-10-06.*

- [x] Session, user, and project files, and the DAP `viewFiles` launch
  argument.
- [x] `.debug_uscope_views`, with its C header and Rust macro, and module
  scoping.
- [x] `extend`, `hide`, `format`, `match`, `record` (in P4),
  `container_of`, `global`.
- [x] `uscope views check` and `views explain`.
- [x] `docs/writing-views.md`.
- [x] End of phase: `/roast` of each of the three commits, whose seven
  confirmed findings were fixed (`container_of` located objects through
  pointers of any type; `utf16` assumed little-endian; the CLI found
  project views where uscope ran rather than where the program does; an
  embedded record could be any size; `views explain` passed despite a
  broken view file and skipped the awaited shutdown on an error; and an
  advertised `views explain` came only with the third commit); `just`
  (919 tests) and `just sim 60` (344,060 sessions).

What P5 built, and what it learned:

- **The language.** `match` is an `if` chain, `(EXPR) == (VALUE)` for each
  arm, so an enumerator binds next to the value it is compared with, and
  ends in a problem that names a value no arm does. A view without a `show`
  presents a record's members, its bases as members named by their types
  (through the P4 upcast), or any other value as itself. `hide` and
  `format` act on a value's named children, and are checked when the view
  binds: a name that is no member or field, or a format that does not suit
  what it writes, keeps the view from binding. Formats are `hex`, `char`,
  `bytes`, `utf16` (arrays), `flags(E)`, `enum(E)`, and `duration(UNIT)`,
  written as Go writes durations. An `extend` is bound in a scope of its
  own and attached to whichever view binds, or to a show-less
  presentation of the members when none does. `container_of` checks that
  its pointer points to the member's type, or is a `void *`: without that,
  a mistyped view located the wrong object. `global(NAME)` reaches the
  value's own module's globals, through a step that ignores its base.
- **Sources.** A type's view is chosen among the session's files, the
  project's `.uscope/views` (where the program runs) and the user's, the
  views its own module carries, and the built-in ones, in that order. The
  client keeps the files and the controller only the parsed set; a module's
  views are parsed with its debug information. The record format is a
  kind, a format, a 32-bit length, and the bytes, padding skipped, each
  record no larger than a view file. The C header and the Rust macro emit
  the section through `.incbin` in assembly, since a C string cannot carry
  view text into `asm` and Rust's `global_asm!` reads braces; labels avoid
  `0` and `1`, which Intel syntax reads as binary.
- **Tools.** `views check` and `views explain` are controller requests that
  reuse the view choices of a live session, which hold before the program
  runs, so `uscope views check PROGRAM` needs no process: 0.3 seconds for
  the C++ containers fixture. It fails only for views given to it or
  carried by the program, never for a built-in view that cannot present a
  type such as `unique_ptr<int[]>`, which it still reports.
- **A presented value has one `[raw]`.** P4 kept a lent value's own
  `[raw]` out; a field that cannot be computed is now that child's
  problem, as an element's is, rather than the whole page's.
- **Tests.** The `embedded-views` fixtures, a C program and library and a
  Rust program, check scoping and both writers; `tests/cli.rs` checks the
  sources' order, `--cwd`, `views load` and `clear`, and `views check` and
  `explain`; `tests/dap/variables.rs` checks `viewFiles` and its errors;
  the reference's examples cover every new statement and shape; the
  `tutorial` fixture's markers and a test that the tutorial quotes its
  fixtures verbatim keep `docs/writing-views.md` true. `just test` and
  `just stress` give tests an empty user configuration.
- **Left for later.** `views explain` names types as `types_named` does,
  so a typedef and its target are separate entries. A library's views are
  checked by `views check` only in a session that has loaded it.

**P6 A wider matrix.** *Done 2026-10-06.*

- [x] The Zig self-hosted backend.
- [x] `-gsimple-template-names`, `-fstandalone-debug` (in P4),
  `_GLIBCXX_DEBUG`, `_GLIBCXX_USE_CXX11_ABI=0` (in P2), `-static-libstdc++`.
- [x] §5.3's Rust `-C debuginfo=limited`.
- [x] End of phase: `/roast` (one finding, fixed: a cyclic or overlong
  parent chain gave a fabricated name, and now fails the type), `just`
  (920 tests), and `just sim 60`.

What P6 found:

- **Zig's own backend** (`containers-zig-self-hosted`) differs from LLVM's
  in three ways, each now read as what it means:
  - it names a type declared inside another by its own name and says
    which in `DW_AT_ZIG_parent` (0x2ccd), so `Header` and even
    `containers.Shape` lost their qualification; a type with that
    attribute is named through its parents, as the LLVM backend spells it;
  - its optionals, error unions, and tagged unions are variant parts whose
    variants are `null` and `?`, and `value` and `error`, with errors named
    without `error.` and `void` and `@TypeOf(null)` as unspecified types;
    the sums of P4 read these too, so both backends show `5`, `null`, `7`,
    `error.Bad`, and `none`;
  - `?*T` and `?[*]T` are variant parts whose discriminant is the
    pointer's own bits; the loader makes them the pointers they are, as
    the LLVM backend describes them, so the hash maps' views bind.

  It emits no entry type for an array hash map, as ReleaseSafe does not,
  so those maps show as stored in both.
- **libstdc++'s debug mode** keeps `list` and `forward_list` nodes in
  `std::__cxx1998`, which the views now also name; everything else bound
  unchanged. The fixture corrupts containers through `_M_base()` there,
  since a debug-mode container begins with its safe-sequence bookkeeping.
- **`-static-libstdc++` and `-gsimple-template-names`** needed nothing:
  identities come from template parameter entries, not names.
- **Limited debug information** describes no variables, so nothing is
  presented, and uscope says no variable has the name.
- **Tests.** The C++ matrix has eleven builds and the Zig one three, every
  marker checked in each; `uscope views check` (P5) found each gap in a
  third of a second before any process ran.

**P7 Kernels.** *Done 2026-10-06*, for Rust's `BTreeMap`.

- [x] The wasmi host and the `uscope_kernel_v1` ABI.
- [x] `kernel("NAME", ARG, …)` generators, and the built-in `rust-btree`
  kernel with the `BTreeMap` and `BTreeSet` views.
- [x] The SDK crate and the C/Zig header.
- [x] Kernel records in `.debug_uscope_views`, and kernels beside view
  files.
- [x] Replayable recorded runs.
- [x] End of phase: `/roast` (one finding, fixed: a recording could hold
  arguments, reads, and items no host gives or takes, and replay read a
  file of any size), `just` (926 tests), and `just sim 60` (313,759
  sessions).

What P7 built, and what it learned:

- **The host.** A kernel is loaded once, eagerly compiled by wasmi with
  floats, SIMD, multiple memories, and start functions refused, wasmi's
  strict parsing limits, and 1024 frames of recursion; its imports must be
  exactly `read` and `yield` with their types, and it must export `run`
  and a memory that starts within 4 MiB. Each run is a new store, limited
  to 4 MiB, with the arguments in a page grown after the kernel's own
  memory. `read` and `yield` stop the kernel as resumable host traps, so
  the store never holds the program: the scan answers each call, charging
  reads to the inspection's budget, and the kernel runs out of fuel every
  4096 instructions, which the scan pays for as 512 units of work, so a
  spinning kernel ends with its budget, deterministically, and run
  control interrupts it as it does any view. wasmi's calls are wrapped in
  `catch_unwind`, and a run that traps or panics is ended.
- **The language.** `for key, value in kernel(…)` names a variable for each
  word of an item, so an item's width is checked against the view's and
  a mismatch is a problem. A kernel is found in the view's own source
  first, then among the built-in ones; a missing one keeps the view from
  binding. Arguments bind as integers, pointers, or truth values. A scan
  with a kernel keeps no checkpoints: a kernel's state is its memory and
  its stack, which wasmi cannot copy, so a later page runs the kernel
  again, and skips. Within one scan the run is kept live.
- **`rust-btree`.** Rust's leaf node is reordered by rustc (values before
  keys), and its length is a `u16`; the view passes every offset and size
  from the debug information, the root through `root.Some.0`, and 0 for an
  empty map, whose root may be `None` or an emptied leaf. The kernel, in
  Zig, walks with an explicit stack of 64 frames, refuses an overfull node
  or a null edge, and is checked in as `views/kernels/rust-btree.wasm`
  (871 bytes) beside its source; `just build-test-programs` rebuilds it
  and fails unless the module is identical.
- **SDKs.** `sdk/c/uscope_kernel.h`, `sdk/zig/uscope_kernel.zig`, and the
  `uscope-views` crate's `kernel` module, for `wasm32-unknown-unknown`, write
  kernels; `USCOPE_KERNEL` and `uscope_kernel!` embed them with their
  source. The dev shell's nightly Rust builds a `no_std` wasm kernel with
  `-Zbuild-std=core,panic_abort` and no other toolchain, once the host's
  linker flags are cleared.
- **Sources.** A kernel record is kind 2, format 1: the name, the source,
  and the module, each length-prefixed. A view file's kernels are the
  `NAME.wasm` files beside it that its views call. `views check` lists
  every kernel with its source.
- **Recordings.** A run is its arguments, each read's address and bytes,
  each item, and how it ended. `views record FILE EXPR` presents a value
  and its first page of children with every kernel run recorded, and
  `uscope views replay FILE [--kernel K.wasm]` replays them, reporting the
  first event that differs. A run its host stopped, because the view had
  its count or the budget ran out, replays as far as it went.
- **A bug it exposed.** A budget that ran out while presenting a Rust
  enum, past a view's share, failed a whole listing of locals; such a
  value is now missing for that reason, as any value is.
- **Tests.** Hand-assembled modules check what loads, deterministic fuel,
  traps, recursion, bad items, and recording; a model check walks fake
  B-trees of heights 0 to 3; the Rust containers fixture's markers cover
  empty, emptied, small, three-level, `String`-keyed, set, and overcounted
  maps in both builds, paged in sizes 7 and 256; the tutorial's C tree
  and the Rust embedded program's tree are walked by kernels written with
  each SDK; `tests/cli.rs` covers kernels beside view files, recording,
  replay, and `views check`; and the hostile harness has a `BTreeMap`, so
  proptest and the fuzz target run the kernel over arbitrary memory.
- **Left for later.** Classic Go maps are gone from the pinned Go 1.26,
  which has only swiss tables, so no kernel walks them. The simulator runs
  no kernels: its golden programs have no type that needs one. Paging deep
  into a large `BTreeMap` costs the reads of every page before it.

## 5. Testing

### 5.1 Pure

- **Parser.** Happy and sad paths with spans. A contained `view_parse` fuzz
  target is run only through `scripts/contained.sh`.
- **Binder and runner, in fake worlds** (`src/eval/fake.rs`):
  - `or` alternatives fall through, and rejections are recorded;
  - checks fail explicitly;
  - random-access children cost one evaluation each;
  - scans resume from checkpoints;
  - Brent's algorithm stops a cycle;
  - a scan past its count, and budget exhaustion mid-page, are typed.
- **`docs/views.md` is executable**, like `docs/expressions.md`. Every
  example runs as a test, so the reference and the implementation change
  together.

### 5.2 Real programs

- **A `containers` fixture per language.** Rust's uses `std`; today's
  fixtures are `no_std`. Each fixture holds:
  - every built-in view's type, empty and populated;
  - a large instance for paging (300+ elements);
  - deliberately corrupted instances: a cyclic `std::list` and C list,
    `len > cap`, a garbage data pointer, a variant index out of range.

  Each value's expectation is a `// VIEW:` marker on its line (§3.15).
  Assertions cover summaries, children, DAP counts and pages, and
  `evaluateName`s that evaluate back. Corrupted instances must produce their
  typed problem, raw underneath, and finish within the budget.
- **"Every built-in view binds."** For each fixture build, every built-in
  container type is presented by the view meant for it, and none falls
  through to raw. A toolchain bump that moves a private field fails here,
  with `info view`'s explanation.
- **Strings move from `string_parts` to views.** The existing string tests
  (`tests/debugger/values.rs`, `tests/dap/variables.rs`, `tests/cli.rs`)
  stay green unchanged.
- **Run control comes first.** A scenario asserts that a `continue` sent
  during a large presentation is acknowledged first, and that the
  presentation ends stale.

### 5.3 Matrix

| Language | Builds |
|---|---|
| C++ | gcc and clang × libstdc++; clang × libc++ (P1); -O0 and -O2. P6 adds `-fstandalone-debug` (the libc++ `shared_ptr` control block is declaration-only by default, so counts must be "unavailable", not 0), `-gsimple-template-names`, `_GLIBCXX_DEBUG`, `_GLIBCXX_USE_CXX11_ABI=0`, and `-static-libstdc++` |
| Rust | debug and `-O` (fat-pointer names change). `-C debuginfo=limited` has no types at all; a test pins that it shows raw with a clear reason |
| Go | `-N -l` and default |
| Zig | `-fllvm` (today) and, in P6, the self-hosted backend (the default for Debug; different shapes); Debug and ReleaseSafe |

The pinned toolchains are the newest of each (D4). Older toolchains are not
part of the matrix.

### 5.4 Simulator

A golden C program, `containers`, builds hand-rolled containers:

- an array with a length;
- a singly linked list that a step can make cyclic;
- an open-addressed table.

Its manifest ships a views file. The simulated client fetches presentations
and child pages at stops; it currently never asks for children. A new
**views oracle** checks every presented element against the simulation's
ground truth:

- each element's storage address and bytes match simulated memory;
- the count matches the marker's `EXPECT` (for example `len(list) == 3`);
- paging in any page size yields the same sequence as one page;
- a cyclic list ends in the typed cycle problem;
- no presentation exceeds its budget.

As AGENTS.md requires, there is a sabotage test (an engine that drops one
element must fail) and coverage marks the gate's seeds must reach.

## 6. Decisions (2026-10-05)

- **D1 Declarative views.** A small view language on uscope's expressions.
  Kernels serve only algorithms, and native Rust views are the exception.
- **D2 No Python and no GPL code.** uscope does not run, read, port, or
  test against Python printers or GPL sources, and takes no heavy
  dependency like an embedded interpreter. Views are written from the DWARF
  of uscope's own fixtures.
- **D3 Load without a prompt.** Views from binaries and from project
  directories load automatically; their inability to act is the safety.
- **D4 Toolchains.** Add libc++ to the dev shell now, and test against the
  newest pinned toolchains only.
- **D5 First wave.** The daily types of P2–P4 across all five languages.
  `BTreeMap`, classic Go maps, `std::any`, iostreams, and channel waiters
  come later. This lifts, for views, the earlier decision to keep language
  support minimal.
- **D6 One neutral summary style**, shown in §3.7, in every language and
  client.
- **D7 Maintenance.**
  - Adopted: `inner()` and `**` patterns, and the "every view binds" gate.
  - Not adopted: a canary job, a promise to keep old layouts, and reuse of
    upstream printer suites.
- **D8 Kernels.** Designed into the contract now, built in P7 when the
  first view needs one: wasmi, with a core-wasm `read`/`yield` ABI.
- **D9 Views run on the controller thread**, in the existing inspection
  paths, with budgets as deterministic timeouts, priority scheduling for
  run control, and cancellation on resume. Keep the architecture simple.
- **D10 uscope first, standards later.** Formats stay uscope-specific
  (`.debug_uscope_views`, `uscope_kernel_v1`). The standards ideas in §7
  wait until views have shipped.

## 7. Later: a shared standard

Deferred by D10, and kept here so the ideas are not lost. Each level is
useful without the others.

- **L0. Producers emit more semantics in DWARF.** Every debugger benefits,
  with no new format.
  - **`counted_by` in DWARF.** Checked on 2026-10-05: neither gcc 15 nor
    clang 21 emits C's `counted_by` into DWARF, and gcc 15 rejects it on
    pointer members. DWARF 5 can already express it, as a `DW_AT_count`
    expression using `DW_OP_push_object_address`. The Linux kernel
    annotates hundreds of structures with it.
  - **GCC template parameters on every instantiation.**
  - **libc++** making the `shared_ptr` control block's debug info complete
    by default.
  - **Zig's LLVM backend** emitting `DW_TAG_variant_part`.
  - **An explicit marker for transparent wrappers.**
- **L1.** Libraries ship declarative views in their binaries.
- **L2.** A tiny executable ABI for algorithms: the kernels.
- **L3.** A shared conformance corpus: programs built across compilers,
  with the values a debugger should show.

## Appendix A. Delve's output, the bar for Go

```text
map[string]int ["one": 1, "two": 2, ]
interface {}(main.Point) {X: 1, Y: 2}
error(*errors.errorString) *{s: "boom"}
main.main.func1 {s string = "hello, world"}
[]int len: 3, cap: 3, [1,2,3]
```

## Appendix B. Structural facts the design relies on

| Fact | Evidence |
|---|---|
| Inline namespaces carry `DW_AT_export_symbols` | gcc 15 `std::__cxx11`; clang 21 `std::__cxx11` and `std::__1`, also with `-gdwarf-4`; gcc's type-unit copies omit it |
| `std::__debug` is an ordinary namespace outside debug mode | `__gnu_debug::vector<int>` is 56 bytes, `std::vector<int>` 24 (gcc 15) |
| Container definitions carry template parameter DIEs | gcc, clang, both libraries; gcc omits them on `std::allocator<T>`, iterators, 39 of 309 templates |
| libc++ default debug info leaves helpers declaration-only | `__shared_weak_count` (shared_ptr counts), iterators; `-fstandalone-debug` restores them |
| Rust enums are `variant_part` with `discr_value`; the variant without one is the default | `Option<&T>`, `Result<u32, String>` (niche 0xffff…ff), never `DW_AT_discr_list` |
| Rust fat pointers are `{data_ptr, length}` and `{pointer, vtable}`; names change at `-O` | `&[i32]` vs `*const [i32]` |
| Rust vtables are variables `<C as T>::{vtable}` whose type has `DW_AT_containing_type` C | rustc nightly 2026-07-10 |
| Rust `-C debuginfo=limited` emits no types | zero `DW_TAG_structure_type` |
| Go kinds and element types are attributes | `go_kind` 0x2900, `go_key` 0x2901, `go_elem` 0x2902, `go_runtime_type` 0x2904 (offset from `runtime.types`) |
| Go swiss-map layout | `map<K,V>{used, dirPtr, dirLen, globalDepth, …}`, groups `{ctrl u64, slots [8]{key, elem}}`, `0x80` = empty |
| Zig self-hosted backend: DWARF 5, `DW_LANG_Zig`, variant parts for `?T`/`E!T`/tagged unions, `DW_AT_ZIG_sentinel` | zig 0.16.0 |
| Zig LLVM backend: DWARF 4, `DW_LANG_C99`, `{payload, some}` optionals, packed struct field names lost | zig 0.16.0 `-fllvm` |
