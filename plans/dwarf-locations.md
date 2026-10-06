# DWARF locations: design record

Optimized code describes most values with more of the DWARF expression
language than a single location: values split across registers and memory
(`DW_OP_piece`, `DW_OP_bit_piece`), values recovered from the caller's call
site (`DW_OP_entry_value`, `DW_OP_GNU_parameter_ref`), pointers to objects
with no address (`DW_OP_implicit_pointer`), and subroutine calls
(`DW_OP_call2`, `DW_OP_call4`, `DW_OP_call_ref`). This records how uscope
evaluates them, why, and the producer facts the design depends on.

## Producer facts

Measured over every fixture binary (GCC 15, Clang 21, rustc, Go 1.26,
Zig 0.16):

- Pieces are everywhere: Go's register ABI, Rust and Clang SROA, GCC's
  split structures. They are whole bytes without offsets, except Zig's
  `DW_OP_bit_piece` with offset 0 on registers, memory, values, and empty
  locations. A third of composites leave some piece empty.
- Some pieces exceed what they read: Go's runtime has a 24-byte piece of
  `rbx`, Zig a 16-byte piece of a generic stack value.
- `DW_OP_entry_value` operands are a single register (`DW_OP_regN`), and
  rarely `DW_OP_regval_type`; never `DW_OP_bregN 0; DW_OP_deref`.
- GCC and Clang mark their subprograms `DW_AT_call_all_calls` and describe
  call sites with `DW_TAG_call_site`, keyed by `DW_AT_call_return_pc`.
  Every call-site parameter is a register, or, for GCC's IPA clones, a
  `DW_AT_call_parameter` reference that `DW_OP_GNU_parameter_ref` names.
  Most call origins are declarations; some are abstract instance roots,
  whose code is a concrete out-of-line instance. Rust, Go, and Zig emit no
  call sites, so their entry values are unavailable.
- Implicit pointers name local variables and parameters, mostly inlined
  ones with no location, and GCC's `DW_TAG_dwarf_procedure` string
  literals held as `DW_OP_implicit_value`. Zig's name missing entries.
- No producer emits `DW_OP_call*`, `DW_OP_GNU_variable_value`,
  `DW_OP_GNU_uninit`, `DW_OP_xderef`, or `DW_AT_call_data_value`.

## Architecture

1. **Evaluation** (`variables/evaluate.rs`) drives gimli and answers what it
   requires, including entry values and called procedures.
2. **Pieces become storage** (`variables/pieces.rs`): each piece is resolved
   once, at the stop: registers, stack values, and implicit values are
   captured as bytes; memory stays an address read lazily; implicit pointers
   and undefined pieces are kept as what they are. One piece covering the
   object is the familiar `Memory`, `Bytes`, or `ImplicitPointer` storage;
   anything else is `ValueStorage::Composite`.
3. **Storage operations** (`variables/storage.rs`) are the only code that
   selects, reads, or describes storage. Selecting a member or element of a
   composite that lies within one piece collapses to that piece's own
   storage, so every later step, from `&s.member` to watchpoints, children
   pages, and assignment, works as for any value in that place.
4. **Call sites** (`variables/call_sites.rs`) are cataloged at load:
   per return address, the call's target and the parameters it passes; per
   function, whether a chain of tail calls may enter it again.
5. **Callers** (`backend/linux/callers.rs`) answer a provider's entry-value
   request: unwind one activation, find the caller's module and call site,
   resolve the call's target, follow the one chain of tail calls from it to
   the frame's function, and evaluate the parameter where it was passed:
   in the caller's frame, with a runtime of its own that may recurse, or in
   a virtual frame for the last tail call.

## Decisions

**D1. Exactness over completeness.** Every rule below refuses with a typed
reason where GDB or LLDB would guess: zero-filling a piece beyond its value,
reading a register past its end, trusting a call site when the function may
have re-entered itself by tail calls, or matching a callee by name when two
modules define it.

**D2. Composite reads are bit-exact.** A read assembles the bits it needs
and reports the undefined bits it would cover as
`OptimizedOut(UndefinedPieces)` relative to the read, or `EmptyLocation`
when it covers no defined bit. A scalar with any undefined bit is
unavailable; an aggregate is available and each child is judged by its own
bits. Bit-fields read only their own bits. Bits an implicit pointer holds
are undefined to a read: the pointer has no value, only a referent.

**D3. Piece rules.** Register and stack-value pieces take the low-order
bits, offset from the least significant bit (DWARF 5 2.6.1.2). A register
piece must fit its register. A stack-value piece wider than its value
extends a typed value by its signedness and a generic value with zeros only
when its sign bit is clear; otherwise it is malformed. An implicit value
must cover its piece. An implicit pointer piece has no bit offset. Pieces
that describe fewer bits than the object leave the rest undefined; more is
malformed. Non-byte-aligned pieces on big-endian targets are unsupported.

**D4. Sources.** A value selected within one piece reports that piece's
source: `Memory` at its address, `Register` only when it starts at the
register's least significant bit, `Computed` or `Constant` for values.
Bytes assembled from several pieces, or a register's upper part, report
`Composite`, which has no address and cannot be assigned or watched.

**D5. Entry values follow the call site.** The value of `DW_OP_entry_value`
(and `DW_OP_GNU_parameter_ref`) is the call-site parameter's
`DW_AT_call_value` evaluated in the caller's frame, which is the physical
activation that called the frame's physical function. It is available only
when:

- the caller exists and is not a signal frame;
- its module describes exactly one call site returning to its return
  address (tail-call sites have none);
- the call site's target is known: the target's own code or the one
  concrete instance of an abstract target; `DW_AT_call_target` evaluated in
  the caller, which is unknown once the call clobbered what it names; or a
  symbol name that the loaded modules define, non-locally, at one address,
  as for a call through the procedure linkage table;
- exactly one chain of tail calls leads from the target to the frame's
  function, the empty chain when the target is the function itself. Every
  function the target's tail calls can reach must describe all its calls,
  and each tail call's target must be a function of the same module; a
  cycle or a second chain leading to the frame's function refuses with
  `TailCalls`, and a function the target cannot reach with
  `TargetMismatch`;
- the call that passed the parameter, the last link of the chain or the
  call site itself, passes it: the same register, or for a parameter
  reference the same parameter entry in the same module.

A tail call's value is evaluated in a virtual frame of the function that
made it, which left nothing behind but the values it was entered with: its
registers, memory, CFA, and thread-local storage are `DiscardedState`, and
its own entry values follow the previous link. The value keeps the type of
`DW_AT_call_value`'s result; a typed register operand reinterprets its
low-order bytes. Chains (a call value that is itself an entry value)
recurse to the next caller, to a fixed depth. What a caller cannot provide
is reported as `EntryValue(Caller(reason))`, so a reason found one caller
further out is never mistaken for one about the frame's own caller.

**D6. Implicit pointer referents** are cataloged data objects, located at
the frame's instruction, or `DW_TAG_dwarf_procedure` entries holding a
single location, evaluated without a frame base. A referent's bounds come
from its type, or from the bytes its location holds.

**D7. `DW_OP_call*`** runs the referenced entry's `DW_AT_location` on the
same stack. An entry without one has no effect (DWARF 5 2.5.1.5); a
location list is selected at the frame's instruction. Procedures are copied
at load with the expressions that call them.

## Tests

- Unit: the piece rules and composite reads against a bit-array oracle
  (property test); entry-value request decoding and called procedures; the
  tail-call analysis over cataloged graphs.
- Scenarios (`tests/debugger/locations.rs`, over `tests/fixtures/c/locations`
  built by GCC and Clang at `-O2`, PIE and non-PIE): split records,
  partially optimized-out values, register halves, `__int128`, implicit
  pointer members, entry-value chains, a unique chain of tail calls and a
  refused cycle, IPA parameter references, a library function called
  through the procedure linkage table, calls through a pointer the caller
  kept or lost, reasons from a caller further out, and refused assignment
  and watches of split values; the existing optimized fixtures, whose
  unsupported values became available.
- Differential: gdb's reading of every frame of dumped cores, extended to
  record members, over the new fixture's cores.
- Simulation: the golden binaries' `facts.json` records where binutils say a
  variable is a register's entry value, and the entry-values oracle checks
  the debugger's value against the register the simulated call entered
  with. No golden program is described with pieces.

## Follow-ups

- An x87 `long double` whose padding a piece leaves undefined could be
  decoded from its 80 value bits; it is `UndefinedPieces` now, as in gdb.
- Views cannot take `&` of a member of a composite value, so a view that
  needs a member's address reports `Refused` for optimized values.
- Backtraces could show the virtual frames of the tail-call chain an entry
  value follows, as gdb does.
