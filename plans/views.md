# Data views: design record

Views (`docs/views.md`) present a value as what it stands for, with the
stored value one step away. This records how the design fits together, why,
the library facts the built-in views depend on, and what is known not to
work. The reference and the code say the rest.

## Goals

- Built-in and user views are written the same way, in one small language,
  and the built-in ones track the newest version of each pinned toolchain.
- A view cannot harm the debugger: no crash, hang, unbounded memory, delay
  to run control, or convincing wrong answer. A value a view cannot make
  sense of shows as stored, with a typed reason.
- Views run in the existing inspection paths, not beside them.

## Architecture

Three layers:

1. **The debug-info provider normalizes structure and identity**, with no
   views involved: sums (`DW_TAG_variant_part`, and Zig's LLVM-backend
   optionals, error unions, and tagged unions by producer and shape), fat
   pointers by shape rather than name, text by encoding, and dynamic types
   (C++ vtable symbols, Rust `<C as Trait>::{vtable}` variables, Go
   `runtime.types`). Each type carries a `TypeIdentity`: its language, its
   path without inline namespaces (and the removed ones, which a name may
   spell), its base name, its arguments by position, where a C++ pack
   begins, and Go's kind attributes. Arguments come from template parameter
   entries, or from parsing the name where DWARF has none (GCC omits them on
   some templates; Go and Zig never emit them). A per-image `TypeIndex`,
   built at load, finds instances by identity. See `src/type_identity.rs`
   and `src/debug_info/dwarf/variables/identity.rs`.
2. **`src/view` is the language**, pure under the same boundary test as
   `src/eval`: `syntax` parses files, `pattern` matches identities, `bind`
   binds a view against one concrete type, `run` and `scan` present a value
   through the evaluator's `Machine`, `kernel` hosts WebAssembly kernels,
   `format` and `summary` write values, `embedded` reads a module's
   section, and `fuzz` is the hostile harness.
3. **The presentation model**: `VariableState::Available` carries an
   optional `Presentation`, and a presented value's children are its
   elements or entries, its fields, and one `[raw]`.
   `src/backend/linux/presentation.rs` chooses each type's view (cached per
   view set), presents values on every inspection path, and keeps scan
   checkpoints per stop.

## Decisions

**A declarative language on uscope's own expressions.** natvis is the
richest vocabulary but not portable (its hosts' evaluators disagree), LLDB's
compiled formatters lag the libraries they describe, and Python printers
and GPL sources are excluded outright: uscope does not run, read, port, or
test against them, and embeds no interpreter. Native Rust views are allowed
only with a stated reason; none exist.

**Bind before running.** A view is bound against each concrete type before
any value is presented: every member, type, and expression must resolve.
A view that does not bind is skipped with its reason, which `info view`
shows. Two layouts of a library are two views, or `or` alternatives, never
version detection: the program's debug information says which layout it
has.

**Match identities, never spellings.** GCC writes `pair<int const, …>` and
`array<int, 4>` where clang writes `pair<const int, …>` and `array<int,
4UL>`, `-gsimple-template-names` emits bare names, and rustc names slices
differently at `-O`. Patterns anchor at the root and base name, `**` spans
moved modules, and `inner()` absorbs wrapper layers: of the 13 Rust std
changes that broke natvis from 2023 to 2026, 11 added, removed, or renamed
a wrapper or module. Go types match by `DW_AT_go_kind`, as Delve does.

**Views run on the controller thread.** There is no worker thread. Budgets
are the timeouts: every read, generator step, and kernel instruction is
charged to the request's `InspectionBudget`, so the same inputs always stop
at the same point and no clock is read. A top-level presentation runs on a
quarter of what remains (`InspectionBudget::share`), so running out ends a
summary early without failing the inspection, and a page of children ends
at the first child it cannot afford.

**Run control comes first.** `next_message` serves a message that reads one
stop (`reads_one_stop`) after run control or a wait event queued behind it
(`preempts_inspection`), so the inspection fails as one for an old stop
does. Evaluation and presentation check every `INTERRUPT_INTERVAL` units
whether run control waits, and if so the request is served again after it
(`serve_later`). Conditions, log messages, and assignments are never
interrupted.

**Scans resume, and never pass their count.** A scan's state is plain data,
kept every 256 elements per stop, so a later page resumes rather than
restarts. A list finds a revisited node exactly within one request and by
Brent's algorithm across resumed ones. A scan stops at its declared count:
reading on to find more would make a sparse hash table scan every empty
slot to prove there are none.

**Views load without asking.** They cannot write, call, or perform I/O, so
session, project, user, and embedded views all load silently; a broken one
costs only its own values.

**The embedded section is not `ALLOC`.** A C `section` attribute and a Rust
`#[link_section]` static both make a section that is loaded at run time and
survives `strip --strip-debug`. The C header and Rust macro emit
`.pushsection .debug_uscope_views,"",@progbits` with `.incbin` in assembly
instead, as gdb documents for `.debug_gdb_scripts`: a C string cannot carry
view text into `asm`, and Rust's `global_asm!` reads braces. Their labels
avoid `0` and `1`, which Intel syntax reads as binary. A module's views
present only its own types, so one library cannot restyle another's.

**Kernels are core WebAssembly with two imports.** Algorithms the language
cannot express (a `BTreeMap`'s walk) call a kernel that imports only `read`
and `yield` from `uscope_kernel_v1`. Two imports are trivial to keep
stable and to host anywhere; the Component Model is unfinished and only
wasmtime implements it; and whole views in WebAssembly would freeze the
entire view contract as a binary ABI. `read` and `yield` stop the kernel as
resumable host traps, so the store never holds the program and a run is a
pure function of its arguments and reads, which is what makes recordings
replay offline.

**The runtime is wasmi.** wasmtime is about six times faster but brings 74
crates and 11 MB, compiles each module in 10–15 ms, installs process-wide
signal handlers, and can abort on native stack overflow, all worse inside a
ptrace debugger than the speed matters; view work is dominated by memory
reads. wasmi compiles eagerly, meters fuel deterministically, bounds
recursion on heap stacks, and installs no handlers. Every call into it is
wrapped in `catch_unwind`, since wasmi has panicked on a valid module
before. A kernel runs out of fuel every 4096 instructions, which the scan
pays for as 512 units of work. A scan with a kernel keeps no checkpoints:
wasmi cannot copy a store, so a later page runs the kernel again.

**One summary style** for every language and client; the CLI and DAP render
plain values with `view::summary` too, so a summary and a printed value
agree.

**Newest toolchains only.** The built-in views target the pinned
toolchains; an alternative for an older layout stays while it is cheap.
Drift is caught by the gate: every built-in view must present a marked
value in every build of the `containers` fixtures.

## Library facts the views depend on

- **libstdc++.** The copy-on-write ABI keeps a string's length, capacity,
  and refcount in three words before its characters, which DWARF does not
  describe. Trees, lists, and hash tables reach their nodes through types
  their allocators' arguments name. The old ABI's `std::list` keeps no
  size. Debug mode keeps `list` and `forward_list` nodes in
  `std::__cxx1998`, and its containers begin with safe-sequence
  bookkeeping. `std::__debug` is an ordinary namespace outside debug mode.
- **libc++.** A long string's capacity is stored halved on little-endian
  targets. The deque's block size is in neither the object nor its DWARF,
  so the view states libc++'s rule (4096 bytes of elements, or 16 elements
  of 256 bytes or more) and checks it against the block map. A
  `shared_ptr`'s control block is described only with
  `-fstandalone-debug`. An empty `std::array` is presented by
  `libstdc++.views`'s view, which reads nothing.
- **Rust.** rustc emits every pointer to a slice or `str` as
  `{data_ptr, length}` outside any module, whatever it names it; a pointer
  to a type with an unsized tail has the same shape but stays a record.
  rustc describes type parameters but not const ones, so the name fills
  those positions. hashbrown keeps buckets below its control bytes, a full
  one's top bit clear. A `BTreeMap` leaf is reordered by rustc (values
  before keys) and its length is a `u16`; the view passes every offset and
  size from the debug information, and an empty map's root may be `None` or
  an emptied leaf. `-C debuginfo=limited` describes no variables.
- **Go 1.26.** Maps are swiss tables only: a small map's single group is
  `dirPtr`, and a table's first directory slot is its `index`. Whether an
  interface stores its value directly is `TFlag` bit 5 (`Kind_` bit 5
  before 1.26). A channel is a pointer only in representation.
- **Zig 0.16.** `std.ArrayList` is `array_list.Aligned`, and the managed
  list `array_list.AlignedManaged`. Array hash maps are unmanaged only, and
  neither ReleaseSafe nor the self-hosted backend emits their entry type.
  The self-hosted backend names a nested type by its own name with
  `DW_AT_ZIG_parent`, and describes optionals, error unions, and tagged
  unions as variant parts, and `?*T` as a variant part whose discriminant
  is the pointer's bits; the loader reads both backends' shapes alike.
- **C++ dynamic types.** A vptr must point into a `vtable for X` symbol,
  and the offset-to-top before it must lead to an object whose own vptr is
  that group's primary address point. Clang names some thunks only by
  their linkage names.

## Testing

- Every example in `docs/views.md` runs against the fake world in
  `src/view/tests.rs`, beside unit tests of the scan, budgets, construction,
  and the parser.
- The hostile harness (`src/view/fuzz.rs`) runs every built-in view and
  arbitrary view text over arbitrary memory, as a proptest in the suite and
  as `just fuzz views`.
- The `containers` fixtures in every language carry `VIEW:` markers checked
  in every build of the C++, Rust, Go, and Zig matrices
  (`tests/debugger/views.rs`), including corrupted instances. The tutorial's
  fixtures keep `docs/writing-views.md` true.
- The simulator's golden `containers` program and views oracle
  (`src/sim/views.rs`) check presented elements against memory; its
  sabotage is `SkipLinkedNodes`.

## Known limits

- Unsized tails (`Rc<str>`, `&Path`) show as stored: rustc describes `str`
  and `[u8]` alike, so nothing says which is text.
- Tuples of more than six, `Rc<dyn Trait>`, and a libc++ `weak_ptr`
  without `-fstandalone-debug` show as stored; a Go interface holding a
  pointer to anything but a struct shows its address.
- Zig array hash maps show as stored (no entry type).
- Text is read up to 256 bytes everywhere; there is no per-request text
  limit.
- A view cannot index a value through another view.
- A page of 256 children slightly exceeds the default 256 reads, so a
  client asking for more than about 250 elements at once gets a short page.
- A cycle leading back past the checkpoint a page resumed from is found by
  Brent's algorithm within a few times its length, so pages before the one
  that reports it may repeat nodes.
- Each tree link is its own read, about four reads an entry; there is no
  read cache. Paging deep into a `BTreeMap` costs the reads of every page
  before it.
- Maps are not indexed by key.
- `views explain` lists a typedef and its target separately, and `views
  check` checks a library's views only in a session that has loaded it.
- The simulator runs no kernels.

## Later

If views prove out, the ideas worth taking beyond uscope are: producers
emitting more semantics in DWARF (C's `counted_by` as a `DW_AT_count`
expression, GCC template parameters on every instance, a complete libc++
`shared_ptr` control block by default, `DW_TAG_variant_part` from Zig's
LLVM backend, a marker for transparent wrappers); libraries shipping
declarative views in their binaries; the kernel ABI as a shared one; and a
shared conformance corpus of programs and the values a debugger should
show. A natvis importer and reading LLDB formatter bytecode wait for users
to ask.
