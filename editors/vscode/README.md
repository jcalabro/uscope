# uscope for VS Code

Debug native Linux programs written in C, C++, Rust, Go, and Zig with
uscope. The extension contributes the `uscope` debugger type and runs
`uscope dap`; [docs/dap.md](../../docs/dap.md#vs-code) covers installing
it, its settings and configurations, and what the adapter supports.

`extension.js` is the whole extension, plain JavaScript that VS Code loads
directly. `test/` holds the acceptance test that `just uat-vscode` runs: it
drives the adapter from a real VS Code window and records each session's
traffic.
