# uscope for VS Code

Debug native Linux programs written in C, C++, Rust, Go, and Zig with
[uscope](../../docs/dap.md).

This extension only declares the `uscope` debugger type; VS Code runs
`uscope dap` from `PATH`. See [docs/dap.md](../../docs/dap.md) for installation,
configuration, and what the adapter supports.

`test/` holds the user acceptance test that `just uat-vscode` runs: it drives
the adapter from a real VS Code window and records each session's traffic.
