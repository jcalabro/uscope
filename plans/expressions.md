# Expression evaluation: design record

`docs/expressions.md` is the language reference, and every example in it runs
as a test. This record keeps what the reference does not: why the language is
shaped as it is, how the evaluator is built, and how it is tested.

## The language

**One neutral language** reads values in C, C++, Rust, Go, and Zig, with one
parser and no per-language parsers, type checkers, compilers, or external
debuggers. Per-language emulation gets the promotion rules wrong in ways that
look right: gdb prints `(short)-70000` as `4464` and gives Rust C's
promotions, and lldb parses Rust as C++.

**Integer arithmetic is exact.** `+ - * / %` give the true result in
[−2^127, 2^128 − 1], exactly what `ScalarValue::Signed(i128)` or
`Unsigned(u128)` holds, so every result that computes can be shown; beyond it
is an error. Only casts truncate. **Bit operations keep their operand's
width**, because they describe a representation rather than a quantity.

**Casts are written `(T)x` and `x as T`.** Both forms stay because each is the
natural spelling in half the supported languages. `(X) - y`, `(X) * y`, and
`(X) & y` are ambiguous: a value `X` groups as `X - y`, a type `X` casts
`-y`, and the two readings group the rest of the expression differently, so
no single tree with a "cast or parenthesis" node holds both. The parser finds
such spots from the tokens alone and parses the text once per combination of
readings (at most four ambiguities, sixteen trees). Binding resolves each name
and picks the matching tree: a value in scope wins, as in C, otherwise a type,
otherwise an unknown-name error. No reading is refused merely for being
ambiguous, and such text prints as written, since parentheses one reading does
not need may matter to another.

**Two spellings of null.** `null` and Go's `nil` mean the same and print as
`null`; `nullptr` and `NULL` are refused with a hint to write `null`. A Go
programmer writes `nil` without thinking, and refusing it only taught a
rule; a variable named `nil`, which Go allows and nobody writes, is still
reached in backticks. A condition still reads the same in every language,
since both spellings mean one thing everywhere. C's `NULL` is a macro the
debugger does not expand, and a third spelling would add nothing.

**Unsupported operands are a category, not a special case.** A type the
debugger cannot compute with (a pointer into another address space, a
malformed or unknown float format) is `Opaque`: it can be shown, sized, and
addressed, and every operator that needs its value refuses it by
construction.

**Type names mean what the stopped code calls them.** A type name is looked up
in the frame's own module first, and in other modules only when that one has
none. Within the module that answers, same-named types are one type only when
their definitions match; otherwise the name is ambiguous and the error lists
the candidates. Pointer width and byte order come from the target, never from
the host.

Function calls, overloaded operators, and per-language data shapes are out of
scope. Per-language support stays at the common types (scalars, records,
arrays, pointers, and the strings and slices the providers already read); C++
static members, Go interface conversion and type assertions, `T(x)`
conversions, and Rust enum payloads each wait for a need.

**Containers are reached through their views.** `m[key]` searches the
entries a map's view presents for the key `==` would call equal, and
`cap(x)` reads a view's `capacity` field, so neither knows a library's
layout. A map is never hashed, since its hash function is the program's:
a lookup reads every entry before the one it finds, and the inspection's
budget bounds it. A missing key is an error, never a zero value. Whether a
pointer only stands for a container, as Go's maps and channels do, is the
scope's answer, so the evaluator names no language.

**A slice of an array is a range, and a slice of text is text.** The
language builds no aggregates, so `a[i:j]` of an array or slice is the
range `a[i..j]`, with its bounds checked and optional; a slice of text is
a string, because comparing part of one is what conditions need.

**`$task` is a capability of the machine**, as a register is: the debugger
says which task a thread runs, and the evaluator knows no runtime.

## Architecture

```
text --parse--> Expression --bind(Scope)--> Program --run(Machine)--> Evaluation
```

- **Parsing needs no program** (`src/eval/syntax`). It is total and pure, so
  clients report syntax errors without a round trip and a pending
  breakpoint's condition is checked when it is set. The lexer, one Pratt
  parser, and one printer share one precedence table. The printer's normal
  form always parses back to the same readings, which makes it safe for DAP
  `evaluateName`s.
- **Binding is separate from running** (`bind.rs`, `ir.rs`). Binding resolves
  names, types, and enumerators in one frame scope through the `Scope` trait
  and emits a typed tree in which every conversion is explicit. A bound
  object names a catalog entry, not a location, so its storage is found again
  on every run and a bound program stays correct as the pc moves within its
  scope and across location lists.
- **Running reads one validated stop** (`interp.rs`) through the `Machine`
  trait. Places stay unread until a value is needed; `&&`, `||`, and `?:` run
  only the side they need; and a value the program state cannot provide
  poisons what depends on it, keeping the span of the operand that caused it.
  An unavailable value is a result, never an error and never a guess.
- **The evaluator is pure.** `Scope` and `Machine` (`target.rs`) are its only
  way to a program; a boundary test keeps process control, debug information,
  I/O, clocks, and threads out of `src/eval`.
- **Data layout belongs to the debug-info providers.** The evaluator asks them
  to plan a step from types, follow it from a place, decode a scalar, and
  present a value; it never computes an offset. Results are `InspectedValue`s,
  so renderers, paging, dereference, and DAP presentation need nothing new.
- The controller implements both traits in
  `src/backend/linux/evaluation.rs`: `Frame` names a resolved frame's locals,
  then globals across modules, enumerators, and types; `StopMachine` reads
  the stop, dispatching each place to the module that owns it.
- Conditions and log messages (`src/condition.rs`) hold parsed expressions and
  bind at each hit. Caching a bound program per scope waits until a profile
  shows the need.

## Limits

Text is at most 4096 bytes, nesting 64 deep, a reading 1024 nodes, and a bound
program 4096 nodes. Evaluation work is charged per node and step against the
inspection budget, and memory through `InspectionBudget`. Every loop that
collects output consumes input on each pass (the lexer asserts it per token),
so a bug spins until a timeout kills it rather than exhausting memory.

## Testing

Few tests, each with high leverage.

- **The executable reference.** `src/eval/tests.rs` runs every
  `uscope-example` block of `docs/expressions.md` against named worlds in
  `src/eval/fake.rs`: small, deterministic implementations of `Scope` and
  `Machine` with the simplest layouts, memory that can fault, regions that
  fail the test if read, a read log, and a work budget. Each semantic rule
  has a normal, a boundary, and an error example, including the known bug
  classes (`-4 >> 1`, `-1 << 1u32`, signed enums, `(f64)BLUE`, `(bool)0.1`,
  array decay, `(short)-70000`, `(n) - 1` against `(T) - 1`).
- **Numbers** (`number/tests.rs`) are checked against native `i128`/`u128`
  arithmetic wherever it is exact: every pair of small integers under every
  operator, every 16-bit value truncated to every width from 1 to 128,
  special floats to every native integer type, and laws by property.
- **Syntax** (`syntax/tests.rs`) checks every ordered pair of binary operators
  against an independently written precedence table, both readings of each
  ambiguity, and by property that the normal form reads back the same and
  every span is balanced and nested. The `expression_parse` fuzz target runs
  the same invariants.
- **Evaluator properties**: short circuits read nothing they skip, reads
  touch exactly the bytes needed, a program bound once runs on other data as
  a freshly bound one, any smaller work budget ends in an unavailable value
  rather than a wrong one, and generated expressions evaluate
  deterministically to the type the binder computed.
- **Real programs**: native fixtures across the compiler matrix, debugger
  scenarios, and CLI and DAP tests exercise evaluation end to end.
- **The simulator** judges evaluation against the simulated machine's ground
  truth (`semantics::evaluations`). At a stop the client evaluates the marker
  condition on the frame's line and its negation, variables by name and as
  `&x` and `*&x`, drawn sums, differences, and products, narrowing casts in
  either form, ill-typed expressions that must be refused, and the `data`
  golden program's `// EXPECT:` expressions over records, arrays, and
  pointers. The oracle requires a true marker and a false negation where the
  variables oracle applies, values and bytes equal to the simulated memory
  and registers, `&x` where the view says `x` lives, exact arithmetic, casts
  that keep exactly the low bits, and refusals. Sabotage kernels
  (`SkewSmallStackWords`, `FlickeringStackWords`, `SkewSmallRegisters`) show
  each check catches the lie it exists for. Floats and bit-fields stay out of
  the simulator, whose CPU has no SSE.

Not used: compiler or debugger oracles at test time, snapshots for semantics,
wall-clock assertions, negative waits, and mutation testing.
