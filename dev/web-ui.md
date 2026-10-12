# Web UI

`uscope web` serves the React application in `web/` and connects every tab to
one debugger session over WebSocket. The server is a `DebuggerHandle` client,
like the CLI and DAP server.

## Protocol and state

The server sends tab state and notices. A tab sends requests that include the
`StopId` of the state it read. Cache read answers by stop and request. Forget
them after writes, setting changes, and session changes.

The address bar contains the tab's complete view: stop, thread or task, frame,
source, panel, and memory location. A shared link must open the same view.

Formatting shared with other clients belongs in `src/present`.

`web/src/protocol.gen.ts` is generated from `src/web/protocol.rs` with ts-rs.
A test detects stale output. Regenerate it with:

```sh
USCOPE_UPDATE_PROTOCOL=1 just test protocol
```

## Building and testing

Debug builds read the page from `build/web` at runtime. Release builds embed
it.

```sh
just web
just web-test
just web-e2e
just web-e2e --project=chromium -g NAME
```

`just web-test` runs TypeScript, Biome, and Vitest. End-to-end tests run real
servers in Chromium and Firefox.

Replay tests use recorded server transcripts. Update them with `just
web-transcripts` when the protocol change is intentional.

To inspect the page, run:

```sh
just web-probe PROGRAM STEPS...
```

The probe saves a screenshot after each step and prints browser-console
messages. `web/e2e/probe.ts` lists the available steps. `just web-dev` starts
the live-reload server.

Use binaries in `web/node_modules/.bin` or the Nix shell's `biome`. Do not use
`npx`, which can download a different package when a command is missing.
