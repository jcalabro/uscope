# uscope for VS Code

Debug native Linux programs written in C, C++, Rust, Go, and Zig with
[uscope](../../docs/dap.md).

The extension declares the `uscope` debugger type and runs `uscope dap` from
`PATH`, or from the `uscope.path` setting. Pressing F5 without a launch.json
creates one, and `${command:pickProcess}` picks a process to attach to. See
[docs/dap.md](../../docs/dap.md) for installation, configuration, and what the
adapter supports.

`extension.js` is the whole extension; it is plain JavaScript that VS Code
loads directly. `test/` holds the user acceptance test that `just uat-vscode`
runs: it drives the adapter from a real VS Code window and records each
session's traffic.
