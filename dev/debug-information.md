# Debug information

Debug-information providers translate format- and language-specific data into
the neutral model used by the debugger. ELF, DWARF, Go tables, native register
layouts, and ABI rules stay behind this boundary.

## Images and bindings

A load produces one validated image of immutable tables under `src/image`.
`ModuleImage` and providers read those tables through checked views. Validate
all offsets, indexes, counts, and cross-table references before publishing the
image.

A module binding holds the program path, identifier, mapping, and separate
debug-file path. Do not put those values in the image. Keeping them separate
allows one image to serve several mappings and makes the cache safe to share.

The cache in `src/cache.rs` keys an entry by the bytes that affect its result
and by revisions of the loader sources. Tests use only a cache path they create
or one explicitly named with `USCOPE_CACHE_DIR`.

## Providers

Normalize data at the provider boundary. The evaluator and clients must not
interpret DWARF expressions, ELF segments, Go pclntab records, or native
register numbers.

Data layout belongs to providers. They resolve field offsets, base classes,
variants, calling conventions, and register locations. If the available debug
information cannot support an answer, return an explicit absence or partial
result.

## Unwinding

The generic unwind loop iterates caller contexts. gimli owns DWARF CFI
interpretation. Runtime-specific stack transitions report enough information
to continue or return a typed reason why the stack ended.

Do not fill unknown registers with zero or assume a frame rule. A convincing
wrong stack is worse than a short stack with a reason.

## Sources

Resolve source paths while loading metadata. Read source contents lazily,
outside the ptrace controller thread. Path remapping changes source lookup, not
the identity or bytes of a module image.
