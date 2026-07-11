# uscope

`uscope` is a Linux x86-64 native debugger written in Rust.

## Development

Enter the pinned development environment and run the checks:

```sh
# either of these:
./scripts/dev.sh
just dev

# then, build and run the test binaries
just build-test-programs

# run the linter and all tests
just

# start the debugger
just run build/test-programs/basic
```

At a breakpoint, use `registers` or `regs` to print the stopped thread's general register set.
The interactive debugger is a plain terminal REPL, so output remains available in normal terminal scrollback. Use `--batch` with command files, `--eval`, or stdin when no interactive prompt is wanted.
