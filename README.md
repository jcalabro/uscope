# Microscope 🔬

[![status-badge](https://nuc01.tail0223.ts.net/api/badges/1/status.svg)](https://nuc01.tail0223.ts.net/repos/1)

<img src="https://github.com/user-attachments/assets/8ad10ca9-42d1-4afe-8397-74b8a92a69f5" />

### Overview

Microscope is a native code introspection toolchain and graphical debugger for Linux.

[See here](https://calabro.io/microscope) for background and motivation on the project.

### Project Status and Roadmap

Microscope is not far enough along to consider using as a daily-driver. It is a side project I'm working on for fun and because I want a great debugger for my own use.

This is a birds-eye overview of the features I'd like implemented before I'd personally be able to completely ditch other "traditional" debuggers. In no particular order:

- Support for visualization of common data types in several languages
  - At least C++, Go, Rust, Odin, and Jai (C and Zig are already supported)
  - I personally use C++ and Go a lot at my day job, so those ones will probably come first even though they're very complicated languages
  - In general, we will design a system that handles transforming data in to user-friendly visualization that is flexible, extensible, and not tied to any one language
- Support for multi-threaded programs
- User-friendly source code navigation (i.e. go to definition, find all references, etc.)
- Run to cursor
- Debug tests by clicking on them

Other long-term features that will be implemented are:

- Build as a library so other people can build other interesting things as well
  - The GUI debugger will be the first consumer of that library (in the same way [Ghostty](https://github.com/mitchellh/ghostty) is the first consumer of libghostty)
- Many more types of domain-specific data visualizations
  - For example, I work on chess engines for my day job, and it would be amazing to have a debugger that natively understands my position encoding and automatically visually renders interactive chess boards
- Remote debugging
- Conditional breakpoints
- Data/address breakpoints (i.e. break when an address is accessed or a variable mutated)
- Trace points (observe variable values over time without actually pausing the subordinate program)
- Load and view core dumps
- Assembly viewer
- Ability to track and visualize system calls (similar to [strace](https://man7.org/linux/man-pages/man1/strace.1.html))
- Various `/proc` views (there's lots of interesting information in there)
- Complete UI/UX revamp (Dear ImGUI has been decent, but it has its limitations - we'll probably just write our own)
- macOS and Windows support

Similarly, the following features are non-goals of the project:

- Supporting non-native languages (i.e. Java, Python, etc.)

### Building and Running

We do not provide pre-built binaries or package manager distributions (yet).

To build from source, clone the repo and run `zig build`. [Zig version 0.13.0](https://ziglang.org/download/) is required.

```bash
git clone git@github.com:jcalabro/microscope.git
cd microscope
zig build -Doptimize=ReleaseSafe -Drelease
```

You'll probably want to create a global config file at `$XDG_CONFIG_HOME/microscope/config.ini` like this (though we'll create an empty config for you if one does not already exist):

```ini
[log]
level=debug
regions=all
```

And a you'll need to create a local, project-specific config file at `$(pwd)/.microscope/config.ini`, whose only required field is `target.path`:

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
tail -d /tmp/microscope.log
```

This repo comes pre-packaged with a bunch of small, simple source programs in various languages in the `assets/` directory. To build them all, ensure you have all the toolchains you could possibly neeed installed and:

```bash
cd assets
./build.sh
```

The compiler versions used to build all the asset programs in CI are in the [Dockerfile](https://github.com/jcalabro/microscope/blob/main/Dockerfile). You run the tests without docker as demonstrated above, or with docker using:

```bash
docker build -t microscope .
docker run --rm -it -v $(pwd):/microscope microscope
cd /microscope/assets
./build.sh
cd ..
zig build test -Drace
```

### FAQ

##### 1. How can I help out?

The absolute best thing you can do is reach out and talk debuggers with me so I know that there is interest in the the project (I love hearing from you).

Additionally, adding features, fixing bugs, and creating tests that move us further along the path towards being able to use this for day-to-day work is also apprecaited! Feel free to reach out to me if you're thinking about doing some large amount of work and I can give you an overview of the project structure so you don't waste your time.

##### 2. When will this project be mature enough to use for my day to day work?

Probably a long time (a year or more at least). I have a day job, and this is a passion project I work on in my spare time. Check back often for updates!

##### 3. Will you provide pre-built binaries?

Once the project is further along, yes, but not now. It's not generally useful to people yet, so there's no point in providing binaries at the moment.

##### 4. Why are you intending to build a library for debugging, not just a new debugger? Why not just use DAP?

There are a wide variety of use-cases for an introspection library outside of traditional debuggers (i.e. reverse engineering tools, novel forms of debuggers, etc.). This toolchain intends to be lower-level and broader in scope than something like DAP would enable.

I do not think [DAP](https://microsoft.github.io/debug-adapter-protocol//) is very good, but lots of editors out there already speak it. By creating an introspection toolchain, we easily create a separate DAP-mode executable in addition to the native GUI we're building so that way neither is bloated by the other.

In short, it allows us all to build simple, focused, and novel introspection tools better than DAP would allow.
