# Data views plan

TODO item (new): *Render standard-library and user data structures as what
they mean: a `std::string` as text, a `Vec` as its elements, a Go map as its
entries.*

A debugger exists to show data. Today uscope shows structure: a Rust `Vec` is
`{buf: {inner: {ptr: …, cap: …}}, len: 3}`, a Go map is a pointer to
`internal/runtime/maps.Map`, and a libc++ `std::string` is a union of bit
fields. This plan adds **views**, which present a value as the thing it
stands for while keeping the raw structure one step away. They cover C++,
Rust, Go, and Zig standard libraries and any type a user describes. Nothing
in the design depends on one compiler, compiler version, linker, or loader.

Research for this plan ran on 2026-10-05 against the pinned toolchains (gcc
15.2, clang 21.1.8 with libstdc++ and libc++ 21, rustc nightly 2026-07-10,
Go 1.26.5, Zig 0.16.0, gdb 17.1, lldb 21.1.8, Delve master). §1 and the
appendix keep its findings.

## Goals

Agreed on 2026-10-05:

1. **Flexible for maintainers and users alike.** uscope's built-in views
   track the newest versions of every supported toolchain. Users can
   override and extend them trivially. They can write views for their own
   complex types as easily as uscope's maintainers write them for standard
   libraries. Upstream contributions to the built-in views are welcome, so
   contributing must be easy too.
2. **Fast and robust.** It should be nigh impossible for a view to harm
   the debugger. That means no crash, no hang, no unbounded memory, no
   delay to run control, and no convincing wrong answer.
3. **Sane defaults, easy to extend and maintain.**

Beyond uscope, the aim is to lead on data visualization for Linux debugging
in public:

- think through the problems and edge cases;
- publish what we learn;
- work with compiler, library, and debugger maintainers toward a standard
  that compilers and debuggers adopt (§7).

Every interface here is designed as if other debuggers will implement it.

## 0. Summary

- **What the ecosystem provides.** No part of the Linux ecosystem emits a
  description of container semantics that is independent of the compiler or
  library version, and that a debugger without Python can consume. What
  exists falls into two groups:
  - Python scripts:
    - libstdc++'s gdb printers, located through the library's auto-load
      directory;
    - rustc's providers, named by a `.debug_gdb_scripts` entry;
    - Go's `runtime-gdb.py`, named by an absolute GOROOT path;
    - Zig's scripts, which users load by hand.
  - LLDB's C++ formatters, compiled into LLDB.

  Every one hard-codes private field paths and breaks as the library moves.
  Several were broken on these toolchains when tested (appendix A).
- **What is reliable.** DWARF's *structure*:
  - template parameters by position;
  - inline namespaces marked `DW_AT_export_symbols`;
  - `DW_TAG_variant_part` discriminants;
  - fat-pointer shapes;
  - Go's `DW_AT_go_kind`, `go_key`, `go_elem`, and `go_runtime_type`;
  - Zig's `DW_AT_ZIG_sentinel`;
  - vtable symbols naming concrete types.

  uscope throws most of this away today (§2).
- **Design.** There are three layers, joined by one small **view
  contract** (§3.0).
  1. The provider normalizes structure and type identity (§3.1–3.2).
  2. Views map a type pattern to a presentation (§3.3–3.6).
     - Most are written in a small **declarative view language** built on
       uscope's expression language.
     - Algorithms the language cannot say well (B-trees, hash tables caught
       mid-resize) can use **kernels**: sandboxed WebAssembly functions from
       memory reads to yielded items, called from inside a declarative view
       (§3.13).
     - A view *binds* against the concrete type before it runs.
       Alternatives that fail to bind fall through to the next, which
       gives tolerance across versions without sniffing versions.
  3. A neutral presentation model, which every client renders (§3.7).
- **Robustness by construction** (§3.14):
  - views cannot write, call, or perform I/O;
  - every step is metered;
  - views run on a presentation worker, never on the ptrace controller, and
    a resume cancels them;
  - a failing view costs only its own value.
- **Where views come from.** uscope ships the standard-library views as data,
  tested against the fixture matrix. Users and projects add their own, and
  binaries can carry them in a `.debug_uscope_views` section. Views are
  declarative and bounded by the existing budgets, so loading them is safe
  without a trust prompt.
- **Maintenance.** The burden that remains is kept small, early, and
  harmless (§1.2, §3.12):
  - layout knowledge is confined to short per-library views that name
    meaning (`inner()` wrappers, template arguments) rather than paths;
  - old alternatives stay;
  - upstream printer test suites and canary builds against nightly
    toolchains find breakage first.
- **Explicit results.** A view never guesses:
  - a value no view binds shows raw;
  - a failed invariant shows raw with the reason;
  - a cycle or an exhausted budget is a typed partial result.

  Raw is always one step away: a `[raw]` child, `print/r`, or a per-session
  switch.

## 1. What exists, and what it teaches

### 1.1 Shipped printers

| Ecosystem | Artifact | How it is found | Keyed on | Reads layout by | State on our toolchains |
|---|---|---|---|---|---|
| libstdc++ | `printers.py` (3014 lines), `xmethods.py` | `libstdc++.so.6.*-gdb.py` beside the library or under `/usr/share/gdb/auto-load` | exact base name after stripping `std::__8`, `__cxx1998`, `__debug` | private members (`_M_impl._M_start`), constructed type names (`std::_List_node<T>`), constants (deque 512 bytes) | Good with gdb. Belongs to the *runtime library*, not the program: a `-static-libstdc++` build or a core without the `.so` gets nothing. GPLv3. |
| libc++ | `libcxx/utils/gdb/libcxx/printers.py` (upstream, tested); LLDB's C++ formatters | gdb: not installed by any distro, so it must be sourced by hand; lldb: compiled into liblldb | gdb: base-name dictionary (31 types); lldb: regexes over `std::__[[:alnum:]]+::` | private members, anonymous `_LIBCPP_COMPRESSED_PAIR` structs | gdb printers work for string, vector, map, unordered_map, list, deque, set, unique_ptr, tuple; there is no printer for optional, variant, array, or span. LLDB is good, but its own tests simulate 60 layouts of `std::string` alone. A January 2026 RFC moves the formatters into libc++ because they keep breaking. |
| Rust | `gdb_providers.py`, `lldb_providers.py`, `rust_types.py` in `lib/rustlib/etc` | `.debug_gdb_scripts` entry `\x01gdb_load_rust_pretty_printers.py`, resolved by `rust-gdb`'s search path | regexes over qualified names (`^(alloc::([a-z_]+::)+)Vec<.+>$`) | `buf.inner.ptr.pointer`, hashbrown control bytes, `RcInner` fields | Seven `BACKCOMPAT` breaks in the providers since 1.32. The most recent layout change landed on 2026-09-15. Under Nix, gdb declines to load them (safe-path), silently. |
| Rust (crates) | `#[debugger_visualizer(gdb_script_file / natvis_file)]` | inline Python in `.debug_gdb_scripts` (kind `0x04`); natvis reaches only PDBs | — | — | A handful of crates (smol_str, slint, windows-rs, url). Nothing a non-Python debugger can use on Linux. |
| Go | `runtime-gdb.py` | `.debug_gdb_scripts` `\x01/nix/store/…/go/src/runtime/runtime-gdb.py` (the builder's absolute path) | type-name regexes | swiss-map internals | Maps work. **Interfaces are broken** (looks for `runtime._type`, now `internal/abi.Type`), and `$len(map)` raises. |
| Go | Delve | — | **`DW_AT_go_kind`**, never names | field-presence checks (`dirPtr` means swiss maps, `buckets` means classic maps), `+rtype` assertions checked in CI | The best existing model (§1.4). |
| Zig | `lib/lldb/pretty_printers.py` (0.17); the gdb scripts were deleted as "too outdated" | loaded by hand | name regexes, `dbHelper` dummy functions | `payload`/`some` etc. | On 0.16 binaries: `?u32 = 42` shows `null`, error unions show `()`. Stock lldb prints nothing for self-hosted-backend Debug builds. |
| LLDB | formatter bytecode in `.lldbformatters` | section in the binary | name or `^regex` | a sandboxed stack VM with a fixed selector table | Only Swift's `@DebugDescription` emits it. The v2 ABI is still moving. |
| Windows | natvis | PDB (`/NATVIS`) or files | `std::vector<*>` wildcards, `$T1` | expressions over private members | The richest declarative vocabulary. VS Code's MIEngine runs a subset over gdb on Linux. There are no maintained libstdc++ or libc++ natvis files. |
| RAD Debugger | `type_view: {type, expr}` | config, or a `.raddbg` section | patterns with captures (`TArray<?{T}>`) | its own expression language with lenses (`slice`, `array`, `list`, `rows`, `bitmap`, …) | The closest design to this plan. Views are expressions, and indexing goes through them. |

### 1.2 Who maintains visualizers, on Windows and on Linux

The question that decides uscope's maintenance burden is who keeps a
visualizer in step with the code it describes. A second round of research
(2026-10-05; appendix C) looked at the Windows ecosystem and at Linux's
equivalents. The findings:

- **Windows is cheap mostly because the MSVC STL's ABI has been frozen since
  2015.**
  - `STL.natvis` changed in 36 commits over 5.5 years, about 6 a year,
    mostly for new types.
  - It has no tests. Its maintainer calls testing it "the most significant
    issue".
  - It ships with the IDE through a hand-mirrored copy that has lagged by a
    year.
  - ABI-compatible member renames still broke it (`atomic`, the
    `make_shared` control blocks), and one bug showed the wrong year for
    February dates.
- **Natvis is not a portable format in practice.** It is XML around each
  host's C++ expression evaluator.
  - Microsoft's own two engines (VS and WinDbg) disagree.
  - JetBrains rewrote an evaluator to support it, and MIEngine and RAD
    Debugger implement subsets.
  - Library-owned natvis files without tests rot for 10–18 months (LLVM's
    `SmallPtrSet`, nlohmann/json, Qt 6).
- **Only one arrangement reliably keeps a visualizer correct: the owner
  changes it in the same commit as the layout, and a test fails if they
  forget.**
  - Rust does this for natvis: all 13 layout-driven natvis changes in the
    last three years landed in the std PR that caused them, caught by cdb
    tests on CI.
  - libstdc++ does it for gdb: 306 value checks, with layout fixes in the
    same commit as the header change.
  - libc++'s gdb printers do the same, upstream.
  - LLDB's libc++ formatters, owned by a different team, chased the same
    libc++ changes in separate PRs months later. This is the clearest
    evidence that visualizers belong with the code they describe.
- **On Linux, only libstdc++ is fully owned, tested, and installed.**
  - Fedora 44's whole repository ships gdb auto-load scripts for libstdc++,
    glib, GStreamer, CPython, Arrow, LibreOffice, and a few others. It ships
    none for libc++, Rust, Go, Qt, Boost, or abseil.
  - libc++'s printers are tested but not installed.
  - Rust's need the active toolchain to be the one that built the binary.
  - Go's `runtime-gdb.py` is tested without ever printing an interface,
    which is why its broken interfaces went unnoticed.
  - libc++ plans to ship formatters inside `libc++.so`: Python first, LLDB
    bytecode later.
- **Upstream-maintained data is usable as a reference, not as a dependency.**
  - Rust's natvis files carry over to Linux DWARF for 92% of their entries,
    as-is or after a mechanical rename.
  - But Linux toolchains do not ship them. They must match the exact rustc
    commit (today's HEAD is already wrong for a July binary). They omit
    `BTreeMap`, `PathBuf`, `Box<str>`, and `Mutex`. Two of their entries
    have been stale since 2022.
- **How fast layouts change, by library:**

  | Library | Change rate |
  |---|---|
  | libstdc++ | frozen since GCC 5; about 1 printer change a year forced by layout |
  | libc++ | stable bytes but renamed DWARF members; about 1–2 a year |
  | Go | rare but large: the map rewrite, `abi.Type` changes |
  | Rust | about 4 a year |
  | Zig | pre-1.0, and its two backends differ |

Nothing on Linux takes the burden off uscope today. What can be done is to
make breakage rare, caught before release, cheap to fix, and harmless when
it happens: §3.12.

### 1.3 What every successful design shares

1. **Matching by type, with captured arguments.** Natvis `$T1`, RAD
   `?{T}`, LLDB template-argument selectors, gdb's `template_argument(n)`.
2. **Children are lazy, and random-access where possible.** gdb 14's
   `num_children`/`child(n)`, LLDB's synthetic `get_child_at_index`, DAP's
   `start`/`count`. Linked structures scan with checkpoints (LLDB caches
   iterators by index).
3. **Fallback across layouts.** Natvis `Priority` and `Optional`: an entry
   that fails to parse against the type yields to the next one. Delve
   chooses map layouts by which fields exist.
4. **A raw view everywhere.** `[Raw View]`, `print/r`, `frame variable --raw`.
5. **Views compose with expressions.** gdb xmethods (`v[1]`, `v.size()`),
   natvis `[]` on `ArrayItems`, RAD lenses as types.

### 1.4 Common failures, which this design forbids

- **A convincing wrong answer.**
  - lldb's libstdc++ formatters print a two-entry `unordered_map` built by
    GCC as `size=0 {}`. GCC omits `std::allocator`'s template parameters,
    and a bare `except` turns the failure into an empty container.
  - lldb truncates 64-bit Rust discriminants to 32 bits, so `Ok(7)` shows as
    `Err("")`.
  - Zig's printers show `?u32 = 42` as `null`.
- **Unbounded work.**
  - Uninitialized `std::set`s hung gdb under Eclipse CDT.
  - A Qt Creator release grew gdb's memory until the machine hung.
  - gdb prints a cyclic list until `print elements` runs out.
- **Name regexes that miss real spellings.**
  - At `-O`, rustc names slices `*const [T]`. uscope's own `&[` prefix test
    misses them too (§2).
  - GCC writes `pair<int const, …>` and `array<int, 4>`, while clang writes
    `pair<const int, …>` and `array<int, 4UL>`.
  - `-gsimple-template-names` emits bare `vector`.
- **Version churn.** Every artifact in §1.1 is a list of private paths kept
  in step with one library version. Delve contains the damage best:
  - it decides kinds from structural attributes;
  - it chooses layouts by which fields exist;
  - it writes each runtime assumption next to the code, where CI checks it
    against the runtime source.

**Conclusion.** Do not run any of these artifacts. Use them as layout
references (license note: write views from the DWARF and headers, not by
translating GPL Python). Build on the DWARF structure that is stable across
toolchains, and keep the library-specific knowledge small and declarative.
It should be checked against real binaries and should fail loudly.

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
debugger*. It is the part worth standardizing (§7), and it is what LLDB's
bytecode RFC called the hard part: "all the interesting/difficult work
here is about how to interface with ValueObject".

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
    pub base: Arc<str>,                 // "vector", "Vec", "Aligned"
    pub arguments: Arc<[TypeArgument]>, // by position, packs flattened
    pub origin: ArgumentOrigin,         // Dwarf | ParsedName | None
}
pub enum TypeArgument { Type(TypeReference), Value(IntegerValue), Unknown(Arc<str>) }
```

- **Inline namespaces collapse structurally.** gcc 15 and clang 21 both mark
  `std::__cxx11` and libc++'s `std::__1` with `DW_AT_export_symbols` (checked
  on this machine). A DWARF 4 fallback list (`__1`, `__Cr`, `__ndk1`,
  `__cxx11`, `__8`, `__debug`, `__cxx1998`) applies only when the attribute
  is absent.
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
- **The type index.** Each image gets a lazily built index from
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
  - `list` runs Brent's cycle detection on node addresses;
  - `inorder` bounds its explicit stack at 128 levels.

  A cycle, an overlong stack, or a scan that yields more than its declared
  `COUNT` ends in a typed partial result (`cycle at element 3`).

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

This is the property gdb's auto-load safe-path exists to approximate for
Python. A parse or bind error in a file is reported once per load (in the
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

- **Running Python**, or reading `.debug_gdb_scripts`. At most,
  `info view` mentions that a binary names gdb scripts uscope does not run.
- **Structural guessing.** One example is RAD's `slice` guessing "the first
  pointer and the first integer". Every view is explicit about which
  members it reads.
- **Inferior calls**, such as `size()` or the stringstream printers' calls
  into the program.
- **Whole views in WebAssembly**, as opposed to kernels (§3.13). A full
  wasm view would need the whole of §3.0 as a binary ABI, typed handles
  for types and places, forever. Kernels need only `read` and `yield`.
  Revisit only if kernels prove too narrow.
- **Natvis import and LLDB bytecode.** Natvis is deferred to a later phase
  (§4, P7), and LLDB bytecode is deferred indefinitely. Natvis's
  vocabulary maps onto §3.3 (DisplayString, ArrayItems, IndexListItems,
  LinkedListItems, TreeItems, ExpandedItem, Condition, Optional, Priority,
  `$T`), so an importer is a translator, not a second engine. That is the
  same conclusion RAD Debugger reached.
  - Its value on Linux is third-party libraries (imgui, EASTL, Godot, EnTT,
    Unreal), as CLion 2026.2 found.
  - It is not a way to get standard-library views. Rust's std natvis is not
    shipped on Linux and is pinned to a commit (§1.2).
  - LLDB bytecode is the format to watch, because libc++ intends to ship
    its formatters that way. Its selectors (child by name, template
    argument, cast, read memory) map onto uscope's `Machine`, so an
    interpreter is plausible once libc++ ships one.
- **A gdb subprocess as a formatter.** uscope would serve the stopped
  tracee's memory read-only over the remote protocol, and gdb would format
  with its auto-loaded printers.
  - It is feasible: Pernosco does exactly this, and a gdbserver stand-in
    worked here with `set sysroot /` and paged over MI.
  - But it adds a gdb dependency, values come back as strings, and it
    inherits upstream's gaps (no libc++ printers installed, broken Go
    interfaces).
  - It would be an opt-in provider for the Linux long tail (glib, CPython,
    Arrow, users' own printers), and only if users ask.

### 3.11 Purity and threads

- **`src/view` is pure.** It holds the parser, binder, generator machinery,
  kernel host, and summary formatter, with no program. It sits under the
  same boundary test as `src/eval`, and reaches programs only through the
  contract's traits (§3.0).
- **Views run on a presentation worker, not on the controller.** The
  controller serves memory a page at a time from a per-stop page cache
  (§3.14), and answers type and symbol queries from immutable module data.
  Everything else (binding, generators, kernels, summaries) happens on the
  worker.
- **The worker owns the stop-scoped state.** It holds the bound-view cache
  (keyed by type and view-set generation) and the scan checkpoints. A
  resume bumps the `StopId`; in-flight work fails as stale and the
  checkpoints are dropped.
- **The simulator drives the engine inline.** The worker is an edge
  adapter around a pure engine, so a simulated world runs presentations
  synchronously, with no real thread.
- **View sources** are parsed and validated off both threads. An immutable
  `Arc<ViewSet>` with a generation number is handed over whole.

### 3.12 Keeping views working

The goal is that a library change never shows a user a wrong value, rarely
shows them raw, and reaches uscope's maintainers before it reaches users.

1. **Most presentation needs no library knowledge at all.** These are all
   DWARF semantics and change only when DWARF does:
   - enums and optionals;
   - slices, `str`, and Go strings and slices;
   - Zig sums and sentinels;
   - trait objects and Go interfaces (§3.1, §3.6).

   The views that do need library knowledge are a short list per library.
2. **Views name meaning, not paths, wherever the DWARF allows it:**
   - `inner()` steps through wrappers;
   - element and key types come from template arguments, `go_key` and
     `go_elem`, or `typeof`;
   - patterns anchor on the crate or namespace root and base name.

   Of the 13 Rust std changes that broke natvis between 2023 and 2026, by
   our reading of each PR, 11 add, remove, or rename a wrapper or a module:
   `Cap`, `RawVecInner`, `Unique` removal, `MaybeDangling`, `WrappedIndex`,
   `ManuallyDrop`, the `NonZero` inner type, two `Pin` field renames,
   `RcBox`→`RcInner`, `rc`→`rcs`. Views written this way would survive
   them. The atomics' `Atomic<T>` rename and the `NonZero` alias flip
   change the type's own name and need a second pattern.
3. **Old layouts stay.** Alternatives (`or`, a second view) are rarely
   deleted, so a user on an old toolchain keeps working after uscope
   learns a new layout. This is the reverse of Rust's providers, which must
   track only the current std.
4. **Failure is visible, never wrong.**
   - When nothing binds, the value is raw and `info view` says why: for
     example, "alloc::vec::Vec: `inner(buf).ptr`: no member `ptr` in
     `RawVecInner`".
   - A failed `check` is shown as the problem it is.
   - Natvis's silent fallback is safe but leaves users guessing;
     `time_point`'s wrong February year is the failure to avoid.
5. **Fixing does not wait for a release.** Views are data:
   - a user or project file overrides a built-in view on the spot (§3.8);
   - a fix to a built-in view is a one-file change with a fixture.
6. **Upstream tests become uscope's tests.**
   - The library owners have already written expected values for their
     printers:
     - libstdc++'s `libstdc++-prettyprinters` (306 value checks);
     - libc++'s `gdb_pretty_printer_test` (about 126);
     - Rust's `tests/debuginfo` (139 gdb tests);
     - Go's `TestGdbPython`.
   - A `just upstream-views` recipe builds their test programs with the
     pinned toolchains and compares uscope's presented *values* (not their
     text format) with the expected ones, under a small translation table
     per suite.
   - When a library changes a layout, its own test already says what the
     value should be.
7. **Canary builds catch breakage before release.** A scheduled job (once
   CI exists; TODO.md) builds the fixtures and upstream suites with the
   newest toolchains:
   - Rust nightly;
   - Go tip;
   - Zig master;
   - libc++ and libstdc++ trunk.

   It runs "every view binds" and the value checks (§5.2). This is the role
   Delve's `+rtype` CI check plays. Breakage is found the week the library
   changes, usually as one `or` alternative to add.
8. **Ownership can move upstream later.**
   - `.debug_uscope_views` (§3.8) lets any library ship views in its own
     binaries, tested in its own CI.
   - An LLDB bytecode interpreter would let uscope consume what libc++
     ships, once it does (§3.10).
   - Either moves a library's views to the people who change its layout.

**Expected burden.** By the churn above, this is a handful of one-line view
edits a year, mostly for Rust and libc++. Each is found by the canary
before users see it, and each degrades to a raw value with a reason if it
ships anyway.

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
- Any debugger can implement two imports in an afternoon, which makes this
  a credible cross-debugger proposal (§7).
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
change.

### 3.14 Robustness model

The guarantee is that a view can make its own value wrong only by being
visibly wrong, and can never affect anything else. Each property below has
a mechanism and a test.

| Property | Mechanism | Test |
|---|---|---|
| No side effects | The contract has no writes, calls, or I/O; kernels can import only `read` and `yield` | import validation; boundary test on `src/view` |
| Bounded work | Every read, generator step, kernel instruction (fuel), and output node is charged to one per-presentation budget; scans may not pass their count | fake-world budget tests; hostile fuzzing (below) |
| Bounded memory | Budget caps on output and text; kernel linear memory 4 MiB; view files 256 KiB | heap cap in every test process (`memory_cap.rs`) |
| Run control never waits | Views run on the presentation worker. The controller only serves cached pages; a resume makes in-flight work stale | scenario: `continue` while a large presentation runs; stress |
| Contained failure | A bind error, failed check, cycle, trap, or exhausted budget makes *that value* raw with a typed problem. Engine panics are caught at the presentation boundary and recorded by the flight recorder | sabotage tests; fault injection in the simulator |
| Deterministic | No clocks (fuel, never time); the simulator runs the engine inline | `a_seed_always_names_the_same_run` |
| Never convincingly wrong | Binding is static and total; `check`s guard invariants; a missing type is "unavailable", never zero | the failures of §1.4 as regression tests |
| Fast | One per-stop page cache shared by all views; bound views cached per type; random-access paging; previews capped | a container-heavy scenario with a read-count budget |

**Hostile fuzzing** is the "nigh impossible" assurance. A contained fuzz
target runs every built-in view, and random view files, over random
memory:

- garbage pointers;
- cycles;
- huge counts;
- unmapped pages.

It asserts that every presentation ends with a value or a typed problem
within its budget, never panics, and never allocates past its cap. It runs
only through `scripts/contained.sh`, like the existing fuzzers.

**The page cache.** At a stop, the worker requests memory in aligned 4 KiB
pages through the handle. The controller reads each page once through
`/proc/<pid>/mem` and hides breakpoint bytes, as reads do today. The cache
dies with the `StopId`. Hash tables and trees touch the same pages
repeatedly, so this cuts reads sharply. It also keeps every tracee access
on the controller, as AGENTS.md requires.

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
- the "every built-in view binds" gate check;
- the canary job (§3.12).

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
(`/roast`) before landing. While iterating, run targeted tests; at the end
of each phase, `just` and `just sim 60`. `just all`, `just stress`, and
`just sim 600` run once, at the end of the project.

- **P1 Type identity (provider; small visible change).**
  - §3.1 fat pointers by shape; this fixes `*const [T]` at `-O`.
  - Zig `[]const u8` text, and text charged to the budget.
  - §3.2 identity: language, path with `DW_AT_export_symbols` collapsing,
    template arguments with packs, name parsing for Zig and GCC gaps, Go
    attributes.
  - The per-image identity index.
  - Add libc++ to the dev shell (§6, D4).
  - `ptype` shows template arguments.
- **P2 The engine and contiguous shapes.**
  - `src/view` parser and binder (`or`, types, checks).
  - Random-access sequences, `text`, `value`, `empty`, `if`, fields.
  - The `Presentation` model, the view children reference, `[raw]`.
  - CLI `print`, `print/r`, `info view`; DAP counts, `filter`, hints.
  - `v[i]` and `len(v)` through views.
  - Built-in views that replace `string_parts`: C++ strings (libstdc++,
    old-ABI, and libc++ short and long forms), Rust `String`, `Box<str>`,
    `PathBuf`, `OsString`, `CString`.
  - Further built-in views: `std::vector` (not `<bool>`), `std::array`,
    `std::span`, `std::string_view`, Rust `Vec`, `VecDeque`, Zig
    `ArrayList`, `ArrayListUnmanaged`.
  - `docs/views.md` with executable examples.
- **P3 Scans and maps.**
  - `list`, `inorder`, filters, nested generators, checkpoints, and cycle
    detection.
  - Map presentation.
  - Built-in views:
    - C++: `std::map`, `set`, and their multi- forms; `unordered_*`;
      `std::list`, `forward_list`, `deque`.
    - Rust: `HashMap`, `HashSet`.
    - Go: maps.
    - Zig: `HashMap`, `ArrayHashMap`.
- **P4 Pointers, sums, and dynamic types.**
  - Rust `Box`, `Rc`, `Arc`, `Weak`, `Cell`, `RefCell`, `Mutex`.
  - C++ `unique_ptr`, `shared_ptr`, `weak_ptr`, `optional`, `variant`,
    `tuple`.
  - §3.6 vtables (C++ and Rust `dyn`), and Go interfaces and `error`.
  - Go channels.
  - Zig LLVM-backend optionals, error unions, and tagged unions.
- **P5 User and embedded views, and the authoring tools.**
  - `uscope views check`, `views explain`, `extend`, `hide`, `format`,
    `match`, `record`, `container_of`, `global`.
  - `docs/writing-views.md`.
  - Session, user, and project files.
  - The DAP `viewFiles` launch argument.
  - `.debug_uscope_views` and its C header.
  - Module scoping.
  - Load diagnostics.
- **P6 Compatibility.** Widen the fixture matrix (§5.3):
  - Zig self-hosted backend;
  - `-gsimple-template-names`;
  - `_GLIBCXX_DEBUG`;
  - `_GLIBCXX_USE_CXX11_ABI=0`;
  - C++ layout simulators for historical libc++ and libstdc++ layouts;
  - native views for Rust `BTreeMap` and classic Go maps if the language
    cannot say them.
- **P7 Kernels**, when the first built-in view needs one (`BTreeMap` or
  classic Go maps):
  - the wasmi host;
  - the `uscope_kernel_v1` ABI;
  - the SDK crate and C/Zig header;
  - kernel records in `.debug_uscope_views`;
  - replayable recorded runs.
- **P8 (later, optional).** Natvis importer; richer presentation hints
  (table, bitmap, memory) for a future TUI or web UI.

The presentation worker, page cache, and hostile fuzz target land in P2
with the engine, not later, because robustness is a property of the
architecture.

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

  Assertions cover summaries, children, DAP counts and pages, and
  `evaluateName`s that evaluate back. Corrupted instances must produce their
  typed problem, raw underneath, and finish within the budget.
- **"Every built-in view binds" checks.** For each fixture build, a test
  asserts:
  - every built-in container type in it is presented by the view meant for
    it;
  - no such type falls through to raw.

  Bumping a toolchain that moves a private field therefore fails the gate
  with `info view`'s explanation, the role Delve's `+rtype` checks play.
- **Strings move from `string_parts` to views.** The existing string tests
  (`tests/debugger/values.rs`, `tests/dap/variables.rs`, `tests/cli.rs`)
  stay green unchanged.

- **Upstream suites** (§3.12): `just upstream-views` runs libstdc++'s,
  libc++'s, Rust's, and Go's printer tests against uscope's presented
  values. It runs outside the gate, at the end of each phase, and in the
  canary job.

### 5.3 Matrix

| Language | Builds |
|---|---|
| C++ | gcc and clang × libstdc++; clang × libc++ (added in P1); -O0 and -O2. P6 adds `-fstandalone-debug` vs default (the libc++ `shared_ptr` control block is declaration-only by default; the view must say "unavailable", not 0), `-gsimple-template-names`, `_GLIBCXX_DEBUG`, `_GLIBCXX_USE_CXX11_ABI=0`, `-static-libstdc++` |
| Rust | debug and `-O` (fat-pointer names change). `-C debuginfo=limited` has no types at all; a test pins that it shows raw with a clear reason |
| Go | `-N -l` and default |
| Zig | `-fllvm` (today) and the self-hosted backend (the default for Debug; different shapes), Debug and ReleaseSafe |

Older layouts are covered without older toolchains where possible. Like
LLDB's libcxx-simulators, C++ fixtures can declare historical layouts under
the real namespaces. Rust and Go history waits for a decision on pinning
extra toolchains (D4).

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

## 6. Decisions to make

- **D1 Declarative views (recommended) or hand-written Rust printers.**
  - Declarative costs a small interpreter on top of `src/eval`. In return,
    standard-library fixes are data changes, users can describe their own
    types, binaries can carry views, and every view gets the same binding,
    budgets, and diagnostics.
  - Rust printers alone are faster to start and would be the Delve model.
    But users could never extend them, and every layout fix would need a
    uscope release.
- **D2 No Python, no gdb scripts (recommended).** Use the DWARF
  structure, ELF symbols, and Go runtime tables. Treat the ecosystem's
  scripts as layout references only, and do not translate the GPL'd
  libstdc++ printers.
- **D3 Load views from binaries and project directories without a prompt
  (recommended).** They are declarative and bounded.
- **D4 Toolchains in the dev shell.**
  - Add libc++ now (recommended).
  - Decide separately whether to pin older Rust and Go toolchains for
    layout history (a `just compat` recipe outside the gate), or to rely on
    simulators and "every view binds" checks against the pinned
    toolchains only.
- **D5 First-wave scope.** The earlier decision to keep language support
  minimal deferred standard-library container views "until there is a
  need". This plan is that need. P2–P4 cover the types users meet daily;
  `BTreeMap`, classic Go maps, `std::any`, iostreams, and channel waiters
  wait for P6 or later.
- **D6 Summary style** (§3.7): `len=3 [1, 2, 3]` and `{"k": v}`, the same in
  every language. The alternatives are gdb's `std::vector of length 3,
  capacity 3 = {1, 2, 3}`, or per-language spellings like Delve's
  `[]int len: 3, cap: 3, [1,2,3]`.
- **D7 Maintenance machinery** (§3.12): `inner()`, upstream test suites
  as uscope tests, and a canary job against nightly toolchains once CI
  exists. Recommended: all three. The canary is what turns "things keep
  breaking for users" into "we fix it the week the library changes".
- **D8 Kernels** (§3.13). Design the contract for them now, and build them
  when the first view needs one.
  - wasmi, a core-wasm `read`/`yield` ABI, no Component Model.
  - Recommended: yes, with the implementation deferred to P7.
- **D9 Presentation worker and page cache** (§3.11, §3.14). Views run off
  the controller thread from P2, so run control never waits on a view.
  Recommended: yes.
- **D10 Standards track** (§7). Recommended:
  - design every public format as if others will implement it;
  - publish the research report and conformance corpus once P3 works;
  - open the producer-side proposals (counted_by, GCC template
    parameters, libc++ `standalone_debug`) early, because they are small
    and help every debugger today.

## 7. Toward a shared standard

Data visualization on Linux is fragmented:

- gdb printers are Python against gdb's API;
- LLDB formatters are C++ or Python against `SBValue`, moving toward a
  bytecode;
- natvis is C++ expressions against Microsoft's evaluators;
- Delve hard-codes Go.

Each library would have to write its visualizers three times, so most
write none, and the ones that exist rot when nobody tests them (§1.2). A
standard has to make the right thing cheap for **producers** (compilers
and libraries), because they are the only people who change layouts. It
works at four levels, each useful without the others.

**L0. Producers emit more semantics in DWARF.** These are the cheapest and
most widely useful changes, since every debugger benefits with no new
format:

- **`counted_by` into DWARF.** C's `__attribute__((counted_by(n)))` (and
  `__counted_by` on pointers in clang) says exactly which member counts an
  array.
  - Checked on 2026-10-05: neither gcc 15 nor clang 21 emits it. The
    flexible array's subrange has no count.
  - gcc 15 also rejects the attribute on pointer members.
  - DWARF 5 already lets `DW_AT_count` be an expression using
    `DW_OP_push_object_address`, so this needs no new DWARF.
  - The Linux kernel annotates hundreds of structures, so this would
    improve kernel debugging (drgn, crash, gdb) as well as uscope.
- **Template parameters on every instantiation.** gcc omits them on 39 of
  309 templates, including `std::allocator<T>`. lldb's libstdc++
  formatters show a two-entry `unordered_map` as empty because of it.
- **libc++ marks the types its containers need for complete debug info**
  (clang's `standalone_debug` attribute). Today the `shared_ptr` control
  block is declaration-only by default, so no debugger can show use counts.
- **Zig's LLVM backend emits `DW_TAG_variant_part`**, as its self-hosted
  backend does, instead of `{payload, some}` records.
- **Rust and others mark transparent wrappers.** `#[repr(transparent)]` is
  the very fact `inner()` infers. Saying it explicitly, perhaps as a
  vendor attribute first, makes it exact.

**L1. Libraries ship declarative views in their binaries.** This is
`.debug_uscope_views` (§3.8), with a debugger-neutral language: its contract
is §3.0, and its patterns name types by identity, not by one compiler's
spelling. The section carries a vendor name until other debuggers want it.
Then the format, not the name, is what gets proposed. A neutral successor
name is chosen together, rather than claimed.

**L2. A tiny executable ABI for algorithms.** That is `read` and `yield`
kernels (§3.13). It is small enough for gdb (C), lldb (C++), and Delve (Go)
to host. It complements LLDB's bytecode rather than competing with it: a
kernel is a pure iterator, and LLDB's selectors could call one.

**L3. A shared conformance corpus.** These are the programs, built across
compilers, with the values a debugger should show:

- uscope's fixtures, including the corrupted containers;
- the upstream printer suites (§3.12).

It measures every debugger by the same standard. Appendix A's failures
(lldb's `Ok(7)` as `Err("")`, the empty `unordered_map`) are exactly what
it would catch. It is the easiest of the four for others to adopt, and the
clearest way to show where things stand.

**Who to talk to, and where:**

| Party | What to talk about | Venue |
|---|---|---|
| LLDB | the formatter bytecode authors and the libc++ "formatters out of LLDB" RFC | LLVM Discourse; LLVM Developers' Meeting |
| GDB | the Rust and DAP maintainers | gdb mailing list; GNU Tools Cauldron |
| GCC | DWARF output | GNU Tools Cauldron |
| Rust | the debuginfo test-suite and visualizer work (the compiler-team MCPs on debuginfo tests), and `#[debugger_visualizer]`, which could gain a declarative kind | t-compiler Zulip |
| Go | Delve | issue trackers |
| Zig | DWARF output | issue tracker |
| The DWARF committee | the L0 items that need the standard | dwarfstd.org issues |
| RAD Debugger and JetBrains | the same problem from Windows and natvis | — |
| All of the above | the debugging and toolchain tracks | Linux Plumbers Conference; FOSDEM |

**Sequencing.**

1. Build first, so the conversation starts from working code and
   measurements rather than a proposal.
2. Once P3 works, publish a report on the state of data visualization on
   Linux, drawing on this plan's research and corpus.
3. Open the L0 proposals early: each is small, independently useful, and
   helps every debugger today.
4. Offer L1 and L2 once uscope has shipped them long enough to know they
   hold up.

## Appendix A. Quality bars observed (2026-10-05)

gdb 17.1 with libstdc++'s printers (gcc and clang, -O0 and -O2 alike):

```text
std::vector of length 3, capacity 3 = {1, 2, 3}
std::map with 2 elements = {["one"] = 1, ["two"] = 2}
std::unordered_map with 2 elements = {[2] = "dos", [1] = "uno"}
std::shared_ptr<Point> (use count 2, weak count 1) = {get() = 0x55555557e870}
std::optional = {[contained value] = 42}      std::variant [index 1] = {"alt"}
```

rust-gdb (debug build):

```text
v = Vec(size=5) = {1, 2, 3, 4, 5}
hm = HashMap(size=3) = {["three"] = 3, ["one"] = 1, ["two"] = 2}
rc = Rc(strong=2, weak=1) = {value = types::Point {x: 3, y: 4}, ...}
m2 = types::Msg::Move{x: 1, y: -2}
```

It fails on `Ref` (Python exception). `Mutex`, `Duration`, `CString`, and
`&Path` print raw, and `Box<dyn>` errors.

Delve (master):

```text
map[string]int ["one": 1, "two": 2, ]
interface {}(main.Point) {X: 1, Y: 2}
error(*errors.errorString) *{s: "boom"}
main.main.func1 {s string = "hello, world"}
```

Failures uscope must not repeat:

- lldb `Ok(7)` shows `Err("")` (64-bit discriminant truncated).
- lldb + gcc libstdc++ `unordered_map` shows `size=0 {}` (missing allocator
  template parameters swallowed by `except`).
- Zig printers show `?u32 = 42` as `null`.
- Go `runtime-gdb.py` prints interfaces raw (`internal/abi.Type` rename).
- gdb shows a garbage `std::vector` as `length 35184372071424`.

## Appendix B. Structural facts the design relies on

| Fact | Evidence |
|---|---|
| Inline namespaces carry `DW_AT_export_symbols` | gcc 15 `std::__cxx11`; clang 21 `std::__cxx11` and `std::__1` |
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

## Appendix C. Maintenance research (2026-10-05)

| Visualizer | Owner | Tested upstream | Reaches users how | Matches the binary |
|---|---|---|---|---|
| MSVC `STL.natvis` | Microsoft STL team | no | with the VS IDE | yes; ABI frozen since 2015 |
| Rust std natvis | rustc | yes (cdb, Windows CI; same-PR fixes) | embedded in PDBs (`/NATVIS`) | yes, on Windows |
| libstdc++ `printers.py` | GCC | yes (306 value checks, also `-flto`) | auto-load beside the runtime `.so` | runtime `.so`; none for static or cores without the `.so` |
| libc++ gdb `printers.py` | LLVM libc++ | yes (~126 assertions) | not installed by distros | manual |
| LLDB libc++ formatters | LLDB (not libc++) | yes (layout simulators) | in liblldb | must handle every layout; chases libc++ months later |
| Rust gdb/lldb providers | rustc | yes (139 gdb, 109 lldb tests) | sysroot + `.debug_gdb_scripts` name | only with the building toolchain active |
| Go `runtime-gdb.py` | Go runtime | partly (never prints an interface) | absolute GOROOT path in the binary | only on the build machine; interfaces broken |
| Zig lldb printers | Zig | 17 lldb cases | `lib/` since 0.17, by hand | manual; wrong on 0.16 |

Commits per year (2019–2026):

- libstdc++ printers: 4–20 (about 4 layout-forced since 2022, each in the
  header commit).
- libc++ gdb printers: 2–6 (about 7 of 30 layout-forced, in the same
  commit).
- LLDB C++ formatters: 30–90.
- Rust natvis: 17 in three years, 13 layout-forced, all in the same PR.
- Go `runtime-gdb.py`: 0–4.

Scratch evidence for these numbers and for appendices A and B (dumps,
scripts, churn tables) was kept in the 2026-10-05 session's scratchpad. It
is not in the repository.
