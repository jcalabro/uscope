# uscope for VS Code

Debug native Linux programs written in C, C++, Rust, Go, and Zig with
[uscope](../../docs/dap.md).

The extension declares the `uscope` debugger type and runs `uscope dap` from
`PATH`, or from the `uscope.path` setting. Pressing F5 without a launch.json
creates one, the Run and Debug view offers the folder's programs to launch,
and `${command:pickProcess}` picks a process to attach to. Hovers evaluate the
whole expression under the pointer, the stopped frame's variables show their
values inline, and *Toggle Hexadecimal Display* in the Variables and Watch
views flips the `uscope.hexadecimal` setting. `uscope.logFile` logs the
protocol. See [docs/dap.md](../../docs/dap.md) for installation,
configuration, and what the adapter supports.

`extension.js` is the whole extension; it is plain JavaScript that VS Code
loads directly. `test/` holds the user acceptance test that `just uat-vscode`
runs: it drives the adapter from a real VS Code window and records each
session's traffic.
