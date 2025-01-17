# Inspect

[![Tests](https://github.com/jcalabro/inspect/actions/workflows/ci.yaml/badge.svg)](https://github.com/jcalabro/inspect/actions/workflows/ci.yaml)

<img src="https://github.com/user-attachments/assets/17eb2df9-35af-4f3e-a589-de519c4a9e35" />

### Overview

`inspect` is a native code debugger for Linux. It supports debugging C and Zig programs (with support for more languages to come).

There is substantial room for innovation in the space of debug tooling, and though we're currently early-days, the vision for this project is a fast, robust debugger that answers the question of "what is my program doing" as quickly and painlessly as possible for a variety of workloads.

All of the debugger-related functionality is written from the ground-up, including:

- Parsing ELF/DWARF files to obtain debug info
- Running a subordiante process and handling control flow (pausing, stepping, continue execution, etc.)
- Setting and handling breakpoints
- Viewing variable values in a user-friendly format
- Call stack unwinding
- etc.

[See here](https://www.calabro.io/dwarf#why-are-you-writing-this) for some further thoughts on the motivation behind this project.

### Project Status

`inspect` is not far enough along to consider using as a daily-driver. It is a side project I'm working on for fun and because I want a good debugger for my own use. Consequently, the pace of updates may be slow and inconsistent, but I do intend to keep working on it when I have time.

I'm always interested in talking debuggers and other areas of tech. Please feel free to reach out via email to jim at [my domain](https://calabro.io). Other forms of contact info can also be found on my site.

### High-level Roadmap

This is a birds-eye overview of the features I'd like implemented before I'd personally be able to completely ditch other "traditional" debuggers. In no particular order:

- Support for visualization of common data types in more languages
  - At least C++, Go, Rust, Odin, and Jai (C and Zig are already supported)
  - I personally use C++ and Go a lot at my day job, so those ones will probably come first even though they're very complicated languages
  - In general, we just need a plugin system that understands natvis, LLDB pretty-printers, or something else of our own design
- Support for multi-threaded programs
- User-friendly source code navigation (i.e. go to definition, find all references, etc.)
- Run to cursor
- Debug tests by clicking on them

Other long-term features that will be implemented are:

- Build as a library so other people can build other interesting things on top of this
  - The GUI will be the first consumer of that library, sort of in the same way [Ghostty](https://github.com/mitchellh/ghostty) is the first consumer of libghostty
- Many more types of domain-specific data visualizations
  - For example, I work on chess engines for my day job, and it would be amazing to have a debugger that natively understands my position encoding and automatically renders interactive chess boards
- Remote debugging
- Conditional breakpoints
- Data breakpoints (i.e. break when an address is accessed or a variable mutated)
- Trace points (observe variable values over time without actually pausing the subordinate program)
- Load and view core dumps
- Assembly viewer
- Ability to track and visualize system calls (similar to [strace](https://man7.org/linux/man-pages/man1/strace.1.html))
- Various `/proc` views (there's lots of interesting information in there)
- Complete UI/UX revamp (Dear ImGUI has been decent, but it has its limitations)
- macOS and Windows support
- i18n

Similarly, the following features are non-goals of the project:

- Supporting non-native languages (i.e. Java, Python, etc.)
- Record/replay like [rr](https://rr-project.org/)
  - `rr` is awesome, it's just a very different model

### Building and Running

We do not provide pre-built binaries or package manager distributions (yet).

To build from source, clone the repo and run `zig build`. [Zig version 0.13.0](https://ziglang.org/download/) is required.

```bash
git clone git@github.com:jcalabro/inspect.git
cd inspect
zig build -Doptimize=ReleaseSafe -Drelease
```

You'll probably want to create a global config file at `$XDG_CONFIG_HOME/inspect/config.ini` like this (though we'll create an empty config for you if one does not already exist):

```ini
[log]
level=debug
regions=all
```

And a you'll need to create a local, project-specific config file at `$(pwd)/.inspect/config.ini`, whose only required field is `target.path`:

```ini
[target]
path=./assets/zigprint/out # required: the path to the binary to debug
# args=...
# stop_on_entry=true
# watch_expressions=...

[sources]
# for convenience, opens this file upon launch and sets breakpoints on lines 33 and 96
open_files=assets/zigprint/main.zig:33:96

# to open multiple files on launch, you could do something like:
# open_files=first.c:1:2, second.c:3:4
```

Note that the plan in the future is to allow all configuration options to be configurable from within the debugger itself rather than requiring the user to edit text files. The debugger will manage these files automatically.

Then, to create a development build, you can do any of:

```bash
# create and run a debug binary
zig build run

# create and run a debug binary with the race detector enabled
zig build run -Drace

# run all tests
zig build test

# run a test by name
zig build test -Dfilter='compile unit header parse errors'

# run a subset of tests based on a prefix match (i.e. this runs all simulator tests)
zig build test -Dfilter=sim:
```

The `Primary` view is open by defaults, which includes views in to source code, program stdout/stderr, variables, registers, etc. To launch the subordinate, press `r`, and press `k` to kill a running subordinate. Click lines of source code to add/remove breakpoints. When you're stopped at a breakpoint, you can:

- `c`: continue execution
- `k`: kill the subprocess
- `w`: step out
- `a`: single step (one assembly instruction)
- `s`: step in
- `d`: step next

To quickly navigate between multiple open source files, press `ctrl+j` to move one source file to the left (according to the order of tabs), and press `ctrl+;` to move one to the right. Press `ctrl+d` to close the open source file. Press `ctrl+q` in the primary view to quickly exit the debugger.

Additionally, we've taken a bit of inspiration from the [Helix editor](https://helix-editor.com/) for menu navigation. Press `space` to open the view picker, then choose a view to open. Press `ctrl+d`, `ctrl+c`, or `ctrl+q` at any time in any sub-view to go back to the main view.

The program outputs a user-friendly log by default to:

```bash
tail -d /tmp/inspect.log
```

This repo comes pre-packaged with a bunch of small, simple source programs in various languages in the `assets/` directory. To build them all, ensure you have all the toolchains you could possibly neeed installed and:

```bash
cd assets
./build.sh
```

Here's what versions I run on my development and CI machines for reference, though getting a healthy variety of compilers and versions is good because it reveals subtle, real-world issues:

- gcc: 14.2.1 20240910
- clang: 18.1.8
- zig: 0.13.0
- rust: 1.83.0
- go: 1.23.4
- odin: dev-2024-12-nightly
- jai: 0.2.002

### FAQ

##### 1. How can I help out?

The absolute best thing you can do is reach out and talk debuggers with me so I know that there is interest in the the project (I love hearing from you).

Additionally, adding features, fixing bugs, and creating tests that move us further along the path towards being able to use this for day-to-day work is also apprecaited! Feel free to reach out to me if you're thinking about doing some large amount of work and I can give you an overview of the project structure so you don't waste your time.

##### 2. When will this project be mature enough to use for my day to day work?

Probably a long time (a year or more at least). I have a day job, and this is a passion project I work on in my spare time. Check back often for updates!

##### 3. Will you provide pre-built binaries?

Once the debugger is further along, yes, but not now. It's not generally useful to people yet, so there's no point in providing binaries at the moment.

##### 4. Why are you intending to build a library for debugging, not just a new debugger? Why not just use DAP?

There are a wide variety of use-cases for an introspection library outside of traditional debuggers (i.e. reverse engineering tools, novel forms of debuggers, etc.). For instance, perhaps a user could create a version of `dwarfdump` that's much more visual where you can click around the DIE tree and explore.

Additionally, I do not think [DAP](https://microsoft.github.io/debug-adapter-protocol//) is very good, but lots of editors out there already speak it. By creating a library, we easily create a separate DAP-mode static executable as opposed to having to also lug around a giant GUI that never gets used.

In short, it allows us all to build simple, focused, and novel introspection tools.
