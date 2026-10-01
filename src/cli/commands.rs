//! The command table, argument parsing, and command handlers.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use uscope::{
    BreakpointId, BreakpointSpec, ByteOrder, Disassembly, DisassemblyQuery, DisassemblyRange,
    HitComparison, HitCondition, LineNumber, MAX_WINDOW_AFTER, RegisterRole, StackFrameId,
    StepKind, ThreadId, VirtualAddress, WatchAccess, WatchpointId, WatchpointSpec,
};

use super::format::{self, plural};
use super::terminal::Role;
use super::value;
use super::{Cli, Control};

const DEFAULT_HEX_DUMP_BYTES: u64 = 64;
pub const MAX_HEX_DUMP_BYTES: u64 = 8 * 1024;
const SOURCE_CONTEXT_RADIUS: u32 = 3;
/// Instructions shown before and from a stop that no function contains.
const DISASSEMBLY_CONTEXT_BEFORE: u32 = 8;
const DISASSEMBLY_CONTEXT_AFTER: u32 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Break,
    Breakpoints,
    Info,
    Delete,
    Ignore,
    Hits,
    Watch,
    AccessWatch,
    ReadWatch,
    Watchpoints,
    Unwatch,
    Run,
    Continue,
    Print,
    Globals,
    Stepi,
    Step,
    Next,
    Finish,
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
    /// Returns the inclusive range of argument counts the usage accepts.
    fn arity(&self) -> (usize, usize) {
        self.usage
            .split_whitespace()
            .skip(1)
            .fold((0, 0), |(minimum, maximum), word| {
                if word.starts_with('[') {
                    (minimum, maximum + 1)
                } else {
                    (minimum + 1, maximum + 1)
                }
            })
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

pub const COMMANDS: &[CommandSpec] = &[
    command!(
        Break,
        "break",
        ["b"],
        "break <function|0xaddress|file:line|file:function> [hit-condition]",
        "Set a breakpoint, optionally stopping only at hits such as >=5, ==3, or %10"
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
        "info breakpoints|watchpoints|core|symbol [0xaddress]",
        "Show debugger information, or the symbol and section containing an address"
    ),
    command!(
        Delete,
        "delete",
        ["del", "d"],
        "delete <id|all>",
        "Delete logical breakpoints"
    ),
    command!(
        Ignore,
        "ignore",
        [],
        "ignore <id> <count>",
        "Skip a breakpoint's next count hits, then stop at every hit; 0 stops at the next"
    ),
    command!(
        Hits,
        "hits",
        [],
        "hits <id> <hit-condition|always>",
        "Choose which hits of a breakpoint stop, such as >=5, ==3, or %10"
    ),
    command!(
        Watch,
        "watch",
        [],
        "watch <value-path|0xaddress:byte-count>",
        "Stop when watched memory is written"
    ),
    command!(
        AccessWatch,
        "awatch",
        [],
        "awatch <value-path|0xaddress:byte-count>",
        "Stop when watched memory is read or written"
    ),
    command!(
        ReadWatch,
        "rwatch",
        [],
        "rwatch <value-path|0xaddress:byte-count>",
        "Stop when watched memory is read"
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
        "unwatch <id|all>",
        "Delete watchpoints"
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
        "print [value-path]",
        "Print variables, indexed values, members, or one bounded range"
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

impl Cli {
    /// Parses and executes one non-empty command line.
    pub(super) async fn execute(&self, line: &str) -> Result<Control> {
        let mut words = line.split_whitespace();
        let entered = words.next().unwrap_or_default();
        let spec = command_named(entered)
            .ok_or_else(|| anyhow!("unknown command '{entered}'; type `help` for a list"))?;
        let arguments = words.collect::<Vec<_>>();
        let (minimum, maximum) = spec.arity();
        if !(minimum..=maximum).contains(&arguments.len()) {
            return Err(spec.usage_error());
        }
        let first = arguments.first().copied();
        let renderer = self.renderers.stdout;
        let debugger = &self.debugger;

        let output = match spec.command {
            Command::Break => {
                self.add_breakpoint(arguments[0], arguments.get(1).copied(), spec)
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
                _ => return Err(spec.usage_error()),
            },
            Command::Delete => self.delete_breakpoints(arguments[0], spec).await?,
            Command::Ignore => self.ignore(arguments[0], arguments[1], spec).await?,
            Command::Hits => self.hits(arguments[0], arguments[1], spec).await?,
            Command::Watch => self.watch(arguments[0], WatchAccess::Write, spec).await?,
            Command::AccessWatch => {
                self.watch(arguments[0], WatchAccess::ReadWrite, spec)
                    .await?
            }
            Command::ReadWatch => self.watch(arguments[0], WatchAccess::Read, spec).await?,
            Command::Watchpoints => self.list_watchpoints().await?,
            Command::Unwatch => self.delete_watchpoints(arguments[0], spec).await?,
            Command::Run => {
                let reason = debugger.run_with(self.launch.options()).await?;
                self.stop_with_source(&reason).await
            }
            Command::Continue => {
                let reason = debugger
                    .resume_with_exception(self.resume_disposition().await?)
                    .await?;
                self.stop_with_source(&reason).await
            }
            Command::Print => match first {
                Some(expression) => self.print(expression).await?,
                None => value::variables(&debugger.variables().await?, renderer),
            },
            Command::Globals => self.globals(first).await?,
            Command::Stepi => self.step(StepKind::Instruction).await?,
            Command::Step => self.step(StepKind::IntoSource).await?,
            Command::Next => self.step(StepKind::OverSource).await?,
            Command::Finish => self.step(StepKind::Out).await?,
            Command::Examine => {
                let address = parse_address(arguments[0])?;
                let byte_count = parse_memory_byte_count(arguments.get(1).copied(), spec)?;
                format::memory_read(&debugger.read_memory(address, byte_count).await?, renderer)
            }
            Command::Disassemble => self.disassemble(first, arguments.get(1).copied()).await?,
            Command::Address => self.address(arguments[0]).await?,
            Command::Where => self.location().await?,
            Command::List => format::source_context(
                &debugger.source_context(SOURCE_CONTEXT_RADIUS).await?,
                renderer,
            ),
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
                None => format::help(renderer),
            },
            Command::Quit => return Ok(Control::Quit),
        };
        Ok(Control::Continue(output))
    }

    async fn address(&self, symbol: &str) -> Result<String> {
        let renderer = self.renderers.stdout;
        Ok(format!(
            "{}: {}",
            renderer.paint(Role::Name, symbol),
            renderer.paint(Role::Metadata, self.debugger.runtime_address(symbol).await?)
        ))
    }

    async fn delete_breakpoints(&self, argument: &str, spec: &CommandSpec) -> Result<String> {
        let what = match parse_id_or_all(argument, spec)? {
            None => plural(
                self.debugger.remove_all_breakpoints().await?.len() as u64,
                "breakpoint",
            ),
            Some(id) => {
                let removed = self
                    .debugger
                    .remove_breakpoint(BreakpointId::new(id))
                    .await?;
                format!("breakpoint {}", removed.id)
            }
        };
        Ok(format::deleted(&what, self.renderers.stdout))
    }

    async fn add_breakpoint(
        &self,
        location: &str,
        condition: Option<&str>,
        spec: &CommandSpec,
    ) -> Result<String> {
        let location = parse_breakpoint_spec(location, spec)?;
        let breakpoint = match condition {
            Some(condition) => {
                self.debugger
                    .add_breakpoint_with_hit_condition(location, condition.parse()?)
                    .await?
            }
            None => self.debugger.add_breakpoint(location).await?,
        };
        Ok(format::breakpoint(&breakpoint, self.renderers.stdout))
    }

    async fn hits(&self, id: &str, condition: &str, spec: &CommandSpec) -> Result<String> {
        let id = parse_breakpoint_id(id, spec)?;
        let condition = match condition {
            "always" => None,
            condition => Some(condition.parse()?),
        };
        Ok(format::breakpoint_hit_condition(
            &self
                .debugger
                .set_breakpoint_hit_condition(id, condition)
                .await?,
            self.renderers.stdout,
        ))
    }

    /// Skips a breakpoint's next `count` hits like gdb's `ignore`: the hit
    /// condition becomes `>=` the hit after them.
    async fn ignore(&self, id: &str, count: &str, spec: &CommandSpec) -> Result<String> {
        let id = parse_breakpoint_id(id, spec)?;
        let count = parse_count(count).ok_or_else(|| spec.usage_error())?;
        let condition = if count == 0 {
            None
        } else {
            let hits = self
                .debugger
                .snapshot()
                .await?
                .breakpoints
                .iter()
                .find(|breakpoint| breakpoint.id == id)
                .ok_or_else(|| anyhow!("breakpoint {id} was not found"))?
                .hit_count;
            let first_stop = hits
                .checked_add(count)
                .and_then(|skipped| skipped.checked_add(1))
                .ok_or_else(|| anyhow!("ignore count {count} is too large"))?;
            Some(HitCondition::new(
                HitComparison::GreaterOrEqual,
                first_stop,
            )?)
        };
        let breakpoint = self
            .debugger
            .set_breakpoint_hit_condition(id, condition)
            .await?;
        let renderer = self.renderers.stdout;
        Ok(if count == 0 {
            format!(
                "breakpoint {} stops at its next hit",
                renderer.paint(Role::Metadata, breakpoint.id)
            )
        } else {
            format!(
                "breakpoint {} ignores its next {}",
                renderer.paint(Role::Metadata, breakpoint.id),
                plural(count, "hit")
            )
        })
    }

    async fn delete_watchpoints(&self, argument: &str, spec: &CommandSpec) -> Result<String> {
        let what = match parse_id_or_all(argument, spec)? {
            None => plural(
                self.debugger.remove_all_watchpoints().await?.len() as u64,
                "watchpoint",
            ),
            Some(id) => {
                let removed = self
                    .debugger
                    .remove_watchpoint(WatchpointId::new(id))
                    .await?;
                format!("watchpoint {}", removed.id)
            }
        };
        Ok(format::deleted(&what, self.renderers.stdout))
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
        let reason = self
            .debugger
            .step_with_exception(kind, self.resume_disposition().await?)
            .await?;
        Ok(self.stop_with_source(&reason).await)
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

    async fn watch(
        &self,
        argument: &str,
        access: WatchAccess,
        spec: &CommandSpec,
    ) -> Result<String> {
        if !self
            .debugger
            .watchpoint_capabilities()
            .access
            .contains(&access)
        {
            return Err(uscope::Error::UnsupportedWatchAccess(access).into());
        }
        let watchpoint = if let Some(location) = parse_watch_location(argument, spec)? {
            self.debugger.add_watchpoint(location, access).await?
        } else {
            let parsed = uscope::parse_value_expression(argument)?;
            if parsed.range.is_some() {
                bail!("cannot watch a range; watch one value or 0xaddress:byte-count");
            }
            self.debugger.watch(parsed.expression, access).await?
        };
        Ok(format::watchpoint_set(&watchpoint, self.renderers.stdout))
    }

    async fn print(&self, expression: &str) -> Result<String> {
        let renderer = self.renderers.stdout;
        let parsed = uscope::parse_value_expression(expression)?;
        if let Some(range) = parsed.range {
            let page = self
                .debugger
                .inspect_range(parsed.expression, range)
                .await?;
            return Ok(value::range(expression, &page, renderer));
        }
        let inspected = self.debugger.inspect(parsed.expression).await?;
        Ok(match &inspected.type_info {
            Some(type_info) => {
                value::expanded(
                    &self.debugger,
                    type_info,
                    expression,
                    &inspected.state,
                    uscope::InspectionLimits::default().remaining_after(inspected.usage),
                    renderer,
                )
                .await?
            }
            None => value::untyped(expression, &inspected.state, renderer),
        })
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
            // Code outside every function, such as a stop in the vDSO, is
            // shown around the frame's instruction instead.
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
            .and_then(|value| <[u8; 8]>::try_from(value.bytes.as_ref()).ok())
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
            match self.debugger.source_context(SOURCE_CONTEXT_RADIUS).await {
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
            // the vDSO.
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
                .map(|file| format!("{}:{}", file.path.display(), source.line)),
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
        let mut images = std::collections::BTreeMap::new();
        for module in trace
            .frames
            .iter()
            .filter(|frame| frame.source.is_some())
            .filter_map(|frame| frame.module)
        {
            if let std::collections::btree_map::Entry::Vacant(entry) = images.entry(module) {
                entry.insert(self.debugger.loaded_module_image(module).await?);
            }
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
    pub(super) async fn stop_with_source(&self, reason: &uscope::StopReason) -> String {
        let renderer = self.renderers.stdout;
        let mut output = match reason {
            uscope::StopReason::Watchpoint { hits } => {
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
            _ => format::stop(reason, renderer),
        };
        if matches!(
            reason,
            uscope::StopReason::Breakpoint { .. }
                | uscope::StopReason::Step { .. }
                | uscope::StopReason::Watchpoint { .. }
        ) {
            match self.debugger.source_context(SOURCE_CONTEXT_RADIUS).await {
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

fn parse_breakpoint_id(argument: &str, spec: &CommandSpec) -> Result<BreakpointId> {
    argument
        .parse()
        .map(BreakpointId::new)
        .map_err(|_| spec.usage_error())
}

/// Parses `all` as `None` or a numeric identifier.
fn parse_id_or_all(argument: &str, spec: &CommandSpec) -> Result<Option<u64>> {
    if argument == "all" {
        return Ok(None);
    }
    argument.parse().map(Some).map_err(|_| spec.usage_error())
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

fn parse_breakpoint_spec(argument: &str, spec: &CommandSpec) -> Result<BreakpointSpec> {
    if argument.starts_with("0x") {
        return Ok(BreakpointSpec::Address(parse_address(argument)?));
    }
    let Some((path, location)) = argument.rsplit_once(':') else {
        return Ok(BreakpointSpec::Function(argument.to_owned()));
    };
    if path.is_empty() || location.is_empty() {
        return Err(spec.usage_error());
    }
    let path = PathBuf::from(path);
    Ok(match location.parse::<u64>() {
        Ok(line) => BreakpointSpec::Source {
            path,
            line: LineNumber::new(line)
                .ok_or_else(|| anyhow!("source line numbers are one-based"))?,
        },
        Err(_) => BreakpointSpec::FileFunction {
            path,
            function: location.to_owned(),
        },
    })
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
        assert_eq!(spec(Command::Break).arity(), (1, 2));
        assert_eq!(spec(Command::Info).arity(), (1, 2));
        assert_eq!(spec(Command::Print).arity(), (0, 1));
        assert_eq!(spec(Command::Examine).arity(), (1, 2));
        assert_eq!(spec(Command::Disassemble).arity(), (0, 2));
        assert!(spec(Command::Next).repeatable);
        assert!(!spec(Command::Watch).repeatable);
        assert_eq!(spec(Command::Frame).arity(), (0, 1));
        assert!(spec(Command::Up).repeatable && spec(Command::Down).repeatable);
        assert!(!spec(Command::Frame).repeatable);
    }

    #[test]
    fn frame_commands_select_absolutely_or_relatively() {
        let parse = |command, argument| parse_frame_target(spec(command), argument).map_err(|_| ());
        assert_eq!(parse(Command::Frame, None), Ok(FrameTarget::Selected));
        assert_eq!(parse(Command::Frame, Some("0")), Ok(FrameTarget::Level(0)));
        assert_eq!(parse(Command::Up, None), Ok(FrameTarget::Outward(1)));
        assert_eq!(parse(Command::Up, Some("3")), Ok(FrameTarget::Outward(3)));
        assert_eq!(parse(Command::Down, None), Ok(FrameTarget::Inward(1)));
        assert_eq!(parse(Command::Down, Some("2")), Ok(FrameTarget::Inward(2)));
        for invalid in ["-1", "0x2", "one", ""] {
            assert!(parse(Command::Frame, Some(invalid)).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn addresses_require_a_hexadecimal_prefix() {
        let spec = spec(Command::Break);
        assert_eq!(
            parse_breakpoint_spec("0x10", spec).expect("address"),
            BreakpointSpec::Address(VirtualAddress::new(0x10))
        );
        for name in ["add", "face", "f", "42"] {
            assert_eq!(
                parse_breakpoint_spec(name, spec).expect("function"),
                BreakpointSpec::Function(name.to_owned())
            );
        }
        assert!(parse_breakpoint_spec("0xzz", spec).is_err());
        assert!(matches!(
            parse_breakpoint_spec("main.c:12", spec).expect("source"),
            BreakpointSpec::Source { line, .. } if line.get() == 12
        ));
        assert!(matches!(
            parse_breakpoint_spec("main.c:helper", spec).expect("file function"),
            BreakpointSpec::FileFunction { function, .. } if function == "helper"
        ));
        assert!(parse_breakpoint_spec("main.c:0", spec).is_err());
        assert!(parse_breakpoint_spec(":12", spec).is_err());
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
