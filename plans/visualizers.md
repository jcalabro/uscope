# Visualizers in every language: gap analysis and plan

TODO.md asks for excellent visualizer support in every supported language.
This records what uscope showed for each language's everyday types on
2026-10-07 (probe programs built with the pinned toolchains: GCC 15.2,
clang 21.1.8 with libstdc++ and libc++, rustc nightly 2026-07-10, Go 1.27.1,
Zig 0.16.0), what this work adds, and what it leaves out on purpose.

## What already worked

- **Views** (`views/`, `docs/views.md`): libstdc++ and libc++ strings,
  vectors, arrays, spans, deques, lists, ordered and unordered containers,
  smart pointers, optional, variant, tuples; Rust `String`, `Vec`,
  `VecDeque`, `HashMap`/`Set`, `BTreeMap`/`Set`, `Box`, `Rc`, `Arc`, `Weak`,
  `Cell`, `RefCell`, `Mutex`, `OsString`, `PathBuf`, `CString`; Go maps,
  channels, `time`, `sync.Mutex`/`RWMutex`, `sync/atomic`, `strings.Builder`,
  `bytes.Buffer`, `[]byte`, errors; Zig `ArrayList`, `HashMap`,
  `ArrayHashMap`.
- **Without views**: C strings and character arrays, Rust `&str` and
  slices, Rust enums, Zig optionals, error unions, tagged unions, slices,
  sentinel strings, Go strings, slices, interfaces, C++ dynamic types, Rust
  trait objects, complex floats, `__int128`, `long double`.
- **Tests**: `VIEW:` markers in the `containers` fixtures (and Go's
  `stdlib`), checked in every build of each language's matrix, with every
  built-in view required to bind in some build (`tests/debugger/views.rs`).

## Gaps found

Debugger (layer 1, no view involved):

1. Character types other than one-byte `char` are unusable. `char8_t`,
   `char16_t`, `char32_t`, and Rust's `char` (`DW_ATE_UTF`) are
   "unsupported representation"; `wchar_t` prints as a bare integer.
2. Wide text: `wchar_t *`, `char16_t *`, `char32_t *` (and arrays of them)
   show only an address, and the `text()` shape takes only one-byte
   characters, so `std::wstring`, `u16string`, `u32string`, and `u8string`
   show as stored in both C++ libraries.
3. A Rust enum with methods (`std::cmp::Ordering`) fails to load as
   malformed: rustc nests `DW_TAG_subprogram`s in the enumeration.
4. `_Float16`, `f16`, and `f128` are unsupported.
5. A C/C++ enumeration of flags prints `3`, not `F_READ | F_WRITE`.
6. `int[2][2]` prints flattened, `[1, 2, 3, 4]`.
7. A Go pointer outside an interface shows only its address; Go's own
   debuggers show the value it points to.
8. Rust tuples print as `{__0 = 1, __1 = "two"}`.

Views missing for common types:

- **C++** (both libraries): `vector<bool>`, `bitset`, `stack`, `queue`,
  `priority_queue`, `chrono::duration` and `time_point`, `atomic`,
  `mutex`, `reference_wrapper`, `expected`, `initializer_list`,
  `flat_map`/`flat_set`, `filesystem::path`, `function`, `unique_ptr<T[]>`,
  `thread::id`, `any` (empty or not).
- **Rust**: `Duration`, `SystemTime`, `Instant`, atomics, `NonZero`,
  `NonNull`, `OnceCell`, `OnceLock`, `RwLock`, `LinkedList`, `BinaryHeap`,
  `Rc<str>`/`Arc<str>`/`Rc<[T]>`, `&Path`/`&OsStr`/`&CStr`, `Wrapping`,
  `Saturating`, `Reverse`, `Pin`.
- **Go**: `sync.WaitGroup`, `sync.Once`, `container/list`, `math/big.Int`,
  named byte slices (`json.RawMessage`) as text.
- **Zig**: `ArrayList(u8)` as text, `Deque`, `PriorityQueue`, `BufSet` (a
  map of `void` values), bit sets, `atomic.Value`, `Io.Writer.Allocating`.
- **C**: glibc's `pthread_mutex_t`.

## Out of scope, and why

- Types whose meaning lives in code uscope never runs: C++ `any`'s and
  `function`'s held types beyond what a symbol names, Go `sync.Map` (a
  concurrent hash trie whose walk needs a kernel; later), `MultiArrayList`
  (a view cannot build a record of an arbitrary element type), Zig's
  intrusive lists (nodes do not name their containers).
- `math/big.Int` beyond two words, which needs decimal conversion of an
  arbitrary-precision number: shown as stored.
- IP addresses as dotted text needs a new format; deferred.

## Phases

1. Characters and wide text (gaps 1, 2) with the C++ wide string views.
2. Layer-1 fixes (gaps 3–8).
3. C++ views. 4. Rust views. 5. Go views. 6. Zig views and the C view.
7. Docs, TODO.md, and the full gate.

Each phase adds markers to the language's `containers` fixture (or a
gallery/scenario test for layer-1 behaviour) before the change, watched to
fail first.
