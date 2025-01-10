# Panacea Debugger

[![Tests](https://github.com/jcalabro/panacea/actions/workflows/ci.yaml/badge.svg)](https://github.com/jcalabro/panacea/actions/workflows/ci.yaml)

<img src="https://github.com/user-attachments/assets/c1369788-bc58-4323-a063-c74f403cc39c" />

### Overview

`panacea` is a native code debugger for Linux. It supports debugging C and Zig programs (with support for more languages to come in the future).

There is substantial room for innovation in the space of debug tooling, and though we're currently early-days, I envision a future with a fast, robust debugger that answers the question of "what is my program doing" as quickly and painlessly as possible.

100% of the debugger-related functionality is written from the ground-up, including:

- Parsing ELF/DWARF files to get debug info
- Running a subordiante process and handling control flow (stepping, continue execution, etc.)
- Setting and handling breakpoints
- Viewing variable values in a human-friendly format
- Call stack unwinding
- etc.

It's critical to build these key areas of the system from scratch so we can own our long-term success, innovate based on solid footing, and remain unencumbered by large dependencies or licenses that enforce obligations.

[See here](https://www.calabro.io/dwarf#why-are-you-writing-this) for some more thoughts on the motivation behind this project.

### Project Status

`panacea` is not far enough along to consider using as a daily-driver. That being said, you may find it useful at present if you're writing simple, single-threaded C or Zig programs.

It is a side project I'm working on for fun and because I want a good debugger for my own use. Consequently, the pace of updates may be slow and inconsistent, but I do intend to keep working on it when I have the time.

I'm always interested in talking debuggers and other areas of tech. Please feel free to reach out via email to jim at [my domain](https://calabro.io). Other forms of contact info can also be found on my site.

### High-level Roadmap

This is a birds-eye overview of the features I'd like implemented before I'd personally be able to completely ditch all other "traditional" debuggers. In no particular order:

- Support for visualization of common data types in more languages
  - At least C++, Go, Rust, Odin, and Jai (C and Zig are already supported)
  - I personally use C++ and Go a lot at my day job, so those ones will probably come first even though they're very complicated languages
  - In general, I think we just need a plugin system similar to natvis or LLDB pretty-printers, but better
- Support for multi-threaded programs
- User-friendly source code navigation (i.e. go to definition, find all references, etc.)
- Run to cursor
- Debug tests by clicking on them

Other long-term features that should be implemented are:

- Build as a library so other people can build other interesting things on top of this
  - The GUI will be the first consumer of that library
- Many more types of domain-specific data visualizations
  - For example, I work on chess engines for my day job, and it would be amazing to have a debugger that natively understands my position encoding and automatically renders interactive chess boards
- Remote debugging over ssh
- Conditional breakpoints
- Data breakpoints (i.e. break when an address is accessed or a variable mutated)
- Trace points (observe variable values over time without actually pausing the subordinate program)
- Load core dumps
- Assembly viewer
- Ability to track and visualize system calls (similar to [strace](https://man7.org/linux/man-pages/man1/strace.1.html))
- Various `/proc` views (there's lots of interesting information in there)
- Complete UI/UX revamp (Dear ImGUI has been decent, but it has its limitations)
- macOS and Windows support
- i18n

Similarly, the following features are non-goals of the project:

- Supporting non-native languages (i.e. Java), JIT'ed languages (i.e. JavaScript), or interpreted langauges (i.e. Python)
- Terminal UI
- Record/rewind (reverse debugging)

### Building and Running

We do not provide pre-built binaries or package manager distributions (yet).

To build from source, clone the repo and run `zig build`. [Zig version 0.13.0](https://ziglang.org/download/) is required.

```bash
git clone git@github.com:jcalabro/panacea.git
cd panacea
zig build -Doptimize=ReleaseSafe -Drelease
```

You'll probably want to create a global config file at `$XDG_CONFIG_HOME/panacea/config.ini` like this (though we'll create an empty config for you if one does not already exist):

```ini
[log]
level=debug
regions=all
```

And a you'll need to create a local, project-specific config file at `$(pwd)/.panacea/config.ini`, whose only required field is `target.path`:

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

Note that the plan in the future is to allow 100% of configuration options to be accssible from within the debugger itself rather than requiring the user to edit text files. The debugger will manage these files automatically.

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
tail -d /tmp/panacea.log
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
