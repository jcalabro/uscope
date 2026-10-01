//! The command table, argument parsing, and command handlers.

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use uscope::{
    BreakpointId, BreakpointSpec, LineNumber, StepKind, ThreadId, VirtualAddress, WatchAccess,
    WatchpointId, WatchpointSpec,
};

use super::format::{self, plural};
use super::terminal::Role;
use super::value;
use super::{Cli, Control};

const DEFAULT_HEX_DUMP_BYTES: u64 = 64;
pub const MAX_HEX_DUMP_BYTES: u64 = 8 * 1024;
const SOURCE_CONTEXT_RADIUS: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Break,
    Breakpoints,
    Info,
    Delete,
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
    Address,
    Where,
    List,
    Backtrace,
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
        "break <function|0xaddress|file:line|file:function>",
        "Set a breakpoint"
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
        "Run until the selected frame returns",
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
        "Show the current execution location"
    ),
    command!(
        List,
        "list",
        ["l"],
        "list",
        "Show source around the current location",
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
                let breakpoint = debugger
                    .add_breakpoint(parse_breakpoint_spec(arguments[0], spec)?)
                    .await?;
                format::breakpoint(&breakpoint, renderer)
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
            Command::Watch => self.watch(arguments[0], WatchAccess::Write, spec).await?,
            Command::AccessWatch => {
                self.watch(arguments[0], WatchAccess::ReadWrite, spec)
                    .await?
            }
            Command::ReadWatch => self.watch(arguments[0], WatchAccess::Read, spec).await?,
            Command::Watchpoints => self.list_watchpoints().await?,
            Command::Unwatch => self.delete_watchpoints(arguments[0], spec).await?,
            Command::Run => {
                let reason = debugger.run().await?;
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
            Command::Address => format!(
                "{}: {}",
                renderer.paint(Role::Name, arguments[0]),
                renderer.paint(
                    Role::Metadata,
                    debugger.runtime_address(arguments[0]).await?
                )
            ),
            Command::Where => self.location().await?,
            Command::List => format::source_context(
                &debugger.source_context(SOURCE_CONTEXT_RADIUS).await?,
                renderer,
            ),
            Command::Backtrace => self.backtrace().await?,
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

    async fn location(&self) -> Result<String> {
        let renderer = self.renderers.stdout;
        let location = match self.debugger.current_location().await {
            Ok(location) => location,
            // No loaded module describes the instruction, such as one in the vDSO.
            Err(uscope::Error::AddressOutsideModule) => {
                let registers = self.debugger.registers().await?;
                let pc = format::program_counter(&registers)
                    .ok_or(uscope::Error::LocationUnavailable)?;
                return Ok(format!(
                    "{} at {} outside every loaded module",
                    renderer.paint(Role::Name, "<unknown>"),
                    renderer.paint(Role::Metadata, pc)
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
        assert_eq!(spec(Command::Break).arity(), (1, 1));
        assert_eq!(spec(Command::Info).arity(), (1, 2));
        assert_eq!(spec(Command::Print).arity(), (0, 1));
        assert_eq!(spec(Command::Examine).arity(), (1, 2));
        assert!(spec(Command::Next).repeatable);
        assert!(!spec(Command::Watch).repeatable);
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
