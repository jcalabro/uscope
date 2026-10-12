# Troubleshooting

## Source code is missing

Debug information records the source path used during the build. If the source
now lives elsewhere, map its old prefix to the current one:

```sh
uscope --source-map /build/worker /home/me/worker ./program
```

Rules are tried in command-line order. A source map changes where source text
is read; it does not change symbols or machine addresses.

## Types, lines, or variables are missing

Confirm that the executable contains debug information or has a matching
separate debug file. Search an additional directory with:

```sh
uscope --debug-directory /srv/debug ./program
```

Use `--debuginfod` when `DEBUGINFOD_URLS` lists trusted servers that carry the
module's build ID.

Optimization can remove variables or leave them available for only part of a
function. uscope reports `optimized out`, an unavailable location, or a partial
value when that is what the debug information says.

## A copied core dump cannot find its modules

Point `--sysroot` at a copy of the original machine's root filesystem. Add
`--module-path` for loose module directories. uscope matches files by recorded
identity and build ID where available.

`--allow-module-mismatch` bypasses that check. Use it only when you accept that
symbols, source lines, and memory layout may be wrong.

## Attach is denied

Linux controls tracing through user IDs, ptrace policy, capabilities, and
container boundaries. Check that the debugger user may inspect the target and
that both processes are in compatible PID and mount namespaces.

When a process is in another mount namespace, uscope also uses
`/proc/PID/root` to find the files that process mapped.

## A stack ends early

Stack unwinding depends on frame pointers, DWARF unwind information, saved
registers, and readable memory. uscope shows why a stack ended when it can
identify the reason. Rebuild with debug information and frame pointers when a
complete stack matters.

## Views do not load

Check a view file without starting a process:

```sh
uscope views check ./program --views custom.views
```

Explain why a type matched or failed to match:

```sh
uscope views explain ./program 'namespace::Type' --views custom.views
```

See [Views](../views.md) for discovery and precedence.

## The image cache appears stale

The cache key includes the input bytes and loader revision, so a changed binary
or debugger should not reuse an incompatible image. Use `--no-cache` to load
everything again while diagnosing a problem.

The default cache is under `$XDG_CACHE_HOME/uscope/images` or
`~/.cache/uscope/images`. `USCOPE_CACHE_DIR` selects another directory.

## Report a reproducible failure

Development builds record debugger requests, native control operations, wait
statuses, and events. A normal run keeps recent logs under
`target/flight-recorder/runs` in a repository build. Include the shortest
reproduction, uscope version, target compiler and version, and relevant log
when filing a bug. Check the log for source paths or program data before
sharing it.
