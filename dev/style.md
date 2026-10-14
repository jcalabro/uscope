# Coding style

This page describes how uscope code is shaped. Read it before adding a data
structure or a function that takes several numbers. It is a starting point and
will grow.

## Code map

| Path | What lives there |
| --- | --- |
| `src/model.rs` | `address_type!`, `id_type!`, and `numeric_id!`, the shared newtype macros |
| `src/image/` | Flat, validated tables indexed by dense ids |

## Data-oriented design

We write data-oriented code. The idea comes from Mike Acton's
"Data-Oriented Design and C++" and Andrew Kelley's "A Practical Guide to
Applying Data-Oriented Design" talks. Start from the data and how it is read,
not from an object model.

- Store many things of one kind in flat `Vec`s and refer to them by a dense
  `u32` id, not by pointer, `Rc`, or `Box`.
- Split rarely used or variable-sized fields into their own tables.
- Keep hot records small and `Copy`. Prefer a few large allocations over many
  small ones.
- Process items in batches over contiguous memory.

## Newtypes

Give every value with its own meaning its own type, even when the bits are a
plain integer. An image address, a virtual address, a load bias, a DWARF
register number, a thread id, and a row in one table must not be
interchangeable. A function that takes two `u64`s with different meanings
invites a swap the compiler can't catch.

- Make newtypes zero-cost: a `Copy` tuple struct over one integer. This fits
  the data-oriented layout above.
- Keep the field private. Construct and unwrap in a few named places, such as
  the provider that reads the value.
- Put conversions between spaces on the type, such as
  `ImageAddress + LoadBias -> VirtualAddress`, and make them checked.
- Don't call `.get()` just to do arithmetic. Add the operation to the type.
- Prefer `Option<Id>` to a sentinel such as `0` or `u32::MAX`.
