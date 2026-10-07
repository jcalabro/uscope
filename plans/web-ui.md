# Web UI

`uscope web` serves a browser debugger: a React single-page app and a
WebSocket, both from the uscope binary. It is dense, keyboard driven, and
built around one idea from Pernosco: every pane is a view of one *focus*
(stop, thread, frame, place in the code), and the URL is that focus. A
link pasted to a coworker opens the same live session at the same place.

## 1. Goals and non-goals

- **One live session, many viewers.** Every browser tab connected to a
  `uscope web` process sees the same debugger. A coworker who opens your
  link joins the session, sees what you see, and (with control access) can
  drive it. Each tab keeps its own focus.
- **Nothing is ever silently wrong.** A tab shows data only for the stop it
  was read at. While the program runs, the last stop's values stay on
  screen dimmed and labelled `stop #N`; a link to a stop that has passed
  says so instead of showing newer values under the old address.
- **A pleasure to use.** Stepping never waits on a pane. Tree expansion,
  scroll position, and selection survive every stop. Values that changed
  since the last stop are highlighted. No modal dialogs.
- **Fast, robust iteration** for the author and for agents: hot reload,
  type-checked protocol, tests that run in seconds, and a screenshot recipe
  an agent can read.

Out of scope for now: several debuggers in one server (one `uscope web`
process debugs one program; the waiter thread reaps every child, so two
debuggers cannot share a process), following forks, recording/replay, a
notebook, and exposure beyond a trusted network (see §6).

## 2. Architecture

```
browser tab ──WebSocket──┐
browser tab ──WebSocket──┤  src/web (binary crate, beside src/dap and src/cli)
                         │    server: axum, static assets, auth, one task per socket
                         │    session: one Debugger, program I/O, shared notices
                         │    connection: per-tab focus, per-stop handle table
                         └──> DebuggerHandle (cloned per connection)
                                └──> controller thread (unchanged)
```

- **A custom protocol on `DebuggerHandle`, not DAP.** DAP allows one client
  per session and its `Session` owns and ends its debugger; sharing would
  mean refactoring it into something DAP does not describe. The handle is
  already built for this: it clones, broadcasts events, publishes revisioned
  snapshots, and the controller arbitrates racing clients by `StopId`
  (`tests/debugger/execution.rs` shows two handles racing a continue, one
  winning). A custom protocol also keeps structure DAP flattens to strings:
  disassembly tokens, typed values, stop reasons.
- **Never the shared selection.** The controller keeps one selected thread
  and frame per inferior, which CLI commands and DAP's console change. Web
  connections address every request with an explicit
  `StopContext { stop, thread, frame }` through `DebuggerHandle::at`, so
  tabs never move each other's focus.
- **Share presentation, don't copy it.** The web layer formats with the
  library's summaries and `cli::format`/`cli::value`, as DAP does. Where a
  needed piece is an `impl Session` method in `src/dap` (frame naming,
  scopes, completions, disassembly padding), it moves to a function both
  clients call, in a change of its own with DAP's tests unchanged.
- **Program I/O belongs to the session.** A launched program's stdout and
  stderr go to pipes the session reads and broadcasts to every tab, kept in
  a bounded ring so a tab that joins late sees recent output; stdin is fed
  from the console's input mode.
- **Source contents are a new library request.** DAP never sends source,
  and a browser cannot open files. `DebuggerHandle::read_source(file)`
  reads a `SourceFileId` the loaded modules know, through the source map,
  off the controller thread, as `source_context` does. Nothing else on disk
  is reachable.
- **Assets are embedded but Node stays out of `cargo build`.** `just web`
  builds the app into `build/web` (ignored). A small `build.rs` embeds that
  directory with `include_bytes!`, or an empty table when it is missing, in
  which case the page says to run `just web`. `cargo build` and the Rust
  test suite never need Node.

## 3. Protocol

One WebSocket per tab at `/api/ws`. JSON text frames.

- Client → server: `{"id": 7, "method": "backtrace", "params": {...}}`.
- Server → client: `{"id": 7, "result": ...}` or
  `{"id": 7, "error": {"kind": "staleStop", "message": "..."}}`, and
  notifications `{"event": "...", ...}`.
- Error kinds are a closed set the UI handles (`staleStop`, `notStopped`,
  `forbidden`, `invalid`, `unsupported`, `failed`), each with the
  debugger's message.

**State is pushed, data is pulled.**

- On connect the server sends `hello` (protocol version, session id,
  program, role, other participants) and a full `state`.
- `state` is the snapshot DTO: revision, inferior state, stop id and
  reason, threads with names and states, breakpoints, watchpoints. It is
  small, so it is sent whole at each new revision, coalesced so a burst of
  revisions sends only the newest. A lagged event receiver means sending a
  fresh `state`, the recovery DAP already proves.
- Ordered notifications carry what a snapshot cannot: `output`
  (stream, bytes), `log` (logpoint message), `signal`, `conditionFailed`,
  `moduleLoaded`/`moduleUnloaded`, `presence` (who is connected and their
  focus), and `notice` (a request another participant made, such as
  "Alice continued").
- Everything else is a request that names its stop: `backtrace`, `scopes`,
  `children`, `evaluate`, `complete`, `console`, `disassemble`,
  `readMemory`, `registers`, `source`, `breakpointLines`, `modules`, and
  the mutations (`continue`, `step`, `pause`, `kill`, `restart`,
  `setBreakpoint`, `removeBreakpoint`, `setWatchpoint`, `setVariable`,
  `writeMemory`, `setSignalPolicy`).

**Values.** A value row is `{name, type, summary, changed?, children?,
evaluateName}`. `children` is a connection-local handle into a per-stop
table, cleared when the stop ends, so a request for a stale handle fails
with `staleStop` and never reads a newer stop. The UI keys expansion by
the row's path (`locals/list/head/next`), not the handle, and re-expands
the same paths at the next stop. That is how expansion survives stepping.
"Changed" is computed client-side by comparing summaries at the same
path with the previous stop.

**Types.** The Rust DTOs live in `src/web/protocol.rs` and derive serde and,
in tests only, `ts_rs::TS`. A test generates `web/src/protocol.gen.ts` and
fails when the checked-in file differs, so the two sides cannot drift.

## 4. URLs

The path names the debugger state; the query names how you are looking at
it. All of it is the focus, so copying the address bar shares the view.

```
/                                      session overview (redirects to the live stop)
/s/{session}/live?…                    follow the newest stop
/s/{session}/stop/{n}/t/{tid}/f/{k}?…  a pinned stop, thread, frame
  ?src=src/parse.c:205                 file shown and line selected (or :205-215)
  &asm=0x401136                        disassembly at an address
  &mem=0x7ffd1000                      memory view at an address
  &w=list->head,len                    watch expressions
  &x=locals/list/head                  expanded paths
```

- `session` is a random id minted when `uscope web` starts. A link from an
  earlier run says the session ended rather than opening a different one.
- `n` is the `StopId`, `tid` the kernel thread id, `k` the frame index. A
  pinned link to a stop that has passed shows a banner, keeps `src`, and
  offers "go to the current stop". It never shows the new stop's values
  under the old stop's address.
- **Copy link** (`y`) copies the pinned form. Opening `/live` follows
  whatever the session does next.

## 5. The interface

**Layout.** One screen, resizable splits, no modals:

```
┌ toolbar: ▶ ⏸ ⤼ ↓ ↑ ■  · program · ● stopped at stop #12 (breakpoint 2) · 2 viewers ┐
├──────────────┬──────────────────────────────────────────┬───────────────────┤
│ Threads      │ source / disassembly / memory (tabs)     │ Watch             │
│ Call stack   │   gutter: breakpoints, PC, frame arrows  │ Locals / Args     │
│ Breakpoints  │   inline values at the stopped line      │ Registers         │
│              ├──────────────────────────────────────────┤                   │
│              │ console · output                         │                   │
└──────────────┴──────────────────────────────────────────┴───────────────────┘
```

- **Source**: CodeMirror 6, read-only, with Lezer grammars for C, C++,
  Rust, and Go and a small tokenizer for Zig; gutter breakpoints (click,
  `F9`), the stopped line and each frame's line, inline values, hover to
  evaluate. Large files stay fast because CodeMirror renders only what is
  visible.
- **Disassembly** shares the source's focus, interleaves source lines, and
  marks branch targets; **memory** is a virtualized hex and ASCII view with
  types under the cursor. Address panes can be unlinked from the focus.
- **Variables and watch** are one tree component: virtualized, lazily
  paged children, views and `[raw]`, edit in place, changed values marked.
- **Console** runs the expression language and uscope's commands, with
  completion and history; run-control commands are the toolbar's.
- **Command palette** (`Ctrl+K`): commands, files, functions, threads,
  breakpoints, with previews. `Ctrl+P` opens a file, `Ctrl+G` a line.
- **Keys**: VS Code's debugger keys by default (`F5`, `F10`, `F11`,
  `Shift+F11`, `F9`), letter keys when no text box has focus (`c n s f`,
  `u d` for frames, `y` copy link, `?` help), `Alt+1…7` to focus a pane.
  Browsers reserve some keys, so every action also has a letter key.
- **State is unmistakable**: a running program dims every pane and labels
  it with the stop it shows; a stale request is quietly retried at the new
  stop, never shown.
- **Style**: plain CSS with custom properties and `light-dark()`, light and
  dark themes, tabular figures, ~20px rows, the system monospace font with
  a bundled OFL fallback (JetBrains Mono).

## 6. Security

The WebSocket can do anything a debugger can, including running code in
the debugged program. So:

- Bind `127.0.0.1` by default. Sharing beyond the machine is explicit
  (`--listen ADDRESS`), and the docs recommend an SSH tunnel.
- Every request needs a token minted per run. `uscope web` prints a join
  URL, `http://127.0.0.1:PORT/join#TOKEN`; the page exchanges the token for
  an HttpOnly, SameSite=Strict cookie and drops it from the address bar, so
  ordinary copied links carry no secret. **Share** produces a join link on
  purpose.
- Two roles, two tokens: **control** and **view**. A viewer's mutations
  fail with `forbidden`; the share dialog picks which link to copy.
- Check `Host` (blocks DNS rebinding) and require `Origin` to equal the
  served origin. The DAP server's rule that refuses any `Origin` stays as
  it is; this is a different endpoint with a different policy.
- No CORS, no other HTTP endpoints besides assets, `join`, and `ws`. Source
  reads are limited to files the debug information names.

## 7. Toolchain

All from the flake:

| Concern | Choice | Why |
|---|---|---|
| Runtime, packages | Node 24 LTS, pnpm 11, committed lockfile, `--frozen-lockfile` | in nixpkgs 26.05; pnpm blocks dependency build scripts by default |
| Build | Vite 8, TypeScript 7 (native compiler), React 19.3 | fast type checks and builds; React Compiler left off (it brings Babel back) |
| Routing | TanStack Router, validated search params | the URL is the focus, so typed search params matter |
| State | Zustand store fed by the socket; a small promise cache keyed by stop | snapshots arrive pushed; data is immutable per stop, so caching by stop is exact |
| Source | CodeMirror 6 + Lezer | read-only views of large files, gutters, decorations; MIT |
| Lists, trees | TanStack Virtual | |
| Lint, format | Biome | one fast binary, in nixpkgs |
| Server | axum 0.8 (`http1`, `tokio`, `ws` only) | about 50 crates, but no hand-written WebSocket handshake |
| Types | ts-rs, test-only | |
| Tests | Vitest browser mode, Playwright 1.59.1 (pinned to nixpkgs' browsers) | see §8 |

Not used: Bun (embeds LGPL JavaScriptCore), Monaco (heavy for read-only),
Shiki (highlights whole files), any CSS framework.

## 8. Testing

Each layer tests what only it can, in seconds:

1. **Rust server scenarios** (`tests/web/*.rs`, nextest): spawn
   `uscope web` on a fixture, connect real WebSocket clients, drive the
   public protocol. Cover auth (token, role, Host, Origin), multi-client
   races (two tabs continue at once: one wins, the other gets `staleStop`),
   stale handles, lag recovery, late join receiving recent output,
   disconnects while running, shutdown with no surviving inferior. The
   lifecycle rules in AGENTS.md apply unchanged.
2. **Recorded transcripts**: those scenarios write their traffic to
   `web/test/transcripts/*.jsonl`. Vitest replays them into the real store
   through an injected transport, so the UI's state logic is tested against
   what the real server says, with no hand-written mock server. A test fails
   when a transcript no longer matches `protocol.gen.ts`.
3. **Component tests**: Vitest browser mode on Chromium for the pieces with
   real behavior: the value tree (expansion survives a stop, changed
   marks), the source gutter, the router's URL ↔ focus round trip, key
   handling. Virtualized lists and CodeMirror need a layout engine, so
   these run in a real browser, not jsdom.
4. **End to end**: a handful of Playwright workflows against a real
   `uscope web`: launch, break, step, inspect; open a copied link in a
   second browser context and see the same stop; reload and keep the
   focus; a viewer cannot continue.
5. **Screenshots for agents**: `just web-shot` starts `uscope web` on a
   fixture, stops it somewhere interesting, and writes light and dark
   screenshots of the whole page to `target/web-shots`, so an agent can see
   what it changed. These are for looking at, not asserting.

Recipes: `just web` (build), `just web-dev` (Vite with hot reload, proxying
`/api` to `uscope web --dev`), `just web-test` (typecheck, Biome, Vitest),
`just web-e2e`. `just` runs `web-test` and the Rust web scenarios;
`web-e2e` joins `just all`.

## 9. Phases

Each phase ends usable, with its tests.

1. **Skeleton.** Flake tooling; `web/` with Vite, React, TypeScript, Biome,
   Vitest, Playwright; embedded assets; `uscope web PROGRAM [ARGS]`,
   `--attach PID`, `--core FILE`; token, roles, Host and Origin checks;
   `hello` and `state`; a page showing the session's state live in two
   tabs. Recipes and the screenshot tool.
2. **Run and see.** Run control, threads, call stack, `source` with the
   gutter, breakpoints (source, function, address; conditions, hit counts,
   logpoints), program output and input, the URL scheme and copy link,
   presence and notices, keys. The main layout.
3. **Inspect.** Scopes, the value tree with views and paging, watch
   expressions in the URL, changed values, hover and inline values, edit
   in place, the console with completion.
4. **Low level.** Disassembly, memory, registers, watchpoints, signal
   policy, modules.
5. **Polish.** Command palette, stop history breadcrumbs, stale-link
   banners, themes, keymap help, performance passes on large programs.

## 10. Open questions

- **Control by default?** Join links carry control or view access. Should
  `uscope web` print a control link (convenient) or a view link with
  control behind a second link (safer)? Recommendation: print control, and
  make Share default to view.
- **Router.** TanStack Router is the type-safe choice; a 100-line
  hand-written router over `URLSearchParams` is the minimal one. The scheme
  is small, so either works. Recommendation: TanStack Router.
- **One process, one program.** Launching or attaching from the page
  (process picker, recent programs) needs either restarting the session's
  debugger in place or a supervisor. Recommendation: phase 1 takes the
  program on the command line; a "launch another" page comes later.
