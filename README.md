# uscope

`uscope` is an early Linux x86-64 native debugger written in Rust.

## Development

Enter the pinned development environment and run the checks:

```sh
nix develop
just check
```

The Rust tests do not invoke a C compiler. Build the fixture binaries explicitly
before running them directly:

```sh
just build-test-programs
cargo test
```

Start the debugger with:

```sh
cargo run -- build/test-programs/basic
```

The initial REPL supports `break <function|runtime-address>`, `run`, `continue`,
`x <runtime-address>`, `address <symbol>`, and `quit`.

Run commands without the terminal UI by passing a command file or streaming them
on standard input:

```sh
cargo run -- --batch -x tests/fixtures/basic.uscope build/test-programs/basic
printf 'break main\nrun\nquit\n' | cargo run -- --batch build/test-programs/basic
```

`-x/--command` may be repeated. `-e/--eval` executes an individual command and
may also be repeated. Without `--batch`, both forms run before the REPL opens.
