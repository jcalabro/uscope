//! The command table, argument parsing, and command handlers.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail};
use uscope::{
    BreakpointId, BreakpointSpec, ByteOrder, DebuggerEvent, Disassembly, DisassemblyQuery,
    DisassemblyRange, HitComparison, HitCondition, LineNumber, MAX_WINDOW_AFTER, RegisterRole,
    SignalPolicy, StackFrameId, StepKind, StopReason, ThreadId, VirtualAddress, WatchAccess,
    WatchpointId, WatchpointSpec,
};

use super::format::{self, plural};
use super::terminal::{Renderer, Role};
use super::value;
use super::{Cli, Control};

const DEFAULT_HEX_DUMP_BYTES: u64 = 64;
pub const MAX_HEX_DUMP_BYTES: u64 = 8 * 1024;
/// Instructions shown before and from a stop that no function contains.
const DISASSEMBLY_CONTEXT_BEFORE: u32 = 8;
const DISASSEMBLY_CONTEXT_AFTER: u32 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Handle,
    Views,
    Break,
    Tbreak,
    Breakpoints,
    Info,
    Delete,
    Enable,
    Disable,
    Ignore,
    Hits,
    Condition,
    Watch,
    AccessWatch,
    ReadWatch,
    Watchpoints,
    Unwatch,
    Run,
    Continue,
    Print,
    Whatis,
    Ptype,
    Set,
    Globals,
    Stepi,
    Nexti,
    Step,
    Next,
    Finish,
    Advance,
    Examine,
    Disassemble,
    Address,
    Where,
    List,
    Backtrace,
    Frame,
    Up,
    Down,
    Registers,
    Threads,
    Thread,
    Clear,
    Help,
    Quit,
}

pub struct CommandSpec {
    pub command: Command,
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// The name followed by one word per argument: `<required>`,
    /// `[optional]`, or a bare `alternative|list` that is required.
    pub usage: &'static str,
    pub summary: &'static str,
    /// Whether an empty interactive line repeats this command.
    pub repeatable: bool,
}

impl CommandSpec {
    /// Returns the inclusive range of argument counts the usage accepts. A
    /// final `[words...]` takes the rest of the line, and every word of a
    /// bracketed group such as `[hits hit-condition]` is optional.
    fn arity(&self) -> (usize, usize) {
        let mut grouped = false;
        let mut range = (0, 0);
        for word in self.usage.split_whitespace().skip(1) {
            let optional = grouped || word.starts_with('[');
            grouped = optional && !word.ends_with(']');
            let (minimum, maximum) = range;
            range = if word.ends_with("...]") {
                (minimum, usize::MAX)
            } else if word.ends_with("...>") {
                (minimum + 1, usize::MAX)
            } else if optional {
                (minimum, maximum.saturating_add(1))
            } else {
                (minimum + 1, maximum.saturating_add(1))
            };
        }
        range
    }

    /// Returns whether usage shows anything beyond the command name.
    pub fn takes_arguments(&self) -> bool {
        self.arity().1 != 0
    }

    pub fn usage_error(&self) -> anyhow::Error {
        anyhow!("usage: {}", self.usage)
    }
}

macro_rules! command {
    ($command:ident, $name:literal, [$($alias:literal),*], $usage:literal, $summary:literal $(, $flag:ident)?) => {
        CommandSpec {
            command: Command::$command,
            name: $name,
            aliases: &[$($alias),*],
            usage: $usage,
            summary: $summary,
            repeatable: command!(@repeatable $($flag)?),
        }
    };
    (@repeatable) => { false };
    (@repeatable repeatable) => { true };
}

/// The subcommands `info` accepts, by their primary names.
pub const INFO_SUBCOMMANDS: [&str; 6] = [
    "breakpoints",
    "watchpoints",
    "signals",
    "core",
    "symbol",
    "view",
];

pub const COMMANDS: &[CommandSpec] = &[
    command!(
        Break,
        "break",
        ["b"],
        "break [location] [if condition...] [hits hit-condition] [log message]",
        "Set a breakpoint at a function, file:line, file:function, or 0xaddress, or at the selected frame's line, line N of its file, or +N lines on"
    ),
    command!(
        Tbreak,
        "tbreak",
        [],
        "tbreak [location] [if condition...] [hits hit-condition] [log message]",
        "Set a breakpoint that the stop it causes deletes"
    ),
    command!(
        Breakpoints,
        "breakpoints",
        [],
        "breakpoints",
        "List logical breakpoints"
    ),
    command!(
        Info,
        "info",
        [],
        "info breakpoints|watchpoints|signals|core|symbol|view [argument...]",
        "Show debugger information, the symbol and section containing an address, or which view presents an expression's value and why"
    ),
    command!(
        Handle,
        "handle",
        [],
        "handle <signal> [action] [action] [action]",
        "Show or change how a signal is handled: stop|nostop, print|noprint, pass|nopass"
    ),
    command!(
        Delete,
        "delete",
        ["del", "d"],
        "delete <ids...>",
        "Delete breakpoints, and watchpoints wID, by id, range such as 3-5, or all breakpoints"
    ),
    command!(
        Enable,
        "enable",
        [],
        "enable <ids...>",
        "Enable breakpoints, and watchpoints wID, by id, range such as 3-5 or w1-2, or all"
    ),
    command!(
        Disable,
        "disable",
        [],
        "disable <ids...>",
        "Disable breakpoints, and watchpoints wID, keeping their conditions and counts, by id, range such as 3-5 or w1-2, or all"
    ),
    command!(
        Ignore,
        "ignore",
        [],
        "ignore <id> <count>",
        "Skip a breakpoint's, or watchpoint wID's, next count hits, then stop at every hit; 0 stops at the next"
    ),
    command!(
        Hits,
        "hits",
        [],
        "hits <id> <hit-condition|always>",
        "Choose which hits of a breakpoint, or watchpoint wID, stop, such as >=5, ==3, or %10"
    ),
    command!(
        Condition,
        "condition",
        [],
        "condition <id> [expression...]",
        "Stop at a breakpoint, or watchpoint wID, only where an expression such as x > 3 && p->next != NULL holds; without one, always"
    ),
    command!(
        Watch,
        "watch",
        [],
        "watch [-w] <expression|0xaddress:byte-count> [if condition...]",
        "Stop when a store changes watched memory; with -w, at every store, even of the same value; with if, only where the condition holds"
    ),
    command!(
        AccessWatch,
        "awatch",
        [],
        "awatch <expression|0xaddress:byte-count> [if condition...]",
        "Stop when watched memory is read or written, and with if, the condition holds"
    ),
    command!(
        ReadWatch,
        "rwatch",
        [],
        "rwatch <expression|0xaddress:byte-count> [if condition...]",
        "Stop when watched memory is read, and with if, the condition holds"
    ),
    command!(
        Watchpoints,
        "watchpoints",
        [],
        "watchpoints",
        "List armed watchpoints"
    ),
    command!(
        Unwatch,
        "unwatch",
        [],
        "unwatch <ids...>",
        "Delete watchpoints by id, range such as 1-3, or all"
    ),
    command!(Run, "run", ["r"], "run", "Launch the inferior"),
    command!(
        Continue,
        "continue",
        ["c"],
        "continue",
        "Continue execution",
        repeatable
    ),
    command!(
        Print,
        "print",
        ["p"],
        "print [expression...]",
        "Print an expression's value, or every variable; print/x shows integers in hexadecimal, and print/r values as stored, without views"
    ),
    command!(
        Whatis,
        "whatis",
        [],
        "whatis <expression...>",
        "Show an expression's type"
    ),
    command!(
        Ptype,
        "ptype",
        [],
        "ptype <expression-or-type...>",
        "Show a type's definition, or the definition of an expression's type"
    ),
    command!(
        Set,
        "set",
        [],
        "set [var] <assignment...>",
        "Change a number, truth value, enumeration, or pointer, such as set var x = y + 1; set views on|off shows values as their views present them or as stored"
    ),
    command!(
        Views,
        "views",
        [],
        "views [load|clear|check|explain|record] [argument...]",
        "List the view files values are presented with, load more, clear those loaded, check how the program's types are presented, explain which view presents a type, or record the kernel runs presenting a value to a file"
    ),
    command!(
        Globals,
        "globals",
        [],
        "globals [filter]",
        "List global variable metadata"
    ),
    command!(
        Stepi,
        "stepi",
        ["si"],
        "stepi",
        "Step one instruction",
        repeatable
    ),
    command!(
        Nexti,
        "nexti",
        ["ni"],
        "nexti",
        "Step one instruction, running a call until it returns",
        repeatable
    ),
    command!(
        Step,
        "step",
        ["s"],
        "step",
        "Step into at source level",
        repeatable
    ),
    command!(
        Next,
        "next",
        ["n"],
        "next",
        "Step over at source level",
        repeatable
    ),
    command!(
        Finish,
        "finish",
        ["fin", "f"],
        "finish",
        "Run until the selected frame returns to its caller",
        repeatable
    ),
    command!(
        Advance,
        "advance",
        ["adv"],
        "advance <function|0xaddress|file:line|file:function>",
        "Run until the selected thread reaches a location, or the selected frame returns first"
    ),
    command!(
        Examine,
        "x",
        [],
        "x <0xaddress> [byte-count]",
        "Display target memory as hexadecimal bytes and ASCII",
        repeatable
    ),
    command!(
        Disassemble,
        "disassemble",
        ["disas"],
        "disassemble [function|0xaddress] [instruction-count]",
        "Disassemble a function, or instructions from an address"
    ),
    command!(
        Address,
        "address",
        [],
        "address <symbol>",
        "Resolve a symbol's runtime address"
    ),
    command!(
        Where,
        "where",
        [],
        "where",
        "Show the selected frame's execution location"
    ),
    command!(
        List,
        "list",
        ["l"],
        "list",
        "Show source around the selected frame's location",
        repeatable
    ),
    command!(
        Backtrace,
        "backtrace",
        ["bt"],
        "backtrace",
        "Show the selected thread's stack"
    ),
    command!(
        Frame,
        "frame",
        ["fr"],
        "frame [level]",
        "Show the selected frame, or select a frame by its backtrace level"
    ),
    command!(
        Up,
        "up",
        [],
        "up [count]",
        "Select an outer frame, toward the callers",
        repeatable
    ),
    command!(
        Down,
        "down",
        ["do"],
        "down [count]",
        "Select an inner frame, toward where execution stopped",
        repeatable
    ),
    command!(
        Registers,
        "registers",
        ["regs"],
        "registers",
        "Show native registers"
    ),
    command!(Threads, "threads", [], "threads", "List threads"),
    command!(Thread, "thread", [], "thread <id>", "Select a thread"),
    command!(
        Clear,
        "clear",
        ["cls"],
        "clear",
        "Clear and redraw the terminal"
    ),
    command!(
        Help,
        "help",
        ["h", "?"],
        "help [command]",
        "Show command help"
    ),
    command!(Quit, "quit", ["q"], "quit", "Exit uscope"),
];

pub fn command_named(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS
        .iter()
        .find(|command| command.name == name || command.aliases.contains(&name))
}

/// The command a line starts with, and the name it is written with, which
/// excludes a format such as the `/x` of `p/x`.
pub fn line_command(line: &str) -> Option<(&'static CommandSpec, &str)> {
    let written = line.split_whitespace().next()?;
    let name = written.split_once('/').map_or(written, |(name, _)| name);
    command_named(name).map(|spec| (spec, name))
}

impl Cli {
    /// A line whose first word is an alias from the settings, with the
    /// command line the alias stands for in its place.
    pub(super) fn expand_alias(&self, line: &str) -> Option<String> {
        let word = line.split_whitespace().next()?;
        let expansion = self.settings.config.aliases.get(word)?;
        let rest = line.trim_start()[word.len()..].trim();
        Some(if rest.is_empty() {
            expansion.clone()
        } else {
            format!("{expansion} {rest}")
        })
    }

    /// Parses and executes one non-empty command line.
    pub(super) async fn execute(&self, line: &str) -> Result<Control> {
        let expanded = self.expand_alias(line);
        let line = expanded.as_deref().unwrap_or(line);
        let (spec, format, rest, arguments) = command_line(line)?;
        let first = arguments.first().copied();
        let renderer = self.renderers.stdout;
        let debugger = &self.debugger;

        let output = match spec.command {
            Command::Break | Command::Tbreak => {
                self.add_breakpoint(rest, spec.command == Command::Tbreak, spec)
                    .await?
            }
            Command::Breakpoints => self.list_breakpoints().await?,
            Command::Info => match (arguments[0], arguments.get(1)) {
                ("breakpoints" | "break", None) => self.list_breakpoints().await?,
                ("watchpoints" | "watch", None) => self.list_watchpoints().await?,
                ("core", None) => debugger
                    .core_dump()
                    .map(|core| format::core_dump(core, renderer))
                    .ok_or_else(|| anyhow!("no core dump is open"))?,
                ("symbol", Some(address)) => format::address_description(
                    &debugger.describe_address(parse_address(address)?).await?,
                    renderer,
                ),
                ("signals" | "handle", None) => self.list_signals().await?,
                ("view", Some(_)) => {
                    let text = rest.trim_start()["view".len()..].trim();
                    self.explain_view(text).await?
                }
                _ => return Err(spec.usage_error()),
            },
            Command::Handle => self.handle_signal(&arguments).await?,
            Command::Delete => self.delete(&arguments, false, spec).await?,
            Command::Enable => self.set_enabled(&arguments, true, spec).await?,
            Command::Disable => self.set_enabled(&arguments, false, spec).await?,
            Command::Ignore => self.ignore(arguments[0], arguments[1], spec).await?,
            Command::Hits => self.hits(arguments[0], arguments[1], spec).await?,
            Command::Condition => self.condition(arguments[0], &arguments[1..], spec).await?,
            Command::Watch => self.watch_stores(&arguments, spec).await?,
            Command::AccessWatch => self.watch(&arguments, WatchAccess::ReadWrite, spec).await?,
            Command::ReadWatch => self.watch(&arguments, WatchAccess::Read, spec).await?,
            Command::Watchpoints => self.list_watchpoints().await?,
            Command::Unwatch => self.delete(&arguments, true, spec).await?,
            Command::Run => {
                self.execute_until_stop(debugger.run_with(self.launch.options()))
                    .await?
            }
            Command::Continue => self.execute_until_stop(debugger.resume()).await?,
            Command::Print => match first {
                Some(_) => self.print(rest, format == "x", format == "r").await?,
                None => value::variables(&debugger.variables().await?, renderer),
            },
            Command::Whatis => self.whatis(rest).await?,
            Command::Ptype => self.ptype(rest).await?,
            Command::Globals => self.globals(first).await?,
            Command::Views => self.views(&arguments).await?,
            Command::Set => self.set(rest, spec).await?,
            Command::Stepi => self.step(StepKind::Instruction).await?,
            Command::Nexti => self.step(StepKind::OverInstruction).await?,
            Command::Step => self.step(StepKind::IntoSource).await?,
            Command::Next => self.step(StepKind::OverSource).await?,
            Command::Finish => self.step(StepKind::Out).await?,
            Command::Advance => {
                let location =
                    parse_breakpoint_location(arguments[0])?.ok_or_else(|| spec.usage_error())?;
                self.execute_until_stop(debugger.advance(location)).await?
            }
            Command::Examine => {
                let address = parse_address(arguments[0])?;
                let byte_count = parse_memory_byte_count(arguments.get(1).copied(), spec)?;
                format::memory_read(&debugger.read_memory(address, byte_count).await?, renderer)
            }
            Command::Disassemble => self.disassemble(first, arguments.get(1).copied()).await?,
            Command::Address => self.address(arguments[0]).await?,
            Command::Where => self.location().await?,
            Command::List => format::source_context(&self.source_context().await?, renderer),
            Command::Backtrace => self.backtrace().await?,
            Command::Frame | Command::Up | Command::Down => {
                self.frame(parse_frame_target(spec, first)?).await?
            }
            Command::Registers => format::registers(&debugger.registers().await?, renderer),
            Command::Threads => format::threads(&debugger.snapshot().await?, renderer),
            Command::Thread => self.select_thread(arguments[0]).await?,
            Command::Clear => return Ok(Control::ClearScreen),
            Command::Help => match first {
                Some(name) => format::command_help(
                    command_named(name).ok_or_else(|| anyhow!("unknown command '{name}'"))?,
                    renderer,
                ),
                None => format::help(&self.settings.config.aliases, renderer),
            },
            Command::Quit => return Ok(Control::Quit),
        };
        Ok(Control::Continue(output))
    }

    /// Runs one command for a client that controls execution itself, such
    /// as a debug adapter's console, or returns `None` when the line names
    /// no command. Commands that run the inferior or end the session are
    /// refused, since the client owns those.
    pub async fn console(&self, line: &str) -> Result<Option<String>> {
        let line = line.trim();
        let Some((spec, _)) = line_command(line) else {
            return Ok(None);
        };
        if matches!(
            spec.command,
            Command::Run
                | Command::Continue
                | Command::Step
                | Command::Next
                | Command::Stepi
                | Command::Nexti
                | Command::Finish
                | Command::Advance
                | Command::Clear
                | Command::Quit
        ) {
            bail!(
                "`{}` is not available in the debug console; use the debugger's controls",
                spec.name
            );
        }
        match self.execute(line).await? {
            Control::Continue(output) => Ok(Some(output)),
            Control::ClearScreen | Control::Quit => unreachable!("refused above"),
        }
    }

    async fn address(&self, symbol: &str) -> Result<String> {
        let renderer = self.renderers.stdout;
        Ok(format!(
            "{}: {}",
            renderer.paint(Role::Name, symbol),
            renderer.paint(Role::Metadata, self.debugger.runtime_address(symbol).await?)
        ))
    }

    /// Deletes the breakpoints and watchpoints `words` name, once every one
    /// is known to exist; `all` deletes every breakpoint, or for `unwatch`
    /// every watchpoint, whose ids bare numbers name.
    async fn delete(&self, words: &[&str], watches: bool, spec: &CommandSpec) -> Result<String> {
        let renderer = self.renderers.stdout;
        let Some(ids) = self.existing_ids(words, watches, spec).await? else {
            let what = if watches {
                plural(
                    self.debugger.remove_all_watchpoints().await?.len() as u64,
                    "watchpoint",
                )
            } else {
                plural(
                    self.debugger.remove_all_breakpoints().await?.len() as u64,
                    "breakpoint",
                )
            };
            return Ok(format::deleted(&what, renderer));
        };
        for &id in &ids {
            match id {
                CountedId::Breakpoint(id) => {
                    self.debugger.remove_breakpoint(id).await?;
                }
                CountedId::Watchpoint(id) => {
                    self.debugger.remove_watchpoint(id).await?;
                }
            }
        }
        Ok(format::deleted(&describe_ids(&ids, renderer), renderer))
    }

    /// The ids `words` name, each known to exist, or `None` for `all`.
    /// Bare numbers name watchpoints when `watches`, and breakpoints
    /// otherwise.
    async fn existing_ids(
        &self,
        words: &[&str],
        watches: bool,
        spec: &CommandSpec,
    ) -> Result<Option<Vec<CountedId>>> {
        let Some(ids) = parse_ids(words, watches, spec)? else {
            return Ok(None);
        };
        let snapshot = self.debugger.snapshot().await?;
        let renderer = Renderer::new(false);
        for &id in &ids {
            let known = match id {
                CountedId::Breakpoint(id) => snapshot
                    .breakpoints
                    .iter()
                    .any(|breakpoint| breakpoint.id == id),
                CountedId::Watchpoint(id) => snapshot
                    .watchpoints
                    .iter()
                    .any(|watchpoint| watchpoint.id == id),
            };
            if !known {
                bail!("{} was not found", id.describe(renderer));
            }
        }
        Ok(Some(ids))
    }

    /// Adds the breakpoint a `break` or `tbreak` line describes.
    async fn add_breakpoint(
        &self,
        line: &str,
        temporary: bool,
        spec: &CommandSpec,
    ) -> Result<String> {
        let parsed = parse_break(line, spec)?;
        let location = match parsed.location {
            Some(written) => match frame_line(written)? {
                Some(line) => self.frame_source(line).await?,
                None => parse_breakpoint_location(written)?.ok_or_else(|| spec.usage_error())?,
            },
            None => self.frame_source(FrameLine::Offset(0)).await?,
        };
        let options = uscope::BreakpointOptions {
            hit_condition: parsed.hits.map(str::parse).transpose()?,
            condition: parsed.condition.map(uscope::Condition::parse).transpose()?,
            log_message: parsed
                .log
                .as_deref()
                .map(uscope::LogMessage::parse)
                .transpose()?,
            temporary,
            ..uscope::BreakpointOptions::default()
        };
        let breakpoint = self.debugger.add_breakpoint_with(location, options).await?;
        let placed = self.placed(&breakpoint).await;
        Ok(format::breakpoint(
            &breakpoint,
            &placed,
            self.renderers.stdout,
        ))
    }

    /// A line of the selected frame's source file.
    async fn frame_source(&self, line: FrameLine) -> Result<BreakpointSpec> {
        let location = self.debugger.current_location().await.map_err(|error| {
            anyhow!("no frame is selected to take a line from ({error}); name a location")
        })?;
        let (Some(source), Ok(image)) = (
            location.image.source.as_ref(),
            self.debugger.loaded_module_image(location.module).await,
        ) else {
            bail!("the selected frame has no source line");
        };
        let file = image
            .source_file(source.file)
            .ok_or_else(|| anyhow!("the selected frame has no source file"))?;
        let number = match line {
            FrameLine::Number(number) => number,
            FrameLine::Offset(offset) => source
                .line
                .get()
                .checked_add_signed(offset)
                .filter(|line| *line > 0)
                .ok_or_else(|| anyhow!("line {} {offset:+} is before the file", source.line))?,
        };
        Ok(BreakpointSpec::Source {
            path: file.path.as_ref().clone(),
            line: LineNumber::new(number)
                .ok_or_else(|| anyhow!("source line numbers are one-based"))?,
        })
    }

    /// Where each of a breakpoint's locations is, as far as its module
    /// tells.
    pub(super) async fn placed(&self, breakpoint: &uscope::Breakpoint) -> Vec<format::Placed> {
        let mut placed = Vec::new();
        for resolved in breakpoint.locations.iter() {
            placed.push(self.place(resolved.location).await);
        }
        placed
    }

    async fn place(&self, location: uscope::BreakpointLocation) -> format::Placed {
        let mut placed = format::Placed {
            location,
            function: None,
            source: None,
            module: None,
        };
        let (image, address) = match location {
            uscope::BreakpointLocation::Image(address) => (
                Some(std::sync::Arc::clone(self.debugger.module_image())),
                address,
            ),
            uscope::BreakpointLocation::Virtual(address) => {
                let Ok(description) = self.debugger.describe_address(address).await else {
                    return placed;
                };
                let Some(module) = description.module else {
                    return placed;
                };
                if module.path.as_path() != self.debugger.module_image().path() {
                    placed.module = Some(std::sync::Arc::clone(&module.path));
                }
                (
                    self.debugger.loaded_module_image(module.module).await.ok(),
                    module.image.address,
                )
            }
        };
        let Some(image) = image else {
            return placed;
        };
        let located = image.locate(address);
        let inline = match &located.inline_frames {
            uscope::InlineFrameLookup::Unique(chain) => chain.instances.last().copied(),
            _ => None,
        };
        placed.function = inline
            .and_then(|instance| image.code_instance(instance))
            .and_then(|instance| image.function(instance.function))
            .or(located.function.as_ref())
            .map(|function| std::sync::Arc::clone(&function.name));
        placed.source = located.source.as_ref().and_then(|source| {
            image
                .source_file(source.file)
                .map(|file| (std::sync::Arc::clone(&file.path), source.line))
        });
        placed
    }

    /// Enables or disables the breakpoints and watchpoints `words` name,
    /// once every one is known to exist.
    async fn set_enabled(
        &self,
        words: &[&str],
        enabled: bool,
        spec: &CommandSpec,
    ) -> Result<String> {
        let ids = if let Some(ids) = self.existing_ids(words, false, spec).await? {
            ids
        } else {
            let snapshot = self.debugger.snapshot().await?;
            snapshot
                .breakpoints
                .iter()
                .map(|breakpoint| CountedId::Breakpoint(breakpoint.id))
                .chain(
                    snapshot
                        .watchpoints
                        .iter()
                        .map(|watchpoint| CountedId::Watchpoint(watchpoint.id)),
                )
                .collect()
        };
        if ids.is_empty() {
            return Ok("no breakpoints or watchpoints".to_owned());
        }
        for &id in &ids {
            match id {
                CountedId::Breakpoint(id) => {
                    self.debugger.set_breakpoint_enabled(id, enabled).await?;
                }
                CountedId::Watchpoint(id) => {
                    self.debugger.set_watchpoint_enabled(id, enabled).await?;
                }
            }
        }
        let renderer = self.renderers.stdout;
        Ok(format!(
            "{} {}",
            renderer.paint(Role::Success, if enabled { "enabled" } else { "disabled" }),
            describe_ids(&ids, renderer)
        ))
    }

    async fn hits(&self, id: &str, condition: &str, spec: &CommandSpec) -> Result<String> {
        let condition = match condition {
            "always" => None,
            condition => Some(condition.parse()?),
        };
        let renderer = self.renderers.stdout;
        Ok(match parse_counted_id(id, spec)? {
            CountedId::Breakpoint(id) => format::breakpoint_hit_condition(
                &self
                    .debugger
                    .set_breakpoint_hit_condition(id, condition)
                    .await?,
                renderer,
            ),
            CountedId::Watchpoint(id) => format::watchpoint_hit_condition(
                &self
                    .debugger
                    .set_watchpoint_hit_condition(id, condition)
                    .await?,
                renderer,
            ),
        })
    }

    /// Sets or removes a breakpoint's or watchpoint's condition, as gdb's
    /// `condition` does.
    async fn condition(&self, id: &str, words: &[&str], spec: &CommandSpec) -> Result<String> {
        let id = parse_counted_id(id, spec)?;
        let condition = if words.is_empty() {
            None
        } else {
            Some(uscope::Condition::parse(&words.join(" "))?)
        };
        let renderer = self.renderers.stdout;
        let id = match id {
            CountedId::Breakpoint(id) => id,
            CountedId::Watchpoint(id) => {
                return Ok(format::watchpoint_condition(
                    &self
                        .debugger
                        .set_watchpoint_condition(id, condition)
                        .await?,
                    renderer,
                ));
            }
        };
        let breakpoint = self
            .debugger
            .set_breakpoint_condition(id, condition)
            .await?;
        Ok(breakpoint.condition.as_ref().map_or_else(
            || {
                format!(
                    "breakpoint {} stops unconditionally",
                    renderer.paint(Role::Metadata, breakpoint.id)
                )
            },
            |condition| {
                format!(
                    "breakpoint {} stops where {} holds",
                    renderer.paint(Role::Metadata, breakpoint.id),
                    renderer.paint(Role::Value, condition)
                )
            },
        ))
    }

    /// Skips a breakpoint's or watchpoint's next `count` hits like gdb's
    /// `ignore`: the hit condition becomes `>=` the hit after them.
    async fn ignore(&self, id: &str, count: &str, spec: &CommandSpec) -> Result<String> {
        let id = parse_counted_id(id, spec)?;
        let count = parse_count(count).ok_or_else(|| spec.usage_error())?;
        let renderer = self.renderers.stdout;
        let condition = if count == 0 {
            None
        } else {
            let snapshot = self.debugger.snapshot().await?;
            let hits = match id {
                CountedId::Breakpoint(id) => snapshot
                    .breakpoints
                    .iter()
                    .find(|breakpoint| breakpoint.id == id)
                    .map(|breakpoint| breakpoint.hit_count),
                CountedId::Watchpoint(id) => snapshot
                    .watchpoints
                    .iter()
                    .find(|watchpoint| watchpoint.id == id)
                    .map(|watchpoint| watchpoint.hit_count),
            }
            .ok_or_else(|| anyhow!("{} was not found", id.describe(Renderer::new(false))))?;
            let first_stop = hits
                .checked_add(count)
                .and_then(|skipped| skipped.checked_add(1))
                .ok_or_else(|| anyhow!("ignore count {count} is too large"))?;
            Some(HitCondition::new(
                HitComparison::GreaterOrEqual,
                first_stop,
            )?)
        };
        match id {
            CountedId::Breakpoint(id) => {
                self.debugger
                    .set_breakpoint_hit_condition(id, condition)
                    .await?;
            }
            CountedId::Watchpoint(id) => {
                self.debugger
                    .set_watchpoint_hit_condition(id, condition)
                    .await?;
            }
        }
        Ok(if count == 0 {
            format!("{} stops at its next hit", id.describe(renderer))
        } else {
            format!(
                "{} ignores its next {}",
                id.describe(renderer),
                plural(count, "hit")
            )
        })
    }

    async fn select_thread(&self, argument: &str) -> Result<String> {
        let id = argument
            .parse()
            .map_err(|_| anyhow!("invalid thread ID: {argument}"))?;
        self.debugger.select_thread(ThreadId::new(id)).await?;
        let renderer = self.renderers.stdout;
        Ok(format!(
            "{} thread {}",
            renderer.paint(Role::Success, "selected"),
            renderer.paint(Role::Metadata, id)
        ))
    }

    async fn step(&self, kind: StepKind) -> Result<String> {
        self.execute_until_stop(self.debugger.step(kind)).await
    }

    /// Waits for an execution request, prefixing its stop with a line for
    /// each signal received without stopping, message logged, condition
    /// that failed, and loaded library view that cannot be used.
    async fn execute_until_stop(
        &self,
        execution: impl std::future::Future<Output = uscope::Result<StopReason>>,
    ) -> Result<String> {
        let mut events = self.debugger.subscribe();
        let mut lines = Vec::new();
        let mut loaded = Vec::new();
        let renderer = self.renderers.stdout;
        let mut record = |event: Result<DebuggerEvent, _>| match event {
            Ok(DebuggerEvent::ModuleLoaded { module, .. }) => loaded.push(module.module.id),
            Ok(DebuggerEvent::SignalReceived {
                thread_id,
                exception,
                ..
            }) => lines.push(format::signal_received(thread_id, &exception, renderer)),
            Ok(DebuggerEvent::LogMessage { parts, .. }) => lines.push(format::log_message(&parts)),
            Ok(DebuggerEvent::ConditionFailed { owner, error, .. }) => lines.push(format!(
                "{}: the condition of {} could not be evaluated: {error}",
                renderer.paint(Role::Warning, "warning"),
                renderer.paint(Role::Metadata, format::condition_owner(owner))
            )),
            _ => {}
        };
        tokio::pin!(execution);
        let reason = loop {
            tokio::select! {
                biased;
                event = events.recv() => record(event),
                reason = &mut execution => break reason?,
            }
        };
        while let Ok(event) = events.try_recv() {
            record(Ok(event));
        }
        // What kept a library's own views out, once, when it loads.
        for module in loaded {
            if let Ok(image) = self.debugger.loaded_module_image(module).await {
                lines.extend(image.view_errors().iter().map(|error| {
                    format!(
                        "{}: views: {error}",
                        renderer.paint(Role::Warning, "warning")
                    )
                }));
            }
        }
        let stop = self.stop_with_source(&reason).await;
        Ok(join_lines(&lines.join("\n"), &stop))
    }

    async fn list_breakpoints(&self) -> Result<String> {
        Ok(format::breakpoints(
            &self.debugger.snapshot().await?.breakpoints,
            self.renderers.stdout,
        ))
    }

    async fn list_watchpoints(&self) -> Result<String> {
        Ok(format::watchpoints(
            &self.debugger.snapshot().await?.watchpoints,
            self.renderers.stdout,
        ))
    }

    /// Watches for stores that change a value, as gdb's `watch` does, or
    /// with `-w` for every store.
    async fn watch_stores(&self, arguments: &[&str], spec: &CommandSpec) -> Result<String> {
        match arguments {
            ["-w", rest @ ..] => self.watch(rest, WatchAccess::Write, spec).await,
            [target, ..] if !target.starts_with('-') => {
                self.watch(arguments, WatchAccess::Change, spec).await
            }
            _ => Err(spec.usage_error()),
        }
    }

    /// Watches a target, the first of `arguments`, which may be followed by
    /// `if` and a condition, as in gdb.
    async fn watch(
        &self,
        arguments: &[&str],
        access: WatchAccess,
        spec: &CommandSpec,
    ) -> Result<String> {
        let (argument, condition) = match arguments {
            [target] => (*target, None),
            [target, "if", condition @ ..] if !condition.is_empty() => (
                *target,
                Some(uscope::Condition::parse(&condition.join(" "))?),
            ),
            _ => return Err(spec.usage_error()),
        };
        if !self
            .debugger
            .watchpoint_capabilities()
            .access
            .contains(&access)
        {
            return Err(uscope::Error::UnsupportedWatchAccess(access).into());
        }
        let options = uscope::WatchpointOptions {
            condition,
            ..uscope::WatchpointOptions::default()
        };
        let watchpoint = if let Some(location) = parse_watch_location(argument, spec)? {
            self.debugger
                .add_watchpoint_with(location, access, options)
                .await?
        } else {
            let expression = parse_expression(argument)?;
            self.debugger
                .watch_with(&expression, access, options)
                .await
                .map_err(|error| expression_error(argument, error))?
        };
        Ok(format::watchpoint_set(&watchpoint, self.renderers.stdout))
    }

    async fn print(&self, text: &str, hexadecimal: bool, raw: bool) -> Result<String> {
        let renderer = self.renderers.stdout;
        let expression = parse_expression(text)?;
        let evaluation = self
            .debugger
            .evaluate(&expression)
            .await
            .map_err(|error| expression_error(text, error))?;
        let (inspected, cause) = match evaluation {
            uscope::Evaluation::Range(page) => return Ok(value::range(text, &page, renderer)),
            uscope::Evaluation::Value { value, cause } => (value, cause),
            _ => bail!("the evaluation produced an unknown kind of result"),
        };
        let mut output = match &inspected.type_info {
            Some(type_info) if hexadecimal => {
                value::hexadecimal(type_info, text, &inspected.state, renderer)
            }
            Some(type_info) => {
                value::expanded(
                    &self.debugger,
                    type_info,
                    text,
                    &inspected.state,
                    uscope::InspectionLimits::default().remaining_after(inspected.usage),
                    raw,
                    renderer,
                )
                .await?
            }
            None => value::untyped(text, &inspected.state, renderer),
        };
        // Say which operand the program could not provide, when it is not
        // the whole expression.
        if let Some(cause) = cause
            && cause.text(text) != text
        {
            let _ = write!(
                output,
                " {}",
                renderer.paint(
                    Role::Metadata,
                    format!("(because of `{}`)", cause.text(text))
                )
            );
        }
        Ok(output)
    }

    /// Which view presents an expression's value, from where, and why each
    /// view tried before it did not bind.
    async fn explain_view(&self, text: &str) -> Result<String> {
        let expression = parse_expression(text)?;
        let explanation = self
            .debugger
            .explain_view(&expression)
            .await
            .map_err(|error| expression_error(text, error))?;
        Ok(format::view_explanation(
            text,
            &explanation,
            self.renderers.stdout,
        ))
    }

    async fn whatis(&self, text: &str) -> Result<String> {
        let expression = parse_expression(text)?;
        let type_info = self
            .debugger
            .expression_type(&expression)
            .await
            .map_err(|error| expression_error(text, error))?;
        Ok(format!(
            "type = {}",
            self.renderers.stdout.paint(Role::Type, &type_info.name)
        ))
    }

    /// Shows a type's definition: of a type named, or of an expression's.
    async fn ptype(&self, text: &str) -> Result<String> {
        let as_expression = match uscope::Expression::parse(text) {
            Ok(expression) => self.debugger.expression_type(&expression).await,
            Err(error) => Err(uscope::Error::Expression(error)),
        };
        let type_info = match as_expression {
            Ok(type_info) => type_info,
            Err(expression_failure) => {
                // A type is measured through a pointer to it, which reads
                // nothing.
                let through = uscope::Expression::parse(&format!("*({text}*)null"));
                let found = match through {
                    Ok(expression) => self.debugger.expression_type(&expression).await.ok(),
                    Err(_) => None,
                };
                found.ok_or_else(|| expression_error(text, expression_failure))?
            }
        };
        let images = self.module_images().await?;
        Ok(value::type_definition(
            &type_info,
            &images,
            self.renderers.stdout,
        ))
    }

    async fn module_images(&self) -> Result<Vec<std::sync::Arc<uscope::ModuleImage>>> {
        let modules = self.debugger.loaded_modules().await?;
        let mut images = Vec::new();
        for module in modules.modules.iter() {
            images.push(self.debugger.loaded_module_image(module.module.id).await?);
        }
        Ok(images)
    }

    /// Assigns a value in the selected frame, as gdb's `set var` does.
    async fn set(&self, text: &str, spec: &CommandSpec) -> Result<String> {
        if let Some(setting @ ("on" | "off")) = text.strip_prefix("views ").map(str::trim) {
            self.debugger.enable_views(setting == "on").await?;
            return Ok(format!(
                "values show {}",
                if setting == "on" {
                    "as their views present them"
                } else {
                    "as stored"
                }
            ));
        }
        let text = text.strip_prefix("var ").map_or(text, str::trim);
        let expression = parse_expression(text)?;
        let Some(target) = expression.assignment_target() else {
            return Err(spec.usage_error());
        };
        let evaluation = self
            .debugger
            .evaluate_with(
                &expression,
                uscope::EvaluationMode::Assign,
                uscope::InspectionLimits::default(),
            )
            .await
            .map_err(|error| expression_error(text, error))?;
        let uscope::Evaluation::Value {
            value: assigned, ..
        } = evaluation
        else {
            bail!("the assignment produced no value");
        };
        let renderer = self.renderers.stdout;
        Ok(match &assigned.type_info {
            Some(type_info) => format!(
                "({}) {} = {}",
                renderer.paint(Role::Type, &type_info.name),
                renderer.paint(Role::Name, target),
                renderer.paint(
                    Role::Value,
                    value::summary(Some(type_info), &assigned.state)
                )
            ),
            None => value::untyped(target, &assigned.state, renderer),
        })
    }

    /// Runs `views` and its subcommands.
    async fn views(&self, arguments: &[&str]) -> Result<String> {
        match arguments {
            [] => {
                let mut lines = vec!["values are presented with, in order:".to_owned()];
                {
                    let views = self.views.lock().expect("the view sources are whole");
                    lines.extend(
                        views
                            .session
                            .iter()
                            .map(|file| format!("  {} (loaded)", file.name)),
                    );
                    lines.extend(
                        views
                            .discovered
                            .iter()
                            .map(|file| format!("  {}", file.name)),
                    );
                }
                lines.push("  the views each module carries for its own types".to_owned());
                lines.push("  the built-in views".to_owned());
                Ok(lines.join("\n"))
            }
            ["load", paths @ ..] if !paths.is_empty() => {
                let files = paths
                    .iter()
                    .map(|path| uscope::view_files::read(std::path::Path::new(path)))
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|error| anyhow!(error))?;
                {
                    let mut views = self.views.lock().expect("the view sources are whole");
                    views.session.splice(0..0, files.into_iter().rev());
                }
                for warning in self.reload_views().await {
                    self.warn(&format!("views: {warning}"));
                }
                Ok(format!(
                    "loaded {}",
                    plural(paths.len() as u64, "view file")
                ))
            }
            ["clear"] => {
                self.views
                    .lock()
                    .expect("the view sources are whole")
                    .session
                    .clear();
                for warning in self.reload_views().await {
                    self.warn(&format!("views: {warning}"));
                }
                Ok("forgot the loaded view files".to_owned())
            }
            ["check"] => {
                let check = self.debugger.check_views().await?;
                Ok(format::view_check(&check, self.renderers.stdout).0)
            }
            ["explain", words @ ..] if !words.is_empty() => {
                let name = words.join(" ");
                let types = self.debugger.explain_type(&name).await?;
                Ok(format::type_views(&name, &types, self.renderers.stdout))
            }
            ["record", path, words @ ..] if !words.is_empty() => {
                let text = words.join(" ");
                let expression = parse_expression(&text)?;
                let recordings = self
                    .debugger
                    .record_kernels(&expression)
                    .await
                    .map_err(|error| expression_error(&text, error))?;
                if recordings.is_empty() {
                    return Ok(format!("no kernel ran presenting `{text}`"));
                }
                std::fs::write(path, recordings.concat())
                    .with_context(|| format!("failed to write {path}"))?;
                Ok(format!(
                    "recorded {} to {path}",
                    plural(recordings.len() as u64, "kernel run")
                ))
            }
            _ => bail!(
                "usage: views [load <file...>|clear|check|explain <type...>|record <file> <expression>]"
            ),
        }
    }

    async fn globals(&self, filter: Option<&str>) -> Result<String> {
        let page = self
            .debugger
            .globals(uscope::GlobalVariableQuery {
                filter: filter.map(str::to_owned),
                ..uscope::GlobalVariableQuery::default()
            })
            .await?;
        Ok(format::globals(&page, self.renderers.stdout))
    }

    /// Disassembles the function containing the stopped instruction, a named
    /// function, or the function containing an address; with a count,
    /// disassembles that many instructions from the address instead.
    async fn disassemble(&self, target: Option<&str>, count: Option<&str>) -> Result<String> {
        let (code_address, marked) = self.selected_code().await?;
        let count = count
            .map(|count| {
                parse_count(count)
                    .and_then(|count| u32::try_from(count).ok())
                    .filter(|count| (1..=MAX_WINDOW_AFTER).contains(count))
                    .ok_or_else(|| {
                        anyhow!("instruction count must be between 1 and {MAX_WINDOW_AFTER}")
                    })
            })
            .transpose()?;
        let address = match target {
            None => code_address,
            Some(target) if target.starts_with("0x") => parse_address(target)?,
            Some(name) => self.debugger.runtime_address(name).await?,
        };
        let range = count.map_or(DisassemblyRange::Function(address), |after| {
            DisassemblyRange::Window {
                address,
                before: 0,
                after,
            }
        });
        let disassembly = match self.disassemble_query(range).await {
            // Code outside every function, such as the vDSO's unexported
            // helpers, is shown around the frame's instruction instead.
            Err(uscope::Error::NoFunctionContainsAddress(_)) if target.is_none() => {
                self.disassemble_query(DisassemblyRange::Window {
                    address: marked,
                    before: DISASSEMBLY_CONTEXT_BEFORE,
                    after: DISASSEMBLY_CONTEXT_AFTER,
                })
                .await?
            }
            Err(uscope::Error::NoFunctionContainsAddress(address)) => {
                bail!(
                    "no function or code symbol contains {address}; give an instruction count to disassemble from it"
                )
            }
            result => result?,
        };

        let mut images = BTreeMap::new();
        for module in format::disassembly_modules(&disassembly) {
            images.insert(module, self.debugger.loaded_module_image(module).await?);
        }
        let modules = self.debugger.loaded_modules().await?;
        Ok(format::disassembly(
            &disassembly,
            Some(marked),
            &modules,
            &images,
            self.settings.config.disassembly.show_bytes,
            self.renderers.stdout,
        ))
    }

    async fn disassemble_query(&self, range: DisassemblyRange) -> uscope::Result<Disassembly> {
        self.debugger
            .disassemble(DisassemblyQuery {
                range,
                syntax: self.syntax,
            })
            .await
    }

    /// Returns the address of the selected frame's code, by which its
    /// function is found, and its instruction. They differ in an outer
    /// frame, whose instruction is a return address that can lie past the
    /// end of a function ending in a call.
    async fn selected_code(&self) -> Result<(VirtualAddress, VirtualAddress)> {
        // The innermost frame's instruction is the program counter, known
        // even where no module or single inline frame describes it.
        if self.selected_level().await? == 0 {
            let program_counter = self.selected_instruction().await?;
            return Ok((program_counter, program_counter));
        }
        let location = match self.debugger.current_location().await {
            Ok(location) => location,
            Err(uscope::Error::AddressOutsideModule) => {
                let instruction = self.selected_instruction().await?;
                return Ok((instruction, instruction));
            }
            Err(error) => return Err(error.into()),
        };
        let modules = self.debugger.loaded_modules().await?;
        let module = modules
            .modules
            .iter()
            .find(|record| record.module.id == location.module)
            .ok_or(uscope::Error::ModuleNotLoaded(location.module))?;
        Ok((
            module.module.virtual_address(location.image.address)?,
            location.address,
        ))
    }

    /// Returns the selected frame's instruction: the selected thread's
    /// program counter in the innermost frame.
    async fn selected_instruction(&self) -> Result<VirtualAddress> {
        let level = self.selected_level().await?;
        if level != 0 {
            let trace = self.debugger.backtrace().await?;
            return trace
                .frames
                .iter()
                .find(|frame| frame.level == level)
                .map(|frame| frame.instruction)
                .ok_or_else(|| anyhow!("the selected frame {level} no longer exists"));
        }
        let registers = self.debugger.registers().await?;
        registers
            .registers
            .iter()
            .find(|value| value.register.role == Some(RegisterRole::ProgramCounter))
            .and_then(|value| <[u8; 8]>::try_from(value.bytes.as_deref()?).ok())
            .map(|bytes| {
                VirtualAddress::new(match registers.target.byte_order {
                    ByteOrder::Little => u64::from_le_bytes(bytes),
                    ByteOrder::Big => u64::from_be_bytes(bytes),
                })
            })
            .ok_or_else(|| uscope::Error::LocationUnavailable.into())
    }

    async fn selected_level(&self) -> Result<u32> {
        Ok(self
            .debugger
            .snapshot()
            .await?
            .selected_frame
            .unwrap_or(StackFrameId::INNERMOST)
            .get())
    }

    /// Selects a frame and shows it with its source.
    async fn frame(&self, target: FrameTarget) -> Result<String> {
        let selected = u64::from(self.selected_level().await?);
        let trace = self.debugger.backtrace().await?;
        let outermost = trace.frames.len().saturating_sub(1) as u64;
        let level = match target {
            FrameTarget::Selected => selected,
            FrameTarget::Level(level) if level > outermost => {
                bail!(
                    "frame {level} does not exist; the backtrace has {} frames",
                    trace.frames.len()
                )
            }
            FrameTarget::Level(level) => level,
            FrameTarget::Outward(_) if selected >= outermost => {
                bail!(
                    "the outermost frame is selected (unwind stopped: {})",
                    trace.termination
                )
            }
            FrameTarget::Outward(count) => selected.saturating_add(count).min(outermost),
            FrameTarget::Inward(_) if selected == 0 => bail!("the innermost frame is selected"),
            FrameTarget::Inward(count) => selected.saturating_sub(count),
        };
        let frame = trace
            .frames
            .iter()
            .find(|frame| u64::from(frame.level) == level)
            .ok_or_else(|| anyhow!("the backtrace has no frame {level}"))?;
        let frame = self.debugger.select_frame(frame.id).await?;

        let renderer = self.renderers.stdout;
        let modules = self.debugger.loaded_modules().await?;
        let mut images = BTreeMap::new();
        if let (Some(module), Some(_)) = (frame.module, &frame.source) {
            images.insert(module, self.debugger.loaded_module_image(module).await?);
        }
        let mut output = format::stack_frame(&frame, Some(&modules), &images, true, renderer);
        if frame.source.is_some() {
            output.push('\n');
            match self.source_context().await {
                Ok(context) => output.push_str(&format::source_context(&context, renderer)),
                Err(error) => write!(
                    output,
                    "{}: {error}",
                    renderer.paint(Role::Warning, "source unavailable")
                )
                .expect("writing to a String cannot fail"),
            }
        }
        Ok(output)
    }

    async fn location(&self) -> Result<String> {
        let renderer = self.renderers.stdout;
        let location = match self.debugger.current_location().await {
            Ok(location) => location,
            // No loaded module describes the frame's code, such as a stop in
            // JIT-compiled code.
            Err(uscope::Error::AddressOutsideModule) => {
                return Ok(format!(
                    "{} at {} outside every loaded module",
                    renderer.paint(Role::Name, "<unknown>"),
                    renderer.paint(
                        Role::Metadata,
                        format_args!("{:#018x}", self.selected_instruction().await?)
                    )
                ));
            }
            Err(error) => return Err(error.into()),
        };
        let name = format::code_name(
            location.image.function.as_ref(),
            location.image.symbol.as_ref(),
        );
        let source = match &location.image.source {
            Some(source) => self
                .debugger
                .loaded_module_image(location.module)
                .await?
                .source_file(source.file)
                .map(|file| renderer.location(&file.path, source.line)),
            None => None,
        };
        if let Some(source) = source {
            return Ok(format!(
                "{} at {} ({})",
                renderer.paint(Role::Name, name),
                renderer.paint(Role::Metadata, source),
                renderer.paint(Role::Metadata, location.address)
            ));
        }
        let modules = self.debugger.loaded_modules().await?;
        Ok(format!(
            "{} at {} from {}",
            renderer.paint(Role::Name, name),
            renderer.paint(Role::Metadata, location.address),
            renderer.paint(
                Role::Metadata,
                format::module_name(&modules, location.module)
                    .unwrap_or_else(|| "<unknown module>".to_owned())
            )
        ))
    }

    /// The selected frame's source, with as many lines around its line as
    /// `[source] context` asks for.
    async fn source_context(&self) -> uscope::Result<uscope::SourceContext> {
        let [before, after] = self.settings.config.source.context;
        let mut context = self.debugger.source_context(before.max(after)).await?;
        let line = context.location.line.get();
        let first = line.saturating_sub(u64::from(before));
        let last = line.saturating_add(u64::from(after));
        context.lines = context
            .lines
            .iter()
            .filter(|source| (first..=last).contains(&source.number.get()))
            .cloned()
            .collect();
        Ok(context)
    }

    async fn backtrace(&self) -> Result<String> {
        let selected = self.selected_level().await?;
        let trace = self.debugger.backtrace().await?;
        let modules = if trace
            .frames
            .iter()
            .any(|frame| frame.module.is_some() && frame.source.is_none())
        {
            Some(self.debugger.loaded_modules().await?)
        } else {
            None
        };
        // Source files are identified within their owning module's image.
        let mut images = BTreeMap::new();
        let modules_with_source = trace
            .frames
            .iter()
            .filter(|frame| frame.source.is_some())
            .filter_map(|frame| frame.module)
            .collect::<std::collections::BTreeSet<_>>();
        for module in modules_with_source {
            images.insert(module, self.debugger.loaded_module_image(module).await?);
        }
        Ok(format::backtrace(
            &trace,
            selected,
            modules.as_ref(),
            &images,
            self.renderers.stdout,
        ))
    }

    /// Formats a stop, adding watched values and surrounding source where
    /// they help explain it.
    async fn stop_with_source(&self, reason: &StopReason) -> String {
        let renderer = self.renderers.stdout;
        let mut output = match reason {
            StopReason::Watchpoint { hits } => {
                let watchpoints = match self.debugger.snapshot().await {
                    Ok(snapshot) => snapshot.watchpoints,
                    Err(error) => {
                        self.warn(&format!("watchpoint details unavailable: {error}"));
                        std::sync::Arc::default()
                    }
                };
                format::watchpoint_hits(
                    hits,
                    &watchpoints,
                    Some(self.debugger.module_image()),
                    renderer,
                )
            }
            StopReason::Breakpoint { hits, .. } => {
                let mut output = format::stop(reason, renderer);
                // A stop deletes the temporary breakpoints it hit before it
                // is published, so the hits missing now were temporaries.
                if let Ok(snapshot) = self.debugger.snapshot().await {
                    let deleted = hits
                        .iter()
                        .filter(|hit| {
                            !snapshot
                                .breakpoints
                                .iter()
                                .any(|breakpoint| breakpoint.id == hit.breakpoint)
                        })
                        .map(|hit| CountedId::Breakpoint(hit.breakpoint))
                        .collect::<Vec<_>>();
                    if !deleted.is_empty() {
                        write!(
                            output,
                            "\n{} temporary {}",
                            renderer.paint(Role::Success, "deleted"),
                            describe_ids(&deleted, renderer)
                        )
                        .expect("writing to a String cannot fail");
                    }
                }
                output
            }
            _ => format::stop(reason, renderer),
        };
        if matches!(
            reason,
            StopReason::Breakpoint { .. }
                | StopReason::Step { .. }
                | StopReason::StepIncomplete { .. }
                | StopReason::Watchpoint { .. }
        ) {
            match self.source_context().await {
                Ok(context) => {
                    output.push('\n');
                    output.push_str(&format::source_context(&context, renderer));
                }
                Err(error) => {
                    output = format!(
                        "{output}\n{}: {error}",
                        renderer.paint(Role::Warning, "source unavailable")
                    );
                }
            }
        }
        output
    }
}

/// Which frame a frame command selects.
#[derive(Clone, Copy)]
enum FrameTarget {
    /// The frame already selected.
    Selected,
    /// The frame at a backtrace level.
    Level(u64),
    /// A frame this many levels toward the callers, or the outermost.
    Outward(u64),
    /// A frame this many levels toward the stop, or the innermost.
    Inward(u64),
}

/// Parses `frame [level]`, `up [count]`, or `down [count]`.
fn parse_frame_target(spec: &CommandSpec, argument: Option<&str>) -> Result<FrameTarget> {
    let number = argument
        .map(|value| value.parse::<u64>().map_err(|_| spec.usage_error()))
        .transpose()?;
    Ok(match (spec.command, number) {
        (Command::Frame, None) => FrameTarget::Selected,
        (Command::Frame, Some(level)) => FrameTarget::Level(level),
        (Command::Up, count) => FrameTarget::Outward(count.unwrap_or(1)),
        (Command::Down, count) => FrameTarget::Inward(count.unwrap_or(1)),
        _ => unreachable!("only frame commands select frames"),
    })
}

/// Parses a `0x`-prefixed hexadecimal address. The prefix keeps addresses
/// distinct from names made of hexadecimal digits, such as `add`.
fn parse_address(value: &str) -> Result<VirtualAddress> {
    value
        .strip_prefix("0x")
        .and_then(|digits| u64::from_str_radix(digits, 16).ok())
        .map(VirtualAddress::new)
        .ok_or_else(|| anyhow!("invalid address '{value}'; addresses are 0x-prefixed hexadecimal"))
}

/// Parses a decimal or `0x`-prefixed hexadecimal count.
fn parse_count(value: &str) -> Option<u64> {
    value.strip_prefix("0x").map_or_else(
        || value.parse().ok(),
        |digits| u64::from_str_radix(digits, 16).ok(),
    )
}

fn parse_memory_byte_count(value: Option<&str>, spec: &CommandSpec) -> Result<u64> {
    let Some(value) = value else {
        return Ok(DEFAULT_HEX_DUMP_BYTES);
    };
    parse_count(value)
        .filter(|count| (1..=MAX_HEX_DUMP_BYTES).contains(count))
        .ok_or_else(|| spec.usage_error())
}

/// A breakpoint, or a watchpoint written `w` and its id, whose hits
/// `condition`, `hits`, and `ignore` choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CountedId {
    Breakpoint(BreakpointId),
    Watchpoint(WatchpointId),
}

impl CountedId {
    fn describe(self, renderer: Renderer) -> String {
        match self {
            Self::Breakpoint(id) => format!("breakpoint {}", renderer.paint(Role::Metadata, id)),
            Self::Watchpoint(id) => format!("watchpoint {}", renderer.paint(Role::Metadata, id)),
        }
    }
}

fn parse_counted_id(argument: &str, spec: &CommandSpec) -> Result<CountedId> {
    argument
        .strip_prefix('w')
        .map_or_else(
            || {
                argument
                    .parse()
                    .map(|id| CountedId::Breakpoint(BreakpointId::new(id)))
            },
            |digits| {
                digits
                    .parse()
                    .map(|id| CountedId::Watchpoint(WatchpointId::new(id)))
            },
        )
        .map_err(|_| spec.usage_error())
}

/// A line of the selected frame's file, as `break` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameLine {
    Number(u64),
    Offset(i64),
}

/// Parses `42`, `+3`, or `-3` as a line of the selected frame's file.
fn frame_line(written: &str) -> Result<Option<FrameLine>> {
    if let Some(offset) = written.strip_prefix(['+', '-']) {
        let lines = offset
            .parse::<i64>()
            .ok()
            .filter(|_| offset.bytes().all(|byte| byte.is_ascii_digit()))
            .ok_or_else(|| anyhow!("invalid line offset '{written}'"))?;
        return Ok(Some(FrameLine::Offset(if written.starts_with('-') {
            -lines
        } else {
            lines
        })));
    }
    if !written.is_empty() && written.bytes().all(|byte| byte.is_ascii_digit()) {
        let line = written
            .parse()
            .map_err(|_| anyhow!("invalid line number '{written}'"))?;
        if line == 0 {
            bail!("source line numbers are one-based");
        }
        return Ok(Some(FrameLine::Number(line)));
    }
    Ok(None)
}

/// A `break` line: its location, and the options written after it.
#[derive(Debug, Default, PartialEq, Eq)]
struct BreakLine<'a> {
    location: Option<&'a str>,
    condition: Option<&'a str>,
    hits: Option<&'a str>,
    log: Option<String>,
}

const BREAK_OPTIONS: [&str; 3] = ["if", "hits", "log"];

/// Splits a `break` line into its location and its options, each of which
/// takes the text up to the next option word outside a string or brackets.
/// A word after the location that is no option is a hit condition, as
/// `break counted ==3` wrote it before options had names.
fn parse_break<'a>(line: &'a str, spec: &CommandSpec) -> Result<BreakLine<'a>> {
    let words = option_words(line);
    let starts = words
        .iter()
        .enumerate()
        .filter(|(_, (_, word))| BREAK_OPTIONS.contains(word))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let first = starts.first().copied().unwrap_or(words.len());
    let mut parsed = BreakLine::default();
    match &words[..first] {
        [] => {}
        [(_, location)] => parsed.location = Some(location),
        [(_, location), (_, hits)] => {
            parsed.location = Some(location);
            parsed.hits = Some(hits);
        }
        _ => return Err(spec.usage_error()),
    }
    for (index, &start) in starts.iter().enumerate() {
        let (offset, keyword) = words[start];
        let end = starts
            .get(index + 1)
            .map_or(line.len(), |&next| words[next].0);
        let text = line[offset + keyword.len()..end].trim();
        if text.is_empty() {
            return Err(spec.usage_error());
        }
        let twice = || anyhow!("`{keyword}` is given twice");
        match keyword {
            "if" if parsed.condition.is_none() => parsed.condition = Some(text),
            "hits" if parsed.hits.is_none() => parsed.hits = Some(text),
            "log" if parsed.log.is_none() => parsed.log = Some(unquoted(text)?),
            _ => return Err(twice()),
        }
    }
    Ok(parsed)
}

/// The words of a line outside strings and brackets, with their offsets;
/// a string or bracketed text is part of the word around it.
fn option_words(line: &str) -> Vec<(usize, &str)> {
    let mut words = Vec::new();
    let (mut depth, mut quoted, mut escaped) = (0_usize, false, false);
    let mut start = None;
    for (offset, character) in line.char_indices() {
        if quoted {
            match character {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => quoted = false,
                _ => {}
            }
            continue;
        }
        if character.is_whitespace() && depth == 0 {
            if let Some(begun) = start.take() {
                words.push((begun, &line[begun..offset]));
            }
            continue;
        }
        start.get_or_insert(offset);
        match character {
            '"' => quoted = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if let Some(begun) = start {
        words.push((begun, &line[begun..]));
    }
    words
}

/// A log message written in quotes, with `\"` and `\\` escaped, or as it is.
fn unquoted(text: &str) -> Result<String> {
    let Some(inner) = text.strip_prefix('"') else {
        return Ok(text.to_owned());
    };
    let mut message = String::new();
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        match character {
            '"' if characters.as_str().trim().is_empty() => return Ok(message),
            '"' => bail!("a log message in quotes ends at its closing quote"),
            '\\' => match characters.next() {
                Some(escaped @ ('"' | '\\')) => message.push(escaped),
                Some(other) => {
                    message.push('\\');
                    message.push(other);
                }
                None => break,
            },
            other => message.push(other),
        }
    }
    bail!("a log message in quotes needs its closing quote")
}

/// Parses breakpoint and watchpoint ids, ranges of them such as `3-5` or
/// `w1-2`, or `all` as `None`.
fn parse_ids(words: &[&str], watches: bool, spec: &CommandSpec) -> Result<Option<Vec<CountedId>>> {
    if words == ["all"] {
        return Ok(None);
    }
    let mut ids = Vec::new();
    for word in words {
        let (watch, digits) = word
            .strip_prefix('w')
            .map_or((watches, *word), |digits| (true, digits));
        let (first, last) = digits.split_once('-').unwrap_or((digits, digits));
        let parse = |digits: &str| digits.parse::<u64>().map_err(|_| spec.usage_error());
        let (first, last) = (parse(first)?, parse(last)?);
        if first > last {
            bail!("range {word} is empty");
        }
        for id in first..=last {
            let id = if watch {
                CountedId::Watchpoint(WatchpointId::new(id))
            } else {
                CountedId::Breakpoint(BreakpointId::new(id))
            };
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    Ok(Some(ids))
}

/// `breakpoints 1, 3 and watchpoint 2`.
fn describe_ids(ids: &[CountedId], renderer: Renderer) -> String {
    let list = |kind: &str, ids: Vec<u64>| -> Option<String> {
        if ids.is_empty() {
            return None;
        }
        let numbers = ids
            .iter()
            .map(|id| renderer.paint(Role::Metadata, id).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "{kind}{} {numbers}",
            if ids.len() == 1 { "" } else { "s" }
        ))
    };
    let breakpoints = ids
        .iter()
        .filter_map(|id| match id {
            CountedId::Breakpoint(id) => Some(id.get()),
            CountedId::Watchpoint(_) => None,
        })
        .collect();
    let watchpoints = ids
        .iter()
        .filter_map(|id| match id {
            CountedId::Watchpoint(id) => Some(id.get()),
            CountedId::Breakpoint(_) => None,
        })
        .collect();
    [
        list("breakpoint", breakpoints),
        list("watchpoint", watchpoints),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" and ")
}

/// Parses `0xaddress:byte-count`. Arguments without the `0x` prefix are value
/// expressions, which may contain `::` qualifiers.
fn parse_watch_location(argument: &str, spec: &CommandSpec) -> Result<Option<WatchpointSpec>> {
    if !argument.starts_with("0x") {
        return Ok(None);
    }
    let (address, byte_count) = argument.split_once(':').ok_or_else(|| spec.usage_error())?;
    Ok(Some(WatchpointSpec::Location {
        address: parse_address(address).map_err(|_| spec.usage_error())?,
        byte_size: parse_count(byte_count).ok_or_else(|| spec.usage_error())?,
    }))
}

/// Parses a breakpoint location: a function, `0xaddress`, `file:line`, or
/// `file:function`, or `None` when a file or its location is missing. The
/// `::` of a qualified name such as `ns::run` never separates a file.
pub fn parse_breakpoint_location(argument: &str) -> Result<Option<BreakpointSpec>> {
    if argument.starts_with("0x") {
        return Ok(Some(BreakpointSpec::Address(parse_address(argument)?)));
    }
    let bytes = argument.as_bytes();
    let separator = (0..bytes.len()).rev().find(|&index| {
        bytes[index] == b':'
            && bytes.get(index + 1) != Some(&b':')
            && (index == 0 || bytes[index - 1] != b':')
    });
    let Some(separator) = separator else {
        return Ok(Some(BreakpointSpec::Function(argument.to_owned())));
    };
    let (path, location) = (&argument[..separator], &argument[separator + 1..]);
    if path.is_empty() || location.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(path);
    Ok(Some(match location.parse::<u64>() {
        Ok(line) => BreakpointSpec::Source {
            path,
            line: LineNumber::new(line)
                .ok_or_else(|| anyhow!("source line numbers are one-based"))?,
        },
        Err(_) => BreakpointSpec::FileFunction {
            path,
            function: location.to_owned(),
        },
    }))
}

/// Joins two parts of a command's output, either of which may be empty.
fn join_lines(first: &str, second: &str) -> String {
    match (first.is_empty(), second.is_empty()) {
        (true, _) => second.to_owned(),
        (false, true) => first.to_owned(),
        (false, false) => format!("{first}\n{second}"),
    }
}

impl Cli {
    async fn list_signals(&self) -> Result<String> {
        let mut policies = Vec::new();
        for code in uscope::signal_codes() {
            policies.push((code, self.debugger.signal_policy(code).await?));
        }
        Ok(format::signal_policies(&policies, self.renderers.stdout))
    }

    /// Shows or changes one signal's policy with gdb's actions: `stop`
    /// implies `print`, and `noprint` implies `nostop`.
    async fn handle_signal(&self, arguments: &[&str]) -> Result<String> {
        let name = arguments[0];
        let code = uscope::signal_named(name).ok_or_else(|| anyhow!("unknown signal '{name}'"))?;
        let mut policy = self.debugger.signal_policy(code).await?;
        for action in &arguments[1..] {
            apply_signal_action(&mut policy, action)?;
        }
        if arguments.len() > 1 {
            self.debugger.set_signal_policy(code, policy).await?;
        }
        Ok(format::signal_policies(
            &[(code, policy)],
            self.renderers.stdout,
        ))
    }
}

/// Changes one aspect of a signal policy as gdb's `handle` does: stopping
/// implies printing, and not printing implies not stopping.
pub fn apply_signal_action(policy: &mut SignalPolicy, action: &str) -> Result<()> {
    match action {
        "stop" => {
            policy.stop = true;
            policy.print = true;
        }
        "nostop" => policy.stop = false,
        "print" => policy.print = true,
        "noprint" => {
            policy.print = false;
            policy.stop = false;
        }
        "pass" | "noignore" => policy.pass = true,
        "nopass" | "ignore" => policy.pass = false,
        other => bail!(
            "unknown signal action '{other}'; use stop, nostop, print, noprint, pass, or nopass"
        ),
    }
    Ok(())
}

fn parse_expression(text: &str) -> Result<uscope::Expression> {
    uscope::Expression::parse(text).map_err(|error| anyhow!(format::expression_error(text, &error)))
}

/// Splits a command line into its command, the format written after it as
/// in `print/x`, the rest of the line as written, and its words.
fn command_line(line: &str) -> Result<(&'static CommandSpec, &str, &str, Vec<&str>)> {
    let mut words = line.split_whitespace();
    let written = words.next().unwrap_or_default();
    let (entered, format) = written.split_once('/').unwrap_or((written, ""));
    let rest = line.trim_start()[written.len()..].trim();
    let spec = command_named(entered).ok_or_else(|| {
        let names = COMMANDS
            .iter()
            .flat_map(|spec| std::iter::once(spec.name).chain(spec.aliases.iter().copied()));
        let hint = super::suggest::did_you_mean(entered, names)
            .unwrap_or_else(|| "type `help` for a list".to_owned());
        anyhow!("unknown command '{entered}'; {hint}")
    })?;
    let arguments = words.collect::<Vec<_>>();
    if !format.is_empty() && (spec.command != Command::Print || !matches!(format, "x" | "r")) {
        bail!("unknown format '/{format}'; print takes /x or /r");
    }
    let (minimum, maximum) = spec.arity();
    if !(minimum..=maximum).contains(&arguments.len()) {
        return Err(spec.usage_error());
    }
    Ok((spec, format, rest, arguments))
}

/// An evaluation's failure, pointing into the expression when it is the
/// expression's.
fn expression_error(text: &str, error: uscope::Error) -> anyhow::Error {
    match error {
        uscope::Error::Expression(error) => anyhow!(format::expression_error(text, &error)),
        error => error.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(command: Command) -> &'static CommandSpec {
        COMMANDS
            .iter()
            .find(|spec| spec.command == command)
            .expect("registered command")
    }

    #[test]
    fn command_registry_has_unique_names_and_derives_arity_from_usage() {
        let mut names = std::collections::BTreeSet::new();
        for command in COMMANDS {
            for name in std::iter::once(&command.name).chain(command.aliases) {
                assert!(names.insert(*name), "duplicate command name {name}");
                assert_eq!(
                    command_named(name).map(|found| found.name),
                    Some(command.name)
                );
            }
            assert!(
                command
                    .usage
                    .strip_prefix(command.name)
                    .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(' ')),
                "usage must begin with command name: {}",
                command.usage
            );
        }
        assert_eq!(spec(Command::Run).arity(), (0, 0));
        assert_eq!(spec(Command::Break).arity(), (0, usize::MAX));
        assert_eq!(spec(Command::Info).arity(), (1, usize::MAX));
        assert_eq!(spec(Command::Print).arity(), (0, usize::MAX));
        assert_eq!(spec(Command::Whatis).arity(), (1, usize::MAX));
        assert_eq!(spec(Command::Examine).arity(), (1, 2));
        assert_eq!(spec(Command::Disassemble).arity(), (0, 2));
        assert!(spec(Command::Next).repeatable);
        assert!(!spec(Command::Watch).repeatable);
        assert_eq!(spec(Command::Frame).arity(), (0, 1));
        assert!(spec(Command::Up).repeatable && spec(Command::Down).repeatable);
        assert!(!spec(Command::Frame).repeatable);
    }

    /// Option words inside strings and brackets belong to the text around
    /// them, and the legacy trailing hit condition still parses.
    #[test]
    fn break_options_end_at_option_words_outside_strings_and_brackets() {
        let spec = spec(Command::Break);
        let parsed = parse_break(
            r#"parse.c:12 if name == "log" && (a log b) hits >=2 log "said \"{name}\"""#,
            spec,
        )
        .expect("parses");
        assert_eq!(
            parsed,
            BreakLine {
                location: Some("parse.c:12"),
                condition: Some(r#"name == "log" && (a log b)"#),
                hits: Some(">=2"),
                log: Some(r#"said "{name}""#.to_owned()),
            }
        );
        assert_eq!(
            parse_break("counted ==3", spec).expect("parses").hits,
            Some("==3")
        );
        assert_eq!(parse_break("if x", spec).expect("parses").location, None);
        assert!(parse_break("f if x if y", spec).is_err());
        assert!(parse_break("f g h", spec).is_err());
        assert!(parse_break(r#"f log "open"#, spec).is_err());
    }

    #[test]
    fn addresses_require_a_hexadecimal_prefix() {
        let parse = |argument| parse_breakpoint_location(argument).ok().flatten();
        assert_eq!(
            parse("0x10").expect("address"),
            BreakpointSpec::Address(VirtualAddress::new(0x10))
        );
        for name in ["add", "face", "f", "42"] {
            assert_eq!(
                parse(name).expect("function"),
                BreakpointSpec::Function(name.to_owned())
            );
        }
        assert!(parse("0xzz").is_none());
        assert!(matches!(
            parse("main.c:12").expect("source"),
            BreakpointSpec::Source { line, .. } if line.get() == 12
        ));
        assert!(matches!(
            parse("main.c:helper").expect("file function"),
            BreakpointSpec::FileFunction { function, .. } if function == "helper"
        ));
        assert!(parse("main.c:0").is_none());
        assert!(parse(":12").is_none());
        // Qualified names are functions, also within a file.
        assert_eq!(
            parse("ns::Type::run").expect("qualified function"),
            BreakpointSpec::Function("ns::Type::run".to_owned())
        );
        assert!(matches!(
            parse("main.cpp:ns::run").expect("qualified file function"),
            BreakpointSpec::FileFunction { path, function }
                if path == std::path::Path::new("main.cpp") && function == "ns::run"
        ));
    }

    #[test]
    fn watch_locations_need_a_prefix_and_leave_qualified_names_to_expressions() {
        let spec = spec(Command::Watch);
        assert_eq!(
            parse_watch_location("0x10:8", spec).expect("location"),
            Some(WatchpointSpec::Location {
                address: VirtualAddress::new(0x10),
                byte_size: 8
            })
        );
        for expression in ["counter", "one.c::duplicate", "ns::value"] {
            assert_eq!(parse_watch_location(expression, spec).expect("name"), None);
        }
        for invalid in ["0x10:", "0x10", "0x10:-1", "0xzz:8", "0x10:many"] {
            assert!(parse_watch_location(invalid, spec).is_err(), "{invalid}");
        }
    }

    #[test]
    fn memory_byte_counts_are_bounded_and_accept_decimal_or_hexadecimal() {
        let spec = spec(Command::Examine);
        assert_eq!(
            parse_memory_byte_count(None, spec).expect("default count"),
            DEFAULT_HEX_DUMP_BYTES
        );
        assert_eq!(
            parse_memory_byte_count(Some("128"), spec).expect("decimal"),
            128
        );
        assert_eq!(
            parse_memory_byte_count(Some("0x80"), spec).expect("hex"),
            128
        );
        for count in ["0", "0x0", "8193", "0x2001", "invalid"] {
            assert!(
                parse_memory_byte_count(Some(count), spec).is_err(),
                "accepted invalid byte count {count:?}"
            );
        }
    }
}
