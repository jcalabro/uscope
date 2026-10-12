# Configuration

uscope reads TOML settings from up to three files:

1. `$XDG_CONFIG_HOME/uscope/config.toml`, or `~/.config/uscope/config.toml`
2. `.uscope/config.toml` in the project root
3. `.uscope/config.local.toml` in the project root

The local file is intended for settings that should not be committed. Add it
to the project's `.gitignore`.

The project root is the nearest parent containing `.uscope`, then the nearest
parent containing `.git`, then the current directory.

## Inspect configuration

```sh
uscope config path
uscope config show
uscope config check
uscope config init
```

`path` prints the project root and files a session would read. `show` prints
effective values and their sources. `check` validates files without starting a
debugger. `init` writes a commented user file and refuses to overwrite one.

Unknown keys, wrong types, and out-of-range values stop startup with the file
and location of the error.

## Precedence

A command-line flag wins over environment settings, followed by the local
project file, project file, user file, and built-in default. Tables merge by
key; lists replace earlier lists.

Startup command lists run in order instead: user, project, local, selected
launch configuration, `-c` files, then `-e` commands.

## Main tables

| Table | Controls |
|---|---|
| `[ui]` | Color, theme, Unicode, links, paths, pager, editor, prompt, quit confirmation |
| `[theme]` | Styles for names, values, changes, keywords, and other output roles |
| `[source]` | Context lines, highlighting, and tab width |
| `[stop]` | Sections printed at stops and their sizes |
| `[print]` | Compact or pretty output, radix, width, depth, and element limits |
| `[disassembly]` | Intel or AT&T syntax and instruction bytes |
| `[step]` | Whether source steps enter supported runtime code |
| `[breakpoints]` | Saved-breakpoint behavior |
| `[debug-info]` | Separate debug-file directories and debuginfod |
| `[history]` | REPL history size |
| `[signals]` | Per-signal stop, print, and delivery policy |
| `[aliases]` | Additional command names |
| `[startup]` | Commands run at startup |
| `[projects]` | Project trust policy in the user file |
| `[[source-map]]` | Source-prefix remapping |
| `[[launch]]` | Named launch, attach, and core-dump sessions |

Run `uscope config init` to produce a file containing every setting, its type,
and its default. That generated file is the exact reference for the installed
version.

## Launch configurations

```toml
[[launch]]
name = "server"
program = "build/server"
args = ["--port", "8080"]
env = { RUST_LOG = "debug" }
cwd = "."
startup = ["break handle_request", "run"]
```

Paths are relative to the project root. Start it with `uscope --launch server`.
When a project has exactly one launch configuration, `uscope` with no target
uses it. Arguments after `--` replace its arguments; command-line environment
and working-directory options override its values.

A launch entry may use `attach` or `core` instead of `program` to describe
another kind of target.

## Trust project actions

Project startup commands, aliases, and launch configurations can execute
actions. Under the default `ask` policy, an interactive session asks before
using them and remembers approval until the files change. A non-interactive
session must use `--trust-project` or previously recorded trust.

```sh
uscope config trust
uscope config untrust
uscope config trusted
```

The user setting `[projects] trust` can be `ask`, `always`, or `never`.
