# Browser

`uscope web` serves one debugging or profiling session to one or more browser
tabs.

## Start a session

```sh
uscope web ./program -- ARG...
uscope web --attach PID
uscope web --core core.1234
uscope web profile.usprof
```

With no target, the page lets you choose what to open. The server listens on
`127.0.0.1:7341` by default and chooses another free port when that port is in
use.

A launched program waits for the first continue. Pass `--run` to start it at
once. An attached process waits at its attach stop unless `--resume` is used.

## Move around

The workspace can show source, disassembly, memory, values, registers,
breakpoints, threads, tasks, signals, the console, and drawings. The command
palette lists available actions and keyboard shortcuts.

Each tab selects its own thread or task, frame, source location, and panel. All
tabs control the same session and receive the same stops.

The address bar records the tab's complete view. Copying the URL gives another
tab the same selected stop, frame, source, panel, and memory location when that
state is still available.

## Draw values

Views can attach visualizers to values. The Drawings panel can show a buffer as
an image, samples as a chart, or a mesh as an interactive model. See
[Writing views](writing-views.md) and [Visualizers](visualizers.md).

Renderers run in a sandbox. They receive only the data selected by a view and
cannot read arbitrary process memory.

## Open profiles

Pass one profile to explore it or several profiles to compare them:

```sh
uscope web current.usprof
uscope web current.usprof baseline.usprof
```

The profile pages include call trees, functions, flame views, timelines,
source and disassembly costs, blocked-time lanes, task stacks, and
instrumentation zones when the recording contains them.

## Serve through a tunnel or proxy

The default loopback listener is available only on the same machine. Use an
SSH tunnel to reach it remotely.

`--listen ADDRESS` exposes the session directly. Anyone who can reach the
address and has a session link can control the debugger, so do not expose it to
an untrusted network.

For a reverse proxy, `--public-url URL` gives uscope the external origin and
path. The proxy is responsible for authentication and access control.
