# uscope 🔬

[![status-badge](https://ci.uscope.dev/api/badges/1/status.svg)](https://ci.uscope.dev/repos/1)

<img src="https://github.com/user-attachments/assets/bc4b539f-77c1-4dbd-95f2-24102c41ab5c" />

### Overview

uscope (pronounced "microscope") is a native code graphical debugger and introspection toolchain for Linux.

[See here](https://calabro.io/uscope) for background and motivation on the project.

Join the [Discord](https://discord.gg/bPWC6PZPhR) if you're interested in talking debuggers.

### Project Status and Roadmap

uscope is not far enough along to consider using as a daily-driver. It's a side project I'm working on for fun and because I need a better debugger for my own use.

In fact, it's currently undergoing a total rewrite of its user interface. I would not recommend even attempting to use it at this time - but stay tuned! See more information on the motivation for this change [here](https://calabro.io/uscope-update-2). If you want to try to use the old native Linux UI, you can clone [this tag](https://github.com/jcalabro/uscope/tree/old-ui), though it likely will not work for your real-world use case.

This is a birds-eye overview of the features I'd like implemented before I'd personally be able to completely ditch other "traditional" debuggers. In no particular order:

- Remote debugging
- Total rewrite of the UI to be web-based to enable easy remote development (even if the agent you're connecting to is `localhost`)
- Ensure that all table-stakes debugger operations are rock-solid and fast
  - Debug symbol parsing
  - Subordinate process Control flow (i.e. stepping)
  - Basic variable value rendering
  - Stack unwinding
  - etc.
- Support for visualization of common data types in several languages (preliminary C, Zig, Odin, and C3 support is already underway)
  - Adding support for Go even though it's a very complicated language since that's what I use for work
  - Also planning on supporting at C++, Rust, Crystal, and Jai
  - In general, we will design a system that handles transforming data in to user-friendly visualization that is flexible, extensible, and not tied to any one language
- Support for multi-threaded programs (preliminary support underway)
- Debug tests by clicking on them, at least for programs with built-in testing solutions like Zig, Go, etc.
- Run to cursor
- User-friendly source code navigation (i.e. go to definition, find all references, etc.)
- Better config file management
  - I don't want to have to manually edit config files; I want to have the debugger configure them for me via the GUI

Other long-term features that will be implemented are:

- Build as a library so other people can build other interesting things as well
  - The remote debugger agent will be the first consumer of that library (in the same way [Ghostty](https://github.com/mitchellh/ghostty) is the first consumer of libghostty)
- Many more types of workload-specific data visualizations
  - For example, I when I work on chess engines, it would be amazing to have a debugger that natively understands my position encoding and automatically visually renders interactive chess boards
- Conditional breakpoints
- Data/address breakpoints (i.e. break when an address is accessed or a variable mutated)
- Trace points (observe variable values over time in a low-overhead manner)
- Load and view core dumps
- Assembly viewer
- Ability to track and visualize system calls (similar to [strace](https://man7.org/linux/man-pages/man1/strace.1.html))
- Various `/proc` views (there's lots of interesting information in there)
- macOS and Windows support
- What is important to _you_? Let me know!

Similarly, the following features are non-goals of the project:

- Supporting non-native languages (i.e. Java, Python, etc.)

### Building and Running

Do not attempt to build and run uscope at this time. It is under heavy development and is in the middle of a total UI rewrite.

### FAQ

##### 1. When will this project be mature enough to use for my day to day work?

Probably a long time (could easily be a year, or several). I have a day job, and this is a passion project I work on in my spare time. Check back often for updates!

##### 2. How can I help out?

The absolute best thing you can do is reach out and talk debuggers so I know that there is interest in the the project. We have a [Discord](https://discord.gg/bPWC6PZPhR), and you can find my email on my personal site. I love hearing from you!

You could also consider [sponsoring my work](https://github.com/sponsors/jcalabro). This is a very strong signal to me that I'm focused on things that matter.

Additionally, please consider donating to the [Zig Software Foundation](https://ziglang.org/zsf/)!

##### 3. Will you provide pre-built binaries?

Once the project is further along, yes, but not now.

##### 4. Why are you building a library for debugging, not just a new debugger? And why not just use DAP?

There are a wide variety of use-cases for an introspection library outside of traditional debuggers (i.e. reverse engineering tools, novel forms of debuggers, etc.). By making this system reusable and nicely packaged, it encourages the entire ecosystem of debugging tools to improve, not just this one project. That being said, we are focusing intently on the traditional debugger first, and then once the core of the system is solid, we will make it more intentionally accessible to other consumers.

Regarding [DAP](https://microsoft.github.io/debug-adapter-protocol), This toolchain intends to be lower-level and broader in scope than something like DAP would enable. I do not think DAP is very good, but lots of editors out there already speak it, so we're partially stuck with it. However, by creating an introspection library, we easily create a separate DAP-compatible executable completely isolated from the native GUI we're building so that way neither is bloated by the other.

In short, building as a library allows us all to build many novel, simple, and focused introspection tools.
