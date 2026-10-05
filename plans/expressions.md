# Expression evaluation plan (v2)

TODO item: *Expand expression evaluation beyond structural value inspection.*

## 0. Where this comes from

A first attempt (v1) was built on 2026-10-02 and 03 in the deleted
`expressions` worktree. Its commits survive as `refs/archive/expressions-v1`
(tip 4a3c4e1, 25 commits on d824dad, 135 files, +23k/−3k lines), with its plan
at `plans/expressions.md` and its reference at `docs/expressions.md` in that
tree. Read them with `git show refs/archive/expressions-v1:<path>`. It is a
reference, not a source to merge: `next` has moved 52 commits since, including
the simulator, and the code is rewritten here phase by phase.

### What v1 got right, and v2 keeps

- **One neutral language** for C, C++, Rust, Go, and Zig, with one parser and
  no per-language parsers, type checkers, compilers, or external debuggers.
- **Exact integer arithmetic.** `+ - * / %` give the true result; only casts
  truncate. **Bit operations keep their operand's width.** This avoids the
  promotion bugs every per-language emulation has (gdb 17.1 prints
  `(short)-70000` as `4464`, gives Rust C's promotions, and lldb parses Rust as
  C++).
- **Parsing needs no program.** Text to syntax tree is total and pure, so
  clients report syntax errors without a round trip, and a pending
  breakpoint's condition is checked when set.
- **Binding is separate from running.** Binding resolves names and types in
  one frame scope into a program a condition reuses at every hit there.
  Running reads one validated stop.
- **The evaluator is pure and reaches the program only through two traits**
  (`Scope` at bind time, `Machine` at run time). A boundary test keeps the
  backend, providers, I/O, clocks, and threads out of `src/eval`.
- **Data layout belongs to the debug-info provider.** The evaluator asks it to
  locate, step, decode, and materialize; it never computes an offset.
- **Results are `InspectedValue`s**, so renderers, paging, dereference, and DAP
  presentation work unchanged. A value the program cannot provide is an
  unavailable result pointing at its cause, never an error and never a guess.
- **The reference is executable.** Every example in `docs/expressions.md`
  runs as a test.

### What went wrong in v1, and what v2 changes

| v1 problem | v2 response |
|---|---|
| One 23k-line branch, never merged; review needed ten roast groups and still found real bugs late (pointers into other address spaces read the wrong memory; same-named types silently merged). | Each phase lands on `next` on its own, small enough for one review, with the gate and (for run control) stress and a sim sweep. Nothing waits on a long-lived branch. |
| Ambiguous corners settled ad hoc: `(name) - 1` was refused as a possible cast, and four spellings of null each needed handling. | Both cast forms stay (D1), but `(X)` followed by an operand has one written rule, settled at binding the way C settles it: a variable in scope wins, otherwise a type (§2). One spelling of null. |
| Exact integers ranged over ±(2^128 − 1), but nothing could present results below −2^127, so a hole of computable but unshowable values needed its own error. | Exact results range over exactly what `ScalarValue::Signed(i128)` or `Unsigned(u128)` holds: [−2^127, 2^128 − 1]. Every result that computes can be shown. |
| Unsupported operands (other address spaces, malformed types) were added late, operator by operator. | An `Opaque` category exists from the start: shown, sized, addressed, and refused by every operator that needs its value, by construction. |
| Type names were merged by kind and size, so different same-named types became one. | Name lookup returns every candidate; two are one type only if structurally equal; otherwise the name is ambiguous with its candidates listed. |
| Pointer width was hard-coded to 8 bytes. | Every derived type takes its width from `TargetDescription` at bind time. |
| Tests ran for a very long time and used so much memory that they OOMed the host at least three times, killing the desktop session. A lexer that stopped advancing filled 78 GB in under 20 seconds across 24 test processes. | Tests have hard resource budgets (§5.0): every test process aborts past a memory cap, every test run is contained, iteration runs only targeted tests that finish in seconds, and the expensive runs happen once, before committing. No mutation testing. |
| A heavy apparatus: a hand-written bignum reference model, a differential fuzzer, three fuzz targets. | Fewer, higher-leverage tests (§5). The reference model is native `i128`/`u128` arithmetic over the domain where it is exact. |
| Per-language data shapes (Go interface casts, C++ statics through objects, Rust enum payloads) were mixed into the core work. | They are separate, individually landed items in the last phase, each mostly provider work plus a fixture. |
| The simulator did not exist yet, so every refactor leaned on hand-written tests alone. | The simulator is the main safety net for the large refactors (P2, P4): its variables, conditions, and watch oracles must stay green through them, and new oracles judge evaluation against the simulated machine's ground truth (§5.6). |

## 1. Where uscope is today

- `src/expression.rs` parses structural paths (`a.b[1]`, `(*p).x`, a terminal
  `[a..b]`) into the public `ValueExpression { steps: [ValuePathStep] }`, used
  by `print`, DAP `evaluate`/`setExpression`, `watch`, and `set var` targets.
- `src/condition.rs` is a separate C-like grammar for breakpoint conditions,
  log messages, and `set var` right-hand sides, collapsing every value to
  `Operand::{Integer, Float, Boolean}`. `src/assign.rs` encodes assignments.
- The DWARF provider already splits locating storage from materializing a
  value: `visible_object`/`located_data_object` find a root, `plan_path` and
  `evaluate_path` fold selectors over a `LocatedStorage`, and
  `materialize_inspected_value` produces the result. `dereference` and
  `value_children` rebuild values from a stored `ValueStorage` the same way.
  That split is the seam the evaluator plugs into.
- The simulator's client sets conditions and log messages (`sim/client`), and
  `sim/markers.rs` has its own small evaluator for `// MARK:` conditions,
  which stays independent as an oracle.

## 2. The language

The user reference is `docs/expressions.md`; this section fixes its rules.

### Lexical

- Identifiers `[A-Za-z_][A-Za-z0-9_]*`, qualified with `::`. Backticks quote
  any name: `` `github.com/acme/pkg.global` ``, `` `{closure#0}` ``.
- A dotted chain `a.b.c` from a name resolves the longest prefix naming a
  global after locals miss, so Go's `main.counter` works (today's rule).
- Registers `$name`, plus `$pc`, `$sp`, `$fp`.
- Integers: decimal, `0x`, `0o`, `0b`, `_` separators, optional built-in type
  suffix (`255u8`, `1_i64`). A leading-zero `017` and C suffixes (`UL`) are
  refused with a hint.
- Floats: `1.5`, `1e9`, `2.5f32`; never ending in `.`, so `a[1..4]` lexes
  cleanly. A literal that rounds to infinity is refused.
- `'a'` is its code point as an exact integer. `"text"` compares with text.
- Keywords: `true`, `false`, `null`, `as`, `sizeof`, and the built-in `len`.
  `nil`, `nullptr`, and `NULL` are refused with a hint to write `null`.

### Precedence (loosest first)

| Operators | Grouping |
|---|---|
| `=` and compound assignments | right, assignment mode only |
| `?:` | right |
| `\|\|` | left |
| `&&` | left |
| `== != < <= > >=` | none (no chaining) |
| `\|`, then `^`, then `&` | left; bit operations bind tighter than comparisons |
| `<< >>` | left |
| `+ -` | left |
| `* / %` | left |
| `as` | left |
| prefix `- ! ~ * &`, casts `(T)x`, `sizeof` | right |
| postfix `.name` `.0` `->name` `[i]` `[a..b]` (terminal) `len(x)` | left |

**Casts are `(T)x` and `x as T` (D1).** Both name a type with one type
grammar: built-ins (`iN`/`uN` for N in 1..=128, `isize`, `usize`, `f32`,
`f64`, `bool`), program type names (qualified, dotted, or backticked),
multi-word C base types in any word order (`unsigned long`),
`struct`/`union`/`enum` tags, and pointers. `const`, `volatile`, and `mut` are
accepted and ignored. A pointer is `*T` in either form, and also `T*` inside
parentheses; after `as` a trailing `*` would read as multiplication
(`x as int * 2`), so it is not a type there.

Parsing stays free of the program. A parenthesized text that can only be a
type (several C base words, a single C base word, a tag, a pointer star, or a
qualifier) followed by an operand is a cast. A parenthesized *name* (`(n)`,
`(ns::n)`, `(main.point)`) followed by a token that can only begin an
operand (a name, a literal, `(`, `!`, `~`) is a cast, and followed by
anything that cannot begin one it only groups. Followed by `-`, `*`, or `&`
it is an *ambiguity*: the two readings group the rest of the expression
differently (`(n) - a * b` is `n - (a * b)`; `(T) - a * b` is
`((T)(-a)) * b`), so no single tree with a "cast or parenthesis" node can
hold both. Ambiguities are found from the tokens alone, and the text is
parsed once per combination of their readings (at most four ambiguities,
so at most sixteen trees). Binding resolves each ambiguity's name and picks
the tree that matches: a value in scope (a variable or an enumerator) wins,
as in C; otherwise a type; otherwise an unknown-name error. No reading is
ever refused merely for being ambiguous. Text with ambiguities prints as
written, since parentheses one reading does not need may matter to another.

### Semantics

| Operation | Rule |
|---|---|
| Categories | Integer (typed, or exact), Float (f32, f64, x87 f80), Bool, Pointer, Text, Aggregate (access only), Opaque (unsupported or malformed: shown, sized, addressed, nothing else). Typedefs, qualifiers, and references are seen through. |
| `+ - * /  %`, unary `-` | Exact; result untyped ("integer"), in [−2^127, 2^128 − 1], else an out-of-range error. `/` truncates toward zero; `%` takes the dividend's sign; dividing by zero is an error. |
| `~ & \| ^ << >>` | At the width of the widest typed operand, keeping its type; unsigned wins at equal width; an exact operand must fit that width. All-exact operands use infinite two's complement. Signed `>>` is arithmetic. A negative shift or one ≥ the width is an error. |
| Comparisons | Exact across signedness and between integers and floats. Pointers compare with pointers and `null`. Text compares bytewise with a string literal, and is unavailable if not read to its end. |
| Floats | In the widest format among operands; integers convert to nearest. Exact literals are f64. IEEE behavior; x87 through `rustc_apfloat`. |
| `! && \|\| ?:` | Bool, or nonzero integers and pointers. Short-circuit: the skipped side is never read. |
| Pointers | `p ± n` scales by the pointee size; `p - q` needs equal pointee sizes; `void*` arithmetic is refused; `p[i]` is `*(p + i)`. Arrays decay in arithmetic and comparison. |
| Indexing | Checked against static bounds (with non-zero lower bounds); slices against their run-time length. |
| Members | `.` on a record or through one pointer; `->` needs a pointer. |
| `&x` | Needs memory; a register or computed value says where it lives. |
| `*p` | A lazy place; `void*` and non-pointers are refused. |
| Casts | Integer to integer truncates two's complement. Integer to float rounds to nearest. Float to integer truncates toward zero and saturates; NaN is an error. Integers and pointers convert as addresses; pointers convert to pointers. Integer to enum is allowed. Anything with a truth value to `bool`. A record cannot be cast by value. |
| Enumerators | Names in scope, and bare names against the other operand's enum type. |
| `sizeof(x \| T)` | Reads nothing. `len(x)`: arrays, slices, strings, character pointers. |
| Registers | The selected frame's, unwound in callers; an unrecoverable one is unavailable. |
| Assignment | The value must fit the target exactly; compound forms follow the same rule. The result is the target read again. |
| Unavailability | Poisons the result, keeping the span of the operand that caused it, unless a short circuit avoids it. |

Excluded: function calls, overloaded operators, implicit conversions beyond
these rules, and per-language behavior.

## 3. Architecture

### 3.1 `src/eval` (pure)

| Module | Responsibility |
|---|---|
| `mod.rs` | Public surface: `Expression`, `Span`, `EvaluationMode`, limits. |
| `number.rs` | `Exact` (i128 ∪ u128 range), `Bits { width, signed, value }`, `Float` (f32, f64, f80), every conversion. |
| `syntax/{lexer,parser,ast,print}.rs` | Tokens with spans; one Pratt parser and one printer from one precedence table; an arena AST. The printer produces DAP `evaluateName`s that always parse back. |
| `category.rs` | Classifies a `TypeInfo` into a category through `Scope::type_info`. |
| `bind.rs` | Names, type names, enumerators, operator legality, `sizeof`; emits the IR. |
| `ir.rs` | A typed tree with every conversion explicit; each node keeps its span. |
| `interp.rs` | Runs IR on a `Machine`: lazy places, short circuits, poisoning, budget charging. |
| `error.rs` | `ExpressionError { kind, span, hint }`. |

The traits, as in v1:

```rust
pub(crate) trait Scope {
    fn scope_key(&self) -> ScopeKey;
    fn lookup(&self, name: &str) -> Result<Binding, LookupError>;   // ambiguity is typed
    fn lookup_type(&self, name: &TypeName) -> Result<TypeRef, LookupError>;
    fn type_info(&self, ty: TypeRef) -> Option<TypeInfo>;
    fn derive(&mut self, ty: DerivedType) -> TypeRef;                // pointer-to, built-ins
    fn register(&self, name: &str) -> Option<RegisterId>;
    fn target(&self) -> TargetDescription;                           // pointer width, endianness
}

pub(crate) trait Machine {
    fn locate(&mut self, binding: &Binding, budget: &mut InspectionBudget) -> Result<Located, Unavailable>;
    fn step(&mut self, from: &Located, step: &Selector, budget: &mut InspectionBudget) -> Result<Located, Unavailable>;
    fn scalar(&mut self, at: &Located, budget: &mut InspectionBudget) -> Result<Scalar, Unavailable>;
    fn text(&mut self, at: &Located, limit: usize, budget: &mut InspectionBudget) -> Result<TextSummary, Unavailable>;
    fn register(&mut self, id: RegisterId) -> Result<Scalar, Unavailable>;
    fn materialize(&mut self, at: &Located, budget: &mut InspectionBudget) -> crate::Result<InspectedValue>;
    fn write(&mut self, at: &Located, bytes: &[u8]) -> crate::Result<()>;   // assignment mode only
}
```

A `Binding` names a catalog entry, not a location: locations are found again
on every run, so a cached program stays correct as the pc moves within its
scope and across location lists.

### 3.2 Provider (`debug_info`)

Split today's code into primitives; write no new layout logic:
`resolve_object`/`locate_object`, `locate_global`, `step` (one iteration of
today's `plan_path` + `evaluate_path`), `materialize`, `type_named` (a lazily
built per-image index, C base spellings normalized, returning every
candidate), and `scope_names` for suggestions and DAP completion. Today's
`inspect_path` and `inspect_global_path` become folds over them until P4
deletes them.

### 3.3 Controller (`backend/linux/evaluation.rs`)

- `FrameScope` implements `Scope` from a resolved frame: locals and
  parameters, then globals across modules (ambiguity typed, candidates named
  by file), then the CU's enumerators, then types.
- `StopMachine` implements `Machine`, dispatching each `Located` to the module
  owning its image; registers come from the frame's (unwound) registers;
  writes reuse `writes.rs`.
- `DerivedTypes`: a controller-owned, append-only, structurally interned
  arena under a reserved image id for pointer-to-T and built-ins.
  `DebuggerHandle::type_info` consults images or the arena.
- `Request::Evaluate { expression, mode, limits, stop_id, thread_id, frame }`:
  validate the stop, refuse after exec, bind, run, present. Recorded with
  `record!`.
- Conditions and log messages hold `Arc<Expression>` plus programs cached per
  `ScopeKey`; a bind failure at a hit publishes `ConditionFailed` and stops.

### 3.4 Public API and clients

- `Expression::parse(text)`, `Display` (normalized), `text()`.
- `EvaluationMode::{Read, Assign, TypeOnly}`.
- `Evaluation::{Value { value: InspectedValue, cause: Option<Span> }, Range, Type}`.
- `Error::Expression(ExpressionError)`; run-time unavailability is a value.
- Removed in P4: `parse_value_expression`, `ParsedValueExpression`,
  `ValueExpression`, `ValuePathStep`, `Condition`, `Operand`, `LogSegment`'s
  path form, and the inspect/inspect-range/assign requests.
- CLI: `print[/x]`, `whatis`, `ptype`, `set var`, `watch`, `break … if`, with
  caret diagnostics rendered purely from `(text, span, kind, hint)`.
- DAP: `evaluate` in every context (assignment only from the console),
  `setExpression`, `setVariable` via `evaluateName`, `completions`, error ids
  per kind.

### 3.5 Limits

Text 4096 bytes; depth 64; AST nodes 1024; IR nodes 1024; work charged per IR
node and selector; memory through `InspectionBudget`. Every loop that
collects output consumes input on each pass, so a bug spins rather than
allocates.

## 4. Phases

Each phase is test-first, lands on `next` when its checks pass, and is
reviewed (`/roast`) before landing. Fast, targeted checks while iterating;
the full gate at the end of each phase; `just stress` and `just sim 600`
for P2 through P4. With the simulator as the net, a phase may refactor
boldly: P2 and P4 replace whole paths rather than keeping old and new side
by side.

- **P1 Guards, numbers, and syntax** (pure, no wiring; done 2026-10-05). First the resource
  budgets of §5.0; then `number.rs`, lexer, parser, printer, errors; the
  `docs/expressions.md` skeleton with its syntax rows running; the
  `expression_parse` fuzz target. Replaces nothing yet.
- **P2 Provider primitives** (no behavior change). Split the provider into
  `locate`/`step`/`materialize`/`type_named`; today's path inspection folds
  over them. The existing suites are the safety net.
- **P3 Evaluator at a stop.** Category, binder, IR, interpreter, test world;
  `FrameScope`, `StopMachine`, `DerivedTypes`, `Request::Evaluate`;
  CLI `print`/`whatis`/`ptype` and DAP `evaluate`/`completions`. The full
  executable reference. Native fixtures across the matrix.
- **P4 One evaluator everywhere.** Conditions and log messages with cached
  programs; `set var`, `setExpression`, `setVariable`; `watch expr` resolving
  its root storage and extent. The simulator's evaluating client, its
  oracles, and the `data` golden program.
  Delete `condition.rs`, `expression.rs`, `assign.rs`, and the old public path
  types and requests, with their superseded tests.
- **P5 Data shapes**, one commit each: Rust enums and `Option`; Go strings,
  slices, and embedded fields; Zig optionals and error unions; C++ `this`,
  bases, references, and static members; Go interface conversion
  (`err as *main.myError`).

## 5. Testing

Few tests, each high-leverage; every new test is seen failing first. The
simulator carries most of the weight for the refactors; the language itself
gets unit, property, fuzz, and integration tests for its happy and sad paths.

### 5.0 Resource budgets (before any other test is written)

v1's tests OOMed the host three times. These guards land in P1, first, and
every later phase lives inside them.

- **A memory cap in every test process.** A counting global allocator, test
  builds only, aborts the process with a message once live heap exceeds a cap
  (`USCOPE_TEST_MEMORY_CAP` overrides). The default is set from the measured
  peak of today's heaviest test (the libc disassembly and DAP suites) with
  headroom, and is a small fraction of the machine. Unit tests get it
  in `src/lib.rs` under `cfg(test)`; each integration-test binary gets it
  from `tests/support`; the sim binary and fuzz targets get it as well
  (libFuzzer's `-rss_limit_mb` is a second line). A runaway loop now fails one
  test in milliseconds instead of taking the machine. The allocator is the one
  narrow `unsafe` exception, with its safety comment, and a test shows it
  aborts a deliberate over-allocation in a child process.
- **Every test run is contained.** `just test` and `just stress` run nextest
  through `scripts/contained.sh` (scope capped at half of memory, no swap,
  OOM score 1000), as `just sim` already does. nextest's slow-timeout already
  ends spins.
- **Progress is structural.** Every loop that collects output consumes input
  on each pass (the lexer asserts it per token), so a bug spins and is killed
  by a timeout rather than allocating.
- **Fast by construction.** Each new unit or property test finishes in well
  under a second in the test profile; property tests set explicit, small case
  counts (64 by default), raised only by `PROPTEST_CASES` in `just all`.
  Exhaustive tables stay at 8-bit pairs and 16-bit casts (about 200k cases,
  milliseconds at `opt-level = 1`).
- **The loop while developing:** build, Clippy on what changed, and the tests
  the change touches, selected by name, each in seconds. `just`, `just
  stress`, `just sim 600`, and the fuzzers (ten minutes each, contained,
  `-rss_limit_mb=2048`, one job) run once, at the end of a phase, before its
  commit.

### 5.1 Executable reference

`docs/expressions.md` blocks of `expression => value : type`,
`=> error KIND at \`text\``, or `=> unavailable at \`text\``, run against
named test worlds. Every row of §2's semantics has a normal, boundary, and
error example, including the known bug classes (`-4 >> 1`, `-1 << 1u32`,
signed enums, `(f64)ENUM`, `(bool)0.1`, array decay, `(short)-70000`,
`(n) - 1` against `(T)-1`).

### 5.2 Numbers

Exhaustive: every pair of 8-bit values (signed, unsigned, mixed, exact) under
every operator, and every 16-bit value cast to every width 1..=128, against
native `i128`/`u128` arithmetic, which is exact on that domain. Boundary
values at 16, 32, 64, and 128 bits. Every special float to every integer
width. Properties: exact-arithmetic laws, width laws, cast round trips.

### 5.3 Syntax

Unit tables for every token, literal, hint, and error kind with its span;
every ordered pair of binary operators against an independently written
parenthesization; every `(X)` form (type-only, name followed by each
ambiguous and each operand-only token). Properties: `parse(print(ast)) ==
ast`, `print` idempotent, spans balanced, nested, and inside the input.
Fuzzing: `expression_parse` (no panic, limits hold, spans valid, successful
parses round-trip; `value_expression` stays until P4 deletes its parser); `dap_request` gains the
expressions in `evaluate`, `setExpression`, and `setVariable`.

### 5.4 Evaluator (no process)

A small test world implementing `Scope` and `Machine`, with memory that can
fault and regions that fail the test if read (short circuits and `TypeOnly`
read nothing they skip), and a read log. Properties: determinism; `TypeOnly`
agrees on type and reads nothing; a program bound once runs on other data as
a freshly bound one; a smaller budget ends at exactly the first exhausted
resource. Fuzzing: `expression_eval` over generated worlds with poisoned
memory.

### 5.5 Real programs

- **Native fixtures.** `tests/fixtures/{c,cpp,rust,go,zig}` expression
  programs print `EXPECT\t<expression>\t<native value>` lines computed by the
  program itself, then call a `barrier()` in another unit; the test evaluates
  each from the caller frame. Matrix: GCC/Clang × O0/O2 × PIE/non-PIE for C
  and C++, Rust O0/O2, Go `-N -l`/default, Zig Debug/ReleaseSafe. Optimized
  rows equal the truth or are explicitly unavailable with a cause, from a
  reviewed per-cell allowlist; DWARF checks prove optimized builds keep the
  location lists, pieces, and register values those rows rely on.
- **Hand-built DWARF.** Pieces with undefined parts, implicit pointers, stack
  values, odd bit-fields, run-time member layouts, other address classes,
  enums narrower than their storage, malformed types, and same-named types
  defined differently.
- **Scenarios** (`tests/support::Scenario`). Stale `StopId`, running, after
  exec, shutdown in flight; caller frames and unwound registers; inline
  frames; TLS per thread; cross-module ambiguity; conditions false N times
  then true; concurrent conditional hits; log `{e}` equals `print e`;
  assignment fit, refusal, and register targets; watch provenance; exact
  memory usage for member reads and short circuits; live versus core.
- **Clients.** CLI carets for every error kind (including multi-byte text
  before a span); `print/x`; `whatis`/`ptype`. DAP schema checks, hover
  refusing assignment, `evaluateName` round trips for every variable row, and
  CLI `print` = DAP `evaluate` = logpoint text at one stop.

### 5.6 Simulator

The simulator runs the real controller against a simulated kernel and CPU
whose memory and registers are ground truth, so it can judge evaluation
independently of DWARF-reading code paths the debugger shares with itself.

- **Safety net for the refactors.** P2 reroutes all value inspection, and P4
  replaces conditions, log messages, assignment, and watch targets. The
  existing variables, breakpoint-conditions, and watch oracles must stay green
  at the gate's fixed seeds and through a `just sim 600` sweep for each.
- **The client evaluates.** At a stop the simulated client may evaluate
  expressions the seed draws over the names in scope: names, members,
  indices, dereferences, address-of, arithmetic, comparisons, casts, and
  deliberately ill-typed ones. Conditions and log messages it sets use the
  new language, including drawn expressions.
- **Oracles** (`sim/evaluations.rs`):
  - *Markers.* At the start of a marker's line in unoptimized code, the
    marker's condition evaluates to `true` and its negation to `false`;
    `sim/markers.rs` stays the independent evaluator.
  - *Storage truth.* A value the debugger says lives in memory or a register
    has, there in the simulated machine, the bytes it shows, and `&x` is the
    address it says `x` lives at.
  - *Agreement.* For drawn expressions over integer variables, the debugger's
    result equals the marker evaluator's exact arithmetic over the values the
    simulated machine holds; `x` equals the variables view of `x`; `*&x`,
    `p->f`/`(*p).f`, and `a[i]`/`*(a + i)` agree.
  - *Never a wrong value.* Under fault injection (failed reads, vanished
    threads) a result is the truth, explicitly unavailable, or a typed error.
  - *Conditions.* The breakpoint-conditions oracle learns the outcome of any
    drawn condition it can compute from ground truth, narrowing "either
    outcome is accepted".
- **Sabotage tests** for each new oracle: a kernel that misreports stack
  values fails storage truth and agreement; an evaluator that ignores a
  short circuit fails the poisoned-read check.
- **Coverage marks** the gate's fixed seeds must reach: an evaluation that
  holds, one that declines, one unavailable, a cast, an ambiguity error, an
  assignment.
- **Golden programs.** The existing ones hold only integers. A new `data`
  golden program adds records, arrays, pointers, enums, a bit-field, and
  floats with markers over them; recording its manifest is its own commit.

### 5.7 Boundary and what is not used

A boundary test keeps `backend`, `debug_info`, `nix`, `gimli`, `std::fs`,
`std::time`, `std::thread`, and `tokio` out of `src/eval`. Not used: compiler
or debugger oracles at test time, snapshots for semantics, wall-clock
assertions, negative waits, mutation testing.

## 6. Decisions

Decided on 2026-10-05:

- **D1** Casts are both `(T)x` and `x as T` (revised the same day from `as`
  only). Each ambiguity is settled at binding, a value winning (§2).
- **D2** Exact integer results range over [−2^127, 2^128 − 1], what
  `ScalarValue::Signed(i128)` or `Unsigned(u128)` holds; beyond is an
  out-of-range error.
- **D3** Each phase lands on `next` on its own once its checks pass.
